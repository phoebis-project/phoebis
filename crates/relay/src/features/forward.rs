//! The forward of a feature call the checks admitted: the body read whole under the model route's bounds, the head as
//! the caller sent it with the feature account's bearer in the caller's slot, and the answer handed back piece by piece
//! under the relay's filter. No answer reaches the account's routing state. The domain check leaves here too, with no
//! credential and under its one deadline. Each call that went upstream publishes its end through one guard, whichever
//! plane carried it.

use std::io;

use axum::body::Body;
use axum::http::request::Parts;
use axum::http::{header, HeaderMap};
use axum::response::Response;
use bytes::Bytes;
use phoebis_bus::{Bus, Event, FeatureCall, FeatureEnd, FeatureOutcome};
use phoebis_oauth::{AcquireError, Clock};
use phoebis_profile::{FeatureRow, Plane};
use phoebis_routing::{Obstacle, UNATTRIBUTED_POOL};
use phoebis_store::{AccountRow, ApiKeyRow, Secret};
use phoebis_transport::control::{ControlAnswer, ControlClient, StreamFailure};
use phoebis_transport::pool::{Conn, PoolKey};
use phoebis_transport::request::RawRequest;
use phoebis_transport::response::{framing, keep_alive, BodyReader, Framing, ResponseHead};
use phoebis_transport::TransportError;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::cap::Place;
use super::refresh_hold::Rejected;
use super::Listener;
use crate::budget::Reservation;
use crate::casing::OriginalHead;
use crate::dispatch::{attempt, AttemptFailure, Sent};
use crate::errors::{
    feature_account_unavailable, key_body_reads, relay_failure, Failure, UPSTREAM_UNREACHABLE,
};
use crate::headers::feature_headers;
use crate::messages::{
    built, copied_head, publish_from_drop, read_outcome, report_landed, target_of, Relay,
};
use crate::stream::Checkin;

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
    let mut reservation = relay.reserve_body(headers)?;
    let read = tokio::time::timeout(
        relay.limits.body_read_timeout,
        axum::body::to_bytes(body, relay.limits.max_request_bytes),
    )
    .await;
    drop(permit);
    let raw = read_outcome(read).map_err(|error| relay_failure(&error))?;
    // The declared length frames the body and the bound caps a chunked one, so this only shrinks.
    let shrunk = reservation.resize(raw.len() as u64);
    debug_assert!(shrunk.is_ok(), "the read outgrew its reservation");
    Ok(Some((raw, reservation)))
}

/// Sends the admitted call under `account`'s token on the row's plane and hands back the upstream's answer, its body
/// streaming as it arrives: on the control plane over a fresh connection, and on the inference plane over the
/// account's pooled connections toward the row's host. `body` is `None` for a head that framed none, which then leaves
/// with no length line, and a framed body, an empty one among them, keeps its line. A 401 that `wants_refresh` names
/// gets the account's one forced refresh and the call once more under the new token, and that answer goes back
/// whatever it is, while a refresh that gives no newer token sends the first 401 back as it came. A 401 under the
/// token the listener's holds keep for the account goes back as it came too, and a resend's own such 401 holds the new
/// token there. `ending` arms at the first send and ends with the answer that goes back, or as unanswered where the
/// forward fails ahead of a head.
pub(crate) async fn forward(
    listener: &Listener,
    account: &AccountRow,
    row: &FeatureRow,
    method: &'static str,
    parts: &Parts,
    body: Option<Bytes>,
    mut ending: Ending,
) -> Result<Response, Unanswered> {
    match answer_of(listener, account, row, method, parts, body, &mut ending).await {
        Ok(held) => Ok(held.streamed(ending)),
        Err(unanswered) => {
            if let Unanswered::Failed(..) = unanswered {
                ending.unanswered();
            }
            Err(unanswered)
        }
    }
}

/// The answer that goes back to the caller, its body still on its connection.
async fn answer_of(
    listener: &Listener,
    account: &AccountRow,
    row: &FeatureRow,
    method: &'static str,
    parts: &Parts,
    body: Option<Bytes>,
    ending: &mut Ending,
) -> Result<Held, Unanswered> {
    let Listener {
        control,
        holds,
        relay,
        ..
    } = listener;
    let token = match relay.manager.acquire_for(account, None).await {
        Ok(token) => token,
        Err(error) => {
            report_landed(relay.router(), &error).await;
            return Err(match error {
                AcquireError::Disconnected
                | AcquireError::Quarantined
                | AcquireError::ReauthRequired(_)
                | AcquireError::Unpaired => {
                    Unanswered::Refused(feature_account_unavailable(&Obstacle::Unready))
                }
                AcquireError::NotRefreshed
                | AcquireError::ShuttingDown
                | AcquireError::Store(_)
                | AcquireError::Unavailable(_) => Unanswered::Failed("refresh_failed", None),
            });
        }
    };
    let target = target_of(parts);
    let length = body.as_ref().map(Bytes::len);
    let body = body.unwrap_or_default();
    let request = |bearer: &Secret| RawRequest {
        method,
        target: target.clone(),
        headers: feature_headers(
            &parts.headers,
            parts.extensions.get::<OriginalHead>(),
            Some(bearer),
            row.host,
            row.plane,
            length,
        ),
        body: body.clone(),
    };
    ending.arm();
    let first = send(
        relay,
        control,
        row.plane,
        account,
        row.host,
        request(&token.secret),
    )
    .await?;
    // A token the upstream rejected behind its own refresh gets no second one. The call went out once, since the
    // upstream can accept the token on another path, and its 401 goes back as it came.
    if !wants_refresh(first.head())
        || holds.holds(&account.id, Rejected::of(account, token.generation))
    {
        return Ok(first);
    }
    // The first answer stays unread while the refresh runs, so where no newer token comes it goes back as it came.
    match relay
        .manager
        .refresh_now(&account.id, token.generation)
        .await
    {
        Ok(fresh) => {
            // Dropped ahead of the resend, its connection with it, since a connection whose body went unread serves no
            // later call.
            drop(first);
            let second = send(
                relay,
                control,
                row.plane,
                account,
                row.host,
                request(&fresh.secret),
            )
            .await?;
            // A resend's 401 alone holds its token: a first 401 whose refresh gave no token and a resend the upstream
            // accepted each leave the next 401 its refresh.
            if wants_refresh(second.head()) {
                holds.set(&account.id, Rejected::of(account, fresh.generation));
            }
            Ok(second)
        }
        Err(error) => {
            report_landed(relay.router(), &error).await;
            Ok(first)
        }
    }
}

/// Sends a call the listener serves without a key on the control plane, with no credential line, and outside the
/// refresh and the resend, since no account stands behind it: its 401 goes back as it came. `deadline` bounds the send
/// exactly, with the connect and the head bounds each capped at the time left, and then each read of the answer's body.
/// `ending` arms at the send and ends as `answered` says, or as unanswered where no head came. The error is the word
/// of the case in which no head came.
pub(crate) async fn keyless(
    listener: &Listener,
    host: &'static str,
    method: &'static str,
    parts: &Parts,
    deadline: Instant,
    mut ending: Ending,
) -> Result<Response, &'static str> {
    let Listener { control, relay, .. } = listener;
    // The shape admits an empty body alone, and a head that framed it by its length keeps that line, as a keyed
    // forward writes it.
    let length = parts
        .headers
        .contains_key(header::CONTENT_LENGTH)
        .then_some(0);
    let request = RawRequest {
        method,
        target: target_of(parts),
        headers: feature_headers(
            &parts.headers,
            parts.extensions.get::<OriginalHead>(),
            None,
            host,
            Plane::Control,
            length,
        ),
        body: Bytes::new(),
    };
    let left = deadline.saturating_duration_since(Instant::now());
    let sent = control.send_streaming(
        request,
        host,
        relay.limits.connect_timeout.min(left),
        relay.limits.head_timeout.min(left),
    );
    ending.arm();
    let case = match tokio::time::timeout_at(deadline, sent).await {
        Ok(Ok(ControlAnswer { head, body })) => {
            return Ok(answered(head, body, None, Some(deadline), ending))
        }
        Ok(Err(failure)) => case_of(&failure),
        // No head came in the call's time: the connect and the head bounds are each capped at the time left, so
        // together they could run to about twice it, and this timer holds the call's one limit over both.
        Err(_) => "stalled",
    };
    ending.unanswered();
    Err(case)
}

/// Whether an answer asks for the account's credential once more: a 401 without `X-Mcp-Error-Code`. A connector that
/// wants its own sign-in names it in that line, and the client acts on that answer itself.
fn wants_refresh(head: &ResponseHead) -> bool {
    head.status == 401 && head.get("x-mcp-error-code").is_none()
}

/// An answer whose head arrived and whose body is still on its connection.
enum Held {
    Control(ControlAnswer),
    Pooled(Sent<Conn>, Checkin),
}

impl Held {
    fn head(&self) -> &ResponseHead {
        match self {
            Held::Control(answer) => &answer.head,
            Held::Pooled(sent, _) => &sent.head,
        }
    }

    /// The answer handed back under `ending`, whichever plane it came on. A pooled connection goes back to the pool
    /// once the body ended and the head lets it be kept.
    fn streamed(self, ending: Ending) -> Response {
        let (head, body, kept) = match self {
            Held::Control(ControlAnswer { head, body }) => (head, body, None),
            Held::Pooled(
                Sent {
                    conn,
                    head,
                    leftover,
                    ..
                },
                checkin,
            ) => {
                let framing = framing(&head);
                let kept = keep_alive(&head, &framing).then_some(checkin);
                (head, BodyReader::new(conn, leftover, framing), kept)
            }
        };
        answered(head, body, kept, None, ending)
    }
}

/// One send of the call on `plane`: through `control` on the control plane, else over the account's pooled
/// connections.
async fn send(
    relay: &Relay,
    control: &ControlClient,
    plane: Plane,
    account: &AccountRow,
    host: &'static str,
    request: RawRequest,
) -> Result<Held, Unanswered> {
    match plane {
        Plane::Inference => pooled(relay, account, host, &request).await,
        Plane::Control => control
            .send_streaming(
                request,
                host,
                relay.limits.connect_timeout,
                relay.limits.head_timeout,
            )
            .await
            .map(Held::Control)
            .map_err(|failure| Unanswered::Failed(case_of(&failure), None)),
    }
}

/// Sends the call over `account`'s pooled connections toward `host`, as the model path sends a request, under the
/// connect and head bounds `attempt` holds. A reused connection that died before the request was in it whole is tried
/// once more on a fresh one. A connection that could not be opened answers the 502 at once, with no wait and no second
/// try as the model path makes, since a feature call has one account and no failover to go on with, and the client can
/// send the call again. This function sends nothing else twice, since the upstream may have acted on a request that was
/// written. The resend after a 401 is `forward`'s.
async fn pooled(
    relay: &Relay,
    account: &AccountRow,
    host: &'static str,
    request: &RawRequest,
) -> Result<Held, Unanswered> {
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
                return Ok(Held::Pooled(sent, checkin));
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

/// The answer with each piece of its body passed on as it arrives and no bound between two, since a session's event
/// stream idles between its events. A read that fails behind the head ends the stream with an error, which ends the
/// caller's connection, and so does a read still waiting at `deadline` where the call has one. `kept` takes the pooled
/// connection back once the body ended whole. It goes with the stream where the caller left or a read failed, since a
/// half-read connection would hand the account's next call the rest of this one. `ending` takes the head's status and
/// ends with the stream: `forwarded` at the body's end, `failed` at a failed read, and `canceled` where the caller left
/// first. An answer whose framing holds no body ends `forwarded` here, since the server drops such a body unpolled.
fn answered(
    head: ResponseHead,
    body: BodyReader<Conn>,
    kept: Option<Checkin>,
    deadline: Option<Instant>,
    mut ending: Ending,
) -> Response {
    ending.went_back(head.status);
    if let Framing::ContentLength(0) = framing(&head) {
        ending.end(FeatureOutcome::Forwarded);
    }
    let state = Some((body, kept, ending));
    let pieces = futures::stream::unfold(state, move |state| async move {
        let (mut reader, kept, mut ending) = state?;
        let read = match deadline {
            Some(deadline) => tokio::time::timeout_at(deadline, reader.next_chunk())
                .await
                .unwrap_or_else(|_| Err(TransportError::Io(io::ErrorKind::TimedOut.into()))),
            None => reader.next_chunk().await,
        };
        match read {
            Ok(Some(piece)) => Some((Ok(piece), Some((reader, kept, ending)))),
            Ok(None) => {
                if let Some(checkin) = kept {
                    checkin.put(reader.into_inner());
                }
                ending.end(FeatureOutcome::Forwarded);
                None
            }
            Err(error) => {
                ending.end(FeatureOutcome::Failed);
                Some((Err(error), None))
            }
        }
    });
    built(copied_head(&head, |_| false), Body::from_stream(pieces))
}

/// A feature endpoint request as its event names it, taken at its arrival. The key and the feature are set as the
/// checks read them.
#[derive(Clone)]
pub(crate) struct Call {
    pub(crate) feature: Option<&'static str>,
    pub(crate) id: String,
    pub(crate) key_id: Option<String>,
    pub(crate) method: String,
    /// The path without its query, since a query can name what the caller looked for.
    pub(crate) path: String,
    pub(crate) received_at: i64,
}

impl Call {
    /// The event of the call's `end` at `now`, on the relay's clock.
    pub(crate) fn ended(self, end: FeatureEnd, now: i64) -> Event {
        Event::FeatureCall(FeatureCall {
            duration_ms: u64::try_from(now - self.received_at).unwrap_or(0),
            end,
            id: self.id,
            method: self.method,
            path: self.path,
            received_at: self.received_at,
        })
    }
}

/// The end of a call that went upstream, published once on the bus: with the outcome its answer's stream decides, as
/// unanswered where no upstream head came, or as `canceled` where it is dropped while armed, as when the caller left.
/// It arms at the call's send, so a caller that leaves ahead of it leaves no end. A domain check's place rides in it to
/// the end, and the check's DEBUG line reads the outcome it decides.
pub(crate) struct Ending(Option<Open>);

struct Open {
    armed: bool,
    bodies: TaskTracker,
    bus: Option<Bus>,
    call: Call,
    clock: Clock,
    kind: Kind,
    /// The status of the upstream's head that the listener passed on, so a 401 a forced refresh dropped never reaches
    /// it.
    status: Option<u16>,
}

enum Kind {
    DomainCheck(Place),
    Sent {
        account_id: String,
        feature: &'static str,
        key_id: String,
    },
}

impl Ending {
    /// A call of `feature` under the key `key_id` and `account_id`'s token.
    pub(crate) fn sent(
        relay: &Relay,
        call: Call,
        account_id: String,
        feature: &'static str,
        key_id: String,
    ) -> Ending {
        Ending::new(
            relay,
            call,
            Kind::Sent {
                account_id,
                feature,
                key_id,
            },
        )
    }

    /// The domain check, which holds `place` of the cap until its end.
    pub(crate) fn domain_check(relay: &Relay, call: Call, place: Place) -> Ending {
        Ending::new(relay, call, Kind::DomainCheck(place))
    }

    fn new(relay: &Relay, call: Call, kind: Kind) -> Ending {
        Ending(Some(Open {
            armed: false,
            bodies: relay.bodies.clone(),
            bus: relay.bus.clone(),
            call,
            clock: relay.clock.clone(),
            kind,
            status: None,
        }))
    }

    fn arm(&mut self) {
        if let Some(open) = &mut self.0 {
            open.armed = true;
        }
    }

    fn went_back(&mut self, status: u16) {
        if let Some(open) = &mut self.0 {
            open.status = Some(status);
        }
    }

    /// The end of a call no upstream head answered, which the 502 answers. It counts armed or not, since the token
    /// manager's own failure answers that 502 too.
    fn unanswered(&mut self) {
        if let Some(open) = self.0.take() {
            open.publish(FeatureOutcome::Failed, Some(UPSTREAM_UNREACHABLE.code));
        }
    }

    /// The end the answer read. A later end and the drop publish nothing.
    fn end(&mut self, outcome: FeatureOutcome) {
        if let Some(open) = self.0.take() {
            open.ended_line(outcome);
            open.publish(outcome, None);
        }
    }
}

impl Drop for Ending {
    fn drop(&mut self) {
        if let Some(open) = self.0.take().filter(|open| open.armed) {
            open.ended_line(FeatureOutcome::Canceled);
            open.publish(FeatureOutcome::Canceled, None);
        }
    }
}

impl Open {
    /// The domain check's DEBUG line at its end, with the path alone since the query names the domain. A keyed call
    /// writes none at its end.
    fn ended_line(&self, outcome: FeatureOutcome) {
        if let Kind::DomainCheck(_) = self.kind {
            tracing::debug!(
                outcome = outcome.as_str(),
                method = self.call.method.as_str(),
                path = self.call.path.as_str(),
                key = UNATTRIBUTED_POOL,
                "a domain check ended"
            );
        }
    }

    fn publish(self, outcome: FeatureOutcome, error_code: Option<&'static str>) {
        let Open {
            bodies,
            bus,
            call,
            clock,
            kind,
            status,
            ..
        } = self;
        let end = match kind {
            Kind::DomainCheck(place) => {
                drop(place);
                FeatureEnd::DomainCheck { outcome }
            }
            Kind::Sent {
                account_id,
                feature,
                key_id,
            } => FeatureEnd::Sent {
                account_id,
                error_code,
                feature,
                key_id,
                outcome,
                status,
            },
        };
        let Some(bus) = bus else {
            return;
        };
        let event = call.ended(end, clock());
        publish_from_drop(&bodies, bus, event, false);
    }
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
