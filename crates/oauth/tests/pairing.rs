use phoebis_oauth::{
    authorization_url, generate_pkce, generate_state, CallbackInput, LoginKind, PairingError,
    PairingSessions, PairingStatus, PendingIdentity,
};
use phoebis_profile::claude_code_2_1_269_linux_x64;

#[test]
fn authorization_url_has_the_measured_parameter_order() {
    let p = claude_code_2_1_269_linux_x64();
    let url = authorization_url(
        p,
        LoginKind::ClaudeAi,
        "http://localhost:4567/callback",
        p.control.default_login_scopes,
        "CHALLENGE",
        "STATE",
    );
    let (base, query) = url.split_once('?').unwrap();
    assert_eq!(base, "https://claude.com/cai/oauth/authorize");
    let keys: Vec<&str> = query
        .split('&')
        .map(|kv| kv.split('=').next().unwrap())
        .collect();
    assert_eq!(
        keys,
        [
            "code",
            "client_id",
            "response_type",
            "redirect_uri",
            "scope",
            "code_challenge",
            "code_challenge_method",
            "state"
        ]
    );
    assert!(query.contains("client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e"));
    assert!(
        query.contains("scope=org%3Acreate_api_key+user%3Aprofile")
            || query.contains("scope=org%3Acreate_api_key%20user%3Aprofile")
    );
}

#[test]
fn pkce_challenge_is_s256_of_the_verifier() {
    use sha2::{Digest, Sha256};
    let p = generate_pkce();
    assert!(p.verifier.len() >= 43 && p.verifier.len() <= 128);
    let expect = base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        Sha256::digest(p.verifier.as_bytes()),
    );
    assert_eq!(p.challenge, expect);
    assert_ne!(generate_state(), generate_state());
    assert!(!format!("{p:?}").contains(&p.verifier));
}

#[test]
fn a_pasted_callback_url_is_validated_against_the_session() {
    let s = PairingSessions::new();
    let first = s
        .start(
            "business",
            "admin-1",
            "http://localhost:4567/callback",
            1_000,
        )
        .unwrap()
        .session;
    let started = s
        .start(
            "business",
            "admin-1",
            "http://localhost:4567/callback",
            1_000,
        )
        .unwrap();
    assert_eq!(
        started.superseded.as_deref(),
        Some(first.id.as_str()),
        "the owner's second start replaces the pairing it left standing"
    );
    assert!(matches!(
        s.get(&first.id, "admin-1", 1_000).unwrap().status,
        PairingStatus::Cancelled
    ));
    let first_link = format!(
        "http://localhost:4567/callback?code=abc&state={}",
        first.state
    );
    assert!(
        matches!(
            s.accept_callback(&first.id, "admin-1", CallbackInput::Url(first_link), 1_001),
            Err(PairingError::AlreadyConsumed)
        ),
        "the replaced pairing's link stops working"
    );
    let sess = started.session;
    let wrong_state = "http://localhost:4567/callback?code=abc&state=nope".to_string();
    assert!(matches!(
        s.accept_callback(&sess.id, "admin-1", CallbackInput::Url(wrong_state), 1_001),
        Err(PairingError::StateMismatch)
    ));
    let wrong_host = format!("http://evil.example/callback?code=abc&state={}", sess.state);
    assert!(matches!(
        s.accept_callback(&sess.id, "admin-1", CallbackInput::Url(wrong_host), 1_001),
        Err(PairingError::RedirectMismatch)
    ));
    assert!(matches!(
        s.accept_callback(
            &sess.id,
            "admin-2",
            CallbackInput::Url(String::new()),
            1_001
        ),
        Err(PairingError::Forbidden)
    ));
    let embedded_mismatch = format!(
        "http://localhost:4567/callback?code=abc%23nope&state={}",
        sess.state
    );
    assert!(matches!(
        s.accept_callback(
            &sess.id,
            "admin-1",
            CallbackInput::Url(embedded_mismatch),
            1_001
        ),
        Err(PairingError::StateMismatch)
    ));
    let ok = format!(
        "http://localhost:4567/callback?code=abc%23{}&state={}",
        sess.state, sess.state
    );
    let after = s
        .accept_callback(&sess.id, "admin-1", CallbackInput::Url(ok.clone()), 1_001)
        .unwrap();
    assert!(
        matches!(after.status, PairingStatus::CodeReceived { ref code } if code == "abc"),
        "the stored code is the part before the '#'"
    );
    let after_debug = format!("{after:?}");
    assert!(!after_debug.contains("abc#xyz"));
    assert!(!after_debug.contains("abc"));
    assert!(matches!(
        s.accept_callback(&sess.id, "admin-1", CallbackInput::Url(ok), 1_002),
        Err(PairingError::AlreadyConsumed)
    ));
}

#[test]
fn sessions_expire_after_ten_minutes() {
    let s = PairingSessions::new();
    let sess = s
        .start(
            "business",
            "admin-1",
            "http://localhost:4567/callback",
            1_000,
        )
        .unwrap()
        .session;
    let url = format!(
        "http://localhost:4567/callback?code=abc&state={}",
        sess.state
    );
    assert!(matches!(
        s.accept_callback(
            &sess.id,
            "admin-1",
            CallbackInput::Url(url),
            1_000 + 601_000
        ),
        Err(PairingError::Expired)
    ));
}

#[test]
fn an_abandoned_session_expires_and_frees_the_account() {
    let s = PairingSessions::new();
    let first = s
        .start(
            "business",
            "admin-1",
            "http://localhost:4567/callback",
            1_000,
        )
        .unwrap()
        .session;
    let later = 1_000 + 601_000;
    assert!(s
        .start(
            "business",
            "admin-1",
            "http://localhost:4567/callback",
            later
        )
        .is_ok());
    let stale = s.get(&first.id, "admin-1", later).unwrap();
    assert!(matches!(stale.status, PairingStatus::Expired));
}

#[test]
fn a_session_walks_from_a_code_through_the_exchange_to_confirmed() {
    let s = PairingSessions::new();
    let redirect = "http://localhost:4567/callback";
    let sess = s
        .start("business", "admin-1", redirect, 1_000)
        .unwrap()
        .session;
    let pending = || PendingIdentity {
        account_uuid: "00000000-0000-4000-8000-000000000001".into(),
        email: "test@example.invalid".into(),
        organization_name: None,
        organization_uuid: None,
    };
    assert!(
        matches!(
            s.confirm(&sess.id, "admin-1", 1_001),
            Err(PairingError::AlreadyConsumed)
        ),
        "a session with no exchange behind it has nothing to confirm"
    );
    let url = format!("{redirect}?code=CODE123&state={}", sess.state);
    s.accept_callback(&sess.id, "admin-1", CallbackInput::Url(url), 1_001)
        .unwrap();
    assert!(
        matches!(
            s.confirm(&sess.id, "admin-1", 1_002),
            Err(PairingError::AlreadyConsumed)
        ),
        "a code alone is not an exchange"
    );

    assert!(matches!(
        s.mark_exchanged(&sess.id, "admin-2", pending(), 1_002),
        Err(PairingError::Forbidden)
    ));
    s.mark_exchanged(&sess.id, "admin-1", pending(), 1_002)
        .unwrap();
    assert!(matches!(
        s.mark_exchanged(&sess.id, "admin-1", pending(), 1_002),
        Err(PairingError::AlreadyConsumed)
    ));
    assert!(matches!(
        s.get(&sess.id, "admin-1", 1_002).unwrap().status,
        PairingStatus::Exchanged { .. }
    ));
    assert!(
        matches!(
            s.start("business", "admin-2", redirect, 1_002),
            Err(PairingError::Busy)
        ),
        "an exchanged session still holds the account's slot against another admin session"
    );

    assert!(matches!(
        s.claim_confirm(&sess.id, "admin-2", 1_003),
        Err(PairingError::Forbidden)
    ));
    assert!(matches!(
        s.claim_confirm("not-a-session", "admin-1", 1_003),
        Err(PairingError::NotFound)
    ));
    assert!(
        matches!(
            s.confirm(&sess.id, "admin-1", 1_003),
            Err(PairingError::AlreadyConsumed)
        ),
        "a confirmation that never claimed the session has nothing to close"
    );
    s.claim_confirm(&sess.id, "admin-1", 1_003).unwrap();
    s.confirm(&sess.id, "admin-1", 1_003).unwrap();
    assert!(matches!(
        s.get(&sess.id, "admin-1", 1_003).unwrap().status,
        PairingStatus::Confirmed
    ));
    assert!(matches!(
        s.confirm(&sess.id, "admin-1", 1_003),
        Err(PairingError::AlreadyConsumed)
    ));
    assert!(
        s.start("business", "admin-1", redirect, 1_003).is_ok(),
        "a confirmed session hands the account's slot back"
    );
}

#[test]
fn an_exchange_that_sat_past_the_deadline_cannot_be_confirmed() {
    let s = PairingSessions::new();
    let redirect = "http://localhost:4567/callback";
    let sess = s
        .start("business", "admin-1", redirect, 1_000)
        .unwrap()
        .session;
    let url = format!("{redirect}?code=CODE123&state={}", sess.state);
    s.accept_callback(&sess.id, "admin-1", CallbackInput::Url(url), 1_001)
        .unwrap();
    s.mark_exchanged(
        &sess.id,
        "admin-1",
        PendingIdentity {
            account_uuid: "00000000-0000-4000-8000-000000000001".into(),
            email: "test@example.invalid".into(),
            organization_name: None,
            organization_uuid: None,
        },
        1_002,
    )
    .unwrap();
    assert!(matches!(
        s.claim_confirm(&sess.id, "admin-1", sess.deadline + 1),
        Err(PairingError::Expired)
    ));
}

/// An exchanged session, one step short of the confirmation that installs its credential.
fn exchanged(s: &PairingSessions, redirect: &str) -> String {
    let sess = s
        .start("business", "admin-1", redirect, 1_000)
        .unwrap()
        .session;
    let url = format!("{redirect}?code=CODE123&state={}", sess.state);
    s.accept_callback(&sess.id, "admin-1", CallbackInput::Url(url), 1_001)
        .unwrap();
    s.mark_exchanged(
        &sess.id,
        "admin-1",
        PendingIdentity {
            account_uuid: "00000000-0000-4000-8000-000000000001".into(),
            email: "test@example.invalid".into(),
            organization_name: None,
            organization_uuid: None,
        },
        1_002,
    )
    .unwrap();
    sess.id
}

#[test]
fn a_claimed_session_cannot_be_cancelled_out_from_under_the_commit() {
    let s = PairingSessions::new();
    let redirect = "http://localhost:4567/callback";
    let id = exchanged(&s, redirect);
    s.claim_confirm(&id, "admin-1", 1_003).unwrap();
    assert!(matches!(
        s.get(&id, "admin-1", 1_003).unwrap().status,
        PairingStatus::Confirming { .. }
    ));
    assert!(
        matches!(
            s.cancel(&id, "admin-1", 1_003),
            Err(PairingError::AlreadyConsumed)
        ),
        "the credential may already be installed by the time this lands"
    );
    assert!(
        matches!(
            s.claim_confirm(&id, "admin-1", 1_003),
            Err(PairingError::AlreadyConsumed)
        ),
        "one confirmation at a time, like everything else about a session"
    );
    assert!(
        matches!(
            s.start("business", "admin-1", redirect, 1_003),
            Err(PairingError::Busy)
        ),
        "a claimed session still holds the account's slot"
    );
    s.confirm(&id, "admin-1", 1_004).unwrap();
    assert!(matches!(
        s.get(&id, "admin-1", 1_004).unwrap().status,
        PairingStatus::Confirmed
    ));
}

#[test]
fn a_start_by_the_owner_supersedes_its_exchanged_pairing() {
    let s = PairingSessions::new();
    let redirect = "http://localhost:4567/callback";
    let id = exchanged(&s, redirect);
    let started = s.start("business", "admin-1", redirect, 1_003).unwrap();
    assert_eq!(started.superseded.as_deref(), Some(id.as_str()));
    assert!(matches!(started.session.status, PairingStatus::Pending));
    assert!(matches!(
        s.get(&id, "admin-1", 1_003).unwrap().status,
        PairingStatus::Cancelled
    ));
    assert!(
        matches!(
            s.claim_confirm(&id, "admin-1", 1_003),
            Err(PairingError::AlreadyConsumed)
        ),
        "a replaced pairing has nothing left to confirm"
    );
}

#[test]
fn another_admin_sessions_start_is_refused_while_a_pairing_stands() {
    let s = PairingSessions::new();
    let redirect = "http://localhost:4567/callback";
    let first = s
        .start("business", "admin-1", redirect, 1_000)
        .unwrap()
        .session;
    assert!(matches!(
        s.start("business", "admin-2", redirect, 1_001),
        Err(PairingError::Busy)
    ));
    assert!(
        matches!(
            s.get(&first.id, "admin-1", 1_001).unwrap().status,
            PairingStatus::Pending
        ),
        "the refused start left the owner's pairing alone"
    );
}

#[test]
fn a_commit_that_failed_ends_the_session_and_frees_the_account() {
    let s = PairingSessions::new();
    let redirect = "http://localhost:4567/callback";
    let id = exchanged(&s, redirect);
    s.claim_confirm(&id, "admin-1", 1_003).unwrap();
    s.fail(&id, "admin-1", "storage failure", 1_003).unwrap();
    let status = s.get(&id, "admin-1", 1_003).unwrap().status;
    assert!(
        matches!(&status, PairingStatus::Failed(reason) if reason == "storage failure"),
        "the operator is told why, in the same words the response carried"
    );
    assert!(
        s.start("business", "admin-1", redirect, 1_003).is_ok(),
        "a failed session hands the account's slot back"
    );
    assert!(matches!(
        s.confirm(&id, "admin-1", 1_004),
        Err(PairingError::AlreadyConsumed)
    ));
}
