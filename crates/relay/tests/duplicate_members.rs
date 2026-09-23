//! A body whose top-level object repeats a routing member is refused before anything reaches the upstream.

mod common;

use axum::body::Body;
use axum::http::Request;
use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule};
use http_body_util::BodyExt;
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

async fn send(h: &Harness, path: &str, body: &str) -> (u16, String, String) {
    let request = Request::post(path)
        .header("authorization", format!("Bearer {KEY}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = h.app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let code = response
        .headers()
        .get("x-phoebis-error-code")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, code, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn a_second_top_level_model_is_refused_before_admission() {
    let up = FakeUpstream::start(vec![ok_rule()]).await;
    let h = harness_restricted_to(&up, "claude-sonnet-4-5").await;
    for body in [
        concat!(
            r#"{"model":"claude-sonnet-4-5","model":"claude-opus-5","#,
            r#""messages":[],"max_tokens":8}"#
        ),
        concat!(
            r#"{"model":"claude-sonnet-4-5","messages":[],"max_tokens":8,"#,
            r#""model":"claude-opus-5"}"#
        ),
    ] {
        let (status, code, text) = send(&h, "/v1/messages", body).await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(code, "body_shape", "{text}");
        assert!(
            // The wire body is JSON, so the message's own embedded quotes are backslash-escaped in it.
            text.contains(r#"top-level member \"model\" more than once"#),
            "{text}"
        );
    }
    assert!(up.requests().is_empty(), "nothing reached the upstream");
    up.shutdown().await;
}

#[tokio::test]
async fn a_repeated_metadata_or_stream_member_is_refused_and_a_nested_repeat_is_not() {
    let up = FakeUpstream::start(vec![ok_rule()]).await;
    let h = harness_restricted_to(&up, "claude-sonnet-4-5").await;
    let twice = concat!(
        r#"{"model":"claude-sonnet-4-5","metadata":{"user_id":"a"},"#,
        r#""metadata":{"user_id":"b"},"messages":[],"max_tokens":8}"#
    );
    let (status, code, text) = send(&h, "/v1/messages", twice).await;
    assert_eq!(status, 400);
    assert_eq!(code, "body_shape");
    assert!(
        // The wire body is JSON, so the message's own embedded quotes are backslash-escaped in it.
        text.contains(r#"top-level member \"metadata\" more than once"#),
        "{text}"
    );
    let streams = r#"{"model":"claude-sonnet-4-5","stream":false,"stream":true,"messages":[],"max_tokens":8}"#;
    let (status, code, text) = send(&h, "/v1/messages", streams).await;
    assert_eq!(status, 400);
    assert_eq!(code, "body_shape");
    assert!(
        // The wire body is JSON, so the message's own embedded quotes are backslash-escaped in it.
        text.contains(r#"top-level member \"stream\" more than once"#),
        "{text}"
    );
    assert!(up.requests().is_empty());
    // A repeat below the top level is the upstream's to judge, and the wire is unchanged for it.
    let nested = r#"{"model":"claude-sonnet-4-5","metadata":{"model":"x","model":"y"},"messages":[],"max_tokens":8}"#;
    let (status, _, _) = send(&h, "/v1/messages", nested).await;
    assert_eq!(status, 200);
    let received: serde_json::Value = serde_json::from_slice(&up.requests()[0].body).unwrap();
    assert_eq!(received["model"], "claude-sonnet-4-5");
    up.shutdown().await;
}

#[tokio::test]
async fn a_scan_that_cannot_finish_refuses_the_body_and_a_deep_single_member_is_admitted() {
    let up = FakeUpstream::start(vec![ok_rule()]).await;
    let h = harness_restricted_to(&up, "claude-sonnet-4-5").await;
    let deep = |levels: usize| format!("{}1{}", "[".repeat(levels), "]".repeat(levels));
    // An allowed first spelling, a value the scanner will not enter, and a restricted second spelling behind
    // it: the scan stops before the repeat, and an unfinished scan proves nothing.
    let past_the_scan = format!(
        r#"{{"model":"claude-sonnet-4-5","padding":{},"model":"claude-opus-5","messages":[],"max_tokens":8}}"#,
        deep(130)
    );
    let (status, code, text) = send(&h, "/v1/messages", &past_the_scan).await;
    assert_eq!(status, 400);
    assert_eq!(code, "invalid_body");
    assert!(text.contains("nests deeper than the relay reads"), "{text}");
    // The second spelling escaped: the keys are compared decoded.
    let escaped_key = "\\u006dodel";
    let escaped = format!(
        r#"{{"model":"claude-sonnet-4-5","messages":[],"max_tokens":8,"{escaped_key}":"claude-opus-5"}}"#
    );
    let (status, code, text) = send(&h, "/v1/messages", &escaped).await;
    assert_eq!(status, 400);
    assert_eq!(code, "body_shape");
    assert!(
        // The wire body is JSON, so the message's own embedded quotes are backslash-escaped in it.
        text.contains(r#"top-level member \"model\" more than once"#),
        "{text}"
    );
    assert!(up.requests().is_empty(), "nothing reached the upstream");
    // One spelling and a value nested past anything a client sends, inside the scanner's depth: admitted, and
    // the guard bodies nest under ten levels, so the regression guard is untouched.
    let within = format!(
        r#"{{"model":"claude-sonnet-4-5","padding":{},"messages":[],"max_tokens":8}}"#,
        deep(100)
    );
    let (status, _, _) = send(&h, "/v1/messages", &within).await;
    assert_eq!(status, 200);
    up.shutdown().await;
}

#[tokio::test]
async fn a_second_top_level_model_is_refused_on_the_count_tokens_route_too() {
    let up = FakeUpstream::start(vec![ok_rule()]).await;
    let h = harness_restricted_to(&up, "claude-sonnet-4-5").await;
    let body = concat!(
        r#"{"model":"claude-sonnet-4-5","model":"claude-opus-5","#,
        r#""messages":[]}"#
    );
    let (status, code, text) = send(&h, "/v1/messages/count_tokens", body).await;
    assert_eq!(status, 400);
    assert_eq!(code, "body_shape");
    assert!(
        // The wire body is JSON, so the message's own embedded quotes are backslash-escaped in it.
        text.contains(r#"top-level member \"model\" more than once"#),
        "{text}"
    );
    assert!(up.requests().is_empty(), "nothing reached the upstream");
    up.shutdown().await;
}
