//! The live model catalog: a per-account cache fetched on demand and held for a short window, and the
//! published map that folds an undated spelling to the newest dated id the enabled accounts' listings carry.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use phoebis_oauth::{Clock, CredentialManager};
use phoebis_profile::catalog::{MODELS_TARGET, MODELS_USER_AGENT};
use phoebis_routing::listing::undated;
pub use phoebis_routing::listing::CatalogRow;
use phoebis_routing::RouterHandle;
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

/// The `YYYYMMDD` a dated id ends in, as an integer, so two snapshots of one spelling order by date.
fn snapshot_date(id: &str) -> Option<u32> {
    let (_, suffix) = id.rsplit_once('-')?;
    (suffix.len() == 8).then(|| suffix.parse().ok()).flatten()
}

const SLOTS_POISONED: &str = "catalog slots lock poisoned by a prior panic";
const PUBLISHED_POISONED: &str = "catalog published map poisoned by a prior panic";

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
    window: Duration,
    /// Spelling to the dated id the router has acknowledged; only ever grown or moved to a strictly newer
    /// snapshot, never shrunk.
    published: Mutex<HashMap<String, String>>,
    /// Held across the whole of `publish`, so two fills landing together derive, hand to the router, and insert
    /// one after the other rather than racing each other's read of `published`.
    publishing: tokio::sync::Mutex<()>,
    warmth: watch::Sender<Warmth>,
    fills_in_flight: watch::Sender<usize>,
    startup_bound: Duration,
    tasks: TaskTracker,
    stop: CancellationToken,
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
        window: Duration,
        startup_bound: Duration,
    ) -> CatalogService {
        let published = store
            .resolutions()
            .unwrap_or_default()
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
            window,
            published: Mutex::new(published),
            publishing: tokio::sync::Mutex::new(()),
            warmth: watch::Sender::new(Warmth::Cold),
            fills_in_flight: watch::Sender::new(0),
            startup_bound,
            tasks: TaskTracker::new(),
            stop: CancellationToken::new(),
        }
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

    /// The account's entry when younger than the window, else one fill (coalesced per account, and dropped
    /// unstored when `evict` ran under it) whose answer, rows or an error, is stored and returned; a fill that
    /// lands rows also derives and publishes the union's resolutions before the fetch is counted done.
    pub async fn fetch(&self, account: &AccountRow) -> Catalog {
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
        let discovered = self.discover(account).await;
        // The window opens when the answer is in hand: dated from before a discovery slower than the window,
        // the slot would be stale on commit and every caller waiting on the fill lock would discover again.
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
            self.slots.lock().expect(SLOTS_POISONED).insert(
                key,
                Slot {
                    cached: Some(catalog.clone()),
                },
            );
            if !catalog.models.is_empty() {
                self.publish(fetched_at).await;
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
        self.lower_in_flight();
        catalog
    }

    /// `evict` then `fetch`.
    pub async fn refresh(&self, account: &AccountRow) -> Catalog {
        self.evict(&account.id);
        self.fetch(account).await
    }

    /// Forgets every entry under the account id, whatever incarnation and revision it was filled for, and
    /// wins over any fill in flight; the account writers (create, delete, disconnect, secret patch, pairing)
    /// call it.
    pub fn evict(&self, account_id: &str) {
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
    /// running at the bound finishes in the background, and an account whose fetch failed is retried there on a
    /// doubling backoff from the cache window, capped at one hour, until it lands, the row leaves `Ready`, or
    /// `shutdown` cancels it.
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
        for (account, handle) in handles {
            match tokio::time::timeout_at(deadline, handle).await {
                Ok(Ok(catalog)) if catalog.error.is_none() => out.landed += 1,
                Ok(Ok(_)) => {
                    out.failed += 1;
                    self.spawn_retry(account);
                }
                Ok(Err(_)) => out.failed += 1,
                Err(_) => out.pending += 1,
            }
        }
        out
    }

    /// Cancels every tracked fill and retry; they end at their next await point. Synchronous, for a drop guard.
    pub fn cancel(&self) {
        self.stop.cancel();
        self.tasks.close();
    }

    /// `cancel`, then waits until every tracked task has ended, so nothing spawned here outlives the caller.
    pub async fn shutdown(&self) {
        self.cancel();
        self.tasks.wait().await;
    }

    /// One fetch failed inside the startup bound: retried here on a doubling backoff until rows land, the
    /// account leaves `Ready`, or `cancel` ends it with everything else the tracker holds.
    fn spawn_retry(self: &Arc<Self>, account: AccountRow) {
        let service = Arc::clone(self);
        self.tasks.spawn(async move {
            let mut backoff = service.window;
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
                backoff = (backoff * 2).min(Duration::from_secs(3_600));
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
                let token = self.manager.acquire(&account.id).await.map_err(|error| {
                    tracing::warn!(
                        account = %account.id,
                        %error,
                        "the catalog discovery could not obtain the account's credential"
                    );
                    DiscoveryError::NoCredential
                })?;
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
        (catalog.fetched_at + self.window.as_millis() as i64 > now).then(|| catalog.clone())
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
            })
        })
        .collect();
    Ok(Discovered { models })
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
        }
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
