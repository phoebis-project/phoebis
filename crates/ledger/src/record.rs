//! One struct per ledger table, field order matching the table's column order.

use phoebis_routing::{CacheMapEntry, DecisionRecord, DecisionSummary};
use phoebis_store::AccountKind;

use crate::LedgerError;

// A parked row outlives the build that wrote it, so a field a later build adds reads at its default.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
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
    /// `server`, `stripped`, `body_shape`; null for every other request.
    pub auto_mode: Option<String>,
    /// The `CohortHold` JSON of the holds the request waited through; `None` for a request never held.
    pub held: Option<String>,
    /// The ledger task's clock when the answer's `message_stop` chunk arrived, held to `finished_at` so a ledger
    /// behind the bus never records a stream that ended after its request finished; `None` for an answer that
    /// never reached it and for a row from before the column.
    pub stream_end_at: Option<i64>,
    /// `client` when the client's body carried its own `diagnostics` member, `router` when the relay inserted it,
    /// `None` for a body that went without one and for a row from before the column.
    pub diagnostics_source: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
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
    /// The `id` of the message the serving attempt's answer carried; `None` on every other attempt.
    pub upstream_message_id: Option<String>,
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
    /// The serving account's rate-limit tier as the router's registry snapshot held it when the request landed,
    /// `None` for an api_key account and for an oauth account whose profile has not been read. Written once, so a
    /// row reads the tier the account had and a later change moves nothing.
    pub tier: Option<String>,
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
    /// When the relay flushed the request body to this attempt's upstream, as the response head reported it.
    pub written_at: Option<i64>,
}

impl AttemptRecord {
    /// Whether the attempt carries any of the five priced token counts: what the fold prices, and what a retried
    /// leg's earlier, answer-less passes lack.
    pub fn has_usage(&self) -> bool {
        self.input_tokens.is_some()
            || self.output_tokens.is_some()
            || self.cache_read_tokens.is_some()
            || self.cache_write_5m_tokens.is_some()
            || self.cache_write_1h_tokens.is_some()
    }
}

/// What `parked_records.record` holds: a priced request the database refused on its own row, its request row and its
/// attempts serialized whole. A parked record keeps its money and not its capture, so the captured bodies have no
/// member to land in.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ParkedRecord {
    pub request: RequestRecord,
    pub attempts: Vec<AttemptRecord>,
}

/// `ParkedRecord`'s shape over rows the writer still holds, so a record is parked without a copy of it.
#[derive(serde::Serialize)]
pub(crate) struct ParkedRows<'a> {
    pub request: &'a RequestRecord,
    pub attempts: &'a [AttemptRecord],
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
    pub pool_id: Option<String>,
    pub agent_id: Option<String>,
    pub request_id: Option<String>,
}

/// One decision as the ledger stores it: the columns, the summary and the record as the router typed them. The
/// record is compressed by the writer inside its batch, never on the broadcast.
#[derive(Clone, Debug, PartialEq)]
pub struct DecisionRow {
    pub request_id: String,
    pub seq: u32,
    pub attempt: u32,
    pub at: i64,
    pub kind: String,
    pub probe: bool,
    pub key_id: Option<String>,
    pub pool_id: Option<String>,
    pub account_id: Option<String>,
    pub from_account_id: Option<String>,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub model: Option<String>,
    pub summary: DecisionSummary,
    pub record: DecisionRecord,
}

impl DecisionRow {
    /// The columns off the record's request and outcome blocks: `kind` and the accounts from the outcome, the
    /// rest from the request, `probe` from its flags.
    pub fn of(summary: DecisionSummary, record: DecisionRecord, at: i64) -> DecisionRow {
        let request = &record.request;
        DecisionRow {
            request_id: request.request_id.clone(),
            seq: request.seq,
            attempt: request.attempt,
            at,
            kind: record.outcome.kind.to_string(),
            probe: request.flags.probe,
            key_id: column(&request.key_id),
            pool_id: column(&request.pool_id),
            account_id: record.outcome.account_id.clone(),
            from_account_id: record.outcome.from_account_id.clone(),
            session_id: None,
            agent_id: column(&request.agent_id),
            model: column(&request.canonical_model),
            summary,
            record,
        }
    }
}

/// A refusal under a key the snapshot no longer holds names no pool, which the record spells empty. The column
/// reads null instead, so no filter finds the row under an empty id.
fn column(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

/// A completed answer's cache-map rows, for the `cache_map` column of the decision `seq` names. `attempt` is the
/// wire attempt that completed, kept for the log line.
#[derive(Clone, Debug, PartialEq)]
pub struct CacheMapWrite {
    pub request_id: String,
    pub seq: u32,
    pub attempt: u32,
    pub entries: Vec<CacheMapEntry>,
    pub at: i64,
}

/// The most a decision's frame may decode to. A record runs to a few kilobytes per pool member, so the bound sits far
/// above any pool's and still keeps a damaged header from sizing an allocation of its own choosing.
pub(crate) const MAX_RECORD_BYTES: u64 = 16 << 20;

/// The decision's frame decoded to its JSON object with a bounded output: the header's content size sizes the output
/// only within the bound, and the decoder refuses a frame that runs past the size it was given.
pub(crate) fn decode_record(
    frame: &[u8],
) -> Result<serde_json::Map<String, serde_json::Value>, LedgerError> {
    let capacity = match zstd::zstd_safe::get_frame_content_size(frame) {
        Ok(Some(size)) if size <= MAX_RECORD_BYTES => size,
        Ok(None) => MAX_RECORD_BYTES,
        Ok(Some(size)) => {
            return Err(LedgerError::Io(std::io::Error::other(format!(
                "a decision record's frame names {size} bytes, past the {MAX_RECORD_BYTES} a record may hold"
            ))))
        }
        Err(_) => {
            return Err(LedgerError::Io(std::io::Error::other(
                "a decision record's frame has no readable header",
            )))
        }
    };
    let bytes = zstd::bulk::decompress(frame, capacity as usize)?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct QuotaSampleRecord {
    pub account_id: String,
    pub bucket: i64,
    pub window: String,
    pub utilization: Option<f64>,
    pub reset_at: Option<i64>,
    pub source: String,
    /// Milliseconds: the poll's `fetched_at` or the head's clock, stamping the reading itself where the bucket
    /// only names its five minutes.
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

/// What the keepalive engine adds to one rollup row: the credit of a kept-warm request, a probe's migration mark, or
/// the settlement of a stopped recipe. Never a request counter: those come from the request rows alone.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct KeepaliveDelta {
    /// The row's instant. On a settlement it is the probe's exchange instant rather than the settlement's clock, so
    /// the row and its price keys are the probe's.
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
    pub probe_migrated: u64,
    pub probes_skipped: u64,
    pub wasted_probes: u64,
    /// The wasted probe's five token counts, what `wasted_probe_usd` priced, priced when read.
    pub wasted_input_tokens: u64,
    pub wasted_cache_read_tokens: u64,
    pub wasted_cache_write_5m_tokens: u64,
    pub wasted_cache_write_1h_tokens: u64,
    pub wasted_output_tokens: u64,
    pub wasted_probe_reasons: std::collections::BTreeMap<String, u64>,
    /// The account tier the settlement read, which keys the credit row. `None` files under `-`.
    pub tier: Option<String>,
    /// A settled probe's prompt-length band, read at its exchange against the revision in force then, so its wasted
    /// counts price at the cells the probe ran at. `None` (standard) for a credit, a probe row or a skip.
    pub band: Option<crate::pricing::Band>,
    /// The serving account's kind as the engine read it once the probe's answer landed, which files an api_key
    /// account's credit under `billed` beside its request row. `None` for a kept-warm credit, a skip or a
    /// settlement, and for a probe whose account left the registry before its answer landed.
    pub account_kind: Option<AccountKind>,
    /// Whether the probe's answer reported overage in use, the fact its request row files under, so the credit row
    /// keys the same funding. `None` for a kept-warm credit, a skip or a settlement, which have no probe of their
    /// own and file under `plan`, or under `fast` for a fast recipe.
    pub overage_in_use: Option<bool>,
    /// The recipe's speed, which keys the credit row: `fast` for `Some(Speed::Fast)`, `standard` otherwise, so
    /// the derived `Default`'s `None` files standard.
    pub speed: Option<crate::pricing::Speed>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A parked row outlives the build that wrote it: one written before a field joined the record still reads, the
    /// field at its default, so the start lands it rather than keeping it parked for good.
    #[test]
    fn a_parked_record_missing_a_field_reads_with_that_field_at_its_default() {
        let parked: ParkedRecord = serde_json::from_str(
            r#"{"request":{"id":"r1","received_at":5},"attempts":[{"request_id":"r1","account_id":"a"}]}"#,
        )
        .expect("a record with fields missing still reads");
        assert_eq!(
            parked.request,
            RequestRecord {
                id: "r1".into(),
                received_at: 5,
                ..RequestRecord::default()
            }
        );
        assert_eq!(
            parked.attempts,
            [AttemptRecord {
                request_id: "r1".into(),
                account_id: "a".into(),
                ..AttemptRecord::default()
            }]
        );
    }
}
