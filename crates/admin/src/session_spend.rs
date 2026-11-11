//! The sessions route's figures over each session's life under its key: its spend apart from its probes', its agents'
//! entries, its newest client request's user agent and the hours of its last day, folded behind the ledger's landing
//! watermark, so a poll reads the rows landed since the last one and fills a session's whole history once.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use phoebis_ledger::{
    add_figures, hour_of, Connection, Funding, LedgerError, PlanTables, PriceRevisions,
    RequestPrice, SessionSpendKey, SessionSpendRow, PROBE_KIND,
};
use phoebis_routing::{DAY_MS, HOUR_MS};
use utoipa::ToSchema;

/// The session's spend over its client requests, and the same figures over its probes, apart.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionSpend {
    /// The client requests priced, probes and refused requests out.
    pub requests: u64,
    /// Their dollars at the list price, the sum of what the request log shows as `cost_usd`, where a client
    /// classifier call the plan paid adds zero.
    pub usd: f64,
    /// The dollars billed by funding, `api_key`, `fast`, `overage` or `usage_based`, a zero entry left out.
    pub billed_usd: BTreeMap<String, f64>,
    /// The requests' Pro-week figures summed, the share of the Pro 7-day window each serving attempt's usage spends
    /// whatever account tier served it, null where a request priced nothing and 0 with no plan-funded request.
    pub equivalent: Option<f64>,
    /// The shares of each account's own windows under the tier the serving attempt recorded, per account, keyed
    /// `five_hour`, `seven_day`, then `seven_day_<family>`, since a share names one account's plan. A shared window the
    /// account's set lacks today is left out, and the account's key stays, so a key with no window under it still marks
    /// that a plan paid.
    pub shares: BTreeMap<String, BTreeMap<String, f64>>,
    /// The same figures over the router's cache probes of the session.
    pub probes: SessionSpendPart,
}

/// `SessionSpend` without its probes.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionSpendPart {
    pub requests: u64,
    pub usd: f64,
    pub billed_usd: BTreeMap<String, f64>,
    pub equivalent: Option<f64>,
    pub shares: BTreeMap<String, BTreeMap<String, f64>>,
}

/// The session's client requests of the last 24 hours by class, and their spend.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionDay {
    /// By `request_class` as the ledger holds it, `other` for a request with none.
    pub requests: BTreeMap<String, u64>,
    pub usd: f64,
    pub billed_usd: BTreeMap<String, f64>,
    pub equivalent: Option<f64>,
}

impl Default for SessionDay {
    /// The day the fold answers for a session with no request in it, so a row the fold holds nothing for reads the
    /// same: no plan request priced, which reads the Pro-week figure 0.
    fn default() -> SessionDay {
        Money::default().day(BTreeMap::new())
    }
}

/// The session's requests per agent and model, from the ledger.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentRequests {
    pub agent_id: String,
    /// The canonical model.
    pub model: String,
    /// The newest request's agent type.
    pub agent_type: Option<String>,
    /// The newest client request's serving account.
    pub account_id: Option<String>,
    /// Client requests.
    pub requests: u64,
    /// The newest client request.
    pub last_request_at: Option<i64>,
    pub spend: SessionSpend,
}

/// The sessions route's running totals behind the ledger's landed-at watermark: per session its spend, its probes'
/// spend, its agents' entries, its newest client request's user agent and 25 hourly buckets of its last day, so a
/// 5-second poll reads the rows written since its last read and nothing older. Public with its fields private, since
/// the router and the admin harness build `AdminState` with one.
#[derive(Default)]
pub struct SessionSpendFold {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// The newest `landed_at` folded, `None` before the first poll and after a revision reset: the fold then starts
    /// at the snapshot's newest landing, which `read_poll` reads, never at 0, since a watermark at 0 would walk the
    /// ledger's whole history on every restart and hold an accumulator for every session it ever saw.
    watermark: Option<i64>,
    /// The `effective` date of the cost revision whose anchor priced the sums, `None` before the first poll. A poll
    /// under another revision starts the fold over, since the Pro-week figures would mix anchors.
    priced_under: Option<String>,
    sessions: BTreeMap<SessionSpendKey, Accumulator>,
}

/// One session's figures over its life under its key.
struct Accumulator {
    spend: Sums,
    probes: Sums,
    /// Keyed by agent id and canonical model.
    agents: BTreeMap<(String, String), AgentSums>,
    /// The newest client request's receipt and its own user agent, null included, so an older string never stands
    /// in for a newest request that sent none.
    user_agent: Option<(i64, Option<String>)>,
    /// Keyed by `hour_of(received_at)`, client requests alone.
    hours: BTreeMap<i64, HourSums>,
    /// The newest poll that filled or listed the session, which `retain` measures the expiry from.
    listed_at: i64,
}

/// A set of requests' count, their money and their shares per account.
#[derive(Default)]
struct Sums {
    requests: u64,
    money: Money,
    /// The serving account is keyed on every plan-funded request before its priced windows add to it, so the key
    /// stands with no window where the price gives none, and a session whose map names no account is one no
    /// subscription window paid.
    shares: BTreeMap<String, BTreeMap<&'static str, f64>>,
}

/// The dollars and the Pro-week figure over a set of requests.
struct Money {
    usd: f64,
    billed_usd: BTreeMap<&'static str, f64>,
    /// `None` once a plan request the cost model priced nothing for leaves the summed figure unknown.
    equivalent: Option<f64>,
}

impl Default for Money {
    fn default() -> Money {
        Money {
            usd: 0.0,
            billed_usd: BTreeMap::new(),
            equivalent: Some(0.0),
        }
    }
}

#[derive(Default)]
struct AgentSums {
    spend: Sums,
    probes: Sums,
    newest: Option<NewestRequest>,
}

/// An agent's newest client request: its receipt, its agent type and its serving account.
struct NewestRequest {
    at: i64,
    agent_type: Option<String>,
    account_id: Option<String>,
}

#[derive(Default)]
struct HourSums {
    classes: BTreeMap<String, u64>,
    money: Money,
}

/// The class word of a request that named none.
const UNCLASSED: &str = "other";

/// The oldest hour a bucket is kept for: one before the day `answer` reads.
fn kept_from(now: i64) -> i64 {
    hour_of(now - DAY_MS - HOUR_MS)
}

fn owned(map: &BTreeMap<&'static str, f64>) -> BTreeMap<String, f64> {
    map.iter()
        .map(|(key, value)| (key.to_string(), *value))
        .collect()
}

impl Money {
    fn add(&mut self, price: &RequestPrice) {
        self.usd += price.usd;
        if price.funding == Funding::Plan {
            self.equivalent = add_figures(self.equivalent, price.equivalent);
        } else if price.usd > 0.0 {
            // `as_str`, since the rollup word files an API key account's dollars under `billed`.
            *self.billed_usd.entry(price.funding.as_str()).or_default() += price.usd;
        }
    }

    fn merge(&mut self, other: &Money) {
        self.usd += other.usd;
        for (&funding, usd) in &other.billed_usd {
            *self.billed_usd.entry(funding).or_default() += usd;
        }
        self.equivalent = add_figures(self.equivalent, other.equivalent);
    }

    fn equivalent(&self) -> Option<f64> {
        self.equivalent
    }

    fn day(&self, requests: BTreeMap<String, u64>) -> SessionDay {
        SessionDay {
            requests,
            usd: self.usd,
            billed_usd: owned(&self.billed_usd),
            equivalent: self.equivalent(),
        }
    }
}

impl Sums {
    /// One request, `served` its serving account and price where its serving attempt priced anything.
    fn add(&mut self, served: Option<(&str, &RequestPrice)>) {
        self.requests += 1;
        let Some((account_id, price)) = served else {
            return;
        };
        self.money.add(price);
        if price.funding != Funding::Plan {
            return;
        }
        let windows = self.shares.entry(account_id.to_string()).or_default();
        for (window, share) in price.shares.iter().flatten() {
            if let Some(share) = share {
                *windows.entry(window).or_default() += share;
            }
        }
    }

    fn part(&self) -> SessionSpendPart {
        SessionSpendPart {
            requests: self.requests,
            usd: self.money.usd,
            billed_usd: owned(&self.money.billed_usd),
            equivalent: self.money.equivalent(),
            shares: self
                .shares
                .iter()
                .map(|(account_id, windows)| (account_id.clone(), owned(windows)))
                .collect(),
        }
    }
}

fn spend_of(spend: &Sums, probes: &Sums) -> SessionSpend {
    let SessionSpendPart {
        requests,
        usd,
        billed_usd,
        equivalent,
        shares,
    } = spend.part();
    SessionSpend {
        requests,
        usd,
        billed_usd,
        equivalent,
        shares,
        probes: probes.part(),
    }
}

impl Accumulator {
    fn new(now: i64) -> Accumulator {
        Accumulator {
            spend: Sums::default(),
            probes: Sums::default(),
            agents: BTreeMap::new(),
            user_agent: None,
            hours: BTreeMap::new(),
            listed_at: now,
        }
    }

    /// One request, priced by the serving attempt the request log takes.
    fn fold(
        &mut self,
        row: SessionSpendRow,
        prices: &PriceRevisions,
        tables: &PlanTables,
        now: i64,
    ) {
        let attempt = row.attempt.as_ref();
        let price = attempt.and_then(|a| a.price(prices, tables, now));
        let served = attempt
            .zip(price.as_ref())
            .map(|(a, price)| (a.account_id.as_str(), price));
        let agent = row.agent_id.map(|agent_id| {
            self.agents
                .entry((agent_id, row.canonical_model))
                .or_default()
        });
        if row.kind == PROBE_KIND {
            self.probes.add(served);
            // The agent's probes alone: a probe is no request of the agent's, so its count and its newest request
            // stay the client's.
            if let Some(agent) = agent {
                agent.probes.add(served);
            }
            return;
        }
        self.spend.add(served);
        if self
            .user_agent
            .as_ref()
            .is_none_or(|(at, _)| *at <= row.received_at)
        {
            self.user_agent = Some((row.received_at, row.user_agent));
        }
        if let Some(agent) = agent {
            agent.spend.add(served);
            if agent
                .newest
                .as_ref()
                .is_none_or(|newest| newest.at <= row.received_at)
            {
                agent.newest = Some(NewestRequest {
                    at: row.received_at,
                    agent_type: row.agent_type,
                    account_id: attempt.map(|a| a.account_id.clone()),
                });
            }
        }
        let hour = hour_of(row.received_at);
        if hour >= kept_from(now) {
            let hour = self.hours.entry(hour).or_default();
            let class = row.request_class.unwrap_or_else(|| UNCLASSED.to_string());
            *hour.classes.entry(class).or_default() += 1;
            if let Some((_, price)) = served {
                hour.money.add(price);
            }
        }
    }

    fn figures(&self, now: i64) -> SessionFigures {
        let mut classes: BTreeMap<String, u64> = BTreeMap::new();
        let mut money = Money::default();
        for hour in self
            .hours
            .range(hour_of(now - DAY_MS)..)
            .map(|(_, hour)| hour)
        {
            for (class, requests) in &hour.classes {
                *classes.entry(class.clone()).or_default() += requests;
            }
            money.merge(&hour.money);
        }
        SessionFigures {
            user_agent: self.user_agent.as_ref().and_then(|(_, ua)| ua.clone()),
            spend: (self.spend.requests > 0 || self.probes.requests > 0)
                .then(|| spend_of(&self.spend, &self.probes)),
            day: money.day(classes),
            agent_requests: self
                .agents
                .iter()
                .map(|((agent_id, model), sums)| {
                    let newest = sums.newest.as_ref();
                    AgentRequests {
                        agent_id: agent_id.clone(),
                        model: model.clone(),
                        agent_type: newest.and_then(|n| n.agent_type.clone()),
                        account_id: newest.and_then(|n| n.account_id.clone()),
                        requests: sums.spend.requests,
                        last_request_at: newest.map(|n| n.at),
                        spend: spend_of(&sums.spend, &sums.probes),
                    }
                })
                .collect(),
            requests: self.spend.requests,
        }
    }
}

/// What the fold copies out before the ledger read: the watermark and the sessions it holds, so the read's statements
/// bind them and the lock is never held across the read.
pub(crate) struct FoldCursor {
    /// `None` where the fold starts: `read_poll` takes the snapshot's newest `landed_at` then.
    pub watermark: Option<i64>,
    pub held: BTreeSet<SessionSpendKey>,
}

/// One poll's rows as the sessions closure read them under its transaction, the watermark the incremental read ran
/// with and the snapshot's newest landing.
pub(crate) struct PollRows {
    pub fill: Vec<SessionSpendKey>,
    pub fill_rows: Vec<(SessionSpendKey, SessionSpendRow)>,
    pub since_rows: Vec<(SessionSpendKey, SessionSpendRow)>,
    pub watermark: i64,
    /// `newest_landed_at` in the snapshot, since the incremental read leaves out the rows the fold passes over and
    /// their landings still move the watermark.
    pub newest: i64,
}

/// The row's four members and its count, as the fold answers them for one session.
pub(crate) struct SessionFigures {
    pub user_agent: Option<String>,
    pub spend: Option<SessionSpend>,
    pub day: SessionDay,
    pub agent_requests: Vec<AgentRequests>,
    pub requests: u64,
}

/// The fold's reads of one poll, on the closure's transaction after the activity read: the newest landing, which is
/// the start watermark where the cursor holds none, then the first fill over the `keys` the cursor does not hold, then
/// the incremental read after the watermark. A function of its own so a test runs the sequence on an in-memory ledger
/// as the route does.
pub(crate) fn read_poll(
    conn: &Connection,
    cursor: &FoldCursor,
    keys: &[SessionSpendKey],
) -> Result<PollRows, LedgerError> {
    // A fold that starts takes the snapshot's newest landing, never 0: the first fill answers the listed sessions whole
    // and fills a session that returns to the list whole later, so nothing landed before the start is needed here, and
    // a read from 0 would walk the ledger's whole history on every restart. The reads ride one snapshot, so the one
    // seek serves the start watermark and the poll's newest landing alike.
    let newest = phoebis_ledger::newest_landed_at(conn)?;
    let watermark = cursor.watermark.unwrap_or(newest);
    let fill: Vec<SessionSpendKey> = keys
        .iter()
        .filter(|key| !cursor.held.contains(*key))
        .cloned()
        .collect();
    let fill_rows = phoebis_ledger::session_spend_rows_of(conn, &fill)?;
    let since_rows = phoebis_ledger::session_spend_rows_since(conn, watermark)?;
    Ok(PollRows {
        fill,
        fill_rows,
        since_rows,
        watermark,
        newest,
    })
}

impl SessionSpendFold {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| {
            // A fold that panicked part way left sums no poll can trust, so the fold starts over as a restart does
            // and the next poll fills the listed sessions again.
            self.inner.clear_poison();
            let mut inner = poisoned.into_inner();
            *inner = Inner::default();
            inner
        })
    }

    /// The cursor of one poll at `now`. Where the cost revision in force at `now` is not the one the sums were priced
    /// under (its `effective`, `tables.revisions.in_force(now)`), the fold resets to its start state first (the
    /// sessions with their hours and agents, the watermark), so the poll refills every listed session under the new
    /// anchor as a restart does.
    pub(crate) fn cursor(&self, tables: &PlanTables, now: i64) -> FoldCursor {
        let mut inner = self.lock();
        let effective = &tables.revisions.in_force(now).effective;
        if inner.priced_under.as_ref() != Some(effective) {
            *inner = Inner {
                priced_under: Some(effective.clone()),
                ..Inner::default()
            };
        }
        FoldCursor {
            watermark: inner.watermark,
            held: inner.sessions.keys().cloned().collect(),
        }
    }

    /// Folds one poll's rows, guarded by the ledger's landing stamps, which the writer keeps strictly increasing,
    /// since two polls can copy one cursor, read under two snapshots and fold in either order. A poll whose newest
    /// landing (`rows.newest`, or `rows.watermark` where it is above) is below the fold's watermark is
    /// stale and applies nothing, its fills waiting for the next poll's snapshot, and so does a poll priced under a
    /// cost revision a newer cursor has reset the fold away from. Otherwise each session `rows.fill` names is reset
    /// and filled from its rows, a held one too, since the fill is its whole history in a snapshot at least as new as
    /// the watermark. Then each incremental row landed above the watermark is applied to a session held before this
    /// call and not filled in it. A session held by neither is passed over: the activity read of the poll that lists
    /// it names it, and its first fill reads it whole, the row included. The fold keeps no set of request ids, since
    /// the writer lands a request once, so an incremental row is a request the fold has not seen. The watermark then
    /// moves to the poll's newest landing and never down.
    pub(crate) fn fold(
        &self,
        rows: PollRows,
        prices: &PriceRevisions,
        tables: &PlanTables,
        now: i64,
    ) {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let effective = &tables.revisions.in_force(now).effective;
        if inner
            .priced_under
            .as_ref()
            .is_some_and(|under| under != effective)
        {
            return;
        }
        let newest = rows.newest.max(rows.watermark);
        if inner.watermark.is_some_and(|watermark| newest < watermark) {
            return;
        }
        inner.priced_under = Some(effective.clone());
        let mut filled: BTreeMap<_, Accumulator> = rows
            .fill
            .into_iter()
            .map(|key| (key, Accumulator::new(now)))
            .collect();
        for (key, row) in rows.fill_rows {
            if let Some(accumulator) = filled.get_mut(&key) {
                accumulator.fold(row, prices, tables, now);
            }
        }
        let folded_to = inner.watermark;
        for (key, row) in rows.since_rows {
            if folded_to.is_some_and(|watermark| row.landed_at <= watermark) {
                continue;
            }
            if filled.contains_key(&key) {
                continue;
            }
            if let Some(accumulator) = inner.sessions.get_mut(&key) {
                accumulator.fold(row, prices, tables, now);
            }
        }
        inner.sessions.extend(filled);
        inner.watermark = Some(newest);
    }

    /// The fold held for one poll's answers and the retain after them, so the poll takes the lock once.
    pub(crate) fn read(&self) -> FoldRead<'_> {
        FoldRead(self.lock())
    }
}

pub(crate) struct FoldRead<'a>(MutexGuard<'a, Inner>);

impl FoldRead<'_> {
    /// The row's four members and its count at `now`, `None` where the fold holds nothing for the session.
    pub(crate) fn answer(&self, key: &SessionSpendKey, now: i64) -> Option<SessionFigures> {
        Some(self.0.sessions.get(key)?.figures(now))
    }

    /// Drops the sessions not listed for `expiry` and the buckets older than 25 hours.
    pub(crate) fn retain(
        mut self,
        listed: impl Fn(&SessionSpendKey) -> bool,
        now: i64,
        expiry: Duration,
    ) {
        let kept_from = kept_from(now);
        let expiry = expiry.as_millis() as i64;
        self.0.sessions.retain(|key, accumulator| {
            if listed(key) {
                accumulator.listed_at = accumulator.listed_at.max(now);
            }
            accumulator.hours.retain(|&hour, _| hour >= kept_from);
            now - accumulator.listed_at <= expiry
        });
    }
}

#[cfg(test)]
mod tests {
    use phoebis_ledger::test_tables::{
        api_prices, subscription_costs, tables, FEBRUARY_2026, JANUARY_2026, SUBSCRIPTION_COSTS,
    };
    use phoebis_ledger::{AttemptRecord, CostRevisions, LedgerDb, RequestRecord};

    use super::*;

    /// Listed in the test price file and weighted 2 units per input token in the test cost model's first entry, so
    /// one input token reads 2 / 50 of the Pro week under that entry's anchor.
    const MODEL: &str = "claude-opus-5-5";

    /// A tier the test cost model does not size, so a plan request on it prices no window share.
    const UNSIZED_TIER: &str = "default_claude_enterprise";

    fn key(session: &str) -> SessionSpendKey {
        ("k".into(), 0, session.into())
    }

    fn request(id: &str, session: &str, received_at: i64) -> RequestRecord {
        RequestRecord {
            id: id.into(),
            received_at,
            key_id: Some("k".into()),
            session_id: Some(session.into()),
            canonical_model: MODEL.into(),
            kind: "inference".into(),
            origin: "client".into(),
            ..Default::default()
        }
    }

    /// A plan-funded attempt on account `a` under the unsized tier.
    fn attempt(
        id: &str,
        attempt: u32,
        input_tokens: Option<u64>,
        started_at: i64,
    ) -> AttemptRecord {
        AttemptRecord {
            request_id: id.into(),
            attempt,
            account_id: "a".into(),
            started_at,
            input_tokens,
            tier: Some(UNSIZED_TIER.into()),
            ..Default::default()
        }
    }

    /// A client request no account served, for the tests whose figures are counts.
    fn row(session: &str, received_at: i64, landed_at: i64) -> (SessionSpendKey, SessionSpendRow) {
        (
            key(session),
            SessionSpendRow {
                agent_id: None,
                agent_type: None,
                user_agent: None,
                request_class: None,
                canonical_model: MODEL.into(),
                kind: "inference".into(),
                received_at,
                landed_at,
                attempt: None,
            },
        )
    }

    /// The ledger's rows of `session`, as the first fill reads them.
    fn rows_of(db: &LedgerDb, session: &str) -> Vec<(SessionSpendKey, SessionSpendRow)> {
        db.read_blocking(|c| phoebis_ledger::session_spend_rows_of(c, &[key(session)]))
            .unwrap()
    }

    fn close(got: Option<f64>, want: f64) -> bool {
        got.is_some_and(|got| (got - want).abs() <= 1e-12 * want.abs().max(1.0))
    }

    /// Three requests of `s` landed at 1, 2 and 3: `r1`, whose retry answered with no token, `r2`, plan-funded on `a`
    /// under a tier the tables do not size, and the probe `p1` naming the agent `w`. The first poll fills `s`, the
    /// second hands the same rows back as rows landed since a watermark the first moved, and the third is a poll whose
    /// snapshot is older than what the fold holds.
    #[test]
    fn a_request_folds_once_per_landing_priced_by_its_serving_attempt() {
        let db = LedgerDb::open_in_memory().unwrap();
        let at = JANUARY_2026;
        db.insert_request(
            &request("r1", "s", at),
            &[
                attempt("r1", 1, Some(1_000), at),
                attempt("r1", 2, None, at + 100),
            ],
            None,
        )
        .unwrap();
        db.insert_request(
            &request("r2", "s", at + 1_000),
            &[attempt("r2", 1, Some(1_000), at + 1_000)],
            None,
        )
        .unwrap();
        db.insert_request(
            &RequestRecord {
                agent_id: Some("w".into()),
                kind: "probe".into(),
                origin: "keepalive".into(),
                ..request("p1", "s", at + 2_000)
            },
            &[attempt("p1", 1, Some(1_000), at + 2_000)],
            None,
        )
        .unwrap();
        let (prices, tables) = (api_prices(), subscription_costs());
        let fold = SessionSpendFold::default();
        let s = key("s");
        fold.fold(
            PollRows {
                fill: vec![s.clone()],
                fill_rows: rows_of(&db, "s"),
                since_rows: Vec::new(),
                watermark: 3,
                newest: 3,
            },
            &prices,
            &tables,
            at,
        );
        fold.fold(
            PollRows {
                fill: Vec::new(),
                fill_rows: Vec::new(),
                since_rows: rows_of(&db, "s"),
                watermark: 3,
                newest: 3,
            },
            &prices,
            &tables,
            at,
        );
        let figures = fold.read().answer(&s, at).expect("s is held");
        assert_eq!(
            figures.requests, 2,
            "the rows the first poll folded are at or below the watermark it moved"
        );
        let spend = figures.spend.expect("s holds client requests");
        assert_eq!(
            spend.shares,
            BTreeMap::from([("a".to_string(), BTreeMap::new())]),
            "a plan-funded request names its account though no window priced"
        );
        let w = figures
            .agent_requests
            .iter()
            .find(|entry| entry.agent_id == "w")
            .expect("the probe's agent has an entry");
        assert_eq!(
            (w.spend.probes.requests, w.requests, w.last_request_at),
            (1, 0, None),
            "a probe adds to its agent's probes alone"
        );
        let t = key("t");
        fold.fold(
            PollRows {
                fill: vec![t.clone()],
                fill_rows: vec![row("t", at, 2)],
                since_rows: Vec::new(),
                watermark: 2,
                newest: 2,
            },
            &prices,
            &tables,
            at,
        );
        assert!(
            fold.read().answer(&t, at).is_none(),
            "a poll older than the fold applies nothing"
        );
        assert_eq!(
            fold.cursor(&tables, at).watermark,
            Some(3),
            "the watermark never moves down"
        );
        let alone = SessionSpendFold::default();
        alone.fold(
            PollRows {
                fill: vec![s.clone()],
                fill_rows: rows_of(&db, "s")
                    .into_iter()
                    .filter(|(_, r)| r.received_at == at)
                    .collect(),
                since_rows: Vec::new(),
                watermark: 1,
                newest: 1,
            },
            &prices,
            &tables,
            at,
        );
        let spend = alone
            .read()
            .answer(&s, at)
            .and_then(|figures| figures.spend)
            .expect("r1 is a client request");
        assert_eq!(
            (spend.requests, spend.usd),
            (1, 0.0),
            "r1's serving attempt answered no token, so it prices nothing"
        );
    }

    /// `r2` of `s` reaches both reads of one poll: the fill reads `s` whole and the incremental read the rows landed
    /// after 1. `t` is held by neither the fold nor the fill.
    #[test]
    fn one_poll_fills_its_sessions_before_it_applies_the_rows_landed_since() {
        let (prices, tables) = (api_prices(), subscription_costs());
        let at = JANUARY_2026;
        let fold = SessionSpendFold::default();
        let (s, t) = (key("s"), key("t"));
        fold.fold(
            PollRows {
                fill: vec![s.clone()],
                fill_rows: vec![row("s", at, 1), row("s", at + 1, 2)],
                since_rows: vec![row("s", at + 1, 2), row("t", at + 2, 3)],
                watermark: 1,
                newest: 3,
            },
            &prices,
            &tables,
            at,
        );
        assert_eq!(
            fold.read().answer(&s, at).map(|figures| figures.requests),
            Some(2),
            "the fill read r2 in the same snapshot"
        );
        assert!(
            fold.read().answer(&t, at).is_none(),
            "a session the fold does not hold waits for its first fill"
        );
        let cursor = fold.cursor(&tables, at);
        assert_eq!(
            (cursor.watermark, cursor.held),
            (Some(3), BTreeSet::from([s])),
            "the watermark reaches the poll's newest landing"
        );
    }

    /// The second cost entry's anchor holds 100 units on the Pro week in place of 50, and the request's units stay
    /// the first entry's, since it was received under it: 2 units read 0.04 of the week under the first anchor and
    /// 0.02 under the second.
    #[test]
    fn a_cost_revision_taking_force_starts_the_fold_over_under_its_anchor() {
        let (first, second) = SUBSCRIPTION_COSTS
            .split_once("  - effective: 2026-02-01\n")
            .expect("the cost fixture carries a second entry");
        let anchor = "default_claude_ai:      {five_hour: 5, seven_day: 50}";
        assert!(second.contains(anchor), "{second}");
        let text = format!(
            "{first}  - effective: 2026-02-01\n{}",
            second.replacen(
                anchor,
                "default_claude_ai:      {five_hour: 5, seven_day: 100}",
                1
            )
        );
        let tables = tables(CostRevisions::from_yaml(&text).unwrap());
        let prices = api_prices();
        let db = LedgerDb::open_in_memory().unwrap();
        db.insert_request(
            &request("r2", "s", JANUARY_2026),
            &[attempt("r2", 1, Some(1), JANUARY_2026)],
            None,
        )
        .unwrap();
        let s = key("s");
        let fold = SessionSpendFold::default();
        // The fill the route's read takes from the cursor: the session where the fold does not hold it.
        let poll = |cursor: &FoldCursor| {
            let held = cursor.held.contains(&s);
            let watermark = cursor.watermark.unwrap_or(1);
            PollRows {
                fill: if held { Vec::new() } else { vec![s.clone()] },
                fill_rows: if held { Vec::new() } else { rows_of(&db, "s") },
                since_rows: Vec::new(),
                watermark,
                newest: watermark,
            }
        };
        let cursor = fold.cursor(&tables, JANUARY_2026);
        fold.fold(poll(&cursor), &prices, &tables, JANUARY_2026);
        let equivalent = |now: i64| {
            fold.read()
                .answer(&s, now)
                .and_then(|figures| figures.spend)
                .and_then(|spend| spend.equivalent)
        };
        assert!(
            close(equivalent(JANUARY_2026), 0.04),
            "{:?}",
            equivalent(JANUARY_2026)
        );
        let cursor = fold.cursor(&tables, FEBRUARY_2026);
        assert_eq!(
            (cursor.watermark, cursor.held.is_empty()),
            (None, true),
            "the new anchor starts the fold over"
        );
        // A poll whose clock read the old revision folds after the reset, as an overlapping poll can.
        fold.fold(poll(&cursor), &prices, &tables, JANUARY_2026);
        assert!(
            fold.read().answer(&s, JANUARY_2026).is_none(),
            "a poll priced under the old anchor applies nothing once the fold reset"
        );
        fold.fold(poll(&cursor), &prices, &tables, FEBRUARY_2026);
        assert!(
            close(equivalent(FEBRUARY_2026), 0.02),
            "the history refilled under the new anchor: {:?}",
            equivalent(FEBRUARY_2026)
        );
    }

    /// `r0` of `u` lands at 1 before the first poll, which lists `s` alone. The second poll lists `u` too.
    #[test]
    fn a_fold_starts_at_the_newest_landing_and_fills_a_session_whole_once_listed() {
        let db = LedgerDb::open_in_memory().unwrap();
        db.insert_request(
            &request("r0", "u", JANUARY_2026),
            &[attempt("r0", 1, Some(1_000), JANUARY_2026)],
            None,
        )
        .unwrap();
        let (prices, tables) = (api_prices(), subscription_costs());
        let (s, u) = (key("s"), key("u"));
        let fold = SessionSpendFold::default();
        let poll = |keys: &[SessionSpendKey]| {
            let cursor = fold.cursor(&tables, JANUARY_2026);
            db.read_blocking(|c| read_poll(c, &cursor, keys)).unwrap()
        };
        let first = poll(std::slice::from_ref(&s));
        assert_eq!(
            (first.watermark, first.since_rows.len()),
            (1, 0),
            "the fold starts at the newest landing and reads no history"
        );
        fold.fold(first, &prices, &tables, JANUARY_2026);
        assert!(
            fold.read().answer(&u, JANUARY_2026).is_none(),
            "u is not listed yet"
        );
        let cursor = fold.cursor(&tables, JANUARY_2026);
        assert_eq!(
            (cursor.watermark, cursor.held),
            (Some(1), BTreeSet::from([s.clone()]))
        );
        let second = poll(&[s, u.clone()]);
        fold.fold(second, &prices, &tables, JANUARY_2026);
        assert_eq!(
            fold.read()
                .answer(&u, JANUARY_2026)
                .map(|figures| figures.requests),
            Some(1),
            "the first fill reads a session landed before the start whole"
        );
    }

    /// `now` sits 30 seconds into its hour, so a request 24 hours and a minute before it falls in the hour before the
    /// one that holds `now - 24 h`, and one 23 hours and 59 minutes before it in that hour.
    #[test]
    fn the_day_reads_the_hours_from_the_one_that_holds_a_day_before_now() {
        let (prices, tables) = (api_prices(), subscription_costs());
        let now = JANUARY_2026 + 30_000;
        assert_eq!(now % HOUR_MS, 30_000);
        let fold = SessionSpendFold::default();
        let s = key("s");
        fold.fold(
            PollRows {
                fill: vec![s.clone()],
                fill_rows: vec![
                    row("s", now - 24 * HOUR_MS - 60_000, 1),
                    row("s", now - 24 * HOUR_MS + 60_000, 2),
                ],
                since_rows: Vec::new(),
                watermark: 2,
                newest: 2,
            },
            &prices,
            &tables,
            now,
        );
        let day = fold.read().answer(&s, now).expect("s is held").day;
        assert_eq!(day.requests.values().sum::<u64>(), 1, "{:?}", day.requests);
    }
}
