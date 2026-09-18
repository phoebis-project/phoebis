//! A store with two accounts and a pool, a fake webhook that records every post and answers what the test
//! scripted, a ledger over an in-memory database, and a clock the test moves.
#![allow(dead_code)]
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use phoebis_alerts::{AlertParts, Alerts, Kind, ScopeType, Signal, Webhook};
use phoebis_bus::Bus;
use phoebis_ledger::{
    spawn as spawn_ledger, Ledger, LedgerConfig, LedgerDb, Metrics, PriceTable, Pricing,
};
use phoebis_oauth::Clock;
use phoebis_routing::{spawn as spawn_router, RouterConfig, RouterHandle};
use phoebis_store::{
    AccountKind, AccountRow, AccountState, AlertRuleRow, ApiKeyRow, OveragePolicy, PoolRow, Secret,
    StorageKey, Store, Strategy,
};

pub const NOW: i64 = 1_700_000_000_000;

#[derive(Clone, Debug)]
pub struct Posted {
    pub body: serde_json::Value,
    pub signature: Option<String>,
    pub url: String,
}

pub struct FakeWebhook {
    pub answers: Mutex<VecDeque<Result<u16, String>>>,
    /// While set, every post waits on it before answering, so a test can hold an open delivery and watch what
    /// queues behind it.
    pub hold: Mutex<Option<Arc<tokio::sync::Notify>>>,
    pub posts: Mutex<Vec<Posted>>,
}

impl FakeWebhook {
    pub fn new() -> Arc<FakeWebhook> {
        Arc::new(FakeWebhook {
            answers: Mutex::new(VecDeque::new()),
            hold: Mutex::new(None),
            posts: Mutex::new(Vec::new()),
        })
    }

    pub fn answer(&self, a: Result<u16, String>) {
        self.answers.lock().unwrap().push_back(a);
    }

    pub fn posts(&self) -> Vec<Posted> {
        self.posts.lock().unwrap().clone()
    }

    /// Holds every later post until the returned notify is signalled and the hold is cleared.
    pub fn hold_posts(&self) -> Arc<tokio::sync::Notify> {
        let n = Arc::new(tokio::sync::Notify::new());
        *self.hold.lock().unwrap() = Some(n.clone());
        n
    }

    pub fn release_posts(&self, n: &Arc<tokio::sync::Notify>) {
        *self.hold.lock().unwrap() = None;
        n.notify_waiters();
    }
}

impl Webhook for FakeWebhook {
    fn post(
        &self,
        url: String,
        body: Vec<u8>,
        signature: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<u16, String>> + Send>> {
        let hold = self.hold.lock().unwrap().clone();
        self.posts.lock().unwrap().push(Posted {
            body: serde_json::from_slice(&body).unwrap(),
            signature,
            url,
        });
        let answer = self.answers.lock().unwrap().pop_front().unwrap_or(Ok(200));
        Box::pin(async move {
            if let Some(hold) = hold {
                hold.notified().await;
            }
            answer
        })
    }
}

pub struct Rig {
    pub alerts: Arc<Alerts>,
    pub bus: Bus,
    pub clock: Arc<AtomicI64>,
    pub db: Arc<LedgerDb>,
    pub ledger: Ledger,
    pub metrics: Arc<Metrics>,
    pub router: RouterHandle,
    pub store: Arc<Store>,
    pub webhook: Arc<FakeWebhook>,
}

pub async fn rig() -> Rig {
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    for (id, kind) in [
        ("business", AccountKind::Oauth),
        ("direct", AccountKind::ApiKey),
    ] {
        store
            .insert_account(&AccountRow {
                id: id.into(),
                kind,
                label: id.into(),
                enabled: true,
                identity: None,
                lifecycle_generation: 1,
                incarnation: 1,
                revision: 1,
                state: AccountState::Ready,
            })
            .unwrap();
    }
    store
        .set_api_key_secret("direct", &Secret::new("sk-ant-api03-PLACEHOLDER"))
        .unwrap();
    store
        .insert_pool(
            &PoolRow {
                id: "main".into(),
                label: "Main".into(),
                enabled: true,
                strategy: Strategy::PriorityRandom,
                affinity_ttl_seconds: 3_600,
                overage_policy: OveragePolicy::PreferPlan,
                migrate_above_utilization: None,
                select_below_utilization: None,
                default_allowed_models: None,
                keepalive_json: r#"{"enabled":true,"probe_cost_cap_usd_per_day":1.0}"#.into(),
                body_capture_hours: 0,
                created_at: NOW,
                updated_at: NOW,
            },
            "test",
        )
        .unwrap();
    store
        .set_pool_members(
            "main",
            &[("business".into(), 0), ("direct".into(), 1)],
            "test",
        )
        .unwrap();
    store
        .insert_api_key(
            &ApiKeyRow {
                id: "desktop".into(),
                label: "Desktop".into(),
                notes: None,
                pool_id: "main".into(),
                enabled: true,
                revoked_at: None,
                expires_at: None,
                allowed_models: None,
                keepalive_opt_out: false,
                body_capture_hours: Some(24),
                secret_digest: store.api_key_digest("phoebis-desktop").to_vec(),
                prefix: "phoebis-desk".into(),
                created_at: NOW,
                rotated_at: None,
                last_used_at: None,
            },
            "test",
        )
        .unwrap();
    let clock_cell = Arc::new(AtomicI64::new(NOW));
    let clock: Clock = {
        let c = clock_cell.clone();
        Arc::new(move || c.load(Ordering::SeqCst))
    };
    let router = spawn_router(store.clone(), None, clock.clone(), RouterConfig::default()).unwrap();
    let bus = Bus::new();
    let db = Arc::new(LedgerDb::open_in_memory().unwrap());
    let pricing = Arc::new(Pricing::new(PriceTable::shipped()));
    let metrics = Arc::new(Metrics::new());
    let ledger = spawn_ledger(
        db.clone(),
        pricing,
        store.clone(),
        None,
        &bus,
        router.clone(),
        clock.clone(),
        metrics.clone(),
        LedgerConfig {
            event_poll: Duration::from_millis(50),
            ..LedgerConfig::default()
        },
    );
    let webhook = FakeWebhook::new();
    // The instant sleeper: a retry test walks the three attempts in milliseconds rather than the real backoff.
    let alerts = Alerts::with_sleeper(
        AlertParts {
            clock,
            ledger: ledger.clone(),
            metrics: metrics.clone(),
            router_name: "phoebis-test".into(),
            store: store.clone(),
            webhook: webhook.clone(),
        },
        Box::new(|_| Box::pin(async {})),
    );
    Rig {
        alerts,
        bus,
        clock: clock_cell,
        db,
        ledger,
        metrics,
        router,
        store,
        webhook,
    }
}

pub fn rule(id: &str, kind: Kind, scope: ScopeType, scope_id: Option<&str>) -> AlertRuleRow {
    AlertRuleRow {
        id: id.into(),
        kind: kind.as_str().into(),
        scope_type: serde_json::to_value(scope)
            .unwrap()
            .as_str()
            .unwrap()
            .into(),
        scope_id: scope_id.map(String::from),
        threshold: None,
        window: None,
        webhook_url: format!("http://127.0.0.1:1/{id}"),
        has_secret: false,
        enabled: true,
        created_at: NOW,
        updated_at: NOW,
    }
}

/// `enabled` is not a parameter of `rule` because one test in the crate turns a rule off.
pub trait RowExt {
    fn with_enabled(self, enabled: bool) -> AlertRuleRow;
}

impl RowExt for AlertRuleRow {
    fn with_enabled(self, enabled: bool) -> AlertRuleRow {
        AlertRuleRow { enabled, ..self }
    }
}

pub fn signal(
    kind: Kind,
    account: Option<&str>,
    key: Option<&str>,
    pool: Option<&str>,
    subject: &str,
) -> Signal {
    Signal {
        kind,
        account_id: account.map(String::from),
        key_id: key.map(String::from),
        pool_id: pool.map(String::from),
        subject: subject.into(),
        detail: serde_json::json!({}),
    }
}

/// Every delivery task the engine spawned has landed its rows.
pub async fn settled(rig: &Rig) {
    rig.alerts.wait_idle().await;
}
