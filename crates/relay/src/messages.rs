//! The Messages routes: admission against the registry, the account the router selects, the failover loop
//! across accounts, and the response the caller gets.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::response::Builder;
use axum::http::{
    header, header::CONTENT_ENCODING, HeaderMap, HeaderName, HeaderValue, StatusCode,
};
use axum::middleware::map_response;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, head, post};
use axum::{Json, Router};
use bytes::Bytes;
use futures::future::BoxFuture;
use phoebis_bus::{
    AccountSelected, AttemptOutcome, AutoMode, Bus, Classifier, ClientFacts, CooldownSet,
    DiagnosticsSource, Event, Fork, GuardrailMatch, Interruption, Kind, Origin, Outcome,
    RequestComplete, RequestObserved, RequestShape, ResponseChunk, ResponseFacts,
    ResponseHead as BusHead, Usage,
};
use phoebis_cch::Stamp;
use phoebis_guardrails::{evaluate_all, Evaluation, HeaderLookup, Lookup, RequestFacts};
use phoebis_oauth::{AccessToken, AcquireError, Clock, CredentialManager};
use phoebis_routing::{
    cohort_hold, latest_full_reset, resolve, AccountTicket, ChainFacts, ClientClass, CohortHold,
    ConversationLookup, EvidenceSource, Exhaustion, HoldFacts, HoldReason, JudgedCharge, KeyPermit,
    LaneFacts, OverageEvidence, PinRef, PinnedObstacle, PredecessorQuery, Presented, Refusal,
    RefusalFacts, RefusedRequest, Report, RequestFlags, ResolveError, RouterHandle, SelectRequest,
    SelectResult, Selection, SelectionTrace, BUDGET_WINDOW, MAIN_AGENT,
};
use phoebis_store::{
    AccountKind, AccountRow, AccountState, ApiKeyRow, CooldownRow, CooldownScope, MemoryStore,
    Secret, StateCause, Store,
};
use phoebis_transport::control::{ControlClient, Resolver};
use phoebis_transport::decode::{decoder, encoding, request_encoding, Decoder, Encoding};
use phoebis_transport::pool::{Conn, Pool, PoolKey};
use phoebis_transport::request::RawRequest;
use phoebis_transport::response::{framing, keep_alive, BodyReader, ResponseHead};
use phoebis_transport::TransportError;
use serde_json::json;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::auth::resolve_key;
use crate::billing::Restore;
use crate::body::{marker_ttls, AccountUuidEdit, RepeatedMember, VersionCheck};
use crate::budget::{BodyBudget, BodyReads};
use crate::casing::OriginalHead;
use crate::catalog::CatalogService;
use crate::chains::Chains;
use crate::classify::{
    classify, error_message, error_type, is_rate_limit_event, refused_injected, Cooldown,
    HeadContext, Scope, Verdict,
};
use crate::credit::{hex_of, CreditDigest, CreditStore};
use crate::dispatch::{self, until_canceled, AttemptFailure, Sent};
use crate::errors::{
    accounts_at_cap, body_budget_exhausted, body_over_budget, drift_words, key_body_reads,
    key_refusal, no_store_answer, relay_failure, Failure, FailureClass, ACCOUNT_CANCELED,
    CLIENT_GONE_CODE, GUARDRAIL_BLOCKED, METHOD_NOT_ALLOWED, MODEL_NOT_ALLOWED, NOT_FOUND,
    ROUTER_SHUTDOWN_CODE, ROUTER_UNAVAILABLE, UPSTREAM_UNREACHABLE,
};
use crate::exhaustion::{exhaustion_failure, exhaustion_response, pool_moved_failure};
use crate::facts::{
    element_count, AttributionLine, BodyFacts, ClientHints, MAX_BODY_ELEMENTS,
    TREE_BYTES_PER_ELEMENT,
};
use crate::headers::{outbound_headers, starts_with_ignoring_case, CredentialKind};
use crate::hold::{hold, Commit, Held, Inspect, INSPECT_BUFFER};
use crate::identity::{
    client_of, climb, contract_gap, identify, is_filed_word, restores_prev_req, PendingReports,
    RequestIdentity, PENDING_REPORT_WAIT,
};
use crate::memory::{
    uuid_v4, Injected, MemoryKey, Previous, SessionMemory, MEMORY_CAP, MEMORY_IDLE_MS,
};
use crate::observe::{answer_from_json, close_miss_reason, published_headers, wants_stream};
use crate::ratelimit::{head_sample, retry_after, HeadReading, UnifiedHeaders};
use crate::sse::SkippingParser;
use crate::stream::{
    drain_bounded, relay_body, AffinityFacts, BodyRead, Checkin, Cut, Drained, HeldText, OnError,
    OnMessageId, Publish, Relayed, Renewal, Waiting, MAX_BUFFERED_BODY,
};
use crate::{
    edit_body, BodyEdits, BodyMember, CacheTtl, CacheTtlPolicy, DriftKind, Markerless, NativeEdit,
    RelayError, SafeguardsEdit, TtlBranch, UpstreamGzip, DANGEROUS_TOOL_USE_BETA,
    EXTENDED_TTL_BETA,
};

/// How often the session memory's changes reach the store; a crash loses at most this much of it, and the
/// shutdown flushes the rest.
pub const MEMORY_FLUSH_INTERVAL: Duration = Duration::from_secs(5);

/// Taken off `head_timeout` for the one deadline the attempt loop runs under, so the exhaustion answer leaves
/// before the native client's own ten minutes run out.
const ATTEMPT_DEADLINE_MARGIN: Duration = Duration::from_secs(30);

/// The slice a held request sleeps between reads of the clock while it waits out an account's backoff.
const HOLD_TICK: Duration = Duration::from_millis(250);

/// How a wait on an account's backoff ended.
enum Waited {
    /// The account left the router meanwhile.
    Canceled,
    Cleared,
    /// The request's deadline came before the backoff cleared.
    Deadline,
    /// The request the wait was for answered or ended before its bound.
    Head,
}

/// Upstream response headers the relay keeps to itself: the connection and framing ones describe the upstream
/// hop, and the framing no longer matches the body the caller is sent; the upstream's own identity and cookies
/// name the account behind the key, never the key holder.
pub(crate) const NOT_FORWARDED: &[&str] = &[
    "anthropic-organization-id",
    "connection",
    "content-length",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "set-cookie",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// The upstream's rate-limit header families, hidden from every committed answer so an organization's standing
/// never reads as the key holder's own: the unified family and the API-key bucket family under the first
/// prefix, the fast-token family under the second. A rate limit keeps them, a 429 or a 200 whose first event is a
/// `rate_limit_error`, so the client's continue arms on the reset the upstream named.
const RATE_LIMIT_FAMILY_PREFIXES: [&str; 2] = [
    crate::ratelimit::BUCKET_PREFIX,
    crate::ratelimit::FAST_PREFIX,
];

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// The bytes of request bodies the process holds at once, across every connection.
    pub body_budget_bytes: u64,
    /// The longest a request's bodies may be kept, whatever a key or pool was left holding: an operator who
    /// lowers the global maximum means it from the next request, not from the next write of every stored row.
    pub body_capture_max_hours: u32,
    /// How long a request body may take to arrive after authentication.
    pub body_read_timeout: Duration,
    /// The bound on a connection's checkout, the connect and the TLS handshake; its timeout is `Unsent`.
    pub connect_timeout: Duration,
    /// How long an account is cooled when the upstream rejected it without saying for how long.
    pub cooldown_fallback: Duration,
    /// The whole of a domain check: its wait for a place, its send and its answer.
    pub domain_check_timeout: Duration,
    /// The domain checks one source address has in flight at one time.
    pub domain_checks_per_source: usize,
    /// The domain checks in flight at one time, whatever their source.
    pub domain_checks_total: usize,
    pub head_timeout: Duration,
    pub hold_timeout: Duration,
    /// The longest an upstream may go silent mid-stream before the relay gives up.
    pub idle_timeout: Duration,
    /// The bodies one key may have arriving at once.
    pub key_body_reads: u32,
    pub max_request_bytes: usize,
    /// How long an account is skipped for a model it answered 404 for.
    pub model_rejection: Duration,
    pub upstream_gzip: UpstreamGzip,
}

/// What one relayed request cost and how it ended, attached to the response for the accounting layer that
/// wraps the relay.
#[derive(Clone, Debug)]
pub struct RelayOutcome {
    pub request_id: String,
    pub key_id: Option<String>,
    pub pool_id: Option<String>,
    pub session_id: Option<String>,
    pub canonical_model: Option<String>,
    pub account_id: String,
    pub upstream_status: u16,
    /// How many times this request was submitted to an upstream, zero for one refused before it got that far.
    pub attempt: u8,
    /// Whether the request was written to the upstream in full, which is what decides if it may have been
    /// billed even when no response came back.
    pub written: bool,
    /// `Hash` when the caller stamped a billing hash this build cannot reproduce, `VersionSuffix` when its
    /// `cc_version` suffix does not reproduce from the prompt and the user agent; in either case nothing was
    /// sent upstream.
    pub client_drift: Option<DriftKind>,
    /// The item a `client_contract` refusal named, `None` for every other request.
    pub client_contract: Option<&'static str>,
    pub client_version: Option<String>,
    pub error_code: Option<String>,
    /// The outcome a local answer files under, set beside `error_code` by the answer that names the code. `None` for
    /// an answer the upstream's status decides.
    pub class: Option<Outcome>,
    pub pin_epoch: Option<u64>,
    pub received_at: i64,
    /// When the head that answered the caller arrived, which a local answer never has.
    pub head_at: Option<i64>,
    /// The upstream's own `request-id` response header on the committed attempt, `None` for a local answer or an
    /// upstream that sent none.
    pub upstream_request_id: Option<String>,
    /// The TTL the relay rewrote the body to on this attempt, in milliseconds; `None` when the body went as sent.
    pub cache_ttl_ms: Option<i64>,
    /// The head's unified `overage-in-use` flag on the committed attempt, the fact the ledger files the request's
    /// funding under; `None` for a local answer or a head without the unified headers.
    pub overage_in_use: Option<bool>,
    /// The served account's kind as the leg that answered stamped it at its selection, whether the upstream's head or
    /// the relay's own answer followed. It rides the outcome so a probe's credit files beside its request row off the
    /// answer alone, an account that leaves the registry while the probe is in flight included. `None` for an answer
    /// the relay built before any selection.
    pub account_kind: Option<AccountKind>,
    /// The served account's class as the leg stamped it beside `account_kind`, so a probe settles under the funding
    /// its request row takes. False for an answer the relay built before any selection.
    pub usage_based: bool,
    /// The source of the `diagnostics` member on the answering leg's body; `None` for an answer the relay built
    /// before any leg and for a leg that sent none.
    pub diagnostics: Option<DiagnosticsSource>,
    /// Whether that member's `previous_message_id` named a message. A null `diagnostics` member in the answer reads
    /// `none` only then, since a member that named no message gave the upstream nothing to compare.
    pub diagnostics_named: bool,
    /// The source of the thinking-binding beta on the answering leg's head, `None` for an answer the relay built
    /// before any leg and for a leg whose head went without it.
    pub thinking_diagnostics: Option<DiagnosticsSource>,
    /// The obstacle the keepalive holds a probe's agent on rather than pausing it as an exhausted pool, `None`
    /// otherwise.
    pub obstacle: Option<ProbeObstacle>,
    /// The holds this request waited through, the summary `phoebis_routing::cohort_hold` gives; `None` for none.
    pub held: Option<CohortHold>,
    /// The router's answer on a classifier call's charge for the leg that answered (`Judged::charge`), which the
    /// completion names `client_off` where it reads `Off`. `None` for every other request and for an answer the
    /// relay built before any leg.
    pub judged: Option<JudgedCharge>,
}

impl RelayOutcome {
    /// Writes `error_code` and `class` together, so no local answer files a code under the upstream's status.
    fn answered(&mut self, code: &str, class: Outcome) {
        self.error_code = Some(code.to_string());
        self.class = Some(class);
    }
}

/// The obstacle `RelayOutcome` names for a probe the keepalive holds on rather than pausing as an exhausted pool.
/// The selector's pinned standings (`PinnedObstacle`) keep their words: `attempted`, `budget_unknown`, `no_pin`,
/// `operator` and `subscription_ended`, each answered untimed since no wait clears it, and `cooldown`, `credits`,
/// `overage` and `spent`, each answered as a wait. The relay adds three of its own: `budget` for a probe the selector
/// answered with the pinned member's spend budget window, `pool_moved` for a request whose key moved to another pool
/// between its admission and its selection, which the keepalive retries since the next probe admits under the new
/// pool, and `reauth_required` for a probe whose leg marked its account for re-authentication, which the keepalive
/// pauses under its authentication reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeObstacle {
    Attempted,
    Budget,
    BudgetUnknown,
    Cooldown,
    Credits,
    NoPin,
    Operator,
    Overage,
    PoolMoved,
    ReauthRequired,
    Spent,
    SubscriptionEnded,
}

impl From<PinnedObstacle> for ProbeObstacle {
    fn from(obstacle: PinnedObstacle) -> ProbeObstacle {
        match obstacle {
            PinnedObstacle::Attempted => ProbeObstacle::Attempted,
            PinnedObstacle::BudgetUnknown => ProbeObstacle::BudgetUnknown,
            PinnedObstacle::Cooldown { .. } => ProbeObstacle::Cooldown,
            PinnedObstacle::Credits => ProbeObstacle::Credits,
            PinnedObstacle::NoPin => ProbeObstacle::NoPin,
            PinnedObstacle::Operator => ProbeObstacle::Operator,
            PinnedObstacle::Overage { .. } => ProbeObstacle::Overage,
            PinnedObstacle::Spent { .. } => ProbeObstacle::Spent,
            PinnedObstacle::SubscriptionEnded => ProbeObstacle::SubscriptionEnded,
        }
    }
}

/// Marks a request the keepalive engine sent on a session's behalf, and names the key it runs under. The
/// engine never holds a key's secret (the store keeps a digest), so a probe authenticates by id; the extension
/// can only be set in process, so the by-id path is not reachable from the socket.
#[derive(Clone, Debug)]
pub struct Probe {
    pub key_id: String,
    /// The generation of the key the recipe was retained under; the relay refuses a probe whose key now carries
    /// another one, so no probe crosses into a replacement key's pool.
    pub key_generation: i64,
}

pub struct Relay {
    accounting: Option<AccountingSource>,
    /// Every body task `relay_body` spawns runs under this, so a shutdown can wait for the completion a body
    /// task publishes after its connection was cut. A feature call's end publishes under it too.
    pub(crate) bodies: TaskTracker,
    pub(crate) body_budget: Arc<BodyBudget>,
    pub(crate) body_reads: BodyReads,
    pub(crate) bus: Option<Bus>,
    catalog: Arc<CatalogService>,
    pub(crate) clock: Clock,
    /// The client the feature calls of the control plane leave through. Without one each of them answers that the
    /// upstream did not answer.
    pub(crate) control: Option<Arc<ControlClient>>,
    /// The connections the pool opened for this relay's checkouts, as against the idle ones it reused.
    #[cfg(feature = "debug-hooks")]
    connections_opened: std::sync::atomic::AtomicUsize,
    /// The issuer of each fallback credit token a refusal carried, by the token's digest, which places the next body
    /// carrying the token on the account whose organization takes it.
    credits: Arc<CreditStore>,
    /// The decoders built for the hold's reads of a response.
    #[cfg(feature = "debug-hooks")]
    pub(crate) decoders_built: Arc<std::sync::atomic::AtomicU64>,
    /// The feature CA's certificate. Without one the download's path is an unknown route, which a launcher reads as a
    /// router without the features.
    feature_ca: Option<Bytes>,
    heads: Arc<Heads>,
    pub(crate) limits: Limits,
    pub(crate) manager: Arc<CredentialManager>,
    memory: Arc<SessionMemory>,
    /// The sequence of the affinity report each conversation's last answer enqueued, which the next turn's
    /// climb waits on.
    pending_reports: Arc<PendingReports>,
    pub(crate) pool: Arc<Pool>,
    /// The pinned profile's name, for the drift warn the operator reads; empty where nothing names one.
    profile_name: String,
    readiness: Option<ReadinessSource>,
    pub(crate) resolver: Resolver,
    router: RouterHandle,
    /// The API keys legs dispatch with, by account id, so a leg reads no registry while its row's lineage
    /// and the snapshot's generation hold.
    secrets: Mutex<HashMap<String, SecretEntry>>,
    shutdown_mark: Option<ShutdownMark>,
    /// `proxy.sign_cch_placeholder`: whether a restored `cch=00000` is signed rather than sent as the placeholder.
    sign_cch_placeholder: bool,
    store: Arc<Store>,
}

/// A cached API key and the lineage it was read under: the holder's incarnation and lifecycle generation
/// beside the credential generation, since a deleted and recreated id and a disconnected and reconnected one both
/// restart the generation at 1, and an entry keyed on the generation alone would serve the old holder's secret.
struct SecretEntry {
    incarnation: u64,
    lifecycle_generation: u64,
    generation: u64,
    secret: Secret,
}

// A poisoned cache mutex means a prior panic happened mid-update, so an entry may not describe its secret.
const SECRETS_POISONED: &str = "relay secret cache mutex poisoned by a prior panic";

const COMPLETION_POISONED: &str = "completion lock poisoned";

/// The components `/api/ready` names dead, read on every ask. A closure rather than the handles themselves, so
/// the relay needs no dependency on the crate that composes them.
pub type ReadinessSource = Arc<dyn Fn() -> Vec<&'static str> + Send + Sync>;

/// The ledger's standing at admission, read on every inference, count-tokens and probe request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Accounting {
    pub behind: bool,
}

/// What admission reads the ledger's standing through: a closure, as `ReadinessSource` is, so the relay needs no
/// dependency on the ledger crate. The argument is true for the reading that decides a `ledger_behind` answer, which
/// the ledger counts, and false for one taken at a refusal of another kind.
pub type AccountingSource = Arc<dyn Fn(bool) -> Accounting + Send + Sync>;

/// Set by the router before it aborts the proxy listener, and by the listener before it cuts what is still open at its
/// own drain deadline, so an end either cut causes is booked as the router's own cut rather than the caller's leaving.
/// Memory, read on the end's path alone.
pub type ShutdownMark = Arc<AtomicBool>;

/// `router_shutdown` while the mark is set, else `client_gone`.
pub(crate) fn cut_code(mark: Option<&ShutdownMark>) -> &'static str {
    match mark {
        Some(mark) if mark.load(Ordering::SeqCst) => ROUTER_SHUTDOWN_CODE,
        _ => CLIENT_GONE_CODE,
    }
}

impl Relay {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<Store>,
        manager: Arc<CredentialManager>,
        pool: Arc<Pool>,
        router: RouterHandle,
        resolver: Resolver,
        clock: Clock,
        limits: Limits,
        catalog: Arc<CatalogService>,
    ) -> Relay {
        Relay {
            accounting: None,
            bodies: TaskTracker::new(),
            body_budget: BodyBudget::new(limits.body_budget_bytes),
            body_reads: BodyReads::default(),
            bus: None,
            catalog,
            clock,
            control: None,
            #[cfg(feature = "debug-hooks")]
            connections_opened: std::sync::atomic::AtomicUsize::new(0),
            credits: Arc::new(CreditStore::new()),
            #[cfg(feature = "debug-hooks")]
            decoders_built: Arc::default(),
            feature_ca: None,
            heads: Arc::new(Mutex::new(HashMap::new())),
            limits,
            manager,
            memory: Arc::new(SessionMemory::new(MEMORY_CAP, MEMORY_IDLE_MS)),
            pending_reports: Arc::new(PendingReports::default()),
            pool,
            profile_name: String::new(),
            readiness: None,
            resolver,
            router,
            secrets: Mutex::new(HashMap::new()),
            shutdown_mark: None,
            sign_cch_placeholder: false,
            store,
        }
    }

    pub fn catalog(&self) -> &Arc<CatalogService> {
        &self.catalog
    }

    pub(crate) fn router(&self) -> &RouterHandle {
        &self.router
    }

    pub(crate) fn shutdown_mark(&self) -> Option<ShutdownMark> {
        self.shutdown_mark.clone()
    }

    /// The live requests the head map holds an entry for, so a test can tell that none outlives its request.
    #[cfg(feature = "debug-hooks")]
    pub fn heads_pending(&self) -> usize {
        self.heads.lock().expect(HEADS_POISONED).len()
    }

    /// The connections the relay opened fresh (a checkout the pool did not reuse), for a test that counts them
    /// without reading the fake upstream's accepts.
    #[cfg(feature = "debug-hooks")]
    pub fn connections_opened(&self) -> usize {
        self.connections_opened.load(Ordering::Relaxed)
    }

    /// Counts a checkout the pool answered with a connection it opened, which the test hooks alone read.
    pub(crate) fn note_opened(&self) {
        #[cfg(feature = "debug-hooks")]
        self.connections_opened.fetch_add(1, Ordering::Relaxed);
    }

    /// A decoder for the relay's own read of a response body, the hold's inspection copy.
    fn copy_decoder(&self, coding: Encoding) -> Box<dyn Decoder> {
        #[cfg(feature = "debug-hooks")]
        self.decoders_built.fetch_add(1, Ordering::Relaxed);
        decoder(coding)
    }

    /// Attaches the bus every relayed request is published to. Without one the relay publishes nothing.
    pub fn with_bus(self, bus: Bus) -> Relay {
        Relay {
            bus: Some(bus),
            ..self
        }
    }

    /// The control client the feature forward sends its control-plane rows through.
    pub fn with_control(self, control: Arc<ControlClient>) -> Relay {
        Relay {
            control: Some(control),
            ..self
        }
    }

    /// The feature CA's certificate, which `GET /v0/features/ca.pem` then serves without a key.
    pub fn with_feature_ca(self, pem: Bytes) -> Relay {
        Relay {
            feature_ca: Some(pem),
            ..self
        }
    }

    /// Attaches the shutdown mark. Without one every closed channel reads as the caller's leaving.
    pub fn with_shutdown_mark(self, mark: ShutdownMark) -> Relay {
        Relay {
            shutdown_mark: Some(mark),
            ..self
        }
    }

    /// What `/api/ready` reads; without it the route answers ready, which the relay's own tests rely on.
    pub fn with_readiness(self, source: ReadinessSource) -> Relay {
        Relay {
            readiness: Some(source),
            ..self
        }
    }

    /// What admission reads the ledger's standing through; without it every request is admitted, which the
    /// relay's own tests rely on.
    pub fn with_accounting(self, source: AccountingSource) -> Relay {
        Relay {
            accounting: Some(source),
            ..self
        }
    }

    /// `proxy.sign_cch_placeholder`.
    pub fn with_signed_placeholder(self, on: bool) -> Relay {
        Relay {
            sign_cch_placeholder: on,
            ..self
        }
    }

    /// The pinned profile's name, for the drift warn the operator reads.
    pub fn with_profile_name(self, name: &str) -> Relay {
        Relay {
            profile_name: name.to_string(),
            ..self
        }
    }

    /// The session memory's bounds from the bootstrap file: the entries held in process and how long one may
    /// go unseen before it is dropped from memory.
    pub fn with_session_memory(self, cap: usize, idle: Duration) -> Relay {
        Relay {
            memory: Arc::new(SessionMemory::new(cap, idle.as_millis() as i64)),
            ..self
        }
    }

    /// Tier 2 of the session memory and how long its rows are kept past a session's last request. The map
    /// stays the truth; the store takes its changes behind, on `flush_memory` and the task `spawn_memory_flush`
    /// runs, and answers a miss the request path fills before it reads the key.
    pub fn with_memory_store(self, store: Arc<dyn MemoryStore>, retention: Duration) -> Relay {
        let memory = SessionMemory::new(self.memory.cap(), self.memory.idle_ms())
            .with_store(store, retention.as_millis() as i64);
        Relay {
            memory: Arc::new(memory),
            ..self
        }
    }

    /// Writes every entry changed since the last flush that landed; the binary calls it once at shutdown,
    /// after the periodic task is joined.
    pub async fn flush_memory(&self) {
        self.memory.flush((self.clock)()).await;
    }

    /// Closes the body-task tracker and waits for every task already spawned under it, so a shutdown gives a
    /// body cut mid-stream the chance to publish its completion before the ledger and the bus close.
    pub async fn drain_bodies(&self) {
        self.bodies.close();
        self.bodies.wait().await;
    }

    /// Flushes every `every` and whenever the memory's overlay asks for one ahead of the tick, and once an
    /// hour drops the rows past the retention; aborted and joined at shutdown before the last flush.
    pub fn spawn_memory_flush(self: Arc<Self>, every: Duration) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(every);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut ticks: u64 = 0;
            loop {
                // The hourly purge counts ticks alone: a flush the overlay asked for says nothing about how
                // much time has passed, and counting it would run the purge early and ever earlier.
                let ticked = tokio::select! {
                    _ = interval.tick() => true,
                    () = self.memory.flush_wanted() => false,
                };
                self.flush_memory().await;
                if !ticked {
                    continue;
                }
                ticks += 1;
                let per_hour = (3_600 / every.as_secs().max(1)).max(1);
                if ticks.is_multiple_of(per_hour) {
                    self.memory.purge_store((self.clock)()).await;
                }
            }
        })
    }

    /// A keepalive probe: the recipe's headers and body, run through the same admission, selection, edit, and
    /// failover loop a real request takes, so the probe follows its pin and renews it as a real request would, and
    /// never moves it.
    pub async fn probe(&self, probe: Probe, mut request: Request) -> Response {
        request.extensions_mut().insert(probe);
        self.handle(request).await
    }

    /// Read-only test support: the previous-request record the response half maintains for `(key, session,
    /// agent)` under one account, the same lookup the failover loop reads for `cc_prev_req`. Not part of the
    /// relay's operational surface; the restoration suite uses it to verify the response id and the message id
    /// are both retrievable after a commit, end to end rather than through a unit test of the memory alone.
    #[doc(hidden)]
    pub fn previous(&self, key: &MemoryKey, account_id: &str, canonical_model: &str) -> Previous {
        self.memory.previous(key, account_id, canonical_model)
    }
}

/// The bus-facing side of one request: the identity every event of the request carries, and the instant the
/// timeline is measured from. Every method is a no-op without a bus.
#[derive(Clone)]
pub(crate) struct Trace {
    bus: Option<Bus>,
    pub request_id: String,
    pub received_at: i64,
    kind: Kind,
    probe_generation: Option<i64>,
    /// The holds waited through so far. Kept here rather than threaded to each outcome, because every answer
    /// after a hold carries it: a served attempt, a local answer on the leg, and an exhaustion that dispatched
    /// nothing, which no attempt row could record.
    held: Arc<Mutex<Option<CohortHold>>>,
    /// Set for a request refused while the ledger is behind: its facts go out best-effort, since a writer that
    /// stopped draining fills the bus queue behind its channel and a refusal that waited there would hold its
    /// connection. Shared across the trace's clones, which the body task and the awaits each hold.
    facts_best_effort: Arc<AtomicBool>,
    /// The relay's clock, which stamps a completion the guard's drop publishes.
    clock: Clock,
    /// The relay's body tracker, under which the guard's drop spawns its publish, so the shutdown's drain waits for
    /// the completion a handler it cut still owes the ledger.
    bodies: TaskTracker,
    /// The handler's armed guard. The handler's own trace holds the slot, and the stream body's copy is detached
    /// after the hand-off emptied it, so the handler future's drop is the guard's drop.
    completion: Arc<Mutex<Option<CompletionGuard>>>,
    /// The relay's shutdown mark, which the stream body and the guard read when they book the end.
    pub shutdown_mark: Option<ShutdownMark>,
}

impl Trace {
    pub(crate) fn bus_attached(&self) -> bool {
        self.bus.is_some()
    }

    fn waited(&self, holds: &[HoldFacts]) {
        *self.held.lock().expect("hold lock poisoned") = cohort_hold(holds);
    }

    fn held(&self) -> Option<CohortHold> {
        self.held.lock().expect("hold lock poisoned").clone()
    }

    fn origin(&self) -> Origin {
        match self.kind {
            Kind::Probe => Origin::Keepalive,
            Kind::CountTokens | Kind::Inference => Origin::Client,
        }
    }

    fn publish(&self, event: Event) {
        if let Some(bus) = &self.bus {
            bus.publish(event);
        }
    }

    /// Waits for the bus's capacity rather than dropping: a fact this request is accounted by must land, or
    /// the row for it can never be written. A refusal's facts while the ledger is behind are the exception: no
    /// money moved, so a lost one costs a row and is counted where the bus counts a drop.
    async fn publish_fact(&self, event: Event) {
        if let Some(bus) = &self.bus {
            if self.facts_best_effort.load(Ordering::Relaxed) {
                bus.publish(event);
            } else {
                bus.publish_fact(event).await;
            }
        }
    }

    fn facts_best_effort(&self) {
        self.facts_best_effort.store(true, Ordering::Relaxed);
    }

    /// Called by `observed` right after it published the request's first fact. A second call keeps the guard
    /// already armed, whose drop would otherwise publish a completion for a request still running.
    fn arm_completion(&self, key: Option<&ApiKeyRow>) {
        self.completion
            .lock()
            .expect(COMPLETION_POISONED)
            .get_or_insert_with(|| CompletionGuard::arm(self, key));
    }

    /// A no-op once the guard left the slot.
    fn with_guard(&self, f: impl FnOnce(&mut CompletionGuard)) {
        if let Some(guard) = self.completion.lock().expect(COMPLETION_POISONED).as_mut() {
            f(guard);
        }
    }

    fn on_leg(&self, attempt: u8, account_id: &str) {
        self.with_guard(|guard| guard.on_leg(attempt, account_id));
    }

    fn on_head(&self, judged: Option<JudgedCharge>) {
        self.with_guard(|guard| guard.on_head(judged));
    }

    /// The armed guard, by value, leaving the slot empty.
    fn take_completion(&self) -> Option<CompletionGuard> {
        self.completion.lock().expect(COMPLETION_POISONED).take()
    }

    /// A clone with an empty slot of its own.
    fn detached(&self) -> Trace {
        Trace {
            completion: Arc::new(Mutex::new(None)),
            ..self.clone()
        }
    }

    pub(crate) fn publish_chunk(&self, chunk: ResponseChunk) {
        self.publish(Event::ResponseChunk(chunk));
    }

    /// Published exactly once per request by the handler, as soon as it knows as much of the identity as the
    /// request will ever yield. A refusal before the key was read publishes with nothing but the id. `inflated`
    /// marks a body past the read, which is plaintext whatever coding the caller's `content-encoding` named, so
    /// that line is not published beside it.
    #[allow(clippy::too_many_arguments)]
    async fn observed(
        &self,
        key: Option<&ApiKeyRow>,
        identity: Option<&RequestIdentity>,
        headers: &HeaderMap,
        inflated: bool,
        body: Bytes,
        stream: bool,
        body_capture_hours: u32,
        shape: RequestShape,
        chains: Option<&Chains>,
        client: ClientFacts,
        auto_mode: Option<AutoMode>,
        presented_retired: bool,
        rotation_at: Option<i64>,
        guardrail_id: Option<String>,
        guardrails: Vec<GuardrailMatch>,
        fallback_digest: Option<CreditDigest>,
        cache_marker: bool,
    ) {
        if self.bus.is_none() {
            return;
        }
        self.publish_fact(Event::RequestObserved(RequestObserved {
            request_id: self.request_id.clone(),
            origin: self.origin(),
            kind: self.kind,
            key_id: key.map(|k| k.id.clone()),
            // A probe's row files under the generation its recipe was retained under, the same one its
            // keepalive deltas carry, so the two never disagree about whose probe it was.
            key_generation: self
                .probe_generation
                .unwrap_or_else(|| key.map_or(0, |k| k.created_at)),
            pool_id: key.map(|k| k.pool_id.clone()),
            // The client's own session, which a companion's keepalive recipe is registered under, so the recipe
            // keeps matching and arming.
            session_id: identity.and_then(|i| i.session_id.clone()),
            agent_id: identity.map(|i| i.agent_id.clone()),
            parent_agent_id: identity.and_then(|i| i.parent_agent_id.clone()),
            owner_id: identity.and_then(|i| i.owner_id.clone()),
            model: identity.map(|i| i.model.clone()).unwrap_or_default(),
            canonical_model: identity
                .map(|i| i.canonical_model.clone())
                .unwrap_or_default(),
            wire_model: identity.map(|i| i.folded_model.clone()).unwrap_or_default(),
            stream,
            headers: published_headers(headers, inflated),
            body,
            body_capture_hours,
            shape,
            static_hash: chains.and_then(Chains::static_hash).map(String::from),
            boundaries: chains
                .map(|c| c.message_boundaries.clone())
                .unwrap_or_default(),
            marked: chains.map(|c| c.marked.clone()).unwrap_or_default(),
            client,
            auto_mode,
            fork: identity
                .map(|i| Fork {
                    forked_from: i.forked_from.clone(),
                    fork_depth: i.fork_depth,
                })
                .unwrap_or_default(),
            presented_retired,
            rotation_at,
            guardrail_id,
            guardrails,
            received_at: self.received_at,
            fallback_digest: fallback_digest.as_ref().map(hex_of),
            cache_marker,
        }))
        .await;
        // The assembler holds an entry for the request from this fact on, and only a completion closes it.
        self.arm_completion(key);
    }

    #[allow(clippy::too_many_arguments)]
    async fn selected(
        &self,
        attempt: u8,
        account: &AccountRow,
        usage_based: bool,
        pin_epoch: Option<u64>,
        started_at: i64,
        cache_ttl_ms: Option<i64>,
        trace: Option<&SelectionTrace>,
        previous_input_tokens: Option<u64>,
    ) {
        self.publish_fact(Event::AccountSelected(AccountSelected {
            request_id: self.request_id.clone(),
            attempt,
            account_id: account.id.clone(),
            account_kind: account.kind.as_str().into(),
            usage_based,
            pin_epoch,
            started_at,
            cache_ttl_ms,
            selection: trace.and_then(|t| serde_json::to_string(t).ok()),
            previous_input_tokens,
        }))
        .await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn head(
        &self,
        attempt: u8,
        head: &ResponseHead,
        first_event: Option<Bytes>,
        outcome: AttemptOutcome,
        cooldown: Option<CooldownSet>,
        written_at: i64,
        observed_at: i64,
    ) {
        self.publish_fact(Event::ResponseHead(BusHead {
            request_id: self.request_id.clone(),
            attempt,
            status: head.status,
            headers: head.headers.clone(),
            first_event,
            outcome,
            cooldown,
            written_at,
            observed_at,
        }))
        .await;
    }

    /// The last event of a request. `status` is what the client was answered with, which is the upstream's
    /// only for a forwarded answer. `end` overrides the outcome derived from the `RelayOutcome` for a body that
    /// ended other than cleanly, with the code that names its cause. `upstream` is a forwarded failure's own word
    /// (`upstream_code`), the code only where the override and the outcome name none. It stays off `outcome`, whose
    /// code the keepalive reads for the relay's own words.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn complete(
        &self,
        outcome: &RelayOutcome,
        status: u16,
        usage: Option<Usage>,
        response: ResponseFacts,
        ttfc_ms: Option<i64>,
        response_bytes: Option<u64>,
        response_chunks: Option<u32>,
        end: Option<(Outcome, Option<String>)>,
        upstream: Option<String>,
        message_id: Option<String>,
        skipped_events: u32,
        interruption: Option<Interruption>,
        finished_at: i64,
    ) {
        // Held by value across the publish: a handler dropped while a durable slot is full drops the guard still
        // armed, and its `Drop` publishes the completion this await never delivered.
        let guard = self.take_completion();
        let (final_outcome, error_code) = match end {
            Some((o, code)) => (o, code.or_else(|| outcome.error_code.clone())),
            None => (outcome_of(outcome), outcome.error_code.clone()),
        };
        let error_code = error_code.or(upstream);
        self.publish_fact(Event::RequestComplete(RequestComplete {
            request_id: self.request_id.clone(),
            key_id: outcome.key_id.clone(),
            pool_id: outcome.pool_id.clone(),
            origin: self.origin(),
            kind: self.kind,
            attempt: outcome.attempt,
            account_id: (!outcome.account_id.is_empty()).then(|| outcome.account_id.clone()),
            outcome: final_outcome,
            status,
            usage: usage
                .map(|mut usage| {
                    close_miss_reason(&mut usage, outcome.diagnostics.is_some());
                    usage
                })
                .filter(|u| !u.is_empty()),
            response,
            ttfb_ms: outcome.head_at.map(|at| at - outcome.received_at),
            ttfc_ms,
            error_code,
            client_drift: outcome.client_drift.map(|k| k.as_str().to_string()),
            client_contract: outcome.client_contract.map(str::to_string),
            client_version: outcome.client_version.clone(),
            response_bytes,
            response_chunks,
            held: outcome
                .held
                .as_ref()
                .and_then(|h| serde_json::to_string(h).ok()),
            diagnostics: outcome.diagnostics,
            thinking_diagnostics: outcome.thinking_diagnostics,
            message_id,
            skipped_events,
            interruption,
            classifier: client_off(outcome.judged),
            finished_at,
        }))
        .await;
        if let Some(mut guard) = guard {
            guard.disarm();
        }
    }
}

/// Armed once the request's first fact opened the assembler's entry, disarmed by `Trace::complete` once its
/// awaited publish returns or by the hand-off to the stream body, which completes on its own path. A drop while
/// armed is a client that left, or the router's shutdown that cut it, before the completion landed: hyper dropped
/// the handler future, or the keepalive dropped a probe's exchange at its deadline or its cancel, so the completion
/// is published from here, with what the handler knew (the attempt it was on and the account of the leg in flight),
/// and the entry lands rather than aging out as a `ledger_gap`.
struct CompletionGuard {
    /// `Trace::detached`: the guard sits in the trace's slot, so a clone holding that slot would keep itself alive.
    trace: Trace,
    /// The key and its pool `observed` published, which a cut request's completion repeats, since the drop has no
    /// outcome to read them from.
    key_id: Option<String>,
    pool_id: Option<String>,
    /// The last leg a `Responding` named, `(attempt, account_id, judged)`, and `(0, None, None)` before the first,
    /// `judged` being that leg's charge answer once the leg answers the client and `None` until then. A client that
    /// leaves between legs or during the `selected` fact's await is completed on it.
    leg: (u8, Option<String>, Option<JudgedCharge>),
    armed: bool,
}

impl CompletionGuard {
    fn arm(trace: &Trace, key: Option<&ApiKeyRow>) -> CompletionGuard {
        CompletionGuard {
            trace: trace.detached(),
            key_id: key.map(|k| k.id.clone()),
            pool_id: key.map(|k| k.pool_id.clone()),
            leg: (0, None, None),
            armed: true,
        }
    }

    fn on_leg(&mut self, attempt: u8, account_id: &str) {
        self.leg = (attempt, Some(account_id.to_string()), None);
    }

    /// The charge answer of the leg that now answers the client. A leg whose head never answered it leaves none, so a
    /// call cut before then completes with the observation's word.
    fn on_head(&mut self, judged: Option<JudgedCharge>) {
        self.leg.2 = judged;
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

/// The completion's word for a classifier call the router answered off on the leg that answered, `None` for every
/// other request, whose row keeps the observation's word.
fn client_off(judged: Option<JudgedCharge>) -> Option<Classifier> {
    (judged == Some(JudgedCharge::Off)).then_some(Classifier::ClientOff)
}

impl Drop for CompletionGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let (attempt, account_id, judged) = std::mem::take(&mut self.leg);
        let event = Event::RequestComplete(RequestComplete {
            request_id: self.trace.request_id.clone(),
            key_id: self.key_id.take(),
            pool_id: self.pool_id.take(),
            origin: self.trace.origin(),
            kind: self.trace.kind,
            attempt,
            account_id,
            outcome: Outcome::Canceled,
            status: 0,
            usage: None,
            response: ResponseFacts::default(),
            ttfb_ms: None,
            ttfc_ms: None,
            error_code: Some(cut_code(self.trace.shutdown_mark.as_ref()).into()),
            client_drift: None,
            client_contract: None,
            client_version: None,
            response_bytes: None,
            response_chunks: None,
            held: self
                .trace
                .held()
                .and_then(|h| serde_json::to_string(&h).ok()),
            diagnostics: None,
            thinking_diagnostics: None,
            message_id: None,
            skipped_events: 0,
            interruption: None,
            classifier: client_off(judged),
            finished_at: (self.trace.clock)(),
        });
        // `Drop` cannot await, so the durable publish runs on the runtime that is dropping the handler, under the body
        // tracker the shutdown drains. With no handle current (a trace dropped outside a runtime) the bus's counted
        // drop is the fallback.
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let trace = self.trace.clone();
                self.trace
                    .bodies
                    .spawn_on(async move { trace.publish_fact(event).await }, &handle);
            }
            Err(_) => self.trace.publish(event),
        }
    }
}

/// The first member admission finds spelled twice: a top-level member, then the billing line's `text`, then
/// `metadata.user_id`.
fn repeated_member(
    members: &phoebis_cch::TopLevelMembers<'_>,
) -> Result<Option<RepeatedMember>, phoebis_cch::CchError> {
    let top_level = [
        RepeatedMember::Metadata,
        RepeatedMember::Model,
        RepeatedMember::Stream,
        RepeatedMember::System,
    ];
    if let Some(index) = members.repeated_top_level_member(&top_level.map(RepeatedMember::path))? {
        return Ok(Some(top_level[index]));
    }
    if members.first_system_block_member_count("text")? > 1 {
        return Ok(Some(RepeatedMember::BillingLine));
    }
    Ok(members
        .repeated_member_within("metadata", &["user_id"])?
        .map(|_| RepeatedMember::MetadataUserId))
}

/// The class and hint facts `observed` publishes, built once per call: the class from the identity already
/// derived it, or read fresh for a request refused before it had one; the turn origin from the body, once it was
/// read.
fn client_facts(
    headers: &HeaderMap,
    hints: &ClientHints,
    identity: Option<&RequestIdentity>,
    facts: Option<&BodyFacts>,
) -> ClientFacts {
    let (class, label) = match identity {
        Some(identity) => (identity.class, identity.client.clone()),
        None => {
            let client = client_of(headers);
            (client.class, client.label)
        }
    };
    // The pair marks under the Claude Code class alone: another client carrying it would hide its share from
    // the prediction and the shedding, where the contract binds Claude Code alone.
    let classifier = (class == ClientClass::ClaudeCode
        && hints.request_class.as_deref() == Some("auxiliary")
        && hints.classifier_beta)
        .then_some(Classifier::Client);
    ClientFacts {
        class: label,
        agent_type: hints.agent_type.clone(),
        classifier,
        compaction: hints.compaction.clone(),
        context_compacted: hints.context_compacted.clone(),
        prev_tool_ms: hints.prev_tool_ms,
        request_class: hints.request_class.clone(),
        turn_origin: facts.and_then(|f| f.turn_origin.clone()),
    }
}

/// The live request's headers as the guardrail evaluator reads them. Every header is in hand, so a name answers
/// its first value or absent and never unknown, and a value that is not visible ASCII reads as absent.
struct HeaderMapLookup<'a>(&'a HeaderMap);

impl HeaderLookup for HeaderMapLookup<'_> {
    fn get(&self, name: &str) -> Lookup<'_> {
        self.0
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map_or(Lookup::Absent, Lookup::Value)
    }
}

/// The declared body length, when the caller sent one the relay can read; a chunked body has none.
pub(crate) fn content_length(headers: &HeaderMap) -> Option<usize> {
    headers
        .get(header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// The coding of the caller's body, `None` for one the relay refuses: a token other than `gzip` or `identity`, a
/// value that is not visible ASCII, or more than one line, which leaves the coding ambiguous.
fn request_coding(headers: &HeaderMap) -> Option<Encoding> {
    let mut lines = headers.get_all(CONTENT_ENCODING).iter();
    match (lines.next(), lines.next()) {
        (None, _) => Some(Encoding::Identity),
        (Some(line), None) => request_encoding(Some(line.to_str().ok()?)).ok(),
        (Some(_), Some(_)) => None,
    }
}

/// A gzip request body inflated within `bound`, the per-body bound the read held the compressed bytes to: past
/// it is the 413 an oversize body is, and anything else the decoder refuses is a body that is not gzip.
fn inflate_request(raw: &[u8], bound: usize) -> Result<Vec<u8>, RelayError> {
    let refused = |error: TransportError| match error {
        TransportError::TooLarge(_) => RelayError::BodyTooLarge,
        _ => RelayError::BodyNotGzip,
    };
    let mut decoder = decoder(Encoding::Gzip);
    let mut plain = Vec::new();
    decoder.feed_into(&mut plain, raw, bound).map_err(refused)?;
    decoder.finish_into(&mut plain, bound).map_err(refused)?;
    Ok(plain)
}

/// The request outcome an answer implies: a local answer's class, else the upstream's status. A body that ends badly
/// overrides it at the end of the body.
fn outcome_of(outcome: &RelayOutcome) -> Outcome {
    match outcome.class {
        Some(class) => class,
        None if outcome.upstream_status < 400 => Outcome::Succeeded,
        None => Outcome::Failed,
    }
}

/// The upstream's word for an answer forwarded with its `status`, `None` below 400: the `error.type` that `body`
/// names where it passes `is_filed_word`, else `upstream_<status>`. The word becomes a map key and a filter value,
/// so a type outside that bound files the fallback.
pub(crate) fn upstream_code(status: u16, body: Option<&[u8]>) -> Option<String> {
    (status >= 400).then(|| {
        body.and_then(error_type)
            .filter(|word| is_filed_word(word))
            .unwrap_or_else(|| format!("upstream_{status}"))
    })
}

/// The attempt outcome the verdict on a head implies. `rate_limit_event` is a held stream whose first event is a
/// `rate_limit_error`.
fn attempt_outcome(verdict: &Verdict, status: u16, rate_limit_event: bool) -> AttemptOutcome {
    match verdict {
        Verdict::Refresh => AttemptOutcome::AuthFailed,
        Verdict::Commit if status < 400 => AttemptOutcome::Succeeded,
        Verdict::Commit => AttemptOutcome::UpstreamError,
        Verdict::FailOver { .. } if status == 429 || rate_limit_event => {
            AttemptOutcome::RateLimited
        }
        Verdict::FailOver {
            mark: Some((AccountState::ReauthRequired, _)),
            ..
        } => AttemptOutcome::AuthFailed,
        Verdict::FailOver { .. } => AttemptOutcome::UpstreamError,
        Verdict::StripInjected(_) => AttemptOutcome::UpstreamError,
    }
}

/// One leg's edited body, what the body editor restored, the beta items it asks the header builder to add, and
/// the TTL it coupled the body to (the fourth element), `None` off the policy.
type Built = Result<(Vec<u8>, Vec<Injected>, Vec<Injected>, Option<CacheTtl>), RelayError>;

/// The glue between what admission recorded and what this leg actually sends: builds a `Restore` from the
/// context every account shares, this leg's previous-request lookup, and the fields the account has refused, so
/// `Restore::skip` mirrors the same set `NativeEdit::diagnostics` is gated on.
fn restore_with<'a>(
    restore: &'a Option<RestoreContext>,
    previous: &'a Previous,
    skip: &BTreeSet<Injected>,
) -> Option<Restore<'a>> {
    restore.as_ref().map(|r| Restore {
        prompt_id: r.prompt_id.as_deref(),
        prev_req: r
            .prev_req
            .then_some(previous.request_id.as_deref())
            .flatten(),
        skip: skip.iter().copied().collect(),
    })
}

/// Whether a 400 on this request says anything about the account past this leg. A `count_tokens` request
/// injects nothing at all, so its refusal has nothing to latch; the strip retry still corrects this leg's own
/// edit.
fn latches(trace: &Trace) -> bool {
    trace.kind == Kind::Inference
}

/// Whether the `anthropic-beta` lines of a head the pass built name `beta`. The builder keeps each token the caller
/// sent apart from the betas it drops and the OAuth claim, so a token it did not add is the caller's.
fn lists_beta(headers: &[(String, String)], beta: &str) -> bool {
    headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
        .any(|(_, line)| line.split(',').any(|token| token.trim() == beta))
}

/// The source of a member or a beta on one pass. The router's half is read first, since the head a pass built lists a
/// beta the router added as well as one the client sent.
fn source_of(client: bool, router: bool) -> Option<DiagnosticsSource> {
    if router {
        Some(DiagnosticsSource::Router)
    } else if client {
        Some(DiagnosticsSource::Client)
    } else {
        None
    }
}

/// The TTL a renewal names when neither a rewrite nor a marker of the client's own says one.
const DEFAULT_CACHE_TTL_MS: i64 = CacheTtl::FiveMinutes.ms();

/// The wire value a coupled TTL dispatches as, for the bus and the outcome; `None` when the body went as sent.
fn cache_ttl_ms(ttl: Option<CacheTtl>) -> Option<i64> {
    ttl.map(CacheTtl::ms)
}

/// Whether the account pays for a cache write, which selects the cache TTL policy's metered set: an api_key account,
/// a usage-based seat and an account in overage do.
fn pays_for_writes(kind: AccountKind, usage_based: bool, in_overage: bool) -> bool {
    in_overage || usage_based || kind == AccountKind::ApiKey
}

/// The row a verdict's scope cools, a model scope naming the request's canonical model.
fn cooldown_scope(scope: Scope, canonical_model: &str) -> CooldownScope {
    match scope {
        Scope::Account => CooldownScope::Account,
        Scope::Fast => CooldownScope::Fast,
        Scope::FastCredits => CooldownScope::FastCredits,
        Scope::Model => CooldownScope::Model(canonical_model.to_string()),
    }
}

fn cooldown_set(cooldown: Option<&Cooldown>, canonical_model: &str) -> Option<CooldownSet> {
    cooldown.map(|c| CooldownSet {
        scope: cooldown_scope(c.scope, canonical_model).to_string(),
        until: c.until,
        reason: c.reason.to_string(),
        window: c.window.clone(),
    })
}

/// The relayed surface. A known path asked with an unsupported method answers an Anthropic-shaped 405 rather
/// than the 404 an unknown path gets, so a client can tell a wrong method from a wrong route.
pub fn router(relay: Arc<Relay>) -> Router {
    // The listing answers the caller's key with what its pool may route to, so a client that sizes its context
    // window by the id sees the `[1m]` twins. Layered apart, so the wrong method's answer is set inside the layer.
    let models = Router::new()
        .route("/v1/models", get(crate::models::list))
        .route("/v1/models/{*id}", get(crate::models::show))
        .method_not_allowed_fallback(wrong_method)
        .layer(map_response(no_store_answer));
    let mut relayed = Router::new()
        .route("/api/hello", head(hello))
        .route("/api/ready", get(ready).head(ready))
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(messages))
        .merge(models);
    // Registered only where the relay holds a CA, so each method on the path keeps the unknown route's 404 on a
    // router without one. Merged ahead of the wrong method's fallback, which another method on the path then gets.
    if let Some(pem) = relay.feature_ca.clone() {
        relayed = relayed.merge(
            Router::new()
                .route("/v0/features/ca.pem", get(feature_ca))
                .with_state(pem),
        );
    }
    relayed
        .method_not_allowed_fallback(wrong_method)
        .fallback(unknown_route)
        .with_state(relay)
}

/// The native client probes this before it sends anything and ignores the answer.
async fn hello() -> StatusCode {
    StatusCode::OK
}

/// The public half alone, so the download reads no key.
async fn feature_ca(State(pem): State<Bytes>) -> Response {
    ([(header::CONTENT_TYPE, "application/x-pem-file")], pem).into_response()
}

/// Readiness, as the image's health check and an orchestrator read it: 200 while the router task, the ledger
/// consumer and the store are alive, 503 naming what died. `/api/hello` stays the liveness answer.
async fn ready(State(relay): State<Arc<Relay>>) -> Response {
    let dead = relay
        .readiness
        .as_ref()
        .map_or_else(Vec::new, |source| source());
    if dead.is_empty() {
        return (StatusCode::OK, Json(json!({ "ready": true }))).into_response();
    }
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "ready": false, "dead": dead })),
    )
        .into_response()
}

async fn unknown_route() -> Response {
    NOT_FOUND.response(&new_request_id())
}

async fn wrong_method() -> Response {
    METHOD_NOT_ALLOWED.response(&new_request_id())
}

async fn messages(State(relay): State<Arc<Relay>>, request: Request) -> Response {
    relay.handle(request).await
}

/// The credential a request goes out with, and the two facts that go with it: the account identity spliced
/// into the body, and the generation a verdict about it has to name.
struct Credential {
    account_uuid: Option<String>,
    generation: u64,
    kind: CredentialKind,
}

impl From<AccessToken> for Credential {
    fn from(token: AccessToken) -> Credential {
        let AccessToken {
            secret,
            generation,
            account_uuid,
        } = token;
        Credential {
            account_uuid: (!account_uuid.is_empty()).then_some(account_uuid),
            generation,
            kind: CredentialKind {
                kind: AccountKind::Oauth,
                secret,
            },
        }
    }
}

impl Relay {
    async fn handle(&self, request: Request) -> Response {
        let received_at = (self.clock)();
        let probe = request.extensions().get::<Probe>().cloned();
        let count_tokens = request.uri().path().ends_with("/count_tokens");
        let trace = Trace {
            bus: self.bus.clone(),
            request_id: new_request_id(),
            received_at,
            kind: if probe.is_some() {
                Kind::Probe
            } else if count_tokens {
                Kind::CountTokens
            } else {
                Kind::Inference
            },
            probe_generation: probe.as_ref().map(|p| p.key_generation),
            held: Arc::new(Mutex::new(None)),
            facts_best_effort: Arc::new(AtomicBool::new(false)),
            clock: self.clock.clone(),
            bodies: self.bodies.clone(),
            completion: Arc::new(Mutex::new(None)),
            shutdown_mark: self.shutdown_mark.clone(),
        };
        let (parts, body) = request.into_parts();
        let original = parts.extensions.get::<OriginalHead>();
        // The snapshot is pinned for the whole request: the key, the pool, and the candidates all come from one
        // registry, and a write that lands mid-request is seen by the next request.
        let snapshot = self.router.snapshot();
        let admitted = match &probe {
            // Before the body, as for a caller: an unauthenticated probe never holds the request limit either. A
            // recipe retained under an earlier key of this id is that key's, so its probe never runs in the
            // replacement's pool.
            Some(probe) => snapshot
                .authenticate_id(&probe.key_id, probe.key_generation, received_at)
                // A probe authenticates by key id, never by secret, so it never presents a retired one.
                .map(|key| (key.clone(), Presented::Live))
                .map_err(|refusal| {
                    tracing::debug!(%refusal, key = %probe.key_id, "a probe's key was refused");
                    key_refusal(&refusal)
                }),
            None => resolve_key(&snapshot, &parts.headers, received_at)
                .map(|(key, presented)| (key.clone(), presented)),
        };
        let (key, presented) = match admitted {
            Ok(admitted) => admitted,
            Err(failure) => {
                return self
                    .refused_before_body(&trace, None, &parts.headers, 0, false, failure, None)
                    .await;
            }
        };
        // A probe is the router's own traffic; "used" in the key list means a client presented the key.
        if probe.is_none() {
            self.router
                .touch_key(&key.id, presented, key.rotated_at)
                .await;
        }
        let capture_hours = snapshot
            .body_capture_hours(&key)
            .min(self.limits.body_capture_max_hours);
        let client_version = crate::drift::client_version(&parts.headers);
        let retired = presented == Presented::Retired;
        // The class is read off the user agent alone, with no body read yet to justify: the router admits Claude
        // Code and refuses any other client before the bytes it would never use are even taken off the wire.
        let client = client_of(&parts.headers);
        if client.class != ClientClass::ClaudeCode {
            tracing::warn!(
                user_agent = %user_agent(&parts.headers),
                key = %key.id,
                "a client outside Claude Code sent a request the router admits from Claude Code alone",
            );
            return self
                .refused_before_body(
                    &trace,
                    Some(&key),
                    &parts.headers,
                    capture_hours,
                    retired,
                    relay_failure(&RelayError::ClientRefused),
                    client_version.as_deref(),
                )
                .await;
        }
        let pool = snapshot.pool(&key.pool_id);
        let failover = pool.map(|p| p.failover).unwrap_or_default();
        // One permit for the whole request, held across every attempt, retry in place and hold until the
        // response ends; it counts the key's requests and bounds nothing. Taken from the head, before a body byte
        // is buffered. A keepalive probe is the router's own traffic and a token count is no turn, so neither
        // takes a slot.
        let mut key_permit = match trace.kind {
            Kind::Inference => Some(
                self.router
                    .in_flight()
                    .try_acquire_key(&key.id, (self.clock)()),
            ),
            Kind::CountTokens | Kind::Probe => None,
        };
        // Declared ahead of the read, so a refusal inside it releases the permit and the reservation where the
        // read leaves them: the permit once the read ends, the reservation with the request.
        let mut read_permit = None;
        let mut reservation = None;
        let body_read = 'read: {
            let Some(coding) = request_coding(&parts.headers) else {
                break 'read Err(relay_failure(&RelayError::UnsupportedEncoding));
            };
            let client_gzip = coding == Encoding::Gzip;
            // The bodies arriving on a key and the bytes of them the process holds are bounded before the read, so
            // a flood of slow or large bodies is refused rather than buffered. A probe's body is the router's own
            // and already in memory, so it takes neither and has no deadline to keep.
            match trace.kind {
                Kind::Probe => {}
                Kind::CountTokens | Kind::Inference => {
                    let Some(permit) = self
                        .body_reads
                        .try_acquire(&key.id, self.limits.key_body_reads)
                    else {
                        break 'read Err(key_body_reads());
                    };
                    read_permit = Some(permit);
                    let declared = content_length(&parts.headers);
                    if declared.is_some_and(|n| n > self.limits.max_request_bytes) {
                        break 'read Err(relay_failure(&RelayError::BodyTooLarge));
                    }
                    // A chunked body may run to the per-body bound, so it holds that much until it has been read.
                    let Some(reserved) = self
                        .body_budget
                        .try_reserve(declared.unwrap_or(self.limits.max_request_bytes) as u64)
                    else {
                        break 'read Err(body_budget_exhausted());
                    };
                    reservation = Some(reserved);
                }
            }
            // A caller that stops sending holds a task and a buffer for nothing, so the read has its own deadline,
            // under the stream's idle bound.
            let read = axum::body::to_bytes(body, self.limits.max_request_bytes);
            let read = match trace.kind {
                Kind::Probe => Ok(read.await),
                Kind::CountTokens | Kind::Inference => {
                    tokio::time::timeout(self.limits.body_read_timeout, read).await
                }
            };
            drop(read_permit.take());
            let body_read = match read {
                Ok(Ok(raw)) => Ok(raw),
                Err(_) => Err(RelayError::BodyTimeout),
                Ok(Err(error)) => {
                    // axum wraps the limit and the body's own failure in one type. Only the limit is the caller's
                    // request being too large, and the rest is the caller leaving before its body was complete.
                    let too_large = error
                        .into_inner()
                        .downcast::<http_body_util::LengthLimitError>()
                        .is_ok();
                    Err(if too_large {
                        RelayError::BodyTooLarge
                    } else {
                        RelayError::BodyIncomplete
                    })
                }
            };
            let raw = match body_read {
                Ok(raw) => raw,
                Err(error) => break 'read Err(relay_failure(&error)),
            };
            if let Some(reservation) = &mut reservation {
                // The declared length frames the body and the per-body bound caps a chunked one, so the read never
                // holds more than was reserved: this only shrinks, and a shrink never fails.
                let shrunk = reservation.resize(raw.len() as u64);
                debug_assert!(shrunk.is_ok(), "the read outgrew its reservation");
            }
            if !client_gzip {
                break 'read Ok((raw, client_gzip));
            }
            // Every read below, the edits and the stamp included, sees the plaintext with the client's tail after
            // the JSON in place, as an identity body is seen.
            let bound = self.limits.max_request_bytes;
            if let Some(reservation) = &mut reservation {
                // The inflated worst case is reserved before a byte is inflated: a budget that cannot grant it
                // refuses here, never after the decoder has already allocated it.
                if reservation.resize(bound as u64).is_err() {
                    break 'read Err(body_budget_exhausted());
                }
            }
            let plain = match inflate_request(&raw, bound) {
                Ok(plain) => plain,
                Err(error) => break 'read Err(relay_failure(&error)),
            };
            // Once the shrink below counts the inflated bytes alone, the compressed copy kept to the end of the
            // request would hold up to the per-body bound the budget no longer sees.
            drop(raw);
            if let Some(reservation) = &mut reservation {
                // The inflated bytes are what the bound and the budget measure from here, and a shrink never fails.
                let shrunk = reservation.resize(plain.len() as u64);
                debug_assert!(shrunk.is_ok(), "the inflated body outgrew its reservation");
            }
            Ok((Bytes::from(plain), client_gzip))
        };
        let body_read = body_read.and_then(|(raw, client_gzip)| {
            // A body of many tiny elements builds a tree far larger than itself, so the count is read off the bytes
            // before the first parse.
            let elements = element_count(&raw, MAX_BODY_ELEMENTS);
            if elements > MAX_BODY_ELEMENTS {
                return Err(relay_failure(&RelayError::BodyTooManyElements));
            }
            // The parsed tree is held from here to the guardrail step, across the catalog's lookup, so the budget
            // counts it before it is built and a budget that cannot grant it refuses here.
            let tree_bytes = elements as u64 * TREE_BYTES_PER_ELEMENT;
            let tree_budget = if reservation.is_some() {
                // The body's reservation holds `raw.len()` here, after the shrinks of the read. Above the whole
                // budget no wait makes room for the body and its tree, so the refusal is for good.
                if raw.len() as u64 + tree_bytes > self.limits.body_budget_bytes {
                    return Err(body_over_budget());
                }
                Some(
                    self.body_budget
                        .try_reserve(tree_bytes)
                        .ok_or_else(body_budget_exhausted)?,
                )
            } else {
                None
            };
            Ok((raw, client_gzip, tree_budget))
        });
        let (raw, client_gzip, tree_budget) = match body_read {
            Ok(read) => read,
            Err(failure) => {
                return self
                    .refused_before_body(
                        &trace,
                        Some(&key),
                        &parts.headers,
                        capture_hours,
                        retired,
                        failure,
                        client_version.as_deref(),
                    )
                    .await;
            }
        };
        // Each lookup below walks the raw body's top-level object itself and holds no table of its members.
        let members = phoebis_cch::TopLevelMembers::read(&raw);
        // The tree lives to the guardrail step and no further: it is dropped before the observation waits on the bus.
        let (facts, tree) = BodyFacts::read_with_tree(&members);
        let tree = (tree, tree_budget);
        let auto_mode_server = snapshot.auto_mode_server(&key);
        let auto_mode = facts.shape.safeguards.then_some(if auto_mode_server {
            AutoMode::Server
        } else {
            AutoMode::Stripped
        });
        let stream = wants_stream(&members);
        // Read after the body, so the refusal's row carries the request's shape, and before the first fact is
        // published, so no probe is paid for and no request is accepted that the ledger cannot record while its
        // writer is behind. The refusal's own facts go best-effort: the bus queue fills behind a writer that
        // stopped draining, and a 503 that waited for it would hold the very client it is meant to release.
        let behind = self.ledger_standing(&trace, true);
        // Admission and the body edit both act on the first spelling of a member; a second one would reach
        // the wire untouched and a last-wins parser upstream would act on it instead. The billing line's path
        // counts as well: the drift check and its placeholder exemption read the first `system` and the first
        // `text` of its first block, so a second spelling would carry a hash nothing verified. So does
        // `metadata.user_id`, the one `metadata` member the relay reads and splices the account into. Refused
        // whole rather than repaired: the caller's body is not edited on its way through. A scan that could not
        // finish proves nothing about the members past where it stopped, so it refuses as well.
        let repeated = repeated_member(&members);
        let refusal = if behind {
            Some(RelayError::LedgerBehind)
        } else {
            match repeated {
                Ok(None) => None,
                Ok(Some(member)) => Some(RelayError::BodyShape(BodyMember::Repeated(member))),
                Err(phoebis_cch::CchError::TooDeep(_)) => Some(RelayError::BodyTooDeep),
                Err(error) => Some(RelayError::Cch(error)),
            }
        };
        // Read once, for both observations and the climb that reads the request class, and reused on every attempt
        // of this request.
        let hints = ClientHints::read(&parts.headers);
        let auxiliary = crate::identity::is_auxiliary(hints.request_class.as_deref());
        let cache_ttl_policy = pool
            .map(|p| CacheTtlPolicy::from_json(&p.cache_ttl_json))
            .unwrap_or_default();
        let markerless = if matches!(trace.kind, Kind::Inference | Kind::Probe)
            && cache_ttl_policy.inject_when_absent
            && !auxiliary
        {
            Markerless::TakesTheMember
        } else {
            Markerless::GoesAsSent
        };
        // One fact for the selection and for the request's row: the body's own marker, or the one the policy adds to
        // a body that carries none.
        let marker = facts.markers_ttl_ms.is_some()
            || (cache_ttl_policy.enabled && markerless == Markerless::TakesTheMember);
        let identified = refusal.map_or_else(|| identify(&parts.headers, &members, client), Err);
        let mut identity = match identified {
            Ok(identity) => identity,
            Err(error) => {
                drop(tree);
                trace
                    .observed(
                        Some(&key),
                        None,
                        &parts.headers,
                        true,
                        raw.clone(),
                        stream,
                        capture_hours,
                        facts.shape.clone(),
                        None,
                        client_facts(&parts.headers, &hints, None, Some(&facts)),
                        auto_mode,
                        retired,
                        key.rotated_at,
                        None,
                        Vec::new(),
                        facts.credit_digest,
                        marker,
                    )
                    .await;
                return self
                    .refused(
                        &trace,
                        0,
                        relay_failure(&error),
                        Some(&key),
                        None,
                        client_version.as_deref(),
                    )
                    .await;
            }
        };
        // The resolved name is substituted before anything downstream reads it, so the observed event, the
        // routing decision, and the ledger agree.
        identity.canonical_model = self.catalog.canonical_for(&identity.folded_model).await;
        // The previous answer's report may still sit in the router task's queue when the client's next turn
        // arrives within milliseconds; the climb reads the map directly, so it waits, bounded, for that report
        // to land.
        let boundaries = facts
            .chains
            .as_ref()
            .map_or(&[][..], |c| &c.message_boundaries[..]);
        let applied = self.router.applied_report();
        if let Some(session_id) = identity.session_id.as_deref() {
            if let Some(seq) =
                self.pending_reports
                    .pending(&key.id, key.created_at, session_id, applied)
            {
                let deadline = tokio::time::Instant::now() + PENDING_REPORT_WAIT;
                if self.router.wait_for_report(seq, deadline).await {
                    self.pending_reports
                        .remove_if(&key.id, key.created_at, session_id, seq);
                }
            }
        }
        climb(
            &mut identity,
            &key.id,
            key.created_at,
            boundaries,
            hints.request_class.as_deref(),
            &self.router,
        );
        let turn = matches!(hints.request_class.as_deref(), Some("main" | "subagent"));
        let subagent = hints.request_class.as_deref() == Some("subagent");
        crate::facts::note_entrypoint_disagreement(
            &trace.request_id,
            identity.entrypoint.as_deref(),
            facts.entrypoint.as_deref(),
        );
        // Before the facts are published and before any account is read: a block is a refusal the operator asked
        // for, so no selection, pin or window sees the request. A probe is the router's own traffic and is never
        // evaluated, and a key without guardrails pays the one lookup and builds no facts.
        let set = snapshot.guardrails_for(&key);
        let evaluation = if trace.kind == Kind::Probe || set.is_empty() {
            Evaluation::default()
        } else {
            let headers = HeaderMapLookup(&parts.headers);
            let user_agent =
                crate::identity::user_agent_of(&parts.headers).map(phoebis_bus::user_agent_cut);
            let request = RequestFacts {
                kind: trace.kind.as_str(),
                request_class: hints.request_class.as_deref(),
                agent_type: hints.agent_type.as_deref(),
                client: identity.client.as_str(),
                model: &identity.canonical_model,
                user_agent,
                headers: &headers,
                body: tree.0.as_ref(),
            };
            evaluate_all(set.iter().map(Arc::as_ref), &request)
        };
        drop(tree);
        let guardrails: Vec<GuardrailMatch> = evaluation
            .matched
            .iter()
            .map(|m| GuardrailMatch {
                id: m.id.clone(),
                position: m.position,
                blocked: m.blocked,
            })
            .collect();
        let guardrail_id = evaluation.blocked.map(|i| guardrails[i].id.clone());
        let client = client_facts(&parts.headers, &hints, Some(&identity), Some(&facts));
        let classifier = client.classifier.is_some();
        trace
            .observed(
                Some(&key),
                Some(&identity),
                &parts.headers,
                true,
                raw.clone(),
                stream,
                capture_hours,
                facts.shape.clone(),
                facts.chains.as_ref(),
                client,
                auto_mode,
                retired,
                key.rotated_at,
                guardrail_id.clone(),
                guardrails,
                facts.credit_digest,
                marker,
            )
            .await;
        if let Some(id) = guardrail_id {
            // The routes refuse a name that carries one of the client's phrases, but a stored row can predate that
            // rule. "model:" or "OAuth token has been revoked" in a 403's message would start the client's own
            // handling, so the id stands in for a stored name that carries any phrase of the list.
            let name = snapshot
                .guardrails()
                .name_of(&id)
                .filter(|name| phoebis_guardrails::client_phrase(name).is_none())
                .unwrap_or(&id)
                .to_string();
            tracing::debug!(
                request_id = %trace.request_id,
                key = %key.id,
                guardrail = %id,
                "a guardrail blocked the request"
            );
            // Unlike the auth, body, route and client-gate faults, which the relay answers before it parses the
            // request or resolves a key, a block holds the facts a decision record needs, so it records one as the
            // drift and contract refusals do. The record is try-sent, so a full router queue drops it rather than
            // delaying the 403.
            self.router.record_refusal(
                RefusedRequest {
                    request_id: trace.request_id.clone(),
                    attempt: 1,
                    seq: 1,
                    at: (self.clock)(),
                    key_id: key.id.clone(),
                    key_generation: key.created_at,
                    agent_id: identity.agent_id.clone(),
                    parent_agent_id: None,
                    session_id: identity.session_id.clone(),
                    request_class: hints.request_class.clone(),
                    canonical_model: identity.canonical_model.clone(),
                    lane: LaneFacts::default(),
                    flags: RequestFlags {
                        count: trace.kind == Kind::CountTokens,
                        probe: trace.kind == Kind::Probe,
                        ..RequestFlags::default()
                    },
                    chain: ChainFacts::default(),
                    attempted: Vec::new(),
                    holds: Vec::new(),
                    client: identity.class,
                    client_version: client_version.clone(),
                },
                RefusalFacts {
                    kind: "guardrail",
                    code: GUARDRAIL_BLOCKED,
                    reason: name.clone(),
                },
            );
            return self
                .refused(
                    &trace,
                    0,
                    relay_failure(&RelayError::GuardrailBlocked { name }),
                    Some(&key),
                    Some(&identity),
                    client_version.as_deref(),
                )
                .await;
        }
        let client_class = identity.class;
        let candidates = match resolve(
            &snapshot,
            &key,
            &identity.canonical_model,
            &identity.folded_model,
        ) {
            Ok(candidates) => candidates,
            Err(ResolveError::ModelNotAllowed) => {
                return self
                    .refused(
                        &trace,
                        0,
                        MODEL_NOT_ALLOWED,
                        Some(&key),
                        Some(&identity),
                        client_version.as_deref(),
                    )
                    .await
            }
        };
        // The contract: a Claude Code request carries what the router reads of it, or is refused before an account
        // is chosen. The gate answered any other client before the body was read, so no class is judged here. After
        // resolve, so a refused model is answered first, and before the drift checks, so a missing line is never a
        // drift. Every kind is judged for the version and the session header, and an inference request for the
        // other five items too.
        if let Some(item) = contract_gap(
            &parts.headers,
            &identity,
            &facts,
            &hints,
            client_version.as_deref(),
            trace.kind,
        ) {
            let refusal = self.contract_refusal(
                item,
                &parts.headers,
                &trace,
                &key,
                &identity,
                hints.request_class.as_deref(),
                client_version.as_deref(),
            );
            return self
                .refused(
                    &trace,
                    0,
                    refusal,
                    Some(&key),
                    Some(&identity),
                    client_version.as_deref(),
                )
                .await;
        }
        // A separate field from `restore`: every client inference request of a conversation gets one, because the
        // diagnostics member the option forces names the previous answer in that conversation whoever sent it.
        // Keyed on the client's own session id, which the contract guarantees of an inference request and a probe.
        // A probe reads under the same key, since it replays the session's own headers and the diagnostics report
        // should name the turn it keeps warm. What a probe is answered is never remembered, which the response half
        // enforces by reading no key for it. A count_tokens request has no slot for either, so it is neither read
        // nor remembered. Dispatch reads the key to look up the previous request id per account, and the response
        // half writes the upstream's `request-id` and message id back under it. What lands on the billing line stays
        // gated on `restore` being `Some`.
        let memory_key = match trace.kind {
            Kind::Inference | Kind::Probe => identity.session_id.as_deref().map(|session| {
                MemoryKey::for_session(&key.id, key.created_at, session, &identity.agent_id)
            }),
            _ => None,
        };
        // The one store read of the request path, ahead of the entry's write: a miss after a restart or an
        // eviction is filled from tier 2 here, on the blocking pool, so no worker blocks on SQLite for it, and the
        // write lands on the restored entry, so the leg's lookup of the previous request id finds it. A probe and
        // a token count restore nothing here, since the entry is written for an inference alone.
        if let Some(key) = memory_key
            .as_ref()
            .filter(|_| trace.kind == Kind::Inference)
        {
            self.memory.restore(key, received_at).await;
        }
        let prev_req = restores_prev_req(hints.request_class.as_deref(), &parts.headers);
        // Claude Code's own inference requests alone: a probe replays what the client sent, a token count has no
        // line to fill, and a body without the block is relayed as sent and gets no client request id either. The
        // contract admitted the version, so it lands in the line as the client spelled it.
        let restore = match client_version.as_deref() {
            Some(version)
                if trace.kind == Kind::Inference && facts.line == AttributionLine::Present =>
            {
                if let Some(key) = memory_key.as_ref() {
                    self.memory
                        .touch(key, hints.prompt_id.as_deref(), received_at);
                }
                Some(RestoreContext {
                    cli_version: version.to_string(),
                    prompt_units: facts.prompt_units.clone(),
                    prompt_id: hints.prompt_id.clone(),
                    prev_req,
                })
            }
            _ => None,
        };
        // The stamp and the suffix depend on the body, the client's version and the prompt units alone, so both are
        // judged once here, after the key's client and model checks and before any account is selected; a leg never
        // meets them, and the refusal's row keeps the model, the session and the agent. The hash goes first, so a
        // stamp only the decoded walk reaches is refused before the suffix's stamp gate reads past it. Only a
        // stamped line is judged for its suffix; a reduced-shape line has its missing fields restored instead.
        let (stamped, client_hash, drifted) = match crate::body::stamp_of(&members) {
            Ok(stamp) => (
                !matches!(stamp, Stamp::None),
                stamp.is_client_hash(),
                crate::body::drift_of(&raw, stamp),
            ),
            Err(error) => (false, false, Err(error)),
        };
        let drifted = drifted.map(|hash| {
            hash.or_else(|| {
                let context = restore.as_ref().filter(|_| stamped)?;
                crate::body::version_drift(
                    facts.cc_version.as_deref(),
                    VersionCheck {
                        client_version: &context.cli_version,
                        prompt_units: &context.prompt_units,
                    },
                )
            })
        });
        let stamp = match drifted {
            Ok(None) => None,
            Ok(Some(kind)) => Some(self.drift_refusal(
                kind,
                facts.resumed,
                &parts.headers,
                &trace,
                &key,
                &identity,
                client_version.as_deref(),
            )),
            Err(error) => Some(LocalRefusal::from(relay_failure(&error))),
        };
        if let Some(refusal) = stamp {
            return self
                .refused(
                    &trace,
                    0,
                    refusal,
                    Some(&key),
                    Some(&identity),
                    client_version.as_deref(),
                )
                .await;
        }
        // The pool's switch alone; a request with no pool reads off. A `count_tokens` request never gets it, since
        // its public schema has no slot for the beta or the member to land in.
        let cache_diagnostics_on = pool.is_some_and(|p| p.cache_diagnostics);
        // Mirrors the client's own gate: the member and its beta go only where the client cached and sent tools,
        // so a helper that shares no prefix with its conversation never reports a false miss. The markers are the
        // client's, read before the cache TTL policy's rewrite, so a body the policy gives a member stays
        // member-less. An empty `tools` array is no tools list.
        let diagnostics = cache_diagnostics_on
            && trace.kind != Kind::CountTokens
            && facts.markers_ttl_ms.is_some()
            && facts.shape.tool_count >= 1;
        // The same raw lookup `insert_diagnostics` makes, so the recorded source and the edit never disagree.
        let client_diagnostics =
            trace.kind != Kind::CountTokens && members.object_member_span("diagnostics").is_some();
        // The markers and the tools gate the cache member alone, so this gate reads neither.
        let thinking_diagnostics = pool.is_some_and(|p| p.thinking_diagnostics)
            && trace.kind != Kind::CountTokens
            && facts
                .shape
                .thinking_type
                .as_deref()
                .is_some_and(|kind| kind != "disabled");
        let portable = facts.portable;
        let target = parts
            .uri
            .path_and_query()
            .map(|target| target.as_str())
            .unwrap_or_else(|| parts.uri.path())
            .to_string();
        let fallback_ms = self.limits.cooldown_fallback.as_millis() as i64;
        tracing::debug!(
            agent = %identity.agent_id,
            key = %key.id,
            model = %identity.canonical_model,
            request = %trace.request_id,
            session = ?identity.session_id,
            "relaying a Messages request",
        );
        // Read once and reused on every attempt of this request: the lane does not change across a failover.
        let lane = crate::lane::lane_of(&identity.canonical_model, &facts, hints.per_turn_effort);
        let deadline = (self.limits.head_timeout > ATTEMPT_DEADLINE_MARGIN).then(|| {
            received_at + (self.limits.head_timeout - ATTEMPT_DEADLINE_MARGIN).as_millis() as i64
        });
        let admitted = Admitted {
            auto_mode_server,
            auxiliary,
            cache_ttl_policy,
            caller: &parts.headers,
            chains: facts.chains.as_ref(),
            classifier,
            client_diagnostics,
            client_gzip,
            client_hash,
            client_version,
            deadline,
            diagnostics,
            fallback_ms,
            fast: facts.shape.speed.as_deref() == Some("fast"),
            floor_held: AtomicBool::new(false),
            identity: &identity,
            key: &key,
            lane,
            marker,
            markerless,
            markers_ttl_ms: facts.markers_ttl_ms,
            memory_key,
            moves_pins: portable && trace.kind != Kind::Probe,
            original,
            pool_id: pool.map(|p| p.id.clone()),
            portable,
            raw: &raw,
            restore,
            retry_in_place: Duration::from_secs(failover.retry_in_place_seconds),
            subagent,
            target: &target,
            thinking_diagnostics,
            trace: &trace,
            turn,
            wait_bound: deadline
                .unwrap_or(received_at + self.limits.head_timeout.as_millis() as i64),
        };
        let mut attempted: Vec<String> = Vec::new();
        // Every hold this request waited through, oldest first: carried onto the next selection so the
        // attempt that follows names the wait on its trace.
        let mut holds: Vec<HoldFacts> = Vec::new();
        let mut attempt: u8 = 0;
        // The request's head entry, registered before its first selection is enqueued so a request a mark names
        // always has one while it lives, and removed when this scope ends, whichever way the request ends. A count
        // carries no mark, so it registers none.
        let _head_entry = (trace.kind != Kind::CountTokens)
            .then(|| HeadEntry::registered(self.heads.clone(), &trace.request_id));
        // The request's end report, armed before its first selection is enqueued so a request dropped while the
        // selection waits still releases what it booked. A count and a probe book nothing, so neither arms one.
        let mut end_guard = (trace.kind == Kind::Inference).then(|| {
            EndGuard::armed(
                self.router.clone(),
                self.bodies.clone(),
                self.clock.clone(),
                trace.request_id.clone(),
            )
        });
        // Once per request: every selection of it hands the same estimate in.
        let body_tokens = body_tokens(&raw);
        // The selections asked for this request, the decision's `seq`: a hold and the placement after it are two
        // decisions of one wire attempt, and a retry in place or a refresh pass dispatches again under one.
        let mut selections: u32 = 0;
        // The accounts this request was held for once already. A freed slot is read, not reserved, so another
        // request can take it before the next selection; without this the same account could hold the request
        // again on every pass.
        let mut held_for: Vec<String> = Vec::new();
        // Set by the request's first hold and shared by every later one, so a pool of any size holds the request
        // one `retry_in_place` at most. Kept on both clocks: the slot wait sleeps on the runtime's and the reset
        // wait reads the relay's.
        let mut hold_deadline: Option<(tokio::time::Instant, i64)> = None;
        let retry_in_place_ms =
            i64::try_from(admitted.retry_in_place.as_millis()).unwrap_or(i64::MAX);
        // Whether a hold ran out, and whether any leg dialled: an untimed exhaustion reached through holds alone
        // is a pool at its cap, which frees on its own, not one only an operator can clear.
        let mut held_out = false;
        let mut dialed = false;
        // Whether a leg of this request marked its account for re-authentication, which a probe's exhaustion
        // answer names so the keepalive pauses under the authentication reason. Never cleared: a later leg's
        // failover of another kind does not undo the mark.
        let mut auth_marked = false;

        loop {
            let now = (self.clock)();
            // One deadline for the whole request, the climb's waits included: past it the pool is answered as
            // exhausted rather than selected again.
            if admitted.deadline.is_some_and(|deadline| now >= deadline) {
                ending(&mut end_guard, EXHAUSTED);
                return self
                    .exhausted(
                        &admitted,
                        attempt,
                        &Exhaustion::Untimed,
                        now,
                        false,
                        auth_marked,
                    )
                    .await;
            }
            selections += 1;
            let answer = match self
                .router
                .select(SelectRequest {
                    request_id: trace.request_id.clone(),
                    // The pass loop of `dispatch_to` increments the counter before it publishes, so the attempt
                    // this decision dispatches is the next one.
                    attempt: u32::from(attempt) + 1,
                    seq: selections,
                    key_id: key.id.clone(),
                    key_generation: key.created_at,
                    agent_id: identity.agent_id.clone(),
                    session_id: identity
                        .session_id
                        .clone()
                        .expect("the contract admits no request without a session id"),
                    canonical_model: identity.canonical_model.clone(),
                    folded_model: identity.folded_model.clone(),
                    candidates: candidates.clone(),
                    portable,
                    credit_account: facts
                        .credit_digest
                        .and_then(|digest| self.credits.issuer(&digest, now)),
                    attempted: attempted.clone(),
                    now,
                    received_at: trace.received_at,
                    fallback_ms,
                    revision: snapshot.revision,
                    probe: admitted.trace.kind == Kind::Probe,
                    count: admitted.trace.kind == Kind::CountTokens,
                    request_class: hints.request_class.clone(),
                    context_compacted: hints.context_compacted.is_some(),
                    boundaries: facts
                        .chains
                        .as_ref()
                        .map(|c| c.message_boundaries.clone())
                        .unwrap_or_default(),
                    client: client_class,
                    entrypoint: identity.entrypoint.clone(),
                    static_boundaries: facts
                        .chains
                        .as_ref()
                        .map(|c| c.static_boundaries.clone())
                        .unwrap_or_default(),
                    marker: admitted.marker,
                    parent_agent_id: identity.parent_agent_id.clone(),
                    failed_over: None,
                    holds: holds.clone(),
                    dialed,
                    held_out,
                    lane: admitted.lane.clone(),
                    fast: admitted.fast,
                    classifier: admitted.classifier,
                    body_tokens,
                    // The store reads an empty word back as none, so the live pin carries none as well and
                    // reads the same before a restart as after it.
                    effort: facts.shape.effort.clone().filter(|e| !e.is_empty()),
                })
                .await
            {
                Ok(answer) => answer,
                Err(_) => {
                    ending(&mut end_guard, REFUSED);
                    return self
                        .refused_admitted(&admitted, attempt, ROUTER_UNAVAILABLE)
                        .await;
                }
            };
            let selection = match answer.result {
                SelectResult::Selected(selection) => selection,
                // The key the request was admitted on left the registry, or another key took its id, before the
                // selection ran: the answer is the one admission gives a key the registry does not hold, so a
                // request and a probe agree.
                SelectResult::Refused(Refusal::Key(refusal)) => {
                    tracing::debug!(%refusal, key = %key.id, "a request's key was refused after admission");
                    ending(&mut end_guard, REFUSED);
                    return self
                        .refused_admitted(&admitted, attempt, key_refusal(&refusal))
                        .await;
                }
                // The registry moved after admission and no longer admits this request; the answer is the one the
                // current registry would have given at admission.
                SelectResult::Refused(Refusal::Resolve(ResolveError::ModelNotAllowed)) => {
                    ending(&mut end_guard, REFUSED);
                    return self
                        .refused_admitted(&admitted, attempt, MODEL_NOT_ALLOWED)
                        .await;
                }
                SelectResult::Exhausted(exhaustion) => {
                    ending(&mut end_guard, EXHAUSTED);
                    return self
                        .exhausted(
                            &admitted,
                            attempt,
                            &exhaustion,
                            now,
                            held_out && !dialed,
                            auth_marked,
                        )
                        .await;
                }
                // A slot freed within the bound lets the next selection place there; past it the account is
                // attempted and the next selection looks elsewhere.
                SelectResult::Held(mut hold) => {
                    let reset = match hold.reason {
                        // A slowed pin waits to its deadline before its head, under the request's `wait_bound` alone,
                        // and selects again with the hold on the request, which answers no second one. The hold is its
                        // own stage: it starts no retry bound and passes no account over, so a slot or reset wait after
                        // it keeps its whole bound and a pool that exhausts after it answers as it would have without
                        // it.
                        HoldReason::SlowDown => {
                            let started = tokio::time::Instant::now();
                            let waited = self
                                .wait_out(
                                    hold.until.unwrap_or(now),
                                    Some(admitted.wait_bound),
                                    &answer.cancel,
                                    None,
                                )
                                .await;
                            hold.held_ms = started.elapsed().as_millis() as u64;
                            hold.ran_out = matches!(waited, Waited::Deadline);
                            holds.push(hold);
                            trace.waited(&holds);
                            continue;
                        }
                        // A moved pin's request waits for the head of the request that carried the move, inside the
                        // relay, which owns both streams: the waiter wakes at that request's final answer, past any
                        // retry in place or replay, or when it ends without one, and never polls the routing. The hold
                        // is its own stage, as the slow-down's is.
                        HoldReason::Rewrite => {
                            let started = tokio::time::Instant::now();
                            // No entry means the request it waits for ended before this selection, so there is no head
                            // to wait for and the waiter goes out at once, as one the sender's drop woke does.
                            let first = hold
                                .waits_for
                                .as_deref()
                                .and_then(|first| head_of(&self.heads, first));
                            let waited = match first {
                                Some(mut head) => {
                                    self.wait_out(
                                        hold.until.unwrap_or(now),
                                        Some(admitted.wait_bound),
                                        &answer.cancel,
                                        Some(&mut head),
                                    )
                                    .await
                                }
                                None => Waited::Head,
                            };
                            hold.held_ms = started.elapsed().as_millis() as u64;
                            hold.ran_out = matches!(waited, Waited::Cleared | Waited::Deadline);
                            holds.push(hold);
                            trace.waited(&holds);
                            continue;
                        }
                        HoldReason::Reset => true,
                        HoldReason::Cap => false,
                    };
                    let account_id = hold.account_id.clone();
                    // The account had its one hold of this request, or the request's one bound has passed: at its
                    // cap it is passed over as one whose wait ran out.
                    if held_for.contains(&account_id)
                        || hold_deadline.is_some_and(|(at, _)| tokio::time::Instant::now() >= at)
                    {
                        held_out = true;
                        attempted.push(account_id);
                        continue;
                    }
                    let started = tokio::time::Instant::now();
                    let (hold_deadline, hold_bound) = *hold_deadline.get_or_insert_with(|| {
                        (
                            tokio::time::Instant::now() + admitted.retry_in_place,
                            now.saturating_add(retry_in_place_ms),
                        )
                    });
                    // A spent window about to reset: the request sleeps to the hold's end, the later of the reset and
                    // the cooldown's end, or to its one bound or its `wait_bound` when one of those comes first, and
                    // selects again with the account still open to it, so the pin renews in place. A wait the bound
                    // cut short, or one the account left during, passes the account over. A hold that ends at or past
                    // `wait_bound` is refused and the account passed over, and the selection that follows tries a
                    // sibling as the retry-in-place path does: where the head timeout is the bound the wait could not
                    // end inside the head guard, and under the shipped 600 s guard `wait_bound` is the request's
                    // `deadline`, at which the loop head answers the pool exhausted.
                    if reset {
                        if hold.until.is_some_and(|until| until >= admitted.wait_bound) {
                            held_out = true;
                            attempted.push(account_id);
                            continue;
                        }
                        let end = admitted.wait_bound.min(hold_bound);
                        let waited = self
                            .wait_out(hold.until.unwrap_or(now), Some(end), &answer.cancel, None)
                            .await;
                        hold.held_ms = started.elapsed().as_millis() as u64;
                        hold.ran_out = matches!(waited, Waited::Deadline);
                        holds.push(hold);
                        trace.waited(&holds);
                        held_for.push(account_id.clone());
                        if !matches!(waited, Waited::Cleared) {
                            attempted.push(account_id);
                        }
                        continue;
                    }
                    let freed = self
                        .router
                        .in_flight()
                        .wait_for_slot(&account_id, failover.in_flight_cap, hold_deadline)
                        .await;
                    // Measured here, the one place that knows: the trace of the attempt that follows names the
                    // wait, and so does the completion when no attempt follows.
                    hold.held_ms = started.elapsed().as_millis() as u64;
                    hold.ran_out = !freed;
                    // The admission dispatches the request, so the selection that follows moves the pin's last
                    // dispatch here and the slot wait never joins the pin's next gap. A wait that ran out leaves the
                    // account and names no end.
                    if freed {
                        hold.until = Some((self.clock)());
                    }
                    holds.push(hold);
                    trace.waited(&holds);
                    held_for.push(account_id.clone());
                    if !freed {
                        held_out = true;
                        attempted.push(account_id);
                    }
                    continue;
                }
            };
            attempted.push(selection.account.id.clone());
            let usage_based = self.router.snapshot().usage_based(&selection.account.id);
            dialed = true;
            match self
                .dispatch_to(
                    &admitted,
                    &selection,
                    selections,
                    &answer.cancel,
                    answer.ticket,
                    answer.funding_ordinal,
                    usage_based,
                    &mut attempt,
                    &mut key_permit,
                    &mut end_guard,
                )
                .await
            {
                Leg::AtCap => {
                    ending(&mut end_guard, EXHAUSTED);
                    return self
                        .exhausted(
                            &admitted,
                            attempt,
                            &Exhaustion::Untimed,
                            now,
                            false,
                            auth_marked,
                        )
                        .await;
                }
                Leg::FailOver {
                    auth_marked: marked,
                } => {
                    auth_marked |= marked;
                    continue;
                }
                Leg::Respond(mut response) => {
                    // Stamped here rather than where the upstream's head is read, so an answer the relay built
                    // itself after the selection carries the attempt's account kind and class too, the facts the
                    // ledger files the attempt's funding from.
                    if let Some(outcome) = response.extensions_mut().get_mut::<RelayOutcome>() {
                        outcome.account_kind = Some(selection.account.kind);
                        outcome.usage_based = usage_based;
                    }
                    return response;
                }
            }
        }
    }

    /// The answer for a pool with nothing to select, the outcome carrying the exhaustion's code. `at_cap` answers
    /// an untimed exhaustion as a pool whose accounts are all at their in-flight cap; `auth_marked` is whether a
    /// leg of this request marked its account for re-authentication.
    async fn exhausted(
        &self,
        admitted: &Admitted<'_>,
        attempt: u8,
        exhaustion: &Exhaustion,
        now: i64,
        at_cap: bool,
        auth_marked: bool,
    ) -> Response {
        // Each builder takes its status from `exhaustion_status` for its arm, the status the routing log's exhausted
        // row records as its `answer`.
        let mut failure = match exhaustion {
            Exhaustion::Untimed if at_cap => accounts_at_cap(admitted.retry_in_place.as_secs()),
            _ => exhaustion_failure(exhaustion, now),
        };
        let mut outcome = admitted.outcome("", 0, attempt, false, None, None);
        let reauth = admitted.trace.kind == Kind::Probe && auth_marked;
        outcome.obstacle = match exhaustion {
            Exhaustion::Timed { window, .. } if window == BUDGET_WINDOW => {
                Some(ProbeObstacle::Budget)
            }
            // A probe that tried its marked member is not one to retry, and no wait clears the mark. The
            // re-authentication reason replaces `attempted` alone: a timed answer keeps the quota pause its 429 gets,
            // a budget hold stays one, and a pinned member's other standings keep their own reasons.
            Exhaustion::Untimed if reauth && !at_cap => Some(ProbeObstacle::ReauthRequired),
            Exhaustion::Pinned {
                obstacle: PinnedObstacle::Attempted,
            } if reauth => Some(ProbeObstacle::ReauthRequired),
            Exhaustion::Pinned { obstacle } => Some((*obstacle).into()),
            _ => None,
        };
        // A probe held on the mark its leg wrote is refused for the probe alone, as one held on its pinned member's
        // standing is, so the pool's exhaustion gauge and rollups never count it.
        if matches!(outcome.obstacle, Some(ProbeObstacle::ReauthRequired)) {
            failure.class = FailureClass::Rejected;
        }
        outcome.answered(failure.code, failure.class.outcome(attempt));
        let response = exhaustion_response(exhaustion, &failure, &admitted.trace.request_id);
        self.answer(admitted.trace, response, outcome).await
    }

    /// One account's leg of the request. `attempt` is the wire-submission index every bus event and attempt row
    /// carries. `seq` is the selecting decision's, which every pass of the leg reports its cache map under.
    /// `key_permit` is the request's own, which a committed stream takes into its body, and so is `end_guard`, the
    /// request's end report, which every pass of the leg re-arms with its attempt. `selected` is the ticket the
    /// selection charged for the leg's first pass. `usage_based` is the account's class, read once by the selection
    /// loop, so a reload during the leg never files its selection and its outcome under two classes.
    ///
    /// An OAuth 401 runs the refresh ladder on this account: a refresh, and after one that failed without a
    /// conclusive answer a second once the account's backoff clears, the key's permit and the ticket held through
    /// the wait. The token endpoint's floor is waited out once per request, through `Admitted::floor_held`, since a
    /// second account's bad minute is the same minute. Every wait ends at `Admitted::deadline`, past which the
    /// attempt loop answers the exhaustion.
    #[allow(clippy::too_many_arguments)]
    async fn dispatch_to(
        &self,
        admitted: &Admitted<'_>,
        selection: &Selection,
        seq: u32,
        cancel: &CancellationToken,
        mut selected: Option<AccountTicket>,
        funding_ordinal: Option<u64>,
        usage_based: bool,
        attempt: &mut u8,
        key_permit: &mut Option<KeyPermit>,
        end_guard: &mut Option<EndGuard>,
    ) -> Leg {
        // The admission pinned this pool's policy and the row's attribution; a selection on a newer revision
        // resolved the key's current pool, so a key moved between the two would dispatch under one pool's policy
        // on another's member. Refused with the router's own 503 and a one-second retry: the next request admits
        // under the new pool.
        if admitted.pool_id.as_deref() != Some(selection.pool_id.as_str()) {
            tracing::warn!(
                key = %admitted.key.id,
                admitted_pool = ?admitted.pool_id,
                selected_pool = %selection.pool_id,
                obstacle = "pool_moved",
                "the key moved between pools after admission, so the request is refused"
            );
            let mut outcome = admitted.outcome("", 0, *attempt, false, None, None);
            outcome.obstacle = Some(ProbeObstacle::PoolMoved);
            ending(end_guard, REFUSED);
            return Leg::Respond(
                self.answer_failure(admitted.trace, pool_moved_failure(), outcome)
                    .await,
            );
        }
        // The row the router selected, not one looked up again in the admission snapshot: the endpoint a
        // request goes out to and the credential it carries have to be the pair one snapshot held.
        let account = &selection.account;
        let pin_epoch = selection.pin.as_ref().map(|pin| pin.epoch);
        // No wire submission, so the index does not move: the account is already in `attempted`, and the next
        // selection skips it. The outer loop is bounded by the pool size, because each account is selected
        // at most once per request.
        let mut credential = match self.credential(account).await {
            Ok(credential) => credential,
            Err(NoCredential::Marked) => return Leg::FailOver { auth_marked: true },
            Err(NoCredential::Stale | NoCredential::Unavailable) => {
                return Leg::FailOver { auth_marked: false }
            }
        };
        // Stable across a same-account replay: an OAuth refresh replaces the token, never the account identity
        // it names, so this is read once rather than through the closure below.
        let account_uuid = match admitted.trace.kind {
            Kind::CountTokens => None,
            Kind::Inference | Kind::Probe => credential.account_uuid.clone(),
        };
        let uuid_edit = match admitted.trace.kind {
            // The public `count_tokens` schema carries no `metadata` member and the upstream refuses unknown
            // top-level ones, so there is no slot the account identity could go in. It is not billed either, so
            // nothing is lost by leaving the body without it.
            Kind::CountTokens => AccountUuidEdit::Keep,
            // An API-key leg carries the empty uuid the native client writes on an API key. An OAuth leg carries
            // the account's own, and one whose credential names none leaves the member as sent.
            Kind::Inference | Kind::Probe if account.kind == AccountKind::ApiKey => {
                AccountUuidEdit::Blank
            }
            Kind::Inference | Kind::Probe => account_uuid
                .as_deref()
                .map_or(AccountUuidEdit::Keep, AccountUuidEdit::Splice),
        };
        let memory_key = admitted.memory_key.as_ref();
        let canonical_model = &admitted.identity.canonical_model;
        // Session memory tracks a repeat of the same wire spelling, not the resolved identity: a resolution
        // landing between two requests for the same spelling must not make the second look like a fresh one.
        let folded_model = &admitted.identity.folded_model;
        // A failover leg may follow a long first one; the key may have been evicted and saved meanwhile, and
        // `build` reads the map alone.
        if let Some(key) = memory_key {
            self.memory.restore(key, (self.clock)()).await;
        }
        // The body for this leg and what the editor added to it; the head is built per pass and reports its own.
        let build = |skip: &BTreeSet<Injected>| -> Built {
            let mut edited = admitted.raw.to_vec();
            let mut body_injected = Vec::new();
            let mut inject_betas: Vec<Injected> = Vec::new();
            let writes_cost_money =
                pays_for_writes(account.kind, usage_based, selection.in_overage);
            // Priced per attempt: a migrated attempt reads its destination's overage, and a same-account
            // retry reuses this body rather than pricing again.
            // A probe replays another turn's markers under that turn's agent, so the branch follows the agent for
            // a probe as for the turn, and a subagent's rewrite never reads the main turn's per-model override.
            let branch = if admitted.identity.agent_id == MAIN_AGENT {
                TtlBranch::Main
            } else {
                TtlBranch::Subagent
            };
            // A client that sends no marker asked for no cache, and the policy adds one only where the
            // operator said so; a count is not the request whose cache it measures, so its markers travel as
            // the client wrote them.
            let priced = match admitted.trace.kind {
                Kind::CountTokens => None,
                Kind::Inference | Kind::Probe => {
                    admitted
                        .cache_ttl_policy
                        .ttl_for(canonical_model, branch, writes_cost_money)
                }
            };
            // The one-hour body and its beta stand or fall together: a host that refused the beta cannot
            // serve the hour, so a leg that stripped it, or an account latched against it, dispatches five
            // minutes and reports that.
            let mut ttl = priced.map(|t| {
                if skip.contains(&Injected::ExtendedTtlBeta) {
                    CacheTtl::FiveMinutes
                } else {
                    t
                }
            });
            // A marker-less body that went as sent claims no TTL, whether because it is a count, because the
            // toggle left it alone or because its class is auxiliary: no beta for it, none dropped for it, and
            // none reported. A body that already carried a marker claims the rewrite's TTL even where the marker
            // already read that way, so a leg does not lose its report to a rewrite that had nothing left to
            // change. A beta naming an hour the body does not carry would be the relay's to answer for on a 400.
            let claims_its_ttl =
                |edits: &BodyEdits| edits.cache_ttl_marked || edits.cache_ttl_edited;
            // Isolated per account, so a failover never carries one account's previous request id, or the
            // previous message id a diagnostics member names, into another account's memory.
            let previous = memory_key
                .map(|key| self.memory.previous(key, &account.id, folded_model))
                .unwrap_or_default();
            // The answer the request continues, on the selected account alone: the deepest boundary of its own
            // conversation that holds an answer, else, while a fork has no answer of its own, of the conversation
            // it forked from. A fork that regenerated its parent's tail holds its answer in the memory alone, as
            // the shared boundary keeps the parent's. The session memory's last id is the fallback, as a first
            // turn after a compaction or a model switch has no boundary to match.
            let own_root = admitted
                .identity
                .session_id
                .as_deref()
                .expect("the contract admits no request without a session id");
            let predecessor_under = |root: &str| {
                self.router.predecessor(&PredecessorQuery {
                    canonical_model: &admitted.identity.canonical_model,
                    account_id: &account.id,
                    key_id: &admitted.key.id,
                    key_generation: admitted.key.created_at,
                    roots: &[root],
                    message_boundaries: admitted
                        .chains
                        .map(|c| &c.message_boundaries[..])
                        .unwrap_or(&[]),
                })
            };
            // The first request a session sends to an account other than the one its previous request landed on
            // names a message only the account it left can find. A probe replays its recipe's member, which from a
            // chain's second probe on names a message older than the upstream keeps, and the keepalive reads the
            // cache tokens rather than the reason, so no probe names one. Read from the selection, so the relay keeps
            // no memory of the session's last account, and whatever the pool's switch reads, since the client's
            // member is the one that misses.
            let detach = selection.left.is_some() || admitted.trace.kind == Kind::Probe;
            let named: Option<Option<String>> = (admitted.diagnostics
                && !skip.contains(&Injected::Diagnostics)
                && !admitted.client_diagnostics)
                .then(|| {
                    if detach {
                        return None;
                    }
                    predecessor_under(own_root)
                        .or_else(|| {
                            admitted
                                .identity
                                .forked_from
                                .as_deref()
                                .filter(|_| previous.message_id.is_none())
                                .and_then(predecessor_under)
                        })
                        .or_else(|| previous.message_id.clone())
                });
            let diagnostics = named.as_ref().map(|id| id.as_deref());
            let edits = edit_body(
                &mut edited,
                &NativeEdit {
                    account_uuid: uuid_edit,
                    upstream_model: &selection.upstream_model,
                    restore: restore_with(&admitted.restore, &previous, skip),
                    diagnostics,
                    cache_ttl: ttl,
                    markerless: admitted.markerless,
                    safeguards: if admitted.auto_mode_server {
                        SafeguardsEdit::Keep
                    } else {
                        SafeguardsEdit::Drop
                    },
                    client_hash: admitted.client_hash,
                    sign_placeholder: self.sign_cch_placeholder,
                    detach_diagnostics: detach,
                },
            )?;
            if edits.diagnostics_detached {
                tracing::debug!(
                    account = %account.id,
                    re_signed = edits.cch_restamped,
                    blanked = matches!(uuid_edit, AccountUuidEdit::Blank),
                    "set the client's previous message id to null"
                );
            }
            if !claims_its_ttl(&edits) {
                ttl = None;
            }
            body_injected.extend(edits.restored);
            if admitted.diagnostics && !skip.contains(&Injected::CacheDiagnosisBeta) {
                inject_betas.push(Injected::CacheDiagnosisBeta);
            }
            if admitted.thinking_diagnostics && !skip.contains(&Injected::ThinkingBindingBeta) {
                inject_betas.push(Injected::ThinkingBindingBeta);
            }
            if ttl == Some(CacheTtl::OneHour) {
                inject_betas.push(Injected::ExtendedTtlBeta);
            }
            Ok((edited, body_injected, inject_betas, ttl))
        };
        // Seeded from what this account has already refused, so a leg the operator has not touched since the
        // last 400 never resubmits a field or a beta the upstream is known to reject.
        let mut skip: BTreeSet<Injected> = self.memory.refused(account);
        let (edited, mut body_injected, mut inject_betas, mut ttl) = match build(&skip) {
            Ok(built) => built,
            Err(error) => {
                ending(end_guard, REFUSED);
                return Leg::Respond(
                    self.refused_admitted(admitted, *attempt, relay_failure(&error))
                        .await,
                );
            }
        };
        let gzip_out = match self.limits.upstream_gzip {
            UpstreamGzip::On => true,
            UpstreamGzip::Off => false,
            UpstreamGzip::Auto => admitted.client_gzip,
        };
        // Encoded once per build: every pass of the leg sends the same bytes, so a retry or a replay reuses them
        // rather than copying or compressing the body again.
        let on_the_wire = |edited: &Bytes| {
            if gzip_out {
                Bytes::from(phoebis_transport::encode::gzip(edited))
            } else {
                edited.clone()
            }
        };
        let mut edited = Bytes::from(edited);
        let mut wire_body = on_the_wire(&edited);
        let authority = dispatch::UPSTREAM_AUTHORITY;
        let Some(addr) = (self.resolver)(authority).await else {
            tracing::warn!(host = %authority, "the upstream host does not resolve");
            return Leg::FailOver { auth_marked: false };
        };
        let pool_key = PoolKey {
            account_id: account.id.clone(),
            host: authority.to_string(),
        };
        let mut fresh = false;
        let mut retried = false;
        let mut retried_in_place = false;
        let mut replayed = false;
        let mut stripped_once = false;
        // Built before the pass loop, so every pass of the leg shares it and a pass that continues drops nothing, with
        // the attempt the selection wrote the landing's mark under. A count carries no mark.
        let mut leg_head = (admitted.trace.kind != Kind::CountTokens).then(|| HeadGuard {
            bodies: self.bodies.clone(),
            clock: self.clock.clone(),
            heads: self.heads.clone(),
            router: self.router.clone(),
            request_id: admitted.trace.request_id.clone(),
            attempt: u32::from(*attempt) + 1,
            sent: false,
            goes_on: false,
        });

        loop {
            // The wire-submission index is a u8 on the bus and the attempt row, so 255 submissions per request
            // is the technical bound; the pool's size is the practical one, and no pool approaches it.
            if *attempt == u8::MAX {
                return Leg::AtCap;
            }
            *attempt += 1;
            if let Some(guard) = end_guard.as_mut() {
                guard.rearm(*attempt);
            }
            // One per wire attempt. The first pass's was charged by its selection, so the count the selector read
            // is the one it charged; a later pass takes its own before the dial. A pass that ends without
            // committing a stream drops it with its response half, before the next pass or leg takes one. A
            // token count takes none on any pass: it is bound to no cap.
            let count = admitted.trace.kind == Kind::CountTokens;
            let ticket = selected.take().or_else(|| {
                (!count).then(|| {
                    self.router
                        .in_flight()
                        .dispatched(&account.id, (self.clock)())
                })
            });
            admitted
                .trace
                .selected(
                    *attempt,
                    account,
                    usage_based,
                    pin_epoch,
                    (self.clock)(),
                    cache_ttl_ms(ttl),
                    // A probe replays a recipe rather than making its own placement, so its own trace names no
                    // choice worth showing on a request drawer.
                    (admitted.trace.kind != Kind::Probe).then_some(&selection.trace),
                    selection.previous_input_tokens,
                )
                .await;
            // `inject_betas` is what this leg asks for; what the caller already sent is the head builder's to
            // find, and only the builder's own report joins `body_injected` in what a 400 may be matched against.
            // A five-minute body carries no extended beta, whichever leg wrote it: the caller's own
            // (`CLAUDE_CODE_PROMPT_CACHE_TTL=1h`) or one an earlier attempt injected.
            let mut drop_betas: Vec<&str> = Vec::new();
            if ttl == Some(CacheTtl::FiveMinutes) {
                drop_betas.push(EXTENDED_TTL_BETA);
            }
            // Off drops the beta on every request of the key, member or no member: after the client's own
            // fallback latch the beta stays in its header with nothing behind it. A caller-sent beta, so the
            // drop is never reported as the relay's own and a 400 naming it is the caller's.
            if !admitted.auto_mode_server {
                drop_betas.push(DANGEROUS_TOOL_USE_BETA);
            }
            // A latched or freshly stripped `ClientRequestId` means this leg generates none: the id is the
            // relay's own addition, and the account has already refused it once.
            let client_request_id = admitted
                .restore
                .as_ref()
                .filter(|_| !skip.contains(&Injected::ClientRequestId))
                .map(|_| uuid_v4());
            let head = outbound_headers(
                admitted.caller,
                admitted.original,
                &credential.kind,
                authority,
                wire_body.len(),
                gzip_out.then_some("gzip"),
                client_request_id.as_deref(),
                &inject_betas,
                &drop_betas,
            );
            let injected: Vec<Injected> = body_injected
                .iter()
                .chain(&head.injected)
                .copied()
                .collect();
            // The source of the member on this pass's body: a strip retry rebuilt the body without it, so its pass
            // reads none and remembers nothing.
            let diagnostics = source_of(
                admitted.client_diagnostics,
                body_injected.contains(&Injected::Diagnostics),
            );
            // Read off the body this pass sends, so the client's own member and the router's insertion are judged
            // alike.
            let diagnostics_named = diagnostics.is_some()
                && phoebis_cch::string_member_span(
                    &edited,
                    &["diagnostics", "previous_message_id"],
                )
                .is_some();
            // The head builder appends the beta only where the caller's list lacks it, so the two sources never meet,
            // and a pass rebuilt after a strip of the beta asks for it no more and reads none.
            let thinking_diagnostics = source_of(
                lists_beta(&head.headers, Injected::ThinkingBindingBeta.wire_name()),
                head.injected.contains(&Injected::ThinkingBindingBeta),
            );
            // After the selection's fact, so a client that leaves from here completes an attempt the ledger holds.
            admitted.trace.on_leg(*attempt, &account.id);
            let responding = Responding {
                account,
                admitted,
                attempt: *attempt,
                body: &edited,
                auth_marked: false,
                cache_ttl_ms: cache_ttl_ms(ttl),
                can_retry_in_place: !retried_in_place,
                can_strip: !stripped_once,
                cancel,
                credential_generation: credential.generation,
                diagnostics,
                diagnostics_named,
                end_guard: &mut *end_guard,
                fresh_pin: selection.pin.is_some() && !selection.trace.renewed,
                funding_ordinal,
                head: &mut leg_head,
                injected: &injected,
                key_permit: &mut *key_permit,
                // A probe's answer is never remembered: the id it was given belongs to the turn it replayed,
                // and recording it would make the next real turn name a probe.
                memory_key: admitted
                    .memory_key
                    .as_ref()
                    .filter(|_| admitted.trace.kind != Kind::Probe),
                pin_epoch,
                replayed,
                selection,
                seq,
                thinking_diagnostics,
                ticket,
            };
            let request = RawRequest {
                method: "POST",
                target: admitted.target.to_string(),
                headers: head.headers,
                body: wire_body.clone(),
            };
            let dispatched =
                match dispatch::attempt(self, &request, &pool_key, addr, fresh, cancel).await {
                    Err(failure) => Err(failure),
                    Ok(sent) => Ok(self
                        .respond(
                            responding,
                            sent,
                            Checkin {
                                pool: self.pool.clone(),
                                key: pool_key.clone(),
                            },
                        )
                        .await),
                };
            let dispatched = match dispatched {
                Err(AttemptFailure::Unsent { detail, connect }) if !retried => {
                    tracing::debug!(
                        account = %account.id,
                        %detail,
                        connect,
                        "retrying a request that never reached the upstream"
                    );
                    // A cancellation that ends the wait answers as one during the attempt, since nothing was written.
                    if connect
                        && until_canceled(tokio::time::sleep(dispatch::CONNECT_RETRY_WAIT), cancel)
                            .await
                            .is_none()
                    {
                        Err(AttemptFailure::Canceled)
                    } else {
                        fresh = true;
                        retried = true;
                        continue;
                    }
                }
                dispatched => dispatched,
            };
            let next = match dispatched {
                Ok(next) => next,
                Err(AttemptFailure::Canceled) => {
                    let outcome = admitted.outcome(
                        &account.id,
                        0,
                        *attempt,
                        false,
                        pin_epoch,
                        cache_ttl_ms(ttl),
                    );
                    return Leg::Respond(
                        self.answer_failure(admitted.trace, ACCOUNT_CANCELED, outcome)
                            .await,
                    );
                }
                Err(failure) => {
                    let written = matches!(failure, AttemptFailure::Lost(_));
                    tracing::warn!(account = %account.id, %failure, "the upstream did not answer");
                    ending(end_guard, TRANSPORT_ERROR);
                    let outcome = admitted.outcome(
                        &account.id,
                        0,
                        *attempt,
                        written,
                        pin_epoch,
                        cache_ttl_ms(ttl),
                    );
                    return Leg::Respond(
                        self.answer_failure(admitted.trace, UPSTREAM_UNREACHABLE, outcome)
                            .await,
                    );
                }
            };
            match next {
                Next::Replay(refreshed) => {
                    credential = refreshed;
                    replayed = true;
                    fresh = false;
                }
                Next::FailOver { auth_marked } => {
                    // The next leg's landing writes a mark of its own, which that leg's answer clears.
                    if let Some(leg_head) = leg_head.as_mut() {
                        leg_head.goes_on();
                    }
                    return Leg::FailOver { auth_marked };
                }
                // The leg answers the client with this pass, so no later head comes on it. A pass the leg retries in
                // place, replays or strips signals nothing, since the next pass answers for the leg.
                Next::Respond(response) => {
                    if let Some(leg_head) = leg_head.as_mut() {
                        leg_head.arrived((self.clock)());
                    }
                    return Leg::Respond(response);
                }
                // The same account and the same credential once the throttle has passed.
                Next::RetryInPlace(delay) => {
                    if until_canceled(tokio::time::sleep(delay), cancel)
                        .await
                        .is_none()
                    {
                        let outcome = admitted.outcome(
                            &account.id,
                            0,
                            *attempt,
                            true,
                            pin_epoch,
                            cache_ttl_ms(ttl),
                        );
                        return Leg::Respond(
                            self.answer_failure(admitted.trace, ACCOUNT_CANCELED, outcome)
                                .await,
                        );
                    }
                    retried_in_place = true;
                    fresh = false;
                }
                Next::Strip(items) => {
                    for item in items {
                        skip.insert(item);
                    }
                    // `skip` only grew from a set `build` already succeeded with, so the rebuild fails only if
                    // the body stopped being editable between two edits of the same bytes.
                    match build(&skip) {
                        Ok((rebuilt, restored, betas, rebuilt_ttl)) => {
                            edited = Bytes::from(rebuilt);
                            wire_body = on_the_wire(&edited);
                            body_injected = restored;
                            inject_betas = betas;
                            ttl = rebuilt_ttl;
                        }
                        Err(error) => {
                            tracing::error!(account = %account.id, %error, "a strip retry could not rebuild the body it just stripped from");
                            ending(end_guard, REFUSED);
                            return Leg::Respond(
                                self.refused_admitted(admitted, *attempt, relay_failure(&error))
                                    .await,
                            );
                        }
                    }
                    // The next pass submits the corrected body on this account, and `stripped_once` keeps the
                    // strip to one round per leg.
                    stripped_once = true;
                    fresh = false;
                    continue;
                }
            }
        }
    }

    /// The half of an attempt that runs once the head is in: the drain, the pre-commit hold, the verdict, and
    /// the response or the decision to move on.
    async fn respond(&self, mut ctx: Responding<'_>, sent: Sent<Conn>, checkin: Checkin) -> Next {
        let Sent {
            conn,
            head,
            leftover,
            written_at,
        } = sent;
        // Shared with the post-commit error hook, which outlives this call on the body task.
        let head = Arc::new(head);
        // When the head arrived, which is what stamps the overage evidence below for an upstream that
        // sends no `Date` of its own.
        let head_at = (self.clock)();
        let reading = HeadReading::parse(&head);
        let observed_at = reading.date.unwrap_or(head_at);
        let framing = framing(&head);
        let reusable = keep_alive(&head, &framing);
        let mut reader = BodyReader::new(conn, leftover, framing);
        let mut outcome = ctx.outcome(head.status);
        outcome.head_at = Some(head_at);
        outcome.upstream_request_id = head.get("request-id").map(str::to_string);
        outcome.overage_in_use = reading.unified.as_ref().and_then(|u| u.overage_in_use);
        self.report_overage(ctx.account, reading.unified.as_ref(), observed_at)
            .await;
        self.report_head_sample(
            ctx.account,
            &head,
            &reading,
            &ctx.admitted.identity.canonical_model,
            observed_at,
            &ctx.admitted.trace.request_id,
        )
        .await;

        // Heads the table reads a body for are drained within the bound. A body past the bound is classified from
        // the prefix the drain read as if it were the whole body, and keeps its reader for the rest should the
        // verdict forward it; a body drained whole has already handed its connection back to the pool.
        let (buffered, mut reader, cut) = if matches!(head.status, 400 | 401 | 403 | 404 | 429) {
            let Some(drained) = until_canceled(
                drain_bounded(&mut reader, self.limits.idle_timeout),
                ctx.cancel,
            )
            .await
            else {
                return Next::Respond(self.canceled(&ctx, &head, written_at).await);
            };
            match drained {
                Ok(Drained::Complete(buffered)) => {
                    if reusable {
                        checkin.clone().put(reader.into_inner());
                    }
                    (Some(buffered), None, false)
                }
                Ok(Drained::TooLarge(prefix)) => (Some(prefix), Some(reader), true),
                Err(error) => {
                    tracing::warn!(account = %ctx.account.id, %error, "the rejection body was not readable");
                    ctx.ending(TRANSPORT_ERROR);
                    ctx.answers();
                    return Next::Respond(
                        self.unreadable(ctx.admitted.trace, &error, outcome).await,
                    );
                }
            }
        } else {
            (None, Some(reader), false)
        };
        // Decoded once: the classification reads the text, and a body drained whole is forwarded as it decoded.
        let decoded = buffered.as_ref().map(|b| decode_drained(&head, b, cut));
        let text = decoded.as_ref().and_then(|d| d.as_deref().ok());
        // A streaming 200 is held until it commits: an error event ahead of `message_start` is a
        // rejection the caller must never see, because the request can still go to another account.
        let streaming = head.status == 200 && is_event_stream(&head);
        let mut held: Option<Held> = None;
        let mut first_event: Option<Bytes> = None;
        if let (true, Some(reader)) = (streaming, reader.as_mut()) {
            let inspect = Inspect::new(match encoding(&head) {
                Ok(Encoding::Identity) => None,
                Ok(coding) => Some(self.copy_decoder(coding)),
                Err(_) => None,
            });
            let Some(result) =
                until_canceled(hold(reader, inspect, self.limits.hold_timeout), ctx.cancel).await
            else {
                return Next::Respond(self.canceled(&ctx, &head, written_at).await);
            };
            match result {
                Ok(h) => {
                    if let Commit::Error(data) = &h.commit {
                        first_event = Some(data.clone());
                    }
                    held = Some(h);
                }
                Err(error) => {
                    tracing::warn!(account = %ctx.account.id, %error, "the stream ended before it committed");
                    ctx.ending(TRANSPORT_ERROR);
                    ctx.answers();
                    return Next::Respond(
                        self.unreadable(ctx.admitted.trace, &error, outcome).await,
                    );
                }
            }
        }
        // Read once for the verdict, for the attempt's recorded outcome and for the rate-limit families a
        // committed error keeps.
        let rate_limit_event = first_event.as_deref().is_some_and(is_rate_limit_event);
        // The commit's own arrival time: a held stream may have taken most of the hold timeout to commit.
        let arrived = (self.clock)();
        // A stream that commits on `message_start` is the leg's final answer: the upstream writes the cache from here.
        if let (Some(leg_head), Some(Commit::MessageStart(_))) =
            (ctx.head.as_mut(), held.as_ref().map(|h| &h.commit))
        {
            leg_head.arrived(arrived);
        }
        if !ctx.can_strip && latches(ctx.admitted.trace) {
            // This leg cannot retry a strip, so the 400 is forwarded as it came; the account is still latched
            // against the item it named, a fact about the account whether or not this request could use it.
            for item in refused_injected(head.status, text, ctx.injected) {
                self.memory.latch(ctx.account, item);
            }
        }
        let verdict = classify(
            &head,
            &HeadContext {
                kind: ctx.account.kind,
                canonical_model: &ctx.admitted.identity.canonical_model,
                now: arrived,
                fallback_ms: ctx.admitted.fallback_ms,
                rate_limit_event,
                body_prefix: text,
                injected: ctx.injected,
                can_strip: ctx.can_strip,
                wire_model: &ctx.admitted.identity.folded_model,
                model_rejection_ms: self.limits.model_rejection.as_millis() as i64,
                fast: ctx.admitted.fast,
                reading: &reading,
                shared_windows: ctx.selection.shared_windows,
            },
        );
        let committing = match held.as_ref().map(|h| &h.commit) {
            Some(Commit::MessageStart(data) | Commit::Error(data)) => Some(data.clone()),
            _ => None,
        };
        let cooldown_seen = match &verdict {
            Verdict::FailOver { cooldown, .. } => cooldown.as_ref(),
            _ => None,
        };
        let auth_failed = matches!(
            verdict,
            Verdict::Refresh
                | Verdict::FailOver {
                    mark: Some((AccountState::ReauthRequired, _)),
                    ..
                }
        );
        let classified = attempt_outcome(&verdict, head.status, rate_limit_event);
        ctx.admitted
            .trace
            .head(
                ctx.attempt,
                &head,
                committing,
                classified,
                cooldown_set(cooldown_seen, &ctx.admitted.identity.canonical_model),
                written_at,
                head_at,
            )
            .await;
        let generations = (
            ctx.account.incarnation,
            ctx.account.lifecycle_generation,
            ctx.credential_generation,
        );
        // A mark this head forms carries the upstream's own words for it to the log's row.
        let marked = |(state, cause): (AccountState, StateCause)| {
            (state, cause, text.and_then(error_message))
        };

        let (cooldown, mark, moves_pin) = match verdict {
            // A rejected replay is conclusive: the refreshed token was refused too. The cache refuses that token's
            // lineage at once, so no leg dials it again while the report below reaches the snapshot.
            Verdict::Refresh if ctx.replayed => {
                self.manager
                    .refuse_rejected(ctx.account, ctx.credential_generation);
                (
                    None,
                    Some((AccountState::ReauthRequired, StateCause::AuthFailed)),
                    false,
                )
            }
            Verdict::Refresh => {
                // Nothing of the rejection goes back, so a body cut at the bound gives up its connection before
                // the ladder's waits rather than hold the rest unread through them.
                drop(reader.take());
                let mut tries = 0u8;
                loop {
                    tries += 1;
                    match self
                        .manager
                        .refresh_now(&ctx.account.id, ctx.credential_generation)
                        .await
                    {
                        Ok(token) => return Next::Replay(Credential::from(token)),
                        // Once the ladder ends nothing was replaced, so the account is finished for this request
                        // and nothing of the upstream's 401 goes back: the request fails over, and a request with
                        // no account left, or one bound to this account, is answered the router's own exhaustion.
                        // An account the refresh found marked, by its own invalid_grant or by a disconnect
                        // meanwhile, keeps its pin for the next selection to end under the account's state; any
                        // other failure moves the pin.
                        Err(error) => {
                            tracing::warn!(account = %ctx.account.id, %error, "the rejected token could not be refreshed");
                            let marked = matches!(
                                error,
                                AcquireError::Disconnected
                                    | AcquireError::ReauthRequired(..)
                                    | AcquireError::Unpaired
                            );
                            if marked || tries == 2 {
                                // A disconnect reached the registry through the admin's own reload; the
                                // re-authentication mark is the manager's store write alone, so it is reported:
                                // the routing writer's fenced write and the reload after it take the snapshot's
                                // row off `ready`, and until that reload the manager's cache refuses the marked
                                // credential, so no leg dials the account in between. The word is the manager's,
                                // so the re-report stores what its own write stored.
                                let mark = matches!(error, AcquireError::ReauthRequired(..))
                                    .then_some((
                                        AccountState::ReauthRequired,
                                        StateCause::RefreshFailed,
                                    ));
                                break (None, mark, !marked);
                            }
                            // One wait on the same account, bounded by the manager's own rule: a short refusal
                            // clears in seconds and a token endpoint's bad minute in a minute, both cheaper than
                            // moving a warm session; a backoff asked for past the floor is not waited out, and
                            // the minute is waited out once per request, since a second account's bad minute is
                            // the same minute. The armed length tells the two apart, not the time left, which a
                            // clock that moved since the arming would misread.
                            let Some(backoff) = self.manager.backoff(&ctx.account.id) else {
                                break (None, None, true);
                            };
                            let floor = backoff.length_ms > phoebis_oauth::TRANSIENT_BACKOFF_MS;
                            if backoff.length_ms > phoebis_oauth::SERVER_ERROR_BACKOFF_MS
                                || (floor && ctx.admitted.floor_held.load(Ordering::Relaxed))
                            {
                                break (None, None, true);
                            }
                            if floor {
                                ctx.admitted.floor_held.store(true, Ordering::Relaxed);
                            }
                            match self
                                .wait_out(backoff.until, ctx.admitted.deadline, ctx.cancel, None)
                                .await
                            {
                                Waited::Cleared | Waited::Head => {}
                                // The loop head answers the exhaustion past the deadline; this account is done.
                                Waited::Deadline => break (None, None, true),
                                // The 401's head is this attempt's, published before the verdict.
                                Waited::Canceled => {
                                    ctx.answers();
                                    return Next::Respond(
                                        self.canceled_after_head(&ctx, head.status).await,
                                    );
                                }
                            }
                        }
                    }
                }
            }
            // `classify` only returns this under `can_strip`, so the one-round rule is already applied; without
            // it the same 400 classifies as `Commit` and is forwarded as any other.
            Verdict::StripInjected(items) => {
                // The retry itself still runs whatever the request is: it corrects this leg's own edit.
                if latches(ctx.admitted.trace) {
                    for item in &items {
                        self.memory.latch(ctx.account, *item);
                    }
                }
                return Next::Strip(items);
            }
            // An account marked for re-authentication keeps its pin, as a refresh's own mark does: the next
            // selection moves the session off it under the account's state.
            Verdict::FailOver { cooldown, mark } => (
                cooldown,
                mark,
                mark.map(|(state, _)| state) != Some(AccountState::ReauthRequired),
            ),
            Verdict::Commit => {
                ctx.answers();
                // A message request's success on a `funding_required` account is the retry that clears its mark. A
                // token count is admitted under the same rule, but a counting answer says nothing about the
                // balance, and neither does an error event the held stream committed on.
                if ctx.account.state == AccountState::FundingRequired
                    && ctx.admitted.trace.kind != Kind::CountTokens
                    && (200..300).contains(&head.status)
                    && first_event.is_none()
                {
                    self.router
                        .report(Report::AccountState {
                            account_id: ctx.account.id.clone(),
                            state: AccountState::Ready,
                            incarnation: generations.0,
                            lifecycle_generation: generations.1,
                            credential_generation: generations.2,
                            cause: StateCause::FundingRetry,
                            detail: None,
                            dispatched_ordinal: ctx.funding_ordinal,
                        })
                        .await;
                }
                if let (Some(key), Some(id)) = (ctx.memory_key, head.get("request-id")) {
                    self.memory
                        .record_request_id(key, &ctx.account.id, id, arrived)
                        .await;
                }
                // Built once and moved into whichever branch below commits: a JSON answer and a streamed one
                // both reach the caller through `stream()`, so both parse for a message id the same way. An
                // answer is remembered only for a request that carried or received the member, so a helper the
                // gate refused never becomes the next turn's named predecessor.
                let on_message_id: Option<OnMessageId> = ctx
                    .memory_key
                    .filter(|_| ctx.diagnostics.is_some())
                    .map(|key| {
                        let memory = self.memory.clone();
                        let key = key.clone();
                        let account_id = ctx.account.id.clone();
                        // The same wire-spelling key `previous` reads above, not the resolved identity: see its
                        // comment.
                        let model = ctx.admitted.identity.folded_model.clone();
                        let clock = self.clock.clone();
                        Box::new(move |id: String| {
                            // Awaited on the body task before the next piece goes out, so the caller's next
                            // request finds the id in memory and a shutdown flush carries it; the restore is
                            // a store read only on a miss. Stamped at fire time, so a slow body never moves
                            // `last_seen` back to the head's clock.
                            Box::pin(async move {
                                let now = (clock)();
                                memory
                                    .record_message_id(&key, &account_id, &model, &id, now)
                                    .await;
                            }) as BoxFuture<'static, ()>
                        }) as OnMessageId
                    });
                // Sent where the answer ends, the only place its usage is known.
                let renewal = self.renewal(&ctx);
                let affinity = self.affinity(&ctx);
                let (prefix, held, on_error) = match held {
                    None => (Bytes::new(), None, None),
                    Some(Held { bytes, inspect, .. }) => {
                        let fallback_ms = ctx.admitted.fallback_ms;
                        let model_rejection_ms = self.limits.model_rejection.as_millis() as i64;
                        let fast = ctx.admitted.fast;
                        let shared_windows = ctx.selection.shared_windows;
                        // Two spellings captured for two purposes that diverge once a resolution lands: the
                        // canonical id is the cooldown's scope and the failover reports' model, the folded
                        // spelling is the wire model `classify`'s rejection arm matches the upstream's own
                        // words against.
                        let (
                            router,
                            clock,
                            account_id,
                            kind,
                            canonical_model,
                            wire_model,
                            pin,
                            request_id,
                        ) = (
                            self.router.clone(),
                            self.clock.clone(),
                            ctx.account.id.clone(),
                            ctx.account.kind,
                            ctx.admitted.identity.canonical_model.clone(),
                            ctx.admitted.identity.folded_model.clone(),
                            ctx.admitted
                                .moves_pins
                                .then(|| ctx.selection.pin.clone())
                                .flatten(),
                            ctx.admitted.trace.request_id.clone(),
                        );
                        let head = Arc::clone(&head);
                        let on_error = Box::new(move |data: Bytes, waiting: &mut Waiting| {
                            // After commit nothing fails over. A rate-limit error records the cooldown
                            // and, on a portable request, invalidates the pin so the next request
                            // migrates. A non-portable request keeps its pin. The deadline is measured
                            // from the error's arrival, not from the selection.
                            let now = clock();
                            let verdict = classify(
                                &head,
                                &HeadContext {
                                    kind,
                                    canonical_model: &canonical_model,
                                    now,
                                    fallback_ms,
                                    rate_limit_event: is_rate_limit_event(&data),
                                    body_prefix: None,
                                    // Nothing is stripped after a commit: this observer only records a
                                    // rate-limit cooldown, and a 400 (which alone can name an injected item)
                                    // never reaches it.
                                    injected: &[],
                                    can_strip: false,
                                    wire_model: &wire_model,
                                    model_rejection_ms,
                                    fast,
                                    reading: &reading,
                                    shared_windows,
                                },
                            );
                            if let Verdict::FailOver { cooldown, mark } = verdict {
                                for report in failover_reports(
                                    &account_id,
                                    &canonical_model,
                                    cooldown,
                                    mark.map(|(state, cause)| (state, cause, None)),
                                    pin,
                                    &request_id,
                                    generations,
                                    now,
                                ) {
                                    waiting.send(&router, report);
                                }
                            }
                        }) as OnError;
                        (bytes, Some(inspect), Some(on_error))
                    }
                };
                let responded = match (decoded, reader) {
                    (Some(decoded), None) => {
                        let guards = ctx.guards();
                        self.forward_buffered(
                            ctx.admitted.trace,
                            &head,
                            decoded,
                            outcome,
                            renewal,
                            affinity,
                            guards,
                        )
                        .await
                    }
                    // A body cut at the bound goes out from the prefix the drain read, a held stream from its held
                    // bytes, and the reader produces the rest.
                    (_, reader) => {
                        let guards = ctx.guards();
                        self.stream(Forward {
                            affinity,
                            cancel: ctx.cancel.clone(),
                            checkin,
                            guards,
                            head: &head,
                            held,
                            incarnation: ctx.account.incarnation,
                            keep_alive: reusable,
                            on_error,
                            on_message_id,
                            outcome,
                            prefix: buffered.unwrap_or(prefix),
                            rate_limit_event: false,
                            reader: reader.expect(
                                "a head the table reads no body for, or one cut at the bound, still holds its reader",
                            ),
                            renewal,
                            trace: ctx.admitted.trace,
                        })
                        .await
                    }
                };
                return Next::Respond(responded);
            }
        };
        let marked_state = mark.map(|(state, _)| state);
        if marked_state == Some(AccountState::ReauthRequired) {
            ctx.auth_marked = true;
        }
        let retry_in_place = self.retry_in_place(
            &ctx,
            &head,
            reading.date,
            cooldown.as_ref(),
            marked_state,
            arrived,
        );
        // A non-portable request keeps its pin: the answer goes back to the client on the account that holds
        // its state, and only the cooldown is recorded. So does one about to retry in place: the row is there
        // for the siblings, and the pin stays where the retry lands.
        let hands_pin = ctx.admitted.moves_pins && moves_pin && retry_in_place.is_none();
        for report in failover_reports(
            &ctx.account.id,
            &ctx.admitted.identity.canonical_model,
            cooldown,
            mark.map(marked),
            hands_pin.then(|| ctx.selection.pin.clone()).flatten(),
            &ctx.admitted.trace.request_id,
            generations,
            arrived,
        ) {
            self.router.report(report).await;
        }
        if let Some(delay) = retry_in_place {
            drop(reader);
            return Next::RetryInPlace(delay);
        }
        // A held stream cannot be rewound onto another connection, so the only way not to commit it is to drop
        // it unread; a request bound to its account takes it as it came.
        if let Some(held) = held {
            if ctx.admitted.portable {
                drop(reader);
                return Next::FailOver {
                    auth_marked: ctx.auth_marked,
                };
            }
            ctx.answers();
            let guards = ctx.guards();
            return Next::Respond(
                self.stream(Forward {
                    affinity: None,
                    cancel: ctx.cancel.clone(),
                    checkin,
                    guards,
                    head: &head,
                    held: Some(held.inspect),
                    incarnation: ctx.account.incarnation,
                    keep_alive: false,
                    on_error: None,
                    on_message_id: None,
                    outcome,
                    prefix: held.bytes,
                    rate_limit_event,
                    reader: reader
                        .expect("a streaming head still holds the reader the hold read from"),
                    renewal: None,
                    trace: ctx.admitted.trace,
                })
                .await,
            );
        }
        if ctx.admitted.portable {
            return Next::FailOver {
                auth_marked: ctx.auth_marked,
            };
        }
        // A request bound to its account takes every head from here as its answer.
        ctx.answers();
        if auth_failed {
            // A non-portable request keeps its account, so its authentication failure is the last word here.
            let release = moves_pin
                .then(|| self.release_on(&ctx, head.status))
                .flatten();
            return Next::Respond(
                self.authentication_exhausted(&mut ctx, &head, outcome, release)
                    .await,
            );
        }
        // Every failover row the table produces answers 400, 401, 403, 404, or 429, all of which were
        // drained above, so the defaults below are the empty body of a head that carried none. A row
        // added for a status outside that set would forward nothing instead of its own body.
        let Some(reader) = reader else {
            // The answer the client gets is the upstream's rejection, and the guards stay with this pass.
            ctx.ending(REFUSED);
            return Next::Respond(
                self.forward_buffered(
                    ctx.admitted.trace,
                    &head,
                    decoded.unwrap_or(Ok(Bytes::new())),
                    outcome,
                    None,
                    None,
                    Guards::default(),
                )
                .await,
            );
        };
        // A body cut at the bound: the prefix the drain read, then the rest.
        let guards = ctx.guards();
        Next::Respond(
            self.stream(Forward {
                affinity: None,
                cancel: ctx.cancel.clone(),
                checkin,
                guards,
                head: &head,
                held: None,
                incarnation: ctx.account.incarnation,
                keep_alive: reusable,
                on_error: None,
                on_message_id: None,
                outcome,
                prefix: buffered.unwrap_or_default(),
                rate_limit_event: false,
                reader,
                renewal: None,
                trace: ctx.admitted.trace,
            })
            .await,
        )
    }

    /// The router's own answer when an account's authentication failure is the request's last word: the
    /// untimed exhaustion body, so the client sees no account and no reason. `release` is the fresh pin this
    /// request created, sent before the answer as the forwarded error would have sent it while the router's
    /// mailbox has room; on a full one it waits on a tracked task after the answer, holding the request's guards.
    async fn authentication_exhausted(
        &self,
        ctx: &mut Responding<'_>,
        head: &ResponseHead,
        mut outcome: RelayOutcome,
        release: Option<Renewal>,
    ) -> Response {
        let mut waiting = Waiting::default();
        if let Some(release) = release {
            release.send(
                (self.clock)(),
                head.status,
                None,
                &ResponseFacts::default(),
                &mut waiting,
            );
        }
        ctx.ending(EXHAUSTED);
        let mut failure = exhaustion_failure(&Exhaustion::Untimed, (self.clock)());
        // A probe bound to its account never reaches the selector's exhaustion, so its own answer names the mark,
        // which refuses the probe alone.
        if ctx.admitted.trace.kind == Kind::Probe && ctx.auth_marked {
            outcome.obstacle = Some(ProbeObstacle::ReauthRequired);
            failure.class = FailureClass::Rejected;
        }
        let response = self
            .answer_failure(ctx.admitted.trace, failure, outcome)
            .await;
        self.after_answer(waiting, ctx.guards());
        response
    }

    /// Runs the reports a full mailbox left waiting on a task the shutdown waits on, holding the request's guards
    /// until they are queued: the client's answer never waits on the router, and the key permits still bound the
    /// reports a parked loop can hold.
    fn after_answer(&self, waiting: Waiting, guards: Guards) {
        if !waiting.is_empty() {
            self.bodies.spawn(async move {
                waiting.land().await;
                drop(guards);
            });
        }
    }

    /// The report a pinned answer sends at its end: a renewal, or the release of a pin this request created and
    /// answered with a client error.
    fn renewal(&self, ctx: &Responding<'_>) -> Option<Renewal> {
        ctx.selection.pin.as_ref().map(|pin| Renewal {
            router: self.router.clone(),
            pin: pin.clone(),
            keepalive: ctx.admitted.trace.kind == Kind::Probe,
            cache_ttl_ms: ctx
                .cache_ttl_ms
                .or(ctx.admitted.markers_ttl_ms)
                .unwrap_or(DEFAULT_CACHE_TTL_MS),
            fresh: ctx.fresh_pin,
            turn: ctx.admitted.turn,
            subagent: ctx.admitted.subagent,
            renews: ctx.admitted.trace.kind != Kind::CountTokens,
            generations: (
                ctx.account.incarnation,
                ctx.account.lifecycle_generation,
                ctx.credential_generation,
            ),
        })
    }

    /// The conversation report a committed answer sends once its usage is known: every fact the request knows,
    /// whatever the cache did and whether or not the request holds a pin, so a marker-less conversation still
    /// links its turns. `None` for a token count, which writes no cache.
    fn affinity(&self, ctx: &Responding<'_>) -> Option<AffinityFacts> {
        if ctx.admitted.trace.kind == Kind::CountTokens {
            return None;
        }
        // The contract refuses a request without a session id at admission, before this path, so one that reached it
        // would send no conversation or usage report rather than panic.
        let root = ctx.admitted.identity.session_id.clone()?;
        // An auxiliary request leaves no content entry: the client's quota probe has one message every probe of
        // the key shares, so an entry under it would read the next session's probe as a fork of this one. Its
        // static boundaries still land, a cache of their own the next probe of the same shape reads.
        let auxiliary = ctx.admitted.auxiliary;
        let ttls = marker_ttls(ctx.body);
        Some(AffinityFacts {
            router: self.router.clone(),
            pending: self.pending_reports.clone(),
            account_id: ctx.account.id.clone(),
            incarnation: ctx.account.incarnation,
            key_id: ctx.admitted.key.id.clone(),
            key_generation: ctx.admitted.key.created_at,
            canonical_model: ctx.admitted.identity.canonical_model.clone(),
            entrypoint: ctx.admitted.identity.entrypoint.clone(),
            judged: ctx.selection.judged.clone(),
            lane: ctx.admitted.lane.clone(),
            root,
            static_boundaries: ctx
                .admitted
                .chains
                .map(|c| c.static_boundaries.clone())
                .unwrap_or_default(),
            static_sizes: ctx
                .admitted
                .chains
                .map(|c| c.static_sizes.clone())
                .unwrap_or_default(),
            message_boundaries: ctx
                .admitted
                .chains
                .filter(|_| !auxiliary)
                .map(|c| c.message_boundaries.clone())
                .unwrap_or_default(),
            static_ttl_ms: ttls.tiers.0,
            message_ttl_ms: ttls.tiers.1,
            uniform_ttl_ms: ttls.uniform,
            request_id: ctx.admitted.trace.request_id.clone(),
            seq: ctx.seq,
            attempt: u32::from(ctx.attempt),
            remembers: ctx.diagnostics.is_some() && ctx.admitted.trace.kind != Kind::Probe,
        })
    }

    /// For a rejection forwarded with no renewal: the release of the pin this request created, when the status
    /// is a client error that ends it, and nothing otherwise, so the pin a renewal would have kept stays as it is.
    fn release_on(&self, ctx: &Responding<'_>, status: u16) -> Option<Renewal> {
        self.renewal(ctx).filter(|renewal| renewal.releases(status))
    }

    /// How long to wait before trying the same account again, for a fail-over this leg can wait out instead: a
    /// portable request, a throttle the upstream timed with a `Retry-After` at or under the pool's bound that
    /// ends before the request's deadline, no mark on the account, and no retry in place on this leg yet.
    /// Anything else fails over.
    fn retry_in_place(
        &self,
        ctx: &Responding<'_>,
        head: &ResponseHead,
        date: Option<i64>,
        cooldown: Option<&Cooldown>,
        mark: Option<AccountState>,
        now: i64,
    ) -> Option<Duration> {
        let cooldown = cooldown?;
        if !ctx.admitted.portable
            || mark.is_some()
            || !ctx.can_retry_in_place
            || cooldown.window != phoebis_routing::RETRY_AFTER_WINDOW
            || retry_after(head, date, now).is_none()
        {
            return None;
        }
        let delay = cooldown.wait_from(now);
        let delay = Duration::from_millis(u64::try_from(delay).unwrap_or(0));
        // The deadline bounds this wait as it bounds the refresh ladder's: the retry a longer wait leads to would
        // be submitted after the request is due its answer, with its permits and buffers held all the while, so
        // the request moves on at once instead.
        let left = ctx.admitted.deadline.map(|deadline| {
            Duration::from_millis(u64::try_from(deadline.saturating_sub(now)).unwrap_or(0))
        });
        (delay <= ctx.admitted.retry_in_place && left.is_none_or(|left| delay < left))
            .then_some(delay)
    }

    /// The credential to dispatch with, from the same snapshot lineage as the row the selection chose: the row
    /// (the router's snapshot at selection), the secret or bundle (the cache entry of that row's holder and
    /// credential generation, read from the registry on a miss alone) and the fence (the router's snapshot once
    /// the credential is in hand), so a worker never waits behind the session memory's flush or an admin write
    /// while the lineage holds. A reload between selection and dispatch that moved the holder, the revision or the
    /// generation past the credential refuses `Stale`. A write landed but not yet swapped passes with the old row
    /// and the cached credential, which belong together; a miss inside that window reads the newer credential,
    /// which goes out under the old row for the one load the reload takes.
    async fn credential(&self, account: &AccountRow) -> Result<Credential, NoCredential> {
        let expected = self.router.snapshot().credential_generation(&account.id);
        let credential = match account.kind {
            AccountKind::Oauth => match self.manager.acquire_for(account, expected).await {
                Ok(token) => Credential::from(token),
                Err(error) => {
                    tracing::warn!(account = %account.id, %error, "no credential for the selected account");
                    // The manager's store write starts no reload, so a mark this acquisition's refresh landed
                    // reaches routing through the report the ladder sends for its own.
                    if let Some(mark) = error.landed_mark() {
                        self.router.report(Report::from(mark)).await;
                        return Err(NoCredential::Marked);
                    }
                    return Err(NoCredential::Unavailable);
                }
            },
            // The generation comes along so an api_key verdict is fenced by the same rule an OAuth one is.
            AccountKind::ApiKey => match self.api_key_secret(account, expected).await {
                Ok(Some((secret, generation))) => Credential {
                    account_uuid: None,
                    generation,
                    kind: CredentialKind {
                        kind: AccountKind::ApiKey,
                        secret,
                    },
                },
                Ok(None) => {
                    tracing::warn!(account = %account.id, "the account has no API key stored");
                    return Err(NoCredential::Unavailable);
                }
                Err(error) => {
                    tracing::error!(account = %account.id, %error, "the stored API key is unreadable");
                    return Err(NoCredential::Unavailable);
                }
            },
        };
        // Read after the awaits, on the snapshot in force now: an edit and a reload that landed while the
        // credential was being acquired moved the row or its holder, and the credential in hand may be the old
        // one. A generation ahead of the snapshot's under the same holder and revision is a refresh the reload has
        // not caught up with and passes; one behind it is a cache a rotation outran and refuses.
        let current = self.router.snapshot();
        match current.account(&account.id) {
            Some(row)
                if row.incarnation == account.incarnation
                    && row.revision == account.revision
                    && current
                        .credential_generation(&account.id)
                        .is_none_or(|snapshot| credential.generation >= snapshot) =>
            {
                Ok(credential)
            }
            _ => {
                tracing::warn!(
                    account = %account.id,
                    "the account changed while its credential was being acquired",
                );
                Err(NoCredential::Stale)
            }
        }
    }

    /// The account's API key and its generation: the cached entry while its holder and lifecycle are the
    /// row's and its generation is at or above the snapshot's, else one store read on a blocking thread, which
    /// replaces the entry under the row's lineage and the generation the store returned. The snapshot's figure
    /// catches up with the store's at the next reload, so a mismatch between the two costs one more read at most.
    async fn api_key_secret(
        &self,
        account: &AccountRow,
        expected: Option<u64>,
    ) -> Result<Option<(Secret, u64)>, String> {
        let cached = self
            .secrets
            .lock()
            .expect(SECRETS_POISONED)
            .get(&account.id)
            .filter(|entry| {
                entry.incarnation == account.incarnation
                    && entry.lifecycle_generation == account.lifecycle_generation
                    && expected.is_none_or(|expected| entry.generation >= expected)
            })
            .map(|entry| (Secret::new(entry.secret.expose()), entry.generation));
        if cached.is_some() {
            return Ok(cached);
        }
        let store = self.store.clone();
        let id = account.id.clone();
        let read = tokio::task::spawn_blocking(move || store.api_key_credential(&id))
            .await
            .map_err(|error| format!("the key read failed: {error}"))?
            .map_err(|error| error.to_string())?;
        // The walk reads its own snapshot: a reload's purge is what takes an id out, and the next miss drops its
        // entry here.
        let current = self.router.snapshot();
        let mut secrets = self.secrets.lock().expect(SECRETS_POISONED);
        secrets.retain(|id, _| current.account(id).is_some());
        let Some((secret, generation)) = read else {
            return Ok(None);
        };
        // A read that lost a race to a newer one of the same holder would put the older key back.
        let newer = secrets.get(&account.id).is_some_and(|entry| {
            entry.incarnation == account.incarnation
                && entry.lifecycle_generation == account.lifecycle_generation
                && entry.generation > generation
        });
        if !newer {
            secrets.insert(
                account.id.clone(),
                SecretEntry {
                    incarnation: account.incarnation,
                    lifecycle_generation: account.lifecycle_generation,
                    generation,
                    secret: Secret::new(secret.expose()),
                },
            );
        }
        Ok(Some((secret, generation)))
    }

    /// Reads the ledger's standing for a request refused or admitted before any dial, once per request. `counts`
    /// is true at the ledger check alone, where a `behind` reading is a `ledger_behind` answer the ledger counts; a
    /// refusal of another kind reads without counting. Behind, the request's own facts go best-effort: a writer that
    /// stopped draining fills the bus queue behind its channel, and a refusal that waited there would hold its
    /// connection. A request admitted past the reading publishes durably, so a priced record never takes that path.
    fn ledger_standing(&self, trace: &Trace, counts: bool) -> bool {
        let behind = self.ledger_behind(counts);
        if behind {
            trace.facts_best_effort();
        }
        behind
    }

    /// Whether the accounting source reads the ledger behind, `counts` as `ledger_standing` says. Without a source the
    /// ledger never stands behind.
    pub(crate) fn ledger_behind(&self, counts: bool) -> bool {
        self.accounting
            .as_ref()
            .is_some_and(|source| source(counts).behind)
    }

    /// A refusal with no body to observe: decided on the head before the read, on a read that ended without a
    /// whole body, or on a whole body refused before its parse. The observation carries the client facts the head
    /// alone gives. `key` is `None` for a refusal decided before a key was known.
    #[allow(clippy::too_many_arguments)]
    async fn refused_before_body(
        &self,
        trace: &Trace,
        key: Option<&ApiKeyRow>,
        headers: &HeaderMap,
        capture_hours: u32,
        presented_retired: bool,
        failure: Failure,
        client_version: Option<&str>,
    ) -> Response {
        self.ledger_standing(trace, false);
        trace
            .observed(
                key,
                None,
                headers,
                false,
                Bytes::new(),
                false,
                capture_hours,
                RequestShape::default(),
                None,
                client_facts(headers, &ClientHints::read(headers), None, None),
                None,
                presented_retired,
                key.and_then(|k| k.rotated_at),
                None,
                Vec::new(),
                None,
                false,
            )
            .await;
        self.refused(trace, 0, failure, key, None, client_version)
            .await
    }

    async fn refused(
        &self,
        trace: &Trace,
        attempt: u8,
        refusal: impl Into<LocalRefusal>,
        key: Option<&ApiKeyRow>,
        identity: Option<&RequestIdentity>,
        client_version: Option<&str>,
    ) -> Response {
        let LocalRefusal {
            failure,
            client_drift,
            client_contract,
        } = refusal.into();
        let mut outcome = relay_outcome(
            trace,
            key,
            identity,
            "",
            0,
            attempt,
            false,
            client_version.map(str::to_string),
            None,
            None,
        );
        outcome.client_drift = client_drift;
        outcome.client_contract = client_contract;
        self.answer_failure(trace, failure, outcome).await
    }

    async fn refused_admitted(
        &self,
        admitted: &Admitted<'_>,
        attempt: u8,
        refusal: impl Into<LocalRefusal>,
    ) -> Response {
        self.refused(
            admitted.trace,
            attempt,
            refusal,
            Some(admitted.key),
            Some(admitted.identity),
            admitted.client_version.as_deref(),
        )
        .await
    }

    /// A local failure's answer, the ledger filing the code of the same `Failure` the caller reads.
    async fn answer_failure(
        &self,
        trace: &Trace,
        failure: Failure,
        mut outcome: RelayOutcome,
    ) -> Response {
        outcome.answered(failure.code, failure.class.outcome(outcome.attempt));
        self.answer(trace, failure.response(&trace.request_id), outcome)
            .await
    }

    /// A client-drift refusal with its trigger, the warn that tells the operator whether a re-pin cures it, and the
    /// refusal's record as the request's one decision. The record is try-sent, so a full router queue drops it
    /// rather than delaying the 403. A moved client is cured by the re-pin, so a hash drift and a resumed body's
    /// suffix warn with it, and a suffix on a conversation that was not resumed warns with its refusal's condition,
    /// that a re-pin does not help where the session's main turns pass. A resumed body warns `resume` and its suffix
    /// refusal's words name the resume, so the operator sees the refused conversation was a resumed one.
    #[allow(clippy::too_many_arguments)]
    fn drift_refusal(
        &self,
        kind: DriftKind,
        resumed: bool,
        headers: &HeaderMap,
        trace: &Trace,
        key: &ApiKeyRow,
        identity: &RequestIdentity,
        client_version: Option<&str>,
    ) -> LocalRefusal {
        let advice = drift_words(kind, resumed).1;
        tracing::warn!(
            drift = %kind.as_str(),
            user_agent = %user_agent(headers),
            profile = %self.profile_name,
            resume = resumed,
            "a caller stamped a billing line this build cannot reproduce. {advice}",
        );
        self.router.record_refusal(
            RefusedRequest {
                request_id: trace.request_id.clone(),
                attempt: 1,
                seq: 1,
                at: (self.clock)(),
                key_id: key.id.clone(),
                key_generation: key.created_at,
                agent_id: identity.agent_id.clone(),
                parent_agent_id: None,
                session_id: identity.session_id.clone(),
                request_class: None,
                canonical_model: identity.canonical_model.clone(),
                lane: LaneFacts::default(),
                flags: RequestFlags::default(),
                chain: ChainFacts::default(),
                attempted: Vec::new(),
                holds: Vec::new(),
                client: identity.class,
                client_version: client_version.map(str::to_string),
            },
            RefusalFacts {
                kind: "client",
                code: "drift",
                reason: kind.as_str().to_string(),
            },
        );
        LocalRefusal {
            failure: relay_failure(&RelayError::ClientDrift { kind, resumed }),
            client_drift: Some(kind),
            client_contract: None,
        }
    }

    /// A contract refusal with its item, the warn that tells the operator which environment line the client lacks,
    /// and the refusal's record as the request's one decision. The record keeps the class the request named, so
    /// the routing log shows it, and is try-sent as the drift's is.
    #[allow(clippy::too_many_arguments)]
    fn contract_refusal(
        &self,
        item: &'static str,
        headers: &HeaderMap,
        trace: &Trace,
        key: &ApiKeyRow,
        identity: &RequestIdentity,
        request_class: Option<&str>,
        client_version: Option<&str>,
    ) -> LocalRefusal {
        tracing::warn!(
            item = %item,
            user_agent = %user_agent(headers),
            key = %key.id,
            "a Claude Code client sent a request without the {item} the router requires. \
             Read the client page for the environment block",
        );
        self.router.record_refusal(
            RefusedRequest {
                request_id: trace.request_id.clone(),
                attempt: 1,
                seq: 1,
                at: (self.clock)(),
                key_id: key.id.clone(),
                key_generation: key.created_at,
                agent_id: identity.agent_id.clone(),
                parent_agent_id: None,
                session_id: identity.session_id.clone(),
                request_class: request_class.map(str::to_string),
                canonical_model: identity.canonical_model.clone(),
                lane: LaneFacts::default(),
                flags: RequestFlags {
                    count: trace.kind == Kind::CountTokens,
                    probe: trace.kind == Kind::Probe,
                    ..RequestFlags::default()
                },
                chain: ChainFacts::default(),
                attempted: Vec::new(),
                holds: Vec::new(),
                client: identity.class,
                client_version: client_version.map(str::to_string),
            },
            RefusalFacts {
                kind: "client",
                code: "contract",
                reason: item.to_string(),
            },
        );
        LocalRefusal {
            failure: relay_failure(&RelayError::ClientContract { item }),
            client_drift: None,
            client_contract: Some(item),
        }
    }

    /// A locally built answer: the outcome rides on the response for the accounting layer, and the completion
    /// is published with the status the response carries, which for a local refusal is the refusal's own. Built
    /// entirely in memory, so it carries no relayed chunks.
    async fn answer(&self, trace: &Trace, response: Response, outcome: RelayOutcome) -> Response {
        trace
            .complete(
                &outcome,
                response.status().as_u16(),
                None,
                ResponseFacts::default(),
                None,
                None,
                None,
                None,
                None,
                None,
                0,
                None,
                (self.clock)(),
            )
            .await;
        with_outcome(response, outcome)
    }

    /// The relay's own 502 for an upstream response it could not read after the head: `upstream_stalled` when the
    /// upstream went silent past the idle bound, filed `stream_stalled`, `upstream_undecodable` for a coding the
    /// relay cannot read or a payload that does not decode, filed `stream_undecodable`, and `upstream_cut` for a cut
    /// connection or a body past its bound, filed `stream_cut`.
    async fn unreadable(
        &self,
        trace: &Trace,
        error: &TransportError,
        outcome: RelayOutcome,
    ) -> Response {
        let cut = Cut::of(error);
        self.answer(
            trace,
            cut.failure().response(&outcome.request_id),
            unreachable_outcome(outcome, cut.code()),
        )
        .await
    }

    /// Sleeps until the relay's clock reaches `until`, or `deadline` when that comes first, or until `head` reads the
    /// answer of the request it belongs to. The clock is read again after every `HOLD_TICK` rather than slept against
    /// once, so a clock a test steps is noticed within a tick and production time passes as it does. `None` waits to
    /// the bound: the receiver is one more wake source, never a precondition.
    async fn wait_out(
        &self,
        until: i64,
        deadline: Option<i64>,
        cancel: &CancellationToken,
        mut head: Option<&mut watch::Receiver<bool>>,
    ) -> Waited {
        if head.as_deref().is_some_and(|head| *head.borrow()) {
            return Waited::Head;
        }
        let end = deadline.map_or(until, |deadline| deadline.min(until));
        loop {
            let now = (self.clock)();
            if now >= end {
                return if deadline.is_some_and(|deadline| deadline < until) {
                    Waited::Deadline
                } else {
                    Waited::Cleared
                };
            }
            let slice = HOLD_TICK.min(Duration::from_millis((end - now) as u64));
            let slept = match head.as_deref_mut() {
                // Any change wakes the waiter: the value moves to `true` at a leg's final answer alone, and a dropped
                // sender means the request ended.
                Some(head) => tokio::select! {
                    slept = until_canceled(tokio::time::sleep(slice), cancel) => slept,
                    _ = head.changed() => return Waited::Head,
                },
                None => until_canceled(tokio::time::sleep(slice), cancel).await,
            };
            if slept.is_none() {
                return Waited::Canceled;
            }
        }
    }

    /// The answer to a response that arrived for an account that is no longer there.
    async fn canceled(
        &self,
        ctx: &Responding<'_>,
        head: &ResponseHead,
        written_at: i64,
    ) -> Response {
        ctx.answers();
        ctx.admitted
            .trace
            .head(
                ctx.attempt,
                head,
                None,
                AttemptOutcome::Canceled,
                None,
                written_at,
                (self.clock)(),
            )
            .await;
        self.canceled_after_head(ctx, head.status).await
    }

    /// The caller's answer once the account left the router after this attempt's head was published: the request
    /// completes `canceled` and the attempt keeps the outcome its head recorded, since a second head for the same
    /// attempt would overwrite that outcome in the ledger.
    async fn canceled_after_head(&self, ctx: &Responding<'_>, status: u16) -> Response {
        self.answer_failure(ctx.admitted.trace, ACCOUNT_CANCELED, ctx.outcome(status))
            .await
    }

    /// A head that answers none of the three header families (the api_key buckets, the fast lane's buckets, the
    /// unified windows) carries no sample and changes nothing.
    async fn report_head_sample(
        &self,
        account: &AccountRow,
        head: &ResponseHead,
        reading: &HeadReading,
        canonical_model: &str,
        observed_at: i64,
        request_id: &str,
    ) {
        let Some(sample) = head_sample(head, reading, account.kind, canonical_model, observed_at)
        else {
            return;
        };
        self.router
            .report(Report::Sample {
                account_id: account.id.clone(),
                sample,
                incarnation: account.incarnation,
                source_request_id: Some(request_id.to_string()),
            })
            .await;
    }

    /// Only an OAuth head with the unified headers is evidence, and everything else changes nothing. A usage-based
    /// seat's overage family is its spend window against the monthly cap, so its head reports the share and the reset
    /// and never overage evidence.
    async fn report_overage(
        &self,
        account: &AccountRow,
        unified: Option<&UnifiedHeaders>,
        observed_at: i64,
    ) {
        if account.kind != AccountKind::Oauth {
            return;
        }
        let Some(unified) = unified else {
            return;
        };
        if self.router.snapshot().usage_based(&account.id) {
            let utilization = unified.overage_utilization;
            if utilization.is_some() || unified.overage_reset.is_some() {
                self.router
                    .report(Report::SpendHead {
                        account_id: account.id.clone(),
                        utilization,
                        reset: unified.overage_reset,
                        observed_at,
                        incarnation: account.incarnation,
                    })
                    .await;
            }
            return;
        }
        let in_use = match unified.overage_in_use {
            Some(flag) => flag,
            None if unified.present() => false,
            None => return,
        };
        // The provider's overage reset when it sends one, else the latest reset among the shared windows that
        // read full.
        let reset = unified.overage_reset.or_else(|| {
            latest_full_reset([
                (unified.five_hour.utilization, unified.five_hour.reset),
                (unified.seven_day.utilization, unified.seven_day.reset),
            ])
        });
        self.router
            .report(Report::Overage {
                account_id: account.id.clone(),
                evidence: OverageEvidence {
                    in_use,
                    since: in_use.then_some(observed_at),
                    reset,
                    observed_at,
                    source: EvidenceSource::Head,
                    incarnation: account.incarnation,
                },
                incarnation: account.incarnation,
            })
            .await;
    }

    async fn stream(&self, forward: Forward<'_>) -> Response {
        let Forward {
            affinity,
            cancel,
            checkin,
            mut guards,
            head,
            held,
            incarnation,
            keep_alive,
            on_error,
            on_message_id,
            outcome,
            prefix,
            rate_limit_event,
            reader,
            renewal,
            trace,
        } = forward;
        let (builder, coding) = match forward_head(head, rate_limit_event) {
            Ok(parts) => parts,
            Err(error) => {
                tracing::warn!(%error, "the upstream response cannot be forwarded");
                guards.ended(Some(TRANSPORT_ERROR));
                return self.unreadable(trace, &error, outcome).await;
            }
        };
        let decoder = (!matches!(coding, Encoding::Identity)).then(|| decoder(coding));
        let event_stream = is_event_stream(head);
        // A held stream was decoded and parsed up to its commit by the hold, so the bus copy goes on from there
        // with the hold's parser.
        let handover = match held {
            Some(Inspect {
                decoder: hold_decoder,
                parser,
                read: Some(read),
            }) => {
                let text = match hold_decoder {
                    Some(_) => read.text.freeze(),
                    None => prefix.clone(),
                };
                Some((
                    parser,
                    HeldText {
                        text,
                        events: read.events,
                    },
                ))
            }
            _ => None,
        };
        // The body completes on its own path, a client that leaves mid-body included, so the handler's guard stands
        // down here, before the body takes its copy of the trace.
        if let Some(mut guard) = trace.take_completion() {
            guard.disarm();
        }
        // Without a bus every emit and the completion are no-ops; a hook with no bus still needs the parse run
        // for it, a committed 200 needs it too, since the served-model check reads the answer, and so does a
        // renewal, which carries the usage the parse assembles.
        let publish = (trace.bus_attached()
            || on_message_id.is_some()
            || head.status == 200
            || renewal.is_some()
            || affinity.is_some())
        .then(|| {
            // A copy that stopped following the stream, and a body no hold read, are read afresh.
            let (parser, held) = match handover {
                Some((parser, held)) => (
                    event_stream.then_some(parser.into_skipping(INSPECT_BUFFER)),
                    Some(held),
                ),
                None => (
                    event_stream.then(|| SkippingParser::new(INSPECT_BUFFER)),
                    None,
                ),
            };
            Publish {
                trace: trace.detached(),
                clock: self.clock.clone(),
                outcome: outcome.clone(),
                incarnation,
                catalog: self.catalog.clone(),
                credits: self.credits.clone(),
                read: match parser {
                    Some(parser) => BodyRead::Events(parser),
                    // A committed JSON answer is inspected for its usage block, and a rejection for its error type.
                    None if head.status == 200 => BodyRead::Json {
                        body: Vec::new(),
                        bound: self.limits.max_request_bytes,
                    },
                    None => BodyRead::Rejection(Vec::new()),
                },
                held,
                on_error,
                usage: Usage::default(),
                response: ResponseFacts::default(),
                index: 0,
                bytes: 0,
                first_content_at: None,
                saw_tool_use: false,
                stream_error: None,
                saw_stop: false,
                status: head.status,
                message_id: None,
                on_message_id,
                renewal,
                affinity,
            }
        });
        let body = relay_body(
            Relayed {
                cancel,
                checkin,
                decoder,
                guards,
                idle_timeout: self.limits.idle_timeout,
                keep_alive,
                prefix,
                publish,
                reader,
            },
            &self.bodies,
        );
        with_outcome(built(builder, body), outcome)
    }

    /// `decoded` is the drained body as `respond` decoded it. `renewal` is sent once the answer's usage is read,
    /// beside the completion. `guards` are the request's counts, which a report the full mailbox makes wait holds on
    /// its task after the answer.
    #[allow(clippy::too_many_arguments)]
    async fn forward_buffered(
        &self,
        trace: &Trace,
        head: &ResponseHead,
        decoded: Result<Bytes, TransportError>,
        outcome: RelayOutcome,
        renewal: Option<Renewal>,
        affinity: Option<AffinityFacts>,
        mut guards: Guards,
    ) -> Response {
        // A buffered answer is never an event stream, so its status alone says whether it is a rate limit.
        let builder = match forward_head(head, false) {
            Ok((builder, _)) => builder,
            Err(error) => {
                tracing::warn!(%error, "the upstream response cannot be forwarded");
                guards.ended(Some(TRANSPORT_ERROR));
                return self.unreadable(trace, &error, outcome).await;
            }
        };
        let payload = match decoded {
            Ok(payload) => payload,
            Err(error) => {
                tracing::warn!(%error, "the upstream response cannot be decoded");
                guards.ended(Some(TRANSPORT_ERROR));
                return self.unreadable(trace, &error, outcome).await;
            }
        };
        // The chunk is the decoded body the caller receives, since a consumer of a chunk was promised text.
        trace.publish_chunk(ResponseChunk {
            request_id: trace.request_id.clone(),
            attempt: outcome.attempt,
            index: 0,
            body: payload.clone(),
            phase: None,
        });
        // Only the error statuses `respond` drains (400, 401, 403, 404, 429) reach here, a 200 being read by the body
        // task, and the 200 arm guards a caller that would buffer one, whose usage still counts.
        let answer = (head.status == 200)
            .then(|| answer_from_json(&payload, &[], outcome.diagnostics_named));
        let (usage, facts) = match answer {
            Some(answer) => (Some(answer.usage), answer.response),
            None => (None, ResponseFacts::default()),
        };
        let mut waiting = Waiting::default();
        if let Some(renewal) = renewal {
            renewal.send(
                (self.clock)(),
                head.status,
                usage.as_ref(),
                &facts,
                &mut waiting,
            );
        }
        let usage_sent = match affinity {
            // A drained error names no answer.
            Some(affinity) => affinity.send(
                (self.clock)(),
                head.status,
                usage.as_ref(),
                &facts,
                None,
                &mut waiting,
            ),
            None => false,
        };
        // A committed usage releases the request's booking itself, and a buffered answer without one is a rejection
        // the relay forwards.
        guards.ended((!usage_sent).then_some(REFUSED));
        let response = built(builder, Body::from(payload.clone()));
        // Not built through `self.answer`: that helper carries no relayed chunks for every other caller, but
        // this path published one above, and a bus that drops it must be detectable rather than uncounted.
        trace
            .complete(
                &outcome,
                response.status().as_u16(),
                usage,
                facts,
                None,
                Some(payload.len() as u64),
                Some(1),
                None,
                upstream_code(head.status, Some(&payload)),
                // An error document carries no message `id`.
                None,
                0,
                None,
                (self.clock)(),
            )
            .await;
        self.after_answer(waiting, guards);
        with_outcome(response, outcome)
    }
}

/// The outcome of a request the relay answered with its own 502 after an upstream head it could not use: `code`
/// names how the response failed, not the head that never reached the caller.
fn unreachable_outcome(mut outcome: RelayOutcome, code: &str) -> RelayOutcome {
    outcome.answered(code, Outcome::Failed);
    outcome
}

/// A response whose head is settled and whose body is still on the wire.
struct Forward<'a> {
    /// The conversation report a committed answer sends, beside the renewal; `None` for a rejection.
    affinity: Option<AffinityFacts>,
    cancel: CancellationToken,
    checkin: Checkin,
    /// The response's in-flight counts, which ride the body to its end.
    guards: Guards,
    head: &'a ResponseHead,
    /// The pre-commit hold's read of a held stream, whose parser the bus copy goes on with.
    held: Option<Inspect>,
    /// The serving account's incarnation, which a refusal's fallback credit token is recorded under.
    incarnation: u64,
    keep_alive: bool,
    /// Set for a committed stream, which is watched for a rate-limit error arriving after the commit.
    on_error: Option<OnError>,
    on_message_id: Option<OnMessageId>,
    outcome: RelayOutcome,
    prefix: Bytes,
    /// Set where a held 200's first event is a `rate_limit_error`: that answer is the upstream's rate limit as a
    /// 429 is, so it keeps the rate-limit families.
    rate_limit_event: bool,
    reader: BodyReader<Conn>,
    /// A pinned request's report, sent when the body ends: a committed answer's renewal, or the release of a
    /// pin its own rejection ends.
    renewal: Option<Renewal>,
    trace: &'a Trace,
}

/// Why a selected account produced nothing to dispatch with. No answer reaches the caller: the account is
/// already in `attempted`, so each sends the loop back to select without a wire submission.
enum NoCredential {
    /// The acquisition's refresh landed the account's `reauth_required` mark, which is already reported.
    Marked,
    /// The account row moved between the selection and the acquisition.
    Stale,
    /// The manager or the store has no credential to hand out.
    Unavailable,
}

/// What one attempt's response half reads about the request it belongs to.
struct Responding<'a> {
    account: &'a AccountRow,
    /// What admission read about the request, which every pass of every leg shares.
    admitted: &'a Admitted<'a>,
    attempt: u8,
    /// The body as this pass sends it upstream, after the TTL rewrite, whose markers a committed answer's affinity
    /// report reads.
    body: &'a [u8],
    /// Whether this pass marked the account `reauth_required`, carried back to the attempt loop through
    /// `Next::FailOver` and read by a probe's own answer.
    auth_marked: bool,
    /// The TTL this pass's body was coupled to, for the outcome the response half builds; `None` off the policy.
    cache_ttl_ms: Option<i64>,
    /// Whether this leg has not retried in place yet: one retry per leg.
    can_retry_in_place: bool,
    /// Whether this leg has not stripped yet. A strip retry is the router correcting its own edit on the same
    /// account, never a failover; the one-round rule is the only gate.
    can_strip: bool,
    cancel: &'a CancellationToken,
    credential_generation: u64,
    /// The source of the member on this pass's body, read for the completion event, the session memory's hook and
    /// the affinity report's id: an answer is remembered only for a request that carried or received the member.
    diagnostics: Option<DiagnosticsSource>,
    /// `RelayOutcome::diagnostics_named` for this pass's body.
    diagnostics_named: bool,
    /// The request's end report, which a committed stream takes into its body with the permit and the ticket.
    end_guard: &'a mut Option<EndGuard>,
    /// Whether this selection created its pin rather than renewing one that already served.
    fresh_pin: bool,
    /// The funding ordinal the selection's claim answered, `None` off a `funding_required` account. Every pass of the
    /// leg reports a retry's success under it, since each one went out under the mark the claim found.
    funding_ordinal: Option<u64>,
    /// The leg's head report, which the pass that learns the leg's answer sends.
    head: &'a mut Option<HeadGuard>,
    /// The attempt's own additions, body and head, that a 400 might name.
    injected: &'a [Injected],
    /// The request's permit, which a committed stream takes into its body.
    key_permit: &'a mut Option<KeyPermit>,
    memory_key: Option<&'a MemoryKey>,
    pin_epoch: Option<u64>,
    replayed: bool,
    /// The selection that chose this leg's account, whose pin, judged value and shared windows the response half
    /// reads.
    selection: &'a Selection,
    /// The decision that selected this leg's account, its `seq` among the request's selections. Every pass of the
    /// leg answers under it.
    seq: u32,
    /// The source of the thinking-binding beta on this pass's head, read for the completion event.
    thinking_diagnostics: Option<DiagnosticsSource>,
    /// This pass's account ticket, which a committed stream takes into its body.
    ticket: Option<AccountTicket>,
}

impl Responding<'_> {
    /// Called where this pass's head becomes the client's answer, never for one the leg strips, replays, retries in
    /// place or fails over, so a client that leaves from here on completes on the charge answer the relay's own
    /// completion of that answer carries.
    fn answers(&self) {
        self.admitted.trace.on_head(self.charge());
    }

    fn charge(&self) -> Option<JudgedCharge> {
        self.selection.judged.as_ref().map(|judged| judged.charge)
    }

    /// This pass's outcome under `status`, with the leg's facts stamped on it. The upstream compared this leg's body
    /// even where the account left after the head, so a canceled completion names the member's source as the head's
    /// outcome does, and the beta's beside it.
    fn outcome(&self, status: u16) -> RelayOutcome {
        let mut outcome = self.admitted.outcome(
            &self.account.id,
            status,
            self.attempt,
            true,
            self.pin_epoch,
            self.cache_ttl_ms,
        );
        outcome.diagnostics = self.diagnostics;
        outcome.diagnostics_named = self.diagnostics_named;
        outcome.thinking_diagnostics = self.thinking_diagnostics;
        // The drop guard takes the word only at `Responding::answers`, since the leg may still retry or fail over past
        // this head.
        outcome.judged = self.charge();
        outcome
    }

    /// Both counts and the end report for a body that outlives the response half, so they are released when it
    /// ends.
    fn guards(&mut self) -> Guards {
        Guards {
            _key: self.key_permit.take(),
            _account: self.ticket.take(),
            end: self.end_guard.take(),
        }
    }

    /// The cause the end report carries for an answer this pass ends the request with.
    fn ending(&mut self, cause: &'static str) {
        ending(self.end_guard, cause);
    }
}

/// The words `Report::Ended` names for how a request ended without a committed usage.
pub(crate) const CANCELED: &str = "canceled";
pub(crate) const EXHAUSTED: &str = "exhausted";
pub(crate) const REFUSED: &str = "refused";
pub(crate) const TRANSPORT_ERROR: &str = "transport_error";

/// The request's end report, armed before its first selection is enqueued and held across its wire attempts: a
/// failover's selection replaces the booking under the request's id, a retry in place or a refresh pass keeps it,
/// and the guard reports once, when the request ends. A committed usage disarms it, since the usage releases the
/// booking itself. Every other end sends `Report::Ended` from the drop, with the cause the end set and `canceled`
/// for an end that set none: a client gone before any verdict.
pub(crate) struct EndGuard {
    router: RouterHandle,
    /// Where the report runs when the mailbox is full, so a shutdown waits for it.
    bodies: TaskTracker,
    clock: Clock,
    request_id: String,
    attempt: u32,
    cause: &'static str,
    armed: bool,
}

impl EndGuard {
    fn armed(
        router: RouterHandle,
        bodies: TaskTracker,
        clock: Clock,
        request_id: String,
    ) -> EndGuard {
        EndGuard {
            router,
            bodies,
            clock,
            request_id,
            attempt: 1,
            cause: CANCELED,
            armed: true,
        }
    }

    /// A pass that dispatches again: its attempt, with the cause back at the default, since the pass has not ended.
    fn rearm(&mut self, attempt: u8) {
        self.attempt = u32::from(attempt);
        self.cause = CANCELED;
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for EndGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let report = Report::Ended {
            request_id: self.request_id.clone(),
            attempt: self.attempt,
            at: (self.clock)(),
            cause: self.cause,
        };
        // A control fact takes a slot behind whatever waits in the mailbox, the request's own selection included, so
        // a booking lands before the release that follows it.
        if let Err(report) = self.router.try_report(report) {
            let router = self.router.clone();
            self.bodies.spawn(async move {
                router.report(*report).await;
            });
        }
    }
}

/// The relay's estimate of the body's prompt tokens: its bytes over four, the byte estimate the static affinity
/// entries carry, so a fresh session's rewrite is priced the way its cache entries will be read.
fn body_tokens(raw: &[u8]) -> u64 {
    (raw.len() / 4) as u64
}

/// Sets the cause the end report will carry, on a request that arms one.
fn ending(guard: &mut Option<EndGuard>, cause: &'static str) {
    if let Some(guard) = guard {
        guard.cause = cause;
    }
}

/// Every live inference request and probe by request id, `false` until a leg's answer is final and `true` from then
/// (a `message_start` commit, or the pass that answers the client) or from the leg's guard's drop, while a leg that
/// fails over leaves it `false` for the next leg, so a waiter held for a later attempt's mark waits for that leg's
/// head. The admission registers the entry before the first selection is enqueued, so a request a mark names always has
/// one while it lives, and the `HeadEntry` the request's scope owns removes it at every end of the request. A waiter
/// that finds no entry or `true` goes out at once and creates nothing. Nothing here outlives its request.
type Heads = Mutex<HashMap<String, watch::Sender<bool>>>;

// A poisoned map means a prior panic happened mid-update, so an entry may not describe its request.
const HEADS_POISONED: &str = "relay head map mutex poisoned by a prior panic";

/// A waiter's view of the entry under `request_id`, `None` where that request has ended. Creates nothing.
fn head_of(heads: &Heads, request_id: &str) -> Option<watch::Receiver<bool>> {
    heads
        .lock()
        .expect(HEADS_POISONED)
        .get(request_id)
        .map(watch::Sender::subscribe)
}

/// One request's entry in `Heads`, registered at its admission and removed when the request's scope ends, the
/// handler's return and its drop alike, which drops the sender and wakes every waiter still on it.
struct HeadEntry {
    heads: Arc<Heads>,
    request_id: String,
}

impl HeadEntry {
    fn registered(heads: Arc<Heads>, request_id: &str) -> HeadEntry {
        heads
            .lock()
            .expect(HEADS_POISONED)
            .entry(request_id.to_string())
            .or_insert_with(|| watch::channel(false).0);
        HeadEntry {
            heads,
            request_id: request_id.to_string(),
        }
    }
}

impl Drop for HeadEntry {
    fn drop(&mut self) {
        // A drop during a panic's unwind must not panic again, so a poisoned map is cleaned all the same.
        self.heads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.request_id);
    }
}

/// One per leg of a turn or a probe, shared by every pass of the leg. Reports `Head` once, at the leg's final answer: a
/// `message_start` commit, or the pass whose answer goes to the client (the hold's other ends, a JSON answer, a head
/// outside 200 to 299), setting the request's `Heads` entry `true` first. A head the leg retries in place, replays or
/// strips answers nothing, since the next pass answers for the leg. Its `Drop` reports where the leg ended before
/// either (a pre-head transport error, a cancel, any early return, a failover), so the routing mark clears on every end
/// of the leg. A leg that hands the request to another leg leaves the entry `false`, so a waiter held for the next
/// landing's mark waits for that leg's head.
struct HeadGuard {
    bodies: TaskTracker,
    clock: Clock,
    heads: Arc<Heads>,
    router: RouterHandle,
    request_id: String,
    attempt: u32,
    sent: bool,
    goes_on: bool,
}

impl HeadGuard {
    /// The leg's answer is final at `at`, and no later head comes on this leg. The waiters wake first and the routing
    /// clears the mark second, so a woken request that selects again finds the mark cleared or carries its hold past a
    /// standing one.
    fn arrived(&mut self, at: i64) {
        if self.sent {
            return;
        }
        self.sent = true;
        self.answered();
        self.report(at);
    }

    /// The leg hands the request to another leg: the entry reads `false` until that leg's answer is final, and the
    /// drop reports this leg's end without reading `true`.
    fn goes_on(&mut self) {
        self.goes_on = true;
        if let Some(sender) = self
            .heads
            .lock()
            .expect(HEADS_POISONED)
            .get(&self.request_id)
        {
            sender.send_if_modified(|answered| std::mem::replace(answered, false));
        }
    }

    fn answered(&self) {
        if let Some(sender) = self
            .heads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&self.request_id)
        {
            sender.send_replace(true);
        }
    }

    fn report(&self, at: i64) {
        let report = Report::Head {
            request_id: self.request_id.clone(),
            attempt: self.attempt,
            at,
        };
        // A control fact takes a slot of its own, as the end report does.
        if let Err(report) = self.router.try_report(report) {
            let router = self.router.clone();
            self.bodies.spawn(async move {
                router.report(*report).await;
            });
        }
    }
}

impl Drop for HeadGuard {
    fn drop(&mut self) {
        if self.sent {
            return;
        }
        if !self.goes_on {
            self.answered();
        }
        self.report((self.clock)());
    }
}

/// The in-flight guards a committed stream carries to its end; dropped with the body, whichever way it ends.
#[derive(Default)]
pub(crate) struct Guards {
    _key: Option<KeyPermit>,
    _account: Option<AccountTicket>,
    /// The request's end report, sent on drop unless the body's usage disarmed it.
    end: Option<EndGuard>,
}

impl Guards {
    /// How the body ended: a committed usage (`None`) disarms the end report, since the usage releases the booking
    /// itself, and any other end names the cause the report carries.
    pub(crate) fn ended(&mut self, cause: Option<&'static str>) {
        if let Some(guard) = self.end.as_mut() {
            match cause {
                None => guard.disarm(),
                Some(cause) => guard.cause = cause,
            }
        }
    }
}

/// What the response half decided the account loop should do next.
enum Next {
    /// This account is finished; another one takes the request. `auth_marked` is whether this leg marked the
    /// account `reauth_required`.
    FailOver {
        auth_marked: bool,
    },
    /// The refreshed token goes back to the same account.
    Replay(Credential),
    Respond(Response),
    /// A short throttle: wait this long and try again on the same account with the same credential.
    RetryInPlace(Duration),
    /// A 400 named these items; strip them and try again on the same account.
    Strip(Vec<Injected>),
}

/// What a local refusal answers and files: the failure the caller is told, the client-drift trigger the request row
/// carries and the attention reader raises its condition from, and the contract's item the request row names, each
/// `None` for every other refusal.
struct LocalRefusal {
    failure: Failure,
    client_drift: Option<DriftKind>,
    client_contract: Option<&'static str>,
}

impl From<Failure> for LocalRefusal {
    fn from(failure: Failure) -> Self {
        LocalRefusal {
            failure,
            client_drift: None,
            client_contract: None,
        }
    }
}

/// The restoration facts computed once at admission: the client version the wire already named, the
/// version-suffix code units, and the prompt id the client's header names. The previous request id is not here
/// because it is per-account, looked up in the failover loop against the account each attempt actually tries.
struct RestoreContext {
    cli_version: String,
    prompt_units: Vec<[u16; 3]>,
    prompt_id: Option<String>,
    /// `cc_prev_req` is filled on this request's class (the conversation classes and the conversation-shaped
    /// forks), read once at admission.
    prev_req: bool,
}

/// What the same-account leg reads about the request it is part of: fixed at admission and shared by every
/// account the request tries.
struct Admitted<'a> {
    /// The key's override else the pool's, read once at admission.
    auto_mode_server: bool,
    /// The request's class is auxiliary (a class header outside the refreshing classes).
    auxiliary: bool,
    /// The pool's own policy, off by default; read once at admission since a migrated attempt still prices the
    /// same request against the same policy, only the account's overage state changes per destination.
    cache_ttl_policy: CacheTtlPolicy,
    caller: &'a HeaderMap,
    /// The two boundary chains the client's body hashed, for the affinity report; `None` for a body with no
    /// chain.
    chains: Option<&'a Chains>,
    /// The request is the client's own auto-mode classifier call (`ClientFacts.classifier`), read once at admission
    /// and shared by every leg: its selection carries the flag, and its usage reports carry the selection's judged
    /// value, whose charge reads `On` where the leg's account is the judged turn's.
    classifier: bool,
    /// The client's body carries its own top-level `diagnostics` member, which the edit never rewrites.
    client_diagnostics: bool,
    /// The client sent its body gzip, which `upstream_gzip: auto` mirrors toward the upstream.
    client_gzip: bool,
    /// The client stamped a hash the drift check verified, which every leg re-signs (`NativeEdit::client_hash`).
    client_hash: bool,
    client_version: Option<String>,
    /// The relay's clock past which the attempt loop answers the exhaustion rather than selecting again, and at
    /// which a wait on an account's backoff ends: admission plus `head_timeout` less `ATTEMPT_DEADLINE_MARGIN`.
    /// `None` when `head_timeout` is no longer than the margin.
    deadline: Option<i64>,
    /// Whether the relay inserts the `diagnostics` member and its beta, resolved at admission: the pool's
    /// `cache_diagnostics` switch is on, the request is not a `count_tokens` request, and the client's body carries
    /// a cache marker of its own and a tools list of one or more. A count never gets either, since the public
    /// schema refuses an unknown top-level member and the beta names nothing there, and a 400 on that path must
    /// never latch the account for its inference requests.
    diagnostics: bool,
    fallback_ms: i64,
    /// The request carried `speed: "fast"`: fast eligibility applies to its selections and its throttles cool the
    /// fast lane.
    fast: bool,
    /// Whether this request already waited out the token endpoint's floor on some account. Set through the shared
    /// reference, so no leg needs a parameter for it; atomic because `Admitted` is borrowed across the handler's
    /// awaits and that future has to be `Send`.
    floor_held: AtomicBool,
    identity: &'a RequestIdentity,
    key: &'a ApiKeyRow,
    /// The cache lane the request writes to, composed once at admission since no failover changes the body.
    lane: String,
    /// The selection's `marker` fact, which the request's row keeps as well.
    marker: bool,
    /// What every build does with a body that carries no marker. The cache TTL policy's `inject_when_absent` choice
    /// leaves an auxiliary body as sent, since a one-off helper's write is a write nothing reads, and the `marker`
    /// fact reads this choice, so the cache gate never counts on a member the body will not get.
    markerless: Markerless,
    /// The largest TTL of the client's own cache markers, the renewal's TTL when no rewrite applied.
    markers_ttl_ms: Option<i64>,
    /// The session memory key this request reads under, absent when the request carries no session; a probe
    /// reads under its session's key and the response half records nothing for it.
    memory_key: Option<MemoryKey>,
    /// Whether a failover report hands this request's pin on: a portable request's, never a probe's, which never moves
    /// the pin it warms. A probe's next selection reads the cooldown or the mark on the pinned member and answers it.
    moves_pins: bool,
    /// The caller's head as it arrived, absent for an in-process caller and for a connection the recorder
    /// gave up on.
    original: Option<&'a OriginalHead>,
    /// The pool the key was in at admission, whose policy this request was admitted under; `None` for a key whose
    /// pool the snapshot did not hold.
    pool_id: Option<String>,
    portable: bool,
    raw: &'a Bytes,
    /// The first-party fields to restore, absent for anything but a `claude-cli/` inference request.
    restore: Option<RestoreContext>,
    /// The pool's `retry_in_place_seconds`: the longest throttle waited out on the same account, and the
    /// longest a held selection waits for a slot.
    retry_in_place: Duration,
    /// The request's class is `subagent`, read once at admission beside `turn` and carried to every leg's renewal: an
    /// answer under it that stops with `end_turn` ends the agent.
    subagent: bool,
    target: &'a str,
    /// Whether a leg asks for the thinking-binding beta, resolved at admission: the pool's `thinking_diagnostics`
    /// switch is on, the request is not a `count_tokens` request, and the body's `thinking.type` is a string other
    /// than `disabled`.
    thinking_diagnostics: bool,
    trace: &'a Trace,
    /// The request's class is a turn's, `main` or `subagent`, read once at admission and carried to every leg's
    /// renewal: a client stream's end under it is the turn's end the router notes for the classifier call that follows.
    turn: bool,
    /// The bound a reset wait and the slow-down and rewrite holds sleep under: `deadline` where the head timeout leaves
    /// one, else the head timeout itself, so a short head timeout bounds the waits that would otherwise outlast it.
    wait_bound: i64,
}

impl Admitted<'_> {
    /// The outcome of this request on `account_id`, naming the trace, key, identity and client version it was
    /// admitted with.
    fn outcome(
        &self,
        account_id: &str,
        upstream_status: u16,
        attempt: u8,
        written: bool,
        pin_epoch: Option<u64>,
        cache_ttl_ms: Option<i64>,
    ) -> RelayOutcome {
        relay_outcome(
            self.trace,
            Some(self.key),
            Some(self.identity),
            account_id,
            upstream_status,
            attempt,
            written,
            self.client_version.clone(),
            pin_epoch,
            cache_ttl_ms,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn relay_outcome(
    trace: &Trace,
    key: Option<&ApiKeyRow>,
    identity: Option<&RequestIdentity>,
    account_id: &str,
    upstream_status: u16,
    attempt: u8,
    written: bool,
    client_version: Option<String>,
    pin_epoch: Option<u64>,
    cache_ttl_ms: Option<i64>,
) -> RelayOutcome {
    RelayOutcome {
        request_id: trace.request_id.clone(),
        key_id: key.map(|k| k.id.clone()),
        pool_id: key.map(|k| k.pool_id.clone()),
        // Matches `RequestObserved.session_id`: the client's own session.
        session_id: identity.and_then(|i| i.session_id.clone()),
        canonical_model: identity.map(|i| i.canonical_model.clone()),
        account_id: account_id.to_string(),
        upstream_status,
        attempt,
        written,
        client_drift: None,
        client_contract: None,
        client_version,
        error_code: None,
        class: None,
        pin_epoch,
        received_at: trace.received_at,
        head_at: None,
        upstream_request_id: None,
        cache_ttl_ms,
        overage_in_use: None,
        account_kind: None,
        usage_based: false,
        diagnostics: None,
        diagnostics_named: false,
        thinking_diagnostics: None,
        obstacle: None,
        held: trace.held(),
        judged: None,
    }
}

/// What one selected account produced: an answer for the caller; nothing, in which case the loop selects the
/// next account; or the end of the wire-submission index, which the loop answers as the pool's untimed
/// exhaustion. The account is already in `attempted` in every case.
enum Leg {
    AtCap,
    /// `auth_marked` is whether the leg marked its account `reauth_required`.
    FailOver {
        auth_marked: bool,
    },
    Respond(Response),
}

/// The state a verdict marks its account with, who moved it as its mark site names it, and the upstream error's
/// message where the verdict read one.
pub(crate) type Mark = (AccountState, StateCause, Option<String>);

/// What a failover verdict tells the router, in order: the cooldown, or without one the end of the pin, then the
/// mark. `generations` is the attempt's (incarnation, lifecycle, credential) triple, which fences the mark: a verdict
/// about a credential that was replaced, or a holder the id lost, while the request was in flight lands nowhere.
/// Between legs no client byte has gone out yet, so the response half awaits each report where it stands.
#[allow(clippy::too_many_arguments)]
pub(crate) fn failover_reports(
    account_id: &str,
    canonical_model: &str,
    cooldown: Option<Cooldown>,
    mark: Option<Mark>,
    pin: Option<PinRef>,
    request_id: &str,
    generations: (u64, u64, u64),
    set_at: i64,
) -> Vec<Report> {
    let mut reports = Vec::new();
    if let Some(cooldown) = cooldown {
        let row = CooldownRow {
            account_id: account_id.to_string(),
            scope: cooldown_scope(cooldown.scope, canonical_model),
            until: cooldown.until,
            set_at,
            reason: cooldown.reason.to_string(),
            window: cooldown.window,
            source_request_id: Some(request_id.to_string()),
            incarnation: generations.0,
        };
        reports.push(Report::Cooldown {
            account_id: account_id.to_string(),
            row,
            pin,
            incarnation: generations.0,
            lifecycle_generation: generations.1,
            credential_generation: generations.2,
        });
    } else if let Some(pin) = pin {
        // A mark without a cooldown still ends the pin: the account cannot serve until an operator acts.
        reports.push(Report::PinInvalidated {
            account_id: account_id.to_string(),
            pin,
            mark: mark.as_ref().map(|(state, cause, _)| (*state, *cause)),
            incarnation: generations.0,
            lifecycle_generation: generations.1,
            credential_generation: generations.2,
        });
    }
    if let Some((state, cause, detail)) = mark {
        reports.push(Report::AccountState {
            account_id: account_id.to_string(),
            state,
            incarnation: generations.0,
            lifecycle_generation: generations.1,
            credential_generation: generations.2,
            cause,
            detail,
            dispatched_ordinal: None,
        });
    }
    reports
}

/// Decoded in pieces under the same bound as the inspection copy, so a small compressed rejection cannot
/// expand without limit in memory.
fn decode_all(decoder: &mut dyn Decoder, raw: &[u8]) -> Result<Vec<u8>, TransportError> {
    let mut out = Vec::new();
    for piece in raw.chunks(DECODE_PIECE) {
        decoder.feed_into(&mut out, piece, MAX_BUFFERED_BODY)?;
    }
    decoder.finish_into(&mut out, MAX_BUFFERED_BODY)?;
    Ok(out)
}

/// A media type compares case-insensitively, so the hold and the bus copy read a stream however the upstream
/// capitalizes its content type.
fn is_event_stream(head: &ResponseHead) -> bool {
    head.get("content-type")
        .is_some_and(|v| starts_with_ignoring_case(v, "text/event-stream"))
}

/// Every coded body is decoded for the forward and `content-encoding` dropped, whatever the caller accepts, so the
/// relay's parser reads every answer and the client always receives identity. `rate_limit_event` marks a 200 whose
/// first event is a `rate_limit_error`, which keeps the rate-limit families as a 429 does.
fn forward_head(
    head: &ResponseHead,
    rate_limit_event: bool,
) -> Result<(Builder, Encoding), TransportError> {
    let upstream = encoding(head)?;
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(head.status).unwrap_or(StatusCode::BAD_GATEWAY));
    // The relay's own read of the families (`ratelimit.rs`) already ran on the upstream head; hiding them
    // from the caller changes nothing it saw.
    let hide_rate_limits = head.status != 429 && !rate_limit_event;
    for (name, value) in &head.headers {
        if NOT_FORWARDED.iter().any(|n| name.eq_ignore_ascii_case(n)) {
            continue;
        }
        if hide_rate_limits
            && RATE_LIMIT_FAMILY_PREFIXES
                .iter()
                .any(|p| starts_with_ignoring_case(name, p))
        {
            continue;
        }
        if name.eq_ignore_ascii_case("content-encoding") {
            continue;
        }
        let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) else {
            continue;
        };
        builder = builder.header(name, value);
    }
    Ok((builder, upstream))
}

fn built(builder: Builder, body: Body) -> Response {
    builder
        .body(body)
        // The status is a validated `StatusCode` and every header was parsed before it was added.
        .expect("a forwarded response head is always well formed")
}

/// The caller's own `User-Agent`, the only handle an operator has on which client drifted.
fn user_agent(headers: &HeaderMap) -> &str {
    crate::identity::user_agent_of(headers).unwrap_or("")
}

/// Bytes fed to a decoder at a time, so the bound below is checked as the copy grows rather than after a
/// small compressed body has already expanded into memory.
const DECODE_PIECE: usize = 4096;

/// The drained body decoded once: the classification table reads it as JSON, and a body drained whole is what
/// `forward_buffered` sends. An error is a payload that does not decode or does not fit. `cut` is a body the drain
/// stopped reading at the bound: its coded stream ends early and its text may run past the bound, so this is what
/// decoded up to there, as far as the bound, for the classification alone, since the forward sends the raw prefix
/// through its own decoder.
fn decode_drained(head: &ResponseHead, raw: &Bytes, cut: bool) -> Result<Bytes, TransportError> {
    Ok(match encoding(head)? {
        Encoding::Identity => raw.clone(),
        coding if cut => Bytes::from(inspectable_prefix(decoder(coding).as_mut(), raw)),
        coding => Bytes::from(decode_all(decoder(coding).as_mut(), raw)?),
    })
}

/// What a cut coded body decodes to, piece by piece until a piece fails or the bound is reached, with no finish:
/// the coded stream's end was never read, so a finish would refuse the whole copy for a trailer it cannot have.
fn inspectable_prefix(decoder: &mut dyn Decoder, raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for piece in raw.chunks(DECODE_PIECE) {
        if decoder
            .feed_into(&mut out, piece, MAX_BUFFERED_BODY)
            .is_err()
        {
            break;
        }
    }
    out
}

/// Local identifier for one relayed request, echoed in every local error body so a caller's failure can be
/// tied to the log line that explains it.
pub fn new_request_id() -> String {
    format!("req_phoebis_{:032x}", rand::random::<u128>())
}

fn with_outcome(mut response: Response, outcome: RelayOutcome) -> Response {
    response.extensions_mut().insert(outcome);
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_usage_based_seat_pays_for_every_cache_write_as_an_api_key_account_does() {
        assert!(
            pays_for_writes(AccountKind::Oauth, true, false),
            "a usage-based seat bills every write in dollars, so the metered set applies"
        );
        assert!(pays_for_writes(AccountKind::ApiKey, false, false));
        assert!(
            pays_for_writes(AccountKind::Oauth, false, true),
            "a plan seat in overage pays for its writes"
        );
        assert!(
            !pays_for_writes(AccountKind::Oauth, false, false),
            "a plan seat on its plan quota keeps the plan set"
        );
    }

    #[test]
    fn a_request_with_no_content_encoding_line_is_identity() {
        assert_eq!(request_coding(&HeaderMap::new()), Some(Encoding::Identity));
    }

    #[test]
    fn two_content_encoding_lines_leave_the_coding_ambiguous_and_are_refused() {
        let mut headers = HeaderMap::new();
        headers.append(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        headers.append(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        assert_eq!(request_coding(&headers), None);
    }

    #[test]
    fn a_content_encoding_value_that_is_not_visible_ascii_is_refused() {
        // A no-break space after `gzip` decodes as UTF-8 and trims away, so only the visible-ASCII rule refuses
        // it.
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_ENCODING,
            HeaderValue::from_bytes(b"gzip\xc2\xa0").unwrap(),
        );
        assert_eq!(request_coding(&headers), None);
    }

    #[test]
    fn a_plain_body_labelled_gzip_is_not_gzip() {
        assert!(matches!(
            inflate_request(br#"{"model":"claude-sonnet-4-5"}"#, 1 << 20),
            Err(RelayError::BodyNotGzip)
        ));
    }

    #[test]
    fn a_truncated_gzip_stream_is_not_gzip() {
        let plain = vec![b'x'; 8 << 10];
        let whole = phoebis_transport::encode::gzip(&plain);
        assert_eq!(inflate_request(&whole, 1 << 20).unwrap(), plain);
        assert!(matches!(
            inflate_request(&whole[..whole.len() - 4], 1 << 20),
            Err(RelayError::BodyNotGzip)
        ));
    }

    /// A bare map holding one live request's entry under `r`, its answer still unknown.
    fn heads_with_r() -> Heads {
        Mutex::new(HashMap::from([(
            "r".to_string(),
            tokio::sync::watch::channel(false).0,
        )]))
    }

    #[test]
    fn a_subscriber_after_the_end_finds_no_entry_and_no_orphan() {
        let heads = heads_with_r();
        heads.lock().unwrap().remove("r");
        assert!(head_of(&heads, "r").is_none(), "the request ended");
        assert_eq!(heads.lock().unwrap().len(), 0, "the lookup creates nothing");
    }
}
