mod common;
use std::time::Duration;

use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule};
use http_body_util::BodyExt;
use phoebis_bus::{Bus, Delivery};
use phoebis_ledger::{Assembler, Record};
use phoebis_store::AccountState;
use tower::ServiceExt;

const A: &str = "sk-ant-oat01-AAAA";
const B: &str = "sk-ant-oat01-BBBB";
const C: &str = "sk-ant-oat01-CCCC";
const RESET: i64 = 4_600; // one hour past NOW (1_000 s), in Unix seconds

async fn two_accounts(up: &FakeUpstream) -> Harness {
    harness(
        up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec::default(),
    )
    .await
}

#[tokio::test]
async fn a_429_migrates_the_session_and_it_stays_on_the_replacement_after_recovery() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    up.add_credential_rule(A, "/v1/messages", json_429_unified(RESET, "five_hour"));
    let h = two_accounts(&up).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let o = outcome(&resp);
    assert_eq!(
        (o.account_id.as_str(), o.attempt, o.error_code),
        ("b", 2, None)
    );
    assert_eq!(credentials_seen(&up), [A, B]);
    let cooldowns = h.router.cooldowns().await.unwrap();
    assert_eq!(
        (
            cooldowns[0].account_id.as_str(),
            cooldowns[0].scope.as_str(),
            cooldowns[0].until,
            cooldowns[0].window.as_str()
        ),
        ("a", "account", RESET * 1_000 + 1_000, "five_hour")
    );
    up.set_credential_rule_after(A, 1, "/v1/messages", json_200());
    h.router.clear_cooldown("a", "account").await.unwrap();
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(
        outcome(&resp).account_id,
        "b",
        "the pin stays on the replacement after recovery"
    );
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s2"))
        .await
        .unwrap();
    assert_eq!(
        outcome(&resp).account_id,
        "a",
        "a new session goes to priority 0 again"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_non_portable_request_gets_the_upstream_429_and_keeps_its_pin() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = two_accounts(&up).await;
    assert_eq!(
        outcome(
            &h.app
                .clone()
                .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
                .await
                .unwrap()
        )
        .account_id,
        "a"
    );
    up.add_credential_rule(A, "/v1/messages", json_429_unified(RESET, "five_hour"));
    let resp = h
        .app
        .clone()
        .oneshot(non_portable_request(KEY, "s1"))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        429,
        "the upstream's own rejection comes back"
    );
    assert_eq!(
        resp.headers()
            .get("anthropic-ratelimit-unified-status")
            .unwrap(),
        "rejected"
    );
    assert_eq!(credentials_seen(&up), [A, A], "no other account was tried");
    let pins = h.router.pins().await.unwrap();
    assert_eq!(
        (pins.len(), pins[0].account_id.as_str()),
        (1, "a"),
        "the pin is kept"
    );
    assert_eq!(h.router.cooldowns().await.unwrap()[0].account_id, "a");
    let resp = h
        .app
        .clone()
        .oneshot(non_portable_request(KEY, "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
    assert_eq!(
        resp.headers().get("x-phoebis-error-code").unwrap(),
        "accounts_exhausted",
        "fenced to the cooled account: the exhaustion for that account and no upstream call"
    );
    assert_eq!(credentials_seen(&up).len(), 2);
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(
        (resp.status().as_u16(), outcome(&resp).account_id.as_str()),
        (200, "b"),
        "the same session's portable request migrates"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn every_eligible_account_is_tried_once_and_then_the_exhaustion_answers() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_429_unified(RESET, "five_hour"),
    }])
    .await;
    let h = harness(
        &up,
        &[
            oauth("a", A, 0),
            oauth("b", B, 0),
            oauth("c", C, 0),
            oauth("d", "sk-ant-oat01-DDDD", 0),
        ],
        PoolSpec::default(),
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
    assert_eq!(outcome(&resp).attempt, 4);
    assert_eq!(
        credentials_seen(&up).len(),
        4,
        "four accounts, each dialled once"
    );
    assert_eq!(
        (
            resp.headers()["x-phoebis-error-code"].to_str().unwrap(),
            resp.headers()["retry-after"].to_str().unwrap()
        ),
        ("accounts_exhausted", "3601"),
        "every account cooled, so the router answers with the earliest deadline"
    );
    // The whole pool is cooling, so the next request dials nothing.
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s2"))
        .await
        .unwrap();
    assert_eq!(credentials_seen(&up).len(), 4);
    assert_eq!(resp.status(), 429);
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s3"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
    assert_eq!(
        resp.headers().get("x-phoebis-error-code").unwrap(),
        "accounts_exhausted"
    );
    assert_eq!(resp.headers().get("retry-after").unwrap(), "3601");
    assert_eq!(
        resp.headers()
            .get("anthropic-ratelimit-unified-status")
            .unwrap(),
        "rejected"
    );
    assert_eq!(
        resp.headers()
            .get("anthropic-ratelimit-unified-reset")
            .unwrap(),
        "4601"
    );
    assert_eq!(
        resp.headers()
            .get("anthropic-ratelimit-unified-representative-claim")
            .unwrap(),
        "five_hour"
    );
    assert_eq!(
        credentials_seen(&up).len(),
        4,
        "no upstream call for an exhausted pool"
    );
    let body: serde_json::Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["error"]["type"], "rate_limit_error");
    for id in ["a", "b", "c", "d"] {
        h.router.clear_cooldown(id, "account").await.unwrap();
    }
    for (id, state) in [
        ("a", AccountState::ReauthRequired),
        ("b", AccountState::FundingRequired),
        ("c", AccountState::ReauthRequired),
        ("d", AccountState::FundingRequired),
    ] {
        h.store.set_state(id, state).unwrap();
    }
    h.router.reload().await.unwrap();
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s4"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
    assert_eq!(
        resp.headers().get("x-phoebis-error-code").unwrap(),
        "accounts_unavailable"
    );
    assert!(
        resp.headers().get("retry-after").is_none()
            && resp
                .headers()
                .get("anthropic-ratelimit-unified-status")
                .is_none()
    );
    h.store.set_state("d", AccountState::Ready).unwrap();
    h.router.reload().await.unwrap();
    h.router.report(phoebis_routing::Report::Cooldown {
        account_id: "d".into(),
        row: phoebis_store::CooldownRow {
            account_id: "d".into(),
            scope: "account".into(),
            until: NOW + 90_000,
            set_at: 0,
            reason: "rate_limit".into(),
            window: "seven_day".into(),
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
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s5"))
        .await
        .unwrap();
    assert_eq!(
        (
            resp.status().as_u16(),
            resp.headers().get("retry-after").unwrap().to_str().unwrap()
        ),
        (429, "90"),
        "a mixed pool answers from the cooled account's deadline"
    );
    assert_eq!(
        resp.headers()
            .get("anthropic-ratelimit-unified-representative-claim")
            .unwrap(),
        "seven_day"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_wire_submission_index_ends_the_request_at_255_with_the_exhaustion_answer() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_429_unified(RESET, "five_hour"),
    }])
    .await;
    // `api_key` takes `'static` names, so the generated ones are leaked for the test's lifetime.
    let accounts: Vec<TestAccount> = (0..256)
        .map(|n| {
            let id: &'static str = Box::leak(format!("k{n:03}").into_boxed_str());
            let secret: &'static str =
                Box::leak(format!("sk-ant-api03-FAKE{n:03}").into_boxed_str());
            api_key(id, secret, 0)
        })
        .collect();
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(&up, &accounts, PoolSpec::default(), bus).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(
        (
            resp.status().as_u16(),
            resp.headers()["x-phoebis-error-code"].to_str().unwrap()
        ),
        (503, "accounts_unavailable"),
        "the index's own bound is answered as the pool's untimed exhaustion"
    );
    assert_eq!(outcome(&resp).attempt, 255);
    assert_eq!(
        credentials_seen(&up).len(),
        255,
        "the 256th account is never dialled"
    );
    let events = drain_until_complete(&mut sub).await;
    let mut assembler = Assembler::new(std::sync::Arc::new(|| 0));
    let records: Vec<Record> = events
        .into_iter()
        .flat_map(|e| assembler.observe(e))
        .collect();
    let request = records
        .iter()
        .find_map(|r| match r {
            Record::Request { request, .. } => Some(request),
            _ => None,
        })
        .expect("the request lands a ledger row");
    assert_eq!(request.outcome, "exhausted");
    up.shutdown().await;
}

#[tokio::test]
async fn a_model_outside_the_rules_is_refused_and_any_other_reaches_the_upstream() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec {
            model_rules: rules(&["claude-opus-5", "NOT *"]),
            ..PoolSpec::default()
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
        (404, "model_not_allowed")
    );
    let body: serde_json::Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["error"]["message"], "model not allowed for this key");
    assert!(up.requests().is_empty());
    let open = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let resp = open
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-haiku-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let requests = up.requests();
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["model"], "claude-haiku-4-5");
    up.shutdown().await;
}

#[tokio::test]
async fn every_member_cooled_for_a_rejection_answers_404_model_rejected_without_an_upstream_call() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
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
    assert!(up.requests().is_empty());
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-opus-5", "s2"))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    up.shutdown().await;
}

#[tokio::test]
async fn a_rejection_at_the_pools_end_records_a_rejected_request_and_an_upstream_error_attempt() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    up.add_credential_rule(A, "/v1/messages", json_404_model("claude-sonnet-4-5"));
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(&up, &[oauth("a", A, 0)], PoolSpec::default(), bus).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(
        resp.headers().get("x-phoebis-error-code").unwrap(),
        "model_rejected",
        "every account rejected the model, so the router answers its own 404"
    );
    let events = drain_until_complete(&mut sub).await;
    let mut assembler = Assembler::new(std::sync::Arc::new(|| 0));
    let records: Vec<Record> = events
        .into_iter()
        .flat_map(|e| assembler.observe(e))
        .collect();
    let (request, attempts) = records
        .iter()
        .find_map(|r| match r {
            Record::Request {
                request, attempts, ..
            } => Some((request, attempts)),
            _ => None,
        })
        .expect("the request lands a ledger row");
    assert_eq!(
        request.outcome, "rejected",
        "a model rejection is a refusal, not a transport or upstream failure"
    );
    assert_eq!(
        attempts.last().expect("the rejected attempt").outcome,
        "upstream_error",
        "the attempt itself still reads as the classification the head gave it"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_permission_error_cools_one_model_for_an_hour_and_leaves_the_credential_usable() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    up.add_credential_rule(
        A,
        "/v1/messages",
        Respond::Json {
            status: 403,
            headers: vec![],
            body:
                br#"{"type":"error","error":{"type":"permission_error","message":"not entitled"}}"#
                    .to_vec(),
        },
    );
    up.set_credential_rule_after(A, 1, "/v1/messages", json_200());
    let h = two_accounts(&up).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(
        (resp.status().as_u16(), outcome(&resp).account_id.as_str()),
        (200, "b")
    );
    let cooldowns = wait_for_async(Duration::from_secs(2), || async {
        let rows = h.router.cooldowns().await.unwrap();
        rows.iter()
            .any(|c| c.scope == "model:claude-sonnet-4-5-20250929")
            .then_some(rows)
    })
    .await;
    assert_eq!(
        (
            cooldowns[0].scope.as_str(),
            cooldowns[0].reason.as_str(),
            cooldowns[0].until
        ),
        (
            "model:claude-sonnet-4-5-20250929",
            "permission",
            NOW + 3_600_000 + 1_000
        )
    );
    assert_eq!(
        h.store.account("a").unwrap().unwrap().state,
        AccountState::Ready
    );
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-opus-5", "s2"))
        .await
        .unwrap();
    assert_eq!(
        outcome(&resp).account_id,
        "a",
        "another model on the same account is still served"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn cooldowns_are_partitioned_by_account_kind() {
    const K1: &str = "sk-ant-api03-KEY1";
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    up.add_credential_rule(
        K1,
        "/v1/messages",
        Respond::Json {
            status: 429,
            headers: vec![("retry-after".into(), "30".into())],
            body: b"{}".to_vec(),
        },
    );
    up.set_credential_rule_after(K1, 1, "/v1/messages", json_200());
    up.add_credential_rule(
        B,
        "/v1/messages",
        Respond::Json {
            status: 429,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    );
    let h = harness(
        &up,
        &[api_key("k1", K1, 0), oauth("b", B, 1), oauth("c", C, 2)],
        PoolSpec::default(),
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(outcome(&resp).account_id, "c", "k1 and b both failed over");
    let c = wait_for_async(Duration::from_secs(2), || async {
        let rows = h.router.cooldowns().await.unwrap();
        rows.iter()
            .any(|row| row.account_id == "k1" && row.scope == "model:claude-sonnet-4-5-20250929")
            .then_some(rows)
    })
    .await;
    let k1 = c.iter().find(|c| c.account_id == "k1").unwrap();
    assert_eq!(
        (k1.scope.as_str(), k1.until),
        ("model:claude-sonnet-4-5-20250929", NOW + 30_000 + 1_000),
        "an api_key 429 with Retry-After cools the model"
    );
    assert!(
        c.iter()
            .any(|c| c.account_id == "b" && c.scope == "account"),
        "an oauth 429 without unified headers cools the account"
    );
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-opus-5", "s2"))
        .await
        .unwrap();
    assert_eq!(
        outcome(&resp).account_id,
        "k1",
        "only one model was cooled on the api_key account"
    );
    up.set_credential_rule_after(
        K1,
        2,
        "/v1/messages",
        Respond::Json {
            status: 429,
            headers: vec![("retry-after".into(), "soon".into())],
            body: b"{}".to_vec(),
        },
    );
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-opus-5", "s3"))
        .await
        .unwrap();
    assert_eq!(outcome(&resp).account_id, "c");
    let c = h.router.cooldowns().await.unwrap();
    assert!(
        c.iter().any(|c| c.account_id == "k1"
            && c.scope == "account"
            && c.until == NOW + 60_000 + 1_000),
        "a malformed Retry-After takes the account-wide fallback"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_fable_only_rejection_leaves_opus_eligible_and_a_5h_rejection_cools_every_model() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    up.add_credential_rule(
        A,
        "/v1/messages",
        Respond::Json {
            status: 429,
            headers: vec![
                (
                    "anthropic-ratelimit-unified-status".into(),
                    "rejected".into(),
                ),
                (
                    "anthropic-ratelimit-unified-representative-claim".into(),
                    "seven_day_fable".into(),
                ),
                (
                    "anthropic-ratelimit-unified-5h-status".into(),
                    "allowed".into(),
                ),
                (
                    "anthropic-ratelimit-unified-7d-status".into(),
                    "allowed".into(),
                ),
                (
                    "anthropic-ratelimit-unified-7d_oi-status".into(),
                    "rejected".into(),
                ),
                (
                    "anthropic-ratelimit-unified-7d_oi-reset".into(),
                    "90000".into(),
                ),
            ],
            body: b"{}".to_vec(),
        },
    );
    up.set_credential_rule_after(A, 1, "/v1/messages", json_200());
    let h = harness_with(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec::default(),
        default_limits(),
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-fable-5-1", "s1"))
        .await
        .unwrap();
    assert_eq!(
        (resp.status().as_u16(), outcome(&resp).account_id.as_str()),
        (200, "b")
    );
    let c = h.router.cooldowns().await.unwrap();
    assert_eq!(
        (c[0].scope.as_str(), c[0].window.as_str()),
        ("model:claude-fable-5-1", "seven_day_fable")
    );
    assert_eq!(
        outcome(
            &h.app
                .clone()
                .oneshot(request_for(KEY, "claude-opus-5", "s2"))
                .await
                .unwrap()
        )
        .account_id,
        "a",
        "opus on the same account is eligible"
    );
    up.set_credential_rule_after(A, 2, "/v1/messages", json_429_unified(RESET, "five_hour"));
    h.app
        .clone()
        .oneshot(request_for(KEY, "claude-opus-5", "s3"))
        .await
        .unwrap();
    assert_eq!(
        outcome(
            &h.app
                .clone()
                .oneshot(request_for(KEY, "claude-sonnet-4-5", "s4"))
                .await
                .unwrap()
        )
        .account_id,
        "b",
        "a 5h rejection cools every model"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_credit_balance_400_marks_funding_required_and_fails_over() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    up.add_credential_rule(
        "sk-ant-api03-KEY1",
        "/v1/messages",
        Respond::Json {
            status: 400,
            headers: vec![],
            body: br#"{"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API."}}"#.to_vec(),
        },
    );
    let h = harness(
        &up,
        &[api_key("k1", "sk-ant-api03-KEY1", 0), oauth("b", B, 1)],
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
        (resp.status().as_u16(), outcome(&resp).account_id.as_str()),
        (200, "b")
    );
    // The mark travels through the router task and its writer, so it lands after the client's response.
    wait_for(Duration::from_secs(2), || {
        h.store
            .account("k1")
            .unwrap()
            .filter(|row| row.state == AccountState::FundingRequired)
    })
    .await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_member_removed_under_a_non_portable_session_answers_that_accounts_exhaustion_and_reaches_no_other_account(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = two_accounts(&up).await;
    assert_eq!(
        outcome(
            &h.app
                .clone()
                .oneshot(non_portable_request(KEY, "s1"))
                .await
                .unwrap()
        )
        .account_id,
        "a"
    );
    h.store
        .set_pool_members("main", &[member("b", 1)], "test")
        .unwrap();
    h.router.reload().await.unwrap();
    let resp = h
        .app
        .clone()
        .oneshot(non_portable_request(KEY, "s1"))
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
        (503, "accounts_unavailable"),
        "the removed member's exhaustion"
    );
    assert_eq!(credentials_seen(&up), [A], "b was never reached");
    up.shutdown().await;
}

#[tokio::test]
async fn an_account_deleted_between_admission_and_selection_is_never_selected() {
    // The first attempt is gated so the deletion lands while the request is in flight; the failover that follows
    // re-selects against the registry as it is then, and `b` is gone from it.
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    up.add_credential_rule(
        A,
        "/v1/messages",
        Respond::Gated(Box::new(json_429_unified(RESET, "five_hour"))),
    );
    let h = harness(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1), oauth("c", C, 2)],
        PoolSpec::default(),
    )
    .await;
    let app = h.app.clone();
    let pending = tokio::spawn(async move {
        app.oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
            .await
            .unwrap()
    });
    wait_for(Duration::from_secs(2), || {
        credentials_seen(&up).contains(&A.to_string()).then_some(())
    })
    .await;
    // `b` is the account the stale candidate list would fail over to next, so deleting it while `a` holds the
    // gated request is what proves the second selection reads the registry as it is then.
    h.store.delete_account("b", "test").unwrap();
    h.router.reload().await.unwrap();
    up.release();
    let resp = pending.await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        outcome(&resp).account_id,
        "c",
        "a deleted account is never selected, whatever its priority"
    );
    assert!(
        !credentials_seen(&up).contains(&B.to_string()),
        "the deleted account was not tried"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn deleting_the_account_holding_an_in_flight_request_answers_account_cancelled_and_closes_the_upstream(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    up.add_credential_rule(A, "/v1/messages", Respond::Gated(Box::new(json_200())));
    let h = two_accounts(&up).await;
    let app = h.app.clone();
    let pending = tokio::spawn(async move {
        app.oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
            .await
            .unwrap()
    });
    wait_for(Duration::from_secs(2), || {
        credentials_seen(&up).contains(&A.to_string()).then_some(())
    })
    .await;
    h.store.delete_account("a", "test").unwrap();
    h.router.reload().await.unwrap();
    up.release();
    let resp = pending.await.unwrap();
    assert_eq!(resp.status(), 503);
    assert_eq!(
        resp.headers().get("x-phoebis-error-code").unwrap(),
        "account_cancelled"
    );
    assert_eq!(
        credentials_seen(&up),
        [A],
        "an answer about a credential that is gone is nothing to fail over from"
    );
    // The attempt's future owns the upstream connection, so cancelling it takes the socket with it.
    wait_for(Duration::from_secs(2), || {
        (up.open_connections() == 0).then_some(())
    })
    .await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_revoked_key_answers_401_at_once_and_a_newer_client_is_relayed_untagged() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = two_accounts(&up).await;
    let mut req = request_for(KEY, "claude-sonnet-4-5", "s1");
    set_header(&mut req, "user-agent", "claude-cli/2.1.300 (external, cli)");
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let o = outcome(&resp);
    assert_eq!(
        (o.client_drift.as_deref(), o.client_version.as_deref()),
        (None, Some("2.1.300")),
        "a version the profile never measured is read for the ledger and tagged as nothing"
    );
    assert!(
        up.requests()[0]
            .headers
            .iter()
            .any(|(n, v)| n == "User-Agent" && v == "claude-cli/2.1.300 (external, cli)"),
        "the caller's own agent reaches the upstream, spelling included"
    );
    h.store.revoke_api_key("desktop", NOW, "test").unwrap();
    h.router.reload().await.unwrap();
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    assert_eq!(credentials_seen(&up).len(), 1);
    up.shutdown().await;
}

#[tokio::test]
async fn a_secret_patched_during_admission_dispatches_with_the_new_secret() {
    const OLD_KEY: &str = "vendor-secret-old";
    const NEW_KEY: &str = "vendor-secret-new";
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Gated(Box::new(json_200())),
    }])
    .await;
    let h = harness_with(
        &up,
        &[api_key("vendor", OLD_KEY, 0)],
        PoolSpec::default(),
        default_limits(),
    )
    .await;
    let app = h.app.clone();
    let inflight = tokio::spawn(async move {
        app.oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
            .await
            .unwrap()
    });
    // The fake has the request, so it was admitted and selected against the registry as it stands now.
    wait_for(Duration::from_secs(5), || {
        (!up.requests().is_empty()).then_some(())
    })
    .await;
    h.store
        .set_api_key_secret("vendor", &phoebis_store::Secret::new(NEW_KEY))
        .unwrap();
    h.router.reload().await.unwrap();
    up.release();
    let resp = inflight.await.unwrap();
    assert_eq!(resp.status(), 200);
    resp.into_body().collect().await.unwrap();

    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.into_body().collect().await.unwrap();
    assert_eq!(
        credentials_seen(&up),
        [OLD_KEY, NEW_KEY],
        "the in-flight leg dispatched with the secret it was verified with, and only the later request saw the rotation"
    );
    up.shutdown().await;
}

/// An OAuth 401 whose refresh fails as the token endpoint is down, carrying the headers an upstream 401 sends.
async fn an_expired_token_and_a_token_endpoint_down() -> FakeUpstream {
    FakeUpstream::start(vec![
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: Respond::Json {
                status: 401,
                headers: vec![
                    ("request-id".into(), "req_upstream_401".into()),
                    ("www-authenticate".into(), "Bearer".into()),
                ],
                body:
                    br#"{"type":"error","error":{"type":"authentication_error","message":"expired"}}"#
                        .to_vec(),
            },
        },
        Rule {
            path_prefix: "/v1/oauth/token".into(),
            respond: Respond::Json {
                status: 503,
                headers: vec![],
                body: b"{}".to_vec(),
            },
        },
    ])
    .await
}

/// The ledger rows of the one request the subscriber saw: the request row and its attempt rows.
async fn ledger_rows(
    sub: &mut phoebis_bus::Subscriber,
) -> (
    phoebis_ledger::RequestRecord,
    Vec<phoebis_ledger::AttemptRecord>,
) {
    rows_of(drain_until_complete(sub).await)
}

/// The rows one request's events assemble into, for a test that also reads the events themselves.
fn rows_of(
    events: Vec<phoebis_bus::Event>,
) -> (
    phoebis_ledger::RequestRecord,
    Vec<phoebis_ledger::AttemptRecord>,
) {
    let mut assembler = Assembler::new(std::sync::Arc::new(|| 0));
    let records: Vec<Record> = events
        .into_iter()
        .flat_map(|e| assembler.observe(e))
        .collect();
    records
        .into_iter()
        .find_map(|r| match r {
            Record::Request {
                request, attempts, ..
            } => Some((request, attempts)),
            _ => None,
        })
        .expect("the request lands a ledger row")
}

fn token_posts(up: &FakeUpstream) -> usize {
    up.requests()
        .iter()
        .filter(|r| r.line.starts_with("POST /v1/oauth/token"))
        .count()
}

/// The token endpoint's answer to a refresh that rotates: the profile's fixture, naming `ACCOUNT_UUID`.
fn token_answer() -> Respond {
    Respond::Json {
        status: 200,
        headers: vec![],
        body: phoebis_profile::fixtures::control_response(
            phoebis_profile::fixtures::ControlFixture::Token,
        )
        .to_vec(),
    }
}

fn expired() -> Respond {
    Respond::Json {
        status: 401,
        headers: vec![],
        body: br#"{"type":"error","error":{"type":"authentication_error","message":"expired"}}"#
            .to_vec(),
    }
}

/// An OAuth account the token fixture's identity matches, so a refresh that reaches the endpoint commits.
fn paired(id: &'static str, secret: &'static str) -> TestAccount {
    TestAccount {
        uuid: Some(ACCOUNT_UUID),
        ..oauth(id, secret, 0)
    }
}

/// The request in the background, so the test can move the clock while the relay holds it.
fn spawned(
    h: &Harness,
    req: axum::http::Request<axum::body::Body>,
) -> tokio::task::JoinHandle<axum::http::Response<axum::body::Body>> {
    let app = h.app.clone();
    tokio::spawn(async move { app.oneshot(req).await.unwrap() })
}

/// The held request's answer, which has to arrive within `wait_for`'s bound of real time.
async fn answered(
    inflight: tokio::task::JoinHandle<axum::http::Response<axum::body::Body>>,
) -> axum::http::Response<axum::body::Body> {
    wait_for(Duration::from_secs(5), || {
        inflight.is_finished().then_some(())
    })
    .await;
    inflight.await.unwrap()
}

#[tokio::test]
async fn a_refusal_that_rotated_nothing_is_retried_once_after_the_backoff_and_then_fails_over() {
    let up = an_expired_token_and_a_token_endpoint_down().await;
    let h = harness_with(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
        default_limits(),
    )
    .await;
    let inflight = spawned(&h, request_for(KEY, "claude-sonnet-4-5", "s1"));
    step_to_backoff(&h, &up, "a").await;
    wait_for(Duration::from_secs(5), || {
        (token_posts(&up) == 2).then_some(())
    })
    .await;
    let resp = answered(inflight).await;
    assert_eq!(resp.status(), 503, "the router's own answer");
    assert_eq!(
        resp.headers()["x-phoebis-error-code"],
        "accounts_unavailable"
    );
    assert!(resp.headers().contains_key("content-type"));
    for upstream in ["request-id", "www-authenticate"] {
        assert!(
            !resp.headers().contains_key(upstream),
            "no header of the upstream's 401 reaches the caller: {upstream}"
        );
    }
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        body["error"]["message"],
        "no account in this pool can serve requests"
    );
    assert!(
        !text.contains(r#""a""#) && !text.contains("expired"),
        "the answer names no account and no reason: {text}"
    );
    assert_eq!(
        credentials_seen(&up),
        [A],
        "the token the upstream rejected is not sent a second time"
    );
    assert_eq!(
        token_posts(&up),
        2,
        "the refresh and its one retry past the backoff"
    );
    // A mark's write is asynchronous, so this window bounds how long one would have to appear rather than
    // proving none is coming.
    stays_for(Duration::from_millis(200), || {
        h.store.account("a").unwrap().unwrap().state == AccountState::Ready
    })
    .await;
    // The failover invalidated the pin this request created, through a report that lands asynchronously.
    wait_for_async(Duration::from_secs(2), || async {
        h.router.pins().await.unwrap().is_empty().then_some(())
    })
    .await;
    up.shutdown().await;
}

#[tokio::test]
async fn an_authentication_failure_as_the_last_word_reads_exhausted_with_auth_failed_attempts() {
    let up = an_expired_token_and_a_token_endpoint_down().await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(&up, &[oauth("a", A, 0)], PoolSpec::default(), bus).await;
    let inflight = spawned(&h, request_for(KEY, "claude-sonnet-4-5", "s1"));
    step_to_backoff(&h, &up, "a").await;
    let resp = answered(inflight).await;
    assert_eq!(resp.status(), 503);
    resp.into_body().collect().await.unwrap();
    let (request, attempts) = ledger_rows(&mut sub).await;
    assert_eq!(
        (
            request.outcome.as_str(),
            request.error_code.as_deref(),
            request.status
        ),
        ("exhausted", Some("accounts_unavailable"), Some(503))
    );
    assert_eq!(
        attempts
            .iter()
            .map(|a| (a.outcome.as_str(), a.upstream_status))
            .collect::<Vec<_>>(),
        [("auth_failed", Some(401))]
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_non_portable_requests_401_answers_the_503_on_its_own_account() {
    let up = an_expired_token_and_a_token_endpoint_down().await;
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let inflight = spawned(&h, non_portable_request(KEY, "s1"));
    step_to_backoff(&h, &up, "a").await;
    let resp = answered(inflight).await;
    assert_eq!(
        (
            resp.status().as_u16(),
            resp.headers()["x-phoebis-error-code"].to_str().unwrap()
        ),
        (503, "accounts_unavailable")
    );
    assert_eq!(credentials_seen(&up), [A]);
    assert_eq!(
        outcome(&resp).account_id,
        "a",
        "the answer is the leg's own, not a reselection's"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_hold_after_a_token_endpoint_5xx_keeps_the_pin_and_the_second_refresh_serves() {
    let up = FakeUpstream::start(vec![
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: expired(),
        },
        Rule {
            path_prefix: "/v1/oauth/token".into(),
            respond: Respond::Json {
                status: 503,
                headers: vec![],
                body: b"{}".to_vec(),
            },
        },
    ])
    .await;
    up.set_rule_after(1, "/v1/messages", json_200());
    up.set_rule_after(1, "/v1/oauth/token", token_answer());
    let h = harness(&up, &[paired("a", A)], PoolSpec::default()).await;
    let inflight = spawned(&h, request_for(KEY, "claude-sonnet-4-5", "s1"));
    wait_for(Duration::from_secs(5), || h.manager.backoff_until("a")).await;
    stays_for(Duration::from_millis(300), || {
        credentials_seen(&up) == [A] && !inflight.is_finished()
    })
    .await;
    step_to_backoff(&h, &up, "a").await;
    let resp = answered(inflight).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(outcome(&resp).account_id, "a");
    resp.into_body().collect().await.unwrap();
    assert_eq!(
        credentials_seen(&up),
        [A, "sk-ant-oat01-PLACEHOLDER"],
        "the replay carries the refreshed token, on the same account"
    );
    wait_for_async(Duration::from_secs(2), || async {
        h.router
            .pins()
            .await
            .unwrap()
            .iter()
            .any(|p| p.session_id == "s1" && p.account_id == "a")
            .then_some(())
    })
    .await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_short_refusal_is_retried_within_the_transient_backoff() {
    let up = FakeUpstream::start(vec![
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: expired(),
        },
        Rule {
            path_prefix: "/v1/oauth/token".into(),
            respond: Respond::Json {
                status: 429,
                headers: vec![],
                body: b"{}".to_vec(),
            },
        },
    ])
    .await;
    up.set_rule_after(1, "/v1/messages", json_200());
    up.set_rule_after(1, "/v1/oauth/token", token_answer());
    let h = harness(&up, &[paired("a", A)], PoolSpec::default()).await;
    let inflight = spawned(&h, request_for(KEY, "claude-sonnet-4-5", "s1"));
    step_to_backoff(&h, &up, "a").await;
    let resp = answered(inflight).await;
    assert_eq!(
        (resp.status().as_u16(), outcome(&resp).account_id.as_str()),
        (200, "a")
    );
    assert_eq!(token_posts(&up), 2);
    up.shutdown().await;
}

#[tokio::test]
async fn a_retry_after_past_the_floor_fails_over_at_once() {
    let up = FakeUpstream::start(vec![
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: expired(),
        },
        Rule {
            path_prefix: "/v1/oauth/token".into(),
            respond: Respond::Json {
                status: 503,
                headers: vec![("retry-after".into(), "300".into())],
                body: b"{}".to_vec(),
            },
        },
    ])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let inflight = spawned(&h, request_for(KEY, "claude-sonnet-4-5", "s1"));
    let resp = answered(inflight).await;
    assert_eq!(
        (
            resp.status().as_u16(),
            resp.headers()["x-phoebis-error-code"].to_str().unwrap()
        ),
        (503, "accounts_unavailable"),
        "not held for the five minutes asked"
    );
    assert_eq!(token_posts(&up), 1);
    up.shutdown().await;
}

#[tokio::test]
async fn a_lost_response_then_an_invalid_grant_marks_the_account_and_fails_over() {
    let up = FakeUpstream::start(vec![
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: expired(),
        },
        Rule {
            path_prefix: "/v1/oauth/token".into(),
            respond: Respond::CloseAfterRequest,
        },
    ])
    .await;
    up.set_rule_after(
        1,
        "/v1/oauth/token",
        Respond::Json {
            status: 400,
            headers: vec![],
            body: br#"{"error":"invalid_grant","error_description":"revoked"}"#.to_vec(),
        },
    );
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let inflight = spawned(&h, request_for(KEY, "claude-sonnet-4-5", "s1"));
    step_to_backoff(&h, &up, "a").await;
    wait_for(Duration::from_secs(5), || {
        (h.store.account("a").unwrap().unwrap().state == AccountState::ReauthRequired).then_some(())
    })
    .await;
    let resp = answered(inflight).await;
    assert_eq!(resp.status(), 503, "the router's own answer");
    assert_eq!(token_posts(&up), 2);
    assert!(
        h.router
            .pins()
            .await
            .unwrap()
            .iter()
            .any(|p| p.session_id == "s1" && p.account_id == "a"),
        "the mark ends the pin at the next selection, under its own reason"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn six_accounts_with_lost_refresh_answers_hold_once_and_answer_within_the_deadline() {
    const D: &str = "sk-ant-oat01-DDDD";
    const E: &str = "sk-ant-oat01-EEEE";
    const F: &str = "sk-ant-oat01-FFFF";
    let up = FakeUpstream::start(vec![
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: expired(),
        },
        Rule {
            path_prefix: "/v1/oauth/token".into(),
            respond: Respond::CloseAfterRequest,
        },
    ])
    .await;
    // The router's own `head_timeout`, so the request carries the deadline production gives it.
    let h = harness_with_limits(
        &up,
        &[
            oauth("a", A, 0),
            oauth("b", B, 1),
            oauth("c", C, 2),
            oauth("d", D, 3),
            oauth("e", E, 4),
            oauth("f", F, 5),
        ],
        PoolSpec::default(),
        phoebis_relay::Limits {
            head_timeout: Duration::from_secs(600),
            ..default_limits()
        },
    )
    .await;
    let inflight = spawned(&h, request_for(KEY, "claude-sonnet-4-5", "s1"));
    step_to_backoff(&h, &up, "a").await;
    let resp = answered(inflight).await;
    assert_eq!(
        (
            resp.status().as_u16(),
            resp.headers()["x-phoebis-error-code"].to_str().unwrap()
        ),
        (503, "accounts_unavailable")
    );
    assert_eq!(
        token_posts(&up),
        7,
        "two on the held account, its first and its retry past the floor, and one on each of the five others"
    );
    assert_eq!(
        credentials_seen(&up).len(),
        6,
        "one hold, then five failovers at once"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_wait_that_would_pass_the_request_deadline_ends_at_it_with_the_exhaustion_answer() {
    let up = an_expired_token_and_a_token_endpoint_down().await;
    // 40 s exceeds the margin, so the request's deadline is 10 s past its admission. `b` is free, so only the
    // deadline at the loop's head keeps the request from selecting it once the wait on `a` ends.
    let h = harness_with_limits(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec::default(),
        phoebis_relay::Limits {
            head_timeout: Duration::from_secs(40),
            ..default_limits()
        },
    )
    .await;
    let inflight = spawned(&h, request_for(KEY, "claude-sonnet-4-5", "s1"));
    wait_for(Duration::from_secs(5), || h.manager.backoff_until("a")).await;
    stays_for(Duration::from_millis(300), || !inflight.is_finished()).await;
    // The deadline, short of the floor.
    h.clock
        .fetch_add(10_000, std::sync::atomic::Ordering::SeqCst);
    let resp = answered(inflight).await;
    assert_eq!(
        (
            resp.status().as_u16(),
            resp.headers()["x-phoebis-error-code"].to_str().unwrap()
        ),
        (503, "accounts_unavailable")
    );
    assert_eq!(
        token_posts(&up),
        1,
        "the floor was not waited out past the request's deadline"
    );
    assert_eq!(
        credentials_seen(&up),
        [A],
        "no new leg started past the deadline"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_short_refusal_waited_out_on_one_account_leaves_the_floor_to_the_next() {
    // Unscoped, for the replay on the refreshed token; each account's own token answers 401.
    let up = FakeUpstream::start(vec![
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: json_200(),
        },
        Rule {
            path_prefix: "/v1/oauth/token".into(),
            respond: Respond::Json {
                status: 429,
                headers: vec![],
                body: b"{}".to_vec(),
            },
        },
    ])
    .await;
    up.add_credential_rule(A, "/v1/messages", expired());
    up.add_credential_rule(B, "/v1/messages", expired());
    // `a`'s refresh and its retry answer 429, `b`'s first refresh loses its answer.
    up.set_rule_after(2, "/v1/oauth/token", Respond::CloseAfterRequest);
    let h = harness(
        &up,
        &[
            oauth("a", A, 0),
            TestAccount {
                uuid: Some(ACCOUNT_UUID),
                ..oauth("b", B, 1)
            },
        ],
        PoolSpec::default(),
    )
    .await;
    let inflight = spawned(&h, request_for(KEY, "claude-sonnet-4-5", "s1"));
    step_to_backoff(&h, &up, "a").await;
    assert_eq!(
        h.clock.load(std::sync::atomic::Ordering::SeqCst),
        NOW + 5_000,
        "the 429 armed the transient backoff, and the request waited it out on `a`"
    );
    wait_for(Duration::from_secs(5), || {
        (token_posts(&up) == 3).then_some(())
    })
    .await;
    wait_for(Duration::from_secs(5), || h.manager.backoff_until("b")).await;
    stays_for(Duration::from_millis(300), || !inflight.is_finished()).await;
    up.set_rule_after(3, "/v1/oauth/token", token_answer());
    step_to_backoff(&h, &up, "b").await;
    assert_eq!(
        h.clock.load(std::sync::atomic::Ordering::SeqCst),
        NOW + 65_000,
        "the lost answer armed the floor, which the transient wait on `a` did not spend"
    );
    let resp = answered(inflight).await;
    assert_eq!(
        (resp.status().as_u16(), outcome(&resp).account_id.as_str()),
        (200, "b")
    );
    assert_eq!(
        token_posts(&up),
        4,
        "two on `a`, the second after its transient backoff, then two on `b` around its floor"
    );
    assert_eq!(credentials_seen(&up), [A, B, "sk-ant-oat01-PLACEHOLDER"]);
    up.shutdown().await;
}

#[tokio::test]
async fn a_hold_the_accounts_removal_cancels_keeps_one_head_and_completes_canceled() {
    let up = an_expired_token_and_a_token_endpoint_down().await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(&up, &[oauth("a", A, 0)], PoolSpec::default(), bus).await;
    let inflight = spawned(&h, request_for(KEY, "claude-sonnet-4-5", "s1"));
    wait_for(Duration::from_secs(5), || h.manager.backoff_until("a")).await;
    stays_for(Duration::from_millis(300), || !inflight.is_finished()).await;
    h.store.delete_account("a", "test").unwrap();
    h.router.reload().await.unwrap();
    let resp = answered(inflight).await;
    assert_eq!(
        (
            resp.status().as_u16(),
            resp.headers()["x-phoebis-error-code"].to_str().unwrap()
        ),
        (503, "account_cancelled")
    );
    resp.into_body().collect().await.unwrap();
    let events = drain_until_complete(&mut sub).await;
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, phoebis_bus::Event::ResponseHead(_)))
            .count(),
        1,
        "the attempt's head is the 401 the hold followed"
    );
    let (request, attempts) = rows_of(events);
    assert_eq!(request.outcome.as_str(), "canceled");
    assert_eq!(
        attempts
            .iter()
            .map(|a| (a.outcome.as_str(), a.upstream_status))
            .collect::<Vec<_>>(),
        [("auth_failed", Some(401))]
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_403_authentication_error_reads_auth_failed_on_the_attempt_row() {
    const KEYED: &str = "sk-ant-api03-KEYED0001";
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(
        KEYED,
        "/v1/messages",
        Respond::Json {
            status: 403,
            headers: vec![],
            body: br#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#
                .to_vec(),
        },
    );
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(&up, &[api_key("a", KEYED, 0)], PoolSpec::default(), bus).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(
        (
            resp.status().as_u16(),
            resp.headers()["x-phoebis-error-code"].to_str().unwrap()
        ),
        (503, "accounts_unavailable")
    );
    resp.into_body().collect().await.unwrap();
    let (_, attempts) = ledger_rows(&mut sub).await;
    assert_eq!(
        attempts
            .iter()
            .map(|a| (a.outcome.as_str(), a.upstream_status))
            .collect::<Vec<_>>(),
        [("auth_failed", Some(403))]
    );
    wait_for(Duration::from_secs(5), || {
        (h.store.account("a").unwrap().unwrap().state == AccountState::ReauthRequired).then_some(())
    })
    .await;
    up.shutdown().await;
}

const KEYED_A: &str = "sk-ant-api03-KEYEDAAAA";
const KEYED_B: &str = "sk-ant-api03-KEYEDBBBB";

fn invalid_key() -> Respond {
    Respond::Json {
        status: 401,
        headers: vec![],
        body: br#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#
            .to_vec(),
    }
}

/// The engine's own probe under the harness key.
fn probe_of(h: &Harness) -> phoebis_relay::Probe {
    phoebis_relay::Probe {
        key_id: "desktop".into(),
        key_generation: h.store.api_key("desktop").unwrap().unwrap().created_at,
    }
}

/// The probe's answer as the keepalive reads it: the status, the code and the obstacle.
fn probe_answer(
    resp: &axum::http::Response<axum::body::Body>,
) -> (u16, String, Option<&'static str>) {
    let o = outcome(resp);
    (
        resp.status().as_u16(),
        o.error_code.unwrap_or_default(),
        o.obstacle,
    )
}

#[tokio::test]
async fn a_probe_whose_leg_marks_its_pinned_account_names_the_mark_though_a_sibling_is_free() {
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(KEYED_A, "/v1/messages", invalid_key());
    up.push_credential_rule(KEYED_A, "/v1/messages", json_200());
    up.add_credential_rule(KEYED_B, "/v1/messages", json_200());
    let h = harness(
        &up,
        &[api_key("a", KEYED_A, 0), api_key("b", KEYED_B, 1)],
        PoolSpec::default(),
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(outcome(&resp).account_id, "a");
    resp.into_body().collect().await.unwrap();
    let probed = h
        .relay
        .probe(probe_of(&h), request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await;
    assert_eq!(
        probe_answer(&probed),
        (503, "accounts_unavailable".into(), Some("reauth_required")),
        "the probe that tried its pinned member is answered under the mark its own leg set"
    );
    assert_eq!(
        credentials_seen(&up),
        [KEYED_A, KEYED_A],
        "the free sibling is never dialled for the probe"
    );
    assert!(
        h.router
            .pins()
            .await
            .unwrap()
            .iter()
            .any(|p| p.session_id == "s1" && p.account_id == "a"),
        "the probe moved no pin"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_probe_whose_refresh_answers_invalid_grant_names_the_mark() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/oauth/token".into(),
        respond: Respond::Json {
            status: 400,
            headers: vec![],
            body: br#"{"error":"invalid_grant","error_description":"revoked"}"#.to_vec(),
        },
    }])
    .await;
    up.add_credential_rule(
        A,
        "/v1/messages",
        Respond::Json {
            status: 401,
            headers: vec![],
            body:
                br#"{"type":"error","error":{"type":"authentication_error","message":"expired"}}"#
                    .to_vec(),
        },
    );
    up.push_credential_rule(A, "/v1/messages", json_200());
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.into_body().collect().await.unwrap();
    let probed = h
        .relay
        .probe(probe_of(&h), request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await;
    assert_eq!(
        probe_answer(&probed),
        (503, "accounts_unavailable".into(), Some("reauth_required")),
        "the refresh's own mark reaches the probe's answer"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_non_portable_probe_whose_leg_marks_its_account_names_the_mark() {
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(KEYED_A, "/v1/messages", invalid_key());
    up.push_credential_rule(KEYED_A, "/v1/messages", json_200());
    let h = harness(&up, &[api_key("a", KEYED_A, 0)], PoolSpec::default()).await;
    let resp = h
        .app
        .clone()
        .oneshot(non_portable_request(KEY, "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.into_body().collect().await.unwrap();
    let probed = h
        .relay
        .probe(probe_of(&h), non_portable_request(KEY, "s1"))
        .await;
    assert_eq!(
        probe_answer(&probed),
        (503, "accounts_unavailable".into(), Some("reauth_required")),
        "a probe bound to its account names the mark on its own answer"
    );
    assert_eq!(outcome(&probed).account_id, "a");
    up.shutdown().await;
}

#[tokio::test]
async fn a_pre_selection_refusal_carries_the_client_version() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec {
            model_rules: rules(&["claude-opus-5", "NOT *"]),
            ..PoolSpec::default()
        },
    )
    .await;
    let mut req = request_for(KEY, "claude-sonnet-4-5", "s1");
    set_header(&mut req, "user-agent", "claude-cli/2.1.300 (external, cli)");
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 404);
    let o = outcome(&resp);
    assert_eq!(
        (
            o.error_code.as_deref(),
            o.client_drift.as_deref(),
            o.client_version.as_deref(),
            o.attempt
        ),
        (Some("model_not_allowed"), None, Some("2.1.300"), 0),
        "a refusal before selection still names the client build that was refused"
    );
    assert!(up.requests().is_empty());
    up.shutdown().await;
}

/// The fence in `Relay::credential`: the row the selection handed over and the credential acquired for it
/// have to describe the same account revision. A token refresh is the one await inside the credential step,
/// so the fake's token endpoint is gated to hold the relay there while the account is edited underneath.
#[tokio::test]
async fn a_credential_acquired_across_an_account_edit_is_not_dispatched_with_the_old_row() {
    use phoebis_profile::fixtures::{control_response, ControlFixture};
    let up = FakeUpstream::start(vec![
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: json_200(),
        },
        Rule {
            path_prefix: "/v1/oauth/token".into(),
            respond: Respond::Gated(Box::new(Respond::Json {
                status: 200,
                headers: vec![],
                body: control_response(ControlFixture::Token).to_vec(),
            })),
        },
    ])
    .await;
    // `a` carries the fixture's account uuid so the refreshed token's identity matches its row.
    let h = harness(
        &up,
        &[
            TestAccount {
                id: "a",
                kind: phoebis_store::AccountKind::Oauth,
                secret: A,
                priority: 0,
                uuid: Some(ACCOUNT_UUID),
            },
            oauth("b", B, 1),
        ],
        PoolSpec::default(),
    )
    .await;
    // An access token inside the refresh margin, so the next acquire refreshes through the gated endpoint.
    h.store
        .commit_credentials(
            "a",
            1,
            1,
            Some(1),
            None,
            &phoebis_store::NewBundle {
                access_token: phoebis_store::Secret::new(A),
                refresh_token: Some(phoebis_store::Secret::new("sk-ant-ort01-A")),
                issued_at: NOW - 3_600_000,
                expires_at: NOW + 1_000,
                refresh_expires_at: None,
                scopes: "user:inference".into(),
            },
        )
        .unwrap();
    let app = h.app.clone();
    let inflight = tokio::spawn(async move {
        app.oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
            .await
            .unwrap()
    });
    wait_for(Duration::from_secs(5), || {
        up.requests()
            .iter()
            .any(|r| r.line.starts_with("POST /v1/oauth/token"))
            .then_some(())
    })
    .await;
    // The relay is inside `credential` for `a`; the row it selected is revision 1, and this makes it 2.
    h.store
        .update_account("a", 1, "a (edited)", "test")
        .unwrap();
    up.release();
    let resp = inflight.await.unwrap();
    assert_eq!(resp.status(), 200);
    let o = outcome(&resp);
    assert_eq!(
        (o.account_id.as_str(), o.attempt),
        ("b", 1),
        "the stale row is not dispatched; the request is re-selected"
    );
    assert_eq!(
        credentials_seen(&up),
        [B],
        "no request went out under a's refreshed token"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_committed_answer_the_relay_drained_renews_its_pin_where_the_answer_ends() {
    use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(
        A,
        "/v1/messages",
        Respond::Json {
            status: 400,
            headers: vec![("content-type".into(), "application/json".into())],
            body: br#"{"type":"error","error":{"type":"invalid_request_error","message":"messages: text content blocks must be non-empty"}}"#
                .to_vec(),
        },
    );
    up.push_credential_rule(A, "/v1/messages", json_200());
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    // A first turn with five-minute markers is answered, so the pin has served and its renewal left it at five
    // minutes: a client error on a pin its own request created releases it instead of renewing it.
    let five_minutes = String::from_utf8(body(BodyFixture::Oauth).to_vec())
        .unwrap()
        .replace(r#","ttl":"1h""#, "");
    let mut first = request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        five_minutes.as_bytes(),
        KEY,
    );
    set_header(&mut first, "x-claude-code-session-id", "s1");
    let first = h.app.clone().oneshot(first).await.unwrap();
    assert_eq!(first.status(), 200);
    first.into_body().collect().await.unwrap();
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "the client's own rejection, committed");
    resp.into_body().collect().await.unwrap();
    let pins = h.router.pins().await.unwrap();
    assert_eq!(pins.len(), 1);
    assert_eq!(
        (pins[0].account_id.as_str(), pins[0].cache_ttl_ms),
        ("a", 3_600_000),
        "the drained answer's renewal named the body's one-hour markers over the first turn's five minutes"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_fast_answers_renewal_folds_its_dollars_and_leaves_the_plan_copy_of_the_burn_at_zero() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"id":"msg_1","type":"message","content":[],"usage":{"input_tokens":100,"output_tokens":20,"speed":"fast"}}"#
                .to_vec(),
        },
    }])
    .await;
    let h = harness(
        &up,
        &[api_key("k1", "sk-ant-api03-KEY1", 0)],
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
    resp.into_body().collect().await.unwrap();
    let pin = wait_for_async(Duration::from_secs(5), || async {
        h.router
            .pins()
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.burn_at.is_some())
    })
    .await;
    assert!(
        pin.burn.input > 0.0 && pin.burn.output > 0.0,
        "the answer's usage folds into the dollar copy: {:?}",
        pin.burn
    );
    assert_eq!(
        pin.burn_banded,
        phoebis_store::PinBurn::default(),
        "an answer reporting usage.speed fast draws no plan window, so the banded copy folds nothing"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_renewal_report_carries_the_usage_the_origin_and_the_ttl_and_a_probe_folds_no_burn() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"id":"msg_1","type":"message","content":[],"usage":{"input_tokens":100,"cache_read_input_tokens":50,"output_tokens":20}}"#
                .to_vec(),
        },
    }])
    .await;
    let h = harness(
        &up,
        &[api_key("k1", "sk-ant-api03-KEY1", 0)],
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
    resp.into_body().collect().await.unwrap();
    let pin = wait_for_async(Duration::from_secs(5), || async {
        h.router
            .pins()
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.burn_at.is_some())
    })
    .await;
    assert_eq!(
        pin.last_input_tokens, 150,
        "the answer's usage reached the pin"
    );
    assert_eq!(
        pin.cache_ttl_ms, 3_600_000,
        "no rewrite applied, so the client's own one-hour markers name the TTL"
    );
    h.clock
        .store(NOW + 60_000, std::sync::atomic::Ordering::SeqCst);
    let resp = h
        .relay
        .probe(
            phoebis_relay::Probe {
                key_id: "desktop".into(),
                key_generation: h.store.api_key("desktop").unwrap().unwrap().created_at,
            },
            request_for(KEY, "claude-sonnet-4-5", "s1"),
        )
        .await;
    assert_eq!(resp.status(), 200);
    resp.into_body().collect().await.unwrap();
    let probed = wait_for_async(Duration::from_secs(5), || async {
        h.router
            .pins()
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.last_refresh_at == NOW + 60_000)
    })
    .await;
    assert_eq!(
        (probed.epoch, probed.burn, probed.last_input_tokens),
        (pin.epoch, pin.burn, pin.last_input_tokens),
        "a probe's renewal stamps the refresh and folds none of its usage"
    );
    up.shutdown().await;
}

/// The first-party fixture with a stamp this build does not compute, and with a version suffix that does not
/// reproduce over a body re-signed so its hash does: one body per drift, with the trigger each files.
fn drifted_first_party_bodies() -> [(Vec<u8>, &'static str); 2] {
    use phoebis_profile::fixtures::{body, BodyFixture};
    let fixture = String::from_utf8(body(BodyFixture::FirstParty9b244).to_vec()).unwrap();
    let hash = fixture.replacen("cch=9b244", "cch=9b245", 1).into_bytes();
    let suffix = phoebis_cch::sign(
        fixture
            .replacen("cc_version=2.1.269.517", "cc_version=2.1.269.abc", 1)
            .as_bytes(),
    )
    .unwrap();
    [(hash, "hash"), (suffix, "version_suffix")]
}

#[tokio::test]
async fn a_drifted_stamp_is_refused_at_admission_before_any_account_is_selected() {
    use phoebis_profile::fixtures::{inference_head, FixtureKind};
    for (drifted, trigger) in drifted_first_party_bodies() {
        let up = FakeUpstream::start(vec![Rule {
            path_prefix: "/v1/messages".into(),
            respond: json_200(),
        }])
        .await;
        let bus = Bus::new();
        let mut sub = bus.subscribe("test", Delivery::Durable);
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
            .oneshot(request_from_fixture(
                inference_head(FixtureKind::OauthFirstParty),
                &drifted,
                KEY,
            ))
            .await
            .unwrap();
        assert_eq!(
            (
                resp.status().as_u16(),
                resp.headers()["x-phoebis-error-code"].to_str().unwrap()
            ),
            (400, "client_drift"),
            "{trigger}"
        );
        let o = outcome(&resp);
        assert_eq!(
            (o.account_id.as_str(), o.attempt, o.client_drift.as_deref()),
            ("", 0, Some(trigger)),
            "no account was selected, so the refusal names none"
        );
        assert!(
            h.router.pins().await.unwrap().is_empty(),
            "{trigger}: no selection pinned the session"
        );
        let events = drain_until_complete(&mut sub).await;
        let complete = events
            .iter()
            .find_map(|e| match e {
                phoebis_bus::Event::RequestComplete(c) => Some(c.clone()),
                _ => None,
            })
            .expect("the refusal completes on the bus");
        // The attention reader raises the drift condition from the completion alone.
        assert_eq!(
            (
                complete.client_drift.as_deref(),
                complete.account_id.as_deref()
            ),
            (Some(trigger), None)
        );
        let (row, attempts) = rows_of(events);
        assert_eq!(
            (
                row.error_code.as_deref(),
                row.client_drift.as_deref(),
                attempts.len()
            ),
            (Some("client_drift"), Some(trigger), 0)
        );
        // Judged once the identity is read, so the row keeps the model, the session and the agent.
        assert_eq!(
            (
                row.model.as_str(),
                row.session_id.as_deref(),
                row.agent_id.as_deref()
            ),
            (
                "claude-sonnet-4-5",
                Some("11111111-1111-4111-8111-111111111111"),
                Some("main")
            )
        );
        assert!(up.requests().is_empty(), "{trigger}: nothing was dialled");
        up.shutdown().await;
    }
}

#[tokio::test]
async fn a_billing_line_with_no_stamp_is_not_refused_for_its_version_suffix() {
    use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    // The reduced shape, its suffix one this build does not reproduce, and no `cch=` claiming a hash.
    let reduced = String::from_utf8(body(BodyFixture::Oauth).to_vec())
        .unwrap()
        .replacen("cc_version=2.1.269.517", "cc_version=2.1.269.abc", 1);
    assert_eq!(phoebis_cch::decoded_stamp_count(reduced.as_bytes()), 0);
    let resp = h
        .app
        .clone()
        .oneshot(request_from_fixture(
            inference_head(FixtureKind::OauthNonFirstParty),
            reduced.as_bytes(),
            KEY,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(outcome(&resp).client_drift, None);
    assert_eq!(credentials_seen(&up), [A]);
    up.shutdown().await;
}

#[tokio::test]
async fn a_drifted_stamp_under_a_key_that_refuses_the_model_gets_the_model_refusal() {
    use phoebis_profile::fixtures::{body, inference_head, BodyFixture, FixtureKind};
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec {
            model_rules: rules(&["claude-opus-5", "NOT *"]),
            ..PoolSpec::default()
        },
    )
    .await;
    let fixture = String::from_utf8(body(BodyFixture::FirstParty9b244).to_vec()).unwrap();
    // A stamp this build does not compute, and one only the decoded walk reaches.
    let drifted = fixture.replacen("cch=9b244", "cch=9b245", 1);
    let escaped = fixture.replacen(
        r#""text":"x-anthropic-billing-header"#,
        r#""te\u0078t":"x-anthropic-billing-header"#,
        1,
    );
    let mut answers = Vec::new();
    for sent in [drifted, escaped] {
        let resp = h
            .app
            .clone()
            .oneshot(request_from_fixture(
                inference_head(FixtureKind::OauthFirstParty),
                sent.as_bytes(),
                KEY,
            ))
            .await
            .unwrap();
        answers.push((
            resp.status().as_u16(),
            resp.headers()["x-phoebis-error-code"]
                .to_str()
                .unwrap()
                .to_string(),
        ));
    }
    assert_eq!(
        answers,
        [
            (404, "model_not_allowed".to_string()),
            (404, "model_not_allowed".to_string())
        ],
        "the key's model check answers ahead of the stamp check"
    );
    assert!(up.requests().is_empty());
    up.shutdown().await;
}
