//! End-to-end tests of the daemon (`desk serve`) and `desk source watch` against the simulated
//! PSU: streaming, commands during a watch, several watchers, endings, the socket protocol.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

const CONFIG: &str = r#"
[providers.sim]
kind = "mock"

[devices.psu]
power  = { provider = "sim", endpoint = "mains" }
source = { provider = "sim", endpoint = "psu", max_voltage = 30, max_current = 5, load_ohms = 10 }
limits = { source = { voltage = 15, current = 2 } }

[devices.lamp]
power = { provider = "sim", endpoint = "lamp" }

[devices.broken]
source = { provider = "sim", endpoint = "fail_psu" }
"#;

struct Bench {
    dir: TempDir,
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
    /// Every stdout line as JSON (watch output).
    fn lines(&self) -> Vec<Value> {
        self.stdout
            .lines()
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}")))
            .collect()
    }
}

impl Bench {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("desk.toml"), CONFIG).unwrap();
        let b = Bench { dir };
        assert_eq!(b.run(&["power", "on", "psu"]).code, 0);
        b
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
            .arg("--config")
            .arg(dir.join("desk.toml"))
            .args(args);
        c
    }

    fn run(&self, args: &[&str]) -> Out {
        let out = self.cmd(args).output().unwrap();
        Out {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        }
    }

    /// Start a command in the background, its stdout read line by line.
    fn spawn(&self, args: &[&str]) -> Running {
        let mut child = self.cmd(args).stdout(Stdio::piped()).spawn().unwrap();
        let lines = reader(child.stdout.take().unwrap());
        Running { child, lines }
    }

    /// The daemon's socket, once it exists.
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
    lines: mpsc::Receiver<String>,
}

impl Running {
    fn line(&self) -> Value {
        let l = self
            .lines
            .recv_timeout(Duration::from_secs(10))
            .expect("no line within 10 s");
        serde_json::from_str(&l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}"))
    }

    /// Wait for the exit; returns the code and the remaining lines.
    fn finish(mut self) -> (i32, Vec<Value>) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let code = loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                break s.code().unwrap_or(-1);
            }
            assert!(Instant::now() < deadline, "did not exit within 10 s");
            std::thread::sleep(Duration::from_millis(20));
        };
        let rest = self
            .lines
            .try_iter()
            .map(|l| serde_json::from_str(&l).unwrap_or(Value::String(l)))
            .collect();
        (code, rest)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn reader(out: ChildStdout) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn watch_streams_readings_while_other_commands_run() {
    let b = Bench::new();
    assert_eq!(b.run(&["source", "set", "psu", "5v", "1a", "--on"]).code, 0);
    let w = b.spawn(&["--json", "source", "watch", "psu", "--interval", "50ms"]);
    let first = w.line();
    assert_eq!(first["ok"], true, "{first}");
    let data = &first["data"];
    assert_eq!(data["device"], "psu");
    assert_eq!(data["dropped"], 0);
    assert!(data["time"].as_str().unwrap().ends_with('Z'), "{data}");
    assert!(data["elapsed"].as_f64().unwrap() < 1.0);
    assert_eq!(data["reading"]["voltage_set"], 5.0);
    assert_eq!(data["reading"]["mode"], "cv");

    // The daemon holds the session; commands go through it.
    let set = b.run(&["--json", "source", "set", "psu", "3v"]);
    assert_eq!(set.code, 0, "{}", set.stdout);
    assert_eq!(set.json()["data"]["verified"]["voltage"], true);
    // Limits still apply to commands that go through the daemon.
    let refused = b.run(&["--json", "source", "set", "psu", "20v"]);
    assert_eq!(refused.code, 8);
    assert_eq!(refused.json()["error"]["code"], "limit_exceeded");

    let mut saw = false;
    for _ in 0..40 {
        if w.line()["data"]["reading"]["voltage_set"] == 3.0 {
            saw = true;
            break;
        }
    }
    assert!(saw, "the watch never showed the new setpoint");
    // Readings come at the requested interval (not faster).
    let a = w.line()["data"]["elapsed"].as_f64().unwrap();
    let c = w.line()["data"]["elapsed"].as_f64().unwrap();
    assert!(c - a >= 0.04, "{a} → {c}");
}

#[test]
fn count_ends_the_watch_and_human_output_is_a_table() {
    let b = Bench::new();
    let out = b.run(&[
        "--json",
        "source",
        "watch",
        "psu",
        "--interval",
        "20ms",
        "--count",
        "5",
    ]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let lines = out.lines();
    assert_eq!(lines.len(), 5);
    assert!(lines.iter().all(|l| l["ok"] == true));

    let out = b.run(&[
        "source",
        "watch",
        "psu",
        "--interval",
        "20ms",
        "--count",
        "2",
    ]);
    assert_eq!(out.code, 0);
    let rows: Vec<&str> = out.stdout.lines().collect();
    assert_eq!(rows.len(), 3, "{}", out.stdout);
    assert!(rows[0].starts_with("ELAPSED"), "{}", out.stdout);
    assert!(
        rows[1].contains(" V ") || rows[1].contains(" V"),
        "{}",
        out.stdout
    );
}

#[test]
fn watchers_share_one_session_each_at_its_own_interval() {
    let b = Bench::new();
    let fast = b.spawn(&[
        "--json",
        "source",
        "watch",
        "psu",
        "--interval",
        "30ms",
        "--count",
        "30",
    ]);
    fast.line();
    let slow = b.run(&[
        "--json",
        "source",
        "watch",
        "psu",
        "--interval",
        "300ms",
        "--count",
        "3",
    ]);
    assert_eq!(slow.code, 0, "{}", slow.stdout);
    let t: Vec<f64> = slow
        .lines()
        .iter()
        .map(|l| l["data"]["elapsed"].as_f64().unwrap())
        .collect();
    assert!(t[1] - t[0] >= 0.25 && t[2] - t[1] >= 0.25, "{t:?}");
    let (code, _) = fast.finish();
    assert_eq!(code, 0);
}

#[test]
fn mains_off_ends_the_watch_with_device_off() {
    let b = Bench::new();
    let w = b.spawn(&["--json", "source", "watch", "psu", "--interval", "50ms"]);
    w.line();
    assert_eq!(b.run(&["power", "off", "psu"]).code, 0);
    let (code, lines) = w.finish();
    assert_eq!(code, 4);
    let last = lines.last().expect("an error line");
    assert_eq!(last["ok"], false);
    assert_eq!(last["error"]["code"], "device_off");
}

#[test]
fn watch_refuses_bad_requests_with_one_json_line() {
    let b = Bench::new();
    for (args, code, err) in [
        (vec!["source", "watch", "nope"], 2, "device_not_found"),
        (vec!["source", "watch", "lamp"], 3, "capability_missing"),
        (
            vec!["source", "watch", "psu", "--interval", "fast"],
            2,
            "usage",
        ),
        (vec!["source", "watch", "psu", "--count", "0"], 2, "usage"),
    ] {
        let mut a = vec!["--json"];
        a.extend(&args);
        let out = b.run(&a);
        assert_eq!(out.code, code, "{args:?}: {}", out.stdout);
        let lines = out.lines();
        assert_eq!(lines.len(), 1, "{args:?}: {}", out.stdout);
        assert_eq!(lines[0]["error"]["code"], err, "{args:?}");
    }
    // Device off before watching.
    assert_eq!(b.run(&["power", "off", "psu"]).code, 0);
    let out = b.run(&["--json", "source", "watch", "psu", "--count", "1"]);
    assert_eq!(out.code, 4);
    assert_eq!(out.lines()[0]["error"]["code"], "device_off");
}

#[test]
fn without_a_daemon_commands_run_in_process() {
    let b = Bench::new();
    let out = b.run(&["--json", "source", "read", "psu"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert!(b.socket().is_none(), "a plain command started a daemon");
    assert!(!b.runtime().exists());
}

#[test]
fn the_daemon_exits_when_idle_and_is_private() {
    let b = Bench::new();
    let w = b.spawn(&["--json", "source", "watch", "psu", "--interval", "50ms"]);
    w.line();
    let socket = b.socket().expect("a socket while watching");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&socket), 0o600);
        assert_eq!(mode(&b.runtime()), 0o700);
    }
    drop(w); // the watcher leaves (killed)
    wait_until("the daemon to exit", || !socket.exists());
    // The next command runs in-process again.
    assert_eq!(b.run(&["source", "read", "psu"]).code, 0);
}

#[cfg(unix)]
#[test]
fn a_stopped_daemon_ends_the_watch_clearly() {
    let b = Bench::new();
    let serve = b.spawn(&["serve", "--idle", "never"]);
    wait_until("the socket", || b.socket().is_some());
    let w = b.spawn(&["--json", "source", "watch", "psu", "--interval", "50ms"]);
    w.line();
    let pid = serve.child.id().to_string();
    assert!(
        Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .unwrap()
            .success()
    );
    let (code, lines) = w.finish();
    assert_eq!(code, 4);
    assert_eq!(lines.last().unwrap()["error"]["code"], "daemon_unavailable");
    let (code, _) = serve.finish();
    assert_eq!(code, 0);

    // Killed hard: the watcher notices the connection is gone.
    let serve = b.spawn(&["serve", "--idle", "never"]);
    wait_until("the socket", || b.socket().is_some());
    let w = b.spawn(&["--json", "source", "watch", "psu", "--interval", "50ms"]);
    w.line();
    let pid = serve.child.id().to_string();
    assert!(
        Command::new("kill")
            .args(["-KILL", &pid])
            .status()
            .unwrap()
            .success()
    );
    let (code, lines) = w.finish();
    assert_eq!(code, 4);
    assert_eq!(lines.last().unwrap()["error"]["code"], "daemon_unavailable");
}

#[cfg(unix)]
#[test]
fn programs_can_talk_to_the_socket_directly() {
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    let b = Bench::new();
    let _serve = b.spawn(&["serve", "--idle", "never"]);
    wait_until("the socket", || b.socket().is_some());
    let mut s = UnixStream::connect(b.socket().unwrap()).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut lines = BufReader::new(s.try_clone().unwrap()).lines();
    let mut ask = |req: &str| -> Value {
        writeln!(s, "{req}").unwrap();
        serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap()
    };
    // Several requests on one connection, same envelope as --json plus `human` and `exit`.
    let r = ask(r#"{"args": ["source", "set", "psu", "4v", "--on"]}"#);
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["exit"], 0);
    assert_eq!(r["data"]["reading"]["voltage_set"], 4.0);
    assert!(r["human"].as_str().unwrap().contains("confirmed"));
    let r = ask(r#"{"args": ["source", "read", "nope"]}"#);
    assert_eq!(r["error"]["code"], "device_not_found");
    assert_eq!(r["exit"], 2);
    let r = ask(r#"{"args": ["schema"]}"#);
    assert_eq!(r["error"]["code"], "usage");
    let r = ask("not json");
    assert_eq!(r["error"]["code"], "usage");

    // A watch turns the connection into a stream.
    let r = ask(r#"{"args": ["source", "watch", "psu", "--interval", "20ms", "--count", "2"]}"#);
    assert_eq!(r["data"]["reading"]["output"], true, "{r}");
    let r: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(r["ok"], true);
    assert!(lines.next().is_none(), "the stream ends after --count");
}

#[test]
fn a_device_that_keeps_failing_ends_the_watch_after_three_tries() {
    let b = Bench::new();
    let out = b.run(&["--json", "source", "watch", "broken", "--interval", "20ms"]);
    assert_eq!(out.code, 4, "{}", out.stdout);
    let lines = out.lines();
    assert_eq!(lines.len(), 1, "{}", out.stdout);
    assert_eq!(lines[0]["error"]["code"], "endpoint_unavailable");
}
