//! The OWON OEL1515 driver against a fake load speaking SCPI text over a pseudo-terminal.
// Linux only: on macOS the serial crate sets the speed with an ioctl that a pty refuses (ENOTTY).
#![cfg(target_os = "linux")]

#[path = "support/models.rs"]
mod harness;

use std::io::{Read, Write};
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use serialport::{SerialPort, TTYPort};
use tempfile::TempDir;

/// See tests/model_fnirsi_dps150.rs: pty creation and process spawning must not overlap.
static FORK_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone)]
struct Behavior {
    idn: Option<&'static str>,
    /// Booleans as the manual says (0|1) instead of what real units send (ON|OFF).
    numeric_bools: bool,
    /// SYST:REM has no effect: like a user holding the panel in local mode.
    stays_local: bool,
    /// Lose the reply to the first query of each kind in this list.
    lose_first: &'static [&'static str],
    /// Garbage line before every reply.
    noise: bool,
    /// The fault fields of MEAS:ALL:INFO?.
    faults: &'static str,
}

impl Default for Behavior {
    fn default() -> Self {
        Behavior {
            idn: Some("OWON,OEL1515,12345678,FV:V1.1.0"),
            numeric_bools: false,
            stays_local: false,
            lose_first: &[],
            noise: false,
            faults: "OFF,OFF,OFF",
        }
    }
}

struct State {
    remote: bool,
    function: String,
    input: bool,
    current: f64,
    voltage: f64,
    resistance: f64,
    power: f64,
    log: Vec<String>,
}

struct FakeLoad {
    path: String,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
}

impl FakeLoad {
    fn start(behavior: Behavior) -> Self {
        let guard = FORK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (mut master, mut slave) = TTYPort::pair().expect("pty pair");
        let path = slave.name().expect("pty name");
        slave.set_exclusive(false).unwrap();
        drop(slave);
        drop(guard);
        master.set_timeout(Duration::from_millis(10)).unwrap();

        let state = Arc::new(Mutex::new(State {
            remote: false,
            function: "current".into(),
            input: false,
            current: 1.0,
            voltage: 55.0,
            resistance: 200.0,
            power: 2.0,
            log: Vec::new(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (st, sp) = (state.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut rx: Vec<u8> = Vec::new();
            let mut buf = [0u8; 256];
            let mut lost: Vec<String> = Vec::new();
            while !sp.load(Ordering::Relaxed) {
                if let Ok(n) = master.read(&mut buf) {
                    rx.extend_from_slice(&buf[..n]);
                }
                while let Some(end) = rx.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = rx.drain(..=end).collect();
                    let command = String::from_utf8_lossy(&line).trim().to_string();
                    let mut s = st.lock().unwrap();
                    s.log.push(command.clone());
                    let reply = respond(&behavior, &mut s, &command);
                    if behavior.lose_first.contains(&command.as_str()) && !lost.contains(&command) {
                        lost.push(command);
                        continue;
                    }
                    if let Some(reply) = reply {
                        let mut out = Vec::new();
                        if behavior.noise {
                            out.extend_from_slice(b"\xff\x00junk\n");
                        }
                        out.extend_from_slice(reply.as_bytes());
                        out.push(b'\n');
                        let _ = master.write_all(&out);
                    }
                }
            }
        });
        FakeLoad { path, state, stop }
    }

    fn log(&self) -> Vec<String> {
        self.state.lock().unwrap().log.clone()
    }

    /// What was sent that isn't a query, leaving out going to remote mode and back.
    fn commands(&self) -> Vec<String> {
        self.log()
            .into_iter()
            .filter(|c| !c.ends_with('?') && c != "SYST:REM" && c != "SYST:LOC")
            .collect()
    }
}

impl Drop for FakeLoad {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn respond(behavior: &Behavior, s: &mut State, command: &str) -> Option<String> {
    let boolean = |on: bool| {
        Some(
            match (behavior.numeric_bools, on) {
                (false, true) => "ON",
                (false, false) => "OFF",
                (true, true) => "1",
                (true, false) => "0",
            }
            .to_string(),
        )
    };
    match command {
        "*IDN?" => return behavior.idn.map(str::to_string),
        "SYST:REM" => {
            s.remote = !behavior.stays_local;
            return None;
        }
        "SYST:LOC" => {
            s.remote = false;
            return None;
        }
        _ => {}
    }
    // Local mode: queries get an empty line, commands are ignored.
    if !s.remote {
        return command.ends_with('?').then(String::new);
    }
    if let Some((name, value)) = command.split_once(' ') {
        let number = value.parse::<f64>();
        match name {
            "FUNC" => {
                s.function = match value {
                    "CURR" => "current",
                    "VOLT" => "voltage",
                    "RES" => "resistance",
                    "POW" => "power",
                    "DYN" => "dynamic",
                    other => panic!("unknown function {other}"),
                }
                .to_string();
            }
            "INP" => s.input = value == "ON",
            // A setpoint of another function is dropped without a word.
            "CURR" if s.function == "current" => s.current = number.unwrap(),
            "VOLT" if s.function == "voltage" => s.voltage = number.unwrap(),
            "RES" if s.function == "resistance" => s.resistance = number.unwrap(),
            "POW" if s.function == "power" => s.power = number.unwrap(),
            "CURR" | "VOLT" | "RES" | "POW" => {}
            other => panic!("unexpected command {other}"),
        }
        return None;
    }
    match command {
        "MEAS:ALL:INFO?" => Some(format!("0.012,0.000,0.000,{}", behavior.faults)),
        "FUNC?" => Some(s.function.clone()),
        "INP?" => boolean(s.input),
        "INP:SHOR?" => boolean(false),
        "CURR?" => Some(format!("{:.3}", s.current)),
        "VOLT?" => Some(format!("{:.3}", s.voltage)),
        "RES?" => Some(format!("{:.2}", s.resistance)),
        "POW?" => Some(format!("{:.3}", s.power)),
        _ => Some("ERR".into()),
    }
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
}

fn bench(path: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("desk.toml"),
        format!(
            r#"
[devices.load]
model = "owon/oel1515"
load  = {{ port = {{ path = "{path}" }}, params = {{ wait = "0s", reply_ms = 150, attempts = 2, gap_ms = 20 }} }}
"#
        ),
    )
    .unwrap();
    dir
}

fn run(dir: &Path, args: &[&str]) -> Out {
    let child = {
        let _g = FORK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        harness::desk()
            .arg("--config")
            .arg(dir.join("desk.toml"))
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let out = child.wait_with_output().unwrap();
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
    }
}

fn get(load: &FakeLoad) -> Value {
    let dir = bench(&load.path);
    let out = run(dir.path(), &["--json", "get", "load"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    out.data()["values"].clone()
}

fn set(load: &FakeLoad, args: &[&str]) -> Out {
    let dir = bench(&load.path);
    let mut all = vec!["--json", "set", "load"];
    all.extend_from_slice(args);
    run(dir.path(), &all)
}

#[test]
fn a_read_reports_everything_and_changes_nothing() {
    let load = FakeLoad::start(Behavior::default());
    let v = get(&load);
    assert_eq!(v["measured_voltage"], 0.012, "{v}");
    assert_eq!(v["measured_current"], 0.0, "{v}");
    assert_eq!(v["measured_power"], 0.0, "{v}");
    assert_eq!(v["ovp_tripped"], false, "{v}");
    assert_eq!(v["ocp_tripped"], false, "{v}");
    assert_eq!(v["opp_tripped"], false, "{v}");
    assert_eq!(v["short"], false, "{v}");
    assert_eq!(v["function"], "current", "{v}");
    assert_eq!(v["input"], false, "{v}");
    assert_eq!(v["current"], 1.0, "{v}");
    assert_eq!(v["voltage"], 55.0, "{v}");
    assert_eq!(v["resistance"], 200.0, "{v}");
    assert_eq!(v["power"], 2.0, "{v}");
    assert_eq!(v["firmware"], "V1.1.0", "{v}");
    assert!(load.commands().is_empty(), "{:?}", load.log());
}

#[test]
fn every_session_identifies_then_goes_to_remote_and_ends_in_local() {
    let load = FakeLoad::start(Behavior::default());
    get(&load);
    let log = load.log();
    assert_eq!(log[0], "*IDN?", "{log:?}");
    assert_eq!(log[1], "SYST:REM", "{log:?}");
    assert_eq!(log.last().unwrap(), "SYST:LOC", "{log:?}");
    assert!(
        !load.state.lock().unwrap().remote,
        "the panel must be usable again"
    );
}

#[test]
fn another_device_is_refused_before_anything_else_is_sent() {
    let load = FakeLoad::start(Behavior {
        idn: Some("OWON,XDM1041,12345678,V4.3.0,3"),
        ..Behavior::default()
    });
    let dir = bench(&load.path);
    let out = run(dir.path(), &["--json", "get", "load"]);
    assert_ne!(out.code, 0, "{}", out.stdout);
    assert!(out.stdout.contains("XDM1041"), "{}", out.stdout);
    assert_eq!(load.log(), ["*IDN?"]);
}

#[test]
fn the_t_version_is_accepted() {
    let load = FakeLoad::start(Behavior {
        idn: Some("OWON,OEL1515T,12345678,FV:V1.1.0"),
        ..Behavior::default()
    });
    assert_eq!(get(&load)["function"], "current");
}

#[test]
fn a_silent_device_fails_with_a_clear_message_and_no_commands() {
    let load = FakeLoad::start(Behavior {
        idn: None,
        ..Behavior::default()
    });
    let dir = bench(&load.path);
    let out = run(dir.path(), &["--json", "get", "load"]);
    assert_ne!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.json()["error"]["code"], "script_failed");
    assert!(out.stdout.contains("no answer to *IDN?"), "{}", out.stdout);
    assert!(load.log().iter().all(|c| c == "*IDN?"), "{:?}", load.log());
}

#[test]
fn a_load_stuck_in_local_mode_is_an_error_not_empty_values() {
    let load = FakeLoad::start(Behavior {
        stays_local: true,
        ..Behavior::default()
    });
    let dir = bench(&load.path);
    let out = run(dir.path(), &["--json", "get", "load"]);
    assert_ne!(out.code, 0, "{}", out.stdout);
    assert!(out.stdout.contains("local mode"), "{}", out.stdout);
}

#[test]
fn the_function_comes_first_and_the_input_last_and_all_are_verified() {
    let load = FakeLoad::start(Behavior::default());
    let out = set(
        &load,
        &["input", "on", "voltage", "5v", "function", "voltage"],
    );
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(load.commands(), ["FUNC VOLT", "VOLT 5.000", "INP ON"]);
    let d = out.data();
    assert_eq!(d["verified"]["function"], true, "{d}");
    assert_eq!(d["verified"]["voltage"], true, "{d}");
    assert_eq!(d["verified"]["input"], true, "{d}");
}

#[test]
fn every_function_takes_its_setpoint() {
    for (function, setpoint, value, sent) in [
        ("current", "current", "500ma", ["FUNC CURR", "CURR 0.500"]),
        ("voltage", "voltage", "12v", ["FUNC VOLT", "VOLT 12.000"]),
        (
            "resistance",
            "resistance",
            "1kohm",
            ["FUNC RES", "RES 1000.00"],
        ),
        ("power", "power", "1.5w", ["FUNC POW", "POW 1.500"]),
    ] {
        let load = FakeLoad::start(Behavior::default());
        let out = set(&load, &["function", function, setpoint, value]);
        assert_eq!(out.code, 0, "{}", out.stdout);
        assert_eq!(load.commands(), sent);
        assert_eq!(out.data()["verified"][setpoint], true, "{}", out.stdout);
    }
}

#[test]
fn a_setpoint_of_another_function_is_refused_not_lost() {
    let load = FakeLoad::start(Behavior::default());
    let out = set(&load, &["voltage", "5v"]);
    assert_ne!(out.code, 0, "{}", out.stdout);
    assert!(out.stdout.contains("function voltage"), "{}", out.stdout);
    assert!(load.commands().is_empty(), "{:?}", load.log());
    assert_eq!(load.state.lock().unwrap().function, "current");
}

#[test]
fn the_panel_is_given_back_after_a_refused_setting() {
    let load = FakeLoad::start(Behavior::default());
    assert_ne!(set(&load, &["voltage", "5v"]).code, 0);
    assert_eq!(load.log().last().unwrap(), "SYST:LOC", "{:?}", load.log());
}

#[test]
fn the_ratings_are_enforced_before_anything_is_sent() {
    let load = FakeLoad::start(Behavior::default());
    for args in [
        ["current", "15.1a"],
        ["voltage", "151v"],
        ["resistance", "0.01ohm"],
        ["power", "151w"],
    ] {
        let out = set(&load, &args);
        assert_ne!(out.code, 0, "{}", out.stdout);
    }
    assert!(load.commands().is_empty(), "{:?}", load.log());
}

#[test]
fn bench_limits_apply() {
    let load = FakeLoad::start(Behavior::default());
    let dir = bench(&load.path);
    let config = dir.path().join("desk.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("limits = { load = { current = 2 } }\n");
    std::fs::write(&config, text).unwrap();
    let out = run(dir.path(), &["--json", "set", "load", "current", "3a"]);
    assert_eq!(
        out.json()["error"]["code"],
        "limit_exceeded",
        "{}",
        out.stdout
    );
    assert!(load.commands().is_empty(), "{:?}", load.log());
}

#[test]
fn the_manuals_numeric_booleans_also_work() {
    let load = FakeLoad::start(Behavior {
        numeric_bools: true,
        ..Behavior::default()
    });
    load.state.lock().unwrap().input = true;
    let v = get(&load);
    assert_eq!(v["input"], true, "{v}");
    assert_eq!(v["short"], false, "{v}");
}

#[test]
fn tripped_protections_are_reported_one_by_one() {
    let load = FakeLoad::start(Behavior {
        faults: "OFF,ON,OFF",
        ..Behavior::default()
    });
    let v = get(&load);
    assert_eq!(v["ovp_tripped"], false, "{v}");
    assert_eq!(v["ocp_tripped"], true, "{v}");
    assert_eq!(v["opp_tripped"], false, "{v}");
}

#[test]
fn dynamic_is_reported_and_an_unknown_function_is_left_out() {
    let load = FakeLoad::start(Behavior::default());
    load.state.lock().unwrap().function = "dynamic".into();
    assert_eq!(get(&load)["function"], "dynamic");
    load.state.lock().unwrap().function = "battery".into();
    let v = get(&load);
    assert!(v["function"].is_null(), "{v}");
    assert_eq!(v["measured_voltage"], 0.012, "{v}");
}

#[test]
fn garbage_and_a_lost_reply_are_survived() {
    let load = FakeLoad::start(Behavior {
        noise: true,
        lose_first: &["MEAS:ALL:INFO?", "*IDN?", "FUNC?"],
        ..Behavior::default()
    });
    let v = get(&load);
    assert_eq!(v["measured_voltage"], 0.012, "{v}");
    assert_eq!(v["function"], "current", "{v}");
}
