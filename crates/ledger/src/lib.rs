//! The statistics ledger: `ledger.sqlite`, written by one task from what the bus and the router publish, and
//! read by the admin routes and the metrics listener.

mod assemble;
mod bodies;
mod db;
mod events;
mod fit;
mod generations;
mod histogram;
mod metrics;
mod pattern;
mod plan;
mod pricing;
mod query;
mod quota;
mod record;
mod rollup;
mod schema;
mod task;
mod usage;

pub use assemble::{Assembler, Expired, Record, IN_FLIGHT_TTL_MS};
pub use bodies::{run_retention, Purged, MAX_CAPTURED_BODY};
pub use db::LedgerDb;
pub use events::{event_of, spawn_event_mirror};
pub use fit::{Fitter, MIN_INTERVAL_MS};
pub use generations::{reconcile_key_generations, LiveKeyLifetime, GENERATIONS_RECONCILED_KEY};
pub use histogram::{bucket_edges, bucket_index, Histogram, BUCKET_COUNT};
pub use metrics::{
    gather, metrics_router, KeepaliveAgents, KeepaliveGaugeSource, Metrics, RoutingGauges, FAMILIES,
};
pub use pattern::{
    matches as pattern_matches, to_like as pattern_to_like,
    within_bounds as patterns_within_bounds, MAX_PATTERNS, MAX_PATTERN_CHARS, PATTERNS_TOO_LONG,
};
pub use plan::{
    Band, Cell, CostTable, Factor, ModelCells, ModelFactor, PlanFactors, PlanTables, PlanWindows,
    Share, TierFactor, TierFactors, TierWindows,
};
pub use pricing::{price_attempts, ModelPrice, PriceTable, Pricing};
pub use query::{
    apply_filter, breakdown, bucket_seconds, burn_rows, burn_series, burn_series_over, dashboard,
    group_of, hour_ceil, key_request_totals, latest_extra_usage, latest_quota, list_requests,
    member_spends, moves_by_reason, parse_range, pin_migrations_by_reason, placements,
    pool_traffic, recent_events_of_kind, request_by_id, request_events, request_totals,
    rewrite_series, series,
    session_activity, session_burn_series, session_identities, BurnPoint, Dashboard, GroupRow,
    LiveKeys, PlacementRow, ProbeCounts, RequestFilter, RollupFilter, SeriesFilter, SeriesPoint,
    SessionActivity, SessionIdentity, Tiers, Totals, TrafficBucket, GROUP_BYS, MAX_POINTS,
    MAX_RANGE_MS,
};
pub use quota::{
    samples_from_head, samples_from_poll, QuotaPoll, Sampled, POLL_INTERVAL, PROFILE_EVERY_POLLS,
};
pub use record::{
    AttemptRecord, BodyRecord, EventRecord, ExtraUsageSampleRecord, KeepaliveDelta,
    QuotaSampleRecord, RequestRecord,
};
pub use rollup::{
    bucket_of, delta_of, fold_account, fold_event, fold_keepalive, fold_request, hour_of,
    read_account_rollups, read_rollups, AccountRollupRow, CostSplit, RollupRow,
    KEY_RANGE_PREDICATE, NO_CLASS, UNATTRIBUTED,
};
pub use task::{spawn, Ledger, LedgerConfig, LedgerGone, LedgerStats, CHANNEL_DEPTH};
pub use usage::{key_usage_windows, usage_router, KeyUsageWindow, ModelUsage, UsageTokens};

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// The blocking worker running the read panicked, or was dropped before it answered. This falls on the one
    /// call it happened to: `LedgerDb::read_blocking` recovers a poisoned guard, so the next read on the same
    /// `LedgerDb` runs as if nothing happened.
    #[error("the ledger read was cancelled before it ran")]
    ReadCancelled,
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("plan table: {0}")]
    Plan(String),
    #[error("pricing table: {0}")]
    Yaml(String),
}
