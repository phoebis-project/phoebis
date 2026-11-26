//! The interval of the engine's save.

use std::time::Duration;

/// How often the engine writes its changed sessions, agents and counters to the registry. The shutdown writes
/// whatever is left.
pub const PERSIST_INTERVAL: Duration = Duration::from_secs(30);
