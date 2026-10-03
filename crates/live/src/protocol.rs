//! What goes over the socket: one snapshot, then deltas keyed by request id and ticks, every message carrying
//! the cursor a reconnecting client compares against.

use serde::{Deserialize, Serialize};

use crate::state::{LiveAttempt, LivePhases, LiveRequest, LiveUsage};
use crate::tick::LiveTick;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LiveSnapshot {
    pub cursor: u64,
    pub in_flight: Vec<LiveRequest>,
    pub recent: Vec<LiveRequest>,
    pub tick: LiveTick,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DensityBucket {
    pub at: i64,
    pub requests: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PastSnapshot {
    pub entries: Vec<LiveRequest>,
    pub density: Vec<DensityBucket>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Delta {
    Started {
        cursor: u64,
        request: LiveRequest,
    },
    Selected {
        at: i64,
        cursor: u64,
        id: String,
        account_id: String,
        attempt: LiveAttempt,
        #[serde(skip_serializing_if = "Option::is_none")]
        failed: Option<LiveAttempt>,
    },
    Head {
        at: i64,
        cursor: u64,
        id: String,
        attempt: LiveAttempt,
        status: u16,
    },
    Progress {
        at: i64,
        cursor: u64,
        id: String,
        output_tps: Option<f64>,
        usage: LiveUsage,
    },
    Preview {
        cursor: u64,
        id: String,
        chunk: String,
    },
    Finished {
        at: i64,
        cursor: u64,
        id: String,
        /// The attempts as the request ended with them: a connection that failed before a head, and a stream
        /// that failed after one, both settle an attempt's outcome here. The snapshot carries the same rows,
        /// so a client that applies this delta and one that reconnects show the same request.
        attempts: Vec<LiveAttempt>,
        cost_usd: f64,
        error_code: Option<String>,
        phases: LivePhases,
        status: Option<u16>,
        usage: LiveUsage,
    },
    Tick {
        cursor: u64,
        tick: LiveTick,
    },
}

impl Delta {
    pub fn cursor(&self) -> u64 {
        match self {
            Delta::Started { cursor, .. }
            | Delta::Selected { cursor, .. }
            | Delta::Head { cursor, .. }
            | Delta::Progress { cursor, .. }
            | Delta::Preview { cursor, .. }
            | Delta::Finished { cursor, .. }
            | Delta::Tick { cursor, .. } => *cursor,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LiveEvent {
    Delta { delta: Delta },
    Snapshot { snapshot: LiveSnapshot },
}
