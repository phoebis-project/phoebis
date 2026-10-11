use phoebis_store::ApiKeyRow;

use crate::rules::{allows, process};
use crate::RegistrySnapshot;

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum ResolveError {
    #[error("model not allowed for this key")]
    ModelNotAllowed,
}

impl ResolveError {
    /// The word a refused decision's `code` carries beside the display reason: `model_not_allowed`.
    pub fn code(&self) -> &'static str {
        match self {
            ResolveError::ModelNotAllowed => "model_not_allowed",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub account_id: String,
    /// The holder of the id when the candidates were resolved, which a fresh pin records.
    pub incarnation: u64,
    pub priority: u32,
    pub upstream_model: String,
}

/// Every member of the key's pool, in priority order, once the key's processed list allows `canonical_model`;
/// `upstream_model` is `folded_model`, the spelling the body carries upstream. No member is an empty list, which
/// selection reports as untimed exhaustion.
pub fn resolve(
    snapshot: &RegistrySnapshot,
    key: &ApiKeyRow,
    canonical_model: &str,
    folded_model: &str,
) -> Result<Vec<Candidate>, ResolveError> {
    // The key was authenticated against this same snapshot, so a missing pool is unreachable in practice; an
    // empty candidate list is the same answer selection gives a pool with no member.
    let Some(pool) = snapshot.pool(&key.pool_id) else {
        return Ok(Vec::new());
    };
    let processed = process(&key.model_rules, key.model_rules_mode, &pool.model_rules);
    if !allows(&processed, canonical_model) {
        return Err(ResolveError::ModelNotAllowed);
    }
    Ok(snapshot
        .members(&pool.id)
        .iter()
        .map(|member| Candidate {
            account_id: member.account_id.clone(),
            incarnation: snapshot
                .account(&member.account_id)
                .map_or(0, |account| account.incarnation),
            priority: member.priority,
            upstream_model: folded_model.to_string(),
        })
        .collect())
}
