//! The token each account's feature calls last saw rejected behind their own forced refresh. No entry leaves the map:
//! a token of another generation ends a hold by the compare, the account's next hold replaces its entry, and the map
//! keeps one entry at most for each account that served a feature call, so the module has no sweep and no timer.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use phoebis_store::AccountRow;

/// One token of one sign-in: the account's incarnation and lifecycle generation beside the token's own generation,
/// since a pairing behind a disconnect and an account created again under its id both start the count at 1.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rejected {
    incarnation: u64,
    lifecycle_generation: u64,
    generation: u64,
}

impl Rejected {
    /// The token of `generation` under the sign-in `account` stands in.
    pub(crate) fn of(account: &AccountRow, generation: u64) -> Rejected {
        Rejected {
            incarnation: account.incarnation,
            lifecycle_generation: account.lifecycle_generation,
            generation,
        }
    }
}

/// For each account the one token whose resend a feature host answered with a 401 that asks for the credential once
/// more. A host that rejects each token of an account then costs one forced refresh for each token, where each call
/// would cost one. The memory is the listener's own and marks nothing on the account.
#[derive(Default)]
pub(crate) struct Holds {
    held: Mutex<HashMap<String, Rejected>>,
}

impl Holds {
    /// Whether `token` is the one held for `account_id`.
    pub(crate) fn holds(&self, account_id: &str, token: Rejected) -> bool {
        let held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        held.get(account_id) == Some(&token)
    }

    /// Holds `token` for `account_id` in place of the one held before.
    pub(crate) fn set(&self, account_id: &str, token: Rejected) {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        held.insert(account_id.to_owned(), token);
    }
}
