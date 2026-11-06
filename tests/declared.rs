//! End-to-end tests of open capabilities: devices that declare their own values, settings and
//! actions, driven by the simulated device (mock provider) or by Lua drivers. The core's generic
//! rules: units, ranges, limits before anything is sent, declaration order, verification, dry runs,
//! channels, `watch`, and the definition format.

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
heater = { provider = "sim", endpoint = "fail_iron", values = { temperature = { type = "number", unit = "°C" } } }

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
        .env_remove("DESK_DEVICES")
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
        (vec!["set", "iron", "setpoint"], "VALUE"),
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
        "settings:     setpoint (°C, 50…450, limit 380), mode (normal|boost), enabled (on|off)",
        "values:       temperature (°C), last_action",
        "actions:      standby, boost (seconds)",
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
    let out = Command::new("timeout")
        .args(["1.5", env!("CARGO_BIN_EXE_desk")])
        .env("DESK_MOCK_STATE", b.dir.path().join("mock-state.json"))
        .env("DESK_RUNTIME_DIR", b.dir.path().join("run"))
        .env("DESK_DAEMON_IDLE", "1s")
        .arg("--config")
        .arg(b.dir.path().join("desk.toml"))
        .args(["--json", "watch", "iron", "--interval", "20ms", "--changes"])
        .output()
        .unwrap();
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

// ---- definitions ------------------------------------------------------------------------------

#[test]
fn definitions_declare_capabilities_and_devices_use_them() {
    let b = Bench::with(
        r#"
[devices.dmm]
model = "acme/dmm-1"
meter = { params = { range_default = 10 } }
limits = { meter = { range = 100 } }
"#,
    );
    b.file(
        "devices/acme/dmm-1/device.toml",
        r#"
[device]
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
        "devices/acme/dmm-1/meter.lua",
        &METER_DRIVER.replace(
            "PARAMS = {}",
            "PARAMS = { range_default = { type = \"number\", doc = \"x\" } }",
        ),
    );
    let out = b.run(&["--json", "library", "check", "acme/dmm-1"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let out = b.run(&["--json", "get", "dmm"]);
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    let out = b.run(&["--json", "set", "dmm", "range", "200v"]);
    assert_eq!(
        out.code_str(),
        "limit_exceeded",
        "the device's limit applies"
    );
    let out = b.run(&["--json", "library", "show", "acme/dmm-1"]);
    assert!(
        out.data()["driver_params"]["meter"].is_array(),
        "{}",
        out.stdout
    );
}

#[test]
fn library_new_scaffolds_a_declared_capability() {
    let b = Bench::with("");
    let out = b.run(&[
        "--json",
        "library",
        "new",
        "acme/iron-9",
        "--capability",
        "heater",
        "--name",
        "ACME Iron 9",
    ]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.data()["check"]["ok"], true, "{}", out.stdout);
    let toml =
        std::fs::read_to_string(b.dir.path().join("devices/acme/iron-9/device.toml")).unwrap();
    assert!(
        toml.contains("[heater]") && toml.contains("[heater.values]"),
        "{toml}"
    );
    let lua = std::fs::read_to_string(b.dir.path().join("devices/acme/iron-9/driver.lua")).unwrap();
    assert!(lua.contains("function read(ctx)"), "{lua}");

    let out = b.run(&[
        "--json",
        "library",
        "new",
        "acme/x",
        "--capability",
        "Bad-Name",
    ]);
    assert_eq!(out.code_str(), "usage");
    let out = b.run(&[
        "--json",
        "library",
        "new",
        "acme/y",
        "--capability",
        "heater",
        "--max-voltage",
        "5",
    ]);
    assert_eq!(out.code_str(), "usage");
}
