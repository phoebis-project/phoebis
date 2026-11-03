//! The guardrail routes: a guardrail created, read, edited and deleted under its id, attached to pools and keys from
//! either side with the positions read back on all three shapes, refused one sentence per rule, never past 64 per
//! pool or per key, every write reaching the router through the reload, and a definition tested against the captured
//! bodies of a pool or a key.
mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Response, StatusCode};
use axum::Router;
use common::{admin_parts_full, delete, get, json, login, patch, post, Captured, Full};
use phoebis_guardrails::{Condition, GuardrailAction, GuardrailMode, Preset};
use phoebis_ledger::{BodyRecord, RequestRecord};
use phoebis_store::{GuardrailRow, PresetStamp};
use serde_json::{json as j, Value};

const STALE_EDIT: &str = "the guardrail changed while it was being edited, so reload and try again";

/// The preset the binary ships first.
const SUMMARY: &str = "subagent-status-summary";

/// A version of the first preset the table does not ship, read by every case that needs a stamp the table cannot
/// answer, so the guard beside each use fails at once when a build ships it.
const UNSHIPPED_VERSION: u32 = 2;

/// A condition on a fact the step reads without the body or a header.
fn condition() -> Value {
    j!({"subject": "request_class", "operator": "equals", "value": "auxiliary"})
}

/// The first preset at the version this build ships as its current one.
fn summary() -> &'static Preset {
    Preset::current(SUMMARY).expect("the table ships the first preset")
}

/// The conditions as the API writes them: `Condition::words`, with a member the condition lacks left out rather than
/// written as null.
fn views(conditions: &[Condition]) -> Value {
    conditions
        .iter()
        .map(|condition| {
            let mut words = serde_json::to_value(condition.words()).unwrap();
            words
                .as_object_mut()
                .unwrap()
                .retain(|_, value| !value.is_null());
            words
        })
        .collect()
}

/// A row stamped with `preset` at `version` as a build that shipped them would write it, holding the first preset's
/// current conditions so the stamp alone decides `customized`.
fn stamped_row(id: &str, preset: &str, version: u32) -> GuardrailRow {
    GuardrailRow {
        id: id.into(),
        name: format!("Guardrail {id}"),
        description: String::new(),
        enabled: true,
        mode: GuardrailMode::Observe,
        action: GuardrailAction::Block,
        conditions: summary().conditions.clone(),
        preset: Some(PresetStamp {
            id: preset.into(),
            version,
        }),
        incarnation: 1,
        created_at: common::NOW,
        updated_at: common::NOW,
    }
}

/// Creates `id` from the first preset with nothing else in the body, failing the case on any answer but 201.
async fn from_preset(app: &Router, cookie: &str, id: &str) -> Value {
    let body = j!({"id": id, "preset": SUMMARY});
    let response = post(app, "/admin/guardrails", &body.to_string(), Some(cookie)).await;
    assert_eq!(response.status(), StatusCode::CREATED, "{body}");
    json(response).await
}

/// The pool `main` with its key `desktop` and the pool `other` with its key `laptop`, and an admin session.
async fn seeded() -> (Full, String) {
    let parts = admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    for (route, body) in [
        ("/admin/pools", r#"{"id":"main","label":"Main"}"#),
        (
            "/admin/keys",
            r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
        ),
        ("/admin/pools", r#"{"id":"other","label":"Other"}"#),
        (
            "/admin/keys",
            r#"{"id":"laptop","label":"Laptop","pool_id":"other"}"#,
        ),
    ] {
        let response = post(&parts.app, route, body, Some(&cookie)).await;
        assert_eq!(response.status(), StatusCode::CREATED, "{route} {body}");
    }
    (parts, cookie)
}

/// `POST /admin/guardrails` under `id` with `body`'s members, a name and the one condition where `body` names none.
async fn create(app: &Router, cookie: &str, id: &str, mut body: Value) -> Response<Body> {
    let members = body.as_object_mut().expect("the body is an object");
    members.insert("id".into(), j!(id));
    members
        .entry("name")
        .or_insert_with(|| j!(format!("Guardrail {id}")));
    members
        .entry("conditions")
        .or_insert_with(|| j!([condition()]));
    post(app, "/admin/guardrails", &body.to_string(), Some(cookie)).await
}

/// Creates the guardrail and answers its JSON, failing the case on any answer but 201.
async fn created(app: &Router, cookie: &str, id: &str, body: Value) -> Value {
    let response = create(app, cookie, id, body).await;
    assert_eq!(response.status(), StatusCode::CREATED, "create {id}");
    json(response).await
}

async fn refusal(response: Response<Body>) -> (StatusCode, String) {
    let status = response.status();
    let body = json(response).await;
    let message = body["error"].as_str().unwrap_or_default().to_string();
    (status, message)
}

async fn show(app: &Router, cookie: &str, route: &str) -> Value {
    let response = get(app, route, Some(cookie)).await;
    assert_eq!(response.status(), StatusCode::OK, "{route}");
    json(response).await
}

/// The ids and positions of a list of attachments or guardrail references.
fn placed(list: &Value) -> Vec<(String, u64)> {
    list.as_array()
        .expect("a list")
        .iter()
        .map(|entry| {
            (
                entry["id"].as_str().unwrap().to_string(),
                entry["position"].as_u64().unwrap(),
            )
        })
        .collect()
}

fn at(pairs: &[(&str, u64)]) -> Vec<(String, u64)> {
    pairs
        .iter()
        .map(|(id, position)| (id.to_string(), *position))
        .collect()
}

/// The row of `id` in a list answer's `member` array.
fn row<'a>(list: &'a Value, member: &str, id: &str) -> &'a Value {
    list[member]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == id)
        .unwrap_or_else(|| panic!("{id} is not in {member}"))
}

fn ids(list: &Value) -> Vec<String> {
    list["guardrails"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn a_guardrail_is_created_with_every_member_and_read_back_on_the_show_and_list_routes() {
    let (parts, cookie) = seeded().await;
    let app = &parts.app;
    let conditions = j!([
        {"subject": "request_class", "operator": "equals", "value": "auxiliary"},
        {"subject": "header", "operator": "absent", "name": "x-claude-code-agent-type"},
        {"subject": "body", "operator": "present", "path": "tools"},
        {"subject": "message", "operator": "matches", "selector": "last_user", "value": "^Describe"}
    ]);
    let answer = created(
        app,
        &cookie,
        "g1",
        j!({
            "name": "Status forks",
            "description": "The forks a subagent's status line sends",
            "enabled": false,
            "mode": "enforce",
            "action": "block",
            "conditions": conditions,
            "pools": ["other", "main"],
            "keys": ["desktop"],
        }),
    )
    .await;
    assert_eq!(answer["id"], "g1");
    assert_eq!(answer["name"], "Status forks");
    assert_eq!(
        answer["description"],
        "The forks a subagent's status line sends"
    );
    assert_eq!(answer["enabled"], false);
    assert_eq!(answer["mode"], "enforce");
    assert_eq!(answer["action"], "block");
    assert_eq!(answer["conditions"], conditions, "the conditions as sent");
    assert_eq!(
        answer["pools"],
        j!([
            {"id": "main", "label": "Main", "position": 0},
            {"id": "other", "label": "Other", "position": 0},
        ]),
        "the pools with their labels in position order, by id within a position"
    );
    assert_eq!(
        answer["keys"],
        j!([{"id": "desktop", "label": "Desktop", "position": 0}])
    );
    assert!(answer["revision"].is_u64());
    assert!(answer["created_at"].is_i64());
    assert_eq!(answer["created_at"], answer["updated_at"]);
    assert_eq!(show(app, &cookie, "/admin/guardrails/g1").await, answer);

    created(app, &cookie, "b-second", j!({})).await;
    created(app, &cookie, "a-first", j!({})).await;
    let list = show(app, &cookie, "/admin/guardrails").await;
    assert_eq!(ids(&list), ["a-first", "b-second", "g1"], "ordered by id");
    assert_eq!(row(&list, "guardrails", "g1"), &answer);

    let taken = create(app, &cookie, "g1", j!({})).await;
    assert_eq!(
        refusal(taken).await,
        (
            StatusCode::CONFLICT,
            "a guardrail with this id exists".to_string()
        )
    );
}

#[tokio::test]
async fn a_show_reads_the_guardrail_and_its_attachments_off_one_take_of_the_store() {
    let (parts, cookie) = seeded().await;
    let app = &parts.app;
    let answer = created(
        app,
        &cookie,
        "g1",
        j!({"pools": ["main"], "keys": ["desktop"]}),
    )
    .await;
    // The fewest takes over three shows, so a background task that takes the store during one of them cannot pass
    // for a read the show made.
    let mut fewest = u64::MAX;
    for _ in 0..3 {
        let before = parts.store.debug_lock_count();
        assert_eq!(show(app, &cookie, "/admin/guardrails/g1").await, answer);
        fewest = fewest.min(parts.store.debug_lock_count() - before);
    }
    assert_eq!(
        fewest, 1,
        "the row and its attachments come off the one registry load"
    );
}

#[tokio::test]
async fn a_create_with_the_required_members_alone_fills_the_defaults() {
    let (parts, cookie) = seeded().await;
    let response = post(
        &parts.app,
        "/admin/guardrails",
        &j!({"id": "g1", "name": "Bare", "conditions": [condition()]}).to_string(),
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let answer = json(response).await;
    assert_eq!(answer["mode"], "observe");
    assert_eq!(answer["enabled"], true);
    assert_eq!(answer["action"], "block");
    assert_eq!(answer["description"], "");
    assert_eq!(answer["pools"], j!([]));
    assert_eq!(answer["keys"], j!([]));
}

#[tokio::test]
async fn a_patch_is_fenced_on_the_revision_and_the_incarnation() {
    let (parts, cookie) = seeded().await;
    let app = &parts.app;
    let first = created(app, &cookie, "g1", j!({})).await;
    let read = first["revision"].as_u64().unwrap();
    let response = patch(
        app,
        "/admin/guardrails/g1",
        &j!({"name": "Renamed", "expected_revision": read}).to_string(),
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let edited = json(response).await;
    assert_eq!(edited["name"], "Renamed");
    let second = edited["revision"].as_u64().unwrap();
    assert_ne!(second, read, "the edit moves the revision");

    let stale = patch(
        app,
        "/admin/guardrails/g1",
        &j!({"description": "late", "expected_revision": read}).to_string(),
        Some(&cookie),
    )
    .await;
    assert_eq!(
        stale.headers()["x-phoebis-error-code"],
        "stale_edit",
        "a stale revision carries the code"
    );
    assert_eq!(
        refusal(stale).await,
        (StatusCode::CONFLICT, STALE_EDIT.to_string())
    );
    assert_eq!(
        show(app, &cookie, "/admin/guardrails/g1").await["description"],
        "",
        "the stale edit wrote nothing"
    );

    let removed = delete(app, "/admin/guardrails/g1", Some(&cookie)).await;
    assert_eq!(removed.status(), StatusCode::NO_CONTENT);
    let again = created(app, &cookie, "g1", j!({})).await;
    let replaced = patch(
        app,
        "/admin/guardrails/g1",
        &j!({"description": "late", "expected_revision": second}).to_string(),
        Some(&cookie),
    )
    .await;
    assert_eq!(replaced.headers()["x-phoebis-error-code"], "stale_edit");
    assert_eq!(
        refusal(replaced).await,
        (StatusCode::CONFLICT, STALE_EDIT.to_string()),
        "a revision of the deleted guardrail never lands on the one created again"
    );

    let current = again["revision"].as_u64().unwrap();
    let response = patch(
        app,
        "/admin/guardrails/g1",
        &j!({"description": "on time", "expected_revision": current}).to_string(),
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let landed = json(response).await;
    assert_eq!(landed["description"], "on time");
    assert_ne!(landed["revision"].as_u64().unwrap(), current);
}

#[tokio::test]
async fn a_pool_save_opened_before_a_guardrail_side_attachment_answers_409_stale_edit_and_the_reread_carries_it(
) {
    let (parts, cookie) = seeded().await;
    let app = &parts.app;
    created(app, &cookie, "g1", j!({})).await;
    let pools = show(app, &cookie, "/admin/pools").await;
    let read = row(&pools, "pools", "main")["revision"].clone();
    let attached = patch(
        app,
        "/admin/guardrails/g1",
        r#"{"pools":["main"]}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(attached.status(), StatusCode::OK);
    let stale = patch(
        app,
        "/admin/pools/main",
        &j!({"guardrails": [], "expected_revision": read}).to_string(),
        Some(&cookie),
    )
    .await;
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    assert_eq!(stale.headers()["x-phoebis-error-code"], "stale_edit");
    let pools = show(app, &cookie, "/admin/pools").await;
    assert_eq!(
        placed(&row(&pools, "pools", "main")["guardrails"]),
        at(&[("g1", 0)]),
        "the reread carries the attachment the stale save would have dropped"
    );
}

#[tokio::test]
async fn a_key_save_opened_before_a_guardrail_side_attachment_answers_409_stale_edit() {
    let (parts, cookie) = seeded().await;
    let app = &parts.app;
    created(app, &cookie, "g1", j!({})).await;
    let keys = show(app, &cookie, "/admin/keys").await;
    let read = row(&keys, "keys", "desktop")["revision"].clone();
    let attached = patch(
        app,
        "/admin/guardrails/g1",
        r#"{"keys":["desktop"]}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(attached.status(), StatusCode::OK);
    let stale = patch(
        app,
        "/admin/keys/desktop",
        &j!({"guardrails": [], "expected_revision": read}).to_string(),
        Some(&cookie),
    )
    .await;
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    assert_eq!(stale.headers()["x-phoebis-error-code"], "stale_edit");
    let keys = show(app, &cookie, "/admin/keys").await;
    assert_eq!(
        placed(&row(&keys, "keys", "desktop")["guardrails"]),
        at(&[("g1", 0)]),
        "the reread carries the attachment the stale save would have dropped"
    );
}

#[tokio::test]
async fn a_delete_answers_204_then_404() {
    let (parts, cookie) = seeded().await;
    let app = &parts.app;
    created(app, &cookie, "g1", j!({"pools": ["main"]})).await;
    let removed = delete(app, "/admin/guardrails/g1", Some(&cookie)).await;
    assert_eq!(removed.status(), StatusCode::NO_CONTENT);
    let missing = (StatusCode::NOT_FOUND, "guardrail not found".to_string());
    assert_eq!(
        refusal(delete(app, "/admin/guardrails/g1", Some(&cookie)).await).await,
        missing
    );
    assert_eq!(
        refusal(get(app, "/admin/guardrails/g1", Some(&cookie)).await).await,
        missing
    );
    assert_eq!(
        refusal(
            patch(
                app,
                "/admin/guardrails/g1",
                r#"{"name":"G"}"#,
                Some(&cookie)
            )
            .await
        )
        .await,
        missing
    );
    assert_eq!(
        show(app, &cookie, "/admin/pools").await["pools"][0]["guardrails"],
        j!([]),
        "the delete detached the guardrail"
    );
}

#[tokio::test]
async fn every_400_sentence_is_answered_one_by_one() {
    let (parts, cookie) = seeded().await;
    let app = &parts.app;
    let c = condition();
    let seventeen = vec![c.clone(); 17];
    let long = "x".repeat(257);
    let creates: Vec<(Value, &str)> = vec![
        (j!({"name": "G", "conditions": [c]}), "id is required"),
        (
            j!({"id": "Bad_Id", "name": "G", "conditions": [c]}),
            "id must match [a-z0-9][a-z0-9-]{0,63}",
        ),
        (j!({"id": "g1", "conditions": [c]}), "name is required"),
        (
            j!({"id": "g1", "name": "", "conditions": [c]}),
            "name is required",
        ),
        (
            j!({"id": "g1", "name": long, "conditions": [c]}),
            "name is at most 256 bytes",
        ),
        (
            j!({"id": "g1", "name": "Prompt Is Too Long!", "conditions": [c]}),
            "name must not contain \"prompt is too long\"",
        ),
        (
            j!({"id": "g1", "name": "Model: Opus Only", "conditions": [c]}),
            "name must not contain \"model:\"",
        ),
        (
            j!({"id": "g1", "name": "oauth TOKEN has been REVOKED", "conditions": [c]}),
            "name must not contain \"OAuth token has been revoked\"",
        ),
        (
            j!({"id": "g1", "name": "G", "description": long, "conditions": [c]}),
            "description is at most 256 bytes",
        ),
        (
            j!({"id": "g1", "name": "G", "mode": "audit", "conditions": [c]}),
            "mode is observe or enforce",
        ),
        (
            j!({"id": "g1", "name": "G", "action": "allow", "conditions": [c]}),
            "action is block",
        ),
        (
            j!({"id": "g1", "name": "G"}),
            "conditions holds 1 to 16 entries",
        ),
        (
            j!({"id": "g1", "name": "G", "conditions": []}),
            "conditions holds 1 to 16 entries",
        ),
        (
            j!({"id": "g1", "name": "G", "conditions": seventeen}),
            "conditions holds 1 to 16 entries",
        ),
        (
            j!({"id": "g1", "name": "G", "conditions": [c, {"subject": "request_class", "operator": "equals"}]}),
            "conditions[1]: value is required",
        ),
        (
            j!({"id": "g1", "name": "G", "conditions": [c], "pools": ["main", "main"]}),
            "pools lists main twice",
        ),
        (
            j!({"id": "g1", "name": "G", "conditions": [c], "keys": ["desktop", "desktop"]}),
            "keys lists desktop twice",
        ),
        (
            j!({"id": "g1", "name": "G", "conditions": [c], "preset_version": 1}),
            "preset_version applies on edit",
        ),
        (
            j!({"id": "g1", "name": "G", "conditions": [c], "color": "red"}),
            "body must be a JSON object",
        ),
    ];
    for (body, sentence) in &creates {
        let response = post(app, "/admin/guardrails", &body.to_string(), Some(&cookie)).await;
        assert_eq!(
            refusal(response).await,
            (StatusCode::BAD_REQUEST, sentence.to_string()),
            "{body}"
        );
    }
    let uncompiled = create(
        app,
        &cookie,
        "g1",
        j!({"conditions": [{"subject": "model", "operator": "matches", "value": "("}]}),
    )
    .await;
    let (status, message) = refusal(uncompiled).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        message.starts_with("conditions[0]: pattern does not compile: "),
        "the validation compiles the pattern: {message}"
    );
    assert!(
        !message.contains('\n'),
        "the compiler's message is one line: {message}"
    );
    assert_eq!(
        show(app, &cookie, "/admin/guardrails").await,
        j!({"guardrails": []}),
        "no refused create wrote a row"
    );

    let row = created(app, &cookie, "g1", j!({})).await;
    let patches: Vec<(Value, &str)> = vec![
        (j!({"id": "g2"}), "id cannot change"),
        (
            j!({"expected_revision": null}),
            "expected_revision is the revision the row was read at",
        ),
        (
            j!({"expected_revision": "x"}),
            "expected_revision is the revision the row was read at",
        ),
        (j!({"name": ""}), "name is required"),
        (
            j!({"name": "PROMPT IS TOO LONG"}),
            "name must not contain \"prompt is too long\"",
        ),
        (j!({"conditions": []}), "conditions holds 1 to 16 entries"),
        (j!({"mode": "audit"}), "mode is observe or enforce"),
        (j!({"pools": ["main", "main"]}), "pools lists main twice"),
        (j!({"color": "red"}), "body must be a JSON object"),
    ];
    for (body, sentence) in &patches {
        let response = patch(
            app,
            "/admin/guardrails/g1",
            &body.to_string(),
            Some(&cookie),
        )
        .await;
        assert_eq!(
            refusal(response).await,
            (StatusCode::BAD_REQUEST, sentence.to_string()),
            "{body}"
        );
    }
    assert_eq!(
        show(app, &cookie, "/admin/guardrails/g1").await,
        row,
        "no refused edit wrote anything"
    );

    let subjects: [(&str, &str, &str, &str); 4] = [
        (
            "POST",
            "/admin/pools",
            r#"{"id":"spare","label":"Spare","guardrails":[]}"#,
            "guardrails applies on edit",
        ),
        (
            "POST",
            "/admin/keys",
            r#"{"id":"spare","label":"Spare","pool_id":"main","guardrails":[]}"#,
            "guardrails applies on edit",
        ),
        (
            "PATCH",
            "/admin/pools/main",
            r#"{"guardrails":["g1","g1"]}"#,
            "guardrails lists g1 twice",
        ),
        (
            "PATCH",
            "/admin/keys/desktop",
            r#"{"guardrails":["g1","g1"]}"#,
            "guardrails lists g1 twice",
        ),
    ];
    for (method, route, body, sentence) in subjects {
        let response = match method {
            "POST" => post(app, route, body, Some(&cookie)).await,
            _ => patch(app, route, body, Some(&cookie)).await,
        };
        assert_eq!(
            refusal(response).await,
            (StatusCode::BAD_REQUEST, sentence.to_string()),
            "{method} {route}"
        );
    }
    let pools = show(app, &cookie, "/admin/pools").await;
    assert!(
        pools["pools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|pool| pool["id"] != "spare" && pool["guardrails"] == j!([])),
        "no refused pool or key write landed: {pools}"
    );
    let keys = show(app, &cookie, "/admin/keys").await;
    assert!(
        keys["keys"]
            .as_array()
            .unwrap()
            .iter()
            .all(|key| key["id"] != "spare" && key["guardrails"] == j!([])),
        "no refused key write landed: {keys}"
    );
}

#[tokio::test]
async fn an_array_body_is_refused_as_no_object_on_the_three_guardrail_routes() {
    let (parts, cookie) = seeded().await;
    let app = &parts.app;
    let row = created(app, &cookie, "g1", j!({})).await;
    for (method, route) in [
        ("POST", "/admin/guardrails"),
        ("PATCH", "/admin/guardrails/g1"),
        ("POST", "/admin/guardrails/test"),
    ] {
        let response = match method {
            "POST" => post(app, route, "[]", Some(&cookie)).await,
            _ => patch(app, route, "[]", Some(&cookie)).await,
        };
        assert_eq!(
            refusal(response).await,
            (
                StatusCode::BAD_REQUEST,
                "body must be a JSON object".to_string()
            ),
            "{method} {route}"
        );
    }
    assert_eq!(
        show(app, &cookie, "/admin/guardrails").await,
        j!({"guardrails": [row]}),
        "no refused write landed"
    );
}

#[tokio::test]
async fn a_reserved_id_is_refused_and_nothing_is_written() {
    let (parts, cookie) = seeded().await;
    let app = &parts.app;
    for id in ["presets", "test"] {
        assert_eq!(
            refusal(create(app, &cookie, id, j!({})).await).await,
            (StatusCode::BAD_REQUEST, "id is reserved".to_string()),
            "{id}"
        );
    }
    assert_eq!(
        show(app, &cookie, "/admin/guardrails").await,
        j!({"guardrails": []})
    );
}

#[tokio::test]
async fn the_guardrail_side_appends_keeps_and_detaches_and_the_pool_and_key_sides_replace_whole() {
    let (parts, cookie) = seeded().await;
    let app = &parts.app;
    created(app, &cookie, "g1", j!({"pools": ["main"]})).await;
    let g2 = created(app, &cookie, "g2", j!({"pools": ["main"]})).await;
    assert_eq!(
        placed(&g2["pools"]),
        at(&[("main", 1)]),
        "a new guardrail joins the end of the pool's list"
    );

    let response = patch(
        app,
        "/admin/guardrails/g1",
        r#"{"pools":["main","other"]}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        placed(&json(response).await["pools"]),
        at(&[("main", 0), ("other", 0)]),
        "a held pool keeps its place and a new one appends"
    );
    created(app, &cookie, "g3", j!({"pools": ["main"]})).await;
    let response = patch(
        app,
        "/admin/guardrails/g2",
        r#"{"pools":[]}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json(response).await["pools"],
        j!([]),
        "an empty list detaches"
    );
    let pools = show(app, &cookie, "/admin/pools").await;
    assert_eq!(
        placed(&row(&pools, "pools", "main")["guardrails"]),
        at(&[("g1", 0), ("g3", 2)]),
        "the guardrail side reorders nothing it keeps"
    );
    assert_eq!(
        row(&pools, "pools", "main")["guardrails"][1],
        j!({"id": "g3", "name": "Guardrail g3", "mode": "observe", "enabled": true, "position": 2})
    );

    let response = patch(
        app,
        "/admin/pools/main",
        r#"{"guardrails":["g3","g2","g1"]}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        placed(&json(response).await["guardrails"]),
        at(&[("g3", 0), ("g2", 1), ("g1", 2)]),
        "the pool side replaces the list whole in its order"
    );
    assert_eq!(
        placed(&show(app, &cookie, "/admin/guardrails/g1").await["pools"]),
        at(&[("other", 0), ("main", 2)])
    );
    assert_eq!(
        placed(&show(app, &cookie, "/admin/guardrails/g2").await["pools"]),
        at(&[("main", 1)])
    );

    let response = patch(
        app,
        "/admin/keys/desktop",
        r#"{"guardrails":["g2","g1"]}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let desktop = json(response).await;
    assert_eq!(
        placed(&desktop["guardrails"]),
        at(&[("g2", 0), ("g1", 1)]),
        "the key side replaces the key's own list whole"
    );
    assert_eq!(
        placed(&desktop["pool_guardrails"]),
        at(&[("g3", 0), ("g2", 1), ("g1", 2)]),
        "the pool's list in the pool's order"
    );

    let response = patch(
        app,
        "/admin/guardrails/g1",
        r#"{"keys":["laptop","desktop"]}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        placed(&json(response).await["keys"]),
        at(&[("laptop", 0), ("desktop", 1)]),
        "the held key keeps its place and the new one appends"
    );
    let keys = show(app, &cookie, "/admin/keys").await;
    assert_eq!(
        placed(&row(&keys, "keys", "desktop")["guardrails"]),
        at(&[("g2", 0), ("g1", 1)])
    );
    assert_eq!(
        placed(&row(&keys, "keys", "laptop")["guardrails"]),
        at(&[("g1", 0)])
    );
    assert_eq!(
        placed(&row(&keys, "keys", "laptop")["pool_guardrails"]),
        at(&[("g1", 0)])
    );
    let pools = show(app, &cookie, "/admin/pools").await;
    assert_eq!(
        placed(&row(&pools, "pools", "main")["guardrails"]),
        at(&[("g3", 0), ("g2", 1), ("g1", 2)])
    );
    assert_eq!(
        placed(&row(&pools, "pools", "other")["guardrails"]),
        at(&[("g1", 0)])
    );
}

#[tokio::test]
async fn the_64_bound_holds_from_both_sides_with_nothing_written() {
    let (parts, cookie) = seeded().await;
    let app = &parts.app;
    for n in 0..64 {
        created(app, &cookie, &format!("g{n:02}"), j!({"pools": ["main"]})).await;
    }
    let over_desktop = (
        StatusCode::BAD_REQUEST,
        "key desktop would hold more than 64 guardrails".to_string(),
    );
    assert_eq!(
        refusal(create(app, &cookie, "g64", j!({"keys": ["desktop"]})).await).await,
        over_desktop,
        "the guardrail side"
    );
    created(app, &cookie, "extra", j!({})).await;
    assert_eq!(
        refusal(
            patch(
                app,
                "/admin/keys/desktop",
                r#"{"guardrails":["extra"]}"#,
                Some(&cookie)
            )
            .await
        )
        .await,
        over_desktop,
        "the key side"
    );
    created(app, &cookie, "own", j!({"keys": ["laptop"]})).await;
    assert_eq!(
        refusal(
            patch(
                app,
                "/admin/keys/laptop",
                r#"{"pool_id":"main"}"#,
                Some(&cookie)
            )
            .await
        )
        .await,
        (
            StatusCode::BAD_REQUEST,
            "key laptop would hold more than 64 guardrails".to_string()
        ),
        "a key moving under a pool whose list takes it over the bound"
    );

    let guardrails = show(app, &cookie, "/admin/guardrails").await;
    assert_eq!(
        ids(&guardrails).len(),
        66,
        "the refused create wrote no row"
    );
    assert!(!ids(&guardrails).contains(&"g64".to_string()));
    assert_eq!(row(&guardrails, "guardrails", "extra")["keys"], j!([]));
    let keys = show(app, &cookie, "/admin/keys").await;
    let desktop = row(&keys, "keys", "desktop");
    assert_eq!(desktop["guardrails"], j!([]));
    assert_eq!(desktop["pool_guardrails"].as_array().unwrap().len(), 64);
    let laptop = row(&keys, "keys", "laptop");
    assert_eq!(laptop["pool_id"], "other", "the refused move left the key");
    assert_eq!(placed(&laptop["guardrails"]), at(&[("own", 0)]));
    assert_eq!(laptop["pool_guardrails"], j!([]));
    let pools = show(app, &cookie, "/admin/pools").await;
    assert_eq!(
        row(&pools, "pools", "main")["guardrails"]
            .as_array()
            .unwrap()
            .len(),
        64
    );
    assert_eq!(row(&pools, "pools", "other")["guardrails"], j!([]));
}

#[tokio::test]
async fn an_empty_pool_refuses_its_65th_guardrail_and_a_key_created_in_a_full_pool_inherits_64() {
    let (parts, cookie) = seeded().await;
    let app = &parts.app;
    let response = post(
        app,
        "/admin/pools",
        r#"{"id":"empty","label":"Empty"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let mut names = Vec::new();
    for n in 0..64 {
        let id = format!("e{n:02}");
        created(app, &cookie, &id, j!({"pools": ["empty"]})).await;
        names.push(id);
    }
    let over_empty = (
        StatusCode::BAD_REQUEST,
        "pool empty would hold more than 64 guardrails".to_string(),
    );
    assert_eq!(
        refusal(create(app, &cookie, "e64", j!({"pools": ["empty"]})).await).await,
        over_empty,
        "a pool with no key still holds at most 64"
    );
    created(app, &cookie, "extra", j!({})).await;
    names.push("extra".to_string());
    assert_eq!(
        refusal(
            patch(
                app,
                "/admin/pools/empty",
                &j!({ "guardrails": names }).to_string(),
                Some(&cookie)
            )
            .await
        )
        .await,
        over_empty,
        "the pool side"
    );
    let pools = show(app, &cookie, "/admin/pools").await;
    let empty = row(&pools, "pools", "empty")["guardrails"]
        .as_array()
        .unwrap();
    assert_eq!(empty.len(), 64, "the refused list wrote nothing");
    assert!(empty.iter().all(|guardrail| guardrail["id"] != "extra"));

    let response = post(
        app,
        "/admin/keys",
        r#"{"id":"fresh","label":"Fresh","pool_id":"empty"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let fresh = json(response).await;
    assert_eq!(fresh["guardrails"], j!([]));
    assert_eq!(fresh["pool_guardrails"].as_array().unwrap().len(), 64);
}

#[tokio::test]
async fn a_keys_pool_guardrails_follow_its_pool() {
    let (parts, cookie) = seeded().await;
    let app = &parts.app;
    created(app, &cookie, "on-main", j!({"pools": ["main"]})).await;
    created(app, &cookie, "on-other", j!({"pools": ["other"]})).await;
    let keys = show(app, &cookie, "/admin/keys").await;
    assert_eq!(
        placed(&row(&keys, "keys", "laptop")["pool_guardrails"]),
        at(&[("on-other", 0)])
    );
    let response = patch(
        app,
        "/admin/keys/laptop",
        r#"{"pool_id":"main"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        placed(&json(response).await["pool_guardrails"]),
        at(&[("on-main", 0)])
    );
    let keys = show(app, &cookie, "/admin/keys").await;
    assert_eq!(
        placed(&row(&keys, "keys", "laptop")["pool_guardrails"]),
        at(&[("on-main", 0)])
    );
}

#[tokio::test]
async fn a_pool_delete_and_a_key_delete_remove_their_attachments() {
    let (parts, cookie) = seeded().await;
    let app = &parts.app;
    let response = post(
        app,
        "/admin/pools",
        r#"{"id":"spare","label":"Spare"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    created(
        app,
        &cookie,
        "g1",
        j!({"pools": ["spare", "main"], "keys": ["laptop", "desktop"]}),
    )
    .await;
    let removed = delete(app, "/admin/keys/laptop", Some(&cookie)).await;
    assert_eq!(removed.status(), StatusCode::NO_CONTENT);
    let removed = delete(app, "/admin/pools/spare", Some(&cookie)).await;
    assert_eq!(removed.status(), StatusCode::NO_CONTENT);
    let g1 = show(app, &cookie, "/admin/guardrails/g1").await;
    assert_eq!(placed(&g1["pools"]), at(&[("main", 0)]));
    assert_eq!(placed(&g1["keys"]), at(&[("desktop", 0)]));
    let held = parts.store.guardrail_attachments().unwrap();
    assert!(
        held.iter()
            .all(|a| !matches!(a.subject.id(), "laptop" | "spare")),
        "no attachment outlives its subject: {held:?}"
    );

    for (route, body) in [
        ("/admin/pools", r#"{"id":"spare","label":"Spare"}"#),
        (
            "/admin/keys",
            r#"{"id":"laptop","label":"Laptop","pool_id":"spare"}"#,
        ),
    ] {
        let response = post(app, route, body, Some(&cookie)).await;
        assert_eq!(response.status(), StatusCode::CREATED, "{route}");
        assert_eq!(
            json(response).await["guardrails"],
            j!([]),
            "an id created again starts with none"
        );
    }
}

#[tokio::test]
async fn the_mutation_log_names_the_guardrail_entity_and_the_change_log_the_three_kinds_and_the_guardrails_member(
) {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(captured.clone())
        .finish();
    // The harness drives every request through `oneshot` on this test's own task, so a thread-local subscriber
    // sees every line the gate writes.
    let _guard = tracing::subscriber::set_default(subscriber);
    let (parts, cookie) = seeded().await;
    // The other cases of this file write mutation lines on threads with no subscriber, and one of them can register
    // the line's callsite as unwanted while this case's subscriber is being set. The seeding registered it by now,
    // so a rebuild reads every subscriber's interest again.
    tracing::callsite::rebuild_interest_cache();
    let app = &parts.app;
    let session = show(app, &cookie, "/admin/session").await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let after = parts
        .store
        .newest_registry_change_id()
        .unwrap()
        .unwrap_or(0);

    created(app, &cookie, "g1", j!({})).await;
    let response = patch(
        app,
        "/admin/guardrails/g1",
        r#"{"mode":"enforce"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = patch(
        app,
        "/admin/pools/main",
        r#"{"guardrails":["g1"]}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let removed = delete(app, "/admin/guardrails/g1", Some(&cookie)).await;
    assert_eq!(removed.status(), StatusCode::NO_CONTENT);

    const MARK: &str = " INFO phoebis_admin::routes: admin mutation ";
    let lines: Vec<String> = captured
        .text()
        .lines()
        .filter_map(|line| line.split_once(MARK).map(|(_, fields)| fields.to_string()))
        .filter(|fields| fields.contains("route=/admin/guardrails"))
        .collect();
    let admitted = format!("credential=session session={session}");
    assert_eq!(
        lines,
        vec![
            format!("method=POST route=/admin/guardrails status=201 entity=guardrail id=g1 {admitted}"),
            format!(
                "method=PATCH route=/admin/guardrails/{{id}} status=200 entity=guardrail id=g1 {admitted}"
            ),
            format!(
                "method=DELETE route=/admin/guardrails/{{id}} status=204 entity=guardrail id=g1 {admitted}"
            ),
        ]
    );

    let changes = parts.store.registry_changes_after(after, 100).unwrap();
    let kinds: Vec<(&str, &str)> = changes
        .iter()
        .map(|change| (change.kind.as_str(), change.target.as_str()))
        .collect();
    assert_eq!(
        kinds,
        [
            ("guardrail_created", "g1"),
            ("guardrail_updated", "g1"),
            ("pool_updated", "main"),
            ("guardrail_deleted", "g1"),
        ]
    );
    let pool_diff: Value = serde_json::from_str(&changes[2].diff_json).unwrap();
    assert_eq!(
        pool_diff["guardrails"],
        j!({"from": [], "to": ["g1"]}),
        "the pool's change names its list"
    );
}

#[tokio::test]
async fn the_routers_snapshot_runs_the_pools_list_then_the_keys_after_a_write() {
    let (parts, cookie) = seeded().await;
    let app = &parts.app;
    for id in ["g1", "g2", "g3"] {
        created(app, &cookie, id, j!({})).await;
    }
    let response = patch(
        app,
        "/admin/pools/main",
        r#"{"guardrails":["g2","g1"]}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = patch(
        app,
        "/admin/keys/desktop",
        r#"{"guardrails":["g1","g3"]}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let snapshot = parts.router.snapshot();
    let ran = |key_id: &str| -> Vec<String> {
        let key = snapshot.key(key_id).expect("the key is in the snapshot");
        snapshot
            .guardrails_for(key)
            .iter()
            .map(|guardrail| guardrail.id.clone())
            .collect()
    };
    assert_eq!(
        ran("desktop"),
        ["g2", "g1", "g3"],
        "the pool's list, then the key's own, one on both once at its pool position"
    );
    assert!(ran("laptop").is_empty());
}

#[tokio::test]
async fn an_unknown_pool_or_key_on_a_write_is_a_404_with_nothing_written() {
    let (parts, cookie) = seeded().await;
    let app = &parts.app;
    assert_eq!(
        refusal(create(app, &cookie, "g1", j!({"pools": ["ghost"]})).await).await,
        (StatusCode::NOT_FOUND, "pool ghost not found".to_string())
    );
    assert_eq!(
        refusal(create(app, &cookie, "g1", j!({"keys": ["ghost"]})).await).await,
        (StatusCode::NOT_FOUND, "key ghost not found".to_string())
    );
    assert_eq!(
        show(app, &cookie, "/admin/guardrails").await,
        j!({"guardrails": []})
    );

    let row = created(app, &cookie, "g1", j!({"pools": ["main"]})).await;
    assert_eq!(
        refusal(
            patch(
                app,
                "/admin/guardrails/g1",
                r#"{"pools":["main","ghost"]}"#,
                Some(&cookie)
            )
            .await
        )
        .await,
        (StatusCode::NOT_FOUND, "pool ghost not found".to_string())
    );
    assert_eq!(
        refusal(
            patch(
                app,
                "/admin/guardrails/g1",
                r#"{"keys":["ghost"]}"#,
                Some(&cookie)
            )
            .await
        )
        .await,
        (StatusCode::NOT_FOUND, "key ghost not found".to_string())
    );
    assert_eq!(show(app, &cookie, "/admin/guardrails/g1").await, row);

    let pools = show(app, &cookie, "/admin/pools").await;
    let keys = show(app, &cookie, "/admin/keys").await;
    for (route, body) in [
        ("/admin/pools/main", r#"{"guardrails":["g1","ghost"]}"#),
        ("/admin/keys/desktop", r#"{"guardrails":["ghost"]}"#),
    ] {
        assert_eq!(
            refusal(patch(app, route, body, Some(&cookie)).await).await,
            (
                StatusCode::NOT_FOUND,
                "guardrail ghost not found".to_string()
            ),
            "{route}"
        );
    }
    let main = row_of(&pools, "pools", "main");
    let desktop = row_of(&keys, "keys", "desktop");
    let pools = show(app, &cookie, "/admin/pools").await;
    let keys = show(app, &cookie, "/admin/keys").await;
    assert_eq!(
        row_of(&pools, "pools", "main")["revision"],
        main["revision"],
        "the refused pool write moved nothing"
    );
    assert_eq!(
        placed(&row_of(&pools, "pools", "main")["guardrails"]),
        at(&[("g1", 0)])
    );
    assert_eq!(
        row_of(&keys, "keys", "desktop")["revision"],
        desktop["revision"],
        "the refused key write moved nothing"
    );
    assert_eq!(row_of(&keys, "keys", "desktop")["guardrails"], j!([]));
}

fn row_of(list: &Value, member: &str, id: &str) -> Value {
    row(list, member, id).clone()
}

#[tokio::test]
async fn the_presets_route_answers_the_tables_current_versions() {
    let (parts, cookie) = seeded().await;
    let answer = show(&parts.app, &cookie, "/admin/guardrails/presets").await;
    let listed: Vec<(&str, u64)> = answer["presets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|preset| {
            (
                preset["id"].as_str().unwrap(),
                preset["version"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        listed,
        [
            ("away-recap", 1),
            ("prompt-suggestion", 1),
            ("subagent-status-summary", 1)
        ],
        "one entry per preset at its current version, ordered by id"
    );
    let entry = row(&answer, "presets", SUMMARY);
    let summary = summary();
    assert_eq!(entry["version"], 1);
    assert_eq!(entry["suggested_mode"], "observe");
    assert_eq!(entry["name"], summary.name);
    assert_eq!(entry["description"], summary.description);
    assert_eq!(entry["measured_on"], summary.measured_on);
    assert_eq!(entry["conditions"].as_array().unwrap().len(), 3);
    assert_eq!(
        entry["conditions"],
        views(&summary.conditions),
        "the conditions in the table's order"
    );
}

#[tokio::test]
async fn a_create_from_the_preset_copies_the_conditions_and_stamps_the_row_and_the_bodys_members_win(
) {
    let (parts, cookie) = seeded().await;
    let app = &parts.app;
    let summary = summary();
    let response = post(
        app,
        "/admin/guardrails",
        &j!({"id": "summary", "preset": SUMMARY, "mode": "enforce", "name": "Mine"}).to_string(),
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let answer = json(response).await;
    assert_eq!(answer["conditions"].as_array().unwrap().len(), 3);
    assert_eq!(
        answer["conditions"],
        views(&summary.conditions),
        "the preset's conditions in its order"
    );
    assert_eq!(answer["mode"], "enforce", "the body's mode wins");
    assert_eq!(answer["name"], "Mine", "the body's name wins");
    assert_eq!(
        answer["description"], "",
        "the preset's description stays on the presets route"
    );
    assert_eq!(
        answer["preset"],
        j!({"id": SUMMARY, "version": 1, "current_version": 1, "customized": false})
    );
    assert_eq!(
        show(app, &cookie, "/admin/guardrails/summary").await,
        answer
    );

    let bare = from_preset(app, &cookie, "bare").await;
    assert_eq!(
        bare["name"], summary.name,
        "the preset supplies the name the body leaves out"
    );
    assert_eq!(bare["mode"], "observe", "and its suggested mode");
    assert_eq!(bare["description"], "");
    assert_eq!(bare["conditions"], answer["conditions"]);
    assert_eq!(bare["preset"], answer["preset"]);
}

#[tokio::test]
async fn a_create_from_the_preset_with_the_bodys_own_conditions_and_description_keeps_both_under_the_presets_stamp(
) {
    let (parts, cookie) = seeded().await;
    let app = &parts.app;
    let summary = summary();
    let response = post(
        app,
        "/admin/guardrails",
        &j!({
            "id": "mine",
            "preset": SUMMARY,
            "conditions": [condition()],
            "description": "Forks of one team",
        })
        .to_string(),
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let answer = json(response).await;
    assert_eq!(
        answer["conditions"],
        j!([condition()]),
        "the body's conditions win"
    );
    assert_eq!(
        answer["description"], "Forks of one team",
        "the body's description wins"
    );
    assert_eq!(
        answer["preset"],
        j!({
            "id": SUMMARY,
            "version": summary.version,
            "current_version": summary.version,
            "customized": true,
        }),
        "the stamp is the preset's, and customized says the conditions left it"
    );
    assert_eq!(show(app, &cookie, "/admin/guardrails/mine").await, answer);
}

#[tokio::test]
async fn an_unknown_preset_is_refused() {
    let (parts, cookie) = seeded().await;
    let app = &parts.app;
    let response = post(
        app,
        "/admin/guardrails",
        &j!({"id": "g1", "name": "G", "conditions": [condition()], "preset": "ghost"}).to_string(),
        Some(&cookie),
    )
    .await;
    assert_eq!(
        refusal(response).await,
        (StatusCode::BAD_REQUEST, "unknown preset ghost".to_string())
    );
    assert_eq!(
        show(app, &cookie, "/admin/guardrails").await,
        j!({"guardrails": []}),
        "the refused create wrote nothing"
    );
}

#[tokio::test]
async fn customized_reads_true_after_a_condition_changes() {
    let (parts, cookie) = seeded().await;
    let app = &parts.app;
    let created = from_preset(app, &cookie, "summary").await;
    assert_eq!(created["preset"]["customized"], false);
    let two = Value::from(views(&summary().conditions).as_array().unwrap()[..2].to_vec());
    let response = patch(
        app,
        "/admin/guardrails/summary",
        &j!({"conditions": two}).to_string(),
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let edited = json(response).await;
    assert_eq!(edited["conditions"], two);
    assert_eq!(
        edited["preset"],
        j!({"id": SUMMARY, "version": 1, "current_version": 1, "customized": true}),
        "the stamp stays and customized says the conditions left it"
    );
    assert_eq!(
        show(app, &cookie, "/admin/guardrails/summary").await["preset"],
        edited["preset"]
    );
}

#[tokio::test]
async fn a_patch_with_a_version_the_table_lacks_is_refused_and_one_without_a_stamp_too() {
    let (parts, cookie) = seeded().await;
    let app = &parts.app;
    assert!(
        Preset::at(SUMMARY, UNSHIPPED_VERSION).is_none(),
        "the table lacks version {UNSHIPPED_VERSION}"
    );
    let stamped = from_preset(app, &cookie, "summary").await;
    let plain = created(app, &cookie, "g1", j!({})).await;
    let patches = [
        (
            "/admin/guardrails/summary",
            j!({"preset_version": UNSHIPPED_VERSION}),
            format!("preset_version is not a shipped version of {SUMMARY}"),
        ),
        (
            "/admin/guardrails/g1",
            j!({"preset_version": 1}),
            "guardrail has no preset".to_string(),
        ),
        (
            "/admin/guardrails/summary",
            j!({"preset": SUMMARY}),
            "preset applies on create".to_string(),
        ),
        (
            "/admin/guardrails/g1",
            j!({"preset": SUMMARY}),
            "preset applies on create".to_string(),
        ),
    ];
    for (route, body, sentence) in patches {
        let response = patch(app, route, &body.to_string(), Some(&cookie)).await;
        assert_eq!(
            refusal(response).await,
            (StatusCode::BAD_REQUEST, sentence),
            "{route} {body}"
        );
    }
    assert_eq!(
        show(app, &cookie, "/admin/guardrails/summary").await,
        stamped,
        "no refused edit wrote anything"
    );
    assert_eq!(show(app, &cookie, "/admin/guardrails/g1").await, plain);
}

#[tokio::test]
async fn a_patch_with_the_shipped_version_and_its_conditions_reads_customized_false_again() {
    let (parts, cookie) = seeded().await;
    let app = &parts.app;
    from_preset(app, &cookie, "summary").await;
    let response = patch(
        app,
        "/admin/guardrails/summary",
        &j!({"conditions": [condition()]}).to_string(),
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json(response).await["preset"]["customized"], true);
    let current = summary();
    let response = patch(
        app,
        "/admin/guardrails/summary",
        &j!({"conditions": views(&current.conditions), "preset_version": current.version})
            .to_string(),
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let updated = json(response).await;
    assert_eq!(updated["conditions"], views(&current.conditions));
    assert_eq!(
        updated["preset"],
        j!({"id": SUMMARY, "version": 1, "current_version": 1, "customized": false}),
        "the shipped version's conditions read as the preset again"
    );

    // A row stamped at a version the table lacks shows the stamp move whatever versions the table ships.
    assert!(
        Preset::at(SUMMARY, UNSHIPPED_VERSION).is_none(),
        "the table lacks version {UNSHIPPED_VERSION}"
    );
    parts
        .store
        .insert_guardrail(
            &stamped_row("newer", SUMMARY, UNSHIPPED_VERSION),
            &[],
            &[],
            "admin",
        )
        .unwrap();
    assert_eq!(
        show(app, &cookie, "/admin/guardrails/newer").await["preset"]["customized"],
        true
    );
    let response = patch(
        app,
        "/admin/guardrails/newer",
        &j!({"preset_version": current.version}).to_string(),
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json(response).await["preset"],
        j!({"id": SUMMARY, "version": 1, "current_version": 1, "customized": false}),
        "the stamp moves to the shipped version"
    );
}

#[tokio::test]
async fn a_guardrail_created_without_preset_carries_null() {
    let (parts, cookie) = seeded().await;
    let app = &parts.app;
    let answer = created(app, &cookie, "g1", j!({})).await;
    assert!(
        answer.as_object().unwrap().contains_key("preset"),
        "the member is written, as null"
    );
    assert_eq!(answer["preset"], Value::Null);
    assert_eq!(
        show(app, &cookie, "/admin/guardrails/g1").await["preset"],
        Value::Null
    );
    let list = show(app, &cookie, "/admin/guardrails").await;
    let listed = row(&list, "guardrails", "g1");
    assert!(listed.as_object().unwrap().contains_key("preset"));
    assert_eq!(listed["preset"], Value::Null);
}

#[tokio::test]
async fn a_stamp_the_table_cannot_answer_reads_customized_with_the_tables_current_version() {
    let (parts, cookie) = seeded().await;
    let app = &parts.app;
    let current = summary();
    assert!(
        Preset::at(SUMMARY, UNSHIPPED_VERSION).is_none(),
        "the table lacks version {UNSHIPPED_VERSION}"
    );
    assert!(Preset::current("ghost").is_none(), "the table lacks the id");
    for row in [
        stamped_row("newer", SUMMARY, UNSHIPPED_VERSION),
        stamped_row("orphan", "ghost", 1),
    ] {
        parts
            .store
            .insert_guardrail(&row, &[], &[], "admin")
            .unwrap();
    }
    assert_eq!(
        show(app, &cookie, "/admin/guardrails/newer").await["preset"],
        j!({"id": SUMMARY, "version": UNSHIPPED_VERSION, "current_version": current.version, "customized": true}),
        "a version the table lacks cannot be compared"
    );
    assert_eq!(
        show(app, &cookie, "/admin/guardrails/orphan").await["preset"],
        j!({"id": "ghost", "version": 1, "current_version": 1, "customized": true}),
        "a preset the table lacks keeps its stamp and cannot be compared"
    );
}

/// The client's subagent summary fork and an auto mode classifier call, in their documented shapes.
const FORK: &[u8] = include_bytes!("../../guardrails/tests/fixtures/subagent_summary_fork.json");
const CLASSIFIER: &[u8] =
    include_bytes!("../../guardrails/tests/fixtures/auto_mode_classifier.json");

const MINUTE: i64 = 60_000;

/// An inference request of `key` in the pool `main`, `minutes` before the clock, with nothing captured.
fn ledger_row(id: &str, minutes: i64, key: &str) -> RequestRecord {
    let at = common::NOW - minutes * MINUTE;
    RequestRecord {
        id: id.into(),
        received_at: at,
        finished_at: Some(at + 1_000),
        key_id: Some(key.into()),
        pool_id: Some("main".into()),
        model: "claude-sonnet-5".into(),
        canonical_model: "claude-sonnet-5".into(),
        kind: "inference".into(),
        origin: "client".into(),
        status: Some(200),
        outcome: "succeeded".into(),
        client: Some("claude-code".into()),
        ..Default::default()
    }
}

/// A fork of the summary as the row records it: the auxiliary class and the agent type hint.
fn fork_row(id: &str, minutes: i64, key: &str) -> RequestRecord {
    RequestRecord {
        request_class: Some("auxiliary".into()),
        agent_type: Some("general-purpose".into()),
        ..ledger_row(id, minutes, key)
    }
}

/// Writes the row, flagged captured with `body` beside it where one is given.
fn write_row(parts: &Full, row: RequestRecord, body: Option<&[u8]>) {
    let bodies = body.map(|body| BodyRecord {
        request_id: row.id.clone(),
        request_body: body.to_vec(),
        response_body: None,
        request_len_seen: body.len() as u64,
        response_len_seen: 0,
        expires_at: common::NOW + 3_600_000,
    });
    let row = RequestRecord {
        bodies_captured: row.bodies_captured || bodies.is_some(),
        ..row
    };
    parts.db.insert_request(&row, &[], bodies.as_ref()).unwrap();
}

/// The pool `main` with both of its keys, `desktop` and `laptop`, an admin session, and eight rows in the pool, newest
/// first: a fork, a classifier call and a fork with their bodies, an uncaptured row, a probe with a body, a row flagged
/// captured whose body row is gone, another uncaptured row, and a classifier call with its body.
async fn seeded_sample() -> (Full, String) {
    let parts = admin_parts_full().await;
    let cookie = login(&parts.app, "admin-secret").await;
    for (route, body) in [
        ("/admin/pools", r#"{"id":"main","label":"Main"}"#),
        (
            "/admin/keys",
            r#"{"id":"desktop","label":"Desktop","pool_id":"main"}"#,
        ),
        (
            "/admin/keys",
            r#"{"id":"laptop","label":"Laptop","pool_id":"main"}"#,
        ),
    ] {
        let response = post(&parts.app, route, body, Some(&cookie)).await;
        assert_eq!(response.status(), StatusCode::CREATED, "{route} {body}");
    }
    write_row(&parts, fork_row("r1-fork", 1, "desktop"), Some(FORK));
    write_row(
        &parts,
        ledger_row("r2-classifier", 2, "laptop"),
        Some(CLASSIFIER),
    );
    write_row(&parts, fork_row("r3-fork", 3, "laptop"), Some(FORK));
    write_row(&parts, ledger_row("r4-uncaptured", 4, "desktop"), None);
    write_row(
        &parts,
        RequestRecord {
            kind: "probe".into(),
            ..ledger_row("r5-probe", 5, "desktop")
        },
        Some(FORK),
    );
    write_row(
        &parts,
        RequestRecord {
            bodies_captured: true,
            ..ledger_row("r6-gone", 6, "laptop")
        },
        None,
    );
    write_row(&parts, ledger_row("r7-uncaptured", 7, "laptop"), None);
    write_row(
        &parts,
        ledger_row("r8-classifier", 8, "desktop"),
        Some(CLASSIFIER),
    );
    (parts, cookie)
}

/// The fork as three conditions: the class, the tools and the opening words of the last user turn.
fn fork_conditions() -> Value {
    j!([
        {"subject": "request_class", "operator": "equals", "value": "auxiliary"},
        {"subject": "body", "operator": "present", "path": "tools"},
        {"subject": "message", "operator": "matches", "selector": "last_user", "value": "^Describe your most recent action"},
    ])
}

/// `POST /admin/guardrails/test` with `body`: the status and the answer.
async fn dry_run(app: &Router, cookie: &str, body: Value) -> (StatusCode, Value) {
    let response = post(
        app,
        "/admin/guardrails/test",
        &body.to_string(),
        Some(cookie),
    )
    .await;
    let status = response.status();
    (status, json(response).await)
}

/// The sampled requests' ids in the answer's order.
fn sample_ids(answer: &Value) -> Vec<String> {
    answer["requests"]
        .as_array()
        .expect("the answer lists its requests")
        .iter()
        .map(|request| request["id"].as_str().unwrap().to_string())
        .collect()
}

/// Each sampled request's id, verdict and condition index, in the answer's order.
fn verdicts(answer: &Value) -> Vec<(String, String, Value)> {
    answer["requests"]
        .as_array()
        .expect("the answer lists its requests")
        .iter()
        .map(|request| {
            (
                request["id"].as_str().unwrap().to_string(),
                request["verdict"].as_str().unwrap().to_string(),
                request["condition"].clone(),
            )
        })
        .collect()
}

fn verdict(id: &str, word: &str, condition: Value) -> (String, String, Value) {
    (id.to_string(), word.to_string(), condition)
}

/// Adds `id` as a key of the pool `main`, so a key sample reads the case's own rows apart from the seeded ones.
async fn sample_key(app: &Router, cookie: &str, id: &str) {
    let body = j!({"id": id, "label": id, "pool_id": "main"}).to_string();
    let response = post(app, "/admin/keys", &body, Some(cookie)).await;
    assert_eq!(response.status(), StatusCode::CREATED, "{body}");
}

#[tokio::test]
async fn the_pool_sample_walks_both_keys_newest_first_and_the_key_sample_one_key() {
    let (parts, cookie) = seeded_sample().await;
    let app = &parts.app;
    let (status, pool) = dry_run(
        app,
        &cookie,
        j!({"conditions": [condition()], "pool_id": "main"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{pool}");
    assert_eq!(pool["sample"]["pool_id"], "main");
    assert!(
        pool["sample"].get("key_id").is_none(),
        "the sample names the one subject the test named"
    );
    assert_eq!(pool["totals"]["sampled"], 4);
    assert_eq!(
        sample_ids(&pool),
        ["r1-fork", "r2-classifier", "r3-fork", "r8-classifier"],
        "both keys of the pool, newest first"
    );
    for (key, expected) in [
        ("desktop", ["r1-fork", "r8-classifier"]),
        ("laptop", ["r2-classifier", "r3-fork"]),
    ] {
        let (status, answer) = dry_run(
            app,
            &cookie,
            j!({"conditions": [condition()], "key_id": key}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{answer}");
        assert_eq!(answer["sample"]["key_id"], key);
        assert!(answer["sample"].get("pool_id").is_none());
        assert_eq!(answer["totals"]["sampled"], 2);
        assert_eq!(sample_ids(&answer), expected, "{key} alone");
    }
}

#[tokio::test]
async fn the_limit_and_the_window_hold() {
    let (parts, cookie) = seeded_sample().await;
    let app = &parts.app;
    // Past any capture window the harness could configure, with everything else the sample reads.
    write_row(&parts, fork_row("r-old", 1_000 * 60, "desktop"), Some(FORK));
    // Three hours back, inside the window, so a window read in minutes leaves it out.
    write_row(&parts, fork_row("r-hours", 3 * 60, "laptop"), Some(FORK));
    let (status, limited) = dry_run(
        app,
        &cookie,
        j!({"conditions": [condition()], "pool_id": "main", "limit": 2}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{limited}");
    assert_eq!(sample_ids(&limited), ["r1-fork", "r2-classifier"]);
    assert_eq!(limited["totals"]["sampled"], 2);
    assert_eq!(limited["sample"]["limit"], 2);

    let (status, whole) = dry_run(
        app,
        &cookie,
        j!({"conditions": [condition()], "pool_id": "main"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{whole}");
    assert_eq!(
        sample_ids(&whole),
        [
            "r1-fork",
            "r2-classifier",
            "r3-fork",
            "r8-classifier",
            "r-hours"
        ],
        "the row three hours back is kept and the row older than the window is left out"
    );
    assert_eq!(whole["sample"]["limit"], 200, "the default limit");
    // The admin harness builds its state with `BODY_CAPTURE_MAX_HOURS`, 72, as the longest capture a key may ask for.
    assert_eq!(whole["sample"]["window_hours"], 72);
}

#[tokio::test]
async fn a_key_with_no_captured_row_inside_the_window_answers_an_empty_sample_with_its_figures() {
    let (parts, cookie) = seeded_sample().await;
    let app = &parts.app;
    sample_key(app, &cookie, "ci").await;
    // Captured, and older than the window, so the key holds a row and its window none.
    write_row(&parts, fork_row("ci-old", 1_000 * 60, "ci"), Some(FORK));
    let (status, answer) = dry_run(
        app,
        &cookie,
        j!({"conditions": [condition()], "key_id": "ci", "limit": 5}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        answer,
        j!({
            "sample": {"key_id": "ci", "limit": 5, "window_hours": 72},
            "totals": {"sampled": 0, "matched": 0, "unmatched": 0, "indeterminate": 0},
            "requests": [],
        })
    );
}

#[tokio::test]
async fn a_body_past_its_own_keys_capture_window_is_left_out_before_the_purge_removes_it() {
    let (parts, cookie) = seeded_sample().await;
    // Captured 61 minutes ago under a key's one-hour window, so its body expired a minute ago while the longest
    // window still covers the row and no purge has run since.
    let row = RequestRecord {
        bodies_captured: true,
        ..fork_row("r-expired", 61, "desktop")
    };
    let body = BodyRecord {
        request_id: row.id.clone(),
        request_body: FORK.to_vec(),
        response_body: None,
        request_len_seen: FORK.len() as u64,
        response_len_seen: 0,
        expires_at: common::NOW - MINUTE,
    };
    parts.db.insert_request(&row, &[], Some(&body)).unwrap();
    let (status, answer) = dry_run(
        &parts.app,
        &cookie,
        j!({"conditions": [condition()], "pool_id": "main"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        sample_ids(&answer),
        ["r1-fork", "r2-classifier", "r3-fork", "r8-classifier"],
        "the row whose body expired is left out"
    );
    assert_eq!(answer["totals"]["sampled"], 4);
}

#[tokio::test]
async fn an_uncaptured_row_a_probe_and_a_row_whose_body_is_gone_are_skipped_without_counting() {
    let (parts, cookie) = seeded_sample().await;
    // The first page of four reads the row whose body is gone as its fourth, so a skipped row counted toward the
    // limit would leave the oldest captured row out.
    let (status, answer) = dry_run(
        &parts.app,
        &cookie,
        j!({"conditions": [condition()], "pool_id": "main", "limit": 4}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        sample_ids(&answer),
        ["r1-fork", "r2-classifier", "r3-fork", "r8-classifier"]
    );
    assert_eq!(answer["totals"]["sampled"], 4);
}

#[tokio::test]
async fn a_sample_one_short_of_its_limit_answers_its_bodied_rows_in_order() {
    let (parts, cookie) = seeded_sample().await;
    let app = &parts.app;
    let response = post(
        app,
        "/admin/keys",
        r#"{"id":"ci","label":"CI","pool_id":"main"}"#,
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    // The two newest rows leave the sample one short of its limit, and the captured rows after them have no body to
    // read until the last, the first row past a full page.
    let rows = phoebis_admin::SAMPLE_PAGE + 1;
    let id = |n: usize| format!("ci-{n:02}");
    for n in 1..=rows {
        let row = fork_row(&id(n), n as i64, "ci");
        if n <= 2 || n == rows {
            write_row(&parts, row, Some(FORK));
        } else {
            write_row(
                &parts,
                RequestRecord {
                    bodies_captured: true,
                    ..row
                },
                None,
            );
        }
    }
    let (status, answer) = dry_run(
        app,
        &cookie,
        j!({"conditions": [condition()], "key_id": "ci", "limit": 3}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(sample_ids(&answer), [id(1), id(2), id(rows)]);
    assert_eq!(answer["totals"]["sampled"], 3);
}

#[tokio::test]
async fn the_dry_run_reads_full_pages_until_the_last_whatever_room_the_sample_has_left() {
    let (parts, cookie) = seeded_sample().await;
    let app = &parts.app;
    sample_key(app, &cookie, "ci").await;
    // Two full pages and one row past them, with a body on the two newest rows and on the last alone, so the sample
    // sits one short of its limit from the first page on and pages sized to the room left would take one read per
    // bodiless row.
    let rows = 2 * phoebis_admin::SAMPLE_PAGE + 1;
    let id = |n: usize| format!("ci-{n:03}");
    for n in 1..=rows {
        let row = fork_row(&id(n), n as i64, "ci");
        if n <= 2 || n == rows {
            write_row(&parts, row, Some(FORK));
        } else {
            write_row(
                &parts,
                RequestRecord {
                    bodies_captured: true,
                    ..row
                },
                None,
            );
        }
    }
    // The counter counts every `read` on the handle. The writer's own reads run on its connection and count nothing,
    // so the dry run's pages are the only reads between the two counts. The replay's own bound runs on the harness's
    // frozen clock and never expires, so a wall-clock bound turns a writer stuck on a batch into a failure.
    tokio::time::timeout(
        Duration::from_secs(5),
        parts
            .ledger
            .replay_parked(common::NOW, Duration::from_secs(1)),
    )
    .await
    .expect("the ledger writer answered no replay within five seconds")
    .unwrap();
    let before = parts.db.debug_page_reads();
    let (status, answer) = dry_run(
        app,
        &cookie,
        j!({"conditions": [condition()], "key_id": "ci", "limit": 3}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(sample_ids(&answer), [id(1), id(2), id(rows)]);
    assert_eq!(
        parts.db.debug_page_reads() - before,
        3,
        "two full pages and the last"
    );
}

#[tokio::test]
async fn two_rows_at_one_instant_on_a_page_boundary_are_sampled_in_id_order_with_neither_left_out()
{
    let (parts, cookie) = seeded_sample().await;
    let app = &parts.app;
    sample_key(app, &cookie, "ci").await;
    // The first page's last row and the second page's first share their instant, so the second page reads past the
    // cursor by id alone.
    let page = phoebis_admin::SAMPLE_PAGE;
    let rows = page + 1;
    let id = |n: usize| format!("ci-{n:02}");
    for n in 1..=rows {
        write_row(
            &parts,
            fork_row(&id(n), n.min(page) as i64, "ci"),
            Some(FORK),
        );
    }
    let (status, answer) = dry_run(
        app,
        &cookie,
        j!({"conditions": [condition()], "key_id": "ci"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let mut expected: Vec<String> = (1..page).map(id).collect();
    expected.extend([id(rows), id(page)]);
    assert_eq!(
        sample_ids(&answer),
        expected,
        "the higher id first at the shared instant"
    );
}

#[tokio::test]
async fn the_verdicts_carry_the_failing_index_in_the_written_order() {
    let (parts, cookie) = seeded_sample().await;
    // Every condition fails on a classifier call, and the class runs first whatever the order, so the index names the
    // class where a walk in the written order would name the tools.
    let written = j!([
        {"subject": "body", "operator": "present", "path": "tools"},
        {"subject": "message", "operator": "matches", "selector": "last_user", "value": "^Describe your most recent action"},
        {"subject": "request_class", "operator": "equals", "value": "auxiliary"},
    ]);
    let (status, answer) = dry_run(
        &parts.app,
        &cookie,
        j!({"conditions": written, "pool_id": "main"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        verdicts(&answer),
        [
            verdict("r1-fork", "matched", Value::Null),
            verdict("r2-classifier", "unmatched", j!(2)),
            verdict("r3-fork", "matched", Value::Null),
            verdict("r8-classifier", "unmatched", j!(2)),
        ]
    );
    assert_eq!(
        answer["totals"],
        j!({"sampled": 4, "matched": 2, "unmatched": 2, "indeterminate": 0})
    );
}

#[tokio::test]
async fn a_header_condition_on_a_kept_hint_evaluates_and_one_on_another_header_reads_indeterminate_unless_a_later_condition_fails(
) {
    let (parts, cookie) = seeded_sample().await;
    let app = &parts.app;
    let (status, hint) = dry_run(
        app,
        &cookie,
        j!({"conditions": [{"subject": "header", "operator": "present", "name": "x-claude-code-agent-type"}], "pool_id": "main"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{hint}");
    assert_eq!(
        verdicts(&hint),
        [
            verdict("r1-fork", "matched", Value::Null),
            verdict("r2-classifier", "unmatched", j!(0)),
            verdict("r3-fork", "matched", Value::Null),
            verdict("r8-classifier", "unmatched", j!(0)),
        ],
        "the row keeps the agent type, so the header reads from it"
    );

    let (status, unknown) = dry_run(
        app,
        &cookie,
        j!({"conditions": [
            {"subject": "header", "operator": "present", "name": "anthropic-beta"},
            {"subject": "request_class", "operator": "equals", "value": "auxiliary"},
        ], "pool_id": "main"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{unknown}");
    assert_eq!(
        verdicts(&unknown),
        [
            verdict("r1-fork", "indeterminate", j!(0)),
            verdict("r2-classifier", "unmatched", j!(1)),
            verdict("r3-fork", "indeterminate", j!(0)),
            verdict("r8-classifier", "unmatched", j!(1)),
        ],
        "the row keeps no other header, and the class decides where it fails"
    );
    assert_eq!(
        unknown["totals"],
        j!({"sampled": 4, "matched": 0, "unmatched": 2, "indeterminate": 2})
    );

    // One condition per hint the row keeps, each value apart from the others, so a hint read off another column
    // fails its own condition.
    write_row(
        &parts,
        RequestRecord {
            request_class: Some("auxiliary".into()),
            agent_type: Some("general-purpose".into()),
            user_agent: Some("claude-cli/2.1.300 (external, cli)".into()),
            compaction: Some("auto".into()),
            context_compacted: Some("true".into()),
            ..ledger_row("r0-hints", 0, "desktop")
        },
        Some(FORK),
    );
    let (status, hints) = dry_run(
        app,
        &cookie,
        j!({"conditions": [
            {"subject": "header", "operator": "equals", "name": "x-claude-code-request-class", "value": "auxiliary"},
            {"subject": "header", "operator": "equals", "name": "x-claude-code-agent-type", "value": "general-purpose"},
            {"subject": "header", "operator": "equals", "name": "user-agent", "value": "claude-cli/2.1.300 (external, cli)"},
            {"subject": "header", "operator": "equals", "name": "x-claude-code-compaction", "value": "auto"},
            {"subject": "header", "operator": "equals", "name": "x-claude-code-context-compacted", "value": "true"},
        ], "pool_id": "main", "limit": 1}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{hints}");
    assert_eq!(
        verdicts(&hints),
        [verdict("r0-hints", "matched", Value::Null)],
        "each of the five hints reads from its own column"
    );
}

#[tokio::test]
async fn a_saved_guardrail_and_the_same_unsaved_definition_answer_alike() {
    let (parts, cookie) = seeded_sample().await;
    let app = &parts.app;
    created(
        app,
        &cookie,
        "forks",
        j!({"enabled": false, "conditions": fork_conditions()}),
    )
    .await;
    let (status, saved) =
        dry_run(app, &cookie, j!({"guardrail": "forks", "pool_id": "main"})).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let (status, unsaved) = dry_run(
        app,
        &cookie,
        j!({"conditions": fork_conditions(), "pool_id": "main"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{unsaved}");
    assert_eq!(
        saved["totals"]["matched"], 2,
        "a disabled guardrail is evaluated as stored"
    );
    assert_eq!(saved["requests"], unsaved["requests"]);
    assert_eq!(saved["totals"], unsaved["totals"]);
    assert_eq!(saved["sample"], unsaved["sample"]);
}

#[tokio::test]
async fn an_unknown_guardrail_pool_or_key_is_a_404() {
    let (parts, cookie) = seeded_sample().await;
    for (body, expected) in [
        (
            j!({"guardrail": "missing", "pool_id": "main"}),
            "no such guardrail",
        ),
        (
            j!({"conditions": [condition()], "pool_id": "missing"}),
            "no such pool",
        ),
        (
            j!({"conditions": [condition()], "key_id": "missing"}),
            "no such key",
        ),
    ] {
        let response = post(
            &parts.app,
            "/admin/guardrails/test",
            &body.to_string(),
            Some(&cookie),
        )
        .await;
        assert_eq!(
            refusal(response).await,
            (StatusCode::NOT_FOUND, expected.to_string()),
            "{body}"
        );
    }
}

#[tokio::test]
async fn the_exclusive_members_and_the_limit_are_refused() {
    let (parts, cookie) = seeded_sample().await;
    let app = &parts.app;
    for (body, expected) in [
        (
            j!({"pool_id": "main"}),
            "guardrail or conditions is required",
        ),
        (
            j!({"guardrail": "forks", "conditions": [condition()], "pool_id": "main"}),
            "guardrail and conditions are exclusive",
        ),
        (
            j!({"conditions": [condition()]}),
            "pool_id or key_id is required",
        ),
        (
            j!({"conditions": [condition()], "pool_id": "main", "key_id": "desktop"}),
            "pool_id and key_id are exclusive",
        ),
        (
            j!({"conditions": [condition()], "pool_id": "main", "limit": 0}),
            "limit is 1 to 500",
        ),
        (
            j!({"conditions": [condition()], "pool_id": "main", "limit": 501}),
            "limit is 1 to 500",
        ),
    ] {
        let response = post(
            app,
            "/admin/guardrails/test",
            &body.to_string(),
            Some(&cookie),
        )
        .await;
        assert_eq!(
            refusal(response).await,
            (StatusCode::BAD_REQUEST, expected.to_string()),
            "{body}"
        );
    }
    for limit in [1, 500] {
        let (status, answer) = dry_run(
            app,
            &cookie,
            j!({"conditions": [condition()], "pool_id": "main", "limit": limit}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "limit {limit}: {answer}");
        assert_eq!(answer["sample"]["limit"], limit);
    }
}

#[tokio::test]
async fn an_invalid_unsaved_condition_reads_its_index() {
    let (parts, cookie) = seeded_sample().await;
    let body =
        j!({"conditions": [{"subject": "request_class", "operator": "equals"}], "pool_id": "main"});
    let response = post(
        &parts.app,
        "/admin/guardrails/test",
        &body.to_string(),
        Some(&cookie),
    )
    .await;
    assert_eq!(
        refusal(response).await,
        (
            StatusCode::BAD_REQUEST,
            "conditions[0]: value is required".to_string()
        )
    );
}

#[tokio::test]
async fn the_mutation_log_writes_one_line_for_a_dry_run() {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(captured.clone())
        .finish();
    // The harness drives every request through `oneshot` on this test's own task, so a thread-local subscriber
    // sees every line the gate writes.
    let _guard = tracing::subscriber::set_default(subscriber);
    let (parts, cookie) = seeded_sample().await;
    // The other cases of this file write mutation lines on threads with no subscriber, and one of them can register
    // the line's callsite as unwanted while this case's subscriber is being set. The seeding registered it by now,
    // so a rebuild reads every subscriber's interest again.
    tracing::callsite::rebuild_interest_cache();
    let app = &parts.app;
    let session = show(app, &cookie, "/admin/session").await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, _) = dry_run(
        app,
        &cookie,
        j!({"conditions": [condition()], "pool_id": "main"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    const MARK: &str = " INFO phoebis_admin::routes: admin mutation ";
    let lines: Vec<String> = captured
        .text()
        .lines()
        .filter_map(|line| line.split_once(MARK).map(|(_, fields)| fields.to_string()))
        .filter(|fields| fields.contains("route=/admin/guardrails/test"))
        .collect();
    assert_eq!(
        lines,
        [format!(
            "method=POST route=/admin/guardrails/test status=200 entity=guardrail id=- credential=session session={session}"
        )]
    );
}

#[tokio::test]
async fn the_answer_carries_no_body_text() {
    let (parts, cookie) = seeded_sample().await;
    let (status, answer) = dry_run(
        &parts.app,
        &cookie,
        j!({"conditions": [condition()], "pool_id": "main"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["totals"]["sampled"], 4, "the bodies were read");
    let text = answer.to_string();
    for marker in [
        "Describe your most recent action",
        "Classify the command",
        "notes.txt",
        "tool_one",
    ] {
        assert!(
            !text.contains(marker),
            "the answer carries {marker:?} from a body: {text}"
        );
    }
    for request in answer["requests"].as_array().unwrap() {
        let mut keys: Vec<&str> = request
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["condition", "id", "received_at", "verdict"],
            "ids and instants alone"
        );
    }
}
