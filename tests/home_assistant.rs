//! End-to-end tests of the Home Assistant provider: the real `desk` binary against a fake,
//! stateful Home Assistant REST API.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{header, method, path, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const TOKEN: &str = "test-token";

/// Shared fake HA state. A POST to turn_on/turn_off only becomes visible after `lag` further
/// state reads, like a Shelly that reports back a moment later.
#[derive(Clone, Default)]
struct FakeHa {
    entities: Arc<Mutex<BTreeMap<String, String>>>,
    pending: Arc<Mutex<HashMap<String, (String, u32)>>>,
    lag: Arc<Mutex<u32>>,
}

impl FakeHa {
    fn with(entities: &[(&str, &str)]) -> Self {
        let ha = FakeHa::default();
        {
            let mut map = ha.entities.lock().unwrap();
            for (id, state) in entities {
                map.insert(id.to_string(), state.to_string());
            }
        }
        ha
    }

    fn set_lag(&self, reads: u32) {
        *self.lag.lock().unwrap() = reads;
    }

    fn state(&self, entity: &str) -> Option<String> {
        let mut pending = self.pending.lock().unwrap();
        if let Some((new_state, reads_left)) = pending.get_mut(entity) {
            if *reads_left == 0 {
                let new_state = new_state.clone();
                pending.remove(entity);
                self.entities
                    .lock()
                    .unwrap()
                    .insert(entity.to_string(), new_state);
            } else {
                *reads_left -= 1;
            }
        }
        self.entities.lock().unwrap().get(entity).cloned()
    }

    fn entity_json(id: &str, state: &str) -> Value {
        let name = id.split('.').nth(1).unwrap_or(id).replace('_', " ");
        json!({ "entity_id": id, "state": state, "attributes": { "friendly_name": name } })
    }
}

struct AllStates(FakeHa);
impl Respond for AllStates {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let all: Vec<Value> = self
            .0
            .entities
            .lock()
            .unwrap()
            .iter()
            .map(|(id, s)| FakeHa::entity_json(id, s))
            .collect();
        ResponseTemplate::new(200).set_body_json(all)
    }
}

struct OneState(FakeHa);
impl Respond for OneState {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let entity = req.url.path().trim_start_matches("/api/states/");
        match self.0.state(entity) {
            Some(s) => ResponseTemplate::new(200).set_body_json(FakeHa::entity_json(entity, &s)),
            None => {
                ResponseTemplate::new(404).set_body_json(json!({"message": "Entity not found."}))
            }
        }
    }
}

struct Service(FakeHa);
impl Respond for Service {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&req.body).unwrap();
        let entity = body["entity_id"].as_str().unwrap().to_string();
        let new_state = if req.url.path().ends_with("/turn_on") {
            "on"
        } else {
            "off"
        };
        let lag = *self.0.lag.lock().unwrap();
        self.0
            .pending
            .lock()
            .unwrap()
            .insert(entity, (new_state.to_string(), lag));
        ResponseTemplate::new(200).set_body_json(json!([]))
    }
}

async fn start(ha: &FakeHa) -> MockServer {
    let server = MockServer::start().await;
    let auth = || header("authorization", format!("Bearer {TOKEN}").as_str());
    Mock::given(method("GET"))
        .and(path("/api/config"))
        .and(auth())
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "version": "2026.10.1", "location_name": "Workshop" })),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/states"))
        .and(auth())
        .respond_with(AllStates(ha.clone()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/api/states/[a-z0-9_]+\.[a-z0-9_]+$"))
        .and(auth())
        .respond_with(OneState(ha.clone()))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/api/services/[a-z_]+/turn_(on|off)$"))
        .and(auth())
        .respond_with(Service(ha.clone()))
        .mount(&server)
        .await;
    // Anything without the right token.
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(401).set_body_string("401: Unauthorized"))
        .with_priority(10)
        .mount(&server)
        .await;
    server
}

struct Bench {
    dir: TempDir,
}

struct Out {
    code: i32,
    stdout: String,
}

impl Out {
    fn json(&self) -> Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("stdout is not JSON ({e}):\n{}", self.stdout))
    }
}

impl Bench {
    /// `provider` is the body of `[providers.ha]`; the token file `ha.token` holds `token`.
    fn new(provider: &str, token: &str) -> Self {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("ha.token"), format!("{token}\n")).unwrap();
        let config = format!(
            r#"
[providers.ha]
kind = "home-assistant"
{provider}

[providers.sim]
kind = "mock"

[devices.psu]
name  = "FNIRSI IPS3608"
power = {{ provider = "ha", entity = "switch.desk_outlet_1" }}

[devices.iron]
power = {{ provider = "ha", entity = "switch.desk_outlet_4" }}

[devices.offline]
power = {{ provider = "ha", entity = "switch.desk_outlet_3" }}

[devices.ghost]
power = {{ provider = "ha", entity = "switch.does_not_exist" }}

[devices.stuck]
power = {{ provider = "ha", entity = "switch.stuck", timeout = "8s" }}

[devices.sim_lamp]
power = {{ provider = "sim", endpoint = "lamp" }}
"#
        );
        std::fs::write(dir.path().join("desk.toml"), config).unwrap();
        Bench { dir }
    }

    fn for_server(server: &MockServer) -> Self {
        Self::new(
            &format!("url = \"{}\"\ntoken_file = \"ha.token\"", server.uri()),
            TOKEN,
        )
    }

    fn config(&self) -> PathBuf {
        self.dir.path().join("desk.toml")
    }

    async fn run(&self, args: &[&str]) -> Out {
        let config = self.config();
        let state = self.dir.path().join("mock-state.json");
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        tokio::task::spawn_blocking(move || run(&config, &state, &args))
            .await
            .unwrap()
    }
}

fn run(config: &Path, state: &Path, args: &[String]) -> Out {
    let out = Command::new(env!("CARGO_BIN_EXE_desk"))
        .env_remove("DESK_CONFIG")
        .env_remove("HA_TOKEN")
        .env("DESK_MOCK_STATE", state)
        .arg("--config")
        .arg(config)
        .args(args)
        .output()
        .unwrap();
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
    }
}

fn bench_entities() -> FakeHa {
    FakeHa::with(&[
        ("switch.desk_outlet_1", "off"),
        ("switch.desk_outlet_2", "off"),
        ("switch.desk_outlet_3", "unavailable"),
        ("switch.desk_outlet_4", "on"),
        ("switch.stuck", "off"),
        ("light.bench_lamp", "off"),
        ("sensor.desk_power", "12.5"),
    ])
}

async fn post_count(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "POST")
        .count()
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_test_reports_version() {
    let ha = bench_entities();
    let server = start(&ha).await;
    let b = Bench::for_server(&server);
    let out = b.run(&["--json", "provider", "test", "ha"]).await;
    assert_eq!(out.code, 0, "{}", out.stdout);
    let detail = out.json()["data"]["detail"].as_str().unwrap().to_string();
    assert!(detail.contains("2026.10.1"), "{detail}");
    assert!(detail.contains("Workshop"), "{detail}");
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_token_is_reported() {
    let ha = bench_entities();
    let server = start(&ha).await;
    let b = Bench::new(
        &format!("url = \"{}\"\ntoken_file = \"ha.token\"", server.uri()),
        "wrong",
    );
    let out = b.run(&["--json", "provider", "test", "ha"]).await;
    assert_eq!(out.code, 4);
    let j = out.json();
    assert_eq!(j["error"]["code"], "provider_unreachable");
    assert!(
        j["error"]["message"]
            .as_str()
            .unwrap()
            .contains("authentication failed")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn endpoints_lists_switchable_entities() {
    let ha = bench_entities();
    let server = start(&ha).await;
    let b = Bench::for_server(&server);

    let j = b
        .run(&["--json", "provider", "endpoints", "ha"])
        .await
        .json();
    let ids: Vec<&str> = j["data"]["endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"light.bench_lamp"));
    assert!(
        !ids.contains(&"sensor.desk_power"),
        "sensors can't be switched"
    );
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted);
    assert_eq!(
        j["data"]["endpoints"][0]["params"]["entity"],
        j["data"]["endpoints"][0]["id"]
    );

    let j = b
        .run(&[
            "--json",
            "provider",
            "endpoints",
            "ha",
            "--search",
            "OUTLET 4",
        ])
        .await
        .json();
    let endpoints = j["data"]["endpoints"].as_array().unwrap();
    assert_eq!(
        endpoints.len(),
        1,
        "matches the friendly name: {endpoints:?}"
    );
    assert_eq!(endpoints[0]["id"], "switch.desk_outlet_4");
    assert_eq!(endpoints[0]["state"], "on");

    let out = b
        .run(&["provider", "endpoints", "ha", "-s", "outlet_1"])
        .await;
    assert!(
        out.stdout
            .contains(r#"power = { provider = "ha", entity = "switch.desk_outlet_1" }"#),
        "{}",
        out.stdout
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn power_on_waits_for_lagging_state() {
    let ha = bench_entities();
    ha.set_lag(3);
    let server = start(&ha).await;
    let b = Bench::for_server(&server);

    let out = b.run(&["--json", "power", "on", "psu"]).await;
    assert_eq!(out.code, 0, "{}", out.stdout);
    let r = &out.json()["data"]["results"][0];
    assert_eq!(r["before"]["state"], "off");
    assert_eq!(r["power"]["state"], "on");
    assert_eq!(r["power"]["confidence"], "real");
    assert_eq!(r["changed"], true);
    assert_eq!(post_count(&server).await, 1);

    let j = b.run(&["--json", "power", "status", "psu"]).await.json();
    assert_eq!(j["data"]["devices"][0]["power"]["state"], "on");
}

#[tokio::test(flavor = "multi_thread")]
async fn already_on_sends_nothing() {
    let ha = bench_entities();
    let server = start(&ha).await;
    let b = Bench::for_server(&server);
    let j = b.run(&["--json", "power", "on", "iron"]).await.json();
    assert_eq!(j["data"]["results"][0]["changed"], false);
    assert_eq!(post_count(&server).await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn unavailable_and_unknown_entities() {
    let ha = bench_entities();
    let server = start(&ha).await;
    let b = Bench::for_server(&server);

    let out = b.run(&["--json", "power", "on", "offline"]).await;
    assert_eq!(out.code, 4);
    let j = out.json();
    assert_eq!(j["error"]["code"], "endpoint_unavailable");
    assert!(
        j["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unavailable")
    );

    let out = b.run(&["--json", "power", "on", "ghost"]).await;
    assert_eq!(out.code, 4);
    let j = out.json();
    assert_eq!(j["error"]["code"], "endpoint_unavailable");
    assert!(
        j["error"]["message"]
            .as_str()
            .unwrap()
            .contains("desk provider endpoints ha")
    );
    assert_eq!(
        post_count(&server).await,
        0,
        "nothing sent to unknown entities"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn state_that_never_changes_fails_verification() {
    let ha = bench_entities();
    ha.set_lag(u32::MAX);
    let server = start(&ha).await;
    let b = Bench::for_server(&server);
    let out = b.run(&["--json", "power", "on", "stuck"]).await;
    assert_eq!(out.code, 7, "{}", out.stdout);
    assert_eq!(out.json()["error"]["code"], "verify_failed");
}

#[tokio::test(flavor = "multi_thread")]
async fn unreachable_home_assistant() {
    // Port 9 (discard) is closed on any sane test machine: connection refused.
    let b = Bench::new(
        "url = \"http://127.0.0.1:9\"\ntoken_file = \"ha.token\"",
        TOKEN,
    );
    let out = b.run(&["--json", "power", "on", "psu"]).await;
    assert_eq!(out.code, 4, "{}", out.stdout);
    assert_eq!(out.json()["error"]["code"], "provider_unreachable");

    let out = b.run(&["--json", "status"]).await;
    assert_eq!(out.code, 0, "status reports problems inline");
    let j = out.json();
    let ha = &j["data"]["providers"][0];
    assert_eq!(ha["ok"], false);
    assert_eq!(ha["error"]["code"], "provider_unreachable");
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_token_only_affects_ha_devices() {
    let b = Bench::new(
        "url = \"http://127.0.0.1:9\"\ntoken_env = \"DESK_TEST_TOKEN_THAT_IS_NOT_SET\"",
        TOKEN,
    );
    let out = b.run(&["--json", "power", "on", "psu"]).await;
    assert_eq!(out.code, 3);
    let j = out.json();
    assert_eq!(j["error"]["code"], "config_invalid");
    assert!(
        j["error"]["message"]
            .as_str()
            .unwrap()
            .contains("DESK_TEST_TOKEN_THAT_IS_NOT_SET")
    );

    let out = b.run(&["--json", "power", "on", "sim_lamp"]).await;
    assert_eq!(out.code, 0, "mock devices still work: {}", out.stdout);
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_entity_in_binding() {
    let dir = TempDir::new().unwrap();
    let config = dir.path().join("desk.toml");
    std::fs::write(
        &config,
        r#"
[providers.ha]
kind = "home-assistant"
url = "http://127.0.0.1:9"
token_file = "t"
[devices.x]
power = { provider = "ha", entity = "sensor.temperature" }
"#,
    )
    .unwrap();
    std::fs::write(dir.path().join("t"), TOKEN).unwrap();
    let state = dir.path().join("s.json");
    let args = vec![
        "--json".to_string(),
        "power".into(),
        "on".into(),
        "x".into(),
    ];
    let out = tokio::task::spawn_blocking(move || run(&config, &state, &args))
        .await
        .unwrap();
    assert_eq!(out.code, 3);
    assert!(
        out.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("cannot be switched")
    );
}
