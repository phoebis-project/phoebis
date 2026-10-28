//! The sessions view: the live pins joined with what the ledger saw, the release that expires a pin, and the
//! per-model cost card the console shows beside them.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_routing::{entrypoint_of, BurnRates, BurnUnit, PinStanding, FIVE_HOUR};
use serde_json::{json, Value};
use utoipa::ToSchema;

use crate::openapi::{ErrorBody, RouterUnavailable};
use crate::routes::failure;
use crate::AdminState;

/// The live pins joined with what the ledger saw and the keepalive engine's own view, one row per session.
#[derive(serde::Serialize, ToSchema)]
pub(crate) struct SessionList {
    #[schema(value_type = Vec<Session>)]
    pub sessions: Vec<Value>,
}

/// One session row. On a row with no pin the keys from `burn` to `shed_reason` read null, `estimated` false.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Session {
    pub key_id: String,
    pub key_generation: i64,
    pub session_id: String,
    pub agent_id: Option<String>,
    pub model: Option<String>,
    pub account_id: Option<String>,
    pub epoch: Option<u64>,
    pub created_at: Option<i64>,
    pub last_used_at: Option<i64>,
    pub last_request_at: Option<i64>,
    pub requests: Option<u64>,
    pub pinned: bool,
    /// The pin's typed burn per hour, decayed to the read with its pool's `burn_average_seconds` as a shedding round
    /// decays it, priced in `burn_unit`: the share of the account's own 5-hour window on plan quota, dollars
    /// otherwise, null when the account's tier is unknown, when neither the model nor its model tier has a weight on
    /// the 5-hour window, or when the account tier has no capacity on it. Null as well on a pinned row whose account
    /// has left the registry.
    pub burn: Option<f64>,
    /// `window_share` or `usd`, also null when the pin's account has left the registry, with no kind left to pick
    /// a unit from.
    #[schema(example = "window_share")]
    pub burn_unit: Option<String>,
    /// The burn per hour on plan quota as the share of a Max 20x account's 5-hour window, whatever account tier the
    /// pin sits on, null where `burn_unit` is not `window_share` and where neither the model nor its model tier has a
    /// weight on the 5-hour window.
    pub burn_equivalent: Option<f64>,
    /// Whether `burn_equivalent` priced at the weight of the model tier's latest weighted model, for a model the cost
    /// model does not weight.
    pub estimated: bool,
    /// The instant the served burn is decayed to, the read's own, so a reader that decays it again from here adds
    /// nothing at the read. Null for a pin never renewed with usage, whose burn has nothing to decay.
    pub burn_at: Option<i64>,
    /// The time until the pin's account's projection reaches the pool's ceiling on the window that binds among those
    /// the pin's model draws, with the pin's burn counted once and no rewrite. Null for an unbounded runway and where
    /// the account projects none of those windows. Null too where the runway reaches past the binding window's next
    /// reset, since the window resets before the ceiling is reached.
    pub runway_ms: Option<i64>,
    /// Null where the account projects none of the windows the pin's model draws, false for an unbounded runway or one
    /// past the window's reset, true where `runway_ms` is set.
    pub runway_bounded: Option<bool>,
    /// The pin's account's factor from the slow-down entry in force, null on a row with no pin and where no entry is
    /// in force.
    pub speed: Option<f64>,
    /// The pin's cycle in milliseconds, null until two requests of the pin after its placement have measured a gap
    /// under the pool's burn average, and on a row with no pin.
    pub cycle_ms: Option<f64>,
    /// The cache TTL in force at the pin's last refresh.
    pub cache_ttl_ms: Option<i64>,
    /// The context of the row's pin, the main agent's conversation pin where it holds one: its last turn's prompt in
    /// tokens, cache reads and writes included, 0 before its first turn.
    pub last_input_tokens: Option<u64>,
    /// The effective effort of the request that placed or last renewed the row's pin, the word as the client sent
    /// it, null where it named none and on a row with no pin.
    pub effort: Option<String>,
    /// The pin's last main, subagent or compaction turn, or a keepalive probe's renewal.
    pub last_refresh_at: Option<i64>,
    /// The account a shed round marked the pin's session to leave.
    pub shed_from: Option<String>,
    /// Why the pin was stamped from `shed_from`: `round` for a shedding round's move, the failover's reason word
    /// (`cooldown`, `fast_lane`, `mark`) for a failover's landing. Null with no stamp and on a pin stamped before the
    /// pin carried a reason, which reads as shed.
    #[schema(example = "round")]
    pub shed_reason: Option<String>,
    pub historical: bool,
    /// `reporting`, `silent`, or `none`.
    #[schema(example = "reporting")]
    pub companion: String,
    pub companion_since: Option<i64>,
    pub agents: u64,
    /// One line per conversation pin the row lists, main's included, ordered by agent id then model, main first:
    /// on a companion row the pins of the agents the engine holds and has not released, on a row whose `companion`
    /// is `none` the pins used within `sessions.non_companion_idle_seconds`. Empty where none draws.
    pub agent_contexts: Vec<AgentContext>,
    /// Main's recipe.
    pub keepalive: SessionKeepalive,
    /// The seconds since main's last presence report, null before the first one.
    pub presence_age_seconds: Option<i64>,
    /// The conversation the session's first request that is not auxiliary (a compaction, main, subagent or
    /// unclassed request) continued at depth 2 or deeper, from a session that branched off another, an
    /// auxiliary request ahead of it skipped. Both fork keys are null for a session that continued no other
    /// conversation and for one the ledger holds no client request for.
    pub forked_from: Option<String>,
    /// The matched depth.
    pub fork_depth: Option<u32>,
}

/// One conversation pin the row lists as a line, read from this pin alone.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentContext {
    pub agent_id: String,
    pub model: String,
    /// The account the pin sits on.
    pub account_id: String,
    /// The pin's effort, as the row's `effort` reads it.
    pub effort: Option<String>,
    /// The pin's last turn's prompt in tokens, cache reads and writes included, 0 before its first turn.
    pub last_input_tokens: u64,
    /// The cache TTL in force at the pin's last refresh.
    pub cache_ttl_ms: i64,
    /// When a request last used the pin, the instant `warm` and the non-companion timeout read.
    pub last_used_at: i64,
    /// Whether `now - last_used_at < cache_ttl_ms` at the read: the cache entry the pin warms is still alive.
    pub warm: bool,
    /// The first placement of the pin's agent under the key and session, null where no record stands.
    pub created_at: Option<i64>,
}

/// The recipe as the session row reads it, every member null while main holds none.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionKeepalive {
    pub cause: Option<String>,
    pub last_guard: Option<String>,
    pub next_probe_at: Option<i64>,
    pub state: Option<String>,
}

/// The per-model cost card the console shows beside the sessions.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionCosts {
    /// The date of the price revision in force.
    pub effective: String,
    pub models: Vec<CostRow>,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CostRow {
    pub model: String,
    pub input_per_m: f64,
    pub cache_write_5m_per_m: f64,
    pub cache_write_1h_per_m: f64,
    pub cache_read_per_m: f64,
    pub output_per_m: f64,
    /// `cache_write_5m` divided by `cache_read` when `cache_read` is positive, else null.
    pub reads_per_write_5m: Option<f64>,
    /// The fast-mode cells under the same five `*_per_m` names, null on a model that serves no fast mode.
    #[schema(value_type = Option<crate::openapi::FastCells>)]
    pub fast: Value,
    /// `over` and the five `*_per_m` cells, null on a model priced one way.
    #[schema(value_type = Option<crate::openapi::LongContextCells>)]
    pub long_context: Value,
}

/// List the sessions.
#[utoipa::path(
    get,
    path = "/admin/sessions",
    operation_id = "sessions_list",
    tag = "Sessions",
    responses(
        (status = 200, description = "The session rows.", body = SessionList),
        (status = 500, description = "- `storage failure`\n- `ledger failure`", body = ErrorBody, example = json!({"error": "storage failure"})),
        (status = 503, response = RouterUnavailable),
    ),
)]
pub(crate) async fn list(State(state): State<Arc<AdminState>>) -> Response {
    match rows(&state).await {
        Ok(out) => Json(SessionList { sessions: out }).into_response(),
        Err(refusal) => refusal,
    }
}

// The error is a rendered HTTP response, returned once per request and dropped. Boxing it would buy a
// pointer chase and nothing else.
#[allow(clippy::result_large_err)]
pub(crate) async fn rows(state: &AdminState) -> Result<Vec<serde_json::Value>, Response> {
    let Ok(pins) = state.router.pins_standing().await else {
        return Err(failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "the router is not available",
        ));
    };
    // Read right after the pins ask and before the ledger read below awaits: a pin belongs to the live holder
    // of its key id alone, and taking this snapshot before that await keeps a key deleted and recreated during
    // it from stamping this request's pins with the replacement's generation.
    let snapshot = state.router.snapshot();
    let live: std::collections::BTreeMap<String, i64> = snapshot
        .keys()
        .map(|k| (k.id.clone(), k.created_at))
        .collect();
    let now = (state.clock)();
    let accounts = match state.store.accounts() {
        Ok(accounts) => accounts,
        Err(e) => return Err(crate::routes::store_failure(e)),
    };
    let overages = state.router.overages().await.unwrap_or_default();
    let fallback_ms = phoebis_routing::seconds_ms(state.settings.cooldown_fallback_seconds);
    let cutoff = now - state.session_expiry.as_millis() as i64;
    // Grouped by the client's own session, which the router keys pins under, so a request shows on the row of the
    // pin it took.
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
    // Whether the pin currently held for a session is its main agent's, so a subagent's pin never displaces it
    // and only a newer main pin can; the keepalive's own agent count answers `agents` when a session has an
    // engine view, so every agent a pin names is counted here as the fallback where it does not.
    let mut held_is_main: std::collections::BTreeMap<(String, i64, String), bool> =
        std::collections::BTreeMap::new();
    let mut agents_by_session: std::collections::BTreeMap<
        (String, i64, String),
        std::collections::BTreeSet<String>,
    > = std::collections::BTreeMap::new();
    // Every conversation pin of each session, main's among them, which the lines pass reads after the historical one.
    let mut agent_pins: std::collections::BTreeMap<(String, i64, String), Vec<PinStanding>> =
        std::collections::BTreeMap::new();
    // A pin's burn is typed tokens a second, priced here on the account it sits on: dollars through the table
    // in force at the set the pin's last prompt length selects, its fast usage at the fast cells beside it and its
    // unmetered usage at that same set, or the share of the account's own 5-hour window through the model's anchor
    // rates over its tier's capacity ratio, both under the pin's regime. The anchor rates alone are the 20x equivalent.
    // Each account's tier is read once per request.
    let revisions = state.pricing.revisions();
    let prices = revisions.in_force(now);
    let costs = state.tables.revisions.in_force(now);
    let rates = BurnRates {
        dollars: prices.rate_table(),
        five_hour: state.tables.regime_rates(FIVE_HOUR, now),
        fast: prices.fast_rate_table(),
    };
    let mut tiers: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();
    // The router decays each pin's copies to the read and reads its runway in the same ask, so the burn priced below
    // and the runway are one instant's, and this crate projects nothing.
    for standing in pins.into_iter().filter(|s| s.pin.last_used_at >= cutoff) {
        let p = &standing.pin;
        // `delete_api_key` deletes the key's pins, so a pin always names the live generation of its id.
        let Some(&generation) = live.get(&p.key_id) else {
            continue;
        };
        let key = (p.key_id.clone(), generation, p.session_id.clone());
        agents_by_session
            .entry(key.clone())
            .or_default()
            .insert(p.agent_id.clone());
        // A pin only auxiliary requests have used holds no conversation: it neither stands for the row nor draws a
        // line, though its agent counts among the session's.
        if standing.auxiliary_only {
            continue;
        }
        let is_main = p.agent_id == "main";
        agent_pins
            .entry(key.clone())
            .or_default()
            .push(standing.clone());
        // A session holds a pin per agent and the row shows one of its conversation pins: the main agent's once one
        // exists (replaced only by a newer one of its own), else whichever the caller used last.
        let keep = match (held_is_main.get(&key).copied(), rows.get(&key)) {
            (Some(true), Some(held)) => {
                is_main && held["last_used_at"].as_i64().unwrap_or(0) < p.last_used_at
            }
            (Some(false), Some(held)) => {
                is_main || held["last_used_at"].as_i64().unwrap_or(0) < p.last_used_at
            }
            _ => true,
        };
        if !keep {
            continue;
        }
        held_is_main.insert(key.clone(), is_main);
        // The unit a reader takes for `burn`, the account's kind, its class and whether its overage evidence stands
        // now: the same predicate every reader of it applies, so this row never disagrees with the selector. A pin
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
                BurnUnit::of(kind, snapshot.usage_based(&p.account_id), in_overage)
            });
        let equivalent = (unit == Some(BurnUnit::WindowShare))
            .then(|| rates.price_pin(BurnUnit::WindowShare, p))
            .flatten()
            .map(|b| b * 3_600.0);
        // The mark is the equivalent's alone: a dollar burn reads the price table's own cells.
        let estimated = equivalent.is_some()
            && costs
                .weight_or_fallback(&p.model, FIVE_HOUR)
                .is_some_and(|(_, fallback)| fallback);
        let burn = match unit {
            None => None,
            Some(BurnUnit::Usd) => rates.price_pin(BurnUnit::Usd, p).map(|b| b * 3_600.0),
            Some(BurnUnit::WindowShare) => {
                if !tiers.contains_key(&p.account_id) {
                    // The selector's own read of the plan, so a plan the cost model does not price (a free
                    // organization) reads unpriced here as it does there.
                    let tier = snapshot.priced_tier(&p.account_id).map(String::from);
                    tiers.insert(p.account_id.clone(), tier);
                }
                let tier = tiers.get(&p.account_id).and_then(Option::as_deref);
                // An account tier the cost model does not size prices nothing, as does a model with no weight of its
                // own or of its model tier, so the figure reads null rather than a guess.
                tier.and_then(|t| {
                    state
                        .tables
                        .tier_factor(t, FIVE_HOUR, entrypoint_of(p), now)
                })
                .and_then(|factor| equivalent.map(|b| b / factor))
            }
        };
        rows.insert(
            key,
            json!({
                "key_id": p.key_id, "key_generation": generation, "session_id": p.session_id, "agent_id": p.agent_id, "model": p.model, "account_id": p.account_id, "epoch": p.epoch,
                "created_at": p.created_at, "last_used_at": p.last_used_at, "last_request_at": null, "requests": null, "pinned": true,
                "burn": burn, "burn_unit": unit, "burn_equivalent": equivalent, "estimated": estimated, "burn_at": p.burn_at, "runway_ms": standing.runway_ms, "runway_bounded": standing.runway_bounded, "cache_ttl_ms": p.cache_ttl_ms,
                "speed": standing.speed,
                "cycle_ms": standing.cycle_ms,
                "last_input_tokens": p.last_input_tokens, "effort": standing.effort, "last_refresh_at": p.last_refresh_at, "shed_from": p.shed_from, "shed_reason": p.shed_reason,
            }),
        );
    }
    // Every row is keyed by its own session, as the pins are, so a session's requests land on the row of its pin.
    for a in activity {
        let entry = rows
            .entry((a.key_id.clone(), a.key_generation, a.session_id.clone()))
            .or_insert_with(|| {
                json!({
                    "key_id": a.key_id, "key_generation": a.key_generation, "session_id": a.session_id, "agent_id": null, "model": a.model, "account_id": null, "epoch": null,
                    "created_at": null, "last_used_at": null, "last_request_at": null, "requests": 0, "pinned": false,
                    "burn": null, "burn_unit": null, "burn_equivalent": null, "estimated": false, "burn_at": null, "runway_ms": null, "runway_bounded": null, "cache_ttl_ms": null, "last_input_tokens": null, "effort": null,
                    "speed": null,
                    "cycle_ms": null,
                    "last_refresh_at": null, "shed_from": null, "shed_reason": null,
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
                "burn": null, "burn_unit": null, "burn_equivalent": null, "estimated": false, "burn_at": null, "runway_ms": null, "runway_bounded": null, "cache_ttl_ms": null, "last_input_tokens": null, "effort": null,
                "speed": null,
                "cycle_ms": null,
                "last_refresh_at": null, "shed_from": null, "shed_reason": null,
            })
        });
        entry["companion"] = json!(view.companion);
        entry["companion_since"] = json!(view.companion_since);
        entry["agents"] = json!(view.agents);
        entry["keepalive"] = json!(view.keepalive);
        entry["presence_age_seconds"] = json!(view.presence_age_seconds);
    }
    // A row whose generation no live key holds is read-only: the engine's sessions and recipes are keyed
    // under the live generation alone and cannot belong to it, so it carries no companion or recipe. A
    // deleted key's session outlives the key in the engine until the engine's next tick ends it, so the
    // clearing is on `historical` as well as on the engine's own map, not on the map alone.
    for (key, entry) in rows.iter_mut() {
        let historical = live.get(&key.0) != Some(&key.1);
        entry["historical"] = json!(historical);
        if historical || !engine.contains_key(key) {
            entry["companion"] = json!("none");
            entry["companion_since"] = json!(null);
            entry["agents"] = json!(agents_by_session.get(key).map_or(0, |a| a.len()));
            entry["keepalive"] =
                json!({ "cause": null, "last_guard": null, "next_probe_at": null, "state": null });
            entry["presence_age_seconds"] = json!(null);
        }
    }
    // Once the historical pass has settled `companion`: a session no companion reports leaves the list once the later
    // of its last use and its last request is older than the non-companion timeout, the session expiry bounding it. An
    // ended session the engine still holds is measured by its end as well, its `companion_since`, so one that sent no
    // request leaves after the timeout too.
    let idle_bound = now
        - state
            .non_companion_idle
            .min(state.session_expiry)
            .as_millis() as i64;
    rows.retain(|_, entry| {
        entry["companion"] != "none"
            || entry["last_used_at"]
                .as_i64()
                .max(entry["last_request_at"].as_i64())
                .max(entry["companion_since"].as_i64())
                .is_none_or(|latest| latest >= idle_bound)
    });
    // The agent lines: on a companion row one per conversation pin of every agent the companion carries a recipe for
    // and has not released, main's included, and on any other row one per conversation pin used within the timeout.
    let line_cutoff = now - state.non_companion_idle.as_millis() as i64;
    let recipes = state.keepalive.recipes();
    let mut unreleased: std::collections::BTreeSet<(String, i64, String, String)> =
        std::collections::BTreeSet::new();
    for recipe in &recipes {
        let Some(&generation) = live.get(&recipe.key_id) else {
            continue;
        };
        if recipe.agent_state != phoebis_keepalive::AgentState::Released {
            unreleased.insert((
                recipe.key_id.clone(),
                generation,
                recipe.session_id.clone(),
                recipe.agent_id.clone(),
            ));
        }
    }
    for (key, entry) in rows.iter_mut() {
        let companion = entry["companion"] != "none";
        let mut lines: Vec<&PinStanding> = agent_pins
            .get(key)
            .into_iter()
            .flatten()
            .filter(|standing| {
                if companion {
                    unreleased.contains(&(
                        key.0.clone(),
                        key.1,
                        key.2.clone(),
                        standing.pin.agent_id.clone(),
                    ))
                } else {
                    standing.pin.last_used_at >= line_cutoff
                }
            })
            .collect();
        lines.sort_by(|a, b| {
            let (a, b) = (&a.pin, &b.pin);
            (a.agent_id != "main", &a.agent_id, &a.model).cmp(&(
                b.agent_id != "main",
                &b.agent_id,
                &b.model,
            ))
        });
        entry["agent_contexts"] = Value::Array(
            lines
                .into_iter()
                .map(|standing| {
                    let pin = &standing.pin;
                    json!({
                        "agent_id": pin.agent_id, "model": pin.model, "account_id": pin.account_id, "effort": standing.effort,
                        "last_input_tokens": pin.last_input_tokens, "cache_ttl_ms": pin.cache_ttl_ms, "last_used_at": pin.last_used_at,
                        "warm": now - pin.last_used_at < pin.cache_ttl_ms, "created_at": standing.first_seen_at,
                    })
                })
                .collect(),
        );
    }
    // A session the engine knows only from a registration carries no pin and no ledger row, so its model and
    // serving account come from its main agent's recipe instead, the first the session's agents sort to. A
    // recipe belongs to the live key alone, so it joins under that generation and never reaches a historical row.
    let mut recipe_by_session: std::collections::BTreeMap<
        (String, i64, String),
        phoebis_keepalive::RecipeView,
    > = std::collections::BTreeMap::new();
    for recipe in recipes {
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
    // One read over every row's key, after the pin, activity and engine passes have settled who belongs on the page
    // at all, so a row's fork facts come from the requests of its own session whichever pass surfaced it.
    let identity_keys: Vec<(String, i64, String)> = rows.keys().cloned().collect();
    let identities = match state
        .ledger
        .db()
        .read(move |c| phoebis_ledger::session_identities(c, &identity_keys))
        .await
    {
        Ok(identities) => identities,
        Err(e) => return Err(crate::analytics::ledger_failure(e)),
    };
    let identity_by_key: std::collections::BTreeMap<
        (String, i64, String),
        phoebis_ledger::SessionIdentity,
    > = identities
        .into_iter()
        .map(|i| {
            (
                (i.key_id.clone(), i.key_generation, i.session_id.clone()),
                i,
            )
        })
        .collect();
    for (key, entry) in rows.iter_mut() {
        let identity = identity_by_key.get(key);
        entry["forked_from"] = json!(identity.and_then(|i| i.forked_from.clone()));
        entry["fork_depth"] = json!(identity.and_then(|i| i.fork_depth));
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

/// Read the per-model cost card.
#[utoipa::path(
    get,
    path = "/admin/sessions/costs",
    operation_id = "sessions_costs",
    tag = "Sessions",
    responses(
        (status = 200, description = "The cost card.", body = SessionCosts),
    ),
)]
pub(crate) async fn costs(State(state): State<Arc<AdminState>>) -> Response {
    let revisions = state.pricing.revisions();
    let table = revisions.in_force((state.clock)());
    Json(SessionCosts {
        effective: table.effective.clone(),
        models: table
            .models
            .iter()
            .map(|(m, row)| {
                let p = &row.standard;
                CostRow {
                    model: m.clone(),
                    input_per_m: p.input,
                    cache_write_5m_per_m: p.cache_write_5m,
                    cache_write_1h_per_m: p.cache_write_1h,
                    cache_read_per_m: p.cache_read,
                    output_per_m: p.output,
                    reads_per_write_5m: if p.cache_read > 0.0 {
                        Some(p.cache_write_5m / p.cache_read)
                    } else {
                        None
                    },
                    fast: crate::pricing::fast_json(row),
                    long_context: crate::pricing::long_context_json(row),
                }
            })
            .collect(),
    })
    .into_response()
}

/// Release a session's pin and recipe.
#[utoipa::path(
    post,
    path = "/admin/sessions/{key}/{session}/release",
    operation_id = "session_release",
    tag = "Sessions",
    params(
        ("key" = String, Path, description = "The key's id"),
        ("session" = String, Path, description = "The client's own session id"),
    ),
    responses(
        (status = 204, description = "Releases both the router pin and the keepalive recipe for the pair.",
            headers(("x-phoebis-released" = String, description = "The pin count"))),
        (status = 404, description = "`no pin or recipe for this session`", body = ErrorBody, example = json!({"error": "no pin or recipe for this session"})),
        (status = 503, description = "- `the keepalive engine is shutting down`\n- `the router is not available`", body = ErrorBody, example = json!({"error": "the keepalive engine is shutting down"})),
    ),
)]
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
    let recipes = match state.keepalive.release(&key, &session) {
        Ok(recipes) => recipes,
        // The engine has quiesced for the shutdown, so its final save stays what the next process restores.
        Err(_) => {
            return failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "the keepalive engine is shutting down",
            )
        }
    };
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openapi::round_trip::{round_trips, round_trips_each};

    #[test]
    fn the_session_fixtures_round_trip_through_their_declared_types() {
        round_trips_each::<Session>("sessions", &["sessions", "*"]);
        round_trips::<SessionCosts>("sessions_costs");
        // A model priced one way carries `fast` and `long_context` as null, which no cell type takes, so each set a
        // model carries round-trips through its type and the nulls are passed over.
        let costs = crate::openapi::round_trip::fixture("sessions_costs");
        let (mut fast, mut long_context) = (0, 0);
        for model in costs["models"].as_array().unwrap() {
            let id = &model["model"];
            if !model["fast"].is_null() {
                let typed: crate::openapi::FastCells =
                    serde_json::from_value(model["fast"].clone()).unwrap();
                assert_eq!(
                    serde_json::to_value(&typed).unwrap(),
                    model["fast"],
                    "{id}: the fast cells"
                );
                fast += 1;
            }
            if !model["long_context"].is_null() {
                let typed: crate::openapi::LongContextCells =
                    serde_json::from_value(model["long_context"].clone()).unwrap();
                assert_eq!(
                    serde_json::to_value(&typed).unwrap(),
                    model["long_context"],
                    "{id}: the long-context cells"
                );
                long_context += 1;
            }
        }
        assert!(
            fast >= 1 && long_context >= 1,
            "a fast set and a long-context set at least, {fast} and {long_context} found"
        );
    }
}
