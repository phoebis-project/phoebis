//! The router task: the registry snapshot the relay authenticates against, model resolution, and the account
//! selection that owns pins, cooldowns, and overage state.

mod resolve;
mod select;
mod snapshot;
mod state;
mod task;

pub use phoebis_store::canonical_id as canonical_model;
pub use resolve::{resolve, Candidate, ResolveError};
pub use select::{
    release_reasons, select, Exhaustion, MigrateReason, SelectRequest, SelectResult, Selection,
    MIGRATE_BAND, MIGRATE_DWELL_MS,
};
pub use snapshot::{KeyRefusal, RegistrySnapshot};
pub use state::{
    EvidenceSource, OverageEvidence, PinRef, Purged, RoutingState, StateWrite, UtilizationSample,
};
pub use task::{
    sample_from_usage, spawn, ReloadError, Report, RouterConfig, RouterEvent, RouterGone,
    RouterHandle, SelectAnswer,
};
