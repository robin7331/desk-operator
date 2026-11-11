//! sigrok as a connection kind: desk runs a `sigrok-cli` program as a child process and never
//! links libsigrok (GPLv3+; desk is MIT OR Apache-2.0). Nothing is bundled or downloaded.
//!
//! * A **backend** is a `sigrok-cli` program: the implicit `sigrok` (found on PATH), or a provider
//!   `kind = "sigrok"` with its own `program` (a vendor build).
//! * A **binding** names a sigrok driver: `sigrok = { driver, build, conn, backend }` on a script
//!   binding of any capability, like `port`. `driver` and `build` come from the model; `conn` and
//!   `backend` are per bench.
//! * The driver script fills a capture with [`acquire`] (`ctx.sigrok.acquire{ … }`). The core
//!   builds the command line, one checked argument per element, no shell: a script can never pass
//!   raw arguments, a path or a program.
//!
//! stderr never decides success (stock builds print Python decoder warnings on every run): the
//! exit code does, and stderr only becomes the error text when the program failed.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::io::AsyncReadExt;

use crate::capture::Output;
use crate::config::ProviderConfig;
use crate::error::{DeskError, Result};
use crate::providers::BuildContext;

/// The provider kind of a sigrok backend.
pub const KIND: &str = "sigrok";
/// The backend that exists without configuration: `sigrok-cli` on PATH.
pub const IMPLICIT: &str = "sigrok";
/// The program the implicit backend runs (and a configured one without `program`).
pub const PROGRAM: &str = "sigrok-cli";
/// Timeout of the short queries (version, drivers, scan, show) unless the provider sets one.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// The message of a missing program (followed by where it was looked for).
const PROGRAM_MISSING: &str = "sigrok-cli not found";
/// The hint a missing program's error carries.
const PROGRAM_MISSING_HINT: &str = "install sigrok (sigrok-cli) from your package manager or https://sigrok.org, or set `program` in [providers.<id>] kind = \"sigrok\" (docs/sigrok.md)";

/// The most of a query's stdout that is kept (a `--show` or `-L` is a few KiB).
const MAX_STDOUT: usize = 1 << 20;
/// The most of stderr that is read back for an error message (its tail).
const MAX_STDERR: usize = 64 * 1024;
/// Meaningful stderr lines kept in an error message.
const ERROR_LINES: usize = 8;

// ---------------------------------------------------------------------------------------------
// The binding

/// `sigrok = { driver, build, conn, backend }` of a script binding.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SigrokSpec {
    /// sigrok's driver id, e.g. `demo`.
    pub driver: String,
    /// Where a sigrok build with this driver comes from (models; only shown in hints).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    /// sigrok's connection string for this unit (`-d <driver>:conn=<conn>`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conn: Option<String>,
    /// The backend (provider id) to run it with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
}

/// The keys a model may give (`conn` and `backend` are per bench).
pub const MODEL_KEYS: &[&str] = &["driver", "build"];

/// sigrok ids (drivers, output modules): lowercase letters, digits, `-`, `_`, not starting with
/// `-`.
pub fn valid_driver(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && !s.starts_with('-')
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// A connection string: no whitespace or control characters (`/`, `.`, `:` and `=` are fine,
/// e.g. `/dev/ttyUSB0:serialcomm=9600/8n1`).
pub fn valid_conn(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && !s.starts_with('-')
        && !s.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn valid_build(s: &str) -> bool {
    (s.starts_with("https://") || s.starts_with("http://"))
        && s.len() > "https://".len()
        && !s.chars().any(|c| c.is_whitespace() || c.is_control())
}

impl TryFrom<toml::Table> for SigrokSpec {
    type Error = String;

    fn try_from(t: toml::Table) -> std::result::Result<Self, String> {
        let mut driver = None;
        let mut spec = SigrokSpec {
            driver: String::new(),
            build: None,
            conn: None,
            backend: None,
        };
        for (k, v) in t {
            let toml::Value::String(s) = v else {
                return Err(format!("sigrok.{k} must be a string"));
            };
            match k.as_str() {
                "driver" if valid_driver(&s) => driver = Some(s),
                "driver" => {
                    return Err(format!(
                        "sigrok.driver `{s}` is not a sigrok driver id (lowercase letters, \
                         digits, `-`, `_`; `desk sigrok drivers` lists them)"
                    ));
                }
                "build" if valid_build(&s) => spec.build = Some(s),
                "build" => return Err(format!("sigrok.build `{s}` must be an http(s) URL")),
                "conn" if valid_conn(&s) => spec.conn = Some(s),
                "conn" => {
                    return Err(format!(
                        "sigrok.conn `{s}` can't contain spaces or control characters"
                    ));
                }
                "backend" if crate::config::valid_id(&s) => spec.backend = Some(s),
                "backend" => {
                    return Err(format!(
                        "sigrok.backend `{s}` must be a provider id (letters, digits, `-`, `_`)"
                    ));
                }
                other => {
                    return Err(format!(
                        "unknown key `sigrok.{other}` (allowed: driver, build, conn, backend)"
                    ));
                }
            }
        }
        spec.driver = driver.ok_or("`sigrok` needs `driver = \"…\"` (sigrok's driver id)")?;
        Ok(spec)
    }
}

impl std::fmt::Display for SigrokSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sigrok {}", self.driver)?;
        if let Some(c) = &self.conn {
            write!(f, " conn={c}")?;
        }
        if let Some(b) = &self.backend {
            write!(f, " via {b}")?;
        }
        Ok(())
    }
}

/// Check a model's `sigrok` table: `driver` (required) and `build`; nothing per bench.
pub fn check_model_table(t: &toml::Value) -> std::result::Result<(), String> {
    let toml::Value::Table(t) = t else {
        return Err("`sigrok` must be a table like { driver = \"…\" }".into());
    };
    for k in t.keys() {
        if !MODEL_KEYS.contains(&k.as_str()) {
            return Err(match k.as_str() {
                "conn" | "backend" => format!(
                    "`sigrok.{k}` is per bench: it belongs in the user's desk.toml, not in a model"
                ),
                other => format!("unknown key `sigrok.{other}` (a model gives: driver, build)"),
            });
        }
    }
    SigrokSpec::try_from(t.clone()).map(drop)
}

// ---------------------------------------------------------------------------------------------
// Backends

/// One `sigrok-cli` program desk can run.
#[derive(Debug, Clone)]
pub struct Backend {
    pub id: String,
    /// The program (resolved), or what was looked for.
    pub program: PathBuf,
    /// Whether the program exists.
    pub found: bool,
    pub timeout: Duration,
    /// The implicit backend (no `[providers.sigrok]`).
    pub implicit: bool,
}

/// Look `name` up on PATH (with `.exe` on Windows).
fn on_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    let names: Vec<String> = if cfg!(windows) {
        vec![format!("{name}.exe"), name.to_string()]
    } else {
        vec![name.to_string()]
    };
    std::env::split_paths(&paths)
        .filter(|d| !d.as_os_str().is_empty())
        .flat_map(|d| names.iter().map(move |n| d.join(n)))
        .find(|p| p.is_file())
}

impl Backend {
    /// The implicit backend: `sigrok-cli` on PATH. The same as a configured backend without
    /// settings, built as the implicit provider.
    pub fn implicit() -> Backend {
        let found = on_path(PROGRAM);
        Backend {
            id: IMPLICIT.into(),
            found: found.is_some(),
            program: found.unwrap_or_else(|| PathBuf::from(PROGRAM)),
            timeout: DEFAULT_TIMEOUT,
            implicit: true,
        }
    }

    /// A `[providers.<id>] kind = "sigrok"`: `program` (relative to the config folder, `~/`
    /// expanded; default `sigrok-cli` on PATH) and `timeout` for the short queries.
    pub fn configured(id: &str, cfg: &ProviderConfig, ctx: &BuildContext) -> Result<Backend> {
        let p = crate::providers::Params::new(&cfg.settings, format!("provider `{id}`"));
        p.only(&["program", "timeout"])?;
        let timeout = p.duration("timeout")?.unwrap_or(DEFAULT_TIMEOUT);
        let (program, found) = match p.str("program")? {
            None => match on_path(PROGRAM) {
                Some(found) => (found, true),
                None => (PathBuf::from(PROGRAM), false),
            },
            Some(s) if s.trim().is_empty() => {
                return Err(DeskError::ConfigInvalid(format!(
                    "provider `{id}`: `program` is empty"
                )));
            }
            Some(s) => {
                let path = ctx.resolve_path(s);
                let found = path.is_file();
                (path, found)
            }
        };
        Ok(Backend {
            id: id.into(),
            program,
            found,
            timeout,
            implicit: ctx.implicit,
        })
    }

    fn missing(&self) -> DeskError {
        let how = if self.implicit {
            "on PATH".to_string()
        } else {
            format!("at {}", self.program.display())
        };
        DeskError::ProviderUnreachable {
            provider: self.id.clone(),
            message: format!("{PROGRAM_MISSING} {how}"),
            hint: Some(PROGRAM_MISSING_HINT),
        }
    }

    fn command(&self, args: &[OsString]) -> Result<tokio::process::Command> {
        if !self.found {
            return Err(self.missing());
        }
        let mut c = tokio::process::Command::new(&self.program);
        c.args(args)
            // Some builds stop when stdin ends: it stays open until the program exits.
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        Ok(c)
    }

    /// Run a short query (`--version`, `-L`, `--scan`, `--show`) and return its stdout.
    pub async fn query(&self, args: &[&str]) -> Result<String> {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        let cmd = self.command(&args)?;
        let what = format!("`{}` ({})", args_text(&args), self.id);
        match run(cmd, self, self.timeout, None).await {
            Ok(Ran {
                ok: true, stdout, ..
            }) => Ok(stdout),
            Ok(ran) => Err(DeskError::ProviderUnreachable {
                provider: self.id.clone(),
                message: format!("{what} failed: {}", ran.error_text()),
                hint: None,
            }),
            Err(Stop::Timeout) => Err(DeskError::Timeout {
                what: format!("sigrok-cli {what}"),
                after: self.timeout,
            }),
            Err(Stop::Failed(e)) => Err(e),
        }
    }

    /// The first line of `--version`, e.g. `sigrok-cli 0.7.2`.
    pub async fn version(&self) -> Result<String> {
        let out = self.query(&["--version"]).await?;
        Ok(out
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("unknown version")
            .to_string())
    }

    /// The drivers this backend has (`-L`), asked once per process and backend.
    pub async fn drivers(&self) -> Result<Arc<Vec<DriverInfo>>> {
        let key = (self.id.clone(), self.program.clone());
        if let Some(d) = lock(&DRIVERS).get(&key) {
            return Ok(d.clone());
        }
        let list = Arc::new(parse_drivers(&self.query(&["-L"]).await?));
        lock(&DRIVERS).insert(key, list.clone());
        Ok(list)
    }

    /// What `--scan` finds (all drivers, or one).
    pub async fn scan(&self, driver: Option<&str>) -> Result<Vec<Found>> {
        let mut args = Vec::new();
        if let Some(d) = driver {
            args.extend(["-d", d]);
        }
        args.push("--scan");
        Ok(parse_scan(&self.query(&args).await?))
    }

    /// sigrok's own `--show` text for a driver (and connection), unparsed.
    pub async fn show(&self, driver: &str, conn: Option<&str>) -> Result<String> {
        let d = device_arg(driver, conn);
        let out = self.query(&["-d", &d, "--show"]).await;
        out.map_err(|e| self.map_failure(e, driver, conn, None))
    }

    /// A failed query or capture as the error that says what to do.
    fn map_failure(
        &self,
        e: DeskError,
        driver: &str,
        conn: Option<&str>,
        build: Option<&str>,
    ) -> DeskError {
        let text = match &e {
            DeskError::ProviderUnreachable { message, .. } | DeskError::ScriptFailed(message) => {
                message.clone()
            }
            _ => return e,
        };
        classify(&text, &self.id, driver, conn, build).unwrap_or(e)
    }
}

/// `driver` or `driver:conn=<conn>`, one argument.
fn device_arg(driver: &str, conn: Option<&str>) -> String {
    match conn {
        Some(c) => format!("{driver}:conn={c}"),
        None => driver.to_string(),
    }
}

/// Known failures in sigrok's text: busy, not found, a missing driver.
fn classify(
    text: &str,
    backend: &str,
    driver: &str,
    conn: Option<&str>,
    build: Option<&str>,
) -> Option<DeskError> {
    let endpoint = device_arg(driver, conn);
    if text.contains("Unable to claim USB interface") {
        return Some(DeskError::EndpointUnavailable {
            endpoint,
            message: format!(
                "the device is busy: another program uses it (close it and try again): {text}"
            ),
        });
    }
    if text.contains("No devices found") {
        return Some(DeskError::EndpointUnavailable {
            endpoint,
            message: format!(
                "sigrok ({backend}) found no such device: is it plugged in and powered, is \
                 `conn` right, and may this user open it (udev rules)?"
            ),
        });
    }
    if text.contains(&format!("Driver {driver} not found")) {
        return Some(no_driver(driver, &[backend.to_string()], build));
    }
    None
}

/// `1 driver`, `162 drivers`.
pub fn drivers_text(n: usize) -> String {
    if n == 1 {
        "1 driver".into()
    } else {
        format!("{n} drivers")
    }
}

/// No backend has the driver.
fn no_driver(driver: &str, asked: &[String], build: Option<&str>) -> DeskError {
    let asked = if asked.is_empty() {
        "none is configured".to_string()
    } else {
        format!("asked: {}", asked.join(", "))
    };
    let get = match build {
        Some(url) => format!(
            "get a sigrok build with this driver from {url} and add it as \
             `[providers.<id>] kind = \"sigrok\"` with `program = \"…\"`"
        ),
        None => "install a sigrok build that has it, and add it as `[providers.<id>] kind = \
                 \"sigrok\"` with `program = \"…\"` if it isn't stock sigrok-cli"
            .to_string(),
    };
    DeskError::ConfigInvalid(format!(
        "no sigrok backend has the driver `{driver}` ({asked}); {get} (see `desk sigrok drivers`)"
    ))
}

type DriverKey = (String, PathBuf);
static DRIVERS: LazyLock<Mutex<HashMap<DriverKey, Arc<Vec<DriverInfo>>>>> =
    LazyLock::new(Default::default);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// How a `[providers.<id>]` backend is built.
fn configured(base_dir: &Path) -> BuildContext<'_> {
    BuildContext {
        base_dir,
        implicit: false,
    }
}

/// Every sigrok backend, in resolution order: `sigrok` first (the configured one, else the
/// implicit one on PATH), then the configured ones in config order. `all` keeps an implicit
/// backend whose program isn't on PATH.
pub fn backends(
    configs: &indexmap::IndexMap<String, ProviderConfig>,
    base_dir: &Path,
    all: bool,
) -> Vec<Result<Backend>> {
    let mut out = Vec::new();
    match configs.get(IMPLICIT) {
        Some(cfg) if cfg.kind == KIND => {
            out.push(Backend::configured(IMPLICIT, cfg, &configured(base_dir)));
        }
        Some(_) => {}
        None => {
            let b = Backend::implicit();
            if all || b.found {
                out.push(Ok(b));
            }
        }
    }
    for (id, cfg) in configs {
        if id != IMPLICIT && cfg.kind == KIND {
            out.push(Backend::configured(id, cfg, &configured(base_dir)));
        }
    }
    out
}

/// One backend by id: a configured `kind = "sigrok"`, or the implicit `sigrok`.
pub fn backend(
    configs: &indexmap::IndexMap<String, ProviderConfig>,
    base_dir: &Path,
    id: &str,
) -> Result<Backend> {
    match configs.get(id) {
        Some(cfg) if cfg.kind == KIND => Backend::configured(id, cfg, &configured(base_dir)),
        Some(cfg) => Err(DeskError::ConfigInvalid(format!(
            "provider `{id}` is a `{}` provider, not a sigrok backend (kind = \"sigrok\")",
            cfg.kind
        ))),
        None if crate::providers::kind_of(id, configs) == Some(KIND) => Ok(Backend::implicit()),
        None => Err(DeskError::ConfigInvalid(format!(
            "no sigrok backend `{id}`: define `[providers.{id}] kind = \"sigrok\"`, or use \
             the implicit `{IMPLICIT}` (sigrok-cli on PATH)"
        ))),
    }
}

/// Which backend runs a binding: its `backend`; otherwise the only one; otherwise the first
/// whose driver list has the driver (`sigrok` first, then config order).
pub async fn resolve(
    configs: &indexmap::IndexMap<String, ProviderConfig>,
    base_dir: &Path,
    spec: &SigrokSpec,
) -> Result<Backend> {
    if let Some(id) = &spec.backend {
        return backend(configs, base_dir, id);
    }
    let mut candidates = Vec::new();
    for b in backends(configs, base_dir, false) {
        candidates.push(b?);
    }
    match candidates.len() {
        // Nothing on PATH and nothing configured: say how to get it.
        0 => return Err(Backend::implicit().missing()),
        // The run itself reports a missing driver.
        1 => return Ok(candidates.remove(0)),
        _ => {}
    }
    let mut asked = Vec::new();
    for b in candidates {
        match b.drivers().await {
            Ok(list) if list.iter().any(|d| d.id == spec.driver) => return Ok(b),
            Ok(_) => asked.push(b.id.clone()),
            Err(e) => asked.push(format!("{} ({e})", b.id)),
        }
    }
    Err(no_driver(&spec.driver, &asked, spec.build.as_deref()))
}

// ---------------------------------------------------------------------------------------------
// Parsing sigrok-cli's output

/// A driver from `-L`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DriverInfo {
    pub id: String,
    pub description: String,
}

/// The `Supported hardware drivers:` section of `sigrok-cli -L`.
pub fn parse_drivers(text: &str) -> Vec<DriverInfo> {
    let mut out = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        if !inside {
            inside = line.trim() == "Supported hardware drivers:";
            continue;
        }
        // The section ends at the next header (or a blank line).
        if line.trim().is_empty() || !line.starts_with(char::is_whitespace) {
            break;
        }
        let line = line.trim();
        let (id, desc) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        out.push(DriverInfo {
            id: id.to_string(),
            description: desc.trim().to_string(),
        });
    }
    out
}

/// A device from `--scan`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Found {
    pub driver: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conn: Option<String>,
    pub description: String,
}

/// `demo - Demo device with 13 channels: …` or `<driver>:conn=<conn> - …`, one per line after
/// `The following devices were found:`.
pub fn parse_scan(text: &str) -> Vec<Found> {
    text.lines()
        .filter_map(|line| {
            let (id, desc) = line.split_once(" - ")?;
            let id = id.trim();
            if id.is_empty() || id.contains(char::is_whitespace) {
                return None;
            }
            let (driver, rest) = id.split_once(':').unwrap_or((id, ""));
            if !valid_driver(driver) {
                return None;
            }
            let conn = rest
                .split(':')
                .find_map(|p| p.strip_prefix("conn="))
                .map(str::to_string);
            Some(Found {
                driver: driver.to_string(),
                conn,
                description: desc.trim().to_string(),
            })
        })
        .collect()
}

/// stderr without the decoder warnings stock builds print (a `…Warning:` line and the source
/// line under it), its last meaningful lines.
pub fn clean_stderr(text: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut after_warning = false;
    for line in text.lines() {
        if line.contains("Warning:") {
            after_warning = true;
            continue;
        }
        if after_warning && line.starts_with(char::is_whitespace) {
            continue;
        }
        after_warning = false;
        if !line.trim().is_empty() {
            kept.push(line.trim_end());
        }
    }
    let start = kept.len().saturating_sub(ERROR_LINES);
    let mut s = kept[start..].join("\n");
    if s.len() > 2000 {
        let mut cut = s.len() - 2000;
        while !s.is_char_boundary(cut) {
            cut += 1;
        }
        s = format!("…{}", &s[cut..]);
    }
    s
}

// ---------------------------------------------------------------------------------------------
// Running the program

struct Ran {
    ok: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Ran {
    fn error_text(&self) -> String {
        let mut text = clean_stderr(&self.stderr);
        if text.is_empty() {
            text = clean_stderr(&self.stdout);
        }
        if text.is_empty() {
            text = "no message".into();
        }
        match self.code {
            Some(c) => format!("{text} (exit {c})"),
            None => format!("{text} (killed by a signal)"),
        }
    }
}

enum Stop {
    Timeout,
    /// The size cap, or the program couldn't be started.
    Failed(DeskError),
}

/// Read a pipe to its end, keeping at most `max` bytes (the head, or the tail with `tail`).
async fn read_bounded<R: tokio::io::AsyncRead + Unpin>(
    pipe: Option<R>,
    max: usize,
    tail: bool,
) -> String {
    let Some(mut pipe) = pipe else {
        return String::new();
    };
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if tail {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.len() > max {
                        buf.drain(..buf.len() - max);
                    }
                } else if buf.len() < max {
                    let room = max - buf.len();
                    buf.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Run the program to its end: killed on timeout, when `guard` (a capture's size cap) fires, and
/// when the future is dropped (cancel). stdin stays open until it exits.
async fn run(
    mut cmd: tokio::process::Command,
    backend: &Backend,
    timeout: Duration,
    guard: Option<&Output>,
) -> std::result::Result<Ran, Stop> {
    let mut child = cmd.spawn().map_err(|e| {
        Stop::Failed(if e.kind() == std::io::ErrorKind::NotFound {
            backend.missing()
        } else {
            DeskError::ProviderUnreachable {
                provider: backend.id.clone(),
                message: format!("cannot run {}: {e}", backend.program.display()),
                hint: None,
            }
        })
    })?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let result = {
        let work = async {
            let (out, err) = tokio::join!(
                read_bounded(stdout, MAX_STDOUT, false),
                read_bounded(stderr, MAX_STDERR, true)
            );
            let status = child.wait().await;
            (out, err, status)
        };
        let cap = async {
            match guard {
                Some(out) => out.guard().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            (stdout, stderr, status) = work => match status {
                Ok(s) => Ok(Ran { ok: s.success(), code: s.code(), stdout, stderr }),
                Err(e) => Err(Stop::Failed(DeskError::Internal(format!(
                    "waiting for {}: {e}",
                    backend.program.display()
                )))),
            },
            _ = tokio::time::sleep(timeout) => Err(Stop::Timeout),
            e = cap => Err(Stop::Failed(e)),
        }
    };
    if result.is_err() {
        // Kill and reap before the sink removes its folder.
        let _ = child.kill().await;
    }
    drop(stdin);
    result
}

fn args_text(args: &[OsString]) -> String {
    args.iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------------------------
// Captures

/// How long a capture runs: `--time <ms>`, `--samples <n>` or `--frames <n>`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Length {
    Time(u64),
    Samples(u64),
    Frames(u64),
}

/// What `ctx.sigrok.acquire{ … }` asked for, checked.
#[derive(Debug, Clone, PartialEq)]
pub struct Acquire {
    /// sigrok config keys and values, already as text, sorted by key.
    pub config: Vec<(String, String)>,
    pub channels: Vec<String>,
    pub length: Length,
    pub triggers: Option<String>,
    pub wait_trigger: bool,
    pub format: String,
    pub timeout: Duration,
}

/// The `--config` key: lowercase letters, digits, `_`.
pub fn check_config_key(k: &str) -> std::result::Result<(), String> {
    let ok = !k.is_empty()
        && k.len() <= 64
        && k.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "config key `{k}` must be lowercase letters, digits and `_` (sigrok's names, e.g. \
             samplerate)"
        ))
    }
}

/// Text that becomes part of one argument: no whitespace, control characters or the characters
/// in `forbidden`; not empty; not starting with `-`.
fn check_text(what: &str, v: &str, forbidden: &[char]) -> std::result::Result<(), String> {
    if v.is_empty() {
        return Err(format!("{what} is empty"));
    }
    if v.len() > 256 {
        return Err(format!("{what} is too long"));
    }
    if v.starts_with('-') {
        return Err(format!("{what} `{v}` can't start with `-`"));
    }
    if let Some(c) = v
        .chars()
        .find(|c| c.is_whitespace() || c.is_control() || forbidden.contains(c))
    {
        return Err(format!("{what} `{}` can't contain {c:?}", v.escape_debug()));
    }
    Ok(())
}

/// A config value: no `:`, `=`, whitespace or control characters, not starting with `-`.
pub fn check_config_value(k: &str, v: &str) -> std::result::Result<(), String> {
    check_text(&format!("config value of `{k}`"), v, &[':', '='])
}

/// A number as sigrok reads it: whole numbers without a fraction or exponent.
pub fn number_text(n: f64) -> std::result::Result<String, String> {
    if !n.is_finite() {
        return Err("numbers must be finite".into());
    }
    if n.fract() == 0.0 && n.abs() < 9.0e15 {
        return Ok(format!("{}", n as i64));
    }
    // Rust never prints an exponent for f64.
    Ok(format!("{n}"))
}

/// A channel name (or a range like `D0-D7`).
pub fn check_channel(c: &str) -> std::result::Result<(), String> {
    let ok = !c.is_empty()
        && c.len() <= 64
        && !c.starts_with('-')
        && c.chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "channel `{}` must be letters, digits, `_`, `.`, `-` (e.g. D0 or D0-D7)",
            c.escape_debug()
        ))
    }
}

/// Triggers (`D0=r,D1=1`): like a config value, but `=` and `,` are allowed.
pub fn check_triggers(t: &str) -> std::result::Result<(), String> {
    check_text("triggers", t, &[':'])
}

/// An output module id (`srzip`, `binary`, `csv`, …).
pub fn check_format(f: &str) -> std::result::Result<(), String> {
    let ok = !f.is_empty()
        && f.len() <= 32
        && f.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "format `{}` must be a sigrok output module id like srzip or binary",
            f.escape_debug()
        ))
    }
}

impl Acquire {
    /// The command line for this capture into `path`: one argument per element.
    pub fn args(&self, spec: &SigrokSpec, path: &Path) -> Vec<OsString> {
        let mut a: Vec<OsString> = vec![
            "-d".into(),
            device_arg(&spec.driver, spec.conn.as_deref()).into(),
        ];
        if !self.config.is_empty() {
            let joined: Vec<String> = self
                .config
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            a.push("--config".into());
            a.push(joined.join(":").into());
        }
        if !self.channels.is_empty() {
            a.push("--channels".into());
            a.push(self.channels.join(",").into());
        }
        if let Some(t) = &self.triggers {
            a.push("--triggers".into());
            a.push(t.into());
        }
        if self.wait_trigger {
            a.push("--wait-trigger".into());
        }
        let (flag, n) = match self.length {
            Length::Time(ms) => ("--time", ms),
            Length::Samples(n) => ("--samples", n),
            Length::Frames(n) => ("--frames", n),
        };
        a.push(flag.into());
        a.push(n.to_string().into());
        a.push("-O".into());
        a.push(self.format.clone().into());
        a.push("-o".into());
        a.push(path.as_os_str().to_owned());
        a
    }
}

/// Run a capture into `out` for `device`: resolve the backend, run it under the capture's own
/// timeout and the size cap. Returns the size of the capture.
pub async fn acquire(
    configs: &indexmap::IndexMap<String, ProviderConfig>,
    base_dir: &Path,
    device: &str,
    spec: &SigrokSpec,
    req: &Acquire,
    out: &Output,
) -> Result<u64> {
    let backend = resolve(configs, base_dir, spec).await?;
    let args = req.args(spec, out.temp_path());
    let mut cmd = backend.command(&args)?;
    // stdout carries nothing a capture needs.
    cmd.stdout(Stdio::null());
    let fail = |text: String| {
        let e = DeskError::ScriptFailed(format!(
            "sigrok-cli ({}) failed for `{device}`: {text}",
            backend.id
        ));
        backend.map_failure(e, &spec.driver, spec.conn.as_deref(), spec.build.as_deref())
    };
    match run(cmd, &backend, req.timeout, Some(out)).await {
        Ok(ran) if ran.ok => out.check_size(),
        Ok(ran) => Err(fail(ran.error_text())),
        Err(Stop::Timeout) => Err(DeskError::Timeout {
            what: format!("the sigrok capture of `{device}`"),
            after: req.timeout,
        }),
        Err(Stop::Failed(e)) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(conn: Option<&str>) -> SigrokSpec {
        SigrokSpec {
            driver: "demo".into(),
            build: None,
            conn: conn.map(Into::into),
            backend: None,
        }
    }

    fn req() -> Acquire {
        Acquire {
            config: vec![],
            channels: vec![],
            length: Length::Samples(1000),
            triggers: None,
            wait_trigger: false,
            format: "srzip".into(),
            timeout: Duration::from_secs(5),
        }
    }

    fn text(a: Vec<OsString>) -> Vec<String> {
        a.into_iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn the_command_line_is_one_argument_per_element() {
        let path = Path::new("/c/.x.partial/data");
        assert_eq!(
            text(req().args(&spec(None), path)),
            [
                "-d",
                "demo",
                "--samples",
                "1000",
                "-O",
                "srzip",
                "-o",
                "/c/.x.partial/data"
            ]
        );
        let full = Acquire {
            config: vec![
                ("averaging".into(), "false".into()),
                ("samplerate".into(), "10000000".into()),
            ],
            channels: vec!["D0".into(), "D1-D3".into()],
            length: Length::Time(2000),
            triggers: Some("D0=r,D1=1".into()),
            wait_trigger: true,
            format: "binary".into(),
            timeout: Duration::from_secs(5),
        };
        assert_eq!(
            text(full.args(&spec(Some("/dev/ttyUSB0:serialcomm=9600/8n1")), path)),
            [
                "-d",
                "demo:conn=/dev/ttyUSB0:serialcomm=9600/8n1",
                "--config",
                "averaging=false:samplerate=10000000",
                "--channels",
                "D0,D1-D3",
                "--triggers",
                "D0=r,D1=1",
                "--wait-trigger",
                "--time",
                "2000",
                "-O",
                "binary",
                "-o",
                "/c/.x.partial/data"
            ]
        );
        let frames = Acquire {
            length: Length::Frames(3),
            ..req()
        };
        assert!(text(frames.args(&spec(None), path)).contains(&"--frames".to_string()));
    }

    #[test]
    fn numbers_print_as_sigrok_reads_them() {
        assert_eq!(number_text(10e6).unwrap(), "10000000");
        assert_eq!(number_text(1e7).unwrap(), "10000000");
        assert_eq!(number_text(250.0).unwrap(), "250");
        assert_eq!(number_text(0.5).unwrap(), "0.5");
        assert_eq!(number_text(1e-7).unwrap(), "0.0000001");
        assert_eq!(number_text(-3.0).unwrap(), "-3");
        assert!(number_text(f64::NAN).is_err());
        assert!(number_text(f64::INFINITY).is_err());
    }

    #[test]
    fn hostile_values_are_refused() {
        for k in ["samplerate", "limit_samples", "a1"] {
            assert!(check_config_key(k).is_ok(), "{k}");
        }
        for k in ["", "Samplerate", "a-b", "a=b", "a:b", "--x", "a b", "ä"] {
            assert!(check_config_key(k).is_err(), "{k}");
        }
        for v in ["1m", "10000000", "off", "true", "1.5"] {
            assert!(check_config_value("k", v).is_ok(), "{v}");
        }
        for v in [
            "",
            "1m:limit_samples=1",
            "a=b",
            "-o",
            "--output-file",
            "a b",
            "a\nb",
            "a\u{0}",
            "a\tb",
        ] {
            assert!(check_config_value("k", v).is_err(), "{v:?}");
        }
        for c in ["D0", "A4", "D0-D7", "CH1.x", "logic_0"] {
            assert!(check_channel(c).is_ok(), "{c}");
        }
        for c in [
            "", "-D0", "D0,D1", "D0 D1", "D0=r", "D0:x", "D0\n", "../x", "a/b",
        ] {
            assert!(check_channel(c).is_err(), "{c:?}");
        }
        for t in ["D0=r", "D0=r,D1=1", "D0=0,D1=f"] {
            assert!(check_triggers(t).is_ok(), "{t}");
        }
        for t in ["", "-D0=r", "D0=r D1", "D0=r:x", "D0=r\n"] {
            assert!(check_triggers(t).is_err(), "{t:?}");
        }
        for f in ["srzip", "binary", "csv", "vcd", "analog"] {
            assert!(check_format(f).is_ok(), "{f}");
        }
        for f in ["", "csv:header=false", "-o", "a b", "../x", "Srzip", "a/b"] {
            assert!(check_format(f).is_err(), "{f:?}");
        }
    }

    #[test]
    fn bindings_check_their_keys() {
        let t = |s: &str| -> toml::Table { toml::from_str(s).unwrap() };
        let s = SigrokSpec::try_from(t(
            "driver = \"fx2lafw\"\nconn = \"/dev/ttyUSB0:serialcomm=9600/8n1\"\nbackend = \"vendor\"",
        ))
        .unwrap();
        assert_eq!(s.driver, "fx2lafw");
        assert_eq!(s.conn.as_deref(), Some("/dev/ttyUSB0:serialcomm=9600/8n1"));
        assert_eq!(
            s.to_string(),
            "sigrok fx2lafw conn=/dev/ttyUSB0:serialcomm=9600/8n1 via vendor"
        );
        for bad in [
            "conn = \"1.2\"",
            "driver = \"Demo\"",
            "driver = \"-d\"",
            "driver = \"demo x\"",
            "driver = \"demo\"\nconn = \"a b\"",
            "driver = \"demo\"\nconn = \"-x\"",
            "driver = \"demo\"\nbuild = \"ftp://x\"",
            "driver = \"demo\"\nbackend = \"a b\"",
            "driver = \"demo\"\nprogram = \"/bin/sh\"",
            "driver = 3",
        ] {
            assert!(SigrokSpec::try_from(t(bad)).is_err(), "{bad}");
        }
        let model = |s: &str| check_model_table(&toml::Value::Table(t(s)));
        assert!(model("driver = \"demo\"\nbuild = \"https://example.org/x\"").is_ok());
        assert!(
            model("driver = \"demo\"\nconn = \"1.2\"")
                .unwrap_err()
                .contains("per bench")
        );
        assert!(
            model("driver = \"demo\"\nbackend = \"v\"")
                .unwrap_err()
                .contains("per bench")
        );
        assert!(model("build = \"https://example.org/x\"").is_err());
    }

    #[test]
    fn parses_what_sigrok_cli_prints() {
        let l = "sigrok-cli 0.7.2\n\nSupported hardware drivers:\n  agilent-dmm          Agilent U12xx series DMMs\n  arachnid-labs-re-load-pro Arachnid Labs Re:load Pro\n  demo                 Demo driver and pattern generator\n\nSupported input formats:\n  binary               Raw binary logic data\n";
        let d = parse_drivers(l);
        assert_eq!(d.len(), 3);
        assert_eq!(d[1].id, "arachnid-labs-re-load-pro");
        assert_eq!(d[1].description, "Arachnid Labs Re:load Pro");
        assert_eq!(d[2].id, "demo");
        let s = parse_scan(
            "The following devices were found:\ndemo - Demo device with 13 channels: D0 D1\nfx2lafw:conn=1.23 - Saleae Logic with 8 channels: D0\n",
        );
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].driver, "demo");
        assert_eq!(s[0].conn, None);
        assert_eq!(s[1].driver, "fx2lafw");
        assert_eq!(s[1].conn.as_deref(), Some("1.23"));
        assert!(parse_scan("").is_empty());
    }

    #[test]
    fn driver_counts_read_right() {
        assert_eq!(drivers_text(1), "1 driver");
        assert_eq!(drivers_text(0), "0 drivers");
        assert_eq!(drivers_text(162), "162 drivers");
    }

    #[test]
    fn decoder_warnings_are_not_errors() {
        let noisy = "/usr/share/x/pd.py:215: SyntaxWarning: \"\\s\" is an invalid escape\n  instpat = re.compile('\\s*')\nunknown channel 'Z9'.\n";
        assert_eq!(clean_stderr(noisy), "unknown channel 'Z9'.");
        let many: String = (0..30).map(|i| format!("line {i}\n")).collect();
        assert_eq!(clean_stderr(&many).lines().count(), ERROR_LINES);
        assert!(clean_stderr(&"x".repeat(10_000)).len() < 2100);
    }

    #[test]
    fn failures_get_their_codes() {
        let busy = classify(
            "sr: Unable to claim USB interface.",
            "s",
            "demo",
            None,
            None,
        );
        assert_eq!(busy.unwrap().code(), "endpoint_unavailable");
        let none = classify("No devices found.", "s", "demo", Some("1.2"), None).unwrap();
        assert_eq!(none.code(), "endpoint_unavailable");
        assert!(none.to_string().contains("demo:conn=1.2"));
        let drv = classify(
            "Driver demo not found.",
            "s",
            "demo",
            None,
            Some("https://x.org/b"),
        );
        let drv = drv.unwrap();
        assert_eq!(drv.code(), "config_invalid");
        assert!(drv.to_string().contains("https://x.org/b"), "{drv}");
        assert!(classify("unknown channel 'Z9'.", "s", "demo", None, None).is_none());
    }
}
