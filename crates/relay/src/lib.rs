//! The Messages relay: caller authentication, request identity, the caller's own headers passed through to
//! the account's endpoint, the two body edits, and the upstream round trip with its retry and refresh rules.

mod auth;
mod billing;
mod body;
mod cache_ttl;
mod casing;
mod catalog;
mod classify;
mod dispatch;
mod drift;
mod errors;
mod exhaustion;
mod facts;
mod headers;
mod hold;
mod identity;
mod memory;
mod messages;
mod observe;
mod portability;
mod ratelimit;
mod serve;
mod sse;
mod stream;

pub use auth::{presented_key, AuthError};
pub use body::{
    edit_body, BodyEdits, BodyMember, Markerless, NativeEdit, VersionCheck, CACHE_DIAGNOSIS_BETA,
    EXTENDED_TTL_BETA,
};
pub use cache_ttl::{CacheTtl, CacheTtlError, CacheTtlPolicy, ModelTtl};
pub use casing::{head_queue, HeadQueue, HeadRecorder, OriginalHead};
pub use catalog::{CatalogService, Discovered, DiscoveryError, SyncOutcome};
pub use classify::{classify, exhaustion_window, Cooldown, HeadContext, Scope, Verdict};
pub use drift::DriftGuard;
pub use errors::{anthropic_error, auth_error_response, key_refusal_response};
pub use exhaustion::exhaustion_response;
pub use headers::{outbound_headers, CredentialKind};
pub use hold::{hold, Commit, Held, Inspect, INSPECT_BUFFER};
pub use identity::{identify, RequestIdentity};
pub use memory::{MemoryKey, Previous};
pub use messages::{
    new_request_id, router, Limits, Probe, Relay, RelayOutcome, MEMORY_FLUSH_INTERVAL,
};
pub use portability::is_portable;
pub use ratelimit::{retry_after, upstream_date, UnifiedHeaders};
pub use serve::serve;
pub use sse::{SseEvent, SseParser};

/// A request the relay refuses before it reaches the upstream.
#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    #[error("the bound account's identity cannot be written into the request body")]
    AccountUuid,
    #[error("request body has no {} member to edit", .0.path())]
    BodyShape(BodyMember),
    #[error("the request body nests deeper than the relay reads")]
    BodyTooDeep,
    #[error("request body is larger than the configured limit")]
    BodyTooLarge,
    #[error("billing header: {0}")]
    Cch(#[from] phoebis_cch::CchError),
    #[error("request body carries a billing hash this build's rules do not reproduce ({kind})")]
    ClientDrift { kind: &'static str },
    #[error("request body names a model the relay will not repeat")]
    InvalidModel,
    #[error("request body has no model")]
    NoModel,
    #[error("request body is not a JSON object")]
    NotAnObject,
}
