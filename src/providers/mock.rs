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
//!
//! And any declared capability (`heater = { provider = "sim", endpoint = "iron", settings = …, … }`):
//! settings are stored (start: `min`, `false`, the first choice, `""`); `fixed = { … }` gives fixed
//! values; `follow = { temperature = "setpoint" }` makes a value report a setting; a declared value
//! `last_action` reports the last action and its args, e.g. `"boost seconds=10"`. With channels,
//! each channel has its own settings. `stuck_*` ignores `set`, `blind_*` reports values only.
//!
//! An action with `produces = "file"` fills the capture with a known pattern (byte `i` is
//! `i % 251`), like a host function would: arguments `bytes` (how many, default 1024), `rate`
//! (bytes per second, default as fast as possible) and `timeout` (in s, default 10) are used when
//! the action declares and gets them. It reports `format = "mock"`, extension `bin`, and
//! `meta = { pattern = "i % 251" }`. `fail_*` fails before writing anything.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use super::{Endpoint, Params, Provider, ProviderInfo};
use crate::capture::{Output, Produced};
use crate::declared::{Declarations, Driver, Type, Val};
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
    /// Declared capabilities: provider → endpoint (`endpoint/channel`) → name → value.
    #[serde(default)]
    declared: BTreeMap<String, BTreeMap<String, BTreeMap<String, serde_json::Value>>>,
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

    fn declared(&self, params: &toml::Table, decls: &Declarations) -> Result<Box<dyn Driver>> {
        let ctx = format!("binding for provider `{}`", self.inner.id);
        let p = Params::new(params, ctx.clone());
        p.only(&["endpoint", "fixed", "follow"])?;
        let table = |k: &str| -> Result<toml::Table> {
            match params.get(k) {
                None => Ok(toml::Table::new()),
                Some(toml::Value::Table(t)) => Ok(t.clone()),
                Some(_) => Err(DeskError::ConfigInvalid(format!(
                    "{ctx}: `{k}` must be a table"
                ))),
            }
        };
        let fixed = table("fixed")?
            .into_iter()
            .map(|(k, v)| {
                serde_json::to_value(v)
                    .map(|v| (k, v))
                    .map_err(|e| DeskError::ConfigInvalid(format!("{ctx}: fixed: {e}")))
            })
            .collect::<Result<_>>()?;
        let follow = table("follow")?
            .into_iter()
            .map(|(k, v)| match v {
                toml::Value::String(s) if decls.settings.contains_key(&s) => Ok((k, s)),
                _ => Err(DeskError::ConfigInvalid(format!(
                    "{ctx}: follow.{k} must name a setting"
                ))),
            })
            .collect::<Result<_>>()?;
        Ok(Box::new(SimDeclared {
            inner: self.inner.clone(),
            endpoint: p.required_str("endpoint")?.to_string(),
            decls: decls.clone(),
            fixed,
            follow,
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

struct SimDeclared {
    inner: Arc<Inner>,
    endpoint: String,
    decls: Declarations,
    fixed: BTreeMap<String, serde_json::Value>,
    follow: BTreeMap<String, String>,
}

impl SimDeclared {
    fn key(&self, channel: Option<&str>) -> String {
        match channel {
            Some(ch) => format!("{}/{ch}", self.endpoint),
            None => self.endpoint.clone(),
        }
    }

    fn stored(&self, channel: Option<&str>) -> Result<BTreeMap<String, serde_json::Value>> {
        let key = self.key(channel);
        self.inner.view(|s| {
            s.declared
                .get(&self.inner.id)
                .and_then(|m| m.get(&key))
                .cloned()
                .unwrap_or_default()
        })
    }

    fn store(&self, channel: Option<&str>, name: &str, v: serde_json::Value) -> Result<()> {
        let key = self.key(channel);
        self.inner.update(|s| {
            s.declared
                .entry(self.inner.id.clone())
                .or_default()
                .entry(key)
                .or_default()
                .insert(name.to_string(), v);
        })
    }
}

#[async_trait]
impl Driver for SimDeclared {
    async fn read(&self, channel: Option<&str>) -> Result<serde_json::Value> {
        self.inner.simulate(&self.endpoint).await?;
        let stored = self.stored(channel)?;
        let mut out = serde_json::Map::new();
        let blind = self.endpoint.starts_with("blind_");
        let mut settings = serde_json::Map::new();
        for (name, d) in &self.decls.settings {
            let v = stored.get(name).cloned().unwrap_or_else(|| match d.kind {
                Type::Number => serde_json::json!(d.min.unwrap_or(0.0)),
                Type::Boolean => serde_json::json!(false),
                Type::Choice => serde_json::json!(d.choices.first().cloned().unwrap_or_default()),
                Type::String => serde_json::json!(""),
            });
            settings.insert(name.clone(), v);
        }
        for name in self.decls.values.keys() {
            if let Some(v) = self.fixed.get(name) {
                out.insert(name.clone(), v.clone());
            } else if let Some(s) = self.follow.get(name) {
                out.insert(name.clone(), settings[s].clone());
            } else if let Some(v) = stored.get(name) {
                out.insert(name.clone(), v.clone());
            }
        }
        if !blind {
            out.extend(settings);
        }
        Ok(serde_json::Value::Object(out))
    }

    async fn set(&self, channel: Option<&str>, name: &str, value: &Val) -> Result<()> {
        self.inner.simulate(&self.endpoint).await?;
        if self.endpoint.starts_with("stuck_") {
            return Ok(());
        }
        self.store(channel, name, crate::declared::to_json(value))
    }

    async fn action(
        &self,
        channel: Option<&str>,
        name: &str,
        args: &indexmap::IndexMap<String, Val>,
        output: Option<&Output>,
    ) -> Result<Option<Produced>> {
        self.inner.simulate(&self.endpoint).await?;
        let mut text = name.to_string();
        for (k, v) in args {
            text.push_str(&format!(" {k}={}", v.display(None)));
        }
        self.store(channel, "last_action", serde_json::Value::String(text))?;
        let Some(out) = output else {
            return Ok(None);
        };
        let num = |k: &str| args.get(k).and_then(Val::as_f64);
        let bytes = num("bytes").unwrap_or(1024.0).max(0.0) as u64;
        let rate = num("rate").filter(|r| *r > 0.0);
        let after = num("timeout")
            .and_then(|s| Duration::try_from_secs_f64(s).ok())
            .unwrap_or(Duration::from_secs(10));
        // Like a host function that fills a sink: its own timeout, the others suspended.
        let _suspended = crate::deadline::suspend();
        tokio::time::timeout(after, fill(out, bytes, rate))
            .await
            .map_err(|_| DeskError::Timeout {
                what: format!("capture of `{}`", self.endpoint),
                after,
            })??;
        let mut meta = serde_json::Map::new();
        meta.insert("pattern".into(), serde_json::json!("i % 251"));
        Ok(Some(Produced {
            format: "mock".into(),
            extension: "bin".into(),
            meta,
        }))
    }

    fn label(&self) -> String {
        format!("sim (endpoint={})", self.endpoint)
    }
}

/// Write `bytes` of the pattern, in 50 ms steps at `rate` bytes per second (or all at once).
async fn fill(out: &Output, bytes: u64, rate: Option<f64>) -> Result<()> {
    const STEP: Duration = Duration::from_millis(50);
    let chunk = match rate {
        Some(r) => ((r * STEP.as_secs_f64()) as u64).max(1),
        None => 64 * 1024,
    };
    let mut at = 0u64;
    while at < bytes {
        let n = chunk.min(bytes - at);
        let data: Vec<u8> = (at..at + n).map(|i| (i % 251) as u8).collect();
        out.write(&data)?;
        at += n;
        if rate.is_some() && at < bytes {
            tokio::time::sleep(STEP).await;
        }
    }
    Ok(())
}
