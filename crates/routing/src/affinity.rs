//! The affinity map: which conversation a boundary belongs to, and how warm the cache is there. Shared between
//! the router task and the relay's conversation climb the way `InFlight` is, so a lookup never waits on a command
//! round trip.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub use phoebis_store::{AffinityChain, AffinityRow};

/// One boundary as the map holds it; `AffinityRow` minus its key. The links are always present;
/// `ttl_ms` is 0 for a link the cache never confirmed, and such an entry is never `live`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AffinityEntry {
    pub chain: AffinityChain,
    pub account_id: String,
    pub incarnation: u64,
    pub written_at: i64,
    pub ttl_ms: i64,
    pub prefix_tokens: u64,
    pub key_id: String,
    pub key_generation: i64,
    pub root: String,
    /// The cache lane of the last report that wrote the entry: the placement reads an entry of another lane as
    /// no warm prefix, while the conversation climb reads across lanes.
    pub lane: String,
    /// The boundary's place in its chain, counted from 1, as the report that wrote the entry listed it, or 0 for an
    /// entry restored from a row stored before the registry kept it.
    pub depth: u32,
    /// The upstream id of the newest answer produced at this boundary on the account that holds the entry,
    /// written for a request that carried or received the cache diagnostics member. A static entry never
    /// carries one.
    pub message_id: Option<String>,
}

/// An entry the purge removed, with why: `ttl` for one past its key's idle TTL or whose key no pool serves,
/// `incarnation` for one whose account is gone or held under another incarnation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PurgedEntry {
    pub model: String,
    pub hash: String,
    pub entry: AffinityEntry,
    pub cause: &'static str,
}

impl AffinityEntry {
    /// Warm: the cache confirmed this boundary and its marker's lifetime has not run out.
    pub fn live(&self, now: i64) -> bool {
        self.ttl_ms > 0 && now - self.written_at <= self.ttl_ms
    }
}

/// One boundary the rung walk visited, deepest first: the gate's verdict beside the entry, so a record names each
/// member's deepest entry whether or not the rung took it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Visited<'a> {
    pub depth: u32,
    pub hash: &'a str,
    pub entry: &'a AffinityEntry,
    /// `None` for an entry the gate accepted, else the gate's name.
    pub refused_by: Option<&'static str>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RungWalk<'a> {
    /// The index into `visited` of the deepest accepted entry, what `deepest` answers, or `None` when every entry
    /// was refused or no boundary has one.
    pub matched: Option<usize>,
    pub visited: Vec<Visited<'a>>,
}

/// What a completed inference or probe answer reports: its conversation's links always, and the cache facts
/// per tier when the response carries any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AffinityReport {
    pub account_id: String,
    pub incarnation: u64,
    pub key_id: String,
    pub key_generation: i64,
    pub canonical_model: String,
    /// The writing request's lane.
    pub lane: String,
    /// The conversation the request belonged to, its session id, which the contract guarantees of every request.
    pub root: String,
    pub static_boundaries: Arc<[String]>,
    pub message_boundaries: Arc<[String]>,
    /// Parallel to `static_boundaries`: the estimated token size of the blocks each boundary covers, as the
    /// relay computed it at chain time (canonical bytes over four); 0 for the empty-tools boundary of a request
    /// without tools.
    pub static_sizes: Vec<u64>,
    /// The lifetime of the marker on each tier as sent upstream after the rewrite: the deepest marker inside
    /// `tools[]` or `system[]` for the static tier, the deepest inside `messages[]` for the message tier, a
    /// top-level member for both; `None` for a tier without a marker, which gets no warmth.
    pub static_ttl_ms: Option<i64>,
    pub message_ttl_ms: Option<i64>,
    /// The usage reported a cache read or a cache write: the evidence the warmth fields need.
    pub cache_touched: bool,
    /// `cache_read + write_5m + write_1h`: the prefix the upstream reported for this request, the promised size
    /// of a message entry.
    pub prefix_tokens: u64,
    pub now: i64,
    /// The response's cache-creation writes as its usage reported them: the split object fills both (the 1-hour
    /// member present, if zero), the older total-only shape the 5-minute one alone; `None` without usage.
    pub cache_write_5m_tokens: Option<u64>,
    pub cache_write_1h_tokens: Option<u64>,
    /// The TTL every `cache_control` marker of the dispatched body carries, when they all carry one; `None` for a
    /// body without a marker or with markers at both TTLs.
    pub uniform_ttl_ms: Option<i64>,
    /// The request whose answer this is, the decision that selected the account (`seq`) and the wire attempt that
    /// completed (`attempt`), so the rows it writes reach the selecting decision's row whichever pass answered.
    pub request_id: String,
    pub seq: u32,
    pub attempt: u32,
    /// The answer's upstream id, for the tail boundary alone. It is `None` for an answer whose request neither
    /// carried nor received the cache diagnostics member, for a probe, and for an answer that named no id.
    pub message_id: Option<String>,
}

pub struct ConversationQuery<'a> {
    pub canonical_model: &'a str,
    pub key_id: &'a str,
    pub key_generation: i64,
    pub message_boundaries: &'a [String],
    /// The current incarnation of an account id, `None` for one the registry no longer holds; the handle
    /// passes the snapshot's reading, a test a stub. An entry under another incarnation is stepped over.
    pub incarnation_of: &'a dyn Fn(&str) -> Option<u64>,
}

/// What the relay asks for the diagnostics member's value once the account is selected: the answer the request
/// continues, on that account alone.
pub struct PredecessorQuery<'a> {
    pub canonical_model: &'a str,
    /// The selected account, under its current incarnation as `incarnation_of` reads it.
    pub account_id: &'a str,
    pub key_id: &'a str,
    pub key_generation: i64,
    /// The roots an entry may carry: the request's own root and, for a fork, the conversation it forked from.
    pub roots: &'a [&'a str],
    pub message_boundaries: &'a [String],
    pub incarnation_of: &'a dyn Fn(&str) -> Option<u64>,
}

/// The deepest own-key match of a request's message boundaries: the matched conversation's root and the depth.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationMatch {
    pub root: String,
    pub depth: Option<u32>,
}

/// What the relay asks of the map: its conversation climb asks `conversation`, and the leg's body build asks
/// `predecessor` for the answer the request continues on the selected account. Implemented by the map and by the
/// handle.
pub trait ConversationLookup {
    fn conversation(&self, query: &ConversationQuery<'_>) -> Option<ConversationMatch>;
    /// The id of the deepest boundary of `query` that holds an answer on the selected account, under the
    /// request's key and generation and one of its roots. Warmth and the lane play no part.
    fn predecessor(&self, query: &PredecessorQuery<'_>) -> Option<String>;
}

/// A boundary under the model it was reported for: an entry's place in the map.
type EntryKey = (String, String);

fn row_of(model: &str, hash: &str, entry: &AffinityEntry) -> AffinityRow {
    AffinityRow {
        model: model.to_string(),
        hash: hash.to_string(),
        chain: entry.chain,
        account_id: entry.account_id.clone(),
        incarnation: entry.incarnation,
        written_at: entry.written_at,
        ttl_ms: entry.ttl_ms,
        prefix_tokens: entry.prefix_tokens,
        key_id: entry.key_id.clone(),
        key_generation: entry.key_generation,
        root: entry.root.clone(),
        lane: entry.lane.clone(),
        depth: entry.depth,
        message_id: entry.message_id.clone(),
    }
}

#[derive(Debug, Default)]
pub struct AffinityMap {
    /// The model, then the boundary: a lookup borrows both keys, so a selection walking a long conversation's
    /// boundaries allocates nothing per boundary. No inner map is left empty.
    entries: HashMap<String, HashMap<String, AffinityEntry>>,
    /// Keys refreshed in place since the last drain, not yet persisted.
    dirty: HashSet<EntryKey>,
    /// A folded spelling to the dated id it resolved to, as `RoutingState::canonical` keeps it, so a report or
    /// a lookup keys on the dated id once it is known, whichever spelling the caller names. An entry already
    /// written under the old spelling is not rekeyed; it ages out through the purge.
    resolutions: HashMap<String, String>,
}

impl AffinityMap {
    pub fn from_rows(rows: Vec<AffinityRow>) -> AffinityMap {
        let mut map = AffinityMap::default();
        for row in rows {
            map.entries.entry(row.model).or_default().insert(
                row.hash,
                AffinityEntry {
                    chain: row.chain,
                    account_id: row.account_id,
                    incarnation: row.incarnation,
                    written_at: row.written_at,
                    ttl_ms: row.ttl_ms,
                    prefix_tokens: row.prefix_tokens,
                    key_id: row.key_id,
                    key_generation: row.key_generation,
                    root: row.root,
                    lane: row.lane,
                    depth: row.depth,
                    message_id: row.message_id,
                },
            );
        }
        map
    }

    /// The name the map keys a model's rows on: the dated id `model` resolved to, else `model` itself, as
    /// `RoutingState::canonical` resolves it.
    fn canonical<'a>(&'a self, model: &'a str) -> &'a str {
        self.resolutions
            .get(model)
            .map(String::as_str)
            .unwrap_or(model)
    }

    /// Learns a folded spelling's dated id, fed at load and when a resolution lands. Never rekeys an entry
    /// already written under the old spelling; it ages out through the purge instead.
    pub fn learn_resolution(&mut self, spelling: &str, dated_id: &str) {
        self.resolutions
            .insert(spelling.to_string(), dated_id.to_string());
    }

    pub fn entry(&self, canonical_model: &str, hash: &str) -> Option<&AffinityEntry> {
        self.entries.get(self.canonical(canonical_model))?.get(hash)
    }

    /// The deepest boundary whose entry `accept` admits, any owner: (depth, entry). Walks from the last
    /// boundary to the first and steps over an entry `accept` refuses (expired, another incarnation, an
    /// account the caller cannot use) rather than stopping at it.
    pub fn deepest(
        &self,
        canonical_model: &str,
        boundaries: &[String],
        accept: impl Fn(&AffinityEntry) -> bool,
    ) -> Option<(u32, &AffinityEntry)> {
        boundaries
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, boundary)| {
                let entry = self.entry(canonical_model, boundary)?;
                accept(entry).then_some((index as u32 + 1, entry))
            })
    }

    /// Every boundary with an entry for the model, from the last boundary to the first, each judged by `gate`
    /// (`None` accepts, `Some(name)` refuses). Every entry is judged, not only those down to the match, so the
    /// record reads a member's standing at a depth the rung never reached.
    pub fn walk<'a>(
        &'a self,
        canonical_model: &str,
        boundaries: &'a [String],
        gate: impl Fn(&'a AffinityEntry) -> Option<&'static str>,
    ) -> RungWalk<'a> {
        let mut walk = RungWalk::default();
        for (index, boundary) in boundaries.iter().enumerate().rev() {
            let Some(entry) = self.entry(canonical_model, boundary) else {
                continue;
            };
            let refused_by = gate(entry);
            if refused_by.is_none() && walk.matched.is_none() {
                walk.matched = Some(walk.visited.len());
            }
            walk.visited.push(Visited {
                depth: index as u32 + 1,
                hash: boundary,
                entry,
                refused_by,
            });
        }
        walk
    }

    /// Upserts the report's boundaries; answers the rows to persist (new or moved).
    pub fn record(&mut self, report: &AffinityReport) -> Vec<AffinityRow> {
        let canonical_model = self.canonical(&report.canonical_model).to_string();
        let root = report.root.as_str();
        // The upstream's own TTL beats the marker's for the boundaries the response wrote, when every marker of the
        // request carried one TTL and the split says the cache wrote at the other: the markers are what the request
        // asked, the split is what the cache holds. A request with markers at both TTLs keeps each tier's marker,
        // since the split names no tier.
        let (five, hour) = (report.cache_write_5m_tokens, report.cache_write_1h_tokens);
        let written = match (five, hour) {
            // The split object always carries the 1-hour member, so its absence is the older total-only shape,
            // which names no TTL.
            (Some(five), Some(0)) if five > 0 => Some(300_000),
            (Some(0) | None, Some(hour)) if hour > 0 => Some(3_600_000),
            _ => None,
        };
        let written = written.filter(|w| report.uniform_ttl_ms.is_some_and(|t| t != *w));
        // The usage names no tier and the static sizes are the relay's estimate from the body's bytes, so only a
        // response that read nothing proves it wrote the static prefix: an estimate must never grant warmth. A static
        // prefix the response partly rewrote keeps its marker's lifetime, as one it only read does.
        let read = report
            .prefix_tokens
            .saturating_sub(five.unwrap_or(0) + hour.unwrap_or(0));
        let static_written = written.filter(|_| read == 0);
        let message_ttl_ms = report.message_ttl_ms.map(|t| written.unwrap_or(t));
        let static_ttl_ms = report.static_ttl_ms.map(|t| static_written.unwrap_or(t));
        let message_confirmed = report.cache_touched && message_ttl_ms.is_some();
        let message_ttl = message_ttl_ms.unwrap_or(0);
        let mut rows = Vec::new();

        let tail = report.message_boundaries.len();
        for (index, boundary) in report.message_boundaries.iter().enumerate() {
            // The answer continues the whole list, so only the tail names it: a prefix boundary keeps the answer
            // of the turn that ended there.
            let message_id = (index + 1 == tail)
                .then_some(report.message_id.as_deref())
                .flatten();
            if let Some(row) = self.upsert(
                &canonical_model,
                boundary,
                AffinityChain::Message,
                ordinal(index),
                report,
                root,
                message_confirmed,
                message_ttl,
                report.prefix_tokens,
                message_id,
            ) {
                rows.push(row);
            }
        }
        if report.cache_touched {
            let static_confirmed = static_ttl_ms.is_some();
            let static_ttl = static_ttl_ms.unwrap_or(0);
            for (index, boundary) in report.static_boundaries.iter().enumerate() {
                let size = report.static_sizes.get(index).copied().unwrap_or(0);
                if let Some(row) = self.upsert(
                    &canonical_model,
                    boundary,
                    AffinityChain::Static,
                    ordinal(index),
                    report,
                    root,
                    static_confirmed,
                    static_ttl,
                    size,
                    None,
                ) {
                    rows.push(row);
                }
            }
        }
        rows
    }

    /// Entries past `ttl_of(key_id)` (an idle TTL in ms; `None` is unserved), or whose account is gone or under
    /// another incarnation, leave. Answers each with its cause, the account's before the TTL's, since a replaced
    /// holder's entry would have left whatever its age.
    pub fn purge(
        &mut self,
        now: i64,
        ttl_of: impl Fn(&str) -> Option<i64>,
        incarnation_of: impl Fn(&str) -> Option<u64>,
    ) -> Vec<PurgedEntry> {
        let mut gone = Vec::new();
        for (model, hashes) in &mut self.entries {
            hashes.retain(|hash, entry| {
                let replaced = incarnation_of(&entry.account_id) != Some(entry.incarnation);
                let expired = match ttl_of(&entry.key_id) {
                    None => true,
                    Some(ttl) => now - entry.written_at > ttl,
                };
                let cause = if replaced {
                    "incarnation"
                } else if expired {
                    "ttl"
                } else {
                    return true;
                };
                gone.push(PurgedEntry {
                    model: model.clone(),
                    hash: hash.clone(),
                    entry: entry.clone(),
                    cause,
                });
                false
            });
        }
        self.entries.retain(|_, hashes| !hashes.is_empty());
        for purged in &gone {
            self.dirty
                .remove(&(purged.model.clone(), purged.hash.clone()));
        }
        gone
    }

    /// The refreshed rows not yet persisted, cleared.
    pub fn drain(&mut self) -> Vec<AffinityRow> {
        std::mem::take(&mut self.dirty)
            .into_iter()
            .filter_map(|(model, hash)| {
                let entry = self.entries.get(&model)?.get(&hash)?;
                Some(row_of(&model, &hash, entry))
            })
            .collect()
    }

    pub fn len(&self) -> usize {
        self.entries.values().map(HashMap::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.values().all(HashMap::is_empty)
    }

    /// An existing entry with the same account, key and generation is refreshed in place and its key marked
    /// dirty; any other case (new, or held by another key) replaces it and yields a row. `confirmed` decides
    /// the warmth: when false a refreshed entry on the report's lane keeps its warm deadline, its `ttl_ms`
    /// shrinking to the lifetime it has left, and one refreshed from another lane or a new one carries no warmth.
    /// `depth` is the boundary's place in the report's chain, which a refresh in place leaves as it was: a
    /// boundary's hash covers everything before it, so its place in the chain never changes.
    #[allow(clippy::too_many_arguments)]
    fn upsert(
        &mut self,
        model: &str,
        hash: &str,
        chain: AffinityChain,
        depth: u32,
        report: &AffinityReport,
        root: &str,
        confirmed: bool,
        ttl_ms: i64,
        prefix_tokens: u64,
        message_id: Option<&str>,
    ) -> Option<AffinityRow> {
        let existing = self
            .entries
            .get_mut(model)
            .and_then(|hashes| hashes.get_mut(hash));
        let inherited_root = match existing {
            Some(entry)
                if entry.account_id == report.account_id
                    && entry.key_id == report.key_id
                    && entry.key_generation == report.key_generation =>
            {
                if confirmed {
                    entry.written_at = report.now;
                    entry.ttl_ms = ttl_ms;
                    entry.prefix_tokens = prefix_tokens;
                } else if !crate::select::same_lane(&entry.lane, &report.lane) {
                    entry.ttl_ms = 0;
                    entry.written_at = report.now;
                } else {
                    // Keeps the old warm deadline (written_at + ttl_ms) fixed: a link-only refresh must not revive
                    // a lapsed boundary by moving written_at forward while leaving the old full ttl_ms in place.
                    entry.ttl_ms = (entry.written_at + entry.ttl_ms - report.now).max(0);
                    entry.written_at = report.now;
                }
                // An answer of the account's earlier incarnation is no answer of the holder the lookup selects, so
                // the id leaves before the refresh carries the entry over.
                if entry.incarnation != report.incarnation {
                    entry.message_id = None;
                }
                // The tail's id is written under the conversation's own root alone: another conversation sending
                // the same list (two sessions opening with one template message) keeps this one's answer findable
                // and falls back to its own memory.
                if let Some(id) = message_id.filter(|_| entry.root == root) {
                    entry.message_id = Some(id.to_string());
                }
                // The root stays the one the boundary was first recorded under: a later writer sharing it (a fork's
                // own turns landing on the same account) refreshes the links and the warmth, never re-roots the
                // conversation that got here first.
                entry.incarnation = report.incarnation;
                // One entry holds one lane: a conversation that switched lanes forgets the earlier lane's cache here,
                // which a lane change reads as cold anyway.
                entry.lane = report.lane.clone();
                self.dirty.insert((model.to_string(), hash.to_string()));
                return None;
            }
            // A writer of the same key and generation keeps the root the boundary was first recorded under, even
            // from another account (a fork placed elsewhere, a migrated base): only another key or generation, a
            // different tenant, replaces it.
            Some(entry) => (entry.key_id == report.key_id
                && entry.key_generation == report.key_generation)
                .then(|| entry.root.clone()),
            None => None,
        };
        let entry_root = inherited_root.unwrap_or_else(|| root.to_string());
        let entry = AffinityEntry {
            chain,
            account_id: report.account_id.clone(),
            incarnation: report.incarnation,
            written_at: report.now,
            ttl_ms: if confirmed { ttl_ms } else { 0 },
            prefix_tokens: if confirmed { prefix_tokens } else { 0 },
            key_id: report.key_id.clone(),
            key_generation: report.key_generation,
            message_id: message_id
                .filter(|_| entry_root == root)
                .map(str::to_string),
            root: entry_root,
            lane: report.lane.clone(),
            depth,
        };
        let row = row_of(model, hash, &entry);
        let key: EntryKey = (model.to_string(), hash.to_string());
        self.dirty.remove(&key);
        self.entries.entry(key.0).or_default().insert(key.1, entry);
        Some(row)
    }
}

/// A chain index as the boundary's place counted from 1.
fn ordinal(index: usize) -> u32 {
    u32::try_from(index + 1).unwrap_or(u32::MAX)
}

impl ConversationLookup for AffinityMap {
    /// The deepest own-key match of the request's message boundaries, which the relay's fork detection reads: the
    /// deepest message boundary the key and generation own. Warmth plays no part.
    fn conversation(&self, query: &ConversationQuery<'_>) -> Option<ConversationMatch> {
        let owns = |entry: &AffinityEntry| -> bool {
            entry.key_id == query.key_id
                && entry.key_generation == query.key_generation
                && (query.incarnation_of)(&entry.account_id) == Some(entry.incarnation)
        };
        let (depth, entry) = self.deepest(query.canonical_model, query.message_boundaries, owns)?;
        Some(ConversationMatch {
            root: entry.root.clone(),
            depth: Some(depth),
        })
    }

    /// The deepest boundary that holds an answer on the selected account, under the request's key and generation
    /// and one of its roots. An entry that fails a test is stepped over, as `conversation` steps over one the key
    /// does not own. Warmth plays no part: an expired entry still names the answer the request continues.
    fn predecessor(&self, query: &PredecessorQuery<'_>) -> Option<String> {
        let names_an_answer = |entry: &AffinityEntry| -> bool {
            entry.message_id.is_some()
                && entry.account_id == query.account_id
                && (query.incarnation_of)(&entry.account_id) == Some(entry.incarnation)
                && entry.key_id == query.key_id
                && entry.key_generation == query.key_generation
                && query.roots.contains(&entry.root.as_str())
        };
        self.deepest(
            query.canonical_model,
            query.message_boundaries,
            names_an_answer,
        )
        .and_then(|(_, entry)| entry.message_id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(key_id: &str, account_id: &str, now: i64) -> AffinityReport {
        AffinityReport {
            account_id: account_id.into(),
            incarnation: 1,
            key_id: key_id.into(),
            key_generation: 0,
            canonical_model: "m".into(),
            lane: crate::lane_key("m", "standard", "none", "-"),
            root: "s".into(),
            static_boundaries: Arc::from([]),
            message_boundaries: Arc::from(["b1".to_string(), "b2".to_string()]),
            static_sizes: Vec::new(),
            static_ttl_ms: None,
            message_ttl_ms: None,
            cache_touched: false,
            prefix_tokens: 0,
            now,
            cache_write_5m_tokens: None,
            cache_write_1h_tokens: None,
            uniform_ttl_ms: None,
            request_id: "r-1".into(),
            seq: 1,
            attempt: 1,
            message_id: None,
        }
    }

    fn incarnation_of(_: &str) -> Option<u64> {
        Some(1)
    }

    #[test]
    fn a_report_records_every_boundary_and_answers_rows_for_the_new_ones_only() {
        let mut map = AffinityMap::default();
        let rows = map.record(&report("k", "a", 1_000));
        assert_eq!(rows.len(), 2, "the two message boundaries");
        assert_eq!(map.len(), 2);
        let mut second = report("k", "a", 2_000);
        second.message_boundaries =
            Arc::from(["b1".to_string(), "b2".to_string(), "b3".to_string()]);
        let more_rows = map.record(&second);
        assert_eq!(
            more_rows.len(),
            1,
            "the two boundaries refresh in place, and only b3 is new"
        );
    }

    #[test]
    fn a_second_report_on_the_same_account_refreshes_without_rows_and_the_drain_answers_the_refreshed_rows_once(
    ) {
        let mut map = AffinityMap::default();
        map.record(&report("k", "a", 1_000));
        let rows = map.record(&report("k", "a", 2_000));
        assert!(rows.is_empty(), "the same holder refreshes in place");
        let drained = map.drain();
        assert_eq!(drained.len(), 2, "every refreshed entry, once");
        assert!(
            map.drain().is_empty(),
            "drained a second time answers nothing"
        );
    }

    #[test]
    fn an_entry_another_key_wrote_answers_deepest_but_not_conversation() {
        let mut map = AffinityMap::default();
        map.record(&report("owner", "a", 1_000));
        let boundaries = vec!["b1".to_string(), "b2".to_string()];
        assert!(map.deepest("m", &boundaries, |_| true).is_some());
        let query = ConversationQuery {
            canonical_model: "m",
            key_id: "someone_else",
            key_generation: 0,
            message_boundaries: &boundaries,
            incarnation_of: &incarnation_of,
        };
        assert_eq!(map.conversation(&query), None);
    }

    #[test]
    fn a_report_under_another_key_takes_the_entry_over_and_answers_a_row() {
        let mut map = AffinityMap::default();
        map.record(&report("first", "a", 1_000));
        let rows = map.record(&report("second", "b", 2_000));
        assert_eq!(rows.len(), 2, "every boundary moves to the new holder");
        assert_eq!(map.entry("m", "b1").unwrap().key_id, "second");
    }

    #[test]
    fn a_report_with_a_one_hour_static_tier_and_a_five_minute_message_tier_records_the_two_lifetimes_on_the_two_chains(
    ) {
        let mut map = AffinityMap::default();
        let mut r = report("k", "a", 1_000);
        r.static_boundaries = Arc::from(["s1".to_string()]);
        r.static_sizes = vec![10];
        r.cache_touched = true;
        r.static_ttl_ms = Some(3_600_000);
        r.message_ttl_ms = Some(300_000);
        map.record(&r);
        assert_eq!(map.entry("m", "s1").unwrap().ttl_ms, 3_600_000);
        assert_eq!(map.entry("m", "b2").unwrap().ttl_ms, 300_000);
    }

    #[test]
    fn a_report_without_cache_evidence_records_the_links_and_no_warmth() {
        let mut map = AffinityMap::default();
        let mut r = report("k", "a", 1_000);
        r.static_boundaries = Arc::from(["s1".to_string()]);
        r.static_sizes = vec![10];
        r.static_ttl_ms = Some(3_600_000);
        r.message_ttl_ms = Some(300_000);
        r.cache_touched = false;
        map.record(&r);
        let entry = map
            .entry("m", "b2")
            .expect("the message boundary was recorded");
        assert!(!entry.live(1_000), "no warmth was ever confirmed");
        assert_eq!(entry.ttl_ms, 0, "no warmth without cache evidence");
        assert_eq!(
            map.entry("m", "s1"),
            None,
            "no static entry exists without cache evidence"
        );
    }

    #[test]
    fn deepest_steps_over_an_entry_the_predicate_refuses_and_answers_the_next_shallower_one() {
        let mut map = AffinityMap::default();
        for (index, boundary) in ["b1", "b2", "b3", "b4"].iter().enumerate() {
            map.record(&AffinityReport {
                message_boundaries: Arc::from([(*boundary).to_string()]),
                ..report(&format!("k{index}"), &format!("a{index}"), 1_000)
            });
        }
        let boundaries: Vec<String> = ["b1", "b2", "b3", "b4"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // A fourth boundary the predicate refuses by account (a3) above a live third (a2): the walk steps over it.
        let (depth, _) = map
            .deepest("m", &boundaries, |e| e.account_id != "a3")
            .expect("the third boundary is accepted");
        assert_eq!(depth, 3);
    }

    #[test]
    fn the_walk_judges_every_entry_and_matches_the_deepest_accepted() {
        let mut map = AffinityMap::default();
        for (boundary, account) in [("b1", "a1"), ("b2", "a1"), ("b3", "a2")] {
            map.record(&AffinityReport {
                message_boundaries: Arc::from([boundary.to_string()]),
                ..report("k", account, 1_000)
            });
        }
        let boundaries: Vec<String> = ["b1", "b2", "b3"].iter().map(|s| s.to_string()).collect();
        let gate = |e: &AffinityEntry| (e.account_id == "a2").then_some("expired");
        let walk = map.walk("m", &boundaries, gate);
        assert_eq!(
            walk.visited
                .iter()
                .map(|v| (v.depth, v.hash, v.entry.account_id.as_str(), v.refused_by))
                .collect::<Vec<_>>(),
            vec![
                (3, "b3", "a2", Some("expired")),
                (2, "b2", "a1", None),
                (1, "b1", "a1", None),
            ],
            "every entry is judged, deepest first, past the match as well"
        );
        assert_eq!(walk.matched, Some(1));
        assert_eq!(
            map.deepest("m", &boundaries, |e| gate(e).is_none())
                .map(|(depth, _)| depth),
            Some(walk.visited[1].depth),
            "the match is the entry `deepest` answers under the same gate"
        );
    }

    #[test]
    fn the_walk_of_a_chain_with_no_entry_is_empty() {
        let mut map = AffinityMap::default();
        map.record(&report("k", "a1", 1_000));
        let boundaries: Vec<String> = ["x1", "x2"].iter().map(|s| s.to_string()).collect();
        assert_eq!(map.walk("m", &boundaries, |_| None), RungWalk::default());
    }

    #[test]
    fn the_lookup_steps_over_an_entry_under_another_incarnation() {
        let mut map = AffinityMap::default();
        // b1 is still owned by a1's recorded incarnation; b2's account moved on to incarnation 2 since.
        map.record(&AffinityReport {
            message_boundaries: Arc::from(["b1".to_string()]),
            ..report("k", "a1", 1_000)
        });
        map.record(&AffinityReport {
            message_boundaries: Arc::from(["b2".to_string()]),
            ..report("k", "a2", 2_000)
        });
        let boundaries = vec!["b1".to_string(), "b2".to_string()];
        let incarnation_of = |account_id: &str| match account_id {
            "a1" => Some(1),
            "a2" => Some(2),
            _ => None,
        };
        let query = ConversationQuery {
            canonical_model: "m",
            key_id: "k",
            key_generation: 0,
            message_boundaries: &boundaries,
            incarnation_of: &incarnation_of,
        };
        let found = map
            .conversation(&query)
            .expect("b1's shallower entry is still owned");
        assert_eq!(found.depth, Some(1));
    }

    #[test]
    fn the_purge_drops_an_entry_past_its_keys_ttl_one_whose_key_is_unserved_and_one_whose_account_was_replaced_and_keeps_the_rest(
    ) {
        let mut map = AffinityMap::default();
        // "expired" and "unserved" sit on a live account, so their own clauses carry the purge, not the
        // incarnation mismatch that "replaced" relies on.
        map.record(&AffinityReport {
            message_boundaries: Arc::from(["expired".to_string()]),
            ..report("expired-key", "a", 1_000)
        });
        map.record(&AffinityReport {
            message_boundaries: Arc::from(["unserved".to_string()]),
            ..report("unserved-key", "a", 1_000)
        });
        map.record(&AffinityReport {
            message_boundaries: Arc::from(["replaced".to_string()]),
            ..report("live-key", "c", 1_000)
        });
        map.record(&AffinityReport {
            message_boundaries: Arc::from(["kept".to_string()]),
            ..report("live-key", "b", 1_000)
        });
        let ttl_of = |key_id: &str| match key_id {
            "expired-key" => Some(1_000),
            "unserved-key" => None,
            _ => Some(1_000_000),
        };
        let incarnation_of = |account_id: &str| match account_id {
            "a" => Some(1),
            "b" => Some(1),
            // Replaced under a new incarnation, not absent outright: distinguishes a mismatch from a gone
            // account.
            "c" => Some(2),
            _ => None,
        };
        let now = 5_000;
        let gone = map.purge(now, ttl_of, incarnation_of);
        assert_eq!(gone.len(), 3);
        assert!(
            map.entry("m", "kept").is_some(),
            "the fourth entry outlives the purge"
        );
        assert!(map.entry("m", "expired").is_none());
        assert!(map.entry("m", "unserved").is_none());
        assert!(map.entry("m", "replaced").is_none());
    }

    #[test]
    fn live_reads_the_entrys_own_ttl_and_is_false_at_ttl_ms_0() {
        let entry = AffinityEntry {
            chain: AffinityChain::Message,
            account_id: "a".into(),
            incarnation: 1,
            written_at: 1_000,
            ttl_ms: 0,
            prefix_tokens: 0,
            key_id: "k".into(),
            key_generation: 0,
            root: "b1".into(),
            lane: crate::lane_key("m", "standard", "none", "-"),
            depth: 1,
            message_id: None,
        };
        assert!(!entry.live(1_000));
        let warm = AffinityEntry {
            ttl_ms: 500,
            ..entry
        };
        assert!(warm.live(1_400));
        assert!(!warm.live(1_600));
    }

    #[test]
    fn a_lookup_with_the_spelling_finds_an_entry_recorded_under_the_dated_id() {
        let mut map = AffinityMap::default();
        map.learn_resolution("spelling", "dated");
        let mut r = report("k", "a", 1_000);
        r.canonical_model = "dated".into();
        map.record(&r);
        assert!(map.entry("spelling", "b2").is_some());
    }

    #[test]
    fn an_entry_recorded_under_the_spelling_before_the_resolution_is_not_found_after_it_and_ages_out_through_the_purge(
    ) {
        let mut map = AffinityMap::default();
        let mut r = report("k", "a", 1_000);
        r.canonical_model = "spelling".into();
        map.record(&r);
        map.learn_resolution("spelling", "dated");
        assert_eq!(
            map.entry("dated", "b2"),
            None,
            "not found under the dated id"
        );
        assert_eq!(
            map.entry("spelling", "b2"),
            None,
            "not found under the spelling either, since the lookup canonicalizes forward"
        );
        let ttl_of = |_: &str| Some(1_000);
        let incarnation_of = |_: &str| Some(1);
        let gone = map.purge(10_000, ttl_of, incarnation_of);
        assert_eq!(
            gone.len(),
            2,
            "the stranded entries still age out on their own ttl"
        );
        assert!(map.is_empty());
    }

    #[test]
    fn a_report_with_no_message_boundaries_records_nothing() {
        let mut map = AffinityMap::default();
        let without_boundaries = AffinityReport {
            message_boundaries: Arc::from([]),
            ..report("k", "a", 1_000)
        };
        let rows = map.record(&without_boundaries);
        assert!(
            rows.is_empty(),
            "a report with no boundary names no entry, so nothing is recorded under its root"
        );
        assert!(map.is_empty());
    }

    #[test]
    fn an_unconfirmed_refresh_keeps_the_old_warm_deadline_and_a_confirmed_one_extends_it() {
        let mut r = report("k", "a", 0);
        r.cache_touched = true;
        r.message_ttl_ms = Some(300_000); // five minutes

        let mut unconfirmed = AffinityMap::default();
        unconfirmed.record(&r);
        let link_only = AffinityReport {
            cache_touched: false,
            ..report("k", "a", 240_000) // four minutes later
        };
        unconfirmed.record(&link_only);
        assert!(
            !unconfirmed.entry("m", "b2").unwrap().live(360_000),
            "the original five-minute deadline was not moved: not live at six minutes"
        );

        let mut confirmed = AffinityMap::default();
        confirmed.record(&r);
        let confirmed_refresh = AffinityReport {
            cache_touched: true,
            message_ttl_ms: Some(300_000),
            ..report("k", "a", 240_000)
        };
        confirmed.record(&confirmed_refresh);
        assert!(
            confirmed.entry("m", "b2").unwrap().live(540_000),
            "a confirmed refresh resets the deadline from its own written_at: live to nine minutes"
        );
        assert!(!confirmed.entry("m", "b2").unwrap().live(540_001));
    }

    /// (static tier, message tier) as recorded for a request whose tiers carry the given deepest markers and whose
    /// response read `read` tokens beside its writes; the static prefix is sized at 500 tokens.
    fn recorded_ttls(
        uniform: Option<i64>,
        tiers: (Option<i64>, Option<i64>),
        writes: (Option<u64>, Option<u64>),
        read: u64,
    ) -> (i64, i64) {
        let mut map = AffinityMap::default();
        map.record(&AffinityReport {
            static_boundaries: Arc::from(["s1".to_string()]),
            static_sizes: vec![500],
            static_ttl_ms: tiers.0,
            message_ttl_ms: tiers.1,
            cache_touched: true,
            prefix_tokens: read + writes.0.unwrap_or(0) + writes.1.unwrap_or(0),
            cache_write_5m_tokens: writes.0,
            cache_write_1h_tokens: writes.1,
            uniform_ttl_ms: uniform,
            ..report("k", "a", 0)
        });
        (
            map.entry("m", "s1").unwrap().ttl_ms,
            map.entry("m", "b2").unwrap().ttl_ms,
        )
    }

    #[test]
    fn a_uniform_request_takes_the_written_ttl_and_a_mixed_one_keeps_its_markers() {
        let hour = Some(3_600_000);
        let five = Some(300_000);
        assert_eq!(
            recorded_ttls(hour, (hour, hour), (Some(200), Some(0)), 0),
            (300_000, 300_000),
            "the upstream read nothing and wrote at five minutes, so every tier the request marked lives five minutes"
        );
        assert_eq!(
            recorded_ttls(hour, (None, hour), (Some(200), Some(0)), 0),
            (0, 300_000),
            "a tier without a marker gets no warmth whatever the split says"
        );
        assert_eq!(
            recorded_ttls(None, (five, five), (Some(0), Some(200)), 0),
            (300_000, 300_000),
            "a request marked at both TTLs keeps each tier's marker, even where the deepest ones agree"
        );
        for writes in [(Some(200), Some(200)), (Some(200), None), (None, None)] {
            assert_eq!(
                recorded_ttls(hour, (hour, hour), writes, 0),
                (3_600_000, 3_600_000),
                "writes {writes:?} name no single TTL, so the markers stand"
            );
        }
    }

    #[test]
    fn a_tier_the_response_only_read_keeps_its_markers_lifetime() {
        let hour = Some(3_600_000);
        let five = Some(300_000);
        assert_eq!(
            recorded_ttls(five, (five, five), (Some(0), Some(200)), 0),
            (3_600_000, 3_600_000),
            "the upstream read nothing and wrote at an hour, so every tier the request marked lives an hour"
        );
        assert_eq!(
            recorded_ttls(hour, (hour, hour), (Some(200), Some(0)), 600),
            (3_600_000, 300_000),
            "the static prefix was read and keeps the hour, and the tail was written for five minutes"
        );
        assert_eq!(
            recorded_ttls(five, (five, five), (Some(0), Some(200)), 600),
            (300_000, 3_600_000),
            "the static prefix was read and keeps five minutes, and the tail was written for an hour"
        );
        assert_eq!(
            recorded_ttls(hour, (hour, hour), (Some(200), Some(0)), 100),
            (3_600_000, 300_000),
            "any read leaves the static tier at its marker's lifetime, and the written tail takes the split's TTL"
        );
    }

    #[test]
    fn a_read_below_the_estimated_static_size_never_gives_the_static_tier_the_splits_ttl() {
        let five = Some(300_000);
        assert_eq!(
            recorded_ttls(five, (five, five), (Some(0), Some(200)), 100),
            (300_000, 3_600_000),
            "the static size is an estimate that can run past the real prefix, so a read short of it proves no static \
             write: the static boundaries keep five minutes and the message boundary records the hour"
        );
    }

    #[test]
    fn drain_carries_the_message_id_a_refresh_wrote() {
        let mut map = AffinityMap::default();
        let one = |now: i64, message_id: Option<&str>| AffinityReport {
            message_boundaries: Arc::from(["b1".to_string()]),
            message_id: message_id.map(String::from),
            ..report("k", "a", now)
        };
        map.record(&one(1_000, None));
        map.record(&one(2_000, Some("msg_01")));
        assert_eq!(
            map.drain()
                .iter()
                .map(|row| (row.hash.as_str(), row.message_id.as_deref()))
                .collect::<Vec<_>>(),
            [("b1", Some("msg_01"))],
            "the refreshed row persists the id the refresh wrote"
        );
    }
}
