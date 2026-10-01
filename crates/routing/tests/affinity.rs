//! The cases the affinity map alone can exercise: what the conversation lookup and `deepest` answer over a
//! continuation, a fork, an edit, a fresh session under a known static prefix, a tools change, a compaction, and a
//! subagent's first request. The warmth verdict and the placement of each case are the selector's tests, and the
//! fork facts the relay reads from a match are the relay's own.

use phoebis_routing::{
    lane_key, AffinityChain, AffinityMap, AffinityReport, AffinityRow, ConversationLookup,
    ConversationMatch, ConversationQuery,
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
        lane: lane_key("claude-sonnet-5", "standard", "none", "-"),
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
        cache_write_5m_tokens: None,
        cache_write_1h_tokens: None,
        uniform_ttl_ms: None,
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

#[test]
fn a_report_stamps_its_lane_and_a_refresh_from_another_lane_rewrites_it() {
    let standard = lane_key("claude-sonnet-5", "standard", "none", "-");
    let fast = lane_key("claude-sonnet-5", "fast", "none", "-");
    let mut map = AffinityMap::default();
    map.record(&report("k", "a", &["b1", "b2"], 1_000));
    assert_eq!(
        map.entry("claude-sonnet-5", "b2").map(|e| e.lane.as_str()),
        Some(standard.as_str())
    );
    let refreshed = map.record(&AffinityReport {
        lane: fast.clone(),
        ..report("k", "a", &["b1", "b2"], 2_000)
    });
    assert!(refreshed.is_empty(), "the same holder refreshes in place");
    assert_eq!(
        map.entry("claude-sonnet-5", "b2").map(|e| e.lane.as_str()),
        Some(fast.as_str()),
        "one entry holds one lane, the last writer's"
    );
    assert!(
        map.drain().iter().all(|row| row.lane == fast),
        "the refreshed rows persist the new lane"
    );
}

#[test]
fn a_link_only_refresh_from_another_lane_carries_no_warmth_over() {
    let fast = lane_key("claude-sonnet-5", "fast", "none", "-");
    let mut map = AffinityMap::default();
    map.record(&AffinityReport {
        cache_touched: true,
        message_ttl_ms: Some(3_600_000),
        ..report("k", "a", &["b1", "b2"], 1_000)
    });
    let deadline = |map: &AffinityMap| {
        map.entry("claude-sonnet-5", "b2")
            .map(|e| e.written_at + e.ttl_ms)
    };
    assert_eq!(deadline(&map), Some(1_000 + 3_600_000));
    map.record(&report("k", "a", &["b1", "b2"], 2_000));
    assert_eq!(
        deadline(&map),
        Some(1_000 + 3_600_000),
        "a link-only refresh on the same lane keeps the warm deadline"
    );
    map.record(&AffinityReport {
        lane: fast.clone(),
        ..report("k", "a", &["b1", "b2"], 3_000)
    });
    let entry = map.entry("claude-sonnet-5", "b2").unwrap();
    assert_eq!(entry.lane, fast);
    assert!(
        !entry.live(3_000),
        "the standard lane's warmth is no warmth on the fast lane"
    );
}

#[test]
fn a_link_only_refresh_of_an_entry_written_before_the_lane_existed_adopts_its_lane_and_keeps_the_warmth(
) {
    let fast = lane_key("claude-sonnet-5", "fast", "none", "-");
    let mut map = AffinityMap::from_rows(vec![AffinityRow {
        model: "claude-sonnet-5".into(),
        hash: "b1".into(),
        chain: AffinityChain::Message,
        account_id: "a".into(),
        incarnation: 1,
        written_at: 1_000,
        ttl_ms: 3_600_000,
        prefix_tokens: 2_000,
        key_id: "k".into(),
        key_generation: 0,
        root: "b1".into(),
        lane: String::new(),
    }]);
    map.record(&AffinityReport {
        lane: fast.clone(),
        ..report("k", "a", &["b1"], 2_000)
    });
    let entry = map.entry("claude-sonnet-5", "b1").unwrap();
    assert_eq!(
        (entry.lane.as_str(), entry.written_at + entry.ttl_ms),
        (fast.as_str(), 1_000 + 3_600_000),
        "the entry takes the report's lane and keeps its warm deadline"
    );
}

#[test]
fn the_conversation_lookup_finds_a_root_across_lanes() {
    let mut map = AffinityMap::default();
    map.record(&AffinityReport {
        lane: lane_key("claude-sonnet-5", "fast", "none", "-"),
        ..report("k", "a", &["b1", "b2", "b3"], 1_000)
    });
    // A speed switch hashes the same message bytes, so the identity must find the conversation whatever lane
    // its entries were written under; the lane only filters where the cache is warm.
    let q = boundaries(&["b1", "b2", "b3", "b4"]);
    content_match(map.conversation(&query("k", &q)), 3);
}
