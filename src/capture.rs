//! Captures: actions that produce a file (`produces = "file"`).
//!
//! A **source** (a driver, through a host function) fills a **sink** that the core owns. Here the
//! sink is a file: written in a private temporary folder inside its destination folder, bounded by
//! a size cap while it grows, and moved to its name when complete. The folder holds the data file
//! and anything a program writes next to it (sigrok-cli leaves `<file>.<random>.part` files), so
//! the size counts all of it and cleanup removes all of it. A failed, cancelled or timed-out capture leaves
//! nothing behind, and an existing file is never overwritten.
//!
//! A sink is filled in one of two ways:
//! * the core writes the bytes itself ([`Output::write`]);
//! * an external program writes to [`Output::temp_path`] while [`Output::guard`] watches the size.
//!
//! Scripts never see a path: they get `ctx.output`, an opaque handle they can only pass on.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use serde::Serialize;

use crate::config::Config;
use crate::error::{DeskError, Result};

/// `[captures] max_size` when the config doesn't say.
pub const DEFAULT_MAX_SIZE: u64 = 1 << 30;

/// The `kind` of a [`DeskError::LimitExceeded`] for the size cap (its hint names `--max-size`).
pub const SIZE_CAP: &str = "size cap";

/// How often [`Output::guard`] looks at the file's size.
const GUARD_POLL: Duration = Duration::from_millis(50);

/// `1GiB`, `500MB`, `64kB`, `1048576`: bytes. Decimal (`kB`, `MB`, `GB`) and binary (`KiB`, `MiB`,
/// `GiB`) units, any case, with or without `B`.
pub fn parse_size(text: &str) -> std::result::Result<u64, String> {
    let s = text.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let bad = || format!("`{text}` is not a size like 1GiB, 500MB or 1048576 (bytes)");
    let n: f64 = num.parse().map_err(|_| bad())?;
    let factor: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" => 1_000,
        "m" | "mb" => 1_000_000,
        "g" | "gb" => 1_000_000_000,
        "ki" | "kib" => 1 << 10,
        "mi" | "mib" => 1 << 20,
        "gi" | "gib" => 1 << 30,
        _ => return Err(bad()),
    };
    let bytes = (n * factor as f64).round();
    if !bytes.is_finite() || bytes < 1.0 || bytes > u64::MAX as f64 {
        return Err(format!("`{text}`: a size must be at least 1 byte"));
    }
    Ok(bytes as u64)
}

/// `1 GiB`, `1.5 MiB`, `300 B`.
pub fn format_size(n: u64) -> String {
    let units = [(1u64 << 30, "GiB"), (1 << 20, "MiB"), (1 << 10, "KiB")];
    for (f, u) in units {
        if n >= f {
            let v = n as f64 / f as f64;
            let t = format!("{v:.1}");
            return format!("{} {u}", t.trim_end_matches(".0"));
        }
    }
    format!("{n} B")
}

/// The capture folder: `$DESK_CAPTURE_DIR`, else `[captures] dir`, else `<state dir>/captures`.
/// Each device gets its own folder in it.
pub fn base_dir(config: &Config) -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("DESK_CAPTURE_DIR").filter(|d| !d.is_empty()) {
        return Ok(std::path::absolute(PathBuf::from(d))?);
    }
    if let Some(d) = &config.captures.dir {
        return Ok(d.clone());
    }
    let dirs = directories::ProjectDirs::from("", "", "desk-operator")
        .ok_or_else(|| DeskError::Internal("cannot determine a state directory".into()))?;
    let state = dirs.state_dir().unwrap_or_else(|| dirs.data_local_dir());
    Ok(state.join("captures"))
}

/// Check a `--output` file before anything runs: absolute, its folder exists, the file doesn't.
pub fn check_output(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(DeskError::Usage(format!(
            "--output {} must be an absolute path here",
            path.display()
        )));
    }
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    match parent {
        Some(p) if p.is_dir() => {}
        _ => {
            return Err(DeskError::Usage(format!(
                "--output {}: the folder {} doesn't exist",
                path.display(),
                parent.map_or_else(|| "?".into(), |p| p.display().to_string())
            )));
        }
    }
    if path.file_name().is_none() || path.is_dir() {
        return Err(DeskError::Usage(format!(
            "--output {}: name a file, not a folder",
            path.display()
        )));
    }
    if std::fs::symlink_metadata(path).is_ok() {
        return Err(DeskError::Usage(format!(
            "--output {} already exists (desk never overwrites a file): choose another name",
            path.display()
        )));
    }
    Ok(())
}

/// Cancels a running capture: Ctrl-C, or a client that closed its daemon connection.
#[derive(Debug, Clone)]
pub struct Cancel(Arc<tokio::sync::watch::Sender<bool>>);

impl Default for Cancel {
    fn default() -> Self {
        Cancel(Arc::new(tokio::sync::watch::channel(false).0))
    }
}

impl Cancel {
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }

    /// Resolves once cancelled.
    pub async fn cancelled(&self) {
        let mut rx = self.0.subscribe();
        // The sender lives in `self`, so this only returns when cancelled.
        let _ = rx.wait_for(|c| *c).await;
    }
}

/// Where a capture goes and its bounds, decided before anything runs.
#[derive(Debug, Clone)]
pub struct Plan {
    /// The device's capture folder (used without `output`).
    pub dir: PathBuf,
    /// `--output`: this file (absolute).
    pub output: Option<PathBuf>,
    pub max_size: u64,
    pub cancel: Cancel,
}

impl Plan {
    /// What a dry run reports: the file, or the folder it would go to.
    pub fn destination(&self) -> &Path {
        self.output.as_deref().unwrap_or(&self.dir)
    }
}

/// What a dry run of a capture reports.
#[derive(Debug, Clone, Serialize)]
pub struct Planned {
    pub into: PathBuf,
    pub max_size: u64,
}

/// What a driver reports about the file it produced.
#[derive(Debug, Clone, PartialEq)]
pub struct Produced {
    pub format: String,
    pub extension: String,
    /// Everything else the driver returned, untouched.
    pub meta: serde_json::Map<String, serde_json::Value>,
}

impl Produced {
    /// Check a driver's `format` and `extension` (default: the format).
    pub fn new(
        format: String,
        extension: Option<String>,
        meta: serde_json::Map<String, serde_json::Value>,
    ) -> std::result::Result<Produced, String> {
        if format.is_empty() || format.len() > 64 || format.chars().any(char::is_control) {
            return Err("`format` must be a short name like \"srzip\"".into());
        }
        let ext = extension.clone().unwrap_or_else(|| format.clone());
        let valid = !ext.is_empty()
            && ext.len() <= 16
            && ext
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
        if !valid {
            return Err(match extension {
                Some(e) => format!(
                    "`extension` \"{e}\" must be 1 to 16 lowercase letters or digits, like \"sr\""
                ),
                None => format!(
                    "`format` \"{format}\" can't be a file extension: also return `extension` \
                     (1 to 16 lowercase letters or digits, like \"sr\")"
                ),
            });
        }
        Ok(Produced {
            format,
            extension: ext,
            meta,
        })
    }
}

/// What a finished capture reports.
#[derive(Debug, Clone, Serialize)]
pub struct Captured {
    pub file: PathBuf,
    pub bytes: u64,
    pub format: String,
    pub meta: serde_json::Map<String, serde_json::Value>,
}

/// The file a sink becomes.
#[derive(Debug)]
enum Target {
    /// `--output`.
    Named(PathBuf),
    /// `<dir>/<stamp>.<ext>`, with `-2`, `-3`, … on a collision.
    Folder { dir: PathBuf, stamp: String },
}

/// The data file's name inside the sink's private folder.
const DATA_FILE: &str = "data";

#[derive(Debug)]
struct Inner {
    /// The private folder the sink created; removed when the sink ends.
    folder: PathBuf,
    /// The data file in it.
    tmp: PathBuf,
    target: Target,
    max: u64,
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    file: Option<std::fs::File>,
    written: u64,
    /// Fillers may write (only during the driver's call).
    open: bool,
    /// Finished or discarded: the temporary file is gone or renamed.
    done: bool,
}

/// What fillers get: write, or hand the temporary path to a program and guard the size. Clones
/// share the sink; only the [`Sink`] that owns it finishes or discards it.
#[derive(Debug, Clone)]
pub struct Output(Arc<Inner>);

/// The owner of a capture's file. Dropped without [`Sink::finish`], it removes the partial file.
#[derive(Debug)]
pub struct Sink(Output);

impl std::ops::Deref for Sink {
    type Target = Output;
    fn deref(&self) -> &Output {
        &self.0
    }
}

fn utc_stamp() -> String {
    humantime::format_rfc3339_seconds(SystemTime::now())
        .to_string()
        .replace(':', "-")
}

impl Sink {
    /// Create the private temporary folder and its data file in the destination folder (the
    /// device folder is created; the folder of `--output` must exist).
    pub fn create(plan: &Plan) -> Result<Sink> {
        let io = |what: &str, p: &Path, e: std::io::Error| {
            DeskError::Internal(format!("capture: cannot {what} {}: {e}", p.display()))
        };
        let (dir, target, base) = match &plan.output {
            Some(path) => {
                check_output(path)?;
                let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                (dir, Target::Named(path.clone()), name)
            }
            None => {
                std::fs::create_dir_all(&plan.dir)
                    .map_err(|e| io("create the folder", &plan.dir, e))?;
                let stamp = utc_stamp();
                let target = Target::Folder {
                    dir: plan.dir.clone(),
                    stamp: stamp.clone(),
                };
                (plan.dir.clone(), target, stamp)
            }
        };
        let pid = std::process::id();
        let mut n = 0;
        let folder = loop {
            let name = match n {
                0 => format!(".{base}.{pid}.partial"),
                _ => format!(".{base}.{pid}-{n}.partial"),
            };
            let folder = dir.join(name);
            match std::fs::create_dir(&folder) {
                Ok(()) => break folder,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 100 => n += 1,
                Err(e) => return Err(io("create", &folder, e)),
            }
        };
        let tmp = folder.join(DATA_FILE);
        let file = match std::fs::File::options()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(f) => f,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&folder);
                return Err(io("create", &tmp, e));
            }
        };
        Ok(Sink(Output(Arc::new(Inner {
            folder,
            tmp,
            target,
            max: plan.max_size,
            state: Mutex::new(State {
                file: Some(file),
                written: 0,
                open: true,
                done: false,
            }),
        }))))
    }

    /// Move the complete data file to its name (never over an existing file) and remove the
    /// private folder. The caller checked that it isn't empty.
    pub fn finish(&self, extension: &str) -> Result<(PathBuf, u64)> {
        let inner = &self.0.0;
        self.close();
        inner.lock().file = None;
        self.check_size()?;
        // Only the data file is the capture; anything else in the folder goes with it.
        let bytes = std::fs::metadata(&inner.tmp).map_or(0, |m| m.len());
        let io = |p: &Path, e: std::io::Error| {
            DeskError::Internal(format!("capture: cannot write {}: {e}", p.display()))
        };
        let mut n = 1;
        let path = loop {
            let path = match &inner.target {
                Target::Named(p) => p.clone(),
                Target::Folder { dir, stamp } if n == 1 => dir.join(format!("{stamp}.{extension}")),
                Target::Folder { dir, stamp } => dir.join(format!("{stamp}-{n}.{extension}")),
            };
            match place(&inner.tmp, &path) {
                Ok(()) => break path,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if let Target::Named(p) = &inner.target {
                        return Err(DeskError::Usage(format!(
                            "--output {} appeared while capturing (desk never overwrites a \
                             file); the capture was discarded",
                            p.display()
                        )));
                    }
                    n += 1;
                    if n > 1000 {
                        return Err(io(&path, e));
                    }
                }
                Err(e) => return Err(io(&path, e)),
            }
        };
        let mut s = inner.lock();
        s.done = true;
        remove_folder(&inner.folder);
        Ok((path, bytes))
    }
}

/// Move `tmp` to `path` unless `path` exists (`AlreadyExists`).
fn place(tmp: &Path, path: &Path) -> std::io::Result<()> {
    // A hard link fails if the name exists, which a rename (it replaces) wouldn't.
    match std::fs::hard_link(tmp, path) {
        Ok(()) => {
            let _ = std::fs::remove_file(tmp);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(e),
        // A file system without hard links.
        Err(_) => {
            if std::fs::symlink_metadata(path).is_ok() {
                return Err(std::io::ErrorKind::AlreadyExists.into());
            }
            std::fs::rename(tmp, path)
        }
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        let inner = &self.0.0;
        let mut s = inner.lock();
        s.open = false;
        s.file = None;
        if !s.done {
            s.done = true;
            remove_folder(&inner.folder);
            // A device folder that only existed for this capture goes too (never recursive,
            // never the folder of an `--output` file).
            if let Target::Folder { dir, .. } = &inner.target {
                let _ = std::fs::remove_dir(dir);
            }
        }
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Output {
    fn too_big(&self, size: u64) -> DeskError {
        DeskError::LimitExceeded {
            what: "capture size".into(),
            requested: format_size(size),
            limit: format_size(self.0.max),
            kind: SIZE_CAP.into(),
            below: false,
        }
    }

    /// Append bytes (the core writes them itself). Over the cap: `limit_exceeded`, nothing more
    /// is written.
    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        let mut s = self.0.lock();
        if !s.open {
            return Err(DeskError::ScriptFailed(
                "ctx.output can only be filled during the action call".into(),
            ));
        }
        let size = s.written + bytes.len() as u64;
        if size > self.0.max {
            s.open = false;
            return Err(self.too_big(size));
        }
        let Some(file) = s.file.as_mut() else {
            return Err(DeskError::Internal("capture file is closed".into()));
        };
        file.write_all(bytes).map_err(|e| {
            DeskError::Internal(format!(
                "capture: cannot write {}: {e}",
                self.0.tmp.display()
            ))
        })?;
        s.written = size;
        Ok(())
    }

    /// The temporary data file, for a program that writes the capture itself. It may put other
    /// files next to it: they count towards the size and are removed with the sink. Never give it
    /// to a script.
    pub(crate) fn temp_path(&self) -> &Path {
        &self.0.tmp
    }

    /// The size on disk now: every file in the private folder.
    pub fn size(&self) -> u64 {
        folder_size(&self.0.folder)
    }

    /// The size on disk, or `limit_exceeded` if it's over the cap.
    pub fn check_size(&self) -> Result<u64> {
        let size = self.size();
        if size > self.0.max {
            self.close();
            return Err(self.too_big(size));
        }
        Ok(size)
    }

    /// Resolves with `limit_exceeded` once the file on disk grows over the cap. Run it next to a
    /// program that writes to [`Output::temp_path`], and stop the program when it resolves.
    pub(crate) async fn guard(&self) -> DeskError {
        loop {
            tokio::time::sleep(GUARD_POLL).await;
            if let Err(e) = self.check_size() {
                return e;
            }
        }
    }

    /// No more writes (the driver's call ended; a handle a script kept is useless).
    pub fn close(&self) {
        self.0.lock().open = false;
    }

    /// Whether fillers may still write.
    pub fn is_open(&self) -> bool {
        self.0.lock().open
    }
}

/// Remove the sink's private folder. A program that was just killed may still have been creating
/// a file in it, so try a few times.
fn remove_folder(folder: &Path) {
    for _ in 0..5 {
        if std::fs::remove_dir_all(folder).is_ok() || !folder.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The sum of the files in `dir` and its sub-folders (a program may make them).
fn folder_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => folder_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map_or(0, |m| m.len()),
            _ => 0,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(dir: &Path, max: u64) -> Plan {
        Plan {
            dir: dir.to_path_buf(),
            output: None,
            max_size: max,
            cancel: Cancel::default(),
        }
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("1GiB").unwrap(), 1 << 30);
        assert_eq!(parse_size("1 gib").unwrap(), 1 << 30);
        assert_eq!(parse_size("500MB").unwrap(), 500_000_000);
        assert_eq!(parse_size("64kB").unwrap(), 64_000);
        assert_eq!(parse_size("1.5KiB").unwrap(), 1536);
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert!(parse_size("0").is_err());
        assert!(parse_size("1TB").is_err());
        assert!(parse_size("lots").is_err());
        assert_eq!(format_size(1 << 30), "1 GiB");
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(format_size(300), "300 B");
    }

    #[test]
    fn extensions() {
        let m = serde_json::Map::new();
        assert_eq!(
            Produced::new("srzip".into(), None, m.clone())
                .unwrap()
                .extension,
            "srzip"
        );
        assert_eq!(
            Produced::new("srzip".into(), Some("sr".into()), m.clone())
                .unwrap()
                .extension,
            "sr"
        );
        assert!(Produced::new("csv:header".into(), None, m.clone()).is_err());
        assert!(Produced::new("x".into(), Some("../x".into()), m.clone()).is_err());
        assert!(Produced::new("x".into(), Some("SR".into()), m.clone()).is_err());
        assert!(Produced::new(String::new(), None, m).is_err());
    }

    #[test]
    fn a_sink_becomes_a_file_or_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Sink::create(&plan(dir.path(), 100)).unwrap();
        sink.write(b"hello").unwrap();
        assert_eq!(files(dir.path()).len(), 1);
        assert!(files(dir.path())[0].ends_with(".partial"));
        assert!(dir.path().join(&files(dir.path())[0]).is_dir());
        // A program's stray file next to the data counts and goes with the sink.
        let stray = sink.temp_path().with_extension("xyz.part");
        std::fs::write(&stray, b"12").unwrap();
        assert_eq!(sink.size(), 7);
        let (path, bytes) = sink.finish("bin").unwrap();
        assert_eq!(bytes, 5);
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        assert!(path.to_string_lossy().ends_with("Z.bin"));
        drop(sink);
        assert_eq!(files(dir.path()).len(), 1, "a finished file stays");

        // Dropped unfinished: nothing left.
        let sink = Sink::create(&plan(dir.path(), 100)).unwrap();
        sink.write(b"partial").unwrap();
        drop(sink);
        assert_eq!(files(dir.path()).len(), 1);

        // Over the cap.
        let sink = Sink::create(&plan(dir.path(), 4)).unwrap();
        let e = sink.write(b"hello").unwrap_err();
        assert_eq!(e.code(), "limit_exceeded");
        assert!(e.to_string().contains("size cap of 4 B"), "{e}");
        assert!(e.hint().unwrap().contains("--max-size"));
        assert!(sink.write(b"x").is_err(), "closed after the cap");
    }

    #[test]
    fn same_second_gets_a_suffix_and_nothing_is_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let a = Sink::create(&plan(dir.path(), 100)).unwrap();
        let b = Sink::create(&plan(dir.path(), 100)).unwrap();
        a.write(b"a").unwrap();
        b.write(b"b").unwrap();
        let (pa, _) = a.finish("bin").unwrap();
        let (pb, _) = b.finish("bin").unwrap();
        assert_ne!(pa, pb);
        assert_eq!(std::fs::read(&pa).unwrap(), b"a");
        assert_eq!(std::fs::read(&pb).unwrap(), b"b");

        let named = dir.path().join("x.bin");
        let p = Plan {
            output: Some(named.clone()),
            ..plan(dir.path(), 100)
        };
        let s = Sink::create(&p).unwrap();
        s.write(b"x").unwrap();
        std::fs::write(&named, b"someone else").unwrap();
        assert_eq!(s.finish("bin").unwrap_err().code(), "usage");
        assert_eq!(std::fs::read(&named).unwrap(), b"someone else");
        assert!(Sink::create(&p).is_err(), "an existing --output is refused");
    }

    #[test]
    fn an_unfinished_sink_removes_its_device_folder_only_if_empty() {
        let base = tempfile::tempdir().unwrap();
        let dev = base.path().join("la");
        let sink = Sink::create(&plan(&dev, 100)).unwrap();
        sink.write(b"x").unwrap();
        drop(sink);
        assert!(!dev.exists(), "an empty device folder goes");
        assert!(base.path().is_dir(), "the capture base folder stays");

        let sink = Sink::create(&plan(&dev, 100)).unwrap();
        sink.write(b"kept").unwrap();
        let (kept, _) = sink.finish("bin").unwrap();
        drop(sink);
        let sink = Sink::create(&plan(&dev, 100)).unwrap();
        sink.write(b"x").unwrap();
        drop(sink);
        assert!(kept.is_file(), "a folder with an earlier capture stays");
        assert_eq!(files(&dev).len(), 1);

        // The folder of an `--output` file is never removed.
        let out = base.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let p = Plan {
            output: Some(out.join("x.bin")),
            ..plan(&dev, 100)
        };
        let sink = Sink::create(&p).unwrap();
        sink.write(b"x").unwrap();
        drop(sink);
        assert!(out.is_dir());
        assert!(files(&out).is_empty());
    }

    #[tokio::test]
    async fn the_guard_sees_a_file_grow_from_outside() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Sink::create(&plan(dir.path(), 10)).unwrap();
        let path = sink.temp_path().to_path_buf();
        // Like sigrok-cli: the growing file is a sibling of the data file.
        let part = path.with_file_name("data.a1b2.part");
        let grow = async {
            let mut f = std::fs::File::create(&part).unwrap();
            for _ in 0..20 {
                f.write_all(b"xxxx").unwrap();
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        let e = tokio::select! {
            e = sink.guard() => e,
            _ = grow => panic!("the guard didn't stop it"),
        };
        assert_eq!(e.code(), "limit_exceeded");
        drop(sink);
        assert!(!path.exists());
        assert!(!part.exists());
        assert!(
            !dir.path().exists(),
            "the private folder and the empty device folder are gone"
        );
    }

    #[tokio::test]
    async fn cancel() {
        let c = Cancel::default();
        let c2 = c.clone();
        tokio::spawn(async move { c2.cancel() });
        tokio::time::timeout(Duration::from_secs(2), c.cancelled())
            .await
            .unwrap();
    }
}
