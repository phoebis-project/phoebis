//! The listeners' TLS: one loader turns a listener's `tls` value into the acceptor its accept loop takes, with
//! the checks the start and `check-config` share, so both refuse a pair with the same line.

use std::cmp::Ordering;
use std::net::IpAddr;
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

/// A loaded listener: the acceptor, and the leaf's facts the start logs and `check-config` prints.
pub struct Loaded {
    pub acceptor: SslAcceptor,
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
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
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
    // The two variants below carry their cause in the line rather than as the error's source, so a chain printer
    // such as `main`'s prints the cause once, in the refusal line.
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
    let read = |file: &'static str, path: &Path| {
        std::fs::read(path).map_err(|error| TlsError::Unreadable {
            key,
            file,
            path: path.to_path_buf(),
            error,
        })
    };
    let cert_pem = read("cert_file", cert_file)?;
    let key_pem = read("key_file", key_file)?;
    // A block that does not parse leaves the file with no certificate the listener could serve, the same answer
    // as a file with no block at all.
    let mut chain = X509::stack_from_pem(&cert_pem)
        .unwrap_or_default()
        .into_iter();
    let leaf = chain.next().ok_or_else(|| TlsError::NoCertificate {
        key,
        path: cert_file.to_path_buf(),
    })?;
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
    Ok(Loaded {
        acceptor: builder.build(),
        leaf: LeafFacts {
            names,
            not_after: leaf.not_after().to_string(),
            days_left,
        },
    })
}

/// The listener's acceptor for its `tls` value: `Ok(None)` for plain HTTP.
pub fn load(key: &'static str, tls: &Option<ListenerTls>) -> Result<Option<Loaded>, TlsError> {
    match ListenerTls::effective(tls) {
        ListenerTls::PlainHttp => Ok(None),
        ListenerTls::Pair {
            cert_file,
            key_file,
        } => load_pair(key, cert_file, key_file).map(Some),
    }
}

/// The scheme word for a `listening` line and a URL: `https` when the value terminates TLS, else `http`.
pub fn scheme(tls: &Option<ListenerTls>) -> &'static str {
    if ListenerTls::effective(tls).terminates_tls() {
        "https"
    } else {
        "http"
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

    use fake_upstream::{generate_pair, generate_pair_valid_between, GeneratedPair};

    /// Writes a generated pair where an operator's pair would sit, the certificate and its key side by side.
    fn written(dir: &Path, name: &str, pair: &GeneratedPair) -> (PathBuf, PathBuf) {
        let cert = dir.join(format!("{name}.pem"));
        let key = dir.join(format!("{name}.key"));
        std::fs::write(&cert, &pair.cert_pem).unwrap();
        std::fs::write(&key, &pair.key_pem).unwrap();
        (cert, key)
    }

    #[test]
    fn a_pair_loads_with_its_leafs_names_and_the_days_it_has_left() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = written(dir.path(), "proxy", &generate_pair(&["localhost"]));
        let loaded = load_pair("proxy.tls", &cert, &key).unwrap();
        assert_eq!(loaded.leaf.names, ["localhost"]);
        assert!(loaded.leaf.days_left > 0, "{:?}", loaded.leaf);
    }

    #[test]
    fn the_leafs_ip_addresses_are_named_beside_its_dns_names() {
        let dir = tempfile::tempdir().unwrap();
        let pair = generate_pair(&["localhost", "127.0.0.1", "::1"]);
        let (cert, key) = written(dir.path(), "admin", &pair);
        let loaded = load_pair("admin.tls", &cert, &key).unwrap();
        assert_eq!(loaded.leaf.names, ["localhost", "127.0.0.1", "::1"]);
    }

    #[test]
    fn a_key_from_another_pair_is_refused_as_a_mismatch_naming_both_files() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, _) = written(dir.path(), "proxy", &generate_pair(&["localhost"]));
        let (_, other_key) = written(dir.path(), "other", &generate_pair(&["localhost"]));
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
        let (_, key) = written(dir.path(), "proxy", &generate_pair(&["localhost"]));
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
        let (cert, key) = written(dir.path(), "proxy", &generate_pair(&["localhost"]));
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
    fn a_key_file_without_a_pem_block_is_refused_as_holding_none() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = written(dir.path(), "admin", &generate_pair(&["localhost"]));
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
        let (cert, key) = written(dir.path(), "proxy", &pair);
        let error = load_pair("proxy.tls", &cert, &key).err().unwrap();
        let text = error.to_string();
        assert!(matches!(error, TlsError::Expired { .. }), "{error:?}");
        assert!(
            text.starts_with("proxy.tls.cert_file ") && text.contains("expired on"),
            "{text}"
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
        let (cert, key) = written(dir.path(), "proxy", &pair);
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
    fn plain_http_and_an_absent_key_load_no_acceptor_and_name_the_http_scheme() {
        assert!(load("proxy.tls", &None).unwrap().is_none());
        assert!(load("proxy.tls", &Some(ListenerTls::PlainHttp))
            .unwrap()
            .is_none());
        assert_eq!(scheme(&None), "http");
        assert_eq!(scheme(&Some(ListenerTls::PlainHttp)), "http");
        let dir = tempfile::tempdir().unwrap();
        let (cert_file, key_file) = written(dir.path(), "proxy", &generate_pair(&["localhost"]));
        let pair = Some(ListenerTls::Pair {
            cert_file,
            key_file,
        });
        assert!(load("proxy.tls", &pair).unwrap().is_some());
        assert_eq!(scheme(&pair), "https");
    }
}
