//! sigrok as a connection kind, end to end against a fake `sigrok-cli`
//! (`tests/support/fake_sigrok_cli.rs`): backends as providers, `desk sigrok drivers|show`, the
//! `sigrok = { … }` binding in models and devices, backend resolution, `ctx.sigrok.acquire` with
//! the exact command line, its checks and failures, timeouts, the size cap, cancelling, and one
//! user per device. The last test uses the real `sigrok-cli -d demo` when it is installed.
//!
//! Unix only for now: the fake and these tests are not yet tried on Windows.
#![cfg(unix)]

#[path = "support/sigrok.rs"]
mod sigrok;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sigrok::{SigrokBench, files};

/// A logic analyzer on the fake backend: `grab` captures `samples` with the given request.
const LA: &str = r#"
[devices.la.capture]
script  = "la.lua"
sigrok  = { driver = "demo", backend = "fake" }
timeout = "300ms"

[devices.la.capture.actions.grab]
switches = false
produces = "file"

[devices.la.capture.actions.grab.args]
mode    = { type = "choice", choices = ["ok", "full", "slow", "hostile", "plain"], default = "ok" }
case    = { type = "string", optional = true }
samples = { type = "number", default = 1000 }
timeout = { type = "number", unit = "s", default = 5 }
"#;

/// The driver: `mode` picks a request, `case` a broken one (each must be refused before sigrok
/// runs).
const LA_LUA: &str = r#"
PARAMS = {}

local BROKEN = {
  unknown_key   = function(r) r.args = { "-o", "/etc/passwd" } end,
  program       = function(r) r.program = "/bin/sh" end,
  config_key    = function(r) r.config = { ["Sample Rate"] = 1 } end,
  config_dash   = function(r) r.config = { ["--output-file"] = "x" } end,
  config_colon  = function(r) r.config = { samplerate = "1m:limit_samples=1" } end,
  config_equals = function(r) r.config = { samplerate = "a=b" } end,
  config_lead   = function(r) r.config = { samplerate = "-o" } end,
  config_space  = function(r) r.config = { samplerate = "1 m" } end,
  config_ctrl   = function(r) r.config = { samplerate = "1\n" } end,
  config_table  = function(r) r.config = { samplerate = {} } end,
  config_nan    = function(r) r.config = { samplerate = 0/0 } end,
  channel_dash  = function(r) r.channels = { "-o" } end,
  channel_path  = function(r) r.channels = "D0,../x" end,
  channel_empty = function(r) r.channels = {} end,
  no_length     = function(r) r.samples = nil end,
  two_lengths   = function(r) r.time = 100 end,
  zero_samples  = function(r) r.samples = 0 end,
  frac_samples  = function(r) r.samples = 1.5 end,
  bad_time      = function(r) r.samples = nil; r.time = "soon" end,
  trigger_colon = function(r) r.triggers = "D0=r:x" end,
  trigger_lead  = function(r) r.triggers = "-D0" end,
  wait_alone    = function(r) r.wait_trigger = true end,
  format_opts   = function(r) r.format = "csv:header=false" end,
  format_path   = function(r) r.format = "../x" end,
  no_timeout    = function(r) r.timeout = nil end,
  zero_timeout  = function(r) r.timeout = 0 end,
  no_into       = function(r) r.into = nil end,
  into_string   = function(r) r.into = "/tmp/x" end,
}

function action(ctx, name, args)
  local r = { samples = args.samples, into = ctx.output, timeout = args.timeout * 1000 }
  if args.mode == "full" then
    r = {
      config = { samplerate = 10e6, averaging = false, captureratio = 20, voltage = 1.5, mode = "fast" },
      channels = { "D0", "D1-D3" },
      time = "2s",
      triggers = "D0=r,D1=1",
      wait_trigger = true,
      format = "binary",
      into = ctx.output,
      timeout = "10s",
    }
  elseif args.mode == "plain" then
    r.channels = "D0, D1"
    r.format = "srzip"
  end
  if args.case then
    local f = BROKEN[args.case]
    if not f then error("no case " .. args.case) end
    f(r)
  end
  local got = ctx.sigrok.acquire(r)
  return { format = r.format or "srzip", extension = "sr", bytes = got.bytes,
           driver = ctx.sigrok.driver, conn = ctx.sigrok.conn }
end
"#;

const DEMO: &str = "demo - Demo device with 13 channels: D0 D1 D2 D3 D4 D5 D6 D7 A0 A1 A2 A3 A4";

fn la_bench() -> SigrokBench {
    let b = SigrokBench::new(LA);
    std::fs::write(b.path("la.lua"), LA_LUA).unwrap();
    b.fake.devices(DEMO);
    b
}

/// Nothing at all below `dir`: no files and no folders (or `dir` doesn't exist).
fn nothing_left(dir: &Path) -> bool {
    std::fs::read_dir(dir).map_or(true, |mut e| e.next().is_none())
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The fake holds `busy.lock` while it captures: free means no capture runs anymore.
fn device_free(fake: &sigrok::Fake) -> bool {
    match std::fs::File::options()
        .write(true)
        .open(fake.dir.join("busy.lock"))
    {
        Ok(f) => f.try_lock().is_ok(),
        Err(_) => true,
    }
}

/// The `-o` argument of a capture.
fn output_of(call: &[String]) -> PathBuf {
    let i = call.iter().position(|a| a == "-o").unwrap();
    PathBuf::from(&call[i + 1])
}

// ---------------------------------------------------------------------------------------------
// Backends as providers

#[test]
fn a_backend_reports_its_program_version_and_drivers() {
    let b = SigrokBench::new("");
    b.fake.version("sigrok-cli 0.8.0").drivers(
        "demo Demo driver and pattern generator\nfx2lafw fx2lafw (generic driver for FX2 based LAs)",
    );
    let out = b.run(&["--json", "provider", "test", "fake"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let detail = out.data()["detail"].as_str().unwrap().to_string();
    assert!(detail.contains("fake-sigrok-cli"), "{detail}");
    assert!(detail.contains("sigrok-cli 0.8.0"), "{detail}");
    assert!(detail.contains("2 drivers"), "{detail}");

    b.fake.drivers("demo Demo driver and pattern generator");
    let human = b.run(&["provider", "test", "fake"]).stdout;
    assert!(
        human.contains("1 driver") && !human.contains("1 drivers"),
        "{human}"
    );
    let human = b.run(&["sigrok", "drivers", "--backend", "fake"]).stdout;
    assert!(human.contains(", 1 driver\n"), "{human}");
}

#[test]
fn endpoints_are_what_the_scan_finds_ready_to_paste() {
    let b = SigrokBench::new("");
    b.fake.devices(&format!(
        "{DEMO}\nfx2lafw:conn=1.23 - Saleae Logic with 8 channels: D0 D1 D2 D3 D4 D5 D6 D7"
    ));
    let out = b.run(&["--json", "provider", "endpoints", "fake"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let d = out.data();
    assert_eq!(d["binding"], "sigrok");
    assert!(d.get("capability").is_none(), "{d}");
    let eps = d["endpoints"].as_array().unwrap();
    assert_eq!(eps.len(), 2);
    assert_eq!(eps[0]["id"], "demo");
    assert_eq!(eps[0]["params"], json!({ "driver": "demo" }));
    assert!(eps[0]["name"].as_str().unwrap().starts_with("Demo device"));
    assert_eq!(eps[1]["id"], "fx2lafw:conn=1.23");
    assert_eq!(
        eps[1]["params"],
        json!({ "driver": "fx2lafw", "conn": "1.23" })
    );
    assert_eq!(b.fake.calls(), [["--scan"]]);

    let human = b.run(&["provider", "endpoints", "fake"]).stdout;
    assert!(human.contains("sigrok = { driver = \"demo\" }"), "{human}");
    // --search works like for any provider.
    let out = b.run(&[
        "--json",
        "provider",
        "endpoints",
        "fake",
        "--search",
        "saleae",
    ]);
    assert_eq!(out.data()["endpoints"].as_array().unwrap().len(), 1);
    // Power providers keep their shape.
    let mock = SigrokBench::new("[providers.sim]\nkind = \"mock\"\nendpoints = [\"a\"]\n");
    let d = mock.run(&["--json", "provider", "endpoints", "sim"]).data();
    assert_eq!(d["capability"], "power_switch");
    assert_eq!(d["binding"], "power");
}

#[test]
fn the_implicit_backend_is_sigrok_cli_on_path() {
    let b = SigrokBench::new("");
    // Not on PATH: not listed, and asking for it says how to get it.
    let list = b.run(&["--json", "provider", "list"]).data();
    let ids: Vec<&str> = list["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["fake"]);
    let out = b.run(&["--json", "provider", "test", "sigrok"]);
    assert_eq!(out.code_str(), "provider_unreachable", "{}", out.stdout);
    assert_eq!(out.code, 4);
    assert!(out.message().contains("sigrok-cli not found on PATH"));
    let hint = out.json()["error"]["hint"].as_str().unwrap().to_string();
    assert!(
        hint.contains("https://sigrok.org") && hint.contains("program"),
        "{hint}"
    );

    // On PATH: listed as implicit, and it works.
    let stock = b.install_on_path();
    let list = b.run(&["--json", "provider", "list"]).data();
    assert_eq!(list["providers"][0]["id"], "sigrok");
    assert_eq!(list["providers"][0]["kind"], "sigrok");
    assert_eq!(list["providers"][0]["implicit"], true);
    assert!(list["providers"][1].get("implicit").is_none());
    let out = b.run(&["--json", "provider", "test", "sigrok"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert!(stock.calls().contains(&vec!["--version".to_string()]));

    // A provider of another kind called `sigrok` replaces the implicit one.
    b.config("[providers.sigrok]\nkind = \"mock\"\n");
    let list = b.run(&["--json", "provider", "list"]).data();
    assert_eq!(list["providers"][0]["kind"], "mock");
    assert!(list["providers"][0].get("implicit").is_none());
}

#[test]
fn a_backend_is_not_a_power_binding() {
    let b = SigrokBench::new("[devices.x]\npower = { provider = \"fake\", endpoint = \"a\" }\n");
    let out = b.run(&["--json", "power", "on", "x"]);
    assert_eq!(out.code_str(), "config_invalid", "{}", out.stdout);
    assert!(out.message().contains("sigrok = {"), "{}", out.message());
}

#[test]
fn a_backend_checks_its_settings() {
    let b = SigrokBench::new("[providers.v]\nkind = \"sigrok\"\nprogrm = \"x\"\n");
    let out = b.run(&["--json", "provider", "test", "v"]);
    assert_eq!(out.code_str(), "config_invalid");
    assert!(out.message().contains("progrm"), "{}", out.message());
    // A program that isn't there; relative paths resolve against the config folder.
    b.config("[providers.v]\nkind = \"sigrok\"\nprogram = \"tools/vendor-cli\"\n");
    let out = b.run(&["--json", "provider", "test", "v"]);
    assert_eq!(out.code_str(), "provider_unreachable");
    let expected = b.path("tools/vendor-cli");
    assert!(
        out.message().contains(&expected.display().to_string()),
        "{}",
        out.message()
    );
    assert!(out.json()["error"]["hint"].as_str().is_some());
}

// ---------------------------------------------------------------------------------------------
// desk sigrok drivers | show

#[test]
fn drivers_lists_every_backend_and_a_missing_one_with_its_error() {
    let b = SigrokBench::new("");
    b.fake
        .drivers("demo Demo driver\nfx2lafw fx2lafw (generic driver for FX2 based LAs)");
    let out = b.run(&["--json", "sigrok", "drivers"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let backends = out.data()["backends"].as_array().unwrap().clone();
    assert_eq!(backends.len(), 2, "{backends:?}");
    assert_eq!(backends[0]["id"], "sigrok");
    assert_eq!(backends[0]["implicit"], true);
    assert_eq!(backends[0]["error"]["code"], "provider_unreachable");
    assert_eq!(backends[1]["id"], "fake");
    assert_eq!(backends[1]["version"], "sigrok-cli 0.7.2-fake");
    assert_eq!(
        backends[1]["drivers"],
        json!([
            { "id": "demo", "description": "Demo driver" },
            { "id": "fx2lafw", "description": "fx2lafw (generic driver for FX2 based LAs)" }
        ])
    );
    let human = b.run(&["sigrok", "drivers"]).stdout;
    assert!(human.contains("fx2lafw"), "{human}");
    assert!(human.contains("sigrok-cli not found"), "{human}");

    // Asked for by name: only that one, and its failure is the command's.
    let out = b.run(&["--json", "sigrok", "drivers", "--backend", "fake"]);
    assert_eq!(out.data()["backends"].as_array().unwrap().len(), 1);
    let out = b.run(&["--json", "sigrok", "drivers", "--backend", "sigrok"]);
    assert_eq!(out.code_str(), "provider_unreachable");
    let out = b.run(&["--json", "sigrok", "drivers", "--backend", "nope"]);
    assert_eq!(out.code_str(), "config_invalid");

    // Without a config file: the implicit backend only.
    let o = Command::new(env!("CARGO_BIN_EXE_desk"))
        .env_remove("DESK_CONFIG")
        .env("PATH", b.path("bin"))
        .arg("--config")
        .arg(b.path("none/desk.toml"))
        .args(["--json", "sigrok", "drivers"])
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["data"]["backends"].as_array().unwrap().len(), 1, "{v}");
}

#[test]
fn show_prints_sigroks_text_unparsed() {
    let b = SigrokBench::new("");
    b.fake
        .devices(DEMO)
        .show("Driver functions:\n    Logic analyzer\nanything at all\n");
    let out = b.run(&["--json", "sigrok", "show", "demo", "--backend", "fake"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(
        out.data(),
        json!({ "backend": "fake", "driver": "demo", "conn": null,
                "text": "Driver functions:\n    Logic analyzer\nanything at all\n" })
    );
    let out = b.run(&["sigrok", "show", "demo", "--conn", "1.23"]);
    assert_eq!(out.code, 0);
    assert!(out.stdout.contains("anything at all"));
    assert_eq!(b.fake.calls()[1], ["-d", "demo:conn=1.23", "--show"]);

    // A driver the backend doesn't have; a device that isn't there; hostile input.
    let out = b.run(&["--json", "sigrok", "show", "nosuch", "--backend", "fake"]);
    assert_eq!(out.code_str(), "config_invalid", "{}", out.stdout);
    assert!(out.message().contains("`nosuch`"), "{}", out.message());
    b.fake.devices("");
    let out = b.run(&["--json", "sigrok", "show", "demo"]);
    assert_eq!(out.code_str(), "endpoint_unavailable", "{}", out.stdout);
    for bad in [&["show", "-x"][..], &["show", "demo", "--conn", "a b"]] {
        let mut args = vec!["--json", "sigrok"];
        args.extend_from_slice(bad);
        let out = b.run(&args);
        assert_eq!(out.code, 2, "{bad:?}: {}", out.stdout);
    }
}

// ---------------------------------------------------------------------------------------------
// The binding in models and devices

const MODEL: &str = r#"
[model]
id       = "test/la"
name     = "Test LA"
status   = "community"
requires = "desk-operator >= 0.3"

[capture]
script = "la.lua"
sigrok = { driver = "demo", build = "https://example.org/builds" }

[capture.actions.grab]
switches = false
produces = "file"
args = { samples = { type = "number", default = 1000 }, timeout = { type = "number", unit = "s", default = 5 } }
"#;

fn model_bench(device: &str) -> SigrokBench {
    let b = SigrokBench::new(device);
    let dir = b.path("models/test/la");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("model.toml"), MODEL).unwrap();
    std::fs::write(dir.join("la.lua"), LA_LUA).unwrap();
    b.fake.devices(DEMO);
    b
}

#[test]
fn a_device_adds_its_conn_and_backend_to_the_models_driver() {
    let b = model_bench(
        "[devices.la]\nmodel = \"test/la\"\ncapture = { sigrok = { conn = \"1.23\", backend = \"fake\" } }\n",
    );
    let out = b.run(&["--json", "device", "show", "la"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(
        out.data()["device"]["bindings"]["capture"]["sigrok"],
        json!({ "driver": "demo", "build": "https://example.org/builds",
                "conn": "1.23", "backend": "fake" })
    );
    let human = b.run(&["device", "show", "la"]).stdout;
    assert!(
        human.contains("with sigrok demo conn=1.23 via fake"),
        "{human}"
    );
    let schema = b.run(&["--json", "schema"]).data();
    assert_eq!(
        schema["devices"][0]["bindings"]["capture"]["sigrok"]["conn"],
        "1.23"
    );
    let model = b.run(&["model", "show", "test/la"]).stdout;
    assert!(
        model.contains("[capture.sigrok]") || model.contains("sigrok = "),
        "{model}"
    );

    let out = b.run(&["--json", "do", "la", "grab", "samples=10"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.data()["meta"]["conn"], "1.23");
    assert_eq!(out.data()["meta"]["driver"], "demo");
    assert_eq!(b.fake.captures()[0][..2], ["-d", "demo:conn=1.23"]);
    assert!(b.run(&["--json", "model", "check", "test/la"]).code == 0);
}

#[test]
fn per_bench_keys_are_refused_in_a_model() {
    for (bad, what) in [
        (
            "sigrok = { driver = \"demo\", conn = \"1.2\" }",
            "per bench",
        ),
        (
            "sigrok = { driver = \"demo\", backend = \"v\" }",
            "per bench",
        ),
        ("sigrok = { build = \"https://example.org\" }", "driver"),
        ("sigrok = { driver = \"Demo\" }", "driver id"),
        (
            "sigrok = { driver = \"demo\", build = \"not a url\" }",
            "URL",
        ),
        (
            "sigrok = { driver = \"demo\", program = \"/bin/sh\" }",
            "unknown key",
        ),
        ("sigrok = \"demo\"", "table"),
    ] {
        let b = SigrokBench::new("");
        let dir = b.path("models/test/la");
        std::fs::create_dir_all(&dir).unwrap();
        let text = MODEL.replace(
            "sigrok = { driver = \"demo\", build = \"https://example.org/builds\" }",
            bad,
        );
        std::fs::write(dir.join("model.toml"), text).unwrap();
        std::fs::write(dir.join("la.lua"), LA_LUA).unwrap();
        let out = b.run(&["--json", "model", "check", &dir.display().to_string()]);
        assert_ne!(out.code, 0, "{bad}: {}", out.stdout);
        let msg = out.data()["results"][0]["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(msg.contains(what), "{bad}: {msg}");
    }
}

#[test]
fn a_devices_sigrok_binding_is_checked() {
    for (bad, what) in [
        ("sigrok = { driver = \"demo\", conn = \"a b\" }", "spaces"),
        (
            "sigrok = { driver = \"demo\", backend = \"missing\" }",
            "not defined",
        ),
        (
            "sigrok = { driver = \"demo\", backend = \"sim\" }",
            "not kind",
        ),
        ("sigrok = { conn = \"1.2\" }", "driver"),
        (
            "sigrok = { driver = \"demo\", extra = \"x\" }",
            "unknown key",
        ),
    ] {
        let b = SigrokBench::new(&format!(
            "[providers.sim]\nkind = \"mock\"\n[devices.la.capture]\nscript = \"la.lua\"\n{bad}\nactions = {{ grab = {{ switches = false, produces = \"file\" }} }}\n"
        ));
        let out = b.run(&["--json", "device", "list"]);
        assert_eq!(out.code_str(), "config_invalid", "{bad}: {}", out.stdout);
        assert!(out.message().contains(what), "{bad}: {}", out.message());
    }
}

// ---------------------------------------------------------------------------------------------
// Which backend

/// `la` without a backend: resolution decides. `driver` is the binding's.
fn resolving(driver: &str, providers: &str) -> String {
    format!(
        "{providers}\n[devices.la.capture]\nscript = \"la.lua\"\nsigrok = {{ driver = \"{driver}\", build = \"https://example.org/builds\" }}\n[devices.la.capture.actions.grab]\nswitches = false\nproduces = \"file\"\nargs = {{ mode = {{ type = \"choice\", choices = [\"ok\"], default = \"ok\" }}, samples = {{ type = \"number\", default = 10 }}, timeout = {{ type = \"number\", unit = \"s\", default = 5 }} }}\n"
    )
}

fn resolving_bench(driver: &str) -> (SigrokBench, sigrok::Fake, sigrok::Fake) {
    let b = SigrokBench::new("");
    let stock = b.install_on_path();
    let (vendor_path, vendor) = b.install("vendor-cli");
    // The vendor build comes first in the config; stock `sigrok` is still asked first.
    b.config(&resolving(
        driver,
        &format!(
            "[providers.vendor]\nkind = \"sigrok\"\nprogram = {:?}\n[providers.fake]\nkind = \"mock\"\n",
            vendor_path.display().to_string()
        ),
    ));
    std::fs::write(b.path("la.lua"), LA_LUA).unwrap();
    stock.drivers("demo Demo driver").devices(DEMO);
    vendor
        .drivers("demo Demo driver\nvendor-la Vendor LA")
        .devices("vendor-la - Vendor LA with 8 channels: D0 D1 D2 D3 D4 D5 D6 D7");
    (b, stock, vendor)
}

#[test]
fn the_first_backend_with_the_driver_runs_it_stock_first() {
    let (b, stock, vendor) = resolving_bench("vendor-la");
    let out = b.run(&["--json", "do", "la", "grab"]);
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    assert_eq!(stock.driver_lists(), 1);
    assert_eq!(vendor.driver_lists(), 1);
    assert!(stock.captures().is_empty());
    assert_eq!(vendor.captures().len(), 1);

    // Both have `demo`: stock wins.
    let (b, stock, vendor) = resolving_bench("demo");
    let out = b.run(&["--json", "do", "la", "grab"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(stock.captures().len(), 1);
    assert_eq!(
        vendor.driver_lists(),
        0,
        "stock had it: the vendor build wasn't asked"
    );
}

#[test]
fn no_backend_with_the_driver_names_the_driver_the_backends_and_the_build() {
    let (b, stock, vendor) = resolving_bench("nosuch");
    let out = b.run(&["--json", "do", "la", "grab"]);
    assert_eq!(out.code_str(), "config_invalid", "{}", out.stdout);
    let msg = out.message();
    for part in ["`nosuch`", "sigrok", "vendor", "https://example.org/builds"] {
        assert!(msg.contains(part), "{part}: {msg}");
    }
    assert!(stock.captures().is_empty() && vendor.captures().is_empty());
}

#[test]
fn one_backend_runs_without_asking_for_its_drivers() {
    let b = la_bench();
    b.config(&LA.replace(", backend = \"fake\"", ""));
    let out = b.run(&["--json", "do", "la", "grab"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(b.fake.driver_lists(), 0);
    assert_eq!(b.fake.captures().len(), 1);

    // It lacks the driver: the run says so, like resolution would.
    b.fake.drivers("other Other driver");
    let out = b.run(&["--json", "do", "la", "grab"]);
    assert_eq!(out.code_str(), "config_invalid", "{}", out.stdout);
    assert!(out.message().contains("`demo`"), "{}", out.message());
    assert_eq!(files(&b.captures()).len(), 1, "only the first capture");

    // Nothing at all: no backend configured, nothing on PATH.
    let none = SigrokBench::new("[providers.fake]\nkind = \"mock\"\n");
    std::fs::write(none.path("la.lua"), LA_LUA).unwrap();
    none.config(&format!(
        "[providers.fake]\nkind = \"mock\"\n{}",
        LA.replace(", backend = \"fake\"", "")
    ));
    let out = none.run(&["--json", "do", "la", "grab"]);
    assert_eq!(out.code_str(), "provider_unreachable", "{}", out.stdout);
    assert!(
        out.json()["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("sigrok.org")
    );
}

#[test]
fn driver_lists_are_asked_once_per_backend_while_the_daemon_runs() {
    let (b, stock, vendor) = resolving_bench("vendor-la");
    let mut daemon = b
        .cmd(&["serve", "--idle", "20s"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for_socket(&b);
    for _ in 0..3 {
        let out = b.run(&["--json", "do", "la", "grab"]);
        assert_eq!(out.code, 0, "{}", out.stdout);
    }
    assert_eq!(vendor.captures().len(), 3);
    assert_eq!(stock.driver_lists(), 1);
    assert_eq!(vendor.driver_lists(), 1);
    let _ = daemon.kill();
    let _ = daemon.wait();
}

fn wait_for_socket(b: &SigrokBench) {
    wait_until("the daemon's socket", || {
        std::fs::read_dir(b.path("run")).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|e| e.path().extension().is_some_and(|x| x == "sock"))
        })
    });
}

// ---------------------------------------------------------------------------------------------
// ctx.sigrok.acquire

#[test]
fn a_capture_runs_the_exact_command_line_and_reports_the_file() {
    let b = la_bench();
    let out = b.run(&["--json", "do", "la", "grab", "samples=1234"]);
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    let calls = b.fake.captures();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    let tmp = output_of(call);
    assert_eq!(
        call,
        &[
            "-d",
            "demo",
            "--samples",
            "1234",
            "-O",
            "srzip",
            "-o",
            &tmp.display().to_string()
        ]
    );
    // The temporary file is in a private folder in the destination folder.
    let folder = tmp.parent().unwrap();
    assert_eq!(folder.parent().unwrap(), b.captures().join("la"));
    assert!(
        folder
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(".partial"),
        "{tmp:?}"
    );
    assert!(!folder.exists(), "the private folder is gone");

    let d = out.data();
    let file = PathBuf::from(d["file"].as_str().unwrap());
    assert_eq!(file.parent().unwrap(), b.captures().join("la"));
    assert!(file.to_string_lossy().ends_with(".sr"));
    assert_eq!(d["bytes"], 1234);
    assert_eq!(d["format"], "srzip");
    assert_eq!(
        d["meta"],
        json!({ "bytes": 1234, "driver": "demo" }),
        "conn is nil, so it isn't there"
    );
    let data = std::fs::read(&file).unwrap();
    assert_eq!(data.len(), 1234);
    assert!(data.iter().enumerate().all(|(i, b)| *b == (i % 251) as u8));
    assert_eq!(files(&b.captures()).len(), 1);
}

#[test]
fn every_part_of_the_request_becomes_one_argument() {
    let b = la_bench();
    let out = b.run(&["--json", "do", "la", "grab", "mode=full"]);
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    let call = &b.fake.captures()[0];
    let tmp = output_of(call).display().to_string();
    assert_eq!(
        call,
        &[
            "-d",
            "demo",
            "--config",
            "averaging=false:captureratio=20:mode=fast:samplerate=10000000:voltage=1.5",
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
            &tmp
        ]
    );
    let out = b.run(&["--json", "do", "la", "grab", "mode=plain"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let call = &b.fake.captures()[1];
    assert_eq!(call[2..4], ["--channels", "D0,D1"]);
}

#[test]
fn hostile_or_broken_requests_never_reach_sigrok() {
    let b = la_bench();
    for case in [
        "unknown_key",
        "program",
        "config_key",
        "config_dash",
        "config_colon",
        "config_equals",
        "config_lead",
        "config_space",
        "config_ctrl",
        "config_table",
        "config_nan",
        "channel_dash",
        "channel_path",
        "channel_empty",
        "no_length",
        "two_lengths",
        "zero_samples",
        "frac_samples",
        "bad_time",
        "trigger_colon",
        "trigger_lead",
        "wait_alone",
        "format_opts",
        "format_path",
        "no_timeout",
        "zero_timeout",
        "no_into",
        "into_string",
    ] {
        let arg = format!("case={case}");
        let out = b.run(&["--json", "do", "la", "grab", &arg]);
        assert_eq!(out.code_str(), "script_failed", "{case}: {}", out.stdout);
        assert!(
            out.message().contains("ctx.sigrok.acquire"),
            "{case}: {}",
            out.message()
        );
    }
    assert!(b.fake.calls().is_empty(), "{:?}", b.fake.calls());
    assert!(nothing_left(&b.captures()));
}

#[test]
fn ctx_sigrok_needs_a_sigrok_binding() {
    let b = la_bench();
    b.config(&LA.replace("sigrok  = { driver = \"demo\", backend = \"fake\" }\n", ""));
    let out = b.run(&["--json", "do", "la", "grab"]);
    assert_eq!(out.code_str(), "config_invalid", "{}", out.stdout);
    assert!(
        out.message().contains("no `sigrok = {"),
        "{}",
        out.message()
    );
}

#[test]
fn a_busy_device_and_a_missing_one_are_unavailable() {
    let b = la_bench();
    b.fake.behaviour("acquire=busy");
    let out = b.run(&["--json", "do", "la", "grab"]);
    assert_eq!(out.code_str(), "endpoint_unavailable", "{}", out.stdout);
    assert!(out.message().contains("busy"), "{}", out.message());
    assert!(out.message().contains("Unable to claim USB interface"));
    assert!(nothing_left(&b.captures()));

    b.fake.behaviour("acquire=ok").devices("");
    let out = b.run(&["--json", "do", "la", "grab"]);
    assert_eq!(out.code_str(), "endpoint_unavailable", "{}", out.stdout);
    assert!(
        out.message().contains("found no such device"),
        "{}",
        out.message()
    );
    assert!(nothing_left(&b.captures()));
}

#[test]
fn a_missing_program_says_how_to_get_one() {
    let b = la_bench();
    b.config(&format!(
        "[providers.fake]\nkind = \"sigrok\"\nprogram = \"/nonexistent/sigrok-cli\"\n{LA}"
    ));
    let out = b.run(&["--json", "do", "la", "grab"]);
    assert_eq!(out.code_str(), "provider_unreachable", "{}", out.stdout);
    assert!(out.message().contains("/nonexistent/sigrok-cli"));
    assert!(
        out.json()["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("program")
    );
    assert!(nothing_left(&b.captures()));
}

#[test]
fn sigroks_own_message_explains_other_failures() {
    let b = la_bench();
    b.fake
        .behaviour("acquire=fail\nmessage=unknown channel 'Z9'.\nexit=1\nnoisy=1");
    let out = b.run(&["--json", "do", "la", "grab"]);
    assert_eq!(out.code_str(), "script_failed", "{}", out.stdout);
    let msg = out.message();
    assert!(msg.contains("unknown channel 'Z9'."), "{msg}");
    assert!(msg.contains("exit 1"), "{msg}");
    assert!(
        !msg.contains("SyntaxWarning"),
        "decoder noise is dropped: {msg}"
    );
    assert!(nothing_left(&b.captures()));
}

#[test]
fn decoder_warnings_on_stderr_are_not_a_failure() {
    let b = la_bench();
    b.fake.behaviour("noisy=1\nstray=1");
    let out = b.run(&["--json", "do", "la", "grab"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.data()["bytes"], 1000);
    assert_eq!(files(&b.captures()).len(), 1, "{:?}", files(&b.captures()));
}

#[test]
fn stdin_stays_open_for_the_whole_capture() {
    // The fake fails as soon as its stdin ends, and checks it all through a slow capture.
    let b = la_bench();
    b.fake.behaviour("bytes=4000\nrate=10000");
    let out = b.run(&["--json", "do", "la", "grab", "mode=slow"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.data()["bytes"], 4000);
}

#[test]
fn a_timeout_mid_capture_stops_sigrok_and_leaves_nothing() {
    let b = la_bench();
    b.fake.behaviour("bytes=100000\nrate=10000\nstray=1");
    let started = Instant::now();
    let out = b.run(&["--json", "do", "la", "grab", "timeout=400ms"]);
    assert_eq!(out.code_str(), "timeout", "{}", out.stdout);
    assert!(out.message().contains("`la`"), "{}", out.message());
    assert!(started.elapsed() < Duration::from_secs(5));
    // The capture had begun (the stray file was there), and nothing of it is left.
    assert_eq!(b.fake.captures().len(), 1);
    assert!(nothing_left(&b.captures()), "{:?}", files(&b.captures()));
    assert!(device_free(&b.fake), "sigrok was stopped");
}

#[test]
fn the_size_cap_stops_sigrok_midway() {
    let b = la_bench();
    b.fake.behaviour("bytes=100000\nrate=20000\nstray=1");
    let out = b.run(&["--json", "do", "la", "grab", "--max-size", "3000"]);
    assert_eq!(out.code_str(), "limit_exceeded", "{}", out.stdout);
    assert!(nothing_left(&b.captures()), "{:?}", files(&b.captures()));
    assert!(device_free(&b.fake));
}

#[test]
fn ctrl_c_stops_sigrok_and_removes_the_partial_files() {
    let b = la_bench();
    b.fake.behaviour("bytes=200000\nrate=20000\nstray=1");
    let mut child = b
        .cmd(&["--json", "do", "la", "grab"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let dir = b.captures().join("la");
    wait_until("the stray file", || {
        files(&dir).iter().any(|f| f.ends_with(".xyz.part"))
    });
    let ok = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(ok.success());
    let status = child.wait().unwrap();
    let mut stdout = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take().unwrap(), &mut stdout).unwrap();
    assert_eq!(status.code(), Some(130), "{stdout}");
    let v: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["error"]["code"], "cancelled");
    wait_until("sigrok to stop", || device_free(&b.fake));
    assert!(
        !dir.exists(),
        "the empty device folder is gone too: {:?}",
        files(&dir)
    );
}

#[test]
fn a_client_that_leaves_the_daemon_stops_sigrok() {
    let b = la_bench();
    b.fake.behaviour("bytes=200000\nrate=20000\nstray=1");
    let mut daemon = b
        .cmd(&["serve", "--idle", "20s"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for_socket(&b);
    let mut client = b
        .cmd(&["--json", "do", "la", "grab"])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let dir = b.captures().join("la");
    wait_until("the capture", || {
        files(&dir).iter().any(|f| f.ends_with(".xyz.part"))
    });
    client.kill().unwrap();
    let _ = client.wait();
    wait_until("the partial files to go", || nothing_left(&dir));
    wait_until("sigrok to stop", || device_free(&b.fake));

    // The device is free again, through the daemon.
    b.fake.behaviour("bytes=10");
    let out = b.run(&["--json", "do", "la", "grab"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let _ = daemon.kill();
    let _ = daemon.wait();
}

#[test]
fn one_capture_per_device_at_a_time() {
    // Without the daemon two processes meet at the device, which refuses the second.
    let b = la_bench();
    b.fake.behaviour("bytes=8000\nrate=20000");
    let mut first = b
        .cmd(&["--json", "do", "la", "grab"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    wait_until("the first capture", || !device_free(&b.fake));
    let second = b.run(&["--json", "do", "la", "grab"]);
    assert_eq!(
        second.code_str(),
        "endpoint_unavailable",
        "{}",
        second.stdout
    );
    assert!(first.wait().unwrap().success());

    // With the daemon, the second command waits for the device and then runs.
    let mut daemon = b
        .cmd(&["serve", "--idle", "20s"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for_socket(&b);
    let before = b.fake.captures().len();
    let mut first = b
        .cmd(&["--json", "do", "la", "grab"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    wait_until("the first capture", || !device_free(&b.fake));
    let second = b.run(&["--json", "do", "la", "grab"]);
    assert_eq!(second.code, 0, "{}", second.stdout);
    let mut out = String::new();
    std::io::Read::read_to_string(&mut first.stdout.take().unwrap(), &mut out).unwrap();
    assert!(first.wait().unwrap().success(), "{out}");
    assert_eq!(b.fake.captures().len(), before + 2);
    assert_eq!(files(&b.captures().join("la")).len(), 3);
    let _ = daemon.kill();
    let _ = daemon.wait();
}

#[test]
fn a_dry_run_runs_nothing() {
    let b = la_bench();
    let out = b.run(&["--json", "do", "la", "grab", "--dry-run"]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert!(b.fake.calls().is_empty());
    assert!(nothing_left(&b.captures()));
}

// ---------------------------------------------------------------------------------------------
// The real thing, where it is installed

/// `sigrok-cli` on this machine's PATH, if any.
fn real_sigrok() -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|d| d.join("sigrok-cli"))
        .find(|p| p.is_file())
}

#[test]
fn the_example_captures_from_the_real_demo_driver() {
    let Some(real) = real_sigrok() else {
        eprintln!("skipped: sigrok-cli is not installed");
        return;
    };
    let dir = tempfile::TempDir::new().unwrap();
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/sigrok/desk.toml");
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_desk"))
            .env_remove("DESK_CONFIG")
            .env("PATH", real.parent().unwrap())
            .env("DESK_RUNTIME_DIR", dir.path().join("run"))
            .env("DESK_CAPTURE_DIR", dir.path().join("captures"))
            .arg("--config")
            .arg(&example)
            .args(args)
            .output()
            .unwrap();
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(
            v["ok"],
            true,
            "{v}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        v["data"].clone()
    };
    let sr = run(&["--json", "do", "la", "capture", "duration=100ms"]);
    assert_eq!(sr["format"], "srzip");
    let file = std::fs::read(sr["file"].as_str().unwrap()).unwrap();
    assert!(file.starts_with(b"PK"), "an srzip file is a zip archive");
    assert_eq!(sr["bytes"], file.len());

    let bin = run(&[
        "--json",
        "do",
        "la",
        "capture",
        "duration=200ms",
        "samplerate=100khz",
        "channels=D0,D1",
        "format=binary",
    ]);
    // One byte per sample for up to 8 logic channels.
    assert_eq!(bin["bytes"], 20000);
    assert_eq!(bin["meta"]["channels"], "D0,D1");
    let leftovers: Vec<String> = files(&dir.path().join("captures"))
        .into_iter()
        .filter(|f| f.contains(".partial") || f.ends_with(".part"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}
