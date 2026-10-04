mod common;

use fake_upstream::{FakeUpstream, Respond, Rule};
use phoebis_oauth::{
    control_request, exchange_body, parse_token_response, refresh_body, AcquireError, ControlKind,
    CredentialManager,
};
use phoebis_profile::{
    claude_code_2_1_269_linux_x64,
    fixtures::{control_head, control_response, ControlFixture},
};
use phoebis_store::{AccountKind, AccountRow, AccountState, NewBundle, Secret, StorageKey, Store};
use phoebis_transport::{
    connector_builder,
    control::{ControlClient, ResolveFuture, Resolver},
    request::serialize_head,
    Plane,
};
use std::sync::{
    atomic::{AtomicI64, Ordering},
    Arc,
};

fn bodies_match_measured_key_order() {
    let p = claude_code_2_1_269_linux_x64();
    let b = refresh_body(p, "sk-ant-ort01-PLACEHOLDER", "user:profile user:inference");
    assert_eq!(
        String::from_utf8(b).unwrap(),
        r#"{"grant_type":"refresh_token","refresh_token":"sk-ant-ort01-PLACEHOLDER","client_id":"9d1c250a-e61b-44d9-88ed-5944d1962f5e","scope":"user:profile user:inference"}"#
    );
    let e = exchange_body(
        p,
        "CODE",
        "http://localhost:1/callback",
        "VERIFIER",
        "STATE",
    );
    let keys: Vec<String> =
        serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&e)
            .unwrap()
            .keys()
            .cloned()
            .collect();
    assert_eq!(
        keys,
        [
            "grant_type",
            "code",
            "redirect_uri",
            "client_id",
            "code_verifier",
            "state"
        ]
    );
}

#[test]
fn control_requests_serialize_to_the_fixture_heads() {
    bodies_match_measured_key_order();
    let p = claude_code_2_1_269_linux_x64();
    let fixture = control_head(ControlFixture::Roles);
    let req = control_request(
        p,
        ControlKind::Roles,
        Some("sk-ant-oat01-PLACEHOLDER"),
        None,
    );
    assert_eq!(serialize_head(&req), fixture);
    let usage = control_request(
        p,
        ControlKind::Usage,
        Some("sk-ant-oat01-PLACEHOLDER"),
        None,
    );
    assert_eq!(serialize_head(&usage), control_head(ControlFixture::Usage));
    let raw = control_head(ControlFixture::Token);
    let terminator = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let token = control_request(
        p,
        ControlKind::Token,
        None,
        Some(raw[terminator..].to_vec()),
    );
    assert_eq!(serialize_head(&token), &raw[..terminator]);
    let profile = control_request(
        p,
        ControlKind::Profile,
        Some("sk-ant-oat01-PLACEHOLDER"),
        None,
    );
    assert_eq!(
        serialize_head(&profile),
        control_head(ControlFixture::Profile)
    );
    let t = parse_token_response(control_response(ControlFixture::Token)).unwrap();
    assert_eq!(t.expires_in, 28800);
    assert_eq!(
        t.account.unwrap().uuid,
        "00000000-0000-4000-8000-000000000001"
    );
}

fn store_with_account() -> Arc<Store> {
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    store
        .insert_account(&AccountRow {
            id: "a".into(),
            kind: AccountKind::Oauth,
            label: "a".into(),
            enabled: true,
            identity: Some(phoebis_store::Identity {
                account_uuid: "00000000-0000-4000-8000-000000000001".into(),
                email: "test@example.invalid".into(),
                organization_uuid: None,
            }),
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state: AccountState::Ready,
        })
        .unwrap();
    store
}

async fn manager_with(
    rules: Vec<Rule>,
    now: Arc<AtomicI64>,
) -> (CredentialManager, Arc<Store>, FakeUpstream) {
    let up = FakeUpstream::start(rules).await;
    let p = claude_code_2_1_269_linux_x64();
    let mut b = connector_builder(p, Plane::Control).unwrap();
    phoebis_transport::trust_extra_ca(&mut b, up.ca_pem()).unwrap();
    let addr = up.addr();
    let resolver: Resolver =
        Arc::new(move |_| Box::pin(async move { Some(addr) }) as ResolveFuture);
    let control = Arc::new(ControlClient::new(p, b.build(), resolver));
    let store = store_with_account();
    let n = now.clone();
    let clock: phoebis_oauth::Clock = Arc::new(move || n.load(Ordering::SeqCst));
    (
        CredentialManager::new(store.clone(), control, p, clock),
        store,
        up,
    )
}

#[tokio::test]
async fn concurrent_acquires_coalesce_into_one_refresh() {
    let now = Arc::new(AtomicI64::new(1_000_000));
    let token_rule = Rule {
        path_prefix: "/v1/oauth/token".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: control_response(ControlFixture::Token).to_vec(),
        },
    };
    let (mgr, store, up) = manager_with(vec![token_rule], now.clone()).await;
    // Expired token: expires_at in the past forces a refresh on first acquire.
    store
        .commit_credentials(
            "a",
            1,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new("old"),
                refresh_token: Some(Secret::new("sk-ant-ort01-PLACEHOLDER")),
                issued_at: 0,
                expires_at: 1,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    let mgr = Arc::new(mgr);
    let results = futures::future::join_all((0..8).map(|_| {
        let m = mgr.clone();
        async move { m.acquire("a").await }
    }))
    .await;
    for r in results {
        assert_eq!(r.unwrap().secret.expose(), "sk-ant-oat01-PLACEHOLDER");
    }
    assert_eq!(
        up.requests()
            .iter()
            .filter(|r| r.line.starts_with("POST /v1/oauth/token"))
            .count(),
        1,
        "one refresh for eight callers"
    );
    let stored = store.credentials("a").unwrap().unwrap();
    assert_eq!(stored.issued_at, 1_000_000);
    assert_eq!(stored.expires_at, 1_000_000 + 28_800_000);
    assert!(store.pending_refresh_attempts().unwrap().is_empty());
    up.shutdown().await;
}

/// An expired bundle at generation 1 under `a`, and a manager whose token endpoint answers the fixture, so the first
/// acquisition refreshes it to generation 2.
async fn refreshed_once() -> (CredentialManager, Arc<Store>, FakeUpstream, AccountRow) {
    let now = Arc::new(AtomicI64::new(1_000_000));
    let token_rule = Rule {
        path_prefix: "/v1/oauth/token".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: control_response(ControlFixture::Token).to_vec(),
        },
    };
    let (mgr, store, up) = manager_with(vec![token_rule], now).await;
    store
        .commit_credentials(
            "a",
            1,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new("old"),
                refresh_token: Some(Secret::new("sk-ant-ort01-PLACEHOLDER")),
                issued_at: 0,
                expires_at: 1,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    let row = store.account("a").unwrap().unwrap();
    let token = mgr.acquire_for(&row, Some(1)).await.unwrap();
    assert_eq!(
        (token.secret.expose(), token.generation),
        ("sk-ant-oat01-PLACEHOLDER", 2)
    );
    (mgr, store, up, row)
}

#[tokio::test]
async fn a_refresh_commit_replaces_the_cached_bundle_so_the_next_acquisition_reads_no_store() {
    let (mgr, store, up, row) = refreshed_once().await;
    let before = store.debug_lock_count();
    let token = mgr.acquire_for(&row, Some(2)).await.unwrap();
    assert_eq!(
        (token.secret.expose(), token.generation),
        ("sk-ant-oat01-PLACEHOLDER", 2)
    );
    assert_eq!(store.debug_lock_count(), before, "no store read");
    up.shutdown().await;
}

#[tokio::test]
async fn a_refreshed_bundle_serves_an_older_expectation_without_a_store_read() {
    let (mgr, store, up, row) = refreshed_once().await;
    let before = store.debug_lock_count();
    // The router's snapshot still names the generation it loaded before the refresh committed.
    let token = mgr.acquire_for(&row, Some(1)).await.unwrap();
    assert_eq!(
        (token.secret.expose(), token.generation),
        ("sk-ant-oat01-PLACEHOLDER", 2)
    );
    assert_eq!(store.debug_lock_count(), before, "no store read");
    up.shutdown().await;
}

#[tokio::test]
async fn a_reconnected_slot_answers_its_new_bundle_under_the_restarted_generation() {
    let now = Arc::new(AtomicI64::new(1_000_000));
    let (mgr, store, up) = manager_with(vec![], now).await;
    let bundle = |token: &str| NewBundle {
        access_token: Secret::new(token),
        refresh_token: Some(Secret::new("rt")),
        issued_at: 1_000_000,
        expires_at: 1_000_000 + 3_600_000,
        refresh_expires_at: None,
        scopes: "user:profile user:inference".into(),
    };
    store
        .commit_credentials("a", 1, 1, None, None, &bundle("before"))
        .unwrap();
    let row = store.account("a").unwrap().unwrap();
    let token = mgr.acquire_for(&row, Some(1)).await.unwrap();
    assert_eq!((token.secret.expose(), token.generation), ("before", 1));
    let lifecycle = store.bump_lifecycle("a").unwrap();
    store.set_state("a", AccountState::Ready).unwrap();
    store
        .commit_credentials("a", 1, lifecycle, None, None, &bundle("after"))
        .unwrap();
    let row = store.account("a").unwrap().unwrap();
    let token = mgr.acquire_for(&row, Some(1)).await.unwrap();
    assert_eq!(
        (token.secret.expose(), token.generation),
        ("after", 1),
        "the generation restarted under the new lifecycle, so the old bundle's figure matches it"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn invalid_grant_marks_reauth_required_and_stops_retrying() {
    let now = Arc::new(AtomicI64::new(1_000_000));
    let rule = Rule {
        path_prefix: "/v1/oauth/token".into(),
        respond: Respond::Json {
            status: 400,
            headers: vec![],
            body: br#"{"error":"invalid_grant","error_description":"revoked"}"#.to_vec(),
        },
    };
    let (mgr, store, up) = manager_with(vec![rule], now).await;
    store
        .commit_credentials(
            "a",
            1,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new("old"),
                refresh_token: Some(Secret::new("rt")),
                issued_at: 0,
                expires_at: 1,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    assert!(matches!(
        mgr.acquire("a").await,
        Err(AcquireError::ReauthRequired)
    ));
    assert!(matches!(
        mgr.acquire("a").await,
        Err(AcquireError::ReauthRequired)
    ));
    assert_eq!(
        up.requests().len(),
        1,
        "no retry after an explicit invalid_grant"
    );
    assert!(matches!(
        store.account("a").unwrap().unwrap().state,
        AccountState::ReauthRequired
    ));
    up.shutdown().await;
}

#[tokio::test]
async fn a_repeated_backoff_failure_leaves_the_credentials_last_error_row_untouched() {
    let now = Arc::new(AtomicI64::new(1_000_000));
    let rule = Rule {
        path_prefix: "/v1/oauth/token".into(),
        respond: Respond::Json {
            status: 503,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    };
    let (mgr, store, up) = manager_with(vec![rule], now.clone()).await;
    store
        .commit_credentials(
            "a",
            1,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new("old"),
                refresh_token: Some(Secret::new("sk-ant-ort01-PLACEHOLDER")),
                issued_at: 0,
                expires_at: 1,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();

    assert!(mgr.acquire("a").await.is_err());
    let after_first = store.credential_metadata("a").unwrap().unwrap();
    assert_eq!(
        after_first.last_error.as_deref(),
        Some("refresh rejected with status 503")
    );

    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    // The 503 armed a backoff the manager's own clock never moves past, so every later acquire fails
    // on the backoff itself rather than reaching the token endpoint again.
    assert!(mgr.acquire("a").await.is_err());
    let after_second = store.credential_metadata("a").unwrap().unwrap();
    assert_eq!(
        after_second.last_error.as_deref(),
        Some("refresh is waiting out a backoff")
    );
    assert_ne!(
        after_second.last_error_at, after_first.last_error_at,
        "a message that changed from the row's still writes"
    );

    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    assert!(mgr.acquire("a").await.is_err());
    let after_third = store.credential_metadata("a").unwrap().unwrap();
    assert_eq!(
        after_third.last_error_at, after_second.last_error_at,
        "the same message a second time leaves the row untouched"
    );

    up.shutdown().await;
}

fn token_answer() -> Respond {
    Respond::Json {
        status: 200,
        headers: vec![],
        body: control_response(ControlFixture::Token).to_vec(),
    }
}

/// Issued at `800_000`, early enough that a refresh at `1_120_000` runs inside the margin of either expiry the
/// floor tests use.
fn old_bundle(expires_at: i64) -> NewBundle {
    NewBundle {
        access_token: Secret::new("old"),
        refresh_token: Some(Secret::new("rt")),
        issued_at: 800_000,
        expires_at,
        refresh_expires_at: None,
        scopes: "user:profile user:inference".into(),
    }
}

fn token_posts(up: &FakeUpstream) -> usize {
    up.requests()
        .iter()
        .filter(|r| r.line.starts_with("POST /v1/oauth/token"))
        .count()
}

#[tokio::test]
async fn a_lost_response_keeps_the_old_token_through_the_floor_and_retries_once_past_it() {
    let now = Arc::new(AtomicI64::new(1_000_000));
    let rule = Rule {
        path_prefix: "/v1/oauth/token".into(),
        respond: Respond::CloseAfterRequest,
    };
    let (mgr, store, up) = manager_with(vec![rule], now.clone()).await;
    // 80 s left of a 400 s lifetime: inside the 100 s margin but still valid, so the refresh runs while the token is usable.
    now.store(1_120_000, Ordering::SeqCst);
    store
        .commit_credentials("a", 1, 1, None, None, &old_bundle(1_200_000))
        .unwrap();
    assert_eq!(
        mgr.acquire("a").await.unwrap().secret.expose(),
        "old",
        "an uncertain rotation does not cost a usable token"
    );
    assert_eq!(store.pending_refresh_attempts().unwrap().len(), 1);
    let attempt = store.last_refresh_attempt("a", 1).unwrap().unwrap();
    assert_eq!(attempt.outcome.as_deref(), Some("uncertain"));
    assert!(
        attempt.detail.is_some_and(|detail| !detail.is_empty()),
        "an uncertain attempt says what went uncertain, or the operator has nothing to act on"
    );
    now.store(1_150_000, Ordering::SeqCst);
    assert_eq!(mgr.acquire("a").await.unwrap().secret.expose(), "old");
    assert_eq!(up.requests().len(), 1, "the floor spaces the retry");
    up.set_rule_after(1, "/v1/oauth/token", token_answer());
    now.store(1_185_000, Ordering::SeqCst);
    assert_eq!(
        mgr.acquire("a").await.unwrap().secret.expose(),
        "sk-ant-oat01-PLACEHOLDER",
        "past the floor one retry runs with the stored refresh token"
    );
    assert_eq!(up.requests().len(), 2);
    assert!(
        store.pending_refresh_attempts().unwrap().is_empty(),
        "the commit settled the uncertain attempt"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_second_lost_response_leaves_the_attempt_pending_and_a_restart_quarantines_it() {
    let now = Arc::new(AtomicI64::new(1_120_000));
    let rule = Rule {
        path_prefix: "/v1/oauth/token".into(),
        respond: Respond::CloseAfterRequest,
    };
    let (mgr, store, up) = manager_with(vec![rule], now.clone()).await;
    store
        .commit_credentials("a", 1, 1, None, None, &old_bundle(1_200_000))
        .unwrap();
    assert_eq!(mgr.acquire("a").await.unwrap().secret.expose(), "old");
    now.store(1_185_000, Ordering::SeqCst);
    assert_eq!(mgr.acquire("a").await.unwrap().secret.expose(), "old");
    assert_eq!(up.requests().len(), 2, "the retry past the floor ran");
    assert_eq!(
        store.pending_refresh_attempts().unwrap().len(),
        2,
        "the retry's own uncertain outcome sits beside the first"
    );
    // Past the token's expiry and inside the floor the second lost answer armed.
    now.store(1_220_000, Ordering::SeqCst);
    assert!(matches!(
        mgr.acquire("a").await,
        Err(AcquireError::Quarantined)
    ));
    assert_eq!(up.requests().len(), 2);
    assert_eq!(mgr.recover_on_start().unwrap(), vec!["a".to_string()]);
    assert!(matches!(
        mgr.acquire("a").await,
        Err(AcquireError::ReauthRequired)
    ));
    up.shutdown().await;
}

#[tokio::test]
async fn an_unreadable_200_quarantines_inside_the_floor_and_retries_once_past_it() {
    let now = Arc::new(AtomicI64::new(1_120_000));
    // `access_token` and `expires_in` are required, so this 200 cannot be read.
    let rule = Rule {
        path_prefix: "/v1/oauth/token".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    };
    let (mgr, store, up) = manager_with(vec![rule], now.clone()).await;
    store
        .commit_credentials("a", 1, 1, None, None, &old_bundle(1_150_000))
        .unwrap();
    assert_eq!(mgr.acquire("a").await.unwrap().secret.expose(), "old");
    let attempt = store.last_refresh_attempt("a", 1).unwrap().unwrap();
    assert_eq!(
        (attempt.outcome.as_deref(), attempt.detail.as_deref()),
        (
            Some("uncertain"),
            Some("refresh response could not be read")
        )
    );
    now.store(1_160_000, Ordering::SeqCst);
    assert!(matches!(
        mgr.acquire("a").await,
        Err(AcquireError::Quarantined)
    ));
    assert_eq!(up.requests().len(), 1, "the floor spaces the retry");
    up.set_rule_after(1, "/v1/oauth/token", token_answer());
    now.store(1_185_000, Ordering::SeqCst);
    assert_eq!(
        mgr.acquire("a").await.unwrap().secret.expose(),
        "sk-ant-oat01-PLACEHOLDER"
    );
    assert_eq!(up.requests().len(), 2);
    assert!(store.pending_refresh_attempts().unwrap().is_empty());
    up.shutdown().await;
}

#[tokio::test]
async fn a_failed_credential_commit_quarantines_inside_the_floor_and_retries_once_past_it() {
    let now = Arc::new(AtomicI64::new(1_120_000));
    let rule = Rule {
        path_prefix: "/v1/oauth/token".into(),
        respond: token_answer(),
    };
    let (mgr, store, up) = manager_with(vec![rule], now.clone()).await;
    store
        .commit_credentials("a", 1, 1, None, None, &old_bundle(1_150_000))
        .unwrap();
    store.debug_fail_next_credential_commit();
    assert_eq!(mgr.acquire("a").await.unwrap().secret.expose(), "old");
    let attempt = store.last_refresh_attempt("a", 1).unwrap().unwrap();
    assert_eq!(attempt.outcome.as_deref(), Some("uncertain"));
    assert!(attempt
        .detail
        .as_deref()
        .is_some_and(|detail| detail.starts_with("commit failed after a rotated response")));
    now.store(1_160_000, Ordering::SeqCst);
    assert!(matches!(
        mgr.acquire("a").await,
        Err(AcquireError::Quarantined)
    ));
    assert_eq!(token_posts(&up), 1, "the floor spaces the retry");
    now.store(1_185_000, Ordering::SeqCst);
    assert_eq!(
        mgr.acquire("a").await.unwrap().secret.expose(),
        "sk-ant-oat01-PLACEHOLDER"
    );
    assert_eq!(token_posts(&up), 2);
    assert!(store.pending_refresh_attempts().unwrap().is_empty());
    let settled = store.refresh_attempts_resolved_after(0, 0, 10).unwrap();
    assert_eq!(
        settled
            .iter()
            .map(|a| (a.outcome.as_str(), a.detail.as_deref()))
            .collect::<Vec<_>>(),
        [("failed", Some("superseded")), ("committed", None)],
        "the retry's commit settled the uncertain attempt"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_refreshed_bundle_with_a_foreign_account_uuid_is_rejected() {
    let now = Arc::new(AtomicI64::new(1_000_000));
    let foreign = String::from_utf8(control_response(ControlFixture::Token).to_vec())
        .unwrap()
        .replace(
            "00000000-0000-4000-8000-000000000001",
            "00000000-0000-4000-8000-0000000000ff",
        );
    let rule = Rule {
        path_prefix: "/v1/oauth/token".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: foreign.into_bytes(),
        },
    };
    let (mgr, store, up) = manager_with(vec![rule], now).await;
    store
        .commit_credentials(
            "a",
            1,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new("old"),
                refresh_token: Some(Secret::new("rt")),
                issued_at: 0,
                expires_at: 1,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    assert!(matches!(
        mgr.acquire("a").await,
        Err(AcquireError::ReauthRequired)
    ));
    assert_eq!(
        store
            .credentials("a")
            .unwrap()
            .unwrap()
            .access_token
            .expose(),
        "old",
        "the foreign bundle is never committed"
    );
    assert!(matches!(
        store.account("a").unwrap().unwrap().state,
        AccountState::ReauthRequired
    ));
    up.shutdown().await;
}

#[tokio::test]
async fn a_valid_token_is_returned_without_any_request() {
    let now = Arc::new(AtomicI64::new(1_000_000));
    let (mgr, store, up) = manager_with(vec![], now).await;
    store
        .commit_credentials(
            "a",
            1,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new("fresh"),
                refresh_token: Some(Secret::new("rt")),
                issued_at: 1_000_000,
                expires_at: 1_000_000 + 3_600_000,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    assert_eq!(mgr.acquire("a").await.unwrap().secret.expose(), "fresh");
    assert!(up.requests().is_empty());
    up.shutdown().await;
}

#[tokio::test]
async fn a_short_lived_token_is_not_permanently_inside_its_own_margin() {
    let now = Arc::new(AtomicI64::new(1_000_000));
    let (mgr, store, up) = manager_with(vec![], now).await;
    // 400 s of lifetime with 200 s left: a flat five-minute margin would refresh this on every single call.
    store
        .commit_credentials(
            "a",
            1,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new("short"),
                refresh_token: Some(Secret::new("rt")),
                issued_at: 800_000,
                expires_at: 1_200_000,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    assert_eq!(mgr.acquire("a").await.unwrap().secret.expose(), "short");
    assert!(
        up.requests().is_empty(),
        "the margin is (expires_at - issued_at) / 4, so 100 s here"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn refresh_now_serves_the_stored_token_when_the_generation_already_moved() {
    let now = Arc::new(AtomicI64::new(1_000_000));
    let (mgr, store, up) = manager_with(vec![], now).await;
    let stale = store
        .commit_credentials(
            "a",
            1,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new("rejected"),
                refresh_token: Some(Secret::new("rt")),
                issued_at: 1_000_000,
                expires_at: 1_000_000 + 3_600_000,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    store
        .commit_credentials(
            "a",
            1,
            1,
            Some(stale),
            None,
            &NewBundle {
                access_token: Secret::new("fresh"),
                refresh_token: Some(Secret::new("rt")),
                issued_at: 1_000_000,
                expires_at: 1_000_000 + 3_600_000,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    // The 401 that triggered this carried the rejected generation, which another caller has already replaced.
    assert_eq!(
        mgr.refresh_now("a", stale).await.unwrap().secret.expose(),
        "fresh"
    );
    assert!(up.requests().is_empty());
    up.shutdown().await;
}

/// A manager whose resolver never yields an address, so every refresh fails before a byte is written.
async fn manager_without_upstream(now: Arc<AtomicI64>) -> (CredentialManager, Arc<Store>) {
    let p = claude_code_2_1_269_linux_x64();
    let resolver: Resolver = Arc::new(|_| Box::pin(async { None }) as ResolveFuture);
    let control = Arc::new(ControlClient::new(
        p,
        connector_builder(p, Plane::Control).unwrap().build(),
        resolver,
    ));
    let store = store_with_account();
    let n = now.clone();
    let clock: phoebis_oauth::Clock = Arc::new(move || n.load(Ordering::SeqCst));
    (
        CredentialManager::new(store.clone(), control, p, clock),
        store,
    )
}

#[tokio::test]
async fn an_unreachable_upstream_does_not_quarantine() {
    let now = Arc::new(AtomicI64::new(1_000_000));
    let (mgr, store) = manager_without_upstream(now.clone()).await;
    store
        .commit_credentials(
            "a",
            1,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new("old"),
                refresh_token: Some(Secret::new("rt")),
                issued_at: 0,
                expires_at: 1,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    assert!(matches!(
        mgr.acquire("a").await,
        Err(AcquireError::Unavailable(_))
    ));
    assert!(
        store.pending_refresh_attempts().unwrap().is_empty(),
        "a request that never left resolves its attempt instead of quarantining the account"
    );

    // Past the backoff the first failure armed, with 100 ms left of a 210 s lifetime, so the refresh runs
    // again and still cannot reach anything. The bundle is committed from outside the manager, which drops the
    // cached one as a pairing confirm does, or the call would answer the newer generation without a refresh.
    now.store(1_010_000, Ordering::SeqCst);
    store
        .commit_credentials(
            "a",
            1,
            1,
            Some(1),
            None,
            &NewBundle {
                access_token: Secret::new("still-good"),
                refresh_token: Some(Secret::new("rt")),
                issued_at: 800_000,
                expires_at: 1_010_100,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    mgr.invalidate("a");
    assert_eq!(
        mgr.acquire("a").await.unwrap().secret.expose(),
        "still-good"
    );
    assert!(store.pending_refresh_attempts().unwrap().is_empty());
}

#[tokio::test]
async fn a_panicking_refresh_does_not_poison_the_account() {
    let now = Arc::new(AtomicI64::new(1_000_000));
    let token_rule = Rule {
        path_prefix: "/v1/oauth/token".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: control_response(ControlFixture::Token).to_vec(),
        },
    };
    let (mgr, store, up) = manager_with(vec![token_rule], now.clone()).await;
    store
        .commit_credentials(
            "a",
            1,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new("old"),
                refresh_token: Some(Secret::new("sk-ant-ort01-PLACEHOLDER")),
                issued_at: 0,
                expires_at: 1,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    mgr.debug_panic_on_next_refresh();
    assert!(mgr.acquire("a").await.is_err());
    assert!(
        up.requests().is_empty(),
        "the panicking refresh never reached the upstream"
    );
    // The in-flight entry is gone, so this starts a refresh of its own instead of joining the dead future.
    assert_eq!(
        mgr.acquire("a").await.unwrap().secret.expose(),
        "sk-ant-oat01-PLACEHOLDER"
    );
    assert_eq!(up.requests().len(), 1);
    up.shutdown().await;
}

#[tokio::test]
async fn a_server_error_from_the_token_endpoint_backs_off_a_minute() {
    let now = Arc::new(AtomicI64::new(1_000_000));
    let rule = Rule {
        path_prefix: "/v1/oauth/token".into(),
        respond: Respond::Json {
            status: 502,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    };
    let (mgr, store, up) = manager_with(vec![rule], now.clone()).await;
    // 80 s left of a 400 s lifetime: inside the 100 s margin, so every acquire below drives the refresh path
    // while the stored token still works.
    now.store(1_120_000, Ordering::SeqCst);
    store
        .commit_credentials(
            "a",
            1,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new("old"),
                refresh_token: Some(Secret::new("rt")),
                issued_at: 800_000,
                expires_at: 1_200_000,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    assert_eq!(mgr.acquire("a").await.unwrap().secret.expose(), "old");
    assert_eq!(up.requests().len(), 1);
    assert!(
        store.pending_refresh_attempts().unwrap().is_empty(),
        "a 5xx fails its attempt rather than quarantining the account"
    );

    now.store(1_120_000 + 59_000, Ordering::SeqCst);
    assert_eq!(mgr.acquire("a").await.unwrap().secret.expose(), "old");
    assert_eq!(
        up.requests().len(),
        1,
        "a 5xx waits out a minute, not the five seconds a 4xx gets"
    );

    now.store(1_120_000 + 60_000, Ordering::SeqCst);
    assert_eq!(mgr.acquire("a").await.unwrap().secret.expose(), "old");
    assert_eq!(up.requests().len(), 2, "the backoff ends after the minute");
    up.shutdown().await;
}

#[tokio::test]
async fn a_refresh_still_running_at_shutdown_is_drained_and_commits_with_no_pending_row() {
    let now = Arc::new(AtomicI64::new(1_000_000));
    let rule = Rule {
        path_prefix: "/v1/oauth/token".into(),
        respond: Respond::Gated(Box::new(Respond::Json {
            status: 200,
            headers: vec![],
            body: control_response(ControlFixture::Token).to_vec(),
        })),
    };
    let (mgr, store, up) = manager_with(vec![rule], now.clone()).await;
    store
        .commit_credentials(
            "a",
            1,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new("old"),
                refresh_token: Some(Secret::new("sk-ant-ort01-PLACEHOLDER")),
                issued_at: 0,
                expires_at: 1,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    let mgr = Arc::new(mgr);
    // The caller goes away with the token request in flight, as a usage poll or a catalog retry does when the
    // process winds down; the refresh itself runs on.
    let caller = tokio::spawn({
        let mgr = mgr.clone();
        async move { mgr.acquire("a").await }
    });
    fake_upstream::wait_for(std::time::Duration::from_secs(5), || {
        (!up.requests().is_empty()).then_some(())
    })
    .await;
    caller.abort();
    tokio::join!(mgr.drain(), async { up.release() });
    assert_eq!(
        store
            .credentials("a")
            .unwrap()
            .unwrap()
            .access_token
            .expose(),
        "sk-ant-oat01-PLACEHOLDER",
        "the rotated token is stored before the drain returns"
    );
    assert!(
        store.pending_refresh_attempts().unwrap().is_empty(),
        "the attempt resolved, so the next start finds nothing to quarantine"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_refresh_requested_after_the_drain_began_starts_nothing_and_is_refused() {
    let now = Arc::new(AtomicI64::new(1_000_000));
    let rule = Rule {
        path_prefix: "/v1/oauth/token".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: control_response(ControlFixture::Token).to_vec(),
        },
    };
    let (mgr, store, up) = manager_with(vec![rule], now.clone()).await;
    store
        .commit_credentials(
            "a",
            1,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new("old"),
                refresh_token: Some(Secret::new("sk-ant-ort01-PLACEHOLDER")),
                issued_at: 0,
                expires_at: 1,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    mgr.drain().await;
    assert!(matches!(
        mgr.acquire("a").await,
        Err(AcquireError::ShuttingDown)
    ));
    assert!(up.requests().is_empty(), "no token request went out");
    assert!(store.pending_refresh_attempts().unwrap().is_empty());
    assert!(
        store
            .credential_metadata("a")
            .unwrap()
            .unwrap()
            .last_error
            .is_none(),
        "a shutdown says nothing about the account, so nothing outlives the process"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_late_invalid_grant_leaves_the_re_paired_slot_alone() {
    let now = Arc::new(AtomicI64::new(1_000_000));
    let rule = Rule {
        path_prefix: "/v1/oauth/token".into(),
        respond: Respond::Gated(Box::new(Respond::Json {
            status: 400,
            headers: vec![],
            body: br#"{"error":"invalid_grant","error_description":"revoked"}"#.to_vec(),
        })),
    };
    let (mgr, store, up) = manager_with(vec![rule], now.clone()).await;
    store
        .commit_credentials(
            "a",
            1,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new("old"),
                refresh_token: Some(Secret::new("rt")),
                issued_at: 0,
                expires_at: 1,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    let mgr = Arc::new(mgr);
    let refreshing = tokio::spawn({
        let mgr = mgr.clone();
        async move { mgr.acquire("a").await }
    });
    while up.requests().is_empty() {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }

    // The operator disconnects and pairs the slot again while the token request is still in flight.
    let lifecycle = store.bump_lifecycle("a").unwrap();
    store.set_state("a", AccountState::Ready).unwrap();
    store
        .commit_credentials(
            "a",
            1,
            lifecycle,
            None,
            None,
            &NewBundle {
                access_token: Secret::new("fresh"),
                refresh_token: Some(Secret::new("rt2")),
                issued_at: 1_000_000,
                expires_at: 1_000_000 + 3_600_000,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    up.release();
    assert!(matches!(
        refreshing.await.unwrap(),
        Err(AcquireError::ReauthRequired)
    ));

    assert!(
        matches!(
            store.account("a").unwrap().unwrap().state,
            AccountState::Ready
        ),
        "the verdict was about credentials the disconnect already threw away"
    );
    assert_eq!(
        store
            .credentials("a")
            .unwrap()
            .unwrap()
            .access_token
            .expose(),
        "fresh"
    );
    let attempt = store.last_refresh_attempt("a", 1).unwrap().unwrap();
    assert_eq!(
        attempt.detail.as_deref(),
        Some("disconnected"),
        "the disconnect settled the attempt, and the late verdict does not rewrite it"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_reauth_verdict_from_a_previous_incarnation_is_dropped() {
    let now = Arc::new(AtomicI64::new(1_000_000));
    let rule = Rule {
        path_prefix: "/v1/oauth/token".into(),
        respond: Respond::Gated(Box::new(Respond::Json {
            status: 400,
            headers: vec![],
            body: br#"{"error":"invalid_grant","error_description":"revoked"}"#.to_vec(),
        })),
    };
    let (mgr, store, up) = manager_with(vec![rule], now.clone()).await;
    store
        .commit_credentials(
            "a",
            1,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new("old"),
                refresh_token: Some(Secret::new("rt")),
                issued_at: 0,
                expires_at: 1,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    let mgr = Arc::new(mgr);
    let refreshing = tokio::spawn({
        let mgr = mgr.clone();
        async move { mgr.acquire("a").await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while up.requests().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the refresh reached the token endpoint");

    // The operator deletes the slot and creates it again under the same id while the token request is in
    // flight. Both generations read 1 again, so only the incarnation tells the two holders apart.
    store.delete_account("a", "test").unwrap();
    store
        .insert_account(&AccountRow {
            id: "a".into(),
            kind: AccountKind::Oauth,
            label: "a".into(),
            enabled: true,
            identity: Some(phoebis_store::Identity {
                account_uuid: "00000000-0000-4000-8000-000000000001".into(),
                email: "test@example.invalid".into(),
                organization_uuid: None,
            }),
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state: AccountState::Ready,
        })
        .unwrap();
    store
        .commit_credentials(
            "a",
            2,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new("fresh"),
                refresh_token: Some(Secret::new("rt2")),
                issued_at: 1_000_000,
                expires_at: 1_000_000 + 3_600_000,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    up.release();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), refreshing)
        .await
        .expect("the refresh finished")
        .unwrap();
    assert!(matches!(outcome, Err(AcquireError::ReauthRequired)));

    assert_eq!(
        store.account("a").unwrap().unwrap().state,
        AccountState::Ready,
        "the verdict was about the holder the id had before"
    );
    assert_eq!(
        mgr.acquire("a").await.unwrap().secret.expose(),
        "fresh",
        "and the new holder serves its own bundle"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_late_invalid_grant_leaves_a_re_paired_bundle_alone() {
    // A re-pairing without a disconnect keeps the lifecycle generation, so the bundle generation is the only
    // thing that says this verdict is about credentials nobody uses any more.
    let now = Arc::new(AtomicI64::new(1_000_000));
    let rule = Rule {
        path_prefix: "/v1/oauth/token".into(),
        respond: Respond::Gated(Box::new(Respond::Json {
            status: 400,
            headers: vec![],
            body: br#"{"error":"invalid_grant","error_description":"revoked"}"#.to_vec(),
        })),
    };
    let (mgr, store, up) = manager_with(vec![rule], now.clone()).await;
    let stale = store
        .commit_credentials(
            "a",
            1,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new("old"),
                refresh_token: Some(Secret::new("rt")),
                issued_at: 0,
                expires_at: 1,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    let mgr = Arc::new(mgr);
    let refreshing = tokio::spawn({
        let mgr = mgr.clone();
        async move { mgr.acquire("a").await }
    });
    while up.requests().is_empty() {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }

    // The operator pairs the slot again while the token request is still in flight. No disconnect, so the
    // lifecycle generation does not move.
    store
        .commit_credentials(
            "a",
            1,
            1,
            Some(stale),
            None,
            &NewBundle {
                access_token: Secret::new("fresh"),
                refresh_token: Some(Secret::new("rt2")),
                issued_at: 1_000_000,
                expires_at: 1_000_000 + 3_600_000,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    up.release();
    assert!(matches!(
        refreshing.await.unwrap(),
        Err(AcquireError::ReauthRequired)
    ));

    assert!(
        matches!(
            store.account("a").unwrap().unwrap().state,
            AccountState::Ready
        ),
        "the rejected refresh token is not the one the slot now holds"
    );
    assert_eq!(
        mgr.acquire("a").await.unwrap().secret.expose(),
        "fresh",
        "the account keeps serving the bundle that replaced the rejected one"
    );
    assert_eq!(
        up.requests().len(),
        1,
        "the re-paired bundle is nowhere near its margin, so nothing is refreshed"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn an_attempt_another_holder_of_the_id_left_is_closed_at_start_rather_than_quarantining() {
    let now = Arc::new(AtomicI64::new(1_000_000));
    let (mgr, store, up) = manager_with(vec![], now).await;
    let generation = store
        .commit_credentials(
            "a",
            1,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new("old"),
                refresh_token: Some(Secret::new("rt")),
                issued_at: 0,
                expires_at: 1,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    // The generations match the stored bundle; only the incarnation says the attempt is not this holder's.
    store.open_refresh_attempt("a", 2, 1, generation).unwrap();
    assert!(mgr.recover_on_start().unwrap().is_empty());
    assert_eq!(
        store.account("a").unwrap().unwrap().state,
        AccountState::Ready
    );
    assert!(store.pending_refresh_attempts().unwrap().is_empty());
    assert_eq!(
        store
            .last_refresh_attempt("a", 2)
            .unwrap()
            .unwrap()
            .detail
            .as_deref(),
        Some("superseded")
    );
    up.shutdown().await;
}

/// The shape a power loss between the durable intent and the commit leaves: the attempt's row on disk and the
/// credentials still the bundle it named. A row whose bundle a later commit replaced is closed `superseded`
/// instead, which `an_attempt_another_holder_of_the_id_left_is_closed_at_start_rather_than_quarantining` and the
/// store's `a_commit_supersedes_an_attempt_left_open_by_the_bundle_it_replaces` cover.
#[tokio::test]
async fn an_attempt_row_without_its_commit_quarantines_the_account_at_start() {
    let now = Arc::new(AtomicI64::new(1_000_000));
    let (mgr, store, up) = manager_with(vec![], now).await;
    let generation = store
        .commit_credentials("a", 1, 1, None, None, &old_bundle(1_200_000))
        .unwrap();
    store.open_refresh_attempt("a", 1, 1, generation).unwrap();

    assert_eq!(mgr.recover_on_start().unwrap(), vec!["a".to_string()]);
    assert_eq!(
        store.account("a").unwrap().unwrap().state,
        AccountState::ReauthRequired
    );
    let attempt = store.last_refresh_attempt("a", 1).unwrap().unwrap();
    assert_eq!(
        (attempt.outcome.as_deref(), attempt.detail.as_deref()),
        (Some("failed"), Some("quarantined at start"))
    );
    up.shutdown().await;
}

#[tokio::test]
async fn an_unreadable_credential_row_quarantines_its_account_instead_of_failing_the_start() {
    let now = Arc::new(AtomicI64::new(1_000_000));
    let (mgr, store, up) = manager_with(vec![], now).await;
    store
        .insert_account(&AccountRow {
            id: "b".into(),
            kind: AccountKind::Oauth,
            label: "b".into(),
            enabled: true,
            identity: None,
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state: AccountState::Ready,
        })
        .unwrap();
    let bundle = || NewBundle {
        access_token: Secret::new("old"),
        refresh_token: Some(Secret::new("rt")),
        issued_at: 0,
        expires_at: 1,
        refresh_expires_at: None,
        scopes: "user:profile user:inference".into(),
    };
    let unreadable = store
        .commit_credentials("a", 1, 1, None, None, &bundle())
        .unwrap();
    let readable = store
        .commit_credentials("b", 1, 1, None, None, &bundle())
        .unwrap();
    store.open_refresh_attempt("a", 1, 1, unreadable).unwrap();
    store.open_refresh_attempt("b", 1, 1, readable).unwrap();
    // What a damaged row, a truncated write, or a storage key that no longer opens this row looks like from
    // here: the bundle cannot be compared against the attempt at all.
    store.debug_corrupt_credentials("a").unwrap();
    assert!(store.credentials("a").is_err());

    let quarantined = mgr.recover_on_start().unwrap();
    assert_eq!(
        quarantined,
        vec!["a".to_string(), "b".to_string()],
        "the account nobody can read is recovered like any other, and does not stop the one after it"
    );
    for account in ["a", "b"] {
        assert!(
            matches!(
                store.account(account).unwrap().unwrap().state,
                AccountState::ReauthRequired
            ),
            "{account} was left unrecovered"
        );
    }
    assert!(store.pending_refresh_attempts().unwrap().is_empty());
    up.shutdown().await;
}

#[tokio::test]
async fn a_lost_response_after_a_re_pair_quarantines_rather_than_serving_the_replaced_token() {
    // The fallback that keeps a usable token through an uncertain rotation must not reach back for a bundle
    // the account no longer has: by the time this resolves, the token it captured has been replaced.
    let now = Arc::new(AtomicI64::new(1_120_000));
    let rule = Rule {
        path_prefix: "/v1/oauth/token".into(),
        respond: Respond::Gated(Box::new(Respond::CloseAfterRequest)),
    };
    let (mgr, store, up) = manager_with(vec![rule], now).await;
    // 80 s left of a 400 s lifetime: inside the 100 s margin, so the refresh runs while the token still works.
    let stale = store
        .commit_credentials(
            "a",
            1,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new("old"),
                refresh_token: Some(Secret::new("rt")),
                issued_at: 800_000,
                expires_at: 1_200_000,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    let mgr = Arc::new(mgr);
    let refreshing = tokio::spawn({
        let mgr = mgr.clone();
        async move { mgr.acquire("a").await }
    });
    while up.requests().is_empty() {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    store
        .commit_credentials(
            "a",
            1,
            1,
            Some(stale),
            None,
            &NewBundle {
                access_token: Secret::new("fresh"),
                refresh_token: Some(Secret::new("rt2")),
                issued_at: 1_120_000,
                expires_at: 1_120_000 + 3_600_000,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    up.release();

    assert!(
        matches!(refreshing.await.unwrap(), Err(AcquireError::Quarantined)),
        "the token this refresh captured is not the account's any more, however usable it still looks"
    );
    assert_eq!(
        mgr.acquire("a").await.unwrap().secret.expose(),
        "fresh",
        "the bundle that replaced it is served by the next call"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_refresh_that_rotates_nothing_answers_that_no_newer_token_exists() {
    let now = Arc::new(AtomicI64::new(1_000_000));
    let rule = Rule {
        path_prefix: "/v1/oauth/token".into(),
        respond: Respond::Json {
            status: 503,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    };
    let (mgr, store, up) = manager_with(vec![rule], now.clone()).await;
    store
        .commit_credentials(
            "a",
            1,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new("old"),
                refresh_token: Some(Secret::new("rt")),
                issued_at: 1_000_000,
                expires_at: 1_400_000,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    assert!(
        matches!(
            mgr.refresh_now("a", 1).await,
            Err(AcquireError::NotRefreshed)
        ),
        "the token endpoint rotated nothing, so the rejected generation is still all there is"
    );
    assert_eq!(up.requests().len(), 1);
    assert_eq!(
        mgr.acquire("a").await.unwrap().secret.expose(),
        "old",
        "the unexpired stored token is still served to everything that has not been rejected"
    );
    assert_eq!(
        store.account("a").unwrap().unwrap().state,
        AccountState::Ready,
        "a token endpoint having a bad minute is not a reason to ask for re-pairing"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_refresh_traces_its_steps() {
    let (lines, _guard) = common::capture_tracing();

    let now = Arc::new(AtomicI64::new(1_000_000));
    let rule = Rule {
        path_prefix: "/v1/oauth/token".into(),
        respond: Respond::Json {
            status: 400,
            headers: vec![],
            body: br#"{"error":"invalid_grant","error_description":"revoked"}"#.to_vec(),
        },
    };
    let (mgr, store, up) = manager_with(vec![rule], now).await;
    store
        .commit_credentials(
            "a",
            1,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new("old"),
                refresh_token: Some(Secret::new("sk-ant-ort01-PLACEHOLDER")),
                issued_at: 0,
                expires_at: 1,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    assert!(matches!(
        mgr.acquire("a").await,
        Err(AcquireError::ReauthRequired)
    ));
    up.shutdown().await;

    let lines = lines.lock().unwrap();
    let text = lines.join("\n");
    assert!(
        lines.iter().all(|line| line.contains("account=a ")),
        "every event carries the account id: {text}"
    );
    assert!(
        lines.iter().any(|l| l.contains("attempt=")),
        "the attempt opened: {text}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("host=") && l.contains("platform.claude.com")),
        "the request sent: {text}"
    );
    assert!(
        lines.iter().any(|l| l.contains("status=400")),
        "the status read: {text}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("outcome=\"failed\"") && l.contains("detail=")),
        "the outcome resolved: {text}"
    );
    assert!(
        !text.contains("sk-ant-"),
        "a token reached the trace: {text}"
    );
    assert!(
        !text.contains("refresh_token="),
        "a refresh token reached the trace: {text}"
    );
}
