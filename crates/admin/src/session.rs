//! Cookie sessions for the admin API: one admin secret, in-memory sessions, signed tokens for a login that asks to
//! stay, and a throttle that makes the secret expensive to guess.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use phoebis_store::{digest_with, Secret, Store, StoreError};
use subtle::ConstantTimeEq;

// A poisoned sessions mutex means a prior panic happened while the session map was being mutated, so
// neither the sessions nor the failure counter can be trusted afterwards.
const LOCK_POISONED: &str = "admin sessions mutex poisoned by a prior panic";

/// Failures tolerated inside `THROTTLE_WINDOW_MS` before every login is refused. Low on purpose: the
/// secret is long enough that a legitimate operator does not mistype it five times in a minute, and an
/// attacker gets five guesses per minute against the whole process.
const MAX_FAILURES: usize = 5;

const THROTTLE_WINDOW_MS: i64 = 60_000;

/// A kept login's life from its last read: the token's own bound and its cookie's `Max-Age`, so the browser drops the
/// cookie when the router would refuse it.
pub const PERSISTENT_LIFE: Duration = Duration::from_secs(2_592_000);

const SESSION_ID_LEN: usize = 32;
const ISSUED_AT_LEN: usize = 8;
const NONCE_LEN: usize = 16;
const TAG_LEN: usize = 32;
const SIGNED_LEN: usize = SESSION_ID_LEN + ISSUED_AT_LEN + NONCE_LEN;

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
    /// Signs the persistent tokens. Derived from the admin secret and the store's session epoch, so a rotation of the
    /// secret or a persistent logout ends every token signed before it, on this process and after a restart.
    token_key: [u8; 32],
}

/// What a presented cookie resolved to. `id` is the session id the routes key on: the map session's own, or the
/// token's id drawn at the login.
pub enum Authenticated {
    Map {
        id: String,
    },
    /// `renewed_cookie` is the same session's token under a fresh issue instant and nonce, for the answer to set.
    Persistent {
        id: String,
        renewed_cookie: String,
    },
}

pub struct Sessions {
    absolute: Duration,
    /// Keys the digests the login and the bearer compare, drawn per process: the compare needs equal lengths, not a
    /// secret key.
    compare_key: [u8; 32],
    idle: Duration,
    secret: Secret,
    state: Mutex<State>,
    store: Arc<Store>,
}

impl Sessions {
    /// Derives the persistent tokens' key from `secret` and the epoch `store` holds, so a token signed before a
    /// restart verifies after it.
    pub fn new(
        idle: Duration,
        absolute: Duration,
        secret: Secret,
        store: Arc<Store>,
    ) -> Result<Self, StoreError> {
        let token_key = token_key(&secret, store.admin_session_epoch()?);
        let mut compare_key = [0u8; 32];
        phoebis_oauth::random::fill(&mut compare_key);
        Ok(Sessions {
            absolute,
            compare_key,
            idle,
            secret,
            state: Mutex::new(State {
                failures: Vec::new(),
                sessions: HashMap::new(),
                token_key,
            }),
            store,
        })
    }

    /// Returns the cookie value to hand the browser.
    pub fn login(&self, presented: &str, now: i64) -> Result<String, LoginError> {
        let mut state = self.state.lock().expect(LOCK_POISONED);
        self.check(&mut state, presented, now)?;
        let cookie = phoebis_oauth::generate_state();
        state.sessions.insert(
            cookie.clone(),
            SessionRec {
                created_at: now,
                id: phoebis_oauth::generate_state(),
                last_seen: now,
            },
        );
        Ok(cookie)
    }

    /// A token session keyed by the admin secret and the store's session epoch, surviving a restart and ending at a
    /// rotation or at a persistent logout. The same compare and throttle as `login` run first, and the session id
    /// drawn here is kept by every renewal.
    pub fn login_persistent(&self, presented: &str, now: i64) -> Result<String, LoginError> {
        let mut state = self.state.lock().expect(LOCK_POISONED);
        self.check(&mut state, presented, now)?;
        let mut session_id = [0u8; SESSION_ID_LEN];
        phoebis_oauth::random::fill(&mut session_id);
        Ok(sign(&state.token_key, &session_id, now))
    }

    /// A scripted caller's one-shot check: the same keyed, constant-time compare as `login`, the same failure
    /// vector and the same throttle, so a bearer costs a guesser what a login costs. Opens no session.
    pub fn bearer(&self, presented: &str, now: i64) -> Result<(), LoginError> {
        let mut state = self.state.lock().expect(LOCK_POISONED);
        self.check(&mut state, presented, now)
    }

    /// The throttle and the compare `login`, `login_persistent` and `bearer` share, under the caller's lock so a
    /// failure lands in the window the next check reads.
    fn check(&self, state: &mut State, presented: &str, now: i64) -> Result<(), LoginError> {
        state.failures.retain(|at| now - *at < THROTTLE_WINDOW_MS);
        if state.failures.len() >= MAX_FAILURES {
            return Err(LoginError::Throttled);
        }
        // Digests of one length on both sides, so how long the compare runs says nothing about the secret's length.
        let expected = digest_with(&self.compare_key, self.secret.expose().as_bytes());
        let offered = digest_with(&self.compare_key, presented.as_bytes());
        if !bool::from(expected.ct_eq(&offered)) {
            state.failures.push(now);
            return Err(LoginError::Invalid);
        }
        Ok(())
    }

    /// Resolves a cookie into its session. A map session renews its idle deadline, and one past either deadline is
    /// dropped here as well as by the sweep, so a dead cookie never resolves between two sweeps. A cookie the map
    /// does not hold is read as a persistent token: it resolves while its tag verifies under the current key and its
    /// 30 days have not passed, and answers its renewal.
    pub fn authenticate(&self, cookie: &str, now: i64) -> Option<Authenticated> {
        let mut state = self.state.lock().expect(LOCK_POISONED);
        if let Some(session) = state.sessions.get_mut(cookie) {
            if self.ended(session, now) {
                state.sessions.remove(cookie);
                return None;
            }
            session.last_seen = now;
            return Some(Authenticated::Map {
                id: session.id.clone(),
            });
        }
        let session_id = verified(&state.token_key, cookie, now)?;
        Some(Authenticated::Persistent {
            id: URL_SAFE_NO_PAD.encode(session_id),
            renewed_cookie: sign(&state.token_key, &session_id, now),
        })
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

    /// Ends every persistent token of the install: the store's epoch moves on and the key is derived again.
    pub fn logout_persistent(&self) -> Result<(), StoreError> {
        // The bump and the derivation under one lock, so two logouts leave the key on the epoch the store holds.
        let mut state = self.state.lock().expect(LOCK_POISONED);
        let epoch = self.store.bump_admin_session_epoch()?;
        state.token_key = token_key(&self.secret, epoch);
        Ok(())
    }
}

fn token_key(secret: &Secret, epoch: u64) -> [u8; 32] {
    let secret = secret.expose().as_bytes();
    // Sized up front so the secret's copy never moves to a second allocation the wipe below would miss.
    let mut input = Vec::with_capacity(secret.len() + 8);
    input.extend_from_slice(secret);
    input.extend_from_slice(&epoch.to_be_bytes());
    let key = digest_with(&[0; 32], &input);
    // The admin secret's copy is wiped before the buffer is freed, and `black_box` keeps the compiler from
    // dropping the writes to a buffer nothing reads again.
    input.fill(0);
    std::hint::black_box(&input);
    key
}

/// `base64url(session_id || issued_at_ms || nonce || tag)`, the tag the digest of the rest under `key` and the nonce
/// fresh on each, so no two renewals of one session read the same.
fn sign(key: &[u8; 32], session_id: &[u8; SESSION_ID_LEN], now: i64) -> String {
    let mut token = Vec::with_capacity(SIGNED_LEN + TAG_LEN);
    token.extend_from_slice(session_id);
    token.extend_from_slice(&now.to_be_bytes());
    let mut nonce = [0u8; NONCE_LEN];
    phoebis_oauth::random::fill(&mut nonce);
    token.extend_from_slice(&nonce);
    let tag = digest_with(key, &token);
    token.extend_from_slice(&tag);
    URL_SAFE_NO_PAD.encode(token)
}

/// The session id of a token whose tag verifies under `key` and whose 30 days have not passed at `now`.
fn verified(key: &[u8; 32], cookie: &str, now: i64) -> Option<[u8; SESSION_ID_LEN]> {
    let token = URL_SAFE_NO_PAD.decode(cookie).ok()?;
    if token.len() != SIGNED_LEN + TAG_LEN {
        return None;
    }
    let (signed, tag) = token.split_at(SIGNED_LEN);
    if !bool::from(digest_with(key, signed).as_slice().ct_eq(tag)) {
        return None;
    }
    let (session_id, rest) = signed.split_at(SESSION_ID_LEN);
    let issued_at = i64::from_be_bytes(rest[..ISSUED_AT_LEN].try_into().ok()?);
    let life = PERSISTENT_LIFE.as_millis() as i64;
    if issued_at.saturating_add(life) <= now {
        return None;
    }
    session_id.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Arc<Store> {
        Arc::new(Store::open_in_memory(&phoebis_store::StorageKey::generate()).unwrap())
    }

    fn sessions_on(store: &Arc<Store>, secret: &str) -> Sessions {
        Sessions::new(
            Duration::from_secs(900),
            Duration::from_secs(43_200),
            Secret::new(secret),
            store.clone(),
        )
        .unwrap()
    }

    fn sessions() -> Sessions {
        sessions_on(&store(), "admin-secret")
    }

    const DAY_MS: i64 = 86_400_000;

    /// The kept session's id, verified, and the token the answer would set, or `None` where the cookie is refused
    /// or resolves to a map session.
    fn kept(sessions: &Sessions, cookie: &str, now: i64) -> Option<(String, String)> {
        match sessions.authenticate(cookie, now)? {
            Authenticated::Persistent { id, renewed_cookie } => Some((id, renewed_cookie)),
            Authenticated::Map { .. } => None,
        }
    }

    fn decoded(token: &str) -> Vec<u8> {
        URL_SAFE_NO_PAD.decode(token).unwrap()
    }

    #[test]
    fn a_kept_tokens_renewal_verifies_and_keeps_the_session_id() {
        let sessions = sessions();
        let token = sessions.login_persistent("admin-secret", 1_000).unwrap();
        let (id, renewed) = kept(&sessions, &token, 5_000).expect("the token verifies");
        let (again, _) = kept(&sessions, &renewed, 6_000).expect("the renewal verifies");
        assert_eq!(again, id, "the renewal keeps the session id");
    }

    #[test]
    fn a_kept_login_runs_the_logins_compare_and_throttle() {
        let sessions = sessions();
        for at in 0..5 {
            assert!(matches!(
                sessions.login_persistent("nope", at),
                Err(LoginError::Invalid)
            ));
        }
        assert!(matches!(
            sessions.login_persistent("admin-secret", 5),
            Err(LoginError::Throttled)
        ));
        assert!(matches!(
            sessions.login("admin-secret", 5),
            Err(LoginError::Throttled)
        ));
        assert!(sessions.login_persistent("admin-secret", 60_001).is_ok());
    }

    #[test]
    fn a_kept_token_verifies_by_its_tag_and_a_changed_byte_anywhere_fails() {
        let sessions = sessions();
        let token = sessions.login_persistent("admin-secret", 0).unwrap();
        assert!(kept(&sessions, &token, 1).is_some());
        let bytes = decoded(&token);
        for (at, part) in [
            (0, "the session id"),
            (32, "the issue instant"),
            (40, "the nonce"),
            (56, "the tag"),
        ] {
            let mut changed = bytes.clone();
            changed[at] ^= 1;
            assert!(
                sessions
                    .authenticate(&URL_SAFE_NO_PAD.encode(&changed), 1)
                    .is_none(),
                "a changed byte in {part}"
            );
        }
        assert!(
            sessions
                .authenticate(&URL_SAFE_NO_PAD.encode(&bytes[..87]), 1)
                .is_none(),
            "a short token"
        );
    }

    #[test]
    fn a_kept_token_ends_thirty_days_after_its_issue() {
        let sessions = sessions();
        let token = sessions.login_persistent("admin-secret", 0).unwrap();
        assert!(kept(&sessions, &token, 30 * DAY_MS - 1).is_some());
        assert!(sessions.authenticate(&token, 30 * DAY_MS).is_none());
        let (_, renewed) = kept(&sessions, &token, 29 * DAY_MS).unwrap();
        assert!(
            kept(&sessions, &renewed, 58 * DAY_MS).is_some(),
            "a renewal counts its 30 days from the read that issued it"
        );
    }

    #[test]
    fn a_key_derived_under_a_moved_epoch_refuses_an_earlier_token() {
        let store = store();
        let sessions = sessions_on(&store, "admin-secret");
        let earlier = sessions.login_persistent("admin-secret", 0).unwrap();
        assert!(kept(&sessions_on(&store, "admin-secret"), &earlier, 1).is_some());
        sessions.logout_persistent().unwrap();
        assert_eq!(store.admin_session_epoch().unwrap(), 1);
        assert!(sessions.authenticate(&earlier, 1).is_none());
        let restarted = sessions_on(&store, "admin-secret");
        assert!(restarted.authenticate(&earlier, 1).is_none());
        let later = sessions.login_persistent("admin-secret", 2).unwrap();
        assert!(kept(&restarted, &later, 3).is_some());
    }

    #[test]
    fn a_kept_token_fails_under_another_admin_secret() {
        let store = store();
        let first = sessions_on(&store, "admin-secret");
        let token = first.login_persistent("admin-secret", 0).unwrap();
        let rotated = sessions_on(&store, "another-admin-secret");
        assert!(rotated.authenticate(&token, 1).is_none());
        assert!(kept(&sessions_on(&store, "admin-secret"), &token, 1).is_some());
    }

    #[test]
    fn a_sixth_failure_in_a_minute_is_throttled_and_the_window_slides() {
        let sessions = sessions();
        for at in 0..5 {
            assert!(matches!(
                sessions.login("nope", at),
                Err(LoginError::Invalid)
            ));
        }
        assert!(matches!(
            sessions.login("admin-secret", 5),
            Err(LoginError::Throttled)
        ));
        assert!(sessions.login("admin-secret", 60_001).is_ok());
    }

    #[test]
    fn a_bearer_matches_as_a_login_does_and_opens_no_session() {
        let sessions = sessions();
        assert!(sessions.bearer("admin-secret", 0).is_ok());
        assert!(matches!(
            sessions.bearer("admin-secre", 0),
            Err(LoginError::Invalid)
        ));
        assert!(matches!(
            sessions.bearer("admin-secret-and-more", 0),
            Err(LoginError::Invalid)
        ));
        assert_eq!(
            sessions.sweep(43_200_001),
            0,
            "past every deadline the sweep finds no session the bearer opened"
        );
    }

    #[test]
    fn wrong_bearers_count_toward_the_login_throttle() {
        let sessions = sessions();
        for at in 0..5 {
            assert!(matches!(
                sessions.bearer("nope", at),
                Err(LoginError::Invalid)
            ));
        }
        assert!(matches!(
            sessions.login("admin-secret", 5),
            Err(LoginError::Throttled)
        ));
        assert!(matches!(
            sessions.bearer("admin-secret", 5),
            Err(LoginError::Throttled)
        ));
        assert!(sessions.bearer("admin-secret", 60_001).is_ok());
    }

    #[test]
    fn a_session_expires_on_the_idle_and_the_absolute_deadline() {
        let sessions = sessions();
        let cookie = sessions.login("admin-secret", 0).unwrap();
        let Some(Authenticated::Map { id }) = sessions.authenticate(&cookie, 900_000) else {
            panic!("a login without the flag resolves to a map session");
        };
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

        let cookie = sessions.login("admin-secret", 0).unwrap();
        assert!(sessions.authenticate(&cookie, 900_001).is_none());

        let cookie = sessions.login("admin-secret", 0).unwrap();
        sessions.logout(&cookie);
        assert!(sessions.authenticate(&cookie, 1).is_none());
    }

    #[test]
    fn an_expired_admin_session_is_swept_rather_than_kept_until_presented() {
        let sessions = sessions();
        let _idle = sessions.login("admin-secret", 0).unwrap();
        let busy = sessions.login("admin-secret", 0).unwrap();
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
