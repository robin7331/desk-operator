//! The device library: `model = "…"`, merging, search path, `desk library list|show|check`.

use std::path::Path;
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

/// A PSU simulated inside the driver (state lives for one command). Its maximum comes from params.
const SIM_DRIVER: &str = r#"
local s = { v = 0, a = 0, out = false }
function info(ctx) return { max_voltage = ctx.params.max, max_current = 5 } end
function set_voltage(ctx, v) s.v = v end
function set_current(ctx, a) s.a = a end
function set_output(ctx, on) s.out = on end
function read(ctx)
  ctx.log("driver " .. (ctx.params.flavor or "definition"))
  return { output = s.out, voltage_set = s.v, current_set = s.a, voltage = s.out and s.v or 0 }
end
"#;

const DEFINITION: &str = r#"
[device]
id       = "acme/psu-3000"
name     = "ACME PSU 3000"
kind     = "bench power supply"
requires = "desk-operator >= 0.1"
status   = "community"

[source]
script = "../shared/sim.lua"
params = { max = 30.0 }
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
    let b = Bench::new("[devices.psu]\nmodel = \"acme/psu-3000\"\nlimits = { voltage = 12 }\n");
    let d = b.run(&["--json", "device", "show", "psu"]).json();
    let dev = &d["data"]["device"];
    assert_eq!(dev["model"], "acme/psu-3000");
    assert_eq!(
        dev["name"], "ACME PSU 3000",
        "name comes from the definition"
    );
    assert_eq!(dev["capabilities"][0], "voltage_source");

    let out = b.run(&["--json", "source", "set", "psu", "5v", "--on"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.json()["data"]["verified"]["voltage"], true);
    // The device's own bench limits still apply.
    assert_eq!(b.run(&["--json", "source", "set", "psu", "13v"]).code, 8);
}

#[test]
fn own_values_merge_over_the_definition() {
    let b = Bench::new(
        "[devices.psu]\nmodel = \"acme/psu-3000\"\nname = \"Mine\"\nsource = { params = { max = 12.0 } }\n",
    );
    let out = b.run(&["--json", "source", "set", "psu", "13v"]);
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
        "[devices.psu]\nmodel = \"acme/psu-3000\"\nsource = { script = \"mine.lua\", params = { max = 30.0, flavor = \"mine\" } }\n",
    );
    b.write("mine.lua", SIM_DRIVER);
    let out = b.run(&["--json", "source", "read", "psu"]);
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
    let out = b.run(&["--json", "source", "read", "psu"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
}

#[test]
fn definitions_may_not_contain_bench_specific_bindings() {
    let b = Bench::new("[devices.x]\nmodel = \"acme/relay\"\n");
    b.write(
        "devices/acme/relay/device.toml",
        "[device]\nid = \"acme/relay\"\nname = \"Relay\"\n[power]\nprovider = \"ha\"\nentity = \"switch.x\"\n",
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
    assert!(error_of("acme/typo").contains("unknown section `sauce`"));
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
    let out = b.run_env(
        &["--json", "source", "read", "x"],
        &[("DESK_DEVICES", lib.path())],
    );
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
    let out = b.run(&["--json", "source", "read", "psu"]);
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
    assert!(out.stdout.contains("[source.params]"), "{}", out.stdout);
    assert!(out.stdout.contains("model = \"acme/psu-3000\""));
    let out = b.run(&["--json", "library", "show", "fnirsi/ips3608"]);
    assert_eq!(
        out.json()["data"]["definition"]["status"],
        "tested on hardware"
    );
}
