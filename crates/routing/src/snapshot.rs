use std::collections::HashMap;

use phoebis_store::{
    digest_with, AccountRow, ApiKeyRow, PoolMember, PoolRow, RegistryLoad, ServedModel,
};
use subtle::ConstantTimeEq;

#[derive(Debug, thiserror::Error)]
pub enum KeyRefusal {
    #[error("the key is disabled")]
    Disabled,
    #[error("the key has expired")]
    Expired,
    #[error("the key's pool is disabled")]
    PoolDisabled,
    #[error("the key's pool does not exist")]
    PoolMissing,
    #[error("the key was revoked")]
    Revoked,
    #[error("the key is not in the registry")]
    Unknown,
}

/// The registry as of one `load_registry`. Replaced whole on every write, never mutated, so a request sees one
/// consistent registry from authentication to selection.
pub struct RegistrySnapshot {
    accounts: HashMap<String, AccountRow>,
    digest_key: [u8; 32],
    keys: HashMap<String, ApiKeyRow>,
    keys_by_digest: HashMap<[u8; 32], String>,
    pub loaded_at: i64,
    pub revision: u64,
    members: HashMap<String, Vec<PoolMember>>,
    models: HashMap<String, HashMap<String, ServedModel>>,
    pools: HashMap<String, PoolRow>,
}

impl RegistrySnapshot {
    pub fn from_load(
        load: RegistryLoad,
        digest_key: [u8; 32],
        loaded_at: i64,
        revision: u64,
    ) -> RegistrySnapshot {
        let mut keys_by_digest = HashMap::with_capacity(load.keys.len());
        for key in &load.keys {
            if let Ok(digest) = <[u8; 32]>::try_from(key.secret_digest.as_slice()) {
                keys_by_digest.insert(digest, key.id.clone());
            }
        }
        let mut members: HashMap<String, Vec<PoolMember>> = HashMap::new();
        for member in load.members {
            members
                .entry(member.pool_id.clone())
                .or_default()
                .push(member);
        }
        for list in members.values_mut() {
            list.sort_by(|a, b| {
                a.priority
                    .cmp(&b.priority)
                    .then_with(|| a.account_id.cmp(&b.account_id))
            });
        }
        let mut models: HashMap<String, HashMap<String, ServedModel>> = HashMap::new();
        for model in load.models.into_iter().filter(|m| m.selected) {
            models
                .entry(model.account_id.clone())
                .or_default()
                .insert(model.canonical_id.clone(), model);
        }
        RegistrySnapshot {
            accounts: load
                .accounts
                .into_iter()
                .map(|a| (a.id.clone(), a))
                .collect(),
            digest_key,
            keys: load.keys.into_iter().map(|k| (k.id.clone(), k)).collect(),
            keys_by_digest,
            loaded_at,
            members,
            revision,
            models,
            pools: load.pools.into_iter().map(|p| (p.id.clone(), p)).collect(),
        }
    }

    /// The digest narrows the search and the constant-time compare decides, as the static key table did.
    pub fn authenticate(&self, presented: &str, now: i64) -> Result<&ApiKeyRow, KeyRefusal> {
        let digest = digest_with(&self.digest_key, presented.as_bytes());
        let id = self
            .keys_by_digest
            .get(&digest)
            .ok_or(KeyRefusal::Unknown)?;
        let key = self.keys.get(id).ok_or(KeyRefusal::Unknown)?;
        if !bool::from(key.secret_digest.as_slice().ct_eq(&digest)) {
            return Err(KeyRefusal::Unknown);
        }
        self.live(key, now)
    }

    /// The liveness half of `authenticate` for a key the relay already matched under an older snapshot.
    pub fn authenticate_id(&self, key_id: &str, now: i64) -> Result<&ApiKeyRow, KeyRefusal> {
        let key = self.keys.get(key_id).ok_or(KeyRefusal::Unknown)?;
        self.live(key, now)
    }

    fn live<'a>(&'a self, key: &'a ApiKeyRow, now: i64) -> Result<&'a ApiKeyRow, KeyRefusal> {
        if key.revoked_at.is_some() {
            return Err(KeyRefusal::Revoked);
        }
        if !key.enabled {
            return Err(KeyRefusal::Disabled);
        }
        if key.expires_at.is_some_and(|at| at <= now) {
            return Err(KeyRefusal::Expired);
        }
        match self.pools.get(&key.pool_id) {
            None => Err(KeyRefusal::PoolMissing),
            Some(pool) if !pool.enabled => Err(KeyRefusal::PoolDisabled),
            Some(_) => Ok(key),
        }
    }

    pub fn account(&self, id: &str) -> Option<&AccountRow> {
        self.accounts.get(id)
    }
    pub fn accounts(&self) -> impl Iterator<Item = &AccountRow> {
        self.accounts.values()
    }
    pub fn key(&self, id: &str) -> Option<&ApiKeyRow> {
        self.keys.get(id)
    }
    pub fn keys(&self) -> impl Iterator<Item = &ApiKeyRow> {
        self.keys.values()
    }
    pub fn pool(&self, id: &str) -> Option<&PoolRow> {
        self.pools.get(id)
    }
    pub fn pools(&self) -> impl Iterator<Item = &PoolRow> {
        self.pools.values()
    }
    pub fn members(&self, pool_id: &str) -> &[PoolMember] {
        self.members.get(pool_id).map(Vec::as_slice).unwrap_or(&[])
    }
    pub fn served(&self, account_id: &str, canonical_id: &str) -> Option<&ServedModel> {
        self.models.get(account_id)?.get(canonical_id)
    }
    pub fn body_capture_hours(&self, key: &ApiKeyRow) -> u32 {
        key.body_capture_hours.unwrap_or_else(|| {
            self.pools
                .get(&key.pool_id)
                .map(|p| p.body_capture_hours)
                .unwrap_or(0)
        })
    }
}
