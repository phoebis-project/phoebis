//! PKCE verifier and challenge generation (RFC 7636), and OAuth state generation.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use sha2::{Digest, Sha256};

#[derive(Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

// The verifier is the bearer secret PKCE exists to protect; redact it so a stray `{:?}` in a log
// line (including through `PairingSession`'s derived `Debug`) can't leak it.
impl std::fmt::Debug for Pkce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pkce")
            .field("verifier", &"***")
            .field("challenge", &self.challenge)
            .finish()
    }
}

/// 32 random bytes base64url-encode to 43 characters, the minimum RFC 7636 verifier length.
pub fn generate_pkce() -> Pkce {
    pkce_of(URL_SAFE_NO_PAD.encode(random_bytes()))
}

/// The pair for `verifier`: the S256 challenge of RFC 7636.
fn pkce_of(verifier: String) -> Pkce {
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Pkce {
        verifier,
        challenge,
    }
}

pub fn generate_state() -> String {
    URL_SAFE_NO_PAD.encode(random_bytes())
}

fn random_bytes() -> [u8; 32] {
    let mut bytes = [0u8; 32];
    crate::random::fill(&mut bytes);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rfc_7636_appendix_b_verifier_answers_its_s256_challenge() {
        let pkce = pkce_of("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".to_string());
        assert_eq!(
            pkce.challenge,
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}
