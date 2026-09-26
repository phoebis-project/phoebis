//! The sessions view: the live pins joined with what the ledger saw, the release that expires a pin, and the
//! per-model cost card the console shows beside them.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_routing::BurnUnit;
use serde_json::json;

use crate::routes::failure;
use crate::AdminState;

pub(crate) async fn list(State(state): State<Arc<AdminState>>) -> Response {
    match rows(&state).await {
        Ok(out) => Json(json!({ "sessions": out })).into_response(),
        Err(refusal) => refusal,
    }
}

// The error is a rendered HTTP response, returned once per request and dropped; boxing it would buy a
// pointer chase and nothing else.
#[allow(clippy::result_large_err)]
pub(crate) async fn rows(state: &AdminState) -> Result<Vec<serde_json::Value>, Response> {
    let Ok(pins) = state.router.pins().await else {
        return Err(failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "the router is not available",
        ));
    };
    // Read right after the pins ask and before the ledger read below awaits: a pin belongs to the live holder
    // of its key id alone, and taking this snapshot before that await keeps a key deleted and recreated during
    // it from stamping this request's pins with the replacement's generation.
    let live: std::collections::BTreeMap<String, i64> = state
        .router
        .snapshot()
        .keys()
        .map(|k| (k.id.clone(), k.created_at))
        .collect();
    let now = (state.clock)();
    let accounts = match state.store.accounts() {
        Ok(accounts) => accounts,
        Err(e) => return Err(crate::routes::store_failure(e)),
    };
    let overages = state.router.overages().await.unwrap_or_default();
    let fallback_ms = (state.settings.cooldown_fallback_seconds * 1_000) as i64;
    let cutoff = now - state.session_expiry.as_millis() as i64;
    let activity = match state
        .ledger
        .db()
        .read(move |c| phoebis_ledger::session_activity(c, cutoff))
        .await
    {
        Ok(a) => a,
        Err(e) => return Err(crate::analytics::ledger_failure(e)),
    };
    let mut rows: std::collections::BTreeMap<(String, i64, String), serde_json::Value> =
        std::collections::BTreeMap::new();
    // Whether the pin currently held for a session is its main agent's, so a subagent's pin (however recently
    // used) never displaces it; the keepalive's own agent count answers `agents` when a session has no engine
    // view, so every agent a pin names is counted here regardless of which one the row displays.
    let mut held_is_main: std::collections::BTreeMap<(String, i64, String), bool> =
        std::collections::BTreeMap::new();
    let mut agents_by_session: std::collections::BTreeMap<
        (String, i64, String),
        std::collections::BTreeSet<String>,
    > = std::collections::BTreeMap::new();
    for p in pins.into_iter().filter(|p| p.last_used_at >= cutoff) {
        // `delete_api_key` deletes the key's pins, so a pin always names the live generation of its id.
        let Some(&generation) = live.get(&p.key_id) else {
            continue;
        };
        let key = (p.key_id.clone(), generation, p.session_id.clone());
        agents_by_session
            .entry(key.clone())
            .or_default()
            .insert(p.agent_id.clone());
        let is_main = p.agent_id == "main";
        // A session holds a pin per agent and the row shows one of them: the main agent's pin once one exists,
        // else whichever the caller used last.
        let keep = match (held_is_main.get(&key).copied(), rows.get(&key)) {
            (Some(true), _) => false,
            (Some(false), Some(held)) => {
                is_main || held["last_used_at"].as_i64().unwrap_or(0) < p.last_used_at
            }
            _ => true,
        };
        if !keep {
            continue;
        }
        held_is_main.insert(key.clone(), is_main);
        // The unit a reader takes for `burn`, the account's kind and whether its overage evidence stands now:
        // the same predicate every reader of it applies, so this row never disagrees with the selector. A pin
        // whose account left the registry has no kind to pick a unit from, so both read null rather than
        // guessing one.
        let unit = accounts
            .iter()
            .find(|a| a.id == p.account_id)
            .map(|a| a.kind)
            .map(|kind| {
                let in_overage = overages
                    .iter()
                    .any(|(id, e)| *id == p.account_id && e.stands(now, fallback_ms));
                BurnUnit::of(kind, in_overage)
            });
        let burn = unit.map(|u| u.read(&p));
        rows.insert(
            key,
            json!({
                "key_id": p.key_id, "key_generation": generation, "session_id": p.session_id, "agent_id": p.agent_id, "model": p.model, "account_id": p.account_id, "epoch": p.epoch,
                "created_at": p.created_at, "last_used_at": p.last_used_at, "last_request_at": null, "requests": null, "pinned": true,
                "burn": burn, "burn_unit": unit, "burn_at": p.burn_at, "cache_ttl_ms": p.cache_ttl_ms,
                "last_input_tokens": p.last_input_tokens, "last_refresh_at": p.last_refresh_at, "shed_from": p.shed_from,
            }),
        );
    }
    for a in activity {
        let entry = rows
            .entry((a.key_id.clone(), a.key_generation, a.session_id.clone()))
            .or_insert_with(|| {
                json!({
                    "key_id": a.key_id, "key_generation": a.key_generation, "session_id": a.session_id, "agent_id": null, "model": a.model, "account_id": null, "epoch": null,
                    "created_at": null, "last_used_at": null, "last_request_at": null, "requests": 0, "pinned": false,
                    "burn": null, "burn_unit": null, "burn_at": null, "cache_ttl_ms": null, "last_input_tokens": null,
                    "last_refresh_at": null, "shed_from": null,
                })
            });
        entry["last_request_at"] = json!(a.last_request_at);
        entry["requests"] = json!(a.requests);
        if entry["account_id"].is_null() {
            // Without a pin the serving account is the ledger's: the newest attempt of the newest request.
            entry["account_id"] = json!(a.account_id);
        }
    }
    let engine: std::collections::BTreeMap<(String, i64, String), phoebis_keepalive::SessionView> =
        state
            .keepalive
            .sessions()
            .into_iter()
            .map(|s| {
                (
                    (s.key_id.clone(), s.key_generation, s.session_id.clone()),
                    s,
                )
            })
            .collect();
    for (key, view) in &engine {
        let entry = rows.entry(key.clone()).or_insert_with(|| {
            json!({
                "key_id": key.0, "key_generation": key.1, "session_id": key.2, "agent_id": null, "model": null, "account_id": null, "epoch": null,
                "created_at": null, "last_used_at": null, "last_request_at": null, "requests": 0, "pinned": false,
                "burn": null, "burn_unit": null, "burn_at": null, "cache_ttl_ms": null, "last_input_tokens": null,
                "last_refresh_at": null, "shed_from": null,
            })
        });
        entry["companion"] = json!(view.companion);
        entry["companion_since"] = json!(view.companion_since);
        entry["agents"] = json!(view.agents);
        entry["keepalive"] = json!(view.keepalive);
    }
    // A row whose generation no live key holds is read-only: the engine's sessions and recipes are keyed
    // under the live generation alone and cannot belong to it, so it carries no companion or recipe. A
    // deleted key's session outlives the key in the engine until the idle sweep or a release clears it, so
    // the clearing is on `historical` as well as on the engine's own map, not on the map alone.
    for (key, entry) in rows.iter_mut() {
        let historical = live.get(&key.0) != Some(&key.1);
        entry["historical"] = json!(historical);
        if historical || !engine.contains_key(key) {
            entry["companion"] = json!("none");
            entry["companion_since"] = json!(null);
            entry["agents"] = json!(agents_by_session.get(key).map_or(0, |a| a.len()));
            entry["keepalive"] = json!({
                "last_guard": null, "net_meter_tokens": null, "net_usd": null, "next_probe_at": null, "state": null
            });
        }
    }
    // A session the engine knows only from a registration carries no pin and no ledger row, so its model and
    // serving account come from its main agent's recipe instead, the first the session's agents sort to. A
    // recipe belongs to the live key alone, so it joins under that generation and never reaches a historical row.
    let mut recipe_by_session: std::collections::BTreeMap<
        (String, i64, String),
        phoebis_keepalive::RecipeView,
    > = std::collections::BTreeMap::new();
    for recipe in state.keepalive.recipes() {
        let Some(&generation) = live.get(&recipe.key_id) else {
            continue;
        };
        recipe_by_session
            .entry((recipe.key_id.clone(), generation, recipe.session_id.clone()))
            .or_insert(recipe);
    }
    for (key, entry) in rows.iter_mut() {
        let Some(recipe) = recipe_by_session.get(key) else {
            continue;
        };
        if entry["model"].is_null() {
            entry["model"] = json!(recipe.model);
        }
        if entry["account_id"].is_null() {
            entry["account_id"] = json!(recipe.account_id);
        }
    }
    let mut out: Vec<serde_json::Value> = rows.into_values().collect();
    let latest = |v: &serde_json::Value| {
        v["last_used_at"]
            .as_i64()
            .unwrap_or(0)
            .max(v["last_request_at"].as_i64().unwrap_or(0))
    };
    out.sort_by_key(|v| std::cmp::Reverse(latest(v)));
    Ok(out)
}

pub(crate) async fn costs(State(state): State<Arc<AdminState>>) -> Response {
    let table = state.pricing.current();
    Json(json!({
        "version": table.version, "effective": table.effective,
        "models": table.models.iter().map(|(m, p)| json!({
            "model": m, "input_per_m": p.input, "cache_write_5m_per_m": p.cache_write_5m, "cache_write_1h_per_m": p.cache_write_1h, "cache_read_per_m": p.cache_read, "output_per_m": p.output,
            "reads_per_write_5m": if p.cache_read > 0.0 { Some(p.cache_write_5m / p.cache_read) } else { None },
        })).collect::<Vec<_>>(),
    }))
    .into_response()
}

pub(crate) async fn release(
    State(state): State<Arc<AdminState>>,
    Path((key, session)): Path<(String, String)>,
) -> Response {
    // A historical row's key id is not the live snapshot's: the console never offers the action on it, and a
    // deleted key's session lingering in the engine must not be reachable by naming it directly either.
    if state.router.snapshot().key(&key).is_none() {
        return failure(StatusCode::NOT_FOUND, "no pin or recipe for this session");
    }
    let pins = match state.router.release_session(&key, &session).await {
        Ok(n) => n,
        Err(_) => {
            return failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "the router is not available",
            )
        }
    };
    let recipes = state.keepalive.release(&key, &session);
    if pins == 0 && recipes == 0 {
        return failure(StatusCode::NOT_FOUND, "no pin or recipe for this session");
    }
    let mut r = StatusCode::NO_CONTENT.into_response();
    r.headers_mut().insert(
        "x-phoebis-released",
        HeaderValue::from_str(&pins.to_string()).expect("digits"),
    );
    r
}
