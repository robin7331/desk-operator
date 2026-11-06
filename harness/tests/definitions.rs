//! Every definition in this library passes `desk library check` (format, ratings, declarations,
//! driver loads in the sandbox with its required functions), and desk uses this checkout's copy.

use harness::{definitions, root, run};

#[test]
fn every_definition_passes_the_check() {
    let defs = definitions();
    assert!(
        !defs.is_empty(),
        "no <vendor>/<model>/device.toml under {}",
        root().display()
    );
    for def in defs {
        let path = def.display().to_string();
        let out = run(&["--json", "library", "check", &path]);
        assert_eq!(out.code, 0, "{path}:\n{}", out.stdout);
    }
}

#[test]
fn desk_uses_this_checkout() {
    let out = run(&["--json", "library", "list"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let data = out.data();
    let listed = data["definitions"].as_array().unwrap();
    for def in definitions() {
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
