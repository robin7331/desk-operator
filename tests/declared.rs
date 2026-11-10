//! End-to-end tests of open capabilities: devices that declare their own values, settings and
//! actions, driven by the simulated device (mock provider) or by Lua drivers. The core's generic
//! rules: units, ranges, limits before anything is sent, declaration order, verification, dry runs,
//! channels, `watch`, and the model format.

use std::path::Path;
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

/// A soldering iron on the simulated provider: its temperature follows the setpoint.
const IRON: &str = r#"
[providers.sim]
kind = "mock"

[devices.iron]
power  = { provider = "sim", endpoint = "iron_mains" }
limits = { heater = { setpoint = 380 } }

[devices.iron.heater]
provider = "sim"
endpoint = "iron"
follow   = { temperature = "setpoint" }

[devices.iron.heater.values]
temperature = { type = "number", unit = "°C", doc = "tip temperature" }
last_action = { type = "string" }

[devices.iron.heater.settings]
setpoint = { type = "number", unit = "°C", min = 50, max = 450 }
mode     = { type = "choice", choices = ["normal", "boost"] }
enabled  = { type = "boolean" }

[devices.iron.heater.actions]
standby = { switches = true, doc = "to standby temperature" }
boost   = { switches = true, args = { seconds = { type = "number", unit = "s", min = 1, max = 60 } } }

# Ignores every set (the device doesn't take it): verification must catch it.
[devices.stuck]
heater = { provider = "sim", endpoint = "stuck_iron", settings = { setpoint = { type = "number", unit = "°C" } } }

# Reports no settings back: set, but not confirmed.
[devices.blind]
heater = { provider = "sim", endpoint = "blind_iron", settings = { setpoint = { type = "number", unit = "°C" } }, values = { temperature = { type = "number", unit = "°C" } }, fixed = { temperature = 21.5 } }

[devices.broken]
heater = { provider = "sim", endpoint = "fail_iron", values = { temperature = { type = "number", unit = "°C" } }, settings = { setpoint = { type = "number", unit = "°C" } } }

[devices.lamp]
power = { provider = "sim", endpoint = "lamp" }
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
        serde_json::from_str(&self.stdout).unwrap_or_else(|e| {
            panic!(
                "stdout is not JSON ({e}):\n{}\n{}",
                self.stdout, self.stderr
            )
        })
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
    fn message(&self) -> String {
        self.json()["error"]["message"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }
    fn lines(&self) -> Vec<Value> {
        self.stdout
            .lines()
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}")))
            .collect()
    }
}

impl Bench {
    fn with(config: &str) -> Self {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("desk.toml"), config).unwrap();
        Bench { dir }
    }
    fn file(&self, name: &str, text: &str) {
        let p = self.dir.path().join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    fn run(&self, args: &[&str]) -> Out {
        run(self.dir.path(), args)
    }
}

fn run(dir: &Path, args: &[&str]) -> Out {
    let out = Command::new(env!("CARGO_BIN_EXE_desk"))
        .env_remove("DESK_CONFIG")
        .env_remove("DESK_MODELS")
        .env("DESK_MOCK_STATE", dir.join("mock-state.json"))
        .env("DESK_RUNTIME_DIR", dir.join("run"))
        .env("DESK_DAEMON_IDLE", "1s")
        .arg("--config")
        .arg(dir.join("desk.toml"))
        .args(args)
        .output()
        .unwrap();
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn iron() -> Bench {
    let b = Bench::with(IRON);
    assert_eq!(b.run(&["power", "on", "iron"]).code, 0);
    b
}

#[test]
fn set_get_and_do_with_units_and_verification() {
    let b = iron();
    let out = b.run(&["--json", "set", "iron", "setpoint", "330c", "mode", "boost"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let d = out.data();
    assert_eq!(d["device"], "iron");
    assert_eq!(d["mains"], "on");
    assert_eq!(d["applied"], true);
    assert_eq!(d["requested"]["setpoint"], 330.0);
    assert_eq!(d["verified"]["setpoint"], true);
    assert_eq!(d["verified"]["mode"], true);
    assert_eq!(d["values"]["temperature"], 330.0);

    // Persisted in the device; other spellings of °C.
    let d = b.run(&["--json", "get", "iron"]).data();
    assert_eq!(d["values"]["setpoint"], 330.0);
    assert_eq!(d["values"]["mode"], "boost");
    assert_eq!(d["values"]["enabled"], false);
    assert!(d["requested"].is_null());
    assert_eq!(d["applied"], false);
    for spelling in ["300°C", "300 degc", "300C"] {
        let out = b.run(&["--json", "set", "iron", "setpoint", spelling]);
        assert_eq!(out.code, 0, "{spelling}: {}", out.stdout);
    }
    let out = b.run(&["--json", "set", "iron", "enabled", "on"]);
    assert_eq!(out.data()["values"]["enabled"], true);

    // Only the named values.
    let d = b
        .run(&["--json", "get", "iron", "temperature", "mode"])
        .data();
    assert_eq!(d["values"].as_object().unwrap().len(), 2, "{d}");

    let out = b.run(&["--json", "do", "iron", "boost", "seconds=10s"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let d = out.data();
    assert_eq!(d["action"], "boost");
    assert_eq!(d["args"]["seconds"], 10.0);
    assert_eq!(d["values"]["last_action"], "boost seconds=10");
    let out = b.run(&["do", "iron", "standby"]);
    assert!(out.stdout.contains("done: standby"), "{}", out.stdout);

    // Human output shows units.
    let out = b.run(&["get", "iron"]);
    assert!(out.stdout.contains("setpoint     300 °C"), "{}", out.stdout);
    assert!(out.stdout.contains("(mains on)"), "{}", out.stdout);
}

#[test]
fn ranges_and_limits_refuse_before_anything_is_sent() {
    let b = iron();
    assert_eq!(b.run(&["set", "iron", "setpoint", "200c"]).code, 0);
    for (value, kind) in [
        ("400c", "bench limit"),
        ("500c", "device maximum"),
        ("20c", "device minimum"),
    ] {
        let out = b.run(&["--json", "set", "iron", "setpoint", value, "mode", "boost"]);
        assert_eq!(out.code, 8, "{value}: {}", out.stdout);
        assert_eq!(out.code_str(), "limit_exceeded");
        assert!(out.message().contains(kind), "{}", out.message());
    }
    // Nothing was sent: neither the setpoint nor the mode in the same command.
    let d = b.run(&["--json", "get", "iron"]).data();
    assert_eq!(d["values"]["setpoint"], 200.0);
    assert_eq!(d["values"]["mode"], "normal");
    // Action arguments have ranges too.
    let out = b.run(&["--json", "do", "iron", "boost", "seconds=2min"]);
    assert_eq!(out.code_str(), "limit_exceeded", "{}", out.stdout);
    assert!(out.message().contains("120 s"), "{}", out.message());
}

#[test]
fn dry_runs_send_nothing() {
    let b = iron();
    let out = b.run(&["--json", "set", "iron", "setpoint", "250c", "--dry-run"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.data()["applied"], false);
    assert!(out.data()["verified"].is_null());
    let out = b.run(&["--json", "do", "iron", "standby", "--dry-run"]);
    assert_eq!(out.data()["applied"], false);
    assert!(out.data()["values"]["last_action"].is_null());
    let d = b.run(&["--json", "get", "iron"]).data();
    assert_eq!(d["values"]["setpoint"], 50.0, "still the starting value");
    let out = b.run(&["set", "iron", "setpoint", "250c", "--dry-run"]);
    assert!(
        out.stdout
            .contains("dry run, nothing sent: would set setpoint 250 °C"),
        "{}",
        out.stdout
    );
}

#[test]
fn honest_about_what_the_device_confirms() {
    let b = iron();
    let out = b.run(&["--json", "set", "stuck", "setpoint", "300c"]);
    assert_eq!(out.code, 7, "{}", out.stdout);
    assert_eq!(out.code_str(), "verify_failed");

    let out = b.run(&["--json", "set", "blind", "setpoint", "300c"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert!(
        out.data()["verified"].is_null(),
        "not confirmed: {}",
        out.stdout
    );
    assert_eq!(out.data()["values"]["temperature"], 21.5);
    let out = b.run(&["set", "blind", "setpoint", "300c"]);
    assert!(
        out.stdout.contains("setpoint NOT confirmed"),
        "{}",
        out.stdout
    );

    let out = b.run(&["--json", "get", "broken"]);
    assert_eq!(out.code_str(), "endpoint_unavailable");
}

#[test]
fn mistakes_get_clear_usage_errors() {
    let b = iron();
    for (args, needle) in [
        (vec!["set", "iron", "setpoint", "330"], "needs a unit"),
        (vec!["set", "iron", "setpoint", "330v"], "not a unit for °C"),
        (
            vec!["set", "iron", "mode", "turbo"],
            "not one of: normal, boost",
        ),
        (vec!["set", "iron", "enabled", "maybe"], "not on/off"),
        (
            vec!["set", "iron", "temperature", "300c"],
            "a value the device reports",
        ),
        (vec!["set", "iron", "standby", "on"], "is an action"),
        (
            vec!["set", "iron", "tip", "1"],
            "has no `tip` (it has: setpoint, mode, enabled",
        ),
        (
            vec!["set", "iron", "setpoint", "300c", "setpoint", "310c"],
            "given twice",
        ),
        (vec!["set", "iron", "setpoint"], "needs a value"),
        (vec!["do", "iron", "boost"], "needs `seconds=…`"),
        (
            vec!["do", "iron", "boost", "secs=10s"],
            "no argument `secs`",
        ),
        (vec!["do", "iron", "boost", "10s"], "name=value"),
        (vec!["do", "iron", "setpoint"], "not an action"),
        (vec!["get", "iron", "boost"], "is an action"),
        (vec!["get", "iron:2"], "no channels"),
        (
            vec!["set", "iron", "setpoint", "300c", "--clear-protection"],
            "output on",
        ),
    ] {
        let mut a = vec!["--json"];
        a.extend(&args);
        let out = b.run(&a);
        assert_eq!(out.code, 2, "{args:?}: {}", out.stdout);
        assert_eq!(out.code_str(), "usage", "{args:?}");
        assert!(
            out.message().contains(needle),
            "{args:?}: {}",
            out.message()
        );
    }
    // A device that can't do it.
    let out = b.run(&["--json", "set", "lamp", "voltage", "5v"]);
    assert_eq!(out.code_str(), "capability_missing", "{}", out.stdout);
    // Mains off: unreachable.
    assert_eq!(b.run(&["power", "off", "iron"]).code, 0);
    let out = b.run(&["--json", "get", "iron"]);
    assert_eq!(out.code_str(), "device_off");
}

#[test]
fn a_left_out_value_or_setting_lists_what_it_takes() {
    let b = iron();
    assert_eq!(b.run(&["set", "iron", "setpoint", "300c"]).code, 0);

    // One setting: a number, with its current value.
    let out = b.run(&["set", "iron", "setpoint"]);
    assert_eq!(out.code, 2, "{}", out.stdout);
    for needle in [
        "setpoint: 300 °C (current)\n",
        "unit: °C\n",
        "range: 50…450\n",
        "bench limit: 380\n",
        "`setpoint` needs a value (nothing was set): desk set iron setpoint <value>",
    ] {
        assert!(out.stdout.contains(needle), "{needle}\n{}", out.stdout);
    }
    assert!(!out.stderr.contains("error"), "said once: {}", out.stderr);
    let out = b.run(&["--json", "set", "iron", "setpoint"]);
    assert_eq!(out.code_str(), "usage");
    let d = out.data();
    assert_eq!(d["setting"], "setpoint");
    assert_eq!(d["value"], 300.0, "{d}");
    assert_eq!(d["max"], 450.0);
    assert_eq!(d["limit"], 380.0);

    // A choice and a boolean.
    let out = b.run(&["set", "iron", "mode"]);
    assert!(
        out.stdout.contains("choices: normal, boost\n"),
        "{}",
        out.stdout
    );
    let out = b.run(&["set", "iron", "enabled"]);
    assert!(out.stdout.contains("choices: on, off\n"), "{}", out.stdout);

    // Every setting.
    let out = b.run(&["set", "iron"]);
    assert_eq!(out.code, 2);
    for needle in [
        "setpoint  number in °C, 50…450, limit 380\n",
        "mode      normal | boost\n",
        "enabled   on | off\n",
        "desk set iron <setting> <value>",
    ] {
        assert!(out.stdout.contains(needle), "{needle}\n{}", out.stdout);
    }
    let out = b.run(&["--json", "set", "iron"]);
    assert_eq!(out.code_str(), "usage");
    assert_eq!(out.data()["settings"]["mode"]["type"], "choice");

    // A pair short: nothing of the line is applied.
    let out = b.run(&["set", "iron", "setpoint", "320c", "mode"]);
    assert_eq!(out.code, 2);
    assert!(
        out.stdout
            .contains("desk set iron setpoint 320c mode <value>"),
        "{}",
        out.stdout
    );
    assert_eq!(
        b.run(&["--json", "get", "iron"]).data()["values"]["setpoint"],
        300.0
    );

    // What can't be listed is still refused the usual way.
    for (args, needle) in [
        (
            vec!["set", "iron", "temperature"],
            "a value the device reports",
        ),
        (vec!["set", "iron", "standby"], "is an action"),
        (vec!["set", "iron", "tip"], "has no `tip`"),
        (vec!["set", "lamp"], "has no settings"),
        (vec!["do", "lamp"], "has no actions"),
        (vec!["set", "iron:2"], "no channels"),
    ] {
        let mut a = vec!["--json"];
        a.extend(&args);
        let out = b.run(&a);
        assert_eq!(out.code_str(), "usage", "{args:?}");
        assert!(
            out.message().contains(needle),
            "{args:?}: {}",
            out.message()
        );
        assert!(out.json().get("data").is_none(), "{args:?}");
    }
    assert_eq!(
        b.run(&["--json", "set", "nope"]).code_str(),
        "device_not_found"
    );
}

#[test]
fn listing_never_switches_mains_and_says_why_a_value_is_missing() {
    let b = iron();
    assert_eq!(b.run(&["power", "off", "iron"]).code, 0);
    let out = b.run(&["set", "iron", "mode"]);
    assert_eq!(out.code, 2);
    assert!(out.stdout.contains("mode: not read ("), "{}", out.stdout);
    assert!(
        out.stdout.contains("choices: normal, boost"),
        "{}",
        out.stdout
    );
    let d = b.run(&["--json", "set", "iron", "mode"]).data();
    assert_eq!(d["value_error"]["code"], "device_off", "{d}");
    assert!(d.get("value").is_none());
    // The list of settings doesn't need the device at all.
    assert_eq!(b.run(&["--json", "set", "iron"]).code_str(), "usage");
    let status = b.run(&["--json", "power", "status", "iron"]).data();
    assert_eq!(status["devices"][0]["power"]["state"], "off", "{status}");

    // A device that fails to answer: the description still comes, with the reason.
    let out = b.run(&["--json", "set", "broken", "setpoint"]);
    assert_eq!(out.code_str(), "usage");
    let d = out.data();
    assert_eq!(d["unit"], "°C");
    assert_eq!(d["value_error"]["code"], "endpoint_unavailable", "{d}");
}

#[test]
fn a_left_out_action_lists_actions_and_their_arguments() {
    let b = iron();
    let out = b.run(&["do", "iron"]);
    assert_eq!(out.code, 2);
    for needle in [
        "standby  to standby temperature\n",
        "boost    seconds=<number in s, 1…60>\n",
        "desk do iron <action>",
    ] {
        assert!(out.stdout.contains(needle), "{needle}\n{}", out.stdout);
    }
    let out = b.run(&["--json", "do", "iron"]);
    assert_eq!(out.code_str(), "usage");
    assert_eq!(
        out.data()["actions"]["boost"]["args"]["seconds"]["max"],
        60.0
    );

    // An action that needs arguments says what they take, and isn't run.
    let out = b.run(&["do", "iron", "boost"]);
    assert_eq!(out.code, 2);
    assert!(
        out.stdout.contains("seconds=<number in s, 1…60>"),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout.contains("desk do iron boost seconds=<value>"),
        "{}",
        out.stdout
    );
    let got = b.run(&["--json", "get", "iron"]).data();
    assert!(got["values"].get("last_action").is_none(), "{got}");
    // One that needs none runs, as before.
    assert_eq!(b.run(&["do", "iron", "standby"]).code, 0);
}

#[test]
fn listings_address_channels() {
    let b = Bench::with(RELAYS);
    b.file("relays.lua", RELAYS_DRIVER);
    let out = b.run(&["set", "relays"]);
    assert!(out.stdout.contains("channels of"), "{}", out.stdout);
    assert!(
        out.stdout
            .contains("desk set relays:<channel> <setting> <value>"),
        "{}",
        out.stdout
    );
    // Without a channel there's no one current value.
    let d = b.run(&["--json", "set", "relays", "on"]).data();
    assert!(d.get("value").is_none(), "{d}");
    assert_eq!(d["channels"], serde_json::json!(["ch1", "ch2"]));
    let d = b.run(&["--json", "set", "relays:ch2", "on"]).data();
    assert_eq!(d["value"], true, "{d}");
    // The channel's own limit.
    let d = b.run(&["--json", "set", "relays:ch2", "delay"]).data();
    let ch1 = b.run(&["--json", "set", "relays:ch1", "delay"]).data();
    assert_ne!(d["limit"], ch1["limit"], "{d} {ch1}");
    let out = b.run(&["--json", "set", "relays:ch3", "on"]);
    assert!(
        out.message().contains("no channel `ch3`"),
        "{}",
        out.message()
    );
}

#[test]
fn the_schema_describes_every_declared_name() {
    let b = iron();
    let d = b.run(&["--json", "schema"]).data();
    let iron = d["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == "iron")
        .unwrap()
        .clone();
    assert_eq!(
        iron["capabilities"],
        serde_json::json!(["power_switch", "heater"])
    );
    let sp = &iron["settings"]["setpoint"];
    assert_eq!(sp["capability"], "heater");
    assert_eq!(sp["unit"], "°C");
    assert_eq!(sp["max"], 450.0);
    assert_eq!(sp["limit"], 380.0);
    assert_eq!(sp["switches"], true);
    assert_eq!(iron["values"]["temperature"]["switches"], false);
    assert_eq!(iron["actions"]["boost"]["args"]["seconds"]["unit"], "s");
    assert!(d["capabilities"]["declared"]["commands"].is_array());

    let out = b.run(&["device", "show", "iron"]);
    for needle in [
        "capabilities: power_switch, heater",
        "settings:\n  setpoint  number in °C, 50…450, limit 380\n",
        "  mode      normal | boost\n",
        "  enabled   on | off\n",
        "values:\n  temperature  number in °C\n               tip temperature\n",
        "  last_action  text\n",
        "actions:\n  standby  to standby temperature\n",
        "  boost    seconds=<number in s, 1…60>\n",
        "limits:       heater setpoint ≤ 380",
    ] {
        assert!(out.stdout.contains(needle), "{needle}\n{}", out.stdout);
    }
}

#[test]
fn watch_streams_declared_values_and_changes() {
    let b = iron();
    let out = b.run(&[
        "--json",
        "watch",
        "iron",
        "--interval",
        "20ms",
        "--count",
        "2",
    ]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let lines = out.lines();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["data"]["values"]["setpoint"], 50.0);
    assert!(lines[0]["data"]["values"]["mode"].is_string());

    let out = b.run(&[
        "--json",
        "watch",
        "iron",
        "temperature",
        "--interval",
        "20ms",
        "--count",
        "1",
    ]);
    let v = &out.lines()[0]["data"]["values"];
    assert_eq!(v.as_object().unwrap().len(), 1, "{v}");

    let out = b.run(&[
        "watch",
        "iron",
        "temperature",
        "mode",
        "--interval",
        "20ms",
        "--count",
        "1",
    ]);
    assert!(
        out.stdout.starts_with("ELAPSED     temperature  mode"),
        "{}",
        out.stdout
    );
    assert!(out.stdout.contains("50 °C"), "{}", out.stdout);

    // --changes: nothing changes, so only the first reading comes within the time.
    let mut watch = Command::new(env!("CARGO_BIN_EXE_desk"))
        .env("DESK_MOCK_STATE", b.dir.path().join("mock-state.json"))
        .env("DESK_RUNTIME_DIR", b.dir.path().join("run"))
        .env("DESK_DAEMON_IDLE", "1s")
        .arg("--config")
        .arg(b.dir.path().join("desk.toml"))
        .args(["--json", "watch", "iron", "--interval", "20ms", "--changes"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1500));
    watch.kill().unwrap();
    let out = watch.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(text.lines().count(), 1, "{text}");

    let out = b.run(&["--json", "watch", "iron", "tip", "--count", "1"]);
    assert_eq!(out.lines()[0]["error"]["code"], "usage");
    let out = b.run(&["--json", "watch", "lamp", "--count", "1"]);
    assert_eq!(out.lines()[0]["error"]["code"], "capability_missing");
}

// ---- Lua drivers ------------------------------------------------------------------------------

/// A bench meter in Lua: its reading's unit follows the function setting; it logs every call.
const METER_DRIVER: &str = r#"
PARAMS = {}
local s = { fn = "dcv", range = 10 }
local readings = { dcv = { value = 4.98, unit = "V" }, ohm = { value = 1500, unit = "Ω" } }
function read(ctx)
  ctx.log("read")
  return { ["function"] = s.fn, range = s.range, reading = readings[s.fn], overload = false }
end
function set(ctx, name, value)
  ctx.log("set " .. name .. "=" .. tostring(value))
  if name == "function" then s.fn = value else s.range = value end
end
function action(ctx, name, args)
  ctx.log("action " .. name .. " " .. tostring(args.mode))
end
"#;

const METER: &str = r#"
[devices.dmm]
[devices.dmm.meter]
script = "meter.lua"
settings = { range = { type = "number", unit = "V", min = 0.1, max = 1000 }, function = { type = "choice", choices = ["dcv", "ohm"], switches = false } }
values   = { reading = { type = "number", units = ["V", "Ω"] }, overload = { type = "boolean" } }
actions  = { hold = { switches = false, args = { mode = { type = "choice", choices = ["on", "off"] } } } }
"#;

#[test]
fn lua_driver_with_changing_units_and_declaration_order() {
    let b = Bench::with(METER);
    b.file("meter.lua", METER_DRIVER);
    let out = b.run(&["--json", "get", "dmm"]);
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    let v = &out.data()["values"];
    assert_eq!(
        v["reading"],
        serde_json::json!({ "value": 4.98, "unit": "V" })
    );
    assert_eq!(v["function"], "dcv");

    // Typed function first, range second; applied in declaration order (range, function).
    let out = b.run(&["--json", "set", "dmm", "function", "ohm", "range", "20v"]);
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    let d = out.data();
    assert_eq!(d["values"]["reading"]["unit"], "Ω");
    let log: Vec<&str> = d["log"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap())
        .collect();
    assert_eq!(
        log,
        ["set range=20.0", "set function=ohm", "read"],
        "{log:?}"
    );
    assert_eq!(d["verified"]["range"], true);

    let out = b.run(&["--json", "do", "dmm", "hold", "mode=on"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert!(out.data()["log"].to_string().contains("action hold on"));

    let out = b.run(&["get", "dmm"]);
    assert!(out.stdout.contains("reading   4.98 V"), "{}", out.stdout);
}

#[test]
fn lua_driver_readings_are_checked_against_the_declarations() {
    for (read, needle) in [
        ("return { volts = 1 }", "`volts` is not a declared value"),
        (
            "return { reading = { value = 1, unit = \"A\" } }",
            "unit \"A\" is not one of",
        ),
        ("return { reading = 1 }", "{ value = <number>"),
        ("return { overload = \"no\" }", "must be a boolean"),
        ("return 5", "must return"),
    ] {
        let b = Bench::with(METER);
        b.file(
            "meter.lua",
            &format!(
                "PARAMS = {{}}\nfunction read(ctx) {read} end\nfunction set(ctx) end\nfunction action(ctx) end\n"
            ),
        );
        let out = b.run(&["--json", "get", "dmm"]);
        assert_eq!(out.code_str(), "script_failed", "{read}: {}", out.stdout);
        assert!(out.message().contains(needle), "{read}: {}", out.message());
    }
    // A driver must implement what's declared.
    let b = Bench::with(METER);
    b.file(
        "meter.lua",
        "PARAMS = {}\nfunction read(ctx) return {} end\n",
    );
    let out = b.run(&["--json", "get", "dmm"]);
    assert_eq!(out.code_str(), "config_invalid");
    assert!(
        out.message().contains("`set(ctx, ...)`"),
        "{}",
        out.message()
    );
}

#[test]
fn a_channel_minimum_keeps_the_general_maximum() {
    let cfg = RELAYS.replace(
        "limits = { board = { delay = 5, ch2 = { delay = 1 } } }",
        "limits = { board = { delay = 5, ch2 = { delay = { min = 1 } } } }",
    );
    assert_ne!(cfg, RELAYS);
    let b = Bench::with(&cfg);
    b.file("relays.lua", RELAYS_DRIVER);
    let out = b.run(&["--json", "set", "relays:ch2", "delay", "6s"]);
    assert_eq!(out.code_str(), "limit_exceeded", "{}", out.message());
    let out = b.run(&["--json", "set", "relays:ch2", "delay", "0.5s"]);
    assert_eq!(out.code_str(), "limit_exceeded", "{}", out.message());
    assert!(out.message().contains("below"), "{}", out.message());
    let out = b.run(&["--json", "set", "relays:ch2", "delay", "3s"]);
    assert_eq!(out.code, 0, "{}", out.message());
    let out = b.run(&["--json", "set", "relays:ch1", "delay", "0.5s"]);
    assert_eq!(out.code, 0, "{}", out.message());
}

#[test]
fn impossible_limits_are_a_config_error() {
    for line in [
        "limits = { board = { delay = { min = 4, max = 2 } } }",
        "limits = { board = { delay = 5, ch2 = { delay = { min = 6 } } } }",
    ] {
        let cfg = RELAYS.replace(
            "limits = { board = { delay = 5, ch2 = { delay = 1 } } }",
            line,
        );
        assert_ne!(cfg, RELAYS);
        let b = Bench::with(&cfg);
        b.file("relays.lua", RELAYS_DRIVER);
        let out = b.run(&["--json", "get", "relays"]);
        assert_eq!(out.code_str(), "config_invalid", "{line}");
        assert!(
            out.message().contains("`min` is above `max`"),
            "{}",
            out.message()
        );
    }
}

const RELAYS: &str = r#"
[devices.relays]
limits = { board = { delay = 5, ch2 = { delay = 1 } } }
[devices.relays.board]
script = "relays.lua"
channels = ["ch1", "ch2"]
settings = { on = { type = "boolean" }, delay = { type = "number", unit = "s", min = 0, max = 10 } }
"#;

const RELAYS_DRIVER: &str = r#"
local state = { ch1 = { on = false, delay = 0 }, ch2 = { on = true, delay = 0 } }
function read(ctx) local c = state[ctx.channel]; return { on = c.on, delay = c.delay } end
function set(ctx, name, value) ctx.log("set " .. ctx.channel .. " " .. name); state[ctx.channel][name] = value end
"#;

#[test]
fn channels_are_addressed_and_limited_one_by_one() {
    let b = Bench::with(RELAYS);
    b.file("relays.lua", RELAYS_DRIVER);
    let d = b.run(&["--json", "get", "relays"]).data();
    assert_eq!(d["channels"]["ch1"]["on"], false, "{d}");
    assert_eq!(d["channels"]["ch2"]["on"], true);
    let d = b.run(&["--json", "get", "relays:ch2"]).data();
    assert_eq!(d["channel"], "ch2");
    assert_eq!(d["values"]["on"], true, "{d}");

    let out = b.run(&["--json", "set", "relays:ch1", "on", "on", "delay", "4s"]);
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    assert!(out.data()["log"].to_string().contains("set ch1 on"));
    // The channel's own limit wins over the capability's.
    let out = b.run(&["--json", "set", "relays:ch2", "delay", "4s"]);
    assert_eq!(out.code_str(), "limit_exceeded", "{}", out.stdout);
    let out = b.run(&["--json", "set", "relays:ch1", "delay", "6s"]);
    assert_eq!(out.code_str(), "limit_exceeded");

    for (args, needle) in [
        (
            vec!["set", "relays", "on", "on"],
            "address one, e.g. `relays:ch1`",
        ),
        (vec!["set", "relays:ch3", "on", "on"], "no channel `ch3`"),
    ] {
        let mut a = vec!["--json"];
        a.extend(&args);
        let out = b.run(&a);
        assert_eq!(out.code_str(), "usage", "{args:?}");
        assert!(
            out.message().contains(needle),
            "{args:?}: {}",
            out.message()
        );
    }
    let out = b.run(&["get", "relays"]);
    assert!(out.stdout.contains("channel ch2:"), "{}", out.stdout);
}

const SINK: &str = r#"
[devices.load.sink]
script = "sink.lua"
settings = { level = { type = "number", unit = "A", min = 0, max = 10 }, enabled = { type = "boolean" } }
"#;

const SINK_DRIVER: &str = r#"
local s = { level = 0, enabled = true }
function read(ctx) return { level = s.level, enabled = s.enabled } end
function set(ctx, name, value) ctx.log("set " .. name); s[name] = value end
"#;

#[test]
fn switching_off_comes_before_other_settings() {
    let b = Bench::with(SINK);
    b.file("sink.lua", SINK_DRIVER);
    let out = b.run(&["--json", "set", "load", "level", "5a", "enabled", "off"]);
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    let log = out.data()["log"].to_string();
    let (e, l) = (log.find("set enabled"), log.find("set level"));
    assert!(e.is_some() && l.is_some() && e < l, "{log}");

    let b = Bench::with(SINK);
    b.file("sink.lua", SINK_DRIVER);
    let out = b.run(&["--json", "set", "load", "level", "3a", "enabled", "on"]);
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    let log = out.data()["log"].to_string();
    let (e, l) = (log.find("set enabled"), log.find("set level"));
    assert!(e.is_some() && l.is_some() && l < e, "{log}");
}

#[test]
fn power_supplies_can_have_channels() {
    let b = Bench::with(
        r#"
[devices.psu]
source = { script = "psu.lua", channels = 2, ratings = { max_voltage = 30, max_current = 3 } }
limits = { source = { voltage = 12, "2" = { voltage = 5 } } }
"#,
    );
    b.file(
        "psu.lua",
        r#"
local s = { ["1"] = { v = 0 }, ["2"] = { v = 0 } }
function set_voltage(ctx, v) s[ctx.channel].v = v end
function set_current(ctx, a) end
function set_output(ctx, on) end
function read(ctx) return { voltage_set = s[ctx.channel].v } end
"#,
    );
    let out = b.run(&["--json", "set", "psu:1", "voltage", "9v"]);
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    assert_eq!(out.data()["verified"]["voltage"], true);
    let out = b.run(&["--json", "set", "psu:2", "voltage", "9v"]);
    assert_eq!(
        out.code_str(),
        "limit_exceeded",
        "channel 2 is limited to 5 V"
    );
    let d = b.run(&["--json", "get", "psu"]).data();
    assert!(
        d["channels"]["1"].is_object() && d["channels"]["2"].is_object(),
        "{d}"
    );
}

#[test]
fn names_that_clash_are_qualified() {
    let b = Bench::with(
        r#"
[providers.sim]
kind = "mock"
[devices.combo]
a = { provider = "sim", endpoint = "a", settings = { level = { type = "number", unit = "%" } } }
b = { provider = "sim", endpoint = "b", settings = { level = { type = "number", unit = "%" } } }
"#,
    );
    let out = b.run(&["--json", "set", "combo", "level", "50%"]);
    assert_eq!(out.code_str(), "usage");
    assert!(
        out.message().contains("`a.level` or `b.level`"),
        "{}",
        out.message()
    );
    let out = b.run(&["--json", "set", "combo", "b.level", "50%"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let d = b.run(&["--json", "schema"]).data();
    assert!(
        d["devices"][0]["settings"]["a.level"].is_object(),
        "{}",
        d["devices"][0]
    );
}

#[test]
fn config_errors_name_the_problem() {
    for (config, needle) in [
        (
            "[devices.x]\nheater = { script = \"x.lua\" }",
            "must declare `values`, `settings` or `actions` (is `heater` a typo?)",
        ),
        (
            "[devices.x]\nheater = { script = \"x.lua\", values = { t = { type = \"number\", unit = \"C\" } } }",
            "write the unit as \"°C\"",
        ),
        (
            "[devices.x]\nheater = { script = \"x.lua\", actions = { go = {} } }",
            "needs `switches = true`",
        ),
        (
            "[devices.x]\nheater = { script = \"x.lua\", settings = { s = { type = \"number\", unit = \"V\" } } }\nlimits = { heater = { t = 5 } }",
            "`heater.t` is not a setting of `heater`",
        ),
        (
            "[devices.x]\nheater = { script = \"x.lua\", settings = { s = { type = \"boolean\" } } }\nlimits = { heater = { s = 5 } }",
            "isn't a number setting",
        ),
        (
            "[devices.x]\nmains = { script = \"x.lua\", values = { t = { type = \"boolean\" } } }",
            "can't be a capability name",
        ),
        ("[devices.x]\ncolour = \"red\"", "unknown key `colour`"),
        (
            "[devices.x]\npower = { script = \"x.lua\", values = { t = { type = \"boolean\" } } }",
            "mains (`power`) declares nothing",
        ),
    ] {
        let b = Bench::with(config);
        let out = b.run(&["--json", "device", "list"]);
        assert_eq!(out.code, 3, "{config}: {}", out.stdout);
        assert!(
            out.message().contains(needle),
            "{config}: {}",
            out.message()
        );
    }
}

// ---- models ------------------------------------------------------------------------------

#[test]
fn models_declare_capabilities_and_devices_use_them() {
    let b = Bench::with(
        r#"
[devices.dmm]
model = "acme/dmm-1"
meter = { params = { range_default = 10 } }
limits = { meter = { range = 100 } }
"#,
    );
    b.file(
        "models/acme/dmm-1/model.toml",
        r#"
[model]
id = "acme/dmm-1"
name = "ACME DMM 1"
requires = "desk-operator >= 0.1"
status = "community"

[meter]
script = "meter.lua"

[meter.settings]
range = { type = "number", unit = "V", min = 0.1, max = 1000 }
function = { type = "choice", choices = ["dcv", "ohm"], switches = false }

[meter.values]
reading = { type = "number", units = ["V", "Ω"] }
overload = { type = "boolean" }

[meter.actions]
hold = { switches = false, args = { mode = { type = "choice", choices = ["on", "off"] } } }
"#,
    );
    b.file(
        "models/acme/dmm-1/meter.lua",
        &METER_DRIVER.replace(
            "PARAMS = {}",
            "PARAMS = { range_default = { type = \"number\", doc = \"x\" } }",
        ),
    );
    let out = b.run(&["--json", "model", "check", "acme/dmm-1"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let out = b.run(&["--json", "get", "dmm"]);
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    let out = b.run(&["--json", "set", "dmm", "range", "200v"]);
    assert_eq!(
        out.code_str(),
        "limit_exceeded",
        "the device's limit applies"
    );
    let out = b.run(&["--json", "model", "show", "acme/dmm-1"]);
    assert!(
        out.data()["driver_params"]["meter"].is_array(),
        "{}",
        out.stdout
    );
}

#[test]
fn models_new_scaffolds_a_declared_capability() {
    let b = Bench::with("");
    let out = b.run(&[
        "--json",
        "model",
        "new",
        "acme/iron-9",
        "--capability",
        "heater",
        "--name",
        "ACME Iron 9",
    ]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.data()["check"]["ok"], true, "{}", out.stdout);
    let toml = std::fs::read_to_string(b.dir.path().join("models/acme/iron-9/model.toml")).unwrap();
    assert!(
        toml.contains("[heater]") && toml.contains("[heater.values]"),
        "{toml}"
    );
    let lua = std::fs::read_to_string(b.dir.path().join("models/acme/iron-9/driver.lua")).unwrap();
    assert!(lua.contains("function read(ctx)"), "{lua}");

    let out = b.run(&[
        "--json",
        "model",
        "new",
        "acme/x",
        "--capability",
        "Bad-Name",
    ]);
    assert_eq!(out.code_str(), "usage");
    let out = b.run(&[
        "--json",
        "model",
        "new",
        "acme/y",
        "--capability",
        "heater",
        "--max-voltage",
        "5",
    ]);
    assert_eq!(out.code_str(), "usage");
}

/// What the shell's completion script asks for: the words so far, the last one at the cursor.
fn complete(b: &Bench, shell: &str, words: &[&str]) -> Vec<String> {
    let dir = b.dir.path();
    let mut c = Command::new(env!("CARGO_BIN_EXE_desk"));
    c.env("DESK_COMPLETE", shell)
        .env("DESK_CONFIG", dir.join("desk.toml"))
        .env("DESK_MOCK_STATE", dir.join("mock-state.json"))
        .env("_CLAP_IFS", "\n")
        .env("_CLAP_COMPLETE_INDEX", words.len().to_string())
        .env("_CLAP_COMPLETE_COMP_TYPE", "9")
        .env("_CLAP_COMPLETE_SPACE", "true")
        .args(["--", "desk"])
        .args(words);
    let out = c.output().unwrap();
    assert!(out.status.success(), "{words:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.split('\t').next().unwrap_or(l).to_string())
        .filter(|l| !l.starts_with('-'))
        .collect()
}

#[test]
fn completions_come_from_the_config_and_models() {
    let b = Bench::with(RELAYS);
    b.file("relays.lua", RELAYS_DRIVER);
    let b2 = iron();
    for shell in ["bash", "fish"] {
        let c = |words: &[&str]| complete(&b2, shell, words);
        assert!(c(&["set", ""]).contains(&"iron".to_string()), "{shell}");
        assert_eq!(c(&["set", "ir"]), ["iron"]);
        assert_eq!(c(&["set", "iron", ""]), ["setpoint", "mode", "enabled"]);
        assert_eq!(c(&["set", "iron", "mo"]), ["mode"]);
        assert_eq!(c(&["set", "iron", "mode", ""]), ["normal", "boost"]);
        assert_eq!(c(&["set", "iron", "enabled", "o"]), ["on", "off"]);
        assert!(c(&["set", "iron", "setpoint", ""]).is_empty(), "a number");
        // A setting that's been given isn't offered again.
        assert_eq!(
            c(&["set", "iron", "mode", "boost", ""]),
            ["setpoint", "enabled"]
        );
        assert_eq!(
            c(&["get", "iron", ""]),
            ["setpoint", "mode", "enabled", "temperature", "last_action"]
        );
        assert_eq!(c(&["do", "iron", ""]), ["standby", "boost"]);
        assert_eq!(c(&["do", "iron", "boost", ""]), ["seconds="]);
        assert_eq!(c(&["--json", "power", "on", "ir"]), ["iron"]);
        assert_eq!(c(&["provider", "test", ""]), ["sim"]);
        assert_eq!(c(&["device", "show", "ir"]), ["iron"]);
        assert_eq!(c(&["skill", "desk-a"]), ["desk-add-device"]);
    }
    // An action's arguments and their choices, whole or as bash splits them.
    let picks = Bench::with(
        r#"
[providers.sim]
kind = "mock"
[devices.cam]
[devices.cam.shutter]
provider = "sim"
endpoint = "cam"
actions = { shoot = { switches = false, args = { mode = { type = "choice", choices = ["fast", "slow"] }, count = { type = "number" } } } }
"#,
    );
    assert_eq!(
        complete(&picks, "fish", &["do", "cam", "shoot", ""]),
        ["mode=", "count="]
    );
    assert_eq!(
        complete(&picks, "fish", &["do", "cam", "shoot", "mode=f"]),
        ["mode=fast"]
    );
    assert_eq!(
        complete(&picks, "bash", &["do", "cam", "shoot", "mode", "=", ""]),
        ["fast", "slow"]
    );
    assert_eq!(
        complete(&picks, "fish", &["do", "cam", "shoot", "mode=fast", ""]),
        ["count="]
    );
    // Channels, whole (one word) or as bash splits them.
    assert_eq!(
        complete(&b, "fish", &["set", "relays:"]),
        ["relays:ch1", "relays:ch2"]
    );
    assert_eq!(
        complete(&b, "bash", &["set", "relays", ":", ""]),
        ["ch1", "ch2"]
    );
    assert_eq!(
        complete(&b, "bash", &["set", "relays", ":", "ch1", "o"]),
        ["on"]
    );
}

#[test]
fn completions_scripts_and_a_broken_config_are_harmless() {
    let b = iron();
    for shell in ["bash", "zsh", "fish", "elvish", "powershell"] {
        let out = b.run(&["completions", shell]);
        assert_eq!(out.code, 0, "{shell}: {}", out.stderr);
        assert!(
            out.stdout.contains("DESK_COMPLETE"),
            "{shell}: {}",
            out.stdout
        );
    }
    let out = b.run(&["--json", "completions", "bash"]);
    assert_eq!(out.data()["shell"], "bash");
    assert!(b.run(&["completions", "nushell"]).code != 0);
    // No device or config needed for the script; Tab on a broken config offers nothing.
    b.file("desk.toml", "this is not toml");
    assert_eq!(b.run(&["completions", "bash"]).code, 0);
    assert!(complete(&b, "bash", &["set", "ir"]).is_empty());
}
