//! The bench: loaded config plus providers. Entry point for every operation on devices.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::config::{Config, Device, PowerBinding};
use crate::error::{DeskError, Result};
use crate::model::{Capability, PowerState};
use crate::power::{self, Outcome, Plan, PowerSwitch, Target};
use crate::providers::{Endpoint, ProviderInfo, Registry};
use crate::script::{DeviceInfo, ScriptSwitch};

pub struct Bench {
    pub config: Config,
    pub providers: Arc<Registry>,
}

/// The result of an operation on one device, plus whatever it logged (scripts), which is
/// reported even when the operation failed.
pub struct Run<T> {
    pub result: Result<T>,
    pub log: Vec<String>,
}

impl Bench {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let config = Config::load(path)?;
        let base_dir = config
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let providers = Arc::new(Registry::new(config.providers.clone(), base_dir));
        Ok(Bench { config, providers })
    }

    fn power_switch(&self, device: &Device) -> Result<(Box<dyn PowerSwitch>, Duration)> {
        let binding = device
            .power
            .as_ref()
            .ok_or_else(|| DeskError::NoPowerPath(device.id.clone()))?;
        Ok(match binding {
            PowerBinding::Native {
                provider,
                params,
                timeout,
            } => (
                self.providers.get(provider)?.power_switch(params)?,
                timeout.unwrap_or(power::DEFAULT_TIMEOUT),
            ),
            PowerBinding::Script {
                script,
                params,
                timeout,
            } => {
                let timeout = timeout.unwrap_or(power::DEFAULT_SCRIPT_TIMEOUT);
                let info = DeviceInfo {
                    id: &device.id,
                    name: &device.name,
                };
                let sw = ScriptSwitch::load(info, script, params, timeout, self.providers.clone())?;
                (Box::new(sw), timeout)
            }
        })
    }

    pub async fn set_power(&self, device: &Device, target: Target) -> Run<Outcome> {
        let (sw, after) = match self.power_switch(device) {
            Ok(v) => v,
            Err(e) => {
                return Run {
                    result: Err(e),
                    log: Vec::new(),
                };
            }
        };
        let result = tokio::time::timeout(after, power::switch(&device.id, sw.as_ref(), target))
            .await
            .unwrap_or_else(|_| {
                Err(DeskError::Timeout {
                    what: format!("switching `{}`", device.id),
                    after,
                })
            });
        Run {
            result,
            log: sw.take_log(),
        }
    }

    pub async fn plan_power(&self, device: &Device, target: Target) -> Run<Plan> {
        let (sw, after) = match self.power_switch(device) {
            Ok(v) => v,
            Err(e) => {
                return Run {
                    result: Err(e),
                    log: Vec::new(),
                };
            }
        };
        let result = tokio::time::timeout(after, power::plan(&device.id, sw.as_ref(), target))
            .await
            .unwrap_or_else(|_| {
                Err(DeskError::Timeout {
                    what: format!("planning `{}`", device.id),
                    after,
                })
            });
        Run {
            result,
            log: sw.take_log(),
        }
    }

    pub async fn power_state(&self, device: &Device) -> Run<PowerState> {
        let (sw, after) = match self.power_switch(device) {
            Ok(v) => v,
            Err(e) => {
                return Run {
                    result: Err(e),
                    log: Vec::new(),
                };
            }
        };
        let result = tokio::time::timeout(after, sw.state())
            .await
            .unwrap_or_else(|_| {
                Err(DeskError::Timeout {
                    what: format!("reading power state of `{}`", device.id),
                    after,
                })
            });
        Run {
            result,
            log: sw.take_log(),
        }
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

    pub async fn endpoints(&self, id: &str, capability: Capability) -> Result<Vec<Endpoint>> {
        let provider = self.providers.get(id)?;
        let after = power::DEFAULT_TIMEOUT;
        tokio::time::timeout(after, provider.endpoints(capability))
            .await
            .map_err(|_| DeskError::Timeout {
                what: format!("listing endpoints of provider `{id}`"),
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
