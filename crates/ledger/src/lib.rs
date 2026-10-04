//! The statistics ledger: `ledger.sqlite`, written by one task from what the bus and the router publish, and
//! read by the admin routes and the metrics listener.

mod assemble;
mod bodies;
mod db;
mod events;
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
pub use bodies::{run_retention, run_retention_blocking, Purged, MAX_CAPTURED_BODY, PURGE_SLICE};
pub use db::LedgerDb;
pub use events::{event_of, spawn_event_mirror};
pub use generations::{reconcile_key_generations, LiveKeyLifetime, GENERATIONS_RECONCILED_KEY};
pub use histogram::{bucket_edges, bucket_index, Histogram, BUCKET_COUNT};
pub use metrics::{
    gather, metrics_router, AttentionGaugeSource, AttentionGauges, KeepaliveAgents,
    KeepaliveGaugeSource, Metrics, RoutingGauges, FAMILIES,
};
pub use pattern::{
    matches as pattern_matches, to_like as pattern_to_like,
    within_bounds as patterns_within_bounds, MAX_PATTERNS, MAX_PATTERN_CHARS, PATTERNS_TOO_LONG,
};
pub use plan::{Band, CostTable, Factors, ModelCells, PlanTables};
pub use pricing::{price_attempts, ModelPrice, ModelRow, PriceTable, Pricing, Speed};
pub use query::{
    apply_filter, breakdown, breakdown_paged, bucket_seconds, burn_groups_paged, burn_rows,
    burn_series, burn_series_over, dashboard, group_of, hour_ceil, key_request_totals,
    latest_extra_usage, latest_quota, list_requests, literal_like, member_spends, moves_by_reason,
    parked_count, parked_summary, parse_range, pin_migrations_by_reason, placements, pool_traffic,
    recent_events_of_kind, recent_gaps, request_by_id, request_events, request_totals,
    rewrite_series, series, series_paged,
    session_activity, session_burn_series, session_identities, BudgetedMember, BurnPoint,
    Dashboard, GroupRow, LiveKeys, ParkedSummary, PlacementRow, ProbeCounts, RequestFilter,
    RollupFilter, SeriesFilter, SeriesPoint, SessionActivity, SessionIdentity, Tiers, Totals,
    TrafficBucket, GROUP_BYS, MAX_POINTS, MAX_RANGE_MS,
};
pub use quota::{
    polled, samples_from_head, samples_from_poll, QuotaPoll, Sampled, POLL_INTERVAL,
    PROFILE_EVERY_POLLS,
};
pub use record::{
    AttemptRecord, BodyRecord, EventRecord, ExtraUsageSampleRecord, KeepaliveDelta, ParkedRecord,
    QuotaSampleRecord, RequestRecord,
};
pub use rollup::{
    bucket_of, delta_of, fold_account, fold_effort, fold_event, fold_identifier, fold_keepalive,
    fold_request, fold_user_agent, hour_of, read_account_rollups, read_rollups, read_rollups_paged,
    AccountRollupRow, CostSplit, KeySelector, RollupRow, RollupSelect, DIMENSION_MAX_BYTES,
    EFFORTS, FAST_FUNDING, KEY_RANGE_PREDICATE, NO_CLASS, OTHER_DIMENSION, OVERAGE_FUNDING,
    PAGE_HOURS, PLAN_FUNDING, UNATTRIBUTED,
};
pub use task::{
    spawn, Admission, Ledger, LedgerConfig, LedgerGone, LedgerStats, BATCH_RETRY_DELAYS,
    BATCH_RETRY_HOLD, CHANNEL_DEPTH, LEDGER_BEHIND_DEPTH,
};
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
    /// A file a newer build wrote: opening it would run no migration and read columns this build does not know.
    #[error("the ledger at {} is at schema {found}, newer than the {supported} this build reads", path.display())]
    SchemaTooNew {
        path: std::path::PathBuf,
        found: u32,
        supported: u32,
    },
}

impl LedgerError {
    /// Whether the failure is the database's rather than one row's, the store's reading of the same codes: a lock
    /// held past the busy timeout, a full disk, an I/O error, a closed handle; every write after it meets it too.
    pub fn is_database_wide(&self) -> bool {
        matches!(
            self,
            LedgerError::Sqlite(rusqlite::Error::SqliteFailure(failure, _))
                if matches!(
                    failure.code,
                    rusqlite::ErrorCode::ApiMisuse
                        | rusqlite::ErrorCode::DatabaseBusy
                        | rusqlite::ErrorCode::DiskFull
                        | rusqlite::ErrorCode::SystemIoFailure
                )
        )
    }
}
