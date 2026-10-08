//! Who and what a request is: the session, the agent, the owner, and the model it asks for.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use axum::http::HeaderMap;
use phoebis_bus::OTHER_CLIENT;
use phoebis_routing::{ClientClass, ConversationLookup, ConversationQuery};
use phoebis_store::ApiKeyRow;

use crate::billing::entrypoint_of;
use crate::RelayError;

/// The agent a request without an agent header belongs to.
const MAIN_AGENT: &str = "main";

/// Marker a downstream client appends to ask for the long-context tier. It is a client-side routing hint, so
/// it is folded out of the canonical model the registry resolves against.
const LONG_CONTEXT_SUFFIX: &str = "[1m]";

/// Bound on an identifier or model name the relay repeats into logs and ledger keys.
pub(crate) const MAX_ID_LEN: usize = 256;

/// The longest the climb waits for the previous answer's affinity report to reach the map: a client that
/// sends its next turn within milliseconds of the last chunk would otherwise be read against a map that lacks
/// its previous answer. Five milliseconds covers a queue the task is draining and costs a backlogged router
/// nothing more.
pub(crate) const PENDING_REPORT_WAIT: Duration = Duration::from_millis(5);

/// The entries `PendingReports` holds before an insert sweeps out the ones the watermark has passed.
const PENDING_REPORTS_SWEEP: usize = 4_096;

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

/// Walks the conversation climb: fixes
/// `forked_from` and `fork_depth` for a request with a session id; reads `canonical_model`. `message_boundaries` are the
/// request's own, from `BodyFacts.chains` (empty for a body without a chain): the fork detection's input, which
/// `RequestIdentity` does not retain.
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
    let ask = |boundaries: &[String]| {
        // A boundary-less call would answer nothing anyway, so the lookup is never asked over an empty slice.
        if boundaries.is_empty() {
            return None;
        }
        lookup.conversation(&ConversationQuery {
            canonical_model: &canonical_model,
            key_id: &key.id,
            key_generation: key.created_at,
            message_boundaries: boundaries,
            incarnation_of: &no_incarnation,
        })
    };

    // A session that has no pin of its own is new to the map, so its deepest own-key match names the
    // conversation it branched from; the session id keeps the identity.
    if identity.session_id.is_some() {
        if let Some(m) = ask(message_boundaries) {
            if identity.session_id.as_deref() != Some(m.root.as_str()) {
                identity.forked_from = Some(m.root);
                identity.fork_depth = m.depth;
            }
        }
    }
}

/// Per conversation identifier, the sequence of the newest affinity report not yet seen applied, under the key id
/// and generation. The identifiers are `session:<id>`.
#[derive(Default)]
pub(crate) struct PendingReports {
    entries: Mutex<HashMap<(String, i64, String), u64>>,
}

impl PendingReports {
    /// Files `seq` under every identifier in `ids`, never lowering a stored sequence; past 4 096 entries an insert
    /// first drops every entry `applied` has passed.
    pub(crate) fn record(
        &self,
        key_id: &str,
        key_generation: i64,
        ids: &[String],
        seq: u64,
        applied: u64,
    ) {
        let mut entries = self.entries.lock().expect("pending reports lock poisoned");
        if entries.len() >= PENDING_REPORTS_SWEEP {
            entries.retain(|_, pending| *pending > applied);
        }
        for id in ids {
            // The sequence is taken at enqueue and filed here under another lock, so a later report's record can
            // land first; the stored maximum keeps the wait on the newest report.
            let stored = entries
                .entry((key_id.to_string(), key_generation, id.clone()))
                .or_insert(seq);
            *stored = (*stored).max(seq);
        }
    }

    /// The identifier and sequence the request waits on: the highest entry among `ids` above `applied`, an entry
    /// `applied` has passed removed on the way; `ids` is what `waits_on` answers for the request.
    pub(crate) fn pending(
        &self,
        key_id: &str,
        key_generation: i64,
        ids: &[String],
        applied: u64,
    ) -> Option<(String, u64)> {
        let mut entries = self.entries.lock().expect("pending reports lock poisoned");
        let mut highest: Option<(String, u64)> = None;
        for id in ids {
            let entry = (key_id.to_string(), key_generation, id.clone());
            let Some(&seq) = entries.get(&entry) else {
                continue;
            };
            if seq <= applied {
                entries.remove(&entry);
            } else if highest.as_ref().is_none_or(|(_, top)| seq > *top) {
                highest = Some((id.clone(), seq));
            }
        }
        highest
    }

    /// Removes the entry only while it still holds `seq`.
    pub(crate) fn remove_if(&self, key_id: &str, key_generation: i64, id: &str, seq: u64) {
        let mut entries = self.entries.lock().expect("pending reports lock poisoned");
        let entry = (key_id.to_string(), key_generation, id.to_string());
        if entries.get(&entry) == Some(&seq) {
            entries.remove(&entry);
        }
    }
}

/// The identifier a request's wait consults: `session:<id>` for its own session id, nothing without one.
pub(crate) fn waits_on(identity: &RequestIdentity, message_boundaries: &[String]) -> Vec<String> {
    identity
        .session_id
        .iter()
        .map(|session_id| format!("session:{session_id}"))
        .collect()
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

    fn key() -> ApiKeyRow {
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
            incarnation: 1,
            created_at: 0,
            updated_at: 0,
            rotated_at: None,
            last_used_at: None,
            retired_secret_last_used_at: None,
            auto_mode_server: None,
        }
    }

    fn identity_of(session_id: Option<&str>) -> RequestIdentity {
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
        let k = key();
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
        let k = key();
        climb(&mut identity, &k, &[], &Panicking);
        assert_eq!(identity.forked_from, None);
        assert_eq!(identity.fork_depth, None);
    }

    #[test]
    fn a_session_request_whose_messages_continue_another_conversation_is_a_fork_of_it() {
        let mut identity = identity_of(Some("s1"));
        let k = key();
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
        let k = key();
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

    #[test]
    fn a_record_never_lowers_a_sequence_and_a_completed_wait_keeps_a_newer_report() {
        let pending = PendingReports::default();
        let ids = vec!["session:s1".to_string()];
        pending.record("k", 0, &ids, 10, 0);
        assert_eq!(
            pending.pending("k", 0, &ids, 0),
            Some(("session:s1".to_string(), 10))
        );
        // A second response of the conversation lands while the next turn waits on the first one's report.
        pending.record("k", 0, &ids, 11, 0);
        pending.remove_if("k", 0, "session:s1", 10);
        assert_eq!(
            pending.pending("k", 0, &ids, 10),
            Some(("session:s1".to_string(), 11)),
            "the wait on 10 completed and left the newer report for the turn after"
        );
        pending.record("k", 0, &ids, 9, 0);
        assert_eq!(
            pending.pending("k", 0, &ids, 10),
            Some(("session:s1".to_string(), 11)),
            "a record that lands late never lowers the stored sequence"
        );
        assert_eq!(pending.pending("k", 0, &ids, 11), None);
        assert_eq!(
            pending.pending("k", 0, &ids, 0),
            None,
            "the lookup removed the entry the watermark had passed"
        );
    }

    #[test]
    fn a_request_waits_on_its_strongest_identifier_alone() {
        let boundaries = vec!["x".to_string(), "y".to_string()];
        assert_eq!(
            waits_on(&identity_of(Some("a")), &boundaries),
            vec!["session:a".to_string()]
        );
        assert!(waits_on(&identity_of(None), &[]).is_empty());

        let pending = PendingReports::default();
        pending.record(
            "k",
            0,
            &["session:a".to_string()],
            5,
            0,
        );
        let other_session = waits_on(&identity_of(Some("b")), &boundaries);
        assert_eq!(
            pending.pending("k", 0, &other_session, 0),
            None,
            "another session never waits on this session's report"
        );
    }
}
