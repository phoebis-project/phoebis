//! The companion's allowance route: the key's pool as the router reads it, each account under its pseudonym, and the
//! session's pins, behind the key's `allowance` permission.
mod common;
use std::collections::BTreeSet;

use axum::body::Body;
use axum::http::Request;
use common::*;
use phoebis_keepalive::{companion_router, pseudonym, ALLOWANCE_PATH};
use phoebis_routing::{
    Report, RouterConfig, SharePricer, UtilizationSample, WindowReading, WindowSource, FIVE_HOUR,
    HOUR_MS, SEVEN_DAY,
};
use phoebis_store::{
    AccountKind, AccountState, ApiKeyRow, CompanionPermissions, CooldownRow, CooldownScope,
    Identity, MemberWrite, RulesMode, StateCause,
};
use serde_json::{json, Value};

const C: &str = "sk-ant-api03-CCCC";
const D: &str = "sk-ant-api03-DDDD";
const OTHER_KEY: &str = "phoebis-otherkey";
const SESSION: &str = "session";
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
    }
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
    let a = pseudonym(&r.h.store.companion_digest_key(), "a");
    assert_eq!(listed, [(a.as_str(), "cooldown")], "{}", answer.text);
}

#[tokio::test]
async fn the_pseudonym_is_stable_across_calls_and_distinct_between_accounts() {
    let r = rig(RigSpec::default()).await;
    let app = companion_router(r.engine.clone());
    let first = get(&app, Some(SESSION), Some(KEY)).await.account_ids();
    let second = get(&app, Some(SESSION), Some(KEY)).await.account_ids();
    let key = r.h.store.companion_digest_key();
    let (a, b) = (pseudonym(&key, "a"), pseudonym(&key, "b"));
    assert_ne!(a, b);
    for id in [&a, &b] {
        assert!(
            id.len() == 8 && id.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')),
            "{id} is not eight lowercase hex characters"
        );
    }
    let expected: Vec<String> = BTreeSet::from([a, b]).into_iter().collect();
    assert_eq!(
        first, expected,
        "each account under the pseudonym the store's key gives it, in its order"
    );
    assert_eq!(second, first, "the second call names the same pseudonyms");
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
    let a = pseudonym(&r.h.store.companion_digest_key(), "a");
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
async fn a_pool_with_no_placeable_member_answers_an_empty_list() {
    let r = rig(RigSpec::default()).await;
    let app = companion_router(r.engine.clone());
    r.h.store.set_enabled("a", false, "test").unwrap();
    r.h.store.set_enabled("b", false, "test").unwrap();
    r.h.router.reload().await.unwrap();
    let answer = get(&app, Some(SESSION), Some(KEY)).await;
    assert_eq!(
        (
            answer.status,
            answer.body["accounts"].clone(),
            answer.body["pins"].clone()
        ),
        (200, json!([]), json!([])),
        "{}",
        answer.text
    );
}

#[tokio::test]
async fn an_account_without_a_reading_of_a_window_omits_it() {
    let r = rig(RigSpec {
        accounts: vec![oauth("a", "sk-ant-oat01-AAAA", 0)],
        tier: Some(TIER),
        ..RigSpec::default()
    })
    .await;
    let app = companion_router(r.engine.clone());
    read_window(&r, "a", FIVE_HOUR, 0.25, NOW + 2 * HOUR_MS).await;
    let answer = get(&app, Some(SESSION), Some(KEY)).await;
    let windows: Option<Vec<&str>> = answer.body["accounts"][0]["windows"]
        .as_object()
        .map(|windows| windows.keys().map(String::as_str).collect());
    assert_eq!(windows, Some(vec![FIVE_HOUR]), "{}", answer.text);
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
    // 0.55 of the week left to the 0.95 ceiling at 0.1 an hour is 5.5 hours, past the five-hour horizon and inside
    // the reset a day out.
    assert_eq!(
        (
            week["burn_per_hour"].clone(),
            week["runway_minutes"].clone(),
            week["horizon_at"].clone()
        ),
        (json!(0.1), json!(330), json!("1970-01-02T00:16:40Z")),
        "{}",
        answer.text
    );
}

#[tokio::test]
async fn the_fixture_is_the_route_answer_on_the_seeded_rig() {
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
    let key = r.h.store.companion_digest_key();
    let text = answer
        .text
        .replace(&format!("\"{}\"", pseudonym(&key, "a")), "\"aaaaaaaa\"")
        .replace(&format!("\"{}\"", pseudonym(&key, "b")), "\"bbbbbbbb\"");
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
        live, committed,
        "the route's answer differs from the committed fixture. A renamed or dropped field is a defect of the route \
         or a contract change the companion's decoder and documentation follow; re-capture with \
         PHOEBIS_WRITE_FIXTURES=1 only for the latter"
    );
}
