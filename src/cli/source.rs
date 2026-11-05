//! `desk source …`: bench power supplies (the `voltage_source` capability).

use serde::Serialize;

use super::{OnOff, SourceCmd};
use crate::bench::Bench;
use crate::error::{DeskError, Result};
use crate::output::{Report, table};
use crate::source::{Reading, Report as SourceReport, Request, fmt_a, fmt_v, fmt_w};

#[derive(Serialize)]
struct Data<'a> {
    device: &'a str,
    #[serde(flatten)]
    report: &'a SourceReport,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    log: Vec<String>,
}

pub async fn run(bench: &Bench, cmd: SourceCmd) -> Result<Report> {
    let (device, req, dry_run) = match cmd {
        SourceCmd::Set {
            device,
            values,
            on,
            dry_run,
        } => {
            let mut req = parse_values(&values)?;
            if on {
                req.output = Some(true);
            }
            (device, req, dry_run)
        }
        SourceCmd::Output {
            state,
            device,
            dry_run,
        } => (
            device,
            Request {
                output: Some(matches!(state, OnOff::On)),
                ..Request::default()
            },
            dry_run,
        ),
        SourceCmd::Read { device } => (device, Request::default(), false),
    };

    let device = bench.config.resolve_device(&device)?;
    let run = bench.source(device, req, dry_run).await;
    let report = match run.result {
        Ok(r) => r,
        // Keep the driver's log with the error: that's when it matters most.
        Err(e) => {
            return Ok(Report::new(
                &serde_json::json!({ "device": device.id, "log": run.log }),
                String::new(),
            )
            .with_failure(Some((e.to_json(), e.exit_code()))));
        }
    };
    let human = render(&device.id, &report);
    Ok(Report::new(
        &Data {
            device: &device.id,
            report: &report,
            log: run.log,
        },
        human,
    ))
}

/// `5v`, `5.0V`, `500mv`, `1a`, `250mA`. Units are required, so a bare number is never guessed.
pub fn parse_values(values: &[String]) -> Result<Request> {
    let mut req = Request::default();
    for raw in values {
        let s = raw.trim().to_lowercase();
        let (number, scale, is_voltage) = if let Some(n) = s.strip_suffix("mv") {
            (n, 1e-3, true)
        } else if let Some(n) = s.strip_suffix("ma") {
            (n, 1e-3, false)
        } else if let Some(n) = s.strip_suffix('v') {
            (n, 1.0, true)
        } else if let Some(n) = s.strip_suffix('a') {
            (n, 1.0, false)
        } else {
            return Err(DeskError::Usage(format!(
                "`{raw}` needs a unit: e.g. 5v, 500mv, 1a or 250ma"
            )));
        };
        let value: f64 = number
            .trim()
            .parse()
            .map_err(|_| DeskError::Usage(format!("`{raw}` is not a number with a unit")))?;
        let slot = if is_voltage {
            &mut req.voltage
        } else {
            &mut req.current
        };
        if slot.is_some() {
            let what = if is_voltage { "voltage" } else { "current" };
            return Err(DeskError::Usage(format!("{what} given twice")));
        }
        *slot = Some(value * scale);
    }
    Ok(req)
}

fn opt(v: Option<f64>, f: fn(f64) -> String) -> String {
    v.map_or_else(|| "?".to_string(), f)
}

fn render(device: &str, r: &SourceReport) -> String {
    let Reading {
        output,
        voltage_set,
        current_set,
        voltage,
        current,
        power,
    } = r.reading;
    let output = match output {
        Some(true) => "on",
        Some(false) => "off",
        None => "?",
    };
    let set = format!("{}  {}", opt(voltage_set, fmt_v), opt(current_set, fmt_a));
    let measured = format!(
        "{}  {}  {}",
        opt(voltage, fmt_v),
        opt(current, fmt_a),
        opt(power, fmt_w)
    );
    let mut out = table(
        &["DEVICE", "OUTPUT", "SET", "MEASURED"],
        &[vec![device.to_string(), output.into(), set, measured]],
    );

    let req = r.requested;
    if !req.is_empty() {
        let mut parts = Vec::new();
        if let Some(v) = req.voltage {
            parts.push(fmt_v(v));
        }
        if let Some(a) = req.current {
            parts.push(fmt_a(a));
        }
        if let Some(o) = req.output {
            parts.push(format!("output {}", if o { "on" } else { "off" }));
        }
        if r.applied {
            let status = |name: &str, requested: bool, v: Option<bool>| match (requested, v) {
                (false, _) => None,
                (true, Some(_)) => Some(format!("{name} confirmed")),
                (true, None) => Some(format!("{name} NOT confirmed (the device can't report it)")),
            };
            let notes: Vec<String> = [
                status("voltage", req.voltage.is_some(), r.verified.voltage),
                status("current", req.current.is_some(), r.verified.current),
                status("output", req.output.is_some(), r.verified.output),
            ]
            .into_iter()
            .flatten()
            .collect();
            out.push_str(&format!("set {}: {}\n", parts.join(", "), notes.join(", ")));
        } else {
            out.push_str(&format!(
                "dry run, nothing sent: would set {}\n",
                parts.join(", ")
            ));
        }
    }
    for w in &r.warnings {
        out.push_str(&format!("warning: {w}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(v: &[&str]) -> Result<Request> {
        parse_values(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn parses_units() {
        let r = p(&["5v", "250mA"]).unwrap();
        assert_eq!(r.voltage, Some(5.0));
        assert_eq!(r.current, Some(0.25));
        let r = p(&["3.3V"]).unwrap();
        assert_eq!(r.voltage, Some(3.3));
        let r = p(&["1500mv", "1A"]).unwrap();
        assert!((r.voltage.unwrap() - 1.5).abs() < 1e-12);
        assert_eq!(r.current, Some(1.0));
    }

    #[test]
    fn rejects_ambiguous_input() {
        assert!(p(&["5"]).unwrap_err().to_string().contains("needs a unit"));
        assert!(p(&["fivev"]).is_err());
        assert!(p(&["5v", "6v"]).unwrap_err().to_string().contains("twice"));
    }
}
