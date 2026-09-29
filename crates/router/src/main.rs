//! The `phoebis` command line: run the router, check a configuration without starting anything, or probe a
//! running proxy listener.

use std::path::PathBuf;

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
    /// Validate the configuration and list what it binds, without opening the store or a listener.
    CheckConfig,
    /// Probe the proxy listener's `/api/hello`: exit 0 when it answers 200, 1 otherwise.
    Health,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = phoebis_router::config::load(&cli.config)?;
    match cli.command {
        Some(Command::CheckConfig) => {
            println!("{} is valid", cli.config.display());
            println!("proxy listens on {}", config.proxy.listen);
            println!("admin listens on {}", config.admin.listen);
            println!("metrics listens on {}", config.stats.metrics_listen);
            println!("profile {}", config.compatibility.profile);
            println!(
                "routing: max_attempts {}, cooldown_fallback {}s, model_rejection {}s, catalog cache {}s, \
                 catalog refresh {}s, startup fetch {}s",
                config.routing.max_attempts,
                config.routing.cooldown_fallback_seconds,
                config.routing.model_rejection_seconds,
                config.routing.catalog_cache_seconds,
                config.routing.catalog_refresh_seconds,
                config.routing.startup_fetch_seconds
            );
            println!("ledger at {}", config.stats.ledger_path.display());
            let days = |n: u32| if n == 0 { "forever".to_string() } else { format!("{n} days") };
            println!(
                "retention {}, rollups {}, samples {} days",
                days(config.stats.retention_days),
                days(config.stats.rollup_retention_days),
                config.quota.sample_retention_days
            );
            println!(
                "keepalive: lease {}s, {} sessions, {} agents, {} workers, {} MiB retained, oauth prewarm {}",
                config.keepalive.lease_seconds,
                config.keepalive.max_sessions,
                config.keepalive.max_agents,
                config.keepalive.max_workers,
                config.keepalive.max_retained_bytes >> 20,
                if config.keepalive.oauth_prewarm_verified {
                    "verified"
                } else {
                    "unverified"
                }
            );
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
