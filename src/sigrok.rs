//! sigrok as a connection kind: desk runs a `sigrok-cli` program as a child process and never
//! links libsigrok (GPLv3+; desk is MIT OR Apache-2.0). Nothing is bundled or downloaded.
//!
//! * A **backend** is a `sigrok-cli` program: the implicit `sigrok` (found on PATH), or a provider
//!   `kind = "sigrok"` with its own `program` (a vendor build).
//! * A **binding** names a sigrok driver: `sigrok = { driver, build, conn, backend }` on a script
//!   binding of any capability, like `port`. `driver` and `build` come from the model; `conn` and
//!   `backend` are per bench.
//! * The driver script fills a capture with [`acquire`] (`ctx.sigrok.acquire{ … }`), takes
//!   readings with [`measure`] and reads and changes config keys with [`get`] and [`set`]. The
//!   core builds the command line, one checked argument per element, no shell: a script can never
//!   pass raw arguments, a path or a program. desk keeps no state between runs.
//!
//! stderr never decides success (stock builds print Python decoder warnings on every run): the
//! exit code does, and stderr only becomes the error text when the program failed.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::io::AsyncReadExt;

use crate::capture::Output;
use crate::config::ProviderConfig;
use crate::error::{DeskError, Result};
use crate::providers::BuildContext;
use crate::stream::StreamOutput;

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

/// How long a capture runs: `--time <ms>`, `--samples <n>` or `--frames <n>`; a stream may also
/// run until it is stopped (`--continuous`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Length {
    Time(u64),
    Samples(u64),
    Frames(u64),
    Continuous,
}

/// An [`Acquire::timeout`] that means "none" (a stream into a stream sink may leave it out).
pub const NO_TIMEOUT: Duration = Duration::MAX;

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

/// A channel group (`-g`): checked like a channel name.
pub fn check_group(g: &str) -> std::result::Result<(), String> {
    check_channel(g).map_err(|_| {
        format!(
            "group `{}` must be letters, digits, `_`, `.`, `-` (a channel group from `desk sigrok \
             show`, e.g. CH1)",
            g.escape_debug()
        )
    })
}

/// `-d <driver>[:conn=…]`, then `-g <group>` when there is one: how every run starts.
fn device_args(spec: &SigrokSpec, group: Option<&str>) -> Vec<OsString> {
    let mut a: Vec<OsString> = vec![
        "-d".into(),
        device_arg(&spec.driver, spec.conn.as_deref()).into(),
    ];
    if let Some(g) = group {
        a.push("-g".into());
        a.push(g.into());
    }
    a
}

/// `--config k=v:k=v`, nothing for no config.
fn push_config(a: &mut Vec<OsString>, config: &[(String, String)]) {
    if config.is_empty() {
        return;
    }
    let joined: Vec<String> = config.iter().map(|(k, v)| format!("{k}={v}")).collect();
    a.push("--config".into());
    a.push(joined.join(":").into());
}

impl Acquire {
    /// The command line for this capture into `path`: one argument per element.
    pub fn args(&self, spec: &SigrokSpec, path: &Path) -> Vec<OsString> {
        self.args_to(spec, Some(path))
    }

    /// The command line into `path`, or to stdout (no `-o`) for a stream sink.
    pub fn args_to(&self, spec: &SigrokSpec, path: Option<&Path>) -> Vec<OsString> {
        let mut a = device_args(spec, None);
        push_config(&mut a, &self.config);
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
        push_length(&mut a, self.length);
        a.push("-O".into());
        a.push(self.format.clone().into());
        if let Some(path) = path {
            a.push("-o".into());
            a.push(path.as_os_str().to_owned());
        }
        a
    }
}

/// How much of the program's stdout a stream reads at once.
const STREAM_READ: usize = 1 << 20;

/// Run an acquisition into a stream sink: the program writes to its stdout (no `-o`), which the
/// core reads in large chunks into `out`. Ends when the program does, when `out` is cancelled
/// (the stream stopped), or at `req.timeout` (unless [`NO_TIMEOUT`]); the program is killed and
/// reaped on anything but its own end. Returns the bytes it delivered.
pub async fn stream(
    configs: &indexmap::IndexMap<String, ProviderConfig>,
    base_dir: &Path,
    device: &str,
    spec: &SigrokSpec,
    req: &Acquire,
    out: &StreamOutput,
) -> Result<u64> {
    let backend = resolve(configs, base_dir, spec).await?;
    let args = req.args_to(spec, None);
    let mut cmd = backend.command(&args)?;
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            backend.missing()
        } else {
            DeskError::ProviderUnreachable {
                provider: backend.id.clone(),
                message: format!("cannot run {}: {e}", backend.program.display()),
                hint: None,
            }
        }
    })?;
    // stdin stays open until the program exits (`--continuous` stops at once on end of file).
    let stdin = child.stdin.take();
    let mut stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let deadline = Instant::now().checked_add(req.timeout);
    let pump = async {
        let mut delivered = 0u64;
        let Some(pipe) = stdout.as_mut() else {
            return Ok(delivered);
        };
        let mut buf = vec![0u8; STREAM_READ];
        loop {
            match pipe.read(&mut buf).await {
                Ok(0) => return Ok(delivered),
                Ok(n) => {
                    out.write(&buf[..n])?;
                    delivered += n as u64;
                }
                Err(e) => {
                    return Err(DeskError::Internal(format!(
                        "reading from {}: {e}",
                        backend.program.display()
                    )));
                }
            }
        }
    };
    let errors = read_bounded(stderr, MAX_STDERR, true);
    tokio::pin!(pump, errors);
    let mut stderr_text: Option<String> = None;
    let pumped: Result<u64> = loop {
        tokio::select! {
            r = &mut pump => break r,
            t = &mut errors, if stderr_text.is_none() => stderr_text = Some(t),
            _ = out.cancelled() => {
                break Err(DeskError::Cancelled(format!("the stream of `{device}` was stopped")));
            }
            _ = sleep_until(deadline) => {
                break Err(DeskError::Timeout {
                    what: format!("the sigrok stream of `{device}`"),
                    after: req.timeout,
                });
            }
        }
    };
    let delivered = match pumped {
        Ok(n) => n,
        Err(e) => {
            // Killed and reaped before anyone looks for it.
            let _ = child.kill().await;
            drop(stdin);
            return Err(e);
        }
    };
    // Its stdout ended: it is done (or about to be). Wait a little for its exit and stderr.
    let finish = async {
        let text = match stderr_text {
            Some(t) => t,
            None => errors.await,
        };
        (text, child.wait().await)
    };
    let ended = tokio::time::timeout(Duration::from_secs(5), finish).await;
    let (text, status) = match ended {
        Ok(v) => v,
        Err(_) => {
            let _ = child.kill().await;
            drop(stdin);
            return Err(DeskError::Timeout {
                what: format!("sigrok-cli exiting after the stream of `{device}`"),
                after: Duration::from_secs(5),
            });
        }
    };
    drop(stdin);
    let status = status.map_err(|e| {
        DeskError::Internal(format!("waiting for {}: {e}", backend.program.display()))
    })?;
    if status.success() {
        return Ok(delivered);
    }
    let ran = Ran {
        ok: false,
        code: status.code(),
        stdout: String::new(),
        stderr: text,
    };
    Err(failed(&backend, device, spec, ran.error_text()))
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
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
    match run(cmd, &backend, req.timeout, Some(out)).await {
        Ok(ran) if ran.ok => out.check_size(),
        Ok(ran) => Err(failed(&backend, device, spec, ran.error_text())),
        Err(Stop::Timeout) => Err(DeskError::Timeout {
            what: format!("the sigrok capture of `{device}`"),
            after: req.timeout,
        }),
        Err(Stop::Failed(e)) => Err(e),
    }
}

/// A run that failed, as the error that says what to do: busy, not found, a missing driver;
/// otherwise sigrok's own text.
fn failed(backend: &Backend, device: &str, spec: &SigrokSpec, text: String) -> DeskError {
    let e = DeskError::ScriptFailed(format!(
        "sigrok-cli ({}) failed for `{device}`: {text}",
        backend.id
    ));
    backend.map_failure(e, &spec.driver, spec.conn.as_deref(), spec.build.as_deref())
}

// ---------------------------------------------------------------------------------------------
// Values and settings: short runs inside a driver's read() or set()

/// What `ctx.sigrok.get{ … }` asked for, checked: config keys, read in this order.
#[derive(Debug, Clone, PartialEq)]
pub struct Get {
    pub keys: Vec<String>,
    pub group: Option<String>,
    /// `None`: the backend's query timeout.
    pub timeout: Option<Duration>,
}

/// What `ctx.sigrok.set{ … }` asked for, checked.
#[derive(Debug, Clone, PartialEq)]
pub struct Set {
    /// Keys and values as text, sorted by key (at least one).
    pub config: Vec<(String, String)>,
    pub group: Option<String>,
    pub timeout: Option<Duration>,
}

/// What `ctx.sigrok.measure{ … }` asked for, checked.
#[derive(Debug, Clone, PartialEq)]
pub struct Measure {
    /// At least one.
    pub channels: Vec<String>,
    /// `--samples <n>` or `--time <ms>`.
    pub length: Length,
    /// Applied in the same run.
    pub config: Vec<(String, String)>,
    pub group: Option<String>,
    pub timeout: Option<Duration>,
}

impl Get {
    /// One `--get <key>` per key: sigrok prints the keys of one `--get a:b` in an order of its
    /// own, but repeated flags in the order given.
    pub fn args(&self, spec: &SigrokSpec) -> Vec<OsString> {
        let mut a = device_args(spec, self.group.as_deref());
        for k in &self.keys {
            a.push("--get".into());
            a.push(k.into());
        }
        a
    }
}

impl Set {
    pub fn args(&self, spec: &SigrokSpec) -> Vec<OsString> {
        let mut a = device_args(spec, self.group.as_deref());
        push_config(&mut a, &self.config);
        a.push("--set".into());
        a
    }
}

impl Measure {
    /// No `-O`: sigrok's default output prints the analog readings as text (`-O analog` fails on
    /// some builds after printing them).
    pub fn args(&self, spec: &SigrokSpec) -> Vec<OsString> {
        let mut a = device_args(spec, self.group.as_deref());
        push_config(&mut a, &self.config);
        a.push("--channels".into());
        a.push(self.channels.join(",").into());
        push_length(&mut a, self.length);
        a
    }
}

/// `--time <ms>`, `--samples <n>`, `--frames <n>` or `--continuous`.
fn push_length(a: &mut Vec<OsString>, length: Length) {
    let (flag, n) = match length {
        Length::Time(ms) => ("--time", ms),
        Length::Samples(n) => ("--samples", n),
        Length::Frames(n) => ("--frames", n),
        Length::Continuous => {
            a.push("--continuous".into());
            return;
        }
    };
    a.push(flag.into());
    a.push(n.to_string().into());
}

/// A value as `--get` printed it, typed.
#[derive(Debug, Clone, PartialEq)]
pub enum Typed {
    Bool(bool),
    Integer(i64),
    Number(f64),
    Text(String),
}

/// A number as sigrok prints it: `Some(Some(n))` for a finite number, `Some(None)` for
/// infinity or NaN (an overload), `None` for anything that isn't a number.
fn number(tok: &str) -> Option<Option<f64>> {
    let lower = tok.to_ascii_lowercase();
    let word = lower.trim_start_matches(['+', '-']);
    if matches!(word, "inf" | "infinity" | "nan") {
        return Some(None);
    }
    let looks = tok.starts_with(|c: char| c.is_ascii_digit() || matches!(c, '+' | '-' | '.'))
        && tok.chars().any(|c| c.is_ascii_digit())
        && tok
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | '.' | 'e' | 'E'));
    if !looks {
        return None;
    }
    let n: f64 = tok.parse().ok()?;
    Some(n.is_finite().then_some(n))
}

/// `true`/`false` → boolean; an integer or float literal → number; text in single or double
/// quotes → the text without them; anything else (tuples, typed values, units) → the trimmed
/// text.
pub fn typed(text: &str) -> Typed {
    let t = text.trim();
    match t {
        "true" => return Typed::Bool(true),
        "false" => return Typed::Bool(false),
        _ => {}
    }
    if let Some(Some(n)) = number(t) {
        return match t.parse::<i64>() {
            Ok(i) => Typed::Integer(i),
            Err(_) => Typed::Number(n),
        };
    }
    for q in ['\'', '"'] {
        if t.len() >= 2 && t.starts_with(q) && t.ends_with(q) {
            return Typed::Text(t[1..t.len() - 1].to_string());
        }
    }
    Typed::Text(t.to_string())
}

/// Some of what sigrok printed, for an error message.
fn excerpt(text: &str) -> String {
    let t = text.trim();
    if t.is_empty() {
        return "nothing".into();
    }
    let mut cut = t.len().min(500);
    while !t.is_char_boundary(cut) {
        cut -= 1;
    }
    let more = if cut < t.len() { "…" } else { "" };
    format!("{:?}{more}", &t[..cut])
}

/// `--get`'s stdout: exactly one line per key, in order (blank lines don't count).
pub fn parse_get(
    keys: &[String],
    stdout: &str,
) -> std::result::Result<Vec<(String, Typed)>, String> {
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() != keys.len() {
        return Err(format!(
            "asked for {} key(s) ({}) but sigrok printed {} line(s), one per key was expected: {}",
            keys.len(),
            keys.join(", "),
            lines.len(),
            excerpt(stdout)
        ));
    }
    Ok(keys
        .iter()
        .cloned()
        .zip(lines.into_iter().map(typed))
        .collect())
}

/// One analog reading from sigrok's default output.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub channel: String,
    /// In `unit`; `None` when sigrok printed no finite number (an overload).
    pub value: Option<f64>,
    /// The base unit when the unit table knows it (`mV` → `V`), else as printed.
    pub unit: Option<String>,
    pub flags: Vec<String>,
    /// Everything after `<channel>: `.
    pub text: String,
}

/// The flags sigrok prints after a reading's unit.
const FLAGS: &[&str] = &[
    "AC", "DC", "RMS", "DIODE", "HOLD", "MAX", "MIN", "AUTO", "REL", "AVG", "REF", "UNSTABLE",
    "4WIRE",
];

/// `<channel>: <number> <unit> [flags…]`, e.g. `A1: 3.0902 V DC` or `P1: 0.0 mV DC`. A line
/// whose number isn't one (`OL`) still is a reading when a known unit follows it. Anything else
/// (headers, logic channels like `D0:100`) is `None`.
pub fn parse_reading(line: &str) -> Option<Reading> {
    let (channel, rest) = line.trim_end().split_once(": ")?;
    check_channel(channel).ok()?;
    let mut tokens = rest.split_whitespace();
    let first = tokens.next()?;
    let mut unit = None;
    let mut flags = Vec::new();
    for t in tokens {
        if unit.is_none() && flags.is_empty() && !FLAGS.contains(&t) {
            unit = Some(t);
        } else {
            flags.push(t.to_string());
        }
    }
    let known = unit.and_then(crate::units::printed);
    let value = match number(first) {
        Some(v) => v,
        None if known.is_some() => None,
        None => return None,
    };
    let (value, unit) = match (known, unit) {
        (Some((symbol, exp)), _) => (
            value
                .map(|n| scaled(first, n, exp))
                .filter(|n| n.is_finite()),
            Some(symbol.to_string()),
        ),
        (None, u) => (value, u.map(str::to_string)),
    };
    Some(Reading {
        channel: channel.to_string(),
        value,
        unit,
        flags,
        text: rest.trim().to_string(),
    })
}

/// `n` (printed as `text`) times 10^`exp`, without the noise of a float multiplication when
/// the text allows it (`3.3` kΩ is 3300, not 3299.9999999999995).
fn scaled(text: &str, n: f64, exp: i32) -> f64 {
    if exp == 0 {
        return n;
    }
    if !text.contains(['e', 'E'])
        && let Ok(v) = format!("{text}e{exp}").parse::<f64>()
    {
        return v;
    }
    if exp < 0 {
        n / 10f64.powi(-exp)
    } else {
        n * 10f64.powi(exp)
    }
}

/// Run one short call (`get`, `set`, `measure`) to its end and return its stdout. Its own
/// `timeout`, or the backend's query timeout.
async fn run_short(
    configs: &indexmap::IndexMap<String, ProviderConfig>,
    base_dir: &Path,
    device: &str,
    spec: &SigrokSpec,
    args: &[OsString],
    timeout: Option<Duration>,
    what: &str,
) -> Result<String> {
    let backend = resolve(configs, base_dir, spec).await?;
    let cmd = backend.command(args)?;
    let timeout = timeout.unwrap_or(backend.timeout);
    match run(cmd, &backend, timeout, None).await {
        Ok(ran) if ran.ok => Ok(ran.stdout),
        Ok(ran) => Err(failed(&backend, device, spec, ran.error_text())),
        Err(Stop::Timeout) => Err(DeskError::Timeout {
            what: format!("sigrok-cli's {what} for `{device}`"),
            after: timeout,
        }),
        Err(Stop::Failed(e)) => Err(e),
    }
}

/// Read config keys (`ctx.sigrok.get`), in the order asked for.
pub async fn get(
    configs: &indexmap::IndexMap<String, ProviderConfig>,
    base_dir: &Path,
    device: &str,
    spec: &SigrokSpec,
    req: &Get,
) -> Result<Vec<(String, Typed)>> {
    let out = run_short(
        configs,
        base_dir,
        device,
        spec,
        &req.args(spec),
        req.timeout,
        "get",
    )
    .await?;
    parse_get(&req.keys, &out)
        .map_err(|m| DeskError::ScriptFailed(format!("sigrok-cli --get for `{device}`: {m}")))
}

/// Change config keys (`ctx.sigrok.set`). desk keeps nothing: a driver's read() reads back.
pub async fn set(
    configs: &indexmap::IndexMap<String, ProviderConfig>,
    base_dir: &Path,
    device: &str,
    spec: &SigrokSpec,
    req: &Set,
) -> Result<()> {
    run_short(
        configs,
        base_dir,
        device,
        spec,
        &req.args(spec),
        req.timeout,
        "set",
    )
    .await?;
    Ok(())
}

/// Take readings (`ctx.sigrok.measure`), in the order sigrok printed them. No readings at all is
/// `endpoint_unavailable`.
pub async fn measure(
    configs: &indexmap::IndexMap<String, ProviderConfig>,
    base_dir: &Path,
    device: &str,
    spec: &SigrokSpec,
    req: &Measure,
) -> Result<Vec<Reading>> {
    let what = "measurement";
    let out = run_short(
        configs,
        base_dir,
        device,
        spec,
        &req.args(spec),
        req.timeout,
        what,
    )
    .await?;
    let readings: Vec<Reading> = out.lines().filter_map(parse_reading).collect();
    if readings.is_empty() {
        return Err(DeskError::EndpointUnavailable {
            endpoint: device_arg(&spec.driver, spec.conn.as_deref()),
            message: format!(
                "the device sent no readings for {} (sigrok printed {})",
                req.channels.join(","),
                excerpt(&out)
            ),
        });
    }
    Ok(readings)
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
    fn get_set_and_measure_are_one_argument_per_element() {
        let get = Get {
            keys: vec!["voltage".into(), "enabled".into()],
            group: None,
            timeout: None,
        };
        assert_eq!(
            text(get.args(&spec(None))),
            ["-d", "demo", "--get", "voltage", "--get", "enabled"]
        );
        let get = Get {
            group: Some("CH1".into()),
            ..get
        };
        assert_eq!(
            text(get.args(&spec(Some("/dev/ttyUSB0")))),
            [
                "-d",
                "demo:conn=/dev/ttyUSB0",
                "-g",
                "CH1",
                "--get",
                "voltage",
                "--get",
                "enabled"
            ]
        );
        let set = Set {
            config: vec![
                ("enabled".into(), "true".into()),
                ("voltage_target".into(), "5".into()),
            ],
            group: Some("CH1".into()),
            timeout: None,
        };
        assert_eq!(
            text(set.args(&spec(None))),
            [
                "-d",
                "demo",
                "-g",
                "CH1",
                "--config",
                "enabled=true:voltage_target=5",
                "--set"
            ]
        );
        let m = Measure {
            channels: vec!["A0".into(), "A1".into()],
            length: Length::Samples(1),
            config: vec![],
            group: None,
            timeout: None,
        };
        assert_eq!(
            text(m.args(&spec(None))),
            ["-d", "demo", "--channels", "A0,A1", "--samples", "1"]
        );
        let m = Measure {
            length: Length::Time(500),
            config: vec![("amplitude".into(), "2".into())],
            group: Some("A0".into()),
            ..m
        };
        let args = text(m.args(&spec(Some("1.2"))));
        assert_eq!(
            args,
            [
                "-d",
                "demo:conn=1.2",
                "-g",
                "A0",
                "--config",
                "amplitude=2",
                "--channels",
                "A0,A1",
                "--time",
                "500"
            ]
        );
        assert!(!args.iter().any(|a| a == "-O" || a == "-o"));
        for g in ["CH1", "A0", "Logic", "ch_1.x-2"] {
            assert!(check_group(g).is_ok(), "{g}");
        }
        for g in ["", "-g", "CH 1", "CH1:x", "a=b", "a/b", "CH1\n"] {
            assert!(check_group(g).is_err(), "{g:?}");
        }
    }

    #[test]
    fn get_values_are_typed() {
        assert_eq!(typed("200000"), Typed::Integer(200000));
        assert_eq!(typed("-3"), Typed::Integer(-3));
        assert_eq!(typed("10.0"), Typed::Number(10.0));
        assert_eq!(typed(" 1.5e-3 \r"), Typed::Number(0.0015));
        assert_eq!(typed("true"), Typed::Bool(true));
        assert_eq!(typed("false"), Typed::Bool(false));
        assert_eq!(typed("'square'"), Typed::Text("square".into()));
        assert_eq!(typed("\"x y\""), Typed::Text("x y".into()));
        assert_eq!(typed("''"), Typed::Text(String::new()));
        assert_eq!(typed("(1, 2)"), Typed::Text("(1, 2)".into()));
        assert_eq!(typed("5 V"), Typed::Text("5 V".into()));
        assert_eq!(typed("inf"), Typed::Text("inf".into()));
        assert_eq!(typed("'"), Typed::Text("'".into()));
        assert_eq!(typed("1.2.3"), Typed::Text("1.2.3".into()));
        assert_eq!(typed("True"), Typed::Text("True".into()));

        let keys = vec!["a".to_string(), "b".to_string()];
        let got = parse_get(&keys, "20\n'x'\n").unwrap();
        assert_eq!(
            got,
            [
                ("a".to_string(), Typed::Integer(20)),
                ("b".to_string(), Typed::Text("x".into()))
            ]
        );
        assert!(
            parse_get(&keys, "20\n\n'x'\n\n").is_ok(),
            "blank lines don't count"
        );
        let e = parse_get(&keys, "20\n").unwrap_err();
        assert!(
            e.contains("2 key(s)") && e.contains("1 line(s)") && e.contains("20"),
            "{e}"
        );
        assert!(parse_get(&keys, "1\n2\n3\n").is_err());
        assert!(parse_get(&keys, "").unwrap_err().contains("nothing"));
    }

    #[test]
    fn readings_are_parsed_into_base_units() {
        let r = parse_reading("A1: 3.0902 V DC").unwrap();
        assert_eq!(r.channel, "A1");
        assert_eq!(r.value, Some(3.0902));
        assert_eq!(r.unit.as_deref(), Some("V"));
        assert_eq!(r.flags, ["DC"]);
        assert_eq!(r.text, "3.0902 V DC");
        let r = parse_reading("A0: -10.0000 V DC").unwrap();
        assert_eq!(r.value, Some(-10.0));
        let r = parse_reading("A1: 0.0 mV DC").unwrap();
        assert_eq!((r.value, r.unit.as_deref()), (Some(0.0), Some("V")));
        let r = parse_reading("P1: 2.4 mV DC").unwrap();
        assert_eq!(r.value, Some(0.0024));
        let r = parse_reading("P1: 3.3 kΩ").unwrap();
        assert_eq!((r.value, r.unit.as_deref()), (Some(3300.0), Some("Ω")));
        assert!(r.flags.is_empty());
        let r = parse_reading("P1: 4.7 k\u{2126} AUTO").unwrap();
        assert_eq!((r.value, r.unit.as_deref()), (Some(4700.0), Some("Ω")));
        let r = parse_reading("P1: 12.5 µA AC RMS AUTO HOLD").unwrap();
        assert_eq!((r.value, r.unit.as_deref()), (Some(12.5e-6), Some("A")));
        assert_eq!(r.flags, ["AC", "RMS", "AUTO", "HOLD"]);
        let r = parse_reading("P1: 25.3 uA DC").unwrap();
        assert_eq!(r.value, Some(25.3e-6));
        let r = parse_reading("P1: 1.2e3 mV DC").unwrap();
        assert_eq!(r.value, Some(1.2));
        let r = parse_reading("T1: 21.5 °C").unwrap();
        assert_eq!((r.value, r.unit.as_deref()), (Some(21.5), Some("°C")));
        // A unit the table doesn't know: kept as printed.
        let r = parse_reading("P1: 70.1 °F").unwrap();
        assert_eq!((r.value, r.unit.as_deref()), (Some(70.1), Some("°F")));
        let r = parse_reading("P1: -3.5 dBm").unwrap();
        assert_eq!((r.value, r.unit.as_deref()), (Some(-3.5), Some("dBm")));
        // No unit: flags right after the number.
        let r = parse_reading("P1: 42 DC").unwrap();
        assert_eq!((r.value, r.unit), (Some(42.0), None));
        assert_eq!(r.flags, ["DC"]);
        let r = parse_reading("P1: 42").unwrap();
        assert_eq!((r.value, r.unit), (Some(42.0), None));
        // Overloads: no value, the text tells.
        for line in ["P1: inf V DC", "P1: -inf mV", "P1: nan Ω", "P1: OL kΩ AUTO"] {
            let r = parse_reading(line).unwrap_or_else(|| panic!("{line}"));
            assert_eq!(r.value, None, "{line}");
            assert!(r.unit.is_some(), "{line}");
            assert_eq!(r.text, line["P1: ".len()..], "{line}");
        }
        // Not readings.
        for junk in [
            "libsigrok 0.5.2-git-9ffa3638",
            "Acquisition with 1/13 channels at 10 Hz",
            "D0:100",
            "D0:10101010 11110000",
            "",
            "A0: ",
            "A0: OL",
            "A0: OL junk",
            "bad channel: 3 V",
            "-x: 3 V",
            "  A0 3 V",
        ] {
            assert_eq!(parse_reading(junk), None, "{junk:?}");
        }
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
