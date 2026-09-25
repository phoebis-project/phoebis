//! The router task: the registry snapshot the relay authenticates against, model resolution, the account
//! selection that owns pins, cooldowns, and overage state, and the listing a caller's key may route to.

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

pub use phoebis_store::canonical_id as canonical_model;
pub use resolve::{resolve, Candidate, ResolveError};
pub use select::{
    release_reasons, select, Exhaustion, MigrateReason, SelectRequest, SelectResult, Selection,
    SelectionTrace, Skipped, MIGRATE_BAND, MODEL_REJECTED_REASON,
};
pub use snapshot::{KeyRefusal, Presented, RegistrySnapshot};
pub use state::{
    AccountTicket, EvidenceSource, InFlight, KeyPermit, OverageEvidence, PinRef, Purged,
    RoutingState, StateWrite, UtilizationSample, WindowReading, FIVE_HOUR, IN_FLIGHT_RECLAIM_MS,
    SEVEN_DAY, SEVEN_DAY_FABLE,
};
pub use task::{
    sample_from_usage, spawn, ReloadError, Report, RouterConfig, RouterEvent, RouterGone,
    RouterHandle, SelectAnswer,
};
pub use windows::window_for;
