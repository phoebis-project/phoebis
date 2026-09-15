//! In-memory pairing sessions: an admin starts one, pastes back the OAuth callback, and this
//! validates it against the session before code exchange can run. A restart drops all sessions,
//! which is fine since a stalled pairing is meant to be restarted anyway.

use std::collections::HashMap;
use std::sync::Mutex;

use url::Url;

use crate::pkce::{generate_pkce, generate_state, Pkce};

/// Matches the ten-minute window Claude Code gives a pasted callback before treating it as stale.
const SESSION_TTL_MS: i64 = 600_000;

const LOCK_POISONED: &str = "pairing sessions mutex poisoned by a prior panic";

#[derive(Debug, Clone)]
pub struct PairingSession {
    pub id: String,
    pub account_id: String,
    pub admin_session_id: String,
    pub state: String,
    pub pkce: Pkce,
    pub redirect_uri: String,
    pub deadline: i64,
    pub status: PairingStatus,
}

#[derive(Clone)]
pub enum PairingStatus {
    Pending,
    CodeReceived { code: String },
    Exchanged { pending: PendingIdentity },
    /// Claimed by a confirmation that is installing the credential. Nothing else may end the session while
    /// it holds, which is what keeps a cancellation from racing the commit.
    Confirming { pending: PendingIdentity },
    Confirmed,
    Cancelled,
    Expired,
    Failed(String),
}

// `code` is the live OAuth authorization code; redact it so it can't leak through a `{:?}` format,
// including through `PairingSession`'s derived `Debug`. `pending` holds no secret, so it prints in full.
impl std::fmt::Debug for PairingStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PairingStatus::Pending => write!(f, "Pending"),
            PairingStatus::CodeReceived { code: _ } => f
                .debug_struct("CodeReceived")
                .field("code", &"***")
                .finish(),
            PairingStatus::Exchanged { pending } => f
                .debug_struct("Exchanged")
                .field("pending", pending)
                .finish(),
            PairingStatus::Confirming { pending } => f
                .debug_struct("Confirming")
                .field("pending", pending)
                .finish(),
            PairingStatus::Confirmed => write!(f, "Confirmed"),
            PairingStatus::Cancelled => write!(f, "Cancelled"),
            PairingStatus::Expired => write!(f, "Expired"),
            PairingStatus::Failed(reason) => f.debug_tuple("Failed").field(reason).finish(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PendingIdentity {
    pub account_uuid: String,
    pub email: String,
    pub organization_uuid: Option<String>,
    pub organization_name: Option<String>,
}

pub enum CallbackInput {
    Url(String),
    Code { code: String, state: String },
}

// The code carries the OAuth authorization code (and the URL variant embeds it too), so both
// arms redact it rather than let it land in a log line via a `{:?}` format.
impl std::fmt::Debug for CallbackInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallbackInput::Url(_) => f.debug_tuple("Url").field(&"***").finish(),
            CallbackInput::Code { state, .. } => f
                .debug_struct("Code")
                .field("code", &"***")
                .field("state", state)
                .finish(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PairingError {
    #[error("a pairing session is already in progress for this account")]
    Busy,
    #[error("pairing session not found")]
    NotFound,
    #[error("admin session does not own this pairing session")]
    Forbidden,
    #[error("pairing session expired")]
    Expired,
    #[error("state does not match the pairing session")]
    StateMismatch,
    #[error("callback does not match the registered redirect")]
    RedirectMismatch,
    #[error("pairing session already consumed")]
    AlreadyConsumed,
    #[error("invalid callback: {0}")]
    Invalid(String),
}

pub struct PairingSessions {
    sessions: Mutex<HashMap<String, PairingSession>>,
}

impl Default for PairingSessions {
    fn default() -> Self {
        Self::new()
    }
}

impl PairingSessions {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// One pending session per account: a second `start` while one is in flight is a caller bug,
    /// not a race to arbitrate, so it fails outright rather than replacing the first.
    pub fn start(
        &self,
        account_id: &str,
        admin_session_id: &str,
        redirect_uri: &str,
        now: i64,
    ) -> Result<PairingSession, PairingError> {
        let mut sessions = self.sessions.lock().expect(LOCK_POISONED);
        // An abandoned session must not hold an account `Busy` forever; expire stale entries
        // before scanning so an old one no longer blocks a fresh start.
        for session in sessions.values_mut() {
            expire_if_stale(session, now);
        }
        let busy = sessions
            .values()
            .any(|s| s.account_id == account_id && is_in_flight(&s.status));
        if busy {
            return Err(PairingError::Busy);
        }
        let session = PairingSession {
            id: generate_state(),
            account_id: account_id.to_string(),
            admin_session_id: admin_session_id.to_string(),
            state: generate_state(),
            pkce: generate_pkce(),
            redirect_uri: redirect_uri.to_string(),
            deadline: now + SESSION_TTL_MS,
            status: PairingStatus::Pending,
        };
        sessions.insert(session.id.clone(), session.clone());
        Ok(session)
    }

    pub fn get(&self, id: &str, admin_session_id: &str, now: i64) -> Option<PairingSession> {
        let mut sessions = self.sessions.lock().expect(LOCK_POISONED);
        let session = sessions.get_mut(id)?;
        if session.admin_session_id != admin_session_id {
            return None;
        }
        expire_if_stale(session, now);
        Some(session.clone())
    }

    pub fn accept_callback(
        &self,
        id: &str,
        admin_session_id: &str,
        callback: CallbackInput,
        now: i64,
    ) -> Result<PairingSession, PairingError> {
        let mut sessions = self.sessions.lock().expect(LOCK_POISONED);
        let session = sessions.get_mut(id).ok_or(PairingError::NotFound)?;
        if session.admin_session_id != admin_session_id {
            return Err(PairingError::Forbidden);
        }
        expire_if_stale(session, now);
        if matches!(session.status, PairingStatus::Expired) {
            return Err(PairingError::Expired);
        }
        if !matches!(session.status, PairingStatus::Pending) {
            return Err(PairingError::AlreadyConsumed);
        }

        let raw_code = match callback {
            CallbackInput::Url(url) => extract_code(&url, &session.redirect_uri, &session.state)?,
            CallbackInput::Code { code, state } => {
                if state != session.state {
                    return Err(PairingError::StateMismatch);
                }
                code
            }
        };

        // The authorization server appends the state again after `#` in the code it returns;
        // only the part before it is what the token endpoint accepts.
        let (code, fragment) = match raw_code.split_once('#') {
            Some((code, fragment)) => (code.to_string(), Some(fragment.to_string())),
            None => (raw_code, None),
        };
        if fragment.is_some_and(|f| f != session.state) {
            return Err(PairingError::StateMismatch);
        }

        session.status = PairingStatus::CodeReceived { code };
        Ok(session.clone())
    }

    /// Records the identity the exchanged credential belongs to, which is what an operator confirms
    /// against. The bundle itself stays with the caller: this store lives in memory and holds no
    /// credential, by design.
    pub fn mark_exchanged(
        &self,
        id: &str,
        admin_session_id: &str,
        pending: PendingIdentity,
        now: i64,
    ) -> Result<(), PairingError> {
        let mut sessions = self.sessions.lock().expect(LOCK_POISONED);
        let session = owned_session(&mut sessions, id, admin_session_id, now)?;
        if !matches!(session.status, PairingStatus::CodeReceived { .. }) {
            return Err(PairingError::AlreadyConsumed);
        }
        session.status = PairingStatus::Exchanged { pending };
        Ok(())
    }

    /// Claims an exchanged session for the confirmation that is about to install its credential. The status
    /// moves under this lock, so the caller holds the session for the rest of the commit: anything else that
    /// would end it, a cancellation above all, is refused from here on and cannot leave an account paired
    /// behind a session that says it was cancelled.
    pub fn claim_confirm(
        &self,
        id: &str,
        admin_session_id: &str,
        now: i64,
    ) -> Result<(), PairingError> {
        let mut sessions = self.sessions.lock().expect(LOCK_POISONED);
        let session = owned_session(&mut sessions, id, admin_session_id, now)?;
        let PairingStatus::Exchanged { pending } = &session.status else {
            return Err(PairingError::AlreadyConsumed);
        };
        session.status = PairingStatus::Confirming {
            pending: pending.clone(),
        };
        Ok(())
    }

    /// Ends a pairing that was committed. The session no longer occupies its account's one-at-a-time
    /// slot, and nothing further can be committed through it.
    pub fn confirm(&self, id: &str, admin_session_id: &str, now: i64) -> Result<(), PairingError> {
        let mut sessions = self.sessions.lock().expect(LOCK_POISONED);
        let session = owned_session(&mut sessions, id, admin_session_id, now)?;
        if !matches!(session.status, PairingStatus::Confirming { .. }) {
            return Err(PairingError::AlreadyConsumed);
        }
        session.status = PairingStatus::Confirmed;
        Ok(())
    }

    /// Ends a pairing that could not be completed, with the same words the operator is answered with. The
    /// account's slot is handed back, so the next attempt does not have to wait out the deadline.
    pub fn fail(
        &self,
        id: &str,
        admin_session_id: &str,
        reason: &str,
        now: i64,
    ) -> Result<(), PairingError> {
        let mut sessions = self.sessions.lock().expect(LOCK_POISONED);
        let session = owned_session(&mut sessions, id, admin_session_id, now)?;
        if !is_in_flight(&session.status) {
            return Err(PairingError::AlreadyConsumed);
        }
        session.status = PairingStatus::Failed(reason.to_string());
        Ok(())
    }

    pub fn cancel(&self, id: &str, admin_session_id: &str, now: i64) -> Result<(), PairingError> {
        let mut sessions = self.sessions.lock().expect(LOCK_POISONED);
        let session = sessions.get_mut(id).ok_or(PairingError::NotFound)?;
        if session.admin_session_id != admin_session_id {
            return Err(PairingError::Forbidden);
        }
        expire_if_stale(session, now);
        if matches!(session.status, PairingStatus::Expired) {
            return Err(PairingError::Expired);
        }
        // A confirmation already holds this session and may be part way through installing the credential.
        // Cancelling it here is what used to leave an account paired behind a cancelled session.
        if matches!(session.status, PairingStatus::Confirming { .. }) {
            return Err(PairingError::AlreadyConsumed);
        }
        session.status = PairingStatus::Cancelled;
        Ok(())
    }
}

/// The lookup, ownership, and staleness guard every transition runs before it reads a status.
fn owned_session<'a>(
    sessions: &'a mut HashMap<String, PairingSession>,
    id: &str,
    admin_session_id: &str,
    now: i64,
) -> Result<&'a mut PairingSession, PairingError> {
    let session = sessions.get_mut(id).ok_or(PairingError::NotFound)?;
    if session.admin_session_id != admin_session_id {
        return Err(PairingError::Forbidden);
    }
    expire_if_stale(session, now);
    if matches!(session.status, PairingStatus::Expired) {
        return Err(PairingError::Expired);
    }
    Ok(session)
}

/// Whether a session still occupies its account's one-pairing-at-a-time slot.
fn is_in_flight(status: &PairingStatus) -> bool {
    matches!(
        status,
        PairingStatus::Pending
            | PairingStatus::CodeReceived { .. }
            | PairingStatus::Confirming { .. }
            | PairingStatus::Exchanged { .. }
    )
}

/// Nothing sweeps in the background, so every read or mutation re-checks the deadline against the
/// caller's `now` and flips a stale in-flight session to `Expired` on the spot.
fn expire_if_stale(session: &mut PairingSession, now: i64) {
    if is_in_flight(&session.status) && now > session.deadline {
        session.status = PairingStatus::Expired;
    }
}

fn extract_code(url: &str, redirect_uri: &str, state: &str) -> Result<String, PairingError> {
    let parsed = Url::parse(url).map_err(|e| PairingError::Invalid(e.to_string()))?;
    let registered = Url::parse(redirect_uri).map_err(|e| PairingError::Invalid(e.to_string()))?;
    let redirect_matches = parsed.scheme() == registered.scheme()
        && parsed.host_str() == registered.host_str()
        && parsed.port_or_known_default() == registered.port_or_known_default()
        && parsed.path() == registered.path();
    if !redirect_matches {
        return Err(PairingError::RedirectMismatch);
    }
    let query_state = parsed
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.into_owned());
    if query_state.as_deref() != Some(state) {
        return Err(PairingError::StateMismatch);
    }
    parsed
        .query_pairs()
        .find(|(k, _)| k == "code")
        .map(|(_, v)| v.into_owned())
        .ok_or_else(|| PairingError::Invalid("callback is missing a code parameter".to_string()))
}
