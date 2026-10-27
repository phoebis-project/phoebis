//! A store with an OAuth account, an API-key account, a pool holding both and a key on it, the router over
//! them, a ledger over an in-memory file and the ledger's test cost model, and a clock the test moves. The reader
//! spawns on the rig's bus.
#![allow(dead_code)]
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use fake_upstream::FakeUpstream;
use phoebis_attention::{Attention, AttentionHandles, AttentionParts, PairFacts, TlsPairs};
use phoebis_bus::{
    Bus, Event, Kind as RequestKind, Origin, Outcome, RequestComplete, RequestObserved,
    RequestShape, ResponseFacts,
};
use phoebis_ledger::{
    CostRevisions, Ledger, LedgerConfig, LedgerDb, Metrics, PlanTables, PriceRevisions, Pricing,
};
use phoebis_oauth::{Clock, CredentialManager, UsageService};
use phoebis_profile::claude_code_2_1_310_linux_x64;
use phoebis_routing::{spawn as spawn_router, RouterConfig, RouterHandle};
use phoebis_store::{
    AccountKind, AccountRow, AccountState, ApiKeyRow, Identity, MemberWrite, NewBundle, PoolRow,
    RulesMode, Secret, StateCause, StorageKey, Store, Strategy,
};
use phoebis_transport::control::{ControlClient, ResolveFuture, Resolver};
use phoebis_transport::{connector_builder, trust_extra_ca, Plane};

pub const NOW: i64 = 1_700_000_000_000;

/// The ledger's test cost model as text, since this crate's tests build the ledger without its test tables. Its
/// first entry is in force at `NOW` and leaves `claude-opus-5` unweighted under the `opus` model tier, whose latest
/// weighted model is `claude-opus-5-5`.
pub const COSTS: &str = include_str!("../../../ledger/tests/fixtures/subscription_costs.yaml");

pub fn tables_of(text: &str) -> PlanTables {
    PlanTables {
        revisions: CostRevisions::from_yaml(text).expect("the cost list parses"),
        path: None,
        source: "test".to_string(),
    }
}

pub struct Rig {
    pub bus: Bus,
    pub clock: Arc<AtomicI64>,
    /// The ledger's file, which a test writes its parked rows and its faults through.
    pub db: Arc<LedgerDb>,
    pub ledger: Ledger,
    pub router: RouterHandle,
    pub store: Arc<Store>,
}

impl Rig {
    pub fn clock(&self) -> Clock {
        let c = self.clock.clone();
        Arc::new(move || c.load(Ordering::SeqCst))
    }

    /// The reader over the rig's store, router, ledger and clock, its arm on the rig's bus.
    pub fn attention(
        &self,
        usage: Option<Arc<UsageService>>,
    ) -> (Arc<Attention>, AttentionHandles) {
        self.attention_with_pairs(usage, None)
    }

    /// The same reader over a listener set the case holds, in place of the router's loaded pairs.
    pub fn attention_with_pairs(
        &self,
        usage: Option<Arc<UsageService>>,
        tls: Option<Arc<dyn TlsPairs>>,
    ) -> (Arc<Attention>, AttentionHandles) {
        phoebis_attention::spawn(AttentionParts {
            bus: self.bus.clone(),
            clock: self.clock(),
            ledger: Some(self.ledger.clone()),
            router: self.router.clone(),
            store: self.store.clone(),
            tls,
            usage,
        })
    }

    pub fn at(&self, ms: i64) {
        self.clock.store(ms, Ordering::SeqCst);
    }
}

/// The listeners' pairs as a case sets them, read whole on every call as the router's set is.
#[derive(Default)]
pub struct Pairs(pub Mutex<Vec<PairFacts>>);

impl Pairs {
    pub fn set(&self, pairs: Vec<PairFacts>) {
        *self.0.lock().unwrap() = pairs;
    }
}

impl TlsPairs for Pairs {
    fn pairs(&self) -> Vec<PairFacts> {
        self.0.lock().unwrap().clone()
    }
}

pub async fn rig() -> Rig {
    rig_with_store(Arc::new(
        Store::open_in_memory(&StorageKey::generate()).unwrap(),
    ))
    .await
}

/// The rig over a store the test opened itself, for the case that reads the store again after a restart.
pub async fn rig_with_store(store: Arc<Store>) -> Rig {
    rig_on(store, tables_of(COSTS)).await
}

/// The rig over a cost model the test wrote, for the case that needs a revision the fixture lacks.
pub async fn rig_with_tables(tables: PlanTables) -> Rig {
    rig_on(
        Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap()),
        tables,
    )
    .await
}

async fn rig_on(store: Arc<Store>, tables: PlanTables) -> Rig {
    if store.accounts().unwrap().is_empty() {
        seed(&store);
    }
    let clock_cell = Arc::new(AtomicI64::new(NOW));
    let clock: Clock = {
        let c = clock_cell.clone();
        Arc::new(move || c.load(Ordering::SeqCst))
    };
    let router = spawn_router(store.clone(), clock.clone(), RouterConfig::default()).unwrap();
    let bus = Bus::new();
    let db = Arc::new(LedgerDb::open_in_memory().unwrap());
    let ledger = phoebis_ledger::spawn(
        db.clone(),
        Arc::new(Pricing::new(PriceRevisions::shipped())),
        Arc::new(tables),
        store.clone(),
        &bus,
        router.clone(),
        clock,
        Arc::new(Metrics::new()),
        LedgerConfig::default(),
    );
    Rig {
        bus,
        clock: clock_cell,
        db,
        ledger,
        router,
        store,
    }
}

fn seed(store: &Store) {
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
                state_cause: StateCause::Operator,
            })
            .unwrap();
    }
    store
        .set_api_key_secret("direct", &Secret::new("sk-ant-api03-PLACEHOLDER"))
        .unwrap();
    store
        .insert_pool(
            &PoolRow {
                label: "Main".into(),
                strategy: Strategy::Priority,
                prefer_subscription: false,
                created_at: NOW,
                updated_at: NOW,
                ..PoolRow::for_tests("main")
            },
            "test",
        )
        .unwrap();
    store
        .set_pool_members(
            "main",
            &[
                MemberWrite {
                    account_id: "business".into(),
                    priority: 0,
                    budget_daily_usd: None,
                    budget_monthly_usd: None,
                },
                MemberWrite {
                    account_id: "direct".into(),
                    priority: 1,
                    budget_daily_usd: None,
                    budget_monthly_usd: None,
                },
            ],
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
                model_rules: Vec::new(),
                model_rules_mode: RulesMode::Extend,
                long_context_twins: true,
                keepalive_opt_out: false,
                body_capture_hours: None,
                secret_digest: store.api_key_digest("phoebis-desktop").to_vec(),
                prefix: "phoebis-desk".into(),
                incarnation: 1,
                created_at: NOW,
                updated_at: 0,
                rotated_at: None,
                last_used_at: None,
                retired_secret_last_used_at: None,
                auto_mode_server: None,
            },
            "test",
        )
        .unwrap();
}

/// A usage service reading `up` on the rig's clock, with `business` given an identity and a token far from
/// expiry so a poll can read clean. No scheduler runs: the test reads the endpoint through `poll`.
pub fn usage_over(rig: &Rig, up: &FakeUpstream) -> Arc<UsageService> {
    rig.store
        .set_identity(
            "business",
            &Identity {
                account_uuid: "00000000-0000-4000-8000-000000000001".into(),
                email: "business@example.invalid".into(),
                organization_uuid: None,
            },
        )
        .unwrap();
    rig.store
        .commit_credentials(
            "business",
            1,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new("sk-ant-oat01-PLACEHOLDER"),
                refresh_token: Some(Secret::new("sk-ant-ort01-PLACEHOLDER")),
                issued_at: NOW,
                expires_at: i64::MAX / 2,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    let profile = claude_code_2_1_310_linux_x64();
    let addr = up.addr();
    let resolver: Resolver =
        Arc::new(move |_: &str| Box::pin(async move { Some(addr) }) as ResolveFuture);
    let mut builder = connector_builder(profile, Plane::Control).unwrap();
    trust_extra_ca(&mut builder, up.ca_pem()).unwrap();
    let control = Arc::new(ControlClient::new(profile, builder.build(), resolver));
    let clock: Clock = {
        let c = rig.clock.clone();
        Arc::new(move || c.load(Ordering::SeqCst))
    };
    let manager = Arc::new(CredentialManager::new(
        rig.store.clone(),
        control.clone(),
        profile,
        clock.clone(),
    ));
    Arc::new(UsageService::new(manager, control, profile, 2, clock))
}

/// A client inference's observation, which carries the key and pool its completion does not; `None` for a refusal
/// published before the key was read.
pub fn observed(id: &str, key: Option<&str>, pool: &str) -> Event {
    observed_as(id, key, pool, Origin::Client, RequestKind::Inference)
}

pub fn observed_as(
    id: &str,
    key: Option<&str>,
    pool: &str,
    origin: Origin,
    kind: RequestKind,
) -> Event {
    Event::RequestObserved(RequestObserved {
        request_id: id.into(),
        origin,
        kind,
        key_id: key.map(String::from),
        key_generation: 0,
        pool_id: Some(pool.into()),
        session_id: Some("s1".into()),
        agent_id: Some("main".into()),
        parent_agent_id: None,
        owner_id: None,
        model: "claude-sonnet-5".into(),
        canonical_model: "claude-sonnet-5".into(),
        wire_model: "claude-sonnet-5".into(),
        stream: true,
        headers: vec![],
        body: Bytes::from_static(b"{}"),
        body_capture_hours: 0,
        shape: RequestShape::default(),
        static_hash: None,
        boundaries: Vec::new(),
        client: phoebis_bus::ClientFacts::default(),
        auto_mode: None,
        fork: phoebis_bus::Fork::default(),
        presented_retired: false,
        rotation_at: None,
        guardrail_id: None,
        guardrails: Vec::new(),
        received_at: NOW,
    })
}

/// A completion at `at`, refused for drift with that trigger when `drift` names one.
pub fn complete(
    id: &str,
    outcome: Outcome,
    drift: Option<&str>,
    version: Option<&str>,
    at: i64,
) -> Event {
    Event::RequestComplete(RequestComplete {
        request_id: id.into(),
        attempt: 1,
        account_id: Some("business".into()),
        outcome,
        status: 200,
        usage: None,
        response: ResponseFacts::default(),
        ttfb_ms: None,
        ttfc_ms: None,
        error_code: None,
        client_drift: drift.map(String::from),
        client_contract: None,
        client_version: version.map(String::from),
        response_bytes: None,
        response_chunks: None,
        held: None,
        diagnostics: None,
        message_id: None,
        skipped_events: 0,
        interruption: None,
        finished_at: at,
    })
}
