//! Both public listeners terminate TLS from an operator's certificate pair: the router started in-process with a
//! pair the test generates and trusts from its own clients alone, `phoebis health` probing its proxy listener over
//! TLS, a pair the start cannot serve refused before any port is taken, and `POST /admin/tls/reload` putting a new pair
//! in front of the next connection or keeping the old one when the new one does not load, with an attention item until
//! a reload lands.

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use boring::x509::X509;
use fake_upstream::{
    connector_trusting, generate_pair, generate_pair_valid_between, FakeUpstream, GeneratedPair,
};
use hyper::Method;
use phoebis_router::admin_client::{AdminClient, Answer, Common};
use phoebis_router::config::Config;
use phoebis_router::status;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_boring::SslStream;

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
    let tls = tls_block(cert_file, key_file);
    configured_with(dir, up, ports, &tls, &tls)
}

/// A listener's `tls` block naming the pair.
fn tls_block(cert_file: &Path, key_file: &Path) -> String {
    format!(
        "  tls:\n    cert_file: {}\n    key_file: {}\n",
        cert_file.display(),
        key_file.display()
    )
}

/// Both listeners on loopback, each under the `tls` block its argument spells, an empty one serving plain HTTP.
fn configured_with(
    dir: &Path,
    up: &FakeUpstream,
    ports: (u16, u16),
    admin_tls: &str,
    proxy_tls: &str,
) -> Config {
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
{admin_tls}proxy:
  listen: 127.0.0.1:{proxy_port}
{proxy_tls}stats:
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
    // Held across the start: a start that bound before it judged the pair would refuse with `binding proxy.listen`
    // rather than with the key's line.
    let _held = std::net::TcpListener::bind(("127.0.0.1", proxy_port)).unwrap();
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
    up.shutdown().await;
}

/// The admin client of `status` and `tls reload` under the bearer, on the plain admin listener.
fn admin_client(admin_port: u16, config: &Path) -> AdminClient {
    let common = Common {
        ca: None,
        json: false,
        secret_env: Some("TEST_ADMIN_SECRET".to_owned()),
        timeout: 10,
        url: Some(format!("http://127.0.0.1:{admin_port}")),
    };
    let _env = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    AdminClient::from_flags(&common, config).unwrap()
}

/// `POST /admin/tls/reload` through the admin client.
async fn reload(admin_port: u16, config: &Path) -> Answer {
    admin_client(admin_port, config)
        .call(Method::POST, "/admin/tls/reload", None)
        .await
        .unwrap_or_else(|error| panic!("the reload call failed: {error}"))
}

/// The items `GET /admin/attention` answers, and the `attention:` block of the `status` screen over the same router:
/// the label's line, then one line per item.
async fn attention(admin_port: u16, config: &Path) -> (Vec<serde_json::Value>, Vec<String>) {
    let client = admin_client(admin_port, config);
    let answer = client
        .call(Method::GET, "/admin/attention", None)
        .await
        .unwrap_or_else(|error| panic!("the attention call failed: {error}"));
    assert_eq!(answer.status, 200, "{}", answer.body);
    let reads = status::read(&client, None, SETTLE).await;
    let listeners = status::Listeners {
        admin: format!("http://127.0.0.1:{admin_port}"),
        certificates: Vec::new(),
        metrics: None,
        proxy: None,
    };
    let screen = status::render(
        &reads,
        &listeners,
        "0.1.0",
        "unknown",
        jiff::Timestamp::now(),
        &jiff::tz::TimeZone::UTC,
        false,
    );
    let block: Vec<String> = screen
        .lines()
        .skip_while(|line| !line.starts_with("  attention"))
        .enumerate()
        .take_while(|(at, line)| *at == 0 || line.starts_with("    "))
        .map(|(_, line)| line.to_owned())
        .collect();
    assert!(!block.is_empty(), "no attention line:\n{screen}");
    (answer.body["items"].as_array().unwrap().clone(), block)
}

/// A connection to the proxy listener whose handshake finished under a client trusting `pair`'s CA alone.
async fn handshaken(proxy_port: u16, pair: &GeneratedPair) -> SslStream<TcpStream> {
    let socket = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
    tokio::time::timeout(
        SETTLE,
        tokio_boring::connect(connector_trusting(pair), "localhost", socket),
    )
    .await
    .expect("the handshake finished")
    .unwrap()
}

/// The `not_after` of the leaf the listener served on `stream`, as BoringSSL prints it.
fn served_not_after(stream: &SslStream<TcpStream>) -> String {
    stream
        .ssl()
        .peer_certificate()
        .expect("the listener served a leaf")
        .not_after()
        .to_string()
}

/// The `not_after` of `pair`'s leaf, as BoringSSL prints it.
fn not_after(pair: &GeneratedPair) -> String {
    X509::from_pem(pair.cert_pem.as_bytes())
        .unwrap()
        .not_after()
        .to_string()
}

/// `GET /api/ready` on `stream`, answered `200`.
async fn assert_ready(stream: &mut SslStream<TcpStream>) {
    stream
        .write_all(b"GET /api/ready HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut answer = Vec::new();
    let _ = tokio::time::timeout(SETTLE, stream.read_to_end(&mut answer)).await;
    let answer = String::from_utf8_lossy(&answer);
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
}

#[tokio::test]
async fn a_reload_with_a_new_pair_at_the_same_paths_serves_the_new_leaf_to_the_next_connection() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let hosts = ["localhost", "127.0.0.1"];
    let old = generate_pair_valid_between(&hosts, (2020, 1, 1), (2098, 1, 1));
    let new = generate_pair_valid_between(&hosts, (2020, 1, 1), (2099, 1, 1));
    let (cert, key) = old.write_to(dir.path(), "proxy");
    let (proxy_port, admin_port) = (free_port(), free_port());
    let config = configured_with(
        dir.path(),
        &up,
        (proxy_port, admin_port),
        "",
        &tls_block(&cert, &key),
    );
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(phoebis_router::app::run(config, async {
        let _ = stop_rx.await;
    }));
    fake_upstream::wait_for_async(SETTLE, || async {
        TcpStream::connect(("127.0.0.1", admin_port)).await.ok()
    })
    .await;

    let mut held = handshaken(proxy_port, &old).await;
    assert_eq!(served_not_after(&held), not_after(&old));
    new.write_to(dir.path(), "proxy");
    let answer = reload(admin_port, &dir.path().join("config.yaml")).await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    let listeners = answer.body["listeners"].as_array().unwrap();
    assert_eq!(listeners.len(), 2, "{}", answer.body);
    let proxy = &listeners[0];
    assert_eq!(
        (
            &proxy["listener"],
            &proxy["serving"],
            &proxy["reloaded"],
            &proxy["cert_file"],
            &proxy["expires"],
        ),
        (
            &json!("proxy"),
            &json!("tls"),
            &json!(true),
            &json!(cert.display().to_string()),
            &json!("2099-01-01T00:00:00Z"),
        ),
        "{}",
        answer.body
    );
    assert!(
        proxy["days_left"].as_i64().is_some_and(|days| days > 0),
        "{proxy}"
    );
    assert_eq!(
        listeners[1],
        json!({"listener": "admin", "serving": "plain"})
    );

    let mut next = handshaken(proxy_port, &new).await;
    assert_eq!(served_not_after(&next), not_after(&new));
    assert_ready(&mut next).await;
    // The connection handshaken before the reload finishes its request on the leaf it took.
    assert_ready(&mut held).await;

    let _ = stop_tx.send(());
    server.await.unwrap().unwrap();
    up.shutdown().await;
}

#[tokio::test]
async fn a_reload_with_a_mismatched_key_keeps_the_old_pair_serving() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let old = generate_pair(&["localhost", "127.0.0.1"]);
    let (cert, key) = old.write_to(dir.path(), "proxy");
    let (proxy_port, admin_port) = (free_port(), free_port());
    let config = configured_with(
        dir.path(),
        &up,
        (proxy_port, admin_port),
        "",
        &tls_block(&cert, &key),
    );
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(phoebis_router::app::run(config, async {
        let _ = stop_rx.await;
    }));
    fake_upstream::wait_for_async(SETTLE, || async {
        TcpStream::connect(("127.0.0.1", admin_port)).await.ok()
    })
    .await;

    std::fs::write(&key, generate_pair(&["localhost"]).key_pem).unwrap();
    let answer = reload(admin_port, &dir.path().join("config.yaml")).await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    let listeners = answer.body["listeners"].as_array().unwrap();
    assert_eq!(listeners.len(), 2, "{}", answer.body);
    let proxy = &listeners[0];
    assert_eq!(
        (&proxy["listener"], &proxy["serving"], &proxy["reloaded"]),
        (&json!("proxy"), &json!("tls"), &json!(false)),
        "{proxy}"
    );
    let error = proxy["error"].as_str().unwrap_or_default();
    assert!(
        error.starts_with("proxy.tls.key_file") && error.contains("does not match"),
        "{proxy}"
    );
    assert_eq!(
        listeners[1],
        json!({"listener": "admin", "serving": "plain"})
    );
    let (items, block) = attention(admin_port, &dir.path().join("config.yaml")).await;
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(
        (
            &items[0]["kind"],
            &items[0]["subject"],
            &items[0]["link"],
            &items[0]["pools"],
            &items[0]["detail"]["error"],
        ),
        (
            &json!("certificate_reload_failed"),
            &json!({"type": "router", "id": "proxy.tls", "label": "proxy.tls"}),
            &json!(null),
            &json!([]),
            &json!(error),
        ),
        "the refused reload raises the item on the listener's subject"
    );
    assert!(
        block.len() == 2
            && block[0] == "  attention:"
            && block[1].starts_with("    certificate_reload_failed proxy.tls since "),
        "{block:?}"
    );

    let mut next = handshaken(proxy_port, &old).await;
    assert_eq!(served_not_after(&next), not_after(&old));
    assert_ready(&mut next).await;

    std::fs::write(&key, &old.key_pem).unwrap();
    let answer = reload(admin_port, &dir.path().join("config.yaml")).await;
    assert_eq!(
        answer.body["listeners"][0]["reloaded"],
        json!(true),
        "{}",
        answer.body
    );
    let (items, block) = attention(admin_port, &dir.path().join("config.yaml")).await;
    assert_eq!(
        (items.len(), block),
        (0, vec!["  attention: none".to_owned()]),
        "a reload that lands clears the item: {items:?}"
    );

    let _ = stop_tx.send(());
    server.await.unwrap().unwrap();
    up.shutdown().await;
}
