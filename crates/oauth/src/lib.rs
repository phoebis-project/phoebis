//! Pairing and refresh.

mod authorize;
mod exchange;
mod inflight;
mod limit_resets;
mod manager;
mod pkce;
pub mod random;
mod session;
mod usage;

pub use authorize::{authorization_url, ephemeral_port, loopback_redirect_uri, LoginKind};
pub use exchange::{
    control_request, exchange_body, parse_profile, parse_token_response, refresh_body, ControlKind,
    OauthError, ProfileResponse, TokenAccount, TokenOrg, TokenResponse,
};
pub use limit_resets::{
    account_window_id, grant_states, resets_left, ClaimAnswer, Grant, GrantState, GrantStatus,
    LimitResets, SEVEN_DAY_FABLE, SEVEN_DAY_OVERAGE_INCLUDED,
};
// The usage service is the parser's one production caller, and it reaches it inside the crate. A rig that stores a
// block writes it from a usage body, in the upstream's field names.
#[cfg(any(test, feature = "test-support"))]
pub use limit_resets::parse_limit_resets;
pub use manager::{AccessToken, AcquireError, Backoff, Clock, CredentialManager, LandedMark};
pub use manager::{SERVER_ERROR_BACKOFF_MS, TRANSIENT_BACKOFF_MS};
pub use pkce::{generate_pkce, generate_state, Pkce};
pub use session::{
    CallbackInput, PairingError, PairingSession, PairingSessions, PairingStatus, PendingIdentity,
    Started, SESSION_TTL_MS,
};
pub use usage::{
    parse_usage, BackoffEdge, BackoffEndCause, BackoffHook, Claim, ClaimOutcome, ClaimRefusal,
    ClaimReply, ExtraUsage, LatestStanding, LimitResetsMemory, ParsedUsage, PollFault, PollIssue,
    PollOutcome, PollStatus, Refused, SpendReading, UsageError, UsageService, UsageSnapshot,
    UsageWindow, BACKOFF_CAP_MS, FABLE_WINDOW_KEY,
};
