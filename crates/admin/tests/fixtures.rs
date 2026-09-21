//! Captures every GET route's answer over a seeded harness into `web/test/fixtures/`, and holds the committed
//! files to the live key shape so a route and its decoder cannot drift apart unnoticed.
mod common;
use std::path::PathBuf;

use phoebis_bus::{
    AccountSelected, AttemptOutcome, Event, Kind, Origin, Phase, RequestComplete, RequestObserved,
    ResponseChunk, ResponseHead, Usage,
};
use phoebis_ledger::{
    AttemptRecord, BodyRecord, EventRecord, ExtraUsageSampleRecord, QuotaSampleRecord, Record,
    RequestRecord,
};
use phoebis_store::{AccountState, Identity, NewBundle, Secret};
use common::{deltas_until, get, json, login, next_text, patch, post, serve, wait_json};
use fake_upstream::{FakeUpstream, Respond, Rule};
use serde_json::{json as j, Value};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../web/test/fixtures")
}

/// Keys only, recursively; the first element stands for an array. Values are not compared: ids and secrets
/// are random, and a value change is the decoder tests' business.
fn shape(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), shape(v))).collect()),
        Value::Array(a) => Value::Array(a.first().map(shape).into_iter().collect()),
        _ => Value::Null,
    }
}

/// Every secret this capture plants or the router mints for it, bound where it is created so a seed that
/// grows a new one has to name it here as well. The three literals are the harness's own configuration.
fn planted(
    access_token: &str,
    refresh_token: &str,
    key_secret: &str,
    rule_secret: &str,
) -> Vec<String> {
    let mut needles: Vec<String> = ["admin-secret", "viewer-secret", "sk-ant-"]
        .into_iter()
        .map(str::to_string)
        .collect();
    needles.push(access_token.into());
    needles.push(refresh_token.into());
    needles.push(key_secret.into());
    needles.push(rule_secret.into());
    needles
}

/// These files are committed, so a capture that picked a credential up would publish it.
fn assert_no_secret(planted: &[String], name: &str, body: &Value) {
    let text = body.to_string();
    for secret in planted {
        assert!(
            !text.contains(secret.as_str()),
            "{name}: the captured body carries a secret"
        );
    }
}

fn hold(planted: &[String], name: &str, route: &str, body: Value) {
    let path = dir().join(format!("{name}.json"));
    assert_no_secret(planted, name, &body);
    let captured = j!({ "route": route, "body": body });
    if std::env::var_os("PHOEBIS_WRITE_FIXTURES").is_some() {
        std::fs::create_dir_all(dir()).unwrap();
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&captured).unwrap() + "\n",
        )
        .unwrap();
        return;
    }
    let committed: Value = serde_json::from_str(
        &std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("{name}.json is missing; run with PHOEBIS_WRITE_FIXTURES=1")),
    )
    .unwrap();
    assert_eq!(shape(&committed["body"]), shape(&captured["body"]), "{name}: the committed fixture's shape differs from the live answer; re-capture and update the decoder");
}

fn request(
    key_generation: i64,
    id: &str,
    at: i64,
    key: &str,
    session: &str,
    status: u16,
    outcome: &str,
) -> RequestRecord {
    RequestRecord {
        id: id.into(),
        received_at: at,
        finished_at: Some(at + 900),
        key_id: Some(key.into()),
        key_generation,
        pool_id: Some("main".into()),
        session_id: Some(session.into()),
        agent_id: Some("main".into()),
        model: "claude-sonnet-5".into(),
        canonical_model: "claude-sonnet-5".into(),
        stream: true,
        kind: "inference".into(),
        origin: "client".into(),
        status: Some(status),
        outcome: outcome.into(),
        attempts: 1,
        ttfb_ms: Some(120),
        ttfc_ms: Some(300),
        duration_ms: Some(900),
        error_code: (outcome != "succeeded").then(|| "upstream_error".into()),
        user_agent: Some("claude-cli/2.1.269 (external, cli)".into()),
        ..Default::default()
    }
}

fn attempt(id: &str, n: u32, account: &str, status: u16, outcome: &str, at: i64) -> AttemptRecord {
    AttemptRecord {
        request_id: id.into(),
        attempt: n,
        account_id: account.into(),
        account_kind: "oauth".into(),
        started_at: at,
        finished_at: Some(at + 400),
        ttfb_ms: Some(120),
        upstream_status: Some(status),
        outcome: outcome.into(),
        input_tokens: Some(1_000),
        output_tokens: Some(100),
        cache_read_tokens: Some(500),
        cache_write_5m_tokens: Some(200),
        cache_miss_reason: Some("none".into()),
        ratelimit_5h_utilization: Some(0.4),
        ratelimit_5h_reset: Some(at + 3_600_000),
        estimated_usd: Some(0.012),
        pricing_version: Some("2026-09-08".into()),
        ..Default::default()
    }
}

#[tokio::test]
async fn every_route_is_captured_and_holds_its_shape() {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let now = common::NOW;
    let app = &parts.app;
    // The full harness leaves its subscription slot unpaired; every page these fixtures feed draws a paired
    // account, so the capture pairs it the way a finished login would.
    parts
        .store
        .set_identity(
            "business",
            &Identity {
                account_uuid: common::ACCOUNT_UUID.into(),
                email: "test@example.invalid".into(),
                organization_uuid: Some("00000000-0000-4000-8000-000000000002".into()),
            },
        )
        .unwrap();
    parts
        .store
        .set_state("business", AccountState::Ready)
        .unwrap();
    // Far from expiry, so reading usage never triggers a refresh of its own against the fake. Both tokens are
    // bound rather than written inline, so the no-secret guard reads the exact values this seed plants.
    let access_token = "oat-fixture";
    let refresh_token = "ort-fixture";
    parts
        .store
        .commit_credentials(
            "business",
            1,
            1,
            None,
            None,
            &NewBundle {
                access_token: Secret::new(access_token),
                refresh_token: Some(Secret::new(refresh_token)),
                issued_at: now,
                expires_at: now + 3_600_000,
                refresh_expires_at: None,
                scopes: "user:profile user:inference".into(),
            },
        )
        .unwrap();
    parts.store.set_profile_json("business", r#"{"account":{"uuid":"u","email_address":"business@example.invalid","display_name":"Biz","has_extra_usage_enabled":true},"organization":{"uuid":"o","name":"Biz Org","organization_type":"claude_max","billing_type":"stripe_subscription","rate_limit_tier":"default_claude_max_5x"}}"#).unwrap();
    post(app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"least_utilized","affinity_ttl_seconds":600,"members":[{"account_id":"business","priority":0},{"account_id":"direct","priority":1}],"keepalive":{"enabled":true,"probe_cost_cap_usd_per_day":4},"body_capture_hours":24}"#, Some(&cookie)).await;
    let created = json(post(app, "/admin/keys", r#"{"id":"desktop","label":"Desktop","pool_id":"main","allowed_models":["claude-sonnet-5"]}"#, Some(&cookie)).await).await;
    let key_secret = created["secret"]
        .as_str()
        .expect("a created key answers its secret once")
        .to_string();
    // The store stamps the key from the wall clock, so the live generation is read back rather than assumed.
    let key_generation = parts.store.api_key("desktop").unwrap().unwrap().created_at;
    // The alert rule seeded below signs with this, and the guard reads every planted secret from here on.
    let rule_secret = "s3cret";
    let planted = planted(access_token, refresh_token, &key_secret, rule_secret);
    let mut key_created = created.clone();
    // The minted secret is random, so the fixture carries a stable stand-in rather than the capture's own.
    key_created["secret"] = j!("phoebis-fixture");
    hold(&planted, "key_created", "POST /admin/keys", key_created);
    let bucket = phoebis_ledger::bucket_of(now - 60_000);
    parts
        .db
        .insert_samples(
            &[
                // The two samplers spell a window differently: the poll writes the usage endpoint's keys, the header the
                // short forms. The fixture carries one of each so the decoder is proven over both.
                QuotaSampleRecord {
                    account_id: "business".into(),
                    bucket,
                    window: "five_hour".into(),
                    utilization: Some(0.62),
                    reset_at: Some(now + 3_600_000),
                    source: "poll".into(),
                },
                QuotaSampleRecord {
                    account_id: "business".into(),
                    bucket,
                    window: "7d".into(),
                    utilization: Some(0.41),
                    reset_at: Some(now + 86_400_000),
                    source: "header".into(),
                },
                QuotaSampleRecord {
                    account_id: "business".into(),
                    bucket,
                    window: "fable".into(),
                    utilization: Some(0.12),
                    reset_at: Some(now + 86_400_000),
                    source: "poll".into(),
                },
            ],
            Some(&ExtraUsageSampleRecord {
                account_id: "business".into(),
                bucket,
                is_enabled: true,
                in_use: Some(false),
                monthly_limit: Some(5_000),
                used_credits: Some(1_234),
                utilization: Some(0.25),
                currency: Some("USD".into()),
                disabled_reason: None,
                overage_reset: None,
                source: "poll".into(),
            }),
        )
        .unwrap();
    let hour_ago = now - 3_600_000;
    parts.ledger.record(Record::Request {
        request: RequestRecord {
            attempts: 2,
            ..request(
                key_generation,
                "r-failover",
                hour_ago,
                "desktop",
                "s1",
                200,
                "succeeded",
            )
        },
        attempts: vec![
            AttemptRecord {
                cooldown_scope: Some("account".into()),
                cooldown_until: Some(now + 1_800_000),
                ..attempt("r-failover", 1, "direct", 429, "rate_limited", hour_ago)
            },
            attempt(
                "r-failover",
                2,
                "business",
                200,
                "succeeded",
                hour_ago + 400,
            ),
        ],
        bodies: None,
    });
    // The session moved during this request's window, so the detail route lists the event beside it and the
    // decoder has a real one to read.
    parts.ledger.record(Record::Event(EventRecord {
        at: hour_ago + 100,
        kind: "pin_migrated".into(),
        account_id: Some("business".into()),
        key_id: Some("desktop".into()),
        session_id: Some("s1".into()),
        model: Some("claude-sonnet-5".into()),
        actor: None,
        detail: Some(j!({ "from_account": "direct", "epoch": 2, "reason": "spent" }).to_string()),
    }));
    parts.ledger.record(Record::Event(EventRecord {
        at: hour_ago + 200,
        kind: "pin_expired".into(),
        account_id: Some("business".into()),
        key_id: Some("desktop".into()),
        session_id: Some("s1".into()),
        model: Some("claude-sonnet-5".into()),
        actor: None,
        detail: Some(j!({ "reasons": ["cooldown", "spent"] }).to_string()),
    }));
    parts.ledger.record(Record::Request {
        request: RequestRecord {
            bodies_captured: true,
            ..request(
                key_generation,
                "r-captured",
                now - 60_000,
                "desktop",
                "s1",
                200,
                "succeeded",
            )
        },
        attempts: vec![attempt(
            "r-captured",
            1,
            "business",
            200,
            "succeeded",
            now - 60_000,
        )],
        bodies: Some(BodyRecord {
            request_id: "r-captured".into(),
            request_body: br#"{"model":"claude-sonnet-5","messages":[]}"#.to_vec(),
            response_body: Some(br#"{"id":"msg","content":[]}"#.to_vec()),
            expires_at: now + 86_400_000,
        }),
    });
    parts.ledger.record(Record::Request {
        request: request(
            key_generation,
            "r-failed",
            now - 120_000,
            "desktop",
            "s2",
            502,
            "failed",
        ),
        attempts: vec![attempt(
            "r-failed",
            1,
            "business",
            502,
            "upstream_error",
            now - 120_000,
        )],
        bodies: None,
    });
    // A row an earlier key under the same id spent, so the key grouping answers the deleted holder beside
    // the live key and the decoder sees the label.
    parts.ledger.record(Record::Request {
        request: request(
            now - 40 * 86_400_000,
            "r-earlier-holder",
            hour_ago + 500,
            "desktop",
            "s0",
            200,
            "succeeded",
        ),
        attempts: vec![attempt(
            "r-earlier-holder",
            1,
            "business",
            200,
            "succeeded",
            hour_ago + 500,
        )],
        bodies: None,
    });
    parts
        .keepalive
        .apply_event(
            "desktop",
            phoebis_keepalive::Event {
                event: "session_start".into(),
                session_id: "s1".into(),
                instance_id: "i1".into(),
                sequence: 1,
                ..Default::default()
            },
        )
        .unwrap();
    parts.keepalive.observe(Event::RequestObserved(RequestObserved { request_id: "k1".into(), origin: Origin::Client, kind: Kind::Inference, key_id: Some("desktop".into()), key_generation, pool_id: Some("main".into()), session_id: Some("s1".into()), agent_id: Some("main".into()), parent_agent_id: None, owner_id: None, model: "claude-sonnet-5".into(), canonical_model: "claude-sonnet-5".into(), stream: false, headers: vec![], body: bytes::Bytes::from(r#"{"model":"claude-sonnet-5","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}"#), body_capture_hours: 0, received_at: now }));
    parts
        .keepalive
        .observe(Event::AccountSelected(AccountSelected {
            request_id: "k1".into(),
            attempt: 1,
            account_id: "direct".into(),
            account_kind: "api_key".into(),
            pin_epoch: Some(1),
            started_at: now,
            cache_ttl_ms: None,
        }));
    parts
        .keepalive
        .observe(Event::RequestComplete(RequestComplete {
            request_id: "k1".into(),
            attempt: 1,
            account_id: Some("direct".into()),
            outcome: phoebis_bus::Outcome::Succeeded,
            status: 200,
            usage: Some(Usage {
                cache_write_5m_tokens: Some(2_000),
                ..Default::default()
            }),
            ttfb_ms: None,
            ttfc_ms: None,
            error_code: None,
            client_drift: None,
            client_version: None,
            response_bytes: None,
            finished_at: now,
        }));
    let prices = FakeUpstream::start_plain(vec![Rule { path_prefix: "/prices.json".into(), respond: Respond::Json { status: 200, headers: vec![], body: br#"{"claude-sonnet-5":{"input_cost_per_token":2e-06,"output_cost_per_token":1e-05,"cache_read_input_token_cost":2e-07,"cache_creation_input_token_cost":2.5e-06,"cache_creation_input_token_cost_above_1hr":4e-06}}"#.to_vec() } }]).await;
    parts
        .store
        .apply_catalog_sync(
            "business",
            &[("claude-sonnet-5".into(), true)],
            None,
            now,
            "test",
        )
        .unwrap();
    let ready = wait_json(app, "/admin/requests", &cookie, |v| {
        v["requests"].as_array().unwrap().len() == 4
    })
    .await;
    hold(&planted, "requests", "GET /admin/requests", ready);
    // One rule per shape the page draws: an enabled one with a secret and a threshold, and a disabled one. The
    // drift rule is turned off only after its event opened, since a disabled rule matches no signal.
    post(app, "/admin/alerts/rules", &format!(r#"{{"kind":"utilization_above","scope":{{"type":"pool","id":"main"}},"threshold":0.9,"window":"7d_oi","webhook_url":"http://127.0.0.1:1/hook","secret":"{rule_secret}"}}"#), Some(&cookie)).await;
    let drift = json(post(app, "/admin/alerts/rules", r#"{"kind":"client_drift","scope":{"type":"all"},"webhook_url":"http://127.0.0.1:1/drift"}"#, Some(&cookie)).await).await;
    // A refusal, then a 500, then the 200 that ends it: the event carries all three attempts.
    parts.webhook.answer(Err("refused".into()));
    parts.webhook.answer(Ok(500));
    parts.webhook.answer(Ok(200));
    parts.alerts.signal(&phoebis_alerts::Signal {
        kind: phoebis_alerts::Kind::ClientDrift,
        account_id: None,
        key_id: Some("desktop".into()),
        pool_id: None,
        subject: "client 2.1.274".into(),
        detail: j!({ "client_version": "2.1.274", "key": "desktop", "trigger": "hash" }),
    });
    parts.alerts.wait_idle().await;
    patch(
        app,
        &format!("/admin/alerts/rules/{}", drift["id"].as_str().unwrap()),
        r#"{"enabled":false}"#,
        Some(&cookie),
    )
    .await;
    for (name, route) in [
        ("account", "/admin/accounts/business"),
        ("account_models", "/admin/accounts/business/models"),
        ("account_quota", "/admin/accounts/business/quota?range=7d"),
        ("account_usage", "/admin/accounts/business/usage"),
        ("accounts", "/admin/accounts"),
        ("alert_events", "/admin/alerts/events"),
        ("alert_rules", "/admin/alerts/rules"),
        ("analytics_accounts", "/admin/analytics/accounts?range=24h"),
        (
            "analytics_breakdown",
            "/admin/analytics/breakdown?range=24h&group_by=key",
        ),
        (
            "analytics_series",
            "/admin/analytics/series?range=24h&group_by=key",
        ),
        ("dashboard", "/admin/dashboard"),
        ("keepalive_overview", "/admin/keepalive/overview"),
        ("keepalive_recipes", "/admin/keepalive/recipes"),
        ("keys", "/admin/keys"),
        ("live_past", "/admin/live/snapshot"),
        ("pool_traffic", "/admin/pools/main/traffic?range=24h"),
        ("pools", "/admin/pools"),
        ("pricing", "/admin/pricing"),
        ("request", "/admin/requests/r-failover"),
        ("request_bodies", "/admin/requests/r-captured/bodies"),
        ("session", "/admin/session"),
        ("sessions", "/admin/sessions"),
        ("sessions_costs", "/admin/sessions/costs"),
        ("settings", "/admin/settings"),
        ("system", "/admin/system"),
    ] {
        let response = get(app, route, Some(&cookie)).await;
        assert_eq!(response.status(), 200, "{route}");
        hold(
            &planted,
            name,
            &format!("GET {route}"),
            json(response).await,
        );
    }
    // The deliveries route needs the id the events answer carries, so it is captured after the loop.
    let events = json(get(app, "/admin/alerts/events", Some(&cookie)).await).await;
    let event = events["events"][0]["id"]
        .as_str()
        .expect("the seeded signal opened an event")
        .to_string();
    hold(
        &planted,
        "alert_deliveries",
        "GET /admin/alerts/events/{id}/deliveries",
        json(
            get(
                app,
                &format!("/admin/alerts/events/{event}/deliveries"),
                Some(&cookie),
            )
            .await,
        )
        .await,
    );
    let preview = json(
        post(
            app,
            "/admin/pricing/import",
            &format!(
                r#"{{"url":"http://127.0.0.1:{}/prices.json"}}"#,
                prices.addr().port()
            ),
            Some(&cookie),
        )
        .await,
    )
    .await;
    hold(
        &planted,
        "pricing_import",
        "POST /admin/pricing/import",
        preview,
    );
    hold(
        &planted,
        "account_sync",
        "POST /admin/accounts/business/models/sync",
        json(
            post(
                app,
                "/admin/accounts/business/models/sync",
                "",
                Some(&cookie),
            )
            .await,
        )
        .await,
    );
    hold(
        &planted,
        "pairing_started",
        "POST /admin/oauth/sessions",
        json(
            post(
                app,
                "/admin/oauth/sessions",
                r#"{"account_id":"business","login":"claude_ai"}"#,
                Some(&cookie),
            )
            .await,
        )
        .await,
    );
    // The live feed: the snapshot, then every delta of one streamed request.
    let addr = serve(app.clone()).await;
    let request = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
        format!("ws://{addr}/admin/live"),
    )
    .map(|mut r| {
        r.headers_mut()
            .insert("cookie", format!("phoebis_admin={cookie}").parse().unwrap());
        r
    })
    .unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let snapshot = next_text(&mut ws).await;
    parts.bus.publish(Event::RequestObserved(RequestObserved {
        request_id: "live-1".into(),
        origin: Origin::Client,
        kind: Kind::Inference,
        key_id: Some("desktop".into()),
        key_generation: 0,
        pool_id: Some("main".into()),
        session_id: Some("s1".into()),
        agent_id: Some("main".into()),
        parent_agent_id: None,
        owner_id: None,
        model: "claude-sonnet-5".into(),
        canonical_model: "claude-sonnet-5".into(),
        stream: true,
        headers: vec![],
        body: bytes::Bytes::from_static(b"{}"),
        body_capture_hours: 24,
        received_at: now,
    }));
    parts.bus.publish(Event::AccountSelected(AccountSelected {
        request_id: "live-1".into(),
        attempt: 1,
        account_id: "business".into(),
        account_kind: "oauth".into(),
        pin_epoch: Some(1),
        started_at: now + 5,
        cache_ttl_ms: None,
    }));
    parts.bus.publish(Event::ResponseHead(ResponseHead {
        request_id: "live-1".into(),
        attempt: 1,
        status: 200,
        headers: vec![],
        first_event: None,
        outcome: AttemptOutcome::Succeeded,
        cooldown: None,
        written_at: now + 10,
        observed_at: now + 100,
    }));
    // Two chunks, because the feed answers a usage-bearing phase with a progress delta and a parseable
    // content delta with a preview; one chunk carrying both would draw them in the other order.
    parts.bus.publish(Event::ResponseChunk(ResponseChunk {
        request_id: "live-1".into(),
        attempt: 1,
        index: 0,
        body: bytes::Bytes::from_static(
            b"event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
        ),
        phase: Some(Phase::MessageStart {
            usage: Usage {
                input_tokens: Some(10),
                ..Default::default()
            },
        }),
    }));
    parts.bus.publish(Event::ResponseChunk(ResponseChunk { request_id: "live-1".into(), attempt: 1, index: 1, body: bytes::Bytes::from_static(b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n"), phase: Some(Phase::FirstContentDelta) }));
    parts.bus.publish(Event::RequestComplete(RequestComplete {
        request_id: "live-1".into(),
        attempt: 1,
        account_id: Some("business".into()),
        outcome: phoebis_bus::Outcome::Succeeded,
        status: 200,
        usage: Some(Usage {
            input_tokens: Some(10),
            output_tokens: Some(2),
            ..Default::default()
        }),
        ttfb_ms: Some(95),
        ttfc_ms: Some(120),
        error_code: None,
        client_drift: None,
        client_version: Some("2.1.269".into()),
        response_bytes: Some(5),
        finished_at: now + 900,
    }));
    let deltas = deltas_until(&mut ws, "finished").await;
    // The socket wraps every delta the way the client reads it, which `deltas_until` unwraps for its callers.
    let deltas: Vec<Value> = deltas
        .into_iter()
        .map(|delta| j!({ "kind": "delta", "delta": delta }))
        .collect();
    hold(
        &planted,
        "live_messages",
        "GET /admin/live",
        j!({ "snapshot": snapshot, "deltas": deltas }),
    );
    prices.shutdown().await;
}
