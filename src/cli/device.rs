use serde::Serialize;

use super::DeviceCmd;
use crate::bench::Bench;
use crate::config::{Binding, Device, DeviceLimits};
use crate::error::Result;
use crate::model::Capability;
use crate::output::{Report, middle_ellipsis, table};
use crate::source::{fmt_a, fmt_v};

#[derive(Serialize)]
pub(super) struct DeviceView<'a> {
    id: &'a str,
    name: &'a str,
    aliases: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    capabilities: Vec<Capability>,
    power: Option<&'a Binding>,
    source: Option<&'a Binding>,
    #[serde(skip_serializing_if = "DeviceLimits::is_empty")]
    limits: DeviceLimits,
}

impl<'a> DeviceView<'a> {
    pub(super) fn new(d: &'a Device) -> Self {
        let mut capabilities = Vec::new();
        if d.power.is_some() {
            capabilities.push(Capability::PowerSwitch);
        }
        if d.source.is_some() {
            capabilities.push(Capability::VoltageSource);
        }
        DeviceView {
            id: &d.id,
            name: &d.name,
            aliases: &d.aliases,
            model: d.model.as_deref(),
            capabilities,
            power: d.power.as_ref(),
            source: d.source.as_ref(),
            limits: d.limits,
        }
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
            let caps: Vec<String> = view
                .capabilities
                .iter()
                .map(|c| {
                    serde_json::to_value(c)
                        .ok()
                        .and_then(|v| v.as_str().map(String::from))
                        .unwrap_or_default()
                })
                .collect();
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
            if let Some(source) = &d.source {
                human.push_str(&format!("source:       {}\n", source.describe()));
            }
            if !d.limits.is_empty() {
                let l = d.limits.source;
                let parts: Vec<String> = [l.voltage.map(fmt_v), l.current.map(fmt_a)]
                    .into_iter()
                    .flatten()
                    .collect();
                human.push_str(&format!("limits:       source {}\n", parts.join(", ")));
            }
            Ok(Report::new(&serde_json::json!({ "device": view }), human))
        }
    }
}
