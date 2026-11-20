//! The feature CA: one certificate authority in the data directory, made by the first start with a `features` block
//! and never replaced by the router, which signs the certificates of the two feature hosts and can sign no other
//! name. Each client of the router trusts it by the fingerprint its operator hands out, so a CA made anew at a start
//! would end each of those clients' trust.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use boring::error::ErrorStack;
use boring::x509::{X509Ref, X509};
use phoebis_store::permissions::{check_private_file, create_private_file};
use phoebis_store::Clock;
use rcgen::{
    date_time_ymd, BasicConstraints, Certificate, CertificateParams, CidrSubnet, DistinguishedName,
    DnType, DnValue, ExtendedKeyUsagePurpose, GeneralSubtree, IsCa, KeyIdMethod, KeyPair,
    KeyUsagePurpose, NameConstraints,
};

use crate::config::FeaturesConfig;
use crate::tls::{read_checked, TlsError};

const CA_CERT_FILE: &str = "feature-ca.pem";
const CA_KEY_FILE: &str = "feature-ca-key.pem";
/// The names the CA may sign for, each with the names beneath it, and the names the listener answers to.
pub(crate) const FEATURE_HOSTS: [&str; 2] = ["api.anthropic.com", "mcp-proxy.anthropic.com"];
const CA_VALIDITY: Duration = Duration::from_secs(3_650 * 86_400);
const LEAF_VALIDITY: Duration = Duration::from_secs(7 * 86_400);
/// How far ahead of a certificate's end the next one is issued.
const LEAF_RENEW_AHEAD: Duration = Duration::from_secs(86_400);
/// How far ahead of its issue a certificate's validity starts, for a client whose clock runs behind.
const BACKDATE: Duration = Duration::from_secs(300);

/// The prefix of each refusal, so the CA's lines read like a listener pair's under its own key.
const KEY: &str = phoebis_attention::FEATURE_CA;

/// The CA's subject. Each certificate names it as its issuer, so the issuer rebuilt at a later start repeats it.
const CA_NAME: &str = "Phoebis feature CA";

/// The name constraint's id, 2.5.29.30, as the contents of its DER element.
const NAME_CONSTRAINTS: &[u8] = &[0x55, 0x1d, 0x1e];

#[derive(Debug, thiserror::Error)]
pub enum FeatureCaError {
    #[error("features.ca.cert_file {path}: holds more than one certificate")]
    Chain { path: PathBuf },
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
    #[error(
        "features.ca.cert_file {path}: holds a CA without the name constraint this router writes"
    )]
    Unconstrained { path: PathBuf },
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
    pub(crate) fn open(data_dir: &Path, now: i64) -> Result<(FeatureCa, bool), FeatureCaError> {
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
    fn issue(&self, host: &str, now: i64) -> Result<IssuedLeaf, FeatureCaError> {
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

    /// Both files through the reader a listener's pair takes, then the stored certificate's own checks: it stands
    /// alone in its file, and it carries a key identifier, the subject and the name constraint `ca_params` writes.
    /// The issuer rcgen signs under is rebuilt from those parameters, the stored key and the stored key identifier,
    /// since rcgen's default build parses no stored certificate.
    fn read(cert_file: PathBuf, key_file: PathBuf) -> Result<FeatureCa, FeatureCaError> {
        let pair = read_checked(KEY, &cert_file, &key_file)?;
        // The download serves the file whole and each launcher trusts what it serves, so a certificate behind the
        // first would join that trust unchecked.
        if !pair.chain.is_empty() {
            return Err(FeatureCaError::Chain { path: cert_file });
        }
        let key = KeyPair::from_pem(&String::from_utf8_lossy(&pair.key_pem)).map_err(|error| {
            FeatureCaError::KeyCannotSign {
                path: key_file.clone(),
                error,
            }
        })?;
        // Each leaf repeats the CA's key identifier as its authority key identifier, which a client chains by, and
        // each CA the router makes carries one.
        let Some(key_identifier) = pair.leaf.subject_key_id() else {
            return Err(FeatureCaError::Foreign { path: cert_file });
        };
        let params = ca_params(KeyIdMethod::PreSpecified(
            key_identifier.as_slice().to_vec(),
        ));
        let issuer = params.self_signed(&key).map_err(issue_error)?;
        // Each certificate signed here names the rebuilt issuer's subject, so under a CA of another subject a client
        // builds no chain and each handshake fails with no line on the router.
        let rebuilt = X509::from_der(issuer.der()).map_err(stack)?;
        if subject(&pair.leaf)? != subject(&rebuilt)? {
            return Err(FeatureCaError::Foreign { path: cert_file });
        }
        // A launcher is told that the file binds its trust to the two feature hosts, which this constraint makes so.
        let der = pair.leaf.to_der().map_err(stack)?;
        let constrained = match (
            extension(&der, NAME_CONSTRAINTS),
            extension(issuer.der(), NAME_CONSTRAINTS),
        ) {
            (Some(stored), Some(written)) => stored.0 && stored == written,
            _ => false,
        };
        if !constrained {
            return Err(FeatureCaError::Unconstrained { path: cert_file });
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

pub(crate) struct IssuedLeaf {
    pub(crate) cert_der: Vec<u8>,
    /// PKCS#8. It lives in memory alone.
    pub(crate) key_der: Vec<u8>,
    /// Unix milliseconds.
    pub(crate) not_after: i64,
}

/// The certificates the feature listener serves, one per feature host.
pub(crate) struct FeatureLeaves {
    ca: Arc<FeatureCa>,
    clock: Clock,
    /// One certificate per name of `FEATURE_HOSTS`, in its order.
    current: Vec<IssuedLeaf>,
}

impl FeatureLeaves {
    pub(crate) fn issue(ca: Arc<FeatureCa>, clock: Clock) -> Result<FeatureLeaves, FeatureCaError> {
        let now = clock();
        let current = FEATURE_HOSTS
            .iter()
            .map(|host| ca.issue(host, now))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(FeatureLeaves { ca, clock, current })
    }

    /// The current certificates, one per name of `FEATURE_HOSTS`, in its order.
    pub(crate) fn leaves(&self) -> &[IssuedLeaf] {
        &self.current
    }

    /// The issuing step: replaces each certificate that ends within `LEAF_RENEW_AHEAD`.
    pub(crate) fn renew_due(&mut self) -> Result<(), FeatureCaError> {
        let now = (self.clock)();
        let ahead = millis(LEAF_RENEW_AHEAD);
        let due: Vec<usize> = self
            .current
            .iter()
            .enumerate()
            .filter(|(_, leaf)| leaf.not_after - ahead <= now)
            .map(|(at, _)| at)
            .collect();
        for &at in &due {
            self.current[at] = self.ca.issue(FEATURE_HOSTS[at], now)?;
        }
        Ok(())
    }
}

/// The `check-config` line: `features listener is off`, `features listens on {addr} over TLS (CA {path}, fingerprint
/// {hex}, expires {date})`, or `features listens on {addr} over TLS, and the first start creates the CA at {path}`.
pub fn describe(features: Option<(&FeaturesConfig, Option<&CaFacts>)>, data_dir: &Path) -> String {
    let Some((FeaturesConfig { listen }, facts)) = features else {
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

/// The CA's `warning:` lines for the start and `check-config`, without the `warning: ` prefix: `expiry_line`'s under
/// `features.ca`, the key its refusals and its attention item name it by, then the key file's mode.
pub fn warnings(ca: &CaFacts) -> Vec<String> {
    crate::tls::expiry_line(KEY, &ca.cert_file, &ca.leaf)
        .into_iter()
        .chain(check_private_file(&ca.key_file, 0o600))
        .collect()
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
    crate::private_dirs::exists(path).unwrap_or(true)
}

/// The key first, so a write that stops between the two leaves no certificate standing without the key it names.
fn create(cert_file: &Path, key_file: &Path, now: i64) -> Result<(), FeatureCaError> {
    let at = now.div_euclid(1_000);
    let key = KeyPair::generate().map_err(issue_error)?;
    let mut params = ca_params(KeyIdMethod::Sha256);
    valid(&mut params, at, at + seconds(CA_VALIDITY));
    let cert = params.self_signed(&key).map_err(issue_error)?;
    write_private(key_file, key.serialize_pem().as_bytes())?;
    write_private(cert_file, cert.pem().as_bytes())
}

/// The CA's parameters but its dates: a path length of zero, so no intermediate stands between it and a leaf, and a
/// critical name constraint in two parts. The DNS subtrees bind DNS names alone, so without the excluded address
/// ranges a certificate whose name is an IP address would pass under the CA. `key_identifier` gives the CA's key
/// identifier, which each leaf repeats: the library's derivation for a new CA, and the stored identifier for a CA
/// read back, so no change of the library's default moves the identifier a client chains a leaf by.
fn ca_params(key_identifier: KeyIdMethod) -> CertificateParams {
    let mut params = CertificateParams::default();
    params.distinguished_name = named(CA_NAME);
    params.key_identifier_method = key_identifier;
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

/// The string type is written out, so the bytes a stored CA's subject is compared with hang on no default of the
/// library.
fn named(common_name: &str) -> DistinguishedName {
    let mut name = DistinguishedName::new();
    name.push(
        DnType::CommonName,
        DnValue::Utf8String(common_name.to_owned()),
    );
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

/// The critical flag and the value of the extension `oid` names in the certificate `der`, `None` for a certificate
/// without it or with a length the walk cannot follow. BoringSSL's binding offers no read of the name constraint, so
/// the walk reads the certificate's encoding. A name constraint with the same subtrees in the same order has one DER
/// form, so a compare of the value hangs on no default of the library that wrote it.
fn extension<'a>(der: &'a [u8], oid: &[u8]) -> Option<(bool, &'a [u8])> {
    const BOOLEAN: u8 = 0x01;
    const OCTET_STRING: u8 = 0x04;
    const OBJECT_IDENTIFIER: u8 = 0x06;
    const SEQUENCE: u8 = 0x30;
    /// The context tag 3 of the certificate's body, which holds its extensions.
    const EXTENSIONS: u8 = 0xa3;

    /// The element at the head of `der`: its tag, its contents and the bytes behind it.
    fn element(der: &[u8]) -> Option<(u8, &[u8], &[u8])> {
        let (&tag, rest) = der.split_first()?;
        let (&first, rest) = rest.split_first()?;
        let (length, rest) = match first {
            0..=0x7f => (usize::from(first), rest),
            0x81..=0x84 => {
                let (bytes, rest) = rest.split_at_checked(usize::from(first & 0x7f))?;
                let length = bytes
                    .iter()
                    .fold(0, |length, &byte| (length << 8) | usize::from(byte));
                (length, rest)
            }
            _ => return None,
        };
        let (contents, rest) = rest.split_at_checked(length)?;
        Some((tag, contents, rest))
    }

    fn tagged(der: &[u8], tag: u8) -> Option<(&[u8], &[u8])> {
        element(der)
            .filter(|(found, _, _)| *found == tag)
            .map(|(_, contents, rest)| (contents, rest))
    }

    let (certificate, _) = tagged(der, SEQUENCE)?;
    let (mut fields, _) = tagged(certificate, SEQUENCE)?;
    let extensions = loop {
        let (tag, contents, rest) = element(fields)?;
        if tag == EXTENSIONS {
            break contents;
        }
        fields = rest;
    };
    let (mut entries, _) = tagged(extensions, SEQUENCE)?;
    while !entries.is_empty() {
        let (entry, rest) = tagged(entries, SEQUENCE)?;
        entries = rest;
        let (id, entry) = tagged(entry, OBJECT_IDENTIFIER)?;
        if id != oid {
            continue;
        }
        // DER leaves the flag out where it is false.
        let (critical, entry) = match tagged(entry, BOOLEAN) {
            Some((flag, entry)) => (flag == [0xff], entry),
            None => (false, entry),
        };
        return tagged(entry, OCTET_STRING).map(|(value, _)| (critical, value));
    }
    None
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cmp::Ordering;
    use std::sync::atomic::{AtomicI64, Ordering as AtomicOrdering};

    use boring::asn1::Asn1Time;

    #[test]
    fn a_feature_certificate_is_issued_again_before_it_ends() {
        let dir = tempfile::tempdir().unwrap();
        // The CA's own dates are checked against the wall clock as it is read, so the test's clock starts there.
        let now = jiff::Timestamp::now().as_millisecond();
        let (ca, _) = FeatureCa::open(dir.path(), now).unwrap();
        let at = Arc::new(AtomicI64::new(now));
        let clock: phoebis_store::Clock = {
            let at = Arc::clone(&at);
            Arc::new(move || at.load(AtomicOrdering::SeqCst))
        };
        let mut leaves = FeatureLeaves::issue(Arc::new(ca), clock).unwrap();
        let end = |leaves: &FeatureLeaves| leaves.leaves()[0].not_after;
        let first = end(&leaves);

        leaves.renew_due().unwrap();
        assert_eq!(end(&leaves), first);

        at.store(first - 1, AtomicOrdering::SeqCst);
        leaves.renew_due().unwrap();
        assert!(end(&leaves) > first, "{} against {first}", end(&leaves));
    }

    #[test]
    fn an_issued_certificate_holds_for_a_client_whose_clock_runs_a_minute_behind() {
        let dir = tempfile::tempdir().unwrap();
        let now = jiff::Timestamp::now().as_millisecond();
        let (ca, _) = FeatureCa::open(dir.path(), now).unwrap();
        let leaf = ca.issue("api.anthropic.com", now).unwrap();
        let cert = X509::from_der(&leaf.cert_der).unwrap();
        let behind = Asn1Time::from_unix(now / 1_000 - 60).unwrap();
        assert_ne!(
            cert.not_before().compare(&behind).unwrap(),
            Ordering::Greater,
            "valid from {}",
            cert.not_before()
        );
    }

    #[test]
    fn a_leaf_names_the_stored_cas_key_identifier() {
        let dir = tempfile::tempdir().unwrap();
        let now = jiff::Timestamp::now().as_millisecond();
        let at = now / 1_000;
        let key = KeyPair::generate().unwrap();
        // An identifier the library would derive from no key, so a leaf that names it took it from the stored CA.
        let mut params = ca_params(KeyIdMethod::PreSpecified(vec![0x11; 20]));
        valid(&mut params, at, at + seconds(CA_VALIDITY));
        let (cert_file, key_file) = files(dir.path());
        std::fs::write(&cert_file, params.self_signed(&key).unwrap().pem()).unwrap();
        std::fs::write(&key_file, key.serialize_pem()).unwrap();

        let ca = FeatureCa::read(cert_file, key_file).unwrap();
        let leaf = X509::from_der(&ca.issue("api.anthropic.com", now).unwrap().cert_der).unwrap();
        assert_eq!(
            leaf.authority_key_id().map(|id| id.as_slice().to_vec()),
            Some(vec![0x11; 20])
        );
    }
}
