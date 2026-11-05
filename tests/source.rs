//! End-to-end tests of the `voltage_source` capability against the simulated PSU (mock provider).

use std::path::Path;
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

const CONFIG: &str = r#"
[providers.sim]
kind = "mock"

[providers.slow]
kind = "mock"
latency = "400ms"

[devices.psu]
aliases = ["supply"]
power   = { provider = "sim", endpoint = "mains" }
source  = { provider = "sim", endpoint = "psu", max_voltage = 30, max_current = 5, load_ohms = 10 }
limits  = { source = { voltage = 15, current = 2 } }

# Same instrument, no bench limits (to put the device into a state the limited one must refuse).
[devices.psu_unlimited]
source = { provider = "sim", endpoint = "psu", max_voltage = 30, max_current = 5, load_ohms = 10 }

[devices.cheap]
source = { provider = "sim", endpoint = "blind_psu" }
limits = { source = { voltage = 12 } }

[devices.broken]
source = { provider = "sim", endpoint = "fail_psu" }

[devices.laggy]
source = { provider = "slow", endpoint = "psu", timeout = "100ms" }

[devices.unknown_mains]
power  = { provider = "sim", endpoint = "blind_mains" }
source = { provider = "sim", endpoint = "psu2" }

[devices.lamp]
power = { provider = "sim", endpoint = "lamp" }

# Trips its over-current protection instead of limiting (10 Ω: trips above V/10 > current setpoint).
[devices.fragile]
source = { provider = "sim", endpoint = "tripping_psu", load_ohms = 10 }
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
    fn data(&self) -> Value {
        self.json()["data"].clone()
    }
    fn code_str(&self) -> String {
        self.json()["error"]["code"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }
}

impl Bench {
    fn new() -> Self {
        Self::with(CONFIG)
    }
    fn with(config: &str) -> Self {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("desk.toml"), config).unwrap();
        let b = Bench { dir };
        b.run(&["power", "on", "psu"]);
        b
    }
    fn run(&self, args: &[&str]) -> Out {
        run(self.dir.path(), args)
    }
}

fn run(dir: &Path, args: &[&str]) -> Out {
    let out = Command::new(env!("CARGO_BIN_EXE_desk"))
        .env_remove("DESK_CONFIG")
        .env("DESK_MOCK_STATE", dir.join("mock-state.json"))
        .arg("--config")
        .arg(dir.join("desk.toml"))
        .args(args)
        .output()
        .unwrap();
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
    }
}

fn close(v: &Value, want: f64) -> bool {
    (v.as_f64().unwrap_or(f64::NAN) - want).abs() < 1e-9
}

#[test]
fn set_and_switch_on_with_verification() {
    let b = Bench::new();
    let out = b.run(&["--json", "source", "set", "psu", "5v", "1a", "--on"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let d = out.data();
    assert_eq!(d["device"], "psu");
    assert_eq!(d["applied"], true);
    assert!(close(&d["requested"]["voltage"], 5.0));
    assert_eq!(d["requested"]["output"], true);
    assert_eq!(d["verified"]["voltage"], true);
    assert_eq!(d["verified"]["current"], true);
    assert_eq!(d["verified"]["output"], true);
    assert_eq!(d["reading"]["output"], true);
    assert!(close(&d["reading"]["voltage"], 5.0));
    assert!(close(&d["reading"]["current"], 0.5), "5 V into 10 Ω");
    assert!(close(&d["info"]["max_voltage"], 30.0));

    let d = b.run(&["--json", "source", "read", "supply"]).data();
    assert!(close(&d["reading"]["voltage_set"], 5.0));
    assert_eq!(d["applied"], false, "a read sends nothing");
    assert!(d["requested"].as_object().unwrap().is_empty());
}

#[test]
fn constant_current_when_the_load_wants_more() {
    let b = Bench::new();
    b.run(&["source", "set", "psu", "12v", "500ma", "--on"]);
    let d = b.run(&["--json", "source", "read", "psu"]).data();
    assert!(close(&d["reading"]["current"], 0.5));
    assert!(close(&d["reading"]["voltage"], 5.0), "CC: 0.5 A × 10 Ω");
}

#[test]
fn limits_refuse_before_anything_is_sent() {
    let b = Bench::new();
    b.run(&["source", "set", "psu", "5v", "1a"]);

    let out = b.run(&["--json", "source", "set", "psu", "16v"]);
    assert_eq!(out.code, 8);
    assert_eq!(out.code_str(), "limit_exceeded");
    assert!(
        out.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("bench limit")
    );

    let out = b.run(&["--json", "source", "set", "psu_unlimited", "31v"]);
    assert_eq!(out.code, 8);
    assert!(
        out.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("device maximum")
    );

    let out = b.run(&["--json", "source", "set", "psu", "3a", "--on"]);
    assert_eq!(out.code, 8);

    let d = b.run(&["--json", "source", "read", "psu"]).data();
    assert!(close(&d["reading"]["voltage_set"], 5.0), "unchanged");
    assert_eq!(d["reading"]["output"], false, "output never switched on");
}

#[test]
fn output_on_refuses_existing_setpoints_above_the_limit() {
    let b = Bench::new();
    // Someone else left the instrument at 24 V.
    let out = b.run(&["--json", "source", "set", "psu_unlimited", "24v"]);
    assert_eq!(out.code, 0, "{}", out.stdout);

    let out = b.run(&["--json", "source", "output", "on", "psu"]);
    assert_eq!(out.code, 8, "{}", out.stdout);
    assert!(
        out.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("24.00 V")
    );
    let d = b.run(&["--json", "source", "read", "psu"]).data();
    assert_eq!(d["reading"]["output"], false);

    // Setting a safe voltage in the same command is fine.
    let out = b.run(&["--json", "source", "set", "psu", "12v", "--on"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
}

#[test]
fn devices_that_cant_report_setpoints_are_unverified() {
    let b = Bench::new();
    let out = b.run(&["--json", "source", "set", "cheap", "5v", "--on"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let d = out.data();
    assert!(
        d["verified"].get("voltage").is_none(),
        "not confirmed: {}",
        d["verified"]
    );
    assert!(d["verified"].get("output").is_none());
    assert!(d["reading"]["voltage_set"].is_null());
    assert!(
        close(&d["reading"]["voltage"], 5.0),
        "measurements still work"
    );
    let warning = d["warnings"][0].as_str().unwrap();
    assert!(
        warning.contains("doesn't report its setpoints"),
        "{warning}"
    );

    let out = b.run(&["source", "set", "cheap", "5v"]);
    assert!(
        out.stdout.contains("voltage NOT confirmed"),
        "{}",
        out.stdout
    );
}

#[test]
fn mains_off_fails_fast_but_unknown_mains_does_not_block() {
    let b = Bench::new();
    b.run(&["power", "off", "psu"]);
    let out = b.run(&["--json", "source", "read", "psu"]);
    assert_eq!(out.code, 4);
    assert_eq!(out.code_str(), "device_off");
    assert!(
        out.json()["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("desk power on")
    );

    let out = b.run(&["--json", "source", "read", "unknown_mains"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
}

#[test]
fn dry_run_sends_nothing() {
    let b = Bench::new();
    let out = b.run(&["--json", "source", "set", "psu", "9v", "--on", "--dry-run"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.data()["applied"], false);
    let d = b.run(&["--json", "source", "read", "psu"]).data();
    assert_eq!(d["reading"]["output"], false);
    assert!(close(&d["reading"]["voltage_set"], 0.0));

    // Limits are still checked in a dry run.
    let out = b.run(&["--json", "source", "set", "psu", "20v", "--dry-run"]);
    assert_eq!(out.code, 8);
}

#[test]
fn usage_and_config_errors() {
    let b = Bench::new();
    for (args, code, err) in [
        (&["--json", "source", "set", "psu", "5"][..], 2, "usage"),
        (
            &["--json", "source", "set", "psu", "5v", "6v"][..],
            2,
            "usage",
        ),
        (&["--json", "source", "set", "psu", "-1v"][..], 2, "usage"),
        (
            &["--json", "source", "read", "lamp"][..],
            3,
            "capability_missing",
        ),
        (
            &["--json", "source", "read", "nope"][..],
            2,
            "device_not_found",
        ),
        (
            &["--json", "source", "read", "broken"][..],
            4,
            "endpoint_unavailable",
        ),
        (&["--json", "source", "read", "laggy"][..], 6, "timeout"),
    ] {
        let out = b.run(args);
        assert_eq!(out.code, code, "{args:?}: {}", out.stdout);
        assert_eq!(out.code_str(), err, "{args:?}");
    }
}

#[test]
fn config_validation() {
    let b = Bench::with(
        r#"
[providers.ha]
kind = "home-assistant"
url = "http://127.0.0.1:9"
token_file = "t"
[devices.a]
limits = { source = { voltage = -1 } }
[devices.b]
source = { provider = "nope", endpoint = "x" }
"#,
    );
    let out = b.run(&["--json", "device", "list"]);
    assert_eq!(out.code, 3);
    let msg = out.json()["error"]["message"].as_str().unwrap().to_string();
    assert!(msg.contains("limits.source.voltage must be >= 0"), "{msg}");
    assert!(msg.contains("source provider `nope`"), "{msg}");

    let b = Bench::with(
        r#"
[providers.ha]
kind = "home-assistant"
url = "http://127.0.0.1:9"
token_file = "t"
[devices.a]
source = { provider = "ha", entity = "switch.x" }
"#,
    );
    std::fs::write(b.dir.path().join("t"), "token").unwrap();
    let out = b.run(&["--json", "source", "read", "a"]);
    assert_eq!(out.code, 3);
    assert!(
        out.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("can't drive a voltage source")
    );
}

#[test]
fn schema_and_device_views_show_the_capability() {
    let b = Bench::new();
    let d = b.run(&["--json", "schema"]).data();
    let psu = d["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == "psu")
        .unwrap()
        .clone();
    let caps: Vec<&str> = psu["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    assert_eq!(caps, ["power_switch", "voltage_source"]);
    assert!(close(&psu["limits"]["source"]["voltage"], 15.0));
    assert!(d["capabilities"]["voltage_source"].is_object());
    let commands: Vec<&str> = d["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["command"].as_str().unwrap())
        .collect();
    for c in ["desk source set", "desk source output", "desk source read"] {
        assert!(commands.contains(&c), "{c}");
    }
    let out = b.run(&["device", "show", "psu"]);
    assert!(
        out.stdout
            .contains("capabilities: power_switch, voltage_source")
    );
    assert!(
        out.stdout.contains("limits:       source 15.00 V, 2.000 A"),
        "{}",
        out.stdout
    );
}

#[test]
fn old_style_limits_explain_the_new_form() {
    let b = Bench::with("[devices.psu]\nlimits = { voltage = 12 }\n");
    let out = b.run(&["--json", "device", "list"]);
    assert_eq!(out.code, 3);
    let msg = out.json()["error"]["message"].as_str().unwrap().to_string();
    assert!(
        msg.contains("limits = { source = { voltage = … } }"),
        "{msg}"
    );
}

#[test]
fn mode_follows_the_load() {
    let b = Bench::new();
    let out = b.run(&["--json", "source", "set", "psu", "5v", "1a", "--on"]);
    assert_eq!(
        out.data()["reading"]["mode"],
        "cv",
        "0.5 A into 10 Ω is below 1 A"
    );
    assert_eq!(out.data()["reading"]["protection"], "none");
    let out = b.run(&["--json", "source", "set", "psu", "200ma"]);
    assert_eq!(
        out.data()["reading"]["mode"],
        "cc",
        "the current limit is reached"
    );
    assert!(close(&out.data()["reading"]["voltage"], 2.0));
    let out = b.run(&["source", "read", "psu"]);
    assert!(
        out.stdout.contains("CC (current limit reached)"),
        "{}",
        out.stdout
    );
    let out = b.run(&["--json", "source", "output", "off", "psu"]);
    assert!(out.data()["reading"]["mode"].is_null(), "no mode while off");
    // Fields a device doesn't report stay null.
    assert!(out.data()["reading"]["temperature"].is_null());
}

#[test]
fn a_tripped_protection_blocks_output_on_until_cleared() {
    let b = Bench::new();
    // 5 V into 10 Ω wants 0.5 A, above the 0.2 A setpoint: the supply trips at switch-on.
    let out = b.run(&["--json", "source", "set", "fragile", "5v", "200ma", "--on"]);
    assert_eq!(out.code, 4, "{}", out.stdout);
    assert_eq!(out.code_str(), "protection_tripped");
    assert!(
        out.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("OCP")
    );
    assert!(
        out.json()["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("--clear-protection")
    );

    // Reading says so, as a warning.
    let out = b.run(&["--json", "source", "read", "fragile"]);
    assert_eq!(out.code, 0);
    assert_eq!(out.data()["reading"]["protection"], "ocp");
    assert_eq!(out.data()["reading"]["output"], false);
    let warnings = out.data()["warnings"].to_string();
    assert!(warnings.contains("--clear-protection"), "{warnings}");

    // Switching on is refused (also as a dry run), and nothing is sent.
    for args in [
        &["--json", "source", "output", "on", "fragile"][..],
        &["--json", "source", "output", "on", "fragile", "--dry-run"][..],
        &["--json", "source", "set", "fragile", "1v", "--on"][..],
    ] {
        let out = b.run(args);
        assert_eq!(
            out.code_str(),
            "protection_tripped",
            "{args:?}: {}",
            out.stdout
        );
    }
    let out = b.run(&["--json", "source", "read", "fragile"]);
    assert!(
        close(&out.data()["reading"]["voltage_set"], 5.0),
        "the refused set sent nothing"
    );

    // Setpoints alone are fine, and switching off is always allowed.
    assert_eq!(b.run(&["--json", "source", "set", "fragile", "1v"]).code, 0);
    assert_eq!(
        b.run(&["--json", "source", "output", "off", "fragile"])
            .code,
        0
    );

    // With the cause fixed (1 V into 10 Ω = 0.1 A), --clear-protection switches on.
    let out = b.run(&[
        "--json",
        "source",
        "output",
        "on",
        "fragile",
        "--clear-protection",
    ]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.data()["requested"]["clear_protection"], true);
    assert_eq!(out.data()["reading"]["protection"], "none");
    assert_eq!(out.data()["verified"]["output"], true);
    assert!(out.data()["warnings"].to_string().contains("was tripped"));
}

#[test]
fn clear_protection_needs_switching_on() {
    let b = Bench::new();
    let out = b.run(&[
        "--json",
        "source",
        "output",
        "off",
        "psu",
        "--clear-protection",
    ]);
    assert_eq!(out.code_str(), "usage");
    let out = b.run(&["--json", "source", "set", "psu", "5v", "--clear-protection"]);
    assert_eq!(
        out.code, 2,
        "--clear-protection requires --on: {}",
        out.stdout
    );
}
