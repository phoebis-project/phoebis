//! The owned, serializable events the relay publishes. Nothing here borrows from the relay, so a consumer can
//! keep an event for as long as it likes without holding a request open.

use bytes::Bytes;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Client,
    Keepalive,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Client => "client",
            Origin::Keepalive => "keepalive",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    CountTokens,
    Inference,
    Probe,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::CountTokens => "count_tokens",
            Kind::Inference => "inference",
            Kind::Probe => "probe",
        }
    }
}

/// Token counts as the upstream reported them. A field the upstream did not send stays `None`, never zero,
/// so a missing usage block is told apart from an empty one.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_5m_tokens: Option<u64>,
    pub cache_write_1h_tokens: Option<u64>,
    pub service_tier: Option<String>,
    pub cache_miss_reason: Option<String>,
}

impl Usage {
    /// `message_delta` repeats the counts it knows with running values, so the later value replaces the
    /// earlier one field by field and a field only `message_start` carried survives.
    pub fn merge(&mut self, later: &Usage) {
        if later.input_tokens.is_some() {
            self.input_tokens = later.input_tokens;
        }
        if later.output_tokens.is_some() {
            self.output_tokens = later.output_tokens;
        }
        if later.cache_read_tokens.is_some() {
            self.cache_read_tokens = later.cache_read_tokens;
        }
        if later.cache_write_5m_tokens.is_some() {
            self.cache_write_5m_tokens = later.cache_write_5m_tokens;
        }
        if later.cache_write_1h_tokens.is_some() {
            self.cache_write_1h_tokens = later.cache_write_1h_tokens;
        }
        if later.service_tier.is_some() {
            self.service_tier = later.service_tier.clone();
        }
        if later.cache_miss_reason.is_some() {
            self.cache_miss_reason = later.cache_miss_reason.clone();
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == Usage::default()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    FirstContentDelta,
    MessageDelta { usage: Usage },
    MessageStart { usage: Usage },
    MessageStop,
    ToolUseDelta,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttemptOutcome {
    AuthFailed,
    Canceled,
    RateLimited,
    Succeeded,
    TransportError,
    UpstreamError,
}

impl AttemptOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            AttemptOutcome::AuthFailed => "auth_failed",
            AttemptOutcome::Canceled => "canceled",
            AttemptOutcome::RateLimited => "rate_limited",
            AttemptOutcome::Succeeded => "succeeded",
            AttemptOutcome::TransportError => "transport_error",
            AttemptOutcome::UpstreamError => "upstream_error",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Canceled,
    Exhausted,
    Failed,
    Rejected,
    Succeeded,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Canceled => "canceled",
            Outcome::Exhausted => "exhausted",
            Outcome::Failed => "failed",
            Outcome::Rejected => "rejected",
            Outcome::Succeeded => "succeeded",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CooldownSet {
    pub scope: String,
    pub until: i64,
    pub reason: String,
    pub window: String,
}

#[derive(Clone, Debug)]
pub struct RequestObserved {
    pub request_id: String,
    pub origin: Origin,
    pub kind: Kind,
    pub key_id: Option<String>,
    pub pool_id: Option<String>,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub parent_agent_id: Option<String>,
    pub owner_id: Option<String>,
    pub model: String,
    pub canonical_model: String,
    pub stream: bool,
    pub headers: Vec<(String, String)>,
    /// The body as received: refcounted, so publishing costs no copy.
    pub body: Bytes,
    /// The key's effective capture window; zero means the ledger keeps no bodies for this request.
    pub body_capture_hours: u32,
    pub received_at: i64,
}

#[derive(Clone, Debug)]
pub struct AccountSelected {
    pub request_id: String,
    pub attempt: u8,
    pub account_id: String,
    pub account_kind: String,
    pub pin_epoch: Option<u64>,
    pub started_at: i64,
}

#[derive(Clone, Debug)]
pub struct ResponseHead {
    pub request_id: String,
    pub attempt: u8,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// The data of the committing event of a held stream: `message_start` or the `error` that failed it.
    pub first_event: Option<Bytes>,
    pub outcome: AttemptOutcome,
    pub cooldown: Option<CooldownSet>,
    /// When the request body was flushed to the upstream, so a waterfall can show the write phase.
    pub written_at: i64,
    pub observed_at: i64,
}

#[derive(Clone, Debug)]
pub struct ResponseChunk {
    pub request_id: String,
    pub attempt: u8,
    pub index: u32,
    /// The decoded inspection copy, so a consumer reads text whatever the wire coding was.
    pub body: Bytes,
    pub phase: Option<Phase>,
}

#[derive(Clone, Debug)]
pub struct RequestComplete {
    pub request_id: String,
    pub attempt: u8,
    pub account_id: Option<String>,
    pub outcome: Outcome,
    pub status: u16,
    pub usage: Option<Usage>,
    pub ttfb_ms: Option<i64>,
    pub ttfc_ms: Option<i64>,
    pub error_code: Option<String>,
    pub client_drift: Option<String>,
    pub client_version: Option<String>,
    pub response_bytes: Option<u64>,
    pub finished_at: i64,
}

/// One queue carries every variant, so a subscriber sees a request's chunks between its head and its
/// completion and never has to reconcile two receivers.
#[derive(Clone, Debug)]
pub enum Event {
    AccountSelected(AccountSelected),
    RequestComplete(RequestComplete),
    RequestObserved(RequestObserved),
    ResponseChunk(ResponseChunk),
    ResponseHead(ResponseHead),
}

impl Event {
    pub fn request_id(&self) -> &str {
        match self {
            Event::AccountSelected(e) => &e.request_id,
            Event::RequestComplete(e) => &e.request_id,
            Event::RequestObserved(e) => &e.request_id,
            Event::ResponseChunk(e) => &e.request_id,
            Event::ResponseHead(e) => &e.request_id,
        }
    }
}

/// Request headers no consumer may see, mirroring the relay's outbound rule so that what keepalive replays is
/// what the caller sent. It holds the relay's `NEVER_COPIED` set (a unit test in crates/relay/src/headers.rs
/// asserts the two agree, since the bus may not depend on the relay), the credentials, and the framing headers
/// the relay regenerates for its own hop.
///
/// `accept-encoding` is the one entry that is neither: the relay decodes a compressed upstream answer only for
/// a caller that sent none, so a replayed one would hand compressed bytes to the probe's usage parser and pause
/// every recipe it ran for.
pub const HEADER_DENYLIST: &[&str] = &[
    "accept-encoding",
    "authorization",
    "connection",
    "content-length",
    "cookie",
    "forwarded",
    "host",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "via",
    "x-api-key",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-port",
    "x-forwarded-proto",
    "x-forwarded-server",
    "x-keepalive-owner-id",
    "x-real-ip",
];

/// Everything the denylist does not name travels, so a header the client starts sending after this code was
/// written still reaches a replay as the caller spelled it.
pub fn header_published(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    !HEADER_DENYLIST.contains(&lower.as_str())
}
