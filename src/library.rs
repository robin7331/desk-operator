//! The device library: device **definitions** (what's true for every unit of a model), found in
//! several places, first match wins:
//!
//! 1. `devices/` next to the config file (your own or unpublished definitions)
//! 2. every directory in `$DESK_DEVICES` (e.g. a checkout of the library)
//! 3. the definitions bundled into this binary at build time
//!
//! A definition is `<vendor>/<model>/device.toml`. It may only contain *script* bindings: how a
//! particular user's device gets mains (their Home Assistant, …) never belongs in a definition.

use std::path::{Component, Path, PathBuf};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::error::{DeskError, Result};

mod bundled {
    include!(concat!(env!("OUT_DIR"), "/bundled_devices.rs"));
}

/// Paths of bundled files look like `bundled:acme/shared/x.lua`.
pub const BUNDLED_PREFIX: &str = "bundled:";

/// Capability binding keys a definition (and a device) may contain.
pub const BINDING_KEYS: &[&str] = &["power", "source"];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase", tag = "kind", content = "path")]
pub enum Origin {
    /// `devices/` next to the config file.
    Local(PathBuf),
    /// A directory from `$DESK_DEVICES`.
    Path(PathBuf),
    /// Built into this binary.
    Bundled,
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Origin::Local(p) => write!(f, "local {}", p.display()),
            Origin::Path(p) => write!(f, "{}", p.display()),
            Origin::Bundled => f.write_str("bundled"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Definition {
    pub id: String,
    pub name: String,
    pub kind: Option<String>,
    pub status: Option<String>,
    pub tested: Vec<String>,
    pub requires: Option<String>,
    pub origin: Origin,
    /// Where `device.toml` is (a real path, or `bundled:<id>/device.toml`).
    pub file: PathBuf,
    /// Capability key → binding table, with script paths already resolved.
    pub bindings: IndexMap<String, toml::Table>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHeader {
    id: String,
    name: String,
    kind: Option<String>,
    status: Option<String>,
    #[serde(default)]
    tested: Vec<String>,
    requires: Option<String>,
}

enum Root {
    Dir(PathBuf, Origin),
    Bundled,
}

pub struct Library {
    roots: Vec<Root>,
}

impl Library {
    /// The search path for a config file at `config_path`.
    pub fn for_config(config_path: &Path) -> Self {
        let mut roots = Vec::new();
        if let Some(dir) = config_path.parent() {
            let local = dir.join("devices");
            roots.push(Root::Dir(local.clone(), Origin::Local(local)));
        }
        if let Some(paths) = std::env::var_os("DESK_DEVICES") {
            for p in std::env::split_paths(&paths) {
                roots.push(Root::Dir(p.clone(), Origin::Path(p)));
            }
        }
        roots.push(Root::Bundled);
        Library { roots }
    }

    /// The first definition with this id along the search path.
    pub fn find(&self, id: &str) -> Result<Definition> {
        if !valid_definition_id(id) {
            return Err(DeskError::ConfigInvalid(format!(
                "`{id}` is not a definition id (expected `<vendor>/<model>`, e.g. `acme/psu-3000`)"
            )));
        }
        for root in &self.roots {
            if let Some((text, file, origin)) = root.read_definition(id) {
                return parse_definition(id, &text, file, origin);
            }
        }
        Err(DeskError::ConfigInvalid(format!(
            "no device definition `{id}` (see `desk library list`)"
        )))
    }

    /// Every definition, in search order. `shadowed` ones are hidden by an earlier root.
    pub fn list(&self) -> Vec<Listed> {
        let mut out: Vec<Listed> = Vec::new();
        for root in &self.roots {
            for id in root.ids() {
                let shadowed = out.iter().any(|l| l.id == id);
                let result = root
                    .read_definition(&id)
                    .ok_or_else(|| DeskError::Internal(format!("{id} vanished")))
                    .and_then(|(text, file, origin)| parse_definition(&id, &text, file, origin));
                out.push(Listed {
                    id,
                    shadowed,
                    result,
                });
            }
        }
        out
    }
}

/// Load a definition from a file outside the search path (`desk library check path/to/device.toml`).
/// Its id comes from the last two directories: `<vendor>/<model>/device.toml`.
pub fn load_file(path: &Path) -> Result<Definition> {
    let file = if path.is_dir() {
        path.join("device.toml")
    } else {
        path.to_path_buf()
    };
    let text = std::fs::read_to_string(&file)
        .map_err(|e| DeskError::ConfigInvalid(format!("cannot read {}: {e}", file.display())))?;
    let file = std::fs::canonicalize(&file).unwrap_or(file);
    let parts: Vec<String> = file
        .parent()
        .into_iter()
        .flat_map(|p| p.components().rev().take(2))
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let id = match parts.as_slice() {
        [model, vendor] => format!("{vendor}/{model}"),
        _ => {
            return Err(DeskError::ConfigInvalid(format!(
                "{}: expected <vendor>/<model>/device.toml",
                file.display()
            )));
        }
    };
    let origin = Origin::Path(file.parent().map(Path::to_path_buf).unwrap_or_default());
    parse_definition(&id, &text, file, origin)
}

pub struct Listed {
    pub id: String,
    pub shadowed: bool,
    pub result: Result<Definition>,
}

impl Root {
    fn read_definition(&self, id: &str) -> Option<(String, PathBuf, Origin)> {
        match self {
            Root::Dir(dir, origin) => {
                let file = dir.join(id).join("device.toml");
                let text = std::fs::read_to_string(&file).ok()?;
                Some((text, file, origin.clone()))
            }
            Root::Bundled => {
                let rel = format!("{id}/device.toml");
                let text = bundled_file(&rel)?;
                Some((
                    text.to_string(),
                    PathBuf::from(format!("{BUNDLED_PREFIX}{rel}")),
                    Origin::Bundled,
                ))
            }
        }
    }

    /// Ids of all definitions in this root (`<vendor>/<model>` with a `device.toml`).
    fn ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        match self {
            Root::Dir(dir, _) => {
                for vendor in read_dirs(dir) {
                    for model in read_dirs(&vendor) {
                        if model.join("device.toml").is_file() {
                            let name = |p: &Path| {
                                p.file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_default()
                            };
                            ids.push(format!("{}/{}", name(&vendor), name(&model)));
                        }
                    }
                }
            }
            Root::Bundled => {
                for (path, _) in bundled::BUNDLED {
                    if let Some(id) = path.strip_suffix("/device.toml")
                        && id.matches('/').count() == 1
                    {
                        ids.push(id.to_string());
                    }
                }
            }
        }
        ids.sort();
        ids
    }
}

fn read_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    dirs
}

fn bundled_file(rel: &str) -> Option<&'static str> {
    bundled::BUNDLED
        .iter()
        .find(|(p, _)| *p == rel)
        .map(|(_, text)| *text)
}

fn valid_definition_id(id: &str) -> bool {
    let mut parts = id.split('/');
    let ok = |s: Option<&str>| {
        s.is_some_and(|s| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        })
    };
    ok(parts.next()) && ok(parts.next()) && parts.next().is_none()
}

/// Read a script or other file of a binding: a real path, or a bundled one.
pub fn read_text(path: &Path) -> std::io::Result<String> {
    let s = path.to_string_lossy();
    if let Some(rel) = s.strip_prefix(BUNDLED_PREFIX) {
        return bundled_file(rel).map(str::to_string).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "not bundled with this desk")
        });
    }
    std::fs::read_to_string(path)
}

pub fn is_bundled(path: &Path) -> bool {
    path.to_string_lossy().starts_with(BUNDLED_PREFIX)
}

/// Resolve `rel` against the directory of `file`, for real and bundled paths alike.
fn resolve_relative(file: &Path, rel: &str) -> PathBuf {
    if Path::new(rel).is_absolute() {
        return PathBuf::from(rel);
    }
    let file_s = file.to_string_lossy();
    if let Some(inner) = file_s.strip_prefix(BUNDLED_PREFIX) {
        let base = Path::new(inner).parent().unwrap_or(Path::new(""));
        return PathBuf::from(format!(
            "{BUNDLED_PREFIX}{}",
            normalize(&base.join(rel))
                .to_string_lossy()
                .replace('\\', "/")
        ));
    }
    normalize(&file.parent().unwrap_or(Path::new(".")).join(rel))
}

/// Remove `.` and `..` components without touching the file system.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn parse_definition(id: &str, text: &str, file: PathBuf, origin: Origin) -> Result<Definition> {
    let bad =
        |msg: String| DeskError::ConfigInvalid(format!("definition {}: {msg}", file.display()));
    let mut table: toml::Table = toml::from_str(text).map_err(|e| bad(e.to_string()))?;

    let header: RawHeader = table
        .remove("device")
        .ok_or_else(|| bad("missing [device]".into()))?
        .try_into()
        .map_err(|e: toml::de::Error| bad(format!("[device]: {}", e.message())))?;
    if header.id != id {
        return Err(bad(format!(
            "[device] id is `{}`, but the file is at `{id}`",
            header.id
        )));
    }
    if let Some(req) = &header.requires {
        check_requires(req).map_err(bad)?;
    }

    let mut bindings = IndexMap::new();
    for (key, value) in table {
        if !BINDING_KEYS.contains(&key.as_str()) {
            return Err(bad(format!(
                "unknown section `{key}` (allowed: device, {})",
                BINDING_KEYS.join(", ")
            )));
        }
        let toml::Value::Table(mut binding) = value else {
            return Err(bad(format!("`{key}` must be a table")));
        };
        if binding.contains_key("provider") {
            return Err(bad(format!(
                "`{key}` uses a provider; definitions may only use scripts (providers such as \
                 Home Assistant are set up per bench, in the user's own config)"
            )));
        }
        let Some(toml::Value::String(script)) = binding.get("script") else {
            return Err(bad(format!("`{key}` needs `script = \"...\"`")));
        };
        let resolved = resolve_relative(&file, script);
        binding.insert(
            "script".into(),
            toml::Value::String(resolved.to_string_lossy().into_owned()),
        );
        bindings.insert(key, binding);
    }

    Ok(Definition {
        id: header.id,
        name: header.name,
        kind: header.kind,
        status: header.status,
        tested: header.tested,
        requires: header.requires,
        origin,
        file,
        bindings,
    })
}

/// `requires = "desk-operator >= 0.2"`: only `>=` is supported for now.
fn check_requires(req: &str) -> std::result::Result<(), String> {
    let rest = req
        .trim()
        .strip_prefix("desk-operator")
        .map(str::trim)
        .and_then(|r| r.strip_prefix(">="))
        .map(str::trim)
        .ok_or_else(|| format!("`requires = \"{req}\"`: expected \"desk-operator >= x.y\""))?;
    let parse = |v: &str| -> Option<Vec<u64>> { v.split('.').map(|p| p.parse().ok()).collect() };
    let need = parse(rest).ok_or_else(|| format!("`requires`: bad version `{rest}`"))?;
    let have = parse(env!("CARGO_PKG_VERSION")).unwrap_or_default();
    let pad = |v: &[u64]| [0, 1, 2].map(|i| v.get(i).copied().unwrap_or(0));
    if pad(&have) < pad(&need) {
        return Err(format!(
            "needs desk-operator >= {rest}, this is {} (update desk)",
            env!("CARGO_PKG_VERSION")
        ));
    }
    Ok(())
}

/// Merge a device's own binding table over a definition's: tables merge key by key (e.g. adding
/// `port.serial_number`), other values are replaced. A device table with its own `script` or
/// `provider` replaces the definition's binding entirely.
pub fn merge(definition: &toml::Table, device: &toml::Table) -> toml::Table {
    if device.contains_key("script") || device.contains_key("provider") {
        return device.clone();
    }
    deep_merge(definition, device)
}

fn deep_merge(base: &toml::Table, over: &toml::Table) -> toml::Table {
    let mut out = base.clone();
    for (k, v) in over {
        let merged = match (out.get(k), v) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => {
                toml::Value::Table(deep_merge(b, o))
            }
            _ => v.clone(),
        };
        out.insert(k.clone(), merged);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> toml::Table {
        toml::from_str(s).unwrap()
    }

    #[test]
    fn merges_device_over_definition() {
        let def = t(r#"
            script = "bundled:v/shared/d.lua"
            port = { usb_vid = 1, usb_pid = 2 }
            params = { max_voltage = 36.0, status = { output = 107 } }
        "#);
        let dev = t(r#"
            port = { serial_number = "X" }
            params = { max_voltage = 12.0 }
        "#);
        let m = merge(&def, &dev);
        assert_eq!(m["script"].as_str(), Some("bundled:v/shared/d.lua"));
        assert_eq!(m["port"]["usb_vid"].as_integer(), Some(1));
        assert_eq!(m["port"]["serial_number"].as_str(), Some("X"));
        assert_eq!(m["params"]["max_voltage"].as_float(), Some(12.0));
        assert_eq!(m["params"]["status"]["output"].as_integer(), Some(107));

        let own = t(r#"script = "mine.lua""#);
        assert_eq!(merge(&def, &own), own, "own script replaces the definition");
    }

    #[test]
    fn resolves_relative_scripts() {
        assert_eq!(
            resolve_relative(
                Path::new("bundled:acme/psu-3000/device.toml"),
                "../shared/x.lua"
            ),
            PathBuf::from("bundled:acme/shared/x.lua")
        );
        assert_eq!(
            resolve_relative(Path::new("/lib/acme/psu/device.toml"), "../shared/x.lua"),
            PathBuf::from("/lib/acme/shared/x.lua")
        );
    }

    #[test]
    fn requires_versions() {
        assert!(check_requires("desk-operator >= 0.0.1").is_ok());
        assert!(
            check_requires("desk-operator >= 99.0")
                .unwrap_err()
                .contains("update desk")
        );
        assert!(check_requires("desk >= 1").is_err());
    }

    #[test]
    fn ids() {
        assert!(valid_definition_id("acme/psu-3000"));
        assert!(valid_definition_id("acme/psu-150"));
        assert!(!valid_definition_id("acme"));
        assert!(!valid_definition_id("a/b/c"));
        assert!(!valid_definition_id("Acme/PSU"));
        assert!(!valid_definition_id("../x"));
    }
}
