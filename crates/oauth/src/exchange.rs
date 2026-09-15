//! Token endpoint bodies, the four control-plane request shapes, and the responses they return.

use bytes::Bytes;
use phoebis_profile::Profile;
use phoebis_store::Secret;
use phoebis_transport::request::RawRequest;
use serde_json::{Map, Value};
use url::Url;

// Measured from the control fixtures under `crates/profile/fixtures/`: Axios sends this Accept and this
// Accept-Encoding on every control call, and the usage endpoint is the only one that carries a beta header.
const ACCEPT: &str = "application/json, text/plain, */*";
const ACCEPT_ENCODING: &str = "gzip, compress, deflate, br";
const CONTENT_TYPE_JSON: &str = "application/json";
const USAGE_BETA: &str = "oauth-2025-04-20";

#[derive(Debug, thiserror::Error)]
pub enum OauthError {
    #[error("malformed control response: {0}")]
    Malformed(String),
    #[error("control response is not valid JSON")]
    Json(#[from] serde_json::Error),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlKind {
    Token,
    Profile,
    Roles,
    Usage,
}

impl ControlKind {
    pub fn url(self, profile: &Profile) -> &'static str {
        match self {
            ControlKind::Token => profile.control.token_url,
            ControlKind::Profile => profile.control.profile_url,
            ControlKind::Roles => profile.control.roles_url,
            ControlKind::Usage => profile.control.usage_url,
        }
    }

    /// The host to resolve, present as SNI, and send in `Host`; the token endpoint lives on a different
    /// host than the other three.
    pub fn host(self, profile: &Profile) -> String {
        split_url(self.url(profile)).0
    }
}

/// The exchange body Claude Code posts after a pairing callback, in its measured key order.
pub fn exchange_body(
    profile: &Profile,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
    state: &str,
) -> Vec<u8> {
    json_body(&[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", profile.control.client_id),
        ("code_verifier", verifier),
        ("state", state),
    ])
}

/// The refresh body, in its measured key order. The captured client also sends an `expires_in` of one
/// year, which the token endpoint ignores; asking for a lifetime we do not control would only invite a
/// response we then have to reconcile, so it is left out.
pub fn refresh_body(profile: &Profile, refresh_token: &str, scopes: &str) -> Vec<u8> {
    json_body(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", profile.control.client_id),
        ("scope", scopes),
    ])
}

/// Builds one control-plane request with the header order measured per endpoint. The sets differ in more
/// than the obvious ways (usage sends the CLI user agent where the others send Axios', roles sends no
/// `Content-Type`), so each one is spelled out rather than derived from a common base.
pub fn control_request(
    profile: &Profile,
    kind: ControlKind,
    bearer: Option<&str>,
    body: Option<Vec<u8>>,
) -> RawRequest {
    let (host, target) = split_url(kind.url(profile));
    let body = Bytes::from(body.unwrap_or_default());
    let axios = profile.control.axios_user_agent;
    let mut headers: Vec<(String, String)> = Vec::with_capacity(9);
    let mut push = |name: &str, value: &str| headers.push((name.to_string(), value.to_string()));

    push("Accept", ACCEPT);
    match kind {
        ControlKind::Token => {
            push("Content-Type", CONTENT_TYPE_JSON);
            push("User-Agent", axios);
            push("Content-Length", &body.len().to_string());
        }
        ControlKind::Profile => {
            push("Content-Type", CONTENT_TYPE_JSON);
            if let Some(bearer) = bearer {
                push("Authorization", &format!("Bearer {bearer}"));
            }
            push("Cache-Control", "no-cache");
            push("User-Agent", axios);
        }
        ControlKind::Roles => {
            if let Some(bearer) = bearer {
                push("Authorization", &format!("Bearer {bearer}"));
            }
            push("User-Agent", axios);
        }
        ControlKind::Usage => {
            push("Content-Type", CONTENT_TYPE_JSON);
            push("User-Agent", profile.user_agent_sdk_cli);
            if let Some(bearer) = bearer {
                push("Authorization", &format!("Bearer {bearer}"));
            }
            push("anthropic-beta", USAGE_BETA);
        }
    }
    push("Accept-Encoding", ACCEPT_ENCODING);
    push("Host", &host);
    push("Connection", "close");

    RawRequest {
        method: match kind {
            ControlKind::Token => "POST",
            ControlKind::Profile | ControlKind::Roles | ControlKind::Usage => "GET",
        },
        target,
        headers,
        body,
    }
}

#[derive(Debug)]
pub struct TokenResponse {
    pub access_token: Secret,
    pub refresh_token: Option<Secret>,
    pub expires_in: i64,
    pub refresh_token_expires_in: Option<i64>,
    pub scope: Option<String>,
    pub account: Option<TokenAccount>,
    pub organization: Option<TokenOrg>,
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct TokenAccount {
    pub uuid: String,
    pub email_address: Option<String>,
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct TokenOrg {
    pub uuid: String,
    pub name: Option<String>,
}

// `Secret` has no `Deserialize`, by design: it must not be constructible from a wire type by accident.
// The token strings land here and are wrapped immediately. No `Debug`, so the raw values cannot be
// printed even from a deserialization failure path.
#[derive(serde::Deserialize)]
struct RawToken {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
    refresh_token_expires_in: Option<i64>,
    scope: Option<String>,
    account: Option<TokenAccount>,
    organization: Option<TokenOrg>,
}

pub fn parse_token_response(body: &[u8]) -> Result<TokenResponse, OauthError> {
    let raw: RawToken = serde_json::from_slice(body)?;
    if raw.access_token.is_empty() {
        return Err(OauthError::Malformed("empty access token".into()));
    }
    // A non-positive lifetime would make the bundle expire the moment it is stored, so every acquire would
    // rotate again; that is a broken response, not a token.
    if raw.expires_in <= 0 {
        return Err(OauthError::Malformed(format!(
            "expires_in must be positive, got {}",
            raw.expires_in
        )));
    }
    Ok(TokenResponse {
        access_token: Secret::new(&raw.access_token),
        refresh_token: raw.refresh_token.as_deref().map(Secret::new),
        expires_in: raw.expires_in,
        refresh_token_expires_in: raw.refresh_token_expires_in,
        scope: raw.scope,
        account: raw.account,
        organization: raw.organization,
    })
}

#[derive(Clone, Debug)]
pub struct ProfileResponse {
    pub account_uuid: String,
    pub email: String,
    pub organization_uuid: Option<String>,
    /// The whole document, so the admin views can show fields this struct does not name.
    pub raw: Value,
}

pub fn parse_profile(body: &[u8]) -> Result<ProfileResponse, OauthError> {
    let raw: Value = serde_json::from_slice(body)?;
    let field = |parent: &str, key: &str| -> Option<String> {
        Some(raw.get(parent)?.get(key)?.as_str()?.to_string())
    };
    let account_uuid = field("account", "uuid")
        .ok_or_else(|| OauthError::Malformed("profile has no account.uuid".into()))?;
    let email = field("account", "email_address")
        .ok_or_else(|| OauthError::Malformed("profile has no account.email_address".into()))?;
    let organization_uuid = field("organization", "uuid");
    Ok(ProfileResponse {
        account_uuid,
        email,
        organization_uuid,
        raw,
    })
}

/// The upstream reads these bodies by key, but the pinned client's byte-for-byte order is what keeps a
/// fingerprint from standing out, so the keys are inserted in the measured order and `serde_json`'s
/// `preserve_order` feature keeps them there.
fn json_body(pairs: &[(&str, &str)]) -> Vec<u8> {
    let mut map = Map::with_capacity(pairs.len());
    for (key, value) in pairs {
        map.insert((*key).to_string(), Value::String((*value).to_string()));
    }
    serde_json::to_vec(&Value::Object(map)).expect("a map of strings always serializes")
}

/// Splits a pinned control URL into its host and its request target. The URLs are compile-time profile
/// constants, so a parse failure is a profile bug rather than a runtime condition.
fn split_url(url: &str) -> (String, String) {
    let parsed = Url::parse(url).expect("profile control URLs are absolute");
    let host = parsed.host_str().unwrap_or_default().to_string();
    let mut target = parsed.path().to_string();
    if let Some(query) = parsed.query() {
        target.push('?');
        target.push_str(query);
    }
    (host, target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_response_never_prints_its_secrets() {
        let parsed = parse_token_response(phoebis_profile::fixtures::control_response(
            phoebis_profile::fixtures::ControlFixture::Token,
        ))
        .unwrap();
        let printed = format!("{parsed:?}");
        assert!(!printed.contains("sk-ant-oat01"), "{printed}");
        assert!(!printed.contains("sk-ant-ort01"), "{printed}");
    }

    #[test]
    fn a_non_positive_lifetime_is_not_a_token() {
        let body = br#"{"access_token":"a","expires_in":0}"#;
        assert!(matches!(
            parse_token_response(body),
            Err(OauthError::Malformed(_))
        ));
    }
}
