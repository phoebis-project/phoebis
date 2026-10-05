//! A leg's credential and its revision fence come from the router's snapshot and the relay's credential caches, so
//! the request path reads the registry only when a credential moved: a rotation, a recreated account or a
//! reconnected slot.
mod common;
use std::time::Duration;

use axum::body::Body;
use common::*;
use fake_upstream::{FakeUpstream, Rule};
use http_body_util::BodyExt;
use phoebis_store::{AccountState, NewBundle, Secret};
use tower::ServiceExt;

const A: &str = "sk-ant-api03-AAAA";
const A_ROTATED: &str = "sk-ant-api03-AAAB";
const B: &str = "sk-ant-api03-BBBB";
const OAUTH_A: &str = "sk-ant-oat01-AAAA";
const OAUTH_A_REPAIRED: &str = "sk-ant-oat01-AAAB";
const OAUTH_B: &str = "sk-ant-oat01-BBBB";

async fn upstream() -> FakeUpstream {
    FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await
}

/// One request on `session`, answered 200 and read to its end.
async fn leg(h: &Harness, session: &str) -> axum::http::Response<Body> {
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", session))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp
}

async fn legs(h: &Harness, count: usize) {
    for _ in 0..count {
        leg(h, "s1").await.into_body().collect().await.unwrap();
    }
}

#[tokio::test]
async fn an_api_key_account_pays_no_credential_read_after_its_first_leg() {
    let up = upstream().await;
    let h = harness(&up, &[api_key("a", A, 0)], PoolSpec::default()).await;
    legs(&h, 1).await;
    let warm = h.store.debug_credential_reads();
    legs(&h, 9).await;
    assert_eq!(
        h.store.debug_credential_reads(),
        warm,
        "the nine later legs read neither the key nor the revision"
    );
    assert_eq!(credentials_seen(&up), [A; 10]);
    up.shutdown().await;
}

#[tokio::test]
async fn an_oauth_account_pays_no_credential_read_after_its_first_leg() {
    let up = upstream().await;
    let h = harness(&up, &[oauth("a", OAUTH_A, 0)], PoolSpec::default()).await;
    legs(&h, 1).await;
    let warm = h.store.debug_credential_reads();
    legs(&h, 9).await;
    assert_eq!(
        h.store.debug_credential_reads(),
        warm,
        "the nine later legs read neither the row, the bundle nor the revision"
    );
    assert_eq!(credentials_seen(&up), [OAUTH_A; 10]);
    up.shutdown().await;
}

#[tokio::test]
async fn a_rotated_key_is_read_once_after_the_reload_and_goes_out_on_the_next_leg() {
    let up = upstream().await;
    let h = harness(&up, &[api_key("a", A, 0)], PoolSpec::default()).await;
    legs(&h, 1).await;
    h.store
        .set_api_key_secret("a", &Secret::new(A_ROTATED))
        .unwrap();
    h.router.reload().await.unwrap();
    // The rotation moved the row's revision, so the catalog refills its listing for the new row in the background
    // and reads the row as it commits; fetched here, that read lands before the count is taken.
    let row = h.router.snapshot().account("a").unwrap().clone();
    h.catalog.fetch(&row).await;
    let reloaded = h.store.debug_credential_reads();
    legs(&h, 1).await;
    assert_eq!(
        h.store.debug_credential_reads(),
        reloaded + 1,
        "the snapshot's generation moved past the cached one, so the key is read once"
    );
    legs(&h, 1).await;
    assert_eq!(h.store.debug_credential_reads(), reloaded + 1);
    assert_eq!(credentials_seen(&up), [A, A_ROTATED, A_ROTATED]);
    up.shutdown().await;
}

#[tokio::test]
async fn a_recreated_account_under_the_same_id_and_generation_serves_the_new_holders_secret() {
    let up = upstream().await;
    let h = harness(&up, &[api_key("a", A, 0)], PoolSpec::default()).await;
    legs(&h, 1).await;
    h.store.delete_account("a", "test").unwrap();
    h.store
        .create_account(
            &test_account_row(&api_key("a", A_ROTATED, 0)),
            Some(&Secret::new(A_ROTATED)),
            NOW,
            "test",
        )
        .unwrap();
    // The delete took the membership with it.
    h.store
        .set_pool_members("main", &[member("a", 0)], "test")
        .unwrap();
    h.router.reload().await.unwrap();
    let snapshot = h.router.snapshot();
    let row = snapshot.account("a").unwrap();
    assert_eq!(
        (
            row.incarnation,
            row.revision,
            snapshot.credential_generation("a")
        ),
        (2, 1, Some(1)),
        "only the incarnation tells the new holder from the old"
    );
    legs(&h, 1).await;
    assert_eq!(credentials_seen(&up), [A, A_ROTATED]);
    up.shutdown().await;
}

#[tokio::test]
async fn a_disconnected_and_reconnected_account_serves_its_new_credential() {
    let up = upstream().await;
    let h = harness(&up, &[api_key("a", A, 0)], PoolSpec::default()).await;
    legs(&h, 1).await;
    h.store.bump_lifecycle("a").unwrap();
    h.store
        .set_api_key_secret("a", &Secret::new(A_ROTATED))
        .unwrap();
    h.store.set_state("a", AccountState::Ready).unwrap();
    h.router.reload().await.unwrap();
    let snapshot = h.router.snapshot();
    let row = snapshot.account("a").unwrap();
    assert_eq!(
        (
            row.incarnation,
            row.lifecycle_generation,
            snapshot.credential_generation("a")
        ),
        (1, 2, Some(1)),
        "the generation restarted under the new lifecycle"
    );
    legs(&h, 1).await;
    assert_eq!(credentials_seen(&up), [A, A_ROTATED]);
    up.shutdown().await;
}

// On worker threads: a credential read parked on the hold must not stall the runtime the test itself runs on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_acquisition_suspended_across_a_rotation_and_a_reload_is_refused_stale() {
    let up = upstream().await;
    let h = harness(
        &up,
        &[api_key("a", A, 0), api_key("b", B, 1)],
        PoolSpec::default(),
    )
    .await;
    let hold = h.store.debug_hold_next_credential_read();
    let app = h.app.clone();
    let inflight = tokio::spawn(async move {
        app.oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
            .await
            .unwrap()
    });
    wait_for(Duration::from_secs(5), || hold.is_parked().then_some(())).await;
    // The leg selected `a` at revision 1 and waits in its key read; the rotation and the reload land under it.
    h.store
        .set_api_key_secret("a", &Secret::new(A_ROTATED))
        .unwrap();
    h.router.reload().await.unwrap();
    hold.release();
    let resp = inflight.await.unwrap();
    assert_eq!(resp.status(), 200);
    let o = outcome(&resp);
    assert_eq!(
        (o.account_id.as_str(), o.attempt),
        ("b", 1),
        "the stale row is not dispatched; the request is re-selected"
    );
    resp.into_body().collect().await.unwrap();
    assert_eq!(
        credentials_seen(&up),
        [B],
        "nothing went out under a's old key"
    );
    leg(&h, "s2").await.into_body().collect().await.unwrap();
    assert_eq!(credentials_seen(&up), [B, A_ROTATED]);
    up.shutdown().await;
}

/// The generation clause of the fence alone: a re-pair keeps the row's revision, so an acquisition whose read
/// returned the old bundle before the pairing committed and the reload landed is refused by the generation it
/// carries rather than dispatched with a token the registry has replaced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_that_returned_before_a_re_pair_and_its_reload_is_refused_stale() {
    let up = upstream().await;
    let h = harness(
        &up,
        &[oauth("a", OAUTH_A, 0), oauth("b", OAUTH_B, 1)],
        PoolSpec::default(),
    )
    .await;
    // The catalog's fill at start warmed the bundle cache, so the leg's acquisition reads the store only once the
    // entry is gone.
    h.manager.invalidate("a");
    let hold = h.store.debug_hold_after_next_credential_read();
    let app = h.app.clone();
    let inflight = tokio::spawn(async move {
        app.oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
            .await
            .unwrap()
    });
    wait_for(Duration::from_secs(5), || hold.is_parked().then_some(())).await;
    h.store
        .commit_pairing(
            "a",
            1,
            1,
            Some(1),
            &NewBundle {
                access_token: Secret::new(OAUTH_A_REPAIRED),
                refresh_token: None,
                issued_at: NOW,
                expires_at: i64::MAX / 2,
                refresh_expires_at: None,
                scopes: "user:inference".into(),
            },
        )
        .unwrap();
    h.router.reload().await.unwrap();
    let snapshot = h.router.snapshot();
    assert_eq!(
        (
            snapshot.account("a").unwrap().revision,
            snapshot.credential_generation("a")
        ),
        (1, Some(2)),
        "the re-pair moved the generation alone"
    );
    hold.release();
    let resp = inflight.await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        outcome(&resp).account_id,
        "b",
        "the old bundle is not dispatched; the request is re-selected"
    );
    resp.into_body().collect().await.unwrap();
    assert_eq!(
        credentials_seen(&up),
        [OAUTH_B],
        "nothing went out under a's replaced token"
    );
    up.shutdown().await;
}

/// The pairing's invalidation outruns a read that returned the replaced bundle: the read drops its bundle rather than
/// put it back, so an acquisition that names no generation (the usage poll's, the catalog's) answers the new token.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_outrun_by_a_pairings_invalidation_does_not_put_the_replaced_bundle_back() {
    let up = upstream().await;
    let h = harness(
        &up,
        &[oauth("a", OAUTH_A, 0), oauth("b", OAUTH_B, 1)],
        PoolSpec::default(),
    )
    .await;
    h.manager.invalidate("a");
    let hold = h.store.debug_hold_after_next_credential_read();
    let app = h.app.clone();
    let inflight = tokio::spawn(async move {
        app.oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
            .await
            .unwrap()
    });
    wait_for(Duration::from_secs(5), || hold.is_parked().then_some(())).await;
    h.store
        .commit_pairing(
            "a",
            1,
            1,
            Some(1),
            &NewBundle {
                access_token: Secret::new(OAUTH_A_REPAIRED),
                refresh_token: None,
                issued_at: NOW,
                expires_at: i64::MAX / 2,
                refresh_expires_at: None,
                scopes: "user:inference".into(),
            },
        )
        .unwrap();
    h.router.reload().await.unwrap();
    // What the pairing's confirmation does once its commit landed.
    h.manager.invalidate("a");
    hold.release();
    let resp = inflight.await.unwrap();
    let token = h.manager.acquire("a").await.unwrap();
    assert_eq!(
        token.secret.expose(),
        OAUTH_A_REPAIRED,
        "the cache answers the pairing's bundle, not the one the outrun read returned"
    );
    assert_eq!(resp.status(), 200);
    resp.into_body().collect().await.unwrap();
    assert_eq!(
        credentials_seen(&up),
        [OAUTH_A_REPAIRED],
        "the outrun read answers the bundle stored now, and nothing went out under the replaced token"
    );
    up.shutdown().await;
}
