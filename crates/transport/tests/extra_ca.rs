//! `upstream.extra_ca_pem` as a bundle: every PEM block of the text a trust anchor, and a text with no block refused.

use boring::x509::X509;
use phoebis_profile::claude_code_2_1_310_linux_x64;
use phoebis_transport::{connector_builder, trust_extra_ca, Plane, TransportError};

#[test]
fn a_bundle_of_two_anchors_trusts_both() {
    let first = fake_upstream::generate_pair(&["localhost"]).ca_pem;
    let second = fake_upstream::generate_pair(&["localhost"]).ca_pem;
    let mut builder = connector_builder(claude_code_2_1_310_linux_x64(), Plane::Inference).unwrap();
    trust_extra_ca(&mut builder, &format!("{first}\n{second}")).unwrap();
    let connector = builder.build();
    // The generated CAs share one subject name and their leaves name no key identifier, so a handshake finds the first
    // CA by name for either leaf: the store's own list says whether each block landed. The read is deprecated for a
    // store another thread may change, and nothing else holds this one.
    #[allow(deprecated)]
    let trusted: Vec<Vec<u8>> = connector
        .context()
        .cert_store()
        .objects()
        .iter()
        .filter_map(|object| object.x509())
        .map(|cert| cert.to_der().unwrap())
        .collect();
    for (which, pem) in [("first", &first), ("second", &second)] {
        let der = X509::from_pem(pem.as_bytes()).unwrap().to_der().unwrap();
        assert!(
            trusted.contains(&der),
            "the {which} anchor of the bundle is not in the store"
        );
    }
}

#[test]
fn a_file_with_no_pem_block_is_refused() {
    for text in ["", "not a certificate\n"] {
        let mut builder =
            connector_builder(claude_code_2_1_310_linux_x64(), Plane::Inference).unwrap();
        let error = trust_extra_ca(&mut builder, text).unwrap_err();
        assert!(
            error.to_string().contains("holds no PEM block"),
            "{text:?}: {error}"
        );
        assert!(
            matches!(&error, TransportError::Io(io) if io.kind() == std::io::ErrorKind::InvalidData),
            "{text:?}: {error:?}"
        );
    }
}
