//! The bench: loaded config plus providers. Entry point for every operation on devices.

use std::path::Path;
use std::time::Duration;

use crate::config::{Config, Device, PowerBinding};
use crate::error::{DeskError, Result};
use crate::model::PowerState;
use crate::power::{self, Outcome, PowerSwitch, Target};
use crate::providers::{ProviderInfo, Registry};

pub struct Bench {
    pub config: Config,
    pub providers: Registry,
}

impl Bench {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let config = Config::load(path)?;
        let providers = Registry::new(config.providers.clone());
        Ok(Bench { config, providers })
    }

    fn power_switch(&self, device: &Device) -> Result<(Box<dyn PowerSwitch>, Duration)> {
        let binding = device
            .power
            .as_ref()
            .ok_or_else(|| DeskError::NoPowerPath(device.id.clone()))?;
        let timeout = binding.timeout().unwrap_or(power::DEFAULT_TIMEOUT);
        let sw = match binding {
            PowerBinding::Native {
                provider, params, ..
            } => self.providers.get(provider)?.power_switch(params)?,
            PowerBinding::Script { .. } => {
                return Err(DeskError::Unsupported(
                    "script power bindings are not implemented yet (milestone M2)".into(),
                ));
            }
        };
        Ok((sw, timeout))
    }

    pub async fn set_power(&self, device: &Device, target: Target) -> Result<Outcome> {
        let (sw, after) = self.power_switch(device)?;
        tokio::time::timeout(after, power::switch(&device.id, sw.as_ref(), target))
            .await
            .map_err(|_| DeskError::Timeout {
                what: format!("switching `{}`", device.id),
                after,
            })?
    }

    pub async fn power_state(&self, device: &Device) -> Result<PowerState> {
        let (sw, after) = self.power_switch(device)?;
        tokio::time::timeout(after, sw.state())
            .await
            .map_err(|_| DeskError::Timeout {
                what: format!("reading power state of `{}`", device.id),
                after,
            })?
    }

    pub async fn test_provider(&self, id: &str) -> Result<ProviderInfo> {
        let provider = self.providers.get(id)?;
        let after = power::DEFAULT_TIMEOUT;
        tokio::time::timeout(after, provider.test())
            .await
            .map_err(|_| DeskError::Timeout {
                what: format!("testing provider `{id}`"),
                after,
            })?
    }

    /// Resolve device queries up front, deduplicated, in the order given. Fails before anything
    /// is touched if any query doesn't resolve.
    pub fn resolve_all(&self, queries: &[String]) -> Result<Vec<&Device>> {
        let mut out: Vec<&Device> = Vec::new();
        for q in queries {
            let d = self.config.resolve_device(q)?;
            if !out.iter().any(|o| o.id == d.id) {
                out.push(d);
            }
        }
        Ok(out)
    }

    pub fn powered_devices(&self) -> Vec<&Device> {
        self.config
            .devices
            .iter()
            .filter(|d| d.power.is_some())
            .collect()
    }
}
