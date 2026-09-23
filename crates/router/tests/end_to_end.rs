//! The binary from the outside: one fixture request relayed through a configured API-key account, and the
//! bootstrap rules that refuse a configuration nobody should be able to start.

use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
use fake_upstream::{FakeUpstream, Respond, Rule};
use std::net::TcpListener;
use std::sync::Mutex;

/// Secrets are read from the process environment inside `load`, and the tests in this binary name the same
/// variables with different values, so each holds this across its whole set-then-load sequence.
static ENV: Mutex<()> = Mutex::new(());

/// How long a poll waits for the binary's asynchronous pipeline (the routing task, the ledger's writer, an
/// admin read) to converge. Generous on purpose: the assertion behind it is "it converges", never "within
/// five seconds", and the loaded workspace run is the case it has to survive.
const SETTLE: std::time::Duration = std::time::Duration::from_secs(30);

/// The listeners bind on the server task's first turns, so a boot is waited out by retrying the admin login
/// until it answers rather than by a fixed sleep. The login that succeeds leaves the session cookie in the
/// client's jar, which is what every admin call after it needs.
async fn logged_in(client: &reqwest::Client, session_url: &str) {
    let give_up = std::time::Instant::now() + SETTLE;
    let attempts = async {
        loop {
            let answered = client
                .post(session_url)
                .header("content-type", "application/json")
                .body(r#"{"secret":"admin-secret-001"}"#)
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
    };
    // The deadline above is read between attempts; the timeout covers an attempt that never returns.
    if tokio::time::timeout(SETTLE, attempts).await.is_err() {
        panic!("{session_url} did not complete the login within {SETTLE:?}");
    }
}

/// How many ports one test process draws from: a hundred slices of a hundred fill `20_000..30_000`, all of it
/// below the kernel's ephemeral range (`net.ipv4.ip_local_port_range`, 32768 upward on Linux).
const PORT_SPAN: u16 = 100;

/// Where the kernel's own ephemeral range starts (`net.ipv4.ip_local_port_range`, 32768 upward on Linux); the
/// allocator's whole span must stay below it.
const EPHEMERAL_FLOOR: u16 = 32_768;

/// The first port of the slice for a given pid residue: pulled out of `port_base` so the bounds test can check
/// every residue's arithmetic without drawing from the counter `free_port` shares with the rest of this
/// process's tests.
fn slice_base(residue: u32) -> u16 {
    20_000 + residue as u16 * PORT_SPAN
}

/// The first port of this process's slice, spread by pid so two test processes rarely share one.
fn port_base() -> u16 {
    slice_base(std::process::id() % 100)
}

/// A port no other test in this binary is handed, from a hundred-port slice below the range the kernel picks
/// ephemeral ports from, so a concurrent test's outbound connection cannot take it between here and the bind
/// that uses it. Probed once so a port another process holds is skipped; a slice with no free port panics.
/// Rests on one precondition, asserted below: the cursor advances fewer than `PORT_SPAN` times in one
/// process, one advance per draw and one per port a foreign listener holds.
fn free_port() -> u16 {
    static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
    let base = port_base();
    // Every pass through the loop takes the next offset, so a probe that finds a port another process holds
    // burns one rather than retrying it; the probe's socket is dropped at once. One full pass with no free
    // port is a host with a hundred listeners in this slice, reported by name rather than looped on.
    for _ in 0..PORT_SPAN {
        // The raw counter rather than the ports handed out, because a slice another process is filling burns
        // advances on failed probes: a counter that wrapped would hand one port out twice, silently.
        let raw = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert!(
            raw < PORT_SPAN,
            "end_to_end.rs advanced its port cursor past {PORT_SPAN} in one process; a test is drawing ports it never binds, foreign listeners are filling the slice, or the slice must grow"
        );
        let port = base + raw;
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
    panic!("no free port in {base}..{}", base + PORT_SPAN);
}

#[test]
fn every_pid_slice_stays_below_the_ephemeral_range() {
    // Checked once rather than per residue: even the highest slice never reaches the kernel's own range.
    assert!(
        20_000u32 + 100 * PORT_SPAN as u32 <= EPHEMERAL_FLOOR as u32,
        "the allocator's full span reaches the kernel's ephemeral range"
    );
    // Arithmetic only: a live draw per residue would draw from the counter every other test in this process
    // shares, which is the bug this test used to cause.
    for residue in 0..100u32 {
        let base = slice_base(residue);
        assert!(
            base as u32 >= 20_000,
            "residue {residue} starts below the allocator's floor: base {base}"
        );
        assert!(
            base as u32 + PORT_SPAN as u32 <= 20_000 + 100 * PORT_SPAN as u32,
            "residue {residue} pushes its slice past the allocator's own span: base {base}"
        );
    }
    // One live draw, which cannot wrap the shared counter by itself, proves free_port actually lands inside
    // this process's own slice.
    let port = free_port();
    let base = slice_base(std::process::id() % 100);
    assert!(
        (base..base + PORT_SPAN).contains(&port),
        "{port} is outside this process's slice ({base}..{})",
        base + PORT_SPAN
    );
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

/// Replays a fixture's head and body to the binary byte for byte: the fixture's request line, then every header
/// line in the fixture's order and spelling, with only the credential, the session id, the host, and the framing
/// replaced. Answers the response's status.
async fn replay_fixture(
    proxy_port: u16,
    head: FixtureKind,
    fixture: BodyFixture,
    secret: &str,
    session: &str,
) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let body = body(fixture).to_vec();
    let (request_line, headers) = phoebis_transport::request::parse_head(inference_head(head)).unwrap();
    let mut wire = format!("{request_line}\r\n");
    for (name, value) in &headers {
        // Matched on the lowercase name, written back under the fixture's own spelling.
        let line = match name.to_ascii_lowercase().as_str() {
            "authorization" => format!("{name}: Bearer {secret}"),
            "content-length" => format!("{name}: {}", body.len()),
            "host" => format!("{name}: 127.0.0.1:{proxy_port}"),
            "x-api-key" => format!("{name}: {secret}"),
            "x-claude-code-session-id" => format!("{name}: {session}"),
            _ => format!("{name}: {value}"),
        };
        wire.push_str(&line);
        wire.push_str("\r\n");
    }
    wire.push_str("\r\n");
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", proxy_port))
        .await
        .unwrap();
    socket.write_all(wire.as_bytes()).await.unwrap();
    socket.write_all(&body).await.unwrap();
    // Read to the end of the response head and no further: the connection is keep-alive, as the fixture's own
    // `Connection` line says, so nothing closes it for us.
    let mut answer = Vec::new();
    let mut chunk = [0u8; 4096];
    while !answer.windows(4).any(|w| w == b"\r\n\r\n") {
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => answer.extend_from_slice(&chunk[..n]),
        }
    }
    String::from_utf8_lossy(&answer)
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .expect("an HTTP status line")
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
  plain_http: true
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
        std::env::set_var("TEST_ADMIN_SECRET", "admin-secret-001");
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
    // Asked without the session cookie, so the gate refuses before the route is reached and an anonymous
    // caller learns nothing about which admin paths exist.
    let anonymous = reqwest::Client::new();
    let unknown = anonymous.get(admin("/nope")).send().await.unwrap();
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

#[tokio::test]
async fn no_socket_is_accepted_on_the_admin_listener_once_shutdown_has_returned() {
    let dir = tempfile::tempdir().unwrap();
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
  secret_env: TEST_SHUTDOWN_ADMIN
  plain_http: true
compatibility:
  outbound_gzip: false
  profile: claude-code-2.1.269-linux-x64
  usage_entrypoint: cli
proxy:
  listen: 127.0.0.1:{proxy_port}
stats:
  ledger_path: {ledger}
  metrics_listen: 127.0.0.1:{metrics_port}
storage:
  encryption_key_file: {key}
  path: {db}
usage:
  cache_ttl_seconds: 60
  refresh_concurrency: 2
"#,
        key = dir.path().join("storage.key").display(),
        db = dir.path().join("phoebis.sqlite").display(),
        ledger = dir.path().join("ledger.sqlite").display(),
    );
    let cfg_path = dir.path().join("config.yaml");
    std::fs::write(&cfg_path, yaml).unwrap();
    let config = {
        let _env = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        std::env::set_var("TEST_SHUTDOWN_ADMIN", "admin-secret-shutdown");
        phoebis_router::config::load(&cfg_path).unwrap()
    };
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(phoebis_router::app::run(config, async {
        let _ = stop_rx.await;
    }));
    // The listener binds on the server task's first turns, so a connect is retried until it succeeds rather
    // than raced with the boot, the same way `logged_in` waits out the boot elsewhere in this file.
    let give_up = std::time::Instant::now() + SETTLE;
    loop {
        if tokio::net::TcpStream::connect(("127.0.0.1", admin_port))
            .await
            .is_ok()
        {
            break;
        }
        assert!(
            std::time::Instant::now() < give_up,
            "the admin listener never accepted a connection"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let _ = stop_tx.send(());
    server.await.unwrap().unwrap();
    let refused = tokio::net::TcpStream::connect(("127.0.0.1", admin_port)).await;
    assert!(
        refused.is_err(),
        "a connection was accepted on the admin port after shutdown had already returned"
    );
}

/// A unified 429 on one seat fails the client's
/// request over to its sibling inside a single call, and the log, the cooldown, and the session all show it.
#[tokio::test]
async fn a_forced_429_on_one_seat_migrates_the_session_through_the_binary() {
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
  plain_http: true
compatibility:
  outbound_gzip: false
  profile: claude-code-2.1.269-linux-x64
  usage_entrypoint: cli
proxy:
  listen: 127.0.0.1:{proxy_port}
routing:
  cooldown_fallback_seconds: 3600
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
        std::env::set_var("TEST_ADMIN_SECRET", "admin-secret-001");
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

    // Distinct priorities give the first selection a deterministic winner: `least_utilized` only orders
    // candidates within a tier, and each account here is alone in its own.
    for (id, secret) in [("a", "sk-ant-api03-AAAA"), ("b", "sk-ant-api03-BBBB")] {
        assert_eq!(
            client
                .post(admin("/accounts"))
                .json(&serde_json::json!({ "id": id, "kind": "api_key", "label": id, "secret": secret }))
                .send()
                .await
                .unwrap()
                .status(),
            201
        );
        up.add_credential_rule(
            secret,
            "/v1/messages",
            Respond::Json {
                status: 200,
                headers: vec![],
                body: b"{\"ok\":true}".to_vec(),
            },
        );
    }
    assert_eq!(
        client
            .post(admin("/pools"))
            .json(&serde_json::json!({ "id": "main", "label": "Main", "strategy": "least_utilized", "members": [{ "account_id": "a", "priority": 0 }, { "account_id": "b", "priority": 1 }] }))
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

    up.push_credential_rule("sk-ant-api03-AAAA", "/v1/messages", exhausted_429());
    let first = fixture_request(&client, proxy_port, &secret, "live-4")
        .send()
        .await
        .unwrap();
    assert_eq!(
        first.status(),
        200,
        "the 429 on a fails over to b inside one client request"
    );
    let sessions = wait_json(&client, &admin("/sessions"), |v| {
        v["sessions"][0]["account_id"] == "b"
    })
    .await;
    assert_eq!(sessions["sessions"][0]["account_id"], "b");
    // The request row lands through the ledger's own async writer, behind the session pin the router updates
    // synchronously; waited on rather than fetched once, so a loaded test run cannot read it half-written.
    let log = wait_json(&client, &admin("/requests?limit=1"), |v| {
        v["requests"][0]["attempts"]
            .as_array()
            .is_some_and(|a| a.len() >= 2)
    })
    .await;
    let attempts = &log["requests"][0]["attempts"];
    assert_eq!(
        (
            attempts[0]["account_id"].as_str(),
            attempts[0]["upstream_status"].as_u64(),
            attempts[1]["account_id"].as_str()
        ),
        (Some("a"), Some(429), Some("b"))
    );
    assert!(
        attempts[0]["cooldown_set"]["until"].is_number(),
        "the log carries the cooldown the 429 set"
    );
    let events: serde_json::Value = client
        .get(admin(&format!(
            "/requests/{}",
            log["requests"][0]["id"].as_str().unwrap()
        )))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "pin_migrated" || e["kind"] == "cooldown_set"),
        "{events}"
    );
    let a: serde_json::Value = client
        .get(admin("/accounts/a"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(a["cooldowns"][0]["scope"], "account");

    let _ = stop_tx.send(());
    server.await.unwrap().unwrap();
    up.shutdown().await;
}

/// The plan-quota sibling takes a session once its
/// current account reports overage, under the pool's default `prefer_plan` policy.
#[tokio::test]
async fn a_session_crossing_into_overage_migrates_under_prefer_plan_through_the_binary() {
    use phoebis_profile::fixtures::{control_response, ControlFixture};
    let control = |which: ControlFixture| Respond::Json {
        status: 200,
        headers: vec![],
        body: control_response(which).to_vec(),
    };
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
        Rule {
            path_prefix: "/v1/oauth/token".into(),
            respond: control(ControlFixture::Token),
        },
        Rule {
            path_prefix: "/api/oauth/profile".into(),
            respond: control(ControlFixture::Profile),
        },
        Rule {
            path_prefix: "/api/oauth/claude_cli/roles".into(),
            respond: control(ControlFixture::Roles),
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
  plain_http: true
compatibility:
  outbound_gzip: false
  profile: claude-code-2.1.269-linux-x64
  usage_entrypoint: cli
proxy:
  listen: 127.0.0.1:{proxy_port}
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
        std::env::set_var("TEST_ADMIN_SECRET", "admin-secret-001");
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

    // Pairs an oauth account through the router's own flow: start, callback, confirm.
    let pair = |account_id: &'static str| {
        let client = client.clone();
        async move {
            assert_eq!(
                client
                    .post(admin("/accounts"))
                    .json(&serde_json::json!({ "id": account_id, "kind": "oauth", "label": account_id }))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                201
            );
            let started: serde_json::Value = client
                .post(admin("/oauth/sessions"))
                .json(&serde_json::json!({ "account_id": account_id }))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let (id, redirect) = (
                started["id"].as_str().unwrap().to_string(),
                started["redirect_uri"].as_str().unwrap().to_string(),
            );
            let state = started["authorization_url"]
                .as_str()
                .unwrap()
                .split("state=")
                .nth(1)
                .unwrap()
                .to_string();
            assert_eq!(
                client
                    .post(admin(&format!("/oauth/sessions/{id}/callback")))
                    .json(&serde_json::json!({ "url": format!("{redirect}?code=CODE-{account_id}&state={state}") }))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                200
            );
            assert_eq!(
                client
                    .post(admin(&format!("/oauth/sessions/{id}/confirm")))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                200
            );
        }
    };
    pair("a").await;
    // The identity a slot binds to comes from the profile lookup, not from the token response, so both are
    // switched once `a`'s pairing has consumed them (one token exchange, two profile lookups): the store's
    // unique index over the bound upstream account would otherwise refuse `b`'s confirm outright.
    let second_token = String::from_utf8(control_response(ControlFixture::Token).to_vec())
        .unwrap()
        .replace(
            "00000000-0000-4000-8000-000000000001",
            "00000000-0000-4000-8000-000000000002",
        )
        .replace("sk-ant-oat01-PLACEHOLDER", "sk-ant-oat01-SECOND");
    up.set_rule_after(
        1,
        "/v1/oauth/token",
        Respond::Json {
            status: 200,
            headers: vec![],
            body: second_token.into_bytes(),
        },
    );
    let second_profile = String::from_utf8(control_response(ControlFixture::Profile).to_vec())
        .unwrap()
        .replace(
            "00000000-0000-4000-8000-000000000001",
            "00000000-0000-4000-8000-000000000002",
        );
    up.set_rule_after(
        2,
        "/api/oauth/profile",
        Respond::Json {
            status: 200,
            headers: vec![],
            body: second_profile.into_bytes(),
        },
    );
    pair("b").await;

    let overage_head = |in_use: &str| Respond::Json {
        status: 200,
        headers: vec![
            (
                "anthropic-ratelimit-unified-status".into(),
                "allowed".into(),
            ),
            (
                "anthropic-ratelimit-unified-5h-status".into(),
                "allowed".into(),
            ),
            (
                "anthropic-ratelimit-unified-5h-utilization".into(),
                "0.5".into(),
            ),
            (
                "anthropic-ratelimit-unified-5h-reset".into(),
                (jiff::Timestamp::now().as_second() + 3_600).to_string(),
            ),
            (
                "anthropic-ratelimit-unified-7d-status".into(),
                "allowed".into(),
            ),
            (
                "anthropic-ratelimit-unified-7d-utilization".into(),
                "0.5".into(),
            ),
            (
                "anthropic-ratelimit-unified-7d-reset".into(),
                (jiff::Timestamp::now().as_second() + 86_400).to_string(),
            ),
            (
                "anthropic-ratelimit-unified-overage-in-use".into(),
                in_use.into(),
            ),
        ],
        body: b"{\"ok\":true}".to_vec(),
    };
    up.add_credential_rule(
        "sk-ant-oat01-PLACEHOLDER",
        "/v1/messages",
        overage_head("true"),
    );
    up.add_credential_rule("sk-ant-oat01-SECOND", "/v1/messages", overage_head("false"));

    assert_eq!(
        client
            .post(admin("/pools"))
            .json(&serde_json::json!({ "id": "main", "label": "Main", "strategy": "priority_random", "members": [{ "account_id": "a", "priority": 0 }, { "account_id": "b", "priority": 1 }] }))
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

    let first = fixture_request(&client, proxy_port, &secret, "live-5")
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    let a: serde_json::Value = wait_json(&client, &admin("/accounts/a"), |v| {
        v["overage"]["in_use"] == true
    })
    .await;
    assert_eq!(
        a["overage"]["in_use"], true,
        "the head is the overage evidence"
    );
    let second = fixture_request(&client, proxy_port, &secret, "live-5")
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 200);
    let sessions = wait_json(&client, &admin("/sessions"), |v| {
        v["sessions"][0]["account_id"] == "b"
    })
    .await;
    assert_eq!(
        sessions["sessions"][0]["account_id"], "b",
        "prefer_plan moved the pin to the plan-quota sibling"
    );
    // Waited on for the same reason as the first rehearsal: the second request's row lands through the
    // ledger's async writer, behind the session pin the router already moved.
    let log = wait_json(&client, &admin("/requests?limit=2"), |v| {
        v["requests"].as_array().is_some_and(|r| r.len() >= 2)
            && v["requests"][0]["attempts"][0]["overage_in_use"].is_boolean()
    })
    .await;
    assert_eq!(log["requests"][0]["attempts"][0]["overage_in_use"], false);
    let detail: serde_json::Value = client
        .get(admin(&format!(
            "/requests/{}",
            log["requests"][0]["id"].as_str().unwrap()
        )))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        detail["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "pin_migrated" && e["detail"].to_string().contains("overage")),
        "{detail}"
    );

    let _ = stop_tx.send(());
    server.await.unwrap().unwrap();
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
fn shared_secrets_are_rejected_and_plain_http_is_allowed_on_any_address() {
    use phoebis_router::config::{load, ConfigError};
    let dir = tempfile::tempdir().unwrap();
    let _env = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    std::env::set_var("TEST_ADMIN_SECRET", "admin-secret-001");
    std::env::set_var("TEST_VIEWER_SECRET", "viewer-secret-01");
    std::env::set_var("TEST_SHARED_SECRET", "admin-secret-001");
    let loopback_plain = "  listen: 127.0.0.1:8081\n  secret_env: TEST_ADMIN_SECRET\n  viewer_secret_env: TEST_VIEWER_SECRET\n  plain_http: true";
    let shared = "  listen: 127.0.0.1:8081\n  secret_env: TEST_ADMIN_SECRET\n  viewer_secret_env: TEST_SHARED_SECRET\n  plain_http: true";
    let public_plain =
        "  listen: 0.0.0.0:8081\n  secret_env: TEST_ADMIN_SECRET\n  plain_http: true";
    let public_plain_with_origin =
        "  listen: 0.0.0.0:8081\n  origin: http://phoebis.lan:8081\n  secret_env: TEST_ADMIN_SECRET\n  plain_http: true";
    let public_secure =
        "  listen: 0.0.0.0:8081\n  secret_env: TEST_ADMIN_SECRET\n  plain_http: false";
    assert!(load(&config_with(dir.path(), "ok", loopback_plain)).is_ok());
    assert!(matches!(
        load(&config_with(dir.path(), "dup", shared)),
        Err(ConfigError::DuplicateSecret { .. })
    ));
    assert!(load(&config_with(dir.path(), "public", public_plain)).is_ok());
    let pinned = load(&config_with(dir.path(), "pinned", public_plain_with_origin)).unwrap();
    assert_eq!(pinned.admin.origin.as_deref(), Some("http://phoebis.lan:8081"));
    assert!(matches!(
        load(&config_with(dir.path(), "origin", public_secure)),
        Err(ConfigError::MissingOrigin)
    ));

    // A storage path that names no database file inside a directory never reaches the store.
    let text = std::fs::read_to_string(config_with(dir.path(), "ok", loopback_plain)).unwrap();
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
    std::env::set_var("TEST_ADMIN_SECRET", "admin-secret-001");
    let base = "  listen: 127.0.0.1:8081\n  secret_env: TEST_ADMIN_SECRET\n  plain_http: true";
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
    let full = load(&with("full", "stats:\n  body_capture_max_hours: 24\n  ledger_path: ./data/l.sqlite\n  metrics_listen: 127.0.0.1:9999\n  pricing_path: ./p.yaml\n  retention_days: 7\n  rollup_retention_days: 500\nquota:\n  poll_interval_seconds: 900\n  sample_retention_days: 3\n")).unwrap();
    assert_eq!(
        (
            full.stats.retention_days,
            full.stats.rollup_retention_days,
            full.quota.poll_interval_seconds,
            full.quota.sample_retention_days
        ),
        (7, 500, 900, 3)
    );
    assert!(matches!(
        load(&with("poll", "quota:\n  poll_interval_seconds: 899\n")),
        Err(ConfigError::PollIntervalTooShort)
    ));
    let zero_retention = load(&with("retention", "stats:\n  retention_days: 0\n")).unwrap();
    assert_eq!(zero_retention.stats.retention_days, 0);
    let zero_rollup = load(&with("rollup", "stats:\n  rollup_retention_days: 0\n")).unwrap();
    assert_eq!(zero_rollup.stats.rollup_retention_days, 0);
    assert!(matches!(
        load(&with(
            "rollup-too-short",
            "stats:\n  retention_days: 30\n  rollup_retention_days: 7\n"
        )),
        Err(ConfigError::RollupRetentionTooShort {
            retention_days: 30,
            value: 7
        })
    ));
    let zero_raw_short_rollup = load(&with(
        "zero-raw-short-rollup",
        "stats:\n  retention_days: 0\n  rollup_retention_days: 7\n",
    ))
    .unwrap();
    assert_eq!(
        (
            zero_raw_short_rollup.stats.retention_days,
            zero_raw_short_rollup.stats.rollup_retention_days
        ),
        (0, 7),
        "no floor when the raw rows are kept forever"
    );
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
    // One file under four spellings. The registry opened as the ledger skips every ledger migration and
    // leaves the writer without its columns, so a name that only looks different has to be refused too.
    let aliased = |name: &str, storage: &str, ledger: &str| {
        let path = dir.path().join(format!("{name}.yaml"));
        std::fs::write(
            &path,
            format!(
                "{}stats:\n  ledger_path: {ledger}\n",
                text.replace(
                    &format!("path: {}", dir.path().join("router.sqlite").display()),
                    &format!("path: {storage}")
                )
            ),
        )
        .unwrap();
        load(&path)
    };
    let here = std::env::current_dir().unwrap();
    let cases = [
        (
            "dot",
            "./data/x.sqlite".to_string(),
            "data/x.sqlite".to_string(),
        ),
        (
            "absolute",
            "data/x.sqlite".to_string(),
            here.join("data/x.sqlite").display().to_string(),
        ),
        (
            "parent",
            "data/x.sqlite".to_string(),
            "a/../data/x.sqlite".to_string(),
        ),
    ];
    for (name, storage, ledger) in cases {
        assert!(
            matches!(
                aliased(name, &storage, &ledger),
                Err(ConfigError::SharedDatabasePath { .. })
            ),
            "{ledger} is {storage} spelled differently"
        );
    }
    std::fs::write(dir.path().join("router.sqlite"), b"").unwrap();
    std::os::unix::fs::symlink(
        dir.path().join("router.sqlite"),
        dir.path().join("link.sqlite"),
    )
    .unwrap();
    assert!(
        matches!(
            aliased(
                "symlink",
                &dir.path().join("router.sqlite").display().to_string(),
                &dir.path().join("link.sqlite").display().to_string()
            ),
            Err(ConfigError::SharedDatabasePath { .. })
        ),
        "a link to the registry is the registry"
    );
    assert!(
        aliased(
            "distinct",
            &dir.path().join("router.sqlite").display().to_string(),
            &dir.path().join("ledger.sqlite").display().to_string()
        )
        .is_ok(),
        "two names for two files are still two files"
    );
    assert!(
        load(&with("unknown", "stats:\n  nope: 1\n")).is_err(),
        "unknown fields reject the configuration"
    );
}

#[test]
fn keepalive_limits_are_validated_with_the_go_engines_bounds() {
    use phoebis_router::config::{load, ConfigError};
    let dir = tempfile::tempdir().unwrap();
    let _env = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    std::env::set_var("TEST_ADMIN_SECRET", "admin-secret-001");
    let base = "  listen: 127.0.0.1:8081\n  secret_env: TEST_ADMIN_SECRET\n  plain_http: true";
    let text = std::fs::read_to_string(config_with(dir.path(), "ok", base)).unwrap();
    let with = |name: &str, extra: &str| {
        let path = dir.path().join(format!("{name}.yaml"));
        std::fs::write(&path, format!("{text}{extra}")).unwrap();
        path
    };
    let defaults = load(&with("defaults", "")).unwrap();
    assert_eq!(
        (
            defaults.keepalive.lease_seconds,
            defaults.keepalive.max_workers,
            defaults.keepalive.oauth_prewarm_verified
        ),
        (90, 4, false),
        "an absent block is the spec's defaults"
    );
    let full = load(&with("full", "keepalive:\n  lease_seconds: 120\n  max_agents: 2048\n  max_request_bytes: 1048576\n  max_retained_bytes: 16777216\n  max_sessions: 256\n  max_workers: 8\n  oauth_prewarm_verified: true\n")).unwrap();
    assert_eq!(
        (
            full.keepalive.lease_seconds,
            full.keepalive.max_agents,
            full.keepalive.max_workers,
            full.keepalive.oauth_prewarm_verified
        ),
        (120, 2_048, 8, true)
    );
    assert!(matches!(
        load(&with("lease", "keepalive:\n  lease_seconds: 9\n")),
        Err(ConfigError::Keepalive(
            phoebis_keepalive::LimitsError::OutOfRange {
                field: "lease_seconds",
                ..
            }
        ))
    ));
    assert!(matches!(
        load(&with(
            "retained",
            "keepalive:\n  max_request_bytes: 4194304\n  max_retained_bytes: 2097152\n"
        )),
        Err(ConfigError::Keepalive(
            phoebis_keepalive::LimitsError::RetainedBelowRequest
        ))
    ));
    assert!(
        matches!(
            load(&with("unknown", "keepalive:\n  probe_enabled: true\n")),
            Err(ConfigError::Syntax(_))
        ),
        "the Go engine's probe_enabled is not a field of the block; probing is a pool setting"
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
        "admin:\n  listen: 127.0.0.1:{admin_port}\n  secret_env: TEST_ADMIN_SECRET\n  plain_http: true\ncompatibility:\n  outbound_gzip: false\n  profile: claude-code-2.1.269-linux-x64\n  usage_entrypoint: cli\nproxy:\n  listen: 127.0.0.1:{proxy_port}\nstats:\n  ledger_path: {ledger}\n  metrics_listen: 127.0.0.1:{metrics_port}\n  pricing_path: {pricing}\nstorage:\n  encryption_key_file: {key}\n  path: {db}\nupstream:\n  resolve_override: {addr}\n  extra_ca_pem: {ca}\nusage:\n  cache_ttl_seconds: 60\n  refresh_concurrency: 2\n",
        ledger = dir.path().join("ledger.sqlite").display(), pricing = dir.path().join("pricing.yaml").display(),
        key = dir.path().join("storage.key").display(), db = dir.path().join("phoebis.sqlite").display(),
        addr = up.addr(), ca = dir.path().join("ca.pem").display(),
    );
    let cfg_path = dir.path().join("config.yaml");
    std::fs::write(&cfg_path, yaml).unwrap();
    let config = {
        let _env = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        std::env::set_var("TEST_ADMIN_SECRET", "admin-secret-001");
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
    let give_up = std::time::Instant::now() + SETTLE;
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
    // `wait_json` sends a bare GET, so the key rides in the client's default headers.
    let mut keyed = reqwest::header::HeaderMap::new();
    keyed.insert(
        reqwest::header::AUTHORIZATION,
        format!("Bearer {secret}").parse().unwrap(),
    );
    let keyed = reqwest::Client::builder()
        .default_headers(keyed)
        .build()
        .unwrap();
    let usage = wait_json(
        &keyed,
        &format!("http://127.0.0.1:{proxy_port}/v1/usage"),
        |v| {
            v["windows"]["24h"]["requests"]
                .as_u64()
                .is_some_and(|n| n >= 1)
        },
    )
    .await;
    assert_eq!(usage["key"]["id"], "desktop");
    assert!(usage["key"].get("secret").is_none() && usage.to_string().find("sk-ant-").is_none());
    let _ = stop_tx.send(());
    server.await.unwrap().unwrap();
    let ledger = phoebis_ledger::LedgerDb::open(&dir.path().join("ledger.sqlite")).unwrap();
    let (rows, _) = ledger
        .read_blocking(|c| {
            phoebis_ledger::list_requests(
                c,
                &phoebis_ledger::RequestFilter::default(),
                &phoebis_ledger::LiveKeys::default(),
                None,
                10,
            )
        })
        .unwrap();
    assert_eq!(
        rows.len(),
        4,
        "the last request's record was on disk when run returned"
    );
    assert!(rows.iter().all(|(r, _)| r.bodies_captured));
    up.shutdown().await;
}

/// A session through the binary is retained, probed on the ticker once its cache entry nears expiry, and shown
/// on every view: the recipes, the overview, the sessions row, the request log under `kind = probe`, and the
/// metrics. The pool's lead is set so the first probe is due one second after the request.
#[tokio::test]
async fn the_binary_retains_a_session_probes_it_and_serves_the_keepalive_views() {
    let up = FakeUpstream::start(vec![
        Rule { path_prefix: "/v1/messages".into(), respond: Respond::Json { status: 200, headers: vec![], body: br#"{"id":"msg_1","type":"message","content":[],"usage":{"input_tokens":1,"cache_read_input_tokens":2000,"cache_creation_input_tokens":2000,"cache_creation":{"ephemeral_5m_input_tokens":2000,"ephemeral_1h_input_tokens":0},"output_tokens":0}}"#.to_vec() } },
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
    let (proxy_port, admin_port, metrics_port) = (free_port(), free_port(), free_port());
    let yaml = format!(
        "admin:\n  listen: 127.0.0.1:{admin_port}\n  secret_env: TEST_ADMIN_SECRET\n  plain_http: true\ncompatibility:\n  outbound_gzip: false\n  profile: claude-code-2.1.269-linux-x64\n  usage_entrypoint: cli\nkeepalive:\n  max_workers: 2\nproxy:\n  listen: 127.0.0.1:{proxy_port}\nstats:\n  ledger_path: {ledger}\n  metrics_listen: 127.0.0.1:{metrics_port}\nstorage:\n  encryption_key_file: {key}\n  path: {db}\nupstream:\n  resolve_override: {addr}\n  extra_ca_pem: {ca}\nusage:\n  cache_ttl_seconds: 60\n  refresh_concurrency: 2\n",
        ledger = dir.path().join("ledger.sqlite").display(), key = dir.path().join("storage.key").display(), db = dir.path().join("phoebis.sqlite").display(), addr = up.addr(), ca = dir.path().join("ca.pem").display(),
    );
    let cfg_path = dir.path().join("config.yaml");
    std::fs::write(&cfg_path, yaml).unwrap();
    let config = {
        let _env = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        std::env::set_var("TEST_ADMIN_SECRET", "admin-secret-001");
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
    // A lead of 299 s on a five-minute entry puts the first probe one second after the request.
    assert_eq!(client.post(admin("/pools")).json(&serde_json::json!({ "id": "main", "label": "Main", "strategy": "priority_random", "members": [{ "account_id": "console", "priority": 0 }], "keepalive": { "enabled": true, "lead_seconds_5m": 299, "jitter_seconds_5m": 0 } })).send().await.unwrap().status(), 201);
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
    // The fixture body asks for fixed-budget thinking, which no probe may replay; an adaptive block is the
    // probe-capable shape.
    let adaptive = String::from_utf8(body(BodyFixture::ApiKey).to_vec())
        .unwrap()
        .replacen(
            r#""thinking":{"budget_tokens":31999,"type":"enabled","display":"omitted"}"#,
            r#""thinking":{"type":"adaptive"}"#,
            1,
        );
    let mut req = fixture_request(&client, proxy_port, &secret, "e2e-warm");
    req = req.body(adaptive.clone());
    let resp = req.send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.text().await.unwrap();
    // Registered through the companion route under the key, so the recipe is retained.
    let events = format!("http://127.0.0.1:{proxy_port}/v0/resource/plugins/cache-keepalive/events?event=session_start&session_id=e2e-warm&instance_id=e2e-instance&sequence=1");
    let ack = client
        .get(&events)
        .header("authorization", format!("Bearer {secret}"))
        .send()
        .await
        .unwrap();
    assert_eq!(ack.status(), 200);
    assert_eq!(
        client.get(&events).send().await.unwrap().status(),
        401,
        "no key, no event"
    );
    let resp = fixture_request(&client, proxy_port, &secret, "e2e-warm")
        .body(adaptive.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.text().await.unwrap();
    let recipes = wait_json(&client, &admin("/keepalive/recipes"), |v| {
        !v["recipes"].as_array().unwrap().is_empty()
    })
    .await;
    assert_eq!(
        (
            recipes["recipes"][0]["session_id"].as_str(),
            recipes["recipes"][0]["state"].as_str()
        ),
        (Some("e2e-warm"), Some("armed"))
    );
    let log = wait_json(
        &client,
        &admin("/requests?include_probes=true&kinds=probe"),
        |v| !v["requests"].as_array().unwrap().is_empty(),
    )
    .await;
    assert_eq!(
        (
            log["requests"][0]["kind"].as_str(),
            log["requests"][0]["session_id"].as_str(),
            log["requests"][0]["status"].as_u64()
        ),
        (Some("probe"), Some("e2e-warm"), Some(200))
    );
    let probes: Vec<_> = up
        .requests()
        .into_iter()
        .filter(|r| r.body.windows(14).any(|w| w == b"\"max_tokens\":0"))
        .collect();
    assert!(
        !probes.is_empty(),
        "the probe reached the upstream with max_tokens 0"
    );
    assert!(probes[0]
        .headers
        .iter()
        .any(|(n, v)| n == "x-api-key" && v == "sk-ant-api03-PLACEHOLDER"));
    let sessions: serde_json::Value = client
        .get(admin("/sessions"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let row = sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["session_id"] == "e2e-warm")
        .unwrap();
    assert_eq!(
        (
            row["companion"].as_str(),
            row["keepalive"]["state"].as_str(),
            row["pinned"].as_bool()
        ),
        (Some("reporting"), Some("busy"), Some(true))
    );
    let overview: serde_json::Value = client
        .get(admin("/keepalive/overview"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        overview["probes_24h"]["ok"].as_u64().unwrap() >= 1
            || overview["agents"]["busy"].as_u64() == Some(1)
    );
    let settings: serde_json::Value = client
        .get(admin("/settings"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(settings["keepalive"]["max_workers"].as_u64(), Some(2));
    let text = client
        .get(format!("http://127.0.0.1:{metrics_port}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        text.contains(r#"phoebis_keepalive_agents{state="busy"} 1"#),
        "{text}"
    );
    assert!(
        text.contains(r#"phoebis_keepalive_probes_total{account="console",outcome="ok"}"#),
        "{text}"
    );
    let _ = stop_tx.send(());
    server.await.unwrap().unwrap();
    up.shutdown().await;
}

/// Polls an admin route until `ready` accepts its JSON, within the settle budget: the deadline for a route that
/// answers but has not converged, the timeout for one that stops answering mid-request.
async fn wait_json(
    client: &reqwest::Client,
    url: &str,
    ready: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let give_up = std::time::Instant::now() + SETTLE;
    let polls = async {
        loop {
            let value: serde_json::Value =
                client.get(url).send().await.unwrap().json().await.unwrap();
            if ready(&value) {
                return value;
            }
            assert!(
                std::time::Instant::now() < give_up,
                "{url} never settled: {value}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    };
    match tokio::time::timeout(SETTLE, polls).await {
        Ok(value) => value,
        Err(_) => panic!("{url} stopped answering within {SETTLE:?}"),
    }
}

/// The companion, unchanged from its own repository, reports to the binary with nothing but the client's own
/// two variables: the management URL defaults to `ANTHROPIC_BASE_URL`, the token to `CLAUDE_CODE_OAUTH_TOKEN`,
/// and the session lands under the key those name.
#[tokio::test]
async fn the_companion_registers_through_the_binary_under_the_clients_environment() {
    let Some(companion) = std::env::var_os("PHOEBIS_COMPANION_DIR") else {
        println!("PHOEBIS_COMPANION_DIR is unset; the companion integration test did not run");
        return;
    };
    let companion = std::path::PathBuf::from(companion);
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/models".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"data":[{"id":"claude-sonnet-4-5","type":"model"}]}"#.to_vec(),
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
    let (proxy_port, admin_port, metrics_port) = (free_port(), free_port(), free_port());
    let yaml = format!(
        "admin:\n  listen: 127.0.0.1:{admin_port}\n  secret_env: TEST_ADMIN_SECRET\n  plain_http: true\ncompatibility:\n  outbound_gzip: false\n  profile: claude-code-2.1.269-linux-x64\n  usage_entrypoint: cli\nproxy:\n  listen: 127.0.0.1:{proxy_port}\nstats:\n  ledger_path: {ledger}\n  metrics_listen: 127.0.0.1:{metrics_port}\nstorage:\n  encryption_key_file: {key}\n  path: {db}\nupstream:\n  resolve_override: {addr}\n  extra_ca_pem: {ca}\nusage:\n  cache_ttl_seconds: 60\n  refresh_concurrency: 2\n",
        ledger = dir.path().join("ledger.sqlite").display(), key = dir.path().join("storage.key").display(), db = dir.path().join("phoebis.sqlite").display(), addr = up.addr(), ca = dir.path().join("ca.pem").display(),
    );
    let cfg_path = dir.path().join("config.yaml");
    std::fs::write(&cfg_path, yaml).unwrap();
    let config = {
        let _env = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        std::env::set_var("TEST_ADMIN_SECRET", "admin-secret-001");
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
    assert_eq!(client.post(admin("/pools")).json(&serde_json::json!({ "id": "main", "label": "Main", "strategy": "priority_random", "members": [{ "account_id": "console", "priority": 0 }], "keepalive": { "enabled": true } })).send().await.unwrap().status(), 201);
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
    // The companion reports only under a Claude Code ancestor; a python binary named `claude` is one.
    let python = std::path::PathBuf::from(
        String::from_utf8(
            std::process::Command::new("python3")
                .args(["-c", "import sys;print(sys.executable)"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim(),
    );
    let fake_claude = dir.path().join("claude");
    std::fs::copy(&python, &fake_claude).unwrap();
    let payload =
        r#"{"session_id":"e2e-companion","hook_event_name":"SessionStart","source":"startup"}"#;
    let script = format!("import subprocess,sys;sys.exit(subprocess.run([sys.executable,{:?},'report'],input={:?},text=True).returncode)", companion.join("scripts/companion.py").to_str().unwrap(), payload);
    let state_dir = dir.path().join("state");
    // Off the runtime thread: the router serves the companion's registration on this same runtime, and a
    // blocking wait for the child would hold the only thread it has.
    let output = {
        let (fake_claude, script, home, state_dir, secret) = (
            fake_claude.clone(),
            script.clone(),
            dir.path().to_path_buf(),
            state_dir.clone(),
            secret.clone(),
        );
        tokio::task::spawn_blocking(move || {
            std::process::Command::new(&fake_claude)
                .args(["-c", &script])
                .env_clear()
                .env("PATH", std::env::var("PATH").unwrap_or_default())
                .env("HOME", &home)
                .env(
                    "ANTHROPIC_BASE_URL",
                    format!("http://127.0.0.1:{proxy_port}"),
                )
                .env("CLAUDE_CODE_OAUTH_TOKEN", &secret)
                .env("CLAUDE_PLUGIN_DATA", &state_dir)
                .output()
                .unwrap()
        })
        .await
        .unwrap()
    };
    assert!(
        output.status.success(),
        "the companion exited {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty() && output.stderr.is_empty(),
        "a hook is silent: {:?}",
        output
    );
    let state = std::fs::read_dir(state_dir.join("lifecycle"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "json"))
        .expect("the companion wrote its state");
    let recorded: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
    assert_eq!(
        (recorded["registered"].as_bool(), recorded["error"].as_str()),
        (Some(true), Some("")),
        "{recorded}"
    );
    assert!(
        !std::fs::read_to_string(&state).unwrap().contains(&secret),
        "the key never reaches the state file"
    );
    let snapshot = client
        .get(format!(
            "http://127.0.0.1:{proxy_port}/v0/resource/plugins/cache-keepalive/snapshot"
        ))
        .header("authorization", format!("Bearer {secret}"))
        .send()
        .await
        .unwrap();
    assert_eq!(snapshot.status(), 200);
    let snapshot: serde_json::Value = snapshot.json().await.unwrap();
    let session = snapshot["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_id"] == "e2e-companion")
        .expect("registered under the key the client's variables name");
    assert_eq!(
        (
            session["instance_id"].as_str(),
            session["ended"].as_bool(),
            session["agents"][0]["agent_id"].as_str()
        ),
        (
            Some(recorded["instance"].as_str().unwrap()),
            Some(false),
            Some("main")
        )
    );
    let sessions: serde_json::Value = client
        .get(admin("/sessions"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let row = sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["session_id"] == "e2e-companion")
        .unwrap();
    assert_eq!(row["companion"].as_str(), Some("reporting"));
    let _ = stop_tx.send(());
    server.await.unwrap().unwrap();
    up.shutdown().await;
}

/// The admin listener carries the console as well as the API: its own routes answer JSON and everything else
/// is the app shell, so the browser reaches both over one origin and one session cookie.
#[tokio::test]
async fn the_admin_listener_serves_the_ui_beside_the_api() {
    // Nothing is relayed here; the fake exists so the configuration can name a reachable upstream and its CA.
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("ca.pem"), up.ca_pem()).unwrap();
    std::fs::write(
        dir.path().join("storage.key"),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    )
    .unwrap();
    let (proxy_port, admin_port, metrics_port) = (free_port(), free_port(), free_port());
    let yaml = format!(
        r#"
admin:
  listen: 127.0.0.1:{admin_port}
  secret_env: TEST_ADMIN_SECRET
  plain_http: true
compatibility:
  outbound_gzip: false
  profile: claude-code-2.1.269-linux-x64
  usage_entrypoint: cli
proxy:
  listen: 127.0.0.1:{proxy_port}
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
        std::env::set_var("TEST_ADMIN_SECRET", "admin-secret-001");
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

    let root = client
        .get(format!("http://127.0.0.1:{admin_port}/"))
        .send()
        .await
        .unwrap();
    assert_eq!(root.status(), 200);
    assert_eq!(root.headers()["cache-control"], "no-store");
    assert!(root.text().await.unwrap().contains("<div id=\"root\">"));
    let deep = client
        .get(format!("http://127.0.0.1:{admin_port}/upstreams/business"))
        .send()
        .await
        .unwrap();
    assert_eq!(deep.status(), 200);
    let api = client.get(admin("/session")).send().await.unwrap();
    assert_eq!(
        api.status(),
        200,
        "the session route answers JSON, not the shell"
    );
    assert_eq!(
        api.json::<serde_json::Value>().await.unwrap()["role"],
        "admin"
    );
    let unknown = client.get(admin("/nope")).send().await.unwrap();
    assert_eq!(
        unknown.status(),
        404,
        "an unknown admin path with a session is a JSON 404"
    );

    let _ = stop_tx.send(());
    server.await.unwrap().unwrap();
    up.shutdown().await;
}

/// The headless browser pass the sub-project ends with: every view in both themes at 1280 wide, one element
/// per view, no console error, screenshots kept outside the repository. Skipped unless PHOEBIS_E2E_BROWSER=1, so
/// a build host without Chromium still runs the rest of this suite.
#[tokio::test]
async fn the_ui_passes_the_browser_pass_over_every_view_in_both_themes() {
    if std::env::var("PHOEBIS_E2E_BROWSER").as_deref() != Ok("1") {
        println!("skipped: set PHOEBIS_E2E_BROWSER=1 to run the browser pass");
        return;
    }
    let up = FakeUpstream::start(vec![
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: warm_answer(),
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
    let (proxy_port, admin_port, metrics_port) = (free_port(), free_port(), free_port());
    let yaml = format!(
        "admin:\n  listen: 127.0.0.1:{admin_port}\n  secret_env: TEST_ADMIN_SECRET\n  viewer_secret_env: TEST_VIEWER_SECRET\n  plain_http: true\ncompatibility:\n  outbound_gzip: false\n  profile: claude-code-2.1.269-linux-x64\n  usage_entrypoint: cli\nkeepalive:\n  max_workers: 2\nproxy:\n  listen: 127.0.0.1:{proxy_port}\nrouting:\n  cooldown_fallback_seconds: 3600\nstats:\n  ledger_path: {ledger}\n  metrics_listen: 127.0.0.1:{metrics_port}\nstorage:\n  encryption_key_file: {key}\n  path: {db}\nupstream:\n  resolve_override: {addr}\n  extra_ca_pem: {ca}\nusage:\n  cache_ttl_seconds: 60\n  refresh_concurrency: 2\n",
        ledger = dir.path().join("ledger.sqlite").display(), key = dir.path().join("storage.key").display(), db = dir.path().join("phoebis.sqlite").display(), addr = up.addr(), ca = dir.path().join("ca.pem").display(),
    );
    let cfg_path = dir.path().join("config.yaml");
    std::fs::write(&cfg_path, yaml).unwrap();
    let config = {
        let _env = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        std::env::set_var("TEST_ADMIN_SECRET", "admin-secret-001");
        std::env::set_var("TEST_VIEWER_SECRET", "viewer-secret-01");
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
    // A lead of 299 s on a five-minute entry puts the first probe one second after the request, which is what
    // gives the pass a wasted probe to draw once the release below settles it.
    assert_eq!(client.post(admin("/pools")).json(&serde_json::json!({ "id": "main", "label": "Main", "strategy": "least_utilized", "affinity_ttl_seconds": 600, "members": [{ "account_id": "console", "priority": 0 }], "keepalive": { "enabled": true, "lead_seconds_5m": 299, "jitter_seconds_5m": 0, "probe_cost_cap_usd_per_day": 4 }, "body_capture_hours": 24 })).send().await.unwrap().status(), 201);
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
    // Written before the first request: a session rule posts its start as the request arrives, and the
    // lifecycle log the alerts view draws has nothing to list without one. The destination resolves to
    // nothing, as the pool rule's below does, so the attempts land as rows without reaching a network.
    assert_eq!(client.post(admin("/alerts/rules")).json(&serde_json::json!({ "kind": "session_active", "scope": { "type": "all" }, "webhook_url": "http://hook.invalid/life" })).send().await.unwrap().status(), 201);
    // The fixture body asks for fixed-budget thinking, which no probe may replay; an adaptive block is the
    // probe-capable shape.
    let adaptive = String::from_utf8(body(BodyFixture::ApiKey).to_vec())
        .unwrap()
        .replacen(
            r#""thinking":{"budget_tokens":31999,"type":"enabled","display":"omitted"}"#,
            r#""thinking":{"type":"adaptive"}"#,
            1,
        );
    let relayed = fixture_request(&client, proxy_port, &secret, "e2e-ui")
        .body(adaptive.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(relayed.status(), 200);
    let _ = relayed.text().await.unwrap();
    // Registered through the companion route under the key, so the recipe is retained.
    let events = format!("http://127.0.0.1:{proxy_port}/v0/resource/plugins/cache-keepalive/events?event=session_start&session_id=e2e-ui&instance_id=e2e-instance&sequence=1");
    assert_eq!(
        client
            .get(&events)
            .header("authorization", format!("Bearer {secret}"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let relayed = fixture_request(&client, proxy_port, &secret, "e2e-ui")
        .body(adaptive)
        .send()
        .await
        .unwrap();
    assert_eq!(relayed.status(), 200);
    let _ = relayed.text().await.unwrap();
    // Asserted before the probe is waited for: the body rewrite above depends on the fixture spelling its
    // thinking block exactly as written, and a fixture that moves on would otherwise show up here as a probe
    // that never arrives, which names the wrong culprit.
    let recipes = wait_json(&client, &admin("/keepalive/recipes"), |v| {
        !v["recipes"].as_array().unwrap().is_empty()
    })
    .await;
    assert_eq!(
        (
            recipes["recipes"][0]["session_id"].as_str(),
            recipes["recipes"][0]["state"].as_str()
        ),
        (Some("e2e-ui"), Some("armed"))
    );
    let probes = wait_json(
        &client,
        &admin("/requests?include_probes=true&kinds=probe"),
        |v| !v["requests"].as_array().unwrap().is_empty(),
    )
    .await;
    assert_eq!(probes["requests"][0]["status"].as_u64(), Some(200));
    // The operator's release settles the probe nobody came back for, which is the one wasted probe the
    // analytics view draws a reason row for.
    assert_eq!(
        client
            .post(admin("/sessions/desktop/e2e-ui/release"))
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    let _ = wait_json(
        &client,
        &admin("/analytics/breakdown?range=24h&group_by=all"),
        |v| {
            v["rows"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["savings"]["wasted_probes"].as_u64().unwrap_or(0) >= 1)
        },
    )
    .await;
    // The destination resolves to nothing (RFC 6761 reserves .invalid), so the delivery fails without reaching
    // a network; the event opens either way, which is what the alerts view shows.
    assert_eq!(client.post(admin("/alerts/rules")).json(&serde_json::json!({ "kind": "pool_exhausted", "scope": { "type": "all" }, "webhook_url": "http://hook.invalid/hook" })).send().await.unwrap().status(), 201);
    // The pool's only member answers a unified 429 once: the router cools it, finds no sibling, answers the
    // exhaustion 429, and the alert engine opens the pool_exhausted event the alerts view shows. The standing
    // answer under this credential stays the 200, so a keepalive probe after the 429 is still served.
    up.add_credential_rule("sk-ant-api03-PLACEHOLDER", "/v1/messages", warm_answer());
    up.push_credential_rule("sk-ant-api03-PLACEHOLDER", "/v1/messages", exhausted_429());
    let exhausted = fixture_request(&client, proxy_port, &secret, "e2e-exhausted")
        .send()
        .await
        .unwrap();
    assert_eq!(exhausted.status(), 429);
    assert_eq!(
        exhausted.headers()["x-phoebis-error-code"],
        "accounts_exhausted"
    );
    let _ = wait_json(&client, &admin("/alerts/events"), |v| {
        v["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "pool_exhausted" && e["state"] == "open")
    })
    .await;
    let _ = wait_json(&client, &admin("/requests"), |v| {
        !v["requests"].as_array().unwrap().is_empty()
    })
    .await;
    let _ = wait_json(&client, &admin("/alerts/deliveries"), |v| {
        !v["deliveries"].as_array().unwrap().is_empty()
    })
    .await;
    let shots = std::env::var("PHOEBIS_SCREENSHOT_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("phoebis-screenshots"));
    let web = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../web");
    // Awaited, never blocked on: the router and the fake upstream are tasks on this same current-thread
    // runtime, and a blocking wait here would starve the very server the browser is talking to.
    let status = tokio::process::Command::new("node")
        .arg("e2e/views.mjs")
        .current_dir(&web)
        .env("PHOEBIS_E2E_BASE", format!("http://127.0.0.1:{admin_port}"))
        .env("PHOEBIS_E2E_SECRET", "admin-secret-001")
        .env("PHOEBIS_SCREENSHOT_DIR", &shots)
        .status()
        .await
        .expect("node runs");
    assert!(
        status.success(),
        "the browser pass failed; its output names the view"
    );
    let pngs = walkdir(&shots)
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "png"))
        .count();
    assert_eq!(
        pngs,
        22,
        "eleven views in two themes under {}",
        shots.display()
    );
    println!("screenshots: {}", shots.display());
    let _ = stop_tx.send(());
    server.await.unwrap().unwrap();
    up.shutdown().await;
}

/// The relayed answer the pass seeds its cache figures from: a five-minute write of 2,000 tokens beside a read
/// of the same size, so the recipe arms and the keepalive views have a saving to show.
fn warm_answer() -> Respond {
    Respond::Json { status: 200, headers: vec![], body: br#"{"id":"msg_1","type":"message","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":10,"cache_read_input_tokens":2000,"cache_creation_input_tokens":2000,"cache_creation":{"ephemeral_5m_input_tokens":2000,"ephemeral_1h_input_tokens":0},"output_tokens":2}}"#.to_vec() }
}

/// The rate-limit answer the relay classifies as an account-wide cooldown, carrying the unified headers a
/// real rejection does. Their resets are absolute Unix seconds, so they are computed from now rather than
/// fixed; an API-key account takes its deadline from `routing.cooldown_fallback_seconds` instead, which the
/// configuration above sets long enough that the member is still cooled when the pass screenshots the pool.
fn exhausted_429() -> Respond {
    let reset = (jiff::Timestamp::now().as_second() + 18_000).to_string();
    Respond::Json {
        status: 429,
        headers: vec![
            (
                "anthropic-ratelimit-unified-status".into(),
                "rejected".into(),
            ),
            (
                "anthropic-ratelimit-unified-5h-status".into(),
                "rejected".into(),
            ),
            ("anthropic-ratelimit-unified-5h-reset".into(), reset.clone()),
            ("anthropic-ratelimit-unified-reset".into(), reset),
            (
                "anthropic-ratelimit-unified-representative-claim".into(),
                "five_hour".into(),
            ),
        ],
        body: br#"{"type":"error","error":{"type":"rate_limit_error","message":"exhausted"}}"#
            .to_vec(),
    }
}

/// The fixture-level equality check the live session procedure runs first: the four measured client runs relayed
/// through the binary, two under a paired oauth account and one under an api_key account, land at the fake
/// upstream in a shape `build-fixtures.py --check --through-router` grades equal to the committed fixtures.
#[tokio::test]
async fn the_four_client_runs_relayed_through_the_binary_grade_equal_to_the_fixtures() {
    use phoebis_profile::fixtures::{control_response, ControlFixture};
    let control = |which: ControlFixture| Respond::Json {
        status: 200,
        headers: vec![],
        body: control_response(which).to_vec(),
    };
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
        Rule {
            path_prefix: "/v1/oauth/token".into(),
            respond: control(ControlFixture::Token),
        },
        Rule {
            path_prefix: "/api/oauth/profile".into(),
            respond: control(ControlFixture::Profile),
        },
        Rule {
            path_prefix: "/api/oauth/claude_cli/roles".into(),
            respond: control(ControlFixture::Roles),
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
  plain_http: true
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
        std::env::set_var("TEST_ADMIN_SECRET", "admin-secret-001");
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

    assert_eq!(
        client
            .post(admin("/accounts"))
            .json(&serde_json::json!({ "id": "sub", "kind": "oauth", "label": "Sub" }))
            .send()
            .await
            .unwrap()
            .status(),
        201
    );
    let started: serde_json::Value = client
        .post(admin("/oauth/sessions"))
        .json(&serde_json::json!({ "account_id": "sub" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let (id, redirect) = (
        started["id"].as_str().unwrap().to_string(),
        started["redirect_uri"].as_str().unwrap().to_string(),
    );
    let state = started["authorization_url"]
        .as_str()
        .unwrap()
        .split("state=")
        .nth(1)
        .unwrap()
        .to_string();
    assert_eq!(
        client
            .post(admin(&format!("/oauth/sessions/{id}/callback")))
            .json(&serde_json::json!({ "url": format!("{redirect}?code=CODE123&state={state}") }))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        client
            .post(admin(&format!("/oauth/sessions/{id}/confirm")))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(client.post(admin("/accounts")).json(&serde_json::json!({ "id": "console", "kind": "api_key", "label": "Console", "secret": "sk-ant-api03-PLACEHOLDER" })).send().await.unwrap().status(), 201);
    for (pool, account) in [("subs", "sub"), ("keys", "console")] {
        assert_eq!(client.post(admin("/pools")).json(&serde_json::json!({ "id": pool, "label": pool, "strategy": "priority_random", "members": [{ "account_id": account, "priority": 0 }] })).send().await.unwrap().status(), 201);
    }
    // Cloned per call rather than captured by the async block directly: `client` is used again below, and a
    // closure invoked twice cannot move the same value out of its environment on each call.
    let key = |pool: &'static str| {
        let client = client.clone();
        async move {
            let v: serde_json::Value = client.post(admin("/keys")).json(&serde_json::json!({ "id": format!("k-{pool}"), "label": pool, "pool_id": pool })).send().await.unwrap().json().await.unwrap();
            v["secret"].as_str().unwrap().to_string()
        }
    };
    let (oauth_key, api_key) = (key("subs").await, key("keys").await);
    // The four runs of scripts/capture-baseline.sh, in its order, each replayed from its fixture head and body.
    let runs: [(FixtureKind, BodyFixture, &str); 4] = [
        (
            FixtureKind::OauthNonFirstParty,
            BodyFixture::Oauth,
            &oauth_key,
        ),
        (
            FixtureKind::OauthFirstParty,
            BodyFixture::FirstParty9b244,
            &oauth_key,
        ),
        (FixtureKind::ApiKey, BodyFixture::ApiKey, &api_key),
        (
            FixtureKind::OauthFirstParty,
            BodyFixture::FirstPartyE142e,
            &oauth_key,
        ),
    ];
    for (i, (head, fixture, secret)) in runs.into_iter().enumerate() {
        let status =
            replay_fixture(proxy_port, head, fixture, secret, &format!("capture-{i}")).await;
        assert_eq!(status, 200, "run {i}");
    }
    let posts: Vec<_> = up
        .requests()
        .into_iter()
        .filter(|r| r.line.starts_with("POST /v1/messages"))
        .collect();
    assert_eq!(posts.len(), 4);
    // Every stamped body the relay re-signed verifies under this build's rules; --skip-hash-check leaves it here.
    for r in &posts {
        if phoebis_cch::find_cch(&r.body).is_some() {
            assert_eq!(
                phoebis_cch::sign(&r.body).unwrap(),
                r.body,
                "the restamp is valid over the edited bytes"
            );
        }
    }
    // The fixture comparison below masks the credential, which proves nothing unless the relay actually
    // swapped it: check the bytes the fake upstream received directly instead. Run 2 (index 2) is the
    // api_key account; the other three are the paired oauth account, whose stored access token is this
    // fixture's canned token response, verbatim.
    for (i, r) in posts.iter().enumerate() {
        let carries_the_account_credential = if i == 2 {
            r.headers.iter().any(|(n, v)| {
                n.eq_ignore_ascii_case("x-api-key") && v == "sk-ant-api03-PLACEHOLDER"
            })
        } else {
            r.headers.iter().any(|(n, v)| {
                n.eq_ignore_ascii_case("authorization") && v == "Bearer sk-ant-oat01-PLACEHOLDER"
            })
        };
        assert!(
            carries_the_account_credential,
            "run {i} did not carry the account's own credential, not the caller's key: {:?}",
            r.headers
        );
    }
    let capture = dir.path().join("capture");
    std::fs::create_dir_all(&capture).unwrap();
    let modes: Vec<serde_json::Value> = posts
        .iter()
        .enumerate()
        .map(|(i, r)| {
            std::fs::write(capture.join(format!("clean_body_{i}.json")), &r.body).unwrap();
            serde_json::json!({ "t": 0, "request_line": r.line, "headers": r.headers.iter().map(|(n, v)| format!("{n}: {v}")).collect::<Vec<_>>(), "body": String::from_utf8_lossy(&r.body) })
        })
        .collect();
    std::fs::write(
        capture.join("clean_modes.json"),
        serde_json::to_vec(&modes).unwrap(),
    )
    .unwrap();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let out = std::process::Command::new("python3")
        .current_dir(&root)
        .args([
            "scripts/build-fixtures.py",
            "--check",
            "--through-router",
            "--skip-hash-check",
        ])
        .arg(&capture)
        .output()
        .unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "the through-router check did not pass:\n{text}"
    );
    assert!(text.contains("fixtures match"), "{text}");
    // The comparison has to be sensitive to the shape it claims to grade: re-casing one header name in the
    // capture must fail it, or a relay that folded every name to lowercase would pass this check too.
    let mutated = dir.path().join("capture-mutated");
    std::fs::create_dir_all(&mutated).unwrap();
    for (i, r) in posts.iter().enumerate() {
        std::fs::write(mutated.join(format!("clean_body_{i}.json")), &r.body).unwrap();
    }
    let mut mutated_modes = modes.clone();
    let lines = mutated_modes[0]["headers"].as_array_mut().unwrap();
    let at = lines
        .iter()
        .position(|l| l.as_str().is_some_and(|l| l.starts_with("X-Stainless")))
        .expect("the measured client's head carries an X-Stainless header");
    let (name, value) = lines[at].as_str().unwrap().split_once(": ").unwrap();
    lines[at] = serde_json::json!(format!("{}: {value}", name.to_ascii_lowercase()));
    std::fs::write(
        mutated.join("clean_modes.json"),
        serde_json::to_vec(&mutated_modes).unwrap(),
    )
    .unwrap();
    let out = std::process::Command::new("python3")
        .current_dir(&root)
        .args([
            "scripts/build-fixtures.py",
            "--check",
            "--through-router",
            "--skip-hash-check",
        ])
        .arg(&mutated)
        .output()
        .unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "one header name re-cased still graded equal, so the check is not reading the wire shape"
    );
    assert!(
        text.contains("inference_oauth.http") || text.contains("fixtures differ:"),
        "the failure did not name the head it disagreed on:\n{text}"
    );
    let _ = stop_tx.send(());
    server.await.unwrap().unwrap();
    up.shutdown().await;
}

fn walkdir(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walkdir(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}
