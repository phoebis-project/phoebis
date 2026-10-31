//! Captures every GET route's answer over a seeded harness into `web/test/fixtures/`, and holds the committed
//! files to the live key shape so a route and its decoder cannot drift apart unnoticed.
mod common;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use common::{get, json, login, patch, post, wait_json};
use fake_upstream::stays_for;
use phoebis_bus::{
    AccountSelected, Event, Kind, Origin, RequestComplete, RequestObserved, RequestShape,
    ResponseFacts, Usage,
};
use phoebis_ledger::{
    AttemptRecord, BodyRecord, EventRecord, ExtraUsageSampleRecord, GuardrailMatchRecord,
    QuotaSampleRecord, Record, RequestRecord,
};
use phoebis_routing::{lane_key, ClientClass};
use phoebis_store::{
    AccountKind, AccountRow, AccountState, Identity, NewBundle, Secret, StateCause, SubscriptionRow,
};
use serde_json::{json as j, Value};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../web/test/fixtures")
}

/// One line per value, depth first in key order: the path and the JSON kind (`null`, `boolean`, `integer`, `float`,
/// `string`, `array`, `object`). The first element stands for an array. Values are not compared: a value change is
/// the decoder tests' business. The order and the kinds are compared, since an answer rebuilt from a struct could
/// move a key, turn an absence into a `null` or an integer into a float and still hold the key set. A backslash, a
/// `.` or a `[` in a key is escaped with a backslash, so a key cannot read as a nested path.
fn shape(v: &Value) -> Vec<String> {
    fn walk(v: &Value, path: &str, out: &mut Vec<String>) {
        let kind = match v {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(n) if n.is_f64() => "float",
            Value::Number(_) => "integer",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        };
        out.push(format!("{path}: {kind}"));
        match v {
            Value::Object(m) => {
                for (k, v) in m {
                    let k = k
                        .replace('\\', "\\\\")
                        .replace('.', "\\.")
                        .replace('[', "\\[");
                    walk(v, &format!("{path}.{k}"), out);
                }
            }
            Value::Array(a) => {
                if let Some(first) = a.first() {
                    walk(first, &format!("{path}[0]"), out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(v, "$", &mut out);
    out
}

#[test]
fn shape_tells_a_moved_key_a_null_for_an_absence_and_a_float_for_an_integer_but_not_a_value() {
    let differ = [
        (
            j!({ "a": 1, "b": "x" }),
            j!({ "b": "x", "a": 1 }),
            "a moved key",
        ),
        (
            j!({ "a": 1, "b": null }),
            j!({ "a": 1 }),
            "a null for an absent key",
        ),
        (j!({ "a": 1.0 }), j!({ "a": 1 }), "a float for an integer"),
        (
            j!({ "a": { "b": 1 } }),
            j!({ "a": {}, "a.b": 1 }),
            "a dotted key for a nested one",
        ),
    ];
    for (left, right, what) in &differ {
        assert_ne!(
            shape(left),
            shape(right),
            "{what} leaves the shape unchanged"
        );
    }
    assert_eq!(
        shape(&j!({ "a": 1, "b": { "c": ["x"] }, "d": null })),
        shape(&j!({ "a": 2, "b": { "c": ["y", "z"] }, "d": null })),
        "the same keys, order and kinds differ in shape"
    );
}

/// Every secret this capture plants or the router mints for it, bound where it is created so a seed that
/// grows a new one has to name it here as well. The two literals are the harness's own configuration.
fn planted(access_token: &str, refresh_token: &str, key_secret: &str) -> Vec<String> {
    let mut needles: Vec<String> = ["admin-secret", "sk-ant-"]
        .into_iter()
        .map(str::to_string)
        .collect();
    needles.push(access_token.into());
    needles.push(refresh_token.into());
    needles.push(key_secret.into());
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
    let committed: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|_| {
            panic!("{name}.json is missing; run with PHOEBIS_WRITE_FIXTURES=1")
        }))
        .unwrap();
    let committed = shape(&committed["body"]);
    let live = shape(&captured["body"]);
    if committed != live {
        let at = committed
            .iter()
            .zip(live.iter())
            .position(|(c, l)| c != l)
            .unwrap_or(committed.len().min(live.len()));
        panic!(
            "{name}: the committed fixture's shape differs from the live answer at entry {at}: committed {:?}, live {:?}. \
             A moved key, a null for an absence or a float for an integer is a defect of the route's answer. A new or \
             dropped key needs a re-capture and a decoder update.",
            committed.get(at),
            live.get(at)
        );
    }
}

/// A value past its field's type fails the body's parse, and that sentence names no bound, so such a probe keeps
/// its status alone.
const UNPARSED: &str = "body must be a JSON object";
const POOL_ROUTE: &str = "PATCH /admin/pools/{id}";

/// The route's answer to every bound the console holds and to one past it: the keepalive policy's, the pool
/// engine's with its in-flight cap, and the model rule count's.
/// The burn average, the shed dwell, the slow-down's stretch and hold, the window ceiling and the shed cap are probed
/// at both edges of their bounds. A probe that sends another field beside the probed one names it under `with`, so a
/// replay sends the same body. An admitted probe is written back at once from the list read taken before the probes,
/// so each probe meets the seeded pool, as the captures after these do.
async fn bound_probes(app: &axum::Router, cookie: &str) -> Value {
    let pools = json(get(app, "/admin/pools", Some(cookie)).await).await;
    let pool = pools["pools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "main")
        .expect("the seeded pool is listed")
        .clone();

    // Each row: the path, the route, the field as the route names it, the value, the body, its restore, and the
    // other fields the body sends. The value is JSON, since the fractions' bounds are not whole.
    type Row = (
        &'static str,
        &'static str,
        String,
        Value,
        Value,
        Value,
        Option<Value>,
    );
    let mut table: Vec<Row> = Vec::new();
    for (field, min, max) in [
        ("max_idle_seconds", 0, 31_536_000),
        ("max_parked_seconds", 60, 604_800),
        ("scheduled_horizon_seconds", 60, 604_800),
    ] {
        let values = if min > 0 {
            vec![min, min - 1, max, max + 1]
        } else {
            vec![min, max, max + 1]
        };
        for value in values {
            table.push((
                "/admin/pools/main",
                POOL_ROUTE,
                format!("keepalive.{field}"),
                j!(value),
                j!({ "keepalive": { field: value } }),
                j!({ "keepalive": { field: pool["keepalive"][field] } }),
                None,
            ));
        }
    }
    for (field, values) in [
        ("burn_average_seconds", &[600_u64, 599, 7_200, 7_201][..]),
        ("shed_dwell_seconds", &[300, 299, 7_200, 7_201]),
        ("slow_down_stretch_seconds", &[60, 59, 18_000, 18_001]),
        ("slow_down_hold_seconds", &[1, 0, 300, 301]),
        ("retry_in_place_seconds", &[0, 300, 301]),
        ("overage_move_grace_minutes", &[0, 300, 301]),
        ("wind_down_minutes", &[0, 10_080, 10_081]),
        ("return_to_plan_dwell_seconds", &[0, 86_400, 86_401]),
        ("in_flight_cap", &[1, 0, 4_294_967_295, 4_294_967_296]),
    ] {
        for &value in values {
            table.push((
                "/admin/pools/main",
                POOL_ROUTE,
                field.into(),
                j!(value),
                j!({ field: value }),
                j!({ field: pool[field] }),
                None,
            ));
        }
    }
    for (field, values) in [
        ("window_ceiling", [0.5, 0.49, 1.0, 1.01]),
        ("shed_cap", [0.1, 0.09, 1.0, 1.01]),
    ] {
        for value in values {
            table.push((
                "/admin/pools/main",
                POOL_ROUTE,
                field.into(),
                j!(value),
                j!({ field: value }),
                j!({ field: pool[field] }),
                None,
            ));
        }
    }
    for count in [256_usize, 257] {
        let rule = j!({ "pattern": "claude-haiku-*", "not": true });
        table.push((
            "/admin/pools/main",
            POOL_ROUTE,
            "model_rules".into(),
            j!(count),
            j!({ "model_rules": vec![rule; count] }),
            j!({ "model_rules": pool["model_rules"] }),
            None,
        ));
    }

    let mut probes = Vec::new();
    for (path, route, field, value, body, restore, with) in table {
        let response = patch(app, path, &body.to_string(), Some(cookie)).await;
        let status = response.status();
        let answer = json(response).await;
        let mut probe =
            j!({ "route": route, "field": field, "value": value, "status": status.as_u16() });
        if let Some(with) = with {
            probe["with"] = with;
        }
        if status.is_success() {
            let restored = patch(app, path, &restore.to_string(), Some(cookie)).await;
            assert_eq!(
                restored.status(),
                200,
                "{field} {value}: the stored value is written back"
            );
        } else if let Some(error) = answer["error"].as_str().filter(|e| *e != UNPARSED) {
            probe["error"] = j!(error);
        }
        probes.push(probe);
    }

    // The token carries the stamp, so it moves with it.
    let unstamped = |mut pool: Value| {
        let fields = pool.as_object_mut().unwrap();
        fields.remove("updated_at");
        fields.remove("revision");
        pool
    };
    let after = json(get(app, "/admin/pools", Some(cookie)).await).await;
    let restored = after["pools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "main")
        .unwrap()
        .clone();
    assert_eq!(
        unstamped(restored),
        unstamped(pool),
        "the probes leave the pool as seeded"
    );
    Value::Array(probes)
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
        model: "claude-sonnet-5-5".into(),
        canonical_model: "claude-sonnet-5-5".into(),
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
        user_agent: Some("claude-cli/2.1.310 (external, cli)".into()),
        client: Some("claude-code/cli".into()),
        effort: Some("high".into()),
        thinking_type: Some("enabled".into()),
        thinking_budget: Some(31_999),
        max_tokens: Some(32_000),
        tool_count: 12,
        system_blocks: 4,
        message_count: 1,
        context_edits: "clear_thinking_20251015".into(),
        stream_end_at: Some(at + 850),
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
        upstream_request_id: Some(format!("req_{id}_{n}")),
        upstream_message_id: (status == 200).then(|| format!("msg_{id}_{n}")),
        outcome: outcome.into(),
        input_tokens: Some(1_000),
        output_tokens: Some(100),
        cache_read_tokens: Some(500),
        cache_write_5m_tokens: Some(200),
        thinking_tokens: Some(40),
        web_search_requests: Some(0),
        web_fetch_requests: Some(0),
        stop_reason: Some("end_turn".into()),
        speed: Some("standard".into()),
        inference_geo: Some("us".into()),
        cache_miss_reason: Some("none".into()),
        ratelimit_5h_utilization: Some(0.4),
        ratelimit_5h_reset: Some(at + 3_600_000),
        written_at: Some(at + 30),
        ..Default::default()
    }
}

/// Seeds every id and secret the capture mints, so two captures at one head write the same bytes.
const FIXTURE_SEED: u64 = 0x0123_4567_89ab_cdef;

#[tokio::test]
async fn every_route_is_captured_and_holds_its_shape() {
    let parts = common::admin_parts_full().await;
    // Ahead of the login, since the login mints the session id `session.json` reads. The seeded source is
    // process-wide, so the repeat holds while this case is the binary's one minting test: a second one running beside
    // it would draw from the same generator and move every id after its first mint.
    phoebis_oauth::random::seed(FIXTURE_SEED);
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
        .set_state("business", AccountState::Ready, StateCause::Paired)
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
    parts.store.set_profile(
        "business",
        r#"{"account":{"uuid":"u","email":"business@example.invalid","display_name":"Biz","has_claude_max":true},"organization":{"uuid":"o","name":"Biz Org","organization_type":"claude_max","billing_type":"stripe_subscription","rate_limit_tier":"default_claude_max_20x","has_extra_usage_enabled":true,"subscription_created_at":"2026-03-14T10:00:00Z"}}"#,
        &SubscriptionRow {
            plan: Some("max".into()),
            tier: Some("default_claude_max_20x".into()),
            since: Some(1_773_482_400_000),
            trial_ends_at: None,
        },
    ).unwrap();
    // A usage-based Enterprise seat beside the plan account, paired and classed from its stored profile, so the
    // account and pool fixtures carry the class on a row that holds it. It shares the seed's two bound tokens, which
    // the no-secret guard already reads.
    parts
        .store
        .insert_account(&AccountRow {
            id: "usage-seat".into(),
            kind: AccountKind::Oauth,
            label: "Seat".into(),
            enabled: true,
            identity: Some(Identity {
                account_uuid: "00000000-0000-4000-8000-000000000003".into(),
                email: "seat@example.invalid".into(),
                organization_uuid: Some("00000000-0000-4000-8000-000000000004".into()),
            }),
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state: AccountState::Ready,
            state_cause: StateCause::Paired,
        })
        .unwrap();
    parts
        .store
        .commit_credentials(
            "usage-seat",
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
    parts.store.set_profile(
        "usage-seat",
        r#"{"account":{"uuid":"s","email":"seat@example.invalid","display_name":"Seat"},"organization":{"uuid":"e","name":"Enterprise Org","organization_type":"claude_enterprise","rate_limit_tier":"default_claude_zero","seat_tier":"enterprise_usage_based","has_extra_usage_enabled":true}}"#,
        &SubscriptionRow {
            plan: None,
            tier: Some("default_claude_zero".into()),
            since: None,
            trial_ends_at: None,
        },
    ).unwrap();
    // A free organization on the Pro tier, so the accounts fixtures carry the plan its type resolves to. It sits in
    // no pool, which leaves the pool fixtures as they stand, and shares the seed's two bound tokens as the seat does.
    parts
        .store
        .insert_account(&AccountRow {
            id: "free-org".into(),
            kind: AccountKind::Oauth,
            label: "Free".into(),
            enabled: true,
            identity: Some(Identity {
                account_uuid: "00000000-0000-4000-8000-000000000005".into(),
                email: "free@example.invalid".into(),
                organization_uuid: Some("00000000-0000-4000-8000-000000000006".into()),
            }),
            lifecycle_generation: 1,
            incarnation: 1,
            revision: 1,
            state: AccountState::Ready,
            state_cause: StateCause::Paired,
        })
        .unwrap();
    parts
        .store
        .commit_credentials(
            "free-org",
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
    parts.store.set_profile(
        "free-org",
        r#"{"account":{"uuid":"f","email":"free@example.invalid","display_name":"Free"},"organization":{"uuid":"g","name":"Free Org","organization_type":"claude_free","billing_type":"free","rate_limit_tier":"default_claude_ai","has_extra_usage_enabled":false}}"#,
        &SubscriptionRow {
            plan: None,
            tier: Some("default_claude_ai".into()),
            since: None,
            trial_ends_at: None,
        },
    ).unwrap();
    // The free organization's term ended eight days ago, inside a week its reading below names, so the unused
    // allowance capture carries a term end. Set ahead of the pool's creation, whose reload brings it to the snapshot.
    const DAY: i64 = 86_400_000;
    let free_end = now - 8 * DAY;
    parts
        .store
        .set_canceled("free-org", true, Some(free_end))
        .unwrap();
    // The profile read a quarter hour after the end found the free document stored above and flagged the account, as
    // the poll does. No event is recorded, so the routing log's ids stand.
    assert!(parts
        .store
        .set_plan_ended("free-org", free_end + 900_000)
        .unwrap());
    // A cap under a quarter, so the pool answers one warning and the console draws a non-empty list.
    post(app, "/admin/pools", r#"{"id":"main","label":"Main","strategy":"pace","affinity_ttl_seconds":600,"shed_cap":0.2,"members":[{"account_id":"business","priority":0,"budget_monthly_usd":30.0},{"account_id":"direct","priority":1,"budget_monthly_usd":50.0},{"account_id":"usage-seat","priority":2}],"keepalive":{"enabled":true},"body_capture_hours":24,"model_rules":[{"pattern":"claude-haiku-*","not":true}]}"#, Some(&cookie)).await;
    let created = json(post(app, "/admin/keys", r#"{"id":"desktop","label":"Desktop","pool_id":"main","model_rules":[{"pattern":"claude-opus-*","not":true}],"model_rules_mode":"extend"}"#, Some(&cookie)).await).await;
    let key_secret = created["secret"]
        .as_str()
        .expect("a created key answers its secret once")
        .to_string();
    // The key's generation is the stamp the store gave it, read back rather than assumed.
    let key_generation = parts.store.api_key("desktop").unwrap().unwrap().created_at;
    let planted = planted(access_token, refresh_token, &key_secret);
    let mut key_created = created.clone();
    // No fixture carries a secret shaped like a real one, so the stand-in stays whatever the capture mints.
    key_created["secret"] = j!("phoebis-fixture");
    hold(&planted, "key_created", "POST /admin/keys", key_created);
    // One guardrail observing on the pool and one enforcing on the key, so the matches the request rows below carry
    // name registry rows and the pool's and the key's answers carry both lists. The first comes from the preset, so
    // the guardrail fixtures carry the stamp the console reads.
    let response = post(app, "/admin/guardrails", r#"{"id":"subagent-status-summary","preset":"subagent-status-summary","description":"The status line fork the client sends for each running subagent.","pools":["main"]}"#, Some(&cookie)).await;
    assert_eq!(response.status(), 201, "the observing guardrail is created");
    hold(
        &planted,
        "guardrail_created",
        "POST /admin/guardrails",
        json(response).await,
    );
    let response = post(app, "/admin/guardrails", r#"{"id":"large-auxiliary","name":"Large auxiliary requests","description":"Auxiliary requests that ask for more than 4096 output tokens.","mode":"enforce","conditions":[{"subject":"request_class","operator":"equals","value":"auxiliary"},{"subject":"body","operator":"gt","path":"max_tokens","value":"4096"}],"keys":["desktop"]}"#, Some(&cookie)).await;
    assert_eq!(response.status(), 201, "the enforcing guardrail is created");
    // A real selection through the router, so `sessions.json` captures a pinned row and the trace below is the
    // shape `select` actually serializes, not a hand-built stand-in.
    let snapshot = parts.router.snapshot();
    let key_row = snapshot
        .key("desktop")
        .expect("the desktop key is in the snapshot")
        .clone();
    let candidates =
        phoebis_routing::resolve(&snapshot, &key_row, "claude-sonnet-5", "claude-sonnet-5")
            .expect("the pool's model rules allow claude-sonnet-5");
    let selected = parts
        .router
        .select(phoebis_routing::SelectRequest {
            request_id: "r-1".into(),
            attempt: 1,
            seq: 1,
            key_id: "desktop".into(),
            key_generation,
            agent_id: "main".into(),
            session_id: "s-fixture".into(),
            canonical_model: "claude-sonnet-5".into(),
            folded_model: "claude-sonnet-5".into(),
            candidates,
            portable: true,
            credit_account: None,
            attempted: Vec::new(),
            now,
            received_at: now,
            fallback_ms: 60_000,
            revision: snapshot.revision,
            probe: false,
            count: false,
            request_class: None,
            context_compacted: false,
            static_hash: None,
            boundaries: Arc::from([]),
            client: ClientClass::ClaudeCode,
            entrypoint: None,
            static_boundaries: Arc::from([]),
            marker: false,
            parent_agent_id: None,
            failed_over: None,
            holds: Vec::new(),
            dialed: false,
            held_out: false,
            lane: lane_key("claude-sonnet-5", "standard", "none", "-"),
            fast: false,
            classifier: false,
            body_tokens: 0,
            effort: None,
        })
        .await
        .unwrap();
    let selection = match &selected.result {
        phoebis_routing::SelectResult::Selected(selection) => selection.clone(),
        other => panic!("the pool's two eligible members answer a selection, not {other:?}"),
    };
    let trace_json = serde_json::to_string(&selection.trace).unwrap();
    // The failover request's first attempt lands where its messages' prefix is warm, after one hold on its
    // cohort's account, so `request.json` captures a placement and a hold as `select` serializes them on the
    // attempt the shape check reads. The boundaries stand for the conversation `s1` forked from.
    let boundaries: Vec<String> = vec!["b-fixture-1".into(), "b-fixture-2".into()];
    parts
        .router
        .report(phoebis_routing::Report::Affinity(
            phoebis_routing::AffinityReport {
                account_id: "direct".into(),
                incarnation: snapshot
                    .account("direct")
                    .expect("the direct account is in the snapshot")
                    .incarnation,
                key_id: "desktop".into(),
                key_generation,
                canonical_model: "claude-sonnet-5-5".into(),
                lane: lane_key("claude-sonnet-5-5", "standard", "none", "-"),
                root: "s0".into(),
                static_boundaries: Arc::from([]),
                message_boundaries: Arc::from(boundaries.clone()),
                static_sizes: Vec::new(),
                static_ttl_ms: None,
                message_ttl_ms: Some(300_000),
                cache_touched: true,
                prefix_tokens: 500,
                now,
                cache_write_5m_tokens: None,
                cache_write_1h_tokens: None,
                uniform_ttl_ms: None,
                request_id: "r-0".into(),
                seq: 1,
                attempt: 1,
                message_id: None,
            },
        ))
        .await;
    let placed = parts
        .router
        .select(phoebis_routing::SelectRequest {
            request_id: "r-2".into(),
            attempt: 1,
            seq: 2,
            key_id: "desktop".into(),
            key_generation,
            agent_id: "main".into(),
            session_id: "s-fixture-2".into(),
            canonical_model: "claude-sonnet-5-5".into(),
            folded_model: "claude-sonnet-5-5".into(),
            candidates: phoebis_routing::resolve(
                &snapshot,
                &key_row,
                "claude-sonnet-5-5",
                "claude-sonnet-5-5",
            )
            .expect("the pool's model rules allow claude-sonnet-5-5"),
            portable: true,
            credit_account: None,
            attempted: Vec::new(),
            now,
            received_at: now,
            fallback_ms: 60_000,
            revision: snapshot.revision,
            probe: false,
            count: false,
            request_class: None,
            context_compacted: false,
            static_hash: None,
            boundaries: Arc::from([boundaries.as_slice(), &["b-fixture-3".to_string()]].concat()),
            client: ClientClass::ClaudeCode,
            entrypoint: None,
            static_boundaries: Arc::from([]),
            marker: false,
            parent_agent_id: None,
            failed_over: None,
            holds: vec![phoebis_routing::HoldFacts {
                account_id: "direct".into(),
                cohort: Some("direct".into()),
                held_ms: 1_250,
                attempt: 1,
                seq: 1,
                ran_out: false,
                reason: "cap",
                until: None,
                waits_for: None,
            }],
            dialed: false,
            held_out: false,
            lane: lane_key("claude-sonnet-5-5", "standard", "none", "-"),
            fast: false,
            classifier: false,
            body_tokens: 0,
            effort: None,
        })
        .await
        .unwrap();
    // The request row carries the same summary as the attempt's trace, as the relay writes it on completion.
    let (placed_json, placed_held) = match &placed.result {
        phoebis_routing::SelectResult::Selected(selection) => {
            assert!(
                selection.trace.placement.is_some() && selection.trace.cohort_hold.is_some(),
                "the warm prefix places the request and the hold is on its trace, not {:?}",
                selection.trace
            );
            (
                serde_json::to_string(&selection.trace).unwrap(),
                serde_json::to_string(&selection.trace.cohort_hold).unwrap(),
            )
        }
        other => panic!("the warm prefix's account answers a selection, not {other:?}"),
    };
    // Its in-flight slot goes back now, so no later capture reads the account as busy.
    drop(placed);
    let bucket = phoebis_ledger::bucket_of(now - 60_000);
    parts
        .db
        .insert_samples(
            &[
                // Both samplers spell a window by the usage endpoint's key; the fixture carries one sample of each
                // source, and a weekly window no family row names, which the console keeps under its own key.
                QuotaSampleRecord {
                    account_id: "business".into(),
                    bucket,
                    window: "five_hour".into(),
                    utilization: Some(0.62),
                    reset_at: Some(now + 3_600_000),
                    source: "poll".into(),
                    observed_at: None,
                },
                QuotaSampleRecord {
                    account_id: "business".into(),
                    bucket,
                    window: "seven_day".into(),
                    utilization: Some(0.41),
                    reset_at: Some(now + 86_400_000),
                    source: "head".into(),
                    observed_at: None,
                },
                QuotaSampleRecord {
                    account_id: "business".into(),
                    bucket,
                    window: "seven_day_fable".into(),
                    utilization: Some(0.12),
                    reset_at: Some(now + 86_400_000),
                    source: "poll".into(),
                    observed_at: None,
                },
                QuotaSampleRecord {
                    account_id: "business".into(),
                    bucket,
                    window: "seven_day_opus".into(),
                    utilization: Some(0.03),
                    reset_at: Some(now + 86_400_000),
                    source: "poll".into(),
                    observed_at: None,
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
                spend_limit_minor: None,
                spend_used_minor: None,
                spend_percent: None,
                spend_currency: None,
                spend_exponent: None,
            }),
        )
        .unwrap();
    // The seat's poll with its body's `spend` object, then a head in a later bucket that moves the share and names the
    // reset, so the account row draws its spend window from both.
    for extra in [
        ExtraUsageSampleRecord {
            account_id: "usage-seat".into(),
            bucket: phoebis_ledger::bucket_of(now - 600_000),
            is_enabled: true,
            in_use: None,
            monthly_limit: Some(250_000),
            used_credits: Some(102_500),
            utilization: Some(0.41),
            currency: Some("USD".into()),
            disabled_reason: None,
            overage_reset: None,
            source: "poll".into(),
            spend_limit_minor: Some(250_000),
            spend_used_minor: Some(102_500),
            spend_percent: Some(41.0),
            spend_currency: Some("USD".into()),
            spend_exponent: Some(2),
        },
        ExtraUsageSampleRecord {
            account_id: "usage-seat".into(),
            bucket,
            is_enabled: true,
            in_use: None,
            monthly_limit: None,
            used_credits: None,
            utilization: Some(0.4),
            currency: None,
            disabled_reason: None,
            overage_reset: Some(common::month_end()),
            source: "head".into(),
            spend_limit_minor: None,
            spend_used_minor: None,
            spend_percent: None,
            spend_currency: None,
            spend_exponent: None,
        },
    ] {
        parts.db.insert_samples(&[], Some(&extra)).unwrap();
    }
    let hour_ago = now - 3_600_000;
    parts.ledger.record(Record::Request {
        request: RequestRecord {
            attempts: 2,
            agent_type: Some("Explore".into()),
            compaction: None,
            context_compacted: Some("auto".into()),
            prev_tool_ms: Some(1_235),
            turn_origin: Some("human".into()),
            forked_from: Some("s0".into()),
            fork_depth: Some(2),
            auto_mode: Some("server".into()),
            held: Some(placed_held),
            diagnostics_source: Some("router".into()),
            // An observing guardrail matched and let the request through, so the row carries a match and no block.
            guardrails: vec![GuardrailMatchRecord {
                guardrail_id: "subagent-status-summary".into(),
                position: 0,
                blocked: false,
            }],
            // The serving attempt started 400 ms in, so the head and the first content follow its write.
            ttfb_ms: Some(400 + 120),
            ttfc_ms: Some(400 + 300),
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
                account_kind: "api_key".into(),
                cooldown_scope: Some("account".into()),
                cooldown_until: Some(now + 1_800_000),
                selection: Some(placed_json),
                stop_reason: None,
                ..attempt("r-failover", 1, "direct", 429, "rate_limited", hour_ago)
            },
            AttemptRecord {
                selection: Some(trace_json.clone()),
                stop_reason: Some("tool_use".into()),
                speed: Some("fast".into()),
                ..attempt(
                    "r-failover",
                    2,
                    "business",
                    200,
                    "succeeded",
                    hour_ago + 400,
                )
            },
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
        detail: Some(
            j!({ "agent_id": "main", "from_account": "direct", "epoch": 2, "reason": "spent" })
                .to_string(),
        ),
        pool_id: None,
        agent_id: Some("main".into()),
        request_id: None,
    }));
    parts.ledger.record(Record::Event(EventRecord {
        at: hour_ago + 200,
        kind: "pin_expired".into(),
        account_id: Some("business".into()),
        key_id: Some("desktop".into()),
        session_id: Some("s1".into()),
        model: Some("claude-sonnet-5".into()),
        actor: None,
        detail: Some(j!({ "agent_id": "main", "reasons": ["cooldown", "spent"] }).to_string()),
        pool_id: None,
        agent_id: Some("main".into()),
        request_id: None,
    }));
    // The pool's model_rules excludes claude-haiku-*, and the account this cools is one of its members, so the
    // key-models fixture reads the pool's deny entry with no cooldown while a cooldown its pattern matches is live.
    parts
        .router
        .report(phoebis_routing::Report::Cooldown {
            account_id: "business".into(),
            row: phoebis_store::CooldownRow {
                account_id: "business".into(),
                scope: "model:claude-haiku-4-5-20251001".into(),
                until: now + 1_800_000,
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
    // An answer to a fast request carried the fast lane's two buckets, so the account rows show the pair.
    let fast_reading = |utilization: f64| phoebis_routing::WindowReading {
        utilization,
        reset: Some(now + 30_000),
        observed_at: now - 5_000,
        source: phoebis_routing::WindowSource::Head,
        plan: None,
        tier: None,
    };
    parts
        .router
        .report(phoebis_routing::Report::Sample {
            account_id: "business".into(),
            sample: phoebis_routing::UtilizationSample {
                observed_at: now - 5_000,
                ..Default::default()
            }
            .with(phoebis_relay::FAST_INPUT, fast_reading(0.42))
            .with(phoebis_relay::FAST_OUTPUT, fast_reading(0.08)),
            incarnation: 1,
            source_request_id: None,
        })
        .await;
    parts.ledger.record(Record::Event(EventRecord {
        at: hour_ago + 300,
        kind: "model_rejected".into(),
        account_id: Some("business".into()),
        key_id: Some("desktop".into()),
        session_id: Some("s1".into()),
        model: Some("claude-haiku-4-5-20251001".into()),
        actor: None,
        detail: Some(j!({ "attempt": 1, "request_id": "req-rejected", "source_request_id": "r-failover", "wire_model": "claude-haiku-4-5" }).to_string()),
        pool_id: None,
        agent_id: None,
        request_id: Some("r-failover".into()),
    }));
    parts.ledger.record(Record::Event(EventRecord {
        at: hour_ago + 350,
        kind: "model_permitted".into(),
        account_id: Some("direct".into()),
        key_id: Some("desktop".into()),
        session_id: Some("s1".into()),
        model: Some("claude-haiku-4-5-20251001".into()),
        actor: None,
        detail: Some(j!({ "attempt": 2, "rejected_by": ["business"], "request_id": "req-permitted", "source_request_id": "r-failover", "wire_model": "claude-haiku-4-5" }).to_string()),
        pool_id: None,
        agent_id: None,
        request_id: Some("r-failover".into()),
    }));
    // The serving attempt's stream was cut after its head, so the detail answers `stream_cut`. The row sits at the
    // request's finish, where the ledger writes it, so the detail's read of the request's window holds it.
    parts.ledger.record(Record::Event(EventRecord {
        at: hour_ago + 900,
        kind: "stream_interrupted".into(),
        account_id: Some("business".into()),
        key_id: Some("desktop".into()),
        session_id: Some("s1".into()),
        model: Some("claude-sonnet-5-5".into()),
        actor: None,
        detail: Some(j!({ "code": "stream_cut", "text": "malformed response: connection closed inside chunked body", "bytes_relayed": 2_048, "upstream_request_id": "req_r-failover_2", "attempt": 2 }).to_string()),
        pool_id: None,
        agent_id: None,
        request_id: Some("r-failover".into()),
    }));
    // Two weeks of `business`'s 7-day window, one reset the clock walk found, and one Fable week, so the unused
    // allowance capture carries both `observed` words and each reset's pair, the seeded profile's. Days back, so the
    // routing log's hour and the failover's window hold none of them.
    for (window, utilization_before, at, observed) in [
        ("seven_day", 0.62, now - 7 * DAY, "sample"),
        ("seven_day", 0.85, now - 14 * DAY, "clock"),
        ("seven_day_fable", 0.4, now - 10 * DAY, "sample"),
    ] {
        parts.ledger.record(Record::Event(EventRecord {
            at,
            kind: "window_reset".into(),
            account_id: Some("business".into()),
            detail: Some(j!({ "window": window, "utilization_before": utilization_before, "plan": "max_20x", "tier": "default_claude_max_20x", "reset_at": at, "next_reset": at + 7 * DAY, "observed": observed }).to_string()),
            ..Default::default()
        }));
    }
    // The free organization's last reading of the week its term end cut short.
    let free_read_at = free_end - 3 * DAY;
    parts
        .db
        .insert_samples(
            &[QuotaSampleRecord {
                account_id: "free-org".into(),
                bucket: phoebis_ledger::bucket_of(free_read_at),
                window: "seven_day".into(),
                utilization: Some(0.3),
                reset_at: Some(free_end + 4 * DAY),
                source: "poll".into(),
                observed_at: Some(free_read_at),
            }],
            None,
        )
        .unwrap();
    parts.ledger.record(Record::Request {
        request: RequestRecord {
            bodies_captured: true,
            diagnostics_source: Some("client".into()),
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
        // Pending beside the other rows' `none`, so the capture carries both words the relay reads from the member.
        attempts: vec![AttemptRecord {
            cache_miss_reason: Some("pending".into()),
            ..attempt("r-captured", 1, "business", 200, "succeeded", now - 60_000)
        }],
        bodies: Some(BodyRecord {
            request_id: "r-captured".into(),
            request_body: br#"{"model":"claude-sonnet-5","messages":[]}"#.to_vec(),
            response_body: Some(br#"{"id":"msg","content":[]}"#.to_vec()),
            request_len_seen: br#"{"model":"claude-sonnet-5","messages":[]}"#.len() as u64,
            response_len_seen: br#"{"id":"msg","content":[]}"#.len() as u64,
            expires_at: now + 86_400_000,
        }),
    });
    parts.ledger.record(Record::Request {
        request: RequestRecord {
            client_drift: Some("hash".into()),
            ..request(
                key_generation,
                "r-failed",
                now - 120_000,
                "desktop",
                "s2",
                502,
                "failed",
            )
        },
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
    // A committed stream whose body ended before its `message_stop`, so the requests route answers the sixth
    // outcome and the analytics groups count it in their own column.
    parts.ledger.record(Record::Request {
        request: RequestRecord {
            error_code: Some("stream_incomplete".into()),
            ..request(
                key_generation,
                "r-incomplete",
                hour_ago + 600,
                "desktop",
                "s1",
                200,
                "incomplete",
            )
        },
        attempts: vec![attempt(
            "r-incomplete",
            1,
            "business",
            200,
            "upstream_error",
            hour_ago + 600,
        )],
        bodies: None,
    });
    // A row whose serving attempt carries a placement, so the placements route has a rung to answer rather
    // than an empty list. It stays on a model the price table lists, since its API-key upstream spends dollars the
    // spend fixtures read.
    parts.ledger.record(Record::Request {
        request: RequestRecord {
            model: "claude-sonnet-5".into(),
            canonical_model: "claude-sonnet-5".into(),
            ..request(
                key_generation,
                "r-placed",
                now - 30_000,
                "desktop",
                "s3",
                200,
                "succeeded",
            )
        },
        attempts: vec![AttemptRecord {
            account_kind: "api_key".into(),
            selection: Some(
                r#"{"placement":{"rung":"content","depth":2,"prefix_tokens":500}}"#.into(),
            ),
            ..attempt("r-placed", 1, "direct", 200, "succeeded", now - 30_000)
        }],
        bodies: None,
    });
    // A row an earlier key under the same id spent, so the key grouping answers the deleted holder beside
    // the live key and the decoder sees the label.
    parts.ledger.record(Record::Request {
        request: request(
            now - 60_000,
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
    // An auxiliary request a guardrail refused at admission: no attempt and no captured body, the observing
    // guardrail's match beside the blocking one's. The client received the 403 while no upstream answered. Older
    // than the rows above it, so the list keeps their order, and the helper's session argument is overridden to
    // none below, so the session rows keep their counts.
    let blocked_at = hour_ago - 600;
    parts.ledger.record(Record::Request {
        request: RequestRecord {
            session_id: None,
            finished_at: Some(blocked_at + 4),
            attempts: 0,
            request_bytes: Some(48_213),
            ttfb_ms: None,
            ttfc_ms: None,
            duration_ms: Some(4),
            error_code: Some("guardrail_blocked".into()),
            // The seed writes what `finish` would set, since the harness's `record` skips the assembler.
            refused: true,
            request_class: Some("auxiliary".into()),
            stream_end_at: None,
            guardrail_id: Some("large-auxiliary".into()),
            guardrails: vec![
                GuardrailMatchRecord {
                    guardrail_id: "subagent-status-summary".into(),
                    position: 0,
                    blocked: false,
                },
                GuardrailMatchRecord {
                    guardrail_id: "large-auxiliary".into(),
                    position: 1,
                    blocked: true,
                },
            ],
            ..request(
                key_generation,
                "r-blocked",
                blocked_at,
                "desktop",
                "s1",
                403,
                "rejected",
            )
        },
        attempts: Vec::new(),
        bodies: None,
    });
    // The client's own auto-mode classifier call, served on the plan at no window share, and a request refused for
    // its missing class header. The refusal is a row of its own, since the guardrail step answers before the
    // contract and the blocked row stays a guardrail refusal. Both are older than the blocked row, so the list's
    // first rows keep their places, and under s2, so s1's session row keeps its count.
    let classifier_at = blocked_at - 600;
    parts.ledger.record(Record::Request {
        request: RequestRecord {
            request_class: Some("auxiliary".into()),
            classifier: Some("client".into()),
            ..request(
                key_generation,
                "r-classifier",
                classifier_at,
                "desktop",
                "s2",
                200,
                "succeeded",
            )
        },
        attempts: vec![attempt(
            "r-classifier",
            1,
            "business",
            200,
            "succeeded",
            classifier_at,
        )],
        bodies: None,
    });
    let refused_at = blocked_at - 1_200;
    parts.ledger.record(Record::Request {
        request: RequestRecord {
            finished_at: Some(refused_at + 3),
            attempts: 0,
            ttfb_ms: None,
            ttfc_ms: None,
            duration_ms: Some(3),
            error_code: Some("client_contract".into()),
            client_contract: Some("class_header".into()),
            // The seed writes what `finish` would set, since the harness's `record` skips the assembler.
            refused: true,
            stream_end_at: None,
            ..request(
                key_generation,
                "r-contract",
                refused_at,
                "desktop",
                "s2",
                403,
                "rejected",
            )
        },
        attempts: Vec::new(),
        bodies: None,
    });
    // A request the usage-based seat served, billed in dollars under the seat's own funding word, so the request rows
    // carry the attempt's class with one true. Older than the list's first rows and in no session, so the list's first
    // rows keep their places and the session rows their counts. The seat has no plan window, so its attempt carries no
    // 5-hour reading.
    let seat_at = refused_at - 600;
    parts.ledger.record(Record::Request {
        request: RequestRecord {
            session_id: None,
            ..request(
                key_generation,
                "r-usage-seat",
                seat_at,
                "desktop",
                "s1",
                200,
                "succeeded",
            )
        },
        attempts: vec![AttemptRecord {
            usage_based: true,
            ratelimit_5h_utilization: None,
            ratelimit_5h_reset: None,
            ..attempt("r-usage-seat", 1, "usage-seat", 200, "succeeded", seat_at)
        }],
        bodies: None,
    });
    // Three answers the upstream refused, each served with the stop reason `refusal`, one category twice and another
    // once, so the request rows carry the refused attempts' responses and the refusal count its rows. Older than every
    // other row and in no session, so the list's first rows keep their places and the session rows their counts.
    for (id, at, category) in [
        ("r-refused-1", seat_at - 1_800, "harmful_request"),
        ("r-refused-2", seat_at - 1_200, "harmful_request"),
        ("r-refused-3", seat_at - 600, "restricted_topic"),
    ] {
        parts.ledger.record(Record::Request {
            request: RequestRecord {
                session_id: None,
                ..request(key_generation, id, at, "desktop", "s1", 200, "succeeded")
            },
            attempts: vec![AttemptRecord {
                stop_reason: Some("refusal".into()),
                refusal_category: Some(category.into()),
                ..attempt(id, 1, "business", 200, "succeeded", at)
            }],
            bodies: None,
        });
    }
    parts
        .keepalive
        .apply_event(
            "desktop",
            key_generation,
            phoebis_keepalive::Event {
                event: "session_start".into(),
                session_id: "s1".into(),
                instance_id: "i1".into(),
                sequence: 1,
                ..Default::default()
            },
        )
        .unwrap();
    // The companion session's main pin and a pin of each of its two agents, each placed under an effort word, renewed
    // with a prompt and holding one live cache-map entry rooted at the session, so `sessions.json` carries the row's
    // prompt and effort and the lines of main and its agents. Each selection is narrowed to one member, so no two pins
    // share an account and a model, and each entry lands on its selection's decision, so the ledger counts no orphan
    // for it. The pins are placed a minute before the routing log's hour, which holds the routing stories alone.
    let placed_at = hour_ago - 60_000;
    for (agent, model, account, effort, prompt, prefix_tokens) in [
        (
            "main",
            "claude-sonnet-5-5",
            "business",
            "high",
            52_000,
            41_000,
        ),
        (
            "worker-1",
            "claude-sonnet-5",
            "business",
            "low",
            31_000,
            24_000,
        ),
        (
            "worker-2",
            "claude-sonnet-5-5",
            "direct",
            "low",
            18_500,
            12_000,
        ),
    ] {
        let request_id = format!("r-s1-{agent}");
        let candidates = phoebis_routing::resolve(&snapshot, &key_row, model, model)
            .expect("the pool's model rules allow the model")
            .into_iter()
            .filter(|c| c.account_id == account)
            .collect();
        let answer = parts
            .router
            .select(phoebis_routing::SelectRequest {
                request_id: request_id.clone(),
                attempt: 1,
                seq: 1,
                key_id: "desktop".into(),
                key_generation,
                agent_id: agent.into(),
                session_id: "s1".into(),
                canonical_model: model.into(),
                folded_model: model.into(),
                candidates,
                portable: true,
                credit_account: None,
                attempted: Vec::new(),
                now: placed_at,
                received_at: placed_at,
                fallback_ms: 60_000,
                revision: snapshot.revision,
                probe: false,
                count: false,
                request_class: None,
                context_compacted: false,
                static_hash: None,
                boundaries: Arc::from([]),
                client: ClientClass::ClaudeCode,
                entrypoint: None,
                static_boundaries: Arc::from([]),
                marker: false,
                parent_agent_id: (agent != "main").then(|| "main".into()),
                failed_over: None,
                holds: Vec::new(),
                dialed: false,
                held_out: false,
                lane: lane_key(model, "standard", "none", "-"),
                fast: false,
                classifier: false,
                body_tokens: 0,
                effort: Some(effort.into()),
            })
            .await
            .unwrap();
        let selection = match &answer.result {
            phoebis_routing::SelectResult::Selected(selection) => selection.clone(),
            other => panic!("{agent}'s one member answers a selection, not {other:?}"),
        };
        // Its in-flight slot goes back now, so no later capture reads the account as busy.
        drop(answer);
        parts
            .router
            .report(phoebis_routing::Report::PinRenewed {
                pin: selection.pin.expect("a session's selection holds a pin"),
                now,
                usage: Some(phoebis_routing::PinUsage {
                    input: prompt,
                    ..phoebis_routing::PinUsage::default()
                }),
                origin: "client",
                cache_ttl_ms: 300_000,
                fast: false,
                classifier: false,
            })
            .await;
        parts
            .router
            .report(phoebis_routing::Report::Affinity(
                phoebis_routing::AffinityReport {
                    account_id: account.into(),
                    incarnation: selection.account.incarnation,
                    key_id: "desktop".into(),
                    key_generation,
                    canonical_model: model.into(),
                    lane: lane_key(model, "standard", "none", "-"),
                    root: "s1".into(),
                    static_boundaries: Arc::from([]),
                    message_boundaries: Arc::from([format!("b-s1-{agent}")]),
                    static_sizes: Vec::new(),
                    static_ttl_ms: None,
                    message_ttl_ms: Some(300_000),
                    cache_touched: true,
                    prefix_tokens,
                    now,
                    cache_write_5m_tokens: None,
                    cache_write_1h_tokens: None,
                    uniform_ttl_ms: None,
                    request_id,
                    seq: 1,
                    attempt: 1,
                    message_id: None,
                },
            ))
            .await;
    }
    // One auxiliary request of main on a second model, naming no effort, so `sessions.json` reads the row from main's
    // conversation pin beside a pin only auxiliary requests have used, which draws no line. It is placed at the pins'
    // instant, before the routing log's hour, and renewed at its answer's end as theirs are, inside the pool's TTL.
    let auxiliary = parts
        .router
        .select(phoebis_routing::SelectRequest {
            now: placed_at,
            received_at: placed_at,
            request_class: Some("auxiliary".into()),
            ..asked(&parts, key_generation, "r-s1-auxiliary", 1, Some("s1"))
        })
        .await
        .unwrap();
    let selection = match &auxiliary.result {
        phoebis_routing::SelectResult::Selected(selection) => selection.clone(),
        other => panic!("the auxiliary request is placed, not {other:?}"),
    };
    drop(auxiliary);
    parts
        .router
        .report(phoebis_routing::Report::PinRenewed {
            pin: selection.pin.expect("a session's selection holds a pin"),
            now,
            usage: None,
            origin: "client",
            cache_ttl_ms: 300_000,
            fast: false,
            classifier: false,
        })
        .await;
    // The two agents register and each sends one turn, so the keepalive's view carries their recipes and the row lists
    // their lines. Main's later `state` event takes a sequence past theirs.
    for (sequence, agent, model, account) in [
        (2, "worker-1", "claude-sonnet-5", "business"),
        (3, "worker-2", "claude-sonnet-5-5", "direct"),
    ] {
        parts
            .keepalive
            .apply_event(
                "desktop",
                key_generation,
                phoebis_keepalive::Event {
                    event: "agent_start".into(),
                    session_id: "s1".into(),
                    instance_id: "i1".into(),
                    sequence,
                    agent_id: Some(agent.into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let request_id = format!("k-{agent}");
        parts.keepalive.observe(Event::RequestObserved(RequestObserved { request_id: request_id.clone(), origin: Origin::Client, kind: Kind::Inference, key_id: Some("desktop".into()), key_generation, pool_id: Some("main".into()), session_id: Some("s1".into()), agent_id: Some(agent.into()), parent_agent_id: Some("main".into()), owner_id: None, model: model.into(), canonical_model: model.into(), wire_model: model.into(), stream: false, headers: vec![], body: bytes::Bytes::from(format!(r#"{{"model":"{model}","max_tokens":10,"messages":[{{"role":"user","content":"hi"}}]}}"#)), body_capture_hours: 0, shape: RequestShape::default(), static_hash: None, boundaries: Vec::new(), client: phoebis_bus::ClientFacts::default(), auto_mode: None, fork: phoebis_bus::Fork::default(), presented_retired: false, rotation_at: None, guardrail_id: None, guardrails: Vec::new(), received_at: now }));
        parts
            .keepalive
            .observe(Event::AccountSelected(AccountSelected {
                request_id: request_id.clone(),
                attempt: 1,
                account_id: account.into(),
                account_kind: if account == "direct" {
                    "api_key"
                } else {
                    "oauth"
                }
                .into(),
                usage_based: false,
                pin_epoch: Some(1),
                started_at: now,
                cache_ttl_ms: None,
                selection: None,
            }));
        parts
            .keepalive
            .observe(Event::RequestComplete(RequestComplete {
                request_id: request_id.clone(),
                attempt: 1,
                account_id: Some(account.into()),
                outcome: phoebis_bus::Outcome::Succeeded,
                status: 200,
                usage: Some(Usage {
                    cache_write_5m_tokens: Some(2_000),
                    ..Default::default()
                }),
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
                message_id: Some(format!("msg_{request_id}_1")),
                skipped_events: 0,
                interruption: None,
                finished_at: now,
            }));
    }
    parts.keepalive.observe(Event::RequestObserved(RequestObserved { request_id: "k1".into(), origin: Origin::Client, kind: Kind::Inference, key_id: Some("desktop".into()), key_generation, pool_id: Some("main".into()), session_id: Some("s1".into()), agent_id: Some("main".into()), parent_agent_id: None, owner_id: None, model: "claude-sonnet-5".into(), canonical_model: "claude-sonnet-5".into(), wire_model: "claude-sonnet-5".into(), stream: false, headers: vec![], body: bytes::Bytes::from(r#"{"model":"claude-sonnet-5","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}"#), body_capture_hours: 0, shape: RequestShape::default(), static_hash: None, boundaries: Vec::new(), client: phoebis_bus::ClientFacts::default(), auto_mode: None, fork: phoebis_bus::Fork::default(), presented_retired: false, rotation_at: None, guardrail_id: None, guardrails: Vec::new(), received_at: now }));
    parts
        .keepalive
        .observe(Event::AccountSelected(AccountSelected {
            request_id: "k1".into(),
            attempt: 1,
            account_id: "direct".into(),
            account_kind: "api_key".into(),
            usage_based: false,
            pin_epoch: Some(1),
            started_at: now,
            cache_ttl_ms: None,
            selection: None,
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
            message_id: Some("msg_k1_1".into()),
            skipped_events: 0,
            interruption: None,
            finished_at: now,
        }));
    let ready = wait_json(app, "/admin/requests", &cookie, |v| {
        v["requests"].as_array().unwrap().len() == 13
    })
    .await;
    hold(&planted, "requests", "GET /admin/requests", ready);
    // A plan-funded answer on a model the shipped cost model does not list, whose id names a model tier it weighs, so
    // the attention capture holds the unweighted model's item and the analytics captures a figure priced as an
    // estimate. An unlisted id rather than a listed unweighted one, so a revision that weighs a listed model moves
    // nothing here. It lands after the requests capture and in the newest hour, so the request list keeps its rows and
    // the newest priced burn point still carries cache reads and output.
    parts.ledger.record(Record::Request {
        request: RequestRecord {
            model: "claude-opus-99".into(),
            canonical_model: "claude-opus-99".into(),
            session_id: None,
            ..request(
                key_generation,
                "r-unweighted",
                now - 60_000,
                "desktop",
                "s1",
                200,
                "succeeded",
            )
        },
        attempts: vec![attempt(
            "r-unweighted",
            1,
            "business",
            200,
            "succeeded",
            now - 60_000,
        )],
        bodies: None,
    });
    fake_upstream::wait_for(Duration::from_secs(5), || {
        parts
            .attention
            .conditions(now)
            .unwrap()
            .iter()
            .any(|c| c.kind == phoebis_attention::Kind::ModelUnweighted && c.holds)
            .then_some(())
    })
    .await;
    // One exhaustion on the harness's pool, fed to the reader as the bus would, so the attention capture carries an
    // item of the shape the console decodes.
    parts
        .attention
        .observe(&Event::RequestObserved(RequestObserved {
            request_id: "x1".into(),
            origin: Origin::Client,
            kind: Kind::Inference,
            key_id: Some("desktop".into()),
            key_generation,
            pool_id: Some("main".into()),
            session_id: Some("s1".into()),
            agent_id: Some("main".into()),
            parent_agent_id: None,
            owner_id: None,
            model: "claude-sonnet-5".into(),
            canonical_model: "claude-sonnet-5".into(),
            wire_model: "claude-sonnet-5".into(),
            stream: false,
            headers: vec![],
            body: bytes::Bytes::from_static(b"{}"),
            body_capture_hours: 0,
            shape: RequestShape::default(),
            static_hash: None,
            boundaries: Vec::new(),
            client: phoebis_bus::ClientFacts::default(),
            auto_mode: None,
            fork: phoebis_bus::Fork::default(),
            presented_retired: false,
            rotation_at: None,
            guardrail_id: None,
            guardrails: Vec::new(),
            received_at: now,
        }));
    parts
        .attention
        .observe(&Event::RequestComplete(RequestComplete {
            request_id: "x1".into(),
            attempt: 0,
            account_id: None,
            outcome: phoebis_bus::Outcome::Exhausted,
            status: 429,
            usage: None,
            response: ResponseFacts::default(),
            ttfb_ms: None,
            ttfc_ms: None,
            error_code: Some("accounts_exhausted".into()),
            client_drift: None,
            client_contract: None,
            client_version: None,
            response_bytes: None,
            response_chunks: None,
            held: None,
            diagnostics: None,
            message_id: None,
            skipped_events: 0,
            interruption: None,
            finished_at: now,
        }));
    // The replay's counts while the parked table is still empty.
    let replayed = post(app, "/admin/ledger/parked/replay", "", Some(&cookie)).await;
    assert_eq!(replayed.status(), 200);
    hold(
        &planted,
        "replay",
        "POST /admin/ledger/parked/replay",
        json(replayed).await,
    );
    // One priced record parked on the harness's pool, so the attention capture carries the parked item beside the
    // exhaustion.
    common::park_unreadable(&parts.db, "main", "business", now);
    // A 5-hour window of `business` that ended two hours ago and charged 1.4 times the table.
    let window_end = now - 2 * 3_600_000;
    // One row of each routing condition, as the ledger's hourly fold writes them, so the attention capture carries the
    // four kinds beside the others, the drift naming the window above.
    let condition =
        |kind: &str, subject_kind: &str, subject: &str, pool: Option<&str>, since: i64, detail| {
            phoebis_ledger::RoutingCondition {
                kind: kind.into(),
                subject_kind: subject_kind.into(),
                subject_id: subject.into(),
                pool_id: pool.map(String::from),
                since,
                count: 1,
                detail,
                computed_at: now,
            }
        };
    common::seed_conditions(
        &parts,
        &[
            condition(
                "wall_reached",
                "account",
                "business",
                None,
                hour_ago + 40 * 60_000,
                j!({
                    "early_ms": null,
                    "projection_error": 0.04,
                    "setting": ["shed_cap", "window_ceiling"],
                    "walls": 1,
                    "windows": ["five_hour"],
                }),
            ),
            condition(
                "destination_spent",
                "account",
                "business",
                None,
                hour_ago + 10 * 60_000,
                j!({
                    "fits": { "rewrite_only": 1 },
                    "moves": 1,
                    "setting": ["window_ceiling"],
                    "windows": ["five_hour"],
                }),
            ),
            condition(
                "shed_no_runway",
                "pool",
                "main",
                Some("main"),
                hour_ago + 5 * 60_000,
                j!({ "accounts": ["business"], "rounds": 1, "setting": ["window_ceiling"] }),
            ),
            condition(
                "meter_drift",
                "account",
                "business",
                None,
                window_end,
                j!({
                    "effective": phoebis_ledger::CostRevisions::shipped().in_force(now).effective,
                    "source": "shipped",
                    "windows": [{
                        "observed_rise": 0.7,
                        "predicted_share": 0.5,
                        "ratio": 1.4,
                        "requests": 3,
                        "window": "five_hour",
                        "window_end": window_end,
                    }],
                }),
            ),
        ],
    )
    .await;
    // The window's reconciled row, as the hourly fold writes it, so the reconciliations capture carries a row. It lands
    // after the ledger's fold at start, which would otherwise read it and write the drift row the seeded one above
    // collides with.
    parts
        .db
        .write(|c| {
            c.execute(
                "INSERT INTO window_reconciliations (account_id, window, window_end, first_reading_at, \
                 last_reading_at, observed_rise, predicted_share, ratio, requests, computed_at) \
                 VALUES ('business', 'five_hour', ?1, ?2, ?3, 0.7, 0.5, 1.4, 3, ?4)",
                rusqlite::params![
                    window_end,
                    window_end - 4 * 3_600_000,
                    window_end - 30 * 60_000,
                    now - 3_600_000
                ],
            )?;
            Ok(())
        })
        .unwrap();
    // The GET reads no endpoint of its own, so the capture reads a clean poll's snapshot and standing.
    parts.usage.poll("business").await.unwrap();
    // The affinity report above stands for an earlier request, `r-0`, whose decision the ledger never stored, so its
    // cache map is the one orphan `system.json` counts. It is waited for, so every capture reads the same figure.
    let stats = parts.ledger.stats();
    fake_upstream::wait_for(std::time::Duration::from_secs(5), || {
        (stats
            .cache_map_orphans
            .load(std::sync::atomic::Ordering::Relaxed)
            == 1)
            .then_some(())
    })
    .await;
    // The ledger's mirror reads the accounts on its own tick, so the transition the pairing above made lands a tick
    // later. It is waited for, so the `written` figure `system.json` holds counts it in every capture.
    fake_upstream::wait_for(std::time::Duration::from_secs(15), || {
        let transitions = parts
            .db
            .read_blocking(|c| {
                Ok(c.query_row(
                    "SELECT COUNT(*) FROM events WHERE kind = 'account_state_changed' \
                     AND json_extract(detail, '$.cause') = 'paired'",
                    [],
                    |r| r.get::<_, i64>(0),
                )?)
            })
            .unwrap();
        (transitions > 0).then_some(())
    })
    .await;
    for (name, route) in [
        ("account", "/admin/accounts/business"),
        ("account_catalog", "/admin/accounts/business/catalog"),
        ("account_quota", "/admin/accounts/business/quota?range=7d"),
        ("account_usage", "/admin/accounts/business/usage"),
        ("accounts", "/admin/accounts"),
        ("analytics_accounts", "/admin/analytics/accounts?range=24h"),
        (
            "analytics_breakdown",
            "/admin/analytics/breakdown?range=24h&group_by=key",
        ),
        (
            "analytics_breakdown_accounts",
            "/admin/analytics/breakdown?range=24h&group_by=account",
        ),
        (
            "analytics_breakdown_speed",
            "/admin/analytics/breakdown?range=24h&group_by=speed",
        ),
        ("analytics_budgets", "/admin/analytics/budgets"),
        (
            "analytics_burn",
            "/admin/analytics/burn?range=24h&group_by=account",
        ),
        (
            "analytics_guardrails",
            "/admin/analytics/guardrails?range=24h",
        ),
        (
            "analytics_placements",
            "/admin/analytics/placements?range=24h",
        ),
        (
            "analytics_reconciliations",
            "/admin/analytics/reconciliations",
        ),
        (
            "analytics_refusal_categories",
            "/admin/analytics/refusal_categories?range=24h",
        ),
        ("analytics_rewrites", "/admin/analytics/rewrites?range=24h"),
        (
            "analytics_series",
            "/admin/analytics/series?range=24h&group_by=key",
        ),
        (
            "analytics_series_accounts",
            "/admin/analytics/series?range=24h&group_by=account",
        ),
        (
            "analytics_unused_allowance",
            "/admin/analytics/unused_allowance",
        ),
        ("attention", "/admin/attention"),
        ("dashboard", "/admin/dashboard"),
        ("guardrail_presets", "/admin/guardrails/presets"),
        (
            "guardrail_show",
            "/admin/guardrails/subagent-status-summary",
        ),
        ("guardrails", "/admin/guardrails"),
        ("keepalive_recipes", "/admin/keepalive/recipes"),
        ("key_models", "/admin/keys/desktop/models"),
        ("keys", "/admin/keys"),
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
    // The pool's one captured row carries no request class, so the observing guardrail's answer holds an unmatched
    // verdict with its failing index, the shape the console's test panel reads.
    let response = post(
        app,
        "/admin/guardrails/test",
        r#"{"guardrail":"subagent-status-summary","pool_id":"main","limit":50}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), 200, "the dry run answers");
    hold(
        &planted,
        "guardrail_test",
        "POST /admin/guardrails/test",
        json(response).await,
    );
    // Main goes idle and a tick opens its idle window, so the overview carries a pool's row with its ten buckets for
    // the console to decode. It is read after the others, which show main busy.
    let request_generation = parts
        .keepalive
        .agent_status("desktop", "s1", "main")
        .unwrap()
        .request_generation;
    parts
        .keepalive
        .apply_event(
            "desktop",
            key_generation,
            phoebis_keepalive::Event {
                event: "state".into(),
                session_id: "s1".into(),
                instance_id: "i1".into(),
                sequence: 4,
                agent_id: Some("main".into()),
                state: Some("idle".into()),
                request_generation: Some(request_generation),
                ..Default::default()
            },
        )
        .unwrap();
    parts.keepalive.step().await;
    let response = get(app, "/admin/keepalive/overview", Some(&cookie)).await;
    assert_eq!(response.status(), 200);
    hold(
        &planted,
        "keepalive_overview",
        "GET /admin/keepalive/overview",
        json(response).await,
    );
    // The web's arithmetic tests work their figures out by hand from the round numbers of the test tables' first
    // entry, so they read the two envelopes in force inside it, as the settings echo carries them, rendered from the
    // same loader the Rust tests use.
    hold(
        &planted,
        "test_tables",
        "phoebis_ledger::test_tables",
        serde_json::to_value(phoebis_admin::table_envelopes(
            &phoebis_ledger::test_tables::api_prices(),
            &phoebis_ledger::test_tables::subscription_costs(),
            phoebis_ledger::test_tables::JANUARY_2026,
        ))
        .unwrap(),
    );
    let probes = bound_probes(app, &cookie).await;
    hold(
        &planted,
        "bound_refusals",
        "PATCH /admin/pools/{id}",
        j!({ "probes": probes.clone() }),
    );
    // `hold` compares shapes, which a moved bound or sentence keeps; the web tests hold the form and the mock to
    // these values, so a route that moves one fails here instead of leaving them on the old answers.
    if std::env::var_os("PHOEBIS_WRITE_FIXTURES").is_none() {
        let committed: Value = serde_json::from_str(
            &std::fs::read_to_string(dir().join("bound_refusals.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            committed["body"]["probes"], probes,
            "a route answers a bound probe differently; re-capture and move the console's copy with it"
        );
    }
    let started = json(
        post(
            app,
            "/admin/oauth/sessions",
            r#"{"account_id":"business","login":"claude_ai"}"#,
            Some(&cookie),
        )
        .await,
    )
    .await;
    hold(
        &planted,
        "pairing_started",
        "POST /admin/oauth/sessions",
        started.clone(),
    );
    // A pairing answers only the session that started it, so the show reads the one the start opened under this
    // cookie. A pairing the seed opened would draw from the seeded source and move every id minted after it.
    let shown = format!(
        "/admin/oauth/sessions/{}",
        started["id"].as_str().expect("the started pairing's id")
    );
    let response = get(app, &shown, Some(&cookie)).await;
    assert_eq!(response.status(), 200, "{shown}");
    hold(
        &planted,
        "oauth_session",
        &format!("GET {shown}"),
        json(response).await,
    );

    // The routing log's hour, seeded after every other capture so its stories leave those files as they were.
    drop(selected);
    routing_stories(&parts, &cookie, key_generation).await;
    let hour_start = jiff::Timestamp::from_millisecond(hour_ago).unwrap();
    // The clock the start row stepped, so the page holds that row.
    let hour_end = jiff::Timestamp::from_millisecond(
        parts.clock_cell.load(std::sync::atomic::Ordering::SeqCst),
    )
    .unwrap();
    let every_kind = phoebis_ledger::DECISION_KINDS
        .iter()
        .chain([&phoebis_ledger::PROBE_KIND])
        .chain(phoebis_ledger::EVENT_KINDS)
        .copied()
        .collect::<Vec<_>>()
        .join(",");
    // Above the hour's row count, since the default page of 25 would cut it and the capture holds every shape.
    let log_route =
        format!("/admin/routing-log?from={hour_start}&to={hour_end}&kinds={every_kind}&limit=200");
    let kinds_of = |log: &Value, row_type: &str| {
        log["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["type"] == row_type)
            .map(|row| row["kind"].as_str().unwrap().to_string())
            .collect::<std::collections::BTreeSet<_>>()
    };
    let named = |kinds: &[&str]| {
        kinds
            .iter()
            .map(|k| k.to_string())
            .collect::<std::collections::BTreeSet<_>>()
    };
    // The guardrail block's record is try-sent, so the capture waits for it beside the kinds.
    let log = wait_json(app, &log_route, &cookie, |log| {
        kinds_of(log, "decision") == named(phoebis_ledger::DECISION_KINDS)
            && kinds_of(log, "event") == named(phoebis_ledger::EVENT_KINDS)
            && log["rows"].as_array().unwrap().iter().any(|row| {
                row["type"] == "decision"
                    && row["kind"] == "refused"
                    && row["refusal"]["kind"] == "guardrail"
            })
    })
    .await;
    assert!(
        log["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["type"] == "decision" && row["probe"] == true),
        "the hour holds a probe's decision"
    );
    assert!(
        log["next_cursor"].is_null(),
        "the capture holds the hour whole"
    );
    let decision_route = "/admin/requests/r-static/decisions/1";
    let decision = wait_json(app, decision_route, &cookie, |record| {
        record["outcome"]["completion"]["status"] == 200
            && record["outcome"]["completion"]["cache_map_written"].is_array()
    })
    .await;
    assert_eq!(
        decision["outcome"]["completion"]["hit"], true,
        "the placement read its static prefix"
    );
    for block in ["candidates", "walk"] {
        assert!(
            decision[block].as_array().is_some_and(|b| !b.is_empty()),
            "{block}"
        );
    }
    for block in ["pin", "request", "settings"] {
        assert!(decision[block].is_object(), "{block}");
    }
    let chosen = decision["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["verdict"] == "chosen")
        .expect("one member is the chosen one");
    let hash = chosen["prefix"]["static"]["hash"].as_str().unwrap_or("");
    assert!(
        hash.len() == 64
            && hash
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
        "the static lookup names the boundary as the affinity map keys it: {hash}"
    );
    assert!(
        chosen["prefix"]["static_match"]["ttl_ms"]
            .as_i64()
            .is_some_and(|ttl| ttl > 0),
        "the static rung accepted a live entry: {}",
        chosen["prefix"]
    );
    let started = log["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["kind"] == "router_started")
        .expect("the hour holds the start row");
    assert!(
        started["detail"]["down_since"].as_i64().unwrap() < started["at"].as_i64().unwrap(),
        "the start dates its gap from a row written before it: {started}"
    );
    let failover = json(get(app, "/admin/requests/r-failover", Some(&cookie)).await).await;
    let phases = &failover["request"]["phases"];
    assert!(
        phases["written_at"].as_i64().unwrap() < phases["head_at"].as_i64().unwrap(),
        "the serving attempt's write precedes the request's head: {phases}"
    );
    for (name, route, body) in [
        ("decision", decision_route.to_string(), decision),
        ("routing_log", log_route, log),
    ] {
        hold(&planted, name, &format!("GET {route}"), body);
    }

    // Last, since it steps the clock: the refusal of a replay whose bound passed while the writer held a batch on the
    // capped file, then the writer freed.
    hold(
        &planted,
        "replay_held",
        "POST /admin/ledger/parked/replay",
        replay_held(&parts, &cookie).await,
    );
}

/// The `503` body of a replay asked for while the writer holds a batch past the route's bound. The batch is a row too
/// wide for the capped file, and the clock steps past the bound while the replay waits, then a hold at a time once the
/// disk is freed, until the batch lands.
async fn replay_held(parts: &common::Full, cookie: &str) -> Value {
    common::hold_the_writer(
        parts,
        request(0, "held", common::NOW, "desktop", "s1", 200, "succeeded"),
        vec![],
    )
    .await;
    let asked = tokio::spawn({
        let (app, cookie) = (parts.app.clone(), cookie.to_string());
        async move { post(&app, "/admin/ledger/parked/replay", "", Some(&cookie)).await }
    });
    stays_for(Duration::from_millis(400), || !asked.is_finished()).await;
    common::step_past_replay_bound(parts);
    let refused = tokio::time::timeout(Duration::from_secs(5), asked)
        .await
        .expect("the route answers once its bound passed")
        .unwrap();
    assert_eq!(refused.status(), 503);
    let body = json(refused).await;
    common::free_the_writer(parts).await;
    body
}

/// A selection of the desktop key on `claude-sonnet-5` at the capture's instant, as the relay asks for its first
/// wire attempt: no prefix chains and no holds, each story setting what it reads, under the story's session or one of
/// the request's own.
fn asked(
    parts: &common::Full,
    key_generation: i64,
    request_id: &str,
    seq: u32,
    session: Option<&str>,
) -> phoebis_routing::SelectRequest {
    let snapshot = parts.router.snapshot();
    let key_row = snapshot
        .key("desktop")
        .expect("the desktop key is in the snapshot")
        .clone();
    phoebis_routing::SelectRequest {
        request_id: request_id.into(),
        attempt: 1,
        seq,
        key_id: "desktop".into(),
        key_generation,
        agent_id: "main".into(),
        session_id: session.map_or_else(|| format!("s-{request_id}"), String::from),
        canonical_model: "claude-sonnet-5".into(),
        folded_model: "claude-sonnet-5".into(),
        candidates: phoebis_routing::resolve(
            &snapshot,
            &key_row,
            "claude-sonnet-5",
            "claude-sonnet-5",
        )
        .expect("the pool's model rules allow claude-sonnet-5"),
        portable: true,
        credit_account: None,
        attempted: Vec::new(),
        now: common::NOW,
        received_at: common::NOW,
        fallback_ms: 60_000,
        revision: snapshot.revision,
        probe: false,
        count: false,
        request_class: None,
        context_compacted: false,
        static_hash: None,
        boundaries: Arc::from([]),
        client: ClientClass::ClaudeCode,
        entrypoint: None,
        static_boundaries: Arc::from([]),
        marker: false,
        parent_agent_id: None,
        failed_over: None,
        holds: Vec::new(),
        dialed: false,
        held_out: false,
        lane: lane_key("claude-sonnet-5", "standard", "none", "-"),
        fast: false,
        classifier: false,
        body_tokens: 0,
        effort: None,
    }
}

/// The answer the request `request_id` would have reported: its static chain and one message boundary written on
/// `business`, the cache map its decision `seq` then holds.
fn warmed(
    parts: &common::Full,
    key_generation: i64,
    request_id: &str,
    root: &str,
    static_hash: &str,
) -> phoebis_routing::Report {
    let now = common::NOW;
    phoebis_routing::Report::Affinity(phoebis_routing::AffinityReport {
        account_id: "business".into(),
        incarnation: parts
            .router
            .snapshot()
            .account("business")
            .expect("the business account is in the snapshot")
            .incarnation,
        key_id: "desktop".into(),
        key_generation,
        canonical_model: "claude-sonnet-5".into(),
        lane: lane_key("claude-sonnet-5", "standard", "none", "-"),
        root: root.into(),
        static_boundaries: Arc::from([static_hash.to_string()]),
        message_boundaries: Arc::from([format!("m-{request_id}")]),
        static_sizes: vec![31_200],
        static_ttl_ms: Some(3_600_000),
        message_ttl_ms: Some(300_000),
        cache_touched: true,
        prefix_tokens: 31_200,
        now,
        cache_write_5m_tokens: None,
        cache_write_1h_tokens: None,
        uniform_ttl_ms: None,
        request_id: request_id.into(),
        seq: 1,
        attempt: 1,
        message_id: None,
    })
}

/// One decision of each kind through the harness's router, at the capture's instant, and one event of each kind the
/// ledger writes inside the hour before it: the router's own where a report or an admin action provokes it, the rest
/// recorded as their writers build them.
async fn routing_stories(parts: &common::Full, cookie: &str, key_generation: i64) {
    use phoebis_routing::{Report, RouterEvent, SelectRequest, SelectResult};
    let app = &parts.app;
    let now = common::NOW;
    let hour_ago = now - 3_600_000;
    let account_of = |answer: &phoebis_routing::SelectAnswer| match &answer.result {
        SelectResult::Selected(selection) => selection.account.id.clone(),
        other => panic!("a selection, not {other:?}"),
    };
    let patched = |body: &'static str| async move {
        let response = patch(app, "/admin/pools/main", body, Some(cookie)).await;
        assert_eq!(response.status(), 200, "{body}");
    };
    // The stories run on the pool's two serving members, so the usage-based seat the earlier captures read leaves
    // the pool first.
    let response = common::put(
        app,
        "/admin/pools/main/members",
        r#"{"members":[{"account_id":"business","priority":0,"budget_monthly_usd":30.0},{"account_id":"direct","priority":1,"budget_monthly_usd":50.0}]}"#,
        Some(cookie),
    )
    .await;
    assert_eq!(response.status(), 200, "the seat leaves the pool");

    // Placed by the static prefix: a session warms the shared static chain on business, and a fresh session lands
    // there by it. The pool places a main conversation by its warm prefix for this.
    patched(r#"{"session_placement":"by_warm_prefix"}"#).await;
    let static_hash = "9c1f6b0e4d2a8f73b5e1c0d9a7f3e2b14c6d8e0f2a4b6c8d0e1f3a5b7c9d1e3f";
    let warm = parts
        .router
        .select(SelectRequest {
            attempted: vec!["direct".into()],
            ..asked(parts, key_generation, "r-warm", 1, Some("s-warm"))
        })
        .await
        .unwrap();
    assert_eq!(account_of(&warm), "business");
    drop(warm);
    parts
        .router
        .report(warmed(
            parts,
            key_generation,
            "r-warm",
            "s-warm",
            static_hash,
        ))
        .await;
    let by_prefix = parts
        .router
        .select(SelectRequest {
            static_boundaries: Arc::from([static_hash.to_string()]),
            portable: false,
            marker: true,
            ..asked(parts, key_generation, "r-static", 1, Some("s-static"))
        })
        .await
        .unwrap();
    match &by_prefix.result {
        SelectResult::Selected(selection) => assert_eq!(
            (
                selection.account.id.as_str(),
                selection.trace.placement.as_ref().map(|p| p.rung)
            ),
            ("business", Some("static"))
        ),
        other => panic!("the warm static prefix places the request, not {other:?}"),
    }
    drop(by_prefix);
    parts
        .router
        .report(warmed(
            parts,
            key_generation,
            "r-static",
            "s-static",
            static_hash,
        ))
        .await;
    parts.ledger.record(Record::Request {
        request: RequestRecord {
            model: "claude-sonnet-5".into(),
            canonical_model: "claude-sonnet-5".into(),
            ..request(
                key_generation,
                "r-static",
                now - 1_000,
                "desktop",
                "s-static",
                200,
                "succeeded",
            )
        },
        attempts: vec![attempt(
            "r-static",
            1,
            "business",
            200,
            "succeeded",
            now - 1_000,
        )],
        bodies: None,
    });

    // Renewed, then held: with a cap of one the renewal's slot stays taken while a fresh session asks for the same
    // account, and the placement after the wait takes it once freed.
    patched(r#"{"in_flight_cap":1}"#).await;
    let busy = parts
        .router
        .select(asked(parts, key_generation, "r-renew", 1, Some("s-warm")))
        .await
        .unwrap();
    assert_eq!(account_of(&busy), "business");
    let held = parts
        .router
        .select(SelectRequest {
            attempted: vec!["direct".into()],
            ..asked(parts, key_generation, "r-held", 1, Some("s-held"))
        })
        .await
        .unwrap();
    let hold = match held.result {
        SelectResult::Held { hold, .. } => hold,
        other => panic!("the account at its cap holds the request, not {other:?}"),
    };

    // Held, then exhausted: with both members at their cap no slot frees within the pool's 60-second bound, so the
    // relay marks the wait run out, passes the other member over once the request's one bound has passed, and the
    // selection after finds both attempted with no leg dialed, answered as a pool at its cap. Its id sorts below the
    // held and exhausted stories', whose rows the console's decoder tests read first.
    let direct_busy = parts.router.in_flight().dispatched("direct", now);
    let capped = parts
        .router
        .select(asked(parts, key_generation, "r-capped", 1, None))
        .await
        .unwrap();
    let wait = match capped.result {
        SelectResult::Held { hold, .. } => phoebis_routing::HoldFacts {
            held_ms: 60_000,
            ran_out: true,
            ..hold
        },
        other => panic!("a pool with every member at its cap holds the request, not {other:?}"),
    };
    let passed = parts
        .router
        .select(SelectRequest {
            attempted: vec![wait.account_id.clone()],
            holds: vec![wait.clone()],
            ..asked(parts, key_generation, "r-capped", 2, None)
        })
        .await
        .unwrap();
    let passed_over = match passed.result {
        SelectResult::Held { account_id, .. } => account_id,
        other => panic!("the other member at its cap holds the request too, not {other:?}"),
    };
    assert_ne!(
        passed_over, wait.account_id,
        "the second hold is on the other member"
    );
    let exhausted = parts
        .router
        .select(SelectRequest {
            attempted: vec![wait.account_id.clone(), passed_over],
            holds: vec![wait],
            held_out: true,
            ..asked(parts, key_generation, "r-capped", 3, None)
        })
        .await
        .unwrap();
    assert_eq!(
        exhausted.result,
        SelectResult::Exhausted(phoebis_routing::Exhaustion::Untimed),
        "with both members attempted the run-out exhausts the pool untimed"
    );
    drop(direct_busy);
    drop(busy);
    let after_hold = parts
        .router
        .select(SelectRequest {
            attempted: vec!["direct".into()],
            holds: vec![phoebis_routing::HoldFacts {
                held_ms: 800,
                ..hold
            }],
            ..asked(parts, key_generation, "r-held", 2, Some("s-held"))
        })
        .await
        .unwrap();
    assert_eq!(account_of(&after_hold), "business");
    drop(after_hold);
    patched(r#"{"in_flight_cap":16}"#).await;

    // A keepalive probe renews its pin.
    let probe = parts
        .router
        .select(SelectRequest {
            probe: true,
            ..asked(parts, key_generation, "r-probe", 1, Some("s-warm"))
        })
        .await
        .unwrap();
    drop(probe);

    // Moved: the pinned account's five-hour window is spent, so its session leaves for the sibling.
    parts
        .router
        .report(Report::Sample {
            account_id: "business".into(),
            sample: phoebis_routing::UtilizationSample {
                observed_at: now,
                ..Default::default()
            }
            .with(
                phoebis_routing::FIVE_HOUR,
                phoebis_routing::WindowReading {
                    utilization: 1.0,
                    reset: Some(now + 3_600_000),
                    observed_at: now,
                    source: phoebis_routing::WindowSource::Head,
                    plan: None,
                    tier: None,
                },
            ),
            incarnation: 1,
            source_request_id: Some("r-probe".into()),
        })
        .await;
    let moved = parts
        .router
        .select(asked(parts, key_generation, "r-move", 1, Some("s-warm")))
        .await
        .unwrap();
    assert_eq!(account_of(&moved), "direct");
    drop(moved);

    // Overage entered and left, and a credits reading that stops fast mode. A pending reading of an account is
    // replaced by a newer one, so each edge's row is waited for before the reading that ends it is sent.
    for (in_use, at, kind) in [
        (true, now - 60_000, "overage_started"),
        (false, now, "overage_ended"),
    ] {
        parts
            .router
            .report(Report::Overage {
                account_id: "business".into(),
                evidence: phoebis_routing::OverageEvidence {
                    in_use,
                    since: in_use.then_some(at),
                    reset: Some(now + 3_600_000),
                    observed_at: at,
                    source: phoebis_routing::EvidenceSource::Head,
                    incarnation: 1,
                },
                incarnation: 1,
            })
            .await;
        fake_upstream::wait_for(std::time::Duration::from_secs(5), || {
            let rows = parts
                .db
                .read_blocking(|c| {
                    Ok(
                        c.query_row("SELECT COUNT(*) FROM events WHERE kind = ?1", [kind], |r| {
                            r.get::<_, i64>(0)
                        })?,
                    )
                })
                .unwrap();
            (rows > 0).then_some(())
        })
        .await;
    }
    parts
        .router
        .report(Report::Credits {
            account_id: "business".into(),
            state: phoebis_routing::CreditsState {
                enabled: false,
                disabled_reason: Some("out_of_credits".into()),
                utilization: Some(0.25),
                observed_at: now,
                incarnation: 1,
            },
            incarnation: 1,
        })
        .await;

    // An operator clears a cooldown and releases a session.
    let cleared = common::delete(
        app,
        "/admin/accounts/business/cooldowns/model:claude-haiku-4-5-20251001",
        Some(cookie),
    )
    .await;
    assert_eq!(cleared.status(), 204);
    let released = post(
        app,
        "/admin/sessions/desktop/s-static/release",
        "",
        Some(cookie),
    )
    .await;
    assert_eq!(released.status(), 204);

    // Exhausted: every member cooling.
    for account_id in ["business", "direct"] {
        parts
            .router
            .report(Report::Cooldown {
                account_id: account_id.into(),
                row: phoebis_store::CooldownRow {
                    account_id: account_id.into(),
                    scope: "account".into(),
                    until: now + 600_000,
                    set_at: now,
                    reason: "rate_limit".into(),
                    window: "five_hour".into(),
                    source_request_id: Some("r-move".into()),
                    incarnation: 1,
                },
                pin: None,
                lifecycle_generation: 1,
                credential_generation: 1,
                incarnation: 1,
            })
            .await;
    }
    let exhausted = parts
        .router
        .select(asked(parts, key_generation, "r-exhausted", 1, None))
        .await
        .unwrap();
    assert!(
        matches!(exhausted.result, SelectResult::Exhausted(_)),
        "every member cooling exhausts the pool, not {:?}",
        exhausted.result
    );

    // Refused: a key revoked between its admission and its selection, which the router re-reads only for a request
    // admitted under an older registry.
    let refused = parts
        .router
        .select(SelectRequest {
            key_id: "retired".into(),
            revision: 0,
            ..asked(parts, key_generation, "r-refused", 1, None)
        })
        .await
        .unwrap();
    assert!(matches!(refused.result, SelectResult::Refused(_)));

    // The `r-blocked` request row sits 0.6 s before the captured hour, and a refusal's `received_at` is its `at`, so
    // the block's record takes the hour's first instant to land in the log.
    parts.router.record_refusal(
        phoebis_routing::RefusedRequest {
            request_id: "r-blocked".into(),
            attempt: 1,
            seq: 1,
            at: hour_ago,
            key_id: "desktop".into(),
            key_generation,
            agent_id: "main".into(),
            parent_agent_id: None,
            session_id: None,
            request_class: Some("auxiliary".into()),
            canonical_model: "claude-sonnet-5-5".into(),
            lane: Default::default(),
            flags: Default::default(),
            chain: Default::default(),
            attempted: Vec::new(),
            holds: Vec::new(),
            client: ClientClass::ClaudeCode,
            client_version: Some("2.1.310".into()),
        },
        phoebis_routing::RefusalFacts {
            kind: "guardrail",
            code: "guardrail_blocked",
            reason: "Large auxiliary requests".into(),
        },
    );

    // The kinds no report or route of this harness writes, each as its writer builds it: the harness's router prices
    // no burn, so it marks no pin, books no reservation and slows no account, and the round, its mark leaving with the
    // pin's move, a booking its request ended without usage and a slow-down's span are seeded here.
    let at = |minute: i64| hour_ago + minute * 60_000;
    let routed = [
        RouterEvent::SheddingRound {
            account_id: "business".into(),
            pool_id: "main".into(),
            request_id: "r-failover".into(),
            key_id: "desktop".into(),
            session_id: "s1".into(),
            agent_id: "main".into(),
            model: "claude-sonnet-5".into(),
            round: Box::new(phoebis_routing::ShedRoundFacts {
                window: "five_hour".into(),
                utilization: 0.71,
                projection: 0.99,
                horizon: at(2) + 2_400_000,
                ceiling: 0.95,
                trigger: "projection",
                stretch_ms: None,
                threshold_ms: None,
                factor: None,
                unit: "window_share",
                rewrite_unit: "pro_week",
                burn_average_seconds: 1_800,
                shed_cap: 0.2,
                burn_sum: 0.12,
                excess: 0.06,
                target: 0.024,
                removed: 0.07,
                warm_sessions: "when_needed",
                pins: vec![phoebis_routing::ShedPinFacts {
                    key_id: "desktop".into(),
                    session_id: "s0".into(),
                    agent_id: "main".into(),
                    model: "claude-sonnet-5".into(),
                    cold: true,
                    burn: Some(0.07),
                    last_input_tokens: 48_100,
                    per_rewrite: Some(0.004),
                    marked: true,
                    rewrite: Some(0.005),
                    rewrite_window: Some("seven_day"),
                    planned: Some("direct".into()),
                    runway_ms: None,
                    skip: None,
                    note: None,
                }],
                marks: vec![phoebis_routing::PinKeyFacts {
                    key_id: "desktop".into(),
                    session_id: "s0".into(),
                    agent_id: "main".into(),
                    model: "claude-sonnet-5".into(),
                }],
                outcome: "marked 1".into(),
                reason: None,
                dwell_seconds: 900,
                dwell_until: at(2) + 900_000,
            }),
            at: at(2),
        },
        RouterEvent::MarkCleared {
            account_id: "business".into(),
            key_id: "desktop".into(),
            session_id: "s0".into(),
            agent_id: "main".into(),
            model: "claude-sonnet-5".into(),
            reason: "moved",
            marked_at: at(2),
            marked_on: "business".into(),
            marked_window: "five_hour".into(),
            projection: None,
            stretch_ms: None,
            threshold_ms: None,
            at: at(7),
        },
        RouterEvent::ReservationExpired {
            account_id: "business".into(),
            request_id: "r-failover".into(),
            attempt: 1,
            cause: "canceled",
            shares: std::collections::BTreeMap::from([
                ("five_hour".to_string(), 0.004),
                ("seven_day".to_string(), 0.0007),
            ]),
            age_ms: 42_000,
            at: at(18),
        },
        RouterEvent::WarmPrefixExpired {
            account_id: "direct".into(),
            key_id: "desktop".into(),
            root: "s0".into(),
            model: "claude-sonnet-5".into(),
            lane: lane_key("claude-sonnet-5", "standard", "none", "-"),
            entries: 3,
            deepest: phoebis_routing::CacheMapEntry {
                tier: "message",
                depth: 3,
                prefix_tokens: 52_000,
                ttl_ms: 300_000,
                account_id: "direct".into(),
            },
            cause: "ttl",
            at: at(3),
        },
        RouterEvent::WindDownStarted {
            account_id: "business".into(),
            pool_id: "main".into(),
            ends_at: now + 86_400_000,
            wind_down_minutes: 1_440,
            end_source: "canceled",
            restored: false,
            at: at(4),
        },
        // The 5-hour window the probe's head spent, read back under its limit under the same reset.
        RouterEvent::WindowCleared {
            account_id: "business".into(),
            window: "five_hour".into(),
            utilization_before: 1.0,
            utilization: 0.0,
            reset: Some(now + 3_600_000),
            source: phoebis_routing::WindowSource::Poll,
            source_request_id: None,
            at: now,
        },
        RouterEvent::WindowReset {
            account_id: "business".into(),
            window: "seven_day".into(),
            utilization_before: 0.83,
            reset_at: at(5),
            next_reset: Some(at(5) + 7 * 86_400_000),
            observed: "sample",
            restored: false,
            at: at(5),
            plan: Some("max_20x".into()),
            tier: Some("default_claude_max_20x".into()),
        },
        RouterEvent::SlowDownStarted {
            account_id: "business".into(),
            pool_id: "main".into(),
            factor: 0.5,
            stretch_ms: 2_520_000,
            threshold_ms: 1_800_000,
            demand: 0.12,
            reset: Some(at(12) + 5_040_000),
            at: at(12),
        },
        RouterEvent::SlowDownEnded {
            account_id: "business".into(),
            pool_id: "main".into(),
            since: at(12),
            duration_ms: 1_080_000,
            factor_min: 0.35,
            held_ms: 240_000,
            at: at(32),
        },
    ];
    for event in &routed {
        parts.ledger.record(Record::Event(
            phoebis_ledger::event_of(event).expect("the router's event has a row"),
        ));
    }
    parts.ledger.record(phoebis_ledger::poll_backoff(
        &phoebis_oauth::BackoffEdge::Started {
            account_id: "business".into(),
            at: at(6),
            endpoint: "usage",
            reason: "429".into(),
            attempts: 1,
            next_attempt_at: at(6) + 60_000,
            last_clean_at: Some(at(5)),
        },
    ));
    parts.ledger.record(phoebis_ledger::poll_backoff(
        &phoebis_oauth::BackoffEdge::Ended {
            account_id: "business".into(),
            at: at(8),
            since: at(6),
            attempts: 2,
            resumed_at: at(8),
            cause: phoebis_oauth::BackoffEndCause::CleanPoll,
        },
    ));
    // The plan-ended pause closes a run at the flag's instant, so this close shares the `plan_ended` row's minute.
    parts.ledger.record(phoebis_ledger::poll_backoff(
        &phoebis_oauth::BackoffEdge::Ended {
            account_id: "business".into(),
            at: at(2),
            since: at(1),
            attempts: 1,
            resumed_at: at(2),
            cause: phoebis_oauth::BackoffEndCause::PlanEnded,
        },
    ));
    parts.ledger.record(phoebis_ledger::poll_failed(
        "business",
        at(4),
        &phoebis_oauth::PollFault {
            endpoint: "usage",
            status: Some(503),
            transport: false,
            text: "usage request rejected with status 503".into(),
        },
    ));
    let profile = |organization_type: &str, rate_limit_tier: &str, subscription_status: &str| {
        j!({ "organization": {
            "organization_type": organization_type,
            "rate_limit_tier": rate_limit_tier,
            "subscription_status": subscription_status,
        }})
    };
    parts.ledger.record(phoebis_ledger::plan_ended(
        "business",
        at(2),
        "usage_403",
        &profile("claude_free", "default_claude_ai", "canceled"),
    ));
    parts.ledger.record(phoebis_ledger::plan_restored(
        "business",
        at(3),
        "profile",
        &profile("claude_max", "default_claude_max_5x", "active"),
    ));
    parts.ledger.record(phoebis_ledger::catalog_changed(
        &phoebis_relay::CatalogChange {
            account_id: "business".into(),
            added: vec!["claude-sonnet-5-5".into()],
            removed: vec!["claude-sonnet-4-5".into()],
            count: 12,
            at: at(9),
        },
    ));
    let written = |minute: i64, kind: &str| EventRecord {
        at: at(minute),
        kind: kind.into(),
        account_id: Some("business".into()),
        ..Default::default()
    };
    for event in [
        EventRecord {
            pool_id: Some("main".into()),
            request_id: Some("r-failover".into()),
            detail: Some(
                j!({ "period": "month", "budget_usd": 30.0, "spent_usd": 30.4, "period_end": now + 86_400_000, "source_request_id": "r-failover" })
                    .to_string(),
            ),
            ..written(10, "budget_reached")
        },
        EventRecord {
            pool_id: Some("main".into()),
            detail: Some(
                j!({ "period": "day", "previous_spent_usd": 4.2, "period_start": at(11) }).to_string(),
            ),
            ..written(11, "budget_period_ended")
        },
        EventRecord {
            account_id: None,
            request_id: Some("r-gap".into()),
            detail: Some(j!({ "request_id": "r-gap", "reason": "chunk_skipped" }).to_string()),
            ..written(12, "ledger_gap")
        },
        EventRecord {
            detail: Some(
                j!({ "from": "ready", "to": "reauth_required", "cause": "refresh_failed", "detail": null }).to_string(),
            ),
            ..written(13, "account_state_changed")
        },
        EventRecord {
            detail: Some(
                j!({ "until": now + 3_600_000, "access_expires_at": now + 3_600_000, "refresh_expires_at": null })
                    .to_string(),
            ),
            ..written(14, "refresh_succeeded")
        },
        EventRecord {
            detail: Some(
                j!({ "cause": "invalid_grant", "error": "refresh rejected with status 400 (invalid_grant)" })
                    .to_string(),
            ),
            ..written(15, "refresh_failed")
        },
        EventRecord {
            detail: Some(
                j!({ "cause": "refresh_response_lost", "error": "refresh response lost: the token endpoint did not answer" })
                    .to_string(),
            ),
            ..written(16, "refresh_uncertain")
        },
        EventRecord {
            account_id: None,
            actor: Some("admin".into()),
            pool_id: Some("main".into()),
            detail: Some(
                j!({ "kind": "pool_updated", "target": "main", "diff": {
                    "in_flight_cap": { "from": 16, "to": 1 },
                    "keepalive.max_idle_seconds": { "from": 3600, "to": 86400 },
                } })
                .to_string(),
            ),
            ..written(17, "registry_changed")
        },
    ] {
        parts.ledger.record(Record::Event(event));
    }

    // The start row lands last, a minute past the capture's instant on a clock stepped by the same minute, so the
    // writer dates its `down_since` from the newest row seeded before it. A story added later lands above this seed.
    common::step(parts, Duration::from_secs(60));
    parts.ledger.record(Record::RouterStarted {
        version: "0.0.0-fixture".into(),
        counts: phoebis_routing::LoadCounts {
            pins: 3,
            windows: 2,
            affinity_entries: 5,
            shed_marks: 2,
            shed_rounds_stale: 1,
        },
        at: now + 60_000,
    });
}
