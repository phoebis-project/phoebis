//! A store with an OAuth account, an Anthropic-key account, a pool holding both and a key on it, the router over
//! them, and a clock the test moves; the reader spawns on the rig's bus.
#![allow(dead_code)]
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use fake_upstream::FakeUpstream;
use phoebis_attention::{Attention, AttentionHandles, AttentionParts};
use phoebis_bus::{
    Bus, Event, Kind as RequestKind, Origin, Outcome, RequestComplete, RequestObserved,
    RequestShape, ResponseFacts,
};
use phoebis_oauth::{Clock, CredentialManager, UsageService};
use phoebis_profile::claude_code_2_1_269_linux_x64;
use phoebis_routing::{spawn as spawn_router, RouterConfig, RouterHandle};
use phoebis_store::{
    AccountKind, AccountRow, AccountState, Affinity, ApiKeyRow, Failover, Identity,
    MemberWrite, NewBundle, OveragePolicy, PoolRow, RulesMode, Secret, Shedding, StorageKey, Store,
    Strategy,
};
use phoebis_transport::control::{ControlClient, ResolveFuture, Resolver};
use phoebis_transport::{connector_builder, trust_extra_ca, Plane};

pub const NOW: i64 = 1_700_000_000_000;

pub struct Rig {
    pub bus: Bus,
    pub clock: Arc<AtomicI64>,
    pub router: RouterHandle,
    pub store: Arc<Store>,
}

impl Rig {
    pub fn clock(&self) -> Clock {
        let c = self.clock.clone();
        Arc::new(move || c.load(Ordering::SeqCst))
    }

    /// The reader over the rig's store, router and clock, its arm on the rig's bus.
    pub fn attention(
        &self,
        usage: Option<Arc<UsageService>>,
    ) -> (Arc<Attention>, AttentionHandles) {
        phoebis_attention::spawn(AttentionParts {
            bus: self.bus.clone(),
            clock: self.clock(),
            router: self.router.clone(),
            store: self.store.clone(),
            usage,
        })
    }

    pub fn at(&self, ms: i64) {
        self.clock.store(ms, Ordering::SeqCst);
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
    if store.accounts().unwrap().is_empty() {
        seed(&store);
    }
    let clock_cell = Arc::new(AtomicI64::new(NOW));
    let clock: Clock = {
        let c = clock_cell.clone();
        Arc::new(move || c.load(Ordering::SeqCst))
    };
    let router = spawn_router(store.clone(), clock, RouterConfig::default()).unwrap();
    Rig {
        bus: Bus::new(),
        clock: clock_cell,
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
                strategy: Strategy::Priority,
                affinity_ttl_seconds: 3_600,
                overage_policy: OveragePolicy::PreferPlan,
                migrate_above_utilization: None,
                select_below_utilization: None,
                model_rules: Vec::new(),
                keepalive_json: "{}".into(),
                body_capture_hours: 0,
                prefer_subscription: false,
                shedding: Shedding::default(),
                failover: Failover::default(),
                cache_ttl_json: "{}".into(),
                cache_diagnostics: false,
                auto_mode_server: true,
                affinity: Affinity::default(),
                created_at: NOW,
                updated_at: NOW,
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
                created_at: NOW,
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
    let profile = claude_code_2_1_269_linux_x64();
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
    Arc::new(UsageService::new(
        manager,
        control,
        profile,
        Duration::from_secs(60),
        2,
        clock,
    ))
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
        client_version: version.map(String::from),
        response_bytes: None,
        response_chunks: None,
        held: None,
        message_id: None,
        finished_at: at,
    })
}
