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
                },
            );
        }
        map
    }

    pub fn entry(&self, canonical_model: &str, hash: &str) -> Option<&AffinityEntry> {
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
        let root = report.root.clone().unwrap_or_else(|| {
            report
                .message_boundaries
                .first()
                .cloned()
                .unwrap_or_default()
        });
        let message_confirmed = report.cache_touched && report.message_ttl_ms.is_some();
        let message_ttl = report.message_ttl_ms.unwrap_or(0);
        let mut rows = Vec::new();

        for boundary in &report.message_boundaries {
            if let Some(row) = self.upsert(
                &report.canonical_model,
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
        if let Some(id) = &report.message_id {
            if let Some(row) = self.upsert(
                &report.canonical_model,
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
                .insert(id.clone(), (report.canonical_model.clone(), id.clone()));
        }
        if let Some(id) = &report.request_id {
            if let Some(row) = self.upsert(
                &report.canonical_model,
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
                .insert(id.clone(), (report.canonical_model.clone(), id.clone()));
        }
        if report.cache_touched {
            let static_confirmed = report.static_ttl_ms.is_some();
            let static_ttl = report.static_ttl_ms.unwrap_or(0);
            for (index, boundary) in report.static_boundaries.iter().enumerate() {
                let size = report.static_sizes.get(index).copied().unwrap_or(0);
                if let Some(row) = self.upsert(
                    &report.canonical_model,
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
        let gone: Vec<EntryKey> = self
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
        gone
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
    /// the warmth: when false a refreshed entry keeps what it already had, and a new one carries no warmth.
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
            entry.written_at = report.now;
            entry.incarnation = report.incarnation;
            entry.root = root.to_string();
            if confirmed {
                entry.ttl_ms = ttl_ms;
                entry.prefix_tokens = prefix_tokens;
            }
            self.dirty.insert(key);
            return None;
        }
        let entry = AffinityEntry {
            chain,
            account_id: report.account_id.clone(),
            incarnation: report.incarnation,
            written_at: report.now,
            ttl_ms: if confirmed { ttl_ms } else { 0 },
            prefix_tokens: if confirmed { prefix_tokens } else { 0 },
            key_id: report.key_id.clone(),
            key_generation: report.key_generation,
            root: root.to_string(),
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
        map.record(&report("k", "a", 1_000));
        let entry = map.parent("resp-1").expect("the response id was recorded");
        assert!(!entry.live(1_000), "no warmth was ever confirmed");
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
        // An expired fourth boundary (account a3) above a live third (account a2): the walk steps over it.
        let (depth, _) = map
            .deepest("m", &boundaries, |e| e.account_id != "a3")
            .expect("the third boundary is accepted");
        assert_eq!(depth, 3);
    }

    #[test]
    fn the_lookup_steps_over_an_entry_under_another_incarnation() {
        let mut map = AffinityMap::default();
        map.record(&report("k", "a", 1_000));
        let boundaries = vec!["b1".to_string(), "b2".to_string()];
        let no_incarnation = |_: &str| None;
        let query = ConversationQuery {
            canonical_model: "m",
            key_id: "k",
            key_generation: 0,
            parent_ids: &[],
            message_boundaries: &boundaries,
            incarnation_of: &no_incarnation,
        };
        assert_eq!(map.conversation(&query), None);
    }

    #[test]
    fn the_purge_drops_an_entry_past_its_keys_ttl_one_whose_key_is_unserved_and_one_whose_account_was_replaced_and_keeps_the_rest(
    ) {
        let mut map = AffinityMap::default();
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
            ..report("live-key", "a", 1_000)
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
            "a" => None,
            _ => Some(1),
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
}
