//! The statistics ledger: `ledger.sqlite`, written by one task from what the bus and the router publish, and
//! read by the admin routes and the metrics listener.

mod assemble;
mod bodies;
mod db;
mod events;
mod histogram;
mod metrics;
mod pricing;
mod query;
mod quota;
mod record;
mod rollup;
mod schema;
mod task;
mod usage;

pub use assemble::{Assembler, Record, IN_FLIGHT_TTL_MS};
pub use bodies::{run_retention, Purged, MAX_CAPTURED_BODY, ROLLUP_RETENTION_DAYS};
pub use db::LedgerDb;
pub use events::{event_of, spawn_event_mirror};
pub use histogram::{bucket_edges, bucket_index, Histogram, BUCKET_COUNT};
pub use metrics::{
    gather, metrics_router, KeepaliveAgents, KeepaliveGaugeSource, Metrics, RoutingGauges, FAMILIES,
};
pub use pricing::{
    diff_against, parse_import, price_attempts, ImportFormat, ModelPrice, PriceChange, PriceDiff,
    PriceTable, Pricing,
};
pub use query::{
    apply_filter, breakdown, bucket_seconds, dashboard, group_of, hour_ceil, latest_extra_usage,
    latest_quota, list_requests, parse_range, pool_traffic, request_by_id, request_events,
    request_totals, series, session_activity, Dashboard, GroupRow, ProbeCounts, RequestFilter,
    RollupFilter, SeriesPoint, SessionActivity, Totals, TrafficBucket, GROUP_BYS, MAX_POINTS,
    MAX_RANGE_MS,
};
pub use quota::{samples_from_head, samples_from_poll, QuotaPoll};
pub use record::{
    AttemptRecord, BodyRecord, EventRecord, ExtraUsageSampleRecord, KeepaliveDelta,
    QuotaSampleRecord, RequestRecord,
};
pub use rollup::{
    bucket_of, delta_of, fold_account, fold_event, fold_keepalive, fold_request, hour_of,
    read_account_rollups, read_rollups, AccountRollupRow, CostSplit, RollupRow,
    KEY_RANGE_PREDICATE, UNATTRIBUTED,
};
pub use task::{spawn, Ledger, LedgerConfig, LedgerGone, LedgerStats, CHANNEL_DEPTH};
pub use usage::{key_usage_windows, usage_router, KeyUsageWindow, ModelUsage, UsageTokens};

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("pricing table: {0}")]
    Yaml(String),
}
