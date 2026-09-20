//! The in-process session memory behind `cc_prev_req` and the prompt id: what the relay last saw for a key,
//! session, and agent, and the per-account latch that remembers which injected field an upstream 400 has
//! already refused.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

use phoebis_store::AccountRow;

/// Entries idle this long are dropped, which is also the client's own session expiry default.
pub const MEMORY_IDLE_MS: i64 = 24 * 3_600_000;
/// Entries held at most; past it the least recently seen entry goes. Every string an entry holds is bounded
/// on the way in (the key's ids and the model by `identity`, the carried prompt id by `facts`, the upstream's
/// own ids by the response limits), so an entry is a few hundred bytes and the cap bounds memory at a few
/// tens of megabytes on a host that never restarts.
pub const MEMORY_CAP: usize = 65_536;

/// A minute between sweeps is often enough to keep the map from growing unbounded between requests, and rare
/// enough that a purge never costs a hot request its own lock hold.
const PURGE_INTERVAL_MS: i64 = 60_000;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MemoryKey {
    pub key_id: String,
    pub session_id: String,
    pub agent_id: String,
}

/// A field or beta the relay inserted on its own; what a 400 rule strips and a latch names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Injected {
    CacheDiagnosisBeta,
    Cch,
    ClientRequestId,
    Diagnostics,
    ExtendedTtlBeta,
    PrevReq,
    PromptId,
}

impl Injected {
    /// The text an upstream's 400 names it by.
    pub const fn wire_name(self) -> &'static str {
        match self {
            Injected::CacheDiagnosisBeta => "cache-diagnosis-2026-04-07",
            Injected::Cch => "cch",
            Injected::ClientRequestId => "x-client-request-id",
            Injected::Diagnostics => "diagnostics",
            Injected::ExtendedTtlBeta => "extended-cache-ttl-2025-04-11",
            Injected::PrevReq => "cc_prev_req",
            Injected::PromptId => "cc_prompt_id",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Previous {
    pub message_id: Option<String>,
    pub request_id: Option<String>,
}

struct AccountMemory {
    request_id: Option<String>,
    message_ids: HashMap<String, String>,
}

struct Entry {
    last_seen: i64,
    prompt_id: Option<String>,
    accounts: HashMap<String, AccountMemory>,
}

struct Latch {
    incarnation: u64,
    revision: u64,
    items: BTreeSet<Injected>,
}

struct Inner {
    entries: HashMap<MemoryKey, Entry>,
    latches: HashMap<String, Latch>,
    last_purge: i64,
}

pub struct SessionMemory {
    inner: Mutex<Inner>,
    cap: usize,
}

impl SessionMemory {
    pub fn new(cap: usize) -> SessionMemory {
        SessionMemory {
            inner: Mutex::new(Inner {
                entries: HashMap::new(),
                latches: HashMap::new(),
                last_purge: 0,
            }),
            cap,
        }
    }

    /// The prompt id this request carries: the one its own line already names, else inherited from
    /// `parent_agent` on an agent's first request, minted on a fresh user turn, or the agent's current one
    /// (minted when it has none). Only a client's own inference request reaches here: a probe replays the
    /// request as the client sent it.
    pub fn prompt_id(
        &self,
        key: &MemoryKey,
        parent_agent: Option<&str>,
        fresh_turn: bool,
        carried: Option<&str>,
        now: i64,
    ) -> String {
        self.purge(now);
        let mut inner = self.inner.lock().unwrap();
        let is_new = !inner.entries.contains_key(key);
        // Resolved before the entry is touched: a parent lookup and the child's own insertion must not borrow
        // the map at once.
        let inherited = is_new
            .then(|| {
                parent_agent.and_then(|agent| {
                    let parent_key = MemoryKey {
                        key_id: key.key_id.clone(),
                        session_id: key.session_id.clone(),
                        agent_id: agent.to_string(),
                    };
                    inner
                        .entries
                        .get(&parent_key)
                        .and_then(|e| e.prompt_id.clone())
                })
            })
            .flatten();
        if is_new {
            evict_if_full(&mut inner.entries, self.cap);
        }
        let entry = inner.entries.entry(key.clone()).or_insert_with(|| Entry {
            last_seen: now,
            prompt_id: None,
            accounts: HashMap::new(),
        });
        entry.last_seen = now;
        if let Some(id) = carried {
            // What the wire says wins: a full-shape turn sends its own id, and a mint recorded beside it
            // would be the one a reduced-shape subagent of that turn inherited.
            entry.prompt_id = Some(id.to_string());
        } else if is_new && inherited.is_some() {
            // The parent's id wins over a fresh-turn mint: a subagent's opening message reads as a fresh turn
            // of its own, but it is still the parent's turn the subagent was spawned to help with.
            entry.prompt_id = inherited;
        } else if fresh_turn || entry.prompt_id.is_none() {
            entry.prompt_id = Some(uuid_v4());
        }
        entry.prompt_id.clone().expect("just set above")
    }

    pub fn previous(&self, key: &MemoryKey, account_id: &str, canonical_model: &str) -> Previous {
        let inner = self.inner.lock().unwrap();
        let Some(account) = inner
            .entries
            .get(key)
            .and_then(|e| e.accounts.get(account_id))
        else {
            return Previous::default();
        };
        Previous {
            message_id: account.message_ids.get(canonical_model).cloned(),
            request_id: account.request_id.clone(),
        }
    }

    pub fn record_request_id(&self, key: &MemoryKey, account_id: &str, request_id: &str, now: i64) {
        self.purge(now);
        let mut inner = self.inner.lock().unwrap();
        let account = account_entry(&mut inner, key, account_id, now, self.cap);
        account.request_id = Some(request_id.to_string());
    }

    pub fn record_message_id(
        &self,
        key: &MemoryKey,
        account_id: &str,
        canonical_model: &str,
        message_id: &str,
        now: i64,
    ) {
        self.purge(now);
        let mut inner = self.inner.lock().unwrap();
        let account = account_entry(&mut inner, key, account_id, now, self.cap);
        account
            .message_ids
            .insert(canonical_model.to_string(), message_id.to_string());
    }

    /// Latched against the row the attempt dispatched with, by id, `incarnation`, and `revision`: a label
    /// patch bumps the revision, so an edit A to B to A with no traffic at B still moves it; a delete and a
    /// re-create under the same id moves the incarnation; a restart forgets it, since nothing here is persisted.
    /// Every other account write bumps the revision too (a pause, a rotation, a state flip), which clears the
    /// latch a round early; the cost is one more 400 before it is set again, and never a wrong skip.
    pub fn latch(&self, account: &AccountRow, item: Injected) {
        let mut inner = self.inner.lock().unwrap();
        let current = inner.latches.get(&account.id);
        let stale = current
            .is_none_or(|l| l.incarnation != account.incarnation || l.revision != account.revision);
        if stale {
            inner.latches.insert(
                account.id.clone(),
                Latch {
                    incarnation: account.incarnation,
                    revision: account.revision,
                    items: BTreeSet::new(),
                },
            );
        }
        inner
            .latches
            .get_mut(&account.id)
            .unwrap()
            .items
            .insert(item);
    }

    /// A late 400 for an attempt made under an older row lands nowhere: `latch` records the row it was given,
    /// and `refuses` matches the current row's incarnation and revision exactly.
    pub fn refuses(&self, account: &AccountRow, item: Injected) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.latches.get(&account.id).is_some_and(|latch| {
            latch.incarnation == account.incarnation
                && latch.revision == account.revision
                && latch.items.contains(&item)
        })
    }

    /// Drops entries idle past `MEMORY_IDLE_MS`; run by every write, at most once a minute.
    pub fn purge(&self, now: i64) {
        let mut inner = self.inner.lock().unwrap();
        purge_locked(&mut inner, now);
    }
}

/// The entry for `(key, account_id)`, inserting either level fresh when absent.
fn account_entry<'a>(
    inner: &'a mut Inner,
    key: &MemoryKey,
    account_id: &str,
    now: i64,
    cap: usize,
) -> &'a mut AccountMemory {
    let is_new = !inner.entries.contains_key(key);
    if is_new {
        evict_if_full(&mut inner.entries, cap);
    }
    let entry = inner.entries.entry(key.clone()).or_insert_with(|| Entry {
        last_seen: now,
        prompt_id: None,
        accounts: HashMap::new(),
    });
    entry.last_seen = now;
    entry
        .accounts
        .entry(account_id.to_string())
        .or_insert_with(|| AccountMemory {
            request_id: None,
            message_ids: HashMap::new(),
        })
}

fn evict_if_full(entries: &mut HashMap<MemoryKey, Entry>, cap: usize) {
    if entries.len() < cap {
        return;
    }
    if let Some(oldest) = entries
        .iter()
        .min_by_key(|(_, e)| e.last_seen)
        .map(|(k, _)| k.clone())
    {
        entries.remove(&oldest);
    }
}

fn purge_locked(inner: &mut Inner, now: i64) {
    if now - inner.last_purge < PURGE_INTERVAL_MS {
        return;
    }
    inner.last_purge = now;
    inner
        .entries
        .retain(|_, e| now - e.last_seen < MEMORY_IDLE_MS);
}

/// `rand::random`'s own bits, restamped with the version and variant nibbles RFC 4122 fixes for a v4 UUID.
pub(crate) fn uuid_v4() -> String {
    let v: u128 = rand::random();
    let v = (v & !(0xF_u128 << 76)) | (4_u128 << 76);
    let v = (v & !(0x3_u128 << 62)) | (2_u128 << 62);
    let hex = format!("{v:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoebis_store::{AccountKind, AccountRow, AccountState};

    fn key(session: &str) -> MemoryKey {
        MemoryKey {
            key_id: "k".into(),
            session_id: session.into(),
            agent_id: "main".into(),
        }
    }

    fn row(incarnation: u64, revision: u64) -> AccountRow {
        AccountRow {
            id: "acct".into(),
            kind: AccountKind::Oauth,
            label: "acct".into(),
            enabled: true,
            identity: None,
            lifecycle_generation: 1,
            incarnation,
            revision,
            state: AccountState::Ready,
        }
    }

    #[test]
    fn the_cap_evicts_the_least_recently_seen_entry() {
        let mem = SessionMemory::new(2);
        let (a, b, c) = (key("a"), key("b"), key("c"));
        let id_a = mem.prompt_id(&a, None, false, None, 1_000);
        mem.prompt_id(&b, None, false, None, 2_000);
        // A third entry past the cap evicts `a`, the least recently seen of the two held.
        mem.prompt_id(&c, None, false, None, 3_000);
        let id_a_again = mem.prompt_id(&a, None, false, None, 4_000);
        assert_ne!(
            id_a, id_a_again,
            "the evicted turn was forgotten, so a fresh id was minted"
        );
    }

    #[test]
    fn the_purge_drops_an_entry_idle_past_the_bound() {
        let mem = SessionMemory::new(10);
        let a = key("a");
        let id = mem.prompt_id(&a, None, false, None, 0);
        let id_after = mem.prompt_id(&a, None, false, None, MEMORY_IDLE_MS);
        assert_ne!(
            id, id_after,
            "an entry idle the whole bound is purged before this call sees it"
        );
    }

    #[test]
    fn a_latch_answers_refuses_only_for_the_row_it_was_set_with() {
        let mem = SessionMemory::new(10);
        let account = row(1, 1);
        mem.latch(&account, Injected::Cch);
        assert!(mem.refuses(&account, Injected::Cch));
        assert!(
            !mem.refuses(&account, Injected::PromptId),
            "a different item was never latched"
        );
        assert!(
            !mem.refuses(&row(1, 2), Injected::Cch),
            "a later revision clears the latch"
        );
        assert!(
            !mem.refuses(&row(2, 1), Injected::Cch),
            "a different incarnation clears the latch"
        );
    }

    #[test]
    fn previous_is_isolated_per_account() {
        let mem = SessionMemory::new(10);
        let a = key("s");
        mem.record_request_id(&a, "acct-a", "req_a", 1_000);
        mem.record_request_id(&a, "acct-b", "req_b", 1_000);
        assert_eq!(
            mem.previous(&a, "acct-a", "claude-sonnet-4-5")
                .request_id
                .as_deref(),
            Some("req_a")
        );
        assert_eq!(
            mem.previous(&a, "acct-b", "claude-sonnet-4-5")
                .request_id
                .as_deref(),
            Some("req_b")
        );
        assert_eq!(
            mem.previous(&a, "acct-c", "claude-sonnet-4-5").request_id,
            None
        );
    }
}
