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
    ConversationMatch, ConversationQuery, PredecessorQuery, PurgedEntry, RungWalk, Visited,
};
pub use guardrails::CompiledGuardrails;
pub use phoebis_store::canonical_id as canonical_model;
pub use record::{
    BudgetFacts, By, CacheMapEntry, CandidateFacts, ChainFacts, CooldownFacts, CreditsFacts,
    DecisionRecord, DecisionSummary, ExhaustionFacts, Fact, FactEntity, FactValue, HoldOutcome,
    LaneFacts, MarkClearedFacts, MoveCause, ObstacleFacts, Outcome, OverageFacts,
    PaceFacts, PinEventFacts, PinFacts, PinKeyFacts, PinShed, PinWritten, PrefixEntry,
    PrefixLookup, RefusalFacts, RefusedRequest, RequestFacts, RequestFlags, ReservationFacts,
    Score, Settings, ShedPinFacts, ShedRoundFacts, SummaryFact, TermFacts, WalkStep, WarmthFacts,
    WindowFacts, PARTITION_VALUES, RECORD_VERSION, WALK_STEPS,
};
pub use resolve::{resolve, Candidate, ResolveError};
pub use select::{
    cohort_hold, decide, effective_warm, elapsed_share_at, exhaustion_status, lane_key,
    release_reasons, seconds_ms, select, warmth_of, Banded, ClientClass, CohortHold, Decided,
    Exhaustion, FailedOver, HoldFacts, HoldWait, MigrateReason, PinHold, Placement, Refusal,
    SelectRequest, SelectResult, Selection, SelectionTrace, ShedRoundRecord, Skipped, SlowDownRead,
    Warmth, WarmthCause, BUDGET_WINDOW, FAST_CREDITS_REASON, FAST_RATE_LIMIT_REASON,
    MODEL_REJECTED_REASON, REFRESHING_CLASSES, SLOW_DOWN_MIN_HOLD_MS, SLOW_DOWN_WAIT_MS,
};
pub use snapshot::{KeyRefusal, Presented, RegistrySnapshot};
pub use state::{
    budget_deadline, burn_average_ms, burn_decayed, ceiling, cooldown_holds, entrypoint_of,
    is_plan_window, load_shedding, return_to_plan_dwell_ms, shed_cap, shed_dwell_ms, slow_down,
    slow_down_hold_ms, slow_down_stretch_ms, warm, AccountTicket, BandSegment, BurnUnit,
    CreditsState, EvidenceSource, HeadMark, InFlight, KeyPermit, LongContextRates, MemberSpend,
    OverageEvidence, PinPace, PinRef, PinUsage, Purged, RateTable, Rates, Reservation,
    RoutingState, SharePricer, SlowDownEdge, SlowDownState, StateWrite, UtilizationSample,
    WindowReading, WindowSource, BUCKET_LIFE_MS, BUCKET_TTL_MS, CYCLE_GAPS, DEFAULT_WINDOW_CEILING,
    FAST_INPUT, FAST_OUTPUT, FIVE_HOUR, IN_FLIGHT_RECLAIM_MS, MAX_BUCKETS_PER_ACCOUNT, SEVEN_DAY,
    SEVEN_DAY_FABLE, WEEK_MS,
};
pub use task::{
    credits_from_usage, sample_from_usage, spawn, AccountProjection, AccountSlowDown, BandWeight,
    BurnRates, LoadCounts, PinStanding, RegimeRates, ReloadError, Report, RouterConfig,
    RouterEvent, RouterGone, RouterHandle, SelectAnswer, WindowProjection, WriterStatus, COMMANDS,
    SPENT_MARGIN, UNATTRIBUTED_POOL, WRITER_DEPTH,
};
#[cfg(any(test, feature = "debug-hooks"))]
pub use task::{LoopPark, ReportHold};
pub use windows::window_for;
