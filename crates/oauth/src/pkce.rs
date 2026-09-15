//! PKCE verifier and challenge generation (RFC 7636), and OAuth state generation.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::Rng;
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
    let verifier = URL_SAFE_NO_PAD.encode(random_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Pkce {
        verifier,
        challenge,
    }
}

pub fn generate_state() -> String {
    URL_SAFE_NO_PAD.encode(random_bytes())
}

// rand 0.10 dropped `rand::rngs::OsRng`; `rand::rng()` is the thread-local CSPRNG that replaces it.
fn random_bytes() -> [u8; 32] {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    bytes
}
