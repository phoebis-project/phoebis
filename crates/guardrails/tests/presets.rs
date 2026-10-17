use std::collections::BTreeMap;

use phoebis_guardrails::{
    evaluate, presets, CompiledGuardrail, GuardrailAction, GuardrailMode, HeaderLookup, Lookup,
    Preset, RequestFacts, Verdict,
};
use serde_json::Value;

const FORK: &str = include_str!("fixtures/subagent_summary_fork.json");
const CLASSIFIER: &str = include_str!("fixtures/auto_mode_classifier.json");

struct NoHeaders;

impl HeaderLookup for NoHeaders {
    fn get(&self, _name: &str) -> Lookup<'_> {
        Lookup::Absent
    }
}

fn auxiliary(body: &Value) -> RequestFacts<'_> {
    RequestFacts {
        kind: "inference",
        request_class: Some("auxiliary"),
        agent_type: None,
        client: "claude-code/cli",
        model: "claude-opus-5",
        user_agent: Some("claude-cli/2.1.300 (external, cli)"),
        headers: &NoHeaders,
        body: Some(body),
    }
}

fn compiled(preset: &Preset) -> CompiledGuardrail {
    CompiledGuardrail::compile(
        preset.id,
        preset.name,
        preset.suggested_mode,
        GuardrailAction::Block,
        &preset.conditions,
    )
    .unwrap_or_else(|error| panic!("{} version {} compiles: {error}", preset.id, preset.version))
}

#[test]
fn every_preset_of_every_version_checks_and_compiles_under_the_size_limit() {
    assert!(!presets().is_empty());
    for preset in presets() {
        for (index, condition) in preset.conditions.iter().enumerate() {
            if let Err(reason) = condition.check() {
                panic!(
                    "{} version {} conditions[{index}]: {reason}",
                    preset.id, preset.version
                );
            }
        }
        compiled(preset);
    }
}

#[test]
fn the_first_preset_is_subagent_status_summary_version_one_in_observe_mode_with_its_three_conditions(
) {
    let first = &presets()[0];
    assert_eq!(first.id, "subagent-status-summary");
    assert_eq!(first.version, 1);
    assert_eq!(first.suggested_mode, GuardrailMode::Observe);
    assert_eq!(first.conditions.len(), 3);
}

#[test]
fn the_first_preset_matches_the_fork_fixture_and_not_the_classifier_fixture() {
    let guardrail = compiled(&presets()[0]);
    let fork: Value = serde_json::from_str(FORK).expect("the fork fixture parses");
    let classifier: Value =
        serde_json::from_str(CLASSIFIER).expect("the classifier fixture parses");
    assert_eq!(evaluate(&guardrail, &auxiliary(&fork)), Verdict::Matched);
    assert!(matches!(
        evaluate(&guardrail, &auxiliary(&classifier)),
        Verdict::Unmatched { .. }
    ));
}

#[test]
fn versions_of_one_id_are_distinct_and_ascending_and_current_and_at_answer_the_table() {
    let mut versions: BTreeMap<&str, Vec<u32>> = BTreeMap::new();
    for preset in presets() {
        versions.entry(preset.id).or_default().push(preset.version);
    }
    for (id, stamped) in &versions {
        assert!(
            stamped.windows(2).all(|pair| pair[0] < pair[1]),
            "{id}: {stamped:?}"
        );
        let highest = *stamped.last().expect("an id holds a version");
        let current = Preset::current(id).expect("a shipped id has a current version");
        assert_eq!((current.id, current.version), (*id, highest));
        for &version in stamped {
            let at = Preset::at(id, version).expect("a shipped version answers");
            assert_eq!((at.id, at.version), (*id, version));
        }
        assert!(Preset::at(id, highest + 1).is_none());
    }
    assert!(Preset::current("not-a-preset").is_none());
    assert!(Preset::at("not-a-preset", 1).is_none());
}
