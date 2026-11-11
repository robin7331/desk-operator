//! End-to-end tests of binary streams (`desk stream`) through the daemon against the mock source:
//! the header, the payload pattern, frames with their `seq`/`offset`, several subscribers, a late
//! joiner, refused arguments, a slow subscriber, endings (source ends, fails, stalls, daemon
//! stops), a busy device, other devices, idle exit, `--bytes`, `--duration`, `--discard`.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

/// The mock source: unit `i` is the little-endian `unit_size`-byte value of `i % 251`.
const STREAM_ARGS: &str = r#"
unit_size   = { type = "number", default = 1 }
rate        = { type = "number", optional = true, doc = "units per second" }
units       = { type = "number", optional = true }
fail_after  = { type = "number", optional = true }
stall_after = { type = "number", optional = true }
"#;

fn config() -> String {
    format!(
        r#"
[providers.sim]
kind = "mock"

[devices.la.capture]
provider = "sim"
endpoint = "la"
timeout  = "2s"

[devices.la.capture.values]
last_action = {{ type = "string" }}

[devices.la.capture.actions.reset]
switches = false

[devices.la.capture.streams.samples]
content = "logic"
doc     = "live samples"

[devices.la.capture.streams.samples.args]
{STREAM_ARGS}

[devices.la.capture.streams.raw]
content = "bytes"

[devices.la.capture.streams.raw.args]
unit_size = {{ type = "number", default = 1 }}
units     = {{ type = "number" }}

[devices.other.capture]
provider = "sim"
endpoint = "other"

[devices.other.capture.values]
last_action = {{ type = "string" }}

[devices.other.capture.streams.samples]
content = "logic"

[devices.other.capture.streams.samples.args]
{STREAM_ARGS}

[devices.broken.capture]
provider = "sim"
endpoint = "fail_la"

[devices.broken.capture.streams.samples]
content = "logic"

[devices.powered]
power = {{ provider = "sim", endpoint = "powered_mains" }}

[devices.powered.capture]
provider = "sim"
endpoint = "powered"

[devices.powered.capture.streams.samples]
content = "logic"

[devices.powered.capture.streams.samples.args]
{STREAM_ARGS}

[devices.lua.capture]
script  = "stream.lua"
timeout = "2s"

[devices.lua.capture.streams.text]
content = "text"
args = {{ mode = {{ type = "choice", choices = ["ok", "twice", "early", "function", "nochannels", "silent", "nothing"], default = "ok" }} }}

[devices.lua.capture.actions.grab]
switches = false
produces = "file"

[devices.lua.capture.streams.logic]
content = "logic"
args = {{ mode = {{ type = "choice", choices = ["nochannels"], default = "nochannels" }} }}
"#
    )
}

/// A Lua driver: `ok` describes its data and appends eight lines (with pauses longer than the
/// capability's timeout in total); the other modes break a rule each.
const STREAM_LUA: &str = r#"
function stream(ctx, name, args)
  local m = args.mode
  if m == "early" then ctx.append{ into = ctx.output, data = "x" } end
  if m == "nothing" then return end
  if m == "function" then
    ctx.describe{ into = ctx.output, f = function() end }
  elseif m == "nochannels" then
    ctx.describe{ into = ctx.output, unit_size = 1, samplerate = 1000 }
  else
    ctx.describe{ into = ctx.output, encoding = "utf-8" }
  end
  if m == "twice" then ctx.describe{ into = ctx.output } end
  if m == "silent" then ctx.sleep(5000) end
  for i = 1, 8 do
    ctx.append{ into = ctx.output, data = "line " .. i .. "\n" }
    ctx.sleep(300)
  end
end

-- A capture's file is not described.
function action(ctx, name, args)
  ctx.describe{ into = ctx.output }
  return { format = "txt" }
end"#;

struct Bench {
    dir: TempDir,
    /// `$DESK_STREAM_QUEUE` for the daemon this bench starts.
    queue: Option<&'static str>,
}

struct Out {
    code: i32,
    stdout: Vec<u8>,
    stderr: String,
}

impl Out {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
    /// stdout as one JSON document.
    fn json(&self) -> Value {
        serde_json::from_slice(&self.stdout).unwrap_or_else(|e| {
            panic!(
                "stdout is not JSON ({e}):\n{}\n{}",
                self.text(),
                self.stderr
            )
        })
    }
    /// The first JSON document on stdout (a pretty envelope or a JSON line).
    fn first(&self) -> Value {
        serde_json::Deserializer::from_slice(&self.stdout)
            .into_iter::<Value>()
            .next()
            .and_then(Result::ok)
            .unwrap_or_else(|| panic!("no JSON on stdout:\n{}\n{}", self.text(), self.stderr))
    }
    /// stdout as JSON lines.
    fn lines(&self) -> Vec<Value> {
        self.text()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}")))
            .collect()
    }
}

impl Bench {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("desk.toml"), config()).unwrap();
        std::fs::write(dir.path().join("stream.lua"), STREAM_LUA).unwrap();
        Bench { dir, queue: None }
    }

    /// A bench whose daemon lets a subscriber queue only `size` before it loses blocks.
    fn with_queue(size: &'static str) -> Self {
        Bench {
            queue: Some(size),
            ..Bench::new()
        }
    }

    fn runtime(&self) -> PathBuf {
        self.dir.path().join("run")
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let dir = self.dir.path();
        let mut c = Command::new(env!("CARGO_BIN_EXE_desk"));
        c.env_remove("DESK_CONFIG")
            .env("DESK_MOCK_STATE", dir.join("mock-state.json"))
            .env("DESK_RUNTIME_DIR", self.runtime())
            .env("DESK_DAEMON_IDLE", "1s")
            // A silent source ends after 500 ms.
            .env("DESK_STREAM_STALL", "500ms")
            .env("DESK_CAPTURE_DIR", dir.join("captures"))
            .arg("--config")
            .arg(dir.join("desk.toml"))
            .args(args)
            .stdin(Stdio::null());
        match self.queue {
            Some(q) => c.env("DESK_STREAM_QUEUE", q),
            None => c.env_remove("DESK_STREAM_QUEUE"),
        };
        c
    }

    fn run(&self, args: &[&str]) -> Out {
        let out = self.cmd(args).output().unwrap();
        Out {
            code: out.status.code().unwrap_or(-1),
            stdout: out.stdout,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// Start a command in the background; its stdout is collected once [`Running::read`] runs.
    fn spawn(&self, args: &[&str]) -> Running {
        let child = self
            .cmd(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Running {
            child,
            data: Arc::new(Mutex::new(Vec::new())),
            reading: false,
        }
    }

    fn daemon_running(&self) -> bool {
        let Ok(entries) = std::fs::read_dir(self.runtime()) else {
            return false;
        };
        entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "lock"))
            .any(|p| {
                std::fs::File::options()
                    .read(true)
                    .write(true)
                    .open(p)
                    .is_ok_and(|f| matches!(f.try_lock(), Err(std::fs::TryLockError::WouldBlock)))
            })
    }

    #[cfg(unix)]
    fn socket(&self) -> Option<PathBuf> {
        std::fs::read_dir(self.runtime())
            .ok()?
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|e| e == "sock"))
    }
}

struct Running {
    child: Child,
    data: Arc<Mutex<Vec<u8>>>,
    reading: bool,
}

impl Running {
    /// Start collecting stdout (until then nothing is read: a slow subscriber).
    fn read(&mut self) {
        if self.reading {
            return;
        }
        self.reading = true;
        let mut out = self.child.stdout.take().unwrap();
        let data = self.data.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 1 << 20];
            loop {
                match out.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => data.lock().unwrap().extend_from_slice(&buf[..n]),
                }
            }
        });
    }

    fn bytes(&self) -> Vec<u8> {
        self.data.lock().unwrap().clone()
    }

    /// The header line, once it is there.
    fn header(&mut self) -> Value {
        self.read();
        wait_until("the header line", || self.bytes().contains(&b'\n'));
        let b = self.bytes();
        let end = b.iter().position(|c| *c == b'\n').unwrap();
        serde_json::from_slice(&b[..end]).unwrap()
    }

    /// Wait for the exit; returns the code and everything on stdout.
    fn finish(mut self) -> (i32, Vec<u8>) {
        self.read();
        let deadline = Instant::now() + Duration::from_secs(15);
        let code = loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                break s.code().unwrap_or(-1);
            }
            assert!(Instant::now() < deadline, "did not exit within 15 s");
            std::thread::sleep(Duration::from_millis(20));
        };
        std::thread::sleep(Duration::from_millis(200));
        (code, self.bytes())
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `units` units of the pattern from unit `offset` on.
fn pattern(offset: u64, units: usize, unit: usize) -> Vec<u8> {
    (offset..offset + units as u64)
        .flat_map(|i| (i % 251).to_le_bytes().into_iter().take(unit))
        .collect()
}

/// One frame of `--frames` output.
#[derive(Debug)]
struct Frame {
    kind: u8,
    seq: u64,
    offset: u64,
    dropped: u32,
    payload: Vec<u8>,
}

fn le(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a[..b.len()].copy_from_slice(b);
    u64::from_le_bytes(a)
}

/// Split `--frames` output into the header line and the frames (a cut-off last frame is left
/// out).
fn frames(out: &[u8]) -> (Value, Vec<Frame>) {
    let nl = out.iter().position(|c| *c == b'\n').expect("a header line");
    let header: Value = serde_json::from_slice(&out[..nl]).unwrap();
    let mut rest = &out[nl + 1..];
    let mut v = Vec::new();
    while rest.len() >= 32 {
        let h = &rest[..32];
        assert_eq!(&h[0..4], b"DESK", "magic");
        assert_eq!(&h[5..8], &[0, 0, 0], "flags and reserved");
        let len = le(&h[8..12]) as usize;
        if rest.len() < 32 + len {
            break;
        }
        v.push(Frame {
            kind: h[4],
            seq: le(&h[12..20]),
            offset: le(&h[20..28]),
            dropped: le(&h[28..32]) as u32,
            payload: rest[32..32 + len].to_vec(),
        });
        rest = &rest[32 + len..];
    }
    (header, v)
}

/// Every data frame follows the pattern from its offset; seq and offset line up, and every
/// jump is counted in `dropped`. Returns the units delivered.
fn check_frames(fs: &[Frame], unit: usize) -> u64 {
    let mut units = 0;
    let mut prev: Option<&Frame> = None;
    for f in fs.iter().filter(|f| f.kind == 0) {
        assert!(f.payload.len() <= 1 << 20, "blocks are at most 1 MiB");
        assert_eq!(f.payload.len() % unit, 0, "blocks hold whole units");
        let n = f.payload.len() / unit;
        assert_eq!(
            f.payload,
            pattern(f.offset, n, unit),
            "frame {} at offset {}",
            f.seq,
            f.offset
        );
        if let Some(p) = prev {
            let lost = u64::from(f.dropped - p.dropped);
            assert_eq!(f.seq, p.seq + 1 + lost, "seq jumps by the dropped blocks");
            let next = p.offset + (p.payload.len() / unit) as u64;
            if lost == 0 {
                assert_eq!(f.offset, next, "offsets are contiguous without drops");
            } else {
                assert!(f.offset > next, "a drop skips units");
            }
        }
        units += n as u64;
        prev = Some(f);
    }
    units
}

#[test]
fn the_header_says_what_the_payload_is_and_the_payload_follows_the_pattern() {
    let b = Bench::new();
    let out = b.run(&[
        "stream",
        "la",
        "samples",
        "units=300000",
        "unit_size=2",
        "--header",
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let nl = out.stdout.iter().position(|c| *c == b'\n').unwrap();
    let h: Value = serde_json::from_slice(&out.stdout[..nl]).unwrap();
    assert_eq!(h["ok"], true, "{h}");
    let d = &h["data"];
    assert_eq!(d["device"], "la");
    assert_eq!(d["capability"], "capture");
    assert_eq!(d["stream"], "samples");
    assert_eq!(d["content"], "logic");
    assert_eq!(d["unit_size"], 2);
    assert_eq!(d["samplerate"], 1e6);
    assert_eq!(d["channels"].as_array().unwrap().len(), 16);
    assert_eq!(d["channels"][0], "D0", "bit 0 first");
    assert_eq!(d["meta"]["pattern"], "unit i = i % 251");
    assert_eq!(d["args"]["units"], 300000.0);
    assert_eq!(d["args"]["unit_size"], 2.0);
    assert_eq!(d["framing"], "desk-stream-1");
    assert_eq!(d["block_max"], 1 << 20);
    assert!(d["time"].as_str().unwrap().ends_with('Z'), "{d}");
    let payload = &out.stdout[nl + 1..];
    assert_eq!(payload.len(), 600_000);
    assert_eq!(payload, pattern(0, 300_000, 2).as_slice());

    // Without --header: the payload only.
    let out = b.run(&["stream", "la", "raw", "units=5000"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.stdout, pattern(0, 5000, 1));
}

#[test]
fn frames_carry_seq_and_offset_and_end_with_an_end_frame() {
    let b = Bench::new();
    let out = b.run(&[
        "stream",
        "la",
        "raw",
        "units=3000000",
        "unit_size=3",
        "--frames",
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let (header, fs) = frames(&out.stdout);
    assert_eq!(header["data"]["content"], "bytes");
    assert_eq!(header["data"]["unit_size"], 3);
    assert!(header["data"].get("channels").is_none(), "{header}");
    assert!(fs.len() > 8, "{} frames", fs.len());
    let data: Vec<&Frame> = fs.iter().filter(|f| f.kind == 0).collect();
    assert_eq!(data[0].seq, 0);
    assert_eq!(data[0].offset, 0);
    assert!(data.iter().all(|f| f.dropped == 0));
    assert_eq!(check_frames(&fs, 3), 3_000_000);
    let end = fs.last().unwrap();
    assert_eq!(end.kind, 1, "the last frame is the end frame");
    assert_eq!(
        end.seq,
        data.last().unwrap().seq + 1,
        "the end has the next seq"
    );
    assert_eq!(end.offset, 3_000_000, "and the next offset");
    let env: Value = serde_json::from_slice(&end.payload).unwrap();
    assert_eq!(env["ok"], true, "{env}");
    assert_eq!(env["data"]["ended"], "source");
    assert_eq!(env["data"]["units"], 3_000_000);
    assert_eq!(env["data"]["bytes"], 9_000_000);
}

#[test]
fn subscribers_share_one_stream_and_a_joiner_starts_later() {
    let b = Bench::new();
    let args = ["stream", "la", "samples", "rate=2000000", "--frames"];
    let mut first = b.spawn(&args);
    let h1 = first.header();
    // Some data went by before the second one joins.
    wait_until("data", || first.bytes().len() > 600_000);
    let mut joined = args.to_vec();
    joined.extend(["--duration", "600ms"]);
    let second = b.run(&joined);
    assert_eq!(second.code, 0, "{}", second.stderr);
    let (h2, fs) = frames(&second.stdout);
    assert_eq!(h1, h2, "the same header, with the stream's start time");
    let start = fs[0].offset;
    assert!(start > 0, "a joiner starts at a later offset");
    assert!(check_frames(&fs, 1) > 100_000);

    // The first one saw the same units at the same offsets.
    std::thread::sleep(Duration::from_millis(100));
    let (_, all) = frames(&first.bytes());
    check_frames(&all, 1);
    let same = all
        .iter()
        .find(|f| f.seq == fs[0].seq)
        .expect("the same block");
    assert_eq!(same.offset, fs[0].offset);
    assert_eq!(same.payload, fs[0].payload);
}

#[test]
fn other_arguments_or_another_stream_are_refused_naming_the_running_one() {
    let b = Bench::new();
    let mut first = b.spawn(&["stream", "la", "samples", "rate=1000000", "--frames"]);
    first.header();
    let out = b.run(&["--json", "stream", "la", "samples", "rate=5", "--discard"]);
    assert_eq!(out.code, 4, "{}", out.text());
    let e = &out.json()["error"];
    assert_eq!(e["code"], "endpoint_unavailable", "{e}");
    let msg = e["message"].as_str().unwrap();
    assert!(msg.contains("rate=1000000"), "{msg}");
    assert!(msg.contains("same arguments"), "{msg}");
    let out = b.run(&["--json", "stream", "la", "raw", "units=10", "--discard"]);
    assert_eq!(out.code, 4, "{}", out.text());
    assert!(
        out.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("one stream per device"),
        "{}",
        out.text()
    );
    // Without --discard, the error envelope goes to stderr (stdout is for the payload).
    let out = b.run(&["--json", "stream", "la", "samples", "rate=5"]);
    assert_eq!(out.code, 4);
    assert!(out.stdout.is_empty());
    let e: Value = serde_json::from_str(out.stderr.trim()).unwrap();
    assert_eq!(e["error"]["code"], "endpoint_unavailable");
}

#[test]
fn a_slow_subscriber_loses_blocks_while_a_fast_one_loses_nothing() {
    // A subscriber loses blocks once 4 MiB wait for it.
    let b = Bench::with_queue("4MiB");
    let args = ["stream", "la", "samples", "rate=10000000"];
    let mut fast_args = args.to_vec();
    fast_args.extend(["--json", "--discard", "--duration", "3s"]);
    let fast = b.spawn(&fast_args);
    let mut slow_args = args.to_vec();
    slow_args.push("--frames");
    // Joins the running stream (the same arguments) and reads nothing for a while.
    std::thread::sleep(Duration::from_millis(300));
    let mut slow = b.spawn(&slow_args);
    std::thread::sleep(Duration::from_millis(2000));
    slow.read();
    std::thread::sleep(Duration::from_millis(500));
    let (_, fs) = frames(&slow.bytes());
    drop(slow);
    let lost = fs.iter().map(|f| f.dropped).max().unwrap_or(0);
    assert!(lost > 0, "the slow one lost blocks ({} frames)", fs.len());
    check_frames(&fs, 1);

    let (code, out) = fast.finish();
    assert_eq!(code, 0);
    let s: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(s["ok"], true, "{s}");
    assert_eq!(s["data"]["ended"], "duration");
    assert_eq!(s["data"]["dropped"], 0, "{s}");
    assert!(s["data"]["bytes"].as_u64().unwrap() > 10_000_000, "{s}");
}

#[test]
fn a_failing_source_ends_the_stream_with_its_error() {
    let b = Bench::new();
    let out = b.run(&[
        "--json",
        "stream",
        "la",
        "samples",
        "fail_after=500000",
        "--discard",
    ]);
    assert_eq!(out.code, 4, "{}", out.text());
    let v = out.json();
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "endpoint_unavailable");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("simulated failure after 500000 units"),
        "{v}"
    );
    assert_eq!(
        v["data"]["bytes"], 500_000,
        "everything before the error arrived"
    );

    // In --frames, the end frame carries the error envelope.
    let out = b.run(&["stream", "la", "samples", "fail_after=1000", "--frames"]);
    assert_eq!(out.code, 4);
    let (_, fs) = frames(&out.stdout);
    let end: Value = serde_json::from_slice(&fs.last().unwrap().payload).unwrap();
    assert_eq!(end["ok"], false);
    assert_eq!(end["error"]["code"], "endpoint_unavailable");
    assert_eq!(end["exit"], 4);

    // Failing before the description: the reply itself is the error, nothing streams.
    let out = b.run(&["--json", "stream", "broken", "samples", "--discard"]);
    assert_eq!(out.code, 4, "{}", out.text());
    assert_eq!(out.lines().len(), 1);
    assert_eq!(out.json()["error"]["code"], "endpoint_unavailable");
}

#[test]
fn a_silent_source_ends_with_timeout() {
    let b = Bench::new();
    let started = Instant::now();
    let out = b.run(&[
        "--json",
        "stream",
        "la",
        "samples",
        "stall_after=1000",
        "--discard",
    ]);
    assert_eq!(out.code, 6, "{}", out.text());
    let v = out.json();
    assert_eq!(v["error"]["code"], "timeout", "{v}");
    assert!(
        v["error"]["message"].as_str().unwrap().contains("no data"),
        "{v}"
    );
    assert_eq!(v["data"]["bytes"], 1000);
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn a_streaming_device_is_busy_for_other_commands_but_others_are_not() {
    let b = Bench::new();
    let mut s = b.spawn(&["stream", "la", "samples", "rate=100000", "--frames"]);
    s.header();
    for args in [
        vec!["--json", "get", "la"],
        vec!["--json", "do", "la", "reset"],
        vec!["--json", "watch", "la", "--count", "1"],
    ] {
        let t = Instant::now();
        let out = b.run(&args);
        assert_eq!(out.code, 4, "{args:?}: {}", out.text());
        let line = &out.first();
        assert_eq!(line["error"]["code"], "endpoint_unavailable", "{args:?}");
        assert!(
            line["error"]["message"]
                .as_str()
                .unwrap()
                .contains("busy: it is streaming"),
            "{line}"
        );
        assert!(t.elapsed() < Duration::from_secs(5), "fails fast");
    }
    // Another device works meanwhile, and can stream too.
    assert_eq!(b.run(&["get", "other"]).code, 0);
    let out = b.run(&["stream", "other", "samples", "units=1000", "--discard"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.text().contains("1000 bytes"), "{}", out.text());

    // When the stream ends (its last subscriber left), the device is free again.
    drop(s);
    wait_until("the device to be free", || b.run(&["get", "la"]).code == 0);
}

#[test]
fn the_last_subscriber_leaving_stops_the_source_and_the_daemon_idles_out() {
    let b = Bench::new();
    let mut s = b.spawn(&["stream", "la", "samples", "rate=100000", "--frames"]);
    s.header();
    assert!(b.daemon_running());
    drop(s);
    wait_until("the daemon to exit", || !b.daemon_running());
}

#[test]
fn bytes_duration_and_discard_end_it_from_the_client() {
    let b = Bench::new();
    let out = b.run(&["stream", "la", "samples", "rate=50000000", "--bytes", "1MB"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.stdout.len(), 1_000_000);
    assert_eq!(out.stdout, pattern(0, 1_000_000, 1));
    // Whole units: 10 bytes of 3-byte units are 12.
    let out = b.run(&[
        "stream",
        "la",
        "samples",
        "unit_size=3",
        "rate=1000",
        "--bytes",
        "10",
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.stdout, pattern(0, 4, 3));

    let t = Instant::now();
    let out = b.run(&[
        "--json",
        "stream",
        "la",
        "samples",
        "rate=100000",
        "--discard",
        "--header",
        "--duration",
        "500ms",
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(t.elapsed() < Duration::from_secs(5));
    let lines = out.lines();
    assert_eq!(lines.len(), 2, "the header, then the summary");
    assert_eq!(lines[0]["data"]["stream"], "samples");
    let s = &lines[1]["data"];
    assert_eq!(s["ended"], "duration");
    assert!(s["seconds"].as_f64().unwrap() >= 0.5);
    assert!(s["bytes"].as_u64().unwrap() > 10_000, "{s}");
    assert_eq!(s["units"], s["bytes"]);
    assert!(s["mb_per_s"].as_f64().unwrap() > 0.0);

    // The human summary.
    let out = b.run(&["stream", "la", "raw", "units=2000", "--discard"]);
    assert_eq!(out.code, 0);
    let text = out.text();
    assert!(text.contains("2000 bytes, 2000 units"), "{text}");
    assert!(text.contains("the source ended"), "{text}");
}

#[test]
fn left_out_streams_and_arguments_are_listed() {
    let b = Bench::new();
    let out = b.run(&["--json", "stream", "la"]);
    assert_eq!(out.code, 2, "{}", out.text());
    let v = out.json();
    assert_eq!(v["error"]["code"], "usage");
    assert_eq!(v["data"]["streams"]["samples"]["content"], "logic");
    assert_eq!(v["data"]["streams"]["raw"]["content"], "bytes");
    assert_eq!(
        v["data"]["streams"]["samples"]["args"]["unit_size"]["default"],
        1.0
    );
    let out = b.run(&["stream", "la"]);
    assert_eq!(out.code, 2);
    assert!(out.text().contains("delivers logic"), "{}", out.text());

    let out = b.run(&["--json", "stream", "la", "raw"]);
    assert_eq!(out.code, 2);
    assert!(
        out.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("`raw` needs `units=…`")
    );
    let out = b.run(&["--json", "stream", "la", "nope"]);
    assert_eq!(out.code, 2);
    assert!(
        out.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("streams: samples, raw"),
        "{}",
        out.text()
    );
    let out = b.run(&["--json", "stream", "la", "samples", "speed=1"]);
    assert_eq!(out.code, 2);
    // A stream's name in `desk do` points to `desk stream`.
    let out = b.run(&["--json", "do", "la", "samples"]);
    assert!(
        out.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("desk stream la samples"),
        "{}",
        out.text()
    );
    // Nothing was started for any of these.
    assert!(!b.daemon_running());
}

#[test]
fn schema_and_device_show_know_streams() {
    let b = Bench::new();
    let schema = b.run(&["--json", "schema"]).json();
    let cmd = schema["data"]["commands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["command"] == "desk stream")
        .expect("desk stream in the schema");
    let args: Vec<&str> = cmd["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    for a in ["--header", "--frames", "--discard", "--duration", "--bytes"] {
        assert!(args.contains(&a), "{args:?}");
    }
    let la = schema["data"]["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == "la")
        .unwrap()
        .clone();
    assert_eq!(la["streams"]["samples"]["capability"], "capture");
    let show = b.run(&["device", "show", "la"]);
    assert!(show.text().contains("streams:"), "{}", show.text());
}

#[cfg(unix)]
#[test]
fn a_stopped_daemon_ends_the_stream_clearly() {
    let b = Bench::new();
    let mut serve = b.spawn(&["serve", "--idle", "never"]);
    serve.read();
    wait_until("the socket", || b.socket().is_some());
    let mut s = b.spawn(&["stream", "la", "samples", "rate=1000000", "--frames"]);
    s.header();
    let pid = serve.child.id().to_string();
    assert!(
        Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .unwrap()
            .success()
    );
    let (code, out) = s.finish();
    assert_eq!(code, 4);
    let (_, fs) = frames(&out);
    let end = fs.last().unwrap();
    assert_eq!(end.kind, 1);
    let env: Value = serde_json::from_slice(&end.payload).unwrap();
    assert_eq!(env["error"]["code"], "daemon_unavailable", "{env}");
    let (code, _) = serve.finish();
    assert_eq!(code, 0);
}

#[test]
fn a_lua_driver_describes_its_stream_and_fills_it() {
    let b = Bench::new();
    // Eight lines 300 ms apart: longer than the capability's 2 s timeout, which every append
    // starts again.
    let out = b.run(&["stream", "lua", "text", "--header"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let text = out.text();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["data"]["content"], "text");
    assert_eq!(header["data"]["unit_size"], 1);
    assert_eq!(header["data"]["meta"]["encoding"], "utf-8");
    let rest: Vec<&str> = lines.collect();
    assert_eq!(rest.len(), 8, "{text}");
    assert_eq!(rest[7], "line 8");

    for (stream, mode, code, what) in [
        ("text", "early", 5, "describe the stream before filling it"),
        ("text", "nothing", 5, "without describing its data"),
        ("text", "function", 5, "plain data"),
        ("text", "twice", 5, "already described"),
        ("logic", "nochannels", 5, "needs `channels`"),
        ("text", "silent", 6, "no data from the source"),
    ] {
        let arg = format!("mode={mode}");
        let out = b.run(&["--json", "stream", "lua", stream, &arg, "--discard"]);
        assert_eq!(out.code, code, "{mode}: {}", out.text());
        let v = out.lines().pop().unwrap();
        let msg = v["error"]["message"].as_str().unwrap();
        assert!(msg.contains(what), "{mode}: {msg}");
    }

    let out = b.run(&["--json", "do", "lua", "grab"]);
    assert_eq!(out.code, 5, "{}", out.text());
    assert!(
        out.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("only a stream is described"),
        "{}",
        out.text()
    );
}

#[test]
fn mains_off_ends_the_stream_with_device_off() {
    let b = Bench::new();
    let out = b.run(&["--json", "stream", "powered", "samples", "--discard"]);
    assert_eq!(out.code, 4, "{}", out.text());
    assert_eq!(out.json()["error"]["code"], "device_off", "mains is off");
    assert_eq!(b.run(&["power", "on", "powered"]).code, 0);
    let mut s = b.spawn(&["stream", "powered", "samples", "rate=100000", "--frames"]);
    s.header();
    assert_eq!(b.run(&["power", "off", "powered"]).code, 0);
    let (code, out) = s.finish();
    assert_eq!(code, 4);
    let (_, fs) = frames(&out);
    let env: Value = serde_json::from_slice(&fs.last().unwrap().payload).unwrap();
    assert_eq!(env["error"]["code"], "device_off", "{env}");
}
