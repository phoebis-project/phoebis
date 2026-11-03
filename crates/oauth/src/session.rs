//! In-memory pairing sessions: an admin starts one, pastes back the OAuth callback, and this
//! validates it against the session before code exchange can run. A restart drops all sessions,
//! which is fine since a stalled pairing is meant to be restarted anyway.

use std::collections::HashMap;
use std::sync::Mutex;

use url::Url;

use crate::pkce::{generate_pkce, generate_state, Pkce};

/// A pairing's lifetime: matches the ten-minute window Claude Code gives a pasted callback before treating it as
/// stale, and is how long an ended pairing stays readable before the sweep drops it.
pub const SESSION_TTL_MS: i64 = 600_000;

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
    /// When the status moved to `Confirmed`, `Canceled`, `Expired` or `Failed`. The sweep keeps an ended session
    /// readable for one lifetime past it.
    pub ended_at: Option<i64>,
}

#[derive(Clone)]
pub enum PairingStatus {
    Pending,
    CodeReceived {
        code: String,
    },
    Exchanged {
        pending: PendingIdentity,
    },
    /// Claimed by a confirmation that is installing the credential. Nothing else may end the session while
    /// it holds, which is what keeps a cancellation from racing the commit.
    Confirming {
        pending: PendingIdentity,
    },
    Confirmed,
    Canceled,
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
            PairingStatus::Canceled => write!(f, "Canceled"),
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

#[derive(Debug, Clone)]
pub struct Started {
    pub session: PairingSession,
    /// The id of the caller's own pairing this start canceled, so the route can discard its vault record.
    pub superseded: Option<String>,
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

    /// One pending session per account. A second start by the admin session that owns the standing one supersedes
    /// it: a reload or a closed tab leaves a pairing that would otherwise hold the account for ten minutes. A start
    /// while a confirmation is installing the credential, or by another admin session, is refused.
    pub fn start(
        &self,
        account_id: &str,
        admin_session_id: &str,
        redirect_uri: &str,
        now: i64,
    ) -> Result<Started, PairingError> {
        let mut sessions = self.sessions.lock().expect(LOCK_POISONED);
        // An abandoned session must not hold an account `Busy` forever; expire stale entries
        // before scanning so an old one neither blocks a fresh start nor reads as superseded.
        for session in sessions.values_mut() {
            expire_if_stale(session, now);
        }
        let mut superseded = None;
        // Canceling inside the scan is safe only because this is the one insert and it leaves an account at
        // most one pairing in flight, so no refusal can follow a cancel made earlier in the same loop.
        for standing in sessions
            .values_mut()
            .filter(|s| s.account_id == account_id && is_in_flight(&s.status))
        {
            if standing.admin_session_id != admin_session_id
                || matches!(standing.status, PairingStatus::Confirming { .. })
            {
                return Err(PairingError::Busy);
            }
            end(standing, PairingStatus::Canceled, now);
            superseded = Some(standing.id.clone());
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
            ended_at: None,
        };
        sessions.insert(session.id.clone(), session.clone());
        Ok(Started {
            session,
            superseded,
        })
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
        let session = owned_session(&mut sessions, id, admin_session_id, now)?;
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
    /// behind a session that says it was canceled.
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
        end(session, PairingStatus::Confirmed, now);
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
        end(session, PairingStatus::Failed(reason.to_string()), now);
        Ok(())
    }

    pub fn cancel(&self, id: &str, admin_session_id: &str, now: i64) -> Result<(), PairingError> {
        let mut sessions = self.sessions.lock().expect(LOCK_POISONED);
        let session = owned_session(&mut sessions, id, admin_session_id, now)?;
        // An ended session keeps the status it ended with: a confirm already installed the credential, a failure
        // already named its reason, a cancel already counted; relabeling any of them would say the opposite of
        // what happened.
        if !is_in_flight(&session.status) {
            return Err(PairingError::AlreadyConsumed);
        }
        // A confirmation already holds this session and may be part way through installing the credential.
        // Canceling it here is what used to leave an account paired behind a canceled session.
        if matches!(session.status, PairingStatus::Confirming { .. }) {
            return Err(PairingError::AlreadyConsumed);
        }
        end(session, PairingStatus::Canceled, now);
        Ok(())
    }

    /// Drops every session one lifetime after it ended, or one lifetime past a deadline it never ended at, so the
    /// map does not keep an entry per pairing ever started. Answers how many it dropped.
    pub fn sweep(&self, now: i64) -> usize {
        let mut sessions = self.sessions.lock().expect(LOCK_POISONED);
        let before = sessions.len();
        sessions.retain(|_, session| {
            now < session.ended_at.unwrap_or(session.deadline) + SESSION_TTL_MS
        });
        before - sessions.len()
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

/// The sweep drops sessions rather than expiring them, and only once a minute, so every read or mutation
/// re-checks the deadline against the caller's `now` and flips a stale in-flight session to `Expired` on the spot.
fn expire_if_stale(session: &mut PairingSession, now: i64) {
    if is_in_flight(&session.status) && now > session.deadline {
        end(session, PairingStatus::Expired, now);
    }
}

/// Keeps the first end's stamp, which the sweep counts a session's lifetime from. Every caller ends a session still
/// in flight, and `cancel` refuses one that has ended.
fn end(session: &mut PairingSession, status: PairingStatus, now: i64) {
    session.status = status;
    session.ended_at.get_or_insert(now);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pairing_leaves_ten_minutes_after_it_ended_or_after_a_deadline_it_never_ended_at() {
        let sessions = PairingSessions::new();
        let redirect = "http://localhost:1/callback";
        let abandoned = sessions.start("a", "admin", redirect, 0).unwrap().session;
        let canceled = sessions.start("b", "admin", redirect, 0).unwrap().session;
        sessions.cancel(&canceled.id, "admin", 1_000).unwrap();
        assert_eq!(
            sessions.sweep(600_999),
            0,
            "the canceled one stays readable for ten minutes after it ended"
        );
        assert_eq!(sessions.sweep(601_000), 1);
        assert!(sessions.get(&canceled.id, "admin", 601_000).is_none());
        assert_eq!(
            sessions.sweep(SESSION_TTL_MS * 2 - 1),
            0,
            "one never read past its deadline stays until ten minutes after it"
        );
        assert_eq!(sessions.sweep(SESSION_TTL_MS * 2), 1);
        assert!(sessions
            .get(&abandoned.id, "admin", SESSION_TTL_MS * 2)
            .is_none());
    }
}
