//! A sigrok backend as a provider (`kind = "sigrok"`, or the implicit `sigrok`): `desk provider
//! test` runs the program, `desk provider endpoints` lists what `--scan` finds. Devices reach it
//! through a script binding's `sigrok = { … }`, never as `provider = …` (see `crate::sigrok`).

use async_trait::async_trait;

use super::{Endpoint, Provider, ProviderInfo};
use crate::declared::{Declarations, Driver};
use crate::error::{DeskError, Result};
use crate::model::Capability;
use crate::power::PowerSwitch;
use crate::sigrok::Backend;
use crate::source::VoltageSource;

pub struct SigrokProvider(pub Backend);

fn not_a_binding() -> DeskError {
    DeskError::ConfigInvalid(
        "a sigrok backend is reached through a model's `sigrok = { … }` binding (a script \
         binding with `sigrok = { driver = \"…\" }`), not `provider = …`"
            .into(),
    )
}

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

    fn power_switch(&self, _params: &toml::Table) -> Result<Box<dyn PowerSwitch>> {
        Err(not_a_binding())
    }

    fn voltage_source(&self, _params: &toml::Table) -> Result<Box<dyn VoltageSource>> {
        Err(not_a_binding())
    }

    fn declared(&self, _params: &toml::Table, _decls: &Declarations) -> Result<Box<dyn Driver>> {
        Err(not_a_binding())
    }
}
