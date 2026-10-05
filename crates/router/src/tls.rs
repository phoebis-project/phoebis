//! The listeners' TLS: one loader turns a listener's `tls` pair into the acceptor its accept loop takes, with the
//! checks the start and `check-config` share, so both refuse a pair with the same line.

use std::cmp::Ordering;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use boring::asn1::Asn1Time;
use boring::error::ErrorStack;
use boring::pkey::PKey;
use boring::ssl::{select_next_proto, AlpnError, SslAcceptor, SslMethod};
use boring::x509::X509;

use crate::config::ListenerTls;

/// The acceptor an accept loop takes: `None` is plain HTTP.
pub type Acceptor = Option<Arc<SslAcceptor>>;

/// A leaf this close to `not_after` earns a warning in `check-config` and a line in `status`, and starts.
pub const EXPIRY_WARNING_DAYS: i32 = 30;

/// A loaded listener: the acceptor, and the leaf's facts the start logs and `check-config` prints.
pub struct Loaded {
    pub acceptor: SslAcceptor,
    /// The file the leaf was read from, which the expiry warning names.
    pub cert_file: PathBuf,
    /// The key's file, whose mode `warnings` checks.
    pub key_file: PathBuf,
    pub leaf: LeafFacts,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeafFacts {
    /// The subject alternative names, DNS names and IP addresses alike, as strings.
    pub names: Vec<String>,
    /// `not_after` as BoringSSL prints it (`Oct  5 12:00:00 2027 GMT`).
    pub not_after: String,
    /// Whole days until `not_after`; negative once it is past.
    pub days_left: i32,
}

/// One line per refusal, naming the listener key and the path, identical in `check-config` and at the start.
///
/// `Stack` and `Unreadable` carry their cause in the line rather than as the error's source, so a chain printer such
/// as `main`'s prints the cause once, in the refusal line.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("{key}.cert_file {path}: holds a PEM certificate block that does not parse")]
    CorruptCertificate { key: &'static str, path: PathBuf },
    #[error("{key}.cert_file {path}: expired on {date}")]
    Expired {
        key: &'static str,
        path: PathBuf,
        date: String,
    },
    #[error("{key}.key_file {path}: does not match the certificate in {cert}")]
    KeyMismatch {
        key: &'static str,
        path: PathBuf,
        cert: PathBuf,
    },
    #[error("{key}.cert_file {path}: holds no PEM certificate")]
    NoCertificate { key: &'static str, path: PathBuf },
    #[error("{key}.key_file {path}: holds no PEM private key")]
    NoKey { key: &'static str, path: PathBuf },
    #[error("{key}.cert_file {path}: not valid before {date}")]
    NotYetValid {
        key: &'static str,
        path: PathBuf,
        date: String,
    },
    #[error("{key}: {error}")]
    Stack {
        key: &'static str,
        error: boring::error::ErrorStack,
    },
    #[error("{key}.{file} {path}: {error}")]
    Unreadable {
        key: &'static str,
        file: &'static str,
        path: PathBuf,
        error: std::io::Error,
    },
}

/// Reads an operator's pair whole (`X509::stack_from_pem`, `PKey::private_key_from_pem`), sets the leaf, the chain
/// and the key on a `mozilla_intermediate_v5` acceptor with ALPN fixed to `http/1.1`, and checks the key and the
/// dates. `key` is `proxy.tls` or `admin.tls`, the prefix of every refusal.
pub fn load_pair(key: &'static str, cert_file: &Path, key_file: &Path) -> Result<Loaded, TlsError> {
    let cert_pem = read_file(key, "cert_file", cert_file)?;
    let key_pem = read_file(key, "key_file", key_file)?;
    let (leaf, chain) = certificates(key, cert_file, &cert_pem)?;
    let pkey = PKey::private_key_from_pem(&key_pem).map_err(|_| TlsError::NoKey {
        key,
        path: key_file.to_path_buf(),
    })?;

    let mut builder = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).map_err(stack(key))?;
    builder.set_certificate(&leaf).map_err(stack(key))?;
    for intermediate in chain {
        builder
            .add_extra_chain_cert(intermediate)
            .map_err(stack(key))?;
    }
    let mismatch = || TlsError::KeyMismatch {
        key,
        path: key_file.to_path_buf(),
        cert: cert_file.to_path_buf(),
    };
    // BoringSSL compares the key with the leaf's public half as the key is set and refuses there
    // (`KEY_VALUES_MISMATCH`); `check_private_key` only confirms both halves are in place.
    builder.set_private_key(&pkey).map_err(|_| mismatch())?;
    builder.check_private_key().map_err(|_| mismatch())?;
    // Both loops serve HTTP/1 only; a client that offers h2 is steered at the handshake rather than failing after
    // it. The server side selects, so this is the select callback and not `set_alpn_protos`.
    builder.set_alpn_select_callback(|_, offered| {
        select_next_proto(b"\x08http/1.1", offered).ok_or(AlpnError::NOACK)
    });

    let leaf = leaf_facts(key, cert_file, &leaf)?;
    Ok(Loaded {
        acceptor: builder.build(),
        cert_file: cert_file.to_path_buf(),
        key_file: key_file.to_path_buf(),
        leaf,
    })
}

/// The leaf's facts from `cert_file` alone, refused as `load_pair` refuses the certificate file. `status` reads the
/// leaf this way: it reports the leaf's dates, and the operator running it may not be able to read the key.
pub fn read_leaf(key: &'static str, cert_file: &Path) -> Result<LeafFacts, TlsError> {
    let cert_pem = read_file(key, "cert_file", cert_file)?;
    let (leaf, _) = certificates(key, cert_file, &cert_pem)?;
    leaf_facts(key, cert_file, &leaf)
}

fn read_file(key: &'static str, file: &'static str, path: &Path) -> Result<Vec<u8>, TlsError> {
    std::fs::read(path).map_err(|error| TlsError::Unreadable {
        key,
        file,
        path: path.to_path_buf(),
        error,
    })
}

/// The leaf and the intermediates behind it.
fn certificates(
    key: &'static str,
    cert_file: &Path,
    cert_pem: &[u8],
) -> Result<(X509, std::vec::IntoIter<X509>), TlsError> {
    // An empty stack is a file with no certificate block; an error is a block that does not parse, which may sit
    // behind a sound leaf, so the two get their own lines.
    let mut chain = X509::stack_from_pem(cert_pem)
        .map_err(|_| TlsError::CorruptCertificate {
            key,
            path: cert_file.to_path_buf(),
        })?
        .into_iter();
    let leaf = chain.next().ok_or_else(|| TlsError::NoCertificate {
        key,
        path: cert_file.to_path_buf(),
    })?;
    Ok((leaf, chain))
}

/// The leaf's names and days left, refusing a leaf outside its validity window.
fn leaf_facts(key: &'static str, cert_file: &Path, leaf: &X509) -> Result<LeafFacts, TlsError> {
    let today = Asn1Time::days_from_now(0).map_err(stack(key))?;
    if leaf.not_after().compare(&today).map_err(stack(key))? == Ordering::Less {
        return Err(TlsError::Expired {
            key,
            path: cert_file.to_path_buf(),
            date: leaf.not_after().to_string(),
        });
    }
    if leaf.not_before().compare(&today).map_err(stack(key))? == Ordering::Greater {
        return Err(TlsError::NotYetValid {
            key,
            path: cert_file.to_path_buf(),
            date: leaf.not_before().to_string(),
        });
    }
    let days_left = today.diff(leaf.not_after()).map_err(stack(key))?.days;
    let names = leaf
        .subject_alt_names()
        .map(|names| {
            names
                .iter()
                .filter_map(|name| {
                    name.dnsname()
                        .map(str::to_owned)
                        .or_else(|| name.ipaddress().and_then(ip_text))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(LeafFacts {
        names,
        not_after: leaf.not_after().to_string(),
        days_left,
    })
}

/// What a listener serves, for the start and for `check-config`: plain HTTP, or the loaded pair.
pub enum Serving {
    Plain,
    Tls(Loaded),
}

/// One call per listener: `Plain` for an absent key, `load_pair` for the pair. `key` is `proxy.tls` or `admin.tls`.
pub fn load(key: &'static str, tls: &Option<ListenerTls>) -> Result<Serving, TlsError> {
    match tls {
        None => Ok(Serving::Plain),
        Some(ListenerTls {
            cert_file,
            key_file,
        }) => load_pair(key, cert_file, key_file).map(Serving::Tls),
    }
}

/// The scheme word for a `listening` line and a URL: `https` when the key names a pair, else `http`.
pub fn scheme(tls: &Option<ListenerTls>) -> &'static str {
    if tls.is_some() {
        "https"
    } else {
        "http"
    }
}

/// The `check-config` line for one listener: `{listener} listens on {listen} over plain HTTP`, or `over TLS
/// (certificate {cert_file}, expires {not_after})` for a loaded pair. `serving` is `load`'s answer for the listener.
pub fn describe(listener: &str, listen: SocketAddr, serving: &Serving) -> String {
    match serving {
        Serving::Tls(loaded) => format!(
            "{listener} listens on {listen} over TLS (certificate {}, expires {})",
            loaded.cert_file.display(),
            loaded.leaf.not_after
        ),
        Serving::Plain => format!("{listener} listens on {listen} over plain HTTP"),
    }
}

/// The `warning:` lines for a loaded pair, without the `warning: ` prefix: `expiry_line`'s for the leaf, and
/// `key_file_warnings`' for its key file.
pub fn warnings(key: &'static str, serving: &Serving) -> Vec<String> {
    let Serving::Tls(loaded) = serving else {
        return Vec::new();
    };
    let mut warnings = Vec::new();
    warnings.extend(expiry_line(key, &loaded.cert_file, &loaded.leaf));
    warnings.extend(key_file_warnings(&loaded.key_file));
    warnings
}

/// `{key}.cert_file {path} expires in {n} days ({date})` for a leaf within `EXPIRY_WARNING_DAYS` of `not_after`, the
/// last whole day included, so a leaf with 30 days and some hours left is named; `None` for any other leaf.
pub fn expiry_line(key: &str, cert_file: &Path, leaf: &LeafFacts) -> Option<String> {
    (0..=EXPIRY_WARNING_DAYS)
        .contains(&leaf.days_left)
        .then(|| {
            format!(
                "{key}.cert_file {} expires in {} days ({})",
                cert_file.display(),
                leaf.days_left,
                leaf.not_after
            )
        })
}

/// `permissions::check_private_file(key_file, 0o600)`'s lines, except for the shape the service unit reads a key
/// through, which `init --owner` writes and the installation page recommends: owned by root and readable by its group
/// alone (`0640 root:phoebis`). That shape passes when this process holds the file's group, as the service does, or
/// runs as root, which has no way to tell the unit's group and is the one user that can have set the shape.
fn key_file_warnings(key_file: &Path) -> Vec<String> {
    use std::os::unix::fs::MetadataExt;

    if let Ok(metadata) = std::fs::metadata(key_file) {
        // SAFETY: `geteuid` reads the process's effective uid and cannot fail.
        let euid = unsafe { libc::geteuid() };
        if read_through_group(
            metadata.mode(),
            metadata.uid(),
            metadata.gid(),
            euid,
            &process_groups(),
        ) {
            return Vec::new();
        }
    }
    phoebis_store::permissions::check_private_file(key_file, 0o600)
}

/// Whether a key file of `mode`, `uid` and `gid` is the unit's group-read shape for a process of `euid` holding
/// `groups`: root's, nothing for other and nothing but read for the group, and a group the process holds unless the
/// process is root.
fn read_through_group(mode: u32, uid: u32, gid: u32, euid: u32, groups: &[u32]) -> bool {
    uid == 0 && mode & 0o037 == 0 && (euid == 0 || groups.contains(&gid))
}

/// The process's effective group and its supplementary groups.
fn process_groups() -> Vec<u32> {
    // SAFETY: `getegid` cannot fail; `getgroups` with a zero size only counts, and the second call writes at most
    // the `count` entries the vector holds.
    unsafe {
        let mut groups = vec![libc::getegid()];
        let count = libc::getgroups(0, std::ptr::null_mut());
        if let Ok(size) = usize::try_from(count) {
            let mut supplementary = vec![0; size];
            let filled = libc::getgroups(count, supplementary.as_mut_ptr());
            if let Ok(filled) = usize::try_from(filled) {
                supplementary.truncate(filled);
                groups.extend(supplementary);
            }
        }
        groups
    }
}

fn stack(key: &'static str) -> impl Fn(ErrorStack) -> TlsError {
    move |error| TlsError::Stack { key, error }
}

/// A SAN's address octets as text: four for IPv4, sixteen for IPv6, anything else is no address at all.
fn ip_text(octets: &[u8]) -> Option<String> {
    let ip = match octets.len() {
        4 => IpAddr::from(<[u8; 4]>::try_from(octets).ok()?),
        16 => IpAddr::from(<[u8; 16]>::try_from(octets).ok()?),
        _ => return None,
    };
    Some(ip.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    use fake_upstream::{generate_pair, generate_pair_valid_between};

    #[test]
    fn a_pair_loads_with_its_leafs_names_and_the_days_it_has_left() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = generate_pair(&["localhost"]).write_to(dir.path(), "proxy");
        let loaded = load_pair("proxy.tls", &cert, &key).unwrap();
        assert_eq!(loaded.leaf.names, ["localhost"]);
        assert!(loaded.leaf.days_left > 0, "{:?}", loaded.leaf);
    }

    #[test]
    fn the_leafs_ip_addresses_are_named_beside_its_dns_names() {
        let dir = tempfile::tempdir().unwrap();
        let pair = generate_pair(&["localhost", "127.0.0.1", "::1"]);
        let (cert, key) = pair.write_to(dir.path(), "admin");
        let loaded = load_pair("admin.tls", &cert, &key).unwrap();
        assert_eq!(loaded.leaf.names, ["localhost", "127.0.0.1", "::1"]);
    }

    #[test]
    fn a_key_from_another_pair_is_refused_as_a_mismatch_naming_both_files() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, _) = generate_pair(&["localhost"]).write_to(dir.path(), "proxy");
        let (_, other_key) = generate_pair(&["localhost"]).write_to(dir.path(), "other");
        let error = load_pair("proxy.tls", &cert, &other_key).err().unwrap();
        assert!(
            matches!(&error, TlsError::KeyMismatch { key: "proxy.tls", path, cert: named }
                if *path == other_key && *named == cert),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            format!(
                "proxy.tls.key_file {}: does not match the certificate in {}",
                other_key.display(),
                cert.display()
            )
        );
    }

    #[test]
    fn a_missing_certificate_file_is_refused_as_unreadable_naming_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let (_, key) = generate_pair(&["localhost"]).write_to(dir.path(), "proxy");
        let missing = dir.path().join("absent.pem");
        let error = load_pair("proxy.tls", &missing, &key).err().unwrap();
        assert!(
            matches!(&error, TlsError::Unreadable { key: "proxy.tls", file: "cert_file", path, .. }
                if *path == missing),
            "{error:?}"
        );
        assert!(
            error
                .to_string()
                .starts_with(&format!("proxy.tls.cert_file {}: ", missing.display())),
            "{error}"
        );
        assert!(
            std::error::Error::source(&error).is_none(),
            "the OS error sits in the one refusal line, so no chain repeats it under the line"
        );
    }

    #[test]
    fn a_certificate_file_without_a_pem_block_is_refused_as_holding_none() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = generate_pair(&["localhost"]).write_to(dir.path(), "proxy");
        std::fs::write(&cert, "not a pem").unwrap();
        let error = load_pair("proxy.tls", &cert, &key).err().unwrap();
        assert_eq!(
            error.to_string(),
            format!(
                "proxy.tls.cert_file {}: holds no PEM certificate",
                cert.display()
            )
        );
    }

    #[test]
    fn a_sound_leaf_followed_by_a_block_that_does_not_parse_is_refused_naming_the_block() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = generate_pair(&["localhost"]).write_to(dir.path(), "proxy");
        let mut pem = std::fs::read_to_string(&cert).unwrap();
        pem.push_str("-----BEGIN CERTIFICATE-----\nZm9vYmFy\n-----END CERTIFICATE-----\n");
        std::fs::write(&cert, pem).unwrap();
        let error = load_pair("proxy.tls", &cert, &key).err().unwrap();
        assert_eq!(
            error.to_string(),
            format!(
                "proxy.tls.cert_file {}: holds a PEM certificate block that does not parse",
                cert.display()
            )
        );
    }

    #[test]
    fn a_key_file_without_a_pem_block_is_refused_as_holding_none() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = generate_pair(&["localhost"]).write_to(dir.path(), "admin");
        std::fs::write(&key, "not a pem").unwrap();
        let error = load_pair("admin.tls", &cert, &key).err().unwrap();
        assert_eq!(
            error.to_string(),
            format!(
                "admin.tls.key_file {}: holds no PEM private key",
                key.display()
            )
        );
    }

    #[test]
    fn a_leaf_past_its_not_after_is_refused_as_expired_with_the_date() {
        let dir = tempfile::tempdir().unwrap();
        let pair = generate_pair_valid_between(&["localhost"], (2020, 1, 1), (2021, 1, 1));
        let (cert, key) = pair.write_to(dir.path(), "proxy");
        let error = load_pair("proxy.tls", &cert, &key).err().unwrap();
        let text = error.to_string();
        assert!(matches!(error, TlsError::Expired { .. }), "{error:?}");
        assert!(
            text.starts_with("proxy.tls.cert_file ") && text.contains("expired on"),
            "{text}"
        );
    }

    #[test]
    fn a_leaf_with_thirty_whole_days_left_and_part_of_a_day_warns() {
        let dir = tempfile::tempdir().unwrap();
        // Midnight UTC 31 days on leaves 30 whole days and what remains of today, the warning's last day.
        let date = jiff::Timestamp::now()
            .to_zoned(jiff::tz::TimeZone::UTC)
            .date()
            .checked_add(jiff::Span::new().days(31))
            .unwrap();
        let pair = generate_pair_valid_between(
            &["localhost"],
            (2020, 1, 1),
            (i32::from(date.year()), date.month() as u8, date.day() as u8),
        );
        let (cert, key) = pair.write_to(dir.path(), "proxy");
        let loaded = load_pair("proxy.tls", &cert, &key).unwrap();
        assert_eq!(loaded.leaf.days_left, 30, "{:?}", loaded.leaf);
        let warned = warnings("proxy.tls", &Serving::Tls(loaded));
        let line = format!(
            "proxy.tls.cert_file {} expires in 30 days (",
            cert.display()
        );
        assert!(
            warned.iter().any(|warning| warning.starts_with(&line)),
            "{warned:?}"
        );
    }

    #[test]
    fn a_key_owned_by_root_and_read_through_the_services_group_passes_and_every_other_shape_warns()
    {
        let (service, staff) = (990, 50);
        for (mode, euid, groups) in [
            (0o100640, service, [service]),
            (0o100440, service, [service]),
            (0o100640, 0, [0]),
        ] {
            assert!(
                read_through_group(mode, 0, service, euid, &groups),
                "{mode:o} for uid {euid}"
            );
        }
        for (mode, uid, gid) in [
            (0o100644, 0, service),
            (0o100660, 0, service),
            (0o100640, 1000, service),
            (0o100640, 0, staff),
        ] {
            assert!(
                !read_through_group(mode, uid, gid, service, &[service]),
                "{mode:o} owned by {uid}:{gid}"
            );
        }

        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = generate_pair(&["localhost"]).write_to(dir.path(), "admin");
        std::fs::set_permissions(&key, std::os::unix::fs::PermissionsExt::from_mode(0o640))
            .unwrap();
        let warned = warnings(
            "admin.tls",
            &Serving::Tls(load_pair("admin.tls", &cert, &key).unwrap()),
        );
        assert!(
            warned
                .iter()
                .any(|warning| warning.contains("readable by group or other (mode 0640)")),
            "a group-readable key this user owns is not the unit's shape: {warned:?}"
        );
    }

    #[test]
    fn a_leaf_valid_from_tomorrow_is_refused_as_not_yet_valid() {
        let dir = tempfile::tempdir().unwrap();
        let tomorrow = jiff::Timestamp::now()
            .to_zoned(jiff::tz::TimeZone::UTC)
            .date()
            .tomorrow()
            .unwrap();
        let pair = generate_pair_valid_between(
            &["localhost"],
            (
                i32::from(tomorrow.year()),
                tomorrow.month() as u8,
                tomorrow.day() as u8,
            ),
            (2100, 1, 1),
        );
        let (cert, key) = pair.write_to(dir.path(), "proxy");
        let error = load_pair("proxy.tls", &cert, &key).err().unwrap();
        assert!(matches!(error, TlsError::NotYetValid { .. }), "{error:?}");
        assert!(
            error.to_string().starts_with(&format!(
                "proxy.tls.cert_file {}: not valid before ",
                cert.display()
            )),
            "{error}"
        );
    }

    #[test]
    fn an_absent_key_loads_no_acceptor_and_a_pair_names_the_https_scheme() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(load("proxy.tls", &None).unwrap(), Serving::Plain));
        assert_eq!(scheme(&None), "http");
        let (cert_file, key_file) = generate_pair(&["localhost"]).write_to(dir.path(), "proxy");
        let pair = Some(ListenerTls {
            cert_file,
            key_file,
        });
        assert!(matches!(load("proxy.tls", &pair).unwrap(), Serving::Tls(_)));
        assert_eq!(scheme(&pair), "https");
    }
}
