//! The feature listener's one handler. The listener and no header picks the profile's feature table, so a path of the
//! upstream's and a path of the router's own never meet in one set of routes.

mod cap;
mod forward;
mod refresh_hold;
mod socket;
mod watch;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, Uri};
use axum::response::Response;
use axum::Router;
use phoebis_bus::FeatureEnd;
use phoebis_profile::{FeatureRow, FeatureTable, KeylessRow, PathRule};
use phoebis_routing::{feature_ready, UNATTRIBUTED_POOL};
use phoebis_store::Feature;
use phoebis_transport::control::ControlClient;
use tokio::time::Instant;

use crate::auth::{resolve_key, AuthError};
use crate::errors::{
    auth_failure, domain_checks_at_cap, feature_account_unavailable, feature_account_unset,
    key_sockets, ledger_behind, sockets_at_cap, Failure, FEATURE_NOT_ALLOWED, UPSTREAM_UNREACHABLE,
};
use crate::messages::{new_request_id, Relay};

use self::cap::DomainCheckCap;
use self::forward::{forward, keyless, read_body, Call, Ending, Unanswered};
use self::refresh_hold::Holds;
pub(crate) use self::socket::Handover;
use self::socket::{held, open, row_of, Admitted, Full, Sockets, Unheld};
use self::watch::{Entry, TokenMap, SOCKET_TOKEN_UNKNOWN};

/// The longest value of the domain check's one query parameter.
const DOMAIN_MAX_BYTES: usize = 512;

/// What the handler reads: the one cap of this listener's domain checks, the client its control-plane calls leave
/// through, the tokens its feature hosts rejected behind a forced refresh, the relay, the counts of its open sockets,
/// the feature table of the router's profile, and the tokens its page reads handed out.
#[derive(Clone)]
struct Listener {
    cap: Arc<DomainCheckCap>,
    control: Arc<ControlClient>,
    holds: Arc<Holds>,
    relay: Arc<Relay>,
    sockets: Arc<Sockets>,
    table: &'static FeatureTable,
    tokens: Arc<TokenMap>,
}

/// The feature listener's routes. Each request on that listener is a feature endpoint request, whatever its path.
/// The calls of the control plane leave through `control`, and `table` is the feature table of the router's profile.
pub(crate) fn router(
    relay: Arc<Relay>,
    control: Arc<ControlClient>,
    table: &'static FeatureTable,
) -> Router {
    let cap = DomainCheckCap::new(
        relay.limits.domain_checks_total,
        relay.limits.domain_checks_per_source,
    );
    Router::new().fallback(feature_call).with_state(Listener {
        cap,
        control,
        holds: Arc::new(Holds::default()),
        relay,
        sockets: Arc::new(Sockets::default()),
        table,
        tokens: Arc::new(TokenMap::default()),
    })
}

/// The checks run in their one order: the two keyless shapes and the shape of a socket request, each compared whole,
/// then the key, so a caller without one learns nothing of what the router forwards, and the account's token is taken
/// only for a call that passed each of them. A request that misses a shape by anything meets the key check as each
/// other request does.
async fn feature_call(State(listener): State<Listener>, request: Request) -> Response {
    // The domain check's one deadline runs from here, over its wait for a place too.
    let arrived = Instant::now();
    let (parts, body) = request.into_parts();
    let relay = &listener.relay;
    let table = listener.table;
    let host = request_host(&parts.headers);
    let method = parts.method.as_str();
    if keyless_method(&table.hello, method, host.as_deref(), parts.uri.path()).is_some() {
        return Response::new(Body::empty());
    }
    let mut call = Call {
        feature: None,
        id: new_request_id(),
        key_id: None,
        method: method.to_owned(),
        path: parts.uri.path().to_owned(),
        received_at: (relay.clock)(),
    };
    if let Some(method) = is_domain_check(
        &table.domain_check,
        method,
        host.as_deref(),
        &parts.uri,
        &parts.headers,
    ) {
        return domain_check(
            &listener,
            table.domain_check.host,
            method,
            &parts,
            arrived,
            call,
        )
        .await;
    }
    // Ahead of the key, which a socket request does not present: its feature's own rule admits it.
    if let Some(row) = row_of(table, method, host.as_deref(), &parts.uri, &parts.headers) {
        return socket_request(&listener, row, parts, call).await;
    }
    let snapshot = relay.router().snapshot();
    let now = (relay.clock)();
    let (key, presented) = match resolve_key(&snapshot, &parts.headers, now) {
        Ok(resolved) => resolved,
        Err(failure) => return refusal(relay, call, &failure, UNATTRIBUTED_POOL).await,
    };
    // A refused call presented the key too, so the stamp stands ahead of each later check, as on the model route.
    relay
        .router()
        .touch_key(&key.id, presented, key.rotated_at)
        .await;
    call.key_id = Some(key.id.clone());
    let Some((feature, row, method)) = matched(
        table,
        parts.method.as_str(),
        host.as_deref(),
        parts.uri.path(),
    ) else {
        return refusal(relay, call, &FEATURE_NOT_ALLOWED, &key.label).await;
    };
    call.feature = Some(feature.as_str());
    let Some(account_id) = snapshot.feature_account(key, feature) else {
        return refusal(relay, call, &feature_account_unset(feature), &key.label).await;
    };
    let account = match feature_ready(&snapshot, account_id, now) {
        Ok(account) => account,
        Err(obstacle) => {
            let failure = feature_account_unavailable(&obstacle);
            return refusal(relay, call, &failure, &key.label).await;
        }
    };
    // The one reading that counts a `ledger_behind` answer. It comes after each other check, so those keep their own
    // answers, and ahead of the body's read, so a refused call takes no read slot and no part of the byte budget. A
    // call that passes publishes its end as a fact, and the calls in flight at a stall bound those ends.
    if relay.ledger_behind(true) {
        return refusal(relay, call, &ledger_behind(), &key.label).await;
    }
    // The reservation stands until the answer's head, so the body counts in the byte budget while the forward holds it.
    let (body, _reservation) = match read_body(relay, key, &parts.headers, body).await {
        Ok(read) => read.unzip(),
        // Nothing left for the call, so the body's own bounds count in no family and publish no end.
        Err(failure) => {
            refused_line(failure.code, &call, &key.label);
            return failure.response(&call.id);
        }
    };
    let ending = Ending::sent(
        relay,
        call.clone(),
        account.id.clone(),
        feature.as_str(),
        key.id.clone(),
    );
    // As `authenticate_id` reads the key and as `signed_in` takes the account's sign-in, for a page read's token.
    let admitted = Admitted {
        account_id: account.id.clone(),
        feature,
        key_generation: key.created_at,
        key_id: key.id.clone(),
        sign_in: (account.incarnation, account.lifecycle_generation),
    };
    match forward(
        &listener, account, row, method, &parts, body, ending, &admitted, &key.label,
    )
    .await
    {
        Ok(answer) => answer,
        Err(Unanswered::Refused(failure)) => refusal(relay, call, &failure, &key.label).await,
        Err(Unanswered::Failed(case, detail)) => {
            tracing::debug!(
                outcome = "failed",
                method = %parts.method,
                path = parts.uri.path(),
                key = %key.label,
                case,
                detail = detail.as_deref(),
                "a feature call failed ahead of the upstream's answer"
            );
            UPSTREAM_UNREACHABLE.response(&call.id)
        }
    }
}

/// A socket request of `row`, admitted by the token a keyed page read handed out. One without an admitting entry gets
/// the `401` of a request without a key, so its answer tells a caller without a key nothing of the socket's path, and
/// counts under the watch's own code.
async fn socket_request(
    listener: &Listener,
    row: &'static phoebis_profile::SocketRow,
    parts: Parts,
    mut call: Call,
) -> Response {
    let relay = &listener.relay;
    let now = (relay.clock)();
    let Some(entry) = listener.tokens.entry(&parts.headers, now) else {
        return token_unknown(relay, call).await;
    };
    // The `403` and the `503` behind the entry carry its key and the feature.
    call.key_id = Some(entry.admitted.key_id.clone());
    call.feature = Some(row.feature);
    let snapshot = relay.router().snapshot();
    let (key, account) = match held(&snapshot, &entry.admitted, now) {
        Ok(rows) => rows,
        Err(unheld) => return lapsed(listener, call, &entry, unheld).await,
    };
    // As for a keyed call, behind each other check so those keep their own answers. No key's last use is stamped,
    // since the request presents no secret and the page read ahead of it stamped the key.
    if relay.ledger_behind(true) {
        return refusal(relay, call, &ledger_behind(), &key.label).await;
    }
    // Refused at once where a count is full, since a socket holds its place for as long as it stays open and nothing
    // bounds a wait for one.
    let limits = &relay.limits;
    let place = match listener
        .sockets
        .try_take(&key.id, limits.key_sockets, limits.sockets_total)
    {
        Ok(place) => place,
        Err(Full::Key) => return refusal(relay, call, &key_sockets(), &key.label).await,
        Err(Full::Total) => return refusal(relay, call, &sockets_at_cap(), &key.label).await,
    };
    match open(
        listener,
        row,
        entry.admitted.clone(),
        key,
        account,
        parts,
        call,
        place,
    )
    .await
    {
        Ok(answer) => answer,
        Err((call, check)) => lapsed(listener, call, &entry, check).await,
    }
}

/// The refusal of a socket request whose identity no longer holds, at the admission or at the upstream's `101`. The
/// key, the selection and the sign-in are one cause to the caller, so each gets the `401` of a request without a key,
/// with the entry the lookup read removed, and the account gets the `403` of a keyed call.
async fn lapsed(listener: &Listener, call: Call, entry: &Entry, unheld: Unheld) -> Response {
    let relay = &listener.relay;
    match unheld {
        Unheld::Key | Unheld::Selection | Unheld::SignIn => {
            listener.tokens.forget(entry);
            token_unknown(relay, call).await
        }
        Unheld::Account(key, obstacle) => {
            refusal(relay, call, &feature_account_unavailable(&obstacle), &key).await
        }
    }
}

/// A refusal of the key, the table, the selection, the account's state, the ledger's standing, the domain check's cap
/// or a count of the open sockets: its DEBUG line, its end, which the ledger counts and writes no row for, and its
/// answer. `key` is the key's label, `-` where none resolved. The end is a fact published ahead of the answer, so it
/// reaches the ledger ahead of any later call of the same caller. While the ledger stands behind, the end goes out
/// best-effort and the bus counts it where it drops it: a writer that stopped draining fills the bus queue, and a
/// refusal that waited there would hold its connection, a caller's without a key among them.
async fn refusal(relay: &Relay, call: Call, failure: &Failure, key: &str) -> Response {
    refusal_under(relay, call, failure, failure.code, key).await
}

/// `refusal` with `code` in its line and its end while the answer is `failure`'s, for the one refusal whose count
/// tells apart what its answer must not: a socket request without an admitting entry answers as a request without a
/// key does.
async fn refusal_under(
    relay: &Relay,
    mut call: Call,
    failure: &Failure,
    code: &'static str,
    key: &str,
) -> Response {
    refused_line(code, &call, key);
    let answer = failure.response(&call.id);
    if let Some(bus) = &relay.bus {
        let end = FeatureEnd::Refused {
            code,
            feature: call.feature,
            key_id: call.key_id.take(),
        };
        let event = call.ended(end, (relay.clock)());
        // Read without counting: the check that answers `ledger_behind` counted its own reading, and each other
        // refusal answers another code.
        if relay.ledger_behind(false) {
            bus.publish(event);
        } else {
            bus.publish_fact(event).await;
        }
    }
    answer
}

/// The socket's `401`: the answer of a request without a key, under the watch's own code.
async fn token_unknown(relay: &Relay, mut call: Call) -> Response {
    // Each `401` of the listener counts under no feature and no key.
    call.feature = None;
    call.key_id = None;
    refusal_under(
        relay,
        call,
        &auth_failure(&AuthError::Missing),
        SOCKET_TOKEN_UNKNOWN,
        UNATTRIBUTED_POOL,
    )
    .await
}

/// The DEBUG line of a refused feature endpoint request, with the path alone.
fn refused_line(code: &str, call: &Call, key: &str) {
    tracing::debug!(
        code,
        method = %call.method,
        path = call.path.as_str(),
        key = %key,
        "a feature call was refused"
    );
}

/// A domain check under the cap and its one deadline from `arrived`, which bounds the wait for a place, the send and
/// each read of the answer's body. Its lines hold the path alone, since the query names the domain. A check whose time
/// ends in the wait is refused with the `429` and publishes a refusal's end.
async fn domain_check(
    listener: &Listener,
    host: &'static str,
    method: &'static str,
    parts: &Parts,
    arrived: Instant,
    call: Call,
) -> Response {
    let Listener { cap, relay, .. } = listener;
    // Ahead of the wait, so a refused check takes no place of the cap. A check that passes publishes its end as a
    // fact, as a keyed call does.
    if relay.ledger_behind(true) {
        return refusal(relay, call, &ledger_behind(), UNATTRIBUTED_POOL).await;
    }
    let deadline = arrived + relay.limits.domain_check_timeout;
    // Each connection the system names no peer for counts under one source, so none of them passes the cap.
    let source = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |peer| peer.0.ip());
    let Ok(place) = tokio::time::timeout_at(deadline, cap.place(source)).await else {
        return refusal(relay, call, &domain_checks_at_cap(), UNATTRIBUTED_POOL).await;
    };
    let id = call.id.clone();
    // The place rides in the ending to the answer's end, whether the body ended whole, failed or the caller left.
    let ending = Ending::domain_check(relay, call, place);
    match keyless(listener, host, method, parts, deadline, ending).await {
        Ok(answer) => answer,
        Err(case) => {
            tracing::debug!(
                outcome = "failed",
                method,
                path = parts.uri.path(),
                key = UNATTRIBUTED_POOL,
                case,
                "a domain check failed ahead of the upstream's answer"
            );
            UPSTREAM_UNREACHABLE.response(&id)
        }
    }
}

/// The row's own spelling of the method where a request is the domain check of the table: its method, its host, its
/// path, a raw query that is `domain=<value>` with 1 to 512 bytes and no `&`, and no body.
fn is_domain_check(
    row: &KeylessRow,
    method: &str,
    host: Option<&str>,
    uri: &Uri,
    headers: &HeaderMap,
) -> Option<&'static str> {
    let value = uri.query()?.strip_prefix("domain=")?;
    let one_value = (1..=DOMAIN_MAX_BYTES).contains(&value.len()) && !value.contains('&');
    keyless_method(row, method, host, uri.path()).filter(|_| one_value && no_body(headers))
}

/// Whether a head declares no body: no `Transfer-Encoding` line, and no `Content-Length` other than 0.
fn no_body(headers: &HeaderMap) -> bool {
    !headers.contains_key(header::TRANSFER_ENCODING)
        && headers
            .get_all(header::CONTENT_LENGTH)
            .iter()
            .all(|length| {
                length
                    .to_str()
                    .is_ok_and(|length| length.trim().parse::<u64>() == Ok(0))
            })
}

/// The row's own spelling of the method where a request names one of a keyless row's methods, its host and its whole
/// path. Nothing else of the request is read.
fn keyless_method(
    row: &KeylessRow,
    method: &str,
    host: Option<&str>,
    path: &str,
) -> Option<&'static str> {
    let held = row.methods.iter().copied().find(|held| *held == method)?;
    (host == Some(row.host) && path == row.path).then_some(held)
}

/// The host a request names: its one `Host` line in lowercase, without a port and without a trailing dot.
fn request_host(headers: &HeaderMap) -> Option<String> {
    let mut lines = headers.get_all(header::HOST).iter();
    let (Some(line), None) = (lines.next(), lines.next()) else {
        return None;
    };
    let value = line.to_str().ok()?;
    let name = match value.rsplit_once(':') {
        Some((name, port)) if port.bytes().all(|byte| byte.is_ascii_digit()) => name,
        _ => value,
    };
    let name = name.strip_suffix('.').unwrap_or(name);
    Some(name.to_ascii_lowercase())
}

/// The row a keyed request falls under, with its feature and the row's own spelling of the method. A row's prefix or
/// whole path is compared with the path as sent, which the forward sends on as it came. Its decoded forms are read
/// too, since an upstream that decodes a path once or more would serve another call than the one the row names. Each
/// form has to be plain, and the part of a form ahead of each `?` or `#` it holds keeps a plain form's segment rules,
/// so neither a segment nor the part of one ahead of a `?` or `#` reads `.` or `..` by its visible ASCII characters. An
/// excepted path holds where the visible ASCII characters of the last form's part ahead of its first `?` or `#` spell
/// it with each empty segment left out.
fn matched(
    table: &'static FeatureTable,
    method: &str,
    host: Option<&str>,
    path: &str,
) -> Option<(Feature, &'static FeatureRow, &'static str)> {
    let host = host?;
    // A path the upstream could fold into another one would carry a refused call under the account's token.
    let mut call = plain_path(path)?;
    // An upstream that decodes a path and then cuts it at a `?` or a `#` would read the part ahead of the mark as the
    // path, so each form's part keeps the segment rules. The cut can change the last segment of a part alone, and the
    // marks of the last form stand in for each form's: an empty segment or a dot segment by its visible characters
    // holds no `%`, so no decode changes it, and no decode yields a slash, so where a form holds one ahead of a mark,
    // the last form holds it ahead of the same mark. A segment's first mark stands in for its later ones, since the
    // part ahead of a later mark holds the first mark, a visible character that is no dot. So one pass reads each
    // segment once, which keeps the read linear in a path with many marks in one segment.
    let mut segment = 0;
    let mut marked = false;
    for (at, byte) in call.iter().enumerate() {
        match byte {
            b'/' => {
                segment = at + 1;
                marked = false;
            }
            b'?' | b'#' if !marked => {
                marked = true;
                let part = &call[segment..at];
                if part.is_empty() || dotted(part) {
                    return None;
                }
            }
            _ => {}
        }
    }
    // An upstream that decodes each escape cuts the path at the last form's first mark, so the excepted call is looked
    // for ahead of it.
    if let Some(mark) = call.iter().position(|byte| matches!(byte, b'?' | b'#')) {
        call.truncate(mark);
    }
    // An upstream that drops a space or a byte outside ASCII would read the excepted call in a part that holds one at
    // any place, so the compare reads the visible characters alone.
    call.retain(u8::is_ascii_graphic);
    // An upstream that drops such a byte leaves an empty segment where a segment held nothing else, and one that then
    // folds a doubled or a closing slash would read the excepted call, so the compare drops each empty segment. No dot
    // segment is left to fold: a part with one by its visible characters matched no row above.
    call.dedup_by(|byte, kept| *byte == b'/' && *kept == b'/');
    if call.ends_with(b"/") {
        call.pop();
    }
    let features = [
        (Feature::Artifacts, table.artifacts),
        (Feature::Connectors, table.connectors),
    ];
    features
        .into_iter()
        .flat_map(|(feature, rows)| rows.iter().map(move |row| (feature, row)))
        .find_map(|(feature, row)| {
            let method = row.methods.iter().copied().find(|held| *held == method)?;
            let on_path = match row.path {
                PathRule::Under(prefix) => path.len() > prefix.len() && path.starts_with(prefix),
                PathRule::Whole(whole) => path == whole,
            };
            // In any case of its letters, since an upstream that reads a path without regard to case would serve the
            // excepted call under another spelling. An excepted path holds no `%`, so of the path's forms the last,
            // which holds no escape, is the one that can be it.
            let excepted = row.except.iter().any(|(held, whole)| {
                *held == method && whole.as_bytes().eq_ignore_ascii_case(&call)
            });
            (row.host == host && on_path && !excepted).then_some((feature, row, method))
        })
}

/// The last of a path's forms where each of them is plain, `None` where one is not. A form is the path as sent, or
/// what one more percent-decode of a form yields, down to a form that holds no escape, a `%` with two hex digits behind
/// it. A `%` without them stays as it is. A plain form has no empty segment at any place, the last among them, no
/// segment whose visible ASCII characters read `.` or `..`, since an upstream that drops the other bytes would resolve
/// it, no backslash, no semicolon, which opens a path parameter an upstream may drop, and no encoded slash, backslash
/// or dot in either case. A form is not plain either where a decode gave it a control byte, since an upstream that
/// drops such a byte or cuts the path at it would read another path. The forms are compared as bytes, so a decoded
/// byte outside ASCII needs no text decode.
fn plain_path(path: &str) -> Option<Vec<u8>> {
    let segments = path.strip_prefix('/')?;
    let encoded = path.as_bytes().windows(3).any(|three| {
        three[0] == b'%'
            && matches!(
                (three[1], three[2].to_ascii_lowercase()),
                (b'2', b'f') | (b'5', b'c') | (b'2', b'e')
            )
    });
    // Each path with an empty segment is refused here. The compare in `matched` drops empty segments only in its search
    // for the excepted call.
    let plain = !encoded
        && !path.contains(['\\', ';'])
        && segments.split('/').all(|segment| !segment.is_empty());
    if !plain {
        return None;
    }
    let form = last_form(path.as_bytes())?;
    // The last form stands in for each form: a segment that reads `.` or `..` by its visible characters holds no `%`,
    // so no decode changes it, and a form that holds such a segment holds it as the last form does.
    let dots = form.split(|byte| *byte == b'/').any(dotted);
    (!dots).then_some(form)
}

/// Whether the visible ASCII characters of `segment` spell `.` or `..`. An upstream that drops a space or a byte
/// outside ASCII would read such a segment as a dot segment.
fn dotted(segment: &[u8]) -> bool {
    let mut visible = segment.iter().filter(|byte| byte.is_ascii_graphic());
    matches!(
        [visible.next(), visible.next(), visible.next()],
        [Some(b'.'), None, _] | [Some(b'.'), Some(b'.'), None]
    )
}

/// The last form of a path whose first form starts with a slash and holds no empty segment, no backslash, no semicolon
/// and no encoded slash, backslash or dot, `None` where a decode on the way yields a slash, a backslash, a dot, a
/// semicolon or a control byte: the form that holds such an escape, or the form after it, is not plain.
///
/// Two escapes never overlap, since `%` is no hex digit, so the order of the decodes changes neither the last form nor
/// the bytes the decodes yield. One pass that decodes an escape as soon as its last byte stands therefore reaches the
/// last form and sees each byte a decode of a form yields. A decode of one form after another would read the path once
/// per form, and nested escapes give a path about half as many forms as it has bytes. A decode that yields no slash,
/// dot or backslash leaves each later form the segments of the first, so the first form's test for an empty segment
/// holds for each, and a semicolon or a control byte a decode yields stays in each later form. A decoded space can
/// change what reads as a dot segment, so the dot segments are read in the last form.
fn last_form(path: &[u8]) -> Option<Vec<u8>> {
    let mut form = Vec::with_capacity(path.len());
    for &byte in path {
        form.push(byte);
        // A decoded byte can be the last of an escape that the two bytes ahead of it open.
        while let [.., b'%', high, low] = form[..] {
            let (Some(high), Some(low)) = (hex(high), hex(low)) else {
                break;
            };
            let decoded = (high << 4) | low;
            if matches!(decoded, b'/' | b'\\' | b'.' | b';') || decoded.is_ascii_control() {
                return None;
            }
            form.truncate(form.len() - 3);
            form.push(decoded);
        }
    }
    Some(form)
}

fn hex(digit: u8) -> Option<u8> {
    char::from(digit)
        .to_digit(16)
        .and_then(|value| u8::try_from(value).ok())
}
