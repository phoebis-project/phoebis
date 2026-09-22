//! The bootstrap file: the settings the process needs before it can serve anything, and the rules that
//! refuse a configuration nobody should be able to start.
//!
//! Secrets are named here, never written here: every one of them is the name of an environment variable
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
    pub compatibility: CompatConfig,
    /// Process-wide keepalive limits, read once at start; whether probes run, and their tuning, are per-pool
    /// registry settings.
    #[serde(default)]
    pub keepalive: KeepaliveLimits,
    pub proxy: ProxyConfig,
    #[serde(default)]
    pub quota: QuotaConfig,
    #[serde(default)]
    pub routing: RoutingConfig,
    #[serde(default)]
    pub sessions: SessionsConfig,
    #[serde(default)]
    pub stats: StatsConfig,
    pub storage: StorageConfig,
    #[serde(default)]
    pub upstream: UpstreamConfig,
    pub usage: UsageConfig,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminConfig {
    pub listen: SocketAddr,
    /// The URL origin the admin API compares every mutation's `Origin` header against.
    #[serde(default)]
    pub origin: Option<String>,
    /// Serves the session cookie without `Secure` and makes `origin` optional, on any listen address. Off
    /// loopback that puts the admin secret and the cookie on the wire in the clear, which is the operator's
    /// trust decision for the network the listener is published on, as it is for the proxy listener.
    #[serde(default)]
    pub plain_http: bool,
    #[serde(default = "unresolved", skip)]
    pub secret: Secret,
    pub secret_env: String,
    #[serde(default, skip)]
    pub viewer_secret: Option<Secret>,
    #[serde(default)]
    pub viewer_secret_env: Option<String>,
}

/// Of these four, `profile` and `cache_diagnostics` are consumed today: `outbound_gzip` has to stay off because
/// the native gzip gate is not implemented, and `usage_entrypoint` selects a `User-Agent` the usage service does
/// not take from configuration yet. Every field is still parsed so a file written for a later build is not
/// silently accepted with the wrong meaning.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompatConfig {
    /// Opt-in: it changes the wire shape toward Anthropic for every client behind the router.
    #[serde(default)]
    pub cache_diagnostics: bool,
    pub outbound_gzip: bool,
    pub profile: String,
    pub usage_entrypoint: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyConfig {
    pub listen: SocketAddr,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    /// A file holding exactly 32 raw bytes or 64 hex characters, outside the database it unseals.
    pub encryption_key_file: PathBuf,
    pub path: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageConfig {
    pub cache_ttl_seconds: u64,
    pub refresh_concurrency: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaConfig {
    /// How often every OAuth account's usage endpoint is polled for a quota sample. At least 900 s: the
    /// endpoint rate-limits per account, and header samples already cover active accounts.
    #[serde(default = "default_poll_interval")]
    pub poll_interval_seconds: u64,
    #[serde(default = "default_sample_retention")]
    pub sample_retention_days: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingConfig {
    #[serde(default = "default_catalog_interval")]
    pub catalog_sync_interval_seconds: u64,
    #[serde(default = "default_cooldown_fallback")]
    pub cooldown_fallback_seconds: u64,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u8,
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
    #[serde(default = "default_body_capture_max")]
    pub body_capture_max_hours: u32,
    #[serde(default = "default_ledger_path")]
    pub ledger_path: PathBuf,
    #[serde(default = "default_metrics_listen")]
    pub metrics_listen: SocketAddr,
    /// A price table that overrides the shipped one until an import is confirmed; a missing file is not an
    /// error, so the example path can stay in a fresh configuration.
    #[serde(default = "default_pricing_path")]
    pub pricing_path: Option<PathBuf>,
    /// Days the raw request, attempt, and event rows are kept; 0 keeps them forever.
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
    /// Days the hourly rollups are kept; 0 keeps them forever, and otherwise at least `retention_days`.
    #[serde(default = "default_rollup_retention_days")]
    pub rollup_retention_days: u32,
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
    #[error("stats.body_capture_max_hours must be between 1 and 168, not {value}")]
    BodyCaptureMaxOutOfRange { value: u32 },
    #[error("routing.catalog_sync_interval_seconds must be at least 300")]
    CatalogIntervalTooShort,
    #[error("{first} and {second} resolve to the same secret")]
    DuplicateSecret { first: String, second: String },
    #[error("compatibility.outbound_gzip must be false: the native gzip gate is not implemented")]
    GzipUnsupported,
    #[error("{0}")]
    Keepalive(#[from] LimitsError),
    #[error("stats.ledger_path {path} must name a database file inside a directory")]
    LedgerPath { path: PathBuf },
    #[error("routing.max_attempts must be between 1 and 8, not {value}")]
    MaxAttemptsOutOfRange { value: u8 },
    #[error("sessions.memory_cap must be between 1 and 1048576, not {value}")]
    MemoryCapOutOfRange { value: usize },
    #[error("sessions.memory_idle_seconds must be at least 60")]
    MemoryIdleTooShort,
    #[error("sessions.memory_retention_days must be at least 1")]
    MemoryRetentionTooShort,
    #[error("admin.origin is required unless admin.plain_http is set")]
    MissingOrigin,
    #[error("environment variable {name} is unset or empty")]
    MissingSecret { name: String },
    #[error("quota.poll_interval_seconds must be at least 900")]
    PollIntervalTooShort,
    #[error("cannot read {path}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{field} must be at least 1")]
    RetentionTooShort { field: &'static str },
    #[error("stats.rollup_retention_days must be 0 or at least stats.retention_days ({retention_days}), not {value}")]
    RollupRetentionTooShort { retention_days: u32, value: u32 },
    #[error("sessions.expire_after_seconds must be at least 60")]
    SessionExpiryTooShort,
    #[error("stats.ledger_path and storage.path must name different files, not {path}")]
    SharedDatabasePath { path: PathBuf },
    #[error("storage.path {path} must name a database file inside a directory")]
    StoragePath { path: PathBuf },
    #[error("the configuration is not valid YAML")]
    Syntax(#[from] serde_norway::Error),
    #[error("compatibility.profile {name} is not a profile this build carries")]
    UnknownProfile { name: String },
    #[error("compatibility.usage_entrypoint must be cli or sdk-cli, not {value}")]
    UnknownUsageEntrypoint { value: String },
    #[error("usage.refresh_concurrency must be at least 1")]
    ZeroRefreshConcurrency,
}

/// Reads, parses, and validates the bootstrap file, resolving every secret from the environment.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let mut config: Config = serde_norway::from_str(&text)?;
    check_shape(&config)?;
    resolve_secrets(&mut config)?;
    check_secrets_are_distinct(&config)?;
    Ok(config)
}

impl fmt::Debug for AdminConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdminConfig")
            .field("listen", &self.listen)
            .field("origin", &self.origin)
            .field("plain_http", &self.plain_http)
            .field("secret", &"***")
            .field("secret_env", &self.secret_env)
            .field("viewer_secret", &self.viewer_secret.as_ref().map(|_| "***"))
            .field("viewer_secret_env", &self.viewer_secret_env)
            .finish()
    }
}

impl Default for QuotaConfig {
    fn default() -> Self {
        QuotaConfig {
            poll_interval_seconds: default_poll_interval(),
            sample_retention_days: default_sample_retention(),
        }
    }
}

impl Default for RoutingConfig {
    fn default() -> Self {
        RoutingConfig {
            catalog_sync_interval_seconds: default_catalog_interval(),
            cooldown_fallback_seconds: default_cooldown_fallback(),
            max_attempts: default_max_attempts(),
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
            body_capture_max_hours: default_body_capture_max(),
            ledger_path: default_ledger_path(),
            metrics_listen: default_metrics_listen(),
            pricing_path: default_pricing_path(),
            retention_days: default_retention_days(),
            rollup_retention_days: default_rollup_retention_days(),
        }
    }
}

fn check_shape(config: &Config) -> Result<(), ConfigError> {
    if phoebis_profile::by_name(&config.compatibility.profile).is_none() {
        return Err(ConfigError::UnknownProfile {
            name: config.compatibility.profile.clone(),
        });
    }
    if config.compatibility.outbound_gzip {
        return Err(ConfigError::GzipUnsupported);
    }
    if !matches!(
        config.compatibility.usage_entrypoint.as_str(),
        "cli" | "sdk-cli"
    ) {
        return Err(ConfigError::UnknownUsageEntrypoint {
            value: config.compatibility.usage_entrypoint.clone(),
        });
    }
    if !config.admin.plain_http && config.admin.origin.is_none() {
        // Without an origin to compare against, the admin API has nothing to tell a browser's cross-site
        // write from an operator's own.
        return Err(ConfigError::MissingOrigin);
    }
    if config.usage.refresh_concurrency == 0 {
        return Err(ConfigError::ZeroRefreshConcurrency);
    }
    // The store creates the database under exactly this name, in the directory that holds it, so a path
    // that names one of the two and not the other is not a path it can honor.
    if config.storage.path.parent().is_none() || config.storage.path.file_name().is_none() {
        return Err(ConfigError::StoragePath {
            path: config.storage.path.clone(),
        });
    }

    if !(1..=8).contains(&config.routing.max_attempts) {
        return Err(ConfigError::MaxAttemptsOutOfRange {
            value: config.routing.max_attempts,
        });
    }
    config.keepalive.validate()?;
    // A sync walks every account against its upstream, so a short interval turns into a standing load on
    // hosts that change their model list a few times a year.
    if config.routing.catalog_sync_interval_seconds < 300 {
        return Err(ConfigError::CatalogIntervalTooShort);
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
    if config.sessions.memory_idle_seconds < 60 {
        return Err(ConfigError::MemoryIdleTooShort);
    }
    if config.sessions.memory_retention_days < 1 {
        return Err(ConfigError::MemoryRetentionTooShort);
    }
    if !(1..=168).contains(&config.stats.body_capture_max_hours) {
        return Err(ConfigError::BodyCaptureMaxOutOfRange {
            value: config.stats.body_capture_max_hours,
        });
    }
    if config.quota.poll_interval_seconds < 900 {
        return Err(ConfigError::PollIntervalTooShort);
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
    if config.quota.sample_retention_days == 0 {
        return Err(ConfigError::RetentionTooShort {
            field: "quota.sample_retention_days",
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

fn resolve_secrets(config: &mut Config) -> Result<(), ConfigError> {
    config.admin.secret = from_env(&config.admin.secret_env)?;
    config.admin.viewer_secret = match &config.admin.viewer_secret_env {
        None => None,
        Some(name) => Some(from_env(name)?),
    };
    Ok(())
}

/// A viewer secret equal to the administration one silently turns the read-only login into a full one,
/// which is a configuration mistake that only shows up as a surprise in production.
fn check_secrets_are_distinct(config: &Config) -> Result<(), ConfigError> {
    let mut held: Vec<(String, &Secret)> =
        vec![("the admin secret".to_string(), &config.admin.secret)];
    if let Some(viewer) = &config.admin.viewer_secret {
        held.push(("the viewer secret".to_string(), viewer));
    }
    for (index, (first, left)) in held.iter().enumerate() {
        for (second, right) in held.iter().skip(index + 1) {
            if left.expose() == right.expose() {
                return Err(ConfigError::DuplicateSecret {
                    first: first.clone(),
                    second: second.clone(),
                });
            }
        }
    }
    Ok(())
}

fn from_env(name: &str) -> Result<Secret, ConfigError> {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Ok(Secret::new(&value)),
        // The value is never part of the error: a variable set to something unusable still holds a secret.
        _ => Err(ConfigError::MissingSecret {
            name: name.to_string(),
        }),
    }
}

fn default_body_capture_max() -> u32 {
    72
}

fn default_catalog_interval() -> u64 {
    86_400
}

fn default_cooldown_fallback() -> u64 {
    60
}

fn default_ledger_path() -> PathBuf {
    "./data/ledger.sqlite".into()
}

fn default_max_attempts() -> u8 {
    3
}

fn default_metrics_listen() -> SocketAddr {
    ([127, 0, 0, 1], 9464).into()
}

fn default_poll_interval() -> u64 {
    3_600
}

fn default_pricing_path() -> Option<PathBuf> {
    Some("./pricing.yaml".into())
}

fn default_retention_days() -> u32 {
    90
}

fn default_rollup_retention_days() -> u32 {
    400
}

fn default_sample_retention() -> u32 {
    30
}

fn default_session_expiry() -> u64 {
    86_400
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
        let yaml = format!("admin:\n  listen: 127.0.0.1:0\n  secret_env: TEST_CFG_ADMIN\n  plain_http: true\ncompatibility:\n  outbound_gzip: false\n  profile: claude-code-2.1.269-linux-x64\n  usage_entrypoint: cli\nproxy:\n  listen: 127.0.0.1:0\nstorage:\n  encryption_key_file: {}\n  path: {}\nusage:\n  cache_ttl_seconds: 60\n  refresh_concurrency: 2\n{body}", key.display(), dir.join("router.sqlite").display());
        let path = dir.join(format!("{name}.yaml"));
        std::fs::write(&path, yaml).unwrap();
        path
    }

    /// `file`'s own template fixes the `compatibility` block, so this builds a whole document with a
    /// caller-chosen one instead of appending a second, ambiguous top-level key.
    fn file_with_compat(
        dir: &std::path::Path,
        name: &str,
        compatibility: &str,
    ) -> std::path::PathBuf {
        let key = dir.join("storage.key");
        std::fs::write(
            &key,
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        let yaml = format!(
            "admin:\n  listen: 127.0.0.1:0\n  secret_env: TEST_CFG_ADMIN\n  plain_http: true\n\
             compatibility:\n{compatibility}\nproxy:\n  listen: 127.0.0.1:0\nstorage:\n  \
             encryption_key_file: {}\n  path: {}\nusage:\n  cache_ttl_seconds: 60\n  refresh_concurrency: 2\n",
            key.display(),
            dir.join("router.sqlite").display()
        );
        let path = dir.join(format!("{name}.yaml"));
        std::fs::write(&path, yaml).unwrap();
        path
    }

    #[test]
    fn cache_diagnostics_parses_when_set_and_defaults_to_false() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("TEST_CFG_ADMIN", "admin-secret");
        let without = "  outbound_gzip: false\n  profile: claude-code-2.1.269-linux-x64\n  usage_entrypoint: cli";
        assert!(
            !load(&file_with_compat(dir.path(), "absent", without))
                .unwrap()
                .compatibility
                .cache_diagnostics
        );
        let with = "  cache_diagnostics: true\n  outbound_gzip: false\n  \
                    profile: claude-code-2.1.269-linux-x64\n  usage_entrypoint: cli";
        assert!(
            load(&file_with_compat(dir.path(), "set", with))
                .unwrap()
                .compatibility
                .cache_diagnostics
        );
    }

    #[test]
    fn the_routing_and_sessions_blocks_default_validate_and_reject_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("TEST_CFG_ADMIN", "admin-secret");
        let config = load(&file(dir.path(), "defaults", "")).unwrap();
        assert_eq!(
            (
                config.routing.max_attempts,
                config.routing.cooldown_fallback_seconds,
                config.routing.catalog_sync_interval_seconds
            ),
            (3, 60, 86_400)
        );
        assert_eq!(
            (
                config.sessions.expire_after_seconds,
                config.stats.body_capture_max_hours
            ),
            (86_400, 72)
        );
        let config = load(&file(dir.path(), "explicit", "routing:\n  catalog_sync_interval_seconds: 3600\n  cooldown_fallback_seconds: 30\n  max_attempts: 5\nsessions:\n  expire_after_seconds: 7200\nstats:\n  body_capture_max_hours: 24\n")).unwrap();
        assert_eq!(
            (
                config.routing.max_attempts,
                config.sessions.expire_after_seconds,
                config.stats.body_capture_max_hours
            ),
            (5, 7_200, 24)
        );
        assert!(matches!(
            load(&file(dir.path(), "zero", "routing:\n  max_attempts: 0\n")),
            Err(ConfigError::MaxAttemptsOutOfRange { value: 0 })
        ));
        assert!(matches!(
            load(&file(dir.path(), "nine", "routing:\n  max_attempts: 9\n")),
            Err(ConfigError::MaxAttemptsOutOfRange { value: 9 })
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
            load(&file(
                dir.path(),
                "unknown",
                "routing:\n  max_attempts: 3\n  strategy: x\n"
            )),
            Err(ConfigError::Syntax(_))
        ));
        assert!(
            matches!(
                load(&file(dir.path(), "legacy", "accounts: []\nroutes: []\n")),
                Err(ConfigError::Syntax(_))
            ),
            "the registry blocks left the file"
        );
    }

    #[test]
    fn the_session_memory_keys_default_to_the_constants_and_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("TEST_CFG_ADMIN", "admin-secret");
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
                "idle",
                "sessions:\n  memory_idle_seconds: 59\n",
                "sessions.memory_idle_seconds must be at least 60",
            ),
            (
                "retention",
                "sessions:\n  memory_retention_days: 0\n",
                "sessions.memory_retention_days must be at least 1",
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
}
