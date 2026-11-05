//! The FNIRSI DPS-150-family driver (`devices/fnirsi/shared/dps150_protocol.lua`) against a fake
//! device that speaks the protocol over a pseudo-terminal.
//!
//! This test belongs to the device library, not to the core: when `devices/` moves to its own
//! repository (M5+), definitions carry their own tests.
#![cfg(unix)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use serialport::{SerialPort, TTYPort};
use tempfile::TempDir;

/// See tests/serial.rs: pty creation and process spawning must not overlap.
static FORK_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy)]
struct Behavior {
    echo_output: bool,
    telemetry: bool,
    garbage: bool,
    /// Answer to the model name query (0xDE).
    name: Option<&'static str>,
    /// Answer the full status query (0xFF) with the IPS3608 layout.
    status: bool,
    /// Ignore this many name queries first (a device that's still booting).
    booting: usize,
    /// Start with this protection latched (1 OVP, 2 OCP, ...): output off until `DB=1`.
    tripped: u8,
    /// Don't answer the full status query with this number (1 = the first).
    skip_status: Option<usize>,
}

impl Default for Behavior {
    /// Like the documented DPS-150: echoes output commands, no status layout used.
    fn default() -> Self {
        Behavior {
            echo_output: true,
            telemetry: true,
            garbage: false,
            name: None,
            status: false,
            booting: 0,
            tripped: 0,
            skip_status: None,
        }
    }
}

impl Behavior {
    /// Like the IPS3608 measured on hardware: no echo, name and full status queries.
    fn ips3608() -> Self {
        Behavior {
            echo_output: false,
            name: Some("IPS3608"),
            status: true,
            ..Behavior::default()
        }
    }
}

/// The IPS3608 full status: 139 bytes, offsets as documented in devices/fnirsi/README.md.
fn status_blob(s: &State) -> Vec<u8> {
    let mut d = vec![0u8; 139];
    let mut put = |offset: usize, v: f32| d[offset..offset + 4].copy_from_slice(&v.to_le_bytes());
    let vout = if s.output { s.vset } else { 0.0 };
    put(0, 48.0); // constant on the IPS3608; the real input voltage comes from query 0xC0
    put(4, s.vset);
    put(8, s.iset);
    put(12, vout);
    put(24, 31.5); // temperature
    put(76, 36.0); // OVP threshold
    put(80, 8.0); // OCP threshold
    put(99, 0.25); // Ah
    put(103, 1.5); // Wh
    d[107] = s.output as u8;
    d[108] = s.protection;
    d[109] = 1; // CV (the IPS3608 reports 1 with the output off, too)
    d
}

#[derive(Default)]
struct State {
    connected: bool,
    vset: f32,
    iset: f32,
    output: bool,
    protection: u8,
    /// Commands received, in order.
    log: Vec<String>,
    /// When each frame arrived, in order.
    times: Vec<Instant>,
    bad_frames: usize,
    name_queries: usize,
    status_queries: usize,
}

struct FakePsu {
    path: String,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
}

fn checksum(cmd: u8, data: &[u8]) -> u8 {
    data.iter()
        .fold(cmd.wrapping_add(data.len() as u8), |a, b| {
            a.wrapping_add(*b)
        })
}

fn device_frame(cmd: u8, data: &[u8]) -> Vec<u8> {
    let mut f = vec![0xF0, 0xA1, cmd, data.len() as u8];
    f.extend_from_slice(data);
    f.push(checksum(cmd, data));
    f
}

fn floats(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

impl FakePsu {
    fn start(behavior: Behavior) -> Self {
        let guard = FORK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (mut master, mut slave) = TTYPort::pair().expect("pty pair");
        let path = slave.name().expect("pty name");
        slave.set_exclusive(false).unwrap();
        drop(slave);
        drop(guard);
        master.set_timeout(Duration::from_millis(10)).unwrap();

        let state = Arc::new(Mutex::new(State {
            protection: behavior.tripped,
            ..State::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (st, sp) = (state.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut rx: Vec<u8> = Vec::new();
            let mut buf = [0u8; 256];
            let mut last_push = Instant::now();
            while !sp.load(Ordering::Relaxed) {
                match master.read(&mut buf) {
                    Ok(n) if n > 0 => rx.extend_from_slice(&buf[..n]),
                    Ok(_) | Err(_) => {}
                }
                // Parse host frames: F1 type cmd len data chk
                loop {
                    let Some(start) = rx.iter().position(|b| *b == 0xF1) else {
                        rx.clear();
                        break;
                    };
                    rx.drain(..start);
                    if rx.len() < 4 || rx.len() < 5 + rx[3] as usize {
                        break;
                    }
                    let (kind, cmd, len) = (rx[1], rx[2], rx[3] as usize);
                    let data = rx[4..4 + len].to_vec();
                    let chk = rx[4 + len];
                    rx.drain(..5 + len);
                    let mut s = st.lock().unwrap();
                    if chk != checksum(cmd, &data) {
                        s.bad_frames += 1;
                        continue;
                    }
                    s.times.push(Instant::now());
                    let float = || f32::from_le_bytes(data[..4].try_into().unwrap());
                    match (kind, cmd) {
                        (0xC1, 0x00) if data == [1] => {
                            s.connected = true;
                            s.log.push("connect".into());
                        }
                        (0xC1, 0x00) if data == [0] => {
                            s.connected = false;
                            s.log.push("disconnect".into());
                        }
                        (0xB1, 0xC1) if len == 4 => {
                            let v = float();
                            s.vset = v;
                            s.log.push(format!("vset {v}"));
                        }
                        (0xB1, 0xC2) if len == 4 => {
                            let a = float();
                            s.iset = a;
                            s.log.push(format!("iset {a}"));
                        }
                        (0xA1, 0xDE) => {
                            s.log.push("query name".into());
                            s.name_queries += 1;
                            if s.name_queries <= behavior.booting {
                                continue; // not ready yet
                            }
                            if let Some(name) = behavior.name {
                                let _ = master.write_all(&device_frame(0xDE, name.as_bytes()));
                            }
                        }
                        (0xA1, 0xFF) => {
                            s.log.push("query status".into());
                            s.status_queries += 1;
                            if behavior.skip_status == Some(s.status_queries) {
                                continue; // a lost reply
                            }
                            if behavior.status {
                                let _ = master.write_all(&device_frame(0xFF, &status_blob(&s)));
                            }
                        }
                        (0xA1, 0xC0) => {
                            s.log.push("query input".into());
                            let _ = master.write_all(&device_frame(0xC0, &floats(&[44.7])));
                        }
                        (0xB1, 0xDB) if len == 1 => {
                            s.output = data[0] == 1;
                            if s.output {
                                s.protection = 0; // like the real device: DB=1 clears a trip
                            }
                            s.log.push(format!("output {}", data[0]));
                            if behavior.echo_output {
                                let _ = master.write_all(&device_frame(0xDB, &data));
                            }
                        }
                        other => s.log.push(format!("unknown {other:02x?}")),
                    }
                }
                // Push telemetry while connected (faster than the real ~600 ms, to keep tests quick).
                let s = st.lock().unwrap();
                if s.connected
                    && behavior.telemetry
                    && last_push.elapsed() > Duration::from_millis(80)
                {
                    last_push = Instant::now();
                    let v = if s.output { s.vset } else { 0.0 };
                    let mut out = Vec::new();
                    if behavior.garbage {
                        out.extend_from_slice(&[0x00, 0xF0, 0xFF, 0x13, 0xF0, 0xA1]);
                        out.extend(device_frame(0xC0, &floats(&[19.8]))); // input voltage push
                    }
                    out.extend(device_frame(0xC3, &floats(&[v, 0.0, 0.0])));
                    let _ = master.write_all(&out);
                }
            }
        });
        FakePsu { path, state, stop }
    }

    fn log(&self) -> Vec<String> {
        self.state.lock().unwrap().log.clone()
    }

    /// Log entries with the time since the previous frame.
    fn timed_log(&self) -> Vec<(String, Duration)> {
        let s = self.state.lock().unwrap();
        s.log
            .iter()
            .zip(&s.times)
            .zip(std::iter::once(None).chain(s.times.iter().map(Some)))
            .map(|((l, t), prev)| (l.clone(), prev.map_or(Duration::ZERO, |p| *t - *p)))
            .collect()
    }
}

impl Drop for FakePsu {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn driver() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("devices/fnirsi/shared/dps150_protocol.lua")
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
    bench_with(path, "")
}

/// `extra` is appended to the driver params, e.g. `, name = "IPS3608"`.
fn bench_with(path: &str, extra: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("desk.toml"),
        format!(
            r#"
[devices.psu]
source = {{ script = "{}", port = {{ path = "{path}" }}, timeout = "6s", ratings = {{ max_voltage = 36.0, max_current = 8.0 }}, params = {{ telemetry_ms = 800, settle_ms = 120, echo_ms = 300, wait = "0s"{extra} }} }}
limits = {{ source = {{ voltage = 24 }} }}
"#,
            driver().display()
        ),
    )
    .unwrap();
    dir
}

fn run(dir: &Path, args: &[&str]) -> Out {
    let child = {
        let _g = FORK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        Command::new(env!("CARGO_BIN_EXE_desk"))
            .env_remove("DESK_CONFIG")
            .env("DESK_MOCK_STATE", dir.join("mock-state.json"))
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

fn close(v: &Value, want: f64) -> bool {
    (v.as_f64().unwrap_or(f64::NAN) - want).abs() < 1e-4
}

#[test]
fn set_and_switch_on_in_one_session() {
    let psu = FakePsu::start(Behavior::default());
    let dir = bench(&psu.path);
    let out = run(
        dir.path(),
        &["--json", "source", "set", "psu", "5v", "1a", "--on"],
    );
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(
        psu.log(),
        ["connect", "vset 5", "iset 1", "output 1", "disconnect"],
        "setpoints before output, one session, front panel released"
    );
    assert_eq!(
        psu.state.lock().unwrap().bad_frames,
        0,
        "every frame had a valid checksum"
    );
    assert!(!psu.state.lock().unwrap().connected);

    let d = out.data();
    assert_eq!(d["verified"]["output"], true, "confirmed by the echo");
    assert!(
        d["verified"].get("voltage").is_none(),
        "setpoints can't be read back: {}",
        d["verified"]
    );
    assert!(
        close(&d["reading"]["voltage"], 5.0),
        "a fresh measurement, after the output went on: {}",
        d["reading"]
    );
    assert_eq!(d["reading"]["output"], true);
}

#[test]
fn read_only_connects_and_measures() {
    let psu = FakePsu::start(Behavior::default());
    let dir = bench(&psu.path);
    let out = run(dir.path(), &["--json", "source", "read", "psu"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(psu.log(), ["connect", "disconnect"]);
    let d = out.data();
    assert!(close(&d["reading"]["voltage"], 0.0));
    assert!(
        d["reading"]["output"].is_null(),
        "output unknown without a command in this session"
    );
}

#[test]
fn without_echo_the_output_is_not_confirmed() {
    let psu = FakePsu::start(Behavior {
        echo_output: false,
        ..Behavior::default()
    });
    let dir = bench(&psu.path);
    let out = run(dir.path(), &["--json", "source", "output", "on", "psu"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let d = out.data();
    assert!(d["verified"].get("output").is_none(), "{}", d["verified"]);
    assert!(psu.log().contains(&"output 1".to_string()));
}

#[test]
fn resynchronizes_after_garbage_and_foreign_frames() {
    let psu = FakePsu::start(Behavior {
        garbage: true,
        ..Behavior::default()
    });
    let dir = bench(&psu.path);
    let out = run(
        dir.path(),
        &["--json", "source", "set", "psu", "12v", "--on"],
    );
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert!(
        close(&out.data()["reading"]["voltage"], 12.0),
        "{}",
        out.data()["reading"]
    );
}

#[test]
fn silent_device_fails_clearly_and_still_disconnects() {
    let psu = FakePsu::start(Behavior {
        telemetry: false,
        ..Behavior::default()
    });
    let dir = bench(&psu.path);
    let out = run(dir.path(), &["--json", "source", "read", "psu"]);
    assert_eq!(out.code, 5, "{}", out.stdout);
    let msg = out.json()["error"]["message"].as_str().unwrap().to_string();
    assert!(msg.contains("no measurement from the device"), "{msg}");
    assert_eq!(psu.log().last().map(String::as_str), Some("disconnect"));
}

#[test]
fn limits_are_enforced_before_any_setpoint_is_sent() {
    let psu = FakePsu::start(Behavior::default());
    let dir = bench(&psu.path);
    for (args, why) in [
        (
            &["--json", "source", "set", "psu", "25v"][..],
            "bench limit",
        ),
        (
            &["--json", "source", "set", "psu", "9a"][..],
            "device maximum",
        ),
    ] {
        let out = run(dir.path(), args);
        assert_eq!(out.code, 8, "{}", out.stdout);
        assert!(
            out.json()["error"]["message"]
                .as_str()
                .unwrap()
                .contains(why)
        );
    }
    assert!(
        psu.log()
            .iter()
            .all(|c| c == "connect" || c == "disconnect"),
        "no setpoint reached the device: {:?}",
        psu.log()
    );
}

#[test]
fn the_bundled_ips3608_definition_works_as_is() {
    // `model = "fnirsi/ips3608"`: name check, status layout and limits come from the definition
    // (bundled into the binary); the device only adds its port and faster timings for the test.
    let psu = FakePsu::start(Behavior::ips3608());
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("desk.toml"),
        format!(
            r#"
[devices.psu]
model  = "fnirsi/ips3608"
source = {{ port = {{ path = "{}" }}, params = {{ wait = "0s", boot_ms = 1500, status_ms = 300, settle_ms = 120 }} }}
"#,
            psu.path
        ),
    )
    .unwrap();
    let out = run(
        dir.path(),
        &["--json", "source", "set", "psu", "12v", "2a", "--on"],
    );
    assert_eq!(out.code, 0, "{}", out.stdout);
    let d = out.data();
    assert_eq!(d["verified"]["voltage"], true, "{}", d["verified"]);
    assert_eq!(d["verified"]["current"], true);
    assert_eq!(d["verified"]["output"], true);
    assert!(close(&d["info"]["max_voltage"], 36.0));
    assert_eq!(
        run(dir.path(), &["--json", "source", "set", "psu", "37v"]).code,
        8
    );
}

const IPS3608_PARAMS: &str = r#", boot_ms = 1500, status_ms = 300, name = "IPS3608", status = { voltage_set = 4, current_set = 8, voltage = 12, current = 16, power = 20, output = 107 }"#;

#[test]
fn ips3608_confirms_setpoints_and_output_from_the_status() {
    let psu = FakePsu::start(Behavior::ips3608());
    let dir = bench_with(&psu.path, IPS3608_PARAMS);
    let out = run(
        dir.path(),
        &["--json", "source", "set", "psu", "5v", "1a", "--on"],
    );
    assert_eq!(out.code, 0, "{}", out.stdout);
    let d = out.data();
    assert_eq!(d["verified"]["voltage"], true, "{}", d["verified"]);
    assert_eq!(d["verified"]["current"], true);
    assert_eq!(d["verified"]["output"], true);
    assert!(close(&d["reading"]["voltage_set"], 5.0));
    assert!(close(&d["reading"]["voltage"], 5.0));
    assert_eq!(
        psu.log(),
        [
            "connect",
            "query name",   // identify before sending anything
            "query status", // core: output on + bench limits -> check the existing setpoints first
            "vset 5",
            "iset 1",
            "output 1",
            "query status", // read back and verify
            "disconnect",
        ]
    );

    // The output state is known without switching it in this session.
    let d = run(dir.path(), &["--json", "source", "read", "psu"]).data();
    assert_eq!(d["reading"]["output"], true);
}

#[test]
fn another_device_on_the_same_usb_id_is_refused() {
    let psu = FakePsu::start(Behavior {
        name: Some("SOMETHING"),
        ..Behavior::ips3608()
    });
    let dir = bench_with(&psu.path, IPS3608_PARAMS);
    let out = run(
        dir.path(),
        &["--json", "source", "set", "psu", "5v", "--on"],
    );
    assert_eq!(out.code, 5, "{}", out.stdout);
    let msg = out.json()["error"]["message"].as_str().unwrap().to_string();
    assert!(msg.contains(r#""SOMETHING", not a IPS3608"#), "{msg}");
    assert!(
        !psu.log()
            .iter()
            .any(|c| c.starts_with("vset") || c.starts_with("output")),
        "nothing was sent to the wrong device: {:?}",
        psu.log()
    );
}

#[test]
fn a_device_that_does_not_answer_the_name_query_is_refused() {
    let psu = FakePsu::start(Behavior {
        name: None,
        ..Behavior::ips3608()
    });
    let dir = bench_with(&psu.path, IPS3608_PARAMS);
    let out = run(dir.path(), &["--json", "source", "read", "psu"]);
    assert_eq!(out.code, 5);
    assert!(
        out.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("doesn't respond as a IPS3608")
    );
}

#[test]
fn a_device_that_is_still_booting_is_retried() {
    let psu = FakePsu::start(Behavior {
        booting: 1,
        ..Behavior::ips3608()
    });
    let dir = bench_with(&psu.path, IPS3608_PARAMS);
    let out = run(dir.path(), &["--json", "source", "read", "psu"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let queries = psu.log().iter().filter(|c| *c == "query name").count();
    assert_eq!(
        queries,
        2,
        "answered on the second attempt: {:?}",
        psu.log()
    );
}

/// The bundled IPS3608 definition against a fake IPS3608, with the timings of the definition.
fn bundled_ips3608(behavior: Behavior) -> (FakePsu, TempDir) {
    let psu = FakePsu::start(behavior);
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("desk.toml"),
        format!(
            r#"
[devices.psu]
model  = "fnirsi/ips3608"
source = {{ port = {{ path = "{}" }}, params = {{ wait = "0s", boot_ms = 1500, status_ms = 300, settle_ms = 120 }} }}
"#,
            psu.path
        ),
    )
    .unwrap();
    (psu, dir)
}

#[test]
fn ips3608_reports_everything_its_status_has() {
    let (psu, dir) = bundled_ips3608(Behavior::ips3608());
    let d = run(dir.path(), &["--json", "source", "read", "psu"]).data();
    let r = &d["reading"];
    assert_eq!(r["output"], false);
    assert!(r["mode"].is_null(), "no mode while the output is off: {r}");
    assert_eq!(r["protection"], "none");
    assert!(close(&r["temperature"], 31.5));
    assert!(
        close(&r["input_voltage"], 44.7),
        "from query 0xC0, not the constant 48: {r}"
    );
    assert!(close(&r["charge"], 0.25));
    assert!(close(&r["energy"], 1.5));
    assert!(close(&d["info"]["max_power"], 285.0));

    let out = run(
        dir.path(),
        &["--json", "source", "set", "psu", "5v", "1a", "--on"],
    );
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.data()["reading"]["mode"], "cv");

    let out = run(dir.path(), &["source", "read", "psu"]);
    assert!(
        out.stdout.contains("CV (constant voltage)"),
        "{}",
        out.stdout
    );
    assert!(out.stdout.contains("31.5 °C"), "{}", out.stdout);

    // Only documented frame types were sent (never 0xB0/0xC0: 0xC0 is the firmware-update mode).
    assert!(
        !psu.log().iter().any(|l| l.starts_with("unknown")),
        "{:?}",
        psu.log()
    );
}

#[test]
fn ips3608_keeps_quiet_after_every_write() {
    let (psu, dir) = bundled_ips3608(Behavior::ips3608());
    let out = run(
        dir.path(),
        &["--json", "source", "set", "psu", "5v", "1a", "--on"],
    );
    assert_eq!(out.code, 0, "{}", out.stdout);
    let log = psu.timed_log();
    let writes = ["connect", "vset", "iset", "output"];
    for pair in log.windows(2) {
        let ((first, _), (next, gap)) = (&pair[0], &pair[1]);
        if writes.iter().any(|w| first.starts_with(w)) {
            assert!(
                *gap >= Duration::from_millis(45),
                "{next} only {gap:?} after {first}: {log:?}"
            );
        }
    }
}

#[test]
fn ips3608_with_a_tripped_protection_stays_off_until_cleared() {
    let (psu, dir) = bundled_ips3608(Behavior {
        tripped: 2, // OCP
        ..Behavior::ips3608()
    });
    let d = run(dir.path(), &["--json", "source", "read", "psu"]).data();
    assert_eq!(d["reading"]["protection"], "ocp");

    let out = run(dir.path(), &["--json", "source", "output", "on", "psu"]);
    assert_eq!(
        out.json()["error"]["code"],
        "protection_tripped",
        "{}",
        out.stdout
    );
    assert!(
        !psu.log().iter().any(|l| l.starts_with("output")),
        "nothing sent: {:?}",
        psu.log()
    );

    let out = run(
        dir.path(),
        &[
            "--json",
            "source",
            "output",
            "on",
            "psu",
            "--clear-protection",
        ],
    );
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.data()["reading"]["protection"], "none");
    assert_eq!(out.data()["reading"]["output"], true);
}

/// Watching through the daemon: one connection for the whole watch, commands run inside it (the
/// port is exclusive, so a second open would fail), the panel is released when the watch ends.
#[test]
fn ips3608_watch_keeps_one_session_and_commands_join_it() {
    use std::io::{BufRead, BufReader};

    let (psu, dir) = bundled_ips3608(Behavior::ips3608());
    let desk = |args: &[&str]| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_desk"));
        c.env_remove("DESK_CONFIG")
            .env("DESK_RUNTIME_DIR", dir.path().join("run"))
            .env("DESK_DAEMON_IDLE", "1s")
            .arg("--config")
            .arg(dir.path().join("desk.toml"))
            .args(args);
        c
    };
    let mut watch = {
        let _g = FORK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        desk(&["--json", "source", "watch", "psu", "--interval", "100ms"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    };
    let mut lines = BufReader::new(watch.stdout.take().unwrap()).lines();
    let mut next =
        || -> Value { serde_json::from_str(&lines.next().expect("a line").unwrap()).unwrap() };
    let first = next();
    assert_eq!(first["ok"], true, "{first}");
    assert_eq!(first["data"]["reading"]["output"], false);
    assert!(close(&first["data"]["reading"]["input_voltage"], 44.7));

    let set = {
        let _g = FORK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        desk(&["--json", "source", "set", "psu", "5v", "1a", "--on"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    };
    let set = set.wait_with_output().unwrap();
    let out = String::from_utf8_lossy(&set.stdout);
    assert_eq!(set.status.code(), Some(0), "{out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["data"]["verified"]["output"], true, "{out}");

    let mut on = false;
    for _ in 0..30 {
        let r = next();
        if r["data"]["reading"]["output"] == true
            && close(&r["data"]["reading"]["voltage_set"], 5.0)
        {
            on = true;
            break;
        }
    }
    assert!(on, "the watch never showed the output on");

    let _ = watch.kill();
    let _ = watch.wait();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !psu.log().contains(&"disconnect".to_string()) {
        assert!(
            Instant::now() < deadline,
            "never disconnected: {:?}",
            psu.log()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let log = psu.log();
    let count = |what: &str| log.iter().filter(|l| *l == what).count();
    assert_eq!(count("connect"), 1, "{log:?}");
    assert_eq!(count("disconnect"), 1, "{log:?}");
    let at = |what: &str| log.iter().position(|l| l == what).unwrap();
    assert!(
        at("connect") < at("vset 5") && at("output 1") < at("disconnect"),
        "{log:?}"
    );
}

/// One lost status reply during a watch is retried, counted, and doesn't end the stream.
#[test]
fn ips3608_watch_survives_a_lost_reply() {
    let (_psu, dir) = bundled_ips3608(Behavior {
        skip_status: Some(4),
        ..Behavior::ips3608()
    });
    let out = {
        let _g = FORK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        Command::new(env!("CARGO_BIN_EXE_desk"))
            .env_remove("DESK_CONFIG")
            .env("DESK_RUNTIME_DIR", dir.path().join("run"))
            .env("DESK_DAEMON_IDLE", "1s")
            .arg("--config")
            .arg(dir.path().join("desk.toml"))
            .args([
                "--json",
                "source",
                "watch",
                "psu",
                "--interval",
                "50ms",
                "--count",
                "6",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }
    .wait_with_output()
    .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    let lines: Vec<Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 6, "{stdout}");
    assert!(lines.iter().all(|l| l["ok"] == true), "{stdout}");
    assert_eq!(lines.last().unwrap()["data"]["failed"], 1, "{stdout}");
}

/// Reads return as soon as the replies are in (not after a read timeout), and values come out as
/// the device means them (44.7, not 44.70000076).
#[test]
fn ips3608_reads_fast_and_rounds_floats() {
    let (_psu, dir) = bundled_ips3608(Behavior {
        telemetry: false,
        ..Behavior::ips3608()
    });
    let out = {
        let _g = FORK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        Command::new(env!("CARGO_BIN_EXE_desk"))
            .env_remove("DESK_CONFIG")
            .env("DESK_RUNTIME_DIR", dir.path().join("run"))
            .env("DESK_DAEMON_IDLE", "1s")
            .arg("--config")
            .arg(dir.path().join("desk.toml"))
            .args([
                "--json",
                "source",
                "watch",
                "psu",
                "--interval",
                "0",
                "--count",
                "11",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }
    .wait_with_output()
    .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    let lines: Vec<Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let t = |i: usize| lines[i]["data"]["elapsed"].as_f64().unwrap();
    // Two queries per reading; waiting out 50 ms read timeouts took ~1 s for these ten.
    assert!(t(10) - t(0) < 0.5, "10 readings took {:.3} s", t(10) - t(0));
    let r = &lines[0]["data"]["reading"];
    assert_eq!(r["input_voltage"], 44.7, "{r}");
    assert_eq!(r["temperature"], 31.5, "{r}");
}
