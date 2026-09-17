//! The recipe builder's shape checks, the TTL evidence rules, and usage parsing; the Go recipe and usage suites.
use phoebis_keepalive::{
    make_recipe, request_ttl_ms, ProbeUsage, FIVE_MINUTES_MS, MAX_USAGE_BYTES, ONE_HOUR_MS,
};
use serde_json::{json, Value};

const BODY: &str = r#"{"model":"routing-alias","max_tokens":1024,"stream":true,"thinking":{"type":"adaptive"},"system":[{"type":"text","text":"private prompt","cache_control":{"type":"ephemeral"}}],"messages":[{"role":"user","content":"hello"}]}"#;

fn headers() -> Vec<(String, String)> {
    vec![
        ("anthropic-beta".into(), "test-beta".into()),
        ("x-claude-code-agent-id".into(), "main".into()),
        ("x-claude-code-session-id".into(), "session".into()),
    ]
}

fn with(path: &str, value: Value) -> Vec<u8> {
    let mut root: Value = serde_json::from_str(BODY).unwrap();
    let mut at = &mut root;
    let parts: Vec<&str> = path.split('.').collect();
    for key in &parts[..parts.len() - 1] {
        at = at.get_mut(*key).unwrap();
    }
    at[parts[parts.len() - 1]] = value;
    serde_json::to_vec(&root).unwrap()
}

#[test]
fn a_replay_changes_only_the_output_controls() {
    let body = br#"{ "model":"alias", "max_tokens":500, "stream":true, "thinking":{"type":"adaptive"}, "output_config":{"effort":"high"}, "system":[{"type":"text","text":"a < b","cache_control":{"type":"ephemeral","ttl":"1h"}}], "tools":[{"name":"example","input_schema":{"type":"object"}}], "messages":[{"role":"user","content":"hello"}] }"#;
    let recipe = make_recipe(&headers(), body, "alias", 1 << 20).unwrap();
    let original: Value = serde_json::from_slice(body).unwrap();
    let result: Value = serde_json::from_slice(&recipe.body).unwrap();
    for (key, value) in original.as_object().unwrap() {
        if key == "max_tokens" || key == "stream" {
            continue;
        }
        assert_eq!(&result[key], value, "changed {key}");
    }
    assert_eq!(
        (result["max_tokens"].as_u64(), result["stream"].as_bool()),
        (Some(0), Some(false))
    );
    assert!(
        recipe
            .body
            .starts_with(b"{ \"model\":\"alias\", \"max_tokens\":0,"),
        "the edit is in place, not a re-serialization"
    );
    assert_eq!(
        (recipe.ttl_ms, recipe.model.as_str()),
        (ONE_HOUR_MS, "alias")
    );
    assert_eq!(
        recipe.headers,
        headers(),
        "every header the bus published, in its order"
    );
    assert_eq!(
        recipe.size,
        recipe.body.len()
            + "alias".len()
            + headers()
                .iter()
                .map(|(n, v)| n.len() + v.len())
                .sum::<usize>()
    );
}

#[test]
fn a_body_without_a_stream_flag_gains_one_and_a_stamped_body_is_resigned() {
    let body = BODY.replace(r#""stream":true,"#, "");
    let recipe = make_recipe(&headers(), body.as_bytes(), "routing-alias", 1 << 20).unwrap();
    let result: Value = serde_json::from_slice(&recipe.body).unwrap();
    assert_eq!(result["stream"].as_bool(), Some(false));
    // The first-party fixture carries a billing hash over a view that includes `stream`, so the edited body
    // must be re-signed or the relay refuses it as drift.
    let stamped = phoebis_profile::fixtures::body(phoebis_profile::fixtures::BodyFixture::FirstParty9b244);
    let adaptive = String::from_utf8(stamped.to_vec()).unwrap().replacen(
        r#""thinking":{"budget_tokens":31999,"type":"enabled","display":"omitted"}"#,
        r#""thinking":{"type":"adaptive"}"#,
        1,
    );
    let recipe = make_recipe(
        &headers(),
        adaptive.as_bytes(),
        "claude-sonnet-4-5",
        8 << 20,
    )
    .unwrap();
    assert_eq!(
        recipe.body,
        phoebis_cch::sign(&recipe.body).unwrap(),
        "the stamp matches the edited body"
    );
    assert_ne!(
        phoebis_cch::find_cch(&recipe.body)
            .map(|l| recipe.body[l.digits_start..l.digits_start + 5].to_vec()),
        Some(b"9b244".to_vec()),
        "and differs from the client's stamp over the unedited body"
    );
}

#[test]
fn unsupported_shapes_are_named() {
    for (path, value, reason) in [
        ("messages", json!([]), "unsupported_shape"),
        ("max_tokens", json!(0), "unsupported_shape"),
        (
            "output_config",
            json!({"format": {"type": "json_schema"}}),
            "structured_output",
        ),
        (
            "thinking",
            json!({"type": "enabled", "budget_tokens": 1024}),
            "fixed_thinking",
        ),
        ("thinking", json!("adaptive"), "unsupported_shape"),
        ("tool_choice", json!({"type": "any"}), "forced_tool"),
        ("tool_choice", json!({"type": "auto"}), ""),
        (
            "output_format",
            json!({"type": "json"}),
            "structured_output",
        ),
        (
            "tools",
            json!([{"name": "t", "strict": true}]),
            "structured_output",
        ),
        (
            "system",
            json!([{"type": "text", "text": "x", "cache_control": {"type": "persistent"}}]),
            "invalid_cache_control",
        ),
        (
            "system",
            json!([{"type": "text", "text": "x", "cache_control": {"type": "ephemeral", "ttl": "2h"}}]),
            "invalid_cache_control",
        ),
    ] {
        let result = make_recipe(&headers(), &with(path, value), "routing-alias", 1 << 20);
        match reason {
            "" => assert!(result.is_ok(), "{path} accepted"),
            reason => assert_eq!(result.unwrap_err(), reason, "{path}"),
        }
    }
    assert_eq!(
        make_recipe(&headers(), &vec![b'x'; 1_025], "m", 1_024).unwrap_err(),
        "request_too_large"
    );
    assert_eq!(
        make_recipe(&headers(), b"{", "m", 1 << 20).unwrap_err(),
        "invalid_request"
    );
    assert_eq!(
        make_recipe(&headers(), b"[]", "m", 1 << 20).unwrap_err(),
        "invalid_request"
    );
    assert_eq!(
        make_recipe(&headers(), BODY.as_bytes(), "", 1 << 20).unwrap_err(),
        "invalid_model"
    );
    assert_eq!(
        make_recipe(&headers(), BODY.as_bytes(), "bad\u{7}model", 1 << 20).unwrap_err(),
        "invalid_model"
    );
    let long = vec![("anthropic-beta".to_string(), "x".repeat(8_193))];
    assert_eq!(
        make_recipe(&long, BODY.as_bytes(), "routing-alias", 1 << 20).unwrap_err(),
        "headers_too_large"
    );
    let many: Vec<(String, String)> = (0..3)
        .map(|i| {
            (
                format!("x-stainless-{}", ["arch", "lang", "os"][i]),
                "y".repeat(8_000),
            )
        })
        .collect();
    assert_eq!(
        make_recipe(&many, BODY.as_bytes(), "routing-alias", 1 << 20).unwrap_err(),
        "headers_too_large"
    );
    // The model's length is part of the budget: 16,375 header bytes pass under a one-byte model and fail under
    // a fifteen-byte one.
    let near: Vec<(String, String)> = vec![
        ("anthropic-beta".to_string(), "b".repeat(8_000)),
        ("user-agent".to_string(), "u".repeat(8_000)),
        ("x-app".to_string(), "a".repeat(346)),
    ];
    assert_eq!(
        near.iter().map(|(n, v)| n.len() + v.len()).sum::<usize>(),
        16_375
    );
    assert!(make_recipe(&near, BODY.as_bytes(), "m", 1 << 20).is_ok());
    assert_eq!(
        make_recipe(&near, BODY.as_bytes(), "routing-alias-x", 1 << 20).unwrap_err(),
        "headers_too_large"
    );
    // The bus denylist is the one gate: a recipe replays what the bus published, so the credential, the
    // forwarding headers, and the relay's own framing never reach one in the first place.
    for name in [
        "accept-encoding",
        "authorization",
        "connection",
        "content-length",
        "cookie",
        "host",
        "transfer-encoding",
        "x-api-key",
        "x-forwarded-for",
        "x-keepalive-owner-id",
    ] {
        assert!(!phoebis_bus::header_published(name), "{name}");
    }
    let unknown = vec![("x-claude-code-future".to_string(), "1".to_string())];
    assert_eq!(
        make_recipe(&unknown, BODY.as_bytes(), "routing-alias", 1 << 20)
            .unwrap()
            .headers,
        unknown,
        "a header this code has never seen is replayed as the caller sent it"
    );
}

#[test]
fn duplicate_output_controls_are_rejected() {
    let body =
        br#"{"max_tokens":1,"max_tokens":10000,"messages":[{"role":"user","content":"test"}]}"#;
    assert_eq!(
        make_recipe(&headers(), body, "m", 1 << 20).unwrap_err(),
        "invalid_request",
        "an ambiguous output limit"
    );
    let body = br#"{"max_tokens":1,"thinking":{"type":"adaptive","type":"enabled"},"messages":[{"role":"user","content":"test"}]}"#;
    assert_eq!(
        make_recipe(&headers(), body, "m", 1 << 20).unwrap_err(),
        "invalid_request",
        "an ambiguous thinking block"
    );
    let body = br#"{"max_tokens":1,"cache_control":{"type":"ephemeral","type":"ephemeral"},"messages":[{"role":"user","content":"test"}]}"#;
    assert_eq!(
        make_recipe(&headers(), body, "m", 1 << 20).unwrap_err(),
        "invalid_request",
        "a repeated key inside a nested object"
    );
}

#[test]
fn a_routing_alias_with_a_thinking_suffix_is_preserved() {
    let recipe = make_recipe(&headers(), BODY.as_bytes(), "routing-alias(high)", 1 << 20).unwrap();
    assert_eq!(recipe.model, "routing-alias(high)");
}

#[test]
fn ttl_is_read_only_evidence_and_mixed_markers_take_the_shorter() {
    for (name, usage, prior, requested, want) in [
        (
            "default",
            ProbeUsage {
                read: 100,
                ..Default::default()
            },
            0,
            0,
            FIVE_MINUTES_MS,
        ),
        (
            "mixed",
            ProbeUsage {
                five_minutes: 1,
                one_hour: 100,
                ..Default::default()
            },
            ONE_HOUR_MS,
            0,
            FIVE_MINUTES_MS,
        ),
        (
            "unreported_short_reads",
            ProbeUsage {
                read: 2_000,
                one_hour: 100,
                ..Default::default()
            },
            FIVE_MINUTES_MS,
            0,
            FIVE_MINUTES_MS,
        ),
        (
            "read_only",
            ProbeUsage {
                read: 100,
                ..Default::default()
            },
            ONE_HOUR_MS,
            0,
            ONE_HOUR_MS,
        ),
        (
            "shorter_request",
            ProbeUsage {
                read: 100,
                ..Default::default()
            },
            ONE_HOUR_MS,
            FIVE_MINUTES_MS,
            FIVE_MINUTES_MS,
        ),
    ] {
        assert_eq!(usage.ttl_ms(prior, requested), want, "{name}");
    }
    let mut body = with(
        "system",
        json!([{"type": "text", "text": "p", "cache_control": {"type": "ephemeral", "ttl": "1h"}}]),
    );
    let mut root: Value = serde_json::from_slice(&body).unwrap();
    root["messages"][0]["content"] =
        json!([{"type": "text", "text": "test", "cache_control": {"type": "ephemeral"}}]);
    body = serde_json::to_vec(&root).unwrap();
    assert_eq!(
        make_recipe(&headers(), &body, "m", 1 << 20).unwrap().ttl_ms,
        FIVE_MINUTES_MS,
        "mixed markers"
    );
    assert_eq!(
        request_ttl_ms(&serde_json::from_str::<Value>(r#"{"messages":[]}"#).unwrap()),
        Some(0),
        "no marker requests nothing"
    );
    assert_eq!(
        request_ttl_ms(
            &json!({"cache_control": {"type": "ephemeral", "ttl": "1h"}, "tools": [{"cache_control": {"type": "ephemeral", "ttl": "1h"}}]})
        ),
        Some(ONE_HOUR_MS)
    );
}

#[test]
fn malformed_usage_is_refused_and_the_bus_shape_is_read() {
    for body in [
        r#"{"#,
        r#"{"usage":{"cache_read_input_tokens":-1}}"#,
        r#"{"usage":{"cache_read_input_tokens":"100"}}"#,
        r#"{"usage":{"output_tokens":1.5}}"#,
        r#"{"usage":{"output_tokens":1e13}}"#,
        r#"{"usage":[]}"#,
        r#"{}"#,
    ] {
        assert!(
            ProbeUsage::parse(body.as_bytes()).is_none(),
            "accepted invalid usage: {body}"
        );
    }
    let parsed = ProbeUsage::parse(br#"{"type":"message_start","message":{"usage":{"cache_creation_input_tokens":2000,"cache_creation":{"ephemeral_5m_input_tokens":2000},"output_tokens":10}}}"#).unwrap();
    assert_eq!(
        parsed,
        ProbeUsage {
            read: 0,
            write: 2_000,
            output: 10,
            five_minutes: 2_000,
            one_hour: 0,
            input: 0
        }
    );
    let split = ProbeUsage::parse(br#"{"usage":{"cache_read_input_tokens":5,"cache_creation":{"ephemeral_1h_input_tokens":7}}}"#).unwrap();
    assert_eq!(
        (split.read, split.write, split.one_hour, split.evidence()),
        (5, 7, 7, 12),
        "write falls back to the split sum"
    );
    let oversized = format!(
        r#"{{"usage":{{"output_tokens":1}},"pad":"{}"}}"#,
        "x".repeat(MAX_USAGE_BYTES)
    );
    assert!(ProbeUsage::parse(oversized.as_bytes()).is_none());
    let bus = phoebis_bus::Usage {
        input_tokens: Some(4),
        cache_read_tokens: Some(2),
        cache_write_5m_tokens: Some(3),
        cache_write_1h_tokens: None,
        output_tokens: Some(7),
        ..Default::default()
    };
    assert_eq!(
        ProbeUsage::from_bus(&bus),
        ProbeUsage {
            read: 2,
            write: 3,
            output: 7,
            five_minutes: 3,
            one_hour: 0,
            input: 4
        }
    );
}
