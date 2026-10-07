//! `phoebis init`: a first installation's storage key, environment file, certificate material and bootstrap file,
//! written once. Each file is created with `create_new` at its mode, so no run replaces a key every stored credential
//! is sealed under, and a run that fails part way removes what it wrote. `--tls self-signed` against an existing
//! bootstrap file is the one run that replaces anything: the leaves it wrote, under the CA the clients already trust.

use std::ffi::{CStr, CString};
use std::fs::{DirBuilder, File, OpenOptions, Permissions};
use std::io::{BufRead, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use base64::Engine;
use phoebis_store::permissions::{create_private_dir, create_private_file};
use phoebis_store::StorageKey;
use rand::Rng;

use crate::config::{ConfigError, ListenerTls};
use crate::selfsigned::{
    self, LeafModes, MaterialError, Placed, SanName, Unfinished, Validity, CA_CERT, CA_KEY,
    CA_LIFETIME_DAYS, LEAF_CERT, LEAF_KEY, LEAF_LIFETIME_DAYS,
};

/// The text the bootstrap file is filled from, embedded since the binary runs where no checkout exists.
const EXAMPLE: &str = include_str!("../../../deploy/config.example.yaml");

/// The paths inside the image and the metrics address `deploy/compose.yaml` publishes.
const CONTAINER_DATA_DIR: &str = "/var/lib/phoebis";
const CONTAINER_KEY_FILE: &str = "/run/secrets/router-storage-key";
const CONTAINER_METRICS_LISTEN: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 9464);
/// Where a generated pair sits inside the container, which the written file names under `--container`.
const CONTAINER_TLS_DIR: &str = "/etc/phoebis/tls";

const DEFAULT_SECRET_ENV: &str = "PHOEBIS_ADMIN_SECRET";

/// One public listener's name, its default addresses, and the words its prompts and refusals use.
struct Side {
    name: &'static str,
    asked_address: &'static str,
    asked_choice: &'static str,
    asked_cert: &'static str,
    asked_key: &'static str,
    loopback: SocketAddr,
    container: SocketAddr,
}

const PROXY: Side = Side {
    name: "proxy",
    asked_address: "the proxy address",
    asked_choice: "the proxy TLS choice",
    asked_cert: "the proxy certificate file",
    asked_key: "the proxy key file",
    loopback: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8317),
    container: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8317),
};

const ADMIN: Side = Side {
    name: "admin",
    asked_address: "the admin address",
    asked_choice: "the admin TLS choice",
    asked_cert: "the admin certificate file",
    asked_key: "the admin key file",
    loopback: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8081),
    container: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8081),
};

/// What a listener off loopback without a pair gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum TlsMode {
    /// The pair flags name every network listener's pair; one they do not name is refused.
    Files,
    /// No `tls` line for a listener without a pair, on any address: the default.
    #[value(name = "none")]
    Plain,
    /// A local CA and one leaf per listener that leaves loopback, under `tls/` beside the bootstrap file.
    #[value(name = "self-signed")]
    SelfSigned,
}

/// The flags of `phoebis init`; a value no flag gives takes its default, or is asked for on a terminal.
#[derive(clap::Args, Debug)]
pub struct InitArgs {
    /// The admin listener's address; 127.0.0.1:8081 when absent.
    #[arg(long, value_name = "ADDR")]
    pub admin_listen: Option<SocketAddr>,
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
    /// The directory of both databases; `data` beside the bootstrap file when absent.
    #[arg(long, value_name = "PATH")]
    pub data_dir: Option<PathBuf>,
    /// Writes the admin secret here, with `RUST_LOG=info`, rather than printing it.
    #[arg(long, value_name = "PATH")]
    pub env_file: Option<PathBuf>,
    /// The storage key file; `storage.key` beside the bootstrap file when absent.
    #[arg(long, value_name = "PATH")]
    pub key_file: Option<PathBuf>,
    /// Asks nothing, even on a terminal; a value a flag did not give takes its default, and a listener off loopback
    /// without a pair gets what `--tls` says, plain HTTP without it.
    #[arg(long)]
    pub non_interactive: bool,
    /// The user that owns the key file and the data directory; needs root.
    #[arg(long, value_name = "USER")]
    pub owner: Option<String>,
    /// The proxy listener's address; 127.0.0.1:8317 when absent.
    #[arg(long, value_name = "ADDR")]
    pub proxy_listen: Option<SocketAddr>,
    /// The proxy listener's certificate chain, PEM, leaf first; `check-config` and the start read it, `init` does not.
    #[arg(long, value_name = "PATH", requires = "proxy_key")]
    pub proxy_cert: Option<PathBuf>,
    /// The proxy listener's private key, PEM.
    #[arg(long, value_name = "PATH", requires = "proxy_cert")]
    pub proxy_key: Option<PathBuf>,
    /// The environment variable that holds the admin secret, written as `admin.secret_env`.
    #[arg(long, value_name = "NAME", default_value = DEFAULT_SECRET_ENV, value_parser = env_name)]
    pub secret_env: String,
    /// What a listener off loopback without a pair gets: `self-signed` generates one, `files` wants the pair flags,
    /// `none` leaves plain HTTP. On a terminal the question is asked when the flag is absent.
    #[arg(long, value_name = "self-signed|files|none")]
    pub tls: Option<TlsMode>,
}

/// One listener as `init` will write it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listener {
    pub listen: SocketAddr,
    pub tls: Option<ListenerTls>,
    /// `tls` names the pair `run` generates under the plan's `tls_dir`.
    pub generated: bool,
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
    /// `tls/` beside the bootstrap file on this host, where a generated pair lands; under `--container` the file
    /// names `CONTAINER_TLS_DIR` instead and the operator mounts this directory there.
    pub tls_dir: PathBuf,
}

/// `--tls self-signed` against an existing bootstrap file: the leaves of the pairs `init` wrote are written again
/// under the CA in `tls_dir`, and nothing else is touched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Renewal {
    pub config_path: PathBuf,
    pub tls_dir: PathBuf,
    /// Each public listener whose `tls` names the pair `init` writes under `tls_dir`, or the container's mount of it
    /// under `CONTAINER_TLS_DIR`, with its address: the leaves to write.
    pub listeners: Vec<(&'static str, SocketAddr)>,
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
    #[error("--tls files needs --{side}-cert and --{side}-key for {side} on {listen}")]
    FilesNeedPair {
        side: &'static str,
        listen: SocketAddr,
    },
    #[error("input ended before {asked} was answered")]
    InputEnded { asked: &'static str },
    #[error("{what}: {error}")]
    Io { what: String, error: std::io::Error },
    #[error(
        "{path} already exists; init never replaces a key, since every stored credential is sealed under it"
    )]
    KeyExists { path: PathBuf },
    #[error("{what}: {error}")]
    Material { what: String, error: MaterialError },
    #[error("--owner names no user: {user}")]
    NoSuchUser { user: String },
    #[error(
        "{path} names no pair under {dir}; init --tls self-signed renews only the pairs it wrote"
    )]
    NothingToRenew { path: PathBuf, dir: PathBuf },
    #[error(
        "the {what} {path} overlaps {tls}, which --tls self-signed writes; name the {what} elsewhere"
    )]
    OverlapsTls {
        what: &'static str,
        path: PathBuf,
        tls: PathBuf,
    },
    #[error("--owner needs root")]
    OwnerNeedsRoot,
    #[error("{path} already exists; init never overwrites a certificate pair")]
    PairExists { path: PathBuf },
    #[error(
        "init --tls self-signed against an existing bootstrap file renews its leaves and takes no other flag"
    )]
    RenewalTakesNoFlag,
    /// A step after the first write failed, and what the run had written is removed again, newest first.
    #[error("{error}; {}", undone(.removed, .kept))]
    Undone {
        error: Box<InitError>,
        removed: Vec<PathBuf>,
        kept: Vec<PathBuf>,
    },
    #[error("the bootstrap file does not load: {error}")]
    Unreadable { error: ConfigError },
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

/// The prompts, over any reader and writer so a test feeds answers: the proxy address, the admin address, and for
/// each that leaves loopback with no pair its TLS answer, unless `--tls` gave it. An empty answer takes the default, a
/// bad one is asked again with the parse error, end of input refuses.
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
    if let Some(mode) = args.tls {
        // The flag answers for an address typed here as for one a flag gave, so the plan is drawn again.
        plan.proxy = listener(
            &PROXY,
            Some(plan.proxy.listen),
            (&args.proxy_cert, &args.proxy_key),
            false,
            Some(mode),
            &plan.tls_dir,
        )?;
        plan.admin = listener(
            &ADMIN,
            Some(plan.admin.listen),
            (&args.admin_cert, &args.admin_key),
            false,
            Some(mode),
            &plan.tls_dir,
        )?;
        return Ok(plan);
    }
    for (side, listener) in [(&PROXY, &mut plan.proxy), (&ADMIN, &mut plan.admin)] {
        if leaves_loopback(listener) && listener.tls.is_none() {
            if let Some((tls, generated)) =
                ask_tls(side, listener.listen, &plan.tls_dir, input, output)?
            {
                listener.tls = Some(tls);
                listener.generated = generated;
            }
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

/// One listener's TLS answer: `None` for plain HTTP, else the pair and whether `run` generates it.
fn ask_tls(
    side: &Side,
    listen: SocketAddr,
    tls_dir: &Path,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> Result<Option<(ListenerTls, bool)>, InitError> {
    loop {
        prompt(
            output,
            &format!(
                "{} on {listen} leaves loopback; tls: self-signed, files, or none for plain HTTP [none]: ",
                side.name
            ),
        )?;
        match answer(input, output, side.asked_choice)?.as_str() {
            "" | "none" => return Ok(None),
            "self-signed" => return Ok(Some((generated_pair(side.name, tls_dir), true))),
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
                return Ok(Some((
                    ListenerTls {
                        cert_file,
                        key_file,
                    },
                    false,
                )));
            }
            _ => say(output, "answer self-signed, files or none")?,
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

/// The flags and the defaults as a plan; a listener off loopback with no pair plans what `--tls` says, plain HTTP
/// without it, until `ask`, on a terminal, offers it a choice.
pub fn plan(config_path: &Path, args: &InitArgs) -> Result<Plan, InitError> {
    let config_path = absolute(config_path)?;
    let dir = parent_of(&config_path).to_path_buf();
    let tls_dir = dir.join("tls");
    let tls_in_file = if args.container {
        PathBuf::from(CONTAINER_TLS_DIR)
    } else {
        tls_dir.clone()
    };
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
            (&args.admin_cert, &args.admin_key),
            args.container,
            args.tls,
            &tls_in_file,
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
            (&args.proxy_cert, &args.proxy_key),
            args.container,
            args.tls,
            &tls_in_file,
        )?,
        secret_env: args.secret_env.clone(),
        tls_dir,
    };
    Ok(plan)
}

/// One listener from its flags: the pair flags' pair; else, off loopback, the pair `run` generates under
/// `tls_in_file` for `self-signed` or the refusal for `files`; else no `tls` line.
fn listener(
    side: &Side,
    listen: Option<SocketAddr>,
    pair: (&Option<PathBuf>, &Option<PathBuf>),
    container: bool,
    mode: Option<TlsMode>,
    tls_in_file: &Path,
) -> Result<Listener, InitError> {
    let default = if container {
        side.container
    } else {
        side.loopback
    };
    let listen = listen.unwrap_or(default);
    let off_loopback = !listen.ip().is_loopback();
    let (tls, generated) = match (pair, mode) {
        ((Some(cert_file), Some(key_file)), _) => (
            Some(ListenerTls {
                cert_file: absolute(cert_file)?,
                key_file: absolute(key_file)?,
            }),
            false,
        ),
        (_, Some(TlsMode::SelfSigned)) if off_loopback => {
            (Some(generated_pair(side.name, tls_in_file)), true)
        }
        (_, Some(TlsMode::Files)) if off_loopback => {
            return Err(InitError::FilesNeedPair {
                side: side.name,
                listen,
            })
        }
        // The container network is plain HTTP by default; the host decides what it publishes.
        _ => (None, false),
    };
    Ok(Listener {
        listen,
        tls,
        generated,
    })
}

/// The names a generated pair is read through under `dir`, `<listener>/cert.pem` and `<listener>/key.pem`.
fn generated_pair(name: &str, dir: &Path) -> ListenerTls {
    let (cert_file, key_file) = selfsigned::leaf_files(dir, name);
    ListenerTls {
        cert_file,
        key_file,
    }
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
    let (key_file, tables) = if plan.container {
        (PathBuf::from(CONTAINER_KEY_FILE), plan.data_dir.clone())
    } else {
        // The service recipe installs the price table beside the bootstrap file, since the loader's relative defaults
        // would resolve against whatever directory starts the process.
        (
            plan.key_file.clone(),
            parent_of(&plan.config_path).to_path_buf(),
        )
    };
    let mut fills = vec![
        listen_fill("admin", &plan.admin),
        value_fill("admin", "secret_env", scalar(&plan.secret_env)),
        listen_fill("proxy", &plan.proxy),
        value_fill(
            "stats",
            "api_prices_path",
            path(&tables.join("api_prices.yaml")),
        ),
        value_fill(
            "stats",
            "ledger_path",
            path(&plan.data_dir.join("ledger.sqlite")),
        ),
        value_fill(
            "stats",
            "subscription_costs_path",
            path(&tables.join("subscription_costs.yaml")),
        ),
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
    if let Some(ListenerTls {
        cert_file,
        key_file,
    }) = &listener.tls
    {
        lines.extend([
            "  tls:".to_string(),
            format!("    cert_file: {}", scalar(&cert_file.to_string_lossy())),
            format!("    key_file: {}", scalar(&key_file.to_string_lossy())),
        ]);
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

/// The refusals that need no write: a file that exists, a registry in the data directory, `--owner` off root or naming
/// no user. They run before `ask`, so a terminal never answers questions for a run that then refuses, and `run` runs
/// them again for whatever landed in between.
pub fn refuse(plan: &Plan) -> Result<(), InitError> {
    refuse_existing(plan)?;
    if let Some(user) = &plan.owner {
        owner_ids(user)?;
    }
    Ok(())
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
    if generates(plan) {
        refuse_tls(plan)?;
    }
    Ok(())
}

/// The refusals of a run that generates certificate material: a storage key or environment file on a path the
/// material takes, a leaf's link that exists, and a CA under `tls_dir` that is there but is not one the run reuses.
/// Each leaf's link and the CA's two files are published under names that must not exist, which covers whatever
/// lands between this check and the write.
fn refuse_tls(plan: &Plan) -> Result<(), InitError> {
    let links: Vec<PathBuf> = generated(plan)
        .map(|(name, _)| plan.tls_dir.join(name))
        .collect();
    let mut taken = vec![plan.tls_dir.join(CA_CERT), plan.tls_dir.join(CA_KEY)];
    taken.extend(links.iter().cloned());
    for (what, path) in [
        ("storage key", Some(&plan.key_file)),
        ("environment file", plan.env_file.as_ref()),
    ] {
        let Some(path) = path else {
            continue;
        };
        // Another file may sit in `tls_dir`, but not on it, on a path the material takes, or under one.
        let overlap = std::iter::once(&plan.tls_dir)
            .chain(&taken)
            .find(|tls| path == *tls || tls.starts_with(path))
            .or_else(|| taken.iter().find(|tls| path.starts_with(tls)));
        if let Some(tls) = overlap {
            return Err(InitError::OverlapsTls {
                what,
                path: path.clone(),
                tls: tls.clone(),
            });
        }
    }
    for link in links {
        if exists(&link)? {
            return Err(InitError::PairExists { path: link });
        }
    }
    selfsigned::read_ca(&plan.tls_dir).map_err(|error| InitError::Material {
        what: format!("reading the CA under {}", plan.tls_dir.display()),
        error,
    })?;
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

    let hex = StorageKey::generate().to_hex();
    let key_file = write_file(&plan.key_file, 0o600, &[hex.as_bytes(), b"\n"], written)?;
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
    if generates(plan) {
        generate(plan, written)?;
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
        hand_over(plan, &key_file, uid, gid, written)?;
    }
    Ok(())
}

fn generates(plan: &Plan) -> bool {
    plan.proxy.generated || plan.admin.generated
}

/// The listeners whose pair `run` generates, by name, in the order the file lists them.
fn generated(plan: &Plan) -> impl Iterator<Item = (&'static str, &Listener)> {
    [(PROXY.name, &plan.proxy), (ADMIN.name, &plan.admin)]
        .into_iter()
        .filter(|(_, listener)| listener.generated)
}

/// The CA under `tls_dir`, reused when a complete one is there so the clients keep their anchor, and a leaf per
/// generated listener. A CA that is there but half, unparseable, mismatched or expired refuses rather than being
/// replaced, since clients may trust it.
fn generate(plan: &Plan, written: &mut Vec<Landed>) -> Result<(), InitError> {
    let material = |error| InitError::Material {
        what: format!(
            "writing the certificate material under {}",
            plan.tls_dir.display()
        ),
        error,
    };
    let existed = exists(&plan.tls_dir)?;
    create_private_dir(&plan.tls_dir)
        .map_err(io_error(format!("creating {}", plan.tls_dir.display())))?;
    if !existed {
        written.push(Landed {
            dir: true,
            path: plan.tls_dir.clone(),
        });
    }
    if plan.container {
        // The container's uid traverses the bind-mounted directory to the certificates; `ca.key` and each leaf key
        // stay 0600 and the caller's, so the wider directory opens nothing secret.
        set_mode(&plan.tls_dir, None, None, 0o755).map_err(io_error(format!(
            "setting the mode of {}",
            plan.tls_dir.display()
        )))?;
    }
    let hostname = selfsigned::hostname();
    let ca = match selfsigned::read_ca(&plan.tls_dir).map_err(material)? {
        Some(ca) => ca,
        None => {
            let mut permitted: Vec<SanName> = generated(plan)
                .flat_map(|(_, listener)| selfsigned::leaf_names(listener.listen))
                .collect();
            permitted.sort();
            permitted.dedup();
            let ca = selfsigned::write_ca(
                &plan.tls_dir,
                &hostname,
                &permitted,
                Validity::days_from_now(CA_LIFETIME_DAYS),
            )
            .map_err(material)?;
            for name in [selfsigned::CA_CERT, selfsigned::CA_KEY] {
                written.push(Landed {
                    dir: false,
                    path: plan.tls_dir.join(name),
                });
            }
            ca
        }
    };
    let leaves: Vec<(&str, Vec<SanName>)> = generated(plan)
        .map(|(name, listener)| (name, selfsigned::leaf_names(listener.listen)))
        .collect();
    let modes = if plan.container {
        LeafModes::Mounted
    } else {
        LeafModes::Private
    };
    let (pairs, stopped) = match selfsigned::write_leaves(
        &plan.tls_dir,
        &ca,
        &hostname,
        &leaves,
        Validity::days_from_now(LEAF_LIFETIME_DAYS),
        modes,
    ) {
        Ok(placed) => (placed, None),
        Err(Unfinished { error, placed }) => (placed, Some(error)),
    };
    for leaf in pairs {
        record_leaf(&leaf, written);
    }
    stopped.map_or(Ok(()), |error| Err(material(error)))
}

/// A live leaf as this run created it: its generation directory, the two files in it, then the link, so `undo`
/// removes the link first and the directory last.
fn record_leaf(leaf: &Placed, written: &mut Vec<Landed>) {
    written.push(Landed {
        dir: true,
        path: leaf.generation.clone(),
    });
    for name in [LEAF_CERT, LEAF_KEY] {
        written.push(Landed {
            dir: false,
            path: leaf.generation.join(name),
        });
    }
    written.push(Landed {
        dir: false,
        path: leaf.link.clone(),
    });
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
) -> Result<File, InitError> {
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
        .map_err(io_error(format!("writing {}", path.display())))?;
    Ok(file)
}

/// The key file, then the directories `owned_dirs` names, go to the owner and its primary group, since the service
/// reads through them. A directory that existed keeps its owner, so a `--data-dir` naming a directory the host already
/// uses is never handed over, and the configuration and environment files stay the caller's.
///
/// The key file goes over through the descriptor `create_new` opened and each directory through `lchown`, so a link
/// swapped in as a path's last component cannot carry the change of owner to another file. A link higher up the path
/// is followed; the documented paths sit under root-owned directories, where nobody else can swap one in.
fn hand_over(
    plan: &Plan,
    key_file: &File,
    uid: u32,
    gid: u32,
    written: &[Landed],
) -> Result<(), InitError> {
    let handed = |path: &Path| io_error(format!("handing {} to the owner", path.display()));
    std::os::unix::fs::fchown(key_file, Some(uid), Some(gid)).map_err(handed(&plan.key_file))?;
    for dir in owned_dirs(plan, written) {
        std::os::unix::fs::lchown(dir, Some(uid), Some(gid)).map_err(handed(dir))?;
    }
    if generates(plan) {
        hand_over_pair(plan, gid)?;
    }
    Ok(())
}

/// A generated pair as the unit wants an operator's: the directory and each leaf's generation directory `0750` and
/// each leaf's key `0640`, all root's with the owner's group, each leaf `0644`. The service reads its leaf and key
/// through the group and never signs, so `ca.pem` and `ca.key` stay root's as written.
fn hand_over_pair(plan: &Plan, gid: u32) -> Result<(), InitError> {
    let set = |path: &Path, group: Option<u32>, mode: u32| {
        set_mode(path, Some(0), group, mode)
            .map_err(io_error(format!("handing {} to the owner", path.display())))
    };
    set(&plan.tls_dir, Some(gid), 0o750)?;
    for (name, _) in generated(plan) {
        // The generation by its own name: `set_mode` refuses a link as the last component.
        let link = plan.tls_dir.join(name);
        let generation = std::fs::read_link(&link)
            .map(|target| plan.tls_dir.join(target))
            .map_err(io_error(format!("reading {}", link.display())))?;
        set(&generation, Some(gid), 0o750)?;
        set(&generation.join(LEAF_CERT), None, 0o644)?;
        set(&generation.join(LEAF_KEY), Some(gid), 0o640)?;
    }
    Ok(())
}

/// Changes `path`'s owner where one is given, then its mode, through a descriptor opened without following a link in
/// the last component, so a link swapped in there cannot carry the change to another file. The parents are followed;
/// the documented paths sit under root-owned directories.
fn set_mode(path: &Path, uid: Option<u32>, gid: Option<u32>, mode: u32) -> std::io::Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    if uid.is_some() || gid.is_some() {
        std::os::unix::fs::fchown(&file, uid, gid)?;
    }
    file.set_permissions(Permissions::from_mode(mode))
}

/// The directories `--owner` receives: the data directory when this run created it, then every other directory this
/// run created on the way to the key file, the data directory or the bootstrap file, which the service reads through
/// them. A directory created only for the environment file stays the caller's: systemd reads that file as root, and
/// owning its directory would let the service replace it.
fn owned_dirs<'a>(plan: &Plan, written: &'a [Landed]) -> Vec<&'a Path> {
    let serves = |dir: &Path| {
        plan.key_file.starts_with(dir)
            || plan.config_path.starts_with(dir)
            || (!plan.container && plan.data_dir.starts_with(dir))
    };
    let (data_dir, others): (Vec<&Path>, Vec<&Path>) = written
        .iter()
        .filter(|landed| landed.dir && serves(&landed.path))
        .map(|landed| landed.path.as_path())
        .partition(|path| *path == plan.data_dir);
    data_dir.into_iter().chain(others).collect()
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
        if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(&landed.path).unwrap_or_default();
            text.push_str(&format!(
                "linked {} to {}\n",
                landed.path.display(),
                target.display()
            ));
            continue;
        }
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
    if generates(plan) {
        text.push_str(&format!(
            "clients must trust {}; copy it to each client and set NODE_EXTRA_CA_CERTS to the copy\n",
            plan.tls_dir.join(CA_CERT).display()
        ));
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
    if plan.container {
        // The key path inside the file is the container's, so a check on the host cannot read it.
        text.push_str(
            "remaining: own storage.key by uid 65532 at mode 0400; copy deploy/compose.yaml with the image tag set",
        );
        if generates(plan) {
            // The leaf keys by name: `ca.key` sits in the mounted directory too, and the container never signs.
            let keys: Vec<String> = generated(plan)
                .map(|(name, _)| format!("tls/{name}/{LEAF_KEY}"))
                .collect();
            text.push_str(&format!(
                "; mount ./tls read-only at /etc/phoebis/tls and own {} by uid 65532 at mode 0400",
                keys.join(" and ")
            ));
        }
        text.push('\n');
        text.push_str("next: docker compose run --rm phoebis check-config\n");
    } else {
        text.push_str(&format!(
            "next: phoebis check-config --config {}\n",
            plan.config_path.display()
        ));
    }
    text
}

/// `Some` when `--tls self-signed` meets an existing bootstrap file, read through `load_unresolved`; `None` when the
/// file does not exist (a first run) or the flag is absent (the refusal of an existing file follows in `refuse`).
/// Refuses any other flag beside `--config` and `--tls`, and a file that names no pair under `tls_dir` or, for the
/// container shape (the storage key at `CONTAINER_KEY_FILE`), under the mount of it at `CONTAINER_TLS_DIR`.
pub fn renewal(config_path: &Path, args: &InitArgs) -> Result<Option<Renewal>, InitError> {
    if args.tls != Some(TlsMode::SelfSigned) {
        return Ok(None);
    }
    let config_path = absolute(config_path)?;
    if !exists(&config_path)? {
        return Ok(None);
    }
    if !tls_alone(args) {
        return Err(InitError::RenewalTakesNoFlag);
    }
    let config = crate::config::load_unresolved(&config_path)
        .map_err(|error| InitError::Unreadable { error })?;
    let tls_dir = parent_of(&config_path).join("tls");
    // Both halves must be the ones `init` writes: a key an operator moved elsewhere would not match a new leaf. A
    // container's file names the mount of `tls/`, whose files the renewal writes on the host; the storage key's
    // secret path marks that file, so a host file naming an operator's pair at the mount's path is not taken for one.
    let container = config.storage.encryption_key_file.as_path() == Path::new(CONTAINER_KEY_FILE);
    let mounted = Path::new(CONTAINER_TLS_DIR);
    let listeners: Vec<(&'static str, SocketAddr)> = [
        (PROXY.name, config.proxy.listen, &config.proxy.tls),
        (ADMIN.name, config.admin.listen, &config.admin.tls),
    ]
    .into_iter()
    .filter(|(name, _, tls)| {
        tls.as_ref().is_some_and(|tls| {
            *tls == generated_pair(name, &tls_dir)
                || (container && *tls == generated_pair(name, mounted))
        })
    })
    .map(|(name, listen, _)| (name, listen))
    .collect();
    if listeners.is_empty() {
        return Err(InitError::NothingToRenew {
            path: config_path,
            dir: tls_dir,
        });
    }
    Ok(Some(Renewal {
        config_path,
        tls_dir,
        listeners,
    }))
}

/// Whether `args` carries nothing beside `--tls`, the one flag a renewal takes. The destructuring names every field,
/// so a flag added later is a compile error here until the renewal decides about it.
fn tls_alone(args: &InitArgs) -> bool {
    let InitArgs {
        admin_listen,
        admin_cert,
        admin_key,
        container,
        data_dir,
        env_file,
        key_file,
        non_interactive,
        owner,
        proxy_listen,
        proxy_cert,
        proxy_key,
        secret_env,
        tls: _,
    } = args;
    admin_listen.is_none()
        && admin_cert.is_none()
        && admin_key.is_none()
        && !container
        && data_dir.is_none()
        && env_file.is_none()
        && key_file.is_none()
        && !non_interactive
        && owner.is_none()
        && proxy_listen.is_none()
        && proxy_cert.is_none()
        && proxy_key.is_none()
        && secret_env == DEFAULT_SECRET_ENV
}

/// Writes the renewal's leaves under the existing CA, each new generation keeping the owner and mode of the one it
/// replaces, and prints one line per leaf and the stop-and-start line on `report`. The run holds the renewal lock under
/// `tls_dir` throughout, so a second renewal refuses rather than writing beside it. Every leaf is staged and checked
/// before any goes live, and a live certificate the CA did not sign is refused, so a refused renewal leaves every
/// pair as it was.
pub fn renew(renewal: &Renewal, report: &mut dyn Write) -> Result<(), InitError> {
    let material = |error| InitError::Material {
        what: format!("renewing the leaves under {}", renewal.tls_dir.display()),
        error,
    };
    let ca = selfsigned::read_ca(&renewal.tls_dir)
        .and_then(|ca| {
            ca.ok_or_else(|| MaterialError::NoCa {
                dir: renewal.tls_dir.clone(),
            })
        })
        .map_err(material)?;
    let _lock = selfsigned::lock_renewal(&renewal.tls_dir).map_err(material)?;
    let leaves: Vec<(&str, Vec<SanName>)> = renewal
        .listeners
        .iter()
        .map(|(name, listen)| (*name, selfsigned::leaf_names(*listen)))
        .collect();
    let (pairs, stopped) = match selfsigned::write_leaves(
        &renewal.tls_dir,
        &ca,
        &selfsigned::hostname(),
        &leaves,
        Validity::days_from_now(LEAF_LIFETIME_DAYS),
        LeafModes::KeepLive,
    ) {
        Ok(placed) => (placed, None),
        Err(Unfinished { error, placed }) => (placed, Some(error)),
    };
    let ca_file = renewal.tls_dir.join(CA_CERT);
    for (cert_file, key_file) in pairs.iter().map(Placed::files) {
        writeln!(
            report,
            "wrote {} and {} under the CA {}",
            cert_file.display(),
            key_file.display(),
            ca_file.display()
        )
        .map_err(io_error("writing the summary".to_string()))?;
    }
    if let Some(error) = stopped {
        return Err(material(error));
    }
    writeln!(
        report,
        "next: stop and start the router, which reads the pair at start"
    )
    .and_then(|()| report.flush())
    .map_err(io_error("writing the summary".to_string()))
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
                "--admin-cert",
                "c.pem",
                "--admin-key",
                "k.pem",
            ]),
            args(&["--tls", "self-signed", "--proxy-listen", "0.0.0.0:8317"]),
            args(&["--container", "--tls", "self-signed"]),
        ];
        for shape in &shapes {
            let plan = plan(&config, shape).unwrap();
            fill(EXAMPLE, &fills(&plan)).unwrap();
        }
        // A drift in the example fails here: the rewriter refuses a key it found no line for, or found twice, rather
        // than render a file without the value.
        let container = fills(&plan(&config, &args(&["--container"])).unwrap());
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
            (
                "self-signed",
                args(&["--tls", "self-signed", "--proxy-listen", "0.0.0.0:8317"]),
            ),
            (
                "container-self-signed",
                args(&["--container", "--tls", "self-signed"]),
            ),
        ];
        for (name, shape) in &shapes {
            let config = dir.path().join(name).join("config.yaml");
            let plan = plan(&config, shape).unwrap();
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
            let (key_file, tables, metrics) = if plan.container {
                (
                    PathBuf::from("/run/secrets/router-storage-key"),
                    PathBuf::from("/var/lib/phoebis"),
                    "0.0.0.0:9464",
                )
            } else {
                (
                    plan.key_file.clone(),
                    dir.path().join(name),
                    "127.0.0.1:9464",
                )
            };
            assert_eq!(loaded.storage.encryption_key_file, key_file, "{name}");
            assert_eq!(
                loaded.stats.api_prices_path,
                Some(tables.join("api_prices.yaml")),
                "{name}"
            );
            assert_eq!(
                loaded.stats.subscription_costs_path,
                Some(tables.join("subscription_costs.yaml")),
                "{name}"
            );
            assert_eq!(
                loaded.stats.metrics_listen,
                metrics.parse::<SocketAddr>().unwrap(),
                "{name}"
            );
        }
        let off = plan(
            &dir.path().join("off-loopback").join("config.yaml"),
            &shapes[1].1,
        )
        .unwrap();
        assert!(off.data_dir.ends_with("odd # dir"), "{off:?}");
        assert!(
            matches!(&off.admin.tls, Some(ListenerTls { cert_file, key_file })
                if cert_file.is_absolute() && key_file.is_absolute()),
            "{off:?}"
        );
        let pair = |dir: &Path, name: &str| {
            Some(ListenerTls {
                cert_file: dir.join(name).join("cert.pem"),
                key_file: dir.join(name).join("key.pem"),
            })
        };
        let generated =
            crate::config::load_unresolved(&dir.path().join("self-signed").join("config.yaml"))
                .unwrap();
        let tls = dir.path().join("self-signed").join("tls");
        assert_eq!(generated.proxy.tls, pair(&tls, "proxy"));
        assert_eq!(generated.admin.tls, None);
        let container = crate::config::load_unresolved(
            &dir.path().join("container-self-signed").join("config.yaml"),
        )
        .unwrap();
        let inside = Path::new("/etc/phoebis/tls");
        assert_eq!(container.proxy.tls, pair(inside, "proxy"));
        assert_eq!(container.admin.tls, pair(inside, "admin"));
    }

    #[test]
    fn an_empty_answer_takes_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let flags = args(&[]);
        let defaults = plan(&dir.path().join("config.yaml"), &flags).unwrap();
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
        let defaults = plan(&dir.path().join("config.yaml"), &flags).unwrap();
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
        let defaults = plan(&dir.path().join("config.yaml"), &flags).unwrap();
        for (input, expected) in [("", "the proxy address"), ("\n", "the admin address")] {
            let error = ask(&flags, &defaults, &mut input.as_bytes(), &mut Vec::new()).unwrap_err();
            assert!(
                matches!(error, InitError::InputEnded { asked } if asked == expected),
                "{input:?}: {error}"
            );
        }
    }

    #[test]
    fn the_prompt_offers_self_signed_files_or_none_and_self_signed_plans_the_pair() {
        let dir = tempfile::tempdir().unwrap();
        let flags = args(&[]);
        let defaults = plan(&dir.path().join("config.yaml"), &flags).unwrap();
        let mut output = Vec::new();
        let asked = ask(
            &flags,
            &defaults,
            &mut "0.0.0.0:8317\n0.0.0.0:8081\nbogus\nself-signed\nfiles\ncert.pem\nkey.pem\n"
                .as_bytes(),
            &mut output,
        )
        .unwrap();
        let tls = dir.path().join("tls");
        assert_eq!(
            asked.proxy.tls,
            Some(ListenerTls {
                cert_file: tls.join("proxy").join("cert.pem"),
                key_file: tls.join("proxy").join("key.pem"),
            })
        );
        assert!(asked.proxy.generated, "{asked:?}");
        let here = std::env::current_dir().unwrap();
        assert_eq!(
            asked.admin.tls,
            Some(ListenerTls {
                cert_file: here.join("cert.pem"),
                key_file: here.join("key.pem"),
            })
        );
        assert!(!asked.admin.generated, "{asked:?}");
        let output = String::from_utf8(output).unwrap();
        assert!(
            output.contains("tls: self-signed, files, or none for plain HTTP [none]: "),
            "{output}"
        );
        assert!(
            output.contains("answer self-signed, files or none"),
            "{output}"
        );
    }

    #[test]
    fn the_tls_flag_answers_the_question_so_ask_does_not_put_it() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.yaml");
        let generated = Some(ListenerTls {
            cert_file: dir.path().join("tls/proxy/cert.pem"),
            key_file: dir.path().join("tls/proxy/key.pem"),
        });
        let flags = args(&["--tls", "self-signed", "--proxy-listen", "0.0.0.0:8317"]);
        let defaults = plan(&config, &flags).unwrap();
        let mut output = Vec::new();
        let asked = ask(&flags, &defaults, &mut "\n".as_bytes(), &mut output).unwrap();
        assert!(asked.proxy.generated, "{asked:?}");
        assert_eq!(asked.proxy.tls, generated);
        assert_eq!(asked.admin.tls, None);
        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains("leaves loopback"), "{output}");

        // An address the terminal answers takes the flag's answer, as a flagged one does.
        let flags = args(&["--tls", "self-signed"]);
        let defaults = plan(&config, &flags).unwrap();
        let mut output = Vec::new();
        let asked = ask(
            &flags,
            &defaults,
            &mut "0.0.0.0:8317\n\n".as_bytes(),
            &mut output,
        )
        .unwrap();
        assert!(asked.proxy.generated, "{asked:?}");
        assert_eq!(asked.proxy.tls, generated);
        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains("leaves loopback"), "{output}");
    }

    #[test]
    fn tls_files_without_a_pair_refuses_naming_the_side_and_the_address() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.yaml");
        let error = plan(
            &config,
            &args(&["--tls", "files", "--proxy-listen", "0.0.0.0:8317"]),
        )
        .unwrap_err();
        assert!(
            matches!(error, InitError::FilesNeedPair { .. }),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            "--tls files needs --proxy-cert and --proxy-key for proxy on 0.0.0.0:8317"
        );
        let planned = plan(
            &config,
            &args(&[
                "--tls",
                "files",
                "--proxy-listen",
                "0.0.0.0:8317",
                "--proxy-cert",
                "c.pem",
                "--proxy-key",
                "k.pem",
            ]),
        )
        .unwrap();
        let here = std::env::current_dir().unwrap();
        assert_eq!(
            planned.proxy,
            Listener {
                listen: "0.0.0.0:8317".parse().unwrap(),
                tls: Some(ListenerTls {
                    cert_file: here.join("c.pem"),
                    key_file: here.join("k.pem"),
                }),
                generated: false,
            }
        );
    }

    #[test]
    fn a_renewal_is_the_flag_against_an_existing_file_and_takes_no_other_flag() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.yaml");
        let renew = args(&["--tls", "self-signed"]);
        let generated = plan(
            &config,
            &args(&["--tls", "self-signed", "--proxy-listen", "0.0.0.0:8317"]),
        )
        .unwrap();
        assert!(generated.proxy.generated, "{generated:?}");
        // Before the file exists the flag asks for a first run.
        assert_eq!(renewal(&config, &renew).unwrap(), None);
        std::fs::write(&config, render(&generated)).unwrap();
        assert_eq!(
            renewal(&config, &renew).unwrap(),
            Some(Renewal {
                config_path: config.clone(),
                tls_dir: dir.path().join("tls"),
                listeners: vec![("proxy", "0.0.0.0:8317".parse().unwrap())],
            })
        );
        let flagged = renewal(
            &config,
            &args(&["--tls", "self-signed", "--proxy-listen", "0.0.0.0:1"]),
        );
        assert!(
            matches!(flagged, Err(InitError::RenewalTakesNoFlag)),
            "{flagged:?}"
        );
        // Without the flag an existing file is `refuse`'s to refuse.
        assert_eq!(renewal(&config, &args(&[])).unwrap(), None);

        let plain_dir = dir.path().join("plain");
        let plain = plain_dir.join("config.yaml");
        std::fs::create_dir(&plain_dir).unwrap();
        std::fs::write(&plain, render(&plan(&plain, &args(&[])).unwrap())).unwrap();
        let error = renewal(&plain, &renew).unwrap_err();
        assert!(
            matches!(&error, InitError::NothingToRenew { path, dir }
                if *path == plain && *dir == plain_dir.join("tls")),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            format!(
                "{} names no pair under {}; init --tls self-signed renews only the pairs it wrote",
                plain.display(),
                plain_dir.join("tls").display()
            )
        );
    }

    #[test]
    fn a_container_file_renews_its_pairs_under_tls_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.yaml");
        let container = plan(&config, &args(&["--container", "--tls", "self-signed"])).unwrap();
        std::fs::write(&config, render(&container)).unwrap();
        // The file names the container's mount at /etc/phoebis/tls; the pairs behind it sit in tls/ beside the file.
        assert_eq!(
            renewal(&config, &args(&["--tls", "self-signed"])).unwrap(),
            Some(Renewal {
                config_path: config.clone(),
                tls_dir: dir.path().join("tls"),
                listeners: vec![
                    ("proxy", "0.0.0.0:8317".parse().unwrap()),
                    ("admin", "0.0.0.0:8081".parse().unwrap()),
                ],
            })
        );

        // A host file naming an operator's pair at the mount's path is no container's file, so it names no pair.
        let host_dir = dir.path().join("host");
        let host = host_dir.join("config.yaml");
        std::fs::create_dir(&host_dir).unwrap();
        let flags = args(&[
            "--proxy-listen",
            "0.0.0.0:8317",
            "--proxy-cert",
            "/etc/phoebis/tls/proxy/cert.pem",
            "--proxy-key",
            "/etc/phoebis/tls/proxy/key.pem",
        ]);
        std::fs::write(&host, render(&plan(&host, &flags).unwrap())).unwrap();
        let error = renewal(&host, &args(&["--tls", "self-signed"])).unwrap_err();
        assert!(
            matches!(&error, InitError::NothingToRenew { path, dir }
                if *path == host && *dir == host_dir.join("tls")),
            "{error:?}"
        );
    }

    #[test]
    fn the_owner_receives_the_directories_created_for_the_key_the_data_and_the_file_and_no_other() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let flags = args(&[
            "--key-file",
            root.join("keys/storage.key").to_str().unwrap(),
            "--data-dir",
            root.join("have/data").to_str().unwrap(),
            "--env-file",
            root.join("env/only/phoebis.env").to_str().unwrap(),
        ]);
        let plan = plan(&root.join("conf/config.yaml"), &flags).unwrap();
        let landed = |path: &str, dir: bool| Landed {
            dir,
            path: root.join(path),
        };
        // As `write` records them: the parents of the key file, the environment file, the data directory and the
        // bootstrap file from the root down, then the files and the data directory itself.
        let written = [
            landed("keys", true),
            landed("env", true),
            landed("env/only", true),
            landed("conf", true),
            landed("keys/storage.key", false),
            landed("env/only/phoebis.env", false),
            landed("have/data", true),
            landed("conf/config.yaml", false),
        ];
        // `have` existed, so it is not on the list; `env` and `env/only` lead to the environment file alone, which
        // systemd reads as root, and owning them would let the service replace that file.
        assert_eq!(
            owned_dirs(&plan, &written),
            [root.join("have/data"), root.join("keys"), root.join("conf")]
        );
    }

    #[test]
    fn a_listener_off_loopback_without_a_pair_plans_plain_http_off_a_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let flags = args(&["--admin-listen", "0.0.0.0:8081"]);
        let plan = plan(&dir.path().join("config.yaml"), &flags).unwrap();
        assert_eq!(
            plan.admin,
            Listener {
                listen: "0.0.0.0:8081".parse().unwrap(),
                tls: None,
                generated: false,
            }
        );
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }
}
