//! `phoebis recover-key`: the recovery after a lost storage key, on a stopped registry. It takes the registry's lock,
//! reads the key file the configuration names or generates it as `init` does, refuses a key that still opens the
//! registry, prints what it discards and asks, then discards every row sealed under the old key and seals a new
//! canary in one transaction. It prints counts and paths, never a key.

use std::io::{BufRead, ErrorKind, Write};
use std::path::Path;

use phoebis_store::{permissions, SealedCounts, StateCause, StorageKey, Store, StoreError};

use crate::{config, init};

#[derive(clap::Args, Debug)]
pub struct RecoverKeyArgs {
    /// The user that owns a key file this run generates, which needs root.
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
/// key "the storage key matches the registry, nothing to recover", the generated key file's path, the discard
/// summary (one line per count), the prompt, and the three follow-up steps.
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

/// The run, answering its exit code, or the one line a refusal or a failure before the transaction prints.
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
    let reading = |error: &dyn std::fmt::Display| {
        format!(
            "reading storage.encryption_key_file {}: {error}",
            key_file.display()
        )
    };
    let key = match std::fs::symlink_metadata(key_file) {
        Ok(_) => StorageKey::from_file(key_file).map_err(|error| reading(&error))?,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let key = generate(key_file, owner)?;
            say(
                out,
                &format!(
                    "wrote a new storage key to {} (mode 0600)",
                    key_file.display()
                ),
            )?;
            key
        }
        Err(error) => return Err(reading(&error)),
    };
    let registry_error =
        |error: StoreError| format!("reading the registry at {}: {error}", registry.display());
    if store.key_matches(&key).map_err(registry_error)? {
        return Err("the storage key matches the registry, nothing to recover".to_string());
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
    let landed = store
        .discard_sealed(&key, StateCause::Operator)
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

/// Writes a new key as `init` does: 64 hex characters and a newline, in a file created at `0600` in one open and
/// handed to `owner` through that descriptor. A write that fails removes the file again, so a rerun generates anew.
fn generate(path: &Path, owner: Option<(u32, u32)>) -> Result<StorageKey, String> {
    let key = StorageKey::generate();
    let hex = key.to_hex();
    let mut file = permissions::create_private_file(path, 0o600)
        .map_err(|error| format!("creating {}: {error}", path.display()))?;
    let written = file
        .write_all(hex.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all())
        .and_then(|()| match owner {
            Some((uid, gid)) => std::os::unix::fs::fchown(&file, Some(uid), Some(gid)),
            None => Ok(()),
        });
    if let Err(error) = written {
        let _ = std::fs::remove_file(path);
        return Err(format!("writing {}: {error}", path.display()));
    }
    Ok(key)
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
