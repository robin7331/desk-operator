//! `desk stream <device[:channel]> [<stream>] [arg=value…]`: a device's live binary data through
//! the daemon, to stdout.
//!
//! stdout carries the payload (never to a terminal), so this command prints its own output:
//!
//! * default: payload bytes only; `--header` puts the header line first; `--frames` passes the
//!   frames as they come from the daemon (after the header line).
//! * `--discard`: no payload; a summary at the end (bytes, units, blocks, dropped, seconds, MB/s).
//! * `--json`: with `--discard`, envelopes on stdout (the header first with `--header`, the
//!   summary or error last); otherwise the final summary or error envelope goes to stderr.
//! * Without `--json`, errors go to stderr as `error: …`.
//!
//! Exit code: 0 when the source ended, `--duration`/`--bytes` was reached, the reader of stdout
//! went away or on Ctrl-C; the error's code otherwise.

use std::io::{IsTerminal, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt};

use super::StreamArgs;
use crate::bench::Bench;
use crate::error::{DeskError, ErrorJson, Result};
use crate::ops::{self, Catalog, split_channel};
use crate::output::Report;
use crate::stream::frame::{HEADER_LEN, Header, Kind};

/// How long the daemon may take to start the source and answer with the header.
const HEADER_TIMEOUT: Duration = Duration::from_secs(120);

/// Payload goes to stdout only when it isn't a terminal (or with `--discard`, not at all).
pub fn check_target(discard: bool, stdout_is_terminal: bool) -> Result<()> {
    if !discard && stdout_is_terminal {
        return Err(DeskError::Usage(
            "the stream is binary data: redirect stdout (`> file`, `| program`) or count it \
             with --discard"
                .into(),
        ));
    }
    Ok(())
}

/// Why the client stopped reading.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Ended {
    /// The end frame came (the source ended, or the error in `end`).
    Source,
    Duration,
    Bytes,
    /// Ctrl-C.
    Interrupted,
    /// The program reading stdout went away.
    OutputClosed,
}

#[derive(Debug, Serialize)]
struct Summary {
    device: String,
    stream: String,
    ended: Ended,
    bytes: u64,
    units: u64,
    blocks: u64,
    /// Blocks this client lost because it read too slowly.
    dropped: u64,
    /// The first block's offset (a late joiner's is above 0).
    #[serde(skip_serializing_if = "Option::is_none")]
    first_offset: Option<u64>,
    seconds: f64,
    mb_per_s: f64,
    /// What the end frame said (`ended`, totals of the whole stream), when one came.
    #[serde(skip_serializing_if = "Option::is_none")]
    end: Option<Value>,
}

impl Summary {
    fn human(&self) -> String {
        format!(
            "{} {}: {} bytes, {} units, {} blocks, {} dropped, {:.2} s, {:.1} MB/s ({})\n",
            self.device,
            self.stream,
            self.bytes,
            self.units,
            self.blocks,
            self.dropped,
            self.seconds,
            self.mb_per_s,
            match self.ended {
                Ended::Source => "the source ended",
                Ended::Duration => "--duration reached",
                Ended::Bytes => "--bytes reached",
                Ended::Interrupted => "interrupted",
                Ended::OutputClosed => "stdout closed",
            }
        )
    }
}

/// Where the payload goes: a thread that writes stdout, so a slow reader never blocks the
/// connection (or Ctrl-C).
struct Writer {
    tx: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    /// Written buffers come back to be filled again.
    back: std::sync::mpsc::Receiver<Vec<u8>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Writer {
    fn start() -> Writer {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
        let (give_back, back) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            while let Some(buf) = rx.blocking_recv() {
                if out.write_all(&buf).is_err() {
                    return;
                }
                let _ = give_back.send(buf);
            }
            let _ = out.flush();
        });
        Writer {
            tx: Some(tx),
            back,
            thread: Some(thread),
        }
    }

    /// A buffer to fill: one that was written already, or a new one.
    fn buffer(&self) -> Vec<u8> {
        self.back.try_recv().unwrap_or_default()
    }

    /// `false`: stdout is gone.
    async fn send(&self, buf: Vec<u8>) -> bool {
        match &self.tx {
            Some(tx) => tx.send(buf).await.is_ok(),
            None => false,
        }
    }

    /// Everything sent is written (or stdout is gone).
    fn finish(mut self) {
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn parse_duration(s: &str) -> Result<Duration> {
    humantime::parse_duration(s.trim())
        .ok()
        .filter(|d| !d.is_zero())
        .ok_or_else(|| {
            DeskError::Usage(format!(
                "--duration: `{s}` is not a duration like 10s or 500ms"
            ))
        })
}

pub async fn run(config: Option<&Path>, json: bool, a: StreamArgs) -> Result<Report> {
    let duration = a.duration.as_deref().map(parse_duration).transpose()?;
    let max_bytes = a
        .bytes
        .as_deref()
        .map(|s| {
            crate::capture::parse_size(s).map_err(|e| DeskError::Usage(format!("--bytes: {e}")))
        })
        .transpose()?;
    // What's possible comes from the config: no device is touched for a listing.
    let bench = Bench::load(config)?;
    let (query, channel) = split_channel(&a.device);
    let device = bench.config.resolve_device(query)?;
    let cat = Catalog::of(device)?;
    let Some(name) = a.stream.as_deref() else {
        return super::ops::list_streams(device, &cat, &a.device, channel);
    };
    let (_, _, decl) = cat.stream(name)?;
    if a.args.is_empty() && decl.needs_args() {
        return super::ops::describe_stream(device, &cat, &a.device, channel, name);
    }
    let op = ops::plan_stream(&cat, channel, name, &a.args)?;
    check_target(a.discard, std::io::stdout().is_terminal())?;

    let mut request = vec!["stream".to_string(), a.device.clone(), name.to_string()];
    request.extend(a.args.iter().cloned());
    let mut link = crate::daemon::stream_link(config, &request).await?;

    let mut line = String::new();
    let got = tokio::time::timeout(HEADER_TIMEOUT, link.read.read_line(&mut line))
        .await
        .map_err(|_| link.lost("the desk daemon didn't answer"))?
        .map_err(|e| link.lost(&format!("reading the daemon's reply failed: {e}")))?;
    if got == 0 {
        return Err(link.lost("the desk daemon closed the connection without a reply"));
    }
    let reply: Value = serde_json::from_str(line.trim_end())
        .map_err(|e| link.lost(&format!("unreadable reply from the desk daemon: {e}")))?;
    if reply["ok"] != json!(true) {
        let (err, exit) = refusal(&reply);
        return Ok(finish_error(json, a.discard, err, exit, None));
    }
    let unit = reply["data"]["unit_size"].as_u64().unwrap_or(1).max(1);
    let header_line = line.trim_end().to_string();
    let limit = max_bytes.map(|b| b.div_ceil(unit) * unit);

    let writer = (!a.discard).then(Writer::start);
    if a.header || a.frames {
        if a.discard {
            println!("{header_line}");
        } else if let Some(w) = &writer {
            w.send(format!("{header_line}\n").into_bytes()).await;
        }
    }

    let start = Instant::now();
    let deadline = duration.map(|d| tokio::time::Instant::now() + d);
    let mut s = Summary {
        device: reply["data"]["device"]
            .as_str()
            .unwrap_or(&device.id)
            .to_string(),
        stream: op.stream.clone(),
        ended: Ended::Source,
        bytes: 0,
        units: 0,
        blocks: 0,
        dropped: 0,
        first_offset: None,
        seconds: 0.0,
        mb_per_s: 0.0,
        end: None,
    };
    let mut failure: Option<(ErrorJson, i32)> = None;
    let mut scratch: Vec<u8> = Vec::new();
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    loop {
        let mut head = [0u8; HEADER_LEN];
        let read_head = link.read.read_exact(&mut head);
        let until = async {
            match deadline {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            r = read_head => {
                if let Err(e) = r {
                    failure = Some(lost_failure(&link, &format!(
                        "the stream from the desk daemon broke off: {e}"
                    )));
                    break;
                }
            }
            _ = until => { s.ended = Ended::Duration; break; }
            _ = &mut ctrl_c => { s.ended = Ended::Interrupted; break; }
        }
        let h = match Header::decode(&head) {
            Ok(h) => h,
            Err(e) => {
                failure = Some(lost_failure(
                    &link,
                    &format!("bad frame from the daemon: {e}"),
                ));
                break;
            }
        };
        let len = h.length as usize;
        // --frames keeps the header in front of the payload, in one buffer.
        let pre = if a.frames { HEADER_LEN } else { 0 };
        let mut buf = match &writer {
            Some(w) => w.buffer(),
            None => std::mem::take(&mut scratch),
        };
        buf.resize(pre + len, 0);
        buf[..pre].copy_from_slice(&head[..pre]);
        if let Err(e) = link.read.read_exact(&mut buf[pre..]).await {
            failure = Some(lost_failure(
                &link,
                &format!("the stream from the desk daemon broke off: {e}"),
            ));
            break;
        }
        s.dropped = u64::from(h.dropped);
        if h.kind == Kind::End {
            let env: Value = serde_json::from_slice(&buf[pre..]).unwrap_or(Value::Null);
            if env["ok"] == json!(true) {
                s.end = Some(env["data"].clone());
            } else {
                failure = Some(refusal(&env));
            }
            if a.frames
                && let Some(w) = &writer
            {
                w.send(buf).await;
            }
            break;
        }
        s.first_offset.get_or_insert(h.offset);
        let mut take = len;
        let mut done = false;
        if let Some(l) = limit
            && s.bytes + len as u64 >= l
        {
            done = true;
            if !a.frames {
                take = (l - s.bytes) as usize;
            }
        }
        s.blocks += 1;
        s.bytes += take as u64;
        match &writer {
            Some(w) => {
                buf.truncate(pre + take);
                if !w.send(buf).await {
                    s.ended = Ended::OutputClosed;
                    break;
                }
            }
            None => scratch = buf,
        }
        if done {
            s.ended = Ended::Bytes;
            break;
        }
    }
    s.seconds = start.elapsed().as_secs_f64();
    s.units = s.bytes / unit;
    s.mb_per_s = if s.seconds > 0.0 {
        s.bytes as f64 / 1e6 / s.seconds
    } else {
        0.0
    };
    drop(link);
    if let Some(w) = writer {
        w.finish();
    }
    if let Some((err, exit)) = failure {
        return Ok(finish_error(json, a.discard, err, exit, Some(&s)));
    }
    if json {
        let line = json!({ "ok": true, "data": s }).to_string();
        if a.discard {
            println!("{line}");
        } else {
            eprintln!("{line}");
        }
    } else if a.discard {
        print!("{}", s.human());
    }
    Ok(Report::printed(0))
}

/// An error envelope from the daemon (`{ ok: false, error, exit }`).
fn refusal(v: &Value) -> (ErrorJson, i32) {
    let err: ErrorJson = serde_json::from_value(v["error"].clone()).unwrap_or(ErrorJson {
        code: "daemon_unavailable".into(),
        message: format!("the daemon reported a failure without details: {v}"),
        hint: None,
    });
    let exit = v["exit"].as_i64().map_or(1, |e| e as i32).max(1);
    (err, exit)
}

fn lost_failure(link: &crate::daemon::Link, what: &str) -> (ErrorJson, i32) {
    let e = link.lost(what);
    (e.to_json(), e.exit_code())
}

/// Print an error the way this command prints everything: an envelope on stdout (`--json
/// --discard`) or stderr (`--json`), else `error: …` on stderr.
fn finish_error(
    json: bool,
    discard: bool,
    err: ErrorJson,
    exit: i32,
    summary: Option<&Summary>,
) -> Report {
    if json {
        let mut env = json!({ "ok": false, "error": err });
        if let Some(s) = summary {
            env["data"] = json!(s);
        }
        if discard {
            println!("{env}");
        } else {
            eprintln!("{env}");
        }
    } else {
        if let (true, Some(s)) = (discard, summary) {
            print!("{}", s.human());
        }
        eprintln!("error: {}", err.message);
        if let Some(h) = &err.hint {
            eprintln!("hint: {h}");
        }
    }
    Report::printed(exit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_never_goes_to_a_terminal() {
        let e = check_target(false, true).unwrap_err();
        assert_eq!(e.code(), "usage");
        assert!(e.to_string().contains("--discard"), "{e}");
        assert!(
            check_target(true, true).is_ok(),
            "--discard writes no payload"
        );
        assert!(check_target(false, false).is_ok(), "a pipe or a file");
    }
}
