//! The fallback credit tokens the upstream issued with a refusal, each under the account that answered it, so the
//! next body carrying one is placed where the credit applies. Process memory alone: nothing persists an entry, and a
//! restart starts empty.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use phoebis_routing::CreditAccount;
use sha2::{Digest, Sha256};

/// Entries held at most, the oldest leaving at the cap.
pub(crate) const CREDIT_CAP: usize = 4_096;
/// How long an entry stands: the credit is spent by the next turn, and a session that never sends it holds the
/// entry for nothing.
pub(crate) const CREDIT_TTL_MS: i64 = 3_600_000;

pub(crate) type CreditDigest = [u8; 32];

// A poisoned store mutex means a prior panic happened mid-update, so the map and its order may disagree.
const CREDITS_POISONED: &str = "credit store mutex poisoned by a prior panic";

/// SHA-256 of the token's bytes, the one form of the token the relay keeps.
pub(crate) fn digest_of(token: &str) -> CreditDigest {
    Sha256::digest(token.as_bytes()).into()
}

struct CreditEntry {
    issuer: CreditAccount,
    recorded_at: i64,
}

impl CreditEntry {
    fn live(&self, now: i64) -> bool {
        now - self.recorded_at < CREDIT_TTL_MS
    }
}

#[derive(Default)]
struct Entries {
    entries: HashMap<CreditDigest, CreditEntry>,
    /// Each digest once, in the order of its latest record, so the front is the oldest and the first to expire.
    order: VecDeque<CreditDigest>,
}

pub(crate) struct CreditStore {
    inner: Mutex<Entries>,
}

impl CreditStore {
    pub(crate) fn new() -> CreditStore {
        CreditStore {
            inner: Mutex::new(Entries::default()),
        }
    }

    /// Records the issuer and its incarnation under the digest, dropping expired entries and then the oldest past the
    /// cap.
    pub(crate) fn record(
        &self,
        digest: CreditDigest,
        account_id: &str,
        incarnation: u64,
        now: i64,
    ) {
        let mut inner = self.inner.lock().expect(CREDITS_POISONED);
        let Entries { entries, order } = &mut *inner;
        while let Some(front) = order.front() {
            if entries.get(front).is_some_and(|entry| entry.live(now)) {
                break;
            }
            if let Some(expired) = order.pop_front() {
                entries.remove(&expired);
            }
        }
        let entry = CreditEntry {
            issuer: CreditAccount {
                account_id: account_id.to_string(),
                incarnation,
            },
            recorded_at: now,
        };
        // A digest recorded again takes the back of the queue, so the cap never evicts the newer record through the
        // older one's place. The scan is linear, which a refusal pays once at a bounded size.
        if entries.insert(digest, entry).is_some() {
            if let Some(at) = order.iter().position(|held| *held == digest) {
                order.remove(at);
            }
        }
        order.push_back(digest);
        while order.len() > CREDIT_CAP {
            if let Some(oldest) = order.pop_front() {
                entries.remove(&oldest);
            }
        }
    }

    /// The issuing account and its incarnation for a digest recorded less than `CREDIT_TTL_MS` ago, else `None`.
    pub(crate) fn issuer(&self, digest: &CreditDigest, now: i64) -> Option<CreditAccount> {
        let inner = self.inner.lock().expect(CREDITS_POISONED);
        inner
            .entries
            .get(digest)
            .filter(|entry| entry.live(now))
            .map(|entry| entry.issuer.clone())
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.lock().expect(CREDITS_POISONED).entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issued(account_id: &str, incarnation: u64) -> Option<CreditAccount> {
        Some(CreditAccount {
            account_id: account_id.into(),
            incarnation,
        })
    }

    #[test]
    fn an_entry_answers_its_issuer_and_incarnation() {
        let store = CreditStore::new();
        store.record(digest_of("cred-abc"), "b", 3, 1_000);
        assert_eq!(store.issuer(&digest_of("cred-abc"), 2_000), issued("b", 3));
        assert_eq!(
            store.issuer(&digest_of("cred-other"), 2_000),
            None,
            "a token the store never saw names no issuer"
        );
    }

    #[test]
    fn an_entry_recorded_an_hour_and_a_millisecond_ago_answers_none() {
        let store = CreditStore::new();
        store.record(digest_of("cred-abc"), "b", 1, 0);
        assert_eq!(
            store.issuer(&digest_of("cred-abc"), CREDIT_TTL_MS - 1),
            issued("b", 1),
            "a millisecond short of the hour the entry stands"
        );
        assert_eq!(
            store.issuer(&digest_of("cred-abc"), CREDIT_TTL_MS + 1),
            None
        );
        store.record(digest_of("cred-def"), "a", 1, CREDIT_TTL_MS + 1);
        assert_eq!(
            store.len(),
            1,
            "the next record drops the expired entry from memory"
        );
    }

    #[test]
    fn the_record_past_the_cap_drops_the_oldest() {
        let store = CreditStore::new();
        for n in 0..=CREDIT_CAP {
            store.record(digest_of(&format!("cred-{n}")), "b", 1, n as i64);
        }
        assert_eq!(store.len(), CREDIT_CAP);
        let now = CREDIT_CAP as i64;
        assert_eq!(store.issuer(&digest_of("cred-0"), now), None);
        assert_eq!(store.issuer(&digest_of("cred-1"), now), issued("b", 1));
        assert_eq!(
            store.issuer(&digest_of(&format!("cred-{CREDIT_CAP}")), now),
            issued("b", 1)
        );
    }

    #[test]
    fn the_same_digest_recorded_twice_counts_one_entry_and_answers_the_newer_issuer() {
        let store = CreditStore::new();
        store.record(digest_of("cred-abc"), "a", 1, 0);
        store.record(digest_of("cred-first"), "a", 1, 1);
        store.record(digest_of("cred-abc"), "b", 2, 2);
        assert_eq!(store.len(), 2);
        assert_eq!(store.issuer(&digest_of("cred-abc"), 3), issued("b", 2));
        // The second record took the back of the queue, so the cap reaches the token recorded between the two first.
        for n in 0..CREDIT_CAP - 1 {
            store.record(digest_of(&format!("cred-{n}")), "c", 1, 3 + n as i64);
        }
        assert_eq!(store.len(), CREDIT_CAP);
        let now = 3 + CREDIT_CAP as i64;
        assert_eq!(store.issuer(&digest_of("cred-first"), now), None);
        assert_eq!(store.issuer(&digest_of("cred-abc"), now), issued("b", 2));
    }
}
