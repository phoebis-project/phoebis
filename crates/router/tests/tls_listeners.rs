//! Both public listeners terminate TLS from an operator's certificate pair: the router started in-process with a
//! pair the test generates and trusts from its own clients alone, `phoebis health` probing its proxy listener over
//! TLS, and a pair the start cannot serve refused before any port is taken. Under `tls: self_signed` the start writes
//! a local CA and a leaf per listener under the data directory at their modes, reuses them, and replaces an expired,
//! half or half-replaced pair at the next start.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use boring::pkey::PKey;
use boring::x509::X509;
use fake_upstream::{connector_trusting, generate_pair, FakeUpstream};
use phoebis_router::config::Config;
use phoebis_router::tls::{self, SanName, Validity};
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

/// Both listeners on loopback under the one `tls` block `tls` spells. The registry sits in `dir`, so `dir/tls` is
/// where `self_signed` writes.
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

/// The six files `self_signed` writes under `<data dir>/tls`, in the order the byte comparisons read them.
const MATERIAL: [&str; 6] = [
    "ca.pem",
    "ca.key",
    "proxy.pem",
    "proxy.key",
    "admin.pem",
    "admin.key",
];

const DAY: i64 = 86_400;

/// A router started in-process with `tls: self_signed` on both listeners.
struct Running {
    stop: tokio::sync::oneshot::Sender<()>,
    server: tokio::task::JoinHandle<anyhow::Result<()>>,
    proxy: u16,
    admin: u16,
}

impl Running {
    async fn stop(self) {
        let _ = self.stop.send(());
        self.server.await.unwrap().unwrap();
    }
}

/// Starts the router over `dir` on fresh ports and returns once the proxy listener accepts; a start that ends first
/// fails the test with its own error rather than at the settle deadline.
async fn start_self_signed(dir: &Path, up: &FakeUpstream) -> Running {
    let (proxy, admin) = (free_port(), free_port());
    let config = configured_with(dir, up, (proxy, admin), "  tls: self_signed\n");
    let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let mut server = tokio::spawn(phoebis_router::app::run(config, async {
        let _ = stop_rx.await;
    }));
    tokio::select! {
        ended = &mut server => panic!("the router ended before it listened: {ended:?}"),
        _ = fake_upstream::wait_for_async(SETTLE, || async {
            TcpStream::connect(("127.0.0.1", proxy)).await.ok()
        }) => {}
    }
    Running {
        stop,
        server,
        proxy,
        admin,
    }
}

/// A client that trusts `ca_pem` and nothing else, with `localhost` resolved to the loopback address the listeners
/// bind, so the name it checks against the leaf is the one in the URL.
fn client_trusting(ca_pem: &[u8]) -> reqwest::Client {
    reqwest::Client::builder()
        .tls_built_in_root_certs(false)
        .add_root_certificate(reqwest::Certificate::from_pem(ca_pem).unwrap())
        .resolve("localhost", SocketAddr::from(([127, 0, 0, 1], 0)))
        .build()
        .unwrap()
}

async fn ready(client: &reqwest::Client, proxy: u16) -> u16 {
    client
        .get(format!("https://127.0.0.1:{proxy}/api/ready"))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

fn read(dir: &Path, name: &str) -> Vec<u8> {
    std::fs::read(dir.join("tls").join(name)).unwrap()
}

fn material(dir: &Path) -> Vec<Vec<u8>> {
    MATERIAL.iter().map(|name| read(dir, name)).collect()
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// `tls/` at `0700`, `ca.pem` at `0644`, every other file at `0600`.
fn assert_modes(dir: &Path) {
    let tls = dir.join("tls");
    assert_eq!(mode(&tls), 0o700, "{}", tls.display());
    for name in MATERIAL {
        let expected = if name == "ca.pem" { 0o644 } else { 0o600 };
        assert_eq!(mode(&tls.join(name)), expected, "{name}");
    }
}

/// The CA as the files hold it, the signer a test writes a leaf under.
fn ca_of(dir: &Path) -> tls::Ca {
    tls::Ca {
        cert: X509::from_pem(&read(dir, "ca.pem")).unwrap(),
        key: PKey::private_key_from_pem(&read(dir, "ca.key")).unwrap(),
    }
}

/// Whether `<listener>.pem` carries a signature by the key behind `ca.pem`.
fn verifies_under_ca(dir: &Path, listener: &str) -> bool {
    let ca = X509::from_pem(&read(dir, "ca.pem")).unwrap();
    let leaf = X509::from_pem(&read(dir, &format!("{listener}.pem"))).unwrap();
    leaf.verify(&ca.public_key().unwrap()).unwrap()
}

/// Whether the listener's pair loads as an operator's pair would: both files parse, the key matches, the dates hold.
fn pair_loads(dir: &Path, listener: &'static str) -> bool {
    let tls = dir.join("tls");
    tls::load_pair(
        "proxy.tls",
        &tls.join(format!("{listener}.pem")),
        &tls.join(format!("{listener}.key")),
    )
    .is_ok()
}

/// A window that ended yesterday.
fn expired() -> Validity {
    let now = jiff::Timestamp::now().as_second();
    Validity {
        not_before: now - 2 * DAY,
        not_after: now - DAY,
    }
}

/// The leaf's subject alternative names as text, read through `boring` from the PEM.
fn san_names(pem: &[u8]) -> Vec<String> {
    X509::from_pem(pem)
        .unwrap()
        .subject_alt_names()
        .unwrap()
        .iter()
        .filter_map(|name| {
            name.dnsname().map(str::to_owned).or_else(|| {
                name.ipaddress().map(|octets| match octets.len() {
                    4 => std::net::IpAddr::from(<[u8; 4]>::try_from(octets).unwrap()).to_string(),
                    _ => std::net::IpAddr::from(<[u8; 16]>::try_from(octets).unwrap()).to_string(),
                })
            })
        })
        .collect()
}

#[tokio::test]
async fn self_signed_material_is_generated_at_the_modes_and_trusted_by_the_ca() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let router = start_self_signed(dir.path(), &up).await;

    assert_modes(dir.path());
    let client = client_trusting(&read(dir.path(), "ca.pem"));
    assert_eq!(ready(&client, router.proxy).await, 200);
    let console = client
        .get(format!("https://localhost:{}/", router.admin))
        .send()
        .await
        .unwrap();
    assert_eq!(console.status(), 200);
    let shell = console.text().await.unwrap();
    assert!(shell.contains("<div id=\"root\">"), "{shell}");
    let names = san_names(&read(dir.path(), "proxy.pem"));
    for name in ["localhost", "127.0.0.1", "::1"] {
        assert!(names.iter().any(|named| named == name), "{name}: {names:?}");
    }

    router.stop().await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_second_start_reuses_the_material() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    start_self_signed(dir.path(), &up).await.stop().await;
    let first = material(dir.path());

    start_self_signed(dir.path(), &up).await.stop().await;
    assert!(
        material(dir.path()) == first,
        "a second start rewrote some of {MATERIAL:?}"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn an_expired_leaf_is_regenerated_under_the_same_ca() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    start_self_signed(dir.path(), &up).await.stop().await;
    let ca_pem = read(dir.path(), "ca.pem");
    let tls_dir = dir.path().join("tls");
    std::fs::remove_file(tls_dir.join("proxy.pem")).unwrap();
    std::fs::remove_file(tls_dir.join("proxy.key")).unwrap();
    tls::write_leaf(
        "proxy.tls",
        &tls_dir,
        "proxy",
        &ca_of(dir.path()),
        "localhost",
        &[SanName::Dns("localhost".to_owned())],
        expired(),
    )
    .unwrap();
    let expired_pem = read(dir.path(), "proxy.pem");
    assert!(
        !pair_loads(dir.path(), "proxy"),
        "the written leaf is expired"
    );

    let router = start_self_signed(dir.path(), &up).await;
    assert_eq!(read(dir.path(), "ca.pem"), ca_pem);
    assert_ne!(read(dir.path(), "proxy.pem"), expired_pem);
    assert!(verifies_under_ca(dir.path(), "proxy"));
    assert!(pair_loads(dir.path(), "proxy"));
    assert_eq!(ready(&client_trusting(&ca_pem), router.proxy).await, 200);
    assert_modes(dir.path());

    router.stop().await;
    up.shutdown().await;
}

#[tokio::test]
async fn an_expired_ca_is_replaced_with_new_leaves() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let tls_dir = dir.path().join("tls");
    let ca = tls::write_ca("proxy.tls", &tls_dir, "localhost", expired()).unwrap();
    for (key, listener) in [("proxy.tls", "proxy"), ("admin.tls", "admin")] {
        tls::write_leaf(
            key,
            &tls_dir,
            listener,
            &ca,
            "localhost",
            &[SanName::Dns("localhost".to_owned())],
            Validity::days_from_now(30),
        )
        .unwrap();
    }
    let expired_ca = read(dir.path(), "ca.pem");

    let router = start_self_signed(dir.path(), &up).await;
    let ca_pem = read(dir.path(), "ca.pem");
    assert_ne!(ca_pem, expired_ca);
    assert!(verifies_under_ca(dir.path(), "proxy"));
    assert!(verifies_under_ca(dir.path(), "admin"));
    assert_eq!(ready(&client_trusting(&ca_pem), router.proxy).await, 200);
    assert_modes(dir.path());

    router.stop().await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_half_pair_is_regenerated_whichever_file_is_missing() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    start_self_signed(dir.path(), &up).await.stop().await;

    for missing in ["proxy.pem", "proxy.key", "ca.pem", "ca.key"] {
        let ca_before = read(dir.path(), "ca.pem");
        std::fs::remove_file(dir.path().join("tls").join(missing)).unwrap();

        let router = start_self_signed(dir.path(), &up).await;
        let ca_pem = read(dir.path(), "ca.pem");
        assert!(pair_loads(dir.path(), "proxy"), "{missing}");
        assert!(verifies_under_ca(dir.path(), "proxy"), "{missing}");
        if missing.starts_with("proxy") {
            assert_eq!(ca_pem, ca_before, "{missing}");
        } else {
            assert_ne!(ca_pem, ca_before, "{missing}");
            assert!(verifies_under_ca(dir.path(), "admin"), "{missing}");
            assert_eq!(
                ready(&client_trusting(&ca_pem), router.proxy).await,
                200,
                "{missing}"
            );
        }
        assert_modes(dir.path());
        router.stop().await;
    }
    up.shutdown().await;
}

#[tokio::test]
async fn an_interrupted_replacement_is_finished_at_the_next_start() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    start_self_signed(dir.path(), &up).await.stop().await;
    let tls_dir = dir.path().join("tls");

    // What a stop between the leaf's two renames leaves: the new certificate in place, the old key beside it, and
    // the new key still under its temporary name.
    let scratch = tempfile::tempdir().unwrap();
    tls::write_leaf(
        "proxy.tls",
        scratch.path(),
        "proxy",
        &ca_of(dir.path()),
        "localhost",
        &[SanName::Dns("localhost".to_owned())],
        Validity::days_from_now(30),
    )
    .unwrap();
    std::fs::write(
        tls_dir.join("proxy.pem"),
        std::fs::read(scratch.path().join("proxy.pem")).unwrap(),
    )
    .unwrap();
    let stale = tls_dir.join("proxy.key.tmp");
    std::fs::write(
        &stale,
        std::fs::read(scratch.path().join("proxy.key")).unwrap(),
    )
    .unwrap();
    std::fs::set_permissions(&stale, std::fs::Permissions::from_mode(0o600)).unwrap();
    // A temporary no write at the next start replaces, since the CA beside it holds: only the sweep removes it.
    let stale_ca = tls_dir.join("ca.key.tmp");
    std::fs::write(&stale_ca, b"interrupted").unwrap();
    std::fs::set_permissions(&stale_ca, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(!pair_loads(dir.path(), "proxy"), "the pair is mismatched");
    assert_modes(dir.path());

    let router = start_self_signed(dir.path(), &up).await;
    assert!(pair_loads(dir.path(), "proxy"));
    assert!(verifies_under_ca(dir.path(), "proxy"));
    assert!(!stale.exists(), "the stale temporary is removed");
    assert!(!stale_ca.exists(), "every stale temporary is removed");
    assert_eq!(
        ready(&client_trusting(&read(dir.path(), "ca.pem")), router.proxy).await,
        200
    );
    assert_modes(dir.path());

    router.stop().await;
    up.shutdown().await;
}
