//! The subcommands that create an account, a pool or a key, pair an account and clear a cooldown over the admin API:
//! each builds the route's body from its flags, calls the route through the admin client and prints the answer, the
//! created id or secret on stdout and the rest on stderr. A pairing reads the callback URL from stdin between its calls
//! and talks to the operator on stderr.

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
    /// Finish a pairing that `account pair --no-confirm` left exchanged: `POST /admin/oauth/sessions/{id}/confirm`.
    Confirm {
        /// The pairing id `account pair --no-confirm` printed.
        pairing_id: String,
        #[command(flatten)]
        common: Common,
    },
    /// Pair an OAuth account: start, paste the callback URL, confirm.
    Pair {
        /// The account's id.
        id: String,
        /// `claude_ai` for a Claude.ai login, `console` for a Console one.
        #[arg(long, value_enum, default_value = "claude_ai")]
        login: LoginArg,
        /// Stop after the callback and print the pairing id with the `account confirm` line that finishes it.
        #[arg(long)]
        no_confirm: bool,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum LoginArg {
    #[value(name = "claude_ai")]
    ClaudeAi,
    Console,
}

impl LoginArg {
    fn as_str(self) -> &'static str {
        match self {
            LoginArg::ClaudeAi => "claude_ai",
            LoginArg::Console => "console",
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
        )
        .map(|()| 0),
        AccountCommand::ClearCooldown { id, scope, common } => {
            clear_cooldown(&id, &scope, &common, config_path, err).map(|()| 0)
        }
        AccountCommand::Confirm { pairing_id, common } => {
            AdminClient::from_flags(&common, config_path)
                .map_err(Failed::from)
                .and_then(|client| {
                    block_on(confirm(&client, &pairing_id, common.json, out, err))
                        .map_err(Failed::from)
                })
        }
        AccountCommand::Pair {
            id,
            login,
            no_confirm,
            common,
        } => AdminClient::from_flags(&common, config_path)
            .map_err(Failed::from)
            .and_then(|client| {
                block_on(pair(
                    &client,
                    &id,
                    login,
                    no_confirm,
                    common.json,
                    stdin,
                    out,
                    err,
                ))
                .map_err(Failed::from)
            }),
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
    let done = create_key(cmd, config_path, stdin, out, err).map(|()| 0);
    finish(done, out, err)
}

pub fn run_pool(
    cmd: PoolCommand,
    config_path: &Path,
    stdin: &mut dyn BufRead,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let done = create_pool(cmd, config_path, stdin, out, err).map(|()| 0);
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
            let printed = serde_json::to_string(&answer.body).unwrap_or_default();
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
    /// A created key whose secret never reached stdout; `reason` says why, and the secret itself is never in it.
    SecretLost {
        id: String,
        reason: String,
    },
    Subcommand(SubcommandError),
}

impl Failed {
    fn print(self, err: &mut dyn Write) -> i32 {
        let _ = match self {
            Failed::Runtime(error) => writeln!(err, "cannot start the runtime: {error}"),
            Failed::SecretLost { id, reason } => writeln!(
                err,
                "the secret of key {id} {reason}; POST /admin/keys/{id}/rotate issues a new one"
            ),
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
    let path = format!(
        "/admin/accounts/{}/cooldowns/{}",
        segment(id),
        segment(scope)
    );
    block_on(client.expect(Method::DELETE, &path, None))??;
    if !common.json {
        let _ = writeln!(err, "cleared {scope} on {id}");
    }
    Ok(())
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
    let lost = |reason: String| Failed::SecretLost {
        id: answer.body["id"].as_str().unwrap_or(&id).to_owned(),
        reason,
    };
    let Some(secret) = answer.body["secret"].as_str() else {
        return Err(lost("is not in the answer".to_owned()));
    };
    // The secret is printed once and the router keeps only its digest, so stdout taking it is the command's result:
    // a write that fails there loses the key's one copy, unlike an id or a field, which the router still answers.
    let mut checked = Checked {
        inner: out,
        failed: None,
    };
    print_answer(&answer, secret, &["secret"], common.json, &mut checked, err);
    let flushed = checked.flush();
    match checked.failed.take().map_or(flushed, Err) {
        Ok(()) => Ok(()),
        Err(error) => Err(lost(format!("did not reach stdout ({error})"))),
    }
}

/// A writer that keeps its first error, so a caller of `print_answer`, which goes on past a failed write, can still
/// tell whether stdout took what was printed.
struct Checked<'a> {
    inner: &'a mut dyn Write,
    failed: Option<std::io::Error>,
}

impl Write for Checked<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf);
        match &written {
            // `write_all` retries an interrupted write, so only a failure it cannot retry costs the reader its copy.
            Err(error)
                if error.kind() != std::io::ErrorKind::Interrupted && self.failed.is_none() =>
            {
                self.failed = Some(std::io::Error::new(error.kind(), error.to_string()));
            }
            _ => {}
        }
        written
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
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

/// The three-route flow. `input` is stdin: one line, the callback URL or `<code> <state>`; its end before the
/// line cancels the pairing. Answers the exit code; `out` gets nothing but the `--json` answers.
// The flow's flags and its three streams, each passed as `run_account` holds it; a struct for them would be built at
// the one call site and read once.
#[allow(clippy::too_many_arguments)]
pub async fn pair(
    client: &AdminClient,
    id: &str,
    login: LoginArg,
    no_confirm: bool,
    json: bool,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let body = json!({ "account_id": id, "login": login.as_str() });
    let started = match client
        .expect(Method::POST, "/admin/oauth/sessions", Some(&body))
        .await
    {
        Ok(answer) => answer,
        Err(error) => return refused(error, err),
    };
    answered(json, &started, out);
    let (Some(pairing), Some(url)) = (
        started.body["id"].as_str(),
        started.body["authorization_url"].as_str(),
    ) else {
        let _ = writeln!(
            err,
            "the start answer carries no pairing id or authorization URL"
        );
        return 1;
    };
    let _ = writeln!(
        err,
        "open this URL, sign in, and paste the callback URL here:"
    );
    let _ = writeln!(err, "{url}");
    let _ = err.flush();

    let mut line = String::new();
    let pasted = match input.read_line(&mut line) {
        Ok(_) => {
            paste(&line).ok_or_else(|| "input ended before the callback URL was pasted".to_owned())
        }
        Err(error) => Err(format!("stdin could not be read ({error})")),
    };
    let pasted = match pasted {
        Ok(pasted) => pasted,
        Err(reason) => {
            let path = format!("/admin/oauth/sessions/{}", segment(pairing));
            // The pairing holds the account until its deadline, so the delete is worth trying; its failure does not
            // change the outcome, which is the cancel either way.
            if let Err(error) = client.expect(Method::DELETE, &path, None).await {
                let _ = writeln!(
                    err,
                    "the router did not take the cancel ({error}); the pairing ends at its deadline"
                );
            }
            let _ = writeln!(err, "the pairing was cancelled: {reason}");
            return 1;
        }
    };

    // A refused callback is left to expire: a code the exchange rejected already ended the pairing at the router,
    // and a mistyped paste is the operator's to retry from a fresh start.
    let path = format!("/admin/oauth/sessions/{}/callback", segment(pairing));
    let exchanged = match client.expect(Method::POST, &path, Some(&pasted)).await {
        Ok(answer) => answer,
        Err(error) => return refused(error, err),
    };
    answered(json, &exchanged, out);
    let pending = &exchanged.body["pending"];
    let email = pending["email"].as_str().unwrap_or_default();
    let _ = match pending["organization_name"].as_str() {
        Some(organization) => writeln!(err, "pending: {email} ({organization})"),
        None => writeln!(err, "pending: {email}"),
    };
    if no_confirm {
        let _ = writeln!(
            err,
            "pairing {pairing} exchanged; finish it with: phoebis account confirm {pairing}"
        );
        return 0;
    }

    let path = format!("/admin/oauth/sessions/{}/confirm", segment(pairing));
    let confirmed = match client.expect(Method::POST, &path, None).await {
        Ok(answer) => answer,
        Err(error) => return refused(error, err),
    };
    answered(json, &confirmed, out);
    let _ = writeln!(err, "paired {id} as {email}");
    0
}

/// The confirm route alone, for a pairing a bearer started and left exchanged. Answers the exit code.
pub async fn confirm(
    client: &AdminClient,
    pairing_id: &str,
    json: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let path = format!("/admin/oauth/sessions/{}/confirm", segment(pairing_id));
    match client.expect(Method::POST, &path, None).await {
        Ok(answer) if json => {
            answered(true, &answer, out);
            0
        }
        Ok(answer) => {
            let account = answer.body["account_id"].as_str().unwrap_or_default();
            let _ = writeln!(err, "paired {account}");
            0
        }
        Err(error) => refused(error, err),
    }
}

/// Under `--json`, the answer as its one line on `out`, flushed: a reader of a piped stdout holds the start answer
/// while `pair` waits on stdin, and a line left in the buffer there would wait as long.
fn answered(json: bool, answer: &Answer, out: &mut dyn Write) {
    if json {
        print_answer(answer, "", &[], true, out, &mut std::io::sink());
        let _ = out.flush();
    }
}

fn refused(error: ClientError, err: &mut dyn Write) -> i32 {
    let _ = writeln!(err, "{error}");
    error.exit_code()
}

/// The callback's body from one pasted line: `<code> <state>` for a line with exactly one space and no `://`, the
/// URL otherwise; `None` for an empty line, which cancels as the end of input does.
fn paste(line: &str) -> Option<Value> {
    // A Windows terminal pastes a carriage return before the newline.
    let line = line.strip_suffix('\n').unwrap_or(line);
    let line = line.strip_suffix('\r').unwrap_or(line);
    if line.is_empty() {
        return None;
    }
    match line.split_once(' ') {
        Some((code, state)) if !state.contains(' ') && !line.contains("://") => {
            Some(json!({ "code": code, "state": state }))
        }
        _ => Some(json!({ "url": line })),
    }
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

fn finish(done: Result<i32, Failed>, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let code = match done {
        Ok(code) => code,
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
        let printed = String::from_utf8(out).unwrap();
        assert_eq!(
            printed,
            format!("{}\n", answer.body),
            "--json prints the answer whole, on one line"
        );
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
    fn a_paste_with_one_space_and_no_scheme_is_a_code_and_a_state_and_any_other_line_a_url() {
        let url = "http://localhost:4123/callback?code=CODE-a&state=st";
        for line in [format!("{url}\n"), format!("{url}\r\n"), url.to_owned()] {
            assert_eq!(paste(&line), Some(json!({ "url": url })), "{line:?}");
        }
        assert_eq!(
            paste("CODE-a#st st\r\n"),
            Some(json!({ "code": "CODE-a#st", "state": "st" }))
        );
        for other in ["http://localhost:4123/callback?code=a b", "a b c", "CODE-a"] {
            assert_eq!(
                paste(&format!("{other}\n")),
                Some(json!({ "url": other })),
                "{other:?}"
            );
        }
        for empty in ["", "\n", "\r\n"] {
            assert_eq!(paste(empty), None, "{empty:?}");
        }
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
