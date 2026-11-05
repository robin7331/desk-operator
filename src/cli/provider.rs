use serde::Serialize;

use super::ProviderCmd;
use crate::bench::Bench;
use crate::config::PowerBinding;
use crate::error::Result;
use crate::model::Capability;
use crate::output::{Report, table};
use crate::providers::Endpoint;

#[derive(Serialize)]
struct ProviderView<'a> {
    id: &'a str,
    kind: &'a str,
    /// Devices whose power path uses this provider.
    used_by: Vec<&'a str>,
}

#[derive(Serialize)]
struct EndpointsData<'a> {
    provider: &'a str,
    capability: Capability,
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
            let mut endpoints = bench.endpoints(&provider, capability).await?;
            if let Some(q) = search.as_deref().map(str::to_lowercase) {
                endpoints.retain(|e| {
                    e.id.to_lowercase().contains(&q)
                        || e.name
                            .as_deref()
                            .is_some_and(|n| n.to_lowercase().contains(&q))
                });
            }
            let human = render_endpoints(&provider, &endpoints);
            Ok(Report::new(
                &EndpointsData {
                    provider: &provider,
                    capability,
                    endpoints,
                },
                human,
            ))
        }
    }
}

fn list(bench: &Bench) -> Result<Report> {
    let views: Vec<ProviderView> = bench
        .providers
        .ids()
        .map(|(id, cfg)| ProviderView {
            id,
            kind: &cfg.kind,
            used_by: bench
                .config
                .devices
                .iter()
                .filter(|d| {
                    matches!(&d.power, Some(PowerBinding::Native { provider, .. }) if provider == id)
                })
                .map(|d| d.id.as_str())
                .collect(),
        })
        .collect();
    let rows: Vec<Vec<String>> = views
        .iter()
        .map(|v| vec![v.id.into(), v.kind.into(), v.used_by.join(", ")])
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
