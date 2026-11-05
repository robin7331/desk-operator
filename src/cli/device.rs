use serde::Serialize;

use super::DeviceCmd;
use crate::bench::Bench;
use crate::config::{Device, PowerBinding};
use crate::error::Result;
use crate::model::Capability;
use crate::output::{Report, table};

#[derive(Serialize)]
pub(super) struct DeviceView<'a> {
    id: &'a str,
    name: &'a str,
    aliases: &'a [String],
    capabilities: Vec<Capability>,
    power: Option<&'a PowerBinding>,
}

impl<'a> DeviceView<'a> {
    pub(super) fn new(d: &'a Device) -> Self {
        let mut capabilities = Vec::new();
        if d.power.is_some() {
            capabilities.push(Capability::PowerSwitch);
        }
        DeviceView {
            id: &d.id,
            name: &d.name,
            aliases: &d.aliases,
            capabilities,
            power: d.power.as_ref(),
        }
    }
}

pub(super) fn power_path(d: &Device) -> String {
    d.power
        .as_ref()
        .map_or_else(|| "(none)".to_string(), PowerBinding::describe)
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
                        power_path(d),
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
                .and_then(PowerBinding::timeout)
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
            Ok(Report::new(&serde_json::json!({ "device": view }), human))
        }
    }
}
