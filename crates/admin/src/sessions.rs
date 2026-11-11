//! The sessions view: the live pins joined with what the ledger saw, and the per-model cost card the console shows
//! beside them.

use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_routing::{BurnRates, BurnUnit, PinStanding, FIVE_HOUR, MAIN_AGENT, SEVEN_DAY};
use utoipa::ToSchema;

use crate::openapi::{LedgerFailure, RouterUnavailable};
use crate::routes::router_unavailable;
use crate::session_spend::{AgentRequests, SessionDay, SessionSpend};
use crate::AdminState;

/// The live pins joined with what the ledger saw and the keepalive engine's own view, one row per session.
#[derive(serde::Serialize, ToSchema)]
pub(crate) struct SessionList {
    pub sessions: Vec<Session>,
}

/// One session row. On a row with no pin the keys from `burn` to `shed_reason` read null.
#[derive(Default, serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Session {
    pub key_id: String,
    pub key_generation: i64,
    pub session_id: String,
    pub agent_id: Option<String>,
    pub model: Option<String>,
    pub account_id: Option<String>,
    pub epoch: Option<u64>,
    /// The first placement of the session's main agent under the key and session, kept until the session's last pin
    /// leaves for good, so a failover, a move or a second pin leaves it alone. Null where main never placed a pin.
    pub created_at: Option<i64>,
    /// The newest use across the session's conversation pins, main's and its agents', null without a pin.
    pub last_used_at: Option<i64>,
    pub last_request_at: Option<i64>,
    /// The session's client requests over its life under its key, the ones `spend` counts, 0 where the ledger holds
    /// none.
    pub requests: u64,
    /// The user agent of the session's newest client request, as the client sent it.
    pub user_agent: Option<String>,
    /// The session's spend over its life under its key, null where the ledger holds no request of it the router did
    /// not refuse.
    pub spend: Option<SessionSpend>,
    /// The session's client requests of the last 24 hours by class, and their spend.
    pub day: SessionDay,
    pub pinned: bool,
    /// The pin's typed burn per hour, decayed to the read with its pool's `burn_average_seconds` as a shedding round
    /// decays it, priced in `burn_unit`: the share of the account's own window `burn_window` names on plan quota,
    /// dollars otherwise, null when the account's tier is unknown, when the model has no weight on the window, or when
    /// the account tier has no capacity on it.
    pub burn: Option<f64>,
    /// `window_share` or `usd`, null on an unpinned row.
    #[schema(value_type = Option<String>, example = "window_share")]
    pub burn_unit: Option<BurnUnit>,
    /// `five_hour` where the account's shared window set has it, `seven_day` otherwise, null where `burn_unit` is not
    /// `window_share`.
    #[schema(example = "five_hour")]
    pub burn_window: Option<String>,
    /// The burn per hour on plan quota as the Pro-week figure, the share of the Pro 7-day window a Pro account would
    /// spend on it, whatever account tier the pin sits on, null where `burn_unit` is not `window_share` and where the
    /// model has no weight on the window.
    pub burn_equivalent: Option<f64>,
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
    /// The pin's account's factor under the slow-down, from the entry in force, null on a row with no pin and where no
    /// entry is in force.
    pub factor: Option<f64>,
    /// The speed the holds realized, from 0 to 1, beside `factor`, null where `factor` is.
    pub speed: Option<f64>,
    /// The pin's cycle in milliseconds, null until two requests of the pin after its placement have measured a gap
    /// under the pool's burn average, and on a row with no pin.
    pub cycle_ms: Option<f64>,
    /// The cache TTL in force at the pin's last refresh.
    pub cache_ttl_ms: Option<i64>,
    /// Whether the pinned account still holds the session's prompt cache, from the pin's last use and its cache
    /// TTL. Null without a pin.
    pub warm: Option<bool>,
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
    /// The session's requests per agent and model, from the ledger, with each agent's type, the account and the
    /// instant of its newest request and its spend.
    pub agent_requests: Vec<AgentRequests>,
    /// Main's recipe, or the newest other agent's where main holds none.
    pub keepalive: SessionKeepalive,
    /// Main's lifecycle word from the keepalive engine's record, whether or not a recipe stands: `busy`, `idle`,
    /// `parked`, `released` or `unknown`, or `registered` for a main the mod registered that has sent no real request
    /// yet. Null where no mod reports the session.
    #[schema(example = "busy")]
    pub lifecycle: Option<String>,
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
    /// The pin's typed burn per hour in `burn_unit`, priced as the row's `burn` is for this pin.
    pub burn: Option<f64>,
    /// `window_share` or `usd` for this pin, as the row's `burn_unit`.
    #[schema(value_type = Option<String>, example = "window_share")]
    pub burn_unit: Option<BurnUnit>,
    /// The window `burn` is a share of for this pin, as the row's `burn_window`.
    #[schema(example = "five_hour")]
    pub burn_window: Option<String>,
    /// The pin's burn per hour as the Pro-week figure, as the row's `burn_equivalent`.
    pub burn_equivalent: Option<f64>,
    /// The instant the pin's served burn is decayed to, as the row's `burn_at`. Null for a pin never renewed with
    /// usage.
    pub burn_at: Option<i64>,
    /// The first placement of the pin's agent under the key and session, null where no record stands.
    pub created_at: Option<i64>,
    /// The agent's lifecycle word from the keepalive engine's record, null where no mod reports it.
    #[schema(example = "idle")]
    pub lifecycle: Option<String>,
}

/// Main's recipe as the session row reads it, or the newest other agent's where main holds none, every member null
/// while no agent holds one.
#[derive(Default, serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionKeepalive {
    #[schema(value_type = Option<String>)]
    pub cause: Option<phoebis_keepalive::Reason>,
    #[schema(value_type = Option<String>)]
    pub last_guard: Option<phoebis_keepalive::Guard>,
    pub next_probe_at: Option<i64>,
    pub state: Option<String>,
}

impl From<&phoebis_keepalive::KeepaliveSummary> for SessionKeepalive {
    fn from(summary: &phoebis_keepalive::KeepaliveSummary) -> Self {
        SessionKeepalive {
            cause: summary.cause,
            last_guard: summary.last_guard,
            next_probe_at: summary.next_probe_at,
            state: summary.state.map(|state| lifecycle_word(state).into()),
        }
    }
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
    pub fast: Option<crate::openapi::FastCells>,
    /// `over` and the five `*_per_m` cells, null on a model priced one way.
    pub long_context: Option<crate::openapi::LongContextCells>,
}

/// List the sessions.
#[utoipa::path(
    get,
    path = "/admin/sessions",
    operation_id = "sessions_list",
    tag = "Sessions",
    responses(
        (status = 200, description = "The session rows.", body = SessionList),
        (status = 500, response = LedgerFailure),
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
pub(crate) async fn rows(state: &AdminState) -> Result<Vec<Session>, Response> {
    let Ok(pins) = state.router.pins_standing().await else {
        return Err(router_unavailable());
    };
    // Read right after the pins ask and before the ledger read below awaits: a pin belongs to the live holder
    // of its key id alone, and taking this snapshot before that await keeps a key deleted and recreated during
    // it from stamping this request's pins with the replacement's generation.
    let snapshot = state.router.snapshot();
    let live = phoebis_ledger::LiveKeys::from_snapshot(&snapshot).0;
    // `delete_api_key` deletes the key's pins, so a pin always names the live generation of its id.
    let pin_key = |p: &phoebis_store::PinRow| {
        Some((
            p.key_id.clone(),
            *live.get(&p.key_id)?,
            p.session_id.clone(),
        ))
    };
    let now = (state.clock)();
    let cutoff = now - state.session_expiry.as_millis() as i64;
    let idle_bound = now
        - state
            .non_companion_idle
            .min(state.session_expiry)
            .as_millis() as i64;
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
    let mut candidates: std::collections::BTreeSet<(String, i64, String)> = pins
        .iter()
        .filter(|standing| standing.pin.last_used_at >= idle_bound)
        .filter_map(|standing| pin_key(&standing.pin))
        .chain(engine.keys().cloned())
        .collect();
    // One read for the activity and the fork facts: the keys the pin and engine passes can surface are known before
    // the read, the activity's come from the read itself, and a row's fork facts come from the requests of its own
    // session whichever pass surfaced it. Only the sessions the retain below can keep are named: every engine
    // session, and one with a pin use or a request at or after `idle_bound`.
    let cursor = state.spend.cursor(&state.tables, now);
    let read = state.ledger.db().read(move |c| {
        // One WAL snapshot for every statement: `read` lends a connection and starts no transaction, and a request
        // landing between the first fill and the incremental read would be dropped by the filled-session rule and
        // passed by the watermark.
        let tx = c.unchecked_transaction()?;
        // Grouped by the client's own session, which the router keys pins under, so a request shows on the row of
        // the pin it took.
        let activity = phoebis_ledger::session_activity(&tx, cutoff)?;
        candidates.extend(
            activity
                .iter()
                .filter(|a| a.last_request_at >= idle_bound)
                .map(activity_key),
        );
        let keys: Vec<(String, i64, String)> = candidates.into_iter().collect();
        // The fold's reads ride in this snapshot: the start watermark where the fold starts, the whole history of
        // each candidate the fold does not hold yet, and the rows landed since the watermark, so one reader hold
        // serves the poll.
        let poll = crate::session_spend::read_poll(&tx, &cursor, &keys)?;
        Ok((
            activity,
            phoebis_ledger::session_identities(&tx, &keys)?,
            poll,
        ))
    });
    // Each read is a snapshot of its own, so they wait together and their order carries no meaning.
    let (reported, read) = tokio::join!(crate::routes::reported_samples(state), read);
    let (activity, identities, poll) = match read {
        Ok(read) => read,
        Err(e) => return Err(crate::analytics::ledger_failure(e)),
    };
    let mut rows: std::collections::BTreeMap<(String, i64, String), Session> =
        std::collections::BTreeMap::new();
    // The keepalive's own agent count answers `agents` when a session has an engine view, so every agent a pin names
    // is counted here as the fallback where it does not.
    let mut agents_by_session: std::collections::BTreeMap<
        (String, i64, String),
        std::collections::BTreeSet<String>,
    > = std::collections::BTreeMap::new();
    // Every conversation pin of each session, main's among them, which the lines pass reads after the historical one.
    let mut agent_pins: std::collections::BTreeMap<(String, i64, String), Vec<PinStanding>> =
        std::collections::BTreeMap::new();
    // A pin's burn is typed tokens a second, priced here on the account it sits on: dollars through the table
    // in force at the set the pin's last prompt length selects, its fast usage at the fast cells beside it and its
    // unmetered usage at that same set, or the share of the account's own window its shared window set picks, the
    // 5-hour window where the set has it and the 7-day window otherwise, through the model's anchor rates on that
    // window over its tier's capacity ratio there, both under the pin's regime. The 7-day anchor rates alone are the
    // Pro-week figure. Each account's window set is read once per request.
    let revisions = state.pricing.revisions();
    // Both spend reads are in, so the fold applies the poll's rows in one call, and before the passes, so a session
    // the fill named reads its figures in this poll.
    state.spend.fold(poll, &revisions, &state.tables, now);
    let prices = revisions.in_force(now);
    let rates = BurnRates {
        dollars: prices.rate_table(),
        five_hour: state.tables.regime_rates(FIVE_HOUR, now),
        seven_day: state.tables.regime_rates(SEVEN_DAY, now),
        fast: prices.fast_rate_table(),
    };
    let pricing = BurnPricing {
        snapshot: &snapshot,
        reported: &reported,
        rates: &rates,
        tables: &state.tables,
        now,
    };
    // Every standing carries its session's creation time, an auxiliary-only one and one past the expiry among them,
    // so a row reads it whichever pin, if any, the row shows.
    let created: std::collections::BTreeMap<(String, i64, String), i64> = pins
        .iter()
        .filter_map(|standing| Some((pin_key(&standing.pin)?, standing.session_first_seen_at?)))
        .collect();
    // The router decays each pin's copies to the read and reads its runway in the same ask, so the burn priced below
    // and the runway are one instant's, and this crate projects nothing.
    for standing in pins.into_iter().filter(|s| s.pin.last_used_at >= cutoff) {
        let Some(key) = pin_key(&standing.pin) else {
            continue;
        };
        agents_by_session
            .entry(key.clone())
            .or_default()
            .insert(standing.pin.agent_id.clone());
        // A pin only auxiliary requests have used holds no conversation: it neither stands for the row nor draws a
        // line, though its agent counts among the session's.
        if standing.auxiliary_only {
            continue;
        }
        agent_pins.entry(key).or_default().push(standing);
    }
    // A session holds a pin per agent and the row shows one of its conversation pins: main's newest where main holds
    // one, else the newest of its agents'.
    let rank = |s: &PinStanding| (s.pin.agent_id == MAIN_AGENT, s.pin.last_used_at);
    for (key, standings) in &agent_pins {
        let Some(standing) = standings
            .iter()
            .reduce(|best, s| if rank(s) > rank(best) { s } else { best })
        else {
            continue;
        };
        let p = &standing.pin;
        let priced = price_pin_burn(p, standing.burn_unit, &pricing);
        rows.insert(
            key.clone(),
            Session {
                agent_id: Some(p.agent_id.clone()),
                account_id: Some(p.account_id.clone()),
                epoch: Some(p.epoch),
                last_used_at: standings.iter().map(|s| s.pin.last_used_at).max(),
                pinned: true,
                burn: priced.burn,
                burn_unit: Some(standing.burn_unit),
                burn_window: priced.window.map(String::from),
                burn_equivalent: priced.equivalent,
                burn_at: priced.at,
                runway_ms: standing.runway_ms,
                runway_bounded: standing.runway_bounded,
                factor: standing.slow_down.map(|s| s.factor),
                speed: standing.slow_down.map(|s| s.speed),
                cycle_ms: standing.cycle_ms,
                cache_ttl_ms: Some(p.cache_ttl_ms),
                warm: Some(now - p.last_used_at < p.cache_ttl_ms),
                last_input_tokens: Some(p.last_input_tokens),
                effort: p.effort.clone(),
                last_refresh_at: Some(p.last_refresh_at),
                shed_from: p.shed.as_ref().map(|shed| shed.from.clone()),
                shed_reason: p.shed.as_ref().and_then(|shed| shed.reason.clone()),
                ..unpinned_row(
                    &key.0,
                    key.1,
                    &key.2,
                    Some(&p.model),
                    created.get(key).copied(),
                )
            },
        );
    }
    // Every row is keyed by its own session, as the pins are, so a session's requests land on the row of its pin.
    for a in activity {
        let key = activity_key(&a);
        let created_at = created.get(&key).copied();
        let entry = rows.entry(key).or_insert_with(|| {
            unpinned_row(
                &a.key_id,
                a.key_generation,
                &a.session_id,
                Some(&a.model),
                created_at,
            )
        });
        entry.last_request_at = Some(a.last_request_at);
        if entry.account_id.is_none() {
            // Without a pin the serving account is the ledger's: the newest attempt of the newest request.
            entry.account_id = a.account_id;
        }
    }
    for (key, view) in &engine {
        let entry = rows.entry(key.clone()).or_insert_with(|| {
            unpinned_row(&key.0, key.1, &key.2, None, created.get(key).copied())
        });
        // A row whose generation no live key holds is read-only: the engine's sessions and recipes are keyed
        // under the live generation alone and cannot belong to it, so it carries no companion or recipe. A
        // deleted key's session outlives the key in the engine until the engine's next tick ends it, so a session
        // the engine still holds is skipped on `historical` as well.
        if live.get(&key.0) != Some(&key.1) {
            continue;
        }
        entry.companion = word(view.companion);
        entry.companion_since = view.companion_since;
        entry.agents = view.agents as u64;
        entry.keepalive = SessionKeepalive::from(&view.keepalive);
        entry.lifecycle = view.lifecycles.get(MAIN_AGENT).map(|&state| {
            if view.registered {
                "registered".into()
            } else {
                lifecycle_word(state).into()
            }
        });
        entry.presence_age_seconds = view.presence_age_seconds;
    }
    for (key, entry) in rows.iter_mut() {
        let historical = live.get(&key.0) != Some(&key.1);
        entry.historical = historical;
        if historical || !engine.contains_key(key) {
            entry.agents = agents_by_session.get(key).map_or(0, |a| a.len() as u64);
        }
    }
    // Once the historical pass has settled `companion`: a session no companion reports leaves the list once the later
    // of its newest pin use and its last request is older than the non-companion timeout, the session expiry bounding
    // it. The row's `last_used_at` carries the newest use across the session's conversation pins, so a subagent's
    // request in flight with no ledger row yet keeps the row. An ended session the engine still holds is measured by
    // its end as well, its `companion_since`, so one that sent no request leaves after the timeout too.
    rows.retain(|_, entry| {
        entry.companion != NO_COMPANION
            || entry
                .last_used_at
                .max(entry.last_request_at)
                .max(entry.companion_since)
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
        let companion = entry.companion != NO_COMPANION;
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
            (a.agent_id != MAIN_AGENT, &a.agent_id, &a.model).cmp(&(
                b.agent_id != MAIN_AGENT,
                &b.agent_id,
                &b.model,
            ))
        });
        let view = engine.get(key);
        entry.agent_contexts = lines
            .into_iter()
            .map(|standing| {
                let pin = &standing.pin;
                let priced = price_pin_burn(pin, standing.burn_unit, &pricing);
                AgentContext {
                    agent_id: pin.agent_id.clone(),
                    model: pin.model.clone(),
                    account_id: pin.account_id.clone(),
                    effort: pin.effort.clone(),
                    last_input_tokens: pin.last_input_tokens,
                    cache_ttl_ms: pin.cache_ttl_ms,
                    last_used_at: pin.last_used_at,
                    warm: now - pin.last_used_at < pin.cache_ttl_ms,
                    burn: priced.burn,
                    burn_unit: Some(standing.burn_unit),
                    burn_window: priced.window.map(String::from),
                    burn_equivalent: priced.equivalent,
                    burn_at: priced.at,
                    created_at: standing.first_seen_at,
                    lifecycle: view
                        .and_then(|view| view.lifecycles.get(&pin.agent_id))
                        .map(|&state| lifecycle_word(state).into()),
                }
            })
            .collect();
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
        if entry.model.is_none() {
            entry.model = Some(recipe.model.clone());
        }
        if entry.account_id.is_none() {
            entry.account_id = recipe.account_id.clone();
        }
    }
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
        entry.forked_from = identity.and_then(|i| i.forked_from.clone());
        entry.fork_depth = identity.and_then(|i| i.fork_depth);
    }
    // The count is the fold's over the session's life, so Requests and Cost read the same requests. The activity's
    // `last_request_at` stays, which the listing's cutoff and the retention above read.
    let fold = state.spend.read();
    for (key, entry) in rows.iter_mut() {
        let Some(figures) = fold.answer(key, now) else {
            continue;
        };
        entry.user_agent = figures.user_agent;
        entry.spend = figures.spend;
        entry.day = figures.day;
        entry.agent_requests = figures.agent_requests;
        entry.requests = figures.requests;
    }
    fold.retain(|key| rows.contains_key(key), now, state.session_expiry);
    let mut out: Vec<Session> = rows.into_values().collect();
    let latest = |row: &Session| {
        row.last_used_at
            .unwrap_or(0)
            .max(row.last_request_at.unwrap_or(0))
    };
    out.sort_by_key(|row| std::cmp::Reverse(latest(row)));
    Ok(out)
}

/// What every pin's burn is priced through, read once per request.
struct BurnPricing<'a> {
    snapshot: &'a phoebis_routing::RegistrySnapshot,
    reported: &'a std::collections::BTreeMap<String, phoebis_routing::UtilizationSample>,
    rates: &'a BurnRates,
    tables: &'a phoebis_ledger::PlanTables,
    now: i64,
}

/// The pin's burn per hour in its unit, its window, its Pro-week figure and the instant it is decayed to, priced on the
/// account the pin sits on: the figures the row carries for its display pin and a line for its own.
struct PricedBurn {
    burn: Option<f64>,
    window: Option<&'static str>,
    equivalent: Option<f64>,
    at: Option<i64>,
}

/// Prices the row's display pin and each line's pin alike, so a row and its display pin's line answer the same figures.
/// The unit is the pin's standing's, which the router reads through the selector's own predicate, so a row or a line
/// never disagrees with the selector.
fn price_pin_burn(p: &phoebis_store::PinRow, unit: BurnUnit, pricing: &BurnPricing) -> PricedBurn {
    let BurnPricing {
        snapshot,
        reported,
        rates,
        tables,
        now,
    } = *pricing;
    let (burn, window, equivalent) = match unit {
        BurnUnit::Usd => (
            rates
                .price_pin(BurnUnit::Usd, p, FIVE_HOUR)
                .map(|b| b * 3_600.0),
            None,
            None,
        ),
        BurnUnit::WindowShare => {
            let window = if reported
                .get(&p.account_id)
                .is_none_or(|sample| sample.has_window(FIVE_HOUR))
            {
                FIVE_HOUR
            } else {
                SEVEN_DAY
            };
            // The selector's own read of the plan, so a plan the cost model does not price (a free
            // organization) reads unpriced here as it does there.
            let tier = snapshot.priced_tier(&p.account_id);
            // An account tier the cost model does not size prices nothing, as does a model without a weight, so
            // the figure reads null rather than a guess.
            let burn = tier
                .and_then(|t| tables.tier_factor(t, window, p.entrypoint.as_deref(), now))
                .and_then(|factor| {
                    rates
                        .price_pin(BurnUnit::WindowShare, p, window)
                        .map(|b| b * 3_600.0 / factor)
                });
            let equivalent = rates
                .price_pin(BurnUnit::WindowShare, p, SEVEN_DAY)
                .map(|b| b * 3_600.0);
            (burn, Some(window), equivalent)
        }
    };
    PricedBurn {
        burn,
        window,
        equivalent,
        at: p.burn_at,
    }
}

/// The row a session's ledger activity lands on, spelled once so the fork-facts read names the row the activity pass
/// files it under.
fn activity_key(a: &phoebis_ledger::SessionActivity) -> (String, i64, String) {
    (a.key_id.clone(), a.key_generation, a.session_id.clone())
}

/// The row of a session no pin stands for, the model the ledger's activity names where it names one.
fn unpinned_row(
    key_id: &str,
    key_generation: i64,
    session_id: &str,
    model: Option<&str>,
    created_at: Option<i64>,
) -> Session {
    Session {
        key_id: key_id.into(),
        key_generation,
        session_id: session_id.into(),
        model: model.map(String::from),
        created_at,
        companion: NO_COMPANION.into(),
        ..Session::default()
    }
}

/// The engine's word on the wire, the match with no wildcard so a sixth word fails to build.
fn lifecycle_word(state: phoebis_keepalive::AgentState) -> &'static str {
    use phoebis_keepalive::AgentState::*;
    match state {
        Busy => "busy",
        Idle => "idle",
        Parked => "parked",
        Released => "released",
        Unknown => "unknown",
    }
}

/// `CompanionState::None`'s serde word, the one the retention and the lines pass test a row's `companion` against.
const NO_COMPANION: &str = "none";

/// The word serde writes for a member of one of the engine's closed sets, so the row spells it as the engine does.
fn word(value: impl serde::Serialize) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(word)) => word,
        other => unreachable!("a closed set's member serializes to its word, not {other:?}"),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openapi::round_trip::{round_trips, round_trips_each};

    #[test]
    fn the_session_fixtures_round_trip_through_their_declared_types() {
        round_trips_each::<Session>("sessions", &["sessions", "*"]);
        round_trips::<SessionCosts>("sessions_costs");
    }
}
