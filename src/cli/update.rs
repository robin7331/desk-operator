//! `desk update`: replace this desk with the latest release, the same steps as `install.sh` and
//! `install.ps1`: download, verify against `SHA256SUMS`, check that the new binary runs and reports
//! the released version, then swap it in. Skills that `desk skill install --global` put in place
//! are updated by the new binary.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::error::{DeskError, Result};
use crate::output::Report;

/// Where the release files are: the latest GitHub release. `DESK_RELEASE_URL` overrides it (tests,
/// mirrors), the same as for the installers.
const RELEASES: &str = "https://github.com/robin7331/desk-operator/releases/latest/download";

/// Set by the release workflow. A desk built from source is updated the way it was built.
const RELEASE_BUILD: Option<&str> = option_env!("DESK_RELEASE_BUILD");

/// The target this desk was built for; release assets are named after it.
const TARGET: &str = env!("DESK_TARGET");

const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// How long running the new binary (`--version`, `skill install`) or unpacking may take.
const RUN_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Serialize)]
struct Outcome {
    current: String,
    latest: String,
    /// `latest` is newer than `current`.
    update_available: bool,
    /// The binary was replaced.
    updated: bool,
    /// The release file for this platform.
    asset: String,
    /// The desk that is (or would be) replaced.
    path: PathBuf,
    /// This desk came from a release (else: built from source, updated with --force only).
    release_build: bool,
    /// What `desk skill install --global` did with the new version (`null`: no skills there).
    #[serde(skip_serializing_if = "Option::is_none")]
    skills: Option<serde_json::Value>,
}

pub async fn run(check: bool, force: bool) -> Result<Report> {
    let base = std::env::var("DESK_RELEASE_URL").unwrap_or_else(|_| RELEASES.to_string());
    let base = base.trim_end_matches('/');
    // The file itself, not a symlink to it (which would be replaced by a copy).
    let exe = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|e| DeskError::UpdateFailed(format!("can't find this desk's binary: {e}")))?;
    #[cfg(windows)]
    remove_old_binary(&exe);

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| DeskError::Internal(format!("http client: {e}")))?;

    let latest = String::from_utf8_lossy(&fetch(&client, &format!("{base}/VERSION")).await?)
        .trim()
        .trim_start_matches('v')
        .to_string();
    let newer = newer(&latest, CURRENT).ok_or_else(|| {
        DeskError::UpdateFailed(format!(
            "the release reports an unreadable version `{latest}`"
        ))
    })?;

    let mut outcome = Outcome {
        current: CURRENT.into(),
        latest: latest.clone(),
        update_available: newer,
        updated: false,
        asset: asset(),
        path: exe.clone(),
        release_build: RELEASE_BUILD.is_some(),
        skills: None,
    };
    if check || !newer {
        let human = if newer {
            format!("desk {latest} is available (this is {CURRENT}): `desk update` installs it\n")
        } else {
            format!("desk {CURRENT} is up to date\n")
        };
        return Ok(Report::new(&outcome, human));
    }
    if RELEASE_BUILD.is_none() && !force {
        return Err(DeskError::UpdateUnsupported(format!(
            "this desk ({}) was built from source, so it updates the same way",
            exe.display()
        )));
    }

    let dir = exe
        .parent()
        .ok_or_else(|| DeskError::UpdateFailed(format!("{} has no folder", exe.display())))?;
    let staging = Staging::new(dir)?;
    let archive = staging.path.join(&outcome.asset);
    let bytes = fetch(&client, &format!("{base}/{}", outcome.asset)).await?;
    let sums = fetch(&client, &format!("{base}/SHA256SUMS")).await?;
    verify(&bytes, &String::from_utf8_lossy(&sums), &outcome.asset)?;
    std::fs::write(&archive, &bytes).map_err(|e| failed("can't save the download", e))?;

    unpack(&archive, &staging.path).await?;
    let new = staging.path.join(binary_name());
    let reports = run_binary(&new, &["--version"]).await?;
    if reports.trim() != format!("desk {latest}") {
        return Err(DeskError::UpdateFailed(format!(
            "the downloaded binary reports `{}`, not desk {latest}; nothing was replaced",
            reports.trim()
        )));
    }
    replace(&exe, &new)?;
    outcome.updated = true;

    let mut human = format!(
        "updated desk {CURRENT} → {latest} ({})\n\
         running watches keep the old version until they end\n",
        exe.display()
    );
    let mut failure = None;
    let global = super::skill::global_dir().ok();
    if let Some(global) = global.filter(|d| super::skill::installed_in(d)) {
        match run_binary(&exe, &["--json", "skill", "install", "--global"]).await {
            Ok(out) => {
                let data = serde_json::from_str::<serde_json::Value>(&out)
                    .ok()
                    .map(|v| v["data"]["skills"].clone());
                human.push_str(&format!("updated the skills in {}\n", global.display()));
                outcome.skills = data;
            }
            Err(e) => {
                let e = DeskError::UpdateFailed(format!(
                    "desk was updated, but its skills in {} weren't: {e}",
                    global.display()
                ));
                failure = Some((e.to_json(), e.exit_code()));
            }
        }
    }
    human.push_str(
        "skills installed elsewhere (--project, --dir): run `desk skill install` there\n",
    );
    Ok(Report::new(&outcome, human).with_failure(failure))
}

fn asset() -> String {
    let ext = if cfg!(windows) { "zip" } else { "tar.gz" };
    format!("desk-{TARGET}.{ext}")
}

fn binary_name() -> &'static str {
    if cfg!(windows) { "desk.exe" } else { "desk" }
}

fn failed(what: &str, e: impl std::fmt::Display) -> DeskError {
    DeskError::UpdateFailed(format!("{what}: {e}"))
}

/// `a` is newer than `b` (both `x.y.z`); `None` when `a` isn't a version.
fn newer(a: &str, b: &str) -> Option<bool> {
    let parse = |v: &str| {
        v.split('.')
            .map(|n| n.parse::<u64>().ok())
            .collect::<Option<Vec<_>>>()
            .filter(|parts| parts.len() == 3)
    };
    Some(parse(a)? > parse(b)?)
}

async fn fetch(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| failed(&format!("can't reach {url}"), e))?;
    if !response.status().is_success() {
        return Err(DeskError::UpdateFailed(format!(
            "{url} answered {}",
            response.status()
        )));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|e| failed(&format!("download of {url} broke off"), e))?;
    Ok(bytes.to_vec())
}

/// The download matches its line in `SHA256SUMS` (`<hex>  <file>`, or `*<file>`).
fn verify(bytes: &[u8], sums: &str, asset: &str) -> Result<()> {
    let expected = sums
        .lines()
        .filter_map(|l| l.split_once(char::is_whitespace))
        .find(|(_, file)| file.trim().trim_start_matches('*') == asset)
        .map(|(hash, _)| hash.to_ascii_lowercase())
        .ok_or_else(|| DeskError::UpdateFailed(format!("SHA256SUMS doesn't list {asset}")))?;
    let actual: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if actual != expected {
        return Err(DeskError::UpdateFailed(format!(
            "checksum mismatch for {asset}; nothing was replaced"
        )));
    }
    Ok(())
}

/// Unpack with the system's `tar` (Windows 10 and later ship one that reads zip files).
async fn unpack(archive: &Path, into: &Path) -> Result<()> {
    let tar = if cfg!(windows) {
        std::env::var_os("SystemRoot")
            .map(|root| PathBuf::from(root).join("System32").join("tar.exe"))
            .unwrap_or_else(|| PathBuf::from("tar.exe"))
    } else {
        PathBuf::from("tar")
    };
    let mut cmd = tokio::process::Command::new(&tar);
    cmd.arg("-xf").arg(archive).arg("-C").arg(into);
    let out = with_timeout("unpacking", cmd).await?;
    if !out.status.success() {
        return Err(DeskError::UpdateFailed(format!(
            "can't unpack {}: {}",
            archive.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

/// Run a desk binary and return its stdout; a failure carries its stderr.
async fn run_binary(binary: &Path, args: &[&str]) -> Result<String> {
    let mut cmd = tokio::process::Command::new(binary);
    cmd.args(args).stdin(std::process::Stdio::null());
    let out = with_timeout(&format!("running {}", binary.display()), cmd).await?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(DeskError::UpdateFailed(format!(
            "{} {} failed: {}",
            binary.display(),
            args.join(" "),
            if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            }
        )));
    }
    Ok(stdout)
}

async fn with_timeout(
    what: &str,
    mut cmd: tokio::process::Command,
) -> Result<std::process::Output> {
    cmd.kill_on_drop(true);
    match tokio::time::timeout(RUN_TIMEOUT, cmd.output()).await {
        Ok(out) => out.map_err(|e| failed(what, e)),
        Err(_) => Err(DeskError::Timeout {
            what: what.to_string(),
            after: RUN_TIMEOUT,
        }),
    }
}

/// Put `new` where `exe` is. Unix: one rename, a running desk keeps its old file. Windows can't
/// overwrite a running `.exe` but can rename it: move it aside first (removed on the next update).
fn replace(exe: &Path, new: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(new, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| failed("can't make the new binary executable", e))?;
        std::fs::rename(new, exe)
            .map_err(|e| failed(&format!("can't replace {}", exe.display()), e))
    }
    #[cfg(windows)]
    {
        let old = old_binary(exe);
        std::fs::rename(exe, &old)
            .map_err(|e| failed(&format!("can't move {} aside", exe.display()), e))?;
        std::fs::rename(new, exe).map_err(|e| {
            let restored = std::fs::rename(&old, exe).is_ok();
            failed(
                &format!(
                    "can't put the new desk in place ({})",
                    if restored {
                        "the old one is back"
                    } else {
                        "the old one is desk.exe.old"
                    }
                ),
                e,
            )
        })
    }
}

#[cfg(windows)]
fn old_binary(exe: &Path) -> PathBuf {
    exe.with_extension("exe.old")
}

/// The binary a previous update moved aside, once nothing runs it any more.
#[cfg(windows)]
fn remove_old_binary(exe: &Path) {
    let _ = std::fs::remove_file(old_binary(exe));
}

/// A folder next to the binary (same filesystem: the final rename is atomic), removed when done.
struct Staging {
    path: PathBuf,
}

impl Staging {
    fn new(dir: &Path) -> Result<Self> {
        let path = dir.join(format!(".desk-update-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path)
            .map_err(|e| failed(&format!("can't write to {}", dir.display()), e))?;
        Ok(Staging { path })
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert_eq!(newer("0.10.0", "0.9.9"), Some(true));
        assert_eq!(newer("0.1.0", "0.1.0"), Some(false));
        assert_eq!(newer("0.1.0", "0.2.0"), Some(false));
        assert_eq!(newer("1.0", "0.1.0"), None);
        assert_eq!(newer("1.0.0-rc1", "0.1.0"), None);
    }

    #[test]
    fn checksums_are_matched_by_file_name() {
        let hash = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"; // "abc"
        let sums = format!("0000  other.tar.gz\n{hash} *desk-x.tar.gz\n");
        assert!(verify(b"abc", &sums, "desk-x.tar.gz").is_ok());
        assert!(verify(b"abd", &sums, "desk-x.tar.gz").is_err());
        assert!(verify(b"abc", &sums, "desk-y.tar.gz").is_err());
    }
}
