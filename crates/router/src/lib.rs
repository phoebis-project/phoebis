//! Wiring for the phoebis binary, exposed as a library so integration tests can build the app.

pub mod admin_client;
pub mod app;
pub mod backup;
pub mod config;
pub mod health;
pub mod init;
pub mod listen;
mod private_dirs;
pub mod recover_key;
pub mod status;
pub mod tls;
pub mod tls_reload;

/// The commit the binary was built from: `PHOEBIS_BUILD_COMMIT` at build time, else `git rev-parse HEAD` in the
/// build's tree, else `unknown`.
pub const BUILD_COMMIT: &str = env!("PHOEBIS_BUILD_COMMIT");

#[cfg(test)]
mod tests {
    use super::BUILD_COMMIT;

    #[test]
    fn the_build_commit_is_a_full_lowercase_hash_or_unknown() {
        let hash = BUILD_COMMIT.len() == 40
            && BUILD_COMMIT
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        assert!(hash || BUILD_COMMIT == "unknown", "{BUILD_COMMIT:?}");
    }
}
