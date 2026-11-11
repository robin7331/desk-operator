//! `ctx.serial`: the host API of a binding's serial `port = { … }`.

use std::time::Duration;

use mlua::{Lua, MultiValue, Table, Value};

use crate::error::DeskError;
use crate::serial::{Port, PortSpec, Settings};

/// The most one `port:read` returns. A read returns "up to" its count, so more is just asked again.
const MAX_READ: usize = 64 * 1024;

/// `ctx.serial`: `open{ baud = …, … }` opens the binding's port (and only that one).
pub(super) fn serial_api(lua: &Lua, port: Option<PortSpec>) -> mlua::Result<Table> {
    let serial = lua.create_table()?;
    let open = lua.create_async_function(move |lua, opts: Option<Table>| {
        let port = port.clone();
        async move {
            let Some(spec) = port else {
                return Err(mlua::Error::external(DeskError::ConfigInvalid(
                    "ctx.serial.open: this binding has no `port = { ... }`".into(),
                )));
            };
            let settings = serial_settings(opts)?;
            let port = Port::open(spec, settings)
                .await
                .map_err(mlua::Error::external)?;
            port_handle(&lua, port)
        }
    })?;
    serial.set("open", open)?;
    Ok(serial)
}

fn serial_settings(opts: Option<Table>) -> mlua::Result<Settings> {
    let mut s = Settings::default();
    let Some(t) = opts else { return Ok(s) };
    let bad = |m: String| mlua::Error::runtime(format!("ctx.serial.open: {m}"));
    for pair in t.pairs::<String, Value>() {
        let (k, v) = pair?;
        match (k.as_str(), v) {
            ("baud", Value::Integer(b)) if b > 0 => s.baud = b as u32,
            ("data_bits", Value::Integer(b)) => {
                s.data_bits = match b {
                    5 => serialport::DataBits::Five,
                    6 => serialport::DataBits::Six,
                    7 => serialport::DataBits::Seven,
                    8 => serialport::DataBits::Eight,
                    _ => return Err(bad(format!("data_bits must be 5..8, got {b}"))),
                }
            }
            ("stop_bits", Value::Integer(b)) => {
                s.stop_bits = match b {
                    1 => serialport::StopBits::One,
                    2 => serialport::StopBits::Two,
                    _ => return Err(bad(format!("stop_bits must be 1 or 2, got {b}"))),
                }
            }
            ("parity", Value::String(p)) => {
                s.parity = match &*p.to_str()? {
                    "none" => serialport::Parity::None,
                    "odd" => serialport::Parity::Odd,
                    "even" => serialport::Parity::Even,
                    other => {
                        return Err(bad(format!(
                            "parity must be none, odd or even, got {other}"
                        )));
                    }
                }
            }
            ("flow", Value::String(f)) => {
                s.flow = match &*f.to_str()? {
                    "none" => serialport::FlowControl::None,
                    "software" => serialport::FlowControl::Software,
                    "hardware" => serialport::FlowControl::Hardware,
                    other => {
                        return Err(bad(format!(
                            "flow must be none, software or hardware, got {other}"
                        )));
                    }
                }
            }
            ("dtr", Value::Boolean(b)) => s.dtr = Some(b),
            ("wait", Value::String(w)) => {
                s.wait = humantime::parse_duration(&w.to_str()?)
                    .map_err(|e| bad(format!("invalid wait: {e}")))?
            }
            ("wait", Value::Integer(ms)) if ms >= 0 => s.wait = Duration::from_millis(ms as u64),
            (k, v) => {
                return Err(bad(format!(
                    "unsupported option `{k}` = {} (options: baud, data_bits, parity, stop_bits, flow, dtr, wait)",
                    v.type_name()
                )));
            }
        }
    }
    Ok(s)
}

/// A Lua table driving one open port: `write`, `read`, `clear`, `close`, `path`.
/// Works with `port:read(…)` and `port.read(…)`.
fn port_handle(lua: &Lua, port: Port) -> mlua::Result<Table> {
    /// Drop a leading `self` table (colon call syntax).
    fn args(mut a: MultiValue) -> MultiValue {
        if matches!(a.front(), Some(Value::Table(_))) {
            a.pop_front();
        }
        a
    }

    let handle = lua.create_table()?;
    handle.set("path", port.path().to_string())?;

    let p = port.clone();
    handle.set(
        "write",
        lua.create_async_function(move |_, a: MultiValue| {
            let p = p.clone();
            async move {
                let Some(Value::String(data)) = args(a).pop_front() else {
                    return Err(mlua::Error::runtime(
                        "port:write expects a string of bytes, e.g. port:write(\"\\x01\\x02\")",
                    ));
                };
                p.write(data.as_bytes().to_vec())
                    .await
                    .map_err(mlua::Error::external)
            }
        })?,
    )?;

    let p = port.clone();
    handle.set(
        "read",
        lua.create_async_function(move |lua, a: MultiValue| {
            let p = p.clone();
            async move {
                let mut a = args(a);
                let max = match a.pop_front() {
                    Some(Value::Integer(n)) if n > 0 => (n as usize).min(MAX_READ),
                    _ => {
                        return Err(mlua::Error::runtime(
                            "port:read expects a byte count, e.g. port:read(17, 500)",
                        ));
                    }
                };
                let timeout = match a.pop_front() {
                    None | Some(Value::Nil) => Duration::from_secs(1),
                    Some(Value::Integer(ms)) if ms >= 0 => Duration::from_millis(ms as u64),
                    Some(Value::Number(ms)) if ms >= 0.0 && ms.is_finite() => {
                        Duration::try_from_secs_f64(ms / 1000.0).map_err(|_| {
                            mlua::Error::runtime("port:read: the timeout is too long")
                        })?
                    }
                    Some(_) => {
                        return Err(mlua::Error::runtime(
                            "port:read timeout must be milliseconds",
                        ));
                    }
                };
                let data = p.read(max, timeout).await.map_err(mlua::Error::external)?;
                lua.create_string(&data)
            }
        })?,
    )?;

    let p = port.clone();
    handle.set(
        "clear",
        lua.create_async_function(move |_, _: MultiValue| {
            let p = p.clone();
            async move { p.clear().await.map_err(mlua::Error::external) }
        })?,
    )?;

    let p = port;
    handle.set(
        "close",
        lua.create_function(move |_, _: MultiValue| {
            p.close();
            Ok(())
        })?,
    )?;
    Ok(handle)
}
