//! Vendor-neutral domain types.

use std::fmt;
use std::time::Duration;

use serde::Serialize;

/// What a provider's endpoints are listed for (`desk provider endpoints`). Only mains so far;
/// declared capabilities are defined by device definitions, not by providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    PowerSwitch,
}

/// Mains power state of a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Power {
    On,
    Off,
    Unknown,
}

impl Power {
    pub fn from_bool(on: bool) -> Self {
        if on { Power::On } else { Power::Off }
    }
}

impl fmt::Display for Power {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Power::On => "on",
            Power::Off => "off",
            Power::Unknown => "unknown",
        })
    }
}

/// How much a reported state can be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    /// Read back from the device or provider.
    Real,
    /// Inferred from a command we just sent (send-only paths, e.g. RF remotes).
    Assumed,
    /// Nothing is known: the state is `unknown`.
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PowerState {
    pub state: Power,
    pub confidence: Confidence,
}

impl PowerState {
    pub fn real(state: Power) -> Self {
        Self {
            state,
            confidence: Confidence::Real,
        }
    }

    pub fn assumed(state: Power) -> Self {
        Self {
            state,
            confidence: Confidence::Assumed,
        }
    }

    pub fn unknown() -> Self {
        Self {
            state: Power::Unknown,
            confidence: Confidence::None,
        }
    }
}

impl fmt::Display for PowerState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.state, self.confidence) {
            (Power::Unknown, _) | (_, Confidence::None) => f.write_str("unknown"),
            (s, Confidence::Real) => write!(f, "{s}"),
            (s, Confidence::Assumed) => write!(f, "{s} (assumed)"),
        }
    }
}

/// What a power path can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerFeatures {
    /// The real state can be read back (enables verification and idempotent switching).
    pub readback: bool,
    /// How long the read-back state may lag behind a command. Zero means "read once".
    pub settle: Duration,
}
