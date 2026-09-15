//! The router task: the registry snapshot the relay authenticates against, model resolution, and the account
//! selection that owns pins, cooldowns, and overage state.

mod resolve;
mod snapshot;

pub use phoebis_store::canonical_id as canonical_model;
pub use resolve::{resolve, Candidate, ResolveError};
pub use snapshot::{KeyRefusal, RegistrySnapshot};
