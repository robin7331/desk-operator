//! Providers: configured connections to things that can switch or control many endpoints
//! (Home Assistant, a Shelly, a relay board, ...).

mod mock;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use indexmap::IndexMap;
use serde::Serialize;

use crate::config::ProviderConfig;
use crate::error::{DeskError, Result};
use crate::power::PowerSwitch;

/// Provider kinds this build knows, as used in `kind = "..."`.
pub const KINDS: &[&str] = &[mock::KIND];

#[async_trait]
pub trait Provider: Send + Sync {
    /// Check connectivity and report something useful (version, state location, ...).
    async fn test(&self) -> Result<ProviderInfo>;
    /// Bind one endpoint, described by the device's power binding params, as a power switch.
    fn power_switch(&self, params: &toml::Table) -> Result<Box<dyn PowerSwitch>>;
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderInfo {
    pub detail: String,
}

fn build(id: &str, cfg: &ProviderConfig) -> Result<Arc<dyn Provider>> {
    let settings = Params::new(&cfg.settings, format!("provider `{id}`"));
    match cfg.kind.as_str() {
        mock::KIND => Ok(Arc::new(mock::MockProvider::new(id, &settings)?)),
        other => Err(DeskError::ConfigInvalid(format!(
            "provider `{id}`: unknown kind `{other}`"
        ))),
    }
}

/// Builds providers lazily, so a broken provider only affects the devices that use it.
pub struct Registry {
    configs: IndexMap<String, ProviderConfig>,
    built: Mutex<HashMap<String, Arc<dyn Provider>>>,
}

impl Registry {
    pub fn new(configs: IndexMap<String, ProviderConfig>) -> Self {
        Registry {
            configs,
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
        let provider = build(id, cfg)?;
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

    pub fn duration(&self, key: &str) -> Result<Option<Duration>> {
        match self.str(key)? {
            None => Ok(None),
            Some(s) => humantime::parse_duration(s)
                .map(Some)
                .map_err(|e| self.invalid(format_args!("invalid `{key}` \"{s}\": {e}"))),
        }
    }
}
