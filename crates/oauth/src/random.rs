//! The one source of the bytes the pairing, the admin listener and the usage service mint: the key secrets, the limit
//! reset claim's request id, the pairing session's id, state, PKCE verifier and redirect port, the session compare key,
//! and the session tokens. A test rig seeds it so two captures of the same registry read the same. Its version 4 UUID
//! formatter also writes the relay's `x-client-request-id`, whose bits the relay draws from its own source.

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

/// A version 4 UUID from 128 drawn bits: the version set to 4 and the variant to the two bits RFC 9562 fixes, in the
/// lowercase 8-4-4-4-12 form.
pub fn uuid_v4_of(bits: u128) -> String {
    let bits = (bits & !(0xf << 76)) | (0x4 << 76);
    let bits = (bits & !(0x3 << 62)) | (0x2 << 62);
    let hex = format!("{bits:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
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

#[cfg(test)]
mod tests {
    use super::uuid_v4_of;

    #[test]
    fn the_formatter_stamps_the_version_and_the_variant_over_any_drawn_bits() {
        assert_eq!(
            [uuid_v4_of(u128::MAX), uuid_v4_of(0)],
            [
                "ffffffff-ffff-4fff-bfff-ffffffffffff",
                "00000000-0000-4000-8000-000000000000"
            ]
        );
    }
}
