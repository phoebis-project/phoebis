//! Hands out access tokens, refreshing at most one per account at a time.

use std::collections::HashMap;
#[cfg(any(test, feature = "debug-hooks"))]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use phoebis_profile::Profile;
use phoebis_store::{
    AccountRow, AccountState, AttemptOutcome, CredentialBundle, NewBundle, Secret, Store,
    StoreError,
};
use phoebis_transport::control::ControlClient;
use phoebis_transport::response::ResponseHead;
use phoebis_transport::TransportError;
use futures::future::{BoxFuture, FutureExt, Shared};
use serde_json::Value;

use crate::exchange::{control_request, parse_token_response, refresh_body, ControlKind};
use crate::identity::{identity_matches, IdentityClaims};

// A poisoned manager mutex means a prior panic happened while the refresh map was being mutated, so the
// in-flight bookkeeping can no longer be trusted.
const LOCK_POISONED: &str = "credential manager mutex poisoned by a prior panic";

/// The most of a token's remaining lifetime the manager spends refreshing ahead of expiry.
const MAX_MARGIN_MS: i64 = 300_000;

/// Backoff after a refusal that cannot have rotated anything. Short on purpose: the token it protects may
/// be a minute from expiring, and a long wait would turn a brief refusal into an outage.
const TRANSIENT_BACKOFF_MS: i64 = 5_000;

/// Floor on the backoff after a 5xx from the token endpoint, which unlike a 4xx may be hiding a rotation the
/// endpoint already committed.
const SERVER_ERROR_BACKOFF_MS: i64 = 60_000;

/// Ceiling on any backoff, including one the upstream asks for through `Retry-After`. A token's own margin
/// bounds how long a refresh can sensibly wait, and an upstream that asks for an hour would otherwise park
/// the account for an hour; an operator who needs longer can re-pair.
const MAX_NOT_BEFORE_MS: i64 = 300_000;

/// A refresh response is a small JSON document, so 30 s covers a slow upstream without letting one stalled
/// connection hold an account's only refresh slot.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);

/// Unix milliseconds. Injected so tests can drive expiry without sleeping.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

#[derive(Debug)]
pub struct AccessToken {
    pub secret: Secret,
    pub generation: u64,
    pub account_uuid: String,
}

// `Secret` is deliberately not `Clone`, so the copy is spelled out here: every caller joined to one shared
// refresh has to come away with its own token.
impl Clone for AccessToken {
    fn clone(&self) -> AccessToken {
        AccessToken {
            secret: Secret::new(self.secret.expose()),
            generation: self.generation,
            account_uuid: self.account_uuid.clone(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AcquireError {
    #[error("account is disconnected")]
    Disconnected,
    #[error("account is quarantined after a refresh of unknown outcome")]
    Quarantined,
    #[error("account has to be paired again")]
    ReauthRequired,
    #[error("account has no credentials")]
    Unpaired,
    #[error("credential unavailable: {0}")]
    Unavailable(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// The refresh outcome shared by every caller joined to one refresh. Separate from `AcquireError` because
/// `Shared` requires a cloneable output and `StoreError` wraps io, SQLite, and serde errors that cannot be
/// cloned; `store_failure` renders those into a message instead.
#[derive(Clone, Debug)]
enum RefreshFailure {
    Disconnected,
    Quarantined,
    ReauthRequired,
    Unavailable(String),
    Unpaired,
}

impl From<RefreshFailure> for AcquireError {
    fn from(failure: RefreshFailure) -> AcquireError {
        match failure {
            RefreshFailure::Disconnected => AcquireError::Disconnected,
            RefreshFailure::Quarantined => AcquireError::Quarantined,
            RefreshFailure::ReauthRequired => AcquireError::ReauthRequired,
            RefreshFailure::Unavailable(detail) => AcquireError::Unavailable(detail),
            RefreshFailure::Unpaired => AcquireError::Unpaired,
        }
    }
}

type RefreshFuture = Shared<BoxFuture<'static, Result<AccessToken, RefreshFailure>>>;

struct Inner {
    clock: Clock,
    control: Arc<ControlClient>,
    inflight: Mutex<HashMap<String, RefreshFuture>>,
    not_before: Mutex<HashMap<String, i64>>,
    #[cfg(any(test, feature = "debug-hooks"))]
    panic_next_refresh: AtomicBool,
    profile: &'static Profile,
    store: Arc<Store>,
}

/// Removes an account's in-flight entry when its refresh task ends, however it ends. A panic inside the
/// refresh must not leave the entry behind: every later caller would join a future that has already
/// resolved, and the account would be stuck on that one result until the process restarts.
struct InflightGuard {
    account_id: String,
    inner: Arc<Inner>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        // Panicking inside `Drop` while already unwinding aborts the process, so a poisoned map is left
        // alone here rather than asserted on.
        if let Ok(mut inflight) = self.inner.inflight.lock() {
            inflight.remove(&self.account_id);
        }
    }
}

pub struct CredentialManager {
    inner: Arc<Inner>,
}

impl CredentialManager {
    pub fn new(
        store: Arc<Store>,
        control: Arc<ControlClient>,
        profile: &'static Profile,
        clock: Clock,
    ) -> Self {
        CredentialManager {
            inner: Arc::new(Inner {
                clock,
                control,
                inflight: Mutex::new(HashMap::new()),
                not_before: Mutex::new(HashMap::new()),
                #[cfg(any(test, feature = "debug-hooks"))]
                panic_next_refresh: AtomicBool::new(false),
                profile,
                store,
            }),
        }
    }

    /// Returns the stored access token while it is valid beyond the refresh margin, otherwise joins or
    /// starts the account's one refresh.
    ///
    /// `enabled` is never read here. Whether a request may be served is an admission question the relay
    /// answers before it asks for a credential, and a paused account still has to keep its tokens alive
    /// for the admin views.
    pub async fn acquire(&self, account_id: &str) -> Result<AccessToken, AcquireError> {
        let (account, bundle) = self.inner.ready_account(account_id)?;
        let now = (self.inner.clock)();
        if bundle.expires_at - now > refresh_margin(&bundle) {
            return Ok(access_token(&account, &bundle));
        }
        self.join_or_start(account_id, bundle.generation).await
    }

    /// Forces the refresh path for a token the upstream has just rejected.
    ///
    /// A stored generation newer than `rejected_generation` means another caller already replaced the
    /// rejected token, so the stored one is served as is: rotating again would burn a working refresh
    /// token to solve a problem that is already solved.
    pub async fn refresh_now(
        &self,
        account_id: &str,
        rejected_generation: u64,
    ) -> Result<AccessToken, AcquireError> {
        let (account, bundle) = self.inner.ready_account(account_id)?;
        if bundle.generation > rejected_generation {
            return Ok(access_token(&account, &bundle));
        }
        self.join_or_start(account_id, bundle.generation).await
    }

    /// Quarantines every account whose last refresh never resolved: the process died between sending a
    /// refresh and storing its result, so whether the upstream rotated the refresh token is unknowable and
    /// only re-pairing can settle it. Returns the quarantined account ids.
    ///
    /// An attempt whose generations no longer describe the stored bundle is closed instead of acted on. It
    /// was about credentials that a later pairing or rotation has already replaced, and quarantining the
    /// replacement over it would leave an account nobody can make ready again.
    pub fn recover_on_start(&self) -> Result<Vec<String>, StoreError> {
        let mut recovered: Vec<String> = Vec::new();
        for attempt in self.inner.store.pending_refresh_attempts()? {
            let superseded = match self.inner.store.credentials(&attempt.account_id) {
                Ok(stored) => {
                    stored.map(|bundle| (bundle.lifecycle_generation, bundle.generation))
                        != Some((attempt.lifecycle_generation, attempt.credential_generation))
                }
                // A row that cannot be read cannot be compared, and an attempt that may still describe the
                // stored bundle is exactly what this quarantines. One unreadable account must not take the
                // whole start down with it, and its own refresh is unusable anyway.
                Err(error) => {
                    tracing::warn!(
                        account = %attempt.account_id,
                        %error,
                        "a credential row could not be read at start, so its account is quarantined",
                    );
                    false
                }
            };
            if superseded {
                self.inner.store.resolve_refresh_attempt(
                    attempt.id,
                    AttemptOutcome::Failed("superseded".into()),
                )?;
                continue;
            }
            self.inner
                .store
                .set_state(&attempt.account_id, AccountState::ReauthRequired)?;
            self.inner.store.resolve_refresh_attempt(
                attempt.id,
                AttemptOutcome::Failed("quarantined at start".into()),
            )?;
            if !recovered.contains(&attempt.account_id) {
                recovered.push(attempt.account_id);
            }
        }
        Ok(recovered)
    }

    /// Makes the next refresh panic, so a test can check that a panicking refresh does not wedge the
    /// account. Compiled only under the `debug-hooks` feature, which the tests turn on, so a release build
    /// carries no way to make a refresh panic.
    #[cfg(any(test, feature = "debug-hooks"))]
    #[doc(hidden)]
    pub fn debug_panic_on_next_refresh(&self) {
        self.inner.panic_next_refresh.store(true, Ordering::SeqCst);
    }

    /// Exposes the store so services built on top of the manager, like the usage cache, can read lifecycle
    /// generations without opening a second handle to the same database.
    pub fn store(&self) -> &Arc<Store> {
        &self.inner.store
    }

    /// Joins the account's refresh, or starts one. `observed_generation` is the generation the caller read
    /// before it got here, which the job rechecks once it holds the slot: the two are not one atomic step, so
    /// another caller can commit a rotation in between and this one must not rotate on top of it.
    async fn join_or_start(
        &self,
        account_id: &str,
        observed_generation: u64,
    ) -> Result<AccessToken, AcquireError> {
        let shared = {
            let mut inflight = self.inner.inflight.lock().expect(LOCK_POISONED);
            match inflight.get(account_id) {
                Some(existing) => existing.clone(),
                None => {
                    let inner = self.inner.clone();
                    let id = account_id.to_string();
                    // The refresh owns the whole sequence in its own task, so a caller that goes away
                    // mid-refresh only stops awaiting: the attempt row, the commit, and the resolution
                    // still run to the end. The guard drops the entry once the refresh is over, and it
                    // takes the same map lock, which is held across the spawn here so a refresh that
                    // finishes at once cannot remove the entry before it is inserted. Dropping the entry
                    // last also keeps the next caller from starting a second rotation while this commit is
                    // still in flight.
                    let handle = tokio::spawn(async move {
                        let guard = InflightGuard {
                            account_id: id,
                            inner,
                        };
                        guard
                            .inner
                            .refresh(&guard.account_id, observed_generation)
                            .await
                    });
                    let shared = async move {
                        match handle.await {
                            Ok(outcome) => outcome,
                            // Nothing else joins a detached task, so this is a panic in the refresh; the
                            // attempt row stays pending and `recover_on_start` settles it.
                            Err(_) => Err(RefreshFailure::Unavailable("refresh panicked".into())),
                        }
                    }
                    .boxed()
                    .shared();
                    inflight.insert(account_id.to_string(), shared.clone());
                    shared
                }
            }
        };
        shared.await.map_err(AcquireError::from)
    }
}

impl Inner {
    /// Loads an account that is allowed to hand out a credential, with its current bundle.
    fn ready_account(
        &self,
        account_id: &str,
    ) -> Result<(AccountRow, CredentialBundle), AcquireError> {
        let account = self.store.account(account_id)?.ok_or_else(|| {
            AcquireError::Store(StoreError::AccountNotFound(account_id.to_string()))
        })?;
        match account.state {
            AccountState::Disconnected => return Err(AcquireError::Disconnected),
            AccountState::ReauthRequired => return Err(AcquireError::ReauthRequired),
            AccountState::Unpaired => return Err(AcquireError::Unpaired),
            _ => {}
        }
        let bundle = self
            .store
            .credentials(account_id)?
            .ok_or(AcquireError::Unpaired)?;
        Ok((account, bundle))
    }

    async fn refresh(
        &self,
        account_id: &str,
        observed_generation: u64,
    ) -> Result<AccessToken, RefreshFailure> {
        // Test seam, see `debug_panic_on_next_refresh`. Checked before anything durable happens, so the
        // panicking refresh leaves no attempt row behind.
        #[cfg(any(test, feature = "debug-hooks"))]
        if self.panic_next_refresh.swap(false, Ordering::SeqCst) {
            panic!("refresh panicked on purpose");
        }
        let account = self
            .store
            .account(account_id)
            .map_err(store_failure)?
            .ok_or(RefreshFailure::Unpaired)?;
        match account.state {
            AccountState::Disconnected => return Err(RefreshFailure::Disconnected),
            AccountState::ReauthRequired => return Err(RefreshFailure::ReauthRequired),
            _ => {}
        }
        let bundle = self
            .store
            .credentials(account_id)
            .map_err(store_failure)?
            .ok_or(RefreshFailure::Unpaired)?;

        // Someone else finished a refresh between the caller's read and this job winning the slot, so the
        // stored bundle is already the answer; rotating again would spend a refresh token that has just
        // been replaced.
        if bundle.generation != observed_generation {
            return Ok(access_token(&account, &bundle));
        }

        if self.holding_off(account_id) {
            return self.stored_or(
                &account,
                &bundle,
                RefreshFailure::Unavailable("refresh is waiting out a backoff".into()),
            );
        }

        let attempt = match self.store.open_refresh_attempt(
            account_id,
            account.lifecycle_generation,
            bundle.generation,
        ) {
            Ok(attempt) => attempt,
            // Either a refresh is still running elsewhere or an earlier one resolved uncertain. Both mean
            // the upstream may already hold a rotated refresh token, and sending the stored one again
            // would spend a credential that is possibly dead.
            Err(StoreError::AttemptPending) => {
                return self.stored_or(&account, &bundle, RefreshFailure::Quarantined)
            }
            Err(error) => return Err(store_failure(error)),
        };

        let Some(refresh_token) = &bundle.refresh_token else {
            self.resolve(attempt, AttemptOutcome::Failed("no refresh token".into()));
            self.mark_reauth(&account, bundle.generation);
            return Err(RefreshFailure::ReauthRequired);
        };
        let scopes = if bundle.scopes.is_empty() {
            self.profile.control.refresh_fallback_scopes
        } else {
            bundle.scopes.as_str()
        };
        let body = refresh_body(self.profile, refresh_token.expose(), scopes);
        let request = control_request(self.profile, ControlKind::Token, None, Some(body));
        let host = ControlKind::Token.host(self.profile);

        let sent = self.control.send(request, &host, REFRESH_TIMEOUT).await;
        let (head, payload) = match sent {
            Ok(response) => response,
            Err(error) if transport_before_write(&error) => {
                self.resolve(
                    attempt,
                    AttemptOutcome::Failed(format!("transport: {error}")),
                );
                self.arm_backoff(account_id, TRANSIENT_BACKOFF_MS);
                return self.stored_or(
                    &account,
                    &bundle,
                    RefreshFailure::Unavailable(format!("refresh transport failure: {error}")),
                );
            }
            Err(error) => {
                self.resolve(
                    attempt,
                    AttemptOutcome::Uncertain(format!("refresh response lost: {error}")),
                );
                return self.stored_or(&account, &bundle, RefreshFailure::Quarantined);
            }
        };

        if head.status != 200 {
            let code = error_code(&payload);
            let detail = match &code {
                Some(code) => format!("refresh rejected with status {} ({code})", head.status),
                None => format!("refresh rejected with status {}", head.status),
            };
            // The refresh token is gone, and no retry brings it back; only re-pairing does.
            if head.status == 401
                || (head.status == 400 && code.as_deref() == Some("invalid_grant"))
            {
                self.resolve(attempt, AttemptOutcome::Failed(detail));
                self.mark_reauth(&account, bundle.generation);
                return Err(RefreshFailure::ReauthRequired);
            }
            // A rejected request never got far enough to rotate anything, so the stored refresh token is
            // still the live one and the only thing to do is wait. A 5xx is the one status where that is not
            // certain: a gateway error can sit in front of a rotation the endpoint already committed. Failing
            // it rather than quarantining is deliberate, because a token-endpoint outage would otherwise take
            // out every account that reaches its margin while it lasts; the accepted cost is that a rotation
            // hidden behind a 5xx is found later through invalid_grant, which ends in re-pairing either way,
            // and the minute of backoff bounds how fast the dead token can be spent in the meantime.
            self.resolve(attempt, AttemptOutcome::Failed(detail.clone()));
            let asked = retry_after_ms(&head, (self.clock)(), MAX_NOT_BEFORE_MS);
            self.arm_backoff(
                account_id,
                match head.status {
                    500..=599 => asked.unwrap_or(0).max(SERVER_ERROR_BACKOFF_MS),
                    _ => asked.unwrap_or(TRANSIENT_BACKOFF_MS),
                },
            );
            return self.stored_or(&account, &bundle, RefreshFailure::Unavailable(detail));
        }

        let token = match parse_token_response(&payload) {
            Ok(token) => token,
            // A 200 that cannot be read may still have rotated the refresh token upstream, which is the
            // same uncertainty as a lost response rather than something a retry can fix.
            Err(_) => {
                self.resolve(
                    attempt,
                    AttemptOutcome::Uncertain("refresh response could not be read".into()),
                );
                return self.stored_or(&account, &bundle, RefreshFailure::Quarantined);
            }
        };

        // A slot must never start serving a different upstream account than the one it was paired to, so a
        // foreign bundle is dropped rather than committed.
        if let Some(stored) = &account.identity {
            if !identity_matches(stored, &IdentityClaims::from_token(&token)) {
                self.resolve(attempt, AttemptOutcome::Failed("identity mismatch".into()));
                self.mark_reauth(&account, bundle.generation);
                return Err(RefreshFailure::ReauthRequired);
            }
        }

        let now = (self.clock)();
        let fresh = NewBundle {
            access_token: token.access_token,
            refresh_token: token.refresh_token,
            issued_at: now,
            // Saturating, because the lifetimes are upstream-controlled numbers and an overflow here would
            // panic the refresh task rather than reject the response.
            expires_at: now.saturating_add(token.expires_in.saturating_mul(1000)),
            refresh_expires_at: token
                .refresh_token_expires_in
                .map(|seconds| now.saturating_add(seconds.saturating_mul(1000))),
            scopes: token.scope.unwrap_or_else(|| bundle.scopes.clone()),
        };
        match self.store.commit_credentials(
            account_id,
            account.lifecycle_generation,
            Some(bundle.generation),
            Some(attempt),
            &fresh,
        ) {
            Ok(generation) => Ok(AccessToken {
                secret: Secret::new(fresh.access_token.expose()),
                generation,
                account_uuid: account_uuid(&account),
            }),
            // The lifecycle moved while this refresh was in flight, so the slot was disconnected and the
            // bundle belongs to an account that no longer exists in this shape.
            Err(StoreError::Stale) => {
                self.resolve(attempt, AttemptOutcome::Failed("stale lifecycle".into()));
                Err(RefreshFailure::Disconnected)
            }
            // The rotation happened upstream and could not be stored, so the stored refresh token is dead
            // even though nothing about the exchange was uncertain. Recording it as uncertain is what stops
            // the next caller from spending that dead token.
            Err(error) => {
                self.resolve(
                    attempt,
                    AttemptOutcome::Uncertain(format!(
                        "commit failed after a rotated response: {error}"
                    )),
                );
                self.stored_or(
                    &account,
                    &bundle,
                    RefreshFailure::Unavailable(format!("store: {error}")),
                )
            }
        }
    }

    /// Serves the stored access token while it is still unexpired, because an unfinished rotation must not
    /// cost a token that still works; once it expires the failure class is all that is left.
    ///
    /// The bundle is read again rather than served from the copy taken before the token request: a
    /// disconnect, a re-pair, or another caller's rotation can have replaced it in the meantime, and both
    /// generations have to still match or the token in hand belongs to a slot that no longer exists.
    fn stored_or(
        &self,
        account: &AccountRow,
        bundle: &CredentialBundle,
        failure: RefreshFailure,
    ) -> Result<AccessToken, RefreshFailure> {
        let Ok(Some(stored)) = self.store.credentials(&account.id) else {
            return Err(failure);
        };
        if stored.generation != bundle.generation
            || stored.lifecycle_generation != bundle.lifecycle_generation
            || stored.expires_at <= (self.clock)()
        {
            return Err(failure);
        }
        Ok(access_token(account, &stored))
    }

    /// Whether a recorded backoff is still in force, dropping it once it has elapsed.
    fn holding_off(&self, account_id: &str) -> bool {
        let mut not_before = self.not_before.lock().expect(LOCK_POISONED);
        let Some(until) = not_before.get(account_id).copied() else {
            return false;
        };
        if (self.clock)() < until {
            return true;
        }
        not_before.remove(account_id);
        false
    }

    fn arm_backoff(&self, account_id: &str, delay_ms: i64) {
        let until = (self.clock)().saturating_add(delay_ms.clamp(0, MAX_NOT_BEFORE_MS));
        self.not_before
            .lock()
            .expect(LOCK_POISONED)
            .insert(account_id.to_string(), until);
    }

    // A failed bookkeeping write must not mask why the refresh failed: the class is what the caller acts
    // on, and the next refresh derives the same state again from the store.
    fn resolve(&self, attempt: u64, outcome: AttemptOutcome) {
        let _ = self.store.resolve_refresh_attempt(attempt, outcome);
    }

    /// Asks for re-pairing, unless the slot moved on: a verdict about credentials that a disconnect threw
    /// away, or that a re-pairing replaced without one, must not lock out what took their place. A re-pair
    /// keeps the lifecycle generation, so the bundle generation this refresh was started against is the half
    /// of the fence that catches it.
    fn mark_reauth(&self, account: &AccountRow, credential_generation: u64) {
        let _ = self.store.set_state_if_generations(
            &account.id,
            AccountState::ReauthRequired,
            account.lifecycle_generation,
            credential_generation,
        );
    }
}

/// How long before expiry a token counts as due for refresh. The quarter-lifetime bound keeps a
/// short-lived token from sitting permanently inside its own margin, which would turn every acquire into a
/// refresh.
fn refresh_margin(bundle: &CredentialBundle) -> i64 {
    let lifetime = (bundle.expires_at - bundle.issued_at).max(0);
    MAX_MARGIN_MS.min(lifetime / 4)
}

/// Renders a store fault into a shareable refresh failure. `AcquireError::Store` carries the real
/// `StoreError`, which wraps io, SQLite, and serde errors that cannot be cloned and so cannot reach every
/// caller joined to one refresh; a `Stale` is not a fault at all, it means the slot was disconnected while
/// the refresh was in flight.
fn store_failure(error: StoreError) -> RefreshFailure {
    match error {
        StoreError::Stale => RefreshFailure::Disconnected,
        other => RefreshFailure::Unavailable(format!("store: {other}")),
    }
}

fn access_token(account: &AccountRow, bundle: &CredentialBundle) -> AccessToken {
    AccessToken {
        secret: Secret::new(bundle.access_token.expose()),
        generation: bundle.generation,
        account_uuid: account_uuid(account),
    }
}

fn account_uuid(account: &AccountRow) -> String {
    account
        .identity
        .as_ref()
        .map(|identity| identity.account_uuid.clone())
        .unwrap_or_default()
}

/// Whether the request was still unsent when the transport failed, in which case no rotation can have
/// happened upstream.
///
/// `ControlClient::send` reports `Unreachable` for every failure before the request was written whole: no
/// address for the host, a refused connection, a rejected handshake, a partially written request, and a
/// timeout that fired before the body went out. `Boring` is a local TLS configuration fault and
/// `Handshake` can only arise while the connection is being set up, so both join it. What is left,
/// `Io`, `Response`, and `TooLarge`, can all happen after the request reached the upstream intact, which
/// makes the response, not the request, the part that went missing.
fn transport_before_write(error: &TransportError) -> bool {
    matches!(
        error,
        TransportError::Boring(_) | TransportError::Handshake(_) | TransportError::Unreachable(_)
    )
}

/// The `error` member of a refusal, either the OAuth string form or the API's nested object form. Only the
/// code is read: an `error_description` is upstream-controlled text that has no business in our logs.
fn error_code(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    match value.get("error")? {
        Value::String(code) => Some(code.clone()),
        nested => nested
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

/// `Retry-After` as a delay in milliseconds, in both of the forms the header takes, clamped to `max_ms`. The
/// ceiling belongs to the caller because what an overlong wait costs differs: a refresh races a token's own
/// expiry, a usage fetch races nothing. The HTTP-date form names an instant rather than a delay, so it is read
/// against the caller's clock, which is also what keeps an upstream clock that runs ahead from parking the
/// account.
pub(crate) fn retry_after_ms(head: &ResponseHead, now: i64, max_ms: i64) -> Option<i64> {
    let value = head.get("retry-after")?.trim();
    let delay = match value.parse::<i64>() {
        Ok(seconds) => seconds.saturating_mul(1000),
        Err(_) => jiff::fmt::rfc2822::DateTimeParser::new()
            .parse_timestamp(value)
            .ok()?
            .as_millisecond()
            .saturating_sub(now),
    };
    Some(delay.clamp(0, max_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    use phoebis_profile::fixtures::{control_response, ControlFixture};
    use phoebis_store::{AccountKind, Identity, StorageKey};
    use phoebis_transport::control::Resolver;
    use phoebis_transport::{connector_builder, Plane};
    use fake_upstream::{FakeUpstream, Respond, Rule};

    /// A manager for account `a` over an in-memory store, a fixed clock, and a fake token endpoint that
    /// answers every refresh with the measured fixture.
    async fn manager_at(now: i64) -> (CredentialManager, Arc<Store>, FakeUpstream) {
        let up = FakeUpstream::start(vec![Rule {
            path_prefix: "/v1/oauth/token".into(),
            respond: Respond::Json {
                status: 200,
                headers: vec![],
                body: control_response(ControlFixture::Token).to_vec(),
            },
        }])
        .await;
        let profile = phoebis_profile::claude_code_2_1_269_linux_x64();
        let mut builder = connector_builder(profile, Plane::Control).unwrap();
        phoebis_transport::trust_extra_ca(&mut builder, up.ca_pem()).unwrap();
        let addr = up.addr();
        let resolver: Resolver = Arc::new(move |_| Some(addr));
        let control = Arc::new(ControlClient::new(profile, builder.build(), resolver));
        let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
        store
            .insert_account(&AccountRow {
                id: "a".into(),
                kind: AccountKind::Oauth,
                label: "a".into(),
                enabled: true,
                identity: Some(Identity {
                    account_uuid: "00000000-0000-4000-8000-000000000001".into(),
                    email: "test@example.invalid".into(),
                    organization_uuid: None,
                }),
                lifecycle_generation: 1,
                state: AccountState::Ready,
            })
            .unwrap();
        let clock: Clock = Arc::new(move || now);
        (
            CredentialManager::new(store.clone(), control, profile, clock),
            store,
            up,
        )
    }

    fn bundle_at(access_token: &str, issued_at: i64, expires_at: i64) -> NewBundle {
        NewBundle {
            access_token: Secret::new(access_token),
            refresh_token: Some(Secret::new("rt")),
            issued_at,
            expires_at,
            refresh_expires_at: None,
            scopes: "user:profile user:inference".into(),
        }
    }

    #[tokio::test]
    async fn a_refresh_that_lost_the_race_serves_the_committed_token() {
        let now = 1_000_000;
        let (manager, store, up) = manager_at(now).await;
        let observed = store
            .commit_credentials("a", 1, None, None, &bundle_at("observed", 0, 1))
            .unwrap();
        // What another caller's refresh does between the generation this one read and the slot it wins.
        store
            .commit_credentials(
                "a",
                1,
                Some(observed),
                None,
                &bundle_at("committed", now, now + 3_600_000),
            )
            .unwrap();
        let token = manager.join_or_start("a", observed).await.unwrap();
        assert_eq!(
            token.secret.expose(),
            "committed",
            "the job serves the rotation it lost the race to"
        );
        assert!(
            up.requests().is_empty(),
            "a token rotated a moment ago is not rotated again"
        );
        up.shutdown().await;
    }

    fn head_with(retry_after: &str) -> ResponseHead {
        ResponseHead {
            status: 429,
            headers: vec![("Retry-After".to_string(), retry_after.to_string())],
        }
    }

    #[test]
    fn a_retry_after_delay_is_bounded_by_the_callers_ceiling() {
        let now = 1_000_000;
        let capped = |value: &str| retry_after_ms(&head_with(value), now, MAX_NOT_BEFORE_MS);
        assert_eq!(capped("30"), Some(30_000));
        assert_eq!(capped("99999999999999"), Some(MAX_NOT_BEFORE_MS));
        assert_eq!(capped("-5"), Some(0));
        assert_eq!(capped("whenever"), None);
        assert_eq!(
            retry_after_ms(&head_with("3600"), now, 86_400_000),
            Some(3_600_000),
            "a caller with room for a longer wait gets the full value"
        );
    }

    #[test]
    fn a_retry_after_date_is_read_against_our_own_clock() {
        let date = "Wed, 21 Oct 2015 07:28:00 GMT";
        let at = 1_445_412_480_000;
        let capped = |now: i64| retry_after_ms(&head_with(date), now, MAX_NOT_BEFORE_MS);
        assert_eq!(capped(at - 45_000), Some(45_000));
        assert_eq!(capped(at - 86_400_000), Some(MAX_NOT_BEFORE_MS));
        assert_eq!(capped(at + 1_000), Some(0));
    }
}
