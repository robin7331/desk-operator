//! `desk port list`: serial ports with their USB ids, to fill in a binding's `port = { … }`.
//! `desk port probe`: raw bytes in, hex out, for finding out how a new device talks.

use std::time::{Duration, Instant};

use serde::Serialize;

use super::ProbeArgs;
use crate::error::{DeskError, Result};
use crate::output::{Report, table};
use crate::serial::{self, Port, PortSpec, Settings};

/// Only USB ports unless `all`: Linux exposes dozens of legacy `/dev/ttyS*` ports on any machine.
pub fn list(all: bool) -> Result<Report> {
    let mut ports = serial::list()?;
    let total = ports.len();
    if !all {
        ports.retain(|p| p.usb_vid.is_some());
    }
    let hidden = total - ports.len();
    let hex = |v: Option<u16>| v.map_or_else(|| "-".to_string(), |v| format!("{v:04x}"));
    let rows: Vec<Vec<String>> = ports
        .iter()
        .map(|p| {
            vec![
                p.path.clone(),
                format!("{}:{}", hex(p.usb_vid), hex(p.usb_pid)),
                p.serial_number.clone().unwrap_or_else(|| "-".into()),
                [p.manufacturer.as_deref(), p.product.as_deref()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" "),
            ]
        })
        .collect();
    let mut human = if rows.is_empty() {
        "no USB serial ports found (is the device powered and plugged in?)\n".to_string()
    } else {
        table(&["PORT", "USB VID:PID", "SERIAL", "DEVICE"], &rows)
    };
    if let Some(p) = ports.iter().find(|p| p.usb_vid.is_some()) {
        human.push_str(&format!(
            "\nbind one in a script binding with: port = {{ usb_vid = 0x{:04x}, usb_pid = 0x{:04x} }}\n",
            p.usb_vid.unwrap_or_default(),
            p.usb_pid.unwrap_or_default()
        ));
    }
    if hidden > 0 {
        human.push_str(&format!(
            "({hidden} non-USB ports hidden; `--all` shows them)\n"
        ));
    }
    Ok(Report::new(
        &serde_json::json!({ "ports": ports, "hidden": hidden }),
        human,
    ))
}

#[derive(Serialize)]
struct Step {
    /// What was sent, as hex (`null`: only listened)
    sent: Option<String>,
    /// Everything received in this step's listen window, as hex
    received: String,
    /// `received` with non-printable bytes as `.`
    ascii: String,
    /// Bursts of bytes separated by silence, with their arrival time after the send
    chunks: Vec<Chunk>,
}

#[derive(Serialize)]
struct Chunk {
    at_ms: u64,
    hex: String,
}

/// A gap this long ends a chunk (bursts closer together are one chunk).
const GAP: Duration = Duration::from_millis(20);
const MAX_LISTEN: Duration = Duration::from_secs(60);

/// `desk port probe`: open a port, send raw bytes, record what comes back. Nothing here knows a
/// protocol, and nothing is checked against limits: it exists to find out how a device talks.
pub async fn probe(args: ProbeArgs) -> Result<Report> {
    let spec = probe_spec(&args)?;
    let settings = line_settings(args.baud, &args.line)?;
    let listen = humantime::parse_duration(&args.listen)
        .map_err(|e| DeskError::Usage(format!("--listen `{}`: {e}", args.listen)))?;
    if listen > MAX_LISTEN {
        return Err(DeskError::Usage(format!(
            "--listen is at most {}",
            humantime::format_duration(MAX_LISTEN)
        )));
    }
    let sends = args
        .send
        .iter()
        .map(|s| if args.text { unescape(s) } else { parse_hex(s) })
        .collect::<Result<Vec<_>>>()?;

    let port = Port::open(spec, settings).await?;
    let mut steps = Vec::new();
    let plan: Vec<Option<Vec<u8>>> = if sends.is_empty() {
        vec![None]
    } else {
        sends.into_iter().map(Some).collect()
    };
    for bytes in plan {
        let start = Instant::now();
        if let Some(b) = &bytes {
            port.write(b.clone()).await?;
        }
        let mut chunks: Vec<Chunk> = Vec::new();
        let mut all = Vec::new();
        let mut in_burst = false;
        while start.elapsed() < listen {
            let wait = GAP.min(listen.saturating_sub(start.elapsed()));
            let got = port.read(4096, wait).await?;
            if got.is_empty() {
                in_burst = false;
                continue;
            }
            match chunks.last_mut() {
                Some(last) if in_burst => {
                    last.hex.push(' ');
                    last.hex.push_str(&hex(&got));
                }
                _ => chunks.push(Chunk {
                    at_ms: start.elapsed().as_millis() as u64,
                    hex: hex(&got),
                }),
            }
            in_burst = true;
            all.extend_from_slice(&got);
        }
        steps.push(Step {
            sent: bytes.as_deref().map(hex),
            received: hex(&all),
            ascii: ascii(&all),
            chunks,
        });
    }
    let path = port.path().to_string();
    port.close();

    let mut human = format!("{path} at {} baud, {}\n", args.baud, args.line);
    for s in &steps {
        match &s.sent {
            Some(sent) => human.push_str(&format!("> {sent}\n")),
            None => human.push_str(&format!("listening for {}\n", args.listen)),
        }
        if s.chunks.is_empty() {
            human.push_str("  (nothing received)\n");
        }
        for c in &s.chunks {
            human.push_str(&format!("< {:>5} ms  {}\n", c.at_ms, c.hex));
        }
        if !s.received.is_empty() {
            human.push_str(&format!("  ascii: {}\n", s.ascii));
        }
    }
    Ok(Report::new(
        &serde_json::json!({ "port": path, "baud": args.baud, "line": args.line, "steps": steps }),
        human,
    ))
}

fn probe_spec(args: &ProbeArgs) -> Result<PortSpec> {
    let mut spec = PortSpec {
        path: args.path.clone(),
        serial_number: args.serial_number.clone(),
        ..PortSpec::default()
    };
    if let Some(usb) = &args.usb {
        let id = |s: &str| u16::from_str_radix(s.trim_start_matches("0x"), 16).ok();
        let (vid, pid) = usb
            .split_once(':')
            .and_then(|(v, p)| Some((id(v)?, id(p)?)))
            .ok_or_else(|| {
                DeskError::Usage(format!(
                    "--usb `{usb}`: expected VID:PID in hex, e.g. 2e3c:5740"
                ))
            })?;
        spec.usb_vid = Some(vid);
        spec.usb_pid = Some(pid);
    }
    if spec == PortSpec::default() {
        return Err(DeskError::Usage(
            "name the port: a path, --usb VID:PID or --serial-number (see `desk port list`)".into(),
        ));
    }
    Ok(spec)
}

/// `8N1`-style line settings.
fn line_settings(baud: u32, line: &str) -> Result<Settings> {
    use serialport::{DataBits, Parity, StopBits};
    let bad = || DeskError::Usage(format!("--line `{line}`: expected e.g. 8N1, 8E1, 7O2"));
    let mut c = line.chars();
    let (Some(d), Some(p), Some(s), None) = (c.next(), c.next(), c.next(), c.next()) else {
        return Err(bad());
    };
    Ok(Settings {
        baud,
        data_bits: match d {
            '5' => DataBits::Five,
            '6' => DataBits::Six,
            '7' => DataBits::Seven,
            '8' => DataBits::Eight,
            _ => return Err(bad()),
        },
        parity: match p.to_ascii_uppercase() {
            'N' => Parity::None,
            'E' => Parity::Even,
            'O' => Parity::Odd,
            _ => return Err(bad()),
        },
        stop_bits: match s {
            '1' => StopBits::One,
            '2' => StopBits::Two,
            _ => return Err(bad()),
        },
        ..Settings::default()
    })
}

/// Hex bytes, with or without separators: `F1 A1 DE`, `f1a1de`, `F1:A1`, `0xF1,0xA1`.
fn parse_hex(s: &str) -> Result<Vec<u8>> {
    let bad = |why: &str| DeskError::Usage(format!("--send `{s}`: {why} (use --text for text)"));
    let digits: String = s
        .split(|c: char| c.is_whitespace() || c == ':' || c == ',' || c == '-')
        .map(|t| t.trim_start_matches("0x").trim_start_matches("0X"))
        .collect();
    if digits.is_empty() {
        return Err(bad("no bytes"));
    }
    if !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(bad("not hex"));
    }
    if !digits.len().is_multiple_of(2) {
        return Err(bad("odd number of hex digits"));
    }
    Ok((0..digits.len())
        .step_by(2)
        .filter_map(|i| u8::from_str_radix(&digits[i..i + 2], 16).ok())
        .collect())
}

/// Text with `\n`, `\r`, `\t`, `\\` and `\xNN` escapes.
fn unescape(s: &str) -> Result<Vec<u8>> {
    let bad = || {
        DeskError::Usage(format!(
            "--send `{s}`: bad escape (\\n, \\r, \\t, \\\\, \\xNN)"
        ))
    };
    let mut out = Vec::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('n') => out.push(b'\n'),
            Some('r') => out.push(b'\r'),
            Some('t') => out.push(b'\t'),
            Some('\\') => out.push(b'\\'),
            Some('x') => {
                let h: String = chars.by_ref().take(2).collect();
                if h.len() != 2 {
                    return Err(bad());
                }
                out.push(u8::from_str_radix(&h, 16).map_err(|_| bad())?);
            }
            _ => return Err(bad()),
        }
    }
    if out.is_empty() {
        return Err(DeskError::Usage(format!("--send `{s}`: no bytes")));
    }
    Ok(out)
}

fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn ascii(b: &[u8]) -> String {
    b.iter()
        .map(|&x| {
            if x.is_ascii_graphic() || x == b' ' {
                x as char
            } else {
                '.'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_and_text() {
        let want = vec![0xF1, 0xA1, 0xDE];
        for s in [
            "F1 A1 DE",
            "f1a1de",
            "F1:A1:DE",
            "0xF1, 0xA1, 0xDE",
            "F1-A1-DE",
        ] {
            assert_eq!(parse_hex(s).unwrap(), want, "{s}");
        }
        assert!(parse_hex("F1 A").is_err());
        assert!(parse_hex("zz").is_err());
        assert!(parse_hex("é1").is_err());
        assert!(parse_hex(" ").is_err());
        assert_eq!(unescape("*IDN?\\n").unwrap(), b"*IDN?\n");
        assert_eq!(unescape("a\\x00\\\\\\r\\t").unwrap(), b"a\x00\\\r\t");
        assert!(unescape("\\q").is_err());
        assert!(unescape("\\x4").is_err());
        assert_eq!(hex(&[0x0a, 0xff]), "0A FF");
        assert_eq!(ascii(b"OK\r\n"), "OK..");
    }

    #[test]
    fn parses_line_settings() {
        let s = line_settings(115200, "8E1").unwrap();
        assert_eq!(s.parity, serialport::Parity::Even);
        assert_eq!(s.baud, 115200);
        assert_eq!(
            line_settings(9600, "7o2").unwrap().data_bits,
            serialport::DataBits::Seven
        );
        for bad in ["8N", "9N1", "8X1", "8N3", "8N1x"] {
            assert!(line_settings(9600, bad).is_err(), "{bad}");
        }
    }
}
