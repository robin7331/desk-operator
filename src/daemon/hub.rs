//! Stream hubs: one running stream per device, its blocks fanned out to every subscriber.
//!
//! The first subscriber's arguments start the source: the hub takes the device's lock like a
//! command (and keeps it for the life of the stream), opens the driver, calls its `stream()` and
//! waits for the description, which becomes the header every subscriber gets. A subscriber with
//! the same arguments joins; one with others is refused, naming the running ones. The stream
//! ends when the source ends or fails, when it stalls, when the daemon stops or mains goes off,
//! or when the last subscriber leaves (the source is then stopped and the device closed).

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use indexmap::IndexMap;
use serde_json::{Value, json};
use tokio::sync::Notify;

use super::session::Sessions;
use crate::bench::Bench;
use crate::declared::{self, Val};
use crate::error::{DeskError, Result};
use crate::ops::{CapDriver, Catalog, StreamOp};
use crate::stream::{self, Description, End, Fanout, StreamOutput, Subscription, frame};

/// How long a stopped source may take to end before its call is dropped.
const WIND_DOWN: Duration = Duration::from_secs(2);
/// How often the hub looks at the stall and start timeouts.
const TICK: Duration = Duration::from_millis(50);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Which stream with which arguments: subscribers with the same key share it.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamKey {
    pub capability: String,
    pub stream: String,
    pub channel: Option<String>,
    pub args: IndexMap<String, Val>,
}

impl StreamKey {
    fn args_text(&self) -> String {
        if self.args.is_empty() {
            return "no arguments".into();
        }
        self.args
            .iter()
            .map(|(k, v)| format!("{k}={}", v.display(None)))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn name(&self, device: &str) -> String {
        match &self.channel {
            Some(ch) => format!("{device}:{ch} {}", self.stream),
            None => format!("{device} {}", self.stream),
        }
    }
}

#[derive(Debug, Clone)]
enum State {
    Starting,
    /// The header line every subscriber gets.
    Running(Arc<Value>),
    Failed(DeskError),
}

/// One running stream.
#[derive(Debug)]
pub struct Hub {
    pub device: String,
    pub key: StreamKey,
    fanout: Arc<Fanout>,
    state: tokio::sync::watch::Sender<State>,
    stop: Mutex<Option<DeskError>>,
    stopping: Notify,
    done: tokio::sync::watch::Sender<bool>,
}

impl Hub {
    pub fn new(device: &str, key: StreamKey) -> Hub {
        Hub {
            device: device.to_string(),
            key,
            fanout: Arc::new(Fanout::with_bounds(
                stream::queue_bytes(),
                stream::QUEUE_BLOCKS,
            )),
            state: tokio::sync::watch::channel(State::Starting).0,
            stop: Mutex::new(None),
            stopping: Notify::new(),
            done: tokio::sync::watch::channel(false).0,
        }
    }

    pub fn subscribe(&self) -> Subscription {
        self.fanout.subscribe()
    }

    pub fn subscribers(&self) -> usize {
        self.fanout.count()
    }

    /// Why other commands for the device are refused meanwhile.
    pub fn busy(&self) -> DeskError {
        DeskError::EndpointUnavailable {
            endpoint: self.device.clone(),
            message: format!(
                "busy: it is streaming (`desk stream {}`); other commands for it work again \
                 when the stream ends",
                self.key.name(&self.device)
            ),
        }
    }

    /// Why a subscriber that asked for something else is refused.
    pub fn refuse(&self, asked: &StreamKey) -> DeskError {
        let running = self.key.name(&self.device);
        let message = if asked.capability == self.key.capability
            && asked.stream == self.key.stream
            && asked.channel == self.key.channel
        {
            format!(
                "busy: `{running}` already runs with {}; join it with the same arguments, or \
                 wait until it ends",
                self.key.args_text()
            )
        } else {
            format!(
                "busy: it is streaming `{running}` ({}); one stream per device at a time",
                self.key.args_text()
            )
        };
        DeskError::EndpointUnavailable {
            endpoint: self.device.clone(),
            message,
        }
    }

    /// Wait until the source described its data (the header) or failed to start.
    pub async fn header(&self) -> Result<Arc<Value>> {
        let mut rx = self.state.subscribe();
        let state = match rx.wait_for(|s| !matches!(s, State::Starting)).await {
            Ok(s) => s.clone(),
            Err(_) => State::Failed(DeskError::DaemonUnavailable("the stream ended".into())),
        };
        match state {
            State::Running(h) => Ok(h),
            State::Failed(e) => Err(e),
            State::Starting => Err(DeskError::Internal("the stream didn't start".into())),
        }
    }

    /// End the stream with `why` (daemon stopping, mains off) and wait until it is closed.
    pub async fn stop(&self, why: DeskError) {
        lock(&self.stop).get_or_insert(why);
        self.stopping.notify_one();
        let mut rx = self.done.subscribe();
        let _ = rx.wait_for(|d| *d).await;
    }

    fn fail(&self, e: DeskError) {
        self.state.send_replace(State::Failed(e));
    }
}

/// How a stream ended.
enum Why {
    Source(Result<()>),
    /// Not described within the capability's timeout.
    NoStart(Duration),
    Stall(Duration),
    /// The last subscriber left.
    Retired,
    Stopped(DeskError),
}

/// The header line: the usual envelope with what the stream is.
fn header(hub: &Hub, content: &str, d: &Description, time: SystemTime) -> Value {
    let args: serde_json::Map<String, Value> = hub
        .key
        .args
        .iter()
        .map(|(k, v)| (k.clone(), declared::to_json(v)))
        .collect();
    let mut data = json!({
        "device": hub.device,
        "capability": hub.key.capability,
        "stream": hub.key.stream,
    });
    if let Some(ch) = &hub.key.channel {
        data["channel"] = json!(ch);
    }
    data["content"] = json!(content);
    data["unit_size"] = json!(d.unit_size);
    if let Some(r) = d.samplerate {
        data["samplerate"] = json!(r);
    }
    if let Some(c) = &d.channels {
        data["channels"] = json!(c);
    }
    data["meta"] = Value::Object(d.meta.clone());
    data["args"] = Value::Object(args);
    data["time"] = json!(humantime::format_rfc3339_millis(time).to_string());
    data["framing"] = json!(frame::FRAMING);
    data["block_max"] = json!(stream::BLOCK_MAX);
    json!({ "ok": true, "data": data })
}

fn envelope(result: std::result::Result<Value, &DeskError>) -> Arc<Vec<u8>> {
    let v = match result {
        Ok(data) => json!({ "ok": true, "data": data }),
        Err(e) => json!({ "ok": false, "error": e.to_json(), "exit": e.exit_code() }),
    };
    Arc::new(v.to_string().into_bytes())
}

/// Open the device for a stream (under its lock): not while it is watched, mains on, the
/// driver opened within the capability's timeout.
async fn open(
    bench: &Bench,
    sessions: &Sessions,
    device_id: &str,
    op: &StreamOp,
) -> Result<(CapDriver, Duration)> {
    if sessions.is_live(device_id) {
        return Err(DeskError::EndpointUnavailable {
            endpoint: device_id.to_string(),
            message: "busy: it is being watched (`desk watch`); a stream needs the device to \
                      itself"
                .into(),
        });
    }
    let device = bench.config.resolve_device(device_id)?;
    bench.require_mains(device).await?;
    let cat = Catalog::of(device)?;
    let entry = cat
        .entries
        .get(op.entry)
        .ok_or_else(|| DeskError::Internal("the stream's capability is gone".into()))?;
    let timeout = entry.timeout;
    let drv = bench.driver(device, entry)?;
    let opened = tokio::time::timeout(timeout, drv.open())
        .await
        .unwrap_or_else(|_| {
            Err(DeskError::Timeout {
                what: format!("connecting to `{device_id}`"),
                after: timeout,
            })
        });
    match opened {
        Ok(()) => Ok((drv, timeout)),
        Err(e) => {
            let _ = tokio::time::timeout(Duration::from_secs(2), drv.close()).await;
            Err(e)
        }
    }
}

/// Run the stream until it ends. Spawned by the first subscriber.
pub async fn run(
    bench: Bench,
    sessions: Arc<Sessions>,
    hub: Arc<Hub>,
    op: StreamOp,
    content: String,
) {
    let port = sessions.port(&hub.device);
    let guard = port.lock_owned().await;
    let (drv, timeout) = match open(&bench, &sessions, &hub.device, &op).await {
        Ok(v) => v,
        Err(e) => {
            sessions.forget_stream(&hub);
            hub.fail(e);
            drop(guard);
            hub.done.send_replace(true);
            return;
        }
    };
    let stall = stream::stall_timeout();
    let out = StreamOutput::new(&content, hub.fanout.clone());
    let mut announced = false;
    let why = {
        let call = async {
            match &drv {
                CapDriver::Declared(d) => {
                    d.stream(op.channel.as_deref(), &op.stream, &op.args, &out)
                        .await
                }
                CapDriver::Source(_) => Err(DeskError::Internal(
                    "streams need a declared capability".into(),
                )),
            }
        };
        tokio::pin!(call);
        let flusher = out.flusher();
        tokio::pin!(flusher);
        let started = Instant::now();
        let mut tick = tokio::time::interval(TICK);
        let why = loop {
            tokio::select! {
                r = &mut call => break Why::Source(r),
                _ = &mut flusher => {}
                d = out.described(), if !announced => {
                    announced = true;
                    hub.state.send_replace(State::Running(Arc::new(header(
                        &hub,
                        &content,
                        &d,
                        SystemTime::now(),
                    ))));
                }
                _ = tick.tick() => {
                    if !announced && started.elapsed() >= timeout {
                        break Why::NoStart(timeout);
                    }
                    if announced && out.idle() >= stall {
                        break Why::Stall(stall);
                    }
                }
                _ = hub.fanout.left.notified() => {
                    if sessions.retire_stream(&hub) {
                        break Why::Retired;
                    }
                }
                _ = hub.stopping.notified() => {
                    if let Some(e) = lock(&hub.stop).clone() {
                        break Why::Stopped(e);
                    }
                }
            }
        };
        if !matches!(why, Why::Source(_)) {
            // Stop the source: host functions that fill the sink end, a program is killed.
            out.cancel();
            let _ = tokio::time::timeout(WIND_DOWN, &mut call).await;
        }
        why
    };
    out.close();
    out.flush();
    // A source that described its data and ended at once: its subscribers still get the header.
    if !announced && let Some(d) = out.description() {
        announced = true;
        hub.state.send_replace(State::Running(Arc::new(header(
            &hub,
            &content,
            &d,
            SystemTime::now(),
        ))));
    }
    sessions.forget_stream(&hub);
    let (seq, offset) = out.next_position();
    let end = |result: std::result::Result<Value, &DeskError>, urgent: bool| {
        hub.fanout.end(
            End {
                seq,
                offset,
                envelope: envelope(result),
            },
            urgent,
        );
    };
    let t = out.totals();
    match why {
        Why::Source(Ok(())) if announced => end(
            Ok(json!({
                "ended": "source",
                "bytes": t.bytes,
                "units": t.units,
                "blocks": t.blocks,
            })),
            false,
        ),
        Why::Source(Ok(())) => hub.fail(DeskError::ScriptFailed(format!(
            "{}: stream(\"{}\") returned without describing its data (ctx.describe)",
            label(&drv),
            op.stream
        ))),
        Why::Source(Err(e)) | Why::Stopped(e) if !announced => hub.fail(e),
        Why::Source(Err(e)) => end(Err(&e), false),
        Why::Stopped(e) => end(Err(&e), true),
        Why::NoStart(after) => hub.fail(DeskError::Timeout {
            what: format!(
                "starting the stream `{}` (no description)",
                hub.key.name(&hub.device)
            ),
            after,
        }),
        Why::Stall(after) => end(
            Err(&DeskError::Timeout {
                what: format!(
                    "the stream `{}`: no data from the source",
                    hub.key.name(&hub.device)
                ),
                after,
            }),
            false,
        ),
        Why::Retired => {}
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), drv.close()).await;
    drop(guard);
    hub.done.send_replace(true);
}

fn label(drv: &CapDriver) -> String {
    match drv {
        CapDriver::Declared(d) => d.label(),
        CapDriver::Source(_) => "source".into(),
    }
}
