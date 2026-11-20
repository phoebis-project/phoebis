//! The feature listener's one handler. The listener and no header picks the profile's feature table, so a path of the
//! upstream's and a path of the router's own never meet in one set of routes.

mod cap;
mod forward;
mod refresh_hold;

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

use crate::auth::resolve_key;
use crate::errors::{
    domain_checks_at_cap, feature_account_unavailable, feature_account_unset, ledger_behind,
    Failure, FEATURE_NOT_ALLOWED, UPSTREAM_UNREACHABLE,
};
use crate::messages::{new_request_id, Relay};

use self::cap::DomainCheckCap;
use self::forward::{forward, keyless, read_body, Call, Ending, Unanswered};
use self::refresh_hold::Holds;

/// The longest value of the domain check's one query parameter.
const DOMAIN_MAX_BYTES: usize = 512;

/// What the handler reads: the one cap of this listener's domain checks, the client its control-plane calls leave
/// through, the tokens its feature hosts rejected behind a forced refresh, the relay, and the feature table of the
/// router's profile.
#[derive(Clone)]
struct Listener {
    cap: Arc<DomainCheckCap>,
    control: Arc<ControlClient>,
    holds: Arc<Holds>,
    relay: Arc<Relay>,
    table: &'static FeatureTable,
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
        table,
    })
}

/// The checks run in their one order: the two keyless shapes, each compared whole, then the key, so a caller without
/// one learns nothing of what the router forwards, and the account's token is taken only for a call that passed each
/// of them. A request that misses a keyless shape by anything meets the key check as each other request does.
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
    let snapshot = relay.router().snapshot();
    let now = (relay.clock)();
    let key = match resolve_key(&snapshot, &parts.headers, now) {
        Ok((key, _)) => key,
        Err(failure) => return refusal(relay, call, &failure, UNATTRIBUTED_POOL).await,
    };
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
            refused_line(&failure, &call, &key.label);
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
    match forward(&listener, account, row, method, &parts, body, ending).await {
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

/// A refusal of the key, the table, the selection, the account's state, the ledger's standing or the domain check's
/// cap: its DEBUG line, its end, which the ledger counts and writes no row for, and its answer. `key` is the key's
/// label, `-` where none resolved. The end is a fact published ahead of the answer, so it reaches the ledger ahead of
/// any later call of the same caller. While the ledger stands behind, the end goes out best-effort and the bus counts
/// it where it drops it: a writer that stopped draining fills the bus queue, and a refusal that waited there would hold
/// its connection, a caller's without a key among them.
async fn refusal(relay: &Relay, mut call: Call, failure: &Failure, key: &str) -> Response {
    refused_line(failure, &call, key);
    let answer = failure.response(&call.id);
    if let Some(bus) = &relay.bus {
        let end = FeatureEnd::Refused {
            code: failure.code,
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

/// The DEBUG line of a refused feature endpoint request, with the path alone.
fn refused_line(failure: &Failure, call: &Call, key: &str) {
    tracing::debug!(
        code = failure.code,
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
    let no_body = !headers.contains_key(header::TRANSFER_ENCODING)
        && headers
            .get_all(header::CONTENT_LENGTH)
            .iter()
            .all(|length| {
                length
                    .to_str()
                    .is_ok_and(|length| length.trim().parse::<u64>() == Ok(0))
            });
    keyless_method(row, method, host, uri.path()).filter(|_| one_value && no_body)
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
/// too, since an upstream that decodes a path once or more would serve another call than the one the row names: each
/// form has to be plain, and an excepted path holds where the path decodes to it.
fn matched(
    table: &'static FeatureTable,
    method: &str,
    host: Option<&str>,
    path: &str,
) -> Option<(Feature, &'static FeatureRow, &'static str)> {
    let host = host?;
    // A path the upstream could fold into another one would carry a refused call under the account's token.
    let decoded = plain_path(path)?;
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
                *held == method && whole.as_bytes().eq_ignore_ascii_case(&decoded)
            });
            (row.host == host && on_path && !excepted).then_some((feature, row, method))
        })
}

/// The last of a path's forms where each of them is plain, `None` where one is not. A form is the path as sent, or
/// what one more percent-decode of a form yields, down to a form that holds no escape, a `%` with two hex digits behind
/// it. A `%` without them stays as it is. A plain form has no segment `.` or `..`, no empty segment at any place, the
/// last among them, no backslash, no semicolon, which opens a path parameter an upstream may drop, and no encoded
/// slash, backslash or dot in either case. The forms are compared as bytes, so a decoded byte outside ASCII needs no
/// text decode.
fn plain_path(path: &str) -> Option<Vec<u8>> {
    let segments = path.strip_prefix('/')?;
    let encoded = path.as_bytes().windows(3).any(|three| {
        three[0] == b'%'
            && matches!(
                (three[1], three[2].to_ascii_lowercase()),
                (b'2', b'f') | (b'5', b'c') | (b'2', b'e')
            )
    });
    let plain = !encoded
        && !path.contains(['\\', ';'])
        && segments
            .split('/')
            .all(|segment| !matches!(segment, "" | "." | ".."));
    if !plain {
        return None;
    }
    last_form(path.as_bytes())
}

/// The last form of a path whose first form is plain, `None` where a decode on the way yields a slash, a backslash, a
/// dot or a semicolon: the form that holds such an escape, or the form after it, is not plain.
///
/// Two escapes never overlap, since `%` is no hex digit, so the order of the decodes changes neither the last form nor
/// the bytes the decodes yield. One pass that decodes an escape as soon as its last byte stands therefore reaches the
/// last form and sees each byte a decode of a form yields. A decode of one form after another would read the path once
/// per form, and nested escapes give a path about half as many forms as it has bytes. A decode that yields no slash,
/// dot or backslash leaves each later form the segments of the first, so the first form's segment rules hold for each,
/// and a semicolon a decode yields stays in each later form.
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
            if matches!(decoded, b'/' | b'\\' | b'.' | b';') {
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
