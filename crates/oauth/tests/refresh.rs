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
    control::{ControlClient, Resolver},
    request::serialize_head,
    Plane,
};
use fake_upstream::{FakeUpstream, Respond, Rule};
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
    let t = parse_token_response(control_response(ControlFixture::Token)).unwrap();
    assert_eq!(t.expires_in, 28800);
    assert_eq!(
        t.account.unwrap().uuid,
        "00000000-0000-4000-8000-000000000001"
    );
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
    let resolver: Resolver = Arc::new(move |_| Some(addr));
    let control = Arc::new(ControlClient::new(p, b.build(), resolver));
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
            state: AccountState::Ready,
        })
        .unwrap();
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
async fn a_lost_response_keeps_the_old_token_until_it_expires_then_quarantines() {
    let now = Arc::new(AtomicI64::new(1_000_000));
    let rule = Rule {
        path_prefix: "/v1/oauth/token".into(),
        respond: Respond::CloseAfterRequest,
    };
    let (mgr, store, up) = manager_with(vec![rule], now.clone()).await;
    // 80 s left of a 400 s lifetime: inside the 100 s margin but still valid, so the refresh runs while the token is usable.
    now.store(1_120_000, Ordering::SeqCst);
    store
        .commit_credentials(
            "a",
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
    assert_eq!(
        mgr.acquire("a").await.unwrap().secret.expose(),
        "old",
        "an uncertain rotation does not cost a usable token"
    );
    assert_eq!(store.pending_refresh_attempts().unwrap().len(), 1);
    now.store(1_300_000, Ordering::SeqCst);
    assert!(matches!(
        mgr.acquire("a").await,
        Err(AcquireError::Quarantined)
    ));
    assert_eq!(
        up.requests().len(),
        1,
        "an uncertain attempt blocks further refreshes"
    );
    let quarantined = mgr.recover_on_start().unwrap();
    assert_eq!(quarantined, vec!["a".to_string()]);
    assert!(matches!(
        mgr.acquire("a").await,
        Err(AcquireError::ReauthRequired)
    ));
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
