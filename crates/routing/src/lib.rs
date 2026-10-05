//! The router task: the registry snapshot the relay authenticates against, model resolution, the account
//! selection that owns pins, cooldowns, and overage state, and the listing a caller's key may route to.

mod affinity;
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
    ConversationMatch, ConversationQuery, RungWalk, Visited,
};
pub use phoebis_store::canonical_id as canonical_model;
pub use record::{
    BudgetFacts, By, CandidateFacts, ChainFacts, CooldownFacts, CreditsFacts, DecisionRecord,
    DecisionSummary, ExhaustionFacts, Fact, FactEntity, FactValue, HoldOutcome,
    LaneFacts, ObstacleFacts, Outcome, OverageFacts, PaceFacts, PinEventFacts, PinFacts,
    PinKeyFacts, PinShed, PinWritten, PrefixEntry, PrefixLookup, RefusalFacts, RefusedRequest,
    RequestFacts, RequestFlags, Score, Settings, SummaryFact, TermFacts, WalkStep, WarmthFacts,
    WindowFacts, RECORD_VERSION,
};
pub use resolve::{resolve, Candidate, ResolveError};
pub use select::{
    cohort_hold, decide, effective_warm, lane_key, release_reasons, seconds_ms, select, warmth_of,
    ClientClass, CohortHold, Decided, Exhaustion, HoldFacts, MigrateReason,
    Placement, Refusal, SelectRequest, SelectResult, Selection, SelectionTrace, Skipped, Warmth,
    WarmthCause, BUDGET_WINDOW, FAST_CREDITS_REASON, FAST_RATE_LIMIT_REASON, MIGRATE_BAND,
    MODEL_REJECTED_REASON, REFRESHING_CLASSES,
};
pub use snapshot::{KeyRefusal, Presented, RegistrySnapshot};
pub use state::{
    budget_deadline, burn_decayed, cooldown_holds, is_plan_window, shed_marks, warm, AccountTicket,
    BurnUnit, CreditsState, EvidenceSource, InFlight, KeyPermit, MemberSpend, OverageEvidence,
    PinRef, PinUsage, Purged, RateTable, Rates, RoutingState, SharePricer, StateWrite,
    UtilizationSample, WindowReading, WindowSource, BUCKET_LIFE_MS, BUCKET_TTL_MS, FAST_INPUT,
    FAST_OUTPUT, FIVE_HOUR, IN_FLIGHT_RECLAIM_MS, MAX_BUCKETS_PER_ACCOUNT, SEVEN_DAY,
    SEVEN_DAY_FABLE, WEEK_MS,
};
pub use task::{
    credits_from_usage, sample_from_usage, spawn, BandWeight, BurnRates, ReloadError, Report,
    RouterConfig, RouterEvent, RouterGone, RouterHandle, SelectAnswer, WriterStatus, COMMANDS,
    UNATTRIBUTED_POOL, WRITER_DEPTH,
};
#[cfg(any(test, feature = "debug-hooks"))]
pub use task::{LoopPark, ReportHold};
pub use windows::window_for;
