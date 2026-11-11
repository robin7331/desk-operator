//! `ctx.sigrok`: the host API of a binding's `sigrok = { … }` device.

use std::sync::Arc;
use std::time::Duration;

use mlua::{Lua, Table, Value};

use super::output_of;
use crate::deadline;
use crate::error::DeskError;
use crate::providers::Registry;
use crate::sigrok::{self, Acquire, Get, Length, Measure, Set, SigrokSpec, Typed};

/// `ctx.sigrok`: `driver`, `conn`, `acquire{ … }`, `measure{ … }`, `get{ … }` and `set{ … }` for
/// the binding's sigrok device (and only that one). Without a `sigrok = { … }` binding, the
/// functions fail with a config error.
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
    let call = Call {
        device: device.to_string(),
        spec,
        providers,
    };

    let c = call.clone();
    let acquire = lua.create_async_function(move |lua, req: Value| {
        let c = c.clone();
        async move {
            let f = "ctx.sigrok.acquire";
            let spec = c.binding(f)?;
            let req = table(
                req,
                f,
                "ctx.sigrok.acquire{ samples = 1000, into = ctx.output, timeout = \"5s\" }",
            )?;
            let acquire = acquire_request(&req).map_err(|m| runtime(f, m))?;
            let out = output_of(&req, f)?;
            // Its own timeout instead of the script's and the session's.
            let _suspended = deadline::suspend();
            let bytes = sigrok::acquire(
                c.providers.configs(),
                c.providers.base_dir(),
                &c.device,
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

    let c = call.clone();
    let measure = lua.create_async_function(move |lua, req: Value| {
        let c = c.clone();
        async move {
            let f = "ctx.sigrok.measure";
            let spec = c.binding(f)?;
            let req = table(req, f, "ctx.sigrok.measure{ channels = { \"P1\" } }")?;
            let m = measure_request(&req).map_err(|m| runtime(f, m))?;
            let (cfg, dir) = (c.providers.configs(), c.providers.base_dir());
            let readings = sigrok::measure(cfg, dir, &c.device, &spec, &m)
                .await
                .map_err(mlua::Error::external)?;
            let list = lua.create_table()?;
            for r in readings {
                let t = lua.create_table()?;
                t.set("channel", r.channel)?;
                t.set("value", r.value)?;
                t.set("unit", r.unit)?;
                t.set("flags", lua.create_sequence_from(r.flags)?)?;
                t.set("text", r.text)?;
                list.push(t)?;
            }
            Ok(list)
        }
    })?;
    t.set("measure", measure)?;

    let c = call.clone();
    let get = lua.create_async_function(move |lua, req: Value| {
        let c = c.clone();
        async move {
            let f = "ctx.sigrok.get";
            let spec = c.binding(f)?;
            let req = table(req, f, "ctx.sigrok.get{ keys = { \"voltage\" } }")?;
            let g = get_request(&req).map_err(|m| runtime(f, m))?;
            let (cfg, dir) = (c.providers.configs(), c.providers.base_dir());
            let values = sigrok::get(cfg, dir, &c.device, &spec, &g)
                .await
                .map_err(mlua::Error::external)?;
            let out = lua.create_table()?;
            for (k, v) in values {
                match v {
                    Typed::Bool(b) => out.set(k, b)?,
                    Typed::Integer(i) => out.set(k, i)?,
                    Typed::Number(n) => out.set(k, n)?,
                    Typed::Text(s) => out.set(k, s)?,
                }
            }
            Ok(out)
        }
    })?;
    t.set("get", get)?;

    let c = call;
    let set = lua.create_async_function(move |_, req: Value| {
        let c = c.clone();
        async move {
            let f = "ctx.sigrok.set";
            let spec = c.binding(f)?;
            let req = table(req, f, "ctx.sigrok.set{ config = { voltage_target = 5 } }")?;
            let s = set_request(&req).map_err(|m| runtime(f, m))?;
            let (cfg, dir) = (c.providers.configs(), c.providers.base_dir());
            sigrok::set(cfg, dir, &c.device, &spec, &s)
                .await
                .map_err(mlua::Error::external)
        }
    })?;
    t.set("set", set)?;
    Ok(t)
}

/// What every `ctx.sigrok` function needs: the device, its binding and the backends.
#[derive(Clone)]
struct Call {
    device: String,
    spec: Option<SigrokSpec>,
    providers: Arc<Registry>,
}

impl Call {
    fn binding(&self, function: &str) -> mlua::Result<SigrokSpec> {
        self.spec.clone().ok_or_else(|| {
            mlua::Error::external(DeskError::ConfigInvalid(format!(
                "{function}: this binding has no `sigrok = {{ driver = \"…\" }}`"
            )))
        })
    }
}

fn runtime(function: &str, message: String) -> mlua::Error {
    mlua::Error::runtime(format!("{function}: {message}"))
}

fn table(req: Value, function: &str, example: &str) -> mlua::Result<Table> {
    match req {
        Value::Table(t) => Ok(t),
        _ => Err(mlua::Error::runtime(format!(
            "{function} expects a table, e.g. {example}"
        ))),
    }
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

fn lua_err(e: mlua::Error) -> String {
    e.to_string()
}

/// Refuse keys that aren't in `keys`.
fn only(t: &Table, keys: &[&str]) -> std::result::Result<(), String> {
    for pair in t.pairs::<Value, Value>() {
        let (k, _) = pair.map_err(lua_err)?;
        let known = match &k {
            Value::String(s) => s.to_str().is_ok_and(|s| keys.contains(&&*s)),
            _ => false,
        };
        if !known {
            return Err(format!(
                "unknown key {} (allowed: {})",
                match &k {
                    Value::String(s) => format!("`{}`", s.to_string_lossy()),
                    other => format!("of type {}", other.type_name()),
                },
                keys.join(", ")
            ));
        }
    }
    Ok(())
}

/// `config = { k = v, … }`: checked keys and values as text, sorted by key.
fn config_of(v: Value) -> std::result::Result<Vec<(String, String)>, String> {
    let mut config = Vec::new();
    match v {
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
    Ok(config)
}

/// `channels = { "D0", "D1" }` or `"D0,D1"`, checked; empty for nil. `empty` says what an empty
/// list means.
fn channels_of(v: Value, empty: &str) -> std::result::Result<Vec<String>, String> {
    let channels: Vec<String> = match v {
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
                return Err(format!("`channels` is empty ({empty})"));
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
    Ok(channels)
}

/// `group = "CH1"`, checked; `None` for nil.
fn group_of(v: Value) -> std::result::Result<Option<String>, String> {
    match v {
        Value::Nil => Ok(None),
        Value::String(s) => {
            let s = s.to_str().map_err(|_| "`group` must be text".to_string())?;
            sigrok::check_group(&s)?;
            Ok(Some(s.to_string()))
        }
        other => Err(format!(
            "`group` must be a channel group name like \"CH1\", not {}",
            other.type_name()
        )),
    }
}

/// An optional `timeout`.
fn timeout_of(v: Value) -> std::result::Result<Option<Duration>, String> {
    match v {
        Value::Nil => Ok(None),
        v => lua_duration("timeout", &v).map(Some),
    }
}

/// Check the table a driver passes to `ctx.sigrok.acquire` (everything but `into`).
fn acquire_request(t: &Table) -> std::result::Result<Acquire, String> {
    only(
        t,
        &[
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
        ],
    )?;
    let get = |k: &str| t.get::<Value>(k).map_err(lua_err);

    let config = config_of(get("config")?)?;
    let channels = channels_of(get("channels")?, "leave it out for all channels")?;

    let mut lengths = Vec::new();
    for k in ["time", "samples", "frames"] {
        let v = get(k)?;
        if v.is_nil() {
            continue;
        }
        lengths.push(match k {
            "time" => time_of(&v)?,
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

/// `time`: whole milliseconds, never shorter than asked.
fn time_of(v: &Value) -> std::result::Result<Length, String> {
    let d = lua_duration("time", v)?;
    let ms = d.as_nanos().div_ceil(1_000_000);
    Ok(Length::Time(
        u64::try_from(ms).map_err(|_| "`time` is too long".to_string())?,
    ))
}

/// Check the table a driver passes to `ctx.sigrok.measure`.
fn measure_request(t: &Table) -> std::result::Result<Measure, String> {
    only(
        t,
        &["channels", "samples", "time", "config", "group", "timeout"],
    )?;
    let get = |k: &str| t.get::<Value>(k).map_err(lua_err);
    let channels = channels_of(get("channels")?, "name the channels to read")?;
    if channels.is_empty() {
        return Err("`channels` is required: the channels to read, e.g. { \"P1\" }".into());
    }
    let length = match (get("samples")?, get("time")?) {
        (Value::Nil, Value::Nil) => Length::Samples(1),
        (s, Value::Nil) => Length::Samples(lua_count("samples", &s)?),
        (Value::Nil, t) => time_of(&t)?,
        _ => return Err("give only one of `samples` and `time`".into()),
    };
    Ok(Measure {
        channels,
        length,
        config: config_of(get("config")?)?,
        group: group_of(get("group")?)?,
        timeout: timeout_of(get("timeout")?)?,
    })
}

/// Check the table a driver passes to `ctx.sigrok.get`.
fn get_request(t: &Table) -> std::result::Result<Get, String> {
    only(t, &["keys", "group", "timeout"])?;
    let get = |k: &str| t.get::<Value>(k).map_err(lua_err);
    let mut keys: Vec<String> = Vec::new();
    match get("keys")? {
        Value::Table(list) => {
            for item in list.sequence_values::<Value>() {
                let Value::String(k) = item.map_err(lua_err)? else {
                    return Err("`keys` lists sigrok config names like \"voltage\"".into());
                };
                let k = k.to_string_lossy();
                sigrok::check_config_key(&k)?;
                if keys.contains(&k) {
                    return Err(format!("`keys` names `{k}` twice"));
                }
                keys.push(k);
            }
        }
        Value::Nil => {}
        other => {
            return Err(format!(
                "`keys` must be a list like {{ \"voltage\", \"enabled\" }}, not {}",
                other.type_name()
            ));
        }
    }
    if keys.is_empty() {
        return Err("`keys` is required: at least one sigrok config key to read".into());
    }
    Ok(Get {
        keys,
        group: group_of(get("group")?)?,
        timeout: timeout_of(get("timeout")?)?,
    })
}

/// Check the table a driver passes to `ctx.sigrok.set`.
fn set_request(t: &Table) -> std::result::Result<Set, String> {
    only(t, &["config", "group", "timeout"])?;
    let get = |k: &str| t.get::<Value>(k).map_err(lua_err);
    let config = config_of(get("config")?)?;
    if config.is_empty() {
        return Err("`config` is required: at least one sigrok config key and its value".into());
    }
    Ok(Set {
        config,
        group: group_of(get("group")?)?,
        timeout: timeout_of(get("timeout")?)?,
    })
}
