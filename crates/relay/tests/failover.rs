mod common;
use std::time::Duration;

use phoebis_store::{AccountState, ServedModelWrite};
use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule};
use http_body_util::BodyExt;
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
async fn at_most_max_attempts_upstream_requests_and_then_the_exhaustion_responses() {
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
    assert_eq!(
        resp.status(),
        429,
        "the budget ran out, the client gets the last upstream answer"
    );
    assert_eq!(outcome(&resp).attempt, 3);
    assert_eq!(
        credentials_seen(&up).len(),
        3,
        "four accounts, three attempts"
    );
    // The fourth account is still eligible, so the next request tries it and only it.
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
async fn key_policy_404s_carry_their_codes_and_reach_no_upstream() {
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
    let open = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let resp = open
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-haiku-4-5", "s1"))
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
        (404, "model_unserved")
    );
    assert!(up.requests().is_empty());
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
    let cooldowns = h.router.cooldowns().await.unwrap();
    assert_eq!(
        (
            cooldowns[0].scope.as_str(),
            cooldowns[0].reason.as_str(),
            cooldowns[0].until
        ),
        (
            "model:claude-sonnet-4-5",
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
    let c = h.router.cooldowns().await.unwrap();
    let k1 = c.iter().find(|c| c.account_id == "k1").unwrap();
    assert_eq!(
        (k1.scope.as_str(), k1.until),
        ("model:claude-sonnet-4-5", NOW + 30_000 + 1_000),
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
        &["claude-fable-5-1", "claude-opus-5", "claude-sonnet-4-5"],
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
        .set_pool_members("main", &[("b".into(), 1)], "test")
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
    h.store
        .set_pool_members("main", &[("a".into(), 0), ("b".into(), 1)], "test")
        .unwrap();
    h.store
        .put_served_models(
            "a",
            &[
                ServedModelWrite {
                    upstream_id: "claude-sonnet-4-5".into(),
                    selected: false,
                    long_context: false,
                },
                ServedModelWrite {
                    upstream_id: "claude-opus-5".into(),
                    selected: true,
                    long_context: false,
                },
            ],
            "test",
        )
        .unwrap();
    h.router.reload().await.unwrap();
    let resp = h
        .app
        .clone()
        .oneshot(non_portable_request(KEY, "s1"))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        503,
        "a deselected model on the pinned account is the same fence"
    );
    assert_eq!(credentials_seen(&up), [A]);
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
async fn a_401_the_budget_cannot_refresh_is_forwarded_without_a_terminal_mark() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 401,
            headers: vec![],
            body:
                br#"{"type":"error","error":{"type":"authentication_error","message":"expired"}}"#
                    .to_vec(),
        },
    }])
    .await;
    let one = phoebis_relay::Limits {
        max_attempts: 1,
        ..default_limits()
    };
    let h = harness_with(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
        &["claude-sonnet-4-5"],
        one,
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "the upstream's own answer");
    assert_eq!(credentials_seen(&up).len(), 1, "no refresh, no replay");
    assert!(up
        .requests()
        .iter()
        .all(|r| !r.line.starts_with("POST /v1/oauth/token")));
    // A mark's write is asynchronous, so this window bounds how long one would have to appear rather than
    // proving none is coming. What guarantees the absence is the path itself: a 401 the budget cannot refresh
    // builds no mark at all, because nothing conclusive happened.
    stays_for(Duration::from_millis(200), || {
        h.store.account("a").unwrap().unwrap().state == AccountState::Ready
    })
    .await;
    assert_eq!(h.router.pins().await.unwrap().len(), 1, "the pin stays");
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
        &["claude-sonnet-4-5"],
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

#[tokio::test]
async fn a_401_whose_refresh_rotates_nothing_is_forwarded_and_keeps_the_account() {
    let up = FakeUpstream::start(vec![
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: Respond::Json {
                status: 401,
                headers: vec![],
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
    .await;
    let h = harness_with(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
        &["claude-sonnet-4-5"],
        default_limits(),
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "the upstream's own answer");
    assert_eq!(
        credentials_seen(&up),
        [A],
        "the token the upstream rejected is not sent a second time"
    );
    assert_eq!(
        up.requests()
            .iter()
            .filter(|r| r.line.starts_with("POST /v1/oauth/token"))
            .count(),
        1,
        "one refresh was tried and none followed it"
    );
    // A mark's write is asynchronous, so this window bounds how long one would have to appear rather than
    // proving none is coming.
    stays_for(Duration::from_millis(200), || {
        h.store.account("a").unwrap().unwrap().state == AccountState::Ready
    })
    .await;
    assert_eq!(h.router.pins().await.unwrap().len(), 1, "the pin stays");
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
