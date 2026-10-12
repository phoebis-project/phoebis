//! The oauth tiers the shares price under, read once per route call.

use phoebis_ledger::Tiers;
use phoebis_store::{AccountKind, AccountRow, Store, StoreError};

/// `accounts` as the caller read them, and one `subscription()` per oauth account among them. A store failure is
/// the caller's 500.
pub(crate) fn oauth_tiers(store: &Store, accounts: &[AccountRow]) -> Result<Tiers, StoreError> {
    let mut tiers = Tiers::new();
    for account in accounts.iter().filter(|a| a.kind == AccountKind::Oauth) {
        let tier = store.subscription(&account.id)?.and_then(|s| s.tier);
        tiers.insert(account.id.clone(), tier);
    }
    Ok(tiers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoebis_store::{AccountState, StateCause, StorageKey, SubscriptionRow};

    fn account(id: &str, kind: AccountKind) -> AccountRow {
        AccountRow {
            id: id.into(),
            kind,
            label: id.into(),
            enabled: true,
            identity: None,
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state: AccountState::Ready,
            state_cause: StateCause::Operator,
        }
    }

    #[test]
    fn an_oauth_account_answers_its_tier_and_an_api_key_account_answers_none() {
        let store = Store::open_in_memory(&StorageKey::generate()).unwrap();
        store
            .insert_account(&account("plan", AccountKind::Oauth))
            .unwrap();
        store
            .insert_account(&account("dollars", AccountKind::ApiKey))
            .unwrap();
        store
            .set_profile(
                "plan",
                "{}",
                &SubscriptionRow {
                    tier: Some("test_tier".into()),
                    ..SubscriptionRow::default()
                },
            )
            .unwrap();
        assert_eq!(
            oauth_tiers(&store, &store.accounts().unwrap()).unwrap(),
            Tiers::from([("plan".to_string(), Some("test_tier".to_string()))]),
        );
    }
}
