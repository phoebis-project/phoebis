//! The affinity map: which conversation a boundary or a parent id belongs to, and how warm the cache is
//! there. Shared between the router task and the relay's conversation climb the way `InFlight` is, so a lookup
//! never waits on a command round trip.

use std::collections::{HashMap, HashSet};

pub use phoebis_store::{AffinityChain, AffinityRow};

/// One boundary or parent id as the map holds it; `AffinityRow` minus its key. The links are always present;
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
}

impl AffinityEntry {
    /// Warm: the cache confirmed this boundary and its marker's lifetime has not run out.
    pub fn live(&self, now: i64) -> bool {
        self.ttl_ms > 0 && now - self.written_at <= self.ttl_ms
    }
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
    /// The conversation the request belonged to, its session id. A report without one is rooted at its first
    /// message boundary.
    pub root: Option<String>,
    pub static_boundaries: Vec<String>,
    pub message_boundaries: Vec<String>,
    /// Parallel to `static_boundaries`: the estimated token size of the blocks each boundary covers, as the
    /// relay computed it at chain time (canonical bytes over four); 0 for the empty-tools boundary of a request
    /// without tools.
    pub static_sizes: Vec<u64>,
    pub message_id: Option<String>,
    pub request_id: Option<String>,
    /// The lifetime of the marker on each tier as sent upstream after the rewrite: the deepest marker inside
    /// `tools[]` or `system[]` for the static tier, the deepest inside `messages[]` for the message tier, a
    /// top-level member for both; `None` for a tier without a marker, which gets no warmth.
    pub static_ttl_ms: Option<i64>,
    pub message_ttl_ms: Option<i64>,
    /// The usage reported a cache read or a cache write: the evidence the warmth fields need.
    pub cache_touched: bool,
    /// `cache_read + write_5m + write_1h`: the prefix the upstream reported for this request, the promised size
    /// of a message or parent entry.
    pub prefix_tokens: u64,
    pub now: i64,
    /// The response's cache-creation writes as its usage reported them: the split object fills both (the 1-hour
    /// member present, if zero), the older total-only shape the 5-minute one alone; `None` without usage.
    pub cache_write_5m_tokens: Option<u64>,
    pub cache_write_1h_tokens: Option<u64>,
    /// The TTL every `cache_control` marker of the dispatched body carries, when they all carry one; `None` for a
    /// body without a marker or with markers at both TTLs.
    pub uniform_ttl_ms: Option<i64>,
}

pub struct ConversationQuery<'a> {
    pub canonical_model: &'a str,
    pub key_id: &'a str,
    pub key_generation: i64,
    /// `diagnostics.previous_message_id` then `cc_prev_req`, as the client sent them.
    pub parent_ids: &'a [String],
    pub message_boundaries: &'a [String],
    /// The current incarnation of an account id, `None` for one the registry no longer holds; the handle
    /// passes the snapshot's reading, a test a stub. An entry under another incarnation is stepped over.
    pub incarnation_of: &'a dyn Fn(&str) -> Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationMatch {
    pub root: String,
    pub depth: Option<u32>,
}

/// What the relay's climb asks: implemented by the map and by the handle.
pub trait ConversationLookup {
    fn conversation(&self, query: &ConversationQuery<'_>) -> Option<ConversationMatch>;
}

/// A boundary or a parent id under the model it was reported for: the map's own key.
type EntryKey = (String, String);

fn row_of(key: &EntryKey, entry: &AffinityEntry) -> AffinityRow {
    AffinityRow {
        model: key.0.clone(),
        hash: key.1.clone(),
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
    }
}

#[derive(Debug, Default)]
pub struct AffinityMap {
    entries: HashMap<EntryKey, AffinityEntry>,
    /// A parent id (a message or a request id) to the entry key it names, independent of model: a conversation
    /// that changes model between turns still finds the link by the id alone.
    parents: HashMap<String, EntryKey>,
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
            let key = (row.model.clone(), row.hash.clone());
            if matches!(row.chain, AffinityChain::Request | AffinityChain::Response) {
                map.parents.insert(row.hash.clone(), key.clone());
            }
            map.entries.insert(
                key,
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
        let canonical_model = self.canonical(canonical_model);
        self.entries
            .get(&(canonical_model.to_string(), hash.to_string()))
    }

    pub fn parent(&self, id: &str) -> Option<&AffinityEntry> {
        self.parents.get(id).and_then(|key| self.entries.get(key))
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

    /// Upserts the report's boundaries and parent ids; answers the rows to persist (new or moved).
    pub fn record(&mut self, report: &AffinityReport) -> Vec<AffinityRow> {
        let canonical_model = self.canonical(&report.canonical_model).to_string();
        let root = report.root.clone().unwrap_or_else(|| {
            report
                .message_boundaries
                .first()
                .cloned()
                .unwrap_or_default()
        });
        // The upstream's own TTL beats the marker's when every marker of the request carried one TTL and the split
        // says the cache wrote at the other: the markers are what the request asked, the split is what the cache
        // holds. A request with markers at both TTLs keeps each tier's marker, since the split names no tier.
        let written = match (report.cache_write_5m_tokens, report.cache_write_1h_tokens) {
            // The split object always carries the 1-hour member, so its absence is the older total-only shape,
            // which names no TTL.
            (Some(five), Some(0)) if five > 0 => Some(300_000),
            (Some(0) | None, Some(hour)) if hour > 0 => Some(3_600_000),
            _ => None,
        };
        let written = written.filter(|w| report.uniform_ttl_ms.is_some_and(|t| t != *w));
        let message_ttl_ms = report.message_ttl_ms.map(|t| written.unwrap_or(t));
        let static_ttl_ms = report.static_ttl_ms.map(|t| written.unwrap_or(t));
        let message_confirmed = report.cache_touched && message_ttl_ms.is_some();
        let message_ttl = message_ttl_ms.unwrap_or(0);
        let mut rows = Vec::new();

        for boundary in &report.message_boundaries {
            if let Some(row) = self.upsert(
                &canonical_model,
                boundary,
                AffinityChain::Message,
                report,
                &root,
                message_confirmed,
                message_ttl,
                report.prefix_tokens,
            ) {
                rows.push(row);
            }
        }
        // An empty root only happens with no message boundaries either, so there is no conversation to name:
        // the parent ids are skipped rather than answering a parent-id match with an empty root.
        if !root.is_empty() {
            if let Some(id) = &report.message_id {
                if let Some(row) = self.upsert(
                    &canonical_model,
                    id,
                    AffinityChain::Response,
                    report,
                    &root,
                    message_confirmed,
                    message_ttl,
                    report.prefix_tokens,
                ) {
                    rows.push(row);
                }
                self.parents
                    .insert(id.clone(), (canonical_model.clone(), id.clone()));
            }
            if let Some(id) = &report.request_id {
                if let Some(row) = self.upsert(
                    &canonical_model,
                    id,
                    AffinityChain::Request,
                    report,
                    &root,
                    message_confirmed,
                    message_ttl,
                    report.prefix_tokens,
                ) {
                    rows.push(row);
                }
                self.parents
                    .insert(id.clone(), (canonical_model.clone(), id.clone()));
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
                    report,
                    &root,
                    static_confirmed,
                    static_ttl,
                    size,
                ) {
                    rows.push(row);
                }
            }
        }
        rows
    }

    /// Entries past `ttl_of(key_id)` (an idle TTL in ms; `None` is unserved), or whose account is gone or under
    /// another incarnation, leave; answers their keys.
    pub fn purge(
        &mut self,
        now: i64,
        ttl_of: impl Fn(&str) -> Option<i64>,
        incarnation_of: impl Fn(&str) -> Option<u64>,
    ) -> Vec<(String, String)> {
        let gone: HashSet<EntryKey> = self
            .entries
            .iter()
            .filter(|(_, entry)| {
                let expired = match ttl_of(&entry.key_id) {
                    None => true,
                    Some(ttl) => now - entry.written_at > ttl,
                };
                expired || incarnation_of(&entry.account_id) != Some(entry.incarnation)
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in &gone {
            self.entries.remove(key);
            self.dirty.remove(key);
        }
        self.parents.retain(|_, key| !gone.contains(key));
        gone.into_iter().collect()
    }

    /// The refreshed rows not yet persisted, cleared.
    pub fn drain(&mut self) -> Vec<AffinityRow> {
        std::mem::take(&mut self.dirty)
            .into_iter()
            .filter_map(|key| self.entries.get(&key).map(|entry| row_of(&key, entry)))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// An existing entry with the same account, key and generation is refreshed in place and its key marked
    /// dirty; any other case (new, or held by another key) replaces it and yields a row. `confirmed` decides
    /// the warmth: when false a refreshed entry on the report's lane keeps its warm deadline, its `ttl_ms`
    /// shrinking to the lifetime it has left, and one refreshed from another lane or a new one carries no warmth.
    #[allow(clippy::too_many_arguments)]
    fn upsert(
        &mut self,
        model: &str,
        hash: &str,
        chain: AffinityChain,
        report: &AffinityReport,
        root: &str,
        confirmed: bool,
        ttl_ms: i64,
        prefix_tokens: u64,
    ) -> Option<AffinityRow> {
        let key = (model.to_string(), hash.to_string());
        let same_holder = self.entries.get(&key).is_some_and(|entry| {
            entry.account_id == report.account_id
                && entry.key_id == report.key_id
                && entry.key_generation == report.key_generation
        });
        if same_holder {
            let entry = self.entries.get_mut(&key).expect("checked above");
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
            // The root stays the one the boundary was first recorded under: a later writer sharing it (a fork's
            // own turns landing on the same account) refreshes the links and the warmth, never re-roots the
            // conversation that got here first.
            entry.incarnation = report.incarnation;
            // One entry holds one lane: a conversation that switched lanes forgets the earlier lane's cache here,
            // which a lane change reads as cold anyway.
            entry.lane = report.lane.clone();
            self.dirty.insert(key);
            return None;
        }
        // A writer of the same key and generation keeps the root the boundary was first recorded under, even
        // from another account (a fork placed elsewhere, a migrated base): only another key or generation, a
        // different tenant, replaces it.
        let inherited_root = self.entries.get(&key).and_then(|entry| {
            (entry.key_id == report.key_id && entry.key_generation == report.key_generation)
                .then(|| entry.root.clone())
        });
        let entry = AffinityEntry {
            chain,
            account_id: report.account_id.clone(),
            incarnation: report.incarnation,
            written_at: report.now,
            ttl_ms: if confirmed { ttl_ms } else { 0 },
            prefix_tokens: if confirmed { prefix_tokens } else { 0 },
            key_id: report.key_id.clone(),
            key_generation: report.key_generation,
            root: inherited_root.unwrap_or_else(|| root.to_string()),
            lane: report.lane.clone(),
        };
        let row = row_of(&key, &entry);
        self.dirty.remove(&key);
        self.entries.insert(key, entry);
        Some(row)
    }
}

impl ConversationLookup for AffinityMap {
    /// Own entries only: the first parent id the key and generation own, else the deepest
    /// message boundary they own. Warmth plays no part in identity.
    fn conversation(&self, query: &ConversationQuery<'_>) -> Option<ConversationMatch> {
        let owns = |entry: &AffinityEntry| -> bool {
            entry.key_id == query.key_id
                && entry.key_generation == query.key_generation
                && (query.incarnation_of)(&entry.account_id) == Some(entry.incarnation)
        };
        for id in query.parent_ids {
            if let Some(entry) = self.parent(id).filter(|e| owns(e)) {
                return Some(ConversationMatch {
                    root: entry.root.clone(),
                    depth: None,
                });
            }
        }
        let (depth, entry) = self.deepest(query.canonical_model, query.message_boundaries, owns)?;
        Some(ConversationMatch {
            root: entry.root.clone(),
            depth: Some(depth),
        })
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
            root: None,
            static_boundaries: Vec::new(),
            message_boundaries: vec!["b1".into(), "b2".into()],
            static_sizes: Vec::new(),
            message_id: Some("resp-1".into()),
            request_id: Some("req-1".into()),
            static_ttl_ms: None,
            message_ttl_ms: None,
            cache_touched: false,
            prefix_tokens: 0,
            now,
            cache_write_5m_tokens: None,
            cache_write_1h_tokens: None,
            uniform_ttl_ms: None,
        }
    }

    fn incarnation_of(_: &str) -> Option<u64> {
        Some(1)
    }

    #[test]
    fn a_report_records_every_boundary_and_both_parents_and_answers_rows_for_the_new_ones_only() {
        let mut map = AffinityMap::default();
        let rows = map.record(&report("k", "a", 1_000));
        assert_eq!(
            rows.len(),
            4,
            "two message boundaries plus the response and request ids"
        );
        assert_eq!(map.len(), 4);
        let mut second = report("k", "a", 2_000);
        second.message_boundaries.push("b3".into());
        let more_rows = map.record(&second);
        assert_eq!(
            more_rows.len(),
            1,
            "the two boundaries and both parents refresh in place; only b3 is new"
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
        assert_eq!(drained.len(), 4, "every refreshed entry, once");
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
            parent_ids: &[],
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
        assert_eq!(
            rows.len(),
            4,
            "every boundary and parent moves to the new holder"
        );
        assert_eq!(map.entry("m", "b1").unwrap().key_id, "second");
    }

    #[test]
    fn a_report_with_a_one_hour_static_tier_and_a_five_minute_message_tier_records_the_two_lifetimes_on_the_two_chains(
    ) {
        let mut map = AffinityMap::default();
        let mut r = report("k", "a", 1_000);
        r.static_boundaries = vec!["s1".into()];
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
        r.static_boundaries = vec!["s1".into()];
        r.static_sizes = vec![10];
        r.static_ttl_ms = Some(3_600_000);
        r.message_ttl_ms = Some(300_000);
        r.cache_touched = false;
        map.record(&r);
        let entry = map.parent("resp-1").expect("the response id was recorded");
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
                message_boundaries: vec![(*boundary).to_string()],
                request_id: None,
                message_id: None,
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
    fn the_lookup_steps_over_an_entry_under_another_incarnation() {
        let mut map = AffinityMap::default();
        // b1 is still owned by a1's recorded incarnation; b2's account moved on to incarnation 2 since.
        map.record(&AffinityReport {
            message_boundaries: vec!["b1".into()],
            request_id: None,
            message_id: None,
            ..report("k", "a1", 1_000)
        });
        map.record(&AffinityReport {
            message_boundaries: vec!["b2".into()],
            request_id: None,
            message_id: None,
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
            parent_ids: &[],
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
            message_boundaries: vec!["expired".into()],
            ..report("expired-key", "a", 1_000)
        });
        map.record(&AffinityReport {
            message_boundaries: vec!["unserved".into()],
            ..report("unserved-key", "a", 1_000)
        });
        map.record(&AffinityReport {
            message_boundaries: vec!["replaced".into()],
            ..report("live-key", "c", 1_000)
        });
        map.record(&AffinityReport {
            message_boundaries: vec!["kept".into()],
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
    fn the_parent_lookup_ignores_the_model() {
        let mut map = AffinityMap::default();
        map.record(&report("k", "a", 1_000)); // canonical_model "m", response id "resp-1"
        let parent_ids = vec!["resp-1".to_string()];
        let boundaries: Vec<String> = Vec::new();
        let query = ConversationQuery {
            canonical_model: "other",
            key_id: "k",
            key_generation: 0,
            parent_ids: &parent_ids,
            message_boundaries: &boundaries,
            incarnation_of: &incarnation_of,
        };
        let found = map
            .conversation(&query)
            .expect("the parent id is found under a model the report never used");
    }

    #[test]
    fn a_report_under_the_spelling_after_the_table_knows_its_dated_id_lands_under_the_dated_id() {
        let mut map = AffinityMap::default();
        map.learn_resolution("spelling", "dated");
        let mut r = report("k", "a", 1_000);
        r.canonical_model = "spelling".into();
        map.record(&r);
        assert!(map.entry("dated", "b2").is_some());
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
            4,
            "the stranded entry still ages out on its own ttl"
        );
        assert!(map.is_empty());
    }

    #[test]
    fn a_report_with_no_root_and_no_message_boundaries_names_no_parent() {
        let mut map = AffinityMap::default();
        let report_without_root = AffinityReport {
            message_boundaries: Vec::new(),
            ..report("k", "a", 1_000)
        };
        let rows = map.record(&report_without_root);
        assert!(
            rows.is_empty(),
            "no root can be named, so nothing is recorded"
        );
        assert!(map.parent("resp-1").is_none());
        assert!(map.parent("req-1").is_none());
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

    #[test]
    fn a_uniform_request_takes_the_written_ttl_and_a_mixed_one_keeps_its_markers() {
        // (static tier, message tier) as recorded for a request whose tiers carry the given deepest markers.
        let recorded = |uniform: Option<i64>,
                        tiers: (Option<i64>, Option<i64>),
                        writes: (Option<u64>, Option<u64>)| {
            let mut map = AffinityMap::default();
            map.record(&AffinityReport {
                static_boundaries: vec!["s1".into()],
                static_sizes: vec![10],
                static_ttl_ms: tiers.0,
                message_ttl_ms: tiers.1,
                cache_touched: true,
                prefix_tokens: 200,
                cache_write_5m_tokens: writes.0,
                cache_write_1h_tokens: writes.1,
                uniform_ttl_ms: uniform,
                ..report("k", "a", 0)
            });
            (
                map.entry("m", "s1").unwrap().ttl_ms,
                map.entry("m", "b2").unwrap().ttl_ms,
            )
        };
        let hour = Some(3_600_000);
        let five = Some(300_000);
        assert_eq!(
            recorded(hour, (hour, hour), (Some(200), Some(0))),
            (300_000, 300_000),
            "the upstream wrote at five minutes, so every tier the request marked lives five minutes"
        );
        assert_eq!(
            recorded(hour, (None, hour), (Some(200), Some(0))),
            (0, 300_000),
            "a tier without a marker gets no warmth whatever the split says"
        );
        assert_eq!(
            recorded(None, (five, five), (Some(0), Some(200))),
            (300_000, 300_000),
            "a request marked at both TTLs keeps each tier's marker, even where the deepest ones agree"
        );
        for writes in [(Some(200), Some(200)), (Some(200), None), (None, None)] {
            assert_eq!(
                recorded(hour, (hour, hour), writes),
                (3_600_000, 3_600_000),
                "writes {writes:?} name no single TTL, so the markers stand"
            );
        }
    }
}
