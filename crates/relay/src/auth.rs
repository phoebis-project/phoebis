//! The router's own API keys: the routes an operator configured, and the lookup that turns a presented key
//! into the account it is bound to.

use std::collections::HashMap;

use axum::http::{header, HeaderMap};
use phoebis_store::Secret;
use hmac::{Hmac, Mac};
use rand::Rng;
use sha2::Sha256;
use subtle::ConstantTimeEq;

/// One downstream client's key and the account it may spend.
#[derive(Debug)]
pub struct Route {
    pub key_id: String,
    pub secret: Secret,
    pub account_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum KeyTableError {
    #[error("route {key_id} reuses another route's secret")]
    DuplicateSecret { key_id: String },
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("more than one credential header was presented")]
    Ambiguous,
    #[error("the credential header is malformed")]
    Malformed,
    #[error("no credential header was presented")]
    Missing,
    #[error("the presented key is not in the table")]
    Unknown,
}

/// Presented keys resolved to routes.
///
/// The lookup key is a keyed digest rather than the secret itself: keying it with a value generated at startup
/// means the map's keys cannot be precomputed from a guessed secret, and it keeps the raw key out of the hash
/// map's own machinery. The digest only narrows the search; the constant-time compare below decides.
pub struct KeyTable {
    digest_key: [u8; 32],
    routes: HashMap<[u8; 32], Route>,
}

impl KeyTable {
    pub fn from_routes(routes: &[Route]) -> Result<KeyTable, KeyTableError> {
        let mut digest_key = [0u8; 32];
        rand::rng().fill_bytes(&mut digest_key);
        let mut table = KeyTable {
            digest_key,
            routes: HashMap::with_capacity(routes.len()),
        };
        for route in routes {
            let digest = table.digest(route.secret.expose().as_bytes());
            // Two routes sharing a secret would collapse into one entry and the last one would silently take
            // over the other's account binding, so the table refuses to exist rather than mis-bill an account.
            if table.routes.contains_key(&digest) {
                return Err(KeyTableError::DuplicateSecret {
                    key_id: route.key_id.clone(),
                });
            }
            table.routes.insert(
                digest,
                Route {
                    key_id: route.key_id.clone(),
                    secret: Secret::new(route.secret.expose()),
                    account_id: route.account_id.clone(),
                },
            );
        }
        Ok(table)
    }

    pub fn authenticate(&self, headers: &HeaderMap) -> Result<&Route, AuthError> {
        let bearers = headers.get_all(header::AUTHORIZATION).iter().count();
        let api_keys = headers.get_all("x-api-key").iter().count();
        let presented = match (bearers, api_keys) {
            (0, 0) => return Err(AuthError::Missing),
            (1, 0) => bearer_token(
                headers
                    .get(header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or(AuthError::Malformed)?,
            )?,
            (0, 1) => headers
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
                .ok_or(AuthError::Malformed)?
                .trim(),
            // Two credentials, or the same one twice, leave the relay guessing which the caller meant.
            _ => return Err(AuthError::Ambiguous),
        };
        if presented.is_empty() {
            return Err(AuthError::Malformed);
        }
        let route = self
            .routes
            .get(&self.digest(presented.as_bytes()))
            .ok_or(AuthError::Unknown)?;
        if bool::from(route.secret.expose().as_bytes().ct_eq(presented.as_bytes())) {
            Ok(route)
        } else {
            Err(AuthError::Unknown)
        }
    }

    fn digest(&self, secret: &[u8]) -> [u8; 32] {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.digest_key)
            .expect("HMAC-SHA256 accepts a 32 byte key");
        mac.update(secret);
        mac.finalize().into_bytes().into()
    }
}

fn bearer_token(raw: &str) -> Result<&str, AuthError> {
    let (scheme, token) = raw.split_once(' ').ok_or(AuthError::Malformed)?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err(AuthError::Malformed);
    }
    Ok(token.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> KeyTable {
        KeyTable::from_routes(&[Route {
            key_id: "desktop".into(),
            secret: Secret::new("phoebis-testkey"),
            account_id: "acct".into(),
        }])
        .expect("a single route builds")
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                axum::http::HeaderName::from_bytes(name.as_bytes()).expect("test header name"),
                value.parse().expect("test header value"),
            );
        }
        map
    }

    #[test]
    fn both_shapes_of_the_same_key_resolve_to_the_route() {
        let table = table();
        assert_eq!(
            table
                .authenticate(&headers(&[("authorization", "Bearer phoebis-testkey")]))
                .expect("bearer resolves")
                .key_id,
            "desktop"
        );
        assert_eq!(
            table
                .authenticate(&headers(&[("x-api-key", "phoebis-testkey")]))
                .expect("x-api-key resolves")
                .account_id,
            "acct"
        );
    }

    #[test]
    fn repeats_and_wrong_schemes_are_rejected() {
        let table = table();
        assert!(matches!(
            table.authenticate(&headers(&[
                ("authorization", "Bearer phoebis-testkey"),
                ("authorization", "Bearer phoebis-testkey"),
            ])),
            Err(AuthError::Ambiguous)
        ));
        assert!(matches!(
            table.authenticate(&headers(&[("authorization", "Basic phoebis-testkey")])),
            Err(AuthError::Malformed)
        ));
        assert!(matches!(
            table.authenticate(&headers(&[("authorization", "Bearer   ")])),
            Err(AuthError::Malformed)
        ));
        assert!(matches!(
            table.authenticate(&HeaderMap::new()),
            Err(AuthError::Missing)
        ));
        assert!(matches!(
            table.authenticate(&headers(&[("x-api-key", "other")])),
            Err(AuthError::Unknown)
        ));
    }
}
