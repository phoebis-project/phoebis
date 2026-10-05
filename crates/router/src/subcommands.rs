//! The subcommands that create an account, a pool or a key and clear a cooldown over the admin API: each builds the
//! route's body from its flags, calls the route through the admin client and prints the answer, the created id or
//! secret on stdout and the rest on stderr.

use std::collections::BTreeMap;
use std::future::Future;
use std::io::{BufRead, Write};
use std::path::Path;

use hyper::Method;
use serde_json::{json, Map, Value};

use crate::admin_client::{self, AdminClient, Answer, ClientError, Common};

#[derive(clap::Subcommand, Debug)]
pub enum AccountCommand {
    /// Create an account: `POST /admin/accounts`. Prints the id on stdout.
    Add {
        /// The account's id.
        id: String,
        /// `oauth` for a subscription paired afterwards, `api_key` for an upstream key given now.
        #[arg(long, value_enum)]
        kind: AccountKindArg,
        /// The label; the id when absent.
        #[arg(long)]
        label: Option<String>,
        /// The variable holding an api_key account's upstream key; the value never travels in a flag.
        // Its own argument id: `--secret-env` is the admin secret's, in the flattened `Common`, and clap refuses
        // one id on two options.
        #[arg(long, value_name = "NAME", conflicts_with = "secret_stdin")]
        api_key_env: Option<String>,
        /// Read an api_key account's secret from the first line of stdin.
        #[arg(long)]
        secret_stdin: bool,
        #[command(flatten)]
        common: Common,
    },
    /// Clear a cooldown: `DELETE /admin/accounts/{id}/cooldowns/{scope}`.
    ClearCooldown {
        /// The account's id.
        id: String,
        /// `account`, `fast`, `fast_credits` or `model:<canonical>`.
        #[arg(long, default_value = "account")]
        scope: String,
        #[command(flatten)]
        common: Common,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum AccountKindArg {
    #[value(name = "api_key")]
    ApiKey,
    Oauth,
}

impl AccountKindArg {
    fn as_str(self) -> &'static str {
        match self {
            AccountKindArg::ApiKey => "api_key",
            AccountKindArg::Oauth => "oauth",
        }
    }
}

#[derive(clap::Subcommand, Debug)]
pub enum KeyCommand {
    /// Create a key: `POST /admin/keys`. Prints the secret on stdout, once.
    Create {
        /// The key's id.
        id: String,
        /// The pool the key routes to.
        #[arg(long, value_name = "POOL")]
        pool: String,
        /// The label; the body's, else the id, when absent.
        #[arg(long)]
        label: Option<String>,
        /// A JSON object of the route's fields, a file or `-` for stdin; a flag wins over the same field.
        #[arg(long, value_name = "PATH|-")]
        body: Option<String>,
        #[command(flatten)]
        common: Common,
    },
}

#[derive(clap::Subcommand, Debug)]
pub enum PoolCommand {
    /// Create a pool: `POST /admin/pools`. Prints the id on stdout.
    Create {
        /// The pool's id.
        id: String,
        /// The label; the body's, else the id, when absent.
        #[arg(long)]
        label: Option<String>,
        /// A member as `<account>` or `<account>:<priority>`; repeatable.
        #[arg(long, value_name = "ACCOUNT[:PRIORITY]")]
        member: Vec<String>,
        /// How the pool picks among its members.
        #[arg(long, value_enum)]
        strategy: Option<StrategyArg>,
        /// A JSON object of the route's fields, a file or `-` for stdin; a flag wins over the same field.
        #[arg(long, value_name = "PATH|-")]
        body: Option<String>,
        #[command(flatten)]
        common: Common,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum StrategyArg {
    Pace,
    Priority,
    #[value(name = "round_robin")]
    RoundRobin,
}

impl StrategyArg {
    fn as_str(self) -> &'static str {
        match self {
            StrategyArg::Pace => "pace",
            StrategyArg::Priority => "priority",
            StrategyArg::RoundRobin => "round_robin",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SubcommandError {
    #[error("--body {path}: {reason}")]
    Body { path: String, reason: String },
    #[error("{0}")]
    Client(#[from] ClientError),
    #[error("--member {member}: {reason}")]
    Member { member: String, reason: String },
    #[error("an api_key account needs --api-key-env or --secret-stdin")]
    SecretMissing,
    #[error("{what}")]
    SecretUnset { what: String },
}

/// The second line of a refusal to an account write sent with the revision this run read: the account moved since.
const READ_AGAIN: &str =
    "the account changed after this command read it; run the command again to read it afresh";

/// `--body`'s object (a file, or `-` for stdin) with `flags` merged over it, the flags winning, and then `defaults`
/// filling the keys neither the body nor a flag named, so a body's `label` or `members` survives an absent flag.
pub fn merged_body(
    body: Option<&str>,
    stdin: &mut dyn BufRead,
    flags: Map<String, Value>,
    defaults: Map<String, Value>,
) -> Result<Value, SubcommandError> {
    let mut merged = match body {
        None => Map::new(),
        Some(path) => {
            let refused = |reason: String| SubcommandError::Body {
                path: path.to_owned(),
                reason,
            };
            let text = if path == "-" {
                let mut text = String::new();
                stdin
                    .read_to_string(&mut text)
                    .map_err(|error| refused(error.to_string()))?;
                text
            } else {
                std::fs::read_to_string(path).map_err(|error| refused(error.to_string()))?
            };
            match serde_json::from_str(&text).map_err(|error| refused(error.to_string()))? {
                Value::Object(object) => object,
                _ => return Err(refused("not a JSON object".to_owned())),
            }
        }
    };
    merged.extend(flags);
    for (key, value) in defaults {
        merged.entry(key).or_insert(value);
    }
    Ok(Value::Object(merged))
}

/// Runs one subcommand to its exit code, with every output on the two writers so a test reads them.
pub fn run_account(
    cmd: AccountCommand,
    config_path: &Path,
    stdin: &mut dyn BufRead,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let done = match cmd {
        AccountCommand::Add {
            id,
            kind,
            label,
            api_key_env,
            secret_stdin,
            common,
        } => add_account(
            NewAccount {
                id: &id,
                kind,
                label,
                api_key_env: api_key_env.as_deref(),
                secret_stdin,
            },
            &common,
            config_path,
            stdin,
            out,
            err,
        ),
        AccountCommand::ClearCooldown { id, scope, common } => {
            clear_cooldown(&id, &scope, &common, config_path, err)
        }
    };
    finish(done, out, err)
}

pub fn run_key(
    cmd: KeyCommand,
    config_path: &Path,
    stdin: &mut dyn BufRead,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let done = create_key(cmd, config_path, stdin, out, err);
    finish(done, out, err)
}

pub fn run_pool(
    cmd: PoolCommand,
    config_path: &Path,
    stdin: &mut dyn BufRead,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let done = create_pool(cmd, config_path, stdin, out, err);
    finish(done, out, err)
}

/// One answer printed the subcommand's way: `--json` puts the body on `out` verbatim; otherwise `headline` (the id
/// or the secret) goes to `out` and every other field, `key: value` per line and sorted, to `err`.
pub fn print_answer(
    answer: &Answer,
    headline: &str,
    hidden: &[&str],
    json: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) {
    // A reader that went away does not undo what the route did, so a failed write leaves the exit code the route's.
    if json {
        if !answer.body.is_null() {
            let printed = serde_json::to_string_pretty(&answer.body).unwrap_or_default();
            let _ = writeln!(out, "{printed}");
        }
        return;
    }
    let _ = writeln!(out, "{headline}");
    let fields: BTreeMap<&str, &Value> = answer
        .body
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, _)| !hidden.contains(&key.as_str()))
        .map(|(key, value)| (key.as_str(), value))
        .collect();
    for (key, value) in fields {
        let _ = match value {
            Value::String(text) => writeln!(err, "{key}: {text}"),
            other => writeln!(err, "{key}: {other}"),
        };
    }
}

/// Why a subcommand stopped, kept apart from its printing so each step can use `?`.
enum Failed {
    Runtime(std::io::Error),
    /// An account write sent with the revision this run read was refused with a conflict: the account moved since.
    Stale(ClientError),
    Subcommand(SubcommandError),
}

impl Failed {
    fn print(self, err: &mut dyn Write) -> i32 {
        let _ = match self {
            Failed::Runtime(error) => writeln!(err, "cannot start the runtime: {error}"),
            Failed::Stale(error) => writeln!(err, "{error}\n{READ_AGAIN}"),
            Failed::Subcommand(error) => writeln!(err, "{error}"),
        };
        1
    }
}

impl From<std::io::Error> for Failed {
    fn from(error: std::io::Error) -> Self {
        Failed::Runtime(error)
    }
}

impl<E: Into<SubcommandError>> From<E> for Failed {
    fn from(error: E) -> Self {
        Failed::Subcommand(error.into())
    }
}

/// `account add`'s own flags; the api_key secret is named by its source and never sits in a flag.
struct NewAccount<'a> {
    id: &'a str,
    kind: AccountKindArg,
    label: Option<String>,
    api_key_env: Option<&'a str>,
    secret_stdin: bool,
}

fn add_account(
    new: NewAccount<'_>,
    common: &Common,
    config_path: &Path,
    stdin: &mut dyn BufRead,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), Failed> {
    let client = AdminClient::from_flags(common, config_path)?;
    let secret = account_secret(new.kind, new.api_key_env, new.secret_stdin, stdin)?;
    let mut body = json!({
        "id": new.id,
        "kind": new.kind.as_str(),
        "label": new.label.unwrap_or_else(|| new.id.to_owned()),
    });
    // A create has no row to fence on, so no `expected_revision` travels with it.
    if let Some(secret) = secret {
        body["secret"] = Value::String(secret);
    }
    let answer = block_on(client.expect(Method::POST, "/admin/accounts", Some(&body)))??;
    let created = answer.body["id"].as_str().unwrap_or(new.id).to_owned();
    print_answer(&answer, &created, &[], common.json, out, err);
    Ok(())
}

fn clear_cooldown(
    id: &str,
    scope: &str,
    common: &Common,
    config_path: &Path,
    err: &mut dyn Write,
) -> Result<(), Failed> {
    let client = AdminClient::from_flags(common, config_path)?;
    let account = format!("/admin/accounts/{}", segment(id));
    let cooldown = format!("{account}/cooldowns/{}", segment(scope));
    let (fenced, cleared) = block_on(async {
        let read = client.expect(Method::GET, &account, None).await?;
        // The revision this run read travels with the write, so a route that fences account writes on it refuses one
        // built on a stale read; an answer without `revision` comes from a router that takes no fence, so none is sent.
        let fence = read
            .body
            .get("revision")
            .filter(|revision| revision.is_u64())
            .map(|revision| json!({ "expected_revision": revision }));
        let cleared = client
            .expect(Method::DELETE, &cooldown, fence.as_ref())
            .await;
        Ok::<_, ClientError>((fence.is_some(), cleared))
    })??;
    match cleared {
        Err(error @ ClientError::Refused { status: 409, .. }) if fenced => {
            Err(Failed::Stale(error))
        }
        Err(error) => Err(error.into()),
        Ok(_) => {
            if !common.json {
                let _ = writeln!(err, "cleared {scope} on {id}");
            }
            Ok(())
        }
    }
}

fn create_key(
    cmd: KeyCommand,
    config_path: &Path,
    stdin: &mut dyn BufRead,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), Failed> {
    let KeyCommand::Create {
        id,
        pool,
        label,
        body,
        common,
    } = cmd;
    let client = AdminClient::from_flags(&common, config_path)?;
    let mut flags = Map::new();
    flags.insert("id".into(), json!(id));
    flags.insert("pool_id".into(), json!(pool));
    if let Some(label) = label {
        flags.insert("label".into(), json!(label));
    }
    let defaults = Map::from_iter([("label".to_owned(), json!(id))]);
    let body = merged_body(body.as_deref(), stdin, flags, defaults)?;
    let answer = block_on(client.expect(Method::POST, "/admin/keys", Some(&body)))??;
    let secret = answer.body["secret"].as_str().unwrap_or_default();
    print_answer(&answer, secret, &["secret"], common.json, out, err);
    Ok(())
}

fn create_pool(
    cmd: PoolCommand,
    config_path: &Path,
    stdin: &mut dyn BufRead,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), Failed> {
    let PoolCommand::Create {
        id,
        label,
        member: members,
        strategy,
        body,
        common,
    } = cmd;
    let client = AdminClient::from_flags(&common, config_path)?;
    let mut flags = Map::new();
    flags.insert("id".into(), json!(id));
    if let Some(label) = label {
        flags.insert("label".into(), json!(label));
    }
    if let Some(strategy) = strategy {
        flags.insert("strategy".into(), json!(strategy.as_str()));
    }
    if !members.is_empty() {
        let members = members
            .iter()
            .map(|spec| member(spec))
            .collect::<Result<Vec<_>, _>>()?;
        flags.insert("members".into(), Value::Array(members));
    }
    let defaults = Map::from_iter([
        ("label".to_owned(), json!(id)),
        ("members".to_owned(), json!([])),
    ]);
    let body = merged_body(body.as_deref(), stdin, flags, defaults)?;
    let answer = block_on(client.expect(Method::POST, "/admin/pools", Some(&body)))??;
    let created = answer.body["id"].as_str().unwrap_or(&id).to_owned();
    print_answer(&answer, &created, &[], common.json, out, err);
    Ok(())
}

/// `member`'s `<account>` or `<account>:<priority>` as the route's member object, the priority 0 when absent.
fn member(spec: &str) -> Result<Value, SubcommandError> {
    let (account, priority) = match spec.split_once(':') {
        None => (spec, 0),
        Some((account, priority)) => (
            account,
            priority
                .parse::<u32>()
                .map_err(|_| SubcommandError::Member {
                    member: spec.to_owned(),
                    reason: "the priority after the colon is not a whole number".to_owned(),
                })?,
        ),
    };
    Ok(json!({ "account_id": account, "priority": priority }))
}

/// The api_key secret from the named variable or stdin's first line, trimmed; `None` for an oauth account given
/// neither, which the route pairs later.
fn account_secret(
    kind: AccountKindArg,
    api_key_env: Option<&str>,
    secret_stdin: bool,
    stdin: &mut dyn BufRead,
) -> Result<Option<String>, SubcommandError> {
    let secret = if let Some(name) = api_key_env {
        // The value is never part of the refusal: a variable set to something unusable still holds a secret.
        let value = std::env::var(name).unwrap_or_default();
        if value.trim().is_empty() {
            return Err(SubcommandError::SecretUnset {
                what: format!("environment variable {name} is unset or empty"),
            });
        }
        Some(value.trim().to_owned())
    } else if secret_stdin {
        let mut line = String::new();
        stdin
            .read_line(&mut line)
            .map_err(|error| SubcommandError::SecretUnset {
                what: format!("--secret-stdin: {error}"),
            })?;
        if line.trim().is_empty() {
            return Err(SubcommandError::SecretUnset {
                what: "--secret-stdin read no secret from stdin".to_owned(),
            });
        }
        Some(line.trim().to_owned())
    } else {
        None
    };
    if kind == AccountKindArg::ApiKey && secret.is_none() {
        return Err(SubcommandError::SecretMissing);
    }
    Ok(secret)
}

/// One path segment, percent-encoded, so an id or a scope names its own route and never another route or a query.
fn segment(raw: &str) -> String {
    let mut encoded = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~:".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Runs `call` on the subcommand runtime and shuts the runtime down in the background rather than dropping it: a
/// host name resolves on its blocking pool, a lookup the deadline abandoned keeps that thread, and a drop would wait
/// for it past `--timeout`. The caller's `process::exit` ends the thread.
fn block_on<T>(call: impl Future<Output = T>) -> std::io::Result<T> {
    let runtime = admin_client::runtime()?;
    let output = runtime.block_on(call);
    runtime.shutdown_background();
    Ok(output)
}

fn finish(done: Result<(), Failed>, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let code = match done {
        Ok(()) => 0,
        Err(failed) => failed.print(err),
    };
    let _ = out.flush();
    let _ = err.flush();
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            other => panic!("{other} is not an object"),
        }
    }

    #[test]
    fn the_flags_win_over_the_files_keys_and_a_dash_reads_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("key.json");
        std::fs::write(&file, r#"{"label": "File", "notes": "from the file"}"#).unwrap();
        let path = file.to_str().unwrap();
        let merged = merged_body(
            Some(path),
            &mut &b""[..],
            map(json!({"id": "laptop", "label": "Flag"})),
            map(json!({"label": "laptop"})),
        )
        .unwrap();
        assert_eq!(
            merged,
            json!({"label": "Flag", "notes": "from the file", "id": "laptop"})
        );

        let merged = merged_body(
            Some("-"),
            &mut &br#"{"notes": "from stdin", "pool_id": "home"}"#[..],
            map(json!({"pool_id": "work"})),
            Map::new(),
        )
        .unwrap();
        assert_eq!(merged, json!({"notes": "from stdin", "pool_id": "work"}));

        for (body, input, reason) in [
            (path, &b""[..], None),
            ("-", &b"[1, 2]"[..], Some("not a JSON object")),
            ("-", &b"{"[..], Some("EOF while parsing")),
        ] {
            let result = merged_body(Some(body), &mut &input[..], Map::new(), Map::new());
            match reason {
                None => assert!(result.is_ok()),
                Some(reason) => {
                    let line = result.unwrap_err().to_string();
                    assert!(
                        line.starts_with(&format!("--body {body}: ")) && line.contains(reason),
                        "{line}"
                    );
                }
            }
        }
        let missing = dir.path().join("missing.json");
        let line = merged_body(
            Some(missing.to_str().unwrap()),
            &mut &b""[..],
            Map::new(),
            Map::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(
            line.starts_with(&format!("--body {}: ", missing.display())),
            "{line}"
        );
    }

    #[test]
    fn a_body_value_survives_an_absent_flag_and_loses_to_a_present_one() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pool.json");
        std::fs::write(
            &file,
            r#"{"label": "Lab", "members": [{"account_id": "a", "priority": 2}]}"#,
        )
        .unwrap();
        let empty = dir.path().join("empty.json");
        std::fs::write(&empty, "{}").unwrap();
        let defaults = || map(json!({"label": "lab", "members": []}));
        let merged = |body: &Path, flags: Value| {
            merged_body(
                Some(body.to_str().unwrap()),
                &mut &b""[..],
                map(flags),
                defaults(),
            )
            .unwrap()
        };

        let kept = merged(&file, json!({}));
        assert_eq!(kept["label"], "Lab");
        assert_eq!(kept["members"], json!([{"account_id": "a", "priority": 2}]));
        assert_eq!(merged(&file, json!({"label": "X"}))["label"], "X");
        let filled = merged(&empty, json!({}));
        assert_eq!(filled["label"], "lab");
        assert_eq!(filled["members"], json!([]));
    }

    #[test]
    fn a_bare_member_is_priority_zero_and_a_priority_that_is_no_number_is_refused() {
        assert_eq!(
            member("alice").unwrap(),
            json!({"account_id": "alice", "priority": 0})
        );
        assert_eq!(
            member("alice:3").unwrap(),
            json!({"account_id": "alice", "priority": 3})
        );
        for bad in ["a:x", "a:-1", "a:"] {
            let line = member(bad).unwrap_err().to_string();
            assert!(line.starts_with(&format!("--member {bad}: ")), "{line}");
        }
    }

    #[test]
    fn print_answer_hides_the_secret_and_prints_the_rest_sorted_by_field() {
        let answer = Answer {
            status: 201,
            body: json!({
                "secret": "phk_live_not_a_real_secret",
                "label": "Laptop",
                "id": "laptop",
                "enabled": true,
                "pool-id": "work",
                "pool": {"id": "work"},
                "model_rules": [],
            }),
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        print_answer(
            &answer,
            "phk_live_not_a_real_secret",
            &["secret"],
            false,
            &mut out,
            &mut err,
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "phk_live_not_a_real_secret\n"
        );
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "enabled: true\nid: laptop\nlabel: Laptop\nmodel_rules: []\npool: {\"id\":\"work\"}\npool-id: work\n"
        );

        let (mut out, mut err) = (Vec::new(), Vec::new());
        print_answer(&answer, "ignored", &["secret"], true, &mut out, &mut err);
        let printed: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(printed, answer.body, "--json prints the answer whole");
        assert!(err.is_empty());
    }

    #[test]
    fn an_api_key_account_needs_a_secret_and_an_empty_source_is_named() {
        const UNSET: &str = "PHOEBIS_SUBCOMMANDS_TEST_NEVER_SET";
        let none = &mut &b""[..];
        assert!(matches!(
            account_secret(AccountKindArg::ApiKey, None, false, none),
            Err(SubcommandError::SecretMissing)
        ));
        assert_eq!(
            account_secret(AccountKindArg::Oauth, None, false, none).unwrap(),
            None
        );
        let line = account_secret(AccountKindArg::ApiKey, Some(UNSET), false, none)
            .unwrap_err()
            .to_string();
        assert_eq!(
            line,
            format!("environment variable {UNSET} is unset or empty")
        );
        assert_eq!(
            account_secret(
                AccountKindArg::ApiKey,
                None,
                true,
                &mut &b"  sk-ant-api03-from-stdin \nsecond line\n"[..]
            )
            .unwrap()
            .as_deref(),
            Some("sk-ant-api03-from-stdin")
        );
        let line = account_secret(AccountKindArg::ApiKey, None, true, &mut &b"\n"[..])
            .unwrap_err()
            .to_string();
        assert_eq!(line, "--secret-stdin read no secret from stdin");
    }

    #[test]
    fn a_path_segment_is_percent_encoded_so_it_names_one_route() {
        assert_eq!(segment("alice"), "alice");
        assert_eq!(
            segment("model:claude-sonnet-4-5"),
            "model:claude-sonnet-4-5"
        );
        assert_eq!(segment("a/b?c d"), "a%2Fb%3Fc%20d");
    }
}
