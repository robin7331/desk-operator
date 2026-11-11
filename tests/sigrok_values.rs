//! Readings and settings through sigrok, end to end against the fake `sigrok-cli`
//! (`tests/support/fake_sigrok_cli.rs`): `ctx.sigrok.measure`, `get` and `set` in the `read()`
//! and `set()` of declared capabilities, `desk get|set|watch` on them, the exact command lines,
//! the checks before sigrok runs, failures and timeouts. The last test uses the real
//! `sigrok-cli -d demo` when it is installed.
//!
//! Unix only for now, like `tests/sigrok.rs`.
#![cfg(unix)]

#[path = "support/sigrok.rs"]
mod sigrok;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sigrok::SigrokBench;

const DEVICES: &str = "demo - Demo device with 4 channels: P1 A0 A1 T1";

/// A multimeter whose reading changes its unit, and a fixed-unit value from another channel.
const DMM: &str = r#"
[devices.dmm.meter]
script = "meter.lua"
sigrok = { driver = "demo", backend = "fake" }
values = { reading = { type = "number", units = ["V", "Ω", "A"] }, overload = { type = "boolean" }, a0 = { type = "number", unit = "V" } }
"#;

const DMM_LUA: &str = r#"
PARAMS = {}
function read(ctx)
  local out = {}
  for _, r in ipairs(ctx.sigrok.measure{ channels = { "P1", "A0" } }) do
    if r.channel == "P1" then
      out.overload = r.value == nil
      if r.value then out.reading = { value = r.value, unit = r.unit } end
    elseif r.channel == "A0" then
      out.a0 = r.value
    end
  end
  return out
end
"#;

/// A supply on channel group CH1: settings read back with `get`, changed with `set`.
const PSU: &str = r#"
[devices.psu.supply]
script = "supply.lua"
sigrok = { driver = "demo", backend = "fake" }
params = { group = "CH1" }
settings = { voltage = { type = "number", unit = "V", min = 0, max = 30 }, output = { type = "boolean" } }
values = { measured = { type = "number", unit = "V" } }
"#;

const PSU_LUA: &str = r#"
local KEYS = { voltage = "voltage_target", output = "enabled" }
function read(ctx)
  local v = ctx.sigrok.get{ keys = { "voltage_target", "enabled", "voltage" }, group = ctx.params.group }
  return { voltage = v.voltage_target, output = v.enabled, measured = v.voltage }
end
function set(ctx, name, value)
  ctx.sigrok.set{ config = { [KEYS[name]] = value }, group = ctx.params.group }
end
"#;

const PSU_CONFIG: &str =
    "CH1/voltage_target=0.0\nCH1/enabled=false\nCH1/voltage=0.0\nCH2/voltage_target=1.0";

/// A probe: each action runs one host function with the request its `case` names and logs
/// what came back, so tests can check requests, command lines and results one by one.
const PROBE: &str = r#"
[devices.probe.lab]
script  = "probe.lua"
sigrok  = { driver = "demo", backend = "fake" }
timeout = "5s"

[devices.probe.lab.actions.measure]
switches = false
args = { case = { type = "string", default = "plain" } }
[devices.probe.lab.actions.get]
switches = false
args = { case = { type = "string", default = "plain" } }
[devices.probe.lab.actions.set]
switches = false
args = { case = { type = "string", default = "plain" } }
"#;

const PROBE_LUA: &str = r#"
PARAMS = {}
local CASES = {
  measure = {
    plain          = { channels = { "P1" } },
    samples        = { channels = { "P1" }, samples = 3 },
    full           = { channels = "A0, A1", time = "500ms", config = { amplitude = 2, pattern = "square" }, group = "A0", timeout = "2s" },
    short          = { channels = { "P1" }, timeout = "300ms" },
    long           = { channels = { "P1" }, timeout = "5s" },
    no_channels    = {},
    empty_channels = { channels = {} },
    channel_dash   = { channels = { "-o" } },
    channel_comma  = { channels = { "P1,P2" } },
    group_space    = { channels = { "P1" }, group = "A 0" },
    group_dash     = { channels = { "P1" }, group = "-g" },
    group_colon    = { channels = { "P1" }, group = "CH1:x" },
    group_table    = { channels = { "P1" }, group = {} },
    two_lengths    = { channels = { "P1" }, samples = 1, time = 100 },
    zero_samples   = { channels = { "P1" }, samples = 0 },
    frames         = { channels = { "P1" }, frames = 1 },
    output         = { channels = { "P1" }, format = "csv" },
    into           = { channels = { "P1" }, into = "/tmp/x" },
    config_colon   = { channels = { "P1" }, config = { amplitude = "1:samplerate=2" } },
    bad_timeout    = { channels = { "P1" }, timeout = "soon" },
  },
  get = {
    plain     = { keys = { "voltage_target", "enabled", "mode", "voltage" } },
    group     = { keys = { "voltage_target" }, group = "CH1", timeout = 2000 },
    unknown   = { keys = { "nosuch" } },
    nogroup   = { keys = { "voltage_target" }, group = "NOPE" },
    no_keys   = {},
    empty     = { keys = {} },
    twice     = { keys = { "enabled", "enabled" } },
    colon     = { keys = { "enabled:voltage" } },
    dash      = { keys = { "--x" } },
    upper     = { keys = { "Voltage" } },
    string    = { keys = "enabled" },
    config    = { keys = { "enabled" }, config = { enabled = true } },
    group_bad = { keys = { "enabled" }, group = "CH 1" },
  },
  set = {
    plain     = { config = { voltage_target = 5, enabled = true, mode = "cv" } },
    group     = { config = { voltage_target = 2.5 }, group = "CH1", timeout = "2s" },
    unknown   = { config = { nosuch = 1 } },
    no_config = {},
    empty     = { config = {} },
    colon     = { config = { mode = "a:b" } },
    equals    = { config = { mode = "a=b" } },
    lead      = { config = { mode = "-o" } },
    keys      = { config = { mode = "x" }, keys = { "mode" } },
    group_bad = { config = { mode = "x" }, group = "-CH1" },
  },
}

local function show(v)
  if math.type(v) then return math.type(v) .. ":" .. tostring(v) end
  return type(v) .. ":" .. tostring(v)
end

function action(ctx, name, args)
  local req = CASES[name][args.case]
  if req == nil then error("no case " .. args.case) end
  if name == "measure" then
    for _, r in ipairs(ctx.sigrok.measure(req)) do
      ctx.log(r.channel .. "|" .. show(r.value) .. "|" .. tostring(r.unit) .. "|"
        .. table.concat(r.flags, ",") .. "|" .. r.text)
    end
  elseif name == "get" then
    local v = ctx.sigrok.get(req)
    for _, k in ipairs(req.keys) do ctx.log(k .. "=" .. show(v[k])) end
  else
    local r = ctx.sigrok.set(req)
    ctx.log("set returned " .. tostring(r))
  end
end
"#;

fn bench(config: &str, files: &[(&str, &str)]) -> SigrokBench {
    let b = SigrokBench::new(config);
    for (name, text) in files {
        std::fs::write(b.path(name), text).unwrap();
    }
    b.fake.devices(DEVICES);
    b
}

fn dmm() -> SigrokBench {
    bench(DMM, &[("meter.lua", DMM_LUA)])
}

fn psu() -> SigrokBench {
    let b = bench(PSU, &[("supply.lua", PSU_LUA)]);
    b.fake.device_config(PSU_CONFIG);
    b
}

fn probe() -> SigrokBench {
    let b = bench(PROBE, &[("probe.lua", PROBE_LUA)]);
    b.fake
        .device_config(
            "voltage_target=4.98\nenabled=true\nmode='cv'\nvoltage=4.97\nCH1/voltage_target=3.3",
        )
        .groups("A0\nA1")
        .readings("P1: 3.3 kΩ AUTO");
    b
}

fn log(out: &sigrok::Out) -> Vec<String> {
    out.data()["log"]
        .as_array()
        .unwrap_or_else(|| panic!("no log: {}", out.stdout))
        .iter()
        .map(|l| l.as_str().unwrap().to_string())
        .collect()
}

fn device_free(fake: &sigrok::Fake) -> bool {
    match std::fs::File::options()
        .write(true)
        .open(fake.dir.join("busy.lock"))
    {
        Ok(f) => f.try_lock().is_ok(),
        Err(_) => true,
    }
}

// ---------------------------------------------------------------------------------------------
// A meter: read() with measure

#[test]
fn a_meter_reads_through_measure_with_its_units() {
    let b = dmm();
    b.fake
        .readings("libsigrok 0.5.2-fake\nP1: 4.98 V DC AUTO\nA0: 250.0 mV DC\nD0:100");
    let out = b.run(&["--json", "get", "dmm"]);
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    let v = &out.data()["values"];
    assert_eq!(v["reading"], json!({ "value": 4.98, "unit": "V" }), "{v}");
    assert_eq!(v["a0"], 0.25, "mV in the base unit: {v}");
    assert_eq!(v["overload"], false);
    assert_eq!(
        b.fake.measures(),
        [["-d", "demo", "--channels", "P1,A0", "--samples", "1"]]
    );

    // The unit follows what sigrok reports, prefix and all.
    b.fake.readings("P1: 1.5 kΩ AUTO\nA0: -1.2 V DC");
    let v = b.run(&["--json", "get", "dmm"]).data()["values"].clone();
    assert_eq!(v["reading"], json!({ "value": 1500.0, "unit": "Ω" }), "{v}");
    assert_eq!(v["a0"], -1.2);
    b.fake.readings("P1: 12.5 mA DC");
    let human = b.run(&["get", "dmm"]).stdout;
    assert!(human.contains("0.0125 A"), "{human}");

    // An overload has no value; a channel that isn't printed is simply absent.
    b.fake.readings("P1: inf Ω");
    let v = b.run(&["--json", "get", "dmm"]).data()["values"].clone();
    assert_eq!(v["overload"], true, "{v}");
    assert!(v.get("reading").is_none() && v.get("a0").is_none(), "{v}");

    // A unit the declaration doesn't list is the driver's mistake, reported as such.
    b.fake.readings("P1: 3 Hz");
    let out = b.run(&["--json", "get", "dmm"]);
    assert_eq!(out.code_str(), "script_failed", "{}", out.stdout);
    assert!(out.message().contains("Hz"), "{}", out.message());
}

#[test]
fn a_meter_without_readings_is_unavailable() {
    let b = dmm();
    b.fake
        .readings("libsigrok 0.5.2-fake\nAcquisition with 2/4 channels");
    let out = b.run(&["--json", "get", "dmm"]);
    assert_eq!(out.code_str(), "endpoint_unavailable", "{}", out.stdout);
    let msg = out.message();
    assert!(
        msg.contains("no readings") && msg.contains("P1,A0"),
        "{msg}"
    );
    assert!(
        msg.contains("Acquisition with"),
        "shows what was printed: {msg}"
    );
}

#[test]
fn watch_streams_a_meter_through_the_daemon() {
    let b = dmm();
    b.fake.readings("P1: 4.98 V DC\nA0: 1.0 V DC");
    let out = b.run(&[
        "--json",
        "watch",
        "dmm",
        "--interval",
        "20ms",
        "--count",
        "3",
    ]);
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    let lines: Vec<Value> = out
        .stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 3, "{}", out.stdout);
    for l in &lines {
        assert_eq!(l["ok"], true, "{l}");
        assert_eq!(l["data"]["values"]["reading"]["value"], 4.98, "{l}");
    }
    assert!(b.fake.measures().len() >= 3);
    // The watch went through the daemon (it started one).
    let sockets = std::fs::read_dir(b.path("run")).map_or(0, |e| {
        e.flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "sock"))
            .count()
    });
    assert!(sockets <= 1);
    assert!(b.path("run").exists(), "the daemon's runtime folder");
}

// ---------------------------------------------------------------------------------------------
// A supply: read() with get, set() with set

#[test]
fn a_supply_is_set_and_confirmed_by_its_read_back() {
    let b = psu();
    let out = b.run(&["--json", "get", "psu"]);
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    let v = &out.data()["values"];
    assert_eq!(v["voltage"], 0.0, "{v}");
    assert_eq!(v["output"], false);
    assert_eq!(v["measured"], 0.0);
    assert_eq!(
        b.fake.gets(),
        [[
            "-d",
            "demo",
            "-g",
            "CH1",
            "--get",
            "voltage_target",
            "--get",
            "enabled",
            "--get",
            "voltage"
        ]]
    );

    let out = b.run(&["--json", "set", "psu", "voltage", "5v", "output", "on"]);
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    let d = out.data();
    assert_eq!(d["verified"]["voltage"], true, "{d}");
    assert_eq!(d["verified"]["output"], true, "{d}");
    assert_eq!(d["values"]["voltage"], 5.0);
    assert_eq!(d["values"]["output"], true);
    let sets = b.fake.sets();
    assert_eq!(
        sets,
        [
            [
                "-d",
                "demo",
                "-g",
                "CH1",
                "--config",
                "voltage_target=5",
                "--set"
            ],
            [
                "-d",
                "demo",
                "-g",
                "CH1",
                "--config",
                "enabled=true",
                "--set"
            ],
        ]
    );
    assert!(
        b.fake.kept().contains("CH1/voltage_target=5"),
        "{}",
        b.fake.kept()
    );

    // Kept by the device: a later command reads it back.
    let v = b.run(&["--json", "get", "psu"]).data()["values"].clone();
    assert_eq!(v["voltage"], 5.0);
    // desk's own checks come first: nothing reaches sigrok.
    let before = b.fake.calls().len();
    let out = b.run(&["--json", "set", "psu", "voltage", "31v"]);
    assert_eq!(out.code_str(), "limit_exceeded", "{}", out.stdout);
    assert_eq!(b.fake.calls().len(), before);
}

#[test]
fn a_supply_that_forgets_its_settings_is_not_confirmed() {
    let b = psu();
    b.fake.behaviour("forget=1");
    let out = b.run(&["--json", "set", "psu", "voltage", "5v"]);
    // desk's generic rule: the read-back differs from the request.
    assert_eq!(out.code_str(), "verify_failed", "{}", out.stdout);
    assert!(out.message().contains("voltage"), "{}", out.message());
    assert_eq!(b.fake.sets().len(), 1, "it was sent");
    assert_eq!(b.fake.kept(), "");
}

// ---------------------------------------------------------------------------------------------
// Exact command lines and results

#[test]
fn measure_runs_the_exact_command_line_and_returns_every_reading() {
    let b = probe();
    b.fake
        .readings("P1: 3.3 kΩ AUTO\nP1: OL kΩ\nD0:1010\nP1: 42 DC");
    let out = b.run(&["--json", "do", "probe", "measure"]);
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    assert_eq!(
        log(&out),
        [
            "P1|float:3300.0|Ω|AUTO|3.3 kΩ AUTO",
            "P1|nil:nil|Ω||OL kΩ",
            "P1|float:42.0|nil|DC|42 DC",
        ]
    );
    assert_eq!(
        b.fake.measures(),
        [["-d", "demo", "--channels", "P1", "--samples", "1"]]
    );

    let out = b.run(&["--json", "do", "probe", "measure", "case=full"]);
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    assert_eq!(
        b.fake.measures()[1],
        [
            "-d",
            "demo",
            "-g",
            "A0",
            "--config",
            "amplitude=2:pattern=square",
            "--channels",
            "A0,A1",
            "--time",
            "500"
        ]
    );

    // One line per sample and channel, in the order sigrok printed them.
    b.fake
        .behaviour("per_channel=1")
        .readings("P1: 1.0 V DC\nA0: 2.0 V DC");
    let out = b.run(&["--json", "do", "probe", "measure", "case=samples"]);
    assert_eq!(log(&out).len(), 3, "{:?}", log(&out));
    assert_eq!(b.fake.measures()[2][4..], ["--samples", "3"]);
}

#[test]
fn get_reads_keys_in_order_and_types_them() {
    let b = probe();
    let out = b.run(&["--json", "do", "probe", "get"]);
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    // Repeated --get flags keep the order (the fake mixes up `--get a:b`, like sigrok).
    assert_eq!(
        log(&out),
        [
            "voltage_target=float:4.98",
            "enabled=boolean:true",
            "mode=string:cv",
            "voltage=float:4.97"
        ]
    );
    assert_eq!(
        b.fake.gets(),
        [[
            "-d",
            "demo",
            "--get",
            "voltage_target",
            "--get",
            "enabled",
            "--get",
            "mode",
            "--get",
            "voltage"
        ]]
    );
    let out = b.run(&["--json", "do", "probe", "get", "case=group"]);
    assert_eq!(log(&out), ["voltage_target=float:3.3"]);
    assert_eq!(
        b.fake.gets()[1],
        ["-d", "demo", "-g", "CH1", "--get", "voltage_target"]
    );

    // With the unit's conn.
    b.config(&PROBE.replace(
        "backend = \"fake\"",
        "conn = \"/dev/ttyUSB0:serialcomm=9600/8n1\", backend = \"fake\"",
    ));
    let out = b.run(&["--json", "do", "probe", "get", "case=group"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(
        b.fake.gets()[2][..2],
        ["-d", "demo:conn=/dev/ttyUSB0:serialcomm=9600/8n1"]
    );
}

#[test]
fn set_runs_the_exact_command_line() {
    let b = probe();
    let out = b.run(&["--json", "do", "probe", "set"]);
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    assert_eq!(log(&out), ["set returned nil"]);
    let out = b.run(&["--json", "do", "probe", "set", "case=group"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(
        b.fake.sets(),
        [
            vec![
                "-d",
                "demo",
                "--config",
                "enabled=true:mode=cv:voltage_target=5",
                "--set"
            ],
            vec![
                "-d",
                "demo",
                "-g",
                "CH1",
                "--config",
                "voltage_target=2.5",
                "--set"
            ],
        ]
    );
    // The fake device keeps it: get reads it back.
    let out = b.run(&["--json", "do", "probe", "get"]);
    assert!(log(&out).contains(&"mode=string:cv".to_string()));
    assert!(log(&out).contains(&"voltage_target=integer:5".to_string()));
}

#[test]
fn hostile_or_broken_requests_never_reach_sigrok() {
    let b = probe();
    for (f, cases) in [
        (
            "measure",
            &[
                "no_channels",
                "empty_channels",
                "channel_dash",
                "channel_comma",
                "group_space",
                "group_dash",
                "group_colon",
                "group_table",
                "two_lengths",
                "zero_samples",
                "frames",
                "output",
                "into",
                "config_colon",
                "bad_timeout",
            ][..],
        ),
        (
            "get",
            &[
                "no_keys",
                "empty",
                "twice",
                "colon",
                "dash",
                "upper",
                "string",
                "config",
                "group_bad",
            ][..],
        ),
        (
            "set",
            &[
                "no_config",
                "empty",
                "colon",
                "equals",
                "lead",
                "keys",
                "group_bad",
            ][..],
        ),
    ] {
        for case in cases {
            let arg = format!("case={case}");
            let out = b.run(&["--json", "do", "probe", f, &arg]);
            assert_eq!(
                out.code_str(),
                "script_failed",
                "{f} {case}: {}",
                out.stdout
            );
            assert!(
                out.message().contains(&format!("ctx.sigrok.{f}")),
                "{f} {case}: {}",
                out.message()
            );
        }
    }
    assert!(b.fake.calls().is_empty(), "{:?}", b.fake.calls());
}

// ---------------------------------------------------------------------------------------------
// Failures

#[test]
fn sigroks_refusals_carry_its_text() {
    let b = probe();
    let out = b.run(&["--json", "do", "probe", "get", "case=unknown"]);
    assert_eq!(out.code_str(), "script_failed", "{}", out.stdout);
    assert!(
        out.message().contains("Unknown option 'nosuch'"),
        "{}",
        out.message()
    );

    let out = b.run(&["--json", "do", "probe", "get", "case=nogroup"]);
    assert_eq!(out.code_str(), "script_failed", "{}", out.stdout);
    assert!(
        out.message().contains("Invalid channel group 'NOPE'"),
        "{}",
        out.message()
    );

    let out = b.run(&["--json", "do", "probe", "set", "case=unknown"]);
    assert_eq!(out.code_str(), "script_failed", "{}", out.stdout);
    assert!(
        out.message().contains("Unknown device option 'nosuch'."),
        "{}",
        out.message()
    );
}

#[test]
fn a_wrong_line_count_from_get_is_an_error_that_shows_the_output() {
    let b = probe();
    b.fake.behaviour("get_extra=surprise");
    let out = b.run(&["--json", "do", "probe", "get"]);
    assert_eq!(out.code_str(), "script_failed", "{}", out.stdout);
    let msg = out.message();
    assert!(
        msg.contains("4 key(s)") && msg.contains("5 line(s)") && msg.contains("surprise"),
        "{msg}"
    );
}

#[test]
fn busy_and_missing_devices_are_unavailable() {
    let b = probe();
    b.fake.behaviour("query=busy");
    for f in ["measure", "get", "set"] {
        let out = b.run(&["--json", "do", "probe", f]);
        assert_eq!(
            out.code_str(),
            "endpoint_unavailable",
            "{f}: {}",
            out.stdout
        );
        assert!(out.message().contains("busy"), "{}", out.message());
    }
    b.fake.behaviour("query=ok").devices("");
    for f in ["measure", "get", "set"] {
        let out = b.run(&["--json", "do", "probe", f]);
        assert_eq!(
            out.code_str(),
            "endpoint_unavailable",
            "{f}: {}",
            out.stdout
        );
        assert!(
            out.message().contains("found no such device"),
            "{}",
            out.message()
        );
    }
    b.fake.devices(DEVICES).readings("nothing to see");
    let out = b.run(&["--json", "do", "probe", "measure"]);
    assert_eq!(out.code_str(), "endpoint_unavailable", "{}", out.stdout);
    assert!(out.message().contains("no readings"), "{}", out.message());
}

#[test]
fn a_slow_device_times_out_and_sigrok_is_stopped() {
    let b = probe();
    b.fake.behaviour("slow=5000");
    let started = Instant::now();
    let out = b.run(&["--json", "do", "probe", "measure", "case=short"]);
    assert_eq!(out.code_str(), "timeout", "{}", out.stdout);
    assert!(out.message().contains("`probe`"), "{}", out.message());
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(device_free(&b.fake), "sigrok was stopped");

    // Without a timeout of their own: the backend's query timeout.
    b.config(&format!(
        "[providers.fake]\nkind = \"sigrok\"\nprogram = {:?}\ntimeout = \"300ms\"\n{PROBE}",
        sigrok::program().display().to_string()
    ));
    let started = Instant::now();
    let out = b.run(&["--json", "do", "probe", "get"]);
    assert_eq!(out.code_str(), "timeout", "{}", out.stdout);
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(device_free(&b.fake));

    // They don't suspend the call's own timeout (unlike a capture).
    b.config(&PROBE.replace("timeout = \"5s\"", "timeout = \"300ms\""));
    let started = Instant::now();
    let out = b.run(&["--json", "do", "probe", "measure", "case=long"]);
    assert_eq!(out.code_str(), "timeout", "{}", out.stdout);
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(device_free(&b.fake));
}

#[test]
fn a_missing_program_and_a_missing_binding() {
    let b = probe();
    b.config(&format!(
        "[providers.fake]\nkind = \"sigrok\"\nprogram = \"/nonexistent/sigrok-cli\"\n{PROBE}"
    ));
    for f in ["measure", "get", "set"] {
        let out = b.run(&["--json", "do", "probe", f]);
        assert_eq!(
            out.code_str(),
            "provider_unreachable",
            "{f}: {}",
            out.stdout
        );
        assert!(out.message().contains("/nonexistent/sigrok-cli"));
    }
    b.config(&PROBE.replace("sigrok  = { driver = \"demo\", backend = \"fake\" }\n", ""));
    for f in ["measure", "get", "set"] {
        let out = b.run(&["--json", "do", "probe", f]);
        assert_eq!(out.code_str(), "config_invalid", "{f}: {}", out.stdout);
        assert!(
            out.message()
                .contains(&format!("ctx.sigrok.{f}: this binding has no `sigrok = {{")),
            "{}",
            out.message()
        );
    }
}

// ---------------------------------------------------------------------------------------------
// The real thing, where it is installed

fn real_sigrok() -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|d| d.join("sigrok-cli"))
        .find(|p| p.is_file())
}

#[test]
fn the_example_meter_reads_the_real_demo_driver() {
    let Some(real) = real_sigrok() else {
        eprintln!("skipped: sigrok-cli is not installed");
        return;
    };
    let dir = tempfile::TempDir::new().unwrap();
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/sigrok/desk.toml");
    let out = Command::new(env!("CARGO_BIN_EXE_desk"))
        .env_remove("DESK_CONFIG")
        .env("PATH", real.parent().unwrap())
        .env("DESK_RUNTIME_DIR", dir.path().join("run"))
        .arg("--config")
        .arg(&example)
        .args(["--json", "get", "dmm"])
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v["ok"],
        true,
        "{v}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let values = &v["data"]["values"];
    // A0 is a square wave of amplitude 10 V by default; `amplitude` comes from `-g A0 --get`.
    assert_eq!(values["amplitude"], 10.0, "{values}");
    let a0 = values["a0"].as_f64().unwrap();
    assert!((a0.abs() - 10.0).abs() < 1e-9, "{values}");
    assert!(values["a1"].as_f64().is_some(), "{values}");
}
