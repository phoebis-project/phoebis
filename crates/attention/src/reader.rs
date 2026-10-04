//! The eight conditions, read fresh on every call: the account states and the pools from the registry, the refresh
//! runs from the store, the parked records and the ledger's degraded state from the ledger, the routing state
//! writer's from the router, the poll's standing from the usage service, and the exhaustions and drift refusals from
//! the request outcomes the bus arm keeps.

use std::collections::{HashMap, HashSet};

use phoebis_bus::{Event, Kind as RequestKind, Origin, Outcome};
use phoebis_ledger::{polled, ParkedSummary};
use phoebis_oauth::UsageService;
use phoebis_routing::RouterHandle;
use phoebis_store::{AccountKind, AccountRow, AccountState, PoolRow, ResolvedAttempt, Store};
use serde_json::json;

use crate::AttentionError;

/// How long after a run's first 429 the poll backoff condition holds, so a single 429 or a short run never reads 1.
pub const BACKOFF_ATTENTION_AFTER_MS: i64 = 30 * 60 * 1_000;

/// How many of a holder's newest settled refresh attempts one read walks: two make the condition, and the rest date
/// the run's start as far back as the read reaches, which bounds the scan a scrape pays per account.
pub const REFRESH_RUN_READ: usize = 16;

/// How long an in-flight entry may wait for its completion before the arm forgets it.
const IN_FLIGHT_TTL_MS: i64 = 15 * 60 * 1_000;

/// The subject of a drift refusal whose completion names no client version.
const UNKNOWN_VERSION: &str = "unknown";

/// The subject of the parked records served under no pool; no pool id can read `-`.
const NO_POOL: &str = "-";

/// The subject of a drift refusal whose version does not read as a release, so a malformed or oversized label shares
/// one series.
const OTHER_VERSION: &str = "other";

/// The most client versions the drift record holds. The version is the client's own user agent, so without a cap a
/// caller sending refused stamps under ever new versions would grow the record and the scrape's series without bound.
const DRIFT_SUBJECTS: usize = 16;

const RELEASE_VERSION_MAX_BYTES: usize = 32;

/// Declared worst first, so the derived order is the ranking: a pool that serves nothing, a client build refused in
/// every pool, a writer that cannot persist what the router decides (a degraded ledger also holds budgeted members
/// back), an account that serves nothing until the operator acts, a credential about to lapse, priced records the
/// ledger file keeps until the next start lands them, windows that read stale.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    PoolExhausted,
    ClientDrift,
    WriterDegraded,
    ReauthRequired,
    FundingRequired,
    RefreshFailed,
    LedgerRecordParked,
    PollBackoff,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::PoolExhausted => "pool_exhausted",
            Kind::ClientDrift => "client_drift",
            Kind::WriterDegraded => "writer_degraded",
            Kind::ReauthRequired => "reauth_required",
            Kind::FundingRequired => "funding_required",
            Kind::RefreshFailed => "refresh_failed",
            Kind::LedgerRecordParked => "ledger_record_parked",
            Kind::PollBackoff => "poll_backoff",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubjectKind {
    Account,
    Client,
    Pool,
    /// One of the process's own components: the ledger or the routing state writer, or `-` for a parked record
    /// served under no pool.
    Router,
}

impl SubjectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SubjectKind::Account => "account",
            SubjectKind::Client => "client",
            SubjectKind::Pool => "pool",
            SubjectKind::Router => "router",
        }
    }
}

/// What a condition is about: the account's or the pool's id and label, the client version as both, or the router's
/// own component as both.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subject {
    pub kind: SubjectKind,
    pub id: String,
    pub label: String,
}

impl Subject {
    fn account(account: &AccountRow) -> Subject {
        Subject {
            kind: SubjectKind::Account,
            id: account.id.clone(),
            label: account.label.clone(),
        }
    }

    fn client(version: &str) -> Subject {
        Subject {
            kind: SubjectKind::Client,
            id: version.into(),
            label: version.into(),
        }
    }

    fn pool(pool: &PoolRow) -> Subject {
        Subject {
            kind: SubjectKind::Pool,
            id: pool.id.clone(),
            label: pool.label.clone(),
        }
    }

    fn router(component: &str) -> Subject {
        Subject {
            kind: SubjectKind::Router,
            id: component.into(),
            label: component.into(),
        }
    }
}

/// The console page where the operator fixes the condition, and the row it opens there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub page: &'static str,
    pub id: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Condition {
    pub kind: Kind,
    pub subject: Subject,
    pub holds: bool,
    /// When the condition started, in Unix milliseconds; `None` while it does not hold.
    pub since: Option<i64>,
    /// The pools the condition belongs to, so a pool card filters the same list the dashboard reads.
    pub pools: Vec<String>,
    /// `None` for a drift refusal that carried no key, a router subject, and a pool deleted under a parked record,
    /// which leave nothing to open.
    pub link: Option<Link>,
    pub detail: serde_json::Value,
}

/// The request-fed state and the first-seen times of the two account states. Nothing of it is stored: a restart
/// starts every exhaustion and refusal over, and the state kinds date themselves from the first read that sees them.
#[derive(Default)]
pub(crate) struct ReaderState {
    drift: HashMap<String, Drift>,
    exhausted: HashMap<String, Exhausted>,
    in_flight: HashMap<String, InFlight>,
    state_since: HashMap<(Kind, String), i64>,
}

/// A request's key, pool, origin and kind, which its completion does not carry, held from its observation until then.
struct InFlight {
    key: Option<String>,
    kind: RequestKind,
    origin: Origin,
    pool: Option<String>,
    seen_at: i64,
}

struct Exhausted {
    key: Option<String>,
    since: i64,
}

/// One client version refused for drift since the process started. A cleared version keeps its record, so its series
/// reads 0 under the same labels instead of vanishing, until a newer version needs its place.
struct Drift {
    active: bool,
    /// The key of the newest refused request, which the link opens.
    key: Option<String>,
    since: i64,
    trigger: String,
}

/// The bus arm's reading of one event: an exhausted outcome on a pool enters it and a succeeded one clears it, and a
/// drift refusal activates its version until a client inference from that version succeeds.
pub(crate) fn observe(state: &mut ReaderState, event: &Event, now: i64) {
    state
        .in_flight
        .retain(|_, e| now - e.seen_at < IN_FLIGHT_TTL_MS);
    match event {
        Event::RequestObserved(o) => {
            state.in_flight.insert(
                o.request_id.clone(),
                InFlight {
                    key: o.key_id.clone(),
                    kind: o.kind,
                    origin: o.origin,
                    pool: o.pool_id.clone(),
                    seen_at: now,
                },
            );
        }
        Event::ResponseChunk(c) => {
            // The TTL asks whether a request went silent, so a stream still sending is not idle.
            if let Some(e) = state.in_flight.get_mut(&c.request_id) {
                e.seen_at = now;
            }
        }
        Event::RequestComplete(c) => {
            let Some(e) = state.in_flight.remove(&c.request_id) else {
                return;
            };
            if let Some(pool) = &e.pool {
                match c.outcome {
                    Outcome::Exhausted => {
                        state
                            .exhausted
                            .entry(pool.clone())
                            .or_insert_with(|| Exhausted {
                                key: e.key.clone(),
                                since: c.finished_at,
                            });
                    }
                    Outcome::Succeeded => {
                        state.exhausted.remove(pool);
                    }
                    _ => {}
                }
            }
            // A completion that names no version is filed under one name on both paths, so a refusal without a
            // version clears on a relayed client inference without one.
            let version = drift_subject(c.client_version.as_deref());
            if let Some(trigger) = &c.client_drift {
                if !state.drift.contains_key(version) && state.drift.len() >= DRIFT_SUBJECTS {
                    evict_drift(&mut state.drift);
                }
                let drift = state.drift.entry(version.into()).or_insert_with(|| Drift {
                    active: false,
                    key: None,
                    since: c.finished_at,
                    trigger: trigger.clone(),
                });
                if !drift.active {
                    drift.active = true;
                    drift.since = c.finished_at;
                }
                drift.key.clone_from(&e.key);
                drift.trigger.clone_from(trigger);
            } else if c.outcome == Outcome::Succeeded
                && e.origin == Origin::Client
                && e.kind == RequestKind::Inference
            {
                // Only a relayed client inference says the build passes the stamp check again: a count carries no
                // stamped line, a keepalive request replays a body the router kept rather than the client's next one,
                // and a request refused for any other reason says nothing about its version.
                if let Some(drift) = state.drift.get_mut(version) {
                    drift.active = false;
                }
            }
        }
        Event::AccountSelected(_) | Event::ResponseHead(_) => {}
    }
}

/// The drift subject a completion files under: the version when it reads as a release (one to four runs of ASCII
/// digits joined by single dots, at most 32 bytes), else one fixed subject, since the label is client-controlled.
fn drift_subject(version: Option<&str>) -> &str {
    match version {
        None => UNKNOWN_VERSION,
        Some(v)
            if v.len() <= RELEASE_VERSION_MAX_BYTES
                && v.split('.').count() <= 4
                && v.split('.')
                    .all(|run| !run.is_empty() && run.bytes().all(|b| b.is_ascii_digit())) =>
        {
            v
        }
        Some(_) => OTHER_VERSION,
    }
}

/// Makes room for a new version at the cap: the cleared version whose run started first goes, else the holding one
/// that has held longest, so a version refused now always reads.
fn evict_drift(drift: &mut HashMap<String, Drift>) {
    let oldest = drift
        .iter()
        .min_by_key(|&(version, d)| (d.active, d.since, version))
        .map(|(version, _)| version.clone());
    if let Some(version) = oldest {
        drift.remove(&version);
    }
}

/// A condition about a pool or one of the router's own components: a pool's belongs to its card and opens it, a
/// component's belongs to no pool and opens nothing.
fn condition_for(
    kind: Kind,
    subject: Subject,
    holds: bool,
    since: Option<i64>,
    detail: serde_json::Value,
) -> Condition {
    let pool = (subject.kind == SubjectKind::Pool).then(|| subject.id.clone());
    Condition {
        kind,
        subject,
        holds,
        since,
        pools: pool.iter().cloned().collect(),
        link: pool.map(|id| Link { page: "pools", id }),
        detail,
    }
}

/// When a state its source records no time for started: the first read that saw it hold. The entry joins `seen`, so
/// the prune at the end of the read keeps it for as long as the state holds.
fn since_of(
    state: &mut ReaderState,
    seen: &mut HashSet<(Kind, String)>,
    kind: Kind,
    id: &str,
    holds: bool,
    now: i64,
) -> Option<i64> {
    holds.then(|| {
        let entry = (kind, id.to_string());
        seen.insert(entry.clone());
        *state.state_since.entry(entry).or_insert(now)
    })
}

/// Every condition the router can tell about now: a series for every subject the kind applies to, holding or clear,
/// so a cleared condition reads 0 on the scrape rather than vanishing. The metrics listener and the admin route read
/// this one function, so a scrape and the dashboard never disagree. A store read that fails fails the whole read, as
/// the caller's parked read does, since a series answered without its evidence could clear a real alarm. `parked` is
/// `None` where no ledger is built, and `ledger_record_parked` then has no series.
pub(crate) fn conditions(
    state: &mut ReaderState,
    store: &Store,
    router: &RouterHandle,
    ledger_degraded: bool,
    parked: Option<&[ParkedSummary]>,
    usage: Option<&UsageService>,
    now: i64,
) -> Result<Vec<Condition>, AttentionError> {
    let snapshot = router.snapshot();
    let accounts = store.accounts()?;
    let mut out = Vec::new();
    // A deleted pool's exhaustion goes with it, so a pool created later under the same id starts clear.
    state.exhausted.retain(|id, _| snapshot.pool(id).is_some());
    for pool in snapshot.pools() {
        let held = state.exhausted.get(&pool.id);
        out.push(Condition {
            kind: Kind::PoolExhausted,
            subject: Subject::pool(pool),
            holds: held.is_some(),
            since: held.map(|e| e.since),
            pools: vec![pool.id.clone()],
            link: Some(Link {
                page: "pools",
                id: pool.id.clone(),
            }),
            detail: json!({ "key": held.and_then(|e| e.key.clone()), "pool": pool.id }),
        });
    }
    let mut seen = HashSet::new();
    // The two writers are the process's own components and a subject of their own, so the dashboard never reads a
    // stalled disk as an exhausted pool. Neither records when it degraded, so the first read that sees it dates it.
    for (component, degraded) in [
        ("ledger", ledger_degraded),
        ("routing", router.writer_status().degraded),
    ] {
        out.push(condition_for(
            Kind::WriterDegraded,
            Subject::router(component),
            degraded,
            since_of(
                state,
                &mut seen,
                Kind::WriterDegraded,
                component,
                degraded,
                now,
            ),
            json!({ "component": component }),
        ));
    }
    if let Some(parked) = parked {
        // The snapshot's pools, then the pools the parked rows name that the snapshot no longer holds: a pool deleted
        // under a parked record keeps the condition, labelled by its id and opening nothing, until the start lands
        // the row.
        let mut pools: Vec<(String, String, bool)> = snapshot
            .pools()
            .map(|pool| (pool.id.clone(), pool.label.clone(), true))
            .collect();
        for id in parked.iter().filter_map(|p| p.pool_id.as_deref()) {
            if snapshot.pool(id).is_none() && !pools.iter().any(|(known, _, _)| known == id) {
                pools.push((id.to_string(), id.to_string(), false));
            }
        }
        for (pool_id, label, live) in pools {
            let row = parked
                .iter()
                .find(|p| p.pool_id.as_deref() == Some(pool_id.as_str()));
            let mut condition = condition_for(
                Kind::LedgerRecordParked,
                Subject {
                    kind: SubjectKind::Pool,
                    id: pool_id.clone(),
                    label,
                },
                row.is_some(),
                row.map(|r| r.oldest_at),
                json!({
                    "accounts": row.map(|r| r.account_ids.clone()).unwrap_or_default(),
                    "count": row.map_or(0, |r| r.count),
                    "pool": pool_id,
                }),
            );
            if !live {
                condition.link = None;
                condition.pools.clear();
            }
            out.push(condition);
        }
        if let Some(row) = parked.iter().find(|p| p.pool_id.is_none()) {
            out.push(condition_for(
                Kind::LedgerRecordParked,
                Subject::router(NO_POOL),
                true,
                Some(row.oldest_at),
                json!({ "accounts": row.account_ids, "count": row.count, "pool": null }),
            ));
        }
    }
    for (version, drift) in &state.drift {
        let pool = drift
            .key
            .as_deref()
            .and_then(|key| snapshot.key(key))
            .map(|k| k.pool_id.clone());
        out.push(Condition {
            kind: Kind::ClientDrift,
            subject: Subject::client(version),
            holds: drift.active,
            since: drift.active.then_some(drift.since),
            pools: pool.into_iter().collect(),
            link: drift.key.as_ref().map(|key| Link {
                page: "keys",
                id: key.clone(),
            }),
            detail: json!({ "client_version": version, "key": drift.key, "trigger": drift.trigger }),
        });
    }
    for account in &accounts {
        let mut pools: Vec<String> = snapshot
            .pools()
            .filter(|p| {
                snapshot
                    .members(&p.id)
                    .iter()
                    .any(|m| m.account_id == account.id)
            })
            .map(|p| p.id.clone())
            .collect();
        pools.sort();
        let condition =
            |kind: Kind, holds: bool, since: Option<i64>, detail: serde_json::Value| Condition {
                kind,
                subject: Subject::account(account),
                holds,
                since,
                pools: pools.clone(),
                link: Some(Link {
                    page: "accounts",
                    id: account.id.clone(),
                }),
                detail,
            };
        // Every account carries the re-auth series, since the relay marks an Anthropic key the upstream refuses the
        // same way as an OAuth login; only an Anthropic key runs out of funding. The registry records no time for a
        // state change, so the first read that sees the state dates it.
        let mut states = vec![(Kind::ReauthRequired, AccountState::ReauthRequired)];
        if account.kind == AccountKind::ApiKey {
            states.push((Kind::FundingRequired, AccountState::FundingRequired));
        }
        for (kind, held_state) in states {
            let holds = account.state == held_state;
            let since = holds.then(|| {
                let entry = (kind, account.id.clone());
                seen.insert(entry.clone());
                *state.state_since.entry(entry).or_insert(now)
            });
            out.push(condition(
                kind,
                holds,
                since,
                json!({ "account": account.id, "label": account.label }),
            ));
        }
        if account.kind == AccountKind::Oauth {
            // Read from the store rather than kept here, so a run of failures survives a restart.
            let attempts = store.newest_resolved_refresh_attempts(
                &account.id,
                account.incarnation,
                REFRESH_RUN_READ,
            )?;
            // A commit ends the run, and so does a `superseded` settlement, which exists only because a newer bundle
            // committed.
            let run: Vec<&ResolvedAttempt> = attempts
                .iter()
                .take_while(|a| {
                    a.outcome != "committed" && a.detail.as_deref() != Some("superseded")
                })
                .collect();
            // One failure is a token endpoint having a bad moment; two in a row is a credential that is not coming
            // back on its own.
            let holds = run.len() >= 2;
            let newest = run.first();
            out.push(condition(
                Kind::RefreshFailed,
                holds,
                holds.then(|| run.last().map(|a| a.started_at)).flatten(),
                json!({
                    "account": account.id,
                    "failures": run.len(),
                    "last_error": newest.and_then(|a| a.detail.clone()),
                    "outcome": newest.map(|a| a.outcome.clone()),
                }),
            ));
        }
        if polled(account.kind, account.state) {
            // A series from before the account's first poll status exists, as after a restart, reading 0.
            let status = usage.and_then(|u| u.poll_status(&account.id));
            let backoff_since = status.as_ref().and_then(|s| s.backoff_since);
            let holds =
                backoff_since.is_some_and(|since| since <= now - BACKOFF_ATTENTION_AFTER_MS);
            out.push(condition(
                Kind::PollBackoff,
                holds,
                backoff_since.filter(|_| holds),
                json!({
                    "account": account.id,
                    "backoff_since": backoff_since,
                    "next_attempt_at": status.as_ref().and_then(|s| s.next_attempt_at),
                    "rate_limited_in_a_row": status.as_ref().map(|s| s.rate_limited_in_a_row),
                }),
            ));
        }
    }
    // A cleared state, a deleted account and an account that changed kind all lose their first-seen time, so the
    // next holding read dates the state afresh.
    state.state_since.retain(|entry, _| seen.contains(entry));
    Ok(out)
}
