use phoebis_routing::rules::{allows, process};
use phoebis_store::{ModelRule, RulesMode};
use serde_json::Value;

const CASES: &str = include_str!("fixtures/model_rules_cases.json");

fn cases() -> Value {
    serde_json::from_str(CASES).expect("fixture parses")
}

fn group(name: &str) -> Vec<Value> {
    cases()["evaluator"][name]
        .as_array()
        .unwrap_or_else(|| panic!("missing evaluator group {name}"))
        .clone()
}

fn rules_of(case: &Value, field: &str) -> Vec<ModelRule> {
    serde_json::from_value(case[field].clone()).expect("rule list parses")
}

fn assert_case(case: &Value) {
    let name = case["name"].as_str().expect("name");
    let allowed = case["allowed"].as_bool().expect("allowed");
    let mode = RulesMode::parse(case["mode"].as_str().expect("mode"))
        .unwrap_or_else(|| panic!("unknown mode {:?}", case["mode"]));
    let processed = process(&rules_of(case, "key"), mode, &rules_of(case, "pool"));
    assert_eq!(allows(&processed, name), allowed, "name {name:?}");
    if let Some(levels) = case["levels"].as_array() {
        let got: Vec<&str> = processed.iter().map(|p| p.level.as_str()).collect();
        let want: Vec<&str> = levels
            .iter()
            .map(|v| v.as_str().expect("level string"))
            .collect();
        assert_eq!(got, want, "processed levels for {name:?}");
    }
}

#[test]
fn a_keys_own_entry_is_ahead_of_the_pools() {
    for case in group("a_keys_own_entry_is_ahead_of_the_pools") {
        assert_case(&case);
    }
}

#[test]
fn an_empty_list_at_both_levels_allows_everything() {
    for case in group("an_empty_list_at_both_levels_allows_everything") {
        assert_case(&case);
    }
}

#[test]
fn first_match_wins_and_an_unmatched_name_is_allowed() {
    for case in group("first_match_wins_and_an_unmatched_name_is_allowed") {
        assert_case(&case);
    }
}

#[test]
fn replace_drops_the_pools_list() {
    for case in group("replace_drops_the_pools_list") {
        assert_case(&case);
    }
}

#[test]
fn the_dated_id_glob_covers_the_spelling_the_id_and_the_twin() {
    for case in group("the_dated_id_glob_covers_the_spelling_the_id_and_the_twin") {
        assert_case(&case);
    }
}

/// The fixture's `a/b` rows carry the no-special-case point: `*` stops at the slash so `NOT *` allows it, `**`
/// crosses it so `NOT **` denies it.
#[test]
fn the_documented_examples_build_an_allowlist_and_a_blocklist() {
    for case in group("the_documented_examples_build_an_allowlist_and_a_blocklist") {
        assert_case(&case);
    }
}
