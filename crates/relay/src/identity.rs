//! Who and what a request is: the session, the agent, the client process's instance, and the model it asks for.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use axum::http::HeaderMap;
use phoebis_bus::{Kind, OTHER_CLIENT};
use phoebis_cch::TopLevelMembers;
use phoebis_routing::{
    ClientClass, ConversationLookup, ConversationQuery, MAIN_AGENT, REFRESHING_CLASSES,
};

use crate::billing::{entrypoint_of, restorable_version};
use crate::facts::{AttributionLine, BodyFacts, ClientHints};
use crate::RelayError;

// The words of the contract's items, in the table's order: the refusal's reason, the request row's
// `client_contract` column and the routing log carry them.
pub(crate) const ITEM_CLIENT_VERSION: &str = "client_version";
pub(crate) const ITEM_SESSION_HEADER: &str = "session_header";
pub(crate) const ITEM_CLASS_HEADER: &str = "class_header";
pub(crate) const ITEM_ATTRIBUTION_LINE: &str = "attribution_line";
pub(crate) const ITEM_METADATA_USER_ID: &str = "metadata_user_id";
pub(crate) const ITEM_AGENT_ID_HEADER: &str = "agent_id_header";
pub(crate) const ITEM_COMPACTION_HEADER: &str = "compaction_header";

/// Bound on an identifier or model name the relay repeats into logs and ledger keys.
pub(crate) const MAX_ID_LEN: usize = 256;

/// The longest word of an upstream answer the relay files as sent.
pub const MAX_FILED_WORD_LEN: usize = 64;

/// The longest the climb waits for the previous answer's affinity report to reach the map: a client that
/// sends its next turn within milliseconds of the last chunk would otherwise be read against a map that lacks
/// its previous answer. Five milliseconds covers a queue the task is draining and costs a backlogged router
/// nothing more.
pub(crate) const PENDING_REPORT_WAIT: Duration = Duration::from_millis(5);

/// The entries `PendingReports` holds before an insert sweeps out the ones the watermark has passed.
const PENDING_REPORTS_SWEEP: usize = 4_096;

#[derive(Debug)]
pub struct RequestIdentity {
    /// The client's own session id from `x-claude-code-session-id`, which the pin, the session memory, the
    /// report wait and the record key hang on. `None` for a request the contract refuses before an account is
    /// chosen.
    pub session_id: Option<String>,
    pub agent_id: String,
    pub parent_agent_id: Option<String>,
    pub instance_id: Option<String>,
    pub model: String,
    /// The client's spelling with the `[1m]` suffix removed and ASCII lowercased: what the body carries
    /// upstream, and the spelling the catalog's published map is keyed on.
    pub folded_model: String,
    /// `folded_model`, or the dated id the catalog cache resolved it to: the routing, ledger, and pricing key.
    pub canonical_model: String,
    /// The user agent's entrypoint token for a `claude-cli/` agent (`cli` where it names none), `None` for any other
    /// client: what the selection's request and the pin carry, and what the cost model's regime prices by.
    pub entrypoint: Option<String>,
    /// `claude-code/<entrypoint>` or `other`: the request's label, which the gate reads before the body and the log
    /// and the row carry.
    pub client: String,
    /// The class the label maps to, which the gate admitted.
    pub class: ClientClass,
    /// For a request whose messages continue another conversation of the key and generation at depth 2 or
    /// deeper: that conversation's root (its session id) and the matched depth. The session id stays the
    /// identity. `None` for a session's own continuation, for a match at depth 1 and for an auxiliary request.
    pub forked_from: Option<String>,
    pub fork_depth: Option<u32>,
    /// `metadata.user_id` is a string member of the body whose escaped text carries the `account_uuid` member
    /// with a value that closes inside the string, as the splice locates both.
    pub has_user_id: bool,
}

/// Reads what the request says about itself, which `climb` completes with the fork facts.
pub fn identify(
    headers: &HeaderMap,
    members: &TopLevelMembers<'_>,
    client: Client,
) -> Result<RequestIdentity, RelayError> {
    let model = string_member(members, &["model"]).ok_or(RelayError::NoModel)?;
    let folded_model = phoebis_store::canonical_id(&model);
    // The model reaches log lines and ledger keys, so it is bounded and charset-checked here rather than
    // wherever it is printed. Unlike the identifiers below it cannot be dropped: a request without a usable
    // model has nothing to relay.
    if !is_valid_model(&folded_model) {
        return Err(RelayError::InvalidModel);
    }
    // The header is the client's own statement of the session, and the climb starts from it.
    let session_id = header_id(headers, "x-claude-code-session-id");
    Ok(RequestIdentity {
        session_id,
        agent_id: agent_header_id(headers, "x-claude-code-agent-id")
            .unwrap_or_else(|| MAIN_AGENT.to_string()),
        parent_agent_id: agent_header_id(headers, "x-claude-code-parent-agent-id"),
        instance_id: header_id(headers, phoebis_bus::INSTANCE_ID_HEADER),
        model,
        canonical_model: folded_model.clone(),
        folded_model,
        entrypoint: client.entrypoint,
        class: client.class,
        client: client.label,
        forked_from: None,
        fork_depth: None,
        has_user_id: members
            .string_member_span(&["metadata", "user_id"])
            .and_then(|user_id| crate::body::account_uuid_in(members.body(), user_id))
            .is_some(),
    })
}

/// The conversation climb: for a request with a session id, reads the fork facts, `forked_from` and `fork_depth`,
/// where the deepest own-key match under `canonical_model` has another conversation's root at depth 2 or deeper,
/// unless `request_class` is auxiliary.
/// `message_boundaries` are the request's own, from `BodyFacts.chains` (empty for a body without a chain): the fork
/// detection's input, which `RequestIdentity` does not retain.
pub fn climb(
    identity: &mut RequestIdentity,
    key_id: &str,
    key_generation: i64,
    message_boundaries: &[String],
    request_class: Option<&str>,
    lookup: &dyn ConversationLookup,
) {
    let Some(session_id) = identity.session_id.as_deref() else {
        return;
    };
    // A session that has no pin of its own is new to the map, so its deepest own-key match names the
    // conversation it branched from, and the session id keeps the identity. Depth 1 is the first user message
    // alone, which a template, a slash command's output or a repeated prompt shares across sessions, while depth
    // 2 includes the model's first answer and is a shared conversation. An auxiliary request names none: the
    // client's quota probe has one message every probe of the key shares.
    // A boundary-less call would answer nothing anyway, so the lookup is never asked over an empty slice.
    if is_auxiliary(request_class) || message_boundaries.is_empty() {
        return;
    }
    let Some(m) = lookup.conversation(&ConversationQuery {
        canonical_model: &identity.canonical_model,
        key_id,
        key_generation,
        message_boundaries,
    }) else {
        return;
    };
    if session_id != m.root && m.depth >= 2 {
        identity.forked_from = Some(m.root);
        identity.fork_depth = Some(m.depth);
    }
}

/// Per session id, the sequence of the newest affinity report not yet seen applied, under the key id and
/// generation.
#[derive(Default)]
pub(crate) struct PendingReports {
    entries: Mutex<HashMap<(String, i64, String), u64>>,
}

impl PendingReports {
    /// Files `seq` under `session_id`, never lowering a stored sequence. Past 4 096 entries an insert first drops
    /// every entry `applied` has passed.
    pub(crate) fn record(
        &self,
        key_id: &str,
        key_generation: i64,
        session_id: &str,
        seq: u64,
        applied: u64,
    ) {
        let mut entries = self.entries.lock().expect("pending reports lock poisoned");
        if entries.len() >= PENDING_REPORTS_SWEEP {
            entries.retain(|_, pending| *pending > applied);
        }
        // The sequence is taken at enqueue and filed here under another lock, so a later report's record can land
        // first. The stored maximum keeps the wait on the newest report.
        let stored = entries
            .entry((key_id.to_string(), key_generation, session_id.to_string()))
            .or_insert(seq);
        *stored = (*stored).max(seq);
    }

    /// The sequence the request waits on: the entry under `session_id` while it is above `applied`. An entry
    /// `applied` has passed is removed on the way.
    pub(crate) fn pending(
        &self,
        key_id: &str,
        key_generation: i64,
        session_id: &str,
        applied: u64,
    ) -> Option<u64> {
        let mut entries = self.entries.lock().expect("pending reports lock poisoned");
        let entry = (key_id.to_string(), key_generation, session_id.to_string());
        let seq = *entries.get(&entry)?;
        if seq <= applied {
            entries.remove(&entry);
            return None;
        }
        Some(seq)
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

/// Whether the class the request named is auxiliary for identity: a class outside `REFRESHING_CLASSES`, the
/// client's quota probe among them. A request without the header is not, so the identity and the warmth read one
/// notion of auxiliary.
pub(crate) fn is_auxiliary(request_class: Option<&str>) -> bool {
    request_class.is_some_and(|class| !REFRESHING_CLASSES.contains(&class))
}

/// The first item of the contract the request lacks, in the table's order, or `None`. The version and the
/// session header are judged for every kind, since a token count and a keepalive probe send both, and the five
/// body and class items for an inference request alone. `client_version` is the user agent's version as
/// `drift::client_version` reads it, judged against the client's own spelling. The class header is judged before the
/// two items that depend on it, so a request without it is never refused for them, and the line is required where
/// `system` is an array alone, so the client's quota probe (no `system`) is outside the item.
pub(crate) fn contract_gap(
    headers: &HeaderMap,
    identity: &RequestIdentity,
    facts: &BodyFacts,
    hints: &ClientHints,
    client_version: Option<&str>,
    kind: Kind,
) -> Option<&'static str> {
    if client_version.and_then(restorable_version).is_none() {
        return Some(ITEM_CLIENT_VERSION);
    }
    if identity.session_id.is_none() {
        return Some(ITEM_SESSION_HEADER);
    }
    if kind != Kind::Inference {
        return None;
    }
    let Some(class) = hints.request_class.as_deref() else {
        return Some(ITEM_CLASS_HEADER);
    };
    if facts.line == AttributionLine::Missing {
        return Some(ITEM_ATTRIBUTION_LINE);
    }
    if !identity.has_user_id {
        return Some(ITEM_METADATA_USER_ID);
    }
    if matches!(class, "subagent" | "workflow")
        && agent_header_id(headers, "x-claude-code-agent-id").is_none()
    {
        return Some(ITEM_AGENT_ID_HEADER);
    }
    if class == "compaction" && hints.compaction.is_none() {
        return Some(ITEM_COMPACTION_HEADER);
    }
    None
}

/// Whether the restoration fills `cc_prev_req`: on the conversation classes the client's own builder passes the
/// previous request id, and on an `auxiliary` request that carries an agent id or a prompt id, the
/// conversation-shaped forks. Never on a side query, whose native line carries none.
pub(crate) fn restores_prev_req(request_class: Option<&str>, headers: &HeaderMap) -> bool {
    match request_class {
        Some("main" | "subagent" | "workflow" | "compaction") => true,
        Some("auxiliary") => {
            agent_header_id(headers, "x-claude-code-agent-id").is_some()
                || crate::facts::header_prompt_id(headers).is_some()
        }
        _ => false,
    }
}

/// The client a request's user agent names, read off the agent's prefix once.
pub(crate) struct Client {
    pub class: ClientClass,
    /// The token `entrypoint_of` reads (`cli` when it reads none) for a `claude-cli/` agent, `None` for any other.
    pub entrypoint: Option<String>,
    /// `claude-code/<entrypoint>` for a `claude-cli/` agent, `other` for anything else. The entrypoint is bounded by
    /// `entrypoint_of`, so the label is safe for a log line and a ledger key.
    pub label: String,
}

pub(crate) fn client_of(headers: &HeaderMap) -> Client {
    match user_agent_of(headers).filter(|agent| agent.starts_with(CLAUDE_CLI_PREFIX)) {
        Some(agent) => {
            let entrypoint = entrypoint_of(agent);
            Client {
                class: ClientClass::ClaudeCode,
                entrypoint: Some(entrypoint.to_string()),
                label: format!("claude-code/{entrypoint}"),
            }
        }
        None => Client {
            class: ClientClass::Other,
            entrypoint: None,
            label: OTHER_CLIENT.to_string(),
        },
    }
}

/// What a Claude Code user agent starts with, ahead of the client's version.
pub(crate) const CLAUDE_CLI_PREFIX: &str = "claude-cli/";

/// The `User-Agent` header as text, `None` where the request carries none or it is not visible ASCII.
pub(crate) fn user_agent_of(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
}

/// Brackets are allowed on top of the identifier charset because a client may name a tier with them, as the
/// long-context suffix does.
fn is_valid_model(value: &str) -> bool {
    bounded_token(value, |b| identifier_byte(b) || matches!(b, b'[' | b']'))
}

/// The bytes an identifier read off a header may hold: ASCII letters and digits and `-`, `.`, `/`, `:`, `_`.
pub fn identifier_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'/' | b':' | b'_')
}

/// Whether `value` is an identifier the relay reads off a header: at least one byte and at most `MAX_ID_LEN`, each an
/// `identifier_byte`. The keepalive checks the ids a companion reports through it, so a report and a request name one
/// session alike.
pub fn is_valid_id(value: &str) -> bool {
    bounded_token(value, identifier_byte)
}

/// `is_valid_id` with the at sign as well: a teammate of an agent team names itself `<name>@<team>`. No other
/// identifier takes the at sign, since a key selector splits `<key id>@<generation>` at it.
pub fn is_valid_agent_id(value: &str) -> bool {
    bounded_token(value, |b| identifier_byte(b) || b == b'@')
}

/// Whether `value` is a word the relay files as the upstream sent it: one to `MAX_FILED_WORD_LEN` bytes, each an
/// `identifier_byte`. A word that fails reads `other` in a key, and it files `upstream_<status>` as a forwarded
/// failure's code and `error` as the code of a stream's `error` event.
pub fn is_filed_word(value: &str) -> bool {
    value.len() <= MAX_FILED_WORD_LEN && is_valid_id(value)
}

pub(crate) fn bounded_token(value: &str, allowed: impl Fn(u8) -> bool) -> bool {
    (1..=MAX_ID_LEN).contains(&value.len()) && value.bytes().all(allowed)
}

pub(crate) fn header_id(headers: &HeaderMap, name: &str) -> Option<String> {
    header_id_under(headers, name, is_valid_id)
}

pub(crate) fn agent_header_id(headers: &HeaderMap, name: &str) -> Option<String> {
    header_id_under(headers, name, is_valid_agent_id)
}

/// A malformed identifier is treated as absent: it is a label, never an authorization decision, so a caller
/// cannot gain anything by sending one the relay would otherwise have to reject the whole request over.
fn header_id_under(headers: &HeaderMap, name: &str, valid: fn(&str) -> bool) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| valid(value))
        .map(str::to_string)
}

/// Decodes a string member of the top-level object without re-serializing the body around it.
fn string_member(members: &TopLevelMembers<'_>, path: &[&str]) -> Option<String> {
    let (start, end) = members.string_member_span(path)?;
    serde_json::from_slice::<String>(&members.body()[start..end]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &[u8] = br#"{"model":"claude-sonnet-4-5[1M]","metadata":{"user_id":"{\"account_uuid\":\"\",\"session_id\":\"s-1\"}"}}"#;

    #[test]
    fn the_model_suffix_is_folded_and_the_metadata_session_is_not_read() {
        let identity = identify(
            &HeaderMap::new(),
            &TopLevelMembers::read(BODY),
            client_of(&HeaderMap::new()),
        )
        .expect("the body has a model");
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
            identify(
                &HeaderMap::new(),
                &TopLevelMembers::read(euro),
                client_of(&HeaderMap::new())
            ),
            Err(RelayError::InvalidModel)
        ));
        let newline = br#"{"model":"claude\nsonnet"}"#;
        assert!(matches!(
            identify(
                &HeaderMap::new(),
                &TopLevelMembers::read(newline),
                client_of(&HeaderMap::new())
            ),
            Err(RelayError::InvalidModel)
        ));
        let long = format!(r#"{{"model":"{}"}}"#, "m".repeat(257));
        assert!(matches!(
            identify(
                &HeaderMap::new(),
                &TopLevelMembers::read(long.as_bytes()),
                client_of(&HeaderMap::new())
            ),
            Err(RelayError::InvalidModel)
        ));
        let empty = br#"{"model":""}"#;
        assert!(matches!(
            identify(
                &HeaderMap::new(),
                &TopLevelMembers::read(empty),
                client_of(&HeaderMap::new())
            ),
            Err(RelayError::InvalidModel)
        ));
    }

    #[test]
    fn a_malformed_header_id_is_absent_and_a_bodyless_model_is_an_error() {
        let mut headers = HeaderMap::new();
        headers.insert("x-claude-code-agent-id", "bad id!".parse().unwrap());
        headers.insert("x-claude-code-session-id", "s-2".parse().unwrap());
        let identity = identify(&headers, &TopLevelMembers::read(BODY), client_of(&headers))
            .expect("the body has a model");
        assert_eq!(identity.agent_id, "main");
        assert_eq!(identity.session_id.as_deref(), Some("s-2"));
        assert!(matches!(
            identify(
                &headers,
                &TopLevelMembers::read(b"{\"messages\":[]}"),
                client_of(&headers)
            ),
            Err(RelayError::NoModel)
        ));
    }

    #[test]
    fn the_identity_carries_the_entrypoint_for_a_claude_cli_agent_and_none_for_any_other() {
        let agent = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(axum::http::header::USER_AGENT, value.parse().unwrap());
            headers
        };
        let entrypoint = |headers: &HeaderMap| {
            identify(headers, &TopLevelMembers::read(BODY), client_of(headers))
                .expect("the body has a model")
                .entrypoint
        };
        assert_eq!(
            entrypoint(&agent(
                "claude-cli/2.1.295 (external, claude-desktop-3p, agent-sdk/0.3.295)"
            ))
            .as_deref(),
            Some("claude-desktop-3p")
        );
        assert_eq!(
            entrypoint(&agent("claude-cli/2.1.269")).as_deref(),
            Some("cli"),
            "an agent that names no entrypoint is the interactive one"
        );
        let mut sdk = agent("anthropic-sdk-python/0.40.0");
        sdk.insert("x-stainless-lang", "python".parse().unwrap());
        assert_eq!(entrypoint(&sdk), None);
        assert_eq!(entrypoint(&HeaderMap::new()), None, "no user agent at all");
    }

    #[test]
    fn the_class_is_derived_from_the_same_label_the_client_field_carries() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::USER_AGENT,
            "claude-cli/2.1.269".parse().unwrap(),
        );
        let identity = identify(&headers, &TopLevelMembers::read(BODY), client_of(&headers))
            .expect("the body has a model");
        assert_eq!(identity.class, ClientClass::ClaudeCode);
        assert_eq!(identity.client, "claude-code/cli");
        let identity = identify(
            &HeaderMap::new(),
            &TopLevelMembers::read(BODY),
            client_of(&HeaderMap::new()),
        )
        .expect("the body has a model");
        assert_eq!(identity.class, ClientClass::Other);
    }

    // -- contract_gap: the items a Claude Code request carries, two of them on a token count and a probe --

    const LINE: &str = "x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=cli;";

    /// A body that carries every item of the contract: the line at `system[0]` and `metadata.user_id`.
    fn whole_body() -> serde_json::Value {
        serde_json::json!({
            "model": "claude-sonnet-5",
            "system": [
                {"type": "text", "text": LINE},
                {"type": "text", "text": "You are Claude Code."}
            ],
            "messages": [{"role": "user", "content": "hi"}],
            "metadata": {"user_id": "{\"account_uuid\":\"\",\"session_id\":\"s-1\"}"}
        })
    }

    fn without_line(mut body: serde_json::Value) -> serde_json::Value {
        body["system"].as_array_mut().unwrap().remove(0);
        body
    }

    fn without_metadata(mut body: serde_json::Value) -> serde_json::Value {
        body.as_object_mut().unwrap().remove("metadata");
        body
    }

    /// The session header and, where `class` names one, the class header, under a `claude-cli/` agent.
    fn contract_headers(class: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::USER_AGENT,
            "claude-cli/2.1.269 (external, cli)".parse().unwrap(),
        );
        headers.insert("x-claude-code-session-id", "s-1".parse().unwrap());
        if let Some(class) = class {
            headers.insert("x-claude-code-request-class", class.parse().unwrap());
        }
        headers
    }

    /// The gap of a request of `kind` as `handle` reads it: the identity and the body facts off the one body, the
    /// hints through `ClientHints::read` and the version through `client_version`.
    fn gap_of(headers: &HeaderMap, body: &serde_json::Value, kind: Kind) -> Option<&'static str> {
        let bytes = serde_json::to_vec(body).unwrap();
        let members = TopLevelMembers::read(&bytes);
        let identity =
            identify(headers, &members, client_of(headers)).expect("the body has a model");
        let (facts, _) = crate::facts::BodyFacts::read_with_tree(&members);
        let hints = ClientHints::read(headers);
        let version = crate::drift::client_version(headers);
        contract_gap(headers, &identity, &facts, &hints, version.as_deref(), kind)
    }

    /// The gap of an inference request.
    fn gap(headers: &HeaderMap, body: &serde_json::Value) -> Option<&'static str> {
        gap_of(headers, body, Kind::Inference)
    }

    #[test]
    fn contract_gap_names_the_version_first_and_judges_a_count_and_a_probe_for_the_two_header_items(
    ) {
        let mut misspelled = contract_headers(Some("main"));
        misspelled.insert(
            axum::http::header::USER_AGENT,
            "claude-cli/2.1.269;x=y (external, cli)".parse().unwrap(),
        );
        misspelled.remove("x-claude-code-session-id");
        assert_eq!(
            gap(&misspelled, &whole_body()),
            Some(ITEM_CLIENT_VERSION),
            "the version is judged before the session header"
        );
        let prerelease = {
            let mut h = contract_headers(Some("main"));
            h.insert(
                axum::http::header::USER_AGENT,
                "claude-cli/2.1.269-beta.1 (external, cli)".parse().unwrap(),
            );
            h
        };
        assert_eq!(gap(&prerelease, &whole_body()), None);
        let count_body = serde_json::json!({
            "model": "claude-sonnet-5",
            "messages": [{"role": "user", "content": "hi"}]
        });
        for kind in [Kind::CountTokens, Kind::Probe] {
            assert_eq!(
                gap_of(&contract_headers(None), &count_body, kind),
                None,
                "{kind:?} needs no class, line or member"
            );
            let mut sessionless = contract_headers(None);
            sessionless.remove("x-claude-code-session-id");
            assert_eq!(
                gap_of(&sessionless, &count_body, kind),
                Some(ITEM_SESSION_HEADER),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn contract_gap_names_the_first_missing_item_in_the_tables_order() {
        let main = contract_headers(Some("main"));
        assert_eq!(gap(&main, &whole_body()), None, "every item is present");
        let mut sessionless = main.clone();
        sessionless.remove("x-claude-code-session-id");
        assert_eq!(
            gap(&sessionless, &whole_body()),
            Some(ITEM_SESSION_HEADER),
            "the body's session never stands in for the header"
        );
        assert_eq!(
            gap(&contract_headers(None), &whole_body()),
            Some(ITEM_CLASS_HEADER)
        );
        assert_eq!(
            gap(&main, &without_line(whole_body())),
            Some(ITEM_ATTRIBUTION_LINE)
        );
        assert_eq!(
            gap(&main, &without_metadata(whole_body())),
            Some(ITEM_METADATA_USER_ID)
        );
        let mut numbered = whole_body();
        numbered["metadata"]["user_id"] = serde_json::json!(7);
        assert_eq!(
            gap(&main, &numbered),
            Some(ITEM_METADATA_USER_ID),
            "a member that is not a string is not the member"
        );
        let mut empty = whole_body();
        empty["metadata"]["user_id"] = serde_json::json!("");
        assert_eq!(
            gap(&main, &empty),
            Some(ITEM_METADATA_USER_ID),
            "an empty string carries no account_uuid"
        );
        let mut keyless = whole_body();
        keyless["metadata"]["user_id"] = serde_json::json!("{\"session_id\":\"s-1\"}");
        assert_eq!(
            gap(&main, &keyless),
            Some(ITEM_METADATA_USER_ID),
            "a string without the account_uuid member is not the member"
        );
        let mut blank_uuid = whole_body();
        blank_uuid["metadata"]["user_id"] = serde_json::json!(
            "{\"device_id\":\"d-1\",\"account_uuid\":\"\",\"session_id\":\"s-1\"}"
        );
        assert_eq!(
            gap(&main, &blank_uuid),
            None,
            "the client writes an empty account_uuid when it holds no OAuth account"
        );
        assert_eq!(
            gap(&contract_headers(Some("subagent")), &whole_body()),
            Some(ITEM_AGENT_ID_HEADER)
        );
        assert_eq!(
            gap(&contract_headers(Some("compaction")), &whole_body()),
            Some(ITEM_COMPACTION_HEADER)
        );
        let mut bare = HeaderMap::new();
        bare.insert(
            axum::http::header::USER_AGENT,
            "claude-cli/2.1.269 (external, cli)".parse().unwrap(),
        );
        assert_eq!(
            gap(&bare, &without_metadata(without_line(whole_body()))),
            Some(ITEM_SESSION_HEADER),
            "two missing answer the lower-numbered"
        );
        assert_eq!(
            gap(&contract_headers(None), &without_line(whole_body())),
            Some(ITEM_CLASS_HEADER)
        );
        assert_eq!(
            gap(&main, &without_metadata(without_line(whole_body()))),
            Some(ITEM_ATTRIBUTION_LINE)
        );
        assert_eq!(
            gap(
                &contract_headers(Some("subagent")),
                &without_metadata(whole_body())
            ),
            Some(ITEM_METADATA_USER_ID)
        );
    }

    #[test]
    fn contract_gap_requires_the_line_where_system_is_an_array_alone() {
        let main = contract_headers(Some("auxiliary"));
        let mut systemless = whole_body();
        systemless.as_object_mut().unwrap().remove("system");
        assert_eq!(
            gap(&main, &systemless),
            None,
            "the quota probe's shape has no slot for the line"
        );
        let mut string = whole_body();
        string["system"] = serde_json::json!("You are Claude Code.");
        assert_eq!(gap(&main, &string), None, "a string carries no slot either");
        assert_eq!(
            gap(&main, &without_line(whole_body())),
            Some(ITEM_ATTRIBUTION_LINE),
            "the opt-out shape: the CLI prompt at system[0]"
        );
        let mut stamped = whole_body();
        stamped["system"][0]["text"] = serde_json::json!(
            "x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=cli; cch=12345;"
        );
        assert_eq!(
            gap(&main, &stamped),
            None,
            "a stamped line is the item, judged for its stamp afterward"
        );
    }

    #[test]
    fn contract_gap_judges_the_class_word_for_presence_alone() {
        for class in ["auxiliary", "main", "side_reply", "something_new"] {
            assert_eq!(
                gap(&contract_headers(Some(class)), &whole_body()),
                None,
                "{class}"
            );
        }
    }

    #[test]
    fn contract_gap_requires_the_agent_id_under_subagent_and_workflow_and_the_compaction_header_under_compaction(
    ) {
        for class in ["subagent", "workflow"] {
            let mut headers = contract_headers(Some(class));
            assert_eq!(
                gap(&headers, &whole_body()),
                Some(ITEM_AGENT_ID_HEADER),
                "{class}"
            );
            headers.insert("x-claude-code-agent-id", "agent-1".parse().unwrap());
            assert_eq!(gap(&headers, &whole_body()), None, "{class}");
        }
        let mut headers = contract_headers(Some("compaction"));
        headers.insert("x-claude-code-agent-id", "agent-1".parse().unwrap());
        assert_eq!(
            gap(&headers, &whole_body()),
            Some(ITEM_COMPACTION_HEADER),
            "the agent id stands in for nothing"
        );
        headers.insert("x-claude-code-compaction", "1".parse().unwrap());
        assert_eq!(gap(&headers, &whole_body()), None);
        assert_eq!(
            gap(&contract_headers(Some("main")), &whole_body()),
            None,
            "the main thread names no agent and no compaction"
        );
    }

    #[test]
    fn a_teammates_id_with_an_at_sign_reads_at_each_agent_id_header() {
        let teammate = "twin@session-22222222";
        let mut headers = contract_headers(Some("subagent"));
        headers.insert("x-claude-code-agent-id", teammate.parse().unwrap());
        assert_eq!(gap(&headers, &whole_body()), None, "the contract");
        let identity = identify(&headers, &TopLevelMembers::read(BODY), client_of(&headers))
            .expect("the body has a model");
        assert_eq!(identity.agent_id, teammate, "the agent");
        assert!(
            restores_prev_req(Some("auxiliary"), &headers),
            "an auxiliary fork"
        );
        let mut subagent = HeaderMap::new();
        subagent.insert("x-claude-code-parent-agent-id", teammate.parse().unwrap());
        let identity = identify(
            &subagent,
            &TopLevelMembers::read(BODY),
            client_of(&subagent),
        )
        .expect("the body has a model");
        assert_eq!(
            identity.parent_agent_id.as_deref(),
            Some(teammate),
            "the parent"
        );
    }

    #[test]
    fn contract_gap_reads_a_malformed_session_header_as_absent() {
        for malformed in [
            "bad id!".to_string(),
            "s".repeat(MAX_ID_LEN + 1),
            "twin@session-22222222".to_string(),
        ] {
            let mut headers = contract_headers(Some("main"));
            headers.insert("x-claude-code-session-id", malformed.parse().unwrap());
            assert_eq!(
                gap(&headers, &whole_body()),
                Some(ITEM_SESSION_HEADER),
                "{malformed}"
            );
        }
    }

    // -- restores_prev_req: cc_prev_req by class --

    #[test]
    fn restores_prev_req_answers_the_table_by_class() {
        let headers = |agent_id: bool, prompt_id: bool| {
            let mut headers = HeaderMap::new();
            if agent_id {
                headers.insert("x-claude-code-agent-id", "agent-1".parse().unwrap());
            }
            if prompt_id {
                headers.insert(
                    "x-claude-code-prompt-id",
                    "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".parse().unwrap(),
                );
            }
            headers
        };
        // The class, the agent-id header, the prompt-id header, and the answer.
        let table = [
            (Some("main"), false, false, true),
            (Some("subagent"), false, false, true),
            (Some("workflow"), false, false, true),
            (Some("compaction"), false, false, true),
            (Some("auxiliary"), false, false, false),
            (Some("auxiliary"), true, false, true),
            (Some("auxiliary"), false, true, true),
            (Some("side_reply"), true, true, false),
            (Some("something_new"), true, true, false),
            (Some("side_reply"), true, false, false),
            (Some("something_new"), false, true, false),
            (None, true, true, false),
        ];
        for (class, agent_id, prompt_id, restored) in table {
            assert_eq!(
                restores_prev_req(class, &headers(agent_id, prompt_id)),
                restored,
                "{class:?}, agent id {agent_id}, prompt id {prompt_id}"
            );
        }
    }

    // -- climb --

    use std::collections::HashMap;

    use phoebis_routing::{ConversationMatch, PredecessorQuery};

    fn identity_of(session_id: Option<&str>) -> RequestIdentity {
        RequestIdentity {
            session_id: session_id.map(String::from),
            agent_id: "main".into(),
            parent_agent_id: None,
            instance_id: None,
            model: "claude-sonnet-4-5".into(),
            folded_model: "claude-sonnet-4-5".into(),
            canonical_model: "claude-sonnet-4-5".into(),
            entrypoint: Some("cli".into()),
            client: "claude-code/cli".into(),
            class: ClientClass::ClaudeCode,
            forked_from: None,
            fork_depth: None,
            has_user_id: true,
        }
    }

    fn matched(root: &str, depth: u32) -> ConversationMatch {
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

        /// The climb never asks for the answer a request continues.
        fn predecessor(&self, _: &PredecessorQuery<'_>) -> Option<String> {
            None
        }
    }

    /// A lookup the climb under test must never ask: the case fails the moment it does.
    struct Panicking;

    impl ConversationLookup for Panicking {
        fn conversation(&self, _: &ConversationQuery<'_>) -> Option<ConversationMatch> {
            panic!("the lookup must never be asked on this climb")
        }

        fn predecessor(&self, _: &PredecessorQuery<'_>) -> Option<String> {
            panic!("the lookup must never be asked on this climb")
        }
    }

    #[test]
    fn a_session_requests_own_continuation_is_no_fork() {
        let mut identity = identity_of(Some("s1"));
        let mut map = HashMap::new();
        map.insert("b3", ("k", matched("s1", 3)));
        climb(
            &mut identity,
            "k",
            0,
            &["b1".into(), "b2".into(), "b3".into()],
            None,
            &Stub(map),
        );
        assert_eq!(identity.forked_from, None);
        assert_eq!(identity.fork_depth, None);
    }

    #[test]
    fn a_session_request_matching_another_conversation_at_depth_1_is_no_fork() {
        let mut identity = identity_of(Some("s1"));
        let mut map = HashMap::new();
        map.insert("b1", ("k", matched("s0", 1)));
        climb(&mut identity, "k", 0, &["b1".into()], None, &Stub(map));
        assert_eq!(identity.session_id.as_deref(), Some("s1"));
        assert_eq!(identity.forked_from, None);
        assert_eq!(identity.fork_depth, None);
    }

    #[test]
    fn a_session_request_matching_another_conversation_at_depth_2_is_a_fork() {
        let mut identity = identity_of(Some("s1"));
        let mut map = HashMap::new();
        map.insert("b2", ("k", matched("s0", 2)));
        climb(
            &mut identity,
            "k",
            0,
            &["b1".into(), "b2".into()],
            None,
            &Stub(map),
        );
        assert_eq!(identity.forked_from.as_deref(), Some("s0"));
        assert_eq!(identity.fork_depth, Some(2));
    }

    #[test]
    fn an_auxiliary_request_matching_another_conversation_at_depth_3_is_no_fork() {
        let boundaries = ["b1".to_string(), "b2".to_string(), "b3".to_string()];
        let mut identity = identity_of(Some("s1"));
        climb(
            &mut identity,
            "k",
            0,
            &boundaries,
            Some("auxiliary"),
            &Panicking,
        );
        assert_eq!(identity.session_id.as_deref(), Some("s1"));
        assert_eq!(identity.forked_from, None);
        assert_eq!(identity.fork_depth, None);

        let mut map = HashMap::new();
        map.insert("b3", ("k", matched("s0", 3)));
        let mut identity = identity_of(Some("s1"));
        climb(&mut identity, "k", 0, &boundaries, Some("main"), &Stub(map));
        assert_eq!(identity.forked_from.as_deref(), Some("s0"));
        assert_eq!(identity.fork_depth, Some(3));
    }

    #[test]
    fn a_record_never_lowers_a_sequence_and_a_completed_wait_keeps_a_newer_report() {
        let pending = PendingReports::default();
        pending.record("k", 0, "s1", 10, 0);
        assert_eq!(pending.pending("k", 0, "s1", 0), Some(10));
        assert_eq!(
            pending.pending("k", 0, "s2", 0),
            None,
            "another conversation's request waits on no report of this one"
        );
        // A second response of the conversation lands while the next turn waits on the first one's report.
        pending.record("k", 0, "s1", 11, 0);
        pending.remove_if("k", 0, "s1", 10);
        assert_eq!(
            pending.pending("k", 0, "s1", 10),
            Some(11),
            "the wait on 10 completed and left the newer report for the turn after"
        );
        pending.record("k", 0, "s1", 9, 0);
        assert_eq!(
            pending.pending("k", 0, "s1", 10),
            Some(11),
            "a record that lands late never lowers the stored sequence"
        );
        assert_eq!(pending.pending("k", 0, "s1", 11), None);
        assert_eq!(
            pending.pending("k", 0, "s1", 0),
            None,
            "the lookup removed the entry the watermark had passed"
        );
    }
}
