//! Both public listeners terminate TLS from an operator's certificate pair: the router started in-process with a
//! pair the test generates and trusts from its own clients alone, `phoebis health` probing its proxy listener over
//! TLS, and a pair the start cannot serve refused before any port is taken.

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use fake_upstream::{connector_trusting, generate_pair, FakeUpstream};
use phoebis_router::config::Config;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// `load` reads the admin secret from the process environment, so each test holds this across its set-then-load
/// sequence.
static ENV: Mutex<()> = Mutex::new(());

/// How long a boot may take to bind its listeners; generous because it is reached only when the boot never ends.
const SETTLE: Duration = Duration::from_secs(30);

/// A port from a hundred-port slice below the kernel's ephemeral range (32768 upward on Linux), spread by pid as
/// `end_to_end.rs` spreads its own, so no outbound connection takes it between the probe and the router's bind.
fn free_port() -> u16 {
    static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
    let base = 20_000 + (std::process::id() % 100) as u16 * 100;
    loop {
        let offset = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert!(offset < 100, "tls_listeners.rs drew past its port slice");
        if std::net::TcpListener::bind(("127.0.0.1", base + offset)).is_ok() {
            return base + offset;
        }
    }
}

/// Both listeners on loopback under the one pair; the upstream overrides point at the fake, so nothing leaves the
/// host.
fn configured(
    dir: &Path,
    up: &FakeUpstream,
    ports: (u16, u16),
    cert_file: &Path,
    key_file: &Path,
) -> Config {
    let tls = format!(
        "  tls:\n    cert_file: {}\n    key_file: {}\n",
        cert_file.display(),
        key_file.display()
    );
    configured_with(dir, up, ports, &tls)
}

/// Both listeners on loopback under the one `tls` block `tls` spells.
fn configured_with(dir: &Path, up: &FakeUpstream, ports: (u16, u16), tls: &str) -> Config {
    let (proxy_port, admin_port) = ports;
    std::fs::write(dir.join("ca.pem"), up.ca_pem()).unwrap();
    std::fs::write(
        dir.join("storage.key"),
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    )
    .unwrap();
    let yaml = format!(
        r#"admin:
  listen: 127.0.0.1:{admin_port}
  secret_env: TEST_ADMIN_SECRET
{tls}proxy:
  listen: 127.0.0.1:{proxy_port}
{tls}stats:
  ledger_path: {ledger}
  metrics_listen: 127.0.0.1:{metrics_port}
storage:
  encryption_key_file: {storage_key}
  path: {db}
upstream:
  resolve_override: {addr}
  extra_ca_pem: {ca}
"#,
        metrics_port = free_port(),
        ledger = dir.join("ledger.sqlite").display(),
        storage_key = dir.join("storage.key").display(),
        db = dir.join("phoebis.sqlite").display(),
        addr = up.addr(),
        ca = dir.join("ca.pem").display()
    );
    let path = dir.join("config.yaml");
    std::fs::write(&path, yaml).unwrap();
    let _env = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    std::env::set_var("TEST_ADMIN_SECRET", "admin-secret-001");
    phoebis_router::config::load(&path).unwrap()
}

#[tokio::test]
async fn both_listeners_serve_an_operators_pair_and_a_plain_request_gets_no_http_answer() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let pair = generate_pair(&["localhost", "127.0.0.1"]);
    let (cert, key) = pair.write_to(dir.path(), "proxy");
    let (proxy_port, admin_port) = (free_port(), free_port());
    let config = configured(dir.path(), &up, (proxy_port, admin_port), &cert, &key);
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(phoebis_router::app::run(config, async {
        let _ = stop_rx.await;
    }));

    // The listeners bind on the server task's first turns, so the first connect is retried until one accepts.
    let socket = fake_upstream::wait_for_async(SETTLE, || async {
        TcpStream::connect(("127.0.0.1", proxy_port)).await.ok()
    })
    .await;
    let mut ready = tokio::time::timeout(
        SETTLE,
        tokio_boring::connect(connector_trusting(&pair), "localhost", socket),
    )
    .await
    .expect("the handshake finished")
    .unwrap();
    assert_eq!(ready.ssl().selected_alpn_protocol(), Some(&b"http/1.1"[..]));
    ready
        .write_all(b"GET /api/ready HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut answer = Vec::new();
    let _ = tokio::time::timeout(SETTLE, ready.read_to_end(&mut answer)).await;
    let answer = String::from_utf8_lossy(&answer);
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");

    let client = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(pair.ca_pem.as_bytes()).unwrap())
        .build()
        .unwrap();
    let console = client
        .get(format!("https://127.0.0.1:{admin_port}/"))
        .send()
        .await
        .unwrap();
    assert_eq!(console.status(), 200);
    let shell = console.text().await.unwrap();
    assert!(shell.contains("<div id=\"root\">"), "{shell}");
    let login = client
        .post(format!("https://127.0.0.1:{admin_port}/admin/session"))
        .header("content-type", "application/json")
        .body(r#"{"secret":"admin-secret-001"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 204);
    let cookie = login
        .headers()
        .get("set-cookie")
        .expect("a login sets the session cookie")
        .to_str()
        .unwrap();
    // The attributes alone in the message: the value is a live session.
    let attributes = cookie
        .split_once(';')
        .map_or("", |(_, attributes)| attributes);
    assert!(
        cookie.ends_with("; Secure"),
        "the admin listener terminates TLS, so its cookie is Secure:{attributes}"
    );

    let mut plain = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
    plain
        .write_all(b"GET /api/ready HTTP/1.1\r\n\r\n")
        .await
        .unwrap();
    let mut buf = [0u8; 64];
    let read = tokio::time::timeout(Duration::from_secs(3), plain.read(&mut buf))
        .await
        .expect("the listener ended the plain connection at the handshake");
    if let Ok(n) = read {
        assert!(
            !buf[..n].starts_with(b"HTTP/"),
            "a plain request got an HTTP answer: {:?}",
            String::from_utf8_lossy(&buf[..n])
        );
    }

    let _ = stop_tx.send(());
    server.await.unwrap().unwrap();
    up.shutdown().await;
}

#[tokio::test]
async fn the_health_subcommand_speaks_tls_to_a_proxy_listener_serving_a_pair() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = generate_pair(&["localhost", "127.0.0.1"]).write_to(dir.path(), "proxy");
    let (proxy_port, admin_port) = (free_port(), free_port());
    let config = configured(dir.path(), &up, (proxy_port, admin_port), &cert, &key);
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(phoebis_router::app::run(config, async {
        let _ = stop_rx.await;
    }));
    fake_upstream::wait_for_async(SETTLE, || async {
        TcpStream::connect(("127.0.0.1", proxy_port)).await.ok()
    })
    .await;

    // The subcommand is a child process, so the runtime keeps serving the router while it probes.
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_phoebis"))
        .args(["health", "--config"])
        .arg(dir.path().join("config.yaml"))
        .env("TEST_ADMIN_SECRET", "admin-secret-001")
        .env_remove("PHOEBIS_CONFIG")
        .output()
        .await
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        stdout.trim(),
        format!("proxy answers on 127.0.0.1:{proxy_port}")
    );

    let _ = stop_tx.send(());
    server.await.unwrap().unwrap();
    up.shutdown().await;
}

#[tokio::test]
async fn a_key_that_does_not_match_the_certificate_refuses_the_start_naming_the_key_file() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let (cert, _) = generate_pair(&["localhost"]).write_to(dir.path(), "proxy");
    let (_, other_key) = generate_pair(&["localhost"]).write_to(dir.path(), "other");
    let (proxy_port, admin_port) = (free_port(), free_port());
    let config = configured(dir.path(), &up, (proxy_port, admin_port), &cert, &other_key);
    let refused = tokio::time::timeout(
        SETTLE,
        phoebis_router::app::run(config, std::future::pending()),
    )
    .await
    .expect("the start refused rather than serving")
    .unwrap_err();
    let line = refused.to_string();
    assert!(
        line.starts_with("proxy.tls.key_file") && line.contains("does not match"),
        "{line}"
    );
    assert!(
        std::net::TcpListener::bind(("127.0.0.1", proxy_port)).is_ok(),
        "the refused start took no port"
    );
    up.shutdown().await;
}
