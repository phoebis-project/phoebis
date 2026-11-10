//! The one source of the bytes the pairing, the admin listener and the usage service mint: the key secrets, the limit
//! reset claim's request id, the pairing session's id, state, PKCE verifier and redirect port, the session compare key,
//! and the session tokens. A test rig seeds it so two captures of the same registry read the same.

use std::ops::RangeInclusive;
#[cfg(any(test, feature = "test-support"))]
use std::sync::{Mutex, MutexGuard, OnceLock};

use rand::Rng;
#[cfg(any(test, feature = "test-support"))]
use rand::{rngs::StdRng, RngExt, SeedableRng};

const DYNAMIC_PORTS: RangeInclusive<u16> = 49_152..=65_535;

#[cfg(any(test, feature = "test-support"))]
const LOCK_POISONED: &str = "seeded source mutex poisoned by a prior panic";

/// Fills `bytes` from the process CSPRNG, or from the seeded generator a rig set under `test-support`.
pub fn fill(bytes: &mut [u8]) {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(rng) = seeded().as_mut() {
        rng.fill_bytes(bytes);
        return;
    }
    // `rand::rng()` is the thread-local CSPRNG that replaced `rand::rngs::OsRng` in rand 0.10.
    rand::rng().fill_bytes(bytes);
}

/// A port of the IANA dynamic range, from the same source.
pub fn ephemeral_port() -> u16 {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(rng) = seeded().as_mut() {
        return rng.random_range(DYNAMIC_PORTS);
    }
    rand::random_range(DYNAMIC_PORTS)
}

/// Replaces the source by a generator seeded with `seed`, for a capture that must repeat. Every later mint reads
/// it in order, so the rig calls it once, before it seeds the registry.
#[cfg(any(test, feature = "test-support"))]
pub fn seed(seed: u64) {
    *seeded() = Some(StdRng::seed_from_u64(seed));
}

/// `None` until a rig seeds it, so a test that never seeds mints from the process CSPRNG as a release build does.
/// The mutex is not reentrant, so a caller holds the guard across one draw and never across a second mint.
#[cfg(any(test, feature = "test-support"))]
fn seeded() -> MutexGuard<'static, Option<StdRng>> {
    static SOURCE: OnceLock<Mutex<Option<StdRng>>> = OnceLock::new();
    SOURCE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect(LOCK_POISONED)
}
