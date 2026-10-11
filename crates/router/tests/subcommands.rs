//! `tls reload` from the outside: the binary against a router started in-process on loopback with an admin pair,
//! trusting the pair's CA through `--ca`, with `--json` and with a refused secret. A removed registry command is
//! refused as an unknown subcommand. `status.rs` and `admin_client.rs` drive the shared flags over plain HTTP and
//! without the file.

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use fake_upstream::{
    generate_pair, generate_pair_valid_between, generate_pair_valid_until, FakeUpstream,
    GeneratedPair,
};
use hyper::Method;
use phoebis_router::admin_client::{AdminClient, Common};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

/// `load` and `from_flags` read the admin secret from the process environment, so each test holds this across its
/// set-then-read sequence.
static ENV: Mutex<()> = Mutex::new(());

/// How long a boot may take to bind its listeners; generous because it is reached only when the boot never ends.
const SETTLE: Duration = Duration::from_secs(30);

const ADMIN_SECRET: &str = "admin-secret-subcommands-001";

/// A port from a hundred-port slice below the kernel's ephemeral range (32768 upward on Linux), spread by pid as
/// `end_to_end.rs` spreads its own, so no outbound connection takes it between the probe and the router's bind.
fn free_port() -> u16 {
    static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
    let base = 20_000 + (std::process::id() % 100) as u16 * 100;
    loop {
        let offset = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert!(offset < 100, "subcommands.rs drew past its port slice");
        if std::net::TcpListener::bind(("127.0.0.1", base + offset)).is_ok() {
            return base + offset;
        }
    }
}

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
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
    secret_env: String,
    _up: FakeUpstream,
}

impl Running {
    async fn stop(self) {
        let _ = self.stop.send(());
        self.server.await.unwrap().unwrap();
    }

    /// `phoebis <args>` against this router's file, the admin secret in the file's variable.
    async fn run(&self, args: &[&str]) -> Output {
        phoebis(
            args,
            &self.config,
            &[(self.secret_env.as_str(), ADMIN_SECRET)],
            &[],
            None,
        )
        .await
    }

    /// A route's answer read through the admin client, so the check shares no code with the command beyond the
    /// transport. The call goes over TLS, trusting the admin pair's CA, when the router was started with one.
    ///
    /// The lock spans the call too: a call over TLS builds its connector through `SslConnector::builder`, which reads
    /// `SSL_CERT_FILE` through C `getenv`, and each test runs on its own current-thread runtime, so no task of this
    /// test waits on the lock it holds.
    #[allow(clippy::await_holding_lock)]
    async fn get(&self, path: &str) -> Value {
        let scheme = if self.admin_ca.is_some() {
            "https"
        } else {
            "http"
        };
        let common = Common {
            ca: self.admin_ca.clone(),
            json: false,
            secret_env: Some(self.secret_env.clone()),
            timeout: 10,
            url: Some(format!("{scheme}://127.0.0.1:{}", self.admin)),
        };
        let _env = env_lock();
        std::env::set_var(&self.secret_env, ADMIN_SECRET);
        let client = AdminClient::from_flags(&common, &self.config).unwrap();
        client
            .expect(Method::GET, path, None)
            .await
            .unwrap_or_else(|error| panic!("GET {path} failed: {error}"))
            .body
    }
}

/// Starts the router with `admin_pair`, when given, as `admin.tls` and its CA written to `admin-ca.pem`, and the admin
/// secret read from `secret_env`. It returns once the admin listener accepts, and a start that ends first fails the
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
        admin_ca,
        config,
        dir,
        secret_env: secret_env.to_owned(),
        _up: up,
    }
}

/// `phoebis <args> --config <config>` with `set` in the child's environment and `unset` removed from it, `input` on
/// its stdin (an empty stdin otherwise).
async fn phoebis(
    args: &[&str],
    config: &Path,
    set: &[(&str, &str)],
    unset: &[&str],
    input: Option<&[u8]>,
) -> Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_phoebis"));
    command
        .env_remove("PHOEBIS_CONFIG")
        .args(args)
        .arg("--config")
        .arg(config)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in set {
        command.env(name, value);
    }
    for name in unset {
        command.env_remove(name);
    }
    let mut child = command.spawn().unwrap();
    if let Some(input) = input {
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(input).await.unwrap();
        drop(stdin);
    }
    child.wait_with_output().await.unwrap()
}

fn text(output: &Output) -> (String, String) {
    (
        String::from_utf8(output.stdout.clone()).unwrap(),
        String::from_utf8(output.stderr.clone()).unwrap(),
    )
}

fn assert_exit(output: &Output, expected: i32) {
    let (stdout, stderr) = text(output);
    assert_eq!(
        output.status.code(),
        Some(expected),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
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

/// The hosts an admin pair names, so the file's `https://127.0.0.1:{port}` verifies.
const ADMIN_HOSTS: [&str; 2] = ["localhost", "127.0.0.1"];

/// `--ca` with the admin pair's CA, which `start` wrote.
fn trusting(running: &Running) -> String {
    running
        .admin_ca
        .as_ref()
        .expect("the router was started with an admin pair")
        .display()
        .to_string()
}

#[tokio::test]
async fn tls_reload_prints_one_line_per_listener_and_exits_0_once_every_pair_reloaded() {
    let pair = generate_pair_valid_between(&ADMIN_HOSTS, (2020, 1, 1), (2099, 1, 1));
    let running = start(Some(&pair), "TEST_SUB_TLS_RELOAD").await;
    let reloaded = running
        .run(&["tls", "reload", "--ca", &trusting(&running)])
        .await;
    assert_exit(&reloaded, 0);
    let (stdout, stderr) = text(&reloaded);
    assert_eq!(
        stdout,
        format!(
            "proxy: plain HTTP, nothing to reload\nadmin: reloaded, certificate {} expires \
             2099-01-01T00:00:00Z\n",
            running.dir.path().join("admin.pem").display()
        )
    );
    assert_eq!(stderr, "");
    assert_no_secret(&reloaded, &[ADMIN_SECRET, &pair.key_pem]);
    running.stop().await;
}

#[tokio::test]
async fn tls_reload_names_a_refused_pair_and_exits_1() {
    let pair = generate_pair(&ADMIN_HOSTS);
    let running = start(Some(&pair), "TEST_SUB_TLS_REFUSED_PAIR").await;
    let (cert, key) = (
        running.dir.path().join("admin.pem"),
        running.dir.path().join("admin.key"),
    );
    std::fs::write(&key, generate_pair(&ADMIN_HOSTS).key_pem).unwrap();
    let refused = running
        .run(&["tls", "reload", "--ca", &trusting(&running)])
        .await;
    assert_exit(&refused, 1);
    assert_eq!(
        text(&refused).0,
        format!(
            "proxy: plain HTTP, nothing to reload\nadmin: not reloaded, admin.tls.key_file {}: does not match \
             the certificate in {}. The router keeps the pair it was serving.\n",
            key.display(),
            cert.display()
        )
    );
    assert_no_secret(&refused, &[ADMIN_SECRET]);
    // Over TLS under the pair's CA: the listener still serves the pair it loaded at start.
    let system = running.get("/admin/system").await;
    assert!(system["version"].is_string(), "{system}");
    running.stop().await;
}

#[tokio::test]
async fn tls_reload_json_prints_the_routes_body() {
    // Four hundred days and an hour on: four hundred whole days whatever the clock reads.
    let not_after = jiff::Timestamp::now().as_second() + 400 * 86_400 + 3_600;
    let pair = generate_pair_valid_until(&ADMIN_HOSTS, not_after);
    let running = start(Some(&pair), "TEST_SUB_TLS_JSON").await;
    let printed = running
        .run(&["tls", "reload", "--ca", &trusting(&running), "--json"])
        .await;
    assert_exit(&printed, 0);
    let body: Value = serde_json::from_str(&text(&printed).0).unwrap();
    assert_eq!(
        body,
        serde_json::json!({"listeners": [
            {"listener": "proxy", "serving": "plain"},
            {
                "listener": "admin",
                "serving": "tls",
                "reloaded": true,
                "cert_file": running.dir.path().join("admin.pem").display().to_string(),
                "expires": jiff::Timestamp::from_second(not_after).unwrap().to_string(),
                "days_left": 400,
            },
        ]})
    );
    running.stop().await;
}

#[tokio::test]
async fn tls_reload_with_a_refused_secret_exits_1_and_prints_no_secret() {
    const WRONG: &str = "not-the-admin-secret-tls";
    let pair = generate_pair(&ADMIN_HOSTS);
    let running = start(Some(&pair), "TEST_SUB_TLS_WRONG").await;
    let refused = phoebis(
        &["tls", "reload", "--ca", &trusting(&running)],
        &running.config,
        &[("TEST_SUB_TLS_WRONG", WRONG)],
        &[],
        None,
    )
    .await;
    assert_exit(&refused, 1);
    let (stdout, stderr) = text(&refused);
    assert_eq!(stdout, "");
    assert_eq!(stderr, "the admin secret was refused\n");
    assert_no_secret(&refused, &[WRONG, ADMIN_SECRET]);
    running.stop().await;
}

/// The help lists the binary's commands in the table's order, with clap's own `help` entry allowed beside them.
#[tokio::test]
async fn the_help_lists_every_command_and_no_other() {
    let dir = tempfile::tempdir().unwrap();
    let absent = dir.path().join("absent.yaml");
    let help = phoebis(&["--help"], &absent, &[], &[], None).await;
    assert_exit(&help, 0);
    let (stdout, _) = text(&help);
    // A command's own line is indented two spaces, and a description wrapped past it further.
    let commands: Vec<&str> = stdout
        .lines()
        .skip_while(|line| *line != "Commands:")
        .skip(1)
        .take_while(|line| !line.is_empty())
        .filter(|line| line.starts_with("  ") && !line.starts_with("   "))
        .filter_map(|line| line.split_whitespace().next())
        .filter(|command| *command != "help")
        .collect();
    assert_eq!(
        commands,
        [
            "backup",
            "check-config",
            "health",
            "init",
            "recover-key",
            "status",
            "tls"
        ],
        "{stdout}"
    );
}

/// The console is the one place a registry row is created or edited, so the binary knows no registry command: clap
/// refuses each as an unknown subcommand before any file is read.
#[tokio::test]
async fn a_removed_subcommand_is_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let absent = dir.path().join("absent.yaml");
    for (args, name) in [
        (&["account", "add", "x"][..], "account"),
        (&["ledger", "replay-parked"], "ledger"),
    ] {
        let refused = phoebis(args, &absent, &[], &[], None).await;
        assert_exit(&refused, 2);
        let (stdout, stderr) = text(&refused);
        assert_eq!(stdout, "", "{args:?}");
        assert!(
            stderr.starts_with(&format!("error: unrecognized subcommand '{name}'\n")),
            "{args:?}: {stderr}"
        );
    }
}
