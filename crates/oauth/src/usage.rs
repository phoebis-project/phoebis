//! Parses the OAuth usage payload and caches it per account, coalescing concurrent fetches.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use phoebis_profile::Profile;
use phoebis_store::Store;
use phoebis_transport::control::ControlClient;
use futures::future::{BoxFuture, FutureExt, Shared};
use serde_json::Value;
use tokio::sync::Semaphore;

use crate::exchange::{control_request, ControlKind, OauthError};
use crate::manager::{retry_after_ms, Clock, CredentialManager};

// A poisoned mutex means a prior panic happened while the cache or in-flight map was being mutated, so the
// bookkeeping can no longer be trusted.
const LOCK_POISONED: &str = "usage service mutex poisoned by a prior panic";

/// A usage response is a small JSON document, so 5 s covers a slow upstream without holding a concurrency
/// slot for long.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Top-level keys the 2.1.270 binary and the Management Center source use for the Fable weekly window,
/// read only when `limits[]` is absent from the response.
const FABLE_KEY_ALIASES: &[&str] = &["cinder_cove", "iguana_necktie"];

const FABLE_WINDOW_KEY: &str = "fable";

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
        Some(limits) => fable_from_limits(limits),
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
    RateLimited(Option<i64>),
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
    identity_generation: u64,
    /// Set past `fetched_at` only by a 429's `Retry-After`; a plain failure leaves it equal to `fetched_at`
    /// so the ordinary cache TTL governs the next retry.
    not_before: i64,
    snapshot: UsageSnapshot,
}

type FetchOutcome = Result<UsageSnapshot, String>;
type SharedFetch = Shared<BoxFuture<'static, FetchOutcome>>;

struct Inner {
    cache: Mutex<HashMap<String, CachedEntry>>,
    inflight: Mutex<HashMap<String, SharedFetch>>,
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

    /// Serves the cached snapshot while it is within `cache_ttl`, with no bypass: an admin refresh reaches
    /// this same path, and a held-down button must not turn into a stream of requests against an endpoint
    /// that rate-limits per account. Concurrent misses for one account coalesce into a single fetch.
    pub async fn snapshot(&self, account_id: &str) -> Result<UsageSnapshot, UsageError> {
        let now = (self.inner.clock)();
        if let Some(snapshot) = self.inner.fresh(account_id, now) {
            return Ok(snapshot);
        }
        self.join_or_start(account_id).await
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
}

impl Inner {
    fn fresh(&self, account_id: &str, now: i64) -> Option<UsageSnapshot> {
        let (fetched_at, identity_generation, snapshot) = {
            let cache = self.cache.lock().expect(LOCK_POISONED);
            let entry = cache.get(account_id)?;
            (
                entry.fetched_at,
                entry.identity_generation,
                entry.snapshot.clone(),
            )
        };
        if now - fetched_at >= self.cache_ttl.as_millis() as i64 {
            return None;
        }
        // A disconnect and re-pairing inside the TTL window must not serve the old identity's cached
        // windows as if they belonged to the one that replaced it.
        if self.lifecycle_generation(account_id) != Some(identity_generation) {
            return None;
        }
        Some(snapshot)
    }

    fn lifecycle_generation(&self, account_id: &str) -> Option<u64> {
        self.store
            .account(account_id)
            .ok()
            .flatten()
            .map(|account| account.lifecycle_generation)
    }

    async fn fetch_and_cache(&self, account_id: &str) -> FetchOutcome {
        let now = (self.clock)();
        if let Some(entry) = self.cache.lock().expect(LOCK_POISONED).get(account_id) {
            if now < entry.not_before {
                return Ok(entry.snapshot.clone());
            }
        }
        let started_generation = self.lifecycle_generation(account_id);
        let result = self.fetch(account_id).await;
        let ended_generation = self.lifecycle_generation(account_id);
        self.commit(
            account_id,
            now,
            started_generation != ended_generation,
            ended_generation,
            result,
        )
    }

    async fn fetch(&self, account_id: &str) -> Result<ParsedUsage, FetchFailure> {
        let token = self
            .manager
            .acquire(account_id)
            .await
            .map_err(|error| FetchFailure::Message(format!("acquire: {error}")))?;
        let permit = self
            .semaphore
            .acquire()
            .await
            .expect("usage semaphore is never closed");
        let req = control_request(
            self.profile,
            ControlKind::Usage,
            Some(token.secret.expose()),
            None,
        );
        let host = ControlKind::Usage.host(self.profile);
        let sent = self.control.send(req, &host, FETCH_TIMEOUT).await;
        drop(permit);
        let (head, body) =
            sent.map_err(|error| FetchFailure::Message(format!("transport: {error}")))?;
        if head.status == 429 {
            return Err(FetchFailure::RateLimited(retry_after_ms(
                &head,
                (self.clock)(),
            )));
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

    /// A result whose lifecycle generation moved between the fetch's start and its completion belongs to an
    /// identity that has already been replaced (a disconnect or a new pairing), so it is discarded instead
    /// of cached; the caller gets whatever was cached before, unchanged.
    fn commit(
        &self,
        account_id: &str,
        fetched_at: i64,
        generation_changed: bool,
        ended_generation: Option<u64>,
        result: Result<ParsedUsage, FetchFailure>,
    ) -> FetchOutcome {
        let mut cache = self.cache.lock().expect(LOCK_POISONED);
        if generation_changed {
            return match cache.get(account_id) {
                Some(entry) => Ok(entry.snapshot.clone()),
                None => Err("account identity changed while fetching usage".to_string()),
            };
        }
        match result {
            Ok(parsed) => {
                let snapshot = UsageSnapshot {
                    account_id: account_id.to_string(),
                    identity_generation: ended_generation.unwrap_or_default(),
                    fetched_at,
                    windows: parsed.windows,
                    fable_window: parsed.fable_window,
                    extra_usage: parsed.extra_usage,
                    raw_keys: parsed.raw_keys,
                    error: None,
                };
                cache.insert(
                    account_id.to_string(),
                    CachedEntry {
                        fetched_at,
                        identity_generation: snapshot.identity_generation,
                        not_before: fetched_at,
                        snapshot: snapshot.clone(),
                    },
                );
                Ok(snapshot)
            }
            Err(failure) => {
                let previous = cache.get(account_id).map(|entry| entry.snapshot.clone());
                let (message, not_before) = match failure {
                    FetchFailure::Message(message) => (message, fetched_at),
                    FetchFailure::RateLimited(delay) => (
                        "usage request rate-limited".to_string(),
                        fetched_at.saturating_add(delay.unwrap_or(0)),
                    ),
                };
                let snapshot = match previous {
                    Some(previous) => UsageSnapshot {
                        error: Some(message),
                        fetched_at,
                        ..previous
                    },
                    None => empty_snapshot(account_id, ended_generation, fetched_at, message),
                };
                cache.insert(
                    account_id.to_string(),
                    CachedEntry {
                        fetched_at,
                        identity_generation: snapshot.identity_generation,
                        not_before,
                        snapshot: snapshot.clone(),
                    },
                );
                Ok(snapshot)
            }
        }
    }
}
