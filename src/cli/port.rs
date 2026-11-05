//! `desk port list`: serial ports with their USB ids, to fill in a binding's `port = { … }`.

use crate::error::Result;
use crate::output::{Report, table};
use crate::serial;

/// Only USB ports unless `all`: Linux exposes dozens of legacy `/dev/ttyS*` ports on any machine.
pub fn list(all: bool) -> Result<Report> {
    let mut ports = serial::list()?;
    let total = ports.len();
    if !all {
        ports.retain(|p| p.usb_vid.is_some());
    }
    let hidden = total - ports.len();
    let hex = |v: Option<u16>| v.map_or_else(|| "-".to_string(), |v| format!("{v:04x}"));
    let rows: Vec<Vec<String>> = ports
        .iter()
        .map(|p| {
            vec![
                p.path.clone(),
                format!("{}:{}", hex(p.usb_vid), hex(p.usb_pid)),
                p.serial_number.clone().unwrap_or_else(|| "-".into()),
                [p.manufacturer.as_deref(), p.product.as_deref()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" "),
            ]
        })
        .collect();
    let mut human = if rows.is_empty() {
        "no USB serial ports found (is the device powered and plugged in?)\n".to_string()
    } else {
        table(&["PORT", "USB VID:PID", "SERIAL", "DEVICE"], &rows)
    };
    if let Some(p) = ports.iter().find(|p| p.usb_vid.is_some()) {
        human.push_str(&format!(
            "\nbind one in a script binding with: port = {{ usb_vid = 0x{:04x}, usb_pid = 0x{:04x} }}\n",
            p.usb_vid.unwrap_or_default(),
            p.usb_pid.unwrap_or_default()
        ));
    }
    if hidden > 0 {
        human.push_str(&format!(
            "({hidden} non-USB ports hidden; `--all` shows them)\n"
        ));
    }
    Ok(Report::new(
        &serde_json::json!({ "ports": ports, "hidden": hidden }),
        human,
    ))
}
