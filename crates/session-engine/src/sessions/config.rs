//! The session engine's lease.

/// How long the companion's lease over a session lasts before the pool's idle limits take over.
pub const LEASE_SECONDS: u64 = 90;
pub const LEASE_MS: i64 = LEASE_SECONDS as i64 * 1_000;
