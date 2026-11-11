//! A starting point for a program that analyzes a desk stream live: it runs
//! `desk stream <args…> --frames` as a subprocess, reads the header line and the frames, checks
//! that `seq` and `offset` line up, counts the edges per channel of a `logic` stream, and prints
//! one line per second (and a note whenever blocks were dropped because it read too slowly).
//!
//! ```sh
//! cargo run --release --example stream_edges -- --config examples/sigrok/desk.toml la samples
//! cargo run --release --example stream_edges -- la samples samplerate=10MHz channels=D0,D1
//! ```
//!
//! It uses only the standard library, so it parses the header line by hand; a real program would
//! use a JSON parser. The frame layout is in `docs/streaming.md` ("Binary streams"). desk is found
//! through `$DESK`, next to this example's build (`target/<profile>/desk`), or on PATH.

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MAGIC: u32 = 0x4B53_4544; // "DESK"

struct Frame {
    end: bool,
    length: usize,
    seq: u64,
    offset: u64,
    dropped: u32,
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn le64(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[..8]);
    u64::from_le_bytes(a)
}

fn frame(h: &[u8; 32]) -> Result<Frame, String> {
    if le32(&h[0..4]) != MAGIC {
        return Err("not a desk frame (wrong magic)".into());
    }
    Ok(Frame {
        end: h[4] == 1,
        length: le32(&h[8..12]) as usize,
        seq: le64(&h[12..20]),
        offset: le64(&h[20..28]),
        dropped: le32(&h[28..32]),
    })
}

/// The number after `"key":` in a JSON line (good enough for desk's header).
fn number(json: &str, key: &str) -> Option<f64> {
    let at = json.find(&format!("\"{key}\":"))? + key.len() + 3;
    let rest = &json[at..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-')))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// The text after `"key":"`.
fn text(json: &str, key: &str) -> Option<String> {
    let at = json.find(&format!("\"{key}\":\""))? + key.len() + 4;
    let rest = &json[at..];
    Some(rest[..rest.find('"')?].to_string())
}

/// The strings of the array after `"key":[`.
fn strings(json: &str, key: &str) -> Vec<String> {
    let Some(at) = json.find(&format!("\"{key}\":[")) else {
        return Vec::new();
    };
    let rest = &json[at + key.len() + 4..];
    let Some(end) = rest.find(']') else {
        return Vec::new();
    };
    rest[..end]
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn desk() -> PathBuf {
    if let Some(p) = std::env::var_os("DESK") {
        return PathBuf::from(p);
    }
    let sibling = std::env::current_exe().ok().and_then(|e| {
        let dir = e.parent()?.parent()?; // target/<profile>/examples -> target/<profile>
        let p = dir.join(format!("desk{}", std::env::consts::EXE_SUFFIX));
        p.is_file().then_some(p)
    });
    sibling.unwrap_or_else(|| PathBuf::from("desk"))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: stream_edges [--config <desk.toml>] <device> <stream> [arg=value…]");
        std::process::exit(2);
    }
    let mut child = Command::new(desk())
        .arg("stream")
        .args(&args)
        .arg("--frames")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| {
            eprintln!("cannot run desk: {e}");
            std::process::exit(1);
        });
    let Some(stdout) = child.stdout.take() else {
        std::process::exit(1);
    };
    let mut input = BufReader::with_capacity(1 << 20, stdout);

    // The header line: what the bytes are.
    let mut header = String::new();
    if input.read_line(&mut header).unwrap_or(0) == 0 {
        // desk printed why on stderr (a usage error, a busy device, …).
        let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(1);
        std::process::exit(code);
    }
    let unit = number(&header, "unit_size").unwrap_or(1.0) as usize;
    let content = text(&header, "content").unwrap_or_default();
    let mut channels = strings(&header, "channels");
    if content != "logic" || channels.is_empty() {
        eprintln!("not a logic stream ({content}): only counting bytes");
        channels.clear();
    }
    let rate = number(&header, "samplerate").unwrap_or(0.0);
    eprintln!(
        "{content}: {} channel(s) {}, {unit} byte(s) per sample, {rate} samples/s",
        channels.len(),
        channels.join(" ")
    );

    let mut edges = vec![0u64; channels.len()];
    let mut prev: Option<u64> = None;
    let (mut next_seq, mut next_offset, mut dropped) = (0u64, 0u64, 0u32);
    let mut payload = Vec::new();
    let mut bytes = 0u64;
    let start = Instant::now();
    let mut report = start + Duration::from_secs(1);
    loop {
        let mut h = [0u8; 32];
        if input.read_exact(&mut h).is_err() {
            eprintln!("the stream broke off");
            break;
        }
        let f = match frame(&h) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("{e}");
                break;
            }
        };
        payload.resize(f.length, 0);
        if input.read_exact(&mut payload).is_err() {
            eprintln!("the stream broke off");
            break;
        }
        if f.end {
            // A JSON envelope: { ok, data } or { ok: false, error }.
            println!("end: {}", String::from_utf8_lossy(&payload));
            break;
        }
        // Lost blocks show in `dropped` and as a jump in `seq` and `offset`.
        if f.dropped != dropped {
            println!(
                "note: {} block(s) dropped (this program reads too slowly); samples {}..{} are missing",
                f.dropped - dropped,
                next_offset,
                f.offset
            );
            dropped = f.dropped;
            prev = None; // no edge across a gap
        } else if f.seq != next_seq || f.offset != next_offset {
            eprintln!(
                "seq/offset out of line: got {}/{}, expected {next_seq}/{next_offset}",
                f.seq, f.offset
            );
        }
        next_seq = f.seq + 1;
        next_offset = f.offset + (f.length / unit) as u64;
        bytes += f.length as u64;

        if !channels.is_empty() {
            for sample in payload.chunks_exact(unit) {
                let mut v = 0u64;
                for (i, b) in sample.iter().enumerate() {
                    v |= u64::from(*b) << (8 * i);
                }
                if let Some(p) = prev {
                    let mut changed = (p ^ v) & ((1u64 << channels.len().min(63)) - 1);
                    while changed != 0 {
                        edges[changed.trailing_zeros() as usize] += 1;
                        changed &= changed - 1;
                    }
                }
                prev = Some(v);
            }
        }
        if Instant::now() >= report {
            let t = start.elapsed().as_secs_f64();
            let counts: Vec<String> = channels
                .iter()
                .zip(&edges)
                .map(|(c, n)| format!("{c}:{n}"))
                .collect();
            println!(
                "{t:6.1} s  {:8.1} MB/s  edges in the last second: {}",
                bytes as f64 / 1e6 / t,
                counts.join(" ")
            );
            edges.iter_mut().for_each(|n| *n = 0);
            report += Duration::from_secs(1);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}
