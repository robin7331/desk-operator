//! Loading and validating `desk.toml`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::error::{DeskError, Result};
use crate::providers;

#[derive(Debug)]
pub struct Config {
    /// Path of the loaded file.
    pub path: PathBuf,
    pub providers: IndexMap<String, ProviderConfig>,
    /// Devices in file order.
    pub devices: Vec<Device>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    pub kind: String,
    /// Everything except `kind`; interpreted by the provider itself.
    #[serde(flatten)]
    pub settings: toml::Table,
}

#[derive(Debug, Clone)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub power: Option<PowerBinding>,
}

/// How a device gets mains power.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(try_from = "toml::Table", tag = "kind", rename_all = "lowercase")]
pub enum PowerBinding {
    /// The core calls a provider directly: `{ provider = "ha", entity = "switch.x" }`.
    Native {
        provider: String,
        /// Everything except `provider`/`timeout`; validated by the provider.
        params: toml::Table,
        #[serde(serialize_with = "ser_opt_duration")]
        timeout: Option<Duration>,
    },
    /// A Lua script implements `on(ctx)` / `off(ctx)` / `state(ctx)`.
    Script {
        /// Resolved against the config file's directory.
        script: PathBuf,
        params: toml::Table,
        #[serde(serialize_with = "ser_opt_duration")]
        timeout: Option<Duration>,
    },
}

impl PowerBinding {
    pub fn timeout(&self) -> Option<Duration> {
        match self {
            PowerBinding::Native { timeout, .. } | PowerBinding::Script { timeout, .. } => *timeout,
        }
    }

    /// Short human-readable description, e.g. `sim (endpoint=outlet1)`.
    pub fn describe(&self) -> String {
        match self {
            PowerBinding::Native {
                provider, params, ..
            } => {
                let params: Vec<String> = params
                    .iter()
                    .map(|(k, v)| match v {
                        toml::Value::String(s) => format!("{k}={s}"),
                        other => format!("{k}={other}"),
                    })
                    .collect();
                if params.is_empty() {
                    provider.clone()
                } else {
                    format!("{provider} ({})", params.join(", "))
                }
            }
            PowerBinding::Script { script, .. } => format!("script {}", script.display()),
        }
    }
}

impl TryFrom<toml::Table> for PowerBinding {
    type Error = String;

    fn try_from(mut t: toml::Table) -> std::result::Result<Self, String> {
        let timeout = match t.remove("timeout") {
            None => None,
            Some(toml::Value::String(s)) => Some(humantime::parse_duration(&s).map_err(|e| {
                format!("invalid `timeout` \"{s}\": {e} (use e.g. \"500ms\", \"15s\")")
            })?),
            Some(_) => return Err("`timeout` must be a string such as \"15s\"".into()),
        };

        match (t.contains_key("script"), t.contains_key("provider")) {
            (true, true) => {
                Err("a power binding uses either `provider` or `script`, not both".into())
            }
            (false, false) => {
                Err("a power binding needs `provider = \"...\"` or `script = \"...\"`".into())
            }
            (true, false) => {
                let script = match t.remove("script") {
                    Some(toml::Value::String(s)) => PathBuf::from(s),
                    _ => return Err("`script` must be a path string".into()),
                };
                let params = match t.remove("params") {
                    None => toml::Table::new(),
                    Some(toml::Value::Table(p)) => p,
                    Some(_) => return Err("`params` must be a table".into()),
                };
                if let Some(k) = t.keys().next() {
                    return Err(format!(
                        "unknown key `{k}` in script binding (allowed: script, params, timeout)"
                    ));
                }
                Ok(PowerBinding::Script {
                    script,
                    params,
                    timeout,
                })
            }
            (false, true) => {
                let provider = match t.remove("provider") {
                    Some(toml::Value::String(s)) => s,
                    _ => return Err("`provider` must be a string".into()),
                };
                Ok(PowerBinding::Native {
                    provider,
                    params: t,
                    timeout,
                })
            }
        }
    }
}

fn ser_opt_duration<S: serde::Serializer>(
    d: &Option<Duration>,
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    match d {
        Some(d) => s.serialize_str(&humantime::format_duration(*d).to_string()),
        None => s.serialize_none(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    providers: IndexMap<String, ProviderConfig>,
    #[serde(default)]
    devices: IndexMap<String, RawDevice>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDevice {
    name: Option<String>,
    #[serde(default)]
    aliases: Vec<String>,
    power: Option<PowerBinding>,
}

/// Platform default: `~/.config/desk-operator/desk.toml` on Linux, the equivalent elsewhere.
pub fn default_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "desk-operator")
        .map(|d| d.config_dir().join("desk.toml"))
}

impl Config {
    /// Load from an explicit path (from `--config` / `DESK_CONFIG`) or the platform default.
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let path = match explicit {
            Some(p) => p.to_path_buf(),
            None => default_path().ok_or_else(|| {
                DeskError::ConfigInvalid(
                    "cannot determine a config directory; pass --config".into(),
                )
            })?,
        };
        let text = std::fs::read_to_string(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                DeskError::ConfigInvalid(format!(
                    "config file not found: {} (create it, see examples/desk.toml, or pass --config)",
                    path.display()
                ))
            } else {
                DeskError::ConfigInvalid(format!("cannot read {}: {e}", path.display()))
            }
        })?;
        Self::parse(&text, path)
    }

    pub fn parse(text: &str, path: PathBuf) -> Result<Self> {
        let raw: RawConfig = toml::from_str(text)
            .map_err(|e| DeskError::ConfigInvalid(format!("{}: {e}", path.display())))?;

        let devices = raw
            .devices
            .into_iter()
            .map(|(id, d)| Device {
                name: d.name.unwrap_or_else(|| id.clone()),
                id,
                aliases: d.aliases,
                power: d.power.map(|p| resolve_script_path(p, &path)),
            })
            .collect();

        let config = Config {
            path,
            providers: raw.providers,
            devices,
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        let mut problems = Vec::new();

        for (id, p) in &self.providers {
            if !valid_id(id) {
                problems.push(format!(
                    "provider id `{id}`: use only letters, digits, `-` and `_`"
                ));
            }
            if !providers::KINDS.contains(&p.kind.as_str()) {
                problems.push(format!(
                    "provider `{id}`: unknown kind `{}` (known: {})",
                    p.kind,
                    providers::KINDS.join(", ")
                ));
            }
        }

        // Ids and aliases share one case-insensitive namespace.
        let mut names: HashMap<String, &str> = HashMap::new();
        for d in &self.devices {
            if !valid_id(&d.id) {
                problems.push(format!(
                    "device id `{}`: use only letters, digits, `-` and `_`",
                    d.id
                ));
            }
            for key in std::iter::once(&d.id).chain(&d.aliases) {
                if key.eq_ignore_ascii_case(ALL) {
                    problems.push(format!(
                        "device `{}`: `{key}` is reserved (`desk power off all` means every device)",
                        d.id
                    ));
                }
                if let Some(other) = names.insert(key.to_lowercase(), &d.id)
                    && other != d.id
                {
                    problems.push(format!(
                        "device `{}`: name `{key}` is already used by device `{other}`",
                        d.id
                    ));
                }
            }
            if let Some(PowerBinding::Native { provider, .. }) = &d.power
                && !self.providers.contains_key(provider)
            {
                problems.push(format!(
                    "device `{}`: power provider `{provider}` is not defined under [providers]",
                    d.id
                ));
            }
        }

        if problems.is_empty() {
            Ok(())
        } else {
            Err(DeskError::ConfigInvalid(format!(
                "{}:\n  - {}",
                self.path.display(),
                problems.join("\n  - ")
            )))
        }
    }

    /// Find a device by id, alias or name (case-insensitive).
    pub fn resolve_device(&self, query: &str) -> Result<&Device> {
        let q = query.to_lowercase();
        let by_key: Vec<&Device> = self
            .devices
            .iter()
            .filter(|d| d.id.to_lowercase() == q || d.aliases.iter().any(|a| a.to_lowercase() == q))
            .collect();
        let matches = if by_key.is_empty() {
            self.devices
                .iter()
                .filter(|d| d.name.to_lowercase() == q)
                .collect()
        } else {
            by_key
        };
        match matches.as_slice() {
            [one] => Ok(one),
            [] => Err(DeskError::DeviceNotFound {
                query: query.to_string(),
                known: self.devices.iter().map(|d| d.id.clone()).collect(),
            }),
            many => Err(DeskError::DeviceAmbiguous {
                query: query.to_string(),
                candidates: many.iter().map(|d| d.id.clone()).collect(),
            }),
        }
    }
}

/// Reserved word: `desk power off all` means every device, so no device may be called this.
pub const ALL: &str = "all";

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn resolve_script_path(binding: PowerBinding, config_path: &Path) -> PowerBinding {
    match binding {
        PowerBinding::Script {
            script,
            params,
            timeout,
        } if script.is_relative() => PowerBinding::Script {
            script: config_path.parent().unwrap_or(Path::new(".")).join(script),
            params,
            timeout,
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config> {
        Config::parse(text, PathBuf::from("/bench/desk.toml"))
    }

    const BASIC: &str = r#"
        [providers.sim]
        kind = "mock"

        [devices.psu]
        name = "FNIRSI IPS3608"
        aliases = ["supply", "Power Supply"]
        power = { provider = "sim", endpoint = "outlet1", timeout = "2s" }

        [devices.lamp]
        power = { script = "scripts/lamp.lua", params = { entity = "x" } }

        [devices.dmm]
        name = "Multimeter"
    "#;

    #[test]
    fn parses_bindings_in_file_order() {
        let c = parse(BASIC).unwrap();
        let ids: Vec<_> = c.devices.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["psu", "lamp", "dmm"]);

        match &c.devices[0].power {
            Some(PowerBinding::Native {
                provider,
                params,
                timeout,
            }) => {
                assert_eq!(provider, "sim");
                assert_eq!(params["endpoint"].as_str(), Some("outlet1"));
                assert!(!params.contains_key("timeout"));
                assert_eq!(*timeout, Some(Duration::from_secs(2)));
            }
            other => panic!("unexpected binding {other:?}"),
        }
        match &c.devices[1].power {
            Some(PowerBinding::Script { script, .. }) => {
                assert_eq!(script, Path::new("/bench/scripts/lamp.lua"));
            }
            other => panic!("unexpected binding {other:?}"),
        }
        assert_eq!(c.devices[1].name, "lamp", "name defaults to id");
        assert!(c.devices[2].power.is_none());
    }

    #[test]
    fn resolves_by_id_alias_and_name() {
        let c = parse(BASIC).unwrap();
        assert_eq!(c.resolve_device("PSU").unwrap().id, "psu");
        assert_eq!(c.resolve_device("power supply").unwrap().id, "psu");
        assert_eq!(c.resolve_device("fnirsi ips3608").unwrap().id, "psu");
        assert!(matches!(
            c.resolve_device("scope"),
            Err(DeskError::DeviceNotFound { .. })
        ));
    }

    #[test]
    fn rejects_bad_bindings() {
        let both = r#"
            [providers.sim]
            kind = "mock"
            [devices.x]
            power = { provider = "sim", script = "a.lua" }
        "#;
        assert!(parse(both).unwrap_err().to_string().contains("not both"));

        let neither = r#"
            [devices.x]
            power = { endpoint = "a" }
        "#;
        assert!(parse(neither).unwrap_err().to_string().contains("needs"));

        let bad_timeout = r#"
            [providers.sim]
            kind = "mock"
            [devices.x]
            power = { provider = "sim", endpoint = "a", timeout = "soon" }
        "#;
        assert!(
            parse(bad_timeout)
                .unwrap_err()
                .to_string()
                .contains("timeout")
        );
    }

    #[test]
    fn reports_all_validation_problems() {
        let text = r#"
            [providers.ha]
            kind = "zigbee-magic"

            [devices.a]
            aliases = ["iron"]
            power = { provider = "missing", endpoint = "x" }

            [devices.iron]
        "#;
        let msg = parse(text).unwrap_err().to_string();
        assert!(msg.contains("unknown kind `zigbee-magic`"), "{msg}");
        assert!(msg.contains("provider `missing` is not defined"), "{msg}");
        assert!(msg.contains("already used by device `a`"), "{msg}");
    }

    #[test]
    fn rejects_unknown_device_fields() {
        let text = r#"
            [devices.a]
            nmae = "typo"
        "#;
        assert!(parse(text).unwrap_err().to_string().contains("nmae"));
    }
}
