//! The processed rule list per key and the cooldown state grouped by model, both read-only routes.
mod common;

use common::{get, json, login, NOW};
use phoebis_ledger::EventRecord;
use phoebis_routing::{Report, MODEL_REJECTED_REASON};
use phoebis_store::{
    ApiKeyRow, CooldownRow, MemberWrite, ModelRule, OveragePolicy, PoolRow, RulesMode, Strategy,
};
use serde_json::json as j;

const MODEL: &str = "claude-haiku-4-5-20251001";
const OPUS_MODEL: &str = "claude-opus-5-20260301";

fn report_cooldown(account_id: &str, scope: &str, until: i64) -> Report {
    Report::Cooldown {
        account_id: account_id.into(),
        row: CooldownRow {
            account_id: account_id.into(),
            scope: scope.into(),
            until,
            // The task stamps its own clock over this on arrival, so the value here does not matter.
            set_at: 0,
            reason: MODEL_REJECTED_REASON.into(),
            window: "model_rejection".into(),
            source_request_id: None,
            incarnation: 1,
        },
        pin: None,
        lifecycle_generation: 1,
        credential_generation: 1,
        incarnation: 1,
    }
}

#[tokio::test]
async fn the_key_models_route_answers_the_processed_list_with_the_matching_cooldowns() {
    let parts = common::admin_parts_full().await;
    parts
        .store
        .insert_pool(
            &PoolRow {
                label: "Main".into(),
                strategy: Strategy::Priority,
                overage_policy: OveragePolicy::Allow,
                model_rules: vec![
                    ModelRule {
                        pattern: "claude-haiku-*".into(),
                        not: true,
                    },
                    ModelRule {
                        pattern: "claude-opus-*".into(),
                        not: true,
                    },
                ],
                prefer_subscription: false,
                created_at: NOW,
                updated_at: NOW,
                ..PoolRow::for_tests("main")
            },
            "test",
        )
        .unwrap();
    parts
        .store
        .set_pool_members(
            "main",
            &[
                MemberWrite {
                    account_id: "business".into(),
                    priority: 0,
                    budget_daily_usd: None,
                    budget_monthly_usd: None,
                },
                MemberWrite {
                    account_id: "direct".into(),
                    priority: 1,
                    budget_daily_usd: None,
                    budget_monthly_usd: None,
                },
            ],
            "test",
        )
        .unwrap();
    let secret = "phoebis-testkey0000000000000000000000000000000000";
    parts
        .store
        .insert_api_key(
            &ApiKeyRow {
                id: "desktop".into(),
                label: "Desktop".into(),
                notes: None,
                pool_id: "main".into(),
                enabled: true,
                revoked_at: None,
                expires_at: None,
                model_rules: vec![ModelRule {
                    pattern: "claude-opus-*".into(),
                    not: false,
                }],
                model_rules_mode: RulesMode::Extend,
                long_context_twins: true,
                keepalive_opt_out: false,
                body_capture_hours: None,
                secret_digest: parts.store.api_key_digest(secret).to_vec(),
                prefix: phoebis_store::key_prefix(secret),
                incarnation: 1,
                created_at: NOW,
                updated_at: 0,
                rotated_at: None,
                last_used_at: None,
                retired_secret_last_used_at: None,
                auto_mode_server: None,
            },
            "test",
        )
        .unwrap();
    let cookie = login(&parts.app, "admin-secret").await;
    parts
        .router
        .report(report_cooldown(
            "business",
            &format!("model:{MODEL}"),
            NOW + 1_800_000,
        ))
        .await;
    parts
        .router
        .report(report_cooldown(
            "business",
            &format!("model:{OPUS_MODEL}"),
            NOW + 1_800_000,
        ))
        .await;

    let body = json(get(&parts.app, "/admin/keys/desktop/models", Some(&cookie)).await).await;
    assert_eq!(body["key_id"], "desktop");
    assert_eq!(body["pool_id"], "main");
    assert_eq!(body["model_rules_mode"], "extend");
    assert_eq!(body["long_context_twins"], true);
    let rules = body["rules"].as_array().unwrap();
    assert_eq!(
        rules.len(),
        3,
        "the key's allow entry, then the pool's two entries under extend"
    );
    assert_eq!(rules[0]["level"], "key");
    assert_eq!(rules[0]["pattern"], "claude-opus-*");
    assert_eq!(rules[0]["not"], false);
    assert_eq!(
        rules[0]["cooldowns"],
        j!([{ "account_id": "business", "scope": format!("model:{OPUS_MODEL}"), "reason": "model_rejected", "until": NOW + 1_800_000, "window": "model_rejection", "set_at": NOW }]),
        "the key's allow entry answers the opus cooldown its pattern matches"
    );
    assert_eq!(rules[1]["level"], "pool");
    assert_eq!(rules[1]["pattern"], "claude-haiku-*");
    assert_eq!(rules[1]["not"], true);
    assert!(
        rules[1]["cooldowns"].as_array().unwrap().is_empty(),
        "a denied model is never routed, so the matching haiku cooldown says nothing about this deny entry"
    );
    assert_eq!(rules[2]["level"], "pool");
    assert_eq!(rules[2]["pattern"], "claude-opus-*");
    assert_eq!(rules[2]["not"], true);
    assert!(
        rules[2]["cooldowns"].as_array().unwrap().is_empty(),
        "a denied model is never routed, so the matching opus cooldown says nothing about this deny entry"
    );

    assert_eq!(
        get(&parts.app, "/admin/keys/ghost/models", Some(&cookie))
            .await
            .status(),
        404,
        "an unknown key 404s"
    );
}

#[tokio::test]
async fn the_models_state_route_groups_live_cooldowns_by_model_and_lists_the_last_rejections_and_permits(
) {
    let parts = common::admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    let model_scope = format!("model:{MODEL}");
    parts
        .router
        .report(report_cooldown("business", &model_scope, NOW + 1_800_000))
        .await;
    parts
        .router
        .report(report_cooldown("direct", &model_scope, NOW + 1_800_000))
        .await;
    parts
        .router
        .report(report_cooldown("business", "account", NOW + 3_600_000))
        .await;

    parts
        .ledger
        .db()
        .insert_event(&EventRecord {
            at: NOW - 500,
            kind: "model_rejected".into(),
            account_id: Some("business".into()),
            key_id: Some("desktop".into()),
            session_id: Some("s1".into()),
            model: Some(MODEL.into()),
            actor: None,
            detail: Some(
                j!({ "attempt": 1, "request_id": "req-1", "source_request_id": "r-1", "wire_model": "claude-haiku-4-5" })
                    .to_string(),
            ),
        })
        .unwrap();
    parts
        .ledger
        .db()
        .insert_event(&EventRecord {
            at: NOW - 100,
            kind: "model_permitted".into(),
            account_id: Some("direct".into()),
            key_id: Some("desktop".into()),
            session_id: Some("s1".into()),
            model: Some(MODEL.into()),
            actor: None,
            detail: Some(
                j!({ "attempt": 2, "rejected_by": ["business"], "request_id": "req-2", "source_request_id": "r-1", "wire_model": "claude-haiku-4-5" })
                    .to_string(),
            ),
        })
        .unwrap();

    let body = json(get(&parts.app, "/admin/models/state", Some(&cookie)).await).await;
    let groups = body["cooldowns"].as_array().unwrap();
    assert_eq!(groups.len(), 1, "the account-wide row is not grouped here");
    assert_eq!(groups[0]["model"], MODEL);
    let rows = groups[0]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "one row per account, rows sorted by account");
    assert_eq!(rows[0]["account_id"], "business");
    assert_eq!(rows[1]["account_id"], "direct");

    assert_eq!(
        body["rejections"],
        j!([{ "at": NOW - 500, "account_id": "business", "key_id": "desktop", "session_id": "s1", "model": MODEL, "wire_model": "claude-haiku-4-5", "request_id": "r-1", "upstream_request_id": "req-1" }]),
    );
    assert_eq!(
        body["permits"],
        j!([{ "at": NOW - 100, "account_id": "direct", "key_id": "desktop", "session_id": "s1", "model": MODEL, "wire_model": "claude-haiku-4-5", "request_id": "r-1", "upstream_request_id": "req-2", "rejected_by": ["business"] }]),
    );
}
