//! The acceptance row "Ledger": every request has one row and one row per attempt; refusals have a row and
//! no attempt; a saturated channel produces a ledger_gap whose count equals the dropped records.
mod common;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule, SseEvent};
use http_body_util::BodyExt;
use phoebis_bus::{
    AccountSelected, AttemptOutcome, AutoMode, Event as BusEvent, Kind, Origin,
    Outcome as BusOutcome, RequestComplete, RequestObserved, RequestShape, ResponseChunk,
    ResponseFacts, Usage, BODY_SHAPE_ERROR_CODE,
};
use phoebis_ledger::{
    list_requests, member_spends, Assembler, AttemptRecord, BudgetedMember, EventRecord,
    LedgerConfig, LiveKeys, PriceTable, Record, RequestFilter, RequestRecord,
};
use phoebis_profile::fixtures::BodyFixture;
use phoebis_store::{BudgetPeriod, MemberWrite};
use tower::ServiceExt;

fn request_id(resp: &axum::http::Response<axum::body::Body>) -> String {
    resp.extensions()
        .get::<phoebis_relay::RelayOutcome>()
        .unwrap()
        .request_id
        .clone()
}

#[tokio::test]
async fn every_client_request_has_one_row_and_one_row_per_attempt() {
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
                    "anthropic-ratelimit-unified-5h-status".into(),
                    "rejected".into(),
                ),
                ("anthropic-ratelimit-unified-5h-reset".into(), "4600".into()),
                ("anthropic-ratelimit-unified-reset".into(), "4600".into()),
            ],
            body: br#"{"type":"error","error":{"type":"rate_limit_error","message":"limit"}}"#
                .to_vec(),
        },
    );
    let h = harness(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        (0, None),
        LedgerConfig::default(),
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5[1m]", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let id = request_id(&resp);
    let _ = resp.into_body().collect().await.unwrap();
    let (row, attempts) = ledgered(&h, &id).await;
    assert_eq!(
        (
            row.key_id.as_deref(),
            row.pool_id.as_deref(),
            row.session_id.as_deref(),
            row.kind.as_str(),
            row.attempts,
            row.status,
            row.outcome.as_str()
        ),
        (
            Some("desktop"),
            Some("main"),
            Some("s1"),
            "inference",
            2,
            Some(200),
            "succeeded"
        )
    );
    assert_eq!(
        (row.model.as_str(), row.canonical_model.as_str()),
        ("claude-sonnet-4-5[1m]", "claude-sonnet-4-5"),
        "the canonical id with the original in model"
    );
    assert_eq!(
        attempts
            .iter()
            .map(|a| (
                a.attempt,
                a.account_id.as_str(),
                a.upstream_status,
                a.outcome.as_str()
            ))
            .collect::<Vec<_>>(),
        [
            (1, "a", Some(429), "rate_limited"),
            (2, "b", Some(200), "succeeded")
        ]
    );
    assert_eq!(
        (
            attempts[0].cooldown_scope.as_deref(),
            attempts[0].cooldown_until
        ),
        (Some("account"), Some(4_600_000 + 1_000))
    );
    assert_eq!(
        (attempts[1].input_tokens, attempts[1].output_tokens),
        (Some(3), Some(4))
    );
    // A count_tokens request is its own kind.
    let mut req = request_for(KEY, "claude-sonnet-4-5", "s1");
    *req.uri_mut() = "/v1/messages/count_tokens".parse().unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    let id = request_id(&resp);
    let _ = resp.into_body().collect().await.unwrap();
    assert_eq!(ledgered(&h, &id).await.0.kind, "count_tokens");
    up.shutdown().await;
}

#[tokio::test]
async fn an_unknown_key_401_a_policy_404_and_an_exhaustion_have_a_row_and_no_attempt_beyond_the_ones_made(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 429,
            headers: vec![
                (
                    "anthropic-ratelimit-unified-status".into(),
                    "rejected".into(),
                ),
                ("anthropic-ratelimit-unified-5h-reset".into(), "4600".into()),
            ],
            body: b"{}".to_vec(),
        },
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    h.store
        .update_api_key(
            &phoebis_store::ApiKeyRow {
                model_rules: vec![
                    phoebis_store::ModelRule {
                        pattern: "claude-sonnet-4-5".into(),
                        not: false,
                    },
                    phoebis_store::ModelRule {
                        pattern: "*".into(),
                        not: true,
                    },
                ],
                ..h.store.api_key("desktop").unwrap().unwrap()
            },
            "test",
        )
        .unwrap();
    h.router.reload().await.unwrap();
    let resp = h
        .app
        .clone()
        .oneshot(request_for("phoebis-nobody", "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let (row, attempts) = ledgered(&h, &request_id(&resp)).await;
    assert_eq!(
        (
            row.key_id,
            row.pool_id,
            row.outcome.as_str(),
            row.error_code.as_deref(),
            row.attempts,
            attempts.len()
        ),
        (None, None, "rejected", Some("auth_unknown"), 0, 0)
    );
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-opus-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let (row, attempts) = ledgered(&h, &request_id(&resp)).await;
    assert_eq!(
        (
            row.key_id.as_deref(),
            row.outcome.as_str(),
            row.error_code.as_deref(),
            attempts.len()
        ),
        (Some("desktop"), "rejected", Some("model_not_allowed"), 0)
    );
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
    let (row, attempts) = ledgered(&h, &request_id(&resp)).await;
    assert_eq!(
        (
            row.outcome.as_str(),
            row.error_code.as_deref(),
            row.attempts,
            attempts.len()
        ),
        ("exhausted", Some("accounts_exhausted"), 1, 1)
    );
    // The second request of the session is fenced by the cooldown before any attempt: a row and no attempt.
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
    let (row, attempts) = ledgered(&h, &request_id(&resp)).await;
    assert_eq!(
        (row.outcome.as_str(), row.attempts, attempts.len()),
        ("exhausted", 0, 0)
    );
    up.shutdown().await;
}

/// Every client version is relayed, so which client made a request is a fact the row has to carry.
#[tokio::test]
async fn the_row_carries_the_callers_user_agent_and_the_log_filters_on_it() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let mut req = request_for(KEY, "claude-sonnet-4-5", "s1");
    req.headers_mut().insert(
        "user-agent",
        "claude-cli/2.1.300 (external, cli)".parse().unwrap(),
    );
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let id = request_id(&resp);
    let _ = resp.into_body().collect().await.unwrap();
    assert_eq!(
        ledgered(&h, &id).await.0.user_agent.as_deref(),
        Some("claude-cli/2.1.300 (external, cli)"),
        "the agent reaches the row as it was sent"
    );
    let matched = |agent: &str| {
        let filter = phoebis_ledger::RequestFilter {
            user_agent: Some(agent.into()),
            ..Default::default()
        };
        h.db.read_blocking(|c| {
            phoebis_ledger::list_requests(
                c,
                &filter,
                &phoebis_ledger::LiveKeys::default(),
                None,
                10,
            )
        })
        .unwrap()
        .0
        .len()
    };
    assert_eq!(
        matched("*2.1.300*"),
        1,
        "a pattern around the version matches"
    );
    assert_eq!(matched("*2.1.269*"), 0, "another build does not");
    assert_eq!(
        matched("claude-cli/2.1.300 (external, cli)"),
        1,
        "no wildcard is an exact match"
    );
    assert_eq!(
        matched("2.1.300"),
        0,
        "a bare substring is no longer enough"
    );
    up.shutdown().await;
}

/// The class the relay derives at admission, the gateway hints the client sent, and the turn origin off a
/// stamped attribution line all reach the request row, and the log filters on each of them; the class is also
/// the hourly rollup's dimension, so two classes in the same hour hold two rows.
#[tokio::test]
async fn the_row_carries_the_client_class_the_hints_and_the_turn_origin_and_the_log_filters_on_them(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let native =
        String::from_utf8(phoebis_profile::fixtures::body(BodyFixture::FirstParty9b244).to_vec())
            .unwrap();
    let (line, headers) =
        phoebis_transport::request::parse_head(phoebis_profile::fixtures::inference_head(
            phoebis_profile::fixtures::FixtureKind::OauthFirstParty,
        ))
        .unwrap();
    let target = line.split(' ').nth(1).unwrap();
    // `agent` swaps the fixture's own `claude-cli/` user agent and drops the fixture's `X-Stainless-Lang` with it,
    // sending the language header `agent` names in its place.
    let build = |body: String, session: &str, agent: Option<(&str, &str)>| {
        let mut req = axum::http::Request::builder().method("POST").uri(target);
        for (n, v) in &headers {
            match n.to_ascii_lowercase().as_str() {
                "authorization" => req = req.header("Authorization", format!("Bearer {KEY}")),
                "host" | "content-length" | "x-claude-code-session-id" => {}
                "user-agent" | "x-stainless-lang" if agent.is_some() => {}
                _ => req = req.header(n.as_str(), v.as_str()),
            }
        }
        if let Some((user_agent, lang)) = agent {
            req = req
                .header("User-Agent", user_agent)
                .header("X-Stainless-Lang", lang);
        }
        req.header("x-claude-code-session-id", session)
            .body(axum::body::Body::from(body.into_bytes()))
            .unwrap()
    };

    // The billing line stamped with a turn origin, the three hint headers set: every fact reaches the row.
    let spliced = native.replacen(
        "cc_prompt_id=aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa;",
        "cc_prompt_id=aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa; cc_turn_origin=human;",
        1,
    );
    let spliced = String::from_utf8(phoebis_cch::sign(spliced.as_bytes()).unwrap()).unwrap();
    let mut req = build(spliced, "s1", None);
    req.headers_mut()
        .insert("x-claude-code-agent-type", "Explore".parse().unwrap());
    req.headers_mut()
        .insert("x-claude-code-compaction", "auto".parse().unwrap());
    req.headers_mut()
        .insert("x-claude-code-context-compacted", "manual".parse().unwrap());
    req.headers_mut().insert(
        "x-claude-code-prev-tool-durations",
        "Bash=1200;Read=35;".parse().unwrap(),
    );
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let id = request_id(&resp);
    let _ = resp.into_body().collect().await.unwrap();
    let (row, _) = ledgered(&h, &id).await;
    assert_eq!(
        (
            row.client.as_deref(),
            row.agent_type.as_deref(),
            row.compaction.as_deref(),
            row.context_compacted.as_deref(),
            row.prev_tool_ms,
            row.turn_origin.as_deref()
        ),
        (
            Some("claude-code/sdk-cli"),
            Some("Explore"),
            Some("auto"),
            Some("manual"),
            Some(1_235),
            Some("human")
        ),
        "the class, the hints, and the turn origin all reach the row"
    );
    let matched_client = |pattern: &str| {
        let filter = phoebis_ledger::RequestFilter {
            client: Some(pattern.into()),
            ..Default::default()
        };
        h.db.read_blocking(|c| {
            phoebis_ledger::list_requests(
                c,
                &filter,
                &phoebis_ledger::LiveKeys::default(),
                None,
                10,
            )
        })
        .unwrap()
        .0
        .len()
    };
    assert_eq!(
        matched_client("claude-code/*"),
        1,
        "the class pattern finds it"
    );
    assert_eq!(
        matched_client("sdk/*"),
        0,
        "and a class it does not match finds nothing"
    );
    let matched_agent_types = |types: &[&str]| {
        let filter = phoebis_ledger::RequestFilter {
            agent_types: types.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        h.db.read_blocking(|c| {
            phoebis_ledger::list_requests(
                c,
                &filter,
                &phoebis_ledger::LiveKeys::default(),
                None,
                10,
            )
        })
        .unwrap()
        .0
        .len()
    };
    assert_eq!(
        matched_agent_types(&["Explore"]),
        1,
        "the agent type finds it"
    );
    assert_eq!(
        matched_agent_types(&["Plan"]),
        0,
        "another agent type does not"
    );
    let matched_turn_origins = |origins: &[&str]| {
        let filter = phoebis_ledger::RequestFilter {
            turn_origins: origins.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        h.db.read_blocking(|c| {
            phoebis_ledger::list_requests(
                c,
                &filter,
                &phoebis_ledger::LiveKeys::default(),
                None,
                10,
            )
        })
        .unwrap()
        .0
        .len()
    };
    assert_eq!(
        matched_turn_origins(&["human"]),
        1,
        "the turn origin finds it"
    );

    // No hint header sent: the four hint members are absent, but the class still reads from the user agent.
    let resp2 = h
        .app
        .clone()
        .oneshot(build(native.clone(), "s2", None))
        .await
        .unwrap();
    assert_eq!(resp2.status(), 200);
    let id2 = request_id(&resp2);
    let _ = resp2.into_body().collect().await.unwrap();
    let (row2, _) = ledgered(&h, &id2).await;
    assert_eq!(
        (
            row2.client.as_deref(),
            row2.agent_type.as_deref(),
            row2.compaction.as_deref(),
            row2.context_compacted.as_deref(),
            row2.prev_tool_ms,
            row2.turn_origin.as_deref()
        ),
        (Some("claude-code/sdk-cli"), None, None, None, None, None),
        "a caller that sends no hint carries the class alone"
    );

    // A caller of a different class, in the same hour: the rollup's client dimension keeps the two apart.
    let resp3 = h
        .app
        .clone()
        .oneshot(build(
            native,
            "s3",
            Some(("anthropic-sdk-python/0.40.0", "python")),
        ))
        .await
        .unwrap();
    assert_eq!(resp3.status(), 200);
    let id3 = request_id(&resp3);
    let _ = resp3.into_body().collect().await.unwrap();
    assert_eq!(
        ledgered(&h, &id3).await.0.client.as_deref(),
        Some("other"),
        "a caller outside Claude Code carries the other label"
    );
    let rollups =
        h.db.read(|c| phoebis_ledger::read_rollups(c, 0, i64::MAX))
            .await
            .unwrap();
    let by_client: std::collections::BTreeMap<&str, u64> =
        rollups
            .iter()
            .fold(std::collections::BTreeMap::new(), |mut m, r| {
                *m.entry(r.client.as_str()).or_default() += r.requests;
                m
            });
    assert_eq!(
        by_client.get("claude-code/sdk-cli").copied(),
        Some(2),
        "the first two requests fold under the same client"
    );
    assert_eq!(
        by_client.get("other").copied(),
        Some(1),
        "the third request's class is its own row in the same hour"
    );
    up.shutdown().await;
}

/// A request with no `User-Agent` header at all is not invisible to the pattern filter: it lands under the
/// `-` marker the rollups and the console's fold already use for one.
#[tokio::test]
async fn a_request_with_no_user_agent_is_found_by_the_wildcard_and_the_exact_dash() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(
        &up,
        &[api_key("a", C, 0)],
        (0, None),
        LedgerConfig::default(),
    )
    .await;
    let mut req = request_for(KEY, "claude-sonnet-4-5", "s1");
    req.headers_mut().remove("user-agent");
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let id = request_id(&resp);
    let _ = resp.into_body().collect().await.unwrap();
    assert_eq!(ledgered(&h, &id).await.0.user_agent, None);
    let matched = |agent: &str| {
        let filter = phoebis_ledger::RequestFilter {
            user_agent: Some(agent.into()),
            ..Default::default()
        };
        h.db.read_blocking(|c| {
            phoebis_ledger::list_requests(
                c,
                &filter,
                &phoebis_ledger::LiveKeys::default(),
                None,
                10,
            )
        })
        .unwrap()
        .0
        .len()
    };
    assert_eq!(matched("*"), 1, "the wildcard includes a row with no agent");
    assert_eq!(
        matched("-"),
        1,
        "the exact unattributed marker selects it too"
    );
    up.shutdown().await;
}

/// The answer's own counts and facts reach the attempt row and the three counts reach the hour's rollup; the
/// served model is recorded only when it is not the one the router resolved.
#[tokio::test]
async fn the_answers_thinking_tool_counts_and_facts_reach_the_attempt_row_and_the_rollup() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let id = request_id(&resp);
    let _ = resp.into_body().collect().await.unwrap();
    let (_, attempts) = ledgered(&h, &id).await;
    let a = &attempts[0];
    assert_eq!(
        (
            a.thinking_tokens,
            a.web_search_requests,
            a.web_fetch_requests
        ),
        (Some(1), Some(1), Some(0)),
        "a count the answer sent is recorded, a zero as zero"
    );
    assert_eq!(
        (
            a.stop_reason.as_deref(),
            a.speed.as_deref(),
            a.inference_geo.as_deref()
        ),
        (Some("end_turn"), Some("standard"), Some("us"))
    );
    assert_eq!(
        a.served_model, None,
        "the answer named the resolved model, so nothing is recorded"
    );
    assert_eq!(
        (
            a.stop_sequence.clone(),
            a.cleared_input_tokens,
            a.refusal_category.as_deref()
        ),
        (None, None, None)
    );
    let rows =
        h.db.read(|c| phoebis_ledger::read_rollups(c, 0, i64::MAX))
            .await
            .unwrap();
    let row = rows.iter().find(|r| r.kind == "inference").unwrap();
    assert_eq!(
        (
            row.thinking_tokens,
            row.web_search_requests,
            row.web_fetch_requests,
            row.effort.as_str()
        ),
        (1, 1, 0, "none")
    );
    up.shutdown().await;
}

/// The one case needing a body already edited by hand: the head-and-header wiring `request_for_fixture` uses,
/// over a caller-supplied body instead of a whole fixture.
fn request_with_native_body(
    native: String,
    key: &str,
    session: &str,
) -> axum::http::Request<axum::body::Body> {
    let (line, headers) =
        phoebis_transport::request::parse_head(phoebis_profile::fixtures::inference_head(
            phoebis_profile::fixtures::FixtureKind::OauthFirstParty,
        ))
        .unwrap();
    let target = line.split(' ').nth(1).unwrap();
    let mut req = axum::http::Request::builder().method("POST").uri(target);
    for (n, v) in headers {
        match n.to_ascii_lowercase().as_str() {
            "authorization" => req = req.header("Authorization", format!("Bearer {key}")),
            "host" | "content-length" | "x-claude-code-session-id" => {}
            _ => req = req.header(n, v),
        }
    }
    req.header("x-claude-code-session-id", session)
        .body(axum::body::Body::from(native.into_bytes()))
        .unwrap()
}

/// The recorded first-party fixture's shape reaches the request row from the parse the relay already performs;
/// a second request with an effort spliced into the same fixture carries it into the row and the rollup key.
#[tokio::test]
async fn the_recorded_first_party_fixtures_shape_reaches_the_row_and_its_effort_the_rollup() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for_fixture(
            BodyFixture::FirstParty9b244,
            KEY,
            "claude-sonnet-4-5",
            "s1",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let id = request_id(&resp);
    let _ = resp.into_body().collect().await.unwrap();
    let (row, _) = ledgered(&h, &id).await;
    assert_eq!(
        (
            row.thinking_type.as_deref(),
            row.thinking_budget,
            row.max_tokens
        ),
        (Some("enabled"), Some(31_999), Some(32_000))
    );
    assert_eq!(
        (row.tool_count, row.system_blocks, row.message_count),
        (12, 4, 1)
    );
    assert_eq!(row.context_edits, "clear_thinking_20251015");
    assert_eq!(row.effort, None);

    let spliced =
        String::from_utf8(phoebis_profile::fixtures::body(BodyFixture::FirstParty9b244).to_vec())
            .unwrap()
            .replacen(
                r#""model":"claude-sonnet-4-5""#,
                r#""model":"claude-sonnet-4-5","output_config":{"effort":"high"}"#,
                1,
            );
    // The fixture carries a first-party client's billing stamp over its own bytes; a hand-spliced body needs
    // its own stamp, or the relay's drift check refuses it as tampered.
    let spliced = String::from_utf8(phoebis_cch::sign(spliced.as_bytes()).unwrap()).unwrap();
    let resp2 = h
        .app
        .clone()
        .oneshot(request_with_native_body(spliced, KEY, "s2"))
        .await
        .unwrap();
    assert_eq!(resp2.status(), 200);
    let id2 = request_id(&resp2);
    let _ = resp2.into_body().collect().await.unwrap();
    let (row2, _) = ledgered(&h, &id2).await;
    assert_eq!(row2.effort.as_deref(), Some("high"));

    let rollups =
        h.db.read(|c| phoebis_ledger::read_rollups(c, 0, i64::MAX))
            .await
            .unwrap();
    assert!(
        rollups.iter().any(|r| r.effort == "high"),
        "the second request's effort lands in a rollup row keyed on it"
    );
    up.shutdown().await;
}

/// A client-drift refusal answers before any account is selected, so it has no attempt row to carry the kind;
/// the request row is where an operator reads which self-check refused it.
#[tokio::test]
async fn a_version_suffix_drift_refusal_is_recorded_on_the_request_row() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let native =
        String::from_utf8(phoebis_profile::fixtures::body(BodyFixture::FirstParty9b244).to_vec())
            .unwrap()
            .replacen("cc_version=2.1.269.517", "cc_version=2.1.269.abc", 1);
    let drifted = String::from_utf8(phoebis_cch::sign(native.as_bytes()).unwrap()).unwrap();
    let resp = h
        .app
        .clone()
        .oneshot(request_with_native_body(drifted, KEY, "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let id = request_id(&resp);
    let _ = resp.into_body().collect().await.unwrap();
    let (row, attempts) = ledgered(&h, &id).await;
    assert_eq!(row.client_drift.as_deref(), Some("version_suffix"));
    assert!(
        attempts.is_empty(),
        "the drift check runs before any account is tried"
    );
    up.shutdown().await;
}

/// A fallback model refused the request: the answer's own model and the refusal's details reach the row, since
/// the model that served is not the one the router resolved.
#[tokio::test]
async fn a_streamed_refusal_from_another_model_records_the_served_model_and_the_refusal() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Sse {
            status: 200,
            headers: vec![],
            events: vec![
                SseEvent {
                    event: "message_start".into(),
                    data: r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-opus-5","usage":{"input_tokens":9}}}"#.into(),
                },
                SseEvent {
                    event: "message_delta".into(),
                    data: r#"{"type":"message_delta","delta":{"stop_reason":"refusal","stop_sequence":null,"stop_details":{"type":"refusal","category":"cyber","recommended_model":"claude-opus-5"}},"usage":{"output_tokens":3}}"#.into(),
                },
                SseEvent {
                    event: "message_stop".into(),
                    data: r#"{"type":"message_stop"}"#.into(),
                },
            ],
            delay_between: Duration::from_millis(1),
        },
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let id = request_id(&resp);
    let _ = resp.into_body().collect().await.unwrap();
    let (_, attempts) = ledgered(&h, &id).await;
    let a = &attempts[0];
    assert_eq!(
        (
            a.served_model.as_deref(),
            a.stop_reason.as_deref(),
            a.refusal_category.as_deref(),
            a.refusal_recommended_model.as_deref()
        ),
        (
            Some("claude-opus-5"),
            Some("refusal"),
            Some("cyber"),
            Some("claude-opus-5")
        ),
        "the model that answered is not the one the router resolved, so it is recorded"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_saturated_channel_drops_records_and_a_ledger_gap_names_the_count() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    // The writer is held by a long transaction on the writer connection, so the channel fills.
    let db = h.db.clone();
    let (hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        db.write(|_| {
            hold_rx.recv().unwrap();
            Ok(())
        })
        .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let total = phoebis_ledger::CHANNEL_DEPTH + 100;
    for i in 0..total {
        h.ledger.record(Record::Event(EventRecord {
            at: i as i64,
            kind: "cooldown_set".into(),
            ..Default::default()
        }));
    }
    let dropped = h.ledger.dropped();
    // The writer took up to one batch (64) before it blocked, so between 36 and 100 records did not fit.
    assert!(
        (36..=100).contains(&dropped),
        "the overflow did not fit: {dropped}"
    );
    assert!(
        h.metrics
            .render(&phoebis_ledger::RoutingGauges::default(), &[], 0)
            .contains(&format!(
                r#"phoebis_ledger_dropped_total{{table="events"}} {dropped}"#
            )),
        "every record that did not fit is charged to its table"
    );
    hold_tx.send(()).unwrap();
    holder.join().unwrap();
    let gap = wait_for_async(Duration::from_secs(5), || async {
        h.db.events(0, i64::MAX, 10_000)
            .await
            .unwrap()
            .into_iter()
            .find(|e| e.kind == "ledger_gap")
    })
    .await;
    let detail: serde_json::Value = serde_json::from_str(gap.detail.as_deref().unwrap()).unwrap();
    assert_eq!(
        detail["dropped"].as_u64().unwrap(),
        dropped,
        "the gap names exactly what was dropped"
    );
    let written =
        h.db.events(0, i64::MAX, 10_000)
            .await
            .unwrap()
            .iter()
            .filter(|e| e.kind == "cooldown_set")
            .count();
    assert_eq!(written as u64 + dropped, total as u64);
    up.shutdown().await;
}

fn observed_event(id: &str) -> BusEvent {
    BusEvent::RequestObserved(RequestObserved {
        request_id: id.into(),
        origin: Origin::Client,
        kind: Kind::Inference,
        key_id: None,
        key_generation: 0,
        pool_id: None,
        session_id: None,
        agent_id: None,
        parent_agent_id: None,
        owner_id: None,
        model: "claude-sonnet-5".into(),
        canonical_model: "claude-sonnet-5".into(),
        wire_model: "claude-sonnet-5".into(),
        stream: false,
        headers: vec![],
        body: bytes::Bytes::new(),
        body_capture_hours: 0,
        shape: RequestShape::default(),
        static_hash: None,
        boundaries: Vec::new(),
        client: phoebis_bus::ClientFacts::default(),
        auto_mode: None,
        fork: phoebis_bus::Fork::default(),
        presented_retired: false,
        rotation_at: None,
        received_at: 0,
    })
}

fn complete_event(id: &str) -> BusEvent {
    BusEvent::RequestComplete(RequestComplete {
        request_id: id.into(),
        attempt: 0,
        account_id: None,
        outcome: BusOutcome::Succeeded,
        status: 200,
        usage: None,
        response: ResponseFacts::default(),
        ttfb_ms: None,
        ttfc_ms: None,
        error_code: None,
        client_drift: None,
        client_version: None,
        response_bytes: None,
        response_chunks: None,
        held: None,
        finished_at: 1,
    })
}

fn observed_capturing_event(id: &str) -> BusEvent {
    let BusEvent::RequestObserved(mut observed) = observed_event(id) else {
        unreachable!()
    };
    observed.body_capture_hours = 1;
    BusEvent::RequestObserved(observed)
}

fn chunk_event(id: &str, index: u32, body: &str) -> BusEvent {
    BusEvent::ResponseChunk(ResponseChunk {
        request_id: id.into(),
        attempt: 1,
        index,
        body: bytes::Bytes::from(body.to_string()),
        phase: None,
    })
}

/// A completion naming more chunks than the test goes on to deliver, so the capture reads back short.
fn complete_short_event(id: &str, response_chunks: u32) -> BusEvent {
    let BusEvent::RequestComplete(mut complete) = complete_event(id) else {
        unreachable!()
    };
    complete.response_chunks = Some(response_chunks);
    BusEvent::RequestComplete(complete)
}

/// The row `list_requests` hands back, the same query `/admin/requests` runs.
async fn listed(h: &Harness, id: &str) -> RequestRecord {
    wait_for_async(Duration::from_secs(5), || async {
        let id = id.to_string();
        h.db.read(move |c| {
            let (rows, _) =
                list_requests(c, &RequestFilter::default(), &LiveKeys::default(), None, 50)?;
            Ok(rows.into_iter().find(|(r, _)| r.id == id).map(|(r, _)| r))
        })
        .await
        .unwrap()
    })
    .await
}

/// A capture whose last chunk was dropped at the bus reads back truncated through a real database round
/// trip, not only through the in-memory assembler: `insert_request_in` has to store the flag the assembler
/// computes, since the read-time comparison against the bodies table cannot see this particular gap (nothing
/// after the drop ever raises `response_len_seen` past what the row already captured).
#[tokio::test]
async fn a_short_capture_reads_back_truncated_through_the_database() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    h.bus.publish(observed_capturing_event("trunc-1"));
    h.bus.publish(chunk_event("trunc-1", 0, "partial"));
    // The completion says two chunks were published; only one arrived.
    h.bus.publish(complete_short_event("trunc-1", 2));
    let row = listed(&h, "trunc-1").await;
    assert!(
        row.response_truncated,
        "a short capture reads back truncated through the same query the admin route runs"
    );
    let gap = wait_for_async(Duration::from_secs(5), || async {
        h.db.events(0, i64::MAX, 100)
            .await
            .unwrap()
            .into_iter()
            .find(|e| e.kind == "ledger_gap")
    })
    .await;
    let detail: serde_json::Value = serde_json::from_str(gap.detail.as_deref().unwrap()).unwrap();
    assert_eq!(detail["reason"].as_str(), Some("chunk_dropped"));
    up.shutdown().await;
}

fn auto_mode_observed(id: &str, auto_mode: Option<AutoMode>) -> BusEvent {
    let BusEvent::RequestObserved(mut observed) = observed_event(id) else {
        unreachable!()
    };
    observed.auto_mode = auto_mode;
    BusEvent::RequestObserved(observed)
}

fn head_event(id: &str, status: u16, at: i64) -> BusEvent {
    BusEvent::ResponseHead(phoebis_bus::ResponseHead {
        request_id: id.into(),
        attempt: 1,
        status,
        headers: vec![],
        first_event: None,
        outcome: if status == 200 {
            AttemptOutcome::Succeeded
        } else {
            AttemptOutcome::UpstreamError
        },
        cooldown: None,
        written_at: at,
        observed_at: at,
    })
}

/// The completion of attempt 1 on account `a` with the answer's facts.
fn answered_event(id: &str, status: u16, response: ResponseFacts) -> BusEvent {
    BusEvent::RequestComplete(RequestComplete {
        attempt: 1,
        account_id: Some("a".into()),
        outcome: if status == 200 {
            BusOutcome::Succeeded
        } else {
            BusOutcome::Failed
        },
        status,
        response,
        ..match complete_event(id) {
            BusEvent::RequestComplete(complete) => complete,
            _ => unreachable!(),
        }
    })
}

#[tokio::test]
async fn the_auto_mode_standing_round_trips_and_a_body_shape_refusal_reads_body_shape() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let requests = [
        ("am-server", Some(AutoMode::Server)),
        ("am-stripped", Some(AutoMode::Stripped)),
        ("am-plain", None),
    ];
    for (id, auto_mode) in requests {
        h.bus.publish(auto_mode_observed(id, auto_mode));
        h.bus.publish(selected_event(id, 1, "a", 10));
        h.bus.publish(head_event(id, 200, 20));
        h.bus
            .publish(answered_event(id, 200, ResponseFacts::default()));
    }
    // Two refusals before any account: the off setting's unreachable member and the cache TTL rule's marker.
    for (id, auto_mode) in [
        ("am-refused-off", Some(AutoMode::Stripped)),
        ("am-refused-ttl", None),
    ] {
        h.bus.publish(auto_mode_observed(id, auto_mode));
        let BusEvent::RequestComplete(refused) = complete_event(id) else {
            unreachable!()
        };
        h.bus.publish(BusEvent::RequestComplete(RequestComplete {
            outcome: BusOutcome::Rejected,
            status: 400,
            error_code: Some(BODY_SHAPE_ERROR_CODE.into()),
            ..refused
        }));
    }
    // A 200 head whose body the caller left before the stop.
    h.bus
        .publish(auto_mode_observed("am-canceled", Some(AutoMode::Server)));
    h.bus.publish(selected_event("am-canceled", 1, "a", 10));
    h.bus.publish(head_event("am-canceled", 200, 20));
    let BusEvent::RequestComplete(canceled) =
        answered_event("am-canceled", 200, ResponseFacts::default())
    else {
        unreachable!()
    };
    h.bus.publish(BusEvent::RequestComplete(RequestComplete {
        outcome: BusOutcome::Canceled,
        error_code: Some("client_gone".into()),
        ..canceled
    }));
    // A failover leg refused for its shape after an earlier leg reached an upstream (an account whose credential the
    // body edit cannot serve): the request did reach one, so the row keeps what admission read.
    h.bus.publish(auto_mode_observed(
        "am-failover-refused",
        Some(AutoMode::Server),
    ));
    h.bus
        .publish(selected_event("am-failover-refused", 1, "a", 10));
    h.bus.publish(head_event("am-failover-refused", 429, 20));
    let BusEvent::RequestComplete(refused) = complete_event("am-failover-refused") else {
        unreachable!()
    };
    h.bus.publish(BusEvent::RequestComplete(RequestComplete {
        attempt: 1,
        outcome: BusOutcome::Failed,
        status: 400,
        error_code: Some(BODY_SHAPE_ERROR_CODE.into()),
        ..refused
    }));
    let harness = &h;
    let read = |id: &'static str| async move { ledgered(harness, id).await.0.auto_mode };
    assert_eq!(read("am-server").await, Some("server".into()));
    assert_eq!(read("am-stripped").await, Some("stripped".into()));
    assert_eq!(
        read("am-plain").await,
        None,
        "a request that asked for nothing records no standing"
    );
    assert_eq!(
        read("am-canceled").await,
        Some("server".into()),
        "an answer cut short keeps the standing admission read"
    );
    for id in ["am-refused-off", "am-refused-ttl"] {
        let (row, attempts) = ledgered(harness, id).await;
        assert_eq!(
            row.auto_mode.as_deref(),
            Some("body_shape"),
            "a body-shape refusal reads body_shape whatever admission read ({id})"
        );
        assert_eq!(
            (row.outcome.as_str(), row.error_code.as_deref()),
            ("rejected", Some("body_shape")),
            "the refusal's outcome and code are recorded as before ({id})"
        );
        assert!(attempts.is_empty(), "a refusal has no attempt ({id})");
    }
    let (row, attempts) = ledgered(harness, "am-failover-refused").await;
    assert_eq!(
        row.auto_mode.as_deref(),
        Some("server"),
        "a leg refused for its shape after one reached an upstream keeps the standing admission read"
    );
    assert_eq!(
        (row.outcome.as_str(), row.error_code.as_deref()),
        ("failed", Some("body_shape"))
    );
    assert_eq!(
        attempts
            .iter()
            .map(|a| a.upstream_status)
            .collect::<Vec<_>>(),
        vec![Some(429)],
        "the leg that reached an upstream"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_held_filter_keeps_the_requests_that_waited_for_a_slot() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let waited = r#"{"waited_on":["a"],"cohort":null,"held_ms":1250}"#;
    h.bus.publish(observed_event("held-served"));
    h.bus.publish(selected_event("held-served", 1, "a", 10));
    h.bus.publish(head_event("held-served", 200, 20));
    let BusEvent::RequestComplete(served) =
        answered_event("held-served", 200, ResponseFacts::default())
    else {
        unreachable!()
    };
    h.bus.publish(BusEvent::RequestComplete(RequestComplete {
        held: Some(waited.into()),
        ..served
    }));
    // The bound ran out with the pool still at its cap: the answer dispatched nothing, so only the request row
    // can say it waited.
    h.bus.publish(observed_event("held-at-cap"));
    let BusEvent::RequestComplete(at_cap) = complete_event("held-at-cap") else {
        unreachable!()
    };
    h.bus.publish(BusEvent::RequestComplete(RequestComplete {
        outcome: BusOutcome::Exhausted,
        status: 429,
        error_code: Some("accounts_at_cap".into()),
        held: Some(waited.into()),
        ..at_cap
    }));
    h.bus.publish(observed_event("never-held"));
    h.bus.publish(selected_event("never-held", 1, "a", 10));
    h.bus.publish(head_event("never-held", 200, 20));
    h.bus
        .publish(answered_event("never-held", 200, ResponseFacts::default()));
    let (never, _) = ledgered(&h, "never-held").await;
    assert_eq!(never.held, None);
    let (at_cap, attempts) = ledgered(&h, "held-at-cap").await;
    assert!(attempts.is_empty(), "nothing was dispatched for it");
    assert_eq!(at_cap.held.as_deref(), Some(waited));
    ledgered(&h, "held-served").await;
    let mut held =
        h.db.read(|c| {
            let filter = RequestFilter {
                held: true,
                ..RequestFilter::default()
            };
            let (rows, _) = list_requests(c, &filter, &LiveKeys::default(), None, 50)?;
            Ok(rows.into_iter().map(|(r, _)| r.id).collect::<Vec<_>>())
        })
        .await
        .unwrap();
    held.sort();
    assert_eq!(
        held,
        ["held-at-cap", "held-served"],
        "a request held and served and one held with no dispatch both waited for a slot"
    );
    up.shutdown().await;
}

/// Held on a slow `land`, `CHANNEL_DEPTH + 8` requests offered through `record_request` all wait for room
/// rather than dropping.
#[tokio::test]
async fn a_request_record_waits_for_the_writer_rather_than_dropping() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let db = h.db.clone();
    let (hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        db.write(|_| {
            hold_rx.recv().unwrap();
            Ok(())
        })
        .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let total = phoebis_ledger::CHANNEL_DEPTH + 8;
    let mut sends = Vec::with_capacity(total);
    for i in 0..total {
        let ledger = h.ledger.clone();
        let id = format!("r{i}");
        sends.push(tokio::spawn(async move {
            ledger
                .record_request(Record::Request {
                    request: RequestRecord {
                        id,
                        ..Default::default()
                    },
                    attempts: Vec::new(),
                    bodies: None,
                })
                .await;
        }));
    }
    // The channel fills to its depth and the rest wait for room, which needs the writer genuinely blocked
    // rather than merely slow.
    tokio::time::sleep(Duration::from_millis(50)).await;
    hold_tx.send(()).unwrap();
    holder.join().unwrap();
    for send in sends {
        tokio::time::timeout(Duration::from_secs(10), send)
            .await
            .expect("every send resolves once the writer drains")
            .unwrap();
    }
    assert_eq!(h.ledger.dropped(), 0);
    for i in 0..total {
        assert!(
            h.db.request(&format!("r{i}")).await.unwrap().is_some(),
            "r{i} landed"
        );
    }
    up.shutdown().await;
}

/// A cooldown announced with the deadline the classifier saturates an absurd `Retry-After` to reaches the writer
/// ahead of the next request's row, and the row still lands. The runtime is let go without waiting on its blocking
/// pool, since a fold that never ends would otherwise hold this test open rather than fail it.
#[test]
fn a_request_recorded_after_a_saturated_cooldown_announcement_still_lands() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let landed = runtime.block_on(async {
        let up = FakeUpstream::start(vec![]).await;
        let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
        h.ledger.record(Record::Event(EventRecord {
            at: 0,
            kind: "cooldown_set".into(),
            account_id: Some("a".into()),
            detail: Some(
                serde_json::json!({ "scope": "account", "until": i64::MAX, "reason": "rate_limited" })
                    .to_string(),
            ),
            ..Default::default()
        }));
        tokio::time::timeout(
            Duration::from_secs(5),
            h.ledger.record_request(Record::Request {
                request: RequestRecord {
                    id: "after".into(),
                    ..Default::default()
                },
                attempts: Vec::new(),
                bodies: None,
            }),
        )
        .await
        .expect("the writer's channel has room");
        let landed = tokio::time::timeout(Duration::from_secs(10), async {
            while h.db.request("after").await.unwrap().is_none() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .is_ok();
        if landed {
            up.shutdown().await;
        }
        landed
    });
    runtime.shutdown_background();
    assert!(
        landed,
        "the request row landed behind the cooldown announcement"
    );
}

/// The bus subscriber's queue filled to its depth behind a writer channel filled to its own, both behind the
/// same held `land`: `shutdown` still returns and lands every completed request, because the writer keeps
/// reading while it waits for the assembler to end rather than waiting for it first.
#[tokio::test]
async fn a_shutdown_with_both_queues_full_lands_every_completed_request() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let db = h.db.clone();
    let (hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        db.write(|_| {
            hold_rx.recv().unwrap();
            Ok(())
        })
        .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let total = phoebis_ledger::CHANNEL_DEPTH + phoebis_bus::QUEUE_DEPTH + 8;
    let bus = h.bus.clone();
    // One task, publishing in order through `publish_fact` (which waits for room rather than dropping, as the
    // relay does): it blocks on its own once the writer's channel and then the assembler's bus queue fill.
    let publisher = tokio::spawn(async move {
        for i in 0..total {
            let id = format!("q{i}");
            bus.publish_fact(observed_event(&id)).await;
            bus.publish_fact(complete_event(&id)).await;
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    hold_tx.send(()).unwrap();
    let shutdown = tokio::time::timeout(Duration::from_secs(10), h.ledger.clone().shutdown()).await;
    holder.join().unwrap();
    shutdown
        .expect("the writer keeps reading while it waits for the assembler, so shutdown does not deadlock")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), publisher)
        .await
        .expect("the publisher resolves once the assembler has drained its queue")
        .unwrap();
    assert_eq!(h.ledger.dropped(), 0);
    for i in 0..total {
        assert!(
            h.db.request(&format!("q{i}")).await.unwrap().is_some(),
            "q{i} landed"
        );
    }
    up.shutdown().await;
}

#[tokio::test]
async fn shutdown_writes_everything_the_channel_held() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    for i in 0..500 {
        h.ledger.record(Record::Event(EventRecord {
            at: i,
            kind: "pin_created".into(),
            ..Default::default()
        }));
    }
    h.ledger.clone().shutdown().await.unwrap();
    assert_eq!(h.db.events(0, i64::MAX, 10_000).await.unwrap().len(), 500);
    assert!(
        h.ledger.clone().shutdown().await.is_err(),
        "a second shutdown finds no task"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_probe_through_the_relay_is_ledgered_with_kind_probe_and_origin_keepalive() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let resp = h
        .relay
        .probe(
            phoebis_relay::Probe {
                key_id: "desktop".into(),
                key_generation: h.store.api_key("desktop").unwrap().unwrap().created_at,
                allow_oauth: true,
            },
            request_for(KEY, "claude-sonnet-4-5", "s1"),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let id = request_id(&resp);
    let _ = resp.into_body().collect().await.unwrap();
    let (row, attempts) = ledgered(&h, &id).await;
    assert_eq!(
        (
            row.kind.as_str(),
            row.origin.as_str(),
            row.key_id.as_deref(),
            attempts.len()
        ),
        ("probe", "keepalive", Some("desktop"), 1)
    );
    let rollups =
        h.db.read_blocking(|c| phoebis_ledger::read_rollups(c, 0, i64::MAX))
            .unwrap();
    assert!(
        rollups.iter().any(|r| r.kind == "probe" && r.requests == 1),
        "the rollup row is keyed kind = probe"
    );
    up.shutdown().await;
}

/// The harness clock `minutes` past `NOW`.
fn minutes(n: i64) -> i64 {
    NOW + n * 60_000
}

fn selected_event(id: &str, attempt: u8, account: &str, at: i64) -> BusEvent {
    BusEvent::AccountSelected(AccountSelected {
        request_id: id.into(),
        attempt,
        account_id: account.into(),
        account_kind: "oauth".into(),
        pin_epoch: None,
        started_at: at,
        cache_ttl_ms: None,
        selection: None,
    })
}

/// The same selection, carrying a real trace: for the tests that read what the attempt row stored.
fn selected_event_with_trace(
    id: &str,
    attempt: u8,
    account: &str,
    at: i64,
    trace: &str,
) -> BusEvent {
    let BusEvent::AccountSelected(mut selected) = selected_event(id, attempt, account, at) else {
        unreachable!()
    };
    selected.selection = Some(trace.into());
    BusEvent::AccountSelected(selected)
}

/// A head that carries the account's 5-hour reading, reset at 4 600 s on the harness clock.
fn head_with_reading(utilization: &str) -> Respond {
    let Respond::Json { status, body, .. } = json_200() else {
        unreachable!()
    };
    Respond::Json {
        status,
        headers: vec![
            (
                "anthropic-ratelimit-unified-status".into(),
                "allowed".into(),
            ),
            (
                "anthropic-ratelimit-unified-5h-status".into(),
                "allowed".into(),
            ),
            (
                "anthropic-ratelimit-unified-5h-utilization".into(),
                utilization.into(),
            ),
            ("anthropic-ratelimit-unified-5h-reset".into(), "4600".into()),
        ],
        body,
    }
}

#[tokio::test]
async fn a_relayed_heads_reading_reaches_the_router_and_a_header_sample_with_its_instant() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: head_with_reading("0.3125"),
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let id = request_id(&resp);
    let _ = resp.into_body().collect().await.unwrap();
    ledgered(&h, &id).await;
    let samples = h.router.samples().await.unwrap();
    let reading = samples
        .iter()
        .find(|(account, _)| account == "a")
        .and_then(|(_, sample)| sample.reading(phoebis_routing::FIVE_HOUR))
        .expect("the router holds the head's 5-hour reading");
    assert_eq!(
        (reading.utilization, reading.reset),
        (0.3125, Some(4_600_000))
    );
    let row = wait_for_async(Duration::from_secs(5), || async {
        h.db.quota_samples("a", 0, i64::MAX)
            .await
            .unwrap()
            .into_iter()
            .find(|q| q.window == "5h")
    })
    .await;
    assert_eq!(
        (row.utilization, row.reset_at, row.source.as_str()),
        (Some(0.3125), Some(4_600_000), "header")
    );
    assert!(
        row.observed_at.is_some(),
        "the head's instant lands beside its bucket"
    );
    up.shutdown().await;
}

/// The 49-column shape `rollups_hourly` held before the class dimension joined it, written out by hand since
/// an integration test cannot reach the crate's private `COLUMNS` constant to strip the new column out of it.
fn pre_class_columns() -> String {
    "hour, key_id, key_generation, pool_id, account_id, canonical_model, kind, user_agent, client, effort, \
     requests, succeeded, rate_limited, failed, exhausted, rejected, attempts, failovers, cache_hits, \
     cache_misses, uncached_requests, input_tokens, cache_read_tokens, cache_write_5m_tokens, \
     cache_write_1h_tokens, output_tokens, thinking_tokens, web_search_requests, web_fetch_requests, \
     estimated_usd, input_usd, cache_read_usd, cache_write_usd, output_usd, kept_warm_requests, \
     kept_warm_tokens, saved_usd_gross, probe_cost_usd, probe_migrated, wasted_probes, wasted_probe_usd, \
     wasted_probe_reasons_json, probes_skipped, ttfb_hist, ttfc_hist, duration_hist, output_tps_hist, \
     errors_json, cache_miss_reasons_json"
        .into()
}

#[tokio::test]
async fn a_rollup_row_from_before_the_class_dimension_reads_under_the_reserved_class() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    {
        let db = phoebis_ledger::LedgerDb::open(&path).unwrap();
        // The four columns migration 18 adds, the five migration 19 adds, migration 20's column and table and
        // the columns migrations 21, 25 and 26 add, each dropped back out so the replay below has to add them
        // again; `rollups_hourly` keeps the shape the class dimension joins, since a primary-key column cannot
        // be dropped in place and the rebuild names its own columns regardless of what the source table still
        // carries.
        db.write(|c| {
            c.execute_batch(
                "ALTER TABLE requests DROP COLUMN request_class; \
                 ALTER TABLE requests DROP COLUMN identified_by; \
                 ALTER TABLE requests DROP COLUMN identity_root; \
                 ALTER TABLE requests DROP COLUMN identity_depth; \
                 ALTER TABLE requests DROP COLUMN forked_from; \
                 ALTER TABLE requests DROP COLUMN fork_depth; \
                 ALTER TABLE account_rollups_hourly DROP COLUMN rewrite_tokens; \
                 ALTER TABLE account_rollups_hourly DROP COLUMN migrations_json; \
                 ALTER TABLE attempts DROP COLUMN selection; \
                 ALTER TABLE quota_samples DROP COLUMN observed_at; \
                 ALTER TABLE requests DROP COLUMN auto_mode; \
                 ALTER TABLE requests DROP COLUMN held; \
                 ALTER TABLE attempts DROP COLUMN upstream_message_id;",
            )?;
            Ok(())
        })
        .unwrap();
        let columns = pre_class_columns();
        db.write(|c| {
            c.execute(
                &format!(
                    "INSERT INTO rollups_hourly ({columns}) VALUES \
                     (3600, 'k', 0, 'p', 'a', 'm', 'inference', 'ua', 'claude-code/cli', 'none', 5, 5, 0, 0, \
                     0, 0, 5, 0, 0, 0, 5, 1, 2, 3, 4, 6, 0, 0, 0, 0.5, 0.1, 0.1, 0.1, 0.2, 0, 0, 0.0, 0.0, 0, \
                     0, 0.0, '{{}}', 0, x'', x'', x'', x'', '{{}}', '{{}}')"
                ),
                [],
            )?;
            c.execute(
                "UPDATE meta SET value = '17' WHERE key = 'schema_version'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    }
    let db = std::sync::Arc::new(phoebis_ledger::LedgerDb::open(&path).unwrap());
    assert_eq!(db.schema_version().await.unwrap(), 26);
    let rows = db
        .read_blocking(|c| phoebis_ledger::read_rollups(c, 0, i64::MAX))
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (
            rows[0].request_class.as_str(),
            rows[0].requests,
            rows[0].output_tokens
        ),
        (phoebis_ledger::NO_CLASS, 5, 6),
        "a row folded before the column existed takes the reserved class and keeps its counts"
    );
    db.write(|c| {
        let tx = c.transaction()?;
        phoebis_ledger::fold_request(
            &tx,
            &RequestRecord {
                id: "r2".into(),
                received_at: 3_600_000,
                key_id: Some("k".into()),
                pool_id: Some("p".into()),
                canonical_model: "m".into(),
                kind: "inference".into(),
                outcome: "succeeded".into(),
                user_agent: Some("ua".into()),
                client: Some("claude-code/cli".into()),
                ..Default::default()
            },
            &[phoebis_ledger::AttemptRecord {
                request_id: "r2".into(),
                attempt: 1,
                account_id: "a".into(),
                account_kind: "oauth".into(),
                outcome: "succeeded".into(),
                output_tokens: Some(1),
                ..Default::default()
            }],
            None,
        )?;
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    let rows = db
        .read_blocking(|c| phoebis_ledger::read_rollups(c, 0, i64::MAX))
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "a fresh classless fold lands on the same reserved-class row"
    );
    assert_eq!((rows[0].requests, rows[0].output_tokens), (6, 7));
}

#[tokio::test]
async fn migration_nineteen_adds_the_identity_columns_to_a_version_eighteen_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    {
        let db = phoebis_ledger::LedgerDb::open(&path).unwrap();
        db.insert_request(
            &RequestRecord {
                id: "r0".into(),
                received_at: 0,
                key_id: Some("desktop".into()),
                pool_id: Some("main".into()),
                canonical_model: "claude-sonnet-5".into(),
                model: "claude-sonnet-5".into(),
                kind: "inference".into(),
                origin: "client".into(),
                outcome: "succeeded".into(),
                attempts: 1,
                ..Default::default()
            },
            &[],
            None,
        )
        .unwrap();
        db.write(|c| {
            c.execute_batch(
                "ALTER TABLE requests DROP COLUMN identified_by; \
                 ALTER TABLE requests DROP COLUMN identity_root; \
                 ALTER TABLE requests DROP COLUMN identity_depth; \
                 ALTER TABLE requests DROP COLUMN forked_from; \
                 ALTER TABLE requests DROP COLUMN fork_depth; \
                 ALTER TABLE quota_samples DROP COLUMN observed_at; \
                 ALTER TABLE requests DROP COLUMN auto_mode; \
                 ALTER TABLE requests DROP COLUMN held; \
                 ALTER TABLE attempts DROP COLUMN upstream_message_id; \
                 UPDATE meta SET value = '18' WHERE key = 'schema_version';",
            )?;
            Ok(())
        })
        .unwrap();
    }
    let db = std::sync::Arc::new(phoebis_ledger::LedgerDb::open(&path).unwrap());
    assert_eq!(db.schema_version().await.unwrap(), 26);
    let (row, _) = db.request("r0").await.unwrap().unwrap();
    assert_eq!(
        (
            row.forked_from,
            row.fork_depth
        ),
        (None, None),
        "a row from before the identity columns reads both fork members null"
    );
    let has_member_index = |ledger: &phoebis_ledger::LedgerDb| {
        ledger
            .read_blocking(|c| {
                Ok(c.query_row(
                    "SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = 'rollups_hourly_member'",
                    [],
                    |_| Ok(()),
                )
                .is_ok())
            })
            .unwrap()
    };
    assert!(
        has_member_index(&db),
        "the migrated database carries the member-spend index"
    );
    assert!(
        has_member_index(&phoebis_ledger::LedgerDb::open_in_memory().unwrap()),
        "a fresh database carries the same index"
    );
}

/// The table migration 23 drops, recreated so a forward replay from version 20 or 21 can drop it again.
const RECREATE_DROPPED_TABLE: &str = "CREATE TABLE window_reconciliations (account_id TEXT NOT NULL, window TEXT NOT NULL, window_end INTEGER NOT NULL, computed_at INTEGER NOT NULL, PRIMARY KEY (account_id, window, window_end)); -- rollback";

#[tokio::test]
async fn a_ledger_from_before_the_auto_mode_column_gains_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    {
        let db = phoebis_ledger::LedgerDb::open(&path).unwrap();
        db.write(|c| {
            c.execute_batch(&format!(
                "ALTER TABLE requests DROP COLUMN auto_mode; ALTER TABLE requests DROP COLUMN held; \
                 ALTER TABLE attempts DROP COLUMN upstream_message_id; \
                 {RECREATE_DROPPED_TABLE}\n\
                 UPDATE meta SET value = '20' WHERE key = 'schema_version';"
            ))?;
            Ok(())
        })
        .unwrap();
    }
    let db = std::sync::Arc::new(phoebis_ledger::LedgerDb::open(&path).unwrap());
    assert_eq!(db.schema_version().await.unwrap(), 26);
    let columns = db
        .read_blocking(|c| {
            let mut stmt = c.prepare("SELECT name FROM pragma_table_info('requests')")?;
            let names = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(names)
        })
        .unwrap();
    assert!(columns.contains(&"auto_mode".to_string()));
    let reconciliation_tables: i64 = db
        .read_blocking(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'window_reconciliations'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(
        reconciliation_tables, 0,
        "migration 23 dropped the reconciliation table"
    );
}

#[tokio::test]
async fn migration_twenty_two_files_every_rollup_row_under_standard_speed_and_keys_by_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let request = |id: &str| RequestRecord {
        id: id.into(),
        received_at: 3_600_000,
        key_id: Some("desktop".into()),
        pool_id: Some("main".into()),
        canonical_model: "claude-opus-5".into(),
        model: "claude-opus-5".into(),
        kind: "inference".into(),
        origin: "client".into(),
        outcome: "succeeded".into(),
        attempts: 1,
        ..Default::default()
    };
    let attempt = |id: &str, speed: Option<&str>| phoebis_ledger::AttemptRecord {
        request_id: id.into(),
        attempt: 1,
        account_id: "a".into(),
        account_kind: "oauth".into(),
        outcome: "succeeded".into(),
        output_tokens: Some(5),
        speed: speed.map(String::from),
        ..Default::default()
    };
    {
        let db = phoebis_ledger::LedgerDb::open(&path).unwrap();
        db.insert_request(&request("r0"), &[attempt("r0", None)], None)
            .unwrap();
        // SQLite cannot drop a primary-key column, so the version-21 table is rebuilt from the live one with
        // the speed left out; the indexes go with the table it replaces, and the migration has to add them back.
        db.write(|c| {
            let columns: String = c.query_row(
                "SELECT group_concat(name, ', ') FROM pragma_table_info('rollups_hourly') WHERE name != 'speed'",
                [],
                |r| r.get(0),
            )?;
            c.execute_batch(&format!(
                "CREATE TABLE rollups_hourly_prev AS SELECT {columns} FROM rollups_hourly; \
                 DROP TABLE rollups_hourly; \
                 ALTER TABLE rollups_hourly_prev RENAME TO rollups_hourly; \
                 ALTER TABLE requests DROP COLUMN held; \
                 ALTER TABLE attempts DROP COLUMN upstream_message_id; {RECREATE_DROPPED_TABLE}\n\
                 UPDATE meta SET value = '21' WHERE key = 'schema_version';"
            ))?;
            Ok(())
        })
        .unwrap();
    }
    let db = std::sync::Arc::new(phoebis_ledger::LedgerDb::open(&path).unwrap());
    assert_eq!(db.schema_version().await.unwrap(), 26);
    let speeds = |ledger: &phoebis_ledger::LedgerDb| {
        ledger
            .read_blocking(|c| {
                let mut stmt =
                    c.prepare("SELECT speed, requests FROM rollups_hourly ORDER BY speed")?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .unwrap()
    };
    assert_eq!(
        speeds(&db),
        vec![("standard".to_string(), 1)],
        "a row folded before the column existed files under standard and keeps its count"
    );
    db.insert_request(&request("r1"), &[attempt("r1", Some("fast"))], None)
        .unwrap();
    assert_eq!(
        speeds(&db),
        vec![("fast".to_string(), 1), ("standard".to_string(), 1)],
        "the key admits a second row that differs from the first in its speed alone"
    );
    for index in ["rollups_hourly_key", "rollups_hourly_member"] {
        let present = db
            .read_blocking(|c| {
                Ok(c
                    .query_row(
                        "SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1 AND tbl_name = 'rollups_hourly'",
                        [index],
                        |_| Ok(()),
                    )
                    .is_ok())
            })
            .unwrap();
        assert!(present, "{index} stands on the rebuilt table");
    }
}

#[tokio::test]
async fn migration_twenty_four_files_a_rollup_row_under_fast_funding_by_its_speed_and_plan_otherwise(
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    {
        let db = phoebis_ledger::LedgerDb::open(&path).unwrap();
        // No version-23 row kept an overage reading, so this one files under plan once migrated.
        plant_funded(&db, ("a", "oauth"), "overage", None, Some(true));
        plant_funded(&db, ("a", "oauth"), "fast", Some("fast"), None);
        // SQLite cannot drop a primary-key column, so the version-23 table is rebuilt from the live one with
        // the funding left out; the indexes go with the table it replaces, and the migration has to add them back.
        db.write(|c| {
            let columns: String = c.query_row(
                "SELECT group_concat(name, ', ') FROM pragma_table_info('rollups_hourly') WHERE name != 'funding'",
                [],
                |r| r.get(0),
            )?;
            c.execute_batch(&format!(
                "CREATE TABLE rollups_hourly_prev AS SELECT {columns} FROM rollups_hourly; \
                 DROP TABLE rollups_hourly; \
                 ALTER TABLE rollups_hourly_prev RENAME TO rollups_hourly; \
                 ALTER TABLE requests DROP COLUMN held; \
                 ALTER TABLE attempts DROP COLUMN upstream_message_id; \
                 UPDATE meta SET value = '23' WHERE key = 'schema_version';"
            ))?;
            Ok(())
        })
        .unwrap();
    }
    let db = std::sync::Arc::new(phoebis_ledger::LedgerDb::open(&path).unwrap());
    assert_eq!(db.schema_version().await.unwrap(), 26);
    let mut rows = db
        .read_blocking(|c| phoebis_ledger::read_rollups(c, 0, i64::MAX))
        .unwrap()
        .into_iter()
        .map(|r| (r.speed, r.funding, r.requests))
        .collect::<Vec<_>>();
    rows.sort();
    assert_eq!(
        rows,
        vec![
            ("fast".to_string(), "fast".to_string(), 1),
            ("standard".to_string(), "plan".to_string(), 1),
        ],
        "a fast row files under fast funding, any other under plan, each keeping its count"
    );
    for index in ["rollups_hourly_key", "rollups_hourly_member"] {
        let present = db
            .read_blocking(|c| {
                Ok(c
                    .query_row(
                        "SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1 AND tbl_name = 'rollups_hourly'",
                        [index],
                        |_| Ok(()),
                    )
                    .is_ok())
            })
            .unwrap();
        assert!(present, "{index} stands on the rebuilt table");
    }
}

#[tokio::test]
async fn migration_twenty_five_gives_a_request_row_from_before_no_hold() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    {
        let db = phoebis_ledger::LedgerDb::open(&path).unwrap();
        db.insert_request(
            &RequestRecord {
                id: "r0".into(),
                canonical_model: "claude-sonnet-5".into(),
                model: "claude-sonnet-5".into(),
                kind: "inference".into(),
                origin: "client".into(),
                outcome: "succeeded".into(),
                ..Default::default()
            },
            &[],
            None,
        )
        .unwrap();
        db.write(|c| {
            c.execute_batch(
                "ALTER TABLE requests DROP COLUMN held; \
                 ALTER TABLE attempts DROP COLUMN upstream_message_id; \
                 ALTER TABLE rollups_hourly DROP COLUMN incomplete; \
                 UPDATE meta SET value = '24' WHERE key = 'schema_version';",
            )?;
            Ok(())
        })
        .unwrap();
    }
    let db = std::sync::Arc::new(phoebis_ledger::LedgerDb::open(&path).unwrap());
    assert_eq!(db.schema_version().await.unwrap(), 26);
    let (row, _) = db.request("r0").await.unwrap().unwrap();
    assert_eq!(
        row.held, None,
        "a request from before the column waited on nothing it recorded"
    );
}

#[tokio::test]
async fn migration_twenty_six_adds_the_incomplete_counter_and_the_message_id_column_to_a_version_twenty_five_file(
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    {
        let db = phoebis_ledger::LedgerDb::open(&path).unwrap();
        db.insert_request(
            &RequestRecord {
                id: "r0".into(),
                received_at: 3_600_000,
                key_id: Some("desktop".into()),
                pool_id: Some("main".into()),
                canonical_model: "claude-sonnet-5".into(),
                model: "claude-sonnet-5".into(),
                kind: "inference".into(),
                origin: "client".into(),
                outcome: "succeeded".into(),
                attempts: 1,
                ..Default::default()
            },
            &[AttemptRecord {
                request_id: "r0".into(),
                attempt: 1,
                account_id: "a".into(),
                account_kind: "oauth".into(),
                outcome: "succeeded".into(),
                output_tokens: Some(5),
                ..Default::default()
            }],
            None,
        )
        .unwrap();
        db.write(|c| {
            c.execute_batch(
                "ALTER TABLE rollups_hourly DROP COLUMN incomplete; \
                 ALTER TABLE attempts DROP COLUMN upstream_message_id; \
                 UPDATE meta SET value = '25' WHERE key = 'schema_version';",
            )?;
            Ok(())
        })
        .unwrap();
    }
    let db = std::sync::Arc::new(phoebis_ledger::LedgerDb::open(&path).unwrap());
    assert_eq!(db.schema_version().await.unwrap(), 26);
    let rows = db
        .read_blocking(|c| phoebis_ledger::read_rollups(c, 0, i64::MAX))
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| (r.requests, r.succeeded, r.incomplete))
            .collect::<Vec<_>>(),
        vec![(1, 1, 0)],
        "a rollup row from before the column keeps its counts and counts no incomplete answer"
    );
    let message_ids = db
        .read_blocking(|c| {
            let mut stmt = c.prepare("SELECT upstream_message_id FROM attempts")?;
            let ids = stmt
                .query_map([], |r| r.get::<_, Option<String>>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(ids)
        })
        .unwrap();
    assert_eq!(
        message_ids,
        vec![None],
        "an attempt from before the column names no message"
    );
}

#[test]
fn an_incomplete_request_counts_in_the_rollups_incomplete_column_and_nowhere_else() {
    let db = phoebis_ledger::LedgerDb::open_in_memory().unwrap();
    db.insert_request(
        &RequestRecord {
            id: "r0".into(),
            received_at: 3_600_000,
            key_id: Some("desktop".into()),
            key_generation: 1,
            pool_id: Some("main".into()),
            canonical_model: "claude-sonnet-5".into(),
            model: "claude-sonnet-5".into(),
            stream: true,
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(200),
            outcome: "incomplete".into(),
            attempts: 1,
            error_code: Some("stream_incomplete".into()),
            ..Default::default()
        },
        &[AttemptRecord {
            request_id: "r0".into(),
            attempt: 1,
            account_id: "a".into(),
            account_kind: "oauth".into(),
            outcome: "upstream_error".into(),
            upstream_status: Some(200),
            output_tokens: Some(5),
            ..Default::default()
        }],
        None,
    )
    .unwrap();
    let rows = db
        .read_blocking(|c| phoebis_ledger::read_rollups(c, 0, i64::MAX))
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| (
                r.requests,
                r.incomplete,
                r.succeeded,
                r.failed,
                r.rate_limited,
                r.exhausted,
                r.rejected
            ))
            .collect::<Vec<_>>(),
        vec![(1, 1, 0, 0, 0, 0, 0)],
        "the answer that ended early is counted once, in its own column"
    );
    let (day, week) = db
        .read_blocking(|c| phoebis_ledger::key_usage_windows(c, "desktop", 1, 7_200_000))
        .unwrap();
    for window in [day, week] {
        assert_eq!(
            (
                window.requests,
                window.succeeded,
                window.failed,
                window.incomplete
            ),
            (1, 0, 0, 1),
            "the key's own usage reports it in its own count beside failed"
        );
    }
}

#[test]
fn a_request_folds_under_its_class_and_a_classless_one_under_the_reserved_value() {
    let db = phoebis_ledger::LedgerDb::open_in_memory().unwrap();
    let plant = |id: &str, class: Option<&str>| {
        db.insert_request(
            &RequestRecord {
                id: id.into(),
                received_at: 3_600_000,
                key_id: Some("k".into()),
                pool_id: Some("p".into()),
                canonical_model: "m".into(),
                kind: "inference".into(),
                outcome: "succeeded".into(),
                request_class: class.map(String::from),
                ..Default::default()
            },
            &[phoebis_ledger::AttemptRecord {
                request_id: id.into(),
                attempt: 1,
                account_id: "a".into(),
                account_kind: "oauth".into(),
                outcome: "succeeded".into(),
                ..Default::default()
            }],
            None,
        )
        .unwrap();
    };
    plant("r1", Some("main"));
    plant("r2", None);
    let rows = db
        .read_blocking(|c| phoebis_ledger::read_rollups(c, 0, i64::MAX))
        .unwrap();
    let classes: Vec<&str> = rows.iter().map(|r| r.request_class.as_str()).collect();
    assert_eq!(
        rows.len(),
        2,
        "the named class and the reserved one are separate rows"
    );
    assert!(
        classes.contains(&"main") && classes.contains(&phoebis_ledger::NO_CLASS),
        "every other key part is equal, so only the class tells the two rows apart: {classes:?}"
    );
}

#[test]
fn an_attempt_keeps_its_selection_trace_with_the_cache_read_folded_in() {
    let trace = r#"{"strategy":"pace","tier":[0,1,0],"renewed":false,"moved":null,"skipped":[]}"#;
    let mut assembler = Assembler::new(Arc::new(|| NOW));
    assembler.observe(observed_event("r1"));
    assembler.observe(selected_event_with_trace("r1", 1, "a", minutes(1), trace));
    let BusEvent::RequestComplete(mut complete) = complete_event("r1") else {
        unreachable!()
    };
    complete.attempt = 1;
    complete.account_id = Some("a".into());
    complete.usage = Some(Usage {
        cache_read_tokens: Some(42),
        ..Usage::default()
    });
    let records = assembler.observe(BusEvent::RequestComplete(complete));
    let [Record::Request { attempts, .. }] = records.as_slice() else {
        panic!("{} records at completion", records.len());
    };
    let selection: serde_json::Value =
        serde_json::from_str(attempts[0].selection.as_deref().unwrap()).unwrap();
    assert_eq!(selection["strategy"], "pace");
    assert_eq!(selection["cache_read"], 42);
}

#[test]
fn a_fast_attempt_prices_at_the_fast_cells_beside_a_standard_one() {
    let table = phoebis_ledger::PriceTable::shipped();
    let attempt = |attempt: u32, speed: Option<&str>| AttemptRecord {
        request_id: "r1".into(),
        attempt,
        input_tokens: Some(1_000_000),
        output_tokens: Some(100_000),
        speed: speed.map(String::from),
        ..Default::default()
    };
    let mut attempts = vec![attempt(1, Some("fast")), attempt(2, None)];
    let unknown = std::sync::atomic::AtomicU64::new(0);
    let split = phoebis_ledger::price_attempts(&table, "claude-opus-5-5", &mut attempts, &unknown);
    let priced = |a: &AttemptRecord| (a.estimated_usd, a.input_usd, a.output_usd);
    // Opus 5.5 lists $8 input and $40 output per million at the fast cells, $4 and $20 at the standard ones.
    assert_eq!(
        priced(&attempts[0]),
        (Some(12.0), Some(8.0), Some(4.0)),
        "the answer that reported speed fast prices at the fast cells"
    );
    assert_eq!(
        priced(&attempts[1]),
        (Some(6.0), Some(4.0), Some(2.0)),
        "the one that reported none at the standard cells"
    );
    assert_eq!(split.total(), 18.0);
    assert_eq!(
        (
            attempts[0].pricing_version.as_deref(),
            unknown.load(Ordering::SeqCst)
        ),
        (Some("2026-10-01"), 0)
    );
}

/// The account's rewrite tokens over the whole table, for a test that checks the figure after each write.
fn rewrite_tokens_of(db: &phoebis_ledger::LedgerDb, account_id: &str) -> u64 {
    db.read_blocking(|c| phoebis_ledger::read_account_rollups(c, 0, i64::MAX))
        .unwrap()
        .into_iter()
        .find(|r| r.account_id == account_id)
        .map(|r| r.rewrite_tokens)
        .unwrap_or(0)
}

#[test]
fn the_first_request_after_a_move_books_its_cache_writes_as_rewrite_tokens_and_the_next_one_does_not(
) {
    let db = phoebis_ledger::LedgerDb::open_in_memory().unwrap();
    let moved_trace =
        r#"{"strategy":"pace","tier":null,"renewed":false,"moved":"shed","skipped":[]}"#;
    let unmoved_trace =
        r#"{"strategy":"pace","tier":[0,1,0],"renewed":true,"moved":null,"skipped":[]}"#;
    let r1 = RequestRecord {
        id: "r1".into(),
        received_at: 3_600_000,
        key_id: Some("k".into()),
        pool_id: Some("p".into()),
        canonical_model: "m".into(),
        kind: "inference".into(),
        outcome: "succeeded".into(),
        attempts: 1,
        ..Default::default()
    };
    let r1_attempt = phoebis_ledger::AttemptRecord {
        request_id: "r1".into(),
        attempt: 1,
        account_id: "b".into(),
        account_kind: "oauth".into(),
        outcome: "succeeded".into(),
        cache_write_5m_tokens: Some(100),
        cache_write_1h_tokens: Some(50),
        selection: Some(moved_trace.into()),
        ..Default::default()
    };
    db.insert_request(&r1, std::slice::from_ref(&r1_attempt), None)
        .unwrap();
    assert_eq!(rewrite_tokens_of(&db, "b"), 150);
    // A replay of the same request (the writer retries a record it never saw acknowledged) must not double
    // the booking: the old serving attempt's write comes out before the new one goes back in.
    db.insert_request(&r1, &[r1_attempt], None).unwrap();
    assert_eq!(
        rewrite_tokens_of(&db, "b"),
        150,
        "a replace subtracts the old booking before adding the new one"
    );
    db.insert_request(
        &RequestRecord {
            id: "r2".into(),
            received_at: 3_600_500,
            key_id: Some("k".into()),
            pool_id: Some("p".into()),
            canonical_model: "m".into(),
            kind: "inference".into(),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        &[phoebis_ledger::AttemptRecord {
            request_id: "r2".into(),
            attempt: 1,
            account_id: "b".into(),
            account_kind: "oauth".into(),
            outcome: "succeeded".into(),
            cache_write_5m_tokens: Some(30),
            selection: Some(unmoved_trace.into()),
            ..Default::default()
        }],
        None,
    )
    .unwrap();
    assert_eq!(
        rewrite_tokens_of(&db, "b"),
        150,
        "a renewed pin's trace names no move, so its cache write never counts as a rewrite"
    );
}

#[test]
fn a_retried_pass_of_a_moved_leg_books_the_attempt_that_carries_the_usage() {
    let db = phoebis_ledger::LedgerDb::open_in_memory().unwrap();
    // Every pass of one leg republishes the same trace; `finish` writes usage onto the serving attempt alone,
    // so the first pass here carries the move but nothing to book, and the second carries both.
    let moved_trace =
        r#"{"strategy":"pace","tier":null,"renewed":false,"moved":"shed","skipped":[]}"#;
    db.insert_request(
        &RequestRecord {
            id: "r1".into(),
            received_at: 3_600_000,
            key_id: Some("k".into()),
            pool_id: Some("p".into()),
            canonical_model: "m".into(),
            kind: "inference".into(),
            outcome: "succeeded".into(),
            attempts: 2,
            ..Default::default()
        },
        &[
            phoebis_ledger::AttemptRecord {
                request_id: "r1".into(),
                attempt: 1,
                account_id: "b".into(),
                account_kind: "oauth".into(),
                outcome: "transport_error".into(),
                started_at: 3_600_000,
                selection: Some(moved_trace.into()),
                ..Default::default()
            },
            phoebis_ledger::AttemptRecord {
                request_id: "r1".into(),
                attempt: 2,
                account_id: "b".into(),
                account_kind: "oauth".into(),
                outcome: "succeeded".into(),
                started_at: 3_600_010,
                cache_write_5m_tokens: Some(100),
                cache_write_1h_tokens: Some(50),
                selection: Some(moved_trace.into()),
                ..Default::default()
            },
        ],
        None,
    )
    .unwrap();
    assert_eq!(
        rewrite_tokens_of(&db, "b"),
        150,
        "the retried pass shares the earlier attempt's trace, but only the serving attempt has usage to book"
    );
}

#[test]
fn a_rate_limit_fail_over_whose_destination_writes_cache_books_its_rewrite_tokens_under_cooldown() {
    let db = phoebis_ledger::LedgerDb::open_in_memory().unwrap();
    // A double move: the first attempt's own trace already names one (it is where an earlier shed landed),
    // and its cache write must never be the one booked once a second move, the fail-over to B, follows it.
    let shed_trace =
        r#"{"strategy":"pace","tier":null,"renewed":false,"moved":"shed","skipped":[]}"#;
    let cooldown_trace =
        r#"{"strategy":"pace","tier":null,"renewed":false,"moved":"cooldown","skipped":[]}"#;
    db.insert_request(
        &RequestRecord {
            id: "r1".into(),
            received_at: 3_600_000,
            key_id: Some("k".into()),
            pool_id: Some("p".into()),
            canonical_model: "m".into(),
            kind: "inference".into(),
            outcome: "succeeded".into(),
            attempts: 2,
            ..Default::default()
        },
        &[
            phoebis_ledger::AttemptRecord {
                request_id: "r1".into(),
                attempt: 1,
                account_id: "a".into(),
                account_kind: "oauth".into(),
                outcome: "rate_limited".into(),
                started_at: 3_600_000,
                cache_write_5m_tokens: Some(999),
                selection: Some(shed_trace.into()),
                ..Default::default()
            },
            phoebis_ledger::AttemptRecord {
                request_id: "r1".into(),
                attempt: 2,
                account_id: "b".into(),
                account_kind: "oauth".into(),
                outcome: "succeeded".into(),
                started_at: 3_600_050,
                cache_write_5m_tokens: Some(80),
                selection: Some(cooldown_trace.into()),
                ..Default::default()
            },
        ],
        None,
    )
    .unwrap();
    db.write(|c| {
        let tx = c.transaction()?;
        phoebis_ledger::fold_event(
            &tx,
            &EventRecord {
                at: 3_600_100,
                kind: "pin_migrated".into(),
                account_id: Some("b".into()),
                detail: Some(r#"{"reason":"cooldown","from_account":"a"}"#.into()),
                ..Default::default()
            },
        )?;
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    let rows = db
        .read_blocking(|c| phoebis_ledger::read_account_rollups(c, 0, i64::MAX))
        .unwrap();
    let row = rows.iter().find(|r| r.account_id == "b").unwrap();
    assert_eq!(row.rewrite_tokens, 80);
    assert_eq!(row.migrations.get("cooldown"), Some(&1));
    assert_eq!(
        rewrite_tokens_of(&db, "a"),
        0,
        "the double move books only the serving leg's write; the earlier account gets nothing"
    );
}

/// A UTC instant, for the member-spend writer-loop tests below.
fn ms(y: i16, m: i8, d: i8, h: i8, mi: i8) -> i64 {
    jiff::civil::date(y, m, d)
        .at(h, mi, 0, 0)
        .to_zoned(jiff::tz::TimeZone::UTC)
        .unwrap()
        .timestamp()
        .as_millisecond()
}

/// A succeeded request of `input_tokens` uncached Sonnet 5 input on `main`'s member `a`, received at `at`.
fn plant_spend(db: &phoebis_ledger::LedgerDb, id: &str, at: i64, input_tokens: u64) {
    db.insert_request(
        &RequestRecord {
            id: id.into(),
            received_at: at,
            finished_at: Some(at + 500),
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            canonical_model: "claude-sonnet-5".into(),
            model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        &[AttemptRecord {
            request_id: id.into(),
            attempt: 1,
            account_id: "a".into(),
            account_kind: "api_key".into(),
            outcome: "succeeded".into(),
            input_tokens: Some(input_tokens),
            ..Default::default()
        }],
        None,
    )
    .unwrap();
}

#[test]
fn a_member_with_two_budgets_gets_a_spend_row_per_period() {
    let db = phoebis_ledger::LedgerDb::open_in_memory().unwrap();
    let now = ms(2026, 1, 15, 12, 0);
    plant_spend(&db, "today", ms(2026, 1, 15, 1, 0), 1_000_000);
    plant_spend(&db, "earlier", ms(2026, 1, 5, 1, 0), 2_000_000);
    plant_spend(&db, "last-month", ms(2025, 12, 31, 1, 0), 4_000_000);
    let budgeted = |period| BudgetedMember {
        pool_id: "main".into(),
        account_id: "a".into(),
        period,
        metered_only: false,
    };
    let rows = db
        .read_blocking(|c| {
            member_spends(
                c,
                &[budgeted(BudgetPeriod::Day), budgeted(BudgetPeriod::Month)],
                now,
                &PriceTable::shipped(),
            )
        })
        .unwrap();
    let row = |period| rows.iter().find(|r| r.period == period).unwrap();
    let (day, month) = (row(BudgetPeriod::Day), row(BudgetPeriod::Month));
    assert_eq!(rows.len(), 2, "one row per budgeted period: {rows:?}");
    assert_eq!(
        (day.period_start, day.period_end),
        (ms(2026, 1, 15, 0, 0), ms(2026, 1, 16, 0, 0))
    );
    assert_eq!(
        (month.period_start, month.period_end),
        (ms(2026, 1, 1, 0, 0), ms(2026, 2, 1, 0, 0))
    );
    assert!(day.spent_usd > 0.0, "today's request is priced");
    assert_eq!(
        month.spent_usd,
        3.0 * day.spent_usd,
        "the month sums today's and the earlier day's, never last month's"
    );
}

/// A succeeded request of a million uncached Opus 5 input tokens on `main`'s member `account` of `kind`, its last
/// attempt served at `speed` with the head's overage reading `overage_in_use`.
fn plant_funded(
    db: &phoebis_ledger::LedgerDb,
    (account, kind): (&str, &str),
    id: &str,
    speed: Option<&str>,
    overage_in_use: Option<bool>,
) {
    let at = ms(2026, 1, 15, 1, 0);
    let id = format!("{account}-{id}");
    db.insert_request(
        &RequestRecord {
            id: id.clone(),
            received_at: at,
            finished_at: Some(at + 500),
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            canonical_model: "claude-opus-5".into(),
            model: "claude-opus-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        &[AttemptRecord {
            request_id: id,
            attempt: 1,
            account_id: account.into(),
            account_kind: kind.into(),
            outcome: "succeeded".into(),
            input_tokens: Some(1_000_000),
            speed: speed.map(String::from),
            overage_in_use,
            ..Default::default()
        }],
        None,
    )
    .unwrap();
}

/// The five funding cases on one member in one hour: overage in use, the plan, and three fast answers, one with no
/// overage reading, one whose head said overage was not in use and one whose head said it was.
fn plant_every_funding(db: &phoebis_ledger::LedgerDb, member: (&str, &str)) {
    plant_funded(db, member, "overage", None, Some(true));
    plant_funded(db, member, "plan", None, Some(false));
    plant_funded(db, member, "fast", Some("fast"), None);
    plant_funded(db, member, "fast-on-plan", Some("fast"), Some(false));
    plant_funded(db, member, "fast-in-overage", Some("fast"), Some(true));
}

#[test]
fn an_attempt_folds_under_fast_overage_or_plan_funding() {
    let db = phoebis_ledger::LedgerDb::open_in_memory().unwrap();
    plant_every_funding(&db, ("a", "oauth"));
    let mut rows = db
        .read_blocking(|c| phoebis_ledger::read_rollups(c, 0, i64::MAX))
        .unwrap()
        .into_iter()
        .map(|r| (r.funding, r.speed, r.requests))
        .collect::<Vec<_>>();
    rows.sort();
    assert_eq!(
        rows,
        vec![
            ("fast".to_string(), "fast".to_string(), 3),
            ("overage".to_string(), "standard".to_string(), 1),
            ("plan".to_string(), "standard".to_string(), 1),
        ],
        "a fast answer draws usage credits whatever the overage flag, so all three file under fast"
    );
}

#[test]
fn member_spends_sums_the_metered_rows_alone_when_asked() {
    let db = phoebis_ledger::LedgerDb::open_in_memory().unwrap();
    plant_every_funding(&db, ("a", "oauth"));
    let spent = |metered_only| {
        db.read_blocking(|c| {
            member_spends(
                c,
                &[BudgetedMember {
                    pool_id: "main".into(),
                    account_id: "a".into(),
                    period: BudgetPeriod::Day,
                    metered_only,
                }],
                ms(2026, 1, 15, 12, 0),
                &PriceTable::shipped(),
            )
        })
        .unwrap()[0]
            .spent_usd
    };
    assert_eq!(
        spent(false),
        5.0 + 5.0 + 10.0 + 10.0 + 10.0,
        "every row: two million standard input tokens at $5 and three million fast at $10"
    );
    assert_eq!(
        spent(true),
        5.0 + 10.0 + 10.0 + 10.0,
        "the overage row and the three fast answers; the plan's own traffic is no spend"
    );
}

#[tokio::test]
async fn a_subscription_members_spend_reads_its_metered_rows_alone() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(
        &up,
        &[oauth("a", A, 0), api_key("b", C, 1)],
        (0, None),
        LedgerConfig::default(),
    )
    .await;
    h.clock.store(ms(2026, 1, 15, 12, 0), Ordering::SeqCst);
    // Planted before the budgets land, so whichever refresh reports both members has read every row.
    plant_every_funding(&h.db, ("a", "oauth"));
    plant_every_funding(&h.db, ("b", "api_key"));
    let budgeted = |account_id: &str| MemberWrite {
        account_id: account_id.into(),
        priority: 0,
        budget_daily_usd: None,
        budget_monthly_usd: Some(1_000.0),
    };
    h.store
        .set_pool_members("main", &[budgeted("a"), budgeted("b")], "test")
        .unwrap();
    h.router.reload().await.unwrap();
    let spent = wait_for_async(Duration::from_secs(5), || async {
        let rows = h.router.member_spends().await.unwrap();
        let of = |account: &str| {
            rows.iter()
                .find(|r| r.pool_id == "main" && r.account_id == account)
                .map(|r| r.spent_usd)
        };
        Some((of("a")?, of("b")?))
    })
    .await;
    assert_eq!(
        spent,
        (5.0 + 10.0 + 10.0 + 10.0, 5.0 + 5.0 + 10.0 + 10.0 + 10.0),
        "the subscription member's month reads its overage row and its fast answers and never its plan row; the \
         API-key member's reads every row"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_ledger_reports_every_budgeted_members_spend_after_a_flush() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    // An API-key member, whose whole spend counts: the fake's answer reports no overage, so a subscription
    // member's budget would read the relayed request as plan traffic and never move.
    let h = harness(
        &up,
        &[api_key("a", C, 0)],
        (0, None),
        LedgerConfig::default(),
    )
    .await;
    h.store
        .set_pool_members(
            "main",
            &[MemberWrite {
                account_id: "a".into(),
                priority: 0,
                budget_daily_usd: None,
                budget_monthly_usd: Some(100.0),
            }],
            "test",
        )
        .unwrap();
    h.router.reload().await.unwrap();
    // Waits for the revision-driven refresh's own zero row before sending the request: without this, the
    // positive spend below could come from a refresh that happened to land after the request rather than from
    // the flush the test means to pin, whenever the two records batch together.
    wait_for_async(Duration::from_secs(5), || async {
        h.router
            .member_spends()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.pool_id == "main" && r.account_id == "a")
    })
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-5[1m]", "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.into_body().collect().await.unwrap();
    // Polled for a positive spend rather than a mere non-empty row: a revision-driven refresh can run before
    // the priced request's own flush lands, reporting a correct-but-premature zero the flush then supersedes.
    let rows = wait_for_async(Duration::from_secs(5), || async {
        let rows = h.router.member_spends().await.unwrap();
        rows.iter().any(|r| r.spent_usd > 0.0).then_some(rows)
    })
    .await;
    assert_eq!(
        (rows[0].pool_id.as_str(), rows[0].account_id.as_str()),
        ("main", "a")
    );
    assert!(
        rows[0].spent_usd > 0.0,
        "the flush that wrote the priced attempt reached the member's spend"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn an_exhausted_monthly_member_changed_to_daily_is_admitted_while_no_member_can_serve() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(
        &up,
        &[oauth("a", A, 0)],
        (0, None),
        LedgerConfig {
            // Production's wake for a registry edit is the event mirror's poll, not the hourly purge tick; a
            // short `event_poll` drives this test through the same path rather than a proxy for it.
            event_poll: Duration::from_millis(20),
            ..LedgerConfig::default()
        },
    )
    .await;
    // Planted directly rather than relayed: an earlier day of the same month, so the monthly sum is exhausted
    // while the day it falls in has already passed. In overage, since a subscription member's budget counts its
    // metered spend alone.
    h.db.insert_request(
        &RequestRecord {
            id: "seed".into(),
            received_at: ms(2026, 1, 5, 0, 0),
            finished_at: Some(ms(2026, 1, 5, 0, 0) + 500),
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            canonical_model: "claude-sonnet-5".into(),
            model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        &[AttemptRecord {
            request_id: "seed".into(),
            attempt: 1,
            account_id: "a".into(),
            account_kind: "oauth".into(),
            outcome: "succeeded".into(),
            input_tokens: Some(1_000_000),
            overage_in_use: Some(true),
            ..Default::default()
        }],
        None,
    )
    .unwrap();
    h.clock.store(ms(2026, 1, 20, 12, 0), Ordering::SeqCst);
    h.store
        .set_pool_members(
            "main",
            &[MemberWrite {
                account_id: "a".into(),
                priority: 0,
                budget_daily_usd: None,
                budget_monthly_usd: Some(0.01),
            }],
            "test",
        )
        .unwrap();
    h.router.reload().await.unwrap();
    let exhausted = wait_for_async(Duration::from_secs(5), || async {
        h.router
            .member_spends()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.pool_id == "main" && r.account_id == "a" && r.spent_usd >= 0.01)
    })
    .await;
    assert_eq!(exhausted.period, BudgetPeriod::Month);

    // The member is genuinely excluded before the edit: the documented 429 with the overage claim, so the
    // 200 after the edit below actually shows the edit's effect rather than a refusal that never happened.
    let refused = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-5[1m]", "s2"))
        .await
        .unwrap();
    assert_eq!(refused.status(), 429);
    assert_eq!(
        refused
            .headers()
            .get("anthropic-ratelimit-unified-representative-claim")
            .map(|v| v.to_str().unwrap()),
        Some("overage")
    );

    // The registry's period changes with no request flowing; the revision alone drives the ledger's next read.
    h.store
        .set_pool_members(
            "main",
            &[MemberWrite {
                account_id: "a".into(),
                priority: 0,
                budget_daily_usd: Some(0.01),
                budget_monthly_usd: None,
            }],
            "test",
        )
        .unwrap();
    h.router.reload().await.unwrap();
    let admitted = wait_for_async(Duration::from_secs(5), || async {
        h.router
            .member_spends()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.pool_id == "main" && r.account_id == "a" && r.period == BudgetPeriod::Day)
    })
    .await;
    assert!(
        admitted.spent_usd < 0.01,
        "today carries none of the earlier day's spend"
    );
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-5[1m]", "s1"))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "the day row below the budget lets a select admit the member"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_period_boundary_tick_refreshes_the_spend_without_traffic() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(
        &up,
        &[oauth("a", A, 0)],
        (0, None),
        LedgerConfig {
            purge_interval: Duration::from_millis(20),
            ..LedgerConfig::default()
        },
    )
    .await;
    h.clock.store(ms(2026, 1, 15, 12, 0), Ordering::SeqCst);
    // Planted directly rather than relayed: priced usage past the budget, inside the day the tick will close, in
    // overage, since a subscription member's budget counts its metered spend alone.
    h.db.insert_request(
        &RequestRecord {
            id: "seed".into(),
            received_at: ms(2026, 1, 15, 1, 0),
            finished_at: Some(ms(2026, 1, 15, 1, 0) + 500),
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            canonical_model: "claude-sonnet-5".into(),
            model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        &[AttemptRecord {
            request_id: "seed".into(),
            attempt: 1,
            account_id: "a".into(),
            account_kind: "oauth".into(),
            outcome: "succeeded".into(),
            input_tokens: Some(51_000_000),
            overage_in_use: Some(true),
            ..Default::default()
        }],
        None,
    )
    .unwrap();
    h.store
        .set_pool_members(
            "main",
            &[MemberWrite {
                account_id: "a".into(),
                priority: 0,
                budget_daily_usd: Some(100.0),
                budget_monthly_usd: None,
            }],
            "test",
        )
        .unwrap();
    h.router.reload().await.unwrap();
    wait_for_async(Duration::from_secs(5), || async {
        h.router
            .member_spends()
            .await
            .unwrap()
            .into_iter()
            .find(|r| {
                r.pool_id == "main"
                    && r.account_id == "a"
                    && r.period_end == ms(2026, 1, 16, 0, 0)
                    && r.spent_usd >= 100.0
            })
    })
    .await;

    // The member is genuinely excluded before the tick: the documented 429 with the overage claim.
    let refused = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-5[1m]", "s2"))
        .await
        .unwrap();
    assert_eq!(refused.status(), 429);
    assert_eq!(
        refused
            .headers()
            .get("anthropic-ratelimit-unified-representative-claim")
            .map(|v| v.to_str().unwrap()),
        Some("overage")
    );

    // The clock alone steps past the period's end; no request and no registry edit follows. The wait below is
    // what proves the tick fired: `budget_exhausted` stops blocking once the clock passes `period_end` on its
    // own, so the request's 200 that follows would hold with or without a refresh, but this wait only ends once
    // the tick has actually reported the new period's row.
    h.clock.store(ms(2026, 1, 16, 0, 5), Ordering::SeqCst);
    wait_for_async(Duration::from_secs(5), || async {
        h.router
            .member_spends()
            .await
            .unwrap()
            .into_iter()
            .find(|r| {
                r.pool_id == "main" && r.account_id == "a" && r.period_end == ms(2026, 1, 17, 0, 0)
            })
    })
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-5[1m]", "s1"))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "the member is admitted again once the new period's row lands"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_registry_change_recorded_ahead_of_the_reload_still_reaches_the_writer() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(
        &up,
        &[oauth("a", A, 0)],
        (0, None),
        LedgerConfig {
            event_poll: Duration::from_millis(20),
            ..LedgerConfig::default()
        },
    )
    .await;
    h.store
        .set_pool_members(
            "main",
            &[MemberWrite {
                account_id: "a".into(),
                priority: 0,
                budget_daily_usd: None,
                budget_monthly_usd: Some(100.0),
            }],
            "test",
        )
        .unwrap();
    // Waits for the mirror to have already recorded the change before the router learns of it: the writer
    // wakes on this very record while its own revision check still reads the snapshot from before the edit,
    // the ordering that starves the check without a periodic re-check of its own.
    wait_for_async(Duration::from_secs(5), || async {
        h.db.read_blocking(|c| phoebis_ledger::recent_events_of_kind(c, "registry_changed", 5))
            .unwrap()
            .into_iter()
            .find(|e| {
                e.detail
                    .as_deref()
                    .is_some_and(|d| d.contains("pool_members_set"))
            })
    })
    .await;
    h.router.reload().await.unwrap();
    wait_for_async(Duration::from_secs(5), || async {
        h.router
            .member_spends()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.pool_id == "main" && r.account_id == "a")
    })
    .await;
    up.shutdown().await;
}
