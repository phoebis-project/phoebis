//! The outbound header sequence: the caller's own headers, in the caller's order and spelling, with the
//! account's credential in place of the router's key and the framing the relay is about to write.

use axum::http::HeaderMap;
use phoebis_store::Secret;

use crate::casing::OriginalHead;

/// The credential the relay presents upstream, which decides both the header the secret goes in and the beta
/// list the account may claim.
#[derive(Debug)]
pub enum CredentialKind {
    Oauth { bearer: Secret },
    ApiKey { secret: Secret },
}

/// Beta only an OAuth credential may claim; an API key that sends it is rejected upstream.
const OAUTH_BETA: &str = "oauth-2025-04-20";

/// Caller headers the relay never repeats upstream: the hop-by-hop ones describe the caller's connection
/// rather than the payload, cookies and proxy credentials are the caller's own secrets, forwarding headers
/// would disclose the caller's network, and `x-keepalive-owner-id` is the router's internal ownership marker.
/// The credential, `content-length`, and `host` are not here because the relay owns their values rather than
/// dropping them; see the match below.
const NEVER_COPIED: &[&str] = &[
    "cookie",
    "forwarded",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "via",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-port",
    "x-forwarded-proto",
    "x-forwarded-server",
    "x-keepalive-owner-id",
    "x-real-ip",
];

/// The head the relay writes upstream. Every caller header travels as it was sent, in the order it was sent,
/// except the never-copied set, the credential slot, the beta list, and the three headers whose values
/// describe the relay's own hop rather than the caller's.
pub fn outbound_headers(
    caller: &HeaderMap,
    original: Option<&OriginalHead>,
    kind: &CredentialKind,
    host: &str,
    content_length: usize,
) -> Vec<(String, String)> {
    let sent: Vec<(&str, &str)> = match original {
        Some(head) => head
            .headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect(),
        // No head was recorded: an in-process caller, or a connection the recorder gave up on. The map's
        // lowercase names in its own iteration order are then the only record of what the caller sent.
        None => caller
            .iter()
            .filter_map(|(name, value)| Some((name.as_str(), value.to_str().ok()?)))
            .collect(),
    };
    // A caller may split a list-valued beta across several lines. Every line's tokens are entries in one
    // list, so they are joined in order and kept once each, and the credential's own claim is applied to the
    // whole list once. A caller that wrote one line has it travel exactly as it was written.
    let beta_lines: Vec<&str> = sent
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
        .map(|(_, value)| *value)
        .collect();
    let beta_list = match beta_lines.as_slice() {
        [only] => (*only).to_string(),
        lines => {
            let mut tokens: Vec<&str> = Vec::new();
            for token in lines.iter().flat_map(|line| line.split(',')).map(str::trim) {
                if !token.is_empty() && !tokens.contains(&token) {
                    tokens.push(token);
                }
            }
            tokens.join(",")
        }
    };
    let mut out: Vec<(String, String)> = Vec::with_capacity(sent.len() + 3);
    // A caller may write the same line twice: hyper keeps one value in its map, but the recorded head holds
    // both lines, and a slot whose value the relay owns would then be written twice upstream.
    let (mut saw_beta, mut saw_credential, mut saw_host, mut saw_length) =
        (false, false, false, false);
    for (name, value) in sent {
        let lower = name.to_ascii_lowercase();
        match lower.as_str() {
            // The caller presented the router's key here, never the account's: the slot is the caller's, the
            // header name and the secret are the account's.
            "authorization" | "x-api-key" => {
                if !saw_credential {
                    saw_credential = true;
                    out.push(credential(kind));
                }
            }
            "anthropic-beta" => {
                if !saw_beta {
                    saw_beta = true;
                    out.push((name.into(), betas(&beta_list, kind)));
                }
            }
            // The relay's upstream connection is pooled and independent of the caller's, so it declares its
            // own hop rather than repeating what the caller said about a connection that ends here.
            "connection" => out.push((name.into(), "keep-alive".into())),
            "content-length" => {
                if !saw_length {
                    saw_length = true;
                    out.push((name.into(), content_length.to_string()));
                }
            }
            "host" => {
                if !saw_host {
                    saw_host = true;
                    out.push((name.into(), host.into()));
                }
            }
            _ if NEVER_COPIED.contains(&lower.as_str()) => {}
            _ => out.push((name.into(), value.into())),
        }
    }
    if !saw_credential {
        // An in-process caller presents no credential of its own, so there is no slot to take over. The
        // measured client puts its API key right after `anthropic-version`, the one position the capture
        // gives for a credential the caller did not place.
        let at = out
            .iter()
            .position(|(name, _)| name.eq_ignore_ascii_case("anthropic-version"))
            .map_or(out.len(), |at| at + 1);
        out.insert(at, credential(kind));
    }
    if !saw_host {
        out.push(("Host".into(), host.into()));
    }
    if !saw_length {
        out.push(("Content-Length".into(), content_length.to_string()));
    }
    out
}

fn credential(kind: &CredentialKind) -> (String, String) {
    match kind {
        CredentialKind::ApiKey { secret } => ("x-api-key".into(), secret.expose().into()),
        CredentialKind::Oauth { bearer } => (
            "Authorization".into(),
            format!("Bearer {}", bearer.expose()),
        ),
    }
}

fn betas(value: &str, kind: &CredentialKind) -> String {
    let claimed = || value.split(',').any(|entry| entry.trim() == OAUTH_BETA);
    match kind {
        CredentialKind::ApiKey { .. } => value
            .split(',')
            .filter(|entry| entry.trim() != OAUTH_BETA)
            .collect::<Vec<_>>()
            .join(","),
        CredentialKind::Oauth { .. } if claimed() => value.into(),
        CredentialKind::Oauth { .. } if value.trim().is_empty() => OAUTH_BETA.into(),
        // The beta is a claim about the credential rather than about the client's feature set, so it leads
        // the list and the caller's own entries keep their order behind it.
        CredentialKind::Oauth { .. } => format!("{OAUTH_BETA},{value}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(pairs: &[(&str, &str)]) -> OriginalHead {
        OriginalHead {
            headers: pairs
                .iter()
                .map(|(n, v)| ((*n).to_string(), (*v).to_string()))
                .collect(),
            ..OriginalHead::default()
        }
    }

    fn api_key() -> CredentialKind {
        CredentialKind::ApiKey {
            secret: Secret::new("sk-ant-api03-PLACEHOLDER"),
        }
    }

    fn oauth() -> CredentialKind {
        CredentialKind::Oauth {
            bearer: Secret::new("sk-ant-oat01-PLACEHOLDER"),
        }
    }

    fn emit(original: &OriginalHead, kind: &CredentialKind) -> Vec<(String, String)> {
        outbound_headers(
            &HeaderMap::new(),
            Some(original),
            kind,
            "api.anthropic.com",
            7,
        )
    }

    /// The bus may not depend on this crate, so it carries its own copy of this set and the two are kept in
    /// step here: a header this relay refuses to repeat upstream must not reach a consumer either.
    #[test]
    fn the_bus_denylist_covers_every_never_copied_header() {
        for name in NEVER_COPIED {
            assert!(!phoebis_bus::header_published(name), "{name}");
        }
    }

    #[test]
    fn the_callers_order_spelling_and_values_travel_as_sent() {
        let caller = head(&[
            ("Accept", "application/json"),
            ("Authorization", "Bearer phoebis-testkey"),
            ("X-Custom", "kept"),
            ("Cookie", "session=1"),
            ("X-Keepalive-Owner-Id", "owner-1"),
            ("X-Real-IP", "203.0.113.9"),
            ("Connection", "close"),
            ("Host", "127.0.0.1:1"),
            ("Content-Length", "1"),
        ]);
        assert_eq!(
            emit(&caller, &oauth()),
            vec![
                ("Accept".into(), "application/json".into()),
                (
                    "Authorization".into(),
                    "Bearer sk-ant-oat01-PLACEHOLDER".into()
                ),
                ("X-Custom".into(), "kept".into()),
                ("Connection".into(), "keep-alive".into()),
                ("Host".into(), "api.anthropic.com".into()),
                ("Content-Length".into(), "7".into()),
            ],
            "the caller's own secrets and the address a reverse proxy stamped stay behind"
        );
    }

    #[test]
    fn an_api_key_account_takes_the_callers_credential_slot_and_drops_the_oauth_beta() {
        let caller = head(&[
            ("Authorization", "Bearer phoebis-testkey"),
            (
                "anthropic-beta",
                "claude-code-20250219,oauth-2025-04-20,extended-cache-ttl-2025-04-11",
            ),
            ("anthropic-version", "2023-06-01"),
        ]);
        let emitted = emit(&caller, &api_key());
        assert_eq!(emitted[0].0, "x-api-key", "the slot is the caller's");
        assert_eq!(emitted[0].1, "sk-ant-api03-PLACEHOLDER");
        assert_eq!(
            emitted[1].1, "claude-code-20250219,extended-cache-ttl-2025-04-11",
            "the oauth beta leaves no empty entry behind"
        );
        assert!(!emitted.iter().any(|(n, _)| n == "Authorization"));
    }

    #[test]
    fn an_oauth_account_inserts_the_beta_once_and_keeps_the_callers_entries_in_order() {
        let caller = head(&[
            ("Authorization", "Bearer phoebis-testkey"),
            (
                "anthropic-beta",
                "claude-code-20250219,fine-grained-tool-streaming-2025-05-14",
            ),
        ]);
        assert_eq!(
            emit(&caller, &oauth())[1].1,
            "oauth-2025-04-20,claude-code-20250219,fine-grained-tool-streaming-2025-05-14"
        );
        let already = head(&[
            ("Authorization", "Bearer phoebis-testkey"),
            ("anthropic-beta", "claude-code-20250219,oauth-2025-04-20"),
        ]);
        assert_eq!(
            emit(&already, &oauth())[1].1,
            "claude-code-20250219,oauth-2025-04-20",
            "a list that already claims it is not touched"
        );
    }

    #[test]
    fn a_caller_that_sent_no_beta_header_gets_none() {
        let caller = head(&[("Authorization", "Bearer phoebis-testkey")]);
        assert!(!emit(&caller, &oauth())
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case("anthropic-beta")));
    }

    #[test]
    fn an_in_process_caller_without_a_head_walks_the_map_and_places_the_credential() {
        let mut caller = HeaderMap::new();
        caller.insert("anthropic-version", "2023-06-01".parse().unwrap());
        caller.insert("x-app", "cli".parse().unwrap());
        let emitted = outbound_headers(
            &caller,
            None,
            &api_key(),
            "api.anthropic.com",
            7,
        );
        let names: Vec<&str> = emitted.iter().map(|(n, _)| n.as_str()).collect();
        let version = names
            .iter()
            .position(|n| *n == "anthropic-version")
            .expect("the caller sent a version");
        assert_eq!(names[version + 1], "x-api-key");
        assert!(names.contains(&"Host") && names.contains(&"Content-Length"));
    }
}
