//! Parsing shared by the pool and key routes for the `model_rules` and `model_rules_mode` fields: every pattern
//! is checked through the routing crate's own grammar, so a rule an admin route accepts is one `resolve` can
//! always evaluate.

use axum::http::StatusCode;
use axum::response::Response;
use phoebis_routing::pattern;
use phoebis_store::{ModelRule, RulesMode};

use crate::routes::failure;

#[derive(serde::Deserialize)]
pub(crate) struct RuleInput {
    pub pattern: String,
    #[serde(default)]
    pub not: bool,
}

/// The most entries one level holds, so a list stays readable in the console and cheap to walk per request.
pub(crate) const MAX_RULES: usize = 256;

/// Every pattern through `check`; the refusal names the entry and carries the error's own text.
// The error is a rendered HTTP response, returned once per request and dropped; boxing it would buy a
// pointer chase and nothing else.
#[allow(clippy::result_large_err)]
pub(crate) fn parse_rules(input: &[RuleInput]) -> Result<Vec<ModelRule>, Response> {
    if input.len() > MAX_RULES {
        return Err(failure(
            StatusCode::BAD_REQUEST,
            "model_rules holds at most 256 entries",
        ));
    }
    input
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            pattern::check(&entry.pattern)
                .map(|()| ModelRule {
                    pattern: entry.pattern.clone(),
                    not: entry.not,
                })
                .map_err(|error| {
                    failure(
                        StatusCode::BAD_REQUEST,
                        &format!("model_rules[{i}]: {error}"),
                    )
                })
        })
        .collect()
}

#[allow(clippy::result_large_err)]
pub(crate) fn parse_mode(value: &str) -> Result<RulesMode, Response> {
    RulesMode::parse(value).ok_or_else(|| {
        failure(
            StatusCode::BAD_REQUEST,
            "model_rules_mode is extend or replace",
        )
    })
}
