//! Who and what a request is: the session, the agent, the owner, and the model it asks for.

use axum::http::HeaderMap;
use phoebis_bus::OTHER_CLIENT;
use phoebis_routing::{ClientClass, ConversationLookup, ConversationQuery};
use phoebis_store::{AffinitySource, ApiKeyRow};

use crate::billing::entrypoint_of;
use crate::RelayError;

/// The agent a request without an agent header belongs to.
const MAIN_AGENT: &str = "main";

/// Marker a downstream client appends to ask for the long-context tier. It is a client-side routing hint, so
/// it is folded out of the canonical model the registry resolves against.
const LONG_CONTEXT_SUFFIX: &str = "[1m]";

/// Bound on an identifier or model name the relay repeats into logs and ledger keys.
pub(crate) const MAX_ID_LEN: usize = 256;

#[derive(Debug)]
pub struct RequestIdentity {
    pub session_id: Option<String>,
    pub agent_id: String,
    pub parent_agent_id: Option<String>,
    pub owner_id: Option<String>,
    pub model: String,
    /// The client's spelling with the `[1m]` suffix removed and ASCII lowercased: what the body carries
    /// upstream, and the spelling the catalog's published map is keyed on.
    pub folded_model: String,
    /// `folded_model`, or the dated id the catalog cache resolved it to: the routing, ledger, and pricing key.
    pub canonical_model: String,
    /// `claude-code/<entrypoint>` or `other`: a log label, never a gate.
    pub client: String,
    /// The class the label maps to.
    pub class: ClientClass,
    /// For a request whose messages continue another conversation of the key and generation: that
    /// conversation's root (its session id) and the matched depth. The session id stays
    /// the identity; `None` for a session's own continuation.
    pub forked_from: Option<String>,
    pub fork_depth: Option<u32>,
}

/// Reads what the request says about itself, which `climb` completes with the fork facts.
pub fn identify(headers: &HeaderMap, body: &[u8]) -> Result<RequestIdentity, RelayError> {
    let model = string_member(body, &["model"]).ok_or(RelayError::NoModel)?;
    let folded_model = {
        let lowered = model.to_ascii_lowercase();
        lowered
            .strip_suffix(LONG_CONTEXT_SUFFIX)
            .unwrap_or(&lowered)
            .to_string()
    };
    // The model reaches log lines and ledger keys, so it is bounded and charset-checked here rather than
    // wherever it is printed. Unlike the identifiers below it cannot be dropped: a request without a usable
    // model has nothing to relay.
    if !is_valid_model(&folded_model) {
        return Err(RelayError::InvalidModel);
    }
    // The header is the client's own statement of the session.
    let session_id = header_id(headers, "x-claude-code-session-id");
    let client = client_class(headers);
    Ok(RequestIdentity {
        session_id,
        agent_id: header_id(headers, "x-claude-code-agent-id")
            .unwrap_or_else(|| MAIN_AGENT.to_string()),
        parent_agent_id: header_id(headers, "x-claude-code-parent-agent-id"),
        owner_id: header_id(headers, "x-keepalive-owner-id"),
        model,
        canonical_model: folded_model.clone(),
        folded_model,
        class: ClientClass::of_label(&client),
        client,
        forked_from: None,
        fork_depth: None,
    })
}

/// Fixes `session_id` for the key's `affinity_source`, and `forked_from` and
/// `fork_depth` for a request with a session id; reads `canonical_model`. `message_boundaries` are the request's own, from
/// `BodyFacts.chains` (empty for a body without a chain): the fork detection's input, which `RequestIdentity` does
/// not retain.
pub fn climb(
    identity: &mut RequestIdentity,
    key: &ApiKeyRow,
    message_boundaries: &[String],
    lookup: &dyn ConversationLookup,
) {
    let canonical_model = identity.canonical_model.clone();
    // The relay has no incarnation reading of its own; the `RouterHandle` implementation substitutes the
    // snapshot's, so a raw lookup only sees this when it is not that handle.
    let no_incarnation = |_: &str| None;
    let ask = |parent_ids: &[String], boundaries: &[String]| {
        // A boundary-less, parent-less call would answer nothing anyway; skipped so the lookup is never
        // reached over an empty slice.
        if parent_ids.is_empty() && boundaries.is_empty() {
            return None;
        }
        lookup.conversation(&ConversationQuery {
            canonical_model: &canonical_model,
            key_id: &key.id,
            key_generation: key.created_at,
            parent_ids,
            message_boundaries: boundaries,
            incarnation_of: &no_incarnation,
        })
    };

    // A fixed source other than `Session` runs its own rung alone, so rung 1's tentative guess never leaks
    // through as the resolved identity.
    if !matches!(
        key.affinity_source,
        AffinitySource::Auto | AffinitySource::Session
    ) {
        identity.session_id = None;
    }

    // A session that has no pin of its own is new to the map, so its deepest own-key match names the
    // conversation it branched from; the session id keeps the identity.
    if identity.session_id.is_some() {
        if let Some(m) = ask(&[], message_boundaries) {
            if identity.session_id.as_deref() != Some(m.root.as_str()) {
                identity.forked_from = Some(m.root);
                identity.fork_depth = m.depth;
            }
        }
    }
}

/// `claude-code/<entrypoint>` for a `claude-cli/` user agent (the token `entrypoint_of` reads, `cli` when it
/// reads none), `other` for anything else. The entrypoint is bounded by `entrypoint_of`, so the class is safe
/// for a log line and a ledger key.
pub fn client_class(headers: &HeaderMap) -> String {
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if user_agent.starts_with("claude-cli/") {
        return format!("claude-code/{}", entrypoint_of(user_agent));
    }
    OTHER_CLIENT.to_string()
}

/// Brackets are allowed on top of the identifier charset because a client may name a tier with them, as the
/// long-context suffix does.
fn is_valid_model(value: &str) -> bool {
    (1..=MAX_ID_LEN).contains(&value.len())
        && value.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'/' | b':' | b'[' | b']' | b'_')
        })
}

pub(crate) fn is_valid_id(value: &str) -> bool {
    (1..=MAX_ID_LEN).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'/' | b':' | b'_'))
}

/// A malformed identifier is treated as absent: it is a label, never an authorization decision, so a caller
/// cannot gain anything by sending one the relay would otherwise have to reject the whole request over.
pub(crate) fn header_id(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| is_valid_id(value))
        .map(str::to_string)
}

/// Decodes a string member of the top-level object without re-serializing the body around it.
fn string_member(body: &[u8], path: &[&str]) -> Option<String> {
    let (start, end) = phoebis_cch::string_member_span(body, path)?;
    serde_json::from_slice::<String>(&body[start..end]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &[u8] = br#"{"model":"claude-sonnet-4-5[1M]","metadata":{"user_id":"{\"account_uuid\":\"\",\"session_id\":\"s-1\"}"}}"#;

    #[test]
    fn the_model_suffix_is_folded_and_the_metadata_session_is_not_read() {
        let identity = identify(&HeaderMap::new(), BODY).expect("the body has a model");
        assert_eq!(identity.session_id, None);
        assert_eq!(identity.agent_id, "main");
        assert_eq!(identity.folded_model, "claude-sonnet-4-5");
        assert_eq!(identity.canonical_model, "claude-sonnet-4-5");
        assert_eq!(identity.model, "claude-sonnet-4-5[1M]");
    }

    #[test]
    fn a_model_outside_the_charset_or_over_the_bound_is_rejected() {
        let euro = br#"{"model":"claude-sonnet-4-5\u20ac\u20ac"}"#;
        assert!(matches!(
            identify(&HeaderMap::new(), euro),
            Err(RelayError::InvalidModel)
        ));
        let newline = br#"{"model":"claude\nsonnet"}"#;
        assert!(matches!(
            identify(&HeaderMap::new(), newline),
            Err(RelayError::InvalidModel)
        ));
        let long = format!(r#"{{"model":"{}"}}"#, "m".repeat(257));
        assert!(matches!(
            identify(&HeaderMap::new(), long.as_bytes()),
            Err(RelayError::InvalidModel)
        ));
        let empty = br#"{"model":""}"#;
        assert!(matches!(
            identify(&HeaderMap::new(), empty),
            Err(RelayError::InvalidModel)
        ));
    }

    #[test]
    fn a_malformed_header_id_is_absent_and_a_bodyless_model_is_an_error() {
        let mut headers = HeaderMap::new();
        headers.insert("x-claude-code-agent-id", "bad id!".parse().unwrap());
        headers.insert("x-claude-code-session-id", "s-2".parse().unwrap());
        let identity = identify(&headers, BODY).expect("the body has a model");
        assert_eq!(identity.agent_id, "main");
        assert_eq!(identity.session_id.as_deref(), Some("s-2"));
        assert!(matches!(
            identify(&headers, b"{\"messages\":[]}"),
            Err(RelayError::NoModel)
        ));
    }

    #[test]
    fn the_client_class_reads_the_entrypoint_or_other() {
        let agent = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(axum::http::header::USER_AGENT, value.parse().unwrap());
            headers
        };
        assert_eq!(
            client_class(&agent("claude-cli/2.1.269 (external, sdk-cli)")),
            "claude-code/sdk-cli"
        );
        assert_eq!(
            client_class(&agent("claude-cli/2.1.269")),
            "claude-code/cli"
        );
        assert_eq!(
            client_class(&agent("claude-cli/2.1.269;x (external, cli; y)")),
            "claude-code/cli",
            "the class is a label and never the version gate"
        );
        let over_bound = format!(
            "claude-cli/2.1.269 (external, {})",
            "a".repeat(MAX_ID_LEN + 1)
        );
        assert_eq!(
            client_class(&agent(&over_bound)),
            "claude-code/cli",
            "an entrypoint token past the bound reads as unknown rather than widening the class"
        );
        let mut headers = agent("anthropic-sdk-python/0.40.0");
        headers.insert("x-stainless-lang", "python".parse().unwrap());
        assert_eq!(client_class(&headers), OTHER_CLIENT);
        assert_eq!(
            client_class(&agent("anthropic-sdk-python/0.40.0")),
            OTHER_CLIENT
        );
        let mut headers = agent("anthropic-sdk-python/0.40.0");
        headers.insert("x-stainless-lang", "py thon".parse().unwrap());
        assert_eq!(
            client_class(&headers),
            OTHER_CLIENT,
            "a stainless header on an agent outside Claude Code does not change the label"
        );
    }

    #[test]
    fn the_class_is_derived_from_the_same_label_the_client_field_carries() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::USER_AGENT,
            "claude-cli/2.1.269".parse().unwrap(),
        );
        let identity = identify(&headers, BODY).expect("the body has a model");
        assert_eq!(identity.class, ClientClass::ClaudeCode);
        assert_eq!(identity.client, "claude-code/cli");
        let identity = identify(&HeaderMap::new(), BODY).expect("the body has a model");
        assert_eq!(identity.class, ClientClass::Other);
    }

    // -- climb --

    use std::collections::HashMap;

    use phoebis_routing::ConversationMatch;
    use phoebis_store::RulesMode;

    fn key(source: AffinitySource) -> ApiKeyRow {
        ApiKeyRow {
            id: "k".into(),
            label: "k".into(),
            notes: None,
            pool_id: "main".into(),
            enabled: true,
            revoked_at: None,
            expires_at: None,
            model_rules: Vec::new(),
            model_rules_mode: RulesMode::Extend,
            long_context_twins: false,
            keepalive_opt_out: false,
            body_capture_hours: None,
            secret_digest: Vec::new(),
            prefix: "k".into(),
            created_at: 0,
            rotated_at: None,
            last_used_at: None,
            retired_secret_last_used_at: None,
            in_flight_cap: None,
            affinity_source: source,
        }
    }

    fn identity_of(
        session_id: Option<&str>,
    ) -> RequestIdentity {
        RequestIdentity {
            session_id: session_id.map(String::from),
            agent_id: "main".into(),
            parent_agent_id: None,
            owner_id: None,
            model: "claude-sonnet-4-5".into(),
            folded_model: "claude-sonnet-4-5".into(),
            canonical_model: "claude-sonnet-4-5".into(),
            client: "claude-code/cli".into(),
            class: ClientClass::ClaudeCode,
            forked_from: None,
            fork_depth: None,
        }
    }

    fn matched(root: &str, depth: Option<u32>) -> ConversationMatch {
        ConversationMatch {
            root: root.into(),
            depth,
        }
    }

    /// A `HashMap`-backed lookup, filtered by the owning key the way the real map filters by key id.
    struct Stub(HashMap<&'static str, (&'static str, ConversationMatch)>);

    impl ConversationLookup for Stub {
        fn conversation(&self, query: &ConversationQuery<'_>) -> Option<ConversationMatch> {
            query.message_boundaries.iter().rev().find_map(|b| {
                self.0
                    .get(b.as_str())
                    .filter(|(owner, _)| *owner == query.key_id)
                    .map(|(_, m)| m.clone())
            })
        }
    }

    #[test]
    fn a_session_header_names_the_conversation() {
        let mut identity = identity_of(Some("s1"));
        let k = key(AffinitySource::Auto);
        climb(&mut identity, &k, &[], &Stub(HashMap::new()));
        assert_eq!(identity.session_id.as_deref(), Some("s1"));
        assert_eq!(identity.forked_from, None);
        assert_eq!(identity.fork_depth, None);
    }

    #[test]
    fn an_empty_boundary_slice_never_reaches_the_lookup() {
        struct Panicking;
        impl ConversationLookup for Panicking {
            fn conversation(&self, _: &ConversationQuery<'_>) -> Option<ConversationMatch> {
                panic!("the lookup must never be asked with an empty boundary slice")
            }
        }
        let mut identity = identity_of(Some("s1"));
        let k = key(AffinitySource::Content);
        climb(&mut identity, &k, &[], &Panicking);
        assert_eq!(identity.forked_from, None);
    }

    #[test]
    fn a_session_request_whose_messages_continue_another_conversation_is_a_fork_of_it() {
        let mut identity = identity_of(Some("s1"));
        let k = key(AffinitySource::Auto);
        let mut map = HashMap::new();
        map.insert("b3", ("k", matched("s0", Some(3))));
        climb(
            &mut identity,
            &k,
            &["b1".into(), "b2".into(), "b3".into()],
            &Stub(map),
        );
        assert_eq!(identity.session_id.as_deref(), Some("s1"));
        assert_eq!(identity.forked_from.as_deref(), Some("s0"));
        assert_eq!(identity.fork_depth, Some(3));
    }

    #[test]
    fn a_session_requests_own_continuation_is_no_fork() {
        let mut identity = identity_of(Some("s1"));
        let k = key(AffinitySource::Auto);
        let mut map = HashMap::new();
        map.insert("b3", ("k", matched("s1", Some(3))));
        climb(
            &mut identity,
            &k,
            &["b1".into(), "b2".into(), "b3".into()],
            &Stub(map),
        );
        assert_eq!(identity.forked_from, None);
        assert_eq!(identity.fork_depth, None);
    }
}
