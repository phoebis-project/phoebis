//! The sessions view: the live pins joined with what the ledger saw, and the per-model cost card the console shows
//! beside them.

use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use phoebis_ledger::SessionSpendKey;
use phoebis_routing::{BurnRates, BurnUnit, PinStanding, FIVE_HOUR, MAIN_AGENT, SEVEN_DAY};
use utoipa::ToSchema;

use crate::openapi::{LedgerFailure, RouterUnavailable};
use crate::routes::router_unavailable;
use crate::session_spend::{agent_of, day_of, spend_of, AgentRequests, SessionDay, SessionSpend};
use crate::AdminState;

/// The live pins joined with what the ledger saw and with what the session engine and the cache warming hold, one row
/// per session.
#[derive(serde::Serialize, ToSchema)]
pub(crate) struct SessionList {
    pub sessions: Vec<Session>,
}

/// One session row. On a row with no pin `pinned_at` and the keys from `burn` to `move_drop_cost` read null.
#[derive(Default, serde::Serialize, ToSchema)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(deny_unknown_fields)]
pub(crate) struct Session {
    pub key_id: String,
    pub key_generation: i64,
    pub session_id: String,
    pub agent_id: Option<String>,
    pub model: Option<String>,
    pub account_id: Option<String>,
    /// The placements of the session's main agent under the key: the ledger's `pin_created` and `pin_migrated` events
    /// of main, at or after `key_generation` and, on a historical row whose key id a live key holds, before that key's
    /// generation. 0 where the ledger holds none, as for a session the engine knows from a registration alone.
    pub placements: u64,
    /// The instant the row's pin was written on the account it stands on, which a move sets anew. Null on a row with
    /// no pin.
    pub pinned_at: Option<i64>,
    /// The newest use across the session's conversation pins, main's and its agents', null without a pin.
    pub last_used_at: Option<i64>,
    /// The session's newest client request under its key over its life, refused ones included and probes out, null
    /// where the ledger holds none.
    pub last_request_at: Option<i64>,
    /// The session's newest request that is neither a probe nor a token count, refused ones included, over the span
    /// `last_request_at` reads, so it is never later than that member. Null while the ledger holds none, as before a
    /// first turn.
    pub last_inference_at: Option<i64>,
    /// The session's first request under its key and generation that is no probe, refused ones included, with no
    /// expiry bound. It moves when the purge removes the first requests. Null where the ledger holds none.
    pub first_request_at: Option<i64>,
    /// The user agent of the session's newest client request as the client sent it, the newest served one's where the
    /// session has one.
    pub user_agent: Option<String>,
    /// The session's spend over its requests the retention keeps under its key, from the ledger's per-session totals,
    /// null where the ledger holds no request of it the router did not refuse. Its `requests` is the session's count of
    /// client requests, which a reader takes as 0 where `spend` is null.
    pub spend: Option<SessionSpend>,
    /// The session's client requests of the 24 whole hours before the current hour and the current hour so far, by
    /// class, and their spend.
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
    /// The display pin's plan copy per hour in dollars at the API price list in force, priced in the read. Null
    /// where `burn_unit` is not `window_share`, on a row with no pin and where the dollar table lists no row for the
    /// pin's model or a token type the pin burns has no rate in it. It answers a figure where `burn` reads null for a
    /// tier the cost model does not size or a model it does not weigh, since the dollar table reads neither.
    pub burn_usd: Option<f64>,
    /// The instant the served burn is decayed to, the read's own, so a reader that decays it again from here adds
    /// nothing at the read. Null for a pin never renewed with usage, whose burn has nothing to decay.
    pub burn_at: Option<i64>,
    /// The time until the pin's account's projection reaches the pool's ceiling on the window that binds among those
    /// the pin's model draws, with the pin's burn counted once and no rewrite. Null for an unbounded runway and where
    /// the account projects none of those windows. A window's runway that reaches the earliest of the window's next
    /// reset and the account's end that lies ahead is unbounded, since the quota resets or the account ends before the
    /// ceiling is reached.
    pub runway_ms: Option<i64>,
    /// Null where the account projects none of the windows the pin's model draws, false for an unbounded runway or one
    /// that reaches the earliest of its window's reset and the account's end that lies ahead, true where `runway_ms` is
    /// set.
    pub runway_bounded: Option<bool>,
    /// The pin's account's factor under the slow-down, from the entry in force, null on a row with no pin and where no
    /// entry is in force.
    pub factor: Option<f64>,
    /// The speed the holds realized inside a slow-down span, from 0 to 1, null where `factor` is null and outside a
    /// span.
    pub speed: Option<f64>,
    /// The pin's cycle in milliseconds, null until two requests of the pin after its placement have measured a gap
    /// under the pool's burn average, and on a row with no pin.
    pub cycle_ms: Option<f64>,
    /// The cache TTL in force at the pin's last refresh.
    pub cache_ttl_ms: Option<i64>,
    /// Whether the pinned account still holds the session's prompt cache, from the pin's last refresh and its cache
    /// TTL as the selector reads it. Null without a pin.
    pub warm: Option<bool>,
    /// The context of the row's pin, the main agent's conversation pin where it holds one: its last turn's prompt in
    /// tokens, cache reads and writes included, 0 before its first turn.
    pub last_input_tokens: Option<u64>,
    /// The effective effort of the last turn on the row's pin, the word as the client sent it, null where it named none
    /// and on a row with no pin. A token count and an auxiliary call leave it.
    pub effort: Option<String>,
    /// The pin's last main, subagent or compaction turn, or a keepalive probe's renewal.
    pub last_refresh_at: Option<i64>,
    /// Null with no stamp and on a row with no pin.
    pub shed: Option<SessionShed>,
    /// Null where the pin holds no mark and on a row with no pin.
    pub mark: Option<SessionMark>,
    /// The display pin's agent ended and its next request has not arrived, as the line's `ended`. Null on a row with
    /// no pin.
    pub ended: Option<bool>,
    /// The display pin's lane is the fast lane, as a shedding round tests it. Null on a row with no pin.
    pub fast: Option<bool>,
    /// The display pin's session holds account state, so the pin stays on its account until a portable turn of the
    /// conversation, or until a request bound by web search results alone leaves it because its account cannot serve
    /// in time. Null on a row with no pin.
    pub account_bound: Option<bool>,
    /// The figure a shedding round's pick compares for the display pin: the Pro share its rewrite spends on the week
    /// its model draws, the Fable week for a model that draws it and the 7-day window otherwise, plus the share of the
    /// thinking its move would drop, `move_drop_cost`. Null where neither is priced above zero, as where the cost model
    /// weighs no rewrite of the model on that week or the pin holds no prompt size, and on a row with no pin. Null too
    /// for a pin that no round can move: one with no destination, one whose pool left the registry and one that holds
    /// account state. A reader takes a null as 0, as the pick does. The figure stands whatever the pin's warmth, where
    /// the pick reads a cold pin's move at `move_drop_cost` alone unless the pool's `warm_sessions` is `always`.
    pub move_cost: Option<f64>,
    /// The size a shedding round's order divides the pin's burn by while the pin is warm, in tokens: the prompt its
    /// move would rewrite on the destination a round would plan for it, plus the thinking the move would drop at the
    /// weight of output. The destination is the one of a round that the pin's own request fires. Null on a row with no
    /// pin and where the pin holds no prompt size.
    pub move_size: Option<f64>,
    /// The thinking the pin's move would drop, at the weight of output and in tokens: the size a round's order divides
    /// a cold pin's burn by. Null on a row with no pin, where the move would drop no thinking and where the cost model
    /// answers no weight.
    pub move_drop_size: Option<f64>,
    /// The share of the dropped thinking as output on the window `move_cost` reads: the cost a round's pick compares
    /// for a cold pin. Null where `move_drop_size` is.
    pub move_drop_cost: Option<f64>,
    pub historical: bool,
    /// `reporting`, `silent`, or `none`.
    #[schema(example = "reporting")]
    pub companion: String,
    pub companion_since: Option<i64>,
    pub agents: u64,
    /// One line per conversation pin the row lists, main's included, ordered by agent id then model, main first:
    /// on a companion row the pins of the agents the engine holds and has not released, on a row whose `companion`
    /// is `none` the pins used within the lesser of `sessions.non_companion_idle_seconds` and the session expiry. Empty
    /// where none draws.
    pub agent_contexts: Vec<AgentContext>,
    /// One entry per agent of the session that the registry holds a stored set of reported facts for, ordered by agent
    /// id, an agent whose record left the engine and a released one among them. Empty where no agent's set is stored
    /// and on a row of an earlier holder.
    pub agent_facts: Vec<SessionAgentFacts>,
    /// One entry per agent the ledger holds requests of, its requests and their spend over every model it ran, with
    /// the model, the type, the account and the instant of its newest request.
    pub agent_requests: Vec<AgentRequests>,
    /// What the session's companion reported of it, read from the registry's stored sets, so a finished session's row
    /// answers it too. Each member reads null where no set is stored and on a row of an earlier holder.
    pub facts: SessionFacts,
    /// Main's recipe, or the newest other agent's where main holds none.
    pub keepalive: SessionKeepalive,
    /// Main's lifecycle word read off the session engine's state and the cache warming's cause, whether or not a recipe
    /// stands: `busy`, `idle`, `parked`, `registered`, `released` or `unknown`, where `registered` is a main busy
    /// awaiting a real request since the mod registered it or started it again. Null where no mod reports the session.
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

/// A pin's shed stamp, written by a shedding round's move or a failover's landing.
#[derive(serde::Serialize, ToSchema)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionShed {
    /// The account a shedding round's move or a failover's landing took the pin from, which keeps the pin off that
    /// account for the dwell. No mark: the pin's mark is the row's `mark`.
    pub from: String,
    /// The instant of the stamp `from` names.
    pub at: i64,
    /// Why the move that wrote the stamp took the pin off `from`: `round` for a shedding round's move, the failover's
    /// reason word (`cooldown`, `fast_lane`, `mark`) for a failover's landing. The stamp records a move that happened,
    /// never a mark. Null on a pin stamped before the pin carried a reason, which reads as a round's move.
    #[schema(example = "round")]
    pub reason: Option<String>,
}

/// A shedding round's mark on a pin.
#[derive(serde::Serialize, ToSchema)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionMark {
    /// The account a shedding round's mark on the pin was made on, the account the session is to leave. The mark
    /// stands while this is the pin's account.
    pub on: String,
    /// When the round made the mark.
    pub at: i64,
    /// The window whose trigger made the mark.
    #[schema(example = "five_hour")]
    pub window: String,
}

impl From<&phoebis_store::PinMark> for SessionMark {
    fn from(mark: &phoebis_store::PinMark) -> Self {
        SessionMark {
            on: mark.on.clone(),
            at: mark.at,
            window: mark.window.clone(),
        }
    }
}

/// One conversation pin the row lists as a line, read from this pin alone.
#[derive(serde::Serialize, ToSchema)]
#[cfg_attr(test, derive(serde::Deserialize))]
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
    /// When a request last used the pin, the instant the non-companion timeout reads.
    pub last_used_at: i64,
    /// Whether the cache entry the pin warms is still alive at the read: `now` is before the pin's last refresh plus
    /// `cache_ttl_ms`, as the selector reads it.
    pub warm: bool,
    /// The first instant `warm` answers false for the pin, Unix milliseconds.
    pub warm_until: i64,
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
    /// The pin's plan copy per hour in dollars, as the row's `burn_usd`.
    pub burn_usd: Option<f64>,
    /// The instant the pin's served burn is decayed to, as the row's `burn_at`. Null for a pin never renewed with
    /// usage.
    pub burn_at: Option<i64>,
    /// The instant the pin was written on the account it stands on, as the row's `pinned_at`.
    pub pinned_at: i64,
    /// The pin's agent ended and its next request has not arrived. The router leaves such a pin out of a round.
    pub ended: bool,
    /// The pin's lane is the fast lane, as the row's `fast`.
    pub fast: bool,
    /// This pin's session holds account state, as the row's `account_bound` reads it for the display pin.
    pub account_bound: bool,
    /// The figure a shedding round's pick compares for this pin, as the row's `move_cost`, the dropped thinking's share
    /// included. Null where neither the rewrite nor the dropped thinking is priced above zero. Null too for a pin that
    /// no round can move: one with no destination, one whose pool left the registry and one that holds account state.
    pub move_cost: Option<f64>,
    /// The size a shedding round's order divides this pin's burn by while the pin is warm, as the row's `move_size`.
    /// Null where the pin holds no prompt size.
    pub move_size: Option<f64>,
    /// The thinking this pin's move would drop at the weight of output, as the row's `move_drop_size`. Null where the
    /// move would drop no thinking and where the cost model answers no weight.
    pub move_drop_size: Option<f64>,
    /// The share of the dropped thinking as output that a round's pick compares for this pin while it is cold, as the
    /// row's `move_drop_cost`. Null where `move_drop_size` is.
    pub move_drop_cost: Option<f64>,
    /// The agent's lifecycle word read off the session engine's state and the cache warming's cause: `busy`, `idle`,
    /// `parked`, `registered`, `released` or `unknown`, where `registered` is an agent busy awaiting a real request
    /// since the mod registered it or started it again. Null where no mod reports it.
    #[schema(example = "idle")]
    pub lifecycle: Option<String>,
}

/// Main's recipe as the session row reads it, or the newest other agent's where main holds none, every member null
/// while no agent holds one. A session whose own option holds the cache warming off reads the cause
/// `session_opted_out` and the state `ended`.
#[derive(Default, serde::Serialize, ToSchema)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionKeepalive {
    #[schema(value_type = Option<String>)]
    pub cause: Option<phoebis_session_engine::Reason>,
    #[schema(value_type = Option<String>)]
    pub last_guard: Option<phoebis_session_engine::Guard>,
    pub next_probe_at: Option<i64>,
    pub state: Option<String>,
}

impl From<&phoebis_session_engine::KeepaliveSummary> for SessionKeepalive {
    fn from(summary: &phoebis_session_engine::KeepaliveSummary) -> Self {
        SessionKeepalive {
            cause: summary.cause,
            last_guard: summary.last_guard,
            next_probe_at: summary.next_probe_at,
            state: summary.state.map(word),
        }
    }
}

/// The session's reported facts, each cleaned by the router before it is kept.
#[derive(Default, serde::Serialize, ToSchema)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionFacts {
    pub branch: Option<String>,
    pub cwd: Option<String>,
    pub project: Option<String>,
    /// The origin remote as `host/path`. A raw remote loses its scheme, user information, port, query, fragment, one
    /// trailing `.git` and each trailing slash, and the ASCII letters of its host fold to lower case.
    pub repository: Option<String>,
    pub repository_root: Option<String>,
    pub title: Option<String>,
}

impl From<&phoebis_session_engine::SessionFacts> for SessionFacts {
    fn from(facts: &phoebis_session_engine::SessionFacts) -> Self {
        SessionFacts {
            branch: facts.branch().map(str::to_owned),
            cwd: facts.cwd().map(str::to_owned),
            project: facts.project().map(str::to_owned),
            repository: facts.repository().map(str::to_owned),
            repository_root: facts.repository_root().map(str::to_owned),
            title: facts.title().map(str::to_owned),
        }
    }
}

/// One agent's reported facts.
#[derive(serde::Serialize, ToSchema)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionAgentFacts {
    pub agent_id: String,
    pub agent_type: Option<String>,
    pub description: Option<String>,
    pub name: Option<String>,
}

/// The per-model cost card the console shows beside the sessions.
#[derive(serde::Serialize, ToSchema)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionCosts {
    /// The `effective` of the price revision in force as its file wrote it, a date `YYYY-MM-DD` or an instant
    /// `YYYY-MM-DDTHH:MM:SSZ`.
    pub effective: String,
    pub models: Vec<CostRow>,
}

#[derive(serde::Serialize, ToSchema)]
#[cfg_attr(test, derive(serde::Deserialize))]
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
        Some(session_key(&p.key_id, *live.get(&p.key_id)?, &p.session_id))
    };
    let now = (state.clock)();
    let cutoff = now - state.session_expiry.as_millis() as i64;
    let idle_bound = now
        - state
            .non_companion_idle
            .min(state.session_expiry)
            .as_millis() as i64;
    let views: std::collections::BTreeMap<SessionSpendKey, phoebis_session_engine::SessionView> =
        state
            .engine
            .sessions()
            .into_iter()
            .map(|s| (session_key(&s.key_id, s.key_generation, &s.session_id), s))
            .collect();
    let mut candidates: std::collections::BTreeSet<SessionSpendKey> = pins
        .iter()
        .filter(|standing| standing.pin.last_used_at >= idle_bound)
        .filter_map(|standing| pin_key(&standing.pin))
        .chain(views.keys().cloned())
        .collect();
    // One read for the activity and the fork facts: the keys the pin and engine passes can surface are known before
    // the read, the activity's come from the read itself, and a row's fork facts come from the requests of its own
    // session whichever pass surfaced it. Only the sessions the retain below can keep are named: every engine
    // session, and one with a pin use or a request at or after `idle_bound`.
    let live_keys = phoebis_ledger::LiveKeys(live.clone());
    // One read of the revisions prices the spend in the read below and the burn after it, so a reload that lands
    // during the read cannot price one answer under two revisions.
    let revisions = state.pricing.revisions();
    let (prices, tables) = (revisions.clone(), state.tables.clone());
    let read = state.ledger.db().read_snapshot(move |c| {
        // Grouped by the client's own session, which the router keys pins under, so a request shows on the row of
        // the pin it took.
        let activity = phoebis_ledger::session_activity(c, cutoff)?;
        candidates.extend(
            activity
                .iter()
                .filter(|a| a.last_request_at >= idle_bound)
                .map(|a| session_key(&a.key_id, a.key_generation, &a.session_id)),
        );
        let keys: Vec<SessionSpendKey> = candidates.into_iter().collect();
        let figures = phoebis_ledger::session_figures(c, &keys, &prices, &tables, now)?;
        Ok((
            activity,
            phoebis_ledger::session_identities(c, &keys)?,
            phoebis_ledger::session_placements(c, &keys, &live_keys)?,
            figures,
        ))
    });
    // Each read is a snapshot of its own, so they wait together and their order carries no meaning.
    let (reported, read) = tokio::join!(crate::routes::reported_samples(state), read);
    let (activity, identities, placements, mut figures) = match read {
        Ok(read) => read,
        Err(e) => return Err(crate::analytics::ledger_failure(e)),
    };
    // The lacked windows depend on the account alone, so they are collected once for every share map of the answer.
    let lacked: Vec<(&str, Vec<&'static str>)> = reported
        .keys()
        .filter_map(|account| {
            let windows = crate::routes::windows_lacked(&reported, account);
            (!windows.is_empty()).then_some((account.as_str(), windows))
        })
        .collect();
    let mut rows: std::collections::BTreeMap<SessionSpendKey, Session> =
        std::collections::BTreeMap::new();
    // The session engine's own agent count answers `agents` when a session has an engine view, so every agent a pin
    // names is counted here as the fallback where it does not.
    let mut agents_by_session: std::collections::BTreeMap<
        SessionSpendKey,
        std::collections::BTreeSet<String>,
    > = std::collections::BTreeMap::new();
    // Every conversation pin of each session, main's among them, which the lines pass reads after the historical one.
    let mut agent_pins: std::collections::BTreeMap<SessionSpendKey, Vec<PinStanding>> =
        std::collections::BTreeMap::new();
    // A pin's burn is typed tokens a second, priced here on the account it sits on: dollars through the table
    // in force at the set the pin's last prompt length selects, its fast usage at the fast cells beside it and its
    // unmetered usage at that same set, or the share of the account's own window its shared window set picks, the
    // 5-hour window where the set has it and the 7-day window otherwise, through the model's anchor rates on that
    // window over its tier's capacity ratio there, both under the pin's regime. The 7-day anchor rates alone are the
    // Pro-week figure. Each account's window set is read once per request.
    let pricing = BurnPricing::new(state, &revisions, &snapshot, &reported, now);
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
        if standing.pin.auxiliary_only {
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
        let line = agent_line(standing, &pricing);
        rows.insert(
            key.clone(),
            Session {
                agent_id: Some(line.agent_id),
                account_id: Some(line.account_id),
                last_used_at: standings.iter().map(|s| s.pin.last_used_at).max(),
                pinned: true,
                burn: line.burn,
                burn_unit: line.burn_unit,
                burn_window: line.burn_window,
                burn_equivalent: line.burn_equivalent,
                burn_usd: line.burn_usd,
                burn_at: line.burn_at,
                runway_ms: standing.runway_ms,
                runway_bounded: standing.runway_bounded,
                factor: standing.slow_down.map(|s| s.factor),
                speed: standing.slow_down.filter(|s| s.slowed).map(|s| s.speed),
                cycle_ms: standing.cycle_ms,
                cache_ttl_ms: Some(line.cache_ttl_ms),
                warm: Some(line.warm),
                last_input_tokens: Some(line.last_input_tokens),
                effort: line.effort,
                last_refresh_at: Some(p.last_refresh_at),
                shed: p.shed.as_ref().map(|shed| SessionShed {
                    from: shed.from.clone(),
                    at: shed.at,
                    reason: shed.reason.clone(),
                }),
                mark: p.mark.as_ref().map(SessionMark::from),
                ended: Some(line.ended),
                fast: Some(line.fast),
                account_bound: Some(line.account_bound),
                move_cost: line.move_cost,
                move_size: line.move_size,
                move_drop_size: line.move_drop_size,
                move_drop_cost: line.move_drop_cost,
                pinned_at: Some(line.pinned_at),
                ..unpinned_row(key, Some(&p.model))
            },
        );
    }
    // Every row is keyed by its own session, as the pins are, so a session's requests land on the row of its pin.
    for a in activity {
        let key = session_key(&a.key_id, a.key_generation, &a.session_id);
        let entry = rows
            .entry(key)
            .or_insert_with_key(|key| unpinned_row(key, Some(&a.model)));
        entry.last_request_at = Some(a.last_request_at);
        entry.last_inference_at = a.last_inference_at;
        if entry.account_id.is_none() {
            // Without a pin the serving account is the ledger's: the newest attempt of the newest request.
            entry.account_id = a.account_id;
        }
        // The totals pick their user agent among the served requests alone, so a session whose every request was
        // refused keeps the agent of its newest one, which the totals' own replaces below where one was served.
        entry.user_agent = a.user_agent;
    }
    for (key, view) in &views {
        let entry = rows
            .entry(key.clone())
            .or_insert_with_key(|key| unpinned_row(key, None));
        // A row whose generation no live key holds is read-only: the engine's sessions and recipes are keyed
        // under the live generation alone and cannot belong to it, so it carries no companion, fact or recipe. A
        // deleted key's session outlives the key in the engine until the engine's next tick ends it, so a session
        // the engine still holds is skipped on `historical` as well.
        if live.get(&key.key_id) != Some(&key.key_generation) {
            continue;
        }
        entry.companion = word(view.companion);
        entry.companion_since = view.companion_since;
        entry.agents = view.agents as u64;
        entry.keepalive = SessionKeepalive::from(&view.keepalive);
        entry.lifecycle = view
            .lifecycles
            .get(MAIN_AGENT)
            .map(|&lifecycle| lifecycle_word(lifecycle).into());
        entry.presence_age_seconds = view.presence_age_seconds;
    }
    for (key, entry) in rows.iter_mut() {
        let historical = live.get(&key.key_id) != Some(&key.key_generation);
        entry.historical = historical;
        if historical || !views.contains_key(key) {
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
    // Behind the retention, so the registry is asked for the listed rows alone. A stored set belongs to the generation
    // that reported it, so a row of an earlier holder is not asked for and reads none.
    let stored = state
        .engine
        .stored_facts(
            rows.iter()
                .filter(|(_, entry)| !entry.historical)
                .map(|(key, _)| {
                    (
                        key.key_id.clone(),
                        key.key_generation,
                        key.session_id.clone(),
                    )
                })
                .collect(),
        )
        .await;
    for ((key_id, key_generation, session_id), facts) in stored {
        let Some(entry) = rows.get_mut(&SessionSpendKey {
            key_id,
            key_generation,
            session_id,
        }) else {
            continue;
        };
        entry.facts = SessionFacts::from(&facts.session);
        entry.agent_facts = facts
            .agents
            .into_iter()
            .map(|(agent_id, agent)| SessionAgentFacts {
                agent_id,
                agent_type: agent.agent_type().map(str::to_owned),
                description: agent.description().map(str::to_owned),
                name: agent.name().map(str::to_owned),
            })
            .collect();
    }
    // A session the engine knows only from a registration carries no pin and no ledger row, so its model and
    // serving account come from its main agent's recipe instead, the first the session's agents sort to. A
    // recipe belongs to the live key alone, so it joins under that generation and never reaches a historical row.
    let mut recipe_by_session: std::collections::BTreeMap<
        SessionSpendKey,
        phoebis_session_engine::RecipeView,
    > = std::collections::BTreeMap::new();
    let mut unreleased: std::collections::BTreeMap<
        SessionSpendKey,
        std::collections::BTreeSet<String>,
    > = std::collections::BTreeMap::new();
    for recipe in state.engine.recipes() {
        let Some(&generation) = live.get(&recipe.key_id) else {
            continue;
        };
        let key = session_key(&recipe.key_id, generation, &recipe.session_id);
        if recipe.agent_state != phoebis_session_engine::Lifecycle::Released {
            unreleased
                .entry(key.clone())
                .or_default()
                .insert(recipe.agent_id.clone());
        }
        recipe_by_session.entry(key).or_insert(recipe);
    }
    // The agent lines: on a companion row one per conversation pin of every agent the companion carries a recipe for
    // and has not released, main's included, and on any other row one per conversation pin used within the timeout.
    for (key, entry) in rows.iter_mut() {
        let companion = entry.companion != NO_COMPANION;
        let mut lines: Vec<&PinStanding> = agent_pins
            .get(key)
            .into_iter()
            .flatten()
            .filter(|standing| {
                if companion {
                    unreleased
                        .get(key)
                        .is_some_and(|agents| agents.contains(&standing.pin.agent_id))
                } else {
                    standing.pin.last_used_at >= idle_bound
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
        let view = views.get(key);
        entry.agent_contexts = lines
            .into_iter()
            .map(|standing| AgentContext {
                lifecycle: view
                    .and_then(|view| view.lifecycles.get(&standing.pin.agent_id))
                    .map(|&lifecycle| lifecycle_word(lifecycle).into()),
                ..agent_line(standing, &pricing)
            })
            .collect();
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
    let mut identity_by_key: std::collections::BTreeMap<
        SessionSpendKey,
        phoebis_ledger::SessionIdentity,
    > = identities
        .into_iter()
        .map(|i| (session_key(&i.key_id, i.key_generation, &i.session_id), i))
        .collect();
    for (key, entry) in rows.iter_mut() {
        let identity = identity_by_key.remove(key);
        entry.fork_depth = identity.as_ref().and_then(|i| i.fork_depth);
        entry.first_request_at = identity.as_ref().and_then(|i| i.first_request_at);
        entry.forked_from = identity.and_then(|i| i.forked_from);
        entry.placements = placements.get(key).copied().unwrap_or(0);
    }
    // The spend reads the session's totals and carries the count Requests reads, so Requests and Cost read the same
    // requests. The two newest instants take the later of the activity's and the totals', after the retention above
    // read the activity's alone, so the rows it keeps do not move.
    for (key, entry) in rows.iter_mut() {
        let Some(phoebis_ledger::SessionFigures {
            last_request_at,
            last_inference_at,
            user_agent,
            spend,
            probes,
            day,
            agents,
        }) = figures.remove(key)
        else {
            continue;
        };
        entry.last_request_at = entry.last_request_at.max(last_request_at);
        entry.last_inference_at = entry.last_inference_at.max(last_inference_at);
        if let Some(user_agent) = user_agent {
            entry.user_agent = user_agent;
        }
        entry.spend = (spend.requests > 0 || probes.requests > 0).then(|| spend_of(spend, probes));
        entry.day = day_of(day);
        entry.agent_requests = agents.into_iter().map(agent_of).collect();
        // The totals price by the tier a row recorded, so a share of a window the account's set lacks today is dropped
        // at the answer, as the request list drops it. The account's key stays, since it marks that a plan paid.
        if let Some(spend) = entry.spend.as_mut() {
            strip_lacked(spend, &lacked);
        }
        for agent in &mut entry.agent_requests {
            strip_lacked(&mut agent.spend, &lacked);
        }
    }
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
pub(crate) struct BurnPricing<'a> {
    snapshot: &'a phoebis_routing::RegistrySnapshot,
    reported: &'a std::collections::BTreeMap<String, phoebis_routing::UtilizationSample>,
    rates: BurnRates,
    tables: &'a phoebis_ledger::PlanTables,
    now: i64,
}

impl<'a> BurnPricing<'a> {
    /// The rates of the price revision in force at `now` and of the cost model. The caller passes `revisions`, so a
    /// route that priced other figures under one read of the list prices the burn under the same revision.
    pub(crate) fn new(
        state: &'a AdminState,
        revisions: &phoebis_ledger::PriceRevisions,
        snapshot: &'a phoebis_routing::RegistrySnapshot,
        reported: &'a std::collections::BTreeMap<String, phoebis_routing::UtilizationSample>,
        now: i64,
    ) -> BurnPricing<'a> {
        let prices = revisions.in_force(now);
        BurnPricing {
            snapshot,
            reported,
            rates: BurnRates {
                dollars: prices.rate_table(),
                five_hour: state.tables.regime_rates(FIVE_HOUR, now),
                seven_day: state.tables.regime_rates(SEVEN_DAY, now),
                fast: prices.fast_rate_table(),
            },
            tables: &state.tables,
            now,
        }
    }
}

/// A pin's agent line priced through `pricing`, which a pool's pins read too, so a pin answers the same members on
/// both routes. `lifecycle` reads null: the session engine's view is the sessions route's alone.
pub(crate) fn agent_line(standing: &PinStanding, pricing: &BurnPricing) -> AgentContext {
    let pin = &standing.pin;
    let priced = price_pin_burn(pin, standing.burn_unit, pricing);
    AgentContext {
        agent_id: pin.agent_id.clone(),
        model: pin.model.clone(),
        account_id: pin.account_id.clone(),
        effort: pin.effort.clone(),
        last_input_tokens: pin.last_input_tokens,
        cache_ttl_ms: pin.cache_ttl_ms,
        last_used_at: pin.last_used_at,
        warm: phoebis_routing::warm(pin, pricing.now),
        warm_until: phoebis_routing::warm_until(pin),
        burn: priced.burn,
        burn_unit: Some(standing.burn_unit),
        burn_window: priced.window.map(String::from),
        burn_equivalent: priced.equivalent,
        burn_usd: priced.usd,
        burn_at: priced.at,
        pinned_at: pin.created_at,
        ended: pin.ended,
        fast: standing.fast(),
        account_bound: pin.account_bound,
        move_cost: standing.move_read.cost,
        move_size: standing.move_read.size,
        move_drop_size: standing.move_read.drop_size,
        move_drop_cost: standing.move_read.drop_cost,
        lifecycle: None,
    }
}

/// The pin's burn per hour in its unit, its window, its Pro-week figure, its plan dollars and the instant it is decayed
/// to, priced on the account the pin sits on: the figures the row carries for its display pin and a line for its own.
struct PricedBurn {
    burn: Option<f64>,
    window: Option<&'static str>,
    equivalent: Option<f64>,
    usd: Option<f64>,
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
    } = pricing;
    let (burn, window, equivalent, usd) = match unit {
        BurnUnit::Usd => (
            rates
                .price_pin(BurnUnit::Usd, p, FIVE_HOUR)
                .map(|b| b * 3_600.0),
            None,
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
                .and_then(|t| tables.tier_factor(t, window, p.entrypoint.as_deref(), *now))
                .and_then(|factor| {
                    rates
                        .price_pin(BurnUnit::WindowShare, p, window)
                        .map(|b| b * 3_600.0 / factor)
                });
            let equivalent = rates
                .price_pin(BurnUnit::WindowShare, p, SEVEN_DAY)
                .map(|b| b * 3_600.0);
            // The plan copy at the list price, and no other copy: fast usage draws usage credits and a classifier
            // call takes no share of a plan window, so neither is plan traffic. A model the price list lacks answers
            // no figure.
            let usd = rates.price_plan_usd(p).map(|b| b * 3_600.0);
            (burn, Some(window), equivalent, usd)
        }
    };
    PricedBurn {
        burn,
        window,
        equivalent,
        usd,
        at: p.burn_at,
    }
}

/// Takes out of `spend`'s shares and its probes' the shared windows `lacked` lists for each account whose set lacks
/// one, as `windows_lacked` names them, and leaves every account's key in place.
fn strip_lacked(spend: &mut SessionSpend, lacked: &[(&str, Vec<&'static str>)]) {
    for shares in [&mut spend.shares, &mut spend.probes.shares] {
        for (account_id, windows) in lacked {
            if let Some(account_shares) = shares.get_mut(*account_id) {
                for window in windows {
                    account_shares.remove(*window);
                }
            }
        }
    }
}

/// The row a session lands on, spelled once so the fork-facts read names the row the activity pass files the session's
/// ledger activity under.
fn session_key(key_id: &str, key_generation: i64, session_id: &str) -> SessionSpendKey {
    SessionSpendKey {
        key_id: key_id.to_string(),
        key_generation,
        session_id: session_id.to_string(),
    }
}

/// The row of a session no pin stands for, the model the ledger's activity names where it names one.
fn unpinned_row(key: &SessionSpendKey, model: Option<&str>) -> Session {
    Session {
        key_id: key.key_id.clone(),
        key_generation: key.key_generation,
        session_id: key.session_id.clone(),
        model: model.map(String::from),
        companion: NO_COMPANION.into(),
        ..Session::default()
    }
}

/// The view's lifecycle word on the wire, the match with no wildcard so a word the view gains fails to build here.
fn lifecycle_word(lifecycle: phoebis_session_engine::Lifecycle) -> &'static str {
    use phoebis_session_engine::Lifecycle::*;
    match lifecycle {
        Busy => "busy",
        Idle => "idle",
        Parked => "parked",
        Registered => "registered",
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
