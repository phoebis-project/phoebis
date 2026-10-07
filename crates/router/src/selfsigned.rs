//! The certificate material `phoebis init --tls self-signed` writes: a local CA and one leaf per listener that leaves
//! loopback, under `tls/` beside the bootstrap file. `init` alone calls this; the running router loads the pair as any
//! operator's through `tls::load_pair` and generates nothing at start.
//!
//! A leaf's two files sit in a generation directory, `tls/<listener>.<16 hex digits>`, and the configuration names them
//! through the link `tls/<listener>`, which points at the live generation. A pair goes live by one switch of that link,
//! so the certificate and the key change together and a stop before the switch leaves the previous pair live.

use std::ffi::OsString;
use std::fmt;
use std::fs::{DirBuilder, File, Metadata, OpenOptions};
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use boring::asn1::Asn1Time;
use boring::bn::{BigNum, MsbOption};
use boring::ec::{EcGroup, EcKey};
use boring::error::ErrorStack;
use boring::hash::MessageDigest;
use boring::nid::Nid;
use boring::pkey::{HasPublic, PKey, PKeyRef, Private};
use boring::stack::Stack;
use boring::x509::extension::{
    AuthorityKeyIdentifier, BasicConstraints, ExtendedKeyUsage, KeyUsage, SubjectAlternativeName,
    SubjectKeyIdentifier,
};
use boring::x509::store::X509StoreBuilder;
use boring::x509::{
    X509Builder, X509Extension, X509Name, X509NameBuilder, X509NameRef, X509Ref, X509StoreContext,
    X509,
};
use phoebis_store::permissions;
use rand::Rng;

/// The CA's lifetime: ten years.
pub const CA_LIFETIME_DAYS: u32 = 3_650;
/// A leaf's lifetime.
pub const LEAF_LIFETIME_DAYS: u32 = 365;

pub const CA_CERT: &str = "ca.pem";
pub const CA_KEY: &str = "ca.key";
/// A leaf's certificate and key inside its generation directory.
pub const LEAF_CERT: &str = "cert.pem";
pub const LEAF_KEY: &str = "key.pem";
/// The file a renewal locks under `tls/` for the whole run.
pub const RENEWAL_LOCK: &str = "renewal.lock";

/// The ending of every refusal that leaves the operator one way out: a new CA, which every client trusts again.
const NEW_CA: &str =
    "remove tls/ca.pem and tls/ca.key for a new CA, which every client must trust again";
const SECONDS_PER_DAY: i64 = 86_400;
/// X.520 caps a common name at 64 characters.
const COMMON_NAME_MAX: usize = 64;

/// A validity window in Unix seconds, so a test writes material that is already expired.
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

/// One subject alternative name of a leaf; the DNS names are also the CA's permitted names. The order is the one
/// `leaf_names` sorts by: DNS names before addresses, each in its own order.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
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

/// The owner and modes a leaf's two files land with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeafModes {
    /// The host's shape: the certificate and the key at 0600, both the caller's; `--owner` hands them over after.
    Private,
    /// The container's shape: the certificate at 0644, which the container's uid reads through the bind mount, and the
    /// key at 0600 until the operator hands it over.
    Mounted,
    /// The renewal: the staged generation and each file in it take the owner and mode of the live ones they replace,
    /// and the live certificate must be one the CA signed, so a renewal never replaces an operator's pair named under
    /// `tls/`.
    KeepLive,
}

/// One line per refusal, the path first; the CA's lines end with `NEW_CA`.
#[derive(Debug, thiserror::Error)]
pub enum MaterialError {
    #[error("{path}: expired on {date}; {}", NEW_CA)]
    ExpiredCa { path: PathBuf, date: String },
    #[error("{path} exists without {other}; {}", NEW_CA)]
    HalfCa { path: PathBuf, other: PathBuf },
    #[error("{path}: {error}")]
    Io {
        path: PathBuf,
        error: std::io::Error,
    },
    #[error("{key}: does not match the certificate in {cert}; {}", NEW_CA)]
    KeyMismatch { key: PathBuf, cert: PathBuf },
    #[error("{path}: another renewal holds the lock; run this one again once it finishes")]
    Locked { path: PathBuf },
    #[error("{dir} holds no ca.pem and ca.key; a renewal needs the CA the clients trust")]
    NoCa { dir: PathBuf },
    #[error("{path}: holds no PEM certificate; {}", NEW_CA)]
    NoCertificate { path: PathBuf },
    #[error("{path}: holds no PEM private key; {}", NEW_CA)]
    NoKey { path: PathBuf },
    #[error(
        "{path}: holds no certificate the CA in {ca} signed; init --tls self-signed renews only the leaves it wrote"
    )]
    NotSignedByCa { path: PathBuf, ca: PathBuf },
    #[error(
        "the leaf's names ({names}) are not all within the CA's name constraint; {}",
        NEW_CA
    )]
    OutsideConstraint { names: String },
    #[error("{0}")]
    Stack(#[from] ErrorStack),
}

/// The names a leaf for `listen` carries: the host's name, `localhost`, `127.0.0.1`, `::1`, the listen IP when it is
/// concrete, and every non-loopback unicast address of the host (`getifaddrs`) when it is unspecified; sorted and
/// deduplicated, so a renewed leaf carries the same names in the same order.
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
    names.sort();
    names.dedup();
    names
}

/// The host's name as `gethostname` answers it, or `localhost` when it fails or is not a plain ASCII name.
pub fn hostname() -> String {
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

/// The two names the configuration carries for `listener`'s generated pair under `dir`, through its link: the
/// certificate then the key.
pub fn leaf_files(dir: &Path, listener: &str) -> (PathBuf, PathBuf) {
    let link = dir.join(listener);
    (link.join(LEAF_CERT), link.join(LEAF_KEY))
}

/// The renewal's hold on `<dir>/renewal.lock`, released when dropped.
pub struct RenewalLock {
    _file: File,
}

/// Locks `<dir>/renewal.lock` for one renewal, or refuses while another run holds it, so two renewals never stage
/// and switch beside each other. The file is never removed: a run that opened it before a removal would lock a name
/// the next run no longer finds, and both would go ahead.
pub fn lock_renewal(dir: &Path) -> Result<RenewalLock, MaterialError> {
    let path = dir.join(RENEWAL_LOCK);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(io(&path))?;
    match file.try_lock() {
        Ok(()) => Ok(RenewalLock { _file: file }),
        Err(std::fs::TryLockError::WouldBlock) => Err(MaterialError::Locked { path }),
        Err(std::fs::TryLockError::Error(error)) => Err(io(&path)(error)),
    }
}

/// `ca.pem` and `ca.key` under `dir` as a signer: `Ok(None)` when neither file exists. A half CA, a file that does not
/// parse, a key that does not match the certificate or an expired certificate refuses naming the file, since a CA the
/// clients trust is never replaced behind their back.
pub fn read_ca(dir: &Path) -> Result<Option<Ca>, MaterialError> {
    let (cert_file, key_file) = (dir.join(CA_CERT), dir.join(CA_KEY));
    // Only a confirmed absence counts as no CA: any other failure to read refuses rather than writing a new one.
    let read = |path: &Path| match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io(path)(error)),
    };
    let (cert_pem, key_pem) = match (read(&cert_file)?, read(&key_file)?) {
        (None, None) => return Ok(None),
        (Some(_), None) => {
            return Err(MaterialError::HalfCa {
                path: cert_file,
                other: key_file,
            })
        }
        (None, Some(_)) => {
            return Err(MaterialError::HalfCa {
                path: key_file,
                other: cert_file,
            })
        }
        (Some(cert_pem), Some(key_pem)) => (cert_pem, key_pem),
    };
    let cert = X509::from_pem(&cert_pem).map_err(|_| MaterialError::NoCertificate {
        path: cert_file.clone(),
    })?;
    let key = PKey::private_key_from_pem(&key_pem).map_err(|_| MaterialError::NoKey {
        path: key_file.clone(),
    })?;
    // The CA signs itself, so its own key verifying the signature is the key matching the certificate.
    if !signed_by(&cert, &key) {
        return Err(MaterialError::KeyMismatch {
            key: key_file,
            cert: cert_file,
        });
    }
    if expired(&cert)? {
        return Err(MaterialError::ExpiredCa {
            path: cert_file,
            date: cert.not_after().to_string(),
        });
    }
    Ok(Some(Ca { cert, key }))
}

/// Writes `ca.pem` (0644) and `ca.key` (0600) under `dir`, staged: both land under `.tmp` names at their modes through
/// the store's private-file helper, are read back and checked (the certificate parses and is signed by the key), then
/// each takes its live name through `place_new`, which refuses a name that exists. The CA is name-constrained to the
/// DNS names in `permitted`, every name the leaves will carry, so a client that trusts it trusts it for this host's
/// names and no other host's.
pub fn write_ca(
    dir: &Path,
    hostname: &str,
    permitted: &[SanName],
    validity: Validity,
) -> Result<Ca, MaterialError> {
    permissions::create_private_dir(dir).map_err(io(dir))?;
    let (cert, key) = build_ca(hostname, permitted, validity)?;
    let (cert_file, key_file) = (dir.join(CA_CERT), dir.join(CA_KEY));
    let cert_tmp = stage(&cert_file, 0o644, &cert.to_pem()?)?;
    let key_tmp = stage(&key_file, 0o600, &key.private_key_to_pem_pkcs8()?)
        .inspect_err(|_| discard(&[&cert_tmp]))?;
    let checked = read_back(&cert_tmp, &key_tmp).and_then(|(cert, key)| {
        if signed_by(&cert, &key) {
            Ok(Ca { cert, key })
        } else {
            Err(MaterialError::KeyMismatch {
                key: key_tmp.clone(),
                cert: cert_tmp.clone(),
            })
        }
    });
    let ca = checked.inspect_err(|_| discard(&[&cert_tmp, &key_tmp]))?;
    place_new(&[(&cert_tmp, &cert_file), (&key_tmp, &key_file)])?;
    Ok(ca)
}

/// Writes `listener`'s pair under `dir`, signed by `ca`, with `names` as the subject alternative names, at `modes`:
/// `write_leaves` for one leaf. Answers the two names the configuration carries, the certificate then the key.
pub fn write_leaf(
    dir: &Path,
    listener: &str,
    ca: &Ca,
    hostname: &str,
    names: &[SanName],
    validity: Validity,
    modes: LeafModes,
) -> Result<(PathBuf, PathBuf), MaterialError> {
    let mut written = write_leaves(
        dir,
        ca,
        hostname,
        &[(listener, names.to_vec())],
        validity,
        modes,
    )
    .map_err(|unfinished| unfinished.error)?;
    Ok(written.remove(0).files())
}

/// A leaf as it went live: `link`, the name the configuration reaches the pair through, points at `generation`, the
/// directory holding `cert.pem` and `key.pem`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placed {
    pub link: PathBuf,
    pub generation: PathBuf,
}

impl Placed {
    /// The two names the configuration carries, through the link: the certificate then the key.
    pub fn files(&self) -> (PathBuf, PathBuf) {
        (self.link.join(LEAF_CERT), self.link.join(LEAF_KEY))
    }
}

/// A `write_leaves` that stopped at `error`, with the leaves it had switched live by then: the caller records them as
/// written as it would on success.
#[derive(Debug)]
pub struct Unfinished {
    pub error: MaterialError,
    /// Each leaf switched live before the stop; empty when the stop came before any switch.
    pub placed: Vec<Placed>,
}

impl From<MaterialError> for Unfinished {
    fn from(error: MaterialError) -> Self {
        Unfinished {
            error,
            placed: Vec::new(),
        }
    }
}

/// Writes one leaf per entry of `leaves` (the listener's name and the names its leaf carries) under `dir`, signed by
/// `ca`, at `modes`. Every leaf is staged and checked before any goes live: its two files land in a new generation
/// directory through the store's private-file helper, synced, and are read back (both parse, the key matches the
/// certificate, and the leaf verifies under a store holding `ca` alone, which is where a name outside the CA's
/// constraint is caught), so a refusal or a failed write on any leaf leaves every live pair as it was. Each leaf then
/// goes live by one switch of its link: a first write creates the link, which refuses a name that exists, and a
/// renewal renames a new link over the live one, then removes the generations before the one it replaced. Answers
/// each leaf as it went live, in the order of `leaves`; a switch that fails once a leaf is live answers the leaves
/// switched before it beside the error.
pub fn write_leaves(
    dir: &Path,
    ca: &Ca,
    hostname: &str,
    leaves: &[(&str, Vec<SanName>)],
    validity: Validity,
    modes: LeafModes,
) -> Result<Vec<Placed>, Unfinished> {
    permissions::create_private_dir(dir).map_err(io(dir))?;
    let mut staged = Vec::with_capacity(leaves.len());
    for (listener, names) in leaves {
        match stage_leaf(dir, listener, ca, hostname, names, validity, modes) {
            Ok(leaf) => staged.push(leaf),
            Err(error) => {
                staged.iter().for_each(StagedLeaf::discard);
                return Err(error.into());
            }
        }
    }
    let mut placed = Vec::with_capacity(staged.len());
    for (at, leaf) in staged.iter().enumerate() {
        if let Err(error) = leaf.switch(dir, modes) {
            staged[at..].iter().for_each(StagedLeaf::discard);
            return Err(Unfinished { error, placed });
        }
        placed.push(Placed {
            link: leaf.link.clone(),
            generation: leaf.generation.clone(),
        });
    }
    if let Err(error) = sync_dir(dir) {
        return Err(Unfinished { error, placed });
    }
    if modes == LeafModes::KeepLive {
        staged.iter().for_each(|leaf| leaf.sweep(dir));
    }
    Ok(placed)
}

/// A leaf's generation directory written, synced and checked, not yet live.
struct StagedLeaf {
    listener: String,
    link: PathBuf,
    generation: PathBuf,
    /// The live generation's name as the link held it before this run, which the sweep keeps.
    previous: Option<OsString>,
}

impl StagedLeaf {
    fn discard(&self) {
        remove_generation(&self.generation);
    }

    /// Points the link at this generation in one step. A first write creates the link, so a name that exists refuses
    /// rather than being replaced; a renewal creates the new link beside the live one and renames it over, so a stop at
    /// any point leaves the link naming one whole generation.
    fn switch(&self, dir: &Path, modes: LeafModes) -> Result<(), MaterialError> {
        let target = self
            .generation
            .file_name()
            .expect("a generation directory has a name");
        if modes != LeafModes::KeepLive {
            return std::os::unix::fs::symlink(target, &self.link).map_err(io(&self.link));
        }
        let next = dir.join(format!(".{}.next", self.listener));
        remove_if_present(&next).map_err(io(&next))?;
        std::os::unix::fs::symlink(target, &next).map_err(io(&next))?;
        std::fs::rename(&next, &self.link).map_err(|error| {
            let _ = remove_if_present(&next);
            io(&self.link)(error)
        })
    }

    /// Removes this listener's generations other than the live one and the one it replaced, which a start that read
    /// the link just before the switch may still be reading. A removal that fails leaves the directory for the next
    /// renewal.
    fn sweep(&self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let live = self.generation.file_name();
        for entry in entries.flatten() {
            let name = entry.file_name();
            if Some(name.as_os_str()) == live || Some(&name) == self.previous.as_ref() {
                continue;
            }
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            if is_dir && is_generation_of(&self.listener, &name) {
                remove_generation(&entry.path());
            }
        }
    }
}

/// Whether `name` is a generation directory's name for `listener`: the listener, a dot, and sixteen hex digits.
fn is_generation_of(listener: &str, name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .and_then(|name| name.strip_prefix(listener))
        .and_then(|rest| rest.strip_prefix('.'))
        .is_some_and(|digits| digits.len() == 16 && digits.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Removes a generation's two files and the directory; whatever cannot be removed stays.
fn remove_generation(generation: &Path) {
    let _ = remove_if_present(&generation.join(LEAF_CERT));
    let _ = remove_if_present(&generation.join(LEAF_KEY));
    let _ = std::fs::remove_dir(generation);
}

/// Creates a new generation directory for `listener` under `dir` at `mode`, set on the directory so the umask cannot
/// narrow the container's `0755`.
fn new_generation(dir: &Path, listener: &str, mode: u32) -> Result<PathBuf, MaterialError> {
    let generation = dir.join(format!("{listener}.{:016x}", rand::rng().next_u64()));
    DirBuilder::new()
        .mode(0o700)
        .create(&generation)
        .and_then(|()| std::fs::set_permissions(&generation, std::fs::Permissions::from_mode(mode)))
        .map_err(|error| {
            let _ = std::fs::remove_dir(&generation);
            io(&generation)(error)
        })?;
    Ok(generation)
}

fn sync_dir(dir: &Path) -> Result<(), MaterialError> {
    File::open(dir)
        .and_then(|file| file.sync_all())
        .map_err(io(dir))
}

fn stage_leaf(
    dir: &Path,
    listener: &str,
    ca: &Ca,
    hostname: &str,
    names: &[SanName],
    validity: Validity,
    modes: LeafModes,
) -> Result<StagedLeaf, MaterialError> {
    let link = dir.join(listener);
    let (dir_mode, cert_mode, keep, previous) = match modes {
        LeafModes::Private => (0o700, 0o600, None, None),
        LeafModes::Mounted => (0o755, 0o644, None, None),
        LeafModes::KeepLive => {
            let (cert_file, key_file) = leaf_files(dir, listener);
            signed_leaf(dir, &cert_file, ca)?;
            let metadata = |live: &Path| std::fs::metadata(live).map_err(io(live));
            let keep = (
                metadata(&link)?,
                metadata(&cert_file)?,
                metadata(&key_file)?,
            );
            let previous = std::fs::read_link(&link).ok().map(PathBuf::into_os_string);
            (0o700, 0o600, Some(keep), previous)
        }
    };
    let (cert, key) = build_leaf(ca, hostname, names, validity)?;
    let (cert_pem, key_pem) = (cert.to_pem()?, key.private_key_to_pem_pkcs8()?);
    let staged = StagedLeaf {
        listener: listener.to_owned(),
        link,
        generation: new_generation(dir, listener, dir_mode)?,
        previous,
    };
    let (cert_file, key_file) = (
        staged.generation.join(LEAF_CERT),
        staged.generation.join(LEAF_KEY),
    );
    let checked = write_new(
        &cert_file,
        cert_mode,
        &cert_pem,
        keep.as_ref().map(|(_, cert, _)| cert),
    )
    .and_then(|()| {
        write_new(
            &key_file,
            0o600,
            &key_pem,
            keep.as_ref().map(|(_, _, key)| key),
        )
    })
    .and_then(|()| check_leaf(&cert_file, &key_file, ca, names, validity.not_before))
    .and_then(|()| match &keep {
        Some((live, _, _)) => {
            take_owner_and_mode(&staged.generation, live).map_err(io(&staged.generation))
        }
        None => Ok(()),
    })
    .and_then(|()| sync_dir(&staged.generation));
    checked.inspect_err(|_| staged.discard())?;
    Ok(staged)
}

/// The staged pair as the disk holds it, checked: both parse, the key matches the certificate, and the leaf chains to
/// `ca` alone at `not_before`, the start of its window.
fn check_leaf(
    cert_file: &Path,
    key_file: &Path,
    ca: &Ca,
    names: &[SanName],
    not_before: i64,
) -> Result<(), MaterialError> {
    read_back(cert_file, key_file).and_then(|(cert, key)| {
        if !matches_key(&cert, &key) {
            return Err(MaterialError::KeyMismatch {
                key: key_file.to_path_buf(),
                cert: cert_file.to_path_buf(),
            });
        }
        // The leaf was signed by `ca.key` a moment ago, so a chain that fails here fails for its names alone.
        if !verifies_under(&cert, ca, not_before)? {
            return Err(MaterialError::OutsideConstraint {
                names: names
                    .iter()
                    .map(SanName::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
            });
        }
        Ok(())
    })
}

/// Gives `path` the owner and mode in `live`, through a descriptor, so the staged generation keeps the group a service
/// unit reads the live one through.
fn take_owner_and_mode(path: &Path, live: &Metadata) -> std::io::Result<()> {
    let file = File::open(path)?;
    std::os::unix::fs::fchown(&file, Some(live.uid()), Some(live.gid()))?;
    file.set_permissions(std::fs::Permissions::from_mode(live.mode() & 0o777))
}

/// Refuses a live certificate `ca` did not sign, so a renewal never replaces an operator's pair named under `tls/`.
/// The signature alone is checked, never the dates, since an expiring leaf is what a renewal is for.
fn signed_leaf(dir: &Path, cert_file: &Path, ca: &Ca) -> Result<(), MaterialError> {
    let pem = std::fs::read(cert_file).map_err(io(cert_file))?;
    match X509::from_pem(&pem) {
        Ok(cert) if signed_by(&cert, &ca.key) => Ok(()),
        _ => Err(MaterialError::NotSignedByCa {
            path: cert_file.to_path_buf(),
            ca: dir.join(CA_CERT),
        }),
    }
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> MaterialError + '_ {
    move |error| MaterialError::Io {
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

fn build_ca(
    hostname: &str,
    permitted: &[SanName],
    validity: Validity,
) -> Result<(X509, PKey<Private>), ErrorStack> {
    let key = new_key()?;
    let name = common_name(&format!("Phoebis local CA {hostname}"))?;
    let mut builder = certificate(&name, &name, &key, validity)?;
    let extensions = [
        // The CA signs leaves only; `NODE_EXTRA_CA_CERTS` makes it a root for every connection the client opens.
        BasicConstraints::new().critical().ca().pathlen(0).build()?,
        KeyUsage::new()
            .critical()
            .key_cert_sign()
            .crl_sign()
            .build()?,
        SubjectKeyIdentifier::new().build(&builder.x509v3_context(None, None))?,
        // boring has no builder for this extension. The value is OpenSSL's configuration mini-language, which can
        // name files, so it is built from the names `leaf_names` computed and never from input.
        X509Extension::new_nid(
            None,
            Some(&builder.x509v3_context(None, None)),
            Nid::NAME_CONSTRAINTS,
            &name_constraint(permitted),
        )?,
    ];
    for extension in &extensions {
        builder.append_extension(extension)?;
    }
    builder.sign(&key, MessageDigest::sha256())?;
    Ok((builder.build(), key))
}

/// The critical name constraint permitting the DNS names in `permitted` alone, each with its subdomains. An address
/// gets no subtree and so stays unconstrained: BoringSSL's verifier answers any address subtree with "unsupported name
/// constraint type", which would refuse every leaf in `write_leaf`'s check and in each boring client trusting the CA.
fn name_constraint(permitted: &[SanName]) -> String {
    let mut value = "critical".to_owned();
    for name in permitted {
        if let SanName::Dns(dns) = name {
            value.push_str(",permitted;DNS:");
            value.push_str(dns);
        }
    }
    value
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

/// Writes `contents` to `<live>.tmp` at `mode` through `write_new`, after removing a temporary an interrupted run left;
/// the live name is untouched until `place_new`.
fn stage(live: &Path, mode: u32, contents: &[u8]) -> Result<PathBuf, MaterialError> {
    let mut name = live.as_os_str().to_owned();
    name.push(".tmp");
    let tmp = PathBuf::from(name);
    remove_if_present(&tmp).map_err(io(&tmp))?;
    write_new(&tmp, mode, contents, None)?;
    Ok(tmp)
}

/// Creates `path` at `mode` through the store's helper, which refuses an existing file, writes `contents` and syncs
/// it. Under `keep`, the live file's metadata, the file takes that owner and mode first, so a renewal under the
/// service unit keeps the group the service reads through. A failed write removes the file.
fn write_new(
    path: &Path,
    mode: u32,
    contents: &[u8],
    keep: Option<&Metadata>,
) -> Result<(), MaterialError> {
    let mut file = permissions::create_private_file(path, mode).map_err(io(path))?;
    let written = file.write_all(contents).and_then(|()| {
        if let Some(live) = keep {
            std::os::unix::fs::fchown(&file, Some(live.uid()), Some(live.gid()))?;
            file.set_permissions(std::fs::Permissions::from_mode(live.mode() & 0o777))?;
        }
        file.sync_all()
    });
    if let Err(error) = written {
        discard(&[path]);
        return Err(io(path)(error));
    }
    Ok(())
}

/// Gives each staged file its live name with a hard link, which appears whole and refuses a name that exists, so a
/// file that landed after the caller looked is never replaced; the staged names go once every link is made. A link
/// that fails removes the live names this call made and every temporary, so what it published is never left half.
fn place_new(staged: &[(&Path, &Path)]) -> Result<(), MaterialError> {
    let temporaries: Vec<&Path> = staged.iter().map(|(tmp, _)| *tmp).collect();
    for (at, (tmp, live)) in staged.iter().enumerate() {
        if let Err(error) = std::fs::hard_link(tmp, live) {
            for (_, made) in &staged[..at] {
                let _ = remove_if_present(made);
            }
            discard(&temporaries);
            return Err(io(live)(error));
        }
    }
    discard(&temporaries);
    Ok(())
}

/// Removes staged temporaries after a refusal; a removal that fails leaves a `.tmp` that the next `stage` of the same
/// name removes first, so the refusal stays the error reported.
fn discard(temporaries: &[&Path]) {
    for tmp in temporaries {
        let _ = remove_if_present(tmp);
    }
}

/// The staged pair as the disk holds it, so the check covers the bytes the listener will serve.
fn read_back(cert_file: &Path, key_file: &Path) -> Result<(X509, PKey<Private>), MaterialError> {
    let cert_pem = std::fs::read(cert_file).map_err(io(cert_file))?;
    let key_pem = std::fs::read(key_file).map_err(io(key_file))?;
    let cert = X509::from_pem(&cert_pem).map_err(|_| MaterialError::NoCertificate {
        path: cert_file.to_path_buf(),
    })?;
    let key = PKey::private_key_from_pem(&key_pem).map_err(|_| MaterialError::NoKey {
        path: key_file.to_path_buf(),
    })?;
    Ok((cert, key))
}

fn remove_if_present(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn expired(cert: &X509Ref) -> Result<bool, ErrorStack> {
    let now = Asn1Time::days_from_now(0)?;
    Ok(cert.not_after().compare(&now)? == std::cmp::Ordering::Less)
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

/// Whether `leaf` chains to `ca` under a store holding `ca` alone, the check that applies the CA's name constraint,
/// verified as of `at`. The verifier's own clock is `time()`, the kernel's coarse clock, which can trail the clock that
/// stamped a leaf's `not_before` across a second boundary and read a leaf signed a moment ago as not yet valid.
fn verifies_under(leaf: &X509Ref, ca: &Ca, at: i64) -> Result<bool, ErrorStack> {
    let mut store = X509StoreBuilder::new()?;
    store.add_cert(ca.cert.clone())?;
    let store = store.build();
    let chain = Stack::<X509>::new()?;
    let verified = X509StoreContext::new()?.init(&store, leaf, &chain, |context| {
        context.verify_param_mut().set_time(at as libc::time_t);
        context.verify_cert()
    })?;
    if !verified {
        let _ = ErrorStack::get();
    }
    Ok(verified)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn temporaries(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "tmp"))
            .collect()
    }

    /// Every generation directory under `dir`, sorted.
    fn generations(dir: &Path) -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap())
            .filter(|entry| {
                let name = entry.file_name();
                is_generation_of("proxy", &name) || is_generation_of("admin", &name)
            })
            .map(|entry| entry.path())
            .collect();
        found.sort();
        found
    }

    fn certificate_in(path: &Path) -> X509 {
        X509::from_pem(&std::fs::read(path).unwrap()).unwrap()
    }

    /// Whether `leaf` chains to `ca` under a store that holds `ca` and nothing else.
    fn verifies_under(ca: &X509, leaf: &X509) -> bool {
        let mut store = X509StoreBuilder::new().unwrap();
        store.add_cert(ca.clone()).unwrap();
        let store = store.build();
        X509StoreContext::new()
            .unwrap()
            .init(&store, leaf, &Stack::<X509>::new().unwrap(), |context| {
                context.verify_cert()
            })
            .unwrap()
    }

    fn days_left(cert: &X509) -> i32 {
        Asn1Time::days_from_now(0)
            .unwrap()
            .diff(cert.not_after())
            .unwrap()
            .days
    }

    fn alt_names(cert: &X509) -> Vec<SanName> {
        cert.subject_alt_names()
            .unwrap()
            .iter()
            .map(|name| match (name.dnsname(), name.ipaddress()) {
                (Some(dns), _) => SanName::Dns(dns.to_owned()),
                (None, Some(octets)) if octets.len() == 4 => {
                    SanName::Ip(IpAddr::from(<[u8; 4]>::try_from(octets).unwrap()))
                }
                (None, Some(octets)) => {
                    SanName::Ip(IpAddr::from(<[u8; 16]>::try_from(octets).unwrap()))
                }
                (None, None) => panic!("a subject alternative name that is neither DNS nor IP"),
            })
            .collect()
    }

    #[test]
    fn a_ca_and_a_leaf_land_staged_at_their_modes_and_the_leaf_verifies_under_the_ca() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let names = leaf_names("192.0.2.7:8317".parse().unwrap());
        let ca = write_ca(
            dir,
            "host.example",
            &names,
            Validity::days_from_now(CA_LIFETIME_DAYS),
        )
        .unwrap();
        let (cert, key) = write_leaf(
            dir,
            "proxy",
            &ca,
            "host.example",
            &names,
            Validity::days_from_now(LEAF_LIFETIME_DAYS),
            LeafModes::Private,
        )
        .unwrap();
        assert_eq!(
            (cert.clone(), key.clone()),
            (
                dir.join("proxy").join(LEAF_CERT),
                dir.join("proxy").join(LEAF_KEY)
            )
        );
        assert_eq!(mode(&dir.join(CA_CERT)), 0o644);
        for path in [dir.join(CA_KEY), cert.clone(), key.clone()] {
            assert_eq!(mode(&path), 0o600, "{}", path.display());
        }
        // The configuration's names reach the pair through the link to its generation directory.
        let generation = std::fs::read_link(dir.join("proxy")).unwrap();
        assert!(
            is_generation_of("proxy", generation.as_os_str()),
            "{}",
            generation.display()
        );
        assert_eq!(mode(&dir.join(&generation)), 0o700);
        assert!(temporaries(dir).is_empty(), "{:?}", temporaries(dir));
        // The start's loader accepts what `init` writes, as it accepts any operator's pair.
        if let Err(error) = crate::tls::load_pair("proxy.tls", &cert, &key) {
            panic!("{error}");
        }
        let (ca_cert, leaf) = (certificate_in(&dir.join(CA_CERT)), certificate_in(&cert));
        assert_eq!(alt_names(&leaf), names);
        assert!(verifies_under(&ca_cert, &leaf));
        assert!(
            (3_649..=3_651).contains(&days_left(&ca_cert)),
            "{}",
            days_left(&ca_cert)
        );
        assert!(
            (364..=366).contains(&days_left(&leaf)),
            "{}",
            days_left(&leaf)
        );

        // A renewal under the service unit finds the certificate at 0644 and the key at 0640 for the service's
        // group, and each renewed file keeps its own.
        std::fs::set_permissions(&cert, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o640)).unwrap();
        let (cert, key) = write_leaf(
            dir,
            "proxy",
            &ca,
            "host.example",
            &names,
            Validity::days_from_now(LEAF_LIFETIME_DAYS),
            LeafModes::KeepLive,
        )
        .unwrap();
        assert_eq!((mode(&cert), mode(&key)), (0o644, 0o640));
        assert!(verifies_under(&ca_cert, &certificate_in(&cert)));
        assert!(temporaries(dir).is_empty(), "{:?}", temporaries(dir));
        assert_ne!(std::fs::read_link(dir.join("proxy")).unwrap(), generation);

        // The container's uid reads the certificate through a bind mount; the key waits for the operator's hand-over.
        let (cert, key) = write_leaf(
            dir,
            "admin",
            &ca,
            "host.example",
            &names,
            Validity::days_from_now(LEAF_LIFETIME_DAYS),
            LeafModes::Mounted,
        )
        .unwrap();
        assert_eq!((mode(&cert), mode(&key)), (0o644, 0o600));
        assert_eq!(mode(&dir.join("admin")), 0o755);
    }

    #[test]
    fn a_renewal_refuses_a_pair_its_ca_did_not_sign_and_leaves_it_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let names = leaf_names("192.0.2.7:8317".parse().unwrap());
        let ca = write_ca(
            dir,
            "host.example",
            &names,
            Validity::days_from_now(CA_LIFETIME_DAYS),
        )
        .unwrap();
        // An operator's pair from a CA of their own, named under tls/ through the pair flags.
        let pair = fake_upstream::generate_pair(&["localhost"]);
        let (cert, key) = leaf_files(dir, "proxy");
        std::fs::create_dir(dir.join("proxy")).unwrap();
        std::fs::write(&cert, &pair.cert_pem).unwrap();
        std::fs::write(&key, &pair.key_pem).unwrap();
        let before = (std::fs::read(&cert).unwrap(), std::fs::read(&key).unwrap());
        let error = write_leaf(
            dir,
            "proxy",
            &ca,
            "host.example",
            &names,
            Validity::days_from_now(LEAF_LIFETIME_DAYS),
            LeafModes::KeepLive,
        )
        .unwrap_err();
        assert!(
            matches!(&error, MaterialError::NotSignedByCa { path, ca }
                if *path == cert && *ca == dir.join(CA_CERT)),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            format!(
                "{}: holds no certificate the CA in {} signed; init --tls self-signed renews only the leaves it wrote",
                cert.display(),
                dir.join(CA_CERT).display()
            )
        );
        assert_eq!(
            (std::fs::read(&cert).unwrap(), std::fs::read(&key).unwrap()),
            before
        );
        assert_eq!(generations(dir), Vec::<PathBuf>::new());
    }

    #[test]
    fn leaves_are_staged_and_checked_together_so_a_failure_on_one_places_none() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let names = leaf_names("192.0.2.7:8317".parse().unwrap());
        let ca = write_ca(
            dir,
            "host.example",
            &names,
            Validity::days_from_now(CA_LIFETIME_DAYS),
        )
        .unwrap();
        let leaves = [("proxy", names.clone()), ("admin", names.clone())];
        let written = write_leaves(
            dir,
            &ca,
            "host.example",
            &leaves,
            Validity::days_from_now(LEAF_LIFETIME_DAYS),
            LeafModes::Private,
        )
        .unwrap();
        assert_eq!(
            written.iter().map(Placed::files).collect::<Vec<_>>(),
            [leaf_files(dir, "proxy"), leaf_files(dir, "admin")]
        );
        // The admin key went missing, so its renewal has no owner and mode to keep; the proxy pair, staged first,
        // stays as it was.
        let (proxy_cert, proxy_key) = leaf_files(dir, "proxy");
        let (admin_cert, admin_key) = leaf_files(dir, "admin");
        std::fs::remove_file(&admin_key).unwrap();
        let live = [proxy_cert, proxy_key, admin_cert];
        let links = || ["proxy", "admin"].map(|name| std::fs::read_link(dir.join(name)).unwrap());
        let (generations_before, links_before) = (generations(dir), links());
        let read = || {
            live.iter()
                .map(|path| std::fs::read(path).unwrap())
                .collect::<Vec<_>>()
        };
        let before = read();
        let unfinished = write_leaves(
            dir,
            &ca,
            "host.example",
            &leaves,
            Validity::days_from_now(LEAF_LIFETIME_DAYS),
            LeafModes::KeepLive,
        )
        .unwrap_err();
        assert!(
            matches!(&unfinished.error, MaterialError::Io { path, .. } if *path == admin_key),
            "{unfinished:?}"
        );
        assert!(unfinished.placed.is_empty(), "{unfinished:?}");
        assert_eq!(read(), before);
        assert_eq!(links(), links_before);
        assert_eq!(generations(dir), generations_before);
    }

    #[test]
    fn a_switch_that_fails_on_a_later_leaf_hands_back_the_leaves_already_live() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let names = leaf_names("192.0.2.7:8317".parse().unwrap());
        let ca = write_ca(
            dir,
            "host.example",
            &names,
            Validity::days_from_now(CA_LIFETIME_DAYS),
        )
        .unwrap();
        // A directory under the admin link's name refuses the link a first write creates there.
        std::fs::create_dir(dir.join("admin")).unwrap();
        let unfinished = write_leaves(
            dir,
            &ca,
            "host.example",
            &[("proxy", names.clone()), ("admin", names.clone())],
            Validity::days_from_now(LEAF_LIFETIME_DAYS),
            LeafModes::Private,
        )
        .unwrap_err();
        assert!(
            matches!(&unfinished.error, MaterialError::Io { path, .. } if *path == dir.join("admin")),
            "{unfinished:?}"
        );
        assert_eq!(
            unfinished
                .placed
                .iter()
                .map(Placed::files)
                .collect::<Vec<_>>(),
            [leaf_files(dir, "proxy")]
        );
        let (cert, key) = leaf_files(dir, "proxy");
        assert!(cert.is_file() && key.is_file());
        // The admin generation that never went live is gone; the proxy's is the one the link names.
        assert_eq!(
            generations(dir),
            [dir.join(std::fs::read_link(dir.join("proxy")).unwrap())]
        );
        assert!(std::fs::read_dir(dir.join("admin"))
            .unwrap()
            .next()
            .is_none());
    }

    #[test]
    fn a_leaf_carrying_the_listen_address_and_the_loopbacks_verifies_under_the_ca_through_boringssl(
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let names = leaf_names("192.0.2.7:8317".parse().unwrap());
        for ip in ["127.0.0.1", "192.0.2.7", "::1"] {
            assert!(
                names.contains(&SanName::Ip(ip.parse().unwrap())),
                "{names:?}"
            );
        }
        let ca = write_ca(
            dir,
            "host.example",
            &names,
            Validity::days_from_now(CA_LIFETIME_DAYS),
        )
        .unwrap();
        // BoringSSL has no matcher for an address subtree and refuses any leaf carrying an address under one, so the
        // constraint names DNS alone; this pins that the leaf `init` writes passes the verifier phoebis's clients use.
        let (cert, _) = write_leaf(
            dir,
            "proxy",
            &ca,
            "host.example",
            &names,
            Validity::days_from_now(LEAF_LIFETIME_DAYS),
            LeafModes::Private,
        )
        .unwrap_or_else(|error| panic!("{error}"));
        let mut store = X509StoreBuilder::new().unwrap();
        store.add_cert(certificate_in(&dir.join(CA_CERT))).unwrap();
        let store = store.build();
        let (verified, result) = X509StoreContext::new()
            .unwrap()
            .init(
                &store,
                &certificate_in(&cert),
                &Stack::<X509>::new().unwrap(),
                |context| Ok((context.verify_cert()?, context.verify_result())),
            )
            .unwrap();
        assert!(verified, "{result:?}");
    }

    #[test]
    fn a_ca_write_refuses_a_name_that_appeared_and_keeps_its_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // A key another run placed after `read_ca` found the directory empty.
        std::fs::write(dir.join(CA_KEY), b"another run's key\n").unwrap();
        let error = write_ca(
            dir,
            "host.example",
            &leaf_names("192.0.2.7:8317".parse().unwrap()),
            Validity::days_from_now(CA_LIFETIME_DAYS),
        )
        .err()
        .unwrap();
        assert!(
            matches!(&error, MaterialError::Io { path, error }
                if *path == dir.join(CA_KEY) && error.kind() == std::io::ErrorKind::AlreadyExists),
            "{error:?}"
        );
        assert_eq!(
            std::fs::read(dir.join(CA_KEY)).unwrap(),
            b"another run's key\n"
        );
        assert!(!dir.join(CA_CERT).exists());
        assert!(temporaries(dir).is_empty(), "{:?}", temporaries(dir));
    }

    #[test]
    fn a_leaf_named_outside_the_cas_constraint_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let ca = write_ca(
            dir,
            "host.example",
            &leaf_names("192.0.2.7:8317".parse().unwrap()),
            Validity::days_from_now(CA_LIFETIME_DAYS),
        )
        .unwrap();
        let error = write_leaf(
            dir,
            "proxy",
            &ca,
            "host.example",
            &[SanName::Dns("other.example".into())],
            Validity::days_from_now(LEAF_LIFETIME_DAYS),
            LeafModes::Private,
        )
        .unwrap_err();
        assert!(
            matches!(&error, MaterialError::OutsideConstraint { names } if names == "other.example"),
            "{error:?}"
        );
        assert!(
            error.to_string().ends_with(
                "remove tls/ca.pem and tls/ca.key for a new CA, which every client must trust again"
            ),
            "{error}"
        );
        assert!(!dir.join("proxy").exists());
        assert_eq!(generations(dir), Vec::<PathBuf>::new());
    }

    #[test]
    fn read_ca_answers_none_for_an_empty_directory_and_refuses_a_half_an_unparseable_a_mismatched_and_an_expired_ca(
    ) {
        let ends_with_new_ca = |error: &MaterialError| {
            assert!(
                error.to_string().ends_with(
                    "remove tls/ca.pem and tls/ca.key for a new CA, which every client must trust again"
                ),
                "{error}"
            );
        };
        let names = leaf_names("192.0.2.7:8317".parse().unwrap());

        let empty = tempfile::tempdir().unwrap();
        assert!(read_ca(empty.path()).unwrap().is_none());

        let half = tempfile::tempdir().unwrap();
        write_ca(
            half.path(),
            "host.example",
            &names,
            Validity::days_from_now(CA_LIFETIME_DAYS),
        )
        .unwrap();
        std::fs::remove_file(half.path().join(CA_KEY)).unwrap();
        let error = read_ca(half.path()).err().unwrap();
        assert!(
            matches!(&error, MaterialError::HalfCa { path, other }
                if *path == half.path().join(CA_CERT) && *other == half.path().join(CA_KEY)),
            "{error:?}"
        );
        ends_with_new_ca(&error);

        let junk = tempfile::tempdir().unwrap();
        write_ca(
            junk.path(),
            "host.example",
            &names,
            Validity::days_from_now(CA_LIFETIME_DAYS),
        )
        .unwrap();
        std::fs::write(junk.path().join(CA_CERT), "not a certificate\n").unwrap();
        let error = read_ca(junk.path()).err().unwrap();
        assert!(
            matches!(&error, MaterialError::NoCertificate { path } if *path == junk.path().join(CA_CERT)),
            "{error:?}"
        );
        ends_with_new_ca(&error);

        let expired = tempfile::tempdir().unwrap();
        let now = jiff::Timestamp::now().as_second();
        write_ca(
            expired.path(),
            "host.example",
            &names,
            Validity {
                not_before: now - 2 * 86_400,
                not_after: now - 86_400,
            },
        )
        .unwrap();
        let error = read_ca(expired.path()).err().unwrap();
        assert!(
            matches!(&error, MaterialError::ExpiredCa { path, date }
                if *path == expired.path().join(CA_CERT) && !date.is_empty()),
            "{error:?}"
        );
        assert!(error.to_string().contains(": expired on "), "{error}");
        ends_with_new_ca(&error);

        let mismatched = tempfile::tempdir().unwrap();
        write_ca(
            mismatched.path(),
            "host.example",
            &names,
            Validity::days_from_now(CA_LIFETIME_DAYS),
        )
        .unwrap();
        let other = new_key().unwrap().private_key_to_pem_pkcs8().unwrap();
        std::fs::write(mismatched.path().join(CA_KEY), other).unwrap();
        let error = read_ca(mismatched.path()).err().unwrap();
        assert!(
            matches!(&error, MaterialError::KeyMismatch { key, cert }
                if *key == mismatched.path().join(CA_KEY) && *cert == mismatched.path().join(CA_CERT)),
            "{error:?}"
        );
        ends_with_new_ca(&error);
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
