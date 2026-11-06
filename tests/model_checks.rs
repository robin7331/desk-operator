//! Every model in `models/` passes `desk model check` (format, ratings, declarations,
//! driver loads in the sandbox with its required functions), and desk uses this checkout's copy.

#[path = "support/models.rs"]
mod harness;

use harness::{models, root, run};

#[test]
fn every_model_passes_the_check() {
    let defs = models();
    assert!(
        !defs.is_empty(),
        "no <vendor>/<model>/model.toml under {}",
        root().display()
    );
    for def in defs {
        let path = def.display().to_string();
        let out = run(&["--json", "model", "check", &path]);
        assert_eq!(out.code, 0, "{path}:\n{}", out.stdout);
    }
}

#[test]
fn desk_uses_this_checkout() {
    let out = run(&["--json", "model", "list"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let data = out.data();
    let listed = data["models"].as_array().unwrap();
    for def in models() {
        let model = def.parent().unwrap();
        let id = format!(
            "{}/{}",
            model
                .parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy(),
            model.file_name().unwrap().to_string_lossy()
        );
        let used = listed
            .iter()
            .find(|d| d["id"] == id.as_str() && d["shadowed"] == false)
            .unwrap_or_else(|| panic!("{id} not listed: {data}"));
        assert_eq!(
            used["origin"].as_str(),
            Some(root().display().to_string().as_str()),
            "{id} doesn't come from this checkout"
        );
    }
}

#[test]
fn the_dps150_driver_declares_its_params() {
    let out = harness::run(&["--json", "model", "show", "fnirsi/ips3608"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let d = out.data();
    let names: Vec<&str> = d["driver_params"]["source"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    for n in ["name", "status", "settle_ms", "boot_ms"] {
        assert!(names.contains(&n), "{n} in {names:?}");
    }
    assert_eq!(d["model"]["status"], "tested");
}

/// The rows of the *Supported devices* table in the README: (device name, status).
fn supported_devices() -> Vec<(String, String)> {
    let readme = std::fs::read_to_string(root().join("../README.md")).unwrap();
    readme
        .lines()
        .skip_while(|l| *l != "## Supported devices")
        .skip(1)
        .take_while(|l| !l.starts_with("## "))
        .filter_map(|l| {
            let cells: Vec<&str> = l.trim_matches('|').split('|').map(str::trim).collect();
            let name = cells.first()?.strip_prefix('[')?.split(']').next()?;
            Some((name.to_string(), cells.get(2)?.to_string()))
        })
        .collect()
}

#[test]
fn the_readme_lists_every_model_with_its_status() {
    let mut expected: Vec<(String, String)> = models()
        .iter()
        .map(|def| {
            let model = def.parent().unwrap();
            let vendor = model
                .parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy();
            let id = format!("{vendor}/{}", model.file_name().unwrap().to_string_lossy());
            let out = run(&["--json", "model", "show", &id]);
            assert_eq!(out.code, 0, "{}", out.stdout);
            let d = &out.data()["model"];
            (
                d["name"].as_str().unwrap().to_string(),
                d["status"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    expected.sort();
    assert_eq!(
        supported_devices(),
        expected,
        "README *Supported devices* must list every model, sorted, with its status"
    );
}
