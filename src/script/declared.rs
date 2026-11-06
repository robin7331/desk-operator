//! A Lua driver for a declared capability (any name other than `power`/`source`).
//!
//! ```lua
//! function read(ctx)                -- required if anything is readable: every value and setting
//!   return { temperature = 312.5, setpoint = 330, mode = "normal",
//!            reading = { value = 4.98, unit = "V" } }   -- a value declared with `units`
//! end
//! function set(ctx, name, value) end    -- required if there are settings (value checked + in unit)
//! function action(ctx, name, args) end  -- required if there are actions (args checked)
//! function open(ctx) end / function close(ctx) end   -- optional: a session (connect)
//! ```
//!
//! With channels, `ctx.channel` names the channel of each call. Ranges, limits, the order of
//! settings and verification are done by the core (`crate::declared`), not by drivers.

use async_trait::async_trait;
use indexmap::IndexMap;
use mlua::Value;

use super::{Engine, ScriptSpec};
use crate::declared::{self, Declarations, Driver, Val};
use crate::error::Result;

pub struct ScriptDriver {
    engine: Engine,
}

impl ScriptDriver {
    pub fn load(spec: ScriptSpec, decls: &Declarations) -> Result<Self> {
        let engine = Engine::load(spec)?;
        let mut needed = Vec::new();
        if decls.readable() {
            needed.push("read");
        }
        if !decls.settings.is_empty() {
            needed.push("set");
        }
        if !decls.actions.is_empty() {
            needed.push("action");
        }
        engine.require(&needed)?;
        Ok(ScriptDriver { engine })
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
impl Driver for ScriptDriver {
    async fn open(&self) -> Result<()> {
        self.optional("open").await
    }

    async fn close(&self) -> Result<()> {
        self.optional("close").await
    }

    async fn read(&self, channel: Option<&str>) -> Result<serde_json::Value> {
        self.engine.set_channel(channel)?;
        let v = self.engine.call("read", Value::Nil).await?;
        if v.is_nil() {
            return Ok(serde_json::Value::Object(serde_json::Map::new()));
        }
        self.engine
            .decode("read", "a table like { temperature = 312.5 }", v)
    }

    async fn set(&self, channel: Option<&str>, name: &str, value: &Val) -> Result<()> {
        self.engine.set_channel(channel)?;
        let args = vec![
            self.engine
                .to_lua(&serde_json::Value::String(name.into()))?,
            self.engine.to_lua(&declared::to_json(value))?,
        ];
        self.engine.call_args("set", args).await.map(drop)
    }

    async fn action(
        &self,
        channel: Option<&str>,
        name: &str,
        args: &IndexMap<String, Val>,
    ) -> Result<()> {
        self.engine.set_channel(channel)?;
        let table: serde_json::Map<String, serde_json::Value> = args
            .iter()
            .map(|(k, v)| (k.clone(), declared::to_json(v)))
            .collect();
        let argv = vec![
            self.engine
                .to_lua(&serde_json::Value::String(name.into()))?,
            self.engine.to_lua(&serde_json::Value::Object(table))?,
        ];
        self.engine.call_args("action", argv).await.map(drop)
    }

    fn take_log(&self) -> Vec<String> {
        self.engine.take_log()
    }

    fn label(&self) -> String {
        self.engine.file_name()
    }
}
