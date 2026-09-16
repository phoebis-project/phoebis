//! One struct per ledger table, field order matching the table's column order.

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RequestRecord {
    pub id: String,
    pub received_at: i64,
    pub finished_at: Option<i64>,
    pub key_id: Option<String>,
    pub pool_id: Option<String>,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub parent_agent_id: Option<String>,
    pub model: String,
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
    pub kept_warm: bool,
    pub bodies_captured: bool,
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
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BodyRecord {
    pub request_id: String,
    pub request_body: Vec<u8>,
    pub response_body: Option<Vec<u8>>,
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
