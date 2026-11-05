use futures_util::future::join_all;
use serde::Serialize;

use super::device::power_path;
use crate::bench::Bench;
use crate::error::{ErrorJson, Result};
use crate::model::PowerState;
use crate::output::{Report, table};

#[derive(Serialize)]
struct StatusData<'a> {
    devices: Vec<DeviceStatus<'a>>,
    providers: Vec<ProviderStatus<'a>>,
}

#[derive(Serialize)]
struct DeviceStatus<'a> {
    id: &'a str,
    name: &'a str,
    /// `null` when the device has no power path.
    power: Option<PowerState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorJson>,
}

#[derive(Serialize)]
struct ProviderStatus<'a> {
    id: &'a str,
    kind: &'a str,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorJson>,
}

/// Overview. Unreachable devices or providers are reported inline; the command itself only fails
/// on config errors.
pub async fn run(bench: &Bench) -> Result<Report> {
    let devices = &bench.config.devices;
    let providers: Vec<_> = bench.providers.ids().collect();

    let (states, tests) = futures_util::future::join(
        join_all(devices.iter().map(|d| async move {
            match d.power {
                None => None,
                Some(_) => Some(bench.power_state(d).await),
            }
        })),
        join_all(providers.iter().map(|(id, _)| bench.test_provider(id))),
    )
    .await;

    let mut dev_rows = Vec::new();
    let mut dev_data = Vec::new();
    for (d, state) in devices.iter().zip(states) {
        let (power, error, cell) = match state {
            None => (None, None, "-".to_string()),
            Some(Ok(s)) => (Some(s), None, s.to_string()),
            Some(Err(e)) => (None, Some(e.to_json()), format!("error: {e}")),
        };
        dev_rows.push(vec![d.id.clone(), d.name.clone(), cell, power_path(d)]);
        dev_data.push(DeviceStatus {
            id: &d.id,
            name: &d.name,
            power,
            error,
        });
    }

    let mut prov_rows = Vec::new();
    let mut prov_data = Vec::new();
    for ((id, cfg), test) in providers.iter().zip(tests) {
        let (ok, detail, error, cell) = match test {
            Ok(info) => (
                true,
                Some(info.detail.clone()),
                None,
                format!("ok: {}", info.detail),
            ),
            Err(e) => (false, None, Some(e.to_json()), format!("error: {e}")),
        };
        prov_rows.push(vec![id.to_string(), cfg.kind.clone(), cell]);
        prov_data.push(ProviderStatus {
            id,
            kind: &cfg.kind,
            ok,
            detail,
            error,
        });
    }

    let mut human = String::new();
    if dev_rows.is_empty() {
        human.push_str("no devices configured\n");
    } else {
        human.push_str(&table(
            &["DEVICE", "NAME", "POWER", "POWER PATH"],
            &dev_rows,
        ));
    }
    if !prov_rows.is_empty() {
        human.push('\n');
        human.push_str(&table(&["PROVIDER", "KIND", "STATUS"], &prov_rows));
    }

    Ok(Report::new(
        &StatusData {
            devices: dev_data,
            providers: prov_data,
        },
        human,
    ))
}
