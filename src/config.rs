//! Loading and validating `desk.toml`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::declared::{self, Declarations, Limit};
use crate::error::{DeskError, Result};
use crate::models::{self, Models};
use crate::providers;
use crate::serial::PortSpec;
use crate::source::Limits;

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
    /// The model it's based on, if any (e.g. `acme/psu-3000`).
    pub model: Option<String>,
    /// Mains (`power_switch`).
    pub power: Option<Binding>,
    /// Every other capability, in config order: `source` (voltage_source, well-known) or one the
    /// model declares.
    pub caps: IndexMap<String, Cap>,
    /// Bench safety limits per capability, enforced by the core for every driver.
    pub limits: DeviceLimits,
}

/// One capability of a device: its binding and what it declares. For `source`, only `channels`
/// (its values and settings are fixed by the core).
#[derive(Debug, Clone)]
pub struct Cap {
    pub binding: Binding,
    pub decls: Declarations,
}

/// The well-known capability keys: `power` (mains) and `source` (bench power supply).
pub const POWER: &str = "power";
pub const SOURCE: &str = "source";

/// Keys of a device table that aren't capabilities.
const DEVICE_KEYS: &[&str] = &["name", "model", "aliases", "limits"];

impl Device {
    pub fn source(&self) -> Option<&Binding> {
        self.caps.get(SOURCE).map(|c| &c.binding)
    }
}

/// How a capability of a device is implemented: by a provider, or by a Lua script.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(try_from = "toml::Table", tag = "kind", rename_all = "lowercase")]
pub enum Binding {
    /// The core calls a provider directly: `{ provider = "ha", entity = "switch.x" }`.
    Native {
        provider: String,
        /// Everything except `provider`/`timeout`; validated by the provider.
        params: toml::Table,
        #[serde(serialize_with = "ser_opt_duration")]
        timeout: Option<Duration>,
    },
    /// A Lua script implements the capability (`on`/`off`/`state`, `set_voltage`/`read`, …).
    Script {
        /// Resolved against the config file's directory.
        script: PathBuf,
        params: toml::Table,
        #[serde(serialize_with = "ser_opt_duration")]
        timeout: Option<Duration>,
        /// The only serial port this script may open (`ctx.serial.open`).
        #[serde(skip_serializing_if = "Option::is_none")]
        port: Option<PortSpec>,
        /// The device's maximums, in the capability's standard form (e.g. `max_voltage`).
        #[serde(skip_serializing_if = "toml::Table::is_empty")]
        ratings: toml::Table,
    },
}

impl Binding {
    pub fn timeout(&self) -> Option<Duration> {
        match self {
            Binding::Native { timeout, .. } | Binding::Script { timeout, .. } => *timeout,
        }
    }

    /// Very short form for narrow table columns: the provider id, or `script <file>`.
    pub fn summary(&self) -> String {
        match self {
            Binding::Native { provider, .. } => provider.clone(),
            Binding::Script { script, .. } => format!(
                "script {}",
                script.file_name().map_or_else(
                    || script.display().to_string(),
                    |n| n.to_string_lossy().into_owned()
                )
            ),
        }
    }

    /// Short human-readable description, e.g. `sim (endpoint=outlet1)`.
    pub fn describe(&self) -> String {
        match self {
            Binding::Native {
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
            Binding::Script { script, port, .. } => match port {
                Some(port) => format!("script {} on {port}", script.display()),
                None => format!("script {}", script.display()),
            },
        }
    }
}

impl TryFrom<toml::Table> for Binding {
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
            (true, true) => Err("a binding uses either `provider` or `script`, not both".into()),
            (false, false) => {
                Err("a binding needs `provider = \"...\"` or `script = \"...\"`".into())
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
                let ratings = match t.remove("ratings") {
                    None => toml::Table::new(),
                    Some(toml::Value::Table(r)) => r,
                    Some(_) => return Err("`ratings` must be a table".into()),
                };
                let port = match t.remove("port") {
                    None => None,
                    Some(toml::Value::Table(p)) => Some(PortSpec::try_from(p)?),
                    Some(_) => {
                        return Err(
                            "`port` must be a table, e.g. { usb_vid = 0x1a86, usb_pid = 0x7523 }"
                                .into(),
                        );
                    }
                };
                if let Some(k) = t.keys().next() {
                    return Err(format!(
                        "unknown key `{k}` in script binding (allowed: script, params, ratings, timeout, port)"
                    ));
                }
                Ok(Binding::Script {
                    script,
                    params,
                    timeout,
                    port,
                    ratings,
                })
            }
            (false, true) => {
                let provider = match t.remove("provider") {
                    Some(toml::Value::String(s)) => s,
                    _ => return Err("`provider` must be a string".into()),
                };
                Ok(Binding::Native {
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
    devices: IndexMap<String, toml::Table>,
}

/// A device table: the known keys, and every other table as a capability (raw: it may be a
/// partial override of a model's capability).
struct RawDevice {
    name: Option<String>,
    /// A model from the models, e.g. `acme/psu-3000`.
    model: Option<String>,
    aliases: Vec<String>,
    limits: Option<toml::Table>,
    caps: IndexMap<String, toml::Table>,
}

impl RawDevice {
    fn parse(id: &str, t: toml::Table) -> Result<RawDevice> {
        let bad = |m: String| DeskError::ConfigInvalid(format!("device `{id}`: {m}"));
        let mut d = RawDevice {
            name: None,
            model: None,
            aliases: Vec::new(),
            limits: None,
            caps: IndexMap::new(),
        };
        for (k, v) in t {
            match (k.as_str(), v) {
                ("name", toml::Value::String(s)) => d.name = Some(s),
                ("model", toml::Value::String(s)) => d.model = Some(s),
                ("aliases", toml::Value::Array(a)) => {
                    d.aliases = a
                        .into_iter()
                        .map(|x| match x {
                            toml::Value::String(s) => Ok(s),
                            _ => Err(bad("`aliases` must be a list of strings".into())),
                        })
                        .collect::<Result<_>>()?;
                }
                ("limits", toml::Value::Table(l)) => d.limits = Some(l),
                (k, _) if DEVICE_KEYS.contains(&k) => {
                    return Err(bad(format!("`{k}` has the wrong type")));
                }
                (_, toml::Value::Table(c)) => {
                    d.caps.insert(k, c);
                }
                (k, _) => {
                    return Err(bad(format!(
                        "unknown key `{k}` (a device has {} and one table per capability)",
                        DEVICE_KEYS.join(", ")
                    )));
                }
            }
        }
        Ok(d)
    }
}

/// `limits = { source = { voltage = 12 }, heater = { setpoint = 380, ch2 = { setpoint = 300 } } }`:
/// one table per capability, a number (the maximum) or `{ min, max }` per setting, and optionally
/// one table per channel that overrides those.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DeviceLimits {
    #[serde(flatten)]
    pub caps: IndexMap<String, CapLimits>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct CapLimits {
    #[serde(flatten)]
    pub all: IndexMap<String, Limit>,
    #[serde(flatten)]
    pub channels: IndexMap<String, IndexMap<String, Limit>>,
}

impl DeviceLimits {
    pub fn is_empty(&self) -> bool {
        self.caps.is_empty()
    }

    /// The bench limit of one setting, for a channel (a channel's own limit wins).
    pub fn get(&self, cap: &str, setting: &str, channel: Option<&str>) -> Option<Limit> {
        let c = self.caps.get(cap)?;
        channel
            .and_then(|ch| c.channels.get(ch))
            .and_then(|l| l.get(setting))
            .or_else(|| c.all.get(setting))
            .copied()
    }

    /// The `voltage_source` limits in its own form.
    pub fn source(&self, channel: Option<&str>) -> Limits {
        let max = |s: &str| self.get(SOURCE, s, channel).and_then(|l| l.max);
        Limits {
            voltage: max("voltage"),
            current: max("current"),
        }
    }
}

/// Settings of `voltage_source` that can have bench limits.
const SOURCE_LIMITS: &[&str] = &["voltage", "current"];

fn parse_limits(
    device: &str,
    raw: Option<&toml::Table>,
    caps: &IndexMap<String, Cap>,
) -> Result<DeviceLimits> {
    let Some(raw) = raw else {
        return Ok(DeviceLimits::default());
    };
    let bad = |m: String| DeskError::ConfigInvalid(format!("device `{device}`: limits: {m}"));
    let mut limits = DeviceLimits::default();
    for (key, value) in raw {
        let Some(cap) = caps.get(key) else {
            if SOURCE_LIMITS.contains(&key.as_str()) {
                return Err(bad(format!(
                    "limits are per capability: write `limits = {{ source = {{ {key} = … }} }}`"
                )));
            }
            let known: Vec<&str> = caps.keys().map(String::as_str).collect();
            return Err(bad(format!(
                "unknown capability `{key}` (this device has: {})",
                if known.is_empty() {
                    "none".into()
                } else {
                    known.join(", ")
                }
            )));
        };
        let toml::Value::Table(t) = value else {
            return Err(bad(format!("`{key}` must be a table of settings")));
        };
        // Which settings can be limited, and how.
        let limitable = |name: &str| -> std::result::Result<(), String> {
            if key == SOURCE {
                return if SOURCE_LIMITS.contains(&name) {
                    Ok(())
                } else {
                    Err(format!(
                        "`{key}.{name}`: source limits exist for: voltage, current"
                    ))
                };
            }
            match cap.decls.settings.get(name) {
                Some(d) if d.kind == declared::Type::Number => Ok(()),
                Some(_) => Err(format!("`{key}.{name}` isn't a number setting")),
                None => Err(format!(
                    "`{key}.{name}` is not a setting of `{key}` (settings: {})",
                    cap.decls
                        .settings
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            }
        };
        let one = |name: &str, v: &toml::Value| -> std::result::Result<Limit, String> {
            limitable(name)?;
            let l = Limit::parse(v).map_err(|e| format!("`{key}.{name}`: {e}"))?;
            if key == SOURCE && l.min.is_some() {
                return Err(format!(
                    "`{key}.{name}`: source limits are maximums (a number)"
                ));
            }
            for b in [l.min, l.max].into_iter().flatten() {
                if key == SOURCE && b < 0.0 {
                    return Err(format!("`{key}.{name}` must be >= 0"));
                }
            }
            Ok(l)
        };
        let mut c = CapLimits::default();
        let channels = cap.decls.channels.clone().unwrap_or_default();
        for (name, v) in t {
            if channels.contains(name) {
                let toml::Value::Table(ct) = v else {
                    return Err(bad(format!("`{key}.{name}` (a channel) must be a table")));
                };
                let mut m = IndexMap::new();
                for (n, v) in ct {
                    m.insert(n.clone(), one(n, v).map_err(bad)?);
                }
                c.channels.insert(name.clone(), m);
            } else {
                c.all.insert(name.clone(), one(name, v).map_err(bad)?);
            }
        }
        limits.caps.insert(key.clone(), c);
    }
    Ok(limits)
}

/// A capability's binding and declarations from its (merged) table.
fn build_cap(
    device: &str,
    key: &str,
    mut table: toml::Table,
) -> Result<(toml::Table, Declarations)> {
    let bad = |e: String| DeskError::ConfigInvalid(format!("device `{device}`: `{key}`: {e}"));
    let decls = Declarations::take(&mut table).map_err(bad)?;
    if key == POWER && decls != Declarations::default() {
        return Err(bad("mains (`power`) declares nothing".into()));
    }
    if key == SOURCE && !decls.is_empty() {
        return Err(bad(
            "extra values, settings or actions on `source` aren't supported yet (only `channels`)"
                .into(),
        ));
    }
    if key != POWER && key != SOURCE && decls.is_empty() {
        return Err(bad(format!(
            "a capability other than power or source must declare `values`, `settings` or `actions` (is `{key}` a typo?)"
        )));
    }
    Ok((table, decls))
}

/// Capability names: lowercase letters, digits, `_`; not a device key, not `mains`/`model`.
pub fn valid_cap_name(k: &str) -> bool {
    !k.is_empty()
        && k.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !DEVICE_KEYS.contains(&k)
        && k != "mains"
        && k != "model"
}

/// Build one capability binding from the device's own table and its model's (if any).
fn build_binding(
    device: &str,
    key: &str,
    own: Option<&toml::Table>,
    model_def: Option<&toml::Table>,
    config_path: &Path,
) -> Result<Option<Cap>> {
    let bad = |e: String| DeskError::ConfigInvalid(format!("device `{device}`: `{key}`: {e}"));
    let (table, own_binding) = match (own, model_def) {
        (None, None) => return Ok(None),
        (None, Some(def)) => (def.clone(), false),
        (Some(own), Some(def)) if !own.contains_key("script") && !own.contains_key("provider") => {
            models::check_tightens(def, own).map_err(bad)?;
            (models::merge(def, own), false)
        }
        // The device's own binding (paths relative to the config file).
        (Some(own), _) => (own.clone(), true),
    };
    let (table, decls) = build_cap(device, key, table)?;
    let mut binding = Binding::try_from(table).map_err(bad)?;
    if own_binding {
        binding = resolve_script_path(binding, config_path);
    }
    if let Binding::Script { ratings, .. } = &binding {
        check_ratings(key, ratings).map_err(bad)?;
    }
    Ok(Some(Cap { binding, decls }))
}

/// Ratings have a standard form per capability.
pub fn check_ratings(key: &str, ratings: &toml::Table) -> std::result::Result<(), String> {
    if ratings.is_empty() {
        return Ok(());
    }
    match key {
        "source" => crate::source::parse_ratings(ratings).map(drop),
        other => Err(format!("`{other}` bindings have no ratings")),
    }
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

        let models = Models::for_config(&path);
        let mut devices = Vec::new();
        for (id, d) in raw.devices {
            let d = RawDevice::parse(&id, d)?;
            let model_def =
                match &d.model {
                    Some(model) => Some(models.find(model).map_err(|e| {
                        DeskError::ConfigInvalid(format!("device `{id}`: model: {e}"))
                    })?),
                    None => None,
                };
            // Capabilities: the model's, then the device's own new ones, in order.
            let mut keys: Vec<String> = model_def
                .as_ref()
                .map(|def| def.bindings.keys().cloned().collect())
                .unwrap_or_default();
            for k in d.caps.keys() {
                if !keys.contains(k) {
                    keys.push(k.clone());
                }
            }
            let mut power = None;
            let mut caps = IndexMap::new();
            for key in keys {
                if !valid_cap_name(&key) {
                    return Err(DeskError::ConfigInvalid(format!(
                        "device `{id}`: `{key}` can't be a capability name (lowercase letters, \
                         digits, `_`; not `mains` or `model`)"
                    )));
                }
                let Some(cap) = build_binding(
                    &id,
                    &key,
                    d.caps.get(&key),
                    model_def.as_ref().and_then(|def| def.bindings.get(&key)),
                    &path,
                )?
                else {
                    continue;
                };
                if key == POWER {
                    power = Some(cap.binding);
                } else {
                    caps.insert(key, cap);
                }
            }
            let limits = parse_limits(&id, d.limits.as_ref(), &caps)?;
            devices.push(Device {
                name: d
                    .name
                    .clone()
                    .or_else(|| model_def.as_ref().map(|def| def.name.clone()))
                    .unwrap_or_else(|| id.clone()),
                model: d.model.clone(),
                aliases: d.aliases.clone(),
                power,
                caps,
                limits,
                id,
            });
        }

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
            let bindings = std::iter::once((POWER, d.power.as_ref()))
                .chain(d.caps.iter().map(|(k, c)| (k.as_str(), Some(&c.binding))));
            for (key, binding) in bindings {
                if let Some(Binding::Native { provider, .. }) = binding
                    && !self.providers.contains_key(provider)
                {
                    problems.push(format!(
                        "device `{}`: {key} provider `{provider}` is not defined under [providers]",
                        d.id
                    ));
                }
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

fn resolve_script_path(binding: Binding, config_path: &Path) -> Binding {
    match binding {
        Binding::Script {
            script,
            params,
            timeout,
            port,
            ratings,
        } if script.is_relative() && !models::is_bundled(&script) => Binding::Script {
            script: config_path.parent().unwrap_or(Path::new(".")).join(script),
            params,
            timeout,
            port,
            ratings,
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
        name = "Lab Supply 3000"
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
            Some(Binding::Native {
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
            Some(Binding::Script { script, .. }) => {
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
        assert_eq!(c.resolve_device("lab supply 3000").unwrap().id, "psu");
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
