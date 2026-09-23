use phoebis_store::ApiKeyRow;

use crate::RegistrySnapshot;

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum ResolveError {
    #[error("model not allowed for this key")]
    ModelNotAllowed,
    #[error("no account in this pool serves the model")]
    ModelUnserved,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub account_id: String,
    /// The holder of the id when the candidates were resolved, which a fresh pin records.
    pub incarnation: u64,
    pub priority: u32,
    pub upstream_model: String,
}

pub fn resolve(
    snapshot: &RegistrySnapshot,
    key: &ApiKeyRow,
    canonical_model: &str,
) -> Result<Vec<Candidate>, ResolveError> {
    let pool = snapshot
        .pool(&key.pool_id)
        .ok_or(ResolveError::ModelUnserved)?;
    let policy = key
        .allowed_models
        .as_ref()
        .or(pool.default_allowed_models.as_ref());
    if let Some(list) = policy {
        if !list.iter().any(|allowed| allowed == canonical_model) {
            return Err(ResolveError::ModelNotAllowed);
        }
    }
    let candidates: Vec<Candidate> = snapshot
        .members(&pool.id)
        .iter()
        .filter_map(|member| {
            snapshot
                .served(&member.account_id, canonical_model)
                .map(|row| Candidate {
                    account_id: member.account_id.clone(),
                    incarnation: snapshot
                        .account(&member.account_id)
                        .map_or(0, |account| account.incarnation),
                    priority: member.priority,
                    upstream_model: row.upstream_id.clone(),
                })
        })
        .collect();
    if candidates.is_empty() {
        return Err(ResolveError::ModelUnserved);
    }
    Ok(candidates)
}
