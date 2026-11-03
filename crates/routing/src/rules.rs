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
    ordered(key_rules, key_mode, pool_rules)
        .map(|(level, rule)| ProcessedRule {
            level,
            rule: rule.clone(),
        })
        .collect()
}

/// The entries `process` lists, in its order, borrowed, for a caller that judges one name and keeps no list.
pub(crate) fn ordered<'a>(
    key_rules: &'a [ModelRule],
    key_mode: RulesMode,
    pool_rules: &'a [ModelRule],
) -> impl Iterator<Item = (RuleLevel, &'a ModelRule)> {
    let pool_rules: &[ModelRule] = if key_mode == RulesMode::Extend {
        pool_rules
    } else {
        &[]
    };
    key_rules
        .iter()
        .map(|rule| (RuleLevel::Key, rule))
        .chain(pool_rules.iter().map(|rule| (RuleLevel::Pool, rule)))
}

/// Top to bottom, first match wins: a matching `not` entry denies, a matching plain entry allows, and a name no
/// entry matches is allowed.
pub fn allows(processed: &[ProcessedRule], canonical: &str) -> bool {
    allowed_by(processed.iter().map(|entry| &entry.rule), canonical)
}

/// The first-match rule `allows` applies, over entries in the order they apply.
pub(crate) fn allowed_by<'a>(
    rules: impl IntoIterator<Item = &'a ModelRule>,
    canonical: &str,
) -> bool {
    for rule in rules {
        if pattern::matches(&rule.pattern, canonical) {
            return !rule.not;
        }
    }
    true
}
