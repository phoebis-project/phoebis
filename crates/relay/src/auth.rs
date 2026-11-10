//! The caller's credential header: which one was presented, and in what shape.

use axum::http::{header, HeaderMap};
use axum::response::Response;
use phoebis_routing::{Presented, RegistrySnapshot};
use phoebis_store::ApiKeyRow;

use crate::errors::{auth_failure, key_refusal, Failure};

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("more than one credential header was presented")]
    Ambiguous,
    #[error("the credential header is malformed")]
    Malformed,
    #[error("no credential header was presented")]
    Missing,
}

/// Exactly one credential header, in either shape. The lookup itself belongs to the registry snapshot, so a
/// key added or revoked between two requests is seen without rebuilding anything here.
pub fn presented_key(headers: &HeaderMap) -> Result<&str, AuthError> {
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
    Ok(presented)
}

/// Whether the body's `Content-Type` names `application/json`, its parameters aside: the admin gate and the keepalive
/// events route each hold a body-carrying write to it, and a missing or unreadable header reads as not JSON.
pub fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|essence| essence.trim().eq_ignore_ascii_case("application/json"))
}

/// The key the request presents, resolved exactly as `/v1/messages` resolves it, for a side route on the proxy
/// listener: either refusal answers the relay's own 401 body, which the route's `no-store` layer marks, so a wrong
/// key learns nothing there it would not learn on the messages route.
#[allow(clippy::result_large_err)]
pub fn authenticated_key<'a>(
    snapshot: &'a RegistrySnapshot,
    headers: &HeaderMap,
    now: i64,
    request_id: &str,
) -> Result<&'a ApiKeyRow, Response> {
    resolve_key(snapshot, headers, now)
        .map(|(key, _)| key)
        .map_err(|failure| failure.response(request_id))
}

/// The one resolution `/v1/messages` and the side routes share, so a key is admitted and refused alike on both.
pub(crate) fn resolve_key<'a>(
    snapshot: &'a RegistrySnapshot,
    headers: &HeaderMap,
    now: i64,
) -> Result<(&'a ApiKeyRow, Presented), Failure> {
    let presented = presented_key(headers).map_err(|error| auth_failure(&error))?;
    snapshot.authenticate(presented, now).map_err(|refused| {
        // Here rather than on each route, so a side route's 401 leaves the same trace on the host as the messages
        // route's. The line names the key's id and the reason, never the presented secret.
        tracing::debug!(
            refusal = %refused.refusal,
            key_id = refused.key_id.as_deref(),
            "a presented key was refused"
        );
        key_refusal(&refused.refusal)
    })
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
    fn both_shapes_of_the_same_key_are_presented_verbatim() {
        assert_eq!(
            presented_key(&headers(&[("authorization", "Bearer phoebis-testkey")]))
                .expect("a bearer is presented"),
            "phoebis-testkey"
        );
        assert_eq!(
            presented_key(&headers(&[("x-api-key", " phoebis-testkey ")]))
                .expect("an x-api-key is presented"),
            "phoebis-testkey"
        );
    }

    #[test]
    fn repeats_and_wrong_schemes_are_rejected() {
        assert!(matches!(
            presented_key(&headers(&[
                ("authorization", "Bearer phoebis-testkey"),
                ("authorization", "Bearer phoebis-testkey"),
            ])),
            Err(AuthError::Ambiguous)
        ));
        assert!(matches!(
            presented_key(&headers(&[("authorization", "Basic phoebis-testkey")])),
            Err(AuthError::Malformed)
        ));
        assert!(matches!(
            presented_key(&headers(&[("authorization", "Bearer   ")])),
            Err(AuthError::Malformed)
        ));
        assert!(matches!(
            presented_key(&HeaderMap::new()),
            Err(AuthError::Missing)
        ));
    }
}
