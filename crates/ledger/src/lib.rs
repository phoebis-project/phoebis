//! The statistics ledger: `ledger.sqlite`, written by one task from what the bus and the router publish, and
//! read by the admin routes and the metrics listener.

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

pub use assemble::{Assembler, Expired, Record, IN_FLIGHT_TTL_MS};
pub use bodies::{
    run_retention, run_retention_blocking, Purged, MAX_CAPTURED_BODY, PURGE_SLICE,
    PURGE_SLICE_BYTES,
};
pub use db::LedgerDb;
pub use events::{catalog_changed, event_of, poll_backoff, spawn_event_mirror, MirrorBaseline};
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
pub use plan::{
    entrypoint_of_client, free_equivalent, free_shares, funding_of, price_equivalent, price_shares,
    Capacity, CostRevisions, DefinedBy, Funding, ModelWindow, Override, Peak, PerWindow,
    PlanTables, PricedShares, Regime, RegimePeak, SubscriptionCosts, TypeMultiplier, TypeOverride,
    Unit, WindowShares,
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
pub use query::{
    breakdown_paged, bucket_seconds, burn_groups_paged, burn_series_over, dashboard,
    decision_record, group_of, guardrail_counts, held_series, hour_ceil, hourly_window_samples,
    key_request_totals, latest_extra_usage, latest_quota, list_requests, list_routing_log,
    literal_like, member_spends, moves_by_reason, moves_by_reason_and_fit, names_one_account,
    parked_count, parked_summary, parse_range, pin_migrations_by_reason, placements, pool_traffic,
    recent_events_of_kind, recent_gaps, request_by_id, request_events, request_totals,
    revision_spans, rewrite_series, routing_conditions,
    series_paged, session_activity, session_burn_series, session_identities,
    BudgetedMember, BurnPoint, CacheEquivalent, CacheFacts, CacheShare, Dashboard, DecisionListRow,
    Equivalent, EventListRow, FundingFilter, GroupRow, GuardrailCountRow, HourlyWindowSample,
    LiveKeys, ParkedSummary, PlacementRow, ProbeCounts, ProbeEquivalent, ProbeShare, RequestFilter,
    RollupFilter, RoutingCondition, RoutingLogCursor, RoutingLogFilter, RoutingLogPage,
    RoutingLogRow, SeriesFilter, SeriesPoint, SessionActivity, SessionIdentity, SharePricing,
    SharedEquivalent, Span, Totals, TrafficBucket, DECISION_KINDS, EVENT_KINDS, GROUP_BYS,
    GUARDRAIL_BLOCKED_BYTES_SQL, GUARDRAIL_COUNTS_SQL, MAX_POINTS, MAX_RANGE_MS, PROBE_KIND,
};
pub use query::{guardrail_sample_page, request_body, SampleRow, SampleSubject};
pub use query::{list_request_page, RequestPage};
pub use query::{agent_models, AgentModel};
pub use quota::{
    lead_over_pace, polled, samples_from_head, samples_from_poll, QuotaPoll, Sampled,
    POLL_INTERVAL, PROFILE_EVERY_POLLS,
};
pub use reconcile::{
    newest_rows, read_reconciliations, reconcile, ReconciliationRow, MIN_RISE, RATIO_HIGH,
    RATIO_LOW,
};
pub use record::{
    AttemptRecord, BodyRecord, CacheMapWrite, DecisionRow, EventRecord, ExtraUsageSampleRecord,
    GuardrailMatchRecord, KeepaliveDelta, ParkedRecord, QuotaSampleRecord, RequestRecord,
};
pub use refold::{refold_bands, PRICE_BANDS_KEY, PRICE_BANDS_REFOLD_KEY};
pub use refusals::{
    is_refusal, refold_refusals, REFUSALS_BACKFILL_CURSOR_KEY, REFUSALS_BACKFILL_KEY, REFUSAL_CODES,
};
pub use rollup::{
    bucket_of, deltas_of, fold_account, fold_effort, fold_event, fold_identifier, fold_keepalive,
    fold_request, fold_routing_conditions, fold_user_agent, hour_of, read_account_rollups,
    read_rollups, read_rollups_paged, AccountRollupRow, CostSplit, KeySelector, RollupRow,
    RollupSelect, BILLED_FUNDING, DIMENSION_MAX_BYTES, EFFORTS, FAST_FUNDING, KEY_RANGE_PREDICATE,
    LONG_BAND, NO_CLASS, OTHER_DIMENSION, OVERAGE_FUNDING, PAGE_HOURS, PLAN_FUNDING, STANDARD_BAND,
    UNATTRIBUTED,
};
pub use task::{
    spawn, Admission, Ledger, LedgerConfig, LedgerGone, LedgerStats, Replayed, UnweightedModel,
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
    /// call it happened to: `LedgerDb::read_blocking` recovers a poisoned guard, so the next read on the same
    /// `LedgerDb` runs as if nothing happened.
    #[error("the ledger read was cancelled before it ran")]
    ReadCancelled,
    /// A replay's bound passed before the writer took its turn, so nothing was replayed.
    #[error("the ledger writer is held, try again")]
    WriterHeld,
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("plan table: {0}")]
    Plan(String),
    #[error("pricing table: {0}")]
    Yaml(String),
    /// A file a newer build wrote: opening it would run no migration and read columns this build does not know. The
    /// message leaves `path` to the caller, whose context names the file.
    #[error(
        "the ledger's schema version {found} is newer than this build supports ({supported}), \
         so restore the backup taken before the upgrade"
    )]
    SchemaTooNew {
        path: std::path::PathBuf,
        found: u32,
        supported: u32,
    },
}

impl LedgerError {
    /// Whether the failure is the database's rather than one row's: a lock held past the busy timeout, a full disk,
    /// an I/O error, a closed handle, or a file that cannot be opened, is read-only (moved or deleted under the
    /// router among them), corrupt or no database at all. Every write after it meets it too, so the record is held
    /// rather than read as a poison row.
    pub fn is_database_wide(&self) -> bool {
        matches!(
            self,
            LedgerError::Sqlite(rusqlite::Error::SqliteFailure(failure, _))
                if matches!(
                    failure.code,
                    rusqlite::ErrorCode::ApiMisuse
                        | rusqlite::ErrorCode::CannotOpen
                        | rusqlite::ErrorCode::DatabaseBusy
                        | rusqlite::ErrorCode::DatabaseCorrupt
                        | rusqlite::ErrorCode::DiskFull
                        | rusqlite::ErrorCode::NotADatabase
                        | rusqlite::ErrorCode::ReadOnly
                        | rusqlite::ErrorCode::SystemIoFailure
                )
        )
    }
}

#[cfg(test)]
mod tests {
    use super::LedgerError;

    fn failure(code: std::ffi::c_int) -> LedgerError {
        LedgerError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(code),
            None,
        ))
    }

    #[test]
    fn a_file_the_database_cannot_read_or_write_is_database_wide_and_a_refused_row_is_not() {
        for code in [
            rusqlite::ffi::SQLITE_BUSY,
            rusqlite::ffi::SQLITE_CANTOPEN,
            rusqlite::ffi::SQLITE_CORRUPT,
            rusqlite::ffi::SQLITE_FULL,
            rusqlite::ffi::SQLITE_IOERR_WRITE,
            rusqlite::ffi::SQLITE_MISUSE,
            rusqlite::ffi::SQLITE_NOTADB,
            rusqlite::ffi::SQLITE_READONLY,
            rusqlite::ffi::SQLITE_READONLY_DBMOVED,
        ] {
            let error = failure(code);
            assert!(error.is_database_wide(), "{code}: {error}");
        }
        for code in [
            rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER,
            rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE,
            rusqlite::ffi::SQLITE_TOOBIG,
        ] {
            let error = failure(code);
            assert!(!error.is_database_wide(), "{code}: {error}");
        }
    }
}
