//! Serial ports: matching a binding's `port = { … }` to a real port, and an async handle around
//! the blocking `serialport` API for drivers.
//!
//! A binding names its port; a script can only open that one (`ctx.serial.open`). Ports are
//! opened exclusively, so two programs never interleave bytes on one device.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::error::{DeskError, Result};

/// Which port a binding uses: `path`, and/or USB ids. Every given field must match.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PortSpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usb_vid: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usb_pid: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial_number: Option<String>,
}

impl TryFrom<toml::Table> for PortSpec {
    type Error = String;

    fn try_from(t: toml::Table) -> std::result::Result<Self, String> {
        let mut spec = PortSpec::default();
        for (k, v) in t {
            match (k.as_str(), v) {
                ("path", toml::Value::String(s)) => spec.path = Some(s),
                ("serial_number", toml::Value::String(s)) => spec.serial_number = Some(s),
                ("usb_vid" | "usb_pid", toml::Value::Integer(i)) => {
                    let id = u16::try_from(i)
                        .map_err(|_| format!("port.{k} must be 0..=0xFFFF, got {i}"))?;
                    if k == "usb_vid" {
                        spec.usb_vid = Some(id);
                    } else {
                        spec.usb_pid = Some(id);
                    }
                }
                ("path" | "serial_number", _) => return Err(format!("port.{k} must be a string")),
                ("usb_vid" | "usb_pid", _) => {
                    return Err(format!("port.{k} must be a number, e.g. 0x1a86"));
                }
                (other, _) => {
                    return Err(format!(
                        "unknown key `port.{other}` (allowed: path, usb_vid, usb_pid, serial_number)"
                    ));
                }
            }
        }
        if spec == PortSpec::default() {
            return Err("`port` needs `path` or `usb_vid`/`usb_pid`".into());
        }
        Ok(spec)
    }
}

impl std::fmt::Display for PortSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts = Vec::new();
        if let Some(p) = &self.path {
            parts.push(p.clone());
        }
        match (self.usb_vid, self.usb_pid) {
            (Some(v), Some(p)) => parts.push(format!("usb {v:04x}:{p:04x}")),
            (Some(v), None) => parts.push(format!("usb vid {v:04x}")),
            (None, Some(p)) => parts.push(format!("usb pid {p:04x}")),
            (None, None) => {}
        }
        if let Some(s) = &self.serial_number {
            parts.push(format!("serial {s}"));
        }
        f.write_str(&parts.join(", "))
    }
}

/// A serial port as `desk port list` shows it.
#[derive(Debug, Clone, Serialize)]
pub struct PortInfo {
    pub path: String,
    pub usb_vid: Option<u16>,
    pub usb_pid: Option<u16>,
    pub serial_number: Option<String>,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
}

pub fn list() -> Result<Vec<PortInfo>> {
    let ports = serialport::available_ports()
        .map_err(|e| DeskError::Internal(format!("cannot list serial ports: {e}")))?;
    let mut out: Vec<PortInfo> = ports
        .into_iter()
        .map(|p| {
            let usb = match p.port_type {
                serialport::SerialPortType::UsbPort(u) => Some(u),
                _ => None,
            };
            PortInfo {
                path: p.port_name,
                usb_vid: usb.as_ref().map(|u| u.vid),
                usb_pid: usb.as_ref().map(|u| u.pid),
                serial_number: usb.as_ref().and_then(|u| u.serial_number.clone()),
                manufacturer: usb.as_ref().and_then(|u| u.manufacturer.clone()),
                product: usb.and_then(|u| u.product),
            }
        })
        .collect();
    // macOS lists every device twice (/dev/tty.X and /dev/cu.X); the cu device is the one to open.
    let cu: Vec<String> = out
        .iter()
        .filter_map(|p| p.path.strip_prefix("/dev/cu.").map(String::from))
        .collect();
    out.retain(|p| {
        !p.path
            .strip_prefix("/dev/tty.")
            .is_some_and(|n| cu.contains(&n.to_string()))
    });
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Find the one port matching `spec`.
///
/// With a `path`, that port is used (symlinks such as `/dev/serial/by-id/…` are followed). If it's
/// a known USB port, its ids must agree with the spec's: a definition's `usb_vid`/`usb_pid` then
/// guard against the wrong device on that path. Ports without USB info (e.g. ptys in tests) are
/// used as given. Without a `path`, exactly one port must match the USB ids.
fn resolve(spec: &PortSpec) -> Result<String> {
    let usb_matches = |p: &PortInfo| {
        spec.usb_vid.is_none_or(|x| p.usb_vid == Some(x))
            && spec.usb_pid.is_none_or(|x| p.usb_pid == Some(x))
            && spec
                .serial_number
                .as_ref()
                .is_none_or(|x| p.serial_number.as_ref() == Some(x))
    };

    if let Some(path) = &spec.path {
        let has_ids =
            spec.usb_vid.is_some() || spec.usb_pid.is_some() || spec.serial_number.is_some();
        if !has_ids {
            return Ok(path.clone());
        }
        let real = std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
        let listed = list()?.into_iter().find(|p| {
            std::fs::canonicalize(&p.path).unwrap_or_else(|_| PathBuf::from(&p.path)) == real
        });
        return match listed {
            Some(p) if p.usb_vid.is_some() && !usb_matches(&p) => {
                Err(DeskError::EndpointUnavailable {
                    endpoint: path.clone(),
                    message: format!(
                        "this port is {}, but the binding expects {spec} (a different device on that path?)",
                        describe_port(&p)
                    ),
                })
            }
            _ => Ok(path.clone()),
        };
    }

    let matches: Vec<PortInfo> = list()?.into_iter().filter(usb_matches).collect();
    match matches.as_slice() {
        [one] => Ok(one.path.clone()),
        [] => Err(DeskError::EndpointUnavailable {
            endpoint: spec.to_string(),
            message: "no serial port matches (is the device powered and plugged in? \
                      see `desk port list`)"
                .into(),
        }),
        many => Err(DeskError::ConfigInvalid(format!(
            "several serial ports match {spec}: {}; add `serial_number` or `path` to `port`",
            many.iter()
                .map(|p| p.path.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

fn describe_port(p: &PortInfo) -> String {
    let mut s = format!(
        "usb {:04x}:{:04x}",
        p.usb_vid.unwrap_or_default(),
        p.usb_pid.unwrap_or_default()
    );
    if let Some(sn) = &p.serial_number {
        s.push_str(&format!(", serial {sn}"));
    }
    s
}

/// Line settings from `ctx.serial.open{ … }`.
#[derive(Debug, Clone)]
pub struct Settings {
    pub baud: u32,
    pub data_bits: serialport::DataBits,
    pub parity: serialport::Parity,
    pub stop_bits: serialport::StopBits,
    pub flow: serialport::FlowControl,
    pub dtr: Option<bool>,
    /// Keep looking for the port this long (e.g. right after switching mains on).
    pub wait: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            baud: 9600,
            data_bits: serialport::DataBits::Eight,
            parity: serialport::Parity::None,
            stop_bits: serialport::StopBits::One,
            flow: serialport::FlowControl::None,
            dtr: None,
            wait: Duration::ZERO,
        }
    }
}

/// An open port. Cheap to clone; all I/O runs on the blocking pool.
#[derive(Clone)]
pub struct Port {
    path: String,
    inner: Arc<Mutex<Option<Box<dyn serialport::SerialPort>>>>,
}

const POLL: Duration = Duration::from_millis(200);

impl Port {
    pub async fn open(spec: PortSpec, settings: Settings) -> Result<Port> {
        tokio::task::spawn_blocking(move || {
            let deadline = Instant::now() + settings.wait;
            loop {
                match resolve(&spec).and_then(|path| open_path(&path, &settings)) {
                    Ok(port) => return Ok(port),
                    // Only "not there (yet)" is worth waiting for.
                    Err(e @ DeskError::EndpointUnavailable { .. }) if Instant::now() < deadline => {
                        let _ = e;
                        std::thread::sleep(POLL);
                    }
                    Err(e) => return Err(e),
                }
            }
        })
        .await
        .map_err(|e| DeskError::Internal(format!("serial task failed: {e}")))?
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    fn unavailable(&self, msg: impl std::fmt::Display) -> DeskError {
        DeskError::EndpointUnavailable {
            endpoint: self.path.clone(),
            message: msg.to_string(),
        }
    }

    async fn with<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Box<dyn serialport::SerialPort>, &Port) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let me = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = me
                .inner
                .lock()
                .map_err(|_| DeskError::Internal("serial port lock poisoned".into()))?;
            match guard.as_mut() {
                Some(port) => f(port, &me),
                None => Err(me.unavailable("the port was closed")),
            }
        })
        .await
        .map_err(|e| DeskError::Internal(format!("serial task failed: {e}")))?
    }

    pub async fn write(&self, data: Vec<u8>) -> Result<()> {
        self.with(move |p, me| {
            p.write_all(&data)
                .and_then(|_| p.flush())
                .map_err(|e| me.unavailable(format!("write failed: {e}")))
        })
        .await
    }

    /// Read up to `max` bytes, returning as soon as `max` arrived or `timeout` passed. May return
    /// fewer bytes, or none.
    pub async fn read(&self, max: usize, timeout: Duration) -> Result<Vec<u8>> {
        self.with(move |p, me| {
            let deadline = Instant::now() + timeout;
            let mut out = Vec::with_capacity(max);
            let mut buf = vec![0u8; max.max(1)];
            while out.len() < max {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                p.set_timeout(left.max(Duration::from_millis(1)))
                    .map_err(|e| me.unavailable(e))?;
                match p.read(&mut buf[..max - out.len()]) {
                    Ok(0) => break,
                    Ok(n) => out.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::TimedOut => break,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(me.unavailable(format!("read failed: {e}"))),
                }
            }
            Ok(out)
        })
        .await
    }

    /// Discard everything received but not read yet.
    pub async fn clear(&self) -> Result<()> {
        self.with(|p, me| {
            p.clear(serialport::ClearBuffer::Input)
                .map_err(|e| me.unavailable(e))
        })
        .await
    }

    pub fn close(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.take();
        }
    }
}

fn open_path(path: &str, s: &Settings) -> Result<Port> {
    let mut port = serialport::new(path, s.baud)
        .data_bits(s.data_bits)
        .parity(s.parity)
        .stop_bits(s.stop_bits)
        .flow_control(s.flow)
        .timeout(Duration::from_secs(1))
        .open()
        .map_err(|e| {
            let busy = matches!(
                e.kind,
                serialport::ErrorKind::Io(std::io::ErrorKind::ResourceBusy)
            ) || e.description.contains("busy")
                || e.description.contains("exclusive")
                || e.description.contains("Resource temporarily unavailable");
            let missing = matches!(e.kind, serialport::ErrorKind::NoDevice)
                || matches!(
                    e.kind,
                    serialport::ErrorKind::Io(std::io::ErrorKind::NotFound)
                );
            DeskError::EndpointUnavailable {
                endpoint: path.to_string(),
                message: if busy {
                    format!("port is busy (used by another program?): {}", e.description)
                } else if missing {
                    format!("port not found: {}", e.description)
                } else {
                    format!("cannot open port: {}", e.description)
                },
            }
        })?;
    if let Some(dtr) = s.dtr {
        port.write_data_terminal_ready(dtr)
            .map_err(|e| DeskError::EndpointUnavailable {
                endpoint: path.to_string(),
                message: format!("cannot set DTR: {e}"),
            })?;
    }
    Ok(Port {
        path: path.to_string(),
        inner: Arc::new(Mutex::new(Some(port))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(t: &str) -> std::result::Result<PortSpec, String> {
        let table: toml::Table = toml::from_str(t).unwrap();
        PortSpec::try_from(table)
    }

    #[test]
    fn parses_port_specs() {
        let s = spec("usb_vid = 0x1a86\nusb_pid = 0x7523").unwrap();
        assert_eq!((s.usb_vid, s.usb_pid), (Some(0x1a86), Some(0x7523)));
        assert_eq!(s.to_string(), "usb 1a86:7523");
        assert_eq!(
            spec("path = \"/dev/ttyACM0\"").unwrap().path.as_deref(),
            Some("/dev/ttyACM0")
        );
        assert!(spec("").unwrap_err().contains("needs"));
        assert!(spec("usb_vid = 70000").unwrap_err().contains("0..=0xFFFF"));
        assert!(spec("vid = 1").unwrap_err().contains("unknown key"));
        assert!(
            spec("usb_vid = \"1a86\"")
                .unwrap_err()
                .contains("must be a number")
        );
    }

    #[test]
    fn missing_usb_device_is_unavailable() {
        let s = spec("usb_vid = 0xfffe\nusb_pid = 0xfffe\nserial_number = \"nope\"").unwrap();
        let e = resolve(&s).unwrap_err();
        assert_eq!(e.code(), "endpoint_unavailable");
        assert!(e.to_string().contains("desk port list"), "{e}");
    }
}
