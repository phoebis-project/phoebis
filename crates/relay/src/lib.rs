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
mod features;
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

pub use auth::{authenticated_key, is_json};
pub use billing::is_entrypoint_token;
pub use body::{
    schema_cache_controls, CACHE_DIAGNOSIS_BETA, CLASSIFIER_BETA, DANGEROUS_TOOL_USE_BETA,
    EXTENDED_TTL_BETA,
};
pub use cache_ttl::{CacheTtl, CacheTtlPolicy, ModelTtl, TtlBranch};
pub use casing::OriginalHead;
pub use catalog::{
    Catalog, CatalogChange, CatalogService, DiscoveryError, Warmth, EFFORT_LEVELS, ERROR_HOLD,
    MAX_BODY, REFRESH_TICK,
};
pub use chains::Chains;
pub use classify::{Cooldown, Verdict, SPEND_CAP_REASON};
pub use drift::DriftKind;
pub use errors::{
    anthropic_error, no_store, no_store_answer, ACCOUNTS_EXHAUSTED, ACCOUNTS_UNAVAILABLE,
    BODY_OVER_BUDGET, CLIENT_GONE_CODE, ERROR_CODE_HEADER, LEDGER_BEHIND,
};
pub use hold::INSPECT_BUFFER;
pub use identity::{
    identifier_byte, is_filed_word, is_valid_agent_id, is_valid_id, MAX_FILED_WORD_LEN,
};
pub use memory::{MemoryKey, MEMORY_CAP, MEMORY_IDLE_MS};
pub use messages::{
    new_request_id, router, Accounting, AccountingSource, Limits, Probe, ProbeObstacle, Relay,
    RelayOutcome, ShutdownMark, MEMORY_FLUSH_INTERVAL,
};
pub use observe::RELAY_MISS_REASONS;
pub use ratelimit::{retry_after, UnifiedHeaders, FAST_INPUT, FAST_OUTPUT};
pub use serve::{
    accept_loop, http1_builder, serve, serve_features, Accepted, Limits as ListenLimits,
    LiveAcceptor,
};
pub use sse::SseEvent;

pub(crate) use body::{edit_body, BodyEdits, BodyMember, Markerless, NativeEdit, SafeguardsEdit};

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
    /// A body that holds more elements than `MAX_BODY_ELEMENTS`, refused before it is parsed.
    #[error("request body holds more elements than the router reads")]
    BodyTooManyElements,
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
    #[error("request body carries a billing hash this build's rules do not reproduce ({})", .kind.as_str())]
    ClientDrift { kind: DriftKind, resumed: bool },
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

phoebis_store::text_enum! {
    plain
    /// How the edited body leaves toward the upstream, `proxy.upstream_gzip`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum UpstreamGzip {
        /// Gzip when the client sent gzip, identity otherwise.
        Auto => "auto",
        Off => "off",
        On => "on",
    }
}
