//! The `phoebis` command line: run the router, or check a configuration without starting anything.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "phoebis",
    version,
    about = "Relays Claude Code traffic to Anthropic"
)]
struct Cli {
    #[arg(
        long,
        global = true,
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
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = phoebis_router::config::load(&cli.config)?;
    match cli.command {
        Some(Command::CheckConfig) => {
            println!("{} is valid", cli.config.display());
            for account in &config.accounts {
                println!("account {} ({})", account.id, account.kind.as_str());
            }
            for route in &config.routes {
                println!("route {} spends account {}", route.key_id, route.account_id);
            }
            Ok(())
        }
        None => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(phoebis_router::app::run(config, async {
                if let Err(error) = tokio::signal::ctrl_c().await {
                    // A handler that could not be installed is not a request to stop, so the process keeps
                    // serving and says why no interrupt will reach it.
                    tracing::error!(%error, "no interrupt handler; stop this process by other means");
                    std::future::pending::<()>().await;
                }
            })),
    }
}
