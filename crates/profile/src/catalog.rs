//! The Claude ids the pinned client can name, used to seed an OAuth account whose models endpoint did not answer.
//! The long-context flag is display-only in this revision.

pub struct BuiltInModel {
    pub id: &'static str,
    pub long_context: bool,
}

pub const BUILT_IN: &[BuiltInModel] = &[
    BuiltInModel {
        id: "claude-fable-5-1",
        long_context: true,
    },
    BuiltInModel {
        id: "claude-haiku-4-5-20251001",
        long_context: false,
    },
    BuiltInModel {
        id: "claude-opus-5",
        long_context: true,
    },
    BuiltInModel {
        id: "claude-sonnet-4-5",
        long_context: false,
    },
    BuiltInModel {
        id: "claude-sonnet-5",
        long_context: true,
    },
];

pub fn built_in() -> Vec<(String, bool)> {
    BUILT_IN
        .iter()
        .map(|m| (m.id.to_string(), m.long_context))
        .collect()
}

/// The models request head measured from the one Claude Code release the profile records under its gateway-discovery flag.
pub const MODELS_TARGET: &str = "/v1/models?limit=1000";
pub const MODELS_USER_AGENT: &str = "claude-code/2.1.269";
