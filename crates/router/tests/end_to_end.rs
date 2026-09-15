//! The binary from the outside: one fixture request relayed through a configured API-key account, and the
//! bootstrap rules that refuse a configuration nobody should be able to start.

use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
use fake_upstream::{FakeUpstream, Respond, Rule};
use std::net::TcpListener;
use std::sync::Mutex;

/// Secrets are read from the process environment inside `load`, and both tests in this binary name
/// `TEST_DESKTOP_KEY` with different values, so each holds this across its whole set-then-load sequence.
static ENV: Mutex<()> = Mutex::new(());

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test]
#[ignore = "the registry replaces the routes block; the acceptance task rewrites this test"]
async fn the_binary_relays_a_fixture_request_through_a_configured_api_key_account() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: b"{\"ok\":true}".to_vec(),
        },
    }])
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
    let yaml = format!(
        r#"
accounts:
  - id: console
    kind: api_key
    label: Console
    secret_env: TEST_CONSOLE_KEY
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
routes:
  - key_id: desktop
    secret_env: TEST_DESKTOP_KEY
    account_id: console
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
        addr = up.addr(),
        ca = dir.path().join("ca.pem").display()
    );
    let cfg_path = dir.path().join("config.yaml");
    std::fs::write(&cfg_path, yaml).unwrap();
    let config = {
        let _env = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        std::env::set_var("TEST_CONSOLE_KEY", "sk-ant-api03-PLACEHOLDER");
        std::env::set_var("TEST_ADMIN_SECRET", "admin-secret");
        std::env::set_var("TEST_DESKTOP_KEY", "phoebis-testkey");
        phoebis_router::config::load(&cfg_path).unwrap()
    };
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(phoebis_router::app::run(config, async {
        let _ = stop_rx.await;
    }));
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // The store uses the configured file name as given rather than a name of its own.
    assert!(dir.path().join("phoebis.sqlite").exists());

    let client = reqwest::Client::new();
    let head = inference_head(FixtureKind::ApiKey);
    let (line, headers) = phoebis_transport::request::parse_head(head).unwrap();
    let _ = line;
    let mut req = client
        .post(format!(
            "http://127.0.0.1:{proxy_port}/v1/messages?beta=true"
        ))
        .body(body(BodyFixture::ApiKey).to_vec());
    for (n, v) in headers {
        match n.to_ascii_lowercase().as_str() {
            "x-api-key" => {
                req = req.header("x-api-key", "phoebis-testkey");
            }
            "host" | "content-length" => {}
            _ => {
                req = req.header(n, v);
            }
        }
    }
    let resp = req.send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "{\"ok\":true}");
    let got = &up.requests()[0];
    assert!(got
        .headers
        .iter()
        .any(|(n, v)| n == "x-api-key" && v == "sk-ant-api03-PLACEHOLDER"));
    assert_eq!(got.body, body(BodyFixture::ApiKey));

    let hello = client
        .head(format!("http://127.0.0.1:{proxy_port}/api/hello"))
        .send()
        .await
        .unwrap();
    assert_eq!(hello.status(), 200);
    let admin = client
        .post(format!("http://127.0.0.1:{admin_port}/admin/session"))
        .body(r#"{"secret":"admin-secret"}"#)
        .header("content-type", "application/json")
        .send()
        .await
        .unwrap();
    assert_eq!(admin.status(), 204);
    // Merging the admin router into the listener's app drops its own fallback, so an unknown admin path
    // must still be refused rather than answered by whatever axum falls back to.
    let unknown = client
        .get(format!("http://127.0.0.1:{admin_port}/admin/nope"))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 401);
    assert_eq!(unknown.headers()["cache-control"], "no-store");

    let _ = stop_tx.send(());
    server.await.unwrap().unwrap();
    up.shutdown().await;
}

/// Writes a configuration built from the given admin and routes blocks, and returns its path.
fn config_with(dir: &std::path::Path, name: &str, admin: &str, routes: &str) -> std::path::PathBuf {
    let key = dir.join("storage.key");
    std::fs::write(
        &key,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    )
    .unwrap();
    let yaml = format!(
        r#"accounts:
  - id: console
    kind: api_key
    label: Console
    secret_env: TEST_CONSOLE_KEY
admin:
{admin}
compatibility:
  outbound_gzip: false
  profile: claude-code-2.1.269-linux-x64
  usage_entrypoint: cli
proxy:
  listen: 127.0.0.1:0
routes:
{routes}
storage:
  encryption_key_file: {key}
  path: {db}
upstream:
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
    std::env::set_var("TEST_CONSOLE_KEY", "sk-ant-api03-PLACEHOLDER");
    std::env::set_var("TEST_ADMIN_SECRET", "admin-secret");
    std::env::set_var("TEST_DESKTOP_KEY", "phoebis-desktop");
    std::env::set_var("TEST_LAPTOP_KEY", "phoebis-desktop");
    let loopback_dev =
        "  listen: 127.0.0.1:8081\n  secret_env: TEST_ADMIN_SECRET\n  dev_loopback_http: true";
    let public_dev =
        "  listen: 0.0.0.0:8081\n  secret_env: TEST_ADMIN_SECRET\n  dev_loopback_http: true";
    let public_plain =
        "  listen: 0.0.0.0:8081\n  secret_env: TEST_ADMIN_SECRET\n  dev_loopback_http: false";
    let one = "  - key_id: desktop\n    secret_env: TEST_DESKTOP_KEY\n    account_id: console";
    let two = format!(
        "{one}\n  - key_id: laptop\n    secret_env: TEST_LAPTOP_KEY\n    account_id: console"
    );
    assert!(load(&config_with(dir.path(), "ok", loopback_dev, one)).is_ok());
    assert!(matches!(
        load(&config_with(dir.path(), "dup", loopback_dev, &two)),
        Err(ConfigError::DuplicateSecret { .. })
    ));
    assert!(matches!(
        load(&config_with(dir.path(), "dev", public_dev, one)),
        Err(ConfigError::DevModeNotLoopback { .. })
    ));
    assert!(matches!(
        load(&config_with(dir.path(), "origin", public_plain, one)),
        Err(ConfigError::MissingOrigin)
    ));
    std::env::set_var("TEST_LAPTOP_KEY", "admin-secret");
    assert!(matches!(
        load(&config_with(dir.path(), "admin_dup", loopback_dev, &two)),
        Err(ConfigError::DuplicateSecret { .. })
    ));

    // A storage path that names no database file inside a directory never reaches the store.
    let text = std::fs::read_to_string(config_with(dir.path(), "ok", loopback_dev, one)).unwrap();
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
