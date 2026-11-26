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
    AccountSelected, AttemptOutcome, AutoMode, Classifier, Event as BusEvent, Interruption, Kind,
    Origin, Outcome as BusOutcome, Phase, RequestComplete, RequestObserved, RequestShape,
    ResponseChunk, ResponseFacts, Usage, BODY_SHAPE_ERROR_CODE,
};
use phoebis_ledger::{
    apply_filter, breakdown, breakdown_paged, list_requests, member_spends, parked_count,
    parked_summary, read_rollups, run_retention, Assembler, AttemptRecord, AttentionGauges,
    BodyRecord, BudgetedMember, CacheMapWrite, DecisionRow, Direction, EventRecord, GroupBy,
    InputTransformation, LedgerConfig, LedgerDb, LedgerError, LiveKeys, LogKinds, ParkedSummary,
    PlanTables, PriceRevisions, Purged, Record, Replayed, RequestCursor, RequestFilter,
    RequestOrder, RequestRecord, RollupFilter, RollupSelect, Speed, WriterRead, BATCH_RETRY_DELAYS,
    BATCH_RETRY_HOLD, LEDGER_BEHIND_DEPTH,
};
use phoebis_profile::fixtures::BodyFixture;
use phoebis_store::{
    AccountKind, BudgetPeriod, EngineBatch, FactsKey, FactsKind, FactsRow, MemberWrite, StorageKey,
    Store,
};
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
    let desktop = h.store.api_key("desktop").unwrap().unwrap();
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
                ..desktop
            },
            desktop.updated_at,
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
            user_agent: phoebis_ledger::Patterns::new(vec![agent.into()]).unwrap(),
            ..Default::default()
        };
        h.db.read_blocking(|c| {
            phoebis_ledger::list_requests(
                c,
                &filter,
                &phoebis_ledger::LiveKeys::default(),
                RequestOrder::ReceivedAt,
                Direction::Desc,
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
/// stamped attribution line all reach the request row, and the log filters on each of them. The class is also
/// the hourly rollup's dimension, so Claude Code's requests and a caller the gate refused under `other` in the
/// same hour hold two rows.
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
        String::from_utf8(phoebis_profile::fixtures::body(BodyFixture::FirstParty).to_vec())
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
        } else if !req
            .headers_ref()
            .is_some_and(|h| h.contains_key("x-claude-code-request-class"))
        {
            req = req.header("x-claude-code-request-class", "main");
        }
        req.header("x-claude-code-session-id", session)
            .body(axum::body::Body::from(body.into_bytes()))
            .unwrap()
    };

    // The billing line stamped with a turn origin other than the fixture's own, the three hint headers set: every
    // fact reaches the row.
    let spliced = native.replacen("cc_turn_origin=sdk;", "cc_turn_origin=human;", 1);
    let spliced = String::from_utf8(phoebis_cch::sign(spliced.as_bytes()).unwrap()).unwrap();
    let mut req = build(spliced, "s1", None);
    assert_eq!(
        req.headers()
            .get_all("x-claude-code-request-class")
            .iter()
            .count(),
        1,
        "the first-party head carries the class header once"
    );
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
            client: phoebis_ledger::Patterns::new(vec![pattern.into()]).unwrap(),
            ..Default::default()
        };
        h.db.read_blocking(|c| {
            phoebis_ledger::list_requests(
                c,
                &filter,
                &phoebis_ledger::LiveKeys::default(),
                RequestOrder::ReceivedAt,
                Direction::Desc,
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
                RequestOrder::ReceivedAt,
                Direction::Desc,
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
                RequestOrder::ReceivedAt,
                Direction::Desc,
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

    // No hint header sent: the four hint members are absent, but the class still reads from the user agent and the
    // turn origin from the fixture's own line.
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
        (
            Some("claude-code/sdk-cli"),
            None,
            None,
            None,
            None,
            Some("sdk")
        ),
        "a caller that sends no hint carries the class and its line's turn origin alone"
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
    assert_eq!(resp3.status(), 403);
    let id3 = request_id(&resp3);
    let _ = resp3.into_body().collect().await.unwrap();
    assert_eq!(
        ledgered(&h, &id3).await.0.client.as_deref(),
        Some("other"),
        "a caller outside Claude Code is refused and its row carries the other label"
    );
    let rollups =
        h.db.read(|c| {
            phoebis_ledger::read_rollups(c, 0, i64::MAX, &phoebis_ledger::RollupSelect::default())
        })
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
    // A refusal before any account was asked files under the row's refusal map alone, beside no request.
    let refused: u64 = rollups
        .iter()
        .filter(|r| r.client == "other")
        .map(|r| r.refusals.get("client_not_allowed").copied().unwrap_or(0))
        .sum();
    assert_eq!(
        (by_client.get("other").copied(), refused),
        (Some(0), 1),
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
    assert_eq!(resp.status(), 403);
    let id = request_id(&resp);
    let _ = resp.into_body().collect().await.unwrap();
    assert_eq!(ledgered(&h, &id).await.0.user_agent, None);
    let matched = |agent: &str| {
        let filter = phoebis_ledger::RequestFilter {
            user_agent: phoebis_ledger::Patterns::new(vec![agent.into()]).unwrap(),
            ..Default::default()
        };
        h.db.read_blocking(|c| {
            phoebis_ledger::list_requests(
                c,
                &filter,
                &phoebis_ledger::LiveKeys::default(),
                RequestOrder::ReceivedAt,
                Direction::Desc,
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

/// The two upstream ids are what an operator holding a client transcript or an upstream support reply searches by.
#[tokio::test]
async fn text_matches_an_upstream_message_id_and_an_upstream_request_id() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![("request-id".into(), "req_up_7".into())],
            body: br#"{"id":"msg_01abc","type":"message","model":"claude-sonnet-4-5","stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":4}}"#
                .to_vec(),
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
    ledgered(&h, &id).await;
    let listed = |text: &str| {
        let filter = RequestFilter {
            text: Some(text.into()),
            ..Default::default()
        };
        h.db.read_blocking(|c| {
            list_requests(
                c,
                &filter,
                &LiveKeys::default(),
                RequestOrder::ReceivedAt,
                Direction::Desc,
                None,
                10,
            )
        })
        .unwrap()
        .0
        .iter()
        .map(|(row, _)| row.id.clone())
        .collect::<Vec<_>>()
    };
    assert_eq!(
        listed("msg_01abc"),
        [id.as_str()],
        "the served attempt's message id"
    );
    assert_eq!(
        listed("req_up_7"),
        [id.as_str()],
        "the attempt's upstream request id"
    );
    assert!(listed("msg_nope").is_empty(), "an id no attempt carries");
    up.shutdown().await;
}

#[test]
fn the_recent_gaps_are_the_newest_gap_events_at_or_after_the_cutoff() {
    let (db, _dir) = common::temp_ledger();
    for (at, kind) in [
        (100, "ledger_gap"),
        (200, "ledger_gap"),
        (300, "registry_changed"),
        (400, "ledger_gap"),
    ] {
        db.insert_event(&EventRecord {
            at,
            kind: kind.into(),
            ..Default::default()
        })
        .unwrap();
    }
    let since = db
        .read_blocking(|c| phoebis_ledger::recent_gaps(c, 200, 50))
        .unwrap();
    assert_eq!(
        since.iter().map(|g| g.at).collect::<Vec<_>>(),
        [400, 200],
        "newest first, the cutoff itself included, no other kind"
    );
    let newest = db
        .read_blocking(|c| phoebis_ledger::recent_gaps(c, 0, 1))
        .unwrap();
    assert_eq!(newest.iter().map(|g| g.at).collect::<Vec<_>>(), [400]);
}

/// The search matches what was typed character for character: `_`, `%` and `\` are characters, not wildcards or an
/// escape, so an id with an underscore never matches one with any other character in its place.
#[test]
fn the_search_matches_its_text_literally() {
    let (db, _dir) = common::temp_ledger();
    let plant = |id: &str, message_id: &str, error_code: &str| {
        let request = RequestRecord {
            id: id.into(),
            received_at: 1_000,
            model: "m".into(),
            canonical_model: "m".into(),
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(500),
            outcome: "failed".into(),
            attempts: 1,
            error_code: Some(error_code.into()),
            ..Default::default()
        };
        let attempt = AttemptRecord {
            request_id: id.into(),
            attempt: 1,
            account_id: "a".into(),
            account_kind: AccountKind::Oauth,
            started_at: 1_000,
            outcome: "failed".into(),
            upstream_message_id: Some(message_id.into()),
            ..Default::default()
        };
        db.insert_request(&request, &[attempt], None).unwrap();
    };
    plant("r1", "msg_01abc", "e1");
    plant("r2", "msgx01abc", "e2");
    plant("r3", "msg_03", "100%");
    plant("r4", "msg_04", "1000");
    plant("r5", "msg_05", "x\\_y");
    plant("r6", "msg_06", "x\\zy");
    let listed = |text: &str| {
        let filter = RequestFilter {
            text: Some(text.into()),
            ..Default::default()
        };
        let mut ids: Vec<String> = db
            .read_blocking(|c| {
                list_requests(
                    c,
                    &filter,
                    &LiveKeys::default(),
                    RequestOrder::ReceivedAt,
                    Direction::Desc,
                    None,
                    10,
                )
            })
            .unwrap()
            .0
            .iter()
            .map(|(row, _)| row.id.clone())
            .collect();
        ids.sort();
        ids
    };
    assert_eq!(listed("msg_01abc"), ["r1"], "`_` matches itself alone");
    assert_eq!(listed("100%"), ["r3"], "`%` matches itself alone");
    assert_eq!(
        listed("x\\_"),
        ["r5"],
        "a backslash is a character, not an escape"
    );
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
        h.db.read(|c| {
            phoebis_ledger::read_rollups(c, 0, i64::MAX, &phoebis_ledger::RollupSelect::default())
        })
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
    let mut req = req
        .header("x-claude-code-session-id", session)
        .body(axum::body::Body::from(native.into_bytes()))
        .unwrap();
    if !req.headers().contains_key("x-claude-code-request-class") {
        req.headers_mut().insert(
            "x-claude-code-request-class",
            axum::http::HeaderValue::from_static("main"),
        );
    }
    req
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
            BodyFixture::FirstParty,
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
        String::from_utf8(phoebis_profile::fixtures::body(BodyFixture::FirstParty).to_vec())
            .unwrap()
            .replacen(
                r#""model":"claude-sonnet-4-5""#,
                r#""model":"claude-sonnet-4-5","output_config":{"effort":"high"}"#,
                1,
            );
    // The fixture carries a first-party client's billing stamp over its own bytes; a hand-spliced body needs
    // its own stamp, or the relay's drift check refuses it as tampered.
    let spliced = String::from_utf8(phoebis_cch::sign(spliced.as_bytes()).unwrap()).unwrap();
    let req2 = request_with_native_body(spliced, KEY, "s2");
    assert_eq!(
        req2.headers()
            .get_all("x-claude-code-request-class")
            .iter()
            .count(),
        1,
        "the first-party head carries the class header once"
    );
    let resp2 = h.app.clone().oneshot(req2).await.unwrap();
    assert_eq!(resp2.status(), 200);
    let id2 = request_id(&resp2);
    let _ = resp2.into_body().collect().await.unwrap();
    let (row2, _) = ledgered(&h, &id2).await;
    assert_eq!(row2.effort.as_deref(), Some("high"));

    let rollups =
        h.db.read(|c| {
            phoebis_ledger::read_rollups(c, 0, i64::MAX, &phoebis_ledger::RollupSelect::default())
        })
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
        String::from_utf8(phoebis_profile::fixtures::body(BodyFixture::FirstParty).to_vec())
            .unwrap()
            .replacen("cc_version=2.1.310.077", "cc_version=2.1.310.abc", 1);
    let drifted = String::from_utf8(phoebis_cch::sign(native.as_bytes()).unwrap()).unwrap();
    let resp = h
        .app
        .clone()
        .oneshot(request_with_native_body(drifted, KEY, "s1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
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
    let (held_tx, held_rx) = tokio::sync::oneshot::channel::<()>();
    let (hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        db.write(|_| {
            held_tx.send(()).unwrap();
            hold_rx.recv().unwrap();
            Ok(())
        })
        .unwrap()
    });
    held_rx.await.unwrap();
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
        instance_id: None,
        model: "claude-sonnet-5".into(),
        canonical_model: "claude-sonnet-5".into(),
        wire_model: "claude-sonnet-5".into(),
        stream: false,
        headers: vec![],
        body: bytes::Bytes::new(),
        body_capture_hours: 0,
        shape: RequestShape::default(),
        static_hash: None,
        boundaries: Vec::new().into(),
        marked: Vec::new().into(),
        client: phoebis_bus::ClientFacts::default(),
        auto_mode: None,
        fork: phoebis_bus::Fork::default(),
        presented_retired: false,
        rotation_at: None,
        guardrail_id: None,
        guardrails: Vec::new(),
        received_at: 0,
        fallback_digest: None,
        cache_marker: false,
    })
}

fn complete_event(id: &str) -> BusEvent {
    BusEvent::RequestComplete(RequestComplete {
        request_id: id.into(),
        key_id: None,
        pool_id: None,
        origin: Origin::Client,
        kind: Kind::Inference,
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
        client_contract: None,
        client_version: None,
        response_bytes: None,
        response_chunks: None,
        held: None,
        diagnostics: None,
        thinking_diagnostics: None,
        message_id: None,
        skipped_events: 0,
        interruption: None,
        classifier: None,
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

/// The two events the relay publishes for a probe its admission refused: the observation of kind `probe`, and the
/// completion `complete` writes for a refusal before any attempt, under no account, with the outcome `outcome_of`
/// answers for a code at attempt 0.
fn refused_probe_events(id: &str, error_code: &str) -> [BusEvent; 2] {
    let BusEvent::RequestObserved(mut observed) = observed_event(id) else {
        unreachable!()
    };
    observed.origin = Origin::Keepalive;
    observed.kind = Kind::Probe;
    let BusEvent::RequestComplete(mut complete) = complete_event(id) else {
        unreachable!()
    };
    complete.origin = Origin::Keepalive;
    complete.kind = Kind::Probe;
    complete.outcome = BusOutcome::Rejected;
    complete.status = 503;
    complete.error_code = Some(error_code.into());
    [
        BusEvent::RequestObserved(observed),
        BusEvent::RequestComplete(complete),
    ]
}

/// The hour's probe row once `landed` reads true of it.
async fn probe_row(
    h: &Harness,
    landed: impl Fn(&phoebis_ledger::RollupRow) -> bool,
) -> phoebis_ledger::RollupRow {
    wait_for_async(Duration::from_secs(5), || async {
        h.db.read(|c| {
            phoebis_ledger::read_rollups(c, 0, i64::MAX, &phoebis_ledger::RollupSelect::default())
        })
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.kind == "probe" && landed(r))
    })
    .await
}

/// The router's own `503 ledger_behind` holds a probe while its ledger catches up, which the account never answered:
/// the row counts it under `probes_held` whatever outcome it carries, outside `requests`, `failed`, `rejected` and the
/// error codes, so the error rate counts what reached an account. A probe refused with another code is a rejection as
/// before.
#[tokio::test]
async fn a_probe_the_relay_refused_at_admission_folds_as_held_and_not_as_failed_or_rejected() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    for event in refused_probe_events("held", phoebis_relay::LEDGER_BEHIND) {
        h.bus.publish(event);
    }
    let row = probe_row(&h, |r| r.probes_held == 1).await;
    assert_eq!(
        (row.probes_held, row.requests, row.rejected, row.failed),
        (1, 0, 0, 0),
        "the held probe counts under its own column alone"
    );
    assert!(row.errors.is_empty(), "no error code: {:?}", row.errors);
    for event in refused_probe_events("refused", "auth_unknown") {
        h.bus.publish(event);
    }
    let row = probe_row(&h, |r| r.rejected == 1).await;
    assert_eq!(
        (row.probes_held, row.requests, row.rejected, row.failed),
        (1, 1, 1, 0),
        "a probe refused with another code folds into rejected"
    );
    assert_eq!(
        row.errors.iter().collect::<Vec<_>>(),
        [(&"auth_unknown".to_string(), &1)],
        "the rejection's code alone"
    );
    up.shutdown().await;
}

/// The digest a refused attempt issued lands on its attempt row and the digest a later request presented on that
/// request's row, each null on the other row.
#[tokio::test]
async fn a_refused_attempt_lands_its_issued_digest_and_a_presenting_request_its_fallback_digest() {
    const DIGEST: &str = "36df6527d0041aa2c3dea0f6e8490bbe05a17e2e41529298d4244f308118662a";
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    h.bus.publish(observed_event("refused"));
    h.bus.publish(selected_event("refused", 1, "a", 0));
    let BusEvent::RequestComplete(mut refusal) = complete_event("refused") else {
        unreachable!()
    };
    refusal.attempt = 1;
    refusal.account_id = Some("a".into());
    refusal.response = ResponseFacts {
        stop_reason: Some("refusal".into()),
        issued_digest: Some(DIGEST.into()),
        ..ResponseFacts::default()
    };
    h.bus.publish(BusEvent::RequestComplete(refusal));
    let BusEvent::RequestObserved(mut presenting) = observed_event("presenting") else {
        unreachable!()
    };
    presenting.fallback_digest = Some(DIGEST.into());
    h.bus.publish(BusEvent::RequestObserved(presenting));
    h.bus.publish(selected_event("presenting", 1, "a", 0));
    let BusEvent::RequestComplete(mut served) = complete_event("presenting") else {
        unreachable!()
    };
    served.attempt = 1;
    served.account_id = Some("a".into());
    h.bus.publish(BusEvent::RequestComplete(served));
    let (refused, refused_attempts) = ledgered(&h, "refused").await;
    let (presenting, presenting_attempts) = ledgered(&h, "presenting").await;
    assert_eq!(
        (
            refused_attempts[0].issued_digest.as_deref(),
            refused.fallback_digest.as_deref(),
            presenting.fallback_digest.as_deref(),
            presenting_attempts[0].issued_digest.as_deref(),
        ),
        (Some(DIGEST), None, Some(DIGEST), None)
    );
    up.shutdown().await;
}

/// The request list's reads of the serving attempt before the columns existed, one subquery per fact, run over a
/// seeded file as the oracle the backfilled columns are held against: the account, the speed under the requested
/// one's fallback, the cache-miss reason, the stop reason and the refusal category.
const PRE_COLUMN_SERVING_FACTS: &str = "SELECT id, \
    (SELECT a.account_id FROM attempts a WHERE a.request_id = requests.id ORDER BY a.attempt DESC LIMIT 1), \
    COALESCE((SELECT COALESCE(a.speed, CASE WHEN COALESCE(a.input_tokens, a.output_tokens, a.cache_read_tokens, \
    a.cache_write_5m_tokens, a.cache_write_1h_tokens) IS NOT NULL THEN 'standard' END) FROM attempts a \
    WHERE a.request_id = requests.id ORDER BY a.attempt DESC LIMIT 1), \
    CASE WHEN requests.requested_speed = 'fast' THEN 'fast' ELSE 'standard' END), \
    (SELECT a.cache_miss_reason FROM attempts a WHERE a.request_id = requests.id ORDER BY a.attempt DESC LIMIT 1), \
    (SELECT a.stop_reason FROM attempts a WHERE a.request_id = requests.id ORDER BY a.attempt DESC LIMIT 1), \
    (SELECT a.refusal_category FROM attempts a WHERE a.request_id = requests.id ORDER BY a.attempt DESC LIMIT 1) \
    FROM requests WHERE attempts > 0 ORDER BY id";

/// The serving facts of a request as `PRE_COLUMN_SERVING_FACTS` answers them: the id, then the five facts.
type FiveFacts = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// Rows written before the columns existed, their columns null, take the facts the request list derived per row from
/// their attempts: a failover's second account, a refusal's category, a cache-miss reason, the standard speed of an
/// answer that named none and the fast speed of a request that asked for it and was never answered. The other six
/// facts read the seeded attempt's own figures, and a request no attempt served keeps every fact null.
#[tokio::test]
async fn the_backfill_fills_old_rows_with_the_facts_the_list_derived_from_their_attempts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let db = Arc::new(LedgerDb::open(&path).unwrap());
    let seed = rusqlite::Connection::open(&path).unwrap();
    seed.execute_batch(&format!(
        "INSERT INTO requests (id, received_at, model, canonical_model, stream, kind, origin, outcome, attempts, \
         requested_speed, refused) VALUES \
         ('failover', {at}, 'claude-sonnet-5-5', 'claude-sonnet-5-5', 0, 'inference', 'client', 'succeeded', 2, NULL, 0), \
         ('missed', {at}, 'claude-sonnet-5-5', 'claude-sonnet-5-5', 0, 'inference', 'client', 'succeeded', 1, NULL, 0), \
         ('refusal', {at}, 'claude-sonnet-5-5', 'claude-sonnet-5-5', 0, 'inference', 'client', 'succeeded', 1, NULL, 0), \
         ('unanswered', {at}, 'claude-sonnet-5-5', 'claude-sonnet-5-5', 0, 'inference', 'client', 'failed', 1, 'fast', 0), \
         ('unattempted', {at}, 'claude-sonnet-5-5', 'claude-sonnet-5-5', 0, 'inference', 'client', 'rejected', 0, NULL, 1), \
         ('unspoken', {at}, 'claude-sonnet-5-5', 'claude-sonnet-5-5', 0, 'inference', 'client', 'succeeded', 1, NULL, 0); \
         INSERT INTO attempts (request_id, attempt, account_id, account_kind, started_at, upstream_status, outcome, \
         input_tokens, output_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens, speed, \
         cache_miss_reason, stop_reason, refusal_category) VALUES \
         ('failover', 1, 'a', 'oauth', {at}, 429, 'rate_limited', NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL), \
         ('failover', 2, 'b', 'oauth', {at}, 200, 'succeeded', 1000000, 200000, 10000000, 400000, 250000, 'standard', \
          NULL, 'end_turn', NULL), \
         ('missed', 1, 'a', 'oauth', {at}, 200, 'succeeded', 1000000, NULL, NULL, 400000, NULL, 'standard', \
          'tools_changed', 'end_turn', NULL), \
         ('refusal', 1, 'b', 'oauth', {at}, 200, 'succeeded', 2000000, 0, NULL, NULL, NULL, 'standard', NULL, \
          'refusal', 'cyber'), \
         ('unanswered', 1, 'a', 'oauth', {at}, NULL, 'transport_error', NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
          NULL, NULL), \
         ('unspoken', 1, 'a', 'oauth', {at}, 200, 'succeeded', 1000000, NULL, NULL, NULL, NULL, NULL, NULL, \
          'end_turn', NULL);",
        at = phoebis_ledger::test_tables::JANUARY_2026,
    ))
    .unwrap();
    let derived: Vec<FiveFacts> = seed
        .prepare(PRE_COLUMN_SERVING_FACTS)
        .unwrap()
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    phoebis_ledger::backfill_serving_facts(&db).unwrap();
    let mut filled: Vec<FiveFacts> = Vec::new();
    let mut figures = Vec::new();
    for (id, ..) in &derived {
        let (row, _) = db.request(id).await.unwrap().unwrap();
        let s = row.serving;
        figures.push((
            id.clone(),
            s.reached_upstream,
            s.outcome_class,
            s.input_tokens,
            s.output_tokens,
            s.cache_read_tokens,
            s.cache_write_tokens,
        ));
        filled.push((
            id.clone(),
            s.account_id,
            s.speed,
            s.cache_miss_reason,
            s.stop_reason,
            s.refusal_category,
        ));
    }
    assert_eq!(filled, derived, "the columns hold what the list derived");
    let succeeded = || Some("succeeded".to_string());
    assert_eq!(
        figures,
        [
            (
                "failover".to_string(),
                Some(true),
                succeeded(),
                Some(1_000_000),
                Some(200_000),
                Some(10_000_000),
                Some(650_000)
            ),
            (
                "missed".to_string(),
                Some(true),
                succeeded(),
                Some(1_000_000),
                None,
                None,
                Some(400_000)
            ),
            (
                "refusal".to_string(),
                Some(true),
                succeeded(),
                Some(2_000_000),
                Some(0),
                None,
                None
            ),
            (
                "unanswered".to_string(),
                Some(false),
                Some("transport_error".to_string()),
                None,
                None,
                None,
                None
            ),
            (
                "unspoken".to_string(),
                Some(true),
                succeeded(),
                Some(1_000_000),
                None,
                None,
                None
            ),
        ],
        "the serving attempt's own figures"
    );
    let (unattempted, _) = db.request("unattempted").await.unwrap().unwrap();
    assert_eq!(unattempted.serving, phoebis_ledger::ServingFacts::default());
}

/// A start fills one slice of the rows written before the columns, the newest first beside ten filled slices, so the
/// request list's account filter finds the most recent history first, and the purge task fills the rest while the
/// router runs. A refused request no attempt served stays null and out of the pending index, and a request whose
/// trail lost every attempt stays null in it, read and counted by neither start.
#[tokio::test]
async fn a_start_fills_one_slice_of_pending_rows_and_skips_refusals_and_lost_trails() {
    use phoebis_ledger::{backfill_serving_facts, Backfilled, BACKFILL_SLICE};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let db = Arc::new(LedgerDb::open(&path).unwrap());
    let seed = rusqlite::Connection::open(&path).unwrap();
    let filled = 10 * BACKFILL_SLICE;
    let pending = BACKFILL_SLICE + 1;
    seed.execute_batch(&format!(
        "WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i < {last_filled}) \
         INSERT INTO requests (id, received_at, model, canonical_model, stream, kind, origin, outcome, attempts, \
         serving_account_id, reached_upstream, outcome_class, speed) \
         SELECT 'filled-' || i, i, 'm', 'm', 0, 'inference', 'client', 'succeeded', 1, 'a', 1, 'succeeded', \
         'standard' FROM n; \
         WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i < {last_pending}) \
         INSERT INTO requests (id, received_at, model, canonical_model, stream, kind, origin, outcome, attempts) \
         SELECT 'pending-' || i, {filled} + 2 * i, 'm', 'm', 0, 'inference', 'client', 'succeeded', 1 FROM n; \
         WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i < 9) \
         INSERT INTO requests (id, received_at, model, canonical_model, stream, kind, origin, outcome, attempts, \
         refused) \
         SELECT 'refused-' || i, {filled} + 2 * i + 1, 'm', 'm', 0, 'inference', 'client', 'rejected', 0, 1 FROM n; \
         INSERT INTO attempts (request_id, attempt, account_id, account_kind, started_at, outcome) \
         SELECT id, 1, 'a', 'oauth', received_at, 'succeeded' FROM requests WHERE attempts = 1; \
         WITH RECURSIVE n(i) AS (SELECT 10 UNION ALL SELECT i + 1 FROM n WHERE i < 14) \
         INSERT INTO requests (id, received_at, model, canonical_model, stream, kind, origin, outcome, attempts) \
         SELECT 'lost-' || i, {filled} + 2 * i + 1, 'm', 'm', 0, 'inference', 'client', 'failed', 1 FROM n;",
        last_filled = filled - 1,
        last_pending = pending - 1,
    ))
    .unwrap();
    let first = backfill_serving_facts(&db).unwrap();
    let by_account_a = phoebis_ledger::RequestFilter {
        request: Some(format!("pending-{}", pending - 1)),
        accounts: vec!["a".into()],
        ..Default::default()
    };
    let newest_listed = db
        .read_blocking(|c| {
            phoebis_ledger::list_requests(
                c,
                &by_account_a,
                &LiveKeys::default(),
                RequestOrder::ReceivedAt,
                Direction::Desc,
                None,
                10,
            )
        })
        .unwrap()
        .0
        .len();
    let (oldest, _) = db.request("pending-0").await.unwrap().unwrap();
    assert_eq!(
        (newest_listed, oldest.serving.account_id),
        (1, None),
        "the first slice fills the newest pending row, which the account filter finds, and leaves the oldest"
    );
    let second = backfill_serving_facts(&db).unwrap();
    assert_eq!(
        (first, second),
        (
            Backfilled {
                written: BACKFILL_SLICE,
                left: 1
            },
            Backfilled {
                written: 1,
                left: 0
            }
        )
    );
    let (refused, _) = db.request("refused-0").await.unwrap().unwrap();
    let (lost, _) = db.request("lost-10").await.unwrap().unwrap();
    // An index b-tree keeps an entry in every cell, an interior page's included, so the cells count the entries.
    let indexed: i64 = seed
        .query_row(
            "SELECT COALESCE(SUM(ncell), 0) FROM dbstat WHERE name = 'requests_serving_pending'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        (refused.serving.account_id, lost.serving.account_id, indexed),
        (None, None, 5),
        "the refusal and the lost trail keep a null account, and the pending index holds the five lost trails alone"
    );
}

/// `count` request rows written before the serving columns, each with one attempt on account `a`, `received_at`
/// rising with the row's number, so `pending-0` is the oldest and the last row a slice reaches.
fn seed_pending(path: &std::path::Path, count: usize) {
    rusqlite::Connection::open(path)
        .unwrap()
        .execute_batch(&format!(
            "WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i < {last}) \
             INSERT INTO requests (id, received_at, model, canonical_model, stream, kind, origin, outcome, attempts) \
             SELECT 'pending-' || i, i, 'm', 'm', 0, 'inference', 'client', 'succeeded', 1 FROM n; \
             INSERT INTO attempts (request_id, attempt, account_id, account_kind, started_at, outcome) \
             SELECT id, 1, 'a', 'oauth', received_at, 'succeeded' FROM requests;",
            last = count - 1,
        ))
        .unwrap();
}

/// The ledger task over `db` with a purge pass every 20 ms.
async fn purging_often(db: Arc<LedgerDb>, up: &FakeUpstream) -> Harness {
    harness_on(
        fresh_store(),
        db,
        up,
        &[api_key("a", C, 0)],
        (0, None),
        LedgerConfig {
            purge_interval: Duration::from_millis(20),
            ..quick()
        },
        false,
    )
    .await
}

async fn wait_for_the_oldest_row_filled(h: &Harness) {
    wait_for_async(Duration::from_secs(5), || async {
        let (oldest, _) = h.db.request("pending-0").await.unwrap().unwrap();
        (oldest.serving.account_id.as_deref() == Some("a")).then_some(())
    })
    .await;
}

fn holding(lines: &std::sync::Mutex<Vec<String>>, text: &str) -> Vec<String> {
    lines
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.contains(text))
        .cloned()
        .collect()
}

/// The rows a start leaves past its one slice fill while the router runs: the purge task's first pass writes slice
/// after slice until none is left and names the rows it filled in one line, after which it asks no more.
#[tokio::test]
async fn the_purge_task_fills_the_rows_a_start_left_within_one_pass_and_says_once_how_many() {
    use phoebis_ledger::BACKFILL_SLICE;
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let db = Arc::new(LedgerDb::open(&path).unwrap());
    seed_pending(&path, BACKFILL_SLICE + 1);
    let (lines, _capture) = capture_tracing();
    let h = purging_often(db, &up).await;
    wait_for_the_oldest_row_filled(&h).await;
    let reported = wait_for(Duration::from_secs(5), || {
        lines
            .lock()
            .unwrap()
            .iter()
            .position(|line| line.contains("written="))
    })
    .await;
    let passes_ahead = lines.lock().unwrap()[..reported]
        .iter()
        .filter(|line| line.contains("purged="))
        .count();
    assert_eq!(passes_ahead, 1, "both slices ran in the first pass");
    let written = holding(&lines, "written=");
    assert_eq!(written.len(), 1, "one line names the rows filled");
    assert!(
        written[0].contains(&format!("written={} ", BACKFILL_SLICE + 1)),
        "the line counts both slices: {}",
        written[0]
    );
    stays_for(Duration::from_millis(200), || {
        holding(&lines, "written=").len() == 1
    })
    .await;
    up.shutdown().await;
}

/// A slice the ledger refuses ends its pass, and the next pass asks again, so the rows fill once the refusal lifts
/// and a pass logs one refusal at most.
#[tokio::test]
async fn a_slice_the_ledger_refuses_ends_the_pass_and_the_next_pass_goes_on() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let db = Arc::new(LedgerDb::open(&path).unwrap());
    seed_pending(&path, 3);
    db.write(|c| {
        c.execute_batch(
            "CREATE TRIGGER refuse_fill BEFORE UPDATE ON requests BEGIN SELECT RAISE(ABORT, 'fill refused by test'); END;",
        )?;
        Ok(())
    })
    .unwrap();
    let (lines, _capture) = capture_tracing();
    let h = purging_often(db, &up).await;
    wait_for(Duration::from_secs(5), || {
        (!holding(&lines, "fill refused by test").is_empty()).then_some(())
    })
    .await;
    drop_trigger(&h.db, "refuse_fill");
    wait_for_the_oldest_row_filled(&h).await;
    let (refusals, passes) = (
        holding(&lines, "fill refused by test").len(),
        holding(&lines, "purged=").len(),
    );
    assert!(
        refusals <= passes,
        "{refusals} refusals over {passes} passes: a refused slice is not tried again inside its pass"
    );
    up.shutdown().await;
}

/// A client request of agent `g` in session `session` under the key `desktop`, received at `at` and finished a second
/// later.
fn session_request(id: &str, session: &str, at: i64) -> RequestRecord {
    RequestRecord {
        id: id.into(),
        received_at: at,
        finished_at: Some(at + 1_000),
        key_id: Some("desktop".into()),
        session_id: Some(session.into()),
        agent_id: Some("g".into()),
        model: "claude-sonnet-5".into(),
        canonical_model: "claude-sonnet-5".into(),
        kind: "inference".into(),
        origin: "client".into(),
        outcome: "succeeded".into(),
        attempts: 1,
        ..Default::default()
    }
}

/// Attempt `n` of `id` on account `account`, started at `at` and answered with `input` input tokens.
fn answered(id: &str, n: u32, account: &str, at: i64, input: u64) -> AttemptRecord {
    AttemptRecord {
        request_id: id.into(),
        attempt: n,
        account_id: account.into(),
        account_kind: AccountKind::Oauth,
        started_at: at,
        finished_at: Some(at + 500),
        outcome: "succeeded".into(),
        input_tokens: Some(input),
        ..Default::default()
    }
}

/// Session `session`'s sums rows as (agent, account, band, requests, input tokens), read on a connection of its own.
fn session_sums(path: &std::path::Path, session: &str) -> Vec<(String, String, String, i64, i64)> {
    rusqlite::Connection::open(path)
        .unwrap()
        .prepare(
            "SELECT agent_id, account_id, band, requests, input_tokens FROM session_sums WHERE session_id = ?1 \
             ORDER BY agent_id, account_id, received_hour",
        )
        .unwrap()
        .query_map([session], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// The requests session `session`'s sums rows count.
fn session_requests(path: &std::path::Path, session: &str) -> i64 {
    session_sums(path, session).iter().map(|row| row.3).sum()
}

/// Whether session `session` holds its own facts row, the fill's mark.
fn session_marked(path: &std::path::Path, session: &str) -> bool {
    rusqlite::Connection::open(path)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM session_facts WHERE session_id = ?1 AND agent_id = ''",
            [session],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
        == 1
}

/// Agent `agent`'s facts in session `session`: its first request's receipt, its newest request's id and that
/// request's serving account.
fn agent_facts(
    path: &std::path::Path,
    session: &str,
    agent: &str,
) -> (Option<i64>, Option<String>, Option<String>) {
    rusqlite::Connection::open(path)
        .unwrap()
        .prepare(
            "SELECT first_request_at, newest_id, newest_account_id FROM session_facts \
             WHERE session_id = ?1 AND agent_id = ?2",
        )
        .unwrap()
        .query_map([session, agent], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .next()
        .transpose()
        .unwrap()
        .unwrap_or_default()
}

async fn wait_for_session_marked(path: &std::path::Path, session: &str) {
    wait_for(Duration::from_secs(5), || {
        session_marked(path, session).then_some(())
    })
    .await;
}

/// Writes rows into the file at `path` as a build before the session totals left them: no sums row and no mark.
fn write_raw(path: &std::path::Path, sql: &str) {
    rusqlite::Connection::open(path)
        .unwrap()
        .execute_batch(sql)
        .unwrap();
}

/// A request landed again leaves the sums row its first landing added and counts its serving attempt's tokens alone,
/// though the trail's earlier attempt on `a` answered with tokens too and the trail lists it last. The first landing
/// found the session holding no other request, so it marked it, and the agent's newest request names the replacement's
/// account.
#[test]
fn a_request_landed_again_counts_once_by_its_serving_attempt_in_the_session_its_first_landing_marked(
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let db = LedgerDb::open(&path).unwrap();
    let prices = PriceRevisions::shipped();
    let at = phoebis_ledger::test_tables::JANUARY_2026;
    let request = session_request("r1", "s", at);
    db.insert_request_at(&request, &[answered("r1", 1, "a", at, 100)], None, &prices)
        .unwrap();
    db.insert_request_at(
        &request,
        &[
            answered("r1", 2, "b", at, 300),
            answered("r1", 1, "a", at, 100),
        ],
        None,
        &prices,
    )
    .unwrap();
    assert_eq!(
        (
            session_sums(&path, "s"),
            session_marked(&path, "s"),
            agent_facts(&path, "s", "g").2
        ),
        (
            vec![("g".into(), "b".into(), "standard".into(), 1, 300)],
            true,
            Some("b".into())
        )
    );
}

/// The purge task's fill sums a session the ledger held before the step, at the task's start ahead of any purge pass:
/// two served requests of one agent at one instant, the greater id written first, and a refused request of the same
/// session. The sums count the two served requests, and the agent's newest request is the greater id.
#[tokio::test]
async fn the_start_fills_a_session_written_before_the_step_from_its_served_requests_by_receipt_and_id(
) {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    drop(LedgerDb::open(&path).unwrap());
    write_raw(
        &path,
        &format!(
            "INSERT INTO requests (id, received_at, model, canonical_model, stream, kind, origin, outcome, attempts, \
             key_id, session_id, agent_id, refused) VALUES \
             ('r-b', {at}, 'claude-sonnet-5', 'claude-sonnet-5', 0, 'inference', 'client', 'succeeded', 1, \
              'desktop', 's', 'g', 0), \
             ('r-a', {at}, 'claude-sonnet-5', 'claude-sonnet-5', 0, 'inference', 'client', 'succeeded', 1, \
              'desktop', 's', 'g', 0), \
             ('r-refused', {later}, 'claude-sonnet-5', 'claude-sonnet-5', 0, 'inference', 'client', 'rejected', 0, \
              'desktop', 's', 'g', 1); \
             INSERT INTO attempts (request_id, attempt, account_id, account_kind, started_at, outcome, input_tokens) \
             VALUES ('r-b', 1, 'a', 'oauth', {at}, 'succeeded', 100), \
             ('r-a', 1, 'a', 'oauth', {at}, 'succeeded', 100);",
            at = phoebis_ledger::test_tables::JANUARY_2026,
            later = phoebis_ledger::test_tables::JANUARY_2026 + 1_000,
        ),
    );
    let _h = on_disk(&up, fresh_store(), &path, quick()).await;
    wait_for_session_marked(&path, "s").await;
    assert_eq!(
        (session_requests(&path, "s"), agent_facts(&path, "s", "g").1),
        (2, Some("r-b".into()))
    );
    up.shutdown().await;
}

/// A request that lands on a session the fill has not reached adds nothing, and the start's fill then counts it once
/// beside the session's request from before the step.
#[tokio::test]
async fn a_landing_on_a_session_the_fill_has_not_reached_is_counted_once_by_the_fill() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let at = phoebis_ledger::test_tables::JANUARY_2026;
    let db = LedgerDb::open(&path).unwrap();
    write_raw(
        &path,
        &format!(
            "INSERT INTO requests (id, received_at, model, canonical_model, stream, kind, origin, outcome, attempts, \
             key_id, session_id, agent_id) VALUES \
             ('r1', {at}, 'claude-sonnet-5', 'claude-sonnet-5', 0, 'inference', 'client', 'succeeded', 1, \
              'desktop', 's', 'g'); \
             INSERT INTO attempts (request_id, attempt, account_id, account_kind, started_at, outcome, input_tokens) \
             VALUES ('r1', 1, 'a', 'oauth', {at}, 'succeeded', 100);"
        ),
    );
    db.insert_request_at(
        &session_request("r2", "s", at + 1_000),
        &[answered("r2", 1, "a", at + 1_000, 100)],
        None,
        &PriceRevisions::shipped(),
    )
    .unwrap();
    drop(db);
    let _h = on_disk(&up, fresh_store(), &path, quick()).await;
    wait_for_session_marked(&path, "s").await;
    assert_eq!(session_requests(&path, "s"), 2);
    up.shutdown().await;
}

/// The fill goes on past a walk whose rows hold no session without a mark: session `s`'s one request lies below 1,000
/// requests of no session received after it, and the start's fill marks it.
#[tokio::test]
async fn the_fill_goes_on_past_a_full_walk_that_met_no_session_without_a_mark() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    drop(LedgerDb::open(&path).unwrap());
    write_raw(
        &path,
        &format!(
            "INSERT INTO requests (id, received_at, model, canonical_model, stream, kind, origin, outcome, attempts, \
             key_id, session_id, agent_id) VALUES \
             ('r1', {at}, 'claude-sonnet-5', 'claude-sonnet-5', 0, 'inference', 'client', 'succeeded', 1, \
              'desktop', 's', 'g'); \
             INSERT INTO attempts (request_id, attempt, account_id, account_kind, started_at, outcome, input_tokens) \
             VALUES ('r1', 1, 'a', 'oauth', {at}, 'succeeded', 100); \
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1000) \
             INSERT INTO requests (id, received_at, model, canonical_model, stream, kind, origin, outcome, attempts) \
             SELECT 'no-session-' || i, {at} + i, 'claude-sonnet-5', 'claude-sonnet-5', 0, 'inference', 'client', \
             'succeeded', 0 FROM n;",
            at = phoebis_ledger::test_tables::JANUARY_2026,
        ),
    );
    let _h = on_disk(&up, fresh_store(), &path, quick()).await;
    wait_for_session_marked(&path, "s").await;
    up.shutdown().await;
}

/// A price list whose Haiku 5.5 row opens its long-context cells past `over` prompt tokens.
fn haiku_over(over: u64) -> PriceRevisions {
    PriceRevisions::from_yaml(
        &format!(
            "revisions:\n  - effective: 2026-01-01\n    models:\n      claude-haiku-5-5:\n        input: 1\n        \
             cache_write_5m: 1.25\n        cache_write_1h: 2\n        cache_read: 0.1\n        output: 5\n        \
             long_context: {{over: {over}, input: 5, cache_write_5m: 6.25, cache_write_1h: 10, cache_read: 0.5, \
             output: 25}}\n"
        ),
        "test",
    )
    .expect("the list parses")
}

/// A start's steps as the router runs them ahead of the ledger task, then the task over the file at `path` with
/// `prices` as its list.
async fn started_under(
    up: &FakeUpstream,
    path: &std::path::Path,
    prices: PriceRevisions,
    now: i64,
) -> Harness {
    let db = Arc::new(LedgerDb::open(path).unwrap());
    phoebis_ledger::refold_bands(&db, &prices, 0, 0, now).unwrap();
    phoebis_ledger::unmark_rebanded_sessions(&db, &prices).unwrap();
    harness_on_tables(
        fresh_store(),
        db,
        up,
        &[api_key("a", C, 0)],
        (0, None),
        quick(),
        false,
        prices,
        PlanTables::shipped(),
    )
    .await
}

/// A request written before the step, a day before the band re-fold's first mark, takes the band of the first start's
/// threshold of 1,000 once that start's fill sums it: its 1,500-token prompt is long. The next start loads a threshold
/// of 2,000, under which the prompt is standard: the re-fold's pass leaves the request out of its walk, and the
/// session's sums row takes the new band once the start's step cleared the session and the fill summed it again.
#[tokio::test]
async fn a_moved_threshold_bands_a_session_again_past_the_refolds_since_floor() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let at = phoebis_ledger::test_tables::JANUARY_2026;
    drop(LedgerDb::open(&path).unwrap());
    write_raw(
        &path,
        &format!(
            "INSERT INTO requests (id, received_at, model, canonical_model, stream, kind, origin, outcome, attempts, \
             key_id, session_id, agent_id) VALUES \
             ('r1', {at}, 'claude-haiku-5-5', 'claude-haiku-5-5', 0, 'inference', 'client', 'succeeded', 1, \
              'desktop', 's', 'g'); \
             INSERT INTO attempts (request_id, attempt, account_id, account_kind, started_at, outcome, input_tokens) \
             VALUES ('r1', 1, 'a', 'oauth', {at}, 'succeeded', 1500);"
        ),
    );
    let h = started_under(&up, &path, haiku_over(1_000), at + 86_400_000).await;
    wait_for_session_marked(&path, "s").await;
    assert_eq!(session_sums(&path, "s")[0].2, "long");
    h.ledger.clone().shutdown().await.unwrap();
    drop(h);
    let _h = started_under(&up, &path, haiku_over(2_000), at + 2 * 86_400_000).await;
    wait_for_session_marked(&path, "s").await;
    assert_eq!(
        session_sums(&path, "s"),
        [("g".into(), "a".into(), "standard".into(), 1, 1_500)]
    );
    up.shutdown().await;
}

/// The retention purge cuts a session's sums at the requests' cutoff: one request in an hour before the cutoff's and
/// two in the hour the cutoff splits, ten minutes either side of it. The pass leaves the request after the cutoff
/// counted alone, and the agent's first request reads its receipt. Session `t` holds a refused request in an hour
/// before the cutoff's, which adds no sums row, and a served one in the hour after the split one: the pass finds `t`
/// through its facts, keeps the served request's row and reads the agent's first request at its receipt.
#[test]
fn the_purge_keeps_a_sessions_totals_to_the_requests_after_the_cutoff_in_the_hour_it_splits() {
    const MINUTE: i64 = 60_000;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let db = LedgerDb::open(&path).unwrap();
    let prices = PriceRevisions::shipped();
    // Half past an hour, so the hour it falls in holds a request on each side of it.
    let cutoff = phoebis_ledger::test_tables::JANUARY_2026 + 30 * MINUTE;
    for (id, at) in [
        ("old", cutoff - 150 * MINUTE),
        ("cut", cutoff - 10 * MINUTE),
        ("kept", cutoff + 10 * MINUTE),
    ] {
        db.insert_request_at(
            &session_request(id, "s", at),
            &[answered(id, 1, "a", at, 100)],
            None,
            &prices,
        )
        .unwrap();
    }
    let refused_at = cutoff - 150 * MINUTE;
    db.insert_request_at(
        &RequestRecord {
            outcome: "rejected".into(),
            refused: true,
            attempts: 0,
            ..session_request("t-refused", "t", refused_at)
        },
        &[],
        None,
        &prices,
    )
    .unwrap();
    let served_at = cutoff + 70 * MINUTE;
    db.insert_request_at(
        &session_request("t-served", "t", served_at),
        &[answered("t-served", 1, "a", served_at, 100)],
        None,
        &prices,
    )
    .unwrap();
    phoebis_ledger::run_retention_blocking(
        &db,
        &LedgerConfig {
            retention_days: 1,
            ..LedgerConfig::default()
        },
        cutoff + 86_400_000,
        &[],
        &prices,
    )
    .unwrap();
    assert_eq!(
        (session_requests(&path, "s"), agent_facts(&path, "s", "g").0),
        (1, Some(cutoff + 10 * MINUTE))
    );
    assert_eq!(
        (session_requests(&path, "t"), agent_facts(&path, "t", "g").0),
        (1, Some(cutoff + 70 * MINUTE))
    );
}

/// The purge reads an agent's probe again where it lies before the cutoff: agent `g`'s one probe, two and a half hours
/// before the cutoff, leaves with the pass, and its client request ten minutes after the cutoff, which the router
/// refused, keeps the agent's row. With no served request left, the session's figures list no line for `g`, and agent
/// `h`, whose one probe lies ten minutes after the cutoff, keeps its line through that probe.
#[test]
fn the_purge_leaves_an_agent_no_probe_where_its_one_probe_lay_before_the_cutoff() {
    const MINUTE: i64 = 60_000;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let db = LedgerDb::open(&path).unwrap();
    let prices = PriceRevisions::shipped();
    let cutoff = phoebis_ledger::test_tables::JANUARY_2026 + 30 * MINUTE;
    let probe_at = cutoff - 150 * MINUTE;
    db.insert_request_at(
        &RequestRecord {
            kind: "probe".into(),
            origin: "keepalive".into(),
            ..session_request("probe", "s", probe_at)
        },
        &[answered("probe", 1, "a", probe_at, 100)],
        None,
        &prices,
    )
    .unwrap();
    let kept_at = cutoff + 10 * MINUTE;
    db.insert_request_at(
        &RequestRecord {
            outcome: "rejected".into(),
            refused: true,
            attempts: 0,
            ..session_request("kept", "s", kept_at)
        },
        &[],
        None,
        &prices,
    )
    .unwrap();
    db.insert_request_at(
        &RequestRecord {
            kind: "probe".into(),
            origin: "keepalive".into(),
            agent_id: Some("h".into()),
            ..session_request("h-probe", "s", kept_at)
        },
        &[answered("h-probe", 1, "a", kept_at, 100)],
        None,
        &prices,
    )
    .unwrap();
    let now = cutoff + 86_400_000;
    phoebis_ledger::run_retention_blocking(
        &db,
        &LedgerConfig {
            retention_days: 1,
            ..LedgerConfig::default()
        },
        now,
        &[],
        &prices,
    )
    .unwrap();
    let figures = figures_of(&db, &prices, &PlanTables::shipped(), now);
    let agents: Vec<&str> = figures
        .agents
        .iter()
        .map(|agent| agent.agent_id.as_str())
        .collect();
    assert_eq!(agents, ["h"]);
}

/// Session `s` under the key `desktop`, as the sessions route names it.
fn desktop_session() -> phoebis_ledger::SessionSpendKey {
    phoebis_ledger::SessionSpendKey {
        key_id: "desktop".into(),
        key_generation: 0,
        session_id: "s".into(),
    }
}

/// Session `s`'s figures read on `db` at `now`, priced through `prices` and `costs`.
fn figures_of(
    db: &LedgerDb,
    prices: &PriceRevisions,
    costs: &PlanTables,
    now: i64,
) -> phoebis_ledger::SessionFigures {
    let key = desktop_session();
    db.read_blocking(|c| {
        phoebis_ledger::session_figures(c, std::slice::from_ref(&key), prices, costs, now)
    })
    .unwrap()
    .remove(&key)
    .expect("the read answers every session it names")
}

/// A cost model of one test tier sized 1,000 on the 5-hour window, whose band halves that window's capacity between
/// `from_hour` and `to_hour` in `zone` on every day, and a second revision from `second` on, where one is given, that
/// sizes the window 500. One Sonnet 5.5 input token weighs one unit.
fn session_costs(zone: &str, from_hour: u8, to_hour: u8, second: Option<&str>) -> PlanTables {
    let revision = |effective: &str, five_hour: u32| {
        format!(
            "  - effective: {effective}\n    capacity:\n      test_tier: {{five_hour: {five_hour}, seven_day: 10000}}\n    \
             peak:\n      zone: {zone}\n      weekdays: false\n      from_hour: {from_hour}\n      to_hour: {to_hour}\n      \
             capacity_factor: {{five_hour: 0.5, seven_day: 1}}\n    \
             type_multiplier: {{cache_read: 0.1, cache_write_1h: 2, cache_write_5m: 2, input: 1, output: 10}}\n    \
             model_weight:\n      claude-sonnet-5-5: 1\n    model_windows: {{}}\n    overrides: {{}}\n    \
             tier_order: [fable, opus, sonnet, haiku]\n    model_tier:\n      claude-sonnet-5-5: sonnet\n"
        )
    };
    let mut text = format!(
        "unit: {{tier: test_tier, window: seven_day}}\nrevisions:\n{}",
        revision("2026-01-01", 1_000)
    );
    if let Some(second) = second {
        text.push_str(&revision(second, 500));
    }
    phoebis_ledger::test_tables::tables(
        phoebis_ledger::CostRevisions::from_yaml(&text).expect("the cost model parses"),
    )
}

/// Two plan-funded Sonnet 5.5 requests of 100 input tokens on account `a` under the test tier, received at
/// `received.0` and `received.1` and each finished five minutes later, landed through `insert_request_at`.
fn land_two_plan_requests(db: &LedgerDb, received: (i64, i64)) {
    for (id, at) in [("r1", received.0), ("r2", received.1)] {
        let request = RequestRecord {
            model: "claude-sonnet-5-5".into(),
            canonical_model: "claude-sonnet-5-5".into(),
            ..session_request(id, "s", at)
        };
        let attempt = AttemptRecord {
            finished_at: Some(at + 300_000),
            tier: Some("test_tier".into()),
            ..answered(id, 1, "a", at, 100)
        };
        db.insert_request_at(
            &request,
            &[attempt],
            None,
            &phoebis_ledger::test_tables::api_prices(),
        )
        .unwrap();
    }
}

/// Account `a`'s share of its 5-hour window in the session's spend.
fn five_hour_share(figures: &phoebis_ledger::SessionFigures) -> f64 {
    figures.spend.shares["a"]["five_hour"]
}

/// A session the fill has not reached, its one request written before the step, answers the spend of that request
/// row at the read: a million input tokens of Sonnet 5.5 at the test list's $3 a million.
#[test]
fn a_session_the_fill_has_not_reached_reads_the_spend_of_its_request_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let db = LedgerDb::open(&path).unwrap();
    write_raw(
        &path,
        &format!(
            "INSERT INTO requests (id, received_at, model, canonical_model, stream, kind, origin, outcome, attempts, \
             key_id, session_id, agent_id) VALUES \
             ('r1', {at}, 'claude-sonnet-5-5', 'claude-sonnet-5-5', 0, 'inference', 'client', 'succeeded', 1, \
              'desktop', 's', 'g'); \
             INSERT INTO attempts (request_id, attempt, account_id, account_kind, started_at, outcome, input_tokens) \
             VALUES ('r1', 1, 'a', 'oauth', {at}, 'succeeded', 1000000);",
            at = phoebis_ledger::test_tables::JANUARY_2026,
        ),
    );
    let figures = figures_of(
        &db,
        &phoebis_ledger::test_tables::api_prices(),
        &phoebis_ledger::test_tables::subscription_costs(),
        phoebis_ledger::test_tables::JANUARY_2026,
    );
    assert!(!session_marked(&path, "s"), "the read writes no mark");
    assert_eq!((figures.spend.requests, figures.spend.money.usd), (1, 3.0));
}

/// Two requests of one sums row on the two sides of a cost entry that takes force at 12:30: the one received at 12:10
/// spends 100 / 1,000 = 0.1 of the 5-hour window under the first entry, the one received at 12:40 100 / 500 = 0.2 under
/// the second.
#[test]
fn a_cost_entry_inside_a_receipt_hour_prices_each_request_at_the_entry_in_force_at_its_receipt() {
    const MINUTE: i64 = 60_000;
    let noon = phoebis_ledger::test_tables::JANUARY_2026;
    let (db, _dir) = common::temp_ledger();
    land_two_plan_requests(&db, (noon + 10 * MINUTE, noon + 40 * MINUTE));
    let figures = figures_of(
        &db,
        &phoebis_ledger::test_tables::api_prices(),
        &session_costs("UTC", 20, 21, Some("2026-01-15T12:30:00Z")),
        noon + 60 * MINUTE,
    );
    assert!(
        (five_hour_share(&figures) - 0.3).abs() < 1e-12,
        "{figures:?}"
    );
}

/// Two requests of one sums row complete on the two sides of the band's opening at 18:00 in Kolkata, 12:30 UTC: the
/// one finished at 12:10 spends 100 / 1,000 = 0.1 of the 5-hour window, the one finished at 12:40 inside the band
/// 100 / 1,000 / 0.5 = 0.2.
#[test]
fn a_peak_band_that_opens_inside_a_completion_hour_prices_each_request_at_its_own_completion() {
    const MINUTE: i64 = 60_000;
    let noon = phoebis_ledger::test_tables::JANUARY_2026;
    let (db, _dir) = common::temp_ledger();
    land_two_plan_requests(&db, (noon + 5 * MINUTE, noon + 35 * MINUTE));
    let figures = figures_of(
        &db,
        &phoebis_ledger::test_tables::api_prices(),
        &session_costs("Asia/Kolkata", 18, 23, None),
        noon + 60 * MINUTE,
    );
    assert!(
        (five_hour_share(&figures) - 0.3).abs() < 1e-12,
        "{figures:?}"
    );
}

/// Two requests received in the hour the band opens in, at 12:30 UTC: the one finished at 12:10 makes its row a
/// boundary row and spends 100 / 1,000 = 0.1 outside the band, and the one finished at 13:02 lies in a row whose
/// completion hour the band covers whole, which prices from its sums at 100 / 1,000 / 0.5 = 0.2. The read of the
/// boundary hour passes the second request over, so the session counts it once.
#[test]
fn a_request_of_a_boundary_hour_whose_own_row_is_no_boundary_row_counts_once_from_its_sums() {
    const MINUTE: i64 = 60_000;
    let noon = phoebis_ledger::test_tables::JANUARY_2026;
    let (db, _dir) = common::temp_ledger();
    land_two_plan_requests(&db, (noon + 5 * MINUTE, noon + 57 * MINUTE));
    let figures = figures_of(
        &db,
        &phoebis_ledger::test_tables::api_prices(),
        &session_costs("Asia/Kolkata", 18, 23, None),
        noon + 120 * MINUTE,
    );
    let (requests, share) = (figures.spend.requests, five_hour_share(&figures));
    assert!(
        requests == 2 && (share - 0.3).abs() < 1e-12,
        "{requests} requests and a 5-hour share of {share}"
    );
}

/// A request's `held` names each of its waits with the reason: the hourly row sums the `slow_down` waits under
/// `slow_down_held_ms` and counts the request once under `slow_down_held`, a request that waited for a slot alone
/// counts under neither, and a replaced request takes its waits back out of its row.
#[test]
fn a_requests_slow_down_waits_count_into_its_hourly_row() {
    let (db, _dir) = common::temp_ledger();
    let request = |id: &str, key_id: &str, held: &str| RequestRecord {
        id: id.into(),
        received_at: 3_600_000,
        key_id: Some(key_id.into()),
        pool_id: Some("main".into()),
        canonical_model: "claude-sonnet-5".into(),
        model: "claude-sonnet-5".into(),
        kind: "inference".into(),
        origin: "client".into(),
        status: Some(200),
        outcome: "succeeded".into(),
        attempts: 1,
        held: Some(held.into()),
        ..Default::default()
    };
    let attempt = |id: &str| AttemptRecord {
        request_id: id.into(),
        attempt: 1,
        account_id: "a".into(),
        account_kind: AccountKind::Oauth,
        outcome: "succeeded".into(),
        output_tokens: Some(5),
        ..Default::default()
    };
    let slowed = r#"{"waited_on":["a","a"],"cohort":null,"held_ms":2500,"waits":[{"reason":"cap","held_ms":500},{"reason":"slow_down","held_ms":2000}]}"#;
    let at_cap = r#"{"waited_on":["a"],"cohort":null,"held_ms":500,"waits":[{"reason":"cap","held_ms":500}]}"#;
    db.insert_request(
        &request("slowed", "desktop", slowed),
        &[attempt("slowed")],
        None,
    )
    .unwrap();
    db.insert_request(
        &request("at-cap", "laptop", at_cap),
        &[attempt("at-cap")],
        None,
    )
    .unwrap();
    let row = |key_id: &str| {
        db.read_blocking(|c| {
            phoebis_ledger::read_rollups(c, 0, i64::MAX, &phoebis_ledger::RollupSelect::default())
        })
        .unwrap()
        .into_iter()
        .find(|r| r.key_id == key_id)
        .map(|r| (r.requests, r.slow_down_held_ms, r.slow_down_held))
    };
    assert_eq!(
        row("desktop"),
        Some((1, 2_000, 1)),
        "the slow-down's wait counts, the slot's beside it does not"
    );
    assert_eq!(
        row("laptop"),
        Some((1, 0, 0)),
        "a wait for a slot alone is no slow-down"
    );
    db.insert_request(
        &request("slowed", "desktop", at_cap),
        &[attempt("slowed")],
        None,
    )
    .unwrap();
    assert_eq!(
        row("desktop"),
        Some((1, 0, 0)),
        "the replaced request's slow-down wait left its row"
    );
}

/// A request refused after it waited through a slow-down hold (its key revoked during the hold) counts under the
/// refusals map alone, so the hour's held count and held time are the served request's.
#[test]
fn a_refusal_after_a_slow_down_hold_counts_under_no_held_figure() {
    let (db, _dir) = common::temp_ledger();
    let slowed = |held_ms: u64| {
        format!(
            r#"{{"waited_on":["a"],"cohort":null,"held_ms":{held_ms},"waits":[{{"reason":"slow_down","held_ms":{held_ms}}}]}}"#
        )
    };
    let request = |id: &str, key_id: &str, held_ms: u64| RequestRecord {
        id: id.into(),
        received_at: 3_600_000,
        key_id: Some(key_id.into()),
        pool_id: Some("main".into()),
        canonical_model: "claude-sonnet-5".into(),
        model: "claude-sonnet-5".into(),
        kind: "inference".into(),
        origin: "client".into(),
        held: Some(slowed(held_ms)),
        ..Default::default()
    };
    let served = RequestRecord {
        status: Some(200),
        outcome: "succeeded".into(),
        attempts: 1,
        ..request("served", "desktop", 2_000)
    };
    let attempt = AttemptRecord {
        request_id: "served".into(),
        attempt: 1,
        account_id: "a".into(),
        account_kind: AccountKind::Oauth,
        outcome: "succeeded".into(),
        output_tokens: Some(5),
        ..Default::default()
    };
    let refused = RequestRecord {
        status: Some(401),
        outcome: "rejected".into(),
        error_code: Some("auth_unknown".into()),
        refused: true,
        ..request("refused", "laptop", 3_000)
    };
    db.insert_request(&served, &[attempt], None).unwrap();
    db.insert_request(&refused, &[], None).unwrap();
    let rows = db
        .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
        .unwrap();
    assert_eq!(
        (
            rows.iter().map(|r| r.slow_down_held).sum::<u64>(),
            rows.iter().map(|r| r.slow_down_held_ms).sum::<u64>(),
        ),
        (1, 2_000),
        "the served request's hold alone: {rows:?}"
    );
    assert_eq!(
        rows.iter()
            .flat_map(|r| r.refusals.clone())
            .collect::<Vec<_>>(),
        [("auth_unknown".to_string(), 1)],
        "the refusal sits in the map"
    );
}

/// The row `list_requests` hands back, the same query `/admin/requests` runs.
async fn listed(h: &Harness, id: &str) -> RequestRecord {
    wait_for_async(Duration::from_secs(5), || async {
        let id = id.to_string();
        h.db.read(move |c| {
            let (rows, _) = list_requests(
                c,
                &RequestFilter::default(),
                &LiveKeys::default(),
                RequestOrder::ReceivedAt,
                Direction::Desc,
                None,
                50,
            )?;
            Ok(rows.into_iter().find(|(r, _)| r.id == id).map(|(r, _)| r))
        })
        .await
        .unwrap()
    })
    .await
}

/// The same row under a filter that keeps the probe rows the default listing hides.
async fn listed_with_probes(h: &Harness, id: &str) -> RequestRecord {
    wait_for_async(Duration::from_secs(5), || async {
        let id = id.to_string();
        h.db.read(move |c| {
            let filter = RequestFilter {
                include_probes: true,
                ..Default::default()
            };
            let (rows, _) = list_requests(
                c,
                &filter,
                &LiveKeys::default(),
                RequestOrder::ReceivedAt,
                Direction::Desc,
                None,
                50,
            )?;
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

#[tokio::test]
async fn a_request_whose_completion_never_arrives_lands_an_expired_gap_naming_it_and_counts_a_drop()
{
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(
        &up,
        &[oauth("a", A, 0)],
        (0, None),
        LedgerConfig {
            expire_interval: Duration::from_millis(5),
            ..LedgerConfig::default()
        },
    )
    .await;
    h.bus.publish(observed_event("silent"));
    // The assembler takes events in publish order, so once this later request has landed the silent one was
    // observed under the clock as it stands, and the step below ages it rather than stamping its arrival.
    h.bus.publish(observed_event("landed"));
    h.bus.publish(complete_event("landed"));
    ledgered(&h, "landed").await;
    h.clock
        .fetch_add(phoebis_ledger::IN_FLIGHT_TTL_MS, Ordering::SeqCst);
    let gap = wait_for_async(Duration::from_secs(5), || async {
        h.db.events(0, i64::MAX, 100)
            .await
            .unwrap()
            .into_iter()
            .find(|e| {
                e.kind == "ledger_gap" && e.detail.as_deref().is_some_and(|d| d.contains("silent"))
            })
    })
    .await;
    let detail: serde_json::Value = serde_json::from_str(gap.detail.as_deref().unwrap()).unwrap();
    assert_eq!(
        detail,
        serde_json::json!({ "request_id": "silent", "reason": "expired" })
    );
    assert_eq!(
        h.ledger.dropped(),
        1,
        "the expired request counts as a drop"
    );
    assert!(
        h.metrics
            .render(&phoebis_ledger::RoutingGauges::default(), &[], 0)
            .contains(r#"phoebis_ledger_dropped_total{table="requests"} 1"#),
        "charged to the requests table"
    );
    up.shutdown().await;
}

/// A request whose client left before the head completes as a cancel with the status 0 the relay answered nothing
/// with: the row lands at once with the attempts it reached and a null status, an attempt that never had a head reads
/// canceled since no transport failed it, and the expiry sweep that ages a silent sibling out names neither request.
#[tokio::test]
async fn a_client_gone_completion_lands_the_attempts_it_reached_and_no_gap() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(
        &up,
        &[oauth("a", A, 0)],
        (0, None),
        LedgerConfig {
            expire_interval: Duration::from_millis(5),
            ..LedgerConfig::default()
        },
    )
    .await;
    let client_gone = |id: &str| {
        let BusEvent::RequestComplete(mut complete) = complete_event(id) else {
            unreachable!()
        };
        complete.attempt = 1;
        complete.account_id = Some("a".into());
        complete.outcome = BusOutcome::Canceled;
        complete.status = 0;
        complete.error_code = Some("client_gone".into());
        BusEvent::RequestComplete(complete)
    };
    h.bus.publish(observed_event("silent"));
    for id in ["headed", "headless"] {
        h.bus.publish(observed_event(id));
        h.bus.publish(selected_event(id, 1, "a", NOW));
    }
    h.bus.publish(head_event("headed", 200, NOW));
    for id in ["headed", "headless"] {
        h.bus.publish(client_gone(id));
    }
    let (row, attempts) = ledgered(&h, "headed").await;
    assert_eq!(
        (
            row.status,
            row.outcome.as_str(),
            row.error_code.as_deref(),
            row.attempts
        ),
        (None, "canceled", Some("client_gone"), 1),
        "the client was answered nothing, so the row's status is null"
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
        [(1, "a", Some(200), "canceled")],
        "the attempt the head reached keeps its head"
    );
    let (row, attempts) = ledgered(&h, "headless").await;
    assert_eq!(
        (row.status, row.error_code.as_deref()),
        (None, Some("client_gone"))
    );
    assert_eq!(
        attempts
            .iter()
            .map(|a| (a.attempt, a.upstream_status, a.outcome.as_str()))
            .collect::<Vec<_>>(),
        [(1, None, "canceled")],
        "the client cut the attempt before its head, and no transport failed it"
    );
    h.clock
        .fetch_add(phoebis_ledger::IN_FLIGHT_TTL_MS, Ordering::SeqCst);
    let gap_for = |event: &EventRecord| -> Option<String> {
        if event.kind != "ledger_gap" {
            return None;
        }
        let detail: serde_json::Value = serde_json::from_str(event.detail.as_deref()?).ok()?;
        detail["request_id"].as_str().map(str::to_string)
    };
    wait_for_async(Duration::from_secs(5), || async {
        h.db.events(0, i64::MAX, 100)
            .await
            .unwrap()
            .iter()
            .any(|e| gap_for(e).as_deref() == Some("silent"))
            .then_some(())
    })
    .await;
    let gaps: Vec<String> =
        h.db.events(0, i64::MAX, 100)
            .await
            .unwrap()
            .iter()
            .filter_map(gap_for)
            .collect();
    assert_eq!(
        gaps,
        ["silent"],
        "the sweep aged the silent request out and found nothing of the two that completed"
    );
    up.shutdown().await;
}

/// A request the router's own shutdown ended before its head files as a client's leaving does: canceled with the
/// attempts it reached, the attempt that never had a head canceled too since no transport failed it.
#[tokio::test]
async fn a_router_shutdown_completion_files_its_headless_attempt_as_canceled() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    h.bus.publish(observed_event("headless"));
    h.bus.publish(selected_event("headless", 1, "a", NOW));
    let BusEvent::RequestComplete(mut complete) = complete_event("headless") else {
        unreachable!()
    };
    complete.attempt = 1;
    complete.account_id = Some("a".into());
    complete.outcome = BusOutcome::Canceled;
    complete.status = 0;
    complete.error_code = Some("router_shutdown".into());
    h.bus.publish(BusEvent::RequestComplete(complete));
    let (row, attempts) = ledgered(&h, "headless").await;
    assert_eq!(
        (
            row.status,
            row.outcome.as_str(),
            row.error_code.as_deref(),
            row.attempts
        ),
        (None, "canceled", Some("router_shutdown"), 1),
        "the router answered nothing, so the row's status is null"
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
        [(1, "a", None, "canceled")],
        "the router cut the attempt before its head, and no transport failed it"
    );
    up.shutdown().await;
}

/// A completion whose body ended short after the head writes one `stream_interrupted` row at its finish: the serving
/// attempt's account and upstream request id, the request's key, session and resolved model, and the cut's code,
/// words, bytes and attempt. A clean completion writes none, and a request with no session finds its own row among
/// the events its detail reads.
#[tokio::test]
async fn a_completion_cut_after_its_head_writes_one_stream_interrupted_row_its_request_reads() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let cut = |code: &str, text: &str| Interruption {
        code: code.into(),
        text: text.into(),
        bytes_relayed: 42,
    };
    let requests = [
        (
            "cut-in-session",
            Some("s1"),
            Some(cut("stream_stalled", "no chunk for 20 s")),
        ),
        (
            "cut-alone",
            None,
            Some(cut(
                "stream_cut",
                "malformed response: connection closed inside chunked body",
            )),
        ),
        ("clean", Some("s1"), None),
    ];
    for (id, session, interruption) in requests.clone() {
        let BusEvent::RequestObserved(mut observed) = observed_event(id) else {
            unreachable!()
        };
        observed.key_id = Some("k1".into());
        observed.session_id = session.map(str::to_string);
        h.bus.publish(BusEvent::RequestObserved(observed));
        h.bus.publish(selected_event(id, 1, "a", 10));
        let BusEvent::ResponseHead(mut head) = head_event(id, 200, 20) else {
            unreachable!()
        };
        head.headers = vec![("request-id".into(), format!("req_{id}"))];
        h.bus.publish(BusEvent::ResponseHead(head));
        let BusEvent::RequestComplete(mut complete) =
            answered_event(id, 200, ResponseFacts::default())
        else {
            unreachable!()
        };
        complete.key_id = Some("k1".into());
        if let Some(interruption) = &interruption {
            complete.outcome = BusOutcome::Incomplete;
            complete.error_code = Some(interruption.code.clone());
            complete.response_bytes = Some(interruption.bytes_relayed);
        }
        complete.interruption = interruption;
        complete.finished_at = 30;
        h.bus.publish(BusEvent::RequestComplete(complete));
    }
    for (id, _, _) in requests {
        ledgered(&h, id).await;
    }
    // The event goes to the writer ahead of its request row, so a landed request has its row written.
    let mut rows =
        h.db.read(|c| phoebis_ledger::recent_events_of_kind(c, "stream_interrupted", 5))
            .await
            .unwrap();
    rows.sort_by(|a, b| a.request_id.cmp(&b.request_id));
    assert_eq!(
        rows.iter()
            .map(|row| row.request_id.as_deref())
            .collect::<Vec<_>>(),
        [Some("cut-alone"), Some("cut-in-session")],
        "one row per cut completion and none for the clean one"
    );
    let row = &rows[1];
    assert_eq!(
        (
            row.at,
            row.account_id.as_deref(),
            row.key_id.as_deref(),
            row.session_id.as_deref(),
            row.model.as_deref(),
        ),
        (
            30,
            Some("a"),
            Some("k1"),
            Some("s1"),
            Some("claude-sonnet-5")
        )
    );
    let detail: serde_json::Value = serde_json::from_str(row.detail.as_deref().unwrap()).unwrap();
    assert_eq!(
        detail,
        serde_json::json!({
            "code": "stream_stalled",
            "text": "no chunk for 20 s",
            "bytes_relayed": 42,
            "upstream_request_id": "req_cut-in-session",
            "attempt": 1,
        })
    );
    let now = h.clock.load(Ordering::SeqCst);
    let own =
        h.db.read(move |c| {
            let (request, _) =
                phoebis_ledger::request_by_id(c, "cut-alone")?.expect("the request landed");
            phoebis_ledger::request_events(c, &request, now)
        })
        .await
        .unwrap();
    assert_eq!(
        own.iter()
            .filter(|e| e.kind == "stream_interrupted")
            .map(|e| e.request_id.as_deref())
            .collect::<Vec<_>>(),
        [Some("cut-alone")],
        "a request with no session reads its own row by its id"
    );
    up.shutdown().await;
}

#[test]
fn a_live_requests_events_are_read_up_to_the_clock_and_a_finished_ones_up_to_its_end() {
    let (db, _dir) = common::temp_ledger();
    let live = RequestRecord {
        finished_at: None,
        received_at: 1_000,
        session_id: Some("s".into()),
        ..finished_row("live")
    };
    for at in [1_500, 2_500] {
        let event = EventRecord {
            at,
            kind: "pin_migrated".into(),
            session_id: Some("s".into()),
            ..Default::default()
        };
        db.insert_event(&event).unwrap();
    }
    let at = |events: Vec<EventRecord>| events.iter().map(|e| e.at).collect::<Vec<_>>();
    assert_eq!(
        at(db
            .read_blocking(|c| phoebis_ledger::request_events(c, &live, 2_000))
            .unwrap()),
        [1_500],
        "a live request reads to the clock"
    );
    let finished = RequestRecord {
        finished_at: Some(1_800),
        ..live.clone()
    };
    let ended = at(db
        .read_blocking(|c| phoebis_ledger::request_events(c, &finished, 3_000))
        .unwrap());
    assert_eq!(ended, [1_500], "a finished request reads to its own end");
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
            let (rows, _) = list_requests(
                c,
                &filter,
                &LiveKeys::default(),
                RequestOrder::ReceivedAt,
                Direction::Desc,
                None,
                50,
            )?;
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

/// A ledger holding `matching` rows under one model, a millisecond apart, and one newer row under another model,
/// so a page reads the model filter rather than the whole table, with the directory of its file.
fn paging_ledger(matching: usize) -> (phoebis_ledger::LedgerDb, tempfile::TempDir) {
    let (db, dir) = common::temp_ledger();
    let row = |id: String, received_at: i64, model: &str| RequestRecord {
        id,
        received_at,
        canonical_model: model.into(),
        model: model.into(),
        kind: "inference".into(),
        origin: "client".into(),
        status: Some(200),
        outcome: "succeeded".into(),
        ..Default::default()
    };
    for n in 0..matching {
        db.insert_request(
            &row(format!("r{n:02}"), n as i64, "claude-sonnet-5"),
            &[],
            None,
        )
        .unwrap();
    }
    db.insert_request(
        &row("other".into(), matching as i64, "claude-opus-5"),
        &[],
        None,
    )
    .unwrap();
    (db, dir)
}

fn sonnet_page(
    db: &phoebis_ledger::LedgerDb,
    dir: Direction,
    cursor: Option<&str>,
    limit: usize,
) -> (Vec<String>, Option<String>) {
    let filter = RequestFilter {
        models: vec!["claude-sonnet-5".into()],
        ..Default::default()
    };
    let cursor = cursor
        .map(|c| RequestCursor::parse(c, RequestOrder::ReceivedAt).expect("a page's own cursor"));
    let (rows, next) = db
        .read_blocking(|c| {
            list_requests(
                c,
                &filter,
                &LiveKeys::default(),
                RequestOrder::ReceivedAt,
                dir,
                cursor.as_ref(),
                limit,
            )
        })
        .unwrap();
    (rows.into_iter().map(|(r, _)| r.id).collect(), next)
}

#[test]
fn a_page_of_exactly_limit_rows_answers_no_cursor() {
    let (db, _dir) = paging_ledger(5);
    let (ids, next) = sonnet_page(&db, Direction::Desc, None, 5);
    assert_eq!(ids, ["r04", "r03", "r02", "r01", "r00"]);
    assert_eq!(
        next, None,
        "no row lies beyond the page, so there is no next page to offer"
    );
}

#[test]
fn a_page_with_one_row_beyond_it_answers_a_cursor_whose_next_page_holds_that_row() {
    let (db, _dir) = paging_ledger(5);
    let (ids, next) = sonnet_page(&db, Direction::Desc, None, 4);
    assert_eq!(ids, ["r04", "r03", "r02", "r01"]);
    assert_eq!(
        next.as_deref(),
        Some("1:r01"),
        "the cursor names the last row of the page, never the row beyond it"
    );
    let (ids, next) = sonnet_page(&db, Direction::Desc, next.as_deref(), 4);
    assert_eq!(
        ids,
        ["r00"],
        "the next page holds the one row beyond the first"
    );
    assert_eq!(next, None);
    let (ids, next) = sonnet_page(&db, Direction::Asc, None, 4);
    assert_eq!(ids, ["r00", "r01", "r02", "r03"], "oldest first under asc");
    assert_eq!(next.as_deref(), Some("3:r03"));
    let (ids, next) = sonnet_page(&db, Direction::Asc, next.as_deref(), 4);
    assert_eq!(
        ids,
        ["r04"],
        "past the cursor under asc lies the newer row alone"
    );
    assert_eq!(next, None);
}

/// Five requests whose arrival order (40, 10, 30, 10, none) differs from their duration order, so a page that
/// continues by the arrival rather than the key rereads or skips a row.
#[test]
fn pages_under_a_nullable_key_continue_at_the_keys_value_and_list_a_row_without_it_last() {
    let (db, _dir) = common::temp_ledger();
    let put = |id: &str, received_at: i64, duration_ms: Option<i64>| {
        db.insert_request(
            &RequestRecord {
                id: id.into(),
                received_at,
                duration_ms,
                ..Default::default()
            },
            &[],
            None,
        )
        .unwrap();
    };
    put("r1", 1, Some(40));
    put("r2", 2, Some(10));
    put("r3", 3, Some(30));
    put("r4", 4, Some(10));
    put("r5", 5, None);
    let page = |cursor: Option<&str>, limit: usize| {
        let cursor = cursor.map(|c| {
            RequestCursor::parse(c, RequestOrder::DurationMs).expect("a page's own cursor")
        });
        let (rows, next) = db
            .read_blocking(|c| {
                list_requests(
                    c,
                    &RequestFilter::default(),
                    &LiveKeys::default(),
                    RequestOrder::DurationMs,
                    Direction::Asc,
                    cursor.as_ref(),
                    limit,
                )
            })
            .unwrap();
        (
            rows.into_iter().map(|(r, _)| r.id).collect::<Vec<_>>(),
            next,
        )
    };
    let (first, next) = page(None, 2);
    assert_eq!(first, ["r2", "r4"], "the two at 10, by id");
    assert_eq!(next.as_deref(), Some("10:r4"));
    let (second, next) = page(next.as_deref(), 2);
    assert_eq!(second, ["r3", "r1"], "past the second 10 lie 30 and 40");
    let (third, next) = page(next.as_deref(), 2);
    assert_eq!(
        (third, next),
        (vec!["r5".to_string()], None),
        "the row without a duration comes last under asc"
    );
    // A second row without a duration, so a page can end on the first and continue at its empty value.
    put("r6", 6, None);
    let (first, next) = page(None, 5);
    assert_eq!(first, ["r2", "r4", "r3", "r1", "r5"]);
    assert_eq!(next.as_deref(), Some(":r5"));
    let (rest, next) = page(next.as_deref(), 5);
    assert_eq!(
        (rest, next),
        (vec!["r6".to_string()], None),
        "past a row without the figure lie the rows without it past its id alone"
    );
}

/// A fast request the router refused before any attempt served lists under the speed it asked for, and one that
/// sent any other text as its speed under `standard`, as the rollups file them.
#[test]
fn a_fast_request_refused_before_any_attempt_is_listed_under_the_fast_speed_filter() {
    let (db, _dir) = common::temp_ledger();
    let refused = |id: &str, received_at: i64, speed: String| RequestRecord {
        id: id.into(),
        received_at,
        canonical_model: "claude-opus-5".into(),
        model: "claude-opus-5".into(),
        kind: "inference".into(),
        origin: "client".into(),
        status: Some(429),
        outcome: "exhausted".into(),
        error_code: Some("fast_lane_exhausted".into()),
        requested_speed: Some(speed),
        ..Default::default()
    };
    db.insert_request(&refused("asked-fast", 1, "fast".into()), &[], None)
        .unwrap();
    db.insert_request(&refused("asked-other", 2, "x".repeat(300)), &[], None)
        .unwrap();
    let listed = |speed: &str| {
        let filter = RequestFilter {
            speeds: vec![speed.into()],
            ..Default::default()
        };
        let (rows, _) = db
            .read_blocking(|c| {
                list_requests(
                    c,
                    &filter,
                    &LiveKeys::default(),
                    RequestOrder::ReceivedAt,
                    Direction::Desc,
                    None,
                    10,
                )
            })
            .unwrap();
        rows.into_iter().map(|(r, _)| r.id).collect::<Vec<_>>()
    };
    assert_eq!(listed("fast"), ["asked-fast"]);
    assert_eq!(
        listed("standard"),
        ["asked-other"],
        "the fast request is not standard, and the other text lists under standard alone"
    );
}

/// A fast request whose serving attempt answered without naming a speed lists under `standard`, the speed the
/// rollups file its answer under and price it at, rather than under the speed it asked for.
#[test]
fn a_fast_request_whose_answer_named_no_speed_is_listed_under_the_standard_speed_filter() {
    let (db, _dir) = common::temp_ledger();
    db.insert_request(
        &RequestRecord {
            id: "answered".into(),
            received_at: 1,
            canonical_model: "claude-opus-5".into(),
            model: "claude-opus-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(200),
            outcome: "succeeded".into(),
            attempts: 1,
            requested_speed: Some("fast".into()),
            ..Default::default()
        },
        &[AttemptRecord {
            request_id: "answered".into(),
            attempt: 1,
            account_id: "a".into(),
            account_kind: AccountKind::Oauth,
            outcome: "succeeded".into(),
            output_tokens: Some(10),
            ..Default::default()
        }],
        None,
    )
    .unwrap();
    let listed = |speed: &str| {
        let filter = RequestFilter {
            speeds: vec![speed.into()],
            ..Default::default()
        };
        let (rows, _) = db
            .read_blocking(|c| {
                list_requests(
                    c,
                    &filter,
                    &LiveKeys::default(),
                    RequestOrder::ReceivedAt,
                    Direction::Desc,
                    None,
                    10,
                )
            })
            .unwrap();
        rows.into_iter().map(|(r, _)| r.id).collect::<Vec<_>>()
    };
    let filed: Vec<String> = db
        .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
        .unwrap()
        .into_iter()
        .map(|r| r.speed)
        .collect();
    assert_eq!(
        filed,
        ["standard"],
        "the rollups file the answer as standard"
    );
    assert_eq!(listed("standard"), ["answered"]);
    assert!(
        listed("fast").is_empty(),
        "the list agrees with the rollups"
    );
}

/// Writes request `id` with one attempt per `(account, status, cache_miss_reason)`, in attempt order.
fn plant_trail(db: &LedgerDb, id: &str, received_at: i64, trail: &[(&str, u16, Option<&str>)]) {
    let request = RequestRecord {
        id: id.into(),
        received_at,
        canonical_model: "claude-sonnet-5".into(),
        model: "claude-sonnet-5".into(),
        kind: "inference".into(),
        origin: "client".into(),
        outcome: if trail.is_empty() {
            "exhausted"
        } else {
            "succeeded"
        }
        .into(),
        attempts: trail.len() as u32,
        ..Default::default()
    };
    let attempts: Vec<AttemptRecord> = trail
        .iter()
        .zip(1..)
        .map(|((account, status, reason), attempt)| AttemptRecord {
            request_id: id.into(),
            attempt,
            account_id: (*account).into(),
            account_kind: AccountKind::Oauth,
            upstream_status: Some(*status),
            outcome: if *status == 200 {
                "succeeded"
            } else {
                "rate_limited"
            }
            .into(),
            cache_miss_reason: reason.map(String::from),
            ..Default::default()
        })
        .collect();
    db.insert_request(&request, &attempts, None).unwrap();
}

/// The ids `filter` lists, newest first.
fn listed_ids(db: &LedgerDb, filter: &RequestFilter) -> Vec<String> {
    let (rows, _) = db
        .read_blocking(|c| {
            list_requests(
                c,
                filter,
                &LiveKeys::default(),
                RequestOrder::ReceivedAt,
                Direction::Desc,
                None,
                10,
            )
        })
        .unwrap();
    rows.into_iter().map(|(r, _)| r.id).collect()
}

#[test]
fn the_cache_miss_reason_filter_reads_the_serving_attempt_alone_and_passes_any_name() {
    let (db, _dir) = common::temp_ledger();
    plant_trail(
        &db,
        "r1",
        3,
        &[
            ("a", 200, Some("messages_changed")),
            ("a", 200, Some("none")),
        ],
    );
    plant_trail(&db, "r2", 2, &[("a", 200, Some("weird_new"))]);
    plant_trail(&db, "r3", 1, &[]);
    let listed = |reasons: &[&str]| {
        listed_ids(
            &db,
            &RequestFilter {
                cache_miss_reasons: reasons.iter().map(|r| r.to_string()).collect(),
                ..Default::default()
            },
        )
    };
    assert!(
        listed(&["messages_changed"]).is_empty(),
        "an earlier attempt's reason is not the one the rollups count"
    );
    assert_eq!(listed(&["none"]), ["r1"]);
    assert_eq!(
        listed(&["weird_new"]),
        ["r2"],
        "a name no release knows yet matches its own rows"
    );
    assert_eq!(
        listed(&[]),
        ["r1", "r2", "r3"],
        "an empty list narrows nothing"
    );
}

/// A listed key matches a key of the request row's copy whole: `redacted_thinking.organization_binding_mismatch` holds
/// the listed key as a part of its text, so a match on the column's text would keep its request.
#[test]
fn the_dropped_blocks_filter_keeps_a_request_whose_answer_listed_the_key_and_no_other() {
    let (db, _dir) = common::temp_ledger();
    let plant = |id: &str, received_at: i64, keys: &[&str]| {
        let request = RequestRecord {
            id: id.into(),
            received_at,
            canonical_model: "claude-sonnet-5".into(),
            model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        };
        let attempt = AttemptRecord {
            request_id: id.into(),
            attempt: 1,
            account_id: "a".into(),
            account_kind: AccountKind::Oauth,
            upstream_status: Some(200),
            outcome: "succeeded".into(),
            input_transformations: Some(
                keys.iter()
                    .map(|key| {
                        (
                            key.to_string(),
                            InputTransformation {
                                blocks: 2,
                                first_message: 0,
                            },
                        )
                    })
                    .collect(),
            ),
            ..Default::default()
        };
        db.insert_request(&request, &[attempt], None).unwrap();
    };
    plant("listed", 3, &["thinking.organization_binding_mismatch"]);
    plant(
        "other",
        2,
        &["redacted_thinking.organization_binding_mismatch"],
    );
    plant("empty", 1, &[]);
    assert_eq!(
        listed_ids(
            &db,
            &RequestFilter {
                input_transformations: vec!["thinking.organization_binding_mismatch".into()],
                ..Default::default()
            },
        ),
        ["listed"],
        "the request with the key alone, neither the one whose key holds it as text nor the one that listed none"
    );
}

/// Request `id`, served by one attempt on `served`'s `(account, stop_reason, refusal_category)` after a 429 on `a`
/// where `retried` is set, or by no attempt where `served` is `None`.
fn stop_rows(
    id: &str,
    received_at: i64,
    retried: bool,
    served: Option<(&str, &str, Option<&str>)>,
) -> (RequestRecord, Vec<AttemptRecord>) {
    let mut attempts = Vec::new();
    if let Some((account, stop_reason, category)) = served {
        if retried {
            attempts.push(AttemptRecord {
                request_id: id.into(),
                attempt: 1,
                account_id: "a".into(),
                account_kind: AccountKind::Oauth,
                upstream_status: Some(429),
                outcome: "rate_limited".into(),
                ..Default::default()
            });
        }
        attempts.push(AttemptRecord {
            request_id: id.into(),
            attempt: attempts.len() as u32 + 1,
            account_id: account.into(),
            account_kind: AccountKind::Oauth,
            upstream_status: Some(200),
            outcome: "succeeded".into(),
            stop_reason: Some(stop_reason.into()),
            refusal_category: category.map(String::from),
            ..Default::default()
        });
    }
    let request = RequestRecord {
        id: id.into(),
        received_at,
        canonical_model: "claude-sonnet-5".into(),
        model: "claude-sonnet-5".into(),
        kind: "inference".into(),
        origin: "client".into(),
        outcome: if attempts.is_empty() {
            "exhausted"
        } else {
            "succeeded"
        }
        .into(),
        attempts: attempts.len() as u32,
        ..Default::default()
    };
    (request, attempts)
}

/// Writes the request `stop_rows` builds.
fn plant_stops(
    db: &LedgerDb,
    id: &str,
    received_at: i64,
    retried: bool,
    served: Option<(&str, &str, Option<&str>)>,
) {
    let (request, attempts) = stop_rows(id, received_at, retried, served);
    db.insert_request(&request, &attempts, None).unwrap();
}

#[test]
fn the_stop_reason_and_refusal_category_filters_read_the_serving_attempt_and_dash_a_request_without_one(
) {
    let (db, _dir) = common::temp_ledger();
    plant_stops(
        &db,
        "harm-a",
        5,
        false,
        Some(("a", "refusal", Some("harmful_request"))),
    );
    // The 429 on `a` ahead of the serving attempt on `b` carries no stop reason, so a read of any attempt but the last
    // would list this request under `-` and under the account `a`.
    plant_stops(
        &db,
        "harm-b",
        4,
        true,
        Some(("b", "refusal", Some("harmful_request"))),
    );
    plant_stops(&db, "bare", 3, false, Some(("a", "refusal", None)));
    // The one row that presented a fallback credit, a made-up digest in the columns' form.
    let (ended, attempts) = stop_rows("ended", 2, false, Some(("a", "end_turn", None)));
    let ended = RequestRecord {
        fallback_digest: Some(
            "3b7e1f9a5c2d8e4b6a0f7c3d9e1b5a8f2c6d0e4a7b9f1c3e5d8a2b6f0c4e7a9d".into(),
        ),
        ..ended
    };
    db.insert_request(&ended, &attempts, None).unwrap();
    plant_stops(&db, "unserved", 1, false, None);
    let listed = |stop_reasons: &[&str], refusal_categories: &[&str], accounts: &[&str]| {
        let words = |list: &[&str]| list.iter().map(|w| w.to_string()).collect();
        listed_ids(
            &db,
            &RequestFilter {
                stop_reasons: words(stop_reasons),
                refusal_categories: words(refusal_categories),
                accounts: words(accounts),
                ..Default::default()
            },
        )
    };
    assert_eq!(
        listed(&[], &["harmful_request"], &[]),
        ["harm-a", "harm-b"],
        "the category keeps the refusals that named it"
    );
    assert_eq!(
        listed(&[], &["-"], &[]),
        ["bare", "ended", "unserved"],
        "`-` keeps every request whose serving attempt names no category, and the one no attempt served"
    );
    assert_eq!(
        listed(&["refusal"], &["-"], &[]),
        ["bare"],
        "with the refusal stop, `-` keeps the refusal that named no category"
    );
    assert_eq!(listed(&["end_turn"], &[], &[]), ["ended"]);
    assert_eq!(
        listed(&["refusal"], &[], &[]),
        ["harm-a", "harm-b", "bare"],
        "the refusal stop keeps every refusal, a category or none"
    );
    assert_eq!(
        listed(&["-"], &[], &[]),
        ["unserved"],
        "`-` keeps the request no attempt served, and the earlier 429 is no serving attempt"
    );
    assert_eq!(
        listed(&[], &["harmful_request"], &["a"]),
        ["harm-a"],
        "the account filter narrows the category to the account that served the refusal"
    );
    assert_eq!(
        listed(&["refusal"], &[], &["a"]),
        ["harm-a", "bare"],
        "the account filter narrows the refusal stop the same way"
    );
    assert_eq!(
        listed(&[], &[], &[]),
        ["harm-a", "harm-b", "bare", "ended", "unserved"],
        "empty lists narrow nothing"
    );
    let fallback = |fallback| {
        listed_ids(
            &db,
            &RequestFilter {
                fallback,
                ..Default::default()
            },
        )
    };
    assert_eq!(
        fallback(Some(true)),
        ["ended"],
        "`true` keeps the row that carries a digest"
    );
    assert_eq!(
        fallback(Some(false)),
        ["harm-a", "harm-b", "bare", "unserved"],
        "`false` keeps every row that carries none"
    );
}

/// Writes request `id` of session `s1` at `received_at` from agent `agent_id`.
fn plant_agent_request(db: &LedgerDb, id: &str, received_at: i64, agent_id: Option<&str>) {
    let request = RequestRecord {
        id: id.into(),
        received_at,
        session_id: Some("s1".into()),
        agent_id: agent_id.map(String::from),
        canonical_model: "claude-sonnet-5".into(),
        model: "claude-sonnet-5".into(),
        kind: "inference".into(),
        origin: "client".into(),
        outcome: "exhausted".into(),
        ..Default::default()
    };
    db.insert_request(&request, &[], None).unwrap();
}

#[test]
fn the_request_log_keeps_one_agents_requests_under_agent_id() {
    let (db, _dir) = common::temp_ledger();
    plant_agent_request(&db, "from-main", 3, Some("main"));
    plant_agent_request(&db, "from-agent", 2, Some("agent-7"));
    plant_agent_request(&db, "from-none", 1, None);
    let listed = |agent_id: &str| {
        listed_ids(
            &db,
            &RequestFilter {
                agent_id: Some(agent_id.into()),
                ..Default::default()
            },
        )
    };
    assert_eq!(
        listed("agent-7"),
        ["from-agent"],
        "the agent's own request, the main agent's and the one that names no agent left out"
    );
    assert_eq!(listed("main"), ["from-main"]);
    assert_eq!(
        listed("agent"),
        Vec::<String>::new(),
        "the value matches in full, never as a prefix"
    );
    assert_eq!(
        listed(""),
        Vec::<String>::new(),
        "the ledger applies the filter as given, and no request stored an empty agent id"
    );
}

/// A row's own `landed_at`, as the ledger wrote it.
fn landed_at(db: &LedgerDb, id: &str) -> Option<i64> {
    db.read_blocking(|c| {
        Ok(
            c.query_row("SELECT landed_at FROM requests WHERE id = ?1", [id], |r| {
                r.get(0)
            })?,
        )
    })
    .unwrap()
}

#[test]
fn a_list_from_a_stamp_answers_the_rows_written_after_it_and_the_newest_stamp() {
    let (db, _dir) = common::temp_ledger();
    let page = |filter: &RequestFilter, cursor: Option<&str>, limit: usize| {
        db.read_blocking(|c| {
            phoebis_ledger::list_request_page(
                c,
                filter,
                &LiveKeys::default(),
                RequestOrder::ReceivedAt,
                Direction::Desc,
                cursor
                    .map(|c| {
                        RequestCursor::parse(c, RequestOrder::ReceivedAt)
                            .expect("a page's own cursor")
                    })
                    .as_ref(),
                limit,
            )
        })
        .unwrap()
    };
    let ids = |page: &phoebis_ledger::RequestPage| -> Vec<String> {
        page.rows.iter().map(|(r, _)| r.id.clone()).collect()
    };
    let empty = page(&RequestFilter::default(), None, 10);
    assert_eq!(
        (empty.rows.len(), empty.landed_through),
        (0, 0),
        "an empty ledger answers 0"
    );
    // Written in this order, so `long` arrived first and lands third, below the rows written before it.
    for (id, received_at) in [
        ("early", 1_000),
        ("late", 3_000),
        ("long", 500),
        ("next", 2_000),
    ] {
        db.insert_request(
            &RequestRecord {
                id: id.into(),
                received_at,
                ..Default::default()
            },
            &[],
            None,
        )
        .unwrap();
    }
    let all = page(&RequestFilter::default(), None, 10);
    assert_eq!(
        all.landed_through,
        landed_at(&db, "next").unwrap(),
        "the newest stamp is the last row written"
    );
    let since = RequestFilter {
        landed_after: landed_at(&db, "late"),
        ..Default::default()
    };
    let after = page(&since, None, 10);
    assert_eq!(
        (ids(&after), after.landed_through),
        (
            vec!["next".to_string(), "long".to_string()],
            all.landed_through
        ),
        "the rows written after the stamp alone, in the list's order"
    );
    let first = page(&since, None, 1);
    let second = page(&since, first.next.as_deref(), 1);
    assert_eq!(
        (ids(&first), ids(&second), second.next),
        (vec!["next".to_string()], vec!["long".to_string()], None),
        "the cursor walks the rows the stamp keeps"
    );
    let caught_up = page(
        &RequestFilter {
            landed_after: Some(all.landed_through),
            ..Default::default()
        },
        None,
        10,
    );
    assert_eq!(
        (caught_up.rows.len(), caught_up.landed_through),
        (0, all.landed_through),
        "nothing was written since the newest stamp"
    );
}

#[test]
fn a_row_received_after_the_pages_bound_keeps_the_watermark_below_its_stamp_for_the_next_poll() {
    let (db, _dir) = common::temp_ledger();
    let page = |filter: &RequestFilter| {
        db.read_blocking(|c| {
            phoebis_ledger::list_request_page(
                c,
                filter,
                &LiveKeys::default(),
                RequestOrder::ReceivedAt,
                Direction::Desc,
                None,
                10,
            )
        })
        .unwrap()
    };
    let ids = |page: &phoebis_ledger::RequestPage| -> Vec<String> {
        page.rows.iter().map(|(r, _)| r.id.clone()).collect()
    };
    // Received after the page's `to` and written before its read, so its stamp is the ledger's newest.
    for (id, received_at) in [("inside", 1_000), ("past_the_bound", 5_000)] {
        db.insert_request(
            &RequestRecord {
                id: id.into(),
                received_at,
                ..Default::default()
            },
            &[],
            None,
        )
        .unwrap();
    }
    let bounded = page(&RequestFilter {
        to: Some(3_000),
        ..Default::default()
    });
    assert!(landed_at(&db, "past_the_bound").unwrap() > landed_at(&db, "inside").unwrap());
    assert_eq!(
        (ids(&bounded), bounded.landed_through),
        (
            vec!["inside".to_string()],
            landed_at(&db, "inside").unwrap()
        ),
        "the row the page left out moves no watermark"
    );
    let next = page(&RequestFilter {
        landed_after: Some(bounded.landed_through),
        to: Some(6_000),
        ..Default::default()
    });
    assert_eq!(
        ids(&next),
        ["past_the_bound"],
        "the next poll's bound reaches the row"
    );
}

/// The writer stamps each row from its clock, so a row received after a read's `to` lands after it too. Here the row
/// the read admits, received before `to`, lands after the row it leaves out, which was received after `to`: a
/// watermark at the admitted row's stamp would pass the other row for good.
#[tokio::test]
async fn a_read_bounded_by_to_caps_its_watermark_there_so_a_later_poll_reads_a_row_received_after_it(
) {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let to = NOW + 1_000;
    for (id, received_at, written_at) in [
        ("past_the_bound", NOW + 1_500, NOW + 2_000),
        ("inside", NOW + 500, NOW + 3_000),
    ] {
        h.clock.store(written_at, Ordering::SeqCst);
        h.ledger
            .record_request(Record::Request {
                request: RequestRecord {
                    id: id.into(),
                    received_at,
                    ..Default::default()
                },
                attempts: Vec::new(),
                bodies: None,
            })
            .await;
        ledgered(&h, id).await;
    }
    let page = |filter: RequestFilter| {
        h.db.read(move |c| {
            phoebis_ledger::list_request_page(
                c,
                &filter,
                &LiveKeys::default(),
                RequestOrder::ReceivedAt,
                Direction::Desc,
                None,
                10,
            )
        })
    };
    let ids = |page: &phoebis_ledger::RequestPage| -> Vec<String> {
        page.rows.iter().map(|(r, _)| r.id.clone()).collect()
    };
    let bounded = page(RequestFilter {
        to: Some(to),
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(ids(&bounded), ["inside"]);
    assert!(
        bounded.landed_through <= to,
        "the watermark {} stays at or under the read's to {to}",
        bounded.landed_through
    );
    let next = page(RequestFilter {
        landed_after: Some(bounded.landed_through),
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(
        ids(&next),
        ["past_the_bound", "inside"],
        "the next poll reads the row received after to, and the admitted row again"
    );
    up.shutdown().await;
}

/// A fixed range's `to` never moves, so no later bound reaches past it and the watermark may pass it. The writer
/// stamps the row from its clock, after `to`, where a capped watermark would hand the row back to every poll.
#[tokio::test]
async fn a_fixed_end_read_lets_its_watermark_pass_to_so_a_row_that_landed_after_the_end_is_read_once(
) {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let to = NOW + 1_000;
    h.clock.store(NOW + 3_000, Ordering::SeqCst);
    h.ledger
        .record_request(Record::Request {
            request: RequestRecord {
                id: "late".into(),
                received_at: NOW + 500,
                ..Default::default()
            },
            attempts: Vec::new(),
            bodies: None,
        })
        .await;
    ledgered(&h, "late").await;
    let stamp = landed_at(&h.db, "late").unwrap();
    assert!(
        stamp > to,
        "the row lands at {stamp}, past the range's to {to}"
    );
    let page = |filter: RequestFilter| {
        h.db.read(move |c| {
            phoebis_ledger::list_request_page(
                c,
                &filter,
                &LiveKeys::default(),
                RequestOrder::ReceivedAt,
                Direction::Desc,
                None,
                10,
            )
        })
    };
    let ids = |page: &phoebis_ledger::RequestPage| -> Vec<String> {
        page.rows.iter().map(|(r, _)| r.id.clone()).collect()
    };
    let capped = page(RequestFilter {
        to: Some(to),
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(
        (ids(&capped), capped.landed_through),
        (vec!["late".to_string()], to),
        "a live range's watermark stops at its to"
    );
    let fixed = page(RequestFilter {
        to: Some(to),
        fixed_end: true,
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(
        (ids(&fixed), fixed.landed_through),
        (vec!["late".to_string()], stamp),
        "a fixed range's watermark is the late row's own stamp"
    );
    let next = page(RequestFilter {
        landed_after: Some(fixed.landed_through),
        to: Some(to),
        fixed_end: true,
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(
        (ids(&next), next.landed_through),
        (Vec::<String>::new(), stamp),
        "the next poll reads no row and keeps the stamp, so the late row was read once"
    );
    up.shutdown().await;
}

#[test]
fn the_account_filter_reads_the_serving_attempt_as_the_rollups_file_it() {
    let (db, _dir) = common::temp_ledger();
    plant_trail(&db, "r1", 3, &[("a", 200, Some("messages_changed"))]);
    plant_trail(
        &db,
        "r2",
        2,
        &[("a", 429, None), ("b", 200, Some("messages_changed"))],
    );
    plant_trail(&db, "r3", 1, &[]);
    let listed = |accounts: &[&str], reasons: &[&str]| {
        listed_ids(
            &db,
            &RequestFilter {
                accounts: accounts.iter().map(|a| a.to_string()).collect(),
                cache_miss_reasons: reasons.iter().map(|r| r.to_string()).collect(),
                ..Default::default()
            },
        )
    };
    assert_eq!(
        listed(&["a"], &["messages_changed"]),
        ["r1"],
        "the failover's reason is counted on the account that answered it"
    );
    assert_eq!(listed(&["b"], &["messages_changed"]), ["r2"]);
    assert_eq!(
        listed(&["a"], &[]),
        ["r1"],
        "a request that failed over lists under its serving account alone"
    );
    assert_eq!(
        listed(&[phoebis_ledger::UNATTRIBUTED], &[]),
        ["r3"],
        "a request no attempt served lists under the reserved id"
    );
}

#[test]
fn the_effort_and_class_filters_match_the_rollups_fold() {
    let (db, _dir) = common::temp_ledger();
    let rows = [
        ("plain", None, None),
        ("high-main", Some("high"), Some("main".to_string())),
        ("ultra-bad", Some("ultra"), Some("bad class".to_string())),
        ("low-long", Some("low"), Some("x".repeat(65))),
    ];
    for (at, (id, effort, class)) in rows.into_iter().enumerate() {
        db.insert_request(
            &RequestRecord {
                id: id.into(),
                received_at: at as i64,
                canonical_model: "claude-sonnet-5".into(),
                model: "claude-sonnet-5".into(),
                kind: "inference".into(),
                origin: "client".into(),
                outcome: "succeeded".into(),
                effort: effort.map(String::from),
                request_class: class,
                ..Default::default()
            },
            &[],
            None,
        )
        .unwrap();
    }
    let by_effort = |efforts: &[&str]| {
        listed_ids(
            &db,
            &RequestFilter {
                efforts: efforts.iter().map(|e| e.to_string()).collect(),
                ..Default::default()
            },
        )
    };
    let by_class = |classes: &[&str]| {
        listed_ids(
            &db,
            &RequestFilter {
                request_classes: classes.iter().map(|c| c.to_string()).collect(),
                ..Default::default()
            },
        )
    };
    assert_eq!(
        by_effort(&[phoebis_ledger::fold_effort(None)]),
        ["plain"],
        "a request that asked for no level matches the reserved word"
    );
    assert_eq!(
        by_effort(&[phoebis_ledger::OTHER_DIMENSION]),
        ["ultra-bad"],
        "a level outside the six matches the fold's word alone"
    );
    assert_eq!(by_effort(&["high"]), ["high-main"]);
    assert_eq!(
        by_class(&[phoebis_ledger::NO_CLASS]),
        ["plain"],
        "a request that sent no class matches the reserved class"
    );
    assert_eq!(
        by_class(&[phoebis_ledger::OTHER_DIMENSION]),
        ["low-long", "ultra-bad"],
        "a class outside the charset or past the bound matches the fold's word alone"
    );
    assert_eq!(by_class(&["main"]), ["high-main"]);
}

/// Held on a slow `land`, a batch and 8 requests past `CHANNEL_DEPTH` offered through `record_request` all wait for
/// room rather than dropping.
#[tokio::test]
async fn a_request_record_waits_for_the_writer_rather_than_dropping() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let db = h.db.clone();
    let (held_tx, held_rx) = tokio::sync::oneshot::channel::<()>();
    let (hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        db.write(|_| {
            held_tx.send(()).unwrap();
            hold_rx.recv().unwrap();
            Ok(())
        })
        .unwrap()
    });
    held_rx.await.unwrap();
    // The writer takes up to one batch (64) off the channel before it blocks behind the holder, so a batch more
    // than the depth fills the channel and at least 8 sends are left waiting for room.
    let total = phoebis_ledger::CHANNEL_DEPTH + 64 + 8;
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
    wait_for(Duration::from_secs(5), || {
        (h.ledger.queued() == phoebis_ledger::CHANNEL_DEPTH).then_some(())
    })
    .await;
    hold_tx.send(()).unwrap();
    holder.join().unwrap();
    for send in sends {
        tokio::time::timeout(Duration::from_secs(10), send)
            .await
            .expect("every send resolves once the writer drains")
            .unwrap();
    }
    // A send resolves once the channel holds its record, so the shutdown, which lands what the channel holds, comes
    // between the last send and the reads of the rows.
    h.ledger.clone().shutdown().await.unwrap();
    assert_eq!(h.ledger.dropped(), 0);
    for i in 0..total {
        assert!(
            h.db.request(&format!("r{i}")).await.unwrap().is_some(),
            "r{i} landed"
        );
    }
    up.shutdown().await;
}

/// The harness's clock never moves, so both rows land in one of its milliseconds, and the second still stamps above the
/// first.
#[tokio::test]
async fn two_request_rows_written_in_one_clock_millisecond_carry_increasing_stamps() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    for id in ["first", "second"] {
        h.ledger
            .record_request(Record::Request {
                request: RequestRecord {
                    id: id.into(),
                    ..Default::default()
                },
                attempts: Vec::new(),
                bodies: None,
            })
            .await;
    }
    ledgered(&h, "second").await;
    let page = |filter: RequestFilter| {
        h.db.read_blocking(|c| {
            phoebis_ledger::list_request_page(
                c,
                &filter,
                &LiveKeys::default(),
                RequestOrder::ReceivedAt,
                Direction::Desc,
                None,
                10,
            )
        })
        .unwrap()
    };
    let first = page(RequestFilter {
        request: Some("first".into()),
        ..Default::default()
    });
    assert_eq!(
        first.landed_through, NOW,
        "the first row takes the clock's instant"
    );
    let after = page(RequestFilter {
        landed_after: Some(first.landed_through),
        ..Default::default()
    });
    assert_eq!(
        (
            after
                .rows
                .iter()
                .map(|(r, _)| r.id.as_str())
                .collect::<Vec<_>>(),
            after.landed_through
        ),
        (vec!["second"], NOW + 1),
        "a poll from the first row's stamp answers the second, stamped one past it"
    );
    up.shutdown().await;
}

/// Holds the writer at a blocked `land` and queues request records until the channel is past its mark. A send on the
/// returned sender releases the hold, and joining the returned thread waits for the held write to end.
async fn hold_the_writer_past_its_mark(
    h: &Harness,
) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
    let db = h.db.clone();
    let (held_tx, held_rx) = tokio::sync::oneshot::channel::<()>();
    let (hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        db.write(|_| {
            held_tx.send(()).unwrap();
            hold_rx.recv().unwrap();
            Ok(())
        })
        .unwrap()
    });
    held_rx.await.unwrap();
    // The writer takes up to one batch (64) off the channel before it blocks behind the holder, so a batch more
    // than the mark is queued and the mark is read against what the channel still holds.
    for i in 0..LEDGER_BEHIND_DEPTH + 64 {
        h.ledger
            .record_request(Record::Request {
                request: RequestRecord {
                    id: format!("r{i}"),
                    ..Default::default()
                },
                attempts: Vec::new(),
                bodies: None,
            })
            .await;
    }
    assert!(
        h.ledger.queued() >= LEDGER_BEHIND_DEPTH,
        "{} queued",
        h.ledger.queued()
    );
    assert!(h.ledger.behind());
    (hold_tx, holder)
}

/// Held at a blocked `land`, the writer channel fills past its mark: the next inference is refused with the router's
/// own 503 before any account is chosen and counted, and once the writer drains the same caller is served.
#[tokio::test]
async fn an_inference_is_refused_with_ledger_behind_while_the_writer_channel_is_past_its_mark() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let (hold_tx, holder) = hold_the_writer_past_its_mark(&h).await;
    let response = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(response.headers()["x-phoebis-error-code"], "ledger_behind");
    assert_eq!(response.headers()["retry-after"], "5");
    assert!(up.requests().is_empty(), "nothing reached the upstream");
    let gauges = phoebis_ledger::gather(
        &h.router,
        PriceRevisions::shipped().in_force(NOW),
        NOW,
        60_000,
        None,
        AttentionGauges::default(),
        Some(&h.ledger),
    )
    .await;
    assert!(
        gauges.ledger_queued >= LEDGER_BEHIND_DEPTH as u64,
        "{} queued at the scrape",
        gauges.ledger_queued
    );
    assert_eq!(
        gauges.ledger_behind_refusals, 1,
        "the one refusal is counted"
    );
    hold_tx.send(()).unwrap();
    holder.join().unwrap();
    wait_for(Duration::from_secs(10), || {
        (!h.ledger.behind()).then_some(())
    })
    .await;
    let response = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s2"))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    up.shutdown().await;
}

/// A request refused for another reason while the writer is behind is no `ledger_behind` answer, so the refusal
/// count moves for the 503 alone.
#[tokio::test]
async fn a_request_refused_for_another_reason_while_the_ledger_is_behind_leaves_the_refusal_count_alone(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let (hold_tx, holder) = hold_the_writer_past_its_mark(&h).await;
    let refusals = || h.ledger.stats().behind_refusals.load(Ordering::SeqCst);
    let response = h
        .app
        .clone()
        .oneshot(request_for("phoebis-not-a-key", "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    assert_eq!(refusals(), 0, "an unknown key is no ledger_behind answer");
    let response = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(response.headers()["x-phoebis-error-code"], "ledger_behind");
    assert_eq!(refusals(), 1, "the ledger_behind answer is counted");
    assert!(up.requests().is_empty(), "nothing reached the upstream");
    hold_tx.send(()).unwrap();
    holder.join().unwrap();
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

/// The bus subscriber's queue filled to within a batch of its depth behind a writer channel filled to its own, both
/// behind the same held `land`: `shutdown` still returns and lands every completed request, because the writer keeps
/// reading while it waits for the assembler to end rather than waiting for it first.
#[tokio::test]
async fn a_shutdown_with_both_queues_full_lands_every_completed_request() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let db = h.db.clone();
    let (held_tx, held_rx) = tokio::sync::oneshot::channel::<()>();
    let (hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        db.write(|_| {
            held_tx.send(()).unwrap();
            hold_rx.recv().unwrap();
            Ok(())
        })
        .unwrap()
    });
    held_rx.await.unwrap();
    // Every request fits behind the held writer: the channel holds its depth and the bus queue two facts for each of
    // the rest, short only of what the writer took (up to one batch, 64) and the record the assembler holds. So the
    // publisher ends with both queues full or within a batch of it.
    let total = phoebis_ledger::CHANNEL_DEPTH + phoebis_bus::QUEUE_DEPTH / 2;
    let bus = h.bus.clone();
    // One task, publishing in order through `publish_fact` (which waits for room rather than dropping, as the
    // relay does).
    let publisher = tokio::spawn(async move {
        for i in 0..total {
            let id = format!("q{i}");
            bus.publish_fact(observed_event(&id)).await;
            bus.publish_fact(complete_event(&id)).await;
        }
    });
    tokio::time::timeout(Duration::from_secs(30), publisher)
        .await
        .expect("every fact fits the two queues behind the held writer")
        .unwrap();
    wait_for(Duration::from_secs(5), || {
        (h.ledger.queued() == phoebis_ledger::CHANNEL_DEPTH).then_some(())
    })
    .await;
    hold_tx.send(()).unwrap();
    // The bound proves shutdown returns rather than deadlocks. A loaded build host stretches this drain past 10 s.
    let shutdown = tokio::time::timeout(Duration::from_secs(30), h.ledger.clone().shutdown()).await;
    holder.join().unwrap();
    shutdown
        .expect("the writer keeps reading while it waits for the assembler, so shutdown does not deadlock")
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
    // The session's own request places the pin a probe renews.
    let real = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    assert_eq!(real.status(), 200);
    let _ = real.into_body().collect().await.unwrap();
    let resp = h
        .relay
        .probe(
            phoebis_relay::Probe {
                key_id: "desktop".into(),
                key_generation: h.store.api_key("desktop").unwrap().unwrap().created_at,
                holder: phoebis_routing::Holder {
                    account_id: "a".into(),
                    incarnation: h.store.account("a").unwrap().unwrap().incarnation,
                },
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
        h.db.read_blocking(|c| {
            phoebis_ledger::read_rollups(c, 0, i64::MAX, &phoebis_ledger::RollupSelect::default())
        })
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
        incarnation: 1,
        account_kind: "oauth".into(),
        usage_based: false,
        pin_epoch: None,
        started_at: at,
        cache_ttl_ms: None,
        selection: None,
        previous_input_tokens: None,
        entry_expired: None,
        agent_released: None,
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
async fn a_relayed_heads_reading_reaches_the_router_and_a_head_sample_with_its_instant() {
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
            .find(|q| q.window == "five_hour")
    })
    .await;
    assert_eq!(
        (row.utilization, row.reset_at, row.source.as_str()),
        (Some(0.3125), Some(4_600_000), "head")
    );
    assert_eq!(
        phoebis_ledger::bucket_of(row.observed_at),
        row.bucket,
        "the head's instant lands beside its bucket"
    );
    up.shutdown().await;
}

/// The flag is the ledger's reading at landing: a refusal before any attempt reads refused, a refusal at the reselect
/// after an attempt stays a rejected request, and a probe the registry refused stays on its probe row.
#[tokio::test]
async fn a_request_landed_with_a_refusal_code_and_no_attempt_reads_refused_and_one_after_an_attempt_does_not(
) {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let rejection = |id: &str, attempt: u8, code: &str| {
        let BusEvent::RequestComplete(complete) = complete_event(id) else {
            unreachable!()
        };
        BusEvent::RequestComplete(RequestComplete {
            attempt,
            outcome: BusOutcome::Rejected,
            status: 403,
            error_code: Some(code.into()),
            ..complete
        })
    };
    h.bus.publish(observed_event("blocked"));
    h.bus.publish(rejection("blocked", 0, "guardrail_blocked"));
    h.bus.publish(observed_event("reselected"));
    h.bus.publish(selected_event("reselected", 1, "a", 10));
    h.bus.publish(head_event("reselected", 429, 20));
    h.bus
        .publish(rejection("reselected", 1, "client_not_allowed"));
    for event in refused_probe_events("refused", "auth_unknown") {
        h.bus.publish(event);
    }
    let blocked = listed_with_probes(&h, "blocked").await;
    assert_eq!(
        (blocked.attempts, blocked.refused),
        (0, true),
        "a block before any attempt is a refusal"
    );
    let reselected = listed_with_probes(&h, "reselected").await;
    assert_eq!(
        (reselected.attempts, reselected.refused),
        (1, false),
        "a refusal at the reselect after an attempt is a rejected request"
    );
    let probe = listed_with_probes(&h, "refused").await;
    assert_eq!(
        (probe.kind.as_str(), probe.refused),
        ("probe", false),
        "a probe is the router's own request and never a refusal"
    );
    up.shutdown().await;
}

/// A parked record serialized before the `refused` member reads it unset.
#[test]
fn a_parked_record_from_before_the_refused_flag_reads_false() {
    let parked: phoebis_ledger::ParkedRecord =
        serde_json::from_str(r#"{"request":{"id":"r1","received_at":5},"attempts":[]}"#)
            .expect("a record from before the member still reads");
    assert!(
        !parked.request.refused,
        "a parked record from before the member reads unflagged"
    );
}

#[tokio::test]
async fn a_request_observed_as_a_classifier_call_round_trips_the_standing_and_files_its_rollup_under_client(
) {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    for (id, classifier) in [
        ("classifier-call", Some(Classifier::Client)),
        ("main-turn", None),
    ] {
        let BusEvent::RequestObserved(mut observed) = observed_event(id) else {
            unreachable!()
        };
        observed.client.classifier = classifier;
        h.bus.publish(BusEvent::RequestObserved(observed));
        h.bus.publish(selected_event(id, 1, "a", 10));
        h.bus.publish(head_event(id, 200, 20));
        h.bus
            .publish(answered_event(id, 200, ResponseFacts::default()));
    }
    let (marked, _) = ledgered(&h, "classifier-call").await;
    let (plain, _) = ledgered(&h, "main-turn").await;
    assert_eq!(marked.classifier.as_deref(), Some("client"));
    assert_eq!(
        plain.classifier, None,
        "a request without the marker records no standing"
    );
    let mut rollups: Vec<(String, u64)> =
        h.db.read_blocking(|c| {
            phoebis_ledger::read_rollups(c, 0, i64::MAX, &phoebis_ledger::RollupSelect::default())
        })
        .unwrap()
        .into_iter()
        .filter(|r| r.requests > 0)
        .map(|r| (r.classifier, r.requests))
        .collect();
    rollups.sort();
    assert_eq!(
        rollups,
        [("-".to_string(), 1), ("client".to_string(), 1)],
        "the classifier call files on a rollup row of its own"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_classifier_call_whose_completion_carries_the_off_answer_lands_client_off_over_the_observed_word(
) {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let BusEvent::RequestObserved(mut observed) = observed_event("off-call") else {
        unreachable!()
    };
    observed.client.classifier = Some(Classifier::Client);
    h.bus.publish(BusEvent::RequestObserved(observed));
    h.bus.publish(selected_event("off-call", 1, "a", 10));
    h.bus.publish(head_event("off-call", 200, 20));
    let BusEvent::RequestComplete(complete) =
        answered_event("off-call", 200, ResponseFacts::default())
    else {
        unreachable!()
    };
    h.bus.publish(BusEvent::RequestComplete(RequestComplete {
        classifier: Some(Classifier::ClientOff),
        ..complete
    }));
    let (row, _) = ledgered(&h, "off-call").await;
    assert_eq!(row.classifier.as_deref(), Some("client_off"));
    up.shutdown().await;
}

#[tokio::test]
async fn a_contract_refusal_round_trips_its_item() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    h.bus.publish(observed_event("contract-refused"));
    let BusEvent::RequestComplete(refused) = complete_event("contract-refused") else {
        unreachable!()
    };
    h.bus.publish(BusEvent::RequestComplete(RequestComplete {
        outcome: BusOutcome::Rejected,
        status: 403,
        error_code: Some("client_contract".into()),
        client_contract: Some("class_header".into()),
        ..refused
    }));
    let (row, attempts) = ledgered(&h, "contract-refused").await;
    assert_eq!(
        (
            row.outcome.as_str(),
            row.error_code.as_deref(),
            row.client_contract.as_deref()
        ),
        ("rejected", Some("client_contract"), Some("class_header"))
    );
    assert!(
        attempts.is_empty(),
        "a refusal before any account has no attempt"
    );
    up.shutdown().await;
}

/// A decision whose every column reads a value of its own: a probe's move from `a` to `b` under session `s1`.
/// Built over a refusal's record, since the columns come off the request and outcome blocks alone.
fn decision(
    request_id: &str,
    seq: u32,
    attempt: u32,
) -> (
    phoebis_routing::DecisionSummary,
    phoebis_routing::DecisionRecord,
) {
    let snapshot = phoebis_routing::RegistrySnapshot::from_load(
        phoebis_store::RegistryLoad::default(),
        [0; 32],
        0,
        1,
    );
    let (mut record, mut summary) = phoebis_routing::DecisionRecord::refused(
        &phoebis_routing::RefusedRequest {
            request_id: request_id.into(),
            attempt,
            seq,
            at: NOW,
            key_id: "desktop".into(),
            key_generation: 0,
            agent_id: "main".into(),
            parent_agent_id: None,
            session_id: Some("s1".into()),
            request_class: None,
            canonical_model: "claude-sonnet-5".into(),
            lane: Default::default(),
            flags: Default::default(),
            chain: Default::default(),
            attempted: Vec::new(),
            holds: Vec::new(),
            client: phoebis_routing::ClientClass::ClaudeCode,
            client_version: None,
        },
        &snapshot,
        &phoebis_routing::RefusalFacts {
            kind: "key",
            code: "unknown",
            reason: "the key is unknown".into(),
        },
    );
    record.request.pool_id = "main".into();
    record.request.flags.probe = true;
    record.outcome.kind = "moved";
    record.outcome.account_id = Some("b".into());
    record.outcome.from_account_id = Some("a".into());
    summary.kind = "moved";
    summary.probe = true;
    (summary, record)
}

#[tokio::test]
async fn a_decision_lands_as_a_row_whose_frame_decodes_to_the_record() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let (summary, record) = decision("r1", 2, 2);
    let row = DecisionRow::of(summary.clone(), record.clone(), NOW);
    h.ledger.record(Record::Decision(Box::new(row.clone())));
    let one = phoebis_ledger::RoutingLogFilter {
        request: Some("r1".into()),
        ..log_filter()
    };
    let listed = wait_for(Duration::from_secs(5), || {
        let page = h
            .db
            .read_blocking(|c| phoebis_ledger::list_routing_log(c, &one, Direction::Desc, None, 25))
            .unwrap();
        match page.rows.as_slice() {
            [phoebis_ledger::RoutingLogRow::Decision(row)] => Some(row.clone()),
            _ => None,
        }
    })
    .await;
    let some = |value: &str| Some(value.to_string());
    assert_eq!(
        (
            listed.attempt,
            listed.at,
            listed.kind,
            listed.probe,
            listed.key_id,
            listed.pool_id,
            listed.account_id,
            listed.from_account_id,
            listed.session_id,
            listed.agent_id,
            listed.model
        ),
        (
            2,
            NOW,
            "moved".to_string(),
            true,
            some("desktop"),
            some("main"),
            some("b"),
            some("a"),
            some("s1"),
            some("main"),
            some("claude-sonnet-5")
        ),
        "the columns are the record's request and outcome facts"
    );
    assert_eq!(
        listed.summary,
        serde_json::to_value(&summary).unwrap(),
        "the summary is stored as its JSON"
    );
    let mut read =
        h.db.read_blocking(|c| phoebis_ledger::decision_record(c, "r1", 2))
            .unwrap()
            .expect("the decision is stored");
    // The read composes the completion from the request's own rows, so only the rest of it comes from the frame.
    read["outcome"]
        .as_object_mut()
        .expect("the record has an outcome")
        .remove("completion");
    let mut answer = serde_json::Map::new();
    answer.insert("request_id".into(), "r1".into());
    answer.insert("seq".into(), 2.into());
    answer.extend(
        serde_json::to_value(&record)
            .unwrap()
            .as_object()
            .unwrap()
            .clone(),
    );
    assert_eq!(read, serde_json::Value::Object(answer));
    // The routing-log tests store their rows through `put_decision`, so its frame has to be the writer's. No reader
    // shows the header's content size, since `decode_record` also reads a frame without one.
    let frame_of = |db: &LedgerDb| {
        db.read_blocking(|c| {
            Ok(c.query_row(
                "SELECT record FROM decisions WHERE request_id = 'r1' AND seq = 2",
                [],
                |r| r.get::<_, Vec<u8>>(0),
            )?)
        })
        .unwrap()
    };
    let (helper, _helper_dir) = common::temp_ledger();
    put_decision(&helper, &row);
    let frame = frame_of(&h.db);
    assert_eq!(
        frame,
        frame_of(&helper),
        "`put_decision` writes the writer's frame"
    );
    let decoded = zstd::decode_all(frame.as_slice()).expect("the record is one zstd frame");
    assert_eq!(
        zstd::zstd_safe::get_frame_content_size(&frame)
            .ok()
            .flatten(),
        Some(decoded.len() as u64),
        "the frame's header names the record's size, so a reader can bound its output before it decodes"
    );
    up.shutdown().await;
}

/// A refusal under a key the snapshot no longer holds spells its pool empty, and the row stores no pool, so no
/// filter finds it under an empty id.
#[test]
fn a_decision_whose_pool_is_empty_reads_back_with_no_pool() {
    let (db, _dir) = common::temp_ledger();
    let (summary, mut record) = decision("r1", 1, 1);
    record.request.pool_id = String::new();
    put_decision(&db, &DecisionRow::of(summary, record, NOW));
    let listed = |pools: Vec<String>| {
        let filter = phoebis_ledger::RoutingLogFilter {
            pools,
            ..log_filter()
        };
        db.read_blocking(|c| {
            phoebis_ledger::list_routing_log(c, &filter, Direction::Desc, None, 25)
        })
        .unwrap()
        .rows
    };
    let all = listed(Vec::new());
    let [phoebis_ledger::RoutingLogRow::Decision(row)] = all.as_slice() else {
        panic!("{all:?}")
    };
    assert_eq!(row.pool_id, None);
    assert!(
        listed(vec![String::new()]).is_empty(),
        "no filter finds it under an empty id"
    );
}

#[tokio::test]
async fn a_cache_map_write_lands_on_the_decision_its_seq_names_and_a_stray_one_counts() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    for (seq, attempt) in [(1, 1), (2, 2)] {
        let (summary, record) = decision("r1", seq, attempt);
        h.ledger.record(Record::Decision(Box::new(DecisionRow::of(
            summary, record, NOW,
        ))));
    }
    let entries = vec![phoebis_routing::CacheMapEntry {
        tier: "message",
        depth: 2,
        prefix_tokens: 1_200,
        ttl_ms: 300_000,
        account_id: "b".into(),
    }];
    // A retry in place on the second leg: its third wire attempt completed under the decision that selected it.
    h.ledger.record(Record::CacheMap(CacheMapWrite {
        request_id: "r1".into(),
        seq: 2,
        attempt: 3,
        entries: entries.clone(),
        at: NOW + 1_000,
    }));
    h.ledger.record(Record::CacheMap(CacheMapWrite {
        request_id: "r-unknown".into(),
        seq: 1,
        attempt: 1,
        entries: entries.clone(),
        at: NOW + 1_000,
    }));
    let stats = h.ledger.stats();
    wait_for(Duration::from_secs(5), || {
        (stats.cache_map_orphans.load(Ordering::Relaxed) == 1).then_some(())
    })
    .await;
    let read = |request: &str, seq: u32| {
        h.db.read_blocking(|c| phoebis_ledger::decision_record(c, request, seq))
            .unwrap()
    };
    let written = |seq: u32| {
        read("r1", seq).expect("the decision is stored")["outcome"]["completion"]
            ["cache_map_written"]
            .clone()
    };
    assert_eq!(
        (written(1), written(2)),
        (
            serde_json::Value::Null,
            serde_json::to_value(&entries).unwrap()
        ),
        "the map lands on the decision its seq names whatever attempt completed"
    );
    assert_eq!(read("r-unknown", 1), None, "the stray write lands nowhere");
    up.shutdown().await;
}

#[tokio::test]
async fn a_request_through_the_relay_lands_its_decision_with_the_cache_map_its_answer_wrote() {
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
    let record = wait_for(Duration::from_secs(5), || {
        h.db.read_blocking(|c| phoebis_ledger::decision_record(c, &id, 1))
            .unwrap()
            .filter(|record| !record["outcome"]["completion"]["cache_map_written"].is_null())
    })
    .await;
    let outcome = &record["outcome"];
    assert_eq!(
        (outcome["kind"].as_str(), outcome["account_id"].as_str()),
        (Some("placed"), Some("a"))
    );
    assert!(
        outcome["completion"]["cache_map_written"].is_array(),
        "the answer's cache-map rows land as a list on the decision that selected it"
    );
    assert_eq!(
        h.ledger.stats().cache_map_orphans.load(Ordering::Relaxed),
        0
    );
    up.shutdown().await;
}

#[test]
fn an_incomplete_request_counts_in_the_rollups_incomplete_column_and_nowhere_else() {
    let (db, _dir) = common::temp_ledger();
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
            account_kind: AccountKind::Oauth,
            outcome: "upstream_error".into(),
            upstream_status: Some(200),
            output_tokens: Some(5),
            ..Default::default()
        }],
        None,
    )
    .unwrap();
    let rows = db
        .read_blocking(|c| {
            phoebis_ledger::read_rollups(c, 0, i64::MAX, &phoebis_ledger::RollupSelect::default())
        })
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
        .read_blocking(|c| {
            phoebis_ledger::key_usage_windows(
                c,
                "desktop",
                1,
                7_200_000,
                &PriceRevisions::shipped(),
            )
        })
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

/// An assembler on a clock the test steps, with `r1` observed and its first attempt selected at `NOW + 10`.
fn assembler_on_a_stepped_clock() -> (Assembler, Arc<std::sync::atomic::AtomicI64>) {
    let clock = Arc::new(std::sync::atomic::AtomicI64::new(NOW));
    let reading = clock.clone();
    let mut assembler = Assembler::new(Arc::new(move || reading.load(Ordering::SeqCst)));
    assembler.observe(observed_event("r1"));
    assembler.observe(selected_event("r1", 1, "a", NOW + 10));
    (assembler, clock)
}

/// The completion of `r1` on its first attempt at `finished_at`.
fn finished_event(finished_at: i64) -> BusEvent {
    let BusEvent::RequestComplete(mut complete) = complete_event("r1") else {
        unreachable!()
    };
    complete.attempt = 1;
    complete.account_id = Some("a".into());
    complete.finished_at = finished_at;
    BusEvent::RequestComplete(complete)
}

fn chunk_with_phase(index: u32, phase: Option<Phase>) -> BusEvent {
    let BusEvent::ResponseChunk(mut chunk) = chunk_event("r1", index, "data: {}\n\n") else {
        unreachable!()
    };
    chunk.phase = phase;
    BusEvent::ResponseChunk(chunk)
}

#[test]
fn an_assembled_request_carries_its_write_and_stream_end_instants() {
    let (mut assembler, clock) = assembler_on_a_stepped_clock();
    let BusEvent::ResponseHead(mut head) = head_event("r1", 200, NOW + 120) else {
        unreachable!()
    };
    head.written_at = NOW + 50;
    assembler.observe(BusEvent::ResponseHead(head));
    clock.store(NOW + 900, Ordering::SeqCst);
    assembler.observe(chunk_with_phase(0, Some(Phase::MessageStop)));
    clock.store(NOW + 1_000, Ordering::SeqCst);
    let records = assembler.observe(finished_event(NOW + 1_000));
    let [Record::Request {
        request, attempts, ..
    }] = records.as_slice()
    else {
        panic!("{} records at completion", records.len());
    };
    assert_eq!(
        attempts[0].written_at,
        Some(NOW + 50),
        "the relay's own write stamp, apart from the instant the head was observed"
    );
    assert_eq!(
        attempts[0].ttfb_ms,
        Some(110),
        "the observed instant still measures the first byte"
    );
    assert_eq!(
        request.stream_end_at,
        Some(NOW + 900),
        "the ledger's clock at the message_stop chunk"
    );
}

#[test]
fn a_stream_without_a_message_stop_chunk_carries_no_stream_end() {
    let (mut assembler, clock) = assembler_on_a_stepped_clock();
    assembler.observe(head_event("r1", 200, NOW + 120));
    clock.store(NOW + 500, Ordering::SeqCst);
    assembler.observe(chunk_with_phase(0, None));
    clock.store(NOW + 900, Ordering::SeqCst);
    assembler.observe(chunk_with_phase(1, None));
    let records = assembler.observe(finished_event(NOW + 1_000));
    let [Record::Request { request, .. }] = records.as_slice() else {
        panic!("{} records at completion", records.len());
    };
    assert_eq!(
        request.stream_end_at, None,
        "a chunk without the phase stamps nothing"
    );
}

#[test]
fn a_stream_end_the_ledger_reads_after_the_finish_is_held_to_the_finish() {
    let (mut assembler, clock) = assembler_on_a_stepped_clock();
    assembler.observe(head_event("r1", 200, NOW + 120));
    clock.store(NOW + 1_200, Ordering::SeqCst);
    assembler.observe(chunk_with_phase(0, Some(Phase::MessageStop)));
    let records = assembler.observe(finished_event(NOW + 1_000));
    let [Record::Request { request, .. }] = records.as_slice() else {
        panic!("{} records at completion", records.len());
    };
    assert_eq!(
        request.stream_end_at,
        Some(NOW + 1_000),
        "a stream end read after the finish is held to the finish"
    );
}

#[test]
fn a_second_message_stop_chunk_keeps_the_first_chunks_stream_end() {
    let (mut assembler, clock) = assembler_on_a_stepped_clock();
    assembler.observe(head_event("r1", 200, NOW + 120));
    clock.store(NOW + 500, Ordering::SeqCst);
    assembler.observe(chunk_with_phase(0, Some(Phase::MessageStop)));
    clock.store(NOW + 900, Ordering::SeqCst);
    assembler.observe(chunk_with_phase(1, Some(Phase::MessageStop)));
    let records = assembler.observe(finished_event(NOW + 1_000));
    let [Record::Request { request, .. }] = records.as_slice() else {
        panic!("{} records at completion", records.len());
    };
    assert_eq!(
        request.stream_end_at,
        Some(NOW + 500),
        "the first message_stop chunk wins"
    );
}

#[test]
fn a_fast_attempt_prices_at_the_fast_cells_beside_a_standard_one() {
    // The test list's Opus 5.5 row in force at `NOW`, its first entry before its date, per million: input 10 and
    // output 50 at the standard cells, 20 and 100 at the fast ones, so a million input and 100,000 output tokens are
    // $15 standard and $30 fast.
    let revisions = phoebis_ledger::test_tables::api_prices();
    let table = revisions.in_force(NOW);
    let (input, output) = (1_000_000_u64, 100_000_u64);
    let estimate = |speed: Speed| {
        table
            .estimate(
                "claude-opus-5-5",
                speed,
                phoebis_routing::PinUsage {
                    input,
                    cache_read: 0,
                    write_5m: 0,
                    write_1h: 0,
                    output,
                },
            )
            .expect("the test list carries the model")
            .total()
    };
    assert!(
        (estimate(Speed::Fast) - 30.0).abs() < 1e-9,
        "the answer that reported speed fast prices at the fast cells"
    );
    assert!(
        (estimate(Speed::Standard) - 15.0).abs() < 1e-9,
        "the one that reported none at the standard cells"
    );
    let attempt = |attempt: u32, speed: Option<&str>| AttemptRecord {
        request_id: "r1".into(),
        attempt,
        account_id: "a".into(),
        account_kind: AccountKind::ApiKey,
        outcome: "succeeded".into(),
        input_tokens: Some(input),
        output_tokens: Some(output),
        speed: speed.map(String::from),
        ..Default::default()
    };
    let (db, _dir) = common::temp_ledger();
    db.insert_request(
        &RequestRecord {
            id: "r1".into(),
            received_at: NOW,
            key_id: Some("desktop".into()),
            pool_id: Some("main".into()),
            canonical_model: "claude-opus-5-5".into(),
            model: "claude-opus-5-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            attempts: 2,
            ..Default::default()
        },
        &[attempt(1, Some("fast")), attempt(2, None)],
        None,
    )
    .unwrap();
    let rows = db
        .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| (r.speed.as_str(), r.attempts, r.input_tokens))
            .collect::<Vec<_>>(),
        [("fast", 1, input), ("standard", 1, input)],
        "each attempt folds under its own speed"
    );
    let live = LiveKeys(std::collections::BTreeMap::from([(
        "desktop".to_string(),
        0,
    )]));
    let groups = breakdown(
        &apply_filter(rows, &RollupFilter::default()),
        phoebis_ledger::GroupBy::Model,
        false,
        &live,
        &[],
        &revisions,
        &phoebis_ledger::test_tables::subscription_costs(),
        NOW,
    );
    let group = groups
        .iter()
        .find(|g| g.key == "claude-opus-5-5")
        .expect("one model group");
    assert!(
        (group.cost_usd - 45.0).abs() < 1e-9,
        "the hour's rows price the fast attempt at the fast cells and the other at the standard ones: {}",
        group.cost_usd
    );
}

/// The landing counts a request with usage on a model the revision in force at its receipt lacks, once however many of
/// its attempts answered, and a request without usage not at all, since it has nothing to price.
#[tokio::test]
async fn a_request_with_usage_on_an_unlisted_model_is_counted_once_at_the_landing() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(
        &up,
        &[api_key("a", C, 0)],
        (0, None),
        LedgerConfig::default(),
    )
    .await;
    let unlisted = |id: &str| RequestRecord {
        canonical_model: "gpt-5".into(),
        model: "gpt-5".into(),
        attempts: 2,
        ..finished_row(id)
    };
    h.ledger
        .record_request(Record::Request {
            request: unlisted("r-usage"),
            attempts: vec![
                serving_attempt("r-usage", true),
                AttemptRecord {
                    attempt: 2,
                    ..serving_attempt("r-usage", true)
                },
            ],
            bodies: None,
        })
        .await;
    h.ledger
        .record_request(Record::Request {
            request: unlisted("r-none"),
            attempts: vec![serving_attempt("r-none", false)],
            bodies: None,
        })
        .await;
    ledgered(&h, "r-usage").await;
    ledgered(&h, "r-none").await;
    assert_eq!(
        h.pricing.unknown_models(),
        1,
        "two answered attempts count their request once, and the request without usage is not counted"
    );
    up.shutdown().await;
}

/// A profile read that names `tier`, as the quota poll stores it.
fn set_tier(store: &Store, account: &str, tier: &str) {
    store
        .set_profile(
            account,
            r#"{"account":{"uuid":"u","email":"a@example.invalid"},"organization":{}}"#,
            &phoebis_store::SubscriptionRow {
                tier: Some(tier.into()),
                since: Some(NOW),
                trial_ends_at: None,
            },
        )
        .unwrap();
}

/// The landing writes the serving account's tier as the registry snapshot holds it on every attempt: the profile's
/// tier for an oauth account on a plan the router prices, none for an api_key account, and a tier that changes later
/// lands on the new rows alone.
#[tokio::test]
async fn the_landing_records_the_serving_accounts_tier_as_the_registry_holds_it() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(
        &up,
        &[oauth("a", A, 0), api_key("b", C, 1)],
        (0, None),
        LedgerConfig::default(),
    )
    .await;
    set_tier(&h.store, "a", "default_claude_max_20x");
    h.router.reload().await.unwrap();
    let on = |id: &str, account: &str, kind: AccountKind| Record::Request {
        request: finished_row(id),
        attempts: vec![AttemptRecord {
            account_id: account.into(),
            account_kind: kind,
            ..serving_attempt(id, true)
        }],
        bodies: None,
    };
    h.ledger
        .record_request(on("r-oauth", "a", AccountKind::Oauth))
        .await;
    h.ledger
        .record_request(on("r-key", "b", AccountKind::ApiKey))
        .await;
    let tier_of = |id: &'static str| {
        let h = &h;
        async move { ledgered(h, id).await.1[0].tier.clone() }
    };
    assert_eq!(
        (tier_of("r-oauth").await, tier_of("r-key").await),
        (Some("default_claude_max_20x".to_string()), None),
        "the oauth attempt reads its profile's tier and the api_key attempt reads none"
    );
    set_tier(&h.store, "a", "default_claude_max_5x");
    h.router.reload().await.unwrap();
    h.ledger
        .record_request(on("r-later", "a", AccountKind::Oauth))
        .await;
    assert_eq!(
        (tier_of("r-later").await, tier_of("r-oauth").await),
        (
            Some("default_claude_max_5x".to_string()),
            Some("default_claude_max_20x".to_string())
        ),
        "the new tier lands on the new row and the earlier row keeps the tier it was written with"
    );
    up.shutdown().await;
}

/// The price-key case's subscription cost model: a capacity row for every tier word its documents name, so a
/// plan-funded row that keeps its tier prices a share and only a tier the landing leaves off keeps it unpriced. The
/// seat's row files under its own funding, which prices no share whatever its tier, so its half of the shares
/// assertion is a guard and the tier assertion carries its rule.
const PRICE_KEY_COSTS: &str = "\
unit: {tier: default_claude_ai, window: seven_day}
revisions:
  - effective: 1970-01-01
    capacity:
      default_claude_ai:      {five_hour: 100, seven_day: 1000}
      default_claude_max_20x: {five_hour: 100, seven_day: 1000}
      default_claude_zero:    {five_hour: 100, seven_day: 1000}
    peak:
      zone: America/Los_Angeles
      weekdays: true
      from_hour: 5
      to_hour: 11
      capacity_factor: {five_hour: 1, seven_day: 1}
    type_multiplier: {cache_read: 0.1, cache_write_1h: 2, cache_write_5m: 2, input: 1, output: 10}
    model_weight:
      claude-sonnet-5: 1
    model_windows: {}
    overrides: {}
    tier_order: [fable, opus, sonnet, haiku]
    model_tier:
      claude-sonnet-5: sonnet
";

/// Stores `organization` as the account's profile document with its tier, as the quota poll stores a read.
fn set_document(store: &Store, account: &str, organization: &str, tier: &str) {
    store
        .set_profile(
            account,
            &format!(r#"{{"organization":{organization}}}"#),
            &phoebis_store::SubscriptionRow {
                tier: Some(tier.into()),
                ..Default::default()
            },
        )
        .unwrap();
}

/// The landing records the tier the router prices the serving account by: none for a free organization and for a
/// usage-based seat, whatever tier their documents name, so neither row prices a share of a plan window, while a Max
/// account's attempt records its tier and prices as before.
#[tokio::test]
async fn the_landing_records_no_tier_for_a_free_organization_or_a_usage_based_seat_and_neither_prices_a_share(
) {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(
        &up,
        &[
            oauth("max", A, 0),
            oauth("free", B, 1),
            oauth("seat", "sk-ant-oat01-SEAT", 2),
        ],
        (0, None),
        LedgerConfig::default(),
    )
    .await;
    set_document(
        &h.store,
        "max",
        r#"{"organization_type":"claude_max","rate_limit_tier":"default_claude_max_20x"}"#,
        "default_claude_max_20x",
    );
    set_document(
        &h.store,
        "free",
        r#"{"organization_type":"claude_free","rate_limit_tier":"default_claude_ai"}"#,
        "default_claude_ai",
    );
    set_document(
        &h.store,
        "seat",
        r#"{"organization_type":"claude_enterprise","rate_limit_tier":"default_claude_zero","seat_tier":"enterprise_usage_based"}"#,
        "default_claude_zero",
    );
    h.router.reload().await.unwrap();
    for (id, account) in [("r-max", "max"), ("r-free", "free"), ("r-seat", "seat")] {
        h.ledger
            .record_request(Record::Request {
                request: finished_row(id),
                attempts: vec![AttemptRecord {
                    account_id: account.into(),
                    account_kind: AccountKind::Oauth,
                    // The selection stamps the seat's class, as it does on a request the relay serves.
                    usage_based: account == "seat",
                    ..serving_attempt(id, true)
                }],
                bodies: None,
            })
            .await;
    }
    let mut tiers = Vec::new();
    for id in ["r-max", "r-free", "r-seat"] {
        tiers.push(ledgered(&h, id).await.1[0].tier.clone());
    }
    assert_eq!(
        tiers,
        [Some("default_claude_max_20x".to_string()), None, None],
        "the Max attempt records its tier, and the free organization's and the seat's record none"
    );
    let tables = PlanTables {
        revisions: phoebis_ledger::CostRevisions::from_yaml(PRICE_KEY_COSTS)
            .expect("the price-key cost model parses"),
        path: None,
        source: "test".into(),
    };
    let groups = breakdown_paged(
        &h.db,
        &RollupFilter::default(),
        &LiveKeys::default(),
        0,
        3_600_000,
        GroupBy::Account,
        &PriceRevisions::shipped(),
        &tables,
        NOW,
    )
    .await
    .unwrap();
    let shares = |account: &str| {
        groups
            .iter()
            .find(|g| g.key == account)
            .unwrap_or_else(|| panic!("no group of {account}: {groups:?}"))
            .cost_shares
            .is_some()
    };
    assert_eq!(
        (shares("max"), shares("free"), shares("seat")),
        (true, false, false),
        "the Max row prices a share of its windows and the free organization's prices none, the seat's none as a guard"
    );
    up.shutdown().await;
}

/// The listing cases' subscription cost model: `claude-sonnet-5` alone carries a weight, so every other model is
/// unpriced.
const LISTING_COSTS: &str = "\
unit: {tier: default_claude_ai, window: seven_day}
revisions:
  - effective: 1970-01-01
    capacity:
      default_claude_ai: {five_hour: 100, seven_day: 1000}
    peak:
      zone: America/Los_Angeles
      weekdays: true
      from_hour: 5
      to_hour: 11
      capacity_factor: {five_hour: 1, seven_day: 1}
    type_multiplier: {cache_read: 0.1, cache_write_1h: 2, cache_write_5m: 2, input: 1, output: 10}
    model_weight:
      claude-sonnet-5: 1
    model_windows: {}
    overrides: {}
    tier_order: [fable, opus, sonnet, haiku]
    model_tier:
      claude-sonnet-5: sonnet
";

/// `LISTING_COSTS` with a second entry from 01:00 UTC that weighs `claude-opus-9` and a third from 02:00 UTC that
/// weighs it no longer.
const REVISED_LISTING_COSTS: &str = "\
unit: {tier: default_claude_ai, window: seven_day}
revisions:
  - effective: 1970-01-01
    capacity:
      default_claude_ai: {five_hour: 100, seven_day: 1000}
    peak:
      zone: America/Los_Angeles
      weekdays: true
      from_hour: 5
      to_hour: 11
      capacity_factor: {five_hour: 1, seven_day: 1}
    type_multiplier: {cache_read: 0.1, cache_write_1h: 2, cache_write_5m: 2, input: 1, output: 10}
    model_weight:
      claude-sonnet-5: 1
    model_windows: {}
    overrides: {}
    tier_order: [fable, opus, sonnet, haiku]
    model_tier:
      claude-sonnet-5: sonnet
  - effective: 1970-01-01T01:00:00Z
    capacity:
      default_claude_ai: {five_hour: 100, seven_day: 1000}
    peak:
      zone: America/Los_Angeles
      weekdays: true
      from_hour: 5
      to_hour: 11
      capacity_factor: {five_hour: 1, seven_day: 1}
    type_multiplier: {cache_read: 0.1, cache_write_1h: 2, cache_write_5m: 2, input: 1, output: 10}
    model_weight:
      claude-opus-9: 1
      claude-sonnet-5: 1
    model_windows: {}
    overrides: {}
    tier_order: [fable, opus, sonnet, haiku]
    model_tier:
      claude-opus-9: opus
      claude-sonnet-5: sonnet
  - effective: 1970-01-01T02:00:00Z
    capacity:
      default_claude_ai: {five_hour: 100, seven_day: 1000}
    peak:
      zone: America/Los_Angeles
      weekdays: true
      from_hour: 5
      to_hour: 11
      capacity_factor: {five_hour: 1, seven_day: 1}
    type_multiplier: {cache_read: 0.1, cache_write_1h: 2, cache_write_5m: 2, input: 1, output: 10}
    model_weight:
      claude-sonnet-5: 1
    model_windows: {}
    overrides: {}
    tier_order: [fable, opus, sonnet, haiku]
    model_tier:
      claude-sonnet-5: sonnet
";

/// A ledger on `LISTING_COSTS` under `dir`, its pool's one member the OAuth account `a`, so a request it serves is
/// plan-funded.
async fn on_listing_costs(up: &FakeUpstream, dir: &std::path::Path) -> Harness {
    on_cost_model(up, dir, LISTING_COSTS, &[oauth("a", A, 0)]).await
}

/// A ledger on the cost model `costs` under `dir`, its pool's members `accounts`.
async fn on_cost_model(
    up: &FakeUpstream,
    dir: &std::path::Path,
    costs: &str,
    accounts: &[Account],
) -> Harness {
    harness_on_tables(
        fresh_store(),
        Arc::new(LedgerDb::open(&dir.join("ledger.sqlite")).unwrap()),
        up,
        accounts,
        (0, None),
        LedgerConfig::default(),
        false,
        phoebis_ledger::test_tables::api_prices(),
        phoebis_ledger::test_tables::tables(
            phoebis_ledger::CostRevisions::from_yaml(costs).expect("the listing cost model parses"),
        ),
    )
    .await
}

/// A catalog change of `account` at `at` that adds `model` alone.
fn adding(account: &str, model: &str, at: i64) -> phoebis_relay::CatalogChange {
    phoebis_relay::CatalogChange {
        account_id: account.into(),
        added: vec![model.into()],
        removed: Vec::new(),
        count: 1,
        at,
    }
}

/// A catalog change enters each added model the cost model does not weigh under its undated id, dated at the
/// change and with no request, and enters nothing for an added model the cost model weighs. A later change that adds
/// the same model, as a second account's listing does, keeps the first date.
#[tokio::test]
async fn a_listing_enters_an_added_model_the_cost_model_does_not_weigh_at_the_change_with_no_request(
) {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let h = on_listing_costs(&up, dir.path()).await;
    h.ledger.on_catalog_change(&phoebis_relay::CatalogChange {
        account_id: "unheld".into(),
        added: vec![
            "claude-opus-9-20260101".to_string(),
            "claude-sonnet-5-20260101".to_string(),
        ],
        removed: Vec::new(),
        count: 2,
        at: NOW + 60_000,
    });
    assert_eq!(
        h.ledger.unpriced_served(NOW + 60_000),
        [phoebis_ledger::UnpricedModel {
            model: "claude-opus-9".into(),
            since: Some(NOW + 60_000),
            requests: 0,
        }],
        "the unweighted model enters at the change's instant, and the weighted one stays out"
    );
    h.ledger
        .on_catalog_change(&adding("unheld", "claude-opus-9-20260101", NOW + 120_000));
    assert_eq!(
        h.ledger.unpriced_served(NOW + 120_000),
        [phoebis_ledger::UnpricedModel {
            model: "claude-opus-9".into(),
            since: Some(NOW + 60_000),
            requests: 0,
        }],
        "a later listing of the model keeps the first listing's date"
    );
    up.shutdown().await;
}

/// A catalog change that adds a model whose unpriced run a plan-funded landing already counts leaves the run as the
/// landing dated and counted it.
#[tokio::test]
async fn a_listing_leaves_an_unpriced_run_that_counts_requests_with_its_since_and_its_count() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let h = on_listing_costs(&up, dir.path()).await;
    let landed = phoebis_ledger::UnpricedModel {
        model: "claude-opus-9".into(),
        since: Some(NOW),
        requests: 1,
    };
    h.ledger
        .record_request(Record::Request {
            request: RequestRecord {
                model: "claude-opus-9-20260101".into(),
                canonical_model: "claude-opus-9-20260101".into(),
                ..finished_row("r1")
            },
            attempts: vec![AttemptRecord {
                account_kind: AccountKind::Oauth,
                ..serving_attempt("r1", true)
            }],
            bodies: None,
        })
        .await;
    // The landing enters its run after its commit, so the listing comes once the entry shows, never before it, where
    // the listing would date the run and the landing count it.
    wait_for(Duration::from_secs(5), || {
        (h.ledger.unpriced_served(NOW) == [landed.clone()]).then_some(())
    })
    .await;
    h.ledger
        .on_catalog_change(&adding("unheld", "claude-opus-9-20260101", NOW + 60_000));
    assert_eq!(
        h.ledger.unpriced_served(NOW + 60_000),
        [landed],
        "the listing keeps the landed run's since and count"
    );
    up.shutdown().await;
}

/// A plan account's listing ahead of the revision that weighs the model reads standing until that revision is in
/// force and cleared from then on. The read writes the clear, so a listing under a later revision that weighs the model
/// no longer dates a new run at its own instant.
#[tokio::test]
async fn a_listed_model_reads_cleared_once_a_revision_weighs_it_and_a_later_listing_dates_a_new_run(
) {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let h = on_cost_model(
        &up,
        dir.path(),
        REVISED_LISTING_COSTS,
        &[oauth("a", A, 0), oauth("b", B, 1)],
    )
    .await;
    // The second entry's and the third entry's instants.
    let (weighted_from, unweighted_from) = (3_600_000, 7_200_000);
    let entry = |since| {
        [phoebis_ledger::UnpricedModel {
            model: "claude-opus-9".into(),
            since,
            requests: 0,
        }]
    };
    // The read writes, so the reads come in rising order.
    h.ledger
        .on_catalog_change(&adding("a", "claude-opus-9-20260101", NOW));
    assert_eq!(
        h.ledger.unpriced_served(NOW),
        entry(Some(NOW)),
        "the first entry weighs nothing for the model, so the listing's entry stands"
    );
    assert_eq!(
        h.ledger.unpriced_served(weighted_from),
        entry(None),
        "the revision in force at the read weighs the model, so the entry reads cleared"
    );
    h.ledger
        .on_catalog_change(&adding("b", "claude-opus-9-20260101", unweighted_from));
    assert_eq!(
        h.ledger.unpriced_served(unweighted_from),
        entry(Some(unweighted_from)),
        "the read cleared the entry it answered, so the next listing opens a run of its own"
    );
    up.shutdown().await;
}

/// A read whose instant lies ahead of an unpriced run's opening leaves the run standing: a plan-funded landing at 02:00
/// opens the run under the third entry, which weighs `claude-opus-9` no longer, a read at 01:00, where the second entry
/// weighs it, clears nothing, and a read at the run's instant still answers its date and its count.
#[tokio::test]
async fn a_read_older_than_an_unpriced_run_leaves_the_run_its_date_and_its_count() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let h = on_cost_model(&up, dir.path(), REVISED_LISTING_COSTS, &[oauth("a", A, 0)]).await;
    // The second entry's and the third entry's instants.
    let (weighted_from, unweighted_from) = (3_600_000, 7_200_000);
    let landed = phoebis_ledger::UnpricedModel {
        model: "claude-opus-9".into(),
        since: Some(unweighted_from),
        requests: 1,
    };
    h.ledger
        .record_request(Record::Request {
            request: RequestRecord {
                received_at: unweighted_from,
                finished_at: Some(unweighted_from + 500),
                model: "claude-opus-9-20260101".into(),
                canonical_model: "claude-opus-9-20260101".into(),
                ..finished_row("r1")
            },
            attempts: vec![AttemptRecord {
                account_kind: AccountKind::Oauth,
                started_at: unweighted_from,
                ..serving_attempt("r1", true)
            }],
            bodies: None,
        })
        .await;
    // The landing enters its run after its commit.
    wait_for(Duration::from_secs(5), || {
        (h.ledger.unpriced_served(unweighted_from) == [landed.clone()]).then_some(())
    })
    .await;
    h.ledger.unpriced_served(weighted_from);
    assert_eq!(
        h.ledger.unpriced_served(unweighted_from),
        [landed],
        "the read at 01:00 lies ahead of the run, so the run keeps its date and its count"
    );
    up.shutdown().await;
}

/// A listing enters its models only where its account's requests are plan-funded: an API-key account's and a
/// usage-based seat's enter nothing, and a plan account's and the listing of an account the routing snapshot does not
/// hold yet enter theirs.
#[tokio::test]
async fn a_listing_enters_its_models_only_where_the_accounts_requests_are_plan_funded() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let h = on_cost_model(
        &up,
        dir.path(),
        LISTING_COSTS,
        &[
            api_key("direct", C, 0),
            oauth("seat", A, 1),
            oauth("plan", B, 2),
        ],
    )
    .await;
    set_tier(&h.store, "seat", "default_claude_zero");
    h.router.reload().await.unwrap();
    h.ledger
        .on_catalog_change(&adding("direct", "claude-haiku-9-20260101", NOW));
    h.ledger
        .on_catalog_change(&adding("seat", "claude-sonnet-9-20260101", NOW));
    h.ledger
        .on_catalog_change(&adding("plan", "claude-opus-9-20260101", NOW));
    h.ledger
        .on_catalog_change(&adding("unheld", "claude-fable-9-20260101", NOW));
    assert_eq!(
        h.ledger.unpriced_served(NOW),
        [
            phoebis_ledger::UnpricedModel {
                model: "claude-fable-9".into(),
                since: Some(NOW),
                requests: 0,
            },
            phoebis_ledger::UnpricedModel {
                model: "claude-opus-9".into(),
                since: Some(NOW),
                requests: 0,
            },
        ],
        "the plan account's and the unheld account's models enter, the API key's and the seat's do not"
    );
    up.shutdown().await;
}

/// A record parked by a build whose attempts carried the dollar members and the table's stamp replays at the next
/// start: the members are ignored and the landing writes the tier the registry holds for the serving account.
#[tokio::test]
async fn a_parked_record_carrying_the_dropped_dollar_members_replays_and_lands_with_the_accounts_tier(
) {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let store = fresh_store();
    let accounts = [oauth("a", A, 0)];
    let h = harness_on(
        store.clone(),
        Arc::new(LedgerDb::open(&path).unwrap()),
        &up,
        &accounts,
        (0, None),
        quick(),
        false,
    )
    .await;
    set_tier(&store, "a", "default_claude_max_20x");
    h.ledger.clone().shutdown().await.unwrap();
    let record = r#"{"request":{"id":"old","received_at":1000000,"finished_at":1000500,"key_id":"desktop","pool_id":"main","model":"claude-sonnet-5","canonical_model":"claude-sonnet-5","kind":"inference","origin":"client","outcome":"succeeded","attempts":1},"attempts":[{"request_id":"old","attempt":1,"account_id":"a","account_kind":"oauth","started_at":1000000,"outcome":"succeeded","input_tokens":1000000,"estimated_usd":2.0,"input_usd":2.0,"cache_read_usd":0.0,"cache_write_usd":0.0,"output_usd":0.0,"pricing_version":"2026-10-01"}]}"#;
    h.db.write(|c| {
        c.execute(
            "INSERT INTO parked_records (parked_at, last_tried_at, request_id, pool_id, account_id, record, error) \
             VALUES (?1, ?1, 'old', 'main', 'a', ?2, 'refused by test')",
            rusqlite::params![NOW, record],
        )?;
        Ok(())
    })
    .unwrap();
    drop(h);
    let h = harness_on(
        store,
        Arc::new(LedgerDb::open(&path).unwrap()),
        &up,
        &accounts,
        (0, None),
        quick(),
        false,
    )
    .await;
    wait_for_async(Duration::from_secs(5), || async {
        (parked_rows(&h).await == 0).then_some(())
    })
    .await;
    let (_, attempts) =
        h.db.request("old")
            .await
            .unwrap()
            .expect("the start landed the parked record");
    assert_eq!(
        (attempts[0].input_tokens, attempts[0].tier.as_deref()),
        (Some(1_000_000), Some("default_claude_max_20x")),
        "the record lands in the new shape, its counts kept and the account's tier written"
    );
    up.shutdown().await;
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
    let (db, _dir) = common::temp_ledger();
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
        account_kind: AccountKind::Oauth,
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
            account_kind: AccountKind::Oauth,
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
    let (db, _dir) = common::temp_ledger();
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
                account_kind: AccountKind::Oauth,
                outcome: "transport_error".into(),
                started_at: 3_600_000,
                selection: Some(moved_trace.into()),
                ..Default::default()
            },
            phoebis_ledger::AttemptRecord {
                request_id: "r1".into(),
                attempt: 2,
                account_id: "b".into(),
                account_kind: AccountKind::Oauth,
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
    let (db, _dir) = common::temp_ledger();
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
                account_kind: AccountKind::Oauth,
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
                account_kind: AccountKind::Oauth,
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
                from_account_id: Some("a".into()),
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
    assert_eq!(row.migrations.get("cooldown/unknown"), Some(&1));
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
            account_kind: AccountKind::ApiKey,
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
    let (db, _dir) = common::temp_ledger();
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
                &PriceRevisions::shipped(),
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
    (account, kind): (&str, AccountKind),
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
            account_kind: kind,
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

/// A million Opus 5 input tokens at the shipped standard and fast cells in force at `plant_funded`'s hour, read
/// through the loader so a revision edits no test.
fn opus_5_million() -> (f64, f64) {
    let shipped = PriceRevisions::shipped();
    let table = shipped.in_force(ms(2026, 1, 15, 1, 0));
    let input = |speed| {
        table
            .price_at("claude-opus-5", speed, 0)
            .expect("the shipped list prices Opus 5")
            .input
    };
    (input(Speed::Standard), input(Speed::Fast))
}

/// The five funding cases on one member in one hour: overage in use, the plan, and three fast answers, one with no
/// overage reading, one whose head said overage was not in use and one whose head said it was.
fn plant_every_funding(db: &phoebis_ledger::LedgerDb, member: (&str, AccountKind)) {
    plant_funded(db, member, "overage", None, Some(true));
    plant_funded(db, member, "plan", None, Some(false));
    plant_funded(db, member, "fast", Some("fast"), None);
    plant_funded(db, member, "fast-on-plan", Some("fast"), Some(false));
    plant_funded(db, member, "fast-in-overage", Some("fast"), Some(true));
}

#[test]
fn an_attempt_folds_under_fast_overage_or_plan_funding() {
    let (db, _dir) = common::temp_ledger();
    plant_every_funding(&db, ("a", AccountKind::Oauth));
    let mut rows = db
        .read_blocking(|c| {
            phoebis_ledger::read_rollups(c, 0, i64::MAX, &phoebis_ledger::RollupSelect::default())
        })
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
fn an_api_key_account_s_attempt_folds_under_billed_whatever_its_speed_or_overage_flag() {
    let (db, _dir) = common::temp_ledger();
    plant_every_funding(&db, ("b", AccountKind::ApiKey));
    let mut rows = db
        .read_blocking(|c| {
            phoebis_ledger::read_rollups(c, 0, i64::MAX, &phoebis_ledger::RollupSelect::default())
        })
        .unwrap()
        .into_iter()
        .map(|r| (r.funding, r.speed, r.requests))
        .collect::<Vec<_>>();
    rows.sort();
    assert_eq!(
        rows,
        vec![
            ("billed".to_string(), "fast".to_string(), 3),
            ("billed".to_string(), "standard".to_string(), 2),
        ],
        "an api_key account's answer is billed in dollars whatever its speed or the head's overage flag, and the key \
         says so without the registry"
    );
}

/// A selection on a usage-based seat lands its class on the attempt row, the row folds under the seat's own funding
/// word and its counts price in dollars at the API rates with no share, and the seat's group, which holds no plan row,
/// reads the Pro-week figure 0.
#[tokio::test]
async fn a_usage_based_seats_attempt_stores_its_class_folds_under_usage_based_and_prices_in_dollars_alone(
) {
    let mut assembler = Assembler::new(Arc::new(|| NOW));
    let BusEvent::RequestObserved(mut observed) = observed_event("r1") else {
        unreachable!()
    };
    observed.received_at = NOW;
    assembler.observe(BusEvent::RequestObserved(observed));
    let BusEvent::AccountSelected(mut selected) = selected_event("r1", 1, "seat", minutes(1))
    else {
        unreachable!()
    };
    selected.usage_based = true;
    assembler.observe(BusEvent::AccountSelected(selected));
    let BusEvent::RequestComplete(mut complete) = complete_event("r1") else {
        unreachable!()
    };
    complete.attempt = 1;
    complete.account_id = Some("seat".into());
    complete.usage = Some(Usage {
        input_tokens: Some(1_000_000),
        ..Usage::default()
    });
    let records = assembler.observe(BusEvent::RequestComplete(complete));
    let [Record::Request {
        request, attempts, ..
    }] = records.as_slice()
    else {
        panic!("{} records at completion", records.len());
    };
    let (db, _dir) = common::temp_ledger();
    let db = Arc::new(db);
    db.insert_request(request, attempts, None).unwrap();
    let stored = db
        .read_blocking(|c| phoebis_ledger::request_by_id(c, "r1"))
        .unwrap()
        .expect("the request landed")
        .1;
    assert_eq!(
        stored.iter().map(|a| a.usage_based).collect::<Vec<_>>(),
        [true],
        "the attempt row stores the class its selection carried"
    );
    let rows = db
        .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
        .unwrap();
    assert_eq!(
        rows.iter().map(|r| r.funding.as_str()).collect::<Vec<_>>(),
        [phoebis_ledger::USAGE_BASED_FUNDING],
        "the row files under the seat's own funding word"
    );
    // A flat dollar a token on the test's own list, so the figure is the count.
    let rate = 1_000_000.0;
    let prices = PriceRevisions::new(
        vec![phoebis_ledger::PriceTable {
            effective: "1970-01-01".into(),
            models: [(
                "claude-sonnet-5".to_string(),
                phoebis_ledger::ModelRow {
                    standard: phoebis_ledger::ModelPrice {
                        input: rate,
                        cache_write_5m: rate,
                        cache_write_1h: rate,
                        cache_read: rate,
                        output: rate,
                    },
                    fast: None,
                    long_context: None,
                },
            )]
            .into_iter()
            .collect(),
        }],
        "test",
    )
    .expect("one entry");
    let groups = breakdown_paged(
        &db,
        &RollupFilter::default(),
        &LiveKeys::default(),
        0,
        3_600_000,
        GroupBy::Account,
        &prices,
        &PlanTables::shipped(),
        NOW,
    )
    .await
    .unwrap();
    let seat = groups
        .iter()
        .find(|g| g.key == "seat")
        .unwrap_or_else(|| panic!("no group of the seat: {groups:?}"));
    // The group holds no plan row, so its Pro-week figure reads 0.
    assert_eq!(
        (seat.cost_usd, &seat.cost_shares, seat.cost_equivalent),
        (1_000_000.0, &None, Some(0.0)),
        "the seat's counts read dollars alone: {seat:?}"
    );
}

#[test]
fn member_spends_sums_the_metered_rows_alone_when_asked() {
    let (db, _dir) = common::temp_ledger();
    plant_every_funding(&db, ("a", AccountKind::Oauth));
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
                &PriceRevisions::shipped(),
            )
        })
        .unwrap()[0]
            .spent_usd
    };
    let (standard, fast) = opus_5_million();
    let close = |got: f64, want: f64| (got - want).abs() < 1e-9;
    assert!(
        close(spent(false), 2.0 * standard + 3.0 * fast),
        "every row: two million standard input tokens and three million fast"
    );
    assert!(
        close(spent(true), standard + 3.0 * fast),
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
    plant_every_funding(&h.db, ("a", AccountKind::Oauth));
    plant_every_funding(&h.db, ("b", AccountKind::ApiKey));
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
    let (standard, fast) = opus_5_million();
    let close = |got: f64, want: f64| (got - want).abs() < 1e-9;
    assert!(
        close(spent.0, standard + 3.0 * fast) && close(spent.1, 2.0 * standard + 3.0 * fast),
        "the subscription member's month reads its overage row and its fast answers and never its plan row; the \
         API-key member's reads every row: {spent:?}"
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
            account_kind: AccountKind::Oauth,
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
        // The writer re-arms its boundary tick from the clock on every wake, and a stepped clock reaches it only
        // through one; the revision poll at this cadence is the wake the test can count on.
        LedgerConfig {
            event_poll: Duration::from_millis(20),
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
            account_kind: AccountKind::Oauth,
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

/// A trigger that refuses every insert into `requests`, or only the one whose id is `only`.
fn refuse_requests(db: &LedgerDb, only: Option<&str>) {
    let when = only.map_or(String::new(), |id| format!("WHEN NEW.id = '{id}'"));
    db.write(|c| {
        c.execute_batch(&format!(
            "CREATE TRIGGER refuse BEFORE INSERT ON requests {when} BEGIN SELECT RAISE(ABORT, 'refused by test'); END;"
        ))?;
        Ok(())
    })
    .unwrap();
}

/// A trigger that refuses every insert into `parked_records`.
fn refuse_parking(db: &LedgerDb) {
    db.write(|c| {
        c.execute_batch(
            "CREATE TRIGGER refuse_park BEFORE INSERT ON parked_records BEGIN SELECT RAISE(ABORT, 'parking refused by test'); END;",
        )?;
        Ok(())
    })
    .unwrap();
}

/// A trigger that refuses every delete from `parked_records`.
fn refuse_parked_deletes(db: &LedgerDb) {
    db.write(|c| {
        c.execute_batch(
            "CREATE TRIGGER refuse_ack BEFORE DELETE ON parked_records BEGIN SELECT RAISE(ABORT, 'acknowledgement refused by test'); END;",
        )?;
        Ok(())
    })
    .unwrap();
}

fn drop_trigger(db: &LedgerDb, name: &str) {
    db.write(|c| {
        c.execute_batch(&format!("DROP TRIGGER {name};"))?;
        Ok(())
    })
    .unwrap();
}

/// A finished request of pool `main` on account `a`.
fn finished_row(id: &str) -> RequestRecord {
    RequestRecord {
        id: id.into(),
        received_at: NOW,
        finished_at: Some(NOW + 500),
        key_id: Some("desktop".into()),
        pool_id: Some("main".into()),
        canonical_model: "claude-sonnet-5".into(),
        model: "claude-sonnet-5".into(),
        kind: "inference".into(),
        origin: "client".into(),
        outcome: "succeeded".into(),
        attempts: 1,
        ..Default::default()
    }
}

/// The attempt that served `id`; a priced one carries a million input tokens.
fn serving_attempt(id: &str, priced: bool) -> AttemptRecord {
    AttemptRecord {
        request_id: id.into(),
        attempt: 1,
        account_id: "a".into(),
        account_kind: AccountKind::ApiKey,
        started_at: NOW,
        outcome: "succeeded".into(),
        input_tokens: priced.then_some(1_000_000),
        ..Default::default()
    }
}

fn finished(id: &str, priced: bool) -> Record {
    Record::Request {
        request: finished_row(id),
        attempts: vec![serving_attempt(id, priced)],
        bodies: None,
    }
}

/// A harness over an on-disk ledger at `path`, its pool's one member the API-key account `a`, whose whole spend
/// counts toward a budget.
async fn on_disk(
    up: &FakeUpstream,
    store: Arc<Store>,
    path: &std::path::Path,
    config: LedgerConfig,
) -> Harness {
    harness_on(
        store,
        Arc::new(LedgerDb::open(path).unwrap()),
        up,
        &[api_key("a", C, 0)],
        (0, None),
        config,
        false,
    )
    .await
}

fn fresh_store() -> Arc<Store> {
    Arc::new(Store::open_in_memory(&StorageKey::generate(), wall()).unwrap())
}

/// A short revision poll, so the writer reads a budget edit within the test's waits.
fn quick() -> LedgerConfig {
    LedgerConfig {
        event_poll: Duration::from_millis(100),
        ..LedgerConfig::default()
    }
}

/// Puts a monthly budget on member `a` and waits for the ledger's first report of its spend, so a later charge is
/// read by the refresh the test means to pin, and for the mirror's record of the edit, so no event of it reaches the
/// writer behind the batch the test means to refuse.
async fn budget_member_a(h: &Harness) {
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
    wait_for_async(Duration::from_secs(5), || async {
        h.router
            .member_spends()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.pool_id == "main" && r.account_id == "a")
    })
    .await;
    wait_for_async(Duration::from_secs(5), || async {
        h.db.read(|c| phoebis_ledger::recent_events_of_kind(c, "registry_changed", 5))
            .await
            .unwrap()
            .into_iter()
            .find(|e| {
                e.detail
                    .as_deref()
                    .is_some_and(|d| d.contains("pool_members_set"))
            })
    })
    .await;
}

async fn spent_by_a(h: &Harness) -> f64 {
    h.router
        .member_spends()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.pool_id == "main" && r.account_id == "a")
        .map_or(0.0, |r| r.spent_usd)
}

fn write_retries(h: &Harness) -> u64 {
    h.ledger.stats().write_retries.load(Ordering::SeqCst)
}

fn retrying(h: &Harness) -> u64 {
    h.ledger.stats().retrying.load(Ordering::SeqCst)
}

fn parked_since_start(h: &Harness) -> u64 {
    h.ledger.stats().parked_priced.load(Ordering::SeqCst)
}

async fn parked_rows(h: &Harness) -> u64 {
    h.db.read(parked_count).await.unwrap()
}

fn step(h: &Harness, by: Duration) {
    h.clock.fetch_add(by.as_millis() as i64, Ordering::SeqCst);
}

/// Steps the ledger's clock by `by` until `done` holds. The writer reads the clock on a 250 ms tick, so each step
/// gets a second to be seen; a step that lands before the writer took its next deadline is answered by the next one.
async fn step_until(h: &Harness, by: Duration, mut done: impl FnMut() -> bool) {
    for _ in 0..10 {
        step(h, by);
        let until = std::time::Instant::now() + Duration::from_secs(1);
        while std::time::Instant::now() < until {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    panic!("the writer never reached the expected state");
}

/// Waits for the batch's first failure, then steps the clock through each in-place retry once the one before it
/// has failed, so the replay one record at a time runs after the last.
async fn through_the_retries(h: &Harness) {
    wait_for(Duration::from_secs(5), || {
        (write_retries(h) == 1 && retrying(h) == 1).then_some(())
    })
    .await;
    for (n, delay) in BATCH_RETRY_DELAYS.into_iter().enumerate() {
        step_until(h, delay, || write_retries(h) >= n as u64 + 2).await;
    }
}

#[tokio::test]
async fn a_priced_batch_refused_once_lands_on_its_retry_and_the_member_spend_reads_it() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let h = on_disk(
        &up,
        fresh_store(),
        &dir.path().join("ledger.sqlite"),
        quick(),
    )
    .await;
    budget_member_a(&h).await;
    refuse_requests(&h.db, None);
    h.ledger.record_request(finished("r1", true)).await;
    wait_for(Duration::from_secs(5), || {
        (h.ledger.degraded() && retrying(&h) == 1).then_some(())
    })
    .await;
    assert!(
        h.router.accounting_degraded().await.unwrap(),
        "the router learns the ledger is degraded"
    );
    assert_eq!(spent_by_a(&h).await, 0.0, "the refused charge is not read");

    drop_trigger(&h.db, "refuse");
    step(&h, BATCH_RETRY_DELAYS[0]);
    let spent = wait_for_async(Duration::from_secs(5), || async {
        let spent = spent_by_a(&h).await;
        (spent > 0.0).then_some(spent)
    })
    .await;
    assert!(
        spent > 0.0,
        "the retried batch's charge reaches the member's spend"
    );
    wait_for_async(Duration::from_secs(5), || async {
        (!h.ledger.degraded() && !h.router.accounting_degraded().await.unwrap()).then_some(())
    })
    .await;
    assert_eq!(retrying(&h), 0);
    assert_eq!(
        (
            write_retries(&h),
            h.ledger.stats().write_failures.load(Ordering::SeqCst)
        ),
        (1, 0),
        "the one refused try counts as a retry, and a batch that landed on it lost nothing"
    );
    assert_eq!(h.ledger.dropped(), 0, "nothing was dropped");
    assert_eq!(h.ledger.parked_priced(), 0);
    assert_eq!(parked_rows(&h).await, 0);
    assert!(h.db.request("r1").await.unwrap().is_some());
    up.shutdown().await;
}

#[tokio::test]
async fn a_priced_record_that_poisons_its_batch_is_parked_and_lands_at_the_next_start() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let store = fresh_store();
    let h = on_disk(&up, store.clone(), &path, quick()).await;
    refuse_requests(&h.db, Some("poison"));
    for id in ["r1", "poison", "r3"] {
        h.ledger.record_request(finished(id, true)).await;
    }
    through_the_retries(&h).await;
    wait_for(Duration::from_secs(5), || {
        (parked_since_start(&h) == 1).then_some(())
    })
    .await;
    let parked_at = h.clock.load(Ordering::SeqCst);
    for id in ["r1", "r3"] {
        let landed = wait_for_async(Duration::from_secs(5), || async {
            h.db.request(id).await.unwrap()
        })
        .await;
        assert_eq!(
            landed.1[0].input_tokens,
            Some(1_000_000),
            "{id} landed whole"
        );
    }
    let rows = h
        .db
        .read(|c| {
            let mut stmt = c.prepare(
                "SELECT request_id, pool_id, account_id, error, parked_at, last_tried_at FROM parked_records",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, i64>(5)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "one parked row: {rows:?}");
    let (request_id, pool_id, account_id, error, at, tried) = &rows[0];
    assert_eq!(
        (
            request_id.as_str(),
            pool_id.as_deref(),
            account_id.as_deref()
        ),
        ("poison", Some("main"), Some("a"))
    );
    assert!(
        error.contains("refused by test"),
        "the row keeps the error that refused it: {error}"
    );
    assert_eq!((*at, *tried), (parked_at, parked_at));
    assert_eq!(
        h.db.read(parked_summary).await.unwrap(),
        vec![ParkedSummary {
            pool_id: Some("main".into()),
            account_ids: vec!["a".into()],
            count: 1,
            oldest_at: parked_at,
        }]
    );
    assert_eq!(
        h.ledger.dropped(),
        0,
        "a priced record is parked rather than dropped"
    );
    let gaps =
        h.db.events(0, i64::MAX, 100)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "ledger_gap")
            .collect::<Vec<_>>();
    assert!(gaps.is_empty(), "a parked record leaves no gap: {gaps:?}");

    h.ledger.clone().shutdown().await.unwrap();
    drop_trigger(&h.db, "refuse");
    drop(h);
    let h = on_disk(&up, store, &path, quick()).await;
    wait_for_async(Duration::from_secs(5), || async {
        (parked_rows(&h).await == 0).then_some(())
    })
    .await;
    let (_, attempts) =
        h.db.request("poison")
            .await
            .unwrap()
            .expect("the start landed the parked record");
    assert_eq!(
        attempts[0].input_tokens,
        Some(1_000_000),
        "with its priced attempt"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn an_unpriced_record_that_poisons_its_batch_is_dropped_with_a_gap_naming_it() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let h = on_disk(
        &up,
        fresh_store(),
        &dir.path().join("ledger.sqlite"),
        quick(),
    )
    .await;
    refuse_requests(&h.db, Some("poison"));
    h.ledger.record_request(finished("poison", false)).await;
    through_the_retries(&h).await;
    let gap = wait_for_async(Duration::from_secs(5), || async {
        h.db.events(0, i64::MAX, 100)
            .await
            .unwrap()
            .into_iter()
            .find(|e| {
                e.kind == "ledger_gap" && e.detail.as_deref().is_some_and(|d| d.contains("poison"))
            })
    })
    .await;
    assert_eq!(
        gap.detail.as_deref(),
        Some(r#"{"request_id":"poison","reason":"poison"}"#)
    );
    assert_eq!(h.ledger.dropped(), 1, "the record is dropped and counted");
    assert_eq!(h.ledger.parked_priced(), 0);
    assert_eq!(
        parked_rows(&h).await,
        0,
        "a record without money is never parked"
    );
    // No member carries a budget, so there is no spend to read and the ledger is current once nothing is held.
    wait_for(Duration::from_secs(5), || {
        (!h.ledger.degraded() && retrying(&h) == 0).then_some(())
    })
    .await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_full_disk_holds_the_batch_and_recovers_with_the_spend_refreshed() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let h = on_disk(
        &up,
        fresh_store(),
        &dir.path().join("ledger.sqlite"),
        quick(),
    )
    .await;
    budget_member_a(&h).await;
    h.db.fill_the_disk();
    // A user agent that spans many pages, so the row needs pages the cap refuses.
    let wide = "x".repeat(512 << 10);
    let refused =
        h.db.write(|c| {
            c.execute(
                "INSERT INTO meta (key, value) VALUES ('probe', ?1)",
                [&wide],
            )?;
            Ok(())
        })
        .unwrap_err();
    assert!(
        matches!(
            &refused,
            LedgerError::Sqlite(rusqlite::Error::SqliteFailure(f, _))
                if f.code == rusqlite::ErrorCode::DiskFull
        ),
        "the cap answers SQLITE_FULL: {refused:?}"
    );
    assert!(
        refused.is_database_wide(),
        "a full disk is the database's failure"
    );
    h.ledger
        .record_request(Record::Request {
            request: RequestRecord {
                user_agent: Some(wide),
                ..finished_row("r1")
            },
            attempts: vec![serving_attempt("r1", true)],
            bodies: None,
        })
        .await;
    through_the_retries(&h).await;
    // The replay met the full disk on its first record, so the batch is held whole.
    wait_for(Duration::from_secs(5), || {
        (write_retries(&h) == 6).then_some(())
    })
    .await;
    step(&h, BATCH_RETRY_HOLD - Duration::from_secs(1));
    stays_for(Duration::from_millis(400), || write_retries(&h) == 6).await;
    step(&h, Duration::from_secs(1));
    wait_for(Duration::from_secs(5), || {
        (write_retries(&h) == 7).then_some(())
    })
    .await;
    assert!(
        h.ledger.degraded(),
        "the ledger stays degraded while the batch is held"
    );
    assert!(h.router.accounting_degraded().await.unwrap());
    assert_eq!(retrying(&h), 1);
    assert_eq!(h.ledger.dropped(), 0, "a held batch is never dropped");
    assert_eq!(parked_rows(&h).await, 0, "a full disk parks nothing");
    let gauges = phoebis_ledger::gather(
        &h.router,
        PriceRevisions::shipped().in_force(NOW),
        NOW,
        60_000,
        None,
        AttentionGauges::default(),
        Some(&h.ledger),
    )
    .await;
    assert!(
        gauges.ledger_degraded,
        "the scrape reads the degraded state"
    );

    h.db.free_the_disk();
    step(&h, BATCH_RETRY_HOLD);
    wait_for_async(Duration::from_secs(5), || async {
        (!h.ledger.degraded() && !h.router.accounting_degraded().await.unwrap()).then_some(())
    })
    .await;
    assert!(
        spent_by_a(&h).await > 0.0,
        "the held charge reaches the member's spend"
    );
    assert_eq!((retrying(&h), h.ledger.dropped()), (0, 0));
    assert!(h.db.request("r1").await.unwrap().is_some());
    up.shutdown().await;
}

#[tokio::test]
async fn a_held_batch_lands_on_the_tier_its_first_try_read_after_a_reload_moved_it() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let h = harness_on(
        fresh_store(),
        Arc::new(LedgerDb::open(&dir.path().join("ledger.sqlite")).unwrap()),
        &up,
        &[oauth("a", A, 0)],
        (0, None),
        quick(),
        false,
    )
    .await;
    set_tier(&h.store, "a", "default_claude_max_20x");
    h.router.reload().await.unwrap();
    h.db.fill_the_disk();
    // A user agent that spans many pages, so the row needs pages the cap refuses.
    h.ledger
        .record_request(Record::Request {
            request: RequestRecord {
                user_agent: Some("x".repeat(512 << 10)),
                ..finished_row("r1")
            },
            attempts: vec![AttemptRecord {
                account_kind: AccountKind::Oauth,
                ..serving_attempt("r1", true)
            }],
            bodies: None,
        })
        .await;
    wait_for(Duration::from_secs(5), || {
        (write_retries(&h) == 1).then_some(())
    })
    .await;
    set_tier(&h.store, "a", "default_claude_max_5x");
    h.router.reload().await.unwrap();
    through_the_retries(&h).await;
    // The replay after the last in-place retry met the full disk too, so the batch waits out the hold.
    wait_for(Duration::from_secs(5), || {
        (write_retries(&h) == 6).then_some(())
    })
    .await;
    h.db.free_the_disk();
    step(&h, BATCH_RETRY_HOLD);
    assert_eq!(
        ledgered(&h, "r1").await.1[0].tier.as_deref(),
        Some("default_claude_max_20x"),
        "the hold's replay stamps the tier the batch's first try read"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_parking_insert_the_database_refuses_keeps_the_record_held() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let h = on_disk(
        &up,
        fresh_store(),
        &dir.path().join("ledger.sqlite"),
        quick(),
    )
    .await;
    budget_member_a(&h).await;
    refuse_requests(&h.db, Some("poison"));
    refuse_parking(&h.db);
    h.ledger.record_request(finished("poison", true)).await;
    through_the_retries(&h).await;
    wait_for(Duration::from_secs(5), || {
        (write_retries(&h) == 6).then_some(())
    })
    .await;
    // The park was refused, so the record waits in the hold and is tried again after it.
    step_until(&h, BATCH_RETRY_HOLD, || write_retries(&h) == 7).await;
    assert_eq!(h.ledger.dropped(), 0, "the record is not dropped");
    assert_eq!(parked_rows(&h).await, 0, "nor parked");
    assert_eq!(h.ledger.parked_priced(), 0);
    assert!(h.ledger.degraded() && retrying(&h) == 1);

    drop_trigger(&h.db, "refuse_park");
    step_until(&h, BATCH_RETRY_HOLD, || parked_since_start(&h) == 1).await;
    wait_for(Duration::from_secs(5), || {
        (!h.ledger.degraded()).then_some(())
    })
    .await;
    assert_eq!(parked_rows(&h).await, 1);
    assert_eq!((retrying(&h), h.ledger.dropped()), (0, 0));
    up.shutdown().await;
}

#[tokio::test]
async fn a_parked_record_keeps_its_money_and_not_its_capture() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let store = fresh_store();
    let h = on_disk(&up, store.clone(), &path, quick()).await;
    refuse_requests(&h.db, Some("poison"));
    h.ledger
        .record_request(Record::Request {
            request: RequestRecord {
                bodies_captured: true,
                ..finished_row("poison")
            },
            attempts: vec![serving_attempt("poison", true)],
            bodies: Some(BodyRecord {
                request_id: "poison".into(),
                request_body: b"{\"messages\":[]}".to_vec(),
                response_body: Some(b"hi".to_vec()),
                request_len_seen: 15,
                response_len_seen: 2,
                expires_at: NOW + 3_600_000,
            }),
        })
        .await;
    through_the_retries(&h).await;
    wait_for(Duration::from_secs(5), || {
        (parked_since_start(&h) == 1).then_some(())
    })
    .await;
    let record: String =
        h.db.read(|c| Ok(c.query_row("SELECT record FROM parked_records", [], |r| r.get(0))?))
            .await
            .unwrap();
    let parked: serde_json::Value = serde_json::from_str(&record).unwrap();
    let members = parked.as_object().unwrap();
    assert!(
        members.contains_key("request") && members.contains_key("attempts"),
        "the request row and its attempts are kept: {record}"
    );
    assert!(
        !members.contains_key("bodies"),
        "the capture has no member at all: {record}"
    );
    assert_eq!(parked["attempts"][0]["input_tokens"], 1_000_000);

    h.ledger.clone().shutdown().await.unwrap();
    drop_trigger(&h.db, "refuse");
    drop(h);
    let h = on_disk(&up, store, &path, quick()).await;
    wait_for_async(Duration::from_secs(5), || async {
        (parked_rows(&h).await == 0).then_some(())
    })
    .await;
    let (request, attempts) =
        h.db.request("poison")
            .await
            .unwrap()
            .expect("the start landed the record");
    assert_eq!(
        attempts[0].input_tokens,
        Some(1_000_000),
        "its money is kept"
    );
    assert!(
        h.db.bodies("poison", h.clock.load(Ordering::SeqCst))
            .await
            .unwrap()
            .is_none(),
        "its capture is gone"
    );
    assert!(
        !request.bodies_captured,
        "the row claims no capture, so the drawer does not read the gone bodies as aged out"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_landing_batch_whose_spend_read_fails_keeps_the_degradation() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let event_poll = Duration::from_secs(1);
    let h = on_disk(
        &up,
        fresh_store(),
        &dir.path().join("ledger.sqlite"),
        LedgerConfig {
            event_poll,
            ..LedgerConfig::default()
        },
    )
    .await;
    budget_member_a(&h).await;
    refuse_requests(&h.db, None);
    h.ledger.record_request(finished("r1", true)).await;
    wait_for(Duration::from_secs(5), || {
        (write_retries(&h) == 1 && h.ledger.degraded()).then_some(())
    })
    .await;
    // Armed before the retry lands, for the spend read the landed batch runs.
    h.db.debug_fail_next_writer_read(WriterRead::MemberSpends);
    drop_trigger(&h.db, "refuse");
    step(&h, BATCH_RETRY_DELAYS[0]);
    wait_for(Duration::from_secs(5), || (retrying(&h) == 0).then_some(())).await;
    stays_for_async(Duration::from_millis(400), || async {
        h.ledger.degraded() && spent_by_a(&h).await == 0.0
    })
    .await;
    assert!(
        h.router.accounting_degraded().await.unwrap(),
        "the landed batch's spend was never read, so the router keeps treating it as unknown"
    );
    // The failed read is retried one poll later, not at the month's end.
    step(&h, event_poll - Duration::from_millis(1));
    stays_for(Duration::from_millis(400), || h.ledger.degraded()).await;
    step(&h, Duration::from_millis(1));
    wait_for_async(Duration::from_secs(5), || async {
        (!h.ledger.degraded() && !h.router.accounting_degraded().await.unwrap()).then_some(())
    })
    .await;
    assert!(
        spent_by_a(&h).await > 0.0,
        "the retried read reports the charge"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_hourly_purge_refreshes_the_planner_statistics_and_moves_the_readers_generation() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let config = LedgerConfig {
        purge_interval: Duration::from_millis(200),
        ..quick()
    };
    // Read before the harness spawns the purge task, whose first pass would race the read.
    let db = Arc::new(LedgerDb::open(&dir.path().join("ledger.sqlite")).unwrap());
    assert_eq!(db.debug_statistics_generation(), 0);
    let h = harness_on(
        fresh_store(),
        db,
        &up,
        &[api_key("a", C, 0)],
        (0, None),
        config,
        false,
    )
    .await;
    h.ledger.record_request(finished("r1", true)).await;
    // The open found the requests table empty and gave it no `sqlite_stat1` row, so the first pass after the landing
    // finds an index without statistics on a table that holds a row, analyzes it, and the generation moves.
    wait_for(Duration::from_secs(5), || {
        (h.db.debug_statistics_generation() >= 1).then_some(())
    })
    .await;
    up.shutdown().await;
}

/// The key `harbor` in the harness's pool `main`, answering the `created_at` it stamped, which is the generation of its
/// stored sets.
fn harbor_key(h: &Harness) -> i64 {
    let desktop = h.store.api_key("desktop").unwrap().unwrap();
    h.store
        .insert_api_key(
            &phoebis_store::ApiKeyRow {
                id: "harbor".into(),
                label: "harbor".into(),
                secret_digest: h.store.api_key_digest("phoebis-harbor").to_vec(),
                prefix: phoebis_store::key_prefix("phoebis-harbor"),
                ..desktop
            },
            "test",
        )
        .unwrap()
        .created_at
}

/// Writes one stored set of `session_id` under the key `harbor`, stamped `seen_at`, in a batch with no session row.
fn store_harbor_set(
    h: &Harness,
    generation: i64,
    (session_id, kind, agent_id): (&str, FactsKind, &str),
    value: &str,
    seen_at: i64,
) {
    h.store
        .save_engine_state(&EngineBatch {
            facts: vec![FactsRow {
                key: FactsKey {
                    key_id: "harbor".into(),
                    key_generation: generation,
                    session_id: session_id.into(),
                    kind,
                    agent_id: agent_id.into(),
                },
                record_version: 1,
                value: value.as_bytes().to_vec(),
                seen_at,
            }],
            ..EngineBatch::default()
        })
        .unwrap();
}

/// Each stored set of `session_id` under the key `harbor` as `(kind, agent, value)`, in the order of the read.
fn harbor_sets(h: &Harness, generation: i64, session_id: &str) -> Vec<(FactsKind, String, String)> {
    h.store
        .load_companion_facts(&[("harbor".into(), generation, session_id.into())])
        .unwrap()
        .into_iter()
        .map(|set| {
            let value = String::from_utf8(set.value.expect("the set opens")).unwrap();
            (set.key.kind, set.key.agent_id, value)
        })
        .collect()
}

const DAY_MS: i64 = 86_400_000;

#[tokio::test]
async fn a_pass_under_no_request_retention_cuts_an_expired_title_and_keeps_an_old_agents_set() {
    let up = FakeUpstream::start(vec![]).await;
    let config = LedgerConfig {
        retention_days: 0,
        purge_interval: Duration::from_millis(200),
        ..quick()
    };
    let h = harness(&up, &[api_key("a", C, 0)], (0, None), config).await;
    let generation = harbor_key(&h);
    let agent = r#"{"name":"dock-warden"}"#;
    store_harbor_set(
        &h,
        generation,
        ("s", FactsKind::Agents, "a1"),
        agent,
        NOW - 400 * DAY_MS,
    );
    store_harbor_set(
        &h,
        generation,
        ("s", FactsKind::Title, ""),
        r#"{"title":"Scrape the dry dock"}"#,
        NOW - 400 * DAY_MS,
    );
    wait_for(Duration::from_secs(5), || {
        (harbor_sets(&h, generation, "s")
            == vec![(FactsKind::Agents, "a1".to_string(), agent.to_string())])
        .then_some(())
    })
    .await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_pass_keeps_a_title_whose_stamp_lies_inside_the_keep_period() {
    let up = FakeUpstream::start(vec![]).await;
    let config = LedgerConfig {
        purge_interval: Duration::from_millis(200),
        session_facts_kept_for: Duration::from_secs(360),
        ..quick()
    };
    let h = harness(&up, &[api_key("a", C, 0)], (0, None), config).await;
    let generation = harbor_key(&h);
    let live = r#"{"title":"Dredge the turning basin"}"#;
    store_harbor_set(
        &h,
        generation,
        ("s1", FactsKind::Title, ""),
        live,
        NOW - 90_000,
    );
    store_harbor_set(
        &h,
        generation,
        ("s2", FactsKind::Title, ""),
        r#"{"title":"Relight the leading marks"}"#,
        NOW - 86_400_000,
    );
    // One statement cuts both titles, so the first one's answer is settled once the second is gone.
    wait_for(Duration::from_secs(5), || {
        harbor_sets(&h, generation, "s2").is_empty().then_some(())
    })
    .await;
    assert_eq!(
        harbor_sets(&h, generation, "s1"),
        vec![(FactsKind::Title, String::new(), live.to_string())],
        "a title whose stamp lies inside the keep period stays"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_pass_under_a_keep_period_that_no_cutoff_can_hold_cuts_no_title() {
    let up = FakeUpstream::start(vec![]).await;
    let config = LedgerConfig {
        purge_interval: Duration::from_millis(200),
        session_facts_kept_for: Duration::from_secs(u64::MAX),
        ..quick()
    };
    let h = harness(&up, &[api_key("a", C, 0)], (0, None), config).await;
    let generation = harbor_key(&h);
    let title = r#"{"title":"Paint the bollards"}"#;
    store_harbor_set(
        &h,
        generation,
        ("s1", FactsKind::Title, ""),
        title,
        NOW - 3_600_000,
    );
    store_harbor_set(
        &h,
        generation,
        ("s2", FactsKind::Agents, "a1"),
        r#"{"name":"fender-hand"}"#,
        NOW - 400 * DAY_MS,
    );
    wait_for(Duration::from_secs(5), || {
        harbor_sets(&h, generation, "s2").is_empty().then_some(())
    })
    .await;
    // The second agent's set leaves in a later pass, so the pass that took the first one has run both of its cuts.
    store_harbor_set(
        &h,
        generation,
        ("s2", FactsKind::Agents, "a2"),
        r#"{"name":"bollard-painter"}"#,
        NOW - 400 * DAY_MS,
    );
    wait_for(Duration::from_secs(5), || {
        harbor_sets(&h, generation, "s2").is_empty().then_some(())
    })
    .await;
    assert_eq!(
        harbor_sets(&h, generation, "s1"),
        vec![(FactsKind::Title, String::new(), title.to_string())],
        "a keep period past what an instant can hold keeps the title"
    );
    up.shutdown().await;
}

/// Parks the priced record `finished(id, true)` straight into the file at `path`, as a writer that met a poison row
/// would have, with `error` and `at` as its last try.
fn park_in_file(path: &std::path::Path, id: &str, error: &str, at: i64) {
    park_in(&LedgerDb::open(path).unwrap(), id, error, at);
}

/// A parked priced record of member `a` in `db`, one the writer lands once nothing refuses it.
fn park_in(db: &LedgerDb, id: &str, error: &str, at: i64) {
    let record = serde_json::to_string(&phoebis_ledger::ParkedRecord {
        request: finished_row(id),
        attempts: vec![serving_attempt(id, true)],
    })
    .unwrap();
    db.write(|c| {
        c.execute(
            "INSERT INTO parked_records (parked_at, last_tried_at, request_id, pool_id, account_id, record, error) \
             VALUES (?1, ?1, ?2, 'main', 'a', ?3, ?4)",
            rusqlite::params![at, id, record, error],
        )?;
        Ok(())
    })
    .unwrap();
}

/// A stop while a batch waits out its hold gives the batch the drain's one try; what the database still refuses then
/// is lost and counted, the one thing a stop may lose.
#[tokio::test]
async fn a_stop_during_a_hold_tries_the_batch_once_more_and_counts_what_is_still_refused() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let h = on_disk(
        &up,
        fresh_store(),
        &dir.path().join("ledger.sqlite"),
        quick(),
    )
    .await;
    h.db.fill_the_disk();
    h.ledger
        .record_request(Record::Request {
            request: RequestRecord {
                user_agent: Some("x".repeat(512 << 10)),
                ..finished_row("r1")
            },
            attempts: vec![serving_attempt("r1", true)],
            bodies: None,
        })
        .await;
    through_the_retries(&h).await;
    wait_for(Duration::from_secs(5), || {
        (write_retries(&h) == 6).then_some(())
    })
    .await;
    assert_eq!(h.ledger.dropped(), 0, "the held batch waits out its hold");
    h.ledger.clone().shutdown().await.unwrap();
    assert!(
        write_retries(&h) >= 7,
        "the stop gave the batch one more try"
    );
    assert_eq!(
        h.ledger.dropped(),
        1,
        "the record the full disk still refused is lost and counted"
    );
    assert_eq!(retrying(&h), 0);
    up.shutdown().await;
}

/// A parked record the database still refuses at the start keeps its row, with the error it met this time and the
/// start's instant as its last try.
#[tokio::test]
async fn a_parked_record_refused_again_at_the_start_keeps_its_row_with_this_starts_error() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    park_in_file(&path, "poison", "an earlier error", 0);
    refuse_requests(&LedgerDb::open(&path).unwrap(), Some("poison"));
    let h = on_disk(&up, fresh_store(), &path, quick()).await;
    let row = wait_for_async(Duration::from_secs(5), || async {
        h.db.read(|c| {
            Ok(c.query_row(
                "SELECT error, last_tried_at, parked_at FROM parked_records",
                [],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                },
            )?)
        })
        .await
        .ok()
        .filter(|(error, _, _)| error != "an earlier error")
    })
    .await;
    assert!(row.0.contains("refused by test"), "{row:?}");
    assert_eq!(
        (row.1, row.2),
        (NOW, 0),
        "tried at the start, parked when it was"
    );
    assert_eq!(parked_rows(&h).await, 1);
    assert!(h.db.request("poison").await.unwrap().is_none());
    up.shutdown().await;
}

/// The parked row leaves in the transaction that lands its record. A start whose acknowledgement the database refuses
/// lands nothing, so the record stays parked past a retention pass that would have cut its raw rows, and the start that
/// lands it charges it once. The pass runs on a configuration of the case's own, 90 days for the raw rows and 400 for
/// the rollups, so one day past the raw window cuts what the record would have landed and keeps the rollups.
#[tokio::test]
async fn a_parked_record_whose_acknowledgement_is_refused_is_charged_once() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    park_in_file(&path, "parked", "an earlier error", 0);
    refuse_parked_deletes(&LedgerDb::open(&path).unwrap());
    let store = fresh_store();
    let h = on_disk(&up, store.clone(), &path, quick()).await;
    // The start's try is over once the row records it, or once the record landed beside it.
    wait_for_async(Duration::from_secs(5), || async {
        let tried =
            h.db.read(|c| {
                Ok(c.query_row("SELECT error FROM parked_records", [], |r| {
                    r.get::<_, String>(0)
                })?)
            })
            .await
            .unwrap()
                != "an earlier error";
        let landed = h.db.request("parked").await.unwrap().is_some();
        (tried || landed).then_some(())
    })
    .await;
    h.ledger.clone().shutdown().await.unwrap();
    drop_trigger(&h.db, "refuse_ack");
    // The refused start landed no row, so a witness received with the record shows what the pass cuts.
    h.db.insert_request(
        &RequestRecord {
            pool_id: None,
            ..finished_row("witness")
        },
        &[],
        None,
    )
    .unwrap();
    // A pass past the raw window: the request rows go, the rollups stay.
    let window = LedgerConfig {
        retention_days: 90,
        rollup_retention_days: 400,
        ..LedgerConfig::default()
    };
    let later = NOW + 91 * 86_400_000;
    let purged = run_retention(
        h.db.clone(),
        window,
        later,
        Vec::new(),
        Arc::new(PriceRevisions::shipped()),
    )
    .await
    .unwrap();
    assert_eq!(
        purged,
        Purged {
            requests: 1,
            ..Purged::default()
        },
        "the pass cuts the witness, as it would have cut the record's own rows"
    );
    drop(h);
    let h = on_disk(&up, store, &path, quick()).await;
    wait_for_async(Duration::from_secs(5), || async {
        (parked_rows(&h).await == 0).then_some(())
    })
    .await;
    let (_, attempts) =
        h.db.request("parked")
            .await
            .unwrap()
            .expect("the start landed the parked record");
    assert_eq!(attempts[0].input_tokens, Some(1_000_000));
    assert_eq!(
        charged_to_a(&h).await,
        1_000_000,
        "the parked charge contributes once"
    );
    up.shutdown().await;
}

/// Parks the priced record `poison` through the writer's own path: the `refuse` trigger refuses its row, the in-place
/// retries run out and the pass one record at a time parks it. The trigger stays until the test drops it.
async fn park_poison(h: &Harness) {
    refuse_requests(&h.db, Some("poison"));
    h.ledger.record_request(finished("poison", true)).await;
    through_the_retries(h).await;
    wait_for_async(Duration::from_secs(5), || async {
        (parked_rows(h).await == 1).then_some(())
    })
    .await;
}

/// The input tokens the rollups hold for member `a` of pool `main`: what a charge contributes, once per landing.
async fn charged_to_a(h: &Harness) -> u64 {
    let select = RollupSelect {
        pools: vec!["main".into()],
        accounts: vec!["a".into()],
        ..RollupSelect::default()
    };
    h.db.read(move |c| read_rollups(c, 0, i64::MAX, &select))
        .await
        .unwrap()
        .iter()
        .map(|row| row.input_tokens)
        .sum()
}

/// The attempts the request `id` landed with, `None` while no row of it is in the ledger.
async fn landed_attempts(h: &Harness, id: &str) -> Option<usize> {
    h.db.request(id)
        .await
        .unwrap()
        .map(|(_, attempts)| attempts.len())
}

/// A parked record whose cause is gone lands on a replay, with no restart: its row leaves the table and the request is
/// in the ledger.
#[tokio::test]
async fn replay_parked_lands_a_parked_record_whose_cause_is_gone() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let h = on_disk(
        &up,
        fresh_store(),
        &dir.path().join("ledger.sqlite"),
        quick(),
    )
    .await;
    park_poison(&h).await;
    drop_trigger(&h.db, "refuse");
    let now = h.clock.load(Ordering::SeqCst);
    assert_eq!(
        h.ledger.replay_parked(now, BOUND).await.unwrap(),
        Replayed {
            landed: 1,
            remaining: 0
        }
    );
    assert_eq!(parked_rows(&h).await, 0);
    let (_, attempts) =
        h.db.request("poison")
            .await
            .unwrap()
            .expect("the replay landed the parked record");
    assert_eq!(attempts[0].input_tokens, Some(1_000_000));
    up.shutdown().await;
}

/// A replay that cannot read the parked rows ran no pass, so it answers the error rather than nothing landed and every
/// row kept; the row stays for the replay after it.
#[tokio::test]
async fn a_replay_whose_parked_rows_cannot_be_read_answers_the_error() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let h = on_disk(
        &up,
        fresh_store(),
        &dir.path().join("ledger.sqlite"),
        quick(),
    )
    .await;
    park_poison(&h).await;
    drop_trigger(&h.db, "refuse");
    // The row read names the column; the count a reply carries does not, so only the pass itself fails.
    rename_parked_record(&h.db, "record", "record_held");
    let now = h.clock.load(Ordering::SeqCst);
    let error = h.ledger.replay_parked(now, BOUND).await.unwrap_err();
    assert!(error.to_string().contains("record"), "{error}");
    assert_eq!(parked_rows(&h).await, 1);

    rename_parked_record(&h.db, "record_held", "record");
    assert_eq!(
        h.ledger.replay_parked(now, BOUND).await.unwrap(),
        Replayed {
            landed: 1,
            remaining: 0
        }
    );
    up.shutdown().await;
}

fn rename_parked_record(db: &LedgerDb, from: &str, to: &str) {
    db.write(|c| {
        c.execute_batch(&format!(
            "ALTER TABLE parked_records RENAME COLUMN {from} TO {to};"
        ))?;
        Ok(())
    })
    .unwrap();
}

/// The parked row leaves in the transaction that lands its record, on a replay as at the start: a replay whose
/// acknowledgement the database refuses lands nothing and keeps the row, and the replay after it charges the record
/// once.
#[tokio::test]
async fn a_replay_whose_acknowledgement_is_refused_keeps_the_row_and_a_retry_charges_once() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let h = on_disk(
        &up,
        fresh_store(),
        &dir.path().join("ledger.sqlite"),
        quick(),
    )
    .await;
    park_poison(&h).await;
    drop_trigger(&h.db, "refuse");
    refuse_parked_deletes(&h.db);
    let charged = charged_to_a(&h).await;
    let now = h.clock.load(Ordering::SeqCst);
    assert_eq!(
        h.ledger.replay_parked(now, BOUND).await.unwrap(),
        Replayed {
            landed: 0,
            remaining: 1
        }
    );
    assert!(
        h.db.request("poison").await.unwrap().is_none(),
        "the record rolled back with its acknowledgement"
    );
    assert_eq!(charged_to_a(&h).await, charged, "and charged nothing");

    drop_trigger(&h.db, "refuse_ack");
    assert_eq!(
        h.ledger.replay_parked(now, BOUND).await.unwrap(),
        Replayed {
            landed: 1,
            remaining: 0
        }
    );
    assert_eq!(landed_attempts(&h, "poison").await, Some(1));
    assert_eq!(
        charged_to_a(&h).await,
        1_000_000,
        "the parked charge contributes once"
    );
    up.shutdown().await;
}

/// A replay reports the members' spend before it answers, so a selection right after a replay whose charge crossed a
/// member's budget is refused on the landed money rather than admitted on the old balance.
#[tokio::test]
async fn a_replay_that_crosses_a_budget_is_followed_by_a_refused_selection() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let h = on_disk(
        &up,
        fresh_store(),
        &dir.path().join("ledger.sqlite"),
        quick(),
    )
    .await;
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
    // The mirror's record of the edit lands first, so no event of it reaches the writer behind the batch the test
    // refuses.
    wait_for_async(Duration::from_secs(5), || async {
        h.db.read(|c| phoebis_ledger::recent_events_of_kind(c, "registry_changed", 5))
            .await
            .unwrap()
            .into_iter()
            .find(|e| {
                e.detail
                    .as_deref()
                    .is_some_and(|d| d.contains("pool_members_set"))
            })
    })
    .await;
    park_poison(&h).await;
    drop_trigger(&h.db, "refuse");
    wait_for(Duration::from_secs(5), || {
        (!h.ledger.degraded()).then_some(())
    })
    .await;
    let day = wait_for_async(Duration::from_secs(5), || async {
        h.router
            .member_spends()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.pool_id == "main" && r.account_id == "a" && r.period == BudgetPeriod::Day)
    })
    .await;
    assert!(
        day.spent_usd < 0.01,
        "the parked charge is not spend yet: {day:?}"
    );

    let now = h.clock.load(Ordering::SeqCst);
    let replayed = h.ledger.replay_parked(now, BOUND).await.unwrap();
    assert_eq!(replayed.landed, 1, "{replayed:?}");
    let refused = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-5[1m]", "s1"))
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
    up.shutdown().await;
}

/// The writer's turn is what serializes replays: two asked for at once land the row once between them.
#[tokio::test]
async fn two_replays_at_once_land_the_row_once() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let h = on_disk(
        &up,
        fresh_store(),
        &dir.path().join("ledger.sqlite"),
        quick(),
    )
    .await;
    park_poison(&h).await;
    drop_trigger(&h.db, "refuse");
    let now = h.clock.load(Ordering::SeqCst);
    let (first, second) = tokio::join!(
        h.ledger.replay_parked(now, BOUND),
        h.ledger.replay_parked(now, BOUND)
    );
    let (first, second) = (first.unwrap(), second.unwrap());
    assert_eq!(first.landed + second.landed, 1, "{first:?} {second:?}");
    assert_eq!((first.remaining, second.remaining), (0, 0));
    assert_eq!(landed_attempts(&h, "poison").await, Some(1));
    assert_eq!(charged_to_a(&h).await, 1_000_000);
    up.shutdown().await;
}

/// The replay route's bound on the writer's turn.
const BOUND: Duration = Duration::from_secs(30);

/// Parks `id` once the writer is past its start-up pass, which would otherwise land the row before the test asks: a
/// replay answers from the writer's loop alone, and the loop starts after the pass.
async fn park_landable(h: &Harness, id: &str) {
    let now = h.clock.load(Ordering::SeqCst);
    assert_eq!(
        h.ledger.replay_parked(now, BOUND).await.unwrap(),
        Replayed::default()
    );
    park_in(&h.db, id, "refused by test", NOW);
}

/// A batch of one unpriced request held on the full disk past its in-place retries, so the writer waits out
/// `BATCH_RETRY_HOLD` and takes no turn between batches until the disk is freed.
async fn hold_a_batch(h: &Harness) {
    h.db.fill_the_disk();
    h.ledger
        .record_request(Record::Request {
            request: RequestRecord {
                user_agent: Some("x".repeat(512 << 10)),
                ..finished_row("held")
            },
            attempts: vec![serving_attempt("held", false)],
            bodies: None,
        })
        .await;
    through_the_retries(h).await;
    wait_for(Duration::from_secs(5), || {
        (write_retries(h) == 6).then_some(())
    })
    .await;
}

/// A replay asked for on a task, so the test can step the ledger's clock while it waits.
fn replay_on_a_task(
    h: &Harness,
    now: i64,
) -> tokio::task::JoinHandle<Result<Replayed, LedgerError>> {
    let ledger = h.ledger.clone();
    tokio::spawn(async move { ledger.replay_parked(now, BOUND).await })
}

/// Frees the disk and steps the clock a hold at a time until the held batch lands.
async fn free_the_writer(h: &Harness) {
    h.db.free_the_disk();
    step_until(h, BATCH_RETRY_HOLD, || retrying(h) == 0).await;
}

/// A replay asked for while the writer holds a batch waits its bound on the ledger's clock and answers that the writer
/// is held. The message it left in the control channel is the caller's once the deadline passed, so the writer drops
/// it unrun when it comes back, and the replay after it lands the row.
#[tokio::test]
async fn a_replay_asked_under_a_held_batch_answers_held_past_its_bound_and_never_runs() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let h = on_disk(
        &up,
        fresh_store(),
        &dir.path().join("ledger.sqlite"),
        quick(),
    )
    .await;
    park_landable(&h, "parked").await;
    hold_a_batch(&h).await;
    let now = h.clock.load(Ordering::SeqCst);
    let replay = replay_on_a_task(&h, now);
    stays_for(Duration::from_millis(400), || !replay.is_finished()).await;
    step(&h, BOUND + Duration::from_secs(1));
    let answer = tokio::time::timeout(Duration::from_secs(5), replay)
        .await
        .expect("the replay answers once its bound passed")
        .unwrap();
    assert!(matches!(answer, Err(LedgerError::WriterHeld)), "{answer:?}");
    // The step also ran out the hold, and the batch met the full disk once more.
    wait_for(Duration::from_secs(5), || {
        (write_retries(&h) == 7).then_some(())
    })
    .await;
    free_the_writer(&h).await;
    assert_eq!(parked_rows(&h).await, 1);
    assert_eq!(
        charged_to_a(&h).await,
        0,
        "the parked charge has not landed"
    );
    // The control channel is read in order, so the message given up on reaches the writer ahead of this one: a landed
    // row here means the first never ran.
    assert_eq!(
        h.ledger.replay_parked(now, BOUND).await.unwrap(),
        Replayed {
            landed: 1,
            remaining: 0
        }
    );
    assert_eq!(charged_to_a(&h).await, 1_000_000, "the charge lands once");
    up.shutdown().await;
}

/// The control channel holds one message, so a second replay asked for under the same held batch waits to send behind
/// the first. Both waits run to one deadline, so both answer held as the bound passes and neither replay runs.
#[tokio::test]
async fn two_replays_asked_under_a_held_batch_both_answer_held_past_the_bound_and_neither_runs() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let h = on_disk(
        &up,
        fresh_store(),
        &dir.path().join("ledger.sqlite"),
        quick(),
    )
    .await;
    park_landable(&h, "parked").await;
    hold_a_batch(&h).await;
    let now = h.clock.load(Ordering::SeqCst);
    let (first, second) = (replay_on_a_task(&h, now), replay_on_a_task(&h, now));
    stays_for(Duration::from_millis(400), || {
        !first.is_finished() && !second.is_finished()
    })
    .await;
    step(&h, BOUND + Duration::from_secs(1));
    // The waits read the ledger's clock every 250 ms, so a second covers both seeing the deadline, and a send left
    // outside the bound would wait on the writer for good.
    let answers = tokio::time::timeout(Duration::from_secs(1), async {
        (first.await.unwrap(), second.await.unwrap())
    })
    .await
    .expect("both replays answer within a clock read of their deadline");
    assert!(
        matches!(
            answers,
            (Err(LedgerError::WriterHeld), Err(LedgerError::WriterHeld))
        ),
        "{answers:?}"
    );
    wait_for(Duration::from_secs(5), || {
        (write_retries(&h) == 7).then_some(())
    })
    .await;
    free_the_writer(&h).await;
    assert_eq!(parked_rows(&h).await, 1);
    assert_eq!(
        charged_to_a(&h).await,
        0,
        "the parked charge has not landed"
    );
    assert_eq!(
        h.ledger.replay_parked(now, BOUND).await.unwrap(),
        Replayed {
            landed: 1,
            remaining: 0
        }
    );
    assert_eq!(charged_to_a(&h).await, 1_000_000, "the charge lands once");
    up.shutdown().await;
}

/// A caller that goes away before its deadline never takes the claim, as the route's future does when its client
/// closes the connection. The message it sent still carries the deadline, so the writer that comes back after it
/// refuses the replay unrun, and the replay after it lands the row.
#[tokio::test]
async fn a_replay_whose_caller_went_away_before_its_deadline_never_runs_after_it() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let h = on_disk(
        &up,
        fresh_store(),
        &dir.path().join("ledger.sqlite"),
        quick(),
    )
    .await;
    park_landable(&h, "parked").await;
    hold_a_batch(&h).await;
    let now = h.clock.load(Ordering::SeqCst);
    let replay = replay_on_a_task(&h, now);
    stays_for(Duration::from_millis(400), || !replay.is_finished()).await;
    replay.abort();
    assert!(replay.await.unwrap_err().is_cancelled());
    step(&h, BOUND + Duration::from_secs(1));
    wait_for(Duration::from_secs(5), || {
        (write_retries(&h) == 7).then_some(())
    })
    .await;
    free_the_writer(&h).await;
    // The control channel is read in order, so the abandoned message reaches the writer ahead of this one: a landed
    // row here means the first never ran.
    assert_eq!(
        h.ledger.replay_parked(now, BOUND).await.unwrap(),
        Replayed {
            landed: 1,
            remaining: 0
        }
    );
    assert_eq!(charged_to_a(&h).await, 1_000_000, "the charge lands once");
    up.shutdown().await;
}

/// The claim decides who owns a replay, never the clock: a replay the writer took before the deadline answers its
/// rows to a caller still waiting past it. The test parks the writer's count read from before the claim until after
/// the deadline, so the writer has landed the row and waits on the count it answers with while the bound runs out.
#[tokio::test]
async fn a_replay_the_writer_claimed_inside_the_bound_answers_its_rows_past_it() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let h = on_disk(
        &up,
        fresh_store(),
        &dir.path().join("ledger.sqlite"),
        quick(),
    )
    .await;
    park_landable(&h, "parked").await;
    hold_a_batch(&h).await;
    let now = h.clock.load(Ordering::SeqCst);
    let replay = replay_on_a_task(&h, now);
    stays_for(Duration::from_millis(400), || !replay.is_finished()).await;
    let hold = h.db.debug_hold_next_writer_read(WriterRead::ParkedCount);
    h.db.free_the_disk();
    // Halfway: the batch's retry hold runs out, the batch lands, and the writer's next turn claims the replay and lands
    // the row.
    step(&h, BOUND / 2);
    wait_for(Duration::from_secs(5), || hold.is_parked().then_some(())).await;
    step(&h, BOUND / 2 + Duration::from_secs(1));
    stays_for(Duration::from_millis(400), || !replay.is_finished()).await;
    hold.release();
    let answer = tokio::time::timeout(Duration::from_secs(5), replay)
        .await
        .expect("the claimed replay answers once the count is read")
        .unwrap();
    assert_eq!(
        answer.unwrap(),
        Replayed {
            landed: 1,
            remaining: 0
        }
    );
    assert!(landed_attempts(&h, "parked").await.is_some());
    assert_eq!(charged_to_a(&h).await, 1_000_000, "the charge lands once");
    up.shutdown().await;
}

/// The start lands its parked records before it reads the members' spend, so a parked charge is in the first report
/// rather than waiting for the next flush or period boundary.
#[tokio::test]
async fn a_parked_charge_landed_at_the_start_is_in_the_first_spend_report() {
    let up = FakeUpstream::start(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let store = fresh_store();
    let h = on_disk(&up, store.clone(), &path, quick()).await;
    budget_member_a(&h).await;
    assert_eq!(spent_by_a(&h).await, 0.0);
    h.ledger.clone().shutdown().await.unwrap();
    drop(h);
    park_in_file(&path, "parked", "refused by test", NOW);
    let h = on_disk(&up, store, &path, quick()).await;
    let first = wait_for_async(Duration::from_secs(5), || async {
        h.router
            .member_spends()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.pool_id == "main" && r.account_id == "a")
    })
    .await;
    assert!(
        first.spent_usd > 0.0,
        "the first report carries the parked charge: {first:?}"
    );
    assert_eq!(parked_rows(&h).await, 0);
    up.shutdown().await;
}

#[tokio::test]
async fn the_start_row_dates_the_gap_from_the_newest_row_and_counts_the_load() {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let start_rows = || async {
        h.db.events(0, i64::MAX, 100)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "router_started")
            .map(|e| {
                assert_eq!(e.account_id, None);
                (
                    e.at,
                    serde_json::from_str::<serde_json::Value>(e.detail.as_deref().unwrap())
                        .unwrap(),
                )
            })
            .collect::<Vec<_>>()
    };
    let counts = phoebis_routing::LoadCounts {
        pins: 3,
        windows: 2,
        affinity_entries: 5,
        shed_marks: 4,
        shed_rounds_stale: 1,
    };
    h.ledger.record(Record::RouterStarted {
        version: "0.1.0".into(),
        counts,
        at: NOW,
    });
    let first = wait_for_async(Duration::from_secs(5), || async {
        start_rows().await.pop()
    })
    .await;
    assert_eq!(
        first,
        (
            NOW,
            serde_json::json!({
                "version": "0.1.0",
                "pins_loaded": 3,
                "shedding_marks_loaded": 4,
                "shedding_rounds_stale": 1,
                "windows_loaded": 2,
                "affinity_entries_loaded": 5,
                "down_since": null,
                "holds_lost": true,
            })
        ),
        "a fresh file has no row to date the gap from"
    );
    for at in [NOW + 100, NOW + 200] {
        h.ledger.record(Record::Event(EventRecord {
            at,
            kind: "pin_created".into(),
            account_id: Some("a".into()),
            ..Default::default()
        }));
    }
    h.ledger.record(Record::RouterStarted {
        version: "0.1.0".into(),
        counts,
        at: NOW + 300,
    });
    let second = wait_for_async(Duration::from_secs(5), || async {
        let rows = start_rows().await;
        (rows.len() == 2).then(|| rows[1].clone())
    })
    .await;
    assert_eq!(second.0, NOW + 300);
    assert_eq!(
        (
            &second.1["down_since"],
            &second.1["holds_lost"],
            &second.1["pins_loaded"]
        ),
        (
            &serde_json::json!(NOW + 200),
            &serde_json::json!(true),
            &serde_json::json!(3)
        ),
        "the gap runs from the newest row the ledger held"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_budget_reached_at_the_start_lands_behind_the_start_row_and_leaves_its_gap_alone() {
    let up = FakeUpstream::start(vec![]).await;
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate(), wall()).unwrap());
    let (db, _dir) = common::temp_ledger();
    let db = Arc::new(db);
    // The previous run seeded the registry and stopped. Its last request spent past a budget the member carries now.
    let previous = harness_on(
        store.clone(),
        db.clone(),
        &up,
        &[api_key("a", C, 0)],
        (0, None),
        LedgerConfig::default(),
        false,
    )
    .await;
    previous.ledger.clone().shutdown().await.unwrap();
    previous.router.shutdown().await.unwrap();
    store
        .set_pool_members(
            "main",
            &[MemberWrite {
                account_id: "a".into(),
                priority: 0,
                budget_daily_usd: None,
                budget_monthly_usd: Some(1.0),
            }],
            "test",
        )
        .unwrap();
    let last_request = NOW - 1_000;
    db.insert_request(
        &RequestRecord {
            id: "r1".into(),
            received_at: last_request,
            pool_id: Some("main".into()),
            canonical_model: "claude-haiku-4-5".into(),
            model: "claude-haiku-4-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        &[AttemptRecord {
            request_id: "r1".into(),
            attempt: 1,
            account_id: "a".into(),
            account_kind: AccountKind::ApiKey,
            outcome: "succeeded".into(),
            input_tokens: Some(2_000_000),
            ..Default::default()
        }],
        None,
    )
    .unwrap();

    let h = harness_on(
        store,
        db,
        &up,
        &[api_key("a", C, 0)],
        (0, None),
        LedgerConfig::default(),
        false,
    )
    .await;
    h.ledger.record(Record::RouterStarted {
        version: "0.1.0".into(),
        counts: phoebis_routing::LoadCounts::default(),
        at: NOW,
    });
    let rows = wait_for_async(Duration::from_secs(5), || async {
        let rows: Vec<EventRecord> = h.db.events(0, i64::MAX, 100).await.unwrap();
        rows.iter()
            .any(|e| e.kind == "budget_reached")
            .then_some(rows)
    })
    .await;
    assert_eq!(
        rows.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
        ["router_started", "budget_reached"],
        "the start's own budget row follows the start row"
    );
    let detail: serde_json::Value =
        serde_json::from_str(rows[0].detail.as_deref().unwrap()).unwrap();
    assert_eq!(
        detail["down_since"],
        serde_json::json!(last_request),
        "the gap ends at the previous run's last request"
    );
    up.shutdown().await;
}

/// The registry and the ledger a previous run left, with the directory of the ledger's file: the member's last request
/// spent past the monthly budget it carries now, so the writer's first spend read at the next start crosses it.
async fn over_budget_after_a_run(
    up: &FakeUpstream,
) -> (Arc<Store>, Arc<LedgerDb>, tempfile::TempDir) {
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate(), wall()).unwrap());
    let (db, dir) = common::temp_ledger();
    let db = Arc::new(db);
    let previous = harness_on(
        store.clone(),
        db.clone(),
        up,
        &[api_key("a", C, 0)],
        (0, None),
        LedgerConfig::default(),
        false,
    )
    .await;
    previous.ledger.clone().shutdown().await.unwrap();
    previous.router.shutdown().await.unwrap();
    store
        .set_pool_members(
            "main",
            &[MemberWrite {
                account_id: "a".into(),
                priority: 0,
                budget_daily_usd: None,
                budget_monthly_usd: Some(1.0),
            }],
            "test",
        )
        .unwrap();
    db.insert_request(
        &RequestRecord {
            id: "r1".into(),
            received_at: NOW - 1_000,
            pool_id: Some("main".into()),
            canonical_model: "claude-haiku-4-5".into(),
            model: "claude-haiku-4-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        &[AttemptRecord {
            request_id: "r1".into(),
            attempt: 1,
            account_id: "a".into(),
            account_kind: AccountKind::ApiKey,
            outcome: "succeeded".into(),
            input_tokens: Some(2_000_000),
            ..Default::default()
        }],
        None,
    )
    .unwrap();
    (store, db, dir)
}

/// A replay the writer runs on its first turn, ahead of the start row the spawn queued, reads the members' spend and
/// leaves the budget rows it crosses for the next batch, so they land behind the start row. The writer takes either
/// first at random, so the case starts the router enough times that a replay landing the rows itself would show.
#[tokio::test]
async fn a_replay_on_the_writers_first_turn_lands_its_budget_row_behind_the_start_row() {
    let up = FakeUpstream::start(vec![]).await;
    for start in 0..30 {
        let (store, db, _dir) = over_budget_after_a_run(&up).await;
        let h = harness_on(
            store,
            db,
            &up,
            &[api_key("a", C, 0)],
            (0, None),
            LedgerConfig {
                start_version: Some("0.1.0".into()),
                ..LedgerConfig::default()
            },
            false,
        )
        .await;
        let replayed = h.ledger.replay_parked(NOW, BOUND).await.unwrap();
        assert_eq!((replayed.landed, replayed.remaining), (0, 0));
        let kinds = wait_for_async(Duration::from_secs(5), || async {
            let rows: Vec<EventRecord> = h.db.events(0, i64::MAX, 100).await.unwrap();
            rows.iter()
                .any(|e| e.kind == "budget_reached")
                .then(|| rows.into_iter().map(|e| e.kind).collect::<Vec<_>>())
        })
        .await;
        assert_eq!(
            kinds,
            ["router_started", "budget_reached"],
            "start {start}: the replay's budget row follows the start row"
        );
        h.ledger.clone().shutdown().await.unwrap();
        h.router.shutdown().await.unwrap();
    }
    up.shutdown().await;
}

#[tokio::test]
async fn a_restart_over_a_ledger_holding_the_period_s_crossing_writes_no_second_one() {
    let up = FakeUpstream::start(vec![]).await;
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate(), wall()).unwrap());
    let (db, _dir) = common::temp_ledger();
    let db = Arc::new(db);
    let accounts = [api_key("a", C, 0)];
    let run = || {
        harness_on_tables(
            store.clone(),
            db.clone(),
            &up,
            &accounts,
            (0, None),
            LedgerConfig::default(),
            false,
            phoebis_ledger::test_tables::api_prices(),
            phoebis_ledger::test_tables::subscription_costs(),
        )
    };
    let started = |h: &Harness, at: i64| {
        h.ledger.record(Record::RouterStarted {
            version: "0.1.0".into(),
            counts: phoebis_routing::LoadCounts::default(),
            at,
        })
    };
    let kinds = |h: &Harness| {
        let db = h.db.clone();
        async move {
            db.events(0, i64::MAX, 100)
                .await
                .unwrap()
                .into_iter()
                .map(|e| e.kind)
                .collect::<Vec<_>>()
        }
    };
    // A first run seeds the registry. The member then carries a budget its spend is past.
    let seeding = run().await;
    seeding.ledger.clone().shutdown().await.unwrap();
    seeding.router.shutdown().await.unwrap();
    store
        .set_pool_members(
            "main",
            &[MemberWrite {
                account_id: "a".into(),
                priority: 0,
                budget_daily_usd: None,
                budget_monthly_usd: Some(1.0),
            }],
            "test",
        )
        .unwrap();
    db.insert_request(
        &RequestRecord {
            id: "r1".into(),
            received_at: NOW - 1_000,
            pool_id: Some("main".into()),
            canonical_model: "claude-haiku-4-5".into(),
            model: "claude-haiku-4-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            outcome: "succeeded".into(),
            attempts: 1,
            ..Default::default()
        },
        &[AttemptRecord {
            request_id: "r1".into(),
            attempt: 1,
            account_id: "a".into(),
            account_kind: AccountKind::ApiKey,
            outcome: "succeeded".into(),
            input_tokens: Some(2_000_000),
            ..Default::default()
        }],
        None,
    )
    .unwrap();

    // The next run announces the crossing it finds at its start.
    let crossing = run().await;
    started(&crossing, NOW);
    wait_for_async(Duration::from_secs(5), || async {
        (kinds(&crossing).await == ["router_started", "budget_reached"]).then_some(())
    })
    .await;
    crossing.ledger.clone().shutdown().await.unwrap();
    crossing.router.shutdown().await.unwrap();

    // A restart in the same period finds the crossing in the ledger: its start's rows ride the start row's batch,
    // and hold no second one.
    let restart = run().await;
    started(&restart, NOW + 1);
    let after_restart = wait_for_async(Duration::from_secs(5), || async {
        let kinds = kinds(&restart).await;
        (kinds.iter().filter(|k| *k == "router_started").count() == 2).then_some(kinds)
    })
    .await;
    assert_eq!(
        after_restart,
        ["router_started", "budget_reached", "router_started"],
        "one crossing per period, whatever the restarts"
    );
    // The crossing the restart took from the ledger still ends with its period.
    restart.clock.store(2_678_400_000 + 1_000, Ordering::SeqCst);
    let ended = wait_for_async(Duration::from_secs(5), || async {
        restart
            .db
            .events(0, i64::MAX, 100)
            .await
            .unwrap()
            .into_iter()
            .find(|e| e.kind == "budget_period_ended")
    })
    .await;
    let detail: serde_json::Value = serde_json::from_str(ended.detail.as_deref().unwrap()).unwrap();
    // Two million Haiku 4.5 input tokens at the test list's $1 a million, its first entry before its date.
    assert_eq!(detail["previous_spent_usd"], serde_json::json!(2.0));
    up.shutdown().await;
}

/// A routing log row as the writer stores it, on the request `request_id` at `at`: a decision of `kind` on account
/// `a` under session `s1`, no probe and no move. A case edits the columns it filters on.
fn logged(request_id: &str, seq: u32, attempt: u32, at: i64, kind: &'static str) -> DecisionRow {
    let (mut summary, mut record) = decision(request_id, seq, attempt);
    record.request.at = at;
    record.request.flags.probe = false;
    record.outcome.kind = kind;
    record.outcome.account_id = Some("a".into());
    record.outcome.from_account_id = None;
    summary.kind = kind;
    summary.probe = false;
    DecisionRow::of(summary, record, at)
}

/// The writer's own encoding: the summary's JSON and the record's one-shot zstd frame, the content size in its header.
fn put_decision(db: &LedgerDb, row: &DecisionRow) {
    let summary = serde_json::to_string(&row.summary).unwrap();
    let frame = zstd::bulk::compress(&serde_json::to_vec(&row.record).unwrap(), 0).unwrap();
    db.write(|c| {
        c.execute(
            "INSERT INTO decisions (request_id, seq, attempt, at, kind, probe, key_id, pool_id, account_id, \
             from_account_id, session_id, agent_id, model, summary, record) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            rusqlite::params![
                row.request_id,
                row.seq,
                row.attempt,
                row.at,
                row.kind,
                row.probe,
                row.key_id,
                row.pool_id,
                row.account_id,
                row.from_account_id,
                row.session_id,
                row.agent_id,
                row.model,
                summary,
                frame
            ],
        )?;
        Ok(())
    })
    .unwrap();
}

fn logged_event(at: i64, kind: &str) -> EventRecord {
    EventRecord {
        at,
        kind: kind.into(),
        detail: Some(r#"{"scope":"account"}"#.into()),
        ..Default::default()
    }
}

/// One attempt row of `request_id`: a 429 with no usage, or an answer with `read` cache-read tokens of 1,000 input.
fn logged_attempt(request_id: &str, attempt: u32, status: u16, read: Option<u64>) -> AttemptRecord {
    AttemptRecord {
        request_id: request_id.into(),
        attempt,
        account_id: "a".into(),
        account_kind: AccountKind::Oauth,
        started_at: NOW,
        finished_at: Some(NOW + i64::from(attempt) * 1_000),
        upstream_status: Some(status),
        outcome: if status == 200 {
            "succeeded"
        } else {
            "rate_limited"
        }
        .into(),
        input_tokens: read.map(|_| 1_000),
        cache_read_tokens: read,
        ..Default::default()
    }
}

fn put_request(db: &LedgerDb, id: &str, attempts: &[AttemptRecord], held: Option<&str>) {
    db.insert_request(
        &RequestRecord {
            id: id.into(),
            received_at: NOW,
            finished_at: Some(NOW + 5_000),
            canonical_model: "claude-sonnet-5".into(),
            model: "claude-sonnet-5".into(),
            kind: "inference".into(),
            origin: "client".into(),
            status: Some(200),
            outcome: "succeeded".into(),
            attempts: attempts.len() as u32,
            held: held.map(String::from),
            ..Default::default()
        },
        attempts,
        None,
    )
    .unwrap();
}

/// The whole default range of these cases: every row they write sits inside it.
fn log_filter() -> phoebis_ledger::RoutingLogFilter {
    phoebis_ledger::RoutingLogFilter {
        from: 0,
        to: 10 * NOW,
        ..Default::default()
    }
}

/// Each row by its identity: `d:<request>/<seq>` for a decision, `e:<kind>@<at>` for an event.
fn log_ids(db: &LedgerDb, filter: &phoebis_ledger::RoutingLogFilter) -> Vec<String> {
    let page = db
        .read_blocking(|c| phoebis_ledger::list_routing_log(c, filter, Direction::Desc, None, 200))
        .unwrap();
    page.rows.iter().map(log_id).collect()
}

fn log_id(row: &phoebis_ledger::RoutingLogRow) -> String {
    match row {
        phoebis_ledger::RoutingLogRow::Decision(d) => format!("d:{}/{}", d.request_id, d.seq),
        phoebis_ledger::RoutingLogRow::Event(e) => format!("e:{}@{}", e.kind, e.at),
    }
}

#[test]
fn the_routing_log_lists_newest_first_with_decisions_before_events_at_one_instant() {
    let (db, _dir) = common::temp_ledger();
    put_decision(&db, &logged("r1", 1, 1, 100, "placed"));
    db.insert_event(&logged_event(200, "cooldown_set")).unwrap();
    put_decision(&db, &logged("r2", 1, 1, 200, "held"));
    put_decision(&db, &logged("r2", 2, 1, 200, "placed"));
    db.insert_event(&logged_event(300, "cooldown_cleared"))
        .unwrap();
    db.insert_event(&logged_event(100, "overage_started"))
        .unwrap();
    assert_eq!(
        log_ids(&db, &log_filter()),
        [
            "e:cooldown_cleared@300",
            "d:r2/2",
            "d:r2/1",
            "e:cooldown_set@200",
            "d:r1/1",
            "e:overage_started@100"
        ],
        "newest first; at one instant the decisions, newest seq first, then the events"
    );
    let ranged = phoebis_ledger::RoutingLogFilter {
        from: 100,
        to: 200,
        ..log_filter()
    };
    assert_eq!(
        log_ids(&db, &ranged),
        [
            "d:r2/2",
            "d:r2/1",
            "e:cooldown_set@200",
            "d:r1/1",
            "e:overage_started@100"
        ],
        "both ends of the range are inclusive"
    );
}

#[test]
fn the_routing_log_answers_a_cursor_only_past_a_full_page() {
    let (db, _dir) = common::temp_ledger();
    put_decision(&db, &logged("r1", 1, 1, 100, "held"));
    put_decision(&db, &logged("r1", 2, 1, 100, "placed"));
    put_decision(&db, &logged("r2", 1, 1, 100, "placed"));
    db.insert_event(&logged_event(100, "cooldown_set")).unwrap();
    db.insert_event(&logged_event(100, "cooldown_cleared"))
        .unwrap();
    let page = |dir: Direction, cursor: Option<&phoebis_ledger::RoutingLogCursor>, limit: usize| {
        db.read_blocking(|c| phoebis_ledger::list_routing_log(c, &log_filter(), dir, cursor, limit))
            .unwrap()
    };
    let whole = page(Direction::Desc, None, 5);
    assert_eq!(
        whole.rows.iter().map(log_id).collect::<Vec<_>>(),
        [
            "d:r2/1",
            "d:r1/2",
            "d:r1/1",
            "e:cooldown_cleared@100",
            "e:cooldown_set@100"
        ],
        "at one instant the newer request id first, then each request's newest seq"
    );
    assert_eq!(
        whole.next_cursor, None,
        "a page of exactly `limit` rows has nothing past it"
    );
    let first = page(Direction::Desc, None, 4);
    assert_eq!(first.rows.len(), 4);
    let cursor = first.next_cursor.expect("a row lies past a page of four");
    let next = page(
        Direction::Desc,
        Some(&phoebis_ledger::RoutingLogCursor::parse(&cursor).expect("the cursor parses back")),
        4,
    );
    assert_eq!(
        next.rows.iter().map(log_id).collect::<Vec<_>>(),
        ["e:cooldown_set@100"],
        "the next page holds the extra row"
    );
    assert_eq!(next.next_cursor, None);
    // Page by page through one instant: the cursor's three keys neither skip nor repeat a row on a tie. The walk stops
    // at twice the rows, so a cursor that rereads a row fails the comparison rather than looping.
    let walk = |dir: Direction| {
        let mut walked = Vec::new();
        let mut cursor: Option<phoebis_ledger::RoutingLogCursor> = None;
        for _ in 0..10 {
            let one = page(dir, cursor.as_ref(), 1);
            walked.extend(one.rows.iter().map(log_id));
            match one.next_cursor {
                Some(next) => cursor = phoebis_ledger::RoutingLogCursor::parse(&next),
                None => break,
            }
        }
        walked
    };
    assert_eq!(
        walk(Direction::Desc),
        whole.rows.iter().map(log_id).collect::<Vec<_>>(),
        "one row a page walks the instant in the whole page's order"
    );
    assert_eq!(
        walk(Direction::Asc),
        [
            "e:cooldown_set@100",
            "e:cooldown_cleared@100",
            "d:r1/1",
            "d:r1/2",
            "d:r2/1"
        ],
        "under asc the exact reverse: at one instant the events, oldest first, then the decisions"
    );
}

#[test]
fn an_absent_kinds_lists_every_kind_and_kinds_names_exactly_its_set() {
    let (db, _dir) = common::temp_ledger();
    put_decision(&db, &logged("r1", 1, 1, 100, "placed"));
    let mut probe = logged("r2", 1, 1, 200, "renewed");
    probe.probe = true;
    put_decision(&db, &probe);
    db.insert_event(&logged_event(300, "warm_prefix_expired"))
        .unwrap();
    db.insert_event(&logged_event(400, "cooldown_set")).unwrap();
    let kinds = |kinds: &[&str]| phoebis_ledger::RoutingLogFilter {
        kinds: Some(
            LogKinds::parse(&kinds.iter().map(|k| k.to_string()).collect::<Vec<_>>())
                .expect("every kind is known"),
        ),
        ..log_filter()
    };
    assert_eq!(
        log_ids(&db, &log_filter()),
        [
            "e:cooldown_set@400",
            "e:warm_prefix_expired@300",
            "d:r2/1",
            "d:r1/1"
        ],
        "an absent kinds lists every decision and event, the probe decisions and the prefix expiries among them"
    );
    assert_eq!(
        log_ids(&db, &kinds(&["probe"])),
        ["d:r2/1"],
        "`probe` lists the probes alone, of any kind"
    );
    assert_eq!(
        log_ids(&db, &kinds(&["renewed"])),
        Vec::<String>::new(),
        "a decision kind names the decisions that are not probes"
    );
    assert_eq!(
        log_ids(&db, &kinds(&["placed", "probe"])),
        ["d:r2/1", "d:r1/1"]
    );
    assert_eq!(
        log_ids(&db, &kinds(&["warm_prefix_expired"])),
        ["e:warm_prefix_expired@300"],
        "the kinds named are exactly the set"
    );
}

/// The summary keeps a refusal's words as the router wrote them, so a guardrail's block lists under `refused` with
/// its own kind and code.
#[test]
fn a_guardrails_refusal_reads_back_through_the_routing_log_with_its_kind_and_code() {
    let (db, _dir) = common::temp_ledger();
    let snapshot = phoebis_routing::RegistrySnapshot::from_load(
        phoebis_store::RegistryLoad::default(),
        [0; 32],
        0,
        1,
    );
    let (record, summary) = phoebis_routing::DecisionRecord::refused(
        &phoebis_routing::RefusedRequest {
            request_id: "r-blocked".into(),
            attempt: 1,
            seq: 1,
            at: 100,
            key_id: "desktop".into(),
            key_generation: 0,
            agent_id: "main".into(),
            parent_agent_id: None,
            session_id: Some("s1".into()),
            request_class: Some("auxiliary".into()),
            canonical_model: "claude-sonnet-5".into(),
            lane: Default::default(),
            flags: Default::default(),
            chain: Default::default(),
            attempted: Vec::new(),
            holds: Vec::new(),
            client: phoebis_routing::ClientClass::ClaudeCode,
            client_version: Some("2.1.310".into()),
        },
        &snapshot,
        &phoebis_routing::RefusalFacts {
            kind: "guardrail",
            code: "guardrail_blocked",
            reason: "Large auxiliary requests".into(),
        },
    );
    put_decision(&db, &DecisionRow::of(summary, record, 100));
    let refused = phoebis_ledger::RoutingLogFilter {
        kinds: Some(LogKinds::parse(&["refused".into()]).expect("a decision kind")),
        ..log_filter()
    };
    let page = db
        .read_blocking(|c| phoebis_ledger::list_routing_log(c, &refused, Direction::Desc, None, 25))
        .unwrap();
    let [phoebis_ledger::RoutingLogRow::Decision(row)] = page.rows.as_slice() else {
        panic!("{:?}", page.rows)
    };
    assert_eq!(
        (
            row.request_id.as_str(),
            row.kind.as_str(),
            &row.summary["refusal"]
        ),
        (
            "r-blocked",
            "refused",
            &serde_json::json!({
                "kind": "guardrail",
                "code": "guardrail_blocked",
                "reason": "Large auxiliary requests"
            })
        )
    );
}

#[test]
fn counts_answer_under_a_request_filter_alone() {
    let (db, _dir) = common::temp_ledger();
    put_decision(&db, &logged("r1", 1, 1, 100, "held"));
    put_decision(&db, &logged("r1", 2, 1, 100, "placed"));
    put_decision(&db, &logged("r2", 1, 1, 200, "placed"));
    for (at, request) in [(150, Some("r1")), (250, Some("r2")), (300, None)] {
        db.insert_event(&EventRecord {
            request_id: request.map(String::from),
            ..logged_event(at, "cooldown_set")
        })
        .unwrap();
    }
    let read = |filter: &phoebis_ledger::RoutingLogFilter, limit: usize| {
        db.read_blocking(|c| {
            phoebis_ledger::list_routing_log(c, filter, Direction::Desc, None, limit)
        })
        .unwrap()
    };
    assert_eq!(
        read(&log_filter(), 25).counts,
        None,
        "a count over a range is a scan no page needs"
    );
    let one = phoebis_ledger::RoutingLogFilter {
        request: Some("r1".into()),
        ..log_filter()
    };
    let page = read(&one, 1);
    assert_eq!(
        page.rows.iter().map(log_id).collect::<Vec<_>>(),
        ["e:cooldown_set@150"]
    );
    assert_eq!(
        page.counts,
        Some((2, 1)),
        "the request's decisions and events, past the page"
    );
}

#[test]
fn a_session_prefix_matches() {
    let (db, _dir) = common::temp_ledger();
    for (n, session) in [(1, "abc-1"), (2, "abd-2")] {
        let mut row = logged(&format!("r{n}"), 1, 1, 100 * n as i64, "placed");
        row.session_id = Some(session.into());
        put_decision(&db, &row);
    }
    db.insert_event(&EventRecord {
        session_id: Some("abc-x".into()),
        ..logged_event(300, "pin_created")
    })
    .unwrap();
    let session = |s: &str| phoebis_ledger::RoutingLogFilter {
        session: Some(s.into()),
        ..log_filter()
    };
    assert_eq!(
        log_ids(&db, &session("abc")),
        ["e:pin_created@300", "d:r1/1"]
    );
    assert_eq!(log_ids(&db, &session("abc-1")), ["d:r1/1"]);
}

#[test]
fn an_agent_id_keeps_the_agents_decisions_and_events_and_leaves_a_session_level_event_out() {
    let (db, _dir) = common::temp_ledger();
    for (n, agent) in [(1, "agent-7"), (2, "main")] {
        let mut row = logged(&format!("r{n}"), 1, 1, 100 * n as i64, "placed");
        row.agent_id = Some(agent.into());
        put_decision(&db, &row);
    }
    db.insert_event(&EventRecord {
        session_id: Some("s1".into()),
        agent_id: Some("agent-7".into()),
        ..logged_event(300, "pin_created")
    })
    .unwrap();
    // A prefix expiry names the conversation and no agent.
    db.insert_event(&EventRecord {
        session_id: Some("s1".into()),
        ..logged_event(400, "warm_prefix_expired")
    })
    .unwrap();
    let agent = |a: &str| phoebis_ledger::RoutingLogFilter {
        agent_id: Some(a.into()),
        ..log_filter()
    };
    assert_eq!(
        log_ids(&db, &log_filter()),
        [
            "e:warm_prefix_expired@400",
            "e:pin_created@300",
            "d:r2/1",
            "d:r1/1"
        ],
        "without the filter, every row"
    );
    assert_eq!(
        log_ids(&db, &agent("agent-7")),
        ["e:pin_created@300", "d:r1/1"],
        "the agent's decision and its own event, the main agent's decision and the session-level event left out"
    );
    assert_eq!(log_ids(&db, &agent("main")), ["d:r2/1"]);
}

#[test]
fn a_session_prefix_with_like_metacharacters_matches_literally() {
    let (db, _dir) = common::temp_ledger();
    for (n, session) in [(1, "s_1"), (2, "sx1"), (3, "100%a"), (4, "1000b")] {
        let mut row = logged(&format!("r{n}"), 1, 1, 100 * n as i64, "placed");
        row.session_id = Some(session.into());
        put_decision(&db, &row);
    }
    let session = |s: &str| phoebis_ledger::RoutingLogFilter {
        session: Some(s.into()),
        ..log_filter()
    };
    assert_eq!(
        log_ids(&db, &session("s_")),
        ["d:r1/1"],
        "`_` is a character"
    );
    assert_eq!(
        log_ids(&db, &session("100%")),
        ["d:r3/1"],
        "`%` is a character"
    );
}

#[test]
fn accounts_match_a_move_s_source() {
    let (db, _dir) = common::temp_ledger();
    let mut moved = logged("r1", 1, 1, 100, "moved");
    moved.account_id = Some("b".into());
    moved.from_account_id = Some("a".into());
    put_decision(&db, &moved);
    let mut placed = logged("r2", 1, 1, 200, "placed");
    placed.account_id = Some("c".into());
    put_decision(&db, &placed);
    db.insert_event(&EventRecord {
        account_id: Some("a".into()),
        ..logged_event(300, "cooldown_set")
    })
    .unwrap();
    let accounts = |a: &str| phoebis_ledger::RoutingLogFilter {
        accounts: vec![a.into()],
        ..log_filter()
    };
    assert_eq!(
        log_ids(&db, &accounts("a")),
        ["e:cooldown_set@300", "d:r1/1"],
        "a move is listed under the account it left"
    );
    assert_eq!(log_ids(&db, &accounts("b")), ["d:r1/1"]);
    assert_eq!(log_ids(&db, &accounts("c")), ["d:r2/1"]);
}

#[test]
fn a_pin_event_and_a_cut_stream_list_under_the_pool_they_ran_in_and_under_no_other() {
    let (db, _dir) = common::temp_ledger();
    let created = phoebis_ledger::event_of(&phoebis_routing::RouterEvent::PinCreated {
        pin: phoebis_store::PinKey::new("k", "s1", "main", "claude-sonnet-5"),
        account_id: "a".into(),
        epoch: 1,
        pool_id: Some("main".into()),
        at: 100,
    })
    .expect("a pin row");
    db.insert_event(&created).unwrap();
    let mut assembler = Assembler::new(Arc::new(|| NOW));
    let BusEvent::RequestObserved(mut observed) = observed_event("r1") else {
        unreachable!()
    };
    observed.pool_id = Some("main".into());
    assembler.observe(BusEvent::RequestObserved(observed));
    let BusEvent::RequestComplete(mut complete) = complete_event("r1") else {
        unreachable!()
    };
    complete.outcome = BusOutcome::Incomplete;
    complete.interruption = Some(Interruption {
        code: "stream_cut".into(),
        text: "malformed response: connection closed inside chunked body".into(),
        bytes_relayed: 42,
    });
    complete.finished_at = 200;
    for record in assembler.observe(BusEvent::RequestComplete(complete)) {
        if let Record::Event(event) = record {
            db.insert_event(&event).unwrap();
        }
    }
    let pools = |pool: &str| phoebis_ledger::RoutingLogFilter {
        pools: vec![pool.into()],
        ..log_filter()
    };
    assert_eq!(
        log_ids(&db, &pools("main")),
        ["e:stream_interrupted@200", "e:pin_created@100"],
        "each event names the pool it ran in"
    );
    assert_eq!(log_ids(&db, &pools("other")), Vec::<String>::new());
}

#[test]
fn a_pin_move_lists_under_the_account_it_left_and_the_one_it_landed_on() {
    let (db, _dir) = common::temp_ledger();
    let moved = phoebis_ledger::event_of(&phoebis_routing::RouterEvent::PinMigrated {
        pin: phoebis_store::PinKey::new("k", "s1", "main", "claude-sonnet-5"),
        from_account: "a".into(),
        to_account: "b".into(),
        epoch: 2,
        reason: "spent".into(),
        reasons: Vec::new(),
        fit: Some("fits"),
        runway_ms: None,
        binding: None,
        cause: None,
        pool_id: Some("main".into()),
        at: 100,
    })
    .expect("a pin row");
    db.insert_event(&moved).unwrap();
    let accounts = |a: &str| phoebis_ledger::RoutingLogFilter {
        accounts: vec![a.into()],
        ..log_filter()
    };
    assert_eq!(
        log_ids(&db, &accounts("a")),
        ["e:pin_migrated@100"],
        "the account the pin left"
    );
    assert_eq!(
        log_ids(&db, &accounts("b")),
        ["e:pin_migrated@100"],
        "the account it landed on"
    );
    assert_eq!(log_ids(&db, &accounts("c")), Vec::<String>::new());
}

/// A decision row's identity, its own attempt, the request's count, and the answering attempt's status and cache.
type CompletionFacts = (
    String,
    u32,
    Option<u32>,
    Option<u16>,
    Option<phoebis_ledger::CacheFacts>,
);

#[test]
fn a_decision_s_completion_is_the_attempt_that_answered_after_a_retry_in_place() {
    let (db, _dir) = common::temp_ledger();
    let content = |mut row: DecisionRow| {
        let rung = phoebis_routing::Placement {
            rung: "content",
            depth: 2,
            prefix_tokens: 500,
        };
        row.summary.rung = Some(rung.clone());
        row.record.outcome.rung = Some(rung);
        row
    };
    // A retry in place: one decision, two wire attempts, the second the one that answered.
    put_decision(&db, &content(logged("r1", 1, 1, 100, "placed")));
    put_request(
        &db,
        "r1",
        &[
            logged_attempt("r1", 1, 429, None),
            logged_attempt("r1", 2, 200, Some(600)),
        ],
        None,
    );
    // A hold and its placement share one attempt.
    put_decision(&db, &logged("r2", 1, 1, 200, "held"));
    put_decision(&db, &logged("r2", 2, 1, 200, "placed"));
    put_request(&db, "r2", &[logged_attempt("r2", 1, 200, Some(0))], None);
    // A failover: the next dispatching decision bounds the first's attempts.
    put_decision(&db, &logged("r3", 1, 1, 300, "placed"));
    put_decision(&db, &logged("r3", 2, 2, 300, "placed"));
    put_request(
        &db,
        "r3",
        &[
            logged_attempt("r3", 1, 429, None),
            logged_attempt("r3", 2, 200, Some(300)),
        ],
        None,
    );
    let page = db
        .read_blocking(|c| {
            phoebis_ledger::list_routing_log(c, &log_filter(), Direction::Desc, None, 25)
        })
        .unwrap();
    let facts: Vec<CompletionFacts> = page
        .rows
        .iter()
        .map(|row| match row {
            phoebis_ledger::RoutingLogRow::Decision(d) => (
                format!("{}/{}", d.request_id, d.seq),
                d.attempt,
                d.attempt_count,
                d.status,
                d.cache.clone(),
            ),
            phoebis_ledger::RoutingLogRow::Event(_) => panic!("no event was written"),
        })
        .collect();
    let cache = |hit: bool, read: u64, promised: Option<u64>| {
        Some(phoebis_ledger::CacheFacts {
            hit,
            read_tokens: read,
            promised_tokens: promised,
            input_tokens: 1_000,
        })
    };
    assert_eq!(
        facts,
        [
            ("r3/2".to_string(), 2, Some(2), Some(200), cache(true, 300, None)),
            ("r3/1".to_string(), 1, Some(2), Some(429), None),
            ("r2/2".to_string(), 1, Some(1), Some(200), cache(false, 0, None)),
            ("r2/1".to_string(), 1, Some(1), None, None),
            (
                "r1/1".to_string(),
                1,
                Some(2),
                Some(200),
                cache(true, 600, Some(500))
            ),
        ],
        "a decision reads the newest attempt it dispatched before the next decision's, a hold reads none, and the \
         retry's row keeps the decision's own attempt"
    );
}

/// A decision row's identity, its account, and the answering attempt's status and cache read.
type AnsweredFacts = (String, Option<String>, Option<u16>, Option<u64>);

#[test]
fn a_decision_reads_only_its_own_account_s_answer_before_the_request_s_next_decision() {
    let (db, _dir) = common::temp_ledger();
    let on = |mut row: DecisionRow, account: &str| {
        row.account_id = Some(account.into());
        row.record.outcome.account_id = Some(account.into());
        row
    };
    let attempt_on = |account: &str, attempt: AttemptRecord| AttemptRecord {
        account_id: account.into(),
        ..attempt
    };
    // A credential failure on `a` fails over before the wire counter advances, so `b` is selected on the same
    // attempt and is the one that answers.
    put_decision(&db, &on(logged("r1", 1, 1, 100, "placed"), "a"));
    put_decision(&db, &on(logged("r1", 2, 1, 100, "placed"), "b"));
    put_request(
        &db,
        "r1",
        &[attempt_on("b", logged_attempt("r1", 1, 200, Some(400)))],
        None,
    );
    // The decision for `b` never reached the ledger: `a`'s interval runs on, and `b`'s answer is still not `a`'s.
    put_decision(&db, &on(logged("r2", 1, 1, 200, "placed"), "a"));
    put_request(
        &db,
        "r2",
        &[
            attempt_on("a", logged_attempt("r2", 1, 429, None)),
            attempt_on("b", logged_attempt("r2", 2, 200, Some(400))),
        ],
        None,
    );
    let page = db
        .read_blocking(|c| {
            phoebis_ledger::list_routing_log(c, &log_filter(), Direction::Desc, None, 25)
        })
        .unwrap();
    let listed: Vec<AnsweredFacts> = page
        .rows
        .iter()
        .map(|row| match row {
            phoebis_ledger::RoutingLogRow::Decision(d) => (
                format!("{}/{}", d.request_id, d.seq),
                d.account_id.clone(),
                d.status,
                d.cache.as_ref().map(|c| c.read_tokens),
            ),
            phoebis_ledger::RoutingLogRow::Event(_) => panic!("no event was written"),
        })
        .collect();
    assert_eq!(
        listed,
        [
            ("r2/1".to_string(), Some("a".to_string()), Some(429), None),
            ("r1/2".to_string(), Some("b".to_string()), Some(200), Some(400)),
            ("r1/1".to_string(), Some("a".to_string()), None, None),
        ],
        "a decision superseded on its own attempt reads no answer, and none reads another account's"
    );
    let completion = |request: &str, seq: u32| {
        let record = db
            .read_blocking(|c| phoebis_ledger::decision_record(c, request, seq))
            .unwrap()
            .expect("the decision is stored");
        let completion = &record["outcome"]["completion"];
        (
            completion["status"].clone(),
            completion["cache_read_tokens"].clone(),
        )
    };
    assert_eq!(
        completion("r1", 1),
        (serde_json::Value::Null, serde_json::Value::Null),
        "the record of the account never contacted carries no answer"
    );
    assert_eq!(
        completion("r1", 2),
        (serde_json::json!(200), serde_json::json!(400))
    );
    assert_eq!(
        completion("r2", 1),
        (serde_json::json!(429), serde_json::Value::Null)
    );
}

#[test]
fn a_record_read_decodes_the_frame_and_composes_the_completion() {
    let (db, _dir) = common::temp_ledger();
    let mut placed = logged("r1", 1, 1, 100, "placed");
    let rung = phoebis_routing::Placement {
        rung: "content",
        depth: 2,
        prefix_tokens: 500,
    };
    placed.summary.rung = Some(rung.clone());
    placed.record.outcome.rung = Some(rung);
    put_decision(&db, &placed);
    let entries = serde_json::json!([
        {"tier": "message", "depth": 2, "prefix_tokens": 500, "ttl_ms": 300_000, "account_id": "a"}
    ]);
    db.write(|c| {
        c.execute(
            "UPDATE decisions SET cache_map = ?1 WHERE request_id = 'r1' AND seq = 1",
            [entries.to_string()],
        )?;
        Ok(())
    })
    .unwrap();
    put_request(
        &db,
        "r1",
        &[
            logged_attempt("r1", 1, 429, None),
            AttemptRecord {
                cache_write_5m_tokens: Some(100),
                cache_write_1h_tokens: Some(27_246),
                ..logged_attempt("r1", 2, 200, Some(600))
            },
        ],
        Some(r#"{"waited_on":["a"],"cohort":null,"held_ms":1250}"#),
    );
    put_decision(&db, &logged("r2", 1, 1, 200, "held"));
    let read = |request: &str, seq: u32| {
        db.read_blocking(|c| phoebis_ledger::decision_record(c, request, seq))
            .unwrap()
    };
    let record = read("r1", 1).expect("the decision is stored");
    let mut expected = serde_json::to_value(&placed.record).unwrap();
    expected["outcome"]["completion"] = serde_json::json!({
        "status": 200,
        "cache_read_tokens": 600,
        "input_tokens": 1_000,
        "cache_write_tokens": 27_346,
        "promised_tokens": 500,
        "hit": true,
        "held_ms": 1_250,
        "freed": true,
        "cache_map_written": entries,
        "completed_at": NOW + 2_000,
        "input_transformations": null,
    });
    let mut answer = serde_json::Map::new();
    answer.insert("request_id".into(), "r1".into());
    answer.insert("seq".into(), 1.into());
    answer.extend(expected.as_object().unwrap().clone());
    assert_eq!(
        record,
        serde_json::Value::Object(answer),
        "the frame decoded, its completion the answering attempt's after the retry in place"
    );
    assert_eq!(
        read("r2", 1).expect("the hold is stored")["outcome"]["completion"],
        serde_json::Value::Null,
        "a hold dispatched nothing"
    );
    assert_eq!(read("r1", 9), None, "a seq the request never reached");
    assert_eq!(read("r9", 1), None, "a request never written");
}

#[test]
fn a_record_frame_naming_a_size_past_the_reader_s_bound_is_refused_unread() {
    let (db, _dir) = common::temp_ledger();
    put_decision(&db, &logged("r1", 1, 1, 100, "placed"));
    // A record of one long string folds to a few kilobytes, so a small frame names a size no record reaches. It is
    // valid JSON, so only the bound refuses it.
    let record = format!(r#"{{"version":1,"pad":"{}"}}"#, "a".repeat(64 << 20));
    let frame = zstd::bulk::compress(record.as_bytes(), 0).unwrap();
    db.write(|c| {
        c.execute(
            "UPDATE decisions SET record = ?1 WHERE request_id = 'r1' AND seq = 1",
            [frame],
        )?;
        Ok(())
    })
    .unwrap();
    assert!(
        db.read_blocking(|c| phoebis_ledger::decision_record(c, "r1", 1))
            .is_err(),
        "the header's size is checked against the bound before any output is sized"
    );
}

/// A retried leg that answered twice carries one estimate per answered attempt, each at the set its own prompt
/// selects, and the hour's rollup files each attempt under its own band, whose counts price at that set.
#[test]
fn a_request_with_two_answered_attempts_books_each_at_the_set_its_own_prompt_selects() {
    // The test table's Haiku 5.5 row, per million: input 1 and output 5 up to 1,000 prompt tokens, 5 and 25 over.
    let revisions = phoebis_ledger::test_tables::api_prices();
    let table = revisions.in_force(phoebis_ledger::test_tables::JANUARY_2026);
    let (db, _dir) = common::temp_ledger();
    let attempt = |attempt: u32, input: u64| AttemptRecord {
        request_id: "r1".into(),
        attempt,
        account_id: "a".into(),
        account_kind: AccountKind::ApiKey,
        outcome: "succeeded".into(),
        input_tokens: Some(input),
        output_tokens: Some(100),
        ..Default::default()
    };
    let attempts = vec![attempt(1, 1_000), attempt(2, 1_001)];
    let estimate = |input: u64| {
        table
            .estimate(
                "claude-haiku-5-5",
                Speed::Standard,
                phoebis_routing::PinUsage {
                    input,
                    cache_read: 0,
                    write_5m: 0,
                    write_1h: 0,
                    output: 100,
                },
            )
            .expect("the test list carries the model")
    };
    let close = |a: f64, b: f64| (a - b).abs() < 1e-12;
    let (under, over) = (estimate(1_000), estimate(1_001));
    assert!(
        close(under.input_usd, 1_000e-6) && close(under.output_usd, 500e-6),
        "{under:?}"
    );
    assert!(
        close(over.input_usd, 5_005e-6) && close(over.output_usd, 2_500e-6),
        "{over:?}"
    );
    assert!(close(under.total() + over.total(), 9_005e-6));
    let request = RequestRecord {
        id: "r1".into(),
        received_at: 1_000_000,
        key_id: Some("desktop".into()),
        pool_id: Some("p".into()),
        canonical_model: "claude-haiku-5-5".into(),
        model: "claude-haiku-5-5".into(),
        kind: "inference".into(),
        origin: "client".into(),
        outcome: "succeeded".into(),
        attempts: 2,
        ..Default::default()
    };
    db.insert_request_at(&request, &attempts, None, &revisions)
        .unwrap();
    let rows = db
        .read_blocking(|c| read_rollups(c, 0, i64::MAX, &RollupSelect::default()))
        .unwrap();
    // One row per band, each holding its own attempt's counts, which price at the cells its band names.
    assert_eq!(
        rows.iter()
            .map(|r| (r.band.as_str(), r.attempts, r.input_tokens))
            .collect::<Vec<_>>(),
        [("long", 1, 1_001), ("standard", 1, 1_000)]
    );
    let live = LiveKeys(std::collections::BTreeMap::from([(
        "desktop".to_string(),
        0,
    )]));
    let groups = breakdown(
        &apply_filter(rows, &RollupFilter::default()),
        phoebis_ledger::GroupBy::Model,
        false,
        &live,
        &[],
        &revisions,
        &PlanTables::shipped(),
        NOW,
    );
    let group = groups
        .iter()
        .find(|g| g.key == "claude-haiku-5-5")
        .expect("one model group");
    assert!(
        (group.cost.input - 6_005e-6).abs() < 1e-12 && (group.cost.output - 3_000e-6).abs() < 1e-12,
        "{:?}",
        group.cost
    );
    assert!(
        (group.cost_usd - 9_005e-6).abs() < 1e-12,
        "the band rows' counts price to both attempts' dollars: {}",
        group.cost_usd
    );
}
