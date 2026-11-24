//! The statistics ledger: `ledger.sqlite`, written by one task from what the bus and the router publish, and
//! read by the admin routes and the metrics listener.

mod allowance;
mod assemble;
mod bodies;
mod db;
mod events;
mod generations;
mod histogram;
mod keys;
mod metrics;
mod pattern;
mod plan;
#[cfg(test)]
mod plan_drift;
mod price;
mod pricing;
mod query;
mod quota;
mod reconcile;
mod record;
mod refold;
mod refusals;
mod rollup;
mod schema;
mod session_totals;
mod task;
mod usage;

pub use allowance::{term_end_of, unused_allowance, AllowanceReset, TermEnd, TermPair};
pub use assemble::{Assembler, Record, IN_FLIGHT_TTL_MS};
pub use bodies::{
    run_retention, run_retention_blocking, Purged, MAX_CAPTURED_BODY, PURGE_SLICE,
    PURGE_SLICE_BYTES,
};
pub use db::{backfill_serving_facts, Backfilled, LedgerDb, WriterRead, BACKFILL_SLICE, READERS};
pub use events::{
    event_of, limit_reset_claimed, plan_ended, plan_restored, poll_backoff, poll_failed,
    MirrorBaseline,
};
pub use generations::{
    key_generations_reconciled, reconcile_key_generations, LiveKeyLifetime,
    GENERATIONS_RECONCILED_KEY,
};
pub use histogram::{bucket_index, Histogram};
pub use metrics::{
    gather, metrics_router, AttentionGaugeSource, AttentionGauges, KeepaliveAgents,
    KeepaliveGaugeSource, Metrics, RoutingGauges, FAMILIES,
};
pub use pattern::{Patterns, PATTERNS_TOO_LONG};
pub use plan::{
    entrypoint_of_client, funding_of, price_shares, Capacity, CostRevisions, Funding, ModelWindow,
    Override, Peak, PerWindow, PlanTables, Regime, RegimePeak, SubscriptionCosts, TypeMultiplier,
    TypeOverride, Unit, WindowShares,
};
pub use price::{price_row, Priced, RowKey, SHARE_REASON_CLIENT_CLASSIFIER};
pub use pricing::{
    effective_instant, Band, LongContext, ModelPrice, ModelRow, PriceRevisions, PriceTable,
    Pricing, Speed,
};
#[cfg(any(test, feature = "test-support"))]
pub use query::{apply_filter, breakdown, series};
#[cfg(any(test, feature = "test-support"))]
pub mod test_tables;
pub use query::session_placements;
pub use query::{
    account_request_counts, breakdown_paged, bucket_seconds, burn_groups_paged, dashboard_on,
    decision_record, guardrail_counts, held_series, hour_ceil, hourly_window_samples,
    key_request_totals, key_request_totals_by_generation, latest_extra_usage,
    latest_extra_usage_polled, latest_extra_usage_share, latest_quota, limit_reset_uses,
    list_requests, list_routing_log, member_spends, moves_by_reason_and_fit, names_one_account,
    parked_count, parked_summary, parse_range, pin_migrations_by_account_and_reason, placements,
    pool_traffic, pools_traffic, recent_events_of_kind, recent_gaps, request_by_id, request_events,
    revision_spans, rewrite_series, routing_conditions, series_paged, session_activity,
    session_burn_series, session_identities, BudgetedMember, BurnPoint, CacheEquivalent,
    CacheFacts, Dashboard, DecisionListRow, EventListRow, FundingFilter, GroupBy, GroupRow,
    GuardrailCountRow, HourlyWindowSample, LimitResetUsed, LiveKeys, LogKinds, ParkedSummary,
    PlacementRow, ProbeCounts, ProbeEquivalent, ProbeView, RequestCursor, RequestFilter,
    RollupFilter, RoutingCondition, RoutingLogCursor, RoutingLogFilter, RoutingLogPage,
    RoutingLogRow, SeriesFilter, SeriesPoint, SessionActivity, SessionIdentity, SessionSpendKey,
    Span, Totals, TrafficBucket, DECISION_KINDS, EVENT_KINDS, MAX_POINTS, MAX_RANGE_MS,
};
pub use query::{guardrail_sample_page, request_body, request_body_held, SampleRow};
pub use query::{list_feature_calls, FeatureCallFilter, FeatureCallRow};
pub use query::{list_request_page, refusal_categories, RefusalCount, Refusals, RequestPage};
pub use query::{Direction, RequestOrder};
pub use quota::{
    asked, lead_over_pace, polled, samples_from_poll, QuotaPoll, Sampled, POLL_INTERVAL,
    PROFILE_EVERY_POLLS,
};
pub use reconcile::{
    newest_rows, read_reconciliations, ReconciliationRow, MIN_RISE, RATIO_HIGH, RATIO_LOW,
};
#[cfg(any(test, feature = "test-support"))]
pub use reconcile::{reconcile_read, reconcile_write};
pub use record::{
    AttemptRecord, BodyRecord, CacheMapWrite, DecisionRow, EventRecord, ExtraUsageSampleRecord,
    GuardrailMatchRecord, InputTransformation, KeepaliveDelta, ParkedRecord, QuotaSampleRecord,
    RequestRecord, SampleSource, ServingFacts,
};
pub use refold::{mark_fold_rules, refold_bands, refold_rules};
pub use rollup::{
    bucket_of, deltas_of, detail_json, fold_account, fold_effort, fold_event, fold_identifier,
    fold_keepalive, fold_request, fold_routing_conditions, fold_user_agent, hour_end, hour_of,
    open_cooldown_remainders, read_account_rollups, read_account_rollups_at,
    read_expired_between_turns, read_overview_rollups, read_rollups, read_rollups_paged,
    trailing_day_hours, AccountRollupRow, CostSplit, ExpiredBetweenTurns, KeySelector,
    OverviewRollupRow, RollupRow, RollupSelect, RoutingKind, BILLED_FUNDING, CEILING_SETTINGS,
    DIMENSION_MAX_BYTES, EXPIRED_BETWEEN_TURNS, FAST_FUNDING, FIVE_HOUR_CEILING_SETTING,
    KEY_RANGE_PREDICATE, LONG_BAND, NO_CLASS, OTHER_DIMENSION, OVERAGE_FUNDING, PAGE_HOURS,
    PLAN_FUNDING, SHED_CAP_SETTING, SLOW_DOWN_STRETCH_SETTING, SLOW_SETTINGS, STANDARD_BAND,
    UNATTRIBUTED, USAGE_BASED_FUNDING, WALL_SETTINGS, WEEKLY_CEILING_SETTING,
};
/// The type `LedgerDb::read` and `read_blocking` lend a closure, so a caller can name it in a signature.
pub use rusqlite::Connection;
pub use session_totals::{
    session_figures, unmark_rebanded_sessions, AgentFigures, DayFigures, SessionFigures,
    SpendFigures, SpendMoney,
};
pub use task::{
    spawn, Admission, Ledger, LedgerConfig, LedgerGone, LedgerStats, Replayed, UnpricedModel,
    BATCH_RETRY_DELAYS, BATCH_RETRY_HOLD, CHANNEL_DEPTH, LEDGER_BEHIND_DEPTH,
};
pub use usage::{key_usage_windows, usage_router, KeyUsageWindow, ModelUsage, UsageTokens};

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// The blocking worker running the read panicked, or was dropped before it answered. This falls on the one
    /// call it happened to: `LedgerDb`'s reads return their connection whatever the closure did, so the next read on
    /// the same ledger runs as if nothing happened.
    #[error("the ledger read was canceled before it ran")]
    ReadCanceled,
    /// A replay's bound passed before the writer took its turn, so nothing was replayed.
    #[error("the ledger writer is held, try again")]
    WriterHeld,
    #[error(transparent)]
    WriterGone(#[from] LedgerGone),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("plan table: {0}")]
    Plan(String),
    #[error("pricing table: {0}")]
    Yaml(String),
    /// A series reader's refusal of the range it was asked for, raised before it reads a row. Its words are the
    /// answer a route gives the caller.
    #[error("{0}")]
    Range(String),
    /// A file a newer build wrote: opening it would run no migration and read columns this build does not know. The
    /// message leaves `path` to the caller, whose context names the file.
    #[error(
        "the ledger's schema version {found} is newer than the {supported} this build opens: restore the backup \
         taken before the upgrade, or recreate a ledger a pre-release build wrote with phoebis init"
    )]
    SchemaTooNew {
        path: std::path::PathBuf,
        found: u32,
        supported: u32,
    },
}

impl LedgerError {
    /// Whether the failure is the database's rather than one row's, as `phoebis_store::is_database_wide_failure` reads
    /// it. Every write after it meets it too, so the record is held rather than read as a poison row.
    pub fn is_database_wide(&self) -> bool {
        matches!(self, LedgerError::Sqlite(error) if phoebis_store::is_database_wide_failure(error))
    }
}
