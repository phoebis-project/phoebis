//! The in-process observation bus: bounded per-subscriber queues the relay publishes into without ever
//! waiting, so a slow consumer loses events rather than slowing a client stream.

mod event;

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::FutureExt;
use tokio::sync::mpsc;

pub use event::{
    header_published, lifecycle_channel, AccountSelected, AttemptOutcome, CooldownSet, Event, Kind,
    LifecycleReceiver, LifecycleSender, Origin, Outcome, Phase, RequestComplete, RequestObserved,
    ResponseChunk, ResponseHead, SessionLifecycle, Usage, HEADER_DENYLIST,
};

/// Deep enough to hold a whole streamed answer's chunks behind a consumer that is busy for a moment; a
/// subscriber that stays behind loses events rather than growing without bound.
pub const QUEUE_DEPTH: usize = 4_096;

const LOCK_POISONED: &str = "bus subscriber list poisoned by a prior panic";

#[derive(Debug, Default)]
pub struct SubscriberStats {
    pub dropped: AtomicU64,
    pub panics: AtomicU64,
}

pub struct Subscriber {
    pub name: &'static str,
    pub events: mpsc::Receiver<Event>,
    pub stats: Arc<SubscriberStats>,
}

/// One subscriber's name beside the counters it shares with the bus.
type NamedStats = (&'static str, Arc<SubscriberStats>);

struct Slot {
    name: &'static str,
    events: mpsc::Sender<Event>,
    stats: Arc<SubscriberStats>,
}

#[derive(Clone, Default)]
pub struct Bus {
    /// Every subscriber that ever attached, kept apart from the slots: a consumer that died is the one an
    /// operator most needs to see, and its slot leaves the publish path the moment its queue closes.
    counters: Arc<Mutex<Vec<NamedStats>>>,
    slots: Arc<Mutex<Vec<Slot>>>,
}

impl Bus {
    pub fn new() -> Bus {
        Bus::default()
    }

    pub fn subscribe(&self, name: &'static str) -> Subscriber {
        let (events_tx, events) = mpsc::channel(QUEUE_DEPTH);
        let stats = Arc::new(SubscriberStats::default());
        self.counters
            .lock()
            .expect(LOCK_POISONED)
            .push((name, stats.clone()));
        self.slots.lock().expect(LOCK_POISONED).push(Slot {
            name,
            events: events_tx,
            stats: stats.clone(),
        });
        Subscriber {
            name,
            events,
            stats,
        }
    }

    /// Never waits: one clone per live subscriber, `try_send`, and a drop counted where the queue is full.
    pub fn publish(&self, event: Event) {
        let mut slots = self.slots.lock().expect(LOCK_POISONED);
        slots.retain(|slot| match slot.events.try_send(event.clone()) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                slot.stats.dropped.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::debug!(subscriber = slot.name, "a bus subscriber went away");
                false
            }
        });
    }

    pub fn stats(&self) -> Vec<(&'static str, Arc<SubscriberStats>)> {
        self.counters.lock().expect(LOCK_POISONED).clone()
    }
}

/// A subscriber's task, with its panic caught and counted rather than left to the runtime: the relay shares
/// nothing with the task, so nothing of a panic reaches it, and the counter is what says a consumer died.
pub fn spawn_subscriber<F, Fut>(subscriber: Subscriber, run: F) -> tokio::task::JoinHandle<()>
where
    F: FnOnce(Subscriber) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let stats = subscriber.stats.clone();
    let name = subscriber.name;
    tokio::spawn(async move {
        if std::panic::AssertUnwindSafe(run(subscriber))
            .catch_unwind()
            .await
            .is_err()
        {
            stats.panics.fetch_add(1, Ordering::Relaxed);
            tracing::error!(subscriber = name, "a bus subscriber panicked and stopped");
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    fn observed(id: &str) -> Event {
        Event::RequestObserved(RequestObserved {
            request_id: id.into(),
            origin: Origin::Client,
            kind: Kind::Inference,
            key_id: Some("k".into()),
            key_generation: 0,
            pool_id: Some("p".into()),
            session_id: None,
            agent_id: Some("main".into()),
            parent_agent_id: None,
            owner_id: None,
            model: "claude-sonnet-5".into(),
            canonical_model: "claude-sonnet-5".into(),
            stream: false,
            headers: vec![],
            body: Bytes::from_static(b"{}"),
            body_capture_hours: 0,
            received_at: 1,
        })
    }

    fn chunk(id: &str, index: u32) -> ResponseChunk {
        ResponseChunk {
            request_id: id.into(),
            attempt: 1,
            index,
            body: Bytes::from_static(b"x"),
            phase: None,
        }
    }

    #[tokio::test]
    async fn a_full_queue_drops_and_counts_without_holding_the_relay() {
        let bus = Bus::new();
        let mut reader = bus.subscribe("reader");
        let sleeper = bus.subscribe("sleeper");
        let started = std::time::Instant::now();
        for i in 0..QUEUE_DEPTH {
            bus.publish(observed(&format!("r{i}")));
            // The reader keeps up; the sleeper never reads.
            if let Ok(event) = reader.events.try_recv() {
                assert_eq!(event.request_id(), format!("r{i}"));
            }
        }
        // Chunks share the queue: the sleeper's is full, so these drop, and they drop in order after the
        // observations that fit.
        for i in 0..40 {
            bus.publish(Event::ResponseChunk(chunk("r0", i)));
        }
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "publishing never waits on a subscriber"
        );
        assert_eq!(sleeper.stats.dropped.load(Ordering::SeqCst), 40);
        assert_eq!(reader.stats.dropped.load(Ordering::SeqCst), 0);
        let mut seen = 0;
        while let Ok(event) = reader.events.try_recv() {
            assert!(matches!(event, Event::ResponseChunk(_)));
            seen += 1;
        }
        assert_eq!(
            seen, 40,
            "the reader got every chunk, after every observation"
        );
        assert_eq!(bus.stats().len(), 2);
    }

    #[tokio::test]
    async fn a_subscriber_that_went_away_is_forgotten_and_a_panic_is_counted() {
        let bus = Bus::new();
        let mut live = bus.subscribe("live");
        let gone = bus.subscribe("gone");
        let stats = gone.stats.clone();
        drop(gone);
        bus.publish(observed("r1"));
        assert_eq!(
            bus.stats().len(),
            2,
            "the closed queue leaves the publish path at the next publish, its counters stay"
        );
        assert_eq!(
            stats.dropped.load(Ordering::SeqCst),
            0,
            "a closed queue is not a drop"
        );
        // The publish above found the queue closed and dropped its slot; the next one has only the live
        // subscriber to reach, and both subscribers are still counted.
        bus.publish(observed("r2"));
        let seen: Vec<String> = std::iter::from_fn(|| live.events.try_recv().ok())
            .map(|event| event.request_id().to_string())
            .collect();
        assert_eq!(
            seen,
            ["r1", "r2"],
            "every event reached the live subscriber"
        );
        assert_eq!(bus.stats().len(), 2);

        let panicking = bus.subscribe("panicking");
        let stats = panicking.stats.clone();
        let handle = spawn_subscriber(panicking, |mut s| async move {
            let _ = s.events.recv().await;
            panic!("subscriber bug");
        });
        bus.publish(observed("r2"));
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stats.panics.load(Ordering::SeqCst), 1);
        // The relay side is untouched: publishing after the panic neither fails nor panics.
        bus.publish(observed("r3"));
        assert_eq!(bus.stats().len(), 3);
    }

    /// A consumer that died is exactly the one an operator has to be able to see: its slot is gone from the
    /// publish path, its counters are not.
    #[tokio::test]
    async fn a_panicked_subscribers_counters_outlive_its_queue() {
        let bus = Bus::new();
        let panicking = bus.subscribe("panicking");
        let handle = spawn_subscriber(panicking, |mut s| async move {
            let _ = s.events.recv().await;
            panic!("subscriber bug");
        });
        bus.publish(observed("r1"));
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap();
        // The publish that finds the queue closed is the one that used to take the counter with it.
        bus.publish(observed("r2"));
        let stats = bus.stats();
        assert_eq!(stats.len(), 1);
        assert_eq!(
            (stats[0].0, stats[0].1.panics.load(Ordering::SeqCst)),
            ("panicking", 1)
        );
    }

    #[test]
    fn the_header_denylist_refuses_credentials_and_the_relays_own_hop() {
        for name in [
            "anthropic-beta",
            "anthropic-version",
            "content-type",
            "user-agent",
            "x-app",
            "x-claude-code-future",
            "x-claude-code-session-id",
            "X-Foo",
            "X-Stainless-Lang",
        ] {
            assert!(header_published(name), "{name}");
        }
        for name in [
            "Accept-Encoding",
            "authorization",
            "connection",
            "content-length",
            "cookie",
            "host",
            "transfer-encoding",
            "x-api-key",
            "x-keepalive-owner-id",
        ] {
            assert!(!header_published(name), "{name}");
        }
    }

    #[test]
    fn usage_merges_field_by_field_and_the_later_value_wins() {
        let mut usage = Usage {
            input_tokens: Some(10),
            cache_read_tokens: Some(4),
            ..Default::default()
        };
        usage.merge(&Usage {
            output_tokens: Some(7),
            cache_read_tokens: Some(5),
            ..Default::default()
        });
        assert_eq!(
            (
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_tokens
            ),
            (Some(10), Some(7), Some(5))
        );
        assert!(!usage.is_empty());
        assert!(Usage::default().is_empty());
    }
}
