//! The statistics ledger: `ledger.sqlite`, written by one task from what the bus and the router publish, and
//! read by the admin routes and the metrics listener.

mod db;
mod histogram;
mod record;
mod schema;

pub use db::LedgerDb;
pub use histogram::{bucket_edges, bucket_index, Histogram, BUCKET_COUNT};
pub use record::{
    AttemptRecord, BodyRecord, EventRecord, ExtraUsageSampleRecord, QuotaSampleRecord,
    RequestRecord,
};

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("pricing table: {0}")]
    Yaml(String),
}
