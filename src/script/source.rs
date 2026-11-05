//! `source = { script = … }`: a Lua driver for the `voltage_source` capability.
//!
//! ```lua
//! function set_voltage(ctx, volts) ... end   -- required
//! function set_current(ctx, amps) ... end    -- required
//! function set_output(ctx, on) ... end       -- required
//! function read(ctx)                         -- required; every field optional
//!   return { output = true, voltage_set = 5, current_set = 1, voltage = 5.01, current = 0.2, power = 1.0,
//!            mode = "cv", protection = "none", temperature = 31.5, input_voltage = 48, charge = 0.1, energy = 0.5 }
//! end
//! function info(ctx) return { max_voltage = 30, max_current = 5 } end   -- optional
//! function open(ctx) ... end                 -- optional: start of a session (connect)
//! function close(ctx) ... end                -- optional: end of a session (always called)
//! ```
//!
//! Limits, ordering and verification are done by the core (`crate::source`), not by drivers.

use async_trait::async_trait;
use mlua::Value;

use super::{Engine, ScriptSpec};
use crate::error::{DeskError, Result};
use crate::source::{Reading, SourceInfo, VoltageSource};

pub struct ScriptSource {
    engine: Engine,
}

impl ScriptSource {
    pub fn load(spec: ScriptSpec) -> Result<Self> {
        let engine = Engine::load(spec)?;
        engine.require(&["set_voltage", "set_current", "set_output", "read"])?;
        Ok(ScriptSource { engine })
    }

    pub fn params(&self) -> Option<&[super::ParamSpec]> {
        self.engine.params()
    }

    async fn optional(&self, name: &str) -> Result<()> {
        if self.engine.has(name) {
            self.engine.call(name, Value::Nil).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl VoltageSource for ScriptSource {
    async fn open(&self) -> Result<()> {
        self.optional("open").await
    }

    async fn close(&self) -> Result<()> {
        self.optional("close").await
    }

    async fn info(&self) -> Result<SourceInfo> {
        if !self.engine.has("info") {
            return Ok(SourceInfo::default());
        }
        let v = self.engine.call("info", Value::Nil).await?;
        self.engine.decode(
            "info",
            "a table like { max_voltage = 30, max_current = 5 }",
            v,
        )
    }

    async fn set_voltage(&self, volts: f64) -> Result<()> {
        self.engine.call("set_voltage", volts).await.map(drop)
    }

    async fn set_current(&self, amps: f64) -> Result<()> {
        self.engine.call("set_current", amps).await.map(drop)
    }

    async fn set_output(&self, on: bool) -> Result<()> {
        self.engine.call("set_output", on).await.map(drop)
    }

    async fn read(&self) -> Result<Reading> {
        match self.engine.call("read", Value::Nil).await? {
            v @ Value::Table(_) => {
                let mut r: Reading = self.engine.decode(
                    "read",
                    "a table like { output = true, voltage = 5.0, current = 0.1, mode = \"cv\" } \
                     (mode: cv|cc; protection: none|ovp|ocp|opp|otp|uvp|other)",
                    v,
                )?;
                // A regulation mode means nothing while the output is off.
                if r.output == Some(false) {
                    r.mode = None;
                }
                Ok(r)
            }
            other => Err(DeskError::ScriptFailed(format!(
                "{}: read() must return a table, got {}",
                self.engine.file_name(),
                other.type_name()
            ))),
        }
    }

    fn take_log(&self) -> Vec<String> {
        self.engine.take_log()
    }
}
