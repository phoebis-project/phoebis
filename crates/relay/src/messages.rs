//! The Messages routes: admission against the registry, the account the router selects, the failover loop
//! across accounts, and the response the caller gets.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::response::Builder;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::{get, head, post};
use axum::Router;
use bytes::Bytes;
use phoebis_bus::{
    AccountSelected, AttemptOutcome, Bus, CooldownSet, Event, Kind, Origin, Outcome,
    RequestComplete, RequestObserved, ResponseChunk, ResponseHead as BusHead, Usage,
};
use phoebis_oauth::{AccessToken, Clock, CredentialManager};
use phoebis_routing::{
    resolve, EvidenceSource, Exhaustion, OverageEvidence, PinRef, Report, ResolveError,
    RouterHandle, SelectRequest, SelectResult, Selection,
};
use phoebis_store::{AccountKind, AccountRow, AccountState, ApiKeyRow, CooldownRow, Store};
use phoebis_transport::control::Resolver;
use phoebis_transport::decode::{decoder, encoding, Decoder, Encoding};
use phoebis_transport::pool::{Pool, PoolKey};
use phoebis_transport::request::RawRequest;
use phoebis_transport::response::{framing, keep_alive, BodyReader, ResponseHead};
use phoebis_transport::TransportError;
use tokio::io::AsyncRead;
use tokio_util::sync::CancellationToken;

use crate::casing::OriginalHead;
use crate::classify::{classify, Cooldown, HeadContext, Scope, Verdict};
use crate::dispatch::{self, until_cancelled, AttemptFailure, Sent};
use crate::errors::{
    auth_failure, key_refusal, relay_failure, Failure, ACCOUNT_CANCELLED, METHOD_NOT_ALLOWED,
    MODEL_NOT_ALLOWED, MODEL_UNSERVED, NOT_FOUND, ROUTER_UNAVAILABLE, UPSTREAM_UNREACHABLE,
};
use crate::exhaustion::exhaustion_response;
use crate::headers::{outbound_headers, CredentialKind};
use crate::hold::{hold, Commit, Held, Inspect, INSPECT_BUFFER};
use crate::identity::{identify, RequestIdentity};
use crate::observe::{published_headers, usage_from_json, wants_stream};
use crate::ratelimit::{upstream_date, UnifiedHeaders};
use crate::sse::SseParser;
use crate::stream::{
    drain_bounded, relay_body, Checkin, Drained, PostCommit, Publish, Relayed, ReturnToPool,
    MAX_BUFFERED_BODY,
};
use crate::{edit_body, is_portable, presented_key, DriftGuard, RelayError};

/// Upstream response headers the relay keeps to itself: they describe the upstream connection rather than the
/// payload, and the framing ones no longer match the body the caller is sent.
const NOT_FORWARDED: &[&str] = &[
    "connection",
    "content-length",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// The longest a request's bodies may be kept, whatever a key or pool was left holding: an operator who
    /// lowers the global maximum means it from the next request, not from the next write of every stored row.
    pub body_capture_max_hours: u32,
    /// How long an account is cooled when the upstream rejected it without saying for how long.
    pub cooldown_fallback: Duration,
    pub head_timeout: Duration,
    pub hold_timeout: Duration,
    pub idle_timeout: Duration,
    /// Upstream submissions one caller request may cost across every account it tries.
    pub max_attempts: u8,
    pub max_request_bytes: usize,
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
    /// `hash` when the caller stamped a billing hash this build cannot reproduce, in which case nothing was
    /// sent upstream. Every client version is relayed, so a version is never drift.
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
}

/// Marks a request the keepalive engine sent on a session's behalf, and names the key it runs under. The
/// engine never holds a key's secret (the store keeps a digest), so a probe authenticates by id; the extension
/// can only be set in process, so the by-id path is not reachable from the socket. `allow_oauth` is the
/// operator's statement that the expiry-crossing experiment passed; without it an `oauth` account serves no
/// probe on any attempt, even one a registry write re-routed mid-request (`probe_eligible` holds the rule
/// against the account actually selected, not just the list admission started with).
#[derive(Clone, Debug)]
pub struct Probe {
    pub key_id: String,
    pub allow_oauth: bool,
}

pub struct Relay {
    bus: Option<Bus>,
    pub(crate) clock: Clock,
    drift: DriftGuard,
    pub(crate) limits: Limits,
    manager: Arc<CredentialManager>,
    pub(crate) pool: Arc<Pool>,
    resolver: Resolver,
    router: RouterHandle,
    store: Arc<Store>,
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
    ) -> Relay {
        Relay {
            bus: None,
            clock,
            drift: DriftGuard,
            limits,
            manager,
            pool,
            resolver,
            router,
            store,
        }
    }

    /// Attaches the bus every relayed request is published to. Without one the relay publishes nothing.
    pub fn with_bus(self, bus: Bus) -> Relay {
        Relay {
            bus: Some(bus),
            ..self
        }
    }

    /// A keepalive probe: the recipe's headers and body, run through the same admission, selection, edit, and
    /// failover loop a real request takes, so the pin follows, renews, and migrates as it would for one.
    pub async fn probe(&self, probe: Probe, mut request: Request) -> Response {
        request.extensions_mut().insert(probe);
        self.handle(request).await
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
}

impl Trace {
    pub(crate) fn bus_attached(&self) -> bool {
        self.bus.is_some()
    }

    fn publish(&self, event: Event) {
        if let Some(bus) = &self.bus {
            bus.publish(event);
        }
    }

    pub(crate) fn publish_chunk(&self, chunk: ResponseChunk) {
        self.publish(Event::ResponseChunk(chunk));
    }

    /// Published exactly once per request by the handler, as soon as it knows as much of the identity as the
    /// request will ever yield. A refusal before the key was read publishes with nothing but the id.
    fn observed(
        &self,
        key: Option<&ApiKeyRow>,
        identity: Option<&RequestIdentity>,
        headers: &HeaderMap,
        body: Bytes,
        stream: bool,
        body_capture_hours: u32,
    ) {
        if self.bus.is_none() {
            return;
        }
        self.publish(Event::RequestObserved(RequestObserved {
            request_id: self.request_id.clone(),
            origin: self.origin,
            kind: self.kind,
            key_id: key.map(|k| k.id.clone()),
            pool_id: key.map(|k| k.pool_id.clone()),
            session_id: identity.and_then(|i| i.session_id.clone()),
            agent_id: identity.map(|i| i.agent_id.clone()),
            parent_agent_id: identity.and_then(|i| i.parent_agent_id.clone()),
            owner_id: identity.and_then(|i| i.owner_id.clone()),
            model: identity.map(|i| i.model.clone()).unwrap_or_default(),
            canonical_model: identity
                .map(|i| i.canonical_model.clone())
                .unwrap_or_default(),
            stream,
            headers: published_headers(headers),
            body,
            body_capture_hours,
            received_at: self.received_at,
        }));
    }

    fn selected(
        &self,
        attempt: u8,
        account: &AccountRow,
        pin_epoch: Option<u64>,
        started_at: i64,
    ) {
        self.publish(Event::AccountSelected(AccountSelected {
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
        }));
    }

    #[allow(clippy::too_many_arguments)]
    fn head(
        &self,
        attempt: u8,
        head: &ResponseHead,
        first_event: Option<Bytes>,
        outcome: AttemptOutcome,
        cooldown: Option<CooldownSet>,
        written_at: i64,
        observed_at: i64,
    ) {
        self.publish(Event::ResponseHead(BusHead {
            request_id: self.request_id.clone(),
            attempt,
            status: head.status,
            headers: head.headers.clone(),
            first_event,
            outcome,
            cooldown,
            written_at,
            observed_at,
        }));
    }

    /// The last event of a request. `status` is what the client was answered with, which is the upstream's
    /// only for a forwarded answer; `end` overrides the outcome derived from the `RelayOutcome` for a body that
    /// ended other than cleanly, and `error` names the cause when the override has one.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn complete(
        &self,
        outcome: &RelayOutcome,
        status: u16,
        usage: Option<Usage>,
        ttfc_ms: Option<i64>,
        response_bytes: Option<u64>,
        end: Option<(Outcome, Option<String>)>,
        finished_at: i64,
    ) {
        let (final_outcome, error_code) = match end {
            Some((o, code)) => (o, code.or_else(|| outcome.error_code.clone())),
            None => (outcome_of(outcome), outcome.error_code.clone()),
        };
        self.publish(Event::RequestComplete(RequestComplete {
            request_id: self.request_id.clone(),
            attempt: outcome.attempt,
            account_id: (!outcome.account_id.is_empty()).then(|| outcome.account_id.clone()),
            outcome: final_outcome,
            status,
            usage: usage.filter(|u| !u.is_empty()),
            ttfb_ms: outcome.head_at.map(|at| at - outcome.received_at),
            ttfc_ms,
            error_code,
            client_drift: outcome.client_drift.clone(),
            client_version: outcome.client_version.clone(),
            response_bytes,
            finished_at,
        }));
    }
}

/// The request outcome a local answer implies; a body that ends badly overrides it at the end of the body.
fn outcome_of(outcome: &RelayOutcome) -> Outcome {
    match outcome.error_code.as_deref() {
        Some("account_cancelled") => Outcome::Canceled,
        Some("accounts_exhausted") | Some("accounts_unavailable") => Outcome::Exhausted,
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
        Verdict::FailOver { .. } if status == 401 => AttemptOutcome::AuthFailed,
        Verdict::FailOver { .. } => AttemptOutcome::UpstreamError,
    }
}

fn cooldown_set(cooldown: Option<&Cooldown>, canonical_model: &str) -> Option<CooldownSet> {
    cooldown.map(|c| CooldownSet {
        scope: match c.scope {
            Scope::Account => "account".into(),
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
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(messages))
        // Model discovery is not part of the relayed surface: answering it would invite a client to pick a
        // model from an account's entitlements rather than from its own configuration.
        .route("/v1/models", get(unknown_route))
        .route("/v1/models/{id}", get(unknown_route))
        .method_not_allowed_fallback(wrong_method)
        .fallback(unknown_route)
        .with_state(relay)
}

/// The native client probes this before it sends anything and ignores the answer.
async fn hello() -> StatusCode {
    StatusCode::OK
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

/// Whether an account may serve a probe: `api_key` always, and `oauth` only when the operator's `allow_oauth`
/// cleared it. Checked before the failover loop against admission's candidate list, and again after every
/// selection, because a registry write landing between admission and a select bumps the revision and the
/// router recomputes candidates from its own snapshot, which does not carry this restriction.
fn probe_eligible(account: &AccountRow, allow_oauth: bool) -> bool {
    account.kind == AccountKind::ApiKey || allow_oauth
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
        };
        let (parts, body) = request.into_parts();
        let original = parts.extensions.get::<OriginalHead>();
        // The snapshot is pinned for the whole request: the key, the pool, and the candidates all come from one
        // registry, and a write that lands mid-request is seen by the next request.
        let snapshot = self.router.snapshot();
        let key = match &probe {
            // Before the body, as for a caller: an unauthenticated probe never holds the request limit either.
            Some(probe) => match snapshot.authenticate_id(&probe.key_id, received_at) {
                Ok(key) => key.clone(),
                Err(refusal) => {
                    tracing::debug!(%refusal, key = %probe.key_id, "a probe's key was refused");
                    trace.observed(None, None, &parts.headers, Bytes::new(), false, 0);
                    return self.refused(&trace, key_refusal(&refusal), None, None, None);
                }
            },
            None => {
                let presented = match presented_key(&parts.headers) {
                    Ok(presented) => presented,
                    Err(error) => {
                        trace.observed(None, None, &parts.headers, Bytes::new(), false, 0);
                        return self.refused(&trace, auth_failure(&error), None, None, None);
                    }
                };
                match snapshot.authenticate(presented, received_at) {
                    Ok(key) => key.clone(),
                    Err(refusal) => {
                        tracing::debug!(%refusal, "a presented key was refused");
                        trace.observed(None, None, &parts.headers, Bytes::new(), false, 0);
                        return self.refused(&trace, key_refusal(&refusal), None, None, None);
                    }
                }
            }
        };
        self.router.touch_key(&key.id);
        let capture_hours = snapshot
            .body_capture_hours(&key)
            .min(self.limits.body_capture_max_hours);
        let client_version = self.drift.check(&parts.headers);
        let Ok(raw) = axum::body::to_bytes(body, self.limits.max_request_bytes).await else {
            // Either the caller went past the limit or its own connection broke mid-body; either way there is
            // no complete request to relay.
            trace.observed(
                Some(&key),
                None,
                &parts.headers,
                Bytes::new(),
                false,
                capture_hours,
            );
            return self.refused(
                &trace,
                relay_failure(&RelayError::BodyTooLarge),
                Some(&key),
                None,
                client_version.as_deref(),
            );
        };
        let stream = wants_stream(&raw);
        let identity = match identify(&parts.headers, &raw) {
            Ok(identity) => identity,
            Err(error) => {
                trace.observed(
                    Some(&key),
                    None,
                    &parts.headers,
                    raw.clone(),
                    stream,
                    capture_hours,
                );
                return self.refused(
                    &trace,
                    relay_failure(&error),
                    Some(&key),
                    None,
                    client_version.as_deref(),
                );
            }
        };
        trace.observed(
            Some(&key),
            Some(&identity),
            &parts.headers,
            raw.clone(),
            stream,
            capture_hours,
        );
        let candidates = match resolve(&snapshot, &key, &identity.canonical_model) {
            Ok(candidates) => candidates,
            Err(ResolveError::ModelNotAllowed) => {
                return self.refused(
                    &trace,
                    MODEL_NOT_ALLOWED,
                    Some(&key),
                    Some(&identity),
                    client_version.as_deref(),
                )
            }
            Err(ResolveError::ModelUnserved) => {
                return self.refused(
                    &trace,
                    MODEL_UNSERVED,
                    Some(&key),
                    Some(&identity),
                    client_version.as_deref(),
                )
            }
        };
        // Narrows what a probe starts the loop with; a registry write can move the revision before any
        // attempt, and then the router recomputes candidates from its own snapshot without this filter, so
        // `probe_eligible` runs again on every selection below to hold the same rule against the chosen row.
        let candidates = match &probe {
            None => candidates,
            Some(probe) => candidates
                .into_iter()
                .filter(|c| {
                    snapshot
                        .account(&c.account_id)
                        .is_some_and(|account| probe_eligible(account, probe.allow_oauth))
                })
                .collect(),
        };
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
        let admitted = Admitted {
            caller: &parts.headers,
            client_version,
            fallback_ms,
            identity: &identity,
            key: &key,
            original,
            portable,
            raw: &raw,
            request_id: &trace.request_id,
            target: &target,
            trace: &trace,
        };
        let mut attempted: Vec<String> = Vec::new();
        let mut attempt: u8 = 0;

        loop {
            // A backstop: every failover point already checks the budget before it comes back here, and
            // `attempted` grows each pass, so the loop is bounded by the pool even without this.
            if attempt >= self.limits.max_attempts {
                return self.refused(
                    &trace,
                    UPSTREAM_UNREACHABLE,
                    Some(&key),
                    Some(&identity),
                    admitted.client_version.as_deref(),
                );
            }
            let now = (self.clock)();
            let answer = match self
                .router
                .select(SelectRequest {
                    key_id: key.id.clone(),
                    session_id: identity.session_id.clone(),
                    canonical_model: identity.canonical_model.clone(),
                    candidates: candidates.clone(),
                    portable,
                    attempted: attempted.clone(),
                    now,
                    fallback_ms,
                    revision: snapshot.revision,
                })
                .await
            {
                Ok(answer) => answer,
                Err(_) => {
                    return self.refused(
                        &trace,
                        ROUTER_UNAVAILABLE,
                        Some(&key),
                        Some(&identity),
                        admitted.client_version.as_deref(),
                    )
                }
            };
            let selection = match answer.result {
                SelectResult::Selected(selection) => selection,
                // The registry moved after admission and no longer admits this request; the answer is the one the
                // current registry would have given at admission.
                SelectResult::Refused(ResolveError::ModelNotAllowed) => {
                    return self.refused(
                        &trace,
                        MODEL_NOT_ALLOWED,
                        Some(&key),
                        Some(&identity),
                        admitted.client_version.as_deref(),
                    )
                }
                SelectResult::Refused(ResolveError::ModelUnserved) => {
                    return self.refused(
                        &trace,
                        MODEL_UNSERVED,
                        Some(&key),
                        Some(&identity),
                        admitted.client_version.as_deref(),
                    )
                }
                SelectResult::Exhausted(exhaustion) => {
                    let code = match exhaustion {
                        Exhaustion::Timed { .. } => "accounts_exhausted",
                        Exhaustion::Untimed => "accounts_unavailable",
                    };
                    let outcome = self.outcome(
                        &trace,
                        Some(&key),
                        Some(&identity),
                        "",
                        0,
                        attempt,
                        false,
                        None,
                        admitted.client_version.clone(),
                        Some(code),
                        None,
                        None,
                    );
                    return self.answer(
                        &trace,
                        exhaustion_response(&exhaustion, now, &trace.request_id),
                        outcome,
                        None,
                        None,
                    );
                }
            };
            attempted.push(selection.account.id.clone());
            // A reload between admission and this select recomputes candidates from the current registry, past
            // the filter above: re-checked here against the row the router actually chose, so a probe still
            // never dispatches to an account the operator has not cleared for it.
            if let Some(probe) = &probe {
                if !probe_eligible(&selection.account, probe.allow_oauth) {
                    continue;
                }
            }
            match self
                .dispatch_to(&admitted, &selection, &answer.cancel, &mut attempt)
                .await
            {
                Leg::FailOver => continue,
                Leg::Respond(response) => return response,
            }
        }
    }

    /// One account's leg of the request: the credential, the body edit, the dispatch with its same-account
    /// retry and its 401 replay, and the response half. Every early exit here is a `Leg`; the account is
    /// already in the loop's `attempted`, so a `FailOver` costs nothing but the next selection.
    async fn dispatch_to(
        &self,
        admitted: &Admitted<'_>,
        selection: &Selection,
        cancel: &CancellationToken,
        attempt: &mut u8,
    ) -> Leg {
        // The row the router selected, not one looked up again in the admission snapshot: the endpoint a
        // request goes out to and the credential it carries have to be the pair one snapshot held.
        let account = &selection.account;
        let pin_epoch = selection.pin.as_ref().map(|pin| pin.epoch);
        // No wire submission, so no attempt is spent: the account is already in `attempted`, and the next
        // selection skips it. The outer loop is bounded by the pool size, because each account is selected
        // at most once per request.
        let mut credential = match self.credential(account).await {
            Ok(credential) => credential,
            Err(_) => return Leg::FailOver,
        };
        let mut edited = admitted.raw.to_vec();
        let edit = {
            edit_body(
                &mut edited,
                // The public `count_tokens` schema carries no `metadata` member and the upstream refuses
                // unknown top-level ones, so there is no slot the account identity could go in. It is not
                // billed either, so nothing is lost by leaving the body without it.
                match admitted.trace.kind {
                    Kind::CountTokens => None,
                    Kind::Inference | Kind::Probe => credential.account_uuid.as_deref(),
                },
                &selection.upstream_model,
            )
        };
        if let Err(error) = edit {
            if let RelayError::ClientDrift { kind } = &error {
                // Drift depends on the body alone, so it is found on the first account before any wire
                // submission: nothing is sent until the router is re-pinned, and the outcome carries the tag.
                tracing::warn!(
                    account = %account.id,
                    drift = %kind,
                    user_agent = %user_agent(admitted.caller),
                    "a caller stamped a billing hash this build cannot reproduce; re-pin the router to that client",
                );
                let outcome = self.outcome(
                    admitted.trace,
                    Some(admitted.key),
                    Some(admitted.identity),
                    &account.id,
                    0,
                    0,
                    false,
                    Some(kind.to_string()),
                    admitted.client_version.clone(),
                    Some("client_drift"),
                    None,
                    None,
                );
                return Leg::Respond(self.answer(
                    admitted.trace,
                    relay_failure(&error).response(admitted.request_id),
                    outcome,
                    None,
                    None,
                ));
            }
            return Leg::Respond(self.refused(
                admitted.trace,
                relay_failure(&error),
                Some(admitted.key),
                Some(admitted.identity),
                admitted.client_version.as_deref(),
            ));
        }
        let edited = Bytes::from(edited);
        let authority = "api.anthropic.com".to_string();
        let Some(addr) = (self.resolver)(&authority) else {
            tracing::warn!(host = %authority, "the upstream host does not resolve");
            return Leg::FailOver;
        };
        let pool_key = PoolKey {
            account_id: account.id.clone(),
            host: authority.to_string(),
        };
        let mut fresh = false;
        let mut retried = false;
        let mut replayed = false;

        loop {
            *attempt += 1;
            admitted.trace.selected(
                *attempt,
                account,
                pin_epoch,
                (self.clock)(),
            );
            let responding = Responding {
                account,
                attempt: *attempt,
                caller: admitted.caller,
                cancel,
                client_version: admitted.client_version.clone(),
                credential_generation: credential.generation,
                fallback_ms: admitted.fallback_ms,
                identity: admitted.identity,
                key: admitted.key,
                pin: selection.pin.as_ref(),
                pin_epoch,
                portable: admitted.portable,
                replayed,
                request_id: admitted.request_id,
                trace: admitted.trace,
            };
            let dispatched = {
                let request = RawRequest {
                    method: "POST",
                    target: admitted.target.to_string(),
                    headers: outbound_headers(
                        admitted.caller,
                        admitted.original,
                        &credential.kind,
                        &authority,
                        edited.len(),
                    ),
                    body: edited.clone(),
                };
                match dispatch::attempt(
                    self,
                    &request,
                    &pool_key,
                    &authority,
                    addr,
                    fresh,
                    cancel,
                )
                .await
                {
                    Err(failure) => Err(failure),
                    Ok(sent) => Ok(self
                        .respond(
                            responding,
                            sent,
                            Checkin::Measured(self.pool.clone(), pool_key.clone()),
                        )
                        .await),
                }
            };
            let next = match dispatched {
                Ok(next) => next,
                Err(AttemptFailure::Unsent(detail))
                    if !retried && *attempt < self.limits.max_attempts =>
                {
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
                    );
                    return Leg::Respond(self.answer(
                        admitted.trace,
                        ACCOUNT_CANCELLED.response(admitted.request_id),
                        outcome,
                        None,
                        None,
                    ));
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
                    );
                    return Leg::Respond(self.answer(
                        admitted.trace,
                        UPSTREAM_UNREACHABLE.response(admitted.request_id),
                        outcome,
                        None,
                        None,
                    ));
                }
            };
            match next {
                Next::Replay(refreshed) => {
                    credential = refreshed;
                    replayed = true;
                    fresh = false;
                }
                Next::FailOver => return Leg::FailOver,
                Next::Respond(response) => return Leg::Respond(response),
            }
        }
    }

    /// The half of an attempt that runs once the head is in: the drain, the pre-commit hold, the verdict, and
    /// the response or the decision to move on.
    async fn respond<C: AsyncRead + ReturnToPool + Send + Unpin + 'static>(
        &self,
        ctx: Responding<'_>,
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
        );
        outcome.upstream_request_id = head.get("request-id").map(str::to_string);
        self.report_overage(ctx.account, &head, head_at);

        // Heads the table reads a body for are drained within the bound; a body too large to read
        // commits. The reader survives only where the body was left alone, because a drained body has
        // already handed its connection back to the pool.
        let (buffered, mut reader) = if matches!(head.status, 400 | 401 | 403 | 429) {
            let Some(drained) = until_cancelled(
                drain_bounded(&mut reader, self.limits.idle_timeout),
                ctx.cancel,
            )
            .await
            else {
                return Next::Respond(self.cancelled(&ctx, &head, written_at));
            };
            match drained {
                Ok(Drained::Complete(buffered)) => {
                    if reusable {
                        reader.into_inner().return_to(checkin.clone());
                    }
                    (Some(buffered), None)
                }
                Ok(Drained::TooLarge(prefix)) => {
                    tracing::debug!(account = %ctx.account.id, "forwarding a rejection too large to inspect");
                    return Next::Respond(self.stream(Forward {
                        caller: ctx.caller,
                        cancel: ctx.cancel.clone(),
                        checkin,
                        head: &head,
                        keep_alive: reusable,
                        observer: None,
                        on_message_id: None,
                        outcome,
                        prefix,
                        reader,
                        trace: ctx.trace,
                    }));
                }
                Err(error) => {
                    tracing::warn!(account = %ctx.account.id, %error, "the rejection body was not readable");
                    return Next::Respond(self.answer(
                        ctx.trace,
                        UPSTREAM_UNREACHABLE.response(ctx.request_id),
                        unreachable_outcome(outcome),
                        None,
                        None,
                    ));
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
                return Next::Respond(self.cancelled(&ctx, &head, written_at));
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
                    return Next::Respond(self.answer(
                        ctx.trace,
                        UPSTREAM_UNREACHABLE.response(ctx.request_id),
                        unreachable_outcome(outcome),
                        None,
                        None,
                    ));
                }
            }
        }
        // The commit's own arrival time: a held stream may have taken most of the hold timeout to commit.
        let arrived = (self.clock)();
        let verdict = classify(
            &head,
            &HeadContext {
                kind: ctx.account.kind,
                canonical_model: &ctx.identity.canonical_model,
                now: arrived,
                fallback_ms: ctx.fallback_ms,
                first_event: first_event.as_deref(),
                body_prefix: decoded.as_deref(),
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
        ctx.trace.head(
            ctx.attempt,
            &head,
            committing,
            attempt_outcome(&verdict, head.status, first_event.as_deref()),
            cooldown_set(cooldown_seen, &ctx.identity.canonical_model),
            written_at,
            head_at,
        );
        let generations = (ctx.account.lifecycle_generation, ctx.credential_generation);

        let (cooldown, mark) = match verdict {
            // A rejected replay is conclusive: the refreshed token was refused too.
            Verdict::Refresh if ctx.replayed => (None, Some(AccountState::ReauthRequired)),
            Verdict::Refresh if ctx.attempt < self.limits.max_attempts => {
                match self
                    .manager
                    .refresh_now(&ctx.account.id, ctx.credential_generation)
                    .await
                {
                    Ok(token) => return Next::Replay(Credential::from(token)),
                    // Nothing was replaced, so nothing conclusive happened: the upstream's 401 goes back as
                    // it came, and the account keeps its state and its pin. Failing over would spend another
                    // account on a request whose own credential may work again in a minute, and invalidating
                    // the pin would move a session that has no reason to move. The manager marks the account
                    // itself when the refresh ended in invalid_grant.
                    Err(error) => {
                        tracing::warn!(account = %ctx.account.id, %error, "the rejected token could not be refreshed");
                        return Next::Respond(self.forward_buffered(
                            ctx.trace,
                            &head,
                            buffered.unwrap_or_default(),
                            ctx.caller,
                            outcome,
                        ));
                    }
                }
            }
            // The budget does not allow a refresh and a replay, so nothing conclusive can happen: the 401
            // is forwarded as it came and the account keeps its state and its pin.
            Verdict::Refresh => {
                return Next::Respond(self.forward_buffered(
                    ctx.trace,
                    &head,
                    buffered.unwrap_or_default(),
                    ctx.caller,
                    outcome,
                ))
            }
            Verdict::FailOver { cooldown, mark } => (cooldown, mark),
            Verdict::Commit => {
                if let Some(pin) = ctx.pin {
                    self.router.report(Report::PinRenewed {
                        pin: pin.clone(),
                        now: (self.clock)(),
                    });
                }
                let (prefix, observer) = match held {
                    None => (Bytes::new(), None),
                    Some(Held { bytes, inspect, .. }) => {
                        let fallback_ms = ctx.fallback_ms;
                        let (router, clock, account_id, kind, model, pin, request_id) = (
                            self.router.clone(),
                            self.clock.clone(),
                            ctx.account.id.clone(),
                            ctx.account.kind,
                            ctx.identity.canonical_model.clone(),
                            ctx.portable.then(|| ctx.pin.cloned()).flatten(),
                            ctx.request_id.to_string(),
                        );
                        let head = ResponseHead {
                            status: head.status,
                            headers: head.headers.clone(),
                        };
                        let on_error = Box::new(move |data: Bytes| {
                            // After commit nothing fails over. A rate-limit error records the cooldown
                            // and, on a portable request, invalidates the pin so the next request
                            // migrates; a non-portable request keeps its pin. The deadline is measured
                            // from the error's arrival, not from the selection.
                            let now = clock();
                            if let Verdict::FailOver { cooldown, mark } = classify(
                                &head,
                                &HeadContext {
                                    kind,
                                    canonical_model: &model,
                                    now,
                                    fallback_ms,
                                    first_event: Some(&data),
                                    body_prefix: None,
                                },
                            ) {
                                report_failover(
                                    &router,
                                    &account_id,
                                    &model,
                                    cooldown,
                                    mark,
                                    pin,
                                    &request_id,
                                    generations,
                                    now,
                                );
                            }
                        }) as Box<dyn FnOnce(Bytes) + Send>;
                        (bytes, Some(PostCommit { inspect, on_error }))
                    }
                };
                return Next::Respond(match buffered {
                    Some(buffered) => {
                        self.forward_buffered(ctx.trace, &head, buffered, ctx.caller, outcome)
                    }
                    None => self.stream(Forward {
                        caller: ctx.caller,
                        cancel: ctx.cancel.clone(),
                        checkin,
                        head: &head,
                        keep_alive: reusable,
                        observer,
                        on_message_id: None,
                        outcome,
                        prefix,
                        reader: reader
                            .expect("a head the table reads no body for still holds its reader"),
                        trace: ctx.trace,
                    }),
                });
            }
        };
        // A non-portable request keeps its pin: the failed response goes back to the client on the
        // account that holds its state, and only the cooldown is recorded.
        report_failover(
            &self.router,
            &ctx.account.id,
            &ctx.identity.canonical_model,
            cooldown,
            mark,
            ctx.portable.then(|| ctx.pin.cloned()).flatten(),
            ctx.request_id,
            generations,
            arrived,
        );
        // A held stream cannot be rewound onto another connection, so the only way not to commit it is
        // to drop it unread; once the budget is spent it goes to the caller as it came.
        if let Some(held) = held {
            if ctx.portable && ctx.attempt < self.limits.max_attempts {
                drop(reader);
                return Next::FailOver;
            }
            return Next::Respond(self.stream(Forward {
                caller: ctx.caller,
                cancel: ctx.cancel.clone(),
                checkin,
                head: &head,
                keep_alive: false,
                observer: None,
                on_message_id: None,
                outcome,
                prefix: held.bytes,
                reader: reader.expect("a streaming head still holds the reader the hold read from"),
                trace: ctx.trace,
            }));
        }
        // Every failover row the table produces answers 400, 401, 403, or 429, all of which were
        // drained above, so the default here is the empty body of a head that carried none. A row
        // added for a status outside that set would forward nothing instead of its own body.
        let buffered = buffered.unwrap_or_default();
        if ctx.portable && ctx.attempt < self.limits.max_attempts {
            return Next::FailOver;
        }
        Next::Respond(self.forward_buffered(ctx.trace, &head, buffered, ctx.caller, outcome))
    }

    /// The credential to dispatch with, fenced by the revision of the row the selection handed over:
    /// acquiring takes time, and an edit landing inside it would pair a replacement secret with the endpoint
    /// the request was selected for, which nothing ever verified together.
    async fn credential(&self, account: &AccountRow) -> Result<Credential, NoCredential> {
        let credential = match account.kind {
            AccountKind::Oauth => match self.manager.acquire(&account.id).await {
                Ok(token) => Credential::from(token),
                Err(error) => {
                    tracing::warn!(account = %account.id, %error, "no credential for the selected account");
                    return Err(NoCredential::Unavailable);
                }
            },
            // The generation comes along so an api_key verdict is fenced by the same rule an OAuth one is.
            AccountKind::ApiKey => match self.store.api_key_credential(&account.id) {
                Ok(Some((secret, generation))) => Credential {
                    account_uuid: None,
                    generation,
                    kind: CredentialKind::ApiKey { secret },
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
        match self.store.account_revision(&account.id) {
            Ok(Some(revision)) if revision == account.revision => Ok(credential),
            Ok(_) => {
                tracing::warn!(
                    account = %account.id,
                    "the account changed while its credential was being acquired",
                );
                Err(NoCredential::Stale)
            }
            Err(error) => {
                tracing::error!(account = %account.id, %error, "the account's revision is unreadable");
                Err(NoCredential::Stale)
            }
        }
    }

    fn refused(
        &self,
        trace: &Trace,
        failure: Failure,
        key: Option<&ApiKeyRow>,
        identity: Option<&RequestIdentity>,
        client_version: Option<&str>,
    ) -> Response {
        let outcome = self.outcome(
            trace,
            key,
            identity,
            "",
            0,
            0,
            false,
            None,
            client_version.map(str::to_string),
            Some(failure.code),
            None,
            None,
        );
        self.answer(
            trace,
            failure.response(&trace.request_id),
            outcome,
            None,
            None,
        )
    }

    /// A locally built answer: the outcome rides on the response for the accounting layer, and the completion
    /// is published with the status the response carries, which for a local refusal is the refusal's own.
    fn answer(
        &self,
        trace: &Trace,
        response: Response,
        outcome: RelayOutcome,
        usage: Option<Usage>,
        response_bytes: Option<u64>,
    ) -> Response {
        trace.complete(
            &outcome,
            response.status().as_u16(),
            usage,
            None,
            response_bytes,
            None,
            (self.clock)(),
        );
        with_outcome(response, outcome)
    }

    /// The answer to a response that arrived for an account that is no longer there.
    fn cancelled(&self, ctx: &Responding<'_>, head: &ResponseHead, written_at: i64) -> Response {
        ctx.trace.head(
            ctx.attempt,
            head,
            None,
            AttemptOutcome::Canceled,
            None,
            written_at,
            (self.clock)(),
        );
        let outcome = self.outcome(
            ctx.trace,
            Some(ctx.key),
            Some(ctx.identity),
            &ctx.account.id,
            head.status,
            ctx.attempt,
            true,
            None,
            ctx.client_version.clone(),
            Some("account_cancelled"),
            ctx.pin_epoch,
            None,
        );
        self.answer(
            ctx.trace,
            ACCOUNT_CANCELLED.response(ctx.request_id),
            outcome,
            None,
            None,
        )
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
    ) -> RelayOutcome {
        RelayOutcome {
            request_id: trace.request_id.clone(),
            key_id: key.map(|k| k.id.clone()),
            pool_id: key.map(|k| k.pool_id.clone()),
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
        }
    }

    /// Only an OAuth head with the unified headers is evidence; everything else changes nothing.
    fn report_overage(&self, account: &AccountRow, head: &ResponseHead, now: i64) {
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
        self.router.report(Report::Overage {
            account_id: account.id.clone(),
            evidence: OverageEvidence {
                in_use,
                since: in_use.then_some(observed_at),
                reset,
                observed_at,
                source: EvidenceSource::Head,
            },
        });
    }

    fn stream<C: AsyncRead + ReturnToPool + Send + Unpin + 'static>(
        &self,
        forward: Forward<'_, C>,
    ) -> Response {
        let Forward {
            caller,
            cancel,
            checkin,
            head,
            keep_alive,
            observer,
            on_message_id,
            outcome,
            prefix,
            reader,
            trace,
        } = forward;
        let (builder, decoder) = match forward_head(head, caller) {
            Ok(parts) => parts,
            Err(error) => {
                tracing::warn!(%error, "the upstream response cannot be forwarded");
                return self.answer(
                    trace,
                    UPSTREAM_UNREACHABLE.response(&outcome.request_id),
                    unreachable_outcome(outcome),
                    None,
                    None,
                );
            }
        };
        let event_stream = head
            .get("content-type")
            .is_some_and(|v| v.to_ascii_lowercase().starts_with("text/event-stream"));
        // Without a bus every emit and the completion are no-ops; a hook with no bus still needs the parse run for it.
        let publish = (trace.bus_attached() || on_message_id.is_some()).then(|| Publish {
            trace: trace.clone(),
            clock: self.clock.clone(),
            outcome: outcome.clone(),
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
            usage: Usage::default(),
            index: 0,
            bytes: 0,
            first_content_at: None,
            saw_tool_use: false,
            stream_error: None,
            status: head.status,
            message_id: None,
            on_message_id,
        });
        let body = relay_body(Relayed {
            cancel,
            checkin,
            decoder,
            idle_timeout: self.limits.idle_timeout,
            keep_alive,
            observer,
            prefix,
            publish,
            reader,
        });
        with_outcome(built(builder, body), outcome)
    }

    fn forward_buffered(
        &self,
        trace: &Trace,
        head: &ResponseHead,
        raw: Bytes,
        caller: &HeaderMap,
        outcome: RelayOutcome,
    ) -> Response {
        let (builder, decoder) = match forward_head(head, caller) {
            Ok(parts) => parts,
            Err(error) => {
                tracing::warn!(%error, "the upstream response cannot be forwarded");
                return self.answer(
                    trace,
                    UPSTREAM_UNREACHABLE.response(&outcome.request_id),
                    unreachable_outcome(outcome),
                    None,
                    None,
                );
            }
        };
        let payload = match decoder {
            None => raw.clone(),
            Some(mut decoder) => match decode_all(decoder.as_mut(), &raw) {
                Ok(payload) => payload,
                Err(error) => {
                    tracing::warn!(%error, "the upstream response cannot be decoded");
                    return self.answer(
                        trace,
                        UPSTREAM_UNREACHABLE.response(&outcome.request_id),
                        unreachable_outcome(outcome),
                        None,
                        None,
                    );
                }
            },
        };
        // The chunk is the decoded body whatever the caller accepts; a coding the caller takes as it is still
        // reaches the bus as text. A body that will not decode reaches it as nothing, because a consumer of a
        // chunk was promised text and coded bytes are worse than none.
        let usage = match inspectable_copy(head, &raw).map(Bytes::from) {
            None => None,
            Some(text) => {
                trace.publish_chunk(ResponseChunk {
                    request_id: trace.request_id.clone(),
                    attempt: outcome.attempt,
                    index: 0,
                    body: text.clone(),
                    phase: None,
                });
                // Only the refresh path reaches here with a 200; the ordinary JSON answer is inspected by the
                // body task.
                (head.status == 200).then(|| usage_from_json(&text, &[]))
            }
        };
        self.answer(
            trace,
            built(builder, Body::from(payload.clone())),
            outcome,
            usage,
            Some(payload.len() as u64),
        )
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
    caller: &'a HeaderMap,
    cancel: CancellationToken,
    checkin: Checkin,
    head: &'a ResponseHead,
    keep_alive: bool,
    /// Set for a committed stream, which is watched for a rate-limit error arriving after the commit.
    observer: Option<PostCommit>,
    on_message_id: Option<Box<dyn FnOnce(String) + Send>>,
    outcome: RelayOutcome,
    prefix: Bytes,
    reader: BodyReader<C>,
    trace: &'a Trace,
}

/// Why a selected account produced nothing to dispatch with. Neither answer reaches the caller: the account
/// is already in `attempted`, so both send the loop back to select without spending an attempt.
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
    caller: &'a HeaderMap,
    cancel: &'a CancellationToken,
    client_version: Option<String>,
    credential_generation: u64,
    fallback_ms: i64,
    identity: &'a RequestIdentity,
    key: &'a ApiKeyRow,
    pin: Option<&'a PinRef>,
    pin_epoch: Option<u64>,
    portable: bool,
    replayed: bool,
    request_id: &'a str,
    trace: &'a Trace,
}

/// What the response half decided the account loop should do next.
enum Next {
    /// This account is finished; another one takes the request.
    FailOver,
    /// The refreshed token goes back to the same account.
    Replay(Credential),
    Respond(Response),
}

/// What the same-account leg reads about the request it is part of: fixed at admission and shared by every
/// account the request tries.
struct Admitted<'a> {
    caller: &'a HeaderMap,
    client_version: Option<String>,
    fallback_ms: i64,
    identity: &'a RequestIdentity,
    key: &'a ApiKeyRow,
    /// The caller's head as it arrived, absent for an in-process caller and for a connection the recorder
    /// gave up on.
    original: Option<&'a OriginalHead>,
    portable: bool,
    raw: &'a Bytes,
    request_id: &'a str,
    target: &'a str,
    trace: &'a Trace,
}

/// What one selected account produced: an answer for the caller, or nothing, in which case the loop selects
/// the next account. The account is already in `attempted` either way.
enum Leg {
    FailOver,
    Respond(Response),
}

/// `generations` is the attempt's (lifecycle, credential) pair, which fences the mark: a verdict about a
/// credential that was replaced while the request was in flight lands nowhere.
#[allow(clippy::too_many_arguments)]
pub(crate) fn report_failover(
    router: &RouterHandle,
    account_id: &str,
    canonical_model: &str,
    cooldown: Option<Cooldown>,
    mark: Option<AccountState>,
    pin: Option<PinRef>,
    request_id: &str,
    generations: (u64, u64),
    set_at: i64,
) {
    if let Some(cooldown) = cooldown {
        let scope = match cooldown.scope {
            Scope::Account => "account".to_string(),
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
        };
        router.report(Report::Cooldown {
            account_id: account_id.to_string(),
            row,
            pin,
            lifecycle_generation: generations.0,
            credential_generation: generations.1,
        });
    } else if let Some(pin) = pin {
        // A mark without a cooldown still ends the pin: the account cannot serve until an operator acts.
        router.report(Report::PinInvalidated {
            account_id: account_id.to_string(),
            pin,
            lifecycle_generation: generations.0,
            credential_generation: generations.1,
        });
    }
    if let Some(state) = mark {
        router.report(Report::AccountState {
            account_id: account_id.to_string(),
            state,
            lifecycle_generation: generations.0,
            credential_generation: generations.1,
        });
    }
}

/// Decoded in pieces under the same bound as the inspection copy, so a small compressed rejection cannot
/// expand without limit in memory.
fn decode_all(decoder: &mut dyn Decoder, raw: &[u8]) -> Result<Bytes, TransportError> {
    let mut out = Vec::new();
    for piece in raw.chunks(DECODE_PIECE) {
        out.extend(decoder.feed(piece)?);
        if out.len() > MAX_BUFFERED_BODY {
            return Err(TransportError::TooLarge(MAX_BUFFERED_BODY));
        }
    }
    out.extend(decoder.finish()?);
    if out.len() > MAX_BUFFERED_BODY {
        return Err(TransportError::TooLarge(MAX_BUFFERED_BODY));
    }
    Ok(Bytes::from(out))
}

/// Builds the caller's response head from the upstream's and decides whether the payload can be passed through
/// as it came. Returns the decoder to run over the body when it cannot.
fn forward_head(
    head: &ResponseHead,
    caller: &HeaderMap,
) -> Result<(Builder, Option<Box<dyn Decoder>>), TransportError> {
    let upstream = encoding(head)?;
    let coding = head.get("content-encoding").unwrap_or("identity");
    let passthrough = matches!(upstream, Encoding::Identity) || caller_accepts(caller, coding);
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(head.status).unwrap_or(StatusCode::BAD_GATEWAY));
    for (name, value) in &head.headers {
        if NOT_FORWARDED.iter().any(|n| name.eq_ignore_ascii_case(n)) {
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

/// Whether the caller said it can read this content coding. An entry weighted `q=0` is a refusal, so the
/// payload is decoded for it rather than passed through.
fn caller_accepts(caller: &HeaderMap, coding: &str) -> bool {
    let Some(accept) = caller
        .get(header::ACCEPT_ENCODING)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    accept.split(',').any(|entry| {
        let (name, params) = entry.split_once(';').unwrap_or((entry, ""));
        let name = name.trim();
        let refused = params
            .split(';')
            .filter_map(|param| param.trim().strip_prefix("q="))
            .any(|weight| {
                weight
                    .trim()
                    .parse::<f32>()
                    .is_ok_and(|weight| weight <= 0.0)
            });
        !refused && (name.eq_ignore_ascii_case(coding) || name == "*")
    })
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
        out.extend(decoder.feed(piece).ok()?);
        if out.len() > MAX_BUFFERED_BODY {
            return None;
        }
    }
    out.extend(decoder.finish().ok()?);
    (out.len() <= MAX_BUFFERED_BODY).then_some(out)
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
}
