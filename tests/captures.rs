//! End-to-end tests of actions that produce a file (`produces = "file"`): arguments with defaults
//! and optional ones, where the file goes, the size cap, timeouts, cancelling (Ctrl-C and a client
//! that leaves the daemon), dry runs, and Lua drivers that fill `ctx.output`.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;
use tempfile::TempDir;

/// `la` is a simulated capture device: `grab` fills the file with `bytes` of a known pattern.
/// Its capability timeout is short: a capture's own `timeout` suspends it.
const CONFIG: &str = r#"
[providers.sim]
kind = "mock"

[devices.la.capture]
provider = "sim"
endpoint = "la"
timeout  = "300ms"

[devices.la.capture.values]
last_action = { type = "string" }

[devices.la.capture.actions.grab]
switches = false
produces = "file"
doc      = "capture into a file"

[devices.la.capture.actions.grab.args]
bytes   = { type = "number", min = 0 }
rate    = { type = "number", optional = true, doc = "bytes per second" }
timeout = { type = "number", unit = "s", default = 5 }
label   = { type = "string", optional = true }

[devices.la.capture.actions.reset]
switches = false

[devices.broken.capture]
provider = "sim"
endpoint = "fail_la"
actions  = { grab = { switches = false, produces = "file", args = { bytes = { type = "number" } } } }

[devices.scripted.capture]
script = "cap.lua"
actions = { grab = { switches = false, produces = "file", args = { mode = { type = "choice", choices = ["ok", "noformat", "nothing", "empty", "badext", "function", "big"], default = "ok" } } } }
"#;

const CAP_LUA: &str = r#"
function action(ctx, name, args)
  if args.mode == "nothing" then return nil end
  if args.mode == "empty" then return { format = "txt" } end
  ctx.append{ into = ctx.output, data = "hello " }
  ctx.append{ into = ctx.output, data = "world" }
  if args.mode == "noformat" then return { samples = 11 } end
  if args.mode == "badext" then return { format = "csv:header" } end
  if args.mode == "function" then return { format = "txt", f = function() end } end
  if args.mode == "big" then
    for i = 1, 100 do ctx.append{ into = ctx.output, data = string.rep("x", 1000) } end
  end
  return { format = "txt", extension = "log", samples = 11, info = { channels = { "D0", "D1" } } }
end
"#;

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
        serde_json::from_str(&self.stdout).unwrap_or_else(|e| {
            panic!(
                "stdout is not JSON ({e}):\n{}\n{}",
                self.stdout, self.stderr
            )
        })
    }
    fn data(&self) -> Value {
        self.json()["data"].clone()
    }
    fn code_str(&self) -> String {
        self.json()["error"]["code"]
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

impl Bench {
    fn new() -> Self {
        Self::with(CONFIG)
    }

    fn with(config: &str) -> Self {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("desk.toml"), config).unwrap();
        std::fs::write(dir.path().join("cap.lua"), CAP_LUA).unwrap();
        Bench { dir }
    }

    fn path(&self, p: &str) -> PathBuf {
        self.dir.path().join(p)
    }

    /// Where `DESK_CAPTURE_DIR` points.
    fn captures(&self) -> PathBuf {
        self.path("captures")
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let dir = self.dir.path();
        let mut c = Command::new(env!("CARGO_BIN_EXE_desk"));
        c.env_remove("DESK_CONFIG")
            .env_remove("DESK_MODELS")
            .env("DESK_MOCK_STATE", dir.join("mock-state.json"))
            .env("DESK_RUNTIME_DIR", dir.join("run"))
            .env("DESK_DAEMON_IDLE", "1s")
            .env("DESK_CAPTURE_DIR", self.captures())
            .arg("--config")
            .arg(dir.join("desk.toml"))
            .args(args);
        c
    }

    fn run(&self, args: &[&str]) -> Out {
        output(self.cmd(args))
    }
}

fn output(mut c: Command) -> Out {
    let out = c.output().unwrap();
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// Every file below `dir` (relative paths, sorted); empty if it doesn't exist.
fn files(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                out.push(p.strip_prefix(base).unwrap().display().to_string());
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// Nothing at all below `dir`: no files and no folders (or `dir` doesn't exist).
fn nothing_left(dir: &Path) -> bool {
    std::fs::read_dir(dir).map_or(true, |mut e| e.next().is_none())
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_exit(child: &mut Child) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            return s.code().unwrap_or(-1);
        }
        assert!(Instant::now() < deadline, "did not exit within 10 s");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

fn is_stamp_name(name: &str, ext: &str) -> bool {
    // 2026-11-11T12-40-03Z.bin, or with -2, -3, … on a collision
    let Some(stem) = name.strip_suffix(&format!(".{ext}")) else {
        return false;
    };
    let (stamp, suffix) = stem.split_at(stem.len().min(20));
    stamp.len() == 20
        && stamp.ends_with('Z')
        && stamp.as_bytes()[10] == b'T'
        && !stamp.contains(':')
        && (suffix.is_empty() || suffix[1..].parse::<u32>().is_ok() && suffix.starts_with('-'))
}

#[test]
fn defaults_fill_in_optional_ones_stay_out_and_required_ones_are_named() {
    let b = Bench::new();
    // No arguments while one is required: the listing, with defaults and optional ones.
    let out = b.run(&["--json", "do", "la", "grab"]);
    assert_eq!(out.code, 2, "{}", out.stdout);
    assert_eq!(out.code_str(), "usage");
    let args = &out.data()["actions"]["grab"]["args"];
    assert_eq!(args["timeout"]["default"], 5.0, "{args}");
    assert_eq!(args["rate"]["optional"], true);
    assert!(args["bytes"].get("default").is_none());
    assert_eq!(out.data()["actions"]["grab"]["produces"], "file");
    assert!(out.message().contains("`bytes=…`"), "{}", out.message());
    assert!(!out.message().contains("timeout"), "{}", out.message());
    let human = b.run(&["do", "la", "grab"]);
    assert!(human.stdout.contains("default 5 s"), "{}", human.stdout);
    assert!(human.stdout.contains("optional"), "{}", human.stdout);
    assert!(human.stdout.contains("produces a file"), "{}", human.stdout);

    // Some given, a required one missing: named.
    let out = b.run(&["--json", "do", "la", "grab", "rate=10"]);
    assert_eq!(out.code, 2);
    assert!(
        out.message().contains("needs `bytes=…`"),
        "{}",
        out.message()
    );
    assert!(nothing_left(&b.captures()), "nothing ran");

    // The driver gets the default and not the optional ones.
    let out = b.run(&["--json", "do", "la", "grab", "bytes=10"]);
    assert_eq!(out.code, 0, "{} {}", out.stdout, out.stderr);
    let d = out.data();
    assert_eq!(d["args"]["timeout"], 5.0, "{d}");
    assert!(d["args"].get("rate").is_none());
    assert_eq!(d["values"]["last_action"], "grab bytes=10 timeout=5");

    // `desk device show` shows them too.
    let show = b.run(&["--json", "device", "show", "la"]);
    let grab = &show.data()["device"]["actions"]["grab"];
    assert_eq!(grab["produces"], "file", "{grab}");
    assert_eq!(grab["args"]["label"]["optional"], true);
}

#[test]
fn bad_defaults_are_config_errors() {
    for (arg, why) in [
        (
            r#"{ type = "number", unit = "s", max = 10, default = 20 }"#,
            "default",
        ),
        (
            r#"{ type = "number", default = 1, optional = true }"#,
            "not both",
        ),
        (
            r#"{ type = "choice", choices = ["a"], default = "b" }"#,
            "one of",
        ),
        (r#"{ type = "number", default = "5s" }"#, "number"),
    ] {
        let b = Bench::with(&format!(
            "[providers.sim]\nkind = \"mock\"\n[devices.x.cap]\nprovider = \"sim\"\nendpoint = \"x\"\nactions = {{ go = {{ switches = false, args = {{ a = {arg} }} }} }}\n"
        ));
        let out = b.run(&["--json", "device", "list"]);
        assert_eq!(out.code_str(), "config_invalid", "{arg}: {}", out.stdout);
        assert!(out.message().contains(why), "{arg}: {}", out.message());
    }
    let b = Bench::with(
        "[providers.sim]\nkind = \"mock\"\n[devices.x.cap]\nprovider = \"sim\"\nendpoint = \"x\"\nvalues = { t = { type = \"number\", default = 1 } }\n",
    );
    assert!(
        b.run(&["--json", "device", "list"])
            .message()
            .contains("only action arguments")
    );
    let b = Bench::with(
        "[providers.sim]\nkind = \"mock\"\n[devices.x.cap]\nprovider = \"sim\"\nendpoint = \"x\"\nactions = { go = { switches = false, produces = \"stream\" } }\n",
    );
    assert!(
        b.run(&["--json", "device", "list"])
            .message()
            .contains("produces")
    );
}

#[test]
fn the_result_names_the_file_and_the_file_holds_the_capture() {
    let b = Bench::new();
    let out = b.run(&["--json", "do", "la", "grab", "bytes=3000"]);
    assert_eq!(out.code, 0, "{} {}", out.stdout, out.stderr);
    let d = out.data();
    assert_eq!(d["device"], "la");
    assert_eq!(d["action"], "grab");
    assert_eq!(d["applied"], true);
    assert_eq!(d["bytes"], 3000);
    assert_eq!(d["format"], "mock");
    assert_eq!(d["meta"]["pattern"], "i % 251");
    let file = PathBuf::from(d["file"].as_str().unwrap());
    assert!(file.is_absolute());
    assert_eq!(file.parent().unwrap(), b.captures().join("la"));
    let name = file.file_name().unwrap().to_string_lossy().into_owned();
    assert!(is_stamp_name(&name, "bin"), "{name}");
    assert_eq!(std::fs::read(&file).unwrap(), pattern(3000));
    assert_eq!(files(&b.captures()).len(), 1, "no temporary file left");

    let human = b.run(&["do", "la", "grab", "bytes=2048"]);
    assert_eq!(human.code, 0);
    assert!(
        human.stdout.contains("done: grab bytes=2048"),
        "{}",
        human.stdout
    );
    assert!(human.stdout.contains("file: "), "{}", human.stdout);
    assert!(human.stdout.contains("(2 KiB, mock)"), "{}", human.stdout);
}

#[cfg(target_os = "linux")]
#[test]
fn without_settings_captures_go_to_the_state_folder() {
    let b = Bench::new();
    let state = b.path("state");
    let mut c = b.cmd(&["--json", "do", "la", "grab", "bytes=5"]);
    c.env_remove("DESK_CAPTURE_DIR")
        .env("XDG_STATE_HOME", &state);
    let out = output(c);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let file = PathBuf::from(out.data()["file"].as_str().unwrap());
    assert_eq!(
        file.parent().unwrap(),
        state.join("desk-operator/captures/la")
    );
}

#[test]
fn captures_dir_in_the_config_and_the_env_override_the_folder() {
    let b = Bench::with(&format!("[captures]\ndir = \"mine\"\n{CONFIG}"));
    let mut c = b.cmd(&["--json", "do", "la", "grab", "bytes=5"]);
    c.env_remove("DESK_CAPTURE_DIR");
    let out = output(c);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let file = PathBuf::from(out.data()["file"].as_str().unwrap());
    assert_eq!(
        file.parent().unwrap(),
        b.path("mine/la"),
        "relative to the config file"
    );
    // The environment wins.
    let out = b.run(&["--json", "do", "la", "grab", "bytes=5"]);
    let file = PathBuf::from(out.data()["file"].as_str().unwrap());
    assert_eq!(file.parent().unwrap(), b.captures().join("la"));

    let b = Bench::with(&format!("[captures]\nfolder = \"x\"\n{CONFIG}"));
    assert_eq!(
        b.run(&["--json", "device", "list"]).code_str(),
        "config_invalid"
    );
}

#[test]
fn output_names_the_file_relative_to_where_desk_runs() {
    let b = Bench::new();
    std::fs::create_dir(b.path("work")).unwrap();
    let mut c = b.cmd(&[
        "--json", "do", "la", "grab", "bytes=7", "--output", "run1.bin",
    ]);
    c.current_dir(b.path("work"));
    let out = output(c);
    assert_eq!(out.code, 0, "{} {}", out.stdout, out.stderr);
    let file = PathBuf::from(out.data()["file"].as_str().unwrap());
    assert_eq!(file, std::path::absolute(b.path("work/run1.bin")).unwrap());
    assert_eq!(std::fs::read(&file).unwrap(), pattern(7));
    assert_eq!(files(&b.path("work")), ["run1.bin"]);

    // An existing file: refused before the driver runs, and left alone.
    let before = b.run(&["--json", "get", "la"]).data()["values"]["last_action"].clone();
    std::fs::write(b.path("work/keep.bin"), b"mine").unwrap();
    let out = b.run(&[
        "--json",
        "do",
        "la",
        "grab",
        "bytes=9",
        "--output",
        b.path("work/keep.bin").to_str().unwrap(),
    ]);
    assert_eq!(out.code, 2, "{}", out.stdout);
    assert!(
        out.message().contains("already exists"),
        "{}",
        out.message()
    );
    assert_eq!(std::fs::read(b.path("work/keep.bin")).unwrap(), b"mine");
    let after = b.run(&["--json", "get", "la"]).data()["values"]["last_action"].clone();
    assert_eq!(before, after, "the driver didn't run");

    // A folder that doesn't exist.
    let out = b.run(&[
        "--json",
        "do",
        "la",
        "grab",
        "bytes=9",
        "--output",
        b.path("nope/x.bin").to_str().unwrap(),
    ]);
    assert_eq!(out.code, 2);
    assert!(out.message().contains("doesn't exist"), "{}", out.message());
    assert!(!b.path("nope").exists());

    // Only for actions that produce a file.
    let out = b.run(&["--json", "do", "la", "reset", "--output", "x.bin"]);
    assert_eq!(out.code, 2);
    assert!(
        out.message().contains("doesn't produce a file"),
        "{}",
        out.message()
    );
    let out = b.run(&["--json", "do", "la", "reset", "--max-size", "1MB"]);
    assert_eq!(out.code, 2);
}

#[test]
fn a_name_taken_in_the_same_second_gets_a_suffix() {
    let b = Bench::new();
    let dir = b.captures().join("la");
    std::fs::create_dir_all(&dir).unwrap();
    // Take the names of the next few seconds.
    let now = SystemTime::now();
    let mut taken = Vec::new();
    for s in 0..4 {
        let stamp = humantime::format_rfc3339_seconds(now + Duration::from_secs(s))
            .to_string()
            .replace(':', "-");
        let p = dir.join(format!("{stamp}.bin"));
        std::fs::write(&p, b"older").unwrap();
        taken.push(p);
    }
    let out = b.run(&["--json", "do", "la", "grab", "bytes=4"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let file = PathBuf::from(out.data()["file"].as_str().unwrap());
    let name = file.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.ends_with("Z-2.bin"), "{name}");
    assert!(is_stamp_name(&name, "bin"));
    for p in taken {
        assert_eq!(std::fs::read(p).unwrap(), b"older", "never overwritten");
    }
}

#[test]
fn the_size_cap_stops_a_capture_midway_and_leaves_nothing() {
    let b = Bench::new();
    // 10 kB every 50 ms; the cap is hit after a few steps.
    let started = Instant::now();
    let out = b.run(&[
        "--json",
        "do",
        "la",
        "grab",
        "bytes=1000000",
        "rate=200000",
        "--max-size",
        "30kB",
    ]);
    assert_eq!(out.code, 8, "{}", out.stdout);
    assert_eq!(out.code_str(), "limit_exceeded");
    assert!(out.message().contains("size cap"), "{}", out.message());
    assert!(
        out.json()["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("--max-size")
    );
    assert!(started.elapsed() < Duration::from_secs(3), "stopped early");
    assert!(nothing_left(&b.captures()), "{:?}", files(&b.captures()));

    // `[captures] max_size` is the default cap.
    let b = Bench::with(&format!("[captures]\nmax_size = \"1KiB\"\n{CONFIG}"));
    let out = b.run(&["--json", "do", "la", "grab", "bytes=1025"]);
    assert_eq!(out.code_str(), "limit_exceeded", "{}", out.stdout);
    assert!(out.message().contains("1 KiB"), "{}", out.message());
    assert_eq!(b.run(&["--json", "do", "la", "grab", "bytes=1024"]).code, 0);
    let out = b.run(&[
        "--json",
        "do",
        "la",
        "grab",
        "bytes=5",
        "--max-size",
        "lots",
    ]);
    assert_eq!(out.code_str(), "usage");
    // --dry-run reports the cap that applies.
    let out = b.run(&["--json", "do", "la", "grab", "bytes=5", "--dry-run"]);
    assert_eq!(out.data()["max_size"], 1024);
}

#[test]
fn a_capture_has_its_own_timeout() {
    let b = Bench::new();
    // Longer than the capability's 300 ms: fine, its own timeout applies.
    let out = b.run(&["--json", "do", "la", "grab", "bytes=6000", "rate=10000"]);
    assert_eq!(out.code, 0, "{} {}", out.stdout, out.stderr);
    assert_eq!(out.data()["bytes"], 6000);
    // Its own timeout runs out midway: nothing is kept.
    let out = b.run(&[
        "--json",
        "do",
        "la",
        "grab",
        "bytes=100000",
        "rate=20000",
        "timeout=0.3s",
    ]);
    assert_eq!(out.code, 6, "{}", out.stdout);
    assert_eq!(out.code_str(), "timeout");
    assert_eq!(files(&b.captures()).len(), 1, "only the first capture");
}

#[test]
fn a_failing_device_leaves_no_file() {
    let b = Bench::new();
    let out = b.run(&["--json", "do", "broken", "grab", "bytes=10"]);
    assert_eq!(out.code_str(), "endpoint_unavailable", "{}", out.stdout);
    assert!(nothing_left(&b.captures()));
    // An empty capture fails too.
    let out = b.run(&["--json", "do", "la", "grab", "bytes=0"]);
    assert_eq!(out.code_str(), "endpoint_unavailable", "{}", out.stdout);
    assert!(out.message().contains("no data"), "{}", out.message());
    assert!(nothing_left(&b.captures()));
}

#[test]
fn a_dry_run_says_where_and_creates_nothing() {
    let b = Bench::new();
    let out = b.run(&["--json", "do", "la", "grab", "bytes=10", "--dry-run"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let d = out.data();
    assert_eq!(d["applied"], false);
    assert_eq!(d["into"], b.captures().join("la").display().to_string());
    assert_eq!(d["max_size"], 1u64 << 30);
    assert!(d.get("file").is_none());
    assert!(!b.captures().exists(), "no folder either");
    let human = b.run(&["do", "la", "grab", "bytes=10", "--dry-run"]);
    assert!(human.stdout.contains("would do grab"), "{}", human.stdout);
    assert!(human.stdout.contains("at most 1 GiB"), "{}", human.stdout);

    // The checks still apply.
    std::fs::write(b.path("x.bin"), b"").unwrap();
    let x = b.path("x.bin");
    let out = b.run(&[
        "--json",
        "do",
        "la",
        "grab",
        "bytes=10",
        "--dry-run",
        "--output",
        x.to_str().unwrap(),
    ]);
    assert_eq!(out.code_str(), "usage");
}

#[test]
fn a_lua_driver_fills_ctx_output_and_describes_the_file() {
    let b = Bench::new();
    let out = b.run(&["--json", "do", "scripted", "grab"]);
    assert_eq!(out.code, 0, "{} {}", out.stdout, out.stderr);
    let d = out.data();
    assert_eq!(d["format"], "txt");
    assert_eq!(d["bytes"], 11);
    assert_eq!(
        d["meta"],
        serde_json::json!({ "samples": 11, "info": { "channels": ["D0", "D1"] } })
    );
    let file = PathBuf::from(d["file"].as_str().unwrap());
    assert!(file.to_string_lossy().ends_with(".log"), "the extension");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello world");
    assert_eq!(files(&b.captures()).len(), 1);

    for (mode, want) in [
        ("noformat", "format"),
        ("nothing", "format"),
        ("empty", "empty"),
        ("badext", "extension"),
        ("function", "no functions"),
    ] {
        let out = b.run(&["--json", "do", "scripted", "grab", &format!("mode={mode}")]);
        assert_eq!(out.code_str(), "script_failed", "{mode}: {}", out.stdout);
        assert!(out.message().contains(want), "{mode}: {}", out.message());
        assert_eq!(files(&b.captures()).len(), 1, "{mode}: nothing new kept");
    }
    // The cap applies to what a script appends.
    let out = b.run(&[
        "--json",
        "do",
        "scripted",
        "grab",
        "mode=big",
        "--max-size",
        "50kB",
    ]);
    assert_eq!(out.code_str(), "limit_exceeded", "{}", out.stdout);
    assert_eq!(files(&b.captures()).len(), 1);
}

#[test]
fn ctx_output_exists_only_in_actions_that_produce_a_file() {
    let b = Bench::with(
        r#"
[devices.s.cap]
script = "s.lua"
actions = { plain = { switches = false } }
"#,
    );
    std::fs::write(
        b.path("s.lua"),
        "function action(ctx, name, args)\n  assert(ctx.output == nil, 'ctx.output is set')\n  ctx.append{ into = ctx.output, data = 'x' }\nend\n",
    )
    .unwrap();
    let out = b.run(&["--json", "do", "s", "plain"]);
    assert_eq!(out.code_str(), "script_failed", "{}", out.stdout);
    assert!(
        out.message().contains("must be ctx.output"),
        "{}",
        out.message()
    );
}

#[cfg(unix)]
#[test]
fn ctrl_c_stops_a_capture_and_removes_the_partial_file() {
    let b = Bench::new();
    let mut child = b
        .cmd(&["--json", "do", "la", "grab", "bytes=200000", "rate=20000"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let dir = b.captures().join("la");
    wait_until("the partial file", || {
        files(&dir).iter().any(|f| f.contains(".partial"))
    });
    let ok = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(ok.success());
    let code = wait_exit(&mut child);
    let mut stdout = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take().unwrap(), &mut stdout).unwrap();
    assert_eq!(code, 130, "{stdout}");
    let v: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["error"]["code"], "cancelled");
    assert!(
        !dir.exists(),
        "the empty device folder is gone too: {:?}",
        files(&dir)
    );
}

#[cfg(unix)]
#[test]
fn a_client_that_leaves_the_daemon_cancels_its_capture() {
    let b = Bench::new();
    let mut daemon = b
        .cmd(&["serve", "--idle", "20s"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Once its socket exists, commands go through it.
    wait_until("the daemon's socket", || {
        std::fs::read_dir(b.path("run")).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|e| e.path().extension().is_some_and(|x| x == "sock"))
        })
    });

    let mut client = b
        .cmd(&["--json", "do", "la", "grab", "bytes=200000", "rate=20000"])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let dir = b.captures().join("la");
    wait_until("the partial file", || {
        files(&dir).iter().any(|f| f.contains(".partial"))
    });
    client.kill().unwrap();
    let _ = client.wait();
    wait_until("the partial file to go", || nothing_left(&dir));

    // The device is free again, and the daemon still works.
    let out = b.run(&["--json", "do", "la", "grab", "bytes=10"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(files(&dir).len(), 1);
    let _ = daemon.kill();
    let _ = daemon.wait();
}
