//! `desk library …`: device definitions (what's true for every unit of a model).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use indexmap::IndexMap;
use serde::Serialize;

use super::LibraryCmd;
use crate::config::{self, Binding};
use crate::error::{DeskError, ErrorJson, Result};
use crate::library::{self, Definition, Library};
use crate::output::{Report, table};
use crate::providers::Registry;
use crate::script::{DeviceInfo, ScriptSource, ScriptSpec, ScriptSwitch};

pub fn run(config_path: Option<&Path>, cmd: LibraryCmd) -> Result<Report> {
    let config_path = config_path
        .map(Path::to_path_buf)
        .or_else(config::default_path)
        .unwrap_or_else(|| PathBuf::from("desk.toml"));
    let library = Library::for_config(&config_path);
    match cmd {
        LibraryCmd::List => list(&library),
        LibraryCmd::Show { id } => show(&library.find(&id)?),
        LibraryCmd::Check { targets } => check(&library, &targets),
    }
}

#[derive(Serialize)]
struct ListEntry<'a> {
    id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    origin: Option<String>,
    capabilities: Vec<&'a str>,
    /// Hidden by a definition with the same id earlier in the search path.
    shadowed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorJson>,
}

fn capabilities(def: &Definition) -> Vec<&'static str> {
    def.bindings
        .keys()
        .filter_map(|k| match k.as_str() {
            "power" => Some("power_switch"),
            "source" => Some("voltage_source"),
            _ => None,
        })
        .collect()
}

fn list(library: &Library) -> Result<Report> {
    let listed = library.list();
    let caps: Vec<Vec<&str>> = listed
        .iter()
        .map(|l| l.result.as_ref().map(capabilities).unwrap_or_default())
        .collect();
    let entries: Vec<ListEntry> = listed
        .iter()
        .zip(&caps)
        .map(|(l, caps)| {
            let def = l.result.as_ref().ok();
            ListEntry {
                id: &l.id,
                name: def.map(|d| d.name.as_str()),
                kind: def.and_then(|d| d.kind.as_deref()),
                status: def.and_then(|d| d.status.as_deref()),
                origin: def.map(|d| d.origin.to_string()),
                capabilities: caps.clone(),
                shadowed: l.shadowed,
                error: l.result.as_ref().err().map(DeskError::to_json),
            }
        })
        .collect();
    let rows: Vec<Vec<String>> = entries
        .iter()
        .map(|e| {
            let from = match (&e.origin, e.shadowed) {
                (Some(o), true) => format!("{o} (shadowed)"),
                (Some(o), false) => o.clone(),
                (None, _) => "-".into(),
            };
            vec![
                e.id.to_string(),
                e.name.unwrap_or("-").to_string(),
                e.status.unwrap_or("-").to_string(),
                match &e.error {
                    Some(err) => format!("error: {}", err.message),
                    None => from,
                },
            ]
        })
        .collect();
    let mut human = if rows.is_empty() {
        "no device definitions found\n".to_string()
    } else {
        table(&["DEFINITION", "NAME", "STATUS", "FROM"], &rows)
    };
    if !rows.is_empty() {
        human.push_str("\nuse one in a device with: model = \"<DEFINITION>\" (details: desk library show <id>)\n");
    }
    Ok(Report::new(
        &serde_json::json!({ "definitions": entries }),
        human,
    ))
}

fn show(def: &Definition) -> Result<Report> {
    let mut human = format!(
        "id:           {}\nname:         {}\nkind:         {}\nstatus:       {}\nfrom:         {}\nfile:         {}\n",
        def.id,
        def.name,
        def.kind.as_deref().unwrap_or("-"),
        def.status.as_deref().unwrap_or("-"),
        def.origin,
        def.file.display(),
    );
    if !def.tested.is_empty() {
        human.push_str(&format!("tested:       {}\n", def.tested.join(", ")));
    }
    for (key, binding) in &def.bindings {
        // Serialize under its capability key, so nested tables print as [source.params] etc.
        let mut wrapped = toml::Table::new();
        wrapped.insert(key.clone(), toml::Value::Table(binding.clone()));
        human.push('\n');
        human.push_str(&toml::to_string(&wrapped).unwrap_or_default());
    }
    human.push_str(&format!(
        "\nuse it:\n  [devices.<name>]\n  model = \"{}\"\n  # plus your own power path, limits, and e.g. port = {{ serial_number = \"…\" }}\n",
        def.id
    ));
    Ok(Report::new(
        &serde_json::json!({ "definition": def }),
        human,
    ))
}

#[derive(Serialize)]
struct CheckResult {
    target: String,
    ok: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    capabilities: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorJson>,
}

/// Check every definition (or the given ids / paths): format, and that each driver loads in the
/// sandbox and defines the functions its capability needs. Never touches hardware.
fn check(library: &Library, targets: &[String]) -> Result<Report> {
    let defs: Vec<(String, Result<Definition>)> = if targets.is_empty() {
        library
            .list()
            .into_iter()
            .filter(|l| !l.shadowed)
            .map(|l| (l.id, l.result))
            .collect()
    } else {
        targets
            .iter()
            .map(|t| {
                let p = Path::new(t);
                let def = if p.exists() {
                    library::load_file(p)
                } else {
                    library.find(t)
                };
                (t.clone(), def)
            })
            .collect()
    };

    let mut results = Vec::new();
    let mut first_error: Option<DeskError> = None;
    for (target, def) in defs {
        let outcome = def.and_then(|d| check_drivers(&d).map(|()| capabilities(&d)));
        match outcome {
            Ok(caps) => results.push(CheckResult {
                target,
                ok: true,
                capabilities: caps,
                error: None,
            }),
            Err(e) => {
                results.push(CheckResult {
                    target,
                    ok: false,
                    capabilities: Vec::new(),
                    error: Some(e.to_json()),
                });
                first_error.get_or_insert(e);
            }
        }
    }

    let rows: Vec<Vec<String>> = results
        .iter()
        .map(|r| {
            vec![
                r.target.clone(),
                match &r.error {
                    None => format!("ok ({})", r.capabilities.join(", ")),
                    Some(e) => format!("error: {}", e.message),
                },
            ]
        })
        .collect();
    let human = table(&["DEFINITION", "CHECK"], &rows);
    let failed = results.iter().filter(|r| !r.ok).count();
    let failure = first_error.map(|e| {
        let mut err = e.to_json();
        if results.len() > 1 {
            err.message = format!(
                "{failed} of {} failed; first: {}",
                results.len(),
                err.message
            );
        }
        (err, e.exit_code())
    });
    Ok(Report::new(&serde_json::json!({ "results": results }), human).with_failure(failure))
}

fn check_drivers(def: &Definition) -> Result<()> {
    let providers = Arc::new(Registry::new(IndexMap::new(), PathBuf::new()));
    for (key, table) in &def.bindings {
        let binding = Binding::try_from(table.clone())
            .map_err(|e| DeskError::ConfigInvalid(format!("`{key}`: {e}")))?;
        let Binding::Script {
            script,
            params,
            port,
            ..
        } = &binding
        else {
            continue;
        };
        let spec = ScriptSpec {
            device: DeviceInfo {
                id: "check",
                name: &def.name,
            },
            path: script,
            params,
            port: port.as_ref(),
            timeout: Duration::from_secs(5),
            providers: providers.clone(),
        };
        match key.as_str() {
            "power" => ScriptSwitch::load(spec).map(drop)?,
            "source" => ScriptSource::load(spec).map(drop)?,
            _ => {}
        }
    }
    Ok(())
}
