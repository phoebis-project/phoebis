//! The companion's allowance read, on the proxy listener under the API key: the key's pool as the router reads it,
//! each active account under a pseudonym, and the asking session's pins. The answer names no label, email, account id
//! or key id, so a key holder learns what is left on the pool without learning whose accounts serve it.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::map_response;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use jiff::Timestamp;
use phoebis_ledger::{funding_of, Speed};
use phoebis_relay::is_valid_id as valid_id;
use phoebis_relay::{authenticated_key, new_request_id, no_store_answer};
use phoebis_routing::{
    ceiling, warm, warm_until, AccountProjection, Obstacle, PinStanding, RegistrySnapshot,
};
use phoebis_store::{AccountRow, AccountState, CompanionKey, PoolRow};
use serde::Serialize;

use crate::engine::Engine;
use crate::routes::json;

const ALLOWANCE_PATH: &str = "/v0/companion/allowance";

/// The body's version, which the companion checks before it reads any other field.
const PROTOCOL: u32 = 1;

const MINUTE_MS: i64 = 60_000;

const READ_ATTEMPTS: u32 = 3;

struct CompanionState {
    engine: Arc<Engine>,
}

pub fn companion_router(engine: Arc<Engine>) -> Router {
    Router::new()
        .route(ALLOWANCE_PATH, get(allowance))
        .layer(map_response(no_store_answer))
        .with_state(Arc::new(CompanionState { engine }))
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
    pinning: &'static str,
    strategy: &'static str,
}

#[derive(Serialize)]
struct AccountView {
    id: String,
    funding: &'static str,
    tier: Option<String>,
    obstacle: Option<&'static str>,
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
    /// The line `runway_minutes` reads to, so a reader needs no rule for which of the pool's ceilings a window takes.
    ceiling: f64,
    /// The fit a fresh placement reads on the window, for a request that prices no rewrite: `fits`, `rewrite_only`,
    /// `least_bad`, `ending` or `spent`.
    fit: &'static str,
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
    let router = &engine.parts.router;
    for _ in 0..READ_ATTEMPTS {
        let snapshot = router.snapshot();
        // The relay's own resolution, so a revoked or expired key gets the same refusal here as on the messages route.
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
        let Ok((projections, pins)) = tokio::try_join!(
            router.projections(&pool.id),
            router.session_pins_standing(&key.id, key.created_at, session_id),
        ) else {
            return refusal(StatusCode::SERVICE_UNAVAILABLE, "router_unavailable");
        };
        // The asks answer from the task's current snapshot, which a reload may have moved past this one. Revisions only
        // grow in the one cell both read, so an equal revision makes the authorization, the membership and the
        // projections one registry, as the selection path makes them.
        if router.snapshot().revision != snapshot.revision {
            continue;
        }
        let companion_key = engine.parts.store.companion_key();
        let body = Allowance {
            protocol: PROTOCOL,
            now: stamp(now),
            pool: PoolView {
                // A zero TTL keeps no pin past its request, whatever the mode reads.
                pinning: if pool.affinity_ttl_seconds == 0 {
                    "off"
                } else {
                    pool.affinity.mode.as_str()
                },
                strategy: pool.strategy.as_str(),
            },
            accounts: accounts(&companion_key, &snapshot, pool, projections),
            pins: session_pins(&companion_key, pins, now),
        };
        return json(
            StatusCode::OK,
            serde_json::to_string(&body).unwrap_or_default(),
        );
    }
    refusal(StatusCode::SERVICE_UNAVAILABLE, "router_unavailable")
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
    json(status, serde_json::json!({ "code": code }).to_string())
}

/// The pool's active members, each under its pseudonym and ordered by it.
fn accounts(
    companion_key: &CompanionKey,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    projections: Vec<AccountProjection>,
) -> Vec<AccountView> {
    let mut accounts: Vec<AccountView> = projections
        .into_iter()
        .filter_map(|projection| {
            let row = snapshot.account(&projection.account_id)?;
            active(row, projection.obstacle.as_ref())
                .then(|| account_view(companion_key, snapshot, pool, row, projection))
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
    companion_key: &CompanionKey,
    snapshot: &RegistrySnapshot,
    pool: &PoolRow,
    row: &AccountRow,
    projection: AccountProjection,
) -> AccountView {
    let funding = funding_of(
        Some(row.kind),
        snapshot.usage_based(&row.id),
        Some(false),
        Speed::Standard,
    )
    .as_str();
    let windows = projection
        .windows
        .into_iter()
        .map(|(window, w)| {
            let line = ceiling(pool, &window);
            let view = WindowView {
                utilization: w.predicted,
                horizon_at: w.wall.map(stamp),
                burn_per_hour: w.burn,
                demand_per_hour: w.demand,
                projected_at_horizon: w.projection,
                ceiling: line,
                fit: w.fit,
                runway_minutes: runway_minutes(w.runway_ms),
            };
            (window, view)
        })
        .collect();
    let spend = projection
        .budget
        .into_iter()
        .filter_map(|budget| {
            Some(SpendView {
                period: budget.period,
                spent_usd: budget.spent_usd?,
                budget_usd: budget.budget_usd,
                period_end: stamp(budget.period_end?),
            })
        })
        .collect();
    AccountView {
        id: companion_key.pseudonym(&row.id),
        funding,
        tier: snapshot.priced_tier(&row.id).map(String::from),
        obstacle: projection.obstacle.as_ref().map(Obstacle::name),
        slow_down: projection.slow_down.map(|entry| AccountSlowDownView {
            factor: entry.factor,
            speed: entry.speed,
            since: entry.span.map(|span| stamp(span.since)),
        }),
        windows,
        spend,
    }
}

/// The asking session's pins, ordered by agent, then model.
fn session_pins(companion_key: &CompanionKey, pins: Vec<PinStanding>, now: i64) -> Vec<PinView> {
    let mut pins: Vec<PinView> = pins
        .into_iter()
        .map(|standing| {
            let pin = &standing.pin;
            PinView {
                agent: pin.agent_id.clone(),
                model: pin.model.clone(),
                account: companion_key.pseudonym(&pin.account_id),
                placed_at: stamp(pin.created_at),
                last_used_at: stamp(pin.last_used_at),
                warm: warm(pin, now),
                warm_until: stamp(warm_until(pin)),
                runway_minutes: runway_minutes(standing.runway_ms),
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

/// The router's runway in whole minutes, floored, a window's and a pin's alike. The router answers the milliseconds and
/// whether the runway is bounded, so the route adds no rule of its own.
fn runway_minutes(runway_ms: Option<i64>) -> Option<u64> {
    runway_ms.map(|ms| u64::try_from(ms / MINUTE_MS).unwrap_or(0))
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
