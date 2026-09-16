//! The binary from the outside: one fixture request relayed through a configured API-key account, and the
//! bootstrap rules that refuse a configuration nobody should be able to start.

use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
use fake_upstream::{FakeUpstream, Respond, Rule};
use std::net::TcpListener;
use std::sync::Mutex;

/// Secrets are read from the process environment inside `load`, and the tests in this binary name the same
/// variables with different values, so each holds this across its whole set-then-load sequence.
static ENV: Mutex<()> = Mutex::new(());

/// The listeners bind on the server task's first turns, so a boot is waited out by retrying the admin login
/// until it answers rather than by a fixed sleep. The login that succeeds leaves the session cookie in the
/// client's jar, which is what every admin call after it needs.
async fn logged_in(client: &reqwest::Client, session_url: &str) {
    let give_up = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let answered = client
            .post(session_url)
            .header("content-type", "application/json")
            .body(r#"{"secret":"admin-secret"}"#)
            .send()
            .await;
        match answered {
            Ok(response) if response.status() == 204 => return,
            Ok(response) => panic!("the admin login answered {}", response.status()),
            Err(_) => {}
        }
        assert!(
            std::time::Instant::now() < give_up,
            "the admin listener never accepted a connection"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The fixture request, replayed against the proxy under the given secret and session id. Headers come from
/// the captured fixture head, apart from the ones that must instead carry this call's own secret and session.
fn fixture_request(
    client: &reqwest::Client,
    proxy_port: u16,
    secret: &str,
    session: &str,
) -> reqwest::RequestBuilder {
    let head = inference_head(FixtureKind::ApiKey);
    let (_, headers) = phoebis_transport::request::parse_head(head).unwrap();
    let mut req = client
        .post(format!(
            "http://127.0.0.1:{proxy_port}/v1/messages?beta=true"
        ))
        .body(body(BodyFixture::ApiKey).to_vec());
    for (n, v) in headers {
        match n.to_ascii_lowercase().as_str() {
            "x-api-key" => {
                req = req.header("x-api-key", secret);
            }
            // The fixture head carries its own capture-time session id; `reqwest::header` appends rather
            // than replaces, so it must be dropped here or the server would see it ahead of the one set below.
            "host" | "content-length" | "x-claude-code-session-id" => {}
            _ => {
                req = req.header(n, v);
            }
        }
    }
    req.header("x-claude-code-session-id", session)
}

#[tokio::test]
async fn the_binary_is_populated_through_the_admin_api_and_routes_without_a_restart() {
    let up = FakeUpstream::start(vec![
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: Respond::Json {
                status: 200,
                headers: vec![],
                body: b"{\"ok\":true}".to_vec(),
            },
        },
        Rule {
            path_prefix: "/v1/models".into(),
            respond: Respond::Json {
                status: 200,
                headers: vec![],
                body: br#"{"data":[{"id":"claude-sonnet-4-5","type":"model"}]}"#.to_vec(),
            },
        },
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("ca.pem"), up.ca_pem()).unwrap();
    std::fs::write(
        dir.path().join("storage.key"),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    )
    .unwrap();
    let proxy_port = free_port();
    let admin_port = free_port();
    let metrics_port = free_port();
    let yaml = format!(
        r#"
admin:
  listen: 127.0.0.1:{admin_port}
  secret_env: TEST_ADMIN_SECRET
  dev_loopback_http: true
compatibility:
  outbound_gzip: false
  profile: claude-code-2.1.269-linux-x64
  usage_entrypoint: cli
proxy:
  listen: 127.0.0.1:{proxy_port}
routing:
  max_attempts: 2
stats:
  ledger_path: {ledger}
  metrics_listen: 127.0.0.1:{metrics_port}
storage:
  encryption_key_file: {key}
  path: {db}
upstream:
  resolve_override: {addr}
  extra_ca_pem: {ca}
usage:
  cache_ttl_seconds: 60
  refresh_concurrency: 2
"#,
        key = dir.path().join("storage.key").display(),
        db = dir.path().join("phoebis.sqlite").display(),
        ledger = dir.path().join("ledger.sqlite").display(),
        addr = up.addr(),
        ca = dir.path().join("ca.pem").display()
    );
    let cfg_path = dir.path().join("config.yaml");
    std::fs::write(&cfg_path, yaml).unwrap();
    let config = {
        let _env = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        std::env::set_var("TEST_ADMIN_SECRET", "admin-secret");
        phoebis_router::config::load(&cfg_path).unwrap()
    };
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(phoebis_router::app::run(config, async {
        let _ = stop_rx.await;
    }));
    let client = reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .unwrap();
    let admin = |path: &str| format!("http://127.0.0.1:{admin_port}/admin{path}");
    logged_in(&client, &admin("/session")).await;
    assert!(dir.path().join("phoebis.sqlite").exists());

    // An empty registry refuses every key, and nothing reaches the upstream.
    let refused = client
        .post(format!("http://127.0.0.1:{proxy_port}/v1/messages"))
        .header("x-api-key", "phoebis-nothing")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 401);

    let created = client
        .post(admin("/accounts"))
        .json(&serde_json::json!({ "id": "console", "kind": "api_key", "label": "Console", "secret": "sk-ant-api03-PLACEHOLDER" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        created.status(),
        201,
        "verified against the fake's models endpoint"
    );
    assert_eq!(
        client
            .post(admin("/pools"))
            .json(&serde_json::json!({ "id": "main", "label": "Main", "strategy": "priority_random", "members": [{ "account_id": "console", "priority": 0 }] }))
            .send()
            .await
            .unwrap()
            .status(),
        201
    );
    let key: serde_json::Value = client
        .post(admin("/keys"))
        .json(&serde_json::json!({ "id": "desktop", "label": "Desktop", "pool_id": "main" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let secret = key["secret"].as_str().unwrap().to_string();
    assert!(secret.starts_with("phoebis-"));

    let send = |secret: String| fixture_request(&client, proxy_port, &secret, "e2e-1").send();
    let resp = send(secret.clone()).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "{\"ok\":true}");
    let relayed = up
        .requests()
        .iter()
        .find(|r| r.line.starts_with("POST /v1/messages"))
        .unwrap()
        .clone();
    assert!(relayed
        .headers
        .iter()
        .any(|(n, v)| n == "x-api-key" && v == "sk-ant-api03-PLACEHOLDER"));
    assert_eq!(relayed.body, body(BodyFixture::ApiKey));
    let sessions: serde_json::Value = client
        .get(admin("/sessions"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(sessions["sessions"][0]["account_id"], "console");

    // Registry edits show in the next selection: no member, no candidate; a revoked key, a 401.
    assert_eq!(
        client
            .put(admin("/pools/main/members"))
            .json(&serde_json::json!({ "members": [] }))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let unserved = send(secret.clone()).await.unwrap();
    assert_eq!(
        (
            unserved.status().as_u16(),
            unserved.headers()["x-phoebis-error-code"].to_str().unwrap()
        ),
        (404, "model_unserved")
    );
    assert_eq!(
        client
            .put(admin("/pools/main/members"))
            .json(&serde_json::json!({ "members": [{ "account_id": "console", "priority": 0 }] }))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(send(secret.clone()).await.unwrap().status(), 200);
    assert_eq!(
        client
            .post(admin("/keys/desktop/revoke"))
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    assert_eq!(send(secret.clone()).await.unwrap().status(), 401);
    // The pin under `e2e-1` stays on `console` for the shutdown assertion below; the account is not deleted here.
    let listed: serde_json::Value = client
        .get(admin("/accounts"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed["accounts"].as_array().unwrap().len(), 1);

    let hello = client
        .head(format!("http://127.0.0.1:{proxy_port}/api/hello"))
        .send()
        .await
        .unwrap();
    assert_eq!(hello.status(), 200);
    let unknown = client.get(admin("/nope")).send().await.unwrap();
    assert_eq!(
        (
            unknown.status().as_u16(),
            unknown.headers()["cache-control"].to_str().unwrap()
        ),
        (401, "no-store")
    );

    let _ = stop_tx.send(());
    server.await.unwrap().unwrap();
    // The pin the last relayed request renewed was acknowledged before the stop; the drain landed it.
    let store = phoebis_store::Store::open(
        &dir.path().join("phoebis.sqlite"),
        &phoebis_store::StorageKey::from_file(&dir.path().join("storage.key")).unwrap(),
    )
    .unwrap();
    assert!(
        store
            .load_routing()
            .unwrap()
            .pins
            .iter()
            .any(|p| p.session_id == "e2e-1"),
        "acknowledged routing state survives shutdown"
    );
    up.shutdown().await;
}

/// Writes a configuration built from the given admin block, and returns its path.
fn config_with(dir: &std::path::Path, name: &str, admin: &str) -> std::path::PathBuf {
    let key = dir.join("storage.key");
    std::fs::write(
        &key,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    )
    .unwrap();
    let yaml = format!(
        r#"admin:
{admin}
compatibility:
  outbound_gzip: false
  profile: claude-code-2.1.269-linux-x64
  usage_entrypoint: cli
proxy:
  listen: 127.0.0.1:0
storage:
  encryption_key_file: {key}
  path: {db}
usage:
  cache_ttl_seconds: 60
  refresh_concurrency: 2
"#,
        key = key.display(),
        db = dir.join("router.sqlite").display()
    );
    let path = dir.join(format!("{name}.yaml"));
    std::fs::write(&path, yaml).unwrap();
    path
}

#[test]
fn shared_secrets_and_cleartext_administration_off_loopback_are_rejected() {
    use phoebis_router::config::{load, ConfigError};
    let dir = tempfile::tempdir().unwrap();
    let _env = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    std::env::set_var("TEST_ADMIN_SECRET", "admin-secret");
    std::env::set_var("TEST_VIEWER_SECRET", "viewer-secret");
    std::env::set_var("TEST_SHARED_SECRET", "admin-secret");
    let loopback_dev = "  listen: 127.0.0.1:8081\n  secret_env: TEST_ADMIN_SECRET\n  viewer_secret_env: TEST_VIEWER_SECRET\n  dev_loopback_http: true";
    let shared = "  listen: 127.0.0.1:8081\n  secret_env: TEST_ADMIN_SECRET\n  viewer_secret_env: TEST_SHARED_SECRET\n  dev_loopback_http: true";
    let public_dev =
        "  listen: 0.0.0.0:8081\n  secret_env: TEST_ADMIN_SECRET\n  dev_loopback_http: true";
    let public_plain =
        "  listen: 0.0.0.0:8081\n  secret_env: TEST_ADMIN_SECRET\n  dev_loopback_http: false";
    assert!(load(&config_with(dir.path(), "ok", loopback_dev)).is_ok());
    assert!(matches!(
        load(&config_with(dir.path(), "dup", shared)),
        Err(ConfigError::DuplicateSecret { .. })
    ));
    assert!(matches!(
        load(&config_with(dir.path(), "dev", public_dev)),
        Err(ConfigError::DevModeNotLoopback { .. })
    ));
    assert!(matches!(
        load(&config_with(dir.path(), "origin", public_plain)),
        Err(ConfigError::MissingOrigin)
    ));

    // A storage path that names no database file inside a directory never reaches the store.
    let text = std::fs::read_to_string(config_with(dir.path(), "ok", loopback_dev)).unwrap();
    let rootless = dir.path().join("rootless.yaml");
    std::fs::write(
        &rootless,
        text.replace(
            &format!("path: {}", dir.path().join("router.sqlite").display()),
            "path: /",
        ),
    )
    .unwrap();
    assert!(matches!(
        load(&rootless),
        Err(ConfigError::StoragePath { .. })
    ));
}

#[test]
fn stats_and_quota_settings_are_validated_with_the_specs_bounds() {
    use phoebis_router::config::{load, ConfigError};
    let dir = tempfile::tempdir().unwrap();
    let _env = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    std::env::set_var("TEST_ADMIN_SECRET", "admin-secret");
    let base =
        "  listen: 127.0.0.1:8081\n  secret_env: TEST_ADMIN_SECRET\n  dev_loopback_http: true";
    let text = std::fs::read_to_string(config_with(dir.path(), "ok", base)).unwrap();
    let with = |name: &str, extra: &str| {
        let path = dir.path().join(format!("{name}.yaml"));
        std::fs::write(&path, format!("{text}{extra}")).unwrap();
        path
    };
    let config = load(&with("defaults", "")).unwrap();
    assert_eq!(
        (
            config.stats.retention_days,
            config.stats.metrics_listen.to_string().as_str(),
            config.stats.ledger_path.to_str().unwrap()
        ),
        (90, "127.0.0.1:9464", "./data/ledger.sqlite")
    );
    assert_eq!(
        (
            config.quota.poll_interval_seconds,
            config.quota.sample_retention_days
        ),
        (3_600, 30)
    );
    assert_eq!(
        config
            .stats
            .pricing_path
            .as_deref()
            .and_then(|p| p.to_str()),
        Some("./pricing.yaml")
    );
    let full = load(&with("full", "stats:\n  body_capture_max_hours: 24\n  ledger_path: ./data/l.sqlite\n  metrics_listen: 127.0.0.1:9999\n  pricing_path: ./p.yaml\n  retention_days: 7\nquota:\n  poll_interval_seconds: 900\n  sample_retention_days: 3\n")).unwrap();
    assert_eq!(
        (
            full.stats.retention_days,
            full.quota.poll_interval_seconds,
            full.quota.sample_retention_days
        ),
        (7, 900, 3)
    );
    assert!(matches!(
        load(&with("poll", "quota:\n  poll_interval_seconds: 899\n")),
        Err(ConfigError::PollIntervalTooShort)
    ));
    assert!(matches!(
        load(&with("retention", "stats:\n  retention_days: 0\n")),
        Err(ConfigError::RetentionTooShort { .. })
    ));
    assert!(matches!(
        load(&with("samples", "quota:\n  sample_retention_days: 0\n")),
        Err(ConfigError::RetentionTooShort { .. })
    ));
    assert!(matches!(
        load(&with("ledger", "stats:\n  ledger_path: /\n")),
        Err(ConfigError::LedgerPath { .. })
    ));
    assert!(matches!(
        load(&with(
            "shared",
            &format!(
                "stats:\n  ledger_path: {}\n",
                dir.path().join("router.sqlite").display()
            )
        )),
        Err(ConfigError::SharedDatabasePath { .. })
    ));
    assert!(
        load(&with("unknown", "stats:\n  nope: 1\n")).is_err(),
        "unknown fields reject the configuration"
    );
}

#[tokio::test]
async fn the_binary_ledgers_serves_statistics_and_metrics_and_lands_the_ledger_on_shutdown() {
    let up = FakeUpstream::start(vec![
        Rule { path_prefix: "/v1/messages".into(), respond: Respond::Json { status: 200, headers: vec![("anthropic-ratelimit-unified-5h-utilization".into(), "0.25".into())], body: br#"{"id":"msg_1","type":"message","usage":{"input_tokens":12,"output_tokens":3}}"#.to_vec() } },
        Rule { path_prefix: "/v1/models".into(), respond: Respond::Json { status: 200, headers: vec![], body: br#"{"data":[{"id":"claude-sonnet-4-5","type":"model"}]}"#.to_vec() } },
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("ca.pem"), up.ca_pem()).unwrap();
    std::fs::write(
        dir.path().join("storage.key"),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    )
    .unwrap();
    std::fs::write(dir.path().join("pricing.yaml"), "effective: 2026-10-01\nmodels:\n  claude-sonnet-4-5: {input: 3, cache_write_5m: 3.75, cache_write_1h: 6, cache_read: 0.3, output: 15}\n").unwrap();
    let (proxy_port, admin_port, metrics_port) = (free_port(), free_port(), free_port());
    let yaml = format!(
        "admin:\n  listen: 127.0.0.1:{admin_port}\n  secret_env: TEST_ADMIN_SECRET\n  dev_loopback_http: true\ncompatibility:\n  outbound_gzip: false\n  profile: claude-code-2.1.269-linux-x64\n  usage_entrypoint: cli\nproxy:\n  listen: 127.0.0.1:{proxy_port}\nstats:\n  ledger_path: {ledger}\n  metrics_listen: 127.0.0.1:{metrics_port}\n  pricing_path: {pricing}\nstorage:\n  encryption_key_file: {key}\n  path: {db}\nupstream:\n  resolve_override: {addr}\n  extra_ca_pem: {ca}\nusage:\n  cache_ttl_seconds: 60\n  refresh_concurrency: 2\n",
        ledger = dir.path().join("ledger.sqlite").display(), pricing = dir.path().join("pricing.yaml").display(),
        key = dir.path().join("storage.key").display(), db = dir.path().join("phoebis.sqlite").display(),
        addr = up.addr(), ca = dir.path().join("ca.pem").display(),
    );
    let cfg_path = dir.path().join("config.yaml");
    std::fs::write(&cfg_path, yaml).unwrap();
    let config = {
        let _env = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        std::env::set_var("TEST_ADMIN_SECRET", "admin-secret");
        phoebis_router::config::load(&cfg_path).unwrap()
    };
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(phoebis_router::app::run(config, async {
        let _ = stop_rx.await;
    }));
    let client = reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .unwrap();
    let admin = |path: &str| format!("http://127.0.0.1:{admin_port}/admin{path}");
    logged_in(&client, &admin("/session")).await;
    assert_eq!(client.post(admin("/accounts")).json(&serde_json::json!({ "id": "console", "kind": "api_key", "label": "Console", "secret": "sk-ant-api03-PLACEHOLDER" })).send().await.unwrap().status(), 201);
    assert_eq!(client.post(admin("/pools")).json(&serde_json::json!({ "id": "main", "label": "Main", "strategy": "priority_random", "body_capture_hours": 1, "members": [{ "account_id": "console", "priority": 0 }] })).send().await.unwrap().status(), 201);
    let key: serde_json::Value = client
        .post(admin("/keys"))
        .json(&serde_json::json!({ "id": "desktop", "label": "Desktop", "pool_id": "main" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let secret = key["secret"].as_str().unwrap().to_string();
    for i in 0..3 {
        let resp = fixture_request(&client, proxy_port, &secret, &format!("e2e-{i}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let _ = resp.text().await.unwrap();
    }
    // The ledger lands on its own task; the request log shows the rows once it has.
    let give_up = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let log = loop {
        let log: serde_json::Value = client
            .get(admin("/requests"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if log["requests"].as_array().unwrap().len() == 3 {
            break log;
        }
        assert!(
            std::time::Instant::now() < give_up,
            "the ledger never landed the three requests: {log}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    let row = &log["requests"][0];
    assert_eq!(
        (
            row["key_id"].as_str(),
            row["account_id"].as_str(),
            row["status"].as_u64(),
            row["captured_bodies"].as_bool()
        ),
        (Some("desktop"), Some("console"), Some(200), Some(true))
    );
    assert_eq!(
        row["attempts"][0]["pricing_version"], "2026-10-01",
        "the override file priced the attempt"
    );
    assert!((row["cost_usd"].as_f64().unwrap() - (12.0 * 3.0 + 3.0 * 15.0) / 1e6).abs() < 1e-12);
    let id = row["id"].as_str().unwrap();
    let bodies: serde_json::Value = client
        .get(admin(&format!("/requests/{id}/bodies")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(bodies["request"]["model"], "claude-sonnet-4-5");
    let breakdown: serde_json::Value = client
        .get(admin("/analytics/breakdown?range=1h&group_by=key"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(breakdown["rows"][0]["requests"].as_u64(), Some(3));
    let dashboard: serde_json::Value = client
        .get(admin("/dashboard"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(dashboard["requests_24h"].as_u64(), Some(3));
    let sessions: serde_json::Value = client
        .get(admin("/sessions"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(sessions["sessions"].as_array().unwrap().len(), 3);
    assert_eq!(sessions["sessions"][0]["requests"].as_u64(), Some(1));
    let system: serde_json::Value = client
        .get(admin("/system"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        (
            system["ledger"]["dropped"].as_u64(),
            system["ledger"]["written"].as_u64().map(|n| n >= 3)
        ),
        (Some(0), Some(true))
    );
    let settings: serde_json::Value = client
        .get(admin("/settings"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        settings["pricing"]["version"], "2026-10-01",
        "the override file's effective date is its version"
    );
    let metrics = client
        .get(format!("http://127.0.0.1:{metrics_port}/metrics"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        metrics.headers()["content-type"].to_str().unwrap(),
        "text/plain; version=0.0.4; charset=utf-8"
    );
    let text = metrics.text().await.unwrap();
    assert!(
        text.contains(
            r#"phoebis_requests_total{key="desktop",kind="inference",outcome="succeeded"} 3"#
        ),
        "{text}"
    );
    assert!(text.contains(r#"phoebis_attempts_total{account="console",model="claude-sonnet-4-5",outcome="succeeded"} 3"#));
    assert!(
        text.contains(r#"phoebis_pins_active{pool="main"} 3"#),
        "three sessions, three pins: {text}"
    );
    assert!(
        client
            .get(format!("http://127.0.0.1:{metrics_port}/nope"))
            .send()
            .await
            .unwrap()
            .status()
            == 404
    );
    // A request whose completion is in flight at the stop: the drain lands it before `run` returns.
    let resp = fixture_request(&client, proxy_port, &secret, "e2e-last")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = stop_tx.send(());
    server.await.unwrap().unwrap();
    let ledger = phoebis_ledger::LedgerDb::open(&dir.path().join("ledger.sqlite")).unwrap();
    let (rows, _) = ledger
        .read(|c| phoebis_ledger::list_requests(c, &phoebis_ledger::RequestFilter::default(), None, 10))
        .unwrap();
    assert_eq!(
        rows.len(),
        4,
        "the last request's record was on disk when run returned"
    );
    assert!(rows.iter().all(|(r, _)| r.bodies_captured));
    up.shutdown().await;
}
