use std::collections::HashMap;
use std::sync::Arc;

use phoebis_guardrails::CompiledGuardrail;
use phoebis_store::{
    digest_with, AccountKind, AccountRow, ApiKeyRow, PoolMember, PoolRow, RegistryLoad,
    SubscriptionTerm,
};
use subtle::ConstantTimeEq;

use crate::guardrails::CompiledGuardrails;

/// Which secret the caller presented: the key's live one, or the one a rotation retired and the grace still
/// admits.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum Presented {
    Live,
    Retired,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeyRefusal {
    #[error("the key is disabled")]
    Disabled,
    #[error("the key has expired")]
    Expired,
    #[error("the key's pool is disabled")]
    PoolDisabled,
    #[error("the key's pool does not exist")]
    PoolMissing,
    #[error("the key was replaced under its id")]
    Replaced,
    #[error("the key was revoked")]
    Revoked,
    #[error("the key is not in the registry")]
    Unknown,
}

impl KeyRefusal {
    /// The variant in snake case: the word a refused decision's `code` carries beside the display reason.
    pub fn code(&self) -> &'static str {
        match self {
            KeyRefusal::Disabled => "disabled",
            KeyRefusal::Expired => "expired",
            KeyRefusal::PoolDisabled => "pool_disabled",
            KeyRefusal::PoolMissing => "pool_missing",
            KeyRefusal::Replaced => "replaced",
            KeyRefusal::Revoked => "revoked",
            KeyRefusal::Unknown => "unknown",
        }
    }
}

/// The registry as of one `load_registry`. Replaced whole on every write, never mutated, so a request sees one
/// consistent registry from authentication to selection.
pub struct RegistrySnapshot {
    accounts: HashMap<String, AccountRow>,
    /// Refreshed with the rest of the snapshot, so a report arriving on the hot path can be fenced without a
    /// store read. It lags a rotation by one reload, which the fence allows for.
    credential_generations: HashMap<String, u64>,
    digest_key: [u8; 32],
    guardrails: CompiledGuardrails,
    keys: HashMap<String, ApiKeyRow>,
    keys_by_digest: HashMap<[u8; 32], String>,
    pub loaded_at: i64,
    pub revision: u64,
    members: HashMap<String, Vec<PoolMember>>,
    pools: HashMap<String, PoolRow>,
    /// Retired secrets by digest, with the key each belonged to and the end of its grace; consulted only after
    /// the current digests miss.
    retired_by_digest: HashMap<[u8; 32], (String, i64)>,
    terms: HashMap<String, SubscriptionTerm>,
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
        let mut retired_by_digest = HashMap::with_capacity(load.retired_secrets.len());
        for retired in &load.retired_secrets {
            if let Ok(digest) = <[u8; 32]>::try_from(retired.secret_digest.as_slice()) {
                retired_by_digest.insert(digest, (retired.key_id.clone(), retired.expires_at));
            }
        }
        RegistrySnapshot {
            accounts: load
                .accounts
                .into_iter()
                .map(|a| (a.id.clone(), a))
                .collect(),
            credential_generations: load.credential_generations.into_iter().collect(),
            digest_key,
            guardrails: CompiledGuardrails::default(),
            keys: load.keys.into_iter().map(|k| (k.id.clone(), k)).collect(),
            keys_by_digest,
            loaded_at,
            members,
            revision,
            pools: load.pools.into_iter().map(|p| (p.id.clone(), p)).collect(),
            retired_by_digest,
            terms: load
                .terms
                .into_iter()
                .map(|t| (t.account_id.clone(), t))
                .collect(),
        }
    }

    /// The set compiled from the same load. A builder rather than an argument of `from_load`, so a caller that
    /// builds a snapshot without the compile step keeps its call and carries an empty set.
    pub fn with_guardrails(mut self, guardrails: CompiledGuardrails) -> RegistrySnapshot {
        self.guardrails = guardrails;
        self
    }

    /// The key's guardrails in evaluation order, its pool's first, read from the list the build derived so the
    /// request path walks no attachment.
    pub fn guardrails_for(&self, key: &ApiKeyRow) -> &[Arc<CompiledGuardrail>] {
        self.guardrails.for_key(&key.id)
    }

    pub fn guardrails(&self) -> &CompiledGuardrails {
        &self.guardrails
    }

    /// The digest narrows the search and the constant-time compare decides, as the static key table did. A
    /// secret a rotation retired still names its key until the grace ends, so a running session switches on
    /// its own time rather than on the operator's click.
    pub fn authenticate(
        &self,
        presented: &str,
        now: i64,
    ) -> Result<(&ApiKeyRow, Presented), KeyRefusal> {
        let digest = digest_with(&self.digest_key, presented.as_bytes());
        if let Some(id) = self.keys_by_digest.get(&digest) {
            let key = self.keys.get(id).ok_or(KeyRefusal::Unknown)?;
            if !bool::from(key.secret_digest.as_slice().ct_eq(&digest)) {
                return Err(KeyRefusal::Unknown);
            }
            return self.live(key, now).map(|k| (k, Presented::Live));
        }
        // The map hit narrows and the constant-time compare decides, as on the live path above; the stored
        // digest is the map's own key.
        let (stored, (id, until)) = self
            .retired_by_digest
            .get_key_value(&digest)
            .ok_or(KeyRefusal::Unknown)?;
        if !bool::from(stored.as_slice().ct_eq(&digest)) {
            return Err(KeyRefusal::Unknown);
        }
        if now >= *until {
            return Err(KeyRefusal::Unknown);
        }
        let key = self.keys.get(id).ok_or(KeyRefusal::Unknown)?;
        self.live(key, now).map(|k| (k, Presented::Retired))
    }

    /// The liveness half of `authenticate` for a key the relay already matched under an older snapshot, created at
    /// `key_generation`. The id is reusable, so the lifetime is the identity: a key created again under the id since
    /// the caller matched it is another key, refused with `Replaced` as a refused id is.
    pub fn authenticate_id(
        &self,
        key_id: &str,
        key_generation: i64,
        now: i64,
    ) -> Result<&ApiKeyRow, KeyRefusal> {
        let key = self.keys.get(key_id).ok_or(KeyRefusal::Unknown)?;
        let key = self.live(key, now)?;
        if key.created_at != key_generation {
            return Err(KeyRefusal::Replaced);
        }
        Ok(key)
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
    /// The account under the id while it is still the holder of `incarnation`, `None` once the id is gone or was
    /// deleted and created again.
    pub fn holder(&self, id: &str, incarnation: u64) -> Option<&AccountRow> {
        self.account(id)
            .filter(|account| account.incarnation == incarnation)
    }
    /// The incarnation of the id's current holder, `None` for an id the snapshot no longer holds.
    pub fn incarnation(&self, id: &str) -> Option<u64> {
        self.account(id).map(|account| account.incarnation)
    }
    /// The credential generation this snapshot was loaded with, `None` for an account holding none.
    pub fn credential_generation(&self, account_id: &str) -> Option<u64> {
        self.credential_generations.get(account_id).copied()
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
    /// The pool of the key under `key_id`, `None` while the snapshot holds no such key.
    pub fn key_pool(&self, key_id: &str) -> Option<&PoolRow> {
        self.pool(&self.key(key_id)?.pool_id)
    }
    pub fn pools(&self) -> impl Iterator<Item = &PoolRow> {
        self.pools.values()
    }
    pub fn members(&self, pool_id: &str) -> &[PoolMember] {
        self.members.get(pool_id).map(Vec::as_slice).unwrap_or(&[])
    }
    /// Every pool member with a spend budget, across every pool: what the ledger's member-spend refresh and
    /// the analytics route both need the same walk over.
    pub fn budgeted_members(&self) -> impl Iterator<Item = &PoolMember> {
        self.pools()
            .flat_map(|pool| self.members(&pool.id))
            .filter(|m| m.budgeted())
    }
    pub fn term(&self, account_id: &str) -> Option<&SubscriptionTerm> {
        self.terms.get(account_id)
    }
    /// The rate-limit tier the account's stored profile names, `None` before the first profile read and for an api_key
    /// account. The router prices its own use of the windows by `priced_tier`, which withholds the tier of a plan the
    /// cost table never prices.
    pub fn tier(&self, account_id: &str) -> Option<&str> {
        self.terms.get(account_id).and_then(|t| t.tier.as_deref())
    }
    /// The resolved plan word of the account's term, `None` for an account with no stored document and no tier the
    /// table names.
    pub fn plan(&self, account_id: &str) -> Option<&str> {
        self.terms.get(account_id).and_then(|t| t.plan.as_deref())
    }
    /// The stored tier where the resolved plan is one the cost table may price (`pro`, `max_5x`, `max_20x`, `max`,
    /// `team` or `enterprise`), the table's capacity rows judging the word, `None` for `free`, `usage_based` and an
    /// account with no plan: what every pricing read keys its capacity by. The term's own rule, which the keepalive's
    /// rows read from the store too.
    pub fn priced_tier(&self, account_id: &str) -> Option<&str> {
        self.terms.get(account_id)?.priced_tier()
    }
    /// The term row's class flag, false for an account with no term row.
    pub fn usage_based(&self, account_id: &str) -> bool {
        self.terms.get(account_id).is_some_and(|t| t.usage_based)
    }
    pub fn body_capture_hours(&self, key: &ApiKeyRow) -> u32 {
        key.body_capture_hours.unwrap_or_else(|| {
            self.pools
                .get(&key.pool_id)
                .map(|p| p.body_capture_hours)
                .unwrap_or(0)
        })
    }
    /// The key's override where set, else the pool's; a key whose pool the snapshot lacks reads on, the wire
    /// unchanged.
    pub fn auto_mode_server(&self, key: &ApiKeyRow) -> bool {
        key.auto_mode_server.unwrap_or_else(|| {
            self.pools
                .get(&key.pool_id)
                .is_none_or(|p| p.auto_mode_server)
        })
    }
}

/// The paid sources the economics read: an `api_key` account and a usage-based `oauth` account, whose spend is
/// billed in dollars and whose window is no plan window.
pub fn paid_source(snapshot: &RegistrySnapshot, account_id: &str) -> bool {
    snapshot
        .account(account_id)
        .is_some_and(|a| a.kind == AccountKind::ApiKey)
        || snapshot.usage_based(account_id)
}
