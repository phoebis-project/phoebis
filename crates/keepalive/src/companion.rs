//! The companion's allowance read, on the proxy listener under the API key: the key's pool as the router reads it,
//! each active account under a pseudonym, and the asking session's pins. The answer names no label, email, account id
//! or key id, so a key holder learns what is left on the pool without learning whose accounts serve it.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{RawQuery, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::map_response;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use jiff::Timestamp;
use phoebis_relay::is_valid_id as valid_id;
use phoebis_relay::{authenticated_key, new_request_id, no_store_answer};
use phoebis_routing::{
    ceiling, warm, warm_until, AccountProjection, MemberSpend, Obstacle, PinStanding,
    RegistrySnapshot, RouterGone,
};
use phoebis_store::{AccountKind, AccountRow, AccountState, ApiKeyRow, PoolRow};
use serde::Serialize;

use crate::engine::Engine;

pub const ALLOWANCE_PATH: &str = "/v0/companion/allowance";

/// The body's version, which the companion checks before it reads any other field.
const PROTOCOL: u32 = 1;

const MINUTE_MS: i64 = 60_000;

struct CompanionState {
    engine: Arc<Engine>,
    /// Read once: the store derives it from the storage key, which a running process never changes.
    key: [u8; 32],
}

pub fn companion_router(engine: Arc<Engine>) -> Router {
    let key = engine.parts.store.companion_digest_key();
    Router::new()
        .route(ALLOWANCE_PATH, get(allowance))
        .layer(map_response(no_store_answer))
        .with_state(Arc::new(CompanionState { engine, key }))
}

/// Eight lowercase hex characters: the first four bytes of the HMAC of the account id under `key`.
pub fn pseudonym(key: &[u8; 32], account_id: &str) -> String {
    let digest = phoebis_store::digest_with(key, account_id.as_bytes());
    format!(
        "{:08x}",
        u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]])
    )
}

#[derive(Serialize)]
struct Allowance {
    protocol: u32,
    now: String,
    pool: PoolView,
    accounts: Vec<AccountView>,
    pins: Vec<PinView>,
}

#[derive(Serialize)]
struct PoolView {
    ceiling: f64,
    pinning: &'static str,
    strategy: &'static str,
}

#[derive(Serialize)]
struct AccountView {
    id: String,
    funding: &'static str,
    tier: Option<String>,
    obstacle: Option<&'static str>,
    pace_score: f64,
    slow_down: Option<AccountSlowDownView>,
    windows: BTreeMap<String, WindowView>,
    spend: Vec<SpendView>,
}

#[derive(Serialize)]
struct AccountSlowDownView {
    factor: f64,
    speed: f64,
    since: Option<String>,
}

#[derive(Serialize)]
struct WindowView {
    utilization: f64,
    horizon_at: Option<String>,
    burn_per_hour: f64,
    demand_per_hour: f64,
    projected_at_horizon: f64,
    runway_minutes: Option<u64>,
}

#[derive(Serialize)]
struct SpendView {
    period: &'static str,
    spent_usd: f64,
    budget_usd: f64,
    period_end: String,
}

#[derive(Serialize)]
struct PinView {
    agent: String,
    model: String,
    account: String,
    placed_at: String,
    last_used_at: String,
    warm: bool,
    warm_until: String,
    runway_minutes: Option<u64>,
    binding_window: Option<String>,
    slow_down: Option<PinSlowDownView>,
}

#[derive(Serialize)]
struct PinSlowDownView {
    factor: f64,
    speed: f64,
}

async fn allowance(
    State(state): State<Arc<CompanionState>>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let engine = &state.engine;
    let now = engine.now();
    let snapshot = engine.parts.router.snapshot();
    // The relay's own resolution and 401 body, so a companion pointed at the wrong port sees what the client would.
    let key = match authenticated_key(&snapshot, &headers, now, &new_request_id()) {
        Ok(key) => key,
        Err(refused) => return refused,
    };
    let Some(session_id) = query.as_deref().and_then(session_id) else {
        return refusal(StatusCode::BAD_REQUEST, "invalid_identity");
    };
    if !key.companion_permissions.allowance {
        return refusal(StatusCode::FORBIDDEN, "companion_not_permitted");
    }
    let Some(pool) = snapshot.key_pool(&key.id) else {
        return refusal(StatusCode::SERVICE_UNAVAILABLE, "router_unavailable");
    };
    let router = &engine.parts.router;
    let read = async {
        Ok::<_, RouterGone>((
            router.projections(&pool.id).await?,
            router.pins_standing().await?,
            router.member_spends().await?,
        ))
    };
    let Ok((projections, pins, spends)) = read.await else {
        return refusal(StatusCode::SERVICE_UNAVAILABLE, "router_unavailable");
    };
    let body = Allowance {
        protocol: PROTOCOL,
        now: stamp(now),
        pool: PoolView {
            ceiling: pool.window_ceiling,
            // A zero TTL keeps no pin past its request, whatever the mode reads.
            pinning: if pool.affinity_ttl_seconds == 0 {
                "off"
            } else {
                pool.affinity.mode.as_str()
            },
            strategy: pool.strategy.as_str(),
        },
        accounts: accounts(&state.key, &snapshot, pool, projections, &spends, now),
        pins: session_pins(&state.key, key, session_id, pins, now),
    };
    (
        StatusCode::OK,
        [(CONTENT_TYPE, "application/json")],
        serde_json::to_string(&body).unwrap_or_default(),
    )
        .into_response()
}

/// The query is the one `session_id=<value>` pair. The value is taken as sent, never percent-decoded, and passes the
/// check a session header's id passes, so it names a session the way the relay pinned it.
fn session_id(query: &str) -> Option<&str> {
    query
        .strip_prefix("session_id=")
        .filter(|value| valid_id(value))
}

fn refusal(status: StatusCode, code: &'static str) -> Response {
    tracing::debug!(code, "the companion's allowance read was refused");
    (
        status,
        [(CONTENT_TYPE, "application/json")],
        serde_json::json!({ "code": code }).to_string(),
    )
        .into_response()
}

/// The pool's active members, each under its pseudonym and ordered by it.
fn accounts(
    digest_key: &[u8; 32],
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    projections: Vec<AccountProjection>,
    spends: &[MemberSpend],
    now: i64,
) -> Vec<AccountView> {
    let mut accounts: Vec<AccountView> = projections
        .into_iter()
        .filter_map(|projection| {
            let row = snapshot.account(&projection.account_id)?;
            active(row, projection.obstacle.as_ref())
                .then(|| account_view(digest_key, snapshot, pool, row, projection, spends, now))
        })
        .collect();
    accounts.sort_by(|a, b| a.id.cmp(&b.id));
    accounts
}

/// Enabled, ready and not past its end. The row's state decides rather than the selector's obstacle, which holds
/// nothing against a `funding_required` account whose retry is due.
fn active(row: &AccountRow, obstacle: Option<&Obstacle>) -> bool {
    row.enabled && row.state == AccountState::Ready && !matches!(obstacle, Some(Obstacle::Ended))
}

fn account_view(
    digest_key: &[u8; 32],
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    row: &AccountRow,
    projection: AccountProjection,
    spends: &[MemberSpend],
    now: i64,
) -> AccountView {
    let funding = if row.kind == AccountKind::ApiKey {
        "api_key"
    } else if snapshot.usage_based(&row.id) {
        "usage_based"
    } else {
        "plan"
    };
    let windows = projection
        .windows
        .into_iter()
        .map(|(window, w)| {
            let runway = runway_minutes(w.predicted, w.burn, ceiling(pool, &window), now, w.wall);
            let view = WindowView {
                utilization: w.predicted,
                horizon_at: w.wall.map(stamp),
                burn_per_hour: w.burn,
                demand_per_hour: w.demand,
                projected_at_horizon: w.projection,
                runway_minutes: runway,
            };
            (window, view)
        })
        .collect();
    let spend = snapshot
        .members(&pool.id)
        .iter()
        .find(|member| member.account_id == row.id)
        .map(|member| {
            member
                .budgets()
                .filter_map(|(period, budget_usd)| {
                    let reported = spends.iter().find(|s| {
                        s.pool_id == pool.id && s.account_id == row.id && s.period == period
                    })?;
                    Some(SpendView {
                        period: period.as_str(),
                        spent_usd: reported.spent_usd,
                        budget_usd,
                        period_end: stamp(reported.period_end),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    AccountView {
        id: pseudonym(digest_key, &row.id),
        funding,
        tier: snapshot.priced_tier(&row.id).map(String::from),
        obstacle: projection.obstacle.as_ref().map(Obstacle::name),
        pace_score: projection.pace_score,
        slow_down: projection.slow_down.map(|entry| AccountSlowDownView {
            factor: entry.factor,
            speed: entry.speed,
            since: entry.span.map(|span| stamp(span.since)),
        }),
        windows,
        spend,
    }
}

/// The pins of the asking session under this key and its generation, ordered by agent, then model.
fn session_pins(
    digest_key: &[u8; 32],
    key: &ApiKeyRow,
    session_id: &str,
    pins: Vec<PinStanding>,
    now: i64,
) -> Vec<PinView> {
    let mut pins: Vec<PinView> = pins
        .into_iter()
        .filter(|standing| {
            standing.pin.key_id == key.id
                && standing.pin.key_generation == key.created_at
                && standing.pin.session_id == session_id
        })
        .map(|standing| {
            let pin = &standing.pin;
            PinView {
                agent: pin.agent_id.clone(),
                model: pin.model.clone(),
                account: pseudonym(digest_key, &pin.account_id),
                placed_at: stamp(pin.created_at),
                last_used_at: stamp(pin.last_used_at),
                warm: warm(pin, standing.auxiliary_only, now),
                warm_until: stamp(warm_until(pin, standing.auxiliary_only)),
                runway_minutes: standing
                    .runway_ms
                    .map(|ms| u64::try_from(ms / MINUTE_MS).unwrap_or(0)),
                binding_window: standing.binding,
                slow_down: standing.slow_down.map(|entry| PinSlowDownView {
                    factor: entry.factor,
                    speed: entry.speed,
                }),
            }
        })
        .collect();
    pins.sort_by(|a, b| (&a.agent, &a.model).cmp(&(&b.agent, &b.model)));
    pins
}

/// The whole minutes until `predicted` reaches `ceiling` at `burn_per_hour`. A window at the ceiling has none left even
/// with no burn, since an idle exhausted window is spent. With no burn, or where the runway reaches `wall`, it is
/// unbounded (`None`), since the window resets before the ceiling. A `wall` at or before `now` is a reading past its
/// reset, which bounds nothing.
fn runway_minutes(
    predicted: f64,
    burn_per_hour: f64,
    ceiling: f64,
    now: i64,
    wall: Option<i64>,
) -> Option<u64> {
    if predicted >= ceiling {
        return Some(0);
    }
    if burn_per_hour <= 0.0 {
        return None;
    }
    // Rounded rather than truncated: the difference of two shares lands a hair under a whole minute (0.95 - 0.4 is
    // 0.5499999999999999), which truncation would read a minute short.
    let minutes = ((ceiling - predicted) / burn_per_hour * 60.0).round() as u64;
    let reached = now.saturating_add(
        i64::try_from(minutes)
            .unwrap_or(i64::MAX)
            .saturating_mul(MINUTE_MS),
    );
    match wall {
        Some(wall) if wall > now && reached >= wall => None,
        _ => Some(minutes),
    }
}

/// `ms` in RFC 3339 at UTC, held inside jiff's range so a field the contract types as a time is always one.
fn stamp(ms: i64) -> String {
    Timestamp::from_millisecond(ms)
        .unwrap_or(if ms < 0 {
            Timestamp::MIN
        } else {
            Timestamp::MAX
        })
        .to_string()
}

#[cfg(test)]
mod tests {
    use phoebis_routing::HOUR_MS;

    use super::*;

    const NOW: i64 = 1_000_000;

    #[test]
    fn a_runway_is_zero_at_the_ceiling_unbounded_without_burn_and_cut_by_the_wall() {
        let wall = NOW + 10 * HOUR_MS;
        assert_eq!(runway_minutes(0.98, 0.0, 0.95, NOW, Some(wall)), Some(0));
        assert_eq!(runway_minutes(0.4, 0.0, 0.95, NOW, Some(wall)), None);
        // The ceiling is 5.5 hours away and the wall one hour.
        assert_eq!(
            runway_minutes(0.4, 0.1, 0.95, NOW, Some(NOW + 60 * MINUTE_MS)),
            None
        );
        // A wall at `now` is a reading already past its reset.
        assert_eq!(runway_minutes(0.4, 0.1, 0.95, NOW, Some(NOW)), Some(330));
    }

    #[test]
    fn a_runway_is_the_minutes_to_the_ceiling_at_the_current_burn() {
        assert_eq!(
            runway_minutes(0.4, 0.1, 0.95, NOW, Some(NOW + 10 * HOUR_MS)),
            Some(330)
        );
    }
}
