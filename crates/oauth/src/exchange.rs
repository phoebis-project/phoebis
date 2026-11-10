//! Token endpoint bodies, the six control-plane request shapes, and the responses they return.

use bytes::Bytes;
use phoebis_profile::{ControlEndpoint, Profile};
use phoebis_store::{NewBundle, Secret, SubscriptionRow};
use phoebis_transport::request::RawRequest;
use serde_json::{Map, Value};

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
    /// serde's message quotes the value it refused, which a usage body or a claim's answer may show and a token body
    /// must not, so the parses of those two alone build this variant.
    #[error("control response is not valid JSON: {0}")]
    UsageJson(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlKind {
    Token,
    Profile,
    Roles,
    Usage,
    LimitResets,
}

impl ControlKind {
    pub fn endpoint(self, profile: &Profile) -> ControlEndpoint {
        match self {
            ControlKind::Token => profile.control.token,
            ControlKind::Profile => profile.control.profile,
            ControlKind::Roles => profile.control.roles,
            ControlKind::Usage => profile.control.usage,
            ControlKind::LimitResets => profile.control.limit_resets,
        }
    }

    /// The host to resolve, present as SNI, and send in `Host`. The token endpoint lives on a different
    /// host than the other four.
    pub fn host(self, profile: &Profile) -> &'static str {
        self.endpoint(profile).host
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

/// The refresh body, in its measured key order.
pub fn refresh_body(profile: &Profile, refresh_token: &str, scopes: &str) -> Vec<u8> {
    json_body(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", profile.control.client_id),
        ("scope", scopes),
    ])
}

/// Builds one control-plane request with the header order measured per endpoint. The sets differ in more
/// than the obvious ways (the token call sends Axios' user agent, the profile and roles reads the client's own,
/// usage the CLI one, and roles sends no `Content-Type`), so each one is spelled out rather than derived from a
/// common base.
pub fn control_request(
    profile: &Profile,
    kind: ControlKind,
    bearer: Option<&str>,
    body: Option<Vec<u8>>,
) -> RawRequest {
    let endpoint = kind.endpoint(profile);
    let body = Bytes::from(body.unwrap_or_default());
    let mut headers: Vec<(String, String)> = Vec::with_capacity(9);
    let mut push = |name: &str, value: &str| headers.push((name.to_string(), value.to_string()));

    push("Accept", ACCEPT);
    match kind {
        ControlKind::Token => {
            push("Content-Type", CONTENT_TYPE_JSON);
            push("User-Agent", profile.control.axios_user_agent);
            push("Content-Length", &body.len().to_string());
        }
        ControlKind::Profile => {
            push("Content-Type", CONTENT_TYPE_JSON);
            push("User-Agent", profile.control.read_user_agent);
            if let Some(bearer) = bearer {
                push("Authorization", &format!("Bearer {bearer}"));
            }
            push("Cache-Control", "no-cache");
        }
        ControlKind::Roles => {
            push("User-Agent", profile.control.read_user_agent);
            if let Some(bearer) = bearer {
                push("Authorization", &format!("Bearer {bearer}"));
            }
        }
        // The probes sent the usage read's head in the client's shape too, and the upstream filled the block for it.
        ControlKind::Usage | ControlKind::LimitResets => {
            push("Content-Type", CONTENT_TYPE_JSON);
            push("User-Agent", profile.user_agent_cli);
            if let Some(bearer) = bearer {
                push("Authorization", &format!("Bearer {bearer}"));
            }
            push("anthropic-beta", USAGE_BETA);
        }
    }
    push("Accept-Encoding", ACCEPT_ENCODING);
    push("Host", endpoint.host);
    push("Connection", "close");

    RawRequest {
        method: match kind {
            ControlKind::Token => "POST",
            ControlKind::Profile
            | ControlKind::Roles
            | ControlKind::Usage
            | ControlKind::LimitResets => "GET",
        },
        target: endpoint.target.to_string(),
        headers,
        body,
    }
}

/// The claim of a limit reset as the client sends it: a `POST` to the organization's `reset_rate_limits` on the
/// usage endpoint's host, under the usage read's head with the body's length before `Accept-Encoding`.
pub fn claim_request(
    profile: &Profile,
    organization_uuid: &str,
    bearer: &str,
    grant_id: &str,
    request_id: &str,
) -> RawRequest {
    let body = Bytes::from(json_body(&[
        ("program", "cedar_ember"),
        ("grant_id", grant_id),
        ("request_id", request_id),
    ]));
    let host = profile.control.usage.host;
    let headers = [
        ("Accept", ACCEPT.to_string()),
        ("Content-Type", CONTENT_TYPE_JSON.to_string()),
        ("User-Agent", profile.user_agent_cli.to_string()),
        ("Authorization", format!("Bearer {bearer}")),
        ("anthropic-beta", USAGE_BETA.to_string()),
        // Where the client's HTTP library writes the length of a body: after the caller's last header.
        ("Content-Length", body.len().to_string()),
        ("Accept-Encoding", ACCEPT_ENCODING.to_string()),
        ("Host", host.to_string()),
        ("Connection", "close".to_string()),
    ];
    RawRequest {
        method: "POST",
        target: format!("/api/organizations/{organization_uuid}/reset_rate_limits"),
        headers: headers
            .into_iter()
            .map(|(name, value)| (name.to_string(), value))
            .collect(),
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

impl TokenResponse {
    /// The bundle the response stores, issued at `now`, with `fallback_scopes` where the response grants none.
    pub fn into_bundle(self, now: i64, fallback_scopes: &str) -> NewBundle {
        NewBundle {
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            issued_at: now,
            // Saturating, because the lifetimes are upstream-controlled numbers and an overflow here would
            // panic the caller rather than reject the response.
            expires_at: now.saturating_add(self.expires_in.saturating_mul(1000)),
            refresh_expires_at: self
                .refresh_token_expires_in
                .map(|seconds| now.saturating_add(seconds.saturating_mul(1000))),
            scopes: self.scope.unwrap_or_else(|| fallback_scopes.to_string()),
        }
    }
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct TokenAccount {
    pub uuid: String,
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
    pub subscription: SubscriptionRow,
    /// The whole document, so the admin views can show fields this struct does not name.
    pub raw: Value,
}

/// An RFC 3339 string as Unix milliseconds, `None` for a value that is not a string or does not parse.
pub(crate) fn instant_ms(value: &Value) -> Option<i64> {
    value
        .as_str()?
        .parse::<jiff::Timestamp>()
        .ok()
        .map(|t| t.as_millisecond())
}

pub fn parse_profile(body: &[u8]) -> Result<ProfileResponse, OauthError> {
    let raw: Value = serde_json::from_slice(body)?;
    let field = |parent: &str, key: &str| -> Option<String> {
        Some(raw.get(parent)?.get(key)?.as_str()?.to_string())
    };
    let account_uuid = field("account", "uuid")
        .ok_or_else(|| OauthError::Malformed("profile has no account.uuid".into()))?;
    let email = field("account", "email")
        .ok_or_else(|| OauthError::Malformed("profile has no account.email".into()))?;
    let organization_uuid = field("organization", "uuid");
    let millis = |key: &str| raw.get("organization")?.get(key).and_then(instant_ms);
    let subscription = SubscriptionRow {
        tier: field("organization", "rate_limit_tier"),
        since: millis("subscription_created_at"),
        trial_ends_at: millis("claude_code_trial_ends_at"),
    };
    Ok(ProfileResponse {
        account_uuid,
        email,
        organization_uuid,
        subscription,
        raw,
    })
}

/// The upstream reads these bodies by key. The keys are inserted in the order measured from one Claude Code release,
/// so the body matches its fixture byte for byte, and `serde_json`'s `preserve_order` feature keeps them there.
fn json_body(pairs: &[(&str, &str)]) -> Vec<u8> {
    let mut map = Map::with_capacity(pairs.len());
    for (key, value) in pairs {
        map.insert((*key).to_string(), Value::String((*value).to_string()));
    }
    serde_json::to_vec(&Value::Object(map)).expect("a map of strings always serializes")
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
    fn a_token_parse_failure_never_quotes_the_value_it_refused() {
        let body = br#"{"access_token":"a","expires_in":"sk-ant-oat01-PLACEHOLDER"}"#;
        let error = parse_token_response(body).expect_err("a string lifetime is not a token");
        assert!(
            !error.to_string().contains("sk-ant-oat01-PLACEHOLDER"),
            "{error}"
        );
    }

    #[test]
    fn a_non_positive_lifetime_is_not_a_token() {
        let body = br#"{"access_token":"a","expires_in":0}"#;
        assert!(matches!(
            parse_token_response(body),
            Err(OauthError::Malformed(_))
        ));
    }

    #[test]
    fn the_email_is_read_from_account_email_as_the_client_validates_it() {
        let measured =
            br#"{"account":{"uuid":"a","email":"a@example.invalid"},"organization":{"uuid":"o"}}"#;
        let parsed = parse_profile(measured).unwrap();
        assert_eq!(parsed.email, "a@example.invalid");
        assert_eq!(parsed.organization_uuid.as_deref(), Some("o"));

        let token_shape = br#"{"account":{"uuid":"a","email_address":"a@example.invalid"}}"#;
        match parse_profile(token_shape) {
            Err(OauthError::Malformed(detail)) => {
                assert_eq!(detail, "profile has no account.email")
            }
            other => panic!("expected a malformed profile, got {other:?}"),
        }
    }

    #[test]
    fn the_tier_the_start_and_the_trial_end_are_read_when_present_and_none_when_absent() {
        let max_with_dates = br#"{"account":{"uuid":"a","email":"a@example.invalid","has_claude_max":true,"has_claude_pro":true},"organization":{"rate_limit_tier":"default_claude_max_5x","subscription_created_at":"2026-03-14T10:00:00Z","claude_code_trial_ends_at":"2026-10-02T00:00:00+00:00"}}"#;
        let parsed = parse_profile(max_with_dates).unwrap().subscription;
        assert_eq!(parsed.tier.as_deref(), Some("default_claude_max_5x"));
        assert_eq!(parsed.since, Some(1_773_482_400_000));
        assert_eq!(parsed.trial_ends_at, Some(1_790_899_200_000));

        let unparseable_date = br#"{"account":{"uuid":"a","email":"a@example.invalid"},"organization":{"subscription_created_at":"soon"}}"#;
        let parsed = parse_profile(unparseable_date).unwrap();
        assert_eq!(parsed.subscription.since, None);
        assert_eq!(
            parsed.raw["organization"]["subscription_created_at"],
            "soon"
        );

        let pinned = parse_profile(phoebis_profile::fixtures::control_response(
            phoebis_profile::fixtures::ControlFixture::Profile,
        ))
        .unwrap()
        .subscription;
        assert_eq!(pinned.tier.as_deref(), Some("default_claude_max_5x"));
        assert_eq!((pinned.since, pinned.trial_ends_at), (None, None));

        let synthetic = br#"{"account":{"uuid":"a","email":"a@example.invalid"}}"#;
        assert_eq!(
            parse_profile(synthetic).unwrap().subscription,
            SubscriptionRow::default()
        );
    }
}
