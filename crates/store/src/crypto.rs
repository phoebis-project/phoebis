//! AES-256-GCM sealing for credential bundles and API-key secrets.
//!
//! Associated data binds a sealed blob to the row it belongs to: `seal`/`open` take the same
//! `format!("{account_id}:{generation}")` or `format!("{account_id}:api_key")` string the caller used, so a
//! sealed blob copied into another account's row, or replayed against a stale generation, fails to open.

use aes_gcm::aead::{Aead, Nonce, Payload};
use aes_gcm::{Aes256Gcm, KeyInit};
use rand::Rng;

use crate::{StorageKey, StoreError};

const NONCE_LEN: usize = 12;

pub fn seal(
    key: &StorageKey,
    aad: &str,
    plaintext: &[u8],
) -> Result<(Vec<u8>, Vec<u8>), StoreError> {
    // StorageKey is always exactly 32 bytes, matching Aes256Gcm's key size, so this cannot fail.
    let cipher = Aes256Gcm::new_from_slice(&key.0).expect("storage key is 32 bytes");
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rng().fill_bytes(&mut nonce_bytes);
    let nonce: Nonce<Aes256Gcm> = nonce_bytes.into();
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| StoreError::Crypto)?;
    Ok((nonce_bytes.to_vec(), ciphertext))
}

pub fn open(
    key: &StorageKey,
    aad: &str,
    nonce: &[u8],
    sealed: &[u8],
) -> Result<Vec<u8>, StoreError> {
    let cipher = Aes256Gcm::new_from_slice(&key.0).expect("storage key is 32 bytes");
    let nonce = Nonce::<Aes256Gcm>::try_from(nonce).map_err(|_| StoreError::Crypto)?;
    cipher
        .decrypt(
            &nonce,
            Payload {
                msg: sealed,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| StoreError::Crypto)
}
