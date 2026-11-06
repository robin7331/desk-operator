//! End-to-end tests of Lua script power bindings, against the mock provider.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

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
    fn error_code(&self) -> String {
        self.json()["error"]["code"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }
    fn error_message(&self) -> String {
        self.json()["error"]["message"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }
}

/// A bench with one scripted device `dev` running `script` (plus extra binding keys), and mock
/// outlets the script can drive.
fn bench(script: &str, binding_extra: &str) -> Bench {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("dev.lua"), script).unwrap();
    std::fs::write(
        dir.path().join("desk.toml"),
        format!(
            r#"
[providers.sim]
kind = "mock"

[devices.dev]
name  = "Scripted device"
power = {{ script = "dev.lua"{binding_extra} }}

[devices.outlet]
power = {{ provider = "sim", endpoint = "outlet" }}
"#
        ),
    )
    .unwrap();
    Bench { dir }
}

impl Bench {
    fn run(&self, args: &[&str]) -> Out {
        run_in(self.dir.path(), &self.dir.path().join("desk.toml"), args)
    }
}

fn run_in(state_dir: &Path, config: &Path, args: &[&str]) -> Out {
    let out = Command::new(env!("CARGO_BIN_EXE_desk"))
        .env_remove("DESK_CONFIG")
        .env("DESK_MOCK_STATE", state_dir.join("mock-state.json"))
        .arg("--config")
        .arg(config)
        .args(args)
        .output()
        .unwrap();
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// Drives the `outlet` mock endpoint, with readback through state().
const OUTLET_SCRIPT: &str = r#"
local function outlet(ctx) return ctx.power{ provider = "sim", endpoint = "outlet" } end
function on(ctx)    outlet(ctx):on();  ctx.log("on called") end
function off(ctx)   outlet(ctx):off(); ctx.log("off called") end
function state(ctx) return outlet(ctx):state() end
"#;

#[test]
fn script_switches_through_a_provider_with_readback() {
    let b = bench(OUTLET_SCRIPT, "");
    let out = b.run(&["--json", "power", "on", "dev"]);
    assert_eq!(out.code, 0, "{}\n{}", out.stdout, out.stderr);
    let r = &out.json()["data"]["results"][0];
    assert_eq!(r["power"]["state"], "on");
    assert_eq!(r["power"]["confidence"], "real");
    assert_eq!(r["before"]["state"], "off");
    assert_eq!(r["log"][0], "on called");
    assert!(out.stderr.contains("dev: on called"), "{}", out.stderr);

    // The script really switched the underlying outlet.
    let j = b.run(&["--json", "power", "status", "outlet"]).json();
    assert_eq!(j["data"]["devices"][0]["power"]["state"], "on");

    let j = b.run(&["--json", "power", "toggle", "dev"]).json();
    assert_eq!(j["data"]["results"][0]["power"]["state"], "off");
}

#[test]
fn script_with_state_is_idempotent() {
    let b = bench(OUTLET_SCRIPT, "");
    b.run(&["power", "on", "dev"]);
    let j = b.run(&["--json", "power", "on", "dev"]).json();
    let r = &j["data"]["results"][0];
    assert_eq!(r["changed"], false);
    assert!(r.get("log").is_none(), "on() must not run again: {r}");
}

#[test]
fn script_without_state_is_assumed() {
    let b = bench(
        r#"
        function on(ctx) end
        function off(ctx) end
        "#,
        "",
    );
    let j = b.run(&["--json", "power", "on", "dev"]).json();
    assert_eq!(j["data"]["results"][0]["power"]["confidence"], "assumed");
    let j = b.run(&["--json", "power", "status", "dev"]).json();
    assert_eq!(j["data"]["devices"][0]["power"]["state"], "unknown");
    let out = b.run(&["--json", "power", "toggle", "dev"]);
    assert_eq!(out.code, 4);
    assert_eq!(out.error_code(), "state_unknown");
}

#[test]
fn ctx_exposes_device_and_params() {
    let b = bench(
        r#"
        function on(ctx)
          ctx.log(ctx.device.id, ctx.device.name, ctx.params.level, ctx.params.nested.x)
        end
        function off(ctx) end
        "#,
        r#", params = { level = 5, nested = { x = "deep" } }"#,
    );
    let j = b.run(&["--json", "power", "on", "dev"]).json();
    assert_eq!(
        j["data"]["results"][0]["log"][0],
        "dev Scripted device 5 deep"
    );
}

#[test]
fn script_errors_are_script_failed_with_location() {
    let b = bench(
        r#"
        function on(ctx)
          error("PSU did not answer *IDN?")
        end
        function off(ctx) end
        "#,
        "",
    );
    let out = b.run(&["--json", "power", "on", "dev"]);
    assert_eq!(out.code, 5);
    assert_eq!(out.error_code(), "script_failed");
    let msg = out.error_message();
    assert!(msg.contains("PSU did not answer *IDN?"), "{msg}");
    assert!(
        msg.contains("dev.lua:3"),
        "message names file and line: {msg}"
    );
    assert!(!msg.contains("stack traceback"), "{msg}");
}

#[test]
fn logs_survive_failures() {
    let b = bench(
        r#"
        function on(ctx) ctx.log("step 1 done"); error("step 2 failed") end
        function off(ctx) end
        "#,
        "",
    );
    let j = b.run(&["--json", "power", "on", "dev"]).json();
    assert_eq!(j["data"]["results"][0]["log"][0], "step 1 done");
}

#[test]
fn host_errors_keep_their_code() {
    let b = bench(
        r#"
        function on(ctx) ctx.power{ provider = "sim", endpoint = "fail_x" }:on() end
        function off(ctx) end
        "#,
        "",
    );
    let out = b.run(&["--json", "power", "on", "dev"]);
    assert_eq!(out.code, 4);
    assert_eq!(out.error_code(), "endpoint_unavailable");
}

#[test]
fn scripts_can_catch_host_errors() {
    let b = bench(
        r#"
        function on(ctx)
          local ok, err = pcall(function()
            ctx.power{ provider = "sim", endpoint = "fail_x" }:on()
          end)
          ctx.log("caught:", ok, tostring(err):find("unavailable") ~= nil)
        end
        function off(ctx) end
        "#,
        "",
    );
    let out = b.run(&["--json", "power", "on", "dev"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(
        out.json()["data"]["results"][0]["log"][0],
        "caught: false true"
    );
}

#[test]
fn raw_set_sends_every_time() {
    let b = bench(
        r#"
        function on(ctx)
          local p = ctx.power{ provider = "sim", endpoint = "outlet" }
          p:set(true); ctx.sleep(10); p.set(true)
          ctx.log(p:state())
        end
        function off(ctx) end
        "#,
        "",
    );
    let out = b.run(&["--json", "power", "on", "dev"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.json()["data"]["results"][0]["log"][0], "on");
}

#[test]
fn busy_loop_hits_the_timeout() {
    let b = bench(
        r#"
        function on(ctx) while true do end end
        function off(ctx) end
        "#,
        r#", timeout = "300ms""#,
    );
    let start = Instant::now();
    let out = b.run(&["--json", "power", "on", "dev"]);
    assert_eq!(out.code, 6, "{}", out.stdout);
    assert_eq!(out.error_code(), "timeout");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn busy_loop_at_top_level_hits_the_timeout() {
    let b = bench("while true do end", r#", timeout = "300ms""#);
    let out = b.run(&["--json", "power", "on", "dev"]);
    assert_eq!(out.code, 6, "{}", out.stdout);
}

#[test]
fn long_sleep_hits_the_timeout() {
    let b = bench(
        r#"
        function on(ctx) ctx.sleep("10s") end
        function off(ctx) end
        "#,
        r#", timeout = "200ms""#,
    );
    let start = Instant::now();
    let out = b.run(&["--json", "power", "on", "dev"]);
    assert_eq!(out.code, 6);
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn sandbox_has_no_io_os_or_modules() {
    let b = bench(
        r#"
        function on(ctx)
          ctx.log(type(io), type(os), type(require), type(package), type(dofile),
                  type(loadfile), type(debug), type(string), type(math), type(table))
          print("print goes to the log, not stdout")
        end
        function off(ctx) end
        "#,
        "",
    );
    let out = b.run(&["--json", "power", "on", "dev"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let log = &out.json()["data"]["results"][0]["log"];
    assert_eq!(log[0], "nil nil nil nil nil nil nil table table table");
    assert_eq!(log[1], "print goes to the log, not stdout");
}

#[test]
fn bad_scripts_are_reported() {
    let syntax = bench("function on(ctx) oops(", "");
    let out = syntax.run(&["--json", "power", "on", "dev"]);
    assert_eq!(out.code, 5);
    assert!(
        out.error_message().contains("dev.lua"),
        "{}",
        out.error_message()
    );

    let missing_off = bench("function on(ctx) end", "");
    let out = missing_off.run(&["--json", "power", "on", "dev"]);
    assert_eq!(out.code, 3);
    assert!(
        out.error_message().contains("`off(ctx"),
        "{}",
        out.error_message()
    );

    let bad_state = bench(
        r#"
        function on(ctx) end
        function off(ctx) end
        function state(ctx) return 42 end
        "#,
        "",
    );
    let out = bad_state.run(&["--json", "power", "status", "dev"]);
    assert_eq!(out.code, 5);
    assert!(out.error_message().contains("state() must return"));

    let nested_script = bench(
        r#"
        function on(ctx) ctx.power{ script = "other.lua" }:on() end
        function off(ctx) end
        "#,
        "",
    );
    let out = nested_script.run(&["--json", "power", "on", "dev"]);
    assert_eq!(out.code, 3);
    assert!(out.error_message().contains("cannot call another script"));
}

#[test]
fn missing_script_file() {
    let b = bench("", "");
    std::fs::remove_file(b.dir.path().join("dev.lua")).unwrap();
    let out = b.run(&["--json", "power", "on", "dev"]);
    assert_eq!(out.code, 3);
    assert!(out.error_message().contains("cannot read script"));
}

#[test]
fn example_scripts_work() {
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/desk.toml");
    let dir = TempDir::new().unwrap();

    let out = run_in(dir.path(), &example, &["--json", "power", "on", "station"]);
    assert_eq!(out.code, 0, "{}\n{}", out.stdout, out.stderr);
    let j = run_in(
        dir.path(),
        &example,
        &["--json", "power", "status", "station"],
    )
    .json();
    assert_eq!(j["data"]["devices"][0]["power"]["state"], "on");
    assert_eq!(j["data"]["devices"][0]["power"]["confidence"], "real");
    let j = run_in(dir.path(), &example, &["--json", "power", "status", "iron"]).json();
    assert_eq!(
        j["data"]["devices"][0]["power"]["state"], "off",
        "the station has its own outlets"
    );
    let out = run_in(dir.path(), &example, &["--json", "power", "off", "station"]);
    assert_eq!(out.code, 0, "{}", out.stdout);

    let out = run_in(dir.path(), &example, &["--json", "power", "on", "lamp"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(
        out.json()["data"]["results"][0]["power"]["confidence"],
        "assumed"
    );
}

// ---- voltage_source drivers -------------------------------------------------------------------

/// A bench with one device `psu` whose `source` is the Lua driver `driver`.
fn source_bench(driver: &str, binding_extra: &str, limits: &str) -> Bench {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("driver.lua"), driver).unwrap();
    std::fs::write(
        dir.path().join("desk.toml"),
        format!(
            r#"
[providers.sim]
kind = "mock"

[devices.psu]
source = {{ script = "driver.lua"{binding_extra} }}
{limits}
"#
        ),
    )
    .unwrap();
    Bench { dir }
}

/// A complete driver for a PSU simulated inside the script (state lives for one command).
const SIM_DRIVER: &str = r#"
local s = { v = 0, a = 0, out = false }
function open(ctx)  ctx.log("open") end
function close(ctx) ctx.log("close") end
function info(ctx)  return { max_voltage = ctx.params.max or 30, max_current = 5 } end
function set_voltage(ctx, v) ctx.log("set_voltage", v); s.v = v end
function set_current(ctx, a) ctx.log("set_current", a); s.a = a end
function set_output(ctx, on) ctx.log("set_output", on); s.out = on end
function read(ctx)
  local v = s.out and s.v or 0
  return { output = s.out, voltage_set = s.v, current_set = s.a, voltage = v, current = 0, power = 0 }
end
"#;

#[test]
fn lua_source_driver_runs_in_one_session_in_order() {
    let b = source_bench(SIM_DRIVER, "", "");
    let out = b.run(&[
        "--json", "set", "psu", "voltage", "5v", "current", "1a", "output", "on",
    ]);
    assert_eq!(out.code, 0, "{}\n{}", out.stdout, out.stderr);
    let d = &out.json()["data"];
    assert_eq!(d["verified"]["voltage"], true);
    assert_eq!(d["verified"]["output"], true);
    assert_eq!(d["values"]["measured_voltage"], 5.0);
    let log: Vec<&str> = d["log"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap())
        .collect();
    assert_eq!(
        log,
        [
            "open",
            "set_voltage 5.0",
            "set_current 1.0",
            "set_output true",
            "close"
        ],
        "one session; setpoints before output"
    );
}

#[test]
fn lua_driver_limits_come_from_info_and_bench() {
    let b = source_bench(
        SIM_DRIVER,
        r#", params = { max = 12 }"#,
        "limits = { source = { current = 2 } }",
    );
    let out = b.run(&["--json", "set", "psu", "voltage", "13v"]);
    assert_eq!(out.code, 8);
    assert!(
        out.error_message().contains("device maximum"),
        "{}",
        out.error_message()
    );
    let out = b.run(&["--json", "set", "psu", "current", "3a"]);
    assert_eq!(out.code, 8);
    assert!(out.error_message().contains("bench limit"));
    // Nothing was sent: the driver only saw the session.
    let log = &out.json()["data"]["log"];
    assert_eq!(log, &serde_json::json!(["open", "close"]), "{}", out.stdout);
}

#[test]
fn lua_driver_session_closes_after_errors() {
    let driver = SIM_DRIVER.replace(
        "function set_output(ctx, on)",
        "function set_output(ctx, on) error('relay stuck') end\nfunction unused(ctx, on)",
    );
    let b = source_bench(&driver, "", "");
    let out = b.run(&["--json", "set", "psu", "voltage", "5v", "output", "on"]);
    assert_eq!(out.code, 5, "{}", out.stdout);
    assert!(out.error_message().contains("relay stuck"));
    assert!(
        out.stderr.contains("psu: close"),
        "close() ran: {}",
        out.stderr
    );
}

#[test]
fn lua_driver_session_closes_after_timeout() {
    let driver = SIM_DRIVER.replace(
        "function read(ctx)",
        "function read(ctx) ctx.sleep(5000) end\nfunction unused(ctx)",
    );
    let b = source_bench(&driver, r#", timeout = "300ms""#, "");
    let out = b.run(&["--json", "get", "psu"]);
    assert_eq!(out.code, 6, "{}", out.stdout);
    assert!(
        out.stderr.contains("psu: close"),
        "close() ran: {}",
        out.stderr
    );
}

#[test]
fn minimal_driver_without_readback_is_unverified() {
    let b = source_bench(
        r#"
        function set_voltage(ctx, v) end
        function set_current(ctx, a) end
        function set_output(ctx, on) end
        function read(ctx) return { voltage = 4.98 } end
        "#,
        "",
        "",
    );
    let out = b.run(&["--json", "set", "psu", "voltage", "5v", "output", "on"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let d = &out.json()["data"];
    assert!(
        d["verified"].as_object().is_none_or(|v| v.is_empty()),
        "{}",
        d["verified"]
    );
    assert_eq!(d["values"]["measured_voltage"], 4.98);
}

#[test]
fn bad_drivers_are_reported() {
    let missing = source_bench("function read(ctx) return {} end", "", "");
    let out = missing.run(&["--json", "get", "psu"]);
    assert_eq!(out.code, 3);
    let msg = out.error_message();
    assert!(
        msg.contains("`set_voltage(ctx") && msg.contains("`set_output(ctx"),
        "{msg}"
    );

    let bad_read = source_bench(
        &SIM_DRIVER.replace(
            "return { output = s.out",
            "do return 42 end\n  return { output = s.out",
        ),
        "",
        "",
    );
    let out = bad_read.run(&["--json", "get", "psu"]);
    assert_eq!(out.code, 5);
    assert!(
        out.error_message().contains("read() must return a table"),
        "{}",
        out.error_message()
    );

    let bad_type = source_bench(
        &SIM_DRIVER.replace("voltage = v,", "voltage = \"high\","),
        "",
        "",
    );
    let out = bad_type.run(&["--json", "get", "psu"]);
    assert_eq!(out.code, 5);
    assert!(
        out.error_message().contains("read() must return"),
        "{}",
        out.error_message()
    );
}

#[test]
fn drivers_can_use_other_power_paths() {
    // E.g. a driver that powers an isolation relay before switching the output.
    let driver = SIM_DRIVER.replace(
        "function set_output(ctx, on) ctx.log(\"set_output\", on); s.out = on end",
        "function set_output(ctx, on)\n  ctx.power{ provider = \"sim\", endpoint = \"relay\" }:set(on)\n  s.out = on\nend",
    );
    let b = source_bench(&driver, "", "");
    let out = b.run(&["--json", "set", "psu", "output", "on"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(b.dir.path().join("mock-state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(state["power"]["sim"]["relay"], true);
}
