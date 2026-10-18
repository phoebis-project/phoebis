use std::sync::LazyLock;

use crate::condition::{Condition, GuardrailMode, Operator, Selector, Subject};

pub struct Preset {
    pub id: &'static str,
    pub version: u32,
    pub name: &'static str,
    pub description: &'static str,
    pub measured_on: &'static str,
    pub suggested_mode: GuardrailMode,
    pub conditions: Vec<Condition>,
}

// A shipped entry's conditions are never edited: a change to them is a new entry with the next version, since a
// stamped guardrail compares its conditions against the version it was created from. The other fields read from the
// current entry (the name and the suggested mode are copied at creation and never compared, the description and the
// measured range are the listing's text), so they are edited in place.
static PRESETS: LazyLock<Vec<Preset>> = LazyLock::new(|| {
    vec![
        Preset {
            id: "subagent-status-summary",
            version: 1,
            name: "Subagent status summary",
            description: concat!(
                "Claude Code forks each running subagent's whole conversation every 30 seconds to ask for a status ",
                "line of three to five words. Each fork reads the subagent's full cached context for about 24 output ",
                "tokens, and no client setting turns it off. Blocking the forks leaves the Agent tool's status line ",
                "static and changes nothing else.",
            ),
            measured_on: "Claude Code 2.1.296 to 2.1.302",
            suggested_mode: GuardrailMode::Observe,
            conditions: vec![
                Condition {
                    subject: Subject::RequestClass,
                    operator: Operator::Equals,
                    value: Some("auxiliary".to_string()),
                    name: None,
                    path: None,
                    selector: None,
                },
                Condition {
                    subject: Subject::Body,
                    operator: Operator::Present,
                    value: None,
                    name: None,
                    path: Some("tools".to_string()),
                    selector: None,
                },
                Condition {
                    subject: Subject::Message,
                    operator: Operator::Matches,
                    value: Some("^Describe your most recent action in 3-5 words".to_string()),
                    name: None,
                    path: None,
                    selector: Some(Selector::LastUser),
                },
            ],
        },
        Preset {
            id: "prompt-suggestion",
            version: 1,
            name: "Prompt suggestion",
            description: concat!(
                "In an interactive session Claude Code forks the main thread's whole conversation to predict what ",
                "the user types next and shows the answer in the input. Each fork reads the thread's full cached ",
                "context for about seven output tokens. The setting promptSuggestionEnabled turns the feature off ",
                "on one machine.",
            ),
            measured_on: "Claude Code 2.1.296 to 2.1.301",
            suggested_mode: GuardrailMode::Observe,
            conditions: vec![
                Condition {
                    subject: Subject::RequestClass,
                    operator: Operator::Equals,
                    value: Some("auxiliary".to_string()),
                    name: None,
                    path: None,
                    selector: None,
                },
                Condition {
                    subject: Subject::Body,
                    operator: Operator::Present,
                    value: None,
                    name: None,
                    path: Some("tools".to_string()),
                    selector: None,
                },
                Condition {
                    subject: Subject::Message,
                    operator: Operator::Matches,
                    value: Some("^\\[SUGGESTION MODE: Suggest what the user might naturally type next".to_string()),
                    name: None,
                    path: None,
                    selector: Some(Selector::LastUser),
                },
            ],
        },
        Preset {
            id: "away-recap",
            version: 1,
            name: "Away recap",
            description: concat!(
                "When the user steps away and comes back, Claude Code forks the main thread's whole conversation ",
                "to ask for a recap of the session in under 40 words. Each fork reads the thread's full cached ",
                "context for about 90 output tokens. The setting awaySummaryEnabled turns the feature off on one ",
                "machine.",
            ),
            measured_on: "Claude Code 2.1.296 to 2.1.301",
            suggested_mode: GuardrailMode::Observe,
            conditions: vec![
                Condition {
                    subject: Subject::RequestClass,
                    operator: Operator::Equals,
                    value: Some("auxiliary".to_string()),
                    name: None,
                    path: None,
                    selector: None,
                },
                Condition {
                    subject: Subject::Body,
                    operator: Operator::Present,
                    value: None,
                    name: None,
                    path: Some("tools".to_string()),
                    selector: None,
                },
                Condition {
                    subject: Subject::Message,
                    operator: Operator::Matches,
                    value: Some("^The user stepped away and is coming back\\. Recap in under 40 words".to_string()),
                    name: None,
                    path: None,
                    selector: Some(Selector::LastUser),
                },
            ],
        },
    ]
});

pub fn presets() -> &'static [Preset] {
    &PRESETS
}

impl Preset {
    pub fn current(id: &str) -> Option<&'static Preset> {
        presets()
            .iter()
            .filter(|preset| preset.id == id)
            .max_by_key(|preset| preset.version)
    }

    pub fn at(id: &str, version: u32) -> Option<&'static Preset> {
        presets()
            .iter()
            .find(|preset| preset.id == id && preset.version == version)
    }
}
