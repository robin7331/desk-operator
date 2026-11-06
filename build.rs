//! Bundles the device library (`devices/`) into the binary, so installed copies of desk have the
//! definitions without a checkout. Only the vendor folders are bundled (`<vendor>/…`).
//! Generates `$OUT_DIR/bundled_devices.rs`:
//! `pub static BUNDLED: &[(&str, &str)] = &[("acme/psu-3000/device.toml", "<contents>"), …];`
//!
//! Only packaging lives here: the core (`src/`) never refers to any specific definition.

use std::path::{Path, PathBuf};

fn collect(dir: &Path, root: &Path, out: &mut Vec<(String, PathBuf)>) {
    // Watch every directory, so newly added definitions trigger a rebuild too.
    println!("cargo:rerun-if-changed={}", dir.display());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect(&path, root, out);
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("toml" | "lua")
        ) {
            let rel = path
                .strip_prefix(root)
                .expect("inside root")
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            out.push((rel, path));
        }
    }
}

fn main() {
    let root =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir")).join("devices");

    println!("cargo:rerun-if-changed={}", root.display());
    let mut files = Vec::new();
    let mut vendors: Vec<PathBuf> = std::fs::read_dir(&root)
        .map(|r| r.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    vendors.sort();
    for dir in vendors.iter().filter(|p| p.is_dir()) {
        let name = dir.file_name().unwrap_or_default().to_string_lossy();
        if !name.starts_with('.') {
            collect(dir, &root, &mut files);
        }
    }
    if files.is_empty() {
        println!("cargo:warning=no device definitions bundled: devices/ is empty");
    }
    let mut code = String::from("pub static BUNDLED: &[(&str, &str)] = &[\n");
    for (rel, path) in &files {
        println!("cargo:rerun-if-changed={}", path.display());
        code.push_str(&format!(
            "    ({rel:?}, include_str!({:?})),\n",
            path.display().to_string()
        ));
    }
    code.push_str("];\n");

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("out dir")).join("bundled_devices.rs");
    std::fs::write(out, code).expect("write bundled_devices.rs");
}
