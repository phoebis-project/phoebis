//! The live feed: the in-flight table and the recent ring the Observe view shows, fed from the bus and
//! served over a WebSocket.

mod protocol;
mod state;
mod tick;
mod ws;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::WebSocket;
use phoebis_bus::{spawn_subscriber, Bus, Subscriber};
use phoebis_ledger::Pricing;
use phoebis_oauth::Clock;
use tokio::sync::broadcast;

pub use protocol::{Delta, DensityBucket, LiveEvent, LiveSnapshot, PastSnapshot};
pub use state::{
    LiveAttempt, LiveCooldown, LivePhases, LiveRequest, LiveState, LiveUsage, LiveWindow,
    IN_FLIGHT_LIMIT, IN_FLIGHT_TTL_MS, PREVIEW_BYTES, PROGRESS_INTERVAL_MS, RATE_WINDOW_MS,
    RING_LIMIT, RING_WINDOW_MS,
};
pub use tick::{LiveMetric, LiveMetrics, LiveTick};
pub use ws::CLIENT_QUEUE;

const LOCK_POISONED: &str = "live state poisoned by a prior panic";

/// Deltas kept for slow readers before the broadcast reports a lag; a lagging client is disconnected anyway,
/// so this only has to cover the jitter between the feed task and a client's writer.
const BROADCAST_DEPTH: usize = 4_096;

#[derive(Clone, Debug)]
pub struct LiveConfig {
    pub tick: Duration,
}

impl Default for LiveConfig {
    fn default() -> Self {
        LiveConfig {
            tick: Duration::from_secs(1),
        }
    }
}

#[derive(Clone)]
pub struct LiveFeed {
    clients: Arc<AtomicUsize>,
    deltas: broadcast::Sender<Delta>,
    state: Arc<Mutex<LiveState>>,
}

impl LiveFeed {
    pub fn spawn(bus: &Bus, pricing: Arc<Pricing>, clock: Clock, config: LiveConfig) -> LiveFeed {
        let (deltas, _) = broadcast::channel(BROADCAST_DEPTH);
        let state = Arc::new(Mutex::new(LiveState::new(pricing, clock)));
        let feed = LiveFeed {
            clients: Arc::new(AtomicUsize::new(0)),
            deltas: deltas.clone(),
            state: state.clone(),
        };
        let subscriber = bus.subscribe("live");
        spawn_subscriber(subscriber, move |subscriber| {
            run(subscriber, state, deltas, config)
        });
        feed
    }

    pub fn snapshot(&self, preview: bool) -> LiveSnapshot {
        self.state.lock().expect(LOCK_POISONED).snapshot(preview)
    }

    pub fn past(&self, from: i64, to: i64) -> PastSnapshot {
        self.state.lock().expect(LOCK_POISONED).past(from, to)
    }

    pub fn clients(&self) -> usize {
        self.clients.load(Ordering::SeqCst)
    }

    pub fn in_flight(&self) -> usize {
        self.state.lock().expect(LOCK_POISONED).in_flight()
    }

    /// Serves one socket until the client goes away or falls behind. The snapshot and the subscription are
    /// taken under one lock so no delta falls between them, and the snapshot is built for the socket's role:
    /// a viewer's first message carries no more than its later ones would.
    pub async fn attach(&self, socket: WebSocket, preview: bool) {
        let (snapshot, deltas) = {
            let mut state = self.state.lock().expect(LOCK_POISONED);
            (state.snapshot(preview), self.deltas.subscribe())
        };
        self.clients.fetch_add(1, Ordering::SeqCst);
        ws::serve(socket, snapshot, deltas, preview).await;
        self.clients.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn run(
    mut subscriber: Subscriber,
    state: Arc<Mutex<LiveState>>,
    deltas: broadcast::Sender<Delta>,
    config: LiveConfig,
) {
    let mut tick = tokio::time::interval(config.tick);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let publish = |out: Vec<Delta>| {
        for delta in out {
            let _ = deltas.send(delta);
        }
    };
    loop {
        tokio::select! {
            event = subscriber.events.recv() => match event {
                Some(event) => publish(state.lock().expect(LOCK_POISONED).apply(event)),
                None => break,
            },
            _ = tick.tick() => publish(state.lock().expect(LOCK_POISONED).tick()),
        }
    }
}
