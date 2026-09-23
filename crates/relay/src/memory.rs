//! The session memory behind `cc_prev_req` and the prompt id: what the relay last saw for a key, session, and
//! agent, held in process as the truth, written behind to the store's tier-2 table and read back from it on a
//! miss, and the per-account latch that remembers which injected field an upstream 400 has already refused.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use phoebis_store::AccountRow;
pub use phoebis_store::MemoryKey;
use phoebis_store::{MemoryRecord, MemoryStore, StoreError};

/// The default idle bound, also the client's own session expiry default; `sessions.memory_idle_seconds`
/// replaces it.
pub const MEMORY_IDLE_MS: i64 = 24 * 3_600_000;
/// Entries held at most; past it the least recently seen entry goes. The overlay an eviction leaves behind is
/// bounded by the same cap in every regime: while the store answers, it asks for a flush the moment it reaches
/// the cap, so a shed there happens only when that flush does not land first; while the store is failing, it
/// sheds its oldest at the cap. So the relay holds at most twice this many records in any regime. Every string
/// an entry holds is bounded on the way in (the key's ids and the model by `identity`, the carried prompt id by
/// `facts`, the upstream's own ids by the response limits), so an entry is a few hundred bytes and the cap
/// bounds memory at a few tens of megabytes on a host that never restarts.
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
    /// The entry's own `last_seen` is the record's, never a flush clock: a save that recovers after an outage
    /// must not stamp an expired entry fresh.
    fn record(&self) -> MemoryRecord {
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
            last_seen: self.last_seen,
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
    /// The keys whose store read is in flight. Neither the cap nor the idle bound may take such a key out of
    /// the map: the read started when the key was absent, and an eviction between its start and its guard is
    /// exactly what would let it install its older copy over a newer one that was written and saved meanwhile.
    loading: HashSet<MemoryKey>,
    /// Whether the last save failed. Gates the flush request alone: while true, an overlay at the cap is
    /// already shedding on its own and asking again would only spend a flush that has nothing new to save.
    store_failing: bool,
    /// Bumped by every write; a dirty or a pending entry is filed under the sequence of its latest write, so a
    /// flush that lands after a newer write still knows which write it saved.
    sequence: u64,
    /// The keys changed since their last acknowledged flush, each under the sequence of its latest write.
    dirty: HashMap<MemoryKey, u64>,
    /// A keyed overlay for the entries that left the map with a change unflushed: an eviction or an idle purge
    /// moves the record here rather than losing it, and a write or a read that lands on the same key revives it
    /// before falling back to the map or the disk. Bounded by `cap` in every regime; a shed is counted in
    /// `dropped_pending`.
    pending: HashMap<MemoryKey, (MemoryRecord, u64)>,
    /// Records the overlay shed because it was at `cap` when another arrived, whether the store was failing or
    /// a save had not landed yet, since the last time a flush reported them.
    dropped_pending: u64,
    /// Unsaved records retired because they fell past the retention before a save took them; reported once,
    /// at the next successful flush.
    expired_unsaved: u64,
}

pub struct SessionMemory {
    inner: Mutex<Inner>,
    cap: usize,
    /// Raised when the overlay reaches the cap with the store answering, so the flusher runs now rather than
    /// at its next tick and the overlay is emptied instead of shedding what it holds.
    flush_wanted: tokio::sync::Notify,
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
                loading: HashSet::new(),
                store_failing: false,
                sequence: 0,
                dirty: HashMap::new(),
                pending: HashMap::new(),
                dropped_pending: 0,
                expired_unsaved: 0,
            }),
            cap,
            flush_wanted: tokio::sync::Notify::new(),
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
            revive_pending(&mut inner, key, now, self.cap, self.retention_ms);
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
        let account = account_entry(
            &mut inner,
            key,
            account_id,
            now,
            self.cap,
            self.retention_ms,
        );
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
        let account = account_entry(
            &mut inner,
            key,
            account_id,
            now,
            self.cap,
            self.retention_ms,
        );
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
        self.ask_for_a_flush(&inner);
    }

    /// Waits until the overlay wants a flush ahead of the next tick. The flusher selects on this and on its
    /// interval, so an overlay at the cap is written out rather than left to shed records.
    pub async fn flush_wanted(&self) {
        self.flush_wanted.notified().await;
    }

    /// Nothing without a store: a memory with no tier 2 accumulates no overlay to flush.
    fn mark_dirty(&self, inner: &mut Inner, key: &MemoryKey) {
        if self.store.is_none() {
            return;
        }
        inner.sequence += 1;
        inner.dirty.insert(key.clone(), inner.sequence);
        self.ask_for_a_flush(inner);
    }

    /// Called under the lock after anything that may have stashed a record.
    fn ask_for_a_flush(&self, inner: &Inner) {
        if !inner.store_failing && inner.pending.len() >= self.cap {
            self.flush_wanted.notify_one();
        }
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
            if inner.entries.contains_key(key)
                || revive_pending(&mut inner, key, now, self.cap, self.retention_ms)
            {
                return;
            }
            // Marked for the whole read: nothing may evict or purge the key while its older copy is on its
            // way back, or a write that landed and was saved meanwhile would be overwritten by it below.
            inner.loading.insert(key.clone());
        }
        let loaded = self.load_entry(store.clone(), key, now).await;
        let mut inner = self.inner.lock().unwrap();
        if let Some(entry) = loaded {
            if !inner.entries.contains_key(key) && !inner.pending.contains_key(key) {
                evict_if_full(&mut inner, self.cap);
                inner.entries.insert(key.clone(), entry);
            }
        }
        inner.loading.remove(key);
        self.ask_for_a_flush(&inner);
    }

    /// The store's record for `key` as an entry: `None` for a miss, a row this build cannot read, a row past
    /// the retention (left for the hourly purge), or a read that failed.
    async fn load_entry(
        &self,
        store: Arc<dyn MemoryStore>,
        key: &MemoryKey,
        now: i64,
    ) -> Option<Entry> {
        let wanted = key.clone();
        let record = match tokio::task::spawn_blocking(move || store.load(&wanted)).await {
            Ok(Ok(Some(record))) => record,
            Ok(Ok(None)) => return None,
            Ok(Err(e)) => {
                tracing::warn!(%e, "the session memory's store could not be read");
                return None;
            }
            Err(e) => {
                tracing::warn!(%e, "the session memory's read did not finish");
                return None;
            }
        };
        if expired(record.last_seen, now, self.retention_ms) {
            return None;
        }
        Entry::from_record(&record, now)
    }

    /// What a flush writes: every overlay record under the sequence it left the map at, and every dirty entry
    /// as a record under the sequence of its latest write, each carrying the `last_seen` of the entry itself
    /// rather than the flush's clock, so saves that fail for longer than the retention and then recover write
    /// no expired entry back under a fresh timestamp. A record already past the retention is retired here
    /// rather than left out: it names a session the read-through would miss, and a record nothing will ever
    /// save has no business in the overlay. Nothing else is removed here; `acknowledge` does that once the
    /// save has landed.
    pub fn snapshot_dirty(&self, now: i64) -> Vec<(MemoryKey, MemoryRecord, u64)> {
        let mut inner = self.inner.lock().unwrap();
        let expired_pending: Vec<MemoryKey> = inner
            .pending
            .iter()
            .filter(|(_, (record, _))| expired(record.last_seen, now, self.retention_ms))
            .map(|(key, _)| key.clone())
            .collect();
        for key in expired_pending {
            inner.pending.remove(&key);
            inner.expired_unsaved += 1;
        }
        let expired_dirty: Vec<MemoryKey> = inner
            .dirty
            .keys()
            .filter(|key| {
                inner
                    .entries
                    .get(*key)
                    .is_some_and(|entry| expired(entry.last_seen, now, self.retention_ms))
            })
            .cloned()
            .collect();
        for key in expired_dirty {
            inner.dirty.remove(&key);
            inner.expired_unsaved += 1;
        }
        let mut out: Vec<(MemoryKey, MemoryRecord, u64)> = inner
            .pending
            .iter()
            .map(|(key, (record, sequence))| (key.clone(), record.clone(), *sequence))
            .collect();
        for (key, sequence) in &inner.dirty {
            if let Some(entry) = inner.entries.get(key) {
                out.push((key.clone(), entry.record(), *sequence));
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
            // A panic inside a flush leaves the lock poisoned; taking the guard anyway keeps every later
            // flush working, since the lock orders flushes and guards no invariant of its own.
            let _flushing = memory
                .flush_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let batch = memory.snapshot_dirty(now);
            if batch.is_empty() {
                // Nothing to save is not a failure: the regime flag would otherwise outlive the outage
                // that set it once every unsaved record has expired.
                memory.inner.lock().unwrap().store_failing = false;
                return Ok(());
            }
            let records: Vec<(MemoryKey, MemoryRecord)> = batch
                .iter()
                .map(|(key, record, _)| (key.clone(), record.clone()))
                .collect();
            // The overlay reads this to decide whether it may shed a record, so it is set from the save
            // itself rather than from the outcome below, which an empty batch also reaches.
            if let Err(e) = store.save(&records) {
                memory.inner.lock().unwrap().store_failing = true;
                return Err(e);
            }
            memory.inner.lock().unwrap().store_failing = false;
            let saved: Vec<(MemoryKey, u64)> = batch
                .into_iter()
                .map(|(key, _, sequence)| (key, sequence))
                .collect();
            memory.acknowledge(&saved);
            // Reported once, on the recovery that follows a streak: an outage that neither drops nor
            // expires anything stays silent here, and one that does says so exactly once rather than on
            // every later flush. Kept out of the empty-batch return above: a flush with nothing to save has
            // not recovered anything yet to report.
            let (dropped, expired) = {
                let mut inner = memory.inner.lock().unwrap();
                (
                    std::mem::take(&mut inner.dropped_pending),
                    std::mem::take(&mut inner.expired_unsaved),
                )
            };
            if dropped > 0 || expired > 0 {
                tracing::warn!(
                    dropped,
                    expired,
                    "the session memory's overlay lost records: shed at the cap, or expired before a save \
                     took them"
                );
            }
            Ok(())
        })
        .await;
        match flushed {
            Ok(Ok(())) => {}
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

/// Whether a row last seen at `last_seen` has fallen behind `now` by more than `retention_ms`: the one
/// retention comparison every read-through, snapshot, and revive path judges a record by.
fn expired(last_seen: i64, now: i64, retention_ms: i64) -> bool {
    last_seen < now.saturating_sub(retention_ms)
}

/// The entry for `(key, account_id)`, inserting either level fresh when absent.
fn account_entry<'a>(
    inner: &'a mut Inner,
    key: &MemoryKey,
    account_id: &str,
    now: i64,
    cap: usize,
    retention_ms: i64,
) -> &'a mut AccountMemory {
    if !inner.entries.contains_key(key) && !revive_pending(inner, key, now, cap, retention_ms) {
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
fn revive_pending(
    inner: &mut Inner,
    key: &MemoryKey,
    now: i64,
    cap: usize,
    retention_ms: i64,
) -> bool {
    let Some((record, _)) = inner.pending.get(key) else {
        return false;
    };
    // Judged as the read-through judges a row: an expired record revived here would be written back under
    // a fresh timestamp, which is what the dirty tier's filter exists to prevent.
    if expired(record.last_seen, now, retention_ms) {
        inner.pending.remove(key);
        inner.expired_unsaved += 1;
        return false;
    }
    // Decoded before the record leaves the overlay: a payload this build cannot read is an unflushed change
    // that still belongs on disk, and removing it first would drop it for good.
    let Some(entry) = Entry::from_record(record, now) else {
        return false;
    };
    inner.pending.remove(key);
    evict_if_full(inner, cap);
    inner.entries.insert(key.clone(), entry);
    inner.sequence += 1;
    inner.dirty.insert(key.clone(), inner.sequence);
    true
}

/// Past the cap the least recently seen entry leaves the map; one with an unflushed change goes to the
/// overlay under its sequence rather than being lost. A key whose store read is in flight is never the one
/// chosen, so the map may hold a few entries past the cap while those reads run.
fn evict_if_full(inner: &mut Inner, cap: usize) {
    if inner.entries.len() < cap {
        return;
    }
    let loading = &inner.loading;
    let Some(oldest) = inner
        .entries
        .iter()
        .filter(|(k, _)| !loading.contains(*k))
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
        let record = entry.record();
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
        .filter(|(k, e)| now - e.last_seen >= idle_ms && !inner.loading.contains(*k))
        .map(|(k, _)| k.clone())
        .collect();
    for key in idle {
        let entry = inner
            .entries
            .remove(&key)
            .expect("just found under the lock");
        if let Some(sequence) = inner.dirty.remove(&key) {
            let record = entry.record();
            stash_pending(inner, cap, key, record, sequence);
        }
    }
}

/// Inserts an overlay record. The overlay is held at `cap` in every regime: whichever record has the smallest
/// `last_seen` goes, the new arrival included, since that is the one the idle purge would have shed first, and
/// it is counted so a later flush can say how much was lost. While the store answers, the flusher is asked for
/// a flush the moment the overlay reaches the cap, so a shed there means a save that hung or failed.
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
        loads_started: AtomicUsize,
        saves_started: AtomicUsize,
    }

    impl MemoryStore for MapStore {
        fn load(&self, key: &MemoryKey) -> Result<Option<MemoryRecord>, StoreError> {
            self.loads_started.fetch_add(1, Ordering::SeqCst);
            // The row is read before the hold, so a held read answers with what was there when it started,
            // which is what a slow store does and what a test about a stale copy needs.
            let row = self.rows.lock().unwrap().get(key).cloned();
            let held = self.hold_load.lock().unwrap().take();
            if let Some(barrier) = held {
                barrier.wait();
            }
            Ok(row)
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

    #[tokio::test]
    async fn the_overlay_is_bounded_by_the_cap_and_drops_its_oldest_record_while_saves_fail() {
        let store = Arc::new(MapStore::default());
        let mem =
            Arc::new(SessionMemory::new(2, MEMORY_IDLE_MS).with_store(store.clone(), i64::MAX));
        let (a, b, c, d, e) = (key("a"), key("b"), key("c"), key("d"), key("e"));
        mem.record_request_id(&a, "acct", "req_a", 1_000);
        // The refused save is what puts the overlay into the regime that sheds records: nothing lands from
        // here on, which is the state a store that stays down leaves behind, one eviction at a time.
        store.fail_next_save.store(true, Ordering::SeqCst);
        mem.flush(1_000).await;
        mem.record_request_id(&b, "acct", "req_b", 2_000);
        mem.record_request_id(&c, "acct", "req_c", 3_000); // evicts a into the overlay
        mem.record_request_id(&d, "acct", "req_d", 4_000); // evicts b into the overlay
        mem.record_request_id(&e, "acct", "req_e", 5_000); // evicts c; the overlay is full, a is dropped
        let inner = mem.inner.lock().unwrap();
        assert_eq!(
            inner.pending.len(),
            2,
            "the overlay never grows past the cap while the store is failing"
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
    async fn expired_records_leave_the_overlay_and_the_dirty_set_at_the_flush_that_skips_them() {
        let store = Arc::new(MapStore::default());
        let mem = Arc::new(SessionMemory::new(1, MEMORY_IDLE_MS).with_store(store.clone(), 1_000));
        let (a, b) = (key("a"), key("b"));
        mem.record_request_id(&a, "acct", "req_a", 1_000);
        store.fail_next_save.store(true, Ordering::SeqCst);
        mem.flush(1_000).await;
        mem.record_request_id(&b, "acct", "req_b", 1_500); // evicts a into the overlay
                                                           // Past the retention for both: the flush that leaves them out also retires them.
        mem.flush(5_000).await;
        let inner = mem.inner.lock().unwrap();
        assert!(
            inner.pending.is_empty(),
            "the expired overlay record was retired"
        );
        assert!(
            inner.dirty.is_empty(),
            "the expired dirty entry was retired"
        );
        assert!(
            !inner.store_failing,
            "an empty batch after a refused save clears the flag"
        );
        assert_eq!(inner.expired_unsaved, 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stalled_save_bounds_the_overlay_at_the_cap() {
        let store = Arc::new(MapStore::default());
        let mem =
            Arc::new(SessionMemory::new(1, MEMORY_IDLE_MS).with_store(store.clone(), i64::MAX));
        let (a, b, c, d) = (key("a"), key("b"), key("c"), key("d"));
        mem.record_request_id(&a, "acct", "req_a", 1_000);
        let gate = Arc::new(Barrier::new(2));
        *store.hold_save.lock().unwrap() = Some(gate.clone());
        let held = tokio::spawn({
            let mem = mem.clone();
            async move { mem.flush(1_000).await }
        });
        fake_upstream::wait_for(Duration::from_secs(5), || {
            (store.saves_started.load(Ordering::SeqCst) == 1).then_some(())
        })
        .await;
        // The save is parked with the store answering nothing; evictions keep arriving.
        mem.record_request_id(&b, "acct", "req_b", 2_000); // evicts a
        mem.record_request_id(&c, "acct", "req_c", 3_000); // evicts b; the overlay is at the cap, a is shed
        mem.record_request_id(&d, "acct", "req_d", 4_000); // evicts c; b is shed
                                                           // Captured before the barrier releases: the parked save thread waits on `gate` for a second party
                                                           // that must arrive whether or not the assertion below holds, or the runtime's drop hangs forever
                                                           // joining a blocking thread nothing would release.
        let (pending_len, holds_c, dropped) = {
            let inner = mem.inner.lock().unwrap();
            (
                inner.pending.len(),
                inner.pending.contains_key(&c),
                inner.dropped_pending,
            )
        };
        tokio::task::spawn_blocking(move || gate.wait())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), held)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            pending_len, 1,
            "the overlay never grows past the cap while a save hangs"
        );
        assert!(holds_c);
        assert_eq!(dropped, 2);
    }

    #[tokio::test]
    async fn an_overlay_record_past_the_retention_is_not_revived() {
        let store = Arc::new(MapStore::default());
        let mem = Arc::new(SessionMemory::new(10, MEMORY_IDLE_MS).with_store(store.clone(), 1_000));
        let a = key("a");
        mem.inner
            .lock()
            .unwrap()
            .pending
            .insert(a.clone(), (seeded("req_old", Some("p-old"), 0), 1));
        mem.restore(&a, 5_000).await;
        assert_eq!(
            mem.previous(&a, "acct", "claude-sonnet-4-5").request_id,
            None
        );
        let inner = mem.inner.lock().unwrap();
        assert!(
            !inner.pending.contains_key(&a),
            "the expired record left the overlay"
        );
        assert_eq!(inner.expired_unsaved, 1);
    }

    #[tokio::test]
    async fn a_healthy_store_keeps_every_evicted_record_and_the_overlay_asks_for_its_flush() {
        let store = Arc::new(MapStore::default());
        let mem =
            Arc::new(SessionMemory::new(2, MEMORY_IDLE_MS).with_store(store.clone(), i64::MAX));
        let (a, b, c, d) = (key("a"), key("b"), key("c"), key("d"));
        // Four sessions at a cap of two: c evicts a, d evicts b, and the overlay holds what the store has not
        // taken yet, reaching the cap and asking for its own flush, rather than shedding either record.
        mem.record_request_id(&a, "acct", "req_a", 1_000);
        mem.record_request_id(&b, "acct", "req_b", 2_000);
        mem.record_request_id(&c, "acct", "req_c", 3_000); // evicts a into the overlay
        mem.record_request_id(&d, "acct", "req_d", 4_000); // evicts b into the overlay; the overlay is at the cap
        tokio::time::timeout(Duration::from_secs(5), mem.flush_wanted())
            .await
            .expect("an overlay at the cap asks the flusher for a flush of its own");
        mem.flush(4_000).await;
        assert_eq!(request_id_of(&store, &a).as_deref(), Some("req_a"));
        assert_eq!(request_id_of(&store, &b).as_deref(), Some("req_b"));
        assert_eq!(request_id_of(&store, &c).as_deref(), Some("req_c"));
        assert_eq!(request_id_of(&store, &d).as_deref(), Some("req_d"));
        assert_eq!(mem.inner.lock().unwrap().dropped_pending, 0);
    }

    #[tokio::test]
    async fn a_dirty_entry_past_the_retention_is_not_written_back_when_the_saves_recover() {
        let store = Arc::new(MapStore::default());
        // A retention shorter than the outage, so the entry falls past it while the saves are failing.
        let mem = Arc::new(SessionMemory::new(10, MEMORY_IDLE_MS).with_store(store.clone(), 1_000));
        let a = key("a");
        mem.record_request_id(&a, "acct", "req_a", 1_000);
        store.fail_next_save.store(true, Ordering::SeqCst);
        mem.flush(1_000).await;
        assert!(
            store.rows.lock().unwrap().is_empty(),
            "the refused save wrote nothing"
        );
        mem.flush(5_000).await;
        assert!(
            store.rows.lock().unwrap().is_empty(),
            "the recovered save leaves out a record whose own last_seen is past the retention"
        );
        let next =
            Arc::new(SessionMemory::new(10, MEMORY_IDLE_MS).with_store(store.clone(), 1_000));
        next.restore(&a, 5_000).await;
        assert_eq!(
            next.previous(&a, "acct", "claude-sonnet-4-5").request_id,
            None,
            "and a read-through after a restart finds nothing for it"
        );
    }

    #[tokio::test]
    async fn a_response_side_write_reads_the_saved_record_before_it_lands() {
        let store = Arc::new(MapStore::default());
        let mem =
            Arc::new(SessionMemory::new(1, MEMORY_IDLE_MS).with_store(store.clone(), i64::MAX));
        let (a, b) = (key("a"), key("b"));
        let prompt = mem.prompt_id(&a, None, true, None, 1_000);
        mem.record_request_id(&a, "acct", "req_a", 1_000);
        // The one slot goes to another session while a's request is still upstream, and the flush saves what
        // the eviction left in the overlay.
        mem.prompt_id(&b, None, true, None, 2_000);
        mem.flush(2_000).await;
        // The order the response half writes in: the read-through first, the write after it.
        mem.restore(&a, 3_000).await;
        mem.record_request_id(&a, "other", "req_other", 3_000);
        mem.flush(3_000).await;
        let rows = store.rows.lock().unwrap();
        let v1: RecordV1 = serde_json::from_str(&rows.get(&a).unwrap().payload).unwrap();
        assert_eq!(
            v1.prompt_id.as_deref(),
            Some(prompt.as_str()),
            "the saved turn's prompt id survived the response-side write"
        );
        assert_eq!(
            v1.accounts.get("acct").and_then(|a| a.request_id.clone()),
            Some("req_a".to_string())
        );
        assert_eq!(
            v1.accounts.get("other").and_then(|a| a.request_id.clone()),
            Some("req_other".to_string()),
            "and the write itself landed beside it"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_held_read_never_installs_its_copy_over_a_newer_saved_one() {
        let store = Arc::new(MapStore::default());
        let a = key("a");
        store
            .rows
            .lock()
            .unwrap()
            .insert(a.clone(), seeded("req_old", None, 500));
        let mem =
            Arc::new(SessionMemory::new(1, MEMORY_IDLE_MS).with_store(store.clone(), i64::MAX));
        let gate = Arc::new(Barrier::new(2));
        *store.hold_load.lock().unwrap() = Some(gate.clone());
        let held = tokio::spawn({
            let (mem, a) = (mem.clone(), a.clone());
            async move { mem.restore(&a, 1_000).await }
        });
        // The load has the row and is holding it; everything below happens while that read is in flight.
        fake_upstream::wait_for(Duration::from_secs(5), || {
            store.hold_load.lock().unwrap().is_none().then_some(())
        })
        .await;
        mem.record_request_id(&a, "acct", "req_new", 2_000);
        mem.flush(2_000).await;
        assert_eq!(request_id_of(&store, &a).as_deref(), Some("req_new"));
        // At a cap of one this write asks for an eviction, and the key whose read is in flight must not be
        // the one it takes: the guard the read runs afterwards would read an absent key as its own to fill.
        mem.record_request_id(&key("b"), "acct", "req_b", 3_000);
        tokio::task::spawn_blocking(move || gate.wait())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), held)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            mem.previous(&a, "acct", "claude-sonnet-4-5")
                .request_id
                .as_deref(),
            Some("req_new"),
            "the older copy the read carried never replaced the newer one"
        );
        mem.flush(4_000).await;
        assert_eq!(
            request_id_of(&store, &a).as_deref(),
            Some("req_new"),
            "and nothing wrote it back to the store"
        );
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
    async fn a_record_at_the_retention_bound_is_restored_and_one_a_millisecond_older_is_not() {
        let store = Arc::new(MapStore::default());
        let (at_bound, older) = (key("at"), key("older"));
        store
            .rows
            .lock()
            .unwrap()
            .insert(at_bound.clone(), seeded("req_at", None, 4_000));
        store
            .rows
            .lock()
            .unwrap()
            .insert(older.clone(), seeded("req_older", None, 3_999));
        let mem = Arc::new(SessionMemory::new(10, MEMORY_IDLE_MS).with_store(store.clone(), 1_000));
        mem.restore(&at_bound, 5_000).await;
        mem.restore(&older, 5_000).await;
        assert_eq!(
            mem.previous(&at_bound, "acct", "claude-sonnet-4-5")
                .request_id
                .as_deref(),
            Some("req_at"),
            "a row last seen exactly the retention ago is still inside it"
        );
        assert_eq!(
            mem.previous(&older, "acct", "claude-sonnet-4-5").request_id,
            None,
            "one millisecond older is past it"
        );
    }

    #[tokio::test]
    async fn an_overlay_record_the_build_cannot_decode_is_kept_and_still_flushed() {
        let store = Arc::new(MapStore::default());
        let mem =
            Arc::new(SessionMemory::new(10, MEMORY_IDLE_MS).with_store(store.clone(), i64::MAX));
        let a = key("a");
        let stashed = MemoryRecord {
            record_version: RECORD_VERSION,
            payload: "{".into(),
            last_seen: 1_000,
        };
        mem.inner
            .lock()
            .unwrap()
            .pending
            .insert(a.clone(), (stashed.clone(), 1));
        // The read that follows an eviction reaches the overlay first, and this record does not decode.
        mem.restore(&a, 2_000).await;
        assert!(
            mem.inner.lock().unwrap().pending.contains_key(&a),
            "a record that could not be decoded is still an unflushed change"
        );
        mem.flush(2_000).await;
        assert_eq!(
            store.rows.lock().unwrap().get(&a),
            Some(&stashed),
            "and the next flush writes it to the store"
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
