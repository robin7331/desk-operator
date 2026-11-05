use futures_util::future::join_all;
use serde::Serialize;

use super::{PowerCmd, Targets, summarize_failures};
use crate::bench::Bench;
use crate::config::Device;
use crate::error::{DeskError, ErrorJson, Result};
use crate::model::PowerState;
use crate::output::{Report, table};
use crate::power::Target;

#[derive(Serialize)]
struct ActionData<'a> {
    action: Target,
    results: Vec<DeviceResult<'a>>,
}

#[derive(Serialize)]
struct StatusData<'a> {
    devices: Vec<DeviceResult<'a>>,
}

#[derive(Serialize)]
struct DeviceResult<'a> {
    device: &'a str,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    power: Option<PowerState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    before: Option<PowerState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    changed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorJson>,
}

impl<'a> DeviceResult<'a> {
    fn failed(device: &'a str, e: &DeskError) -> Self {
        DeviceResult {
            device,
            ok: false,
            power: None,
            before: None,
            changed: None,
            error: Some(e.to_json()),
        }
    }
}

pub async fn run(bench: &Bench, cmd: PowerCmd) -> Result<Report> {
    match cmd {
        PowerCmd::On(t) => act(bench, Target::On, t).await,
        PowerCmd::Off(t) => act(bench, Target::Off, t).await,
        PowerCmd::Toggle(t) => act(bench, Target::Toggle, t).await,
        PowerCmd::Status { devices } => status(bench, devices).await,
    }
}

/// Resolve targets and make sure all of them have a power path, before touching anything.
fn targets<'b>(bench: &'b Bench, queries: &[String], all: bool) -> Result<Vec<&'b Device>> {
    let devices = if all || queries.is_empty() {
        let d = bench.powered_devices();
        if d.is_empty() {
            return Err(DeskError::Usage(
                "no device has a power path configured".into(),
            ));
        }
        d
    } else {
        bench.resolve_all(queries)?
    };
    if let Some(d) = devices.iter().find(|d| d.power.is_none()) {
        return Err(DeskError::NoPowerPath(d.id.clone()));
    }
    Ok(devices)
}

async fn act(bench: &Bench, target: Target, t: Targets) -> Result<Report> {
    let devices = targets(bench, &t.devices, t.all)?;
    let outcomes = join_all(devices.iter().map(|d| bench.set_power(d, target))).await;

    let mut rows = Vec::new();
    let mut results = Vec::new();
    let mut failures = Vec::new();
    for (d, outcome) in devices.iter().zip(&outcomes) {
        match outcome {
            Ok(o) => {
                let note = match (o.changed, o.before) {
                    (false, _) => format!("already {}", o.after.state),
                    (true, Some(b)) => format!("switched (was {})", b.state),
                    (true, None) => "command sent, state not read back".to_string(),
                };
                rows.push(vec![d.id.clone(), o.after.to_string(), note]);
                results.push(DeviceResult {
                    device: &d.id,
                    ok: true,
                    power: Some(o.after),
                    before: o.before,
                    changed: Some(o.changed),
                    error: None,
                });
            }
            Err(e) => {
                rows.push(vec![d.id.clone(), "-".into(), format!("error: {e}")]);
                results.push(DeviceResult::failed(&d.id, e));
                failures.push(e);
            }
        }
    }

    let human = table(&["DEVICE", "POWER", "RESULT"], &rows);
    let failure = summarize_failures(devices.len(), &failures);
    Ok(Report::new(
        &ActionData {
            action: target,
            results,
        },
        human,
    )
    .with_failure(failure))
}

async fn status(bench: &Bench, queries: Vec<String>) -> Result<Report> {
    let devices = targets(bench, &queries, false)?;
    let states = join_all(devices.iter().map(|d| bench.power_state(d))).await;

    let mut rows = Vec::new();
    let mut results = Vec::new();
    let mut failures = Vec::new();
    for (d, state) in devices.iter().zip(&states) {
        match state {
            Ok(s) => {
                rows.push(vec![d.id.clone(), d.name.clone(), s.to_string()]);
                results.push(DeviceResult {
                    device: &d.id,
                    ok: true,
                    power: Some(*s),
                    before: None,
                    changed: None,
                    error: None,
                });
            }
            Err(e) => {
                rows.push(vec![d.id.clone(), d.name.clone(), format!("error: {e}")]);
                results.push(DeviceResult::failed(&d.id, e));
                failures.push(e);
            }
        }
    }

    let human = table(&["DEVICE", "NAME", "POWER"], &rows);
    let failure = summarize_failures(devices.len(), &failures);
    Ok(Report::new(&StatusData { devices: results }, human).with_failure(failure))
}
