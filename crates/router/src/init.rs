//! `phoebis init`: a first installation's storage key, environment file and bootstrap file, written once. Each file is
//! created with `create_new` at its mode, so no run replaces a key every stored credential is sealed under, and a run
//! that fails part way removes what it wrote.

use std::ffi::{CStr, CString};
use std::fs::DirBuilder;
use std::io::{BufRead, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

use base64::Engine;
use phoebis_store::permissions::{create_private_dir, create_private_file};
use phoebis_store::StorageKey;
use rand::Rng;

use crate::config::{ConfigError, ListenerTls};

/// The text the bootstrap file is filled from, embedded since the binary runs where no checkout exists.
const EXAMPLE: &str = include_str!("../../../deploy/config.example.yaml");

/// The paths inside the image and the metrics address `deploy/compose.yaml` publishes.
const CONTAINER_DATA_DIR: &str = "/var/lib/phoebis";
const CONTAINER_KEY_FILE: &str = "/run/secrets/router-storage-key";
const CONTAINER_METRICS_LISTEN: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 9464);

/// One public listener's flags and the words its prompts and refusals use.
struct Side {
    name: &'static str,
    listen_flag: &'static str,
    tls_flag: &'static str,
    cert_flag: &'static str,
    key_flag: &'static str,
    asked_address: &'static str,
    asked_choice: &'static str,
    asked_cert: &'static str,
    asked_key: &'static str,
    loopback: SocketAddr,
    container: SocketAddr,
}

const PROXY: Side = Side {
    name: "proxy",
    listen_flag: "proxy-listen",
    tls_flag: "proxy-tls",
    cert_flag: "proxy-cert",
    key_flag: "proxy-key",
    asked_address: "the proxy address",
    asked_choice: "the proxy TLS choice",
    asked_cert: "the proxy certificate file",
    asked_key: "the proxy key file",
    loopback: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8317),
    container: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8317),
};

const ADMIN: Side = Side {
    name: "admin",
    listen_flag: "admin-listen",
    tls_flag: "admin-tls",
    cert_flag: "admin-cert",
    key_flag: "admin-key",
    asked_address: "the admin address",
    asked_choice: "the admin TLS choice",
    asked_cert: "the admin certificate file",
    asked_key: "the admin key file",
    loopback: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8081),
    container: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8081),
};

/// The flags of `phoebis init`; a value no flag gives takes its default, or is asked for on a terminal.
#[derive(clap::Args, Debug)]
pub struct InitArgs {
    /// The admin listener's address; 127.0.0.1:8081 when absent.
    #[arg(long, value_name = "ADDR")]
    pub admin_listen: Option<SocketAddr>,
    /// The admin listener's `tls` value, which an address off loopback needs.
    #[arg(long, value_name = "self_signed|plain_http", conflicts_with_all = ["admin_cert", "admin_key"])]
    pub admin_tls: Option<TlsChoice>,
    /// The admin listener's certificate chain, PEM, leaf first; `check-config` and the start read it, `init` does not.
    #[arg(long, value_name = "PATH", requires = "admin_key")]
    pub admin_cert: Option<PathBuf>,
    /// The admin listener's private key, PEM.
    #[arg(long, value_name = "PATH", requires = "admin_cert")]
    pub admin_key: Option<PathBuf>,
    /// Writes the compose directory's shape: the container's paths inside the file, `storage.key` and `phoebis.env`
    /// beside it, both listeners on every interface over plain HTTP.
    #[arg(long, conflicts_with_all = ["data_dir", "env_file", "key_file"])]
    pub container: bool,
    /// The directory of both databases and the self-signed material; `data` beside the bootstrap file when absent.
    #[arg(long, value_name = "PATH")]
    pub data_dir: Option<PathBuf>,
    /// Writes the admin secret here, with `RUST_LOG=info`, rather than printing it.
    #[arg(long, value_name = "PATH")]
    pub env_file: Option<PathBuf>,
    /// The storage key file; `storage.key` beside the bootstrap file when absent.
    #[arg(long, value_name = "PATH")]
    pub key_file: Option<PathBuf>,
    /// Asks nothing, even on a terminal; a value a flag did not give takes its default or refuses.
    #[arg(long)]
    pub non_interactive: bool,
    /// The user that owns the key file and the data directory; needs root.
    #[arg(long, value_name = "USER")]
    pub owner: Option<String>,
    /// The proxy listener's address; 127.0.0.1:8317 when absent.
    #[arg(long, value_name = "ADDR")]
    pub proxy_listen: Option<SocketAddr>,
    /// The proxy listener's `tls` value, which an address off loopback needs.
    #[arg(long, value_name = "self_signed|plain_http", conflicts_with_all = ["proxy_cert", "proxy_key"])]
    pub proxy_tls: Option<TlsChoice>,
    /// The proxy listener's certificate chain, PEM, leaf first; `check-config` and the start read it, `init` does not.
    #[arg(long, value_name = "PATH", requires = "proxy_key")]
    pub proxy_cert: Option<PathBuf>,
    /// The proxy listener's private key, PEM.
    #[arg(long, value_name = "PATH", requires = "proxy_cert")]
    pub proxy_key: Option<PathBuf>,
    /// The environment variable that holds the admin secret, written as `admin.secret_env`.
    #[arg(long, value_name = "NAME", default_value = "PHOEBIS_ADMIN_SECRET", value_parser = env_name)]
    pub secret_env: String,
}

/// The word a listener's TLS flag takes; an operator's pair is the two path flags instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum TlsChoice {
    #[value(name = "self_signed")]
    SelfSigned,
    #[value(name = "plain_http")]
    PlainHttp,
}

impl From<TlsChoice> for ListenerTls {
    fn from(choice: TlsChoice) -> ListenerTls {
        match choice {
            TlsChoice::PlainHttp => ListenerTls::PlainHttp,
            TlsChoice::SelfSigned => ListenerTls::SelfSigned,
        }
    }
}

/// One listener as `init` will write it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listener {
    pub listen: SocketAddr,
    pub tls: Option<ListenerTls>,
}

/// Everything `init` is about to write, resolved from the flags, the prompts and the defaults, before a byte lands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub admin: Listener,
    pub config_path: PathBuf,
    pub container: bool,
    pub data_dir: PathBuf,
    pub env_file: Option<PathBuf>,
    pub key_file: PathBuf,
    pub owner: Option<String>,
    pub proxy: Listener,
    pub secret_env: String,
}

#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error("{path} already exists; init never overwrites a configuration")]
    ConfigExists { path: PathBuf },
    #[error(
        "{path} exists and a new key cannot open it; keep the key that sealed it, or name an empty --data-dir"
    )]
    DatabaseExists { path: PathBuf },
    #[error("{path} already exists; init never overwrites an environment file")]
    EnvExists { path: PathBuf },
    #[error("input ended before {asked} was answered")]
    InputEnded { asked: &'static str },
    #[error("{what}: {error}")]
    Io { what: String, error: std::io::Error },
    #[error(
        "{path} already exists; init never replaces a key, since every stored credential is sealed under it"
    )]
    KeyExists { path: PathBuf },
    #[error("--owner names no user: {user}")]
    NoSuchUser { user: String },
    #[error("--owner needs root")]
    OwnerNeedsRoot,
    #[error(
        "--{flag} {listen} leaves loopback; name --{flag_tls} self_signed or plain_http, or --{flag_cert} and \
         --{flag_key}"
    )]
    TlsChoiceMissing {
        flag: &'static str,
        flag_tls: &'static str,
        flag_cert: &'static str,
        flag_key: &'static str,
        listen: SocketAddr,
    },
    /// A step after the first write failed, and what the run had written is removed again, newest first.
    #[error("{error}; {}", undone(.removed, .kept))]
    Undone {
        error: Box<InitError>,
        removed: Vec<PathBuf>,
        kept: Vec<PathBuf>,
    },
    #[error("the written configuration does not load: {error}")]
    Written { error: ConfigError },
}

fn undone(removed: &[PathBuf], kept: &[PathBuf]) -> String {
    let list = |paths: &[PathBuf]| {
        paths
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut text = format!("removed what this run wrote: {}", list(removed));
    if !kept.is_empty() {
        text.push_str(&format!("; could not remove {}", list(kept)));
    }
    text
}

/// The prompts, over any reader and writer so a test feeds answers: the proxy address, the admin address, and the
/// TLS choice for each that leaves loopback. An empty answer takes the default, a bad one is asked again with the
/// parse error, end of input refuses.
pub fn ask(
    args: &InitArgs,
    defaults: &Plan,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> Result<Plan, InitError> {
    let mut plan = defaults.clone();
    // The container's listeners bind every interface by design, and the host's publication is the choice that
    // matters, so the shape asks nothing.
    if args.container {
        return Ok(plan);
    }
    if args.proxy_listen.is_none() {
        plan.proxy.listen = ask_address(&PROXY, defaults.proxy.listen, input, output)?;
    }
    if args.admin_listen.is_none() {
        plan.admin.listen = ask_address(&ADMIN, defaults.admin.listen, input, output)?;
    }
    for (side, listener) in [(&PROXY, &mut plan.proxy), (&ADMIN, &mut plan.admin)] {
        if leaves_loopback(listener) && listener.tls.is_none() {
            listener.tls = Some(ask_tls(side, listener.listen, input, output)?);
        }
    }
    Ok(plan)
}

fn ask_address(
    side: &Side,
    default: SocketAddr,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> Result<SocketAddr, InitError> {
    loop {
        prompt(
            output,
            &format!("{} listen address [{default}]: ", side.name),
        )?;
        let answer = answer(input, output, side.asked_address)?;
        if answer.is_empty() {
            return Ok(default);
        }
        match answer.parse() {
            Ok(listen) => return Ok(listen),
            Err(error) => say(output, &error.to_string())?,
        }
    }
}

fn ask_tls(
    side: &Side,
    listen: SocketAddr,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> Result<ListenerTls, InitError> {
    loop {
        prompt(
            output,
            &format!(
                "{} on {listen} leaves loopback; tls: self_signed, plain_http or files [self_signed]: ",
                side.name
            ),
        )?;
        match answer(input, output, side.asked_choice)?.as_str() {
            "" | "self_signed" => return Ok(ListenerTls::SelfSigned),
            "plain_http" => return Ok(ListenerTls::PlainHttp),
            "files" => {
                let cert_file = ask_path(
                    &format!("{} certificate file, PEM, leaf first: ", side.name),
                    side.asked_cert,
                    input,
                    output,
                )?;
                let key_file = ask_path(
                    &format!("{} key file, PEM: ", side.name),
                    side.asked_key,
                    input,
                    output,
                )?;
                return Ok(ListenerTls::Pair {
                    cert_file,
                    key_file,
                });
            }
            _ => say(output, "answer self_signed, plain_http or files")?,
        }
    }
}

fn ask_path(
    text: &str,
    asked: &'static str,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> Result<PathBuf, InitError> {
    loop {
        prompt(output, text)?;
        let answer = answer(input, output, asked)?;
        if !answer.is_empty() {
            return absolute(Path::new(&answer));
        }
        say(output, "a path is required")?;
    }
}

fn prompt(output: &mut dyn Write, text: &str) -> Result<(), InitError> {
    output
        .write_all(text.as_bytes())
        .and_then(|()| output.flush())
        .map_err(io_error("writing a prompt".to_string()))
}

fn say(output: &mut dyn Write, line: &str) -> Result<(), InitError> {
    writeln!(output, "{line}").map_err(io_error("writing a prompt".to_string()))
}

fn answer(
    input: &mut dyn BufRead,
    output: &mut dyn Write,
    asked: &'static str,
) -> Result<String, InitError> {
    let mut line = String::new();
    let read = input
        .read_line(&mut line)
        .map_err(io_error(format!("reading {asked}")))?;
    if read == 0 {
        // The prompt leaves the cursor after its text, so the refusal that follows starts on a line of its own.
        say(output, "")?;
        return Err(InitError::InputEnded { asked });
    }
    Ok(line.trim().to_string())
}

/// The flags and the defaults as a plan; `interactive` is `stdin` and `stderr` both terminals and no
/// `--non-interactive`, and decides whether a missing TLS choice asks or refuses.
pub fn plan(config_path: &Path, args: &InitArgs, interactive: bool) -> Result<Plan, InitError> {
    let config_path = absolute(config_path)?;
    let dir = parent_of(&config_path).to_path_buf();
    let given = |flag: &Option<PathBuf>, default: PathBuf| match flag {
        Some(path) => absolute(path),
        None => Ok(default),
    };
    let (data_dir, key_file, env_file) = if args.container {
        (
            PathBuf::from(CONTAINER_DATA_DIR),
            dir.join("storage.key"),
            Some(dir.join("phoebis.env")),
        )
    } else {
        (
            given(&args.data_dir, dir.join("data"))?,
            given(&args.key_file, dir.join("storage.key"))?,
            args.env_file.as_deref().map(absolute).transpose()?,
        )
    };
    let plan = Plan {
        admin: listener(
            &ADMIN,
            args.admin_listen,
            args.admin_tls,
            (&args.admin_cert, &args.admin_key),
            args.container,
        )?,
        config_path,
        container: args.container,
        data_dir,
        env_file,
        key_file,
        owner: args.owner.clone(),
        proxy: listener(
            &PROXY,
            args.proxy_listen,
            args.proxy_tls,
            (&args.proxy_cert, &args.proxy_key),
            args.container,
        )?,
        secret_env: args.secret_env.clone(),
    };
    if !interactive {
        for (side, listener) in [(&PROXY, &plan.proxy), (&ADMIN, &plan.admin)] {
            if leaves_loopback(listener) && listener.tls.is_none() {
                return Err(InitError::TlsChoiceMissing {
                    flag: side.listen_flag,
                    flag_tls: side.tls_flag,
                    flag_cert: side.cert_flag,
                    flag_key: side.key_flag,
                    listen: listener.listen,
                });
            }
        }
    }
    Ok(plan)
}

fn listener(
    side: &Side,
    listen: Option<SocketAddr>,
    choice: Option<TlsChoice>,
    pair: (&Option<PathBuf>, &Option<PathBuf>),
    container: bool,
) -> Result<Listener, InitError> {
    let tls = match (choice, pair) {
        (Some(choice), _) => Some(choice.into()),
        (None, (Some(cert_file), Some(key_file))) => Some(ListenerTls::Pair {
            cert_file: absolute(cert_file)?,
            key_file: absolute(key_file)?,
        }),
        // The container network is plain HTTP by default; the host decides what it publishes.
        _ if container => Some(ListenerTls::PlainHttp),
        _ => None,
    };
    let default = if container {
        side.container
    } else {
        side.loopback
    };
    Ok(Listener {
        listen: listen.unwrap_or(default),
        tls,
    })
}

fn leaves_loopback(listener: &Listener) -> bool {
    !listener.listen.ip().is_loopback()
}

fn absolute(path: &Path) -> Result<PathBuf, InitError> {
    std::path::absolute(path).map_err(io_error(format!("resolving {}", path.display())))
}

fn parent_of(path: &Path) -> &Path {
    path.parent().unwrap_or(Path::new("/"))
}

fn io_error(what: String) -> impl FnOnce(std::io::Error) -> InitError {
    move |error| InitError::Io { what, error }
}

/// An environment variable name a shell and systemd's `EnvironmentFile` both read, since the name starts the
/// environment file's line as given.
fn env_name(value: &str) -> Result<String, String> {
    let mut chars = value.chars();
    let valid = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    match valid {
        true => Ok(value.to_string()),
        false => Err(
            "a variable name is letters, digits and underscores, not starting with a digit"
                .to_string(),
        ),
    }
}

/// The example file's text with the plan's values filled in, by line, every other line as the example spells it.
pub fn render(plan: &Plan) -> String {
    fill(EXAMPLE, &fills(plan))
        .unwrap_or_else(|key| panic!("the embedded example has no single line for {key}"))
}

/// One key the renderer fills: the top-level block it sits in, and the lines that replace the example's line.
struct Fill {
    block: &'static str,
    key: &'static str,
    lines: Vec<String>,
}

fn fills(plan: &Plan) -> Vec<Fill> {
    let path = |path: &Path| scalar(&path.to_string_lossy());
    let (key_file, pricing) = if plan.container {
        (
            PathBuf::from(CONTAINER_KEY_FILE),
            plan.data_dir.join("pricing.yaml"),
        )
    } else {
        // The service recipe installs the price table beside the bootstrap file, since the loader's relative default
        // would resolve against whatever directory starts the process.
        (
            plan.key_file.clone(),
            parent_of(&plan.config_path).join("pricing.yaml"),
        )
    };
    let mut fills = vec![
        listen_fill("admin", &plan.admin),
        value_fill("admin", "secret_env", scalar(&plan.secret_env)),
        listen_fill("proxy", &plan.proxy),
        value_fill(
            "stats",
            "ledger_path",
            path(&plan.data_dir.join("ledger.sqlite")),
        ),
        value_fill("stats", "pricing_path", path(&pricing)),
        value_fill("storage", "encryption_key_file", path(&key_file)),
        value_fill(
            "storage",
            "path",
            path(&plan.data_dir.join("router.sqlite")),
        ),
    ];
    if plan.container {
        fills.push(value_fill(
            "stats",
            "metrics_listen",
            CONTAINER_METRICS_LISTEN.to_string(),
        ));
    }
    fills
}

fn value_fill(block: &'static str, key: &'static str, value: String) -> Fill {
    Fill {
        block,
        key,
        lines: vec![format!("  {key}: {value}")],
    }
}

fn listen_fill(block: &'static str, listener: &Listener) -> Fill {
    let mut lines = vec![format!(
        "  listen: {}",
        scalar(&listener.listen.to_string())
    )];
    match &listener.tls {
        None => {}
        Some(ListenerTls::PlainHttp) => lines.push("  tls: plain_http".to_string()),
        Some(ListenerTls::SelfSigned) => lines.push("  tls: self_signed".to_string()),
        Some(ListenerTls::Pair {
            cert_file,
            key_file,
        }) => lines.extend([
            "  tls:".to_string(),
            format!("    cert_file: {}", scalar(&cert_file.to_string_lossy())),
            format!("    key_file: {}", scalar(&key_file.to_string_lossy())),
        ]),
    }
    Fill {
        block,
        key: "listen",
        lines,
    }
}

/// `example` with each fill's line replaced by the fill's lines and every other line as the example spells it, or
/// the `block.key` of a fill that matched no line or more than one, so a drift in the example never reaches a file.
fn fill(example: &str, fills: &[Fill]) -> Result<String, String> {
    let mut found = vec![0usize; fills.len()];
    let mut text = String::with_capacity(example.len() + 512);
    let mut block = "";
    for line in example.lines() {
        if let Some(name) = line
            .strip_suffix(':')
            .filter(|name| !name.starts_with([' ', '#']))
        {
            block = name;
        }
        let at = fills.iter().position(|fill| {
            fill.block == block
                && line
                    .strip_prefix("  ")
                    .and_then(|rest| rest.strip_prefix(fill.key))
                    .is_some_and(|rest| rest.starts_with(':'))
        });
        let lines = match at {
            Some(at) => {
                found[at] += 1;
                fills[at].lines.iter().map(String::as_str).collect()
            }
            None => vec![line],
        };
        for line in lines {
            text.push_str(line);
            text.push('\n');
        }
    }
    match found.iter().position(|count| *count != 1) {
        Some(at) => Err(format!("{}.{}", fills[at].block, fills[at].key)),
        None => Ok(text),
    }
}

/// `value` as a YAML scalar: plain where it reads back as the same string, double-quoted otherwise, so a path holding
/// ` #`, a bracketed IPv6 address or a word YAML would read as a number loads as written.
fn scalar(value: &str) -> String {
    let plain = matches!(
        serde_norway::from_str::<serde_norway::Value>(value),
        Ok(serde_norway::Value::String(read)) if read == value
    );
    if plain {
        return value.to_string();
    }
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for c in value.chars() {
        match c {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            c if c.is_control() => quoted.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

/// A path this run created, so a failure removes it and `--owner` hands over the directories that are the run's own.
struct Landed {
    dir: bool,
    path: PathBuf,
}

/// Writes the plan; a step that fails removes what this run wrote and names it. `secret_out` is stdout and gets
/// the one `<NAME>=<secret>` line when no environment file is written; `report` is stderr.
pub fn run(
    plan: &Plan,
    secret_out: &mut dyn Write,
    report: &mut dyn Write,
) -> Result<(), InitError> {
    refuse_existing(plan)?;
    let owner = match &plan.owner {
        Some(user) => Some(owner_ids(user)?),
        None => None,
    };
    let secret = admin_secret();
    let mut written = Vec::new();
    // The secret is printed and the summary written only once every file is in place, so an exit other than 0
    // leaves nothing behind and a printed secret always belongs to a configuration that landed.
    write(plan, &secret, owner, &mut written)
        .and_then(|()| {
            let summary = summary(plan, &written);
            if plan.env_file.is_none() {
                writeln!(secret_out, "{}={secret}", plan.secret_env)
                    .and_then(|()| secret_out.flush())
                    .map_err(io_error("printing the admin secret".to_string()))?;
            }
            report
                .write_all(summary.as_bytes())
                .and_then(|()| report.flush())
                .map_err(io_error("writing the summary".to_string()))
        })
        .map_err(|error| undo(error, written))
}

fn refuse_existing(plan: &Plan) -> Result<(), InitError> {
    if exists(&plan.config_path)? {
        return Err(InitError::ConfigExists {
            path: plan.config_path.clone(),
        });
    }
    if exists(&plan.key_file)? {
        return Err(InitError::KeyExists {
            path: plan.key_file.clone(),
        });
    }
    if let Some(env_file) = &plan.env_file {
        if exists(env_file)? {
            return Err(InitError::EnvExists {
                path: env_file.clone(),
            });
        }
    }
    // A registry sealed under a key this run would replace is the key-loss case the operations page recovers by
    // hand; a new key over it would only make every sealed row unreadable for good. The container's data directory
    // is the volume's path inside the image, which the host does not have.
    if !plan.container {
        for name in ["router.sqlite", "ledger.sqlite"] {
            let path = plan.data_dir.join(name);
            if exists(&path)? {
                return Err(InitError::DatabaseExists { path });
            }
        }
    }
    Ok(())
}

/// Whether anything sits at `path`, a dangling link included, since `create_new` refuses that too.
fn exists(path: &Path) -> Result<bool, InitError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(InitError::Io {
            what: format!("checking {}", path.display()),
            error,
        }),
    }
}

fn write(
    plan: &Plan,
    secret: &str,
    owner: Option<(u32, u32)>,
    written: &mut Vec<Landed>,
) -> Result<(), InitError> {
    // Every missing parent is created here, one component at a time from the root down, rather than by the store
    // helper's recursive create, so the run knows which directories are its own to hand over and to remove again.
    let mut parents = vec![parent_of(&plan.key_file)];
    parents.extend(plan.env_file.as_deref().map(parent_of));
    if !plan.container {
        parents.push(parent_of(&plan.data_dir));
    }
    parents.push(parent_of(&plan.config_path));
    for dir in parents {
        create_missing(dir, written)?;
    }

    let key = StorageKey::generate().to_hex();
    write_file(&plan.key_file, 0o600, &[key.as_bytes(), b"\n"], written)?;
    if let Some(env_file) = &plan.env_file {
        let lines = format!("{}={secret}\nRUST_LOG=info\n", plan.secret_env);
        write_file(env_file, 0o600, &[lines.as_bytes()], written)?;
    }
    if !plan.container {
        let existed = exists(&plan.data_dir)?;
        create_private_dir(&plan.data_dir)
            .map_err(io_error(format!("creating {}", plan.data_dir.display())))?;
        if !existed {
            written.push(Landed {
                dir: true,
                path: plan.data_dir.clone(),
            });
        }
    }
    write_file(
        &plan.config_path,
        0o644,
        &[render(plan).as_bytes()],
        written,
    )?;
    crate::config::load_unresolved(&plan.config_path)
        .map_err(|error| InitError::Written { error })?;
    if let Some((uid, gid)) = owner {
        hand_over(plan, uid, gid, written)?;
    }
    Ok(())
}

fn create_missing(dir: &Path, written: &mut Vec<Landed>) -> Result<(), InitError> {
    let mut missing = Vec::new();
    for ancestor in dir.ancestors() {
        if exists(ancestor)? {
            break;
        }
        missing.push(ancestor);
    }
    for dir in missing.into_iter().rev() {
        DirBuilder::new()
            .mode(0o700)
            .create(dir)
            .map_err(io_error(format!("creating {}", dir.display())))?;
        written.push(Landed {
            dir: true,
            path: dir.to_path_buf(),
        });
    }
    Ok(())
}

fn write_file(
    path: &Path,
    mode: u32,
    parts: &[&[u8]],
    written: &mut Vec<Landed>,
) -> Result<(), InitError> {
    let mut file = create_private_file(path, mode)
        .map_err(io_error(format!("creating {}", path.display())))?;
    written.push(Landed {
        dir: false,
        path: path.to_path_buf(),
    });
    for part in parts {
        file.write_all(part)
            .map_err(io_error(format!("writing {}", path.display())))?;
    }
    file.sync_all()
        .map_err(io_error(format!("writing {}", path.display())))
}

/// The key file, then the data directory and every other directory this run created, go to the owner and its primary
/// group, since the service must traverse them. A directory that existed keeps its owner, so a `--data-dir` naming a
/// directory the host already uses is never handed over, and the configuration and environment files stay the
/// caller's.
fn hand_over(plan: &Plan, uid: u32, gid: u32, written: &[Landed]) -> Result<(), InitError> {
    let (data_dir, others): (Vec<&Path>, Vec<&Path>) = written
        .iter()
        .filter(|landed| landed.dir)
        .map(|landed| landed.path.as_path())
        .partition(|path| *path == plan.data_dir);
    let paths = std::iter::once(plan.key_file.as_path())
        .chain(data_dir)
        .chain(others);
    for path in paths {
        std::os::unix::fs::chown(path, Some(uid), Some(gid))
            .map_err(io_error(format!("handing {} to the owner", path.display())))?;
    }
    Ok(())
}

fn undo(error: InitError, written: Vec<Landed>) -> InitError {
    if written.is_empty() {
        return error;
    }
    let (mut removed, mut kept) = (Vec::new(), Vec::new());
    for landed in written.into_iter().rev() {
        let gone = match landed.dir {
            true => std::fs::remove_dir(&landed.path),
            false => std::fs::remove_file(&landed.path),
        };
        match gone {
            Ok(()) => removed.push(landed.path),
            Err(_) => kept.push(landed.path),
        }
    }
    InitError::Undone {
        error: Box::new(error),
        removed,
        kept,
    }
}

fn summary(plan: &Plan, written: &[Landed]) -> String {
    let mut text = String::new();
    for landed in written {
        let Ok(metadata) = std::fs::symlink_metadata(&landed.path) else {
            continue;
        };
        let (mode, owner) = (metadata.mode() & 0o777, user_name(metadata.uid()));
        let line = match landed.dir {
            true => format!(
                "created {} (directory, mode {mode:04o}, owner {owner})\n",
                landed.path.display()
            ),
            false => format!(
                "wrote {} (mode {mode:04o}, owner {owner})\n",
                landed.path.display()
            ),
        };
        text.push_str(&line);
    }
    match &plan.env_file {
        Some(env_file) => text.push_str(&format!(
            "the admin secret is in {}, written to {}\n",
            plan.secret_env,
            env_file.display()
        )),
        None => text.push_str(&format!(
            "the admin secret is in {}, printed once on stdout\n",
            plan.secret_env
        )),
    }
    let self_signed = [&plan.proxy.tls, &plan.admin.tls]
        .into_iter()
        .any(|tls| matches!(tls, Some(ListenerTls::SelfSigned)));
    if self_signed {
        text.push_str(&format!(
            "the first start writes the CA and the leaf under {}/; ca.pem there is the file each client trusts \
             through NODE_EXTRA_CA_CERTS\n",
            plan.data_dir.join("tls").display()
        ));
    }
    if plan.container {
        // The key path inside the file is the container's, so a check on the host cannot read it.
        text.push_str(
            "remaining: own storage.key by uid 65532 at mode 0400; copy deploy/compose.yaml with the image tag set\n",
        );
        text.push_str("next: docker compose run --rm phoebis check-config\n");
    } else {
        text.push_str(&format!(
            "next: phoebis check-config --config {}\n",
            plan.config_path.display()
        ));
    }
    text
}

fn admin_secret() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn owner_ids(user: &str) -> Result<(u32, u32), InitError> {
    if unsafe { libc::geteuid() } != 0 {
        return Err(InitError::OwnerNeedsRoot);
    }
    let no_such_user = || InitError::NoSuchUser {
        user: user.to_string(),
    };
    let name = CString::new(user).map_err(|_| no_such_user())?;
    let entry = passwd(|entry, buffer, size, found| {
        // SAFETY: every pointer is live for the call, and `size` is the buffer's length.
        unsafe { libc::getpwnam_r(name.as_ptr(), entry, buffer, size, found) }
    })
    .map_err(io_error(format!("looking up the user {user}")))?;
    entry
        .map(|entry| (entry.uid, entry.gid))
        .ok_or_else(no_such_user)
}

fn user_name(uid: u32) -> String {
    let entry = passwd(|entry, buffer, size, found| {
        // SAFETY: every pointer is live for the call, and `size` is the buffer's length.
        unsafe { libc::getpwuid_r(uid, entry, buffer, size, found) }
    });
    match entry {
        Ok(Some(entry)) => entry.name,
        _ => format!("uid {uid}"),
    }
}

struct PasswdEntry {
    gid: u32,
    name: String,
    uid: u32,
}

/// One reentrant `getpw*_r` lookup, growing the buffer while the entry does not fit; `None` when no entry matches.
fn passwd(
    lookup: impl Fn(
        *mut libc::passwd,
        *mut libc::c_char,
        libc::size_t,
        *mut *mut libc::passwd,
    ) -> libc::c_int,
) -> std::io::Result<Option<PasswdEntry>> {
    let mut size = 1024;
    loop {
        let mut buffer: Vec<libc::c_char> = vec![0; size];
        // SAFETY: `passwd` is a plain C struct, for which all zeroes is a valid value; the lookup fills it.
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        match lookup(&mut entry, buffer.as_mut_ptr(), buffer.len(), &mut found) {
            0 if found.is_null() => return Ok(None),
            0 => {
                // SAFETY: on success `pw_name` points at a NUL-terminated string inside `buffer`, still alive here.
                let name = unsafe { CStr::from_ptr(entry.pw_name) }
                    .to_string_lossy()
                    .into_owned();
                return Ok(Some(PasswdEntry {
                    gid: entry.pw_gid,
                    name,
                    uid: entry.pw_uid,
                }));
            }
            libc::ERANGE if size < 1 << 20 => size *= 2,
            // Some C libraries answer a missing entry with an error rather than with a null result.
            libc::ENOENT | libc::ESRCH => return Ok(None),
            code => return Err(std::io::Error::from_raw_os_error(code)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: InitArgs,
    }

    fn args(flags: &[&str]) -> InitArgs {
        Cli::parse_from(std::iter::once("init").chain(flags.iter().copied())).args
    }

    #[test]
    fn every_key_the_renderer_fills_exists_once_in_the_example() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.yaml");
        let shapes = [
            args(&[]),
            args(&["--container"]),
            args(&[
                "--proxy-listen",
                "0.0.0.0:8317",
                "--proxy-tls",
                "self_signed",
                "--admin-cert",
                "c.pem",
                "--admin-key",
                "k.pem",
            ]),
        ];
        for shape in &shapes {
            let plan = plan(&config, shape, false).unwrap();
            fill(EXAMPLE, &fills(&plan)).unwrap();
        }
        // A drift in the example fails here: the rewriter refuses a key it found no line for, or found twice, rather
        // than render a file without the value.
        let container = fills(&plan(&config, &args(&["--container"]), false).unwrap());
        let lost = EXAMPLE.replace("  ledger_path: ./data/ledger.sqlite\n", "");
        assert_ne!(lost, EXAMPLE);
        assert_eq!(
            fill(&lost, &container),
            Err("stats.ledger_path".to_string())
        );
        let doubled = EXAMPLE.replace(
            "  metrics_listen: 127.0.0.1:9464\n",
            "  metrics_listen: 127.0.0.1:9464\n  metrics_listen: 127.0.0.1:9464\n",
        );
        assert_ne!(doubled, EXAMPLE);
        assert_eq!(
            fill(&doubled, &container),
            Err("stats.metrics_listen".to_string())
        );
    }

    #[test]
    fn the_rendered_file_loads_and_carries_the_plans_values() {
        let dir = tempfile::tempdir().unwrap();
        let shapes = [
            ("loopback", args(&[])),
            (
                "off-loopback",
                args(&[
                    "--proxy-listen",
                    "0.0.0.0:8317",
                    "--proxy-tls",
                    "self_signed",
                    "--admin-listen",
                    "[::1]:9081",
                    "--admin-cert",
                    "c.pem",
                    "--admin-key",
                    "k.pem",
                    "--secret-env",
                    "OTHER_SECRET",
                    "--data-dir",
                    "odd # dir",
                ]),
            ),
            ("container", args(&["--container"])),
        ];
        for (name, shape) in &shapes {
            let config = dir.path().join(name).join("config.yaml");
            let plan = plan(&config, shape, false).unwrap();
            std::fs::create_dir_all(config.parent().unwrap()).unwrap();
            std::fs::write(&config, render(&plan)).unwrap();
            let loaded = crate::config::load_unresolved(&config)
                .unwrap_or_else(|error| panic!("{name}: {error}\n{}", render(&plan)));
            assert_eq!(loaded.proxy.listen, plan.proxy.listen, "{name}");
            assert_eq!(loaded.proxy.tls, plan.proxy.tls, "{name}");
            assert_eq!(loaded.admin.listen, plan.admin.listen, "{name}");
            assert_eq!(loaded.admin.tls, plan.admin.tls, "{name}");
            assert_eq!(loaded.admin.secret_env, plan.secret_env, "{name}");
            assert_eq!(
                loaded.storage.path,
                plan.data_dir.join("router.sqlite"),
                "{name}"
            );
            assert_eq!(
                loaded.stats.ledger_path,
                plan.data_dir.join("ledger.sqlite"),
                "{name}"
            );
            let (key_file, pricing, metrics) = if plan.container {
                (
                    PathBuf::from("/run/secrets/router-storage-key"),
                    PathBuf::from("/var/lib/phoebis/pricing.yaml"),
                    "0.0.0.0:9464",
                )
            } else {
                (
                    plan.key_file.clone(),
                    dir.path().join(name).join("pricing.yaml"),
                    "127.0.0.1:9464",
                )
            };
            assert_eq!(loaded.storage.encryption_key_file, key_file, "{name}");
            assert_eq!(loaded.stats.pricing_path, Some(pricing), "{name}");
            assert_eq!(
                loaded.stats.metrics_listen,
                metrics.parse::<SocketAddr>().unwrap(),
                "{name}"
            );
        }
        let off = plan(
            &dir.path().join("off-loopback").join("config.yaml"),
            &shapes[1].1,
            false,
        )
        .unwrap();
        assert!(off.data_dir.ends_with("odd # dir"), "{off:?}");
        assert!(
            matches!(&off.admin.tls, Some(ListenerTls::Pair { cert_file, key_file })
                if cert_file.is_absolute() && key_file.is_absolute()),
            "{off:?}"
        );
    }

    #[test]
    fn an_empty_answer_takes_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let flags = args(&[]);
        let defaults = plan(&dir.path().join("config.yaml"), &flags, true).unwrap();
        let mut output = Vec::new();
        let asked = ask(&flags, &defaults, &mut "\n\n".as_bytes(), &mut output).unwrap();
        assert_eq!(asked, defaults);
        let output = String::from_utf8(output).unwrap();
        assert!(
            output.contains("proxy listen address [127.0.0.1:8317]: "),
            "{output}"
        );
        assert!(
            output.contains("admin listen address [127.0.0.1:8081]: "),
            "{output}"
        );
        assert!(!output.contains("tls"), "{output}");
    }

    #[test]
    fn an_answer_that_is_not_an_address_is_asked_again_with_the_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let flags = args(&[]);
        let defaults = plan(&dir.path().join("config.yaml"), &flags, true).unwrap();
        let mut output = Vec::new();
        let asked = ask(
            &flags,
            &defaults,
            &mut "not an address\n127.0.0.1:9317\n\n".as_bytes(),
            &mut output,
        )
        .unwrap();
        assert_eq!(asked.proxy.listen, "127.0.0.1:9317".parse().unwrap());
        assert_eq!(asked.admin, defaults.admin);
        let output = String::from_utf8(output).unwrap();
        let prompt = "proxy listen address [127.0.0.1:8317]: ";
        let first = output.find(prompt).unwrap();
        let error = output.find("invalid socket address syntax").unwrap();
        let second = output.rfind(prompt).unwrap();
        assert!(first < error && error < second, "{output}");
    }

    #[test]
    fn end_of_input_refuses_naming_what_was_asked() {
        let dir = tempfile::tempdir().unwrap();
        let flags = args(&[]);
        let defaults = plan(&dir.path().join("config.yaml"), &flags, true).unwrap();
        for (input, expected) in [("", "the proxy address"), ("\n", "the admin address")] {
            let error = ask(&flags, &defaults, &mut input.as_bytes(), &mut Vec::new()).unwrap_err();
            assert!(
                matches!(error, InitError::InputEnded { asked } if asked == expected),
                "{input:?}: {error}"
            );
        }
    }

    #[test]
    fn an_address_off_loopback_is_asked_for_its_tls_choice() {
        let dir = tempfile::tempdir().unwrap();
        let flags = args(&[]);
        let defaults = plan(&dir.path().join("config.yaml"), &flags, true).unwrap();
        let mut output = Vec::new();
        let asked = ask(
            &flags,
            &defaults,
            &mut "0.0.0.0:8317\n0.0.0.0:8081\nbogus\n\nfiles\ncert.pem\nkey.pem\n".as_bytes(),
            &mut output,
        )
        .unwrap();
        assert_eq!(asked.proxy.tls, Some(ListenerTls::SelfSigned));
        let here = std::env::current_dir().unwrap();
        assert_eq!(
            asked.admin.tls,
            Some(ListenerTls::Pair {
                cert_file: here.join("cert.pem"),
                key_file: here.join("key.pem"),
            })
        );
        let output = String::from_utf8(output).unwrap();
        assert!(
            output.contains("answer self_signed, plain_http or files"),
            "{output}"
        );
        assert!(output.contains("[self_signed]: "), "{output}");
    }

    #[test]
    fn a_listener_off_loopback_without_a_choice_refuses_off_a_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let flags = args(&["--admin-listen", "0.0.0.0:8081"]);
        let error = plan(&dir.path().join("config.yaml"), &flags, false).unwrap_err();
        assert_eq!(
            error.to_string(),
            "--admin-listen 0.0.0.0:8081 leaves loopback; name --admin-tls self_signed or plain_http, or \
             --admin-cert and --admin-key"
        );
        assert!(plan(&dir.path().join("config.yaml"), &flags, true).is_ok());
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }
}
