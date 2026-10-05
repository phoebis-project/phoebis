//! `account add`, `account clear-cooldown`, `account pair`, `account confirm`, `pool create`, `key create` and `ledger
//! replay-parked` from the outside: the binary against a router started in-process on loopback, over plain HTTP and
//! over TLS, with the file, without it, and with a refused secret, the fake upstream standing in for the provider a
//! pairing exchanges its code with; and `clear-cooldown` and `key create` against a scripted admin listener for the one
//! request each sends and the answer each cannot print.

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fake_upstream::{generate_pair, FakeUpstream, GeneratedPair, Respond, Rule};
use hyper::Method;
use phoebis_profile::fixtures::{control_response, ControlFixture};
use phoebis_router::admin_client::{AdminClient, Common};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, ChildStdin, ChildStdout};

/// `load` and `from_flags` read the admin secret from the process environment, so each test holds this across its
/// set-then-read sequence.
static ENV: Mutex<()> = Mutex::new(());

/// How long a boot may take to bind its listeners; generous because it is reached only when the boot never ends.
const SETTLE: Duration = Duration::from_secs(30);

const ADMIN_SECRET: &str = "admin-secret-subcommands-001";

/// An upstream key the fake upstream's discovery rule answers 200 for.
const UPSTREAM_KEY: &str = "sk-ant-api03-subcommands-fake-key";

/// How long a pairing run may take to answer or end; reached only when the command waits on input it already has.
const PAIRED: Duration = Duration::from_secs(30);

/// The identity the profile control fixture answers, which a pairing's callback shows as pending.
const FIXTURE_EMAIL: &str = "test@example.invalid";

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
    config: PathBuf,
    dir: tempfile::TempDir,
    secret_env: String,
    up: FakeUpstream,
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

    /// A route's answer read through the subcommands' own client, so the check shares no code with the subcommand
    /// beyond the transport.
    async fn get(&self, path: &str) -> Value {
        let common = Common {
            ca: None,
            json: false,
            secret_env: Some(self.secret_env.clone()),
            timeout: 10,
            url: Some(format!("http://127.0.0.1:{}", self.admin)),
        };
        let client = {
            let _env = env_lock();
            std::env::set_var(&self.secret_env, ADMIN_SECRET);
            AdminClient::from_flags(&common, &self.config).unwrap()
        };
        client
            .expect(Method::GET, path, None)
            .await
            .unwrap_or_else(|error| panic!("GET {path} failed: {error}"))
            .body
    }

    /// `phoebis account pair <args> --json` with its three streams piped, and the start answer read off stdout while
    /// the command waits on stdin for the paste.
    async fn pair(&self, args: &[&str]) -> (Pairing, Value) {
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_phoebis"))
            .env_remove("PHOEBIS_CONFIG")
            .env(&self.secret_env, ADMIN_SECRET)
            .args(["account", "pair"])
            .args(args)
            .arg("--json")
            .arg("--config")
            .arg(&self.config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut stderr_pipe = child.stderr.take().unwrap();
        // Read beside stdout, so a command that writes more to stderr than a pipe holds never stalls the test.
        let stderr = tokio::spawn(async move {
            let mut text = String::new();
            stderr_pipe.read_to_string(&mut text).await.unwrap();
            text
        });
        match started(&mut stdout).await {
            Ok(start) => (
                Pairing {
                    child,
                    stdin,
                    stdout,
                    stderr,
                },
                start,
            ),
            Err(first) => {
                drop(stdin);
                let status = child.wait().await.unwrap();
                let stderr = stderr.await.unwrap();
                panic!("no start answer on stdout ({first:?}), {status}, stderr:\n{stderr}")
            }
        }
    }
}

/// A running `account pair` whose stdout has been read up to the start answer.
struct Pairing {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    stderr: tokio::task::JoinHandle<String>,
}

/// A pairing run's end: the exit code, the stdout lines after the start answer, and stderr whole.
struct Ended {
    code: Option<i32>,
    answers: Vec<Value>,
    stderr: String,
}

impl Pairing {
    /// Writes `paste`, if any, closes stdin and waits for the command to end.
    async fn end(mut self, paste: Option<&str>) -> Ended {
        let mut stdin = self.stdin.take().unwrap();
        if let Some(paste) = paste {
            stdin.write_all(paste.as_bytes()).await.unwrap();
        }
        drop(stdin);
        let ended = tokio::time::timeout(PAIRED, async {
            let mut rest = String::new();
            self.stdout.read_to_string(&mut rest).await.unwrap();
            let status = self.child.wait().await.unwrap();
            (status.code(), rest, self.stderr.await.unwrap())
        })
        .await
        .expect("the pairing run ended");
        let (code, rest, stderr) = ended;
        let answers = rest
            .lines()
            .map(|line| {
                serde_json::from_str(line)
                    .unwrap_or_else(|_| panic!("stdout carries a line that is no answer: {line:?}"))
            })
            .collect();
        Ended {
            code,
            answers,
            stderr,
        }
    }
}

/// The start answer, the first line `--json` writes: the one place the pairing id, the redirect and the state
/// travel, since no route lists a pairing, `pairing_show` answers no redirect and the instruction names no id. The
/// line read is the error when it is no JSON object.
async fn started(stdout: &mut BufReader<ChildStdout>) -> Result<Value, String> {
    let mut first = String::new();
    tokio::time::timeout(PAIRED, stdout.read_line(&mut first))
        .await
        .expect("the start answer arrived")
        .unwrap();
    match serde_json::from_str::<Value>(&first) {
        Ok(start) if start.is_object() => Ok(start),
        _ => Err(first),
    }
}

/// The address the browser lands on after the sign-in, as the provider builds it for the start answer's redirect.
fn callback_url(start: &Value, code: &str) -> String {
    let state = start["authorization_url"]
        .as_str()
        .unwrap()
        .split("state=")
        .nth(1)
        .unwrap();
    format!(
        "{}?code={code}&state={state}",
        start["redirect_uri"].as_str().unwrap()
    )
}

/// Starts the router with `admin_pair`, when given, as `admin.tls`, the admin secret read from `secret_env`, and an
/// upstream whose discovery answers 200 and whose token, profile and roles routes answer the control fixtures, so a
/// pairing's exchange reaches it as the provider; returns once the admin listener accepts, and a start that ends first
/// fails the test with its own error rather than at the settle deadline.
async fn start(admin_pair: Option<&GeneratedPair>, secret_env: &str) -> Running {
    let control = |which: ControlFixture| Respond::Json {
        status: 200,
        headers: vec![],
        body: control_response(which).to_vec(),
    };
    let up = FakeUpstream::start(vec![
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
    std::fs::write(dir.path().join("upstream-ca.pem"), up.ca_pem()).unwrap();
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
        config,
        dir,
        secret_env: secret_env.to_owned(),
        up,
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

/// The entry of a list answer (`{"accounts": [...]}` and the like) whose `id` is `id`.
fn entry<'a>(list: &'a Value, member: &str, id: &str) -> Option<&'a Value> {
    list[member]
        .as_array()
        .and_then(|entries| entries.iter().find(|entry| entry["id"] == id))
}

#[tokio::test]
async fn account_add_prints_the_id_and_the_account_lists() {
    let running = start(None, "TEST_SUB_ADD").await;
    let added = running
        .run(&["account", "add", "alice", "--kind", "oauth"])
        .await;
    assert_exit(&added, 0);
    let (stdout, stderr) = text(&added);
    assert_eq!(stdout, "alice\n");
    let fields: Vec<&str> = stderr.lines().collect();
    assert!(
        fields.contains(&"kind: oauth") && fields.contains(&"state: unpaired"),
        "{stderr}"
    );
    assert!(
        fields.contains(&"label: alice"),
        "the label defaults to the id: {stderr}"
    );
    let mut sorted = fields.clone();
    sorted.sort();
    assert_eq!(fields, sorted, "the fields print sorted");

    let accounts = running.get("/admin/accounts").await;
    let alice = entry(&accounts, "accounts", "alice").expect("alice is listed");
    assert_eq!(alice["kind"], "oauth");
    running.stop().await;
}

#[tokio::test]
async fn pool_create_with_members_creates_the_pool() {
    let running = start(None, "TEST_SUB_POOL").await;
    assert_exit(
        &running
            .run(&["account", "add", "alice", "--kind", "oauth"])
            .await,
        0,
    );
    let created = running
        .run(&[
            "pool",
            "create",
            "work",
            "--label",
            "Work",
            "--member",
            "alice:1",
            "--strategy",
            "priority",
        ])
        .await;
    assert_exit(&created, 0);
    let (stdout, stderr) = text(&created);
    assert_eq!(stdout, "work\n");
    assert!(stderr.lines().any(|line| line == "label: Work"), "{stderr}");

    let pools = running.get("/admin/pools").await;
    let work = entry(&pools, "pools", "work").expect("work is listed");
    assert_eq!(work["label"], "Work");
    assert_eq!(work["strategy"], "priority");
    let members = work["members"].as_array().unwrap();
    assert_eq!(members.len(), 1, "{work}");
    assert_eq!(members[0]["account_id"], "alice");
    assert_eq!(members[0]["priority"], 1);
    running.stop().await;
}

#[tokio::test]
async fn key_create_prints_the_secret_once_and_the_key_lists_without_it() {
    let running = start(None, "TEST_SUB_KEY").await;
    assert_exit(&running.run(&["pool", "create", "work"]).await, 0);
    let created = running
        .run(&[
            "key", "create", "laptop", "--pool", "work", "--label", "Laptop",
        ])
        .await;
    assert_exit(&created, 0);
    let (stdout, stderr) = text(&created);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "stdout is the secret alone: {stdout:?}");
    let secret = lines[0];
    assert!(secret.len() >= 16, "{secret:?}");
    assert!(
        !stderr.contains(secret),
        "the secret is printed once: {stderr}"
    );
    let fields: Vec<&str> = stderr.lines().collect();
    assert!(
        fields.contains(&"label: Laptop") && fields.contains(&"pool_id: work"),
        "{stderr}"
    );
    assert!(
        !fields.iter().any(|line| line.starts_with("secret:")),
        "{stderr}"
    );

    let keys = running.get("/admin/keys").await;
    assert!(entry(&keys, "keys", "laptop").is_some(), "{keys}");
    assert!(
        !keys.to_string().contains(secret),
        "the list carries no secret"
    );
    running.stop().await;
}

/// `/dev/full` answers every write with "no space left", the full disk under `key create ... > laptop.secret`.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn key_create_exits_one_naming_the_rotate_route_when_stdout_takes_no_secret() {
    let running = start(None, "TEST_SUB_KEY_FULL").await;
    assert_exit(&running.run(&["pool", "create", "work"]).await, 0);
    let full = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .unwrap();
    let created = tokio::process::Command::new(env!("CARGO_BIN_EXE_phoebis"))
        .env_remove("PHOEBIS_CONFIG")
        .env("TEST_SUB_KEY_FULL", ADMIN_SECRET)
        .args(["key", "create", "laptop", "--pool", "work", "--config"])
        .arg(&running.config)
        .stdin(Stdio::null())
        .stdout(Stdio::from(full))
        .stderr(Stdio::piped())
        // `output()` would pipe stdout over the file, so the child is spawned and waited on.
        .spawn()
        .unwrap()
        .wait_with_output()
        .await
        .unwrap();
    assert_exit(&created, 1);
    let stderr = String::from_utf8(created.stderr.clone()).unwrap();
    let last = stderr.lines().last().unwrap_or_default();
    assert!(
        last.starts_with("the secret of key laptop did not reach stdout")
            && last.ends_with("POST /admin/keys/laptop/rotate issues a new one"),
        "{stderr}"
    );
    let keys = running.get("/admin/keys").await;
    assert!(
        entry(&keys, "keys", "laptop").is_some(),
        "the key exists: {keys}"
    );
    running.stop().await;
}

#[tokio::test]
async fn clear_cooldown_on_a_bare_account_is_404() {
    let running = start(None, "TEST_SUB_CLEAR").await;
    assert_exit(
        &running
            .run(&["account", "add", "alice", "--kind", "oauth"])
            .await,
        0,
    );
    let cleared = running.run(&["account", "clear-cooldown", "alice"]).await;
    assert_exit(&cleared, 1);
    let (stdout, stderr) = text(&cleared);
    assert_eq!(stdout, "");
    assert_eq!(stderr, "404 no such cooldown\n");
    running.stop().await;
}

#[tokio::test]
async fn json_prints_the_routes_answer() {
    let running = start(None, "TEST_SUB_JSON").await;
    let created = running.run(&["pool", "create", "home", "--json"]).await;
    assert_exit(&created, 0);
    let (stdout, stderr) = text(&created);
    assert_eq!(stderr, "", "nothing but errors on stderr under --json");
    assert_eq!(
        (stdout.matches('\n').count(), stdout.ends_with('\n')),
        (1, true),
        "the answer is one line, as the route wrote it: {stdout:?}"
    );
    let answer: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(answer["id"], "home");
    assert_eq!(answer["label"], "home", "the label defaults to the id");
    assert_eq!(answer["members"], Value::Array(vec![]));
    running.stop().await;
}

#[tokio::test]
async fn a_refused_secret_exits_one_with_no_secret_in_any_output() {
    const WRONG: &str = "not-the-admin-secret-subcommands";
    let running = start(None, "TEST_SUB_REFUSED").await;
    // Four refusals, one short of the login throttle, so each run meets the 401 rather than a 429.
    for args in [
        &["account", "add", "bob", "--kind", "oauth"][..],
        &["account", "clear-cooldown", "bob"],
        &["pool", "create", "work"],
        &["key", "create", "laptop", "--pool", "work"],
    ] {
        let refused = phoebis(
            args,
            &running.config,
            &[("TEST_SUB_REFUSED", WRONG)],
            &[],
            None,
        )
        .await;
        assert_exit(&refused, 1);
        let (stdout, stderr) = text(&refused);
        assert_eq!(stdout, "", "{args:?}");
        assert_eq!(stderr, "the admin secret was refused\n", "{args:?}");
        assert_no_secret(&refused, &[WRONG, ADMIN_SECRET]);
    }
    running.stop().await;
}

#[tokio::test]
async fn url_and_secret_env_run_with_no_configuration_file() {
    let running = start(None, "TEST_SUB_URL_ADMIN").await;
    let absent = running.dir.path().join("absent.yaml");
    let url = format!("http://127.0.0.1:{}", running.admin);
    let added = phoebis(
        &[
            "account",
            "add",
            "bob",
            "--kind",
            "oauth",
            "--url",
            &url,
            "--secret-env",
            "TEST_SUB_URL_CALLER",
        ],
        &absent,
        &[("TEST_SUB_URL_CALLER", ADMIN_SECRET)],
        &["TEST_SUB_URL_ADMIN"],
        None,
    )
    .await;
    assert_exit(&added, 0);
    assert_eq!(text(&added).0, "bob\n");
    running.stop().await;
}

#[tokio::test]
async fn a_body_file_reaches_fields_no_flag_names() {
    let running = start(None, "TEST_SUB_BODY").await;
    let body = running.dir.path().join("pool.json");
    std::fs::write(&body, r#"{"shed_share": 0.25, "label": "Lab"}"#).unwrap();
    let body = body.to_str().unwrap();

    let lab = running
        .run(&["pool", "create", "lab", "--body", body, "--json"])
        .await;
    assert_exit(&lab, 0);
    let answer: Value = serde_json::from_str(&text(&lab).0).unwrap();
    assert_eq!(answer["shed_share"], 0.25);
    assert_eq!(
        answer["label"], "Lab",
        "the body's label survives the absent flag"
    );

    let two = running
        .run(&[
            "pool", "create", "lab2", "--label", "Two", "--body", body, "--json",
        ])
        .await;
    assert_exit(&two, 0);
    let answer: Value = serde_json::from_str(&text(&two).0).unwrap();
    assert_eq!(answer["shed_share"], 0.25);
    assert_eq!(
        answer["label"], "Two",
        "the flag wins where both name a key"
    );
    running.stop().await;
}

#[tokio::test]
async fn account_add_over_https_trusts_the_ca_named_by_ca() {
    let pair = generate_pair(&["localhost", "127.0.0.1"]);
    let running = start(Some(&pair), "TEST_SUB_TLS").await;
    let ca = running.dir.path().join("admin-ca.pem");
    std::fs::write(&ca, &pair.ca_pem).unwrap();
    let added = running
        .run(&[
            "account",
            "add",
            "carol",
            "--kind",
            "oauth",
            "--ca",
            ca.to_str().unwrap(),
        ])
        .await;
    assert_exit(&added, 0);
    assert_eq!(text(&added).0, "carol\n");
    running.stop().await;
}

#[tokio::test]
async fn account_add_api_key_reads_the_key_from_its_own_variable() {
    let running = start(None, "TEST_SUB_APIKEY_ADMIN").await;
    let first = phoebis(
        &[
            "account",
            "add",
            "alice2",
            "--kind",
            "api_key",
            "--api-key-env",
            "ALICE_KEY",
        ],
        &running.config,
        &[
            ("TEST_SUB_APIKEY_ADMIN", ADMIN_SECRET),
            ("ALICE_KEY", UPSTREAM_KEY),
        ],
        &[],
        None,
    )
    .await;
    assert_exit(&first, 0);
    assert_eq!(text(&first).0, "alice2\n");

    // The admin secret from a second variable while the file's is unset: the two credentials never share one.
    let second = phoebis(
        &[
            "account",
            "add",
            "alice3",
            "--kind",
            "api_key",
            "--secret-env",
            "OTHER_ADMIN",
            "--api-key-env",
            "ALICE_KEY",
        ],
        &running.config,
        &[("OTHER_ADMIN", ADMIN_SECRET), ("ALICE_KEY", UPSTREAM_KEY)],
        &["TEST_SUB_APIKEY_ADMIN"],
        None,
    )
    .await;
    assert_exit(&second, 0);
    assert_eq!(text(&second).0, "alice3\n");

    for run in [&first, &second] {
        assert_no_secret(run, &[ADMIN_SECRET, UPSTREAM_KEY]);
    }
    let accounts = running.get("/admin/accounts").await;
    for id in ["alice2", "alice3"] {
        let account = entry(&accounts, "accounts", id).unwrap_or_else(|| panic!("{id} is listed"));
        assert_eq!(account["kind"], "api_key");
    }
    let discovered = running
        .up
        .requests()
        .into_iter()
        .filter(|r| r.line.starts_with("GET /v1/models"))
        .filter(|r| {
            r.headers
                .iter()
                .any(|(n, v)| n == "x-api-key" && v == UPSTREAM_KEY)
        })
        .count();
    assert!(
        discovered >= 2,
        "each creation's discovery carried the key from ALICE_KEY"
    );
    running.stop().await;
}

#[tokio::test]
async fn account_pair_runs_the_three_routes_on_stdin() {
    let running = start(None, "TEST_SUB_PAIR").await;
    assert_exit(
        &running
            .run(&["account", "add", "dave", "--kind", "oauth"])
            .await,
        0,
    );
    let (pairing, start) = running.pair(&["dave"]).await;
    let url = start["authorization_url"].as_str().unwrap().to_owned();
    assert!(
        url.starts_with("https://claude.com/cai/oauth/authorize?"),
        "a claude_ai login by default: {url}"
    );
    let paste = format!("{}\n", callback_url(&start, "CODE-dave"));
    let ended = pairing.end(Some(&paste)).await;
    assert_eq!(ended.code, Some(0), "stderr:\n{}", ended.stderr);
    assert_eq!(ended.answers.len(), 2, "{:?}", ended.answers);
    let (callback, confirm) = (&ended.answers[0], &ended.answers[1]);
    assert_eq!(callback["status"], "exchanged");
    assert_eq!(callback["pending"]["email"], FIXTURE_EMAIL);
    assert_eq!(confirm["status"], "confirmed");
    assert_eq!(confirm["account_id"], "dave");

    let lines: Vec<&str> = ended.stderr.lines().collect();
    assert!(
        lines.contains(&"open this URL, sign in, and paste the callback URL here:")
            && lines.contains(&url.as_str()),
        "{}",
        ended.stderr
    );
    assert!(
        lines.contains(&format!("pending: {FIXTURE_EMAIL} (Test Org)").as_str()),
        "{}",
        ended.stderr
    );
    assert_eq!(
        lines.last().copied(),
        Some(format!("paired dave as {FIXTURE_EMAIL}").as_str())
    );
    let token = String::from_utf8(control_response(ControlFixture::Token).to_vec()).unwrap();
    let token: Value = serde_json::from_str(&token).unwrap();
    for secret in [
        ADMIN_SECRET,
        token["access_token"].as_str().unwrap(),
        token["refresh_token"].as_str().unwrap(),
    ] {
        assert!(
            !ended.stderr.contains(secret)
                && !ended.answers.iter().any(|a| a.to_string().contains(secret)),
            "a secret was printed"
        );
    }

    assert_eq!(running.get("/admin/accounts/dave").await["state"], "ready");
    running.stop().await;
}

#[tokio::test]
async fn end_of_input_before_the_paste_cancels_the_pairing() {
    let running = start(None, "TEST_SUB_PAIR_EOF").await;
    assert_exit(
        &running
            .run(&["account", "add", "dave", "--kind", "oauth"])
            .await,
        0,
    );
    let (pairing, start) = running.pair(&["dave", "--login", "console"]).await;
    let url = start["authorization_url"].as_str().unwrap();
    assert!(
        url.starts_with("https://platform.claude.com/oauth/authorize?"),
        "--login console reaches the start route: {url}"
    );
    let ended = pairing.end(None).await;
    assert_eq!(ended.code, Some(1), "stderr:\n{}", ended.stderr);
    assert!(ended.answers.is_empty(), "{:?}", ended.answers);
    assert_eq!(
        ended.stderr.lines().last(),
        Some("the pairing was cancelled: input ended before the callback URL was pasted")
    );
    let id = start["id"].as_str().unwrap();
    let session = running.get(&format!("/admin/oauth/sessions/{id}")).await;
    assert_eq!(session["status"], "cancelled", "{session}");
    assert_eq!(
        running.get("/admin/accounts/dave").await["state"],
        "unpaired"
    );
    running.stop().await;
}

#[tokio::test]
async fn an_empty_line_cancels_the_pairing_as_the_end_of_input_does() {
    let running = start(None, "TEST_SUB_PAIR_EMPTY").await;
    assert_exit(
        &running
            .run(&["account", "add", "dave", "--kind", "oauth"])
            .await,
        0,
    );
    let (pairing, start) = running.pair(&["dave"]).await;
    let ended = pairing.end(Some("\r\nnever read\n")).await;
    assert_eq!(ended.code, Some(1), "stderr:\n{}", ended.stderr);
    assert!(ended.answers.is_empty(), "{:?}", ended.answers);
    assert_eq!(
        ended.stderr.lines().last(),
        Some("the pairing was cancelled: input ended before the callback URL was pasted")
    );
    let id = start["id"].as_str().unwrap();
    let session = running.get(&format!("/admin/oauth/sessions/{id}")).await;
    assert_eq!(session["status"], "cancelled", "{session}");
    running.stop().await;
}

#[tokio::test]
async fn no_confirm_stops_after_the_callback_and_account_confirm_finishes_it() {
    let running = start(None, "TEST_SUB_PAIR_NO_CONFIRM").await;
    assert_exit(
        &running
            .run(&["account", "add", "erin", "--kind", "oauth"])
            .await,
        0,
    );
    let (pairing, start) = running.pair(&["erin", "--no-confirm"]).await;
    let id = start["id"].as_str().unwrap().to_owned();
    let paste = format!("{}\n", callback_url(&start, "CODE-erin"));
    let ended = pairing.end(Some(&paste)).await;
    assert_eq!(ended.code, Some(0), "stderr:\n{}", ended.stderr);
    assert_eq!(ended.answers.len(), 1, "{:?}", ended.answers);
    assert_eq!(ended.answers[0]["status"], "exchanged");
    assert_eq!(
        ended.stderr.lines().last(),
        Some(
            format!("pairing {id} exchanged; finish it with: phoebis account confirm {id}")
                .as_str()
        )
    );
    assert!(
        !ended.stderr.contains("paired"),
        "nothing was confirmed: {}",
        ended.stderr
    );
    assert_eq!(
        running.get("/admin/accounts/erin").await["state"],
        "unpaired"
    );

    let confirmed = running.run(&["account", "confirm", &id]).await;
    assert_exit(&confirmed, 0);
    assert_eq!(
        text(&confirmed),
        (String::new(), "paired erin\n".to_owned())
    );
    assert_eq!(running.get("/admin/accounts/erin").await["state"], "ready");

    // A second pairing of the same slot, the paste as `<code> <state>` and the confirm under `--json`.
    let (pairing, start) = running.pair(&["erin", "--no-confirm"]).await;
    let id = start["id"].as_str().unwrap().to_owned();
    let state = start["authorization_url"]
        .as_str()
        .unwrap()
        .split("state=")
        .nth(1)
        .unwrap()
        .to_owned();
    let ended = pairing.end(Some(&format!("CODE-erin {state}\n"))).await;
    assert_eq!(ended.code, Some(0), "stderr:\n{}", ended.stderr);
    assert_eq!(ended.answers.len(), 1, "{:?}", ended.answers);
    assert_eq!(ended.answers[0]["status"], "exchanged");
    assert_eq!(ended.answers[0]["pending"]["email"], FIXTURE_EMAIL);

    let confirmed = running.run(&["account", "confirm", &id, "--json"]).await;
    assert_exit(&confirmed, 0);
    let (stdout, stderr) = text(&confirmed);
    assert_eq!(stderr, "", "nothing but errors on stderr under --json");
    assert_eq!(
        (stdout.matches('\n').count(), stdout.ends_with('\n')),
        (1, true),
        "the answer is one line: {stdout:?}"
    );
    let answer: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(answer["account_id"], "erin");
    assert_eq!(answer["status"], "confirmed");
    running.stop().await;
}

#[tokio::test]
async fn account_confirm_of_an_unknown_pairing_is_404() {
    let running = start(None, "TEST_SUB_CONFIRM_404").await;
    let confirmed = running.run(&["account", "confirm", "nope"]).await;
    assert_exit(&confirmed, 1);
    assert_eq!(
        text(&confirmed),
        (String::new(), "404 pairing session not found\n".to_owned())
    );
    running.stop().await;
}

#[tokio::test]
async fn a_refused_start_prints_the_routes_line_and_exits_one() {
    let running = start(None, "TEST_SUB_PAIR_REFUSED").await;
    let refused = running.run(&["account", "pair", "nobody"]).await;
    assert_exit(&refused, 1);
    assert_eq!(
        text(&refused),
        (String::new(), "404 account not found\n".to_owned())
    );
    running.stop().await;
}

#[tokio::test]
async fn ledger_replay_parked_answers_the_counts() {
    let running = start(None, "TEST_SUB_REPLAY").await;
    let replayed = running.run(&["ledger", "replay-parked"]).await;
    assert_exit(&replayed, 0);
    assert_eq!(
        text(&replayed),
        (String::new(), "landed 0, remaining 0\n".to_owned())
    );
    let answered = running.run(&["ledger", "replay-parked", "--json"]).await;
    assert_exit(&answered, 0);
    assert_eq!(
        text(&answered),
        ("{\"landed\":0,\"remaining\":0}\n".to_owned(), String::new())
    );
    assert_no_secret(&replayed, &[ADMIN_SECRET]);
    assert_no_secret(&answered, &[ADMIN_SECRET]);
    running.stop().await;
}

/// One request as the scripted admin listener read it.
#[derive(Clone, Debug)]
struct Seen {
    line: String,
    content_type: Option<String>,
    body: Vec<u8>,
}

/// An admin listener on loopback that answers each connection with the next scripted response and records the
/// request, so a case sees exactly what a subcommand sent and can answer what no router on this branch answers.
struct ScriptedAdmin {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl ScriptedAdmin {
    async fn start(script: Vec<String>) -> ScriptedAdmin {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        tokio::spawn(async move {
            for response in script {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut read = Vec::new();
                let mut chunk = [0u8; 4096];
                let head_end = loop {
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0, "the client closed before its request head ended");
                    read.extend_from_slice(&chunk[..n]);
                    if let Some(at) = read.windows(4).position(|w| w == b"\r\n\r\n") {
                        break at + 4;
                    }
                };
                let head = String::from_utf8(read[..head_end].to_vec()).unwrap();
                let header = |name: &str| {
                    head.lines().find_map(|line| {
                        let (n, v) = line.split_once(':')?;
                        n.eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
                    })
                };
                let length: usize = header("content-length")
                    .map(|v| v.parse().unwrap())
                    .unwrap_or(0);
                while read.len() < head_end + length {
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0, "the client closed before its body ended");
                    read.extend_from_slice(&chunk[..n]);
                }
                record.lock().unwrap().push(Seen {
                    line: head.lines().next().unwrap_or_default().to_owned(),
                    content_type: header("content-type"),
                    body: read[head_end..head_end + length].to_vec(),
                });
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
            }
        });
        ScriptedAdmin { url, seen }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

/// A JSON answer with `status` and its reason phrase.
fn json_response(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
}

const NO_CONTENT: &str = "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n";

/// `phoebis <args>` against `admin`, with no configuration file.
async fn against(admin: &ScriptedAdmin, args: &[&str]) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let mut args = args.to_vec();
    args.extend(["--url", &admin.url, "--secret-env", "TEST_SUB_SCRIPTED"]);
    phoebis(
        &args,
        &dir.path().join("absent.yaml"),
        &[("TEST_SUB_SCRIPTED", ADMIN_SECRET)],
        &[],
        None,
    )
    .await
}

#[tokio::test]
async fn clear_cooldown_sends_one_delete_for_the_named_scope_and_no_body() {
    let admin = ScriptedAdmin::start(vec![NO_CONTENT.to_owned()]).await;
    let cleared = against(
        &admin,
        &[
            "account",
            "clear-cooldown",
            "alice",
            "--scope",
            "model:claude-sonnet-4-5",
        ],
    )
    .await;
    assert_exit(&cleared, 0);
    assert_eq!(
        text(&cleared),
        (
            String::new(),
            "cleared model:claude-sonnet-4-5 on alice\n".to_owned()
        )
    );
    let seen = admin.seen();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(
        seen[0].line,
        "DELETE /admin/accounts/alice/cooldowns/model:claude-sonnet-4-5 HTTP/1.1"
    );
    assert!(seen[0].body.is_empty(), "{seen:?}");
    assert_eq!(seen[0].content_type, None);
}

#[tokio::test]
async fn key_create_exits_one_naming_the_rotate_route_when_the_answer_carries_no_secret() {
    let admin = ScriptedAdmin::start(vec![json_response(
        "201 Created",
        r#"{"id":"laptop","label":"Laptop","pool_id":"work"}"#,
    )])
    .await;
    let created = against(&admin, &["key", "create", "laptop", "--pool", "work"]).await;
    assert_exit(&created, 1);
    let (stdout, stderr) = text(&created);
    assert_eq!(stdout, "", "no empty line stands in for the secret");
    assert_eq!(
        stderr.lines().last(),
        Some(
            "the secret of key laptop is not in the answer; POST /admin/keys/laptop/rotate issues a new one"
        ),
        "{stderr}"
    );
}
