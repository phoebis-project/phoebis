//! The feature listener's one handler. The listener and no header picks the profile's feature table, so a path of the
//! upstream's and a path of the router's own never meet in one set of routes.

mod forward;

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap};
use axum::response::Response;
use axum::Router;
use phoebis_profile::{claude_code_2_1_310_linux_x64, FeatureRow, FeatureTable, PathRule};
use phoebis_routing::{feature_obstacle, Obstacle};
use phoebis_store::Feature;

use crate::auth::resolve_key;
use crate::errors::{
    feature_account_unavailable, feature_account_unset, Failure, FEATURE_NOT_ALLOWED,
    UPSTREAM_UNREACHABLE,
};
use crate::messages::{new_request_id, Relay};

use self::forward::{forward, read_body, Unanswered};

/// The feature listener's routes. Each request on that listener is a feature endpoint request, whatever its path.
pub fn router(relay: Arc<Relay>) -> Router {
    Router::new().fallback(feature_call).with_state(relay)
}

/// The checks run in their one order, the key first, so a caller without one learns nothing of what the router
/// forwards, and the account's token is taken only for a call that passed each of them.
async fn feature_call(State(relay): State<Arc<Relay>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let snapshot = relay.router().snapshot();
    let now = (relay.clock)();
    let key = match resolve_key(&snapshot, &parts.headers, now) {
        Ok((key, _)) => key,
        Err(failure) => return failure.response(&new_request_id()),
    };
    let refused = |failure: &Failure| {
        tracing::debug!(
            code = failure.code,
            method = %parts.method,
            path = parts.uri.path(),
            key = %key.label,
            "a feature call was refused"
        );
        failure.response(&new_request_id())
    };
    let host = request_host(&parts.headers);
    let table = &claude_code_2_1_310_linux_x64().features;
    let Some((feature, row, method)) = matched(
        table,
        parts.method.as_str(),
        host.as_deref(),
        parts.uri.path(),
    ) else {
        return refused(&FEATURE_NOT_ALLOWED);
    };
    let Some(account_id) = snapshot.feature_account(key, feature) else {
        return refused(&feature_account_unset(feature));
    };
    let account = match (
        feature_obstacle(&snapshot, account_id, now),
        snapshot.account(account_id),
    ) {
        (None, Some(account)) => account,
        (Some(obstacle), _) => return refused(&feature_account_unavailable(obstacle.name())),
        // Not taken: `feature_obstacle` answers `Deleted` for an id the snapshot lacks.
        (None, None) => {
            return refused(&feature_account_unavailable(Obstacle::Deleted.name()));
        }
    };
    // The reservation stands until the answer's head, so the body counts in the byte budget while the forward holds it.
    let (body, _reservation) = match read_body(&relay, key, &parts.headers, body).await {
        Ok(read) => read.unzip(),
        Err(failure) => return refused(&failure),
    };
    match forward(&relay, account, row, method, &parts, body).await {
        Ok(answer) => answer,
        Err(Unanswered::Refused(failure)) => refused(&failure),
        Err(Unanswered::Failed(case)) => {
            tracing::debug!(
                outcome = "failed",
                method = %parts.method,
                path = parts.uri.path(),
                key = %key.label,
                case,
                "a feature call failed ahead of the upstream's answer"
            );
            UPSTREAM_UNREACHABLE.response(&new_request_id())
        }
    }
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

/// The row a keyed request falls under, with its feature and the row's own spelling of the method.
fn matched(
    table: &'static FeatureTable,
    method: &str,
    host: Option<&str>,
    path: &str,
) -> Option<(Feature, &'static FeatureRow, &'static str)> {
    let host = host?;
    // A path the upstream could fold into another one would carry a refused call under the account's token.
    if !plain_path(path) {
        return None;
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
            // excepted call under another spelling.
            let excepted = row
                .except
                .iter()
                .any(|(held, whole)| *held == method && whole.eq_ignore_ascii_case(path));
            (row.host == host && on_path && !excepted).then_some((feature, row, method))
        })
}

/// A path with no segment `.` or `..`, no empty segment at any place, the last among them, no backslash, no semicolon,
/// which opens a path parameter an upstream may drop, and no encoded slash, backslash or dot in either case. Nothing is
/// decoded.
fn plain_path(path: &str) -> bool {
    let Some(segments) = path.strip_prefix('/') else {
        return false;
    };
    let encoded = path.as_bytes().windows(3).any(|three| {
        three[0] == b'%'
            && matches!(
                (three[1], three[2].to_ascii_lowercase()),
                (b'2', b'f') | (b'5', b'c') | (b'2', b'e')
            )
    });
    !encoded
        && !path.contains(['\\', ';'])
        && segments
            .split('/')
            .all(|segment| !matches!(segment, "" | "." | ".."))
}
