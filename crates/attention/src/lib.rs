//! The operator conditions as gauges: a stateless reader over the state the router already holds, fed the request
//! outcomes off the bus. Nothing here is configured, delivered or recorded.

mod reader;

use std::sync::{Arc, Mutex};

use phoebis_bus::{spawn_subscriber, Bus, Event};
use phoebis_ledger::{AttentionGaugeSource, AttentionGauges};
use phoebis_oauth::{Clock, UsageService};
use phoebis_routing::RouterHandle;
use phoebis_store::{Store, StoreError};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::reader::ReaderState;

pub use reader::{
    Condition, Kind, Link, Subject, SubjectKind, BACKOFF_ATTENTION_AFTER_MS, REFRESH_RUN_READ,
};

const STATE_POISONED: &str = "attention state mutex poisoned by a prior panic";

pub struct AttentionParts {
    pub bus: Bus,
    pub clock: Clock,
    pub router: RouterHandle,
    pub store: Arc<Store>,
    /// Where the poll's standing per account is read; `None` where a harness builds no usage service, and no account
    /// then reads a backoff.
    pub usage: Option<Arc<UsageService>>,
}

/// The one reader both the metrics listener and the admin route ask, with the request-fed state the bus arm writes.
pub struct Attention {
    clock: Clock,
    router: RouterHandle,
    state: Mutex<ReaderState>,
    store: Arc<Store>,
    usage: Option<Arc<UsageService>>,
}

impl Attention {
    /// Every subject of every kind, holding or clear; a failed store read fails the whole read.
    pub fn conditions(&self, now: i64) -> Result<Vec<Condition>, StoreError> {
        let mut state = self.state.lock().expect(STATE_POISONED);
        reader::conditions(
            &mut state,
            &self.store,
            &self.router,
            self.usage.as_deref(),
            now,
        )
    }

    /// The bus arm's entry, public so a test drives it event by event.
    pub fn observe(&self, event: &Event) {
        let now = (self.clock)();
        reader::observe(&mut self.state.lock().expect(STATE_POISONED), event, now);
    }

    /// The metrics listener reads the conditions through a closure, since the ledger that renders them is one of this
    /// crate's dependencies and cannot name its types. Sorted, so two scrapes list the series in one order.
    pub fn gauge_source(self: &Arc<Self>) -> AttentionGaugeSource {
        let attention = self.clone();
        Arc::new(move || {
            let mut conditions = attention.conditions((attention.clock)())?;
            conditions.sort_by(|a, b| (a.kind, &a.subject.id).cmp(&(b.kind, &b.subject.id)));
            Ok(AttentionGauges {
                conditions: conditions
                    .into_iter()
                    .map(|c| (c.kind.as_str(), c.subject.id, c.holds))
                    .collect(),
            })
        })
    }
}

/// The bus arm's task, so the binary's teardown can end it.
pub struct AttentionHandles {
    arm: JoinHandle<()>,
    cancel: CancellationToken,
}

impl AttentionHandles {
    /// Ends the arm once the event in hand is read.
    pub fn stop(&mut self) {
        self.cancel.cancel();
    }

    /// Ends the arm wherever it is, for a teardown out of time.
    pub fn abort(&mut self) {
        self.cancel.cancel();
        self.arm.abort();
    }
}

/// The reader and its bus arm, which hands every event to `observe`. Nothing else runs: every other condition is read
/// when a scrape or the admin route asks.
pub fn spawn(parts: AttentionParts) -> (Arc<Attention>, AttentionHandles) {
    let subscriber = parts.bus.subscribe("attention");
    let attention = Arc::new(Attention {
        clock: parts.clock,
        router: parts.router,
        state: Mutex::new(ReaderState::default()),
        store: parts.store,
        usage: parts.usage,
    });
    let cancel = CancellationToken::new();
    let arm = spawn_subscriber(subscriber, {
        let attention = attention.clone();
        let cancel = cancel.clone();
        move |mut subscriber| async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    event = subscriber.events.recv() => match event {
                        Some(event) => attention.observe(&event),
                        None => break,
                    },
                }
            }
        }
    });
    (attention, AttentionHandles { arm, cancel })
}
