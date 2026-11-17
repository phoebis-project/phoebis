//! The feature CA: one certificate authority in the data directory, made by the first start with a `features` block
//! and never replaced by the router, which signs the certificates of the two feature hosts and can sign no other
//! name. Each client of the router trusts it by the fingerprint its operator hands out, so a CA made anew at a start
//! would end each of those clients' trust.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use boring::error::ErrorStack;
use boring::x509::{X509Ref, X509};
use phoebis_store::permissions::create_private_file;
use phoebis_store::Clock;
use rcgen::{
    date_time_ymd, BasicConstraints, Certificate, CertificateParams, CidrSubnet, DistinguishedName,
    DnType, ExtendedKeyUsagePurpose, GeneralSubtree, IsCa, KeyPair, KeyUsagePurpose,
    NameConstraints,
};

use crate::config::FeaturesConfig;
use crate::tls::{read_checked, TlsError};

pub const CA_CERT_FILE: &str = "feature-ca.pem";
pub const CA_KEY_FILE: &str = "feature-ca-key.pem";
/// The names the CA may sign for, each with the names beneath it, and the names the listener answers to.
pub const FEATURE_HOSTS: [&str; 2] = ["api.anthropic.com", "mcp-proxy.anthropic.com"];
pub const CA_VALIDITY: Duration = Duration::from_secs(3_650 * 86_400);
pub const LEAF_VALIDITY: Duration = Duration::from_secs(7 * 86_400);
/// How far ahead of a certificate's end the next one is issued.
pub const LEAF_RENEW_AHEAD: Duration = Duration::from_secs(86_400);
/// How far ahead of its issue a certificate's validity starts, for a client whose clock runs behind.
pub const BACKDATE: Duration = Duration::from_secs(300);

/// The prefix of each refusal, so the CA's lines read like a listener pair's under its own key.
const KEY: &str = "features.ca";

/// The CA's subject. Each certificate names it as its issuer, so the issuer rebuilt at a later start repeats it.
const CA_NAME: &str = "Phoebis feature CA";

#[derive(Debug, thiserror::Error)]
pub enum FeatureCaError {
    #[error("features.ca.cert_file {path}: holds no CA this router made")]
    Foreign { path: PathBuf },
    #[error("features.ca: {present} stands without {missing}: restore the missing file, or remove both for a new CA")]
    Half { missing: PathBuf, present: PathBuf },
    #[error("features.ca: {0}")]
    Issue(String),
    #[error("features.ca.key_file {path}: holds a key that cannot sign: {error}")]
    KeyCannotSign { path: PathBuf, error: rcgen::Error },
    #[error(transparent)]
    Pair(#[from] crate::tls::TlsError),
    #[error("features.ca {path}: {error}")]
    Write {
        path: PathBuf,
        error: std::io::Error,
    },
}

/// What the start, `check-config`, the settings answer and the attention reader read of the CA.
#[derive(Clone, Debug)]
pub struct CaFacts {
    pub cert_file: PathBuf,
    /// 64 lowercase hex characters: the SHA-256 of `pem`, the bytes the download serves.
    pub fingerprint: String,
    pub key_file: PathBuf,
    pub leaf: crate::tls::LeafFacts,
    /// The certificate file's bytes.
    pub pem: String,
}

/// The CA's facts beside the signing key and the issuer each certificate is signed under.
pub struct FeatureCa {
    facts: CaFacts,
    issuer: Certificate,
    key: KeyPair,
}

impl FeatureCa {
    /// The CA of `data_dir`: read where both files stand, created where neither does, refused where one does.
    /// The flag says whether this call created it.
    pub fn open(data_dir: &Path, now: i64) -> Result<(FeatureCa, bool), FeatureCaError> {
        let (cert_file, key_file) = files(data_dir);
        let created = !both_stand(&cert_file, &key_file)?;
        if created {
            create(&cert_file, &key_file, now)?;
        }
        Ok((FeatureCa::read(cert_file, key_file)?, created))
    }

    /// The facts of the CA on disk, `None` where neither file stands. Creates nothing.
    pub fn inspect(data_dir: &Path) -> Result<Option<CaFacts>, FeatureCaError> {
        let (cert_file, key_file) = files(data_dir);
        if !both_stand(&cert_file, &key_file)? {
            return Ok(None);
        }
        FeatureCa::read(cert_file, key_file).map(|ca| Some(ca.facts))
    }

    pub fn facts(&self) -> &CaFacts {
        &self.facts
    }

    /// One certificate for `host` under the CA with a key of its own, valid until `now` plus `LEAF_VALIDITY`.
    pub fn issue(&self, host: &str, now: i64) -> Result<IssuedLeaf, FeatureCaError> {
        let at = now.div_euclid(1_000);
        let not_after = at + seconds(LEAF_VALIDITY);
        let mut params = CertificateParams::new(vec![host.to_owned()]).map_err(issue_error)?;
        params.distinguished_name = named(host);
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        // Lets a client that trusts an earlier CA under the same subject tell the two apart.
        params.use_authority_key_identifier_extension = true;
        valid(&mut params, at, not_after);
        let key = KeyPair::generate().map_err(issue_error)?;
        let cert = params
            .signed_by(&key, &self.issuer, &self.key)
            .map_err(issue_error)?;
        Ok(IssuedLeaf {
            cert_der: cert.der().to_vec(),
            key_der: key.serialize_der(),
            not_after: not_after * 1_000,
        })
    }

    /// Both files through the reader a listener's pair takes. The issuer rcgen signs under is rebuilt from the
    /// module's own parameters and the stored key, since rcgen's default build parses no stored certificate.
    fn read(cert_file: PathBuf, key_file: PathBuf) -> Result<FeatureCa, FeatureCaError> {
        let pair = read_checked(KEY, &cert_file, &key_file)?;
        let key = KeyPair::from_pem(&String::from_utf8_lossy(&pair.key_pem)).map_err(|error| {
            FeatureCaError::KeyCannotSign {
                path: key_file.clone(),
                error,
            }
        })?;
        let issuer = ca_params().self_signed(&key).map_err(issue_error)?;
        // Each certificate signed here names the rebuilt issuer's subject, so under a CA of another subject a client
        // builds no chain and each handshake fails with no line on the router.
        let rebuilt = X509::from_der(issuer.der()).map_err(stack)?;
        if subject(&pair.leaf)? != subject(&rebuilt)? {
            return Err(FeatureCaError::Foreign { path: cert_file });
        }
        // Read once into text, so the fingerprint is taken over the very bytes the download serves.
        let pem = String::from_utf8_lossy(&pair.cert_pem).into_owned();
        Ok(FeatureCa {
            facts: CaFacts {
                cert_file,
                fingerprint: hex(&boring::sha::sha256(pem.as_bytes())),
                key_file,
                leaf: pair.facts,
                pem,
            },
            issuer,
            key,
        })
    }
}

pub struct IssuedLeaf {
    pub cert_der: Vec<u8>,
    /// PKCS#8. It lives in memory alone.
    pub key_der: Vec<u8>,
    /// Unix milliseconds.
    pub not_after: i64,
}

/// The certificates the feature listener serves, one per feature host.
pub struct FeatureLeaves {
    ca: Arc<FeatureCa>,
    clock: Clock,
    /// One certificate per name of `FEATURE_HOSTS`, in its order.
    current: Mutex<Vec<Arc<IssuedLeaf>>>,
}

impl FeatureLeaves {
    pub fn issue(ca: Arc<FeatureCa>, clock: Clock) -> Result<FeatureLeaves, FeatureCaError> {
        let now = clock();
        let current = FEATURE_HOSTS
            .iter()
            .map(|host| ca.issue(host, now).map(Arc::new))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(FeatureLeaves {
            ca,
            clock,
            current: Mutex::new(current),
        })
    }

    /// The certificate a handshake that names `host` gets now, `None` for each other name.
    pub fn current(&self, host: &str) -> Option<Arc<IssuedLeaf>> {
        let name = host.strip_suffix('.').unwrap_or(host);
        let at = FEATURE_HOSTS
            .iter()
            .position(|feature| feature.eq_ignore_ascii_case(name))?;
        Some(Arc::clone(&self.leaves()[at]))
    }

    /// The issuing step: replaces each certificate that ends within `LEAF_RENEW_AHEAD` and answers how many.
    pub fn renew_due(&self) -> Result<usize, FeatureCaError> {
        let now = (self.clock)();
        let ahead = millis(LEAF_RENEW_AHEAD);
        let due: Vec<usize> = self
            .leaves()
            .iter()
            .enumerate()
            .filter(|(_, leaf)| leaf.not_after - ahead <= now)
            .map(|(at, _)| at)
            .collect();
        // Issued outside the lock, so a handshake that reads the current certificate never waits on a signature.
        for &at in &due {
            let leaf = Arc::new(self.ca.issue(FEATURE_HOSTS[at], now)?);
            self.leaves()[at] = leaf;
        }
        Ok(due.len())
    }

    fn leaves(&self) -> MutexGuard<'_, Vec<Arc<IssuedLeaf>>> {
        self.current.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The `check-config` line: `features listener is off`, `features listens on {addr} over TLS (CA {path}, fingerprint
/// {hex}, expires {date})`, or `features listens on {addr} over TLS, and the first start creates the CA at {path}`.
pub fn describe(
    features: Option<&FeaturesConfig>,
    data_dir: &Path,
    facts: Option<&CaFacts>,
) -> String {
    let Some(FeaturesConfig { listen }) = features else {
        return "features listener is off".to_owned();
    };
    match facts {
        Some(facts) => format!(
            "features listens on {listen} over TLS (CA {}, fingerprint {}, expires {})",
            facts.cert_file.display(),
            facts.fingerprint,
            facts.leaf.not_after
        ),
        None => format!(
            "features listens on {listen} over TLS, and the first start creates the CA at {}",
            files(data_dir).0.display()
        ),
    }
}

fn files(data_dir: &Path) -> (PathBuf, PathBuf) {
    (data_dir.join(CA_CERT_FILE), data_dir.join(CA_KEY_FILE))
}

/// Whether both files stand, `false` where neither does, and the refusal where one stands without the other: a new
/// CA written over half of an old one would change the fingerprint without a word.
fn both_stand(cert_file: &Path, key_file: &Path) -> Result<bool, FeatureCaError> {
    match (stands(cert_file), stands(key_file)) {
        (true, true) => Ok(true),
        (false, false) => Ok(false),
        (true, false) => Err(FeatureCaError::Half {
            missing: key_file.to_path_buf(),
            present: cert_file.to_path_buf(),
        }),
        (false, true) => Err(FeatureCaError::Half {
            missing: cert_file.to_path_buf(),
            present: key_file.to_path_buf(),
        }),
    }
}

/// A path stands unless it is absent, so a file that cannot be read for another cause reaches the read, which names
/// the cause.
fn stands(path: &Path) -> bool {
    !matches!(std::fs::symlink_metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
}

/// The key first, so a write that stops between the two leaves no certificate standing without the key it names.
fn create(cert_file: &Path, key_file: &Path, now: i64) -> Result<(), FeatureCaError> {
    let at = now.div_euclid(1_000);
    let key = KeyPair::generate().map_err(issue_error)?;
    let mut params = ca_params();
    valid(&mut params, at, at + seconds(CA_VALIDITY));
    let cert = params.self_signed(&key).map_err(issue_error)?;
    write_private(key_file, key.serialize_pem().as_bytes())?;
    write_private(cert_file, cert.pem().as_bytes())
}

/// The CA's parameters but its dates: a path length of zero, so no intermediate stands between it and a leaf, and a
/// critical name constraint in two parts. The DNS subtrees bind DNS names alone, so without the excluded address
/// ranges a certificate whose name is an IP address would pass under the CA.
fn ca_params() -> CertificateParams {
    let mut params = CertificateParams::default();
    params.distinguished_name = named(CA_NAME);
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    params.name_constraints = Some(NameConstraints {
        permitted_subtrees: FEATURE_HOSTS
            .iter()
            .map(|host| GeneralSubtree::DnsName((*host).to_owned()))
            .collect(),
        excluded_subtrees: vec![
            GeneralSubtree::IpAddress(CidrSubnet::V4([0; 4], [0; 4])),
            GeneralSubtree::IpAddress(CidrSubnet::V6([0; 16], [0; 16])),
        ],
    });
    params
}

fn named(common_name: &str) -> DistinguishedName {
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, common_name);
    name
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), FeatureCaError> {
    create_private_file(path, 0o600)
        .and_then(|mut file| file.write_all(bytes).and_then(|()| file.sync_all()))
        .map_err(|error| FeatureCaError::Write {
            path: path.to_path_buf(),
            error,
        })
}

/// The validity from `issued` less `BACKDATE` until `until`, both Unix seconds. rcgen's dates are `time`'s, which
/// this crate does not name: the epoch from rcgen's own constructor, moved by a std duration, is the same instant.
fn valid(params: &mut CertificateParams, issued: i64, until: i64) {
    let epoch = date_time_ymd(1970, 1, 1);
    let since_epoch =
        |unix_seconds: i64| Duration::from_secs(u64::try_from(unix_seconds).unwrap_or(0));
    params.not_before = epoch + since_epoch(issued - seconds(BACKDATE));
    params.not_after = epoch + since_epoch(until);
}

fn seconds(duration: Duration) -> i64 {
    i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

fn issue_error(error: rcgen::Error) -> FeatureCaError {
    FeatureCaError::Issue(error.to_string())
}

fn stack(error: ErrorStack) -> FeatureCaError {
    FeatureCaError::Pair(TlsError::Stack { key: KEY, error })
}

/// The subject as its encoding, the form a leaf's issuer field repeats.
fn subject(cert: &X509Ref) -> Result<Vec<u8>, FeatureCaError> {
    cert.subject_name().to_der().map_err(stack)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
