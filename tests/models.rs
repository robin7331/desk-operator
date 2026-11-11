//! Models: `model = "…"`, merging, search path, `desk model list|show|check`.

use std::path::Path;
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

/// A PSU simulated inside the driver (state lives for one command).
const SIM_DRIVER: &str = r#"
PARAMS = { flavor = { type = "string", default = "model", doc = "which copy of the driver" } }
local s = { v = 0, a = 0, out = false }
function set_voltage(ctx, v) s.v = v end
function set_current(ctx, a) s.a = a end
function set_output(ctx, on) s.out = on end
function read(ctx)
  ctx.log("driver " .. ctx.params.flavor)
  return { output = s.out, voltage_set = s.v, current_set = s.a, voltage = s.out and s.v or 0 }
end
"#;

const MODEL: &str = r#"
[model]
id       = "acme/psu-3000"
name     = "ACME PSU 3000"
requires = "desk-operator >= 0.1"
status   = "community"
sources  = ["https://example.com/acme-psu-3000-manual.pdf"]

[source]
script  = "../shared/sim.lua"
ratings = { max_voltage = 30.0, max_current = 5.0 }
"#;

struct Bench {
    dir: TempDir,
}

struct Out {
    code: i32,
    stdout: String,
}

impl Out {
    fn json(&self) -> Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("stdout is not JSON ({e}):\n{}", self.stdout))
    }
    fn message(&self) -> String {
        self.json()["error"]["message"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }
}

impl Bench {
    /// A config with `devices_toml` and the local model `acme/psu-3000`.
    fn new(devices_toml: &str) -> Self {
        let dir = TempDir::new().unwrap();
        let b = Bench { dir };
        b.write("models/acme/shared/sim.lua", SIM_DRIVER);
        b.write("models/acme/psu-3000/model.toml", MODEL);
        b.write("desk.toml", devices_toml);
        b
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.dir.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn run(&self, args: &[&str]) -> Out {
        self.run_env(args, &[])
    }

    fn run_env(&self, args: &[&str], env: &[(&str, &Path)]) -> Out {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_desk"));
        cmd.env_remove("DESK_CONFIG")
            .env_remove("DESK_MODELS")
            .env("DESK_MOCK_STATE", self.dir.path().join("mock-state.json"))
            .arg("--config")
            .arg(self.dir.path().join("desk.toml"));
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.args(args).output().unwrap();
        Out {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        }
    }
}

#[test]
fn model_supplies_bindings_and_name() {
    let b = Bench::new(
        "[devices.psu]\nmodel = \"acme/psu-3000\"\nlimits = { source = { voltage = 12 } }\n",
    );
    let d = b.run(&["--json", "device", "show", "psu"]).json();
    let dev = &d["data"]["device"];
    assert_eq!(dev["model"], "acme/psu-3000");
    assert_eq!(dev["name"], "ACME PSU 3000", "name comes from the model");
    assert_eq!(dev["capabilities"][0], "voltage_source");

    let out = b.run(&["--json", "set", "psu", "voltage", "5v", "output", "on"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.json()["data"]["verified"]["voltage"], true);
    // The device's own bench limits still apply.
    assert_eq!(b.run(&["--json", "set", "psu", "voltage", "13v"]).code, 8);
}

#[test]
fn own_values_merge_over_the_model() {
    let b = Bench::new(
        "[devices.psu]\nmodel = \"acme/psu-3000\"\nname = \"Mine\"\nsource = { ratings = { max_voltage = 12.0 } }\n",
    );
    let out = b.run(&["--json", "set", "psu", "voltage", "13v"]);
    assert_eq!(out.code, 8, "{}", out.stdout);
    assert!(
        out.message().contains("device maximum of 12.00 V"),
        "{}",
        out.message()
    );
    let d = b.run(&["--json", "device", "show", "psu"]).json();
    assert_eq!(d["data"]["device"]["name"], "Mine");
    let source = &d["data"]["device"]["source"];
    assert!(
        source["script"]
            .as_str()
            .unwrap()
            .replace('\\', "/")
            .ends_with("acme/shared/sim.lua"),
        "{source}"
    );
}

#[test]
fn a_device_cannot_raise_the_models_ratings() {
    let b = Bench::new(
        "[devices.psu]\nmodel = \"acme/psu-3000\"\nsource = { ratings = { max_voltage = 100.0 } }\n",
    );
    let out = b.run(&["--json", "get", "psu"]);
    assert_eq!(
        out.json()["error"]["code"],
        "config_invalid",
        "{}",
        out.stdout
    );
    assert!(out.message().contains("max_voltage"), "{}", out.message());
    assert!(out.message().contains("model"), "{}", out.message());
}

#[test]
fn own_script_replaces_the_model() {
    let b = Bench::new(
        "[devices.psu]\nmodel = \"acme/psu-3000\"\nsource = { script = \"mine.lua\", params = { flavor = \"mine\" } }\n",
    );
    b.write("mine.lua", SIM_DRIVER);
    let out = b.run(&["--json", "get", "psu"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.json()["data"]["log"][0], "driver mine");
}

#[test]
fn unknown_models_are_config_errors() {
    let b = Bench::new("[devices.psu]\nmodel = \"acme/nope\"\n");
    let out = b.run(&["--json", "device", "list"]);
    assert_eq!(out.code, 3);
    assert!(
        out.message().contains("no model `acme/nope`"),
        "{}",
        out.message()
    );
    assert!(out.message().contains("desk model list"));

    let b = Bench::new("[devices.psu]\nmodel = \"NotAnId\"\n");
    assert!(
        b.run(&["--json", "device", "list"])
            .message()
            .contains("not a model id")
    );
}

/// The models bundled into desk (from `models/`; the core's tests don't depend on which ones
/// it has).
fn bundled(b: &Bench) -> Vec<Value> {
    let j = b.run(&["--json", "model", "list"]).json();
    let all: Vec<Value> = j["data"]["models"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["origin"] == "bundled")
        .cloned()
        .collect();
    assert!(!all.is_empty(), "no bundled models");
    all
}

/// A bundled model with a `source` binding, its id and `models show` data.
fn bundled_source(b: &Bench) -> (String, Value) {
    bundled(b)
        .iter()
        .map(|d| d["id"].as_str().unwrap().to_string())
        .find_map(|id| {
            let d = b.run(&["--json", "model", "show", &id]).json()["data"].clone();
            d["model"]["bindings"]["source"]
                .is_object()
                .then_some((id, d))
        })
        .expect("a bundled power supply model")
}

#[test]
fn local_models_shadow_bundled_ones() {
    let probe = Bench::new("");
    let (id, _) = bundled_source(&probe);
    let b = Bench::new(&format!("[devices.psu]\nmodel = \"{id}\"\n"));
    b.write(
        &format!("models/{id}/model.toml"),
        &MODEL
            .replace("acme/psu-3000", &id)
            .replace("../shared/sim.lua", "../../acme/shared/sim.lua"),
    );
    let j = b.run(&["--json", "model", "list"]).json();
    let same: Vec<&Value> = j["data"]["models"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["id"] == id.as_str())
        .collect();
    assert_eq!(same.len(), 2, "{same:?}");
    assert!(same[0]["origin"].as_str().unwrap().starts_with("local"));
    assert_eq!(same[0]["shadowed"], false);
    assert_eq!(same[1]["origin"], "bundled");
    assert_eq!(same[1]["shadowed"], true);

    // A notice, not a failure: named in `list` and `show`, in both outputs.
    let notice = j["data"]["notices"][0].as_str().unwrap().to_string();
    assert!(notice.contains(&id), "{notice}");
    assert!(notice.contains("Delete"), "{notice}");
    assert_eq!(j["ok"], true);
    assert!(b.run(&["model", "list"]).stdout.contains(&notice));
    let shown = b.run(&["--json", "model", "show", &id]).json();
    assert_eq!(shown["ok"], true);
    assert_eq!(shown["data"]["notices"][0], notice.as_str());
    assert!(b.run(&["model", "show", &id]).stdout.contains(&notice));

    // Nothing to report for a model that only exists once.
    let other = bundled(&b)
        .iter()
        .map(|d| d["id"].as_str().unwrap().to_string())
        .find(|other| *other != id);
    if let Some(other) = other {
        let shown = b.run(&["--json", "model", "show", &other]).json();
        assert_eq!(shown["data"]["notices"], Value::Array(Vec::new()));
    }

    // The device uses the local one (a simulated PSU: no hardware needed).
    let out = b.run(&["--json", "get", "psu"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
}

#[test]
fn models_may_not_contain_bench_specific_bindings() {
    let b = Bench::new("[devices.x]\nmodel = \"acme/relay\"\n");
    b.write(
        "models/acme/relay/model.toml",
        "[model]\nid = \"acme/relay\"\nname = \"Relay\"\nrequires = \"desk-operator >= 0.1\"\nstatus = \"community\"\n[power]\nprovider = \"ha\"\nentity = \"switch.x\"\n",
    );
    let out = b.run(&["--json", "device", "list"]);
    assert_eq!(out.code, 3);
    assert!(
        out.message().contains("models may only use scripts"),
        "{}",
        out.message()
    );
}

#[test]
fn malformed_models() {
    let b = Bench::new("");
    b.write(
        "models/acme/wrong-id/model.toml",
        &MODEL.replace("acme/psu-3000", "acme/other"),
    );
    b.write(
        "models/acme/too-new/model.toml",
        &MODEL
            .replace("acme/psu-3000", "acme/too-new")
            .replace(">= 0.1", ">= 99.0"),
    );
    b.write(
        "models/acme/typo/model.toml",
        &MODEL
            .replace("acme/psu-3000", "acme/typo")
            .replace("[source]", "[sauce]"),
    );
    let j = b.run(&["--json", "model", "list"]).json();
    let error_of = |id: &str| {
        j["data"]["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["id"] == id)
            .unwrap()["error"]["message"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert!(error_of("acme/wrong-id").contains("id is `acme/other`"));
    assert!(error_of("acme/too-new").contains("update desk"));
    assert!(
        error_of("acme/typo").contains("is `sauce` a typo"),
        "{}",
        error_of("acme/typo")
    );
}

#[test]
fn check_loads_every_driver() {
    let b = Bench::new("");
    let out = b.run(&["--json", "model", "check"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let results = out.json()["data"]["results"].as_array().unwrap().clone();
    let r = results
        .iter()
        .find(|r| r["target"] == "acme/psu-3000")
        .unwrap();
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["capabilities"][0], "voltage_source");
    // Every bundled model loads, whatever the models has.
    for d in bundled(&b) {
        let r = results
            .iter()
            .find(|r| r["target"] == d["id"])
            .unwrap_or_else(|| panic!("{} not checked", d["id"]));
        assert_eq!(r["ok"], true, "{r}");
    }

    // A driver without set_output fails the check, by id and by path.
    b.write(
        "models/acme/broken/model.toml",
        &MODEL
            .replace("acme/psu-3000", "acme/broken")
            .replace("../shared/sim.lua", "driver.lua"),
    );
    b.write(
        "models/acme/broken/driver.lua",
        &SIM_DRIVER.replace("function set_output", "function not_set_output"),
    );
    let out = b.run(&["--json", "model", "check", "acme/broken"]);
    assert_eq!(out.code, 3, "{}", out.stdout);
    assert!(
        out.message().contains("`set_output(ctx"),
        "{}",
        out.message()
    );
    let path = b.dir.path().join("models/acme/broken/model.toml");
    let out = b.run(&["--json", "model", "check", path.to_str().unwrap()]);
    assert_eq!(out.code, 3);
}

#[test]
fn desk_devices_adds_a_library_path() {
    let b = Bench::new("[devices.x]\nmodel = \"other/thing\"\n");
    let lib = TempDir::new().unwrap();
    std::fs::create_dir_all(lib.path().join("other/thing")).unwrap();
    std::fs::write(
        lib.path().join("other/thing/model.toml"),
        MODEL
            .replace("acme/psu-3000", "other/thing")
            .replace("../shared/sim.lua", "sim.lua"),
    )
    .unwrap();
    std::fs::write(lib.path().join("other/thing/sim.lua"), SIM_DRIVER).unwrap();

    assert_eq!(
        b.run(&["--json", "device", "list"]).code,
        3,
        "not found without DESK_MODELS"
    );
    let out = b.run_env(&["--json", "get", "x"], &[("DESK_MODELS", lib.path())]);
    assert_eq!(out.code, 0, "{}", out.stdout);
}

#[test]
fn bundled_models_work_with_your_own_port() {
    // A bundled driver loads from the binary; the device adds its own port, the rest of the
    // model's port stays.
    let probe = Bench::new("");
    let (id, def) = bundled_source(&probe);
    let defined = &def["model"]["bindings"]["source"];
    let b = Bench::new(&format!(
        "[devices.psu]\nmodel = \"{id}\"\nsource = {{ port = {{ path = \"/dev/desk-test-missing\" }} }}\n"
    ));
    let d = b.run(&["--json", "device", "show", "psu"]).json();
    let source = &d["data"]["device"]["source"];
    assert_eq!(source["script"], defined["script"]);
    assert!(
        source["script"].as_str().unwrap().starts_with("bundled:"),
        "{source}"
    );
    if let Some(port) = defined["port"].as_object() {
        for (k, v) in port {
            assert_eq!(&source["port"][k], v, "`{k}` kept from the model");
        }
    }
    assert_eq!(
        source["port"]["path"], "/dev/desk-test-missing",
        "added by the device"
    );
}

#[test]
fn models_show() {
    let b = Bench::new("");
    let out = b.run(&["model", "show", "acme/psu-3000"]);
    assert_eq!(out.code, 0);
    assert!(out.stdout.contains("[source.ratings]"), "{}", out.stdout);
    assert!(out.stdout.contains("model = \"acme/psu-3000\""));
    let out = b.run(&["--json", "model", "show", "acme/psu-3000"]);
    assert_eq!(out.json()["data"]["model"]["status"], "community");
}

/// A local model `acme/<model>` with the given [model] fields and [source] body.
fn write_model(b: &Bench, model: &str, device_extra: &str, source: &str) {
    b.write(
        &format!("models/acme/{model}/model.toml"),
        &format!(
            "[model]\nid = \"acme/{model}\"\nname = \"X\"\nrequires = \"desk-operator >= 0.1\"\n{device_extra}\n[source]\nscript = \"../shared/sim.lua\"\n{source}\n"
        ),
    );
}

fn list_error(b: &Bench, id: &str) -> String {
    let j = b.run(&["--json", "model", "list"]).json();
    j["data"]["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == id)
        .unwrap_or_else(|| panic!("{id} not listed"))["error"]["message"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

#[test]
fn format_rules_for_models() {
    let b = Bench::new("");
    let ratings = "ratings = { max_voltage = 30.0, max_current = 5.0 }";
    write_model(&b, "no-ratings", "status = \"community\"", "");
    write_model(
        &b,
        "half-ratings",
        "status = \"community\"",
        "ratings = { max_voltage = 30.0 }",
    );
    write_model(
        &b,
        "bad-rating",
        "status = \"community\"",
        "ratings = { max_voltage = 0.0, max_current = 5.0 }",
    );
    write_model(
        &b,
        "odd-rating",
        "status = \"community\"",
        "ratings = { max_volts = 3.0 }",
    );
    write_model(&b, "bad-status", "status = \"works for me\"", ratings);
    write_model(&b, "untested", "status = \"tested\"", ratings);
    write_model(&b, "no-requires", "status = \"community\"", ratings);
    b.write(
        "models/acme/no-requires/model.toml",
        &std::fs::read_to_string(b.dir.path().join("models/acme/no-requires/model.toml"))
            .unwrap()
            .replace("requires = \"desk-operator >= 0.1\"\n", ""),
    );
    write_model(
        &b,
        "with-kind",
        "status = \"community\"\nkind = \"psu\"",
        ratings,
    );

    assert!(
        list_error(&b, "acme/no-ratings")
            .contains("needs `ratings = { max_voltage = …, max_current = … }`")
    );
    assert!(list_error(&b, "acme/half-ratings").contains("max_current = …"));
    assert!(list_error(&b, "acme/bad-rating").contains("must be > 0"));
    assert!(list_error(&b, "acme/odd-rating").contains("max_volts"));
    assert!(list_error(&b, "acme/bad-status").contains("status"));
    assert!(list_error(&b, "acme/untested").contains("needs `tested ="));
    assert!(list_error(&b, "acme/no-requires").contains("requires"));
    assert!(list_error(&b, "acme/with-kind").contains("kind"));
    assert_eq!(
        list_error(&b, "acme/psu-3000"),
        "",
        "the valid one stays valid"
    );
}

#[test]
fn models_drivers_must_declare_params() {
    let b = Bench::new("");
    b.write(
        "models/acme/shared/undeclared.lua",
        &SIM_DRIVER.replace("PARAMS = ", "NOT_PARAMS = "),
    );
    write_model(
        &b,
        "undeclared",
        "status = \"community\"",
        "ratings = { max_voltage = 30.0, max_current = 5.0 }",
    );
    b.write(
        "models/acme/undeclared/model.toml",
        &std::fs::read_to_string(b.dir.path().join("models/acme/undeclared/model.toml"))
            .unwrap()
            .replace("../shared/sim.lua", "../shared/undeclared.lua"),
    );
    let out = b.run(&["--json", "model", "check", "acme/undeclared"]);
    assert_eq!(out.code, 3, "{}", out.stdout);
    assert!(
        out.message().contains("must declare `PARAMS"),
        "{}",
        out.message()
    );
}

#[test]
fn params_are_validated_against_the_driver() {
    let b = Bench::new("");
    for (model, params, why) in [
        (
            "typo",
            "params = { flavr = \"x\" }",
            "unknown param `flavr` (the driver accepts: flavor)",
        ),
        (
            "wrong-type",
            "params = { flavor = 3 }",
            "`flavor` must be a string",
        ),
    ] {
        write_model(
            &b,
            model,
            "status = \"community\"",
            &format!("ratings = {{ max_voltage = 30.0, max_current = 5.0 }}\n{params}"),
        );
        let out = b.run(&["--json", "model", "check", &format!("acme/{model}")]);
        assert_eq!(out.code, 3, "{model}: {}", out.stdout);
        assert!(out.message().contains(why), "{model}: {}", out.message());
    }

    // Required params, and defaults reaching the driver.
    b.write(
        "models/acme/shared/strict.lua",
        &SIM_DRIVER.replace(
            "PARAMS = { flavor",
            "PARAMS = { addr = { type = \"integer\", required = true }, flavor",
        ),
    );
    write_model(
        &b,
        "strict",
        "status = \"community\"",
        "ratings = { max_voltage = 30.0, max_current = 5.0 }",
    );
    b.write(
        "models/acme/strict/model.toml",
        &std::fs::read_to_string(b.dir.path().join("models/acme/strict/model.toml"))
            .unwrap()
            .replace("../shared/sim.lua", "../shared/strict.lua"),
    );
    let out = b.run(&["--json", "model", "check", "acme/strict"]);
    assert_eq!(out.code, 3);
    assert!(
        out.message().contains("missing required param `addr`"),
        "{}",
        out.message()
    );

    let b = Bench::new("[devices.psu]\nmodel = \"acme/psu-3000\"\n");
    let out = b.run(&["--json", "get", "psu"]);
    assert_eq!(
        out.json()["data"]["log"][0],
        "driver model",
        "default applied"
    );
}

#[test]
fn show_lists_ratings_sources_and_driver_params() {
    let b = Bench::new("");
    let out = b.run(&["model", "show", "acme/psu-3000"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert!(
        out.stdout
            .contains("ratings = { max_voltage = 30.0, max_current = 5.0 }")
            || out.stdout.contains("[source.ratings]"),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout
            .contains("https://example.com/acme-psu-3000-manual.pdf")
    );
    assert!(out.stdout.contains("source driver params:"));
    assert!(out.stdout.contains("flavor"), "{}", out.stdout);

    let j = b.run(&["--json", "model", "show", "acme/psu-3000"]).json();
    let params = j["data"]["driver_params"]["source"].as_array().unwrap();
    let names: Vec<&str> = params.iter().map(|p| p["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"flavor"), "{names:?}");
}

/// The JSON Schema for editors must agree with desk: accept every real model, reject what
/// desk rejects (as far as a schema can express it).
#[test]
fn json_schema_agrees_with_the_models() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let schema: Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("schemas/model.schema.json")).unwrap(),
    )
    .unwrap();
    let validator = jsonschema::validator_for(&schema).expect("valid schema");
    let as_json = |toml_text: &str| -> Value {
        let t: toml::Table = toml::from_str(toml_text).unwrap();
        serde_json::to_value(t).unwrap()
    };

    let mut checked = 0;
    for vendor in std::fs::read_dir(root.join("models")).unwrap().flatten() {
        for model in std::fs::read_dir(vendor.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            let file = model.path().join("model.toml");
            if file.is_file() {
                let v = as_json(&std::fs::read_to_string(&file).unwrap());
                let errors: Vec<String> =
                    validator.iter_errors(&v).map(|e| e.to_string()).collect();
                assert!(errors.is_empty(), "{}: {errors:?}", file.display());
                checked += 1;
            }
        }
    }
    assert!(checked >= 1, "no models in models/");
    let declared = format!(
        "{MODEL}\n[heater]\nscript = \"h.lua\"\nchannels = 2\n[heater.values]\nt = {{ type = \"number\", unit = \"°C\" }}\nr = {{ type = \"number\", units = [\"V\", \"Ω\"] }}\n[heater.settings]\nsp = {{ type = \"number\", unit = \"°C\", min = 50, max = 450 }}\n[heater.actions]\ngo = {{ switches = true, args = {{ s = {{ type = \"number\", unit = \"s\" }} }} }}\n"
    );
    let errors: Vec<String> = validator
        .iter_errors(&as_json(&declared))
        .map(|e| e.to_string())
        .collect();
    assert!(errors.is_empty(), "a declared capability: {errors:?}");
    let capture = format!(
        "{MODEL}\n[capture]\nscript = \"c.lua\"\n[capture.actions.capture]\nswitches = false\nproduces = \"file\"\nargs = {{ duration = {{ type = \"number\", unit = \"s\" }}, rate = {{ type = \"number\", unit = \"Hz\", default = 1e6 }}, trigger = {{ type = \"string\", optional = true }} }}\n"
    );
    let errors: Vec<String> = validator
        .iter_errors(&as_json(&capture))
        .map(|e| e.to_string())
        .collect();
    assert!(
        errors.is_empty(),
        "an action that produces a file: {errors:?}"
    );
    let sigrok = capture.replace(
        "script = \"c.lua\"\n",
        "script = \"c.lua\"\nsigrok = { driver = \"demo\", build = \"https://example.org/builds\" }\n",
    );
    let errors: Vec<String> = validator
        .iter_errors(&as_json(&sigrok))
        .map(|e| e.to_string())
        .collect();
    assert!(errors.is_empty(), "a sigrok binding: {errors:?}");
    for (bad, why) in [
        (
            "sigrok = { driver = \"demo\", conn = \"1.2\" }",
            "conn in a model",
        ),
        (
            "sigrok = { driver = \"demo\", backend = \"v\" }",
            "backend in a model",
        ),
        ("sigrok = { build = \"https://example.org\" }", "no driver"),
        ("sigrok = { driver = \"Demo\" }", "driver id"),
    ] {
        let broken = capture.replace(
            "script = \"c.lua\"\n",
            &format!("script = \"c.lua\"\n{bad}\n"),
        );
        assert!(
            !validator.is_valid(&as_json(&broken)),
            "schema should reject: {why}"
        );
    }

    for (broken, why) in [
        (
            MODEL.replace("status   = \"community\"", "status = \"works\""),
            "status",
        ),
        (
            MODEL.replace("ratings = { max_voltage = 30.0, max_current = 5.0 }", ""),
            "ratings",
        ),
        (
            MODEL.replace("script  = \"../shared/sim.lua\"", "provider = \"ha\""),
            "provider",
        ),
        (MODEL.replace("[model]", "[model]\nkind = \"psu\""), "kind"),
        (
            format!(
                "{MODEL}\n[heater]\nscript = \"h.lua\"\nvalues = {{ t = {{ type = \"float\" }} }}\n"
            ),
            "declaration type",
        ),
        (
            format!(
                "{MODEL}\n[heater]\nscript = \"h.lua\"\nactions = {{ go = {{ doc = \"x\" }} }}\n"
            ),
            "action without switches",
        ),
        (
            format!(
                "{MODEL}\n[heater]\nscript = \"h.lua\"\nactions = {{ go = {{ switches = false, produces = \"stream\" }} }}\n"
            ),
            "produces something other than a file",
        ),
        (
            format!(
                "{MODEL}\n[heater]\nscript = \"h.lua\"\nactions = {{ go = {{ switches = false, args = {{ a = {{ type = \"number\", default = 1, optional = true }} }} }} }}\n"
            ),
            "default and optional",
        ),
    ] {
        assert!(
            !validator.is_valid(&as_json(&broken)),
            "schema should reject: {why}"
        );
    }
}

#[test]
fn scaffold_creates_a_checkable_model() {
    let b = Bench::new("");
    b.write("models/acme/README.md", "# ACME, written by hand\n");
    let out = b.run(&[
        "--json",
        "model",
        "new",
        "acme/psu-9000",
        "--capability",
        "voltage_source",
        "--name",
        "ACME PSU 9000",
        "--max-voltage",
        "30",
        "--max-current",
        "5",
    ]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let d = &out.json()["data"];
    assert_eq!(d["check"]["ok"], true, "{d}");
    let created: Vec<String> = d["created"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap().replace('\\', "/"))
        .collect();
    assert!(
        created
            .iter()
            .any(|p| p.ends_with("acme/psu-9000/driver.lua"))
    );
    assert!(
        created
            .iter()
            .any(|p| p.ends_with("acme/psu-9000/model.toml"))
    );
    assert!(
        !created.iter().any(|p| p.ends_with("README.md")),
        "existing README kept"
    );
    let readme = std::fs::read_to_string(b.dir.path().join("models/acme/README.md")).unwrap();
    assert_eq!(readme, "# ACME, written by hand\n");

    // It's on the search path right away; the skeleton fails loudly until implemented.
    let def = b.run(&["--json", "model", "show", "acme/psu-9000"]).json();
    assert_eq!(def["data"]["model"]["name"], "ACME PSU 9000");
    b.write("desk.toml", "[devices.x]\nmodel = \"acme/psu-9000\"\n");
    let out = b.run(&["--json", "set", "x", "voltage", "5v"]);
    assert_eq!(out.code, 5);
    assert!(
        out.message().contains("not implemented: set_voltage"),
        "{}",
        out.message()
    );

    // Without maximums the check names them; existing models are never overwritten.
    let out = b.run(&[
        "--json",
        "model",
        "new",
        "acme/psu-9001",
        "--capability",
        "voltage_source",
    ]);
    assert_eq!(out.json()["data"]["check"]["ok"], false);
    assert!(
        out.stdout.contains("ratings.max_voltage must be > 0"),
        "{}",
        out.stdout
    );
    let out = b.run(&[
        "--json",
        "model",
        "new",
        "acme/psu-9000",
        "--capability",
        "voltage_source",
    ]);
    assert_eq!(out.code, 2);
    assert!(out.message().contains("already exists"));

    // A power_switch model reusing an existing driver gets no new driver file.
    b.write(
        "models/acme/shared/relay.lua",
        "PARAMS = {}\nfunction on(ctx) end\nfunction off(ctx) end\n",
    );
    let out = b.run(&[
        "--json",
        "model",
        "new",
        "acme/relay",
        "--capability",
        "power_switch",
        "--driver",
        "../shared/relay.lua",
    ]);
    assert_eq!(out.json()["data"]["check"]["ok"], true, "{}", out.stdout);
    assert_eq!(out.json()["data"]["created"].as_array().unwrap().len(), 1);
}
