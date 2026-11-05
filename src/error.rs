//! Error type shared by the whole crate.
//!
//! Every error has a stable machine-readable `code` (used in `--json` output) and maps to a
//! process exit code. Both are part of the CLI contract that agents rely on, so do not rename
//! codes casually.

use std::time::Duration;

use serde::Serialize;

#[derive(Debug, Clone, thiserror::Error)]
pub enum DeskError {
    #[error("{0}")]
    Usage(String),

    #[error("{0}")]
    ConfigInvalid(String),

    #[error("no device matches `{query}` (known: {})", known.join(", "))]
    DeviceNotFound { query: String, known: Vec<String> },

    #[error("`{query}` is ambiguous, it matches: {}", candidates.join(", "))]
    DeviceAmbiguous {
        query: String,
        candidates: Vec<String>,
    },

    #[error("device `{0}` has no power path configured")]
    NoPowerPath(String),

    #[error("device `{device}` has no `{key}` binding, so it can't do {capability}")]
    CapabilityMissing {
        device: String,
        key: &'static str,
        capability: &'static str,
    },

    #[error("device `{0}` is powered off (mains)")]
    DeviceOff(String),

    #[error("{0} protection is tripped: the device switched its output off")]
    ProtectionTripped(String),

    #[error("{what} {requested} exceeds the {kind} of {limit}")]
    LimitExceeded {
        what: String,
        requested: String,
        limit: String,
        kind: String,
    },

    #[error("provider `{provider}` unreachable: {message}")]
    ProviderUnreachable { provider: String, message: String },

    #[error("endpoint `{endpoint}` unavailable: {message}")]
    EndpointUnavailable { endpoint: String, message: String },

    #[error("power state of `{0}` is unknown, cannot toggle")]
    StateUnknown(String),

    #[error("script failed: {0}")]
    ScriptFailed(String),

    #[error("{what} timed out after {}", humantime::format_duration(*after))]
    Timeout { what: String, after: Duration },

    #[error("verification failed: expected {expected}, device reports {actual}")]
    VerifyFailed { expected: String, actual: String },

    #[error("{0}")]
    Internal(String),
}

impl DeskError {
    /// Stable machine-readable code, used in `--json` output.
    pub fn code(&self) -> &'static str {
        match self {
            DeskError::Usage(_) => "usage",
            DeskError::ConfigInvalid(_) => "config_invalid",
            DeskError::DeviceNotFound { .. } => "device_not_found",
            DeskError::DeviceAmbiguous { .. } => "device_ambiguous",
            DeskError::NoPowerPath(_) => "no_power_path",
            DeskError::CapabilityMissing { .. } => "capability_missing",
            DeskError::DeviceOff(_) => "device_off",
            DeskError::LimitExceeded { .. } => "limit_exceeded",
            DeskError::ProtectionTripped(_) => "protection_tripped",
            DeskError::ProviderUnreachable { .. } => "provider_unreachable",
            DeskError::EndpointUnavailable { .. } => "endpoint_unavailable",
            DeskError::StateUnknown(_) => "state_unknown",
            DeskError::ScriptFailed(_) => "script_failed",
            DeskError::Timeout { .. } => "timeout",
            DeskError::VerifyFailed { .. } => "verify_failed",
            DeskError::Internal(_) => "internal",
        }
    }

    /// Process exit code. See `plan.md` for the table.
    pub fn exit_code(&self) -> i32 {
        match self {
            DeskError::Internal(_) => 1,
            DeskError::Usage(_)
            | DeskError::DeviceNotFound { .. }
            | DeskError::DeviceAmbiguous { .. } => 2,
            DeskError::ConfigInvalid(_)
            | DeskError::NoPowerPath(_)
            | DeskError::CapabilityMissing { .. } => 3,
            DeskError::ProviderUnreachable { .. }
            | DeskError::EndpointUnavailable { .. }
            | DeskError::StateUnknown(_)
            | DeskError::DeviceOff(_)
            | DeskError::ProtectionTripped(_) => 4,
            DeskError::ScriptFailed(_) => 5,
            DeskError::Timeout { .. } => 6,
            DeskError::VerifyFailed { .. } => 7,
            DeskError::LimitExceeded { .. } => 8,
        }
    }

    /// Optional next step for the user or agent.
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            DeskError::DeviceNotFound { .. } | DeskError::DeviceAmbiguous { .. } => {
                Some("run `desk device list` to see device ids and aliases")
            }
            DeskError::NoPowerPath(_) => Some("add a `power = { ... }` binding to the device"),
            DeskError::StateUnknown(_) => Some("use `desk power on` or `desk power off` instead"),
            DeskError::DeviceOff(_) => Some("switch its mains on first: `desk power on <device>`"),
            DeskError::ProtectionTripped(_) => Some(
                "find out why (short, overload, heat) first; then switch on with `--clear-protection`",
            ),
            DeskError::LimitExceeded { .. } => Some(
                "use a lower value (a bench `limit` can be raised in the device's config; a device maximum can't)",
            ),
            _ => None,
        }
    }

    pub fn to_json(&self) -> ErrorJson {
        ErrorJson {
            code: self.code(),
            message: self.to_string(),
            hint: self.hint(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorJson {
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<&'static str>,
}

impl From<std::io::Error> for DeskError {
    fn from(e: std::io::Error) -> Self {
        DeskError::Internal(e.to_string())
    }
}

pub type Result<T, E = DeskError> = std::result::Result<T, E>;

/// Every error code with its exit code and meaning, as published by `desk schema`.
/// A test keeps this in sync with [`DeskError::code`] / [`DeskError::exit_code`].
pub const ERROR_CODES: &[(&str, i32, &str)] = &[
    ("internal", 1, "bug or unexpected environment problem"),
    ("usage", 2, "invalid command line"),
    (
        "device_not_found",
        2,
        "no device matches the given id, alias or name",
    ),
    ("device_ambiguous", 2, "the name matches several devices"),
    (
        "config_invalid",
        3,
        "desk.toml (or a referenced file) is missing or invalid",
    ),
    ("no_power_path", 3, "the device has no power binding"),
    (
        "capability_missing",
        3,
        "the device has no binding for this capability (e.g. no `source`)",
    ),
    (
        "device_off",
        4,
        "the device's mains power is off, so it can't be reached",
    ),
    (
        "protection_tripped",
        4,
        "a protection (OVP, OCP, ...) tripped and switched the output off; output on is refused without --clear-protection",
    ),
    (
        "limit_exceeded",
        8,
        "refused: the value exceeds the device maximum or the bench `limits`; nothing was sent",
    ),
    (
        "provider_unreachable",
        4,
        "provider offline, unreachable, or authentication failed",
    ),
    (
        "endpoint_unavailable",
        4,
        "the provider is fine but this entity/outlet is unknown or offline",
    ),
    (
        "state_unknown",
        4,
        "the current state can't be read (send-only path), so toggle is refused",
    ),
    ("script_failed", 5, "a Lua power script raised an error"),
    ("timeout", 6, "the operation exceeded its timeout"),
    (
        "verify_failed",
        7,
        "a command was sent but the device did not reach the target state",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_table_matches_variants() {
        let all = [
            DeskError::Internal(String::new()),
            DeskError::Usage(String::new()),
            DeskError::DeviceNotFound {
                query: String::new(),
                known: vec![],
            },
            DeskError::DeviceAmbiguous {
                query: String::new(),
                candidates: vec![],
            },
            DeskError::ConfigInvalid(String::new()),
            DeskError::NoPowerPath(String::new()),
            DeskError::CapabilityMissing {
                device: String::new(),
                key: "",
                capability: "",
            },
            DeskError::DeviceOff(String::new()),
            DeskError::ProtectionTripped(String::new()),
            DeskError::LimitExceeded {
                what: String::new(),
                requested: String::new(),
                limit: String::new(),
                kind: String::new(),
            },
            DeskError::ProviderUnreachable {
                provider: String::new(),
                message: String::new(),
            },
            DeskError::EndpointUnavailable {
                endpoint: String::new(),
                message: String::new(),
            },
            DeskError::StateUnknown(String::new()),
            DeskError::ScriptFailed(String::new()),
            DeskError::Timeout {
                what: String::new(),
                after: Duration::ZERO,
            },
            DeskError::VerifyFailed {
                expected: String::new(),
                actual: String::new(),
            },
        ];
        assert_eq!(all.len(), ERROR_CODES.len());
        for e in &all {
            assert!(
                ERROR_CODES
                    .iter()
                    .any(|(c, x, _)| *c == e.code() && *x == e.exit_code()),
                "{} / {} missing from ERROR_CODES",
                e.code(),
                e.exit_code()
            );
        }
    }
}
