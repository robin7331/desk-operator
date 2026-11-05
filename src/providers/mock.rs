//! A fake provider for tests, demos and contributors without hardware.
//!
//! State persists in a JSON file so separate CLI invocations see each other's changes:
//! `$DESK_MOCK_STATE`, or `mock-state.json` in the platform state/data dir.
//!
//! Endpoint name prefixes simulate failure modes:
//! * `fail_*`  — every operation fails with `endpoint_unavailable`
//! * `stuck_*` — `set` succeeds but the state never changes (→ `verify_failed`)
//! * `blind_*` — send-only: no readback, state is unknown
//!
//! Settings: `latency = "200ms"` delays every operation (for timeout tests);
//! `endpoints = ["outlet1", ...]` is what `desk provider endpoints` lists (plus anything already in
//! the state file).
//!
//! It also simulates bench power supplies (`voltage_source`):
//! `source = { provider = "sim", endpoint = "psu1", max_voltage = 30, max_current = 5, load_ohms = 10 }`.
//! With `load_ohms` the output behaves like a resistor is connected (CV, or CC when V/R exceeds
//! the current setpoint); without it the output is open (0 A). `blind_*` endpoints report only
//! measurements, no setpoints or output state, like many cheap supplies. `tripping_*` endpoints
//! trip their over-current protection instead of going into CC: the output switches off and stays
//! off until it's switched on again (which clears the trip, like many real supplies).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use super::{Endpoint, Params, Provider, ProviderInfo};
use crate::error::{DeskError, Result};
use crate::model::{Capability, Power, PowerFeatures, PowerState};
use crate::power::PowerSwitch;
use crate::source::{Mode, Protection, Reading, SourceInfo, VoltageSource};

pub const KIND: &str = "mock";

/// Serialises read-modify-write of the state file within this process.
static FILE_LOCK: Mutex<()> = Mutex::new(());

/// Everything the mock remembers, keyed by provider id, then endpoint.
#[derive(Default, Serialize, Deserialize)]
struct StateFile {
    #[serde(default)]
    power: BTreeMap<String, BTreeMap<String, bool>>,
    #[serde(default)]
    sources: BTreeMap<String, BTreeMap<String, SimPsu>>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
struct SimPsu {
    voltage_set: f64,
    current_set: f64,
    output: bool,
    #[serde(default)]
    tripped: bool,
}

pub struct MockProvider {
    inner: Arc<Inner>,
}

struct Inner {
    id: String,
    latency: Duration,
    path: PathBuf,
    endpoints: Vec<String>,
}

impl MockProvider {
    pub fn new(id: &str, settings: &Params) -> Result<Self> {
        settings.only(&["latency", "endpoints"])?;
        Ok(MockProvider {
            inner: Arc::new(Inner {
                id: id.to_string(),
                latency: settings.duration("latency")?.unwrap_or_default(),
                path: state_path()?,
                endpoints: settings
                    .str_list("endpoints")?
                    .into_iter()
                    .map(String::from)
                    .collect(),
            }),
        })
    }
}

fn state_path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("DESK_MOCK_STATE") {
        return Ok(PathBuf::from(p));
    }
    let dirs = directories::ProjectDirs::from("", "", "desk-operator")
        .ok_or_else(|| DeskError::Internal("cannot determine a state directory".into()))?;
    let dir = dirs.state_dir().unwrap_or_else(|| dirs.data_local_dir());
    Ok(dir.join("mock-state.json"))
}

impl Inner {
    fn read(&self) -> Result<StateFile> {
        match std::fs::read_to_string(&self.path) {
            // Older or foreign files simply start empty.
            Ok(text) => Ok(serde_json::from_str(&text).unwrap_or_default()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(StateFile::default()),
            Err(e) => Err(e.into()),
        }
    }

    fn view<R>(&self, f: impl FnOnce(&StateFile) -> R) -> Result<R> {
        let _guard = FILE_LOCK
            .lock()
            .map_err(|_| DeskError::Internal("mock lock poisoned".into()))?;
        Ok(f(&self.read()?))
    }

    fn update<R>(&self, f: impl FnOnce(&mut StateFile) -> R) -> Result<R> {
        let _guard = FILE_LOCK
            .lock()
            .map_err(|_| DeskError::Internal("mock lock poisoned".into()))?;
        let mut state = self.read()?;
        let result = f(&mut state);
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let text =
            serde_json::to_string_pretty(&state).map_err(|e| DeskError::Internal(e.to_string()))?;
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(result)
    }

    fn get(&self, endpoint: &str) -> Result<bool> {
        self.view(|s| {
            s.power
                .get(&self.id)
                .and_then(|m| m.get(endpoint))
                .copied()
                .unwrap_or(false)
        })
    }

    fn put(&self, endpoint: &str, on: bool) -> Result<()> {
        self.update(|s| {
            s.power
                .entry(self.id.clone())
                .or_default()
                .insert(endpoint.to_string(), on);
        })
    }

    fn psu(&self, endpoint: &str) -> Result<SimPsu> {
        self.view(|s| {
            s.sources
                .get(&self.id)
                .and_then(|m| m.get(endpoint))
                .copied()
                .unwrap_or_default()
        })
    }

    fn update_psu(&self, endpoint: &str, f: impl FnOnce(&mut SimPsu)) -> Result<()> {
        self.update(|s| {
            f(s.sources
                .entry(self.id.clone())
                .or_default()
                .entry(endpoint.to_string())
                .or_default());
        })
    }

    async fn simulate(&self, endpoint: &str) -> Result<()> {
        tokio::time::sleep(self.latency).await;
        if endpoint.starts_with("fail_") {
            return Err(DeskError::EndpointUnavailable {
                endpoint: endpoint.to_string(),
                message: "simulated failure".into(),
            });
        }
        Ok(())
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn test(&self) -> Result<ProviderInfo> {
        tokio::time::sleep(self.inner.latency).await;
        Ok(ProviderInfo {
            detail: format!("simulated, state in {}", self.inner.path.display()),
        })
    }

    async fn endpoints(&self, _capability: Capability) -> Result<Vec<Endpoint>> {
        tokio::time::sleep(self.inner.latency).await;
        let known = self
            .inner
            .view(|s| s.power.get(&self.inner.id).cloned().unwrap_or_default())?;
        let mut ids: Vec<String> = self.inner.endpoints.clone();
        ids.extend(
            known
                .keys()
                .filter(|k| !ids.contains(k))
                .cloned()
                .collect::<Vec<_>>(),
        );
        Ok(ids
            .into_iter()
            .map(|id| {
                let state = if id.starts_with("blind_") {
                    "unknown"
                } else if known.get(&id).copied().unwrap_or(false) {
                    "on"
                } else {
                    "off"
                };
                let mut params = toml::Table::new();
                params.insert("endpoint".into(), toml::Value::String(id.clone()));
                Endpoint {
                    id,
                    name: None,
                    state: Some(state.into()),
                    params,
                }
            })
            .collect())
    }

    fn voltage_source(&self, params: &toml::Table) -> Result<Box<dyn VoltageSource>> {
        let params = Params::new(
            params,
            format!("source binding for provider `{}`", self.inner.id),
        );
        params.only(&["endpoint", "max_voltage", "max_current", "load_ohms"])?;
        Ok(Box::new(SimSource {
            inner: self.inner.clone(),
            endpoint: params.required_str("endpoint")?.to_string(),
            info: SourceInfo {
                max_voltage: Some(params.float("max_voltage")?.unwrap_or(30.0)),
                max_current: Some(params.float("max_current")?.unwrap_or(5.0)),
                max_power: None,
            },
            load_ohms: params.float("load_ohms")?,
        }))
    }

    fn power_switch(&self, params: &toml::Table) -> Result<Box<dyn PowerSwitch>> {
        let params = Params::new(
            params,
            format!("power binding for provider `{}`", self.inner.id),
        );
        params.only(&["endpoint"])?;
        Ok(Box::new(MockSwitch {
            inner: self.inner.clone(),
            endpoint: params.required_str("endpoint")?.to_string(),
        }))
    }
}

struct MockSwitch {
    inner: Arc<Inner>,
    endpoint: String,
}

impl MockSwitch {
    async fn simulate(&self) -> Result<()> {
        self.inner.simulate(&self.endpoint).await
    }
}

#[async_trait]
impl PowerSwitch for MockSwitch {
    fn features(&self) -> PowerFeatures {
        PowerFeatures {
            readback: !self.endpoint.starts_with("blind_"),
            settle: Duration::ZERO,
        }
    }

    async fn set(&self, on: bool) -> Result<()> {
        self.simulate().await?;
        if self.endpoint.starts_with("stuck_") {
            return Ok(());
        }
        self.inner.put(&self.endpoint, on)
    }

    async fn state(&self) -> Result<PowerState> {
        self.simulate().await?;
        if !self.features().readback {
            return Ok(PowerState::unknown());
        }
        Ok(PowerState::real(Power::from_bool(
            self.inner.get(&self.endpoint)?,
        )))
    }
}

struct SimSource {
    inner: Arc<Inner>,
    endpoint: String,
    info: SourceInfo,
    load_ohms: Option<f64>,
}

#[async_trait]
impl VoltageSource for SimSource {
    async fn info(&self) -> Result<SourceInfo> {
        self.inner.simulate(&self.endpoint).await?;
        Ok(self.info)
    }

    async fn set_voltage(&self, volts: f64) -> Result<()> {
        self.inner.simulate(&self.endpoint).await?;
        self.inner
            .update_psu(&self.endpoint, |p| p.voltage_set = volts)
    }

    async fn set_current(&self, amps: f64) -> Result<()> {
        self.inner.simulate(&self.endpoint).await?;
        self.inner
            .update_psu(&self.endpoint, |p| p.current_set = amps)
    }

    async fn set_output(&self, on: bool) -> Result<()> {
        self.inner.simulate(&self.endpoint).await?;
        let trips = self.endpoint.starts_with("tripping_");
        let load = self.load_ohms;
        self.inner.update_psu(&self.endpoint, |p| {
            p.output = on;
            if on {
                // Switching on clears a trip; it trips again at once if the load is still too much.
                p.tripped = trips && load.is_some_and(|r| p.voltage_set / r > p.current_set);
                p.output = !p.tripped;
            }
        })
    }

    async fn read(&self) -> Result<Reading> {
        self.inner.simulate(&self.endpoint).await?;
        let p = self.inner.psu(&self.endpoint)?;
        let (voltage, current) = match (p.output, self.load_ohms) {
            (false, _) => (0.0, 0.0),
            (true, None) => (p.voltage_set, 0.0),
            // CV until V/R exceeds the current setpoint, then CC.
            (true, Some(r)) if p.voltage_set / r > p.current_set => {
                (p.current_set * r, p.current_set)
            }
            (true, Some(r)) => (p.voltage_set, p.voltage_set / r),
        };
        let blind = self.endpoint.starts_with("blind_");
        let mode = match (p.output, self.load_ohms) {
            (false, _) => None,
            (true, Some(r)) if p.voltage_set / r > p.current_set => Some(Mode::Cc),
            (true, _) => Some(Mode::Cv),
        };
        Ok(Reading {
            output: (!blind).then_some(p.output),
            voltage_set: (!blind).then_some(p.voltage_set),
            current_set: (!blind).then_some(p.current_set),
            voltage: Some(voltage),
            current: Some(current),
            power: Some(voltage * current),
            mode: if blind { None } else { mode },
            protection: (!blind).then_some(if p.tripped {
                Protection::Ocp
            } else {
                Protection::None
            }),
            ..Reading::default()
        })
    }
}
