//! The subcommands' admin client against routers started in-process: the bearer on a plain listener, the
//! self-signed CA trusted from the configuration file, a refused secret, a port nothing listens on, a route's own
//! refusal, and a body that stalls past the deadline.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use fake_upstream::FakeUpstream;
use hyper::Method;
use phoebis_router::admin_client::{AdminClient, ClientError, Common};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// `load` and `from_flags` read the admin secret from the process environment, so each test holds this across its
/// set-then-read sequence.
static ENV: Mutex<()> = Mutex::new(());

/// How long a boot may take to bind its listeners; generous because it is reached only when the boot never ends.
const SETTLE: Duration = Duration::from_secs(30);

const ADMIN_SECRET: &str = "admin-secret-client-001";

/// A port from a hundred-port slice below the kernel's ephemeral range (32768 upward on Linux), spread by pid as
/// `end_to_end.rs` spreads its own, so no outbound connection takes it between the probe and the router's bind.
fn free_port() -> u16 {
    static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
    let base = 20_000 + (std::process::id() % 100) as u16 * 100;
    loop {
        let offset = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert!(offset < 100, "admin_client.rs drew past its port slice");
        if std::net::TcpListener::bind(("127.0.0.1", base + offset)).is_ok() {
            return base + offset;
        }
    }
}

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn flags(url: Option<String>, secret_env: Option<&str>) -> Common {
    Common {
        ca: None,
        json: false,
        secret_env: secret_env.map(str::to_owned),
        timeout: 10,
        url,
    }
}

/// `from_flags` with `name` set to `value` for the read.
fn client_with(
    common: &Common,
    config: &Path,
    name: &str,
    value: &str,
) -> Result<AdminClient, ClientError> {
    let _env = env_lock();
    std::env::set_var(name, value);
    AdminClient::from_flags(common, config)
}

/// A router started in-process on fresh loopback ports, its registry empty, its upstream the fake.
struct Running {
    stop: tokio::sync::oneshot::Sender<()>,
    server: tokio::task::JoinHandle<anyhow::Result<()>>,
    admin: u16,
    config: PathBuf,
    dir: tempfile::TempDir,
    _up: FakeUpstream,
}

impl Running {
    async fn stop(self) {
        let _ = self.stop.send(());
        self.server.await.unwrap().unwrap();
    }
}

/// Starts the router with `admin_tls` (a `  tls: ...` line or nothing) under `admin`, the admin secret read from
/// `secret_env`, and returns once the admin listener accepts; a start that ends first fails the test with its own
/// error rather than at the settle deadline.
async fn start(admin_tls: &str, secret_env: &str) -> Running {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("upstream-ca.pem"), up.ca_pem()).unwrap();
    std::fs::write(
        dir.path().join("storage.key"),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    )
    .unwrap();
    let (proxy, admin, metrics) = (free_port(), free_port(), free_port());
    let yaml = format!(
        r#"admin:
  listen: 127.0.0.1:{admin}
  secret_env: {secret_env}
{admin_tls}proxy:
  listen: 127.0.0.1:{proxy}
stats:
  ledger_path: {ledger}
  metrics_listen: 127.0.0.1:{metrics}
storage:
  encryption_key_file: {storage_key}
  path: {db}
upstream:
  resolve_override: {addr}
  extra_ca_pem: {ca}
"#,
        ledger = dir.path().join("ledger.sqlite").display(),
        storage_key = dir.path().join("storage.key").display(),
        db = dir.path().join("phoebis.sqlite").display(),
        addr = up.addr(),
        ca = dir.path().join("upstream-ca.pem").display()
    );
    let config = dir.path().join("config.yaml");
    std::fs::write(&config, yaml).unwrap();
    let loaded = {
        let _env = env_lock();
        std::env::set_var(secret_env, ADMIN_SECRET);
        phoebis_router::config::load(&config).unwrap()
    };
    let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let mut server = tokio::spawn(phoebis_router::app::run(loaded, async {
        let _ = stop_rx.await;
    }));
    tokio::select! {
        ended = &mut server => panic!("the router ended before it listened: {ended:?}"),
        _ = fake_upstream::wait_for_async(SETTLE, || async {
            TcpStream::connect(("127.0.0.1", admin)).await.ok()
        }) => {}
    }
    Running {
        stop,
        server,
        admin,
        config,
        dir,
        _up: up,
    }
}

/// The answer, or the test fails with the client's own line.
fn answered(
    result: Result<phoebis_router::admin_client::Answer, ClientError>,
) -> phoebis_router::admin_client::Answer {
    result.unwrap_or_else(|error| panic!("the call failed: {error}"))
}

/// The refusal, or the test fails naming the status that answered instead.
fn refused(result: Result<phoebis_router::admin_client::Answer, ClientError>) -> ClientError {
    match result {
        Ok(answer) => panic!("the call answered {} {}", answer.status, answer.body),
        Err(error) => error,
    }
}

#[tokio::test]
async fn the_client_calls_a_plain_listener_with_the_bearer() {
    let running = start("", "TEST_CLIENT_PLAIN_ADMIN").await;
    let absent = running.dir.path().join("absent.yaml");
    let common = flags(
        Some(format!("http://127.0.0.1:{}", running.admin)),
        Some("TEST_CLIENT_PLAIN_CALLER"),
    );
    let client = client_with(&common, &absent, "TEST_CLIENT_PLAIN_CALLER", ADMIN_SECRET).unwrap();
    assert!(
        !absent.exists(),
        "with --url and --secret-env the file is not read"
    );
    let answer = answered(client.expect(Method::GET, "/admin/system", None).await);
    assert_eq!(answer.status, 200);
    assert!(answer.body["version"].is_string(), "{}", answer.body);
    running.stop().await;
}

#[tokio::test]
async fn the_client_trusts_a_self_signed_ca() {
    let running = start("  tls: self_signed\n", "TEST_CLIENT_SELF_SIGNED").await;
    let ca = running.dir.path().join("tls").join("ca.pem");
    assert!(ca.is_file(), "the start wrote the CA the client trusts");
    let client = client_with(
        &flags(None, None),
        &running.config,
        "TEST_CLIENT_SELF_SIGNED",
        ADMIN_SECRET,
    )
    .unwrap();
    let base = client.base();
    assert_eq!(
        (base.scheme_str(), base.host(), base.port_u16()),
        (Some("https"), Some("127.0.0.1"), Some(running.admin))
    );
    // No `--ca` was given and the system roots never hold this CA, so the answer proves the file's CA is trusted.
    let answer = answered(client.expect(Method::GET, "/admin/system", None).await);
    assert_eq!(answer.status, 200);
    assert!(answer.body["version"].is_string(), "{}", answer.body);

    // Another CA in its place is not enough: the client verifies the leaf rather than trusting every certificate.
    let mut other = flags(None, None);
    other.ca = Some(running.dir.path().join("upstream-ca.pem"));
    let stranger = client_with(
        &other,
        &running.config,
        "TEST_CLIENT_SELF_SIGNED",
        ADMIN_SECRET,
    )
    .unwrap();
    let error = refused(stranger.expect(Method::GET, "/admin/system", None).await);
    let line = error.to_string();
    assert!(matches!(error, ClientError::Transport { .. }), "{line}");
    assert!(
        line.starts_with(&format!("https://127.0.0.1:{}: ", running.admin))
            && line.contains("verif"),
        "{line}"
    );
    running.stop().await;
}

#[tokio::test]
async fn a_refused_secret_is_one_line_with_no_secret_in_it() {
    const WRONG: &str = "not-the-admin-secret-42";
    let running = start("", "TEST_CLIENT_REFUSED_ADMIN").await;
    let common = flags(
        Some(format!("http://127.0.0.1:{}", running.admin)),
        Some("TEST_CLIENT_REFUSED_CALLER"),
    );
    let client = client_with(
        &common,
        &running.config,
        "TEST_CLIENT_REFUSED_CALLER",
        WRONG,
    )
    .unwrap();
    let error = refused(client.expect(Method::GET, "/admin/system", None).await);
    assert!(matches!(error, ClientError::Refused401), "{error}");
    assert_eq!(error.exit_code(), 1);
    let line = error.to_string();
    assert_eq!(line, "the admin secret was refused");
    let debug = format!("{error:?}");
    for secret in [WRONG, ADMIN_SECRET] {
        assert!(
            !line.contains(secret) && !debug.contains(secret),
            "{line} {debug}"
        );
    }
    running.stop().await;
}

#[tokio::test]
async fn a_port_nothing_listens_on_names_the_url() {
    let port = free_port();
    let dir = tempfile::tempdir().unwrap();
    let common = flags(
        Some(format!("http://127.0.0.1:{port}")),
        Some("TEST_CLIENT_NOBODY"),
    );
    let client = client_with(
        &common,
        &dir.path().join("absent.yaml"),
        "TEST_CLIENT_NOBODY",
        ADMIN_SECRET,
    )
    .unwrap();
    let error = refused(client.expect(Method::GET, "/admin/system", None).await);
    let line = error.to_string();
    assert!(matches!(error, ClientError::Transport { .. }), "{line}");
    assert_eq!(error.exit_code(), 1);
    assert!(
        line.starts_with(&format!("http://127.0.0.1:{port}: "))
            && line.to_lowercase().contains("connection refused"),
        "{line}"
    );
    assert!(!line.contains(ADMIN_SECRET), "{line}");
}

#[tokio::test]
async fn a_mutation_sends_json_and_reads_the_routes_refusal() {
    let running = start("", "TEST_CLIENT_MUTATION").await;
    let common = flags(
        Some(format!("http://127.0.0.1:{}", running.admin)),
        Some("TEST_CLIENT_MUTATION"),
    );
    let client = client_with(
        &common,
        &running.config,
        "TEST_CLIENT_MUTATION",
        ADMIN_SECRET,
    )
    .unwrap();
    let body = serde_json::json!({"id": "x"});
    let error = refused(
        client
            .expect(Method::POST, "/admin/pools", Some(&body))
            .await,
    );
    match &error {
        ClientError::Refused { status, message } => {
            assert_eq!(
                (*status, message.as_str()),
                (400, "id and label are required")
            );
        }
        other => panic!("the route's own refusal was expected: {other}"),
    }
    assert_eq!(error.to_string(), "400 id and label are required");
    running.stop().await;
}

#[tokio::test]
async fn a_stalled_body_ends_the_call_at_the_deadline() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (closed_tx, closed_rx) = tokio::sync::oneshot::channel::<Instant>();
    // Answers the head with a body it never finishes, then reads until the client's side of the socket goes away.
    let peer = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        let mut chunk = [0u8; 1024];
        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
            let read = socket.read(&mut chunk).await.unwrap();
            assert!(read > 0, "the client closed before its head ended");
            head.extend_from_slice(&chunk[..read]);
        }
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 64\r\n\r\n{\"ver",
            )
            .await
            .unwrap();
        loop {
            match socket.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(_) => continue,
            }
        }
        let _ = closed_tx.send(Instant::now());
        String::from_utf8_lossy(&head).into_owned()
    });
    let dir = tempfile::tempdir().unwrap();
    let mut common = flags(
        Some(format!("http://127.0.0.1:{port}")),
        Some("TEST_CLIENT_STALLED"),
    );
    common.timeout = 1;
    let client = client_with(
        &common,
        &dir.path().join("absent.yaml"),
        "TEST_CLIENT_STALLED",
        ADMIN_SECRET,
    )
    .unwrap();

    let called = Instant::now();
    let error = refused(client.expect(Method::GET, "/admin/system", None).await);
    let returned = Instant::now();
    let line = error.to_string();
    assert!(matches!(error, ClientError::Transport { .. }), "{line}");
    assert!(
        line.starts_with(&format!("http://127.0.0.1:{port}: "))
            && line.ends_with("the call did not finish within 1s"),
        "{line}"
    );
    assert!(
        returned - called < Duration::from_secs(3),
        "the call ran {:?}",
        returned - called
    );
    let closed = tokio::time::timeout(Duration::from_secs(1), closed_rx)
        .await
        .expect("the connection closes within a second of the deadline")
        .unwrap();
    assert!(closed >= called);
    let head = peer.await.unwrap();
    let lower = head.to_lowercase();
    assert!(
        lower.starts_with("get /admin/system http/1.1\r\n"),
        "{head}"
    );
    assert!(lower.contains("\r\nconnection: close\r\n"), "{head}");
    assert!(
        head.contains(&format!("Bearer {ADMIN_SECRET}")),
        "the call carries the bearer: {head}"
    );
}
