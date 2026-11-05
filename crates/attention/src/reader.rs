//! The nineteen conditions, read fresh on every call: the account states and the pools from the registry, the refresh
//! runs from the store, the parked records, the ledger's degraded state, the unpriced models its landing saw, the
//! routing conditions its hourly fold wrote and the expiries between turns its rollups count from the ledger, the
//! routing state writer's and the listeners' pairs from the router, the bus gaps and the caps from the keepalive
//! engine, the poll's standing from the usage service, and the exhaustions and drift refusals from the request
//! outcomes the bus arm keeps.

use std::collections::{HashMap, HashSet};

use phoebis_bus::{Event, Kind as RequestKind, Origin, Outcome};
use phoebis_ledger::{
    polled, ParkedSummary, RoutingCondition, RoutingKind, UnpricedModel, CEILING_SETTING,
    IN_FLIGHT_TTL_MS, SHED_CAP_SETTING, SLOW_DOWN_STRETCH_SETTING, SLOW_SETTINGS, WALL_SETTINGS,
    WINDOW_CEILING_SETTING,
};
use phoebis_oauth::UsageService;
use phoebis_routing::{shed_cap, RegistrySnapshot, RouterHandle, DAY_MS};
use phoebis_store::{
    AccountKind, AccountRow, AccountState, PoolRow, ResolvedAttempt, Settled, Store, StoreError,
};
use serde_json::json;

use crate::{AttentionError, KeepaliveStanding, PairFacts};

/// How long after a run's first 429 the poll backoff condition holds, so a single 429 or a short run never reads 1.
pub const BACKOFF_ATTENTION_AFTER_MS: i64 = 30 * 60 * 1_000;

/// A loaded leaf holds `certificate_expiring` from this many days before its `not_after`. The router's
/// `EXPIRY_WARNING_DAYS`, which `check-config` and `status` warn under, reads this constant.
pub const CERTIFICATE_WARNING_DAYS: i64 = 30;

/// How many of a holder's newest settled refresh attempts one read walks: two make the condition, and the rest date
/// the run's start as far back as the read reaches, which bounds the scan a scrape pays per account.
pub const REFRESH_RUN_READ: usize = 16;

/// The subject of a drift refusal whose completion names no client version.
const UNKNOWN_VERSION: &str = "unknown";

/// The subject of the parked records served under no pool; no pool id can read `-`.
const NO_POOL: &str = "-";

/// The keepalive engine's subject, and the stem of its caps' subjects.
const KEEPALIVE: &str = "keepalive";

/// The subject of a drift refusal whose version does not read as a release, so a malformed or oversized label shares
/// one series.
const OTHER_VERSION: &str = "other";

/// The most client versions the drift record holds. The version is the client's own user agent, so without a cap a
/// caller sending refused stamps under ever new versions would grow the record and the scrape's series without bound.
const DRIFT_SUBJECTS: usize = 16;

const RELEASE_VERSION_MAX_BYTES: usize = 32;

/// Declared worst first, so the derived order is the ranking: a pool that serves nothing, a client build refused in
/// every pool, a writer that cannot persist what the router decides (a degraded ledger also holds budgeted members
/// back), a listener serving a pair whose replacement did not load (the next start would refuse the files as they
/// stand), an account that serves nothing until the operator acts, an account's credential about to lapse, the
/// router's own certificate about to lapse, priced records the ledger file keeps until the next start or a replay
/// lands them, a keepalive engine that missed bus events in the last day, windows that read stale, a model that served
/// plan-funded traffic and prices nothing, while nothing is broken, and an account whose meter charged its windows off
/// the cost model's figures. That last one names a pricing that is off and ranks above the four routing conditions,
/// whose causes it can explain. Those four stop no traffic and rank under every kind that does, worst first among
/// themselves: an account that reached its wall, a move whose destination was spent within the hour, a pool whose
/// rounds found no destination with runway, and an account slowed past twice its pool's stretch threshold in the day,
/// a slow-down that worked for long. The keepalive engine's two warnings rank last: a cap filled to 90 percent or
/// more, then cache entries that expired between turns while a pool warms caches.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    PoolExhausted,
    ClientDrift,
    WriterDegraded,
    CertificateReloadFailed,
    ReauthRequired,
    FundingRequired,
    RefreshFailed,
    CertificateExpiring,
    LedgerRecordParked,
    KeepaliveBusGaps,
    PollBackoff,
    ModelUnpriced,
    MeterDrift,
    WallReached,
    DestinationSpent,
    ShedNoRunway,
    SlowedLong,
    KeepaliveCap,
    ExpiredBetweenTurns,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::PoolExhausted => "pool_exhausted",
            Kind::ClientDrift => "client_drift",
            Kind::WriterDegraded => "writer_degraded",
            Kind::CertificateReloadFailed => "certificate_reload_failed",
            Kind::ReauthRequired => "reauth_required",
            Kind::FundingRequired => "funding_required",
            Kind::RefreshFailed => "refresh_failed",
            Kind::CertificateExpiring => "certificate_expiring",
            Kind::LedgerRecordParked => "ledger_record_parked",
            Kind::KeepaliveBusGaps => "keepalive_bus_gaps",
            Kind::PollBackoff => "poll_backoff",
            Kind::ModelUnpriced => "model_unpriced",
            Kind::MeterDrift => RoutingKind::MeterDrift.as_str(),
            Kind::WallReached => RoutingKind::WallReached.as_str(),
            Kind::DestinationSpent => RoutingKind::DestinationSpent.as_str(),
            Kind::ShedNoRunway => RoutingKind::ShedNoRunway.as_str(),
            Kind::SlowedLong => RoutingKind::SlowedLong.as_str(),
            Kind::KeepaliveCap => "keepalive_cap",
            Kind::ExpiredBetweenTurns => "expired_between_turns",
        }
    }

    /// The ledger's kind for a condition its hourly fold writes, read off the word, so `as_str` alone pairs the two.
    /// `None` for a kind the reader derives itself.
    fn routing(self) -> Option<RoutingKind> {
        RoutingKind::parse(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubjectKind {
    Account,
    Client,
    Pool,
    /// One of the process's own components: the ledger or the routing state writer, a listener's pair by its key
    /// (`proxy.tls`, `admin.tls`), the keepalive engine (`keepalive`, and `keepalive.<cap>` for one of its caps), or
    /// `-` for a parked record served under no pool.
    Router,
    /// A model by its undated id.
    Model,
}

impl SubjectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SubjectKind::Account => "account",
            SubjectKind::Client => "client",
            SubjectKind::Pool => "pool",
            SubjectKind::Router => "router",
            SubjectKind::Model => "model",
        }
    }
}

/// What a condition is about: the account's or the pool's id and label, the client version as both, the router's own
/// component as both, except the keepalive engine's subjects, which share the label `Keepalive`, or a model's undated
/// id as both.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subject {
    pub kind: SubjectKind,
    pub id: String,
    pub label: String,
}

impl Subject {
    fn new(kind: SubjectKind, id: String, label: String) -> Subject {
        Subject { kind, id, label }
    }

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

    fn keepalive(id: &str) -> Subject {
        Subject {
            kind: SubjectKind::Router,
            id: id.into(),
            label: "Keepalive".into(),
        }
    }

    fn model(model: &str) -> Subject {
        Subject {
            kind: SubjectKind::Model,
            id: model.into(),
            label: model.into(),
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
    /// which leave nothing to open, and for an unpriced model, whose cost reference dialog opens from a link on a
    /// tab and has no address of its own.
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
    /// When the in-flight map was last swept, so a sweep runs once per TTL rather than on every chunk a stream
    /// publishes. Between sweeps each lookup treats an expired entry as absent.
    swept_at: i64,
}

/// A request's key, pool, origin and kind, which its completion does not carry, held from its observation until then.
struct InFlight {
    key: Option<String>,
    kind: RequestKind,
    origin: Origin,
    pool: Option<String>,
    seen_at: i64,
}

impl InFlight {
    fn fresh(&self, now: i64) -> bool {
        now - self.seen_at < IN_FLIGHT_TTL_MS
    }
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
    if now - state.swept_at >= IN_FLIGHT_TTL_MS {
        state.in_flight.retain(|_, e| e.fresh(now));
        state.swept_at = now;
    }
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
            if let Some(e) = state
                .in_flight
                .get_mut(&c.request_id)
                .filter(|e| e.fresh(now))
            {
                e.seen_at = now;
            }
        }
        Event::RequestComplete(c) => {
            let Some(e) = state
                .in_flight
                .remove(&c.request_id)
                .filter(|e| e.fresh(now))
            else {
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

/// A condition about a pool, one of the router's own components or a model. `card` names the pool whose card the
/// condition joins and opens: a live pool's own, and `None` for a pool the registry no longer holds and for every
/// other subject.
fn condition_for(
    kind: Kind,
    subject: Subject,
    card: Option<&str>,
    holds: bool,
    since: Option<i64>,
    detail: serde_json::Value,
) -> Condition {
    Condition {
        kind,
        subject,
        holds,
        since,
        pools: card.iter().map(|id| id.to_string()).collect(),
        link: card.map(|id| Link {
            page: "pools",
            id: id.to_string(),
        }),
        detail,
    }
}

/// A condition about an account, which joins its pools' cards and opens the account while the registry holds it
/// (`live`): one whose account the registry no longer holds belongs to no card and opens nothing.
fn account_condition(
    kind: Kind,
    subject: Subject,
    pools: &[String],
    live: bool,
    holds: bool,
    since: Option<i64>,
    detail: serde_json::Value,
) -> Condition {
    let link = live.then(|| Link {
        page: "accounts",
        id: subject.id.clone(),
    });
    Condition {
        kind,
        subject,
        holds,
        since,
        pools: if live { pools.to_vec() } else { Vec::new() },
        link,
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

/// Each live subject as `(id, label, true)`, then each id the rows name that none of them has, labeled by its id and
/// marked gone, so a deleted subject keeps its condition until its row goes.
fn with_orphans<'a>(
    live: impl IntoIterator<Item = (String, String)>,
    named: impl IntoIterator<Item = &'a str>,
) -> Vec<(String, String, bool)> {
    let mut subjects: Vec<(String, String, bool)> = live
        .into_iter()
        .map(|(id, label)| (id, label, true))
        .collect();
    for id in named {
        if !subjects.iter().any(|(known, _, _)| known == id) {
            subjects.push((id.to_string(), id.to_string(), false));
        }
    }
    subjects
}

/// Each account's pools, from one walk of the snapshot's memberships. An account no pool holds has no entry.
fn account_pools(snapshot: &RegistrySnapshot) -> HashMap<String, Vec<String>> {
    let mut pools: HashMap<String, Vec<String>> = HashMap::new();
    for pool in snapshot.pools() {
        for member in snapshot.members(&pool.id).iter() {
            pools
                .entry(member.account_id.clone())
                .or_default()
                .push(pool.id.clone());
        }
    }
    pools
}

/// Each OAuth account's newest settled refresh attempts, read from the store rather than kept here, so a run of
/// failures survives a restart.
pub(crate) fn refresh_runs(
    store: &Store,
    accounts: &[AccountRow],
) -> Result<HashMap<String, Vec<ResolvedAttempt>>, StoreError> {
    accounts
        .iter()
        .filter(|account| account.kind == AccountKind::Oauth)
        .map(|account| {
            let attempts = store.newest_resolved_refresh_attempts(
                &account.id,
                account.incarnation,
                REFRESH_RUN_READ,
            )?;
            Ok((account.id.clone(), attempts))
        })
        .collect()
}

/// Every condition the router can tell about now: a series for every subject the kind applies to, holding or clear, so
/// a cleared condition reads 0 on the scrape rather than vanishing. The metrics listener and the admin route read this
/// one function, so a scrape and the dashboard never disagree. The caller reads the registry's `accounts` and their
/// `refresh_runs`, and the parked rows, before it takes the reader's state, and a read that fails fails the whole
/// answer, since a series answered without its evidence could clear a real alarm. `parked` and `routing` are `None`
/// where no ledger is built, and neither `ledger_record_parked`, `meter_drift` nor the four routing conditions
/// (`wall_reached`, `destination_spent`, `shed_no_runway` and `slowed_long`) then have a series. Where one is built,
/// `destination_spent` stops holding once a `window_cleared` followed its spent window. `pairs` is `None`
/// where no listener is built, and neither certificate kind then has a series. `unpriced` is the ledger's map, empty
/// where no ledger is built. `keepalive` is `None` where no engine is built, and none of `keepalive_bus_gaps`,
/// `keepalive_cap` and `expired_between_turns` then has a series. `expired` is the ledger's count of expiries between
/// turns over the last 24 hours, `None` where no ledger is built, and `expired_between_turns` then has no series.
#[allow(clippy::too_many_arguments)]
pub(crate) fn conditions(
    state: &mut ReaderState,
    accounts: &[AccountRow],
    refresh_runs: &HashMap<String, Vec<ResolvedAttempt>>,
    router: &RouterHandle,
    ledger_degraded: bool,
    parked: Option<&[ParkedSummary]>,
    unpriced: &[UnpricedModel],
    routing: Option<&[RoutingCondition]>,
    pairs: Option<&[PairFacts]>,
    usage: Option<&UsageService>,
    keepalive: Option<&KeepaliveStanding>,
    expired: Option<u64>,
    now: i64,
) -> Result<Vec<Condition>, AttentionError> {
    let snapshot = router.snapshot();
    let pools_of = account_pools(&snapshot);
    let mut out = Vec::new();
    // A deleted pool's exhaustion goes with it, so a pool created later under the same id starts clear.
    state.exhausted.retain(|id, _| snapshot.pool(id).is_some());
    for pool in snapshot.pools() {
        let held = state.exhausted.get(&pool.id);
        out.push(condition_for(
            Kind::PoolExhausted,
            Subject::pool(pool),
            Some(&pool.id),
            held.is_some(),
            held.map(|e| e.since),
            json!({ "key": held.and_then(|e| e.key.clone()), "pool": pool.id }),
        ));
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
            None,
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
    // The pairs the router serves rather than the files on disk: a pair replaced and not yet reloaded changes neither
    // condition, and the operator who reads the files has `status` and `check-config`.
    for pair in pairs.unwrap_or_default() {
        let cert_file = pair.cert_file.display().to_string();
        out.push(condition_for(
            Kind::CertificateReloadFailed,
            Subject::router(&pair.key),
            None,
            pair.failed.is_some(),
            pair.failed.as_ref().map(|(at, _)| *at),
            json!({
                "cert_file": cert_file,
                "error": pair.failed.as_ref().map(|(_, line)| line),
                "key_file": pair.key_file.display().to_string(),
                "serving_since": pair.loaded_at,
            }),
        ));
        // Judged on this read's clock rather than at the load, so a pair loaded with time to spare raises the
        // condition when its day comes without a reload. Dated from `not_after`, so a restart keeps the date, and held
        // from that instant on rather than by the whole-day count, so a dated item never starts in the future.
        let warning_from = pair.not_after_ms - CERTIFICATE_WARNING_DAYS * DAY_MS;
        let days_left = (pair.not_after_ms - now).div_euclid(DAY_MS);
        let expiring = now >= warning_from;
        out.push(condition_for(
            Kind::CertificateExpiring,
            Subject::router(&pair.key),
            None,
            expiring,
            expiring.then_some(warning_from),
            json!({ "cert_file": cert_file, "days_left": days_left, "expires": pair.not_after_ms }),
        ));
    }
    if let Some(parked) = parked {
        // The snapshot's pools, then the pools the parked rows name that the snapshot no longer holds: a pool deleted
        // under a parked record keeps the condition, labeled by its id and opening nothing, until the next start or
        // a replay lands the row.
        let pools = with_orphans(
            snapshot
                .pools()
                .map(|pool| (pool.id.clone(), pool.label.clone())),
            parked.iter().filter_map(|p| p.pool_id.as_deref()),
        );
        for (pool_id, label, live) in pools {
            let row = parked
                .iter()
                .find(|p| p.pool_id.as_deref() == Some(pool_id.as_str()));
            out.push(condition_for(
                Kind::LedgerRecordParked,
                Subject::new(SubjectKind::Pool, pool_id.clone(), label),
                live.then_some(pool_id.as_str()),
                row.is_some(),
                row.map(|r| r.oldest_at),
                json!({
                    "accounts": row.map(|r| r.account_ids.clone()).unwrap_or_default(),
                    "count": row.map_or(0, |r| r.count),
                    "pool": pool_id,
                }),
            ));
        }
        if let Some(row) = parked.iter().find(|p| p.pool_id.is_none()) {
            out.push(condition_for(
                Kind::LedgerRecordParked,
                Subject::router(NO_POOL),
                None,
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
    for account in accounts {
        let pools = pools_of
            .get(&account.id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let condition = |kind: Kind, holds: bool, since: Option<i64>, detail: serde_json::Value| {
            account_condition(
                kind,
                Subject::account(account),
                pools,
                true,
                holds,
                since,
                detail,
            )
        };
        // Every account carries the re-auth series, since the relay marks an API key the upstream refuses the
        // same way as an OAuth login; only an API key runs out of funding. The registry records no time for a
        // state change, so the first read that sees the state dates it.
        let mut states = vec![(Kind::ReauthRequired, AccountState::ReauthRequired)];
        if account.kind == AccountKind::ApiKey {
            states.push((Kind::FundingRequired, AccountState::FundingRequired));
        }
        for (kind, held_state) in states {
            let holds = account.state == held_state;
            let since = since_of(state, &mut seen, kind, &account.id, holds, now);
            out.push(condition(
                kind,
                holds,
                since,
                json!({ "account": account.id, "label": account.label }),
            ));
        }
        if account.kind == AccountKind::Oauth {
            let attempts = refresh_runs
                .get(&account.id)
                .map(Vec::as_slice)
                .unwrap_or_default();
            // A commit ends the run, and so does a `superseded` settlement, which exists only because a newer bundle
            // committed.
            let run: Vec<&ResolvedAttempt> = attempts
                .iter()
                .take_while(|a| {
                    !matches!(
                        a.settled(),
                        Settled::Committed | Settled::Paired | Settled::Superseded
                    )
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
    // A model belongs to no pool card. A model a revision weighted keeps its entry, so its series reads 0.
    for entry in unpriced {
        out.push(condition_for(
            Kind::ModelUnpriced,
            Subject::model(&entry.model),
            None,
            entry.since.is_some(),
            entry.since,
            json!({
                "model": entry.model,
                "requests": entry.requests,
            }),
        ));
    }
    if let Some(rows) = routing {
        out.extend(routing_conditions(&snapshot, &pools_of, accounts, rows));
    }
    if let Some(standing) = keepalive {
        // `bus_gaps` counts since the start, so the item reads the last gap's instant: on the counter alone it would
        // stand until a restart.
        let gapped = standing.last_bus_gap_at.is_some_and(|at| now - at < DAY_MS);
        out.push(condition_for(
            Kind::KeepaliveBusGaps,
            Subject::keepalive(KEEPALIVE),
            None,
            gapped,
            since_of(
                state,
                &mut seen,
                Kind::KeepaliveBusGaps,
                KEEPALIVE,
                gapped,
                now,
            ),
            json!({ "gaps": standing.bus_gaps, "last_at": standing.last_bus_gap_at }),
        ));
        for cap in &standing.caps {
            let id = format!("{KEEPALIVE}.{}", cap.name);
            let full = cap.max > 0 && cap.count * 100 >= cap.max * 90;
            // NaN has no JSON number, so a max of 0 reads 0.
            let pct = if cap.max == 0 {
                0.0
            } else {
                cap.count as f64 / cap.max as f64
            };
            out.push(condition_for(
                Kind::KeepaliveCap,
                Subject::keepalive(&id),
                None,
                full,
                since_of(state, &mut seen, Kind::KeepaliveCap, &id, full, now),
                json!({ "cap": cap.name, "count": cap.count, "max": cap.max, "pct": pct }),
            ));
        }
        if let Some(expired) = expired {
            // Without a pool that warms caches, an entry expiring between turns is the expected course.
            let holds = expired > 0 && standing.warming;
            out.push(condition_for(
                Kind::ExpiredBetweenTurns,
                Subject::keepalive(KEEPALIVE),
                None,
                holds,
                since_of(
                    state,
                    &mut seen,
                    Kind::ExpiredBetweenTurns,
                    KEEPALIVE,
                    holds,
                    now,
                ),
                json!({ "expired": expired }),
            ));
        }
    }
    // A cleared state, a deleted account and an account that changed kind all lose their first-seen time, so the
    // next holding read dates the state afresh.
    state.state_since.retain(|entry, _| seen.contains(entry));
    Ok(out)
}

/// The five conditions the ledger's hourly fold wrote: one per pool for `shed_no_runway` and one per OAuth account for
/// `meter_drift`, `wall_reached`, `destination_spent` and `slowed_long`, each holding where a row names its subject,
/// with the row's `since` and its detail, so `destination_spent` ends where the fold read a `window_cleared` after the
/// spent window. A row whose pool the snapshot holds gains `setting_values`, that pool's value
/// of each setting the row's `setting` names, since the ledger reads no registry. A subject the registry no longer
/// holds keeps its row's condition, labeled by its id and opening nothing, until the fold no longer writes the row.
fn routing_conditions(
    snapshot: &RegistrySnapshot,
    pools_of: &HashMap<String, Vec<String>>,
    accounts: &[AccountRow],
    rows: &[RoutingCondition],
) -> Vec<Condition> {
    // The first row per kind and subject answers, as a scan in order finds it.
    let mut by_subject: HashMap<(RoutingKind, &str), &RoutingCondition> = HashMap::new();
    for r in rows {
        by_subject
            .entry((r.kind, r.subject_id.as_str()))
            .or_insert(r);
    }
    let row = |kind: RoutingKind, id: &str| by_subject.get(&(kind, id)).copied();
    let detail_of = |row: Option<&RoutingCondition>, cleared: serde_json::Value| {
        let Some(row) = row else {
            return cleared;
        };
        let mut detail = row.detail.clone();
        if let Some(pool) = row.pool_id.as_deref().and_then(|id| snapshot.pool(id)) {
            let values: serde_json::Map<String, serde_json::Value> = detail["setting"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|name| {
                    let name = name.as_str()?;
                    let value = match name {
                        SHED_CAP_SETTING => json!(shed_cap(pool)),
                        SLOW_DOWN_STRETCH_SETTING => {
                            json!(pool.slow_down.slow_down_stretch_seconds)
                        }
                        WINDOW_CEILING_SETTING => json!(pool.window_ceiling),
                        _ => return None,
                    };
                    Some((name.to_string(), value))
                })
                .collect();
            detail["setting_values"] = serde_json::Value::Object(values);
        }
        detail
    };
    let mut out = Vec::new();
    let pools = with_orphans(
        snapshot
            .pools()
            .map(|pool| (pool.id.clone(), pool.label.clone())),
        rows.iter()
            .filter(|r| r.kind == RoutingKind::ShedNoRunway)
            .map(|r| r.subject_id.as_str()),
    );
    for (pool_id, label, live) in pools {
        let held = row(RoutingKind::ShedNoRunway, &pool_id);
        out.push(condition_for(
            Kind::ShedNoRunway,
            Subject::new(SubjectKind::Pool, pool_id.clone(), label),
            live.then_some(pool_id.as_str()),
            held.is_some(),
            held.map(|r| r.since),
            detail_of(
                held,
                json!({ "accounts": [], "rounds": 0, "setting": CEILING_SETTING }),
            ),
        ));
    }
    let account_kinds = [
        (
            Kind::MeterDrift,
            json!({ "effective": null, "source": null, "windows": [] }),
        ),
        (
            Kind::WallReached,
            json!({
                "early_ms": null,
                "projection_error": null,
                "setting": WALL_SETTINGS,
                "walls": 0,
                "windows": [],
            }),
        ),
        (
            Kind::DestinationSpent,
            json!({ "fits": {}, "moves": 0, "setting": CEILING_SETTING, "windows": [] }),
        ),
        (
            Kind::SlowedLong,
            json!({
                "bar_ms": null,
                "factor_min": null,
                "open": false,
                "setting": SLOW_SETTINGS,
                "slowed_ms": 0,
                "spans": 0,
            }),
        ),
    ]
    .map(|(kind, cleared)| (kind, kind.routing(), cleared));
    let subjects = with_orphans(
        accounts
            .iter()
            .filter(|a| a.kind == AccountKind::Oauth)
            .map(|a| (a.id.clone(), a.label.clone())),
        rows.iter()
            .filter(|r| {
                account_kinds
                    .iter()
                    .any(|(_, routing, _)| *routing == Some(r.kind))
            })
            .map(|r| r.subject_id.as_str()),
    );
    for (account_id, label, live) in subjects {
        let pools = pools_of
            .get(&account_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for (kind, routing, cleared) in &account_kinds {
            let held = routing.and_then(|routing| row(routing, &account_id));
            out.push(account_condition(
                *kind,
                Subject::new(SubjectKind::Account, account_id.clone(), label.clone()),
                pools,
                live,
                held.is_some(),
                held.map(|r| r.since),
                detail_of(held, cleared.clone()),
            ));
        }
    }
    out
}
