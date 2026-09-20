//! The engine: the rules in memory, one open event per rule and subject in the store, and a delivery task per
//! opened or resolved event.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use phoebis_ledger::{EventRecord, Ledger, Metrics, Record};
use phoebis_oauth::Clock;
use phoebis_store::{AlertEventRow, Store, StoreError};
use rand::RngExt;
use tokio::sync::mpsc;
use tokio_util::task::TaskTracker;

use crate::deliver::{body, deliver, delivered, sign, Sleeper, Webhook};
use crate::rule::{Kind, Rule, Signal};

const LOCK_POISONED: &str = "alert rules mutex poisoned by a prior panic";

/// Transitions queued per event beyond the one in flight: an open and its resolve, plus room for a resolve that
/// lands while the open still retries.
const QUEUE_DEPTH: usize = 8;

/// One transition of one event, queued to that event's own delivery worker so a resolve never overtakes the
/// open it follows, however long the open's retries take.
pub struct Transition {
    pub event: AlertEventRow,
    pub rule: Rule,
    pub state: &'static str,
}

pub struct AlertParts {
    pub clock: Clock,
    pub ledger: Ledger,
    pub metrics: Arc<Metrics>,
    /// What the webhook body's `router` field names: the admin origin when configured, else the admin listener.
    pub router_name: String,
    pub store: Arc<Store>,
    pub webhook: Arc<dyn Webhook>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct DeliveryResult {
    pub status: Option<u16>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct AlertStats {
    pub deliveries_failed: u64,
    pub deliveries_ok: u64,
    pub open: usize,
    pub rules: usize,
}

pub struct Alerts {
    parts: AlertParts,
    rules: Mutex<Vec<Rule>>,
    deliveries_ok: AtomicU64,
    deliveries_failed: AtomicU64,
    /// One sender per event whose worker is still running; an entry goes when its queue has drained.
    queues: Mutex<HashMap<String, mpsc::Sender<Transition>>>,
    sleeper: Box<Sleeper>,
    /// Held from a transition's store write through its enqueue, so the queue's order is the order the
    /// transitions committed in: an opener that lost its thread between the two would otherwise let the
    /// resolve of the row it just wrote go out first, and the receiver would end on the wrong state.
    transitions: Mutex<()>,
    workers: TaskTracker,
}

const TRANSITIONS_POISONED: &str = "alert transition mutex poisoned by a prior panic";

fn event_id() -> String {
    format!("evt_{:016x}", rand::rng().random::<u64>())
}

impl Alerts {
    /// The binary's constructor: the sleeper is `tokio::time::sleep`, so the retry backoff is real.
    pub fn new(parts: AlertParts) -> Arc<Alerts> {
        Self::with_sleeper(parts, Box::new(|d| Box::pin(tokio::time::sleep(d))))
    }

    /// The tests hand in a sleeper that returns at once, so the retry path runs without its waits.
    pub fn with_sleeper(parts: AlertParts, sleeper: Box<Sleeper>) -> Arc<Alerts> {
        let alerts = Arc::new(Alerts {
            parts,
            rules: Mutex::new(Vec::new()),
            deliveries_ok: AtomicU64::new(0),
            deliveries_failed: AtomicU64::new(0),
            queues: Mutex::new(HashMap::new()),
            sleeper,
            transitions: Mutex::new(()),
            workers: TaskTracker::new(),
        });
        if let Err(e) = alerts.reload() {
            tracing::warn!(%e, "the alert rules could not be read; the engine starts with none");
        }
        alerts
    }

    /// Re-reads the rules from the store; the routes call it after every write.
    pub fn reload(&self) -> Result<(), StoreError> {
        let rows = self.parts.store.alert_rules()?;
        *self.rules.lock().expect(LOCK_POISONED) = rows.iter().filter_map(Rule::from_row).collect();
        Ok(())
    }

    pub fn rules(&self) -> Vec<Rule> {
        self.rules.lock().expect(LOCK_POISONED).clone()
    }

    pub fn parts(&self) -> &AlertParts {
        &self.parts
    }

    /// Whether any enabled rule watches `kind`, so an input can skip work the store would answer with nothing:
    /// where no rule of the kind is enabled, no event of it can be open.
    pub fn watches(&self, kind: Kind) -> bool {
        self.rules
            .lock()
            .expect(LOCK_POISONED)
            .iter()
            .any(|r| r.enabled && r.kind == kind)
    }

    /// A condition holds for a subject: every enabled matching rule without an open event for the subject opens
    /// one and posts it.
    pub fn signal(self: &Arc<Self>, signal: &Signal) {
        for rule in self
            .rules()
            .into_iter()
            .filter(|r| r.enabled && r.matches(signal))
        {
            self.open(rule, signal);
        }
    }

    /// `signal` narrowed to one rule, for a kind whose condition is the rule's own threshold rather than a
    /// fact about the world every rule of the kind would read the same way.
    pub fn signal_rule(self: &Arc<Self>, rule_id: &str, signal: &Signal) {
        let Some(rule) = self
            .rules()
            .into_iter()
            .find(|r| r.id == rule_id && r.enabled && r.matches(signal))
        else {
            return;
        };
        self.open(rule, signal);
    }

    /// One rule's event for the signal's subject, opened atomically: the insert against the unique open index
    /// says whether this caller landed the row, and only then is the transition recorded and delivered. A
    /// concurrent opener whose insert did not land does nothing, so one incident posts once.
    fn open(self: &Arc<Self>, rule: Rule, signal: &Signal) {
        let now = (self.parts.clock)();
        let event = AlertEventRow {
            id: event_id(),
            rule_id: rule.id.clone(),
            kind: rule.kind.as_str().into(),
            subject: signal.subject.clone(),
            state: "open".into(),
            opened_at: now,
            resolved_at: None,
            detail_json: Some(signal.detail.to_string()),
        };
        let transitions = self.transitions.lock().expect(TRANSITIONS_POISONED);
        match self.parts.store.insert_alert_event(&event) {
            Ok(true) => {}
            Ok(false) => return,
            Err(e) => {
                tracing::warn!(%e, rule = %rule.id, "an alert event could not be written");
                return;
            }
        }
        self.record(&rule, &event, "alert_opened", signal);
        self.post(rule, event, "opened");
        drop(transitions);
    }

    /// A condition stopped holding: every open event of the kind and subject resolves and posts a resolve.
    pub fn clear(self: &Arc<Self>, kind: Kind, subject: &str) {
        let now = (self.parts.clock)();
        let open = match self.parts.store.open_alert_events() {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!(%e, "the open alert events could not be read");
                return;
            }
        };
        for event in open
            .into_iter()
            .filter(|e| e.kind == kind.as_str() && e.subject == subject)
        {
            self.finish(event, now);
        }
    }

    /// `clear` narrowed to one rule, the dual of `signal_rule`.
    pub fn clear_rule(self: &Arc<Self>, rule_id: &str, subject: &str) {
        if let Ok(Some(event)) = self.parts.store.open_alert_event(rule_id, subject) {
            self.finish(event, (self.parts.clock)());
        }
    }

    /// The operator's resolve (client_drift): the event resolves whatever the input says.
    pub fn resolve(self: &Arc<Self>, event_id: &str) -> Result<bool, StoreError> {
        let Some(event) = self.parts.store.alert_event(event_id)? else {
            return Ok(false);
        };
        if event.state != "open" {
            return Ok(false);
        }
        self.finish(event, (self.parts.clock)());
        Ok(true)
    }

    /// `resolve` with the event's detail replaced first, so the resolved delivery carries `detail`.
    pub fn resolve_with_detail(
        self: &Arc<Self>,
        event_id: &str,
        detail: serde_json::Value,
    ) -> Result<bool, StoreError> {
        let Some(mut event) = self.parts.store.alert_event(event_id)? else {
            return Ok(false);
        };
        if event.state != "open" {
            return Ok(false);
        }
        let detail_json = detail.to_string();
        self.parts
            .store
            .update_alert_event_detail(event_id, &detail_json)?;
        event.detail_json = Some(detail_json);
        self.finish(event, (self.parts.clock)());
        Ok(true)
    }

    fn finish(self: &Arc<Self>, mut event: AlertEventRow, now: i64) {
        let transitions = self.transitions.lock().expect(TRANSITIONS_POISONED);
        match self.parts.store.resolve_alert_event(&event.id, now) {
            Ok(true) => {}
            Ok(false) => return,
            Err(e) => {
                tracing::warn!(%e, event = %event.id, "an alert event could not be resolved");
                return;
            }
        }
        event.state = "resolved".into();
        event.resolved_at = Some(now);
        let Some(rule) = self.rules().into_iter().find(|r| r.id == event.rule_id) else {
            return;
        };
        let signal = Signal {
            kind: rule.kind,
            account_id: None,
            key_id: None,
            pool_id: None,
            subject: event.subject.clone(),
            detail: serde_json::from_str(event.detail_json.as_deref().unwrap_or("null"))
                .unwrap_or(serde_json::Value::Null),
        };
        self.record(&rule, &event, "alert_resolved", &signal);
        self.post(rule, event, "resolved");
        drop(transitions);
    }

    fn record(&self, rule: &Rule, event: &AlertEventRow, kind: &str, signal: &Signal) {
        let detail = &signal.detail;
        self.parts.ledger.record(Record::Event(EventRecord {
            at: (self.parts.clock)(),
            kind: kind.into(),
            account_id: signal
                .account_id
                .clone()
                .or_else(|| detail["account"].as_str().map(String::from)),
            key_id: signal
                .key_id
                .clone()
                .or_else(|| detail["key"].as_str().map(String::from)),
            session_id: detail["session"].as_str().map(String::from),
            model: None,
            actor: None,
            detail: Some(
                serde_json::json!({ "event": event.id, "rule": rule.id, "subject": event.subject })
                    .to_string(),
            ),
        }));
    }

    /// Queues the transition on the event's worker, starting one when the event has none. The worker delivers
    /// its queue in order, so the open's retries always finish before the resolve goes out and a receiver
    /// applying notifications in arrival order ends in the right state. Producer and worker both hold the map's
    /// lock while they send and while they retire a drained queue, so a transition cannot reach a worker that
    /// has already decided to stop.
    fn post(self: &Arc<Self>, rule: Rule, event: AlertEventRow, state: &'static str) {
        let id = event.id.clone();
        let transition = Transition { event, rule, state };
        let mut queues = self.queues.lock().expect(LOCK_POISONED);
        let sender = queues.entry(id.clone()).or_insert_with(|| {
            let (tx, mut rx) = mpsc::channel::<Transition>(QUEUE_DEPTH);
            let this = self.clone();
            let key = id.clone();
            self.workers.spawn(async move {
                while let Some(t) = rx.recv().await {
                    let result = deliver(
                        &this.parts,
                        &t.rule,
                        &t.event,
                        t.state,
                        this.sleeper.as_ref(),
                    )
                    .await;
                    let counter = if result.status.is_some_and(delivered) {
                        &this.deliveries_ok
                    } else {
                        &this.deliveries_failed
                    };
                    counter.fetch_add(1, Ordering::Relaxed);
                    // The emptiness check and the removal share the lock with the producer's send, so a
                    // transition offered between the two cannot land on a queue nobody will read.
                    let mut queues = this.queues.lock().expect(LOCK_POISONED);
                    if rx.is_empty() {
                        queues.remove(&key);
                        break;
                    }
                }
            });
            tx
        });
        match sender.try_send(transition) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(event = %id, "an alert transition was dropped: the event's delivery queue is full");
            }
            // A worker leaves its entry behind only by panicking, so this is a dead delivery task rather than
            // a busy one, and every later transition for the event would be dropped the same way.
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::error!(event = %id, "an alert transition was dropped: the event's delivery worker is gone");
            }
        }
    }

    /// A test delivery for a rule: the spec's body with `state = "test"`, one attempt, no event row. A test is
    /// the operator watching, and a retry would only delay the answer.
    pub async fn test(&self, rule_id: &str) -> DeliveryResult {
        let Some(rule) = self.rules().into_iter().find(|r| r.id == rule_id) else {
            return DeliveryResult {
                status: None,
                error: Some("rule not found".into()),
            };
        };
        let now = (self.parts.clock)();
        let event = AlertEventRow {
            id: "test".into(),
            rule_id: rule.id.clone(),
            kind: rule.kind.as_str().into(),
            subject: "test".into(),
            state: "test".into(),
            opened_at: now,
            resolved_at: None,
            detail_json: None,
        };
        let raw = body(&rule, &event, "test", &self.parts.router_name);
        let signed = match sign(&self.parts, &rule, &raw) {
            Ok(signed) => signed,
            Err(error) => {
                return DeliveryResult {
                    status: None,
                    error: Some(error),
                }
            }
        };
        match self
            .parts
            .webhook
            .post(rule.webhook_url.clone(), raw, signed)
            .await
        {
            Ok(status) => DeliveryResult {
                status: Some(status),
                error: None,
            },
            Err(error) => DeliveryResult {
                status: None,
                error: Some(error),
            },
        }
    }

    pub fn stats(&self) -> AlertStats {
        AlertStats {
            deliveries_failed: self.deliveries_failed.load(Ordering::Relaxed),
            deliveries_ok: self.deliveries_ok.load(Ordering::Relaxed),
            open: self
                .parts
                .store
                .open_alert_events()
                .map(|o| o.len())
                .unwrap_or(0),
            rules: self.rules().len(),
        }
    }

    /// Resolves once every delivery task spawned so far has finished; the tests and the shutdown wait on it.
    pub async fn wait_idle(&self) {
        self.workers.close();
        self.workers.wait().await;
        self.workers.reopen();
    }

    /// Closes the worker tracker and waits up to five seconds for the deliveries in flight.
    pub async fn shutdown(&self) {
        self.workers.close();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.workers.wait()).await;
    }
}
