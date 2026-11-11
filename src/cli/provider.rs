use serde::Serialize;

use super::ProviderCmd;
use crate::bench::Bench;
use crate::error::Result;
use crate::model::Capability;
use crate::output::{Report, table};
use crate::providers::{self, Endpoint, EndpointUse};

#[derive(Serialize)]
struct ProviderView<'a> {
    id: &'a str,
    kind: &'a str,
    /// Devices that use this provider: their power path names it, or their connections run
    /// through it (a connection backend).
    used_by: Vec<&'a str>,
    /// An implicit provider: not configured, but there (e.g. its program is installed).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    implicit: bool,
}

#[derive(Serialize)]
struct EndpointsData<'a> {
    provider: &'a str,
    /// What the endpoints are listed for (only for native bindings).
    #[serde(skip_serializing_if = "Option::is_none")]
    capability: Option<Capability>,
    /// The binding key their `params` go in: `power`, or a connection key for a connection
    /// backend.
    binding: &'static str,
    endpoints: Vec<Endpoint>,
}

pub async fn run(bench: &Bench, cmd: ProviderCmd) -> Result<Report> {
    match cmd {
        ProviderCmd::List => list(bench),
        ProviderCmd::Test { provider } => {
            // Fail with config_invalid for unknown ids before trying to connect.
            bench.providers.get(&provider)?;
            let info = bench.test_provider(&provider).await?;
            let human = format!("{provider}: ok, {}\n", info.detail);
            Ok(Report::new(
                &serde_json::json!({ "provider": provider, "detail": info.detail }),
                human,
            ))
        }
        ProviderCmd::Endpoints { provider, search } => {
            let capability = Capability::PowerSwitch;
            let usage = bench.providers.get(&provider)?.endpoint_use();
            let mut endpoints = bench.endpoints(&provider, capability).await?;
            if let Some(q) = search.as_deref().map(str::to_lowercase) {
                endpoints.retain(|e| {
                    e.id.to_lowercase().contains(&q)
                        || e.name
                            .as_deref()
                            .is_some_and(|n| n.to_lowercase().contains(&q))
                });
            }
            let (human, binding, capability) = match &usage {
                EndpointUse::Native => (
                    render_endpoints(&provider, &endpoints),
                    "power",
                    Some(capability),
                ),
                EndpointUse::Connection {
                    key,
                    note,
                    none_found,
                    ..
                } => (render_found(key, note, none_found, &endpoints), *key, None),
            };
            Ok(Report::new(
                &EndpointsData {
                    provider: &provider,
                    capability,
                    binding,
                    endpoints,
                },
                human,
            ))
        }
    }
}

/// The devices that use provider `id`.
fn users<'a>(bench: &'a Bench, id: &str) -> Vec<&'a str> {
    // A provider that can't be built still counts its native bindings.
    let natively = bench
        .providers
        .get(id)
        .map_or(true, |p| p.endpoint_use() == EndpointUse::Native);
    bench
        .config
        .devices
        .iter()
        .filter(|d| d.uses_provider(id, natively))
        .map(|d| d.id.as_str())
        .collect()
}

fn list(bench: &Bench) -> Result<Report> {
    let mut views: Vec<ProviderView> = Vec::new();
    // Providers that exist without configuration, when they are there.
    for (id, kind) in providers::implicit() {
        if !bench.providers.configs().contains_key(id)
            && bench.providers.get(id).is_ok_and(|p| p.present())
        {
            views.push(ProviderView {
                id,
                kind,
                used_by: users(bench, id),
                implicit: true,
            });
        }
    }
    views.extend(bench.providers.ids().map(|(id, cfg)| ProviderView {
        id,
        kind: &cfg.kind,
        used_by: users(bench, id),
        implicit: false,
    }));
    let rows: Vec<Vec<String>> = views
        .iter()
        .map(|v| {
            let kind = if v.implicit {
                format!("{} (implicit)", v.kind)
            } else {
                v.kind.to_string()
            };
            vec![v.id.into(), kind, v.used_by.join(", ")]
        })
        .collect();
    let human = if rows.is_empty() {
        "no providers configured\n".to_string()
    } else {
        table(&["PROVIDER", "KIND", "USED BY"], &rows)
    };
    Ok(Report::new(
        &serde_json::json!({ "providers": views }),
        human,
    ))
}

/// What a connection backend found, with the connection to paste into a script binding.
fn render_found(key: &str, note: &str, none_found: &str, endpoints: &[Endpoint]) -> String {
    let Some(first) = endpoints.first() else {
        return format!("{none_found}\n");
    };
    let rows: Vec<Vec<String>> = endpoints
        .iter()
        .map(|e| vec![e.id.clone(), e.name.clone().unwrap_or_default()])
        .collect();
    let mut out = table(&["DEVICE", "DESCRIPTION"], &rows);
    let params: Vec<String> = first
        .params
        .iter()
        .map(|(k, v)| format!("{k} = {v}"))
        .collect();
    out.push_str(&format!(
        "\nbind it in a script binding with: {key} = {{ {} }}\n{note}\n",
        params.join(", ")
    ));
    out
}

/// Suggest `--search` when a listing is longer than this.
const SEARCH_HINT_OVER: usize = 20;

fn render_endpoints(provider: &str, endpoints: &[Endpoint]) -> String {
    let Some(first) = endpoints.first() else {
        return format!("provider `{provider}` offers no matching endpoints\n");
    };
    let rows: Vec<Vec<String>> = endpoints
        .iter()
        .map(|e| {
            vec![
                e.id.clone(),
                e.name.clone().unwrap_or_default(),
                e.state.clone().unwrap_or_default(),
            ]
        })
        .collect();
    let mut out = table(&["ENDPOINT", "NAME", "STATE"], &rows);
    // With exactly one match, show its real binding; otherwise a placeholder for the id.
    let params: Vec<String> = first
        .params
        .iter()
        .map(|(k, v)| match v {
            toml::Value::String(s) if endpoints.len() > 1 && *s == first.id => {
                format!("{k} = \"<ENDPOINT>\"")
            }
            other => format!("{k} = {other}"),
        })
        .collect();
    out.push_str(&format!(
        "\nbind a device with: power = {{ provider = \"{provider}\", {} }}\n",
        params.join(", ")
    ));
    if endpoints.len() > SEARCH_HINT_OVER {
        out.push_str(&format!(
            "{} endpoints; narrow them down with: desk provider endpoints {provider} --search <text>\n",
            endpoints.len()
        ));
    }
    out
}
