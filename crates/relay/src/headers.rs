//! The outbound header sequence, emitted in the measured order with the measured casing.

use axum::http::HeaderMap;
use phoebis_profile::{HeaderSlot, Profile};
use phoebis_store::Secret;

/// The credential the relay presents upstream, which decides both the credential slot and the beta list.
#[derive(Debug)]
pub enum CredentialKind {
    Oauth { bearer: Secret },
    ApiKey { secret: Secret },
}

/// Beta only an OAuth credential may claim; an API key that sends it is rejected upstream.
const OAUTH_BETA: &str = "oauth-2025-04-20";

/// Caller headers the relay never repeats upstream: hop-by-hop headers describe the caller's connection rather
/// than the payload, cookies and proxy credentials are the caller's own secrets, forwarding headers would
/// disclose the caller's network, the credential headers carry the router's key rather than the account's, and
/// `x-keepalive-owner-id` is the router's internal ownership marker.
const NEVER_COPIED: &[&str] = &[
    "authorization",
    "connection",
    "content-length",
    "cookie",
    "forwarded",
    "host",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "via",
    "x-api-key",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-keepalive-owner-id",
];

pub fn outbound_headers(
    profile: &Profile,
    caller: &HeaderMap,
    kind: &CredentialKind,
    host: &str,
    content_length: usize,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> =
        Vec::with_capacity(profile.inference_header_order.len() + 1);
    for slot in profile.inference_header_order {
        let emitted = match slot {
            HeaderSlot::Fixed(name, value) => {
                let value = caller_value(caller, name).unwrap_or(value);
                out.push(((*name).to_string(), value.to_string()));
                Some(*name)
            }
            HeaderSlot::Credential => match kind {
                CredentialKind::Oauth { bearer } => {
                    out.push((
                        "Authorization".to_string(),
                        format!("Bearer {}", bearer.expose()),
                    ));
                    Some("Authorization")
                }
                // An API key is not a bearer credential and sits in its own slot further down.
                CredentialKind::ApiKey { .. } => None,
            },
            HeaderSlot::Optional(name) => match caller_value(caller, name) {
                Some(value) => {
                    out.push(((*name).to_string(), value.to_string()));
                    Some(*name)
                }
                None => None,
            },
            HeaderSlot::FromCaller(name) => {
                match caller_value(caller, name).or_else(|| baseline(profile, name, kind)) {
                    Some(value) => {
                        out.push(((*name).to_string(), adjust(name, value, kind)));
                        Some(*name)
                    }
                    None => None,
                }
            }
            HeaderSlot::Host => {
                out.push(("Host".to_string(), host.to_string()));
                Some("Host")
            }
            HeaderSlot::ContentLength => {
                out.push(("Content-Length".to_string(), content_length.to_string()));
                Some("Content-Length")
            }
        };
        if let (Some(name), CredentialKind::ApiKey { secret }) = (emitted, kind) {
            if name.eq_ignore_ascii_case(profile.api_key_slot_after) {
                out.push(("x-api-key".to_string(), secret.expose().to_string()));
            }
        }
    }
    out
}

/// Value for a slot the caller left out. A non-native caller gets the profile's own baseline so the request
/// still looks like the measured client.
fn baseline(profile: &Profile, name: &str, kind: &CredentialKind) -> Option<&'static str> {
    Some(match name.to_ascii_lowercase().as_str() {
        // The first-party list is not a default: those betas are only accepted behind the first-party base
        // URL, so a caller that says nothing gets the list every account may send.
        "anthropic-beta" => match kind {
            CredentialKind::ApiKey { .. } => profile.betas.api_key_non_first_party,
            CredentialKind::Oauth { .. } => profile.betas.oauth_non_first_party,
        },
        "user-agent" => profile.user_agent_sdk_cli,
        "x-stainless-arch" => profile.stainless.arch,
        "x-stainless-lang" => profile.stainless.lang,
        "x-stainless-os" => profile.stainless.os,
        "x-stainless-package-version" => profile.stainless.package_version,
        // The relay's own attempt counter starts at zero; its retries are invisible to the caller.
        "x-stainless-retry-count" => "0",
        "x-stainless-runtime" => profile.stainless.runtime,
        "x-stainless-runtime-version" => profile.stainless.runtime_version,
        "x-stainless-timeout" => profile.stainless.timeout,
        _ => return None,
    })
}

fn adjust(name: &str, value: &str, kind: &CredentialKind) -> String {
    if name.eq_ignore_ascii_case("anthropic-beta") && matches!(kind, CredentialKind::ApiKey { .. })
    {
        return value
            .split(',')
            .filter(|entry| entry.trim() != OAUTH_BETA)
            .collect::<Vec<_>>()
            .join(",");
    }
    value.to_string()
}

fn caller_value<'a>(caller: &'a HeaderMap, name: &str) -> Option<&'a str> {
    if NEVER_COPIED.iter().any(|n| name.eq_ignore_ascii_case(n)) {
        return None;
    }
    caller.get(name).and_then(|value| value.to_str().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caller() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "anthropic-beta",
            "claude-code-20250219,oauth-2025-04-20,extended-cache-ttl-2025-04-11"
                .parse()
                .unwrap(),
        );
        headers.insert("connection", "close".parse().unwrap());
        headers.insert("cookie", "session=1".parse().unwrap());
        headers.insert("x-keepalive-owner-id", "owner-1".parse().unwrap());
        headers
    }

    #[test]
    fn an_api_key_account_drops_the_oauth_beta_and_fills_its_own_slot() {
        let profile = phoebis_profile::claude_code_2_1_269_linux_x64();
        let emitted = outbound_headers(
            profile,
            &caller(),
            &CredentialKind::ApiKey {
                secret: Secret::new("sk-ant-api03-PLACEHOLDER"),
            },
            "api.anthropic.com",
            7,
        );
        let names: Vec<&str> = emitted.iter().map(|(n, _)| n.as_str()).collect();
        let version = names
            .iter()
            .position(|n| *n == "anthropic-version")
            .expect("the version slot is emitted");
        assert_eq!(names[version + 1], "x-api-key");
        let beta = emitted
            .iter()
            .find(|(n, _)| n == "anthropic-beta")
            .map(|(_, v)| v.as_str())
            .expect("the caller sent a beta list");
        assert_eq!(
            beta, "claude-code-20250219,extended-cache-ttl-2025-04-11",
            "the oauth beta leaves no empty entry behind"
        );
        assert!(!names.contains(&"Authorization"));
    }

    #[test]
    fn caller_connection_and_local_headers_never_reach_the_upstream() {
        let profile = phoebis_profile::claude_code_2_1_269_linux_x64();
        let emitted = outbound_headers(
            profile,
            &caller(),
            &CredentialKind::Oauth {
                bearer: Secret::new("sk-ant-oat01-PLACEHOLDER"),
            },
            "api.anthropic.com",
            7,
        );
        assert!(emitted
            .iter()
            .any(|(n, v)| n == "Connection" && v == "keep-alive"));
        assert!(
            !emitted.iter().any(|(n, _)| n.eq_ignore_ascii_case("cookie")
                || n.eq_ignore_ascii_case("x-keepalive-owner-id"))
        );
        assert!(emitted
            .iter()
            .any(|(n, v)| n == "Authorization" && v == "Bearer sk-ant-oat01-PLACEHOLDER"));
    }
}
