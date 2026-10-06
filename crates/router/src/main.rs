//! The `phoebis` command line: run the router, check a configuration without starting anything, or probe a
//! running proxy listener.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "phoebis",
    version,
    about = "Relays Claude Code traffic to Anthropic"
)]
struct Cli {
    /// The bootstrap file; `PHOEBIS_CONFIG` names it when the flag is absent.
    #[arg(
        long,
        global = true,
        env = "PHOEBIS_CONFIG",
        default_value = "./config.yaml",
        value_name = "PATH"
    )]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Validate the configuration, the key file, the registry's key canary and the registry's and the ledger's
    /// schema versions, taking no lock and opening no socket.
    CheckConfig,
    /// Probe the proxy listener's `/api/ready`: exit 0 when it answers 200, 1 otherwise.
    Health,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let mut config = match cli.command {
        Some(Command::CheckConfig) => phoebis_router::config::load_unresolved(&cli.config)?,
        _ => phoebis_router::config::load(&cli.config)?,
    };
    match cli.command {
        Some(Command::CheckConfig) => {
            println!("{} is valid", cli.config.display());
            println!("proxy listens on {}", config.proxy.listen);
            println!("admin listens on {}", config.admin.listen);
            println!("metrics listens on {}", config.stats.metrics_listen);
            println!(
                "proxy: body budget {} bytes, body read {}s, key body reads {}, max request {} bytes, \
                 sign cch placeholder {}, upstream gzip {}",
                config.proxy.body_budget_bytes,
                config.proxy.body_read_seconds,
                config.proxy.key_body_reads,
                config.proxy.max_request_bytes,
                if config.proxy.sign_cch_placeholder { "on" } else { "off" },
                config.proxy.upstream_gzip
            );
            println!(
                "profile {}",
                phoebis_profile::claude_code_2_1_269_linux_x64().name
            );
            println!(
                "routing: cooldown_fallback {}s, model_rejection {}s, catalog refresh {}s, startup fetch {}s",
                config.routing.cooldown_fallback_seconds,
                config.routing.model_rejection_seconds,
                config.routing.catalog_refresh_seconds,
                config.routing.startup_fetch_seconds
            );
            println!("ledger at {}", config.stats.ledger_path.display());
            // The start refuses a price file or a cost model it cannot use, so the check reads both the same way.
            let pricing = phoebis_ledger::Pricing::load(config.stats.api_prices_path.as_deref()).with_context(|| {
                format!(
                    "loading the price table from {}",
                    config
                        .stats
                        .api_prices_path
                        .as_deref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "the built-in table".to_string())
                )
            })?;
            let prices = pricing.current();
            println!(
                "api prices: {}",
                table_line(
                    &prices.source,
                    &prices.effective,
                    config.stats.api_prices_path.as_deref(),
                    format!("{} models", prices.models.len())
                )
            );
            let tables = phoebis_ledger::PlanTables::load(config.stats.subscription_costs_path.as_deref())
                .with_context(|| {
                    format!(
                        "loading the subscription cost model from {}",
                        config
                            .stats
                            .subscription_costs_path
                            .as_deref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "the built-in model".to_string())
                    )
                })?;
            let costs = &tables.costs;
            // A model the overrides weigh on a window of their own prices there even without a shared weight.
            let weighted = costs
                .model_weight
                .iter()
                .filter(|(_, weight)| weight.is_some_and(|w| w > 0.0))
                .map(|(model, _)| model)
                .chain(
                    costs
                        .overrides
                        .iter()
                        .filter(|(_, o)| o.weight_by_window.as_ref().is_some_and(|w| !w.is_empty()))
                        .map(|(model, _)| model),
                )
                .collect::<std::collections::HashSet<_>>()
                .len();
            let sized = costs
                .capacity
                .values()
                .filter(|c| c.five_hour.is_some() || c.seven_day.is_some())
                .count();
            println!(
                "subscription costs: {}",
                table_line(
                    &tables.source,
                    &costs.effective,
                    config.stats.subscription_costs_path.as_deref(),
                    format!("{weighted} models, {sized} tiers")
                )
            );
            let days = |n: u32| if n == 0 { "forever".to_string() } else { format!("{n} days") };
            println!(
                "retention {}, rollups {}",
                days(config.stats.retention_days),
                days(config.stats.rollup_retention_days)
            );
            println!(
                "keepalive: {} sessions, {} agents, {} MiB retained",
                config.keepalive.max_sessions,
                config.keepalive.max_agents,
                config.keepalive.max_retained_bytes >> 20
            );
            let key_file = &config.storage.encryption_key_file;
            let key = phoebis_store::StorageKey::from_file(key_file).with_context(|| {
                format!("reading storage.encryption_key_file {}", key_file.display())
            })?;
            let registry = &config.storage.path;
            // The mismatch names the registry itself and is the whole finding; any other failure (an unreadable
            // directory, a file that is not a database) says nothing of which file it met.
            let canary = phoebis_store::Store::read_canary(registry, &key).map_err(|error| match error {
                phoebis_store::StoreError::KeyMismatch { .. } => anyhow::Error::new(error),
                error => anyhow::Error::new(error)
                    .context(format!("reading the registry at {}", registry.display())),
            })?;
            match canary {
                phoebis_store::CanaryRead::NoRegistry => println!(
                    "registry at {} does not exist yet; the first start creates it",
                    registry.display()
                ),
                phoebis_store::CanaryRead::SealedRowMatches => println!(
                    "registry at {} has no key canary yet; a sealed row opens under the key, and the next \
                     start seals one",
                    registry.display()
                ),
                phoebis_store::CanaryRead::Absent | phoebis_store::CanaryRead::Matches => {}
            }
            // The start refuses a file a newer build wrote; reading both versions here, read-only and taking no
            // lock, finds that before anything starts.
            let found = phoebis_store::Store::read_schema_version(registry)
                .with_context(|| format!("reading the registry at {}", registry.display()))?;
            if let Some(found) = found.filter(|found| *found > phoebis_store::SCHEMA_VERSION) {
                bail!(
                    "the registry at {} is at schema {found}, newer than the {} this build reads",
                    registry.display(),
                    phoebis_store::SCHEMA_VERSION
                );
            }
            let ledger = &config.stats.ledger_path;
            let found = phoebis_ledger::LedgerDb::read_schema_version(ledger)
                .with_context(|| format!("reading the ledger at {}", ledger.display()))?;
            let supported = phoebis_ledger::LedgerDb::supported_schema_version();
            if let Some(found) = found.filter(|found| *found > supported) {
                bail!(
                    "the ledger at {} is at schema {found}, newer than the {supported} this build reads",
                    ledger.display()
                );
            }
            let name = &config.admin.secret_env;
            if phoebis_router::config::secret_is_unset(name) {
                println!("warning: environment variable {name} is unset or empty");
            }
            match phoebis_router::config::resolve_secrets(&mut config) {
                // An unset variable was warned about above: the file is checked without the unit's environment.
                // A set one the start refuses (too short, the example's placeholder) is the same finding here.
                Ok(()) | Err(phoebis_router::config::ConfigError::MissingSecret { .. }) => {}
                Err(error) => return Err(error.into()),
            }
            for warning in phoebis_store::permissions::check_private_file(&config.storage.encryption_key_file, 0o600) {
                println!("warning: {warning}");
            }
            let data_dir = phoebis_store::permissions::data_dir_of(&config.storage.path);
            for warning in phoebis_store::permissions::check_private_dir(data_dir) {
                println!("warning: {warning}");
            }
            for warning in phoebis_store::permissions::check_private_file(&config.storage.path, 0o600) {
                println!("warning: {warning}");
            }
            Ok(())
        }
        Some(Command::Health) => match phoebis_router::health::probe(config.proxy.listen, phoebis_router::health::TIMEOUT) {
            Ok(addr) => {
                println!("proxy answers on {addr}");
                Ok(())
            }
            Err(reason) => {
                println!("{reason}");
                std::process::exit(1)
            }
        },
        None => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(phoebis_router::app::run(config, async {
                let ctrl_c = tokio::signal::ctrl_c();
                let mut terminate =
                    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                        Ok(signal) => signal,
                        Err(error) => {
                            tracing::error!(%error, "no SIGTERM handler; a service manager's stop will not drain");
                            match ctrl_c.await {
                                Ok(()) => return,
                                Err(error) => {
                                    tracing::error!(%error, "no interrupt handler; stop this process by other means");
                                    return std::future::pending::<()>().await;
                                }
                            }
                        }
                    };
                tokio::select! {
                    result = ctrl_c => {
                        if let Err(error) = result {
                            // A handler that could not be installed is not a request to stop, so the process
                            // keeps serving and says why no interrupt will reach it.
                            tracing::error!(%error, "no interrupt handler; stop this process by other means");
                            std::future::pending::<()>().await;
                        }
                    }
                    _ = terminate.recv() => {}
                }
            })),
    }
}

/// One table's `check-config` line: the file read, or the built-in table with the configured path it found nothing
/// at, so an operator can tell a missing override from a misspelled one.
fn table_line(source: &str, effective: &str, path: Option<&Path>, counts: String) -> String {
    match (source, path) {
        ("file", Some(path)) => {
            format!("{} (file, effective {effective}, {counts})", path.display())
        }
        (_, Some(path)) => format!(
            "built in (effective {effective}, {counts}); {} not found",
            path.display()
        ),
        (_, None) => format!("built in (effective {effective}, {counts})"),
    }
}
