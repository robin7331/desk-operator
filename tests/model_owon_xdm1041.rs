//! The OWON XDM1041 driver against a fake meter speaking SCPI text over a pseudo-terminal.
#![cfg(unix)]

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

/// What firmware V4.3.0 sends for Ω; older firmware sends A6 B8.
const OHM: &[u8] = b"\xce\xa9";
const DEG_C: &[u8] = b"\xa1\xe6";

#[derive(Clone)]
struct Behavior {
    idn: Option<&'static str>,
    /// Like firmware 20210525: every command (no `?`) is answered with OK lines.
    ok_spam: bool,
    /// Lose the reply to the first query of each kind in this list.
    lose_first: &'static [&'static str],
    /// Garbage line before every reply.
    noise: bool,
    /// What MEAS1? answers.
    measurement: &'static str,
    /// What RATE? answers while slow (the manual says L).
    slow_reply: &'static str,
    /// Like the real meter: the first reply after waking up starts with 00 01 00.
    wake_prefix: bool,
}

impl Default for Behavior {
    fn default() -> Self {
        Behavior {
            idn: Some("OWON,XDM1041,12345678,V4.3.0,3"),
            ok_spam: false,
            lose_first: &[],
            noise: false,
            measurement: "+1.2345E+00",
            slow_reply: "S",
            wake_prefix: false,
        }
    }
}

/// (FUNC? reply, ranges as RANGE? shows them). Empty: RANGE? stays unanswered.
fn function_table(function: &str) -> (&'static str, Vec<Vec<u8>>) {
    let text = |names: &[&str]| names.iter().map(|n| n.as_bytes().to_vec()).collect();
    let ohms = |names: &[&str]| {
        names
            .iter()
            .map(|n| [n.as_bytes(), OHM].concat())
            .collect::<Vec<_>>()
    };
    match function {
        "VOLT:DC" => (
            "VOLT",
            text(&["50 mV", "500 mV", "5 V", "50 V", "500 V", "1000 V"]),
        ),
        "VOLT:AC" => ("VOLT AC", text(&["500mV", "5V", "50V", "500V", "750V"])),
        "CURR:DC" => (
            "CURR",
            text(&["500uA", "5mA", "50mA", "500mA", "5A", "10A"]),
        ),
        "RES" => (
            "RES",
            ohms(&["500 ", "5 K", "50 K", "500 K", "5 M", "50 M"]),
        ),
        "CAP" => (
            "CAP",
            vec![
                b"50nF".to_vec(),
                b"500nF".to_vec(),
                b"5\xaa\xccF".to_vec(),
                b"50\xaa\xccF".to_vec(),
                b"500\xaa\xccF".to_vec(),
                b"5mF".to_vec(),
                b"50mF".to_vec(),
            ],
        ),
        "DIOD" => ("DIOD", vec![]),
        "CONT" => ("CONT", vec![]),
        "FREQ" => ("FREQ", vec![]),
        "PER" => ("PER", vec![]),
        "TEMP:RTD KITS90" => ("TEMP", text(&["KITS90"])),
        "TEMP:RTD PT100" => ("TEMP", text(&["Pt100"])),
        other => panic!("unknown function {other}"),
    }
}

struct State {
    function: String,
    auto: bool,
    range: usize,
    rate: String,
    log: Vec<String>,
}

struct FakeMeter {
    path: String,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
}

impl FakeMeter {
    fn start(behavior: Behavior) -> Self {
        let guard = FORK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (mut master, mut slave) = TTYPort::pair().expect("pty pair");
        let path = slave.name().expect("pty name");
        slave.set_exclusive(false).unwrap();
        drop(slave);
        drop(guard);
        master.set_timeout(Duration::from_millis(10)).unwrap();

        let state = Arc::new(Mutex::new(State {
            function: "VOLT:DC".into(),
            auto: true,
            range: 2,
            rate: "S".into(),
            log: Vec::new(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (st, sp) = (state.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut rx: Vec<u8> = Vec::new();
            let mut buf = [0u8; 256];
            let mut lost: Vec<String> = Vec::new();
            let mut woke = !behavior.wake_prefix;
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
                        if !woke {
                            woke = true;
                            out.extend_from_slice(&[0x00, 0x01, 0x00]);
                        }
                        if behavior.noise {
                            out.extend_from_slice(b"\xff\x00junk\r\n");
                        }
                        out.extend_from_slice(&reply);
                        out.extend_from_slice(b"\r\n");
                        let _ = master.write_all(&out);
                    } else if behavior.ok_spam && !command.ends_with('?') {
                        let _ = master.write_all(b"OK\nOK\nOK\n");
                    }
                }
            }
        });
        FakeMeter { path, state, stop }
    }

    fn log(&self) -> Vec<String> {
        self.state.lock().unwrap().log.clone()
    }

    fn commands(&self) -> Vec<String> {
        self.log()
            .into_iter()
            .filter(|c| !c.ends_with('?'))
            .collect()
    }
}

fn respond(behavior: &Behavior, s: &mut State, command: &str) -> Option<Vec<u8>> {
    let (func, ranges) = function_table(&s.function);
    if let Some(conf) = command.strip_prefix("CONF:") {
        s.function = conf.strip_suffix(" AUTO").unwrap_or(conf).to_string();
        s.auto = true;
        s.range = 0;
        return None;
    }
    if let Some(n) = command.strip_prefix("RANGE ") {
        s.auto = false;
        s.range = n.parse::<usize>().unwrap() - 1;
        return None;
    }
    if let Some(rate) = command.strip_prefix("RATE ") {
        s.rate = rate.to_string();
        return None;
    }
    match command {
        "AUTO" => {
            s.auto = true;
            None
        }
        "*IDN?" => behavior.idn.map(|i| i.as_bytes().to_vec()),
        "FUNC?" => Some(format!("\"{func}\"").into_bytes()),
        "AUTO?" => Some(if s.auto { b"1".to_vec() } else { b"0".to_vec() }),
        "RANGE?" => ranges.get(s.range).cloned(),
        "RATE?" => Some(
            if s.rate == "S" {
                behavior.slow_reply
            } else {
                &s.rate
            }
            .as_bytes()
            .to_vec(),
        ),
        "MEAS1?" => Some(behavior.measurement.as_bytes().to_vec()),
        "MEAS1:SHOW?" => Some([b"23.5".as_slice(), DEG_C].concat()),
        _ => None,
    }
}

impl Drop for FakeMeter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
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
[devices.dmm]
model = "owon/xdm1041"
meter = {{ port = {{ path = "{path}" }}, params = {{ wait = "0s", reply_ms = 150, attempts = 3, gap_ms = 20, settle_ms = 10 }} }}
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

fn get(meter: &FakeMeter) -> Value {
    let dir = bench(&meter.path);
    let out = run(dir.path(), &["--json", "get", "dmm"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    out.data()["values"].clone()
}

fn set(meter: &FakeMeter, args: &[&str]) -> Out {
    let dir = bench(&meter.path);
    let mut all = vec!["--json", "set", "dmm"];
    all.extend_from_slice(args);
    run(dir.path(), &all)
}

fn select(meter: &FakeMeter, function: &str) {
    let mut s = meter.state.lock().unwrap();
    s.function = function.into();
    if function.starts_with("TEMP") {
        s.range = 0; // the only "range": the sensor type
    }
}

fn assert_reading(values: &Value, value: f64, unit: &str) {
    let r = &values["reading"];
    let got = r["value"].as_f64().unwrap_or(f64::NAN);
    assert!((got - value).abs() < 1e-9, "{values}");
    assert_eq!(r["unit"], unit, "{values}");
}

#[test]
fn a_read_reports_everything_and_only_sends_queries() {
    let meter = FakeMeter::start(Behavior::default());
    let v = get(&meter);
    assert_reading(&v, 1.2345, "V");
    assert_eq!(v["function"], "dc_voltage");
    assert_eq!(v["range"], "auto");
    assert_eq!(v["active_range"], "5V");
    assert_eq!(v["rate"], "slow");
    assert_eq!(v["firmware"], "V4.3.0");
    let log = meter.log();
    assert_eq!(log[0], "*IDN?", "identified before anything else");
    assert!(
        meter.commands().is_empty(),
        "reading must not lock the panel or change anything: {log:?}"
    );
}

#[test]
fn the_first_reply_after_waking_up_is_understood_at_once() {
    let meter = FakeMeter::start(Behavior {
        wake_prefix: true,
        ..Behavior::default()
    });
    assert_eq!(get(&meter)["firmware"], "V4.3.0");
    let idn = meter.log().iter().filter(|c| *c == "*IDN?").count();
    assert_eq!(idn, 1, "no retry needed: {:?}", meter.log());
}

#[test]
fn another_device_is_refused_before_anything_else_is_sent() {
    let meter = FakeMeter::start(Behavior {
        idn: Some("OWON,XDM2041,12345678,V1.0.0,3"),
        ..Behavior::default()
    });
    let dir = bench(&meter.path);
    let out = run(dir.path(), &["--json", "set", "dmm", "rate", "fast"]);
    assert_ne!(out.code, 0, "{}", out.stdout);
    assert!(out.stdout.contains("XDM2041"), "{}", out.stdout);
    assert_eq!(meter.log(), ["*IDN?"]);
}

#[test]
fn a_silent_device_fails_with_a_clear_message() {
    let meter = FakeMeter::start(Behavior {
        idn: None,
        ..Behavior::default()
    });
    let dir = bench(&meter.path);
    let out = run(dir.path(), &["--json", "get", "dmm"]);
    assert_ne!(out.code, 0, "{}", out.stdout);
    assert!(out.stdout.contains("no answer to *IDN?"), "{}", out.stdout);
    assert!(meter.log().iter().all(|c| c == "*IDN?"));
}

#[test]
fn survives_lost_replies_and_stray_lines() {
    let meter = FakeMeter::start(Behavior {
        lose_first: &["*IDN?", "FUNC?", "MEAS1?"],
        noise: true,
        ..Behavior::default()
    });
    assert_reading(&get(&meter), 1.2345, "V");
}

#[test]
fn selecting_a_function_uses_auto_range_and_changes_the_unit() {
    let meter = FakeMeter::start(Behavior {
        ok_spam: true,
        ..Behavior::default()
    });
    let out = set(&meter, &["function", "dc_current"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(meter.commands(), ["CONF:CURR:DC AUTO"]);
    let d = out.data();
    assert_eq!(d["verified"]["function"], true, "{d}");
    assert_reading(&d["values"], 1.2345, "A");
}

#[test]
fn function_is_applied_before_range() {
    let meter = FakeMeter::start(Behavior::default());
    let out = set(&meter, &["range", "50mA", "function", "dc_current"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(meter.commands(), ["CONF:CURR:DC AUTO", "RANGE 3"]);
    let d = out.data();
    assert_eq!(d["verified"]["range"], true, "{d}");
    assert_eq!(d["values"]["range"], "50mA");
}

#[test]
fn back_to_auto_range() {
    let meter = FakeMeter::start(Behavior::default());
    assert_eq!(set(&meter, &["range", "50V"]).code, 0);
    let out = set(&meter, &["range", "auto"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(meter.commands(), ["RANGE 4", "AUTO"]);
    assert_eq!(out.data()["values"]["range"], "auto");
    assert_eq!(out.data()["values"]["active_range"], "50V");
}

#[test]
fn a_range_of_another_function_is_refused() {
    let meter = FakeMeter::start(Behavior::default());
    let out = set(&meter, &["range", "10A"]);
    assert_ne!(out.code, 0, "{}", out.stdout);
    assert!(out.stdout.contains("not a range of"), "{}", out.stdout);
    assert!(meter.commands().is_empty(), "{:?}", meter.log());
}

#[test]
fn functions_without_ranges_are_read_without_asking_for_one() {
    let meter = FakeMeter::start(Behavior::default());
    select(&meter, "FREQ");
    let v = get(&meter);
    assert_reading(&v, 1.2345, "Hz");
    assert!(v["range"].is_null(), "{v}");
    assert!(!meter.log().contains(&"RANGE?".to_string()));

    let out = set(&meter, &["range", "auto"]);
    assert_ne!(out.code, 0, "{}", out.stdout);
    assert!(meter.commands().is_empty(), "{:?}", meter.log());
}

#[test]
fn resistance_ranges_decode_the_meters_ohm_bytes() {
    let meter = FakeMeter::start(Behavior::default());
    select(&meter, "RES");
    meter.state.lock().unwrap().range = 1;
    meter.state.lock().unwrap().auto = false;
    let v = get(&meter);
    assert_eq!(v["range"], "5kohm", "{v}");
    assert_reading(&v, 1.2345, "Ω");
    assert_eq!(v["overload"], false);
}

#[test]
fn capacitance_ranges_decode_the_meters_micro_bytes() {
    let meter = FakeMeter::start(Behavior::default());
    select(&meter, "CAP");
    meter.state.lock().unwrap().range = 2;
    assert_eq!(get(&meter)["active_range"], "5uF");
}

#[test]
fn over_range_is_an_overload_and_not_a_reading() {
    let meter = FakeMeter::start(Behavior {
        measurement: "1E+9",
        ..Behavior::default()
    });
    select(&meter, "RES");
    let v = get(&meter);
    assert_eq!(v["overload"], true, "{v}");
    assert!(v["reading"].is_null(), "{v}");
}

#[test]
fn the_overload_value_is_never_reported_as_a_reading() {
    for function in ["VOLT:DC", "FREQ", "TEMP:RTD PT100"] {
        let meter = FakeMeter::start(Behavior {
            measurement: "1E+9",
            ..Behavior::default()
        });
        select(&meter, function);
        let v = get(&meter);
        assert_eq!(v["overload"], true, "{function}: {v}");
        assert!(v["reading"].is_null(), "{function}: {v}");
    }
}

#[test]
fn replies_are_used_as_soon_as_they_arrive() {
    let meter = FakeMeter::start(Behavior::default());
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("desk.toml"),
        format!(
            r#"
[devices.dmm]
model = "owon/xdm1041"
meter = {{ port = {{ path = "{}" }}, params = {{ wait = "0s", reply_ms = 2000 }} }}
"#,
            meter.path
        ),
    )
    .unwrap();
    let start = std::time::Instant::now();
    let out = run(dir.path(), &["--json", "get", "dmm"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert!(
        start.elapsed() < Duration::from_millis(1500),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn old_firmware_mixes_up_diode_and_continuity() {
    let meter = FakeMeter::start(Behavior {
        idn: Some("OWON,XDM1041,12345678,V4.2.0,3"),
        ..Behavior::default()
    });
    select(&meter, "DIOD");
    let v = get(&meter);
    assert_eq!(v["function"], "continuity", "{v}");
    assert_reading(&v, 1.2345, "Ω");
}

#[test]
fn current_firmware_reports_diode_as_diode() {
    let meter = FakeMeter::start(Behavior::default());
    select(&meter, "DIOD");
    let v = get(&meter);
    assert_eq!(v["function"], "diode", "{v}");
    assert_reading(&v, 1.2345, "V");
}

#[test]
fn temperature_tells_the_sensor_and_the_unit() {
    let meter = FakeMeter::start(Behavior::default());
    let out = set(&meter, &["function", "temperature_pt100"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(meter.commands(), ["CONF:TEMP:RTD PT100"]);
    let v = &out.data()["values"];
    assert_eq!(v["function"], "temperature_pt100", "{v}");
    assert_reading(v, 1.2345, "°C");
}

#[test]
fn rate_uses_s_for_slow_and_reads_the_manuals_l_too() {
    let meter = FakeMeter::start(Behavior {
        slow_reply: "L",
        ..Behavior::default()
    });
    assert_eq!(get(&meter)["rate"], "slow");
    let out = set(&meter, &["rate", "fast"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.data()["verified"]["rate"], true);
    assert_eq!(meter.commands(), ["RATE F"]);
    let out = set(&meter, &["rate", "slow"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(meter.commands(), ["RATE F", "RATE S"]);
}
