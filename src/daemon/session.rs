//! Live sessions: one open driver session per watched device, owned by the daemon.
//!
//! A session reads the device in a loop for its watchers and runs commands for that device
//! between two readings, so watching never locks anyone out. It ends when the last watcher
//! leaves, when a read fails, or when it is stopped (mains off, daemon shutdown).
//!
//! Every operation on a device's port, with or without a session, is serialized by that device's
//! port lock: a one-shot command never opens the port while a session is starting or closing.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::{Notify, broadcast, mpsc, oneshot};

use crate::bench::{Run, close_drivers, take_logs};
use crate::error::{DeskError, Result};
use crate::ops::{self, Catalog, DeviceReadings, Drivers, Op, Outcome};

/// Readings buffered per watcher. A watcher that falls further behind loses the oldest ones and
/// is told how many (`dropped`).
const BACKLOG: usize = 256;

/// Failed reads in a row that end a session (one serial hiccup shouldn't end an hours-long log).
const MAX_FAILED_READS: u32 = 3;

/// What a session tells its watchers.
#[derive(Debug, Clone)]
pub enum Event {
    Reading {
        reading: DeviceReadings,
        /// When it was read (monotonic, for intervals) and the wall-clock time.
        at: Instant,
        time: SystemTime,
        log: Vec<String>,
    },
    /// A read failed; the session tries again (watchers count these as `failed`; the error is
    /// in the daemon's log).
    Failed,
    /// The session ended for everyone; no more events follow.
    Ended(DeskError),
}

enum Msg {
    Run {
        op: Op,
        /// The capability keys the op was resolved against (a changed config can't mix them up).
        keys: Vec<String>,
        reply: oneshot::Sender<Run<Outcome>>,
    },
    Stop {
        why: DeskError,
        done: oneshot::Sender<()>,
    },
}

struct Live {
    msgs: mpsc::Sender<Msg>,
    events: broadcast::Sender<Event>,
    /// Watcher id → the interval it asked for. The session reads at the shortest one.
    watchers: Mutex<HashMap<u64, Duration>>,
    /// A watcher joined or left.
    changed: Notify,
}

impl Live {
    fn interval(&self) -> Option<Duration> {
        lock(&self.watchers).values().min().copied()
    }
}

/// A watcher's subscription. Dropping it leaves the session.
pub struct Watcher {
    live: Arc<Live>,
    id: u64,
    pub events: broadcast::Receiver<Event>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        lock(&self.live.watchers).remove(&self.id);
        self.live.changed.notify_one();
    }
}

#[derive(Default)]
pub struct Sessions {
    live: Mutex<HashMap<String, Arc<Live>>>,
    ports: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    next_id: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Sessions {
    /// The lock that serializes everything touching this device's port.
    pub fn port(&self, device: &str) -> Arc<tokio::sync::Mutex<()>> {
        lock(&self.ports)
            .entry(device.to_string())
            .or_default()
            .clone()
    }

    pub fn is_live(&self, device: &str) -> bool {
        lock(&self.live).contains_key(device)
    }

    pub fn count(&self) -> usize {
        lock(&self.live).len()
    }

    /// Join the device's session, or start one with `open` (which opens the driver session).
    pub async fn watch<F, Fut>(
        self: &Arc<Self>,
        device: &str,
        interval: Duration,
        open: F,
    ) -> Result<Watcher>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(Catalog, Drivers, Duration)>>,
    {
        if let Some(w) = self.join(device, interval) {
            return Ok(w);
        }
        let port = self.port(device);
        let _guard = port.lock().await;
        if let Some(w) = self.join(device, interval) {
            return Ok(w);
        }
        let (cat, drivers, timeout) = open().await?;
        let (msgs, rx) = mpsc::channel(16);
        let live = Arc::new(Live {
            msgs,
            events: broadcast::channel(BACKLOG).0,
            watchers: Mutex::new(HashMap::new()),
            changed: Notify::new(),
        });
        lock(&self.live).insert(device.to_string(), live.clone());
        // Subscribed before the session's first reading.
        let watcher = self.subscribe(&live, interval);
        tokio::spawn(
            self.clone()
                .serve(device.to_string(), live, cat, drivers, timeout, rx),
        );
        Ok(watcher)
    }

    /// Under the registry lock, so a retiring session can't miss a watcher that just joined.
    fn join(&self, device: &str, interval: Duration) -> Option<Watcher> {
        let all = lock(&self.live);
        let live = all.get(device)?;
        Some(self.subscribe(live, interval))
    }

    fn subscribe(&self, live: &Arc<Live>, interval: Duration) -> Watcher {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        lock(&live.watchers).insert(id, interval);
        live.changed.notify_one();
        Watcher {
            live: live.clone(),
            id,
            events: live.events.subscribe(),
        }
    }

    /// Run an op through the device's session. `None`: there is no session (anymore).
    pub async fn run(&self, device: &str, keys: Vec<String>, op: Op) -> Option<Run<Outcome>> {
        let live = lock(&self.live).get(device).cloned()?;
        let (reply, answer) = oneshot::channel();
        live.msgs.send(Msg::Run { op, keys, reply }).await.ok()?;
        answer.await.ok()
    }

    /// End the device's session (if any) for every watcher with `why`, and wait until it is
    /// closed.
    pub async fn stop(&self, device: &str, why: DeskError) {
        let Some(live) = lock(&self.live).get(device).cloned() else {
            return;
        };
        let (done, closed) = oneshot::channel();
        if live.msgs.send(Msg::Stop { why, done }).await.is_ok() {
            let _ = closed.await;
        }
    }

    pub async fn stop_all(&self, why: DeskError) {
        let devices: Vec<String> = lock(&self.live).keys().cloned().collect();
        for d in devices {
            self.stop(&d, why.clone()).await;
        }
    }

    async fn serve(
        self: Arc<Self>,
        device: String,
        live: Arc<Live>,
        cat: Catalog,
        drivers: Drivers,
        timeout: Duration,
        mut msgs: mpsc::Receiver<Msg>,
    ) {
        let read_all = Op::Get { channel: None };
        let port = self.port(&device);
        let mut last: Option<Instant> = None;
        let mut failed_reads = 0;
        let mut stopped: Option<oneshot::Sender<()>> = None;
        let why: Option<DeskError> = loop {
            let Some(interval) = live.interval() else {
                // Nobody watches: retire, unless someone joined meanwhile.
                let guard = port.clone().lock_owned().await;
                let retired = {
                    let mut all = lock(&self.live);
                    let nobody = live.interval().is_none();
                    if nobody {
                        all.remove(&device);
                    }
                    nobody
                };
                if retired {
                    close_drivers(&drivers).await;
                    drop(guard);
                    return;
                }
                continue;
            };
            let next = last.map_or_else(Instant::now, |l| l + interval);
            tokio::select! {
                msg = msgs.recv() => match msg {
                    Some(Msg::Run { op, keys, reply }) => {
                        let result = if keys != cat.keys() {
                            Err(DeskError::Usage(format!(
                                "the config of `{device}` changed while it's watched; stop the \
                                 watch first"
                            )))
                        } else {
                            tokio::time::timeout(timeout, ops::run(&cat, &drivers, &op))
                                .await
                                .unwrap_or_else(|_| Err(timed_out(&device, timeout)))
                        };
                        // Watchers see a change at the next reading, right away.
                        if result.is_ok() && op.changes() {
                            last = None;
                        }
                        let _ = reply.send(Run { result, log: take_logs(&drivers) });
                    }
                    Some(Msg::Stop { why, done }) => {
                        stopped = Some(done);
                        break Some(why);
                    }
                    None => break None,
                },
                _ = tokio::time::sleep_until(next.into()) => {
                    last = Some(Instant::now());
                    let read = ops::run(&cat, &drivers, &read_all);
                    let error = match tokio::time::timeout(timeout, read).await {
                        Ok(Ok(outcome)) => {
                            failed_reads = 0;
                            let _ = live.events.send(Event::Reading {
                                reading: outcome.readings,
                                at: Instant::now(),
                                time: SystemTime::now(),
                                log: take_logs(&drivers),
                            });
                            continue;
                        }
                        Ok(Err(e)) => e,
                        Err(_) => timed_out(&device, timeout),
                    };
                    failed_reads += 1;
                    if failed_reads >= MAX_FAILED_READS {
                        break Some(error);
                    }
                    eprintln!("{device}: read failed, retrying: {error}");
                    let _ = live.events.send(Event::Failed);
                }
                _ = live.changed.notified() => {}
            }
        };
        // Ended for everyone: tell the watchers, then close under the port lock so a waiting
        // one-shot command only opens the port once it is free.
        let guard = port.lock_owned().await;
        lock(&self.live).remove(&device);
        if let Some(why) = why {
            let _ = live.events.send(Event::Ended(why));
        }
        drop(msgs);
        close_drivers(&drivers).await;
        drop(guard);
        if let Some(done) = stopped {
            let _ = done.send(());
        }
    }
}

fn timed_out(device: &str, after: Duration) -> DeskError {
    DeskError::Timeout {
        what: format!("watching `{device}`"),
        after,
    }
}
