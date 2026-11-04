//! The `phoebis` command line: run the router, write a first installation's files, check a configuration without
//! starting anything, probe a running proxy listener, read a running router's readiness, pools, accounts and pins,
//! have it re-read its listeners' certificate pairs, copy both databases into a backup, or discard a stopped
//! registry's secrets after a lost storage key.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{Parser, Subcommand};
use phoebis_router::admin_client::{self, AdminClient, Common};
use phoebis_router::backup::{self, BackupArgs};
use phoebis_router::config::Config;
use phoebis_router::recover_key::{self, RecoverKeyArgs};
use phoebis_router::status::{self, Listeners, StatusArgs};
use phoebis_router::tls_reload::{self, TlsReloadArgs};
use phoebis_router::{app, init, tls};

#[derive(Parser)]
#[command(
    name = "phoebis",
    version,
    about = "Relays Claude Code traffic to Anthropic"
)]
struct Cli {
    /// The bootstrap file. `PHOEBIS_CONFIG` names it when the flag is absent.
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

// Parsed once per process, so the init flags' size costs nothing worth a box.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum Command {
    /// Copy both databases into a directory with a manifest that names the key file, taking no lock and opening no
    /// socket: exit 0 once the manifest is written, 1 otherwise.
    Backup(BackupArgs),
    /// Validate the configuration, the key file, the registry's key canary, the registry's and the ledger's schema
    /// versions and the certificate pairs, taking no lock and opening no socket.
    CheckConfig,
    /// Probe the proxy listener's `/api/ready`: exit 0 when it answers 200, 1 otherwise.
    Health,
    /// Write the bootstrap file, the storage key and the admin secret, once, refusing anything that exists.
    Init(init::InitArgs),
    /// Discard the registry's secrets after a lost storage key and seal a new canary, or seal a missing canary under a
    /// key that opens the sealed rows, on a stopped registry.
    RecoverKey(RecoverKeyArgs),
    /// Probe the proxy listener, read the admin API and print one screen of readiness, pools, accounts and live pins:
    /// exit 0 when ready, 1 otherwise.
    Status(StatusArgs),
    /// Ask the running router to re-read every listener's certificate pair.
    #[command(subcommand)]
    Tls(TlsCommand),
}

#[derive(Subcommand)]
enum TlsCommand {
    /// Re-read every listener's certificate pair over the admin API: exit 0 once every pair reloaded, 1 otherwise. A
    /// pair that does not load leaves the listener serving the pair it had.
    Reload(TlsReloadArgs),
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Backup(args)) => run_backup(&cli.config, &args),
        Some(Command::CheckConfig) => {
            let config = phoebis_router::config::load_unresolved(&cli.config)?;
            // Read before the first line, so a pair the start would refuse is the one line the check prints.
            let proxy_tls = tls::load("proxy.tls", &config.proxy.tls)?;
            let admin_tls = tls::load("admin.tls", &config.admin.tls)?;
            println!("{} is valid", cli.config.display());
            println!(
                "{}",
                tls::describe("proxy", config.proxy.listen, &proxy_tls)
            );
            println!(
                "{}",
                tls::describe("admin", config.admin.listen, &admin_tls)
            );
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
            println!("profile {}", app::profile().name);
            println!(
                "routing: cooldown_fallback {}s, model_rejection {}s, catalog refresh {}s, startup fetch {}s",
                config.routing.cooldown_fallback_seconds,
                config.routing.model_rejection_seconds,
                config.routing.catalog_refresh_seconds,
                config.routing.startup_fetch_seconds
            );
            println!("ledger at {}", config.stats.ledger_path.display());
            // The start refuses a price file or a cost model it cannot use, so the check reads both the same way.
            let pricing = app::load_pricing(&config.stats)?;
            let now = jiff::Timestamp::now().as_millisecond();
            let revisions = pricing.revisions();
            let prices = revisions.in_force(now);
            println!(
                "api prices: {}",
                table_line(
                    &revisions.source,
                    revisions.len(),
                    &prices.effective,
                    revisions.pending(now).map(|next| next.effective.as_str()),
                    config.stats.api_prices_path.as_deref(),
                    format!("{} models", prices.models.len())
                )
            );
            let tables = app::load_plan_tables(&config.stats)?;
            let costs = tables.revisions.in_force(now);
            let weighted = costs.weighted_model_count();
            let sized = costs.sized_tier_count();
            let regimes = regime_clause(costs.regimes.keys().map(String::as_str));
            // `PlanTables::load` replaces the shipped revisions whole, so an instance with a file never sees a shipped
            // revision and nothing else says so.
            let shipped = phoebis_ledger::CostRevisions::shipped();
            let newest = &shipped.latest().effective;
            let newer = if tables.source == "file" && *newest > tables.revisions.latest().effective
            {
                format!(", the built-in model has a newer revision {newest}")
            } else {
                String::new()
            };
            println!(
                "subscription costs: {}",
                table_line(
                    &tables.source,
                    tables.revisions.len(),
                    &costs.effective,
                    tables
                        .revisions
                        .pending(now)
                        .map(|next| next.effective.as_str()),
                    config.stats.subscription_costs_path.as_deref(),
                    format!("{weighted} weighted models, {sized} account tiers{regimes}{newer}")
                )
            );
            let days = |n: u32| {
                if n == 0 {
                    "forever".to_string()
                } else {
                    format!("{n} days")
                }
            };
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
            let key = app::load_storage_key(&config.storage)?;
            let registry = &config.storage.path;
            // The mismatch names the registry itself and is the whole finding; any other failure (an unreadable
            // directory, a file that is not a database) says nothing of which file it met.
            let canary =
                phoebis_store::Store::read_canary(registry, &key).map_err(|error| match error {
                    phoebis_store::StoreError::KeyMismatch { .. } => anyhow::Error::new(error),
                    error => anyhow::Error::new(error)
                        .context(format!("reading the registry at {}", registry.display())),
                })?;
            match canary {
                phoebis_store::CanaryRead::NoRegistry => println!(
                    "registry at {} does not exist yet, and the first start creates it",
                    registry.display()
                ),
                phoebis_store::CanaryRead::SealedRowMatches => println!(
                    "registry at {} has no key canary yet. A sealed row opens under the key, and the next \
                     start seals one",
                    registry.display()
                ),
                phoebis_store::CanaryRead::Absent | phoebis_store::CanaryRead::Matches => {}
            }
            // The start refuses a file a newer build wrote. Both checks run here read-only and take no lock, so the
            // check finds that before anything starts, in the start's own words.
            phoebis_store::Store::check_readable(registry)
                .with_context(|| format!("reading the registry at {}", registry.display()))?;
            let ledger = &config.stats.ledger_path;
            phoebis_ledger::LedgerDb::check_readable(ledger)
                .with_context(|| format!("reading the ledger at {}", ledger.display()))?;
            let name = &config.admin.secret_env;
            match phoebis_router::config::from_env(name) {
                Ok(_) => {}
                // A warning rather than a refusal: the file is checked without the unit's environment.
                Err(phoebis_router::config::ConfigError::MissingSecret { .. }) => {
                    println!("warning: environment variable {name} is unset or empty");
                }
                // A set one the start refuses (too short, the example's placeholder) is the same finding here.
                Err(error) => return Err(error.into()),
            }
            let data_dir = phoebis_store::permissions::data_dir_of(&config.storage.path);
            for warning in phoebis_store::permissions::check_private_file(
                &config.storage.encryption_key_file,
                0o600,
            )
            .into_iter()
            .chain(tls::warnings(&proxy_tls))
            .chain(tls::warnings(&admin_tls))
            .chain(phoebis_store::permissions::check_private_dir(data_dir))
            .chain(phoebis_store::permissions::check_private_file(
                &config.storage.path,
                0o600,
            )) {
                println!("warning: {warning}");
            }
            Ok(())
        }
        Some(Command::Health) => {
            let config = phoebis_router::config::load_unresolved(&cli.config)?;
            match phoebis_router::health::probe(
                config.proxy.listen,
                config.proxy.tls.is_some(),
                phoebis_router::health::TIMEOUT,
            ) {
                Ok(addr) => {
                    println!("proxy answers on {addr}");
                    Ok(())
                }
                Err(reason) => {
                    println!("{reason}");
                    std::process::exit(1)
                }
            }
        }
        Some(Command::Init(args)) => {
            let interactive = std::io::stdin().is_terminal()
                && std::io::stderr().is_terminal()
                && !args.non_interactive;
            let mut plan = init::plan(&cli.config, &args)?;
            init::refuse(&plan)?;
            if interactive {
                plan = init::ask(
                    &args,
                    &plan,
                    &mut std::io::stdin().lock(),
                    &mut std::io::stderr().lock(),
                )?;
            }
            init::run(
                &plan,
                &mut std::io::stdout().lock(),
                &mut std::io::stderr().lock(),
            )?;
            Ok(())
        }
        Some(Command::RecoverKey(args)) => run_recover_key(&cli.config, &args),
        Some(Command::Status(args)) => run_status(&cli.config, &args),
        Some(Command::Tls(TlsCommand::Reload(args))) => run_tls_reload(&cli.config, &args),
        None => {
            let config = phoebis_router::config::load(&cli.config)?;
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            let deadline = phoebis_router::app::ShutdownDeadline::default();
            let served = runtime.block_on(phoebis_router::app::run_until(config, async {
                    let ctrl_c = tokio::signal::ctrl_c();
                    let mut terminate =
                        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                            Ok(signal) => signal,
                            Err(error) => {
                                tracing::error!(%error, "no SIGTERM handler, so a service manager's stop will not drain");
                                match ctrl_c.await {
                                    Ok(()) => return,
                                    Err(error) => {
                                        tracing::error!(
                                            %error,
                                            "no interrupt handler, so stop this process by other means"
                                        );
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
                                tracing::error!(%error, "no interrupt handler, so stop this process by other means");
                                std::future::pending::<()>().await;
                            }
                        }
                        _ = terminate.recv() => {}
                    }
                }, &deadline));
            // A writer the budget cut still runs on a blocking thread, which a plain drop of the runtime would wait
            // for without a bound.
            runtime.shutdown_timeout(deadline.remaining_at(std::time::Instant::now()));
            served
        }
    }
}

/// `phoebis backup`. It never returns: it exits with the run's answer as the other commands do.
fn run_backup(config_path: &Path, args: &BackupArgs) -> ! {
    let taken = backup::run(
        config_path,
        args,
        jiff::Timestamp::now(),
        backup::CONTENTION_DEADLINE,
        &mut std::io::stderr().lock(),
    );
    match taken {
        Ok((manifest, _)) => {
            if args.json {
                print(&pretty(&manifest));
            }
            std::process::exit(0)
        }
        Err(error) => {
            eprintln!("Error: {:?}", anyhow::Error::new(error));
            std::process::exit(1)
        }
    }
}

/// `phoebis recover-key`. It never returns: it exits with the run's answer as `backup` does.
fn run_recover_key(config_path: &Path, args: &RecoverKeyArgs) -> ! {
    let code = recover_key::run(
        config_path,
        args,
        &mut std::io::stdin().lock(),
        &mut std::io::stdout().lock(),
    );
    std::process::exit(code)
}

/// `phoebis status`. It never returns: the result is printed and the process exits while the runtime is still alive,
/// since dropping the runtime would wait for a name lookup the deadline abandoned.
fn run_status(config_path: &Path, args: &StatusArgs) -> ! {
    // The listeners line and the probe come from the file the client read, and without one only the admin URL is
    // known.
    let (client, file) = client(&args.common, config_path);
    let called = client.url();
    let listeners = match &file {
        Some(config) => Listeners {
            certificates: status::certificate_lines(&[
                ("proxy.tls", &config.proxy.tls),
                ("admin.tls", &config.admin.tls),
            ]),
            admin: match args.common.url {
                Some(_) => called,
                None => format!(
                    "{}://{}",
                    tls::scheme(&config.admin.tls),
                    config.admin.listen
                ),
            },
            metrics: Some(format!("http://{}", config.stats.metrics_listen)),
            proxy: Some(format!(
                "{}://{}",
                tls::scheme(&config.proxy.tls),
                config.proxy.listen
            )),
        },
        None => Listeners {
            admin: called,
            certificates: Vec::new(),
            metrics: None,
            proxy: None,
        },
    };
    let proxy = file
        .as_ref()
        .map(|config| (config.proxy.listen, config.proxy.tls.is_some()));
    // Bound to a local that outlives the exit below: a temporary runtime would drop at the end of its statement and
    // wait there for every blocking thread.
    let runtime = runtime();
    let reads = runtime.block_on(status::read(&client, proxy));
    let printed = if args.common.json {
        pretty(&status::render_json(&reads))
    } else {
        status::render(
            &reads,
            &listeners,
            env!("CARGO_PKG_VERSION"),
            phoebis_router::BUILD_COMMIT,
            jiff::Timestamp::now(),
            &jiff::tz::TimeZone::system(),
            args.all,
        )
    };
    print(&printed);
    std::process::exit(status::exit_code(&reads))
}

/// `phoebis tls reload`. It never returns, for the reason `run_status` gives.
fn run_tls_reload(config_path: &Path, args: &TlsReloadArgs) -> ! {
    let (client, _) = client(&args.common, config_path);
    let runtime = runtime();
    let body = runtime
        .block_on(tls_reload::read(&client))
        .unwrap_or_else(|error| {
            eprintln!("{error}");
            std::process::exit(1)
        });
    let printed = if args.common.json {
        pretty(&body)
    } else {
        tls_reload::render(&body)
    };
    print(&printed);
    std::process::exit(tls_reload::exit_code(&body))
}

/// The admin client and the configuration file it read, or the client's line and exit 1.
fn client(common: &Common, config_path: &Path) -> (AdminClient, Option<Config>) {
    AdminClient::from_flags_with_file(common, config_path).unwrap_or_else(|error| {
        eprintln!("{error}");
        std::process::exit(1)
    })
}

/// The subcommand's runtime, or the line and exit 1 when it cannot start.
fn runtime() -> tokio::runtime::Runtime {
    admin_client::runtime().unwrap_or_else(|error| {
        eprintln!("cannot start the runtime: {error}");
        std::process::exit(1)
    })
}

/// A `--json` answer as printed: pretty, with a closing newline.
fn pretty(value: &impl serde::Serialize) -> String {
    serde_json::to_string_pretty(value)
        .map(|json| format!("{json}\n"))
        .unwrap_or_default()
}

/// Writes `text` to stdout through one lock. A reader that went away (`| head`) undoes nothing the command did, so a
/// failed write is dropped and the command exits with its own answer.
fn print(text: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush());
}

/// One table's `check-config` line: the file read, or the built-in list with the configured path it found nothing
/// at, so an operator can tell a missing override from a misspelled one. `next` names the first entry dated after the
/// one in force, `None` when none is, so the operator knows what a restart changes.
fn table_line(
    source: &str,
    revisions: usize,
    in_force: &str,
    next: Option<&str>,
    path: Option<&Path>,
    counts: String,
) -> String {
    let plural = if revisions == 1 { "" } else { "s" };
    let next = next.map_or(String::new(), |date| format!(", next {date}"));
    let listed = format!("{revisions} revision{plural}, in force {in_force}{next}, {counts}");
    match (source, path) {
        ("file", Some(path)) => format!("{} (file, {listed})", path.display()),
        (_, Some(path)) => format!(
            "built in ({listed}), since {} was not found",
            path.display()
        ),
        (_, None) => format!("built in ({listed})"),
    }
}

/// The regimes of the cost entry in force for the `subscription costs:` line, named in the file's order, so an
/// operator can tell a file that prices some entrypoints apart from one that prices all of them at the base rows.
fn regime_clause<'a>(names: impl ExactSizeIterator<Item = &'a str>) -> String {
    let count = names.len();
    let names = names.collect::<Vec<_>>().join(", ");
    match count {
        0 => ", no regime".to_string(),
        1 => format!(", 1 regime: {names}"),
        n => format!(", {n} regimes: {names}"),
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::Cli;

    #[test]
    fn the_command_tree_passes_claps_assertions() {
        Cli::command().debug_assert();
    }
}
