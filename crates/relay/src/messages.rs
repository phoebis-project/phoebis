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
use axum::response::{IntoResponse, Response};
use axum::routing::{get, head, post};
use axum::{Json, Router};
use bytes::Bytes;
use futures::future::BoxFuture;
use phoebis_bus::{
    AccountSelected, AttemptOutcome, AutoMode, Bus, ClientFacts, CooldownSet, Event, Fork,
    Kind, Origin, Outcome, RequestComplete, RequestObserved, RequestShape, ResponseChunk,
    ResponseFacts, ResponseHead as BusHead, Usage,
};
use phoebis_oauth::{AccessToken, AcquireError, Clock, CredentialManager};
use phoebis_routing::{
    cohort_hold, resolve, AccountTicket, ClientClass, CohortHold, EvidenceSource, Exhaustion,
    HoldFacts, KeyPermit, KeyRefusal, OverageEvidence, PinRef, Presented, Refusal,
    Report, ResolveError, RouterHandle, SelectRequest, SelectResult, Selection, SelectionTrace,
    BUDGET_WINDOW,
};
use phoebis_store::{
    AccountKind, AccountRow, AccountState, ApiKeyRow, CooldownRow, MemoryStore,
    Secret, Store,
};
use phoebis_transport::control::Resolver;
use phoebis_transport::decode::{decoder, encoding, request_encoding, Decoder, Encoding};
use phoebis_transport::pool::{Pool, PoolKey};
use phoebis_transport::request::RawRequest;
use phoebis_transport::response::{framing, keep_alive, BodyReader, ResponseHead};
use phoebis_transport::TransportError;
use serde_json::json;
use tokio::io::AsyncRead;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::billing::{restorable_version, Restore};
use crate::body::{tier_ttls, uniform_ttl, VersionCheck, BILLING_LINE_PATH};
use crate::budget::{BodyBudget, BodyReads};
use crate::casing::OriginalHead;
use crate::catalog::CatalogService;
use crate::chains::Chains;
use crate::classify::{
    classify, error_type, refused_names, Cooldown, HeadContext, Scope, Verdict, MARGIN_MS,
};
use crate::dispatch::{self, until_cancelled, AttemptFailure, Sent};
use crate::errors::{
    accounts_at_cap, auth_failure, body_budget_exhausted, key_body_reads, key_refusal,
    relay_failure, Failure, ACCOUNTS_AT_CAP, ACCOUNT_CANCELLED, METHOD_NOT_ALLOWED,
    MODEL_NOT_ALLOWED, MODEL_REJECTED, NOT_FOUND, ROUTER_UNAVAILABLE, UPSTREAM_UNREACHABLE,
};
use crate::exhaustion::exhaustion_response;
use crate::facts::{BodyFacts, ClientHints};
use crate::headers::{outbound_headers, CredentialKind};
use crate::hold::{hold, Commit, Held, Inspect, INSPECT_BUFFER};
use crate::identity::{
    client_class, climb, identify, waits_on, PendingReports, RequestIdentity, PENDING_REPORT_WAIT,
};
use crate::memory::{
    uuid_v4, Injected, MemoryKey, Previous, SessionMemory, MEMORY_CAP, MEMORY_IDLE_MS,
};
use crate::observe::{answer_from_json, published_headers, wants_stream};
use crate::ratelimit::{head_sample, retry_after, upstream_date, UnifiedHeaders};
use crate::sse::SseParser;
use crate::stream::{
    drain_bounded, relay_body, AffinityFacts, Checkin, Drained, OnError, PostCommit, Publish,
    Relayed, Renewal, ReturnToPool, Waiting, MAX_BUFFERED_BODY,
};
use crate::{
    edit_body, is_portable, presented_key, BodyEdits, BodyMember, CacheTtl, CacheTtlPolicy,
    DriftGuard, Markerless, NativeEdit, RelayError, SafeguardsEdit, TtlBranch, UpstreamGzip,
    DANGEROUS_TOOL_USE_BETA, EXTENDED_TTL_BETA,
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
    Cancelled,
    Cleared,
    /// The request's deadline came before the backoff cleared.
    Deadline,
}

/// Upstream response headers the relay keeps to itself: the connection and framing ones describe the upstream
/// hop, and the framing no longer matches the body the caller is sent; the upstream's own identity and cookies
/// name the account behind the key, never the key holder.
const NOT_FORWARDED: &[&str] = &[
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
/// never reads as the key holder's own: the unified family and the Anthropic-key bucket family under the first
/// prefix, the fast-token family under the second. A rate limit keeps them, a 429 or a 200 whose first event is a
/// `rate_limit_error`, so the client's continue arms on the reset the upstream named.
const RATE_LIMIT_FAMILY_PREFIXES: [&str; 2] = ["anthropic-ratelimit-", "anthropic-fast-"];

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// The bytes of request bodies the process holds at once, across every connection.
    pub body_budget_bytes: u64,
    /// The longest a request's bodies may be kept, whatever a key or pool was left holding: an operator who
    /// lowers the global maximum means it from the next request, not from the next write of every stored row.
    pub body_capture_max_hours: u32,
    /// How long a request body may take to arrive after authentication.
    pub body_read_timeout: Duration,
    /// How long an account is cooled when the upstream rejected it without saying for how long.
    pub cooldown_fallback: Duration,
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
    /// `hash` when the caller stamped a billing hash this build cannot reproduce, `version_suffix` when its
    /// `cc_version` suffix does not reproduce from the prompt and the user agent; in either case nothing was
    /// sent upstream.
    pub client_drift: Option<String>,
    pub client_version: Option<String>,
    pub error_code: Option<String>,
    pub pin_epoch: Option<u64>,
    pub received_at: i64,
    /// When the head that answered the caller arrived, which a local answer never has.
    pub head_at: Option<i64>,
    /// The upstream's own `request-id` response header on the committed attempt, `None` for a local answer or an
    /// upstream that sent none.
    pub upstream_request_id: Option<String>,
    /// The TTL the relay rewrote the body to on this attempt, in milliseconds; `None` when the body went as sent.
    pub cache_ttl_ms: Option<i64>,
    /// `budget` when the selector answered a probe pinned to a member at its spend budget, `operator`,
    /// `subscription_ended` or `attempted` when it answered one on its pinned member's standing
    /// (`Exhaustion::Pinned`), `reauth_required` when a leg of the probe marked its account for re-authentication
    /// (`REAUTH_OBSTACLE`): the obstacles the keepalive holds on rather than pauses as an exhausted pool; `None`
    /// otherwise.
    pub obstacle: Option<&'static str>,
    /// The holds this request waited through, the summary `phoebis_routing::cohort_hold` gives; `None` for none.
    pub held: Option<CohortHold>,
}

/// On `RelayOutcome`: the obstacle a probe's exhaustion carries when a leg of the request marked the pinned
/// account `reauth_required`; the keepalive pauses under `ProbeAuthenticationError` on it.
pub const REAUTH_OBSTACLE: &str = "reauth_required";

/// Which side funded the account a request served under, on the response's extensions beside `RelayOutcome`.
/// The keepalive's probe path is the one reader today: a probe's cost and the credit it later earns price
/// through the funding its own serving attempt had, which can only be read off the selection that dispatched it.
#[derive(Clone, Copy, Debug)]
pub struct ServedUnder {
    pub in_overage: bool,
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
    /// task publishes after its connection was cut.
    bodies: TaskTracker,
    body_budget: Arc<BodyBudget>,
    body_reads: BodyReads,
    bus: Option<Bus>,
    catalog: Arc<CatalogService>,
    pub(crate) clock: Clock,
    drift: DriftGuard,
    pub(crate) limits: Limits,
    manager: Arc<CredentialManager>,
    memory: Arc<SessionMemory>,
    /// The sequence of the affinity report each conversation's last answer enqueued, which the next turn's
    /// climb waits on.
    pending_reports: Arc<PendingReports>,
    pub(crate) pool: Arc<Pool>,
    /// The pinned profile's name, for the drift warn the operator reads; empty where nothing names one.
    profile_name: String,
    readiness: Option<ReadinessSource>,
    resolver: Resolver,
    router: RouterHandle,
    /// The Anthropic keys legs dispatch with, by account id, so a leg reads no registry while its row's lineage
    /// and the snapshot's generation hold.
    secrets: Mutex<HashMap<String, SecretEntry>>,
    /// `proxy.sign_cch_placeholder`: whether a restored `cch=00000` is signed rather than sent as the placeholder.
    sign_cch_placeholder: bool,
    store: Arc<Store>,
}

/// A cached Anthropic key and the lineage it was read under: the holder's incarnation and lifecycle generation
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
            drift: DriftGuard,
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

    /// Attaches the bus every relayed request is published to. Without one the relay publishes nothing.
    pub fn with_bus(self, bus: Bus) -> Relay {
        Relay {
            bus: Some(bus),
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
    /// failover loop a real request takes, so the pin follows, renews, and migrates as it would for one.
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
    origin: Origin,
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
        static_hash: Option<String>,
        boundaries: Vec<String>,
        client: ClientFacts,
        auto_mode: Option<AutoMode>,
        presented_retired: bool,
        rotation_at: Option<i64>,
    ) {
        if self.bus.is_none() {
            return;
        }
        self.publish_fact(Event::RequestObserved(RequestObserved {
            request_id: self.request_id.clone(),
            origin: self.origin,
            kind: self.kind,
            key_id: key.map(|k| k.id.clone()),
            // A probe's row files under the generation its recipe was retained under, the same one its
            // keepalive deltas carry, so the two never disagree about whose probe it was.
            key_generation: self
                .probe_generation
                .unwrap_or_else(|| key.map_or(0, |k| k.created_at)),
            pool_id: key.map(|k| k.pool_id.clone()),
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
            static_hash,
            boundaries,
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
            received_at: self.received_at,
        }))
        .await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn selected(
        &self,
        attempt: u8,
        account: &AccountRow,
        pin_epoch: Option<u64>,
        started_at: i64,
        cache_ttl_ms: Option<i64>,
        trace: Option<&SelectionTrace>,
    ) {
        self.publish_fact(Event::AccountSelected(AccountSelected {
            request_id: self.request_id.clone(),
            attempt,
            account_id: account.id.clone(),
            account_kind: match account.kind {
                AccountKind::ApiKey => "api_key",
                AccountKind::Oauth => "oauth",
            }
            .into(),
            pin_epoch,
            started_at,
            cache_ttl_ms,
            selection: trace.and_then(|t| serde_json::to_string(t).ok()),
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
    /// only for a forwarded answer; `end` overrides the outcome derived from the `RelayOutcome` for a body that
    /// ended other than cleanly, and `error` names the cause when the override has one.
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
        message_id: Option<String>,
        finished_at: i64,
    ) {
        let (final_outcome, error_code) = match end {
            Some((o, code)) => (o, code.or_else(|| outcome.error_code.clone())),
            None => (outcome_of(outcome), outcome.error_code.clone()),
        };
        self.publish_fact(Event::RequestComplete(RequestComplete {
            request_id: self.request_id.clone(),
            attempt: outcome.attempt,
            account_id: (!outcome.account_id.is_empty()).then(|| outcome.account_id.clone()),
            outcome: final_outcome,
            status,
            usage: usage.filter(|u| !u.is_empty()),
            response,
            ttfb_ms: outcome.head_at.map(|at| at - outcome.received_at),
            ttfc_ms,
            error_code,
            client_drift: outcome.client_drift.clone(),
            client_version: outcome.client_version.clone(),
            response_bytes,
            response_chunks,
            held: outcome
                .held
                .as_ref()
                .and_then(|h| serde_json::to_string(h).ok()),
            message_id,
            finished_at,
        }))
        .await;
    }
}

/// The class and hint facts `observed` publishes, built once per call: the class from the identity already
/// derived it, or read fresh for a request refused before it had one; the turn origin from the body, once it was
/// read.
fn client_facts(
    headers: &HeaderMap,
    identity: Option<&RequestIdentity>,
    facts: Option<&BodyFacts>,
) -> ClientFacts {
    let hints = ClientHints::read(headers);
    ClientFacts {
        class: identity.map_or_else(|| client_class(headers), |i| i.client.clone()),
        agent_type: hints.agent_type,
        compaction: hints.compaction,
        context_compacted: hints.context_compacted,
        prev_tool_ms: hints.prev_tool_ms,
        request_class: hints.request_class,
        turn_origin: facts.and_then(|f| f.turn_origin.clone()),
    }
}

/// The declared body length, when the caller sent one the relay can read; a chunked body has none.
fn content_length(headers: &HeaderMap) -> Option<usize> {
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
    let mut plain = decoder.feed_bounded(raw, bound).map_err(refused)?;
    plain.extend(
        decoder
            .finish_bounded(bound - plain.len())
            .map_err(refused)?,
    );
    Ok(plain)
}

/// The request outcome a local answer implies; a body that ends badly overrides it at the end of the body.
fn outcome_of(outcome: &RelayOutcome) -> Outcome {
    match outcome.error_code.as_deref() {
        Some("account_cancelled") => Outcome::Canceled,
        // A probe answered on its pinned member's standing (`Exhaustion::Pinned`, the one untimed answer that
        // names an obstacle) is refused for the probe alone, whatever its siblings hold: conclusive, and no
        // exhaustion of the pool for the attention gauge or the rollups to count.
        Some("accounts_unavailable") if outcome.obstacle.is_some() => Outcome::Rejected,
        // A pool whose every account is at its in-flight cap had nothing to place the request on either, and the
        // attention gauge and the rollups count it with the other exhaustions.
        Some("accounts_exhausted") | Some("accounts_unavailable") | Some(ACCOUNTS_AT_CAP) => {
            Outcome::Exhausted
        }
        // A model rejection is a refusal whether it is caught before any attempt (an obstacle at
        // selection) or after one, where a portable rejection fails over and ends at the selector's
        // exhaustion: the account that answered it is conclusive either way, not a transport or upstream
        // failure.
        Some("model_rejected") => Outcome::Rejected,
        // A key or policy refusal is conclusive whether it is caught at admission or at the reselect after an
        // attempt, so the ledger row reads rejected rather than failed.
        Some("auth_unknown") | Some("client_not_allowed") | Some("model_not_allowed") => {
            Outcome::Rejected
        }
        Some(_) if outcome.attempt == 0 => Outcome::Rejected,
        Some(_) => Outcome::Failed,
        None if outcome.upstream_status < 400 => Outcome::Succeeded,
        None => Outcome::Failed,
    }
}

/// The attempt outcome the verdict on a head implies.
fn attempt_outcome(verdict: &Verdict, status: u16, first_event: Option<&[u8]>) -> AttemptOutcome {
    match verdict {
        Verdict::Refresh => AttemptOutcome::AuthFailed,
        Verdict::Commit if status < 400 => AttemptOutcome::Succeeded,
        Verdict::Commit => AttemptOutcome::UpstreamError,
        Verdict::FailOver { .. } if status == 429 => AttemptOutcome::RateLimited,
        Verdict::FailOver { .. }
            if first_event
                .is_some_and(|e| e.windows(18).any(|w| w == br#""rate_limit_error""#)) =>
        {
            AttemptOutcome::RateLimited
        }
        Verdict::FailOver {
            mark: Some(AccountState::ReauthRequired),
            ..
        } => AttemptOutcome::AuthFailed,
        Verdict::FailOver { .. } => AttemptOutcome::UpstreamError,
        Verdict::StripInjected(_) => AttemptOutcome::UpstreamError,
    }
}

/// One leg's edited body, what the body editor restored, the beta items it asks the header builder to add, and
/// the TTL it coupled the body to (the fourth element), `None` off the policy.
type Built = Result<(Vec<u8>, Vec<Injected>, Vec<Injected>, Option<CacheTtl>), RelayError>;

/// Every kind of injection the relay may make. Order carries no meaning: it only seeds a `BTreeSet`.
const INJECTED_KINDS: [Injected; 7] = [
    Injected::CacheDiagnosisBeta,
    Injected::Cch,
    Injected::ClientRequestId,
    Injected::Diagnostics,
    Injected::ExtendedTtlBeta,
    Injected::PrevReq,
    Injected::PromptId,
];

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
        prev_req: previous.request_id.as_deref(),
        restore_cch: true,
        skip: skip.iter().copied().collect(),
    })
}

/// Whether a 400 on this request says anything about the account past this leg. A `count_tokens` request
/// injects nothing at all, so its refusal has nothing to latch; the strip retry still corrects this leg's own
/// edit.
fn latches(trace: &Trace) -> bool {
    trace.kind == Kind::Inference
}

/// The TTL a renewal names when neither a rewrite nor a marker of the client's own says one.
const DEFAULT_CACHE_TTL_MS: i64 = 300_000;

/// The wire value a coupled TTL dispatches as, for the bus and the outcome; `None` when the body went as sent.
fn cache_ttl_ms(ttl: Option<CacheTtl>) -> Option<i64> {
    ttl.map(|t| match t {
        CacheTtl::FiveMinutes => 300_000,
        CacheTtl::OneHour => 3_600_000,
    })
}

fn cooldown_set(cooldown: Option<&Cooldown>, canonical_model: &str) -> Option<CooldownSet> {
    cooldown.map(|c| CooldownSet {
        scope: match c.scope {
            Scope::Account => "account".into(),
            Scope::Fast => "fast".into(),
            Scope::FastCredits => "fast_credits".into(),
            Scope::Model => format!("model:{canonical_model}"),
        },
        until: c.until,
        reason: c.reason.to_string(),
        window: c.window.clone(),
    })
}

/// The relayed surface. A known path asked with an unsupported method answers an Anthropic-shaped 405 rather
/// than the 404 an unknown path gets, so a client can tell a wrong method from a wrong route.
pub fn router(relay: Arc<Relay>) -> Router {
    Router::new()
        .route("/api/hello", head(hello))
        .route("/api/ready", get(ready).head(ready))
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(messages))
        // The listing answers the caller's key with what its pool may route to, so a client that sizes its
        // context window by the id sees the `[1m]` twins.
        .route("/v1/models", get(crate::models::list))
        .route("/v1/models/{*id}", get(crate::models::show))
        .method_not_allowed_fallback(wrong_method)
        .fallback(unknown_route)
        .with_state(relay)
}

/// The native client probes this before it sends anything and ignores the answer.
async fn hello() -> StatusCode {
    StatusCode::OK
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
            kind: CredentialKind::Oauth { bearer: secret },
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
            origin: if probe.is_some() {
                Origin::Keepalive
            } else {
                Origin::Client
            },
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
        };
        let (parts, body) = request.into_parts();
        let original = parts.extensions.get::<OriginalHead>();
        // The snapshot is pinned for the whole request: the key, the pool, and the candidates all come from one
        // registry, and a write that lands mid-request is seen by the next request.
        let snapshot = self.router.snapshot();
        let (key, presented) = match &probe {
            // Before the body, as for a caller: an unauthenticated probe never holds the request limit either.
            Some(probe) => match snapshot.authenticate_id(&probe.key_id, received_at) {
                // A recipe retained under an earlier key of this id is that key's; its probe never runs in
                // the replacement's pool.
                Ok(key) if key.created_at != probe.key_generation => {
                    let refusal = KeyRefusal::Replaced;
                    tracing::debug!(%refusal, key = %probe.key_id, "a probe's key was refused");
                    self.ledger_standing(&trace, false);
                    trace
                        .observed(
                            None,
                            None,
                            &parts.headers,
                            false,
                            Bytes::new(),
                            false,
                            0,
                            RequestShape::default(),
                            None,
                            Vec::new(),
                            client_facts(&parts.headers, None, None),
                            None,
                            false,
                            None,
                        )
                        .await;
                    return self
                        .refused(&trace, 0, key_refusal(&refusal), None, None, None)
                        .await;
                }
                // A probe authenticates by key id, never by secret, so it never presents a retired one.
                Ok(key) => (key.clone(), Presented::Live),
                Err(refusal) => {
                    tracing::debug!(%refusal, key = %probe.key_id, "a probe's key was refused");
                    self.ledger_standing(&trace, false);
                    trace
                        .observed(
                            None,
                            None,
                            &parts.headers,
                            false,
                            Bytes::new(),
                            false,
                            0,
                            RequestShape::default(),
                            None,
                            Vec::new(),
                            client_facts(&parts.headers, None, None),
                            None,
                            false,
                            None,
                        )
                        .await;
                    return self
                        .refused(&trace, 0, key_refusal(&refusal), None, None, None)
                        .await;
                }
            },
            None => {
                let presented = match presented_key(&parts.headers) {
                    Ok(presented) => presented,
                    Err(error) => {
                        self.ledger_standing(&trace, false);
                        trace
                            .observed(
                                None,
                                None,
                                &parts.headers,
                                false,
                                Bytes::new(),
                                false,
                                0,
                                RequestShape::default(),
                                None,
                                Vec::new(),
                                client_facts(&parts.headers, None, None),
                                None,
                                false,
                                None,
                            )
                            .await;
                        return self
                            .refused(&trace, 0, auth_failure(&error), None, None, None)
                            .await;
                    }
                };
                match snapshot.authenticate(presented, received_at) {
                    Ok((key, presented)) => (key.clone(), presented),
                    Err(refusal) => {
                        tracing::debug!(%refusal, "a presented key was refused");
                        self.ledger_standing(&trace, false);
                        trace
                            .observed(
                                None,
                                None,
                                &parts.headers,
                                false,
                                Bytes::new(),
                                false,
                                0,
                                RequestShape::default(),
                                None,
                                Vec::new(),
                                client_facts(&parts.headers, None, None),
                                None,
                                false,
                                None,
                            )
                            .await;
                        return self
                            .refused(&trace, 0, key_refusal(&refusal), None, None, None)
                            .await;
                    }
                }
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
        let client_version = self.drift.check(&parts.headers);
        let retired = presented == Presented::Retired;
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
        let Some(coding) = request_coding(&parts.headers) else {
            return self
                .refused_before_body(
                    &trace,
                    &key,
                    &parts.headers,
                    capture_hours,
                    retired,
                    relay_failure(&RelayError::UnsupportedEncoding),
                    client_version.as_deref(),
                )
                .await;
        };
        let client_gzip = coding == Encoding::Gzip;
        // The bodies arriving on a key and the bytes of them the process holds are bounded before the read, so a
        // flood of slow or large bodies is refused rather than buffered. A probe's body is the router's own and
        // already in memory, so it takes neither and has no deadline to keep.
        let (read_permit, mut reservation) = match trace.kind {
            Kind::Probe => (None, None),
            Kind::CountTokens | Kind::Inference => {
                let Some(read_permit) = self
                    .body_reads
                    .try_acquire(&key.id, self.limits.key_body_reads)
                else {
                    return self
                        .refused_before_body(
                            &trace,
                            &key,
                            &parts.headers,
                            capture_hours,
                            retired,
                            key_body_reads(),
                            client_version.as_deref(),
                        )
                        .await;
                };
                let declared = content_length(&parts.headers);
                if declared.is_some_and(|n| n > self.limits.max_request_bytes) {
                    return self
                        .refused_before_body(
                            &trace,
                            &key,
                            &parts.headers,
                            capture_hours,
                            retired,
                            relay_failure(&RelayError::BodyTooLarge),
                            client_version.as_deref(),
                        )
                        .await;
                }
                // A chunked body may run to the per-body bound, so it holds that much until it has been read.
                let Some(reservation) = self
                    .body_budget
                    .try_reserve(declared.unwrap_or(self.limits.max_request_bytes) as u64)
                else {
                    return self
                        .refused_before_body(
                            &trace,
                            &key,
                            &parts.headers,
                            capture_hours,
                            retired,
                            body_budget_exhausted(),
                            client_version.as_deref(),
                        )
                        .await;
                };
                (Some(read_permit), Some(reservation))
            }
        };
        // A caller that stops sending holds a task and a buffer for nothing; the read has its own deadline, under
        // the stream's idle bound.
        let read = axum::body::to_bytes(body, self.limits.max_request_bytes);
        let read = match trace.kind {
            Kind::Probe => Ok(read.await),
            Kind::CountTokens | Kind::Inference => {
                tokio::time::timeout(self.limits.body_read_timeout, read).await
            }
        };
        drop(read_permit);
        let body_read = match read {
            Ok(Ok(raw)) => Ok(raw),
            Err(_) => Err(RelayError::BodyTimeout),
            Ok(Err(error)) => {
                // axum wraps the limit and the body's own failure in one type; only the limit is the caller's
                // request being too large, the rest is the caller leaving before its body was complete.
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
            Err(error) => {
                return self
                    .refused_before_body(
                        &trace,
                        &key,
                        &parts.headers,
                        capture_hours,
                        retired,
                        relay_failure(&error),
                        client_version.as_deref(),
                    )
                    .await;
            }
        };
        if let Some(reservation) = &mut reservation {
            // The declared length frames the body and the per-body bound caps a chunked one, so the read never
            // holds more than was reserved: this only shrinks, and a shrink never fails.
            let shrunk = reservation.resize(raw.len() as u64);
            debug_assert!(shrunk.is_ok(), "the read outgrew its reservation");
        }
        // Every read below, the edits and the stamp included, sees the plaintext with the client's tail after the
        // JSON in place, as an identity body is seen.
        let raw = if client_gzip {
            let bound = self.limits.max_request_bytes;
            if let Some(reservation) = &mut reservation {
                // The inflated worst case is reserved before a byte is inflated: a budget that cannot grant it
                // refuses here, never after the decoder has already allocated it.
                if reservation.resize(bound as u64).is_err() {
                    return self
                        .refused_before_body(
                            &trace,
                            &key,
                            &parts.headers,
                            capture_hours,
                            retired,
                            body_budget_exhausted(),
                            client_version.as_deref(),
                        )
                        .await;
                }
            }
            let plain = match inflate_request(&raw, bound) {
                Ok(plain) => plain,
                Err(error) => {
                    return self
                        .refused_before_body(
                            &trace,
                            &key,
                            &parts.headers,
                            capture_hours,
                            retired,
                            relay_failure(&error),
                            client_version.as_deref(),
                        )
                        .await;
                }
            };
            // Once the shrink below counts the inflated bytes alone, the compressed copy kept to the end of the
            // request would hold up to the per-body bound the budget no longer sees.
            drop(raw);
            if let Some(reservation) = &mut reservation {
                // The inflated bytes are what the bound and the budget measure from here; a shrink never fails.
                let shrunk = reservation.resize(plain.len() as u64);
                debug_assert!(shrunk.is_ok(), "the inflated body outgrew its reservation");
            }
            Bytes::from(plain)
        } else {
            raw
        };
        let facts = BodyFacts::read(&raw);
        let auto_mode_server = snapshot.auto_mode_server(&key);
        let auto_mode = facts.shape.safeguards.then_some(if auto_mode_server {
            AutoMode::Server
        } else {
            AutoMode::Stripped
        });
        let stream = wants_stream(&raw);
        // Read after the body, so the refusal's row carries the request's shape, and before the first fact is
        // published, so no probe is paid for and no request is accepted that the ledger cannot record while its
        // writer is behind. The refusal's own facts go best-effort: the bus queue fills behind a writer that
        // stopped draining, and a 503 that waited for it would hold the very client it is meant to release.
        let behind = matches!(
            trace.kind,
            Kind::Inference | Kind::CountTokens | Kind::Probe
        ) && self.ledger_standing(&trace, true);
        // Admission and the body edit both act on the first spelling of a member; a second one would reach
        // the wire untouched and a last-wins parser upstream would act on it instead. The billing line's path
        // counts as well: the drift check and its placeholder exemption read the first `system` and the first
        // `text` of its first block, so a second spelling would carry a hash nothing verified. Refused whole
        // rather than repaired: the caller's body is not edited on its way through. A scan that could not
        // finish proves nothing about the members past where it stopped, so it refuses as well.
        let repeated = phoebis_cch::repeated_top_level_member(
            &raw,
            &["metadata", "model", "stream", "system"],
        )
        .and_then(|repeated| match repeated {
            Some(name) => Ok(Some(name)),
            None => phoebis_cch::first_system_block_member_count(&raw, "text")
                .map(|texts| (texts > 1).then_some(BILLING_LINE_PATH)),
        });
        let refusal = if behind {
            Some(RelayError::LedgerBehind)
        } else {
            match repeated {
                Ok(None) => None,
                Ok(Some(name)) => Some(RelayError::BodyShape(BodyMember::Repeated(name))),
                Err(phoebis_cch::CchError::TooDeep(_)) => Some(RelayError::BodyTooDeep),
                Err(error) => Some(RelayError::Cch(error)),
            }
        };
        if let Some(error) = refusal {
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
                    Vec::new(),
                    client_facts(&parts.headers, None, Some(&facts)),
                    auto_mode,
                    presented == Presented::Retired,
                    key.rotated_at,
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
        let mut identity = match identify(&parts.headers, &raw) {
            Ok(identity) => identity,
            Err(error) => {
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
                        Vec::new(),
                        client_facts(&parts.headers, None, Some(&facts)),
                        auto_mode,
                        presented == Presented::Retired,
                        key.rotated_at,
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
        let ids = waits_on(&identity, boundaries);
        let applied = self.router.applied_report();
        if let Some((id, seq)) =
            self.pending_reports
                .pending(&key.id, key.created_at, &ids, applied)
        {
            let deadline = tokio::time::Instant::now() + PENDING_REPORT_WAIT;
            if self.router.wait_for_report(seq, deadline).await {
                self.pending_reports
                    .remove_if(&key.id, key.created_at, &id, seq);
            }
        }
        climb(&mut identity, &key, boundaries, &self.router);
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
                facts
                    .chains
                    .as_ref()
                    .and_then(|c| c.static_hash())
                    .map(String::from),
                facts
                    .chains
                    .as_ref()
                    .map(|c| c.message_boundaries.to_vec())
                    .unwrap_or_default(),
                client_facts(&parts.headers, Some(&identity), Some(&facts)),
                auto_mode,
                presented == Presented::Retired,
                key.rotated_at,
            )
            .await;
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
        // A separate field from `restore`: every client inference request of a conversation gets one,
        // because the diagnostics member the option forces names the previous answer in
        // that conversation whoever sent it. A probe reads under the same key, since it replays the session's own
        // headers and the diagnostics report should name the turn it keeps warm; what a probe is answered is
        // never remembered, which the response half enforces by reading no key for it. A count_tokens request
        // has no slot for either, so it is neither read nor remembered. Dispatch reads the key to look up the
        // previous request id per account, and the response half writes the upstream's `request-id` and
        // message id back under it; what lands on the billing line stays gated on `restore` being `Some`.
        // Keyed on the conversation: the client's own session id when it sent one (Claude Code).
        let memory_key = match trace.kind {
            Kind::Inference | Kind::Probe => identity.session_id.as_deref().map(|session| {
                MemoryKey::for_session(&key.id, key.created_at, session, &identity.agent_id)
            }),
            _ => None,
        };
        // The one store read of the request path, before either the prompt id or the previous request id is
        // read: a miss after a restart or an eviction is filled from tier 2 here, on the blocking pool, so no
        // worker blocks on SQLite for it. A probe and a token count restore nothing, since the prompt id is
        // read for an inference alone.
        if let Some(key) = memory_key
            .as_ref()
            .filter(|_| trace.kind == Kind::Inference)
        {
            let resident = self.memory.restore(key, received_at).await;
            // The parent's row matters only when the child is new: `prompt_id` consults the parent for an
            // agent's first request alone, so a resident child never pays a parent read.
            if !resident {
                if let Some(parent) = identity.parent_agent_id.as_deref() {
                    self.memory
                        .restore(&key.with_agent(parent), received_at)
                        .await;
                }
            }
        }
        // Claude Code's own inference requests alone: a probe replays what the client sent, a token count has no
        // line to fill, and a body without the block is relayed as sent and gets no client request id either.
        let restore = match (client_version.as_deref(), identity.session_id.as_deref()) {
            // The version lands in a line this build signs, so one spelled any other way than the client's
            // own restores nothing rather than a version guessed around whatever it carried.
            (Some(version), _) if trace.kind == Kind::Inference && facts.carries_line => {
                restorable_version(version).map(|version| RestoreContext {
                    cli_version: version.to_string(),
                    prompt_units: facts.prompt_units,
                    prompt_id: memory_key.as_ref().map(|key| {
                        self.memory.prompt_id(
                            key,
                            identity.parent_agent_id.as_deref(),
                            facts.fresh_user_turn,
                            facts.prompt_id.as_deref(),
                            received_at,
                        )
                    }),
                })
            }
            _ => None,
        };
        // The stamp and the suffix depend on the body, the client's version and the prompt units alone, so both are
        // judged once here, after the key's client and model checks and before any account is selected; a leg never
        // meets them, and the refusal's row keeps the model, the session and the agent. The hash goes first, so a
        // stamp only the decoded walk reaches is refused before the suffix's stamp gate reads past it. Only a
        // stamped line is judged for its suffix; a reduced-shape line has its missing fields restored instead.
        let stamp = match crate::body::drift(&raw) {
            Ok(None) => None,
            Ok(Some(kind)) => Some(self.drift_refusal(kind, &parts.headers)),
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
        if let Some(context) = &restore {
            let check = VersionCheck {
                client_version: &context.cli_version,
                prompt_units: context.prompt_units,
            };
            if matches!(crate::body::carries_a_stamp(&raw), Ok(true)) {
                if let Some(kind) = crate::body::version_drift(&raw, check) {
                    let refusal = self.drift_refusal(kind, &parts.headers);
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
            }
        }
        // The pool's switch alone; a request with no pool reads off. A `count_tokens` request never gets it, since
        // its public schema has no slot for the beta or the member to land in.
        let cache_diagnostics_on = snapshot
            .pool(&key.pool_id)
            .is_some_and(|p| p.cache_diagnostics);
        let diagnostics = cache_diagnostics_on && trace.kind != Kind::CountTokens;
        let cache_ttl_policy = snapshot
            .pool(&key.pool_id)
            .map(|p| CacheTtlPolicy::from_json(&p.cache_ttl_json))
            .unwrap_or_default();
        let portable = is_portable(&raw);
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
        // Read once and reused on every attempt of this request: neither fact changes across a failover.
        let hints = ClientHints::read(&parts.headers);
        let lane = crate::lane::lane_of(&identity.canonical_model, &facts, hints.per_turn_effort);
        let admitted = Admitted {
            auto_mode_server,
            cache_ttl_policy,
            caller: &parts.headers,
            chains: facts.chains.as_ref(),
            client_gzip,
            client_version,
            deadline: (self.limits.head_timeout > ATTEMPT_DEADLINE_MARGIN).then(|| {
                received_at
                    + (self.limits.head_timeout - ATTEMPT_DEADLINE_MARGIN).as_millis() as i64
            }),
            diagnostics,
            fallback_ms,
            fast: facts.shape.speed.as_deref() == Some("fast"),
            floor_held: AtomicBool::new(false),
            identity: &identity,
            key: &key,
            lane,
            markers_ttl_ms: facts.markers_ttl_ms,
            memory_key,
            original,
            portable,
            raw: &raw,
            request_id: &trace.request_id,
            restore,
            retry_in_place: Duration::from_secs(failover.retry_in_place_seconds),
            target: &target,
            trace: &trace,
        };
        let mut attempted: Vec<String> = Vec::new();
        // Every hold this request waited through, oldest first: carried onto the next selection so the
        // attempt that follows names the wait on its trace.
        let mut holds: Vec<HoldFacts> = Vec::new();
        let mut attempt: u8 = 0;
        // The accounts this request was held for once already. A freed slot is read, not reserved, so another
        // request can take it before the next selection; without this the same account could hold the request
        // again on every pass.
        let mut held_for: Vec<String> = Vec::new();
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
            let answer = match self
                .router
                .select(SelectRequest {
                    key_id: key.id.clone(),
                    key_generation: key.created_at,
                    agent_id: identity.agent_id.clone(),
                    session_id: identity.session_id.clone(),
                    canonical_model: identity.canonical_model.clone(),
                    candidates: candidates.clone(),
                    portable,
                    attempted: attempted.clone(),
                    now,
                    fallback_ms,
                    revision: snapshot.revision,
                    probe: admitted.trace.kind == Kind::Probe,
                    count: admitted.trace.kind == Kind::CountTokens,
                    request_class: hints.request_class.clone(),
                    context_compacted: hints.context_compacted.is_some(),
                    static_hash: facts
                        .chains
                        .as_ref()
                        .and_then(|c| c.static_hash())
                        .map(String::from),
                    boundaries: facts
                        .chains
                        .as_ref()
                        .map(|c| c.message_boundaries.clone())
                        .unwrap_or_default(),
                    client: client_class,
                    static_boundaries: facts
                        .chains
                        .as_ref()
                        .map(|c| c.static_boundaries.clone())
                        .unwrap_or_default(),
                    marker: admitted.markers_ttl_ms.is_some()
                        || (matches!(admitted.trace.kind, Kind::Inference | Kind::Probe)
                            && admitted.cache_ttl_policy.enabled
                            && admitted.cache_ttl_policy.inject_when_absent),
                    parent_agent_id: identity.parent_agent_id.clone(),
                    holds: holds.clone(),
                    lane: admitted.lane.clone(),
                    fast: admitted.fast,
                })
                .await
            {
                Ok(answer) => answer,
                Err(_) => {
                    return self
                        .refused(
                            &trace,
                            attempt,
                            ROUTER_UNAVAILABLE,
                            Some(&key),
                            Some(&identity),
                            admitted.client_version.as_deref(),
                        )
                        .await
                }
            };
            let selection = match answer.result {
                SelectResult::Selected(selection) => selection,
                // The key the request was admitted on left the registry, or another key took its id, before the
                // selection ran: the answer is the one admission gives a key the registry does not hold, so a
                // request and a probe agree.
                SelectResult::Refused(Refusal::Key(refusal)) => {
                    tracing::debug!(%refusal, key = %key.id, "a request's key was refused after admission");
                    return self
                        .refused(
                            &trace,
                            attempt,
                            key_refusal(&refusal),
                            Some(&key),
                            Some(&identity),
                            admitted.client_version.as_deref(),
                        )
                        .await;
                }
                // The registry moved after admission and no longer admits this request; the answer is the one the
                // current registry would have given at admission.
                SelectResult::Refused(Refusal::Resolve(ResolveError::ModelNotAllowed)) => {
                    return self
                        .refused(
                            &trace,
                            attempt,
                            MODEL_NOT_ALLOWED,
                            Some(&key),
                            Some(&identity),
                            admitted.client_version.as_deref(),
                        )
                        .await
                }
                SelectResult::Exhausted(exhaustion) => {
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
                SelectResult::Held {
                    account_id,
                    mut hold,
                } => {
                    // The account had its one hold of this request, so at its cap it is passed over as one whose
                    // wait ran out.
                    if held_for.contains(&account_id) {
                        held_out = true;
                        attempted.push(account_id);
                        continue;
                    }
                    let started = tokio::time::Instant::now();
                    let deadline = started + admitted.retry_in_place;
                    let freed = self
                        .router
                        .in_flight()
                        .wait_for_slot(&account_id, failover.in_flight_cap, deadline)
                        .await;
                    // Measured here, the one place that knows: the trace of the attempt that follows names the
                    // wait, and so does the completion when no attempt follows.
                    hold.held_ms = started.elapsed().as_millis() as u64;
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
            dialed = true;
            match self
                .dispatch_to(
                    &admitted,
                    &selection,
                    &answer.cancel,
                    answer.ticket,
                    &mut attempt,
                    &mut key_permit,
                )
                .await
            {
                Leg::AtCap => {
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
                    // Every kind rides this, not the probe path alone: the keepalive's is the only reader
                    // today, but a request's own funding is a fact of its selection, not of who asked for it.
                    response.extensions_mut().insert(ServedUnder {
                        in_overage: selection.in_overage,
                    });
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
        let (code, response) = match exhaustion {
            Exhaustion::Untimed if at_cap => {
                let failure = accounts_at_cap(admitted.retry_in_place.as_secs());
                (failure.code, failure.response(admitted.request_id))
            }
            Exhaustion::ModelRejected => (
                MODEL_REJECTED.code,
                exhaustion_response(exhaustion, now, admitted.request_id),
            ),
            Exhaustion::Timed { .. } => (
                "accounts_exhausted",
                exhaustion_response(exhaustion, now, admitted.request_id),
            ),
            Exhaustion::Untimed | Exhaustion::Pinned { .. } => (
                "accounts_unavailable",
                exhaustion_response(exhaustion, now, admitted.request_id),
            ),
            Exhaustion::FastLane { .. } => (
                "fast_lane_exhausted",
                exhaustion_response(exhaustion, now, admitted.request_id),
            ),
            Exhaustion::FastCredits { .. } => (
                "fast_credits_unavailable",
                exhaustion_response(exhaustion, now, admitted.request_id),
            ),
        };
        let mut outcome = self.outcome(
            admitted.trace,
            Some(admitted.key),
            Some(admitted.identity),
            "",
            0,
            attempt,
            false,
            None,
            admitted.client_version.clone(),
            Some(code),
            None,
            None,
            None,
        );
        if let Exhaustion::Timed { window, .. } = exhaustion {
            if window == BUDGET_WINDOW {
                outcome.obstacle = Some(BUDGET_WINDOW);
            }
        }
        if let Exhaustion::Pinned { obstacle } = exhaustion {
            outcome.obstacle = Some(*obstacle);
        }
        // Read last, so it overrides `attempted` alone: a probe that tried its marked member is not one to retry,
        // and no wait clears the mark. A timed answer keeps the quota pause its 429 gets, a budget hold stays
        // one, and a pinned member's other standings keep their own reasons.
        if admitted.trace.kind == Kind::Probe
            && auth_marked
            && code == "accounts_unavailable"
            && outcome
                .obstacle
                .is_none_or(|obstacle| obstacle == "attempted")
        {
            outcome.obstacle = Some(REAUTH_OBSTACLE);
        }
        self.answer(
            admitted.trace,
            response,
            outcome,
            None,
            ResponseFacts::default(),
            None,
        )
        .await
    }

    /// One account's leg of the request. `attempt` is the wire-submission index every bus event and attempt row
    /// carries. `key_permit` is the request's own, which a committed stream takes into its body; `selected` is the
    /// ticket the selection charged for the leg's first pass.
    ///
    /// An OAuth 401 runs the refresh ladder on this account: a refresh, and after one that failed without a
    /// conclusive answer a second once the account's backoff clears, the key's permit and the ticket held through
    /// the wait. The token endpoint's floor is waited out once per request, through `Admitted::floor_held`, since a
    /// second account's bad minute is the same minute. Every wait ends at `Admitted::deadline`, past which the
    /// attempt loop answers the exhaustion.
    async fn dispatch_to(
        &self,
        admitted: &Admitted<'_>,
        selection: &Selection,
        cancel: &CancellationToken,
        mut selected: Option<AccountTicket>,
        attempt: &mut u8,
        key_permit: &mut Option<KeyPermit>,
    ) -> Leg {
        // The row the router selected, not one looked up again in the admission snapshot: the endpoint a
        // request goes out to and the credential it carries have to be the pair one snapshot held.
        let account = &selection.account;
        let pin_epoch = selection.pin.as_ref().map(|pin| pin.epoch);
        // No wire submission, so the index does not move: the account is already in `attempted`, and the next
        // selection skips it. The outer loop is bounded by the pool size, because each account is selected
        // at most once per request.
        let mut credential = match self.credential(account).await {
            Ok(credential) => credential,
            Err(_) => return Leg::FailOver { auth_marked: false },
        };
        // Stable across a same-account replay: an OAuth refresh replaces the token, never the account identity
        // it names, so this is read once rather than through the closure below.
        let account_uuid = match admitted.trace.kind {
            // The public `count_tokens` schema carries no `metadata` member and the upstream refuses unknown
            // top-level ones, so there is no slot the account identity could go in. It is not billed either, so
            // nothing is lost by leaving the body without it.
            Kind::CountTokens => None,
            Kind::Inference | Kind::Probe => credential.account_uuid.clone(),
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
            // Read ahead of every other edit, none of which add or remove a `cache_control` member, so this
            // matches what the cache TTL rewrite itself will find.
            let carries_a_marker =
                phoebis_cch::cache_control_spans(&edited).is_ok_and(|spans| !spans.is_empty());
            let mut body_injected = Vec::new();
            let mut inject_betas: Vec<Injected> = Vec::new();
            // An api_key account, or an account in overage, pays for the write, so the metered set applies.
            let writes_cost_money = selection.in_overage || account.kind == AccountKind::ApiKey;
            // Priced per attempt: a migrated attempt reads its destination's overage, and a same-account
            // retry reuses this body rather than pricing again.
            // A probe replays another turn's markers under that turn's agent, so the branch follows the agent for
            // a probe as for the turn, and a subagent's rewrite never reads the main turn's per-model override.
            let branch = if admitted.identity.agent_id == "main" {
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
            let markerless = match admitted.trace.kind {
                Kind::CountTokens => Markerless::GoesAsSent,
                Kind::Inference | Kind::Probe => {
                    if admitted.cache_ttl_policy.inject_when_absent {
                        Markerless::TakesTheMember
                    } else {
                        Markerless::GoesAsSent
                    }
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
            // A marker-less body that went as sent claims no TTL, whether because it is a count or because
            // the toggle left it alone: no beta for it, none dropped for it, and none reported. A body that
            // already carried a marker claims the rewrite's TTL even where the marker already read that way,
            // so a leg does not lose its report to a rewrite that had nothing left to change. A beta naming an
            // hour the body does not carry would be the relay's to answer for on a 400.
            let claims_its_ttl = |edits: &BodyEdits| carries_a_marker || edits.cache_ttl_edited;
            // Isolated per account, so a failover never carries one account's previous request id, or the
            // previous message id a diagnostics member names, into another account's memory.
            let previous = memory_key
                .map(|key| self.memory.previous(key, &account.id, folded_model))
                .unwrap_or_default();
            let diagnostics = (admitted.diagnostics && !skip.contains(&Injected::Diagnostics))
                .then_some(previous.message_id.as_deref());
            let edits = edit_body(
                &mut edited,
                &NativeEdit {
                    account_uuid: account_uuid.as_deref(),
                    upstream_model: &selection.upstream_model,
                    restore: restore_with(&admitted.restore, &previous, skip),
                    diagnostics,
                    cache_ttl: ttl,
                    markerless,
                    safeguards: if admitted.auto_mode_server {
                        SafeguardsEdit::Keep
                    } else {
                        SafeguardsEdit::Drop
                    },
                    sign_placeholder: self.sign_cch_placeholder,
                },
            )?;
            if !claims_its_ttl(&edits) {
                ttl = None;
            }
            body_injected.extend(edits.restored);
            if admitted.diagnostics && !skip.contains(&Injected::CacheDiagnosisBeta) {
                inject_betas.push(Injected::CacheDiagnosisBeta);
            }
            if ttl == Some(CacheTtl::OneHour) {
                inject_betas.push(Injected::ExtendedTtlBeta);
            }
            Ok((edited, body_injected, inject_betas, ttl))
        };
        // Seeded from what this account has already refused, so a leg the operator has not touched since the
        // last 400 never resubmits a field or a beta the upstream is known to reject.
        let mut skip: BTreeSet<Injected> = INJECTED_KINDS
            .into_iter()
            .filter(|item| self.memory.refuses(account, *item))
            .collect();
        let (mut edited, mut body_injected, mut inject_betas, mut ttl) = match build(&skip) {
            Ok(built) => built,
            Err(error) => {
                return Leg::Respond(
                    self.refused(
                        admitted.trace,
                        *attempt,
                        relay_failure(&error),
                        Some(admitted.key),
                        Some(admitted.identity),
                        admitted.client_version.as_deref(),
                    )
                    .await,
                );
            }
        };
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

        loop {
            // The wire-submission index is a u8 on the bus and the attempt row, so 255 submissions per request
            // is the technical bound; the pool's size is the practical one, and no pool approaches it.
            if *attempt == u8::MAX {
                return Leg::AtCap;
            }
            *attempt += 1;
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
                    pin_epoch,
                    (self.clock)(),
                    cache_ttl_ms(ttl),
                    // A probe replays a recipe rather than making its own placement, so its own trace names no
                    // choice worth showing on a request drawer.
                    (admitted.trace.kind != Kind::Probe).then_some(&selection.trace),
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
            let gzip_out = match self.limits.upstream_gzip {
                UpstreamGzip::On => true,
                UpstreamGzip::Off => false,
                UpstreamGzip::Auto => admitted.client_gzip,
            };
            let wire_body = if gzip_out {
                phoebis_transport::encode::gzip(&edited)
            } else {
                edited.clone()
            };
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
            let responding = Responding {
                account,
                attempt: *attempt,
                auth_marked: false,
                cache_ttl_ms: cache_ttl_ms(ttl),
                caller: admitted.caller,
                can_retry_in_place: !retried_in_place,
                can_strip: !stripped_once,
                cancel,
                chains: admitted.chains,
                client_version: admitted.client_version.clone(),
                credential_generation: credential.generation,
                deadline: admitted.deadline,
                fallback_ms: admitted.fallback_ms,
                fast: admitted.fast,
                floor_held: &admitted.floor_held,
                fresh_pin: selection.pin.is_some() && !selection.trace.renewed,
                identity: admitted.identity,
                injected: &injected,
                key: admitted.key,
                key_permit: &mut *key_permit,
                lane: &admitted.lane,
                markers_ttl_ms: admitted.markers_ttl_ms,
                // A probe's answer is never remembered: the id it was given belongs to the turn it replayed,
                // and recording it would make the next real turn name a probe.
                memory_key: admitted
                    .memory_key
                    .as_ref()
                    .filter(|_| admitted.trace.kind != Kind::Probe),
                pin: selection.pin.as_ref(),
                pin_epoch,
                portable: admitted.portable,
                replayed,
                request_id: admitted.request_id,
                retry_in_place: admitted.retry_in_place,
                ticket,
                tier_ttls: tier_ttls(&edited),
                trace: admitted.trace,
                uniform_ttl_ms: uniform_ttl(&edited),
            };
            let request = RawRequest {
                method: "POST",
                target: admitted.target.to_string(),
                headers: head.headers,
                body: Bytes::from(wire_body),
            };
            let dispatched =
                match dispatch::attempt(self, &request, &pool_key, authority, addr, fresh, cancel)
                    .await
                {
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
            let next = match dispatched {
                Ok(next) => next,
                Err(AttemptFailure::Unsent(detail)) if !retried => {
                    tracing::debug!(account = %account.id, %detail, "retrying a request that never reached the upstream");
                    fresh = true;
                    retried = true;
                    continue;
                }
                Err(AttemptFailure::Cancelled) => {
                    let outcome = self.outcome(
                        admitted.trace,
                        Some(admitted.key),
                        Some(admitted.identity),
                        &account.id,
                        0,
                        *attempt,
                        false,
                        None,
                        admitted.client_version.clone(),
                        Some("account_cancelled"),
                        pin_epoch,
                        None,
                        cache_ttl_ms(ttl),
                    );
                    return Leg::Respond(
                        self.answer(
                            admitted.trace,
                            ACCOUNT_CANCELLED.response(admitted.request_id),
                            outcome,
                            None,
                            ResponseFacts::default(),
                            None,
                        )
                        .await,
                    );
                }
                Err(failure) => {
                    let written = matches!(failure, AttemptFailure::Lost(_));
                    tracing::warn!(account = %account.id, %failure, "the upstream did not answer");
                    let outcome = self.outcome(
                        admitted.trace,
                        Some(admitted.key),
                        Some(admitted.identity),
                        &account.id,
                        0,
                        *attempt,
                        written,
                        None,
                        admitted.client_version.clone(),
                        Some("upstream_unreachable"),
                        pin_epoch,
                        None,
                        cache_ttl_ms(ttl),
                    );
                    return Leg::Respond(
                        self.answer(
                            admitted.trace,
                            UPSTREAM_UNREACHABLE.response(admitted.request_id),
                            outcome,
                            None,
                            ResponseFacts::default(),
                            None,
                        )
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
                Next::FailOver { auth_marked } => return Leg::FailOver { auth_marked },
                Next::Respond(response) => return Leg::Respond(response),
                // The same account and the same credential once the throttle has passed.
                Next::RetryInPlace(delay) => {
                    if until_cancelled(tokio::time::sleep(delay), cancel)
                        .await
                        .is_none()
                    {
                        let outcome = self.outcome(
                            admitted.trace,
                            Some(admitted.key),
                            Some(admitted.identity),
                            &account.id,
                            0,
                            *attempt,
                            true,
                            None,
                            admitted.client_version.clone(),
                            Some("account_cancelled"),
                            pin_epoch,
                            None,
                            cache_ttl_ms(ttl),
                        );
                        return Leg::Respond(
                            self.answer(
                                admitted.trace,
                                ACCOUNT_CANCELLED.response(admitted.request_id),
                                outcome,
                                None,
                                ResponseFacts::default(),
                                None,
                            )
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
                            edited = rebuilt;
                            body_injected = restored;
                            inject_betas = betas;
                            ttl = rebuilt_ttl;
                        }
                        Err(error) => {
                            tracing::error!(account = %account.id, %error, "a strip retry could not rebuild the body it just stripped from");
                            return Leg::Respond(
                                self.refused(
                                    admitted.trace,
                                    *attempt,
                                    relay_failure(&error),
                                    Some(admitted.key),
                                    Some(admitted.identity),
                                    admitted.client_version.as_deref(),
                                )
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
    async fn respond<C: AsyncRead + ReturnToPool + Send + Unpin + 'static>(
        &self,
        mut ctx: Responding<'_>,
        sent: Sent<C>,
        checkin: Checkin,
    ) -> Next {
        let Sent {
            conn,
            head,
            leftover,
            written_at,
        } = sent;
        // When the head arrived, which is what stamps the overage evidence below for an upstream that
        // sends no `Date` of its own.
        let head_at = (self.clock)();
        let framing = framing(&head);
        let reusable = keep_alive(&head, &framing);
        let mut reader = BodyReader::new(conn, leftover, framing);
        let mut outcome = self.outcome(
            ctx.trace,
            Some(ctx.key),
            Some(ctx.identity),
            &ctx.account.id,
            head.status,
            ctx.attempt,
            true,
            None,
            ctx.client_version.clone(),
            None,
            ctx.pin_epoch,
            Some(head_at),
            ctx.cache_ttl_ms,
        );
        outcome.upstream_request_id = head.get("request-id").map(str::to_string);
        self.report_overage(ctx.account, &head, head_at).await;
        self.report_head_sample(ctx.account, &head, &ctx.identity.canonical_model, head_at)
            .await;

        // Heads the table reads a body for are drained within the bound; a body too large to read
        // commits. The reader survives only where the body was left alone, because a drained body has
        // already handed its connection back to the pool.
        let (buffered, mut reader) = if matches!(head.status, 400 | 401 | 403 | 404 | 429) {
            let Some(drained) = until_cancelled(
                drain_bounded(&mut reader, self.limits.idle_timeout),
                ctx.cancel,
            )
            .await
            else {
                return Next::Respond(self.cancelled(&ctx, &head, written_at).await);
            };
            match drained {
                Ok(Drained::Complete(buffered)) => {
                    if reusable {
                        reader.into_inner().return_to(checkin.clone());
                    }
                    (Some(buffered), None)
                }
                // The body is past the buffer bound, so nothing can be read that would justify a refresh or a
                // mark: the leg fails over once, unmarked, rather than act on a head nobody classified, and the
                // 401 never reaches the client.
                Ok(Drained::TooLarge(_)) if head.status == 401 => {
                    tracing::debug!(account = %ctx.account.id, "failing over from a 401 too large to inspect");
                    drop(reader);
                    ctx.trace
                        .head(
                            ctx.attempt,
                            &head,
                            None,
                            AttemptOutcome::AuthFailed,
                            None,
                            written_at,
                            head_at,
                        )
                        .await;
                    let generations = (
                        ctx.account.incarnation,
                        ctx.account.lifecycle_generation,
                        ctx.credential_generation,
                    );
                    report_failover(
                        &self.router,
                        &ctx.account.id,
                        &ctx.identity.canonical_model,
                        None,
                        None,
                        ctx.portable.then(|| ctx.pin.cloned()).flatten(),
                        ctx.request_id,
                        generations,
                        (self.clock)(),
                    )
                    .await;
                    if ctx.portable {
                        return Next::FailOver { auth_marked: false };
                    }
                    let release = self.release_on(&ctx, head.status);
                    return Next::Respond(
                        self.authentication_exhausted(&mut ctx, &head, outcome, release)
                            .await,
                    );
                }
                Ok(Drained::TooLarge(prefix)) => {
                    tracing::debug!(account = %ctx.account.id, "forwarding a rejection too large to inspect");
                    let release = self.release_on(&ctx, head.status);
                    return Next::Respond(
                        self.stream(Forward {
                            affinity: None,
                            caller: ctx.caller,
                            cancel: ctx.cancel.clone(),
                            checkin,
                            guards: ctx.guards(),
                            head: &head,
                            keep_alive: reusable,
                            observer: None,
                            on_message_id: None,
                            outcome,
                            prefix,
                            rate_limit_event: false,
                            reader,
                            renewal: release,
                            trace: ctx.trace,
                        })
                        .await,
                    );
                }
                Err(error) => {
                    tracing::warn!(account = %ctx.account.id, %error, "the rejection body was not readable");
                    return Next::Respond(
                        self.answer(
                            ctx.trace,
                            UPSTREAM_UNREACHABLE.response(ctx.request_id),
                            unreachable_outcome(outcome),
                            None,
                            ResponseFacts::default(),
                            None,
                        )
                        .await,
                    );
                }
            }
        } else {
            (None, Some(reader))
        };
        let decoded = buffered.as_ref().and_then(|b| inspectable_copy(&head, b));
        // A streaming 200 is held until it commits: an error event ahead of `message_start` is a
        // rejection the caller must never see, because the request can still go to another account.
        let streaming = head.status == 200
            && head
                .get("content-type")
                .is_some_and(|v| v.to_ascii_lowercase().starts_with("text/event-stream"));
        let mut held: Option<Held> = None;
        let mut first_event: Option<Bytes> = None;
        if let (true, Some(reader)) = (streaming, reader.as_mut()) {
            let inspect = Inspect::new(match encoding(&head) {
                Ok(Encoding::Identity) => None,
                Ok(coding) => Some(decoder(coding)),
                Err(_) => None,
            });
            let Some(result) =
                until_cancelled(hold(reader, inspect, self.limits.hold_timeout), ctx.cancel).await
            else {
                return Next::Respond(self.cancelled(&ctx, &head, written_at).await);
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
                    return Next::Respond(
                        self.answer(
                            ctx.trace,
                            UPSTREAM_UNREACHABLE.response(ctx.request_id),
                            unreachable_outcome(outcome),
                            None,
                            ResponseFacts::default(),
                            None,
                        )
                        .await,
                    );
                }
            }
        }
        // The commit's own arrival time: a held stream may have taken most of the hold timeout to commit.
        let arrived = (self.clock)();
        if !ctx.can_strip && head.status == 400 && latches(ctx.trace) {
            // This leg cannot retry a strip, so the 400 is forwarded as it came; the account is still latched
            // against the item it named, a fact about the account whether or not this request could use it.
            // Gated on the same error type the strip path itself requires, so a 400 of another kind that
            // happens to spell an injected name in its message never latches on the strength of a coincidence.
            if let Some(body) = decoded
                .as_deref()
                .filter(|b| error_type(b).as_deref() == Some("invalid_request_error"))
            {
                for item in refused_names(body, ctx.injected) {
                    self.memory.latch(ctx.account, item);
                }
            }
        }
        let verdict = classify(
            &head,
            &HeadContext {
                kind: ctx.account.kind,
                canonical_model: &ctx.identity.canonical_model,
                now: arrived,
                fallback_ms: ctx.fallback_ms,
                first_event: first_event.as_deref(),
                body_prefix: decoded.as_deref(),
                injected: ctx.injected,
                can_strip: ctx.can_strip,
                wire_model: &ctx.identity.folded_model,
                model_rejection_ms: self.limits.model_rejection.as_millis() as i64,
                fast: ctx.fast,
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
                    mark: Some(AccountState::ReauthRequired),
                    ..
                }
        );
        ctx.trace
            .head(
                ctx.attempt,
                &head,
                committing,
                attempt_outcome(&verdict, head.status, first_event.as_deref()),
                cooldown_set(cooldown_seen, &ctx.identity.canonical_model),
                written_at,
                head_at,
            )
            .await;
        let generations = (
            ctx.account.incarnation,
            ctx.account.lifecycle_generation,
            ctx.credential_generation,
        );

        let (cooldown, mark, moves_pin) = match verdict {
            // A rejected replay is conclusive: the refreshed token was refused too.
            Verdict::Refresh if ctx.replayed => (None, Some(AccountState::ReauthRequired), false),
            Verdict::Refresh => {
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
                                    | AcquireError::ReauthRequired
                                    | AcquireError::Unpaired
                            );
                            if matches!(error, AcquireError::ReauthRequired) {
                                ctx.auth_marked = true;
                            }
                            if marked || tries == 2 {
                                break (None, None, !marked);
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
                                || (floor && ctx.floor_held.load(Ordering::Relaxed))
                            {
                                break (None, None, true);
                            }
                            if floor {
                                ctx.floor_held.store(true, Ordering::Relaxed);
                            }
                            match self.wait_out(backoff.until, ctx.deadline, ctx.cancel).await {
                                Waited::Cleared => {}
                                // The loop head answers the exhaustion past the deadline; this account is done.
                                Waited::Deadline => break (None, None, true),
                                // The 401's head is this attempt's, published before the verdict.
                                Waited::Cancelled => {
                                    return Next::Respond(
                                        self.cancelled_after_head(&ctx, head.status).await,
                                    )
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
                if latches(ctx.trace) {
                    for item in &items {
                        self.memory.latch(ctx.account, *item);
                    }
                }
                return Next::Strip(items);
            }
            // An account marked for re-authentication keeps its pin, as a refresh's own mark does: the next
            // selection moves the session off it under the account's state.
            Verdict::FailOver { cooldown, mark } => {
                (cooldown, mark, mark != Some(AccountState::ReauthRequired))
            }
            Verdict::Commit => {
                // The key may have been evicted and its record saved while this request was upstream. The
                // writes below would then build a fresh entry, with no prompt id and none of the session's
                // other accounts, and the next flush would overwrite the saved record with it. One lock while
                // the entry is still resident, which is the usual case.
                if let Some(key) = ctx.memory_key {
                    self.memory.restore(key, arrived).await;
                }
                if let (Some(key), Some(id)) = (ctx.memory_key, head.get("request-id")) {
                    self.memory
                        .record_request_id(key, &ctx.account.id, id, arrived);
                }
                // Built once and moved into whichever branch below commits: a JSON answer and a streamed one
                // both reach the caller through `stream()`, so both parse for a message id the same way.
                let on_message_id: Option<
                    Box<dyn FnOnce(String) -> BoxFuture<'static, ()> + Send>,
                > = ctx.memory_key.map(|key| {
                    let memory = self.memory.clone();
                    let key = key.clone();
                    let account_id = ctx.account.id.clone();
                    // The same wire-spelling key `previous` reads above, not the resolved identity: see its
                    // comment.
                    let model = ctx.identity.folded_model.clone();
                    let clock = self.clock.clone();
                    Box::new(move |id: String| {
                        // Awaited on the body task before the next piece goes out, so the caller's next
                        // request finds the id in memory and a shutdown flush carries it; the restore is
                        // a store read only on a miss. Stamped at fire time, so a slow body never moves
                        // `last_seen` back to the head's clock.
                        Box::pin(async move {
                            let now = (clock)();
                            memory.restore(&key, now).await;
                            memory.record_message_id(&key, &account_id, &model, &id, now);
                        }) as BoxFuture<'static, ()>
                    })
                        as Box<dyn FnOnce(String) -> BoxFuture<'static, ()> + Send>
                });
                // Sent where the answer ends, the only place its usage is known.
                let renewal = self.renewal(&ctx);
                let affinity = self.affinity(&ctx);
                let (prefix, observer) = match held {
                    None => (Bytes::new(), None),
                    Some(Held { bytes, inspect, .. }) => {
                        let fallback_ms = ctx.fallback_ms;
                        let model_rejection_ms = self.limits.model_rejection.as_millis() as i64;
                        let fast = ctx.fast;
                        // Two spellings captured for two purposes that diverge once a resolution lands: the
                        // canonical id is the cooldown's scope and `report_failover`'s model, the folded
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
                            ctx.identity.canonical_model.clone(),
                            ctx.identity.folded_model.clone(),
                            ctx.portable.then(|| ctx.pin.cloned()).flatten(),
                            ctx.request_id.to_string(),
                        );
                        let head = ResponseHead {
                            status: head.status,
                            headers: head.headers.clone(),
                        };
                        let on_error = Box::new(move |data: Bytes, waiting: &mut Waiting| {
                            // After commit nothing fails over. A rate-limit error records the cooldown
                            // and, on a portable request, invalidates the pin so the next request
                            // migrates; a non-portable request keeps its pin. The deadline is measured
                            // from the error's arrival, not from the selection.
                            let now = clock();
                            if let Verdict::FailOver { cooldown, mark } = classify(
                                &head,
                                &HeadContext {
                                    kind,
                                    canonical_model: &canonical_model,
                                    now,
                                    fallback_ms,
                                    first_event: Some(&data),
                                    body_prefix: None,
                                    // Nothing is stripped after a commit: this observer only records a
                                    // rate-limit cooldown, and a 400 (which alone can name an injected item)
                                    // never reaches it.
                                    injected: &[],
                                    can_strip: false,
                                    wire_model: &wire_model,
                                    model_rejection_ms,
                                    fast,
                                },
                            ) {
                                for report in failover_reports(
                                    &account_id,
                                    &canonical_model,
                                    cooldown,
                                    mark,
                                    pin,
                                    &request_id,
                                    generations,
                                    now,
                                ) {
                                    waiting.send(&router, report);
                                }
                            }
                        }) as OnError;
                        (bytes, Some(PostCommit { inspect, on_error }))
                    }
                };
                let responded = match buffered {
                    Some(buffered) => {
                        let guards = ctx.guards();
                        self.forward_buffered(
                            ctx.trace, &head, buffered, ctx.caller, outcome, renewal, affinity,
                            guards,
                        )
                        .await
                    }
                    None => {
                        let guards = ctx.guards();
                        self.stream(Forward {
                            affinity,
                            caller: ctx.caller,
                            cancel: ctx.cancel.clone(),
                            checkin,
                            guards,
                            head: &head,
                            keep_alive: reusable,
                            observer,
                            on_message_id,
                            outcome,
                            prefix,
                            rate_limit_event: false,
                            reader: reader.expect(
                                "a head the table reads no body for still holds its reader",
                            ),
                            renewal,
                            trace: ctx.trace,
                        })
                        .await
                    }
                };
                return Next::Respond(responded);
            }
        };
        if mark == Some(AccountState::ReauthRequired) {
            ctx.auth_marked = true;
        }
        let retry_in_place = self.retry_in_place(&ctx, &head, cooldown.as_ref(), mark, arrived);
        // A non-portable request keeps its pin: the answer goes back to the client on the account that holds
        // its state, and only the cooldown is recorded. So does one about to retry in place: the row is there
        // for the siblings, and the pin stays where the retry lands.
        report_failover(
            &self.router,
            &ctx.account.id,
            &ctx.identity.canonical_model,
            cooldown,
            mark,
            (ctx.portable && moves_pin && retry_in_place.is_none())
                .then(|| ctx.pin.cloned())
                .flatten(),
            ctx.request_id,
            generations,
            arrived,
        )
        .await;
        if let Some(delay) = retry_in_place {
            drop(reader);
            return Next::RetryInPlace(delay);
        }
        // A held stream cannot be rewound onto another connection, so the only way not to commit it is to drop
        // it unread; a request bound to its account takes it as it came.
        if let Some(held) = held {
            if ctx.portable {
                drop(reader);
                return Next::FailOver {
                    auth_marked: ctx.auth_marked,
                };
            }
            let guards = ctx.guards();
            return Next::Respond(
                self.stream(Forward {
                    affinity: None,
                    caller: ctx.caller,
                    cancel: ctx.cancel.clone(),
                    checkin,
                    guards,
                    head: &head,
                    keep_alive: false,
                    observer: None,
                    on_message_id: None,
                    outcome,
                    prefix: held.bytes,
                    rate_limit_event: first_event
                        .as_deref()
                        .is_some_and(|e| error_type(e).as_deref() == Some("rate_limit_error")),
                    reader: reader
                        .expect("a streaming head still holds the reader the hold read from"),
                    renewal: None,
                    trace: ctx.trace,
                })
                .await,
            );
        }
        // Every failover row the table produces answers 400, 401, 403, 404, or 429, all of which were
        // drained above, so the default here is the empty body of a head that carried none. A row
        // added for a status outside that set would forward nothing instead of its own body.
        let buffered = buffered.unwrap_or_default();
        if ctx.portable {
            return Next::FailOver {
                auth_marked: ctx.auth_marked,
            };
        }
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
        Next::Respond(
            self.forward_buffered(
                ctx.trace,
                &head,
                buffered,
                ctx.caller,
                outcome,
                None,
                None,
                Guards::default(),
            )
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
            release.send((self.clock)(), head.status, None, false, &mut waiting);
        }
        outcome.error_code = Some("accounts_unavailable".to_string());
        // A probe bound to its account never reaches the selector's exhaustion, so its own answer names the mark.
        if ctx.trace.kind == Kind::Probe && ctx.auth_marked {
            outcome.obstacle = Some(REAUTH_OBSTACLE);
        }
        let response = self
            .answer(
                ctx.trace,
                exhaustion_response(&Exhaustion::Untimed, (self.clock)(), ctx.request_id),
                outcome,
                None,
                ResponseFacts::default(),
                None,
            )
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
        ctx.pin.map(|pin| Renewal {
            router: self.router.clone(),
            pin: pin.clone(),
            origin: match ctx.trace.kind {
                Kind::Probe => "keepalive",
                Kind::CountTokens | Kind::Inference => "client",
            },
            cache_ttl_ms: ctx
                .cache_ttl_ms
                .or(ctx.markers_ttl_ms)
                .unwrap_or(DEFAULT_CACHE_TTL_MS),
            fresh: ctx.fresh_pin,
            renews: ctx.trace.kind != Kind::CountTokens,
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
        if ctx.trace.kind == Kind::CountTokens {
            return None;
        }
        let root = ctx
            .identity
            .session_id
            .is_some()
            .then(|| ctx.identity.session_id.clone())
            .flatten();
        Some(AffinityFacts {
            router: self.router.clone(),
            pending: self.pending_reports.clone(),
            session_id: ctx.identity.session_id.clone(),
            account_id: ctx.account.id.clone(),
            incarnation: ctx.account.incarnation,
            key_id: ctx.key.id.clone(),
            key_generation: ctx.key.created_at,
            canonical_model: ctx.identity.canonical_model.clone(),
            lane: ctx.lane.to_string(),
            root,
            static_boundaries: ctx
                .chains
                .map(|c| c.static_boundaries.clone())
                .unwrap_or_default(),
            static_sizes: ctx
                .chains
                .map(|c| c.static_sizes.clone())
                .unwrap_or_default(),
            message_boundaries: ctx
                .chains
                .map(|c| c.message_boundaries.clone())
                .unwrap_or_default(),
            static_ttl_ms: ctx.tier_ttls.0,
            message_ttl_ms: ctx.tier_ttls.1,
            uniform_ttl_ms: ctx.uniform_ttl_ms,
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
        cooldown: Option<&Cooldown>,
        mark: Option<AccountState>,
        now: i64,
    ) -> Option<Duration> {
        let cooldown = cooldown?;
        if !ctx.portable
            || mark.is_some()
            || !ctx.can_retry_in_place
            || cooldown.window != "retry_after"
            || retry_after(head, now).is_none()
        {
            return None;
        }
        let delay = cooldown.until.saturating_sub(now).saturating_sub(MARGIN_MS);
        let delay = Duration::from_millis(u64::try_from(delay).unwrap_or(0));
        // The deadline bounds this wait as it bounds the refresh ladder's: the retry a longer wait leads to would
        // be submitted after the request is due its answer, with its permits and buffers held all the while, so
        // the request moves on at once instead.
        let left = ctx.deadline.map(|deadline| {
            Duration::from_millis(u64::try_from(deadline.saturating_sub(now)).unwrap_or(0))
        });
        (delay <= ctx.retry_in_place && left.is_none_or(|left| delay < left)).then_some(delay)
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
                    return Err(NoCredential::Unavailable);
                }
            },
            // The generation comes along so an api_key verdict is fenced by the same rule an OAuth one is.
            AccountKind::ApiKey => match self.api_key_secret(account, expected).await {
                Ok(Some((secret, generation))) => Credential {
                    account_uuid: None,
                    generation,
                    kind: CredentialKind::ApiKey { secret },
                },
                Ok(None) => {
                    tracing::warn!(account = %account.id, "the account has no Anthropic key stored");
                    return Err(NoCredential::Unavailable);
                }
                Err(error) => {
                    tracing::error!(account = %account.id, %error, "the stored Anthropic key is unreadable");
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

    /// The account's Anthropic key and its generation: the cached entry while its holder and lifecycle are the
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
        let behind = self
            .accounting
            .as_ref()
            .is_some_and(|source| source(counts).behind);
        if behind {
            trace.facts_best_effort();
        }
        behind
    }

    /// A refusal with no body to observe: decided on the head before the read, or on a read that ended without
    /// a whole body. The observation carries the client facts the head alone gives.
    #[allow(clippy::too_many_arguments)]
    async fn refused_before_body(
        &self,
        trace: &Trace,
        key: &ApiKeyRow,
        headers: &HeaderMap,
        capture_hours: u32,
        presented_retired: bool,
        failure: Failure,
        client_version: Option<&str>,
    ) -> Response {
        self.ledger_standing(trace, false);
        trace
            .observed(
                Some(key),
                None,
                headers,
                false,
                Bytes::new(),
                false,
                capture_hours,
                RequestShape::default(),
                None,
                Vec::new(),
                client_facts(headers, None, None),
                None,
                presented_retired,
                key.rotated_at,
            )
            .await;
        self.refused(trace, 0, failure, Some(key), None, client_version)
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
        } = refusal.into();
        let outcome = self.outcome(
            trace,
            key,
            identity,
            "",
            0,
            attempt,
            false,
            client_drift.map(str::to_string),
            client_version.map(str::to_string),
            Some(failure.code),
            None,
            None,
            None,
        );
        self.answer(
            trace,
            failure.response(&trace.request_id),
            outcome,
            None,
            ResponseFacts::default(),
            None,
        )
        .await
    }

    /// A client-drift refusal with its trigger, and the warn that tells the operator which client to re-pin to.
    fn drift_refusal(&self, kind: &'static str, headers: &HeaderMap) -> LocalRefusal {
        tracing::warn!(
            drift = %kind,
            user_agent = %user_agent(headers),
            profile = %self.profile_name,
            "a caller stamped a billing line this build cannot reproduce; re-pin the router to that client",
        );
        LocalRefusal {
            failure: relay_failure(&RelayError::ClientDrift { kind }),
            client_drift: Some(kind),
        }
    }

    /// A locally built answer: the outcome rides on the response for the accounting layer, and the completion
    /// is published with the status the response carries, which for a local refusal is the refusal's own. Built
    /// entirely in memory, so it carries no relayed chunks.
    async fn answer(
        &self,
        trace: &Trace,
        response: Response,
        outcome: RelayOutcome,
        usage: Option<Usage>,
        facts: ResponseFacts,
        response_bytes: Option<u64>,
    ) -> Response {
        trace
            .complete(
                &outcome,
                response.status().as_u16(),
                usage,
                facts,
                None,
                response_bytes,
                None,
                None,
                None,
                (self.clock)(),
            )
            .await;
        with_outcome(response, outcome)
    }

    /// Sleeps until the relay's clock reaches `until`, or `deadline` when that comes first. The clock is read again
    /// after every `HOLD_TICK` rather than slept against once, so a clock a test steps is noticed within a tick and
    /// production time passes as it does.
    async fn wait_out(
        &self,
        until: i64,
        deadline: Option<i64>,
        cancel: &CancellationToken,
    ) -> Waited {
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
            if until_cancelled(tokio::time::sleep(slice), cancel)
                .await
                .is_none()
            {
                return Waited::Cancelled;
            }
        }
    }

    /// The answer to a response that arrived for an account that is no longer there.
    async fn cancelled(
        &self,
        ctx: &Responding<'_>,
        head: &ResponseHead,
        written_at: i64,
    ) -> Response {
        ctx.trace
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
        self.cancelled_after_head(ctx, head.status).await
    }

    /// The caller's answer once the account left the router after this attempt's head was published: the request
    /// completes `canceled` and the attempt keeps the outcome its head recorded, since a second head for the same
    /// attempt would overwrite that outcome in the ledger.
    async fn cancelled_after_head(&self, ctx: &Responding<'_>, status: u16) -> Response {
        let outcome = self.outcome(
            ctx.trace,
            Some(ctx.key),
            Some(ctx.identity),
            &ctx.account.id,
            status,
            ctx.attempt,
            true,
            None,
            ctx.client_version.clone(),
            Some("account_cancelled"),
            ctx.pin_epoch,
            None,
            ctx.cache_ttl_ms,
        );
        self.answer(
            ctx.trace,
            ACCOUNT_CANCELLED.response(ctx.request_id),
            outcome,
            None,
            ResponseFacts::default(),
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    fn outcome(
        &self,
        trace: &Trace,
        key: Option<&ApiKeyRow>,
        identity: Option<&RequestIdentity>,
        account_id: &str,
        upstream_status: u16,
        attempt: u8,
        written: bool,
        client_drift: Option<String>,
        client_version: Option<String>,
        error_code: Option<&str>,
        pin_epoch: Option<u64>,
        head_at: Option<i64>,
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
            client_drift,
            client_version,
            error_code: error_code.map(str::to_string),
            pin_epoch,
            received_at: trace.received_at,
            head_at,
            upstream_request_id: None,
            cache_ttl_ms,
            obstacle: None,
            held: trace.held(),
        }
    }

    /// A head that answers none of the three header families (the api_key buckets, the fast lane's buckets, the
    /// unified windows) carries no sample and changes nothing.
    async fn report_head_sample(
        &self,
        account: &AccountRow,
        head: &ResponseHead,
        canonical_model: &str,
        now: i64,
    ) {
        let observed_at = upstream_date(head).unwrap_or(now);
        let Some(sample) = head_sample(head, account.kind, canonical_model, observed_at) else {
            return;
        };
        self.router
            .report(Report::Sample {
                account_id: account.id.clone(),
                sample,
                incarnation: account.incarnation,
            })
            .await;
    }

    /// Only an OAuth head with the unified headers is evidence; everything else changes nothing.
    async fn report_overage(&self, account: &AccountRow, head: &ResponseHead, now: i64) {
        if account.kind != AccountKind::Oauth {
            return;
        }
        let Some(unified) = UnifiedHeaders::parse(head) else {
            return;
        };
        let in_use = match unified.overage_in_use {
            Some(flag) => flag,
            None if unified.present() => false,
            None => return,
        };
        let observed_at = upstream_date(head).unwrap_or(now);
        // The provider's overage reset when it sends one, else the latest reset among the shared windows that
        // read full: the account stays out under `never` until every full window has reset.
        let full = |u: Option<f64>| u.is_some_and(|u| u >= 1.0);
        let reset = unified.overage_reset.or_else(|| {
            [
                (full(unified.five_hour_utilization), unified.five_hour_reset),
                (full(unified.seven_day_utilization), unified.seven_day_reset),
            ]
            .into_iter()
            .filter(|(f, _)| *f)
            .filter_map(|(_, r)| r)
            .max()
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

    async fn stream<C: AsyncRead + ReturnToPool + Send + Unpin + 'static>(
        &self,
        forward: Forward<'_, C>,
    ) -> Response {
        let Forward {
            affinity,
            caller,
            cancel,
            checkin,
            guards,
            head,
            keep_alive,
            observer,
            on_message_id,
            outcome,
            prefix,
            rate_limit_event,
            reader,
            renewal,
            trace,
        } = forward;
        let (builder, decoder) = match forward_head(head, caller, rate_limit_event) {
            Ok(parts) => parts,
            Err(error) => {
                tracing::warn!(%error, "the upstream response cannot be forwarded");
                return self
                    .answer(
                        trace,
                        UPSTREAM_UNREACHABLE.response(&outcome.request_id),
                        unreachable_outcome(outcome),
                        None,
                        ResponseFacts::default(),
                        None,
                    )
                    .await;
            }
        };
        let event_stream = head
            .get("content-type")
            .is_some_and(|v| v.to_ascii_lowercase().starts_with("text/event-stream"));
        // Without a bus every emit and the completion are no-ops; a hook with no bus still needs the parse run
        // for it, a committed 200 needs it too, since the served-model check reads the answer, and so does a
        // renewal, which carries the usage the parse assembles.
        let publish = (trace.bus_attached()
            || on_message_id.is_some()
            || head.status == 200
            || renewal.is_some()
            || affinity.is_some())
        .then(|| {
            Publish {
                trace: trace.clone(),
                clock: self.clock.clone(),
                outcome: outcome.clone(),
                catalog: self.catalog.clone(),
                decoder: decoder
                    .is_none()
                    .then(|| {
                        encoding(head)
                            .ok()
                            .filter(|e| !matches!(e, Encoding::Identity))
                            .map(phoebis_transport::decode::decoder)
                    })
                    .flatten(),
                decode_failed: false,
                parser: event_stream.then(|| SseParser::new(INSPECT_BUFFER)),
                // A committed JSON answer is inspected for its usage block; a rejection or a stream is not.
                json: (!event_stream && head.status == 200).then(Vec::new),
                json_bound: self.limits.max_request_bytes,
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
                pending_message_id: None,
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
                observer,
                prefix,
                publish,
                reader,
            },
            &self.bodies,
        );
        with_outcome(built(builder, body), outcome)
    }

    /// `renewal` is sent once the answer's usage is read, beside the completion. `guards` are the request's
    /// counts, which a report the full mailbox makes wait holds on its task after the answer.
    #[allow(clippy::too_many_arguments)]
    async fn forward_buffered(
        &self,
        trace: &Trace,
        head: &ResponseHead,
        raw: Bytes,
        caller: &HeaderMap,
        outcome: RelayOutcome,
        renewal: Option<Renewal>,
        affinity: Option<AffinityFacts>,
        guards: Guards,
    ) -> Response {
        // A buffered answer is never an event stream, so its status alone says whether it is a rate limit.
        let (builder, decoder) = match forward_head(head, caller, false) {
            Ok(parts) => parts,
            Err(error) => {
                tracing::warn!(%error, "the upstream response cannot be forwarded");
                return self
                    .answer(
                        trace,
                        UPSTREAM_UNREACHABLE.response(&outcome.request_id),
                        unreachable_outcome(outcome),
                        None,
                        ResponseFacts::default(),
                        None,
                    )
                    .await;
            }
        };
        let payload = match decoder {
            None => raw.clone(),
            Some(mut decoder) => match decode_all(decoder.as_mut(), &raw) {
                Ok(payload) => payload,
                Err(error) => {
                    tracing::warn!(%error, "the upstream response cannot be decoded");
                    return self
                        .answer(
                            trace,
                            UPSTREAM_UNREACHABLE.response(&outcome.request_id),
                            unreachable_outcome(outcome),
                            None,
                            ResponseFacts::default(),
                            None,
                        )
                        .await;
                }
            },
        };
        // The chunk is the decoded body whatever the caller accepts; a coding the caller takes as it is still
        // reaches the bus as text. A body that will not decode reaches it as nothing, because a consumer of a
        // chunk was promised text and coded bytes are worse than none.
        let mut chunk_published = false;
        let answer = match inspectable_copy(head, &raw).map(Bytes::from) {
            None => None,
            Some(text) => {
                trace.publish_chunk(ResponseChunk {
                    request_id: trace.request_id.clone(),
                    attempt: outcome.attempt,
                    index: 0,
                    body: text.clone(),
                    phase: None,
                });
                chunk_published = true;
                // Only the error statuses `respond` drains (400, 401, 403, 404, 429) reach here, a 200 being read
                // by the body task; the 200 arm guards a caller that would buffer one, whose usage still counts.
                (head.status == 200).then(|| answer_from_json(&text, &[]))
            }
        };
        let (usage, facts) = match answer {
            Some(answer) => (Some(answer.usage), answer.response),
            None => (None, ResponseFacts::default()),
        };
        let fast = facts.speed.as_deref() == Some("fast");
        let mut waiting = Waiting::default();
        if let Some(renewal) = renewal {
            renewal.send(
                (self.clock)(),
                head.status,
                usage.as_ref(),
                fast,
                &mut waiting,
            );
        }
        if let Some(affinity) = affinity {
            affinity.send(
                (self.clock)(),
                head.status,
                usage.as_ref(),
                fast,
                &mut waiting,
            );
        }
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
                chunk_published.then_some(1),
                None,
                // An error document carries no message `id`.
                None,
                (self.clock)(),
            )
            .await;
        self.after_answer(waiting, guards);
        with_outcome(response, outcome)
    }
}

/// The outcome of a request the relay answered with its own 502 after an upstream head it could not use: the
/// code names the failure the caller was actually given, not the head that never reached it.
fn unreachable_outcome(outcome: RelayOutcome) -> RelayOutcome {
    RelayOutcome {
        error_code: Some(UPSTREAM_UNREACHABLE.code.to_string()),
        ..outcome
    }
}

/// A response whose head is settled and whose body is still on the wire.
struct Forward<'a, C> {
    /// The conversation report a committed answer sends, beside the renewal; `None` for a rejection.
    affinity: Option<AffinityFacts>,
    caller: &'a HeaderMap,
    cancel: CancellationToken,
    checkin: Checkin,
    /// The response's in-flight counts, which ride the body to its end.
    guards: Guards,
    head: &'a ResponseHead,
    keep_alive: bool,
    /// Set for a committed stream, which is watched for a rate-limit error arriving after the commit.
    observer: Option<PostCommit>,
    on_message_id: Option<Box<dyn FnOnce(String) -> BoxFuture<'static, ()> + Send>>,
    outcome: RelayOutcome,
    prefix: Bytes,
    /// Set where a held 200's first event is a `rate_limit_error`: that answer is the upstream's rate limit as a
    /// 429 is, so it keeps the rate-limit families.
    rate_limit_event: bool,
    reader: BodyReader<C>,
    /// A pinned request's report, sent when the body ends: a committed answer's renewal, or the release of a
    /// pin its own rejection ends.
    renewal: Option<Renewal>,
    trace: &'a Trace,
}

/// Why a selected account produced nothing to dispatch with. Neither answer reaches the caller: the account
/// is already in `attempted`, so both send the loop back to select without a wire submission.
enum NoCredential {
    /// The account row moved between the selection and the acquisition.
    Stale,
    /// The manager or the store has no credential to hand out.
    Unavailable,
}

/// What one attempt's response half reads about the request it belongs to.
struct Responding<'a> {
    account: &'a AccountRow,
    attempt: u8,
    /// Whether this pass marked the account `reauth_required`, carried back to the attempt loop through
    /// `Next::FailOver` and read by a probe's own answer.
    auth_marked: bool,
    /// The TTL this pass's body was coupled to, for the outcome the response half builds; `None` off the policy.
    cache_ttl_ms: Option<i64>,
    caller: &'a HeaderMap,
    /// Whether this leg has not retried in place yet: one retry per leg.
    can_retry_in_place: bool,
    /// Whether this leg has not stripped yet. A strip retry is the router correcting its own edit on the same
    /// account, never a failover; the one-round rule is the only gate.
    can_strip: bool,
    cancel: &'a CancellationToken,
    /// The two boundary chains the client's body hashed, for the affinity report; `None` for a body with no
    /// chain.
    chains: Option<&'a Chains>,
    client_version: Option<String>,
    credential_generation: u64,
    /// `Admitted::deadline`, which ends the refresh ladder's wait.
    deadline: Option<i64>,
    fallback_ms: i64,
    /// The request carried `speed: "fast"`, which a throttle's classification reads.
    fast: bool,
    /// `Admitted::floor_held`, which the refresh ladder reads and sets.
    floor_held: &'a AtomicBool,
    /// Whether this selection created `pin` rather than renewing one that already served.
    fresh_pin: bool,
    identity: &'a RequestIdentity,
    /// The attempt's own additions, body and head, that a 400 might name.
    injected: &'a [Injected],
    key: &'a ApiKeyRow,
    /// The request's permit, which a committed stream takes into its body.
    key_permit: &'a mut Option<KeyPermit>,
    /// The cache lane the request writes to, for the affinity report.
    lane: &'a str,
    markers_ttl_ms: Option<i64>,
    memory_key: Option<&'a MemoryKey>,
    pin: Option<&'a PinRef>,
    pin_epoch: Option<u64>,
    portable: bool,
    replayed: bool,
    request_id: &'a str,
    retry_in_place: Duration,
    /// This pass's account ticket, which a committed stream takes into its body.
    ticket: Option<AccountTicket>,
    /// `tier_ttls` of the body as this pass sends it upstream, after the TTL rewrite.
    tier_ttls: (Option<i64>, Option<i64>),
    trace: &'a Trace,
    /// `uniform_ttl` of the same body.
    uniform_ttl_ms: Option<i64>,
}

impl Responding<'_> {
    /// Both counts for a body that outlives the response half, so they are released when it ends.
    fn guards(&mut self) -> Guards {
        Guards {
            _key: self.key_permit.take(),
            _account: self.ticket.take(),
        }
    }
}

/// The in-flight guards a committed stream carries to its end; dropped with the body, whichever way it ends.
#[derive(Default)]
pub(crate) struct Guards {
    _key: Option<KeyPermit>,
    _account: Option<AccountTicket>,
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

/// What a local refusal answers and files: the failure the caller is told, and the client-drift trigger the
/// request row carries and the attention reader raises its condition from, `None` for every other refusal.
struct LocalRefusal {
    failure: Failure,
    client_drift: Option<&'static str>,
}

impl From<Failure> for LocalRefusal {
    fn from(failure: Failure) -> Self {
        LocalRefusal {
            failure,
            client_drift: None,
        }
    }
}

/// The restoration facts computed once at admission: the client version the wire already named, the
/// version-suffix code units, and the prompt id this turn carries. The previous request id is not here because
/// it is per-account, looked up in the failover loop against the account each attempt actually tries.
struct RestoreContext {
    cli_version: String,
    prompt_units: [u16; 3],
    prompt_id: Option<String>,
}

/// What the same-account leg reads about the request it is part of: fixed at admission and shared by every
/// account the request tries.
struct Admitted<'a> {
    /// The key's override else the pool's, read once at admission.
    auto_mode_server: bool,
    /// The pool's own policy, off by default; read once at admission since a migrated attempt still prices the
    /// same request against the same policy, only the account's overage state changes per destination.
    cache_ttl_policy: CacheTtlPolicy,
    caller: &'a HeaderMap,
    /// The two boundary chains the client's body hashed, for the affinity report; `None` for a body with no
    /// chain.
    chains: Option<&'a Chains>,
    /// The client sent its body gzip, which `upstream_gzip: auto` mirrors toward the upstream.
    client_gzip: bool,
    client_version: Option<String>,
    /// The relay's clock past which the attempt loop answers the exhaustion rather than selecting again, and at
    /// which a wait on an account's backoff ends: admission plus `head_timeout` less `ATTEMPT_DEADLINE_MARGIN`.
    /// `None` when `head_timeout` is no longer than the margin.
    deadline: Option<i64>,
    /// The pool's `cache_diagnostics` switch, resolved at admission and never for a `count_tokens` request: the
    /// public schema refuses an unknown top-level member and the beta names nothing there, and a 400 on that path
    /// must never latch the account for its inference requests.
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
    /// The largest TTL of the client's own cache markers, the renewal's TTL when no rewrite applied.
    markers_ttl_ms: Option<i64>,
    /// The session memory key this request reads under, absent when the request carries no session; a probe
    /// reads under its session's key and the response half records nothing for it.
    memory_key: Option<MemoryKey>,
    /// The caller's head as it arrived, absent for an in-process caller and for a connection the recorder
    /// gave up on.
    original: Option<&'a OriginalHead>,
    portable: bool,
    raw: &'a Bytes,
    request_id: &'a str,
    /// The first-party fields to restore, absent for anything but a `claude-cli/` inference request.
    restore: Option<RestoreContext>,
    /// The pool's `retry_in_place_seconds`: the longest throttle waited out on the same account, and the
    /// longest a held selection waits for a slot.
    retry_in_place: Duration,
    target: &'a str,
    trace: &'a Trace,
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

/// `generations` is the attempt's (incarnation, lifecycle, credential) triple, which fences the mark: a
/// verdict about a credential that was replaced, or a holder the id lost, while the request was in flight
/// lands nowhere. Awaited where it stands: between legs no client byte has gone out yet.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn report_failover(
    router: &RouterHandle,
    account_id: &str,
    canonical_model: &str,
    cooldown: Option<Cooldown>,
    mark: Option<AccountState>,
    pin: Option<PinRef>,
    request_id: &str,
    generations: (u64, u64, u64),
    set_at: i64,
) {
    for report in failover_reports(
        account_id,
        canonical_model,
        cooldown,
        mark,
        pin,
        request_id,
        generations,
        set_at,
    ) {
        router.report(report).await;
    }
}

/// What a failover verdict tells the router, in order: the cooldown, or without one the end of the pin, then the
/// mark.
#[allow(clippy::too_many_arguments)]
pub(crate) fn failover_reports(
    account_id: &str,
    canonical_model: &str,
    cooldown: Option<Cooldown>,
    mark: Option<AccountState>,
    pin: Option<PinRef>,
    request_id: &str,
    generations: (u64, u64, u64),
    set_at: i64,
) -> Vec<Report> {
    let mut reports = Vec::new();
    if let Some(cooldown) = cooldown {
        let scope = match cooldown.scope {
            Scope::Account => "account".to_string(),
            Scope::Fast => "fast".to_string(),
            Scope::FastCredits => "fast_credits".to_string(),
            Scope::Model => format!("model:{canonical_model}"),
        };
        let row = CooldownRow {
            account_id: account_id.to_string(),
            scope,
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
            incarnation: generations.0,
            lifecycle_generation: generations.1,
            credential_generation: generations.2,
        });
    }
    if let Some(state) = mark {
        reports.push(Report::AccountState {
            account_id: account_id.to_string(),
            state,
            incarnation: generations.0,
            lifecycle_generation: generations.1,
            credential_generation: generations.2,
        });
    }
    reports
}

/// The sink reports the room left in the call it refused; the caller's own bound is the number worth telling.
fn at_buffered_bound(error: TransportError) -> TransportError {
    match error {
        TransportError::TooLarge(_) => TransportError::TooLarge(MAX_BUFFERED_BODY),
        other => other,
    }
}

/// Decoded in pieces under the same bound as the inspection copy, so a small compressed rejection cannot
/// expand without limit in memory.
fn decode_all(decoder: &mut dyn Decoder, raw: &[u8]) -> Result<Bytes, TransportError> {
    let mut out = Vec::new();
    for piece in raw.chunks(DECODE_PIECE) {
        out.extend(
            decoder
                .feed_bounded(piece, MAX_BUFFERED_BODY - out.len())
                .map_err(at_buffered_bound)?,
        );
    }
    out.extend(
        decoder
            .finish_bounded(MAX_BUFFERED_BODY - out.len())
            .map_err(at_buffered_bound)?,
    );
    Ok(Bytes::from(out))
}

/// Builds the caller's response head from the upstream's and decides whether the payload can be passed through
/// as it came. Returns the decoder to run over the body when it cannot. `rate_limit_event` marks a 200 whose
/// first event is a `rate_limit_error`, which keeps the rate-limit families as a 429 does.
fn forward_head(
    head: &ResponseHead,
    caller: &HeaderMap,
    rate_limit_event: bool,
) -> Result<(Builder, Option<Box<dyn Decoder>>), TransportError> {
    let upstream = encoding(head)?;
    let coding = head.get("content-encoding").unwrap_or("identity");
    let passthrough = matches!(upstream, Encoding::Identity) || caller_accepts(caller, coding);
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(head.status).unwrap_or(StatusCode::BAD_GATEWAY));
    // The relay's own read of the families (`ratelimit.rs`) already ran on the upstream head; hiding them
    // from the caller changes nothing it saw.
    let hide_rate_limits = head.status != 429 && !rate_limit_event;
    for (name, value) in &head.headers {
        if NOT_FORWARDED.iter().any(|n| name.eq_ignore_ascii_case(n)) {
            continue;
        }
        let lower = name.to_ascii_lowercase();
        if hide_rate_limits
            && RATE_LIMIT_FAMILY_PREFIXES
                .iter()
                .any(|p| lower.starts_with(p))
        {
            continue;
        }
        if !passthrough && name.eq_ignore_ascii_case("content-encoding") {
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
    Ok((builder, (!passthrough).then(|| decoder(upstream))))
}

fn built(builder: Builder, body: Body) -> Response {
    builder
        .body(body)
        // The status is a validated `StatusCode` and every header was parsed before it was added.
        .expect("a forwarded response head is always well formed")
}

/// Whether the caller said it can read this content coding. An explicit entry for the coding decides, the last
/// one written winning, and only when none names it does a wildcard speak; every `Accept-Encoding` line is read,
/// since a proxy may add one beside the client's. `q=0` on the deciding entry is a refusal.
fn caller_accepts(caller: &HeaderMap, coding: &str) -> bool {
    let mut explicit = None;
    let mut wildcard = None;
    for line in caller
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|v| v.to_str().ok())
    {
        for entry in line.split(',') {
            let (name, params) = entry.split_once(';').unwrap_or((entry, ""));
            let refused = params
                .split(';')
                .filter_map(|p| p.trim().strip_prefix("q="))
                .any(|w| w.trim().parse::<f32>().is_ok_and(|w| w <= 0.0));
            let accepted = !refused;
            let name = name.trim();
            if name.eq_ignore_ascii_case(coding) {
                explicit = Some(accepted);
            } else if name == "*" {
                wildcard = Some(accepted);
            }
        }
    }
    explicit.or(wildcard).unwrap_or(false)
}

/// The caller's own `User-Agent`, the only handle an operator has on which client drifted.
fn user_agent(headers: &HeaderMap) -> &str {
    headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
}

/// Bytes fed to a decoder at a time, so the bound below is checked as the copy grows rather than after a
/// small compressed body has already expanded into memory.
const DECODE_PIECE: usize = 4096;

/// The decoded copy the classification table reads. A compressed body is decoded because the table reads JSON,
/// and the copy is inspection only: what the caller receives is the upstream's own bytes under the upstream's
/// own headers. `None` means the payload does not decode or does not fit.
fn inspectable_copy(head: &ResponseHead, raw: &[u8]) -> Option<Vec<u8>> {
    match encoding(head) {
        Ok(Encoding::Identity) => Some(raw.to_vec()),
        Ok(coding) => inspectable(decoder(coding).as_mut(), raw),
        Err(_) => None,
    }
}

fn inspectable(decoder: &mut dyn Decoder, raw: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for piece in raw.chunks(DECODE_PIECE) {
        out.extend(
            decoder
                .feed_bounded(piece, MAX_BUFFERED_BODY - out.len())
                .ok()?,
        );
    }
    out.extend(decoder.finish_bounded(MAX_BUFFERED_BODY - out.len()).ok()?);
    Some(out)
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
    fn a_refused_coding_is_not_passed_through() {
        let mut caller = HeaderMap::new();
        caller.insert(header::ACCEPT_ENCODING, "gzip;q=0, br".parse().unwrap());
        assert!(!caller_accepts(&caller, "gzip"));
        assert!(caller_accepts(&caller, "br"));
        assert!(!caller_accepts(&HeaderMap::new(), "gzip"));
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
}
