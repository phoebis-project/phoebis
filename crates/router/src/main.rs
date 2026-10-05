//! The `phoebis` command line: run the router, write a first installation's files, check a configuration without
//! starting anything, probe a running proxy listener, read a running router's readiness, pools, accounts and pins, or
//! copy both databases into a backup.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use phoebis_router::admin_client::{self, AdminClient};
use phoebis_router::backup::{self, BackupArgs};
use phoebis_router::status::{self, Listeners, StatusArgs};
use phoebis_router::{init, tls};

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
    /// Probe the proxy listener, read the admin API and print one screen of readiness, pools, accounts and live pins:
    /// exit 0 when ready, 1 otherwise.
    Status(StatusArgs),
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Backup(args)) => run_backup(&cli.config, &args),
        Some(Command::CheckConfig) => {
            let mut config = phoebis_router::config::load_unresolved(&cli.config)?;
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
            // The start refuses a price file an estimate cannot use, so the check reads it the same way.
            phoebis_ledger::Pricing::load(config.stats.pricing_path.as_deref()).with_context(
                || {
                    format!(
                        "loading the price table from {}",
                        config
                            .stats
                            .pricing_path
                            .as_deref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "the built-in table".to_string())
                    )
                },
            )?;
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
            let key_file = &config.storage.encryption_key_file;
            let key = phoebis_store::StorageKey::from_file(key_file).with_context(|| {
                format!("reading storage.encryption_key_file {}", key_file.display())
            })?;
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
            for warning in phoebis_store::permissions::check_private_file(
                &config.storage.encryption_key_file,
                0o600,
            ) {
                println!("warning: {warning}");
            }
            for warning in tls::warnings("proxy.tls", &proxy_tls)
                .into_iter()
                .chain(tls::warnings("admin.tls", &admin_tls))
            {
                println!("warning: {warning}");
            }
            let data_dir = phoebis_store::permissions::data_dir_of(&config.storage.path);
            for warning in phoebis_store::permissions::check_private_dir(data_dir) {
                println!("warning: {warning}");
            }
            for warning in
                phoebis_store::permissions::check_private_file(&config.storage.path, 0o600)
            {
                println!("warning: {warning}");
            }
            Ok(())
        }
        Some(Command::Health) => {
            let config = phoebis_router::config::load(&cli.config)?;
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
        Some(Command::Status(args)) => run_status(&cli.config, &args),
        None => {
            let config = phoebis_router::config::load(&cli.config)?;
            tokio::runtime::Builder::new_multi_thread()
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
                                        tracing::error!(
                                            %error,
                                            "no interrupt handler; stop this process by other means"
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
                                tracing::error!(%error, "no interrupt handler; stop this process by other means");
                                std::future::pending::<()>().await;
                            }
                        }
                        _ = terminate.recv() => {}
                    }
                }))
        }
    }
}

/// `phoebis backup`. It never returns: it exits with the run's answer as the other subcommands do.
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
                let printed = serde_json::to_string_pretty(&manifest)
                    .map(|json| format!("{json}\n"))
                    .unwrap_or_default();
                let mut stdout = std::io::stdout().lock();
                // The manifest on disk is the backup's record; a reader that went away does not undo it.
                let _ = stdout
                    .write_all(printed.as_bytes())
                    .and_then(|()| stdout.flush());
            }
            std::process::exit(0)
        }
        Err(error) => {
            eprintln!("Error: {:?}", anyhow::Error::new(error));
            std::process::exit(1)
        }
    }
}

/// `phoebis status`. It never returns: the result is printed and the process exits while the runtime is still alive,
/// since dropping the runtime would wait for a name lookup the deadline abandoned.
fn run_status(config_path: &Path, args: &StatusArgs) -> ! {
    let client = AdminClient::from_flags(&args.common, config_path).unwrap_or_else(|error| {
        eprintln!("{error}");
        std::process::exit(error.exit_code())
    });
    // `from_flags` read the file unless `--url` and `--secret-env` stand in for it; the listeners line and the probe
    // come from the same file, and without one only the admin URL is known.
    let file = if args.common.url.is_some() && args.common.secret_env.is_some() {
        None
    } else {
        Some(
            phoebis_router::config::load_unresolved(config_path).unwrap_or_else(|error| {
                eprintln!("{error}");
                std::process::exit(1)
            }),
        )
    };
    let called = client.url();
    let listeners = match &file {
        Some(config) => Listeners {
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
            metrics: None,
            proxy: None,
        },
    };
    let proxy = file
        .as_ref()
        .map(|config| (config.proxy.listen, config.proxy.tls.is_some()));
    // Bound to a local that outlives the exit below: a temporary runtime would drop at the end of its statement and
    // wait there for every blocking thread.
    let runtime = admin_client::runtime().unwrap_or_else(|error| {
        eprintln!("cannot start the runtime: {error}");
        std::process::exit(1)
    });
    let reads = runtime.block_on(status::read(
        &client,
        proxy,
        Duration::from_secs(args.common.timeout),
    ));
    let printed = if args.common.json {
        serde_json::to_string_pretty(&status::render_json(&reads))
            .map(|json| format!("{json}\n"))
            .unwrap_or_default()
    } else {
        status::render(
            &reads,
            &listeners,
            env!("CARGO_PKG_VERSION"),
            jiff::Timestamp::now(),
            &jiff::tz::TimeZone::system(),
            args.all,
        )
    };
    let mut stdout = std::io::stdout().lock();
    // A reader that went away (`| head`) is not a failure of the read, so the exit code stays the readiness answer.
    let _ = stdout
        .write_all(printed.as_bytes())
        .and_then(|()| stdout.flush());
    std::process::exit(status::exit_code(&reads))
}
