//! The live model catalog: a per-account cache fetched on demand and held for a short window, and the
//! resolution map that folds an undated spelling to the dated id the API lists, off the request path.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use phoebis_oauth::{Clock, CredentialManager};
use phoebis_profile::catalog::{MODELS_TARGET, MODELS_USER_AGENT};
pub use phoebis_routing::listing::CatalogRow;
use phoebis_routing::{Report, RouterHandle};
use phoebis_store::{AccountKind, AccountRow, AccountState, Secret, Store};
use phoebis_transport::control::Resolver;
use phoebis_transport::decode::{decoder, encoding};
use phoebis_transport::pool::{Pool, PoolKey};
use phoebis_transport::request::{write_request, RawRequest};
use phoebis_transport::response::{framing, read_head, BodyReader};
use phoebis_transport::TransportError;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::dispatch::UPSTREAM_AUTHORITY;

/// A models list is a few hundred short objects; anything past this is not one.
const MAX_BODY: usize = 4 * 1024 * 1024;

/// The most spellings the resolution map holds, cached and in flight together; a spelling past it is admitted
/// under its folded spelling and probed once room returns. The API lists a dozen ids, so the bound is slack.
pub const RESOLUTION_CAPACITY: usize = 256;
/// The most `GET /v1/models/{spelling}` probes in flight at once.
pub const PROBES_IN_FLIGHT: usize = 4;

const SLOTS_POISONED: &str = "catalog slots lock poisoned by a prior panic";
const RESOLUTION_POISONED: &str = "catalog resolution lock poisoned by a prior panic";

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

/// The last status a spelling's probe ended in. `dated` on `Resolution` is the durable answer; this is only
/// what the most recent attempt reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Probe {
    Dated,
    Missing,
    Failed,
}

#[derive(Clone, Debug)]
struct Resolution {
    /// The last successful answer. Nothing after it clears it: a later 404 or failure only moves `last` and
    /// `retry_at`, never this.
    dated: Option<String>,
    last: Probe,
    at: i64,
    retry_at: i64,
    failures: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Admission {
    Probe,
    Skip,
}

/// What one probe of `GET /v1/models/{spelling}` answered, read into `record`.
enum ProbeAnswer {
    Dated(String),
    Missing,
    Failed,
}

/// The folded-spelling to dated-id map: bounded, coalesced per spelling, and never blocking. Its own unit
/// tests exercise the capacity and backoff rules directly; the relay-level tests exercise it through
/// `CatalogService`.
struct ResolutionMap {
    capacity: usize,
    in_flight: usize,
    window_ms: i64,
    entries: HashMap<String, Resolution>,
    resolving: HashSet<String>,
}

impl ResolutionMap {
    fn new(capacity: usize, in_flight: usize, window_ms: i64) -> ResolutionMap {
        ResolutionMap {
            capacity,
            in_flight,
            window_ms,
            entries: HashMap::new(),
            resolving: HashSet::new(),
        }
    }

    /// Loads a persisted answer as if a probe had just landed it, so a restart answers before any probe runs.
    fn seed(&mut self, spelling: String, dated_id: String, resolved_at: i64) {
        self.entries.insert(
            spelling,
            Resolution {
                dated: Some(dated_id),
                last: Probe::Dated,
                at: resolved_at,
                retry_at: resolved_at + self.window_ms,
                failures: 0,
            },
        );
    }

    fn canonical_for(&self, spelling: &str) -> Option<String> {
        self.entries.get(spelling).and_then(|e| e.dated.clone())
    }

    /// Drops every entry with no dated id past its retry time: the negative side of the map is the only side
    /// that ever expires, since a dated answer is kept whatever a later probe says.
    fn expire_negatives(&mut self, now: i64) {
        self.entries
            .retain(|_, entry| entry.dated.is_some() || entry.retry_at > now);
    }

    fn admit(&mut self, spelling: &str, now: i64) -> Admission {
        if self.resolving.contains(spelling) {
            return Admission::Skip;
        }
        match self.entries.get(spelling) {
            Some(entry) if entry.retry_at > now => return Admission::Skip,
            Some(_) => {}
            None => {
                if self.entries.len() + self.resolving.len() >= self.capacity {
                    self.expire_negatives(now);
                    if self.entries.len() + self.resolving.len() >= self.capacity {
                        return Admission::Skip;
                    }
                }
            }
        }
        if self.resolving.len() >= self.in_flight {
            return Admission::Skip;
        }
        self.resolving.insert(spelling.to_string());
        Admission::Probe
    }

    /// Answers the dated id when this probe is the first to name it, or changes it: the caller reports that
    /// case alone, since a repeat of the same answer moves nothing the routing state does not already hold.
    fn record(&mut self, spelling: &str, outcome: ProbeAnswer, now: i64) -> Option<String> {
        self.resolving.remove(spelling);
        let entry = self
            .entries
            .entry(spelling.to_string())
            .or_insert(Resolution {
                dated: None,
                last: Probe::Missing,
                at: now,
                retry_at: now,
                failures: 0,
            });
        match outcome {
            ProbeAnswer::Dated(id) => {
                let changed = entry.dated.as_deref() != Some(id.as_str());
                entry.dated = Some(id.clone());
                entry.last = Probe::Dated;
                entry.failures = 0;
                entry.at = now;
                entry.retry_at = now + self.window_ms;
                changed.then_some(id)
            }
            ProbeAnswer::Missing => {
                entry.last = Probe::Missing;
                entry.at = now;
                entry.retry_at = now + self.window_ms;
                None
            }
            ProbeAnswer::Failed => {
                entry.last = Probe::Failed;
                entry.failures += 1;
                entry.at = now;
                // One, two, four, then eight windows; the fifth failure and beyond stays at eight.
                let doublings = entry.failures.min(4) - 1;
                entry.retry_at = now + self.window_ms * (1i64 << doublings);
                None
            }
        }
    }
}

pub struct CatalogService {
    clock: Clock,
    manager: Arc<CredentialManager>,
    pool: Arc<Pool>,
    resolver: Resolver,
    resolution: Mutex<ResolutionMap>,
    router: RouterHandle,
    slots: Mutex<HashMap<SlotKey, Slot>>,
    /// Bumped per account id by `evict`, independently of whether a slot yet exists to clear: a fill that
    /// started before an eviction, on a key the eviction never got to insert a slot for, still has to lose.
    generations: Mutex<HashMap<String, u64>>,
    fills: tokio::sync::Mutex<HashMap<SlotKey, Arc<tokio::sync::Mutex<()>>>>,
    store: Arc<Store>,
    timeout: Duration,
    window: Duration,
}

impl CatalogService {
    /// `pool` and `resolver` are the catalog's own transport; `router` carries `Report::Resolved` to the
    /// routing state, and `store.resolutions()` seeds the map so a restart answers before any probe.
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
    ) -> CatalogService {
        let mut resolution = ResolutionMap::new(
            RESOLUTION_CAPACITY,
            PROBES_IN_FLIGHT,
            window.as_millis() as i64,
        );
        for row in store.resolutions().unwrap_or_default() {
            resolution.seed(row.spelling, row.dated_id, row.resolved_at);
        }
        CatalogService {
            clock,
            manager,
            pool,
            resolver,
            resolution: Mutex::new(resolution),
            router,
            slots: Mutex::new(HashMap::new()),
            generations: Mutex::new(HashMap::new()),
            fills: tokio::sync::Mutex::new(HashMap::new()),
            store,
            timeout,
            window,
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
        let headers = self.credential_headers(account, secret).await?;
        let (status, body) = self
            .send(account, MODELS_TARGET.to_string(), headers)
            .await?;
        match status {
            200 => parse_models(&body),
            401 => Err(DiscoveryError::Unauthorized),
            other => Err(DiscoveryError::Status(other)),
        }
    }

    /// The account's entry when younger than the window, else one fill (coalesced per account, and dropped
    /// unstored when `evict` ran under it) whose answer, rows or an error, is stored and returned.
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
        let now = (self.clock)();
        let catalog = match self.discover(account).await {
            Ok(discovered) => Catalog {
                models: discovered.models,
                fetched_at: now,
                error: None,
            },
            Err(error) => Catalog {
                models: Vec::new(),
                fetched_at: now,
                error: Some(error.to_string()),
            },
        };
        // The commit-time fence: a slot read or filled for one credential must never answer for the next, so
        // the row is re-read and both the generation (bumped independently of a slot's existence, so a fill
        // that started before the account even had one still loses) and the holder it names must still match.
        let still_current = self
            .store
            .account(&account.id)
            .ok()
            .flatten()
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
        }
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

    /// The dated id the map last resolved a folded spelling to, or `None`; a later 404 or failure never
    /// clears it, and it never waits.
    pub fn canonical_for(&self, spelling: &str) -> Option<String> {
        self.resolution
            .lock()
            .expect(RESOLUTION_POISONED)
            .canonical_for(spelling)
    }

    /// One background resolution for a spelling the map lacks or holds past its retry time; coalesced per
    /// spelling, capped at `PROBES_IN_FLIGHT`, and skipped without blocking when the map is full.
    pub fn request_resolution(self: &Arc<Self>, spelling: &str) {
        let now = (self.clock)();
        let admission = self
            .resolution
            .lock()
            .expect(RESOLUTION_POISONED)
            .admit(spelling, now);
        if admission != Admission::Probe {
            return;
        }
        let service = Arc::clone(self);
        let spelling = spelling.to_string();
        tokio::spawn(async move {
            let outcome = service.probe_spelling(&spelling).await;
            let now = (service.clock)();
            let resolved = service
                .resolution
                .lock()
                .expect(RESOLUTION_POISONED)
                .record(&spelling, outcome, now);
            if let Some(dated_id) = resolved {
                service.router.report(Report::Resolved {
                    spelling,
                    dated_id,
                    at: now,
                });
            }
        });
    }

    /// Credential resolution shared by the list fetch and a resolution probe. `secret` is only ever set from
    /// the list path (a staged replacement under verification); a probe always reads the account's own.
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

    /// The GET round trip over the catalog's own pool and resolver, shared by the list fetch and a
    /// resolution probe.
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

    /// One `GET /v1/models/{spelling}` through the first account that is enabled, `Ready`, and answers a
    /// credential; a status this fetch does not carry a rule for, and no ready account at all, both fail.
    async fn probe_spelling(&self, spelling: &str) -> ProbeAnswer {
        let accounts = self.store.accounts().unwrap_or_default();
        let target = format!("/v1/models/{}", percent_encode_segment(spelling));
        for account in accounts
            .into_iter()
            .filter(|a| a.enabled && a.state == AccountState::Ready)
        {
            let Ok(headers) = self.credential_headers(&account, None).await else {
                continue;
            };
            return match self.send(&account, target.clone(), headers).await {
                Ok((200, body)) => match parse_id(&body) {
                    Some(id) => ProbeAnswer::Dated(id),
                    None => ProbeAnswer::Failed,
                },
                Ok((404, _)) => ProbeAnswer::Missing,
                Ok(_) | Err(_) => ProbeAnswer::Failed,
            };
        }
        ProbeAnswer::Failed
    }

    fn fresh_slot(&self, key: &SlotKey, now: i64) -> Option<Catalog> {
        let slots = self.slots.lock().expect(SLOTS_POISONED);
        let catalog = slots.get(key)?.cached.as_ref()?;
        (catalog.fetched_at + self.window.as_millis() as i64 > now).then(|| catalog.clone())
    }
}

/// Encodes everything but RFC 3986's unreserved characters, so a spelling naming a slash or another
/// path-carrying byte still names one path segment.
fn percent_encode_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
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

/// A single model object, the shape `GET /v1/models/{id}` answers: only `id` matters to a resolution probe.
fn parse_id(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()?
        .get("id")?
        .as_str()
        .map(str::to_string)
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

    fn map() -> ResolutionMap {
        ResolutionMap::new(4, 2, 100)
    }

    #[test]
    fn the_map_holds_at_most_its_capacity_and_probes_at_most_its_in_flight_count() {
        let mut m = map();
        assert_eq!(m.admit("s1", 0), Admission::Probe);
        assert_eq!(m.admit("s2", 0), Admission::Probe);
        assert_eq!(m.admit("s3", 0), Admission::Skip, "in-flight is at its cap");
        assert_eq!(m.admit("s4", 0), Admission::Skip);
        assert_eq!(m.admit("s5", 0), Admission::Skip);
        // Recording the two in flight frees the in-flight slots, not the capacity: two more may now probe.
        assert_eq!(m.record("s1", ProbeAnswer::Missing, 0), None);
        assert_eq!(m.record("s2", ProbeAnswer::Missing, 1), None);
        assert_eq!(m.admit("s3", 1), Admission::Probe);
        assert_eq!(m.admit("s4", 2), Admission::Probe);
        assert_eq!(m.record("s3", ProbeAnswer::Missing, 2), None);
        assert_eq!(m.record("s4", ProbeAnswer::Missing, 3), None);
        // The map now holds four negative entries with retry times 100, 101, 102, and 103; at 99 none
        // has passed, so a fifth spelling stays skipped rather than start a probe.
        assert_eq!(
            m.admit("s5", 99),
            Admission::Skip,
            "four negative entries still sit inside their own window"
        );
        assert_eq!(m.entries.len() + m.resolving.len(), 4);
        // At 100, s1's retry time alone has passed: exactly one negative entry expires and room opens.
        assert_eq!(
            m.admit("s5", 100),
            Admission::Probe,
            "s1's retry time passed"
        );
        assert_eq!(m.record("s5", ProbeAnswer::Missing, 100), None);
        assert_eq!(m.entries.len(), 4, "the map holds four entries again");
    }

    #[test]
    fn a_failed_probe_backs_off_by_doubling_windows_and_a_success_resets_it() {
        let mut m = map();
        assert_eq!(m.admit("s", 0), Admission::Probe);
        assert_eq!(m.record("s", ProbeAnswer::Failed, 0), None);
        assert_eq!(
            m.admit("s", 99),
            Admission::Skip,
            "one window has not passed"
        );
        assert_eq!(m.admit("s", 100), Admission::Probe, "one window has passed");
        assert_eq!(m.record("s", ProbeAnswer::Failed, 100), None);
        assert_eq!(
            m.admit("s", 299),
            Admission::Skip,
            "two windows have not passed"
        );
        assert_eq!(
            m.admit("s", 300),
            Admission::Probe,
            "two windows have passed"
        );
        assert_eq!(m.record("s", ProbeAnswer::Failed, 300), None);
        assert_eq!(
            m.admit("s", 699),
            Admission::Skip,
            "four windows have not passed"
        );
        assert_eq!(
            m.admit("s", 700),
            Admission::Probe,
            "four windows have passed"
        );
        assert_eq!(m.record("s", ProbeAnswer::Failed, 700), None);
        assert_eq!(
            m.admit("s", 1499),
            Admission::Skip,
            "eight windows have not passed"
        );
        assert_eq!(
            m.admit("s", 1500),
            Admission::Probe,
            "eight windows have passed"
        );
        // A fifth failure stays at eight windows, not sixteen.
        assert_eq!(m.record("s", ProbeAnswer::Failed, 1500), None);
        assert_eq!(m.admit("s", 2299), Admission::Skip, "still eight windows");
        assert_eq!(m.admit("s", 2300), Admission::Probe);
        // A success resets the backoff to one window.
        assert_eq!(
            m.record("s", ProbeAnswer::Dated("claude-s-1".into()), 2300),
            Some("claude-s-1".to_string())
        );
        assert_eq!(
            m.admit("s", 2399),
            Admission::Skip,
            "one window after a success"
        );
        assert_eq!(
            m.admit("s", 2400),
            Admission::Probe,
            "one window has passed again"
        );
    }

    #[test]
    fn a_refresh_answering_404_or_failing_keeps_the_dated_id() {
        let mut m = map();
        assert_eq!(m.canonical_for("s"), None);
        assert_eq!(
            m.record("s", ProbeAnswer::Dated("claude-s-1".into()), 0),
            Some("claude-s-1".to_string())
        );
        assert_eq!(m.canonical_for("s"), Some("claude-s-1".to_string()));
        // The same answer again reports no change: nothing new for the caller to publish.
        assert_eq!(
            m.record("s", ProbeAnswer::Dated("claude-s-1".into()), 100),
            None
        );
        assert_eq!(m.record("s", ProbeAnswer::Missing, 200), None);
        assert_eq!(
            m.canonical_for("s"),
            Some("claude-s-1".to_string()),
            "a 404 never clears a dated id already known"
        );
        assert_eq!(m.record("s", ProbeAnswer::Failed, 300), None);
        assert_eq!(
            m.canonical_for("s"),
            Some("claude-s-1".to_string()),
            "a failed refresh never clears it either"
        );
    }
}
