//! Providers: configured connections to things that can switch or control many endpoints
//! (Home Assistant, a Shelly, a relay board, ...).

mod home_assistant;
mod mock;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use indexmap::IndexMap;
use serde::Serialize;

use crate::config::ProviderConfig;
use crate::error::{DeskError, Result};
use crate::model::Capability;
use crate::power::PowerSwitch;
use crate::source::VoltageSource;

/// Provider kinds this build knows, as used in `kind = "..."`.
pub const KINDS: &[&str] = &[home_assistant::KIND, mock::KIND];

#[async_trait]
pub trait Provider: Send + Sync {
    /// Check connectivity and report something useful (version, state location, ...).
    async fn test(&self) -> Result<ProviderInfo>;
    /// Everything this provider can offer for a capability, e.g. HA entities that can be switched.
    async fn endpoints(&self, capability: Capability) -> Result<Vec<Endpoint>>;
    /// Bind one endpoint, described by the device's power binding params, as a power switch.
    fn power_switch(&self, params: &toml::Table) -> Result<Box<dyn PowerSwitch>>;
    /// Bind an endpoint as a bench power supply. Most providers can't.
    fn voltage_source(&self, _params: &toml::Table) -> Result<Box<dyn VoltageSource>> {
        Err(DeskError::ConfigInvalid(
            "this provider can't drive a voltage source (use a driver script or another provider)"
                .into(),
        ))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderInfo {
    pub detail: String,
}

/// One thing a provider can control.
#[derive(Debug, Clone, Serialize)]
pub struct Endpoint {
    /// Provider-specific identifier, e.g. `switch.desk_outlet_1`.
    pub id: String,
    /// Human-friendly name, if the provider knows one.
    pub name: Option<String>,
    /// Current raw state as the provider reports it (e.g. `on`, `unavailable`).
    pub state: Option<String>,
    /// The binding params that select this endpoint (`provider` excluded), ready to paste into
    /// `power = { provider = "...", ... }`.
    pub params: toml::Table,
}

/// What a provider gets besides its own settings.
pub struct BuildContext<'a> {
    /// Directory of the config file; relative paths in settings resolve against it.
    pub base_dir: &'a Path,
}

impl BuildContext<'_> {
    /// Resolve a path from the config: `~/` expands to the home directory, relative paths are
    /// relative to the config file.
    pub fn resolve_path(&self, p: &str) -> PathBuf {
        if let Some(rest) = p.strip_prefix("~/")
            && let Some(home) = directories::BaseDirs::new()
        {
            return home.home_dir().join(rest);
        }
        self.base_dir.join(p)
    }
}

fn build(id: &str, cfg: &ProviderConfig, ctx: &BuildContext) -> Result<Arc<dyn Provider>> {
    let settings = Params::new(&cfg.settings, format!("provider `{id}`"));
    match cfg.kind.as_str() {
        home_assistant::KIND => Ok(Arc::new(home_assistant::HomeAssistant::new(
            id, &settings, ctx,
        )?)),
        mock::KIND => Ok(Arc::new(mock::MockProvider::new(id, &settings)?)),
        other => Err(DeskError::ConfigInvalid(format!(
            "provider `{id}`: unknown kind `{other}`"
        ))),
    }
}

/// Builds providers lazily, so a broken provider only affects the devices that use it.
pub struct Registry {
    configs: IndexMap<String, ProviderConfig>,
    base_dir: PathBuf,
    built: Mutex<HashMap<String, Arc<dyn Provider>>>,
}

impl Registry {
    pub fn new(configs: IndexMap<String, ProviderConfig>, base_dir: PathBuf) -> Self {
        Registry {
            configs,
            base_dir,
            built: Mutex::new(HashMap::new()),
        }
    }

    pub fn ids(&self) -> impl Iterator<Item = (&str, &ProviderConfig)> {
        self.configs.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn get(&self, id: &str) -> Result<Arc<dyn Provider>> {
        let mut built = self
            .built
            .lock()
            .map_err(|_| DeskError::Internal("provider registry poisoned".into()))?;
        if let Some(p) = built.get(id) {
            return Ok(p.clone());
        }
        let cfg = self.configs.get(id).ok_or_else(|| {
            DeskError::ConfigInvalid(format!("provider `{id}` is not defined under [providers]"))
        })?;
        let ctx = BuildContext {
            base_dir: &self.base_dir,
        };
        let provider = build(id, cfg, &ctx)?;
        built.insert(id.to_string(), provider.clone());
        Ok(provider)
    }
}

/// Typed access to a provider's settings or a binding's params, with errors that name their
/// context. Providers should call [`Params::only`] so typos are caught.
pub struct Params<'a> {
    table: &'a toml::Table,
    context: String,
}

impl<'a> Params<'a> {
    pub fn new(table: &'a toml::Table, context: impl Into<String>) -> Self {
        Params {
            table,
            context: context.into(),
        }
    }

    fn invalid(&self, msg: impl std::fmt::Display) -> DeskError {
        DeskError::ConfigInvalid(format!("{}: {msg}", self.context))
    }

    pub fn only(&self, allowed: &[&str]) -> Result<()> {
        match self.table.keys().find(|k| !allowed.contains(&k.as_str())) {
            None => Ok(()),
            Some(k) => Err(self.invalid(format_args!(
                "unknown key `{k}` (allowed: {})",
                allowed.join(", ")
            ))),
        }
    }

    pub fn str(&self, key: &str) -> Result<Option<&'a str>> {
        match self.table.get(key) {
            None => Ok(None),
            Some(toml::Value::String(s)) => Ok(Some(s)),
            Some(_) => Err(self.invalid(format_args!("`{key}` must be a string"))),
        }
    }

    pub fn required_str(&self, key: &str) -> Result<&'a str> {
        self.str(key)?
            .ok_or_else(|| self.invalid(format_args!("missing `{key}`")))
    }

    pub fn str_list(&self, key: &str) -> Result<Vec<&'a str>> {
        match self.table.get(key) {
            None => Ok(Vec::new()),
            Some(toml::Value::Array(items)) => items
                .iter()
                .map(|v| {
                    v.as_str().ok_or_else(|| {
                        self.invalid(format_args!("`{key}` must be a list of strings"))
                    })
                })
                .collect(),
            Some(_) => Err(self.invalid(format_args!("`{key}` must be a list of strings"))),
        }
    }

    pub fn float(&self, key: &str) -> Result<Option<f64>> {
        match self.table.get(key) {
            None => Ok(None),
            Some(toml::Value::Float(f)) => Ok(Some(*f)),
            Some(toml::Value::Integer(i)) => Ok(Some(*i as f64)),
            Some(_) => Err(self.invalid(format_args!("`{key}` must be a number"))),
        }
    }

    pub fn duration(&self, key: &str) -> Result<Option<Duration>> {
        match self.str(key)? {
            None => Ok(None),
            Some(s) => humantime::parse_duration(s)
                .map(Some)
                .map_err(|e| self.invalid(format_args!("invalid `{key}` \"{s}\": {e}"))),
        }
    }
}
