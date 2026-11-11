//! Sipeed's SLogic logic analyzers (`models/sipeed/shared/slogic.lua`) against the fake
//! `sigrok-cli`: the exact command lines, and that nothing the vendor driver would silently
//! change (a rate it doesn't have, one above the channel mode's limit) ever reaches sigrok.

#[path = "support/sigrok.rs"]
mod sigrok;

use serde_json::json;
use sigrok::SigrokBench;

const DRIVER: &str = "sipeed-slogic-analyzer Sipeed SLogic Analyzer";
/// What Sipeed's sigrok-cli 0.8.0 prints for `--scan` (the S/N is the firmware version).
const SCAN: &str = "sipeed-slogic-analyzer - (10G) SLogic32 U3 [S/N: 202610300413] with 32 channels: \
    D0 D1 D2 D3 D4 D5 D6 D7 D8 D9 D10 D11 D12 D13 D14 D15 D16 D17 D18 D19 D20 D21 D22 D23 D24 \
    D25 D26 D27 D28 D29 D30 D31";

fn bench() -> SigrokBench {
    let b = SigrokBench::new(
        "[devices.la]\nmodel = \"sipeed/slogic32u3\"\ncapture = { sigrok = { backend = \"fake\" } }\n",
    );
    b.fake.drivers(DRIVER).devices(SCAN);
    b
}

/// A capture's command line without the path of its temporary file.
fn line(call: &[String]) -> Vec<&str> {
    let (path, rest) = call.split_last().unwrap();
    assert!(path.ends_with(".sr") || path.contains(".partial"), "{path}");
    rest.iter().map(String::as_str).collect()
}

#[test]
fn a_plain_capture_takes_all_channels_at_the_defaults() {
    let b = bench();
    let out = b.run(&["--json", "do", "la", "capture", "duration=100ms"]);
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    let calls = b.fake.captures();
    assert_eq!(calls.len(), 1);
    // `logic_channels` before `samplerate`: the vendor driver needs that order.
    assert_eq!(
        line(&calls[0]),
        [
            "-d",
            "sipeed-slogic-analyzer",
            "--config",
            "logic_channels=32:pattern=Normal:samplerate=10000000:voltage_threshold=1.7-1.7",
            "--time",
            "100",
            "-O",
            "srzip",
            "-o",
        ]
    );
    let d = out.data();
    assert!(d["file"].as_str().unwrap().ends_with(".sr"), "{d}");
    assert_eq!(d["format"], "srzip");
    assert_eq!(
        d["meta"],
        json!({
            "bytes": 1000,
            "channels": 32,
            "duration": 0.1,
            "pattern": "normal",
            "samplerate": 10000000.0,
            "threshold": 1.7,
        })
    );
}

#[test]
fn every_argument_reaches_sigrok() {
    let b = bench();
    let out = b.run(&[
        "--json",
        "do",
        "la",
        "capture",
        "duration=2.5ms",
        "samplerate=800MHz",
        "channels=8",
        "threshold=0.9v",
        "pattern=emulation",
        "format=binary",
        "trigger=D0=r,D1=1",
    ]);
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    assert_eq!(
        line(&b.fake.captures()[0]),
        [
            "-d",
            "sipeed-slogic-analyzer",
            "--config",
            "logic_channels=8:pattern=Emulation:samplerate=800000000:voltage_threshold=0.9-0.9",
            "--triggers",
            "D0=r,D1=1",
            "--wait-trigger",
            "--time",
            "3",
            "-O",
            "binary",
            "-o",
        ]
    );
    let d = out.data();
    assert!(d["file"].as_str().unwrap().ends_with(".bin"), "{d}");
    // Whole milliseconds: 2.5 ms became 3 ms, and the report says so.
    assert_eq!(d["meta"]["duration"], 0.003);
    assert_eq!(d["meta"]["trigger"], "D0=r,D1=1");
    assert_eq!(d["meta"]["channels"], 8);
}

#[test]
fn each_channel_mode_allows_its_highest_rate() {
    let b = bench();
    for (channels, rate, hz) in [
        ("4", "1400MHz", "1400000000"),
        ("8", "800MHz", "800000000"),
        ("16", "400MHz", "400000000"),
        ("32", "200MHz", "200000000"),
    ] {
        let out = b.run(&[
            "--json",
            "do",
            "la",
            "capture",
            "duration=1ms",
            &format!("channels={channels}"),
            &format!("samplerate={rate}"),
        ]);
        assert_eq!(out.code, 0, "{channels} channels at {rate}: {}", out.stdout);
        let calls = b.fake.captures();
        assert_eq!(
            calls.last().unwrap()[3],
            format!(
                "logic_channels={channels}:pattern=Normal:samplerate={hz}:voltage_threshold=1.7-1.7"
            )
        );
    }
}

#[test]
fn what_the_vendor_driver_would_silently_change_never_reaches_sigrok() {
    let b = bench();
    // (arguments, what the message says)
    let refused = [
        // Not one of the analyzer's rates: the vendor driver would capture at 200 MHz.
        (
            vec!["samplerate=12MHz"],
            "no samplerate 12 MHz with 32 channels",
        ),
        // Above the mode's limit: the vendor driver would cut it down to 200 MHz.
        (
            vec!["samplerate=400MHz"],
            "5, 8, 10, 16, 20, 25, 32, 40, 50, 80, 100, 160, 200 MHz",
        ),
        (
            vec!["samplerate=1400MHz", "channels=8"],
            "no samplerate 1400 MHz with 8 channels",
        ),
        (vec!["threshold=1.75v"], "0.1 V steps"),
    ];
    for (args, message) in refused {
        let mut cmd = vec!["--json", "do", "la", "capture", "duration=10ms"];
        cmd.extend(&args);
        let out = b.run(&cmd);
        assert_ne!(out.code, 0, "{args:?}: {}", out.stdout);
        assert_eq!(out.code_str(), "script_failed", "{args:?}: {}", out.stdout);
        assert!(
            out.message().contains(message),
            "{args:?}: {}",
            out.message()
        );
    }
    // Outside what the model declares: desk refuses these before the driver runs.
    for arg in [
        "samplerate=1MHz",
        "samplerate=2GHz",
        "threshold=6.1v",
        "channels=5",
        "pattern=usb",
        "format=wav",
        "duration=0",
    ] {
        let mut cmd = vec!["--json", "do", "la", "capture"];
        if !arg.starts_with("duration") {
            cmd.push("duration=10ms");
        }
        cmd.push(arg);
        let out = b.run(&cmd);
        assert_ne!(out.code, 0, "{arg}: {}", out.stdout);
        assert_ne!(out.code_str(), "script_failed", "{arg}: {}", out.stdout);
    }
    assert!(b.fake.captures().is_empty(), "{:?}", b.fake.captures());
    assert!(sigrok::files(&b.captures()).is_empty());
}

#[test]
fn stock_sigrok_alone_points_at_sipeeds_build() {
    let b = SigrokBench::new("[devices.la]\nmodel = \"sipeed/slogic32u3\"\n");
    let out = b.run(&["--json", "do", "la", "capture", "duration=10ms"]);
    assert_eq!(out.code_str(), "config_invalid", "{}", out.stdout);
    let text = out.stdout;
    assert!(text.contains("sipeed-slogic-analyzer"), "{text}");
    assert!(
        text.contains("https://github.com/sipeed/SLogic/releases"),
        "{text}"
    );
    assert!(sigrok::files(&b.captures()).is_empty());
}

#[test]
fn sigroks_own_refusal_is_the_message() {
    let b = bench();
    b.fake
        .behaviour("acquire=fail\nmessage=Invalid channel 'D9'.");
    let out = b.run(&[
        "--json",
        "do",
        "la",
        "capture",
        "duration=10ms",
        "channels=8",
        "trigger=D9=r",
    ]);
    assert_eq!(out.code_str(), "script_failed", "{}", out.stdout);
    assert!(out.message().contains("Invalid channel 'D9'."));
    assert!(sigrok::files(&b.captures()).is_empty());
}

// ---------------------------------------------------------------------------------------------
// The `samples` stream (`desk stream la samples`). Unix only, like tests/sigrok_streams.rs.

/// `desk stream la samples <args> --header --discard --bytes 64`: the header and the summary.
#[cfg(unix)]
fn stream(b: &SigrokBench, args: &[&str]) -> (sigrok::Out, Vec<serde_json::Value>) {
    let mut cmd = vec!["--json", "stream", "la", "samples"];
    cmd.extend(args);
    cmd.extend(["--header", "--discard", "--bytes", "64"]);
    let out = b.run(&cmd);
    let lines = out
        .stdout
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    (out, lines)
}

#[cfg(unix)]
#[test]
fn a_stream_runs_sigrok_continuously_into_its_stdout() {
    let b = bench();
    let (out, lines) = stream(&b, &[]);
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    // The capture's config keys, in the same order, then binary output on stdout.
    assert_eq!(
        b.fake.streams(),
        [[
            "-d",
            "sipeed-slogic-analyzer",
            "--config",
            "logic_channels=32:pattern=Normal:samplerate=10000000:voltage_threshold=1.7-1.7",
            "--continuous",
            "-O",
            "binary",
        ]]
    );
    let h = &lines[0]["data"];
    assert_eq!(h["content"], "logic");
    assert_eq!(h["meta"], json!({ "pattern": "normal", "threshold": 1.7 }));
    assert_eq!(lines[1]["data"]["bytes"], 64);
    assert!(b.fake.captures().is_empty());

    let (out, _) = stream(
        &b,
        &[
            "channels=8",
            "samplerate=800MHz",
            "threshold=0.9v",
            "pattern=emulation",
        ],
    );
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    assert_eq!(
        b.fake.streams()[1][3],
        "logic_channels=8:pattern=Emulation:samplerate=800000000:voltage_threshold=0.9-0.9"
    );
}

#[cfg(unix)]
#[test]
fn each_channel_mode_describes_its_samples() {
    let b = bench();
    for (channels, rate, unit_size) in [
        ("4", "1400MHz", 1),
        ("8", "800MHz", 1),
        ("16", "400MHz", 2),
        ("32", "200MHz", 4),
    ] {
        let (out, lines) = stream(
            &b,
            &[
                &format!("channels={channels}"),
                &format!("samplerate={rate}"),
            ],
        );
        assert_eq!(out.code, 0, "{channels}: {}{}", out.stdout, out.stderr);
        let h = &lines[0]["data"];
        let n: usize = channels.parse().unwrap();
        let names: Vec<String> = (0..n).map(|i| format!("D{i}")).collect();
        assert_eq!(h["channels"], json!(names), "{channels}");
        assert_eq!(h["unit_size"], unit_size, "{channels}");
        assert_eq!(
            h["samplerate"].as_f64().unwrap(),
            rate.trim_end_matches("MHz").parse::<f64>().unwrap() * 1e6
        );
        assert_eq!(lines[1]["data"]["units"], 64 / unit_size, "{channels}");
    }
}

#[cfg(unix)]
#[test]
fn a_stream_the_analyzer_cannot_deliver_never_reaches_sigrok() {
    let b = bench();
    for (args, message) in [
        (
            vec!["samplerate=400MHz"],
            "no samplerate 400 MHz with 32 channels",
        ),
        (
            vec!["samplerate=1400MHz", "channels=8"],
            "5, 8, 10, 16, 20, 25, 32, 40, 50, 80, 100, 160, 200, 400, 800 MHz",
        ),
        (vec!["samplerate=12MHz"], "no samplerate 12 MHz"),
        (vec!["threshold=1.75v"], "0.1 V steps"),
    ] {
        let (out, _) = stream(&b, &args);
        assert_eq!(out.code_str(), "script_failed", "{args:?}: {}", out.stdout);
        assert!(
            out.message().contains(message),
            "{args:?}: {}",
            out.message()
        );
    }
    // Outside what the model declares, or only the capture's.
    for arg in [
        "samplerate=1MHz",
        "channels=5",
        "pattern=usb",
        "duration=1s",
        "format=srzip",
        "trigger=D0=r",
    ] {
        let (out, _) = stream(&b, &[arg]);
        assert_ne!(out.code, 0, "{arg}: {}", out.stdout);
        assert_ne!(out.code_str(), "script_failed", "{arg}: {}", out.stdout);
    }
    assert!(b.fake.streams().is_empty(), "{:?}", b.fake.streams());
}
