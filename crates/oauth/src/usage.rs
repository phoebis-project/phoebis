//! Parses the OAuth usage payload and caches it per account, coalescing concurrent fetches.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::{BoxFuture, FutureExt, Shared};
use phoebis_profile::Profile;
use phoebis_store::Store;
use phoebis_transport::control::ControlClient;
use serde_json::Value;
use tokio::sync::Semaphore;

use crate::exchange::{control_request, parse_profile, ControlKind, OauthError, ProfileResponse};
use crate::manager::{retry_after_ms, Clock, CredentialManager};

// A poisoned mutex means a prior panic happened while the cache or in-flight map was being mutated, so the
// bookkeeping can no longer be trusted.
const LOCK_POISONED: &str = "usage service mutex poisoned by a prior panic";

/// A usage response is a small JSON document, so 5 s covers a slow upstream without holding a concurrency
/// slot for long.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// The longest an account waits out the usage endpoint, whether a `Retry-After` asked for more or the ladder
/// reached it: fifteen minutes, the token refresh's own cap. A 429 that keeps coming keeps the account here
/// rather than parking its readings for a day behind one answer.
pub const BACKOFF_CAP_MS: i64 = 15 * 60 * 1_000;

/// The wait a 429 without `Retry-After` earns, by how many 429s the account has answered in a row; past the
/// ladder every further 429 waits `BACKOFF_CAP_MS`. The ladder starts at two minutes: the endpoint's own limit
/// is per account and per minute, so a one-minute retry met the same answer.
const BACKOFF_LADDER_MS: [i64; 3] = [120_000, 240_000, 480_000];

/// The endpoint names a 429's warning and the held answer carry.
const PROFILE_ENDPOINT: &str = "profile";
const USAGE_ENDPOINT: &str = "usage";

/// Top-level keys the 2.1.270 binary and the Management Center source use for the Fable weekly window,
/// read only when `limits[]` is absent from the response.
const FABLE_KEY_ALIASES: &[&str] = &["cinder_cove", "iguana_necktie"];

/// The key a snapshot gives the Fable week, so an answer built from the router's windows reads like one the
/// endpoint gave.
pub const FABLE_WINDOW_KEY: &str = "fable";

/// How an attempt at the usage or the profile endpoint ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PollOutcome {
    Clean,
    Failed,
    RateLimited,
}

/// One account's standing with the endpoint after its last attempt, kept beside the cached snapshot because
/// every caller (the poll, the startup step, the profile refresh, an admin refresh) passes through the service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PollStatus {
    /// The first 429 of the current run of 429s; `None` outside a run.
    pub backoff_since: Option<i64>,
    pub last_attempt_at: i64,
    /// When the windows of the last clean answer were read; `None` until one lands.
    pub last_clean_at: Option<i64>,
    /// The message of the last attempt that was not clean; `None` after a clean one.
    pub last_error: Option<String>,
    pub last_outcome: PollOutcome,
    /// When the account may be asked again; `None` when the next slot may ask it.
    pub next_attempt_at: Option<i64>,
    /// 429s answered in a row, the ladder's step; 0 after a clean answer.
    pub rate_limited_in_a_row: u32,
}

/// One edge of an account's usage backoff, fired where the service moves `CachedEntry.backoff_since`: a run starts
/// on the first 429 of either endpoint (`endpoint` names it) and ends on the next clean usage answer. Called with
/// the cache lock released, after the entry landed, so a hook may take any lock of its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackoffEdge {
    Started {
        account_id: String,
        at: i64,
        endpoint: &'static str,
        /// The status or the error class that opened the run.
        reason: String,
        attempts: u32,
        next_attempt_at: i64,
        /// When the last clean usage answer before the run was read, or `None` when this process read no clean answer.
        last_clean_at: Option<i64>,
    },
    Ended {
        account_id: String,
        at: i64,
        since: i64,
        attempts: u32,
        resumed_at: i64,
    },
}

pub type BackoffHook = Arc<dyn Fn(BackoffEdge) + Send + Sync>;

#[derive(Clone, Debug)]
pub struct UsageWindow {
    pub key: String,
    pub utilization: Option<f64>,
    pub resets_at: Option<i64>,
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct ExtraUsage {
    pub is_enabled: bool,
    pub monthly_limit: Option<i64>,
    pub used_credits: Option<i64>,
    pub utilization: Option<f64>,
    pub currency: Option<String>,
    pub disabled_reason: Option<String>,
}

#[derive(Clone, Debug)]
pub struct UsageSnapshot {
    pub account_id: String,
    pub identity_generation: u64,
    pub fetched_at: i64,
    pub windows: Vec<UsageWindow>,
    pub fable_window: Option<UsageWindow>,
    pub extra_usage: Option<ExtraUsage>,
    pub raw_keys: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum UsageError {
    #[error("{0}")]
    Unavailable(String),
}

/// Reads every top-level object with a `utilization` member as a window, keeping its key. `extra_usage`
/// carries a `utilization` of its own but describes purchased credit rather than a quota window, so it is
/// parsed separately. The Fable weekly window comes from `limits[]` when present; a top-level key is read
/// only as a fallback for the wire shapes captured before that field existed.
#[allow(clippy::type_complexity)]
pub fn parse_usage(
    body: &[u8],
) -> Result<
    (
        Vec<UsageWindow>,
        Option<UsageWindow>,
        Option<ExtraUsage>,
        Vec<String>,
    ),
    OauthError,
> {
    let root: Value = serde_json::from_slice(body)?;
    let map = root
        .as_object()
        .ok_or_else(|| OauthError::Malformed("usage response is not an object".into()))?;

    let raw_keys: Vec<String> = map.keys().cloned().collect();

    let mut windows: Vec<UsageWindow> = map
        .iter()
        .filter(|(key, _)| key.as_str() != "extra_usage" && key.as_str() != "limits")
        .filter_map(|(key, value)| window_from_value(key, value))
        .collect();

    let extra_usage = match map.get("extra_usage") {
        Some(Value::Null) | None => None,
        Some(value) => Some(serde_json::from_value(value.clone())?),
    };

    let fable_window = match map.get("limits") {
        Some(limits) => {
            let scoped = fable_from_limits(limits);
            if scoped.is_some() {
                // The alias is the same quota under the name an older response used, so leaving it among the
                // windows would show the operator one limit twice.
                windows.retain(|window| !FABLE_KEY_ALIASES.contains(&window.key.as_str()));
            }
            scoped
        }
        None => {
            let alias = windows
                .iter()
                .position(|w| FABLE_KEY_ALIASES.contains(&w.key.as_str()));
            alias.map(|index| windows.remove(index))
        }
    };

    Ok((windows, fable_window, extra_usage, raw_keys))
}

fn window_from_value(key: &str, value: &Value) -> Option<UsageWindow> {
    let object = value.as_object()?;
    object.get("utilization")?;
    Some(UsageWindow {
        key: key.to_string(),
        utilization: object.get("utilization").and_then(Value::as_f64),
        resets_at: object.get("resets_at").and_then(parse_resets_at),
    })
}

fn fable_from_limits(limits: &Value) -> Option<UsageWindow> {
    let entry = limits.as_array()?.iter().find(|item| {
        item.get("kind").and_then(Value::as_str) == Some("weekly_scoped")
            && item
                .get("scope")
                .and_then(|scope| scope.get("model"))
                .and_then(|model| model.get("display_name"))
                .and_then(Value::as_str)
                .is_some_and(|name| name.starts_with("Fable"))
    })?;
    Some(UsageWindow {
        key: FABLE_WINDOW_KEY.to_string(),
        utilization: entry.get("percent").and_then(Value::as_f64),
        resets_at: entry.get("resets_at").and_then(parse_resets_at),
    })
}

fn parse_resets_at(value: &Value) -> Option<i64> {
    value
        .as_str()?
        .parse::<jiff::Timestamp>()
        .ok()
        .map(|t| t.as_millisecond())
}

struct ParsedUsage {
    windows: Vec<UsageWindow>,
    fable_window: Option<UsageWindow>,
    extra_usage: Option<ExtraUsage>,
    raw_keys: Vec<String>,
}

/// Why a fetch did not produce a usable payload. Kept apart from `UsageError`, which surfaces only when the
/// fetch task itself panicked; an ordinary upstream failure always yields a snapshot instead, empty on a
/// first-ever fetch, so the not-before that gates the next attempt is never lost for want of one to attach to.
enum FetchFailure {
    Message(String),
    /// The instant the 429 arrived, which the hold runs from, and the `Retry-After` it asked for.
    RateLimited {
        received_at: i64,
        retry_after: Option<i64>,
    },
}

/// Whose reading a fetch or a cache entry is. A deleted id created again draws a new incarnation but starts
/// over at lifecycle generation 1, so the generation alone would take the old holder's answer for the new one's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Identity {
    incarnation: u64,
    lifecycle_generation: u64,
}

/// The snapshot cached for a first-ever fetch that failed: no windows to fall back on, but a real snapshot
/// all the same, so its `not_before` survives in the same cache entry instead of being discarded for lack
/// of a prior one to update.
fn empty_snapshot(
    account_id: &str,
    generation: Option<u64>,
    fetched_at: i64,
    error: String,
) -> UsageSnapshot {
    UsageSnapshot {
        account_id: account_id.to_string(),
        identity_generation: generation.unwrap_or_default(),
        fetched_at,
        windows: Vec::new(),
        fable_window: None,
        extra_usage: None,
        raw_keys: Vec::new(),
        error: Some(error),
    }
}

struct CachedEntry {
    fetched_at: i64,
    identity: Identity,
    /// Set past `fetched_at` only by a 429, to the ladder's step or the capped `Retry-After`; any other answer
    /// leaves it equal to `fetched_at` so the ordinary cache TTL governs the next retry.
    not_before: i64,
    snapshot: UsageSnapshot,
    /// The endpoint whose 429 armed the hold, named in the held answer.
    held_by: &'static str,
    backoff_since: Option<i64>,
    last_clean_at: Option<i64>,
    /// The message of the last attempt that was not clean; `None` after a clean one.
    last_error: Option<String>,
    last_outcome: PollOutcome,
    rate_limited_in_a_row: u32,
}

impl CachedEntry {
    /// Counts a 429 against the account: the ladder steps, a run starts unless one is under way, and the hold
    /// becomes the `Retry-After` asked for or else the ladder's step, never shorter than one already armed.
    /// Answers the run's start when this 429 opened it.
    fn rate_limited(
        &mut self,
        account_id: &str,
        endpoint: &'static str,
        at: i64,
        retry_after: Option<i64>,
    ) -> Option<BackoffEdge> {
        let opens_run = self.backoff_since.is_none();
        self.rate_limited_in_a_row = self.rate_limited_in_a_row.saturating_add(1);
        self.backoff_since.get_or_insert(at);
        self.last_outcome = PollOutcome::RateLimited;
        self.held_by = endpoint;
        let wait = retry_after.unwrap_or_else(|| ladder_step(self.rate_limited_in_a_row));
        self.not_before = self.not_before.max(at.saturating_add(wait));
        self.last_error = Some(self.hold_message());
        tracing::warn!(
            account = %account_id,
            next_attempt_at = %utc(self.not_before),
            retry_after_ms = ?retry_after,
            "the {endpoint} endpoint rate-limited the account"
        );
        opens_run.then(|| BackoffEdge::Started {
            account_id: account_id.to_string(),
            at,
            endpoint,
            // A run opens on a 429 alone, so the status is its reason. The hold's sentence is the snapshot's.
            reason: "429".into(),
            attempts: self.rate_limited_in_a_row,
            next_attempt_at: self.not_before,
            last_clean_at: self.last_clean_at,
        })
    }

    /// The cached snapshot marked as no new reading, for a caller asking while the account waits out its hold:
    /// the poll and a manual refresh must not record the cached windows as freshly read.
    fn held_answer(&self) -> UsageSnapshot {
        UsageSnapshot {
            error: Some(self.hold_message()),
            ..self.snapshot.clone()
        }
    }

    /// The one sentence the held answer and the poll's last error both read.
    fn hold_message(&self) -> String {
        format!(
            "the {} endpoint rate-limited the account, and the next attempt is at {}",
            self.held_by,
            utc(self.not_before)
        )
    }
}

/// `at_ms` as the UTC time the console shows, or the raw milliseconds for one out of range.
fn utc(at_ms: i64) -> String {
    jiff::Timestamp::from_millisecond(at_ms)
        .map(|at| at.strftime("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|_| at_ms.to_string())
}

/// The ladder's wait for the `in_a_row`-th 429 in a row, counted from 1.
fn ladder_step(in_a_row: u32) -> i64 {
    (in_a_row as usize)
        .checked_sub(1)
        .and_then(|index| BACKOFF_LADDER_MS.get(index))
        .copied()
        .unwrap_or(BACKOFF_CAP_MS)
}

type FetchOutcome = Result<UsageSnapshot, String>;
type SharedFetch = Shared<BoxFuture<'static, FetchOutcome>>;

struct Inner {
    cache: Mutex<HashMap<String, CachedEntry>>,
    inflight: Mutex<HashMap<String, SharedFetch>>,
    on_backoff: Mutex<Option<BackoffHook>>,
    semaphore: Semaphore,
    manager: Arc<CredentialManager>,
    control: Arc<ControlClient>,
    profile: &'static Profile,
    store: Arc<Store>,
    cache_ttl: Duration,
    clock: Clock,
}

/// Removes an account's in-flight entry once its fetch task ends, however it ends, so a panic never leaves
/// every later caller joined to a future that has already resolved.
struct InflightGuard {
    account_id: String,
    inner: Arc<Inner>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if let Ok(mut inflight) = self.inner.inflight.lock() {
            inflight.remove(&self.account_id);
        }
    }
}

pub struct UsageService {
    inner: Arc<Inner>,
}

impl UsageService {
    pub fn new(
        manager: Arc<CredentialManager>,
        control: Arc<ControlClient>,
        profile: &'static Profile,
        cache_ttl: Duration,
        concurrency: usize,
        clock: Clock,
    ) -> Self {
        let store = manager.store().clone();
        UsageService {
            inner: Arc::new(Inner {
                cache: Mutex::new(HashMap::new()),
                inflight: Mutex::new(HashMap::new()),
                on_backoff: Mutex::new(None),
                semaphore: Semaphore::new(concurrency),
                manager,
                control,
                profile,
                store,
                cache_ttl,
                clock,
            }),
        }
    }

    /// Serves the cached snapshot while it is within `cache_ttl`, with no bypass, and coalesces concurrent
    /// misses for one account into a single fetch. The scheduled poll and the admin refresh read through `poll`
    /// instead, under the same hold and coalescing, so a held-down refresh button cannot turn into a stream of
    /// requests against an endpoint that rate-limits per account.
    pub async fn snapshot(&self, account_id: &str) -> Result<UsageSnapshot, UsageError> {
        let now = (self.inner.clock)();
        if let Some(snapshot) = self.inner.fresh(account_id, now) {
            return Ok(snapshot);
        }
        self.join_or_start(account_id).await
    }

    /// The scheduled poll's read: the endpoint regardless of the cache TTL, under the same hold, joined to a
    /// fetch already out and written to the same cache, so the cadence never waits on `cache_ttl` and a poll
    /// beside a manual refresh makes one request; a held account answers its last snapshot with the error set.
    pub async fn poll(&self, account_id: &str) -> Result<UsageSnapshot, UsageError> {
        let now = (self.inner.clock)();
        if let Some(held) = self.inner.held(account_id, now) {
            return Ok(held);
        }
        self.join_or_start(account_id).await
    }

    /// How much longer the account waits out its backoff at the service's clock; `None` when it may be asked now.
    pub fn held_for(&self, account_id: &str) -> Option<Duration> {
        let now = (self.inner.clock)();
        let not_before = self.inner.next_attempt_at(account_id, now)?;
        Some(Duration::from_millis((not_before - now) as u64))
    }

    /// The last snapshot held for the account, clean or carrying its last attempt's error, with no fetch;
    /// `None` for an entry another holder of the id left behind (a re-pairing or a delete and a new account
    /// under the id that no route invalidated), so no caller shows the old holder's windows as the new one's.
    pub fn latest(&self, account_id: &str) -> Option<UsageSnapshot> {
        let identity = self.inner.identity(account_id);
        self.inner
            .cache
            .lock()
            .expect(LOCK_POISONED)
            .get(account_id)
            .filter(|entry| identity == Some(entry.identity))
            .map(|entry| entry.snapshot.clone())
    }

    /// The account's standing after its last attempt; `None` for an account not asked since the process
    /// started or since `invalidate` cleared it.
    pub fn poll_status(&self, account_id: &str) -> Option<PollStatus> {
        let now = (self.inner.clock)();
        let cache = self.inner.cache.lock().expect(LOCK_POISONED);
        let entry = cache.get(account_id)?;
        Some(PollStatus {
            backoff_since: entry.backoff_since,
            last_attempt_at: entry.fetched_at,
            last_clean_at: entry.last_clean_at,
            last_error: entry.last_error.clone(),
            last_outcome: entry.last_outcome,
            next_attempt_at: (now < entry.not_before).then_some(entry.not_before),
            rate_limited_in_a_row: entry.rate_limited_in_a_row,
        })
    }

    /// Replaces the hook every backoff edge is handed to. No edge reaches a hook before one is set.
    pub fn set_on_backoff(&self, hook: BackoffHook) {
        *self.inner.on_backoff.lock().expect(LOCK_POISONED) = Some(hook);
    }

    pub fn invalidate(&self, account_id: &str) {
        self.inner
            .cache
            .lock()
            .expect(LOCK_POISONED)
            .remove(account_id);
    }

    async fn join_or_start(&self, account_id: &str) -> Result<UsageSnapshot, UsageError> {
        let shared = {
            let mut inflight = self.inner.inflight.lock().expect(LOCK_POISONED);
            match inflight.get(account_id) {
                Some(existing) => existing.clone(),
                None => {
                    let inner = self.inner.clone();
                    let id = account_id.to_string();
                    // Spawned so a caller that goes away mid-fetch does not stop the fetch other callers are
                    // joined to; the guard's map lock is held across the spawn so a fetch finishing at once
                    // cannot remove the entry before it is inserted.
                    let handle = tokio::spawn(async move {
                        let guard = InflightGuard {
                            account_id: id,
                            inner,
                        };
                        guard.inner.fetch_and_cache(&guard.account_id).await
                    });
                    let shared = async move {
                        match handle.await {
                            Ok(outcome) => outcome,
                            Err(_) => Err("usage fetch panicked".to_string()),
                        }
                    }
                    .boxed()
                    .shared();
                    inflight.insert(account_id.to_string(), shared.clone());
                    shared
                }
            }
        };
        shared.await.map_err(UsageError::Unavailable)
    }

    /// The profile document, fetched now: the usage cache's `not_before` gates it and a 429 here counts against
    /// that gate, so the two endpoints share one backoff per account.
    pub async fn profile(&self, account_id: &str) -> Result<ProfileResponse, UsageError> {
        let now = (self.inner.clock)();
        if let Some(entry) = self
            .inner
            .cache
            .lock()
            .expect(LOCK_POISONED)
            .get(account_id)
        {
            if now < entry.not_before {
                return Err(UsageError::Unavailable(
                    "the account is waiting out a rate limit".into(),
                ));
            }
        }
        let token = self
            .inner
            .manager
            .acquire(account_id)
            .await
            .map_err(|error| UsageError::Unavailable(format!("acquire: {error}")))?;
        let permit = self
            .inner
            .semaphore
            .acquire()
            .await
            .expect("usage semaphore is never closed");
        let req = control_request(
            self.inner.profile,
            ControlKind::Profile,
            Some(token.secret.expose()),
            None,
        );
        let host = ControlKind::Profile.host(self.inner.profile);
        let sent = self.inner.control.send(req, &host, FETCH_TIMEOUT).await;
        drop(permit);
        let (head, body) =
            sent.map_err(|error| UsageError::Unavailable(format!("transport: {error}")))?;
        if head.status == 429 {
            // The hold runs from the answer's arrival, as a usage 429's does; `now` predates the credential and
            // permit waits.
            let received_at = (self.inner.clock)();
            self.inner.rate_limited(
                account_id,
                PROFILE_ENDPOINT,
                received_at,
                retry_after_ms(&head, received_at, BACKOFF_CAP_MS),
            );
            return Err(UsageError::Unavailable(
                "profile request rate limited".into(),
            ));
        }
        if head.status != 200 {
            return Err(UsageError::Unavailable(format!(
                "profile request rejected with status {}",
                head.status
            )));
        }
        parse_profile(&body).map_err(|error| UsageError::Unavailable(format!("parse: {error}")))
    }
}

impl Inner {
    fn fresh(&self, account_id: &str, now: i64) -> Option<UsageSnapshot> {
        let (fetched_at, identity, snapshot) = {
            let cache = self.cache.lock().expect(LOCK_POISONED);
            let entry = cache.get(account_id)?;
            // A hit inside a hold answers the held snapshot, as a miss inside one does: neither is a new reading.
            let answer = if now < entry.not_before {
                entry.held_answer()
            } else {
                entry.snapshot.clone()
            };
            (entry.fetched_at, entry.identity, answer)
        };
        if now - fetched_at >= self.cache_ttl.as_millis() as i64 {
            return None;
        }
        // A disconnect and re-pairing, or a delete and a new account under the id, inside the TTL window must
        // not serve the old identity's cached windows as if they belonged to the one that replaced it.
        if self.identity(account_id) != Some(identity) {
            return None;
        }
        Some(snapshot)
    }

    fn identity(&self, account_id: &str) -> Option<Identity> {
        self.store
            .account(account_id)
            .ok()
            .flatten()
            .map(|account| Identity {
                incarnation: account.incarnation,
                lifecycle_generation: account.lifecycle_generation,
            })
    }

    /// The held answer while the account waits out its hold at `now`.
    fn held(&self, account_id: &str, now: i64) -> Option<UsageSnapshot> {
        let cache = self.cache.lock().expect(LOCK_POISONED);
        let entry = cache.get(account_id)?;
        (now < entry.not_before).then(|| entry.held_answer())
    }

    fn next_attempt_at(&self, account_id: &str, now: i64) -> Option<i64> {
        let cache = self.cache.lock().expect(LOCK_POISONED);
        let entry = cache.get(account_id)?;
        (now < entry.not_before).then_some(entry.not_before)
    }

    async fn fetch_and_cache(&self, account_id: &str) -> FetchOutcome {
        let now = (self.clock)();
        if let Some(held) = self.held(account_id, now) {
            return Ok(held);
        }
        let started = self.identity(account_id);
        let (read_at, result) = self.fetch(account_id).await;
        let ended = self.identity(account_id);
        self.commit(account_id, read_at, started != ended, ended, result)
    }

    /// The attempt's instant with its result: the send's, read once the credential and the concurrency permit
    /// are held, since a request that completes during those waits is already in what the endpoint answers and
    /// the router drops a charge only up to the reading's instant; the failure's own when no request went out.
    async fn fetch(&self, account_id: &str) -> (i64, Result<ParsedUsage, FetchFailure>) {
        let token = match self.manager.acquire(account_id).await {
            Ok(token) => token,
            Err(error) => {
                return (
                    (self.clock)(),
                    Err(FetchFailure::Message(format!("acquire: {error}"))),
                )
            }
        };
        let permit = self
            .semaphore
            .acquire()
            .await
            .expect("usage semaphore is never closed");
        let sent_at = (self.clock)();
        (
            sent_at,
            self.request_usage(token.secret.expose(), permit).await,
        )
    }

    async fn request_usage(
        &self,
        bearer: &str,
        permit: tokio::sync::SemaphorePermit<'_>,
    ) -> Result<ParsedUsage, FetchFailure> {
        let req = control_request(self.profile, ControlKind::Usage, Some(bearer), None);
        let host = ControlKind::Usage.host(self.profile);
        let sent = self.control.send(req, &host, FETCH_TIMEOUT).await;
        drop(permit);
        let (head, body) =
            sent.map_err(|error| FetchFailure::Message(format!("transport: {error}")))?;
        if head.status == 429 {
            let received_at = (self.clock)();
            return Err(FetchFailure::RateLimited {
                received_at,
                retry_after: retry_after_ms(&head, received_at, BACKOFF_CAP_MS),
            });
        }
        if head.status != 200 {
            return Err(FetchFailure::Message(format!(
                "usage request rejected with status {}",
                head.status
            )));
        }
        let (windows, fable_window, extra_usage, raw_keys) =
            parse_usage(&body).map_err(|error| FetchFailure::Message(format!("parse: {error}")))?;
        Ok(ParsedUsage {
            windows,
            fable_window,
            extra_usage,
            raw_keys,
        })
    }

    /// A result whose identity moved between the fetch's start and its completion belongs to a holder that has
    /// already been replaced (a disconnect, a new pairing, or a delete and a new account under the id), so it
    /// is discarded instead of cached; the caller gets whatever was cached before, unchanged.
    fn commit(
        &self,
        account_id: &str,
        fetched_at: i64,
        identity_changed: bool,
        ended: Option<Identity>,
        result: Result<ParsedUsage, FetchFailure>,
    ) -> FetchOutcome {
        let (outcome, edge) = {
            let mut cache = self.cache.lock().expect(LOCK_POISONED);
            let (outcome, edge) = self.commit_in(
                &mut cache,
                account_id,
                fetched_at,
                identity_changed,
                ended,
                result,
            );
            (outcome, self.backoff_hook(edge))
        };
        announce(edge);
        outcome
    }

    fn backoff_hook(&self, edge: Option<BackoffEdge>) -> Option<(BackoffHook, BackoffEdge)> {
        let edge = edge?;
        let hook = self.on_backoff.lock().expect(LOCK_POISONED).clone()?;
        Some((hook, edge))
    }

    fn commit_in(
        &self,
        cache: &mut HashMap<String, CachedEntry>,
        account_id: &str,
        fetched_at: i64,
        identity_changed: bool,
        ended: Option<Identity>,
        result: Result<ParsedUsage, FetchFailure>,
    ) -> (FetchOutcome, Option<BackoffEdge>) {
        if identity_changed {
            // A hold armed while this fetch was out still marks the answer, as on every other path; an entry an
            // earlier holder of the id left behind is nobody's answer.
            let now = (self.clock)();
            let outcome = match cache.get(account_id) {
                Some(entry) if Some(entry.identity) != ended => {
                    Err("account identity changed while fetching usage".to_string())
                }
                Some(entry) if now < entry.not_before => Ok(entry.held_answer()),
                Some(entry) => Ok(entry.snapshot.clone()),
                None => Err("account identity changed while fetching usage".to_string()),
            };
            return (outcome, None);
        }
        // A hold armed while this fetch was out belongs to the account rather than to the snapshot it is
        // stored beside, so replacing the entry never shortens it.
        let (held, held_by) = cache
            .get(account_id)
            .map(|entry| (entry.not_before, entry.held_by))
            .unwrap_or((i64::MIN, USAGE_ENDPOINT));
        match result {
            Ok(parsed) => {
                let snapshot = UsageSnapshot {
                    account_id: account_id.to_string(),
                    identity_generation: ended.unwrap_or_default().lifecycle_generation,
                    fetched_at,
                    windows: parsed.windows,
                    fable_window: parsed.fable_window,
                    extra_usage: parsed.extra_usage,
                    raw_keys: parsed.raw_keys,
                    error: None,
                };
                let edge = cache.get(account_id).and_then(|entry| {
                    entry.backoff_since.map(|since| BackoffEdge::Ended {
                        account_id: account_id.to_string(),
                        at: fetched_at,
                        since,
                        attempts: entry.rate_limited_in_a_row,
                        resumed_at: fetched_at,
                    })
                });
                cache.insert(
                    account_id.to_string(),
                    CachedEntry {
                        fetched_at,
                        identity: ended.unwrap_or_default(),
                        not_before: fetched_at.max(held),
                        snapshot: snapshot.clone(),
                        held_by,
                        backoff_since: None,
                        last_clean_at: Some(fetched_at),
                        last_error: None,
                        last_outcome: PollOutcome::Clean,
                        rate_limited_in_a_row: 0,
                    },
                );
                (Ok(snapshot), edge)
            }
            Err(failure) => {
                let previous = cache.get(account_id);
                let (backoff_since, last_clean_at, rate_limited_in_a_row, identity) = previous
                    .map(|entry| {
                        (
                            entry.backoff_since,
                            entry.last_clean_at,
                            entry.rate_limited_in_a_row,
                            entry.identity,
                        )
                    })
                    .unwrap_or((None, None, 0, ended.unwrap_or_default()));
                let previous = previous.map(|entry| entry.snapshot.clone());
                let (message, rate_limited) = match failure {
                    FetchFailure::Message(message) => (message, None),
                    FetchFailure::RateLimited {
                        received_at,
                        retry_after,
                    } => (
                        "usage request rate-limited".to_string(),
                        Some((received_at, retry_after)),
                    ),
                };
                let last_error = Some(message.clone());
                let snapshot = match previous {
                    // The snapshot's timestamp says when its windows were read, and a failed attempt read
                    // none, so it keeps the one from the last success rather than passing stale quota off as
                    // freshly fetched. The cache entry's own timestamp is this attempt's, and that is what
                    // the TTL and the not-before are measured from.
                    Some(previous) => UsageSnapshot {
                        error: Some(message),
                        ..previous
                    },
                    None => empty_snapshot(
                        account_id,
                        ended.map(|identity| identity.lifecycle_generation),
                        fetched_at,
                        message,
                    ),
                };
                let mut entry = CachedEntry {
                    fetched_at,
                    identity,
                    not_before: fetched_at.max(held),
                    snapshot: snapshot.clone(),
                    held_by,
                    backoff_since,
                    last_clean_at,
                    last_error,
                    last_outcome: PollOutcome::Failed,
                    rate_limited_in_a_row,
                };
                let edge = rate_limited.and_then(|(received_at, retry_after)| {
                    entry.rate_limited(account_id, USAGE_ENDPOINT, received_at, retry_after)
                });
                cache.insert(account_id.to_string(), entry);
                (Ok(snapshot), edge)
            }
        }
    }

    /// Counts a profile 429 against the account without touching the snapshot, so a usage fetch after this
    /// waits as a usage 429 would have made it wait.
    fn rate_limited(
        &self,
        account_id: &str,
        endpoint: &'static str,
        at: i64,
        retry_after: Option<i64>,
    ) {
        let edge = {
            let mut cache = self.cache.lock().expect(LOCK_POISONED);
            let edge = self.rate_limited_in(&mut cache, account_id, endpoint, at, retry_after);
            self.backoff_hook(edge)
        };
        announce(edge);
    }

    fn rate_limited_in(
        &self,
        cache: &mut HashMap<String, CachedEntry>,
        account_id: &str,
        endpoint: &'static str,
        at: i64,
        retry_after: Option<i64>,
    ) -> Option<BackoffEdge> {
        match cache.get_mut(account_id) {
            Some(entry) => entry.rate_limited(account_id, endpoint, at, retry_after),
            None => {
                let identity = self.identity(account_id).unwrap_or_default();
                let mut entry = CachedEntry {
                    fetched_at: at,
                    identity,
                    not_before: at,
                    snapshot: empty_snapshot(
                        account_id,
                        Some(identity.lifecycle_generation),
                        at,
                        "no usage fetched yet".into(),
                    ),
                    held_by: endpoint,
                    backoff_since: None,
                    last_clean_at: None,
                    last_error: None,
                    last_outcome: PollOutcome::RateLimited,
                    rate_limited_in_a_row: 0,
                };
                let edge = entry.rate_limited(account_id, endpoint, at, retry_after);
                cache.insert(account_id.to_string(), entry);
                edge
            }
        }
    }
}

/// Hands an edge to its hook. The caller has released the cache lock.
fn announce(edge: Option<(BackoffHook, BackoffEdge)>) {
    if let Some((hook, edge)) = edge {
        hook(edge);
    }
}
