//! Helpers for testing the definitions in this library against a real `desk` binary.
//!
//! Tests run the `desk` on your `PATH`, or the one in `$DESK_BIN` (e.g. a desk-operator build:
//! `DESK_BIN=../desk-operator/target/debug/desk cargo test`). desk finds this checkout's
//! definitions through `$DESK_DEVICES`, so a test always uses the files here, never the ones
//! bundled into desk.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

/// The root of this library checkout (vendor folders live here).
pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("harness/ is inside the library")
        .to_path_buf()
}

/// `desk` with this checkout first on the definition search path and no user config.
pub fn desk() -> Command {
    let bin = std::env::var_os("DESK_BIN").unwrap_or_else(|| "desk".into());
    let mut c = Command::new(bin);
    c.env_remove("DESK_CONFIG").env("DESK_DEVICES", root());
    c
}

/// Every `device.toml` in this library (`<vendor>/<model>/device.toml`).
pub fn definitions() -> Vec<PathBuf> {
    let dirs = |p: &Path| -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(p)
            .map(|r| {
                r.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    };
    dirs(&root())
        .iter()
        .flat_map(|vendor| dirs(vendor))
        .map(|model| model.join("device.toml"))
        .filter(|p| p.is_file())
        .collect()
}

/// A finished `desk` run.
pub struct Out {
    pub code: i32,
    pub stdout: String,
}

impl Out {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("stdout is not JSON ({e}):\n{}", self.stdout))
    }
    pub fn data(&self) -> Value {
        self.json()["data"].clone()
    }
}

/// Run `desk` (see [`desk`]) and collect its exit code and stdout.
pub fn run(args: &[&str]) -> Out {
    let out = desk()
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("cannot run desk (install it, or set DESK_BIN): {e}"));
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
    }
}
