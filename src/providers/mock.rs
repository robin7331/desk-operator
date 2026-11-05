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

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use super::{Endpoint, Params, Provider, ProviderInfo};
use crate::error::{DeskError, Result};
use crate::model::{Capability, Power, PowerFeatures, PowerState};
use crate::power::PowerSwitch;

pub const KIND: &str = "mock";

/// Serialises read-modify-write of the state file within this process.
static FILE_LOCK: Mutex<()> = Mutex::new(());

/// `{ "<provider id>": { "<endpoint>": true } }`
type StateFile = BTreeMap<String, BTreeMap<String, bool>>;

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
            Ok(text) => serde_json::from_str(&text).map_err(|e| {
                DeskError::Internal(format!("corrupt mock state {}: {e}", self.path.display()))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(StateFile::new()),
            Err(e) => Err(e.into()),
        }
    }

    fn get(&self, endpoint: &str) -> Result<bool> {
        let _guard = FILE_LOCK
            .lock()
            .map_err(|_| DeskError::Internal("mock lock poisoned".into()))?;
        let state = self.read()?;
        Ok(state
            .get(&self.id)
            .and_then(|m| m.get(endpoint))
            .copied()
            .unwrap_or(false))
    }

    fn put(&self, endpoint: &str, on: bool) -> Result<()> {
        let _guard = FILE_LOCK
            .lock()
            .map_err(|_| DeskError::Internal("mock lock poisoned".into()))?;
        let mut state = self.read()?;
        state
            .entry(self.id.clone())
            .or_default()
            .insert(endpoint.to_string(), on);
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let text =
            serde_json::to_string_pretty(&state).map_err(|e| DeskError::Internal(e.to_string()))?;
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &self.path)?;
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
        let known = {
            let _guard = FILE_LOCK
                .lock()
                .map_err(|_| DeskError::Internal("mock lock poisoned".into()))?;
            self.inner
                .read()?
                .remove(&self.inner.id)
                .unwrap_or_default()
        };
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
        tokio::time::sleep(self.inner.latency).await;
        if self.endpoint.starts_with("fail_") {
            return Err(DeskError::EndpointUnavailable {
                endpoint: self.endpoint.clone(),
                message: "simulated failure".into(),
            });
        }
        Ok(())
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
