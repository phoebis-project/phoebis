//! The trail of what the operator changed: every admin mutation, admitted or refused, writes one `info` line
//! naming its method, matched route, status, entity and session, and nothing a log must not hold.
mod common;

use axum::body::Body;
use axum::http::Response;
use common::{
    admin_app, admin_parts, delete, exchanged_session_for_new, get, json, login, patch, post, put,
};
use fake_upstream::Captured;

/// The fields of every mutation line `captured` holds, in the order the lines were written.
fn mutations(captured: &Captured) -> Vec<String> {
    const MARK: &str = " INFO phoebis_admin::routes: admin mutation ";
    captured
        .text()
        .lines()
        .filter_map(|line| line.split_once(MARK).map(|(_, fields)| fields.to_string()))
        .collect()
}

fn status(response: &Response<Body>) -> u16 {
    response.status().as_u16()
}

#[tokio::test]
async fn every_admin_mutation_writes_one_line_with_its_route_entity_and_session_and_no_secret() {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(captured.clone())
        .finish();
    // The harness drives every request through `oneshot` on this test's own task, so a thread-local
    // subscriber sees every line the gate writes.
    let _guard = tracing::subscriber::set_default(subscriber);
    let (app, _up) = admin_app().await;

    let cookie = login(&app, "admin-secret").await;
    let session = json(get(&app, "/admin/session", Some(&cookie)).await).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let _ = get(&app, "/admin/pools", Some(&cookie)).await;

    let created = post(
        &app,
        "/admin/pools",
        r#"{"id":"main","label":"label-sentinel","strategy":"priority"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(status(&created), 201);
    let edited = patch(
        &app,
        "/admin/pools/main?note=query-sentinel",
        r#"{"label":"edited-sentinel"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(status(&edited), 200);
    let members = put(
        &app,
        "/admin/pools/main/members",
        r#"{"members":[{"account_id":"direct","priority":0}]}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(status(&members), 200);
    let key = post(
        &app,
        "/admin/keys",
        r#"{"id":"desk","label":"Desk","pool_id":"main"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(status(&key), 201);
    let key_secret = json(key).await["secret"].as_str().unwrap().to_string();
    let released = post(&app, "/admin/sessions/desk/s1/release", "", Some(&cookie)).await;
    let replayed = post(&app, "/admin/ledger/parked/replay", "", Some(&cookie)).await;
    assert_eq!(status(&replayed), 200);
    let refused = delete(&app, "/admin/pools/main", None).await;
    assert_eq!(status(&refused), 401);
    let unknown = delete(&app, "/admin/nope", Some(&cookie)).await;
    assert_eq!(status(&unknown), 404);

    let admitted = format!("credential=session session={session}");
    assert_eq!(
        mutations(&captured),
        vec![
            "method=POST route=/admin/session status=204 entity=admin_session id=- credential=none"
                .to_string(),
            format!("method=POST route=/admin/pools status=201 entity=pool id=main {admitted}"),
            format!("method=PATCH route=/admin/pools/{{id}} status=200 entity=pool id=main {admitted}"),
            format!(
                "method=PUT route=/admin/pools/{{id}}/members status=200 entity=pool id=main {admitted}"
            ),
            format!("method=POST route=/admin/keys status=201 entity=key id=desk {admitted}"),
            format!(
                "method=POST route=/admin/sessions/{{key}}/{{session}}/release status={} entity=session \
                 id=desk/s1 {admitted}",
                status(&released)
            ),
            format!(
                "method=POST route=/admin/ledger/parked/replay status=200 entity=ledger id=- {admitted}"
            ),
            "method=DELETE route=/admin/pools/{id} status=401 entity=pool id=main credential=none"
                .to_string(),
            format!("method=DELETE route=/admin/{{*rest}} status=404 entity=none id=- {admitted}"),
        ],
        "one line per mutation and none for the reads"
    );

    let text = captured.text();
    for (what, needle) in [
        ("the admin secret", "admin-secret"),
        ("a body field", "label-sentinel"),
        ("a body field", "edited-sentinel"),
        ("the created key's secret", key_secret.as_str()),
        ("the cookie's value", cookie.as_str()),
        ("a query string", "query-sentinel"),
    ] {
        assert!(!text.contains(needle), "the log holds {what}: {text}");
    }
}

/// A create whose row committed but whose answer is a refusal (here the registry reload failing) still names the row
/// it left behind, so the trail never reads `id=-` for an entity that exists.
#[tokio::test]
async fn a_create_refused_after_its_row_committed_logs_the_created_id() {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(captured.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let (app, _up, store, _router) = admin_parts().await;

    let cookie = login(&app, "admin-secret").await;
    let session = json(get(&app, "/admin/session", Some(&cookie)).await).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    for (path, body) in [
        (
            "/admin/pools",
            r#"{"id":"main","label":"Main","strategy":"priority"}"#,
        ),
        (
            "/admin/keys",
            r#"{"id":"desk","label":"Desk","pool_id":"main"}"#,
        ),
        (
            "/admin/accounts",
            r#"{"id":"spare","kind":"oauth","label":"Spare"}"#,
        ),
    ] {
        store.debug_fail_next_load();
        let refused = post(&app, path, body, Some(&cookie)).await;
        assert_eq!(status(&refused), 503, "{path}");
    }
    assert!(
        store.pool("main").unwrap().is_some()
            && store.api_key("desk").unwrap().is_some()
            && store.account("spare").unwrap().is_some(),
        "every row was written before its reload failed"
    );

    let admitted = format!("credential=session session={session}");
    assert_eq!(
        mutations(&captured),
        vec![
            "method=POST route=/admin/session status=204 entity=admin_session id=- credential=none"
                .to_string(),
            format!("method=POST route=/admin/pools status=503 entity=pool id=main {admitted}"),
            format!("method=POST route=/admin/keys status=503 entity=key id=desk {admitted}"),
            format!(
                "method=POST route=/admin/accounts status=503 entity=account id=spare {admitted}"
            ),
        ]
    );
}

/// The confirm of a pairing started with `new` creates the account, so its line names the row it made rather than the
/// session its path carries. The start and the callback keep naming the session.
#[tokio::test]
async fn a_confirm_of_a_pairing_started_with_new_names_the_created_account() {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(captured.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let (app, _up) = admin_app().await;

    let cookie = login(&app, "admin-secret").await;
    let session = json(get(&app, "/admin/session", Some(&cookie)).await).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let pairing = exchanged_session_for_new(&app, &cookie, "fresh", "Fresh").await;
    let confirmed = post(
        &app,
        &format!("/admin/oauth/sessions/{pairing}/confirm"),
        "",
        Some(&cookie),
    )
    .await;
    assert_eq!(status(&confirmed), 200);

    let admitted = format!("credential=session session={session}");
    assert_eq!(
        mutations(&captured),
        vec![
            "method=POST route=/admin/session status=204 entity=admin_session id=- credential=none"
                .to_string(),
            format!(
                "method=POST route=/admin/oauth/sessions status=201 entity=pairing id={pairing} {admitted}"
            ),
            format!(
                "method=POST route=/admin/oauth/sessions/{{id}}/callback status=200 entity=pairing \
                 id={pairing} {admitted}"
            ),
            format!(
                "method=POST route=/admin/oauth/sessions/{{id}}/confirm status=200 entity=account id=fresh \
                 {admitted}"
            ),
        ]
    );
}

/// A confirm whose row committed but whose registry reload failed answers 503 and still names the account it created,
/// as the create route does, so the trail never reads a session id for a row that exists.
#[tokio::test]
async fn a_confirm_refused_after_its_row_committed_logs_the_created_account() {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(captured.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let (app, _up, store, _router) = admin_parts().await;

    let cookie = login(&app, "admin-secret").await;
    let session = json(get(&app, "/admin/session", Some(&cookie)).await).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let pairing = exchanged_session_for_new(&app, &cookie, "fresh", "Fresh").await;
    store.debug_fail_next_load();
    let refused = post(
        &app,
        &format!("/admin/oauth/sessions/{pairing}/confirm"),
        "",
        Some(&cookie),
    )
    .await;
    assert_eq!(status(&refused), 503);
    assert!(
        store.account("fresh").unwrap().is_some(),
        "the row was written before its reload failed"
    );

    let admitted = format!("credential=session session={session}");
    assert_eq!(
        mutations(&captured).last(),
        Some(&format!(
            "method=POST route=/admin/oauth/sessions/{{id}}/confirm status=503 entity=account id=fresh \
             {admitted}"
        ))
    );
}

#[tokio::test]
async fn a_bearer_mutation_names_the_bearer_credential_and_no_secret() {
    use tower::ServiceExt;
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(captured.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let (app, _up) = admin_app().await;

    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/admin/accounts/business/pause")
        .header("authorization", "Bearer admin-secret")
        .body(Body::empty())
        .unwrap();
    let paused = app.clone().oneshot(request).await.unwrap();
    assert_eq!(status(&paused), 204);

    assert_eq!(
        mutations(&captured),
        vec![
            "method=POST route=/admin/accounts/{id}/pause status=204 entity=account id=business \
             credential=bearer session=bearer"
                .to_string()
        ]
    );
    let text = captured.text();
    assert!(
        !text.contains("admin-secret"),
        "the log holds the bearer: {text}"
    );
}
