//! Streams from sigrok devices (`ctx.sigrok.acquire{ continuous = true, into = ctx.output }`),
//! end to end through the daemon against the fake `sigrok-cli`: the exact command line, the
//! payload from its stdout, a bounded acquisition, two subscribers on one acquisition, no child
//! left behind, failures, and `continuous` refused for a file. The last test streams from the
//! real `sigrok-cli -d demo` when it is installed.
//!
//! Unix only, like tests/sigrok.rs: the fake and these tests are not yet tried on Windows.
#![cfg(unix)]

#[path = "support/sigrok.rs"]
mod sigrok;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use sigrok::SigrokBench;

const LA: &str = r#"
[devices.la.capture]
script  = "la.lua"
sigrok  = { driver = "demo", backend = "fake" }
timeout = "2s"

[devices.la.capture.actions.grab]
switches = false
produces = "file"

[devices.la.capture.streams.samples]
content = "logic"

[devices.la.capture.streams.samples.args]
samplerate = { type = "number", unit = "Hz", default = 10e6 }
channels   = { type = "string", default = "D0,D1,D2,D3,D4,D5,D6,D7" }
samples    = { type = "number", optional = true, doc = "a bounded acquisition" }
"#;

const LA_LUA: &str = r#"
PARAMS = {}

local function list(text)
  local out = {}
  for c in string.gmatch(text, "[^,]+") do out[#out + 1] = c end
  return out
end

function stream(ctx, name, args)
  local channels = list(args.channels)
  ctx.describe{
    into = ctx.output,
    unit_size = math.ceil(#channels / 8),
    samplerate = args.samplerate,
    channels = channels,
  }
  local r = { config = { samplerate = args.samplerate }, channels = channels, into = ctx.output }
  if args.samples then r.samples = args.samples else r.continuous = true end
  ctx.sigrok.acquire(r)
end

function action(ctx, name, args)
  -- A file can't be filled continuously.
  ctx.sigrok.acquire{ continuous = true, into = ctx.output, timeout = "1s" }
  return { format = "binary" }
end
"#;

const DEMO: &str = "demo - Demo device with 8 channels: D0 D1 D2 D3 D4 D5 D6 D7";

fn la_bench() -> SigrokBench {
    let b = SigrokBench::new(LA);
    std::fs::write(b.path("la.lua"), LA_LUA).unwrap();
    b.fake.devices(DEMO);
    b
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Bytes of the fake's pattern from byte `offset` on.
fn pattern(offset: u64, n: usize) -> Vec<u8> {
    (offset..offset + n as u64)
        .map(|i| (i % 251) as u8)
        .collect()
}

struct Running {
    child: Child,
    data: Arc<Mutex<Vec<u8>>>,
}

impl Running {
    fn start(mut c: Command) -> Running {
        let mut child = c
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut out = child.stdout.take().unwrap();
        let data = Arc::new(Mutex::new(Vec::new()));
        let sink = data.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 1 << 16];
            while let Ok(n) = out.read(&mut buf) {
                if n == 0 {
                    return;
                }
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        Running { child, data }
    }

    fn bytes(&self) -> Vec<u8> {
        self.data.lock().unwrap().clone()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `--frames` output: (offset, payload) of every data frame, and the end frame's envelope.
fn frames(out: &[u8]) -> (Vec<(u64, Vec<u8>)>, Option<Value>) {
    let le = |b: &[u8]| {
        let mut a = [0u8; 8];
        a[..b.len()].copy_from_slice(b);
        u64::from_le_bytes(a)
    };
    let nl = out.iter().position(|c| *c == b'\n').unwrap();
    let mut rest = &out[nl + 1..];
    let (mut data, mut end) = (Vec::new(), None);
    while rest.len() >= 32 {
        let len = le(&rest[8..12]) as usize;
        if rest.len() < 32 + len {
            break;
        }
        let payload = rest[32..32 + len].to_vec();
        if rest[4] == 1 {
            end = serde_json::from_slice(&payload).ok();
        } else {
            data.push((le(&rest[20..28]), payload));
        }
        rest = &rest[32 + len..];
    }
    (data, end)
}

#[test]
fn a_continuous_stream_reads_sigrok_cli_s_stdout() {
    let b = la_bench();
    let out = b
        .cmd(&["stream", "la", "samples", "--bytes", "300000"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, pattern(0, 300_000));
    assert_eq!(
        b.fake.streams(),
        [[
            "-d",
            "demo",
            "--config",
            "samplerate=10000000",
            "--channels",
            "D0,D1,D2,D3,D4,D5,D6,D7",
            "--continuous",
            "-O",
            "binary"
        ]]
    );
    // A channel subset: its bits are packed in the order given.
    let out = b.run(&[
        "--json",
        "stream",
        "la",
        "samples",
        "channels=D3,D7",
        "--bytes",
        "10",
        "--header",
        "--discard",
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let header: Value = serde_json::from_str(out.stdout.lines().next().unwrap()).unwrap();
    assert_eq!(header["data"]["channels"], serde_json::json!(["D3", "D7"]));
    assert_eq!(header["data"]["unit_size"], 1);
    assert_eq!(b.fake.streams()[1][5], "D3,D7");
}

#[test]
fn a_bounded_acquisition_ends_the_stream_normally() {
    let b = la_bench();
    let out = b.run(&[
        "--json",
        "stream",
        "la",
        "samples",
        "samples=5000",
        "--discard",
    ]);
    assert_eq!(out.code, 0, "{}\n{}", out.stdout, out.stderr);
    let s = out.json();
    assert_eq!(s["data"]["ended"], "source");
    assert_eq!(s["data"]["bytes"], 5000);
    assert_eq!(s["data"]["end"]["units"], 5000);
    let call = &b.fake.streams()[0];
    assert_eq!(call[6..], ["--samples", "5000", "-O", "binary"]);
}

#[test]
fn two_subscribers_share_one_acquisition() {
    let b = la_bench();
    b.fake.behaviour("rate=2000000");
    let first = Running::start(b.cmd(&["stream", "la", "samples", "--frames"]));
    wait_until("data", || first.bytes().len() > 200_000);
    let second = b
        .cmd(&["stream", "la", "samples", "--frames", "--duration", "500ms"])
        .output()
        .unwrap();
    assert!(second.status.success());
    let (data, _) = frames(&second.stdout);
    assert!(data[0].0 > 0, "it joined later");
    for (offset, payload) in &data {
        assert_eq!(*payload, pattern(*offset, payload.len()));
    }
    let (mine, _) = frames(&first.bytes());
    for (offset, payload) in &mine {
        assert_eq!(*payload, pattern(*offset, payload.len()));
    }
    assert_eq!(b.fake.streams().len(), 1, "one sigrok-cli for both");
}

/// Whether a process (not a zombie) with this pid exists.
fn alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[test]
fn when_the_last_subscriber_leaves_sigrok_cli_is_gone() {
    let b = la_bench();
    b.fake.behaviour("rate=1000000");
    let s = Running::start(b.cmd(&["stream", "la", "samples", "--frames"]));
    wait_until("data", || s.bytes().len() > 100_000);
    let pid = b.fake.stream_pids()[0].to_string();
    assert!(alive(&pid));
    drop(s);
    wait_until("sigrok-cli to be killed and reaped", || !alive(&pid));
    // The device is free: a capture of its own works again.
    wait_until("the device to be free", || {
        b.run(&["stream", "la", "samples", "samples=10", "--discard"])
            .code
            == 0
    });
}

#[test]
fn a_failing_or_busy_sigrok_ends_the_stream_with_its_error() {
    let b = la_bench();
    b.fake.behaviour("acquire=busy");
    let out = b.run(&["--json", "stream", "la", "samples", "--discard"]);
    assert_eq!(out.code, 4, "{}", out.stdout);
    let v = out.json();
    assert_eq!(v["error"]["code"], "endpoint_unavailable", "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("busy"));

    b.fake
        .behaviour("acquire=fail\nmessage=sr: no samples for you\nexit=3");
    let out = b.run(&["--json", "stream", "la", "samples", "--discard"]);
    assert_eq!(out.code_str(), "script_failed", "{}", out.stdout);
    assert!(
        out.message().contains("no samples for you"),
        "{}",
        out.message()
    );
    assert!(out.message().contains("exit 3"), "{}", out.message());
}

#[test]
fn continuous_only_fills_a_stream() {
    let b = la_bench();
    let out = b.run(&["--json", "do", "la", "grab"]);
    assert_eq!(out.code_str(), "script_failed", "{}", out.stdout);
    assert!(
        out.message().contains("only fills a stream"),
        "{}",
        out.message()
    );
    assert!(b.fake.streams().is_empty() && b.fake.captures().is_empty());
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
fn the_example_streams_from_the_real_demo_driver() {
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
        .env("DESK_DAEMON_IDLE", "1s")
        .arg("--config")
        .arg(&example)
        .args([
            "--json",
            "stream",
            "la",
            "samples",
            "samplerate=1MHz",
            "--discard",
            "--header",
            "--duration",
            "1500ms",
        ])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lines: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines[0]["data"]["channels"].as_array().unwrap().len(), 8);
    assert_eq!(lines[0]["data"]["samplerate"], 1e6);
    // 1 MHz, one byte per sample, for about a second once sigrok runs.
    let bytes = lines[1]["data"]["bytes"].as_u64().unwrap();
    assert!((200_000..2_000_000).contains(&bytes), "{bytes} bytes");
    assert_eq!(lines[1]["data"]["dropped"], 0);
}
