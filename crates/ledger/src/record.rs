//! One struct per ledger table, its fields in the table's column order but for a few members kept beside the ones they
//! belong with: `RequestRecord`'s truncation flag follows `bodies_captured`, `client_contract` follows `client_drift`,
//! `classifier` follows `auto_mode` and `thinking_diagnostics_source` follows `diagnostics_source`, and
//! `row_to_request` reads each by its column's index.
//! `RequestRecord::guardrails` is the one member no column holds: its rows live in the child table
//! `request_guardrails`. `ServingFacts` is the request row's nested block of its serving attempt's columns, read into
//! the one member `RequestRecord::serving`. `InputTransformation` holds one key's entry of the JSON object the two
//! `input_transformations` columns store.

use std::collections::BTreeMap;

use phoebis_routing::{CacheMapEntry, DecisionRecord, DecisionSummary, PinUsage};
use phoebis_store::AccountKind;

use crate::plan::{funding_of, Funding};
use crate::pricing::Speed;
use crate::rollup::fold_speed;
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
    /// The item a `client_contract` refusal named, one of the six words of the contract, and `None` for every other
    /// request and on a row from before the column.
    pub client_contract: Option<String>,
    pub kept_warm: bool,
    pub bodies_captured: bool,
    /// Whether the captured response is shorter than what the relay saw, false when nothing was captured. The
    /// assembler's own verdict, stored on this row: a stream cut by a chunk lost after the last one that arrived
    /// never shows up in a length comparison against the bodies table, so the row has to carry it.
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
    /// `client` for the client's own auto-mode classifier call, `client_off` for one whose committed attempt ran off
    /// the account and incarnation its judged turn ended on, or in a session with no turn to judge, null otherwise
    /// and on a row from before the column.
    pub classifier: Option<String>,
    /// The `CohortHold` JSON of the holds the request waited through; `None` for a request never held.
    pub held: Option<String>,
    /// The ledger task's clock when the answer's `message_stop` chunk arrived, held to `finished_at` so a ledger
    /// behind the bus never records a stream that ended after its request finished; `None` for an answer that
    /// never reached it and for a row from before the column.
    pub stream_end_at: Option<i64>,
    /// `client` when the client's body carried its own `diagnostics` member, `router` when the relay inserted it,
    /// `None` for a body that went without one and for a row from before the column.
    pub diagnostics_source: Option<String>,
    /// `client` when the caller's `anthropic-beta` list carried the thinking-binding beta, `router` when the relay
    /// added it, `None` for a request that went without it and for a row from before the column.
    pub thinking_diagnostics_source: Option<String>,
    /// The guardrail that blocked the request, `None` where none did and on a row from before the column.
    pub guardrail_id: Option<String>,
    /// Every guardrail that matched, in evaluation order, empty on a row from before the table.
    pub guardrails: Vec<GuardrailMatchRecord>,
    /// The router refused the request before it asked any account, for the client's or a policy's cause, so the
    /// figures leave it out and the logs keep it. A row from before the column reads what the step's backfill set.
    pub refused: bool,
    /// The SHA-256 of the fallback credit token the body presented, lowercase hex, `None` where it presented none and
    /// on a row from before the column.
    pub fallback_digest: Option<String>,
    /// The serving attempt's facts as the landing wrote them, every field `None` on a row from before the columns
    /// until the backfill reaches it. The assembler leaves them at their default, and the landing derives them from
    /// the attempts it writes.
    pub serving: ServingFacts,
    /// The selection's `marker` fact: the body carried a `cache_control` marker, or the pool's TTL policy added one.
    /// `None` on a row from before the column.
    pub cache_marker: Option<bool>,
}

/// The serving attempt's facts the request row carries: the last attempt by number, the one the rollups file the
/// request under. Every field `None` for a request no attempt served.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ServingFacts {
    pub account_id: Option<String>,
    pub reached_upstream: Option<bool>,
    pub outcome_class: Option<String>,
    /// `rollup::fold_speed`'s word, the one speed rule.
    pub speed: Option<String>,
    pub cache_miss_reason: Option<String>,
    pub stop_reason: Option<String>,
    pub refusal_category: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    /// The 5-minute and 1-hour writes summed.
    pub cache_write_tokens: Option<u64>,
    /// The serving attempt's `input_transformations`, cloned as the attempt holds it.
    pub input_transformations: Option<BTreeMap<String, InputTransformation>>,
}

impl ServingFacts {
    /// The facts of `attempts`' last attempt, every field `None` for a request no attempt served.
    pub(crate) fn of(request: &RequestRecord, attempts: &[AttemptRecord]) -> ServingFacts {
        let Some(last) = attempts.last() else {
            return ServingFacts::default();
        };
        ServingFacts {
            account_id: Some(last.account_id.clone()),
            reached_upstream: Some(last.upstream_status.is_some()),
            outcome_class: Some(last.outcome.clone()),
            speed: Some(fold_speed(Some(last), request.requested_speed.as_deref()).to_string()),
            cache_miss_reason: last.cache_miss_reason.clone(),
            stop_reason: last.stop_reason.clone(),
            refusal_category: last.refusal_category.clone(),
            input_tokens: last.input_tokens,
            output_tokens: last.output_tokens,
            cache_read_tokens: last.cache_read_tokens,
            cache_write_tokens: last.cache_write_tokens(),
            input_transformations: last.input_transformations.clone(),
        }
    }
}

/// One row of `request_guardrails`: a guardrail that matched the request, its place in the evaluation order and
/// whether it blocked.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GuardrailMatchRecord {
    pub guardrail_id: String,
    pub position: u32,
    pub blocked: bool,
}

/// The blocks an answer dropped under one `<type>.<reason>` key of its `input_transformations`.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InputTransformation {
    pub blocks: u64,
    /// The lowest index `i` of the dropped blocks' `messages.{i}` paths.
    pub first_message: u64,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AttemptRecord {
    pub request_id: String,
    pub attempt: u32,
    pub account_id: String,
    pub account_kind: AccountKind,
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
    /// The serving account's rate-limit tier the router priced by when the request landed, `None` for an api_key
    /// account, for an oauth account whose profile has not been read, and for a plan the router never prices (a free
    /// organization, a usage-based seat). Written once, so a row reads the tier the account had and a later change
    /// moves nothing.
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
    /// The serving account's class as the registry held it at the attempt, false on a row from before the column.
    pub usage_based: bool,
    /// The SHA-256 of `stop_details.fallback_credit_token` where the attempt's answer carries one, which the upstream
    /// issues with a refusal, lowercase hex, `None` elsewhere and on a row from before the column.
    pub issued_digest: Option<String>,
    /// The blocks the answer dropped, per `<type>.<reason>` key. An empty map for an answer that listed nothing,
    /// `None` for an answer without the member and for an attempt from before the column.
    pub input_transformations: Option<BTreeMap<String, InputTransformation>>,
    /// The prompt size of the session's previous request as the selection handed it on, the size the expiry count
    /// halves. `None` where the selection handed none on and for an attempt from before the column.
    pub previous_input_tokens: Option<u64>,
}

impl AttemptRecord {
    /// Whether the attempt carries any of the five priced token counts: what the fold sums and a reader prices, and
    /// what a retried leg's earlier, answer-less passes lack.
    pub fn has_usage(&self) -> bool {
        self.input_tokens.is_some()
            || self.output_tokens.is_some()
            || self.cache_read_tokens.is_some()
            || self.cache_write_5m_tokens.is_some()
            || self.cache_write_1h_tokens.is_some()
    }

    /// The five priced counts as the usage the pricing reads take, an absent count read as 0.
    pub fn usage(&self) -> PinUsage {
        PinUsage {
            input: self.input_tokens.unwrap_or(0),
            cache_read: self.cache_read_tokens.unwrap_or(0),
            write_5m: self.cache_write_5m_tokens.unwrap_or(0),
            write_1h: self.cache_write_1h_tokens.unwrap_or(0),
            output: self.output_tokens.unwrap_or(0),
        }
    }

    /// The 5-minute and 1-hour cache writes summed, an absent half read as 0, `None` when both are absent.
    pub(crate) fn cache_write_tokens(&self) -> Option<u64> {
        match (self.cache_write_5m_tokens, self.cache_write_1h_tokens) {
            (None, None) => None,
            (five_minute, hour) => Some(five_minute.unwrap_or(0) + hour.unwrap_or(0)),
        }
    }

    /// Who paid for the attempt, `funding_of` read off its own four facts.
    pub fn funding(&self) -> Funding {
        funding_of(
            Some(self.account_kind),
            self.usage_based,
            self.overage_in_use,
            Speed::of(self.speed.as_deref()),
        )
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
    /// The account a pin move left, on a `pin_migrated` row alone, so the Account filter lists the move under it.
    pub from_account_id: Option<String>,
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
            session_id: request.session_id.clone(),
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

phoebis_store::text_enum! {
    /// The answer a sample came from. A response head truncates the window's percent and the usage poll rounds it. The
    /// column stores the word `as_str` answers, and a stored word outside the two fails the read.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub enum SampleSource {
        Head => "head",
        /// A record that names no source lands as a poll, which fills only a bucket nothing else wrote.
        #[default]
        Poll => "poll",
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct QuotaSampleRecord {
    pub account_id: String,
    pub bucket: i64,
    pub window: String,
    pub utilization: Option<f64>,
    pub reset_at: Option<i64>,
    pub source: SampleSource,
    /// Milliseconds: the poll's `fetched_at` or the head's clock, stamping the reading itself where the bucket
    /// only names its five minutes.
    pub observed_at: i64,
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
    pub source: SampleSource,
    /// The poll's `spend` object as the body gave it: the cap and the spend in minor units, the percent on the
    /// percent scale, the currency and its exponent. Null on a head row and on a poll whose body carried no object.
    pub spend_limit_minor: Option<i64>,
    pub spend_used_minor: Option<i64>,
    pub spend_percent: Option<f64>,
    pub spend_currency: Option<String>,
    pub spend_exponent: Option<u8>,
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
    /// The client label the row keys under, that of the probe's own request row on a mark, a settlement and a skip, and
    /// of the credited request on a credit, so the row prices under that client's regime as its request row does. An
    /// empty value folds under the reserved id.
    pub client: String,
    /// The user agent of the probe's own request row on a mark, a settlement and a skip, and of the credited request
    /// on a credit.
    pub user_agent: Option<String>,
    /// The request the credit is for, marked `kept_warm` by id.
    pub kept_warm_request_id: Option<String>,
    pub kept_warm_requests: u64,
    /// The credited request's cache-read tokens, booked under its observation's TTL: an hour books
    /// `kept_warm_1h_tokens`, any other TTL (the API's five minutes, and a body with no marker, written at the default)
    /// `kept_warm_5m_tokens`.
    pub kept_warm_5m_tokens: u64,
    pub kept_warm_1h_tokens: u64,
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
    /// The priced tier that keys the row, as `SubscriptionTerm::priced_tier` answers it: the one a settlement read for
    /// its probe, or the one a credited request's serving account prices by. `None`, the answer for a plan the router
    /// does not price, files under `-`.
    pub tier: Option<String>,
    /// The prompt-length band the row's counts price at: a settled probe's, read at its exchange, or a credited
    /// request's own, read off its usage against the revision in force at its arrival. Standard for a probe row or a
    /// skip.
    pub band: crate::pricing::Band,
    /// The serving account's kind as the engine read it, which files an api_key account's row under `billed` beside
    /// its request row. Set on every delta the savings code records (the migration mark, the kept-warm credit, the
    /// settlement and the skip), `None` there where the registry no longer holds the account, and `None` on a delta
    /// built by default.
    pub account_kind: Option<AccountKind>,
    /// The serving account's class beside its kind, which files a usage-based account's row under `usage_based`
    /// beside its request row. False on a delta built by default.
    pub usage_based: bool,
    /// Whether the serving answer reported overage in use, the fact its request row files under, so the row keys the
    /// same funding. Set on every delta the savings code records (the migration mark, the kept-warm credit, the
    /// settlement and the skip), and false on a delta built by default.
    pub overage_in_use: bool,
    /// The speed that keys the credit row, the recipe's for a credit, a skip or a migration mark and the probe answer's
    /// for a settlement, standard on a delta built by default.
    pub speed: crate::pricing::Speed,
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
