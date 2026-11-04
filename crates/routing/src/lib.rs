//! The router task: the registry snapshot the relay authenticates against, model resolution, the account
//! selection that owns pins, cooldowns, and overage state, and the listing a caller's key may route to.

mod affinity;
mod guardrails;
pub mod listing;
pub mod pattern;
mod record;
mod resolve;
pub mod rules;
mod select;
mod snapshot;
mod state;
mod task;
pub mod term;
pub mod windows;

pub use affinity::{
    AffinityChain, AffinityEntry, AffinityMap, AffinityReport, AffinityRow, ConversationLookup,
    ConversationMatch, ConversationQuery, PredecessorQuery, PurgedEntry, FIVE_MINUTES_MS,
    ONE_HOUR_MS,
};
pub use guardrails::CompiledGuardrails;
pub use phoebis_store::canonical_id as canonical_model;
pub use record::{
    BudgetFacts, By, CacheMapEntry, CandidateFacts, ChainFacts, CooldownFacts, CreditsFacts,
    DecisionRecord, DecisionSummary, ExhaustionFacts, Fact, FactEntity, FactValue, HoldOutcome,
    LaneFacts, MarkClearedFacts, MoveCause, ObstacleFacts, Outcome, OverageFacts, PaceFacts,
    PinEventFacts, PinFacts, PinKeyFacts, PinShed, PinWritten, PrefixEntry, PrefixLookup,
    RefusalFacts, RefusedRequest, RequestFacts, RequestFlags, ReservationFacts, Score, Settings,
    ShedPinFacts, ShedRoundFacts, SummaryFact, TermFacts, WalkStep, WarmthFacts, WindowFacts,
    PARTITION_VALUES, RECORD_VERSION, WALK_STEPS,
};
pub use resolve::{resolve, Candidate, ResolveError};
pub use select::{
    cohort_hold, decide, effective_warm, elapsed_share_at, exhaustion_status, lane_key,
    release_reasons, routed_agent, seconds_ms, select, warmth_of, Banded, ClientClass, CohortHold,
    CreditAccount, Decided, Exhaustion, FailedOver, HoldFacts, HoldWait, MigrateReason, Obstacle,
    PinHold, PinnedObstacle, Placement, Refusal, SelectRequest, SelectResult, Selection,
    SelectionTrace, Skipped, SlowDownRead, Warmth, WarmthCause, BUDGET_WINDOW, FAST_CREDITS_REASON,
    FAST_RATE_LIMIT_REASON, MAIN_AGENT, MODEL_REJECTED_REASON, RATE_LIMIT_REASON,
    REFRESHING_CLASSES, RETRY_AFTER_WINDOW, SLOW_DOWN_MIN_HOLD_MS, SLOW_DOWN_WAIT_MS,
};
pub use snapshot::{paid_source, KeyRefusal, Presented, RegistrySnapshot};
pub use state::{
    budget_deadline, burn_average_ms, burn_decayed, ceiling, cooldown_holds, entrypoint_of,
    is_plan_window, latest_full_reset, load_shedding, plan_windows, return_to_plan_dwell_ms,
    shed_cap, shed_dwell_ms, slow_down, slow_down_hold_ms, slow_down_stretch_ms,
    spend_reading_reset, spend_reset_estimate, wait_until, warm, AccountTicket, BandSegment,
    BurnUnit, CreditsState, EvidenceSource, HeadMark, InFlight, KeyPermit, LongContextRates,
    MemberSpend, OverageEvidence, PinPace, PinRef, PinUsage, Purged, RateTable, Rates, Reservation,
    RoutingState, SharePricer, SlowDownEdge, SlowDownState, SpendResetSource, SpendWindow,
    StateWrite, UtilizationSample, WindowReading, WindowSource, BUCKET_LIFE_MS, BUCKET_TTL_MS,
    CYCLE_GAPS, DAY_MS, FAST_INPUT, FAST_OUTPUT, FIVE_HOUR, HOUR_MS, INPUT_TOKENS_BUCKET,
    IN_FLIGHT_RECLAIM_MS, MAX_BUCKETS_PER_ACCOUNT, OUTPUT_TOKENS_BUCKET, REQUESTS_BUCKET,
    SEVEN_DAY, SEVEN_DAY_FABLE, WEEK_MS,
};
pub use task::{
    credits_from_usage, same_reset, sample_from_usage, spawn, AccountProjection, AccountSlowDown,
    BandWeight, BurnRates, LoadCounts, PinStanding, RegimeRates, ReloadError, Report, RouterConfig,
    RouterEvent, RouterGone, RouterHandle, SelectAnswer, WindowProjection, WriterStatus, COMMANDS,
    PIN_CREATED, PIN_EXPIRED, PIN_MIGRATED, RESET_SLACK_MS, SPENT_MARGIN, UNATTRIBUTED_POOL,
    WRITER_DEPTH,
};
#[cfg(any(test, feature = "debug-hooks"))]
pub use task::{LoopPark, ReportHold};
pub use windows::{
    account_window_id, model_week, window_for, window_length_ms, SEVEN_DAY_OVERAGE_INCLUDED,
};
