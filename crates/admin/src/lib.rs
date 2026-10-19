//! Admin API: cookie sessions, account views, OAuth pairing, and usage.

// The request row literal in `requests.rs::request_summary` lists enough fields that `json!`'s macro
// recursion outgrows the default.
#![recursion_limit = "256"]

mod accounts;
mod analytics;
mod attention;
mod dashboard;
mod guardrails;
mod keepalive;
mod keys;
mod ledger;
mod models;
mod openapi;
mod pools;
mod pricing;
mod readiness;
mod requests;
mod routes;
mod routing_log;
mod rules;
mod session;
mod sessions;
mod settings;
mod tls;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use phoebis_bus::Bus;
use phoebis_keepalive::{Engine, KeepaliveLimits};
use phoebis_ledger::{Ledger, Metrics, Pricing};
use phoebis_oauth::{
    Clock, CredentialManager, PairingSessions, PendingIdentity, ProfileResponse, UsageService,
};
use phoebis_profile::Profile;
use phoebis_relay::CatalogService;
use phoebis_routing::RouterHandle;
use phoebis_store::{NewBundle, Store};
use phoebis_transport::control::ControlClient;

pub use guardrails::SAMPLE_PAGE;
pub use pricing::table_envelopes;
pub use readiness::{Readiness, ReadinessSource};
pub use routes::path_count;
pub use routes::router;
pub use session::{AdminSecrets, LoginError, Sessions};
pub use tls::{Reloaded, TlsReload};

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
    /// Whether the admin listener terminates TLS, which decides the session cookie's `Secure` flag and makes a
    /// presented `Origin` need `https://`.
    pub tls: bool,
    /// The router's pairs, for the reload route; `None` where a harness builds no listeners, which answers an
    /// empty list.
    pub tls_reload: Option<Arc<dyn TlsReload>>,
    pub vault: Arc<PairingVault>,
    /// Unix milliseconds this state was built, which `GET /system` reports as uptime.
    pub started_at: i64,
    pub router: RouterHandle,
    pub catalog: Arc<CatalogService>,
    /// The longest body capture a route may ask for. A capture holds request and response bodies on disk, so
    /// the ceiling is the operator's, not the caller's.
    pub body_capture_max_hours: u32,
    /// How long ago a pin's `last_used_at` may be and still count as live for `GET /admin/sessions`.
    pub session_expiry: Duration,
    pub bus: Bus,
    pub ledger: Ledger,
    pub metrics: Arc<Metrics>,
    pub pricing: Arc<Pricing>,
    /// The subscription cost model in force, the one copy the router's burn rates and the keepalive read too, so the
    /// settings echo and a session's burn price through what the rest of the process prices through.
    pub tables: Arc<phoebis_ledger::PlanTables>,
    pub settings: Arc<SettingsView>,
    pub keepalive: Arc<Engine>,
    /// The condition reader the metrics listener scrapes too, so the attention route and a scrape never disagree.
    pub attention: Arc<phoebis_attention::Attention>,
    /// The same reads `/api/ready` answers from, so `GET /admin/system` and the proxy listener agree.
    pub readiness: Arc<Readiness>,
}

/// The running configuration an operator reads back from the admin API. Every secret is left out, and so is
/// the name of the variable each one is read from: a console that shows this view must not be able to point
/// at where a credential lives.
#[derive(Clone, Debug, serde::Serialize)]
pub struct SettingsView {
    pub api_prices_path: Option<String>,
    pub body_capture_max_hours: u32,
    /// Not answered by the settings route: the pool and session routes read it to tell whether a cooldown the
    /// upstream gave no length for still stands.
    pub cooldown_fallback_seconds: u64,
    /// Not answered by the settings route: the sessions overview reads it to answer each cap beside its count.
    pub keepalive: KeepaliveLimits,
    pub ledger_path: String,
    pub metrics_listen: String,
    pub profile: String,
    pub proxy_listen: String,
    /// Whether the proxy listener terminates TLS, the console's URL scheme.
    pub proxy_tls: bool,
    pub session_expire_after_seconds: u64,
    pub storage_path: String,
    pub subscription_costs_path: Option<String>,
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
    /// Set for a pairing started with `new`: the row the confirm creates. `None` for an existing row.
    pub new_account: Option<NewAccount>,
    pub profile: ProfileResponse,
}

/// The row a pairing started with `new` creates at its confirm. Until then the id is held here, in memory, so a
/// pairing nobody confirms leaves no row and the id free.
#[derive(Clone, Debug)]
pub struct NewAccount {
    pub id: String,
    pub label: String,
}

struct PairingRecord {
    account_id: String,
    admin_session_id: String,
    /// Unix milliseconds the pairing session expires at, which is also when the bundle it may hold becomes
    /// unusable.
    deadline: i64,
    exchange: Option<Exchange>,
    /// The account's lifecycle generation when the pairing started. A bundle confirmed past a bump was
    /// issued for a slot that has since been disconnected.
    lifecycle_generation: u64,
    /// The row the confirm creates, for a pairing started with `new`.
    new_account: Option<NewAccount>,
    status: VaultStatus,
    /// When `status` last moved; an ended record leaves one pairing lifetime after it.
    status_at: i64,
}

struct Exchange {
    bundle: NewBundle,
    identity: PendingIdentity,
    profile: ProfileResponse,
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

    // The record's fields, passed once by the start route: a parameter struct would be the record itself.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        &self,
        session_id: &str,
        account_id: &str,
        admin_session_id: &str,
        lifecycle_generation: u64,
        deadline: i64,
        now: i64,
        new_account: Option<NewAccount>,
    ) {
        self.records.lock().expect(LOCK_POISONED).insert(
            session_id.to_string(),
            PairingRecord {
                account_id: account_id.to_string(),
                admin_session_id: admin_session_id.to_string(),
                deadline,
                exchange: None,
                lifecycle_generation,
                new_account,
                status: VaultStatus::Started,
                status_at: now,
            },
        );
    }

    /// Fails when the record is gone, or no longer `Started`: a disconnect that landed during the exchange discarded
    /// it, and a record already exchanged or confirmed takes no second bundle. The caller drops what it just fetched.
    pub fn record_exchange(
        &self,
        session_id: &str,
        identity: PendingIdentity,
        bundle: NewBundle,
        profile: ProfileResponse,
        now: i64,
    ) -> bool {
        let mut records = self.records.lock().expect(LOCK_POISONED);
        let Some(record) = records.get_mut(session_id) else {
            return false;
        };
        if record.status != VaultStatus::Started {
            return false;
        }
        record.exchange = Some(Exchange {
            bundle,
            identity,
            profile,
        });
        record.move_to(VaultStatus::Exchanged, now);
        true
    }

    /// Removes the bundle from memory and marks the record `Discarded`. Confirmation turns that into
    /// `Confirmed` once every check has passed, so a check that fails anywhere in between leaves nothing
    /// behind to commit later.
    pub fn take_exchange(&self, session_id: &str, now: i64) -> Option<TakenExchange> {
        let mut records = self.records.lock().expect(LOCK_POISONED);
        let record = records.get_mut(session_id)?;
        let exchange = record.exchange.take();
        record.move_to(VaultStatus::Discarded, now);
        let exchange = exchange?;
        Some(TakenExchange {
            account_id: record.account_id.clone(),
            bundle: exchange.bundle,
            identity: exchange.identity,
            lifecycle_generation: record.lifecycle_generation,
            new_account: record.new_account.clone(),
            profile: exchange.profile,
        })
    }

    pub fn mark_confirmed(&self, session_id: &str, now: i64) {
        if let Some(record) = self
            .records
            .lock()
            .expect(LOCK_POISONED)
            .get_mut(session_id)
        {
            record.move_to(VaultStatus::Confirmed, now);
        }
    }

    pub fn discard(&self, session_id: &str, now: i64) {
        if let Some(record) = self
            .records
            .lock()
            .expect(LOCK_POISONED)
            .get_mut(session_id)
        {
            record.exchange = None;
            record.move_to(VaultStatus::Discarded, now);
        }
    }

    /// Drops every bundle held for an account and returns the pairing sessions to cancel, each with the
    /// admin session that owns it. Cancelling is left to the caller so no pairing lock is ever taken
    /// while this one is held.
    pub fn discard_for_account(&self, account_id: &str, now: i64) -> Vec<(String, String)> {
        let mut records = self.records.lock().expect(LOCK_POISONED);
        let mut cancelled = Vec::new();
        for (session_id, record) in records.iter_mut() {
            if record.account_id != account_id {
                continue;
            }
            if matches!(record.status, VaultStatus::Started | VaultStatus::Exchanged) {
                record.exchange = None;
                record.move_to(VaultStatus::Discarded, now);
                cancelled.push((session_id.clone(), record.admin_session_id.clone()));
            }
        }
        cancelled
    }

    /// Drops every pairing whose session has run out: the bundle it may hold can never be confirmed, and
    /// nothing else would free it before the process ends. The record goes with the bundle, so the vault does
    /// not keep an entry per pairing ever started, and an exchange that lands afterwards cannot revive it:
    /// `record_exchange` fails for a record that is gone, and its caller drops what it fetched. A confirmed or
    /// discarded record holds no bundle and leaves one pairing lifetime after its status moved.
    pub fn sweep(&self, now: i64) -> usize {
        let mut records = self.records.lock().expect(LOCK_POISONED);
        let before = records.len();
        records.retain(|_, record| match record.status {
            VaultStatus::Started | VaultStatus::Exchanged => now <= record.deadline,
            VaultStatus::Confirmed | VaultStatus::Discarded => {
                now < record.status_at + phoebis_oauth::SESSION_TTL_MS
            }
        });
        before - records.len()
    }
}

impl PairingRecord {
    /// An ended record keeps the stamp of its first end, which the sweep counts its lifetime from, so a second
    /// discard (a callback's, when the owner cancelled while its exchange ran) leaves its lifetime where the first end
    /// put it.
    fn move_to(&mut self, status: VaultStatus, now: i64) {
        if matches!(self.status, VaultStatus::Started | VaultStatus::Exchanged) {
            self.status_at = now;
        }
        self.status = status;
    }
}
