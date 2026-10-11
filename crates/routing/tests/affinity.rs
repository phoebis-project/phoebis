//! The cases the affinity map alone can exercise: what the conversation lookup and `deepest` answer over a
//! continuation, a fork, an edit, a fresh session under a known static prefix, a tools change, a compaction, and a
//! subagent's first request. The warmth verdict and the placement of each case are the selector's tests, and the
//! fork facts the relay reads from a match are the relay's own.

use std::sync::Arc;

use phoebis_routing::{
    lane_key, AffinityChain, AffinityMap, AffinityReport, AffinityRow, ConversationLookup,
    ConversationMatch, ConversationQuery, PredecessorQuery, PurgedEntry,
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
        static_boundaries: Arc::from([]),
        message_boundaries: Arc::from(boundaries(message_boundaries)),
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

fn report_with(
    key_id: &str,
    account_id: &str,
    message_boundaries: &[&str],
    now: i64,
    message_id: &str,
) -> AffinityReport {
    AffinityReport {
        message_id: Some(message_id.into()),
        ..report(key_id, account_id, message_boundaries, now)
    }
}

fn incarnation_of(_: &str) -> Option<u64> {
    Some(1)
}

/// The id the entry at `hash` holds. The entry must exist, so a boundary the report never wrote cannot pass for
/// an entry without an id.
fn id_at(map: &AffinityMap, hash: &str) -> Option<String> {
    map.entry("claude-sonnet-5", hash)
        .unwrap_or_else(|| panic!("an entry at {hash}"))
        .message_id
        .clone()
}

/// The lookup of a request on account `a` under key `k`, rooted at `s1`, over `message_boundaries`.
fn predecessor_query<'a>(message_boundaries: &'a [String]) -> PredecessorQuery<'a> {
    PredecessorQuery {
        canonical_model: "claude-sonnet-5",
        account_id: "a",
        key_id: "k",
        key_generation: 0,
        roots: &["s1"],
        message_boundaries,
        incarnation_of: &incarnation_of,
    }
}

fn query<'a>(key_id: &'a str, message_boundaries: &'a [String]) -> ConversationQuery<'a> {
    ConversationQuery {
        canonical_model: "claude-sonnet-5",
        key_id,
        key_generation: 0,
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
        static_boundaries: Arc::from(boundaries(&["s1", "s2"])),
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
        static_boundaries: Arc::from(boundaries(&["s1"])),
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
        static_boundaries: Arc::from(boundaries(&["t1", "t2"])),
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
        depth: 0,
        message_id: None,
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

#[test]
fn purge_answers_each_removed_entry_with_its_cause() {
    let mut map = AffinityMap::default();
    map.record(&report("aged", "a", &["old"], 1_000));
    map.record(&report("live", "replaced", &["moved"], 1_000));
    map.record(&report("live", "a", &["kept"], 1_000));
    let ttl_of = |key_id: &str| match key_id {
        "aged" => Some(1_000),
        _ => Some(1_000_000),
    };
    // `replaced` holds the id under a newer incarnation than the one its entry names.
    let incarnation_of = |account_id: &str| match account_id {
        "replaced" => Some(2),
        _ => Some(1),
    };
    let mut gone: Vec<PurgedEntry> = map.purge(5_000, ttl_of, incarnation_of);
    gone.sort_by(|a, b| a.hash.cmp(&b.hash));
    assert_eq!(
        gone.iter()
            .map(|g| (
                g.model.as_str(),
                g.hash.as_str(),
                g.entry.account_id.as_str(),
                g.cause
            ))
            .collect::<Vec<_>>(),
        [
            ("claude-sonnet-5", "moved", "replaced", "incarnation"),
            ("claude-sonnet-5", "old", "a", "ttl"),
        ],
        "each removal names the entry it took and why"
    );
    assert!(map.entry("claude-sonnet-5", "kept").is_some());
}

#[test]
fn an_entry_records_the_ordinal_of_its_boundary_and_a_refresh_keeps_it() {
    let mut map = AffinityMap::default();
    map.record(&AffinityReport {
        static_boundaries: std::sync::Arc::from(["s1".to_string(), "s2".to_string()]),
        static_sizes: vec![100, 200],
        static_ttl_ms: Some(300_000),
        cache_touched: true,
        ..report("k", "a", &["b1", "b2", "b3"], 1_000)
    });
    let depth = |hash: &str| map.entry("claude-sonnet-5", hash).map(|e| e.depth);
    assert_eq!(
        ["b1", "b2", "b3", "s1", "s2"].map(depth),
        [Some(1), Some(2), Some(3), Some(1), Some(2)],
        "each chain counts its own boundaries from 1"
    );
    map.record(&report("k", "a", &["b3"], 2_000));
    assert_eq!(
        map.entry("claude-sonnet-5", "b3")
            .map(|e| (e.depth, e.written_at)),
        Some((3, 2_000)),
        "a refresh in place moves the entry's time and keeps the ordinal it was written at"
    );
}

#[test]
fn a_reports_id_lands_on_its_tail_boundary_alone_and_a_static_entry_never_holds_one() {
    let mut map = AffinityMap::default();
    map.record(&AffinityReport {
        static_boundaries: Arc::from(boundaries(&["s1"])),
        static_sizes: vec![10],
        static_ttl_ms: Some(3_600_000),
        message_ttl_ms: Some(300_000),
        cache_touched: true,
        prefix_tokens: 500,
        ..report_with("k", "a", &["b1", "b2"], 1_000, "msg_02")
    });
    assert_eq!(
        ["b1", "b2", "s1"].map(|hash| id_at(&map, hash)),
        [None, Some("msg_02".to_string()), None],
        "the answer continues the whole list, so the prefix and the static tier name no answer of it"
    );
}

#[test]
fn a_refresh_in_place_keeps_the_prefix_ids_overwrites_the_tails_and_keeps_it_without_an_id() {
    let mut map = AffinityMap::default();
    map.record(&report_with("k", "a", &["b1"], 1_000, "msg_01"));
    map.record(&report_with("k", "a", &["b1", "b2"], 2_000, "msg_02"));
    assert_eq!(
        (id_at(&map, "b1"), id_at(&map, "b2")),
        (Some("msg_01".to_string()), Some("msg_02".to_string())),
        "the prefix keeps the answer of the turn that ended there and the new tail takes the new answer"
    );
    map.record(&report("k", "a", &["b1", "b2"], 3_000));
    assert_eq!(
        id_at(&map, "b2"),
        Some("msg_02".to_string()),
        "a report that names no id leaves the tail's answer in place"
    );
    map.record(&report_with("k", "a", &["b1", "b2"], 4_000, "msg_03"));
    assert_eq!(
        (id_at(&map, "b1"), id_at(&map, "b2")),
        (Some("msg_01".to_string()), Some("msg_03".to_string())),
        "a newer answer at the same tail overwrites the older one"
    );
}

#[test]
fn a_replacement_by_another_account_starts_with_the_tails_id_alone() {
    let mut map = AffinityMap::default();
    map.record(&report_with("k", "a", &["b1"], 1_000, "msg_01"));
    map.record(&report_with("k", "a", &["b1", "b2"], 2_000, "msg_02"));
    map.record(&report_with("k", "b", &["b1", "b2"], 3_000, "msg_b2"));
    let held = |hash: &str| {
        map.entry("claude-sonnet-5", hash)
            .map(|e| (e.account_id.clone(), e.message_id.clone()))
    };
    assert_eq!(
        held("b2"),
        Some(("b".to_string(), Some("msg_b2".to_string()))),
        "the new holder's answer lands on its tail"
    );
    assert_eq!(
        held("b1"),
        Some(("b".to_string(), None)),
        "the new holder has no answer at the prefix, so the old holder's id leaves with it"
    );
}

#[test]
fn another_conversations_identical_list_leaves_the_first_conversations_id_in_place() {
    let mut map = AffinityMap::default();
    map.record(&AffinityReport {
        root: Some("s1".into()),
        ..report_with("k", "a", &["b1"], 1_000, "msg_01")
    });
    map.record(&AffinityReport {
        root: Some("s2".into()),
        ..report_with("k", "a", &["b1"], 2_000, "msg_s2")
    });
    let entry = map.entry("claude-sonnet-5", "b1").unwrap();
    assert_eq!(
        (entry.root.as_str(), entry.message_id.as_deref()),
        ("s1", Some("msg_01")),
        "a second session opening with the same message files no answer under the first session's root"
    );
    map.record(&AffinityReport {
        root: Some("s2".into()),
        ..report_with("k", "b", &["b1"], 3_000, "msg_b1")
    });
    let entry = map.entry("claude-sonnet-5", "b1").unwrap();
    assert_eq!(
        (
            entry.account_id.as_str(),
            entry.root.as_str(),
            entry.message_id.as_deref()
        ),
        ("b", "s1", None),
        "a replacement under the same key inherits the first root, so the second session's answer is not filed \
         under it"
    );
}

#[test]
fn a_refresh_under_a_new_incarnation_drops_the_entrys_id() {
    let mut map = AffinityMap::default();
    map.record(&report_with("k", "a", &["b1"], 1_000, "msg_01"));
    map.record(&AffinityReport {
        incarnation: 2,
        ..report("k", "a", &["b1"], 2_000)
    });
    assert_eq!(
        id_at(&map, "b1"),
        None,
        "the earlier incarnation's answer is no answer of the account's new holder"
    );
    map.record(&AffinityReport {
        incarnation: 2,
        ..report_with("k", "a", &["b1"], 3_000, "msg_02")
    });
    assert_eq!(id_at(&map, "b1"), Some("msg_02".to_string()));
}

#[test]
fn the_predecessor_is_the_deepest_boundary_the_selected_account_holds_for_the_request_or_its_fork_parent(
) {
    let mut map = AffinityMap::default();
    map.record(&AffinityReport {
        root: Some("s1".into()),
        ..report_with("k", "a", &["b1"], 1_000, "msg_01")
    });
    map.record(&AffinityReport {
        root: Some("s1".into()),
        ..report_with("k", "a", &["b1", "b2"], 2_000, "msg_02")
    });
    let continuation = boundaries(&["b1", "b2", "b3"]);
    assert_eq!(
        map.predecessor(&predecessor_query(&continuation)),
        Some("msg_02".to_string()),
        "a continuation names the answer at the previous tail"
    );
    let rewind = boundaries(&["b1", "b9"]);
    assert_eq!(
        map.predecessor(&predecessor_query(&rewind)),
        Some("msg_01".to_string()),
        "a rewind names the answer at the deepest boundary it still shares"
    );
    let unknown = boundaries(&["b7"]);
    assert_eq!(map.predecessor(&predecessor_query(&unknown)), None);
    assert_eq!(
        map.predecessor(&PredecessorQuery {
            roots: &["s9", "s1"],
            ..predecessor_query(&continuation)
        }),
        Some("msg_02".to_string()),
        "a fork under its own root names its parent's answer at the fork point"
    );
}

#[test]
fn the_predecessor_steps_over_an_entry_without_an_id_another_account_another_key_or_another_root() {
    let mut map = AffinityMap::default();
    for (list, message_id) in [
        (&["b1"][..], Some("msg_01")),
        (&["b1", "b2"][..], Some("msg_02")),
        (&["b1", "b2", "b3"][..], None),
    ] {
        map.record(&AffinityReport {
            root: Some("s1".into()),
            message_id: message_id.map(String::from),
            ..report("k", "a", list, 1_000)
        });
    }
    let all = boundaries(&["b1", "b2", "b3"]);
    assert_eq!(
        map.predecessor(&predecessor_query(&all)),
        Some("msg_02".to_string()),
        "a tail that names no answer is stepped over for the deepest one that does"
    );
    assert_eq!(
        map.predecessor(&PredecessorQuery {
            account_id: "b",
            ..predecessor_query(&all)
        }),
        None,
        "another account holds no answer of this conversation"
    );
    assert_eq!(
        map.predecessor(&PredecessorQuery {
            key_id: "other",
            ..predecessor_query(&all)
        }),
        None,
        "another key never reads this key's answers"
    );
    assert_eq!(
        map.predecessor(&PredecessorQuery {
            key_generation: 1,
            ..predecessor_query(&all)
        }),
        None,
        "a later lifetime of the key never reads an earlier lifetime's answers"
    );
    assert_eq!(
        map.predecessor(&PredecessorQuery {
            roots: &["s2"],
            ..predecessor_query(&all)
        }),
        None,
        "another conversation's identical list names none of this conversation's answers"
    );
    let moved_on = |_: &str| Some(2);
    assert_eq!(
        map.predecessor(&PredecessorQuery {
            incarnation_of: &moved_on,
            ..predecessor_query(&all)
        }),
        None,
        "an answer of the account's earlier incarnation is no answer of its current holder"
    );
}
