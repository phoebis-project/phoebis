//! The Messages relay: caller authentication, request identity, the caller's own headers passed through to
//! the account's endpoint, the two body edits, and the upstream round trip with its retry and refresh rules.

mod auth;
mod billing;
mod body;
mod budget;
mod cache_ttl;
mod casing;
mod catalog;
mod chains;
mod classify;
mod dispatch;
mod drift;
mod errors;
mod exhaustion;
mod facts;
mod headers;
mod hold;
mod identity;
mod lane;
mod memory;
mod messages;
mod models;
mod observe;
mod portability;
mod ratelimit;
mod serve;
mod sse;
mod stream;

pub use auth::{presented_key, AuthError};
pub use body::{
    edit_body, BodyEdits, BodyMember, Markerless, NativeEdit, SafeguardsEdit, CACHE_DIAGNOSIS_BETA,
    DANGEROUS_TOOL_USE_BETA, EXTENDED_TTL_BETA,
};
pub use cache_ttl::{
    CacheTtl, CacheTtlError, CacheTtlPolicy, ModelTtl, PartialTtlRules, TtlBranch, TtlRules,
};
pub use casing::{head_queue, HeadQueue, HeadRecorder, OriginalHead};
pub use catalog::{
    derive, derive_all, Catalog, CatalogChange, CatalogHook, CatalogRow, CatalogService,
    Discovered, DiscoveryError, Prefetched, Warmth, ERROR_HOLD, MAX_BODY, REFRESH_TICK,
};
pub use chains::{canonical, Chains};
pub use classify::{
    classify, exhaustion_window, next_month_start, Cooldown, HeadContext, Scope, Verdict,
    SPEND_CAP_REASON,
};
pub use drift::DriftGuard;
pub use errors::{anthropic_error, auth_error_response, key_refusal_response};
pub use exhaustion::exhaustion_response;
pub use headers::{outbound_headers, CredentialKind};
pub use hold::{hold, Commit, Held, HeldRead, Inspect, INSPECT_BUFFER};
pub use identity::{identify, RequestIdentity};
pub use memory::{MemoryKey, Previous};
pub use messages::{
    new_request_id, router, Accounting, AccountingSource, Limits, Probe, ReadinessSource, Relay,
    RelayOutcome, ServedUnder, MEMORY_FLUSH_INTERVAL,
};
pub use portability::is_portable;
pub use ratelimit::{
    head_sample, retry_after, upstream_date, Bucket, BucketHeaders, UnifiedHeaders, FAST_INPUT,
    FAST_OUTPUT,
};
pub use serve::{serve, Limits as ListenLimits};
pub use sse::{SseEvent, SseParser};

#[cfg(feature = "debug-hooks")]
impl Relay {
    /// How many decoders this relay built for the hold's and the bus copy's reads of a response, so a test can
    /// tell how many times a committed stream was decoded.
    pub fn decoders_built(&self) -> u64 {
        self.decoders_built
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// A request the relay refuses before it reaches the upstream.
#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    #[error("the bound account's identity cannot be written into the request body")]
    AccountUuid,
    #[error("the request body ended before it was complete")]
    BodyIncomplete,
    #[error("the request body is not valid gzip")]
    BodyNotGzip,
    #[error("{}", .0.shape_message())]
    BodyShape(BodyMember),
    #[error("the request body did not arrive in time")]
    BodyTimeout,
    #[error("the request body nests deeper than the relay reads")]
    BodyTooDeep,
    #[error("request body is larger than the configured limit")]
    BodyTooLarge,
    #[error("billing header: {0}")]
    Cch(#[from] phoebis_cch::CchError),
    /// The client stamped a billing hash or a version suffix this build's rules do not reproduce. A stamp the
    /// router cannot verify never travels under an account's credential, edited or not: a wrong hash is
    /// indistinguishable from tampering and the cost falls on the account, not on the request. Relaying the
    /// body unedited under the client's stamp and stripping the stamp are the two alternatives this rejects.
    #[error("request body carries a billing hash this build's rules do not reproduce ({kind})")]
    ClientDrift { kind: &'static str },
    #[error("request body names a model the relay will not repeat")]
    InvalidModel,
    #[error("the router is not accepting requests right now")]
    LedgerBehind,
    #[error("request body has no model")]
    NoModel,
    #[error("request body is not a JSON object")]
    NotAnObject,
    #[error("the request body's content-encoding is not supported; send gzip or identity")]
    UnsupportedEncoding,
}

/// How the edited body leaves toward the upstream, `proxy.upstream_gzip`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpstreamGzip {
    /// Gzip when the client sent gzip, identity otherwise.
    Auto,
    Off,
    On,
}
