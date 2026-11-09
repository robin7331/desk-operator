//! `desk update` against a fake release (wiremock): a copy of the binary in a temp folder updates
//! itself. Replacing it is tested on Unix, where a shell script can stand in for the new desk.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CURRENT: &str = env!("CARGO_PKG_VERSION");

struct Install {
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
}

impl Install {
    /// A copy of this build's desk, as if installed in `<tmp>/bin`.
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("bin")).unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        let install = Install { dir };
        std::fs::copy(env!("CARGO_BIN_EXE_desk"), install.exe()).unwrap();
        install
    }

    fn exe(&self) -> PathBuf {
        let name = if cfg!(windows) { "desk.exe" } else { "desk" };
        self.dir.path().join("bin").join(name)
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    async fn run(&self, server: &MockServer, args: &[&str]) -> Out {
        let mut cmd = Command::new(self.exe());
        cmd.args(args)
            .env("DESK_RELEASE_URL", server.uri())
            .env("HOME", self.home())
            .env("DESK_CONFIG", self.dir.path().join("desk.toml"));
        let out = tokio::task::spawn_blocking(move || cmd.output().unwrap())
            .await
            .unwrap();
        Out {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        }
    }

    /// Nothing but the binary is left in its folder (no staging folder).
    fn bin_entries(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.dir.path().join("bin"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

/// A release that serves `VERSION`, and the asset with its `SHA256SUMS` line when given.
async fn release(version: &str, asset: Option<(&str, Vec<u8>, String)>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/VERSION"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!("{version}\n")))
        .mount(&server)
        .await;
    if let Some((name, bytes, sums)) = asset {
        Mock::given(method("GET"))
            .and(path(format!("/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/SHA256SUMS"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sums))
            .mount(&server)
            .await;
    }
    server
}

/// This platform's asset name, as `desk update --check` reports it.
async fn asset_name() -> String {
    let install = Install::new();
    let server = release(CURRENT, None).await;
    let out = install.run(&server, &["--json", "update", "--check"]).await;
    out.json()["data"]["asset"].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn check_reports_a_newer_release_and_changes_nothing() {
    let install = Install::new();
    let server = release("99.0.0", None).await;
    let out = install.run(&server, &["--json", "update", "--check"]).await;
    assert_eq!(out.code, 0, "{}", out.stdout);
    let data = &out.json()["data"];
    assert_eq!(data["current"], CURRENT);
    assert_eq!(data["latest"], "99.0.0");
    assert_eq!(data["update_available"], true);
    assert_eq!(data["updated"], false);
    assert_eq!(
        data["release_build"], false,
        "tests run a build from source"
    );
    let asset = data["asset"].as_str().unwrap();
    assert!(asset.starts_with("desk-"), "{asset}");
    assert!(asset.ends_with(if cfg!(windows) { ".zip" } else { ".tar.gz" }));

    let human = install.run(&server, &["update", "--check"]).await;
    assert!(
        human.stdout.contains("desk 99.0.0 is available"),
        "{}",
        human.stdout
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_same_or_an_older_release_is_up_to_date() {
    let install = Install::new();
    for version in [CURRENT, "0.0.1"] {
        let server = release(version, None).await;
        let out = install.run(&server, &["--json", "update"]).await;
        assert_eq!(out.code, 0, "{}", out.stdout);
        assert_eq!(out.json()["data"]["update_available"], false);
        assert_eq!(out.json()["data"]["updated"], false);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_desk_built_from_source_is_not_replaced() {
    let install = Install::new();
    let before = std::fs::read(install.exe()).unwrap();
    let server = release("99.0.0", None).await;
    let out = install.run(&server, &["--json", "update"]).await;
    assert_eq!(out.code, 3, "{}", out.stdout);
    let error = &out.json()["error"];
    assert_eq!(error["code"], "update_unsupported");
    assert!(error["hint"].as_str().unwrap().contains("--force"));
    assert_eq!(std::fs::read(install.exe()).unwrap(), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_release_or_an_odd_version_fails_clearly() {
    let install = Install::new();
    let empty = MockServer::start().await;
    let out = install.run(&empty, &["--json", "update", "--check"]).await;
    assert_eq!(out.code, 4, "{}", out.stdout);
    assert_eq!(out.json()["error"]["code"], "update_failed");
    assert!(
        out.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("404")
    );

    let odd = release("next", None).await;
    let out = install.run(&odd, &["--json", "update", "--check"]).await;
    assert_eq!(out.json()["error"]["code"], "update_failed");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_checksum_mismatch_replaces_nothing() {
    let asset = asset_name().await;
    let install = Install::new();
    let before = std::fs::read(install.exe()).unwrap();
    let sums = format!("{}  {asset}\n", "0".repeat(64));
    let server = release("99.0.0", Some((&asset, b"not desk".to_vec(), sums))).await;
    let out = install.run(&server, &["--json", "update", "--force"]).await;
    assert_eq!(out.code, 4, "{}", out.stdout);
    let message = out.json()["error"]["message"].as_str().unwrap().to_string();
    assert!(message.contains("checksum mismatch"), "{message}");
    assert_eq!(std::fs::read(install.exe()).unwrap(), before);
    assert_eq!(
        install.bin_entries().len(),
        1,
        "{:?}",
        install.bin_entries()
    );
}

/// A release archive whose `desk` is a shell script: `--version` reports `reports`, anything
/// else runs the real desk (so `skill install` works).
#[cfg(unix)]
fn fake_release(dir: &Path, asset: &str, reports: &str) -> (Vec<u8>, String) {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;
    let pkg = dir.join("pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    let script = pkg.join("desk");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo '{reports}'; else exec '{}' \"$@\"; fi\n",
            env!("CARGO_BIN_EXE_desk")
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let archive = dir.join(asset);
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&pkg)
        .arg("desk")
        .status()
        .unwrap();
    assert!(status.success());
    let bytes = std::fs::read(&archive).unwrap();
    let hash: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    (bytes, format!("{hash}  {asset}\n"))
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn force_replaces_the_binary_with_the_verified_release() {
    let asset = asset_name().await;
    let install = Install::new();
    let work = TempDir::new().unwrap();
    let (bytes, sums) = fake_release(work.path(), &asset, "desk 99.0.0");
    let server = release("99.0.0", Some((&asset, bytes, sums))).await;

    let out = install.run(&server, &["--json", "update", "--force"]).await;
    assert_eq!(out.code, 0, "{}", out.stdout);
    let data = &out.json()["data"];
    assert_eq!(data["updated"], true);
    assert!(
        data.get("skills").is_none(),
        "no skills were installed: {data}"
    );
    let version = Command::new(install.exe())
        .arg("--version")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        "desk 99.0.0"
    );
    assert_eq!(install.bin_entries(), vec!["desk".to_string()]);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_binary_that_reports_another_version_is_not_installed() {
    let asset = asset_name().await;
    let install = Install::new();
    let before = std::fs::read(install.exe()).unwrap();
    let work = TempDir::new().unwrap();
    let (bytes, sums) = fake_release(work.path(), &asset, "desk 98.0.0");
    let server = release("99.0.0", Some((&asset, bytes, sums))).await;

    let out = install.run(&server, &["--json", "update", "--force"]).await;
    assert_eq!(out.code, 4, "{}", out.stdout);
    let message = out.json()["error"]["message"].as_str().unwrap().to_string();
    assert!(message.contains("nothing was replaced"), "{message}");
    assert_eq!(std::fs::read(install.exe()).unwrap(), before);
    assert_eq!(install.bin_entries(), vec!["desk".to_string()]);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn globally_installed_skills_are_updated_too() {
    let asset = asset_name().await;
    let install = Install::new();
    let skill = install.home().join(".claude/skills/desk-operator/SKILL.md");
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(
        &skill,
        "---\nname: desk-operator\ndescription: old\n---\nold\n",
    )
    .unwrap();
    let work = TempDir::new().unwrap();
    let (bytes, sums) = fake_release(work.path(), &asset, "desk 99.0.0");
    let server = release("99.0.0", Some((&asset, bytes, sums))).await;

    let out = install.run(&server, &["--json", "update", "--force"]).await;
    assert_eq!(out.code, 0, "{}", out.stdout);
    let skills = out.json()["data"]["skills"].clone();
    let operator = skills
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "desk-operator")
        .unwrap()
        .clone();
    assert_eq!(operator["action"], "updated", "{skills}");
    let current = Command::new(env!("CARGO_BIN_EXE_desk"))
        .args(["skill", "desk-operator"])
        .output()
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&skill).unwrap(),
        String::from_utf8_lossy(&current.stdout)
    );
}
