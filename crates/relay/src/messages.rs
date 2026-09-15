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
use phoebis_oauth::{AccessToken, Clock, CredentialManager};
use phoebis_profile::Profile;
use phoebis_routing::{
    resolve, EvidenceSource, Exhaustion, OverageEvidence, PinRef, Report, ResolveError,
    RouterHandle, SelectRequest, SelectResult,
};
use phoebis_store::{AccountKind, AccountRow, AccountState, ApiKeyRow, CooldownRow, Store};
use phoebis_transport::control::Resolver;
use phoebis_transport::decode::{decoder, encoding, Decoder, Encoding};
use phoebis_transport::pool::{Conn, Pool, PoolKey};
use phoebis_transport::request::RawRequest;
use phoebis_transport::response::{framing, keep_alive, BodyReader, ResponseHead};
use phoebis_transport::TransportError;
use tokio_util::sync::CancellationToken;

use crate::classify::{classify, Cooldown, HeadContext, Scope, Verdict};
use crate::dispatch::{self, until_cancelled, AttemptFailure, Sent};
use crate::errors::{
    acquire_failure, auth_failure, key_refusal, relay_failure, Failure, ACCOUNTS_UNAVAILABLE,
    ACCOUNT_CANCELLED, METHOD_NOT_ALLOWED, MODEL_NOT_ALLOWED, MODEL_UNSERVED, NOT_FOUND,
    ROUTER_UNAVAILABLE, UPSTREAM_UNREACHABLE,
};
use crate::exhaustion::exhaustion_response;
use crate::headers::{outbound_headers, CredentialKind};
use crate::identity::{identify, RequestIdentity};
use crate::ratelimit::{upstream_date, UnifiedHeaders};
use crate::stream::{drain_bounded, relay_body, Drained, Relayed, MAX_BUFFERED_BODY};
use crate::{edit_body, is_portable, presented_key, DriftGuard, RelayError};

/// The one host the relay dials.
const INFERENCE_HOST: &str = "api.anthropic.com";

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
    /// Names the kind of drift when the caller was not the pinned client: `hash` for a billing hash this
    /// build cannot reproduce, in which case the body went out unedited, and `version` for a foreign build.
    pub client_drift: Option<String>,
    pub client_version: Option<String>,
    pub error_code: Option<String>,
    pub pin_epoch: Option<u64>,
}

pub struct Relay {
    clock: Clock,
    drift: DriftGuard,
    pub(crate) limits: Limits,
    manager: Arc<CredentialManager>,
    pub(crate) pool: Arc<Pool>,
    profile: &'static Profile,
    resolver: Resolver,
    router: RouterHandle,
    store: Arc<Store>,
}

impl Relay {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        profile: &'static Profile,
        store: Arc<Store>,
        manager: Arc<CredentialManager>,
        pool: Arc<Pool>,
        router: RouterHandle,
        resolver: Resolver,
        clock: Clock,
        limits: Limits,
    ) -> Relay {
        Relay {
            clock,
            drift: DriftGuard::new(profile),
            limits,
            manager,
            pool,
            profile,
            resolver,
            router,
            store,
        }
    }
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

impl Relay {
    async fn handle(&self, request: Request) -> Response {
        let request_id = new_request_id();
        let now = (self.clock)();
        let (parts, body) = request.into_parts();
        // Before the body: an unauthenticated caller that could reach the read below would hold the whole
        // request limit per connection for as long as it cared to keep sending.
        let presented = match presented_key(&parts.headers) {
            Ok(presented) => presented,
            Err(error) => return auth_failure(&error).response(&request_id),
        };
        // The snapshot is pinned for the whole request: the key, the pool, and the candidates all come from one
        // registry, and a write that lands mid-request is seen by the next request.
        let snapshot = self.router.snapshot();
        let key = match snapshot.authenticate(presented, now) {
            Ok(key) => key.clone(),
            Err(refusal) => {
                tracing::debug!(%refusal, "a presented key was refused");
                return key_refusal(&refusal).response(&request_id);
            }
        };
        self.router.touch_key(&key.id);
        let Ok(raw) = axum::body::to_bytes(body, self.limits.max_request_bytes).await else {
            // Either the caller went past the limit or its own connection broke mid-body; either way there is
            // no complete request to relay.
            return self.refused(
                relay_failure(&RelayError::BodyTooLarge),
                &request_id,
                &key,
                None,
            );
        };
        let identity = match identify(&parts.headers, &raw) {
            Ok(identity) => identity,
            Err(error) => return self.refused(relay_failure(&error), &request_id, &key, None),
        };
        let client_version = self.drift.check(&parts.headers);
        // Version drift is a tag on a relayed request; hash drift is a refusal, below.
        let client_drift = client_version.as_ref().map(|_| "version".to_string());
        let candidates = match resolve(&snapshot, &key, &identity.canonical_model) {
            Ok(candidates) => candidates,
            Err(ResolveError::ModelNotAllowed) => {
                return self.refused(MODEL_NOT_ALLOWED, &request_id, &key, Some(&identity))
            }
            Err(ResolveError::ModelUnserved) => {
                return self.refused(MODEL_UNSERVED, &request_id, &key, Some(&identity))
            }
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
            request = %request_id,
            session = ?identity.session_id,
            "relaying a Messages request",
        );
        let mut attempted: Vec<String> = Vec::new();
        let mut attempt: u8 = 0;
        // The last failover row's non-streaming response, committed to the client when the budget runs out.
        let mut last_failed: Option<(ResponseHead, Bytes, RelayOutcome)> = None;

        loop {
            if attempt >= self.limits.max_attempts {
                return match last_failed {
                    Some((head, buffered, outcome)) => {
                        forward_buffered(&head, buffered, &parts.headers, outcome)
                    }
                    None => self.refused(UPSTREAM_UNREACHABLE, &request_id, &key, Some(&identity)),
                };
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
                    return self.refused(ROUTER_UNAVAILABLE, &request_id, &key, Some(&identity))
                }
            };
            let selection = match answer.result {
                SelectResult::Selected(selection) => selection,
                // The registry moved after admission and no longer admits this request; the answer is the one the
                // current registry would have given at admission.
                SelectResult::Refused(ResolveError::ModelNotAllowed) => {
                    return self.refused(MODEL_NOT_ALLOWED, &request_id, &key, Some(&identity))
                }
                SelectResult::Refused(ResolveError::ModelUnserved) => {
                    return self.refused(MODEL_UNSERVED, &request_id, &key, Some(&identity))
                }
                SelectResult::Exhausted(exhaustion) => {
                    let code = match exhaustion {
                        Exhaustion::Timed { .. } => "accounts_exhausted",
                        Exhaustion::Untimed => "accounts_unavailable",
                    };
                    let outcome = self.outcome(
                        &request_id,
                        &key,
                        Some(&identity),
                        "",
                        0,
                        attempt,
                        false,
                        client_drift.clone(),
                        client_version.clone(),
                        Some(code),
                        None,
                    );
                    return with_outcome(
                        exhaustion_response(&exhaustion, now, &request_id),
                        outcome,
                    );
                }
            };
            // No wire submission, so no attempt is spent: the account is already in `attempted`, and the next
            // selection skips it. The outer loop is bounded by the pool size, because each account is selected
            // at most once per request.
            let Some(account) = snapshot.account(&selection.account_id).cloned() else {
                attempted.push(selection.account_id);
                continue;
            };
            attempted.push(account.id.clone());
            let pin_epoch = selection.pin.as_ref().map(|pin| pin.epoch);
            let mut credential = match self.credential(&account).await {
                Ok(credential) => credential,
                Err(_) => continue,
            };
            let mut edited = raw.to_vec();
            if let Err(error) = edit_body(
                &mut edited,
                credential.account_uuid.as_deref(),
                &selection.upstream_model,
            ) {
                if let RelayError::ClientDrift { kind } = &error {
                    // Drift depends on the body alone, so it is found on the first account before any wire
                    // submission: nothing is sent until the router is re-pinned, and the outcome carries the tag.
                    tracing::warn!(
                        account = %account.id,
                        drift = %kind,
                        user_agent = %user_agent(&parts.headers),
                        "a caller stamped a billing hash this build cannot reproduce; re-pin the router to that client",
                    );
                    let outcome = self.outcome(
                        &request_id,
                        &key,
                        Some(&identity),
                        &account.id,
                        0,
                        0,
                        false,
                        Some(kind.to_string()),
                        client_version.clone(),
                        Some("client_drift"),
                        None,
                    );
                    return with_outcome(relay_failure(&error).response(&request_id), outcome);
                }
                return self.refused(relay_failure(&error), &request_id, &key, Some(&identity));
            }
            let edited = Bytes::from(edited);
            let host = INFERENCE_HOST.to_string();
            let Some(addr) = (self.resolver)(&host) else {
                tracing::warn!(host = %host, "the upstream host does not resolve");
                continue;
            };
            let pool_key = PoolKey {
                account_id: account.id.clone(),
                host: host.clone(),
            };
            let mut fresh = false;
            let mut retried = false;
            let mut replayed = false;

            'account: loop {
                attempt += 1;
                let request = RawRequest {
                    method: "POST",
                    target: target.clone(),
                    headers: outbound_headers(
                        self.profile,
                        &parts.headers,
                        &credential.kind,
                        &host,
                        edited.len(),
                    ),
                    body: edited.clone(),
                };
                let sent = match dispatch::attempt(
                    self,
                    &request,
                    &pool_key,
                    &host,
                    addr,
                    fresh,
                    &answer.cancel,
                )
                .await
                {
                    Ok(sent) => sent,
                    Err(AttemptFailure::Unsent(detail))
                        if !retried && attempt < self.limits.max_attempts =>
                    {
                        tracing::debug!(account = %account.id, %detail, "retrying a request that never reached the upstream");
                        fresh = true;
                        retried = true;
                        continue 'account;
                    }
                    Err(AttemptFailure::Cancelled) => {
                        let outcome = self.outcome(
                            &request_id,
                            &key,
                            Some(&identity),
                            &account.id,
                            0,
                            attempt,
                            false,
                            client_drift.clone(),
                            client_version.clone(),
                            Some("account_cancelled"),
                            pin_epoch,
                        );
                        return with_outcome(ACCOUNT_CANCELLED.response(&request_id), outcome);
                    }
                    Err(failure) => {
                        let written = matches!(failure, AttemptFailure::Lost(_));
                        tracing::warn!(account = %account.id, %failure, "the upstream did not answer");
                        let outcome = self.outcome(
                            &request_id,
                            &key,
                            Some(&identity),
                            &account.id,
                            0,
                            attempt,
                            written,
                            client_drift.clone(),
                            client_version.clone(),
                            Some("upstream_unreachable"),
                            pin_epoch,
                        );
                        return with_outcome(UPSTREAM_UNREACHABLE.response(&request_id), outcome);
                    }
                };
                let Sent {
                    conn,
                    head,
                    leftover,
                } = sent;
                // Cooldown deadlines are measured from when the head arrived, not from when the account was
                // selected: a slow upstream must not shorten what it asked for.
                let arrived = (self.clock)();
                let framing = framing(&head);
                let reusable = keep_alive(&head, &framing);
                let mut reader = BodyReader::new(conn, leftover, framing);
                let outcome = self.outcome(
                    &request_id,
                    &key,
                    Some(&identity),
                    &account.id,
                    head.status,
                    attempt,
                    true,
                    client_drift.clone(),
                    client_version.clone(),
                    None,
                    pin_epoch,
                );
                self.report_overage(&account, &head, arrived);

                // Heads the table reads a body for are drained within the bound; a body too large to read
                // commits. The reader survives only where the body was left alone, because a drained body has
                // already handed its connection back to the pool.
                let (buffered, reader) = if matches!(head.status, 400 | 401 | 403 | 429) {
                    let Some(drained) = until_cancelled(
                        drain_bounded(&mut reader, self.limits.idle_timeout),
                        &answer.cancel,
                    )
                    .await
                    else {
                        return self.cancelled(
                            &request_id,
                            &key,
                            &identity,
                            &account.id,
                            head.status,
                            attempt,
                            &client_drift,
                            &client_version,
                            pin_epoch,
                        );
                    };
                    match drained {
                        Ok(Drained::Complete(buffered)) => {
                            if reusable {
                                self.pool.checkin(pool_key.clone(), reader.into_inner());
                            }
                            (Some(buffered), None)
                        }
                        Ok(Drained::TooLarge(prefix)) => {
                            tracing::debug!(account = %account.id, "forwarding a rejection too large to inspect");
                            return self.stream(Forward {
                                caller: &parts.headers,
                                cancel: answer.cancel.clone(),
                                head: &head,
                                keep_alive: reusable,
                                key: pool_key,
                                outcome,
                                prefix,
                                reader,
                            });
                        }
                        Err(error) => {
                            tracing::warn!(account = %account.id, %error, "the rejection body was not readable");
                            return with_outcome(
                                UPSTREAM_UNREACHABLE.response(&request_id),
                                outcome,
                            );
                        }
                    }
                } else {
                    (None, Some(reader))
                };
                let decoded = buffered.as_ref().and_then(|b| inspectable_copy(&head, b));
                let verdict = classify(
                    &head,
                    &HeadContext {
                        kind: account.kind,
                        canonical_model: &identity.canonical_model,
                        now: arrived,
                        fallback_ms,
                        first_event: None,
                        body_prefix: decoded.as_deref(),
                    },
                );
                let generations = (account.lifecycle_generation, credential.generation);

                let (cooldown, mark) = match verdict {
                    // A rejected replay is conclusive: the refreshed token was refused too.
                    Verdict::Refresh if replayed => (None, Some(AccountState::ReauthRequired)),
                    Verdict::Refresh if attempt < self.limits.max_attempts => {
                        match self
                            .manager
                            .refresh_now(&account.id, credential.generation)
                            .await
                        {
                            Ok(token) => {
                                credential = Credential::from(token);
                                replayed = true;
                                fresh = false;
                                continue 'account;
                            }
                            // The manager marks the account itself when the refresh ended in invalid_grant; anything
                            // else is inconclusive, so the upstream's 401 goes back with no mark from here.
                            Err(error) => {
                                tracing::warn!(account = %account.id, %error, "the rejected token could not be refreshed");
                                (None, None)
                            }
                        }
                    }
                    // The budget does not allow a refresh and a replay, so nothing conclusive can happen: the 401
                    // is forwarded as it came and the account keeps its state and its pin.
                    Verdict::Refresh => {
                        return forward_buffered(
                            &head,
                            buffered.unwrap_or_default(),
                            &parts.headers,
                            outcome,
                        )
                    }
                    Verdict::FailOver { cooldown, mark } => (cooldown, mark),
                    Verdict::Commit | Verdict::RetrySameAccount => {
                        if let Some(pin) = &selection.pin {
                            self.router.report(Report::PinRenewed {
                                pin: pin.clone(),
                                now: (self.clock)(),
                            });
                        }
                        return match buffered {
                            Some(buffered) => {
                                forward_buffered(&head, buffered, &parts.headers, outcome)
                            }
                            None => self.stream(Forward {
                                caller: &parts.headers,
                                cancel: answer.cancel.clone(),
                                head: &head,
                                keep_alive: reusable,
                                key: pool_key,
                                outcome,
                                prefix: Bytes::new(),
                                reader: reader.expect(
                                    "a head the table reads no body for still holds its reader",
                                ),
                            }),
                        };
                    }
                };
                // A non-portable request keeps its pin: the failed response goes back to the client on the
                // account that holds its state, and only the cooldown is recorded.
                report_failover(
                    &self.router,
                    &account.id,
                    &identity.canonical_model,
                    cooldown,
                    mark,
                    if portable {
                        selection.pin.clone()
                    } else {
                        None
                    },
                    &request_id,
                    generations,
                );
                // Every failover row the table produces answers 400, 401, 403, or 429, all of which were
                // drained above, so the default here is the empty body of a head that carried none. A row
                // added for a status outside that set would forward nothing instead of its own body.
                let buffered = buffered.unwrap_or_default();
                if portable && attempt < self.limits.max_attempts {
                    last_failed = Some((head, buffered, outcome));
                    break 'account;
                }
                return forward_buffered(&head, buffered, &parts.headers, outcome);
            }
        }
    }

    async fn credential(&self, account: &AccountRow) -> Result<Credential, Failure> {
        match account.kind {
            AccountKind::Oauth => match self.manager.acquire(&account.id).await {
                Ok(token) => Ok(Credential::from(token)),
                Err(error) => {
                    tracing::warn!(account = %account.id, %error, "no credential for the selected account");
                    Err(acquire_failure(&error))
                }
            },
            // The generation comes along so an api_key verdict is fenced by the same rule an OAuth one is.
            AccountKind::ApiKey => match self.store.api_key_credential(&account.id) {
                Ok(Some((secret, generation))) => Ok(Credential {
                    account_uuid: None,
                    generation,
                    kind: CredentialKind::ApiKey { secret },
                }),
                Ok(None) => {
                    tracing::warn!(account = %account.id, "the account has no API key stored");
                    Err(ACCOUNTS_UNAVAILABLE)
                }
                Err(error) => {
                    tracing::error!(account = %account.id, %error, "the stored API key is unreadable");
                    Err(ACCOUNTS_UNAVAILABLE)
                }
            },
        }
    }

    fn refused(
        &self,
        failure: Failure,
        request_id: &str,
        key: &ApiKeyRow,
        identity: Option<&RequestIdentity>,
    ) -> Response {
        let outcome = self.outcome(
            request_id,
            key,
            identity,
            "",
            0,
            0,
            false,
            None,
            None,
            Some(failure.code),
            None,
        );
        with_outcome(failure.response(request_id), outcome)
    }

    /// The answer to a response that arrived for an account that is no longer there.
    #[allow(clippy::too_many_arguments)]
    fn cancelled(
        &self,
        request_id: &str,
        key: &ApiKeyRow,
        identity: &RequestIdentity,
        account_id: &str,
        upstream_status: u16,
        attempt: u8,
        client_drift: &Option<String>,
        client_version: &Option<String>,
        pin_epoch: Option<u64>,
    ) -> Response {
        let outcome = self.outcome(
            request_id,
            key,
            Some(identity),
            account_id,
            upstream_status,
            attempt,
            true,
            client_drift.clone(),
            client_version.clone(),
            Some("account_cancelled"),
            pin_epoch,
        );
        with_outcome(ACCOUNT_CANCELLED.response(request_id), outcome)
    }

    #[allow(clippy::too_many_arguments)]
    fn outcome(
        &self,
        request_id: &str,
        key: &ApiKeyRow,
        identity: Option<&RequestIdentity>,
        account_id: &str,
        upstream_status: u16,
        attempt: u8,
        written: bool,
        client_drift: Option<String>,
        client_version: Option<String>,
        error_code: Option<&str>,
        pin_epoch: Option<u64>,
    ) -> RelayOutcome {
        RelayOutcome {
            request_id: request_id.to_string(),
            key_id: Some(key.id.clone()),
            pool_id: Some(key.pool_id.clone()),
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

    fn stream(&self, forward: Forward<'_>) -> Response {
        let Forward {
            caller,
            cancel,
            head,
            keep_alive,
            key,
            outcome,
            prefix,
            reader,
        } = forward;
        let (builder, decoder) = match forward_head(head, caller) {
            Ok(parts) => parts,
            Err(error) => {
                tracing::warn!(%error, "the upstream response cannot be forwarded");
                return with_outcome(UPSTREAM_UNREACHABLE.response(&outcome.request_id), outcome);
            }
        };
        let body = relay_body(Relayed {
            cancel,
            decoder,
            idle_timeout: self.limits.idle_timeout,
            keep_alive,
            key,
            pool: self.pool.clone(),
            prefix,
            reader,
        });
        with_outcome(built(builder, body), outcome)
    }
}

/// A response whose head is settled and whose body is still on the wire.
struct Forward<'a> {
    caller: &'a HeaderMap,
    cancel: CancellationToken,
    head: &'a ResponseHead,
    keep_alive: bool,
    key: PoolKey,
    outcome: RelayOutcome,
    prefix: Bytes,
    reader: BodyReader<Conn>,
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
            reason: cooldown.reason.to_string(),
            window: cooldown.window,
            source_request_id: Some(request_id.to_string()),
        };
        router.report(Report::Cooldown {
            account_id: account_id.to_string(),
            row,
            pin,
        });
    } else if let Some(pin) = pin {
        // A mark without a cooldown still ends the pin: the account cannot serve until an operator acts.
        router.report(Report::PinInvalidated {
            account_id: account_id.to_string(),
            pin,
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

fn forward_buffered(
    head: &ResponseHead,
    raw: Bytes,
    caller: &HeaderMap,
    outcome: RelayOutcome,
) -> Response {
    let (builder, decoder) = match forward_head(head, caller) {
        Ok(parts) => parts,
        Err(error) => {
            tracing::warn!(%error, "the upstream response cannot be forwarded");
            return with_outcome(UPSTREAM_UNREACHABLE.response(&outcome.request_id), outcome);
        }
    };
    let payload = match decoder {
        None => raw,
        Some(mut decoder) => match decode_all(decoder.as_mut(), &raw) {
            Ok(payload) => payload,
            Err(error) => {
                tracing::warn!(%error, "the upstream response cannot be decoded");
                return with_outcome(UPSTREAM_UNREACHABLE.response(&outcome.request_id), outcome);
            }
        },
    };
    with_outcome(built(builder, Body::from(payload)), outcome)
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
fn new_request_id() -> String {
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
