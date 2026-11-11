use serde::Serialize;

use super::DeviceCmd;
use crate::bench::Bench;
use indexmap::IndexMap;

use crate::config::{Binding, Device, DeviceLimits, SOURCE};
use crate::declared::{Action, Decl, Limit, Stream, Type};
use crate::error::Result;
use crate::ops::Catalog;
use crate::output::{Report, middle_ellipsis, table};
use crate::source::{fmt_a, fmt_v};

#[derive(Serialize)]
pub(super) struct DeviceView<'a> {
    id: &'a str,
    name: &'a str,
    aliases: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    /// `power_switch`, `voltage_source`, or the name of a declared capability.
    capabilities: Vec<String>,
    power: Option<&'a Binding>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<&'a Binding>,
    /// Bindings of declared capabilities.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    bindings: IndexMap<&'a str, &'a Binding>,
    #[serde(flatten)]
    names: Names,
    #[serde(skip_serializing_if = "DeviceLimits::is_empty")]
    limits: DeviceLimits,
}

/// Every setting, value and action across a device's capabilities (`cap.name` where names clash),
/// with types, units, ranges and the effective bench limit.
#[derive(Default, Serialize)]
pub(super) struct Names {
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub settings: IndexMap<String, NameView>,
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub values: IndexMap<String, NameView>,
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub actions: IndexMap<String, ActionView>,
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub streams: IndexMap<String, StreamView>,
    /// Capability → its channels (`desk set <device>:<channel> …`).
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub channels: IndexMap<String, Vec<String>>,
}

#[derive(Clone, Serialize)]
pub(super) struct NameView {
    pub capability: String,
    #[serde(flatten)]
    pub decl: Decl,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<Limit>,
}

#[derive(Clone, Serialize)]
pub(super) struct ActionView {
    pub capability: String,
    #[serde(flatten)]
    pub action: Action,
}

#[derive(Clone, Serialize)]
pub(super) struct StreamView {
    pub capability: String,
    #[serde(flatten)]
    pub stream: Stream,
}

impl Names {
    /// `channel`: whose bench limits to show (a channel's own limit wins).
    pub(super) fn of(d: &Device, cat: &Catalog, channel: Option<&str>) -> Names {
        let mut names = Names::default();
        for (i, e) in cat.entries.iter().enumerate() {
            // The same rule as for what desk reports.
            let key = |n: &str| cat.shown(i, n);
            for (n, decl) in &e.decls.settings {
                names.settings.insert(
                    key(n),
                    NameView {
                        capability: e.key.clone(),
                        decl: decl.clone(),
                        limit: d.limits.get(&e.key, n, channel),
                    },
                );
            }
            for (n, decl) in &e.decls.values {
                names.values.insert(
                    key(n),
                    NameView {
                        capability: e.key.clone(),
                        decl: decl.clone(),
                        limit: None,
                    },
                );
            }
            for (n, action) in &e.decls.actions {
                names.actions.insert(
                    key(n),
                    ActionView {
                        capability: e.key.clone(),
                        action: action.clone(),
                    },
                );
            }
            for (n, stream) in &e.decls.streams {
                names.streams.insert(
                    key(n),
                    StreamView {
                        capability: e.key.clone(),
                        stream: stream.clone(),
                    },
                );
            }
            if let Some(chs) = &e.decls.channels {
                names.channels.insert(e.key.clone(), chs.clone());
            }
        }
        names
    }
}

impl<'a> DeviceView<'a> {
    pub(super) fn new(d: &'a Device) -> Self {
        let mut capabilities = Vec::new();
        if d.power.is_some() {
            capabilities.push("power_switch".to_string());
        }
        let mut bindings = IndexMap::new();
        for (key, cap) in &d.caps {
            if key == SOURCE {
                capabilities.push("voltage_source".to_string());
            } else {
                capabilities.push(key.clone());
                bindings.insert(key.as_str(), &cap.binding);
            }
        }
        DeviceView {
            id: &d.id,
            name: &d.name,
            aliases: &d.aliases,
            model: d.model.as_deref(),
            capabilities,
            power: d.power.as_ref(),
            source: d.source(),
            bindings,
            names: Catalog::of(d)
                .map(|cat| Names::of(d, &cat, None))
                .unwrap_or_default(),
            limits: d.limits.clone(),
        }
    }
}

/// Text is wrapped to this many columns. Long choice lists stay readable in any terminal.
const WIDTH: usize = 80;

/// What a number may be: `0…36`, `≤ 36`, `≥ 0`.
pub(super) fn range_text(d: &Decl) -> Option<String> {
    match (d.min, d.max) {
        (Some(lo), Some(hi)) => Some(format!("{lo}…{hi}")),
        (None, Some(hi)) => Some(format!("≤ {hi}")),
        (Some(lo), None) => Some(format!("≥ {lo}")),
        (None, None) => None,
    }
}

/// A bench limit: `380` (a maximum), or `5…15`.
pub(super) fn limit_text(l: Limit) -> String {
    match (l.min, l.max) {
        (None, Some(hi)) => hi.to_string(),
        (lo, hi) => format!(
            "{}…{}",
            lo.map_or("".into(), |x| x.to_string()),
            hi.map_or("".into(), |x| x.to_string())
        ),
    }
}

/// One line about what a name takes or reports: `number in °C, 50…450, limit 380`, `a | b`.
fn summary(d: &Decl, limit: Option<Limit>) -> String {
    let mut parts = vec![match d.kind {
        Type::Number => match (&d.unit, d.units.is_empty()) {
            (Some(u), _) => format!("number in {u}"),
            (None, false) => format!("number in {}", d.units.join(" | ")),
            (None, true) => "number".to_string(),
        },
        Type::Boolean => "on | off".to_string(),
        Type::Choice => d.choices.join(" | "),
        Type::String => "text".to_string(),
    }];
    parts.extend(range_text(d));
    parts.extend(limit.map(|l| format!("limit {}", limit_text(l))));
    parts.join(", ")
}

/// Break `text` into lines of at most `width` columns, at spaces. A `|` stays with the word before
/// it, so no line of a choice list starts with one.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        match words.last_mut() {
            Some(last) if word == "|" => last.push_str(" |"),
            _ => words.push(word.to_string()),
        }
    }
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in &words {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// `label: text`, continuation lines hanging under the text.
pub(super) fn labelled(label: &str, text: &str) -> String {
    let head = format!("{label}: ");
    let hang = head.chars().count();
    let mut out = String::new();
    for (i, line) in wrap(text, WIDTH.saturating_sub(hang).max(30))
        .iter()
        .enumerate()
    {
        let pad = if i == 0 {
            head.clone()
        } else {
            " ".repeat(hang)
        };
        out.push_str(&format!("{pad}{line}\n"));
    }
    out
}

/// Aligned blocks: a name, then its lines (each wrapped) in a column of their own.
fn blocks(rows: &[(&str, Vec<String>)], indent: &str) -> String {
    let name_width = rows
        .iter()
        .map(|(n, _)| n.chars().count())
        .max()
        .unwrap_or(0);
    let hang = indent.chars().count() + name_width + 2;
    let width = WIDTH.saturating_sub(hang).max(30);
    let mut out = String::new();
    for (name, lines) in rows {
        let mut first = true;
        for line in lines.iter().flat_map(|l| wrap(l, width)) {
            if first {
                out.push_str(&format!("{indent}{name:<name_width$}  {line}\n"));
                first = false;
            } else {
                out.push_str(&format!("{}{line}\n", " ".repeat(hang)));
            }
        }
        if first {
            out.push_str(&format!("{indent}{name}\n"));
        }
    }
    out
}

/// One block per setting or value.
pub(super) fn name_blocks(names: &IndexMap<String, NameView>, indent: &str) -> String {
    let rows: Vec<(&str, Vec<String>)> = names
        .iter()
        .map(|(n, v)| {
            let mut lines = vec![summary(&v.decl, v.limit)];
            lines.extend(v.decl.doc.clone());
            (n.as_str(), lines)
        })
        .collect();
    blocks(&rows, indent)
}

/// One block per action, with what its arguments take.
pub(super) fn action_blocks(actions: &IndexMap<String, ActionView>, indent: &str) -> String {
    let rows: Vec<(&str, Vec<String>)> = actions
        .iter()
        .map(|(n, a)| {
            let mut lines: Vec<String> = a
                .action
                .args
                .iter()
                .map(|(k, d)| {
                    let mut line = format!("{k}=<{}>", summary(d, None));
                    if let Some(v) = &d.default {
                        line.push_str(&format!(", default {}", v.display(Some(d))));
                    } else if d.optional {
                        line.push_str(", optional");
                    }
                    line
                })
                .collect();
            lines.extend(a.action.doc.clone());
            if a.action.produces_file() {
                lines.push("produces a file (--output, --max-size)".into());
            }
            (n.as_str(), lines)
        })
        .collect();
    blocks(&rows, indent)
}

/// One block per stream: what its arguments take, what it delivers.
pub(super) fn stream_blocks(streams: &IndexMap<String, StreamView>, indent: &str) -> String {
    let rows: Vec<(&str, Vec<String>)> = streams
        .iter()
        .map(|(n, s)| {
            let mut lines: Vec<String> = s
                .stream
                .args
                .iter()
                .map(|(k, d)| {
                    let mut line = format!("{k}=<{}>", summary(d, None));
                    if let Some(v) = &d.default {
                        line.push_str(&format!(", default {}", v.display(Some(d))));
                    } else if d.optional {
                        line.push_str(", optional");
                    }
                    line
                })
                .collect();
            lines.extend(s.stream.doc.clone());
            lines.push(format!("delivers {} (desk stream)", s.stream.content));
            (n.as_str(), lines)
        })
        .collect();
    blocks(&rows, indent)
}

/// Max width of the power path column in `device list`; `device show` prints it in full.
const PATH_WIDTH: usize = 48;

pub(super) fn power_path(d: &Device) -> String {
    d.power
        .as_ref()
        .map_or_else(|| "(none)".to_string(), Binding::describe)
}

/// The short "via" column: provider id or script name.
pub(super) fn via(d: &Device) -> String {
    d.power
        .as_ref()
        .map_or_else(|| "-".to_string(), Binding::summary)
}

pub fn run(bench: &Bench, cmd: DeviceCmd) -> Result<Report> {
    match cmd {
        DeviceCmd::List => {
            let devices = &bench.config.devices;
            let rows: Vec<Vec<String>> = devices
                .iter()
                .map(|d| {
                    vec![
                        d.id.clone(),
                        d.name.clone(),
                        d.aliases.join(", "),
                        middle_ellipsis(&power_path(d), PATH_WIDTH),
                    ]
                })
                .collect();
            let human = if rows.is_empty() {
                format!("no devices configured in {}\n", bench.config.path.display())
            } else {
                table(&["DEVICE", "NAME", "ALIASES", "POWER PATH"], &rows)
            };
            let views: Vec<DeviceView> = devices.iter().map(DeviceView::new).collect();
            Ok(Report::new(&serde_json::json!({ "devices": views }), human))
        }
        DeviceCmd::Show { device } => {
            let d = bench.config.resolve_device(&device)?;
            let view = DeviceView::new(d);
            let caps = view.capabilities.clone();
            let timeout = d
                .power
                .as_ref()
                .and_then(Binding::timeout)
                .map(|t| format!(", timeout {}", humantime::format_duration(t)))
                .unwrap_or_default();
            let human = format!(
                "id:           {}\nname:         {}\naliases:      {}\ncapabilities: {}\npower path:   {}{timeout}\n",
                d.id,
                d.name,
                if d.aliases.is_empty() {
                    "-".into()
                } else {
                    d.aliases.join(", ")
                },
                if caps.is_empty() {
                    "-".into()
                } else {
                    caps.join(", ")
                },
                power_path(d),
            );
            let mut human = human;
            if let Some(model) = &d.model {
                human.push_str(&format!("model:        {model}\n"));
            }
            if let Some(source) = d.source() {
                human.push_str(&format!("source:       {}\n", source.describe()));
            }
            for (key, b) in &view.bindings {
                human.push_str(&format!("{:<14}{}\n", format!("{key}:"), b.describe()));
            }
            if !view.names.settings.is_empty() {
                human.push_str("settings:\n");
                human.push_str(&name_blocks(&view.names.settings, "  "));
            }
            if !view.names.values.is_empty() {
                human.push_str("values:\n");
                human.push_str(&name_blocks(&view.names.values, "  "));
            }
            if !view.names.actions.is_empty() {
                human.push_str("actions:\n");
                human.push_str(&action_blocks(&view.names.actions, "  "));
            }
            if !view.names.streams.is_empty() {
                human.push_str("streams:\n");
                human.push_str(&stream_blocks(&view.names.streams, "  "));
            }
            for (cap, chs) in &view.names.channels {
                human.push_str(&format!("channels:     {cap}: {}\n", chs.join(", ")));
            }
            for (cap, l) in &d.limits.caps {
                let parts: Vec<String> = if cap == SOURCE {
                    let s = d.limits.source(None);
                    [s.voltage.map(fmt_v), s.current.map(fmt_a)]
                        .into_iter()
                        .flatten()
                        .collect()
                } else {
                    l.all
                        .iter()
                        .map(|(n, lim)| match (lim.min, lim.max) {
                            (None, Some(hi)) => format!("{n} ≤ {hi}"),
                            (lo, hi) => format!(
                                "{n} {}…{}",
                                lo.map_or("".into(), |x| x.to_string()),
                                hi.map_or("".into(), |x| x.to_string())
                            ),
                        })
                        .collect()
                };
                human.push_str(&format!("limits:       {cap} {}\n", parts.join(", ")));
            }
            Ok(Report::new(&serde_json::json!({ "device": view }), human))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapped_lists_never_start_a_line_with_a_separator() {
        let lines = wrap("aaaa | bbbb | cccc | dddd", 9);
        assert_eq!(lines, ["aaaa |", "bbbb |", "cccc |", "dddd"]);
        assert_eq!(wrap("one two", 80), ["one two"]);
        // A word longer than the line stays whole.
        assert_eq!(wrap("x averyveryverylongword y", 5).len(), 3);
    }
}
