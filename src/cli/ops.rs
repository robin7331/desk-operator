//! `desk get`, `desk set`, `desk do`: values, settings and actions of any device.

use std::path::PathBuf;

use serde::Serialize;

use super::device::{self, ActionView, NameView, Names, StreamView};
use crate::bench::Bench;
use crate::config::Device;
use crate::declared::{Type, Val};
use crate::error::{DeskError, ErrorJson, Result};
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
    if pairs.is_empty() {
        return list_settings(device, &cat, target, channel);
    }
    if let (true, Some(last)) = (!pairs.len().is_multiple_of(2), pairs.last()) {
        let given = &pairs[..pairs.len() - 1];
        return describe_setting(bench, device, &cat, target, channel, given, last).await;
    }
    let op = ops::plan_set(&cat, device, channel, pairs, clear_protection, dry_run)?;
    finish(bench, device, &cat, op, None).await
}

/// `desk do … --output … --max-size …` (only for actions that produce a file).
pub struct CaptureArgs {
    /// Absolute (the CLI made it so before forwarding it to the daemon).
    pub output: Option<PathBuf>,
    pub max_size: Option<String>,
}

pub async fn act(
    bench: &Bench,
    target: &str,
    action: Option<&str>,
    args: &[String],
    dry_run: bool,
    capture: CaptureArgs,
) -> Result<Report> {
    let (query, channel) = split_channel(target);
    let device = bench.config.resolve_device(query)?;
    let cat = Catalog::of(device)?;
    let Some(action) = action else {
        return list_actions(device, &cat, target, channel);
    };
    let decl = cat.action(action)?.2;
    if args.is_empty() && decl.needs_args() {
        return describe_action(device, &cat, target, channel, action);
    }
    let plan = if decl.produces_file() {
        let max_size = match &capture.max_size {
            Some(s) => crate::capture::parse_size(s)
                .map_err(|e| DeskError::Usage(format!("--max-size: {e}")))?,
            None => bench.config.captures.max_size,
        };
        Some(crate::capture::Plan {
            dir: crate::capture::base_dir(&bench.config)?.join(&device.id),
            output: capture.output,
            max_size,
            cancel: bench.cancel.clone(),
        })
    } else if capture.output.is_some() || capture.max_size.is_some() {
        return Err(DeskError::Usage(format!(
            "`{action}` doesn't produce a file, so --output and --max-size don't apply"
        )));
    } else {
        None
    };
    let capturing = plan.is_some() && !dry_run;
    let op = ops::plan_do(&cat, channel, action, args, dry_run, plan)?;
    // Run directly (not in the daemon): Ctrl-C stops the capture and removes the partial file.
    if capturing && bench.sessions.is_none() {
        let cancel = bench.cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                cancel.cancel();
            }
        });
    }
    finish(bench, device, &cat, op, None).await
}

// ---------------------------------------------------------------------------------------------
// Left-out arguments: say what's possible. Still a usage error, with nothing changed: a script's
// `desk set psu voltage $EMPTY && desk set psu output on` must not carry on as if it had worked.

/// A listing in place of a command: `data` and the text, plus the usage error that ends it.
fn listing(data: &impl Serialize, human: String, message: String) -> Report {
    let error = DeskError::Usage(message);
    let human = format!("{human}{error}\n");
    Report::new(data, human).with_failure(Some((error.to_json(), error.exit_code())))
}

/// How to address the device in a suggested command: `iron`, `relays:<channel>`.
fn address(target: &str, channel: Option<&str>, has_channels: bool) -> String {
    if has_channels && channel.is_none() {
        format!("{target}:<channel>")
    } else {
        target.to_string()
    }
}

fn channels_text(names: &Names) -> String {
    names
        .channels
        .iter()
        .map(|(cap, chs)| device::labelled(&format!("channels of {cap}"), &chs.join(", ")))
        .collect()
}

#[derive(Serialize)]
struct SettingsList<'a> {
    device: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    channel: Option<&'a str>,
    settings: &'a indexmap::IndexMap<String, NameView>,
    #[serde(skip_serializing_if = "indexmap::IndexMap::is_empty")]
    channels: &'a indexmap::IndexMap<String, Vec<String>>,
}

/// `desk set <device>`: every setting and what it takes.
fn list_settings(
    device: &Device,
    cat: &Catalog,
    target: &str,
    channel: Option<&str>,
) -> Result<Report> {
    if let Some(ch) = channel {
        cat.check_any_channel(ch)?;
    }
    let names = Names::of(device, cat, channel);
    if names.settings.is_empty() {
        return Err(DeskError::Usage(format!(
            "device `{}` has no settings (`desk device show {}` shows what it has)",
            device.id, device.id
        )));
    }
    let human = device::name_blocks(&names.settings, "") + &channels_text(&names);
    let has_channels = names
        .settings
        .values()
        .any(|v| names.channels.contains_key(&v.capability));
    let message = format!(
        "name a setting and its value (nothing was set): desk set {} <setting> <value>",
        address(target, channel, has_channels)
    );
    let data = SettingsList {
        device: &device.id,
        channel,
        settings: &names.settings,
        channels: &names.channels,
    };
    Ok(listing(&data, human, message))
}

#[derive(Serialize)]
struct SettingHelp<'a> {
    device: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    channel: Option<&'a str>,
    setting: &'a str,
    #[serde(flatten)]
    view: NameView,
    /// The current value, if it could be read (a read changes nothing).
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<Val>,
    /// Why it couldn't: the device is off or unreachable, say.
    #[serde(skip_serializing_if = "Option::is_none")]
    value_error: Option<ErrorJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    channels: Option<&'a [String]>,
}

/// `desk set <device> <setting>`: what it takes and, if the device can tell, what it's set to.
async fn describe_setting(
    bench: &Bench,
    device: &Device,
    cat: &Catalog,
    target: &str,
    channel: Option<&str>,
    given: &[String],
    query: &str,
) -> Result<Report> {
    let (i, name, decl) = cat.setting(query)?;
    cat.check_channel(i, channel, false)?;
    let e = &cat.entries[i];
    let channels = e.decls.channels.as_deref();
    let view = NameView {
        capability: e.key.clone(),
        decl: decl.clone(),
        limit: device.limits.get(&e.key, &name, channel),
    };
    // Which channel's value? Without one there's no single answer.
    let read = match (channels, channel) {
        (Some(_), None) => None,
        _ => Some(read_setting(bench, device, cat, channel, &name).await),
    };
    let current = match &read {
        None => "address a channel to read it".to_string(),
        Some(Ok(Some(v))) => format!("{} (current)", v.display(Some(decl))),
        Some(Ok(None)) => "the device doesn't report it".to_string(),
        Some(Err(e)) => format!("not read ({e})"),
    };
    let mut human = format!("{name}: {current}\n");
    match decl.kind {
        Type::Choice => human.push_str(&device::labelled("choices", &decl.choices.join(", "))),
        Type::Boolean => human.push_str("choices: on, off\n"),
        Type::String => human.push_str("type: text\n"),
        Type::Number => {
            if let Some(u) = &decl.unit {
                human.push_str(&format!("unit: {u}\n"));
            }
            if let Some(r) = device::range_text(decl) {
                human.push_str(&format!("range: {r}\n"));
            }
            if let Some(l) = view.limit {
                human.push_str(&format!("bench limit: {}\n", device::limit_text(l)));
            }
        }
    }
    if let Some(doc) = &decl.doc {
        human.push_str(&device::labelled("about", doc));
    }
    if let Some(chs) = channels {
        human.push_str(&device::labelled("channels", &chs.join(", ")));
    }
    let command: Vec<&str> = given
        .iter()
        .map(String::as_str)
        .chain([query, "<value>"])
        .collect();
    let message = format!(
        "`{name}` needs a value (nothing was set): desk set {} {}",
        address(target, channel, channels.is_some()),
        command.join(" ")
    );
    let (value, value_error) = match read {
        Some(Ok(v)) => (v, None),
        Some(Err(e)) => (None, Some(e.to_json())),
        None => (None, None),
    };
    let data = SettingHelp {
        device: &device.id,
        channel,
        setting: &name,
        view,
        value,
        value_error,
        channels,
    };
    Ok(listing(&data, human, message))
}

async fn read_setting(
    bench: &Bench,
    device: &Device,
    cat: &Catalog,
    channel: Option<&str>,
    name: &str,
) -> Result<Option<Val>> {
    let (op, _) = ops::plan_get(cat, channel, &[])?;
    let report = bench.run(device, cat, op).await.result?;
    Ok(report.outcome.readings.values.get(name).cloned())
}

#[derive(Serialize)]
struct ActionsList<'a> {
    device: &'a str,
    actions: &'a indexmap::IndexMap<String, ActionView>,
}

/// `desk do <device>`: every action and its arguments.
fn list_actions(
    device: &Device,
    cat: &Catalog,
    target: &str,
    channel: Option<&str>,
) -> Result<Report> {
    if let Some(ch) = channel {
        cat.check_any_channel(ch)?;
    }
    let names = Names::of(device, cat, channel);
    if names.actions.is_empty() {
        return Err(DeskError::Usage(format!(
            "device `{}` has no actions (`desk device show {}` shows what it has)",
            device.id, device.id
        )));
    }
    let human = device::action_blocks(&names.actions, "") + &channels_text(&names);
    let has_channels = names
        .actions
        .values()
        .any(|a| names.channels.contains_key(&a.capability));
    let message = format!(
        "name an action (nothing was done): desk do {} <action> [<arg>=<value>...]",
        address(target, channel, has_channels)
    );
    let data = ActionsList {
        device: &device.id,
        actions: &names.actions,
    };
    Ok(listing(&data, human, message))
}

/// `desk do <device> <action>` when the action needs arguments: what they take.
fn describe_action(
    device: &Device,
    cat: &Catalog,
    target: &str,
    channel: Option<&str>,
    query: &str,
) -> Result<Report> {
    let (i, name, action) = cat.action(query)?;
    cat.check_channel(i, channel, false)?;
    let e = &cat.entries[i];
    let view = ActionView {
        capability: e.key.clone(),
        action: action.clone(),
    };
    let mut one = indexmap::IndexMap::new();
    one.insert(name.clone(), view);
    let human = device::action_blocks(&one, "");
    let required = || action.args.iter().filter(|(_, d)| d.required());
    let needs: Vec<String> = required().map(|(k, _)| format!("`{k}=…`")).collect();
    let args: Vec<String> = required().map(|(k, _)| format!("{k}=<value>")).collect();
    let message = format!(
        "`{name}` needs {} (nothing was done): desk do {} {query} {}",
        needs.join(", "),
        address(target, channel, e.decls.channels.is_some()),
        args.join(" ")
    );
    let data = ActionsList {
        device: &device.id,
        actions: &one,
    };
    Ok(listing(&data, human, message))
}

#[derive(Serialize)]
struct StreamsList<'a> {
    device: &'a str,
    streams: &'a indexmap::IndexMap<String, StreamView>,
}

/// `desk stream <device>`: every stream and its arguments.
pub(super) fn list_streams(
    device: &Device,
    cat: &Catalog,
    target: &str,
    channel: Option<&str>,
) -> Result<Report> {
    if let Some(ch) = channel {
        cat.check_any_channel(ch)?;
    }
    let names = Names::of(device, cat, channel);
    if names.streams.is_empty() {
        return Err(DeskError::Usage(format!(
            "device `{}` has no streams (`desk device show {}` shows what it has)",
            device.id, device.id
        )));
    }
    let human = device::stream_blocks(&names.streams, "") + &channels_text(&names);
    let has_channels = names
        .streams
        .values()
        .any(|s| names.channels.contains_key(&s.capability));
    let message = format!(
        "name a stream (nothing was started): desk stream {} <stream> [<arg>=<value>...]",
        address(target, channel, has_channels)
    );
    let data = StreamsList {
        device: &device.id,
        streams: &names.streams,
    };
    Ok(listing(&data, human, message))
}

/// `desk stream <device> <stream>` when the stream needs arguments: what they take.
pub(super) fn describe_stream(
    device: &Device,
    cat: &Catalog,
    target: &str,
    channel: Option<&str>,
    query: &str,
) -> Result<Report> {
    let (i, name, stream) = cat.stream(query)?;
    cat.check_channel(i, channel, false)?;
    let e = &cat.entries[i];
    let mut one = indexmap::IndexMap::new();
    one.insert(
        name.clone(),
        StreamView {
            capability: e.key.clone(),
            stream: stream.clone(),
        },
    );
    let human = device::stream_blocks(&one, "");
    let required = || stream.args.iter().filter(|(_, d)| d.required());
    let needs: Vec<String> = required().map(|(k, _)| format!("`{k}=…`")).collect();
    let args: Vec<String> = required().map(|(k, _)| format!("{k}=<value>")).collect();
    let message = format!(
        "`{name}` needs {} (nothing was started): desk stream {} {query} {}",
        needs.join(", "),
        address(target, channel, e.decls.channels.is_some()),
        args.join(" ")
    );
    let data = StreamsList {
        device: &device.id,
        streams: &one,
    };
    Ok(listing(&data, human, message))
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
            if let Some(p) = &r.planned {
                out.push_str(&format!(
                    "the file would go to {} (at most {})\n",
                    p.into.display(),
                    crate::capture::format_size(p.max_size)
                ));
            }
        } else {
            out.push_str(&format!("done: {call}\n"));
        }
    }
    if let Some(c) = &r.outcome.capture {
        out.push_str(&format!(
            "file: {} ({}, {})\n",
            c.file.display(),
            crate::capture::format_size(c.bytes),
            c.format
        ));
    }
    for w in &r.outcome.warnings {
        out.push_str(&format!("warning: {w}\n"));
    }
    out
}
