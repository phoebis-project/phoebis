//! `account add`, `account clear-cooldown`, `pool create` and `key create` from the outside: the binary against a
//! router started in-process on loopback, over plain HTTP and over TLS, with the file, without it, and with a refused
//! secret; and `clear-cooldown` against a scripted admin listener for the account revision it sends.

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fake_upstream::{generate_pair, FakeUpstream, GeneratedPair, Respond, Rule};
use hyper::Method;
use phoebis_router::admin_client::{AdminClient, Common};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// `load` and `from_flags` read the admin secret from the process environment, so each test holds this across its
/// set-then-read sequence.
static ENV: Mutex<()> = Mutex::new(());

/// How long a boot may take to bind its listeners; generous because it is reached only when the boot never ends.
const SETTLE: Duration = Duration::from_secs(30);

const ADMIN_SECRET: &str = "admin-secret-subcommands-001";

/// An upstream key the fake upstream's discovery rule answers 200 for.
const UPSTREAM_KEY: &str = "sk-ant-api03-subcommands-fake-key";

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
}

/// Starts the router with `admin_pair`, when given, as `admin.tls`, the admin secret read from `secret_env`, and an
/// upstream whose discovery answers 200, and returns once the admin listener accepts; a start that ends first fails
/// the test with its own error rather than at the settle deadline.
async fn start(admin_pair: Option<&GeneratedPair>, secret_env: &str) -> Running {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/models".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"data":[{"id":"claude-sonnet-4-5","type":"model"}]}"#.to_vec(),
        },
    }])
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

/// One request as the scripted admin listener read it.
#[derive(Clone, Debug)]
struct Seen {
    line: String,
    content_type: Option<String>,
    body: Vec<u8>,
}

/// An admin listener on loopback that answers each connection with the next scripted response and records the
/// request, standing in for a router whose account answer carries a field this branch's routes do not.
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

/// A JSON answer with `status` and its reason phrase, plus any `extra` header lines.
fn json_response(status: &str, extra: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n{extra}\r\n{body}",
        body.len()
    )
}

const NO_CONTENT: &str = "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n";

/// `account clear-cooldown alice` against `admin`, with no configuration file.
async fn clear_against(admin: &ScriptedAdmin) -> Output {
    let dir = tempfile::tempdir().unwrap();
    phoebis(
        &[
            "account",
            "clear-cooldown",
            "alice",
            "--url",
            &admin.url,
            "--secret-env",
            "TEST_SUB_SCRIPTED",
        ],
        &dir.path().join("absent.yaml"),
        &[("TEST_SUB_SCRIPTED", ADMIN_SECRET)],
        &[],
        None,
    )
    .await
}

#[tokio::test]
async fn clear_cooldown_sends_the_revision_the_account_answer_carries_and_nothing_without_one() {
    let with = ScriptedAdmin::start(vec![
        json_response(
            "200 OK",
            "",
            r#"{"id":"alice","kind":"oauth","revision":7}"#,
        ),
        NO_CONTENT.to_owned(),
    ])
    .await;
    let cleared = clear_against(&with).await;
    assert_exit(&cleared, 0);
    assert_eq!(
        text(&cleared),
        (String::new(), "cleared account on alice\n".to_owned())
    );
    let seen = with.seen();
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert!(
        seen[0].line.starts_with("GET /admin/accounts/alice "),
        "{seen:?}"
    );
    assert!(
        seen[1]
            .line
            .starts_with("DELETE /admin/accounts/alice/cooldowns/account "),
        "{seen:?}"
    );
    assert_eq!(seen[1].content_type.as_deref(), Some("application/json"));
    let sent: Value = serde_json::from_slice(&seen[1].body).unwrap();
    assert_eq!(sent, serde_json::json!({"expected_revision": 7}));

    let without = ScriptedAdmin::start(vec![
        json_response("200 OK", "", r#"{"id":"alice","kind":"oauth"}"#),
        NO_CONTENT.to_owned(),
    ])
    .await;
    let cleared = clear_against(&without).await;
    assert_exit(&cleared, 0);
    let seen = without.seen();
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert!(
        seen[1]
            .line
            .starts_with("DELETE /admin/accounts/alice/cooldowns/account "),
        "{seen:?}"
    );
    assert!(
        seen[1].body.is_empty(),
        "nothing is sent without a revision: {seen:?}"
    );
    assert_eq!(seen[1].content_type, None);
}

#[tokio::test]
async fn a_stale_revision_prints_the_refusal_and_says_to_read_the_account_again() {
    let stale = ScriptedAdmin::start(vec![
        json_response(
            "200 OK",
            "",
            r#"{"id":"alice","kind":"oauth","revision":7}"#,
        ),
        json_response(
            "409 Conflict",
            "x-phoebis-error-code: stale_edit\r\n",
            r#"{"error":"the account changed while the edit was being saved"}"#,
        ),
    ])
    .await;
    let refused = clear_against(&stale).await;
    assert_exit(&refused, 1);
    let (stdout, stderr) = text(&refused);
    assert_eq!(stdout, "");
    let lines: Vec<&str> = stderr.lines().collect();
    assert_eq!(
        lines.first().copied(),
        Some("409 the account changed while the edit was being saved"),
        "{stderr}"
    );
    assert_eq!(
        lines.get(1).copied(),
        Some("the account changed after this command read it; run the command again to read it afresh"),
        "{stderr}"
    );
    assert_eq!(lines.len(), 2, "{stderr}");
}
