//! Providers: configured connections to things that can switch or control many endpoints
//! (Home Assistant, a Shelly, a relay board, ...).

mod home_assistant;
mod mock;
mod sigrok;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use indexmap::IndexMap;
use serde::Serialize;

use crate::config::ProviderConfig;
use crate::declared::{Declarations, Driver};
use crate::error::{DeskError, Result};
use crate::model::Capability;
use crate::power::PowerSwitch;
use crate::source::VoltageSource;

/// Provider kinds this build knows, as used in `kind = "..."`.
pub const KINDS: &[&str] = &[home_assistant::KIND, mock::KIND, crate::sigrok::KIND];

/// Providers that exist without configuration (id, kind), built like a configured one with no
/// settings. A `[providers.<id>]` of the same id replaces it.
const IMPLICIT: &[(&str, &str)] = &[(crate::sigrok::IMPLICIT, crate::sigrok::KIND)];

/// The providers that exist without configuration, as (id, kind).
pub fn implicit() -> impl Iterator<Item = (&'static str, &'static str)> {
    IMPLICIT.iter().copied()
}

/// The kind of provider `id`, given these configs: a configured one, or an implicit one.
pub fn kind_of<'a>(id: &str, configs: &'a IndexMap<String, ProviderConfig>) -> Option<&'a str> {
    match configs.get(id) {
        Some(c) => Some(&c.kind),
        None => implicit().find(|(i, _)| *i == id).map(|(_, k)| k),
    }
}

/// How a provider's endpoints are used in a device's config.
#[derive(Debug, Clone, PartialEq)]
pub enum EndpointUse {
    /// As a native binding: `<capability> = { provider = <id>, …params }`.
    Native,
    /// As the connection `key` of a script binding: `key = { …params }`. The provider is a
    /// connection backend, never `provider = …`.
    Connection {
        key: &'static str,
        /// Why it can't be bound as `provider = …` (what every binder answers).
        not_native: &'static str,
        /// Printed under a listing of its endpoints: where their params go.
        note: String,
        /// Printed instead of an empty listing.
        none_found: String,
    },
}

/// The binders' answer when a provider can't bind an endpoint this way.
fn cannot_bind(usage: EndpointUse, native: &str) -> DeskError {
    DeskError::ConfigInvalid(match usage {
        EndpointUse::Native => native.into(),
        EndpointUse::Connection { not_native, .. } => not_native.into(),
    })
}

#[async_trait]
pub trait Provider: Send + Sync {
    /// Check connectivity and report something useful (version, state location, ...).
    async fn test(&self) -> Result<ProviderInfo>;
    /// Everything this provider can offer for a capability, e.g. HA entities that can be switched.
    async fn endpoints(&self, capability: Capability) -> Result<Vec<Endpoint>>;
    /// How its endpoints are used: as native bindings unless the provider says otherwise.
    fn endpoint_use(&self) -> EndpointUse {
        EndpointUse::Native
    }
    /// The ceiling around `test` and `endpoints`.
    fn query_ceiling(&self) -> Duration {
        crate::power::DEFAULT_TIMEOUT
    }
    /// Whether an implicit provider is there to be listed (e.g. its program is installed).
    fn present(&self) -> bool {
        true
    }
    /// Bind one endpoint, described by the device's power binding params, as a power switch.
    fn power_switch(&self, _params: &toml::Table) -> Result<Box<dyn PowerSwitch>> {
        Err(cannot_bind(
            self.endpoint_use(),
            "this provider can't switch mains power (use a driver script or another provider)",
        ))
    }
    /// Bind an endpoint as a bench power supply. Most providers can't.
    fn voltage_source(&self, _params: &toml::Table) -> Result<Box<dyn VoltageSource>> {
        Err(cannot_bind(
            self.endpoint_use(),
            "this provider can't drive a voltage source (use a driver script or another provider)",
        ))
    }
    /// Bind an endpoint as a declared capability. Most providers can't.
    fn declared(&self, _params: &toml::Table, _decls: &Declarations) -> Result<Box<dyn Driver>> {
        Err(cannot_bind(
            self.endpoint_use(),
            "this provider can't drive a declared capability (use a driver script)",
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
    /// Built as an implicit provider (no `[providers.<id>]`).
    pub implicit: bool,
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
        crate::sigrok::KIND => Ok(Arc::new(sigrok::SigrokProvider(
            crate::sigrok::Backend::configured(id, cfg, ctx)?,
        ))),
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

    /// The configured providers (for resolving connection backends).
    pub fn configs(&self) -> &IndexMap<String, ProviderConfig> {
        &self.configs
    }

    /// The folder relative paths in provider settings resolve against.
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    pub fn get(&self, id: &str) -> Result<Arc<dyn Provider>> {
        let mut built = self
            .built
            .lock()
            .map_err(|_| DeskError::Internal("provider registry poisoned".into()))?;
        if let Some(p) = built.get(id) {
            return Ok(p.clone());
        }
        let implicit_cfg;
        let (cfg, implicit) = match self.configs.get(id) {
            Some(cfg) => (cfg, false),
            None => {
                let Some((_, kind)) = implicit().find(|(i, _)| *i == id) else {
                    return Err(DeskError::ConfigInvalid(format!(
                        "provider `{id}` is not defined under [providers]"
                    )));
                };
                implicit_cfg = ProviderConfig {
                    kind: kind.into(),
                    settings: toml::Table::new(),
                };
                (&implicit_cfg, true)
            }
        };
        let ctx = BuildContext {
            base_dir: &self.base_dir,
            implicit,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn implicit_providers_have_a_kind_until_configured_otherwise() {
        let mut configs = IndexMap::new();
        for (id, kind) in implicit() {
            assert_eq!(kind_of(id, &configs), Some(kind));
            let registry = Registry::new(configs.clone(), PathBuf::new());
            assert!(
                registry.get(id).is_ok(),
                "{id} builds without configuration"
            );
        }
        assert_eq!(kind_of("nope", &configs), None);
        for (id, _) in implicit() {
            configs.insert(
                id.to_string(),
                ProviderConfig {
                    kind: mock::KIND.into(),
                    settings: toml::Table::new(),
                },
            );
            assert_eq!(kind_of(id, &configs), Some(mock::KIND));
        }
    }

    #[test]
    fn a_connection_backend_refuses_every_native_binding() {
        let registry = Registry::new(IndexMap::new(), PathBuf::new());
        for (id, _) in implicit() {
            let p = registry.get(id).unwrap();
            let EndpointUse::Connection { not_native, .. } = p.endpoint_use() else {
                continue;
            };
            let t = toml::Table::new();
            let errors = [
                p.power_switch(&t).err().map(|e| e.to_string()),
                p.voltage_source(&t).err().map(|e| e.to_string()),
                p.declared(&t, &Declarations::default())
                    .err()
                    .map(|e| e.to_string()),
            ];
            for e in errors {
                assert_eq!(e.as_deref(), Some(not_native));
            }
        }
    }
}
