//! Streams: live data a device delivers until it is stopped (`desk stream`).
//!
//! The second kind of the sink from captures (`crate::capture`): a source (a driver, through a
//! host function) fills a **stream sink**, and the daemon fans its blocks out to subscribers. The
//! core never looks inside the bytes:
//!
//! * The driver describes the data once, before the first byte ([`StreamOutput::describe`]).
//!   `logic` is the well-known content: its description gives `unit_size` (bytes per sample),
//!   `samplerate` and `channels` (names in bit order, the first is bit 0). Any other content is
//!   declared by the model and opaque; `unit_size` (default 1) aligns its blocks.
//! * Bytes are cut into blocks of up to [`BLOCK_MAX`] on unit boundaries. A block goes out when it
//!   is full or [`FLUSH_AFTER`] after its first byte, so low rates stay live. Every block has its
//!   sequence number and the offset of its first unit since the start.
//! * A [`Fanout`] queues the blocks (shared, never copied) per subscriber. A slow subscriber loses
//!   whole blocks, oldest first, and counts them; it never slows the source or the others.

pub mod frame;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Map, Value};
use tokio::sync::Notify;

use crate::capture::Cancel;
use crate::error::{DeskError, Result};

/// The well-known content of a logic analyzer's samples.
pub const LOGIC: &str = "logic";
/// The largest block (1 MiB).
pub const BLOCK_MAX: usize = 1 << 20;
/// A block goes out at the latest this long after its first byte.
pub const FLUSH_AFTER: Duration = Duration::from_millis(5);
/// A source that sends nothing for this long ends the stream with `timeout`
/// (`$DESK_STREAM_STALL` overrides it, for tests).
pub const STALL: Duration = Duration::from_secs(10);
/// What one subscriber may have queued before it loses the oldest blocks.
pub const QUEUE_BYTES: usize = 64 << 20;
/// The same in blocks (low rates make small blocks).
pub const QUEUE_BLOCKS: usize = 4096;
/// The largest `unit_size` of a content other than `logic`.
const UNIT_MAX: u64 = 4096;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The stall timeout: `$DESK_STREAM_STALL` (a duration, for tests), else [`STALL`].
pub fn stall_timeout() -> Duration {
    std::env::var("DESK_STREAM_STALL")
        .ok()
        .and_then(|s| humantime::parse_duration(s.trim()).ok())
        .filter(|d| !d.is_zero())
        .unwrap_or(STALL)
}

/// What one subscriber may have queued: `$DESK_STREAM_QUEUE` (a size, for tests), else
/// [`QUEUE_BYTES`].
pub fn queue_bytes() -> usize {
    std::env::var("DESK_STREAM_QUEUE")
        .ok()
        .and_then(|s| crate::capture::parse_size(&s).ok())
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(QUEUE_BYTES)
}

// ---------------------------------------------------------------------------------------------
// The description

/// What the driver said about its data, checked.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Description {
    /// Bytes per unit (a sample for `logic`); blocks hold whole units.
    pub unit_size: u32,
    /// `logic`: samples per second.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub samplerate: Option<f64>,
    /// `logic`: channel names in bit order (the first is bit 0).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channels: Option<Vec<String>>,
    /// Everything else the driver said, untouched.
    pub meta: Map<String, Value>,
}

impl Description {
    /// Check a driver's description of `content`.
    pub fn parse(content: &str, mut raw: Map<String, Value>) -> std::result::Result<Self, String> {
        let unit = raw.remove("unit_size");
        let logic = content == LOGIC;
        let unit_size = match unit {
            None if logic => {
                return Err("a `logic` stream needs `unit_size` (bytes per sample, 1 to 8)".into());
            }
            None => 1,
            Some(v) => {
                let max = if logic { 8 } else { UNIT_MAX };
                match v.as_u64() {
                    Some(n) if (1..=max).contains(&n) => n as u32,
                    _ => {
                        return Err(format!(
                            "`unit_size` must be a whole number from 1 to {max}, got {v}"
                        ));
                    }
                }
            }
        };
        let (mut samplerate, mut channels) = (None, None);
        if logic {
            samplerate = match raw.remove("samplerate") {
                Some(Value::Number(n)) if n.as_f64().is_some_and(|f| f > 0.0 && f.is_finite()) => {
                    n.as_f64()
                }
                Some(v) => return Err(format!("`samplerate` must be a number above 0, got {v}")),
                None => {
                    return Err("a `logic` stream needs `samplerate` (samples per second)".into());
                }
            };
            let names: Vec<String> = match raw.remove("channels") {
                Some(Value::Array(a)) => a
                    .into_iter()
                    .map(|c| match c {
                        Value::String(s)
                            if !s.is_empty()
                                && s.len() <= 64
                                && !s.chars().any(char::is_control) =>
                        {
                            Ok(s)
                        }
                        other => Err(format!("`channels` lists names like \"D0\", not {other}")),
                    })
                    .collect::<std::result::Result<_, _>>()?,
                Some(v) => {
                    return Err(format!(
                        "`channels` must be a list of names in bit order, like {{ \"D0\", \"D1\" }}, got {v}"
                    ));
                }
                None => {
                    return Err(
                        "a `logic` stream needs `channels` (names in bit order: the first is bit 0)"
                            .into(),
                    );
                }
            };
            if names.is_empty() {
                return Err("`channels` is empty".into());
            }
            if names.len() > unit_size as usize * 8 {
                return Err(format!(
                    "{} channels don't fit in {unit_size} byte(s) per sample (at most {})",
                    names.len(),
                    unit_size * 8
                ));
            }
            channels = Some(names);
        }
        Ok(Description {
            unit_size,
            samplerate,
            channels,
            meta: raw,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Blocks and the fan-out

/// Bytes of whole units, shared by every subscriber.
#[derive(Debug)]
pub struct Block {
    pub seq: u64,
    /// The first unit's number since the start.
    pub offset: u64,
    pub data: Vec<u8>,
}

/// The end of a stream as a subscriber sees it.
#[derive(Debug, Clone)]
pub struct End {
    /// The next block's `seq` and `offset`.
    pub seq: u64,
    pub offset: u64,
    /// The end frame's payload: `{ ok, data }` or `{ ok: false, error, exit }`.
    pub envelope: Arc<Vec<u8>>,
}

/// What a subscriber gets next.
#[derive(Debug)]
pub enum Item {
    /// A block, and how many this subscriber lost so far.
    Block(Arc<Block>, u64),
    End(End, u64),
}

#[derive(Debug, Default)]
struct QueueState {
    blocks: VecDeque<Arc<Block>>,
    bytes: usize,
    dropped: u64,
    end: Option<End>,
}

#[derive(Debug, Default)]
struct Queue {
    state: Mutex<QueueState>,
    ready: Notify,
}

/// Every subscriber of one stream and its queue.
#[derive(Debug)]
pub struct Fanout {
    subs: Mutex<HashMap<u64, Arc<Queue>>>,
    next: AtomicU64,
    max_bytes: usize,
    max_blocks: usize,
    /// A subscriber left.
    pub left: Notify,
}

impl Default for Fanout {
    fn default() -> Self {
        Fanout::with_bounds(QUEUE_BYTES, QUEUE_BLOCKS)
    }
}

impl Fanout {
    pub fn with_bounds(max_bytes: usize, max_blocks: usize) -> Fanout {
        Fanout {
            subs: Mutex::default(),
            next: AtomicU64::new(0),
            max_bytes,
            max_blocks,
            left: Notify::new(),
        }
    }

    pub fn subscribe(self: &Arc<Self>) -> Subscription {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let queue = Arc::new(Queue::default());
        lock(&self.subs).insert(id, queue.clone());
        Subscription {
            id,
            queue,
            fanout: self.clone(),
        }
    }

    pub fn count(&self) -> usize {
        lock(&self.subs).len()
    }

    /// Queue a block for everyone. Never waits: a full queue loses its oldest blocks.
    pub fn publish(&self, block: Arc<Block>) {
        for q in lock(&self.subs).values() {
            let mut s = lock(&q.state);
            if s.end.is_some() {
                continue;
            }
            s.bytes += block.data.len();
            s.blocks.push_back(block.clone());
            while s.blocks.len() > 1
                && (s.bytes > self.max_bytes || s.blocks.len() > self.max_blocks)
            {
                if let Some(old) = s.blocks.pop_front() {
                    s.bytes -= old.data.len();
                    s.dropped += 1;
                }
            }
            drop(s);
            q.ready.notify_one();
        }
    }

    /// End the stream for everyone. `urgent` (the daemon stops): queued blocks are dropped (and
    /// counted) so the end arrives at once; otherwise it comes after them.
    pub fn end(&self, end: End, urgent: bool) {
        for q in lock(&self.subs).values() {
            let mut s = lock(&q.state);
            if urgent {
                s.dropped += s.blocks.len() as u64;
                s.blocks.clear();
                s.bytes = 0;
            }
            s.end = Some(end.clone());
            drop(s);
            q.ready.notify_one();
        }
    }
}

/// One subscriber's place in a [`Fanout`]. Dropping it leaves the stream.
#[derive(Debug)]
pub struct Subscription {
    id: u64,
    queue: Arc<Queue>,
    fanout: Arc<Fanout>,
}

impl Subscription {
    /// The next block, or the end (again and again once it ended).
    pub async fn next(&self) -> Item {
        loop {
            {
                let mut s = lock(&self.queue.state);
                if let Some(b) = s.blocks.pop_front() {
                    s.bytes -= b.data.len();
                    return Item::Block(b, s.dropped);
                }
                if let Some(e) = &s.end {
                    return Item::End(e.clone(), s.dropped);
                }
            }
            self.queue.ready.notified().await;
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        lock(&self.fanout.subs).remove(&self.id);
        self.fanout.left.notify_one();
    }
}

// ---------------------------------------------------------------------------------------------
// The sink

/// Totals of a stream so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Totals {
    pub bytes: u64,
    pub units: u64,
    pub blocks: u64,
}

#[derive(Debug)]
struct Fill {
    description: Option<Description>,
    unit: usize,
    /// The largest block: [`BLOCK_MAX`] rounded down to whole units.
    cap: usize,
    buf: Vec<u8>,
    /// When the block being built got its first byte: the flusher sends it [`FLUSH_AFTER`]
    /// later. `None` while there is nothing to send: an empty buffer, or only a partial unit
    /// that waits for its rest.
    first_at: Option<Instant>,
    seq: u64,
    /// Units in blocks that went out.
    units: u64,
    /// Every byte written (in blocks or still in `buf`).
    bytes: u64,
    /// The last byte arrived (or the stream was described).
    last_data: Instant,
    open: bool,
}

#[derive(Debug)]
struct Shared {
    content: String,
    fill: Mutex<Fill>,
    described: tokio::sync::watch::Sender<bool>,
    /// A block got its first byte.
    wake: Notify,
    cancel: Cancel,
    fanout: Arc<Fanout>,
}

/// The stream sink as fillers see it (`ctx.output` during a driver's `stream()` call). Clones
/// share it. Scripts get an opaque handle they can only pass on.
#[derive(Debug, Clone)]
pub struct StreamOutput(Arc<Shared>);

impl StreamOutput {
    pub fn new(content: &str, fanout: Arc<Fanout>) -> StreamOutput {
        StreamOutput(Arc::new(Shared {
            content: content.to_string(),
            fill: Mutex::new(Fill {
                description: None,
                unit: 1,
                cap: BLOCK_MAX,
                buf: Vec::new(),
                first_at: None,
                seq: 0,
                units: 0,
                bytes: 0,
                last_data: Instant::now(),
                open: true,
            }),
            described: tokio::sync::watch::channel(false).0,
            wake: Notify::new(),
            cancel: Cancel::default(),
            fanout,
        }))
    }

    pub fn content(&self) -> &str {
        &self.0.content
    }

    /// Describe the data: once, before the first byte.
    pub fn describe(&self, raw: Map<String, Value>) -> Result<()> {
        let d = Description::parse(&self.0.content, raw)
            .map_err(|m| DeskError::ScriptFailed(format!("ctx.describe: {m}")))?;
        let mut f = lock(&self.0.fill);
        if !f.open {
            return Err(closed());
        }
        if f.description.is_some() {
            return Err(DeskError::ScriptFailed(
                "ctx.describe: the stream is already described (once, before the first byte)"
                    .into(),
            ));
        }
        f.unit = d.unit_size as usize;
        f.cap = (BLOCK_MAX / f.unit) * f.unit;
        f.last_data = Instant::now();
        f.description = Some(d);
        drop(f);
        self.0.described.send_replace(true);
        Ok(())
    }

    pub fn description(&self) -> Option<Description> {
        lock(&self.0.fill).description.clone()
    }

    /// Resolves once the stream is described.
    pub async fn described(&self) -> Description {
        let mut rx = self.0.described.subscribe();
        // The sender lives in `self`, so this only returns once described.
        let _ = rx.wait_for(|d| *d).await;
        self.description().unwrap_or(Description {
            unit_size: 1,
            samplerate: None,
            channels: None,
            meta: Map::new(),
        })
    }

    /// Append bytes. Full blocks go out at once; the rest within [`FLUSH_AFTER`].
    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        let mut f = lock(&self.0.fill);
        if !f.open {
            return Err(closed());
        }
        if self.0.cancel.is_cancelled() {
            return Err(DeskError::Cancelled("the stream was stopped".into()));
        }
        if f.description.is_none() {
            return Err(DeskError::ScriptFailed(
                "describe the stream before filling it: ctx.describe{ into = ctx.output, … }"
                    .into(),
            ));
        }
        if bytes.is_empty() {
            return Ok(());
        }
        let now = Instant::now();
        f.last_data = now;
        f.bytes += bytes.len() as u64;
        let mut wake = false;
        let mut rest = bytes;
        while !rest.is_empty() {
            if f.buf.is_empty() {
                let want = f.cap.min(rest.len().max(64 * 1024));
                f.buf.reserve(want);
            }
            // The first byte of a block, or more bytes for a partial unit that waited.
            if f.first_at.is_none() {
                f.first_at = Some(now);
                wake = true;
            }
            let n = (f.cap - f.buf.len()).min(rest.len());
            f.buf.extend_from_slice(&rest[..n]);
            rest = &rest[n..];
            if f.buf.len() == f.cap {
                let cap = f.cap;
                self.emit(&mut f, cap);
            }
        }
        drop(f);
        if wake {
            self.0.wake.notify_one();
        }
        Ok(())
    }

    /// Send the first `n` bytes of the block being built (a multiple of the unit).
    fn emit(&self, f: &mut Fill, n: usize) {
        let data = if n == f.buf.len() {
            std::mem::take(&mut f.buf)
        } else {
            let tail = f.buf.split_off(n);
            std::mem::replace(&mut f.buf, tail)
        };
        f.first_at = if f.buf.is_empty() {
            None
        } else {
            Some(Instant::now())
        };
        let block = Block {
            seq: f.seq,
            offset: f.units,
            data,
        };
        f.seq += 1;
        f.units += (n / f.unit) as u64;
        self.0.fanout.publish(Arc::new(block));
    }

    /// Send what's there in whole units (a partial unit waits for its rest).
    pub fn flush(&self) {
        let mut f = lock(&self.0.fill);
        let n = f.buf.len() - f.buf.len() % f.unit;
        if n > 0 {
            self.emit(&mut f, n);
        } else {
            // Nothing whole to send. A partial unit has no deadline until more bytes arrive
            // (`write` sets one then); with one, the flusher would come back at once, forever.
            f.first_at = None;
        }
    }

    /// Runs next to the source: sends a block [`FLUSH_AFTER`] after its first byte. Never ends.
    pub async fn flusher(&self) {
        loop {
            let due = lock(&self.0.fill).first_at.map(|t| t + FLUSH_AFTER);
            match due {
                None => self.0.wake.notified().await,
                Some(at) => {
                    tokio::time::sleep_until(at.into()).await;
                    let late = lock(&self.0.fill)
                        .first_at
                        .is_some_and(|t| t + FLUSH_AFTER <= Instant::now());
                    if late {
                        self.flush();
                    }
                }
            }
        }
    }

    /// How long ago the last byte arrived (or the stream was described).
    pub fn idle(&self) -> Duration {
        lock(&self.0.fill).last_data.elapsed()
    }

    pub fn totals(&self) -> Totals {
        let f = lock(&self.0.fill);
        Totals {
            bytes: f.bytes,
            units: f.bytes / f.unit as u64,
            blocks: f.seq,
        }
    }

    /// The next block's `seq` and `offset`.
    pub fn next_position(&self) -> (u64, u64) {
        let f = lock(&self.0.fill);
        (f.seq, f.units)
    }

    /// No more writes (the driver's call ended).
    pub fn close(&self) {
        lock(&self.0.fill).open = false;
    }

    pub fn is_open(&self) -> bool {
        lock(&self.0.fill).open
    }

    /// Stop the source: host functions that fill it end, writes fail with `cancelled`.
    pub fn cancel(&self) {
        self.0.cancel.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancel.is_cancelled()
    }

    pub async fn cancelled(&self) {
        self.0.cancel.cancelled().await
    }
}

fn closed() -> DeskError {
    DeskError::ScriptFailed("this ctx.output belongs to a stream call that has ended".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logic(unit: u64) -> Map<String, Value> {
        let v = serde_json::json!({
            "unit_size": unit, "samplerate": 1e6, "channels": ["D0", "D1"], "driver": "x"
        });
        v.as_object().unwrap().clone()
    }

    #[test]
    fn descriptions_are_checked() {
        let d = Description::parse(LOGIC, logic(1)).unwrap();
        assert_eq!(d.unit_size, 1);
        assert_eq!(d.samplerate, Some(1e6));
        assert_eq!(d.channels.as_deref().unwrap(), ["D0", "D1"]);
        assert_eq!(d.meta["driver"], "x", "other keys are meta");
        let err = |m: Value| Description::parse(LOGIC, m.as_object().unwrap().clone()).unwrap_err();
        assert!(err(serde_json::json!({"samplerate": 1, "channels": ["a"]})).contains("unit_size"));
        assert!(
            err(serde_json::json!({"unit_size": 9, "samplerate": 1, "channels": ["a"]}))
                .contains("1 to 8")
        );
        assert!(
            err(serde_json::json!({"unit_size": 1, "samplerate": 0, "channels": ["a"]}))
                .contains("above 0")
        );
        assert!(err(serde_json::json!({"unit_size": 1, "samplerate": 1})).contains("channels"));
        let nine: Vec<String> = (0..9).map(|i| format!("D{i}")).collect();
        assert!(
            err(serde_json::json!({"unit_size": 1, "samplerate": 1, "channels": nine}))
                .contains("don't fit")
        );
        // Another content: everything is meta, unit_size optional.
        let o = Description::parse("frames", serde_json::Map::new()).unwrap();
        assert_eq!(o.unit_size, 1);
        let m = serde_json::json!({"unit_size": 4, "width": 640});
        let o = Description::parse("frames", m.as_object().unwrap().clone()).unwrap();
        assert_eq!(o.unit_size, 4);
        assert_eq!(o.meta["width"], 640);
        assert!(o.channels.is_none());
    }

    fn data(item: Item) -> (Arc<Block>, u64) {
        match item {
            Item::Block(b, d) => (b, d),
            Item::End(..) => panic!("ended"),
        }
    }

    #[tokio::test]
    async fn blocks_are_cut_on_unit_boundaries() {
        let fan = Arc::new(Fanout::default());
        let sub = fan.subscribe();
        let out = StreamOutput::new(LOGIC, fan.clone());
        assert!(out.write(b"x").is_err(), "describe first");
        out.describe(logic(3)).unwrap();
        assert!(out.describe(logic(3)).is_err(), "once");
        // Two full blocks and a bit more, in odd pieces.
        let cap = (BLOCK_MAX / 3) * 3;
        let total = 2 * cap + 7;
        let bytes: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();
        for piece in bytes.chunks(100_003) {
            out.write(piece).unwrap();
        }
        let (a, _) = data(sub.next().await);
        let (b, _) = data(sub.next().await);
        assert_eq!((a.seq, a.offset, a.data.len()), (0, 0, cap));
        assert_eq!((b.seq, b.offset), (1, (cap / 3) as u64));
        // The rest goes out on a flush, in whole units: 7 bytes = 2 units + 1 waiting.
        out.flush();
        let (c, _) = data(sub.next().await);
        assert_eq!(
            (c.seq, c.offset, c.data.len()),
            (2, (2 * cap / 3) as u64, 6)
        );
        assert_eq!(c.data[..], bytes[2 * cap..2 * cap + 6]);
        assert_eq!(out.totals().bytes, total as u64);
        assert_eq!(out.next_position(), (3, (2 * cap / 3 + 2) as u64));
    }

    #[tokio::test]
    async fn a_block_goes_out_a_few_ms_after_its_first_byte() {
        let fan = Arc::new(Fanout::default());
        let sub = fan.subscribe();
        let out = StreamOutput::new("bytes", fan.clone());
        out.describe(Map::new()).unwrap();
        let flusher = out.clone();
        let task = tokio::spawn(async move { flusher.flusher().await });
        out.write(b"hello").unwrap();
        let got = tokio::time::timeout(Duration::from_secs(1), sub.next())
            .await
            .unwrap();
        assert_eq!(data(got).0.data, b"hello");
        task.abort();
    }

    #[tokio::test]
    async fn a_partial_unit_waits_for_its_rest_without_spinning() {
        use std::future::Future;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let fan = Arc::new(Fanout::default());
        let sub = fan.subscribe();
        let out = StreamOutput::new("bytes", fan.clone());
        let three = serde_json::json!({ "unit_size": 3 });
        out.describe(three.as_object().unwrap().clone()).unwrap();
        // The flusher as in the daemon, with its polls counted.
        let polls = Arc::new(AtomicUsize::new(0));
        let task = {
            let (flusher, polls) = (out.clone(), polls.clone());
            tokio::spawn(async move {
                let run = flusher.flusher();
                tokio::pin!(run);
                std::future::poll_fn(|cx| {
                    polls.fetch_add(1, Ordering::Relaxed);
                    run.as_mut().poll(cx)
                })
                .await
            })
        };
        // One byte of a three-byte unit, and then nothing for a while.
        out.write(b"a").unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let n = polls.load(Ordering::Relaxed);
        assert!(
            n < 20,
            "the flusher spins while a unit is incomplete: {n} polls"
        );
        // Its rest arrives: the unit goes out within the usual few milliseconds.
        out.write(b"bc").unwrap();
        let got = tokio::time::timeout(Duration::from_secs(1), sub.next())
            .await
            .unwrap();
        assert_eq!(data(got).0.data, b"abc");
        // A whole unit and a bit: the unit goes out, the bit waits, again without spinning.
        out.write(b"defg").unwrap();
        let got = tokio::time::timeout(Duration::from_secs(1), sub.next())
            .await
            .unwrap();
        assert_eq!(data(got).0.data, b"def");
        tokio::time::sleep(Duration::from_millis(100)).await;
        let n = polls.load(Ordering::Relaxed);
        assert!(
            n < 40,
            "the flusher spins on the rest of a block: {n} polls"
        );
        task.abort();
    }

    #[tokio::test]
    async fn a_slow_subscriber_loses_the_oldest_blocks_and_counts_them() {
        let fan = Arc::new(Fanout::with_bounds(45, 100));
        let slow = fan.subscribe();
        let fast = fan.subscribe();
        let block = |seq: u64| {
            Arc::new(Block {
                seq,
                offset: seq * 10,
                data: vec![seq as u8; 10],
            })
        };
        for seq in 0..3 {
            fan.publish(block(seq));
            let (b, d) = data(fast.next().await);
            assert_eq!((b.seq, d), (seq, 0));
        }
        // The slow one holds 3 blocks; 4 more make 70 bytes: the oldest 3 go (40 are left).
        for seq in 3..7 {
            fan.publish(block(seq));
        }
        let (b, dropped) = data(slow.next().await);
        assert_eq!((b.seq, dropped), (3, 3), "oldest first, counted");
        for seq in 3..7 {
            let (b, d) = data(fast.next().await);
            assert_eq!((b.seq, d), (seq, 0), "the fast one lost nothing");
        }
        // The end comes after what's queued; urgent clears it.
        fan.end(
            End {
                seq: 7,
                offset: 70,
                envelope: Arc::new(b"{}".to_vec()),
            },
            false,
        );
        assert_eq!(data(slow.next().await).0.seq, 4);
        drop(fast);
        assert_eq!(fan.count(), 1);
        fan.end(
            End {
                seq: 7,
                offset: 70,
                envelope: Arc::new(b"{}".to_vec()),
            },
            true,
        );
        match slow.next().await {
            Item::End(e, dropped) => assert_eq!((e.seq, dropped), (7, 5)),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_cancelled_or_closed_sink_takes_nothing() {
        let out = StreamOutput::new("bytes", Arc::new(Fanout::default()));
        out.describe(Map::new()).unwrap();
        out.cancel();
        assert_eq!(out.write(b"x").unwrap_err().code(), "cancelled");
        let out = StreamOutput::new("bytes", Arc::new(Fanout::default()));
        out.close();
        assert!(out.describe(Map::new()).is_err());
    }
}
