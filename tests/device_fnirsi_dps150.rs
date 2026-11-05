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
}

impl Default for Behavior {
    fn default() -> Self {
        Behavior {
            echo_output: true,
            telemetry: true,
            garbage: false,
        }
    }
}

#[derive(Default)]
struct State {
    connected: bool,
    vset: f32,
    iset: f32,
    output: bool,
    /// Commands received, in order.
    log: Vec<String>,
    bad_frames: usize,
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

        let state = Arc::new(Mutex::new(State::default()));
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
                        (0xB1, 0xDB) if len == 1 => {
                            s.output = data[0] == 1;
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
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("desk.toml"),
        format!(
            r#"
[devices.psu]
source = {{ script = "{}", port = {{ path = "{path}" }}, timeout = "6s", params = {{ max_voltage = 36.0, max_current = 8.0, telemetry_ms = 800, settle_ms = 120, echo_ms = 300, wait = "0s" }} }}
limits = {{ voltage = 24 }}
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
fn definitions_reference_the_shared_driver() {
    // Until M5 loads definitions, at least keep them well-formed and pointing at real files.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("devices/fnirsi");
    for model in ["ips3608", "dps-150"] {
        let dir = root.join(model);
        let text = std::fs::read_to_string(dir.join("device.toml")).unwrap();
        let def: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(
            def["device"]["id"].as_str(),
            Some(format!("fnirsi/{model}").as_str())
        );
        let script = def["source"]["script"].as_str().unwrap();
        assert!(dir.join(script).exists(), "{model}: {script} missing");
        assert!(def["source"]["params"]["max_voltage"].as_float().is_some());
    }
}
