//! The acceptance row "Ledger": every request has one row and one row per attempt; refusals have a row and
//! no attempt; a saturated channel produces a ledger_gap whose count equals the dropped records.
mod common;
use std::time::Duration;

use phoebis_bus::{
    Event as BusEvent, Kind, Origin, Outcome as BusOutcome, RequestComplete, RequestObserved,
    RequestShape, ResponseChunk, ResponseFacts,
};
use phoebis_ledger::{
    list_requests, EventRecord, LedgerConfig, LiveKeys, Record, RequestFilter, RequestRecord,
};
use phoebis_profile::fixtures::BodyFixture;
use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule, SseEvent};
use http_body_util::BodyExt;
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
                allowed_models: Some(vec!["claude-sonnet-4-5".into()]),
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
            phoebis_ledger::list_requests(c, &filter, &phoebis_ledger::LiveKeys::default(), None, 10)
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
    let (line, headers) = phoebis_transport::request::parse_head(
        phoebis_profile::fixtures::inference_head(phoebis_profile::fixtures::FixtureKind::OauthFirstParty),
    )
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
            phoebis_ledger::list_requests(c, &filter, &phoebis_ledger::LiveKeys::default(), None, 10)
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
            phoebis_ledger::list_requests(c, &filter, &phoebis_ledger::LiveKeys::default(), None, 10)
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
            phoebis_ledger::list_requests(c, &filter, &phoebis_ledger::LiveKeys::default(), None, 10)
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
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
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
            phoebis_ledger::list_requests(c, &filter, &phoebis_ledger::LiveKeys::default(), None, 10)
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
    let (line, headers) = phoebis_transport::request::parse_head(
        phoebis_profile::fixtures::inference_head(phoebis_profile::fixtures::FixtureKind::OauthFirstParty),
    )
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
        stream: false,
        headers: vec![],
        body: bytes::Bytes::new(),
        body_capture_hours: 0,
        shape: RequestShape::default(),
        client: phoebis_bus::ClientFacts::default(),
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
