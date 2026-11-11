//! A Lua driver for a declared capability (any name other than `power`/`source`).
//!
//! ```lua
//! function read(ctx)                -- required if anything is readable: every value and setting
//!   return { temperature = 312.5, setpoint = 330, mode = "normal",
//!            reading = { value = 4.98, unit = "V" } }   -- a value declared with `units`
//! end
//! function set(ctx, name, value) end    -- required if there are settings (value checked + in unit)
//! function action(ctx, name, args) end  -- required if there are actions (args checked)
//!   -- an action with `produces = "file"` fills ctx.output and returns { format = "…", … }
//! function stream(ctx, name, args) end  -- required if there are streams: ctx.describe{ … }
//!   -- once, then fill ctx.output until the source ends (or the stream is stopped)
//! function open(ctx) end / function close(ctx) end   -- optional: a session (connect)
//! ```
//!
//! With channels, `ctx.channel` names the channel of each call. Ranges, limits, the order of
//! settings and verification are done by the core (`crate::declared`), not by drivers.

use async_trait::async_trait;
use indexmap::IndexMap;
use mlua::Value;

use super::{Engine, ScriptSpec};
use crate::capture::{Output, Produced};
use crate::declared::{self, Declarations, Driver, Val};
use crate::error::{DeskError, Result};
use crate::stream::StreamOutput;

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
        if !decls.streams.is_empty() {
            needed.push("stream");
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
        output: Option<&Output>,
    ) -> Result<Option<Produced>> {
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
        self.engine.set_output(output)?;
        let result = self.engine.call_args("action", argv).await;
        // Only during the call: a handle the script kept can't be filled later.
        self.engine.set_output(None)?;
        if let Some(out) = output {
            out.close();
        }
        let returned = result?;
        let Some(out) = output else {
            return Ok(None);
        };
        let file = self.engine.file_name();
        let bad = |m: String| DeskError::ScriptFailed(format!("{file}: action(\"{name}\"): {m}"));
        let raw: serde_json::Value = self.engine.decode(
            "action",
            "a table of plain data like { format = \"srzip\" } (no functions or handles)",
            returned,
        )?;
        let mut meta = match raw {
            serde_json::Value::Object(m) => m,
            _ => {
                return Err(bad(
                    "an action that produces a file must return { format = \"…\", … }".into(),
                ));
            }
        };
        let text = |v: Option<serde_json::Value>, key: &str| match v {
            None => Ok(None),
            Some(serde_json::Value::String(s)) => Ok(Some(s)),
            Some(_) => Err(bad(format!("`{key}` must be a string"))),
        };
        let format = text(meta.remove("format"), "format")?.ok_or_else(|| {
            bad("must return the file's `format`, e.g. { format = \"srzip\" }".into())
        })?;
        let extension = text(meta.remove("extension"), "extension")?;
        let produced = Produced::new(format, extension, meta).map_err(bad)?;
        if out.size() == 0 {
            return Err(bad(
                "left ctx.output empty: fill it with a host function (`into = ctx.output`)".into(),
            ));
        }
        Ok(Some(produced))
    }

    async fn stream(
        &self,
        channel: Option<&str>,
        name: &str,
        args: &IndexMap<String, Val>,
        output: &StreamOutput,
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
        self.engine.set_stream_output(Some(output))?;
        let result = self.engine.call_args("stream", argv).await;
        self.engine.set_stream_output(None)?;
        output.close();
        result.map(drop)
    }

    fn take_log(&self) -> Vec<String> {
        self.engine.take_log()
    }

    fn label(&self) -> String {
        self.engine.file_name()
    }
}
