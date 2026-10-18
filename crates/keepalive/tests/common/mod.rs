//! The fake-clock rig: an engine over the relay harness, a bus the test pumps by hand so every event reaches
//! the engine before the assertion that follows, and a ledger for the accounting tests.
#![allow(dead_code)]

#[path = "../../../relay/tests/common/mod.rs"]
pub mod relay_common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use bytes::Bytes;
use fake_upstream::{FakeUpstream, Respond, Rule};
use phoebis_bus::{
    AccountSelected, Bus, Delivery, Event as BusEvent, Kind, Origin, Outcome, RequestComplete,
    RequestObserved, RequestShape, ResponseChunk, ResponseFacts, Subscriber, Usage,
};
use phoebis_keepalive::{
    Ack, Engine, EngineParts, Event, EventError, KeepaliveLimits, KeepalivePolicy, KeepaliveTiming,
};
use phoebis_ledger::{
    spawn as spawn_ledger, AttemptRecord, Ledger, LedgerConfig, LedgerDb, Metrics, PlanTables,
    PriceRevisions, Pricing, Record, RequestRecord,
};
use phoebis_oauth::Clock;
use phoebis_relay::AccountingSource;
use phoebis_store::{StorageKey, Store};
// Re-exported for every test binary that shares this rig; only some of them, this one included, reach for all
// of `oauth`, `wait_for`, and `NOW` themselves.
#[allow(unused_imports)]
pub use relay_common::{api_key, oauth, wait_for, PoolSpec, TestAccount, KEY, NOW};
// A test drives the harness's app directly and reads its body, so the traits behind both have to reach it too.
pub use http_body_util::BodyExt;
pub use tower::ServiceExt;

pub const A: &str = "sk-ant-api03-AAAA";
pub const B: &str = "sk-ant-api03-BBBB";
pub const MINUTE: i64 = 60_000;

/// The request body the Go rig replayed: adaptive thinking, one five-minute marker, one message, behind the
/// attribution line the client sends at `system[0]`.
pub const BODY: &str = r#"{"model":"claude-sonnet-4-5","max_tokens":1024,"stream":true,"thinking":{"type":"adaptive"},"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=sdk-cli;"},{"type":"text","text":"private prompt","cache_control":{"type":"ephemeral"}}],"messages":[{"role":"user","content":"hello"}],"metadata":{"user_id":"{\"device_id\":\"d\",\"account_uuid\":\"\",\"session_id\":\"session\"}"}}"#;

pub fn probe_answer(read: u64, write: u64) -> Respond {
    Respond::Json {
        status: 200,
        headers: vec![],
        body: format!(r#"{{"id":"msg_p","type":"message","content":[],"usage":{{"input_tokens":0,"cache_read_input_tokens":{read},"cache_creation_input_tokens":{write},"cache_creation":{{"ephemeral_5m_input_tokens":{write},"ephemeral_1h_input_tokens":0}},"output_tokens":0}}}}"#).into_bytes(),
    }
}

/// A real request's answer when a test wants one that differs from the standing probe answer: a five-minute
/// write of 2,000 tokens and a little output. Queue it with `push_credential_rule` on the account the router
/// will pick.
pub fn real_answer() -> Respond {
    Respond::Json {
        status: 200,
        headers: vec![],
        body: br#"{"id":"msg_r","type":"message","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":10,"cache_creation_input_tokens":2000,"cache_creation":{"ephemeral_5m_input_tokens":2000,"ephemeral_1h_input_tokens":0},"output_tokens":10}}"#.to_vec(),
    }
}

pub struct Rig {
    pub bus: Bus,
    pub clock: Arc<std::sync::atomic::AtomicI64>,
    pub db: Arc<LedgerDb>,
    pub engine: Arc<Engine>,
    pub h: relay_common::Harness,
    /// Session id to the instance its last accepted `session_start` through `event` registered.
    instances: BTreeMap<String, String>,
    pub ledger: Ledger,
    /// The spec's limits, kept so `restart` builds its engine with what this one was built with.
    pub limits: KeepaliveLimits,
    pub metrics: Arc<Metrics>,
    /// The spec's price table, kept so `restart` builds its engine with what this one was built with.
    pub pricing: PriceRevisions,
    pub sequence: u64,
    /// The registry file of a rig built `on_disk`, which a restart test opens beside the store.
    pub store_path: Option<PathBuf>,
    pub sub: Subscriber,
    /// The spec's tables, kept so `restart` builds its engine with what this one was built with.
    pub tables: Arc<PlanTables>,
    /// The spec's timing, kept so `restart` builds its engine with what this one was built with.
    pub timing: KeepaliveTiming,
    pub up: FakeUpstream,
    /// Last, so the directory goes after every holder of the store inside it has let go.
    _scratch: Option<Scratch>,
}

/// A directory of the rig's own for an on-disk registry, removed with the rig.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Scratch {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "phoebis-keepalive-rig-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        // A run that died before its drop leaves its directory behind, whose registry would read as this rig's.
        let _ = std::fs::remove_dir_all(&path);
        Scratch(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub struct RigSpec {
    /// The ledger's standing the relay's admission reads, `None` to admit every probe; a case that drives the
    /// router's own `ledger_behind` refusal names a reading it can flip.
    pub accounting: Option<AccountingSource>,
    pub accounts: Vec<TestAccount>,
    /// What the catalog fake answers; nothing by default, so a spelling never resolves to a dated id mid-test.
    pub catalog: Vec<Rule>,
    pub limits: KeepaliveLimits,
    /// The registry in a file of its own rather than in memory, for a test that reaches it past the store.
    pub on_disk: bool,
    pub policy: KeepalivePolicy,
    pub pool: PoolSpec,
    /// The shipped table by default. A ceiling test writes its own rows so a break-even follows from the text, and a
    /// case on a row priced by prompt length hands in the test table.
    pub pricing: PriceRevisions,
    pub session_expiry: Duration,
    /// The shipped tables by default; a test naming a model the cost model has no weight for (though the price
    /// table prices it) overrides this so the gap is deliberate rather than a coincidence of the shipped tables.
    pub tables: PlanTables,
    /// The subscription tier every OAuth account's stored profile names, `None` to store none: a plan-funded
    /// probe settles in the share of the tier's window, and in dollars on an account without one.
    pub tier: Option<&'static str>,
    pub timing: KeepaliveTiming,
}

impl Default for RigSpec {
    fn default() -> Self {
        RigSpec {
            accounting: None,
            accounts: vec![api_key("a", A, 0), api_key("b", B, 1)],
            catalog: Vec::new(),
            limits: KeepaliveLimits::default(),
            on_disk: false,
            policy: KeepalivePolicy {
                enabled: true,
                ..KeepalivePolicy::default()
            },
            pool: PoolSpec::default(),
            pricing: PriceRevisions::shipped(),
            session_expiry: Duration::from_secs(86_400),
            tables: PlanTables::shipped(),
            tier: None,
            // The Go rig's 90 percent deadline: a five-minute entry is probed at 270 s, and no jitter, so the
            // parity cases assert exact times.
            timing: KeepaliveTiming {
                lead_5m_ms: 30_000,
                jitter_5m_ms: 0,
                lead_1h_ms: 360_000,
                jitter_1h_ms: 0,
                ..KeepaliveTiming::default()
            },
        }
    }
}

/// Every account answers `probe_answer(2_000, 0)` by default, through a standing credential rule, so a test
/// that queues a response with `push_credential_rule` gets the standing answer back once its queue drains. A
/// real request reads the same answer: 2,000 cached tokens is cache evidence for it exactly as for a probe.
pub async fn rig(spec: RigSpec) -> Rig {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: probe_answer(2_000, 0),
    }])
    .await;
    for account in &spec.accounts {
        up.add_credential_rule(account.secret, "/v1/messages", probe_answer(2_000, 0));
    }
    let bus = Bus::new();
    let sub = bus.subscribe("test", Delivery::Durable);
    // Most of this suite is about a session's pin and account continuity, where a spelling resolving to a dated id
    // mid-test is not what it is testing; the cases that are about a resolution name the rules that answer one.
    let catalog_up = FakeUpstream::start(spec.catalog).await;
    let (store, store_path, scratch) = if spec.on_disk {
        let scratch = Scratch::new();
        let path = scratch.0.join("router.sqlite");
        let store = Arc::new(Store::open(&path, &StorageKey::generate()).unwrap());
        (store, Some(path), Some(scratch))
    } else {
        let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
        (store, None, None)
    };
    let h = relay_common::harness_on_with_bus_and_catalog(
        store,
        &up,
        &spec.accounts,
        spec.pool,
        bus.clone(),
        catalog_up,
        spec.accounting,
    )
    .await;
    let mut pool = h.store.pool("main").unwrap().unwrap();
    pool.keepalive_json = serde_json::to_string(&spec.policy).unwrap();
    h.store.update_pool(&pool, pool.updated_at, "test").unwrap();
    if let Some(tier) = spec.tier {
        for account in spec
            .accounts
            .iter()
            .filter(|a| a.kind == phoebis_store::AccountKind::Oauth)
        {
            name_the_tier(&h.store, account.id, tier);
        }
    }
    h.router.reload().await.unwrap();
    let clock: Clock = {
        let c = h.clock.clone();
        Arc::new(move || c.load(Ordering::SeqCst))
    };
    let db = Arc::new(LedgerDb::open_in_memory().unwrap());
    let pricing = Arc::new(Pricing::new(spec.pricing.clone()));
    let metrics = Arc::new(Metrics::new());
    let tables = Arc::new(spec.tables);
    let ledger = spawn_ledger(
        db.clone(),
        pricing.clone(),
        tables.clone(),
        h.store.clone(),
        &bus,
        h.router.clone(),
        clock.clone(),
        metrics.clone(),
        LedgerConfig::default(),
    );
    let engine = Engine::new(EngineParts {
        bus: bus.clone(),
        clock,
        cooldown_fallback: Duration::from_secs(60),
        ledger: ledger.clone(),
        limits: spec.limits.clone(),
        max_request_bytes: 32 << 20,
        metrics: metrics.clone(),
        pricing,
        relay: h.relay.clone(),
        router: h.router.clone(),
        session_expiry: spec.session_expiry,
        store: h.store.clone(),
        tables: tables.clone(),
        timing: spec.timing,
    });
    let mut rig = Rig {
        bus,
        clock: h.clock.clone(),
        db,
        engine,
        h,
        instances: BTreeMap::new(),
        ledger,
        limits: spec.limits,
        metrics,
        pricing: spec.pricing,
        sequence: 0,
        store_path,
        sub,
        tables,
        timing: spec.timing,
        up,
        _scratch: scratch,
    };
    rig.event(Event {
        event: "session_start".into(),
        ..Default::default()
    })
    .unwrap();
    rig
}

/// Stores a profile naming `tier` for `account`, as the profile fetch would.
pub fn name_the_tier(store: &phoebis_store::Store, account: &str, tier: &str) {
    store
        .set_profile(
            account,
            "{}",
            &phoebis_store::SubscriptionRow {
                plan: None,
                tier: Some(tier.to_string()),
                since: None,
                trial_ends_at: None,
            },
        )
        .unwrap();
}

/// A `session_start` for `session_id`, under the rig's one instance: a multi-session test names its own
/// session while sharing the instance a single-session test gets by default.
pub fn session_start(session_id: &str) -> Event {
    Event {
        event: "session_start".into(),
        session_id: session_id.into(),
        ..Default::default()
    }
}

/// A `session_end` for `session_id`.
pub fn session_end(session_id: &str) -> Event {
    Event {
        event: "session_end".into(),
        session_id: session_id.into(),
        ..Default::default()
    }
}

/// A `session_start` naming its own instance and, when replacing one, the instance it replaces.
pub fn session_start_as(
    session_id: &str,
    instance_id: &str,
    previous_instance: Option<&str>,
) -> Event {
    Event {
        event: "session_start".into(),
        session_id: session_id.into(),
        instance_id: instance_id.into(),
        previous_instance_id: previous_instance.map(String::from),
        ..Default::default()
    }
}

impl Rig {
    /// A second engine over the same ledger, store and router: a real restart, whose fresh `Inner` starts
    /// empty and holds only what the persistence round trip brings back, the first engine's `flush_persisted`
    /// and then this one's `load_persisted`.
    pub async fn restart(&self) -> Arc<Engine> {
        self.restart_with_limits(self.limits.clone()).await
    }

    /// `restart` under the limits a changed bootstrap file would give the next process.
    pub async fn restart_with_limits(&self, limits: KeepaliveLimits) -> Arc<Engine> {
        let clock: Clock = {
            let c = self.clock.clone();
            Arc::new(move || c.load(Ordering::SeqCst))
        };
        Engine::new(EngineParts {
            bus: Bus::new(),
            clock,
            cooldown_fallback: Duration::from_secs(60),
            ledger: self.ledger.clone(),
            limits,
            max_request_bytes: 32 << 20,
            metrics: Arc::new(Metrics::new()),
            pricing: Arc::new(Pricing::new(self.pricing.clone())),
            relay: self.h.relay.clone(),
            router: self.h.router.clone(),
            session_expiry: Duration::from_secs(86_400),
            store: self.h.store.clone(),
            tables: self.tables.clone(),
            timing: self.timing,
        })
    }

    pub fn now(&self) -> i64 {
        self.clock.load(Ordering::SeqCst)
    }

    pub fn advance(&self, ms: i64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
    }

    /// `advance` in steps inside the companion's lease, each followed by a heartbeat, as an attending companion
    /// renews it: for a test that moves the clock past the lease and then reports on the session.
    pub fn advance_attended(&mut self, ms: i64) {
        let mut left = ms;
        while left > 0 {
            let step = left.min(phoebis_keepalive::LEASE_MS / 2);
            self.advance(step);
            left -= step;
            self.event(Event {
                event: "heartbeat".into(),
                ..Default::default()
            })
            .unwrap();
        }
    }

    /// The generation the store stamped for `key_id`: a companion session belongs to the pair, so every call
    /// that names a key names its generation beside it. A key the store never held answers 0.
    pub fn generation(&self, key_id: &str) -> i64 {
        self.h
            .store
            .api_key(key_id)
            .unwrap()
            .map(|k| k.created_at)
            .unwrap_or_default()
    }

    /// A report under the harness key, with the next sequence, handed to `Engine::apply_event` as the route hands
    /// it a decoded body. A caller that leaves `session_id` unset gets the rig's one session, and one that leaves
    /// `instance_id` unset gets `instance_of` that session, so `session_end` and `session_start` need only the
    /// session id a multi-session test is driving.
    pub fn event(&mut self, mut event: Event) -> Result<Ack, EventError> {
        self.sequence += 1;
        event.sequence = self.sequence;
        if event.session_id.is_empty() {
            event.session_id = "session".into();
        }
        if event.instance_id.is_empty() {
            event.instance_id = self.instance_of(&event.session_id);
        }
        let registers = event.event == "session_start";
        let acked = self
            .engine
            .apply_event("desktop", self.generation("desktop"), event);
        // A refused start registered nothing, so the session keeps the instance the engine still holds.
        if let (true, Ok(ack)) = (registers, &acked) {
            self.instances
                .insert(ack.session_id.clone(), ack.instance_id.clone());
        }
        acked
    }

    /// The instance each session of the rig registered under, kept by `event` from the `session_start` it sent
    /// (the rig's default session starts as "instance"), so a report finds it without reading the engine. A start
    /// a test hands to `r.engine.apply_event` itself bypasses `event` and misses this map, so that test names the
    /// instance in every report it sends after it.
    pub fn instance_of(&self, session_id: &str) -> String {
        self.instances
            .get(session_id)
            .cloned()
            .unwrap_or_else(|| "instance".into())
    }

    /// The acknowledgment of a same-instance re-registration of the rig's session, the mod's view of the session.
    pub fn reregister(&mut self) -> Ack {
        let instance_id = self.instance_of("session");
        self.event(Event {
            event: "session_start".into(),
            session_id: "session".into(),
            instance_id,
            ..Default::default()
        })
        .expect("a same-instance re-registration of the rig's session")
    }

    /// The companion's lease, so a test that waits one out never repeats the number.
    pub fn lease_ms(&self) -> i64 {
        phoebis_keepalive::LEASE_MS
    }

    /// A `parked` word goes out as a background park, the shape the mod sends, since the route refuses a park
    /// without its kind.
    pub fn state(&mut self, agent: &str, state: &str) {
        let generation = self.agent(agent).request_generation;
        self.event(Event {
            event: "state".into(),
            agent_id: Some(agent.into()),
            state: Some(state.into()),
            request_generation: Some(generation),
            park_kind: (state == "parked").then(|| "background".into()),
            ..Default::default()
        })
        .unwrap();
    }

    /// The engine's own record of one agent of the rig's session, through `Engine::agent_status`.
    pub fn status(&self, id: &str) -> phoebis_keepalive::AgentStatus {
        self.engine
            .agent_status("desktop", "session", id)
            .unwrap_or_else(|| panic!("no agent {id}"))
    }

    /// `status` under the name the protocol tests read an agent's state and generation by.
    pub fn agent(&self, id: &str) -> phoebis_keepalive::AgentStatus {
        self.status(id)
    }

    pub fn request(&self, agent: &str) -> Request<Body> {
        self.request_with(agent, BODY)
    }

    /// The fixture head over `body`, for a test that sends another model or another cache marker.
    pub fn request_with(&self, agent: &str, body: &str) -> Request<Body> {
        let mut req = relay_common::request_from_fixture(
            phoebis_profile::fixtures::inference_head(
                phoebis_profile::fixtures::FixtureKind::ApiKey,
            ),
            body.as_bytes(),
            KEY,
        );
        relay_common::set_header(&mut req, "x-claude-code-session-id", "session");
        relay_common::set_header(&mut req, "x-claude-code-agent-id", agent);
        relay_common::set_header(&mut req, "anthropic-beta", "test-beta");
        req
    }

    /// Feeds the engine every bus event published so far, and waits for the completion of `request_id` when one
    /// is named: the relay's body task publishes it a moment after the response.
    pub async fn pump(&mut self, request_id: Option<&str>) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let mut complete = false;
            while let Ok(event) = self.sub.events.try_recv() {
                if let BusEvent::RequestComplete(c) = &event {
                    complete |= Some(c.request_id.as_str()) == request_id;
                }
                self.engine.observe(event);
            }
            if request_id.is_none() || complete {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no completion for {request_id:?}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// One real request through the relay for `agent`, answered by the standing rule, and pumped into the
    /// engine.
    pub async fn observe(&mut self, agent: &str) -> String {
        self.observe_with(agent, BODY).await
    }

    pub async fn observe_with(&mut self, agent: &str, body: &str) -> String {
        let resp = self
            .h
            .app
            .clone()
            .oneshot(self.request_with(agent, body))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let id = relay_common::outcome(&resp).request_id;
        let _ = resp.into_body().collect().await.unwrap();
        self.pump(Some(&id)).await;
        id
    }

    /// Pumps until `ready` holds, for a request whose events arrive while its body is still open.
    pub async fn pump_until(&mut self, ready: impl Fn(&Rig) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            self.pump(None).await;
            if ready(self) {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the engine never reached the awaited state"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// One engine tick, drained: the probes it starts are awaited and their bus events pumped, so an assertion
    /// on the line after it reads a settled engine. A test that has to look at a probe still in flight drives
    /// `engine.step` itself instead.
    pub async fn step(&mut self) {
        self.engine.step().await;
        self.engine.wait_idle().await;
        self.pump(None).await;
    }

    /// The model is the body's, as the relay would report it, so a test observes a request on another model by
    /// changing the body alone.
    pub fn observed(&self, id: &str, agent: &str, body: &str) -> BusEvent {
        let model = serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v["model"].as_str().map(str::to_string))
            .unwrap_or_else(|| "claude-sonnet-4-5".into());
        BusEvent::RequestObserved(RequestObserved {
            request_id: id.into(),
            origin: Origin::Client,
            kind: Kind::Inference,
            key_id: Some("desktop".into()),
            key_generation: self.generation("desktop"),
            pool_id: Some("main".into()),
            session_id: Some("session".into()),
            agent_id: Some(agent.into()),
            parent_agent_id: None,
            owner_id: None,
            model: model.clone(),
            canonical_model: model.to_ascii_lowercase(),
            wire_model: model.to_ascii_lowercase(),
            stream: true,
            headers: vec![
                ("anthropic-beta".into(), "test-beta".into()),
                ("x-claude-code-agent-id".into(), agent.into()),
                ("x-claude-code-session-id".into(), "session".into()),
            ],
            body: Bytes::from(body.to_string()),
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
            received_at: self.now(),
        })
    }

    /// A second observation whose class, static hash, and boundary chain a guard test drives directly, rather
    /// than the ones `observed` infers from the body alone.
    #[allow(clippy::too_many_arguments)]
    pub fn observed_with(
        &self,
        id: &str,
        agent: &str,
        request_class: Option<&str>,
        static_hash: Option<&str>,
        boundaries: &[&str],
        body: &[u8],
        model: &str,
    ) -> BusEvent {
        BusEvent::RequestObserved(RequestObserved {
            request_id: id.into(),
            origin: Origin::Client,
            kind: Kind::Inference,
            key_id: Some("desktop".into()),
            key_generation: self.generation("desktop"),
            pool_id: Some("main".into()),
            session_id: Some("session".into()),
            agent_id: Some(agent.into()),
            parent_agent_id: None,
            owner_id: None,
            model: model.into(),
            canonical_model: model.to_ascii_lowercase(),
            wire_model: model.to_ascii_lowercase(),
            stream: true,
            headers: vec![
                ("anthropic-beta".into(), "test-beta".into()),
                ("x-claude-code-agent-id".into(), agent.into()),
                ("x-claude-code-session-id".into(), "session".into()),
            ],
            body: Bytes::copy_from_slice(body),
            body_capture_hours: 0,
            shape: RequestShape::default(),
            static_hash: static_hash.map(String::from),
            boundaries: boundaries.iter().map(|b| b.to_string()).collect(),
            client: phoebis_bus::ClientFacts {
                request_class: request_class.map(String::from),
                ..phoebis_bus::ClientFacts::default()
            },
            auto_mode: None,
            fork: phoebis_bus::Fork::default(),
            presented_retired: false,
            rotation_at: None,
            guardrail_id: None,
            guardrails: Vec::new(),
            received_at: self.now(),
        })
    }

    pub fn selected(&self, id: &str, account: &str) -> BusEvent {
        BusEvent::AccountSelected(AccountSelected {
            request_id: id.into(),
            attempt: 1,
            account_id: account.into(),
            account_kind: "api_key".into(),
            pin_epoch: Some(1),
            started_at: self.now(),
            cache_ttl_ms: None,
            selection: None,
        })
    }

    pub fn chunk(&self, id: &str, index: u32, body: &str) -> BusEvent {
        BusEvent::ResponseChunk(ResponseChunk {
            request_id: id.into(),
            attempt: 1,
            index,
            body: Bytes::from(body.to_string()),
            phase: None,
        })
    }

    pub fn complete(&self, id: &str, outcome: Outcome, usage: Option<Usage>) -> BusEvent {
        BusEvent::RequestComplete(RequestComplete {
            request_id: id.into(),
            attempt: 1,
            account_id: Some("a".into()),
            outcome,
            status: 200,
            usage,
            response: ResponseFacts::default(),
            ttfb_ms: None,
            ttfc_ms: None,
            error_code: None,
            client_drift: None,
            client_contract: None,
            client_version: None,
            response_bytes: None,
            response_chunks: None,
            held: None,
            diagnostics: None,
            message_id: None,
            finished_at: self.now(),
        })
    }

    /// Reports evidence that stands: the overage gate reads it through the router's view on the scheduling tick
    /// that follows.
    pub async fn enter_overage(&self, account: &str, reset_at: i64) {
        let incarnation = self.h.store.account(account).unwrap().unwrap().incarnation;
        self.h
            .router
            .report(phoebis_routing::Report::Overage {
                account_id: account.into(),
                evidence: phoebis_routing::OverageEvidence {
                    in_use: true,
                    since: Some(self.now()),
                    reset: Some(reset_at),
                    observed_at: self.now(),
                    source: phoebis_routing::EvidenceSource::Head,
                    incarnation,
                },
                incarnation,
            })
            .await;
    }

    /// Reports evidence whose reset has already passed, so `OverageEvidence::stands` reads it as ended without
    /// the fallback window ever coming into it.
    pub async fn leave_overage(&self, account: &str) {
        let incarnation = self.h.store.account(account).unwrap().unwrap().incarnation;
        self.h
            .router
            .report(phoebis_routing::Report::Overage {
                account_id: account.into(),
                evidence: phoebis_routing::OverageEvidence {
                    in_use: true,
                    since: None,
                    reset: Some(self.now() - 1),
                    observed_at: self.now(),
                    source: phoebis_routing::EvidenceSource::Head,
                    incarnation,
                },
                incarnation,
            })
            .await;
    }
}

/// A report lands on the router task's own queue, and the state it changes is visible once the handle says so.
///
/// The lifetime is named rather than elided: `check` is only ever called with this one borrow of `r`, and an
/// elided `Fn(&Rig) -> Fut` would desugar to a higher-ranked bound that no borrowing async closure can satisfy.
pub async fn settled<'a, F, Fut>(r: &'a Rig, check: F)
where
    F: Fn(&'a Rig) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    while !check(r).await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the router never applied the report"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// A real request and attempt for `a` in `main`: enough `claude-sonnet-5` input tokens, at the shipped table's
/// live rate, to price at `usd` dollars, so a spend this test wants is a row the ledger's own rollups carry
/// rather than a number the test invented.
pub fn spend_of(id: &str, now: i64, usd: f64) -> (RequestRecord, AttemptRecord) {
    let rate = PriceRevisions::shipped()
        .in_force(now)
        .price("claude-sonnet-5")
        .expect("the shipped table prices claude-sonnet-5")
        .input;
    let request = RequestRecord {
        id: id.into(),
        received_at: now,
        finished_at: Some(now + 500),
        key_id: Some("desktop".into()),
        pool_id: Some("main".into()),
        canonical_model: "claude-sonnet-5".into(),
        model: "claude-sonnet-5".into(),
        kind: "inference".into(),
        origin: "client".into(),
        outcome: "succeeded".into(),
        attempts: 1,
        ..Default::default()
    };
    let attempt = AttemptRecord {
        request_id: id.into(),
        attempt: 1,
        account_id: "a".into(),
        account_kind: "api_key".into(),
        outcome: "succeeded".into(),
        input_tokens: Some((usd / rate * 1_000_000.0).round() as u64),
        ..Default::default()
    };
    (request, attempt)
}

/// Lands a real, priced request for `a` through the ledger's own channel, the path a real request's completion
/// uses, and waits for the router's view of `a`'s spend to agree: a priced landing is what makes the writer
/// refresh the router at once, rather than at its next slow poll of the registry for the members a test just
/// changed, so this is safe as the seed each test starts from, before anything else races its timing.
pub async fn plant_spend(r: &Rig, id: &str, usd: f64) {
    let (request, attempts) = spend_of(id, r.now(), usd);
    r.ledger
        .record_request(Record::Request {
            request,
            attempts: vec![attempts],
            bodies: None,
        })
        .await;
    settled(r, |r| async move {
        r.h.router
            .member_spends()
            .await
            .unwrap()
            .iter()
            .any(|row| row.account_id == "a" && row.spent_usd >= usd - 0.01)
    })
    .await;
}

/// The formatter's output, kept in memory so a case reads back exactly what the host's journal would hold.
#[derive(Clone, Default)]
pub struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Captured {
    /// Makes the capture this thread's subscriber until the guard drops. A `#[tokio::test]` polls the engine, the
    /// relay and the probes on the case's own thread, so every line the case drives lands here. A case that reads a
    /// debug line captures at `Level::DEBUG`.
    pub fn install_at(&self, level: tracing::Level) -> tracing::subscriber::DefaultGuard {
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_ansi(false)
                .with_max_level(level)
                .with_writer(self.clone())
                .finish(),
        )
    }

    /// `install_at` at INFO, the level the warning cases read.
    pub fn install(&self) -> tracing::subscriber::DefaultGuard {
        self.install_at(tracing::Level::INFO)
    }

    pub fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

/// The probe dollars and the wasted dollars a reader answers over `rows`, each row's counts priced on the rig's list:
/// a probe's cost is its own request row's counts, and a settlement's waste the probe's counts it carries.
pub fn read_dollars(r: &Rig, rows: &[phoebis_ledger::RollupRow]) -> (f64, f64) {
    phoebis_ledger::breakdown(
        rows,
        "all",
        true,
        &phoebis_ledger::LiveKeys::default(),
        &[],
        None,
        &r.pricing,
        &r.tables,
    )
    .first()
    .map_or((0.0, 0.0), |group| {
        (group.probe_cost_usd, group.wasted_probe_usd)
    })
}
