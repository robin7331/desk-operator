//! The `power_switch` capability and the switching semantics shared by every power path.

use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;

use crate::error::{DeskError, Result};
use crate::model::{Power, PowerFeatures, PowerState};

/// Used when a binding doesn't set `timeout`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Poll interval while waiting for a state change to show up.
const SETTLE_POLL: Duration = Duration::from_millis(150);

/// One switchable mains path. Providers (and later Lua scripts) implement this; the core never
/// knows what an "entity" or "endpoint" is.
#[async_trait]
pub trait PowerSwitch: Send + Sync {
    fn features(&self) -> PowerFeatures;
    async fn set(&self, on: bool) -> Result<()>;
    async fn state(&self) -> Result<PowerState>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    On,
    Off,
    Toggle,
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

/// Read the state until it matches `wanted`, for at most the path's settle window. Providers such
/// as Home Assistant may report the old state for a moment after a command.
async fn settle(sw: &dyn PowerSwitch, wanted: Power) -> Result<PowerState> {
    let deadline = tokio::time::Instant::now() + sw.features().settle;
    loop {
        let state = sw.state().await?;
        if state.state == wanted {
            return Ok(state);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(DeskError::VerifyFailed {
                expected: wanted.to_string(),
                actual: state.state.to_string(),
            });
        }
        tokio::time::sleep(SETTLE_POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// In-memory switch; `stuck` ignores `set`, `readback=false` hides the state.
    struct Fake {
        on: Mutex<bool>,
        stuck: bool,
        readback: bool,
        sets: Mutex<u32>,
    }

    impl Fake {
        fn new(on: bool) -> Self {
            Fake {
                on: Mutex::new(on),
                stuck: false,
                readback: true,
                sets: Mutex::new(0),
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
                settle: Duration::ZERO,
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
            Ok(PowerState::real(Power::from_bool(*self.on.lock().unwrap())))
        }
    }

    fn run<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
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
