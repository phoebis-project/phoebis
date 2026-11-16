//! The sessions route's totals kept per session in the ledger. `session_sums` holds a session's request and token
//! counts under every part a price reads, so a reader adds the counts of one key and prices them once, and
//! `session_facts` holds the picks no sum carries. Nothing in either table is a price: every reader prices the counts
//! at its own instant.
//!
//! A session's own facts row is its mark: the totals of a marked session count every request the ledger holds of it.
//! The landing keeps a marked session's totals in its own transaction and marks a session at its first request. The
//! fill in the purge task sums each session no landing marked, a whole session at a time, newest first, so a ledger
//! from before the tables fills behind the start. The fill's cursor stays where the fill ended, so a later start walks
//! no row unless a threshold moved. A start whose price list moved a threshold clears the sessions whose rows it
//! re-bands for the fill to sum again, and a family of the retention purge follows the requests' cutoff. The sessions
//! route reads the rows through `session_figures` and prices them at its own instant.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use phoebis_bus::Kind;
use phoebis_routing::{PinUsage, HOUR_MS};
use phoebis_store::AccountKind;
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Params, Transaction};
use serde::{Deserialize, Serialize};

use crate::db::{begin_write, LANDING_STAMP_SQL};
use crate::plan::{entrypoint_of_client, funding_of, Funding, PlanTables};
use crate::price::{is_free_classifier, RowKey};
use crate::pricing::{Band, PriceRevisions, Speed};
use crate::query::{
    add_figures, boundary_instants, price_request, session_facts_statement, session_hour_statement,
    session_requests_of, session_spend_of_statement, session_sums_statement, RequestPrice,
    SessionSpendKey, SERVING_ATTEMPT_NUMBER, SESSION_IDENTITY_CHUNK,
};
use crate::record::{AttemptRecord, RequestRecord};
use crate::refold::{
    first_difference, read_meta, revisions_of, thresholds_of, write_meta, Thresholds,
};
use crate::rollup::{
    fold_identifier, hour_of, last_millisecond, serving_speed, trailing_day_hours, NO_CLASS,
    PLAN_FUNDING, STANDARD_BAND, UNATTRIBUTED,
};
use crate::{LedgerDb, LedgerError};

/// The agent id of a session's own facts row and of a request with no agent, and the account id of a request no
/// account served. No id takes it: the relay files an agent id of one byte at least, and an account id starts with a
/// letter or a digit.
pub(crate) const RESERVED_ID: &str = "";

/// The `meta` key of the fill's cursor: `{"received_at": <ms>, "rowid": <rowid>, "ended": <bool>}`, the last request
/// row a walk examined, newest first, and whether the fill ended. It stands from a fill's first call until a reset
/// drops it.
const FILL_CURSOR_KEY: &str = "session_totals_fill";

/// The `meta` key of the thresholds the stored sums rows were banded under: `{"revisions": [..]}`, each revision in
/// the form `PRICE_BANDS_KEY` holds it.
const BANDS_KEY: &str = "session_totals_bands";

/// The request rows one call of the fill walks at most while it looks for a session with no mark, so a call that finds
/// none holds the purger's connection for a bounded read.
const FILL_WALK: usize = 1_000;

/// What the totals read of one request: the parts of its record and of its serving attempt, the attempt with the
/// highest number, whatever the order of the trail's list. The landing builds it from the record it writes and the
/// fill from the stored rows, each from the attempt's own row and from no serving column of the request's row, so a
/// row reads alike before and after the backfill.
#[derive(Clone, Debug)]
pub(crate) struct SessionRequest {
    /// `None` for a request with no key or no session, which no session's totals count.
    pub(crate) key: Option<SessionSpendKey>,
    pub(crate) id: String,
    pub(crate) agent_id: Option<String>,
    pub(crate) agent_type: Option<String>,
    pub(crate) user_agent: Option<String>,
    pub(crate) kind: String,
    pub(crate) refused: bool,
    pub(crate) canonical_model: String,
    pub(crate) requested_speed: Option<String>,
    pub(crate) client: Option<String>,
    pub(crate) classifier: Option<String>,
    pub(crate) request_class: Option<String>,
    pub(crate) received_at: i64,
    pub(crate) serving: Option<Serving>,
}

/// The serving attempt's parts a sums key and its counts read.
#[derive(Clone, Debug)]
pub(crate) struct Serving {
    pub(crate) account_id: String,
    pub(crate) account_kind: Option<AccountKind>,
    pub(crate) usage_based: bool,
    pub(crate) overage_in_use: Option<bool>,
    pub(crate) speed: Option<String>,
    pub(crate) tier: Option<String>,
    /// The attempt's finish, else the request's, else the attempt's start, where the meter charges it.
    pub(crate) completed_at: i64,
    pub(crate) input_tokens: Option<u64>,
    pub(crate) cache_read_tokens: Option<u64>,
    pub(crate) cache_write_5m_tokens: Option<u64>,
    pub(crate) cache_write_1h_tokens: Option<u64>,
    pub(crate) output_tokens: Option<u64>,
}

impl Serving {
    fn answered(&self) -> bool {
        self.input_tokens.is_some()
            || self.cache_read_tokens.is_some()
            || self.cache_write_5m_tokens.is_some()
            || self.cache_write_1h_tokens.is_some()
            || self.output_tokens.is_some()
    }

    fn usage(&self) -> PinUsage {
        PinUsage {
            input: self.input_tokens.unwrap_or(0),
            cache_read: self.cache_read_tokens.unwrap_or(0),
            write_5m: self.cache_write_5m_tokens.unwrap_or(0),
            write_1h: self.cache_write_1h_tokens.unwrap_or(0),
            output: self.output_tokens.unwrap_or(0),
        }
    }
}

impl SessionRequest {
    pub(crate) fn of(request: &RequestRecord, attempts: &[AttemptRecord]) -> SessionRequest {
        let key =
            request
                .key_id
                .clone()
                .zip(request.session_id.clone())
                .map(|(key_id, session_id)| SessionSpendKey {
                    key_id,
                    key_generation: request.key_generation,
                    session_id,
                });
        SessionRequest {
            key,
            id: request.id.clone(),
            agent_id: request.agent_id.clone(),
            agent_type: request.agent_type.clone(),
            user_agent: request.user_agent.clone(),
            kind: request.kind.clone(),
            refused: request.refused,
            canonical_model: request.canonical_model.clone(),
            requested_speed: request.requested_speed.clone(),
            client: request.client.clone(),
            classifier: request.classifier.clone(),
            request_class: request.request_class.clone(),
            received_at: request.received_at,
            serving: attempts.iter().max_by_key(|a| a.attempt).map(|a| Serving {
                account_id: a.account_id.clone(),
                account_kind: Some(a.account_kind),
                usage_based: a.usage_based,
                overage_in_use: a.overage_in_use,
                speed: a.speed.clone(),
                tier: a.tier.clone(),
                completed_at: a
                    .finished_at
                    .or(request.finished_at)
                    .unwrap_or(a.started_at),
                input_tokens: a.input_tokens,
                cache_read_tokens: a.cache_read_tokens,
                cache_write_5m_tokens: a.cache_write_5m_tokens,
                cache_write_1h_tokens: a.cache_write_1h_tokens,
                output_tokens: a.output_tokens,
            }),
        }
    }

    fn probe(&self) -> bool {
        self.kind == Kind::Probe.as_str()
    }
}

/// The parts of a `session_sums` row's key beside the session's three, in the table's order.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SumsKey {
    pub(crate) agent_id: String,
    pub(crate) probe: bool,
    pub(crate) account_id: String,
    pub(crate) canonical_model: String,
    pub(crate) speed: String,
    pub(crate) funding: String,
    pub(crate) band: String,
    pub(crate) tier: String,
    pub(crate) client: String,
    pub(crate) classifier: String,
    pub(crate) request_class: String,
    /// Seconds, as `hour_of` answers them.
    pub(crate) received_hour: i64,
    pub(crate) completed_hour: i64,
}

/// A `session_sums` row's counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    pub(crate) requests: u64,
    pub(crate) input_tokens: u64,
    pub(crate) cache_read_tokens: u64,
    pub(crate) cache_write_5m_tokens: u64,
    pub(crate) cache_write_1h_tokens: u64,
    pub(crate) output_tokens: u64,
}

impl Counts {
    pub(crate) fn add(&mut self, other: Counts) {
        self.requests += other.requests;
        self.input_tokens += other.input_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_5m_tokens += other.cache_write_5m_tokens;
        self.cache_write_1h_tokens += other.cache_write_1h_tokens;
        self.output_tokens += other.output_tokens;
    }
}

/// The sums key and the counts `request` adds to its session's totals, `None` for a refused request and a request
/// with no key or no session, which add to no sums row. The landing, the unfold of a request landed again, the fill
/// and the purge's split hour each read a request through this one function. The key holds the words `deltas_of`
/// folds for the hourly key, the funding's rollup word and the band of the serving attempt's own prompt at the
/// thresholds of `prices` in force at the receipt, and the counts the serving attempt's five token counts, a missing
/// count read as 0. A request no account served keys the reserved account and its receipt's hour as its completion's.
pub(crate) fn sums_of(
    request: &SessionRequest,
    prices: &PriceRevisions,
) -> Option<(SumsKey, Counts)> {
    if request.key.is_none() || request.refused {
        return None;
    }
    let serving = request.serving.as_ref();
    let answered = serving.filter(|s| s.answered());
    let folded =
        |raw: Option<&str>, none: &str| raw.map_or_else(|| none.to_string(), fold_identifier);
    let key = SumsKey {
        agent_id: request
            .agent_id
            .clone()
            .unwrap_or_else(|| RESERVED_ID.to_string()),
        probe: request.probe(),
        account_id: serving.map_or_else(|| RESERVED_ID.to_string(), |s| s.account_id.clone()),
        canonical_model: request.canonical_model.clone(),
        speed: serving_speed(
            serving.map(|s| (s.speed.as_deref(), s.answered())),
            request.requested_speed.as_deref(),
        )
        .to_string(),
        funding: serving
            .map_or(PLAN_FUNDING, |s| {
                funding_of(
                    s.account_kind,
                    s.usage_based,
                    s.overage_in_use,
                    Speed::of(s.speed.as_deref()),
                )
                .rollup_word()
            })
            .to_string(),
        band: answered
            .map_or(STANDARD_BAND, |s| {
                prices
                    .in_force(request.received_at)
                    .band_of(&request.canonical_model, s.usage().prompt_tokens())
                    .as_str()
            })
            .to_string(),
        tier: serving
            .and_then(|s| s.tier.clone())
            .unwrap_or_else(|| UNATTRIBUTED.to_string()),
        client: folded(request.client.as_deref(), UNATTRIBUTED),
        classifier: folded(request.classifier.as_deref(), NO_CLASS),
        request_class: folded(request.request_class.as_deref(), NO_CLASS),
        received_hour: hour_of(request.received_at),
        completed_hour: hour_of(serving.map_or(request.received_at, |s| s.completed_at)),
    };
    let usage = serving.map(Serving::usage).unwrap_or_default();
    Some((
        key,
        Counts {
            requests: 1,
            input_tokens: usage.input,
            cache_read_tokens: usage.cache_read,
            cache_write_5m_tokens: usage.write_5m,
            cache_write_1h_tokens: usage.write_1h,
            output_tokens: usage.output,
        },
    ))
}

/// One `session_facts` row's columns beside its key. The session's own row holds the first five, an agent's row the
/// rest.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Facts {
    pub(crate) last_request_at: Option<i64>,
    pub(crate) last_inference_at: Option<i64>,
    pub(crate) served_at: Option<i64>,
    pub(crate) served_id: Option<String>,
    pub(crate) user_agent: Option<String>,
    pub(crate) first_request_at: Option<i64>,
    pub(crate) newest_at: Option<i64>,
    pub(crate) newest_id: Option<String>,
    pub(crate) newest_model: Option<String>,
    pub(crate) newest_agent_type: Option<String>,
    pub(crate) newest_account_id: Option<String>,
    pub(crate) probe_at: Option<i64>,
    pub(crate) probe_id: Option<String>,
    pub(crate) probe_model: Option<String>,
}

/// Whether `request` is newer than the pick at `at` with `id`, by receipt and then id, so two requests of one instant
/// resolve the same whichever order a read visits them.
fn newer(request: &SessionRequest, at: Option<i64>, id: Option<&str>) -> bool {
    at.zip(id)
        .is_none_or(|pick| (request.received_at, request.id.as_str()) > pick)
}

impl Facts {
    /// `request` on the session's own row: the newest receipt of a client request and of one that is no token count,
    /// refused ones included, and the newest served client request with its own user agent, null included.
    fn add_to_session(&mut self, request: &SessionRequest) {
        if request.probe() {
            return;
        }
        self.last_request_at = self.last_request_at.max(Some(request.received_at));
        if request.kind != Kind::CountTokens.as_str() {
            self.last_inference_at = self.last_inference_at.max(Some(request.received_at));
        }
        if !request.refused && newer(request, self.served_at, self.served_id.as_deref()) {
            self.served_at = Some(request.received_at);
            self.served_id = Some(request.id.clone());
            self.user_agent = request.user_agent.clone();
        }
    }

    /// `request` on its agent's row: the agent's first client request, refused ones included, its newest served client
    /// request and its newest served probe.
    fn add_to_agent(&mut self, request: &SessionRequest) {
        if !request.probe() {
            self.first_request_at = Some(
                self.first_request_at
                    .map_or(request.received_at, |at| at.min(request.received_at)),
            );
        }
        if request.refused {
            return;
        }
        if request.probe() {
            if newer(request, self.probe_at, self.probe_id.as_deref()) {
                self.probe_at = Some(request.received_at);
                self.probe_id = Some(request.id.clone());
                self.probe_model = Some(request.canonical_model.clone());
            }
        } else if newer(request, self.newest_at, self.newest_id.as_deref()) {
            self.newest_at = Some(request.received_at);
            self.newest_id = Some(request.id.clone());
            self.newest_model = Some(request.canonical_model.clone());
            self.newest_agent_type = request.agent_type.clone();
            self.newest_account_id = request.serving.as_ref().map(|s| s.account_id.clone());
        }
    }

    /// The instant of `pick`, `None` where the row holds none.
    fn instant(&self, pick: Pick) -> Option<i64> {
        match pick {
            Pick::LastRequest => self.last_request_at,
            Pick::LastInference => self.last_inference_at,
            Pick::Served => self.served_at,
            Pick::FirstRequest => self.first_request_at,
            Pick::Newest => self.newest_at,
            Pick::Probe => self.probe_at,
        }
    }

    /// Whether `request` is the one `pick` names: its id is the pick's, or its receipt the pick's instant where the
    /// pick keeps no id.
    fn held(&self, pick: Pick, request: &SessionRequest) -> bool {
        let id = Some(request.id.as_str());
        match pick {
            Pick::LastRequest | Pick::LastInference | Pick::FirstRequest => {
                self.instant(pick) == Some(request.received_at)
            }
            Pick::Served => self.served_id.as_deref() == id,
            Pick::Newest => self.newest_id.as_deref() == id,
            Pick::Probe => self.probe_id.as_deref() == id,
        }
    }
}

/// A pick of a facts row, which `reread` reads again on its own: the session row's first three, an agent row's last
/// three.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pick {
    LastRequest,
    LastInference,
    Served,
    FirstRequest,
    Newest,
    Probe,
}

impl Pick {
    const ALL: [Pick; 6] = [
        Pick::LastRequest,
        Pick::LastInference,
        Pick::Served,
        Pick::FirstRequest,
        Pick::Newest,
        Pick::Probe,
    ];
}

/// One session's totals: its sums rows by key and its facts rows by agent, the session's own row under `RESERVED_ID`.
#[derive(Debug)]
pub(crate) struct Totals {
    pub(crate) sums: BTreeMap<SumsKey, Counts>,
    pub(crate) facts: BTreeMap<String, Facts>,
}

impl Totals {
    /// The totals of a session that holds no request yet: its own facts row alone, the mark.
    fn new() -> Totals {
        Totals {
            sums: BTreeMap::new(),
            facts: BTreeMap::from([(RESERVED_ID.to_string(), Facts::default())]),
        }
    }

    fn add(&mut self, request: &SessionRequest, prices: &PriceRevisions) {
        self.facts
            .entry(RESERVED_ID.to_string())
            .or_default()
            .add_to_session(request);
        if let Some(agent_id) = &request.agent_id {
            self.facts
                .entry(agent_id.clone())
                .or_default()
                .add_to_agent(request);
        }
        if let Some((sums, counts)) = sums_of(request, prices) {
            self.sums.entry(sums).or_default().add(counts);
        }
    }
}

/// The totals of session `key` summed from every request the ledger holds of it under its key, on `conn`'s snapshot:
/// the fill's sum, which a reader runs as well for a session the fill has not reached.
pub(crate) fn totals_of(
    conn: &Connection,
    key: &SessionSpendKey,
    prices: &PriceRevisions,
) -> Result<Totals, LedgerError> {
    let (sql, binds) = session_spend_of_statement(std::slice::from_ref(key));
    let mut totals = Totals::new();
    for request in session_requests_of(conn, &sql, binds)? {
        totals.add(&request, prices);
    }
    Ok(totals)
}

/// The landing's writes into a session's totals, in the transaction of `insert_request_in` and `fold_request` and
/// behind both. A request landed again first leaves the sums row its replaced record was added to, and each pick that
/// record held is read again from the session's request rows that remain. The request then adds to a marked session
/// alone, its counts and its facts, a refused one its facts alone. A session with no mark that holds no other request
/// under its key takes the request as its first and its mark with it. One that holds another is left to the fill,
/// which counts the request with the rest.
pub(crate) fn land(
    tx: &Transaction<'_>,
    request: &RequestRecord,
    attempts: &[AttemptRecord],
    previous: Option<(&RequestRecord, &[AttemptRecord])>,
    prices: &PriceRevisions,
) -> Result<(), LedgerError> {
    if let Some((record, record_attempts)) = previous {
        let old = SessionRequest::of(record, record_attempts);
        if let Some(key) = &old.key {
            if marked(tx, key)? {
                if let Some((sums, counts)) = sums_of(&old, prices) {
                    take_sums(tx, key, &sums, counts)?;
                }
                for agent in std::iter::once(RESERVED_ID).chain(old.agent_id.as_deref()) {
                    if let Some(facts) = read_facts(tx, key, agent)? {
                        reread(tx, key, agent, facts, |facts, pick| facts.held(pick, &old))?;
                    }
                }
            }
        }
    }
    let new = SessionRequest::of(request, attempts);
    let Some(key) = &new.key else {
        return Ok(());
    };
    if !marked(tx, key)? {
        if !holds_another(tx, key, &new.id)? {
            let mut totals = Totals::new();
            totals.add(&new, prices);
            write_totals(tx, key, &totals)?;
        }
        return Ok(());
    }
    if let Some((sums, counts)) = sums_of(&new, prices) {
        add_sums(tx, key, &sums, counts)?;
    }
    for agent in std::iter::once(RESERVED_ID).chain(new.agent_id.as_deref()) {
        let mut facts = read_facts(tx, key, agent)?.unwrap_or_default();
        if agent == RESERVED_ID {
            facts.add_to_session(&new);
        } else {
            facts.add_to_agent(&new);
        }
        write_facts(tx, key, agent, &facts)?;
    }
    Ok(())
}

/// The last request row a walk of the fill examined, in `(received_at, rowid)` order, newest first.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct FillCursor {
    received_at: i64,
    rowid: i64,
    /// Whether the walk that stored this cursor found no row left, so the fill ended.
    ended: bool,
}

/// Where a walk starts with no cursor stored: above every request row.
const FILL_TOP: FillCursor = FillCursor {
    received_at: i64::MAX,
    rowid: i64::MAX,
    ended: false,
};

/// The next request rows below the cursor, newest first, through the received index, which holds the rowid.
const FILL_WALK_SQL: &str = "SELECT rowid, received_at, key_id, key_generation, session_id FROM requests \
    INDEXED BY requests_received WHERE (received_at, rowid) < (?1, ?2) ORDER BY received_at DESC, rowid DESC LIMIT ?3";

/// One walk's result: the last row it passed and the first session it met with no mark, beside that session's row.
struct Walk {
    passed: FillCursor,
    unmarked: Option<(SessionSpendKey, FillCursor)>,
    /// The walk read fewer rows than `FILL_WALK`, so no row lies below the ones it read.
    ended: bool,
}

fn walk(conn: &Connection, from: FillCursor) -> Result<Walk, LedgerError> {
    let mut stmt = conn.prepare_cached(FILL_WALK_SQL)?;
    let mut rows = stmt.query(params![from.received_at, from.rowid, FILL_WALK as i64])?;
    let mut passed = from;
    let mut read = 0;
    let mut seen = BTreeSet::new();
    while let Some(row) = rows.next()? {
        read += 1;
        let at = FillCursor {
            received_at: row.get(1)?,
            rowid: row.get(0)?,
            ended: false,
        };
        let key_id: Option<String> = row.get(2)?;
        let session_id: Option<String> = row.get(4)?;
        if let Some((key_id, session_id)) = key_id.zip(session_id) {
            let key = SessionSpendKey {
                key_id,
                key_generation: row.get(3)?,
                session_id,
            };
            if !seen.contains(&key) {
                if !marked(conn, &key)? {
                    return Ok(Walk {
                        passed,
                        unmarked: Some((key, at)),
                        ended: false,
                    });
                }
                seen.insert(key);
            }
        }
        passed = at;
    }
    Ok(Walk {
        passed,
        unmarked: None,
        ended: read < FILL_WALK,
    })
}

/// One call of the fill: a walk below the cursor to the newest session with no mark, its sum on a read snapshot, then
/// one write transaction that writes the sum with the mark and moves the cursor past the session's row. A request of
/// the session that landed after the snapshot's newest landing makes the write leave the sum out and the cursor short
/// of the row, so the next call sums the session again: no landing batch waits on a session's sum, and no request is
/// counted twice or left out. The call that finds no row left stores the cursor as ended, and a call that reads an
/// ended cursor walks no row and writes nothing, so a start behind a fill that ended costs one call. A start behind a
/// fill that a shutdown cut short goes on below the stored cursor. Two resets drop the cursor, and the fill then walks
/// from the top: the start's step for a moved threshold, and a purge slice that deleted request rows while the fill
/// had not ended, since those rows can be the older requests of a session that resumed above the cursor. A session
/// with no mark above an ended cursor waits for the next walk from the top, which a moved threshold brings, and the
/// reader sums it from its request rows until then. Answers whether the walk left rows below the cursor.
pub(crate) fn fill_step(db: &LedgerDb, prices: &PriceRevisions) -> Result<bool, LedgerError> {
    db.purge(|conn| {
        let (walked, summed) = {
            let read = conn.transaction()?;
            let cursor = read_meta::<FillCursor>(&read, FILL_CURSOR_KEY)?.unwrap_or(FILL_TOP);
            if cursor.ended {
                return Ok(false);
            }
            let walked = walk(&read, cursor)?;
            let summed = match &walked.unmarked {
                Some((key, _)) => Some((
                    totals_of(&read, key, prices)?,
                    read.query_row(LANDING_STAMP_SQL, [], |r| r.get::<_, i64>(0))?,
                )),
                None => None,
            };
            (walked, summed)
        };
        let tx = begin_write(conn)?;
        let mut cursor = walked.passed;
        if let (Some((key, at)), Some((totals, noted))) = (&walked.unmarked, summed) {
            if !landed_since(&tx, key, noted)? {
                write_totals(&tx, key, &totals)?;
                cursor = *at;
            }
        }
        let left = walked.unmarked.is_some() || !walked.ended;
        cursor.ended = !left;
        write_meta(&tx, FILL_CURSOR_KEY, &cursor)?;
        tx.commit()?;
        Ok(left)
    })
}

/// Drops the cursor of a fill that has not ended. A purge slice calls it in the transaction that deleted request rows:
/// those rows can be the older requests of a session that resumed above the cursor, which no walk below the cursor
/// meets again, so the fill walks from the top.
pub(crate) fn drop_cursor_under_way(tx: &Transaction<'_>) -> Result<(), LedgerError> {
    if read_meta::<FillCursor>(tx, FILL_CURSOR_KEY)?.is_some_and(|cursor| !cursor.ended) {
        tx.execute("DELETE FROM meta WHERE key = ?1", params![FILL_CURSOR_KEY])?;
    }
    Ok(())
}

/// The thresholds `BANDS_KEY` holds.
#[derive(Debug, Serialize, Deserialize)]
struct Banded {
    revisions: Vec<Thresholds>,
}

/// The start's step for a moved threshold, behind `refold_bands` and ahead of the ledger task. A start that finds the
/// stored thresholds differ from `prices` clears each session with a sums row at or after the first hour the two
/// lists differ, with no `since` floor, so a request `refold_bands` leaves out of its walk moves too. The clears, the
/// reset of the fill's cursor, which drops the cursor whether its fill ended or a shutdown cut it short, and the write
/// of the loaded list commit together, and the fill then sums those sessions again.
/// A first start finds no stored list, writes it and clears nothing. Answers the sessions cleared, and sums nothing.
pub fn unmark_rebanded_sessions(
    db: &LedgerDb,
    prices: &PriceRevisions,
) -> Result<usize, LedgerError> {
    let loaded = thresholds_of(prices);
    db.write(|c| {
        let tx = begin_write(c)?;
        let mut cleared = 0;
        match read_meta::<Banded>(&tx, BANDS_KEY)? {
            Some(stored) if stored.revisions == loaded => return Ok(0),
            Some(stored) => {
                let (old, new) = (revisions_of(&stored.revisions)?, revisions_of(&loaded)?);
                if let Some(first) = first_difference(&old, &new, i64::MIN) {
                    for key in keys_of(&tx, SESSIONS_FROM_HOUR, params![hour_of(first)])? {
                        clear_session(&tx, &key)?;
                        cleared += 1;
                    }
                }
                tx.execute("DELETE FROM meta WHERE key = ?1", params![FILL_CURSOR_KEY])?;
            }
            None => {}
        }
        write_meta(&tx, BANDS_KEY, &Banded { revisions: loaded })?;
        tx.commit()?;
        Ok(cleared)
    })
}

/// The sessions a purge at `cutoff` touches, read ahead of its cut, which deletes the rows that name them.
pub(crate) struct Touched {
    /// Each session with a sums row in an hour that ends at or before the cutoff or with a facts instant before it.
    sessions: BTreeSet<SessionSpendKey>,
    /// Each session with a sums row in the hour the cutoff splits, which the pass sums again.
    split: BTreeSet<SessionSpendKey>,
}

/// `Touched` at `cutoff`, read outside the write lock. A session whose old requests were all refused holds no sums row
/// before the cutoff, and its facts name it.
pub(crate) fn touched(db: &LedgerDb, cutoff: i64) -> Result<Touched, LedgerError> {
    db.purge(|conn| {
        let read = conn.transaction()?;
        let split_hour = hour_of(cutoff);
        let mut sessions = keys_of(&read, SESSIONS_BEFORE_HOUR, params![split_hour])?;
        sessions.extend(keys_of(&read, SESSIONS_WITH_FACTS_BEFORE, params![cutoff])?);
        // A cutoff on an hour's first millisecond splits no hour: every request of the hour it opens stays.
        let split = if cutoff.rem_euclid(HOUR_MS) == 0 {
            BTreeSet::new()
        } else {
            keys_of(&read, SESSIONS_IN_HOUR, params![split_hour])?
        };
        Ok(Touched { sessions, split })
    })
}

/// The rest of the family's pass behind its cut, one write transaction per session: the hour the cutoff splits summed
/// again from the requests that remain in it, then each touched session's picks that lie before the cutoff read again,
/// or its rows of both tables deleted where no request of it is left. Answers the rows it deleted.
pub(crate) fn settle(
    db: &LedgerDb,
    cutoff: i64,
    prices: &PriceRevisions,
    touched: &Touched,
) -> Result<usize, LedgerError> {
    let split_hour = hour_of(cutoff);
    let mut deleted = 0;
    for key in &touched.split {
        deleted += db.purge(|conn| {
            let tx = begin_write(conn)?;
            let deleted = sum_hour_again(&tx, key, split_hour, prices)?;
            tx.commit()?;
            Ok(deleted)
        })?;
    }
    for key in touched.sessions.union(&touched.split) {
        deleted += db.purge(|conn| {
            let tx = begin_write(conn)?;
            let deleted = settle_session(&tx, key, cutoff)?;
            tx.commit()?;
            Ok(deleted)
        })?;
    }
    Ok(deleted)
}

/// Session `key`'s rows of the receipt hour `hour` summed again from its request rows of that hour, through
/// `sums_of`. Answers the rows that left.
fn sum_hour_again(
    tx: &Transaction<'_>,
    key: &SessionSpendKey,
    hour: i64,
    prices: &PriceRevisions,
) -> Result<usize, LedgerError> {
    if !marked(tx, key)? {
        return Ok(0);
    }
    let stored = read_sums(tx, key, hour)?;
    let (sql, binds) = session_hour_statement(key, hour);
    let mut summed: BTreeMap<SumsKey, Counts> = BTreeMap::new();
    for request in session_requests_of(tx, &sql, binds)? {
        if let Some((sums, counts)) = sums_of(&request, prices) {
            summed.entry(sums).or_default().add(counts);
        }
    }
    let mut deleted = 0;
    for sums in stored.keys().filter(|sums| !summed.contains_key(*sums)) {
        tx.prepare_cached(&DELETE_SUMS)?
            .execute(params_from_iter(row_binds(key, sums)))?;
        deleted += 1;
    }
    for (sums, counts) in &summed {
        if stored.get(sums) != Some(counts) {
            put_sums(tx, key, sums, *counts)?;
        }
    }
    Ok(deleted)
}

fn settle_session(
    tx: &Transaction<'_>,
    key: &SessionSpendKey,
    cutoff: i64,
) -> Result<usize, LedgerError> {
    if !holds(tx, HOLDS_ANY, key, None)? {
        return clear_session(tx, key);
    }
    let mut deleted = 0;
    for (agent, facts) in read_all_facts(tx, key)? {
        deleted += reread(tx, key, &agent, facts, |facts, pick| {
            facts.instant(pick).is_some_and(|at| at < cutoff)
        })?;
    }
    Ok(deleted)
}

/// The predicate on a session's key in both tables, bound as `session_binds` binds it.
const SESSION_KEY: &str = "key_id = ?1 AND key_generation = ?2 AND session_id = ?3";

/// The sums table's primary key in its order: the terms of `SUMS_ROW_KEY` and the row values the retention purge
/// selects by.
pub(crate) const SUMS_KEY_COLUMNS: [&str; 16] = [
    "key_id",
    "key_generation",
    "session_id",
    "agent_id",
    "probe",
    "account_id",
    "canonical_model",
    "speed",
    "funding",
    "band",
    "tier",
    "client",
    "classifier",
    "request_class",
    "received_hour",
    "completed_hour",
];

/// A sums row's key bound as `row_binds` binds it.
static SUMS_ROW_KEY: LazyLock<String> = LazyLock::new(|| {
    SUMS_KEY_COLUMNS
        .iter()
        .enumerate()
        .map(|(i, column)| format!("{column} = ?{}", i + 1))
        .collect::<Vec<_>>()
        .join(" AND ")
});

/// The sums columns past the session's three, in `SumsKey`'s order and then the counts'.
pub(crate) const SUMS_COLUMNS: &str = "agent_id, probe, account_id, canonical_model, speed, funding, band, tier, client, \
    classifier, request_class, received_hour, completed_hour, requests, input_tokens, cache_read_tokens, \
    cache_write_5m_tokens, cache_write_1h_tokens, output_tokens";

/// The facts columns past the key's four, in `Facts`' order.
pub(crate) const FACTS_COLUMNS: &str = "last_request_at, last_inference_at, served_at, served_id, user_agent, first_request_at, \
    newest_at, newest_id, newest_model, newest_agent_type, newest_account_id, probe_at, probe_id, probe_model";

static ADD_SUMS: LazyLock<String> = LazyLock::new(|| {
    format!(
        "INSERT INTO session_sums (key_id, key_generation, session_id, {SUMS_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, \
         ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22) ON CONFLICT DO UPDATE SET \
         requests = requests + excluded.requests, input_tokens = input_tokens + excluded.input_tokens, \
         cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens, \
         cache_write_5m_tokens = cache_write_5m_tokens + excluded.cache_write_5m_tokens, \
         cache_write_1h_tokens = cache_write_1h_tokens + excluded.cache_write_1h_tokens, \
         output_tokens = output_tokens + excluded.output_tokens"
    )
});

static PUT_SUMS: LazyLock<String> = LazyLock::new(|| {
    format!(
        "INSERT OR REPLACE INTO session_sums (key_id, key_generation, session_id, {SUMS_COLUMNS}) VALUES (?1, ?2, \
         ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)"
    )
});

static TAKE_SUMS: LazyLock<String> = LazyLock::new(|| {
    format!(
        "UPDATE session_sums SET requests = MAX(requests - ?17, 0), input_tokens = MAX(input_tokens - ?18, 0), \
         cache_read_tokens = MAX(cache_read_tokens - ?19, 0), \
         cache_write_5m_tokens = MAX(cache_write_5m_tokens - ?20, 0), \
         cache_write_1h_tokens = MAX(cache_write_1h_tokens - ?21, 0), output_tokens = MAX(output_tokens - ?22, 0) \
         WHERE {}",
        *SUMS_ROW_KEY
    )
});

static DELETE_SUMS: LazyLock<String> =
    LazyLock::new(|| format!("DELETE FROM session_sums WHERE {}", *SUMS_ROW_KEY));

static EMPTIED_SUMS: LazyLock<String> = LazyLock::new(|| {
    format!(
        "DELETE FROM session_sums WHERE {} AND requests = 0",
        *SUMS_ROW_KEY
    )
});

static READ_SUMS_OF_HOUR: LazyLock<String> = LazyLock::new(|| {
    format!("SELECT {SUMS_COLUMNS} FROM session_sums WHERE {SESSION_KEY} AND received_hour = ?4")
});

static READ_FACTS: LazyLock<String> = LazyLock::new(|| {
    format!("SELECT {FACTS_COLUMNS} FROM session_facts WHERE {SESSION_KEY} AND agent_id = ?4")
});

static READ_ALL_FACTS: LazyLock<String> = LazyLock::new(|| {
    format!("SELECT agent_id, {FACTS_COLUMNS} FROM session_facts WHERE {SESSION_KEY}")
});

static PUT_FACTS: LazyLock<String> = LazyLock::new(|| {
    format!(
        "INSERT OR REPLACE INTO session_facts (key_id, key_generation, session_id, agent_id, {FACTS_COLUMNS}) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)"
    )
});

/// The session's own facts row, its mark, with `RESERVED_ID` bound as `?4`.
const MARKED: &str =
    "SELECT 1 FROM session_facts WHERE key_id = ?1 AND key_generation = ?2 AND session_id = ?3 \
    AND agent_id = ?4";

/// A request of the session under its key other than `?4`. The reads of a session's requests walk the session index of
/// its id and check the key on each row.
const HOLDS_ANOTHER: &str =
    "SELECT 1 FROM requests INDEXED BY requests_session WHERE session_id = ?3 AND key_id = ?1 \
    AND key_generation = ?2 AND id <> ?4 LIMIT 1";

const HOLDS_ANY: &str =
    "SELECT 1 FROM requests INDEXED BY requests_session WHERE session_id = ?3 AND key_id = ?1 \
    AND key_generation = ?2 LIMIT 1";

/// A request of the session landed after the stamp `?4`, through the landing index, since few rows lie past it.
const LANDED_SINCE: &str =
    "SELECT 1 FROM requests INDEXED BY requests_landed_at WHERE landed_at > ?4 \
    AND session_id = ?3 AND key_id = ?1 AND key_generation = ?2 LIMIT 1";

/// The picks `Facts::add_to_session` and `Facts::add_to_agent` keep, read again from the request rows. Each read walks
/// the session index from one end and stops at its first match. `probe` and `count_tokens` are the words of
/// `Kind::Probe` and `Kind::CountTokens`.
const SESSION_NEWEST: &str =
    "SELECT received_at FROM requests INDEXED BY requests_session WHERE session_id = ?3 \
    AND key_id = ?1 AND key_generation = ?2 AND kind <> 'probe' ORDER BY received_at DESC LIMIT 1";

const SESSION_NEWEST_INFERENCE: &str = "SELECT received_at FROM requests INDEXED BY requests_session \
    WHERE session_id = ?3 AND key_id = ?1 AND key_generation = ?2 AND kind NOT IN ('probe', 'count_tokens') \
    ORDER BY received_at DESC LIMIT 1";

const SESSION_SERVED: &str = "SELECT received_at, id, user_agent FROM requests INDEXED BY requests_session \
    WHERE session_id = ?3 AND key_id = ?1 AND key_generation = ?2 AND kind <> 'probe' AND refused = 0 \
    ORDER BY received_at DESC, id DESC LIMIT 1";

const AGENT_FIRST: &str = "SELECT received_at FROM requests INDEXED BY requests_session WHERE session_id = ?3 \
    AND key_id = ?1 AND key_generation = ?2 AND agent_id = ?4 AND kind <> 'probe' ORDER BY received_at LIMIT 1";

static AGENT_NEWEST: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT r.received_at, r.id, r.canonical_model, r.agent_type, a.account_id \
         FROM requests r INDEXED BY requests_session \
         LEFT JOIN attempts a ON a.request_id = r.id AND a.attempt = {SERVING_ATTEMPT_NUMBER} \
         WHERE r.session_id = ?3 AND r.key_id = ?1 AND r.key_generation = ?2 AND r.agent_id = ?4 \
         AND r.kind <> 'probe' AND r.refused = 0 ORDER BY r.received_at DESC, r.id DESC LIMIT 1"
    )
});

const AGENT_PROBE: &str = "SELECT received_at, id, canonical_model FROM requests INDEXED BY requests_session \
    WHERE session_id = ?3 AND key_id = ?1 AND key_generation = ?2 AND agent_id = ?4 AND kind = 'probe' \
    AND refused = 0 ORDER BY received_at DESC, id DESC LIMIT 1";

/// The sessions with a sums row from the receipt hour `?1` on, through the index.
const SESSIONS_FROM_HOUR: &str =
    "SELECT DISTINCT key_id, key_generation, session_id FROM session_sums \
    INDEXED BY session_sums_received_hour WHERE received_hour >= ?1";

const SESSIONS_BEFORE_HOUR: &str =
    "SELECT DISTINCT key_id, key_generation, session_id FROM session_sums \
    INDEXED BY session_sums_received_hour WHERE received_hour < ?1";

const SESSIONS_IN_HOUR: &str =
    "SELECT DISTINCT key_id, key_generation, session_id FROM session_sums \
    INDEXED BY session_sums_received_hour WHERE received_hour = ?1";

/// A walk of the facts table, which the purge runs outside the write lock, since no index leads with an instant.
const SESSIONS_WITH_FACTS_BEFORE: &str = "SELECT DISTINCT key_id, key_generation, session_id FROM session_facts \
    WHERE last_request_at < ?1 OR last_inference_at < ?1 OR served_at < ?1 OR first_request_at < ?1 \
    OR newest_at < ?1 OR probe_at < ?1";

fn session_binds(key: &SessionSpendKey) -> [Value; 3] {
    [
        Value::Text(key.key_id.clone()),
        Value::Integer(key.key_generation),
        Value::Text(key.session_id.clone()),
    ]
}

/// The sums row's sixteen key parts in `SUMS_ROW_KEY`'s order.
fn row_binds(key: &SessionSpendKey, sums: &SumsKey) -> Vec<Value> {
    let mut binds = session_binds(key).to_vec();
    binds.extend([
        Value::Text(sums.agent_id.clone()),
        Value::Integer(i64::from(sums.probe)),
        Value::Text(sums.account_id.clone()),
        Value::Text(sums.canonical_model.clone()),
        Value::Text(sums.speed.clone()),
        Value::Text(sums.funding.clone()),
        Value::Text(sums.band.clone()),
        Value::Text(sums.tier.clone()),
        Value::Text(sums.client.clone()),
        Value::Text(sums.classifier.clone()),
        Value::Text(sums.request_class.clone()),
        Value::Integer(sums.received_hour),
        Value::Integer(sums.completed_hour),
    ]);
    binds
}

/// `row_binds` with the six counts after them, as the writes of a sums row bind them.
fn counted_binds(key: &SessionSpendKey, sums: &SumsKey, counts: Counts) -> Vec<Value> {
    let mut binds = row_binds(key, sums);
    binds.extend(
        [
            counts.requests,
            counts.input_tokens,
            counts.cache_read_tokens,
            counts.cache_write_5m_tokens,
            counts.cache_write_1h_tokens,
            counts.output_tokens,
        ]
        .map(|n| Value::Integer(n as i64)),
    );
    binds
}

fn add_sums(
    tx: &Transaction<'_>,
    key: &SessionSpendKey,
    sums: &SumsKey,
    counts: Counts,
) -> Result<(), LedgerError> {
    tx.prepare_cached(&ADD_SUMS)?
        .execute(params_from_iter(counted_binds(key, sums, counts)))?;
    Ok(())
}

fn put_sums(
    tx: &Transaction<'_>,
    key: &SessionSpendKey,
    sums: &SumsKey,
    counts: Counts,
) -> Result<(), LedgerError> {
    tx.prepare_cached(&PUT_SUMS)?
        .execute(params_from_iter(counted_binds(key, sums, counts)))?;
    Ok(())
}

/// Takes `counts` back out of the row `sums` names, and deletes a row whose requests reach 0.
fn take_sums(
    tx: &Transaction<'_>,
    key: &SessionSpendKey,
    sums: &SumsKey,
    counts: Counts,
) -> Result<(), LedgerError> {
    tx.prepare_cached(&TAKE_SUMS)?
        .execute(params_from_iter(counted_binds(key, sums, counts)))?;
    tx.prepare_cached(&EMPTIED_SUMS)?
        .execute(params_from_iter(row_binds(key, sums)))?;
    Ok(())
}

fn read_sums(
    conn: &Connection,
    key: &SessionSpendKey,
    hour: i64,
) -> Result<BTreeMap<SumsKey, Counts>, LedgerError> {
    let mut binds = session_binds(key).to_vec();
    binds.push(Value::Integer(hour));
    let mut stmt = conn.prepare_cached(&READ_SUMS_OF_HOUR)?;
    let rows = stmt.query_map(params_from_iter(binds), |row| sums_row(row, 0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The sums row from column `first` on, in `SUMS_COLUMNS`' order.
fn sums_row(row: &rusqlite::Row, first: usize) -> rusqlite::Result<(SumsKey, Counts)> {
    let count =
        |i: usize| -> rusqlite::Result<u64> { Ok(row.get::<_, i64>(first + i)?.max(0) as u64) };
    Ok((
        SumsKey {
            agent_id: row.get(first)?,
            probe: row.get(first + 1)?,
            account_id: row.get(first + 2)?,
            canonical_model: row.get(first + 3)?,
            speed: row.get(first + 4)?,
            funding: row.get(first + 5)?,
            band: row.get(first + 6)?,
            tier: row.get(first + 7)?,
            client: row.get(first + 8)?,
            classifier: row.get(first + 9)?,
            request_class: row.get(first + 10)?,
            received_hour: row.get(first + 11)?,
            completed_hour: row.get(first + 12)?,
        },
        Counts {
            requests: count(13)?,
            input_tokens: count(14)?,
            cache_read_tokens: count(15)?,
            cache_write_5m_tokens: count(16)?,
            cache_write_1h_tokens: count(17)?,
            output_tokens: count(18)?,
        },
    ))
}

/// The facts from column `first` on, in `FACTS_COLUMNS`' order.
fn facts_of(row: &rusqlite::Row, first: usize) -> rusqlite::Result<Facts> {
    Ok(Facts {
        last_request_at: row.get(first)?,
        last_inference_at: row.get(first + 1)?,
        served_at: row.get(first + 2)?,
        served_id: row.get(first + 3)?,
        user_agent: row.get(first + 4)?,
        first_request_at: row.get(first + 5)?,
        newest_at: row.get(first + 6)?,
        newest_id: row.get(first + 7)?,
        newest_model: row.get(first + 8)?,
        newest_agent_type: row.get(first + 9)?,
        newest_account_id: row.get(first + 10)?,
        probe_at: row.get(first + 11)?,
        probe_id: row.get(first + 12)?,
        probe_model: row.get(first + 13)?,
    })
}

fn read_facts(
    conn: &Connection,
    key: &SessionSpendKey,
    agent: &str,
) -> Result<Option<Facts>, LedgerError> {
    let mut binds = session_binds(key).to_vec();
    binds.push(Value::Text(agent.to_string()));
    Ok(conn
        .prepare_cached(&READ_FACTS)?
        .query_row(params_from_iter(binds), |row| facts_of(row, 0))
        .optional()?)
}

fn read_all_facts(
    conn: &Connection,
    key: &SessionSpendKey,
) -> Result<Vec<(String, Facts)>, LedgerError> {
    let mut stmt = conn.prepare_cached(&READ_ALL_FACTS)?;
    let rows = stmt.query_map(params_from_iter(session_binds(key)), |row| {
        Ok((row.get(0)?, facts_of(row, 1)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn write_facts(
    tx: &Transaction<'_>,
    key: &SessionSpendKey,
    agent: &str,
    facts: &Facts,
) -> Result<(), LedgerError> {
    let mut binds = session_binds(key).to_vec();
    binds.extend([
        Value::Text(agent.to_string()),
        facts.last_request_at.into(),
        facts.last_inference_at.into(),
        facts.served_at.into(),
        facts.served_id.clone().into(),
        facts.user_agent.clone().into(),
        facts.first_request_at.into(),
        facts.newest_at.into(),
        facts.newest_id.clone().into(),
        facts.newest_model.clone().into(),
        facts.newest_agent_type.clone().into(),
        facts.newest_account_id.clone().into(),
        facts.probe_at.into(),
        facts.probe_id.clone().into(),
        facts.probe_model.clone().into(),
    ]);
    tx.prepare_cached(&PUT_FACTS)?
        .execute(params_from_iter(binds))?;
    Ok(())
}

/// Writes `totals` as session `key`'s rows of both tables, over whatever stood under its key, so a stray row never
/// adds to a sum.
fn write_totals(
    tx: &Transaction<'_>,
    key: &SessionSpendKey,
    totals: &Totals,
) -> Result<(), LedgerError> {
    clear_session(tx, key)?;
    for (sums, counts) in &totals.sums {
        put_sums(tx, key, sums, *counts)?;
    }
    for (agent, facts) in &totals.facts {
        write_facts(tx, key, agent, facts)?;
    }
    Ok(())
}

/// Deletes session `key`'s rows of both tables, its mark among them, and answers how many.
fn clear_session(tx: &Transaction<'_>, key: &SessionSpendKey) -> Result<usize, LedgerError> {
    let mut deleted = 0;
    for table in ["session_sums", "session_facts"] {
        deleted += tx
            .prepare_cached(&format!("DELETE FROM {table} WHERE {SESSION_KEY}"))?
            .execute(params_from_iter(session_binds(key)))?;
    }
    Ok(deleted)
}

fn marked(conn: &Connection, key: &SessionSpendKey) -> Result<bool, LedgerError> {
    let mut binds = session_binds(key).to_vec();
    binds.push(Value::Text(RESERVED_ID.to_string()));
    Ok(conn
        .prepare_cached(MARKED)?
        .query_row(params_from_iter(binds), |_| Ok(()))
        .optional()?
        .is_some())
}

/// Whether the session holds a request through `sql`, `HOLDS_ANY` or `HOLDS_ANOTHER` with the id `other`.
fn holds(
    conn: &Connection,
    sql: &str,
    key: &SessionSpendKey,
    other: Option<&str>,
) -> Result<bool, LedgerError> {
    let mut binds = session_binds(key).to_vec();
    binds.extend(other.map(|id| Value::Text(id.to_string())));
    Ok(conn
        .prepare_cached(sql)?
        .query_row(params_from_iter(binds), |_| Ok(()))
        .optional()?
        .is_some())
}

fn holds_another(conn: &Connection, key: &SessionSpendKey, id: &str) -> Result<bool, LedgerError> {
    holds(conn, HOLDS_ANOTHER, key, Some(id))
}

fn landed_since(conn: &Connection, key: &SessionSpendKey, stamp: i64) -> Result<bool, LedgerError> {
    let mut binds = session_binds(key).to_vec();
    binds.push(Value::Integer(stamp));
    Ok(conn
        .prepare_cached(LANDED_SINCE)?
        .query_row(params_from_iter(binds), |_| Ok(()))
        .optional()?
        .is_some())
}

/// Each pick of facts row `agent` of session `key` that `stale` names, among those the row holds a value of, read again
/// from the request rows that remain, the session's own row under `RESERVED_ID`. A pick that holds no value takes no
/// walk of the session. An agent's row left with no pick is deleted, and the answer counts it.
fn reread(
    tx: &Transaction<'_>,
    key: &SessionSpendKey,
    agent: &str,
    mut facts: Facts,
    stale: impl Fn(&Facts, Pick) -> bool,
) -> Result<usize, LedgerError> {
    let picks: Vec<Pick> = Pick::ALL
        .into_iter()
        .filter(|pick| facts.instant(*pick).is_some() && stale(&facts, *pick))
        .collect();
    if picks.is_empty() {
        return Ok(0);
    }
    let session = session_binds(key).to_vec();
    let mut agent_binds = session.clone();
    agent_binds.push(Value::Text(agent.to_string()));
    let one = |sql: &str, binds: &[Value]| -> Result<Option<i64>, LedgerError> {
        Ok(tx
            .prepare_cached(sql)?
            .query_row(params_from_iter(binds), |r| r.get(0))
            .optional()?)
    };
    for pick in picks {
        match pick {
            Pick::LastRequest => facts.last_request_at = one(SESSION_NEWEST, &session)?,
            Pick::LastInference => {
                facts.last_inference_at = one(SESSION_NEWEST_INFERENCE, &session)?
            }
            Pick::Served => {
                let served: Option<(i64, String, Option<String>)> = tx
                    .prepare_cached(SESSION_SERVED)?
                    .query_row(params_from_iter(&session), |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                    })
                    .optional()?;
                facts.served_at = served.as_ref().map(|s| s.0);
                facts.served_id = served.as_ref().map(|s| s.1.clone());
                facts.user_agent = served.and_then(|s| s.2);
            }
            Pick::FirstRequest => facts.first_request_at = one(AGENT_FIRST, &agent_binds)?,
            Pick::Newest => {
                type Newest = (i64, String, String, Option<String>, Option<String>);
                let newest: Option<Newest> = tx
                    .prepare_cached(&AGENT_NEWEST)?
                    .query_row(params_from_iter(&agent_binds), |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                    })
                    .optional()?;
                facts.newest_at = newest.as_ref().map(|n| n.0);
                facts.newest_id = newest.as_ref().map(|n| n.1.clone());
                facts.newest_model = newest.as_ref().map(|n| n.2.clone());
                facts.newest_agent_type = newest.as_ref().and_then(|n| n.3.clone());
                facts.newest_account_id = newest.and_then(|n| n.4);
            }
            Pick::Probe => {
                let probe: Option<(i64, String, String)> = tx
                    .prepare_cached(AGENT_PROBE)?
                    .query_row(params_from_iter(&agent_binds), |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                    })
                    .optional()?;
                facts.probe_at = probe.as_ref().map(|p| p.0);
                facts.probe_id = probe.as_ref().map(|p| p.1.clone());
                facts.probe_model = probe.map(|p| p.2);
            }
        }
    }
    if agent != RESERVED_ID && facts == Facts::default() {
        return Ok(tx
            .prepare_cached(&format!(
                "DELETE FROM session_facts WHERE {SESSION_KEY} AND agent_id = ?4"
            ))?
            .execute(params_from_iter(agent_binds))?);
    }
    write_facts(tx, key, agent, &facts)?;
    Ok(0)
}

/// The sessions `sql` answers under `binds`, each row's first three columns their key.
fn keys_of(
    conn: &Connection,
    sql: &str,
    binds: impl Params,
) -> Result<BTreeSet<SessionSpendKey>, LedgerError> {
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map(binds, |row| {
        Ok(SessionSpendKey {
            key_id: row.get(0)?,
            key_generation: row.get(1)?,
            session_id: row.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// One listed session's figures, priced at the read: its spend apart from its probes', its trailing day, its agents'
/// entries and the picks of its own facts row.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionFigures {
    /// The newest receipt of a client request and of one that is no token count, refused ones included.
    pub last_request_at: Option<i64>,
    pub last_inference_at: Option<i64>,
    /// The newest served client request's user agent, null included, `None` where the session holds no served client
    /// request.
    pub user_agent: Option<Option<String>>,
    pub spend: SpendFigures,
    pub probes: SpendFigures,
    pub day: DayFigures,
    /// One entry per agent that holds a served client request or a served probe, by agent id.
    pub agents: Vec<AgentFigures>,
}

/// A set of requests' count and money, with their billed dollars and plan shares by the account that served them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SpendFigures {
    pub requests: u64,
    pub money: SpendMoney,
    /// The billed dollars by account, a zero entry left out, so its sum is the sum of `money.billed_usd`.
    pub billed_usd_by_account: BTreeMap<String, f64>,
    /// The plan shares by account and window. An account is keyed on every plan-funded request that priced, so a key
    /// with no window still marks that a plan paid.
    pub shares: BTreeMap<String, BTreeMap<&'static str, f64>>,
}

/// The list dollars, the billed dollars by funding and the Pro-week figure over a set of requests.
#[derive(Clone, Debug, PartialEq)]
pub struct SpendMoney {
    pub usd: f64,
    /// By funding, in the order each funding's first dollars came in, a zero entry left out.
    pub billed_usd: Vec<(Funding, f64)>,
    /// `None` once a plan request the cost model priced nothing for leaves the sum unknown.
    pub equivalent: Option<f64>,
}

impl Default for SpendMoney {
    /// No request priced, which reads the Pro-week figure 0.
    fn default() -> SpendMoney {
        SpendMoney {
            usd: 0.0,
            billed_usd: Vec::new(),
            equivalent: Some(0.0),
        }
    }
}

/// The session's client requests received in the hours `trailing_day_hours` names, by the class word the sums rows
/// hold (`NO_CLASS` for a request with none), and their money.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DayFigures {
    pub requests: BTreeMap<String, u64>,
    pub money: SpendMoney,
}

/// One agent's requests of the session over every model it ran.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentFigures {
    pub agent_id: String,
    /// The canonical model of the agent's newest served client request, else of its newest served probe.
    pub model: String,
    pub agent_type: Option<String>,
    pub account_id: Option<String>,
    pub last_request_at: Option<i64>,
    pub first_request_at: Option<i64>,
    pub spend: SpendFigures,
    pub probes: SpendFigures,
}

impl SpendMoney {
    /// Answers whether the dollars were filed as billed, so the map by account reads this one condition.
    fn add(&mut self, price: &RequestPrice) -> bool {
        self.usd += price.usd;
        if price.funding == Funding::Plan {
            self.equivalent = add_figures(self.equivalent, price.equivalent);
            return false;
        }
        if price.usd <= 0.0 {
            return false;
        }
        match self
            .billed_usd
            .iter_mut()
            .find(|(funding, _)| *funding == price.funding)
        {
            Some((_, usd)) => *usd += price.usd,
            None => self.billed_usd.push((price.funding, price.usd)),
        }
        true
    }
}

impl SpendFigures {
    /// `requests` requests on `account_id` priced together at `price`, or pricing nothing.
    fn add(&mut self, requests: u64, account_id: &str, price: Option<&RequestPrice>) {
        self.requests += requests;
        let Some(price) = price else {
            return;
        };
        if self.money.add(price) {
            *self
                .billed_usd_by_account
                .entry(account_id.to_string())
                .or_default() += price.usd;
        }
        if price.funding != Funding::Plan {
            return;
        }
        let windows = self.shares.entry(account_id.to_string()).or_default();
        for (window, share) in price.shares.iter().flatten() {
            if let Some(share) = share {
                *windows.entry(*window).or_default() += share;
            }
        }
    }
}

/// The figures of each of `sessions` on `conn`'s snapshot, priced at `now` through `prices` and `tables`: one seek of
/// each table's key per session, and for a session with no mark the fill's own sum, with nothing written, so a session
/// reads the same figures before, during and after the fill.
pub fn session_figures(
    conn: &Connection,
    sessions: &[SessionSpendKey],
    prices: &PriceRevisions,
    tables: &PlanTables,
    now: i64,
) -> Result<BTreeMap<SessionSpendKey, SessionFigures>, LedgerError> {
    let mut sums: BTreeMap<SessionSpendKey, BTreeMap<SumsKey, Counts>> = BTreeMap::new();
    let mut facts: BTreeMap<SessionSpendKey, BTreeMap<String, Facts>> = BTreeMap::new();
    for chunk in sessions.chunks(SESSION_IDENTITY_CHUNK) {
        let (sql, binds) = session_sums_statement(chunk);
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(binds), |row| {
            Ok((session_key_of(row)?, sums_row(row, 3)?))
        })?;
        for row in rows {
            let (key, (sums_key, counts)) = row?;
            sums.entry(key).or_default().insert(sums_key, counts);
        }
        let (sql, binds) = session_facts_statement(chunk);
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(binds), |row| {
            Ok((
                session_key_of(row)?,
                row.get::<_, String>(3)?,
                facts_of(row, 4)?,
            ))
        })?;
        for row in rows {
            let (key, agent, row) = row?;
            facts.entry(key).or_default().insert(agent, row);
        }
    }
    let mut out = BTreeMap::new();
    for key in sessions {
        let totals = match facts.remove(key) {
            Some(facts) if facts.contains_key(RESERVED_ID) => Totals {
                sums: sums.remove(key).unwrap_or_default(),
                facts,
            },
            _ => totals_of(conn, key, prices)?,
        };
        let figures = priced(conn, key, &totals, prices, tables, now)?;
        out.insert(key.clone(), figures);
    }
    Ok(out)
}

/// The figure a sums row adds to: the session's spend or its probes', its agent's, or the session's day.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Target {
    Session { probe: bool },
    Agent { agent_id: String, probe: bool },
    Day,
}

/// The targets a row of `sums` adds to, the day where the row is a client one received inside the day's hours. The
/// day's money keys no account, so it names the reserved one.
fn targets(sums: &SumsKey, in_day: bool) -> Vec<(Target, &str)> {
    let mut targets = vec![(
        Target::Session { probe: sums.probe },
        sums.account_id.as_str(),
    )];
    if sums.agent_id != RESERVED_ID {
        targets.push((
            Target::Agent {
                agent_id: sums.agent_id.clone(),
                probe: sums.probe,
            },
            sums.account_id.as_str(),
        ));
    }
    if !sums.probe && in_day {
        targets.push((Target::Day, RESERVED_ID));
    }
    targets
}

/// The parts of a sums key a price reads, so the rows that hold the same ones add their counts and price once.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct PriceParts {
    canonical_model: String,
    speed: String,
    funding: String,
    band: String,
    tier: String,
    client: String,
    classifier: String,
    received_hour: i64,
    completed_hour: i64,
}

impl PriceParts {
    fn of(sums: &SumsKey) -> PriceParts {
        PriceParts {
            canonical_model: sums.canonical_model.clone(),
            speed: sums.speed.clone(),
            funding: sums.funding.clone(),
            band: sums.band.clone(),
            tier: sums.tier.clone(),
            client: sums.client.clone(),
            classifier: sums.classifier.clone(),
            received_hour: sums.received_hour,
            completed_hour: sums.completed_hour,
        }
    }

    /// The price key at the instant `at` that selects both entries and the instant `band_at` that selects the peak
    /// band, the stored words read back as the hourly rows' are.
    fn key(&self, at: i64, band_at: i64, now: i64) -> RowKey<'_> {
        RowKey {
            model: &self.canonical_model,
            speed: Speed::of(Some(self.speed.as_str())),
            band: Band::of_rollup(&self.band),
            funding: Funding::of_rollup(&self.funding),
            tier: (self.tier != UNATTRIBUTED).then_some(self.tier.as_str()),
            entrypoint: entrypoint_of_client(&self.client),
            classifier: is_free_classifier(Some(self.classifier.as_str())),
            at,
            band_at: Some(band_at),
            now,
        }
    }
}

fn usage_of(counts: Counts) -> PinUsage {
    PinUsage {
        input: counts.input_tokens,
        cache_read: counts.cache_read_tokens,
        write_5m: counts.cache_write_5m_tokens,
        write_1h: counts.cache_write_1h_tokens,
        output: counts.output_tokens,
    }
}

/// Whether a sums row prices from its request rows: an instant of the price list or of the cost list lies inside its
/// receipt hour, after the hour's first millisecond, or an edge of the peak band lies inside its completion hour.
fn boundary_row(prices: &PriceRevisions, tables: &PlanTables, sums: &SumsKey) -> bool {
    !boundary_instants(prices, Some(&tables.revisions), sums.received_hour).is_empty()
        || band_edge_inside(tables, sums.received_hour, sums.completed_hour)
}

/// Whether the peak band of the cost revision in force at the receipt hour opens or closes inside the completion hour:
/// its state at the hour's first millisecond differs from its state at its last.
fn band_edge_inside(tables: &PlanTables, received_hour: i64, completed_hour: i64) -> bool {
    let peak = &tables.revisions.in_force(received_hour * 1_000).peak;
    peak.covers(completed_hour * 1_000) != peak.covers(last_millisecond(completed_hour))
}

/// The figures of one session's totals. A row that is no boundary row adds its counts to its targets by account and
/// price parts, and each sum prices once at the row's receipt hour and completion hour. A receipt hour that holds a
/// boundary row reads the session's request rows of that hour once, however many boundary rows it holds, and prices
/// each request whose own two hours make a boundary row by its own receipt and completion. A request of that hour
/// whose own two hours make no boundary row is passed over, since its row prices from its sums.
fn priced(
    conn: &Connection,
    key: &SessionSpendKey,
    totals: &Totals,
    prices: &PriceRevisions,
    tables: &PlanTables,
    now: i64,
) -> Result<SessionFigures, LedgerError> {
    let (day_from, day_to) = trailing_day_hours(now);
    let in_day = |hour: i64| (day_from..day_to).contains(&hour);
    let mut accrued = Accrued::default();
    let mut grouped: BTreeMap<(Target, String, PriceParts), Counts> = BTreeMap::new();
    let mut split: BTreeSet<i64> = BTreeSet::new();
    for (sums, counts) in &totals.sums {
        if boundary_row(prices, tables, sums) {
            split.insert(sums.received_hour);
            continue;
        }
        let day = in_day(sums.received_hour);
        accrued.count_day(sums, counts.requests, day);
        for (target, account_id) in targets(sums, day) {
            grouped
                .entry((target, account_id.to_string(), PriceParts::of(sums)))
                .or_default()
                .add(*counts);
        }
    }
    for ((target, account_id, parts), counts) in &grouped {
        let key = parts.key(
            parts.received_hour * 1_000,
            parts.completed_hour * 1_000,
            now,
        );
        let price = price_request(prices, tables, key, usage_of(*counts));
        accrued.add(target, counts.requests, account_id, price.as_ref());
    }
    for hour in &split {
        let (sql, binds) = session_hour_statement(key, *hour);
        for request in session_requests_of(conn, &sql, binds)? {
            let Some((sums, counts)) = sums_of(&request, prices) else {
                continue;
            };
            if !boundary_row(prices, tables, &sums) {
                continue;
            }
            let completed = request
                .serving
                .as_ref()
                .map_or(request.received_at, |s| s.completed_at);
            let parts = PriceParts::of(&sums);
            let price = price_request(
                prices,
                tables,
                parts.key(request.received_at, completed, now),
                usage_of(counts),
            );
            let day = in_day(sums.received_hour);
            accrued.count_day(&sums, 1, day);
            for (target, account_id) in targets(&sums, day) {
                accrued.add(&target, 1, account_id, price.as_ref());
            }
        }
    }
    Ok(accrued.answer(&totals.facts))
}

/// The figures one session's rows add up to, before its facts name the agents' entries.
#[derive(Default)]
struct Accrued {
    spend: SpendFigures,
    probes: SpendFigures,
    /// By agent id, its client requests' figures and its probes'.
    agents: BTreeMap<String, (SpendFigures, SpendFigures)>,
    day: DayFigures,
}

impl Accrued {
    fn add(
        &mut self,
        target: &Target,
        requests: u64,
        account_id: &str,
        price: Option<&RequestPrice>,
    ) {
        let figures = match target {
            Target::Session { probe: false } => &mut self.spend,
            Target::Session { probe: true } => &mut self.probes,
            Target::Agent { agent_id, probe } => {
                let (spend, probes) = self.agents.entry(agent_id.clone()).or_default();
                if *probe {
                    probes
                } else {
                    spend
                }
            }
            Target::Day => {
                if let Some(price) = price {
                    self.day.money.add(price);
                }
                return;
            }
        };
        figures.add(requests, account_id, price);
    }

    /// The day's count of a client row by its class, priced or not.
    fn count_day(&mut self, sums: &SumsKey, requests: u64, in_day: bool) {
        if !sums.probe && in_day {
            *self
                .day
                .requests
                .entry(sums.request_class.clone())
                .or_default() += requests;
        }
    }

    fn answer(mut self, facts: &BTreeMap<String, Facts>) -> SessionFigures {
        let own = facts.get(RESERVED_ID).cloned().unwrap_or_default();
        let agents = facts
            .iter()
            .filter(|(agent_id, row)| {
                agent_id.as_str() != RESERVED_ID
                    && (row.newest_id.is_some() || row.probe_id.is_some())
            })
            .map(|(agent_id, row)| {
                let (spend, probes) = self.agents.remove(agent_id).unwrap_or_default();
                AgentFigures {
                    agent_id: agent_id.clone(),
                    model: row
                        .newest_model
                        .clone()
                        .or_else(|| row.probe_model.clone())
                        .unwrap_or_default(),
                    agent_type: row.newest_agent_type.clone(),
                    account_id: row.newest_account_id.clone(),
                    last_request_at: row.newest_at,
                    first_request_at: row.first_request_at,
                    spend,
                    probes,
                }
            })
            .collect();
        SessionFigures {
            last_request_at: own.last_request_at,
            last_inference_at: own.last_inference_at,
            user_agent: own.served_id.is_some().then_some(own.user_agent),
            spend: self.spend,
            probes: self.probes,
            day: self.day,
            agents,
        }
    }
}

/// The session's key from a row's first three columns.
fn session_key_of(row: &rusqlite::Row) -> rusqlite::Result<SessionSpendKey> {
    Ok(SessionSpendKey {
        key_id: row.get(0)?,
        key_generation: row.get(1)?,
        session_id: row.get(2)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_tables::JANUARY_2026;

    /// A price list whose Haiku 5.5 row opens its long-context cells past `over` prompt tokens.
    fn haiku_over(over: u64) -> PriceRevisions {
        PriceRevisions::from_yaml(
            &format!(
                "revisions:\n  - effective: 2026-01-01\n    models:\n      claude-haiku-5-5:\n        input: 1\n        \
                 cache_write_5m: 1.25\n        cache_write_1h: 2\n        cache_read: 0.1\n        output: 5\n        \
                 long_context: {{over: {over}, input: 5, cache_write_5m: 6.25, cache_write_1h: 10, cache_read: 0.5, \
                 output: 25}}\n"
            ),
            "test",
        )
        .expect("the list parses")
    }

    /// A start whose moved threshold clears a session above the cursor of a fill that a shutdown cut short has that
    /// session summed again: the first start's fill makes one call, which marks `new` and stops with `old` below its
    /// cursor, and the next start's step clears `new` before its fill runs to the end.
    #[test]
    fn a_moved_threshold_drops_the_cursor_of_a_fill_cut_short_and_the_fill_marks_the_session_it_cleared(
    ) {
        let db = LedgerDb::open_in_memory().unwrap();
        db.write(|c| {
            c.execute_batch(&format!(
                "INSERT INTO requests (id, received_at, model, canonical_model, stream, kind, origin, outcome, \
                 attempts, key_id, session_id) VALUES \
                 ('r-old', {at}, 'claude-haiku-5-5', 'claude-haiku-5-5', 0, 'inference', 'client', 'succeeded', 1, \
                  'desktop', 'old'), \
                 ('r-new', {later}, 'claude-haiku-5-5', 'claude-haiku-5-5', 0, 'inference', 'client', 'succeeded', \
                  1, 'desktop', 'new'); \
                 INSERT INTO attempts (request_id, attempt, account_id, account_kind, started_at, outcome, \
                 input_tokens) VALUES ('r-old', 1, 'a', 'oauth', {at}, 'succeeded', 100), \
                 ('r-new', 1, 'a', 'oauth', {later}, 'succeeded', 100);",
                at = JANUARY_2026,
                later = JANUARY_2026 + 1_000,
            ))?;
            Ok(())
        })
        .unwrap();
        let first = haiku_over(1_000);
        unmark_rebanded_sessions(&db, &first).unwrap();
        fill_step(&db, &first).unwrap();
        let moved = haiku_over(2_000);
        unmark_rebanded_sessions(&db, &moved).unwrap();
        while fill_step(&db, &moved).unwrap() {}
        let new = SessionSpendKey {
            key_id: "desktop".into(),
            key_generation: 0,
            session_id: "new".into(),
        };
        assert!(
            db.read_blocking(|c| marked(c, &new)).unwrap(),
            "the fill marks the session the step cleared"
        );
    }

    /// Writes request `id` of session `session`, received at `at`, with its one served attempt straight into the
    /// tables, so no landing marks the session.
    fn write_raw(db: &LedgerDb, id: &str, session: &str, at: i64) {
        db.write(|c| {
            c.execute_batch(&format!(
                "INSERT INTO requests (id, received_at, model, canonical_model, stream, kind, origin, outcome, \
                 attempts, key_id, session_id) VALUES ('{id}', {at}, 'claude-haiku-5-5', 'claude-haiku-5-5', 0, \
                 'inference', 'client', 'succeeded', 1, 'desktop', '{session}'); \
                 INSERT INTO attempts (request_id, attempt, account_id, account_kind, started_at, outcome, \
                 input_tokens) VALUES ('{id}', 1, 'a', 'oauth', {at}, 'succeeded', 100);"
            ))?;
            Ok(())
        })
        .unwrap();
    }

    /// A retention pass under a request retention of one day, run a day and 30 minutes past `JANUARY_2026`, so it
    /// takes each request received before `JANUARY_2026` plus 30 minutes.
    fn purge_the_first_half_hour(db: &LedgerDb, prices: &PriceRevisions) {
        let config = crate::LedgerConfig {
            retention_days: 1,
            ..Default::default()
        };
        let now = JANUARY_2026 + phoebis_routing::DAY_MS + 30 * 60_000;
        crate::bodies::run_retention_blocking(db, &config, now, &[], prices).unwrap();
    }

    /// A purge pass that takes the older request of a session that resumed above the cursor of a fill under way has
    /// the fill walk from the top and sum that session: the first call marks `other` and stops with `s`'s older
    /// request below its cursor, `s`'s new request stands above the cursor with no mark, as a landing leaves a session
    /// that holds another request, and the pass takes the older request before the fill reaches it.
    #[test]
    fn a_purge_pass_drops_the_cursor_of_a_fill_under_way_and_the_fill_marks_the_session_it_cut() {
        let dir = tempfile::tempdir().unwrap();
        let db = LedgerDb::open(&dir.path().join("ledger.sqlite")).unwrap();
        let prices = haiku_over(1_000);
        write_raw(&db, "r-old", "s", JANUARY_2026);
        write_raw(&db, "r-other", "other", JANUARY_2026 + HOUR_MS);
        fill_step(&db, &prices).unwrap();
        write_raw(&db, "r-new", "s", JANUARY_2026 + 2 * HOUR_MS);
        purge_the_first_half_hour(&db, &prices);
        while fill_step(&db, &prices).unwrap() {}
        let s = SessionSpendKey {
            key_id: "desktop".into(),
            key_generation: 0,
            session_id: "s".into(),
        };
        assert!(
            db.read_blocking(|c| marked(c, &s)).unwrap(),
            "the fill marks the session the pass cut"
        );
    }

    /// A call behind a fill that ended walks no request row, across a purge pass that took a request: the fill marks
    /// `old` and ends, the pass takes `old`'s request, and `new` stands above the cursor with no mark, a state no path
    /// writes, since a landing marks a new session.
    #[test]
    fn a_call_behind_a_fill_that_ended_walks_no_row_and_a_purge_pass_keeps_its_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let db = LedgerDb::open(&dir.path().join("ledger.sqlite")).unwrap();
        let prices = haiku_over(1_000);
        write_raw(&db, "r-old", "old", JANUARY_2026);
        while fill_step(&db, &prices).unwrap() {}
        purge_the_first_half_hour(&db, &prices);
        write_raw(&db, "r-new", "new", JANUARY_2026 + 2 * HOUR_MS);
        let new = SessionSpendKey {
            key_id: "desktop".into(),
            key_generation: 0,
            session_id: "new".into(),
        };
        let left = fill_step(&db, &prices).unwrap();
        assert_eq!(
            (left, db.read_blocking(|c| marked(c, &new)).unwrap()),
            (false, false),
            "the call answers that no row is left and sums nothing"
        );
    }
}
