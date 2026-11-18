//! The feature CA: a router started in-process with a `features` block serves the same CA at the download across a
//! restart, refuses half of one, a key that does not match it and a CA under another subject, and writes its key at
//! 0600. Through the CA's module with a clock the test moves, a certificate is issued again before it ends and holds
//! for a client whose clock runs behind.

use std::cmp::Ordering;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use boring::asn1::Asn1Time;
use boring::x509::X509;
use fake_upstream::{generate_pair, FakeUpstream};
use phoebis_router::config::Config;
use phoebis_router::feature_ca::{
    FeatureCa, FeatureLeaves, CA_CERT_FILE, CA_KEY_FILE, LEAF_RENEW_AHEAD,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

/// `app::run` reads the admin secret from the process environment. The lock covers each test's write of the variable,
/// so no two tests write it at once, and each writes the same value.
static ENV: Mutex<()> = Mutex::new(());

/// How long a boot may take to bind its listeners, generous because it is reached only when the boot never ends.
const SETTLE: Duration = Duration::from_secs(30);

/// A port from a hundred-port slice below the kernel's ephemeral range (32768 upward on Linux), spread by pid as
/// `end_to_end.rs` spreads its own, so no outbound connection takes it between the probe and the router's bind.
fn free_port() -> u16 {
    static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
    let base = 20_000 + (std::process::id() % 100) as u16 * 100;
    loop {
        let offset = NEXT.fetch_add(1, AtomicOrdering::SeqCst);
        assert!(offset < 100, "feature_ca.rs drew past its port slice");
        if std::net::TcpListener::bind(("127.0.0.1", base + offset)).is_ok() {
            return base + offset;
        }
    }
}

/// The router on loopback ports of its own with a `features` block and its data directory at `dir`, and the proxy
/// listener's port. The upstream overrides point at the fake, so nothing leaves the host.
fn configured(dir: &Path, up: &FakeUpstream) -> (Config, u16) {
    let proxy_port = free_port();
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
features:
  listen: 127.0.0.1:{features_port}
proxy:
  listen: 127.0.0.1:{proxy_port}
stats:
  ledger_path: {ledger}
  metrics_listen: 127.0.0.1:{metrics_port}
storage:
  encryption_key_file: {storage_key}
  path: {db}
upstream:
  resolve_override: {addr}
  extra_ca_pem: {ca}
"#,
        admin_port = free_port(),
        features_port = free_port(),
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
    (
        phoebis_router::config::load_unresolved(&path).unwrap(),
        proxy_port,
    )
}

struct Running {
    stop: tokio::sync::oneshot::Sender<()>,
    server: JoinHandle<anyhow::Result<()>>,
}

/// Starts a router on `dir` and waits until its proxy listener accepts, failing with the start's own error where
/// the start ends instead.
async fn start(dir: &Path, up: &FakeUpstream) -> (Running, u16) {
    let (config, proxy_port) = configured(dir, up);
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let mut server = tokio::spawn(phoebis_router::app::run(config, async {
        let _ = stopped.await;
    }));
    let deadline = Instant::now() + SETTLE;
    loop {
        if server.is_finished() {
            let ended = (&mut server).await.unwrap();
            panic!("the router ended before it served: {ended:?}");
        }
        if TcpStream::connect(("127.0.0.1", proxy_port)).await.is_ok() {
            return (Running { stop, server }, proxy_port);
        }
        assert!(Instant::now() < deadline, "the proxy listener never bound");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn stop(running: Running) {
    let _ = running.stop.send(());
    running.server.await.unwrap().unwrap();
}

/// The line of a start that refuses on `dir`.
async fn refused(dir: &Path, up: &FakeUpstream) -> String {
    let (config, _) = configured(dir, up);
    tokio::time::timeout(
        SETTLE,
        phoebis_router::app::run(config, std::future::pending()),
    )
    .await
    .expect("the start served where it had to refuse")
    .unwrap_err()
    .to_string()
}

/// `GET /v0/features/ca.pem` on the proxy listener, with no key: the answer's head and its body.
async fn download(proxy_port: u16) -> (String, Vec<u8>) {
    let mut stream = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
    stream
        .write_all(
            b"GET /v0/features/ca.pem HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n",
        )
        .await
        .unwrap();
    let mut answer = Vec::new();
    tokio::time::timeout(SETTLE, stream.read_to_end(&mut answer))
        .await
        .expect("the answer ended")
        .unwrap();
    let split = answer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("the answer has a head");
    (
        String::from_utf8_lossy(&answer[..split]).into_owned(),
        answer[split + 4..].to_vec(),
    )
}

#[tokio::test]
async fn the_feature_ca_survives_a_restart() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();

    let (first, proxy_port) = start(dir.path(), &up).await;
    let (head, served) = download(proxy_port).await;
    stop(first).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(
        served.starts_with(b"-----BEGIN CERTIFICATE-----"),
        "{}",
        String::from_utf8_lossy(&served)
    );

    let (second, proxy_port) = start(dir.path(), &up).await;
    let (head, again) = download(proxy_port).await;
    stop(second).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(
        String::from_utf8_lossy(&again),
        String::from_utf8_lossy(&served)
    );
    up.shutdown().await;
}

#[tokio::test]
async fn half_a_feature_ca_refuses_the_start() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = (dir.path().join(CA_CERT_FILE), dir.path().join(CA_KEY_FILE));
    let (first, _) = start(dir.path(), &up).await;
    stop(first).await;
    std::fs::remove_file(&key).unwrap();
    let left = std::fs::read(&cert).unwrap();

    let line = refused(dir.path(), &up).await;
    assert!(line.contains(&key.display().to_string()), "{line}");
    assert_eq!(std::fs::read(&cert).unwrap(), left);
    assert!(!key.exists(), "the refused start wrote a key: {line}");
    up.shutdown().await;
}

#[tokio::test]
async fn the_feature_ca_key_file_is_created_at_0600() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let (first, _) = start(dir.path(), &up).await;
    stop(first).await;
    let mode = std::fs::metadata(dir.path().join(CA_KEY_FILE))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "{mode:o}");
    up.shutdown().await;
}

#[test]
fn a_feature_certificate_is_issued_again_before_it_ends() {
    let dir = tempfile::tempdir().unwrap();
    // The CA's own dates are checked against the wall clock as it is read, so the test's clock starts there.
    let now = jiff::Timestamp::now().as_millisecond();
    let (ca, _) = FeatureCa::open(dir.path(), now).unwrap();
    let at = Arc::new(AtomicI64::new(now));
    let clock: phoebis_store::Clock = {
        let at = Arc::clone(&at);
        Arc::new(move || at.load(AtomicOrdering::SeqCst))
    };
    let mut leaves = FeatureLeaves::issue(Arc::new(ca), clock).unwrap();
    let end = |leaves: &FeatureLeaves| leaves.leaves()[0].not_after;
    let first = end(&leaves);
    let renewal = first - LEAF_RENEW_AHEAD.as_millis() as i64;

    at.store(renewal - 1, AtomicOrdering::SeqCst);
    leaves.renew_due().unwrap();
    assert_eq!(end(&leaves), first);

    at.store(renewal, AtomicOrdering::SeqCst);
    leaves.renew_due().unwrap();
    assert!(end(&leaves) > first, "{} against {first}", end(&leaves));
}

#[tokio::test]
async fn a_key_that_does_not_match_the_feature_ca_refuses_the_start() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join(CA_KEY_FILE);
    let (first, _) = start(dir.path(), &up).await;
    stop(first).await;
    std::fs::write(&key, generate_pair(&["localhost"]).key_pem).unwrap();

    let line = refused(dir.path(), &up).await;
    assert!(
        line.starts_with(&format!("features.ca.key_file {}", key.display()))
            && line.contains("does not match"),
        "{line}"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_feature_ca_under_another_subject_refuses_the_start() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let cert = dir.path().join(CA_CERT_FILE);
    // Its key matches and its dates hold, so its subject alone sets it apart from a CA the router made.
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::default();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Constrained(0));
    let mut name = rcgen::DistinguishedName::new();
    name.push(rcgen::DnType::CommonName, "A CA made by hand");
    params.distinguished_name = name;
    std::fs::write(&cert, params.self_signed(&key).unwrap().pem()).unwrap();
    std::fs::write(dir.path().join(CA_KEY_FILE), key.serialize_pem()).unwrap();

    let line = refused(dir.path(), &up).await;
    assert!(
        line.starts_with(&format!("features.ca.cert_file {}", cert.display())),
        "{line}"
    );
    up.shutdown().await;
}

#[test]
fn an_issued_certificate_holds_for_a_client_whose_clock_runs_a_minute_behind() {
    let dir = tempfile::tempdir().unwrap();
    let now = jiff::Timestamp::now().as_millisecond();
    let (ca, _) = FeatureCa::open(dir.path(), now).unwrap();
    let leaf = ca.issue("api.anthropic.com", now).unwrap();
    let cert = X509::from_der(&leaf.cert_der).unwrap();
    let behind = Asn1Time::from_unix(now / 1_000 - 60).unwrap();
    assert_ne!(
        cert.not_before().compare(&behind).unwrap(),
        Ordering::Greater,
        "valid from {}",
        cert.not_before()
    );
}
