//! A body whose top-level object repeats a routing member, or whose billing line's path repeats a member or spells
//! one with an escape, is refused before anything reaches the upstream.

mod common;

use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule};

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
    // A repeat of a `metadata` member the relay never reads is the upstream's to judge, and the wire is unchanged
    // for it.
    let nested = concat!(
        r#"{"model":"claude-sonnet-4-5","metadata":{"model":"x","model":"y","#,
        r#""user_id":"{\"device_id\":\"test-device\",\"account_uuid\":\"\",\"session_id\":\"s1\"}"},"messages":[],"max_tokens":8}"#
    );
    let (status, _, _) = send(&h, "/v1/messages", nested).await;
    assert_eq!(status, 200);
    let received: serde_json::Value = serde_json::from_slice(&up.requests()[0].body).unwrap();
    assert_eq!(received["model"], "claude-sonnet-4-5");
    up.shutdown().await;
}

#[tokio::test]
async fn a_repeated_metadata_user_id_is_refused_raw_or_escaped_and_a_repeat_inside_a_tool_input_is_not(
) {
    let up = FakeUpstream::start(vec![ok_rule()]).await;
    let h = harness_restricted_to(&up, "claude-sonnet-4-5").await;
    // The account splice lands on one spelling and a last-wins parser upstream would read the other.
    for (first, second) in [
        ("user_id", "user_id"),
        ("user_id", "\\u0075ser_id"),
        ("\\u0075ser_id", "user_id"),
    ] {
        let body = format!(
            r#"{{"model":"claude-sonnet-4-5","metadata":{{"{first}":"a","{second}":"b"}},"messages":[],"max_tokens":8}}"#
        );
        let (status, code, text) = send(&h, "/v1/messages", &body).await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(code, "body_shape", "{text}");
        assert!(
            // The wire body is JSON, so the message's own embedded quotes are backslash-escaped in it.
            text.contains(r#"member \"metadata.user_id\" more than once"#),
            "{text}"
        );
        assert!(!text.contains("top-level"), "{text}");
    }
    assert!(up.requests().is_empty(), "nothing reached the upstream");
    // The same object inside a tool call's input is the client's data, relayed as sent.
    let tool_input = concat!(
        r#"{"model":"claude-sonnet-4-5","messages":[{"role":"assistant","content":[{"type":"tool_use","#,
        r#""id":"t1","name":"x","input":{"metadata":{"user_id":"a","user_id":"b"}}}]}],"max_tokens":8,"#,
        r#""metadata":{"user_id":"{\"device_id\":\"test-device\",\"account_uuid\":\"\",\"session_id\":\"s1\"}"}}"#
    );
    let (status, _, text) = send(&h, "/v1/messages", tool_input).await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(
        up.requests()[0].body,
        tool_input.as_bytes(),
        "the wire body is the one sent"
    );
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
        r#"{{"model":"claude-sonnet-4-5","padding":{},"messages":[],"max_tokens":8,"metadata":{{"user_id":"{{\"device_id\":\"test-device\",\"account_uuid\":\"\",\"session_id\":\"s1\"}}"}}}}"#,
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

#[tokio::test]
async fn a_billing_block_repeating_its_text_is_refused_before_the_placeholder_exempts_it() {
    let up = FakeUpstream::start(vec![ok_rule()]).await;
    let h = harness_restricted_to(&up, "claude-sonnet-4-5").await;
    // The drift check reads the first spelling and a last-wins parser upstream the second, raw or escaped.
    for second in ["text", "\\u0074ext"] {
        let body = format!(
            r#"{{"model":"claude-sonnet-4-5","system":[{{"type":"text","text":"{PLACEHOLDER_LINE}","{second}":"{UNCHECKED_LINE}"}}],"messages":[],"max_tokens":8}}"#
        );
        let (status, code, text) = send(&h, "/v1/messages", &body).await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(code, "body_shape", "{text}");
        assert!(
            // The wire body is JSON, so the message's own embedded quotes are backslash-escaped in it.
            text.contains(r#"member \"system[0].text\" more than once"#),
            "{text}"
        );
    }
    assert!(up.requests().is_empty(), "nothing reached the upstream");
    // A repeat in a later block is off the billing line's path and stays the upstream's to judge.
    let later = format!(
        r#"{{"model":"claude-sonnet-4-5","system":[{{"type":"text","text":"{BARE_PLACEHOLDER_LINE}"}},{{"type":"text","text":"a","text":"b"}}],"messages":[],"max_tokens":8,"metadata":{{"user_id":"{{\"device_id\":\"test-device\",\"account_uuid\":\"\",\"session_id\":\"s1\"}}"}}}}"#
    );
    let (status, _, text) = send(&h, "/v1/messages", &later).await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(up.requests().len(), 1);
    up.shutdown().await;
}

#[tokio::test]
async fn a_body_repeating_its_top_level_system_is_refused_before_the_placeholder_exempts_it() {
    let up = FakeUpstream::start(vec![ok_rule()]).await;
    let h = harness_restricted_to(&up, "claude-sonnet-4-5").await;
    for second in ["system", "\\u0073ystem"] {
        let body = format!(
            r#"{{"model":"claude-sonnet-4-5","system":[{{"type":"text","text":"{PLACEHOLDER_LINE}"}}],"messages":[],"max_tokens":8,"{second}":[{{"type":"text","text":"{UNCHECKED_LINE}"}}]}}"#
        );
        let (status, code, text) = send(&h, "/v1/messages", &body).await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(code, "body_shape", "{text}");
        assert!(
            // The wire body is JSON, so the message's own embedded quotes are backslash-escaped in it.
            text.contains(r#"top-level member \"system\" more than once"#),
            "{text}"
        );
    }
    assert!(up.requests().is_empty(), "nothing reached the upstream");
    up.shutdown().await;
}

#[tokio::test]
async fn a_stamp_bearing_member_spelled_with_an_escape_is_refused_as_a_repeat_is() {
    let up = FakeUpstream::start(vec![ok_rule()]).await;
    let h = harness_restricted_to(&up, "claude-sonnet-4-5").await;
    // One spelling, escaped: the decoded walk reads the stamp, the raw walk the restamp edits through does not.
    // Refused whether or not an edit would touch the body, so the hash never travels unverified.
    for (system, text) in [("\\u0073ystem", "text"), ("system", "\\u0074ext")] {
        let body = format!(
            r#"{{"model":"claude-sonnet-4-5","{system}":[{{"type":"text","{text}":"{UNCHECKED_LINE}"}}],"messages":[],"max_tokens":8,"metadata":{{"user_id":"{{\"device_id\":\"test-device\",\"account_uuid\":\"\",\"session_id\":\"s1\"}}"}}}}"#
        );
        let (status, code, text) = send(&h, "/v1/messages", &body).await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(code, "body_shape", "{text}");
        assert!(
            text.contains("billing stamp the relay can neither check nor restamp"),
            "{text}"
        );
    }
    assert!(up.requests().is_empty(), "nothing reached the upstream");
    // The raw spelling with the placeholder still travels: the placeholder claims no hash.
    let plain = format!(
        r#"{{"model":"claude-sonnet-4-5","system":[{{"type":"text","text":"{BARE_PLACEHOLDER_LINE}"}}],"messages":[],"max_tokens":8,"metadata":{{"user_id":"{{\"device_id\":\"test-device\",\"account_uuid\":\"\",\"session_id\":\"s1\"}}"}}}}"#
    );
    let (status, _, text) = send(&h, "/v1/messages", &plain).await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(up.requests().len(), 1);
    up.shutdown().await;
}

#[tokio::test]
async fn a_lone_surrogate_beside_an_escaped_stamp_token_is_refused_before_anything_is_sent() {
    let up = FakeUpstream::start(vec![ok_rule()]).await;
    let h = harness_restricted_to(&up, "claude-sonnet-4-5").await;
    // A strict decoder refuses each line for its lone surrogate, and the raw walk finds no stamp in it.
    for line in [
        r"x-anthropic-billing-header: v=1 \udc00 cch\u003d01234;",
        r"x-anthropic-billing-header\u003a v=1 \udc00 cch=01234;",
    ] {
        let body = format!(
            r#"{{"model":"claude-sonnet-4-5","system":[{{"type":"text","text":"{line}"}}],"messages":[],"max_tokens":8,"metadata":{{"user_id":"{{\"device_id\":\"test-device\",\"account_uuid\":\"\",\"session_id\":\"s1\"}}"}}}}"#
        );
        let (status, code, text) = send(&h, "/v1/messages", &body).await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(code, "body_shape", "{text}");
        assert!(
            text.contains("billing stamp the relay can neither check nor restamp"),
            "{text}"
        );
    }
    assert!(up.requests().is_empty(), "nothing reached the upstream");
    up.shutdown().await;
}
