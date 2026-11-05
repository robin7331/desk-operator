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
    let find = |id: &str| devices.iter().find(|d| d["device"] == id).unwrap();
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

#[test]
fn dry_run_reads_but_never_switches() {
    let b = Bench::new();
    b.run(&["power", "on", "psu"]);
    let out = b.run(&["--json", "power", "on", "psu", "iron", "lamp", "--dry-run"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let j = out.json();
    assert_eq!(j["data"]["dry_run"], true);
    let r = j["data"]["results"].as_array().unwrap();
    assert_eq!(r[0]["would_send"], false, "psu already on");
    assert_eq!(r[1]["would_send"], true);
    assert_eq!(r[1]["before"]["state"], "off");
    assert_eq!(r[1]["target"], "on");
    assert_eq!(r[1]["power_path"], "sim (endpoint=outlet4)");
    assert!(r[2]["before"].is_null(), "send-only path has no state");

    let j = b.run(&["--json", "power", "status", "iron"]).json();
    assert_eq!(
        j["data"]["devices"][0]["power"]["state"], "off",
        "nothing was switched"
    );

    let out = b.run(&["--json", "power", "toggle", "lamp", "--dry-run"]);
    assert_eq!(out.code, 4);
    assert_eq!(out.json()["error"]["code"], "state_unknown");
}

#[test]
fn schema_describes_bench_commands_and_errors() {
    let b = Bench::new();
    let out = b.run(&["--json", "schema"]);
    assert_eq!(out.code, 0);
    let d = &out.json()["data"];
    assert_eq!(d["config"]["ok"], true);
    assert_eq!(d["devices"][0]["id"], "psu");
    assert_eq!(d["devices"][0]["aliases"][0], "supply");
    assert_eq!(d["providers"][0]["kind"], "mock");

    let commands: Vec<&str> = d["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["command"].as_str().unwrap())
        .collect();
    for expected in [
        "desk power on",
        "desk power status",
        "desk provider endpoints",
        "desk schema",
    ] {
        assert!(
            commands.contains(&expected),
            "{expected} missing: {commands:?}"
        );
    }
    let power_on = d["commands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["command"] == "desk power on")
        .unwrap();
    assert!(
        power_on["args"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["name"] == "--dry-run")
    );

    let globals: Vec<&str> = d["global_args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert!(globals.contains(&"--json") && globals.contains(&"--config"));

    let errors = d["errors"].as_array().unwrap();
    assert!(
        errors
            .iter()
            .any(|e| e["code"] == "verify_failed" && e["exit"] == 7)
    );
}

#[test]
fn schema_works_without_config() {
    let dir = TempDir::new().unwrap();
    let out = run_with(
        Some(&dir.path().join("missing.toml")),
        dir.path(),
        &["--json", "schema"],
    );
    assert_eq!(out.code, 0, "{}", out.stdout);
    let d = &out.json()["data"];
    assert_eq!(d["config"]["ok"], false);
    assert_eq!(d["config"]["error"]["code"], "config_invalid");
    assert!(!d["commands"].as_array().unwrap().is_empty());
}

#[test]
fn skill_matches_the_cli() {
    let dir = TempDir::new().unwrap();
    let out = run_with(
        Some(&dir.path().join("missing.toml")),
        dir.path(),
        &["skill"],
    );
    assert_eq!(out.code, 0, "skill works without config");
    let skill = out.stdout;
    assert!(skill.starts_with("---\nname: desk-operator\ndescription: "));

    // Every error code and command in the schema is documented, and the skill doesn't mention
    // error codes that don't exist.
    let schema = Bench::new().run(&["--json", "schema"]).json();
    let codes: Vec<String> = schema["data"]["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["code"].as_str().unwrap().to_string())
        .collect();
    for line in skill.lines().filter(|l| l.starts_with("| `")) {
        for code in line.split('`').skip(1).step_by(2) {
            if code.chars().all(|c| c.is_ascii_lowercase() || c == '_') && code.contains('_') {
                assert!(
                    codes.contains(&code.to_string()),
                    "skill mentions unknown code `{code}`"
                );
            }
        }
    }
    for cmd in [
        "desk power",
        "desk status",
        "desk schema",
        "desk device",
        "desk provider",
    ] {
        assert!(skill.contains(cmd), "skill doesn't mention `{cmd}`");
    }
}

#[test]
fn power_off_without_devices_or_with_all_means_everything() {
    for args in [
        &["--json", "power", "off"][..],
        &["--json", "power", "off", "all"][..],
        &["--json", "power", "off", "ALL"][..],
    ] {
        let b = Bench::new();
        b.run(&["power", "on", "psu", "iron"]);
        let out = b.run(args);
        let j = out.json();
        let results = j["data"]["results"].as_array().unwrap();
        // Every device with a power path is included (broken ones fail, the rest switch off).
        let ids: Vec<&str> = results
            .iter()
            .map(|r| r["device"].as_str().unwrap())
            .collect();
        assert!(
            ids.contains(&"psu") && ids.contains(&"iron") && ids.contains(&"lamp"),
            "{args:?}: {ids:?}"
        );
        assert!(
            !ids.contains(&"scope"),
            "devices without a power path are skipped"
        );
        let psu = results.iter().find(|r| r["device"] == "psu").unwrap();
        assert_eq!(psu["power"]["state"], "off", "{args:?}");
    }
}

#[test]
fn power_off_all_dry_run() {
    let b = Bench::new();
    b.run(&["power", "on", "psu"]);
    let j = b.run(&["--json", "power", "off", "--dry-run"]).json();
    assert_eq!(j["data"]["dry_run"], true);
    let j = b.run(&["--json", "power", "status", "psu"]).json();
    assert_eq!(
        j["data"]["devices"][0]["power"]["state"], "on",
        "dry run switched nothing"
    );
}

#[test]
fn switching_everything_on_needs_the_explicit_flag() {
    let b = Bench::new();
    for args in [
        &["--json", "power", "on"][..],
        &["--json", "power", "on", "all"][..],
        &["--json", "power", "toggle", "all"][..],
    ] {
        let out = b.run(args);
        assert_eq!(out.code, 2, "{args:?}");
        assert_eq!(out.json()["error"]["code"], "usage", "{args:?}");
    }
    let msg = b.run(&["--json", "power", "on", "all"]).json()["error"]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        msg.contains("only a shortcut for `desk power off`"),
        "{msg}"
    );
    let j = b.run(&["--json", "power", "status", "psu"]).json();
    assert_eq!(
        j["data"]["devices"][0]["power"]["state"], "off",
        "nothing was switched on"
    );
}

#[test]
fn all_is_a_reserved_device_name() {
    let b = Bench::with_config("[devices.lamp]\naliases = [\"All\"]\n");
    let out = b.run(&["--json", "device", "list"]);
    assert_eq!(out.code, 3);
    assert!(
        out.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("`All` is reserved")
    );
}

#[test]
fn core_never_references_specific_devices() {
    // The core knows capabilities, not models. Device definitions live in `devices/` and may be
    // moved to their own repository, so nothing in `src/` may name a vendor or model, or read the
    // repository's library folder directly (build.rs bundles it).
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    walk(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    let vendors: Vec<String> =
        std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("devices"))
            .unwrap()
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().to_lowercase())
            .collect();
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap().to_lowercase();
        // Bundling the library is build.rs's job; src/ never reaches into the repo's folders.
        assert!(
            !text.contains("cargo_manifest_dir"),
            "{} reaches into the repository (bundle through build.rs)",
            f.display()
        );
        for v in &vendors {
            assert!(
                !text.contains(v.as_str()),
                "{} mentions vendor `{v}`",
                f.display()
            );
        }
    }
}
