//! The operator conditions as gauges: a stateless reader over the state the router already holds, fed the request
//! outcomes off the bus. Nothing here is configured, delivered or recorded.

mod reader;

pub use reader::{
    Condition, Kind, Link, Subject, SubjectKind, BACKOFF_ATTENTION_AFTER_MS, REFRESH_RUN_READ,
};
