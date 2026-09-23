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
use tokio::sync::{broadcast, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

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

/// Concurrent admin sockets this process serves. A connection past the bound is refused outright rather than
/// queued: an unbounded live feed is an unbounded amount of broadcast fan-out and per-socket state.
pub const MAX_SOCKETS: usize = 64;

/// Whether the session a socket was opened under still stands, asked by the writer before every send. Boxed so
/// the admin crate can close over its own session store without this crate depending on it.
pub type SessionCheck = Box<dyn Fn() -> bool + Send + Sync>;

/// A permit against `MAX_SOCKETS`, held for exactly as long as the connection it admits. Nothing reads the
/// permit back; the socket cap it releases on drop is the whole point of holding it.
#[allow(dead_code)]
pub struct SocketSlot(OwnedSemaphorePermit);

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
    cancel: CancellationToken,
    clients: Arc<AtomicUsize>,
    deltas: broadcast::Sender<Delta>,
    slots: Arc<Semaphore>,
    sockets: TaskTracker,
    state: Arc<Mutex<LiveState>>,
}

impl LiveFeed {
    pub fn spawn(bus: &Bus, pricing: Arc<Pricing>, clock: Clock, config: LiveConfig) -> LiveFeed {
        let (deltas, _) = broadcast::channel(BROADCAST_DEPTH);
        let state = Arc::new(Mutex::new(LiveState::new(pricing, clock)));
        let feed = LiveFeed {
            cancel: CancellationToken::new(),
            clients: Arc::new(AtomicUsize::new(0)),
            deltas: deltas.clone(),
            slots: Arc::new(Semaphore::new(MAX_SOCKETS)),
            sockets: TaskTracker::new(),
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

    /// A permit against the socket cap, or `None` once `MAX_SOCKETS` are already being served.
    pub fn slot(&self) -> Option<SocketSlot> {
        self.slots.clone().try_acquire_owned().ok().map(SocketSlot)
    }

    /// Serves one socket until the client goes away, falls behind, its session ends, or the feed shuts down.
    /// The snapshot and the subscription are taken under one lock so no delta falls between them, and the
    /// snapshot is built for the socket's role: a viewer's first message carries no more than its later ones
    /// would.
    pub async fn attach(
        &self,
        socket: WebSocket,
        preview: bool,
        alive: SessionCheck,
        slot: SocketSlot,
    ) {
        // A logout between the gate and this callback: the close frame and nothing else leaves.
        if !alive() {
            ws::close_ended(socket).await;
            return;
        }
        let (snapshot, deltas) = {
            let mut state = self.state.lock().expect(LOCK_POISONED);
            (state.snapshot(preview), self.deltas.subscribe())
        };
        let (cancel, clients) = (self.cancel.clone(), self.clients.clone());
        clients.fetch_add(1, Ordering::SeqCst);
        // Tracked, so a shutdown can cancel it and wait: hyper detaches the upgrade task this runs on.
        let served = self.sockets.spawn(async move {
            let _slot = slot;
            ws::serve(socket, snapshot, deltas, preview, alive, cancel).await;
            clients.fetch_sub(1, Ordering::SeqCst);
        });
        let _ = served.await;
    }

    /// Ends every socket: their serving tasks are cancelled, the tracker closed, and all of them waited for.
    pub async fn shutdown(&self) {
        self.cancel.cancel();
        self.sockets.close();
        self.sockets.wait().await;
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
