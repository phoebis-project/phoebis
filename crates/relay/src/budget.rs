//! The bounds on request bodies before they are read: the bytes the whole process holds at once, and the bodies
//! one key has arriving at once.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// The process-wide bytes of request bodies in flight, shared by every connection of the relay.
pub struct BodyBudget {
    held: AtomicU64,
    cap: u64,
}

impl BodyBudget {
    pub fn new(cap: u64) -> Arc<BodyBudget> {
        Arc::new(BodyBudget {
            held: AtomicU64::new(0),
            cap,
        })
    }

    /// The reservation, or `None` when `bytes` would take the budget past its cap.
    pub fn try_reserve(self: &Arc<Self>, bytes: u64) -> Option<Reservation> {
        self.grow(bytes).ok()?;
        Some(Reservation {
            budget: self.clone(),
            bytes,
        })
    }

    /// The check and the add are one step, so two reservations racing for the last bytes never both land.
    fn grow(&self, bytes: u64) -> Result<(), ()> {
        self.held
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                held.checked_add(bytes).filter(|&total| total <= self.cap)
            })
            .map(|_| ())
            .map_err(|_| ())
    }

    fn shrink(&self, bytes: u64) {
        self.held.fetch_sub(bytes, Ordering::AcqRel);
    }

    #[cfg(test)]
    fn held(&self) -> u64 {
        self.held.load(Ordering::Acquire)
    }
}

/// Released on drop; `resize` moves it to the bytes the body turned out to hold (grown to an inflated size, it
/// reads `Err` when the budget cannot grant the growth).
pub struct Reservation {
    budget: Arc<BodyBudget>,
    bytes: u64,
}

impl Reservation {
    /// A shrink never fails; a refused growth leaves the reservation as it was.
    pub fn resize(&mut self, bytes: u64) -> Result<(), ()> {
        if bytes > self.bytes {
            self.budget.grow(bytes - self.bytes)?;
        } else {
            self.budget.shrink(self.bytes - bytes);
        }
        self.bytes = bytes;
        Ok(())
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.shrink(self.bytes);
    }
}

/// The bodies arriving at once per key; a permit is released on drop, when the read ends.
#[derive(Default)]
pub struct BodyReads {
    counts: Arc<Mutex<HashMap<String, u32>>>,
}

impl BodyReads {
    /// The permit, or `None` when `key_id` already has `bound` bodies arriving.
    pub fn try_acquire(&self, key_id: &str, bound: u32) -> Option<ReadPermit> {
        let mut counts = self.counts.lock().expect("body read counts poisoned");
        let held = counts.get(key_id).copied().unwrap_or(0);
        if held >= bound {
            return None;
        }
        counts.insert(key_id.to_string(), held + 1);
        Some(ReadPermit {
            counts: self.counts.clone(),
            key_id: key_id.to_string(),
        })
    }
}

pub struct ReadPermit {
    counts: Arc<Mutex<HashMap<String, u32>>>,
    key_id: String,
}

impl Drop for ReadPermit {
    fn drop(&mut self) {
        let mut counts = self.counts.lock().expect("body read counts poisoned");
        if let Some(held) = counts.get_mut(&self.key_id) {
            *held -= 1;
            if *held == 0 {
                counts.remove(&self.key_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reservation_past_the_cap_is_refused_and_a_dropped_one_frees_its_bytes() {
        let budget = BodyBudget::new(100);
        let first = budget.try_reserve(60).expect("under the cap");
        assert!(budget.try_reserve(41).is_none(), "past the cap");
        let second = budget.try_reserve(40).expect("exactly at the cap");
        assert_eq!(budget.held(), 100);
        drop(first);
        assert_eq!(budget.held(), 40);
        drop(second);
        assert_eq!(budget.held(), 0);
    }

    #[test]
    fn a_resize_shrinks_freely_and_grows_only_within_the_cap() {
        let budget = BodyBudget::new(100);
        let mut reservation = budget.try_reserve(80).unwrap();
        assert_eq!(reservation.resize(10), Ok(()));
        assert_eq!(budget.held(), 10);
        let other = budget.try_reserve(50).unwrap();
        assert_eq!(reservation.resize(51), Err(()), "51 and 50 pass the cap");
        assert_eq!(
            budget.held(),
            60,
            "a refused growth leaves the reservation as it was"
        );
        assert_eq!(reservation.resize(50), Ok(()));
        assert_eq!(budget.held(), 100);
        drop(other);
        drop(reservation);
        assert_eq!(budget.held(), 0);
    }

    #[test]
    fn a_key_holds_at_most_its_bound_of_reads_and_a_dropped_permit_frees_one() {
        let reads = BodyReads::default();
        let first = reads.try_acquire("k", 2).expect("under the bound");
        let _second = reads.try_acquire("k", 2).expect("at the bound");
        assert!(reads.try_acquire("k", 2).is_none(), "past the bound");
        assert!(
            reads.try_acquire("other", 2).is_some(),
            "the bound is per key"
        );
        drop(first);
        assert!(
            reads.try_acquire("k", 2).is_some(),
            "the dropped permit freed one"
        );
    }
}
