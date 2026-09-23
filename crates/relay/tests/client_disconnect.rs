//! A body stream that ends before it is complete is the caller leaving, not the caller sending too much.

mod common;

use axum::body::Body;
use axum::http::Request;
use bytes::Bytes;
use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule};
use futures::stream;
use tower::ServiceExt;

fn ok_rule() -> Rule {
    Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"id":"msg_1","type":"message","content":[],"usage":{"input_tokens":1,"output_tokens":1}}"#
                .to_vec(),
        },
    }
}

#[tokio::test]
async fn a_body_stream_that_errors_mid_read_is_answered_as_incomplete_not_too_large() {
    let up = FakeUpstream::start(vec![ok_rule()]).await;
    let h = harness_restricted_to(&up, "claude-sonnet-4-5").await;
    let partial = r#"{"model":"claude-sonnet-4-5","messages":[{"role":"#;
    let chunks = stream::iter(vec![
        Ok::<Bytes, std::io::Error>(Bytes::from_static(partial.as_bytes())),
        Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "the caller hung up",
        )),
    ]);
    let request = Request::post("/v1/messages")
        .header("authorization", format!("Bearer {KEY}"))
        .header("content-type", "application/json")
        .body(Body::from_stream(chunks))
        .unwrap();
    let response = h.app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status().as_u16(), 400);
    let code = response
        .headers()
        .get("x-phoebis-error-code")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert_eq!(code, "request_incomplete");
    assert!(up.requests().is_empty(), "the body never reached admission");
    up.shutdown().await;
}
