//! Keeps a slot bound to the upstream account it was paired to.

use phoebis_store::Identity;

/// Whether a bundle whose token names the upstream account `account_uuid` may be committed against the paired
/// identity. `account_uuid` is optional because a refresh response may omit the whole `account` object.
///
/// Only the account uuid decides. An email address and an organization can both change while the paired
/// upstream account stays the same, so comparing them would quarantine accounts over a rename or an org
/// move; the uuid is the one stable identifier of the pairing. A token that names no account cannot
/// contradict it and is accepted.
pub fn identity_matches(stored: &Identity, account_uuid: Option<&str>) -> bool {
    account_uuid.is_none_or(|uuid| uuid == stored.account_uuid)
}
