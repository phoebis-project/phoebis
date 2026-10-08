//! The bootstrap file: the settings the process needs before it can serve anything, and the rules that
//! refuse a configuration nobody should be able to start.
//!
//! The admin secret is named here, never written here: the file holds the name of the environment variable
//! that `load` resolves, so a configuration file can be read by anyone who may read the deployment.

use std::fmt;
use std::net::SocketAddr;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use phoebis_keepalive::{KeepaliveLimits, LimitsError};
use phoebis_store::Secret;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub admin: AdminConfig,
    /// Process-wide keepalive limits, read once at start; whether probes run, and their tuning, are per-pool
    /// registry settings.
    #[serde(default)]
    pub keepalive: KeepaliveLimits,
    pub proxy: ProxyConfig,
    #[serde(default)]
    pub routing: RoutingConfig,
    #[serde(default)]
    pub sessions: SessionsConfig,
    #[serde(default)]
    pub stats: StatsConfig,
    pub storage: StorageConfig,
    #[serde(default)]
    pub upstream: UpstreamConfig,
    #[serde(default)]
    pub usage: UsageConfig,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminConfig {
    pub listen: SocketAddr,
    #[serde(default = "unresolved", skip)]
    pub secret: Secret,
    pub secret_env: String,
    #[serde(default, deserialize_with = "present_tls")]
    pub tls: Option<ListenerTls>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyConfig {
    /// The bytes of request bodies the process holds at once, across every connection, 512 MiB by default.
    #[serde(default = "default_body_budget_bytes")]
    pub body_budget_bytes: u64,
    /// How long a request body may take to arrive after authentication, under the stream idle bound.
    #[serde(default = "default_body_read_seconds")]
    pub body_read_seconds: u64,
    /// How many request bodies one key may have arriving at once.
    #[serde(default = "default_key_body_reads")]
    pub key_body_reads: u32,
    pub listen: SocketAddr,
    /// The per-body bound, the Messages API's own request limit by default; a larger value is refused.
    #[serde(default = "default_max_request_bytes")]
    pub max_request_bytes: usize,
    /// Whether a restored billing placeholder is signed with this build's formula; off until a server
    /// rejection of the placeholder proves it necessary.
    #[serde(default)]
    pub sign_cch_placeholder: bool,
    #[serde(default, deserialize_with = "present_tls")]
    pub tls: Option<ListenerTls>,
    /// How the edited body leaves toward the upstream: `auto` mirrors the client, `on` always gzip, `off` never.
    #[serde(default = "default_upstream_gzip")]
    pub upstream_gzip: String,
}

/// One public listener's `tls` key: the operator's certificate pair, and the listener terminates TLS. Absent, the
/// listener serves plain HTTP on the address named, loopback or not; the file records that trust decision by naming
/// no pair, and no address requires one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListenerTls {
    /// PEM, the leaf first with any intermediates after it.
    pub cert_file: PathBuf,
    /// PEM, the matching private key.
    pub key_file: PathBuf,
}

/// The line for a string or a null in the key's place, a stray key, a missing key or a repeated one; serde puts the
/// key's path in front of it. A hand-written visitor rather than a derive, so a word earns this line and not serde's
/// "invalid type" one, which names no key.
const TLS_SHAPES: &str = "must be a mapping with cert_file and key_file";

impl<'de> Deserialize<'de> for ListenerTls {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Shapes;

        impl<'de> serde::de::Visitor<'de> for Shapes {
            type Value = ListenerTls;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a mapping with cert_file and key_file")
            }

            fn visit_str<E: serde::de::Error>(self, _word: &str) -> Result<ListenerTls, E> {
                Err(E::custom(TLS_SHAPES))
            }

            fn visit_unit<E: serde::de::Error>(self) -> Result<ListenerTls, E> {
                Err(E::custom(TLS_SHAPES))
            }

            fn visit_none<E: serde::de::Error>(self) -> Result<ListenerTls, E> {
                Err(E::custom(TLS_SHAPES))
            }

            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<ListenerTls, M::Error> {
                let (mut cert_file, mut key_file) = (None, None);
                while let Some(key) = map.next_key::<String>()? {
                    let slot = match key.as_str() {
                        "cert_file" => &mut cert_file,
                        "key_file" => &mut key_file,
                        _ => return Err(serde::de::Error::custom(TLS_SHAPES)),
                    };
                    if slot.replace(map.next_value::<PathBuf>()?).is_some() {
                        return Err(serde::de::Error::custom(TLS_SHAPES));
                    }
                }
                match (cert_file, key_file) {
                    (Some(cert_file), Some(key_file)) => Ok(ListenerTls {
                        cert_file,
                        key_file,
                    }),
                    _ => Err(serde::de::Error::custom(TLS_SHAPES)),
                }
            }
        }

        deserializer.deserialize_any(Shapes)
    }
}

/// A present `tls` key must hold the pair. Serde's own `Option` reads a YAML null, the key written with nothing
/// beneath it, as an absent key, and the listener would then serve plain HTTP on an address whose operator meant to
/// name a pair; only a missing key means plain HTTP.
fn present_tls<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<ListenerTls>, D::Error> {
    ListenerTls::deserialize(deserializer).map(Some)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    /// A file holding exactly 32 raw bytes, or 64 hex characters with at most one trailing newline, outside the
    /// database it unseals.
    pub encryption_key_file: PathBuf,
    pub path: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageConfig {
    /// How long a cached usage snapshot answers a manual refresh before the endpoint is read again.
    #[serde(default = "default_usage_cache_ttl")]
    pub cache_ttl_seconds: u64,
    /// How many accounts' usage and profile fetches run at once.
    #[serde(default = "default_refresh_concurrency")]
    pub refresh_concurrency: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingConfig {
    /// How often every enabled upstream's model listing is read again, 60 to 604800.
    #[serde(default = "default_catalog_refresh")]
    pub catalog_refresh_seconds: u64,
    #[serde(default = "default_cooldown_fallback")]
    pub cooldown_fallback_seconds: u64,
    /// How long an account is skipped for a model it answered 404 for.
    #[serde(default = "default_model_rejection")]
    pub model_rejection_seconds: u64,
    /// How long the proxy listener waits at startup for the model listings and the usage snapshots, 1 to 60.
    #[serde(default = "default_startup_fetch")]
    pub startup_fetch_seconds: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionsConfig {
    #[serde(default = "default_session_expiry")]
    pub expire_after_seconds: u64,
    /// Entries the in-process session memory holds at most; past it the least recently seen entry goes to
    /// disk and out of memory.
    #[serde(default = "default_memory_cap")]
    pub memory_cap: usize,
    /// How long an entry may go unseen before the in-process memory drops it; the on-disk copy outlives it.
    #[serde(default = "default_memory_idle")]
    pub memory_idle_seconds: u64,
    /// How long the on-disk copy of a session's memory is kept after its last request.
    #[serde(default = "default_memory_retention")]
    pub memory_retention_days: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatsConfig {
    /// A price table that overrides the shipped one; a missing file is not an error. Default `./api_prices.yaml`.
    #[serde(default = "default_api_prices_path")]
    pub api_prices_path: Option<PathBuf>,
    #[serde(default = "default_body_capture_max")]
    pub body_capture_max_hours: u32,
    #[serde(default = "default_ledger_path")]
    pub ledger_path: PathBuf,
    #[serde(default = "default_metrics_listen")]
    pub metrics_listen: SocketAddr,
    /// Days the raw request, attempt, and event rows, and the quota and extra-usage samples, are kept; 0 keeps
    /// them forever.
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
    /// Days the hourly rollups are kept; 0 keeps them forever, and otherwise at least `retention_days`.
    #[serde(default = "default_rollup_retention_days")]
    pub rollup_retention_days: u32,
    /// A subscription cost model that overrides the shipped one; a missing file is not an error. Default
    /// `./subscription_costs.yaml`.
    #[serde(default = "default_subscription_costs_path")]
    pub subscription_costs_path: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    /// An extra trust anchor for the inference and control connectors, for a lab upstream that is not
    /// signed by a public authority.
    #[serde(default)]
    pub extra_ca_pem: Option<PathBuf>,
    /// Sends every host to this address instead of asking the resolver, which is how a test or a lab
    /// deployment points the router at a local upstream.
    #[serde(default)]
    pub resolve_override: Option<SocketAddr>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("proxy.body_budget_bytes must be at most 1099511627776, not {value}")]
    BodyBudgetTooLarge { value: u64 },
    #[error(
        "proxy.body_budget_bytes must be at least proxy.max_request_bytes ({minimum}), not {value}"
    )]
    BodyBudgetTooSmall { minimum: usize, value: u64 },
    #[error("stats.body_capture_max_hours must be between 1 and 168, not {value}")]
    BodyCaptureMaxOutOfRange { value: u32 },
    #[error("proxy.body_read_seconds must be between 1 and {max}, not {value}")]
    BodyReadOutOfRange { max: u64, value: u64 },
    #[error("routing.catalog_refresh_seconds must be between 60 and 604800, not {value}")]
    CatalogRefreshOutOfRange { value: u64 },
    #[error("routing.cooldown_fallback_seconds must be between 1 and 3600, not {value}")]
    CooldownFallbackOutOfRange { value: u64 },
    #[error("{0}")]
    Keepalive(#[from] LimitsError),
    #[error("proxy.key_body_reads must be between 1 and 4096, not {value}")]
    KeyBodyReadsOutOfRange { value: u32 },
    #[error("stats.ledger_path {path} must name a database file inside a directory")]
    LedgerPath { path: PathBuf },
    #[error("proxy.max_request_bytes must be between 1024 and 33554432, not {value}")]
    MaxRequestBytesOutOfRange { value: usize },
    #[error("sessions.memory_cap must be between 1 and 1048576, not {value}")]
    MemoryCapOutOfRange { value: usize },
    #[error("sessions.memory_idle_seconds must be between 60 and 31536000, not {value}")]
    MemoryIdleOutOfRange { value: u64 },
    #[error("sessions.memory_retention_days must be between 1 and 3650, not {value}")]
    MemoryRetentionOutOfRange { value: u32 },
    #[error("environment variable {name} is unset or empty")]
    MissingSecret { name: String },
    #[error("routing.model_rejection_seconds must be between 1 and 604800, not {value}")]
    ModelRejectionOutOfRange { value: u64 },
    #[error("environment variable {name} still holds the example placeholder")]
    PlaceholderSecret { name: String },
    #[error("cannot read {path}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("usage.refresh_concurrency must be between 1 and 16, not {value}")]
    RefreshConcurrencyOutOfRange { value: usize },
    #[error("stats.rollup_retention_days must be 0 or at least stats.retention_days ({retention_days}), not {value}")]
    RollupRetentionTooShort { retention_days: u32, value: u32 },
    #[error("environment variable {name} holds a secret outside the allowed shape (printable ASCII, no surrounding spaces)")]
    SecretShape { name: String },
    #[error("environment variable {name} holds a secret shorter than 16 characters")]
    SecretTooShort { name: String },
    #[error("sessions.expire_after_seconds must be at least 60")]
    SessionExpiryTooShort,
    #[error("stats.ledger_path and storage.path must name different files, not {path}")]
    SharedDatabasePath { path: PathBuf },
    #[error("routing.startup_fetch_seconds must be between 1 and 60, not {value}")]
    StartupFetchOutOfRange { value: u64 },
    #[error("storage.path {path} must name a database file inside a directory")]
    StoragePath { path: PathBuf },
    #[error("the configuration is not valid YAML")]
    Syntax(#[from] serde_norway::Error),
    #[error("proxy.upstream_gzip must be auto, on or off, not {value}")]
    UpstreamGzipInvalid { value: String },
    #[error("usage.cache_ttl_seconds must be between 1 and 3600, not {value}")]
    UsageCacheTtlOutOfRange { value: u64 },
}

/// Reads, parses, and validates the bootstrap file, resolving the admin secret from the environment.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let mut config = load_unresolved(path)?;
    resolve_secrets(&mut config)?;
    Ok(config)
}

/// Reads, parses and validates the bootstrap file and leaves the admin secret unresolved, for `check-config`,
/// which must run without the environment the service gets from its unit.
pub fn load_unresolved(path: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let config: Config = serde_norway::from_str(&text)?;
    check_shape(&config)?;
    Ok(config)
}

impl fmt::Debug for AdminConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdminConfig")
            .field("listen", &self.listen)
            .field("secret", &"***")
            .field("secret_env", &self.secret_env)
            .field("tls", &self.tls)
            .finish()
    }
}

impl Default for RoutingConfig {
    fn default() -> Self {
        RoutingConfig {
            catalog_refresh_seconds: default_catalog_refresh(),
            cooldown_fallback_seconds: default_cooldown_fallback(),
            model_rejection_seconds: default_model_rejection(),
            startup_fetch_seconds: default_startup_fetch(),
        }
    }
}

impl Default for SessionsConfig {
    fn default() -> Self {
        SessionsConfig {
            expire_after_seconds: default_session_expiry(),
            memory_cap: default_memory_cap(),
            memory_idle_seconds: default_memory_idle(),
            memory_retention_days: default_memory_retention(),
        }
    }
}

impl Default for StatsConfig {
    fn default() -> Self {
        StatsConfig {
            api_prices_path: default_api_prices_path(),
            body_capture_max_hours: default_body_capture_max(),
            ledger_path: default_ledger_path(),
            metrics_listen: default_metrics_listen(),
            retention_days: default_retention_days(),
            rollup_retention_days: default_rollup_retention_days(),
            subscription_costs_path: default_subscription_costs_path(),
        }
    }
}

impl Default for UsageConfig {
    fn default() -> UsageConfig {
        UsageConfig {
            cache_ttl_seconds: default_usage_cache_ttl(),
            refresh_concurrency: default_refresh_concurrency(),
        }
    }
}

fn check_shape(config: &Config) -> Result<(), ConfigError> {
    if !(1..=3600).contains(&config.usage.cache_ttl_seconds) {
        return Err(ConfigError::UsageCacheTtlOutOfRange {
            value: config.usage.cache_ttl_seconds,
        });
    }
    if !(1..=16).contains(&config.usage.refresh_concurrency) {
        return Err(ConfigError::RefreshConcurrencyOutOfRange {
            value: config.usage.refresh_concurrency,
        });
    }
    // The store creates the database under exactly this name, in the directory that holds it, so a path
    // that names one of the two and not the other is not a path it can honor.
    if config.storage.path.parent().is_none() || config.storage.path.file_name().is_none() {
        return Err(ConfigError::StoragePath {
            path: config.storage.path.clone(),
        });
    }
    let proxy = &config.proxy;
    // The upstream refuses a larger request itself, so a higher bound would only buffer what it turns away.
    if !(1_024..=33_554_432).contains(&proxy.max_request_bytes) {
        return Err(ConfigError::MaxRequestBytesOutOfRange {
            value: proxy.max_request_bytes,
        });
    }
    // A budget under one body at the per-body bound refuses a body the bound itself admits.
    if proxy.body_budget_bytes < proxy.max_request_bytes as u64 {
        return Err(ConfigError::BodyBudgetTooSmall {
            minimum: proxy.max_request_bytes,
            value: proxy.body_budget_bytes,
        });
    }
    if proxy.body_budget_bytes > 1 << 40 {
        return Err(ConfigError::BodyBudgetTooLarge {
            value: proxy.body_budget_bytes,
        });
    }
    // Under the stream idle bound: a body that takes as long as the client's own ten minutes leaves no time to
    // answer it.
    let body_read_max = crate::app::IDLE_TIMEOUT.as_secs() - 1;
    if !(1..=body_read_max).contains(&proxy.body_read_seconds) {
        return Err(ConfigError::BodyReadOutOfRange {
            max: body_read_max,
            value: proxy.body_read_seconds,
        });
    }
    if !(1..=4_096).contains(&proxy.key_body_reads) {
        return Err(ConfigError::KeyBodyReadsOutOfRange {
            value: proxy.key_body_reads,
        });
    }
    if !matches!(proxy.upstream_gzip.as_str(), "auto" | "on" | "off") {
        return Err(ConfigError::UpstreamGzipInvalid {
            value: proxy.upstream_gzip.clone(),
        });
    }
    if !(1..=604_800).contains(&config.routing.model_rejection_seconds) {
        return Err(ConfigError::ModelRejectionOutOfRange {
            value: config.routing.model_rejection_seconds,
        });
    }
    if !(1..=3600).contains(&config.routing.cooldown_fallback_seconds) {
        return Err(ConfigError::CooldownFallbackOutOfRange {
            value: config.routing.cooldown_fallback_seconds,
        });
    }
    config.keepalive.validate(proxy.max_request_bytes)?;
    if !(1..=60).contains(&config.routing.startup_fetch_seconds) {
        return Err(ConfigError::StartupFetchOutOfRange {
            value: config.routing.startup_fetch_seconds,
        });
    }
    if !(60..=604_800).contains(&config.routing.catalog_refresh_seconds) {
        return Err(ConfigError::CatalogRefreshOutOfRange {
            value: config.routing.catalog_refresh_seconds,
        });
    }
    if config.sessions.expire_after_seconds < 60 {
        return Err(ConfigError::SessionExpiryTooShort);
    }
    // A million entries is a few hundred megabytes at the sizes `identity` and `facts` bound a record to, and
    // the most a host should be asked to hold for this; a zero cap would drop every entry on its next write.
    if !(1..=1_048_576).contains(&config.sessions.memory_cap) {
        return Err(ConfigError::MemoryCapOutOfRange {
            value: config.sessions.memory_cap,
        });
    }
    // Bounded above as well as below: the relay holds both as milliseconds in an i64, and a value near the
    // type's own ceiling wraps that arithmetic negative, after which every entry reads as idle at once.
    if !(60..=31_536_000).contains(&config.sessions.memory_idle_seconds) {
        return Err(ConfigError::MemoryIdleOutOfRange {
            value: config.sessions.memory_idle_seconds,
        });
    }
    if !(1..=3_650).contains(&config.sessions.memory_retention_days) {
        return Err(ConfigError::MemoryRetentionOutOfRange {
            value: config.sessions.memory_retention_days,
        });
    }
    if !(1..=168).contains(&config.stats.body_capture_max_hours) {
        return Err(ConfigError::BodyCaptureMaxOutOfRange {
            value: config.stats.body_capture_max_hours,
        });
    }
    // Zero is "forever" on both keys; otherwise a rollup must outlive the rows it was folded from.
    let (raw, rollup) = (
        config.stats.retention_days,
        config.stats.rollup_retention_days,
    );
    if rollup != 0 && raw != 0 && rollup < raw {
        return Err(ConfigError::RollupRetentionTooShort {
            retention_days: raw,
            value: rollup,
        });
    }
    if config.stats.ledger_path.parent().is_none() || config.stats.ledger_path.file_name().is_none()
    {
        return Err(ConfigError::LedgerPath {
            path: config.stats.ledger_path.clone(),
        });
    }
    // Two schemas in one file open quietly and then lose everything: `LedgerDb::open` creates its tables
    // inside the registry file, reads the store's `meta.schema_version`, skips every ledger migration, and
    // leaves `requests` without the columns the writer inserts.
    if same_file(&config.stats.ledger_path, &config.storage.path) {
        return Err(ConfigError::SharedDatabasePath {
            path: config.stats.ledger_path.clone(),
        });
    }
    Ok(())
}

/// One path folded to the name the filesystem would open it under, without asking the filesystem anything: a
/// `.` component drops out, a `..` resolves against the component before it, and a relative path is taken
/// against the working directory, which is what the process itself will do with it.
fn normalized(path: &Path) -> PathBuf {
    let absolute = match path.is_absolute() {
        true => path.to_path_buf(),
        false => std::env::current_dir().unwrap_or_default().join(path),
    };
    let mut folded = PathBuf::new();
    for part in absolute.components() {
        match part {
            Component::CurDir => {}
            // A `..` at the root stays at the root, as it does on the filesystem.
            Component::ParentDir => {
                folded.pop();
            }
            other => folded.push(other),
        }
    }
    folded
}

/// Whether two configured paths name one file. The fold above catches the spellings; the device and inode
/// catch a symlink or a hard link between two of them, and are only consulted for files that already exist,
/// so validating a configuration never creates the database it is about to refuse.
fn same_file(left: &Path, right: &Path) -> bool {
    if normalized(left) == normalized(right) {
        return true;
    }
    match (std::fs::metadata(left), std::fs::metadata(right)) {
        (Ok(left), Ok(right)) => left.dev() == right.dev() && left.ino() == right.ino(),
        _ => false,
    }
}

/// The step `load` runs after `load_unresolved`, exposed so `check-config` can warn instead of refuse.
pub fn resolve_secrets(config: &mut Config) -> Result<(), ConfigError> {
    config.admin.secret = from_env(&config.admin.secret_env)?;
    Ok(())
}

/// Whether `name` is a variable `resolve_secrets` would refuse as unset, so `check-config` warns about exactly the
/// variables the start would find missing.
pub fn secret_is_unset(name: &str) -> bool {
    matches!(from_env(name), Err(ConfigError::MissingSecret { .. }))
}

/// The example file's own placeholder (`deploy/phoebis.env.example`), so a config copied without editing it
/// refuses to start rather than run with a secret every reader of the repository already knows.
const PLACEHOLDER_ADMIN_SECRET: &str = "replace-with-a-long-random-string";

pub(crate) fn from_env(name: &str) -> Result<Secret, ConfigError> {
    let value = match std::env::var_os(name) {
        // The value is never part of the error: a variable set to something unusable still holds a secret. A byte
        // that is not UTF-8 reads as U+FFFD, so the shape check below refuses it rather than calling it unset.
        Some(value) if !value.is_empty() => value.to_string_lossy().into_owned(),
        _ => {
            return Err(ConfigError::MissingSecret {
                name: name.to_string(),
            })
        }
    };
    if value.chars().count() < 16 {
        return Err(ConfigError::SecretTooShort {
            name: name.to_string(),
        });
    }
    if value == PLACEHOLDER_ADMIN_SECRET {
        return Err(ConfigError::PlaceholderSecret {
            name: name.to_string(),
        });
    }
    // The bearer path reads its header as ASCII text and trims it, so a secret of another shape would open the
    // console and never a bearer call.
    if value.chars().any(|c| !(' '..='~').contains(&c))
        || value.starts_with(' ')
        || value.ends_with(' ')
    {
        return Err(ConfigError::SecretShape {
            name: name.to_string(),
        });
    }
    Ok(Secret::new(&value))
}

fn default_api_prices_path() -> Option<PathBuf> {
    Some("./api_prices.yaml".into())
}

/// Room for 512 one-MiB bodies at once, or sixteen at the per-body bound: a household whose keys carry agent tools
/// that fan out.
fn default_body_budget_bytes() -> u64 {
    512 << 20
}

fn default_body_capture_max() -> u32 {
    72
}

fn default_body_read_seconds() -> u64 {
    60
}

fn default_catalog_refresh() -> u64 {
    21_600
}

fn default_cooldown_fallback() -> u64 {
    60
}

fn default_key_body_reads() -> u32 {
    128
}

fn default_ledger_path() -> PathBuf {
    "./data/ledger.sqlite".into()
}

/// The Messages API's own request limit, so the router never refuses a body the upstream would accept.
fn default_max_request_bytes() -> usize {
    32 << 20
}

fn default_metrics_listen() -> SocketAddr {
    ([127, 0, 0, 1], 9464).into()
}

fn default_model_rejection() -> u64 {
    3_600
}

fn default_refresh_concurrency() -> usize {
    2
}

fn default_retention_days() -> u32 {
    90
}

fn default_rollup_retention_days() -> u32 {
    400
}

fn default_session_expiry() -> u64 {
    86_400
}

fn default_startup_fetch() -> u64 {
    30
}

fn default_subscription_costs_path() -> Option<PathBuf> {
    Some("./subscription_costs.yaml".into())
}

fn default_upstream_gzip() -> String {
    "auto".into()
}

fn default_usage_cache_ttl() -> u64 {
    60
}

fn default_memory_cap() -> usize {
    65_536
}

fn default_memory_idle() -> u64 {
    86_400
}

fn default_memory_retention() -> u32 {
    30
}

/// Stands in for a secret until `load` resolves it. `load` is the only way to obtain a `Config`, and it
/// replaces every one of these before it returns.
fn unresolved() -> Secret {
    Secret::new("")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        let key = dir.join("storage.key");
        std::fs::write(
            &key,
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        let yaml = format!("admin:\n  listen: 127.0.0.1:0\n  secret_env: TEST_CFG_ADMIN\nproxy:\n  listen: 127.0.0.1:0\nstorage:\n  encryption_key_file: {}\n  path: {}\n{body}", key.display(), dir.join("router.sqlite").display());
        let path = dir.join(format!("{name}.yaml"));
        std::fs::write(&path, yaml).unwrap();
        path
    }

    /// `file`'s own template fixes the `proxy` block at `listen` alone; this one carries the caller's further keys
    /// under it.
    fn file_with_proxy(dir: &std::path::Path, name: &str, keys: &str) -> std::path::PathBuf {
        let key = dir.join("storage.key");
        std::fs::write(
            &key,
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        let yaml = format!(
            "admin:\n  listen: 127.0.0.1:0\n  secret_env: TEST_CFG_ADMIN\n\
             proxy:\n  listen: 127.0.0.1:0\n{keys}storage:\n  encryption_key_file: {}\n  path: {}\n",
            key.display(),
            dir.join("router.sqlite").display()
        );
        let path = dir.join(format!("{name}.yaml"));
        std::fs::write(&path, yaml).unwrap();
        path
    }

    #[test]
    fn the_proxy_keys_default_and_refuse_outside_their_bounds() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("TEST_CFG_ADMIN", "admin-secret-001");
        let proxy = load(&file(dir.path(), "defaults", "")).unwrap().proxy;
        assert_eq!(
            (
                proxy.body_budget_bytes,
                proxy.body_read_seconds,
                proxy.key_body_reads,
                proxy.max_request_bytes,
                proxy.sign_cch_placeholder,
                proxy.upstream_gzip.as_str()
            ),
            (536_870_912, 60, 128, 33_554_432, false, "auto")
        );
        for (name, keys, expected) in [
            (
                "max-request-low",
                "  max_request_bytes: 1023\n",
                "proxy.max_request_bytes must be between 1024 and 33554432, not 1023",
            ),
            (
                "max-request-high",
                "  max_request_bytes: 33554433\n",
                "proxy.max_request_bytes must be between 1024 and 33554432, not 33554433",
            ),
            (
                "budget-small",
                "  body_budget_bytes: 1024\n",
                "proxy.body_budget_bytes must be at least proxy.max_request_bytes (33554432), not 1024",
            ),
            (
                "budget-large",
                "  body_budget_bytes: 1099511627777\n",
                "proxy.body_budget_bytes must be at most 1099511627776, not 1099511627777",
            ),
            (
                "read-zero",
                "  body_read_seconds: 0\n",
                "proxy.body_read_seconds must be between 1 and 599, not 0",
            ),
            (
                "read-long",
                "  body_read_seconds: 600\n",
                "proxy.body_read_seconds must be between 1 and 599, not 600",
            ),
            (
                "reads-zero",
                "  key_body_reads: 0\n",
                "proxy.key_body_reads must be between 1 and 4096, not 0",
            ),
            (
                "reads-high",
                "  key_body_reads: 4097\n",
                "proxy.key_body_reads must be between 1 and 4096, not 4097",
            ),
            (
                "gzip",
                "  upstream_gzip: gzip\n",
                "proxy.upstream_gzip must be auto, on or off, not gzip",
            ),
        ] {
            let error = load(&file_with_proxy(dir.path(), name, keys)).unwrap_err();
            assert_eq!(error.to_string(), expected, "{name}");
        }
        let edges = load(&file_with_proxy(
            dir.path(),
            "edges",
            "  body_budget_bytes: 1099511627776\n  body_read_seconds: 599\n  key_body_reads: 4096\n  \
             max_request_bytes: 1024\n  sign_cch_placeholder: true\n  upstream_gzip: off\n",
        ))
        .unwrap()
        .proxy;
        assert_eq!(
            (
                edges.body_budget_bytes,
                edges.body_read_seconds,
                edges.key_body_reads,
                edges.max_request_bytes,
                edges.sign_cch_placeholder,
                edges.upstream_gzip.as_str()
            ),
            (1 << 40, 599, 4_096, 1_024, true, "off")
        );
        let low = load(&file_with_proxy(
            dir.path(),
            "low",
            "  body_budget_bytes: 1024\n  body_read_seconds: 1\n  key_body_reads: 1\n  \
             max_request_bytes: 1024\n  upstream_gzip: on\n",
        ))
        .unwrap()
        .proxy;
        assert_eq!(
            (low.body_budget_bytes, low.upstream_gzip.as_str()),
            (1_024, "on"),
            "a budget of one body at the per-body bound is enough"
        );
    }

    #[test]
    fn the_tls_key_is_the_pair_and_every_other_shape_is_refused_naming_the_two_keys() {
        let dir = tempfile::tempdir().unwrap();
        let pair = load_unresolved(&file_with_proxy(
            dir.path(),
            "pair",
            "  tls:\n    cert_file: /a\n    key_file: /b\n",
        ))
        .unwrap()
        .proxy;
        assert_eq!(
            pair.tls,
            Some(ListenerTls {
                cert_file: "/a".into(),
                key_file: "/b".into(),
            })
        );
        let refused = |name: &str, keys: &str| {
            let error = load_unresolved(&file_with_proxy(dir.path(), name, keys)).unwrap_err();
            let chain = chain(&error);
            assert!(
                matches!(error, ConfigError::Syntax(_))
                    && chain.contains("proxy.tls")
                    && chain.contains("must be a mapping with cert_file and key_file"),
                "{name}: {chain}"
            );
        };
        for word in ["plain_http", "self_signed", "mutual"] {
            refused(word, &format!("  tls: {word}\n"));
        }
        for (name, keys) in [
            ("stray", "  tls:\n    cert_file: /a\n    chain: /c\n"),
            ("half", "  tls:\n    cert_file: /a\n"),
            (
                "twice",
                "  tls:\n    cert_file: /a\n    cert_file: /b\n    key_file: /c\n",
            ),
            ("empty", "  tls:\n"),
            ("tilde", "  tls: ~\n"),
            ("null", "  tls: null\n"),
        ] {
            refused(name, keys);
        }
        let admin = file_with_proxy(dir.path(), "admin-empty", "");
        under(&admin, "admin", "  tls:\n");
        let error = load_unresolved(&admin).unwrap_err();
        let chain = chain(&error);
        assert!(
            chain.contains("admin.tls") && chain.contains(TLS_SHAPES),
            "admin-empty: {chain}"
        );
    }

    #[test]
    fn a_listener_without_tls_loads_on_every_address_and_serves_plain_http() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("storage.key");
        std::fs::write(
            &key,
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        let binding = |name: &str, admin: &str, proxy: &str| {
            let path = dir.path().join(format!("{name}.yaml"));
            let yaml = format!(
                "admin:\n{admin}  secret_env: TEST_CFG_ADMIN\nproxy:\n{proxy}\
                 storage:\n  encryption_key_file: {}\n  path: {}\n",
                key.display(),
                dir.path().join("router.sqlite").display()
            );
            std::fs::write(&path, yaml).unwrap();
            path
        };
        let loopback = "  listen: 127.0.0.1:0\n";
        let both = load_unresolved(&binding("loopback", loopback, loopback)).unwrap();
        assert_eq!((&both.proxy.tls, &both.admin.tls), (&None, &None));
        assert_eq!(crate::tls::scheme(&None), "http");
        for unspecified in ["0.0.0.0:0", "\"[::]:0\""] {
            let bare = format!("  listen: {unspecified}\n");
            let proxy = load_unresolved(&binding("proxy-bare", loopback, &bare))
                .unwrap()
                .proxy;
            assert!(
                proxy.listen.ip().is_unspecified() && proxy.tls.is_none(),
                "{unspecified}: {proxy:?}"
            );
            assert_eq!(crate::tls::scheme(&proxy.tls), "http", "{unspecified}");
            let admin = load_unresolved(&binding("admin-bare", &bare, loopback))
                .unwrap()
                .admin;
            assert!(
                admin.listen.ip().is_unspecified() && admin.tls.is_none(),
                "{unspecified}: {admin:?}"
            );
            assert_eq!(crate::tls::scheme(&admin.tls), "http", "{unspecified}");
        }
    }

    /// Places `lines` at the top of `block` in a file a helper wrote: a key has to sit inside its block for serde
    /// to read it as that block's member rather than as a second top-level key.
    fn under(path: &std::path::Path, block: &str, lines: &str) {
        let text = std::fs::read_to_string(path).unwrap();
        let header = format!("{block}:\n");
        assert!(text.contains(&header), "the template has no {block} block");
        std::fs::write(path, text.replacen(&header, &format!("{header}{lines}"), 1)).unwrap();
    }

    /// The error's line and every cause beneath it, the way `check-config` prints them.
    fn chain(error: &ConfigError) -> String {
        let mut chain = error.to_string();
        let mut source = std::error::Error::source(error);
        while let Some(cause) = source {
            chain.push_str(": ");
            chain.push_str(&cause.to_string());
            source = cause.source();
        }
        chain
    }

    #[test]
    fn the_removed_admin_keys_are_refused_as_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        for (key, lines) in [
            (
                "origin",
                include_str!("../tests/fixtures/removed-keys/admin-origin.yaml"),
            ),
            (
                "plain_http",
                include_str!("../tests/fixtures/removed-keys/admin-plain-http.yaml"),
            ),
        ] {
            let path = file(dir.path(), key, "");
            under(&path, "admin", lines);
            let error = load_unresolved(&path).unwrap_err();
            let chain = chain(&error);
            assert!(
                matches!(error, ConfigError::Syntax(_))
                    && chain.contains("unknown field")
                    && chain.contains(&format!("`{key}`"))
                    && chain.contains("expected one of `listen`, `secret_env`, `tls`"),
                "{key}: {chain}"
            );
        }
    }

    #[test]
    fn the_routing_and_sessions_blocks_default_validate_and_reject_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("TEST_CFG_ADMIN", "admin-secret-001");
        let config = load(&file(dir.path(), "defaults", "")).unwrap();
        assert_eq!(
            (
                config.routing.cooldown_fallback_seconds,
                config.routing.model_rejection_seconds,
                config.routing.catalog_refresh_seconds,
                config.routing.startup_fetch_seconds
            ),
            (60, 3_600, 21_600, 30)
        );
        assert_eq!(
            (
                config.sessions.expire_after_seconds,
                config.stats.body_capture_max_hours
            ),
            (86_400, 72)
        );
        let config = load(&file(dir.path(), "explicit", "routing:\n  catalog_refresh_seconds: 3600\n  cooldown_fallback_seconds: 30\n  model_rejection_seconds: 1800\n  startup_fetch_seconds: 2\nsessions:\n  expire_after_seconds: 7200\nstats:\n  body_capture_max_hours: 24\n")).unwrap();
        assert_eq!(
            (
                config.routing.model_rejection_seconds,
                config.routing.catalog_refresh_seconds,
                config.routing.startup_fetch_seconds,
                config.sessions.expire_after_seconds,
                config.stats.body_capture_max_hours
            ),
            (1_800, 3_600, 2, 7_200, 24)
        );
        assert!(matches!(
            load(&file(
                dir.path(),
                "startup-zero",
                "routing:\n  startup_fetch_seconds: 0\n"
            )),
            Err(ConfigError::StartupFetchOutOfRange { value: 0 })
        ));
        assert!(matches!(
            load(&file(
                dir.path(),
                "startup-too-long",
                "routing:\n  startup_fetch_seconds: 61\n"
            )),
            Err(ConfigError::StartupFetchOutOfRange { value: 61 })
        ));
        assert!(matches!(
            load(&file(
                dir.path(),
                "refresh-too-short",
                "routing:\n  catalog_refresh_seconds: 59\n"
            )),
            Err(ConfigError::CatalogRefreshOutOfRange { value: 59 })
        ));
        assert!(matches!(
            load(&file(
                dir.path(),
                "refresh-too-long",
                "routing:\n  catalog_refresh_seconds: 604801\n"
            )),
            Err(ConfigError::CatalogRefreshOutOfRange { value: 604_801 })
        ));
        assert!(matches!(
            load(&file(
                dir.path(),
                "rejection-zero",
                "routing:\n  model_rejection_seconds: 0\n"
            )),
            Err(ConfigError::ModelRejectionOutOfRange { value: 0 })
        ));
        assert!(matches!(
            load(&file(
                dir.path(),
                "rejection-too-long",
                "routing:\n  model_rejection_seconds: 604801\n"
            )),
            Err(ConfigError::ModelRejectionOutOfRange { value: 604_801 })
        ));
        assert!(matches!(
            load(&file(
                dir.path(),
                "hours",
                "stats:\n  body_capture_max_hours: 0\n"
            )),
            Err(ConfigError::BodyCaptureMaxOutOfRange { .. })
        ));
        assert!(matches!(
            load(&file(dir.path(), "unknown", "routing:\n  strategy: x\n")),
            Err(ConfigError::Syntax(_))
        ));
        assert!(
            matches!(
                load(&file(dir.path(), "legacy", "accounts: []\nroutes: []\n")),
                Err(ConfigError::Syntax(_))
            ),
            "the registry blocks left the file"
        );
        assert!(
            matches!(
                load(&file(
                    dir.path(),
                    "unknown-key",
                    "upstream:\n  resolver: 1\n"
                )),
                Err(ConfigError::Syntax(_))
            ),
            "an unknown upstream key is refused"
        );
    }

    #[test]
    fn a_file_carrying_routing_max_attempts_is_refused_as_an_unknown_key() {
        let dir = tempfile::tempdir().unwrap();
        let error = load(&file(dir.path(), "budget", "routing:\n  max_attempts: 3\n")).unwrap_err();
        // `Syntax` displays a fixed text; the key is named by the serde error underneath it, which is what the
        // binary prints through anyhow's chain.
        let mut chain = error.to_string();
        let mut source = std::error::Error::source(&error);
        while let Some(cause) = source {
            chain.push_str(": ");
            chain.push_str(&cause.to_string());
            source = cause.source();
        }
        assert!(
            matches!(error, ConfigError::Syntax(_)) && chain.contains("max_attempts"),
            "the pool's size is the attempt budget; the key is refused by name: {chain}"
        );
    }

    #[test]
    fn a_key_no_block_carries_is_refused_at_load() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("TEST_CFG_ADMIN", "admin-secret-001");
        assert_eq!(
            load(&file(
                dir.path(),
                "sessions",
                "sessions:\n  expire_after_seconds: 7200\n"
            ))
            .unwrap()
            .sessions
            .expire_after_seconds,
            7_200
        );
        assert!(
            matches!(
                load(&file(
                    dir.path(),
                    "sessions-unknown",
                    "sessions:\n  expire_after_seconds: 7200\n  refresh_minutes: 5\n"
                )),
                Err(ConfigError::Syntax(_))
            ),
            "refresh_minutes is a key no build has ever carried under sessions, so it is refused"
        );
    }

    #[test]
    fn a_file_without_the_compatibility_quota_or_usage_block_loads_and_usage_takes_its_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("TEST_CFG_ADMIN", "admin-secret-001");
        let config = load(&file(dir.path(), "bare", "")).unwrap();
        assert_eq!(
            (
                config.usage.cache_ttl_seconds,
                config.usage.refresh_concurrency
            ),
            (60, 2)
        );
    }

    const REMOVED_COMPATIBILITY: &str =
        "compatibility:\n  profile: claude-code-2.1.269-linux-x64\n"; // removed name
    const REMOVED_QUOTA: &str = "quota:\n  sample_retention_days: 30\n"; // removed name
    const REMOVED_CATALOG_CACHE: &str = "routing:\n  catalog_cache_seconds: 60\n"; // removed name
    const REMOVED_VIEWER: &str = "  viewer_secret_env: PHOEBIS_VIEWER_SECRET\n"; // removed name

    #[test]
    fn a_file_naming_a_removed_key_is_refused_as_invalid_yaml_with_the_unknown_field_underneath() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("TEST_CFG_ADMIN", "admin-secret-001");
        // `file` already writes an `admin` block, and a second one is refused as a duplicate field before its keys
        // are read, so the removed key joins the first.
        let admin_block = format!("admin:\n{REMOVED_VIEWER}"); // removed name
        let admin = file(dir.path(), "admin", "");
        let text = std::fs::read_to_string(&admin).unwrap();
        std::fs::write(&admin, text.replacen("admin:\n", &admin_block, 1)).unwrap();
        for (name, path) in [
            (
                "compatibility",
                file(dir.path(), "compatibility", REMOVED_COMPATIBILITY),
            ),
            ("quota", file(dir.path(), "quota", REMOVED_QUOTA)),
            (
                "catalog-cache",
                file(dir.path(), "catalog-cache", REMOVED_CATALOG_CACHE),
            ),
            ("admin", admin),
        ] {
            let error = load(&path).unwrap_err();
            assert_eq!(
                error.to_string(),
                "the configuration is not valid YAML",
                "{name}"
            );
            let ConfigError::Syntax(cause) = &error else {
                panic!("{name}: {error:?}");
            };
            assert!(
                cause.to_string().contains("unknown field"),
                "{name}: {cause}"
            );
        }
    }

    #[test]
    fn the_cooldown_and_usage_bounds_refuse_outside_their_ranges_and_load_at_their_edges() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("TEST_CFG_ADMIN", "admin-secret-001");
        for (name, body, expected) in [
            (
                "cooldown-low",
                "routing:\n  cooldown_fallback_seconds: 0\n",
                "routing.cooldown_fallback_seconds must be between 1 and 3600, not 0",
            ),
            (
                "cooldown-high",
                "routing:\n  cooldown_fallback_seconds: 3601\n",
                "routing.cooldown_fallback_seconds must be between 1 and 3600, not 3601",
            ),
            (
                "ttl-low",
                "usage:\n  cache_ttl_seconds: 0\n",
                "usage.cache_ttl_seconds must be between 1 and 3600, not 0",
            ),
            (
                "ttl-high",
                "usage:\n  cache_ttl_seconds: 3601\n",
                "usage.cache_ttl_seconds must be between 1 and 3600, not 3601",
            ),
            (
                "concurrency-low",
                "usage:\n  refresh_concurrency: 0\n",
                "usage.refresh_concurrency must be between 1 and 16, not 0",
            ),
            (
                "concurrency-high",
                "usage:\n  refresh_concurrency: 17\n",
                "usage.refresh_concurrency must be between 1 and 16, not 17",
            ),
        ] {
            let error = load(&file(dir.path(), name, body)).unwrap_err();
            assert_eq!(error.to_string(), expected, "{name}");
        }
        for (name, body) in [
            ("cooldown-one", "routing:\n  cooldown_fallback_seconds: 1\n"),
            (
                "cooldown-hour",
                "routing:\n  cooldown_fallback_seconds: 3600\n",
            ),
            ("ttl-one", "usage:\n  cache_ttl_seconds: 1\n"),
            ("ttl-hour", "usage:\n  cache_ttl_seconds: 3600\n"),
            ("concurrency-one", "usage:\n  refresh_concurrency: 1\n"),
            ("concurrency-sixteen", "usage:\n  refresh_concurrency: 16\n"),
        ] {
            assert!(load(&file(dir.path(), name, body)).is_ok(), "{name}");
        }
    }

    #[test]
    fn the_session_memory_keys_default_to_the_constants_and_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("TEST_CFG_ADMIN", "admin-secret-001");
        let config = load(&file(dir.path(), "defaults", "")).unwrap();
        assert_eq!(
            (
                config.sessions.memory_cap,
                config.sessions.memory_idle_seconds,
                config.sessions.memory_retention_days
            ),
            (65_536, 86_400, 30)
        );
        for (name, body, expected) in [
            (
                "cap-low",
                "sessions:\n  memory_cap: 0\n",
                "sessions.memory_cap must be between 1 and 1048576, not 0",
            ),
            (
                "cap-high",
                "sessions:\n  memory_cap: 1048577\n",
                "sessions.memory_cap must be between 1 and 1048576, not 1048577",
            ),
            (
                "idle-low",
                "sessions:\n  memory_idle_seconds: 59\n",
                "sessions.memory_idle_seconds must be between 60 and 31536000, not 59",
            ),
            (
                "idle-high",
                "sessions:\n  memory_idle_seconds: 31536001\n",
                "sessions.memory_idle_seconds must be between 60 and 31536000, not 31536001",
            ),
            (
                "retention-low",
                "sessions:\n  memory_retention_days: 0\n",
                "sessions.memory_retention_days must be between 1 and 3650, not 0",
            ),
            (
                "retention-high",
                "sessions:\n  memory_retention_days: 3651\n",
                "sessions.memory_retention_days must be between 1 and 3650, not 3651",
            ),
        ] {
            let error = load(&file(dir.path(), name, body)).unwrap_err();
            assert_eq!(error.to_string(), expected, "{name}");
        }
        let custom = load(&file(
            dir.path(),
            "custom",
            "sessions:\n  memory_cap: 1024\n  memory_idle_seconds: 600\n  memory_retention_days: 7\n",
        ))
        .unwrap();
        assert_eq!(
            (
                custom.sessions.memory_cap,
                custom.sessions.memory_retention_days
            ),
            (1024, 7)
        );
    }

    /// `file`'s own template names `TEST_CFG_ADMIN`; this one takes the environment variable name as a
    /// parameter, so a secret-length or placeholder test never races another test's value under that name.
    fn file_with_secret_env(
        dir: &std::path::Path,
        name: &str,
        secret_env: &str,
    ) -> std::path::PathBuf {
        let key = dir.join("storage.key");
        std::fs::write(
            &key,
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        let yaml = format!(
            "admin:\n  listen: 127.0.0.1:0\n  secret_env: {secret_env}\n\
             proxy:\n  listen: 127.0.0.1:0\n\
             storage:\n  encryption_key_file: {}\n  path: {}\nusage:\n  cache_ttl_seconds: 60\n  \
             refresh_concurrency: 2\n",
            key.display(),
            dir.join("router.sqlite").display()
        );
        let path = dir.join(format!("{name}.yaml"));
        std::fs::write(&path, yaml).unwrap();
        path
    }

    #[test]
    fn a_secret_shorter_than_sixteen_characters_refuses_with_secret_too_short() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("TEST_CFG_SHORT", "twelve-chars");
        assert!(matches!(
            load(&file_with_secret_env(dir.path(), "short", "TEST_CFG_SHORT")),
            Err(ConfigError::SecretTooShort { name }) if name == "TEST_CFG_SHORT"
        ));
    }

    #[test]
    fn a_fifteen_character_secret_with_one_multibyte_character_still_refuses_as_too_short() {
        let dir = tempfile::tempdir().unwrap();
        // Fifteen characters, sixteen bytes: a byte count would wrongly read this as long enough.
        std::env::set_var("TEST_CFG_MULTIBYTE_SHORT", "fourteen-charsé");
        assert!(matches!(
            load(&file_with_secret_env(
                dir.path(),
                "multibyte-short",
                "TEST_CFG_MULTIBYTE_SHORT"
            )),
            Err(ConfigError::SecretTooShort { name }) if name == "TEST_CFG_MULTIBYTE_SHORT"
        ));
    }

    #[test]
    fn a_secret_with_a_non_ascii_character_or_a_surrounding_space_refuses_with_secret_shape() {
        let dir = tempfile::tempdir().unwrap();
        for (name, secret) in [
            ("TEST_CFG_SHAPE_NON_ASCII", "sixteen-chars-ok-é"),
            ("TEST_CFG_SHAPE_SPACES", " sixteen-chars-ok "),
            ("TEST_CFG_SHAPE_LEADING", " sixteen-chars-ok"),
            ("TEST_CFG_SHAPE_TRAILING", "sixteen-chars-ok "),
        ] {
            std::env::set_var(name, secret);
            let refused = load(&file_with_secret_env(dir.path(), name, name));
            assert!(
                matches!(&refused, Err(ConfigError::SecretShape { name: named }) if named == name),
                "{secret:?}: {refused:?}"
            );
        }
        std::env::set_var("TEST_CFG_SHAPE_OK", "sixteen-chars-ok");
        assert!(load(&file_with_secret_env(
            dir.path(),
            "shape-ok",
            "TEST_CFG_SHAPE_OK"
        ))
        .is_ok());
    }

    #[test]
    fn a_base64url_secret_init_writes_passes_the_shape() {
        let dir = tempfile::tempdir().unwrap();
        let secret = "VWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        assert_eq!(secret.len(), 43);
        std::env::set_var("TEST_CFG_BASE64URL", secret);
        assert!(load(&file_with_secret_env(
            dir.path(),
            "base64url",
            "TEST_CFG_BASE64URL"
        ))
        .is_ok());
    }

    #[test]
    fn the_example_files_placeholder_refuses_with_placeholder_secret() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(
            "TEST_CFG_PLACEHOLDER_ADMIN",
            "replace-with-a-long-random-string",
        );
        assert!(matches!(
            load(&file_with_secret_env(dir.path(), "placeholder-admin", "TEST_CFG_PLACEHOLDER_ADMIN")),
            Err(ConfigError::PlaceholderSecret { name }) if name == "TEST_CFG_PLACEHOLDER_ADMIN"
        ));
    }

    #[test]
    fn a_sixteen_character_random_secret_passes() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("TEST_CFG_VALID", "sixteen-char-key");
        assert!(load(&file_with_secret_env(dir.path(), "valid", "TEST_CFG_VALID")).is_ok());
    }
}
