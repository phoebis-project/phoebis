//! What both oauth suites share.

pub use fake_upstream::capture_tracing;

/// The wall clock, for a case that reads no stamp.
pub fn wall() -> phoebis_store::Clock {
    std::sync::Arc::new(|| jiff::Timestamp::now().as_millisecond())
}
