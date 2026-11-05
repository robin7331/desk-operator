//! `power = { script = … }`: a Lua power path.
//!
//! ```lua
//! function on(ctx)  ... end            -- required
//! function off(ctx) ... end            -- required
//! function state(ctx) return "on" end  -- optional: "on" | "off" | "unknown" | true | false | nil
//! ```

use std::time::Duration;

use async_trait::async_trait;
use mlua::Value;

use super::{Engine, ScriptSpec};
use crate::error::{DeskError, Result};
use crate::model::{Power, PowerFeatures, PowerState};
use crate::power::PowerSwitch;

pub struct ScriptSwitch {
    engine: Engine,
    has_state: bool,
}

impl ScriptSwitch {
    pub fn load(spec: ScriptSpec) -> Result<Self> {
        let engine = Engine::load(spec)?;
        engine.require(&["on", "off"])?;
        let has_state = engine.has("state");
        Ok(ScriptSwitch { engine, has_state })
    }

    pub fn params(&self) -> Option<&[super::ParamSpec]> {
        self.engine.params()
    }

    fn bad_state(&self, got: &str) -> DeskError {
        DeskError::ScriptFailed(format!(
            "{}: state() must return \"on\", \"off\", \"unknown\", a boolean or nil, got {got}",
            self.engine.file_name()
        ))
    }
}

#[async_trait]
impl PowerSwitch for ScriptSwitch {
    fn features(&self) -> PowerFeatures {
        PowerFeatures {
            readback: self.has_state,
            settle: Duration::ZERO,
        }
    }

    async fn set(&self, on: bool) -> Result<()> {
        self.engine
            .call(if on { "on" } else { "off" }, Value::Nil)
            .await
            .map(drop)
    }

    async fn state(&self) -> Result<PowerState> {
        if !self.has_state {
            return Ok(PowerState::unknown());
        }
        let state = match self.engine.call("state", Value::Nil).await? {
            Value::Boolean(b) => Power::from_bool(b),
            Value::Nil => Power::Unknown,
            Value::String(s) => match s.to_str().map(|s| s.to_lowercase()).as_deref() {
                Ok("on") => Power::On,
                Ok("off") => Power::Off,
                Ok("unknown") => Power::Unknown,
                _ => return Err(self.bad_state(&format!("\"{}\"", s.to_string_lossy()))),
            },
            other => return Err(self.bad_state(other.type_name())),
        };
        Ok(match state {
            Power::Unknown => PowerState::unknown(),
            s => PowerState::real(s),
        })
    }

    fn take_log(&self) -> Vec<String> {
        self.engine.take_log()
    }
}
