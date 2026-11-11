//! The `power_switch` capability and the switching semantics shared by every power path.

use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;

use crate::error::{DeskError, Result};
use crate::model::{Power, PowerFeatures, PowerState};

/// Used when a binding doesn't set `timeout`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// Scripts typically wait for devices, so they get more time by default.
pub const DEFAULT_SCRIPT_TIMEOUT: Duration = Duration::from_secs(30);

/// Poll interval while waiting for a state change to show up.
const SETTLE_POLL: Duration = Duration::from_millis(150);

/// One switchable mains path. Providers (and later Lua scripts) implement this; the core never
/// knows what an "entity" or "endpoint" is.
#[async_trait]
pub trait PowerSwitch: Send + Sync {
    fn features(&self) -> PowerFeatures;
    async fn set(&self, on: bool) -> Result<()>;
    async fn state(&self) -> Result<PowerState>;
    /// Log lines produced since the last call (scripts use `ctx.log`); most paths have none.
    fn take_log(&self) -> Vec<String> {
        Vec::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    On,
    Off,
    Toggle,
}

impl Target {
    pub fn as_str(self) -> &'static str {
        match self {
            Target::On => "on",
            Target::Off => "off",
            Target::Toggle => "toggle",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    /// State before acting; only known when the path supports readback.
    pub before: Option<PowerState>,
    pub after: PowerState,
    /// Whether a command was actually sent. `false` when the device already was in the target state.
    pub changed: bool,
}

/// Switch a power path, idempotently and with verification where possible:
///
/// * readback: read the state, skip if already there, set, then verify (polling within `settle`).
/// * no readback: set and report the result as `assumed`. Toggle is refused because the current
///   state is unknown.
pub async fn switch(device_id: &str, sw: &dyn PowerSwitch, target: Target) -> Result<Outcome> {
    if !sw.features().readback {
        let on = match target {
            Target::On => true,
            Target::Off => false,
            Target::Toggle => return Err(DeskError::StateUnknown(device_id.to_string())),
        };
        sw.set(on).await?;
        return Ok(Outcome {
            before: None,
            after: PowerState::assumed(Power::from_bool(on)),
            changed: true,
        });
    }

    let before = sw.state().await?;
    let on = match (target, before.state) {
        (Target::On, _) => true,
        (Target::Off, _) => false,
        (Target::Toggle, Power::On) => false,
        (Target::Toggle, Power::Off) => true,
        (Target::Toggle, Power::Unknown) => {
            return Err(DeskError::StateUnknown(device_id.to_string()));
        }
    };
    let wanted = Power::from_bool(on);
    if before.state == wanted {
        return Ok(Outcome {
            before: Some(before),
            after: before,
            changed: false,
        });
    }

    sw.set(on).await?;
    let after = settle(sw, wanted).await?;
    Ok(Outcome {
        before: Some(before),
        after,
        changed: true,
    })
}

/// What [`switch`] would do, without sending anything. The state is still read where possible.
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub before: Option<PowerState>,
    pub target: Power,
    /// `false` when the device already is in the target state.
    pub would_send: bool,
}

pub async fn plan(device_id: &str, sw: &dyn PowerSwitch, target: Target) -> Result<Plan> {
    if !sw.features().readback {
        let on = match target {
            Target::On => true,
            Target::Off => false,
            Target::Toggle => return Err(DeskError::StateUnknown(device_id.to_string())),
        };
        return Ok(Plan {
            before: None,
            target: Power::from_bool(on),
            would_send: true,
        });
    }
    let before = sw.state().await?;
    let on = match (target, before.state) {
        (Target::On, _) => true,
        (Target::Off, _) => false,
        (Target::Toggle, Power::On) => false,
        (Target::Toggle, Power::Off) => true,
        (Target::Toggle, Power::Unknown) => {
            return Err(DeskError::StateUnknown(device_id.to_string()));
        }
    };
    let target = Power::from_bool(on);
    Ok(Plan {
        before: Some(before),
        target,
        would_send: before.state != target,
    })
}

/// Read the state until it matches `wanted`, for at most the path's settle window. Providers such
/// as Home Assistant may report the old state for a moment after a command, or fail a read while
/// they are busy with it: both are tried again until the window ends.
async fn settle(sw: &dyn PowerSwitch, wanted: Power) -> Result<PowerState> {
    let deadline = tokio::time::Instant::now() + sw.features().settle;
    loop {
        let seen = sw.state().await;
        if let Ok(state) = &seen
            && state.state == wanted
        {
            return Ok(*state);
        }
        if tokio::time::Instant::now() >= deadline {
            return match seen {
                Ok(state) => Err(DeskError::VerifyFailed {
                    expected: wanted.to_string(),
                    actual: state.state.to_string(),
                }),
                // The command went out, but the state can't be read: say why.
                Err(e) => Err(e),
            };
        }
        tokio::time::sleep(SETTLE_POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// In-memory switch; `stuck` ignores `set`, `readback=false` hides the state, and after a
    /// `set` the next `failing_reads` reads fail (a provider that is busy for a moment).
    struct Fake {
        on: Mutex<bool>,
        stuck: bool,
        readback: bool,
        sets: Mutex<u32>,
        settle: Duration,
        failing_reads: Mutex<u32>,
    }

    impl Fake {
        fn new(on: bool) -> Self {
            Fake {
                on: Mutex::new(on),
                stuck: false,
                readback: true,
                sets: Mutex::new(0),
                settle: Duration::ZERO,
                failing_reads: Mutex::new(0),
            }
        }
        fn sets(&self) -> u32 {
            *self.sets.lock().unwrap()
        }
    }

    #[async_trait]
    impl PowerSwitch for Fake {
        fn features(&self) -> PowerFeatures {
            PowerFeatures {
                readback: self.readback,
                settle: self.settle,
            }
        }
        async fn set(&self, on: bool) -> Result<()> {
            *self.sets.lock().unwrap() += 1;
            if !self.stuck {
                *self.on.lock().unwrap() = on;
            }
            Ok(())
        }
        async fn state(&self) -> Result<PowerState> {
            if self.sets() > 0 {
                let mut left = self.failing_reads.lock().unwrap();
                if *left > 0 {
                    *left -= 1;
                    return Err(DeskError::EndpointUnavailable {
                        endpoint: "fake".into(),
                        message: "busy".into(),
                    });
                }
            }
            Ok(PowerState::real(Power::from_bool(*self.on.lock().unwrap())))
        }
    }

    fn run<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn switches_and_verifies() {
        let sw = Fake::new(false);
        let out = run(switch("d", &sw, Target::On)).unwrap();
        assert!(out.changed);
        assert_eq!(out.after, PowerState::real(Power::On));
        assert_eq!(sw.sets(), 1);
    }

    #[test]
    fn is_idempotent() {
        let sw = Fake::new(true);
        let out = run(switch("d", &sw, Target::On)).unwrap();
        assert!(!out.changed);
        assert_eq!(sw.sets(), 0);
    }

    #[test]
    fn toggles() {
        let sw = Fake::new(true);
        let out = run(switch("d", &sw, Target::Toggle)).unwrap();
        assert_eq!(out.after.state, Power::Off);
    }

    #[test]
    fn detects_stuck_devices() {
        let sw = Fake {
            stuck: true,
            ..Fake::new(false)
        };
        let err = run(switch("d", &sw, Target::On)).unwrap_err();
        assert_eq!(err.code(), "verify_failed");
    }

    #[test]
    fn a_read_that_fails_right_after_switching_is_tried_again() {
        let sw = Fake {
            settle: Duration::from_secs(2),
            failing_reads: Mutex::new(2),
            ..Fake::new(false)
        };
        let out = run(switch("d", &sw, Target::On)).unwrap();
        assert!(out.changed);
        assert_eq!(out.after, PowerState::real(Power::On));
        assert_eq!(sw.sets(), 1, "the command is sent once");
    }

    #[test]
    fn reads_that_keep_failing_end_with_their_error() {
        let sw = Fake {
            settle: Duration::from_millis(200),
            failing_reads: Mutex::new(u32::MAX),
            ..Fake::new(false)
        };
        let err = run(switch("d", &sw, Target::On)).unwrap_err();
        assert_eq!(err.code(), "endpoint_unavailable");
        assert_eq!(sw.sets(), 1);
    }

    #[test]
    fn send_only_paths_are_assumed_and_cannot_toggle() {
        let sw = Fake {
            readback: false,
            ..Fake::new(false)
        };
        let out = run(switch("d", &sw, Target::On)).unwrap();
        assert_eq!(out.after, PowerState::assumed(Power::On));
        let err = run(switch("d", &sw, Target::Toggle)).unwrap_err();
        assert_eq!(err.code(), "state_unknown");
    }
}
