//! The bench: loaded config plus providers. Entry point for every operation on devices.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::capture::Cancel;
use crate::config::{Binding, Config, Device};
use crate::connections::Connections;
use crate::daemon::session::{Sessions, Watcher};
use crate::error::{DeskError, Result};
use crate::model::{Capability, Confidence, Power, PowerState};
use crate::ops::{self, CapDriver, Catalog, Drivers, Entry, Kind, Op};
use crate::power::{self, Outcome, Plan, PowerSwitch, Target};
use crate::providers::{Endpoint, ProviderInfo, Registry};
use crate::script::{DeviceInfo, ScriptDriver, ScriptSource, ScriptSpec, ScriptSwitch};
use crate::source;

pub struct Bench {
    pub config: Config,
    pub providers: Arc<Registry>,
    /// Set inside the daemon: live device sessions that commands go through.
    pub sessions: Option<Arc<Sessions>>,
    /// Cancels a capture this bench runs (Ctrl-C, or the daemon client went away).
    pub cancel: Cancel,
}

/// The result of an operation on one device, plus whatever it logged (scripts), which is
/// reported even when the operation failed.
pub struct Run<T> {
    pub result: Result<T>,
    pub log: Vec<String>,
}

impl Bench {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let config = Config::load(path)?;
        let base_dir = config
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let providers = Arc::new(Registry::new(config.providers.clone(), base_dir));
        Ok(Bench {
            config,
            providers,
            sessions: None,
            cancel: Cancel::default(),
        })
    }

    fn power_switch(&self, device: &Device) -> Result<(Box<dyn PowerSwitch>, Duration)> {
        let binding = device
            .power
            .as_ref()
            .ok_or_else(|| DeskError::NoPowerPath(device.id.clone()))?;
        Ok(match binding {
            Binding::Native {
                provider,
                params,
                timeout,
            } => (
                self.providers.get(provider)?.power_switch(params)?,
                timeout.unwrap_or(power::DEFAULT_TIMEOUT),
            ),
            Binding::Script {
                script,
                params,
                timeout,
                connections,
                ..
            } => {
                let timeout = timeout.unwrap_or(power::DEFAULT_SCRIPT_TIMEOUT);
                let sw = ScriptSwitch::load(self.script_spec(
                    device,
                    script,
                    params,
                    connections,
                    timeout,
                ))?;
                (Box::new(sw), timeout)
            }
        })
    }

    pub async fn set_power(&self, device: &Device, target: Target) -> Run<Outcome> {
        let (sw, after) = match self.power_switch(device) {
            Ok(v) => v,
            Err(e) => {
                return Run {
                    result: Err(e),
                    log: Vec::new(),
                };
            }
        };
        // A watched device is on, so off and toggle cut it: end the session first, while the
        // device can still be told goodbye, and tell the watchers why.
        if let Some(sessions) = &self.sessions
            && matches!(target, Target::Off | Target::Toggle)
        {
            sessions
                .stop(&device.id, DeskError::DeviceOff(device.id.clone()))
                .await;
        }
        let result = tokio::time::timeout(after, power::switch(&device.id, sw.as_ref(), target))
            .await
            .unwrap_or_else(|_| {
                Err(DeskError::Timeout {
                    what: format!("switching `{}`", device.id),
                    after,
                })
            });
        Run {
            result,
            log: sw.take_log(),
        }
    }

    fn script_spec<'a>(
        &self,
        device: &'a Device,
        path: &'a Path,
        params: &'a toml::Table,
        connections: &'a Connections,
        timeout: Duration,
    ) -> ScriptSpec<'a> {
        ScriptSpec {
            device: DeviceInfo {
                id: &device.id,
                name: &device.name,
            },
            path,
            params,
            connections,
            timeout,
            providers: self.providers.clone(),
        }
    }

    /// Build the driver of one catalog entry (nothing is opened yet).
    pub(crate) fn driver(&self, device: &Device, entry: &Entry) -> Result<CapDriver> {
        let cap = device.caps.get(&entry.key).ok_or_else(|| {
            DeskError::Internal(format!("`{}` has no `{}`", device.id, entry.key))
        })?;
        Ok(match (&entry.kind, &cap.binding) {
            (
                Kind::Source(_),
                Binding::Native {
                    provider, params, ..
                },
            ) => CapDriver::Source(self.providers.get(provider)?.voltage_source(params)?),
            (
                Kind::Source(_),
                Binding::Script {
                    script,
                    params,
                    connections,
                    ..
                },
            ) => CapDriver::Source(Box::new(ScriptSource::load(self.script_spec(
                device,
                script,
                params,
                connections,
                entry.timeout,
            ))?)),
            (
                Kind::Declared,
                Binding::Native {
                    provider, params, ..
                },
            ) => CapDriver::Declared(
                self.providers
                    .get(provider)?
                    .declared(params, &entry.decls)?,
            ),
            (
                Kind::Declared,
                Binding::Script {
                    script,
                    params,
                    connections,
                    ..
                },
            ) => CapDriver::Declared(Box::new(ScriptDriver::load(
                self.script_spec(device, script, params, connections, entry.timeout),
                &entry.decls,
            )?)),
        })
    }

    /// Mains first: a device whose power path reads `off` can't be reached, so say that instead
    /// of failing later with "port not found". Unknown or unreadable power doesn't block. Returns
    /// the mains state when it could be read.
    pub(crate) async fn require_mains(&self, device: &Device) -> Result<Option<Power>> {
        if device.power.is_none() {
            return Ok(None);
        }
        match self.power_state(device).await.result {
            Ok(PowerState {
                state: Power::Off,
                confidence: Confidence::Real,
            }) => Err(DeskError::DeviceOff(device.id.clone())),
            Ok(PowerState {
                state,
                confidence: Confidence::Real,
            }) => Ok(Some(state)),
            _ => Ok(None),
        }
    }

    /// Run a `get`, `set` or `do` on a device: through its live session if the daemon has one,
    /// else in a session of its own.
    pub async fn run(&self, device: &Device, cat: &Catalog, op: Op) -> Run<ops::Report> {
        let fail = |e| Run {
            result: Err(e),
            log: Vec::new(),
        };
        if cat.entries.is_empty() && device.power.is_none() {
            return fail(DeskError::CapabilityMissing {
                device: device.id.clone(),
                missing: "has no capabilities (no power path, no `source`, nothing declared)"
                    .into(),
            });
        }
        // A stream holds the device: fail at once rather than wait behind it.
        if let Some(e) = self
            .sessions
            .as_ref()
            .and_then(|s| s.stream_busy(&device.id))
        {
            return fail(e);
        }
        let mains = match self.require_mains(device).await {
            Ok(m) => m,
            Err(e) => return fail(e),
        };
        let run = match &self.sessions {
            None => self.run_once(device, cat, &op).await,
            Some(sessions) => loop {
                let port = sessions.port(&device.id);
                let guard = port.lock().await;
                if !sessions.is_live(&device.id) {
                    // No session can start while we hold the port lock.
                    let run = self.run_once(device, cat, &op).await;
                    drop(guard);
                    break run;
                }
                drop(guard);
                if let Some(run) = sessions.run(&device.id, cat.keys(), op.clone()).await {
                    break run;
                }
                // The session ended meanwhile: run on our own.
            },
        };
        Run {
            result: run.result.map(|o| ops::Report::new(cat, &op, mains, o)),
            log: run.log,
        }
    }

    /// Open the drivers an op needs, run it, close them (also after errors and timeouts).
    async fn run_once(&self, device: &Device, cat: &Catalog, op: &Op) -> Run<ops::Outcome> {
        let needed = op.entries(cat);
        let mut drivers: Drivers = (0..cat.entries.len()).map(|_| None).collect();
        for i in &needed {
            match self.driver(device, &cat.entries[*i]) {
                Ok(d) => drivers[*i] = Some(d),
                Err(e) => {
                    return Run {
                        result: Err(e),
                        log: Vec::new(),
                    };
                }
            }
        }
        let timeout = needed
            .iter()
            .map(|i| cat.entries[*i].timeout)
            .max()
            .unwrap_or(source::DEFAULT_TIMEOUT);
        let what = format!("controlling `{}`", device.id);
        let result = session(&drivers, timeout, &what, ops::run(cat, &drivers, op))
            .await
            .map(|(mut outcome, closed)| {
                if let Some(e) = closed {
                    outcome.warnings.push(format!(
                        "the command is done, but closing the session failed: {e}"
                    ));
                }
                outcome
            });
        Run {
            result,
            log: take_logs(&drivers),
        }
    }

    /// Watch a device: join its live session or start one (daemon only).
    pub async fn watch(&self, device: &Device, interval: Duration) -> Result<(Watcher, Catalog)> {
        let sessions = self
            .sessions
            .as_ref()
            .ok_or_else(|| DeskError::Internal("watching needs the daemon's sessions".into()))?;
        let cat = Catalog::of(device)?;
        let readable: Vec<usize> = (0..cat.entries.len())
            .filter(|i| cat.entries[*i].decls.readable())
            .collect();
        if readable.is_empty() {
            return Err(DeskError::CapabilityMissing {
                device: device.id.clone(),
                missing: "has nothing to watch (no capability with values or settings)".into(),
            });
        }
        let watcher = sessions
            .watch(&device.id, interval, || {
                self.open_all(device, &cat, &readable)
            })
            .await?;
        Ok((watcher, cat))
    }

    /// Open drivers for a long-lived session.
    async fn open_all(
        &self,
        device: &Device,
        cat: &Catalog,
        which: &[usize],
    ) -> Result<(Catalog, Drivers, Duration)> {
        self.require_mains(device).await?;
        let mut drivers: Drivers = (0..cat.entries.len()).map(|_| None).collect();
        for i in which {
            drivers[*i] = Some(self.driver(device, &cat.entries[*i])?);
        }
        let timeout = which
            .iter()
            .map(|i| cat.entries[*i].timeout)
            .max()
            .unwrap_or(source::DEFAULT_TIMEOUT);
        let opened = tokio::time::timeout(timeout, open_drivers(&drivers))
            .await
            .unwrap_or_else(|_| {
                Err(DeskError::Timeout {
                    what: format!("connecting to `{}`", device.id),
                    after: timeout,
                })
            });
        match opened {
            Ok(()) => Ok((cat.clone(), drivers, timeout)),
            Err(e) => {
                close_drivers(&drivers).await;
                Err(e)
            }
        }
    }

    pub async fn plan_power(&self, device: &Device, target: Target) -> Run<Plan> {
        let (sw, after) = match self.power_switch(device) {
            Ok(v) => v,
            Err(e) => {
                return Run {
                    result: Err(e),
                    log: Vec::new(),
                };
            }
        };
        let result = tokio::time::timeout(after, power::plan(&device.id, sw.as_ref(), target))
            .await
            .unwrap_or_else(|_| {
                Err(DeskError::Timeout {
                    what: format!("planning `{}`", device.id),
                    after,
                })
            });
        Run {
            result,
            log: sw.take_log(),
        }
    }

    pub async fn power_state(&self, device: &Device) -> Run<PowerState> {
        let (sw, after) = match self.power_switch(device) {
            Ok(v) => v,
            Err(e) => {
                return Run {
                    result: Err(e),
                    log: Vec::new(),
                };
            }
        };
        let result = tokio::time::timeout(after, sw.state())
            .await
            .unwrap_or_else(|_| {
                Err(DeskError::Timeout {
                    what: format!("reading power state of `{}`", device.id),
                    after,
                })
            });
        Run {
            result,
            log: sw.take_log(),
        }
    }

    pub async fn test_provider(&self, id: &str) -> Result<ProviderInfo> {
        let provider = self.providers.get(id)?;
        let after = provider.query_ceiling();
        tokio::time::timeout(after, provider.test())
            .await
            .map_err(|_| DeskError::Timeout {
                what: format!("testing provider `{id}`"),
                after,
            })?
    }

    pub async fn endpoints(&self, id: &str, capability: Capability) -> Result<Vec<Endpoint>> {
        let provider = self.providers.get(id)?;
        let after = provider.query_ceiling();
        tokio::time::timeout(after, provider.endpoints(capability))
            .await
            .map_err(|_| DeskError::Timeout {
                what: format!("listing endpoints of provider `{id}`"),
                after,
            })?
    }

    /// Resolve device queries up front, deduplicated, in the order given. Fails before anything
    /// is touched if any query doesn't resolve.
    pub fn resolve_all(&self, queries: &[String]) -> Result<Vec<&Device>> {
        let mut out: Vec<&Device> = Vec::new();
        for q in queries {
            let d = self.config.resolve_device(q)?;
            if !out.iter().any(|o| o.id == d.id) {
                out.push(d);
            }
        }
        Ok(out)
    }

    pub fn powered_devices(&self) -> Vec<&Device> {
        self.config
            .devices
            .iter()
            .filter(|d| d.power.is_some())
            .collect()
    }
}

async fn open_drivers(drivers: &Drivers) -> Result<()> {
    for d in drivers.iter().flatten() {
        d.open().await?;
    }
    Ok(())
}

/// Close every driver (each within 2 s), whatever happened before.
pub async fn close_drivers(drivers: &Drivers) -> Option<DeskError> {
    let mut first = None;
    for d in drivers.iter().flatten() {
        let r = tokio::time::timeout(Duration::from_secs(2), d.close()).await;
        let e = match r {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e),
            Err(_) => Some(DeskError::Timeout {
                what: "closing the session".into(),
                after: Duration::from_secs(2),
            }),
        };
        if first.is_none() {
            first = e;
        }
    }
    first
}

pub fn take_logs(drivers: &Drivers) -> Vec<String> {
    drivers
        .iter()
        .flatten()
        .flat_map(|d| d.take_log())
        .collect()
}

/// Open, run `op` with a timeout, always close (devices that lock their panel get released).
/// Next to what `op` returned comes an error from closing: once the op is done (something may be
/// switched on, a capture is on disk), a failing close must not hide its result.
async fn session<T>(
    drivers: &Drivers,
    timeout: Duration,
    what: &str,
    op: impl std::future::Future<Output = Result<T>>,
) -> Result<(T, Option<DeskError>)> {
    // A host function that fills a capture suspends this timeout while its own one runs.
    let result = crate::deadline::timeout(timeout, async {
        open_drivers(drivers).await?;
        op.await
    })
    .await
    .unwrap_or_else(|_| {
        Err(DeskError::Timeout {
            what: what.to_string(),
            after: timeout,
        })
    });
    let closed = close_drivers(drivers).await;
    // A failed op keeps its own error: that is the one to act on.
    result.map(|v| (v, closed))
}
