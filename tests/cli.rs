//! End-to-end tests of the `desk` binary against the mock provider. No hardware, no network.

use std::path::{Path, PathBuf};
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
name    = "FNIRSI IPS3608"
aliases = ["supply"]
power   = { provider = "sim", endpoint = "outlet1" }

[devices.iron]
name  = "Soldering iron"
power = { provider = "sim", endpoint = "outlet4" }

[devices.broken]
power = { provider = "sim", endpoint = "fail_outlet" }

[devices.stuck]
power = { provider = "sim", endpoint = "stuck_outlet" }

[devices.lamp]
power = { provider = "sim", endpoint = "blind_lamp" }

[devices.laggy]
power = { provider = "slow", endpoint = "x", timeout = "100ms" }

[devices.scripted]
power = { script = "scripts/lamp.lua" }

[devices.scope]
name = "Oscilloscope"
"#;

struct Bench {
    dir: TempDir,
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Out {
    fn json(&self) -> Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("stdout is not JSON ({e}):\n{}", self.stdout))
    }
}

impl Bench {
    fn new() -> Self {
        Self::with_config(CONFIG)
    }

    fn with_config(config: &str) -> Self {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("desk.toml"), config).unwrap();
        Bench { dir }
    }

    fn config(&self) -> PathBuf {
        self.dir.path().join("desk.toml")
    }

    fn run(&self, args: &[&str]) -> Out {
        run_with(Some(&self.config()), self.dir.path(), args)
    }
}

fn run_with(config: Option<&Path>, state_dir: &Path, args: &[&str]) -> Out {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_desk"));
    cmd.env_remove("DESK_CONFIG")
        .env("DESK_MOCK_STATE", state_dir.join("mock-state.json"));
    if let Some(c) = config {
        cmd.arg("--config").arg(c);
    }
    let out = cmd.args(args).output().unwrap();
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

#[test]
fn power_on_persists_across_invocations() {
    let b = Bench::new();
    let out = b.run(&["--json", "power", "on", "psu"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let j = out.json();
    assert_eq!(j["ok"], true);
    assert_eq!(j["data"]["action"], "on");
    assert_eq!(j["data"]["results"][0]["device"], "psu");
    assert_eq!(j["data"]["results"][0]["power"]["state"], "on");
    assert_eq!(j["data"]["results"][0]["power"]["confidence"], "real");
    assert_eq!(j["data"]["results"][0]["before"]["state"], "off");
    assert_eq!(j["data"]["results"][0]["changed"], true);

    let j = b.run(&["--json", "power", "status", "psu"]).json();
    assert_eq!(j["data"]["devices"][0]["power"]["state"], "on");
}

#[test]
fn power_on_is_idempotent() {
    let b = Bench::new();
    b.run(&["power", "on", "psu"]);
    let j = b.run(&["--json", "power", "on", "psu"]).json();
    assert_eq!(j["data"]["results"][0]["changed"], false);
}

#[test]
fn aliases_names_and_toggle() {
    let b = Bench::new();
    assert_eq!(b.run(&["power", "on", "SUPPLY"]).code, 0);
    let j = b
        .run(&["--json", "power", "toggle", "fnirsi ips3608"])
        .json();
    assert_eq!(j["data"]["results"][0]["power"]["state"], "off");
}

#[test]
fn multiple_devices_and_dedup() {
    let b = Bench::new();
    let j = b
        .run(&["--json", "power", "on", "psu", "iron", "supply"])
        .json();
    let results = j["data"]["results"].as_array().unwrap();
    assert_eq!(results.len(), 2, "psu given twice (id + alias) runs once");
    assert!(results.iter().all(|r| r["power"]["state"] == "on"));
}

#[test]
fn partial_failure_reports_every_device() {
    let b = Bench::new();
    let out = b.run(&["--json", "power", "on", "psu", "broken"]);
    assert_eq!(out.code, 4);
    let j = out.json();
    assert_eq!(j["ok"], false);
    assert_eq!(j["error"]["code"], "endpoint_unavailable");
    assert_eq!(j["data"]["results"][0]["ok"], true);
    assert_eq!(j["data"]["results"][1]["ok"], false);
    assert_eq!(
        j["data"]["results"][1]["error"]["code"],
        "endpoint_unavailable"
    );
}

#[test]
fn verify_failure() {
    let b = Bench::new();
    let out = b.run(&["--json", "power", "on", "stuck"]);
    assert_eq!(out.code, 7);
    assert_eq!(out.json()["error"]["code"], "verify_failed");
}

#[test]
fn send_only_path_is_assumed_and_refuses_toggle() {
    let b = Bench::new();
    let j = b.run(&["--json", "power", "on", "lamp"]).json();
    assert_eq!(j["data"]["results"][0]["power"]["confidence"], "assumed");

    let out = b.run(&["--json", "power", "toggle", "lamp"]);
    assert_eq!(out.code, 4);
    assert_eq!(out.json()["error"]["code"], "state_unknown");
}

#[test]
fn timeout() {
    let b = Bench::new();
    let out = b.run(&["--json", "power", "on", "laggy"]);
    assert_eq!(out.code, 6);
    assert_eq!(out.json()["error"]["code"], "timeout");
}

#[test]
fn unknown_device_fails_before_touching_anything() {
    let b = Bench::new();
    let out = b.run(&["--json", "power", "on", "psu", "nope"]);
    assert_eq!(out.code, 2);
    let j = out.json();
    assert_eq!(j["error"]["code"], "device_not_found");
    assert!(j["error"]["hint"].as_str().unwrap().contains("device list"));

    let j = b.run(&["--json", "power", "status", "psu"]).json();
    assert_eq!(
        j["data"]["devices"][0]["power"]["state"], "off",
        "psu untouched"
    );
}

#[test]
fn device_without_power_path() {
    let b = Bench::new();
    let out = b.run(&["--json", "power", "on", "scope"]);
    assert_eq!(out.code, 3);
    assert_eq!(out.json()["error"]["code"], "no_power_path");
}

#[test]
fn script_bindings_not_yet_supported() {
    let b = Bench::new();
    let out = b.run(&["--json", "power", "on", "scripted"]);
    assert_eq!(out.code, 1);
    assert_eq!(out.json()["error"]["code"], "unsupported");
}

#[test]
fn power_all() {
    let b = Bench::with_config(
        r#"
        [providers.sim]
        kind = "mock"
        [devices.a]
        power = { provider = "sim", endpoint = "a" }
        [devices.b]
        power = { provider = "sim", endpoint = "b" }
        [devices.c]
        "#,
    );
    let out = b.run(&["--json", "power", "on", "--all"]);
    assert_eq!(out.code, 0);
    assert_eq!(out.json()["data"]["results"].as_array().unwrap().len(), 2);

    let j = b.run(&["--json", "power", "status"]).json();
    let devices = j["data"]["devices"].as_array().unwrap();
    assert!(devices.iter().all(|d| d["power"]["state"] == "on"));
}

#[test]
fn status_reports_errors_inline() {
    let b = Bench::new();
    let out = b.run(&["--json", "status"]);
    assert_eq!(out.code, 0, "status itself succeeds even if devices fail");
    let j = out.json();
    let devices = j["data"]["devices"].as_array().unwrap();
    let find = |id: &str| devices.iter().find(|d| d["id"] == id).unwrap();
    assert_eq!(find("psu")["power"]["state"], "off");
    assert_eq!(find("broken")["error"]["code"], "endpoint_unavailable");
    assert!(find("scope")["power"].is_null());
    assert_eq!(j["data"]["providers"][0]["ok"], true);
}

#[test]
fn device_list_and_show() {
    let b = Bench::new();
    let j = b.run(&["--json", "device", "list"]).json();
    let first = &j["data"]["devices"][0];
    assert_eq!(first["id"], "psu");
    assert_eq!(first["capabilities"][0], "power_switch");
    assert_eq!(first["power"]["kind"], "native");
    assert_eq!(first["power"]["provider"], "sim");
    assert_eq!(first["power"]["params"]["endpoint"], "outlet1");

    let out = b.run(&["device", "show", "supply"]);
    assert_eq!(out.code, 0);
    assert!(out.stdout.contains("FNIRSI IPS3608"));
    assert!(out.stdout.contains("sim (endpoint=outlet1)"));
}

#[test]
fn human_output() {
    let b = Bench::new();
    let out = b.run(&["power", "on", "psu"]);
    assert_eq!(out.code, 0);
    assert!(out.stdout.starts_with("DEVICE"), "{}", out.stdout);
    assert!(out.stdout.contains("switched (was off)"), "{}", out.stdout);

    let out = b.run(&["power", "on", "nope"]);
    assert_eq!(out.code, 2);
    assert!(out.stdout.is_empty());
    assert!(
        out.stderr.contains("error: no device matches `nope`"),
        "{}",
        out.stderr
    );
    assert!(out.stderr.contains("hint:"), "{}", out.stderr);
}

#[test]
fn config_errors() {
    let b = Bench::with_config("[devices.x]\npower = { provider = \"ghost\", endpoint = \"a\" }\n");
    let out = b.run(&["--json", "status"]);
    assert_eq!(out.code, 3);
    let j = out.json();
    assert_eq!(j["error"]["code"], "config_invalid");
    assert!(j["error"]["message"].as_str().unwrap().contains("ghost"));

    let dir = TempDir::new().unwrap();
    let missing = dir.path().join("nope.toml");
    let out = run_with(Some(&missing), dir.path(), &["--json", "status"]);
    assert_eq!(out.code, 3);
    assert!(
        out.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not found")
    );
}

#[test]
fn usage_errors_are_json_when_asked() {
    let b = Bench::new();
    let out = b.run(&["--json", "power", "on"]);
    assert_eq!(out.code, 2);
    assert_eq!(out.json()["error"]["code"], "usage");
}

#[test]
fn example_config_is_valid() {
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/desk.toml");
    let dir = TempDir::new().unwrap();
    let out = run_with(Some(&example), dir.path(), &["--json", "device", "list"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
}
