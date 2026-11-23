mod common;
use std::time::Duration;

use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule, SseEvent};
use http_body_util::BodyExt;
use phoebis_bus::{Bus, Delivery};
use phoebis_ledger::{Assembler, Record};
use phoebis_oauth::AcquireError;
use phoebis_relay::ProbeObstacle;
use phoebis_routing::{
    FactValue, MoveCause, Report, RouterConfig, RouterEvent, SharePricer, UtilizationSample,
    WindowReading, WindowSource, FIVE_HOUR, SEVEN_DAY,
};
use phoebis_store::{AccountState, CooldownScope, Failover, PinKey, StateCause, SubscriptionRow};
use tokio::sync::broadcast;
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

/// Every event already on the broadcast: a selection's decision goes out before its answer reaches the relay.
fn already_sent(events: &mut broadcast::Receiver<RouterEvent>) -> Vec<RouterEvent> {
    std::iter::from_fn(|| events.try_recv().ok()).collect()
}

#[tokio::test]
async fn a_failover_s_second_decision_names_attempt_two_and_seq_two() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    up.add_credential_rule(A, "/v1/messages", json_429_unified(RESET, "five_hour"));
    let h = two_accounts(&up).await;
    let mut events = h.router.subscribe_events();
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    let o = outcome(&resp);
    assert_eq!(
        (resp.status().as_u16(), o.account_id.as_str(), o.attempt),
        (200, "b", 2)
    );
    // The body first and every event after it through the cache map: the body task is spawned, so a drain taken
    // before it ends could swallow the cache map's event.
    resp.into_body().collect().await.unwrap();
    let written = router_events_until(&mut events, |event| {
        matches!(event, RouterEvent::CacheMapWritten { request_id, .. } if *request_id == o.request_id)
    })
    .await;
    assert_eq!(
        decisions_of(&written, &o.request_id)
            .iter()
            .map(|(_, record)| (
                record.request.attempt,
                record.request.seq,
                record.outcome.account_id.as_deref()
            ))
            .collect::<Vec<_>>(),
        [(1, 1, Some("a")), (2, 2, Some("b"))]
    );
    assert!(
        matches!(
            written.last(),
            Some(RouterEvent::CacheMapWritten {
                seq: 2,
                attempt: 2,
                ..
            })
        ),
        "the answer's rows reach the decision that selected b"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_failover_after_a_hold_names_the_hold_on_the_attempt_that_waited_alone() {
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(A, "/v1/messages", json_429_unified(RESET, "five_hour"));
    up.add_credential_rule(B, "/v1/messages", json_200());
    let h = harness(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec {
            failover: Failover {
                in_flight_cap: 1,
                retry_in_place_seconds: 10,
                ..Failover::default()
            },
            ..PoolSpec::default()
        },
    )
    .await;
    let mut events = h.router.subscribe_events();
    // Both members at their cap of one, so the selection holds the request on the strategy's pick.
    let a_busy = h.router.in_flight().dispatched("a", NOW);
    let b_busy = h.router.in_flight().dispatched("b", NOW);
    let app = h.app.clone();
    let request = tokio::spawn(async move {
        app.oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
            .await
            .unwrap()
    });
    let mut seen = router_events_until(
        &mut events,
        |event| matches!(event, RouterEvent::Decision { summary, .. } if summary.kind == "held"),
    )
    .await;
    drop(b_busy);
    drop(a_busy);
    let resp = tokio::time::timeout(Duration::from_secs(5), request)
        .await
        .expect("a freed slot ends the hold")
        .unwrap();
    let o = outcome(&resp);
    assert_eq!(
        (resp.status().as_u16(), o.account_id.as_str(), o.attempt),
        (200, "b", 2)
    );
    seen.extend(already_sent(&mut events));
    assert_eq!(
        decisions_of(&seen, &o.request_id)
            .iter()
            .map(|(summary, record)| (
                record.request.seq,
                record.request.attempt,
                summary.kind,
                summary.by.kind,
                summary.by.value.as_str()
            ))
            .collect::<Vec<_>>(),
        [
            (1, 1, "held", "hold", "strategy_pick"),
            (2, 1, "placed", "hold", "strategy_pick"),
            (3, 2, "placed", "strategy", "priority")
        ],
        "the wait explains the attempt that waited; the failover's attempt takes its own placement's By"
    );
    up.shutdown().await;
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
    let mut events = h.router.subscribe_events();
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
    let seen = router_events_until(
        &mut events,
        |event| matches!(event, RouterEvent::PinMigrated { pin: PinKey { session_id, .. }, .. } if session_id == "s1"),
    )
    .await;
    match seen.last() {
        Some(RouterEvent::PinMigrated {
            from_account,
            reason,
            cause,
            ..
        }) => assert_eq!(
            (from_account.as_str(), reason.as_str(), cause),
            (
                "a",
                "cooldown",
                &Some(MoveCause::Cooldown {
                    scope: "account".into(),
                    reason: "rate_limit".into(),
                    window: Some("five_hour".into()),
                })
            ),
            "the move names the cooldown's row as its cause"
        ),
        other => panic!("{other:?}"),
    }
    let cooldowns = h.router.cooldowns().await.unwrap();
    assert_eq!(
        (
            cooldowns[0].account_id.as_str(),
            cooldowns[0].scope.to_string().as_str(),
            cooldowns[0].until,
            cooldowns[0].window.as_str()
        ),
        ("a", "account", RESET * 1_000 + 1_000, "five_hour")
    );
    up.set_credential_rule_after(A, 1, "/v1/messages", json_200());
    h.router
        .clear_cooldown("a", CooldownScope::Account)
        .await
        .unwrap();
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
        h.router
            .clear_cooldown(id, CooldownScope::Account)
            .await
            .unwrap();
    }
    for id in ["a", "c"] {
        h.store
            .set_state(id, AccountState::ReauthRequired, StateCause::Operator)
            .unwrap();
    }
    // Marked through the relay's own report, so each funding mark carries a retry that is not due yet: a mark the
    // routing state holds no instant for is tried at once, as after a restart.
    for id in ["b", "d"] {
        h.router.report(funding_mark(&h, id)).await;
        stored_with(&h, id, AccountState::FundingRequired).await;
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
    h.store
        .set_state("d", AccountState::Ready, StateCause::Operator)
        .unwrap();
    h.router.reload().await.unwrap();
    h.router
        .report(phoebis_routing::Report::Cooldown {
            account_id: "d".into(),
            row: phoebis_store::CooldownRow {
                account_id: "d".into(),
                scope: CooldownScope::Account,
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
        })
        .await;
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
async fn a_lookup_that_fails_at_a_pool_miss_fails_over_to_the_next_account() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(
        &up,
        &[oauth("first", "tok-1", 1), oauth("second", "tok-2", 2)],
        PoolSpec::default(),
    )
    .await;
    // No connection of either account stands in the pool yet, so the first account's checkout looks the host up.
    h.failing_lookups
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(credentials_seen(&up), ["tok-2"]);
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
    h.router
        .report(phoebis_routing::Report::Cooldown {
            account_id: "a".into(),
            row: phoebis_store::CooldownRow {
                account_id: "a".into(),
                scope: CooldownScope::Model("claude-sonnet-4-5".into()),
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
        })
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
            .any(|c| c.scope == CooldownScope::Model("claude-sonnet-4-5-20250929".into()))
            .then_some(rows)
    })
    .await;
    assert_eq!(
        (
            cooldowns[0].scope.to_string().as_str(),
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
            headers: vec![("retry-after".into(), "90".into())],
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
            .any(|row| {
                row.account_id == "k1"
                    && row.scope == CooldownScope::Model("claude-sonnet-4-5-20250929".into())
            })
            .then_some(rows)
    })
    .await;
    let k1 = c.iter().find(|c| c.account_id == "k1").unwrap();
    assert_eq!(
        (k1.scope.to_string().as_str(), k1.until),
        ("model:claude-sonnet-4-5-20250929", NOW + 90_000 + 1_000),
        "an api_key 429 with Retry-After cools the model"
    );
    assert!(
        c.iter()
            .any(|c| c.account_id == "b" && c.scope == CooldownScope::Account),
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
            && c.scope == CooldownScope::Account
            && c.until == NOW + 60_000 + 1_000),
        "a malformed Retry-After takes the account-wide fallback"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_model_only_rejection_leaves_opus_eligible_and_a_5h_rejection_cools_every_model() {
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
        (c[0].scope.to_string().as_str(), c[0].window.as_str()),
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
async fn a_model_week_rejection_on_an_account_without_the_five_hour_window_cools_the_model_alone() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    // The head an account without a 5-hour window answers at its Fable week's wall: no 5-hour family, the week under
    // its limit, the overage week rejected and spent.
    let header = |name: &str, value: &str| {
        (
            format!("anthropic-ratelimit-unified-{name}"),
            value.to_string(),
        )
    };
    up.add_credential_rule(
        A,
        "/v1/messages",
        Respond::Json {
            status: 429,
            headers: vec![
                header("status", "rejected"),
                header("representative-claim", "seven_day_overage_included"),
                header("7d-utilization", "0.55"),
                header("7d_oi-status", "rejected"),
                header("7d_oi-utilization", "1.0"),
                header("7d_oi-reset", "90000"),
            ],
            body: b"{}".to_vec(),
        },
    );
    up.set_credential_rule_after(A, 1, "/v1/messages", json_200());
    let h = two_accounts(&up).await;
    // a's clean poll carried the week alone, which settles its window set to the 7-day window.
    h.router
        .report(Report::Sample {
            account_id: "a".into(),
            sample: UtilizationSample {
                observed_at: NOW - 1_000,
                poll: true,
                ..UtilizationSample::default()
            }
            .with(
                SEVEN_DAY,
                WindowReading {
                    utilization: 0.55,
                    reset: None,
                    observed_at: NOW - 1_000,
                    source: WindowSource::Poll,
                    plan: None,
                    tier: None,
                },
            ),
            incarnation: 1,
            source_request_id: None,
        })
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
        (c[0].scope.to_string().as_str(), c[0].window.as_str()),
        ("model:claude-fable-5-1", "seven_day_overage_included"),
        "the rejection of the week cools Fable on a and no other model"
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
        "Opus stays on the account"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_spend_cap_429_on_a_fable_request_answers_the_exhausted_pool_with_the_overage_included_claim(
) {
    // The header set an organization at its spend cap answers, as observed: both shared windows healthy, the
    // overage week rejected under its own claim.
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 429,
            headers: vec![
                (
                    "anthropic-ratelimit-unified-status".into(),
                    "rejected".into(),
                ),
                (
                    "anthropic-ratelimit-unified-representative-claim".into(),
                    "seven_day_overage_included".into(),
                ),
                (
                    "anthropic-ratelimit-unified-5h-utilization".into(),
                    "0.00".into(),
                ),
                (
                    "anthropic-ratelimit-unified-7d-status".into(),
                    "allowed".into(),
                ),
                (
                    "anthropic-ratelimit-unified-7d-utilization".into(),
                    "0.69".into(),
                ),
                (
                    "anthropic-ratelimit-unified-7d_oi-status".into(),
                    "rejected".into(),
                ),
                (
                    "anthropic-ratelimit-unified-overage-disabled-reason".into(),
                    "org_spend_cap_reached".into(),
                ),
                ("retry-after".into(), "121180".into()),
            ],
            body: b"{}".to_vec(),
        },
    }])
    .await;
    let h = harness_with(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
        default_limits(),
    )
    .await;
    h.app
        .clone()
        .oneshot(request_for(KEY, "claude-fable-5-1", "s1"))
        .await
        .unwrap();
    let c = h.router.cooldowns().await.unwrap();
    assert_eq!(
        (c[0].scope.to_string().as_str(), c[0].window.as_str()),
        ("model:claude-fable-5-1", "seven_day_overage_included"),
        "the model row keeps the upstream's claim"
    );
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-fable-5-1", "s2"))
        .await
        .unwrap();
    assert_eq!(
        (
            resp.status().as_u16(),
            resp.headers()
                .get("anthropic-ratelimit-unified-representative-claim")
                .map(|v| v.to_str().unwrap())
        ),
        (429, Some("seven_day_overage_included")),
        "the exhausted pool answers with a claim the client resumes on"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn an_api_key_401_marks_the_account_with_the_verdict_s_cause_and_the_upstream_s_message() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    up.add_credential_rule(
        "sk-ant-api03-KEY1",
        "/v1/messages",
        Respond::Json {
            status: 401,
            headers: vec![],
            body: br#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#
                .to_vec(),
        },
    );
    let h = harness(
        &up,
        &[api_key("k1", "sk-ant-api03-KEY1", 0), oauth("b", B, 1)],
        PoolSpec::default(),
    )
    .await;
    let mut events = h.router.subscribe_events();
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
    // The mark travels through the router task and its writer, so its event lands after the client's response.
    let seen = router_events_until(&mut events, |event| {
        matches!(event, RouterEvent::AccountStateChanged { .. })
    })
    .await;
    match seen.last() {
        Some(RouterEvent::AccountStateChanged {
            account_id,
            to,
            cause,
            detail,
            ..
        }) => assert_eq!(
            (account_id.as_str(), *to, *cause, detail.as_deref()),
            (
                "k1",
                AccountState::ReauthRequired,
                StateCause::AuthFailed,
                Some("invalid x-api-key")
            )
        ),
        other => panic!("the moved row's event, not {other:?}"),
    }
    up.shutdown().await;
}

/// The account's state and the word its writer stored with it, once the row reads `state`.
async fn stored_with(h: &Harness, id: &str, state: AccountState) -> StateCause {
    wait_for(Duration::from_secs(5), || {
        h.store
            .account(id)
            .unwrap()
            .filter(|row| row.state == state)
            .map(|row| row.state_cause)
    })
    .await
}

#[tokio::test]
async fn an_api_key_401_stores_auth_failed_with_its_mark() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    up.add_credential_rule("sk-ant-api03-KEY1", "/v1/messages", invalid_key());
    let h = harness(
        &up,
        &[api_key("k1", "sk-ant-api03-KEY1", 0), oauth("b", B, 1)],
        PoolSpec::default(),
    )
    .await;
    h.app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(
        stored_with(&h, "k1", AccountState::ReauthRequired).await,
        StateCause::AuthFailed
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_credit_balance_refusal_stores_funding_refused() {
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
    h.app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(
        stored_with(&h, "k1", AccountState::FundingRequired).await,
        StateCause::FundingRefused
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_credit_balance_refusal_moves_the_pin_with_the_marks_state_as_its_cause() {
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
    let mut events = h.router.subscribe_events();
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
    let seen = router_events_until(
        &mut events,
        |event| matches!(event, RouterEvent::PinMigrated { pin: PinKey { session_id, .. }, .. } if session_id == "s1"),
    )
    .await;
    match seen.last() {
        Some(RouterEvent::PinMigrated {
            from_account,
            reason,
            cause,
            ..
        }) => assert_eq!(
            (from_account.as_str(), reason.as_str(), cause),
            (
                "k1",
                "mark",
                &Some(MoveCause::Mark {
                    state: "funding_required",
                    cause: "funding_refused",
                })
            ),
            "the mark the relay reported travels with the move"
        ),
        other => panic!("{other:?}"),
    }
    up.shutdown().await;
}

/// The relay's credit-balance mark on `id`'s current holder, as a failover reports it.
fn funding_mark(h: &Harness, id: &str) -> phoebis_routing::Report {
    let snapshot = h.router.snapshot();
    let account = snapshot.account(id).unwrap();
    phoebis_routing::Report::AccountState {
        account_id: id.into(),
        state: AccountState::FundingRequired,
        incarnation: account.incarnation,
        lifecycle_generation: account.lifecycle_generation,
        credential_generation: snapshot.credential_generation(id).unwrap_or(1),
        cause: StateCause::FundingRefused,
        detail: None,
        dispatched_ordinal: None,
    }
}

/// Marks `id` `funding_required` in the registry alone, as a restart reads the mark: no retry instant.
async fn marked_in_the_registry(h: &Harness, id: &str) {
    h.store
        .set_state(
            id,
            AccountState::FundingRequired,
            StateCause::FundingRefused,
        )
        .unwrap();
    h.router.reload().await.unwrap();
}

/// The router's retry instant for `id` once it reads `at`.
async fn retry_at_reads(h: &Harness, id: &str, at: i64) {
    wait_for_async(Duration::from_secs(5), || async {
        h.router
            .funding_retries()
            .await
            .unwrap()
            .contains(&(id.to_string(), at))
            .then_some(())
    })
    .await;
}

#[tokio::test]
async fn a_funding_retry_that_answers_200_clears_the_mark_with_the_funding_retry_cause() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(
        &up,
        &[api_key("k1", "sk-ant-api03-KEY1", 0), oauth("b", B, 1)],
        PoolSpec::default(),
    )
    .await;
    marked_in_the_registry(&h, "k1").await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(
        (resp.status().as_u16(), outcome(&resp).account_id.as_str()),
        (200, "k1"),
        "the marked account is tried again"
    );
    assert_eq!(
        stored_with(&h, "k1", AccountState::Ready).await,
        StateCause::FundingRetry
    );
    assert!(h.router.funding_retries().await.unwrap().is_empty());
    up.shutdown().await;
}

#[tokio::test]
async fn a_second_credit_balance_refusal_marks_the_account_again_with_a_later_retry() {
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
    assert_eq!(outcome(&resp).account_id, "b");
    retry_at_reads(&h, "k1", NOW + 300_000).await;
    wait_for(Duration::from_secs(5), || {
        h.router
            .snapshot()
            .account("k1")
            .filter(|a| a.state == AccountState::FundingRequired)
            .map(|_| ())
    })
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s2"))
        .await
        .unwrap();
    assert_eq!(
        (
            outcome(&resp).account_id.as_str(),
            credentials_seen(&up).len()
        ),
        ("b", 3),
        "the retry is not due, so the second request goes to b alone"
    );
    h.clock
        .store(NOW + 300_000, std::sync::atomic::Ordering::SeqCst);
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s3"))
        .await
        .unwrap();
    assert_eq!(outcome(&resp).account_id, "b");
    assert_eq!(
        credentials_seen(&up)
            .iter()
            .filter(|c| c.as_str() == "sk-ant-api03-KEY1")
            .count(),
        2,
        "the due retry tried the account once more"
    );
    retry_at_reads(&h, "k1", NOW + 600_000).await;
    assert_eq!(
        stored_with(&h, "k1", AccountState::FundingRequired).await,
        StateCause::FundingRefused
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_funding_retry_whose_head_is_held_keeps_the_next_request_off_the_account_until_it_answers(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    up.add_credential_rule(
        "sk-ant-api03-KEY1",
        "/v1/messages",
        Respond::Gated(Box::new(json_200())),
    );
    let h = harness(
        &up,
        &[api_key("k1", "sk-ant-api03-KEY1", 0), oauth("b", B, 1)],
        PoolSpec::default(),
    )
    .await;
    marked_in_the_registry(&h, "k1").await;
    let app = h.app.clone();
    let first = tokio::spawn(async move {
        app.oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
            .await
            .unwrap()
    });
    wait_for(Duration::from_secs(5), || {
        credentials_seen(&up)
            .contains(&"sk-ant-api03-KEY1".to_string())
            .then_some(())
    })
    .await;
    // Past the relay's body read bound: the claim lasts as long as its attempt does.
    h.clock
        .fetch_add(61_000, std::sync::atomic::Ordering::SeqCst);
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s2"))
        .await
        .unwrap();
    assert_eq!(
        (resp.status().as_u16(), outcome(&resp).account_id.as_str()),
        (200, "b"),
        "the held retry still holds the one slot, on a pool whose cap is above one"
    );
    up.release();
    let resp = first.await.unwrap();
    assert_eq!(
        (resp.status().as_u16(), outcome(&resp).account_id.as_str()),
        (200, "k1")
    );
    assert_eq!(
        stored_with(&h, "k1", AccountState::Ready).await,
        StateCause::FundingRetry
    );
    up.shutdown().await;
}

/// The upstream's credit-balance refusal, as an `api_key` account reads it.
fn credit_balance_refusal() -> Respond {
    Respond::Json {
        status: 400,
        headers: vec![],
        body: br#"{"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API."}}"#.to_vec(),
    }
}

/// Marks `id` through the relay's own report and steps the clock to the retry it stamps, so the next selection may
/// place a request there and its entry shows whether a clearing landed.
async fn marked_and_due(h: &Harness, id: &str) {
    h.router.report(funding_mark(h, id)).await;
    retry_at_reads(h, id, NOW + 300_000).await;
    wait_for(Duration::from_secs(5), || {
        h.router
            .snapshot()
            .account(id)
            .filter(|a| a.state == AccountState::FundingRequired)
            .map(|_| ())
    })
    .await;
    h.clock
        .store(NOW + 300_000, std::sync::atomic::Ordering::SeqCst);
}

/// The registry's state and cause for `id`.
fn stored(h: &Harness, id: &str) -> Option<(AccountState, StateCause)> {
    h.store
        .account(id)
        .unwrap()
        .map(|row| (row.state, row.state_cause))
}

#[tokio::test]
async fn a_token_count_answered_200_on_a_marked_account_leaves_the_mark_standing() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(
        &up,
        &[api_key("k1", "sk-ant-api03-KEY1", 0), oauth("b", B, 1)],
        PoolSpec::default(),
    )
    .await;
    marked_and_due(&h, "k1").await;
    let resp = h
        .app
        .clone()
        .oneshot(request_from_fixture(
            phoebis_profile::fixtures::inference_head(
                phoebis_profile::fixtures::FixtureKind::CountApiKey,
            ),
            phoebis_profile::fixtures::body(phoebis_profile::fixtures::BodyFixture::Count),
            KEY,
        ))
        .await
        .unwrap();
    assert_eq!(
        (resp.status().as_u16(), outcome(&resp).account_id.as_str()),
        (200, "k1"),
        "the due retry admits the count"
    );
    // Asked after the answer, so a report the count sent has landed on the loop by now.
    assert_eq!(
        h.router.funding_retries().await.unwrap(),
        vec![("k1".to_string(), NOW + 300_000)],
        "a counting answer says nothing about the balance, so nothing clears the mark"
    );
    assert_eq!(
        stored(&h, "k1"),
        Some((AccountState::FundingRequired, StateCause::FundingRefused))
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_held_stream_that_commits_on_an_error_event_leaves_the_mark_standing() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    up.add_credential_rule(
        "sk-ant-api03-KEY1",
        "/v1/messages",
        Respond::Sse {
            status: 200,
            headers: vec![],
            events: vec![fake_upstream::SseEvent {
                event: "error".into(),
                data: r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#
                    .into(),
            }],
            delay_between: Duration::from_millis(1),
        },
    );
    let h = harness(
        &up,
        &[api_key("k1", "sk-ant-api03-KEY1", 0), oauth("b", B, 1)],
        PoolSpec::default(),
    )
    .await;
    marked_and_due(&h, "k1").await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(
        (resp.status().as_u16(), outcome(&resp).account_id.as_str()),
        (200, "k1"),
        "the error event committed under the 200 head"
    );
    let _ = resp.into_body().collect().await.unwrap();
    assert_eq!(
        h.router.funding_retries().await.unwrap(),
        vec![("k1".to_string(), NOW + 300_000)],
        "an error event is no success, whatever the head's status"
    );
    assert_eq!(
        stored(&h, "k1"),
        Some((AccountState::FundingRequired, StateCause::FundingRefused))
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_success_from_a_retry_sent_before_the_newest_mark_leaves_that_mark_standing() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    up.push_credential_rule(
        "sk-ant-api03-KEY1",
        "/v1/messages",
        Respond::Gated(Box::new(json_200())),
    );
    up.push_credential_rule(
        "sk-ant-api03-KEY1",
        "/v1/messages",
        credit_balance_refusal(),
    );
    let h = harness(
        &up,
        &[api_key("k1", "sk-ant-api03-KEY1", 0), oauth("b", B, 1)],
        PoolSpec::default(),
    )
    .await;
    marked_in_the_registry(&h, "k1").await;
    let app = h.app.clone();
    let retry = tokio::spawn(async move {
        app.oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
            .await
            .unwrap()
    });
    wait_for(Duration::from_secs(5), || {
        credentials_seen(&up)
            .contains(&"sk-ant-api03-KEY1".to_string())
            .then_some(())
    })
    .await;
    // The operator clears funding while the retry's head is held, and the reload starts after the claim's stamp.
    h.clock
        .store(NOW + 1_000, std::sync::atomic::Ordering::SeqCst);
    h.store
        .set_state("k1", AccountState::Ready, StateCause::Operator)
        .unwrap();
    h.router.reload().await.unwrap();
    assert!(h.router.funding_retries().await.unwrap().is_empty());
    h.clock
        .store(NOW + 2_000, std::sync::atomic::Ordering::SeqCst);
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s2"))
        .await
        .unwrap();
    assert_eq!(
        outcome(&resp).account_id,
        "b",
        "the ready account was refused over its balance and the request failed over"
    );
    retry_at_reads(&h, "k1", NOW + 302_000).await;
    assert_eq!(
        stored_with(&h, "k1", AccountState::FundingRequired).await,
        StateCause::FundingRefused
    );
    up.release();
    let resp = retry.await.unwrap();
    assert_eq!(
        (resp.status().as_u16(), outcome(&resp).account_id.as_str()),
        (200, "k1"),
        "the older retry still answers its caller"
    );
    assert_eq!(
        h.router.funding_retries().await.unwrap(),
        vec![("k1".to_string(), NOW + 302_000)],
        "the retry went out before the refusal behind the newest mark, so that mark stands"
    );
    assert_eq!(
        stored(&h, "k1"),
        Some((AccountState::FundingRequired, StateCause::FundingRefused))
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_refused_replay_stores_auth_failed_and_the_ladder_s_mark_refresh_failed() {
    // The replay on the freshly refreshed token is refused too: the relay's own mark.
    let up = FakeUpstream::start(vec![
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: expired(),
        },
        Rule {
            path_prefix: "/v1/oauth/token".into(),
            respond: token_answer(),
        },
    ])
    .await;
    let h = harness(&up, &[paired("a", A)], PoolSpec::default()).await;
    h.app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(
        stored_with(&h, "a", AccountState::ReauthRequired).await,
        StateCause::AuthFailed
    );
    up.shutdown().await;

    // The refresh ends in `invalid_grant`: the manager's write names it, and the relay's re-report of that mark
    // carries the same word, so the reload it leads to leaves the word as the manager stored it.
    let up = FakeUpstream::start(vec![
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: expired(),
        },
        Rule {
            path_prefix: "/v1/oauth/token".into(),
            respond: Respond::Json {
                status: 400,
                headers: vec![],
                body: br#"{"error":"invalid_grant","error_description":"revoked"}"#.to_vec(),
            },
        },
    ])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    h.app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    wait_for(Duration::from_secs(5), || {
        h.router
            .snapshot()
            .account("a")
            .filter(|row| row.state == AccountState::ReauthRequired)
            .map(|_| ())
    })
    .await;
    let row = h.store.account("a").unwrap().unwrap();
    assert_eq!(
        (row.state, row.state_cause),
        (AccountState::ReauthRequired, StateCause::RefreshFailed)
    );
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
async fn deleting_the_account_holding_an_in_flight_request_answers_account_canceled_and_closes_the_upstream(
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
        "account_canceled"
    );
    assert_eq!(
        credentials_seen(&up),
        [A],
        "an answer about a credential that is gone is nothing to fail over from"
    );
    // The attempt's future owns the upstream connection, so canceling it takes the socket with it.
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
        (
            o.client_drift.map(|kind| kind.as_str()),
            o.client_version.as_deref()
        ),
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

/// Commits a bundle for `id` inside its refresh margin and drops the manager's cached one, as a pairing does, so the
/// account's next acquisition goes to the token endpoint.
fn due_for_refresh(h: &Harness, id: &str) {
    let stored = h.store.credentials(id).unwrap().unwrap();
    h.store
        .commit_credentials(
            id,
            1,
            1,
            Some(stored.generation),
            None,
            &phoebis_store::NewBundle {
                access_token: phoebis_store::Secret::new("sk-ant-oat01-SHORT"),
                refresh_token: Some(phoebis_store::Secret::new("sk-ant-oat01-SHORT-refresh")),
                issued_at: NOW - 3_600_000,
                expires_at: NOW + 60_000,
                refresh_expires_at: None,
                scopes: "user:inference".into(),
            },
        )
        .unwrap();
    h.manager.invalidate(id);
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

/// A re-authentication mark the refresh lands is authoritative over the credential cache and the routing snapshot
/// alike: the next request selects past the marked account rather than dialing it on the token the cache still holds.
#[tokio::test]
async fn a_marked_account_is_not_dialed_again_on_its_cached_token() {
    let up = FakeUpstream::start(vec![
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: expired(),
        },
        Rule {
            path_prefix: "/v1/oauth/token".into(),
            respond: Respond::Json {
                status: 400,
                headers: vec![],
                body: br#"{"error":"invalid_grant","error_description":"revoked"}"#.to_vec(),
            },
        },
    ])
    .await;
    // The first leg's 401 alone; every inference after it answers 200.
    up.set_rule_after(1, "/v1/messages", json_200());
    let h = two_accounts(&up).await;
    // The row and generation a leg selected before the mark carries into its acquisition.
    let before = h.router.snapshot();
    let (row_before, generation_before) = (
        before.account("a").unwrap().clone(),
        before.credential_generation("a"),
    );
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "the request fails over to b");
    resp.into_body().collect().await.unwrap();
    wait_for(Duration::from_secs(5), || {
        (h.store.account("a").unwrap().unwrap().state == AccountState::ReauthRequired).then_some(())
    })
    .await;
    // The relay's report, the routing writer's fenced write and the reload after it take the snapshot's row off
    // `ready`, so the next selection skips the account whatever the cache holds.
    wait_for(Duration::from_secs(5), || {
        (h.router.snapshot().account("a").unwrap().state == AccountState::ReauthRequired)
            .then_some(())
    })
    .await;
    // The usage poll's or the catalog's refresh lands the same mark with no relay report behind it, so the cache's
    // own refusal is what stops a leg that still carries the pre-mark row: refused, and without reading the store.
    let reads = h.store.debug_credential_reads();
    assert!(
        matches!(
            h.manager.acquire_for(&row_before, generation_before).await,
            Err(AcquireError::ReauthRequired(..))
        ),
        "the pre-mark row is refused by the cache"
    );
    assert_eq!(
        h.store.debug_credential_reads(),
        reads,
        "the refusal reads nothing from the store"
    );
    assert_eq!(credentials_seen(&up), [A, B]);
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s2"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        outcome(&resp).account_id,
        "b",
        "the marked account is not selected"
    );
    resp.into_body().collect().await.unwrap();
    assert_eq!(
        credentials_seen(&up),
        [A, B, B],
        "nothing dials the marked account on its cached token"
    );
    assert_eq!(
        token_posts(&up),
        1,
        "the marked account is not refreshed again"
    );
    up.shutdown().await;
}

/// A replay the upstream refuses too marks the account, and the credential cache refuses the replayed lineage from
/// that answer on: a leg that still carries the pre-mark row is not handed the token the upstream just refused.
#[tokio::test]
async fn a_refused_replay_refuses_the_replayed_lineage_in_the_credential_cache() {
    let up = FakeUpstream::start(vec![
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: expired(),
        },
        Rule {
            path_prefix: "/v1/oauth/token".into(),
            respond: token_answer(),
        },
    ])
    .await;
    up.add_credential_rule(B, "/v1/messages", json_200());
    let h = harness(
        &up,
        &[paired("a", A), oauth("b", B, 1)],
        PoolSpec::default(),
    )
    .await;
    let before = h.router.snapshot();
    let (row_before, generation_before) = (
        before.account("a").unwrap().clone(),
        before.credential_generation("a"),
    );
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "the request fails over to b");
    resp.into_body().collect().await.unwrap();
    assert_eq!(token_posts(&up), 1, "one refresh, then the replay");
    let replayed = h.store.credentials("a").unwrap().unwrap().generation;
    assert!(generation_before.is_some_and(|before| replayed > before));
    let seen = credentials_seen(&up);
    assert_eq!(
        (seen.len(), seen[0].as_str(), seen[2].as_str()),
        (3, A, B),
        "a's token, the replayed token, then b's"
    );
    let reads = h.store.debug_credential_reads();
    for expected in [generation_before, Some(replayed)] {
        assert!(
            matches!(
                h.manager.acquire_for(&row_before, expected).await,
                Err(AcquireError::ReauthRequired(..))
            ),
            "the pre-mark row is refused at {expected:?}, the replayed token with it"
        );
    }
    assert_eq!(
        h.store.debug_credential_reads(),
        reads,
        "the refusal reads nothing from the store"
    );
    up.shutdown().await;
}

/// The relay's own refresh at dispatch lands the reauth mark as the ladder's does, and reports it as the ladder's is
/// reported: routing reads the account `reauth_required` without a reload, and nothing dials it again.
#[tokio::test]
async fn a_dispatch_whose_refresh_lands_a_reauth_mark_reports_it_to_routing_at_once() {
    let up = FakeUpstream::start(vec![
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: json_200(),
        },
        Rule {
            path_prefix: "/v1/oauth/token".into(),
            respond: invalid_grant(),
        },
    ])
    .await;
    let h = two_accounts(&up).await;
    due_for_refresh(&h, "a");
    // The row and generation a leg selected before the mark carries into its acquisition.
    let before = h.router.snapshot();
    let (row_before, generation_before) = (
        before.account("a").unwrap().clone(),
        before.credential_generation("a"),
    );
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(
        (resp.status().as_u16(), outcome(&resp).account_id.as_str()),
        (200, "b"),
        "the request fails over to b"
    );
    resp.into_body().collect().await.unwrap();
    assert_eq!(
        h.store.account("a").unwrap().unwrap().state,
        AccountState::ReauthRequired,
        "the dispatch's refresh landed the mark"
    );
    // The manager's mark is a store write alone, which starts no registry reload, so only a report reaches the
    // snapshot.
    wait_for(Duration::from_secs(5), || {
        (h.router.snapshot().account("a").unwrap().state == AccountState::ReauthRequired)
            .then_some(())
    })
    .await;
    let reads = h.store.debug_credential_reads();
    assert!(
        matches!(
            h.manager.acquire_for(&row_before, generation_before).await,
            Err(AcquireError::ReauthRequired(..))
        ),
        "the pre-mark row is refused by the cache"
    );
    assert_eq!(
        h.store.debug_credential_reads(),
        reads,
        "the refusal reads nothing from the store"
    );
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s2"))
        .await
        .unwrap();
    assert_eq!(
        outcome(&resp).account_id,
        "b",
        "the marked account is not selected"
    );
    resp.into_body().collect().await.unwrap();
    assert_eq!(
        credentials_seen(&up),
        [B, B],
        "nothing dials the marked account"
    );
    assert_eq!(
        token_posts(&up),
        1,
        "the marked account is not refreshed again"
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
        (503, "account_canceled")
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

/// The engine's own probe under the harness key, its recipe last answered on `a`, where each case's pin stands.
fn probe_of(h: &Harness) -> phoebis_relay::Probe {
    phoebis_relay::Probe {
        key_id: "desktop".into(),
        key_generation: h.store.api_key("desktop").unwrap().unwrap().created_at,
        holder: phoebis_routing::Holder {
            account_id: "a".into(),
            incarnation: h.store.account("a").unwrap().unwrap().incarnation,
        },
    }
}

/// The probe's answer as the keepalive reads it: the status, the code and the obstacle.
fn probe_answer(
    resp: &axum::http::Response<axum::body::Body>,
) -> (u16, String, Option<ProbeObstacle>) {
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
        (
            503,
            "accounts_unavailable".into(),
            Some(ProbeObstacle::ReauthRequired)
        ),
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
        (
            503,
            "accounts_unavailable".into(),
            Some(ProbeObstacle::ReauthRequired)
        ),
        "the refresh's own mark reaches the probe's answer"
    );
    up.shutdown().await;
}

/// A probe whose dispatch refresh lands its pinned account's reauth mark names the mark on its answer, as a probe whose
/// leg's rejection marked the account does.
#[tokio::test]
async fn a_probe_whose_dispatch_refresh_answers_invalid_grant_names_the_mark() {
    let up = FakeUpstream::start(vec![
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: json_200(),
        },
        Rule {
            path_prefix: "/v1/oauth/token".into(),
            respond: invalid_grant(),
        },
    ])
    .await;
    let h = two_accounts(&up).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(outcome(&resp).account_id, "a");
    resp.into_body().collect().await.unwrap();
    due_for_refresh(&h, "a");
    let probed = h
        .relay
        .probe(probe_of(&h), request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await;
    assert_eq!(
        probe_answer(&probed),
        (
            503,
            "accounts_unavailable".into(),
            Some(ProbeObstacle::ReauthRequired)
        ),
        "the dispatch's own mark reaches the probe's answer"
    );
    assert_eq!(token_posts(&up), 1);
    assert_eq!(credentials_seen(&up), [A], "the probe dials nothing");
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
        (
            503,
            "accounts_unavailable".into(),
            Some(ProbeObstacle::ReauthRequired)
        ),
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
            o.client_drift.map(|kind| kind.as_str()),
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
    // Committed through the store, so the reload is what moves the snapshot's generation past the bundle the
    // manager cached at the catalog's prefetch.
    h.router.reload().await.unwrap();
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
    rename(&h, "a", 1, "a (edited)", "test").unwrap();
    h.router.reload().await.unwrap();
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
async fn a_fast_answers_renewal_folds_its_usage_into_the_fast_copy_and_leaves_both_pin_copies_at_zero(
) {
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
    let fast = h
        .router
        .fast_burns()
        .await
        .unwrap()
        .get(&pin.key())
        .copied()
        .unwrap_or_default();
    assert!(
        fast.input > 0.0 && fast.output > 0.0,
        "the answer's usage folds into the pin's fast copy, saved on the row beside its plan copies: {fast:?}"
    );
    assert_eq!(
        (pin.burn, pin.burn_banded),
        (
            phoebis_store::PinBurn::default(),
            phoebis_store::PinBurn::default()
        ),
        "an answer reporting usage.speed fast prices at the fast cells, and the row's separate fast copy is why it \
         draws no plan window, so both of the row's plan copies fold nothing"
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
                holder: phoebis_routing::Holder {
                    account_id: "k1".into(),
                    incarnation: h.store.account("k1").unwrap().unwrap().incarnation,
                },
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

#[tokio::test]
async fn a_move_back_prices_the_thinking_each_account_produced_from_the_answers_the_relay_read() {
    let up = FakeUpstream::start(vec![]).await;
    // Ten hours past NOW in Unix seconds, so the spent rule moves the pin rather than waiting for the reset.
    let reset = (NOW / 1_000 + 10 * 3_600).to_string();
    let spent = |body: &str| Respond::Json {
        status: 200,
        headers: vec![
            (
                "anthropic-ratelimit-unified-5h-utilization".into(),
                "1.0".into(),
            ),
            ("anthropic-ratelimit-unified-5h-reset".into(), reset.clone()),
        ],
        body: body.as_bytes().to_vec(),
    };
    up.push_credential_rule(
        A,
        "/v1/messages",
        spent(
            r#"{"id":"msg_a01","type":"message","content":[],"usage":{"input_tokens":100000,"output_tokens":41000,"output_tokens_details":{"thinking_tokens":40000}}}"#,
        ),
    );
    up.push_credential_rule(
        B,
        "/v1/messages",
        spent(
            r#"{"id":"msg_b01","type":"message","content":[],"usage":{"input_tokens":120000,"output_tokens":11000,"output_tokens_details":{"thinking_tokens":10000}},"input_transformations":[{"type":"thinking_dropped","reason":"end_user_binding_mismatch","path":"messages.1"}]}"#,
        ),
    );
    up.push_credential_rule(A, "/v1/messages", ok_with_id("msg_a02"));
    // As `priced_harness` builds its own, with a write at either TTL priced, since the fixture's markers ask for an
    // hour.
    let accounts = [oauth("a", A, 0), oauth("b", B, 1)];
    let h = harness_with_router_config(
        &up,
        &accounts,
        PoolSpec::default(),
        RouterConfig {
            share: SharePricer::new(|_, _, tier, _, usage, _| {
                (tier == TIER)
                    .then(|| (usage.input + usage.write_5m + usage.write_1h) as f64 / 10_000_000.0)
            }),
            ..RouterConfig::default()
        },
    )
    .await;
    for account in &accounts {
        h.store
            .set_profile(
                account.id,
                "{}",
                &SubscriptionRow {
                    tier: Some(TIER.into()),
                    ..SubscriptionRow::default()
                },
            )
            .unwrap();
    }
    h.router.reload().await.unwrap();
    let poll = |account: &str, five_hour: f64, at: i64| {
        let reading = |utilization: f64, reset: i64| WindowReading {
            utilization,
            reset: Some(reset),
            observed_at: at,
            source: WindowSource::Poll,
            plan: None,
            tier: None,
        };
        Report::Sample {
            account_id: account.into(),
            sample: UtilizationSample::default()
                .with(FIVE_HOUR, reading(five_hour, at + 3 * 3_600_000))
                .with(SEVEN_DAY, reading(0.1, at + 72 * 3_600_000)),
            incarnation: 1,
            source_request_id: None,
        }
    };

    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(outcome(&resp).account_id, "a");
    resp.into_body().collect().await.unwrap();
    wait_for_async(Duration::from_secs(5), || async {
        h.router
            .pins()
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.last_input_tokens == 100_000)
    })
    .await;
    h.router.report(poll("b", 0.3, NOW)).await;
    // Past the poll, so b's head replaces its reading.
    h.clock
        .store(NOW + 30_000, std::sync::atomic::Ordering::SeqCst);

    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(
        outcome(&resp).account_id,
        "b",
        "the spent rule moves the pin off a"
    );
    resp.into_body().collect().await.unwrap();
    wait_for_async(Duration::from_secs(5), || async {
        h.router
            .pins()
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.last_input_tokens == 120_000)
    })
    .await;
    h.clock
        .store(NOW + 60_000, std::sync::atomic::Ordering::SeqCst);
    h.router.report(poll("a", 0.1, NOW + 60_000)).await;
    wait_for_async(Duration::from_secs(5), || async {
        let samples = h.router.samples().await.unwrap();
        let five_hour = |account: &str| {
            samples
                .iter()
                .find(|(id, _)| id == account)
                .and_then(|(_, sample)| sample.reading(FIVE_HOUR))
                .map(|reading| reading.utilization)
        };
        (five_hour("a") == Some(0.1) && five_hour("b") >= Some(1.0)).then_some(())
    })
    .await;

    let mut events = h.router.subscribe_events();
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    let moved = outcome(&resp);
    assert_eq!(
        moved.account_id, "a",
        "the spent rule moves the pin back off b"
    );
    let request_id = moved.request_id;
    resp.into_body().collect().await.unwrap();
    let seen = router_events_until(&mut events, |event| {
        matches!(event, RouterEvent::Decision { record, .. } if record.request.request_id == request_id)
    })
    .await;
    let (_, record) = decisions_of(&seen, &request_id)
        .pop()
        .expect("the move's decision");
    assert_eq!(
        (record.outcome.kind, record.outcome.rule),
        ("moved", Some("spent"))
    );
    let booked = record
        .outcome
        .reservation
        .as_ref()
        .and_then(|booked| booked.rewrite.get(FIVE_HOUR).copied());
    // 120,000 less b's 10,000 plus a's 40,000, a ten-millionth each.
    assert!(
        booked.is_some_and(|share| (share - 0.015).abs() < 1e-9),
        "{booked:?}"
    );
    up.shutdown().await;
}

/// The first-party fixture with a stamp this build does not compute, and with a version suffix that does not
/// reproduce over a body re-signed so its hash does: one body per drift, with the trigger each files.
fn drifted_first_party_bodies() -> [(Vec<u8>, &'static str); 2] {
    use phoebis_profile::fixtures::{body, BodyFixture};
    let fixture = String::from_utf8(body(BodyFixture::FirstParty).to_vec()).unwrap();
    let hash = fixture.replacen("cch=91984", "cch=91985", 1).into_bytes();
    let suffix = phoebis_cch::sign(
        fixture
            .replacen("cc_version=2.1.310.077", "cc_version=2.1.310.abc", 1)
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
            (403, "client_drift"),
            "{trigger}"
        );
        let o = outcome(&resp);
        assert_eq!(
            (
                o.account_id.as_str(),
                o.attempt,
                o.client_drift.map(|kind| kind.as_str())
            ),
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
            ("claude-sonnet-4-5", Some(FIRST_PARTY_SESSION), Some("main"))
        );
        assert!(up.requests().is_empty(), "{trigger}: nothing was dialled");
        up.shutdown().await;
    }
}

#[tokio::test]
async fn a_drift_refusal_emits_one_decision_with_the_client_trigger_and_version() {
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
        let mut events = h.router.subscribe_events();
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
        assert_eq!(resp.status().as_u16(), 403, "{trigger}");
        let id = outcome(&resp).request_id;
        // The record is try-sent ahead of the answer and lands on the loop's next turn.
        let mut seen = router_events_until(&mut events, |event| {
            matches!(event, RouterEvent::Decision { record, .. } if record.request.request_id == id)
        })
        .await;
        let bus_events = drain_until_complete(&mut sub).await;
        seen.extend(already_sent(&mut events));
        let decisions = decisions_of(&seen, &id);
        assert_eq!(decisions.len(), 1, "{trigger}: the request's one decision");
        let (summary, record) = &decisions[0];
        assert_eq!(
            (
                summary.kind,
                summary
                    .refusal
                    .as_ref()
                    .map(|r| (r.kind, r.code, r.reason.as_str()))
            ),
            ("refused", Some(("client", "drift", trigger)))
        );
        assert!(
            record.request.attempted.is_empty() && record.request.holds.is_empty(),
            "{trigger}: the refusal precedes any selection, so it names nothing tried or waited on"
        );
        assert_eq!(
            record.request.session_id.as_deref(),
            Some(FIRST_PARTY_SESSION),
            "{trigger}: the refusal names the session the request carried"
        );
        assert_eq!(
            summary
                .fact
                .iter()
                .map(|f| (f.key, &f.value))
                .collect::<Vec<_>>(),
            [("client", &FactValue::Text("2.1.310".into()))],
            "{trigger}: the version rides the summary's own fact"
        );
        assert_eq!(
            (
                record.request.request_id.as_str(),
                record.request.seq,
                record.request.attempt
            ),
            (id.as_str(), 1, 1)
        );
        assert!(
            !bus_events
                .iter()
                .any(|event| matches!(event, phoebis_bus::Event::AccountSelected(_))),
            "{trigger}: no account was selected"
        );
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
        .replacen("cc_version=2.1.310.077", "cc_version=2.1.310.abc", 1);
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
    let fixture = String::from_utf8(body(BodyFixture::FirstParty).to_vec()).unwrap();
    // A stamp this build does not compute, and one only the decoded walk reaches.
    let drifted = fixture.replacen("cch=91984", "cch=91985", 1);
    let escaped = fixture.replacen(
        r#""text":"x-anthropic-billing-header"#,
        r#""te\u0078t":"x-anthropic-billing-header"#,
        1,
    );
    let mut answers = Vec::new();
    for sent in [drifted, escaped] {
        assert_ne!(sent, fixture, "the edit reached the fixture's spelling");
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

/// A JSON rejection whose error type sits in its first bytes and whose message runs the body past the 64 KiB the relay
/// reads before it decides.
fn over_the_bound(error_type: &str, message: &str) -> Vec<u8> {
    format!(r#"{{"type":"error","error":{{"type":"{error_type}","message":"{message}"}}}}"#)
        .into_bytes()
}

/// `n` letters a compressor barely shrinks, so a gzip body built from them stays past the bound on the wire.
fn noise(n: usize) -> String {
    let mut state: u32 = 0x9e37_79b9;
    (0..n)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            char::from(b'a' + (state % 26) as u8)
        })
        .collect()
}

fn gzip(payload: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(payload)
        .expect("a Vec never fails a write");
    encoder.finish().expect("the encoder flushes into the Vec")
}

#[tokio::test]
async fn a_429_over_the_buffer_bound_is_classified_from_its_prefix_and_fails_over() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = two_accounts(&up).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(outcome(&resp).account_id, "a", "the session is pinned to a");
    resp.into_body().collect().await.unwrap();
    let Respond::Json { headers, .. } = json_429_unified(RESET, "five_hour") else {
        unreachable!()
    };
    let body = over_the_bound("rate_limit_error", &"x".repeat(70 * 1024));
    assert!(body.len() > 64 * 1024);
    up.add_credential_rule(
        A,
        "/v1/messages",
        Respond::Json {
            status: 429,
            headers,
            body,
        },
    );
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    let o = outcome(&resp);
    assert_eq!(
        (resp.status().as_u16(), o.account_id.as_str(), o.attempt),
        (200, "b", 2),
        "the 429 cooled its account and the request failed over"
    );
    assert_eq!(credentials_seen(&up), [A, A, B]);
    let cooldowns = h.router.cooldowns().await.unwrap();
    assert_eq!(
        cooldowns
            .iter()
            .map(|c| (c.account_id.as_str(), &c.scope, c.window.as_str()))
            .collect::<Vec<_>>(),
        [("a", &CooldownScope::Account, "five_hour")]
    );
    assert!(
        h.router
            .pins()
            .await
            .unwrap()
            .iter()
            .any(|p| p.session_id == "s1" && p.account_id == "b"),
        "the pin moved to the account that served"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_403_authentication_error_over_the_bound_takes_the_mark_path() {
    // A coded body is read as far as its first 64 KiB on the wire decode, so the gzip one is classified as well.
    for coding in [None, Some("gzip")] {
        let body = over_the_bound("authentication_error", &noise(160 * 1024));
        let (headers, body) = match coding {
            None => (vec![], body),
            Some(coding) => (
                vec![("Content-Encoding".to_string(), coding.to_string())],
                gzip(&body),
            ),
        };
        assert!(
            body.len() > 64 * 1024,
            "{coding:?}: past the bound on the wire"
        );
        let up = FakeUpstream::start(vec![
            Rule {
                path_prefix: "/v1/messages".into(),
                respond: Respond::Json {
                    status: 403,
                    headers,
                    body,
                },
            },
            Rule {
                path_prefix: "/v1/oauth/token".into(),
                respond: Respond::Json {
                    status: 400,
                    headers: vec![],
                    body: br#"{"error":"invalid_grant","error_description":"revoked"}"#.to_vec(),
                },
            },
        ])
        .await;
        let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
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
                    .map(|code| code.to_str().unwrap()),
                outcome(&resp).account_id.as_str()
            ),
            (503, Some("accounts_unavailable"), ""),
            "{coding:?}: the router's own answer, naming no account"
        );
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let answer: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            answer["error"]["message"], "no account in this pool can serve requests",
            "{coding:?}"
        );
        assert_eq!(token_posts(&up), 1, "{coding:?}: the refresh ran once");
        wait_for(Duration::from_secs(5), || {
            (h.store.account("a").unwrap().unwrap().state == AccountState::ReauthRequired)
                .then_some(())
        })
        .await;
        up.shutdown().await;
    }
}

#[tokio::test]
async fn a_400_over_the_buffer_bound_whose_arm_forwards_it_reaches_the_client_whole() {
    // Long enough that the drain stops well before the body's end, so the reader still holds a rest to forward.
    let body = over_the_bound("invalid_request_error", &"x".repeat(200 * 1024));
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 400,
            headers: vec![],
            body: body.clone(),
        },
    }])
    .await;
    let h = two_accounts(&up).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        bytes.as_ref(),
        body.as_slice(),
        "the prefix and the rest, every byte"
    );
    assert_eq!(
        credentials_seen(&up),
        [A],
        "the client's own 400 fails nothing over"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_model_404_over_the_buffer_bound_cools_the_model_and_fails_over() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    // Long enough that the drain stops well before the body's end, so the document never parses whole.
    let message = format!("model: claude-sonnet-4-5 ({})", "x".repeat(200 * 1024));
    up.add_credential_rule(
        A,
        "/v1/messages",
        Respond::Json {
            status: 404,
            headers: vec![],
            body: over_the_bound("not_found_error", &message),
        },
    );
    let h = two_accounts(&up).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    let o = outcome(&resp);
    assert_eq!(
        (resp.status().as_u16(), o.account_id.as_str(), o.attempt),
        (200, "b", 2),
        "the 404 named the model, so the request failed over"
    );
    assert_eq!(credentials_seen(&up), [A, B]);
    let cooldowns = h.router.cooldowns().await.unwrap();
    assert_eq!(
        cooldowns
            .iter()
            .map(|c| (c.account_id.as_str(), &c.scope, c.reason.as_str()))
            .collect::<Vec<_>>(),
        [(
            "a",
            &CooldownScope::Model("claude-sonnet-4-5-20250929".into()),
            "model_rejected"
        )]
    );
    up.shutdown().await;
}

/// The rate-limit tier the priced harness puts every account on, which its pricer keys on.
const TIER: &str = "default_claude_max_20x";

/// A ten-millionth of either window per token of input or cache write on `TIER`, so a body of `n` tokens books
/// `n / 10_000_000` and a usage of `n` input tokens charges the same.
fn tenth_per_million() -> SharePricer {
    SharePricer::new(|_, _, tier, _, usage, _| {
        (tier == TIER).then(|| (usage.input + usage.write_5m) as f64 / 10_000_000.0)
    })
}

/// `harness_with_router_config` over `accounts` on `TIER`, each reading 0.5 on its 5-hour window a second before the
/// harness clock, so a booking and a charge at the clock move the prediction the test reads.
async fn priced_harness(up: &FakeUpstream, accounts: &[TestAccount]) -> Harness {
    let h = harness_with_router_config(
        up,
        accounts,
        PoolSpec::default(),
        RouterConfig {
            share: tenth_per_million(),
            ..RouterConfig::default()
        },
    )
    .await;
    for account in accounts {
        h.store
            .set_profile(
                account.id,
                "{}",
                &SubscriptionRow {
                    tier: Some(TIER.into()),
                    ..SubscriptionRow::default()
                },
            )
            .unwrap();
    }
    h.router.reload().await.unwrap();
    for account in accounts {
        h.router
            .report(Report::Sample {
                account_id: account.id.into(),
                sample: UtilizationSample::default().with(
                    FIVE_HOUR,
                    WindowReading {
                        utilization: 0.5,
                        reset: None,
                        observed_at: NOW - 1_000,
                        source: WindowSource::Poll,
                        plan: None,
                        tier: None,
                    },
                ),
                incarnation: 1,
                source_request_id: None,
            })
            .await;
    }
    h
}

/// The 5-hour prediction of `account`, as the router answers it now.
async fn five_hour(h: &Harness, account: &str) -> f64 {
    h.router
        .samples()
        .await
        .unwrap()
        .into_iter()
        .find(|(id, _)| id == account)
        .and_then(|(_, sample)| sample.windows.get(FIVE_HOUR).map(|r| r.utilization))
        .expect("a reading")
}

/// The expiries among `events`, as `(request, attempt, cause)`.
fn expiries(events: &[RouterEvent]) -> Vec<(String, u32, &'static str)> {
    events
        .iter()
        .filter_map(|event| match event {
            RouterEvent::ReservationExpired {
                request_id,
                attempt,
                cause,
                ..
            } => Some((request_id.clone(), *attempt, *cause)),
            _ => None,
        })
        .collect()
}

fn ev(event: &str, data: &str) -> SseEvent {
    SseEvent {
        event: event.into(),
        data: data.into(),
    }
}

/// A committed stream of `input_tokens` the fake holds after its first event until the test releases it, so the
/// request is in flight while the test reads the router.
fn held_stream(input_tokens: u64) -> Respond {
    Respond::SseGated {
        status: 200,
        headers: vec![],
        events: vec![
            ev(
                "message_start",
                &format!(
                    r#"{{"type":"message_start","message":{{"id":"msg_1","usage":{{"input_tokens":{input_tokens}}}}}}}"#
                ),
            ),
            ev("message_stop", r#"{"type":"message_stop"}"#),
        ],
    }
}

#[tokio::test]
async fn a_failover_replaces_the_booking_and_a_usage_a_refusal_or_an_exhaustion_settles_it() {
    // A 429 on `a` fails over to `b`, whose selection takes the booking: one live booking, on `b`, and the usage
    // of its answer replaces it with no expiry.
    let up = FakeUpstream::start(vec![]).await;
    up.add_credential_rule(A, "/v1/messages", json_429_unified(RESET, "five_hour"));
    up.add_credential_rule(B, "/v1/messages", held_stream(100));
    let h = priced_harness(&up, &[oauth("a", A, 0), oauth("b", B, 1)]).await;
    let mut events = h.router.subscribe_events();
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    let o = outcome(&resp);
    assert_eq!(
        (resp.status().as_u16(), o.account_id.as_str(), o.attempt),
        (200, "b", 2)
    );
    assert!(
        (five_hour(&h, "a").await - 0.5).abs() < 1e-9,
        "the failover's selection took the booking off a"
    );
    let on_b = five_hour(&h, "b").await;
    // The fixture body's 84,390 bytes over four are 21,097 tokens, a ten-millionth of the window each.
    assert!(
        (on_b - (0.5 + 0.002_109_7)).abs() < 1e-9,
        "and placed the body's estimate on b: {on_b}"
    );
    up.release();
    resp.into_body().collect().await.unwrap();
    let written = router_events_until(&mut events, |event| {
        matches!(event, RouterEvent::CacheMapWritten { request_id, .. } if *request_id == o.request_id)
    })
    .await;
    assert!(
        expiries(&written).is_empty(),
        "a usage replaces the booking without an expiry: {written:?}"
    );
    let settled = five_hour(&h, "b").await;
    assert!(
        (settled - (0.5 + 100.0 / 10_000_000.0)).abs() < 1e-9,
        "the reading plus the usage's share alone: {settled}"
    );
    up.shutdown().await;

    // A committed answer the relay forwards with a status other than 200 ends the request refused.
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 500,
            headers: vec![],
            body: br#"{"type":"error","error":{"type":"api_error","message":"down"}}"#.to_vec(),
        },
    }])
    .await;
    let h = priced_harness(&up, &[oauth("a", A, 0)]).await;
    let mut events = h.router.subscribe_events();
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 500);
    let id = outcome(&resp).request_id;
    resp.into_body().collect().await.unwrap();
    let seen = router_events_until(&mut events, |event| {
        matches!(event, RouterEvent::ReservationExpired { .. })
    })
    .await;
    assert_eq!(expiries(&seen), [(id, 1, "refused")]);
    assert!(
        (five_hour(&h, "a").await - 0.5).abs() < 1e-9,
        "the booking left and nothing was charged"
    );
    up.shutdown().await;

    // A 429 on the pool's one account leaves no account to fail over to: the request ends exhausted.
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_429_unified(RESET, "five_hour"),
    }])
    .await;
    let h = priced_harness(&up, &[oauth("a", A, 0)]).await;
    let mut events = h.router.subscribe_events();
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    let o = outcome(&resp);
    assert_eq!(o.error_code.as_deref(), Some("accounts_exhausted"));
    let seen = router_events_until(&mut events, |event| {
        matches!(event, RouterEvent::ReservationExpired { .. })
    })
    .await;
    assert_eq!(expiries(&seen), [(o.request_id, 1, "exhausted")]);
    assert!((five_hour(&h, "a").await - 0.5).abs() < 1e-9);
    up.shutdown().await;
}
