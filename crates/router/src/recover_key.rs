//! `phoebis recover-key`: the recovery after a lost storage key, on a stopped registry. It takes the registry's lock,
//! reads the key file the configuration names or generates a key in memory, refuses a key that still opens the
//! registry, prints what it discards and asks, then writes a generated key's file as `init` does, prepares the
//! schema, and discards every row sealed under the old key and seals a new canary in one transaction. It prints
//! counts and paths, never a key.

use std::io::{BufRead, ErrorKind, Write};
use std::path::{Path, PathBuf};

use phoebis_store::{
    permissions, KeyMatch, RecoveryStore, SealedCounts, StateCause, StorageKey, Store, StoreError,
};

use crate::{config, init};

#[derive(clap::Args, Debug)]
pub struct RecoverKeyArgs {
    /// The user that receives the key file, its directories and the registry's lock file when this run creates
    /// them, which needs root.
    #[arg(long, value_name = "USER")]
    pub owner: Option<String>,
    /// Skip the confirmation, for a script.
    #[arg(long)]
    pub yes: bool,
}

/// The steps the recovery leaves to the operator, in the order the console takes them.
const FOLLOW_UPS: [&str; 3] = [
    "re-enter the API-key secrets in the console",
    "pair the OAuth accounts again in the console",
    "rotate every downstream key",
];

/// Exit 0 once the accounts are marked, 1 on a refusal or a failure, 2 when the operator answered anything but
/// `yes`. The lines it prints: the lock refusal "the registry is locked, stop the service first", the matching
/// key "the storage key matches the registry, nothing to recover", the corrupt canary over rows the key still opens,
/// the discard summary (one line per count), the prompt, the generated key file's path, the preparation's failure,
/// the discard's failure, and the three follow-up steps. A run that exits 1 or 2 removes the key file it wrote, with
/// the directories it created for it, in one line naming them, and keeps the registry's lock file.
pub fn run(
    config_path: &Path,
    args: &RecoverKeyArgs,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> i32 {
    let code = match recover(config_path, args, input, out) {
        Ok(code) => code,
        Err(line) => {
            let _ = writeln!(out, "{line}");
            1
        }
    };
    let _ = out.flush();
    code
}

/// The run, answering its exit code, or the lines a refusal or a failure before the transaction prints.
fn recover(
    config_path: &Path,
    args: &RecoverKeyArgs,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<i32, String> {
    let config = config::load_unresolved(config_path).map_err(|error| error.to_string())?;
    let registry = &config.storage.path;
    let present = registry
        .try_exists()
        .map_err(|error| format!("reading {}: {error}", registry.display()))?;
    if !present {
        return Err(format!(
            "no registry at {}, nothing to recover",
            registry.display()
        ));
    }
    // Ahead of the lock and the key file: a run that could not hand a generated key to the service user would leave
    // a key the service cannot read.
    let owner = args
        .owner
        .as_deref()
        .map(init::owner_ids)
        .transpose()
        .map_err(|error| error.to_string())?;
    // Handed over before the open answers, so an open the registry refuses still leaves the service a lock file it can
    // open.
    if let Some(owner) = owner {
        lock_file_for(registry, owner)?;
    }
    // The lock comes before the key file, so a refusal on a running router leaves no generated key behind.
    let mut store = match Store::open_for_recovery(registry) {
        Ok(store) => store,
        Err(StoreError::Locked) => {
            return Err("the registry is locked, stop the service first".to_string())
        }
        Err(error) => {
            return Err(format!(
                "opening the registry at {}: {error}",
                registry.display()
            ))
        }
    };
    let key_file = &config.storage.encryption_key_file;
    let mut written = None;
    let outcome = key_of(key_file, owner).and_then(|(key, source)| {
        discard(
            &mut store,
            registry,
            &key,
            source,
            key_file,
            args,
            input,
            out,
            &mut written,
        )
    });
    if matches!(outcome, Ok(0)) {
        return outcome;
    }
    let Some(written) = written.as_ref() else {
        return outcome;
    };
    // Removed while the lock is held, so no other run reads a key this one leaves unused. A start after a declined or
    // failed run then reports the missing key file it found rather than a key the registry refuses. The lock file
    // stays: a start that opened it before the exit locks that file, and an unlinked one would let a later start
    // create and lock a second file at the path, so both would pass the lock.
    let removed = remove_created(&written.key_file, &written.dirs);
    match outcome {
        Ok(code) => {
            let _ = say(out, &removed);
            Ok(code)
        }
        Err(line) => Err(format!("{line}\n{removed}")),
    }
}

/// Where the run's key came from. A generated key lives in memory alone until the operator confirms.
enum KeySource {
    Read,
    Generated { owner: Option<(u32, u32)> },
}

/// The key file a run wrote and the directories it created for it, from the top down.
struct Written {
    key_file: PathBuf,
    dirs: Vec<PathBuf>,
}

/// The key in the file `key_file` names where one is there, else a new key whose file `discard` writes.
fn key_of(key_file: &Path, owner: Option<(u32, u32)>) -> Result<(StorageKey, KeySource), String> {
    let reading = |error: &dyn std::fmt::Display| {
        format!(
            "reading storage.encryption_key_file {}: {error}",
            key_file.display()
        )
    };
    match std::fs::symlink_metadata(key_file) {
        Ok(_) => Ok((
            StorageKey::from_file(key_file).map_err(|error| reading(&error))?,
            KeySource::Read,
        )),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            Ok((StorageKey::generate(), KeySource::Generated { owner }))
        }
        Err(error) => Err(reading(&error)),
    }
}

/// The run past the key: the refusal of a key that opens the registry, the summary, the prompt, the file of a
/// generated key, the preparation and the transaction. `written` names the key file once it is on disk, so the run
/// removes it again after an exit of 1 or 2.
#[allow(clippy::too_many_arguments)]
fn discard(
    store: &mut RecoveryStore,
    registry: &Path,
    key: &StorageKey,
    source: KeySource,
    key_file: &Path,
    args: &RecoverKeyArgs,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    written: &mut Option<Written>,
) -> Result<i32, String> {
    let registry_error =
        |error: StoreError| format!("reading the registry at {}: {error}", registry.display());
    match store.key_matches(key).map_err(registry_error)? {
        KeyMatch::Matches => {
            return Err("the storage key matches the registry, nothing to recover".to_string())
        }
        // The key is the right one and the discard would erase what it still opens.
        KeyMatch::CorruptCanary { readable_rows } => {
            let rows = if readable_rows == 1 { "row" } else { "rows" };
            return Err(format!(
                "the storage key opens {readable_rows} sealed {rows} but not the corrupt canary, so nothing was \
                 discarded: restore the canary from a backup"
            ));
        }
        KeyMatch::Differs => {}
    }
    let counts = store.sealed_counts().map_err(registry_error)?;
    say(
        out,
        &summary(
            "the recovery discards what the old key sealed:",
            "and marks the accounts:",
            &counts,
        ),
    )?;
    if !args.yes {
        write!(out, "type yes to discard them: ")
            .and_then(|()| out.flush())
            .map_err(|error| format!("writing the prompt: {error}"))?;
        let mut answer = String::new();
        input
            .read_line(&mut answer)
            .map_err(|error| format!("reading the answer: {error}"))?;
        if answer.trim_end_matches(['\n', '\r']) != "yes" {
            // An answer cut off by the end of the input leaves the cursor on the prompt's line.
            let opener = if answer.ends_with('\n') { "" } else { "\n" };
            say(out, &format!("{opener}the registry was not changed"))?;
            return Ok(2);
        }
    }
    // Written once the operator confirmed, so a run stopped at the prompt leaves no key file behind.
    if let KeySource::Generated { owner } = source {
        let dirs = write_key(key_file, key, owner)?;
        *written = Some(Written {
            key_file: key_file.to_path_buf(),
            dirs,
        });
        say(
            out,
            &format!(
                "wrote a new storage key to {} (mode 0600)",
                key_file.display()
            ),
        )?;
    }
    store.prepare_recovery().map_err(|error| {
        format!(
            "the registry's schema could not be prepared for the recovery, so nothing was discarded: {error}"
        )
    })?;
    let landed = store
        .discard_sealed(key, StateCause::Operator)
        .map_err(|error| {
            format!(
                "the recovery did not land, so the registry at {} is unchanged: {error}",
                registry.display()
            )
        })?;
    // The transaction has landed, and a reader that went away does not undo it.
    let _ = say(
        out,
        &summary(
            "discarded what the old key sealed:",
            "and marked the accounts:",
            &landed,
        ),
    );
    let _ = say(
        out,
        &format!(
            "sealed a new canary under the key at {}",
            key_file.display()
        ),
    );
    let _ = say(out, "start the service, then:");
    for step in FOLLOW_UPS {
        let _ = say(out, &format!("  {step}"));
    }
    Ok(0)
}

/// Writes `key` as `init` writes a new one: 64 hex characters and a newline, in a file created at `0600` in one open,
/// under the directories missing on the way to it created at `0700`, all handed to `owner`. Answers the directories
/// it created, from the top down, the run's own to remove again. A step that fails removes what it created, so a
/// rerun starts from the same state.
fn write_key(
    path: &Path,
    key: &StorageKey,
    owner: Option<(u32, u32)>,
) -> Result<Vec<PathBuf>, String> {
    let mut dirs: Vec<PathBuf> = permissions::data_dir_of(path)
        .ancestors()
        .take_while(|dir| {
            !dir.as_os_str().is_empty()
                && matches!(
                    std::fs::symlink_metadata(dir),
                    Err(error) if error.kind() == ErrorKind::NotFound
                )
        })
        .map(Path::to_path_buf)
        .collect();
    dirs.reverse();
    let mut file = match permissions::create_private_file(path, 0o600) {
        Ok(file) => file,
        Err(error) => {
            let _ = remove_dirs(&dirs);
            return Err(format!("creating {}: {error}", path.display()));
        }
    };
    let written = file
        .write_all(key.to_hex().as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("writing {}: {error}", path.display()))
        .and_then(|()| match owner {
            // Through the descriptor `create_new` opened and then `lchown`, as `init` hands its own over, so a link
            // swapped in as a path's last component cannot carry the change of owner to another file.
            Some((uid, gid)) => std::os::unix::fs::fchown(&file, Some(uid), Some(gid))
                .map_err(|error| format!("handing {} to the owner: {error}", path.display()))
                .and_then(|()| dirs.iter().try_for_each(|dir| hand_over(dir, (uid, gid)))),
            None => Ok(()),
        });
    if let Err(error) = written {
        let _ = std::fs::remove_file(path);
        let _ = remove_dirs(&dirs);
        return Err(error);
    }
    Ok(dirs)
}

fn hand_over(path: &Path, (uid, gid): (u32, u32)) -> Result<(), String> {
    std::os::unix::fs::lchown(path, Some(uid), Some(gid))
        .map_err(|error| format!("handing {} to the owner: {error}", path.display()))
}

/// Creates the lock file the store's open takes beside `registry`, at `0600` in one open, and hands it to `owner` at
/// once, where none is there yet.
fn lock_file_for(registry: &Path, owner: (u32, u32)) -> Result<(), String> {
    let lock = permissions::data_dir_of(registry).join("lock");
    match permissions::create_private_file(&lock, 0o600) {
        Ok(_) => hand_over(&lock, owner),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(format!("creating {}: {error}", lock.display())),
    }
}

/// Removes the key file `write_key` wrote with the directories it created, and answers the one line naming what went,
/// followed by a line for each that stayed. The directories of a key file that stays are not empty, so they are left
/// untried.
fn remove_created(path: &Path, dirs: &[PathBuf]) -> String {
    let mut removed = Vec::new();
    let mut stayed = Vec::new();
    match std::fs::remove_file(path) {
        Ok(()) => {
            removed.push(format!("the key file {}", path.display()));
            if let Err((dir, error)) = remove_dirs(dirs) {
                stayed.push(format!(
                    "the directory this run created at {} was not removed: {error}",
                    dir.display()
                ));
            }
        }
        Err(error) => stayed.push(format!(
            "the storage key this run created at {} was not removed: {error}",
            path.display()
        )),
    }
    let mut lines = Vec::new();
    if !removed.is_empty() {
        lines.push(format!(
            "removed {} this run created",
            removed.join(" and ")
        ));
    }
    lines.extend(stayed);
    lines.join("\n")
}

/// The deepest first, so each one is empty by the time it goes.
fn remove_dirs(dirs: &[PathBuf]) -> Result<(), (&Path, std::io::Error)> {
    dirs.iter()
        .rev()
        .try_for_each(|dir| std::fs::remove_dir(dir).map_err(|error| (dir.as_path(), error)))
}

/// The counts in the order the transaction runs: what goes, then how the accounts are left.
fn summary(discards: &str, marks: &str, counts: &SealedCounts) -> String {
    format!(
        "{discards}\n  OAuth credential bundles: {}\n  API-key secrets: {}\n  keepalive recipe bodies: {}\n\
         {marks}\n  oauth accounts disconnected: {}\n  api_key accounts unpaired: {}\n  \
         open refresh attempts failed: {}",
        counts.credentials,
        counts.api_key_secrets,
        counts.recipe_bodies,
        counts.oauth_accounts,
        counts.api_key_accounts,
        counts.open_refresh_attempts,
    )
}

fn say(out: &mut dyn Write, text: &str) -> Result<(), String> {
    writeln!(out, "{text}").map_err(|error| format!("writing the summary: {error}"))
}

#[cfg(test)]
mod tests {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// The key file and its directory, created at `0700` under `parent`.
    fn created_under(parent: &Path) -> (PathBuf, PathBuf) {
        let keys = parent.join("keys");
        std::fs::create_dir(&keys).unwrap();
        let key_file = keys.join("storage.key");
        std::fs::write(&key_file, "").unwrap();
        (keys, key_file)
    }

    // Off root the one owner a run may hand a file to is itself, so the case reads the creation and the mode, while the
    // root case in the binary's tests reads the hand-over to another user.
    #[test]
    fn the_lock_file_a_run_creates_for_its_owner_is_private_and_one_already_there_stays_as_it_is() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let registry = dir.path().join("router.sqlite");
        let lock = dir.path().join("lock");
        let own = unsafe { (libc::geteuid(), libc::getegid()) };
        lock_file_for(&registry, own).unwrap();
        let metadata = std::fs::symlink_metadata(&lock).unwrap();
        assert_eq!(
            (metadata.mode() & 0o777, metadata.uid(), metadata.gid()),
            (0o600, own.0, own.1)
        );
        std::fs::write(&lock, "held").unwrap();
        lock_file_for(&registry, own).unwrap();
        assert_eq!(
            std::fs::read_to_string(&lock).unwrap(),
            "held",
            "a lock file already there stays as it is"
        );
    }

    // A read-only directory refuses an unlink inside it off root, which drives the lines a run reaches only when a
    // removal fails.
    #[test]
    fn a_removal_that_fails_names_the_key_file_or_the_directory_that_stayed_with_its_error() {
        let denied = std::io::Error::from_raw_os_error(libc::EACCES);
        let dir = tempfile::tempdir().unwrap();

        let (keys, key_file) = created_under(dir.path());
        std::fs::set_permissions(&keys, Permissions::from_mode(0o500)).unwrap();
        let key_stayed = remove_created(&key_file, std::slice::from_ref(&keys));
        // Restored before the assertions so the temporary directory can be cleaned up on either outcome.
        std::fs::set_permissions(&keys, Permissions::from_mode(0o700)).unwrap();

        let parent = dir.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        let (inner, inner_key) = created_under(&parent);
        std::fs::set_permissions(&parent, Permissions::from_mode(0o500)).unwrap();
        let dir_stayed = remove_created(&inner_key, std::slice::from_ref(&inner));
        std::fs::set_permissions(&parent, Permissions::from_mode(0o700)).unwrap();

        assert_eq!(
            key_stayed,
            format!(
                "the storage key this run created at {} was not removed: {denied}",
                key_file.display()
            )
        );
        assert!(key_file.exists() && keys.exists());
        assert_eq!(
            dir_stayed,
            format!(
                "removed the key file {} this run created\nthe directory this run created at {} was not \
                 removed: {denied}",
                inner_key.display(),
                inner.display()
            )
        );
        assert!(!inner_key.exists() && inner.exists());
    }
}
