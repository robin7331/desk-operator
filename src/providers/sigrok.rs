//! A sigrok backend as a provider (`kind = "sigrok"`, or the implicit `sigrok`): `desk provider
//! test` runs the program, `desk provider endpoints` lists what `--scan` finds. Devices reach it
//! through a script binding's `sigrok = { … }`, never as `provider = …` (see `crate::sigrok`).

use std::time::Duration;

use async_trait::async_trait;

use super::{Endpoint, EndpointUse, Provider, ProviderInfo};
use crate::error::Result;
use crate::model::Capability;
use crate::sigrok::Backend;

pub struct SigrokProvider(pub Backend);

#[async_trait]
impl Provider for SigrokProvider {
    async fn test(&self) -> Result<ProviderInfo> {
        let b = &self.0;
        let version = b.version().await?;
        let drivers = b.drivers().await?;
        Ok(ProviderInfo {
            detail: format!(
                "{} ({version}), {}",
                b.program.display(),
                crate::sigrok::drivers_text(drivers.len())
            ),
        })
    }

    /// What `--scan` finds, with `{ driver, conn }` ready to paste. The capability doesn't apply.
    async fn endpoints(&self, _capability: Capability) -> Result<Vec<Endpoint>> {
        let found = self.0.scan(None).await?;
        Ok(found
            .into_iter()
            .map(|f| {
                let mut params = toml::Table::new();
                params.insert("driver".into(), toml::Value::String(f.driver.clone()));
                let id = match &f.conn {
                    Some(c) => {
                        params.insert("conn".into(), toml::Value::String(c.clone()));
                        format!("{}:conn={c}", f.driver)
                    }
                    None => f.driver.clone(),
                };
                Endpoint {
                    id,
                    name: Some(f.description),
                    state: None,
                    params,
                }
            })
            .collect())
    }

    /// As a script binding's `sigrok = { … }`, never as `provider = …`.
    fn endpoint_use(&self) -> EndpointUse {
        let id = &self.0.id;
        EndpointUse::Connection {
            key: "sigrok",
            not_native: "a sigrok backend is reached through a model's `sigrok = { … }` binding (a \
                         script binding with `sigrok = { driver = \"…\" }`), not `provider = …`",
            note: format!(
                "(a model gives `driver`; `conn` and `backend = \"{id}\"` go in your desk.toml)"
            ),
            none_found: format!(
                "sigrok backend `{id}` found no devices (a plain scan finds USB devices; a \
                 serial one needs its `conn`: try `desk sigrok show <driver> --conn <port>`)"
            ),
        }
    }

    /// `test` and `endpoints` run several queries, each under the backend's own `timeout`.
    fn query_ceiling(&self) -> Duration {
        Duration::from_secs(300)
    }

    /// The implicit backend is listed only when `sigrok-cli` is on PATH.
    fn present(&self) -> bool {
        self.0.found
    }
}
