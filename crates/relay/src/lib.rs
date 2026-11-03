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
mod credit;
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

pub use auth::{authenticated_key, presented_key, AuthError};
pub use billing::is_entrypoint_token;
pub use body::{
    edit_body, BodyEdits, BodyMember, Markerless, NativeEdit, SafeguardsEdit, CACHE_DIAGNOSIS_BETA,
    CLASSIFIER_BETA, DANGEROUS_TOOL_USE_BETA, EXTENDED_TTL_BETA,
};
pub use cache_ttl::{
    CacheTtl, CacheTtlError, CacheTtlPolicy, ModelTtl, PartialTtlRules, TtlBranch, TtlRules,
};
pub use casing::{head_queue, HeadQueue, HeadRecorder, OriginalHead};
pub use catalog::{
    Catalog, CatalogChange, CatalogHook, CatalogRow, CatalogService, Discovered, DiscoveryError,
    Prefetched, Warmth, EFFORT_LEVELS, ERROR_HOLD, MAX_BODY, REFRESH_TICK,
};
pub use chains::{canonical, Chains};
pub use classify::{
    classify, exhaustion_window, next_month_start, Cooldown, HeadContext, Scope, Verdict,
    SPEND_CAP_REASON,
};
pub use drift::DriftGuard;
pub use errors::{
    anthropic_error, auth_error_response, key_refusal_response, no_store, LEDGER_BEHIND,
};
pub use exhaustion::exhaustion_response;
pub use headers::{outbound_headers, CredentialKind};
pub use hold::{hold, Commit, Held, HeldRead, Inspect, INSPECT_BUFFER};
pub use identity::{identifier_byte, identify, RequestIdentity};
pub use memory::{MemoryKey, Previous};
pub use messages::{
    new_request_id, router, Accounting, AccountingSource, Limits, Probe, ReadinessSource, Relay,
    RelayOutcome, ServedUnder, ShutdownMark, MEMORY_FLUSH_INTERVAL,
};
pub use portability::is_portable;
pub use ratelimit::{
    head_sample, retry_after, upstream_date, Bucket, BucketHeaders, UnifiedHeaders, FAST_INPUT,
    FAST_OUTPUT,
};
pub use serve::{handshake, serve, Accepted, HandshakeError, Limits as ListenLimits, LiveAcceptor};
pub use sse::{SseEvent, SseParser};

#[cfg(feature = "debug-hooks")]
impl Relay {
    /// How many decoders this relay built for the hold's reads of a response, so a test can tell that a held coded
    /// stream is decoded once beside the forward.
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
    /// A Claude Code inference request lacks an item the router reads of every one: refused before an account is
    /// chosen, since the fix is the client's environment and the body never decides a class.
    #[error("the request lacks the {item} the router requires of a Claude Code client")]
    ClientContract { item: &'static str },
    /// The client stamped a billing hash or a version suffix this build's rules do not reproduce. A stamp the
    /// router cannot verify never travels under an account's credential, edited or not: a wrong hash is
    /// indistinguishable from tampering and the cost falls on the account, not on the request. Relaying the
    /// body unedited under the client's stamp and stripping the stamp are the two alternatives this rejects.
    /// The cure is a re-pin to the client that moved. `resumed` marks a body whose first user message is the
    /// client's resume placeholder, a body the client stamped over a prompt the wire no longer carries, so the
    /// version suffix refusal's words name the resume apart.
    #[error("request body carries a billing hash this build's rules do not reproduce ({kind})")]
    ClientDrift { kind: &'static str, resumed: bool },
    #[error("the router admits Claude Code clients only")]
    ClientRefused,
    #[error("blocked by guardrail \"{name}\"")]
    GuardrailBlocked { name: String },
    #[error("request body names a model the relay will not repeat")]
    InvalidModel,
    #[error("the router is not accepting requests right now")]
    LedgerBehind,
    #[error("request body has no model")]
    NoModel,
    #[error("request body is not a JSON object")]
    NotAnObject,
    #[error("the request body's content-encoding is not supported. Send gzip or identity")]
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
