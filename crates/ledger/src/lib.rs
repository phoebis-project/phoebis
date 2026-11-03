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
mod task;
mod usage;

pub use allowance::{unused_allowance, AllowanceReset};
pub use assemble::{Assembler, Record, IN_FLIGHT_TTL_MS};
pub use bodies::{
    run_retention, run_retention_blocking, Purged, MAX_CAPTURED_BODY, PURGE_SLICE,
    PURGE_SLICE_BYTES,
};
pub use db::{LedgerDb, WriterRead, READERS};
pub use events::{
    catalog_changed, event_of, plan_ended, plan_restored, poll_backoff, poll_failed, MirrorBaseline,
};
pub use generations::{reconcile_key_generations, LiveKeyLifetime, GENERATIONS_RECONCILED_KEY};
pub use histogram::{bucket_index, Histogram};
pub use metrics::{
    gather, metrics_router, AttentionGaugeSource, AttentionGauges, KeepaliveAgents,
    KeepaliveGaugeSource, Metrics, RoutingGauges, FAMILIES,
};
pub use pattern::{
    literal_like, within_bounds as patterns_within_bounds, Patterns, PATTERNS_TOO_LONG,
};
pub use plan::{
    entrypoint_of_client, free_equivalent, free_shares, funding_of, price_equivalent, price_shares,
    Capacity, CostRevisions, Funding, ModelWindow, Override, Peak, PerWindow, PlanTables, Regime,
    RegimePeak, SubscriptionCosts, TypeMultiplier, TypeOverride, Unit, WindowShares,
};
pub use price::{price_row, Priced, RowKey, SHARE_REASON_CLIENT_CLASSIFIER};
pub use pricing::{
    effective_instant, prompt_tokens, Band, LongContext, ModelPrice, ModelRow, PriceRevisions,
    PriceTable, Pricing, Speed,
};
#[cfg(any(test, feature = "test-support"))]
pub use query::{apply_filter, breakdown, burn_rows, burn_series, series};
#[cfg(any(test, feature = "test-support"))]
pub mod test_tables;
pub use query::{agent_models, AgentModel};
pub use query::{
    breakdown_paged, bucket_seconds, burn_groups_paged, burn_series_over, dashboard_on,
    decision_record, guardrail_counts, held_series, hour_ceil, hourly_window_samples,
    key_request_totals, latest_extra_usage, latest_extra_usage_polled, latest_extra_usage_share,
    latest_quota, list_requests, list_routing_log, member_spends, moves_by_reason,
    moves_by_reason_and_fit, names_one_account, parked_count, parked_summary, parse_range,
    pin_migrations_by_account_and_reason, placements, pool_traffic, pools_traffic,
    recent_events_of_kind, recent_gaps, request_by_id, request_events, request_totals,
    revision_spans, rewrite_series, routing_conditions, series_paged, session_activity,
    session_burn_series, session_identities, BudgetedMember, BurnPoint, CacheEquivalent,
    CacheFacts, Dashboard, DecisionListRow, EventListRow, FundingFilter, GroupBy, GroupRow,
    GuardrailCountRow, HourlyWindowSample, LiveKeys, ParkedSummary, PlacementRow, ProbeCounts,
    ProbeEquivalent, RequestFilter, RollupFilter, RoutingCondition, RoutingLogCursor,
    RoutingLogFilter, RoutingLogPage, RoutingLogRow, SeriesFilter, SeriesPoint, SessionActivity,
    SessionIdentity, Span, Totals, TotalsBy, TrafficBucket, DECISION_KINDS, EVENT_KINDS, GROUP_BYS,
    GUARDRAIL_BLOCKED_BYTES_SQL, GUARDRAIL_COUNTS_SQL, MAX_POINTS, MAX_RANGE_MS, PROBE_KIND,
};
pub use query::{guardrail_sample_page, request_body, SampleRow, SampleSubject};
pub use query::{list_request_page, refusal_categories, RequestPage};
pub use quota::{
    lead_over_pace, polled, samples_from_poll, QuotaPoll, Sampled, POLL_INTERVAL,
    PROFILE_EVERY_POLLS,
};
pub use reconcile::{
    newest_rows, read_reconciliations, reconcile, ReconciliationRow, MIN_RISE, RATIO_HIGH,
    RATIO_LOW,
};
pub use record::{
    AttemptRecord, BodyRecord, CacheMapWrite, DecisionRow, EventRecord, ExtraUsageSampleRecord,
    GuardrailMatchRecord, KeepaliveDelta, ParkedRecord, QuotaSampleRecord, RequestRecord,
    SampleSource,
};
pub use refold::refold_bands;
pub use refusals::{
    is_refusal, refold_refusals, REFUSALS_BACKFILL_CURSOR_KEY, REFUSALS_BACKFILL_KEY, REFUSAL_CODES,
};
pub use rollup::{
    bucket_of, deltas_of, fold_account, fold_effort, fold_event, fold_identifier, fold_keepalive,
    fold_request, fold_routing_conditions, fold_user_agent, hour_of, read_account_rollups,
    read_rollups, read_rollups_paged, AccountRollupRow, CostSplit, KeySelector, RollupRow,
    RollupSelect, BILLED_FUNDING, CEILING_SETTING, DESTINATION_SPENT_KIND, DIMENSION_MAX_BYTES,
    EFFORTS, FAST_FUNDING, KEY_RANGE_PREDICATE, LONG_BAND, METER_DRIFT_KIND, NO_CLASS,
    OTHER_DIMENSION, OVERAGE_FUNDING, PAGE_HOURS, PLAN_FUNDING, SHED_NO_RUNWAY_KIND,
    SLOWED_LONG_KIND, SLOW_SETTINGS, STANDARD_BAND, UNATTRIBUTED, USAGE_BASED_FUNDING,
    WALL_REACHED_KIND, WALL_SETTINGS,
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
