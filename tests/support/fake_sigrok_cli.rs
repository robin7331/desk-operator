//! A fake `sigrok-cli` for tests (a Cargo example: `cargo test` builds it). std only.
//!
//! It reads its scenario from the folder in `$FAKE_SIGROK_DIR`, or from the subfolder named like
//! this program's file (without extension) when that exists, so several copies of it can be
//! different backends. Every call appends its arguments to `log` there: one line per call, the
//! arguments separated by U+001F.
//!
//! Scenario files (all optional):
//! * `version`: the first line of `--version` (default `sigrok-cli 0.7.2-fake`)
//! * `drivers`: `<id> <description>` per line, for `-L` (default: `demo`)
//! * `devices`: what `--scan` prints per device, e.g. `demo - Demo device with 13 channels: D0 D1`
//!   or `fx2lafw:conn=1.5 - …` (default: none)
//! * `show`: the text of `--show`
//! * `behaviour`: `key=value` lines:
//!   * `acquire=ok|busy|fail` (default ok); `message=…` and `exit=N` for `fail`
//!   * `bytes=N`: how much a capture writes (default: `--samples`, else 1000)
//!   * `rate=N`: write N bytes per second, in 50 ms steps (a slow capture)
//!   * `noisy=1`: decoder warnings on stderr, also on success
//!   * `stray=1`: also create `<path>.xyz.part` next to the output, like srzip does
//!
//! Like real hardware, one device serves one capture at a time (`busy.lock`, held while it
//! captures): a second one fails with "Unable to claim USB interface". While capturing it watches
//! stdin: if it reaches end of file, it fails (desk must keep it open).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::exit;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

fn read(dir: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(name)).ok()
}

fn scenario_dir() -> PathBuf {
    let Some(base) = std::env::var_os("FAKE_SIGROK_DIR") else {
        eprintln!("fake sigrok-cli: FAKE_SIGROK_DIR is not set");
        exit(70);
    };
    let base = PathBuf::from(base);
    let own = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()));
    match own.map(|n| base.join(n)) {
        Some(sub) if sub.is_dir() => sub,
        _ => base,
    }
}

fn die(code: i32, msg: &str) -> ! {
    eprintln!("{msg}");
    exit(code);
}

fn noise(on: bool) {
    if on {
        eprintln!(
            "/usr/share/libsigrokdecode/decoders/x/pd.py:215: SyntaxWarning: \"\\s\" is an invalid escape sequence."
        );
        eprintln!("  instpat = re.compile('\\s*([0-9a-fA-F]+)')");
    }
}

struct Device {
    driver: String,
    conn: Option<String>,
    line: String,
    channels: Vec<String>,
}

fn devices(dir: &Path) -> Vec<Device> {
    read(dir, "devices")
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let id = l.split(" - ").next().unwrap_or("").trim();
            let (driver, rest) = id.split_once(':').unwrap_or((id, ""));
            let channels = l
                .rsplit_once(": ")
                .map(|(_, c)| c.split_whitespace().map(String::from).collect())
                .unwrap_or_default();
            Device {
                driver: driver.to_string(),
                conn: rest.strip_prefix("conn=").map(String::from),
                line: l.to_string(),
                channels,
            }
        })
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = scenario_dir();
    if let Ok(mut log) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("log"))
    {
        let _ = writeln!(log, "{}", args.join("\u{1f}"));
    }
    let behaviour: HashMap<String, String> = read(&dir, "behaviour")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let b = Behaviour(behaviour);
    noise(b.get("noisy") == Some("1"));

    let drivers: Vec<(String, String)> = read(&dir, "drivers")
        .unwrap_or_else(|| "demo Demo driver and pattern generator\n".into())
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let (id, d) = l.trim().split_once(' ').unwrap_or((l.trim(), ""));
            (id.to_string(), d.trim().to_string())
        })
        .collect();

    let mut device_arg = None;
    let mut opts: HashMap<&str, String> = HashMap::new();
    let mut flags: Vec<&str> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--version" | "-L" | "--scan" | "--show" | "--wait-trigger" => flags.push(a),
            "-d" | "--config" | "--channels" | "--triggers" | "--time" | "--samples"
            | "--frames" | "-O" | "-o" => {
                let Some(v) = it.next() else {
                    die(2, &format!("fake sigrok-cli: {a} needs a value"));
                };
                if *a == "-d" {
                    device_arg = Some(v.clone());
                } else {
                    let key = match a.as_str() {
                        "--config" => "config",
                        "--channels" => "channels",
                        "--triggers" => "triggers",
                        "--time" => "time",
                        "--samples" => "samples",
                        "--frames" => "frames",
                        "-O" => "format",
                        _ => "output",
                    };
                    opts.insert(key, v.clone());
                }
            }
            other => die(2, &format!("fake sigrok-cli: unknown argument `{other}`")),
        }
    }

    if flags.contains(&"--version") {
        let v = read(&dir, "version").unwrap_or_else(|| "sigrok-cli 0.7.2-fake".into());
        println!(
            "{}\n\nLibraries and features:\n- libsigrok 0.5.2-fake",
            v.trim()
        );
        exit(0);
    }
    if flags.contains(&"-L") {
        println!("sigrok-cli 0.7.2-fake\n\nSupported hardware drivers:");
        for (id, d) in &drivers {
            println!("  {id:<20} {d}");
        }
        println!("\nSupported input formats:\n  binary               Raw binary logic data");
        exit(0);
    }

    let all = devices(&dir);
    let (driver, conn) = match &device_arg {
        Some(d) => {
            let (drv, rest) = d.split_once(':').unwrap_or((d, ""));
            if !drivers.iter().any(|(id, _)| id == drv) {
                die(1, &format!("Driver {drv} not found."));
            }
            (
                Some(drv.to_string()),
                rest.strip_prefix("conn=").map(String::from),
            )
        }
        None => (None, None),
    };
    let matching: Vec<&Device> = all
        .iter()
        .filter(|dev| driver.as_ref().is_none_or(|d| *d == dev.driver))
        .filter(|dev| conn.is_none() || dev.conn.is_none() || dev.conn == conn)
        .collect();

    if flags.contains(&"--scan") {
        if !matching.is_empty() {
            println!("The following devices were found:");
            for dev in &matching {
                println!("{}", dev.line);
            }
        }
        exit(0);
    }
    if driver.is_none() {
        die(1, "fake sigrok-cli: no -d given");
    }
    let Some(device) = matching.first() else {
        die(1, "No devices found.");
    };
    if flags.contains(&"--show") {
        let text = read(&dir, "show").unwrap_or_else(|| {
            format!(
                "Driver functions:\n    Logic analyzer\n{}\nSupported configuration options:\n    samplerate (1 Hz - 1 GHz in steps of 1 Hz)\n",
                device.line
            )
        });
        print!("{text}");
        exit(0);
    }
    acquire(&dir, &b, device, &opts, &flags);
}

struct Behaviour(HashMap<String, String>);

impl Behaviour {
    fn get(&self, k: &str) -> Option<&str> {
        self.0.get(k).map(String::as_str)
    }
}

fn acquire(
    dir: &Path,
    b: &Behaviour,
    device: &Device,
    opts: &HashMap<&str, String>,
    flags: &[&str],
) {
    let Some(out) = opts.get("output") else {
        die(1, "fake sigrok-cli: nothing to do (no -o)");
    };
    let format = opts.get("format").map_or("srzip", String::as_str);
    let known = [
        "srzip", "binary", "csv", "vcd", "bits", "hex", "ascii", "analog",
    ];
    if !known.contains(&format) {
        die(1, &format!("Unknown output module '{format}'."));
    }
    if let Some(chs) = opts.get("channels") {
        for c in chs.split(',') {
            if !c.contains('-') && !device.channels.iter().any(|d| d == c) {
                die(1, &format!("unknown channel '{c}'."));
            }
        }
    }
    if flags.contains(&"--wait-trigger") && !opts.contains_key("triggers") {
        die(1, "fake sigrok-cli: --wait-trigger without --triggers");
    }
    let lengths = ["time", "samples", "frames"]
        .iter()
        .filter(|k| opts.contains_key(*k))
        .count();
    if lengths != 1 {
        die(
            1,
            "fake sigrok-cli: give one of --time, --samples, --frames",
        );
    }
    match b.get("acquire") {
        Some("busy") => die(
            1,
            "sr: fake: Unable to claim USB interface. Another program or driver has already claimed it.",
        ),
        Some("fail") => {
            let code = b.get("exit").and_then(|e| e.parse().ok()).unwrap_or(1);
            die(code, b.get("message").unwrap_or("fake failure"));
        }
        _ => {}
    }
    // One capture at a time per device, like real hardware.
    let lock = match std::fs::File::create(dir.join("busy.lock")) {
        Ok(f) => f,
        Err(e) => die(70, &format!("fake sigrok-cli: busy.lock: {e}")),
    };
    if lock.try_lock().is_err() {
        die(
            1,
            "sr: fake: Unable to claim USB interface. Another program or driver has already claimed it.",
        );
    }

    // desk must keep stdin open until the program exits.
    let eof = Arc::new(AtomicBool::new(false));
    let seen = eof.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 256];
        let mut stdin = std::io::stdin();
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => {
                    seen.store(true, Ordering::SeqCst);
                    return;
                }
                Ok(_) => {}
            }
        }
    });
    std::thread::sleep(Duration::from_millis(50));
    let check_stdin = || {
        if eof.load(Ordering::SeqCst) {
            die(
                9,
                "fake sigrok-cli: stdin reached end of file, so the capture stopped",
            );
        }
    };
    check_stdin();

    let bytes: u64 = b
        .get("bytes")
        .and_then(|v| v.parse().ok())
        .or_else(|| opts.get("samples").and_then(|s| s.parse().ok()))
        .unwrap_or(1000);
    let rate: Option<u64> = b.get("rate").and_then(|v| v.parse().ok());
    let path = PathBuf::from(out);
    if b.get("stray") == Some("1") {
        let mut stray = path.clone().into_os_string();
        stray.push(".xyz.part");
        let _ = std::fs::write(&stray, b"partial");
    }
    let mut f = match std::fs::File::create(&path) {
        Ok(f) => f,
        Err(e) => die(1, &format!("Error opening output file {out}: {e}")),
    };
    let step = Duration::from_millis(50);
    let chunk = rate.map_or(bytes, |r| (r / 20).max(1));
    let mut at = 0u64;
    while at < bytes {
        let n = chunk.min(bytes - at);
        let data: Vec<u8> = (at..at + n).map(|i| (i % 251) as u8).collect();
        if f.write_all(&data).and_then(|()| f.flush()).is_err() {
            die(1, "fake sigrok-cli: write failed");
        }
        at += n;
        if rate.is_some() && at < bytes {
            std::thread::sleep(step);
            check_stdin();
        }
    }
    if b.get("stray") == Some("1") {
        let mut stray = path.into_os_string();
        stray.push(".xyz.part");
        let _ = std::fs::remove_file(&stray);
    }
    check_stdin();
    drop(lock);
    exit(0);
}
