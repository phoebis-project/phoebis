//! Admin API: cookie sessions, account views, OAuth pairing, and usage.

mod routes;
mod session;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use phoebis_oauth::{Clock, CredentialManager, PairingSessions, PendingIdentity, UsageService};
use phoebis_profile::Profile;
use phoebis_store::{NewBundle, Store};
use phoebis_transport::control::ControlClient;

pub use routes::router;
pub use session::{AdminSecrets, LoginError, Role, Sessions};

// A poisoned vault mutex means a prior panic happened while a pending bundle was being moved, so what
// is still held, and what was already committed, can no longer be told apart.
const LOCK_POISONED: &str = "pairing vault mutex poisoned by a prior panic";

pub struct AdminState {
    pub store: Arc<Store>,
    pub manager: Arc<CredentialManager>,
    pub usage: Arc<UsageService>,
    pub pairing: Arc<PairingSessions>,
    pub control: Arc<ControlClient>,
    pub profile: &'static Profile,
    pub sessions: Arc<Sessions>,
    pub secrets: AdminSecrets,
    pub clock: Clock,
    pub dev_loopback_http: bool,
    /// The admin origin every mutation's `Origin` header is compared against. `None` only in the
    /// loopback development mode, where there is no browser origin to pin.
    pub origin: Option<String>,
    pub vault: Arc<PairingVault>,
    /// Unix milliseconds this state was built, which `GET /system` reports as uptime.
    pub started_at: i64,
}

/// Credentials exchanged during pairing, held between the callback and the operator's confirmation.
///
/// `PairingSessions` deliberately carries no credential, and a bundle nobody confirms must die with the
/// process rather than reach the store, so the exchange lives here and nowhere else.
pub struct PairingVault {
    records: Mutex<HashMap<String, PairingRecord>>,
}

/// How far a pairing got, as the vault saw it. A disconnect cancels only the sessions still in flight,
/// so a pairing that already ended is never rewritten; the status an operator reads comes from
/// `PairingSessions` instead.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VaultStatus {
    Confirmed,
    /// The exchange was taken out of the vault and not committed, so nothing of it is left.
    Discarded,
    Exchanged,
    Started,
}

/// A pending exchange handed to the confirm route, with the account state the pairing started from.
pub struct TakenExchange {
    pub account_id: String,
    pub bundle: NewBundle,
    pub identity: PendingIdentity,
    pub lifecycle_generation: u64,
}

struct PairingRecord {
    account_id: String,
    admin_session_id: String,
    exchange: Option<Exchange>,
    /// The account's lifecycle generation when the pairing started. A bundle confirmed past a bump was
    /// issued for a slot that has since been disconnected.
    lifecycle_generation: u64,
    status: VaultStatus,
}

struct Exchange {
    bundle: NewBundle,
    identity: PendingIdentity,
}

impl Default for PairingVault {
    fn default() -> Self {
        Self::new()
    }
}

impl PairingVault {
    pub fn new() -> Self {
        PairingVault {
            records: Mutex::new(HashMap::new()),
        }
    }

    pub fn open(
        &self,
        session_id: &str,
        account_id: &str,
        admin_session_id: &str,
        lifecycle_generation: u64,
    ) {
        self.records.lock().expect(LOCK_POISONED).insert(
            session_id.to_string(),
            PairingRecord {
                account_id: account_id.to_string(),
                admin_session_id: admin_session_id.to_string(),
                exchange: None,
                lifecycle_generation,
                status: VaultStatus::Started,
            },
        );
    }

    /// Fails when the record is gone, which is how a disconnect that landed during the exchange takes
    /// the bundle with it: the caller drops what it just fetched.
    pub fn record_exchange(
        &self,
        session_id: &str,
        identity: PendingIdentity,
        bundle: NewBundle,
    ) -> bool {
        let mut records = self.records.lock().expect(LOCK_POISONED);
        let Some(record) = records.get_mut(session_id) else {
            return false;
        };
        record.exchange = Some(Exchange { bundle, identity });
        record.status = VaultStatus::Exchanged;
        true
    }

    /// Removes the bundle from memory and marks the record `Discarded`. Confirmation turns that into
    /// `Confirmed` once every check has passed, so a check that fails anywhere in between leaves nothing
    /// behind to commit later.
    pub fn take_exchange(&self, session_id: &str) -> Option<TakenExchange> {
        let mut records = self.records.lock().expect(LOCK_POISONED);
        let record = records.get_mut(session_id)?;
        let exchange = record.exchange.take();
        record.status = VaultStatus::Discarded;
        let exchange = exchange?;
        Some(TakenExchange {
            account_id: record.account_id.clone(),
            bundle: exchange.bundle,
            identity: exchange.identity,
            lifecycle_generation: record.lifecycle_generation,
        })
    }

    pub fn mark_confirmed(&self, session_id: &str) {
        if let Some(record) = self
            .records
            .lock()
            .expect(LOCK_POISONED)
            .get_mut(session_id)
        {
            record.status = VaultStatus::Confirmed;
        }
    }

    pub fn discard(&self, session_id: &str) {
        if let Some(record) = self
            .records
            .lock()
            .expect(LOCK_POISONED)
            .get_mut(session_id)
        {
            record.exchange = None;
            record.status = VaultStatus::Discarded;
        }
    }

    /// Drops every bundle held for an account and returns the pairing sessions to cancel, each with the
    /// admin session that owns it. Cancelling is left to the caller so no pairing lock is ever taken
    /// while this one is held.
    pub fn discard_for_account(&self, account_id: &str) -> Vec<(String, String)> {
        let mut records = self.records.lock().expect(LOCK_POISONED);
        let mut cancelled = Vec::new();
        for (session_id, record) in records.iter_mut() {
            if record.account_id != account_id {
                continue;
            }
            if matches!(record.status, VaultStatus::Started | VaultStatus::Exchanged) {
                record.exchange = None;
                record.status = VaultStatus::Discarded;
                cancelled.push((session_id.clone(), record.admin_session_id.clone()));
            }
        }
        cancelled
    }
}
