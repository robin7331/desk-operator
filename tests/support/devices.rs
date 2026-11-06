//! Helpers for testing the definitions in `devices/` against this checkout's `desk` binary.
//!
//! desk finds the definitions through `$DESK_DEVICES`, so a test sees the files as they are now,
//! and the user's own config and `devices/` folder stay out of the way.
#![allow(dead_code)] // every test file uses only some of these

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

/// The device library (vendor folders live here).
pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("devices")
}

/// This checkout's `desk`, with its library on the definition search path and no user config.
pub fn desk() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_desk"));
    // A config path that doesn't exist: no `devices/` next to it can shadow the library.
    let config = std::env::temp_dir().join(format!("desk-no-config-{}", std::process::id()));
    c.env("DESK_CONFIG", config.join("desk.toml"))
        .env("DESK_DEVICES", root());
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
        .unwrap_or_else(|e| panic!("cannot run desk: {e}"));
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
    }
}
