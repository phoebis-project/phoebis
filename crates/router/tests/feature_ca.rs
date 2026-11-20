//! The feature CA: a router started in-process with a `features` block serves the same CA at the download across a
//! restart, refuses half of one, a key that does not match it, a file with a second certificate, a CA without a key
//! identifier, a CA under another subject and a CA without the name constraint, and writes its key at 0600.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use boring::asn1::{Asn1Time, Asn1Type};
use boring::bn::BigNum;
use boring::hash::MessageDigest;
use boring::nid::Nid;
use boring::pkey::PKey;
use boring::x509::extension::BasicConstraints;
use boring::x509::{X509Builder, X509NameBuilder};
use fake_upstream::{generate_pair, FakeUpstream};
use phoebis_router::config::Config;
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
    let (cert, key) = (
        dir.path().join("feature-ca.pem"),
        dir.path().join("feature-ca-key.pem"),
    );
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
    let mode = std::fs::metadata(dir.path().join("feature-ca-key.pem"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "{mode:o}");
    up.shutdown().await;
}

#[tokio::test]
async fn a_key_that_does_not_match_the_feature_ca_refuses_the_start() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("feature-ca-key.pem");
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
    let cert = dir.path().join("feature-ca.pem");
    // Its key matches, its dates hold and it carries a key identifier. It lacks the name constraint as well, which the
    // read compares behind the subject, so the subject alone decides the line.
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::default();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Constrained(0));
    let mut name = rcgen::DistinguishedName::new();
    name.push(rcgen::DnType::CommonName, "A CA made by hand");
    params.distinguished_name = name;
    std::fs::write(&cert, params.self_signed(&key).unwrap().pem()).unwrap();
    std::fs::write(dir.path().join("feature-ca-key.pem"), key.serialize_pem()).unwrap();

    let line = refused(dir.path(), &up).await;
    assert_eq!(
        line,
        format!(
            "features.ca.cert_file {}: holds no CA this router made",
            cert.display()
        )
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_feature_ca_without_the_name_constraint_refuses_the_start() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let cert = dir.path().join("feature-ca.pem");
    // Its key matches, its dates hold, it carries a key identifier and its subject is the router's, so the missing
    // constraint alone sets it apart from a CA the router made.
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::default();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Constrained(0));
    let mut name = rcgen::DistinguishedName::new();
    name.push(rcgen::DnType::CommonName, "Phoebis feature CA");
    params.distinguished_name = name;
    std::fs::write(&cert, params.self_signed(&key).unwrap().pem()).unwrap();
    std::fs::write(dir.path().join("feature-ca-key.pem"), key.serialize_pem()).unwrap();

    let line = refused(dir.path(), &up).await;
    assert_eq!(
        line,
        format!(
            "features.ca.cert_file {}: holds a CA without the name constraint this router writes",
            cert.display()
        )
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_feature_ca_without_a_key_identifier_refuses_the_start() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let cert = dir.path().join("feature-ca.pem");
    // rcgen writes a key identifier into each CA it signs, so this one is built through BoringSSL. Its key matches,
    // its dates hold and its subject is the router's, so a read that took a derived identifier in place of the missing
    // one would reach the name constraint and answer that line.
    let key = rcgen::KeyPair::generate().unwrap();
    let signing = PKey::private_key_from_pem(key.serialize_pem().as_bytes()).unwrap();
    let mut subject = X509NameBuilder::new().unwrap();
    subject
        .append_entry_by_nid_with_type(Nid::COMMONNAME, "Phoebis feature CA", Asn1Type::UTF8STRING)
        .unwrap();
    let subject = subject.build();
    let mut constraint = BasicConstraints::new();
    let constraint = constraint.critical().ca().pathlen(0).build().unwrap();
    let mut builder = X509Builder::new().unwrap();
    builder.set_version(2).unwrap();
    builder
        .set_serial_number(&BigNum::from_u32(7).unwrap().to_asn1_integer().unwrap())
        .unwrap();
    builder.set_subject_name(&subject).unwrap();
    builder.set_issuer_name(&subject).unwrap();
    builder.set_pubkey(&signing).unwrap();
    builder
        .set_not_before(&Asn1Time::days_from_now(0).unwrap())
        .unwrap();
    builder
        .set_not_after(&Asn1Time::days_from_now(1).unwrap())
        .unwrap();
    builder.append_extension(&constraint).unwrap();
    builder.sign(&signing, MessageDigest::sha256()).unwrap();
    std::fs::write(&cert, builder.build().to_pem().unwrap()).unwrap();
    std::fs::write(dir.path().join("feature-ca-key.pem"), key.serialize_pem()).unwrap();

    let line = refused(dir.path(), &up).await;
    assert_eq!(
        line,
        format!(
            "features.ca.cert_file {}: holds no CA this router made",
            cert.display()
        )
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_feature_ca_file_with_a_second_certificate_refuses_the_start() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let cert = dir.path().join("feature-ca.pem");
    let (first, _) = start(dir.path(), &up).await;
    stop(first).await;
    let mut params = rcgen::CertificateParams::default();
    let mut name = rcgen::DistinguishedName::new();
    name.push(rcgen::DnType::CommonName, "A certificate nobody checked");
    params.distinguished_name = name;
    let second = params
        .self_signed(&rcgen::KeyPair::generate().unwrap())
        .unwrap();
    let mut file = std::fs::read_to_string(&cert).unwrap();
    file.push_str(&second.pem());
    std::fs::write(&cert, file).unwrap();

    let line = refused(dir.path(), &up).await;
    assert_eq!(
        line,
        format!(
            "features.ca.cert_file {}: holds more than one certificate",
            cert.display()
        )
    );
    up.shutdown().await;
}
