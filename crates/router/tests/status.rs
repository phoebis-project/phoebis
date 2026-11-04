//! `phoebis status` from the outside: the binary against a router started in-process on loopback, the screen and its
//! JSON twin, a refused secret, a router that is down, and the file skipped under `--url` with `--secret-env`; and the
//! reads against a listener that never answers, on the paused test clock.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Mutex;
use std::time::Duration;

use fake_upstream::FakeUpstream;
use phoebis_router::admin_client::{AdminClient, Common};
use phoebis_router::status;
use tokio::net::TcpStream;

/// `load` and `from_flags` read the admin secret from the process environment, so each test holds this across its
/// set-then-read sequence.
static ENV: Mutex<()> = Mutex::new(());

/// How long a boot may take to answer ready; generous because it is reached only when the boot never ends.
const SETTLE: Duration = Duration::from_secs(30);

const ADMIN_SECRET: &str = "admin-secret-status-001";

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The line under the header, which the in-process router's answer and the binary's own fallback both fill from this
/// build.
fn build_line() -> String {
    format!("  build {}", phoebis_router::BUILD_COMMIT)
}

/// The five reads as the screen and the JSON's `errors` name them.
const READS: [&str; 5] = ["system", "attention", "pools", "accounts", "sessions"];

/// A port from a hundred-port slice below the kernel's ephemeral range (32768 upward on Linux), spread by pid as
/// `end_to_end.rs` spreads its own, so no outbound connection takes it between the probe and the router's bind.
fn free_port() -> u16 {
    static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
    let base = 20_000 + (std::process::id() % 100) as u16 * 100;
    loop {
        let offset = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert!(offset < 100, "status.rs drew past its port slice");
        if std::net::TcpListener::bind(("127.0.0.1", base + offset)).is_ok() {
            return base + offset;
        }
    }
}

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The three listeners' ports.
#[derive(Clone, Copy)]
struct Ports {
    admin: u16,
    metrics: u16,
    proxy: u16,
}

impl Ports {
    fn fresh() -> Ports {
        Ports {
            admin: free_port(),
            metrics: free_port(),
            proxy: free_port(),
        }
    }
}

/// The bootstrap file for a router on `ports`, its upstream at `upstream` (an address nothing needs to answer on
/// when no router starts).
fn write_config(dir: &Path, secret_env: &str, ports: Ports, upstream: &str) -> PathBuf {
    let key = dir.join("storage.key");
    std::fs::write(
        &key,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    )
    .unwrap();
    let yaml = format!(
        r#"admin:
  listen: 127.0.0.1:{admin}
  secret_env: {secret_env}
proxy:
  listen: 127.0.0.1:{proxy}
stats:
  ledger_path: {ledger}
  metrics_listen: 127.0.0.1:{metrics}
storage:
  encryption_key_file: {key}
  path: {db}
upstream:
  resolve_override: {upstream}
  extra_ca_pem: {ca}
"#,
        admin = ports.admin,
        proxy = ports.proxy,
        metrics = ports.metrics,
        ledger = dir.join("ledger.sqlite").display(),
        key = key.display(),
        db = dir.join("phoebis.sqlite").display(),
        ca = dir.join("upstream-ca.pem").display()
    );
    let config = dir.join("config.yaml");
    std::fs::write(&config, yaml).unwrap();
    config
}

/// A router started in-process on fresh loopback ports, its registry empty, its upstream the fake.
struct Running {
    stop: tokio::sync::oneshot::Sender<()>,
    server: tokio::task::JoinHandle<anyhow::Result<()>>,
    ports: Ports,
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

/// Starts the router with the admin secret under `secret_env` and returns once the admin listener accepts and the
/// proxy listener answers `/api/ready` with 200, so the binary's probe meets a ready router; a start that ends first
/// fails the test with its own error.
async fn start(secret_env: &str) -> Running {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("upstream-ca.pem"), up.ca_pem()).unwrap();
    let ports = Ports::fresh();
    let config = write_config(dir.path(), secret_env, ports, &up.addr().to_string());
    let loaded = {
        let _env = env_lock();
        std::env::set_var(secret_env, ADMIN_SECRET);
        phoebis_router::config::load_unresolved(&config).unwrap()
    };
    let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let mut server = tokio::spawn(phoebis_router::app::run(loaded, async {
        let _ = stop_rx.await;
    }));
    let proxy = std::net::SocketAddr::from(([127, 0, 0, 1], ports.proxy));
    tokio::select! {
        ended = &mut server => panic!("the router ended before it listened: {ended:?}"),
        _ = fake_upstream::wait_for_async(SETTLE, || async move {
            TcpStream::connect(("127.0.0.1", ports.admin)).await.ok()?;
            tokio::task::spawn_blocking(move || {
                phoebis_router::health::probe(proxy, false, Duration::from_secs(1))
            })
            .await
            .ok()?
            .ok()
        }) => {}
    }
    Running {
        stop,
        server,
        ports,
        config,
        dir,
        _up: up,
    }
}

/// `phoebis status` with `args`, the variable `name` holding `value` in the child's environment alone.
async fn status(args: &[&str], config: &Path, name: &str, value: &str) -> Output {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_phoebis"))
        .env_remove("PHOEBIS_CONFIG")
        .env(name, value)
        .arg("status")
        .arg("--config")
        .arg(config)
        .args(args)
        .output()
        .await
        .unwrap()
}

fn text(output: &Output) -> (String, String) {
    (
        String::from_utf8(output.stdout.clone()).unwrap(),
        String::from_utf8(output.stderr.clone()).unwrap(),
    )
}

/// Every byte of both streams, checked for each secret.
fn assert_no_secret(output: &Output, secrets: &[&str]) {
    let (stdout, stderr) = text(output);
    for secret in secrets {
        assert!(
            !stdout.contains(secret) && !stderr.contains(secret),
            "a secret was printed:\n{stdout}\n{stderr}"
        );
    }
}

#[tokio::test]
async fn status_prints_one_screen_of_a_ready_router_and_the_same_reads_as_json() {
    let running = start("TEST_STATUS_READY").await;
    let ports = running.ports;

    let screen = status(&[], &running.config, "TEST_STATUS_READY", ADMIN_SECRET).await;
    let (stdout, stderr) = text(&screen);
    assert_eq!(screen.status.code(), Some(0), "{stdout}\n{stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(
        lines[0].starts_with(&format!("phoebis {VERSION}, up ")) && lines[0].ends_with(", ready"),
        "{stdout}"
    );
    assert_eq!(lines[1], build_line(), "{stdout}");
    // The listeners wrap between entries rather than past 80 columns, so the third may sit on the line below.
    assert!(
        lines[2].starts_with(&format!("  proxy http://127.0.0.1:{}", ports.proxy)),
        "{stdout}"
    );
    let listeners = format!("{} {}", lines[2], lines[3]);
    for named in [
        format!("admin http://127.0.0.1:{}", ports.admin),
        format!("metrics http://127.0.0.1:{}", ports.metrics),
    ] {
        assert!(listeners.contains(&named), "{named}: {stdout}");
    }
    assert!(lines.contains(&"pools: none"), "{stdout}");
    assert!(lines.contains(&"pins: none"), "{stdout}");
    for line in &lines {
        assert!(line.chars().count() <= status::WIDTH, "{line}");
    }
    assert_no_secret(&screen, &[ADMIN_SECRET]);

    let json = status(
        &["--json"],
        &running.config,
        "TEST_STATUS_READY",
        ADMIN_SECRET,
    )
    .await;
    let (stdout, stderr) = text(&json);
    assert_eq!(json.status.code(), Some(0), "{stdout}\n{stderr}");
    let answer: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(answer["health"]["ready"], true, "{answer}");
    assert_eq!(
        answer["health"]["addr"],
        format!("127.0.0.1:{}", ports.proxy),
        "{answer}"
    );
    for member in READS {
        assert!(answer[member].is_object(), "{member}: {answer}");
    }
    assert_eq!(answer["system"]["ready"], true, "{answer}");
    assert_eq!(answer["errors"], serde_json::json!({}), "{answer}");
    assert_no_secret(&json, &[ADMIN_SECRET]);
    running.stop().await;
}

#[tokio::test]
async fn a_refused_secret_exits_one_and_neither_stream_carries_a_secret() {
    const WRONG: &str = "not-the-admin-secret-status-42";
    let running = start("TEST_STATUS_REFUSED").await;
    let output = status(&[], &running.config, "TEST_STATUS_REFUSED", WRONG).await;
    let (stdout, stderr) = text(&output);
    assert_eq!(output.status.code(), Some(1), "{stdout}\n{stderr}");
    assert!(
        format!("{stdout}{stderr}").contains("the admin secret was refused"),
        "{stdout}\n{stderr}"
    );
    assert_no_secret(&output, &[ADMIN_SECRET, WRONG]);
    running.stop().await;
}

/// The admin listener counts a refused bearer as a failed login and refuses every login after five in a minute, so
/// a run with a stale secret must spend one failure and leave the corrected run, and the console, their logins.
#[tokio::test]
async fn a_refused_secret_spends_one_failed_login_so_the_corrected_run_reads_ready() {
    const WRONG: &str = "not-the-admin-secret-status-43";
    let running = start("TEST_STATUS_THROTTLE").await;
    let refused = status(&[], &running.config, "TEST_STATUS_THROTTLE", WRONG).await;
    let (stdout, stderr) = text(&refused);
    assert_eq!(refused.status.code(), Some(1), "{stdout}\n{stderr}");
    assert_eq!(
        stdout.matches("the admin secret was refused").count(),
        5,
        "every section names the refusal: {stdout}"
    );

    let corrected = status(&[], &running.config, "TEST_STATUS_THROTTLE", ADMIN_SECRET).await;
    let (stdout, stderr) = text(&corrected);
    assert_eq!(corrected.status.code(), Some(0), "{stdout}\n{stderr}");
    assert!(
        stdout
            .lines()
            .next()
            .is_some_and(|line| line.ends_with(", ready")),
        "{stdout}"
    );
    running.stop().await;
}

#[tokio::test]
async fn a_router_that_is_down_prints_the_down_screen_and_exits_one() {
    let dir = tempfile::tempdir().unwrap();
    let ports = Ports::fresh();
    let upstream = format!("127.0.0.1:{}", free_port());
    let config = write_config(dir.path(), "TEST_STATUS_DOWN", ports, &upstream);
    let output = status(&[], &config, "TEST_STATUS_DOWN", ADMIN_SECRET).await;
    let (stdout, stderr) = text(&output);
    assert_eq!(output.status.code(), Some(1), "{stdout}\n{stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(
        lines[0].starts_with(&format!(
            "phoebis {VERSION}, proxy down: proxy does not answer on 127.0.0.1:{}: ",
            ports.proxy
        )),
        "{stdout}"
    );
    assert!(
        lines.contains(&build_line().as_str()),
        "a router that is down leaves the binary's own build under the header: {stdout}"
    );
    for read in READS {
        let failed = format!(
            "{read}: the admin listener did not answer on http://127.0.0.1:{}:",
            ports.admin
        );
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.trim_start().starts_with(&failed))
                .count(),
            1,
            "{read}: {stdout}"
        );
    }
    assert_no_secret(&output, &[ADMIN_SECRET]);
}

#[tokio::test]
async fn the_url_and_the_secret_variable_stand_in_for_the_file_and_skip_the_probe() {
    let running = start("TEST_STATUS_REMOTE_ADMIN").await;
    let absent = running.dir.path().join("absent.yaml");
    let url = format!("http://127.0.0.1:{}", running.ports.admin);
    let flags = ["--url", url.as_str(), "--secret-env", "TEST_STATUS_REMOTE"];

    let screen = status(&flags, &absent, "TEST_STATUS_REMOTE", ADMIN_SECRET).await;
    let (stdout, stderr) = text(&screen);
    assert_eq!(
        screen.status.code(),
        Some(0),
        "a skipped probe is not a failed one: {stdout}\n{stderr}"
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(lines[0].ends_with(", ready"), "{stdout}");
    assert_eq!(lines[1], build_line(), "{stdout}");
    assert_eq!(lines[2], format!("  admin {url}"), "{stdout}");
    assert_eq!(
        lines[3].trim_start(),
        "proxy: not probed, no configuration file names the listener",
        "{stdout}"
    );

    let mut json_flags = flags.to_vec();
    json_flags.push("--json");
    let json = status(&json_flags, &absent, "TEST_STATUS_REMOTE", ADMIN_SECRET).await;
    let (stdout, stderr) = text(&json);
    assert_eq!(json.status.code(), Some(0), "{stdout}\n{stderr}");
    let answer: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(answer["health"].is_null(), "{answer}");
    assert_eq!(answer["errors"], serde_json::json!({}), "{answer}");
    running.stop().await;
}

/// The paused clock advances to the next deadline whenever the runtime waits on nothing else, so the five deadlines
/// pass at once in wall time while the test clock reads their sum.
#[tokio::test(start_paused = true)]
async fn a_route_that_never_answers_ends_each_read_at_the_deadline() {
    // Bound and never accepted: the kernel completes the connection and takes the request, and no answer comes.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = silent.local_addr().unwrap().port();
    let client = {
        let _env = env_lock();
        std::env::set_var("TEST_STATUS_SILENT", ADMIN_SECRET);
        AdminClient::from_flags(
            &Common {
                ca: None,
                json: false,
                secret_env: Some("TEST_STATUS_SILENT".to_owned()),
                timeout: 2,
                url: Some(format!("http://127.0.0.1:{port}")),
            },
            Path::new("absent.yaml"),
        )
        .unwrap()
    };

    let started = tokio::time::Instant::now();
    let reads = status::read(&client, None).await;
    let spent = started.elapsed();

    let line = format!(
        "the admin listener did not answer on http://127.0.0.1:{port}: the call did not finish within 2s"
    );
    for (name, read) in [
        ("system", &reads.system),
        ("attention", &reads.attention),
        ("pools", &reads.pools),
        ("accounts", &reads.accounts),
        ("sessions", &reads.sessions),
    ] {
        assert_eq!(read.as_ref().err(), Some(&line), "{name}");
    }
    assert!(reads.health.is_none());
    assert!(
        spent >= Duration::from_secs(10) && spent < Duration::from_secs(11),
        "five reads at the client's two seconds each: {spent:?}"
    );
    assert_eq!(status::exit_code(&reads), 1);
    drop(silent);
}

/// A loopback listener answering every request with `status` and `body`, counting the connections.
async fn answering(
    status: &'static str,
    body: String,
) -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = calls.clone();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut head = Vec::new();
            let mut chunk = [0u8; 1024];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                let read = socket.read(&mut chunk).await.unwrap();
                if read == 0 {
                    break;
                }
                head.extend_from_slice(&chunk[..read]);
            }
            let answer = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(answer.as_bytes()).await.unwrap();
        }
    });
    (port, calls)
}

#[tokio::test]
async fn a_refused_secret_or_a_throttled_login_is_asked_once_and_named_in_every_read() {
    for (status_line, line, calls_wanted) in [
        (
            "401 Unauthorized",
            "the admin secret was refused".to_owned(),
            1,
        ),
        (
            "429 Too Many Requests",
            "the admin listener answered 429 refused for the test, 429 Too Many Requests"
                .to_owned(),
            1,
        ),
        (
            "503 Service Unavailable",
            "the admin listener answered 503 refused for the test, 503 Service Unavailable"
                .to_owned(),
            5,
        ),
    ] {
        let body = format!(r#"{{"error":"refused for the test, {status_line}"}}"#);
        let (port, calls) = answering(status_line, body).await;
        let client = {
            let _env = env_lock();
            std::env::set_var("TEST_STATUS_ONCE", ADMIN_SECRET);
            AdminClient::from_flags(
                &Common {
                    ca: None,
                    json: false,
                    secret_env: Some("TEST_STATUS_ONCE".to_owned()),
                    timeout: 10,
                    url: Some(format!("http://127.0.0.1:{port}")),
                },
                Path::new("absent.yaml"),
            )
            .unwrap()
        };
        let reads = status::read(&client, None).await;
        for (name, read) in [
            ("system", &reads.system),
            ("attention", &reads.attention),
            ("pools", &reads.pools),
            ("accounts", &reads.accounts),
            ("sessions", &reads.sessions),
        ] {
            assert_eq!(read.as_ref().err(), Some(&line), "{status_line}: {name}");
        }
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            calls_wanted,
            "{status_line}: a refusal of the secret is asked once, any other refusal per read"
        );
    }
}

#[tokio::test]
async fn a_listener_that_answers_something_else_is_named_as_answering() {
    let (port, _calls) = answering("200 OK", "hello".to_owned()).await;
    let client = {
        let _env = env_lock();
        std::env::set_var("TEST_STATUS_STRANGER", ADMIN_SECRET);
        AdminClient::from_flags(
            &Common {
                ca: None,
                json: false,
                secret_env: Some("TEST_STATUS_STRANGER".to_owned()),
                timeout: 10,
                url: Some(format!("http://127.0.0.1:{port}")),
            },
            Path::new("absent.yaml"),
        )
        .unwrap()
    };
    let reads = status::read(&client, None).await;
    let line = format!("http://127.0.0.1:{port}: answered 200 with a body that is not JSON");
    for (name, read) in [
        ("system", &reads.system),
        ("attention", &reads.attention),
        ("pools", &reads.pools),
        ("accounts", &reads.accounts),
        ("sessions", &reads.sessions),
    ] {
        assert_eq!(read.as_ref().err(), Some(&line), "{name}");
    }
}

/// 2026-10-31T12:00:00Z and 15:00:00Z and 2026-11-01T00:00:00Z in epoch milliseconds.
const OCT_31_NOON: i64 = 1_793_448_000_000;
const OCT_31_THREE: i64 = 1_793_458_800_000;
const NOV_1: i64 = 1_793_491_200_000;

/// A usage-based seat `id` of pool `main` whose account row carries `spend`.
fn spend_seat(id: &str, spend: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "id": id, "label": id, "enabled": true, "state": "ready", "windows": [], "cooldowns": [],
        "pool": {"pool_id": "main", "priority": 1}, "usage_based": true, "spend": spend
    })
}

#[test]
fn a_usage_based_seats_row_prints_its_spend_share_its_amounts_and_its_reset_marked_while_estimated()
{
    use serde_json::json;
    let amounts = |currency: &str, used: i64, limit: Option<i64>| json!({"currency": currency, "exponent": 2, "used_minor": used, "limit_minor": limit, "polled_at": OCT_31_NOON});
    let spend = |amounts: serde_json::Value, utilization: f64, resets_at: i64, source: &str| {
        json!({
            "amounts": amounts, "utilization": utilization, "resets_at": resets_at, "reset_source": source,
            "observed_at": OCT_31_NOON
        })
    };
    let seats = [
        (
            "seat-e",
            spend(
                amounts("USD", 102_500, Some(250_000)),
                0.41,
                NOV_1,
                "estimated",
            ),
        ),
        (
            "seat-h",
            spend(
                amounts("USD", 102_500, Some(250_000)),
                0.41,
                OCT_31_THREE,
                "head",
            ),
        ),
        (
            "seat-u",
            spend(serde_json::Value::Null, 0.3, NOV_1, "estimated"),
        ),
        (
            "seat-o",
            spend(amounts("USD", 5_000, None), 0.01, NOV_1, "estimated"),
        ),
        (
            "seat-x",
            spend(
                amounts("EUR", 102_500, Some(250_000)),
                0.41,
                NOV_1,
                "estimated",
            ),
        ),
        (
            "seat-r",
            spend(
                amounts("USD", 102_550, Some(250_000)),
                0.41,
                NOV_1,
                "estimated",
            ),
        ),
    ];
    let mut accounts: Vec<serde_json::Value> = seats
        .iter()
        .map(|(id, spend)| spend_seat(id, spend.clone()))
        .collect();
    accounts.push(json!({
        "id": "plan", "label": "plan", "enabled": true, "state": "ready", "cooldowns": [],
        "windows": [{"id": "five_hour", "utilization": 0.4, "resets_at": OCT_31_THREE, "observed_at": OCT_31_NOON}],
        "pool": {"pool_id": "main", "priority": 0}, "usage_based": false, "spend": null
    }));
    let members: Vec<serde_json::Value> = accounts
        .iter()
        .map(|a| json!({"account_id": a["id"], "priority": a["pool"]["priority"], "eligible": "eligible"}))
        .collect();
    let reads = status::Reads {
        accounts: Ok(json!({ "accounts": accounts })),
        attention: Ok(json!({"items": []})),
        health: None,
        pools: Ok(json!({"pools": [{
            "id": "main", "status": "ok", "members": members, "keys": [], "active_pins": 0, "requests_24h": 0
        }]})),
        sessions: Ok(json!({"sessions": []})),
        system: Ok(json!({"ready": true})),
    };
    // The day before the estimate's: a head's reset prints as a time of the clock's day, the estimate by its date.
    let screen = render_at(&reads, OCT_31_NOON);
    for (id, cells, why) in [
        (
            "seat-e",
            ["spend 41%", "$1025 of $2500", "resets 11-01 est"],
            "the amounts in whole dollars and the estimate by its date",
        ),
        (
            "seat-h",
            ["spend 41%", "$1025 of $2500", "resets 15:00"],
            "a head's reset carries no mark",
        ),
        (
            "seat-u",
            ["spend 30%", "-", "resets 11-01 est"],
            "a spend with no amounts prints a dash in their cell",
        ),
        (
            "seat-o",
            ["spend 1%", "$50", "resets 11-01 est"],
            "a spend with no limit prints the spend alone",
        ),
        (
            "seat-x",
            ["spend 41%", "EUR 1025 of EUR 2500", "resets 11-01 est"],
            "another currency prints its code",
        ),
        (
            "seat-r",
            ["spend 41%", "$1026 of $2500", "resets 11-01 est"],
            "a half dollar rounds up to the next whole dollar",
        ),
        (
            "plan",
            ["5h 40%", "7d -", "resets 15:00"],
            "a plan account's row prints as before",
        ),
    ] {
        let expected: Vec<String> = [id, "ready", "eligible"]
            .into_iter()
            .chain(cells)
            .map(String::from)
            .collect();
        assert_eq!(member_row(&screen, id), expected, "{why}: {screen}");
    }
}

#[test]
fn an_account_flagged_plan_ended_prints_plan_ended_beside_its_state_word() {
    use serde_json::json;
    let account = |id: &str, plan_ended_at: serde_json::Value, pool: serde_json::Value| {
        json!({
            "id": id, "label": id, "enabled": true, "state": "ready", "windows": [], "cooldowns": [],
            "pool": pool, "usage_based": false, "spend": null, "plan_ended_at": plan_ended_at
        })
    };
    let member = json!({"pool_id": "main", "priority": 0});
    let reads = status::Reads {
        accounts: Ok(json!({ "accounts": [
            account("ended", json!(OCT_31_NOON), member.clone()),
            account("plan", serde_json::Value::Null, json!({"pool_id": "main", "priority": 1})),
            account("loose", json!(OCT_31_NOON), serde_json::Value::Null),
        ]})),
        attention: Ok(json!({"items": []})),
        health: None,
        pools: Ok(json!({"pools": [{
            "id": "main", "status": "ok", "keys": [], "active_pins": 0, "requests_24h": 0,
            "members": [
                {"account_id": "ended", "priority": 0, "eligible": "eligible"},
                {"account_id": "plan", "priority": 1, "eligible": "eligible"}
            ]
        }]})),
        sessions: Ok(json!({"sessions": []})),
        system: Ok(json!({"ready": true})),
    };
    let screen = render_at(&reads, OCT_31_NOON);
    assert_eq!(
        member_row(&screen, "ended")[..3],
        ["ended", "ready, plan ended", "eligible"],
        "{screen}"
    );
    assert_eq!(
        member_row(&screen, "plan")[..3],
        ["plan", "ready", "eligible"],
        "{screen}"
    );
    assert_eq!(
        member_row(&screen, "loose"),
        ["loose", "ready, plan ended"],
        "an account in no pool prints the word on its state line: {screen}"
    );
}

/// `reads` rendered at `at_ms` in UTC.
fn render_at(reads: &status::Reads, at_ms: i64) -> String {
    let listeners = status::Listeners {
        admin: "http://127.0.0.1:8081".to_owned(),
        certificates: Vec::new(),
        metrics: None,
        proxy: None,
    };
    status::render(
        reads,
        &listeners,
        VERSION,
        "build",
        jiff::Timestamp::from_millisecond(at_ms).unwrap(),
        &jiff::tz::TimeZone::UTC,
        false,
    )
}

/// The cells of the pools block's line for the member labelled `label`, which the screen printed whole: the screen
/// cuts a line past its width with an ellipsis, so a whole line has none.
fn member_row(screen: &str, label: &str) -> Vec<String> {
    let line = screen
        .lines()
        .find(|line| line.trim_start().starts_with(&format!("{label} ")))
        .unwrap_or_else(|| panic!("no {label} row: {screen}"));
    assert!(
        !line.contains('\u{2026}') && line.chars().count() <= status::WIDTH,
        "{label}'s row prints whole inside the width: {line}"
    );
    line.split("  ")
        .map(str::trim)
        .filter(|cell| !cell.is_empty())
        .map(String::from)
        .collect()
}

/// One captured admin answer of the fixture set, unwrapped from its `{"route", "body"}` envelope.
fn fixture(name: &str) -> serde_json::Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../web/test/fixtures")
        .join(format!("{name}.json"));
    let wrapped: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    wrapped["body"].clone()
}

/// The fixture set's clock, 2026-10-11T12:16:40Z, the day its captured answers read.
const FIXTURE_NOW: i64 = 1_791_721_000_000;

#[test]
fn the_fixtures_seat_row_prints_whole_inside_eighty_columns_on_its_own_day_and_the_day_before_the_estimate(
) {
    let reads = status::Reads {
        accounts: Ok(fixture("accounts")),
        attention: Ok(fixture("attention")),
        health: None,
        pools: Ok(fixture("pools")),
        sessions: Ok(fixture("sessions")),
        system: Ok(fixture("system")),
    };
    let seat = fixture("accounts")["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|account| account["usage_based"] == true)
        .cloned()
        .expect("the fixture set seeds a usage-based seat");
    assert_eq!(
        seat["spend"]["reset_source"], "estimated",
        "the seed's seat reads the estimate"
    );
    let label = seat["label"].as_str().unwrap();
    for at in [FIXTURE_NOW, OCT_31_NOON] {
        let screen = render_at(&reads, at);
        assert_eq!(
            member_row(&screen, label),
            [
                label,
                "ready",
                "eligible",
                "spend 40%",
                "$1025 of $2500",
                "resets 11-01 est"
            ],
            "{screen}"
        );
    }
}
