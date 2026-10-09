//! Cookie sessions for the admin API: one admin secret, in-memory sessions, and a throttle that makes the secret
//! expensive to guess.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use phoebis_store::{digest_with, Secret};
use rand::Rng;
use subtle::ConstantTimeEq;

// A poisoned sessions mutex means a prior panic happened while the session map was being mutated, so
// neither the sessions nor the failure counter can be trusted afterwards.
const LOCK_POISONED: &str = "admin sessions mutex poisoned by a prior panic";

/// Failures tolerated inside `THROTTLE_WINDOW_MS` before every login is refused. Low on purpose: the
/// secret is long enough that a legitimate operator does not mistype it five times in a minute, and an
/// attacker gets five guesses per minute against the whole process.
const MAX_FAILURES: usize = 5;

const THROTTLE_WINDOW_MS: i64 = 60_000;

pub struct AdminSecrets {
    pub admin: Secret,
}

#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    #[error("secret does not match")]
    Invalid,
    #[error("too many failed logins")]
    Throttled,
}

struct SessionRec {
    created_at: i64,
    /// Names the session in pairing ownership and in logs, so the cookie value itself never travels
    /// beyond this module.
    id: String,
    last_seen: i64,
}

struct State {
    /// Unix milliseconds of each recent failure, oldest first; pruned to `THROTTLE_WINDOW_MS` on every login and
    /// bearer check.
    failures: Vec<i64>,
    /// Keyed by the cookie value, which is the session's credential.
    sessions: HashMap<String, SessionRec>,
}

pub struct Sessions {
    absolute: Duration,
    /// Keys the digests the login and the bearer compare, drawn per process: the compare needs equal lengths, not a
    /// secret key.
    compare_key: [u8; 32],
    idle: Duration,
    state: Mutex<State>,
}

impl Sessions {
    pub fn new(idle: Duration, absolute: Duration) -> Self {
        let mut compare_key = [0u8; 32];
        rand::rng().fill_bytes(&mut compare_key);
        Sessions {
            absolute,
            compare_key,
            idle,
            state: Mutex::new(State {
                failures: Vec::new(),
                sessions: HashMap::new(),
            }),
        }
    }

    /// Returns the cookie value to hand the browser.
    pub fn login(
        &self,
        presented: &str,
        secrets: &AdminSecrets,
        now: i64,
    ) -> Result<String, LoginError> {
        let mut state = self.state.lock().expect(LOCK_POISONED);
        self.check(&mut state, presented, secrets, now)?;
        let cookie = random_token();
        state.sessions.insert(
            cookie.clone(),
            SessionRec {
                created_at: now,
                id: random_token(),
                last_seen: now,
            },
        );
        Ok(cookie)
    }

    /// A scripted caller's one-shot check: the same keyed, constant-time compare as `login`, the same failure
    /// vector and the same throttle, so a bearer costs a guesser what a login costs. Opens no session.
    pub fn bearer(
        &self,
        presented: &str,
        secrets: &AdminSecrets,
        now: i64,
    ) -> Result<(), LoginError> {
        let mut state = self.state.lock().expect(LOCK_POISONED);
        self.check(&mut state, presented, secrets, now)
    }

    /// The throttle and the compare `login` and `bearer` share, under the caller's lock so a failure lands in the
    /// window the next check reads.
    fn check(
        &self,
        state: &mut State,
        presented: &str,
        secrets: &AdminSecrets,
        now: i64,
    ) -> Result<(), LoginError> {
        state.failures.retain(|at| now - *at < THROTTLE_WINDOW_MS);
        if state.failures.len() >= MAX_FAILURES {
            return Err(LoginError::Throttled);
        }
        // Digests of one length on both sides, so how long the compare runs says nothing about the secret's length.
        let expected = digest_with(&self.compare_key, secrets.admin.expose().as_bytes());
        let offered = digest_with(&self.compare_key, presented.as_bytes());
        if !bool::from(expected.ct_eq(&offered)) {
            state.failures.push(now);
            return Err(LoginError::Invalid);
        }
        Ok(())
    }

    /// Resolves a cookie into the session id, renewing the idle deadline. A session past either deadline is
    /// dropped here as well as by the sweep, so a dead cookie never resolves between two sweeps.
    pub fn authenticate(&self, cookie: &str, now: i64) -> Option<String> {
        let mut state = self.state.lock().expect(LOCK_POISONED);
        let session = state.sessions.get_mut(cookie)?;
        if self.ended(session, now) {
            state.sessions.remove(cookie);
            return None;
        }
        session.last_seen = now;
        Some(session.id.clone())
    }

    /// Drops every session past its idle or absolute deadline, so the map holds live sessions rather than every
    /// one a browser never presented again. Answers how many it dropped.
    pub fn sweep(&self, now: i64) -> usize {
        let mut state = self.state.lock().expect(LOCK_POISONED);
        let before = state.sessions.len();
        state.sessions.retain(|_, s| !self.ended(s, now));
        before - state.sessions.len()
    }

    /// Past either deadline; a session exactly at one still stands.
    fn ended(&self, session: &SessionRec, now: i64) -> bool {
        now.saturating_sub(session.last_seen) > self.idle.as_millis() as i64
            || now.saturating_sub(session.created_at) > self.absolute.as_millis() as i64
    }

    pub fn logout(&self, cookie: &str) {
        self.state
            .lock()
            .expect(LOCK_POISONED)
            .sessions
            .remove(cookie);
    }
}

/// 32 random bytes base64url, the same size the pairing sessions use for their own unguessable handles.
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secrets() -> AdminSecrets {
        AdminSecrets {
            admin: Secret::new("admin-secret"),
        }
    }

    #[test]
    fn a_sixth_failure_in_a_minute_is_throttled_and_the_window_slides() {
        let sessions = Sessions::new(Duration::from_secs(900), Duration::from_secs(43_200));
        for at in 0..5 {
            assert!(matches!(
                sessions.login("nope", &secrets(), at),
                Err(LoginError::Invalid)
            ));
        }
        assert!(matches!(
            sessions.login("admin-secret", &secrets(), 5),
            Err(LoginError::Throttled)
        ));
        assert!(sessions.login("admin-secret", &secrets(), 60_001).is_ok());
    }

    #[test]
    fn a_bearer_matches_as_a_login_does_and_opens_no_session() {
        let sessions = Sessions::new(Duration::from_secs(900), Duration::from_secs(43_200));
        assert!(sessions.bearer("admin-secret", &secrets(), 0).is_ok());
        assert!(matches!(
            sessions.bearer("admin-secre", &secrets(), 0),
            Err(LoginError::Invalid)
        ));
        assert!(matches!(
            sessions.bearer("admin-secret-and-more", &secrets(), 0),
            Err(LoginError::Invalid)
        ));
        assert!(sessions.state.lock().unwrap().sessions.is_empty());
        assert_eq!(sessions.state.lock().unwrap().failures, vec![0, 0]);
    }

    #[test]
    fn wrong_bearers_count_toward_the_login_throttle() {
        let sessions = Sessions::new(Duration::from_secs(900), Duration::from_secs(43_200));
        for at in 0..5 {
            assert!(matches!(
                sessions.bearer("nope", &secrets(), at),
                Err(LoginError::Invalid)
            ));
        }
        assert!(matches!(
            sessions.login("admin-secret", &secrets(), 5),
            Err(LoginError::Throttled)
        ));
        assert!(matches!(
            sessions.bearer("admin-secret", &secrets(), 5),
            Err(LoginError::Throttled)
        ));
        assert!(sessions.bearer("admin-secret", &secrets(), 60_001).is_ok());
    }

    #[test]
    fn a_session_expires_on_the_idle_and_the_absolute_deadline() {
        let sessions = Sessions::new(Duration::from_secs(900), Duration::from_secs(43_200));
        let cookie = sessions.login("admin-secret", &secrets(), 0).unwrap();
        let id = sessions.authenticate(&cookie, 900_000).unwrap();
        assert_ne!(
            id, cookie,
            "the cookie value stays inside the session store"
        );
        // Each call renews the idle deadline from itself, so a session in constant use only ever ends
        // on the absolute deadline.
        let mut at = 900_000;
        while at < 43_200_000 {
            at += 900_000;
            assert!(
                sessions.authenticate(&cookie, at).is_some(),
                "renewed at {at}"
            );
        }
        assert!(
            sessions.authenticate(&cookie, 43_200_001).is_none(),
            "the absolute deadline is not renewable"
        );

        let cookie = sessions.login("admin-secret", &secrets(), 0).unwrap();
        assert!(sessions.authenticate(&cookie, 900_001).is_none());

        let cookie = sessions.login("admin-secret", &secrets(), 0).unwrap();
        sessions.logout(&cookie);
        assert!(sessions.authenticate(&cookie, 1).is_none());
    }

    #[test]
    fn an_expired_admin_session_is_swept_rather_than_kept_until_presented() {
        let sessions = Sessions::new(Duration::from_secs(900), Duration::from_secs(43_200));
        let _idle = sessions.login("admin-secret", &secrets(), 0).unwrap();
        let busy = sessions.login("admin-secret", &secrets(), 0).unwrap();
        sessions.authenticate(&busy, 600_000).unwrap();
        assert_eq!(
            sessions.sweep(900_000),
            0,
            "a session at its idle deadline still stands"
        );
        assert_eq!(
            sessions.sweep(900_001),
            1,
            "the idle one leaves without ever being presented again"
        );
        assert_eq!(sessions.state.lock().unwrap().sessions.len(), 1);
        let mut at = 600_000;
        while at + 600_000 < 43_200_000 {
            at += 600_000;
            assert!(sessions.authenticate(&busy, at).is_some());
        }
        assert_eq!(
            sessions.sweep(43_200_001),
            1,
            "the absolute deadline sweeps a session in constant use"
        );
        assert!(sessions.state.lock().unwrap().sessions.is_empty());
    }
}
