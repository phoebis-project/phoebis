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
use phoebis_ledger::{
    spawn as spawn_ledger, AttemptRecord, Ledger, LedgerConfig, LedgerDb, Metrics, PlanTables,
    PriceRevisions, Pricing, Record, RequestRecord,
};
use phoebis_oauth::Clock;
use phoebis_relay::AccountingSource;
use phoebis_routing::RouterConfig;
use phoebis_session_engine::{
    events_router, AgentState, Engine, EngineParts, KeepaliveLimits, KeepalivePolicy,
    KeepaliveTiming, Lifecycle, Reason, RecipeView, EVENTS_PATH,
};
use phoebis_store::{AccountKind, StorageKey, Store};
use serde_json::{json, Value};
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

/// The events route's acknowledgment, read from the answer's body. `AckedAgent` holds the members that the route
/// writes for one agent of a registration's answer.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct Acked {
    pub agent_id: Option<String>,
    pub agents: Option<Vec<AckedAgent>>,
    pub instance_id: String,
    pub lease_expires_ms: i64,
    pub request_generation: u64,
    pub sequence: u64,
    pub session_id: String,
}

/// One agent of a registration's answer. `park` holds the park's members as the route wrote them, and it is empty for
/// an agent that is not parked.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct AckedAgent {
    pub agent_id: String,
    #[serde(flatten)]
    pub park: serde_json::Map<String, serde_json::Value>,
    pub request_generation: u64,
    pub state: AgentState,
}

/// The events route's refusal: the answer's status with the members of its body.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct Refused {
    pub code: String,
    pub instance_id: Option<String>,
    pub request_generation: Option<u64>,
    #[serde(skip)]
    pub status: u16,
}

pub struct Rig {
    pub bus: Bus,
    pub clock: Arc<std::sync::atomic::AtomicI64>,
    pub db: Arc<LedgerDb>,
    pub engine: Arc<Engine>,
    /// Session id and agent id to the request generation the agent's last answer through `event` carried, as the mod
    /// adopts it.
    generations: BTreeMap<(String, String), u64>,
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
    /// The directory of the ledger's file, behind every holder of the ledger, so it goes after they have let go.
    _ledger_dir: tempfile::TempDir,
    /// Last, so the directory goes after every holder of the store inside it has let go.
    _scratch: Option<Scratch>,
}

/// A directory of the rig's own for an on-disk registry, removed with the rig.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Scratch {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "phoebis-session-engine-rig-{}-{}",
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
    /// The router task's settings. The default's share pricer prices nothing, so a case that reads an account's burn
    /// hands in a pricer of its own.
    pub router: RouterConfig,
    pub session_expiry: Duration,
    /// The key the registry seals under and the companion's pseudonyms are digested with: drawn per rig by default,
    /// and a fixed one for a case whose pseudonyms have to sort in a known order.
    pub storage_key: StorageKey,
    /// The clock that the registry stamps a row with: the wall clock by default, and a fixed instant for a case that
    /// needs two rows on one stamp.
    pub store_clock: Clock,
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
            router: RouterConfig::default(),
            session_expiry: Duration::from_secs(86_400),
            storage_key: StorageKey::generate(),
            store_clock: relay_common::wall(),
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
        let store = Arc::new(Store::open(&path, &spec.storage_key, spec.store_clock).unwrap());
        (store, Some(path), Some(scratch))
    } else {
        let store = Arc::new(Store::open_in_memory(&spec.storage_key, spec.store_clock).unwrap());
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
        spec.router,
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
    let ledger_dir = tempfile::tempdir().unwrap();
    let db = Arc::new(LedgerDb::open(&ledger_dir.path().join("ledger.sqlite")).unwrap());
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
        generations: BTreeMap::new(),
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
        _ledger_dir: ledger_dir,
        _scratch: scratch,
    };
    rig.event(json!({"event": "session_start"})).await.unwrap();
    // The registration asks the save worker for a save, which would otherwise meet a case's own saves at no fixed
    // place. The wait sees that save read its batch, and the flush behind it returns once it landed, so each case
    // starts with two flushes counted and its registration in the registry.
    rig.engine.persist_reached(1, 1).await;
    rig.engine.flush_persisted().await;
    rig
}

/// The storage key `hex` spells. The store reads a fixed key from a file alone, so it goes through one in a scratch
/// directory removed on return.
pub fn storage_key(hex: &str) -> StorageKey {
    let scratch = Scratch::new();
    std::fs::create_dir_all(&scratch.0).unwrap();
    let path = scratch.0.join("storage.key");
    std::fs::write(&path, hex).unwrap();
    StorageKey::from_file(&path).unwrap()
}

/// Stores a profile naming `tier` for `account`, as the profile fetch would.
pub fn name_the_tier(store: &phoebis_store::Store, account: &str, tier: &str) {
    store
        .set_profile(
            account,
            "{}",
            &phoebis_store::SubscriptionRow {
                tier: Some(tier.to_string()),
                since: None,
                trial_ends_at: None,
            },
        )
        .unwrap();
}

/// A `session_start` for `session_id`, under the rig's one instance: a multi-session test names its own
/// session while sharing the instance a single-session test gets by default.
pub fn session_start(session_id: &str) -> Value {
    json!({"event": "session_start", "session_id": session_id})
}

/// A `session_end` for `session_id`.
pub fn session_end(session_id: &str) -> Value {
    json!({"event": "session_end", "session_id": session_id})
}

/// A `session_start` naming its own instance and, when replacing one, the instance it replaces.
pub fn session_start_as(
    session_id: &str,
    instance_id: &str,
    previous_instance: Option<&str>,
) -> Value {
    let mut body = json!({
        "event": "session_start",
        "session_id": session_id,
        "instance_id": instance_id
    });
    if let Some(previous_instance) = previous_instance {
        body["previous_instance_id"] = previous_instance.into();
    }
    body
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
    pub async fn advance_attended(&mut self, ms: i64) {
        let mut left = ms;
        while left > 0 {
            let step = left.min(phoebis_session_engine::LEASE_MS / 2);
            self.advance(step);
            left -= step;
            self.event(json!({"event": "heartbeat"})).await.unwrap();
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

    /// The incarnation the store holds for `account`, the one a selection on its row carries and a pin made on it
    /// names. An account the store never held answers 0.
    pub fn incarnation(&self, account: &str) -> u64 {
        self.h
            .store
            .account(account)
            .unwrap()
            .map(|row| row.incarnation)
            .unwrap_or_default()
    }

    /// A report under the harness key, with the next sequence, posted to the events route. `body` is a JSON object. The
    /// rig fills `protocol` and `sequence`, and `session_id` and `instance_id` each where the body does not name it:
    /// the rig's one session, and `instance_of` that session, so `session_end` and `session_start` need only the
    /// session id a multi-session test is driving.
    pub async fn event(&mut self, mut body: Value) -> Result<Acked, Refused> {
        let session_id = self.fill(&mut body);
        let registers = body["event"] == "session_start";
        let agent_id = body["agent_id"].as_str().map(String::from);
        let acked = self.post(KEY, body).await;
        // A refused start registered nothing, so the session keeps the instance the engine still holds.
        if let (true, Ok(ack)) = (registers, &acked) {
            self.instances
                .insert(ack.session_id.clone(), ack.instance_id.clone());
        }
        self.adopt(&session_id, agent_id.as_deref(), &acked);
        acked
    }

    /// A body under another key's secret, for a case that reports as a second holder. The rig fills the body as
    /// `event` does. It keeps no instance and no generation of the answer, since the rig's maps hold the harness key's
    /// sessions.
    pub async fn event_as(&mut self, secret: &str, mut body: Value) -> Result<Acked, Refused> {
        self.fill(&mut body);
        self.post(secret, body).await
    }

    /// The fills of `event`, answering the session id that the body names after them.
    fn fill(&mut self, body: &mut Value) -> String {
        self.sequence += 1;
        let fields = body
            .as_object_mut()
            .expect("a report's body is a JSON object");
        fields.entry("protocol").or_insert(1.into());
        fields.insert("sequence".into(), self.sequence.into());
        let session_id = fields
            .entry("session_id")
            .or_insert("session".into())
            .as_str()
            .expect("a session id is a string")
            .to_string();
        if !fields.contains_key("instance_id") {
            fields.insert("instance_id".into(), self.instance_of(&session_id).into());
        }
        session_id
    }

    /// A body as the case wrote it, under the bearer `secret`: the rig fills `protocol` where the body names none, and
    /// on a `session_start` `cache_warming` with `true` and `reported_facts` with the three kinds, each where the body
    /// names none, and nothing else, and it keeps no instance and no generation of the answer.
    pub async fn post(&self, secret: &str, mut body: Value) -> Result<Acked, Refused> {
        let fields = body
            .as_object_mut()
            .expect("a report's body is a JSON object");
        fields.entry("protocol").or_insert(1.into());
        if fields.get("event") == Some(&json!("session_start")) {
            fields.entry("cache_warming").or_insert(true.into());
            fields
                .entry("reported_facts")
                .or_insert(json!(["agents", "project", "title"]));
        }
        let request = Request::builder()
            .method("POST")
            .uri(EVENTS_PATH)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {secret}"))
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        // Built per post, since a restart test replaces the rig's engine.
        let answer = events_router(self.engine.clone())
            .oneshot(request)
            .await
            .unwrap();
        let status = answer.status().as_u16();
        let bytes = answer.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8_lossy(&bytes);
        if status == 200 {
            return Ok(serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| panic!("an acknowledgment, not {text}")));
        }
        // A 401 of the key's check carries the relay's error body and no code, which is no refusal of the report.
        match serde_json::from_slice::<Refused>(&bytes) {
            Ok(refused) => Err(Refused { status, ..refused }),
            Err(_) => panic!("the events route answered {status} with no refusal code: {text}"),
        }
    }

    /// Keeps each generation an answer carries: every agent of a registration, the agent an agent event names, main's
    /// on a session-level answer, and the current one a refusal names.
    fn adopt(&mut self, session_id: &str, agent_id: Option<&str>, acked: &Result<Acked, Refused>) {
        match acked {
            Ok(ack) => {
                for agent in ack.agents.iter().flatten() {
                    self.generations.insert(
                        (session_id.to_string(), agent.agent_id.clone()),
                        agent.request_generation,
                    );
                }
                let named = match (&ack.agent_id, agent_id) {
                    (Some(named), _) => Some(named.as_str()),
                    (None, None) => Some(phoebis_session_engine::MAIN_AGENT),
                    (None, Some(_)) => None,
                };
                if let Some(named) = named {
                    self.generations.insert(
                        (session_id.to_string(), named.to_string()),
                        ack.request_generation,
                    );
                }
            }
            Err(refusal) => {
                if let (Some(agent_id), Some(generation)) = (agent_id, refusal.request_generation) {
                    self.generations
                        .insert((session_id.to_string(), agent_id.to_string()), generation);
                }
            }
        }
    }

    /// The request generation the last answer for `agent` of the rig's session carried, 0 before any did.
    pub fn request_generation(&self, agent: &str) -> u64 {
        self.request_generation_on("session", agent)
    }

    /// `request_generation` for an agent of `session_id`.
    pub fn request_generation_on(&self, session_id: &str, agent: &str) -> u64 {
        self.generations
            .get(&(session_id.to_string(), agent.to_string()))
            .copied()
            .unwrap_or(0)
    }

    /// An agent report through `event` under the generation the agent's last answer carried. Where the engine moved
    /// the generation with no answer in between, the refusal names the current one, and the report goes once more
    /// under it, as the mod sends it. A case that sends a stale generation on purpose builds its report itself.
    pub async fn report(&mut self, mut body: Value) -> Result<Acked, Refused> {
        let session_id = body["session_id"].as_str().unwrap_or("session").to_string();
        let agent = body["agent_id"]
            .as_str()
            .expect("an agent report names its agent")
            .to_string();
        body["request_generation"] = self.request_generation_on(&session_id, &agent).into();
        match self.event(body.clone()).await {
            Err(refusal) if refusal.code == "stale_request_generation" => {
                body["request_generation"] = refusal.request_generation.into();
                self.event(body).await
            }
            acked => acked,
        }
    }

    /// The instance each session of the rig registered under, kept by `event` from the `session_start` it sent
    /// (the rig's default session starts as "instance"), so a report finds it without reading the engine. A start
    /// a test sends through `post` itself bypasses `event` and misses this map, so that test names the instance in
    /// every report it sends after it.
    pub fn instance_of(&self, session_id: &str) -> String {
        self.instances
            .get(session_id)
            .cloned()
            .unwrap_or_else(|| "instance".into())
    }

    /// The acknowledgment of a same-instance re-registration of the rig's session, the mod's view of the session.
    pub async fn reregister(&mut self) -> Acked {
        let instance_id = self.instance_of("session");
        self.event(json!({
            "event": "session_start",
            "session_id": "session",
            "instance_id": instance_id
        }))
        .await
        .expect("a same-instance re-registration of the rig's session")
    }

    /// The companion's lease, so a test that waits one out never repeats the number.
    pub fn lease_ms(&self) -> i64 {
        phoebis_session_engine::LEASE_MS
    }

    /// A `parked` word goes out as a background park, the shape the mod sends, since the route refuses a park
    /// without its kind.
    pub async fn state(&mut self, agent: &str, state: &str) {
        let mut body = json!({"event": "state", "agent_id": agent, "state": state});
        if state == "parked" {
            body["park_kind"] = "background".into();
        }
        self.report(body).await.unwrap();
    }

    /// The recipe view's row of one agent of the rig's session, through `Engine::recipes`, which lists an agent once
    /// it has sent a real request.
    pub fn view(&self, id: &str) -> RecipeView {
        view_of(&self.engine, "session", id).unwrap_or_else(|| panic!("no view row for {id}"))
    }

    /// The lifecycle word the session view gives one agent record of the rig's session, a registered-only one too.
    pub fn lifecycle(&self, id: &str) -> Option<Lifecycle> {
        lifecycle_of(&self.engine, "session", id)
    }

    /// The payload the registry holds for one agent of the rig's session, read back once a save landed.
    pub async fn saved(&self, id: &str) -> serde_json::Value {
        self.engine.flush_persisted().await;
        saved_of(&self.h.store, "session", id).unwrap_or_else(|| panic!("no saved row for {id}"))
    }

    /// The probes no request has answered yet in `saved`'s payload: the open chain's and those kept for the next
    /// request.
    pub async fn saved_unanswered(&self, id: &str) -> usize {
        let payload = self.saved(id).await;
        let count = |member: &str| payload[member].as_array().map_or(0, Vec::len);
        count("unanswered") + count("kept")
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
        no_probe_running(&self.engine).await;
        self.pump(None).await;
    }

    /// Waits for the answer of the probe in flight on `id`: the dispatch words the view's cause `probing`, and
    /// `apply_probe` replaces it under the lock that writes the rest of the result. What the worker sends behind the
    /// apply, its metrics and its settlements, can still be on the way, so a read of those waits for
    /// `no_probe_running`.
    pub async fn probe_answered(&self, id: &str) {
        wait_for(Duration::from_secs(5), || {
            (self.view(id).cause != Reason::Probing).then_some(())
        })
        .await;
    }

    /// The model is the body's, as the relay would report it, so a test observes a request on another model by
    /// changing the body alone. The headers are the ones a probe replays, so they carry the user agent and the
    /// session the router requires of a Claude Code client.
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
            instance_id: None,
            model: model.clone(),
            canonical_model: model.to_ascii_lowercase(),
            wire_model: model.to_ascii_lowercase(),
            stream: true,
            headers: vec![
                ("anthropic-beta".into(), "test-beta".into()),
                (
                    "user-agent".into(),
                    "claude-cli/2.1.269 (external, cli)".into(),
                ),
                ("x-claude-code-agent-id".into(), agent.into()),
                ("x-claude-code-session-id".into(), "session".into()),
            ],
            body: Bytes::from(body.to_string()),
            body_capture_hours: 0,
            shape: RequestShape::default(),
            static_hash: None,
            boundaries: Vec::new().into(),
            marked: Vec::new().into(),
            client: phoebis_bus::ClientFacts::default(),
            auto_mode: None,
            fork: phoebis_bus::Fork::default(),
            presented_retired: false,
            rotation_at: None,
            guardrail_id: None,
            guardrails: Vec::new(),
            received_at: self.now(),
            fallback_digest: None,
            cache_marker: false,
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
            instance_id: None,
            model: model.into(),
            canonical_model: model.to_ascii_lowercase(),
            wire_model: model.to_ascii_lowercase(),
            stream: true,
            headers: vec![
                ("anthropic-beta".into(), "test-beta".into()),
                (
                    "user-agent".into(),
                    "claude-cli/2.1.269 (external, cli)".into(),
                ),
                ("x-claude-code-agent-id".into(), agent.into()),
                ("x-claude-code-session-id".into(), "session".into()),
            ],
            body: Bytes::copy_from_slice(body),
            body_capture_hours: 0,
            shape: RequestShape::default(),
            static_hash: static_hash.map(String::from),
            boundaries: boundaries.iter().map(|b| b.to_string()).collect(),
            marked: Vec::new().into(),
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
            fallback_digest: None,
            cache_marker: false,
        })
    }

    /// The router's pin of the rig's session for `agent` on `model`, placed on `account` through the relay harness's
    /// own selection: the pin a synthetic `selected` event on `account` stands for, which the recipe's probe renews.
    pub async fn pin(&self, agent: &str, model: &str, account: &str) {
        assert_eq!(
            relay_common::pin_session(&self.h, "session", agent, model).await,
            account,
            "the router placed the session where the synthetic selection names it"
        );
    }

    pub fn selected(&self, id: &str, account: &str) -> BusEvent {
        BusEvent::AccountSelected(AccountSelected {
            request_id: id.into(),
            attempt: 1,
            account_id: account.into(),
            incarnation: self.incarnation(account),
            account_kind: "api_key".into(),
            usage_based: false,
            pin_epoch: Some(1),
            started_at: self.now(),
            cache_ttl_ms: None,
            selection: None,
            previous_input_tokens: None,
            entry_expired: None,
            agent_released: None,
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
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            origin: Origin::Client,
            kind: Kind::Inference,
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
            thinking_diagnostics: None,
            message_id: None,
            skipped_events: 0,
            interruption: None,
            classifier: None,
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

/// The recipe view's row of `agent` on `session_id` under the harness key, from any engine of the rig, a restarted one
/// among them.
pub fn view_of(engine: &Engine, session_id: &str, agent: &str) -> Option<RecipeView> {
    engine
        .recipes()
        .into_iter()
        .find(|v| v.key_id == "desktop" && v.session_id == session_id && v.agent_id == agent)
}

/// Waits until `engine` has no probe in flight, read off `Engine::stats`: a worker lowers the count behind its last
/// send, so a read after the wait finds what each probe settled.
pub async fn no_probe_running(engine: &Engine) {
    wait_for(Duration::from_secs(5), || {
        (engine.stats().running_probes == 0).then_some(())
    })
    .await;
}

/// The session view's lifecycle word for `agent` on `session_id` under the harness key, from any engine of the rig.
pub fn lifecycle_of(engine: &Engine, session_id: &str, agent: &str) -> Option<Lifecycle> {
    engine
        .sessions()
        .into_iter()
        .find(|s| s.key_id == "desktop" && s.session_id == session_id)
        .and_then(|s| s.lifecycles.get(agent).copied())
}

/// The saved payload of `agent` on `session_id` as the registry holds it now, with no flush of its own: the members of
/// the agent's row of the session engine and of its row of the warming in one object, since the two share no member.
/// A half of the agent's rows alone panics rather than answer half an object, where a read of a member the missing row
/// holds would compare `null` with `null`.
pub fn saved_of(store: &Store, session_id: &str, agent: &str) -> Option<serde_json::Value> {
    let state = store.load_engine_state().unwrap();
    let held = |key_id: &str, session: &str, agent_id: &str| {
        key_id == "desktop" && session == session_id && agent_id == agent
    };
    let companion = state
        .agents
        .into_iter()
        .find(|row| held(&row.key_id, &row.session_id, &row.agent_id));
    let warming = state
        .warming_agents
        .into_iter()
        .find(|row| held(&row.key_id, &row.session_id, &row.agent_id));
    match (companion, warming) {
        (None, None) => None,
        (Some(_), None) => panic!("keepalive_agents holds no row of {agent} on {session_id}"),
        (None, Some(_)) => panic!("companion_agents holds no row of {agent} on {session_id}"),
        (Some(companion), Some(warming)) => {
            let mut joined: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(&companion.payload).unwrap();
            let warming: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(&warming.payload).unwrap();
            joined.extend(warming);
            Some(serde_json::Value::Object(joined))
        }
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
        account_kind: AccountKind::ApiKey,
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

/// The probe dollars and the wasted dollars a reader answers over `rows`, each row's counts priced on the rig's list:
/// a probe's cost is its own request row's counts, and a settlement's waste the probe's counts it carries.
pub fn read_dollars(r: &Rig, rows: &[phoebis_ledger::RollupRow]) -> (f64, f64) {
    phoebis_ledger::breakdown(
        rows,
        phoebis_ledger::GroupBy::All,
        true,
        &phoebis_ledger::LiveKeys::default(),
        &[],
        &r.pricing,
        &r.tables,
        r.now(),
    )
    .first()
    .map_or((0.0, 0.0), |group| {
        (group.probe_cost_usd, group.wasted_probe_usd)
    })
}
