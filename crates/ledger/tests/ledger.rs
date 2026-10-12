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
    Outcome as BusOutcome, Phase, RequestComplete, RequestObserved, RequestShape, ResponseChunk,
    ResponseFacts, Usage, BODY_SHAPE_ERROR_CODE,
};
use phoebis_ledger::{
    apply_filter, breakdown, list_requests, member_spends, parked_count, parked_summary,
    read_rollups, run_retention, Assembler, AttemptRecord, AttentionGauges, BodyRecord,
    BudgetedMember, CacheMapWrite, DecisionRow, EventRecord, LedgerConfig, LedgerDb, LedgerError,
    LiveKeys, ParkedSummary, PlanTables, PriceRevisions, Record, Replayed, RequestFilter,
    RequestRecord, RollupFilter, RollupRow, RollupSelect, Speed, BATCH_RETRY_DELAYS,
    BATCH_RETRY_HOLD, LEDGER_BEHIND_DEPTH, UNATTRIBUTED,
};

/// A rollup row's dollars, its counts priced through `price_row` at its own key on `prices`.
fn row_usd(prices: &PriceRevisions, row: &RollupRow) -> f64 {
    let key = phoebis_ledger::RowKey {
        model: &row.canonical_model,
        speed: Speed::of(Some(row.speed.as_str())),
        band: phoebis_ledger::Band::of_rollup(&row.band),
        funding: phoebis_ledger::Funding::of_rollup(&row.funding),
        kind: &row.kind,
        tier: (row.tier != UNATTRIBUTED).then_some(row.tier.as_str()),
        at: row.hour * 1_000,
        band_at: None,
    };
    phoebis_ledger::price_row(prices, &PlanTables::shipped(), &key, row.usage())
        .usd
        .map_or(0.0, |usd| usd.total())
}
use phoebis_profile::fixtures::BodyFixture;
use phoebis_store::{BudgetPeriod, MemberWrite, StorageKey, Store};
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
        h.db.read_blocking(|c| list_requests(c, &filter, &LiveKeys::default(), None, 10))
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
    let db = LedgerDb::open_in_memory().unwrap();
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
    let db = LedgerDb::open_in_memory().unwrap();
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
            account_kind: "oauth".into(),
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
            .read_blocking(|c| list_requests(c, &filter, &LiveKeys::default(), None, 10))
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
        diagnostics: None,
        message_id: None,
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

/// A ledger holding `matching` rows under one model, a millisecond apart, and one newer row under another model,
/// so a page reads the model filter rather than the whole table.
fn paging_ledger(matching: usize) -> phoebis_ledger::LedgerDb {
    let db = phoebis_ledger::LedgerDb::open_in_memory().unwrap();
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
    db
}

fn sonnet_page(
    db: &phoebis_ledger::LedgerDb,
    cursor: Option<&str>,
    limit: usize,
) -> (Vec<String>, Option<String>) {
    let filter = RequestFilter {
        models: vec!["claude-sonnet-5".into()],
        ..Default::default()
    };
    let (rows, next) = db
        .read_blocking(|c| list_requests(c, &filter, &LiveKeys::default(), cursor, limit))
        .unwrap();
    (rows.into_iter().map(|(r, _)| r.id).collect(), next)
}

#[test]
fn a_page_of_exactly_limit_rows_answers_no_cursor() {
    let db = paging_ledger(5);
    let (ids, next) = sonnet_page(&db, None, 5);
    assert_eq!(ids, ["r04", "r03", "r02", "r01", "r00"]);
    assert_eq!(
        next, None,
        "no row lies beyond the page, so there is no next page to offer"
    );
}

#[test]
fn a_page_with_one_row_beyond_it_answers_a_cursor_whose_next_page_holds_that_row() {
    let db = paging_ledger(5);
    let (ids, next) = sonnet_page(&db, None, 4);
    assert_eq!(ids, ["r04", "r03", "r02", "r01"]);
    assert_eq!(
        next.as_deref(),
        Some("1:r01"),
        "the cursor names the last row of the page, never the row beyond it"
    );
    let (ids, next) = sonnet_page(&db, next.as_deref(), 4);
    assert_eq!(
        ids,
        ["r00"],
        "the next page holds the one row beyond the first"
    );
    assert_eq!(next, None);
}

/// A fast request the router refused before any attempt served lists under the speed it asked for, and one that
/// sent any other text as its speed under `standard`, as the rollups file them.
#[test]
fn a_fast_request_refused_before_any_attempt_is_listed_under_the_fast_speed_filter() {
    let db = phoebis_ledger::LedgerDb::open_in_memory().unwrap();
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
            .read_blocking(|c| list_requests(c, &filter, &LiveKeys::default(), None, 10))
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
    let db = phoebis_ledger::LedgerDb::open_in_memory().unwrap();
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
            account_kind: "oauth".into(),
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
            .read_blocking(|c| list_requests(c, &filter, &LiveKeys::default(), None, 10))
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
            account_kind: "oauth".into(),
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
        .read_blocking(|c| list_requests(c, filter, &LiveKeys::default(), None, 10))
        .unwrap();
    rows.into_iter().map(|(r, _)| r.id).collect()
}

#[test]
fn the_cache_miss_reason_filter_reads_the_serving_attempt_alone_and_passes_any_name() {
    let db = LedgerDb::open_in_memory().unwrap();
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

#[test]
fn the_account_filter_reads_the_serving_attempt_as_the_rollups_file_it() {
    let db = LedgerDb::open_in_memory().unwrap();
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
    let db = LedgerDb::open_in_memory().unwrap();
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
        // The four columns migration 18 adds, the five migration 19 adds, migration 20's column and table, the
        // columns migrations 21, 25, 26, 30, 31 and 32 add, migration 27's table and index and migration 29's table,
        // columns and indexes, each dropped back out so the replay below has to add them again. `rollups_hourly`
        // keeps the shape the class dimension joins, since a primary-key column cannot be dropped in place and the
        // rebuild names its own columns regardless of what the source table still carries.
        db.write(|c| {
            c.execute_batch(VERSION_33_ROLLUPS)?;
            c.execute_batch(VERSION_32_ATTEMPTS)?;
            c.execute_batch(
                "ALTER TABLE attempts DROP COLUMN written_at; \
                 ALTER TABLE requests DROP COLUMN diagnostics_source; \
                 ALTER TABLE requests DROP COLUMN stream_end_at; \
                 ALTER TABLE rollups_hourly DROP COLUMN probes_held; \
                 ALTER TABLE requests DROP COLUMN request_class; \
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
                 ALTER TABLE attempts DROP COLUMN upstream_message_id; \
                 DROP TABLE parked_records; DROP INDEX IF EXISTS bodies_expires; \
                 DROP TABLE decisions; DROP INDEX IF EXISTS events_account; \
                 DROP INDEX IF EXISTS events_session; DROP INDEX IF EXISTS events_request; \
                 ALTER TABLE events DROP COLUMN request_id; ALTER TABLE events DROP COLUMN agent_id; \
                 ALTER TABLE events DROP COLUMN pool_id;",
            )?;
            Ok(())
        })
        .unwrap();
        let columns = pre_class_columns();
        db.write(|c| {
            c.execute(
                &format!(
                    "INSERT INTO rollups_hourly ({columns}) VALUES \
                     (3600, 'k', 0, 'p', 'a', 'm', 'inference', 'ua/1', 'claude-code/cli', 'none', 5, 5, 0, 0, \
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
    assert_eq!(db.schema_version().await.unwrap(), 34);
    let rows = db
        .read_blocking(|c| {
            phoebis_ledger::read_rollups(c, 0, i64::MAX, &phoebis_ledger::RollupSelect::default())
        })
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
                user_agent: Some("ua/1".into()),
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
            &PriceRevisions::shipped(),
        )?;
        tx.commit()?;
        Ok(())
    })
    .unwrap();
    let rows = db
        .read_blocking(|c| {
            phoebis_ledger::read_rollups(c, 0, i64::MAX, &phoebis_ledger::RollupSelect::default())
        })
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
            c.execute_batch(VERSION_33_ROLLUPS)?;
            c.execute_batch(VERSION_32_ATTEMPTS)?;
            c.execute_batch(
                "ALTER TABLE attempts DROP COLUMN written_at; \
                 ALTER TABLE requests DROP COLUMN diagnostics_source; \
                 ALTER TABLE requests DROP COLUMN stream_end_at; \
                 ALTER TABLE rollups_hourly DROP COLUMN probes_held; \
                 ALTER TABLE requests DROP COLUMN identified_by; \
                 ALTER TABLE requests DROP COLUMN identity_root; \
                 ALTER TABLE requests DROP COLUMN identity_depth; \
                 ALTER TABLE requests DROP COLUMN forked_from; \
                 ALTER TABLE requests DROP COLUMN fork_depth; \
                 ALTER TABLE quota_samples DROP COLUMN observed_at; \
                 ALTER TABLE requests DROP COLUMN auto_mode; \
                 ALTER TABLE requests DROP COLUMN held; \
                 ALTER TABLE attempts DROP COLUMN upstream_message_id; \
                 DROP TABLE parked_records; DROP INDEX IF EXISTS bodies_expires; \
                 DROP TABLE decisions; DROP INDEX IF EXISTS events_account; \
                 DROP INDEX IF EXISTS events_session; DROP INDEX IF EXISTS events_request; \
                 ALTER TABLE events DROP COLUMN request_id; ALTER TABLE events DROP COLUMN agent_id; \
                 ALTER TABLE events DROP COLUMN pool_id; \
                 UPDATE meta SET value = '18' WHERE key = 'schema_version';",
            )?;
            Ok(())
        })
        .unwrap();
    }
    let db = std::sync::Arc::new(phoebis_ledger::LedgerDb::open(&path).unwrap());
    assert_eq!(db.schema_version().await.unwrap(), 34);
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

/// The attempts table as version 32 held it, restored over a file the current schema built: the tier out and the six
/// columns migration 33 drops back, so a replay from an earlier version runs migration 33 over the table it was
/// written for.
const VERSION_32_ATTEMPTS: &str = "ALTER TABLE attempts DROP COLUMN tier; \
     ALTER TABLE attempts ADD COLUMN estimated_usd REAL; ALTER TABLE attempts ADD COLUMN pricing_version TEXT; \
     ALTER TABLE attempts ADD COLUMN input_usd REAL; ALTER TABLE attempts ADD COLUMN cache_read_usd REAL; \
     ALTER TABLE attempts ADD COLUMN cache_write_usd REAL; ALTER TABLE attempts ADD COLUMN output_usd REAL;";

/// The table migration 23 drops, recreated so a forward replay from version 20 or 21 can drop it again.
const RECREATE_DROPPED_TABLE: &str = "CREATE TABLE window_reconciliations (account_id TEXT NOT NULL, window TEXT NOT NULL, window_end INTEGER NOT NULL, computed_at INTEGER NOT NULL, PRIMARY KEY (account_id, window, window_end)); -- rollback";

#[tokio::test]
async fn a_ledger_from_before_the_auto_mode_column_gains_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    {
        let db = phoebis_ledger::LedgerDb::open(&path).unwrap();
        db.write(|c| {
            c.execute_batch(VERSION_33_ROLLUPS)?;
            c.execute_batch(VERSION_32_ATTEMPTS)?;
            c.execute_batch(&format!(
                "ALTER TABLE attempts DROP COLUMN written_at; \
                 ALTER TABLE requests DROP COLUMN diagnostics_source; \
                 ALTER TABLE requests DROP COLUMN stream_end_at; \
                 ALTER TABLE rollups_hourly DROP COLUMN probes_held; \
                 ALTER TABLE requests DROP COLUMN auto_mode; ALTER TABLE requests DROP COLUMN held; \
                 ALTER TABLE attempts DROP COLUMN upstream_message_id; \
                 {RECREATE_DROPPED_TABLE}\n\
                 DROP TABLE parked_records; DROP INDEX IF EXISTS bodies_expires; \
                 DROP TABLE decisions; DROP INDEX IF EXISTS events_account; \
                 DROP INDEX IF EXISTS events_session; DROP INDEX IF EXISTS events_request; \
                 ALTER TABLE events DROP COLUMN request_id; ALTER TABLE events DROP COLUMN agent_id; \
                 ALTER TABLE events DROP COLUMN pool_id; \
                 UPDATE meta SET value = '20' WHERE key = 'schema_version';"
            ))?;
            Ok(())
        })
        .unwrap();
    }
    let db = std::sync::Arc::new(phoebis_ledger::LedgerDb::open(&path).unwrap());
    assert_eq!(db.schema_version().await.unwrap(), 34);
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
            c.execute_batch(VERSION_33_ROLLUPS)?;
            let columns: String = c.query_row(
                "SELECT group_concat(name, ', ') FROM pragma_table_info('rollups_hourly') WHERE name != 'speed'",
                [],
                |r| r.get(0),
            )?;
            c.execute_batch(VERSION_32_ATTEMPTS)?;
            c.execute_batch(&format!(
                "CREATE TABLE rollups_hourly_prev AS SELECT {columns} FROM rollups_hourly; \
                 DROP TABLE rollups_hourly; \
                 ALTER TABLE rollups_hourly_prev RENAME TO rollups_hourly; \
                 ALTER TABLE attempts DROP COLUMN written_at; \
                 ALTER TABLE requests DROP COLUMN diagnostics_source; \
                 ALTER TABLE requests DROP COLUMN stream_end_at; \
                 ALTER TABLE requests DROP COLUMN held; \
                 ALTER TABLE attempts DROP COLUMN upstream_message_id; {RECREATE_DROPPED_TABLE}\n\
                 DROP TABLE parked_records; DROP INDEX IF EXISTS bodies_expires; \
                 DROP TABLE decisions; DROP INDEX IF EXISTS events_account; \
                 DROP INDEX IF EXISTS events_session; DROP INDEX IF EXISTS events_request; \
                 ALTER TABLE events DROP COLUMN request_id; ALTER TABLE events DROP COLUMN agent_id; \
                 ALTER TABLE events DROP COLUMN pool_id; \
                 UPDATE meta SET value = '21' WHERE key = 'schema_version';"
            ))?;
            Ok(())
        })
        .unwrap();
    }
    let db = std::sync::Arc::new(phoebis_ledger::LedgerDb::open(&path).unwrap());
    assert_eq!(db.schema_version().await.unwrap(), 34);
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
            c.execute_batch(VERSION_33_ROLLUPS)?;
            let columns: String = c.query_row(
                "SELECT group_concat(name, ', ') FROM pragma_table_info('rollups_hourly') WHERE name != 'funding'",
                [],
                |r| r.get(0),
            )?;
            c.execute_batch(VERSION_32_ATTEMPTS)?;
            c.execute_batch(&format!(
                "CREATE TABLE rollups_hourly_prev AS SELECT {columns} FROM rollups_hourly; \
                 DROP TABLE rollups_hourly; \
                 ALTER TABLE rollups_hourly_prev RENAME TO rollups_hourly; \
                 ALTER TABLE attempts DROP COLUMN written_at; \
                 ALTER TABLE requests DROP COLUMN diagnostics_source; \
                 ALTER TABLE requests DROP COLUMN stream_end_at; \
                 ALTER TABLE requests DROP COLUMN held; \
                 ALTER TABLE attempts DROP COLUMN upstream_message_id; \
                 DROP TABLE parked_records; DROP INDEX IF EXISTS bodies_expires; \
                 DROP TABLE decisions; DROP INDEX IF EXISTS events_account; \
                 DROP INDEX IF EXISTS events_session; DROP INDEX IF EXISTS events_request; \
                 ALTER TABLE events DROP COLUMN request_id; ALTER TABLE events DROP COLUMN agent_id; \
                 ALTER TABLE events DROP COLUMN pool_id; \
                 UPDATE meta SET value = '23' WHERE key = 'schema_version';"
            ))?;
            Ok(())
        })
        .unwrap();
    }
    let db = std::sync::Arc::new(phoebis_ledger::LedgerDb::open(&path).unwrap());
    assert_eq!(db.schema_version().await.unwrap(), 34);
    let mut rows = db
        .read_blocking(|c| {
            phoebis_ledger::read_rollups(c, 0, i64::MAX, &phoebis_ledger::RollupSelect::default())
        })
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
            c.execute_batch(VERSION_33_ROLLUPS)?;
            c.execute_batch(VERSION_32_ATTEMPTS)?;
            c.execute_batch(
                "ALTER TABLE attempts DROP COLUMN written_at; \
                 ALTER TABLE requests DROP COLUMN diagnostics_source; \
                 ALTER TABLE requests DROP COLUMN stream_end_at; \
                 ALTER TABLE rollups_hourly DROP COLUMN probes_held; \
                 ALTER TABLE requests DROP COLUMN held; \
                 ALTER TABLE attempts DROP COLUMN upstream_message_id; \
                 ALTER TABLE rollups_hourly DROP COLUMN incomplete; \
                 DROP TABLE parked_records; DROP INDEX IF EXISTS bodies_expires; \
                 DROP TABLE decisions; DROP INDEX IF EXISTS events_account; \
                 DROP INDEX IF EXISTS events_session; DROP INDEX IF EXISTS events_request; \
                 ALTER TABLE events DROP COLUMN request_id; ALTER TABLE events DROP COLUMN agent_id; \
                 ALTER TABLE events DROP COLUMN pool_id; \
                 UPDATE meta SET value = '24' WHERE key = 'schema_version';",
            )?;
            Ok(())
        })
        .unwrap();
    }
    let db = std::sync::Arc::new(phoebis_ledger::LedgerDb::open(&path).unwrap());
    assert_eq!(db.schema_version().await.unwrap(), 34);
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
            c.execute_batch(VERSION_33_ROLLUPS)?;
            c.execute_batch(VERSION_32_ATTEMPTS)?;
            c.execute_batch(
                "ALTER TABLE attempts DROP COLUMN written_at; \
                 ALTER TABLE requests DROP COLUMN diagnostics_source; \
                 ALTER TABLE requests DROP COLUMN stream_end_at; \
                 ALTER TABLE rollups_hourly DROP COLUMN probes_held; \
                 ALTER TABLE rollups_hourly DROP COLUMN incomplete; \
                 ALTER TABLE attempts DROP COLUMN upstream_message_id; \
                 DROP TABLE parked_records; DROP INDEX IF EXISTS bodies_expires; \
                 DROP TABLE decisions; DROP INDEX IF EXISTS events_account; \
                 DROP INDEX IF EXISTS events_session; DROP INDEX IF EXISTS events_request; \
                 ALTER TABLE events DROP COLUMN request_id; ALTER TABLE events DROP COLUMN agent_id; \
                 ALTER TABLE events DROP COLUMN pool_id; \
                 UPDATE meta SET value = '25' WHERE key = 'schema_version';",
            )?;
            Ok(())
        })
        .unwrap();
    }
    let db = std::sync::Arc::new(phoebis_ledger::LedgerDb::open(&path).unwrap());
    assert_eq!(db.schema_version().await.unwrap(), 34);
    let rows = db
        .read_blocking(|c| {
            phoebis_ledger::read_rollups(c, 0, i64::MAX, &phoebis_ledger::RollupSelect::default())
        })
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

#[tokio::test]
async fn migration_twenty_eight_spells_every_quota_window_by_the_endpoints_key_and_keeps_the_head_row_of_a_collision(
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    {
        let db = phoebis_ledger::LedgerDb::open(&path).unwrap();
        // Before 28 the head wrote `5h`, `7d` and `7d_oi` and the poll the endpoint's keys plus `fable`, so a head
        // and a poll of one bucket could each hold a row for one window; the top-level `seven_day_overage_included`
        // and `seven_day_fable` rows stand for a poll key the captured bodies never carried.
        db.write(|c| {
            c.execute_batch(VERSION_33_ROLLUPS)?;
            c.execute_batch(VERSION_32_ATTEMPTS)?;
            c.execute_batch(
                "ALTER TABLE attempts DROP COLUMN written_at; \
                 ALTER TABLE requests DROP COLUMN diagnostics_source; \
                 ALTER TABLE requests DROP COLUMN stream_end_at; \
                 ALTER TABLE rollups_hourly DROP COLUMN probes_held; \
                 INSERT INTO quota_samples (account_id, bucket, window, utilization, reset_at, source, observed_at) VALUES \
                   ('a', 0, '5h', 0.5, NULL, 'head', 10), \
                   ('a', 0, 'five_hour', 0.4, NULL, 'poll', 20), \
                   ('a', 0, '7d', 0.2, NULL, 'head', 10), \
                   ('a', 0, '7d_oi', 0.9, NULL, 'head', 10), \
                   ('a', 0, 'seven_day_overage_included', 0.8, NULL, 'poll', 20), \
                   ('a', 0, 'fable', 0.42, NULL, 'poll', 20), \
                   ('a', 0, 'seven_day_fable', 0.5, NULL, 'poll', 20), \
                   ('a', 0, 'seven_day_opus', 0.03, NULL, 'poll', 20), \
                   ('a', 300, 'five_hour', 0.3, NULL, 'poll', 320); \
                 DROP TABLE decisions; DROP INDEX IF EXISTS events_account; \
                 DROP INDEX IF EXISTS events_session; DROP INDEX IF EXISTS events_request; \
                 ALTER TABLE events DROP COLUMN request_id; ALTER TABLE events DROP COLUMN agent_id; \
                 ALTER TABLE events DROP COLUMN pool_id; \
                 UPDATE meta SET value = '27' WHERE key = 'schema_version';",
            )?;
            Ok(())
        })
        .unwrap();
    }
    let db = std::sync::Arc::new(phoebis_ledger::LedgerDb::open(&path).unwrap());
    assert_eq!(db.schema_version().await.unwrap(), 34);
    let rows: Vec<(i64, String, Option<f64>, String)> = db
        .quota_samples("a", 0, i64::MAX)
        .await
        .unwrap()
        .into_iter()
        .map(|q| (q.bucket, q.window, q.utilization, q.source))
        .collect();
    let rows: Vec<(i64, &str, Option<f64>, &str)> = rows
        .iter()
        .map(|(bucket, window, utilization, source)| {
            (*bucket, window.as_str(), *utilization, source.as_str())
        })
        .collect();
    assert_eq!(
        rows,
        [
            (0, "five_hour", Some(0.5), "head"),
            (300, "five_hour", Some(0.3), "poll"),
            (0, "seven_day", Some(0.2), "head"),
            (0, "seven_day_fable", Some(0.42), "poll"),
            (0, "seven_day_opus", Some(0.03), "poll"),
            (0, "seven_day_overage_included", Some(0.9), "head"),
        ],
        "each window reads under the endpoint's key, a colliding pair keeps the row the old spelling named, and a \
         poll row with nothing to collide with stays"
    );
}

#[tokio::test]
async fn a_ledger_below_the_held_column_migrates_and_a_fresh_file_carries_it() {
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
            c.execute_batch(VERSION_33_ROLLUPS)?;
            c.execute_batch(VERSION_32_ATTEMPTS)?;
            c.execute_batch(
                "ALTER TABLE attempts DROP COLUMN written_at; \
                 ALTER TABLE requests DROP COLUMN diagnostics_source; \
                 ALTER TABLE requests DROP COLUMN stream_end_at; \
                 ALTER TABLE rollups_hourly DROP COLUMN probes_held; \
                 UPDATE meta SET value = '29' WHERE key = 'schema_version';",
            )?;
            Ok(())
        })
        .unwrap();
    }
    let db = std::sync::Arc::new(phoebis_ledger::LedgerDb::open(&path).unwrap());
    assert_eq!(db.schema_version().await.unwrap(), 34);
    let rows = db
        .read_blocking(|c| {
            phoebis_ledger::read_rollups(c, 0, i64::MAX, &phoebis_ledger::RollupSelect::default())
        })
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| (r.requests, r.succeeded, r.probes_held))
            .collect::<Vec<_>>(),
        vec![(1, 1, 0)],
        "a rollup row from before the column keeps its counts and counts no held probe"
    );
    let held_columns = |ledger: &phoebis_ledger::LedgerDb| {
        ledger
            .read_blocking(|c| {
                Ok(c.query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('rollups_hourly') WHERE name = 'probes_held'",
                    [],
                    |r| r.get::<_, i64>(0),
                )?)
            })
            .unwrap()
    };
    assert_eq!(held_columns(&db), 1, "the migration added the column");
    assert_eq!(
        held_columns(&phoebis_ledger::LedgerDb::open_in_memory().unwrap()),
        1,
        "a fresh file carries it"
    );
}

#[tokio::test]
async fn a_ledger_below_the_phase_columns_migrates_and_a_fresh_file_carries_them() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    {
        let db = phoebis_ledger::LedgerDb::open(&path).unwrap();
        db.insert_request(
            &RequestRecord {
                id: "r0".into(),
                received_at: 3_600_000,
                finished_at: Some(3_601_000),
                key_id: Some("desktop".into()),
                pool_id: Some("main".into()),
                canonical_model: "claude-sonnet-5".into(),
                model: "claude-sonnet-5".into(),
                kind: "inference".into(),
                origin: "client".into(),
                outcome: "succeeded".into(),
                attempts: 1,
                stream_end_at: Some(3_600_900),
                ..Default::default()
            },
            &[AttemptRecord {
                request_id: "r0".into(),
                attempt: 1,
                account_id: "a".into(),
                account_kind: "oauth".into(),
                started_at: 3_600_010,
                outcome: "succeeded".into(),
                output_tokens: Some(5),
                written_at: Some(3_600_040),
                ..Default::default()
            }],
            None,
        )
        .unwrap();
        db.write(|c| {
            c.execute_batch(VERSION_33_ROLLUPS)?;
            c.execute_batch(VERSION_32_ATTEMPTS)?;
            c.execute_batch(
                "ALTER TABLE attempts DROP COLUMN written_at; \
                 ALTER TABLE requests DROP COLUMN diagnostics_source; \
                 ALTER TABLE requests DROP COLUMN stream_end_at; \
                 UPDATE meta SET value = '30' WHERE key = 'schema_version';",
            )?;
            Ok(())
        })
        .unwrap();
    }
    let db = std::sync::Arc::new(phoebis_ledger::LedgerDb::open(&path).unwrap());
    assert_eq!(db.schema_version().await.unwrap(), 34);
    let (row, attempts) = db
        .read_blocking(|c| phoebis_ledger::request_by_id(c, "r0"))
        .unwrap()
        .expect("the row outlives the rewind");
    assert_eq!(
        (row.stream_end_at, attempts[0].written_at),
        (None, None),
        "the rewind dropped both instants, so the migrated file holds neither"
    );
    let phase_columns = |ledger: &phoebis_ledger::LedgerDb| {
        ledger
            .read_blocking(|c| {
                let named = |table: &str, column: &str| {
                    c.query_row(
                        &format!(
                            "SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = '{column}'"
                        ),
                        [],
                        |r| r.get::<_, i64>(0),
                    )
                };
                Ok((
                    named("attempts", "written_at")?,
                    named("requests", "stream_end_at")?,
                ))
            })
            .unwrap()
    };
    assert_eq!(
        phase_columns(&db),
        (1, 1),
        "the migration added both columns"
    );
    assert_eq!(
        phase_columns(&phoebis_ledger::LedgerDb::open_in_memory().unwrap()),
        (1, 1),
        "a fresh file carries them"
    );
}

#[tokio::test]
async fn a_requests_diagnostics_source_round_trips_and_a_ledger_at_version_31_reads_null() {
    let request = |id: &str, source: Option<&str>| RequestRecord {
        id: id.into(),
        received_at: 3_600_000,
        canonical_model: "claude-sonnet-5".into(),
        model: "claude-sonnet-5".into(),
        kind: "inference".into(),
        origin: "client".into(),
        outcome: "succeeded".into(),
        diagnostics_source: source.map(String::from),
        ..Default::default()
    };
    let db = Arc::new(LedgerDb::open_in_memory().unwrap());
    for (id, source) in [
        ("r-client", Some("client")),
        ("r-router", Some("router")),
        ("r-none", None),
    ] {
        db.insert_request(&request(id, source), &[], None).unwrap();
        let (row, _) = db.request(id).await.unwrap().unwrap();
        assert_eq!(row.diagnostics_source.as_deref(), source, "{id}");
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    {
        let db = LedgerDb::open(&path).unwrap();
        db.insert_request(&request("r0", Some("client")), &[], None)
            .unwrap();
        db.write(|c| {
            c.execute_batch(VERSION_33_ROLLUPS)?;
            c.execute_batch(VERSION_32_ATTEMPTS)?;
            c.execute_batch(
                "ALTER TABLE requests DROP COLUMN diagnostics_source; \
                 UPDATE meta SET value = '31' WHERE key = 'schema_version';",
            )?;
            Ok(())
        })
        .unwrap();
    }
    let db = Arc::new(LedgerDb::open(&path).unwrap());
    assert_eq!(db.schema_version().await.unwrap(), 34);
    let (row, _) = db.request("r0").await.unwrap().unwrap();
    assert_eq!(
        row.diagnostics_source, None,
        "the rewind dropped the source, so the migrated row holds none"
    );
}

/// What one query lists in its first column, in the query's order.
fn names(db: &phoebis_ledger::LedgerDb, sql: &str) -> Vec<String> {
    db.read_blocking(|c| {
        let mut stmt = c.prepare(sql)?;
        let names = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(names)
    })
    .unwrap()
}

#[tokio::test]
async fn migration_29_adds_the_decisions_table_and_the_events_columns_on_a_fresh_and_an_existing_file(
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let decision_indexes =
        "SELECT name FROM pragma_index_list('decisions') WHERE origin = 'c' ORDER BY name";
    let event_columns = "SELECT name FROM pragma_table_info('events') ORDER BY cid";
    let event_indexes = "SELECT name FROM pragma_index_list('events') ORDER BY name";
    {
        let fresh = Arc::new(LedgerDb::open(&path).unwrap());
        assert_eq!(fresh.schema_version().await.unwrap(), 34);
        assert_eq!(
            names(
                &fresh,
                "SELECT name FROM pragma_table_info('decisions') ORDER BY cid"
            ),
            [
                "request_id",
                "seq",
                "attempt",
                "at",
                "kind",
                "probe",
                "key_id",
                "pool_id",
                "account_id",
                "from_account_id",
                "session_id",
                "agent_id",
                "model",
                "summary",
                "record",
                "cache_map"
            ]
        );
        assert_eq!(
            names(&fresh, decision_indexes),
            [
                "decisions_account",
                "decisions_at",
                "decisions_key",
                "decisions_pool",
                "decisions_session"
            ]
        );
        // The indexes leave first, since SQLite refuses to drop an indexed column.
        fresh
            .write(|c| {
                c.execute_batch(VERSION_33_ROLLUPS)?;
                c.execute_batch(VERSION_32_ATTEMPTS)?;
                c.execute_batch(
                    "ALTER TABLE attempts DROP COLUMN written_at; \
                     ALTER TABLE requests DROP COLUMN diagnostics_source; \
                     ALTER TABLE requests DROP COLUMN stream_end_at; \
                     ALTER TABLE rollups_hourly DROP COLUMN probes_held; \
                     DROP TABLE decisions; DROP INDEX IF EXISTS events_account; \
                     DROP INDEX IF EXISTS events_session; DROP INDEX IF EXISTS events_request; \
                     ALTER TABLE events DROP COLUMN request_id; ALTER TABLE events DROP COLUMN agent_id; \
                     ALTER TABLE events DROP COLUMN pool_id; \
                     UPDATE meta SET value = '28' WHERE key = 'schema_version';",
                )?;
                Ok(())
            })
            .unwrap();
        assert_eq!(
            names(&fresh, event_columns).len(),
            8,
            "the rewound file's events table is back to its eight columns"
        );
    }
    let existing = Arc::new(LedgerDb::open(&path).unwrap());
    assert_eq!(existing.schema_version().await.unwrap(), 34);
    assert_eq!(
        names(&existing, event_columns),
        [
            "at",
            "kind",
            "account_id",
            "key_id",
            "session_id",
            "model",
            "actor",
            "detail",
            "pool_id",
            "agent_id",
            "request_id"
        ]
    );
    assert_eq!(
        names(&existing, event_indexes),
        [
            "events_account",
            "events_at",
            "events_kind_at",
            "events_request",
            "events_session"
        ]
    );
    assert_eq!(names(&existing, decision_indexes).len(), 5);
}

/// A decision whose every column reads a value of its own: a probe's move from `a` to `b` under routing session
/// `s1`. Built over a refusal's record, since the columns come off the request and outcome blocks alone.
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

type DecisionColumns = (
    u32,
    i64,
    String,
    bool,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

#[tokio::test]
async fn a_decision_lands_as_a_row_whose_frame_decodes_to_the_record() {
    use rusqlite::OptionalExtension as _;
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, &[oauth("a", A, 0)], (0, None), LedgerConfig::default()).await;
    let (summary, record) = decision("r1", 2, 2);
    h.ledger.record(Record::Decision(Box::new(DecisionRow::of(
        summary.clone(),
        record.clone(),
        NOW,
    ))));
    let (columns, stored_summary, frame) = wait_for(Duration::from_secs(5), || {
        h.db.read_blocking(|c| {
            Ok(c.query_row(
                "SELECT attempt, at, kind, probe, key_id, pool_id, account_id, from_account_id, session_id, \
                 agent_id, model, summary, record FROM decisions WHERE request_id = ?1 AND seq = ?2",
                rusqlite::params!["r1", 2],
                |r| {
                    let columns: DecisionColumns = (
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                        r.get(10)?,
                    );
                    Ok((columns, r.get::<_, String>(11)?, r.get::<_, Vec<u8>>(12)?))
                },
            )
            .optional()?)
        })
        .unwrap()
    })
    .await;
    let some = |value: &str| Some(value.to_string());
    assert_eq!(
        columns,
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
        serde_json::from_str::<serde_json::Value>(&stored_summary).unwrap(),
        serde_json::to_value(&summary).unwrap(),
        "the summary is stored as its JSON"
    );
    let decoded = zstd::decode_all(frame.as_slice()).expect("the record is one zstd frame");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&decoded).unwrap(),
        serde_json::to_value(&record).unwrap()
    );
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
    let db = LedgerDb::open_in_memory().unwrap();
    let (summary, mut record) = decision("r1", 1, 1);
    record.request.pool_id = String::new();
    put_decision(&db, &DecisionRow::of(summary, record, NOW));
    let pool_id: Option<String> = db
        .read_blocking(|c| {
            Ok(c.query_row(
                "SELECT pool_id FROM decisions WHERE request_id = 'r1'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(pool_id, None);
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
    let maps =
        h.db.read_blocking(|c| {
            let mut stmt = c.prepare(
                "SELECT request_id, seq, cache_map FROM decisions ORDER BY request_id, seq",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, u32>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap();
    assert_eq!(
        maps,
        [
            ("r1".to_string(), 1, None),
            (
                "r1".to_string(),
                2,
                Some(serde_json::to_string(&entries).unwrap())
            )
        ],
        "the map lands on the decision its seq names whatever attempt completed, and the stray write lands nowhere"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_request_through_the_relay_lands_its_decision_with_the_cache_map_its_answer_wrote() {
    use rusqlite::OptionalExtension as _;
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
    let (kind, account_id, cache_map) = wait_for(Duration::from_secs(5), || {
        h.db.read_blocking(|c| {
            Ok(c.query_row(
                "SELECT kind, account_id, cache_map FROM decisions WHERE request_id = ?1 AND seq = 1",
                rusqlite::params![id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?)
        })
        .unwrap()
        .filter(|(_, _, cache_map)| cache_map.is_some())
    })
    .await;
    assert_eq!(
        (kind.as_str(), account_id.as_deref()),
        ("placed", Some("a"))
    );
    assert!(
        serde_json::from_str::<serde_json::Value>(&cache_map.unwrap())
            .unwrap()
            .is_array(),
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
                &PlanTables::shipped(),
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
        .read_blocking(|c| {
            phoebis_ledger::read_rollups(c, 0, i64::MAX, &phoebis_ledger::RollupSelect::default())
        })
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
    let revisions = PriceRevisions::shipped();
    let table = revisions.in_force(NOW);
    let row = &table.models["claude-opus-5-5"];
    let fast = row
        .fast
        .as_ref()
        .expect("the shipped Opus 5.5 row carries fast cells");
    let (input, output) = (1_000_000_u64, 100_000_u64);
    let at = |cells: &phoebis_ledger::ModelPrice| {
        (input as f64 * cells.input + output as f64 * cells.output) / 1e6
    };
    let estimate = |speed: Speed| {
        table
            .estimate("claude-opus-5-5", speed, input, 0, 0, 0, output)
            .expect("the shipped list carries the model")
            .0
    };
    assert!(
        (estimate(Speed::Fast) - at(fast)).abs() < 1e-9,
        "the answer that reported speed fast prices at the fast cells"
    );
    assert!(
        (estimate(Speed::Standard) - at(&row.standard)).abs() < 1e-9,
        "the one that reported none at the standard cells"
    );
    assert!(at(fast) > at(&row.standard), "the two sets differ");
    let attempt = |attempt: u32, speed: Option<&str>| AttemptRecord {
        request_id: "r1".into(),
        attempt,
        account_id: "a".into(),
        account_kind: "api_key".into(),
        outcome: "succeeded".into(),
        input_tokens: Some(input),
        output_tokens: Some(output),
        speed: speed.map(String::from),
        ..Default::default()
    };
    let db = LedgerDb::open_in_memory().unwrap();
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
    let priced: f64 = rows.iter().map(|r| row_usd(&revisions, r)).sum();
    assert!(
        (priced - (at(fast) + at(&row.standard))).abs() < 1e-9,
        "the hour's rows price the fast attempt at the fast cells and the other at the standard ones: {priced}"
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

/// A profile read that names the Max 20x tier, as the quota poll stores it.
fn set_tier(store: &Store, account: &str, tier: &str) {
    store
        .set_profile(
            account,
            r#"{"account":{"uuid":"u","email":"a@example.invalid"},"organization":{}}"#,
            &phoebis_store::SubscriptionRow {
                plan: Some("max".into()),
                tier: Some(tier.into()),
                since: Some(NOW),
                trial_ends_at: None,
            },
        )
        .unwrap();
}

/// The landing writes the serving account's tier as the registry snapshot holds it on every attempt: the profile's
/// tier for an oauth account, none for an api_key account, and a tier that changes later lands on the new rows alone.
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
    let on = |id: &str, account: &str, kind: &str| Record::Request {
        request: finished_row(id),
        attempts: vec![AttemptRecord {
            account_id: account.into(),
            account_kind: kind.into(),
            ..serving_attempt(id, true)
        }],
        bodies: None,
    };
    h.ledger.record_request(on("r-oauth", "a", "oauth")).await;
    h.ledger.record_request(on("r-key", "b", "api_key")).await;
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
    h.ledger.record_request(on("r-later", "a", "oauth")).await;
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
    let db = phoebis_ledger::LedgerDb::open_in_memory().unwrap();
    plant_every_funding(&db, ("b", "api_key"));
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
        account_kind: "api_key".into(),
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
    Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap())
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

fn write_failures(h: &Harness) -> u64 {
    h.ledger.stats().write_failures.load(Ordering::SeqCst)
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
        (write_failures(h) == 1 && retrying(h) == 1).then_some(())
    })
    .await;
    for (n, delay) in BATCH_RETRY_DELAYS.into_iter().enumerate() {
        step_until(h, delay, || write_failures(h) >= n as u64 + 2).await;
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
        (write_failures(&h) == 6).then_some(())
    })
    .await;
    step(&h, BATCH_RETRY_HOLD - Duration::from_secs(1));
    stays_for(Duration::from_millis(400), || write_failures(&h) == 6).await;
    step(&h, Duration::from_secs(1));
    wait_for(Duration::from_secs(5), || {
        (write_failures(&h) == 7).then_some(())
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
        (write_failures(&h) == 6).then_some(())
    })
    .await;
    // The park was refused, so the record waits in the hold and is tried again after it.
    step_until(&h, BATCH_RETRY_HOLD, || write_failures(&h) == 7).await;
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
        h.db.bodies("poison").await.unwrap().is_none(),
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
        (write_failures(&h) == 1 && h.ledger.degraded()).then_some(())
    })
    .await;
    // Armed before the retry lands; nothing below reads through the ledger until the refresh has taken it.
    h.db.debug_fail_next_read();
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
        (write_failures(&h) == 6).then_some(())
    })
    .await;
    assert_eq!(h.ledger.dropped(), 0, "the held batch waits out its hold");
    h.ledger.clone().shutdown().await.unwrap();
    assert!(
        write_failures(&h) >= 7,
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
/// lands it charges it once.
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
    // A pass past the raw window: the request and attempt rows go, the rollups stay.
    let later = NOW + 91 * 86_400_000;
    run_retention(h.db.clone(), LedgerConfig::default(), later, Vec::new())
        .await
        .unwrap();
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
    let charged: i64 = h
        .db
        .read(|c| {
            Ok(c.query_row(
                "SELECT COALESCE(SUM(input_tokens), 0) FROM rollups_hourly WHERE pool_id = 'main' AND account_id = 'a'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(charged, 1_000_000, "the parked charge contributes once");
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
async fn charged_to_a(h: &Harness) -> i64 {
    h.db.read(|c| {
        Ok(c.query_row(
            "SELECT COALESCE(SUM(input_tokens), 0) FROM rollups_hourly WHERE pool_id = 'main' AND account_id = 'a'",
            [],
            |r| r.get(0),
        )?)
    })
    .await
    .unwrap()
}

async fn rows_of(h: &Harness, table: &'static str, column: &'static str, id: &'static str) -> i64 {
    h.db.read(move |c| {
        Ok(c.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE {column} = ?1"),
            [id],
            |r| r.get(0),
        )?)
    })
    .await
    .unwrap()
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
    assert_eq!(rows_of(&h, "requests", "id", "poison").await, 1);
    assert_eq!(rows_of(&h, "attempts", "request_id", "poison").await, 1);
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
    assert_eq!(rows_of(&h, "requests", "id", "poison").await, 1);
    assert_eq!(rows_of(&h, "attempts", "request_id", "poison").await, 1);
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
        (write_failures(h) == 6).then_some(())
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

/// The parked rows counted on the writer's connection, for a test that holds the reader.
fn parked_on_the_writer(h: &Harness) -> i64 {
    h.db.write(|c| Ok(c.query_row("SELECT COUNT(*) FROM parked_records", [], |r| r.get(0))?))
        .unwrap()
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
        (write_failures(&h) == 7).then_some(())
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
        (write_failures(&h) == 7).then_some(())
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
        (write_failures(&h) == 7).then_some(())
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
/// rows to a caller still waiting past it. The test holds the reader connection from before the claim until after the
/// deadline, so the writer has landed the row and waits on the count it answers with while the bound runs out.
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
    let (taken, held) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let reader = std::thread::spawn({
        let db = h.db.clone();
        move || {
            db.read_blocking(|_| {
                taken.send(()).unwrap();
                released.recv().unwrap();
                Ok(())
            })
            .unwrap()
        }
    });
    held.recv().unwrap();
    h.db.free_the_disk();
    // Halfway: the hold runs out, the batch lands, and the writer's next turn claims the replay and lands the row.
    step(&h, BOUND / 2);
    wait_for(Duration::from_secs(5), || {
        (parked_on_the_writer(&h) == 0).then_some(())
    })
    .await;
    step(&h, BOUND / 2 + Duration::from_secs(1));
    stays_for(Duration::from_millis(400), || !replay.is_finished()).await;
    release.send(()).unwrap();
    reader.join().unwrap();
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
    assert_eq!(rows_of(&h, "requests", "id", "parked").await, 1);
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
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    let db = Arc::new(LedgerDb::open_in_memory().unwrap());
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
            account_kind: "api_key".into(),
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

/// The registry and the ledger a previous run left: the member's last request spent past the monthly budget it
/// carries now, so the writer's first spend read at the next start crosses it.
async fn over_budget_after_a_run(up: &FakeUpstream) -> (Arc<Store>, Arc<LedgerDb>) {
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    let db = Arc::new(LedgerDb::open_in_memory().unwrap());
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
            account_kind: "api_key".into(),
            outcome: "succeeded".into(),
            input_tokens: Some(2_000_000),
            ..Default::default()
        }],
        None,
    )
    .unwrap();
    (store, db)
}

/// A replay the writer runs on its first turn, ahead of the start row the spawn queued, reads the members' spend and
/// leaves the budget rows it crosses for the next batch, so they land behind the start row. The writer takes either
/// first at random, so the case starts the router enough times that a replay landing the rows itself would show.
#[tokio::test]
async fn a_replay_on_the_writers_first_turn_lands_its_budget_row_behind_the_start_row() {
    let up = FakeUpstream::start(vec![]).await;
    for start in 0..30 {
        let (store, db) = over_budget_after_a_run(&up).await;
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
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    let db = Arc::new(LedgerDb::open_in_memory().unwrap());
    let accounts = [api_key("a", C, 0)];
    let run = || {
        harness_on(
            store.clone(),
            db.clone(),
            &up,
            &accounts,
            (0, None),
            LedgerConfig::default(),
            false,
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
            account_kind: "api_key".into(),
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
        account_kind: "oauth".into(),
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
        .read_blocking(|c| phoebis_ledger::list_routing_log(c, filter, None, 200))
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
    let db = LedgerDb::open_in_memory().unwrap();
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
    let db = LedgerDb::open_in_memory().unwrap();
    put_decision(&db, &logged("r1", 1, 1, 100, "held"));
    put_decision(&db, &logged("r1", 2, 1, 100, "placed"));
    put_decision(&db, &logged("r2", 1, 1, 100, "placed"));
    db.insert_event(&logged_event(100, "cooldown_set")).unwrap();
    db.insert_event(&logged_event(100, "cooldown_cleared"))
        .unwrap();
    let page = |cursor: Option<&phoebis_ledger::RoutingLogCursor>, limit: usize| {
        db.read_blocking(|c| phoebis_ledger::list_routing_log(c, &log_filter(), cursor, limit))
            .unwrap()
    };
    let whole = page(None, 5);
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
    let first = page(None, 4);
    assert_eq!(first.rows.len(), 4);
    let cursor = first.next_cursor.expect("a row lies past a page of four");
    let next = page(
        Some(&phoebis_ledger::RoutingLogCursor::parse(&cursor).expect("the cursor parses back")),
        4,
    );
    assert_eq!(
        next.rows.iter().map(log_id).collect::<Vec<_>>(),
        ["e:cooldown_set@100"],
        "the next page holds the extra row"
    );
    assert_eq!(next.next_cursor, None);
    // Page by page through one instant: the cursor's three keys neither skip nor repeat a row on a tie.
    let mut walked = Vec::new();
    let mut cursor: Option<phoebis_ledger::RoutingLogCursor> = None;
    loop {
        let one = page(cursor.as_ref(), 1);
        walked.extend(one.rows.iter().map(log_id));
        match one.next_cursor {
            Some(next) => cursor = phoebis_ledger::RoutingLogCursor::parse(&next),
            None => break,
        }
    }
    assert_eq!(
        walked,
        whole.rows.iter().map(log_id).collect::<Vec<_>>(),
        "one row a page walks the instant in the whole page's order"
    );
}

#[test]
fn the_default_kind_set_hides_probes_and_prefix_expiries_and_kinds_replaces_it() {
    let db = LedgerDb::open_in_memory().unwrap();
    put_decision(&db, &logged("r1", 1, 1, 100, "placed"));
    let mut probe = logged("r2", 1, 1, 200, "renewed");
    probe.probe = true;
    put_decision(&db, &probe);
    db.insert_event(&logged_event(300, "warm_prefix_expired"))
        .unwrap();
    db.insert_event(&logged_event(400, "cooldown_set")).unwrap();
    let kinds = |kinds: &[&str]| phoebis_ledger::RoutingLogFilter {
        kinds: Some(kinds.iter().map(|k| k.to_string()).collect()),
        ..log_filter()
    };
    assert_eq!(
        log_ids(&db, &log_filter()),
        ["e:cooldown_set@400", "d:r1/1"],
        "the default set hides probe decisions and the prefix expiries"
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
        "the kinds named replace the default set whole"
    );
}

#[test]
fn counts_answer_under_a_request_filter_alone() {
    let db = LedgerDb::open_in_memory().unwrap();
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
        db.read_blocking(|c| phoebis_ledger::list_routing_log(c, filter, None, limit))
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
    let db = LedgerDb::open_in_memory().unwrap();
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
fn a_session_prefix_with_like_metacharacters_matches_literally() {
    let db = LedgerDb::open_in_memory().unwrap();
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
    let db = LedgerDb::open_in_memory().unwrap();
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
    let db = LedgerDb::open_in_memory().unwrap();
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
        .read_blocking(|c| phoebis_ledger::list_routing_log(c, &log_filter(), None, 25))
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
    let db = LedgerDb::open_in_memory().unwrap();
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
        .read_blocking(|c| phoebis_ledger::list_routing_log(c, &log_filter(), None, 25))
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
    let db = LedgerDb::open_in_memory().unwrap();
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
            logged_attempt("r1", 2, 200, Some(600)),
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
        "promised_tokens": 500,
        "hit": true,
        "held_ms": 1_250,
        "freed": true,
        "cache_map_written": entries,
        "completed_at": NOW + 2_000,
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
    assert_eq!(record["version"], 1);
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
    let db = LedgerDb::open_in_memory().unwrap();
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
    let db = LedgerDb::open_in_memory().unwrap();
    let attempt = |attempt: u32, input: u64| AttemptRecord {
        request_id: "r1".into(),
        attempt,
        account_id: "a".into(),
        account_kind: "api_key".into(),
        outcome: "succeeded".into(),
        input_tokens: Some(input),
        output_tokens: Some(100),
        ..Default::default()
    };
    let attempts = vec![attempt(1, 1_000), attempt(2, 1_001)];
    let estimate = |input: u64| {
        table
            .estimate("claude-haiku-5-5", Speed::Standard, input, 0, 0, 0, 100)
            .expect("the test list carries the model")
            .1
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
    assert!(
        close(row_usd(&revisions, &rows[0]), over.total())
            && close(row_usd(&revisions, &rows[1]), under.total()),
        "{rows:?}"
    );
    let live = LiveKeys(std::collections::BTreeMap::from([(
        "desktop".to_string(),
        0,
    )]));
    let groups = breakdown(
        &apply_filter(rows, &RollupFilter::default()),
        "model",
        false,
        &live,
        &[],
        None,
        &revisions,
        &PlanTables::shipped(),
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
