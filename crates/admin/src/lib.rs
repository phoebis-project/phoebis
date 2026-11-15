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
mod lifetime;
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
mod session_spend;
mod sessions;
mod settings;
mod tls;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use phoebis_bus::Bus;
use phoebis_keepalive::Engine;
use phoebis_ledger::{Ledger, Metrics, Pricing, QuotaPoll};
use phoebis_oauth::{
    Clock, CredentialManager, PairingSessions, PendingIdentity, ProfileResponse, UsageService,
};
use phoebis_profile::Profile;
use phoebis_relay::CatalogService;
use phoebis_routing::RouterHandle;
use phoebis_store::{NewBundle, Store};
use phoebis_transport::control::ControlClient;

pub use guardrails::SAMPLE_PAGE;
pub use keepalive::EngineFacts;
pub use lifetime::OldTotals;
pub use pricing::table_envelopes;
pub use readiness::Readiness;
pub use routes::path_count;
pub use routes::router;
pub use session::{LoginError, Sessions};
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
    /// The commit the binary was built from, or `unknown`: every answer carries it as `x-phoebis-build`, and the
    /// system and settings answers as `build`.
    pub build: &'static str,
    pub router: RouterHandle,
    pub catalog: Arc<CatalogService>,
    /// The longest body capture a route may ask for. A capture holds request and response bodies on disk, so
    /// the ceiling is the operator's, not the caller's.
    pub body_capture_max_hours: u32,
    /// How long ago a pin's `last_used_at` may be and still count as live for `GET /admin/sessions`.
    pub session_expiry: Duration,
    /// How long ago the latest of a non-companion session's newest pin use, its last request and its companion's end
    /// may be and still list the row, bounded above by `session_expiry`, and how long ago a line's pin may have been
    /// used on such a row.
    pub non_companion_idle: Duration,
    pub bus: Bus,
    pub ledger: Ledger,
    /// The rows older than the week that the account and key lifetimes add to their week, cached per route so a poll
    /// walks them once an hour.
    pub old_totals: OldTotals,
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
    pub claims: Arc<Claims>,
}

impl AdminState {
    /// Drops what the process still holds of the account's identity once the store has deleted its row or bumped its
    /// lifecycle: the manager's bundle, the catalog's listing, the usage entry and the vault's pairings, each pairing
    /// session canceled with it.
    pub(crate) fn forget_account(&self, id: &str, now: i64) {
        self.drop_credential_caches(id);
        for (session_id, owner) in self.vault.discard_for_account(id, now) {
            let _ = self.pairing.cancel(&session_id, &owner, now);
        }
    }

    /// Drops the caches a committed credential makes stale: the manager's bundle, the catalog's listing and the usage
    /// entry.
    pub(crate) fn drop_credential_caches(&self, id: &str) {
        // The store's write took the bundle outside the manager, whose copy the usage service and the catalog, asking
        // for no generation, would be served until it neared its expiry.
        self.manager.invalidate(id);
        self.catalog.evict(id);
        self.usage.invalidate(id);
    }

    /// The quota poll over the handles this state holds, the ones the scheduler polls through.
    pub(crate) fn quota_poll(&self) -> QuotaPoll {
        QuotaPoll {
            ledger: self.ledger.clone(),
            router: self.router.clone(),
            store: self.store.clone(),
            tables: self.tables.clone(),
            usage: self.usage.clone(),
        }
    }
}

/// The limit reset claims whose first stage is on its way: the request, the fold of its answer and its event. A stop
/// waits for the set before the store and the ledger close, since a claim's request cannot be taken back and its
/// record must not be lost.
pub struct Claims {
    running: tokio::sync::watch::Sender<usize>,
}

impl Default for Claims {
    fn default() -> Claims {
        Claims {
            running: tokio::sync::watch::channel(0).0,
        }
    }
}

impl Claims {
    /// Joins the set until the guard is dropped, which a return, a panic and an abort all do.
    pub(crate) fn begin(&self) -> ClaimRunning {
        self.running.send_modify(|running| *running += 1);
        ClaimRunning {
            running: self.running.clone(),
        }
    }

    /// Waits until no first stage is on its way.
    pub async fn drain(&self) {
        let mut running = self.running.subscribe();
        // The sender lives in `self`, so the wait cannot end on a closed channel.
        let _ = running.wait_for(|running| *running == 0).await;
    }
}

pub(crate) struct ClaimRunning {
    running: tokio::sync::watch::Sender<usize>,
}

impl Drop for ClaimRunning {
    fn drop(&mut self) {
        self.running.send_modify(|running| *running -= 1);
    }
}

/// The running configuration an operator reads back from the admin API. Every secret is left out, and so is
/// the name of the variable each one is read from: a console that shows this view must not be able to point
/// at where a credential lives.
pub struct SettingsView {
    pub api_prices_path: Option<String>,
    pub ledger_path: String,
    /// `usage.limit_reset_warning_days`, 0 for never.
    pub limit_reset_warning_days: u32,
    pub metrics_listen: String,
    pub proxy_listen: String,
    /// Whether the proxy listener terminates TLS, the console's URL scheme.
    pub proxy_tls: bool,
    pub retention_days: u32,
    pub rollup_retention_days: u32,
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
enum Phase {
    /// Confirmed or discarded: the exchange was taken out of the vault, so nothing of it is left. `at` is the first
    /// end, which the sweep counts the record's last lifetime from.
    Ended {
        at: i64,
    },
    Exchanged(Box<Exchange>),
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
    /// The account's lifecycle generation when the pairing started. A bundle confirmed past a bump was
    /// issued for a slot that has since been disconnected.
    lifecycle_generation: u64,
    /// The row the confirm creates, for a pairing started with `new`.
    new_account: Option<NewAccount>,
    phase: Phase,
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

    pub fn open(
        &self,
        session_id: &str,
        account_id: &str,
        admin_session_id: &str,
        lifecycle_generation: u64,
        deadline: i64,
        new_account: Option<NewAccount>,
    ) {
        self.records.lock().expect(LOCK_POISONED).insert(
            session_id.to_string(),
            PairingRecord {
                account_id: account_id.to_string(),
                admin_session_id: admin_session_id.to_string(),
                deadline,
                lifecycle_generation,
                new_account,
                phase: Phase::Started,
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
    ) -> bool {
        let mut records = self.records.lock().expect(LOCK_POISONED);
        let Some(record) = records.get_mut(session_id) else {
            return false;
        };
        if !matches!(record.phase, Phase::Started) {
            return false;
        }
        record.phase = Phase::Exchanged(Box::new(Exchange {
            bundle,
            identity,
            profile,
        }));
        true
    }

    /// Removes the bundle from memory and ends the record, so a check of the confirm that fails anywhere after leaves
    /// nothing behind to commit later.
    pub fn take_exchange(&self, session_id: &str, now: i64) -> Option<TakenExchange> {
        let mut records = self.records.lock().expect(LOCK_POISONED);
        let record = records.get_mut(session_id)?;
        let exchange = record.end(now)?;
        Some(TakenExchange {
            account_id: record.account_id.clone(),
            bundle: exchange.bundle,
            identity: exchange.identity,
            lifecycle_generation: record.lifecycle_generation,
            new_account: record.new_account.clone(),
            profile: exchange.profile,
        })
    }

    pub fn discard(&self, session_id: &str, now: i64) {
        if let Some(record) = self
            .records
            .lock()
            .expect(LOCK_POISONED)
            .get_mut(session_id)
        {
            record.end(now);
        }
    }

    /// Drops every bundle held for an account and returns the pairing sessions to cancel, each with the
    /// admin session that owns it. Canceling is left to the caller so no pairing lock is ever taken
    /// while this one is held.
    pub fn discard_for_account(&self, account_id: &str, now: i64) -> Vec<(String, String)> {
        let mut records = self.records.lock().expect(LOCK_POISONED);
        let mut canceled = Vec::new();
        for (session_id, record) in records.iter_mut() {
            if record.account_id != account_id {
                continue;
            }
            if matches!(record.phase, Phase::Started | Phase::Exchanged(_)) {
                record.end(now);
                canceled.push((session_id.clone(), record.admin_session_id.clone()));
            }
        }
        canceled
    }

    /// Drops every pairing whose session has run out: the bundle it may hold can never be confirmed, and
    /// nothing else would free it before the process ends. The record goes with the bundle, so the vault does
    /// not keep an entry per pairing ever started, and an exchange that lands afterwards cannot revive it:
    /// `record_exchange` fails for a record that is gone, and its caller drops what it fetched. An ended record holds
    /// no bundle and leaves one pairing lifetime after its first end.
    pub fn sweep(&self, now: i64) -> usize {
        let mut records = self.records.lock().expect(LOCK_POISONED);
        let before = records.len();
        records.retain(|_, record| match record.phase {
            Phase::Started | Phase::Exchanged(_) => now <= record.deadline,
            Phase::Ended { at } => now < at + phoebis_oauth::SESSION_TTL_MS,
        });
        before - records.len()
    }
}

impl PairingRecord {
    /// Ends an in-flight record at `now` and hands back the bundle it held. An ended record keeps the stamp of its
    /// first end, which the sweep counts its lifetime from, so a second discard (a callback's, when the owner canceled
    /// while its exchange ran) leaves its lifetime where the first end put it.
    fn end(&mut self, now: i64) -> Option<Exchange> {
        match std::mem::replace(&mut self.phase, Phase::Ended { at: now }) {
            Phase::Exchanged(exchange) => Some(*exchange),
            Phase::Started => None,
            ended @ Phase::Ended { .. } => {
                self.phase = ended;
                None
            }
        }
    }
}
