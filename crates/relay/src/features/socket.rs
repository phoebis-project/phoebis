//! A socket through the feature listener: the shape of a socket request, the identity a socket was admitted with and
//! the checks that read it, the send of the upgrade request, the `101` that goes back, and the tunnel of bytes the
//! connection's own task carries behind it. Nothing here names how a feature admits its sockets.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Uri};
use axum::response::Response;
use bytes::{Bytes, BytesMut};
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use phoebis_bus::FeatureOutcome;
use phoebis_profile::{FeatureTable, Plane, SocketRow};
use phoebis_routing::{feature_ready, Obstacle, RegistrySnapshot};
use phoebis_store::{AccountRow, ApiKeyRow, Feature};
use phoebis_transport::control::{ControlAnswer, UpgradeAnswer};
use phoebis_transport::pool::Conn;
use phoebis_transport::request::RawRequest;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::watch;
use tokio::time::{Instant, MissedTickBehavior};

use super::forward::{answered, case_of, Call, Ending};
use super::{no_body, plain_path, Listener};
use crate::casing::{HeadRecorder, OriginalHead};
use crate::errors::UPSTREAM_UNREACHABLE;
use crate::headers::{feature_headers, Hop};
use crate::messages::{built, copied_head, target_of, Relay};
use crate::serve::{stopped, Accepted};

/// The most one read of either leg hands over: the payload of one TLS record.
const COPY_BUFFER: usize = 16 * 1024;

/// The identity a socket was admitted with. The admission and each later check read it against one snapshot.
#[derive(Clone, Debug)]
pub(crate) struct Admitted {
    pub(crate) account_id: String,
    pub(crate) feature: Feature,
    pub(crate) key_generation: i64,
    pub(crate) key_id: String,
    /// The account's `(incarnation, lifecycle_generation)`, as `AccountRow` holds both and as `AccessToken` pairs them.
    pub(crate) sign_in: (u64, u64),
}

/// The check an identity failed.
pub(crate) enum Unheld<'a> {
    /// `feature_ready` refuses the account. The key's row stands, for the refusal's label.
    Account(&'a ApiKeyRow, Obstacle),
    Key,
    Selection,
    SignIn,
}

impl Unheld<'_> {
    /// The failed check's word in the tunnel's line: `account`, `key`, `selection` or `sign_in`.
    pub(crate) fn word(&self) -> &'static str {
        match self {
            Unheld::Account(..) => "account",
            Unheld::Key => "key",
            Unheld::Selection => "selection",
            Unheld::SignIn => "sign_in",
        }
    }
}

/// The checks in their order: the key, the selection, the sign-in pair, `feature_ready`.
pub(crate) fn held<'a>(
    snapshot: &'a RegistrySnapshot,
    admitted: &Admitted,
    now: i64,
) -> Result<(&'a ApiKeyRow, &'a AccountRow), Unheld<'a>> {
    let key = snapshot
        .authenticate_id(&admitted.key_id, admitted.key_generation, now)
        .map_err(|_| Unheld::Key)?;
    if snapshot.feature_account(key, admitted.feature) != Some(admitted.account_id.as_str()) {
        return Err(Unheld::Selection);
    }
    let signed_in = snapshot
        .account(&admitted.account_id)
        .is_some_and(|row| (row.incarnation, row.lifecycle_generation) == admitted.sign_in);
    if !signed_in {
        return Err(Unheld::SignIn);
    }
    let account = feature_ready(snapshot, &admitted.account_id, now)
        .map_err(|obstacle| Unheld::Account(key, obstacle))?;
    Ok((key, account))
}

/// A failed check without the snapshot it was read on, for a caller that refuses behind the snapshot's end: the key's
/// label and the obstacle where the account is the cause. The key, the selection and the sign-in are one cause to the
/// caller, whose refusal reads as a request without a key.
pub(crate) enum Lapsed {
    Account(String, Obstacle),
    Identity,
}

impl From<Unheld<'_>> for Lapsed {
    fn from(unheld: Unheld<'_>) -> Lapsed {
        match unheld {
            Unheld::Account(key, obstacle) => Lapsed::Account(key.label.clone(), obstacle),
            Unheld::Key | Unheld::Selection | Unheld::SignIn => Lapsed::Identity,
        }
    }
}

/// The socket row a request falls under: a `GET` whose host and plain path equal a row's, whose head asks for a
/// WebSocket and declares no body.
pub(crate) fn row_of(
    table: &'static FeatureTable,
    method: &str,
    host: Option<&str>,
    uri: &Uri,
    headers: &HeaderMap,
) -> Option<&'static SocketRow> {
    if method != "GET" {
        return None;
    }
    let host = host?;
    let path = plain_path(uri.path())?;
    let row = table
        .sockets
        .iter()
        .find(|row| row.host == host && row.path.as_bytes() == path.as_slice())?;
    let websocket = lists(headers, header::UPGRADE, |protocol| {
        // A protocol names its version behind a slash, which the shape does not read.
        let name = protocol.split('/').next().unwrap_or(protocol);
        name.eq_ignore_ascii_case("websocket")
    });
    let upgrade = lists(headers, header::CONNECTION, |option| {
        option.eq_ignore_ascii_case("upgrade")
    });
    (websocket && upgrade && no_body(headers)).then_some(row)
}

/// Whether one item of the comma lists of `name`'s lines holds for `item`.
fn lists(headers: &HeaderMap, name: HeaderName, item: impl Fn(&str) -> bool) -> bool {
    headers.get_all(name).iter().any(|line| {
        line.to_str()
            .is_ok_and(|line| line.split(',').map(str::trim).any(&item))
    })
}

/// The open sockets, by key and in total.
#[derive(Default)]
pub(crate) struct Sockets(Mutex<Counts>);

#[derive(Default)]
struct Counts {
    keys: HashMap<String, usize>,
    total: usize,
}

/// The count that had no place.
pub(crate) enum Full {
    Key,
    Total,
}

impl Sockets {
    /// A place in both counts, without a wait. The key's count is read first. The place keeps a clone of the handle.
    pub(crate) fn try_take(
        self: &Arc<Sockets>,
        key_id: &str,
        key_bound: usize,
        total_bound: usize,
    ) -> Result<SocketPlace, Full> {
        let mut counts = self.0.lock().expect("socket counts poisoned");
        let held = counts.keys.get(key_id).copied().unwrap_or(0);
        if held >= key_bound {
            return Err(Full::Key);
        }
        if counts.total >= total_bound {
            return Err(Full::Total);
        }
        counts.keys.insert(key_id.to_owned(), held + 1);
        counts.total += 1;
        Ok(SocketPlace {
            key_id: key_id.to_owned(),
            sockets: Arc::clone(self),
        })
    }
}

/// One socket's place in both counts, freed on drop.
pub(crate) struct SocketPlace {
    key_id: String,
    sockets: Arc<Sockets>,
}

impl Drop for SocketPlace {
    fn drop(&mut self) {
        let mut counts = self.sockets.0.lock().expect("socket counts poisoned");
        counts.total -= 1;
        if let Some(held) = counts.keys.get_mut(&self.key_id) {
            *held -= 1;
            if *held == 0 {
                counts.keys.remove(&self.key_id);
            }
        }
    }
}

/// The send of an admitted socket request and its answer. Ahead of the answer to a `101` the identity is checked once
/// more, through `held` on a snapshot of that moment. A failed check drops the upstream connection, withdraws the
/// call's end and hands the call back with the check, for the refusal its caller answers. `place` goes into the tunnel
/// at a `101` that passed its check and frees at the return on each other road, ahead of the answer.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn open(
    listener: &Listener,
    row: &'static SocketRow,
    admitted: Admitted,
    key: &ApiKeyRow,
    account: &AccountRow,
    mut parts: Parts,
    call: Call,
    place: SocketPlace,
) -> Result<Response, (Call, Lapsed)> {
    let Listener { control, relay, .. } = listener;
    let slot = parts.extensions.get::<Handover>().cloned();
    let (Some(slot), Some(client)) = (slot, parts.extensions.remove::<OnUpgrade>()) else {
        // Each connection of the feature listener carries both, so this names a request that reached the handler
        // some other way, and nothing goes upstream for it.
        tracing::debug!(
            outcome = "failed",
            method = %call.method,
            path = call.path.as_str(),
            key = %key.label,
            case = "unupgradable",
            "a feature call failed ahead of the upstream's answer"
        );
        Ending::sent(
            relay,
            call.clone(),
            account.id.clone(),
            row.feature,
            key.id.clone(),
        )
        .unanswered();
        return Ok(UPSTREAM_UNREACHABLE.response(&call.id));
    };
    // The shape admits an empty body alone, and a head that framed it by its length keeps that line.
    let length = parts
        .headers
        .contains_key(header::CONTENT_LENGTH)
        .then_some(0);
    let request = RawRequest {
        method: "GET",
        target: target_of(&parts),
        headers: feature_headers(
            &parts.headers,
            parts.extensions.get::<OriginalHead>(),
            None,
            row.host,
            Hop::Socket,
            length,
        ),
        body: Bytes::new(),
    };
    let mut ending = Ending::sent(
        relay,
        call.clone(),
        account.id.clone(),
        row.feature,
        key.id.clone(),
    );
    ending.arm();
    let (connect, head) = (relay.limits.connect_timeout, relay.limits.head_timeout);
    // A fresh connection on either plane, which no pool takes back: the socket's protocol owns it from the `101`.
    let sent = match row.plane {
        Plane::Inference => {
            relay
                .pool
                .send_upgrade(&relay.resolver, request, row.host, connect, head)
                .await
        }
        Plane::Control => control.send_upgrade(request, row.host, connect, head).await,
    };
    match sent {
        Ok(UpgradeAnswer::Switched {
            conn,
            head,
            leftover,
        }) => {
            // The send can wait out the head bound, and a key deleted meanwhile opens no socket. The refusal is then
            // the call's one end, so the call that got no `101` leaves no row, and the return drops the connection.
            if let Err(unheld) = held(&relay.router().snapshot(), &admitted, (relay.clock)()) {
                ending.withdraw();
                return Err((call, unheld.into()));
            }
            // The record is the upgrade request's, so it ends at the `101` the caller gets, whatever the socket's life.
            ending.went_back(head.status);
            ending.end(FeatureOutcome::Forwarded);
            slot.leave(Tunnel {
                admitted,
                client,
                idle: relay.limits.socket_idle_timeout,
                key: key.label.clone(),
                leftover,
                method: call.method,
                opened: Instant::now(),
                path: call.path,
                place,
                relay: Arc::clone(relay),
                upstream: conn,
            });
            Ok(switched(&head))
        }
        Ok(UpgradeAnswer::Answered(ControlAnswer { head, body })) => {
            Ok(answered(head, body, None, None, ending, None))
        }
        Err(failure) => {
            ending.unanswered();
            tracing::debug!(
                outcome = "failed",
                method = %call.method,
                path = call.path.as_str(),
                key = %key.label,
                case = case_of(&failure),
                "a feature call failed ahead of the upstream's answer"
            );
            Ok(UPSTREAM_UNREACHABLE.response(&call.id))
        }
    }
}

/// The slot of one connection: an admitted socket's handler leaves the tunnel there, and the connection's future
/// takes it once hyper has handed the stream over.
#[derive(Clone, Default)]
pub(crate) struct Handover(Arc<Mutex<Option<Tunnel>>>);

impl Handover {
    pub(crate) fn take(&self) -> Option<Tunnel> {
        self.0.lock().expect("handover slot poisoned").take()
    }

    fn leave(&self, tunnel: Tunnel) {
        *self.0.lock().expect("handover slot poisoned") = Some(tunnel);
    }
}

/// The client's `OnUpgrade`, the upstream connection with the bytes that came behind its `101`, the identity the socket
/// was admitted with and the relay whose registry its checks read, the socket's idle time and its place in the counts,
/// and what the tunnel's line names.
pub(crate) struct Tunnel {
    admitted: Admitted,
    client: OnUpgrade,
    idle: Duration,
    key: String,
    leftover: BytesMut,
    method: String,
    opened: Instant,
    path: String,
    place: SocketPlace,
    relay: Arc<Relay>,
    upstream: Conn,
}

impl Tunnel {
    /// Runs to the tunnel's end and writes its line. A `stop` that is set, or flips, ends it at once, and so do a check
    /// that the admitted identity fails and the idle time with no byte in either direction. The client's stream is the
    /// accepted one, taken out of hyper's `Upgraded` and out of the head recorder, and the bytes hyper read past the
    /// request's head go to the upstream ahead of the first copy.
    pub(crate) async fn run(self, mut stop: watch::Receiver<bool>) {
        let Tunnel {
            admitted,
            client,
            idle,
            key,
            leftover,
            method,
            opened,
            path,
            place,
            relay,
            upstream,
        } = self;
        let started = Instant::now();
        // The instant of the last byte in either direction. Its first value is the run's start, so the idle bound holds
        // over the hand-over and the first writes too.
        let last = Mutex::new(started);
        // A socket need not end by itself, and its client can die without a close where no keepalive finds it. So the
        // stop ends it at once, since a tunnel that waited out the drain would hold each stop for its bound, a check
        // ends one whose admission no longer holds, and the idle time ends one that went silent. A check that is due
        // wins over a byte or the idle time of the same poll, so such a socket carries no more bytes and ends under
        // the check's cause.
        let (cause, check) = tokio::select! {
            biased;
            () = stopped(&mut stop) => ("stopped", None),
            word = unheld(&relay, &admitted, started) => ("check_failed", Some(word)),
            () = silent(&last, idle) => ("idle", None),
            cause = carry(client, upstream, leftover, place, &last) => (cause, None),
        };
        tracing::debug!(
            cause,
            check,
            method = %method,
            path = path.as_str(),
            key = %key,
            life_ms = u64::try_from(opened.elapsed().as_millis()).unwrap_or(u64::MAX),
            "a socket's tunnel ended"
        );
    }
}

/// The words of one leg's end in the tunnel's line.
struct Leg {
    ended: &'static str,
    failed: &'static str,
}

const CLIENT: Leg = Leg {
    ended: "client_ended",
    failed: "client_failed",
};

const UPSTREAM: Leg = Leg {
    ended: "upstream_ended",
    failed: "upstream_failed",
};

/// A socket's place and its two legs, each in its halves, from the moment the client's stream is in hand. The place is
/// the first field, since a struct's fields drop in the order of their declaration, so it frees ahead of the legs on
/// each road out, the return of `carry` and the drop of its future alike: a client that reads its connection's end can
/// dial again at once, and its request must find the place its own tunnel held free.
struct Legs {
    _place: SocketPlace,
    client_read: ReadHalf<Accepted>,
    client_write: WriteHalf<Accepted>,
    upstream_read: ReadHalf<Conn>,
    upstream_write: WriteHalf<Conn>,
}

/// The two legs of a tunnel to the first end of a direction, with its cause. The place and both legs free on the
/// return, and on the drop of this future at the tunnel's other ends, the stop, a failed check and the idle time. No
/// close frame is written on either road, so the session reads a lost connection and dials again. Each copy writes
/// the instant of its last byte into `last`.
async fn carry(
    client: OnUpgrade,
    upstream: Conn,
    leftover: BytesMut,
    place: SocketPlace,
    last: &Mutex<Instant>,
) -> &'static str {
    let Ok(upgraded) = client.await else {
        return CLIENT.failed;
    };
    // Each connection of the feature listener hands over this type.
    let Ok(parts) = upgraded.downcast::<TokioIo<HeadRecorder<Accepted>>>() else {
        return CLIENT.failed;
    };
    // Out of the recorder too: its parse would meet each byte of the socket's life, and nothing reads its queue
    // behind the upgrade.
    let (client_read, client_write) = tokio::io::split(parts.io.into_inner().into_inner());
    let (upstream_read, upstream_write) = tokio::io::split(upstream);
    let mut legs = Legs {
        _place: place,
        client_read,
        client_write,
        upstream_read,
        upstream_write,
    };
    if whole(&mut legs.client_write, &leftover).await.is_err() {
        return CLIENT.failed;
    }
    if whole(&mut legs.upstream_write, &parts.read_buf)
        .await
        .is_err()
    {
        return UPSTREAM.failed;
    }
    // A direction that ends ends both, so a client that left with no close frame frees the upstream connection at
    // once, and no loop shuts the other leg's half down at its own read's end.
    tokio::select! {
        cause = pass(&mut legs.client_read, &mut legs.upstream_write, CLIENT, UPSTREAM, last) => cause,
        cause = pass(&mut legs.upstream_read, &mut legs.client_write, UPSTREAM, CLIENT, last) => cause,
    }
}

/// One direction's copy, to the end or the failure of its read, or the failure of its write. Each read that brings
/// bytes writes its instant into `last`.
async fn pass(
    from: &mut (impl AsyncRead + Unpin),
    to: &mut (impl AsyncWrite + Unpin),
    reader: Leg,
    writer: Leg,
    last: &Mutex<Instant>,
) -> &'static str {
    let mut buffer = vec![0; COPY_BUFFER];
    loop {
        let read = match from.read(&mut buffer).await {
            Ok(0) => return reader.ended,
            Ok(read) => read,
            Err(_) => return reader.failed,
        };
        *last.lock().expect("last byte's instant poisoned") = Instant::now();
        if whole(to, &buffer[..read]).await.is_err() {
            return writer.failed;
        }
    }
}

/// Resolves with the word of the first check that `admitted` fails. Each check reads the registry's snapshot anew, the
/// first one interval behind `started` and each one interval behind the last, so a check that runs late moves the
/// later ones with it and none is caught up.
async fn unheld(relay: &Relay, admitted: &Admitted, started: Instant) -> &'static str {
    let every = relay.limits.socket_check_interval;
    let mut checks = tokio::time::interval_at(started + every, every);
    checks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        checks.tick().await;
        // The snapshot drops here, ahead of the next wait, so an open socket holds no registry of an earlier moment.
        let failed = held(&relay.router().snapshot(), admitted, (relay.clock)())
            .err()
            .map(|unheld| unheld.word());
        if let Some(word) = failed {
            return word;
        }
    }
}

/// Resolves once `idle` has passed since the instant `last` holds. It sleeps to that instant plus `idle` and reads the
/// instant again when it wakes, so a byte that moved meanwhile starts the time again.
async fn silent(last: &Mutex<Instant>, idle: Duration) {
    let read = || *last.lock().expect("last byte's instant poisoned");
    let mut since = read();
    loop {
        tokio::time::sleep_until(since + idle).await;
        let latest = read();
        if latest == since {
            return;
        }
        since = latest;
    }
}

/// `bytes` written whole and flushed.
async fn whole(to: &mut (impl AsyncWrite + Unpin), bytes: &[u8]) -> std::io::Result<()> {
    to.write_all(bytes).await?;
    to.flush().await
}

/// The `101` as the client gets it: the upstream's head as `copied_head` passes it, with the upstream's `connection`
/// and `upgrade` lines back in, since the client's handshake reads both.
fn switched(head: &phoebis_transport::response::ResponseHead) -> Response {
    let mut builder = copied_head(head, |_| false);
    for (name, value) in &head.headers {
        if !(name.eq_ignore_ascii_case("connection") || name.eq_ignore_ascii_case("upgrade")) {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            builder = builder.header(name, value);
        }
    }
    built(builder, Body::empty())
}
