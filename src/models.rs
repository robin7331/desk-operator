//! Models: what's true for every unit of a product, such as "ACME PSU 3000". Found in several
//! places, first match wins:
//!
//! 1. `models/` next to the config file (your own or unpublished models)
//! 2. every directory in `$DESK_MODELS` (e.g. `models/` in a clone of desk)
//! 3. the models bundled into this binary at build time
//!
//! A model is `<vendor>/<model>/model.toml`. It may only contain *script* bindings: how a
//! particular user's device gets mains (their Home Assistant, …) never belongs in a model.

use std::path::{Component, Path, PathBuf};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::error::{DeskError, Result};

mod bundled {
    include!(concat!(env!("OUT_DIR"), "/bundled_models.rs"));
}

/// Paths of bundled files look like `bundled:acme/shared/x.lua`.
pub const BUNDLED_PREFIX: &str = "bundled:";

/// The well-known capability keys; any other key is a declared capability.
pub const BINDING_KEYS: &[&str] = &["power", "source"];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase", tag = "kind", content = "path")]
pub enum Origin {
    /// `models/` next to the config file.
    Local(PathBuf),
    /// A directory from `$DESK_MODELS`.
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

/// How far a model has been verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Written from documentation or community work, not yet run on real hardware.
    Community,
    /// Smoke-tested on real hardware (see `tested` for firmware versions).
    Tested,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Status::Community => "community",
            Status::Tested => "tested",
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub status: Status,
    /// Hardware/firmware versions it was tested with.
    pub tested: Vec<String>,
    /// Manuals, protocol documentation, community work the model is based on.
    pub sources: Vec<String>,
    pub requires: String,
    pub origin: Origin,
    /// Where `model.toml` is (a real path, or `bundled:<id>/model.toml`).
    pub file: PathBuf,
    /// Capability key → binding table, with script paths already resolved.
    pub bindings: IndexMap<String, toml::Table>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHeader {
    id: String,
    name: String,
    status: Status,
    #[serde(default)]
    tested: Vec<String>,
    #[serde(default)]
    sources: Vec<String>,
    requires: String,
}

enum Root {
    Dir(PathBuf, Origin),
    Bundled,
}

pub struct Models {
    roots: Vec<Root>,
}

impl Models {
    /// The search path for a config file at `config_path`.
    pub fn for_config(config_path: &Path) -> Self {
        let mut roots = Vec::new();
        if let Some(dir) = config_path.parent() {
            let local = dir.join("models");
            roots.push(Root::Dir(local.clone(), Origin::Local(local)));
        }
        if let Some(paths) = std::env::var_os("DESK_MODELS") {
            for p in std::env::split_paths(&paths) {
                roots.push(Root::Dir(p.clone(), Origin::Path(p)));
            }
        }
        roots.push(Root::Bundled);
        Models { roots }
    }

    /// The first model with this id along the search path.
    pub fn find(&self, id: &str) -> Result<Model> {
        if !valid_model_id(id) {
            return Err(DeskError::ConfigInvalid(format!(
                "`{id}` is not a model id (expected `<vendor>/<model>`, e.g. `acme/psu-3000`)"
            )));
        }
        for root in &self.roots {
            if let Some((text, file, origin)) = root.read_model(id) {
                return parse_model(id, &text, file, origin);
            }
        }
        Err(DeskError::ConfigInvalid(format!(
            "no model `{id}` (see `desk model list`)"
        )))
    }

    /// Every model, in search order. `shadowed` ones are hidden by an earlier root.
    pub fn list(&self) -> Vec<Listed> {
        let mut out: Vec<Listed> = Vec::new();
        for root in &self.roots {
            for id in root.ids() {
                let shadowed = out.iter().any(|l| l.id == id);
                let result = root
                    .read_model(&id)
                    .ok_or_else(|| DeskError::Internal(format!("{id} vanished")))
                    .and_then(|(text, file, origin)| parse_model(&id, &text, file, origin));
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

/// Load a model from a file outside the search path (`desk model check path/to/model.toml`).
/// Its id comes from the last two directories: `<vendor>/<model>/model.toml`.
pub fn load_file(path: &Path) -> Result<Model> {
    let file = if path.is_dir() {
        path.join("model.toml")
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
                "{}: expected <vendor>/<model>/model.toml",
                file.display()
            )));
        }
    };
    let origin = Origin::Path(file.parent().map(Path::to_path_buf).unwrap_or_default());
    parse_model(&id, &text, file, origin)
}

pub struct Listed {
    pub id: String,
    pub shadowed: bool,
    pub result: Result<Model>,
}

impl Root {
    fn read_model(&self, id: &str) -> Option<(String, PathBuf, Origin)> {
        match self {
            Root::Dir(dir, origin) => {
                let file = dir.join(id).join("model.toml");
                let text = std::fs::read_to_string(&file).ok()?;
                Some((text, file, origin.clone()))
            }
            Root::Bundled => {
                let rel = format!("{id}/model.toml");
                let text = bundled_file(&rel)?;
                Some((
                    text.to_string(),
                    PathBuf::from(format!("{BUNDLED_PREFIX}{rel}")),
                    Origin::Bundled,
                ))
            }
        }
    }

    /// Ids of all models in this root (`<vendor>/<model>` with a `model.toml`).
    fn ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        match self {
            Root::Dir(dir, _) => {
                for vendor in read_dirs(dir) {
                    for model in read_dirs(&vendor) {
                        if model.join("model.toml").is_file() {
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
                    if let Some(id) = path.strip_suffix("/model.toml")
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

pub fn valid_model_id(id: &str) -> bool {
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

fn parse_model(id: &str, text: &str, file: PathBuf, origin: Origin) -> Result<Model> {
    let bad = |msg: String| DeskError::ConfigInvalid(format!("model {}: {msg}", file.display()));
    let mut table: toml::Table = toml::from_str(text).map_err(|e| bad(e.to_string()))?;

    let header: RawHeader = table
        .remove("model")
        .ok_or_else(|| bad("missing [model]".into()))?
        .try_into()
        .map_err(|e: toml::de::Error| bad(format!("[model]: {}", e.message())))?;
    if header.id != id {
        return Err(bad(format!(
            "[model] id is `{}`, but the file is at `{id}`",
            header.id
        )));
    }
    check_requires(&header.requires).map_err(bad)?;
    if header.status == Status::Tested && header.tested.is_empty() {
        return Err(bad(
            "status = \"tested\" needs `tested = [\"<hardware/firmware version>\"]`".into(),
        ));
    }

    let mut bindings = IndexMap::new();
    for (key, value) in table {
        if !crate::config::valid_cap_name(&key) {
            return Err(bad(format!(
                "`{key}` can't be a capability name (lowercase letters, digits, `_`; not `mains`)"
            )));
        }
        let toml::Value::Table(mut binding) = value else {
            return Err(bad(format!("`{key}` must be a table")));
        };
        // Declarations are checked here too, so `models check` reports them.
        let mut rest = binding.clone();
        let decls = crate::declared::Declarations::take(&mut rest)
            .map_err(|e| bad(format!("`{key}`: {e}")))?;
        if !BINDING_KEYS.contains(&key.as_str()) && decls.is_empty() {
            return Err(bad(format!(
                "`{key}` declares no `values`, `settings` or `actions` (a capability other than \
                 power or source must; is `{key}` a typo?)"
            )));
        }
        if binding.contains_key("provider") {
            return Err(bad(format!(
                "`{key}` uses a provider; models may only use scripts (providers such as \
                 Home Assistant are set up per bench, in the user's own config)"
            )));
        }
        let Some(toml::Value::String(script)) = binding.get("script") else {
            return Err(bad(format!("`{key}` needs `script = \"...\"`")));
        };
        required_ratings(&key, &binding).map_err(bad)?;
        if let Some(t) = binding.get("sigrok") {
            crate::sigrok::check_model_table(t).map_err(|e| bad(format!("`{key}`: {e}")))?;
        }
        let resolved = resolve_relative(&file, script);
        binding.insert(
            "script".into(),
            toml::Value::String(resolved.to_string_lossy().into_owned()),
        );
        bindings.insert(key, binding);
    }

    Ok(Model {
        id: header.id,
        name: header.name,
        status: header.status,
        tested: header.tested,
        sources: header.sources,
        requires: header.requires,
        origin,
        file,
        bindings,
    })
}

/// Ratings every model must declare per capability (the device's maximums), so the core can
/// enforce them without trusting a driver.
pub const REQUIRED_RATINGS: &[(&str, &[&str])] = &[("source", &["max_voltage", "max_current"])];

fn required_ratings(key: &str, binding: &toml::Table) -> std::result::Result<(), String> {
    let ratings = match binding.get("ratings") {
        None => toml::Table::new(),
        Some(toml::Value::Table(r)) => r.clone(),
        Some(_) => return Err(format!("`{key}.ratings` must be a table")),
    };
    crate::config::check_ratings(key, &ratings).map_err(|e| format!("`{key}`: {e}"))?;
    for (cap, needed) in REQUIRED_RATINGS {
        if *cap == key {
            let missing: Vec<&str> = needed
                .iter()
                .copied()
                .filter(|n| !ratings.contains_key(*n))
                .collect();
            if !missing.is_empty() {
                return Err(format!(
                    "`{key}` needs `ratings = {{ {} }}` (the device's maximums)",
                    missing
                        .iter()
                        .map(|m| format!("{m} = …"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
    }
    Ok(())
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

const WAY_OUT: &str = "a binding of your own with `script = …` replaces the model's";

fn num(v: &toml::Value) -> Option<f64> {
    match v {
        toml::Value::Float(f) => Some(*f),
        toml::Value::Integer(i) => Some(*i as f64),
        _ => None,
    }
}

fn show(v: &toml::Value) -> String {
    match v {
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Integer(i) => i.to_string(),
        other => other.to_string(),
    }
}

fn too_loose(path: &str, device: &toml::Value, model: &toml::Value, word: &str) -> String {
    format!(
        "`{path}` = {} is {word} the model's {} (a device can tighten what its model states, \
         not loosen it; {WAY_OUT})",
        show(device),
        show(model)
    )
}

fn model_owned(path: &str) -> String {
    format!("`{path}` belongs to the model and can't be changed here ({WAY_OUT})")
}

fn check_switches(
    path: &str,
    model: &toml::Table,
    device: &toml::Table,
) -> std::result::Result<(), String> {
    let lowers = device.get("switches") == Some(&toml::Value::Boolean(false))
        && model.get("switches") != Some(&toml::Value::Boolean(false));
    if lowers {
        return Err(model_owned(&format!("{path}.switches")));
    }
    Ok(())
}

fn check_decl(
    path: &str,
    model: &toml::Table,
    device: &toml::Table,
) -> std::result::Result<(), String> {
    if let (Some(m), Some(d)) = (
        model.get("max").and_then(num),
        device.get("max").and_then(num),
    ) && d > m
    {
        return Err(too_loose(
            &format!("{path}.max"),
            &device["max"],
            &model["max"],
            "above",
        ));
    }
    if let (Some(m), Some(d)) = (
        model.get("min").and_then(num),
        device.get("min").and_then(num),
    ) && d < m
    {
        return Err(too_loose(
            &format!("{path}.min"),
            &device["min"],
            &model["min"],
            "below",
        ));
    }
    for k in ["type", "unit", "units", "choices"] {
        if let Some(d) = device.get(k)
            && model.get(k) != Some(d)
        {
            return Err(model_owned(&format!("{path}.{k}")));
        }
    }
    check_switches(path, model, device)
}

/// Check that a device's own values only tighten what its model states: `ratings` may be lowered,
/// a declared `max` lowered and `min` raised, and a declared name keeps its `type`, `unit`,
/// `units`, `choices` and `switches`. Names and keys the model doesn't have are free.
pub fn check_tightens(
    model: &toml::Table,
    device: &toml::Table,
) -> std::result::Result<(), String> {
    let table = |t: &toml::Table, k: &str| match t.get(k) {
        Some(toml::Value::Table(x)) => Some(x.clone()),
        _ => None,
    };
    if let (Some(m), Some(d)) = (table(model, "ratings"), table(device, "ratings")) {
        for (k, dv) in &d {
            if let Some(mv) = m.get(k)
                && let (Some(dn), Some(mn)) = (num(dv), num(mv))
                && dn > mn
            {
                return Err(too_loose(&format!("ratings.{k}"), dv, mv, "above"));
            }
        }
    }
    for section in ["values", "settings"] {
        let (Some(m), Some(d)) = (table(model, section), table(device, section)) else {
            continue;
        };
        for (name, dd) in &d {
            if let (Some(toml::Value::Table(md)), toml::Value::Table(dd)) = (m.get(name), dd) {
                check_decl(&format!("{section}.{name}"), md, dd)?;
            }
        }
    }
    if let (Some(m), Some(d)) = (table(model, "actions"), table(device, "actions")) {
        for (name, da) in &d {
            let (Some(toml::Value::Table(ma)), toml::Value::Table(da)) = (m.get(name), da) else {
                continue;
            };
            let path = format!("actions.{name}");
            check_switches(&path, ma, da)?;
            if let Some(d) = da.get("produces")
                && ma.get("produces") != Some(d)
            {
                return Err(model_owned(&format!("{path}.produces")));
            }
            if let (Some(margs), Some(dargs)) = (table(ma, "args"), table(da, "args")) {
                for (arg, dd) in &dargs {
                    if let (Some(toml::Value::Table(md)), toml::Value::Table(dd)) =
                        (margs.get(arg), dd)
                    {
                        check_decl(&format!("{path}.args.{arg}"), md, dd)?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Merge a device's own binding table over a model's: tables merge key by key (e.g. adding
/// `port.serial_number`), other values are replaced. A device table with its own `script` or
/// `provider` replaces the model's binding entirely. `check_tightens` guards the model's
/// `ratings` and declarations and runs before this.
pub fn merge(model: &toml::Table, device: &toml::Table) -> toml::Table {
    if device.contains_key("script") || device.contains_key("provider") {
        return device.clone();
    }
    deep_merge(model, device)
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
        assert_eq!(merge(&def, &own), own, "own script replaces the model");
    }

    #[test]
    fn resolves_relative_scripts() {
        assert_eq!(
            resolve_relative(
                Path::new("bundled:acme/psu-3000/model.toml"),
                "../shared/x.lua"
            ),
            PathBuf::from("bundled:acme/shared/x.lua")
        );
        assert_eq!(
            resolve_relative(Path::new("/lib/acme/psu/model.toml"), "../shared/x.lua"),
            PathBuf::from("/lib/acme/shared/x.lua")
        );
    }

    #[test]
    fn devices_tighten_but_never_loosen() {
        let ratings = t("ratings = { max_voltage = 30.0, max_current = 5.0 }");
        let ok = |m: &toml::Table, d: &str| {
            let r = check_tightens(m, &t(d));
            assert!(r.is_ok(), "{d}: {r:?}");
        };
        let err = |m: &toml::Table, d: &str, what: &str| {
            let r = check_tightens(m, &t(d));
            assert!(r.as_ref().is_err_and(|e| e.contains(what)), "{d}: {r:?}");
        };
        ok(&ratings, "ratings = { max_voltage = 12 }");
        ok(&ratings, "ratings = { max_voltage = 30 }");
        err(
            &ratings,
            "ratings = { max_voltage = 100.0 }",
            "ratings.max_voltage",
        );
        ok(&ratings, "ratings = { max_power = 50.0 }");
        ok(
            &ratings,
            "port = { serial_number = \"X\" }\nparams = { max_voltage = 99 }",
        );

        let s = t("[settings]\nrange = { type = \"number\", unit = \"V\", min = 0.1, max = 1000 }");
        ok(&s, "settings = { range = { max = 500 } }");
        err(
            &s,
            "settings = { range = { max = 5000 } }",
            "settings.range.max",
        );
        err(
            &s,
            "settings = { range = { min = 0.01 } }",
            "settings.range.min",
        );
        ok(&s, "settings = { range = { min = 1 } }");
        err(
            &s,
            "settings = { range = { unit = \"mV\" } }",
            "settings.range.unit",
        );
        ok(&s, "settings = { range = { unit = \"V\" } }");
        err(
            &s,
            "settings = { range = { switches = false } }",
            "settings.range.switches",
        );
        ok(&s, "settings = { extra = { type = \"boolean\" } }");

        let a = t(
            "[actions]\nboost = { switches = true, args = { seconds = { type = \"number\", unit = \"s\", min = 1, max = 60 } } }",
        );
        err(
            &a,
            "actions = { boost = { args = { seconds = { max = 600 } } } }",
            "actions.boost.args.seconds.max",
        );
        err(
            &a,
            "actions = { boost = { switches = false } }",
            "actions.boost.switches",
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
        assert!(valid_model_id("acme/psu-3000"));
        assert!(valid_model_id("acme/psu-150"));
        assert!(!valid_model_id("acme"));
        assert!(!valid_model_id("a/b/c"));
        assert!(!valid_model_id("Acme/PSU"));
        assert!(!valid_model_id("../x"));
    }
}
