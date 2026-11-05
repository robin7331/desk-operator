use futures_util::future::join_all;
use serde::Serialize;

use super::{PowerCmd, Targets, summarize_failures};
use crate::bench::Bench;
use crate::config::{ALL, Device, PowerBinding};
use crate::error::{DeskError, ErrorJson, Result};
use crate::model::PowerState;
use crate::output::{Report, table};
use crate::power::{Plan, Target};

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
    /// Lines logged by a script binding (`ctx.log` / `print`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    log: Vec<String>,
}

impl<'a> DeviceResult<'a> {
    fn failed(device: &'a str, e: &DeskError, log: Vec<String>) -> Self {
        DeviceResult {
            device,
            ok: false,
            power: None,
            before: None,
            changed: None,
            error: Some(e.to_json()),
            log,
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

/// `desk power off` and `desk power off all` mean every device. Switching on (or toggling)
/// everything needs the explicit `--all` flag.
fn wants_all(target: Target, t: &Targets) -> Result<bool> {
    let says_all = t.devices.iter().any(|d| d.eq_ignore_ascii_case(ALL));
    if target == Target::Off {
        return Ok(t.all || t.devices.is_empty() || says_all);
    }
    let verb = target.as_str();
    let action = match target {
        Target::Toggle => "toggle",
        _ => &format!("switch {verb}"),
    };
    if says_all {
        return Err(DeskError::Usage(format!(
            "`all` is only a shortcut for `desk power off`; name the devices to {action} \
             (or pass --all explicitly)"
        )));
    }
    if t.devices.is_empty() && !t.all {
        return Err(DeskError::Usage(format!(
            "name the devices to {action}, e.g. `desk power {verb} psu`"
        )));
    }
    Ok(t.all)
}

async fn act(bench: &Bench, target: Target, t: Targets) -> Result<Report> {
    let devices = if wants_all(target, &t)? {
        targets(bench, &[], true)?
    } else {
        targets(bench, &t.devices, false)?
    };
    if t.dry_run {
        return dry_run(bench, target, devices).await;
    }
    let outcomes = join_all(devices.iter().map(|d| bench.set_power(d, target))).await;

    let mut rows = Vec::new();
    let mut results = Vec::new();
    let mut failures = Vec::new();
    for (d, run) in devices.iter().zip(&outcomes) {
        match &run.result {
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
                    log: run.log.clone(),
                });
            }
            Err(e) => {
                rows.push(vec![d.id.clone(), "-".into(), format!("error: {e}")]);
                results.push(DeviceResult::failed(&d.id, e, run.log.clone()));
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
    for (d, run) in devices.iter().zip(&states) {
        match &run.result {
            Ok(s) => {
                rows.push(vec![d.id.clone(), d.name.clone(), s.to_string()]);
                results.push(DeviceResult {
                    device: &d.id,
                    ok: true,
                    power: Some(*s),
                    before: None,
                    changed: None,
                    error: None,
                    log: run.log.clone(),
                });
            }
            Err(e) => {
                rows.push(vec![d.id.clone(), d.name.clone(), format!("error: {e}")]);
                results.push(DeviceResult::failed(&d.id, e, run.log.clone()));
                failures.push(e);
            }
        }
    }

    let human = table(&["DEVICE", "NAME", "POWER"], &rows);
    let failure = summarize_failures(devices.len(), &failures);
    Ok(Report::new(&StatusData { devices: results }, human).with_failure(failure))
}

#[derive(Serialize)]
struct DryRunData<'a> {
    action: Target,
    dry_run: bool,
    results: Vec<PlanResult<'a>>,
}

#[derive(Serialize)]
struct PlanResult<'a> {
    device: &'a str,
    ok: bool,
    /// The resolved power binding, e.g. `ha (entity=switch.x)` or `script iron.lua`.
    power_path: String,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    plan: Option<Plan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorJson>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    log: Vec<String>,
}

async fn dry_run(bench: &Bench, target: Target, devices: Vec<&Device>) -> Result<Report> {
    let plans = join_all(devices.iter().map(|d| bench.plan_power(d, target))).await;

    let mut rows = Vec::new();
    let mut results = Vec::new();
    let mut failures = Vec::new();
    for (d, run) in devices.iter().zip(&plans) {
        let power_path = d
            .power
            .as_ref()
            .map(PowerBinding::describe)
            .unwrap_or_default();
        match &run.result {
            Ok(p) => {
                let now = p
                    .before
                    .map_or_else(|| "unknown".to_string(), |b| b.to_string());
                let would = if p.would_send {
                    let via = d
                        .power
                        .as_ref()
                        .map(PowerBinding::summary)
                        .unwrap_or_default();
                    format!("would switch {} via {via}", p.target)
                } else {
                    "nothing to do".to_string()
                };
                rows.push(vec![d.id.clone(), now, would]);
                results.push(PlanResult {
                    device: &d.id,
                    ok: true,
                    power_path,
                    plan: Some(p.clone()),
                    error: None,
                    log: run.log.clone(),
                });
            }
            Err(e) => {
                rows.push(vec![d.id.clone(), "-".into(), format!("error: {e}")]);
                results.push(PlanResult {
                    device: &d.id,
                    ok: false,
                    power_path,
                    plan: None,
                    error: Some(e.to_json()),
                    log: run.log.clone(),
                });
                failures.push(e);
            }
        }
    }

    let mut human = table(&["DEVICE", "NOW", "PLAN (dry run, nothing sent)"], &rows);
    if rows.is_empty() {
        human.clear();
    }
    let failure = summarize_failures(devices.len(), &failures);
    Ok(Report::new(
        &DryRunData {
            action: target,
            dry_run: true,
            results,
        },
        human,
    )
    .with_failure(failure))
}
