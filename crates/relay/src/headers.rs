//! The outbound header sequence: the caller's own headers, in the caller's order and spelling, with the
//! account's credential in place of the router's key and the framing the relay is about to write.

use axum::http::HeaderMap;
use phoebis_store::Secret;

use crate::casing::OriginalHead;
use crate::memory::Injected;

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
///
/// `client_request_id` is emitted only when the caller sent no `x-client-request-id`: after the caller's `x-app`
/// line, else immediately before the credential slot.
///
/// The head and exactly what the builder added on its own: a beta the caller's list lacked, the generated
/// `x-client-request-id`. A beta the caller sent is never reported, so its 400 is the caller's to handle.
pub struct OutboundHead {
    pub headers: Vec<(String, String)>,
    pub injected: Vec<Injected>,
}

/// `inject_betas` names beta items by their `wire_name`; each one absent from the caller's list is appended to
/// the merged line, and when the caller sent no `anthropic-beta` line at all and the list is non-empty, a line
/// holding exactly the injected entries is inserted after `anthropic-version` (else right before the credential
/// slot, else at the tail ahead of the relay's own `Host`); with nothing to inject a missing line stays missing.
/// `drop_betas` entries are removed from the caller's list, whether the caller sent them or an earlier attempt
/// injected them. An `anthropic-beta` line that ends up empty emits no header at all, whether the caller sent it
/// empty or a drop emptied it: the list is what the line says, and a line saying nothing is left out.
#[allow(clippy::too_many_arguments)]
pub fn outbound_headers(
    caller: &HeaderMap,
    original: Option<&OriginalHead>,
    kind: &CredentialKind,
    host: &str,
    content_length: usize,
    client_request_id: Option<&str>,
    inject_betas: &[Injected],
    drop_betas: &[&str],
) -> OutboundHead {
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
    let caller_has_request_id = sent
        .iter()
        .any(|(n, _)| n.eq_ignore_ascii_case("x-client-request-id"));
    // A caller may split a list-valued beta across several lines. Every line's tokens are entries in one
    // list, so they are joined in order and kept once each, and the credential's own claim is applied to the
    // whole list once. A caller that wrote one line has it travel exactly as it was written.
    let beta_lines: Vec<&str> = sent
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
        .map(|(_, value)| *value)
        .collect();
    let has_beta_line = !beta_lines.is_empty();
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
    // An operator's `x-app` replaces the caller's line, so the generated id has no caller line to follow and
    // takes the credential slot instead; a caller that wrote `x-app` twice gets the id after the first.
    let caller_has_app = sent.iter().any(|(n, _)| n.eq_ignore_ascii_case("x-app"));
    let generated = client_request_id.filter(|_| !caller_has_request_id);
    let effective_beta = has_beta_line.then_some(beta_list.as_str());
    let (merged_beta, merged_added) = match effective_beta {
        Some(value) => {
            let (line, added) = betas(value, kind, inject_betas, drop_betas);
            (Some(line), added)
        }
        None => (None, Vec::new()),
    };
    let mut out: Vec<(String, String)> = Vec::with_capacity(sent.len() + 3);
    let mut injected: Vec<Injected> = Vec::new();
    // A caller may write the same line twice: hyper keeps one value in its map, but the recorded head holds
    // both lines, and a slot whose value the relay owns would then be written twice upstream.
    let (mut saw_app, mut saw_beta, mut saw_credential, mut saw_host, mut saw_length) =
        (false, false, false, false, false);
    for (name, value) in sent {
        let lower = name.to_ascii_lowercase();
        match lower.as_str() {
            // The caller presented the router's key here, never the account's: the slot is the caller's, the
            // header name and the secret are the account's.
            "authorization" | "x-api-key" => {
                if !saw_credential {
                    saw_credential = true;
                    if let (false, Some(id)) = (caller_has_app, generated) {
                        out.push(("x-client-request-id".into(), id.into()));
                        injected.push(Injected::ClientRequestId);
                    }
                    out.push(credential(kind));
                }
            }
            // Only a line no account header replaced reaches here, so the effective list is the caller's own.
            "anthropic-beta" => {
                if !saw_beta {
                    saw_beta = true;
                    // A drop can empty a list whose only entry it removed; the caller's line then has
                    // nothing left to say, so the header is left out rather than sent empty.
                    if let Some(line) = merged_beta.as_deref().filter(|line| !line.is_empty()) {
                        out.push((name.into(), line.to_string()));
                    }
                    injected.extend(merged_added.iter().copied());
                }
            }
            // The relay's upstream connection is pooled and independent of the caller's, so it declares its
            // own hop rather than repeating what the caller said about a connection that ends here.
            "connection" => {
                out.push((name.into(), "keep-alive".into()));
            }
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
            _ => {
                out.push((name.into(), value.into()));
                if lower == "x-app" && !saw_app {
                    saw_app = true;
                    if let Some(id) = generated {
                        out.push(("x-client-request-id".into(), id.into()));
                        injected.push(Injected::ClientRequestId);
                    }
                }
            }
        }
    }
    if !saw_credential {
        // An in-process caller presents no credential of its own, so there is no slot to take over. The
        // measured client puts its API key right after `anthropic-version`, the one position the capture
        // gives for a credential the caller did not place.
        let mut at = out
            .iter()
            .position(|(name, _)| name.eq_ignore_ascii_case("anthropic-version"))
            .map_or(out.len(), |at| at + 1);
        if let (false, Some(id)) = (caller_has_app, generated) {
            out.insert(at, ("x-client-request-id".into(), id.into()));
            injected.push(Injected::ClientRequestId);
            at += 1;
        }
        out.insert(at, credential(kind));
    }
    // No line at all to merge into, neither the caller's nor the account's, so nothing above ever ran `betas`;
    // an injected feature still needs a line of its own, but never the OAuth claim, which rides on a list the
    // caller or the operator wrote.
    if effective_beta.is_none() && !inject_betas.is_empty() {
        let line = inject_betas
            .iter()
            .map(|item| item.wire_name())
            .collect::<Vec<_>>()
            .join(",");
        let at = out
            .iter()
            .position(|(name, _)| name.eq_ignore_ascii_case("anthropic-version"))
            .map(|at| at + 1)
            .or_else(|| {
                out.iter().position(|(name, _)| {
                    name.eq_ignore_ascii_case("authorization")
                        || name.eq_ignore_ascii_case("x-api-key")
                })
            })
            .or_else(|| {
                out.iter()
                    .position(|(name, _)| name.eq_ignore_ascii_case("host"))
            })
            .unwrap_or(out.len());
        out.insert(at, ("anthropic-beta".into(), line));
        injected.extend(inject_betas.iter().copied());
    }
    if !saw_host {
        out.push(("Host".into(), host.into()));
    }
    if !saw_length {
        out.push(("Content-Length".into(), content_length.to_string()));
    }
    OutboundHead {
        headers: out,
        injected,
    }
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

/// Splits `value`, drops every `drop` entry, applies the OAuth claim (inserted for an OAuth account and
/// stripped for an API key), then appends each `inject` item whose `wire_name` the list still lacks. Returns
/// the merged line and exactly the items it appended, so a client-sent beta of the same name is never reported
/// as the relay's own.
fn betas(
    value: &str,
    kind: &CredentialKind,
    inject: &[Injected],
    drop: &[&str],
) -> (String, Vec<Injected>) {
    let mut list = value
        .split(',')
        .filter(|entry| !drop.contains(&entry.trim()))
        .collect::<Vec<_>>()
        .join(",");
    let mut injected = Vec::new();
    for item in inject {
        let name = item.wire_name();
        if list.split(',').any(|entry| entry.trim() == name) {
            continue;
        }
        list = if list.is_empty() {
            name.to_string()
        } else {
            format!("{list},{name}")
        };
        injected.push(*item);
    }
    let claimed = || list.split(',').any(|entry| entry.trim() == OAUTH_BETA);
    let list = match kind {
        CredentialKind::ApiKey { .. } => list
            .split(',')
            .filter(|entry| entry.trim() != OAUTH_BETA)
            .collect::<Vec<_>>()
            .join(","),
        CredentialKind::Oauth { .. } if claimed() => list,
        CredentialKind::Oauth { .. } if list.trim().is_empty() => OAUTH_BETA.into(),
        // The beta is a claim about the credential rather than about the client's feature set, so it leads
        // the list and the caller's own entries keep their order behind it.
        CredentialKind::Oauth { .. } => format!("{OAUTH_BETA},{list}"),
    };
    (list, injected)
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
            None,
            &[],
            &[],
        )
        .headers
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
            None,
            &[],
            &[],
        )
        .headers;
        let names: Vec<&str> = emitted.iter().map(|(n, _)| n.as_str()).collect();
        let version = names
            .iter()
            .position(|n| *n == "anthropic-version")
            .expect("the caller sent a version");
        assert_eq!(names[version + 1], "x-api-key");
        assert!(names.contains(&"Host") && names.contains(&"Content-Length"));
    }

    #[test]
    fn a_generated_client_request_id_follows_the_callers_x_app_line() {
        let caller = head(&[
            ("Authorization", "Bearer phoebis-testkey"),
            ("anthropic-version", "2023-06-01"),
            ("x-app", "cli"),
        ]);
        let emitted = outbound_headers(
            &HeaderMap::new(),
            Some(&caller),
            &oauth(),
            "api.anthropic.com",
            7,
            Some("generated-id"),
            &[],
            &[],
        )
        .headers;
        let names: Vec<&str> = emitted.iter().map(|(n, _)| n.as_str()).collect();
        let app = names
            .iter()
            .position(|n| n.eq_ignore_ascii_case("x-app"))
            .expect("the caller sent x-app");
        assert_eq!(names[app + 1], "x-client-request-id");
        assert_eq!(emitted[app + 1].1, "generated-id");
    }

    #[test]
    fn without_an_x_app_line_the_generated_id_precedes_the_credential() {
        let caller = head(&[("Authorization", "Bearer phoebis-testkey")]);
        let emitted = outbound_headers(
            &HeaderMap::new(),
            Some(&caller),
            &oauth(),
            "api.anthropic.com",
            7,
            Some("generated-id"),
            &[],
            &[],
        )
        .headers;
        assert_eq!(
            emitted[0],
            ("x-client-request-id".into(), "generated-id".into())
        );
        assert_eq!(emitted[1].0, "Authorization");
    }

    #[test]
    fn a_callers_own_client_request_id_travels_once_at_its_own_position_and_nothing_is_generated() {
        let caller = head(&[
            ("Authorization", "Bearer phoebis-testkey"),
            ("x-app", "cli"),
            ("x-client-request-id", "caller-id"),
        ]);
        let emitted = outbound_headers(
            &HeaderMap::new(),
            Some(&caller),
            &oauth(),
            "api.anthropic.com",
            7,
            Some("generated-id"),
            &[],
            &[],
        )
        .headers;
        assert_eq!(
            emitted
                .iter()
                .filter(|(n, _)| n.eq_ignore_ascii_case("x-client-request-id"))
                .count(),
            1
        );
        let names: Vec<&str> = emitted.iter().map(|(n, _)| n.as_str()).collect();
        let app = names
            .iter()
            .position(|n| n.eq_ignore_ascii_case("x-app"))
            .expect("the caller sent x-app");
        assert_eq!(names[app + 1], "x-client-request-id");
        assert_eq!(emitted[app + 1].1, "caller-id");
    }

    #[test]
    fn the_generated_request_id_is_emitted_once_when_x_app_is_repeated() {
        let emit = |caller: &OriginalHead| {
            outbound_headers(
                &HeaderMap::new(),
                Some(caller),
                &oauth(),
                "api.anthropic.com",
                7,
                Some("generated-id"),
                &[],
                &[],
            )
            .headers
        };
        let ids = |emitted: &[(String, String)]| {
            emitted
                .iter()
                .filter(|(n, _)| n.eq_ignore_ascii_case("x-client-request-id"))
                .count()
        };
        let twice = head(&[
            ("Authorization", "Bearer phoebis-testkey"),
            ("x-app", "cli"),
            ("x-app", "cli"),
        ]);
        let emitted = emit(&twice);
        assert_eq!(ids(&emitted), 1, "two x-app lines, one id: {emitted:?}");
        assert_eq!(emitted[1].0, "x-app");
        assert_eq!(emitted[2].0, "x-client-request-id");
    }

    /// An API-key credential, so the OAuth claim `betas` would otherwise insert into an empty list never
    /// contaminates what these cases check about `inject_betas` alone.
    fn emit_injected(caller: &OriginalHead, inject: &[Injected]) -> OutboundHead {
        outbound_headers(
            &HeaderMap::new(),
            Some(caller),
            &api_key(),
            "api.anthropic.com",
            7,
            None,
            inject,
            &[],
        )
    }

    #[test]
    fn a_caller_with_no_beta_line_gets_one_holding_exactly_the_injected_entries_after_the_version()
    {
        let caller = head(&[
            ("Authorization", "Bearer phoebis-testkey"),
            ("anthropic-version", "2023-06-01"),
        ]);
        let head = emit_injected(&caller, &[Injected::CacheDiagnosisBeta]);
        let names: Vec<&str> = head.headers.iter().map(|(n, _)| n.as_str()).collect();
        let version = names
            .iter()
            .position(|n| *n == "anthropic-version")
            .unwrap();
        assert_eq!(names[version + 1], "anthropic-beta");
        assert_eq!(head.headers[version + 1].1, "cache-diagnosis-2026-04-07");
        assert_eq!(head.injected, vec![Injected::CacheDiagnosisBeta]);
    }

    #[test]
    fn an_empty_injection_list_leaves_a_missing_beta_line_missing() {
        let caller = head(&[("Authorization", "Bearer phoebis-testkey")]);
        let head = emit_injected(&caller, &[]);
        assert!(!head
            .headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case("anthropic-beta")));
        assert!(head.injected.is_empty());
    }

    #[test]
    fn an_empty_beta_line_carries_the_injected_beta_alone() {
        let caller = head(&[
            ("Authorization", "Bearer phoebis-testkey"),
            ("anthropic-beta", ""),
        ]);
        let head = emit_injected(&caller, &[Injected::CacheDiagnosisBeta]);
        let (_, value) = head
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("anthropic-beta"))
            .unwrap();
        assert_eq!(value, "cache-diagnosis-2026-04-07");
        assert_eq!(head.injected, vec![Injected::CacheDiagnosisBeta]);
    }

    #[test]
    fn two_beta_lines_merge_into_one_ending_with_the_injected_beta_once() {
        let caller = head(&[
            ("Authorization", "Bearer phoebis-testkey"),
            ("anthropic-beta", "claude-code-20250219"),
            ("anthropic-beta", "fine-grained-tool-streaming-2025-05-14"),
        ]);
        let head = emit_injected(&caller, &[Injected::CacheDiagnosisBeta]);
        let (_, value) = head
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("anthropic-beta"))
            .unwrap();
        assert_eq!(
            value,
            "claude-code-20250219,fine-grained-tool-streaming-2025-05-14,cache-diagnosis-2026-04-07"
        );
        assert_eq!(head.injected, vec![Injected::CacheDiagnosisBeta]);
    }

    #[test]
    fn a_list_already_carrying_the_beta_reports_nothing_and_a_generated_request_id_reports_it() {
        let caller = head(&[
            ("Authorization", "Bearer phoebis-testkey"),
            (
                "anthropic-beta",
                "claude-code-20250219,cache-diagnosis-2026-04-07",
            ),
        ]);
        assert!(emit_injected(&caller, &[Injected::CacheDiagnosisBeta])
            .injected
            .is_empty());

        let caller = head(&[("Authorization", "Bearer phoebis-testkey")]);
        let outbound = outbound_headers(
            &HeaderMap::new(),
            Some(&caller),
            &oauth(),
            "api.anthropic.com",
            7,
            Some("generated-id"),
            &[],
            &[],
        );
        assert_eq!(outbound.injected, vec![Injected::ClientRequestId]);
    }

    #[test]
    fn betas_drops_a_middle_entry_and_injects_into_an_empty_list() {
        let (line, injected) = betas(
            "a,extended-cache-ttl-2025-04-11,b",
            &api_key(),
            &[],
            &["extended-cache-ttl-2025-04-11"],
        );
        assert_eq!(
            line, "a,b",
            "the middle entry is gone, the others keep their order"
        );
        assert!(injected.is_empty());

        let (line, injected) = betas("", &api_key(), &[Injected::ExtendedTtlBeta], &[]);
        assert_eq!(line, "extended-cache-ttl-2025-04-11");
        assert_eq!(injected, vec![Injected::ExtendedTtlBeta]);
    }

    #[test]
    fn a_dropped_beta_leaves_the_list() {
        let caller = head(&[(
            "anthropic-beta",
            "claude-code-20250219,extended-cache-ttl-2025-04-11",
        )]);
        let (line, injected) = betas(
            "claude-code-20250219,extended-cache-ttl-2025-04-11",
            &api_key(),
            &[],
            &["extended-cache-ttl-2025-04-11"],
        );
        assert_eq!(line, "claude-code-20250219");
        assert!(injected.is_empty());

        let emitted = outbound_headers(
            &HeaderMap::new(),
            Some(&caller),
            &api_key(),
            "api.anthropic.com",
            7,
            None,
            &[],
            &["extended-cache-ttl-2025-04-11"],
        );
        assert_eq!(
            emitted
                .headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case("anthropic-beta"))
                .unwrap()
                .1,
            "claude-code-20250219"
        );
    }

    #[test]
    fn a_drop_that_empties_the_callers_only_entry_leaves_the_header_out_entirely() {
        let caller = head(&[("anthropic-beta", "extended-cache-ttl-2025-04-11")]);
        let emitted = outbound_headers(
            &HeaderMap::new(),
            Some(&caller),
            &api_key(),
            "api.anthropic.com",
            7,
            None,
            &[],
            &["extended-cache-ttl-2025-04-11"],
        );
        assert!(
            !emitted
                .headers
                .iter()
                .any(|(n, _)| n.eq_ignore_ascii_case("anthropic-beta")),
            "the drop left nothing for the caller's line to say"
        );
    }
}
