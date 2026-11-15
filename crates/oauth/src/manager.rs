//! Hands out access tokens, refreshing at most one per account at a time.

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::{BoxFuture, FutureExt, Shared};
use futures::stream::{FuturesUnordered, StreamExt};
use phoebis_profile::Profile;
use phoebis_store::{
    AccountRow, AccountState, AttemptOutcome, CredentialBundle, Identity, Secret, StateCause,
    Store, StoreError,
};
use phoebis_transport::control::ControlClient;
use phoebis_transport::response::ResponseHead;
use phoebis_transport::TransportError;
use serde_json::Value;
use tracing::Instrument;

use crate::exchange::{control_request, parse_token_response, refresh_body, ControlKind};
use crate::usage::Refused;

// A poisoned manager mutex means a prior panic happened while the refresh map was being mutated, so the
// in-flight bookkeeping can no longer be trusted.
const LOCK_POISONED: &str = "credential manager mutex poisoned by a prior panic";

/// The most of a token's remaining lifetime the manager spends refreshing ahead of expiry.
const MAX_MARGIN_MS: i64 = 300_000;

/// Backoff after a refusal that cannot have rotated anything. Short on purpose: the token it protects may
/// be a minute from expiring, and a long wait would turn a brief refusal into an outage. Exported so the relay
/// bounds its wait on the same account by the manager's own rule.
pub const TRANSIENT_BACKOFF_MS: i64 = 5_000;

/// Floor on the backoff after a 5xx from the token endpoint, which unlike a 4xx may be hiding a rotation the
/// endpoint already committed, and after every attempt resolved uncertain. Exported so the relay bounds its
/// wait on the same account by the manager's own rule.
pub const SERVER_ERROR_BACKOFF_MS: i64 = 60_000;

/// Ceiling on any backoff, including one the upstream asks for through `Retry-After`. A token's own margin
/// bounds how long a refresh can sensibly wait, and an upstream that asks for an hour would otherwise park
/// the account for an hour even when its limit lifts early; retrying every quarter hour costs a few 429s
/// against the token endpoint and lands the refresh soon after the limit is gone.
const MAX_NOT_BEFORE_MS: i64 = 900_000;

/// A refresh response is a small JSON document, so 30 s covers a slow upstream without letting one stalled
/// connection hold an account's only refresh slot.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);

pub use phoebis_store::Clock;

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

/// A `reauth_required` mark an acquisition landed, for the caller to report: the state, its cause and the three
/// generations the fenced write carried, as the relay reports its own mark.
#[derive(Clone, Debug, PartialEq)]
pub struct LandedMark {
    pub account_id: String,
    pub state: AccountState,
    pub cause: StateCause,
    pub incarnation: u64,
    pub lifecycle_generation: u64,
    pub credential_generation: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum AcquireError {
    #[error("account is disconnected")]
    Disconnected,
    #[error("the refresh produced no newer credential")]
    NotRefreshed,
    #[error("account is quarantined after a refresh of unknown outcome")]
    Quarantined,
    /// Carries the mark when this acquisition's refresh landed it. The store holds the mark from then on, but the
    /// routing snapshot only learns of it from a report, so the caller that got the mark sends it.
    #[error("account has to be paired again")]
    ReauthRequired(Option<LandedMark>),
    #[error("account has no credentials")]
    Unpaired,
    /// The manager is draining for shutdown, so no refresh starts.
    #[error("refresh refused at shutdown")]
    ShuttingDown,
    #[error("credential unavailable: {0}")]
    Unavailable(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl AcquireError {
    /// The mark this acquisition landed, for the caller to report.
    pub fn landed_mark(&self) -> Option<&LandedMark> {
        match self {
            AcquireError::ReauthRequired(mark) => mark.as_ref(),
            _ => None,
        }
    }
}

/// The refresh outcome shared by every caller joined to one refresh. Separate from `AcquireError` because
/// `Shared` requires a cloneable output and `StoreError` wraps io, SQLite, and serde errors that cannot be
/// cloned; `store_failure` renders those into a message instead.
#[derive(Clone, Debug)]
enum RefreshFailure {
    Disconnected,
    Quarantined,
    ReauthRequired(Option<LandedMark>),
    Unavailable(String),
    Unpaired,
}

impl From<RefreshFailure> for AcquireError {
    fn from(failure: RefreshFailure) -> AcquireError {
        match failure {
            RefreshFailure::Disconnected => AcquireError::Disconnected,
            RefreshFailure::Quarantined => AcquireError::Quarantined,
            RefreshFailure::ReauthRequired(mark) => AcquireError::ReauthRequired(mark),
            RefreshFailure::Unavailable(detail) => AcquireError::Unavailable(detail),
            RefreshFailure::Unpaired => AcquireError::Unpaired,
        }
    }
}

type RefreshFuture = Shared<BoxFuture<'static, Result<AccessToken, RefreshFailure>>>;

/// A backoff the manager armed on an account, on the manager's clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backoff {
    /// The instant before which no refresh of the account runs.
    pub until: i64,
    /// How long the backoff was armed for. A caller tells a short refusal from the token endpoint's floor by
    /// this rather than by `until` less the clock, which a clock that moved since the arming would misread.
    pub length_ms: i64,
}

/// What armed an account's backoff, which decides what a caller inside it is told: a refusal or a 5xx is waited
/// out as unavailable, and an uncertain attempt answers quarantined, since the refresh token it would spend may
/// already be dead upstream.
#[derive(Clone, Copy)]
enum Armed {
    Refused,
    Uncertain,
}

/// Why a refresh attempt ended other than committed, holding the stored cause word and the detail sentence of each
/// ending side by side, since the ledger's refresh rows read the word.
enum RefreshCause {
    CommitFailed,
    IdentityMismatch,
    NoRefreshToken,
    QuarantinedAtStart,
    Rejected { status: u16, code: Option<String> },
    ResponseLost,
    ResponseUnreadable,
    StaleLifecycle,
    Transport,
}

impl RefreshCause {
    /// The cause the attempt stores, which for a rejection is the endpoint's code or else its status.
    fn word(&self) -> String {
        match self {
            RefreshCause::CommitFailed => "commit_failed_after_a_rotated_response".into(),
            RefreshCause::IdentityMismatch => "identity_mismatch".into(),
            RefreshCause::NoRefreshToken => "no_refresh_token".into(),
            RefreshCause::QuarantinedAtStart => "quarantined_at_start".into(),
            RefreshCause::Rejected { status, code } => {
                code.clone().unwrap_or_else(|| status.to_string())
            }
            RefreshCause::ResponseLost => "refresh_response_lost".into(),
            RefreshCause::ResponseUnreadable => "refresh_response_could_not_be_read".into(),
            RefreshCause::StaleLifecycle => "stale_lifecycle".into(),
            RefreshCause::Transport => "transport".into(),
        }
    }

    /// The attempt's detail sentence, with the error that ended it where there is one.
    fn detail(&self, error: Option<&dyn Display>) -> String {
        let sentence: String = match self {
            RefreshCause::CommitFailed => "commit failed after a rotated response".into(),
            RefreshCause::IdentityMismatch => "identity mismatch".into(),
            RefreshCause::NoRefreshToken => "no refresh token".into(),
            RefreshCause::QuarantinedAtStart => "quarantined at start".into(),
            RefreshCause::Rejected {
                status,
                code: Some(code),
            } => format!("refresh rejected with status {status} ({code})"),
            RefreshCause::Rejected { status, code: None } => {
                format!("refresh rejected with status {status}")
            }
            RefreshCause::ResponseLost => "refresh response lost".into(),
            RefreshCause::ResponseUnreadable => "refresh response could not be read".into(),
            RefreshCause::StaleLifecycle => "stale lifecycle".into(),
            RefreshCause::Transport => "transport".into(),
        };
        match error {
            Some(error) => format!("{sentence}: {error}"),
            None => sentence,
        }
    }

    fn failed(self, error: Option<&dyn Display>) -> AttemptOutcome {
        AttemptOutcome::Failed {
            cause: self.word(),
            detail: self.detail(error),
        }
    }

    fn uncertain(self, error: Option<&dyn Display>) -> AttemptOutcome {
        AttemptOutcome::Uncertain {
            cause: self.word(),
            detail: self.detail(error),
        }
    }
}

/// Where an acquisition takes the store reads a cache miss costs.
#[derive(Clone, Copy)]
enum Read {
    /// On a blocking thread: a leg's worker serves other requests rather than wait on the store's connection.
    Blocking,
    /// On the caller's task: the usage poll, off the request path, keeps an attempt's reads and its request inside
    /// the scheduler's turn that drew it, which its suite asserts by stepping a paused clock; a hop to a blocking
    /// thread finishes in real time while that clock stands still, so the request would leave a step late.
    Inline,
}

impl Read {
    /// Runs `op` on the store where the read says. The join error of a blocking run is the caller's to handle.
    async fn run<T, F>(self, store: &Arc<Store>, op: F) -> Result<T, tokio::task::JoinError>
    where
        T: Send + 'static,
        F: FnOnce(&Store) -> T + Send + 'static,
    {
        match self {
            Read::Blocking => {
                let store = store.clone();
                tokio::task::spawn_blocking(move || op(&store)).await
            }
            Read::Inline => Ok(op(store)),
        }
    }
}

/// A bundle the manager read or committed, under its lineage.
struct BundleEntry {
    lineage: Refused,
    bundle: Arc<CredentialBundle>,
}

/// An account's place in the cache.
#[derive(Default)]
struct CacheSlot {
    /// Advanced by every invalidation and every mark, so a read or a refresh that began before one can tell that the
    /// bundle it holds may be the replaced one and leave the entry alone.
    epoch: u64,
    /// The bundle the account's acquisitions answer from while its lineage holds, so a leg pays no registry read.
    entry: Option<BundleEntry>,
    /// The lineage whose refresh the token endpoint refused for good. The mark is the store's alone until the next
    /// reload, while the rows the legs carry still read `ready`, and the bundle stored under the lineage is the one
    /// the mark is about: a miss on it is refused rather than read. A credential committed past it, by a pairing or a
    /// disconnect, ends the refusal.
    refused: Option<Refused>,
}

impl CacheSlot {
    /// Whether a miss for the row answers the recorded refusal: the row is the refused holder and slot, and the
    /// caller expects no generation past the refused one, which only a pairing committed since could have produced.
    fn refuses(&self, account: &AccountRow, expected: Option<u64>) -> bool {
        self.refused.is_some_and(|refused| {
            refused.of_row(account)
                && expected.is_none_or(|expected| expected <= refused.credential_generation)
        })
    }

    /// Drops the entry and moves the epoch, so a bundle read or committed before this call is not put back.
    fn outrun(&mut self) {
        self.entry = None;
        self.epoch += 1;
    }
}

/// What the cache answers a row.
enum Lookup {
    Hit(Arc<CredentialBundle>),
    Miss,
    /// A miss on a lineage a re-authentication mark refused.
    Refused,
}

struct Inner {
    bundles: Mutex<HashMap<String, CacheSlot>>,
    clock: Clock,
    control: Arc<ControlClient>,
    /// Set by `drain` under the `inflight` lock, so no refresh starts outside the set it waits for.
    closing: AtomicBool,
    inflight: Mutex<HashMap<String, RefreshFuture>>,
    not_before: Mutex<HashMap<String, (Backoff, Armed)>>,
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
                bundles: Mutex::new(HashMap::new()),
                clock,
                control,
                closing: AtomicBool::new(false),
                inflight: Mutex::new(HashMap::new()),
                not_before: Mutex::new(HashMap::new()),
                #[cfg(any(test, feature = "debug-hooks"))]
                panic_next_refresh: AtomicBool::new(false),
                profile,
                store,
            }),
        }
    }

    /// The token for the row the selection chose, under the generation the router's snapshot read: the cached
    /// bundle answers when its incarnation and lifecycle generation match the row's and its generation is at or
    /// above the expectation, else the store is read once. The row's `state` answers the three refusals, so a leg
    /// pays no registry read while the snapshot lineage holds. A bundle ahead of the expectation is a refresh the
    /// router's reload has not caught up with, and it serves; one behind it is a cache a rotation outran.
    ///
    /// A token valid beyond the refresh margin is answered as it is, otherwise the account's one refresh is
    /// joined or started. `enabled` is never read here. Whether a request may be served is an admission question
    /// the relay answers before it asks for a credential, and a paused account still has to keep its tokens
    /// alive for the admin views.
    pub async fn acquire_for(
        &self,
        account: &AccountRow,
        expected_generation: Option<u64>,
    ) -> Result<AccessToken, AcquireError> {
        self.acquire_with(account, expected_generation, Read::Blocking)
            .await
    }

    /// `acquire_for`'s body, the store read of a miss taken where `read` says.
    async fn acquire_with(
        &self,
        account: &AccountRow,
        expected_generation: Option<u64>,
        read: Read,
    ) -> Result<AccessToken, AcquireError> {
        refuse_unusable(account)?;
        let bundle = self
            .inner
            .bundle(account, expected_generation, read)
            .await?;
        let account_id = account.id.as_str();
        let now = (self.inner.clock)();
        if bundle.expires_at - now > refresh_margin(&bundle) {
            return Ok(access_token(account, &bundle));
        }
        match self.join_or_start(account_id, bundle.generation).await {
            Ok(token) => Ok(token),
            // Says nothing about the account, and a recorded message would outlive the process it was true of.
            Err(AcquireError::ShuttingDown) => Err(AcquireError::ShuttingDown),
            Err(failure) => {
                // A backoff, a quarantine, or a store race can all fail this call without ever opening a
                // refresh attempt, which is the one place `resolve` would otherwise have recorded why.
                // `Unavailable`'s own detail is used bare rather than through `AcquireError`'s Display, which
                // wraps it in a generic "credential unavailable:" prefix that only restates the field name.
                let message = match &failure {
                    AcquireError::Unavailable(detail) => detail.clone(),
                    other => other.to_string(),
                };
                let id = account_id.to_string();
                let incarnation = account.incarnation;
                // The record reads and writes the store, so it runs where `read` says. The span is entered inside the
                // closure, so the warning keeps the caller's span on a blocking thread too.
                let span = tracing::Span::current();
                let recorded = read
                    .run(&self.inner.store, move |store| {
                        let _entered = span.enter();
                        record_failure(store, &id, incarnation, &message);
                    })
                    .await;
                if let Err(error) = recorded {
                    if error.is_panic() {
                        std::panic::resume_unwind(error.into_panic());
                    }
                }
                Err(failure)
            }
        }
    }

    /// `acquire_for` over the row read from the store, for a caller that holds an id alone (the usage service).
    /// Its reads run inline on the caller's task, for the reason `Read::Inline` gives.
    pub async fn acquire(&self, account_id: &str) -> Result<AccessToken, AcquireError> {
        let account = self.inner.stored_account(account_id)?;
        self.acquire_with(&account, None, Read::Inline).await
    }

    /// Drops the account's cached bundle: a credential committed from outside the manager (a pairing, a
    /// disconnect) is served on the next call rather than after the cached one expires. A refusal recorded against
    /// the replaced credential goes with it.
    pub fn invalidate(&self, account_id: &str) {
        let mut bundles = self.inner.bundles.lock().expect(LOCK_POISONED);
        let slot = bundles.entry(account_id.to_string()).or_default();
        slot.outrun();
        slot.refused = None;
    }

    /// Refuses the lineage of a token the upstream rejected on a replay, as a landed mark refuses its own: the token
    /// is the one a refresh just committed, valid to its expiry, so a leg that still carries the pre-mark row would be
    /// handed it again from the cache or the store until the relay's report reaches the snapshot. A credential
    /// committed past `credential_generation`, by a pairing, ends the refusal, and one committed while the replay was
    /// upstream supersedes it: the slot's newer bundle or refusal stays as it is.
    pub fn refuse_rejected(&self, account: &AccountRow, credential_generation: u64) {
        // Read before the store, so an invalidation that lands after the read supersedes the refusal as well.
        let epoch = self.inner.epoch(&account.id);
        let superseded = self
            .inner
            .store
            .credential_metadata(&account.id)
            .is_ok_and(|stored| {
                stored.is_some_and(|stored| {
                    stored.lifecycle_generation != account.lifecycle_generation
                        || stored.generation > credential_generation
                })
            });
        if !superseded {
            self.inner.refuse(
                &account.id,
                Refused::of(account, credential_generation),
                Some(epoch),
            );
        }
    }

    /// Forces the refresh path for a token the upstream has just rejected.
    ///
    /// A stored generation newer than `rejected_generation` means another caller already replaced the
    /// rejected token, so the stored one is served as is: rotating again would burn a working refresh
    /// token to solve a problem that is already solved.
    ///
    /// A refresh that could not rotate still hands back the stored token, because an unexpired credential
    /// must not be lost to a token endpoint having a bad minute. That token is the one the upstream just
    /// rejected, so `NotRefreshed` says so rather than letting the caller replay it into a second 401 and
    /// the operator-cleared lockout that follows.
    pub async fn refresh_now(
        &self,
        account_id: &str,
        rejected_generation: u64,
    ) -> Result<AccessToken, AcquireError> {
        let (account, bundle) = self.inner.ready_account(account_id)?;
        if bundle.generation > rejected_generation {
            return Ok(access_token(&account, &bundle));
        }
        let token = self.join_or_start(account_id, bundle.generation).await?;
        if token.generation <= rejected_generation {
            return Err(AcquireError::NotRefreshed);
        }
        Ok(token)
    }

    /// Quarantines every account whose last refresh never resolved: the process died between sending a
    /// refresh and storing its result, so whether the upstream rotated the refresh token is unknowable and
    /// only re-pairing can settle it. Returns the quarantined account ids.
    ///
    /// An attempt whose incarnation or generations no longer describe the stored account and bundle is
    /// closed instead of acted on. It was about a holder the id no longer has, or credentials that a later
    /// pairing or rotation has already replaced, and quarantining the replacement over it would leave an
    /// account nobody can make ready again.
    pub fn recover_on_start(&self) -> Result<Vec<String>, StoreError> {
        let mut recovered: Vec<String> = Vec::new();
        for attempt in self.inner.store.pending_refresh_attempts()? {
            // The row standing under this id now, whose incarnation settles whether the attempt is even
            // about it. An id with no row left has orphaned attempts, which the store settles regardless.
            let incarnation = self
                .inner
                .store
                .account(&attempt.account_id)?
                .map_or(0, |account| account.incarnation);
            let superseded = attempt.incarnation != incarnation
                || match self.inner.store.credentials(&attempt.account_id) {
                    Ok(stored) => {
                        stored.map(|bundle| (bundle.lifecycle_generation, bundle.generation))
                            != Some((attempt.lifecycle_generation, attempt.credential_generation))
                    }
                    // A row that cannot be read cannot be compared, and an attempt that may still describe
                    // the stored bundle is exactly what this quarantines. One unreadable account must not
                    // take the whole start down with it, and its own refresh is unusable anyway.
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
                    incarnation,
                    AttemptOutcome::Superseded,
                )?;
                continue;
            }
            self.inner.store.set_state(
                &attempt.account_id,
                AccountState::ReauthRequired,
                StateCause::RefreshFailed,
            )?;
            self.inner.store.resolve_refresh_attempt(
                attempt.id,
                incarnation,
                RefreshCause::QuarantinedAtStart.failed(None),
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

    /// The backoff the manager last armed on the account: a refusal's, a token-endpoint 5xx's or an uncertain
    /// attempt's. The relay reads it to wait out a short backoff on the same account rather than spend another
    /// account on a request whose own credential may work again in seconds.
    ///
    /// `None` when nothing holds the account off or the clock is past the instant: an entry left by a refresh that
    /// failed before reading it is not this failure's backoff, and a wait of zero on it would spend the request's
    /// one hold. At the instant itself it still reads as armed, so a clock that reaches it between the arming and
    /// this read ends the wait rather than skipping the retry.
    pub fn backoff(&self, account_id: &str) -> Option<Backoff> {
        let now = (self.inner.clock)();
        self.inner
            .not_before
            .lock()
            .expect(LOCK_POISONED)
            .get(account_id)
            .map(|(backoff, _)| *backoff)
            .filter(|backoff| backoff.until >= now)
    }

    /// The instant, on the manager's clock, before which no refresh of the account runs: the backoff a refusal,
    /// a token-endpoint 5xx or an uncertain attempt armed, as `backoff` reads it.
    pub fn backoff_until(&self, account_id: &str) -> Option<i64> {
        self.backoff(account_id).map(|backoff| backoff.until)
    }

    /// `mark_reauth`'s conditional write under `StateCause::AuthFailed`, fenced on `refused`: a refresh or a re-pair
    /// that moved a generation since the fetch lands nothing.
    pub fn mark_auth_failed(&self, account_id: &str, refused: Refused) -> Option<LandedMark> {
        self.inner
            .mark_reauth(account_id, StateCause::AuthFailed, refused)
    }

    /// Exposes the store so services built on top of the manager, like the usage cache, can read lifecycle
    /// generations without opening a second handle to the same database.
    pub fn store(&self) -> &Arc<Store> {
        &self.inner.store
    }

    /// Closes the manager to new refreshes and awaits every in-flight one up to `REFRESH_TIMEOUT`, the longest a
    /// refresh's token request may take; one still running then keeps its pending row, its mark at the next start
    /// and its log line, as an unjoined refresh does. The flag is set under the `inflight` lock before the set is
    /// taken, so a caller that asks afterwards joins a refresh in the set or is refused with `ShuttingDown`.
    pub async fn drain(&self) {
        self.drain_within(REFRESH_TIMEOUT).await;
    }

    /// `drain` bounded by the shorter of `within` and `REFRESH_TIMEOUT`, for a shutdown whose budget has less left.
    pub async fn drain_within(&self, within: Duration) {
        let mut running: FuturesUnordered<RefreshFuture> = {
            let inflight = self.inner.inflight.lock().expect(LOCK_POISONED);
            self.inner.closing.store(true, Ordering::SeqCst);
            inflight.values().cloned().collect()
        };
        if running.is_empty() {
            return;
        }
        let joined = tokio::time::timeout(within.min(REFRESH_TIMEOUT), async {
            while running.next().await.is_some() {}
        })
        .await;
        if joined.is_err() {
            tracing::warn!(
                unjoined = running.len(),
                "credential refreshes still running at shutdown, so the next start quarantines their accounts"
            );
        }
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
                None if self.inner.closing.load(Ordering::SeqCst) => {
                    return Err(AcquireError::ShuttingDown)
                }
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
    /// The account's bundle: the cached one while its lineage is the row's and its generation is at or above
    /// `expected`, else one store read where `read` says, which replaces the entry. A read an invalidation outran
    /// drops what it read, since a pairing may have committed under it, and runs once more; a second invalidation
    /// under that read answers what the cache holds by then.
    async fn bundle(
        &self,
        account: &AccountRow,
        expected: Option<u64>,
        read: Read,
    ) -> Result<Arc<CredentialBundle>, AcquireError> {
        let mut retried = false;
        loop {
            let (epoch, found) = self.lookup(account, expected);
            match found {
                Lookup::Hit(bundle) => return Ok(bundle),
                Lookup::Refused => return Err(AcquireError::ReauthRequired(None)),
                Lookup::Miss => {}
            }
            let id = account.id.clone();
            let stored = read
                .run(&self.store, move |store| store.credentials(&id))
                .await
                .map_err(|error| {
                    AcquireError::Unavailable(format!("the credential read failed: {error}"))
                })??;
            let bundle = Arc::new(stored.ok_or(AcquireError::Unpaired)?);
            if self.remember(account, bundle.clone(), epoch) {
                return Ok(bundle);
            }
            if retried {
                return match self.lookup(account, expected).1 {
                    Lookup::Hit(bundle) => Ok(bundle),
                    Lookup::Refused => Err(AcquireError::ReauthRequired(None)),
                    Lookup::Miss => Err(AcquireError::Unavailable(
                        "the credential changed while it was read".into(),
                    )),
                };
            }
            retried = true;
        }
    }

    /// The cache's answer for the row, with the epoch it was read under: the entry while its lineage is the row's
    /// and its generation at or above `expected`, the refusal for a miss on a refused lineage, or a miss.
    fn lookup(&self, account: &AccountRow, expected: Option<u64>) -> (u64, Lookup) {
        let bundles = self.bundles.lock().expect(LOCK_POISONED);
        let Some(slot) = bundles.get(&account.id) else {
            return (0, Lookup::Miss);
        };
        let hit = slot
            .entry
            .as_ref()
            .filter(|entry| {
                entry.lineage.of_row(account)
                    && expected
                        .is_none_or(|expected| entry.lineage.credential_generation >= expected)
            })
            .map(|entry| entry.bundle.clone());
        let found = match hit {
            Some(bundle) => Lookup::Hit(bundle),
            None if slot.refuses(account, expected) => Lookup::Refused,
            None => Lookup::Miss,
        };
        (slot.epoch, found)
    }

    /// The account's current epoch, read before a store read or a refresh so `remember` can tell whether an
    /// invalidation landed in between.
    fn epoch(&self, account_id: &str) -> u64 {
        self.bundles
            .lock()
            .expect(LOCK_POISONED)
            .get(account_id)
            .map_or(0, |slot| slot.epoch)
    }

    /// Keeps `bundle` as the account's entry under the row's incarnation, and answers whether the caller may serve
    /// it: `false` when the slot's epoch moved past `epoch` since the caller read it, since an invalidation landed in
    /// between and the bundle may be the one it replaced, and `false` for a bundle at or under a refused generation,
    /// which is the one the mark is about. Under a held epoch, a bundle of another lifecycle than the row's is left
    /// out, since its lineage is not the row's to vouch for; an entry the same lineage holds at a newer generation
    /// stays, since a read that lost a race to a refresh commit would put the older one back; and a bundle past the
    /// refused generation is a pairing's and ends the refusal.
    fn remember(&self, account: &AccountRow, bundle: Arc<CredentialBundle>, epoch: u64) -> bool {
        let mut bundles = self.bundles.lock().expect(LOCK_POISONED);
        let slot = bundles.entry(account.id.clone()).or_default();
        if slot.epoch != epoch {
            return false;
        }
        if bundle.lifecycle_generation != account.lifecycle_generation {
            return true;
        }
        let lineage = Refused::of(account, bundle.generation);
        if let Some(refused) = slot.refused.filter(|refused| refused.of_row(account)) {
            if lineage.credential_generation <= refused.credential_generation {
                return false;
            }
            slot.refused = None;
        }
        let newer = slot.entry.as_ref().is_some_and(|entry| {
            entry.lineage.of_row(account)
                && entry.lineage.credential_generation > lineage.credential_generation
        });
        if !newer {
            slot.entry = Some(BundleEntry { lineage, bundle });
        }
        true
    }

    /// The token stored now, for a refresh whose own bundle an invalidation outran: one read under the current
    /// epoch, and what the cache holds when a second invalidation outruns that one too.
    fn stored_now(&self, account: &AccountRow) -> Result<AccessToken, RefreshFailure> {
        let epoch = self.epoch(&account.id);
        let bundle = self
            .store
            .credentials(&account.id)
            .map_err(store_failure)?
            .ok_or(RefreshFailure::Unpaired)?;
        let bundle = Arc::new(bundle);
        if self.remember(account, bundle.clone(), epoch) {
            return Ok(access_token(account, &bundle));
        }
        match self.lookup(account, None).1 {
            Lookup::Hit(current) => Ok(access_token(account, &current)),
            Lookup::Refused => Err(RefreshFailure::ReauthRequired(None)),
            Lookup::Miss => Err(RefreshFailure::Unavailable(
                "the credential changed while it was refreshed".into(),
            )),
        }
    }

    fn stored_account(&self, account_id: &str) -> Result<AccountRow, AcquireError> {
        self.store
            .account(account_id)?
            .ok_or_else(|| AcquireError::Store(StoreError::AccountNotFound(account_id.to_string())))
    }

    /// Loads an account that is allowed to hand out a credential, with its current bundle.
    fn ready_account(
        &self,
        account_id: &str,
    ) -> Result<(AccountRow, CredentialBundle), AcquireError> {
        let account = self.stored_account(account_id)?;
        refuse_unusable(&account)?;
        let bundle = self
            .store
            .credentials(account_id)?
            .ok_or(AcquireError::Unpaired)?;
        Ok((account, bundle))
    }

    /// Traced under one span per call, so an operator reading logs for a failing account can follow one
    /// refresh's steps without the account id repeated on every line.
    async fn refresh(
        &self,
        account_id: &str,
        observed_generation: u64,
    ) -> Result<AccessToken, RefreshFailure> {
        self.refresh_inner(account_id, observed_generation)
            .instrument(tracing::debug_span!("refresh", account = %account_id))
            .await
    }

    async fn refresh_inner(
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
        // Read ahead of the store reads, so a pairing or a disconnect committed anywhere under this refresh shows as
        // a moved epoch when the result is published.
        let epoch = self.epoch(account_id);
        let account = self
            .store
            .account(account_id)
            .map_err(store_failure)?
            .ok_or(RefreshFailure::Unpaired)?;
        match account.state {
            AccountState::Disconnected => return Err(RefreshFailure::Disconnected),
            AccountState::ReauthRequired => return Err(RefreshFailure::ReauthRequired(None)),
            _ => {}
        }
        let bundle = self
            .store
            .credentials(account_id)
            .map_err(store_failure)?
            .ok_or(RefreshFailure::Unpaired)?;

        // Someone else finished a refresh between the caller's read and this job winning the slot, so the
        // stored bundle is already the answer; rotating again would spend a refresh token that has just
        // been replaced. It replaces the entry the caller read too, or every later call would come back here.
        if bundle.generation != observed_generation {
            let token = access_token(&account, &bundle);
            if !self.remember(&account, Arc::new(bundle), epoch) {
                return self.stored_now(&account);
            }
            return Ok(token);
        }

        if let Some(failure) = self.holding_off(account_id) {
            return self.stored_or(&account, &bundle, failure);
        }

        let attempt = match self.store.open_refresh_attempt(
            account_id,
            account.incarnation,
            account.lifecycle_generation,
            bundle.generation,
        ) {
            Ok(attempt) => attempt,
            // A refresh is still running elsewhere, so the upstream may already hold a rotated refresh token
            // and sending the stored one again would spend a credential that is possibly dead; one resolved
            // uncertain is retried after the manager's floor instead.
            Err(StoreError::AttemptPending) => {
                return self.stored_or(&account, &bundle, RefreshFailure::Quarantined)
            }
            Err(error) => return Err(store_failure(error)),
        };
        tracing::debug!(attempt, "refresh attempt opened");

        let Some(refresh_token) = &bundle.refresh_token else {
            return Err(self.refused_for_good(
                attempt,
                &account,
                &bundle,
                RefreshCause::NoRefreshToken,
                None,
            ));
        };
        let scopes = if bundle.scopes.is_empty() {
            self.profile.control.refresh_fallback_scopes
        } else {
            bundle.scopes.as_str()
        };
        let body = refresh_body(self.profile, refresh_token.expose(), scopes);
        let request = control_request(self.profile, ControlKind::Token, None, Some(body));
        let host = ControlKind::Token.host(self.profile);
        tracing::debug!(host = %host, "refresh request sent");

        let sent = self.control.send(request, host, REFRESH_TIMEOUT).await;
        let (head, payload) = match sent {
            Ok(response) => response,
            Err(error) if transport_before_write(&error) => {
                self.resolve(
                    attempt,
                    account.incarnation,
                    RefreshCause::Transport.failed(Some(&error)),
                );
                self.arm_backoff(account_id, TRANSIENT_BACKOFF_MS, Armed::Refused);
                return self.stored_or(
                    &account,
                    &bundle,
                    RefreshFailure::Unavailable(format!("refresh transport failure: {error}")),
                );
            }
            Err(error) => {
                // The write happened and the answer did not: the endpoint may hold a rotated token, the one
                // fact every uncertain outcome shares. Waiting out the floor before one retry with the stored
                // token keeps a valid token serving and lets the retry settle the attempt either way; a restart
                // in between still quarantines it.
                return self.uncertain(
                    attempt,
                    &account,
                    &bundle,
                    RefreshCause::ResponseLost,
                    Some(&error),
                );
            }
        };
        let code = error_code(&payload);
        tracing::debug!(status = head.status, code, "refresh response status read");

        if head.status != 200 {
            let cause = RefreshCause::Rejected {
                status: head.status,
                code: code.clone(),
            };
            // The refresh token is gone, and no retry brings it back; only re-pairing does.
            if head.status == 401
                || (head.status == 400 && code.as_deref() == Some("invalid_grant"))
            {
                return Err(self.refused_for_good(attempt, &account, &bundle, cause, None));
            }
            // A rejected request never got far enough to rotate anything, so the stored refresh token is
            // still the live one and the only thing to do is wait. A 5xx is the one status where that is not
            // certain: a gateway error can sit in front of a rotation the endpoint already committed. Failing
            // it rather than quarantining is deliberate, because a token-endpoint outage would otherwise take
            // out every account that reaches its margin while it lasts; the accepted cost is that a rotation
            // hidden behind a 5xx is found later through invalid_grant, which ends in re-pairing either way,
            // and the minute of backoff bounds how fast the dead token can be spent in the meantime.
            let detail = cause.detail(None);
            self.resolve(attempt, account.incarnation, cause.failed(None));
            let asked = retry_after_ms(&head, (self.clock)(), MAX_NOT_BEFORE_MS);
            self.arm_backoff(
                account_id,
                match head.status {
                    500..=599 => asked.unwrap_or(0).max(SERVER_ERROR_BACKOFF_MS),
                    _ => asked.unwrap_or(TRANSIENT_BACKOFF_MS),
                },
                Armed::Refused,
            );
            return self.stored_or(&account, &bundle, RefreshFailure::Unavailable(detail));
        }

        let token = match parse_token_response(&payload) {
            Ok(token) => token,
            // A 200 that cannot be read may still have rotated the refresh token upstream, which is the
            // same uncertainty as a lost response rather than something a retry can fix.
            Err(_) => {
                return self.uncertain(
                    attempt,
                    &account,
                    &bundle,
                    RefreshCause::ResponseUnreadable,
                    None,
                );
            }
        };

        // A slot must never start serving a different upstream account than the one it was paired to, so a
        // foreign bundle is dropped rather than committed.
        if let Some(stored) = &account.identity {
            if !identity_matches(stored, token.account.as_ref().map(|a| a.uuid.as_str())) {
                return Err(self.refused_for_good(
                    attempt,
                    &account,
                    &bundle,
                    RefreshCause::IdentityMismatch,
                    None,
                ));
            }
        }

        let now = (self.clock)();
        let fresh = token.into_bundle(now, &bundle.scopes);
        match self.store.commit_credentials(
            account_id,
            account.incarnation,
            account.lifecycle_generation,
            Some(bundle.generation),
            Some(attempt),
            &fresh,
        ) {
            Ok(generation) => {
                // The one write that moves the generation without a reload, so the entry moves with it. A response
                // without a refresh token keeps the stored one, and one without the token's end keeps the stored
                // end, as the commit does.
                let committed = Arc::new(CredentialBundle {
                    access_token: Secret::new(fresh.access_token.expose()),
                    refresh_token: fresh
                        .refresh_token
                        .as_ref()
                        .or(bundle.refresh_token.as_ref())
                        .map(|token| Secret::new(token.expose())),
                    issued_at: fresh.issued_at,
                    expires_at: fresh.expires_at,
                    refresh_expires_at: fresh.refresh_expires_at.or(bundle.refresh_expires_at),
                    scopes: fresh.scopes.clone(),
                    generation,
                    lifecycle_generation: account.lifecycle_generation,
                });
                let token = access_token(&account, &committed);
                if !self.remember(&account, committed, epoch) {
                    return self.stored_now(&account);
                }
                Ok(token)
            }
            // The lifecycle or the incarnation moved while this refresh was in flight, so the slot was
            // disconnected or replaced; a generation race between two commits reads the same to a refresh,
            // since either way the bundle this refresh held no longer names the account's current state.
            Err(StoreError::Stale | StoreError::Disconnected) => {
                self.resolve(
                    attempt,
                    account.incarnation,
                    RefreshCause::StaleLifecycle.failed(None),
                );
                Err(RefreshFailure::Disconnected)
            }
            // The rotation happened upstream and could not be stored, so the stored refresh token is likely
            // dead even though nothing about the exchange was uncertain: it is recorded, spaced and answered as
            // the lost response is, so a caller reads one class whichever uncertain arm it met.
            Err(error) => self.uncertain(
                attempt,
                &account,
                &bundle,
                RefreshCause::CommitFailed,
                Some(&error),
            ),
        }
    }

    /// The ending of a refresh whose refresh token is gone for good: the attempt failed, and the slot is marked for
    /// re-pairing at the bundle generation the refresh read.
    fn refused_for_good(
        &self,
        attempt: u64,
        account: &AccountRow,
        bundle: &CredentialBundle,
        cause: RefreshCause,
        error: Option<&dyn Display>,
    ) -> RefreshFailure {
        self.resolve(attempt, account.incarnation, cause.failed(error));
        let mark = self.mark_reauth(
            &account.id,
            StateCause::RefreshFailed,
            Refused::of(account, bundle.generation),
        );
        RefreshFailure::ReauthRequired(mark)
    }

    /// The ending of a refresh that may have rotated the token upstream: the attempt is uncertain, the floor is
    /// armed before one retry, and the stored token serves while it is unexpired.
    fn uncertain(
        &self,
        attempt: u64,
        account: &AccountRow,
        bundle: &CredentialBundle,
        cause: RefreshCause,
        error: Option<&dyn Display>,
    ) -> Result<AccessToken, RefreshFailure> {
        self.resolve(attempt, account.incarnation, cause.uncertain(error));
        self.arm_backoff(&account.id, SERVER_ERROR_BACKOFF_MS, Armed::Uncertain);
        self.stored_or(account, bundle, RefreshFailure::Quarantined)
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

    /// The failure a caller inside a recorded backoff is told, or `None` once it has elapsed, which drops it.
    fn holding_off(&self, account_id: &str) -> Option<RefreshFailure> {
        let mut not_before = self.not_before.lock().expect(LOCK_POISONED);
        let (backoff, armed) = not_before.get(account_id).copied()?;
        if (self.clock)() < backoff.until {
            return Some(match armed {
                Armed::Refused => {
                    RefreshFailure::Unavailable("refresh is waiting out a backoff".into())
                }
                Armed::Uncertain => RefreshFailure::Quarantined,
            });
        }
        not_before.remove(account_id);
        None
    }

    fn arm_backoff(&self, account_id: &str, delay_ms: i64, armed: Armed) {
        let length_ms = delay_ms.clamp(0, MAX_NOT_BEFORE_MS);
        let until = (self.clock)().saturating_add(length_ms);
        self.not_before.lock().expect(LOCK_POISONED).insert(
            account_id.to_string(),
            (Backoff { until, length_ms }, armed),
        );
    }

    // A failed bookkeeping write must not mask why the refresh failed: the class is what the caller acts
    // on, and the next refresh derives the same state again from the store.
    fn resolve(&self, attempt: u64, incarnation: u64, outcome: AttemptOutcome) {
        tracing::debug!(
            outcome = outcome.word(),
            detail = outcome.detail(),
            "refresh attempt resolved"
        );
        let _ = self
            .store
            .resolve_refresh_attempt(attempt, incarnation, outcome);
    }

    /// Asks for re-pairing, unless the slot moved on: a verdict about credentials that a disconnect threw
    /// away, or that a re-pairing replaced without one, must not lock out what took their place, and neither
    /// may one about a holder the id no longer has. A re-pair keeps the lifecycle generation, so the bundle
    /// generation this refresh was started against is the half of the fence that catches it.
    ///
    /// A mark that lands takes the cached bundle with it and refuses the lineage: the store knows the state, but the
    /// rows the legs carry read `ready` until the next reload, and a miss that read the store would find the bundle
    /// the mark is about and serve it to its expiry. The landed mark is answered for the caller to report, since the
    /// store write alone starts no reload.
    fn mark_reauth(
        &self,
        account_id: &str,
        cause: StateCause,
        fence: Refused,
    ) -> Option<LandedMark> {
        let landed = self
            .store
            .set_state_if_generations(
                account_id,
                AccountState::ReauthRequired,
                cause,
                fence.incarnation,
                fence.lifecycle_generation,
                fence.credential_generation,
            )
            .is_ok_and(|landed| landed.is_some());
        if !landed {
            return None;
        }
        self.refuse(account_id, fence, None);
        Some(LandedMark {
            account_id: account_id.to_string(),
            state: AccountState::ReauthRequired,
            cause,
            incarnation: fence.incarnation,
            lifecycle_generation: fence.lifecycle_generation,
            credential_generation: fence.credential_generation,
        })
    }

    /// Drops the account's cached bundle and refuses `lineage`, so a miss on it answers the refusal rather than reading
    /// the refused bundle back from the store. With `since`, the epoch a caller read before its own store read, an
    /// invalidation that moved the epoch meanwhile leaves the slot as it is.
    fn refuse(&self, account_id: &str, lineage: Refused, since: Option<u64>) {
        let mut bundles = self.bundles.lock().expect(LOCK_POISONED);
        let slot = bundles.entry(account_id.to_string()).or_default();
        if since.is_some_and(|epoch| epoch != slot.epoch) {
            return;
        }
        slot.outrun();
        slot.refused = Some(lineage);
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
/// caller joined to one refresh; `Stale` and `Disconnected` are not faults at all, they mean the slot moved
/// while the refresh was in flight.
fn store_failure(error: StoreError) -> RefreshFailure {
    match error {
        StoreError::Stale | StoreError::Disconnected => RefreshFailure::Disconnected,
        other => RefreshFailure::Unavailable(format!("store: {other}")),
    }
}

/// The states that hand out no credential, refused before any bundle is read. A refresh answers its own failure
/// class for them and lets an unpaired row through to the credential read, so it keeps its own match.
fn refuse_unusable(account: &AccountRow) -> Result<(), AcquireError> {
    match account.state {
        AccountState::Disconnected => Err(AcquireError::Disconnected),
        AccountState::ReauthRequired => Err(AcquireError::ReauthRequired(None)),
        AccountState::Unpaired => Err(AcquireError::Unpaired),
        _ => Ok(()),
    }
}

/// Whether a bundle whose token names the upstream account `account_uuid` may be committed against the paired
/// identity. `account_uuid` is optional because a refresh response may omit the whole `account` object.
///
/// Only the account uuid decides. An email address and an organization can both change while the paired
/// upstream account stays the same, so comparing them would quarantine accounts over a rename or an org
/// move. The uuid is the one stable identifier of the pairing. A token that names no account cannot
/// contradict it and is accepted.
fn identity_matches(stored: &Identity, account_uuid: Option<&str>) -> bool {
    account_uuid.is_none_or(|uuid| uuid == stored.account_uuid)
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
/// `Decode`, `Io`, `Response`, `Stalled`, and `TooLarge`, can all happen after the request reached the upstream
/// intact, which makes the response, not the request, the part that went missing.
fn transport_before_write(error: &TransportError) -> bool {
    matches!(
        error,
        TransportError::Boring(_) | TransportError::Handshake(_) | TransportError::Unreachable(_)
    )
}

/// Lands `message` as the credential's last error. An account stuck on the same failure for hours must not cost a store
/// write per request it is handed, so only a message that changed is written.
fn record_failure(store: &Store, account_id: &str, incarnation: u64, message: &str) {
    let unchanged = store
        .credential_metadata(account_id)
        .ok()
        .flatten()
        .is_some_and(|metadata| metadata.last_error.as_deref() == Some(message));
    if !unchanged {
        if let Err(error) = store.record_credential_error(account_id, incarnation, message) {
            tracing::warn!(account = %account_id, %error, "could not record the credential failure");
        }
    }
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

    use fake_upstream::{FakeUpstream, Respond, Rule};
    use phoebis_profile::fixtures::{control_response, ControlFixture};
    use phoebis_store::{AccountKind, Identity, NewBundle, StorageKey};
    use phoebis_transport::control::{ResolveFuture, Resolver};
    use phoebis_transport::{connector_builder, Plane};

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
        let profile = phoebis_profile::claude_code_2_1_310_linux_x64();
        let mut builder = connector_builder(profile, Plane::Control).unwrap();
        phoebis_transport::trust_extra_ca(&mut builder, up.ca_pem()).unwrap();
        let addr = up.addr();
        let resolver: Resolver =
            Arc::new(move |_| Box::pin(async move { Some(addr) }) as ResolveFuture);
        let control = Arc::new(ControlClient::new(profile, builder.build(), resolver));
        let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
        store
            .insert_account(&AccountRow {
                identity: Some(Identity {
                    account_uuid: "00000000-0000-4000-8000-000000000001".into(),
                    email: "test@example.invalid".into(),
                    organization_uuid: None,
                }),
                ..AccountRow::for_tests("a", AccountKind::Oauth)
            })
            .unwrap();
        let clock: Clock = Arc::new(move || now);
        (
            CredentialManager::new(store.clone(), control, profile, clock),
            store,
            up,
        )
    }

    /// A refresh that never answers, in the set a drain awaits.
    fn stalled_refresh(manager: &CredentialManager) {
        let stalled: RefreshFuture = futures::future::pending().boxed().shared();
        manager
            .inner
            .inflight
            .lock()
            .unwrap()
            .insert("a".into(), stalled);
    }

    #[tokio::test(start_paused = true)]
    async fn a_drain_within_a_bound_shorter_than_the_refresh_timeout_releases_a_stalled_refresh_at_the_bound(
    ) {
        let (manager, _store, up) = manager_at(1_000_000).await;
        stalled_refresh(&manager);
        let start = tokio::time::Instant::now();
        manager.drain_within(Duration::from_secs(4)).await;
        assert_eq!(start.elapsed(), Duration::from_secs(4));
        up.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_drain_within_a_bound_past_the_refresh_timeout_is_cut_to_it() {
        let (manager, _store, up) = manager_at(1_000_000).await;
        stalled_refresh(&manager);
        let start = tokio::time::Instant::now();
        manager.drain_within(Duration::from_secs(300)).await;
        assert_eq!(start.elapsed(), REFRESH_TIMEOUT);
        up.shutdown().await;
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
            .commit_credentials("a", 1, 1, None, None, &bundle_at("observed", 0, 1))
            .unwrap();
        // What another caller's refresh does between the generation this one read and the slot it wins.
        store
            .commit_credentials(
                "a",
                1,
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

    #[tokio::test]
    async fn a_backoff_past_its_instant_reads_as_nothing_armed() {
        let now = 1_000_000;
        let (manager, _store, up) = manager_at(now).await;
        let arm = |until: i64| {
            manager.inner.not_before.lock().unwrap().insert(
                "a".into(),
                (
                    Backoff {
                        until,
                        length_ms: SERVER_ERROR_BACKOFF_MS,
                    },
                    Armed::Uncertain,
                ),
            );
        };
        arm(now + 1);
        assert_eq!(manager.backoff_until("a"), Some(now + 1));
        // A clock stepped to the instant ends the wait on it; the backoff still reads as the one armed.
        arm(now);
        assert_eq!(manager.backoff_until("a"), Some(now));
        // What a refresh that failed before `holding_off` leaves behind, since nothing dropped it.
        arm(now - 1);
        assert_eq!(manager.backoff("a"), None);
        assert_eq!(manager.backoff_until("a"), None);
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

    #[tokio::test]
    async fn a_bundle_at_a_refused_generation_is_neither_kept_nor_served() {
        let now = 1_000_000;
        let (manager, store, up) = manager_at(now).await;
        let generation = store
            .commit_credentials(
                "a",
                1,
                1,
                None,
                None,
                &bundle_at("refused", now, now + 3_600_000),
            )
            .unwrap();
        let account = store.account("a").unwrap().unwrap();
        let refused = Refused {
            incarnation: account.incarnation,
            lifecycle_generation: account.lifecycle_generation,
            credential_generation: generation,
        };
        let epoch = {
            let mut bundles = manager.inner.bundles.lock().unwrap();
            let slot = bundles.entry("a".into()).or_default();
            slot.refused = Some(refused);
            slot.epoch
        };
        let bundle = Arc::new(store.credentials("a").unwrap().unwrap());
        assert!(
            !manager.inner.remember(&account, bundle, epoch),
            "the bundle the mark is about is not served"
        );
        assert!(
            manager.inner.bundles.lock().unwrap()["a"].entry.is_none(),
            "nor kept"
        );
        up.shutdown().await;
    }
}
