//! The forward of a feature call the checks admitted: the body read whole under the model route's bounds, the head as
//! the caller sent it with the feature account's bearer in the caller's slot, and the answer handed back piece by piece
//! under the relay's filter. No answer reaches the account's routing state.

use axum::body::Body;
use axum::http::request::Parts;
use axum::http::response::Builder;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use phoebis_oauth::AcquireError;
use phoebis_profile::{FeatureRow, Plane};
use phoebis_routing::{Obstacle, Report};
use phoebis_store::{AccountRow, ApiKeyRow};
use phoebis_transport::control::{ControlAnswer, StreamFailure};
use phoebis_transport::pool::{Conn, PoolKey};
use phoebis_transport::request::RawRequest;
use phoebis_transport::response::{framing, keep_alive, BodyReader, ResponseHead};
use tokio_util::sync::CancellationToken;

use crate::budget::Reservation;
use crate::casing::OriginalHead;
use crate::dispatch::{attempt, AttemptFailure, Sent};
use crate::errors::{
    body_budget_exhausted, feature_account_unavailable, key_body_reads, relay_failure, Failure,
};
use crate::headers::feature_headers;
use crate::messages::{content_length, Relay, NOT_FORWARDED};
use crate::stream::Checkin;
use crate::RelayError;

/// Why a forward handed back no answer of the upstream's.
pub(crate) enum Unanswered {
    /// The manager refused the account's token for a state only the operator cures.
    Refused(Failure),
    /// A failure ahead of the upstream's answer head, by the word the DEBUG line names its case with, and the text a
    /// pooled connection's loss carries, since that one word takes in a cut, a late head and an unreadable one.
    Failed(&'static str, Option<String>),
}

/// The body read whole under the model route's four bounds: the key's count of arriving bodies, the size, the
/// process's byte budget and the read's deadline. The reservation counts the body's bytes in the budget for as long as
/// the caller holds it. A head with neither `Content-Length` nor `Transfer-Encoding` frames no body, so it answers
/// `None` and takes no place in any of the bounds.
pub(crate) async fn read_body(
    relay: &Relay,
    key: &ApiKeyRow,
    headers: &HeaderMap,
    body: Body,
) -> Result<Option<(Bytes, Reservation)>, Failure> {
    let framed = [header::CONTENT_LENGTH, header::TRANSFER_ENCODING]
        .iter()
        .any(|name| headers.contains_key(name));
    if !framed {
        return Ok(None);
    }
    let Some(permit) = relay
        .body_reads
        .try_acquire(&key.id, relay.limits.key_body_reads)
    else {
        return Err(key_body_reads());
    };
    let bound = relay.limits.max_request_bytes;
    let declared = content_length(headers);
    if declared.is_some_and(|length| length > bound) {
        return Err(relay_failure(&RelayError::BodyTooLarge));
    }
    // A chunked body may run to the bound, so it holds that much until it has been read.
    let Some(mut reservation) = relay
        .body_budget
        .try_reserve(declared.unwrap_or(bound) as u64)
    else {
        return Err(body_budget_exhausted());
    };
    let read = tokio::time::timeout(
        relay.limits.body_read_timeout,
        axum::body::to_bytes(body, bound),
    )
    .await;
    drop(permit);
    let raw = match read {
        Ok(Ok(raw)) => raw,
        Err(_) => return Err(relay_failure(&RelayError::BodyTimeout)),
        Ok(Err(error)) => {
            // axum wraps the bound and the body's own failure in one type, and the bound alone is a body too large.
            let too_large = error
                .into_inner()
                .downcast::<http_body_util::LengthLimitError>()
                .is_ok();
            return Err(relay_failure(if too_large {
                &RelayError::BodyTooLarge
            } else {
                &RelayError::BodyIncomplete
            }));
        }
    };
    // The declared length frames the body and the bound caps a chunked one, so this only shrinks.
    let shrunk = reservation.resize(raw.len() as u64);
    debug_assert!(shrunk.is_ok(), "the read outgrew its reservation");
    Ok(Some((raw, reservation)))
}

/// Sends the admitted call under `account`'s token on the row's plane and hands back the upstream's answer, its body
/// streaming as it arrives: on the control plane over a fresh connection, and on the inference plane over the
/// account's pooled connections toward the row's host. `body` is `None` for a head that framed none, which then leaves
/// with no length line, and a framed body, an empty one among them, keeps its line.
pub(crate) async fn forward(
    relay: &Relay,
    account: &AccountRow,
    row: &FeatureRow,
    method: &'static str,
    parts: &Parts,
    body: Option<Bytes>,
) -> Result<Response, Unanswered> {
    let control = match row.plane {
        Plane::Control => match relay.control.as_ref() {
            Some(control) => Some(control),
            None => return Err(Unanswered::Failed("unreachable", None)),
        },
        Plane::Inference => None,
    };
    let token = match relay.manager.acquire_for(account, None).await {
        Ok(token) => token,
        Err(error) => {
            // The manager's store write starts no reload, so the mark reaches the routing through this report.
            if let Some(mark) = error.landed_mark() {
                relay.router().report(Report::from(mark)).await;
            }
            return Err(match error {
                AcquireError::Disconnected
                | AcquireError::Quarantined
                | AcquireError::ReauthRequired(_)
                | AcquireError::Unpaired => {
                    Unanswered::Refused(feature_account_unavailable(Obstacle::Unready.name()))
                }
                AcquireError::NotRefreshed
                | AcquireError::ShuttingDown
                | AcquireError::Store(_)
                | AcquireError::Unavailable(_) => Unanswered::Failed("refresh_failed", None),
            });
        }
    };
    let headers = feature_headers(
        &parts.headers,
        parts.extensions.get::<OriginalHead>(),
        Some(&token.secret),
        row.host,
        row.plane,
        body.as_ref().map(Bytes::len),
    );
    let target = parts
        .uri
        .path_and_query()
        .map_or("/", |target| target.as_str())
        .to_owned();
    let request = RawRequest {
        method,
        target,
        headers,
        body: body.unwrap_or_default(),
    };
    let Some(control) = control else {
        return pooled(relay, account, row.host, &request).await;
    };
    match control
        .send_streaming(
            request,
            row.host,
            relay.limits.connect_timeout,
            relay.limits.head_timeout,
        )
        .await
    {
        Ok(answer) => Ok(answered(answer)),
        Err(failure) => Err(Unanswered::Failed(case_of(&failure), None)),
    }
}

/// Sends the call over `account`'s pooled connections toward `host`, as the model path sends a request, under the
/// connect and head bounds `attempt` holds. A reused connection that died before the request was in it whole is tried
/// once more on a fresh one. Nothing else is sent twice: the upstream may have acted on a request that was written.
async fn pooled(
    relay: &Relay,
    account: &AccountRow,
    host: &'static str,
    request: &RawRequest,
) -> Result<Response, Unanswered> {
    let Some(addr) = (relay.resolver)(host).await else {
        return Err(Unanswered::Failed("unreachable", None));
    };
    let key = PoolKey {
        account_id: account.id.clone(),
        host: host.to_owned(),
    };
    // A feature call is placed by no routing selection, so no account's token is there to end it: a stop of the
    // account's features refuses its next call, and a stream in flight runs to its own end.
    let never = CancellationToken::new();
    let mut fresh = false;
    loop {
        match attempt(relay, request, &key, addr, fresh, &never).await {
            Ok(sent) => {
                let checkin = Checkin {
                    pool: relay.pool.clone(),
                    key,
                };
                return Ok(pooled_answer(sent, checkin));
            }
            Err(AttemptFailure::Unsent { connect: false, .. }) if !fresh => fresh = true,
            Err(AttemptFailure::Unsent { .. }) => {
                return Err(Unanswered::Failed("unreachable", None));
            }
            Err(AttemptFailure::Lost(text)) => return Err(Unanswered::Failed("cut", Some(text))),
            // The token above never fires, so this cannot arise, and it answers as a loss would.
            Err(AttemptFailure::Canceled) => return Err(Unanswered::Failed("cut", None)),
        }
    }
}

/// The answer of a pooled call, its body passed on as on the control plane. The connection goes back to the pool
/// once the body ended and the head lets it be kept. It goes with the stream where the caller left or a read failed,
/// since a half-read connection would hand the account's next call the rest of this one.
fn pooled_answer(sent: Sent<Conn>, checkin: Checkin) -> Response {
    let Sent {
        conn,
        head,
        leftover,
        ..
    } = sent;
    let framing = framing(&head);
    let kept = keep_alive(&head, &framing).then_some(checkin);
    let reader = BodyReader::new(conn, leftover, framing);
    let pieces = futures::stream::unfold(Some((reader, kept)), |state| async move {
        let (mut reader, kept) = state?;
        match reader.next_chunk().await {
            Ok(Some(piece)) => Some((Ok(piece), Some((reader, kept)))),
            Ok(None) => {
                if let Some(checkin) = kept {
                    checkin.put(reader.into_inner());
                }
                None
            }
            Err(error) => Some((Err(error), None)),
        }
    });
    answer_head(&head)
        .body(Body::from_stream(pieces))
        // The status is a validated `StatusCode` and each line was parsed before it was added.
        .expect("a forwarded answer's head is always well formed")
}

/// The head a caller gets: the upstream's status and each of its lines whose name `NOT_FORWARDED` does not hold.
/// It adds no line.
pub(crate) fn answer_head(head: &ResponseHead) -> Builder {
    let status = StatusCode::from_u16(head.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(status);
    for (name, value) in &head.headers {
        if NOT_FORWARDED
            .iter()
            .any(|kept| name.eq_ignore_ascii_case(kept))
        {
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
    builder
}

/// The answer with each piece of its body passed on as it arrives and no bound between two, since a session's event
/// stream idles between its events. A read that fails behind the head ends the stream with an error, which ends the
/// caller's connection.
fn answered(answer: ControlAnswer) -> Response {
    let ControlAnswer { head, body } = answer;
    let pieces = futures::stream::unfold(Some(body), |reader| async move {
        let mut reader = reader?;
        match reader.next_chunk().await {
            Ok(Some(piece)) => Some((Ok(piece), Some(reader))),
            Ok(None) => None,
            Err(error) => Some((Err(error), None)),
        }
    });
    answer_head(&head)
        .body(Body::from_stream(pieces))
        // The status is a validated `StatusCode` and each line was parsed before it was added.
        .expect("a forwarded answer's head is always well formed")
}

/// The word of a failed send's case, which the answer does not tell apart.
fn case_of(failure: &StreamFailure) -> &'static str {
    match failure {
        StreamFailure::Cut(_) => "cut",
        StreamFailure::Stalled => "stalled",
        StreamFailure::Undecodable(_) => "undecodable",
        StreamFailure::Unreachable(_) => "unreachable",
    }
}
