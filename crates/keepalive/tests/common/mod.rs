//! The fake-clock rig: an engine over the relay harness, a bus the test pumps by hand so every event reaches
//! the engine before the assertion that follows, and a ledger for the accounting tests.
#![allow(dead_code)]

#[path = "../../../relay/tests/common/mod.rs"]
pub mod relay_common;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use bytes::Bytes;
use phoebis_bus::{
    AccountSelected, Bus, Event as BusEvent, Kind, LifecycleSender, Origin, Outcome,
    RequestComplete, RequestObserved, RequestShape, ResponseChunk, ResponseFacts, Subscriber,
    Usage,
};
use phoebis_keepalive::{
    Ack, Engine, EngineParts, Event, EventError, KeepaliveLimits, KeepalivePolicy,
};
use phoebis_ledger::{
    spawn as spawn_ledger, Ledger, LedgerConfig, LedgerDb, Metrics, PlanFactors, PlanWindows,
    PriceTable, Pricing,
};
use phoebis_oauth::Clock;
use fake_upstream::{FakeUpstream, Respond, Rule};
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

/// The request body the Go rig replayed: adaptive thinking, one five-minute marker, one message.
pub const BODY: &str = r#"{"model":"claude-sonnet-4-5","max_tokens":1024,"stream":true,"thinking":{"type":"adaptive"},"system":[{"type":"text","text":"private prompt","cache_control":{"type":"ephemeral"}}],"messages":[{"role":"user","content":"hello"}],"metadata":{"user_id":"{\"device_id\":\"d\",\"account_uuid\":\"\",\"session_id\":\"session\"}"}}"#;

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
    pub ledger: Ledger,
    pub metrics: Arc<Metrics>,
    pub sequence: u64,
    pub sub: Subscriber,
    pub up: FakeUpstream,
}

pub struct RigSpec {
    pub accounts: Vec<TestAccount>,
    /// What the catalog fake answers; nothing by default, so a spelling never resolves to a dated id mid-test.
    pub catalog: Vec<Rule>,
    pub lifecycle: Option<LifecycleSender>,
    pub limits: KeepaliveLimits,
    pub policy: KeepalivePolicy,
    pub pool: PoolSpec,
    pub session_expiry: Duration,
}

impl Default for RigSpec {
    fn default() -> Self {
        RigSpec {
            accounts: vec![api_key("a", A, 0), api_key("b", B, 1)],
            catalog: Vec::new(),
            lifecycle: None,
            limits: KeepaliveLimits {
                lease_seconds: 3_600,
                ..KeepaliveLimits::default()
            },
            // The Go rig's 90 percent deadline: a five-minute entry is probed at 270 s, and no jitter, so the
            // parity cases assert exact times.
            policy: KeepalivePolicy {
                enabled: true,
                lead_seconds_5m: 30,
                jitter_seconds_5m: 0,
                lead_seconds_1h: 360,
                jitter_seconds_1h: 0,
                ..KeepalivePolicy::default()
            },
            pool: PoolSpec::default(),
            session_expiry: Duration::from_secs(86_400),
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
    let sub = bus.subscribe("test");
    // Most of this suite is about a session's pin and account continuity, where a spelling resolving to a dated id
    // mid-test is not what it is testing; the cases that are about a resolution name the rules that answer one.
    let catalog_up = FakeUpstream::start(spec.catalog).await;
    let h = relay_common::harness_with_bus_and_catalog(
        &up,
        &spec.accounts,
        spec.pool,
        bus.clone(),
        catalog_up,
    )
    .await;
    let mut pool = h.store.pool("main").unwrap().unwrap();
    pool.keepalive_json = serde_json::to_string(&spec.policy).unwrap();
    h.store.update_pool(&pool, "test").unwrap();
    h.router.reload().await.unwrap();
    let clock: Clock = {
        let c = h.clock.clone();
        Arc::new(move || c.load(Ordering::SeqCst))
    };
    let db = Arc::new(LedgerDb::open_in_memory().unwrap());
    let pricing = Arc::new(Pricing::new(PriceTable::shipped()));
    let metrics = Arc::new(Metrics::new());
    let ledger = spawn_ledger(
        db.clone(),
        pricing.clone(),
        h.store.clone(),
        None,
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
        lifecycle: spec.lifecycle,
        limits: spec.limits,
        metrics: metrics.clone(),
        plan: Arc::new(PlanFactors::shipped()),
        pricing,
        relay: h.relay.clone(),
        router: h.router.clone(),
        session_expiry: spec.session_expiry,
        windows: Arc::new(PlanWindows::shipped()),
    });
    let mut rig = Rig {
        bus,
        clock: h.clock.clone(),
        db,
        engine,
        h,
        ledger,
        metrics,
        sequence: 0,
        sub,
        up,
    };
    rig.event(Event {
        event: "session_start".into(),
        ..Default::default()
    })
    .unwrap();
    rig
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
    pub fn now(&self) -> i64 {
        self.clock.load(Ordering::SeqCst)
    }

    pub fn advance(&self, ms: i64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
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

    /// A lifecycle event under the harness key, with the next sequence. A caller that leaves `session_id`
    /// unset gets the rig's one session; one that leaves `instance_id` unset gets that session's own current
    /// instance (the rig's default session and instance before either is registered), so `session_end` and
    /// `session_start` need only the session id a multi-session test is driving.
    pub fn event(&mut self, mut event: Event) -> Result<Ack, EventError> {
        self.sequence += 1;
        event.sequence = self.sequence;
        if event.session_id.is_empty() {
            event.session_id = "session".into();
        }
        if event.instance_id.is_empty() {
            let generation = self.generation("desktop");
            event.instance_id = self
                .engine
                .snapshot("desktop", generation)
                .sessions
                .into_iter()
                .find(|s| s.session_id == event.session_id)
                .map(|s| s.instance_id)
                .unwrap_or_else(|| "instance".into());
        }
        self.engine
            .apply_event("desktop", self.generation("desktop"), event)
    }

    /// A snapshot of the rig's own key, for a test that reads a tombstone's `ended` flag or a live session's
    /// sequence.
    pub fn snapshot(&self) -> phoebis_keepalive::Snapshot {
        self.engine.snapshot("desktop", self.generation("desktop"))
    }

    /// The lease the rig's limits carry, so a test that waits one out never repeats the number.
    pub fn lease_ms(&self) -> i64 {
        self.engine.limits().lease_ms()
    }

    pub fn state(&mut self, agent: &str, state: &str) {
        let generation = self.agent(agent).request_generation;
        self.event(Event {
            event: "state".into(),
            agent_id: Some(agent.into()),
            state: Some(state.into()),
            request_generation: Some(generation),
            ..Default::default()
        })
        .unwrap();
    }

    pub fn agent(&self, id: &str) -> phoebis_keepalive::SnapshotAgent {
        self.engine
            .snapshot("desktop", self.generation("desktop"))
            .sessions
            .into_iter()
            .flat_map(|s| s.agents)
            .find(|a| a.agent_id == id)
            .unwrap_or_else(|| panic!("missing agent {id}"))
    }

    /// The engine's own record of one agent, past what the companion snapshot shows.
    pub fn status(&self, id: &str) -> phoebis_keepalive::AgentStatus {
        self.engine
            .agent_status("desktop", "session", id)
            .unwrap_or_else(|| panic!("no agent {id}"))
    }

    pub fn request(&self, agent: &str) -> Request<Body> {
        self.request_with(agent, BODY)
    }

    /// The fixture head over `body`, for a test that sends another model or another cache marker.
    pub fn request_with(&self, agent: &str, body: &str) -> Request<Body> {
        let mut req = relay_common::request_from_fixture(
            phoebis_profile::fixtures::inference_head(phoebis_profile::fixtures::FixtureKind::ApiKey),
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
            presented_retired: false,
            rotation_at: None,
            received_at: self.now(),
        })
    }

    /// A second observation whose class, static hash, and boundary chain a guard test drives directly, rather
    /// than the ones `observed` infers from the body alone.
    #[allow(clippy::too_many_arguments)]
    pub fn observed_with_facts(
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
            presented_retired: false,
            rotation_at: None,
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
            client_version: None,
            response_bytes: None,
            response_chunks: None,
            finished_at: self.now(),
        })
    }

    /// Reports evidence that stands: the plan-side cap and the overage gate both read it through the router's
    /// view on the scheduling tick that follows.
    pub fn enter_overage(&self, account: &str, reset_at: i64) {
        let incarnation = self.h.store.account(account).unwrap().unwrap().incarnation;
        self.h.router.report(phoebis_routing::Report::Overage {
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
        });
    }

    /// Reports evidence whose reset has already passed, so `OverageEvidence::stands` reads it as ended without
    /// the fallback window ever coming into it.
    pub fn leave_overage(&self, account: &str) {
        let incarnation = self.h.store.account(account).unwrap().unwrap().incarnation;
        self.h.router.report(phoebis_routing::Report::Overage {
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
        });
    }
}
