//! The Messages routes: admission, the account's credential, the upstream attempt with its retry and refresh
//! rules, and the response the caller gets.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::response::Builder;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::{get, head, post};
use axum::Router;
use bytes::{Bytes, BytesMut};
use phoebis_oauth::{AccessToken, CredentialManager};
use phoebis_profile::Profile;
use phoebis_store::{AccountKind, AccountRow, AccountState, Store};
use phoebis_transport::control::Resolver;
use phoebis_transport::decode::{decoder, encoding, Decoder, Encoding};
use phoebis_transport::pool::{Conn, Pool, PoolKey};
use phoebis_transport::request::{write_request, RawRequest, WriteProgress};
use phoebis_transport::response::{framing, keep_alive, read_head, ResponseHead};
use phoebis_transport::TransportError;

use crate::errors::{
    acquire_failure, auth_failure, relay_failure, Failure, ACCOUNTS_UNAVAILABLE,
    METHOD_NOT_ALLOWED, NOT_FOUND, UPSTREAM_UNREACHABLE,
};
use crate::headers::{outbound_headers, CredentialKind};
use crate::identity::{identify, upstream_model};
use crate::stream::{read_bounded, relay_body, Relayed};
use crate::{edit_body, KeyTable, RelayError};

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
    pub max_request_bytes: usize,
    pub head_timeout: Duration,
    pub idle_timeout: Duration,
}

/// What one relayed request cost and how it ended, attached to the response for the accounting layer that
/// wraps the relay.
#[derive(Clone, Debug)]
pub struct RelayOutcome {
    pub request_id: String,
    pub account_id: String,
    pub upstream_status: u16,
    /// One for the first upstream attempt, zero for a request refused before it got that far.
    pub attempt: u8,
    /// Whether the request was written to the upstream in full, which is what decides if it may have been
    /// billed even when no response came back.
    pub written: bool,
}

pub struct Relay {
    keys: KeyTable,
    limits: Limits,
    manager: Arc<CredentialManager>,
    pool: Arc<Pool>,
    profile: &'static Profile,
    resolver: Resolver,
    store: Arc<Store>,
}

impl Relay {
    pub fn new(
        profile: &'static Profile,
        store: Arc<Store>,
        manager: Arc<CredentialManager>,
        pool: Arc<Pool>,
        keys: KeyTable,
        resolver: Resolver,
        limits: Limits,
    ) -> Relay {
        Relay {
            keys,
            limits,
            manager,
            pool,
            profile,
            resolver,
            store,
        }
    }
}

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
/// into the body, and the generation a rejection has to name when it asks for a refresh.
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

struct Sent {
    conn: Conn,
    head: ResponseHead,
    leftover: BytesMut,
}

#[derive(Debug, thiserror::Error)]
enum AttemptFailure {
    /// The upstream may have received the whole request and acted on it, so it must not be sent again.
    #[error("the response was lost: {0}")]
    Lost(String),
    /// The request was never written whole into a live connection, so nothing upstream can have acted on it.
    #[error("the request never reached the upstream: {0}")]
    Unsent(String),
}

impl Relay {
    async fn handle(&self, request: Request) -> Response {
        let request_id = new_request_id();
        let (parts, body) = request.into_parts();
        let Ok(raw) = axum::body::to_bytes(body, self.limits.max_request_bytes).await else {
            // Either the caller went past the limit or its own connection broke mid-body; either way there is
            // no complete request to relay.
            return relay_failure(&RelayError::BodyTooLarge).response(&request_id);
        };
        let route = match self.keys.authenticate(&parts.headers) {
            Ok(route) => route,
            Err(error) => return auth_failure(&error).response(&request_id),
        };
        let account_id = route.account_id.clone();
        let identity = match identify(&parts.headers, &raw) {
            Ok(identity) => identity,
            Err(error) => {
                return refused(relay_failure(&error), &request_id, &account_id);
            }
        };
        let account = match self.store.account(&account_id) {
            Ok(Some(account)) => account,
            Ok(None) => {
                tracing::error!(account = %account_id, "a route names an account the store does not have");
                return refused(ACCOUNTS_UNAVAILABLE, &request_id, &account_id);
            }
            Err(error) => {
                tracing::error!(account = %account_id, %error, "the account registry is unreadable");
                return refused(ACCOUNTS_UNAVAILABLE, &request_id, &account_id);
            }
        };
        // Admission comes before any credential work: the credential manager keeps serving paused accounts so
        // their tokens stay alive, so this is the only place that decides whether a request may be served, and
        // deciding here spares a paused account an upstream round trip.
        if !account.enabled || !admissible(account.state) {
            tracing::warn!(account = %account.id, state = ?account.state, enabled = account.enabled, "a request arrived for an account that cannot serve it");
            return refused(ACCOUNTS_UNAVAILABLE, &request_id, &account_id);
        }
        let mut credential = match self.credential(&account).await {
            Ok(credential) => credential,
            Err(failure) => return refused(failure, &request_id, &account_id),
        };
        let mut edited = raw.to_vec();
        if let Err(error) = edit_body(
            &mut edited,
            credential.account_uuid.as_deref(),
            upstream_model(&identity.model),
        ) {
            return refused(relay_failure(&error), &request_id, &account_id);
        }
        let edited = Bytes::from(edited);
        let host = INFERENCE_HOST.to_string();
        let Some(addr) = (self.resolver)(&host) else {
            tracing::warn!(host = %host, "the upstream host does not resolve");
            return refused(UPSTREAM_UNREACHABLE, &request_id, &account_id);
        };
        let key = PoolKey {
            account_id: account.id.clone(),
            host: host.clone(),
        };
        let target = parts
            .uri
            .path_and_query()
            .map(|target| target.as_str())
            .unwrap_or_else(|| parts.uri.path())
            .to_string();
        tracing::debug!(
            account = %account.id,
            agent = %identity.agent_id,
            model = %identity.canonical_model,
            request = %request_id,
            session = ?identity.session_id,
            "relaying a Messages request",
        );

        let mut attempt: u8 = 1;
        let mut fresh = false;
        let mut replayed = false;
        let mut retried = false;
        loop {
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
            let sent = match self.attempt(&request, &key, &host, addr, fresh).await {
                Ok(sent) => sent,
                Err(AttemptFailure::Unsent(detail)) if !retried => {
                    tracing::debug!(account = %account.id, detail = %detail, "retrying a request that never reached the upstream");
                    attempt += 1;
                    fresh = true;
                    retried = true;
                    continue;
                }
                Err(failure) => {
                    let written = matches!(failure, AttemptFailure::Lost(_));
                    tracing::warn!(account = %account.id, %failure, "the upstream did not answer");
                    return with_outcome(
                        UPSTREAM_UNREACHABLE.response(&request_id),
                        RelayOutcome {
                            request_id,
                            account_id,
                            upstream_status: 0,
                            attempt,
                            written,
                        },
                    );
                }
            };

            // A rejected OAuth token is the one upstream answer the relay acts on itself, and only once.
            if !replayed
                && matches!(account.kind, AccountKind::Oauth)
                && matches!(sent.head.status, 401 | 403)
            {
                let Sent {
                    mut conn,
                    head,
                    leftover,
                } = sent;
                let framing = framing(&head);
                let reusable = keep_alive(&head, &framing);
                let buffered = match read_bounded(
                    &mut conn,
                    leftover,
                    framing,
                    self.limits.idle_timeout,
                )
                .await
                {
                    Ok(buffered) => buffered,
                    Err(error) => {
                        tracing::warn!(account = %account.id, %error, "the rejection body was not readable");
                        return with_outcome(
                            UPSTREAM_UNREACHABLE.response(&request_id),
                            RelayOutcome {
                                request_id,
                                account_id,
                                upstream_status: head.status,
                                attempt,
                                written: true,
                            },
                        );
                    }
                };
                if reusable {
                    self.pool.checkin(key.clone(), conn);
                }
                if is_authentication_error(&buffered) {
                    match self
                        .manager
                        .refresh_now(&account.id, credential.generation)
                        .await
                    {
                        Ok(token) => {
                            credential = Credential::from(token);
                            attempt += 1;
                            fresh = false;
                            replayed = true;
                            continue;
                        }
                        // The rejected token could not be replaced, so the upstream's own verdict is the
                        // honest answer to the caller; the account's state carries the reason for the operator.
                        Err(error) => {
                            tracing::warn!(account = %account.id, %error, "the rejected token could not be refreshed")
                        }
                    }
                }
                return forward_buffered(
                    &head,
                    buffered,
                    &parts.headers,
                    RelayOutcome {
                        request_id,
                        account_id,
                        upstream_status: head.status,
                        attempt,
                        written: true,
                    },
                );
            }

            let outcome = RelayOutcome {
                request_id,
                account_id,
                upstream_status: sent.head.status,
                attempt,
                written: true,
            };
            return self.forward(sent, &parts.headers, key, outcome);
        }
    }

    async fn credential(&self, account: &AccountRow) -> Result<Credential, Failure> {
        match account.kind {
            AccountKind::Oauth => match self.manager.acquire(&account.id).await {
                Ok(token) => Ok(Credential::from(token)),
                Err(error) => {
                    tracing::warn!(account = %account.id, %error, "no credential for the bound account");
                    Err(acquire_failure(&error))
                }
            },
            AccountKind::ApiKey => match self.store.api_key_secret(&account.id) {
                Ok(Some(secret)) => Ok(Credential {
                    account_uuid: None,
                    // An API key is never rotated by the relay, so no generation describes it.
                    generation: 0,
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

    async fn attempt(
        &self,
        request: &RawRequest,
        key: &PoolKey,
        host: &str,
        addr: SocketAddr,
        fresh: bool,
    ) -> Result<Sent, AttemptFailure> {
        // Read by the timeout arms, which cannot see how far the attempt itself got.
        let written = AtomicBool::new(false);
        match tokio::time::timeout(
            self.limits.head_timeout,
            self.attempt_once(request, key, host, addr, fresh, &written),
        )
        .await
        {
            Ok(result) => result,
            Err(_) if !written.load(Ordering::SeqCst) => Err(AttemptFailure::Unsent(
                "timed out before the request was written".into(),
            )),
            Err(_) => Err(AttemptFailure::Lost(
                "timed out waiting for the response head".into(),
            )),
        }
    }

    async fn attempt_once(
        &self,
        request: &RawRequest,
        key: &PoolKey,
        host: &str,
        addr: SocketAddr,
        fresh: bool,
        written: &AtomicBool,
    ) -> Result<Sent, AttemptFailure> {
        let (mut conn, reused) = if fresh {
            (checkout_fresh(&self.pool, key, host, addr).await?, false)
        } else {
            self.pool
                .checkout(key.clone(), host, addr)
                .await
                .map_err(unsent)?
        };
        write_request(&mut conn, request)
            .await
            .map_err(|failure| match failure.progress {
                WriteProgress::NothingWritten | WriteProgress::HeadWritten => {
                    AttemptFailure::Unsent(failure.error.to_string())
                }
                // The body is already in the socket and only the flush failed, so the upstream may well have
                // read the whole request.
                WriteProgress::BodyWritten => AttemptFailure::Lost(failure.error.to_string()),
            })?;
        written.store(true, Ordering::SeqCst);
        let mut leftover = BytesMut::new();
        match read_head(&mut conn, &mut leftover).await {
            Ok(head) => Ok(Sent {
                conn,
                head,
                leftover,
            }),
            // A pooled connection the upstream closed while it sat idle fails exactly here: the request went
            // into a socket that was already gone and not one byte came back.
            Err(error) if reused && leftover.is_empty() => {
                Err(AttemptFailure::Unsent(error.to_string()))
            }
            Err(error) => Err(AttemptFailure::Lost(error.to_string())),
        }
    }

    fn forward(
        &self,
        sent: Sent,
        caller: &HeaderMap,
        key: PoolKey,
        outcome: RelayOutcome,
    ) -> Response {
        let Sent {
            conn,
            head,
            leftover,
        } = sent;
        let (builder, decoder) = match forward_head(&head, caller) {
            Ok(parts) => parts,
            Err(error) => {
                tracing::warn!(%error, "the upstream response cannot be forwarded");
                return with_outcome(UPSTREAM_UNREACHABLE.response(&outcome.request_id), outcome);
            }
        };
        let framing = framing(&head);
        let keep = keep_alive(&head, &framing);
        let body = relay_body(Relayed {
            conn,
            decoder,
            framing,
            idle_timeout: self.limits.idle_timeout,
            keep_alive: keep,
            key,
            leftover,
            pool: self.pool.clone(),
        });
        with_outcome(built(builder, body), outcome)
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

fn decode_all(decoder: &mut dyn Decoder, raw: &[u8]) -> Result<Bytes, TransportError> {
    let mut out = decoder.feed(raw)?;
    out.extend(decoder.finish()?);
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

fn is_authentication_error(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")?
                .get("type")?
                .as_str()
                .map(str::to_string)
        })
        .is_some_and(|error_type| error_type == "authentication_error")
}

/// States a request may be served in. Listing the admissible ones rather than the refused ones means a state
/// added later is refused until someone decides otherwise.
fn admissible(state: AccountState) -> bool {
    matches!(
        state,
        AccountState::FundingRequired | AccountState::Ready | AccountState::Refreshing
    )
}

/// Local identifier for one relayed request, echoed in every local error body so a caller's failure can be
/// tied to the log line that explains it.
fn new_request_id() -> String {
    format!("req_phoebis_{:032x}", rand::random::<u128>())
}

fn refused(failure: Failure, request_id: &str, account_id: &str) -> Response {
    with_outcome(
        failure.response(request_id),
        RelayOutcome {
            request_id: request_id.to_string(),
            account_id: account_id.to_string(),
            upstream_status: 0,
            attempt: 0,
            written: false,
        },
    )
}

fn with_outcome(mut response: Response, outcome: RelayOutcome) -> Response {
    response.extensions_mut().insert(outcome);
    response
}

/// Drains the idle connections for this key until the pool opens a new one. The retry that calls this has
/// just been failed by a connection that was already idle, so handing it another one would repeat the failure.
async fn checkout_fresh(
    pool: &Pool,
    key: &PoolKey,
    host: &str,
    addr: SocketAddr,
) -> Result<Conn, AttemptFailure> {
    loop {
        let (conn, reused) = pool
            .checkout(key.clone(), host, addr)
            .await
            .map_err(unsent)?;
        if !reused {
            return Ok(conn);
        }
        drop(conn);
    }
}

fn unsent(error: TransportError) -> AttemptFailure {
    AttemptFailure::Unsent(error.to_string())
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
