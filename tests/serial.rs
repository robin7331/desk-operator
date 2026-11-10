//! End-to-end tests of the serial host API (`ctx.serial`) over a pseudo-terminal pair: the test
//! plays the device on the master side, `desk` opens the slave side like a real port.
// Linux only: on macOS the serial crate sets the speed with an ioctl that a pty refuses (ENOTTY).
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use serialport::{SerialPort, TTYPort};
use tempfile::TempDir;

/// Tests run in parallel threads, and children inherit open file descriptors. If one test forks
/// `desk` while another test briefly holds its pty slave open, the child inherits that slave and
/// its exclusive lock. So creating ptys and spawning children never overlap. (Test-only: desk
/// itself never spawns processes.)
static FORK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A fake device: echoes everything back, with ASCII letters upper-cased.
struct FakeDevice {
    path: String,
    stop: Arc<AtomicBool>,
}

impl FakeDevice {
    fn start() -> Self {
        let guard = FORK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (mut master, mut slave) = TTYPort::pair().expect("pty pair");
        let path = slave.name().expect("pty name");
        slave.set_exclusive(false).unwrap();
        drop(slave); // desk opens it by path
        drop(guard);
        master.set_timeout(Duration::from_millis(20)).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 256];
            while !stop2.load(Ordering::Relaxed) {
                match master.read(&mut buf) {
                    Ok(n) if n > 0 => {
                        let reply: Vec<u8> =
                            buf[..n].iter().map(|b| b.to_ascii_uppercase()).collect();
                        let _ = master.write_all(&reply);
                    }
                    // No slave open yet (EIO), timeouts: keep waiting.
                    _ => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        FakeDevice { path, stop }
    }
}

impl Drop for FakeDevice {
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
    fn log(&self, i: usize) -> String {
        self.json()["data"]["results"][0]["log"][i]
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
}

/// One device `dev` whose power path is `script`, bound to `port` (a TOML table or "" for none).
fn bench(script: &str, port: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("dev.lua"), script).unwrap();
    let port = if port.is_empty() {
        String::new()
    } else {
        format!(", port = {port}")
    };
    std::fs::write(
        dir.path().join("desk.toml"),
        format!("[devices.dev]\npower = {{ script = \"dev.lua\"{port} }}\n"),
    )
    .unwrap();
    dir
}

fn run(dir: &Path, args: &[&str]) -> Out {
    let child = {
        let _guard = FORK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        Command::new(env!("CARGO_BIN_EXE_desk"))
            .env_remove("DESK_CONFIG")
            .env("DESK_MOCK_STATE", dir.join("mock-state.json"))
            .arg("--config")
            .arg(dir.join("desk.toml"))
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    let out = child.wait_with_output().unwrap();
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
    }
}

fn path_port(dev: &FakeDevice) -> String {
    format!("{{ path = \"{}\" }}", dev.path)
}

fn script(on_body: &str) -> String {
    format!("function on(ctx)\n{on_body}\nend\nfunction off(ctx) end\n")
}

#[test]
fn text_round_trip() {
    let dev = FakeDevice::start();
    let dir = bench(
        &script(
            r#"
            local port = ctx.serial.open{ baud = 115200 }
            port:write("ping\n")
            ctx.log(port:read(5, 1000))
            ctx.log(port.path)
            port:close()"#,
        ),
        &path_port(&dev),
    );
    let out = run(dir.path(), &["--json", "power", "on", "dev"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.log(0), "PING\n");
    assert_eq!(out.log(1), dev.path);
}

#[test]
fn binary_frames_with_string_pack() {
    let dev = FakeDevice::start();
    let dir = bench(
        &script(
            r#"
            local port = ctx.serial.open{ baud = 115200 }
            local frame = string.pack("<BBf", 0xF1, 0xC1, 5.0)
            port:write(frame)
            local reply = port:read(#frame, 1000)
            local a, b, v = string.unpack("<BBf", reply)
            ctx.log(#reply, a, b, v)"#,
        ),
        &path_port(&dev),
    );
    let out = run(dir.path(), &["--json", "power", "on", "dev"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    // 0xF1/0xC1 and the float's bytes are not ASCII letters, so they come back unchanged.
    assert_eq!(out.log(0), "6 241 193 5.0");
}

#[test]
fn a_huge_read_count_is_capped() {
    let dev = FakeDevice::start();
    let dir = bench(
        &script(
            r#"
            local port = ctx.serial.open{}
            local t = port:read(1099511627776, 100)
            ctx.log(#t)"#,
        ),
        &path_port(&dev),
    );
    let out = run(dir.path(), &["--json", "power", "on", "dev"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.log(0), "0");
}

#[test]
fn read_returns_what_arrived_within_the_timeout() {
    let dev = FakeDevice::start();
    let dir = bench(
        &script(
            r#"
            local port = ctx.serial.open{}
            local t = port:read(10, 200)
            ctx.log("[" .. t .. "]", #t)
            port:write("ab")
            ctx.log(port:read(10, 300))"#,
        ),
        &path_port(&dev),
    );
    let start = Instant::now();
    let out = run(dir.path(), &["--json", "power", "on", "dev"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.log(0), "[] 0", "nothing arrived");
    assert_eq!(
        out.log(1),
        "AB",
        "fewer bytes than asked for, returned at the timeout"
    );
    assert!(start.elapsed() < Duration::from_secs(3));
}

#[test]
fn clear_discards_pending_input() {
    let dev = FakeDevice::start();
    let dir = bench(
        &script(
            r#"
            local port = ctx.serial.open{}
            port:write("junk")
            ctx.sleep(150)
            port:clear()
            port:write("ok")
            ctx.log(port:read(10, 300))"#,
        ),
        &path_port(&dev),
    );
    let out = run(dir.path(), &["--json", "power", "on", "dev"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.log(0), "OK");
}

#[test]
fn ports_are_exclusive() {
    let dev = FakeDevice::start();
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("hold.lua"),
        script("local port = ctx.serial.open{}\nctx.sleep(400)"),
    )
    .unwrap();
    let port = path_port(&dev);
    std::fs::write(
        dir.path().join("desk.toml"),
        format!(
            "[devices.a]\npower = {{ script = \"hold.lua\", port = {port} }}\n\
             [devices.b]\npower = {{ script = \"hold.lua\", port = {port} }}\n"
        ),
    )
    .unwrap();
    let is_root = Command::new("id")
        .arg("-u")
        .output()
        .map(|o| o.stdout == b"0\n")
        .unwrap_or(false);
    let out = run(dir.path(), &["--json", "power", "on", "a", "b"]);
    if is_root {
        return; // root bypasses TIOCEXCL
    }
    let results = out.json()["data"]["results"].as_array().unwrap().clone();
    let failed: Vec<&Value> = results.iter().filter(|r| r["ok"] == false).collect();
    assert_eq!(
        failed.len(),
        1,
        "exactly one of two users of one port wins: {results:?}"
    );
    assert_eq!(failed[0]["error"]["code"], "endpoint_unavailable");
    assert!(
        failed[0]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("busy"),
        "{failed:?}"
    );
}

#[test]
fn binding_without_port() {
    let dir = bench(&script("ctx.serial.open{}"), "");
    let out = run(dir.path(), &["--json", "power", "on", "dev"]);
    assert_eq!(out.code, 3);
    assert!(out.message().contains("has no `port"), "{}", out.message());
}

#[test]
fn missing_ports() {
    let dir = bench(
        &script("ctx.serial.open{}"),
        "{ path = \"/dev/desk-test-nonexistent\" }",
    );
    let out = run(dir.path(), &["--json", "power", "on", "dev"]);
    assert_eq!(out.code, 4, "{}", out.stdout);
    assert_eq!(out.json()["error"]["code"], "endpoint_unavailable");

    let dir = bench(
        &script("ctx.serial.open{}"),
        "{ usb_vid = 0xfffe, usb_pid = 0xfffd }",
    );
    let out = run(dir.path(), &["--json", "power", "on", "dev"]);
    assert_eq!(out.code, 4);
    assert!(
        out.message().contains("desk port list"),
        "{}",
        out.message()
    );
}

#[test]
fn wait_keeps_looking_for_the_port() {
    let dir = bench(
        &script("ctx.serial.open{ wait = \"600ms\" }"),
        "{ usb_vid = 0xfffe, usb_pid = 0xfffd }",
    );
    let start = Instant::now();
    let out = run(dir.path(), &["--json", "power", "on", "dev"]);
    assert_eq!(out.code, 4);
    assert!(
        start.elapsed() >= Duration::from_millis(600),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn bad_open_options_and_port_specs() {
    let dev = FakeDevice::start();
    let dir = bench(&script("ctx.serial.open{ bauds = 9600 }"), &path_port(&dev));
    let out = run(dir.path(), &["--json", "power", "on", "dev"]);
    assert_eq!(out.code, 5);
    assert!(
        out.message().contains("unsupported option `bauds`"),
        "{}",
        out.message()
    );

    let dir = bench(&script(""), "{ vid = 1 }");
    let out = run(dir.path(), &["--json", "device", "list"]);
    assert_eq!(out.code, 3);
    assert!(
        out.message().contains("unknown key `port.vid`"),
        "{}",
        out.message()
    );
}

#[test]
fn device_views_and_port_list() {
    let dev = FakeDevice::start();
    let dir = bench(&script(""), &path_port(&dev));
    let out = run(dir.path(), &["device", "show", "dev"]);
    assert!(
        out.stdout.contains(&format!("on {}", dev.path)),
        "{}",
        out.stdout
    );

    let out = run(dir.path(), &["--json", "port", "list"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert!(out.json()["data"]["ports"].is_array());
    assert!(out.json()["data"]["hidden"].is_number());
}

#[test]
fn probe_sends_hex_and_dumps_replies() {
    let dev = FakeDevice::start();
    let dir = bench(&script(""), "");
    let out = run(
        dir.path(),
        &[
            "--json",
            "port",
            "probe",
            &dev.path,
            "--baud",
            "115200",
            "--send",
            "61 62 0A",
            "--send",
            "0x7a,0x00",
            "--listen",
            "300ms",
        ],
    );
    assert_eq!(out.code, 0, "{}", out.stdout);
    let d = &out.json()["data"];
    assert_eq!(d["port"], dev.path.as_str());
    assert_eq!(d["baud"], 115200);
    let steps = d["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0]["sent"], "61 62 0A");
    assert_eq!(
        steps[0]["received"], "41 42 0A",
        "the fake upper-cases letters"
    );
    assert_eq!(steps[0]["ascii"], "AB.");
    assert_eq!(steps[1]["received"], "5A 00");
    let chunk = &steps[0]["chunks"][0];
    assert_eq!(chunk["hex"], "41 42 0A");
    assert!(chunk["at_ms"].as_u64().unwrap() < 300);
}

#[test]
fn probe_text_and_listen_only() {
    let dev = FakeDevice::start();
    let dir = bench(&script(""), "");
    let out = run(
        dir.path(),
        &[
            "--json", "port", "probe", &dev.path, "--text", "--send", "*idn?\\n", "--listen",
            "300ms",
        ],
    );
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.json()["data"]["steps"][0]["ascii"], "*IDN?.");

    // Without --send it only listens; the fake sends nothing unprompted.
    let start = Instant::now();
    let out = run(
        dir.path(),
        &["--json", "port", "probe", &dev.path, "--listen", "300ms"],
    );
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert!(start.elapsed() >= Duration::from_millis(300));
    let step = &out.json()["data"]["steps"][0];
    assert!(step["sent"].is_null());
    assert_eq!(step["received"], "");
    assert_eq!(step["chunks"].as_array().unwrap().len(), 0);

    // Human output shows what went where.
    let out = run(
        dir.path(),
        &[
            "port", "probe", &dev.path, "--send", "61", "--listen", "200ms",
        ],
    );
    assert!(out.stdout.contains("> 61"), "{}", out.stdout);
    assert!(out.stdout.contains("ms  41"), "{}", out.stdout);
}

#[test]
fn probe_refuses_bad_arguments_and_missing_ports() {
    let dir = bench(&script(""), "");
    for (args, msg) in [
        (&["/dev/null", "--send", "F1 A"][..], "odd number"),
        (&["/dev/null", "--send", "zz"][..], "not hex"),
        (&["/dev/null", "--text", "--send", "\\q"][..], "bad escape"),
        (&["/dev/null", "--line", "8X1"][..], "--line"),
        (&["/dev/null", "--listen", "2m"][..], "at most"),
        (&["--usb", "2e3c"][..], "VID:PID"),
        (&["--baud", "9600"][..], "name the port"),
    ] {
        let mut full = vec!["--json", "port", "probe"];
        full.extend_from_slice(args);
        let out = run(dir.path(), &full);
        assert_eq!(out.code, 2, "{args:?}: {}", out.stdout);
        assert_eq!(out.json()["error"]["code"], "usage");
        assert!(out.message().contains(msg), "{args:?}: {}", out.message());
    }

    let out = run(
        dir.path(),
        &[
            "--json",
            "port",
            "probe",
            "/dev/desk-no-such-port",
            "--send",
            "00",
        ],
    );
    assert_eq!(out.json()["error"]["code"], "endpoint_unavailable");
    let out = run(
        dir.path(),
        &[
            "--json",
            "port",
            "probe",
            "--usb",
            "fffe:fffe",
            "--serial-number",
            "nope",
        ],
    );
    assert_eq!(out.json()["error"]["code"], "endpoint_unavailable");
}
