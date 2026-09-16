//! The Messages relay: caller authentication, request identity, the measured outbound header sequence, the two
//! body edits, and the upstream round trip with its retry and refresh rules.

mod auth;
mod body;
mod catalog;
mod classify;
mod dispatch;
mod drift;
mod errors;
mod exhaustion;
mod headers;
mod hold;
mod identity;
mod messages;
mod portability;
mod ratelimit;
mod sse;
mod stream;

pub use auth::{presented_key, AuthError};
pub use body::{edit_body, BodyEdits};
pub use catalog::{CatalogService, Discovered, DiscoveryError, SyncOutcome};
pub use classify::{classify, exhaustion_window, Cooldown, HeadContext, Scope, Verdict};
pub use drift::DriftGuard;
pub use errors::anthropic_error;
pub use exhaustion::exhaustion_response;
pub use headers::{outbound_headers, CredentialKind};
pub use hold::{hold, Commit, Held, Inspect};
pub use identity::{identify, RequestIdentity};
pub use messages::{router, Limits, Relay, RelayOutcome};
pub use portability::is_portable;
pub use ratelimit::{retry_after, upstream_date, UnifiedHeaders};
pub use sse::{SseEvent, SseParser};

/// A request the relay refuses before it reaches the upstream.
#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    #[error("the bound account's identity cannot be written into the request body")]
    AccountUuid,
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
