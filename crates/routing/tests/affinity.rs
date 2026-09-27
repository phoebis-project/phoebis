//! The cases the affinity map alone can exercise: what the conversation lookup and `deepest` answer over a
//! continuation, a fork, an edit, a fresh session under a known static prefix, a tools change, a compaction, and a
//! subagent's first request. The warmth verdict and the placement of each case are the selector's tests, and the
//! fork facts the relay reads from a match are the relay's own.

use phoebis_routing::{
    AffinityMap, AffinityReport, ConversationLookup, ConversationMatch, ConversationQuery,
};

fn boundaries(hashes: &[&str]) -> Vec<String> {
    hashes.iter().map(|h| h.to_string()).collect()
}

fn report(key_id: &str, account_id: &str, message_boundaries: &[&str], now: i64) -> AffinityReport {
    AffinityReport {
        account_id: account_id.into(),
        incarnation: 1,
        key_id: key_id.into(),
        key_generation: 0,
        canonical_model: "claude-sonnet-5".into(),
        root: None,
        static_boundaries: Vec::new(),
        message_boundaries: boundaries(message_boundaries),
        static_sizes: Vec::new(),
        message_id: None,
        request_id: None,
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

fn query<'a>(key_id: &'a str, message_boundaries: &'a [String]) -> ConversationQuery<'a> {
    ConversationQuery {
        canonical_model: "claude-sonnet-5",
        key_id,
        key_generation: 0,
        parent_ids: &[],
        message_boundaries,
        incarnation_of: &incarnation_of,
    }
}

fn content_match(m: Option<ConversationMatch>, depth: u32) {
    let m = m.expect("a match at this depth");
    assert_eq!(m.depth, Some(depth));
}

#[test]
fn the_lookup_answers_a_continuation_by_one_message_at_the_previous_tails_depth() {
    let mut map = AffinityMap::default();
    map.record(&report("k", "a", &["b1", "b2", "b3"], 1_000));
    let q = boundaries(&["b1", "b2", "b3", "b4"]);
    content_match(map.conversation(&query("k", &q)), 3);
}

#[test]
fn a_fork_matches_at_the_last_shared_boundary() {
    let mut map = AffinityMap::default();
    map.record(&report("k", "a", &["b1", "b2", "b3", "bA4"], 1_000));
    let q = boundaries(&["b1", "b2", "b3", "bB4"]);
    content_match(map.conversation(&query("k", &q)), 3);
}

#[test]
fn the_lookup_answers_a_replaced_last_message_one_boundary_shallower() {
    let mut map = AffinityMap::default();
    map.record(&report("k", "a", &["b1", "b2", "b3", "b4"], 1_000));
    let q = boundaries(&["b1", "b2", "b3", "b4-edited"]);
    content_match(map.conversation(&query("k", &q)), 3);
}

#[test]
fn fresh_messages_under_a_known_static_prefix_name_no_conversation_and_match_the_static_depth() {
    let mut map = AffinityMap::default();
    map.record(&AffinityReport {
        static_boundaries: boundaries(&["s1", "s2"]),
        static_sizes: vec![10, 20],
        cache_touched: true,
        static_ttl_ms: Some(3_600_000),
        ..report("k1", "a", &["old-m1"], 1_000)
    });
    let q = boundaries(&["fresh-m1"]);
    assert_eq!(map.conversation(&query("k2", &q)), None);
    let statics = boundaries(&["s1", "s2"]);
    let (depth, _) = map
        .deepest("claude-sonnet-5", &statics, |_| true)
        .expect("the static prefix is known");
    assert_eq!(depth, 2);
}

#[test]
fn the_lookup_follows_the_message_chain_across_a_tools_change() {
    let mut map = AffinityMap::default();
    map.record(&AffinityReport {
        static_boundaries: boundaries(&["s1"]),
        static_sizes: vec![10],
        cache_touched: true,
        static_ttl_ms: Some(3_600_000),
        ..report("k", "a", &["b1", "b2", "b3"], 1_000)
    });
    let q = boundaries(&["b1", "b2", "b3", "b4"]);
    content_match(map.conversation(&query("k", &q)), 3);
    let s2 = boundaries(&["s2"]);
    assert_eq!(map.deepest("claude-sonnet-5", &s2, |_| true), None);
}

#[test]
fn a_compacted_body_names_no_conversation_and_its_continuation_matches_the_compacted_chain() {
    let mut map = AffinityMap::default();
    map.record(&report("k", "a", &["b1", "b2", "b3", "b4"], 1_000));
    let compacted = boundaries(&["c1"]);
    assert_eq!(map.conversation(&query("k", &compacted)), None);
    map.record(&report("k", "a", &["c1"], 2_000));
    let continuation = boundaries(&["c1", "c2"]);
    content_match(map.conversation(&query("k", &continuation)), 1);
}

#[test]
fn a_subagents_first_request_matches_its_cohorts_static_prefix_and_no_message_boundary() {
    let mut map = AffinityMap::default();
    map.record(&AffinityReport {
        static_boundaries: boundaries(&["t1", "t2"]),
        static_sizes: vec![10, 20],
        cache_touched: true,
        static_ttl_ms: Some(3_600_000),
        ..report("k", "a", &["x1"], 1_000)
    });
    let q = boundaries(&["y1"]);
    assert_eq!(map.conversation(&query("k", &q)), None);
    let statics = boundaries(&["t1", "t2"]);
    let (depth, _) = map
        .deepest("claude-sonnet-5", &statics, |_| true)
        .expect("the cohort's static prefix is known");
    assert_eq!(depth, 2);
}
