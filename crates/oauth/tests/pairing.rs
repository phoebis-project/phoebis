use phoebis_oauth::{
    authorization_url, generate_pkce, generate_state, CallbackInput, LoginKind, PairingError,
    PairingSessions, PairingStatus,
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
    let sess = s
        .start(
            "business",
            "admin-1",
            "http://localhost:4567/callback",
            1_000,
        )
        .unwrap();
    assert!(matches!(
        s.start(
            "business",
            "admin-1",
            "http://localhost:4567/callback",
            1_000
        ),
        Err(PairingError::Busy)
    ));
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
        .unwrap();
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
        .unwrap();
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
