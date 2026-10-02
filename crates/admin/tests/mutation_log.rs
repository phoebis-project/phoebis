//! The trail of what the operator changed: every admin mutation, admitted or refused, writes one `info` line
//! naming its method, matched route, status, entity and session, and nothing a log must not hold.
mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::Response;
use common::{admin_app, delete, get, json, login, patch, post, put};

/// The formatter's output, kept in memory so the test reads back exactly what the host's journal would hold.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Captured {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }

    /// The fields of every mutation line, in the order the lines were written.
    fn mutations(&self) -> Vec<String> {
        const MARK: &str = " INFO phoebis_admin::routes: admin mutation ";
        self.text()
            .lines()
            .filter_map(|line| line.split_once(MARK).map(|(_, fields)| fields.to_string()))
            .collect()
    }
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
    let refused = delete(&app, "/admin/pools/main", None).await;
    assert_eq!(status(&refused), 401);
    let unknown = delete(&app, "/admin/nope", Some(&cookie)).await;
    assert_eq!(status(&unknown), 404);

    let admitted = format!("credential=session session={session}");
    assert_eq!(
        captured.mutations(),
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
