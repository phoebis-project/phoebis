//! The ordered rule list a key or a pool is judged by: a key's own entries first, then the pool's under
//! `Extend`; `allows` walks the list top to bottom and the first matching entry decides.

pub use phoebis_store::{ModelRule, RulesMode};

use crate::pattern;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleLevel {
    Key,
    Pool,
}

impl RuleLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            RuleLevel::Key => "key",
            RuleLevel::Pool => "pool",
        }
    }
}

/// One entry of a processed list, in the order it applies, with the level it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessedRule {
    pub level: RuleLevel,
    pub rule: ModelRule,
}

/// The ordered list a key is judged by: its own entries, then the pool's under `Extend`, nothing more under
/// `Replace`.
pub fn process(
    key_rules: &[ModelRule],
    key_mode: RulesMode,
    pool_rules: &[ModelRule],
) -> Vec<ProcessedRule> {
    let mut processed: Vec<ProcessedRule> = key_rules
        .iter()
        .cloned()
        .map(|rule| ProcessedRule {
            level: RuleLevel::Key,
            rule,
        })
        .collect();
    if key_mode == RulesMode::Extend {
        processed.extend(pool_rules.iter().cloned().map(|rule| ProcessedRule {
            level: RuleLevel::Pool,
            rule,
        }));
    }
    processed
}

/// Top to bottom, first match wins: a matching `not` entry denies, a matching plain entry allows, and a name no
/// entry matches is allowed.
pub fn allows(processed: &[ProcessedRule], canonical: &str) -> bool {
    for entry in processed {
        if pattern::matches(&entry.rule.pattern, canonical) {
            return !entry.rule.not;
        }
    }
    true
}
