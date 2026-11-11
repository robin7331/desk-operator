//! Home Assistant via its REST API.
//!
//! ```toml
//! [providers.ha]
//! kind      = "home-assistant"
//! url       = "http://homeassistant.local:8123"
//! token_env = "HA_TOKEN"            # default; or token_file = "~/.config/desk-operator/ha.token"
//!
//! [devices.psu]
//! power = { provider = "ha", entity = "switch.desk_outlet_1" }
//! ```
//!
//! The token is a long-lived access token (HA profile → Security). It is never logged or printed.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::json;

use super::{BuildContext, Endpoint, Params, Provider, ProviderInfo};
use crate::error::{DeskError, Result};
use crate::model::{Capability, Power, PowerFeatures, PowerState};
use crate::power::PowerSwitch;

pub const KIND: &str = "home-assistant";

/// Entity domains that have `turn_on` / `turn_off` services and an on/off state.
const POWER_DOMAINS: &[&str] = &["switch", "light", "fan", "input_boolean"];

const DEFAULT_TOKEN_ENV: &str = "HA_TOKEN";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// HA (and the Shelly behind it) may report the old state briefly after a service call.
const SETTLE: Duration = Duration::from_secs(3);

pub struct HomeAssistant {
    client: Arc<Client>,
}

struct Client {
    id: String,
    base: String,
    token: String,
    http: reqwest::Client,
}

#[derive(Deserialize)]
struct EntityState {
    entity_id: String,
    state: String,
    #[serde(default)]
    attributes: Attributes,
}

#[derive(Default, Deserialize)]
struct Attributes {
    friendly_name: Option<String>,
}

#[derive(Deserialize)]
struct HaConfig {
    version: Option<String>,
    location_name: Option<String>,
}

impl HomeAssistant {
    pub fn new(id: &str, settings: &Params, ctx: &BuildContext) -> Result<Self> {
        settings.only(&["url", "token_env", "token_file"])?;
        let url = settings.required_str("url")?;
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(DeskError::ConfigInvalid(format!(
                "provider `{id}`: `url` must start with http:// or https://"
            )));
        }
        let token = match (settings.str("token_env")?, settings.str("token_file")?) {
            (Some(_), Some(_)) => {
                return Err(DeskError::ConfigInvalid(format!(
                    "provider `{id}`: set either `token_env` or `token_file`, not both"
                )));
            }
            (None, Some(file)) => {
                let path = ctx.resolve_path(file);
                let token = std::fs::read_to_string(&path).map_err(|e| {
                    DeskError::ConfigInvalid(format!(
                        "provider `{id}`: cannot read token_file {}: {e}",
                        path.display()
                    ))
                })?;
                token.trim().to_string()
            }
            (env, None) => {
                let var = env.unwrap_or(DEFAULT_TOKEN_ENV);
                std::env::var(var).map_err(|_| {
                    DeskError::ConfigInvalid(format!(
                        "provider `{id}`: environment variable {var} is not set \
                         (export a Home Assistant long-lived access token, or use token_file)"
                    ))
                })?
            }
        };
        if token.is_empty() {
            return Err(DeskError::ConfigInvalid(format!(
                "provider `{id}`: the access token is empty"
            )));
        }

        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(REQUEST_TIMEOUT)
            .user_agent(concat!("desk-operator/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| DeskError::Internal(format!("cannot create HTTP client: {e}")))?;

        Ok(HomeAssistant {
            client: Arc::new(Client {
                id: id.to_string(),
                base: url.trim_end_matches('/').to_string(),
                token,
                http,
            }),
        })
    }
}

impl Client {
    fn unreachable(&self, message: impl Into<String>) -> DeskError {
        DeskError::ProviderUnreachable {
            provider: self.id.clone(),
            message: message.into(),
            hint: None,
        }
    }

    /// Send a request; map transport and auth failures. Other statuses are left to the caller.
    async fn send(&self, req: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        let resp = req
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| self.unreachable(describe(&e)))?;
        match resp.status() {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(self.unreachable(format!(
                "authentication failed (HTTP {}), check the access token",
                resp.status().as_u16()
            ))),
            _ => Ok(resp),
        }
    }

    async fn json<T: serde::de::DeserializeOwned>(&self, resp: reqwest::Response) -> Result<T> {
        resp.json()
            .await
            .map_err(|e| self.unreachable(format!("unexpected response: {}", describe(&e))))
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let resp = self
            .send(self.http.get(format!("{}{path}", self.base)))
            .await?;
        if !resp.status().is_success() {
            return Err(self.unreachable(format!("GET {path}: HTTP {}", resp.status().as_u16())));
        }
        self.json(resp).await
    }
}

/// reqwest's Display hides the interesting part ("connection refused") in the source chain.
fn describe(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        return format!(
            "request timed out after {}",
            humantime::format_duration(REQUEST_TIMEOUT)
        );
    }
    let mut msg = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        source = s.source();
    }
    msg
}

#[async_trait]
impl Provider for HomeAssistant {
    async fn test(&self) -> Result<ProviderInfo> {
        let c = &self.client;
        let cfg: HaConfig = c.get_json("/api/config").await?;
        let mut detail = format!(
            "Home Assistant {} at {}",
            cfg.version.as_deref().unwrap_or("(unknown version)"),
            c.base
        );
        if let Some(name) = cfg.location_name {
            detail.push_str(&format!(" ({name})"));
        }
        Ok(ProviderInfo { detail })
    }

    async fn endpoints(&self, capability: Capability) -> Result<Vec<Endpoint>> {
        if capability != Capability::PowerSwitch {
            return Ok(Vec::new());
        }
        let mut states: Vec<EntityState> = self.client.get_json("/api/states").await?;
        states.retain(|s| domain_of(&s.entity_id).is_some_and(|d| POWER_DOMAINS.contains(&d)));
        states.sort_by(|a, b| a.entity_id.cmp(&b.entity_id));
        Ok(states
            .into_iter()
            .map(|s| {
                let mut params = toml::Table::new();
                params.insert("entity".into(), toml::Value::String(s.entity_id.clone()));
                Endpoint {
                    id: s.entity_id,
                    name: s.attributes.friendly_name,
                    state: Some(s.state),
                    params,
                }
            })
            .collect())
    }

    fn power_switch(&self, params: &toml::Table) -> Result<Box<dyn PowerSwitch>> {
        let id = &self.client.id;
        let params = Params::new(params, format!("power binding for provider `{id}`"));
        params.only(&["entity"])?;
        let entity = params.required_str("entity")?;
        let domain = validate_entity(entity).map_err(|msg| {
            DeskError::ConfigInvalid(format!("power binding for provider `{id}`: {msg}"))
        })?;
        Ok(Box::new(HaSwitch {
            client: self.client.clone(),
            entity: entity.to_string(),
            domain: domain.to_string(),
        }))
    }
}

fn domain_of(entity: &str) -> Option<&str> {
    entity.split_once('.').map(|(d, _)| d)
}

/// `domain.object_id`, lowercase `[a-z0-9_]`, with a switchable domain. Returns the domain.
fn validate_entity(entity: &str) -> std::result::Result<&str, String> {
    let valid = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    match entity.split_once('.') {
        Some((domain, object)) if valid(domain) && valid(object) => {
            if POWER_DOMAINS.contains(&domain) {
                Ok(domain)
            } else {
                Err(format!(
                    "entity `{entity}`: domain `{domain}` cannot be switched (use one of: {})",
                    POWER_DOMAINS.join(", ")
                ))
            }
        }
        _ => Err(format!(
            "`{entity}` is not an entity id (expected e.g. `switch.desk_outlet_1`)"
        )),
    }
}

struct HaSwitch {
    client: Arc<Client>,
    entity: String,
    domain: String,
}

impl HaSwitch {
    fn unavailable(&self, message: impl Into<String>) -> DeskError {
        DeskError::EndpointUnavailable {
            endpoint: self.entity.clone(),
            message: message.into(),
        }
    }
}

#[async_trait]
impl PowerSwitch for HaSwitch {
    fn features(&self) -> PowerFeatures {
        PowerFeatures {
            readback: true,
            settle: SETTLE,
        }
    }

    async fn set(&self, on: bool) -> Result<()> {
        let c = &self.client;
        let service = if on { "turn_on" } else { "turn_off" };
        let path = format!("/api/services/{}/{service}", self.domain);
        let resp = c
            .send(
                c.http
                    .post(format!("{}{path}", c.base))
                    .json(&json!({ "entity_id": self.entity })),
            )
            .await?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        let body = body.trim();
        match status {
            StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND => Err(self.unavailable(format!(
                "{}.{service} rejected (HTTP {}){}",
                self.domain,
                status.as_u16(),
                if body.is_empty() {
                    String::new()
                } else {
                    format!(": {body}")
                }
            ))),
            _ => Err(c.unreachable(format!("POST {path}: HTTP {}", status.as_u16()))),
        }
    }

    async fn state(&self) -> Result<PowerState> {
        let c = &self.client;
        let path = format!("/api/states/{}", self.entity);
        let resp = c.send(c.http.get(format!("{}{path}", c.base))).await?;
        match resp.status() {
            StatusCode::NOT_FOUND => {
                return Err(self.unavailable(format!(
                    "no such entity in Home Assistant (list them with `desk provider endpoints {}`)",
                    c.id
                )));
            }
            s if !s.is_success() => {
                return Err(c.unreachable(format!("GET {path}: HTTP {}", s.as_u16())));
            }
            _ => {}
        }
        let entity: EntityState = c.json(resp).await?;
        match entity.state.as_str() {
            "on" => Ok(PowerState::real(Power::On)),
            "off" => Ok(PowerState::real(Power::Off)),
            "unavailable" => Err(self
                .unavailable("Home Assistant reports it as unavailable (is the Shelly online?)")),
            _ => Ok(PowerState::unknown()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_validation() {
        assert_eq!(validate_entity("switch.desk_outlet_1"), Ok("switch"));
        assert_eq!(validate_entity("light.bench"), Ok("light"));
        assert!(
            validate_entity("sensor.temp")
                .unwrap_err()
                .contains("cannot be switched")
        );
        assert!(
            validate_entity("switch")
                .unwrap_err()
                .contains("not an entity id")
        );
        assert!(validate_entity("switch.Desk").is_err());
        assert!(validate_entity("switch.a/../b").is_err());
    }
}
