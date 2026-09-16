//! The statistics ledger: `ledger.sqlite`, written by one task from what the bus and the router publish, and
//! read by the admin routes and the metrics listener.

mod assemble;
mod db;
mod histogram;
mod pricing;
mod record;
mod rollup;
mod schema;
mod task;

pub use assemble::{Assembler, Record, IN_FLIGHT_TTL_MS};
pub use db::LedgerDb;
pub use histogram::{bucket_edges, bucket_index, Histogram, BUCKET_COUNT};
pub use pricing::{
    diff_against, parse_import, price_attempts, ImportFormat, ModelPrice, PriceChange, PriceDiff,
    PriceTable, Pricing,
};
pub use record::{
    AttemptRecord, BodyRecord, EventRecord, ExtraUsageSampleRecord, QuotaSampleRecord,
    RequestRecord,
};
pub use rollup::{
    bucket_of, delta_of, fold_account, fold_request, hour_of, read_account_rollups, read_rollups,
    AccountRollupRow, CostSplit, RollupRow, UNATTRIBUTED,
};
pub use task::{spawn, Ledger, LedgerConfig, LedgerGone, LedgerStats, CHANNEL_DEPTH};

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
