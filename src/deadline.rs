//! Deadlines that a long host call can suspend.
//!
//! Every command runs under timeouts: the capability's (around a whole session) and a script's
//! per-call deadline (checked by the Lua hook). A host function that fills a capture brings its own
//! timeout, which may be much longer (a 10-minute capture). While it runs, it suspends the
//! deadlines around it with [`suspend`]; they continue afterwards with the time they had left.
//!
//! The deadlines of a task form a stack: [`timeout`] and [`scope`] add one for the future they
//! run, and [`suspend`] pauses all of them until its guard is dropped.

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How often a suspended [`timeout`] checks whether it runs again.
const PAUSED_POLL: Duration = Duration::from_millis(50);

/// A deadline that can be paused. Clones share it.
#[derive(Debug, Clone, Default)]
pub struct Deadline(Arc<Mutex<State>>);

#[derive(Debug, Default)]
struct State {
    end: Option<Instant>,
    /// What the last [`Deadline::start`] gave (for [`restart`]).
    after: Option<Duration>,
    /// Open [`Suspended`] guards.
    paused: usize,
    since: Option<Instant>,
}

impl Deadline {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Expire `after` from now.
    pub fn start(&self, after: Duration) {
        let mut s = self.lock();
        let now = Instant::now();
        s.end = Some(now + after);
        s.after = Some(after);
        if s.paused > 0 {
            s.since = Some(now);
        }
    }

    /// No deadline (between calls).
    pub fn clear(&self) {
        self.lock().end = None;
    }

    /// Past its end and not suspended.
    pub fn expired(&self) -> bool {
        let s = self.lock();
        s.paused == 0 && s.end.is_some_and(|e| Instant::now() >= e)
    }

    /// Time left; `None` while suspended or without an end.
    fn left(&self) -> Option<Duration> {
        let s = self.lock();
        if s.paused > 0 {
            return None;
        }
        s.end.map(|e| e.saturating_duration_since(Instant::now()))
    }

    fn pause(&self) {
        let mut s = self.lock();
        if s.paused == 0 {
            s.since = Some(Instant::now());
        }
        s.paused += 1;
    }

    fn resume(&self) {
        let mut s = self.lock();
        s.paused = s.paused.saturating_sub(1);
        if s.paused == 0
            && let (Some(end), Some(since)) = (s.end, s.since.take())
        {
            s.end = Some(end + since.elapsed());
        }
    }
}

tokio::task_local! {
    static ACTIVE: Vec<Deadline>;
}

/// Run `f` with `deadline` on this task's stack, so [`suspend`] inside it pauses it too.
pub async fn scope<F: Future>(deadline: Deadline, f: F) -> F::Output {
    let mut stack = ACTIVE.try_with(Clone::clone).unwrap_or_default();
    stack.push(deadline);
    ACTIVE.scope(stack, f).await
}

/// The deadline ran out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elapsed;

/// Like `tokio::time::timeout`, but [`suspend`] inside `f` stops the clock.
pub async fn timeout<F: Future>(after: Duration, f: F) -> Result<F::Output, Elapsed> {
    let deadline = Deadline::default();
    deadline.start(after);
    let fut = scope(deadline.clone(), f);
    tokio::pin!(fut);
    loop {
        let wait = deadline.left().unwrap_or(PAUSED_POLL);
        tokio::select! {
            biased;
            out = &mut fut => return Ok(out),
            _ = tokio::time::sleep(wait) => {
                if deadline.expired() {
                    return Err(Elapsed);
                }
            }
        }
    }
}

/// Suspend every deadline of the calling task until the guard is dropped. A host function that
/// fills a capture holds it while it runs, and applies its own timeout instead.
#[must_use = "the deadlines run again when the guard is dropped"]
pub fn suspend() -> Suspended {
    let stack = ACTIVE.try_with(Clone::clone).unwrap_or_default();
    for d in &stack {
        d.pause();
    }
    Suspended(stack)
}

/// Start every running deadline of the calling task again with its whole time: a stream's driver
/// that keeps delivering data never runs out of time, one that stops does.
pub fn restart() {
    let stack = ACTIVE.try_with(Clone::clone).unwrap_or_default();
    for d in &stack {
        let after = {
            let s = d.lock();
            s.end.and(s.after)
        };
        if let Some(after) = after {
            d.start(after);
        }
    }
}

pub struct Suspended(Vec<Deadline>);

impl Drop for Suspended {
    fn drop(&mut self) {
        for d in &self.0 {
            d.resume();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn times_out_like_tokio() {
        let r = timeout(
            Duration::from_millis(30),
            tokio::time::sleep(Duration::from_secs(5)),
        )
        .await;
        assert_eq!(r, Err(Elapsed));
        let r = timeout(Duration::from_millis(500), async { 7 }).await;
        assert_eq!(r, Ok(7));
    }

    #[tokio::test]
    async fn a_suspended_deadline_continues_with_the_time_it_had_left() {
        // 100 ms of budget; 250 ms suspended; then 40 ms more: fits.
        let r = timeout(Duration::from_millis(100), async {
            {
                let _paused = suspend();
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
            "done"
        })
        .await;
        assert_eq!(r, Ok("done"));
        // The time left still counts after the suspension.
        let r = timeout(Duration::from_millis(100), async {
            {
                let _paused = suspend();
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        })
        .await;
        assert_eq!(r, Err(Elapsed));
    }

    #[tokio::test]
    async fn restart_gives_a_deadline_its_whole_time_again() {
        let r = timeout(Duration::from_millis(150), async {
            for _ in 0..4 {
                tokio::time::sleep(Duration::from_millis(80)).await;
                restart();
            }
            "done"
        })
        .await;
        assert_eq!(r, Ok("done"));
    }

    #[tokio::test]
    async fn suspend_reaches_nested_deadlines() {
        let inner = Deadline::default();
        inner.start(Duration::from_millis(20));
        let seen = inner.clone();
        let r = timeout(Duration::from_millis(50), async move {
            scope(seen.clone(), async {
                let _paused = suspend();
                tokio::time::sleep(Duration::from_millis(120)).await;
                seen.expired()
            })
            .await
        })
        .await;
        assert_eq!(r, Ok(false), "both deadlines were suspended");
        assert!(!inner.expired(), "and the inner one got its time back");
        // Without a scope, suspend() is a no-op.
        drop(suspend());
    }
}
