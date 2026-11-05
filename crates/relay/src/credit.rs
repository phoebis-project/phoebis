//! The fallback credit tokens the upstream issued with a refusal, each under the account that answered it, so the
//! next body carrying one is placed where the credit applies. Process memory alone: nothing persists an entry, and a
//! restart starts empty.

use std::collections::HashMap;
use std::sync::Mutex;

use phoebis_routing::{CreditAccount, HOUR_MS};
use sha2::{Digest, Sha256};

/// Entries held at most, the oldest leaving at the cap.
pub(crate) const CREDIT_CAP: usize = 4_096;
/// How long an entry stands: the credit is spent by the next turn, and a session that never sends it holds the
/// entry for nothing.
pub(crate) const CREDIT_TTL_MS: i64 = HOUR_MS;

pub(crate) type CreditDigest = [u8; 32];

// A poisoned store mutex means a prior panic happened mid-update.
const CREDITS_POISONED: &str = "credit store mutex poisoned by a prior panic";

/// SHA-256 of the token's bytes, the one form of the token the relay keeps.
pub(crate) fn digest_of(token: &str) -> CreditDigest {
    Sha256::digest(token.as_bytes()).into()
}

/// The digest as the bus and the ledger carry it, lowercase hex.
pub(crate) fn hex_of(digest: &CreditDigest) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
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

pub(crate) struct CreditStore {
    inner: Mutex<HashMap<CreditDigest, CreditEntry>>,
}

impl CreditStore {
    pub(crate) fn new() -> CreditStore {
        CreditStore {
            inner: Mutex::new(HashMap::new()),
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
        let mut entries = self.inner.lock().expect(CREDITS_POISONED);
        entries.retain(|_, entry| entry.live(now));
        if entries.len() >= CREDIT_CAP && !entries.contains_key(&digest) {
            let oldest = entries
                .iter()
                .min_by_key(|(_, entry)| entry.recorded_at)
                .map(|(held, _)| *held);
            if let Some(oldest) = oldest {
                entries.remove(&oldest);
            }
        }
        let entry = CreditEntry {
            issuer: CreditAccount {
                account_id: account_id.to_string(),
                incarnation,
            },
            recorded_at: now,
        };
        entries.insert(digest, entry);
    }

    /// The issuing account and its incarnation for a digest recorded less than `CREDIT_TTL_MS` ago, else `None`.
    pub(crate) fn issuer(&self, digest: &CreditDigest, now: i64) -> Option<CreditAccount> {
        let entries = self.inner.lock().expect(CREDITS_POISONED);
        entries
            .get(digest)
            .filter(|entry| entry.live(now))
            .map(|entry| entry.issuer.clone())
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.lock().expect(CREDITS_POISONED).len()
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
        // The second record stamped the entry anew, so the cap reaches the token recorded between the two first.
        for n in 0..CREDIT_CAP - 1 {
            store.record(digest_of(&format!("cred-{n}")), "c", 1, 3 + n as i64);
        }
        assert_eq!(store.len(), CREDIT_CAP);
        let now = 3 + CREDIT_CAP as i64;
        assert_eq!(store.issuer(&digest_of("cred-first"), now), None);
        assert_eq!(store.issuer(&digest_of("cred-abc"), now), issued("b", 2));
    }
}
