//! `ctx.sigrok`: the host API of a binding's `sigrok = { … }` device.

use std::sync::Arc;
use std::time::Duration;

use mlua::{Lua, Table, Value};

use super::output_of;
use crate::deadline;
use crate::error::DeskError;
use crate::providers::Registry;
use crate::sigrok::{self, Acquire, Length, SigrokSpec};

/// `ctx.sigrok`: `driver`, `conn` and `acquire{ … }` for the binding's sigrok device (and only
/// that one). Without a `sigrok = { … }` binding, `acquire` fails with a config error.
pub(super) fn sigrok_api(
    lua: &Lua,
    device: &str,
    spec: Option<SigrokSpec>,
    providers: Arc<Registry>,
) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    if let Some(s) = &spec {
        t.set("driver", s.driver.as_str())?;
        t.set("conn", s.conn.as_deref())?;
    }
    let device = device.to_string();
    let acquire = lua.create_async_function(move |lua, req: Value| {
        let spec = spec.clone();
        let providers = providers.clone();
        let device = device.clone();
        async move {
            let Some(spec) = spec else {
                return Err(mlua::Error::external(DeskError::ConfigInvalid(
                    "ctx.sigrok.acquire: this binding has no `sigrok = { driver = \"…\" }`".into(),
                )));
            };
            let Value::Table(req) = req else {
                return Err(mlua::Error::runtime(
                    "ctx.sigrok.acquire expects a table, e.g. ctx.sigrok.acquire{ samples = 1000, \
                     into = ctx.output, timeout = \"5s\" }",
                ));
            };
            let acquire = acquire_request(&req)
                .map_err(|m| mlua::Error::runtime(format!("ctx.sigrok.acquire: {m}")))?;
            let out = output_of(&req, "ctx.sigrok.acquire")?;
            // Its own timeout instead of the script's and the session's.
            let _suspended = deadline::suspend();
            let bytes = sigrok::acquire(
                providers.configs(),
                providers.base_dir(),
                &device,
                &spec,
                &acquire,
                &out,
            )
            .await
            .map_err(mlua::Error::external)?;
            let r = lua.create_table()?;
            r.set("bytes", bytes)?;
            Ok(r)
        }
    })?;
    t.set("acquire", acquire)?;
    Ok(t)
}

/// A duration from Lua: milliseconds (number) or a string like "2s".
fn lua_duration(what: &str, v: &Value) -> std::result::Result<Duration, String> {
    let d = match v {
        Value::Integer(ms) if *ms > 0 => Duration::from_millis(*ms as u64),
        Value::Number(ms) if *ms > 0.0 && ms.is_finite() => {
            Duration::try_from_secs_f64(ms / 1000.0).map_err(|_| format!("`{what}` is too long"))?
        }
        Value::String(s) => {
            let s = s.to_str().map_err(|_| format!("`{what}` must be text"))?;
            humantime::parse_duration(s.trim())
                .map_err(|e| format!("`{what}` \"{s}\" is not a duration like \"2s\": {e}"))?
        }
        _ => {
            return Err(format!(
                "`{what}` must be milliseconds (a number above 0) or a duration like \"2s\""
            ));
        }
    };
    if d.is_zero() {
        return Err(format!("`{what}` must be above 0"));
    }
    Ok(d)
}

fn lua_count(what: &str, v: &Value) -> std::result::Result<u64, String> {
    match v {
        Value::Integer(n) if *n > 0 => Ok(*n as u64),
        Value::Number(n) if *n >= 1.0 && n.fract() == 0.0 && *n < 9.0e15 => Ok(*n as u64),
        _ => Err(format!("`{what}` must be a whole number above 0")),
    }
}

/// Check the table a driver passes to `ctx.sigrok.acquire` (everything but `into`).
fn acquire_request(t: &Table) -> std::result::Result<Acquire, String> {
    const KEYS: &[&str] = &[
        "config",
        "channels",
        "time",
        "samples",
        "frames",
        "triggers",
        "wait_trigger",
        "format",
        "into",
        "timeout",
    ];
    let lua_err = |e: mlua::Error| e.to_string();
    for pair in t.pairs::<Value, Value>() {
        let (k, _) = pair.map_err(lua_err)?;
        let known = match &k {
            Value::String(s) => s.to_str().is_ok_and(|s| KEYS.contains(&&*s)),
            _ => false,
        };
        if !known {
            return Err(format!(
                "unknown key {} (allowed: {})",
                match &k {
                    Value::String(s) => format!("`{}`", s.to_string_lossy()),
                    other => format!("of type {}", other.type_name()),
                },
                KEYS.join(", ")
            ));
        }
    }
    let get = |k: &str| t.get::<Value>(k).map_err(lua_err);

    let mut config = Vec::new();
    match get("config")? {
        Value::Nil => {}
        Value::Table(c) => {
            for pair in c.pairs::<Value, Value>() {
                let (k, v) = pair.map_err(lua_err)?;
                let Value::String(k) = k else {
                    return Err("`config` keys must be sigrok config names (strings)".into());
                };
                let k = k.to_string_lossy();
                sigrok::check_config_key(&k)?;
                let v = match v {
                    Value::String(s) => {
                        let s = s
                            .to_str()
                            .map_err(|_| format!("config `{k}` must be text"))?
                            .to_string();
                        sigrok::check_config_value(&k, &s)?;
                        s
                    }
                    Value::Integer(i) => i.to_string(),
                    Value::Number(n) => {
                        sigrok::number_text(n).map_err(|e| format!("config `{k}`: {e}"))?
                    }
                    Value::Boolean(b) => b.to_string(),
                    other => {
                        return Err(format!(
                            "config `{k}` must be a string, number or boolean, not {}",
                            other.type_name()
                        ));
                    }
                };
                config.push((k, v));
            }
            config.sort();
        }
        other => {
            return Err(format!(
                "`config` must be a table like {{ samplerate = 1000000 }}, not {}",
                other.type_name()
            ));
        }
    }

    let channels: Vec<String> = match get("channels")? {
        Value::Nil => Vec::new(),
        Value::String(s) => {
            let s = s
                .to_str()
                .map_err(|_| "`channels` must be text".to_string())?;
            s.split(',').map(|c| c.trim().to_string()).collect()
        }
        Value::Table(list) => {
            let mut v = Vec::new();
            for item in list.sequence_values::<Value>() {
                match item.map_err(lua_err)? {
                    Value::String(s) => v.push(s.to_string_lossy()),
                    other => {
                        return Err(format!(
                            "`channels` lists names like \"D0\", not {}",
                            other.type_name()
                        ));
                    }
                }
            }
            if v.is_empty() {
                return Err("`channels` is empty (leave it out for all channels)".into());
            }
            v
        }
        other => {
            return Err(format!(
                "`channels` must be a list like {{ \"D0\", \"D1\" }} or \"D0,D1\", not {}",
                other.type_name()
            ));
        }
    };
    for c in &channels {
        sigrok::check_channel(c)?;
    }

    let mut lengths = Vec::new();
    for k in ["time", "samples", "frames"] {
        let v = get(k)?;
        if v.is_nil() {
            continue;
        }
        lengths.push(match k {
            "time" => {
                let d = lua_duration("time", &v)?;
                // Whole milliseconds, never shorter than asked.
                let ms = d.as_nanos().div_ceil(1_000_000);
                Length::Time(u64::try_from(ms).map_err(|_| "`time` is too long".to_string())?)
            }
            "samples" => Length::Samples(lua_count(k, &v)?),
            _ => Length::Frames(lua_count(k, &v)?),
        });
    }
    let length = match lengths.as_slice() {
        [one] => *one,
        [] => return Err("give one of `time` (ms), `samples` or `frames`".into()),
        _ => return Err("give only one of `time`, `samples` and `frames`".into()),
    };

    let triggers = match get("triggers")? {
        Value::Nil => None,
        Value::String(s) => {
            let s = s
                .to_str()
                .map_err(|_| "`triggers` must be text".to_string())?;
            sigrok::check_triggers(&s)?;
            Some(s.to_string())
        }
        other => {
            return Err(format!(
                "`triggers` must be a string like \"D0=r\", not {}",
                other.type_name()
            ));
        }
    };
    let wait_trigger = match get("wait_trigger")? {
        Value::Nil => false,
        Value::Boolean(b) => b,
        other => {
            return Err(format!(
                "`wait_trigger` must be a boolean, not {}",
                other.type_name()
            ));
        }
    };
    if wait_trigger && triggers.is_none() {
        return Err("`wait_trigger = true` needs `triggers`".into());
    }
    let format = match get("format")? {
        Value::Nil => "srzip".to_string(),
        Value::String(s) => {
            let s = s
                .to_str()
                .map_err(|_| "`format` must be text".to_string())?;
            sigrok::check_format(&s)?;
            s.to_string()
        }
        other => {
            return Err(format!(
                "`format` must be a string like \"srzip\", not {}",
                other.type_name()
            ));
        }
    };
    let timeout = match get("timeout")? {
        Value::Nil => {
            return Err(
                "`timeout` is required: how long the whole capture may take, trigger wait included (ms or \"10s\")"
                    .into(),
            );
        }
        v => lua_duration("timeout", &v)?,
    };
    Ok(Acquire {
        config,
        channels,
        length,
        triggers,
        wait_trigger,
        format,
        timeout,
    })
}
