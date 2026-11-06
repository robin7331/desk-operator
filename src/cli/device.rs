use serde::Serialize;

use super::DeviceCmd;
use crate::bench::Bench;
use indexmap::IndexMap;

use crate::config::{Binding, Device, DeviceLimits, SOURCE};
use crate::declared::{Action, Decl, Limit};
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
    /// Every setting, value and action across the capabilities (`cap.name` where names clash),
    /// with types, units, ranges and the effective bench limit.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    settings: IndexMap<String, NameView>,
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    values: IndexMap<String, NameView>,
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    actions: IndexMap<String, ActionView>,
    /// Capability → its channels (`desk set <device>:<channel> …`).
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    channels: IndexMap<String, Vec<String>>,
    #[serde(skip_serializing_if = "DeviceLimits::is_empty")]
    limits: DeviceLimits,
}

#[derive(Serialize)]
pub(super) struct NameView {
    capability: String,
    #[serde(flatten)]
    decl: Decl,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<Limit>,
}

#[derive(Serialize)]
pub(super) struct ActionView {
    capability: String,
    #[serde(flatten)]
    action: Action,
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
        let mut view = DeviceView {
            id: &d.id,
            name: &d.name,
            aliases: &d.aliases,
            model: d.model.as_deref(),
            capabilities,
            power: d.power.as_ref(),
            source: d.source(),
            bindings,
            settings: IndexMap::new(),
            values: IndexMap::new(),
            actions: IndexMap::new(),
            channels: IndexMap::new(),
            limits: d.limits.clone(),
        };
        if let Ok(cat) = Catalog::of(d) {
            let clashes = |n: &str| {
                cat.entries
                    .iter()
                    .filter(|e| e.decls.names().any(|m| m == n))
                    .count()
                    > 1
            };
            for e in &cat.entries {
                let key = |n: &str| {
                    if clashes(n) {
                        format!("{}.{n}", e.key)
                    } else {
                        n.to_string()
                    }
                };
                for (n, decl) in &e.decls.settings {
                    view.settings.insert(
                        key(n),
                        NameView {
                            capability: e.key.clone(),
                            decl: decl.clone(),
                            limit: d.limits.get(&e.key, n, None),
                        },
                    );
                }
                for (n, decl) in &e.decls.values {
                    view.values.insert(
                        key(n),
                        NameView {
                            capability: e.key.clone(),
                            decl: decl.clone(),
                            limit: None,
                        },
                    );
                }
                for (n, action) in &e.decls.actions {
                    view.actions.insert(
                        key(n),
                        ActionView {
                            capability: e.key.clone(),
                            action: action.clone(),
                        },
                    );
                }
                if let Some(chs) = &e.decls.channels {
                    view.channels.insert(e.key.clone(), chs.clone());
                }
            }
        }
        view
    }
}

/// `setpoint (°C, 50…450, limit 380)` for `device show`.
fn describe_decl(name: &str, v: &NameView) -> String {
    let d = &v.decl;
    let mut parts: Vec<String> = Vec::new();
    if let Some(u) = &d.unit {
        parts.push(u.clone());
    }
    if !d.units.is_empty() {
        parts.push(d.units.join("|"));
    }
    if !d.choices.is_empty() {
        parts.push(d.choices.join("|"));
    }
    if d.kind == crate::declared::Type::Boolean {
        parts.push("on|off".into());
    }
    match (d.min, d.max) {
        (Some(lo), Some(hi)) => parts.push(format!("{lo}…{hi}")),
        (None, Some(hi)) => parts.push(format!("≤ {hi}")),
        (Some(lo), None) => parts.push(format!("≥ {lo}")),
        (None, None) => {}
    }
    if let Some(l) = v.limit {
        match (l.min, l.max) {
            (None, Some(hi)) => parts.push(format!("limit {hi}")),
            (lo, hi) => parts.push(format!(
                "limit {}…{}",
                lo.map_or("".into(), |x| x.to_string()),
                hi.map_or("".into(), |x| x.to_string())
            )),
        }
    }
    if parts.is_empty() {
        name.to_string()
    } else {
        format!("{name} ({})", parts.join(", "))
    }
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
            let list = |m: &IndexMap<String, NameView>| -> String {
                m.iter()
                    .map(|(n, v)| describe_decl(n, v))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            if !view.settings.is_empty() {
                human.push_str(&format!("settings:     {}\n", list(&view.settings)));
            }
            if !view.values.is_empty() {
                human.push_str(&format!("values:       {}\n", list(&view.values)));
            }
            if !view.actions.is_empty() {
                let acts: Vec<String> = view
                    .actions
                    .iter()
                    .map(|(n, a)| {
                        let args: Vec<&str> = a.action.args.keys().map(String::as_str).collect();
                        if args.is_empty() {
                            n.clone()
                        } else {
                            format!("{n} ({})", args.join(", "))
                        }
                    })
                    .collect();
                human.push_str(&format!("actions:      {}\n", acts.join(", ")));
            }
            for (cap, chs) in &view.channels {
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
