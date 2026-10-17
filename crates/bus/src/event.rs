//! The owned, serializable events the relay publishes. Nothing here borrows from the relay, so a consumer can
//! keep an event for as long as it likes without holding a request open.

use bytes::Bytes;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Client,
    Keepalive,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Client => "client",
            Origin::Keepalive => "keepalive",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    CountTokens,
    Inference,
    Probe,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::CountTokens => "count_tokens",
            Kind::Inference => "inference",
            Kind::Probe => "probe",
        }
    }
}

/// What a request had to do with auto mode. Admission reads `Server` or `Stripped` from the `safeguards` member
/// and the key's effective setting; the ledger reads `BodyShape` off a `body_shape` refusal before any dispatch,
/// whatever admission read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoMode {
    BodyShape,
    Server,
    Stripped,
}

impl AutoMode {
    pub fn as_str(self) -> &'static str {
        match self {
            AutoMode::BodyShape => "body_shape",
            AutoMode::Server => "server",
            AutoMode::Stripped => "stripped",
        }
    }
}

/// Who put `diagnostics.previous_message_id` on the body the upstream compared: the client's own member, or the
/// relay's insertion under the pool's `cache_diagnostics` switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticsSource {
    Client,
    Router,
}

impl DiagnosticsSource {
    pub fn as_str(self) -> &'static str {
        match self {
            DiagnosticsSource::Client => "client",
            DiagnosticsSource::Router => "router",
        }
    }
}

/// The relay's error code for a body refused for its shape. The ledger matches a completion's code against it to
/// record the `BodyShape` standing, so both crates read the one spelling rather than each writing its own.
pub const BODY_SHAPE_ERROR_CODE: &str = "body_shape";

/// Token counts as the upstream reported them. A field the upstream did not send stays `None`, never zero,
/// so a missing usage block is told apart from an empty one.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_5m_tokens: Option<u64>,
    pub cache_write_1h_tokens: Option<u64>,
    /// `output_tokens_details.thinking_tokens`: the share of `output_tokens` spent thinking, never more than it.
    pub thinking_tokens: Option<u64>,
    /// `server_tool_use.web_search_requests` and `web_fetch_requests`: calls, not tokens.
    pub web_search_requests: Option<u64>,
    pub web_fetch_requests: Option<u64>,
    pub service_tier: Option<String>,
    pub cache_miss_reason: Option<String>,
}

impl Usage {
    /// `message_delta` repeats the counts it knows with running values, so the later value replaces the
    /// earlier one field by field and a field only `message_start` carried survives. The write columns are
    /// the exception: a read that names the 5-minute column alone carries the bare total of the older wire
    /// shape, and a delta repeats that total for an answer whose start already gave the split, so the split
    /// stands or a 1-hour write would be counted a second time as a 5-minute one.
    pub fn merge(&mut self, later: &Usage) {
        if later.input_tokens.is_some() {
            self.input_tokens = later.input_tokens;
        }
        if later.output_tokens.is_some() {
            self.output_tokens = later.output_tokens;
        }
        if later.cache_read_tokens.is_some() {
            self.cache_read_tokens = later.cache_read_tokens;
        }
        let bare_total = later.cache_write_1h_tokens.is_none();
        if later.cache_write_5m_tokens.is_some()
            && !(bare_total && self.cache_write_1h_tokens.is_some())
        {
            self.cache_write_5m_tokens = later.cache_write_5m_tokens;
        }
        if later.cache_write_1h_tokens.is_some() {
            self.cache_write_1h_tokens = later.cache_write_1h_tokens;
        }
        if later.thinking_tokens.is_some() {
            self.thinking_tokens = later.thinking_tokens;
        }
        if later.web_search_requests.is_some() {
            self.web_search_requests = later.web_search_requests;
        }
        if later.web_fetch_requests.is_some() {
            self.web_fetch_requests = later.web_fetch_requests;
        }
        if later.service_tier.is_some() {
            self.service_tier = later.service_tier.clone();
        }
        if later.cache_miss_reason.is_some() {
            self.cache_miss_reason = later.cache_miss_reason.clone();
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == Usage::default()
    }
}

/// What the answer said about itself beyond its usage, read from `message_start` (the served model), from
/// `message_delta` (the stop, the context edits, the refusal), or from the whole JSON answer. A member the
/// upstream did not send stays `None`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResponseFacts {
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
    /// The `model` the answer names, as sent; the ledger records it only when it differs from the resolved one.
    pub model: Option<String>,
    pub speed: Option<String>,
    pub inference_geo: Option<String>,
    /// The sum of `context_management.applied_edits[].cleared_input_tokens`.
    pub cleared_input_tokens: Option<u64>,
    pub refusal_category: Option<String>,
    pub refusal_recommended_model: Option<String>,
}

impl ResponseFacts {
    /// A later read replaces an earlier one member by member, as `Usage::merge` does: `message_delta` names the
    /// stop that `message_start` could not, and neither unsays what the other already told.
    pub fn merge(&mut self, later: &ResponseFacts) {
        if later.stop_reason.is_some() {
            self.stop_reason = later.stop_reason.clone();
        }
        if later.stop_sequence.is_some() {
            self.stop_sequence = later.stop_sequence.clone();
        }
        if later.model.is_some() {
            self.model = later.model.clone();
        }
        if later.speed.is_some() {
            self.speed = later.speed.clone();
        }
        if later.inference_geo.is_some() {
            self.inference_geo = later.inference_geo.clone();
        }
        if later.cleared_input_tokens.is_some() {
            self.cleared_input_tokens = later.cleared_input_tokens;
        }
        if later.refusal_category.is_some() {
            self.refusal_category = later.refusal_category.clone();
        }
        if later.refusal_recommended_model.is_some() {
            self.refusal_recommended_model = later.refusal_recommended_model.clone();
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == ResponseFacts::default()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    FirstContentDelta,
    MessageDelta { usage: Usage },
    MessageStart { usage: Usage },
    MessageStop,
    ToolUseDelta,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttemptOutcome {
    AuthFailed,
    Canceled,
    RateLimited,
    Succeeded,
    TransportError,
    UpstreamError,
}

impl AttemptOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            AttemptOutcome::AuthFailed => "auth_failed",
            AttemptOutcome::Canceled => "canceled",
            AttemptOutcome::RateLimited => "rate_limited",
            AttemptOutcome::Succeeded => "succeeded",
            AttemptOutcome::TransportError => "transport_error",
            AttemptOutcome::UpstreamError => "upstream_error",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Canceled,
    Exhausted,
    Failed,
    /// A 200 answer whose body ended before its end; the error code says how.
    Incomplete,
    Rejected,
    Succeeded,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Canceled => "canceled",
            Outcome::Exhausted => "exhausted",
            Outcome::Failed => "failed",
            Outcome::Incomplete => "incomplete",
            Outcome::Rejected => "rejected",
            Outcome::Succeeded => "succeeded",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CooldownSet {
    pub scope: String,
    pub until: i64,
    pub reason: String,
    pub window: String,
}

/// What the body said about the request, read once at observation from the parse the relay performs for the
/// restoration. Absent members stay `None`; the counts are 0 for a body with no such member.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RequestShape {
    /// The effort the upstream applies: the `output_config.effort` of the last `messages` entry with role
    /// `system` that carries one, else the top-level `output_config.effort`.
    pub effort: Option<String>,
    /// `thinking.type` and `thinking.budget_tokens`.
    pub thinking_type: Option<String>,
    pub thinking_budget: Option<u64>,
    pub max_tokens: Option<u64>,
    pub tool_count: u32,
    pub system_blocks: u32,
    pub message_count: u32,
    /// `context_management.edits[].type`, in the body's order.
    pub context_edits: Vec<String>,
    pub temperature: Option<f64>,
    pub speed: Option<String>,
    pub service_tier: Option<String>,
    /// The body carries a top-level `safeguards` member, read through escapes.
    pub safeguards: bool,
}

/// The class of a caller that is not Claude Code: a request's label is `claude-code/<entrypoint>` or `other`.
pub const OTHER_CLIENT: &str = "other";

/// Who sent the request and what its hint headers and its attribution line said about the turn, read once at
/// admission. Every member but `class` is absent unless the client sent it.
#[derive(Clone, Debug, PartialEq)]
pub struct ClientFacts {
    /// `claude-code/<entrypoint>` for a `claude-cli/` user agent, else `other`.
    pub class: String,
    /// `x-claude-code-agent-type`, on a subagent's own turns.
    pub agent_type: Option<String>,
    /// `x-claude-code-compaction`, on the summarization request.
    pub compaction: Option<String>,
    /// `x-claude-code-context-compacted`, once on the first request after a compaction.
    pub context_compacted: Option<String>,
    /// The sum of `x-claude-code-prev-tool-durations` in milliseconds; the per-tool list is not kept.
    pub prev_tool_ms: Option<i64>,
    /// `x-claude-code-request-class`.
    pub request_class: Option<String>,
    /// `cc_turn_origin` off the attribution line, first-party only; never restored, since the relay cannot
    /// know who started the turn.
    pub turn_origin: Option<String>,
}

impl Default for ClientFacts {
    fn default() -> Self {
        ClientFacts {
            class: OTHER_CLIENT.into(),
            agent_type: None,
            compaction: None,
            context_compacted: None,
            prev_tool_ms: None,
            request_class: None,
            turn_origin: None,
        }
    }
}

/// One guardrail that matched the request, at its position in the key's list, `blocked` on the first enforcing one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuardrailMatch {
    pub id: String,
    pub position: u32,
    pub blocked: bool,
}

/// What the relay's fork detection read of the request: for one whose messages continue another conversation
/// of the key and generation at depth 2 or deeper, that conversation's root (its session id) and the matched
/// depth. Both `None` for a session's own continuation, a match at depth 1, an auxiliary request and a refusal
/// published before the climb ran.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Fork {
    pub forked_from: Option<String>,
    pub fork_depth: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct RequestObserved {
    pub request_id: String,
    pub origin: Origin,
    pub kind: Kind,
    pub key_id: Option<String>,
    /// The key's `created_at`, the generation every rollup row of this request is filed under; 0 with no key.
    pub key_generation: i64,
    pub pool_id: Option<String>,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub parent_agent_id: Option<String>,
    pub owner_id: Option<String>,
    pub model: String,
    /// The name the router records the model under: the dated id the catalog lists once the router has
    /// resolved the client's spelling, the folded spelling (the `[1m]` suffix removed, ASCII lowercased) until
    /// then.
    pub canonical_model: String,
    /// The spelling the body carries upstream: the client's `model` minus the `[1m]` suffix, ASCII lowercased.
    pub wire_model: String,
    pub stream: bool,
    pub headers: Vec<(String, String)>,
    /// The body as received: refcounted, so publishing costs no copy.
    pub body: Bytes,
    /// The key's effective capture window; zero means the ledger keeps no bodies for this request.
    pub body_capture_hours: u32,
    /// Empty for a refusal published before the body was read.
    pub shape: RequestShape,
    /// The static chain's last boundary, lowercase hex; `None` without a chain.
    pub static_hash: Option<String>,
    /// Every message boundary, lowercase hex; empty without a chain.
    pub boundaries: Vec<String>,
    /// The class and hint facts read at admission, structured; `headers` carries the same hint headers
    /// unstructured, as the caller spelled them.
    pub client: ClientFacts,
    /// Decided at admission from the body and the key's effective setting; `None` for a request that did not ask
    /// for a server-side check and for a refusal published before the body was read. The ledger records a
    /// `body_shape` refusal before any dispatch as `BodyShape` whatever this says.
    pub auto_mode: Option<AutoMode>,
    /// What the fork detection read, both members `None` for a refusal published before the climb ran.
    pub fork: Fork,
    /// The caller presented the secret a rotation retired.
    pub presented_retired: bool,
    /// The key's `rotated_at` at authentication, `None` for a key never rotated: which rotation a retired
    /// presentation belongs to.
    pub rotation_at: Option<i64>,
    /// The guardrail that blocked the request, `None` where none did or none was evaluated.
    pub guardrail_id: Option<String>,
    /// Every guardrail that matched, in evaluation order, empty for a refusal published before the step.
    pub guardrails: Vec<GuardrailMatch>,
    pub received_at: i64,
}

#[derive(Clone, Debug)]
pub struct AccountSelected {
    pub request_id: String,
    pub attempt: u8,
    pub account_id: String,
    pub account_kind: String,
    pub pin_epoch: Option<u64>,
    pub started_at: i64,
    /// The TTL the relay rewrote the body to on this attempt, in milliseconds; `None` when the body went as sent.
    pub cache_ttl_ms: Option<i64>,
    /// The selection trace as JSON, `None` for a probe's own placement.
    pub selection: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ResponseHead {
    pub request_id: String,
    pub attempt: u8,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// The data of the committing event of a held stream: `message_start` or the `error` that failed it.
    pub first_event: Option<Bytes>,
    pub outcome: AttemptOutcome,
    pub cooldown: Option<CooldownSet>,
    /// When the request body was flushed to the upstream, so a waterfall can show the write phase.
    pub written_at: i64,
    pub observed_at: i64,
}

#[derive(Clone, Debug)]
pub struct ResponseChunk {
    pub request_id: String,
    pub attempt: u8,
    pub index: u32,
    /// The decoded inspection copy, so a consumer reads text whatever the wire coding was.
    pub body: Bytes,
    pub phase: Option<Phase>,
}

#[derive(Clone, Debug)]
pub struct RequestComplete {
    pub request_id: String,
    pub attempt: u8,
    pub account_id: Option<String>,
    pub outcome: Outcome,
    pub status: u16,
    pub usage: Option<Usage>,
    /// Empty for a local answer and for an upstream that named nothing.
    pub response: ResponseFacts,
    pub ttfb_ms: Option<i64>,
    pub ttfc_ms: Option<i64>,
    pub error_code: Option<String>,
    pub client_drift: Option<String>,
    pub client_version: Option<String>,
    pub response_bytes: Option<u64>,
    /// How many `ResponseChunk` events the relay published for this request; `None` for a local answer with
    /// no relayed body.
    pub response_chunks: Option<u32>,
    /// The holds the request waited through before its answer, as the `CohortHold` JSON the selection trace
    /// carries; `None` for a request never held. Set on a request held and then served, and on one whose hold
    /// ended in an exhaustion with no attempt dispatched.
    pub held: Option<String>,
    /// The source of the `diagnostics` member on the body the answering leg sent, `None` for a body without one:
    /// the gate refused it, the switch is off, a strip after a 400 removed it, the request is a token count, or
    /// no leg answered.
    pub diagnostics: Option<DiagnosticsSource>,
    /// The `id` of the message the committed answer carried (`message_start`, or the JSON document), as the
    /// upstream sent it; `None` for a local answer, a cut before the first event, or an answer without one.
    pub message_id: Option<String>,
    pub finished_at: i64,
}

/// One queue carries every variant, so a subscriber sees a request's chunks between its head and its
/// completion and never has to reconcile two receivers.
// Boxing the completion would put an allocation on every clone the bus makes per subscriber, to shrink a
// value whose queues are already bounded.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum Event {
    AccountSelected(AccountSelected),
    RequestComplete(RequestComplete),
    RequestObserved(RequestObserved),
    ResponseChunk(ResponseChunk),
    ResponseHead(ResponseHead),
}

impl Event {
    pub fn request_id(&self) -> &str {
        match self {
            Event::AccountSelected(e) => &e.request_id,
            Event::RequestComplete(e) => &e.request_id,
            Event::RequestObserved(e) => &e.request_id,
            Event::ResponseChunk(e) => &e.request_id,
            Event::ResponseHead(e) => &e.request_id,
        }
    }
}

/// Request headers no consumer may see, mirroring the relay's outbound rule so that what keepalive replays is
/// what the caller sent. It holds the relay's `NEVER_COPIED` set (a unit test in crates/relay/src/headers.rs
/// asserts the two agree, since the bus may not depend on the relay), the credentials, and the framing headers
/// the relay regenerates for its own hop.
///
/// `accept-encoding` is the one entry that is neither: the relay decodes a compressed upstream answer only for
/// a caller that sent none, so a replayed one would hand compressed bytes to the probe's usage parser and pause
/// every recipe it ran for.
pub const HEADER_DENYLIST: &[&str] = &[
    "accept-encoding",
    "authorization",
    "connection",
    "content-length",
    "cookie",
    "expect",
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
    "x-forwarded-port",
    "x-forwarded-proto",
    "x-forwarded-server",
    "x-keepalive-owner-id",
    "x-real-ip",
];

/// Everything the denylist does not name travels, so a header the client starts sending after this code was
/// written still reaches a replay as the caller spelled it.
pub fn header_published(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    !HEADER_DENYLIST.contains(&lower.as_str())
}

/// Bound on the `User-Agent` a ledger row keeps, the same as the identity fields carry: the header reaches the
/// row from a request refused before any key was read, so it is the one string an unauthenticated caller can put
/// there.
pub const USER_AGENT_MAX_BYTES: usize = 256;

/// `value` cut to `USER_AGENT_MAX_BYTES` at a character boundary: the one rule the row and the live step apply.
pub fn user_agent_cut(value: &str) -> &str {
    let mut end = value.len().min(USER_AGENT_MAX_BYTES);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

pub fn user_agent_of(headers: &[(String, String)]) -> Option<String> {
    let value = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("user-agent"))
        .map(|(_, v)| v)?;
    Some(user_agent_cut(value).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_later_usage_carries_the_thinking_and_server_tool_counts_over_an_earlier_one() {
        let mut usage = Usage {
            output_tokens: Some(10),
            thinking_tokens: Some(4),
            web_search_requests: Some(1),
            ..Default::default()
        };
        usage.merge(&Usage {
            output_tokens: Some(40),
            thinking_tokens: Some(25),
            web_fetch_requests: Some(2),
            ..Default::default()
        });
        assert_eq!(
            (
                usage.output_tokens,
                usage.thinking_tokens,
                usage.web_search_requests,
                usage.web_fetch_requests
            ),
            (Some(40), Some(25), Some(1), Some(2)),
            "the later count wins and one only the earlier read carried survives"
        );
    }

    #[test]
    fn a_later_bare_write_total_does_not_replace_an_earlier_split() {
        // A streamed answer written at the 1-hour tier: `message_start` names the split, the final
        // `message_delta` repeats the write as one total with no tier.
        let mut usage = Usage {
            cache_write_5m_tokens: Some(0),
            cache_write_1h_tokens: Some(11_285),
            ..Default::default()
        };
        usage.merge(&Usage {
            cache_write_5m_tokens: Some(11_285),
            output_tokens: Some(90),
            ..Default::default()
        });
        assert_eq!(
            (
                usage.cache_write_5m_tokens,
                usage.cache_write_1h_tokens,
                usage.output_tokens
            ),
            (Some(0), Some(11_285), Some(90)),
            "the split stands and the counts beside it still move"
        );
        // With no split before it, a bare total is the whole write and the later one replaces the earlier.
        let mut bare = Usage {
            cache_write_5m_tokens: Some(3),
            ..Default::default()
        };
        bare.merge(&Usage {
            cache_write_5m_tokens: Some(5),
            ..Default::default()
        });
        assert_eq!(
            (bare.cache_write_5m_tokens, bare.cache_write_1h_tokens),
            (Some(5), None)
        );
    }

    #[test]
    fn the_answers_facts_merge_member_by_member_and_a_default_says_nothing() {
        let mut facts = ResponseFacts {
            model: Some("claude-opus-5".into()),
            speed: Some("fast".into()),
            ..Default::default()
        };
        facts.merge(&ResponseFacts {
            speed: Some("standard".into()),
            stop_reason: Some("refusal".into()),
            cleared_input_tokens: Some(1_200),
            ..Default::default()
        });
        assert_eq!(
            (
                facts.model.as_deref(),
                facts.speed.as_deref(),
                facts.stop_reason.as_deref(),
                facts.cleared_input_tokens
            ),
            (
                Some("claude-opus-5"),
                Some("standard"),
                Some("refusal"),
                Some(1_200)
            ),
            "a member only the earlier read carried survives the later one"
        );
        assert!(!facts.is_empty());
        assert!(ResponseFacts::default().is_empty());
    }

    #[test]
    fn a_user_agent_past_the_bound_is_cut_at_a_char_boundary() {
        // "é" is two bytes; landing the cut mid-character would produce invalid UTF-8.
        let long = format!("{}é{}", "a".repeat(255), "b".repeat(44));
        let headers = vec![("User-Agent".to_string(), long)];
        let cut = user_agent_of(&headers).unwrap();
        assert!(cut.len() <= USER_AGENT_MAX_BYTES);
        assert_eq!(cut, "a".repeat(255));
        assert!(user_agent_of(&[]).is_none());
    }

    #[test]
    fn user_agent_cut_on_a_value_under_at_and_over_the_bound_and_on_a_multibyte_boundary() {
        let under = "a".repeat(USER_AGENT_MAX_BYTES - 1);
        let at = "a".repeat(USER_AGENT_MAX_BYTES);
        assert_eq!(user_agent_cut(&under), under);
        assert_eq!(user_agent_cut(&at), at);
        assert_eq!(user_agent_cut(&format!("{at}b")), at);
        // "é" is two bytes and starts at the last byte the bound keeps, so the cut lands before it.
        assert_eq!(user_agent_cut(&format!("{under}é")), under);
    }
}
