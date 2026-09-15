//! The bootstrap file: the settings the process needs before it can serve anything, and the rules that
//! refuse a configuration nobody should be able to start.
//!
//! Secrets are named here, never written here: every one of them is the name of an environment variable
//! that `load` resolves, so a configuration file can be read by anyone who may read the deployment.

use std::collections::HashSet;
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use phoebis_store::Secret;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub admin: AdminConfig,
    pub compatibility: CompatConfig,
    pub proxy: ProxyConfig,
    pub storage: StorageConfig,
    pub usage: UsageConfig,
    pub accounts: Vec<AccountConfig>,
    pub routes: Vec<RouteConfig>,
    #[serde(default)]
    pub upstream: UpstreamConfig,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminConfig {
    /// Serves the session cookie without `Secure` over cleartext, which only a loopback listener makes
    /// defensible.
    #[serde(default)]
    pub dev_loopback_http: bool,
    pub listen: SocketAddr,
    /// The URL origin the admin API compares every mutation's `Origin` header against.
    #[serde(default)]
    pub origin: Option<String>,
    #[serde(default = "unresolved", skip)]
    pub secret: Secret,
    pub secret_env: String,
    #[serde(default, skip)]
    pub viewer_secret: Option<Secret>,
    #[serde(default)]
    pub viewer_secret_env: Option<String>,
}

/// Of these three, only `profile` is consumed today: `outbound_gzip` has to stay off because the native
/// gzip gate is not implemented, and `usage_entrypoint` selects a `User-Agent` the usage service does not
/// take from configuration yet. Both are parsed and validated so a file written for a later build is not
/// silently accepted with the wrong meaning.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompatConfig {
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountConfig {
    pub id: String,
    pub kind: AccountKind,
    pub label: String,
    #[serde(default, skip)]
    pub secret: Option<Secret>,
    #[serde(default)]
    pub secret_env: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    ApiKey,
    Oauth,
}

/// One downstream client's key and the account it may spend.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    pub account_id: String,
    pub key_id: String,
    #[serde(default = "unresolved", skip)]
    pub secret: Secret,
    pub secret_env: String,
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
    #[error("account {account_id} is an api_key account, so it needs secret_env")]
    AccountSecretMissing { account_id: String },
    #[error("account {account_id} is an oauth account, whose credentials come from pairing rather than from secret_env")]
    AccountSecretUnused { account_id: String },
    #[error("admin.dev_loopback_http serves the session cookie over cleartext, so admin.listen must be a loopback address, not {listen}")]
    DevModeNotLoopback { listen: SocketAddr },
    #[error("account id {id} is configured twice")]
    DuplicateAccount { id: String },
    #[error("route key_id {key_id} is configured twice")]
    DuplicateKeyId { key_id: String },
    #[error("{first} and {second} resolve to the same secret")]
    DuplicateSecret { first: String, second: String },
    #[error("compatibility.outbound_gzip must be false: the native gzip gate is not implemented")]
    GzipUnsupported,
    #[error("admin.origin is required unless admin.dev_loopback_http is set")]
    MissingOrigin,
    #[error("environment variable {name} is unset or empty")]
    MissingSecret { name: String },
    #[error("cannot read {path}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("storage.path {path} must name a database file inside a directory")]
    StoragePath { path: PathBuf },
    #[error("the configuration is not valid YAML")]
    Syntax(#[from] serde_norway::Error),
    #[error("route {key_id} names account {account_id}, which is not configured")]
    UnknownAccount { account_id: String, key_id: String },
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

impl AccountKind {
    /// The spelling the bootstrap file uses, for messages that quote configuration back at an operator.
    pub fn as_str(self) -> &'static str {
        match self {
            AccountKind::ApiKey => "api_key",
            AccountKind::Oauth => "oauth",
        }
    }

    pub fn store_kind(self) -> phoebis_store::AccountKind {
        match self {
            AccountKind::ApiKey => phoebis_store::AccountKind::ApiKey,
            AccountKind::Oauth => phoebis_store::AccountKind::Oauth,
        }
    }
}

impl fmt::Debug for AdminConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdminConfig")
            .field("dev_loopback_http", &self.dev_loopback_http)
            .field("listen", &self.listen)
            .field("origin", &self.origin)
            .field("secret", &"***")
            .field("secret_env", &self.secret_env)
            .field("viewer_secret", &self.viewer_secret.as_ref().map(|_| "***"))
            .field("viewer_secret_env", &self.viewer_secret_env)
            .finish()
    }
}

impl fmt::Debug for AccountConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccountConfig")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("label", &self.label)
            .field("secret", &self.secret.as_ref().map(|_| "***"))
            .field("secret_env", &self.secret_env)
            .finish()
    }
}

impl fmt::Debug for RouteConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RouteConfig")
            .field("account_id", &self.account_id)
            .field("key_id", &self.key_id)
            .field("secret", &"***")
            .field("secret_env", &self.secret_env)
            .finish()
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
    if config.admin.dev_loopback_http {
        if !config.admin.listen.ip().is_loopback() {
            return Err(ConfigError::DevModeNotLoopback {
                listen: config.admin.listen,
            });
        }
    } else if config.admin.origin.is_none() {
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

    let mut ids = HashSet::new();
    for account in &config.accounts {
        if !ids.insert(account.id.as_str()) {
            return Err(ConfigError::DuplicateAccount {
                id: account.id.clone(),
            });
        }
        match (account.kind, &account.secret_env) {
            (AccountKind::ApiKey, None) => {
                return Err(ConfigError::AccountSecretMissing {
                    account_id: account.id.clone(),
                })
            }
            (AccountKind::Oauth, Some(_)) => {
                return Err(ConfigError::AccountSecretUnused {
                    account_id: account.id.clone(),
                })
            }
            _ => {}
        }
    }

    let mut key_ids = HashSet::new();
    for route in &config.routes {
        if !key_ids.insert(route.key_id.as_str()) {
            return Err(ConfigError::DuplicateKeyId {
                key_id: route.key_id.clone(),
            });
        }
        if !ids.contains(route.account_id.as_str()) {
            return Err(ConfigError::UnknownAccount {
                account_id: route.account_id.clone(),
                key_id: route.key_id.clone(),
            });
        }
    }
    Ok(())
}

fn resolve_secrets(config: &mut Config) -> Result<(), ConfigError> {
    config.admin.secret = from_env(&config.admin.secret_env)?;
    config.admin.viewer_secret = match &config.admin.viewer_secret_env {
        None => None,
        Some(name) => Some(from_env(name)?),
    };
    for account in &mut config.accounts {
        account.secret = match &account.secret_env {
            None => None,
            Some(name) => Some(from_env(name)?),
        };
    }
    for route in &mut config.routes {
        route.secret = from_env(&route.secret_env)?;
    }
    Ok(())
}

/// Two routes sharing a secret collapse into one key-table entry, so one account binding silently wins;
/// an administration secret that doubles as a route secret turns an inference key into an administration
/// login. Both are configuration mistakes that only show up as a surprise in production.
fn check_secrets_are_distinct(config: &Config) -> Result<(), ConfigError> {
    let mut held: Vec<(String, &Secret)> =
        vec![("the admin secret".to_string(), &config.admin.secret)];
    if let Some(viewer) = &config.admin.viewer_secret {
        held.push(("the viewer secret".to_string(), viewer));
    }
    for route in &config.routes {
        held.push((format!("route {}", route.key_id), &route.secret));
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

/// Stands in for a secret until `load` resolves it. `load` is the only way to obtain a `Config`, and it
/// replaces every one of these before it returns.
fn unresolved() -> Secret {
    Secret::new("")
}
