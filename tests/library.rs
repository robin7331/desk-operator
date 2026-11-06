//! The device library: `model = "…"`, merging, search path, `desk library list|show|check`.

use std::path::Path;
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

/// A PSU simulated inside the driver (state lives for one command).
const SIM_DRIVER: &str = r#"
PARAMS = { flavor = { type = "string", default = "definition", doc = "which copy of the driver" } }
local s = { v = 0, a = 0, out = false }
function set_voltage(ctx, v) s.v = v end
function set_current(ctx, a) s.a = a end
function set_output(ctx, on) s.out = on end
function read(ctx)
  ctx.log("driver " .. ctx.params.flavor)
  return { output = s.out, voltage_set = s.v, current_set = s.a, voltage = s.out and s.v or 0 }
end
"#;

const DEFINITION: &str = r#"
[device]
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
    /// A config with `devices_toml` and the local definition `acme/psu-3000`.
    fn new(devices_toml: &str) -> Self {
        let dir = TempDir::new().unwrap();
        let b = Bench { dir };
        b.write("devices/acme/shared/sim.lua", SIM_DRIVER);
        b.write("devices/acme/psu-3000/device.toml", DEFINITION);
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
            .env_remove("DESK_DEVICES")
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
    assert_eq!(
        dev["name"], "ACME PSU 3000",
        "name comes from the definition"
    );
    assert_eq!(dev["capabilities"][0], "voltage_source");

    let out = b.run(&["--json", "set", "psu", "voltage", "5v", "output", "on"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.json()["data"]["verified"]["voltage"], true);
    // The device's own bench limits still apply.
    assert_eq!(b.run(&["--json", "set", "psu", "voltage", "13v"]).code, 8);
}

#[test]
fn own_values_merge_over_the_definition() {
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
            .ends_with("acme/shared/sim.lua"),
        "{source}"
    );
}

#[test]
fn own_script_replaces_the_definition() {
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
        out.message().contains("no device definition `acme/nope`"),
        "{}",
        out.message()
    );
    assert!(out.message().contains("desk library list"));

    let b = Bench::new("[devices.psu]\nmodel = \"NotAnId\"\n");
    assert!(
        b.run(&["--json", "device", "list"])
            .message()
            .contains("not a definition id")
    );
}

#[test]
fn local_definitions_shadow_bundled_ones() {
    let b = Bench::new("[devices.psu]\nmodel = \"fnirsi/ips3608\"\n");
    b.write(
        "devices/fnirsi/ips3608/device.toml",
        &DEFINITION
            .replace("acme/psu-3000", "fnirsi/ips3608")
            .replace("../shared/sim.lua", "../../acme/shared/sim.lua"),
    );
    let j = b.run(&["--json", "library", "list"]).json();
    let ips: Vec<&Value> = j["data"]["definitions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["id"] == "fnirsi/ips3608")
        .collect();
    assert_eq!(ips.len(), 2, "{ips:?}");
    assert!(ips[0]["origin"].as_str().unwrap().starts_with("local"));
    assert_eq!(ips[0]["shadowed"], false);
    assert_eq!(ips[1]["origin"], "bundled");
    assert_eq!(ips[1]["shadowed"], true);

    // The device uses the local one (a simulated PSU: no hardware needed).
    let out = b.run(&["--json", "get", "psu"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
}

#[test]
fn definitions_may_not_contain_bench_specific_bindings() {
    let b = Bench::new("[devices.x]\nmodel = \"acme/relay\"\n");
    b.write(
        "devices/acme/relay/device.toml",
        "[device]\nid = \"acme/relay\"\nname = \"Relay\"\nrequires = \"desk-operator >= 0.1\"\nstatus = \"community\"\n[power]\nprovider = \"ha\"\nentity = \"switch.x\"\n",
    );
    let out = b.run(&["--json", "device", "list"]);
    assert_eq!(out.code, 3);
    assert!(
        out.message().contains("definitions may only use scripts"),
        "{}",
        out.message()
    );
}

#[test]
fn malformed_definitions() {
    let b = Bench::new("");
    b.write(
        "devices/acme/wrong-id/device.toml",
        &DEFINITION.replace("acme/psu-3000", "acme/other"),
    );
    b.write(
        "devices/acme/too-new/device.toml",
        &DEFINITION
            .replace("acme/psu-3000", "acme/too-new")
            .replace(">= 0.1", ">= 99.0"),
    );
    b.write(
        "devices/acme/typo/device.toml",
        &DEFINITION
            .replace("acme/psu-3000", "acme/typo")
            .replace("[source]", "[sauce]"),
    );
    let j = b.run(&["--json", "library", "list"]).json();
    let error_of = |id: &str| {
        j["data"]["definitions"]
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
    let out = b.run(&["--json", "library", "check"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let results = out.json()["data"]["results"].as_array().unwrap().clone();
    for id in ["acme/psu-3000", "fnirsi/ips3608", "fnirsi/dps-150"] {
        let r = results
            .iter()
            .find(|r| r["target"] == id)
            .unwrap_or_else(|| panic!("{id}"));
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["capabilities"][0], "voltage_source");
    }

    // A driver without set_output fails the check, by id and by path.
    b.write(
        "devices/acme/broken/device.toml",
        &DEFINITION
            .replace("acme/psu-3000", "acme/broken")
            .replace("../shared/sim.lua", "driver.lua"),
    );
    b.write(
        "devices/acme/broken/driver.lua",
        &SIM_DRIVER.replace("function set_output", "function not_set_output"),
    );
    let out = b.run(&["--json", "library", "check", "acme/broken"]);
    assert_eq!(out.code, 3, "{}", out.stdout);
    assert!(
        out.message().contains("`set_output(ctx"),
        "{}",
        out.message()
    );
    let path = b.dir.path().join("devices/acme/broken/device.toml");
    let out = b.run(&["--json", "library", "check", path.to_str().unwrap()]);
    assert_eq!(out.code, 3);
}

#[test]
fn desk_devices_adds_a_library_path() {
    let b = Bench::new("[devices.x]\nmodel = \"other/thing\"\n");
    let lib = TempDir::new().unwrap();
    std::fs::create_dir_all(lib.path().join("other/thing")).unwrap();
    std::fs::write(
        lib.path().join("other/thing/device.toml"),
        DEFINITION
            .replace("acme/psu-3000", "other/thing")
            .replace("../shared/sim.lua", "sim.lua"),
    )
    .unwrap();
    std::fs::write(lib.path().join("other/thing/sim.lua"), SIM_DRIVER).unwrap();

    assert_eq!(
        b.run(&["--json", "device", "list"]).code,
        3,
        "not found without DESK_DEVICES"
    );
    let out = b.run_env(&["--json", "get", "x"], &[("DESK_DEVICES", lib.path())]);
    assert_eq!(out.code, 0, "{}", out.stdout);
}

#[test]
fn bundled_definitions_work_with_your_own_port() {
    // The bundled FNIRSI driver loads from the binary; the device adds its own port.
    let b = Bench::new(
        "[devices.psu]\nmodel = \"fnirsi/ips3608\"\nsource = { port = { path = \"/dev/desk-test-missing\" }, params = { wait = \"0s\", boot_ms = 0 } }\n",
    );
    let d = b.run(&["--json", "device", "show", "psu"]).json();
    let source = &d["data"]["device"]["source"];
    assert_eq!(
        source["script"],
        "bundled:fnirsi/shared/dps150_protocol.lua"
    );
    assert_eq!(
        source["port"]["usb_vid"], 0x2e3c,
        "kept from the definition"
    );
    assert_eq!(
        source["port"]["path"], "/dev/desk-test-missing",
        "added by the device"
    );
    let out = b.run(&["--json", "get", "psu"]);
    assert_eq!(
        out.code, 4,
        "a missing port keeps its own error code: {}",
        out.stdout
    );
    assert_eq!(out.json()["error"]["code"], "endpoint_unavailable");
}

#[test]
fn library_show() {
    let b = Bench::new("");
    let out = b.run(&["library", "show", "acme/psu-3000"]);
    assert_eq!(out.code, 0);
    assert!(out.stdout.contains("[source.ratings]"), "{}", out.stdout);
    assert!(out.stdout.contains("model = \"acme/psu-3000\""));
    let out = b.run(&["--json", "library", "show", "fnirsi/ips3608"]);
    assert_eq!(out.json()["data"]["definition"]["status"], "tested");
}

/// A local definition `acme/<model>` with the given [device] fields and [source] body.
fn definition(b: &Bench, model: &str, device_extra: &str, source: &str) {
    b.write(
        &format!("devices/acme/{model}/device.toml"),
        &format!(
            "[device]\nid = \"acme/{model}\"\nname = \"X\"\nrequires = \"desk-operator >= 0.1\"\n{device_extra}\n[source]\nscript = \"../shared/sim.lua\"\n{source}\n"
        ),
    );
}

fn list_error(b: &Bench, id: &str) -> String {
    let j = b.run(&["--json", "library", "list"]).json();
    j["data"]["definitions"]
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
fn format_rules_for_definitions() {
    let b = Bench::new("");
    let ratings = "ratings = { max_voltage = 30.0, max_current = 5.0 }";
    definition(&b, "no-ratings", "status = \"community\"", "");
    definition(
        &b,
        "half-ratings",
        "status = \"community\"",
        "ratings = { max_voltage = 30.0 }",
    );
    definition(
        &b,
        "bad-rating",
        "status = \"community\"",
        "ratings = { max_voltage = 0.0, max_current = 5.0 }",
    );
    definition(
        &b,
        "odd-rating",
        "status = \"community\"",
        "ratings = { max_volts = 3.0 }",
    );
    definition(&b, "bad-status", "status = \"works for me\"", ratings);
    definition(&b, "untested", "status = \"tested\"", ratings);
    definition(&b, "no-requires", "status = \"community\"", ratings);
    b.write(
        "devices/acme/no-requires/device.toml",
        &std::fs::read_to_string(b.dir.path().join("devices/acme/no-requires/device.toml"))
            .unwrap()
            .replace("requires = \"desk-operator >= 0.1\"\n", ""),
    );
    definition(
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
fn library_drivers_must_declare_params() {
    let b = Bench::new("");
    b.write(
        "devices/acme/shared/undeclared.lua",
        &SIM_DRIVER.replace("PARAMS = ", "NOT_PARAMS = "),
    );
    definition(
        &b,
        "undeclared",
        "status = \"community\"",
        "ratings = { max_voltage = 30.0, max_current = 5.0 }",
    );
    b.write(
        "devices/acme/undeclared/device.toml",
        &std::fs::read_to_string(b.dir.path().join("devices/acme/undeclared/device.toml"))
            .unwrap()
            .replace("../shared/sim.lua", "../shared/undeclared.lua"),
    );
    let out = b.run(&["--json", "library", "check", "acme/undeclared"]);
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
        definition(
            &b,
            model,
            "status = \"community\"",
            &format!("ratings = {{ max_voltage = 30.0, max_current = 5.0 }}\n{params}"),
        );
        let out = b.run(&["--json", "library", "check", &format!("acme/{model}")]);
        assert_eq!(out.code, 3, "{model}: {}", out.stdout);
        assert!(out.message().contains(why), "{model}: {}", out.message());
    }

    // Required params, and defaults reaching the driver.
    b.write(
        "devices/acme/shared/strict.lua",
        &SIM_DRIVER.replace(
            "PARAMS = { flavor",
            "PARAMS = { addr = { type = \"integer\", required = true }, flavor",
        ),
    );
    definition(
        &b,
        "strict",
        "status = \"community\"",
        "ratings = { max_voltage = 30.0, max_current = 5.0 }",
    );
    b.write(
        "devices/acme/strict/device.toml",
        &std::fs::read_to_string(b.dir.path().join("devices/acme/strict/device.toml"))
            .unwrap()
            .replace("../shared/sim.lua", "../shared/strict.lua"),
    );
    let out = b.run(&["--json", "library", "check", "acme/strict"]);
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
        "driver definition",
        "default applied"
    );
}

#[test]
fn show_lists_ratings_sources_and_driver_params() {
    let b = Bench::new("");
    let out = b.run(&["library", "show", "acme/psu-3000"]);
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

    let j = b
        .run(&["--json", "library", "show", "fnirsi/ips3608"])
        .json();
    let params = j["data"]["driver_params"]["source"].as_array().unwrap();
    let names: Vec<&str> = params.iter().map(|p| p["name"].as_str().unwrap()).collect();
    for n in ["name", "status", "settle_ms", "boot_ms"] {
        assert!(names.contains(&n), "{n} in {names:?}");
    }
    assert_eq!(j["data"]["definition"]["status"], "tested");
}

/// The JSON Schema for editors must agree with desk: accept every real definition, reject what
/// desk rejects (as far as a schema can express it).
#[test]
fn json_schema_agrees_with_the_definitions() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let schema: Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("schemas/device-definition.schema.json")).unwrap(),
    )
    .unwrap();
    let validator = jsonschema::validator_for(&schema).expect("valid schema");
    let as_json = |toml_text: &str| -> Value {
        let t: toml::Table = toml::from_str(toml_text).unwrap();
        serde_json::to_value(t).unwrap()
    };

    let mut checked = 0;
    for vendor in std::fs::read_dir(root.join("devices")).unwrap().flatten() {
        for model in std::fs::read_dir(vendor.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            let file = model.path().join("device.toml");
            if file.is_file() {
                let v = as_json(&std::fs::read_to_string(&file).unwrap());
                let errors: Vec<String> =
                    validator.iter_errors(&v).map(|e| e.to_string()).collect();
                assert!(errors.is_empty(), "{}: {errors:?}", file.display());
                checked += 1;
            }
        }
    }
    assert!(checked >= 2);
    let declared = format!(
        "{DEFINITION}\n[heater]\nscript = \"h.lua\"\nchannels = 2\n[heater.values]\nt = {{ type = \"number\", unit = \"°C\" }}\nr = {{ type = \"number\", units = [\"V\", \"Ω\"] }}\n[heater.settings]\nsp = {{ type = \"number\", unit = \"°C\", min = 50, max = 450 }}\n[heater.actions]\ngo = {{ switches = true, args = {{ s = {{ type = \"number\", unit = \"s\" }} }} }}\n"
    );
    let errors: Vec<String> = validator
        .iter_errors(&as_json(&declared))
        .map(|e| e.to_string())
        .collect();
    assert!(errors.is_empty(), "a declared capability: {errors:?}");

    for (broken, why) in [
        (
            DEFINITION.replace("status   = \"community\"", "status = \"works\""),
            "status",
        ),
        (
            DEFINITION.replace("ratings = { max_voltage = 30.0, max_current = 5.0 }", ""),
            "ratings",
        ),
        (
            DEFINITION.replace("script  = \"../shared/sim.lua\"", "provider = \"ha\""),
            "provider",
        ),
        (
            DEFINITION.replace("[device]", "[device]\nkind = \"psu\""),
            "kind",
        ),
        (
            format!(
                "{DEFINITION}\n[heater]\nscript = \"h.lua\"\nvalues = {{ t = {{ type = \"float\" }} }}\n"
            ),
            "declaration type",
        ),
        (
            format!(
                "{DEFINITION}\n[heater]\nscript = \"h.lua\"\nactions = {{ go = {{ doc = \"x\" }} }}\n"
            ),
            "action without switches",
        ),
    ] {
        assert!(
            !validator.is_valid(&as_json(&broken)),
            "schema should reject: {why}"
        );
    }
}

#[test]
fn scaffold_creates_a_checkable_definition() {
    let b = Bench::new("");
    b.write("devices/acme/README.md", "# ACME, written by hand\n");
    let out = b.run(&[
        "--json",
        "library",
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
    let created: Vec<&str> = d["created"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    assert!(
        created
            .iter()
            .any(|p| p.ends_with("acme/psu-9000/driver.lua"))
    );
    assert!(
        created
            .iter()
            .any(|p| p.ends_with("acme/psu-9000/device.toml"))
    );
    assert!(
        !created.iter().any(|p| p.ends_with("README.md")),
        "existing README kept"
    );
    let readme = std::fs::read_to_string(b.dir.path().join("devices/acme/README.md")).unwrap();
    assert_eq!(readme, "# ACME, written by hand\n");

    // It's on the search path right away; the skeleton fails loudly until implemented.
    let def = b
        .run(&["--json", "library", "show", "acme/psu-9000"])
        .json();
    assert_eq!(def["data"]["definition"]["name"], "ACME PSU 9000");
    b.write("desk.toml", "[devices.x]\nmodel = \"acme/psu-9000\"\n");
    let out = b.run(&["--json", "set", "x", "voltage", "5v"]);
    assert_eq!(out.code, 5);
    assert!(
        out.message().contains("not implemented: set_voltage"),
        "{}",
        out.message()
    );

    // Without maximums the check names them; existing definitions are never overwritten.
    let out = b.run(&[
        "--json",
        "library",
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
        "library",
        "new",
        "acme/psu-9000",
        "--capability",
        "voltage_source",
    ]);
    assert_eq!(out.code, 2);
    assert!(out.message().contains("already exists"));

    // A power_switch definition reusing an existing driver gets no new driver file.
    b.write(
        "devices/acme/shared/relay.lua",
        "PARAMS = {}\nfunction on(ctx) end\nfunction off(ctx) end\n",
    );
    let out = b.run(&[
        "--json",
        "library",
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
