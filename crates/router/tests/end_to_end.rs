//! The binary from the outside: one fixture request relayed through a configured API-key account, and the
//! bootstrap rules that refuse a configuration nobody should be able to start.

use fake_upstream::{FakeUpstream, Respond, Rule};
use phoebis_profile::fixtures::{
    body, control_response, inference_head, with_dangerous_tool_use_beta, with_safeguards,
    BodyFixture, ControlFixture, FixtureKind, SAFEGUARDS_MEMBER,
};
use std::net::TcpListener;
use std::sync::Mutex;

/// `load` reads the admin secret from the process environment, and the tests in this binary set the same
/// variables to different values, so each holds this across its whole set-then-load sequence.
static ENV: Mutex<()> = Mutex::new(());

/// The binary reads the keepalive's debug timing once, at its start, so a test that waits for a probe a second
/// after its request keeps the two variables set from before `app::run` until its admin answers. The two such
/// tests may overlap, so the variables are unset only once the last of them has started.
static PROBE_SOON: Mutex<usize> = Mutex::new(0);

/// Puts the first probe of a five-minute entry one second after its request, for the binary started next.
fn probe_soon_on() {
    let mut starting = PROBE_SOON
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if *starting == 0 {
        let _env = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        std::env::set_var("PHOEBIS_DEBUG_KEEPALIVE_LEAD_5M_SECONDS", "299");
        std::env::set_var("PHOEBIS_DEBUG_KEEPALIVE_JITTER_5M_SECONDS", "0");
    }
    *starting += 1;
}

/// Called once the binary answers its admin login, which it does only after its start read the timing.
fn probe_soon_off() {
    let mut starting = PROBE_SOON
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *starting -= 1;
    if *starting == 0 {
        let _env = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        std::env::remove_var("PHOEBIS_DEBUG_KEEPALIVE_LEAD_5M_SECONDS");
        std::env::remove_var("PHOEBIS_DEBUG_KEEPALIVE_JITTER_5M_SECONDS");
    }
}

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
/// process, one advance per draw and one per port a fake upstream holds.
///
/// Two processes whose pids are congruent modulo 100 share a slice, and a draw inside the probe-to-bind
/// window can collide; the default cargo runner never runs two of these binaries at once, so the window is
/// left open. A parallel runner would need a per-slice lock file under `target/` held for the process's
/// lifetime, taken before the first draw.
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
            "end_to_end.rs advanced its port cursor past {PORT_SPAN} in one process; a test is drawing ports it never binds, another process's listeners are filling the slice, or the slice must grow"
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
    head: &[u8],
    body: &[u8],
    secret: &str,
    session: &str,
) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (request_line, headers) = phoebis_transport::request::parse_head(head).unwrap();
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
    socket.write_all(body).await.unwrap();
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
            .json(&serde_json::json!({ "id": "main", "label": "Main", "strategy": "priority", "members": [{ "account_id": "console", "priority": 0 }] }))
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

    // Registry edits show in the next selection: no member, no candidate, nothing an operator has not caused;
    // a revoked key, a 401.
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
        (503, "accounts_unavailable")
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
    let ready = client
        .head(format!("http://127.0.0.1:{proxy_port}/api/ready"))
        .send()
        .await
        .unwrap();
    assert_eq!(ready.status(), 200);
    let ready = client
        .get(format!("http://127.0.0.1:{proxy_port}/api/ready"))
        .send()
        .await
        .unwrap();
    assert_eq!(ready.status(), 200);
    assert_eq!(
        ready.json::<serde_json::Value>().await.unwrap(),
        serde_json::json!({ "ready": true }),
        "the binary's router task, ledger consumer and store are alive"
    );
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

/// The formatter's output kept in memory, so a case reads back the lines the binary logged on its thread.
#[derive(Clone, Default)]
struct Captured(std::sync::Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Captured {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

#[tokio::test]
async fn every_new_upstream_connection_logs_its_host_and_its_dial_and_handshake_durations_at_debug()
{
    // This thread's subscriber, set before the binary starts: `app::run` installs a global one at INFO, and a
    // `#[tokio::test]` polls the binary's tasks on this thread, where the thread's own default outranks it.
    let captured = Captured::default();
    let _capture = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(captured.clone())
            .finish(),
    );
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
    let (proxy_port, admin_port, metrics_port) = (free_port(), free_port(), free_port());
    let yaml = format!(
        "admin:\n  listen: 127.0.0.1:{admin_port}\n  secret_env: TEST_ADMIN_SECRET\nproxy:\n  listen: 127.0.0.1:{proxy_port}\nstats:\n  ledger_path: {ledger}\n  metrics_listen: 127.0.0.1:{metrics_port}\nstorage:\n  encryption_key_file: {key}\n  path: {db}\nupstream:\n  resolve_override: {addr}\n  extra_ca_pem: {ca}\n",
        ledger = dir.path().join("ledger.sqlite").display(),
        key = dir.path().join("storage.key").display(),
        db = dir.path().join("phoebis.sqlite").display(),
        addr = up.addr(),
        ca = dir.path().join("ca.pem").display(),
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
    let account_secret = "sk-ant-api03-CONNECTIONLINE";
    assert_eq!(client.post(admin("/accounts")).json(&serde_json::json!({ "id": "console", "kind": "api_key", "label": "Console", "secret": account_secret })).send().await.unwrap().status(), 201);
    assert_eq!(client.post(admin("/pools")).json(&serde_json::json!({ "id": "main", "label": "Main", "strategy": "priority", "members": [{ "account_id": "console", "priority": 0 }] })).send().await.unwrap().status(), 201);
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
    let resp = fixture_request(&client, proxy_port, &secret, "e2e-connection")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "{\"ok\":true}");
    let _ = stop_tx.send(());
    server.await.unwrap().unwrap();
    up.shutdown().await;

    let text = captured.text();
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("a new upstream connection"))
        .collect();
    assert!(
        lines.iter().any(|line| line.contains("DEBUG")
            && line.contains(&format!("host=\"{}\"", phoebis_store::INFERENCE_HOST))
            && line.contains("dial_ms=")
            && line.contains("handshake_ms=")),
        "a connection line for the inference host with both durations: {lines:#?}"
    );
    assert!(
        lines
            .iter()
            .all(|line| !line.contains(account_secret) && !line.contains(&secret)),
        "a connection line names no credential: {lines:#?}"
    );
}

/// The binary reads its shutdown budget once, at its start, so the case sets the figure before `app::run` and
/// removes it once the admin answers, as `probe_soon_on` and `probe_soon_off` do for the keepalive's timing.
const SHUTDOWN_BUDGET_SECONDS: &str = "PHOEBIS_DEBUG_SHUTDOWN_BUDGET_SECONDS";

/// The figure is removed when this drops, a panic's unwinding included, so a case that fails before its admin answers
/// leaves no short budget to a binary another case starts later.
struct ShortShutdownBudget;

impl ShortShutdownBudget {
    fn set(seconds: &str) -> ShortShutdownBudget {
        let _env = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        std::env::set_var(SHUTDOWN_BUDGET_SECONDS, seconds);
        ShortShutdownBudget
    }
}

impl Drop for ShortShutdownBudget {
    fn drop(&mut self) {
        let _env = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        std::env::remove_var(SHUTDOWN_BUDGET_SECONDS);
    }
}

#[tokio::test]
async fn a_shutdown_with_a_stalled_body_ends_inside_its_budget_with_both_flushes_run() {
    // This thread's subscriber, as in the connection line's case: the binary's own lines are logged on this thread.
    let captured = Captured::default();
    let _capture = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(captured.clone())
            .finish(),
    );
    // The head and `message_start` commit the stream, so the relay hands the rest to a body task, and the gate
    // holds that task on its upstream read until the release below, past the whole stop.
    let up = FakeUpstream::start(vec![
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: Respond::SseGated {
                status: 200,
                headers: vec![],
                events: vec![
                    fake_upstream::SseEvent {
                        event: "message_start".into(),
                        data: r#"{"type":"message_start","message":{"id":"msg_stalled","type":"message","role":"assistant","model":"claude-sonnet-4-5","content":[],"usage":{"input_tokens":5,"output_tokens":1}}}"#.into(),
                    },
                    fake_upstream::SseEvent {
                        event: "message_stop".into(),
                        data: r#"{"type":"message_stop"}"#.into(),
                    },
                ],
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
    let (proxy_port, admin_port, metrics_port) = (free_port(), free_port(), free_port());
    let yaml = format!(
        "admin:\n  listen: 127.0.0.1:{admin_port}\n  secret_env: TEST_ADMIN_SECRET\nproxy:\n  listen: 127.0.0.1:{proxy_port}\nstats:\n  ledger_path: {ledger}\n  metrics_listen: 127.0.0.1:{metrics_port}\nstorage:\n  encryption_key_file: {key}\n  path: {db}\nupstream:\n  resolve_override: {addr}\n  extra_ca_pem: {ca}\n",
        ledger = dir.path().join("ledger.sqlite").display(),
        key = dir.path().join("storage.key").display(),
        db = dir.path().join("phoebis.sqlite").display(),
        addr = up.addr(),
        ca = dir.path().join("ca.pem").display(),
    );
    let cfg_path = dir.path().join("config.yaml");
    std::fs::write(&cfg_path, yaml).unwrap();
    // Set outside the block below: its `ENV` guard drops first on a panic, so the figure's own drop can take the lock.
    let short_budget = ShortShutdownBudget::set("3");
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
    drop(short_budget);
    assert_eq!(client.post(admin("/accounts")).json(&serde_json::json!({ "id": "console", "kind": "api_key", "label": "Console", "secret": "sk-ant-api03-STALLEDBODY" })).send().await.unwrap().status(), 201);
    assert_eq!(client.post(admin("/pools")).json(&serde_json::json!({ "id": "main", "label": "Main", "strategy": "priority", "members": [{ "account_id": "console", "priority": 0 }] })).send().await.unwrap().status(), 201);
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
    let held = fixture_request(&client, proxy_port, &secret, "e2e-stalled-body")
        .send()
        .await
        .unwrap();
    assert_eq!(held.status(), 200, "the stream committed before the stop");

    let _ = stop_tx.send(());
    // Well past the three-second budget and the flushes after it, and well short of the 30 s the listeners' drain
    // alone would wait for the stalled stream.
    let ended = tokio::time::timeout(std::time::Duration::from_secs(15), server).await;
    let text = captured.text();
    assert!(
        ended.is_ok(),
        "the stop with a stalled body ran past 15 s: {text}"
    );
    ended.unwrap().unwrap().unwrap();
    drop(held);
    up.release();
    up.shutdown().await;

    for phase in ["listeners", "bodies"] {
        assert!(
            text.lines()
                .any(|line| line.contains("WARN") && line.contains(&format!("phase=\"{phase}\""))),
            "the budget cut the {phase} phase: {text}"
        );
    }
    // One deadline leaves the bodies nothing once the listeners spent it, where a budget of their own would give them
    // three seconds.
    assert!(
        text.lines()
            .any(|line| line.contains("phase=\"bodies\"") && line.contains("seconds=0")),
        "the bodies got what the listeners left: {text}"
    );
    for flushed in [
        "the keepalive's last flush ran",
        "the session memory's last flush ran",
    ] {
        assert!(
            text.lines()
                .any(|line| line.contains("INFO") && line.contains(flushed)),
            "{flushed}: {text}"
        );
    }
}

#[tokio::test]
async fn a_key_set_off_through_the_admin_api_strips_the_next_request_and_set_back_on_passes_it_through(
) {
    let up = FakeUpstream::start(vec![
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: Respond::Json {
                status: 200,
                headers: vec![],
                body: br#"{"id":"msg_1","type":"message","usage":{"input_tokens":12,"output_tokens":3}}"#
                    .to_vec(),
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
    let (proxy_port, admin_port, metrics_port) = (free_port(), free_port(), free_port());
    let yaml = format!(
        "admin:\n  listen: 127.0.0.1:{admin_port}\n  secret_env: TEST_ADMIN_SECRET\nproxy:\n  listen: 127.0.0.1:{proxy_port}\nstats:\n  ledger_path: {ledger}\n  metrics_listen: 127.0.0.1:{metrics_port}\nstorage:\n  encryption_key_file: {key}\n  path: {db}\nupstream:\n  resolve_override: {addr}\n  extra_ca_pem: {ca}\nusage:\n  refresh_concurrency: 2\n",
        ledger = dir.path().join("ledger.sqlite").display(),
        key = dir.path().join("storage.key").display(),
        db = dir.path().join("phoebis.sqlite").display(),
        addr = up.addr(),
        ca = dir.path().join("ca.pem").display(),
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
    let pool = client
        .post(admin("/pools"))
        .json(&serde_json::json!({ "id": "main", "label": "Main", "strategy": "priority", "members": [{ "account_id": "console", "priority": 0 }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(pool.status(), 201);
    let pool: serde_json::Value = pool.json().await.unwrap();
    assert_eq!(pool["auto_mode_server"], true, "on by default");
    let key: serde_json::Value = client
        .post(admin("/keys"))
        .json(&serde_json::json!({ "id": "desktop", "label": "Desktop", "pool_id": "main" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        (&key["auto_mode_server"], &key["effective_auto_mode_server"]),
        (&serde_json::Value::Null, &serde_json::json!(true)),
        "a new key inherits the pool's setting"
    );
    let secret = key["secret"].as_str().unwrap().to_string();
    let set = |value: serde_json::Value| {
        let client = client.clone();
        async move {
            let answer = client
                .patch(admin("/keys/desktop"))
                .json(&serde_json::json!({ "auto_mode_server": value }))
                .send()
                .await
                .unwrap();
            assert_eq!(answer.status(), 200);
            let key: serde_json::Value = answer.json().await.unwrap();
            (
                key["auto_mode_server"].clone(),
                key["effective_auto_mode_server"].clone(),
            )
        }
    };

    // First-party, so the off state's restamp has a client stamp to replace.
    let head = with_dangerous_tool_use_beta(inference_head(FixtureKind::OauthFirstParty));
    let sent = phoebis_cch::sign(&with_safeguards(body(BodyFixture::FirstParty9b244))).unwrap();
    let member = SAFEGUARDS_MEMBER.as_bytes();
    let posts = || -> Vec<fake_upstream::RecordedRequest> {
        up.requests()
            .into_iter()
            .filter(|r| r.line.starts_with("POST /v1/messages"))
            .collect()
    };
    let asks = |r: &fake_upstream::RecordedRequest| {
        let betas: Vec<String> = r
            .headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case("anthropic-beta"))
            .flat_map(|(_, v)| v.split(',').map(|b| b.trim().to_string()))
            .collect();
        (
            r.body.windows(member.len()).any(|w| w == member),
            betas
                .iter()
                .any(|b| b == phoebis_relay::DANGEROUS_TOOL_USE_BETA),
        )
    };

    assert_eq!(
        replay_fixture(proxy_port, &head, &sent, &secret, "auto-1").await,
        200
    );
    assert_eq!(
        asks(&posts()[0]),
        (true, true),
        "on by inheritance, the member and the beta reach the upstream"
    );

    assert_eq!(
        set(serde_json::json!(false)).await,
        (serde_json::json!(false), serde_json::json!(false))
    );
    assert_eq!(
        replay_fixture(proxy_port, &head, &sent, &secret, "auto-2").await,
        200
    );
    let stripped = &posts()[1];
    assert_eq!(
        asks(stripped),
        (false, false),
        "off on the key, the next request loses both"
    );
    assert!(phoebis_cch::find_cch(&stripped.body).is_some());
    assert_eq!(
        phoebis_cch::sign(&stripped.body).unwrap(),
        stripped.body,
        "the body was restamped over the cut"
    );

    assert_eq!(
        set(serde_json::Value::Null).await,
        (serde_json::Value::Null, serde_json::json!(true))
    );
    assert_eq!(
        replay_fixture(proxy_port, &head, &sent, &secret, "auto-3").await,
        200
    );
    assert_eq!(
        asks(&posts()[2]),
        (true, true),
        "back to inherit, both travel again"
    );

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
    let mut standing: Vec<(String, String)> = log["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["session_id"].as_str().unwrap().to_string(),
                row["auto_mode"].as_str().unwrap_or("null").to_string(),
            )
        })
        .collect();
    standing.sort();
    assert_eq!(
        standing,
        [
            ("auto-1", "server"),
            ("auto-2", "stripped"),
            ("auto-3", "server")
        ]
        .map(|(session, auto_mode)| (session.to_string(), auto_mode.to_string()))
    );

    let _ = stop_tx.send(());
    server.await.unwrap().unwrap();
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
proxy:
  listen: 127.0.0.1:{proxy_port}
stats:
  ledger_path: {ledger}
  metrics_listen: 127.0.0.1:{metrics_port}
storage:
  encryption_key_file: {key}
  path: {db}
usage:
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
proxy:
  listen: 127.0.0.1:{proxy_port}
routing:
  cooldown_fallback_seconds: 3600
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

    // Distinct priorities give the first selection a deterministic winner: `pace` only orders
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
            .json(&serde_json::json!({ "id": "main", "label": "Main", "strategy": "pace", "members": [{ "account_id": "a", "priority": 0 }, { "account_id": "b", "priority": 1 }] }))
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

/// The API-key fixture body on `claude-opus-5-5`, whose price table row carries fast cells, with `"speed":"fast"`
/// spliced at the top level when `fast`.
fn opus_5_5_body(fast: bool) -> Vec<u8> {
    let speed = if fast { r#""speed":"fast","# } else { "" };
    let spliced = String::from_utf8(body(BodyFixture::ApiKey).to_vec())
        .unwrap()
        .replacen(
            r#"{"model":"claude-sonnet-4-5""#,
            &format!(r#"{{{speed}"model":"claude-opus-5-5""#),
            1,
        );
    assert!(spliced.contains(r#""model":"claude-opus-5-5""#));
    spliced.into_bytes()
}

/// A 200 served fast under the upstream id `request_id`: the six `anthropic-fast-*` headers, both buckets with
/// room and resetting a minute out, and 1,000 input and 100 output tokens at speed `fast`.
fn served_fast(request_id: &str) -> Respond {
    let reset = (jiff::Timestamp::now().as_second() + 60).to_string();
    let mut headers = vec![("request-id".to_string(), request_id.to_string())];
    for (family, limit) in [("input", "400000"), ("output", "80000")] {
        let name = |part: &str| format!("anthropic-fast-{family}-tokens-{part}");
        headers.push((name("limit"), limit.into()));
        headers.push((name("remaining"), "200000".into()));
        headers.push((name("reset"), reset.clone()));
    }
    Respond::Json {
        status: 200,
        headers,
        body: br#"{"id":"msg_fast","type":"message","model":"claude-opus-5-5","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":1000,"output_tokens":100,"speed":"fast"}}"#.to_vec(),
    }
}

/// A fast request's 429 for the fast lane's own limit with no overage-disabled reason, timed by `Retry-After`
/// alone, past the pool's retry-in-place bound.
fn fast_rate_limited() -> Respond {
    Respond::Json {
        status: 429,
        headers: vec![("retry-after".into(), "120".into())],
        body: br#"{"type":"error","error":{"type":"rate_limit_error","message":"fast limit"}}"#
            .to_vec(),
    }
}

#[tokio::test]
async fn a_fast_session_is_served_fast_walked_on_a_fast_429_and_refused_locally_when_no_account_can(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/models".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"data":[{"id":"claude-opus-5-5","type":"model"},{"id":"claude-sonnet-4-5","type":"model"}]}"#.to_vec(),
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
        "admin:\n  listen: 127.0.0.1:{admin_port}\n  secret_env: TEST_FAST_ADMIN_SECRET\nproxy:\n  listen: 127.0.0.1:{proxy_port}\nrouting:\n  cooldown_fallback_seconds: 3600\nstats:\n  ledger_path: {ledger}\n  metrics_listen: 127.0.0.1:{metrics_port}\nstorage:\n  encryption_key_file: {key}\n  path: {db}\nupstream:\n  resolve_override: {addr}\n  extra_ca_pem: {ca}\nusage:\n  refresh_concurrency: 2\n",
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
        std::env::set_var("TEST_FAST_ADMIN_SECRET", "admin-secret-001");
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

    // Each account answers a standard request by default; every fast answer below is queued for its request.
    const A: &str = "sk-ant-api03-AAAA";
    const B: &str = "sk-ant-api03-BBBB";
    for (id, secret) in [("a", A), ("b", B)] {
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
                headers: vec![("request-id".into(), format!("req_standard_{id}"))],
                body: br#"{"id":"msg_std","type":"message","model":"claude-opus-5-5","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":10,"output_tokens":1}}"#.to_vec(),
            },
        );
    }
    // `pace` orders candidates within a tier alone, so the priorities make `a` the fresh placement.
    assert_eq!(
        client
            .post(admin("/pools"))
            .json(&serde_json::json!({ "id": "main", "label": "Main", "strategy": "pace", "members": [{ "account_id": "a", "priority": 0 }, { "account_id": "b", "priority": 1 }] }))
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
    let send = |fast: bool| {
        fixture_request(&client, proxy_port, &secret, "fast-e2e")
            .body(opus_5_5_body(fast))
            .send()
    };
    // The ledger's row for the attempt the upstream answered under `upstream_id`, once it has landed.
    let attempt_of = |upstream_id: &'static str| {
        let client = client.clone();
        let url = admin("/requests?limit=50");
        async move {
            let log = wait_json(&client, &url, |v| {
                v["requests"].as_array().is_some_and(|rows| {
                    rows.iter().any(|row| {
                        row["attempts"].as_array().is_some_and(|attempts| {
                            attempts
                                .iter()
                                .any(|a| a["upstream_request_id"] == upstream_id)
                        })
                    })
                })
            })
            .await;
            log["requests"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| {
                    row["attempts"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|a| a["upstream_request_id"] == upstream_id)
                })
                .cloned()
                .unwrap()
        }
    };
    let dialed = || {
        up.requests()
            .iter()
            .filter(|r| r.line.starts_with("POST /v1/messages"))
            .count()
    };

    // Served fast: the first fast request lands on `a` and is priced at the fast cells.
    up.push_credential_rule(A, "/v1/messages", served_fast("req_fast_a1"));
    assert_eq!(send(true).await.unwrap().status(), 200);
    let row = attempt_of("req_fast_a1").await;
    let attempt = &row["attempts"][0];
    assert_eq!(
        (
            attempt["account_id"].as_str(),
            attempt["response"]["speed"].as_str()
        ),
        (Some("a"), Some("fast"))
    );
    // 1,000 input tokens at 8 and 100 output tokens at 40 per million, the fast cells of `claude-opus-5-5`.
    let usd = attempt["estimated_usd"].as_f64().unwrap();
    assert!((usd - 0.012).abs() < 1e-9, "{usd}");
    wait_json(&client, &admin("/accounts"), |v| {
        v["accounts"].as_array().is_some_and(|rows| {
            rows.iter()
                .any(|r| r["id"] == "a" && r["fast_lane"].is_object())
        })
    })
    .await;

    // Walked: `a`'s fast 429 cools its fast lane alone and the request moves to `b` under `fast_lane`.
    up.push_credential_rule(A, "/v1/messages", fast_rate_limited());
    up.push_credential_rule(B, "/v1/messages", served_fast("req_fast_b1"));
    assert_eq!(send(true).await.unwrap().status(), 200);
    let row = attempt_of("req_fast_b1").await;
    assert_eq!(
        row["attempts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| (a["account_id"].as_str(), a["upstream_status"].as_u64()))
            .collect::<Vec<_>>(),
        [(Some("a"), Some(429)), (Some("b"), Some(200))]
    );
    wait_json(
        &client,
        &admin(&format!("/requests/{}", row["id"].as_str().unwrap())),
        |v| {
            v["events"].as_array().is_some_and(|events| {
                events
                    .iter()
                    .any(|e| e["kind"] == "pin_migrated" && e["detail"]["reason"] == "fast_lane")
            })
        },
    )
    .await;
    let a: serde_json::Value = client
        .get(admin("/accounts/a"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        a["cooldowns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| (c["scope"].as_str(), c["reason"].as_str()))
            .collect::<Vec<_>>(),
        [(Some("fast"), Some("fast_rate_limit"))]
    );

    // Refused locally: `b`'s own fast 429 leaves no fast-eligible account, so the request that met it is
    // refused, and the next fast request is refused before any dial.
    up.push_credential_rule(B, "/v1/messages", fast_rate_limited());
    let before = dialed();
    let met = send(true).await.unwrap();
    assert_eq!(dialed(), before + 1, "the request dialled `b` alone");
    assert_eq!(
        (
            met.status().as_u16(),
            met.headers()["x-phoebis-error-code"].to_str().unwrap()
        ),
        (429, "fast_lane_exhausted")
    );
    let before = dialed();
    let refused = send(true).await.unwrap();
    assert_eq!(
        (
            refused.status().as_u16(),
            refused.headers()["x-phoebis-error-code"].to_str().unwrap()
        ),
        (429, "fast_lane_exhausted")
    );
    let retry_after: u64 = refused.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    // The earlier fast deadline, `a`'s 120 seconds plus the second of margin a `Retry-After` row keeps.
    assert!((1..=121).contains(&retry_after), "{retry_after}");
    assert_eq!(dialed(), before, "the refusal dialled no upstream");

    // A standard request of the same session holds no pin and places afresh on `a`, whose fast-lane row holds
    // fast requests alone.
    assert_eq!(send(false).await.unwrap().status(), 200);
    let row = attempt_of("req_standard_a").await;
    assert_eq!(row["attempts"][0]["account_id"], "a");

    // Clearing `a`'s fast lane lets the next fast request land there.
    assert_eq!(
        client
            .delete(admin("/accounts/a/cooldowns/fast"))
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    up.push_credential_rule(A, "/v1/messages", served_fast("req_fast_a2"));
    assert_eq!(send(true).await.unwrap().status(), 200);
    let row = attempt_of("req_fast_a2").await;
    assert_eq!(row["attempts"][0]["account_id"], "a");

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
            .json(&serde_json::json!({ "id": "main", "label": "Main", "strategy": "priority", "members": [{ "account_id": "a", "priority": 0 }, { "account_id": "b", "priority": 1 }] }))
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
proxy:
  listen: 127.0.0.1:0
storage:
  encryption_key_file: {key}
  path: {db}
usage:
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
fn a_listener_without_tls_loads_on_any_address_and_a_rootless_storage_path_does_not() {
    use phoebis_router::config::{load, ConfigError};
    let dir = tempfile::tempdir().unwrap();
    let _env = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    std::env::set_var("TEST_ADMIN_SECRET", "admin-secret-001");
    let loopback_plain = "  listen: 127.0.0.1:8081\n  secret_env: TEST_ADMIN_SECRET";
    let public_bare = "  listen: 0.0.0.0:8081\n  secret_env: TEST_ADMIN_SECRET";
    for (name, admin) in [("ok", loopback_plain), ("public-bare", public_bare)] {
        let config = load(&config_with(dir.path(), name, admin)).unwrap();
        assert!(config.admin.tls.is_none(), "{name}: {:?}", config.admin);
    }

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
fn stats_and_cooldown_settings_are_validated_with_the_specs_bounds() {
    use phoebis_router::config::{load, ConfigError};
    let dir = tempfile::tempdir().unwrap();
    let _env = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    std::env::set_var("TEST_ADMIN_SECRET", "admin-secret-001");
    let base = "  listen: 127.0.0.1:8081\n  secret_env: TEST_ADMIN_SECRET";
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
    let path =
        |p: &Option<std::path::PathBuf>| p.as_deref().and_then(|p| p.to_str()).map(str::to_string);
    assert_eq!(
        (
            path(&config.stats.api_prices_path),
            path(&config.stats.subscription_costs_path)
        ),
        (
            Some("./api_prices.yaml".to_string()),
            Some("./subscription_costs.yaml".to_string())
        )
    );
    let full = load(&with("full", "stats:\n  api_prices_path: ./p.yaml\n  body_capture_max_hours: 24\n  ledger_path: ./data/l.sqlite\n  metrics_listen: 127.0.0.1:9999\n  retention_days: 7\n  rollup_retention_days: 500\n  subscription_costs_path: ./c.yaml\n")).unwrap();
    assert_eq!(
        (full.stats.retention_days, full.stats.rollup_retention_days),
        (7, 500)
    );
    assert_eq!(
        (
            path(&full.stats.api_prices_path),
            path(&full.stats.subscription_costs_path)
        ),
        (Some("./p.yaml".to_string()), Some("./c.yaml".to_string()))
    );
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
    assert_eq!(
        load(&with(
            "cooldown",
            "routing:\n  cooldown_fallback_seconds: 3601\n"
        ))
        .unwrap_err()
        .to_string(),
        "routing.cooldown_fallback_seconds must be between 1 and 3600, not 3601"
    );
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

const REMOVED_LEASE: &str = "keepalive:\n  lease_seconds: 90\n"; // removed name
const REMOVED_WORKERS: &str = "keepalive:\n  max_workers: 4\n"; // removed name
const REMOVED_REQUEST_BYTES: &str = "keepalive:\n  max_request_bytes: 8388608\n"; // removed name
const REMOVED_PREWARM: &str = "keepalive:\n  oauth_prewarm_verified: false\n"; // removed name

#[test]
fn keepalive_limits_are_validated_with_the_go_engines_bounds() {
    use phoebis_router::config::{load, ConfigError};
    let dir = tempfile::tempdir().unwrap();
    let _env = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    std::env::set_var("TEST_ADMIN_SECRET", "admin-secret-001");
    let base = "  listen: 127.0.0.1:8081\n  secret_env: TEST_ADMIN_SECRET";
    let text = std::fs::read_to_string(config_with(dir.path(), "ok", base)).unwrap();
    let with = |name: &str, extra: &str| {
        let path = dir.path().join(format!("{name}.yaml"));
        std::fs::write(&path, format!("{text}{extra}")).unwrap();
        path
    };
    let defaults = load(&with("defaults", "")).unwrap();
    assert_eq!(
        (
            defaults.keepalive.max_agents,
            defaults.keepalive.max_retained_bytes,
            defaults.keepalive.max_sessions
        ),
        (1_024, 256 << 20, 128),
        "an absent block is the spec's defaults"
    );
    let full = load(&with(
        "full",
        "keepalive:\n  max_agents: 2048\n  max_retained_bytes: 33554432\n  max_sessions: 256\n",
    ))
    .unwrap();
    assert_eq!(
        (
            full.keepalive.max_agents,
            full.keepalive.max_retained_bytes,
            full.keepalive.max_sessions
        ),
        (2_048, 32 << 20, 256)
    );
    for (name, body) in [
        ("lease", REMOVED_LEASE),
        ("workers", REMOVED_WORKERS),
        ("request-bytes", REMOVED_REQUEST_BYTES),
        ("prewarm", REMOVED_PREWARM),
    ] {
        let error = load(&with(name, body)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "the configuration is not valid YAML",
            "{name}"
        );
        assert!(
            matches!(&error, ConfigError::Syntax(cause) if cause.to_string().contains("unknown field")),
            "{name}: {error:?}"
        );
    }
    assert_eq!(
        load(&with(
            "retained",
            "keepalive:\n  max_retained_bytes: 33554431\n"
        ))
        .unwrap_err()
        .to_string(),
        "keepalive.max_retained_bytes must be between 33554432 and 536870912, not 33554431",
        "the retained budget holds at least one recipe at the relay's request bound"
    );
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
    std::fs::write(dir.path().join("api_prices.yaml"), "effective: 2026-10-01\nmodels:\n  claude-sonnet-4-5: {input: 3, cache_write_5m: 3.75, cache_write_1h: 6, cache_read: 0.3, output: 15}\n").unwrap();
    std::fs::write(
        dir.path().join("subscription_costs.yaml"),
        phoebis_ledger::test_tables::SUBSCRIPTION_COSTS,
    )
    .unwrap();
    let (proxy_port, admin_port, metrics_port) = (free_port(), free_port(), free_port());
    let yaml = format!(
        "admin:\n  listen: 127.0.0.1:{admin_port}\n  secret_env: TEST_ADMIN_SECRET\nproxy:\n  listen: 127.0.0.1:{proxy_port}\nstats:\n  api_prices_path: {prices}\n  ledger_path: {ledger}\n  metrics_listen: 127.0.0.1:{metrics_port}\n  subscription_costs_path: {costs}\nstorage:\n  encryption_key_file: {key}\n  path: {db}\nupstream:\n  resolve_override: {addr}\n  extra_ca_pem: {ca}\nusage:\n  refresh_concurrency: 2\n",
        ledger = dir.path().join("ledger.sqlite").display(), prices = dir.path().join("api_prices.yaml").display(),
        costs = dir.path().join("subscription_costs.yaml").display(),
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
    assert_eq!(client.post(admin("/pools")).json(&serde_json::json!({ "id": "main", "label": "Main", "strategy": "priority", "body_capture_hours": 1, "members": [{ "account_id": "console", "priority": 0 }] })).send().await.unwrap().status(), 201);
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
            system["ledger"]["written"].as_u64().map(|n| n >= 3),
            system["ledger"]["queued"].as_u64().is_some(),
            system["ledger"]["behind"].as_bool(),
            system["ledger"]["behind_refusals"].as_u64()
        ),
        (Some(0), Some(true), true, Some(false), Some(0))
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
        settings["api_prices"]["effective"], "2026-10-01",
        "the override file's effective date"
    );
    assert_eq!(
        (
            &settings["subscription_costs"]["source"],
            &settings["subscription_costs"]["content"]["effective"]
        ),
        (&serde_json::json!("file"), &serde_json::json!("2026-01-01")),
        "the operator's cost model is the one in force, here the test table"
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
        text.contains(r#"phoebis_attention{kind="pool_exhausted",subject="main"} 0"#),
        "the binary hands the attention reader to the metrics listener: {text}"
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

/// One `Ready` OAuth account and its credentials, written before `run` ever opens the store, the way the relay
/// harness seeds one for its own tests. The lock this takes on the data directory is released when the
/// returned `Store` drops at the end of the caller's scope, before `run` opens the same file.
fn seed_oauth_account(db_path: &std::path::Path, key_path: &std::path::Path) {
    let key = phoebis_store::StorageKey::from_file(key_path).unwrap();
    let store = phoebis_store::Store::open(db_path, &key).unwrap();
    if store.accounts().unwrap().is_empty() {
        store
            .insert_account(&phoebis_store::AccountRow {
                id: "a".into(),
                kind: phoebis_store::AccountKind::Oauth,
                label: "a".into(),
                enabled: true,
                identity: Some(phoebis_store::Identity {
                    account_uuid: "00000000-0000-4000-8000-000000000001".into(),
                    email: "a@example.invalid".into(),
                    organization_uuid: None,
                }),
                lifecycle_generation: 1,
                incarnation: 1,
                revision: 1,
                state: phoebis_store::AccountState::Ready,
                state_cause: phoebis_store::StateCause::Operator,
            })
            .unwrap();
        store
            .commit_credentials(
                "a",
                1,
                1,
                None,
                None,
                &phoebis_store::NewBundle {
                    access_token: phoebis_store::Secret::new("sk-ant-oat01-startup-fetch-aaaa"),
                    refresh_token: Some(phoebis_store::Secret::new(
                        "sk-ant-oat01-startup-fetch-refresh",
                    )),
                    issued_at: 0,
                    expires_at: i64::MAX / 2,
                    refresh_expires_at: None,
                    scopes: "user:inference".into(),
                },
            )
            .unwrap();
    }
}

/// The fixture head with an Opus 5 body in place of the captured fixture's own model; a test that needs a model the
/// subscription cost model has a weight for names it through `request_on`.
fn opus_request(
    client: &reqwest::Client,
    proxy_port: u16,
    secret: &str,
    session: &str,
) -> reqwest::RequestBuilder {
    request_on("claude-opus-5", client, proxy_port, secret, session)
}

/// The fixture head with a body on `model` in place of the captured fixture's own.
fn request_on(
    model: &str,
    client: &reqwest::Client,
    proxy_port: u16,
    secret: &str,
    session: &str,
) -> reqwest::RequestBuilder {
    let head = inference_head(FixtureKind::ApiKey);
    let (_, headers) = phoebis_transport::request::parse_head(head).unwrap();
    let mut body: serde_json::Value = serde_json::from_slice(body(BodyFixture::ApiKey)).unwrap();
    body["model"] = serde_json::json!(model);
    let mut req = client
        .post(format!(
            "http://127.0.0.1:{proxy_port}/v1/messages?beta=true"
        ))
        .json(&body);
    for (n, v) in headers {
        match n.to_ascii_lowercase().as_str() {
            "x-api-key" => {
                req = req.header("x-api-key", secret);
            }
            "host" | "content-length" | "content-type" | "x-claude-code-session-id" => {}
            _ => {
                req = req.header(n, v);
            }
        }
    }
    req.header("x-claude-code-session-id", session)
}

#[tokio::test]
async fn a_client_request_folds_the_pins_typed_burn_and_the_sessions_route_prices_it_in_window_share(
) {
    let up = FakeUpstream::start(vec![
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: Respond::Json {
                status: 200,
                headers: vec![],
                body: br#"{"id":"msg_1","type":"message","model":"claude-opus-5-5","usage":{"input_tokens":1000,"output_tokens":100}}"#.to_vec(),
            },
        },
        Rule {
            path_prefix: "/v1/models".into(),
            respond: Respond::Json {
                status: 200,
                headers: vec![],
                body: br#"{"data":[{"id":"claude-opus-5-5","type":"model"}]}"#.to_vec(),
            },
        },
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join("storage.key");
    std::fs::write(
        &key_path,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    )
    .unwrap();
    std::fs::write(dir.path().join("ca.pem"), up.ca_pem()).unwrap();
    let db_path = dir.path().join("phoebis.sqlite");
    // No `api_prices_path`, and its default `./api_prices.yaml` does not exist under the test's working directory, so
    // `Pricing::load` chooses the shipped table, matched against directly below; the cost model's default path is
    // just as absent.
    seed_oauth_account(&db_path, &key_path);
    phoebis_store::Store::open(
        &db_path,
        &phoebis_store::StorageKey::from_file(&key_path).unwrap(),
    )
    .unwrap()
    .set_profile(
        "a",
        "{}",
        &phoebis_store::SubscriptionRow {
            plan: None,
            tier: Some("default_claude_max_20x".into()),
            since: None,
            trial_ends_at: None,
        },
    )
    .unwrap();
    let (proxy_port, admin_port, metrics_port) = (free_port(), free_port(), free_port());
    let yaml = format!(
        "admin:\n  listen: 127.0.0.1:{admin_port}\n  secret_env: TEST_BURN_ADMIN_SECRET\nproxy:\n  listen: 127.0.0.1:{proxy_port}\nstats:\n  ledger_path: {ledger}\n  metrics_listen: 127.0.0.1:{metrics_port}\nstorage:\n  encryption_key_file: {key}\n  path: {db}\nupstream:\n  resolve_override: {addr}\n  extra_ca_pem: {ca}\nusage:\n  refresh_concurrency: 2\n",
        ledger = dir.path().join("ledger.sqlite").display(),
        key = key_path.display(),
        db = db_path.display(),
        addr = up.addr(),
        ca = dir.path().join("ca.pem").display(),
    );
    let cfg_path = dir.path().join("config.yaml");
    std::fs::write(&cfg_path, yaml).unwrap();
    let config = {
        let _env = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        std::env::set_var("TEST_BURN_ADMIN_SECRET", "admin-secret-001");
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
            .post(admin("/pools"))
            .json(&serde_json::json!({ "id": "main", "label": "Main", "strategy": "priority", "members": [{ "account_id": "a", "priority": 0 }] }))
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
    let resp = request_on("claude-opus-5-5", &client, proxy_port, &secret, "burn-e2e")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.text().await.unwrap();
    let sessions = wait_json(&client, &admin("/sessions"), |v| {
        v["sessions"][0]["burn"].as_f64().is_some_and(|b| b > 0.0)
    })
    .await;
    let row = &sessions["sessions"][0];
    assert_eq!(
        row["burn_unit"], "window_share",
        "an oauth account funded by its plan reads the share of its window: {row}"
    );
    let burn_at = row["burn_at"].as_i64().unwrap();
    let settings: serde_json::Value = client
        .get(admin("/settings"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        (
            &settings["api_prices"]["source"],
            &settings["subscription_costs"]["source"]
        ),
        (&serde_json::json!("shipped"), &serde_json::json!("shipped")),
        "both tables in force are the shipped ones the figures below are read from"
    );
    let tables = phoebis_ledger::PlanTables::shipped();
    let usage = phoebis_routing::PinUsage {
        input: 1_000,
        output: 100,
        ..Default::default()
    };
    // The default `burn_window_seconds` (no pool override above): a single renewal from a cold average is the
    // usage spread over that window, weighed by the band at the renewal, read per hour on Max 20x.
    let share = tables
        .share_at(
            phoebis_routing::FIVE_HOUR,
            "default_claude_max_20x",
            "claude-opus-5-5",
            usage,
            burn_at,
        )
        .unwrap();
    let expected_burn = share / 600.0 * 3_600.0;
    assert!(
        (row["burn"].as_f64().unwrap() - expected_burn).abs() < 1e-12,
        "{row} against {expected_burn}"
    );
    let _ = stop_tx.send(());
    server.await.unwrap().unwrap();
    let store = phoebis_store::Store::open(
        &db_path,
        &phoebis_store::StorageKey::from_file(&key_path).unwrap(),
    )
    .unwrap();
    let pin = store
        .load_routing()
        .unwrap()
        .pins
        .into_iter()
        .find(|p| p.key_id == "desktop" && p.session_id == "burn-e2e")
        .expect("the pin the request created is on disk once the binary has stopped");
    let per_second = |tokens: f64| tokens / 600.0;
    let band = tables.band_factor(phoebis_routing::FIVE_HOUR, burn_at);
    assert_eq!(
        (pin.burn, pin.burn_at),
        (
            phoebis_store::PinBurn {
                input: per_second(1_000.0),
                output: per_second(100.0),
                ..Default::default()
            },
            Some(burn_at)
        ),
        "the plain copy holds each type's tokens a second, unpriced"
    );
    assert!(
        (pin.burn_banded.input - per_second(1_000.0) * band).abs() < 1e-12
            && (pin.burn_banded.output - per_second(100.0) * band).abs() < 1e-12
            && pin.burn_banded.cache_read == 0.0,
        "the banded copy weighs the same tokens by the band at the renewal ({band}): {:?}",
        pin.burn_banded
    );
    let prompt = usage.input + usage.cache_read + usage.write_5m + usage.write_1h;
    let dollars = phoebis_ledger::PriceTable::shipped()
        .rate_table()
        .price("claude-opus-5", usage, prompt)
        .unwrap();
    assert!(
        (phoebis_ledger::PriceTable::shipped()
            .rate_table()
            .price_burn("claude-opus-5", &pin.burn, pin.last_input_tokens)
            .unwrap()
            - dollars / 600.0)
            .abs()
            < 1e-12,
        "priced when read, the plain copy is the dollar rate the usage costs over the window"
    );
    up.shutdown().await;
}

/// A refresh the previous run left in flight is quarantined at the start, and the routing log answers the settled
/// refresh and the account's move into `reauth_required`: the ledger's mirror takes its baseline before the
/// recovery runs.
#[tokio::test]
async fn a_refresh_left_in_flight_is_quarantined_at_the_start_and_the_routing_log_answers_both_rows(
) {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join("storage.key");
    std::fs::write(
        &key_path,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    )
    .unwrap();
    std::fs::write(dir.path().join("ca.pem"), up.ca_pem()).unwrap();
    let db_path = dir.path().join("phoebis.sqlite");
    seed_oauth_account(&db_path, &key_path);
    // On the seeded account's incarnation and generations, so the start quarantines it rather than closing the
    // attempt as superseded.
    phoebis_store::Store::open(
        &db_path,
        &phoebis_store::StorageKey::from_file(&key_path).unwrap(),
    )
    .unwrap()
    .open_refresh_attempt("a", 1, 1, 1)
    .unwrap();
    let (proxy_port, admin_port, metrics_port) = (free_port(), free_port(), free_port());
    let yaml = format!(
        "admin:\n  listen: 127.0.0.1:{admin_port}\n  secret_env: TEST_QUARANTINE_ADMIN_SECRET\nproxy:\n  listen: 127.0.0.1:{proxy_port}\nstats:\n  ledger_path: {ledger}\n  metrics_listen: 127.0.0.1:{metrics_port}\nstorage:\n  encryption_key_file: {key}\n  path: {db}\nupstream:\n  resolve_override: {addr}\n  extra_ca_pem: {ca}\n",
        ledger = dir.path().join("ledger.sqlite").display(),
        key = key_path.display(),
        db = db_path.display(),
        addr = up.addr(),
        ca = dir.path().join("ca.pem").display(),
    );
    let cfg_path = dir.path().join("config.yaml");
    std::fs::write(&cfg_path, yaml).unwrap();
    let config = {
        let _env = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        std::env::set_var("TEST_QUARANTINE_ADMIN_SECRET", "admin-secret-001");
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
    let log = wait_json(
        &client,
        &admin("/routing-log?accounts=a&kinds=account_state_changed,refresh_failed"),
        |log| log["rows"].as_array().is_some_and(|rows| rows.len() == 2),
    )
    .await;
    let row = |kind: &str| {
        log["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["kind"] == kind)
            .unwrap_or_else(|| panic!("no {kind} row: {log}"))
            .clone()
    };
    assert_eq!(
        row("refresh_failed")["detail"]["error"],
        "quarantined at start",
        "{log}"
    );
    assert_eq!(
        row("account_state_changed")["detail"],
        serde_json::json!({ "from": "ready", "to": "reauth_required", "cause": "refresh_failed", "detail": null }),
        "{log}"
    );
    let _ = stop_tx.send(());
    server.await.unwrap().unwrap();
    up.shutdown().await;
}

/// The catalog hook and the first purge's release, through the running router: a listing that changed between two
/// reads lands a `catalog_changed` row, and a pin the start found past its pool's affinity is purged once the start
/// releases the held tick, landing a `pin_expired` row.
#[tokio::test]
async fn a_changed_listing_lands_a_catalog_row_and_the_released_first_purge_expires_a_stale_pin() {
    let listing = |ids: &[&str]| Respond::Json {
        status: 200,
        headers: vec![],
        body: serde_json::to_vec(&serde_json::json!({
            "data": ids
                .iter()
                .map(|id| serde_json::json!({ "id": id, "type": "model" }))
                .collect::<Vec<_>>(),
        }))
        .unwrap(),
    };
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/models".into(),
        respond: listing(&["claude-sonnet-4-5"]),
    }])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join("storage.key");
    std::fs::write(
        &key_path,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    )
    .unwrap();
    std::fs::write(dir.path().join("ca.pem"), up.ca_pem()).unwrap();
    let db_path = dir.path().join("phoebis.sqlite");
    {
        let store = phoebis_store::Store::open(
            &db_path,
            &phoebis_store::StorageKey::from_file(&key_path).unwrap(),
        )
        .unwrap();
        pair_oauth(
            &store,
            "a",
            "sk-ant-oat01-catalog-hook-aaaa",
            "00000000-0000-4000-8000-00000000000a",
        );
        store
            .insert_pool(&phoebis_store::PoolRow::for_tests("main"), "test")
            .unwrap();
        store
            .set_pool_members(
                "main",
                &[phoebis_store::MemberWrite {
                    account_id: "a".into(),
                    priority: 0,
                    budget_daily_usd: None,
                    budget_monthly_usd: None,
                }],
                "test",
            )
            .unwrap();
        store
            .insert_api_key(
                &phoebis_store::ApiKeyRow {
                    id: "desktop".into(),
                    label: "Desktop".into(),
                    notes: None,
                    pool_id: "main".into(),
                    enabled: true,
                    revoked_at: None,
                    expires_at: None,
                    model_rules: Vec::new(),
                    model_rules_mode: phoebis_store::RulesMode::Extend,
                    long_context_twins: true,
                    keepalive_opt_out: false,
                    body_capture_hours: None,
                    secret_digest: store.api_key_digest("phoebis-catalog-hook-secret").to_vec(),
                    prefix: phoebis_store::key_prefix("phoebis-catalog-hook-secret"),
                    incarnation: 1,
                    created_at: 0,
                    updated_at: 0,
                    rotated_at: None,
                    last_used_at: None,
                    retired_secret_last_used_at: None,
                    auto_mode_server: None,
                },
                "test",
            )
            .unwrap();
        // Made for the key and the account as the store holds them, so the purge reads it as the holders' own, and
        // last used at the epoch, so the pool's hour of affinity has long run out when the first purge reads it.
        store
            .upsert_pin(&phoebis_store::PinRow {
                key_id: "desktop".into(),
                session_id: "stale".into(),
                agent_id: "main".into(),
                model: "claude-sonnet-4-5".into(),
                account_id: "a".into(),
                epoch: 1,
                created_at: 0,
                last_used_at: 0,
                key_generation: store.api_key("desktop").unwrap().unwrap().created_at,
                incarnation: store.account("a").unwrap().unwrap().incarnation,
                burn: phoebis_store::PinBurn::default(),
                burn_at: None,
                burn_banded: phoebis_store::PinBurn::default(),
                last_input_tokens: 0,
                last_refresh_at: 0,
                cache_ttl_ms: 300_000,
                shed_from: None,
                shed_at: None,
                static_hash: None,
                tail_hash: None,
                lane: String::new(),
            })
            .unwrap();
    }
    let (proxy_port, admin_port, metrics_port) = (free_port(), free_port(), free_port());
    let yaml = format!(
        "admin:\n  listen: 127.0.0.1:{admin_port}\n  secret_env: TEST_CATALOG_HOOK_ADMIN_SECRET\nproxy:\n  listen: 127.0.0.1:{proxy_port}\nstats:\n  ledger_path: {ledger}\n  metrics_listen: 127.0.0.1:{metrics_port}\nstorage:\n  encryption_key_file: {key}\n  path: {db}\nupstream:\n  resolve_override: {addr}\n  extra_ca_pem: {ca}\n",
        ledger = dir.path().join("ledger.sqlite").display(),
        key = key_path.display(),
        db = db_path.display(),
        addr = up.addr(),
        ca = dir.path().join("ca.pem").display(),
    );
    let cfg_path = dir.path().join("config.yaml");
    std::fs::write(&cfg_path, yaml).unwrap();
    let config = {
        let _env = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        std::env::set_var("TEST_CATALOG_HOOK_ADMIN_SECRET", "admin-secret-001");
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
    // An account's first listing names none before it, so it moves nothing and lands no row.
    wait_json(&client, &admin("/accounts/a/catalog"), |catalog| {
        catalog["models"]
            .as_array()
            .is_some_and(|models| !models.is_empty())
    })
    .await;
    up.set_rule_after(
        0,
        "/v1/models",
        listing(&["claude-opus-5", "claude-sonnet-4-5"]),
    );
    let refreshed: serde_json::Value = client
        .get(admin("/accounts/a/catalog?refresh=1"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        refreshed["models"].as_array().map(Vec::len),
        Some(2),
        "{refreshed}"
    );
    let log = wait_json(
        &client,
        &admin("/routing-log?accounts=a&kinds=catalog_changed,pin_expired"),
        |log| log["rows"].as_array().is_some_and(|rows| rows.len() == 2),
    )
    .await;
    let row = |kind: &str| {
        log["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["kind"] == kind)
            .unwrap_or_else(|| panic!("no {kind} row: {log}"))
            .clone()
    };
    assert_eq!(
        row("catalog_changed")["detail"],
        serde_json::json!({ "added": ["claude-opus-5"], "removed": [], "count": 2 }),
        "{log}"
    );
    let expired = row("pin_expired");
    assert_eq!(
        (&expired["session_id"], &expired["detail"]),
        (
            &serde_json::json!("stale"),
            &serde_json::json!({ "agent_id": "main", "reasons": ["expired"] })
        ),
        "{log}"
    );
    let _ = stop_tx.send(());
    server.await.unwrap().unwrap();
    up.shutdown().await;
}

#[tokio::test]
async fn the_proxy_listener_binds_after_the_startup_fetch_or_its_bound_whichever_comes_first() {
    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join("storage.key");
    std::fs::write(
        &key_path,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    )
    .unwrap();
    let db_path = dir.path().join("phoebis.sqlite");
    let ca_path = dir.path().join("ca.pem");

    // Each scenario gets its own ledger: the store file is reused across all three so its process lock is the
    // one under test, but a shared ledger would carry the previous scenario's quota rows into the next.
    let config_yaml = |admin_port: u16,
                       proxy_port: u16,
                       metrics_port: u16,
                       addr: std::net::SocketAddr,
                       ledger_path: &std::path::Path,
                       routing: &str| {
        format!(
            "admin:\n  listen: 127.0.0.1:{admin_port}\n  secret_env: TEST_STARTUP_ADMIN\n\
             proxy:\n  listen: 127.0.0.1:{proxy_port}\n{routing}\
             stats:\n  ledger_path: {ledger}\n  metrics_listen: 127.0.0.1:{metrics_port}\n\
             storage:\n  encryption_key_file: {key}\n  path: {db}\n\
             upstream:\n  resolve_override: {addr}\n  extra_ca_pem: {ca}\n\
             usage:\n  refresh_concurrency: 2\n",
            ledger = ledger_path.display(),
            key = key_path.display(),
            db = db_path.display(),
            ca = ca_path.display(),
        )
    };
    let load = |cfg_path: &std::path::Path| {
        let _env = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        std::env::set_var("TEST_STARTUP_ADMIN", "admin-secret-001");
        phoebis_router::config::load(cfg_path).unwrap()
    };
    async fn first_success(proxy_port: u16) -> std::time::Duration {
        let client = reqwest::Client::new();
        let started = std::time::Instant::now();
        loop {
            if let Ok(resp) = client
                .head(format!("http://127.0.0.1:{proxy_port}/api/hello"))
                .send()
                .await
            {
                assert_eq!(resp.status(), 200);
                return started.elapsed();
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
    let models_ok = || Rule {
        path_prefix: "/v1/models".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"data":[{"id":"claude-sonnet-4-5","type":"model"}]}"#.to_vec(),
        },
    };
    let profile_ok = || Rule {
        path_prefix: "/api/oauth/profile".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: control_response(ControlFixture::Profile).to_vec(),
        },
    };
    let usage_ok = || Rule {
        path_prefix: "/api/oauth/usage".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: control_response(ControlFixture::Usage).to_vec(),
        },
    };

    // Every startup fetch answers at once, so the proxy binds with the sample already recorded.
    {
        let up = FakeUpstream::start(vec![models_ok(), profile_ok(), usage_ok()]).await;
        std::fs::write(&ca_path, up.ca_pem()).unwrap();
        seed_oauth_account(&db_path, &key_path);
        let (admin_port, proxy_port, metrics_port) = (free_port(), free_port(), free_port());
        let cfg_path = dir.path().join("startup-immediate.yaml");
        let ledger_path = dir.path().join("ledger-immediate.sqlite");
        std::fs::write(
            &cfg_path,
            config_yaml(
                admin_port,
                proxy_port,
                metrics_port,
                up.addr(),
                &ledger_path,
                "",
            ),
        )
        .unwrap();
        let config = load(&cfg_path);
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(phoebis_router::app::run(config, async {
            let _ = stop_rx.await;
        }));
        let elapsed = first_success(proxy_port).await;
        assert!(elapsed < std::time::Duration::from_secs(1), "{elapsed:?}");
        let client = reqwest::Client::builder()
            .cookie_store(true)
            .build()
            .unwrap();
        let admin = |path: &str| format!("http://127.0.0.1:{admin_port}/admin{path}");
        logged_in(&client, &admin("/session")).await;
        let account: serde_json::Value = client
            .get(admin("/accounts/a"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            !account["windows"].as_array().unwrap().is_empty(),
            "the sample landed before the proxy listener bound: {account}"
        );
        let _ = stop_tx.send(());
        server.await.unwrap().unwrap();
        up.shutdown().await;
    }

    // The usage endpoint never answers within the one-second bound the file sets: the proxy still binds at
    // the bound, and the sample the stalled fetch would have carried has not landed yet.
    {
        let up = FakeUpstream::start(vec![
            models_ok(),
            profile_ok(),
            Rule {
                path_prefix: "/api/oauth/usage".into(),
                respond: Respond::Gated(Box::new(usage_ok().respond)),
            },
        ])
        .await;
        std::fs::write(&ca_path, up.ca_pem()).unwrap();
        seed_oauth_account(&db_path, &key_path);
        let (admin_port, proxy_port, metrics_port) = (free_port(), free_port(), free_port());
        let cfg_path = dir.path().join("startup-gated.yaml");
        let ledger_path = dir.path().join("ledger-gated.sqlite");
        std::fs::write(
            &cfg_path,
            config_yaml(
                admin_port,
                proxy_port,
                metrics_port,
                up.addr(),
                &ledger_path,
                "routing:\n  startup_fetch_seconds: 1\n",
            ),
        )
        .unwrap();
        let config = load(&cfg_path);
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(phoebis_router::app::run(config, async {
            let _ = stop_rx.await;
        }));
        let elapsed = first_success(proxy_port).await;
        assert!(
            elapsed >= std::time::Duration::from_secs(1)
                && elapsed < std::time::Duration::from_secs(4),
            "{elapsed:?}"
        );
        let client = reqwest::Client::builder()
            .cookie_store(true)
            .build()
            .unwrap();
        let admin = |path: &str| format!("http://127.0.0.1:{admin_port}/admin{path}");
        logged_in(&client, &admin("/session")).await;
        // The account's windows may already hold what the heads of the requests above reported, so the evidence
        // is the poll's standing, which only the stalled fetch can move.
        let usage: serde_json::Value = client
            .get(admin("/accounts/a/usage"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            usage["poll"].is_null() || usage["poll"]["last_clean_at"].is_null(),
            "the stalled fetch had not reported by the bound: {usage}"
        );
        let _ = stop_tx.send(());
        server.await.unwrap().unwrap();
        // The gated usage fetch still runs in the background, holding the store open; releasing it lets that
        // fetch finish and drop its hold before the file is reused below.
        up.release();
        let give_up = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Ok(store) = phoebis_store::Store::open(
                &db_path,
                &phoebis_store::StorageKey::from_file(&key_path).unwrap(),
            ) {
                drop(store);
                break;
            }
            assert!(
                std::time::Instant::now() < give_up,
                "the background fetch never released the store"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        up.shutdown().await;
    }

    // The catalog fetch fails and its retry sleeps `ERROR_HOLD`, sixty seconds: the proxy still binds at the
    // bound, and the retry does not outlive `run` holding the store and its process lock.
    {
        let up = FakeUpstream::start(vec![
            Rule {
                path_prefix: "/v1/models".into(),
                respond: Respond::Json {
                    status: 503,
                    headers: vec![],
                    body: b"{}".to_vec(),
                },
            },
            profile_ok(),
            usage_ok(),
        ])
        .await;
        std::fs::write(&ca_path, up.ca_pem()).unwrap();
        seed_oauth_account(&db_path, &key_path);
        let (admin_port, proxy_port, metrics_port) = (free_port(), free_port(), free_port());
        let cfg_path = dir.path().join("startup-failing-catalog.yaml");
        let ledger_path = dir.path().join("ledger-failing-catalog.sqlite");
        std::fs::write(
            &cfg_path,
            config_yaml(
                admin_port,
                proxy_port,
                metrics_port,
                up.addr(),
                &ledger_path,
                "",
            ),
        )
        .unwrap();
        let config = load(&cfg_path);
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let started = std::time::Instant::now();
        let server = tokio::spawn(phoebis_router::app::run(config, async {
            let _ = stop_rx.await;
        }));
        let _ = first_success(proxy_port).await;
        let _ = stop_tx.send(());
        server.await.unwrap().unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(35),
            "{:?}",
            started.elapsed()
        );
        // No retry outlived `run` holding the store and its process lock.
        drop(
            phoebis_store::Store::open(
                &db_path,
                &phoebis_store::StorageKey::from_file(&key_path).unwrap(),
            )
            .unwrap(),
        );
        up.shutdown().await;
    }
}

/// An OAuth account paired straight into the store, as the pairing flow leaves one.
fn pair_oauth(store: &phoebis_store::Store, id: &str, token: &str, account_uuid: &str) {
    store
        .insert_account(&phoebis_store::AccountRow {
            id: id.into(),
            kind: phoebis_store::AccountKind::Oauth,
            label: id.into(),
            enabled: true,
            identity: Some(phoebis_store::Identity {
                account_uuid: account_uuid.into(),
                email: format!("{id}@example.invalid"),
                organization_uuid: None,
            }),
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state: phoebis_store::AccountState::Ready,
            state_cause: phoebis_store::StateCause::Operator,
        })
        .unwrap();
    // A re-paired id takes the next incarnation, which the credential write is fenced on.
    let incarnation = store.account(id).unwrap().unwrap().incarnation;
    store
        .commit_credentials(
            id,
            incarnation,
            1,
            None,
            None,
            &phoebis_store::NewBundle {
                access_token: phoebis_store::Secret::new(token),
                refresh_token: Some(phoebis_store::Secret::new("sk-ant-ort01-persisted-refresh")),
                issued_at: 0,
                expires_at: i64::MAX / 2,
                refresh_expires_at: None,
                scopes: "user:inference".into(),
            },
        )
        .unwrap();
}

/// `a` and `b` on one priority, since Pace ranks within the first priority tier: the windows alone order them.
fn members() -> Vec<phoebis_store::MemberWrite> {
    ["a", "b"]
        .into_iter()
        .map(|id| phoebis_store::MemberWrite {
            account_id: id.into(),
            priority: 0,
            budget_daily_usd: None,
            budget_monthly_usd: None,
        })
        .collect()
}

#[tokio::test]
async fn a_startup_poll_the_endpoint_refuses_routes_the_first_request_from_the_persisted_windows() {
    const A_TOKEN: &str = "sk-ant-oat01-persisted-aaaa";
    const B_TOKEN: &str = "sk-ant-oat01-persisted-bbbb";
    const KEY_SECRET: &str = "phoebis-persisted-windows-secret";
    let up = FakeUpstream::start(vec![
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: Respond::Json {
                status: 200,
                headers: vec![],
                body: br#"{"id":"msg_1","type":"message","model":"claude-opus-5","usage":{"input_tokens":1000,"output_tokens":100}}"#.to_vec(),
            },
        },
        Rule {
            path_prefix: "/v1/models".into(),
            respond: Respond::Json {
                status: 200,
                headers: vec![],
                body: br#"{"data":[{"id":"claude-opus-5","type":"model"}]}"#.to_vec(),
            },
        },
        Rule {
            path_prefix: "/api/oauth/profile".into(),
            respond: Respond::Json {
                status: 200,
                headers: vec![],
                body: control_response(ControlFixture::Profile).to_vec(),
            },
        },
        Rule {
            path_prefix: "/api/oauth/usage".into(),
            respond: Respond::Json {
                status: 429,
                headers: vec![],
                body: br#"{"type":"error","error":{"type":"rate_limit_error","message":"limit"}}"#.to_vec(),
            },
        },
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join("storage.key");
    std::fs::write(
        &key_path,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    )
    .unwrap();
    let ca_path = dir.path().join("ca.pem");
    std::fs::write(&ca_path, up.ca_pem()).unwrap();
    let db_path = dir.path().join("phoebis.sqlite");
    let storage_key = || phoebis_store::StorageKey::from_file(&key_path).unwrap();
    // The run before this one: `a` read near its limit, `b` barely touched, both a day from their reset.
    let reset = jiff::Timestamp::now().as_millisecond() + 86_400_000;
    {
        let store = phoebis_store::Store::open(&db_path, &storage_key()).unwrap();
        pair_oauth(&store, "a", A_TOKEN, "00000000-0000-4000-8000-00000000000a");
        pair_oauth(&store, "b", B_TOKEN, "00000000-0000-4000-8000-00000000000b");
        store
            .insert_pool(
                &phoebis_store::PoolRow {
                    label: "Main".into(),
                    prefer_subscription: false,
                    ..phoebis_store::PoolRow::for_tests("main")
                },
                "test",
            )
            .unwrap();
        store.set_pool_members("main", &members(), "test").unwrap();
        store
            .insert_api_key(
                &phoebis_store::ApiKeyRow {
                    id: "desktop".into(),
                    label: "Desktop".into(),
                    notes: None,
                    pool_id: "main".into(),
                    enabled: true,
                    revoked_at: None,
                    expires_at: None,
                    model_rules: Vec::new(),
                    model_rules_mode: phoebis_store::RulesMode::Extend,
                    long_context_twins: true,
                    keepalive_opt_out: false,
                    body_capture_hours: None,
                    secret_digest: store.api_key_digest(KEY_SECRET).to_vec(),
                    prefix: phoebis_store::key_prefix(KEY_SECRET),
                    incarnation: 1,
                    created_at: 0,
                    updated_at: 0,
                    rotated_at: None,
                    last_used_at: None,
                    retired_secret_last_used_at: None,
                    auto_mode_server: None,
                },
                "test",
            )
            .unwrap();
        for (id, utilization) in [("a", 0.9), ("b", 0.1)] {
            for window in [phoebis_routing::FIVE_HOUR, phoebis_routing::SEVEN_DAY] {
                assert!(store
                    .upsert_window(&phoebis_store::WindowRow {
                        account_id: id.into(),
                        incarnation: 1,
                        observed_at: reset - 2 * 86_400_000,
                        reset: Some(reset),
                        utilization,
                        window: window.into(),
                    })
                    .unwrap());
            }
        }
    }
    let run = |name: &str| {
        let (admin_port, proxy_port, metrics_port) = (free_port(), free_port(), free_port());
        let cfg_path = dir.path().join(format!("{name}.yaml"));
        std::fs::write(
            &cfg_path,
            format!(
                "admin:\n  listen: 127.0.0.1:{admin_port}\n  secret_env: TEST_PERSISTED_ADMIN\n\
                 proxy:\n  listen: 127.0.0.1:{proxy_port}\n\
                 routing:\n  startup_fetch_seconds: 5\n\
                 stats:\n  ledger_path: {ledger}\n  metrics_listen: 127.0.0.1:{metrics_port}\n\
                 storage:\n  encryption_key_file: {key}\n  path: {db}\n\
                 upstream:\n  resolve_override: {addr}\n  extra_ca_pem: {ca}\n\
                 usage:\n  refresh_concurrency: 2\n",
                ledger = dir.path().join(format!("{name}-ledger.sqlite")).display(),
                key = key_path.display(),
                db = db_path.display(),
                addr = up.addr(),
                ca = ca_path.display(),
            ),
        )
        .unwrap();
        let config = {
            let _env = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            std::env::set_var("TEST_PERSISTED_ADMIN", "admin-secret-001");
            phoebis_router::config::load(&cfg_path).unwrap()
        };
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(phoebis_router::app::run(config, async {
            let _ = stop_rx.await;
        }));
        (admin_port, proxy_port, stop_tx, server)
    };
    async fn bound(proxy_port: u16) -> std::time::Duration {
        let client = reqwest::Client::new();
        let started = std::time::Instant::now();
        loop {
            if client
                .head(format!("http://127.0.0.1:{proxy_port}/api/hello"))
                .send()
                .await
                .is_ok()
            {
                return started.elapsed();
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
    // With the persisted windows lost, `a` and `b` tie (one profile fixture, no reading) and Pace breaks the tie at
    // random, so a `served_by` check alone passes half the time; the usage `GET`'s two windows are what pin the
    // reload, and the routing task's tests pin the ranking.
    let served_by = |up: &FakeUpstream| {
        up.requests()
            .iter()
            .rev()
            .find(|r| r.line.contains("/v1/messages"))
            .and_then(|r| r.credential().map(str::to_string))
    };
    // The startup step's refused read is the account's standing, whatever windows the answer carries.
    let held_usage = |client: &reqwest::Client, admin_port: u16| {
        let client = client.clone();
        async move {
            wait_json(
                &client,
                &format!("http://127.0.0.1:{admin_port}/admin/accounts/a/usage"),
                |usage| usage["poll"]["last_outcome"] == "rate_limited",
            )
            .await
        }
    };
    let admin_client = || {
        reqwest::Client::builder()
            .cookie_store(true)
            .build()
            .unwrap()
    };

    let (admin_port, proxy_port, stop_tx, server) = run("first");
    let elapsed = bound(proxy_port).await;
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "the listener binds inside the bound: {elapsed:?}"
    );
    let client = admin_client();
    logged_in(
        &client,
        &format!("http://127.0.0.1:{admin_port}/admin/session"),
    )
    .await;
    let resp = opus_request(&client, proxy_port, KEY_SECRET, "persisted-1")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.text().await.unwrap();
    assert_eq!(
        served_by(&up).as_deref(),
        Some(B_TOKEN),
        "the persisted windows leave b more allowance per hour than a"
    );
    let usage = held_usage(&client, admin_port).await;
    assert!(
        usage["error"].is_null(),
        "the persisted windows answer in place of the refused read: {usage}"
    );
    assert_eq!(
        usage["windows"].as_array().map(Vec::len),
        Some(2),
        "{usage}"
    );
    let _ = stop_tx.send(());
    server.await.unwrap().unwrap();

    // `a` is deleted and paired again under its id: the replacement is another holder, and the rows the first
    // holder left went with it.
    let give_up = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let store = loop {
        if let Ok(store) = phoebis_store::Store::open(&db_path, &storage_key()) {
            break store;
        }
        assert!(
            std::time::Instant::now() < give_up,
            "the first run never released the store"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    store.delete_account("a", "test").unwrap();
    pair_oauth(&store, "a", A_TOKEN, "00000000-0000-4000-8000-00000000000a");
    store.set_pool_members("main", &members(), "test").unwrap();
    assert_eq!(store.account("a").unwrap().unwrap().incarnation, 2);
    drop(store);

    let (admin_port, proxy_port, stop_tx, server) = run("second");
    bound(proxy_port).await;
    let client = admin_client();
    logged_in(
        &client,
        &format!("http://127.0.0.1:{admin_port}/admin/session"),
    )
    .await;
    let usage = held_usage(&client, admin_port).await;
    assert!(
        !usage["error"].is_null(),
        "the replacement holds no windows, so the refused read is the answer: {usage}"
    );
    assert_eq!(usage["windows"], serde_json::json!([]), "{usage}");
    let resp = opus_request(&client, proxy_port, KEY_SECRET, "persisted-2")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.text().await.unwrap();
    assert_eq!(
        served_by(&up).as_deref(),
        Some(B_TOKEN),
        "b's persisted rows still rank it above the untouched replacement of a"
    );
    let _ = stop_tx.send(());
    server.await.unwrap().unwrap();
    let give_up = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let store = loop {
        if let Ok(store) = phoebis_store::Store::open(&db_path, &storage_key()) {
            break store;
        }
        assert!(
            std::time::Instant::now() < give_up,
            "the second run never released the store"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert_eq!(
        store
            .load_routing()
            .unwrap()
            .windows
            .iter()
            .map(|w| (w.account_id.as_str(), w.window.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("b", phoebis_routing::FIVE_HOUR),
            ("b", phoebis_routing::SEVEN_DAY)
        ],
        "b's rows are still there and the replacement a holds none"
    );
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
        "admin:\n  listen: 127.0.0.1:{admin_port}\n  secret_env: TEST_ADMIN_SECRET\nkeepalive:\n  max_sessions: 64\nproxy:\n  listen: 127.0.0.1:{proxy_port}\nstats:\n  ledger_path: {ledger}\n  metrics_listen: 127.0.0.1:{metrics_port}\nstorage:\n  encryption_key_file: {key}\n  path: {db}\nupstream:\n  resolve_override: {addr}\n  extra_ca_pem: {ca}\nusage:\n  refresh_concurrency: 2\n",
        ledger = dir.path().join("ledger.sqlite").display(), key = dir.path().join("storage.key").display(), db = dir.path().join("phoebis.sqlite").display(), addr = up.addr(), ca = dir.path().join("ca.pem").display(),
    );
    let cfg_path = dir.path().join("config.yaml");
    std::fs::write(&cfg_path, yaml).unwrap();
    probe_soon_on();
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
    probe_soon_off();
    assert_eq!(client.post(admin("/accounts")).json(&serde_json::json!({ "id": "console", "kind": "api_key", "label": "Console", "secret": "sk-ant-api03-PLACEHOLDER" })).send().await.unwrap().status(), 201);
    assert_eq!(client.post(admin("/pools")).json(&serde_json::json!({ "id": "main", "label": "Main", "strategy": "priority", "members": [{ "account_id": "console", "priority": 0 }], "keepalive": { "enabled": true } })).send().await.unwrap().status(), 201);
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
    // Registered through the keepalive route under the key, so the recipe is retained.
    let events = format!("http://127.0.0.1:{proxy_port}/v0/keepalive/events");
    let registration = serde_json::json!({
        "protocol": 1,
        "event": "session_start",
        "session_id": "e2e-warm",
        "instance_id": "e2e-instance",
        "sequence": 1,
    });
    let ack = client
        .post(&events)
        .header("authorization", format!("Bearer {secret}"))
        .json(&registration)
        .send()
        .await
        .unwrap();
    assert_eq!(ack.status(), 200);
    assert_eq!(
        client
            .post(&events)
            .json(&registration)
            .send()
            .await
            .unwrap()
            .status(),
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
        (Some("e2e-warm"), Some("retained"))
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
    assert_eq!(
        (
            overview["limits"]["sessions"]["count"].as_u64(),
            overview["limits"]["sessions"]["max"].as_u64()
        ),
        (Some(1), Some(64)),
        "the session count beside the cap the file sets"
    );
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
        text.contains(r#"phoebis_keepalive_probes_total{outcome="cold",pool="main"}"#),
        "the probe read part of the request's prefix back, so it counts cold under its pool: {text}"
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
        .get(format!("http://127.0.0.1:{admin_port}/accounts/business"))
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
    let session = api.json::<serde_json::Value>().await.unwrap();
    assert!(
        session["session_id"].is_string() && session.get("role").is_none(),
        "the session answer names the session and no role: {session}"
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

/// The headless browser pass the full gate ends with: every view in both themes at 1280 wide, one element
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
        "admin:\n  listen: 127.0.0.1:{admin_port}\n  secret_env: TEST_ADMIN_SECRET\nproxy:\n  listen: 127.0.0.1:{proxy_port}\nrouting:\n  cooldown_fallback_seconds: 3600\nstats:\n  ledger_path: {ledger}\n  metrics_listen: 127.0.0.1:{metrics_port}\nstorage:\n  encryption_key_file: {key}\n  path: {db}\nupstream:\n  resolve_override: {addr}\n  extra_ca_pem: {ca}\nusage:\n  refresh_concurrency: 2\n",
        ledger = dir.path().join("ledger.sqlite").display(), key = dir.path().join("storage.key").display(), db = dir.path().join("phoebis.sqlite").display(), addr = up.addr(), ca = dir.path().join("ca.pem").display(),
    );
    let cfg_path = dir.path().join("config.yaml");
    std::fs::write(&cfg_path, yaml).unwrap();
    probe_soon_on();
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
    probe_soon_off();
    assert_eq!(client.post(admin("/accounts")).json(&serde_json::json!({ "id": "console", "kind": "api_key", "label": "Console", "secret": "sk-ant-api03-PLACEHOLDER" })).send().await.unwrap().status(), 201);
    assert_eq!(client.post(admin("/pools")).json(&serde_json::json!({ "id": "main", "label": "Main", "strategy": "pace", "affinity_ttl_seconds": 600, "members": [{ "account_id": "console", "priority": 0 }], "keepalive": { "enabled": true }, "body_capture_hours": 24 })).send().await.unwrap().status(), 201);
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
    let relayed = fixture_request(&client, proxy_port, &secret, "e2e-ui")
        .body(adaptive.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(relayed.status(), 200);
    let _ = relayed.text().await.unwrap();
    // Registered through the keepalive route under the key, so the recipe is retained.
    let events = format!("http://127.0.0.1:{proxy_port}/v0/keepalive/events");
    assert_eq!(
        client
            .post(&events)
            .header("authorization", format!("Bearer {secret}"))
            .json(&serde_json::json!({
                "protocol": 1,
                "event": "session_start",
                "session_id": "e2e-ui",
                "instance_id": "e2e-instance",
                "sequence": 1,
            }))
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
        (Some("e2e-ui"), Some("retained"))
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
                .any(|r| r["wasted_probes"].as_u64().unwrap_or(0) >= 1)
        },
    )
    .await;
    // The pool's only member answers a unified 429 once: the router cools it, finds no sibling, answers the
    // exhaustion 429, and the attention reader holds the pool's pool_exhausted condition. The standing answer
    // under this credential stays the 200, so a keepalive probe after the 429 is still served.
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
    let _ = wait_json(&client, &admin("/attention"), |v| {
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["kind"] == "pool_exhausted" && i["subject"]["id"] == "main")
    })
    .await;
    let _ = wait_json(&client, &admin("/requests"), |v| {
        !v["requests"].as_array().unwrap().is_empty()
    })
    .await;
    let shots = std::env::var("PHOEBIS_SCREENSHOT_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("phoebis-screenshots"));
    let web = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../web");
    // The directory may also hold the mock pass's shots, so only the files written from here on are this pass's.
    let started = std::time::SystemTime::now();
    // Awaited, never blocked on: the router and the fake upstream are tasks on this same current-thread
    // runtime, and a blocking wait here would starve the very server the browser is talking to.
    let mut pass = tokio::process::Command::new("node")
        .arg("e2e/views.mjs")
        .current_dir(&web)
        .env("PHOEBIS_E2E_BASE", format!("http://127.0.0.1:{admin_port}"))
        .env("PHOEBIS_E2E_SECRET", "admin-secret-001")
        .env("PHOEBIS_SCREENSHOT_DIR", &shots)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("node runs");
    // Each line is passed on as the pass writes it, so a pass that hangs under `--nocapture` shows the view it
    // reached. Both pipes drain together, or a full stderr pipe would stall the pass behind a stdout read.
    use tokio::io::AsyncBufReadExt;
    let mut out = tokio::io::BufReader::new(pass.stdout.take().expect("piped stdout")).lines();
    let mut err = tokio::io::BufReader::new(pass.stderr.take().expect("piped stderr")).lines();
    let kept = async {
        let mut kept = Vec::new();
        while let Some(line) = out.next_line().await.expect("the pass's stdout reads") {
            println!("{line}");
            kept.push(line);
        }
        kept
    };
    let forwarded = async {
        while let Some(line) = err.next_line().await.expect("the pass's stderr reads") {
            eprintln!("{line}");
        }
    };
    let (printed, ()) = tokio::join!(kept, forwarded);
    let status = pass.wait().await.expect("node runs");
    assert!(
        status.success(),
        "the browser pass failed; its output names the view"
    );
    // The pass names how many views it ran, so a view added to it needs no count here.
    let views: usize = printed
        .iter()
        .find_map(|line| {
            let (count, _) = line
                .strip_prefix("browser pass: ")?
                .split_once(" views in 2 themes")?;
            count.parse().ok()
        })
        .expect("the pass prints its summary line");
    let pngs = walkdir(&shots)
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "png"))
        .filter(|p| {
            std::fs::metadata(p)
                .and_then(|metadata| metadata.modified())
                .is_ok_and(|written| written >= started)
        })
        .count();
    assert_eq!(
        pngs,
        2 * views,
        "{views} views in two themes under {}",
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
        assert_eq!(client.post(admin("/pools")).json(&serde_json::json!({ "id": pool, "label": pool, "strategy": "priority", "members": [{ "account_id": account, "priority": 0 }] })).send().await.unwrap().status(), 201);
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
        let status = replay_fixture(
            proxy_port,
            inference_head(head),
            body(fixture),
            secret,
            &format!("capture-{i}"),
        )
        .await;
        assert_eq!(status, 200, "run {i}");
    }
    let posts: Vec<_> = up
        .requests()
        .into_iter()
        .filter(|r| r.line.starts_with("POST /v1/messages"))
        .collect();
    assert_eq!(posts.len(), 4);
    // Every stamped body carries either the placeholder the relay restored on a reduced line, or a client hash
    // the relay re-signed, which verifies under this build's rules; --skip-hash-check leaves it here.
    let (mut placeholders, mut signed) = (0, 0);
    for r in &posts {
        if let Some(l) = phoebis_cch::find_cch(&r.body) {
            if r.body[l.digits_start..l.digits_start + 5] == *b"00000" {
                placeholders += 1;
            } else {
                assert_eq!(
                    phoebis_cch::sign(&r.body).unwrap(),
                    r.body,
                    "the restamp is valid over the edited bytes"
                );
                signed += 1;
            }
        }
    }
    assert!(
        placeholders > 0 && signed > 0,
        "both shapes reached the upstream: {placeholders} placeholders, {signed} signed"
    );
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
