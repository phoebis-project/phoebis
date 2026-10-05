//! The listeners' TLS: one loader turns a listener's `tls` value into the acceptor its accept loop takes, with
//! the checks the start and `check-config` share, so both refuse a pair with the same line. Under `self_signed` the
//! start writes a local CA and a leaf per listener under the data directory and serves them as an operator's pair.

use std::cmp::Ordering;
use std::fmt;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use boring::asn1::Asn1Time;
use boring::bn::{BigNum, MsbOption};
use boring::ec::{EcGroup, EcKey};
use boring::error::ErrorStack;
use boring::hash::MessageDigest;
use boring::nid::Nid;
use boring::pkey::{HasPublic, PKey, PKeyRef, Private};
use boring::ssl::{select_next_proto, AlpnError, SslAcceptor, SslMethod};
use boring::x509::extension::{
    AuthorityKeyIdentifier, BasicConstraints, ExtendedKeyUsage, KeyUsage, SubjectAlternativeName,
    SubjectKeyIdentifier,
};
use boring::x509::{X509Builder, X509Name, X509NameBuilder, X509NameRef, X509Ref, X509};
use phoebis_store::permissions;

use crate::config::ListenerTls;

/// The acceptor an accept loop takes: `None` is plain HTTP.
pub type Acceptor = Option<Arc<SslAcceptor>>;

/// A leaf this close to `not_after` earns a warning in `check-config` and a line in `status`, and starts.
pub const EXPIRY_WARNING_DAYS: i32 = 30;

/// The self-signed CA's lifetime: ten years.
pub const CA_LIFETIME_DAYS: u32 = 3_650;
/// A self-signed leaf's lifetime.
pub const LEAF_LIFETIME_DAYS: u32 = 365;

const CA_CERT: &str = "ca.pem";
const CA_KEY: &str = "ca.key";
/// The listeners whose leaves one CA signs, so a new CA voids both pairs.
const LISTENERS: [&str; 2] = ["admin", "proxy"];
const SECONDS_PER_DAY: i64 = 86_400;
/// X.520 caps a common name at 64 characters.
const COMMON_NAME_MAX: usize = 64;

/// A loaded listener: the acceptor, and the leaf's facts the start logs and `check-config` prints.
pub struct Loaded {
    pub acceptor: SslAcceptor,
    /// The file the leaf was read from, which `check-config` names for a self-signed leaf.
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
/// `Material`, `Stack` and `Unreadable` carry their cause in the line rather than as the error's source, so a chain
/// printer such as `main`'s prints the cause once, in the refusal line.
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
    #[error("{key}: {path}: {error}")]
    Material {
        key: &'static str,
        path: PathBuf,
        error: std::io::Error,
    },
    #[error("{key}.cert_file {path}: holds no PEM certificate")]
    NoCertificate { key: &'static str, path: PathBuf },
    #[error("{key}.key_file {path}: holds no PEM private key")]
    NoKey { key: &'static str, path: PathBuf },
    #[error("{key}: the leaf {path} is not signed by {ca}")]
    NotSignedByCa {
        key: &'static str,
        path: PathBuf,
        ca: PathBuf,
    },
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
        cert_file: cert_file.to_path_buf(),
        key_file: key_file.to_path_buf(),
        leaf: LeafFacts {
            names,
            not_after: leaf.not_after().to_string(),
            days_left,
        },
    })
}

/// `<data dir>/tls`, `data_dir_of(storage.path)` below: the one place the running service writes.
pub fn material_dir(storage_path: &Path) -> PathBuf {
    permissions::data_dir_of(storage_path).join("tls")
}

/// `<data dir>/tls/ca.pem`, the one file the operator copies out.
pub fn ca_path(storage_path: &Path) -> PathBuf {
    material_dir(storage_path).join(CA_CERT)
}

/// A validity window in Unix seconds, so a test can write material that is already expired.
#[derive(Clone, Copy, Debug)]
pub struct Validity {
    pub not_before: i64,
    pub not_after: i64,
}

impl Validity {
    /// From now until `days` from now.
    pub fn days_from_now(days: u32) -> Validity {
        let now = jiff::Timestamp::now().as_second();
        Validity {
            not_before: now,
            not_after: now + i64::from(days) * SECONDS_PER_DAY,
        }
    }
}

/// The CA as a signer: its certificate and key, read from `ca.pem` and `ca.key` or just written.
pub struct Ca {
    pub cert: X509,
    pub key: PKey<Private>,
}

/// One subject alternative name of a self-signed leaf.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SanName {
    Dns(String),
    Ip(IpAddr),
}

impl fmt::Display for SanName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SanName::Dns(name) => f.write_str(name),
            SanName::Ip(ip) => ip.fmt(f),
        }
    }
}

/// Writes `ca.pem` (0644) and `ca.key` (0600) under `dir`, staged: both land under `.tmp` names at their modes, are
/// read back and checked (the certificate parses and is signed by the key), then each moves into place by one
/// `rename`, replacing what was there. A stale `.tmp` of either name from an interrupted run is removed first.
/// `key` (`proxy.tls` or `admin.tls`) prefixes every refusal, as it does for an operator's pair.
pub fn write_ca(
    key: &'static str,
    dir: &Path,
    hostname: &str,
    validity: Validity,
) -> Result<Ca, TlsError> {
    permissions::create_private_dir(dir).map_err(material(key, dir))?;
    let (cert, pkey) = build_ca(hostname, validity).map_err(stack(key))?;
    let cert_tmp = stage(
        key,
        dir,
        CA_CERT,
        0o644,
        &cert.to_pem().map_err(stack(key))?,
    )?;
    let key_pem = pkey.private_key_to_pem_pkcs8().map_err(stack(key))?;
    let key_tmp = stage(key, dir, CA_KEY, 0o600, &key_pem)?;
    let (cert, pkey) = read_back(key, &cert_tmp, &key_tmp)?;
    if !signed_by(&cert, &pkey) {
        return Err(TlsError::KeyMismatch {
            key,
            path: key_tmp,
            cert: cert_tmp,
        });
    }
    place(key, &cert_tmp, &dir.join(CA_CERT))?;
    place(key, &key_tmp, &dir.join(CA_KEY))?;
    Ok(Ca { cert, key: pkey })
}

/// Writes `<listener>.pem` and `<listener>.key` (0600) under `dir`, signed by `ca`, with `names` as the subject
/// alternative names (DNS names and IP addresses), staged the same way: the two `.tmp` files are read back and
/// checked (both parse, the key matches the certificate, the leaf verifies under `ca`; the dates are not checked,
/// since a test writes expired material through this on purpose and `load` judges the dates at the start), then the
/// certificate and the key are renamed into place.
pub fn write_leaf(
    key: &'static str,
    dir: &Path,
    listener: &str,
    ca: &Ca,
    hostname: &str,
    names: &[SanName],
    validity: Validity,
) -> Result<(), TlsError> {
    permissions::create_private_dir(dir).map_err(material(key, dir))?;
    let (cert, pkey) = build_leaf(ca, hostname, names, validity).map_err(stack(key))?;
    let (cert_name, key_name) = (format!("{listener}.pem"), format!("{listener}.key"));
    let cert_tmp = stage(
        key,
        dir,
        &cert_name,
        0o600,
        &cert.to_pem().map_err(stack(key))?,
    )?;
    let key_pem = pkey.private_key_to_pem_pkcs8().map_err(stack(key))?;
    let key_tmp = stage(key, dir, &key_name, 0o600, &key_pem)?;
    let (cert, pkey) = read_back(key, &cert_tmp, &key_tmp)?;
    if !matches_key(&cert, &pkey) {
        return Err(TlsError::KeyMismatch {
            key,
            path: key_tmp,
            cert: cert_tmp,
        });
    }
    if !signed_by(&cert, &ca.key) {
        return Err(TlsError::NotSignedByCa {
            key,
            path: cert_tmp,
            ca: dir.join(CA_CERT),
        });
    }
    place(key, &cert_tmp, &dir.join(cert_name))?;
    place(key, &key_tmp, &dir.join(key_name))
}

/// The names a leaf for `listen` carries: the host's name, `localhost`, `127.0.0.1`, `::1`, the listen IP when it
/// is concrete, and every non-loopback unicast address of the host (`getifaddrs`) when it is unspecified.
pub fn leaf_names(listen: SocketAddr) -> Vec<SanName> {
    let mut names = vec![
        SanName::Dns(hostname()),
        SanName::Dns("localhost".to_owned()),
        SanName::Ip(Ipv4Addr::LOCALHOST.into()),
        SanName::Ip(Ipv6Addr::LOCALHOST.into()),
    ];
    if listen.ip().is_unspecified() {
        names.extend(
            interface_addresses()
                .into_iter()
                .filter(|ip| !ip.is_loopback() && !ip.is_unspecified() && !ip.is_multicast())
                .map(SanName::Ip),
        );
    } else {
        names.push(SanName::Ip(listen.ip()));
    }
    // Sorted and deduplicated, so a regenerated leaf carries the same names in the same order.
    names.sort_by(|a, b| match (a, b) {
        (SanName::Dns(a), SanName::Dns(b)) => a.cmp(b),
        (SanName::Ip(a), SanName::Ip(b)) => a.cmp(b),
        (SanName::Dns(_), SanName::Ip(_)) => Ordering::Less,
        (SanName::Ip(_), SanName::Dns(_)) => Ordering::Greater,
    });
    names.dedup();
    names
}

/// What a listener serves, for the start and for `check-config`.
pub enum Serving {
    Plain,
    Tls(Loaded),
    /// `self_signed` whose material is not generated yet; only `check-config` sees it (`generate` false).
    Pending {
        leaf: PathBuf,
    },
    /// `self_signed` whose CA the next start writes again, and every leaf with it; only `check-config` sees it.
    StaleCa {
        ca: PathBuf,
        reason: Stale,
    },
    /// `self_signed` whose leaf the next start writes again under the CA; only `check-config` sees it.
    StaleLeaf {
        leaf: PathBuf,
        reason: Stale,
    },
}

/// Why the next start writes a self-signed CA or leaf again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stale {
    Expired { date: String },
    KeyMismatch,
    Missing { file: String },
    NotSignedByCa,
    Unparseable { file: String },
}

impl fmt::Display for Stale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Stale::Expired { date } => write!(f, "expired on {date}"),
            Stale::KeyMismatch => f.write_str("the key does not match"),
            Stale::Missing { file } => write!(f, "{file} is missing"),
            Stale::NotSignedByCa => f.write_str("not signed by the CA"),
            Stale::Unparseable { file } => write!(f, "{file} does not parse"),
        }
    }
}

/// A self-signed CA or leaf as the files hold it: the start writes anything but `Holds` again.
enum Judged<T> {
    Absent,
    Stale(Stale),
    Holds(T),
}

/// One call per listener. `generate` is the start's word: missing or expired material is written (the CA when
/// `ca.pem` or `ca.key` is missing or expired, the leaf when its pair is missing, expired, or no longer signed by
/// the CA); under `check-config` nothing is written, missing material answers `Pending`, and material the next start
/// writes again answers `StaleCa` or `StaleLeaf` with the reason. Both refuse what the start cannot serve: a file it
/// cannot read, and a leaf that is not valid yet.
pub fn load(
    key: &'static str,
    listener: &'static str,
    tls: &Option<ListenerTls>,
    storage_path: &Path,
    listen: SocketAddr,
    generate: bool,
) -> Result<Serving, TlsError> {
    match ListenerTls::effective(tls) {
        ListenerTls::PlainHttp => Ok(Serving::Plain),
        ListenerTls::Pair {
            cert_file,
            key_file,
        } => load_pair(key, cert_file, key_file).map(Serving::Tls),
        ListenerTls::SelfSigned => {
            self_signed(key, listener, &material_dir(storage_path), listen, generate)
        }
    }
}

fn self_signed(
    key: &'static str,
    listener: &'static str,
    dir: &Path,
    listen: SocketAddr,
    generate: bool,
) -> Result<Serving, TlsError> {
    let leaf = dir.join(format!("{listener}.pem"));
    let leaf_key = dir.join(format!("{listener}.key"));
    if !generate {
        // The start's own judgement, read without writing, so `check-config` prints what the next start will do.
        return Ok(match judge_ca(key, dir)? {
            Judged::Stale(reason) => Serving::StaleCa {
                ca: dir.join(CA_CERT),
                reason,
            },
            Judged::Absent if absent(&leaf) && absent(&leaf_key) => Serving::Pending { leaf },
            Judged::Absent => Serving::StaleCa {
                ca: dir.join(CA_CERT),
                reason: Stale::Missing {
                    file: CA_CERT.to_owned(),
                },
            },
            Judged::Holds(ca) => match judge_leaf(key, &leaf, &leaf_key, &ca)? {
                Judged::Absent => Serving::Pending { leaf },
                Judged::Stale(reason) => Serving::StaleLeaf { leaf, reason },
                Judged::Holds(()) => Serving::Tls(load_pair(key, &leaf, &leaf_key)?),
            },
        });
    }
    permissions::create_private_dir(dir).map_err(material(key, dir))?;
    remove_temporaries(key, dir)?;
    let hostname = hostname();
    let ca = match judge_ca(key, dir)? {
        Judged::Holds(ca) => ca,
        judged => {
            let ca = write_ca(
                key,
                dir,
                &hostname,
                Validity::days_from_now(CA_LIFETIME_DAYS),
            )?;
            if !matches!(judged, Judged::Absent) {
                tracing::warn!(
                    ca = %dir.join(CA_CERT).display(),
                    "the self-signed CA was replaced; the clients must trust the new ca.pem"
                );
            }
            for name in LISTENERS
                .iter()
                .flat_map(|listener| [format!("{listener}.pem"), format!("{listener}.key")])
            {
                let path = dir.join(name);
                remove_if_present(&path).map_err(material(key, &path))?;
            }
            ca
        }
    };
    if !matches!(judge_leaf(key, &leaf, &leaf_key, &ca)?, Judged::Holds(())) {
        let names = leaf_names(listen);
        write_leaf(
            key,
            dir,
            listener,
            &ca,
            &hostname,
            &names,
            Validity::days_from_now(LEAF_LIFETIME_DAYS),
        )?;
        tracing::info!(
            listener,
            names = ?names.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "a self-signed leaf was written"
        );
    }
    load_pair(key, &leaf, &leaf_key).map(Serving::Tls)
}

/// The scheme word for a `listening` line and a URL: `https` when the value terminates TLS, else `http`.
pub fn scheme(tls: &Option<ListenerTls>) -> &'static str {
    if ListenerTls::effective(tls).terminates_tls() {
        "https"
    } else {
        "http"
    }
}

/// The `check-config` line for one listener: `{listener} listens on {listen} over plain HTTP`, `over TLS
/// (certificate {cert_file}, expires {not_after})` for a loaded pair, or a self-signed leaf's `over TLS (self-signed
/// leaf {path}, expires {not_after})`, its pending shape, or what the next start writes again and why. `serving` is
/// `load`'s answer for the same `tls`.
pub fn describe(
    listener: &str,
    listen: SocketAddr,
    tls: &Option<ListenerTls>,
    serving: &Serving,
) -> String {
    match (ListenerTls::effective(tls), serving) {
        (ListenerTls::Pair { cert_file, .. }, Serving::Tls(loaded)) => format!(
            "{listener} listens on {listen} over TLS (certificate {}, expires {})",
            cert_file.display(),
            loaded.leaf.not_after
        ),
        (ListenerTls::SelfSigned, Serving::Tls(loaded)) => format!(
            "{listener} listens on {listen} over TLS (self-signed leaf {}, expires {})",
            loaded.cert_file.display(),
            loaded.leaf.not_after
        ),
        (ListenerTls::SelfSigned, Serving::Pending { leaf }) => format!(
            "{listener} listens on {listen} over TLS (self-signed leaf {}, not generated yet; the first start \
             generates it)",
            leaf.display()
        ),
        (ListenerTls::SelfSigned, Serving::StaleLeaf { leaf, reason }) => format!(
            "{listener} listens on {listen} over TLS (self-signed leaf {}, {reason}; the next start regenerates it)",
            leaf.display()
        ),
        (ListenerTls::SelfSigned, Serving::StaleCa { ca, reason }) => format!(
            "{listener} listens on {listen} over TLS (self-signed CA {}, {reason}; the next start regenerates it \
             and every leaf)",
            ca.display()
        ),
        _ => format!("{listener} listens on {listen} over plain HTTP"),
    }
}

/// The `warning:` lines for a loaded pair or self-signed leaf, without the `warning: ` prefix: the leaf within
/// `EXPIRY_WARNING_DAYS`, and `permissions::check_private_file(key_file, 0o600)`'s lines for its key file.
pub fn warnings(key: &'static str, tls: &Option<ListenerTls>, serving: &Serving) -> Vec<String> {
    let Serving::Tls(loaded) = serving else {
        return Vec::new();
    };
    let leaf = match ListenerTls::effective(tls) {
        ListenerTls::Pair { .. } => format!("{key}.cert_file {}", loaded.cert_file.display()),
        ListenerTls::SelfSigned => format!("{key} self-signed leaf {}", loaded.cert_file.display()),
        ListenerTls::PlainHttp => return Vec::new(),
    };
    let mut warnings = Vec::new();
    // A negative count never reaches here: `load_pair` refuses an expired leaf.
    if (0..EXPIRY_WARNING_DAYS).contains(&loaded.leaf.days_left) {
        warnings.push(format!(
            "{leaf} expires in {} days ({})",
            loaded.leaf.days_left, loaded.leaf.not_after
        ));
    }
    warnings.extend(phoebis_store::permissions::check_private_file(
        &loaded.key_file,
        0o600,
    ));
    warnings
}

fn stack(key: &'static str) -> impl Fn(ErrorStack) -> TlsError {
    move |error| TlsError::Stack { key, error }
}

fn material<'a>(key: &'static str, path: &'a Path) -> impl FnOnce(std::io::Error) -> TlsError + 'a {
    move |error| TlsError::Material {
        key,
        path: path.to_path_buf(),
        error,
    }
}

fn new_key() -> Result<PKey<Private>, ErrorStack> {
    let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1)?;
    PKey::from_ec_key(EcKey::generate(&group)?)
}

fn common_name(text: &str) -> Result<X509Name, ErrorStack> {
    // BoringSSL refuses a longer common name; the subject alternative names carry the host's full name.
    let text = text
        .char_indices()
        .nth(COMMON_NAME_MAX)
        .map_or(text, |(end, _)| &text[..end]);
    let mut name = X509NameBuilder::new()?;
    name.append_entry_by_nid(Nid::COMMONNAME, text)?;
    Ok(name.build())
}

/// A builder with the fields both certificates share: v3, a random 128-bit serial, the names, the window, the key.
fn certificate(
    subject: &X509NameRef,
    issuer: &X509NameRef,
    key: &PKeyRef<Private>,
    validity: Validity,
) -> Result<X509Builder, ErrorStack> {
    let mut builder = X509Builder::new()?;
    // The field counts from zero, so 2 is X.509 v3, the version that carries extensions.
    builder.set_version(2)?;
    let mut serial = BigNum::new()?;
    serial.rand(128, MsbOption::MAYBE_ZERO, false)?;
    let serial = serial.to_asn1_integer()?;
    builder.set_serial_number(&serial)?;
    builder.set_subject_name(subject)?;
    builder.set_issuer_name(issuer)?;
    let (not_before, not_after) = (
        Asn1Time::from_unix(validity.not_before)?,
        Asn1Time::from_unix(validity.not_after)?,
    );
    builder.set_not_before(&not_before)?;
    builder.set_not_after(&not_after)?;
    builder.set_pubkey(key)?;
    Ok(builder)
}

fn build_ca(hostname: &str, validity: Validity) -> Result<(X509, PKey<Private>), ErrorStack> {
    let key = new_key()?;
    let name = common_name(&format!("Phoebis local CA {hostname}"))?;
    let mut builder = certificate(&name, &name, &key, validity)?;
    let extensions = [
        BasicConstraints::new().critical().ca().build()?,
        KeyUsage::new()
            .critical()
            .key_cert_sign()
            .crl_sign()
            .build()?,
        SubjectKeyIdentifier::new().build(&builder.x509v3_context(None, None))?,
    ];
    for extension in &extensions {
        builder.append_extension(extension)?;
    }
    builder.sign(&key, MessageDigest::sha256())?;
    Ok((builder.build(), key))
}

fn build_leaf(
    ca: &Ca,
    hostname: &str,
    names: &[SanName],
    validity: Validity,
) -> Result<(X509, PKey<Private>), ErrorStack> {
    let key = new_key()?;
    let subject = common_name(hostname)?;
    let mut builder = certificate(&subject, ca.cert.subject_name(), &key, validity)?;
    let mut alt_names = SubjectAlternativeName::new();
    for name in names {
        match name {
            SanName::Dns(dns) => alt_names.dns(dns),
            SanName::Ip(ip) => alt_names.ip(&ip.to_string()),
        };
    }
    let extensions = {
        let context = builder.x509v3_context(Some(&ca.cert), None);
        [
            BasicConstraints::new().critical().build()?,
            KeyUsage::new().critical().digital_signature().build()?,
            ExtendedKeyUsage::new().server_auth().build()?,
            alt_names.build(&context)?,
            SubjectKeyIdentifier::new().build(&context)?,
            AuthorityKeyIdentifier::new().keyid(true).build(&context)?,
        ]
    };
    for extension in &extensions {
        builder.append_extension(extension)?;
    }
    builder.sign(&ca.key, MessageDigest::sha256())?;
    Ok((builder.build(), key))
}

/// Writes `contents` to `<dir>/<name>.tmp` at `mode` through the store's helper, after removing a temporary an
/// interrupted run left; the live name is untouched until `place`.
fn stage(
    key: &'static str,
    dir: &Path,
    name: &str,
    mode: u32,
    contents: &[u8],
) -> Result<PathBuf, TlsError> {
    let tmp = dir.join(format!("{name}.tmp"));
    remove_if_present(&tmp).map_err(material(key, &tmp))?;
    permissions::create_private_file(&tmp, mode)
        .and_then(|mut file| file.write_all(contents))
        .map_err(material(key, &tmp))?;
    Ok(tmp)
}

/// `rename` replaces the live name in one step, so it is never half written and never meets the store helper's
/// refusal of an existing file.
fn place(key: &'static str, tmp: &Path, live: &Path) -> Result<(), TlsError> {
    std::fs::rename(tmp, live).map_err(material(key, live))
}

/// The staged pair as the disk holds it, so the check covers the bytes the listener will serve.
fn read_back(
    key: &'static str,
    cert_file: &Path,
    key_file: &Path,
) -> Result<(X509, PKey<Private>), TlsError> {
    let cert_pem = std::fs::read(cert_file).map_err(material(key, cert_file))?;
    let key_pem = std::fs::read(key_file).map_err(material(key, key_file))?;
    let cert = X509::from_pem(&cert_pem).map_err(|_| TlsError::NoCertificate {
        key,
        path: cert_file.to_path_buf(),
    })?;
    let pkey = PKey::private_key_from_pem(&key_pem).map_err(|_| TlsError::NoKey {
        key,
        path: key_file.to_path_buf(),
    })?;
    Ok((cert, pkey))
}

/// The CA as `ca.pem` and `ca.key` hold it. `Holds` needs both files, both parsing, the certificate unexpired and
/// signed by the key; a key that does not verify the certificate is what a stop between the CA's two renames leaves.
fn judge_ca(key: &'static str, dir: &Path) -> Result<Judged<Ca>, TlsError> {
    let (cert, pkey) = match judge_pair(key, &dir.join(CA_CERT), &dir.join(CA_KEY))? {
        Judged::Holds(pair) => pair,
        Judged::Absent => return Ok(Judged::Absent),
        Judged::Stale(reason) => return Ok(Judged::Stale(reason)),
    };
    if expired(&cert).map_err(stack(key))? {
        return Ok(Judged::Stale(Stale::Expired {
            date: cert.not_after().to_string(),
        }));
    }
    if !signed_by(&cert, &pkey) {
        return Ok(Judged::Stale(Stale::KeyMismatch));
    }
    Ok(Judged::Holds(Ca { cert, key: pkey }))
}

/// The listener's pair as its two files hold it. `Holds` needs both files, both parsing, the leaf unexpired, its key
/// matching, and its signature by `ca`; a mismatch is what a stop between the leaf's two renames leaves.
fn judge_leaf(
    key: &'static str,
    cert_file: &Path,
    key_file: &Path,
    ca: &Ca,
) -> Result<Judged<()>, TlsError> {
    let (cert, pkey) = match judge_pair(key, cert_file, key_file)? {
        Judged::Holds(pair) => pair,
        Judged::Absent => return Ok(Judged::Absent),
        Judged::Stale(reason) => return Ok(Judged::Stale(reason)),
    };
    if expired(&cert).map_err(stack(key))? {
        return Ok(Judged::Stale(Stale::Expired {
            date: cert.not_after().to_string(),
        }));
    }
    if !matches_key(&cert, &pkey) {
        return Ok(Judged::Stale(Stale::KeyMismatch));
    }
    if !signed_by(&cert, &ca.key) {
        return Ok(Judged::Stale(Stale::NotSignedByCa));
    }
    Ok(Judged::Holds(()))
}

/// A certificate and its key read and parsed: `Absent` when neither file exists, `Stale` naming the file that is
/// missing or does not parse. A read that fails for any other reason refuses.
fn judge_pair(
    key: &'static str,
    cert_file: &Path,
    key_file: &Path,
) -> Result<Judged<(X509, PKey<Private>)>, TlsError> {
    let name = |path: &Path| {
        path.file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
    };
    let (cert_pem, key_pem) = match (
        read_if_present(key, cert_file)?,
        read_if_present(key, key_file)?,
    ) {
        (None, None) => return Ok(Judged::Absent),
        (None, Some(_)) => {
            return Ok(Judged::Stale(Stale::Missing {
                file: name(cert_file),
            }))
        }
        (Some(_), None) => {
            return Ok(Judged::Stale(Stale::Missing {
                file: name(key_file),
            }))
        }
        (Some(cert_pem), Some(key_pem)) => (cert_pem, key_pem),
    };
    let Ok(cert) = X509::from_pem(&cert_pem) else {
        return Ok(Judged::Stale(Stale::Unparseable {
            file: name(cert_file),
        }));
    };
    let Ok(pkey) = PKey::private_key_from_pem(&key_pem) else {
        return Ok(Judged::Stale(Stale::Unparseable {
            file: name(key_file),
        }));
    };
    Ok(Judged::Holds((cert, pkey)))
}

/// The file's bytes, or `None` when it does not exist. Any other failure refuses the start rather than replacing
/// material the clients may already trust.
fn read_if_present(key: &'static str, path: &Path) -> Result<Option<Vec<u8>>, TlsError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(material(key, path)(error)),
    }
}

/// Only a confirmed absence counts: a path behind an unreadable directory is read next and refused with its error.
fn absent(path: &Path) -> bool {
    matches!(path.try_exists(), Ok(false))
}

fn remove_if_present(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

/// Removes every `.tmp` an interrupted write left, so no half-written file outlives the start that finds it.
fn remove_temporaries(key: &'static str, dir: &Path) -> Result<(), TlsError> {
    for entry in std::fs::read_dir(dir).map_err(material(key, dir))? {
        let path = entry.map_err(material(key, dir))?.path();
        if path.extension().is_some_and(|extension| extension == "tmp") {
            remove_if_present(&path).map_err(material(key, &path))?;
        }
    }
    Ok(())
}

fn expired(cert: &X509Ref) -> Result<bool, ErrorStack> {
    let now = Asn1Time::days_from_now(0)?;
    Ok(cert.not_after().compare(&now)? == Ordering::Less)
}

/// Whether `key` verifies `cert`'s signature. A failed verify leaves its reason on the thread's error queue, which
/// is drained so a later refusal's `ErrorStack` does not carry it.
fn signed_by<T: HasPublic>(cert: &X509Ref, key: &PKeyRef<T>) -> bool {
    let verified = matches!(cert.verify(key), Ok(true));
    if !verified {
        let _ = ErrorStack::get();
    }
    verified
}

fn matches_key(cert: &X509Ref, key: &PKeyRef<Private>) -> bool {
    let matched = cert.public_key().is_ok_and(|public| public.public_eq(key));
    if !matched {
        let _ = ErrorStack::get();
    }
    matched
}

/// The host's name as `gethostname` answers it, or `localhost` when it fails or is not a plain ASCII name.
fn hostname() -> String {
    let mut buffer = [0u8; 256];
    // SAFETY: the pointer and the length describe `buffer`, which outlives the call.
    let answered = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } == 0;
    let end = buffer
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(buffer.len());
    match std::str::from_utf8(&buffer[..end]) {
        Ok(name) if answered && !name.is_empty() && name.is_ascii() => name.to_owned(),
        _ => "localhost".to_owned(),
    }
}

/// Every IPv4 and IPv6 address on the host's interfaces, loopback included; `leaf_names` filters.
fn interface_addresses() -> Vec<IpAddr> {
    let mut addresses = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `getifaddrs` fills `head` with a list this function frees once, below.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return addresses;
    }
    let mut cursor = head;
    while !cursor.is_null() {
        // SAFETY: `cursor` is a node of the list `getifaddrs` returned, which stays alive until `freeifaddrs`; an
        // address's family says which `sockaddr` layout it has.
        unsafe {
            let entry = &*cursor;
            if !entry.ifa_addr.is_null() {
                match i32::from((*entry.ifa_addr).sa_family) {
                    libc::AF_INET => {
                        let address = &*entry.ifa_addr.cast::<libc::sockaddr_in>();
                        addresses.push(IpAddr::from(address.sin_addr.s_addr.to_ne_bytes()));
                    }
                    libc::AF_INET6 => {
                        let address = &*entry.ifa_addr.cast::<libc::sockaddr_in6>();
                        addresses.push(IpAddr::from(address.sin6_addr.s6_addr));
                    }
                    _ => {}
                }
            }
            cursor = entry.ifa_next;
        }
    }
    // SAFETY: `head` is the list `getifaddrs` returned, freed exactly once.
    unsafe { libc::freeifaddrs(head) };
    addresses
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
    fn plain_http_and_an_absent_key_load_no_acceptor_and_name_the_http_scheme() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("router.sqlite");
        let listen: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let loaded = |tls: &Option<ListenerTls>| {
            load("proxy.tls", "proxy", tls, &storage, listen, false).unwrap()
        };
        assert!(matches!(loaded(&None), Serving::Plain));
        assert!(matches!(
            loaded(&Some(ListenerTls::PlainHttp)),
            Serving::Plain
        ));
        assert_eq!(scheme(&None), "http");
        assert_eq!(scheme(&Some(ListenerTls::PlainHttp)), "http");
        let (cert_file, key_file) = generate_pair(&["localhost"]).write_to(dir.path(), "proxy");
        let pair = Some(ListenerTls::Pair {
            cert_file,
            key_file,
        });
        assert!(matches!(loaded(&pair), Serving::Tls(_)));
        assert_eq!(scheme(&pair), "https");
        assert_eq!(scheme(&Some(ListenerTls::SelfSigned)), "https");
    }

    #[test]
    fn a_leafs_names_carry_the_host_the_two_loopbacks_and_the_listen_address_sorted() {
        // The host's name sorts among the DNS names wherever it falls, and collapses into `localhost` on a host
        // that answers that name.
        let mut dns = vec![hostname(), "localhost".to_owned()];
        dns.sort();
        dns.dedup();
        let expected = |ips: &[&str]| {
            dns.iter()
                .cloned()
                .map(SanName::Dns)
                .chain(ips.iter().map(|ip| SanName::Ip(ip.parse().unwrap())))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            leaf_names("192.0.2.7:8317".parse().unwrap()),
            expected(&["127.0.0.1", "192.0.2.7", "::1"])
        );
        assert_eq!(
            leaf_names("127.0.0.1:8317".parse().unwrap()),
            expected(&["127.0.0.1", "::1"]),
            "a loopback listen address is named once"
        );
        let every = leaf_names("0.0.0.0:8317".parse().unwrap());
        assert!(
            every.iter().all(|name| match name {
                SanName::Ip(ip) =>
                    !ip.is_loopback() || ["127.0.0.1", "::1"].contains(&ip.to_string().as_str()),
                SanName::Dns(_) => true,
            }),
            "{every:?}"
        );
        assert_eq!(every, leaf_names("0.0.0.0:8317".parse().unwrap()));
        let mut deduplicated = every.clone();
        deduplicated.dedup();
        assert_eq!(deduplicated, every);
    }
}
