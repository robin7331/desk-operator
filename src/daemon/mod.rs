//! The daemon (`desk serve`): a background process that owns device sessions, so programs can
//! watch a device while `desk` and others keep controlling it.
//!
//! It starts on demand (the first `desk source watch`) and exits when nothing has used it for
//! `--idle`. While it runs, `desk power …` and `desk source …` go through it; otherwise they run
//! in-process as before. One daemon per config file and desk build.
//!
//! Protocol (JSON lines over the local socket, see `docs/streaming.md`): a request is
//! `{"args": [...]}` with the same arguments as the CLI; the reply is the `--json` envelope plus
//! `human` and `exit`. A `source watch` request turns the connection into a stream: one envelope
//! per reading, and an `ok: false` envelope if the session ends with an error.

mod ipc;
pub mod session;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use clap::Parser;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use crate::bench::Bench;
use crate::cli::{self, Cli, Command, SourceCmd};
use crate::error::{DeskError, ErrorJson, Result};
use crate::output::Report;
use session::{Event, Sessions};

/// How long a client waits for the daemon to come up.
const START_TIMEOUT: Duration = Duration::from_secs(5);
/// A ceiling for one forwarded command (the daemon enforces the real timeouts).
const REPLY_TIMEOUT: Duration = Duration::from_secs(300);

/// Where one daemon lives: its socket, its lock file and its log.
struct Endpoint {
    address: String,
    dir: PathBuf,
    lock: PathBuf,
    log: PathBuf,
    /// The config file the daemon serves (absolute).
    config: PathBuf,
}

impl Endpoint {
    fn resolve(config: Option<&Path>) -> Result<Endpoint> {
        let config = match config {
            Some(p) => p.to_path_buf(),
            None => crate::config::default_path().ok_or_else(|| {
                DeskError::ConfigInvalid(
                    "cannot determine a config directory; pass --config".into(),
                )
            })?,
        };
        let config = std::fs::canonicalize(&config)
            .or_else(|_| std::path::absolute(&config))
            .unwrap_or(config);
        let dir = runtime_dir()?;
        let key = key(&config);
        #[cfg(unix)]
        let address = {
            let address = dir.join(format!("desk-{key}.sock")).display().to_string();
            // Socket paths are limited to about 100 bytes: a long runtime folder gets a short
            // name in the temp folder (still mode 0600, so only this user can connect).
            if address.len() > 100 {
                let short = fnv(&format!("{}\0{key}", dir.display()));
                std::env::temp_dir()
                    .join(format!("desk-{short}.sock"))
                    .display()
                    .to_string()
            } else {
                address
            }
        };
        #[cfg(windows)]
        let address = format!(r"\\.\pipe\desk-operator-{key}");
        Ok(Endpoint {
            address,
            lock: dir.join(format!("desk-{key}.lock")),
            log: dir.join(format!("desk-{key}.log")),
            dir,
            config,
        })
    }

    fn create_dir(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir).map_err(|e| {
            DeskError::DaemonUnavailable(format!("cannot create {}: {e}", self.dir.display()))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

/// `$DESK_RUNTIME_DIR`, else the platform's per-user runtime or cache folder.
fn runtime_dir() -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("DESK_RUNTIME_DIR") {
        return Ok(PathBuf::from(d));
    }
    if let Some(d) =
        directories::BaseDirs::new().and_then(|b| b.runtime_dir().map(Path::to_path_buf))
    {
        return Ok(d.join("desk-operator"));
    }
    directories::ProjectDirs::from("", "", "desk-operator")
        .map(|d| d.cache_dir().to_path_buf())
        .ok_or_else(|| DeskError::DaemonUnavailable("no runtime or cache folder found".into()))
}

/// One daemon per config file and desk build: a rebuilt or upgraded desk starts its own.
fn key(config: &Path) -> String {
    let build = std::env::current_exe()
        .and_then(std::fs::metadata)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_default();
    fnv(&format!(
        "{}\0{build}\0{user}\0{}",
        env!("CARGO_PKG_VERSION"),
        config.display()
    ))
}

/// FNV-1a as 16 hex digits: stable across Rust versions, unlike DefaultHasher.
fn fnv(text: &str) -> String {
    let hash = text.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{hash:016x}")
}

// ---------------------------------------------------------------------------------------------
// Server

struct State {
    config: PathBuf,
    sessions: Arc<Sessions>,
    connections: AtomicUsize,
}

impl State {
    fn bench(&self) -> Result<Bench> {
        let mut bench = Bench::load(Some(&self.config))?;
        bench.sessions = Some(self.sessions.clone());
        Ok(bench)
    }
}

/// `desk serve`: run until idle for `idle` (`None`: until stopped).
pub async fn serve(config: Option<&Path>, idle: Option<Duration>) -> Result<Report> {
    let ep = Endpoint::resolve(config)?;
    ep.create_dir()?;
    let lock = std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&ep.lock)?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Ok(Report::new(
                &json!({ "running": true, "address": ep.address }),
                format!(
                    "a desk daemon already runs for this config ({})\n",
                    ep.address
                ),
            ));
        }
        Err(std::fs::TryLockError::Error(e)) => {
            return Err(DeskError::DaemonUnavailable(format!(
                "cannot lock {}: {e}",
                ep.lock.display()
            )));
        }
    }
    let mut listener = ipc::bind(&ep.address).map_err(|e| {
        DeskError::DaemonUnavailable(format!("cannot listen on {}: {e}", ep.address))
    })?;
    eprintln!(
        "desk daemon {} listening on {} (config {})",
        env!("CARGO_PKG_VERSION"),
        ep.address,
        ep.config.display()
    );

    let state = Arc::new(State {
        config: ep.config.clone(),
        sessions: Arc::new(Sessions::default()),
        connections: AtomicUsize::new(0),
    });
    let mut last_active = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let why = loop {
        tokio::select! {
            conn = listener.accept() => match conn {
                Ok(conn) => {
                    state.connections.fetch_add(1, Ordering::SeqCst);
                    tokio::spawn(handle(state.clone(), conn));
                }
                Err(e) => eprintln!("accept failed: {e}"),
            },
            _ = tick.tick() => {
                let busy = state.connections.load(Ordering::SeqCst) > 0 || state.sessions.count() > 0;
                if busy {
                    last_active = Instant::now();
                } else if idle.is_some_and(|idle| last_active.elapsed() >= idle) {
                    break "idle";
                }
            }
            _ = &mut shutdown => break "stopped",
        }
    };
    state
        .sessions
        .stop_all(DeskError::DaemonUnavailable(
            "the desk daemon was stopped".into(),
        ))
        .await;
    ipc::cleanup(&ep.address);
    eprintln!("desk daemon exiting ({why})");
    drop(lock);
    Ok(Report::new(
        &json!({ "stopped": why }),
        format!("desk daemon exited ({why})\n"),
    ))
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[derive(Deserialize)]
struct Request {
    args: Vec<String>,
}

enum Parsed {
    Watch {
        device: String,
        interval: Duration,
        count: Option<u64>,
    },
    Command(Command),
}

fn parse(line: &str) -> Result<Parsed> {
    let req: Request = serde_json::from_str(line).map_err(|e| {
        DeskError::Usage(format!(
            "a request is one JSON line like {{\"args\": [\"source\", \"read\", \"psu\"]}}: {e}"
        ))
    })?;
    let cli =
        Cli::try_parse_from(std::iter::once("desk".to_string()).chain(req.args)).map_err(|e| {
            let msg = e.render().to_string();
            DeskError::Usage(msg.trim().trim_start_matches("error: ").to_string())
        })?;
    match cli.command {
        Command::Source {
            action:
                SourceCmd::Watch {
                    device,
                    interval,
                    count,
                },
        } => Ok(Parsed::Watch {
            device,
            interval: cli::parse_interval(&interval)?,
            count,
        }),
        cmd if cli::routed(&cmd) => Ok(Parsed::Command(cmd)),
        _ => Err(DeskError::Usage(
            "only `desk power …` and `desk source …` commands run through the daemon".into(),
        )),
    }
}

/// Decrements the connection count however the handler ends.
struct Connection(Arc<State>);

impl Drop for Connection {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn handle(state: Arc<State>, conn: impl ipc::Conn) {
    let _counted = Connection(state.clone());
    let (read, mut write) = tokio::io::split(conn);
    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        match parse(&line) {
            Ok(Parsed::Watch {
                device,
                interval,
                count,
            }) => {
                stream(&state, &mut lines, &mut write, &device, interval, count).await;
                return;
            }
            Ok(Parsed::Command(cmd)) => {
                let result = match state.bench() {
                    Ok(bench) => cli::dispatch(&bench, cmd).await,
                    Err(e) => Err(e),
                };
                if send(&mut write, &reply(result)).await.is_err() {
                    return;
                }
            }
            Err(e) => {
                if send(&mut write, &reply(Err(e))).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// The `--json` envelope plus what the CLI client needs to print it like a local run.
fn reply(result: Result<Report>) -> Value {
    match result {
        Ok(r) => {
            let mut v = json!({ "ok": r.failure.is_none(), "data": r.data, "human": r.human });
            match &r.failure {
                Some((err, exit)) => {
                    v["error"] = json!(err);
                    v["exit"] = json!(exit);
                }
                None => v["exit"] = json!(0),
            }
            v
        }
        Err(e) => error_line(&e),
    }
}

fn error_line(e: &DeskError) -> Value {
    json!({ "ok": false, "error": e.to_json(), "exit": e.exit_code() })
}

async fn send(w: &mut (impl AsyncWrite + Unpin), v: &Value) -> std::io::Result<()> {
    let mut line = v.to_string();
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    w.flush().await
}

async fn stream<R: tokio::io::AsyncBufRead + Unpin>(
    state: &State,
    lines: &mut tokio::io::Lines<R>,
    w: &mut (impl AsyncWrite + Unpin),
    query: &str,
    interval: Duration,
    count: Option<u64>,
) {
    let started = async {
        let bench = state.bench()?;
        let device = bench.config.resolve_device(query)?;
        let watcher = bench.watch(device, interval).await?;
        Ok::<_, DeskError>((device.id.clone(), watcher))
    }
    .await;
    let (device, mut watcher) = match started {
        Ok(v) => v,
        Err(e) => {
            let _ = send(w, &error_line(&e)).await;
            return;
        }
    };
    let start = Instant::now();
    let mut last_sent: Option<Instant> = None;
    let mut dropped = 0u64;
    let mut failed = 0u64;
    let mut sent = 0u64;
    loop {
        tokio::select! {
            ev = watcher.events.recv() => match ev {
                Ok(Event::Reading { reading, at, time, log }) => {
                    // At most one reading per interval (the session reads for its fastest watcher).
                    if last_sent.is_some_and(|t| at.saturating_duration_since(t) < interval * 9 / 10) {
                        continue;
                    }
                    last_sent = Some(at);
                    let mut data = json!({
                        "device": device,
                        "time": humantime::format_rfc3339_millis(time).to_string(),
                        "elapsed": round_ms(at.saturating_duration_since(start)),
                        "reading": reading,
                        "dropped": dropped,
                        "failed": failed,
                    });
                    if !log.is_empty() {
                        data["log"] = json!(log);
                    }
                    if send(w, &json!({ "ok": true, "data": data })).await.is_err() {
                        return;
                    }
                    sent += 1;
                    if count.is_some_and(|c| sent >= c) {
                        return;
                    }
                }
                Ok(Event::Failed) => failed += 1,
                Ok(Event::Ended(e)) => {
                    let _ = send(w, &error_line(&e)).await;
                    return;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => dropped += n,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    let e = DeskError::DaemonUnavailable("the session ended".into());
                    let _ = send(w, &error_line(&e)).await;
                    return;
                }
            },
            // Input during a watch is ignored; EOF means the watcher left.
            line = lines.next_line() => if !matches!(line, Ok(Some(_))) {
                return;
            },
        }
    }
}

fn round_ms(d: Duration) -> f64 {
    (d.as_secs_f64() * 1000.0).round() / 1000.0
}

// ---------------------------------------------------------------------------------------------
// Client

/// The arguments this process was started with, to send to the daemon unchanged.
pub fn own_args() -> Vec<String> {
    std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

#[derive(Deserialize)]
struct Reply {
    ok: bool,
    #[serde(default)]
    data: Value,
    error: Option<ErrorJson>,
    #[serde(default)]
    human: String,
    #[serde(default)]
    exit: i32,
}

impl Reply {
    fn into_report(self) -> Report {
        let failure = match (self.ok, self.error) {
            (false, Some(e)) => Some((e, self.exit.max(1))),
            (false, None) => Some((
                DeskError::DaemonUnavailable(
                    "the daemon reported a failure without details".into(),
                )
                .to_json(),
                1,
            )),
            (true, _) => None,
        };
        Report {
            data: self.data,
            human: self.human,
            failure,
            streamed: false,
        }
    }
}

fn lost(ep: &Endpoint, what: &str) -> DeskError {
    DeskError::DaemonUnavailable(format!("{what} (daemon log: {})", ep.log.display()))
}

/// Run a command through the daemon if one runs. `None`: no daemon, run it in-process.
pub async fn forward(config: Option<&Path>, args: &[String]) -> Result<Option<Report>> {
    let Ok(ep) = Endpoint::resolve(config) else {
        return Ok(None);
    };
    let conn = match ipc::connect(&ep.address).await {
        Ok(c) => c,
        Err(e) if ipc::not_running(&e) => return Ok(None),
        Err(e) => return Err(lost(&ep, &format!("cannot reach the desk daemon: {e}"))),
    };
    let (read, mut write) = tokio::io::split(conn);
    send(&mut write, &json!({ "args": args }))
        .await
        .map_err(|e| lost(&ep, &format!("cannot talk to the desk daemon: {e}")))?;
    let mut lines = BufReader::new(read).lines();
    let line = tokio::time::timeout(REPLY_TIMEOUT, lines.next_line())
        .await
        .map_err(|_| lost(&ep, "the desk daemon didn't answer"))?
        .map_err(|e| lost(&ep, &format!("reading the daemon's reply failed: {e}")))?
        .ok_or_else(|| lost(&ep, "the desk daemon closed the connection without a reply"))?;
    let reply: Reply = serde_json::from_str(&line)
        .map_err(|e| lost(&ep, &format!("unreadable reply from the desk daemon: {e}")))?;
    Ok(Some(reply.into_report()))
}

/// `desk source watch`: stream readings from the daemon (starting it if needed) to stdout.
pub async fn watch(
    config: Option<&Path>,
    args: &[String],
    json_mode: bool,
    count: Option<u64>,
) -> Result<Report> {
    let ep = Endpoint::resolve(config)?;
    let conn = connect_or_start(&ep).await?;
    let (read, mut write) = tokio::io::split(conn);
    send(&mut write, &json!({ "args": args }))
        .await
        .map_err(|e| lost(&ep, &format!("cannot talk to the desk daemon: {e}")))?;
    let mut lines = BufReader::new(read).lines();
    let mut printer = Printer::default();
    let mut received = 0u64;
    loop {
        let line = lines.next_line().await.map_err(|e| {
            lost(
                &ep,
                &format!("the connection to the desk daemon failed: {e}"),
            )
        })?;
        let Some(line) = line else {
            if count.is_some_and(|c| received >= c) {
                return Ok(Report::streamed(None));
            }
            return Err(lost(&ep, "the desk daemon stopped while watching"));
        };
        let reply: Reply = serde_json::from_str(&line)
            .map_err(|e| lost(&ep, &format!("unreadable line from the desk daemon: {e}")))?;
        if !reply.ok {
            return Ok(Report::streamed(reply.into_report().failure));
        }
        received += 1;
        if json_mode {
            println!("{line}");
        } else {
            print!("{}", printer.row(&reply.data));
        }
    }
}

async fn connect_or_start(ep: &Endpoint) -> Result<impl ipc::Conn> {
    match ipc::connect(&ep.address).await {
        Ok(c) => return Ok(c),
        Err(e) if ipc::not_running(&e) => {}
        Err(e) => return Err(lost(ep, &format!("cannot reach the desk daemon: {e}"))),
    }
    let mut child = start(ep)?;
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        match ipc::connect(&ep.address).await {
            Ok(c) => return Ok(c),
            Err(e) if ipc::not_running(&e) => {}
            Err(e) => return Err(lost(ep, &format!("cannot reach the desk daemon: {e}"))),
        }
        // Exit 0 means another daemon won the race: keep connecting.
        if let Ok(Some(status)) = child.try_wait()
            && !status.success()
        {
            let last = std::fs::read_to_string(&ep.log)
                .ok()
                .and_then(|l| l.lines().last().map(str::to_string))
                .unwrap_or_default();
            return Err(lost(ep, &format!("the desk daemon didn't start: {last}")));
        }
        if Instant::now() > deadline {
            return Err(lost(ep, "the desk daemon didn't start in time"));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Start `desk serve` for this config, detached from the terminal (Ctrl-C on the watcher
/// doesn't stop it).
fn start(ep: &Endpoint) -> Result<std::process::Child> {
    ep.create_dir()?;
    let fail = |e: std::io::Error| lost(ep, &format!("cannot start the desk daemon: {e}"));
    let exe = std::env::current_exe().map_err(fail)?;
    let log = std::fs::File::create(&ep.log).map_err(fail)?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--config")
        .arg(&ep.config)
        .arg("serve")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn().map_err(fail)
}

/// Human output of `watch`: one row per reading.
#[derive(Default)]
struct Printer {
    header: bool,
    dropped: u64,
    failed: u64,
}

impl Printer {
    fn row(&mut self, data: &Value) -> String {
        let mut out = String::new();
        if !self.header {
            self.header = true;
            out.push_str("ELAPSED     OUTPUT  VOLTAGE    CURRENT    POWER      MODE\n");
        }
        let r = &data["reading"];
        let num = |k: &str, unit: &str, digits: usize| {
            r[k].as_f64()
                .map_or_else(|| "?".to_string(), |v| format!("{v:.digits$} {unit}"))
        };
        let output = match r["output"].as_bool() {
            Some(true) => "on",
            Some(false) => "off",
            None => "?",
        };
        let mode = r["mode"]
            .as_str()
            .map(str::to_uppercase)
            .unwrap_or_default();
        let mut line = format!(
            "{:<10}  {:<6}  {:<9}  {:<9}  {:<9}  {}",
            format!("{:.3} s", data["elapsed"].as_f64().unwrap_or_default()),
            output,
            num("voltage", "V", 2),
            num("current", "A", 3),
            num("power", "W", 2),
            mode
        );
        if let Some(p) = r["protection"].as_str().filter(|p| *p != "none") {
            line.push_str(&format!("  {} TRIPPED", p.to_uppercase()));
        }
        out.push_str(line.trim_end());
        out.push('\n');
        let dropped = data["dropped"].as_u64().unwrap_or_default();
        if dropped > self.dropped {
            out.push_str(&format!(
                "({} readings dropped: this terminal reads too slowly)\n",
                dropped - self.dropped
            ));
            self.dropped = dropped;
        }
        let failed = data["failed"].as_u64().unwrap_or_default();
        if failed > self.failed {
            out.push_str(&format!(
                "({} reads failed and were retried)\n",
                failed - self.failed
            ));
            self.failed = failed;
        }
        out
    }
}
