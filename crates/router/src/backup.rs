//! `phoebis backup`: a consistent copy of the registry and the ledger in a private directory, with a manifest that
//! names the key file and carries nothing derived from it. It takes no lock and opens no socket, so it runs beside a
//! serving router as `check-config` does, and it loads the file without the admin secret.

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use phoebis_ledger::LedgerDb;
use phoebis_store::{permissions, Store};

use crate::config::{self, ConfigError};
use crate::private_dirs;

#[derive(clap::Args, Debug)]
pub struct BackupArgs {
    /// Print the manifest on stdout.
    #[arg(long)]
    pub json: bool,
    /// The destination directory. `./phoebis-backup-<YYYYMMDD>T<HHMMSS>Z` by default.
    #[arg(long, value_name = "DIR")]
    pub to: Option<PathBuf>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Manifest {
    pub format: u32,
    pub phoebis_version: String,
    /// RFC 3339, UTC.
    pub taken_at: String,
    pub config: PathBuf,
    /// As the file names it; nothing derived from the key.
    pub key_file: PathBuf,
    pub registry: Copied,
    pub ledger: Copied,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Copied {
    pub source: PathBuf,
    pub schema_version: Option<u32>,
    pub bytes: u64,
    pub sha256: String,
}

/// How long one copy waits out a lock on its source before the run fails and cleans up; the binary's value, passed
/// to `run` so a test proves the bound in milliseconds.
pub const CONTENTION_DEADLINE: Duration = Duration::from_secs(10);

const FORMAT: u32 = 1;
const LEDGER_COPY: &str = "ledger.sqlite";
const MANIFEST: &str = "manifest.json";
const REGISTRY_COPY: &str = "router.sqlite";

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("backup failed: {reason}. Nothing was written to {dir}")]
    Failed { dir: PathBuf, reason: String },
    #[error("a backup carries both databases, but {path} does not exist")]
    MissingSource { path: PathBuf },
    #[error("{path} is not an empty directory")]
    NotEmpty { path: PathBuf },
    #[error("{path}: group or other can write it, or another user owns it. Nothing was written.")]
    NotPrivate { path: PathBuf },
}

/// The run: every refusal before a byte is written, every failure after one cleaned up. `report` is stderr.
pub fn run(
    config_path: &Path,
    args: &BackupArgs,
    now: jiff::Timestamp,
    contention: Duration,
    report: &mut dyn Write,
) -> Result<(Manifest, PathBuf), RunError> {
    let config = config::load_unresolved(config_path)?;
    for source in [&config.storage.path, &config.stats.ledger_path] {
        // A copy of one database alone restores the two at different points in the request history; an answer
        // other than "absent" is left to the copy, which names its own error.
        if matches!(source.try_exists(), Ok(false)) {
            return Err(RunError::MissingSource {
                path: source.clone(),
            });
        }
    }
    let dir = args.to.clone().unwrap_or_else(|| default_dir(now));
    let existed = match std::fs::metadata(&dir) {
        Ok(metadata) => {
            let empty = metadata.is_dir()
                && std::fs::read_dir(&dir)
                    .map_err(|error| {
                        failed(&dir, format!("cannot read {}: {error}", dir.display()))
                    })?
                    .next()
                    .is_none();
            if !empty {
                return Err(RunError::NotEmpty { path: dir });
            }
            // SAFETY: `geteuid` reads the process's effective uid and cannot fail.
            let euid = unsafe { libc::geteuid() };
            if refused_destination(metadata.mode(), metadata.uid(), euid) {
                return Err(RunError::NotPrivate { path: dir });
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(failed(
                &dir,
                format!("cannot read {}: {error}", dir.display()),
            ))
        }
    };
    let key_file = &config.storage.encryption_key_file;
    // Checked for existence alone: the key is never read, so nothing derived from it can reach the backup.
    if matches!(key_file.try_exists(), Ok(false)) {
        let _ = writeln!(
            report,
            "warning: the key file {} does not exist, and the registry copy cannot be opened without the key that sealed \
             it",
            key_file.display()
        );
    }
    if existed {
        for warning in permissions::check_private_dir(&dir) {
            let _ = writeln!(report, "warning: {warning}");
        }
    }

    let mut created = Vec::new();
    let mut written = Vec::new();
    let taken = take(
        config_path,
        &config,
        &dir,
        now,
        contention,
        &mut created,
        &mut written,
    );
    match taken {
        Ok((manifest, lines)) => {
            for line in lines {
                let _ = writeln!(report, "{line}");
            }
            let _ = writeln!(
                report,
                "the key file {} is not in this backup. Keep it apart, since the registry copy is sealed under it",
                key_file.display()
            );
            Ok((manifest, dir))
        }
        Err(reason) => {
            // Cleanup is best effort: the failure's own reason is the line worth printing.
            for path in written.iter().rev() {
                let _ = std::fs::remove_file(path);
            }
            for path in created.iter().rev() {
                let _ = std::fs::remove_dir(path);
            }
            Err(RunError::Failed { dir, reason })
        }
    }
}

/// Everything after the refusals. Each directory this run creates lands on `created` and each file on `written` the
/// moment it exists, so the caller removes exactly what the run made. The report's lines are returned rather than
/// printed, so a run that fails never names a file it then removed.
fn take(
    config_path: &Path,
    config: &config::Config,
    dir: &Path,
    now: jiff::Timestamp,
    contention: Duration,
    created: &mut Vec<PathBuf>,
    written: &mut Vec<PathBuf>,
) -> Result<(Manifest, Vec<String>), String> {
    create_missing(dir, created)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    let mut lines = Vec::new();
    let registry_copy = dir.join(REGISTRY_COPY);
    let mut registry = copy(
        &config.storage.path,
        &registry_copy,
        contention,
        written,
        &mut lines,
    )?;
    let ledger_copy = dir.join(LEDGER_COPY);
    let mut ledger = copy(
        &config.stats.ledger_path,
        &ledger_copy,
        contention,
        written,
        &mut lines,
    )?;
    registry.schema_version = Store::read_schema_version(&registry_copy)
        .map_err(|error| format!("{}: {error}", registry_copy.display()))?;
    ledger.schema_version = LedgerDb::read_schema_version(&ledger_copy)
        .map_err(|error| format!("{}: {error}", ledger_copy.display()))?;
    sync_dir(dir)?;

    let manifest = Manifest {
        format: FORMAT,
        phoebis_version: env!("CARGO_PKG_VERSION").to_string(),
        taken_at: taken_at(now),
        config: std::path::absolute(config_path).unwrap_or_else(|_| config_path.to_path_buf()),
        key_file: config.storage.encryption_key_file.clone(),
        registry,
        ledger,
    };
    let mut json =
        serde_json::to_vec_pretty(&manifest).map_err(|error| format!("the manifest: {error}"))?;
    json.push(b'\n');
    let manifest_path = dir.join(MANIFEST);
    let mut file = permissions::create_private_file(&manifest_path, 0o600)
        .map_err(|error| format!("{}: {error}", manifest_path.display()))?;
    written.push(manifest_path.clone());
    file.write_all(&json)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("{}: {error}", manifest_path.display()))?;
    // The manifest's own directory entry is durable before the run answers that the backup is whole.
    sync_dir(dir)?;
    lines.push(format!(
        "wrote {} ({} bytes)",
        manifest_path.display(),
        json.len()
    ));
    lines.push(format!("manifest at {}", manifest_path.display()));
    Ok((manifest, lines))
}

/// One database to its copy: the snapshot, its `quick_check`, and its digest. The schema version is read once both
/// copies landed.
fn copy(
    source: &Path,
    destination: &Path,
    contention: Duration,
    written: &mut Vec<PathBuf>,
    lines: &mut Vec<String>,
) -> Result<Copied, String> {
    // A failed snapshot has removed its own destination, a copy `quick_check` refused among them.
    let bytes = phoebis_store::snapshot(source, destination, contention)
        .map_err(|error| error.to_string())?;
    written.push(destination.to_path_buf());
    let sha256 =
        sha256_of(destination).map_err(|error| format!("{}: {error}", destination.display()))?;
    lines.push(format!("wrote {} ({bytes} bytes)", destination.display()));
    lines.push(format!("quick_check on {}: ok", destination.display()));
    Ok(Copied {
        source: source.to_path_buf(),
        schema_version: None,
        bytes,
        sha256,
    })
}

/// Creates `dir` and its missing parents, then syncs the directory holding each one the run created, so the entries
/// outlive a power loss once the run answers.
fn create_missing(dir: &Path, created: &mut Vec<PathBuf>) -> std::io::Result<()> {
    private_dirs::create_missing(dir, created).map_err(|walk| walk.error)?;
    for path in created.iter() {
        File::open(permissions::data_dir_of(path))?.sync_all()?;
    }
    Ok(())
}

fn sha256_of(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = boring::sha::Sha256::new();
    let mut piece = vec![0; 64 * 1024];
    loop {
        let read = file.read(&mut piece)?;
        if read == 0 {
            break;
        }
        hasher.update(&piece[..read]);
    }
    Ok(hasher
        .finish()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn sync_dir(dir: &Path) -> Result<(), String> {
    File::open(dir)
        .and_then(|handle| handle.sync_all())
        .map_err(|error| format!("{}: {error}", dir.display()))
}

/// Whether an existing destination is refused: a group or other write bit, or an owner other than the run's uid,
/// lets someone besides the run's user replace or remove the copies, and the ledger copy holds captured prompts in
/// plain text. Read access alone is left to `check_private_dir`'s warning.
fn refused_destination(mode: u32, owner: u32, euid: u32) -> bool {
    mode & 0o022 != 0 || owner != euid
}

fn failed(dir: &Path, reason: String) -> RunError {
    RunError::Failed {
        dir: dir.to_path_buf(),
        reason,
    }
}

fn taken_at(now: jiff::Timestamp) -> String {
    now.strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn default_dir(now: jiff::Timestamp) -> PathBuf {
    Path::new(".").join(format!("phoebis-backup-{}", now.strftime("%Y%m%dT%H%M%SZ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_owned_by_another_uid_reads_as_refused() {
        let (run_as, other) = (1000, 1001);
        assert!(refused_destination(0o40700, other, run_as));
        assert!(refused_destination(0o40770, run_as, run_as));
        assert!(refused_destination(0o40707, run_as, run_as));
        assert!(!refused_destination(0o40700, run_as, run_as));
        assert!(
            !refused_destination(0o40755, run_as, run_as),
            "read access alone is a warning"
        );
    }

    #[test]
    fn the_default_directory_and_the_stamp_read_the_instant_in_utc() {
        let now: jiff::Timestamp = "2026-10-05T23:04:05.678+02:00".parse().unwrap();
        assert_eq!(
            default_dir(now),
            Path::new("./phoebis-backup-20261005T210405Z")
        );
        assert_eq!(taken_at(now), "2026-10-05T21:04:05Z");
    }
}
