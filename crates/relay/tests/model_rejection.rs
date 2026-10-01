//! A 404 `not_found_error` naming the wire model: the account cools for that model alone, the request fails
//! over, and everything else a 404 or a 400/422 can say reaches the client as it came.
mod common;
use std::time::Duration;

use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule};
use http_body_util::BodyExt;
use phoebis_bus::{Bus, Event};
use phoebis_relay::Limits;
use tower::ServiceExt;

const A: &str = "sk-ant-oat01-AAAA";
const B: &str = "sk-ant-oat01-BBBB";

fn status_and_body(r: &Respond) -> (u16, Vec<u8>) {
    match r {
        Respond::Json { status, body, .. } => (*status, body.clone()),
        _ => panic!("expected a Json respond"),
    }
}

#[tokio::test]
async fn a_404_naming_the_model_cools_the_pair_and_fails_over() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    up.add_credential_rule(A, "/v1/messages", json_404_model("claude-sonnet-4-5"));
    // "a"'s rule only has to answer the rejection once: after that, an eligible request (a different model,
    // once this one has cooled "a" for it) reaches "a" and succeeds.
    up.set_credential_rule_after(A, 1, "/v1/messages", json_200());
    let h = harness(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec::default(),
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let o = outcome(&resp);
    assert_eq!((o.account_id.as_str(), o.attempt), ("b", 2));
    assert_eq!(credentials_seen(&up), [A, B]);
    let cooldowns = wait_for_async(Duration::from_secs(2), || async {
        let rows = h.router.cooldowns().await.unwrap();
        rows.iter()
            .any(|c| c.scope == "model:claude-sonnet-4-5-20250929")
            .then_some(rows)
    })
    .await;
    assert_eq!(cooldowns.len(), 1);
    assert_eq!(
        (
            cooldowns[0].account_id.as_str(),
            cooldowns[0].scope.as_str(),
            cooldowns[0].until,
            cooldowns[0].window.as_str()
        ),
        (
            "a",
            "model:claude-sonnet-4-5-20250929",
            4_601_000,
            "model_rejection"
        )
    );
    assert_eq!(cooldowns[0].reason, "model_rejected");
    // A second request for the same model finds "a" already cooling and goes to "b" alone.
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s2"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let o = outcome(&resp);
    assert_eq!((o.account_id.as_str(), o.attempt), ("b", 1));
    // "a" only cooled for this one model; a request for another still reaches it.
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-opus-5", "s3"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(outcome(&resp).account_id, "a");
    up.shutdown().await;
}

#[tokio::test]
async fn a_rejection_head_publishes_the_cooldown_reason_on_the_bus() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    up.add_credential_rule(A, "/v1/messages", json_404_model("claude-sonnet-4-5"));
    let bus = Bus::new();
    let mut sub = bus.subscribe("test");
    let h = harness_with_bus(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec::default(),
        bus,
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let events = drain_until_complete(&mut sub).await;
    let head1 = events
        .iter()
        .find_map(|e| match e {
            Event::ResponseHead(h) if h.attempt == 1 => Some(h),
            _ => None,
        })
        .expect("attempt 1's head");
    assert_eq!(
        head1.cooldown.as_ref().map(|c| c.reason.as_str()),
        Some("model_rejected")
    );
    let complete = events
        .iter()
        .find_map(|e| match e {
            Event::RequestComplete(c) => Some(c),
            _ => None,
        })
        .expect("the completion");
    assert_eq!(complete.account_id.as_deref(), Some("b"));
    up.shutdown().await;
}

#[tokio::test]
async fn a_404_naming_no_model_or_another_model_a_400_and_a_422_reach_the_client_as_they_came() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let cases = [
        json_404_plain(),
        // The same-prefix pair: the request names "claude-sonnet-4-5" but the body names the longer id, which
        // is not the same whole token.
        json_404_model("claude-sonnet-4-5-1"),
        Respond::Json {
            status: 400,
            headers: vec![],
            body: br#"{"type":"error","error":{"type":"invalid_request_error","message":"bad request"},"request_id":"req_fake"}"#.to_vec(),
        },
        Respond::Json {
            status: 422,
            headers: vec![],
            body: br#"{"type":"error","error":{"type":"invalid_request_error","message":"unprocessable"},"request_id":"req_fake"}"#.to_vec(),
        },
    ];
    for (n, respond) in cases.into_iter().enumerate() {
        let (status, body) = status_and_body(&respond);
        up.push_credential_rule(A, "/v1/messages", respond);
        let resp = h
            .app
            .clone()
            .oneshot(request_for(KEY, "claude-sonnet-4-5", &format!("s{n}")))
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), status, "case {n}");
        let got = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(got.as_ref(), body.as_slice(), "case {n}");
        assert_eq!(up.requests().len(), n + 1);
    }
    assert!(h.router.cooldowns().await.unwrap().is_empty());
    up.shutdown().await;
}

#[tokio::test]
async fn a_rejection_on_the_last_attempt_answers_404_model_rejected() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    up.add_credential_rule(A, "/v1/messages", json_404_model("claude-sonnet-4-5"));
    let h = harness_with_limits(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
        Limits {
            max_attempts: 1,
            ..default_limits()
        },
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(
        (
            resp.status().as_u16(),
            resp.headers()
                .get("x-phoebis-error-code")
                .unwrap()
                .to_str()
                .unwrap()
        ),
        (404, "model_rejected")
    );
    let body: serde_json::Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(
        body["error"]["message"],
        "every account in this pool rejected the model"
    );
    assert!(!h.router.cooldowns().await.unwrap().is_empty());
    up.shutdown().await;

    // Two accounts, both rejecting: the budget spends itself failing over between them, and every member's
    // cooldown row still lands.
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    up.add_credential_rule(A, "/v1/messages", json_404_model("claude-sonnet-4-5"));
    up.add_credential_rule(B, "/v1/messages", json_404_model("claude-sonnet-4-5"));
    let h = harness(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec::default(),
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(
        (
            resp.status().as_u16(),
            resp.headers()
                .get("x-phoebis-error-code")
                .unwrap()
                .to_str()
                .unwrap()
        ),
        (404, "model_rejected")
    );
    let cooldowns = h.router.cooldowns().await.unwrap();
    assert_eq!(cooldowns.len(), 2);
    up.shutdown().await;
}

#[tokio::test]
async fn a_non_portable_request_keeps_the_upstreams_404_and_records_the_cooldown() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec::default(),
    )
    .await;
    let pinning = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(pinning.status(), 200);
    assert_eq!(outcome(&pinning).account_id, "a");
    let rejection = json_404_model("claude-sonnet-4-5");
    let (expected_status, expected_body) = status_and_body(&rejection);
    up.add_credential_rule(A, "/v1/messages", rejection);
    let resp = h
        .app
        .clone()
        .oneshot(non_portable_request(KEY, "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), expected_status);
    assert_ne!(
        resp.headers()
            .get("x-phoebis-error-code")
            .map(|v| v.to_str().unwrap()),
        Some("model_rejected"),
        "the upstream's own answer carries no local error code"
    );
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/json"
    );
    assert_eq!(resp.headers().get("request-id").unwrap(), "req_fake");
    let got_body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(got_body.as_ref(), expected_body.as_slice());
    assert_eq!(credentials_seen(&up), [A, A]);
    let cooldowns = wait_for_async(Duration::from_secs(2), || async {
        let rows = h.router.cooldowns().await.unwrap();
        rows.iter()
            .any(|c| c.account_id == "a" && c.scope == "model:claude-sonnet-4-5-20250929")
            .then_some(rows)
    })
    .await;
    assert!(cooldowns.iter().any(|c| c.account_id == "a"
        && c.scope == "model:claude-sonnet-4-5-20250929"
        && c.reason == "model_rejected"));
    let pins = h.router.pins().await.unwrap();
    assert_eq!(
        (pins.len(), pins[0].account_id.as_str()),
        (1, "a"),
        "the pin is kept"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_suffix_is_folded_before_the_cooldown_lookup_and_absent_from_the_wire() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec::default(),
    )
    .await;
    h.router.report(phoebis_routing::Report::Cooldown {
        account_id: "a".into(),
        row: phoebis_store::CooldownRow {
            account_id: "a".into(),
            scope: "model:claude-sonnet-4-5".into(),
            until: NOW + 3_600_000,
            set_at: 0,
            reason: phoebis_routing::MODEL_REJECTED_REASON.into(),
            window: "model_rejection".into(),
            source_request_id: None,
            incarnation: 1,
        },
        pin: None,
        lifecycle_generation: 1,
        credential_generation: 1,
        incarnation: 1,
    });
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5[1m]", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(outcome(&resp).account_id, "b");
    let requests = up.requests();
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["model"], "claude-sonnet-4-5");
    up.shutdown().await;
}
