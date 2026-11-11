//! `desk model …`: models (what's true for every unit of a model).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use indexmap::IndexMap;
use serde::Serialize;

use super::ModelCmd;
use crate::config::{self, Binding};
use crate::error::{DeskError, ErrorJson, Result};
use crate::models::{self, Model, Models};
use crate::output::{Report, table};
use crate::providers::Registry;
use crate::script::{DeviceInfo, ParamSpec, ScriptDriver, ScriptSource, ScriptSpec, ScriptSwitch};

pub fn run(config_path: Option<&Path>, cmd: ModelCmd) -> Result<Report> {
    let config_path = config_path
        .map(Path::to_path_buf)
        .or_else(config::default_path)
        .unwrap_or_else(|| PathBuf::from("desk.toml"));
    let models = Models::for_config(&config_path);
    match cmd {
        ModelCmd::List => list(&models),
        ModelCmd::Show { id } => {
            let notices = notices(&models.list())
                .into_iter()
                .filter(|(of, _)| *of == id)
                .map(|(_, notice)| notice)
                .collect();
            show(&models.find(&id)?, notices)
        }
        ModelCmd::Check { targets } => check(&models, &targets),
        ModelCmd::New {
            id,
            capability,
            name,
            driver,
            max_voltage,
            max_current,
            dir,
        } => {
            let root = dir.unwrap_or_else(|| {
                config_path
                    .parent()
                    .map(|p| p.join("models"))
                    .unwrap_or_else(|| PathBuf::from("models"))
            });
            scaffold(
                &root,
                &id,
                &capability,
                name,
                driver,
                max_voltage,
                max_current,
            )
        }
    }
}

#[derive(Serialize)]
struct ListEntry<'a> {
    id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    origin: Option<String>,
    capabilities: Vec<&'a str>,
    /// Hidden by a model with the same id earlier in the search path.
    shadowed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorJson>,
}

fn capabilities(def: &Model) -> Vec<&'static str> {
    def.bindings
        .keys()
        .filter_map(|k| match k.as_str() {
            "power" => Some("power_switch"),
            "source" => Some("voltage_source"),
            _ => None,
        })
        .collect()
}

fn list(models: &Models) -> Result<Report> {
    let listed = models.list();
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
                status: def.map(|d| d.status.to_string()),
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
                e.status.clone().unwrap_or_else(|| "-".into()),
                e.capabilities.join(", "),
                match &e.error {
                    Some(err) => format!("error: {}", err.message),
                    None => from,
                },
            ]
        })
        .collect();
    let mut human = if rows.is_empty() {
        "no models found\n".to_string()
    } else {
        table(&["MODEL", "NAME", "STATUS", "CAPABILITIES", "FROM"], &rows)
    };
    if !rows.is_empty() {
        human.push_str(
            "\nuse one in a device with: model = \"<MODEL>\" (details: desk model show <id>)\n",
        );
    }
    let notices: Vec<String> = notices(&listed).into_iter().map(|(_, n)| n).collect();
    for notice in &notices {
        human.push_str(&format!("\nnote: {notice}\n"));
    }
    Ok(Report::new(
        &serde_json::json!({ "models": entries, "notices": notices }),
        human,
    ))
}

/// A copy in `models/` next to the config keeps overriding a bundled model after a release brought
/// that model, so an old driver can linger unnoticed. `(model id, notice)` for each such copy.
fn notices(listed: &[models::Listed]) -> Vec<(String, String)> {
    let bundled = |l: &models::Listed| {
        l.shadowed
            && l.result
                .as_ref()
                .is_ok_and(|m| matches!(m.origin, models::Origin::Bundled))
    };
    listed
        .iter()
        .filter(|l| !l.shadowed)
        .filter(|l| listed.iter().any(|o| o.id == l.id && bundled(o)))
        .filter_map(|l| match &l.result.as_ref().ok()?.origin {
            models::Origin::Local(dir) => Some((
                l.id.clone(),
                format!(
                    "`{}`: the copy in {} overrides the model bundled into desk. Delete the copy \
                     unless it still has changes you need",
                    l.id,
                    dir.display()
                ),
            )),
            _ => None,
        })
        .collect()
}

fn show(def: &Model, notices: Vec<String>) -> Result<Report> {
    let mut human = format!(
        "id:           {}\nname:         {}\nstatus:       {}\ncapabilities: {}\nrequires:     {}\nfrom:         {}\nfile:         {}\n",
        def.id,
        def.name,
        def.status,
        capabilities(def).join(", "),
        def.requires,
        def.origin,
        def.file.display(),
    );
    if !def.tested.is_empty() {
        human.push_str(&format!("tested:       {}\n", def.tested.join(", ")));
    }
    for (i, src) in def.sources.iter().enumerate() {
        let label = if i == 0 { "sources:" } else { "" };
        human.push_str(&format!("{label:<14}{src}\n"));
    }
    for (key, binding) in &def.bindings {
        // Serialize under its capability key, so nested tables print as [source.params] etc.
        let mut wrapped = toml::Table::new();
        wrapped.insert(key.clone(), toml::Value::Table(binding.clone()));
        human.push('\n');
        human.push_str(&toml::to_string(&wrapped).unwrap_or_default());
    }
    // What the drivers accept, from their PARAMS declarations.
    let mut drivers = serde_json::Map::new();
    for (key, params) in driver_params(def) {
        match &params {
            Ok(specs) => {
                human.push_str(&format!("\n{key} driver params:\n"));
                let rows: Vec<Vec<String>> = specs
                    .iter()
                    .map(|p| {
                        vec![
                            format!("  {}", p.name),
                            p.kind.clone(),
                            match (&p.default, p.required) {
                                (Some(d), _) => format!("default {d}"),
                                (None, true) => "required".into(),
                                (None, false) => "optional".into(),
                            },
                            p.doc.clone().unwrap_or_default(),
                        ]
                    })
                    .collect();
                if rows.is_empty() {
                    human.push_str("  (none)\n");
                } else {
                    human.push_str(&table(&["  NAME", "TYPE", "", "MEANING"], &rows));
                }
            }
            Err(e) => human.push_str(&format!("\n{key} driver: error: {e}\n")),
        }
        drivers.insert(
            key,
            match params {
                Ok(p) => serde_json::to_value(p).unwrap_or_default(),
                Err(e) => serde_json::json!({ "error": e.to_json() }),
            },
        );
    }
    human.push_str(&format!(
        "\nuse it:\n  [devices.<name>]\n  model = \"{}\"\n  # plus your own power path, limits, and e.g. port = {{ serial_number = \"…\" }}\n",
        def.id
    ));
    for notice in &notices {
        human.push_str(&format!("\nnote: {notice}\n"));
    }
    Ok(Report::new(
        &serde_json::json!({ "model": def, "driver_params": drivers, "notices": notices }),
        human,
    ))
}

/// Load each script binding's driver (sandboxed, no hardware) and return its declared params.
fn driver_params(def: &Model) -> Vec<(String, Result<Vec<ParamSpec>>)> {
    let providers = Arc::new(Registry::new(IndexMap::new(), PathBuf::new()));
    def.bindings
        .iter()
        .map(|(key, table)| {
            let mut table = table.clone();
            let decls = crate::declared::Declarations::take(&mut table);
            let result = decls
                .map_err(DeskError::ConfigInvalid)
                .and_then(|decls| {
                    Binding::try_from(table)
                        .map(|b| (b, decls))
                        .map_err(DeskError::ConfigInvalid)
                })
                .and_then(|(binding, decls)| {
                    let Binding::Script {
                        script,
                        params,
                        connections,
                        ..
                    } = &binding
                    else {
                        return Ok(Vec::new());
                    };
                    let spec = spec_for(def, script, params, connections, &providers);
                    let declared = match key.as_str() {
                        "power" => ScriptSwitch::load(spec)?.params().map(<[_]>::to_vec),
                        "source" => ScriptSource::load(spec)?.params().map(<[_]>::to_vec),
                        _ => ScriptDriver::load(spec, &decls)?
                            .params()
                            .map(<[_]>::to_vec),
                    };
                    declared.ok_or_else(|| {
                        DeskError::ConfigInvalid(format!(
                            "{}: model drivers must declare `PARAMS = {{ … }}` (may be empty)",
                            script.display()
                        ))
                    })
                });
            (key.clone(), result)
        })
        .collect()
}

fn spec_for<'a>(
    def: &'a Model,
    script: &'a Path,
    params: &'a toml::Table,
    connections: &'a crate::connections::Connections,
    providers: &Arc<Registry>,
) -> ScriptSpec<'a> {
    ScriptSpec {
        device: DeviceInfo {
            id: "check",
            name: &def.name,
        },
        path: script,
        params,
        connections,
        timeout: Duration::from_secs(5),
        providers: providers.clone(),
    }
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

/// Check every model (or the given ids / paths): format, and that each driver loads in the
/// sandbox and defines the functions its capability needs. Never touches hardware.
fn check(models: &Models, targets: &[String]) -> Result<Report> {
    let defs: Vec<(String, Result<Model>)> = if targets.is_empty() {
        models
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
                    models::load_file(p)
                } else {
                    models.find(t)
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
    let human = table(&["MODEL", "CHECK"], &rows);
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

/// Load every driver: required functions present, `PARAMS` declared, the model's params valid.
fn check_drivers(def: &Model) -> Result<()> {
    for (_, result) in driver_params(def) {
        result?;
    }
    Ok(())
}

const DRIVER_SOURCE: &str = r#"-- Driver for {name} (voltage_source). Contract: docs/scripting.md.
-- desk does limits, ordering (voltage, current, output) and verification; the driver only talks
-- to the device. Return from read() only what the device really reports.

-- Every param the model may set, validated by desk (unknown or mistyped params are errors).
PARAMS = {
  -- example = { type = "number", default = 300, doc = "what it means" },
}

-- Optional: start/end of one command's session (connect, handshake, release the front panel).
-- function open(ctx) end
-- function close(ctx) end

function set_voltage(ctx, volts)
  error("not implemented: set_voltage")
end

function set_current(ctx, amps)
  error("not implemented: set_current")
end

function set_output(ctx, on)
  error("not implemented: set_output")
end

function read(ctx)
  -- e.g. return { output = true, voltage_set = 5.0, current_set = 1.0, voltage = 4.99, current = 0.1, power = 0.5 }
  error("not implemented: read")
end
"#;

const POWER_DRIVER_SOURCE: &str = r#"-- Driver for {name} (power_switch). Contract: docs/scripting.md.

PARAMS = {}

function on(ctx)
  error("not implemented: on")
end

function off(ctx)
  error("not implemented: off")
end

-- Optional, enables read-back verification and toggle: return "on", "off" or "unknown".
-- function state(ctx) end
"#;

const DECLARED_DRIVER_SOURCE: &str = r#"-- Driver for {name} ({key}, a declared capability). Contract: docs/scripting.md.
-- desk parses units, checks ranges and limits, applies settings in declaration order (a
-- switch-off first) and verifies them by reading back; the driver only talks to the device.
-- Return from read() only what the device really reports.

PARAMS = {
  -- example = { type = "number", default = 300, doc = "what it means" },
}

-- Optional: start/end of a session (connect, handshake).
-- function open(ctx) end
-- function close(ctx) end

-- Every declared value and setting the device reports, in the declared unit.
function read(ctx)
  error("not implemented: read")
end

-- Needed once there are settings / actions / streams:
-- function set(ctx, name, value) end
-- function action(ctx, name, args) end
-- function stream(ctx, name, args) end   -- ctx.describe{ into = ctx.output, … }, then fill it
"#;

const VENDOR_README: &str = r#"# {vendor}

| Model | Name | Status |
|---|---|---|
| `{id}` | {model} | community, not yet tested on hardware |

## Protocol

How the devices talk (transport, framing, commands), with sources.

## Known gaps

## Observed behavior

What a smoke test on real hardware showed (boot time, ramps, quirks), with firmware versions.
"#;

fn scaffold(
    root: &Path,
    id: &str,
    capability: &str,
    name: Option<String>,
    driver: Option<String>,
    max_voltage: Option<f64>,
    max_current: Option<f64>,
) -> Result<Report> {
    if !models::valid_model_id(id) {
        return Err(DeskError::Usage(format!(
            "`{id}` is not a model id: use vendor/model (lowercase, digits, `-`, `_`)"
        )));
    }
    let (vendor, model) = id.split_once('/').unwrap_or((id, id));
    let dir = root.join(vendor).join(model);
    let file = dir.join("model.toml");
    if file.exists() {
        return Err(DeskError::Usage(format!(
            "{} already exists",
            file.display()
        )));
    }
    std::fs::create_dir_all(&dir)?;
    let mut created = Vec::new();

    let (key, template) = match capability {
        "voltage_source" | "source" => ("source", DRIVER_SOURCE),
        "power_switch" | "power" => ("power", POWER_DRIVER_SOURCE),
        other if crate::config::valid_cap_name(other) => (other, DECLARED_DRIVER_SOURCE),
        other => {
            return Err(DeskError::Usage(format!(
                "`{other}` can't be a capability name: voltage_source, power_switch, or a name \
                 of lowercase letters, digits and `_` (e.g. heater)"
            )));
        }
    };
    if (max_voltage.is_some() || max_current.is_some()) && key != "source" {
        return Err(DeskError::Usage(
            "--max-voltage/--max-current are for voltage_source; declared capabilities give \
             ranges per setting (min/max)"
                .into(),
        ));
    }
    let script = match driver {
        Some(d) => d,
        None => {
            let path = dir.join("driver.lua");
            std::fs::write(&path, template.replace("{name}", id).replace("{key}", key))?;
            created.push(path);
            "driver.lua".to_string()
        }
    };
    let ratings = match key {
        "source" => {
            let v = |x: Option<f64>| x.map_or("0.0".to_string(), |x| format!("{x:?}"));
            let todo = if max_voltage.is_none() || max_current.is_none() {
                "   # TODO: the device's maximums"
            } else {
                ""
            };
            format!(
                "ratings = {{ max_voltage = {}, max_current = {} }}{todo}\n",
                v(max_voltage),
                v(max_current)
            )
        }
        "power" => String::new(),
        _ => format!(
            r#"
# What the device can do (docs/model-format.md). At least one declaration; replace this one.
[{key}.values]
reading = {{ type = "number", unit = "V", doc = "TODO" }}

# [{key}.settings]
# setpoint = {{ type = "number", unit = "°C", min = 50, max = 450, doc = "…" }}   # switches = true by default

# [{key}.actions]
# standby = {{ switches = true, doc = "…" }}

# [{key}.streams]
# samples = {{ content = "logic", doc = "…" }}   # live data for `desk stream`
"#
        ),
    };
    let version = env!("CARGO_PKG_VERSION");
    let requires = version.rsplit_once('.').map_or(version, |(mm, _)| mm);
    // Point editors at the JSON Schema when creating inside a clone of desk (schemas/ next to
    // the models folder); elsewhere there's nothing to point at.
    let schema = if root.join("../schemas/model.schema.json").is_file() {
        "#:schema ../../../schemas/model.schema.json\n"
    } else {
        ""
    };
    let (name, name_todo) = match name {
        Some(n) => (n, ""),
        None => (
            model.to_string(),
            "   # TODO: the model's name, e.g. \"Vendor Model 3000\"",
        ),
    };
    std::fs::write(
        &file,
        format!(
            r#"{schema}# Model: true for every unit of this model, nothing about any particular bench.
# Format: docs/model-format.md

[model]
id       = "{id}"
name     = {name:?}{name_todo}
requires = "desk-operator >= {requires}"
status   = "community"   # "tested" after a smoke test on real hardware (then list versions in `tested`)
sources  = []            # manuals, protocol docs, community work this is based on

[{key}]
script = "{script}"
# port = {{ usb_vid = 0x0000, usb_pid = 0x0000 }}   # from `desk port list`, if the device has a USB id
{ratings}"#,
        ),
    )?;
    created.push(file.clone());

    let readme = root.join(vendor).join("README.md");
    if !readme.exists() {
        std::fs::write(
            &readme,
            VENDOR_README
                .replace("{vendor}", vendor)
                .replace("{id}", id)
                .replace("{model}", model),
        )?;
        created.push(readme);
    }

    // Check what was created, so the next steps are concrete.
    let check = models::load_file(&file).and_then(|d| check_drivers(&d));
    let mut human: String = created
        .iter()
        .map(|p| format!("created {}\n", p.display()))
        .collect();
    match &check {
        Ok(()) => human.push_str("check: ok\n"),
        Err(e) => human.push_str(&format!("check: {e}\n")),
    }
    human.push_str(&format!(
        "\nnext: fill in {}, implement the driver, then `desk model check {}`\n",
        file.display(),
        file.display()
    ));
    Ok(Report::new(
        &serde_json::json!({
            "id": id,
            "created": created,
            "check": match &check { Ok(()) => serde_json::json!({ "ok": true }), Err(e) => serde_json::json!({ "ok": false, "error": e.to_json() }) },
        }),
        human,
    ))
}
