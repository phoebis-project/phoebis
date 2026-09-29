//! A tracing capture for the suites that assert on what the router logs.

use std::sync::{Arc, Mutex};

use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

/// The module paths whose events are captured: the oauth crate's (the refresh's steps, a 429's warning) and the
/// ledger's (the poll's warnings). Any other crate's events would only be noise in an assertion that every line
/// names the account.
const CAPTURED_TARGETS: &[&str] = &["phoebis_ledger::", "phoebis_oauth::"];

/// One formatted line per captured event, the enclosing spans' fields joined ahead of the event's, so a line
/// carries what a span attached (an account id, say) without a second pass over the span stack.
#[derive(Default)]
struct Fields(String);

impl tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!("{}={value:?} ", field.name()));
    }
}

thread_local! {
    // A global subscriber's callsite interest is cached process-wide: a thread-local default installed only
    // for one test would race every other test in the binary, some of which would win the cache with a
    // subscriber that is not interested and silence this one's events for good. Routing
    // through a thread-local sink instead of a thread-local default keeps the interest decision uncontested
    // (one global subscriber, always interested) while keeping every test's capture to its own thread.
    static SINK: std::cell::RefCell<Option<Arc<Mutex<Vec<String>>>>> = const { std::cell::RefCell::new(None) };
}

struct CaptureLayer;

impl<S> tracing_subscriber::Layer<S> for CaptureLayer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::Id,
        ctx: Context<'_, S>,
    ) {
        let mut fields = Fields::default();
        attrs.record(&mut fields);
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(fields);
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        let target = event.metadata().target();
        if !CAPTURED_TARGETS
            .iter()
            .any(|prefix| target.starts_with(prefix))
        {
            return;
        }
        SINK.with(|cell| {
            let Some(sink) = cell.borrow().clone() else {
                return;
            };
            let mut line = String::new();
            if let Some(scope) = ctx.event_scope(event) {
                for span in scope.from_root() {
                    if let Some(fields) = span.extensions().get::<Fields>() {
                        line.push_str(&fields.0);
                    }
                }
            }
            let mut fields = Fields::default();
            event.record(&mut fields);
            line.push_str(&fields.0);
            sink.lock().unwrap().push(line);
        });
    }
}

/// Installs the capturing layer as the process's one subscriber, the first time any test calls this, and
/// points this thread's events at a fresh buffer until the guard drops.
pub fn capture_tracing() -> (Arc<Mutex<Vec<String>>>, impl Drop) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let subscriber = tracing_subscriber::registry().with(CaptureLayer);
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
    let lines = Arc::new(Mutex::new(Vec::new()));
    SINK.with(|cell| *cell.borrow_mut() = Some(lines.clone()));
    struct ClearSink;
    impl Drop for ClearSink {
        fn drop(&mut self) {
            SINK.with(|cell| *cell.borrow_mut() = None);
        }
    }
    (lines, ClearSink)
}
