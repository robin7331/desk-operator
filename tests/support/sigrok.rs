//! Helpers for tests of sigrok bindings against a fake `sigrok-cli` (`fake_sigrok_cli.rs`, a Cargo
//! example), so no test needs sigrok or hardware.
//!
//! A [`SigrokBench`] is a temp folder with a `desk.toml`, a scenario for the fake and a `bin/`
//! folder that is desk's whole PATH: the implicit backend `sigrok` only exists when a test puts a
//! copy of the fake there ([`SigrokBench::install_on_path`]). Use it from a `tests/model_*.rs` file:
//!
//! ```ignore
//! #[path = "support/sigrok.rs"]
//! mod sigrok;
//! let b = sigrok::SigrokBench::new(r#"
//!     [devices.la]
//!     model = "acme/la-1"
//!     capture = { sigrok = { backend = "fake" } }
//! "#);
//! b.fake.devices("acme-la - ACME LA with 8 channels: D0 D1 D2 D3 D4 D5 D6 D7");
//! let out = b.run(&["--json", "do", "la", "capture", "duration=10ms"]);
//! assert_eq!(b.fake.calls()[0], ["-d", "acme-la", "--time", "10", "-O", "srzip", "-o", "…"]);
//! ```
//!
//! The config gets `[providers.fake] kind = "sigrok"` pointing at the fake, unless it defines a
//! provider `fake` itself. Models come from `$DESK_MODELS` (this checkout's `models/`).
#![allow(dead_code)] // every test file uses only some of these

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use serde_json::Value;
use tempfile::TempDir;

/// The fake program, built if needed (`cargo test` builds examples; `cargo test --test x` doesn't).
pub fn program() -> PathBuf {
    static BUILD: Mutex<()> = Mutex::new(());
    let exe = std::env::current_exe().unwrap();
    // target/<profile>/deps/<test> -> target/<profile>/examples/fake-sigrok-cli
    let profile = exe.parent().and_then(Path::parent).unwrap();
    let path = profile
        .join("examples")
        .join(format!("fake-sigrok-cli{}", std::env::consts::EXE_SUFFIX));
    let _guard = BUILD.lock().unwrap_or_else(|e| e.into_inner());
    if !path.is_file() {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let mut c = Command::new(cargo);
        c.args(["build", "--example", "fake-sigrok-cli"])
            .current_dir(env!("CARGO_MANIFEST_DIR"));
        if profile.file_name().is_some_and(|n| n == "release") {
            c.arg("--release");
        }
        let ok = c.status().map(|s| s.success()).unwrap_or(false);
        assert!(ok && path.is_file(), "cannot build the fake sigrok-cli");
    }
    path
}

/// One scenario of the fake: what it has, finds and does, and the log of its calls.
pub struct Fake {
    pub dir: PathBuf,
}

impl Fake {
    pub fn new(dir: PathBuf) -> Fake {
        std::fs::create_dir_all(&dir).unwrap();
        Fake { dir }
    }

    fn write(&self, name: &str, text: &str) -> &Self {
        std::fs::write(self.dir.join(name), format!("{}\n", text.trim())).unwrap();
        self
    }

    /// `-L`: `<id> <description>` per line.
    pub fn drivers(&self, lines: &str) -> &Self {
        self.write("drivers", lines)
    }

    /// `--scan`: one line per device, as sigrok prints it.
    pub fn devices(&self, lines: &str) -> &Self {
        self.write("devices", lines)
    }

    /// `key=value` lines (see `fake_sigrok_cli.rs`).
    pub fn behaviour(&self, lines: &str) -> &Self {
        self.write("behaviour", lines)
    }

    pub fn version(&self, v: &str) -> &Self {
        self.write("version", v)
    }

    pub fn show(&self, text: &str) -> &Self {
        std::fs::write(self.dir.join("show"), text).unwrap();
        self
    }

    /// The device's config values for `--get` and `--set`: `key=value` or `GROUP/key=value` per
    /// line, values as sigrok prints them (`200000`, `10.0`, `true`, `'square'`). Clears what an
    /// earlier `--set` kept.
    pub fn device_config(&self, lines: &str) -> &Self {
        let _ = std::fs::remove_file(self.dir.join("kept"));
        self.write("config", lines)
    }

    /// Channel groups `-g` accepts, one per line (groups in `device_config` count too).
    pub fn groups(&self, lines: &str) -> &Self {
        self.write("groups", lines)
    }

    /// What a measurement prints on stdout (with `per_channel=1` in `behaviour`: per sample and
    /// requested channel, the first line for that channel).
    pub fn readings(&self, lines: &str) -> &Self {
        self.write("readings", lines)
    }

    /// What `--set` kept so far (`key=value` or `GROUP/key=value` lines).
    pub fn kept(&self) -> String {
        std::fs::read_to_string(self.dir.join("kept")).unwrap_or_default()
    }

    /// The calls that read config keys (they have `--get`).
    pub fn gets(&self) -> Vec<Vec<String>> {
        self.calls()
            .into_iter()
            .filter(|c| c.iter().any(|a| a == "--get"))
            .collect()
    }

    /// The calls that changed config keys (they have `--set`).
    pub fn sets(&self) -> Vec<Vec<String>> {
        self.calls()
            .into_iter()
            .filter(|c| c.iter().any(|a| a == "--set"))
            .collect()
    }

    /// The calls that took readings (`--samples` or `--time`, no `-o`).
    pub fn measures(&self) -> Vec<Vec<String>> {
        self.calls()
            .into_iter()
            .filter(|c| {
                c.iter().any(|a| a == "--samples" || a == "--time") && !c.iter().any(|a| a == "-o")
            })
            .collect()
    }

    /// Every call so far, each as its argument vector.
    pub fn calls(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(self.dir.join("log"))
            .unwrap_or_default()
            .lines()
            .map(|l| {
                if l.is_empty() {
                    Vec::new()
                } else {
                    l.split('\u{1f}').map(String::from).collect()
                }
            })
            .collect()
    }

    /// How many calls asked for the driver list.
    pub fn driver_lists(&self) -> usize {
        self.calls().iter().filter(|c| *c == &["-L"]).count()
    }

    /// The calls that captured (they have `-o`).
    pub fn captures(&self) -> Vec<Vec<String>> {
        self.calls()
            .into_iter()
            .filter(|c| c.iter().any(|a| a == "-o"))
            .collect()
    }
}

/// A finished `desk` run.
pub struct Out {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Out {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|e| {
            panic!(
                "stdout is not JSON ({e}):\n{}\n{}",
                self.stdout, self.stderr
            )
        })
    }
    pub fn data(&self) -> Value {
        self.json()["data"].clone()
    }
    pub fn code_str(&self) -> String {
        self.json()["error"]["code"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }
    pub fn message(&self) -> String {
        self.json()["error"]["message"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }
}

pub struct SigrokBench {
    pub dir: TempDir,
    /// The scenario of the `fake` backend (and of every copy without a folder of its own).
    pub fake: Fake,
}

impl SigrokBench {
    pub fn new(config: &str) -> SigrokBench {
        let dir = TempDir::new().unwrap();
        let fake = Fake::new(dir.path().join("fake"));
        fake.drivers("demo Demo driver and pattern generator");
        std::fs::create_dir_all(dir.path().join("bin")).unwrap();
        let b = SigrokBench { dir, fake };
        b.config(config);
        b
    }

    /// Replace `desk.toml` (a `fake` backend is added unless it defines one).
    pub fn config(&self, config: &str) {
        let mut text = config.to_string();
        if !config.contains("[providers.fake]") {
            text.push_str(&format!(
                "\n[providers.fake]\nkind = \"sigrok\"\nprogram = {:?}\n",
                program().display().to_string()
            ));
        }
        std::fs::write(self.path("desk.toml"), text).unwrap();
    }

    pub fn path(&self, p: &str) -> PathBuf {
        self.dir.path().join(p)
    }

    /// Where `DESK_CAPTURE_DIR` points.
    pub fn captures(&self) -> PathBuf {
        self.path("captures")
    }

    /// A copy of the fake named `name` in `bin/` with its own scenario folder; returns its path
    /// (for `program = …`) and its scenario.
    pub fn install(&self, name: &str) -> (PathBuf, Fake) {
        let to = self
            .path("bin")
            .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        std::fs::copy(program(), &to).unwrap();
        let fake = Fake::new(self.fake.dir.join(name));
        fake.drivers("demo Demo driver and pattern generator");
        (to, fake)
    }

    /// `sigrok-cli` on desk's PATH: the implicit backend.
    pub fn install_on_path(&self) -> Fake {
        self.install("sigrok-cli").1
    }

    pub fn cmd(&self, args: &[&str]) -> Command {
        let dir = self.dir.path();
        let mut c = Command::new(env!("CARGO_BIN_EXE_desk"));
        c.env_remove("DESK_CONFIG")
            .env(
                "DESK_MODELS",
                Path::new(env!("CARGO_MANIFEST_DIR")).join("models"),
            )
            .env("PATH", dir.join("bin"))
            .env("FAKE_SIGROK_DIR", &self.fake.dir)
            .env("DESK_MOCK_STATE", dir.join("mock-state.json"))
            .env("DESK_RUNTIME_DIR", dir.join("run"))
            .env("DESK_DAEMON_IDLE", "1s")
            .env("DESK_CAPTURE_DIR", self.captures())
            .arg("--config")
            .arg(dir.join("desk.toml"))
            .args(args);
        c
    }

    pub fn run(&self, args: &[&str]) -> Out {
        let out = self.cmd(args).output().unwrap();
        Out {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }
}

/// Every file below `dir` (relative paths, sorted); empty if it doesn't exist.
pub fn files(dir: &Path) -> Vec<String> {
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
