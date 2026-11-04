//! The admin client of `status` and `tls reload` against routers started in-process: the bearer on a plain listener,
//! the CA `--ca` names trusted and no other, the system roots left unconsulted once `--ca` names a bundle, a refused
//! secret, a port nothing listens on, a route's own refusal, and a body that stalls past the deadline.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use fake_upstream::{generate_pair, FakeUpstream, GeneratedPair};
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
    /// The CA that signed the admin listener's pair, written by `start` only when it was given one.
    admin_ca: Option<PathBuf>,
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

/// Starts the router with `admin_pair`, when given, as `admin.tls` and its CA written to `admin-ca.pem`, the admin
/// secret read from `secret_env`, and returns once the admin listener accepts; a start that ends first fails the
/// test with its own error rather than at the settle deadline.
async fn start(admin_pair: Option<&GeneratedPair>, secret_env: &str) -> Running {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("upstream-ca.pem"), up.ca_pem()).unwrap();
    let admin_ca = admin_pair.map(|pair| {
        let path = dir.path().join("admin-ca.pem");
        std::fs::write(&path, &pair.ca_pem).unwrap();
        path
    });
    let admin_tls = match admin_pair {
        Some(pair) => {
            let (cert, key) = pair.write_to(dir.path(), "admin");
            format!(
                "  tls:\n    cert_file: {}\n    key_file: {}\n",
                cert.display(),
                key.display()
            )
        }
        None => String::new(),
    };
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
        phoebis_router::config::load_unresolved(&config).unwrap()
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
        admin_ca,
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
    let running = start(None, "TEST_CLIENT_PLAIN_ADMIN").await;
    let absent = running.dir.path().join("absent.yaml");
    let common = flags(
        Some(format!("http://127.0.0.1:{}", running.admin)),
        Some("TEST_CLIENT_PLAIN_CALLER"),
    );
    let client = client_with(&common, &absent, "TEST_CLIENT_PLAIN_CALLER", ADMIN_SECRET).unwrap();
    // The same URL without `--secret-env` needs the file and is refused on its absent path, so the client above was
    // built without reading it.
    let url_alone = flags(common.url.clone(), None);
    let Err(refusal) = client_with(
        &url_alone,
        &absent,
        "TEST_CLIENT_PLAIN_CALLER",
        ADMIN_SECRET,
    ) else {
        panic!("a client was built from --url alone with no configuration file");
    };
    assert!(matches!(refusal, ClientError::Config(_)), "{refusal}");
    assert_eq!(
        refusal.to_string(),
        format!("cannot read {}", absent.display())
    );
    let answer = answered(client.expect(Method::GET, "/admin/system", None).await);
    assert_eq!(answer.status, 200);
    assert!(answer.body["version"].is_string(), "{}", answer.body);
    running.stop().await;
}

/// Every client of this test is built and called under the lock: a call over TLS builds its connector through
/// `SslConnector::builder`, which reads `SSL_CERT_FILE` through C `getenv`, so a test that sets the variable meanwhile
/// would change the roots the call reads. Each test runs on its own current-thread runtime, so no task of this test
/// waits on the lock it holds.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn the_client_trusts_the_ca_named_by_ca_and_no_other() {
    let pair = generate_pair(&["localhost", "127.0.0.1"]);
    let running = start(Some(&pair), "TEST_CLIENT_NAMED_CA").await;
    let admin_ca = running
        .admin_ca
        .clone()
        .expect("the router was started with an admin pair");
    // Called with the lock held, which `client_with` would take a second time.
    let client = |ca: Option<PathBuf>| {
        let mut common = flags(None, None);
        common.ca = ca;
        std::env::set_var("TEST_CLIENT_NAMED_CA", ADMIN_SECRET);
        AdminClient::from_flags(&common, &running.config).unwrap()
    };
    let env = env_lock();
    let named = client(Some(admin_ca));
    let base = named.base();
    assert_eq!(
        (base.scheme_str(), base.host(), base.port_u16()),
        (Some("https"), Some("127.0.0.1"), Some(running.admin))
    );
    let answer = answered(named.expect(Method::GET, "/admin/system", None).await);
    drop(env);
    assert_eq!(answer.status, 200);
    assert!(answer.body["version"].is_string(), "{}", answer.body);

    // The system roots never hold the test's CA and the upstream's CA did not sign the leaf, so neither call is
    // admitted: the client verifies the leaf rather than trusting every certificate.
    for ca in [None, Some(running.dir.path().join("upstream-ca.pem"))] {
        let env = env_lock();
        let error = refused(
            client(ca.clone())
                .expect(Method::GET, "/admin/system", None)
                .await,
        );
        drop(env);
        let line = error.to_string();
        assert!(
            matches!(error, ClientError::Transport { .. }),
            "{ca:?}: {line}"
        );
        assert!(
            line.starts_with(&format!(
                "the admin listener did not answer on https://127.0.0.1:{}: ",
                running.admin
            )) && line.contains("verif"),
            "{ca:?}: {line}"
        );
    }
    running.stop().await;
}

/// `SSL_CERT_FILE` is read when a call builds its connector, so the lock spans both calls; each test runs on its own
/// current-thread runtime, so no task of this test waits on the lock it holds.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn a_named_ca_replaces_the_system_roots() {
    let running = start(
        Some(&generate_pair(&["localhost", "127.0.0.1"])),
        "TEST_CLIENT_PINNED",
    )
    .await;
    let url = format!("https://127.0.0.1:{}", running.admin);
    let absent = running.dir.path().join("absent.yaml");
    let admin_ca = running
        .admin_ca
        .clone()
        .expect("the router was started with an admin pair");
    let env = env_lock();
    let previous = std::env::var_os("SSL_CERT_FILE");
    // The default roots become the admin pair's CA, so a call that still consulted them would be admitted.
    std::env::set_var("SSL_CERT_FILE", &admin_ca);
    std::env::set_var("TEST_CLIENT_PINNED", ADMIN_SECRET);
    let roots = AdminClient::from_flags(
        &flags(Some(url.clone()), Some("TEST_CLIENT_PINNED")),
        &absent,
    )
    .unwrap();
    let through_roots = roots.expect(Method::GET, "/admin/system", None).await;
    let mut pin = flags(Some(url.clone()), Some("TEST_CLIENT_PINNED"));
    pin.ca = Some(running.dir.path().join("upstream-ca.pem"));
    let pinned = AdminClient::from_flags(&pin, &absent).unwrap();
    let through_pin = pinned.expect(Method::GET, "/admin/system", None).await;
    match previous {
        Some(value) => std::env::set_var("SSL_CERT_FILE", value),
        None => std::env::remove_var("SSL_CERT_FILE"),
    }
    drop(env);

    assert_eq!(
        answered(through_roots).status,
        200,
        "with no --ca the default roots, here the pair's CA, are the trust"
    );
    let error = refused(through_pin);
    let line = error.to_string();
    assert!(matches!(error, ClientError::Transport { .. }), "{line}");
    assert!(
        line.starts_with(&format!("the admin listener did not answer on {url}: "))
            && line.contains("verif"),
        "{line}"
    );
    running.stop().await;
}

#[tokio::test]
async fn a_refused_secret_is_one_line_with_no_secret_in_it() {
    const WRONG: &str = "not-the-admin-secret-42";
    let running = start(None, "TEST_CLIENT_REFUSED_ADMIN").await;
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
    assert!(
        line.starts_with(&format!(
            "the admin listener did not answer on http://127.0.0.1:{port}: "
        )) && line.to_lowercase().contains("connection refused"),
        "{line}"
    );
    assert!(!line.contains(ADMIN_SECRET), "{line}");
}

#[tokio::test]
async fn a_mutation_sends_json_and_reads_the_routes_refusal() {
    let running = start(None, "TEST_CLIENT_MUTATION").await;
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
        line.starts_with(&format!(
            "the admin listener did not answer on http://127.0.0.1:{port}: "
        )) && line.ends_with("the call did not finish within 1s"),
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
