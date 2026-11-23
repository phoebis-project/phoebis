//! The companion's allowance route: the key's pool as the router reads it, each account under its pseudonym, and the
//! session's pins, behind the key's `allowance` permission.
mod common;
use axum::body::Body;
use axum::http::Request;
use common::*;
use phoebis_keepalive::{companion_router, KeepalivePolicy};
use phoebis_ledger::{AttemptRecord, PriceRevisions, Record, RequestRecord};
use phoebis_routing::{
    Report, RouterConfig, SharePricer, UtilizationSample, WindowReading, WindowSource, FIVE_HOUR,
    HOUR_MS, SEVEN_DAY, SEVEN_DAY_FABLE,
};
use phoebis_store::{
    AccountKind, AccountState, AccountUpdate, AffinityMode, ApiKeyRow, Cancellation,
    CompanionPermissions, CooldownRow, CooldownScope, Identity, KeyFeatureAccounts, MemberWrite,
    RulesMode, StateCause,
};
use serde_json::{json, Value};
use std::collections::BTreeSet;

const ALLOWANCE_PATH: &str = "/v0/companion/allowance";
const C: &str = "sk-ant-api03-CCCC";
const D: &str = "sk-ant-api03-DDDD";
const OTHER_KEY: &str = "phoebis-otherkey";
/// A table of the suite's own, so a planted spend prices from rates this file states: input at $4 a million tokens.
const PRICES: &str = r#"
revisions:
  - effective: 2026-01-01
    models:
      claude-sonnet-4-5: {input: 4, cache_write_5m: 5, cache_write_1h: 8, cache_read: 0.4, output: 20}
"#;
const SESSION: &str = "session";
/// A storage key under which `b`'s pseudonym sorts before `a`'s, against the account-id order the routing task answers
/// in.
const STORAGE_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const TIER: &str = "default_claude_max_20x";

struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: Value,
    text: String,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// The `id` of every listed account, in the answer's order.
    fn account_ids(&self) -> Vec<String> {
        self.body["accounts"]
            .as_array()
            .unwrap_or_else(|| panic!("no account list: {}", self.text))
            .iter()
            .map(|account| account["id"].as_str().unwrap().to_string())
            .collect()
    }
}

/// The session id goes into the query as written, so a case can send one the client would have to encode.
async fn get(app: &axum::Router, session_id: Option<&str>, key: Option<&str>) -> Answer {
    let uri = match session_id {
        Some(id) => format!("{ALLOWANCE_PATH}?session_id={id}"),
        None => ALLOWANCE_PATH.to_string(),
    };
    let mut req = Request::builder().method("GET").uri(uri);
    if let Some(key) = key {
        req = req.header("authorization", format!("Bearer {key}"));
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let headers = resp
        .headers()
        .iter()
        .map(|(n, v)| (n.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    Answer {
        status,
        headers,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        text: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

/// A second key `laptop` in the rig's pool under `OTHER_KEY`, holding `permissions`.
fn laptop_key(r: &Rig, permissions: CompanionPermissions) -> ApiKeyRow {
    ApiKeyRow {
        id: "laptop".into(),
        label: "laptop".into(),
        notes: None,
        pool_id: "main".into(),
        enabled: true,
        revoked_at: None,
        expires_at: None,
        model_rules: Vec::new(),
        model_rules_mode: RulesMode::Extend,
        long_context_twins: true,
        companion_permissions: permissions,
        body_capture_hours: None,
        secret_digest: r.h.store.api_key_digest(OTHER_KEY).to_vec(),
        prefix: phoebis_store::key_prefix(OTHER_KEY),
        incarnation: 1,
        created_at: NOW,
        updated_at: 0,
        rotated_at: None,
        last_used_at: None,
        retired_secret_last_used_at: None,
        auto_mode_server: None,
        feature_accounts: KeyFeatureAccounts::default(),
    }
}

/// One real request of the main agent on `session` under the key `secret`, the rig's own request otherwise, pumped
/// into the engine.
async fn observe_as(r: &mut Rig, secret: &str, session: &str) {
    let mut req = relay_common::request_from_fixture(
        phoebis_profile::fixtures::inference_head(phoebis_profile::fixtures::FixtureKind::ApiKey),
        BODY.as_bytes(),
        secret,
    );
    relay_common::set_header(&mut req, "x-claude-code-session-id", session);
    relay_common::set_header(&mut req, "x-claude-code-agent-id", "main");
    relay_common::set_header(&mut req, "anthropic-beta", "test-beta");
    let resp = r.h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let id = relay_common::outcome(&resp).request_id;
    let _ = resp.into_body().collect().await.unwrap();
    r.pump(Some(&id)).await;
}

/// One real request of `agent` on `session` under the rig's key, its user agent naming `entrypoint`, pumped into the
/// engine.
async fn observe_on(r: &mut Rig, session: &str, agent: &str, entrypoint: &str) {
    let mut req = r.request_with(agent, BODY);
    relay_common::set_header(&mut req, "x-claude-code-session-id", session);
    relay_common::set_header(
        &mut req,
        "user-agent",
        &format!("claude-cli/2.1.310 (external, {entrypoint})"),
    );
    let resp = r.h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let id = relay_common::outcome(&resp).request_id;
    let _ = resp.into_body().collect().await.unwrap();
    r.pump(Some(&id)).await;
}

/// A usage-poll reading of `window` on `account`, held by the router task once this returns.
async fn read_window(r: &Rig, account: &str, window: &str, utilization: f64, reset: i64) {
    r.h.router
        .report(Report::Sample {
            account_id: account.into(),
            sample: UtilizationSample::default().with(
                window,
                WindowReading {
                    utilization,
                    reset: Some(reset),
                    observed_at: r.now(),
                    source: WindowSource::Poll,
                    plan: None,
                    tier: None,
                },
            ),
            incarnation: 1,
            source_request_id: None,
        })
        .await;
    r.h.router.samples().await.unwrap();
}

/// Cancels `account` with its end at `ends_at` through the store write the admin route's account edit commits. The
/// write runs none of the route's checks, so a caller passes an OAuth account, the one kind the route cancels.
fn cancel(r: &Rig, account: &str, ends_at: i64) {
    let row = r.h.store.account(account).unwrap().unwrap();
    r.h.store
        .commit_account_update(&AccountUpdate {
            id: account,
            expected_revision: row.revision,
            incarnation: row.incarnation,
            lifecycle_generation: row.lifecycle_generation,
            label: &row.label,
            secret: None,
            state: None,
            at: r.now(),
            actor: "test",
            enabled: None,
            cancellation: Some(Cancellation::On {
                ends_at: Some(ends_at),
            }),
        })
        .unwrap();
}

/// Writes the pool's affinity TTL and mode through the pool write the admin route commits, then reloads the router.
async fn set_affinity(r: &Rig, ttl_seconds: u64, mode: AffinityMode) {
    let mut pool = r.h.store.pool("main").unwrap().unwrap();
    pool.affinity_ttl_seconds = ttl_seconds;
    pool.affinity.mode = mode;
    r.h.store
        .update_pool_with_members_and_guardrails(&pool, pool.updated_at, None, None, "test")
        .unwrap();
    r.h.router.reload().await.unwrap();
}

/// One line per value, depth first in key order: the path and the JSON kind (`null`, `boolean`, `integer`, `float`,
/// `string`, `array`, `object`). Every array entry is walked, since the API-key account and the plan account differ in
/// shape. Values are not compared: a runway or a projection is the routing task's to change.
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
                    walk(v, &format!("{path}.{k}"), out);
                }
            }
            Value::Array(a) => {
                for (i, v) in a.iter().enumerate() {
                    walk(v, &format!("{path}[{i}]"), out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(v, "$", &mut out);
    out
}

#[tokio::test]
async fn a_key_without_the_allowance_permission_gets_403_and_no_figures() {
    let r = rig(RigSpec::default()).await;
    let app = companion_router(r.engine.clone());
    let without = CompanionPermissions {
        allowance: false,
        keepalive: true,
    };
    r.h.store
        .insert_api_key(&laptop_key(&r, without), "test")
        .unwrap();
    r.h.router.reload().await.unwrap();
    let refused = get(&app, Some(SESSION), Some(OTHER_KEY)).await;
    assert_eq!(
        (refused.status, refused.body.clone()),
        (403, json!({"code": "companion_not_permitted"})),
        "{}",
        refused.text
    );
    assert_eq!(refused.header("cache-control"), Some("no-store"));
    let permitted = get(&app, Some(SESSION), Some(KEY)).await;
    assert_eq!(permitted.status, 200, "{}", permitted.text);
}

#[tokio::test]
async fn a_request_without_a_key_answers_the_relays_401() {
    let r = rig(RigSpec::default()).await;
    let app = companion_router(r.engine.clone());
    let refused = get(&app, Some(SESSION), None).await;
    assert_eq!(
        (refused.status, refused.header("x-phoebis-error-code")),
        (401, Some("auth_missing")),
        "{}",
        refused.text
    );
}

#[tokio::test]
async fn the_body_names_no_label_email_or_id_of_the_registry() {
    let mut r = rig(RigSpec {
        accounts: vec![oauth("a", "sk-ant-oat01-AAAA", 0), api_key("b", B, 1)],
        tier: Some(TIER),
        ..RigSpec::default()
    })
    .await;
    let app = companion_router(r.engine.clone());
    r.h.store
        .update_account_config("a", AccountKind::Oauth, "Alice Example")
        .unwrap();
    r.h.store
        .update_account_config("b", AccountKind::ApiKey, "Alice Example")
        .unwrap();
    let identity = r.h.store.account("a").unwrap().unwrap().identity.unwrap();
    r.h.store
        .set_identity(
            "a",
            &Identity {
                email: "alice@example.com".into(),
                ..identity
            },
        )
        .unwrap();
    r.h.router.reload().await.unwrap();
    read_window(&r, "a", FIVE_HOUR, 0.25, NOW + 2 * HOUR_MS).await;
    r.observe("main").await;
    let answer = get(&app, Some(SESSION), Some(KEY)).await;
    assert_eq!(
        (
            answer.status,
            answer.account_ids().len(),
            answer.body["pins"].as_array().map(Vec::len)
        ),
        (200, 2, Some(1)),
        "both accounts and the pin are listed: {}",
        answer.text
    );
    for named in ["\"a\"", "\"b\"", "Alice", "alice@", "desktop"] {
        assert!(
            !answer.text.contains(named),
            "the answer names {named}: {}",
            answer.text
        );
    }
}

#[tokio::test]
async fn a_disabled_or_unready_account_is_absent_and_a_cooled_one_present_with_its_obstacle() {
    let r = rig(RigSpec {
        accounts: vec![
            api_key("a", A, 0),
            api_key("b", B, 1),
            api_key("c", C, 2),
            api_key("d", D, 3),
        ],
        ..RigSpec::default()
    })
    .await;
    let app = companion_router(r.engine.clone());
    r.h.store.set_enabled("b", false, "test").unwrap();
    r.h.store
        .set_state("c", AccountState::ReauthRequired, StateCause::Operator)
        .unwrap();
    // A funding mark with no retry entry yet is one the selector admits at once, so the selector holds no obstacle
    // against it and the row's state alone keeps it out.
    r.h.store
        .set_state(
            "d",
            AccountState::FundingRequired,
            StateCause::FundingRefused,
        )
        .unwrap();
    r.h.router.reload().await.unwrap();
    r.h.router
        .report(Report::Cooldown {
            account_id: "a".into(),
            row: CooldownRow {
                account_id: "a".into(),
                scope: CooldownScope::Account,
                until: NOW + 10 * MINUTE,
                set_at: r.now(),
                reason: "rate_limit".into(),
                window: "five_hour".into(),
                source_request_id: None,
                incarnation: 1,
            },
            pin: None,
            lifecycle_generation: 1,
            credential_generation: 1,
            incarnation: 1,
        })
        .await;
    settled(&r, |r| async move {
        r.h.router
            .cooldowns()
            .await
            .unwrap()
            .iter()
            .any(|row| row.account_id == "a")
    })
    .await;
    let answer = get(&app, Some(SESSION), Some(KEY)).await;
    let listed: Vec<(&str, &str)> = answer.body["accounts"]
        .as_array()
        .unwrap_or_else(|| panic!("no account list: {}", answer.text))
        .iter()
        .map(|account| {
            (
                account["id"].as_str().unwrap(),
                account["obstacle"].as_str().unwrap_or("null"),
            )
        })
        .collect();
    let a = r.h.store.companion_key().pseudonym("a");
    assert_eq!(listed, [(a.as_str(), "cooldown")], "{}", answer.text);
}

#[tokio::test]
async fn each_account_reads_as_eight_hex_characters_and_the_list_runs_in_pseudonym_order() {
    let r = rig(RigSpec {
        storage_key: storage_key(STORAGE_KEY),
        ..RigSpec::default()
    })
    .await;
    let app = companion_router(r.engine.clone());
    let listed = get(&app, Some(SESSION), Some(KEY)).await.account_ids();
    let key = r.h.store.companion_key();
    let (a, b) = (key.pseudonym("a"), key.pseudonym("b"));
    for id in [&a, &b] {
        assert!(
            id.len() == 8 && id.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')),
            "{id} is not eight lowercase hex characters"
        );
    }
    assert_eq!(
        listed,
        [b, a],
        "each account under the pseudonym the store's key gives it, in pseudonym order"
    );
}

#[tokio::test]
async fn a_pin_names_its_holder_and_warm_follows_the_refresh_and_the_ttl() {
    let mut r = rig(RigSpec::default()).await;
    let app = companion_router(r.engine.clone());
    // A label apart from the id, so a holder named through the label reads another pseudonym.
    r.h.store
        .update_account_config("a", AccountKind::ApiKey, "Work seat")
        .unwrap();
    r.h.router.reload().await.unwrap();
    r.observe("main").await;
    r.advance(MINUTE);
    r.observe("main").await;
    let a = r.h.store.companion_key().pseudonym("a");
    // Placed at NOW (00:16:40), refreshed a minute later, and the body's five-minute marker sets the TTL.
    let pin = |answer: &Answer| {
        let pin = &answer.body["pins"][0];
        (
            answer.body["pins"].as_array().map(Vec::len),
            pin["agent"].clone(),
            pin["account"].clone(),
            pin["placed_at"].clone(),
            pin["warm"].clone(),
            pin["warm_until"].clone(),
        )
    };
    let warm = get(&app, Some(SESSION), Some(KEY)).await;
    assert_eq!(
        pin(&warm),
        (
            Some(1),
            json!("main"),
            json!(a),
            json!("1970-01-01T00:16:40Z"),
            json!(true),
            json!("1970-01-01T00:22:40Z")
        ),
        "{}",
        warm.text
    );
    r.advance(5 * MINUTE);
    let cold = get(&app, Some(SESSION), Some(KEY)).await;
    assert_eq!(
        pin(&cold),
        (
            Some(1),
            json!("main"),
            json!(a),
            json!("1970-01-01T00:16:40Z"),
            json!(false),
            json!("1970-01-01T00:22:40Z")
        ),
        "{}",
        cold.text
    );
}

#[tokio::test]
async fn a_pin_of_another_session_or_another_key_is_absent() {
    let mut r = rig(RigSpec {
        store_clock: std::sync::Arc::new(|| NOW),
        ..RigSpec::default()
    })
    .await;
    let app = companion_router(r.engine.clone());
    // The store's clock stands at one instant for the rig's life, so both keys carry one generation and the key id
    // alone tells the two pins apart.
    let generation = r.generation("desktop");
    r.h.store
        .insert_api_key(&laptop_key(&r, CompanionPermissions::default()), "test")
        .unwrap();
    assert_eq!(r.generation("laptop"), generation);
    r.h.router.reload().await.unwrap();
    r.observe("main").await;
    observe_as(&mut r, KEY, "other").await;
    observe_as(&mut r, OTHER_KEY, SESSION).await;
    assert_eq!(
        r.h.router.pins().await.unwrap().len(),
        3,
        "the asking session's pin, another session's under its key and the same session id's under another key"
    );
    let answer = get(&app, Some(SESSION), Some(KEY)).await;
    assert_eq!(
        answer.body["pins"].as_array().map(Vec::len),
        Some(1),
        "{}",
        answer.text
    );
}

#[tokio::test]
async fn a_missing_or_invalid_session_id_answers_400_invalid_identity() {
    let r = rig(RigSpec::default()).await;
    let app = companion_router(r.engine.clone());
    let refusal = (400, json!({"code": "invalid_identity"}));
    let missing = get(&app, None, Some(KEY)).await;
    assert_eq!((missing.status, missing.body), refusal);
    let invalid = get(&app, Some("bad%20id%21"), Some(KEY)).await;
    assert_eq!((invalid.status, invalid.body), refusal);
    // The one pair is the whole query, so a valid id followed by a second pair is refused too.
    let trailing = get(&app, Some("session&extra=1"), Some(KEY)).await;
    assert_eq!((trailing.status, trailing.body), refusal);
}

#[tokio::test]
async fn a_weekly_windows_runway_runs_to_its_reset_past_the_five_hour_horizon() {
    // Every pin on the account burns a tenth of the week per hour, whatever its tokens.
    let share = SharePricer::default()
        .with_rate(|window, _, _, _, _, _| (window == SEVEN_DAY).then_some(0.1 / 3_600.0));
    let mut r = rig(RigSpec {
        accounts: vec![oauth("a", "sk-ant-oat01-AAAA", 0)],
        router: RouterConfig {
            share,
            ..RouterConfig::default()
        },
        tier: Some(TIER),
        ..RigSpec::default()
    })
    .await;
    let app = companion_router(r.engine.clone());
    r.observe("main").await;
    read_window(&r, "a", SEVEN_DAY, 0.4, NOW + 24 * HOUR_MS).await;
    let answer = get(&app, Some(SESSION), Some(KEY)).await;
    let week = &answer.body["accounts"][0]["windows"][SEVEN_DAY];
    // 0.6 of the week left to the weekly ceiling of 1.0 at 0.1 an hour is 6 hours, past the five-hour horizon and
    // inside the reset a day out.
    assert_eq!(
        (
            week["burn_per_hour"].clone(),
            week["ceiling"].clone(),
            week["runway_minutes"].clone(),
            week["horizon_at"].clone()
        ),
        (
            json!(0.1),
            json!(1.0),
            json!(360),
            json!("1970-01-02T00:16:40Z")
        ),
        "{}",
        answer.text
    );
}

#[tokio::test]
async fn a_weekly_runway_past_the_accounts_end_reads_null_with_the_end_as_its_horizon() {
    // Every pin on the account burns a tenth of the week per hour, whatever its tokens.
    let share = SharePricer::default()
        .with_rate(|window, _, _, _, _, _| (window == SEVEN_DAY).then_some(0.1 / 3_600.0));
    let mut r = rig(RigSpec {
        accounts: vec![oauth("a", "sk-ant-oat01-AAAA", 0)],
        router: RouterConfig {
            share,
            ..RouterConfig::default()
        },
        tier: Some(TIER),
        ..RigSpec::default()
    })
    .await;
    let app = companion_router(r.engine.clone());
    cancel(&r, "a", NOW + 3 * HOUR_MS);
    r.h.router.reload().await.unwrap();
    r.observe("main").await;
    read_window(&r, "a", SEVEN_DAY, 0.4, NOW + 24 * HOUR_MS).await;
    let answer = get(&app, Some(SESSION), Some(KEY)).await;
    let week = &answer.body["accounts"][0]["windows"][SEVEN_DAY];
    // 0.6 of the week left to the weekly ceiling of 1.0 at 0.1 an hour is 6 hours, past the end three hours out and
    // inside the reset a day out, so the end is the window's wall and the runway reaches it first.
    assert_eq!(
        (week["runway_minutes"].clone(), week["horizon_at"].clone()),
        (json!(null), json!("1970-01-01T03:16:40Z")),
        "{}",
        answer.text
    );
}

#[tokio::test]
async fn a_pins_runway_reads_the_routers_milliseconds_as_whole_minutes() {
    // Every pin on the account burns a tenth of the week per hour, whatever its tokens.
    let share = SharePricer::default()
        .with_rate(|window, _, _, _, _, _| (window == SEVEN_DAY).then_some(0.1 / 3_600.0));
    let mut r = rig(RigSpec {
        accounts: vec![oauth("a", "sk-ant-oat01-AAAA", 0)],
        router: RouterConfig {
            share,
            ..RouterConfig::default()
        },
        tier: Some(TIER),
        ..RigSpec::default()
    })
    .await;
    let app = companion_router(r.engine.clone());
    r.observe("main").await;
    read_window(&r, "a", SEVEN_DAY, 0.4, NOW + 24 * HOUR_MS).await;
    let answer = get(&app, Some(SESSION), Some(KEY)).await;
    // The router's runway on the week is 0.6 to the weekly ceiling of 1.0 at 0.1 an hour, 21,600,000 ms inside the
    // reset a day out: 360 minutes.
    assert_eq!(
        answer.body["pins"][0]["runway_minutes"],
        json!(360),
        "{}",
        answer.text
    );
}

#[tokio::test]
async fn each_window_answers_its_own_fit_and_a_spent_fable_week_holds_no_obstacle() {
    let r = rig(RigSpec {
        accounts: vec![oauth("a", "sk-ant-oat01-AAAA", 0)],
        tier: Some(TIER),
        ..RigSpec::default()
    })
    .await;
    let app = companion_router(r.engine.clone());
    read_window(&r, "a", FIVE_HOUR, 0.25, NOW + 2 * HOUR_MS).await;
    read_window(&r, "a", SEVEN_DAY, 0.4, NOW + 72 * HOUR_MS).await;
    read_window(&r, "a", SEVEN_DAY_FABLE, 1.0, NOW + 72 * HOUR_MS).await;
    let answer = get(&app, Some(SESSION), Some(KEY)).await;
    let account = &answer.body["accounts"][0];
    let windows = &account["windows"];
    // Nothing burns, so both shared windows stay under their ceilings, while the Fable week's reading of 1.0 leaves it
    // no allowance. The obstacle is asked for no model in particular, which draws no model week.
    assert_eq!(
        (
            windows[FIVE_HOUR]["fit"].clone(),
            windows[SEVEN_DAY]["fit"].clone(),
            windows[SEVEN_DAY_FABLE]["fit"].clone(),
            account["obstacle"].clone(),
        ),
        (json!("fits"), json!("fits"), json!("spent"), json!(null)),
        "{}",
        answer.text
    );
}

#[tokio::test]
async fn the_allowance_read_answers_each_window_under_the_regime_of_the_sessions_newest_pin() {
    // Every pin burns a tenth of the 5-hour window an hour under `desk`, the one regime, whatever its tokens, and
    // nothing under the base regime.
    let share = SharePricer::default()
        .with_rate(|window, entrypoint, _, _, _, _| {
            (window == FIVE_HOUR && entrypoint == Some("desk")).then_some(0.1 / 3_600.0)
        })
        .with_regimes(|_| vec![("desk".to_string(), vec!["desk".to_string()])]);
    let mut r = rig(RigSpec {
        accounts: vec![oauth("a", "sk-ant-oat01-AAAA", 0)],
        router: RouterConfig {
            share,
            ..RouterConfig::default()
        },
        tier: Some(TIER),
        ..RigSpec::default()
    })
    .await;
    let app = companion_router(r.engine.clone());
    // `newest-desk` uses its agent on `sdk-cli`, an entrypoint no regime lists, first and its agent on `desk` last.
    // `newest-sdk` uses the same two agents in the reverse order.
    observe_on(&mut r, "newest-desk", "main", "sdk-cli").await;
    r.advance(1_000);
    observe_on(&mut r, "newest-desk", "sub", "desk").await;
    r.advance(1_000);
    observe_on(&mut r, "newest-sdk", "sub", "desk").await;
    r.advance(1_000);
    observe_on(&mut r, "newest-sdk", "main", "sdk-cli").await;
    read_window(&r, "a", FIVE_HOUR, 0.5, r.now() + 2 * HOUR_MS).await;
    let fit = |answer: Answer| answer.body["accounts"][0]["windows"][FIVE_HOUR]["fit"].clone();
    // The window has 0.45 of room to its ceiling of 0.95 before its reset two hours out, and the two pins on `desk`
    // burn 0.2 of it an hour. A fresh session's prior adds 0.1 an hour under `desk`, which brings the window to the
    // ceiling in an hour and a half, and nothing under the base regime, which leaves it 2 hours 15 minutes.
    assert_eq!(
        [
            fit(get(&app, Some("newest-desk"), Some(KEY)).await),
            fit(get(&app, Some("newest-sdk"), Some(KEY)).await),
            fit(get(&app, Some("no-pin"), Some(KEY)).await),
        ],
        [json!("rewrite_only"), json!("fits"), json!("fits")]
    );
}

#[tokio::test]
async fn an_account_past_its_saved_end_leaves_the_list_it_reads_in_before_the_end() {
    let r = rig(RigSpec {
        accounts: vec![oauth("a", "sk-ant-oat01-AAAA", 0), api_key("b", B, 1)],
        ..RigSpec::default()
    })
    .await;
    let app = companion_router(r.engine.clone());
    cancel(&r, "a", r.now() + 1_000);
    r.h.router.reload().await.unwrap();
    let key = r.h.store.companion_key();
    let (a, b) = (key.pseudonym("a"), key.pseudonym("b"));
    // Membership alone, since the list's order is the pseudonym order case's to prove.
    let listed = |answer: &Answer| answer.account_ids().into_iter().collect::<BTreeSet<_>>();
    let before = get(&app, Some(SESSION), Some(KEY)).await;
    assert_eq!(
        listed(&before),
        BTreeSet::from([a, b.clone()]),
        "a canceled account stays listed while its end is ahead: {}",
        before.text
    );
    r.advance(10_000);
    let after = get(&app, Some(SESSION), Some(KEY)).await;
    assert_eq!(listed(&after), BTreeSet::from([b]), "{}", after.text);
}

#[tokio::test]
async fn a_budget_the_ledger_has_not_reported_reads_no_spend_row_beside_one_it_has() {
    let r = rig(RigSpec {
        accounts: vec![api_key("a", A, 0)],
        pricing: PriceRevisions::from_yaml(PRICES, "test").unwrap(),
        ..RigSpec::default()
    })
    .await;
    let app = companion_router(r.engine.clone());
    let budgets = |monthly: Option<f64>| {
        [MemberWrite {
            account_id: "a".into(),
            priority: 0,
            budget_daily_usd: Some(10.0),
            budget_monthly_usd: monthly,
        }]
    };
    r.h.store
        .set_pool_members("main", &budgets(None), "test")
        .unwrap();
    r.h.router.reload().await.unwrap();
    let request = RequestRecord {
        id: "spend".into(),
        received_at: r.now(),
        finished_at: Some(r.now() + 500),
        key_id: Some("desktop".into()),
        pool_id: Some("main".into()),
        canonical_model: "claude-sonnet-4-5".into(),
        model: "claude-sonnet-4-5".into(),
        kind: "inference".into(),
        origin: "client".into(),
        outcome: "succeeded".into(),
        attempts: 1,
        ..Default::default()
    };
    let attempt = AttemptRecord {
        request_id: "spend".into(),
        attempt: 1,
        account_id: "a".into(),
        account_kind: AccountKind::ApiKey,
        outcome: "succeeded".into(),
        input_tokens: Some(125_000),
        ..Default::default()
    };
    r.ledger
        .record_request(Record::Request {
            request,
            attempts: vec![attempt],
            bodies: None,
        })
        .await;
    settled(&r, |r| async move {
        r.h.router
            .member_spends()
            .await
            .unwrap()
            .iter()
            .any(|row| row.account_id == "a" && row.spent_usd > 0.0)
    })
    .await;
    // Once the writer has stopped, nothing reads the monthly budget's spend, so the router holds no row for it.
    r.ledger.clone().shutdown().await.unwrap();
    r.h.store
        .set_pool_members("main", &budgets(Some(40.0)), "test")
        .unwrap();
    r.h.router.reload().await.unwrap();
    let answer = get(&app, Some(SESSION), Some(KEY)).await;
    // 125,000 input tokens at $4 a million are $0.50, and the day of NOW ends at the next UTC midnight.
    assert_eq!(
        answer.body["accounts"][0]["spend"],
        json!([{
            "period": "day",
            "spent_usd": 0.5,
            "budget_usd": 10.0,
            "period_end": "1970-01-02T00:00:00Z"
        }]),
        "{}",
        answer.text
    );
}

#[tokio::test]
async fn pinning_reads_off_at_a_zero_affinity_ttl_and_the_pools_mode_at_sixty_seconds() {
    // The pools route takes a TTL of 0 or at least 60, and an enabled keepalive policy only beside a TTL of at least
    // 300, so the pool runs with keepalive off and the least TTL past zero.
    let r = rig(RigSpec {
        policy: KeepalivePolicy::default(),
        ..RigSpec::default()
    })
    .await;
    let app = companion_router(r.engine.clone());
    // The default mode is `always`, so `when_cached` shows the second read takes the pool's own mode.
    set_affinity(&r, 0, AffinityMode::WhenCached).await;
    let off = get(&app, Some(SESSION), Some(KEY)).await;
    assert_eq!(off.body["pool"]["pinning"], json!("off"), "{}", off.text);
    set_affinity(&r, 60, AffinityMode::WhenCached).await;
    let on = get(&app, Some(SESSION), Some(KEY)).await;
    assert_eq!(
        on.body["pool"]["pinning"],
        json!("when_cached"),
        "{}",
        on.text
    );
}

#[tokio::test]
async fn the_key_is_checked_before_the_session_id_and_the_session_id_before_the_permission() {
    let r = rig(RigSpec::default()).await;
    let app = companion_router(r.engine.clone());
    let without = CompanionPermissions {
        allowance: false,
        keepalive: true,
    };
    r.h.store
        .insert_api_key(&laptop_key(&r, without), "test")
        .unwrap();
    r.h.router.reload().await.unwrap();
    let unpermitted = get(&app, None, Some(OTHER_KEY)).await;
    assert_eq!(
        (unpermitted.status, unpermitted.body.clone()),
        (400, json!({"code": "invalid_identity"})),
        "{}",
        unpermitted.text
    );
    let keyless = get(&app, None, None).await;
    assert_eq!(
        (keyless.status, keyless.header("x-phoebis-error-code")),
        (401, Some("auth_missing")),
        "{}",
        keyless.text
    );
}

#[tokio::test]
async fn a_read_after_the_routers_shutdown_answers_503_router_unavailable() {
    let r = rig(RigSpec::default()).await;
    let app = companion_router(r.engine.clone());
    let before = get(&app, Some(SESSION), Some(KEY)).await;
    assert_eq!(before.status, 200, "{}", before.text);
    r.h.router.shutdown().await.unwrap();
    let after = get(&app, Some(SESSION), Some(KEY)).await;
    assert_eq!(
        (after.status, after.body.clone()),
        (503, json!({"code": "router_unavailable"})),
        "{}",
        after.text
    );
}

#[tokio::test]
async fn the_route_answer_on_the_seeded_rig_holds_the_fixtures_shape() {
    let mut r = rig(RigSpec {
        accounts: vec![oauth("b", "sk-ant-oat01-BBBB", 0), api_key("a", A, 1)],
        tier: Some(TIER),
        ..RigSpec::default()
    })
    .await;
    let app = companion_router(r.engine.clone());
    r.h.store
        .set_pool_members(
            "main",
            &[
                relay_common::member("b", 0),
                MemberWrite {
                    account_id: "a".into(),
                    priority: 1,
                    budget_daily_usd: Some(10.0),
                    budget_monthly_usd: None,
                },
            ],
            "test",
        )
        .unwrap();
    r.h.router.reload().await.unwrap();
    read_window(&r, "b", FIVE_HOUR, 0.25, NOW + 2 * HOUR_MS).await;
    read_window(&r, "b", SEVEN_DAY, 0.4, NOW + 72 * HOUR_MS).await;
    r.observe("main").await;
    plant_spend(&r, "spend", 2.5).await;
    let answer = get(&app, Some(SESSION), Some(KEY)).await;
    assert_eq!(answer.status, 200, "{}", answer.text);
    // The store's key is drawn per rig, so each pseudonym is written as a placeholder of its account and the list
    // ordered by those, as the route orders it by the pseudonyms.
    let key = r.h.store.companion_key();
    let text = answer
        .text
        .replace(&format!("\"{}\"", key.pseudonym("a")), "\"aaaaaaaa\"")
        .replace(&format!("\"{}\"", key.pseudonym("b")), "\"bbbbbbbb\"");
    let mut live: Value = serde_json::from_str(&text).unwrap();
    live["accounts"]
        .as_array_mut()
        .unwrap()
        .sort_by_key(|account| account["id"].as_str().unwrap().to_string());
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/allowance.json");
    if std::env::var_os("PHOEBIS_WRITE_FIXTURES").is_some() {
        std::fs::create_dir_all(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures")).unwrap();
        std::fs::write(path, serde_json::to_string_pretty(&live).unwrap() + "\n").unwrap();
        return;
    }
    let committed: Value = serde_json::from_str(
        &std::fs::read_to_string(path)
            .unwrap_or_else(|_| panic!("{path} is missing; run with PHOEBIS_WRITE_FIXTURES=1")),
    )
    .unwrap();
    assert_eq!(
        shape(&live),
        shape(&committed),
        "the route's answer differs in shape from the committed fixture. A renamed, dropped or retyped field is a \
         defect of the route or a contract change the companion's decoder and documentation follow. Re-capture with \
         PHOEBIS_WRITE_FIXTURES=1 only for the latter"
    );
}
