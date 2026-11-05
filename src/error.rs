//! Error type shared by the whole crate.
//!
//! Every error has a stable machine-readable `code` (used in `--json` output) and maps to a
//! process exit code. Both are part of the CLI contract that agents rely on, so do not rename
//! codes casually.

use std::time::Duration;

use serde::Serialize;

#[derive(Debug, thiserror::Error)]
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

    #[allow(dead_code)] // constructed by network providers (M1)
    #[error("provider `{provider}` unreachable: {message}")]
    ProviderUnreachable { provider: String, message: String },

    #[error("endpoint `{endpoint}` unavailable: {message}")]
    EndpointUnavailable { endpoint: String, message: String },

    #[error("power state of `{0}` is unknown, cannot toggle")]
    StateUnknown(String),

    #[allow(dead_code)] // constructed by Lua bindings (M2)
    #[error("script failed: {0}")]
    ScriptFailed(String),

    #[error("{what} timed out after {}", humantime::format_duration(*after))]
    Timeout { what: String, after: Duration },

    #[error("verification failed: expected {expected}, device reports {actual}")]
    VerifyFailed { expected: String, actual: String },

    #[error("{0}")]
    Unsupported(String),

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
            DeskError::ProviderUnreachable { .. } => "provider_unreachable",
            DeskError::EndpointUnavailable { .. } => "endpoint_unavailable",
            DeskError::StateUnknown(_) => "state_unknown",
            DeskError::ScriptFailed(_) => "script_failed",
            DeskError::Timeout { .. } => "timeout",
            DeskError::VerifyFailed { .. } => "verify_failed",
            DeskError::Unsupported(_) => "unsupported",
            DeskError::Internal(_) => "internal",
        }
    }

    /// Process exit code. See `plan.md` for the table.
    pub fn exit_code(&self) -> i32 {
        match self {
            DeskError::Internal(_) | DeskError::Unsupported(_) => 1,
            DeskError::Usage(_)
            | DeskError::DeviceNotFound { .. }
            | DeskError::DeviceAmbiguous { .. } => 2,
            DeskError::ConfigInvalid(_) | DeskError::NoPowerPath(_) => 3,
            DeskError::ProviderUnreachable { .. }
            | DeskError::EndpointUnavailable { .. }
            | DeskError::StateUnknown(_) => 4,
            DeskError::ScriptFailed(_) => 5,
            DeskError::Timeout { .. } => 6,
            DeskError::VerifyFailed { .. } => 7,
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
