use std::collections::BTreeMap;

use phoebis_guardrails::{
    evaluate, presets, CompiledGuardrail, GuardrailAction, HeaderLookup, Lookup, Preset,
    RequestFacts, Verdict,
};
use serde_json::Value;

const FORK: &str = include_str!("fixtures/subagent_summary_fork.json");
const SUGGESTION: &str = include_str!("fixtures/prompt_suggestion_fork.json");
const RECAP: &str = include_str!("fixtures/away_recap_fork.json");
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
    }
    // The table ships each id at version 1 alone.
    for id in ["away-recap", "prompt-suggestion", "subagent-status-summary"] {
        let current = Preset::current(id).expect("a shipped id has a current version");
        assert_eq!((current.id, current.version), (id, 1));
        let at = Preset::at(id, 1).expect("a shipped version answers");
        assert_eq!((at.id, at.version), (id, 1));
        assert!(Preset::at(id, 2).is_none());
    }
    assert!(Preset::current("not-a-preset").is_none());
    assert!(Preset::at("not-a-preset", 1).is_none());
}

#[test]
fn each_preset_matches_its_own_fork_fixture_and_none_of_the_other_bodies() {
    let body = |name: &'static str, text: &str| -> (&'static str, Value) {
        (
            name,
            serde_json::from_str(text).expect("the fixture parses"),
        )
    };
    let summary = body("subagent-status-summary", FORK);
    let suggestion = body("prompt-suggestion", SUGGESTION);
    let recap = body("away-recap", RECAP);
    let classifier = body("classifier", CLASSIFIER);
    for (id, own, others) in [
        (
            "subagent-status-summary",
            &summary,
            [&suggestion, &recap, &classifier],
        ),
        (
            "prompt-suggestion",
            &suggestion,
            [&summary, &recap, &classifier],
        ),
        ("away-recap", &recap, [&summary, &suggestion, &classifier]),
    ] {
        let preset = Preset::current(id).unwrap_or_else(|| panic!("{id} is not in the table"));
        let guardrail = compiled(preset);
        assert_eq!(
            evaluate(&guardrail, &auxiliary(&own.1)),
            Verdict::Matched,
            "{id} on its own fork"
        );
        for (name, body) in others {
            let verdict = evaluate(&guardrail, &auxiliary(body));
            assert!(
                matches!(verdict, Verdict::Unmatched { .. }),
                "{id} on the {name} body: {verdict:?}"
            );
        }
    }
}

// A create from a preset writes the preset's name past the route's name checks, so the table holds each name to them.
#[test]
fn every_presets_name_passes_the_create_routes_five_rules() {
    for preset in presets() {
        assert!(
            preset.name.len() <= 256,
            "{}: the name is {} bytes",
            preset.id,
            preset.name.len()
        );
        assert!(!preset.name.is_empty(), "{}: the name is empty", preset.id);
        assert!(
            !preset.name.to_lowercase().contains("prompt is too long"),
            "{}: the name carries the compaction sentence",
            preset.id
        );
        assert!(
            !preset.name.to_lowercase().contains("model:"),
            "{}: the name carries the client's model-access phrase",
            preset.id
        );
        assert!(
            !preset
                .name
                .to_lowercase()
                .contains("oauth token has been revoked"),
            "{}: the name carries the client's token-refresh phrase",
            preset.id
        );
    }
}
