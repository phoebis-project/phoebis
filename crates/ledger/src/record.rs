//! One struct per ledger table, field order matching the table's column order.

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RequestRecord {
    pub id: String,
    pub received_at: i64,
    pub finished_at: Option<i64>,
    pub key_id: Option<String>,
    pub key_generation: i64,
    pub pool_id: Option<String>,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub parent_agent_id: Option<String>,
    pub model: String,
    /// The name the router records the model under: the dated id the catalog lists once the router has
    /// resolved the client's spelling, the folded spelling (the `[1m]` suffix removed, ASCII lowercased) until
    /// then.
    pub canonical_model: String,
    pub stream: bool,
    pub kind: String,
    pub origin: String,
    pub status: Option<u16>,
    pub outcome: String,
    pub attempts: u32,
    pub request_bytes: Option<u64>,
    pub response_bytes: Option<u64>,
    pub ttfb_ms: Option<i64>,
    pub ttfc_ms: Option<i64>,
    pub duration_ms: Option<i64>,
    pub error_code: Option<String>,
    /// The kind a client-drift refusal answered with, whether or not the refusal reached an account: `hash` or
    /// `version_suffix`. `None` for a request that never tripped either self-check.
    pub client_drift: Option<String>,
    pub kept_warm: bool,
    pub bodies_captured: bool,
    /// Whether the captured body is shorter than what the relay saw; false when nothing was captured. The
    /// assembler's own verdict, stored on this row: a stream cut by a chunk lost after the last one that
    /// arrived never shows up in a length comparison against the bodies table, so the row has to carry it.
    pub request_truncated: bool,
    pub response_truncated: bool,
    /// The caller's `User-Agent` as it was sent, `None` when it sent none.
    pub user_agent: Option<String>,
    pub effort: Option<String>,
    pub thinking_type: Option<String>,
    pub thinking_budget: Option<u64>,
    pub max_tokens: Option<u64>,
    pub tool_count: u32,
    pub system_blocks: u32,
    pub message_count: u32,
    /// Comma-joined, empty for none: a request sends at most two edit types and the log reads them as a list.
    pub context_edits: String,
    pub temperature: Option<f64>,
    pub requested_speed: Option<String>,
    pub requested_service_tier: Option<String>,
    /// The caller presented the secret a rotation retired.
    pub presented_retired: bool,
    /// The key's `rotated_at` at authentication, `None` for a key never rotated: which rotation a retired
    /// presentation belongs to.
    pub rotation_at: Option<i64>,
    /// The class the relay derived at admission, `claude-code/<entrypoint>` or `other`. `None` on a row written
    /// before the column existed.
    pub client: Option<String>,
    /// The gateway hint headers as the client sent them, each `None` when it sent none.
    pub agent_type: Option<String>,
    pub compaction: Option<String>,
    pub context_compacted: Option<String>,
    /// The sum of the client's `x-claude-code-prev-tool-durations`, in milliseconds.
    pub prev_tool_ms: Option<i64>,
    /// `cc_turn_origin` off the attribution line; first-party only.
    pub turn_origin: Option<String>,
    /// The class the client named on `x-claude-code-request-class`, `None` when it sent none.
    pub request_class: Option<String>,
    /// A request that continued another conversation: that conversation's root and the matched depth.
    pub forked_from: Option<String>,
    pub fork_depth: Option<u32>,
    /// `server`, `stripped`, `client_stage1`, `client_stage2`; null for every other request.
    pub auto_mode: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AttemptRecord {
    pub request_id: String,
    pub attempt: u32,
    pub account_id: String,
    pub account_kind: String,
    pub pin_epoch: Option<u64>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub ttfb_ms: Option<i64>,
    pub ttfc_ms: Option<i64>,
    pub upstream_status: Option<u16>,
    pub upstream_request_id: Option<String>,
    pub outcome: String,
    pub cooldown_scope: Option<String>,
    pub cooldown_until: Option<i64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_5m_tokens: Option<u64>,
    pub cache_write_1h_tokens: Option<u64>,
    pub service_tier: Option<String>,
    pub cache_miss_reason: Option<String>,
    pub client_drift: Option<String>,
    pub ratelimit_5h_utilization: Option<f64>,
    pub ratelimit_5h_reset: Option<i64>,
    pub ratelimit_7d_utilization: Option<f64>,
    pub ratelimit_7d_reset: Option<i64>,
    pub ratelimit_7d_oi_utilization: Option<f64>,
    pub ratelimit_7d_oi_reset: Option<i64>,
    pub overage_in_use: Option<bool>,
    pub overage_utilization: Option<f64>,
    pub estimated_usd: Option<f64>,
    pub input_usd: Option<f64>,
    pub cache_read_usd: Option<f64>,
    pub cache_write_usd: Option<f64>,
    pub output_usd: Option<f64>,
    pub pricing_version: Option<String>,
    pub thinking_tokens: Option<u64>,
    pub web_search_requests: Option<u64>,
    pub web_fetch_requests: Option<u64>,
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
    /// The model the answer named, recorded only when it is not the one the router resolved.
    pub served_model: Option<String>,
    pub speed: Option<String>,
    pub inference_geo: Option<String>,
    pub cleared_input_tokens: Option<u64>,
    pub refusal_category: Option<String>,
    pub refusal_recommended_model: Option<String>,
    /// The trace as stored, `cache_read` folded in at completion.
    pub selection: Option<String>,
    /// One vocabulary for both checks: a server check's `not_flagged`, `flagged`, `skipped`, `unsupported`,
    /// `unavailable`, `none`, `unrecognized`; a classifier request's `not_flagged`, `flagged`, `refused`,
    /// `unreadable`. Null for a request that asked for nothing and for an attempt no answer reached.
    pub auto_mode_verdict: Option<String>,
    /// The `unavailable` reason, the refusal category, or the stage-2 `<category>` slug.
    pub auto_mode_detail: Option<String>,
    /// A server check's map, `{"<tool_use_id>": {"outcome", "reason", "explanation"}}` with null members left out.
    pub auto_mode_tool_uses: Option<String>,
}

impl AttemptRecord {
    /// Whether the attempt carries any of the five priced token counts: what `price_attempts` prices, and what
    /// a retried leg's earlier, answer-less passes lack.
    pub fn has_usage(&self) -> bool {
        self.input_tokens.is_some()
            || self.output_tokens.is_some()
            || self.cache_read_tokens.is_some()
            || self.cache_write_5m_tokens.is_some()
            || self.cache_write_1h_tokens.is_some()
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BodyRecord {
    pub request_id: String,
    pub request_body: Vec<u8>,
    pub response_body: Option<Vec<u8>>,
    /// The bytes each side ran to as the capture saw them; equal to the stored vector's length for a whole
    /// body, larger for a cut one. A truncation flag is derived: seen > stored.
    pub request_len_seen: u64,
    pub response_len_seen: u64,
    pub expires_at: i64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct EventRecord {
    pub at: i64,
    pub kind: String,
    pub account_id: Option<String>,
    pub key_id: Option<String>,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub actor: Option<String>,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct QuotaSampleRecord {
    pub account_id: String,
    pub bucket: i64,
    pub window: String,
    pub utilization: Option<f64>,
    pub reset_at: Option<i64>,
    pub source: String,
    /// Milliseconds: the poll's `fetched_at` or the head's clock. The bucket alone cannot say which side of a
    /// request completed in the same five minutes a reading fell on.
    pub observed_at: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExtraUsageSampleRecord {
    pub account_id: String,
    pub bucket: i64,
    pub is_enabled: bool,
    pub in_use: Option<bool>,
    pub monthly_limit: Option<i64>,
    pub used_credits: Option<i64>,
    pub utilization: Option<f64>,
    pub currency: Option<String>,
    pub disabled_reason: Option<String>,
    pub overage_reset: Option<i64>,
    pub source: String,
}

/// What the keepalive engine adds to one rollup row: the credit of a kept-warm request, the cost of a probe, or
/// the settlement of a stopped recipe. Never a request counter: those come from the request rows alone.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct KeepaliveDelta {
    pub at: i64,
    pub key_id: String,
    pub key_generation: i64,
    pub pool_id: String,
    pub account_id: String,
    pub canonical_model: String,
    pub kind: String,
    /// The request the credit is for, marked `kept_warm` by id.
    pub kept_warm_request_id: Option<String>,
    pub kept_warm_requests: u64,
    pub kept_warm_tokens: u64,
    pub saved_usd_gross: f64,
    pub probe_cost_usd: f64,
    pub probe_migrated: u64,
    pub probes_skipped: u64,
    pub wasted_probes: u64,
    pub wasted_probe_usd: f64,
    pub wasted_probe_reasons: std::collections::BTreeMap<String, u64>,
}
