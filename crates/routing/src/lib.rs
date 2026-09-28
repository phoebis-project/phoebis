//! The router task: the registry snapshot the relay authenticates against, model resolution, the account
//! selection that owns pins, cooldowns, and overage state, and the listing a caller's key may route to.

mod affinity;
pub mod listing;
pub mod pattern;
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
    ConversationMatch, ConversationQuery,
};
pub use phoebis_store::canonical_id as canonical_model;
pub use resolve::{resolve, Candidate, ResolveError};
pub use select::{
    effective_warm, release_reasons, select, warmth_of, ClientClass, CohortHold, Exhaustion,
    HoldFacts, MigrateReason, Placement, Refusal, SelectRequest,
    SelectResult, Selection, SelectionTrace, Skipped, Warmth, MIGRATE_BAND, MODEL_REJECTED_REASON,
    REFRESHING_CLASSES,
};
pub use snapshot::{KeyRefusal, Presented, RegistrySnapshot};
pub use state::{
    burn_decayed, shed_marks, warm, AccountTicket, BurnUnit, EvidenceSource, InFlight, KeyPermit,
    MemberSpend, OverageEvidence, PinRef, PinUsage, Purged, RateTable, Rates, RoutingState,
    StateWrite, UtilizationSample, WindowReading, FIVE_HOUR, IN_FLIGHT_RECLAIM_MS, SEVEN_DAY,
    SEVEN_DAY_FABLE,
};
pub use task::{
    sample_from_usage, spawn, BandWeight, BurnRates, ReloadError, Report, RouterConfig,
    RouterEvent, RouterGone, RouterHandle, SelectAnswer,
};
pub use windows::window_for;
