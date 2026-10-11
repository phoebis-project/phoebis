//! The live model catalog: a per-account listing fetched on demand and held until the next read, and the
//! published map that folds an undated spelling to the newest dated id the enabled accounts' listings carry.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use phoebis_oauth::{Clock, CredentialManager};
use phoebis_profile::catalog::{MODELS_TARGET, MODELS_USER_AGENT};
pub use phoebis_routing::listing::CatalogRow;
use phoebis_routing::listing::{snapshot_date, undated};
use phoebis_routing::{Report, RouterHandle};
use phoebis_store::{AccountKind, AccountRow, AccountState, Secret, Store};
use phoebis_transport::control::Resolver;
use phoebis_transport::decode::{decoder, encoding};
use phoebis_transport::pool::{Pool, PoolKey};
use phoebis_transport::request::{write_request, RawRequest};
use phoebis_transport::response::{framing, read_head, BodyReader};
use phoebis_transport::TransportError;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::dispatch::UPSTREAM_AUTHORITY;

/// A models list is a few hundred short objects; anything past this is not one.
pub const MAX_BODY: usize = 4 * 1024 * 1024;

/// How often the periodic loop wakes to compare each slot's age against the refresh interval; a refresh lands
/// within one tick of falling due.
pub const REFRESH_TICK: Duration = Duration::from_secs(60);
/// The least time between two served-model refreshes of one account.
pub const SERVED_REFRESH_INTERVAL_MS: i64 = 60_000;
/// The retry backoff's ceiling.
const ONE_HOUR: Duration = Duration::from_secs(3_600);
/// How long a slot that holds a failed read answers before the account is read again on demand; also the
/// retry's first sleep. A listing is held until the scheduled refresh or an eviction replaces it.
pub const ERROR_HOLD: Duration = Duration::from_secs(60);

/// The dated id `spelling` resolves to under `listing`: the newest `<spelling>-YYYYMMDD` entry, or `None` when
/// the listing holds no dated entry for it (a dateless id of the 4.6 generation and later is its own snapshot).
pub fn derive(listing: &[CatalogRow], spelling: &str) -> Option<String> {
    listing
        .iter()
        .filter(|row| undated(&row.id) == Some(spelling))
        .map(|row| row.id.as_str())
        // The date is the id's last eight bytes and zero-padded, so the lexical maximum is the newest.
        .max()
        .map(str::to_string)
}

/// Every spelling `listing` implies with its newest dated id: one entry per undated base the listing carries a
/// dated id for, keyed by `phoebis_routing::listing::undated`.
pub fn derive_all(listing: &[CatalogRow]) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    for row in listing {
        if let Some(base) = undated(&row.id) {
            let entry = out.entry(base.to_string()).or_default();
            if row.id.as_str() > entry.as_str() {
                *entry = row.id.clone();
            }
        }
    }
    out
}

const SLOTS_POISONED: &str = "catalog slots lock poisoned by a prior panic";
const PUBLISHED_POISONED: &str = "catalog published map poisoned by a prior panic";
const LISTED_POISONED: &str = "catalog listed-ids set poisoned by a prior panic";
const SERVED_REFRESHED_POISONED: &str = "catalog served-refresh map poisoned by a prior panic";
const MISMATCHED_POISONED: &str = "catalog mismatched-triple set poisoned by a prior panic";
const LISTINGS_POISONED: &str = "catalog previous-listing map poisoned by a prior panic";
const ON_CHANGE_POISONED: &str = "catalog change hook poisoned by a prior panic";

/// The sink reports the room left in the call it refused; the caller's own bound is the number worth telling.
fn at_models_bound(error: TransportError) -> TransportError {
    match error {
        TransportError::TooLarge(_) => TransportError::TooLarge(MAX_BODY),
        other => other,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("the models response is malformed: {0}")]
    Malformed(String),
    #[error("the account has no credential")]
    NoCredential,
    #[error("the models endpoint answered {0}")]
    Status(u16),
    #[error("the credential was rejected")]
    Unauthorized,
    #[error("the models endpoint is unreachable: {0}")]
    Unreachable(String),
}

pub struct Discovered {
    pub models: Vec<CatalogRow>,
}

/// An account's listing differs from the one before: the ids that came and went and the new count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogChange {
    pub account_id: String,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub count: usize,
    pub at: i64,
}

pub type CatalogHook = Arc<dyn Fn(CatalogChange) + Send + Sync>;

/// An account's catalog as of one fetch: rows, or none with the error the fetch ended in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Catalog {
    pub models: Vec<CatalogRow>,
    pub fetched_at: i64,
    pub error: Option<String>,
}

/// Fences a cached or in-flight fetch to the credential it was read for: an id is reusable, and a deletion
/// followed by a re-creation, or a secret patch, must never answer with the replaced holder's rows.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SlotKey {
    account_id: String,
    incarnation: u64,
    revision: u64,
}

struct Slot {
    cached: Option<Catalog>,
}

/// What the startup fetch found before the bound: `landed` accounts answered rows, `failed` answered an error
/// and are retried in the background, `pending` were still fetching when the bound passed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Prefetched {
    pub landed: usize,
    pub failed: usize,
    pub pending: usize,
}

/// How far the cache is from answering a resolution: `Warm` once any enabled account's listing landed and its
/// moves were acknowledged in this lifetime and never earlier, `Filling` while a fill is in flight and nothing
/// has landed, `Cold` otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Warmth {
    Cold,
    Filling,
    Warm,
}

pub struct CatalogService {
    clock: Clock,
    manager: Arc<CredentialManager>,
    pool: Arc<Pool>,
    resolver: Resolver,
    router: RouterHandle,
    slots: Mutex<HashMap<SlotKey, Slot>>,
    /// Bumped per account id by `evict`, independently of whether a slot yet exists to clear: a fill that
    /// started before an eviction, on a key the eviction never got to insert a slot for, still has to lose.
    generations: Mutex<HashMap<String, u64>>,
    fills: tokio::sync::Mutex<HashMap<SlotKey, Arc<tokio::sync::Mutex<()>>>>,
    store: Arc<Store>,
    timeout: Duration,
    /// Spelling to the dated id the router has acknowledged; only ever grown or moved to a strictly newer
    /// snapshot, never shrunk.
    published: Mutex<HashMap<String, String>>,
    /// Every id the enabled union's listings carry, rewritten under the `publishing` lock each time `publish`
    /// derives: the API lists a spelling either dated or dateless, never a base beside its dated sibling, so
    /// an id here is already its own snapshot and a miss on it need not wait.
    listed: Mutex<HashSet<String>>,
    /// Held across the whole of `publish`, so two fills landing together derive, hand to the router, and insert
    /// one after the other rather than racing each other's read of `published`.
    publishing: tokio::sync::Mutex<()>,
    warmth: watch::Sender<Warmth>,
    fills_in_flight: watch::Sender<usize>,
    startup_bound: Duration,
    /// The clock reading of each account's last served-model refresh, rate-limiting `note_served`.
    served_refreshed: Mutex<HashMap<String, i64>>,
    /// Every `(account_id, resolved, served)` triple whose served-model refresh landed rows without the union
    /// ever deriving `served` for `resolved`'s spelling: a mismatch no further refresh can fix. Cleared for an
    /// account outright by the periodic sweep's next refresh of it; a served-model refresh instead re-judges
    /// the account's triples one by one, since it must not erase the ones it did not just answer for.
    mismatched: Mutex<HashSet<(String, String, String)>>,
    /// Each account's last listing as its ids, with the incarnation it was read for: what the next listing is
    /// compared with. Kept apart from the slots, since a refresh empties the account's slot before it reads.
    listings: Mutex<HashMap<String, (u64, BTreeSet<String>)>>,
    on_change: Mutex<Option<CatalogHook>>,
    tasks: TaskTracker,
    stop: CancellationToken,
}

/// Lowers `fills_in_flight` on drop, whether `fetch` ran to its end or its future was dropped mid-poll: the
/// count must return to zero on every exit, not only the one its author had in mind.
struct InFlightGuard<'a>(&'a CatalogService);

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.0.lower_in_flight();
    }
}

impl CatalogService {
    /// `pool` and `resolver` are the catalog's own transport; `router` takes each landed resolution to the
    /// routing state, and `store.resolutions()` seeds `published` so a restart answers before any fetch lands.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<Store>,
        manager: Arc<CredentialManager>,
        pool: Arc<Pool>,
        resolver: Resolver,
        router: RouterHandle,
        clock: Clock,
        timeout: Duration,
        startup_bound: Duration,
    ) -> CatalogService {
        // A read failure here leaves `published` empty rather than failing construction: the router's own
        // resolution rows, loaded independently at its own start, are unaffected, and every spelling falls
        // back to a fill or the spelling itself until one lands.
        let published = store
            .resolutions()
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "the catalog's persisted resolutions could not be read");
                Vec::new()
            })
            .into_iter()
            .map(|row| (row.spelling, row.dated_id))
            .collect();
        CatalogService {
            clock,
            manager,
            pool,
            resolver,
            router,
            slots: Mutex::new(HashMap::new()),
            generations: Mutex::new(HashMap::new()),
            fills: tokio::sync::Mutex::new(HashMap::new()),
            store,
            timeout,
            published: Mutex::new(published),
            listed: Mutex::new(HashSet::new()),
            publishing: tokio::sync::Mutex::new(()),
            warmth: watch::Sender::new(Warmth::Cold),
            fills_in_flight: watch::Sender::new(0),
            startup_bound,
            served_refreshed: Mutex::new(HashMap::new()),
            mismatched: Mutex::new(HashSet::new()),
            listings: Mutex::new(HashMap::new()),
            on_change: Mutex::new(None),
            tasks: TaskTracker::new(),
            stop: CancellationToken::new(),
        }
    }

    /// Called once per fill whose listing differs from the one the slot held. The first listing of an account
    /// fires nothing, since there is no listing before it to differ from.
    pub fn set_on_change(&self, hook: CatalogHook) {
        *self.on_change.lock().expect(ON_CHANGE_POISONED) = Some(hook);
    }

    /// The uncached probe with the measured request shape; the admin's verify and the staged-secret check.
    pub async fn discover(&self, account: &AccountRow) -> Result<Discovered, DiscoveryError> {
        self.discover_with(account, None).await
    }

    /// `secret` is a staged replacement: the admin route verifies it against the account before the store
    /// learns of it. On create there is no stored row to fall back to.
    pub async fn discover_with(
        &self,
        account: &AccountRow,
        secret: Option<&Secret>,
    ) -> Result<Discovered, DiscoveryError> {
        // The credential step (an OAuth `acquire` that may refresh) shares the round trip's own bound, so
        // the whole discovery is bounded by `self.timeout`, not just the network exchange.
        let (status, body) = tokio::time::timeout(self.timeout, async {
            let headers = self.credential_headers(account, secret).await?;
            self.send(account, MODELS_TARGET.to_string(), headers).await
        })
        .await
        .map_err(|_| DiscoveryError::Unreachable("timed out".into()))??;
        match status {
            200 => parse_models(&body),
            401 => Err(DiscoveryError::Unauthorized),
            other => Err(DiscoveryError::Status(other)),
        }
    }

    /// The account's stored listing, or its failed read while younger than `ERROR_HOLD`, else one fill
    /// (coalesced per account, and dropped unstored when `evict` ran under it) whose answer, rows or an error,
    /// is stored and returned; a fill that lands rows also derives and publishes the union's resolutions before
    /// the fetch is counted done. The publication itself runs on a tracked task: a caller whose own future is
    /// dropped mid-wait (the three HTTP handlers below, on a client disconnect) leaves the publication to
    /// finish rather than abandoning it mid-way.
    pub async fn fetch(self: &Arc<Self>, account: &AccountRow) -> Catalog {
        let key = SlotKey {
            account_id: account.id.clone(),
            incarnation: account.incarnation,
            revision: account.revision,
        };
        if let Some(catalog) = self.fresh_slot(&key, (self.clock)()) {
            return catalog;
        }
        let fill_lock = {
            let mut fills = self.fills.lock().await;
            fills
                .entry(key.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _guard = fill_lock.lock().await;
        if let Some(catalog) = self.fresh_slot(&key, (self.clock)()) {
            return catalog;
        }
        let generation = self
            .generations
            .lock()
            .expect(SLOTS_POISONED)
            .get(&key.account_id)
            .copied()
            .unwrap_or(0);
        self.raise_in_flight();
        // Two callers await `fetch` inside an HTTP handler (the catalog popup, `GET /v1/models`) whose future
        // the server drops on a client disconnect; the guard's `Drop` still lowers the count then, so a caller
        // leaving mid-fill never strands a later miss on the full wait.
        let _lower_on_drop = InFlightGuard(self);
        let discovered = self.discover(account).await;
        // The hold opens when the answer is in hand: dated from before a discovery slower than `ERROR_HOLD`, a
        // failed slot would be stale on commit and every caller waiting on the fill lock would discover again.
        let fetched_at = (self.clock)();
        let catalog = match discovered {
            Ok(discovered) => Catalog {
                models: discovered.models,
                fetched_at,
                error: None,
            },
            Err(error) => Catalog {
                models: Vec::new(),
                fetched_at,
                error: Some(error.to_string()),
            },
        };
        // The commit-time fence: a slot read or filled for one credential must never answer for the next, so
        // the row is re-read and both the generation (bumped independently of a slot's existence, so a fill
        // that started before the account even had one still loses) and the holder it names must still match.
        let reread = self.store.account(&account.id).ok().flatten();
        let still_current = reread
            .as_ref()
            .is_some_and(|row| row.incarnation == key.incarnation && row.revision == key.revision);
        let current_generation = self
            .generations
            .lock()
            .expect(SLOTS_POISONED)
            .get(&key.account_id)
            .copied()
            .unwrap_or(0);
        if still_current && current_generation == generation {
            let change = catalog
                .error
                .is_none()
                .then(|| self.listing_changed(&key, &catalog))
                .flatten();
            self.slots.lock().expect(SLOTS_POISONED).insert(
                key,
                Slot {
                    cached: Some(catalog.clone()),
                },
            );
            if let Some(change) = change {
                let hook = self.on_change.lock().expect(ON_CHANGE_POISONED).clone();
                if let Some(hook) = hook {
                    hook(change);
                }
            }
            if !catalog.models.is_empty() {
                let service = Arc::clone(self);
                let handle = self
                    .tasks
                    .spawn(async move { service.publish(fetched_at).await });
                if let Err(error) = handle.await {
                    tracing::warn!(%error, "a catalog publication task did not finish cleanly");
                }
                if reread.as_ref().is_some_and(|row| row.enabled) {
                    self.warmth.send_if_modified(|w| match w {
                        Warmth::Warm => false,
                        other => {
                            *other = Warmth::Warm;
                            true
                        }
                    });
                }
            }
        }
        catalog
    }

    /// What a landed listing moved against the account's previous one, which it then replaces, or `None` for the
    /// first listing of a holder, whose incarnation names no listing before it, and for one that moved nothing.
    fn listing_changed(&self, key: &SlotKey, catalog: &Catalog) -> Option<CatalogChange> {
        let ids: BTreeSet<String> = catalog.models.iter().map(|row| row.id.clone()).collect();
        let (incarnation, before) = self
            .listings
            .lock()
            .expect(LISTINGS_POISONED)
            .insert(key.account_id.clone(), (key.incarnation, ids.clone()))?;
        if incarnation != key.incarnation {
            return None;
        }
        let added: Vec<String> = ids.difference(&before).cloned().collect();
        let removed: Vec<String> = before.difference(&ids).cloned().collect();
        (!added.is_empty() || !removed.is_empty()).then(|| CatalogChange {
            account_id: key.account_id.clone(),
            added,
            removed,
            count: catalog.models.len(),
            at: catalog.fetched_at,
        })
    }

    /// `invalidate_listing` then `fetch`.
    pub async fn refresh(self: &Arc<Self>, account: &AccountRow) -> Catalog {
        self.invalidate_listing(&account.id);
        self.fetch(account).await
    }

    /// Refreshes every enabled `Ready` account whose listing is `every` old, checked each `tick` on the
    /// service's clock; the handle ends with the service. Accounts refresh one after another, a known bound: a
    /// slow upstream stretches the sweep by its own round trips, and nothing shrinks it meanwhile, since a
    /// published spelling never moves back to an older snapshot. A due account with nothing already filling it
    /// is read through `refresh` (the listing invalidated, then fetched), since a plain fetch would answer from
    /// the held listing without a request; a due account with a fill already in flight is skipped, since that
    /// fill's own landing is a fresher read than this tick could force.
    pub fn spawn_refresh(self: &Arc<Self>, every: Duration, tick: Duration) -> JoinHandle<()> {
        let service = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tick).await;
                let now = (service.clock)();
                let accounts: Vec<AccountRow> = service
                    .store
                    .accounts()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|a| a.enabled && a.state == AccountState::Ready)
                    .collect();
                for account in accounts {
                    if service.slot_due(&account, every, now) && !service.fill_in_flight(&account) {
                        service.refresh(&account).await;
                        // The one place an account's mismatches are dropped outright: a fresh listing on
                        // a due account's own turn deserves to judge everything afresh, unlike a
                        // served-model refresh, which re-judges one triple beside the others it carries.
                        service.clear_mismatched(&account.id);
                    }
                }
            }
        })
    }

    /// Whether the account's current slot (keyed on its own incarnation and revision) is missing or has aged
    /// past `every`.
    fn slot_due(&self, account: &AccountRow, every: Duration, now: i64) -> bool {
        let key = SlotKey {
            account_id: account.id.clone(),
            incarnation: account.incarnation,
            revision: account.revision,
        };
        let fetched_at = self
            .slots
            .lock()
            .expect(SLOTS_POISONED)
            .get(&key)
            .and_then(|slot| slot.cached.as_ref())
            .map(|catalog| catalog.fetched_at);
        match fetched_at {
            Some(fetched_at) => fetched_at + every.as_millis() as i64 <= now,
            None => true,
        }
    }

    /// Whether another caller is between `fetch`'s two `fresh_slot` checks for this account's current slot key
    /// right now: its coalescing lock is held. A map this cannot even inspect (another insert mid-flight) counts
    /// as in flight too, since skipping a tick costs nothing but colliding with a caller it cannot see does.
    fn fill_in_flight(&self, account: &AccountRow) -> bool {
        let key = SlotKey {
            account_id: account.id.clone(),
            incarnation: account.incarnation,
            revision: account.revision,
        };
        let Ok(fills) = self.fills.try_lock() else {
            return true;
        };
        match fills.get(&key) {
            Some(lock) => lock.try_lock().is_err(),
            None => false,
        }
    }

    /// An answer named `served` where the router had resolved `resolved`: one background refresh of the
    /// account, skipped inside `SERVED_REFRESH_INTERVAL_MS` of the account's last such refresh; a match is a
    /// no-op. A triple this remembers as a standing mismatch (the refresh landed and the union still does not
    /// derive `served` for `resolved`'s spelling, the case of an alias whose target the listing never carries)
    /// is skipped at once rather than refreshed again.
    pub fn note_served(self: &Arc<Self>, account_id: &str, resolved: &str, served: &str) {
        if served == resolved {
            return;
        }
        // A body completing after `cancel`, which `shutdown` runs first, must not spawn a refresh that outlives it.
        if self.stop.is_cancelled() {
            return;
        }
        let triple = (
            account_id.to_string(),
            resolved.to_string(),
            served.to_string(),
        );
        if self
            .mismatched
            .lock()
            .expect(MISMATCHED_POISONED)
            .contains(&triple)
        {
            return;
        }
        let now = (self.clock)();
        {
            let mut refreshed = self
                .served_refreshed
                .lock()
                .expect(SERVED_REFRESHED_POISONED);
            if refreshed
                .get(account_id)
                .is_some_and(|&last| now - last < SERVED_REFRESH_INTERVAL_MS)
            {
                return;
            }
            refreshed.insert(account_id.to_string(), now);
        }
        let service = Arc::clone(self);
        self.tasks.spawn(async move {
            let Ok(Some(row)) = service.store.account(&triple.0) else {
                return;
            };
            if !row.enabled || row.state != AccountState::Ready {
                return;
            }
            let refreshed = service.refresh(&row).await;
            // A refresh that ended in an error leaves `lookup` at the old mapping, which would misjudge
            // every carried triple as still mismatched; nothing is remembered from a refresh that answered
            // no rows, so the next occurrence past the minute retries as before rather than staying silent.
            if refreshed.error.is_none() {
                let mut mismatched = service.mismatched.lock().expect(MISMATCHED_POISONED);
                let carried: Vec<(String, String, String)> = mismatched
                    .iter()
                    .filter(|(id, _, _)| id == &triple.0)
                    .cloned()
                    .collect();
                for candidate in carried.into_iter().chain(std::iter::once(triple)) {
                    let spelling = undated(&candidate.1).unwrap_or(&candidate.1).to_string();
                    if service.lookup(&spelling).as_deref() == Some(candidate.2.as_str()) {
                        mismatched.remove(&candidate);
                    } else {
                        mismatched.insert(candidate);
                    }
                }
            }
        });
    }

    /// Forgets every entry under the account id, whatever incarnation and revision it was filled for, and
    /// wins over any fill in flight; the account writers (create, delete, disconnect, secret patch, pairing)
    /// call it.
    pub fn evict(&self, account_id: &str) {
        self.invalidate_listing(account_id);
        self.served_refreshed
            .lock()
            .expect(SERVED_REFRESHED_POISONED)
            .remove(account_id);
    }

    /// `evict` without the served-model rate limit: bumps the account's generation and drops its cached
    /// listing and any fill in flight, but leaves `served_refreshed` untouched. `refresh` uses this rather
    /// than `evict`, so a served-model refresh stalled on its own I/O does not also drop the reservation that
    /// keeps a second mismatching answer from starting another one meanwhile.
    fn invalidate_listing(&self, account_id: &str) {
        *self
            .generations
            .lock()
            .expect(SLOTS_POISONED)
            .entry(account_id.to_string())
            .or_insert(0) += 1;
        let mut slots = self.slots.lock().expect(SLOTS_POISONED);
        slots.retain(|key, _| key.account_id != account_id);
        drop(slots);
        // Best effort: a fill lock this misses is merely unused from here on, never looked up again under
        // the evicted holder's key, so leaving it costs nothing but a little memory.
        if let Ok(mut fills) = self.fills.try_lock() {
            fills.retain(|key, _| key.account_id != account_id);
        }
    }

    /// Forgets every mismatch remembered for the account outright, so a fresh listing is judged on its own
    /// merits rather than one carried from before: the periodic sweep's own refresh of a due account calls it.
    fn clear_mismatched(&self, account_id: &str) {
        self.mismatched
            .lock()
            .expect(MISMATCHED_POISONED)
            .retain(|(id, _, _)| id != account_id);
    }

    /// The dated id the router has acknowledged for a spelling, or `None`; never waits and never asks anyone.
    pub fn lookup(&self, spelling: &str) -> Option<String> {
        self.published
            .lock()
            .expect(PUBLISHED_POISONED)
            .get(spelling)
            .cloned()
    }

    /// `lookup`, after a wait bounded by the startup bound on a miss while any fill is in flight: the canonical
    /// name admission records the request under, the spelling itself when nothing dated is known.
    pub async fn canonical_for(&self, spelling: &str) -> String {
        if let Some(dated) = self.lookup(spelling) {
            return dated;
        }
        // A dated id resolves to itself by definition; the API lists a spelling either dated or dateless,
        // never a base beside its dated sibling, so an id the union already lists exactly is already its
        // own snapshot. Waiting for a fill buys neither of them anything.
        if undated(spelling).is_some()
            || self
                .listed
                .lock()
                .expect(LISTED_POISONED)
                .contains(spelling)
        {
            return spelling.to_string();
        }
        let mut rx = self.fills_in_flight.subscribe();
        if *rx.borrow() > 0 {
            // The count, not the warmth, gates the wait: a listing that landed for one account leaves the
            // service `Warm` while another account's fill, the one that will resolve this spelling, is still
            // in flight.
            let _ = tokio::time::timeout(self.startup_bound, rx.wait_for(|n| *n == 0)).await;
        }
        self.lookup(spelling)
            .unwrap_or_else(|| spelling.to_string())
    }

    pub fn warmth(&self) -> Warmth {
        *self.warmth.borrow()
    }

    /// One `fetch` per enabled `Ready` account, concurrently, awaited up to the startup bound; a fetch still
    /// running at the bound is tracked further and, once it lands, retried the same way if it failed. An
    /// account whose fetch failed inside the bound is retried at once, on a doubling backoff from
    /// `ERROR_HOLD` up to one hour, until it lands, the row leaves `Ready`, or `shutdown` cancels it.
    pub async fn prefetch(self: &Arc<Self>) -> Prefetched {
        let deadline = tokio::time::Instant::now() + self.startup_bound;
        let accounts: Vec<AccountRow> = self
            .store
            .accounts()
            .unwrap_or_default()
            .into_iter()
            .filter(|a| a.enabled && a.state == AccountState::Ready)
            .collect();
        let handles: Vec<(AccountRow, JoinHandle<Catalog>)> = accounts
            .into_iter()
            .map(|account| {
                let service = Arc::clone(self);
                let for_task = account.clone();
                let handle = self
                    .tasks
                    .spawn(async move { service.fetch(&for_task).await });
                (account, handle)
            })
            .collect();
        let mut out = Prefetched::default();
        for (account, mut handle) in handles {
            match tokio::time::timeout_at(deadline, &mut handle).await {
                Ok(Ok(catalog)) if catalog.error.is_none() => out.landed += 1,
                Ok(Ok(_)) => {
                    out.failed += 1;
                    self.spawn_retry(account);
                }
                Ok(Err(_)) => {
                    out.failed += 1;
                    self.spawn_retry(account);
                }
                Err(_) => {
                    out.pending += 1;
                    self.spawn_pending_retry(account, handle);
                }
            }
        }
        out
    }

    /// A fetch still running at the startup bound: tracked here so its answer, once it lands, still enters the
    /// retry loop on an error the way a fetch that failed inside the bound already does.
    fn spawn_pending_retry(self: &Arc<Self>, account: AccountRow, handle: JoinHandle<Catalog>) {
        let service = Arc::clone(self);
        self.tasks.spawn(async move {
            match handle.await {
                Ok(catalog) if catalog.error.is_none() => {}
                Ok(_) => service.spawn_retry(account),
                Err(error) => {
                    tracing::warn!(%error, "a startup catalog fetch past the bound did not finish cleanly");
                    service.spawn_retry(account);
                }
            }
        });
    }

    /// Cancels every tracked fill and retry: a sleeping retry ends at once; a fill in flight ends with its
    /// round trip, bounded by the discovery timeout. Synchronous, for a drop guard.
    pub fn cancel(&self) {
        self.stop.cancel();
        self.tasks.close();
    }

    /// `cancel`, then waits until every tracked task has ended, so nothing spawned here outlives the caller.
    pub async fn shutdown(&self) {
        self.cancel();
        self.tasks.wait().await;
    }

    /// One fetch failed: retried here on a doubling backoff from `ERROR_HOLD` up to one hour, until rows land,
    /// the account leaves `Ready`, or `cancel` ends it with everything else the tracker holds.
    fn spawn_retry(self: &Arc<Self>, account: AccountRow) {
        let service = Arc::clone(self);
        self.tasks.spawn(async move {
            let mut backoff = ERROR_HOLD;
            loop {
                tokio::select! {
                    _ = service.stop.cancelled() => return,
                    _ = tokio::time::sleep(backoff) => {}
                }
                let Ok(Some(row)) = service.store.account(&account.id) else {
                    return;
                };
                if !row.enabled || row.state != AccountState::Ready {
                    return;
                }
                if service.refresh(&row).await.error.is_none() {
                    return;
                }
                backoff = (backoff * 2).min(ONE_HOUR);
            }
        });
    }

    /// Derives the union's mapping and hands every moved spelling to the router, one publication at a time: the
    /// lock covers the derivation, the acknowledgement and the insert, so two fills landing together cannot leave
    /// an older acknowledgement behind a newer one, and a spelling only ever moves to a strictly newer snapshot,
    /// so a stale listing (an account fetched after a restart whose org lags the others) moves nothing.
    /// `published` takes a move only once the router answered, so a request is named by a dated id no earlier
    /// than its rows are keyed on it.
    async fn publish(&self, now: i64) {
        let _serialized = self.publishing.lock().await;
        let enabled: HashSet<String> = self
            .store
            .accounts()
            .unwrap_or_default()
            .into_iter()
            .filter(|a| a.enabled)
            .map(|a| a.id)
            .collect();
        let union: Vec<CatalogRow> = {
            let slots = self.slots.lock().expect(SLOTS_POISONED);
            slots
                .iter()
                .filter(|(key, _)| enabled.contains(&key.account_id))
                .filter_map(|(_, slot)| slot.cached.as_ref())
                .flat_map(|catalog| catalog.models.iter().cloned())
                .collect()
        };
        *self.listed.lock().expect(LISTED_POISONED) =
            union.iter().map(|row| row.id.clone()).collect();
        let moved: Vec<(String, String)> = {
            let published = self.published.lock().expect(PUBLISHED_POISONED);
            derive_all(&union)
                .into_iter()
                .filter(|(spelling, dated)| match published.get(spelling) {
                    None => true,
                    Some(current) => snapshot_date(dated) > snapshot_date(current),
                })
                .collect()
        };
        for (spelling, dated) in moved {
            match self.router.resolve(&spelling, &dated, now).await {
                Ok(()) => {
                    self.published
                        .lock()
                        .expect(PUBLISHED_POISONED)
                        .insert(spelling, dated);
                }
                // Left unpublished: requests keep the name the router's rows are under, and the next landing asks again.
                Err(error) => {
                    tracing::debug!(%spelling, %error, "a resolution was not handed to the router")
                }
            }
        }
    }

    /// The count leaves zero: `Filling` opens unless the cache is already `Warm`.
    fn raise_in_flight(&self) {
        self.fills_in_flight.send_modify(|n| *n += 1);
        self.warmth.send_if_modified(|w| match w {
            Warmth::Cold => {
                *w = Warmth::Filling;
                true
            }
            _ => false,
        });
    }

    /// The count returns to zero: `Cold` resumes unless a landed publication already made the cache `Warm`.
    fn lower_in_flight(&self) {
        let mut at_zero = false;
        self.fills_in_flight.send_modify(|n| {
            *n -= 1;
            at_zero = *n == 0;
        });
        if at_zero {
            self.warmth.send_if_modified(|w| match w {
                Warmth::Warm => false,
                other => {
                    *other = Warmth::Cold;
                    true
                }
            });
        }
    }

    /// Credential resolution for the list fetch, shared with the admin route's own verification of a staged
    /// secret before the store learns of it: `secret` is only ever set from that path, every other caller
    /// reads the account's own.
    async fn credential_headers(
        &self,
        account: &AccountRow,
        secret: Option<&Secret>,
    ) -> Result<Vec<(String, String)>, DiscoveryError> {
        let credential = match (account.kind, secret) {
            (AccountKind::Oauth, _) => {
                let token = match self.manager.acquire_for(account, None).await {
                    Ok(token) => token,
                    Err(error) => {
                        tracing::warn!(
                            account = %account.id,
                            %error,
                            "the catalog discovery could not obtain the account's credential"
                        );
                        // The manager's store write starts no reload, so the mark reaches the routing through
                        // the report the relay sends for its own.
                        if let Some(mark) = error.landed_mark() {
                            self.router
                                .report(Report::AccountState {
                                    account_id: mark.account_id.clone(),
                                    state: mark.state,
                                    incarnation: mark.incarnation,
                                    lifecycle_generation: mark.lifecycle_generation,
                                    credential_generation: mark.credential_generation,
                                    cause: mark.cause,
                                    detail: None,
                                    dispatched_at: None,
                                })
                                .await;
                        }
                        return Err(DiscoveryError::NoCredential);
                    }
                };
                (
                    "Authorization".to_string(),
                    format!("Bearer {}", token.secret.expose()),
                )
            }
            (AccountKind::ApiKey, Some(staged)) => {
                ("x-api-key".to_string(), staged.expose().to_string())
            }
            (AccountKind::ApiKey, None) => {
                let stored = match self.store.api_key_secret(&account.id) {
                    Ok(secret) => secret,
                    Err(error) => {
                        tracing::warn!(
                            account = %account.id,
                            %error,
                            "the catalog discovery could not obtain the account's credential"
                        );
                        None
                    }
                };
                (
                    "x-api-key".to_string(),
                    stored
                        .ok_or(DiscoveryError::NoCredential)?
                        .expose()
                        .to_string(),
                )
            }
        };
        Ok(vec![
            credential,
            ("User-Agent".into(), MODELS_USER_AGENT.into()),
            ("anthropic-version".into(), "2023-06-01".into()),
            ("Connection".into(), "keep-alive".into()),
            ("Accept".into(), "*/*".into()),
            ("Host".into(), UPSTREAM_AUTHORITY.to_string()),
            ("Accept-Encoding".into(), "gzip, deflate, br, zstd".into()),
        ])
    }

    /// The GET round trip over the catalog's own pool and resolver, the list fetch's only use of either.
    async fn send(
        &self,
        account: &AccountRow,
        target: String,
        headers: Vec<(String, String)>,
    ) -> Result<(u16, Vec<u8>), DiscoveryError> {
        let request = RawRequest {
            method: "GET",
            target,
            headers,
            body: Bytes::new(),
        };
        let addr = (self.resolver)(UPSTREAM_AUTHORITY).await.ok_or_else(|| {
            DiscoveryError::Unreachable(format!("no address for {UPSTREAM_AUTHORITY}"))
        })?;
        tokio::time::timeout(self.timeout, async {
            let key = PoolKey {
                account_id: account.id.clone(),
                host: UPSTREAM_AUTHORITY.to_string(),
            };
            let (mut conn, _) = self
                .pool
                .checkout(key.clone(), UPSTREAM_AUTHORITY, addr)
                .await
                .map_err(|e| DiscoveryError::Unreachable(e.to_string()))?;
            let (status, body, reusable) = exchange(&mut conn, &request).await?;
            if reusable {
                self.pool.checkin(key, conn);
            }
            Ok((status, body))
        })
        .await
        .map_err(|_| DiscoveryError::Unreachable("timed out".into()))?
    }

    fn fresh_slot(&self, key: &SlotKey, now: i64) -> Option<Catalog> {
        let slots = self.slots.lock().expect(SLOTS_POISONED);
        let catalog = slots.get(key)?.cached.as_ref()?;
        let held =
            catalog.error.is_none() || catalog.fetched_at + ERROR_HOLD.as_millis() as i64 > now;
        held.then(|| catalog.clone())
    }
}

async fn exchange<C: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut C,
    request: &RawRequest,
) -> Result<(u16, Vec<u8>, bool), DiscoveryError> {
    write_request(conn, request)
        .await
        .map_err(|f| DiscoveryError::Unreachable(f.error.to_string()))?;
    let mut leftover = BytesMut::new();
    let head = read_head(conn, &mut leftover)
        .await
        .map_err(|e| DiscoveryError::Unreachable(e.to_string()))?;
    let framing = framing(&head);
    let reusable = phoebis_transport::response::keep_alive(&head, &framing);
    let mut decoder =
        decoder(encoding(&head).map_err(|e| DiscoveryError::Malformed(e.to_string()))?);
    let mut reader = BodyReader::new(&mut *conn, leftover, framing);
    let mut out = Vec::new();
    while let Some(chunk) = reader
        .next_chunk()
        .await
        .map_err(|e| DiscoveryError::Unreachable(e.to_string()))?
    {
        out.extend(
            decoder
                .feed_bounded(&chunk, MAX_BODY - out.len())
                .map_err(at_models_bound)
                .map_err(|e| DiscoveryError::Malformed(e.to_string()))?,
        );
    }
    out.extend(
        decoder
            .finish_bounded(MAX_BODY - out.len())
            .map_err(at_models_bound)
            .map_err(|e| DiscoveryError::Malformed(e.to_string()))?,
    );
    Ok((head.status, out, reusable))
}

fn parse_models(body: &[u8]) -> Result<Discovered, DiscoveryError> {
    let root: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| DiscoveryError::Malformed(e.to_string()))?;
    let data = root
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| DiscoveryError::Malformed("no data array".into()))?;
    let models = data
        .iter()
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?.to_string();
            let display_name = entry
                .get("display_name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let created_at = entry
                .get("created_at")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let max_input_tokens = entry.get("max_input_tokens").and_then(|v| v.as_u64());
            let max_tokens = entry.get("max_tokens").and_then(|v| v.as_u64());
            Some(CatalogRow {
                id,
                display_name,
                created_at,
                max_input_tokens,
                max_tokens,
                effort_levels: effort_levels(entry),
            })
        })
        .collect();
    Ok(Discovered { models })
}

/// The API's own effort order, which a row's levels are shown in; a name outside it follows, as listed.
const EFFORT_ORDER: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// The effort levels a listing row says its model supports. The Models API reference shows `capabilities.effort` as
/// an object of levels, each an object with its own `supported`, beside the effort object's own `supported` flag;
/// no captured listing confirms it, so an array of names reads too, and any other shape reads as none rather than
/// failing the row.
fn effort_levels(entry: &serde_json::Value) -> Vec<String> {
    let listed: Vec<&str> = match entry.get("capabilities").and_then(|c| c.get("effort")) {
        Some(serde_json::Value::Object(levels))
            if levels.get("supported").and_then(|s| s.as_bool()) == Some(false) =>
        {
            Vec::new()
        }
        // The effort object's own flag is a bare boolean, which no level is, so only object entries count.
        Some(serde_json::Value::Object(levels)) => levels
            .iter()
            .filter(|(_, level)| {
                level.as_object().is_some_and(|level| {
                    level
                        .get("supported")
                        .is_none_or(|supported| supported.as_bool() == Some(true))
                })
            })
            .map(|(name, _)| name.as_str())
            .collect(),
        Some(serde_json::Value::Array(levels)) => {
            levels.iter().filter_map(|level| level.as_str()).collect()
        }
        _ => Vec::new(),
    };
    let mut out: Vec<String> = EFFORT_ORDER
        .iter()
        .filter(|level| listed.contains(level))
        .map(|level| level.to_string())
        .collect();
    for level in listed {
        if !out.iter().any(|kept| kept == level) {
            out.push(level.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str) -> CatalogRow {
        CatalogRow {
            id: id.to_string(),
            display_name: String::new(),
            created_at: String::new(),
            max_input_tokens: None,
            max_tokens: None,
            effort_levels: Vec::new(),
        }
    }

    #[test]
    fn the_effort_levels_read_from_a_capabilities_object_or_array_and_an_unknown_shape_reads_none()
    {
        let body = serde_json::json!({ "data": [
            { "id": "object", "capabilities": { "effort": {
                "low": { "supported": true },
                "medium": { "supported": true },
                "max": { "supported": false },
            } } },
            { "id": "array", "capabilities": { "effort": ["high", "low"] } },
            { "id": "unknown", "capabilities": { "effort": "all" } },
            { "id": "absent" },
            { "id": "others", "capabilities": { "effort": {
                "turbo": {},
                "max": {},
                "eco": { "supported": true },
                "low": { "supported": true },
            } } },
            // The Models API reference's shape: the levels beside the object's own `supported` flag, keys sorted.
            { "id": "documented", "capabilities": { "effort": {
                "high": { "supported": true },
                "low": { "supported": true },
                "max": { "supported": true },
                "medium": { "supported": true },
                "supported": true,
                "xhigh": { "supported": true },
            } } },
            { "id": "unsupported", "capabilities": { "effort": { "supported": false } } },
            { "id": "unsupported_listed", "capabilities": { "effort": {
                "low": { "supported": true },
                "supported": false,
            } } },
        ] });
        let models = parse_models(body.to_string().as_bytes()).unwrap().models;
        assert_eq!(models.len(), 8, "no shape of capabilities drops its row");
        let levels = |id: &str| {
            models
                .iter()
                .find(|m| m.id == id)
                .map(|m| m.effort_levels.clone())
                .unwrap()
        };
        assert_eq!(levels("object"), vec!["low", "medium"]);
        assert_eq!(levels("array"), vec!["low", "high"]);
        assert!(levels("unknown").is_empty());
        assert!(levels("absent").is_empty());
        assert_eq!(
            levels("others"),
            vec!["low", "max", "turbo", "eco"],
            "the API's levels first, an absent supported keeps its level, other names as listed"
        );
        assert_eq!(
            levels("documented"),
            vec!["low", "medium", "high", "xhigh", "max"],
            "the object's own supported flag is no level"
        );
        assert!(levels("unsupported").is_empty());
        assert!(
            levels("unsupported_listed").is_empty(),
            "an effort object marked unsupported lists no level"
        );
    }

    #[test]
    fn the_newest_dated_entry_wins_and_a_dateless_id_is_its_own_snapshot() {
        let listing = vec![
            row("claude-sonnet-4-5-20250601"),
            row("claude-sonnet-4-5-20250929"),
            row("claude-opus-5"),
            row("claude-sonnet-4-6"),
        ];
        assert_eq!(
            derive(&listing, "claude-sonnet-4-5"),
            Some("claude-sonnet-4-5-20250929".to_string())
        );
        assert_eq!(derive(&listing, "claude-opus-5"), None);
        assert_eq!(derive(&listing, "claude-sonnet-4-6"), None);
        assert_eq!(derive(&listing, "claude-nope"), None);
        assert_eq!(derive(&listing, "claude-sonnet-4-5-20250929"), None);
        let all = derive_all(&listing);
        assert_eq!(all.len(), 1);
        assert_eq!(
            all.get("claude-sonnet-4-5"),
            Some(&"claude-sonnet-4-5-20250929".to_string())
        );
    }
}
