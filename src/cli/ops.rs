//! `desk get`, `desk set`, `desk do`: values, settings and actions of any device.

use serde::Serialize;

use crate::bench::Bench;
use crate::declared::Val;
use crate::error::Result;
use crate::ops::{self, Catalog, DeviceReadings, Op, split_channel};
use crate::output::Report;

#[derive(Serialize)]
struct Data<'a> {
    #[serde(flatten)]
    report: &'a ops::Report,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    log: Vec<String>,
}

pub async fn get(bench: &Bench, target: &str, names: &[String]) -> Result<Report> {
    let (query, channel) = split_channel(target);
    let device = bench.config.resolve_device(query)?;
    let cat = Catalog::of(device)?;
    let (op, filter) = ops::plan_get(&cat, channel, names)?;
    finish(bench, device, &cat, op, filter.as_deref()).await
}

pub async fn set(
    bench: &Bench,
    target: &str,
    pairs: &[String],
    clear_protection: bool,
    dry_run: bool,
) -> Result<Report> {
    let (query, channel) = split_channel(target);
    let device = bench.config.resolve_device(query)?;
    let cat = Catalog::of(device)?;
    let op = ops::plan_set(&cat, device, channel, pairs, clear_protection, dry_run)?;
    finish(bench, device, &cat, op, None).await
}

pub async fn act(
    bench: &Bench,
    target: &str,
    action: &str,
    args: &[String],
    dry_run: bool,
) -> Result<Report> {
    let (query, channel) = split_channel(target);
    let device = bench.config.resolve_device(query)?;
    let cat = Catalog::of(device)?;
    let op = ops::plan_do(&cat, channel, action, args, dry_run)?;
    finish(bench, device, &cat, op, None).await
}

async fn finish(
    bench: &Bench,
    device: &crate::config::Device,
    cat: &Catalog,
    op: Op,
    filter: Option<&[String]>,
) -> Result<Report> {
    let dry_run = matches!(
        op,
        Op::Set { dry_run: true, .. } | Op::Do { dry_run: true, .. }
    );
    let run = bench.run(device, cat, op).await;
    let mut report = match run.result {
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
    if let Some(names) = filter {
        report.outcome.readings = report.outcome.readings.filtered(names);
    }
    let human = render(cat, &report, dry_run);
    Ok(Report::new(
        &Data {
            report: &report,
            log: run.log,
        },
        human,
    ))
}

fn show(cat: &Catalog, name: &str, v: &Val) -> String {
    v.display(cat.readable_decl(name))
}

/// One `name  value` block, aligned.
pub fn lines(cat: &Catalog, r: &DeviceReadings, indent: &str) -> String {
    let mut out = String::new();
    let width = r
        .flat()
        .iter()
        .map(|(_, n, _)| n.chars().count())
        .max()
        .unwrap_or(0);
    for (n, v) in &r.values {
        out.push_str(&format!("{indent}{n:<width$}  {}\n", show(cat, n, v)));
    }
    for (ch, vals) in &r.channels {
        out.push_str(&format!("{indent}channel {ch}:\n"));
        for (n, v) in vals {
            out.push_str(&format!("{indent}  {n:<width$}  {}\n", show(cat, n, v)));
        }
    }
    out
}

fn render(cat: &Catalog, r: &ops::Report, dry_run: bool) -> String {
    let mut out = String::new();
    let mut head = r.device.clone();
    if let Some(ch) = &r.channel {
        head.push_str(&format!(":{ch}"));
    }
    if let Some(m) = r.mains {
        head.push_str(&format!("  (mains {m})"));
    }
    out.push_str(&head);
    out.push('\n');
    out.push_str(&lines(cat, &r.outcome.readings, "  "));

    if !r.requested.is_empty() {
        let parts: Vec<String> = r
            .requested
            .iter()
            .map(|(n, v)| format!("{n} {}", show(cat, n, v)))
            .collect();
        if dry_run {
            out.push_str(&format!(
                "dry run, nothing sent: would set {}\n",
                parts.join(", ")
            ));
        } else {
            let notes: Vec<String> = r
                .requested
                .keys()
                .map(|n| match r.outcome.verified.get(n) {
                    Some(true) => format!("{n} confirmed"),
                    _ => format!("{n} NOT confirmed (the device can't report it)"),
                })
                .collect();
            out.push_str(&format!("set {}: {}\n", parts.join(", "), notes.join(", ")));
        }
    }
    if let Some(a) = &r.action {
        let decl = |k: &str| {
            cat.entries
                .iter()
                .find_map(|e| e.decls.actions.get(a))
                .and_then(|act| act.args.get(k))
        };
        let args: Vec<String> = r
            .args
            .iter()
            .map(|(k, v)| format!("{k}={}", v.display(decl(k))))
            .collect();
        let call = if args.is_empty() {
            a.clone()
        } else {
            format!("{a} {}", args.join(" "))
        };
        if dry_run {
            out.push_str(&format!("dry run, nothing sent: would do {call}\n"));
        } else {
            out.push_str(&format!("done: {call}\n"));
        }
    }
    for w in &r.outcome.warnings {
        out.push_str(&format!("warning: {w}\n"));
    }
    out
}
