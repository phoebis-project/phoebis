//! The session memory behind `cc_prev_req` and the prompt id: what the relay last saw for a key, session, and
//! agent, held in process as the truth, written behind to the store's tier-2 table and read back from it on a
//! miss, and the per-account latch that remembers which injected field an upstream 400 has already refused.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use phoebis_store::AccountRow;
pub use phoebis_store::MemoryKey;
use phoebis_store::{MemoryRecord, MemoryStore, StoreError};

/// The default idle bound, also the client's own session expiry default; `sessions.memory_idle_seconds`
/// replaces it.
pub const MEMORY_IDLE_MS: i64 = 24 * 3_600_000;
/// Entries held at most; past it the least recently seen entry goes. The unflushed overlay a store that stays
/// down leaves behind is bounded by the same cap, dropping its own oldest rather than growing without limit,
/// so the relay holds at most twice this many records while the store is failing. Every string an entry holds
/// is bounded on the way in (the key's ids and the model by `identity`, the carried prompt id by `facts`, the
/// upstream's own ids by the response limits), so an entry is a few hundred bytes and the cap bounds memory at
/// a few tens of megabytes on a host that never restarts.
pub const MEMORY_CAP: usize = 65_536;

/// A minute between sweeps is often enough to keep the map from growing unbounded between requests, and rare
/// enough that a purge never costs a hot request its own lock hold.
const PURGE_INTERVAL_MS: i64 = 60_000;

/// The shape a tier-2 record is written in; a reader takes an older version by its number and a newer one as
/// a miss, so a build never reads a shape it does not know.
pub const RECORD_VERSION: u32 = 1;

/// Version 1 of the record: the entry as the map holds it, minus `last_seen`, which is the row's own column.
#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
struct RecordV1 {
    #[serde(default)]
    accounts: HashMap<String, AccountRecordV1>,
    #[serde(default)]
    prompt_id: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
struct AccountRecordV1 {
    #[serde(default)]
    message_ids: HashMap<String, String>,
    #[serde(default)]
    request_id: Option<String>,
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

impl Entry {
    fn record(&self, now: i64) -> MemoryRecord {
        let v1 = RecordV1 {
            accounts: self
                .accounts
                .iter()
                .map(|(id, a)| {
                    (
                        id.clone(),
                        AccountRecordV1 {
                            message_ids: a.message_ids.clone(),
                            request_id: a.request_id.clone(),
                        },
                    )
                })
                .collect(),
            prompt_id: self.prompt_id.clone(),
        };
        MemoryRecord {
            record_version: RECORD_VERSION,
            payload: serde_json::to_string(&v1).expect("the record serializes"),
            last_seen: now,
        }
    }

    /// `None` for a version this build does not read: the entry starts fresh rather than half-read.
    fn from_record(record: &MemoryRecord, now: i64) -> Option<Entry> {
        if record.record_version != RECORD_VERSION {
            return None;
        }
        let v1: RecordV1 = serde_json::from_str(&record.payload).ok()?;
        Some(Entry {
            last_seen: now,
            prompt_id: v1.prompt_id,
            accounts: v1
                .accounts
                .into_iter()
                .map(|(id, a)| {
                    (
                        id,
                        AccountMemory {
                            request_id: a.request_id,
                            message_ids: a.message_ids,
                        },
                    )
                })
                .collect(),
        })
    }
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
    /// Bumped by every write; a dirty or a pending entry is filed under the sequence of its latest write, so a
    /// flush that lands after a newer write still knows which write it saved.
    sequence: u64,
    /// The keys changed since their last acknowledged flush, each under the sequence of its latest write.
    dirty: HashMap<MemoryKey, u64>,
    /// A keyed overlay for the entries that left the map with a change unflushed: an eviction or an idle purge
    /// moves the record here rather than losing it, and a write or a read that lands on the same key revives it
    /// before falling back to the map or the disk. Bounded by `cap`, the same as `entries`.
    pending: HashMap<MemoryKey, (MemoryRecord, u64)>,
    /// Records the overlay has shed because it was already at `cap` when another arrived, since the last time
    /// a flush reported them. Real loss, not double counting: an entry the overlay drops is one `acknowledge`
    /// will never see.
    dropped_pending: u64,
}

pub struct SessionMemory {
    inner: Mutex<Inner>,
    cap: usize,
    idle_ms: i64,
    store: Option<Arc<dyn MemoryStore>>,
    retention_ms: i64,
    /// Serializes a snapshot, its save, and its acknowledgement across the periodic task and the shutdown flush,
    /// so two flushes never race and an older snapshot never lands after a newer one.
    flush_lock: std::sync::Mutex<()>,
}

impl SessionMemory {
    pub fn new(cap: usize, idle_ms: i64) -> SessionMemory {
        SessionMemory {
            inner: Mutex::new(Inner {
                entries: HashMap::new(),
                latches: HashMap::new(),
                last_purge: 0,
                sequence: 0,
                dirty: HashMap::new(),
                pending: HashMap::new(),
                dropped_pending: 0,
            }),
            cap,
            idle_ms,
            store: None,
            retention_ms: i64::MAX,
            flush_lock: std::sync::Mutex::new(()),
        }
    }

    /// Attaches tier 2: a store to read a miss from and write dirty entries behind to, and how long a row is
    /// kept past its last request.
    pub fn with_store(mut self, store: Arc<dyn MemoryStore>, retention_ms: i64) -> SessionMemory {
        self.store = Some(store);
        self.retention_ms = retention_ms;
        self
    }

    pub fn cap(&self) -> usize {
        self.cap
    }

    pub fn idle_ms(&self) -> i64 {
        self.idle_ms
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
        // A revived entry is not new, and keeps the prompt id its record carried; checked ahead of `is_new` so
        // a write landing on a key evicted moments earlier continues that record rather than racing it to the
        // store with a fresh one.
        if !inner.entries.contains_key(key) {
            revive_pending(&mut inner, key, now, self.cap);
        }
        let is_new = !inner.entries.contains_key(key);
        // Resolved before the entry is touched: a parent lookup and the child's own insertion must not borrow
        // the map at once.
        let inherited = is_new
            .then(|| {
                parent_agent.and_then(|agent| {
                    let parent_key = MemoryKey {
                        key_id: key.key_id.clone(),
                        key_generation: key.key_generation,
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
            evict_if_full(&mut inner, self.cap);
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
        let id = entry.prompt_id.clone().expect("just set above");
        self.mark_dirty(&mut inner, key);
        id
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
        self.mark_dirty(&mut inner, key);
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
        self.mark_dirty(&mut inner, key);
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

    /// Drops entries idle past the configured bound; run by every write, at most once a minute.
    pub fn purge(&self, now: i64) {
        let mut inner = self.inner.lock().unwrap();
        purge_locked(&mut inner, now, self.idle_ms, self.cap);
    }

    /// Nothing without a store: a memory with no tier 2 accumulates no overlay to flush.
    fn mark_dirty(&self, inner: &mut Inner, key: &MemoryKey) {
        if self.store.is_none() {
            return;
        }
        inner.sequence += 1;
        inner.dirty.insert(key.clone(), inner.sequence);
    }

    /// The one store read of a request, run by the relay before it reads or writes the key: a tier-1 miss
    /// after a restart or an eviction is filled from the overlay, else from tier 2, before the caller decides
    /// the entry is new. The purge runs first, so an entry the idle bound is about to drop is re-read here
    /// rather than found, dropped by the next writer's purge, and replaced by a fresh one. The store's read
    /// runs on the blocking pool with the map unlocked, so a slow disk stalls this request alone; an entry
    /// that arrived meanwhile wins over the row. A row last seen before the retention is a miss and is left
    /// for the hourly purge.
    pub async fn restore(&self, key: &MemoryKey, now: i64) {
        let Some(store) = &self.store else { return };
        self.purge(now);
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.entries.contains_key(key) || revive_pending(&mut inner, key, now, self.cap) {
                return;
            }
        }
        let store = store.clone();
        let wanted = key.clone();
        let record = match tokio::task::spawn_blocking(move || store.load(&wanted)).await {
            Ok(Ok(Some(record))) => record,
            Ok(Ok(None)) => return,
            Ok(Err(e)) => {
                tracing::warn!(%e, "the session memory's store could not be read");
                return;
            }
            Err(e) => {
                tracing::warn!(%e, "the session memory's read did not finish");
                return;
            }
        };
        if record.last_seen < now.saturating_sub(self.retention_ms) {
            return;
        }
        let Some(entry) = Entry::from_record(&record, now) else {
            return;
        };
        let mut inner = self.inner.lock().unwrap();
        if !inner.entries.contains_key(key) && !inner.pending.contains_key(key) {
            evict_if_full(&mut inner, self.cap);
            inner.entries.insert(key.clone(), entry);
        }
    }

    /// What a flush writes: every overlay record under the sequence it left the map at, and every dirty entry
    /// as a record under the sequence of its latest write. Nothing is removed here; `acknowledge` does that
    /// once the save has landed.
    pub fn snapshot_dirty(&self, now: i64) -> Vec<(MemoryKey, MemoryRecord, u64)> {
        let inner = self.inner.lock().unwrap();
        let mut out: Vec<(MemoryKey, MemoryRecord, u64)> = inner
            .pending
            .iter()
            .map(|(key, (record, sequence))| (key.clone(), record.clone(), *sequence))
            .collect();
        for (key, sequence) in &inner.dirty {
            if let Some(entry) = inner.entries.get(key) {
                out.push((key.clone(), entry.record(now), *sequence));
            }
        }
        out
    }

    /// Retires what a successful save carried: a key leaves `dirty` only while its sequence is the saved one
    /// (a write since the snapshot keeps it dirty), and leaves the overlay only while the record there is the
    /// saved one (a record `restore` moved back is no longer pending).
    pub fn acknowledge(&self, saved: &[(MemoryKey, u64)]) {
        let mut inner = self.inner.lock().unwrap();
        for (key, sequence) in saved {
            if inner.dirty.get(key) == Some(sequence) {
                inner.dirty.remove(key);
            }
            if inner.pending.get(key).is_some_and(|(_, s)| s == sequence) {
                inner.pending.remove(key);
            }
        }
    }

    /// A snapshot, its save, and the acknowledgement, on the blocking pool under one lock: two flushes at
    /// once could otherwise persist an older snapshot after a newer one, and a flush whose caller is aborted
    /// still runs to its end, so the shutdown's flush queues behind it rather than racing it. A failed save
    /// logs and leaves everything for the next flush.
    pub async fn flush(self: &Arc<Self>, now: i64) {
        let Some(store) = self.store.clone() else {
            return;
        };
        let memory = self.clone();
        let flushed = tokio::task::spawn_blocking(move || -> Result<(), StoreError> {
            let _flushing = memory.flush_lock.lock().unwrap();
            let batch = memory.snapshot_dirty(now);
            if batch.is_empty() {
                return Ok(());
            }
            let records: Vec<(MemoryKey, MemoryRecord)> = batch
                .iter()
                .map(|(key, record, _)| (key.clone(), record.clone()))
                .collect();
            store.save(&records)?;
            let saved: Vec<(MemoryKey, u64)> = batch
                .into_iter()
                .map(|(key, _, sequence)| (key, sequence))
                .collect();
            memory.acknowledge(&saved);
            Ok(())
        })
        .await;
        match flushed {
            Ok(Ok(())) => {
                // Reported once, on the recovery that follows a streak: an outage that never drops anything
                // stays silent here, and one that does says so exactly once rather than on every later flush.
                let dropped = std::mem::take(&mut self.inner.lock().unwrap().dropped_pending);
                if dropped > 0 {
                    tracing::warn!(
                        dropped,
                        "the session memory's overlay dropped records while the store was unreachable"
                    );
                }
            }
            Ok(Err(e)) => {
                let dropped = self.inner.lock().unwrap().dropped_pending;
                tracing::warn!(
                    %e,
                    dropped,
                    "the session memory could not be flushed; its changes wait for the next flush"
                );
            }
            Err(e) => tracing::warn!(%e, "the session memory's flush did not finish"),
        }
    }

    /// Drops the store's rows last seen before the retention, on the blocking pool.
    pub async fn purge_store(self: &Arc<Self>, now: i64) {
        let Some(store) = self.store.clone() else {
            return;
        };
        let mark = now.saturating_sub(self.retention_ms);
        match tokio::task::spawn_blocking(move || store.purge(mark)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::warn!(%e, "the session memory's rows could not be purged"),
            Err(e) => tracing::warn!(%e, "the session memory's purge did not finish"),
        }
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
    if !inner.entries.contains_key(key) && !revive_pending(inner, key, now, cap) {
        evict_if_full(inner, cap);
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

/// An overlay record for `key` back into the map, dirty under a fresh sequence so the next flush writes it
/// with its new `last_seen`; answers whether there was one.
fn revive_pending(inner: &mut Inner, key: &MemoryKey, now: i64, cap: usize) -> bool {
    let Some((record, _)) = inner.pending.remove(key) else {
        return false;
    };
    let Some(entry) = Entry::from_record(&record, now) else {
        return false;
    };
    evict_if_full(inner, cap);
    inner.entries.insert(key.clone(), entry);
    inner.sequence += 1;
    inner.dirty.insert(key.clone(), inner.sequence);
    true
}

/// Past the cap the least recently seen entry leaves the map; one with an unflushed change goes to the
/// overlay under its sequence rather than being lost.
fn evict_if_full(inner: &mut Inner, cap: usize) {
    if inner.entries.len() < cap {
        return;
    }
    let Some(oldest) = inner
        .entries
        .iter()
        .min_by_key(|(_, e)| e.last_seen)
        .map(|(k, _)| k.clone())
    else {
        return;
    };
    let entry = inner
        .entries
        .remove(&oldest)
        .expect("just found under the lock");
    if let Some(sequence) = inner.dirty.remove(&oldest) {
        let record = entry.record(entry.last_seen);
        stash_pending(inner, cap, oldest, record, sequence);
    }
}

fn purge_locked(inner: &mut Inner, now: i64, idle_ms: i64, cap: usize) {
    if now - inner.last_purge < PURGE_INTERVAL_MS {
        return;
    }
    inner.last_purge = now;
    let idle: Vec<MemoryKey> = inner
        .entries
        .iter()
        .filter(|(_, e)| now - e.last_seen >= idle_ms)
        .map(|(k, _)| k.clone())
        .collect();
    for key in idle {
        let entry = inner
            .entries
            .remove(&key)
            .expect("just found under the lock");
        if let Some(sequence) = inner.dirty.remove(&key) {
            let record = entry.record(entry.last_seen);
            stash_pending(inner, cap, key, record, sequence);
        }
    }
}

/// Inserts an overlay record, holding `pending` at `cap`: when it is already full, whichever record has the
/// smallest `last_seen` goes, the new arrival included, since that is the one the idle purge would have shed
/// first anyway. Counted so a flush that keeps failing can eventually say how much it lost.
fn stash_pending(
    inner: &mut Inner,
    cap: usize,
    key: MemoryKey,
    record: MemoryRecord,
    sequence: u64,
) {
    if inner.pending.len() >= cap {
        let oldest = inner
            .pending
            .iter()
            .min_by_key(|(_, (r, _))| r.last_seen)
            .map(|(k, (r, _))| (k.clone(), r.last_seen));
        match oldest {
            Some((oldest_key, oldest_seen)) if oldest_seen <= record.last_seen => {
                inner.pending.remove(&oldest_key);
                inner.dropped_pending += 1;
            }
            _ => {
                inner.dropped_pending += 1;
                return;
            }
        }
    }
    inner.pending.insert(key, (record, sequence));
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::time::Duration;

    use phoebis_store::{AccountKind, AccountRow, AccountState};

    /// A store over a map, with the seams the flush and read tests need: a save that fails once, and a load
    /// or a save that waits on a barrier, each taken once.
    #[derive(Default)]
    struct MapStore {
        rows: Mutex<HashMap<MemoryKey, MemoryRecord>>,
        fail_next_save: AtomicBool,
        hold_load: Mutex<Option<Arc<Barrier>>>,
        hold_save: Mutex<Option<Arc<Barrier>>>,
        saves_started: AtomicUsize,
    }

    impl MemoryStore for MapStore {
        fn load(&self, key: &MemoryKey) -> Result<Option<MemoryRecord>, StoreError> {
            let held = self.hold_load.lock().unwrap().take();
            if let Some(barrier) = held {
                barrier.wait();
            }
            Ok(self.rows.lock().unwrap().get(key).cloned())
        }

        fn save(&self, records: &[(MemoryKey, MemoryRecord)]) -> Result<(), StoreError> {
            self.saves_started.fetch_add(1, Ordering::SeqCst);
            if self.fail_next_save.swap(false, Ordering::SeqCst) {
                return Err(StoreError::Locked);
            }
            let held = self.hold_save.lock().unwrap().take();
            if let Some(barrier) = held {
                barrier.wait();
            }
            let mut rows = self.rows.lock().unwrap();
            for (key, record) in records {
                rows.insert(key.clone(), record.clone());
            }
            Ok(())
        }

        fn purge(&self, older_than: i64) -> Result<usize, StoreError> {
            let mut rows = self.rows.lock().unwrap();
            let before = rows.len();
            rows.retain(|_, r| r.last_seen >= older_than);
            Ok(before - rows.len())
        }
    }

    /// A version-1 record over one account, the shape `record_request_id` and `prompt_id` would have written.
    fn seeded(request_id: &str, prompt_id: Option<&str>, last_seen: i64) -> MemoryRecord {
        let v1 = RecordV1 {
            accounts: HashMap::from([(
                "acct".to_string(),
                AccountRecordV1 {
                    message_ids: HashMap::new(),
                    request_id: Some(request_id.to_string()),
                },
            )]),
            prompt_id: prompt_id.map(String::from),
        };
        MemoryRecord {
            record_version: RECORD_VERSION,
            payload: serde_json::to_string(&v1).expect("the record serializes"),
            last_seen,
        }
    }

    fn request_id_of(store: &MapStore, key: &MemoryKey) -> Option<String> {
        let rows = store.rows.lock().unwrap();
        let record = rows.get(key)?;
        let v1: RecordV1 = serde_json::from_str(&record.payload).ok()?;
        v1.accounts.get("acct").and_then(|a| a.request_id.clone())
    }

    fn key(session: &str) -> MemoryKey {
        MemoryKey {
            key_id: "k".into(),
            key_generation: 1,
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
        let mem = SessionMemory::new(2, MEMORY_IDLE_MS);
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
        let mem = SessionMemory::new(10, MEMORY_IDLE_MS);
        let a = key("a");
        let id = mem.prompt_id(&a, None, false, None, 0);
        // The second call must clear both gates: the idle bound itself, and PURGE_INTERVAL_MS since the
        // sweep only runs once a minute, so it lands at their sum rather than at the idle bound alone.
        let id_after = mem.prompt_id(&a, None, false, None, MEMORY_IDLE_MS + PURGE_INTERVAL_MS);
        assert_ne!(
            id, id_after,
            "an entry idle the whole bound is purged once the once-a-minute sweep next runs"
        );

        // An idle bound far shorter than PURGE_INTERVAL_MS still waits on the sweep: the entry is stale
        // well before the sweep runs, but the sweep, not the bound, decides when it is dropped.
        let custom = SessionMemory::new(10, 1_000);
        let id = custom.prompt_id(&a, None, false, None, 0);
        let id_after = custom.prompt_id(&a, None, false, None, PURGE_INTERVAL_MS + 1_000);
        assert_ne!(
            id, id_after,
            "a 1-second bound is enforced only once the 60-second sweep gate opens, at 60_000 + 1_000"
        );
    }

    #[test]
    fn a_latch_answers_refuses_only_for_the_row_it_was_set_with() {
        let mem = SessionMemory::new(10, MEMORY_IDLE_MS);
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
        let mem = SessionMemory::new(10, MEMORY_IDLE_MS);
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

    #[tokio::test]
    async fn a_write_marks_the_entry_dirty_and_a_flush_takes_it_once() {
        let store = Arc::new(MapStore::default());
        let mem =
            Arc::new(SessionMemory::new(10, MEMORY_IDLE_MS).with_store(store.clone(), i64::MAX));
        let a = key("a");
        mem.record_request_id(&a, "acct", "req", 1_000);
        let batch = mem.snapshot_dirty(1_000);
        assert_eq!(batch.len(), 1);
        let v1: RecordV1 = serde_json::from_str(&batch[0].1.payload).unwrap();
        assert_eq!(
            v1.accounts.get("acct").and_then(|a| a.request_id.clone()),
            Some("req".to_string())
        );
        mem.flush(1_000).await;
        assert_eq!(request_id_of(&store, &a).as_deref(), Some("req"));
        assert!(mem.snapshot_dirty(1_000).is_empty());
    }

    #[tokio::test]
    async fn an_evicted_entry_with_an_unflushed_change_is_pending_for_the_next_flush() {
        let store = Arc::new(MapStore::default());
        let mem =
            Arc::new(SessionMemory::new(1, MEMORY_IDLE_MS).with_store(store.clone(), i64::MAX));
        let (a, b) = (key("a"), key("b"));
        mem.record_request_id(&a, "acct", "req_a", 1_000);
        mem.prompt_id(&b, None, false, None, 2_000);
        let keys: std::collections::HashSet<MemoryKey> = mem
            .snapshot_dirty(2_000)
            .into_iter()
            .map(|(k, _, _)| k)
            .collect();
        assert_eq!(keys, std::collections::HashSet::from([a, b]));
    }

    #[test]
    fn the_overlay_is_bounded_by_the_cap_and_drops_its_oldest_record_when_full() {
        let store = Arc::new(MapStore::default());
        let mem = SessionMemory::new(2, MEMORY_IDLE_MS).with_store(store.clone(), i64::MAX);
        let (a, b, c, d, e) = (key("a"), key("b"), key("c"), key("d"), key("e"));
        // Never flushed: a store that stays down leaves exactly this state behind, one eviction at a time.
        mem.record_request_id(&a, "acct", "req_a", 1_000);
        mem.record_request_id(&b, "acct", "req_b", 2_000);
        mem.record_request_id(&c, "acct", "req_c", 3_000); // evicts a into the overlay
        mem.record_request_id(&d, "acct", "req_d", 4_000); // evicts b into the overlay
        mem.record_request_id(&e, "acct", "req_e", 5_000); // evicts c; the overlay is full, a is dropped
        let inner = mem.inner.lock().unwrap();
        assert_eq!(
            inner.pending.len(),
            2,
            "the overlay never grows past the cap"
        );
        assert!(
            !inner.pending.contains_key(&a),
            "the oldest last_seen left first"
        );
        assert!(inner.pending.contains_key(&b));
        assert!(inner.pending.contains_key(&c));
        assert_eq!(inner.dropped_pending, 1);
    }

    #[tokio::test]
    async fn an_evicted_entry_is_read_from_the_overlay_before_the_disk() {
        let store = Arc::new(MapStore::default());
        let a = key("a");
        store
            .rows
            .lock()
            .unwrap()
            .insert(a.clone(), seeded("req_old", None, 500));
        let mem =
            Arc::new(SessionMemory::new(1, MEMORY_IDLE_MS).with_store(store.clone(), i64::MAX));
        let b = key("b");
        mem.record_request_id(&a, "acct", "req_new", 1_000);
        mem.prompt_id(&b, None, false, None, 2_000);
        mem.restore(&a, 3_000).await;
        assert_eq!(
            mem.previous(&a, "acct", "claude-sonnet-4-5")
                .request_id
                .as_deref(),
            Some("req_new"),
            "the overlay answers before the disk is ever read"
        );
        mem.flush(3_000).await;
        assert_eq!(request_id_of(&store, &a).as_deref(), Some("req_new"));
    }

    #[tokio::test]
    async fn a_failed_save_keeps_the_batch_for_the_next_flush() {
        let store = Arc::new(MapStore::default());
        store.fail_next_save.store(true, Ordering::SeqCst);
        let mem =
            Arc::new(SessionMemory::new(1, MEMORY_IDLE_MS).with_store(store.clone(), i64::MAX));
        let (a, b) = (key("a"), key("b"));
        mem.record_request_id(&a, "acct", "req_a", 1_000);
        mem.prompt_id(&b, None, false, None, 2_000);
        mem.flush(2_000).await;
        assert!(
            store.rows.lock().unwrap().is_empty(),
            "the failed save wrote nothing"
        );
        assert_eq!(
            mem.snapshot_dirty(2_000).len(),
            2,
            "the batch waits for the next flush"
        );
        mem.flush(2_000).await;
        assert_eq!(store.rows.lock().unwrap().len(), 2);
        assert!(mem.snapshot_dirty(2_000).is_empty());
    }

    #[tokio::test]
    async fn a_write_after_the_snapshot_keeps_the_key_dirty_past_the_acknowledgement() {
        let store = Arc::new(MapStore::default());
        let mem =
            Arc::new(SessionMemory::new(10, MEMORY_IDLE_MS).with_store(store.clone(), i64::MAX));
        let a = key("a");
        mem.record_request_id(&a, "acct", "req_1", 1_000);
        let batch = mem.snapshot_dirty(1_000);
        mem.record_request_id(&a, "acct", "req_2", 2_000);
        let saved: Vec<(MemoryKey, u64)> = batch.into_iter().map(|(k, _, s)| (k, s)).collect();
        mem.acknowledge(&saved);
        let after = mem.snapshot_dirty(2_000);
        assert_eq!(
            after.len(),
            1,
            "the write since the snapshot keeps the key dirty"
        );
        let v1: RecordV1 = serde_json::from_str(&after[0].1.payload).unwrap();
        assert_eq!(
            v1.accounts.get("acct").and_then(|a| a.request_id.clone()),
            Some("req_2".to_string())
        );
    }

    #[tokio::test]
    async fn a_miss_reads_the_store_and_an_unknown_version_starts_fresh() {
        let store = Arc::new(MapStore::default());
        let (a, b) = (key("a"), key("b"));
        store
            .rows
            .lock()
            .unwrap()
            .insert(a.clone(), seeded("req_old", None, 500));
        store.rows.lock().unwrap().insert(
            b.clone(),
            MemoryRecord {
                record_version: RECORD_VERSION + 1,
                payload: "{}".into(),
                last_seen: 500,
            },
        );
        let mem =
            Arc::new(SessionMemory::new(10, MEMORY_IDLE_MS).with_store(store.clone(), i64::MAX));
        mem.restore(&a, 1_000).await;
        mem.restore(&b, 1_000).await;
        assert_eq!(
            mem.previous(&a, "acct", "claude-sonnet-4-5")
                .request_id
                .as_deref(),
            Some("req_old")
        );
        assert_eq!(
            mem.previous(&b, "acct", "claude-sonnet-4-5").request_id,
            None,
            "a version this build does not know starts fresh rather than half-read"
        );
    }

    #[tokio::test]
    async fn a_restored_entry_keeps_its_prompt_id() {
        let store = Arc::new(MapStore::default());
        let a = key("a");
        store
            .rows
            .lock()
            .unwrap()
            .insert(a.clone(), seeded("req", Some("p-old"), 500));
        let mem =
            Arc::new(SessionMemory::new(10, MEMORY_IDLE_MS).with_store(store.clone(), i64::MAX));
        mem.restore(&a, 1_000).await;
        assert_eq!(mem.prompt_id(&a, None, false, None, 1_000), "p-old");
        let minted = mem.prompt_id(&a, None, true, None, 2_000);
        assert_ne!(
            minted, "p-old",
            "a fresh turn still mints over a restored id"
        );
    }

    #[tokio::test]
    async fn a_record_past_the_retention_is_a_miss_and_one_inside_it_is_restored() {
        let store = Arc::new(MapStore::default());
        let (a, b) = (key("a"), key("b"));
        store
            .rows
            .lock()
            .unwrap()
            .insert(a.clone(), seeded("req_a", None, 0));
        store
            .rows
            .lock()
            .unwrap()
            .insert(b.clone(), seeded("req_b", None, 5_000));
        let mem = Arc::new(SessionMemory::new(10, MEMORY_IDLE_MS).with_store(store.clone(), 1_000));
        mem.restore(&a, 5_500).await;
        mem.restore(&b, 5_500).await;
        assert_eq!(
            mem.previous(&a, "acct", "claude-sonnet-4-5").request_id,
            None,
            "a row past the retention is a miss on read-through"
        );
        assert_eq!(
            mem.previous(&b, "acct", "claude-sonnet-4-5")
                .request_id
                .as_deref(),
            Some("req_b")
        );
        assert!(
            store.rows.lock().unwrap().contains_key(&a),
            "the read leaves the row for the hourly purge rather than dropping it itself"
        );
    }

    #[tokio::test]
    async fn the_first_request_past_the_idle_bound_reads_the_persisted_entry() {
        let store = Arc::new(MapStore::default());
        let mem = Arc::new(SessionMemory::new(10, 1_000).with_store(store.clone(), i64::MAX));
        let a = key("a");
        mem.record_request_id(&a, "acct", "req_kept", 0);
        mem.flush(0).await;
        // Overwritten after the flush and read only through the store, not the in-process copy: the two
        // would otherwise agree on `req_kept` whether or not the idle purge ever ran.
        store
            .rows
            .lock()
            .unwrap()
            .insert(a.clone(), seeded("req_disk", None, 0));
        mem.restore(&a, 61_000).await;
        assert_eq!(
            mem.previous(&a, "acct", "claude-sonnet-4-5")
                .request_id
                .as_deref(),
            Some("req_disk"),
            "the idle purge drops the in-process entry, and the same read fills it back in from the store"
        );
    }

    #[tokio::test]
    async fn a_flush_in_flight_holds_the_next_one_so_the_newer_record_lands_last() {
        let store = Arc::new(MapStore::default());
        let mem =
            Arc::new(SessionMemory::new(10, MEMORY_IDLE_MS).with_store(store.clone(), i64::MAX));
        let a = key("a");
        mem.record_request_id(&a, "acct", "req_old", 1_000);
        let gate = Arc::new(Barrier::new(2));
        *store.hold_save.lock().unwrap() = Some(gate.clone());
        let first = tokio::spawn({
            let mem = mem.clone();
            async move { mem.flush(1_000).await }
        });
        fake_upstream::wait_for(Duration::from_secs(5), || {
            (store.saves_started.load(Ordering::SeqCst) == 1).then_some(())
        })
        .await;
        // The first save is at the barrier with the older record; the newer write and the second flush must
        // queue behind it rather than overtake it.
        mem.record_request_id(&a, "acct", "req_new", 2_000);
        let second = tokio::spawn({
            let mem = mem.clone();
            async move { mem.flush(2_000).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            store.saves_started.load(Ordering::SeqCst),
            1,
            "the second flush waits on the lock"
        );
        tokio::task::spawn_blocking(move || gate.wait())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            first.await.unwrap();
            second.await.unwrap();
        })
        .await
        .unwrap();
        assert_eq!(request_id_of(&store, &a).as_deref(), Some("req_new"));
        assert!(
            mem.snapshot_dirty(3_000).is_empty(),
            "both flushes were acknowledged"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_blocked_store_read_does_not_stall_the_runtime() {
        let store = Arc::new(MapStore::default());
        let mem =
            Arc::new(SessionMemory::new(10, MEMORY_IDLE_MS).with_store(store.clone(), i64::MAX));
        let (a, b) = (key("a"), key("b"));
        mem.record_request_id(&b, "acct", "req_b", 1_000);
        let gate = Arc::new(Barrier::new(2));
        *store.hold_load.lock().unwrap() = Some(gate.clone());
        let blocked = tokio::spawn({
            let mem = mem.clone();
            async move { mem.restore(&a, 1_000).await }
        });
        // The read for `a` is held in the store; the map is free, and the runtime's timer still fires.
        tokio::time::timeout(
            Duration::from_secs(5),
            tokio::time::sleep(Duration::from_millis(50)),
        )
        .await
        .unwrap();
        assert_eq!(
            mem.previous(&b, "acct", "claude-sonnet-4-5")
                .request_id
                .as_deref(),
            Some("req_b")
        );
        assert!(!blocked.is_finished(), "the read is still held");
        tokio::task::spawn_blocking(move || gate.wait())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), blocked)
            .await
            .unwrap()
            .unwrap();
    }
}
