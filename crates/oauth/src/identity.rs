//! Keeps a slot bound to the upstream account it was paired to.

use phoebis_store::Identity;

use crate::exchange::{ProfileResponse, TokenResponse};

/// Who a token or profile response claims to belong to. Every field is optional because a refresh
/// response may omit the whole `account` object.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IdentityClaims {
    pub account_uuid: Option<String>,
    pub email: Option<String>,
    pub organization_uuid: Option<String>,
}

impl IdentityClaims {
    pub fn from_token(token: &TokenResponse) -> IdentityClaims {
        IdentityClaims {
            account_uuid: token.account.as_ref().map(|a| a.uuid.clone()),
            email: token
                .account
                .as_ref()
                .and_then(|a| a.email_address.as_ref().cloned()),
            organization_uuid: token.organization.as_ref().map(|o| o.uuid.clone()),
        }
    }

    pub fn from_profile(profile: &ProfileResponse) -> IdentityClaims {
        IdentityClaims {
            account_uuid: Some(profile.account_uuid.clone()),
            email: Some(profile.email.clone()),
            organization_uuid: profile.organization_uuid.clone(),
        }
    }
}

/// Whether a bundle carrying these claims may be committed against the paired identity.
///
/// Only the account uuid decides. An email address and an organization can both change while the paired
/// upstream account stays the same, so comparing them would quarantine accounts over a rename or an org
/// move; the uuid is the one stable identifier of the pairing. Claims that name no account cannot
/// contradict it and are accepted.
pub fn identity_matches(stored: &Identity, claims: &IdentityClaims) -> bool {
    match &claims.account_uuid {
        Some(uuid) => uuid == &stored.account_uuid,
        None => true,
    }
}
