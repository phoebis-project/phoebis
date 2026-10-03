//! Cookie sessions for the admin API: one admin secret, in-memory sessions, and a throttle that makes the secret
//! expensive to guess.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use phoebis_store::Secret;
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
    /// Unix milliseconds of each recent failure, oldest first; pruned to `THROTTLE_WINDOW_MS` on every login.
    failures: Vec<i64>,
    /// Keyed by the cookie value, which is the bearer credential for the session.
    sessions: HashMap<String, SessionRec>,
}

pub struct Sessions {
    absolute: Duration,
    idle: Duration,
    state: Mutex<State>,
}

impl Sessions {
    pub fn new(idle: Duration, absolute: Duration) -> Self {
        Sessions {
            absolute,
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
        state.failures.retain(|at| now - *at < THROTTLE_WINDOW_MS);
        if state.failures.len() >= MAX_FAILURES {
            return Err(LoginError::Throttled);
        }
        if !bool::from(
            secrets
                .admin
                .expose()
                .as_bytes()
                .ct_eq(presented.as_bytes()),
        ) {
            state.failures.push(now);
            return Err(LoginError::Invalid);
        }
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

    /// Resolves a cookie into the session id, renewing the idle deadline. A session past either deadline is
    /// dropped here rather than swept, so an idle process keeps no live sessions.
    pub fn authenticate(&self, cookie: &str, now: i64) -> Option<String> {
        let mut state = self.state.lock().expect(LOCK_POISONED);
        let session = state.sessions.get_mut(cookie)?;
        let idle_over = now.saturating_sub(session.last_seen) > self.idle.as_millis() as i64;
        let absolute_over =
            now.saturating_sub(session.created_at) > self.absolute.as_millis() as i64;
        if idle_over || absolute_over {
            state.sessions.remove(cookie);
            return None;
        }
        session.last_seen = now;
        Some(session.id.clone())
    }

    /// Whether the session named by `session_id` still stands, without renewing it: a live socket asks this on
    /// every message it sends, and a socket is not the activity the idle deadline measures.
    pub fn is_live(&self, session_id: &str, now: i64) -> bool {
        let state = self.state.lock().expect(LOCK_POISONED);
        state.sessions.values().any(|s| {
            s.id == session_id
                && now.saturating_sub(s.last_seen) <= self.idle.as_millis() as i64
                && now.saturating_sub(s.created_at) <= self.absolute.as_millis() as i64
        })
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
}
