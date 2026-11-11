use serde::Serialize;

use super::ProviderCmd;
use crate::bench::Bench;
use crate::config::Binding;
use crate::error::Result;
use crate::model::Capability;
use crate::output::{Report, table};
use crate::providers::Endpoint;
use crate::sigrok;

#[derive(Serialize)]
struct ProviderView<'a> {
    id: &'a str,
    kind: &'a str,
    /// Devices whose power path uses this provider (a sigrok backend: whose `sigrok` binding
    /// names it).
    used_by: Vec<&'a str>,
    /// The implicit sigrok backend: not configured, `sigrok-cli` is on PATH.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    implicit: bool,
}

#[derive(Serialize)]
struct EndpointsData<'a> {
    provider: &'a str,
    /// What the endpoints are listed for (not for a sigrok backend).
    #[serde(skip_serializing_if = "Option::is_none")]
    capability: Option<Capability>,
    /// The binding key their `params` go in: `power`, or `sigrok` for a sigrok backend.
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
            let is_sigrok = bench.providers.kind(&provider) == Some(sigrok::KIND);
            let mut endpoints = bench.endpoints(&provider, capability).await?;
            if let Some(q) = search.as_deref().map(str::to_lowercase) {
                endpoints.retain(|e| {
                    e.id.to_lowercase().contains(&q)
                        || e.name
                            .as_deref()
                            .is_some_and(|n| n.to_lowercase().contains(&q))
                });
            }
            let human = if is_sigrok {
                render_scan(&provider, &endpoints)
            } else {
                render_endpoints(&provider, &endpoints)
            };
            Ok(Report::new(
                &EndpointsData {
                    provider: &provider,
                    capability: (!is_sigrok).then_some(capability),
                    binding: if is_sigrok { "sigrok" } else { "power" },
                    endpoints,
                },
                human,
            ))
        }
    }
}

/// Devices whose `sigrok` binding names `backend`.
fn sigrok_users<'a>(bench: &'a Bench, backend: &str) -> Vec<&'a str> {
    bench
        .config
        .devices
        .iter()
        .filter(|d| {
            d.caps.values().any(|c| {
                matches!(&c.binding, Binding::Script { sigrok: Some(s), .. }
                    if s.backend.as_deref() == Some(backend))
            })
        })
        .map(|d| d.id.as_str())
        .collect()
}

fn list(bench: &Bench) -> Result<Report> {
    let mut views: Vec<ProviderView> = Vec::new();
    // `sigrok-cli` on PATH is a backend without configuration.
    if bench.providers.kind(sigrok::IMPLICIT) == Some(sigrok::KIND)
        && !bench.providers.configs().contains_key(sigrok::IMPLICIT)
        && sigrok::Backend::implicit().found
    {
        views.push(ProviderView {
            id: sigrok::IMPLICIT,
            kind: sigrok::KIND,
            used_by: sigrok_users(bench, sigrok::IMPLICIT),
            implicit: true,
        });
    }
    views.extend(bench.providers.ids().map(|(id, cfg)| ProviderView {
        id,
        kind: &cfg.kind,
        used_by: if cfg.kind == sigrok::KIND {
            sigrok_users(bench, id)
        } else {
            bench
                .config
                .devices
                .iter()
                .filter(|d| {
                    matches!(&d.power, Some(Binding::Native { provider, .. }) if provider == id)
                })
                .map(|d| d.id.as_str())
                .collect()
        },
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

/// What a sigrok backend's scan found, with the binding to paste.
fn render_scan(provider: &str, endpoints: &[Endpoint]) -> String {
    let Some(first) = endpoints.first() else {
        return format!(
            "sigrok backend `{provider}` found no devices (a plain scan finds USB devices; a \
             serial one needs its `conn`: try `desk sigrok show <driver> --conn <port>`)\n"
        );
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
        "\nbind it in a script binding with: sigrok = {{ {} }}\n(a model gives `driver`; \
         `conn` and `backend = \"{provider}\"` go in your desk.toml)\n",
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
