//! The admin listener's OpenAPI document: served without the gate, complete over the route table, one tag per
//! operation, the bearer scheme on every operation but the login and the logout, every refusal an `ErrorBody`, and
//! the capture's answers as the examples.
mod common;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use common::{admin_app_reloading, admin_parts_full, get, json, login, post};
use http_body_util::BodyExt;
use phoebis_admin::{Reloaded, TlsReload};
use phoebis_ledger::{BodyRecord, RequestRecord};
use serde_json::Value;
use tower::ServiceExt;

const DOCUMENT_PATH: &str = "/api-reference/openapi.json";

/// The route table's source, read as text so a route added as a plain `.route` cannot escape the document.
const ROUTES_RS: &str = include_str!("../src/routes.rs");

/// The two catch-alls, the paths a plain `.route(` registers, which the introduction describes.
const PLAIN_ROUTES: &[&str] = &["/admin", "/admin/{*rest}"];

/// The eighteen tags in the order the document declares them, the API page's area order.
const TAGS: [&str; 18] = [
    "Accounts",
    "Analytics",
    "Attention",
    "Dashboard",
    "Guardrails",
    "Keepalive",
    "Keys",
    "Ledger",
    "Models",
    "Pairing",
    "Pools",
    "Pricing",
    "Requests",
    "Routing log",
    "Session",
    "Sessions",
    "Settings",
    "System",
];

/// Each operation's refusal statuses, as its Refuses line names them, with the shared `500` and `503` where the code
/// reaches them, and any other status the code reaches. One row per operation, so an operation the document gains
/// without a row fails the test.
const REFUSALS: &[(&str, &[u16])] = &[
    ("session_show", &[]),
    ("session_login", &[400, 401, 429]),
    ("session_logout", &[500]),
    ("accounts_list", &[500]),
    ("accounts_create", &[400, 404, 409, 500, 502, 503]),
    ("account_show", &[404, 500]),
    ("account_patch", &[400, 404, 409, 500, 502, 503]),
    ("account_delete", &[404, 500, 503]),
    ("account_catalog", &[404, 500]),
    ("account_clear_funding", &[404, 409, 500, 503]),
    ("account_cooldown_delete", &[400, 404, 500, 503]),
    ("account_credentials_delete", &[404, 500, 503]),
    ("account_pause", &[404, 500, 503]),
    ("account_quota", &[400, 404, 500]),
    ("account_resume", &[404, 500, 503]),
    ("account_usage", &[404, 500]),
    ("account_usage_refresh", &[404, 500, 503]),
    ("account_verify", &[400, 404, 409, 500, 502]),
    ("analytics_accounts", &[400, 500]),
    ("analytics_breakdown", &[400, 500]),
    ("analytics_budgets", &[503]),
    ("analytics_burn", &[400, 500]),
    ("analytics_guardrails", &[400, 500]),
    ("analytics_placements", &[400, 500]),
    ("analytics_reconciliations", &[500]),
    ("analytics_rewrites", &[400, 500]),
    ("analytics_series", &[400, 500]),
    ("attention_show", &[500]),
    ("dashboard_show", &[500]),
    ("guardrails_list", &[500]),
    ("guardrails_create", &[400, 404, 409, 500, 503]),
    ("guardrail_presets", &[]),
    ("guardrails_test", &[400, 404, 500]),
    ("guardrail_show", &[404, 500]),
    ("guardrail_patch", &[400, 404, 409, 500, 503]),
    ("guardrail_delete", &[404, 500, 503]),
    ("keepalive_overview", &[500]),
    ("keepalive_recipes", &[]),
    ("keys_list", &[500]),
    ("keys_create", &[400, 404, 409, 500, 503]),
    ("key_patch", &[400, 404, 409, 500, 503]),
    ("key_delete", &[404, 500, 503]),
    ("key_models", &[404, 500, 503]),
    ("key_revoke", &[404, 500, 503]),
    ("key_rotate", &[404, 409, 500, 503]),
    ("ledger_replay_parked", &[500, 503]),
    ("models_state", &[500, 503]),
    ("pairing_start", &[400, 404, 409, 500]),
    ("pairing_show", &[404]),
    ("pairing_cancel", &[404, 409]),
    ("pairing_callback", &[400, 404, 409, 502]),
    ("pairing_confirm", &[404, 409, 500, 503]),
    ("pools_list", &[500]),
    ("pools_create", &[400, 404, 409, 500, 503]),
    ("pool_patch", &[400, 404, 409, 500, 503]),
    ("pool_delete", &[404, 409, 500, 503]),
    ("pool_members_put", &[400, 404, 409, 500, 503]),
    ("pool_traffic", &[400, 404, 500]),
    ("pricing_show", &[]),
    ("requests_list", &[400, 500]),
    ("request_show", &[404, 500]),
    ("request_bodies", &[404, 500]),
    ("request_decision", &[400, 404, 500]),
    ("routing_log_list", &[400, 500]),
    ("sessions_list", &[500, 503]),
    ("session_release", &[404, 503]),
    ("sessions_costs", &[]),
    ("settings_show", &[]),
    ("system_show", &[]),
    ("tls_reload", &[500]),
];

/// The fixture stems the document carries as examples, each file's `body` member on its operation's answer.
const EXAMPLE_FILES: &[&str] = &[
    "session",
    "accounts",
    "account",
    "account_catalog",
    "account_quota",
    "account_usage",
    "analytics_accounts",
    "analytics_breakdown",
    "analytics_breakdown_accounts",
    "analytics_breakdown_speed",
    "analytics_budgets",
    "analytics_burn",
    "analytics_guardrails",
    "analytics_placements",
    "analytics_reconciliations",
    "analytics_rewrites",
    "analytics_series",
    "analytics_series_accounts",
    "attention",
    "dashboard",
    "guardrails",
    "guardrail_created",
    "guardrail_presets",
    "guardrail_show",
    "guardrail_test",
    "keepalive_overview",
    "keepalive_recipes",
    "keys",
    "key_created",
    "key_models",
    "replay",
    "replay_held",
    "models_state",
    "pairing_started",
    "oauth_session",
    "pools",
    "pool_traffic",
    "pricing",
    "requests",
    "request",
    "request_bodies",
    "decision",
    "routing_log",
    "sessions",
    "sessions_costs",
    "settings",
    "system",
];

/// Every file under `web/test/fixtures/` and its `routing/` directory by stem, with the operation and the status whose
/// answer schema the file's `body` member validates against. A file the table does not name fails the corpus case,
/// so a new fixture adds its row.
const CORPUS: &[(&str, Option<(&str, u16)>)] = &[
    ("account", Some(("account_show", 200))),
    ("account_catalog", Some(("account_catalog", 200))),
    ("account_quota", Some(("account_quota", 200))),
    ("account_usage", Some(("account_usage", 200))),
    ("accounts", Some(("accounts_list", 200))),
    ("analytics_accounts", Some(("analytics_accounts", 200))),
    ("analytics_breakdown", Some(("analytics_breakdown", 200))),
    (
        "analytics_breakdown_accounts",
        Some(("analytics_breakdown", 200)),
    ),
    (
        "analytics_breakdown_speed",
        Some(("analytics_breakdown", 200)),
    ),
    ("analytics_budgets", Some(("analytics_budgets", 200))),
    ("analytics_burn", Some(("analytics_burn", 200))),
    ("analytics_guardrails", Some(("analytics_guardrails", 200))),
    ("analytics_placements", Some(("analytics_placements", 200))),
    (
        "analytics_reconciliations",
        Some(("analytics_reconciliations", 200)),
    ),
    ("analytics_rewrites", Some(("analytics_rewrites", 200))),
    ("analytics_series", Some(("analytics_series", 200))),
    ("analytics_series_accounts", Some(("analytics_series", 200))),
    ("attention", Some(("attention_show", 200))),
    ("dashboard", Some(("dashboard_show", 200))),
    ("decision", Some(("request_decision", 200))),
    ("guardrail_created", Some(("guardrails_create", 201))),
    ("guardrail_presets", Some(("guardrail_presets", 200))),
    ("guardrail_show", Some(("guardrail_show", 200))),
    ("guardrail_test", Some(("guardrails_test", 200))),
    ("guardrails", Some(("guardrails_list", 200))),
    ("keepalive_overview", Some(("keepalive_overview", 200))),
    ("keepalive_recipes", Some(("keepalive_recipes", 200))),
    ("key_created", Some(("keys_create", 201))),
    ("key_models", Some(("key_models", 200))),
    ("keys", Some(("keys_list", 200))),
    ("models_state", Some(("models_state", 200))),
    ("oauth_session", Some(("pairing_show", 200))),
    ("pairing_started", Some(("pairing_start", 201))),
    ("pool_traffic", Some(("pool_traffic", 200))),
    ("pools", Some(("pools_list", 200))),
    ("pricing", Some(("pricing_show", 200))),
    ("replay", Some(("ledger_replay_parked", 200))),
    ("replay_held", Some(("ledger_replay_parked", 503))),
    ("request", Some(("request_show", 200))),
    ("request_bodies", Some(("request_bodies", 200))),
    ("requests", Some(("requests_list", 200))),
    ("routing_log", Some(("routing_log_list", 200))),
    ("session", Some(("session_show", 200))),
    ("sessions", Some(("sessions_list", 200))),
    ("sessions_costs", Some(("sessions_costs", 200))),
    ("settings", Some(("settings_show", 200))),
    ("system", Some(("system_show", 200))),
    // No route's answer: the bound probes' inputs and statuses, a test's own tables and the routing walk's steps.
    ("bound_refusals", None),
    ("test_tables", None),
    ("walk_steps", None),
    ("walk_steps_v1", None),
];

/// Every handler module that reads a query map, with its source, for the check that the names it reads are the
/// names its parameter types declare.
const QUERY_MODULES: &[(&str, &str)] = &[
    ("accounts", include_str!("../src/accounts.rs")),
    ("analytics", include_str!("../src/analytics.rs")),
    ("pools", include_str!("../src/pools.rs")),
    ("requests", include_str!("../src/requests.rs")),
    ("routing_log", include_str!("../src/routing_log.rs")),
];

/// Every handler module, so a module that reads a query and is missing from the check is found.
const HANDLER_MODULES: &[(&str, &str)] = &[
    ("accounts", include_str!("../src/accounts.rs")),
    ("analytics", include_str!("../src/analytics.rs")),
    ("attention", include_str!("../src/attention.rs")),
    ("dashboard", include_str!("../src/dashboard.rs")),
    ("guardrails", include_str!("../src/guardrails.rs")),
    ("keepalive", include_str!("../src/keepalive.rs")),
    ("keys", include_str!("../src/keys.rs")),
    ("ledger", include_str!("../src/ledger.rs")),
    ("models", include_str!("../src/models.rs")),
    ("pools", include_str!("../src/pools.rs")),
    ("pricing", include_str!("../src/pricing.rs")),
    ("requests", include_str!("../src/requests.rs")),
    ("routes", include_str!("../src/routes.rs")),
    ("routing_log", include_str!("../src/routing_log.rs")),
    ("sessions", include_str!("../src/sessions.rs")),
    ("settings", include_str!("../src/settings.rs")),
    ("tls", include_str!("../src/tls.rs")),
];

fn fixtures_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../web/test/fixtures")
}

/// The app and the parsed document. The fake upstream rides along so the app outlives the test's requests.
async fn document() -> (axum::Router, fake_upstream::FakeUpstream, Value) {
    let (app, up) = common::admin_app().await;
    let response = get(&app, DOCUMENT_PATH, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let document = json(response).await;
    (app, up, document)
}

/// Whether a captured path is an instance of a document path: segment by segment, a `{...}` segment taking any one.
fn matches(pattern: &str, path: &str) -> bool {
    let (mut p, mut q) = (pattern.split('/'), path.split('/'));
    loop {
        match (p.next(), q.next()) {
            (None, None) => return true,
            (Some(a), Some(b))
                if a == b || (a.starts_with('{') && a.ends_with('}') && !b.is_empty()) => {}
            _ => return false,
        }
    }
}

/// Every operation of the document as (path, method, operation).
fn operations(document: &Value) -> Vec<(String, String, &Value)> {
    let mut out = Vec::new();
    for (path, item) in document["paths"].as_object().expect("paths") {
        for (method, operation) in item.as_object().expect("a path item") {
            if ["get", "post", "put", "patch", "delete"].contains(&method.as_str()) {
                out.push((path.clone(), method.clone(), operation));
            }
        }
    }
    out
}

/// The operation the fixture's `route` member names and the status its body is the example of: the `503` for the
/// replay's held answer, the one `2xx` otherwise.
fn target<'a>(document: &'a Value, stem: &str) -> (&'a Value, String) {
    let file: Value = serde_json::from_str(
        &std::fs::read_to_string(fixtures_dir().join(format!("{stem}.json"))).unwrap(),
    )
    .unwrap();
    let route = file["route"]
        .as_str()
        .unwrap_or_else(|| panic!("{stem}.json names no route"));
    let (method, path) = route.split_once(' ').unwrap();
    let path = path.split('?').next().unwrap();
    let (_, _, operation) = operations(document)
        .into_iter()
        .find(|(pattern, m, _)| m.eq_ignore_ascii_case(method) && matches(pattern, path))
        .unwrap_or_else(|| panic!("{stem}.json: no operation at {route}"));
    let responses = operation["responses"].as_object().unwrap();
    let status = if stem == "replay_held" {
        "503".to_string()
    } else {
        responses
            .keys()
            .find(|s| s.starts_with('2'))
            .expect("a success response")
            .clone()
    };
    (operation, status)
}

/// The example the fixture is, whether the response's one `example` or its named one.
fn example_of<'a>(operation: &'a Value, status: &str, stem: &str) -> &'a Value {
    let content = &operation["responses"][status]["content"]["application/json"];
    if content["example"].is_object() {
        &content["example"]
    } else {
        &content["examples"][stem]["value"]
    }
}

/// The first error of `body` against the answer schema of the operation at `status`, its instance path first. The
/// schema compiles inside a root that carries the document's `components`, so every `$ref` the document writes
/// resolves in place.
fn schema_error(document: &Value, operation_id: &str, status: u16, body: &Value) -> Option<String> {
    let (_, _, operation) = operations(document)
        .into_iter()
        .find(|(_, _, operation)| operation["operationId"] == operation_id)
        .unwrap_or_else(|| panic!("no operation is {operation_id}"));
    let schema =
        &operation["responses"][status.to_string()]["content"]["application/json"]["schema"];
    assert!(
        schema.is_object(),
        "{operation_id} {status} declares no JSON schema"
    );
    let root = serde_json::json!({
        "components": document["components"],
        "$defs": { "answer": schema },
        "$ref": "#/$defs/answer",
    });
    let validator = jsonschema::validator_for(&root)
        .unwrap_or_else(|e| panic!("{operation_id} {status}: the schema does not compile: {e}"));
    validator
        .validate(body)
        .err()
        .map(|e| format!("at {:?}: {e}", e.instance_path().as_str()))
}

/// The names a handler module reads off its query map, as the literal of each read. A helper handed the map takes it
/// by reference in a handler (`list_param(&q, ..)`) and by name inside a function that holds the reference already
/// (`list(q, ..)`, `analytics.rs`).
fn reader_names(source: &str) -> BTreeSet<String> {
    const READERS: &[&str] = &[
        "q.get(\"",
        "q.contains_key(\"",
        "list(&q, \"",
        "list(q, \"",
        "agent_list(&q, \"",
        "agent_list(q, \"",
        "flag(&q, \"",
        "flag(q, \"",
        "list_param(&q, \"",
        "time(&q, \"",
        "number(\"",
    ];
    // rustfmt splits a long read into a chain (`q` then `.get("include_probes")` on the next line, `requests.rs`), so
    // every run of whitespace collapses to one space and a space before a `.` goes, which rejoins the call.
    let source = source
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace(" .", ".");
    let mut names = BTreeSet::new();
    for reader in READERS {
        let mut rest = source.as_str();
        while let Some(at) = rest.find(reader) {
            let after = &rest[at + reader.len()..];
            let end = after.find('"').expect("a closed literal");
            names.insert(after[..end].to_string());
            rest = &after[end..];
        }
    }
    names
}

/// The field names of every struct in the module that derives `IntoParams`: the names the document declares.
fn declared_names(source: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut lines = source.lines();
    while let Some(line) = lines.next() {
        if !(line.contains("#[derive(") && line.contains("IntoParams")) {
            continue;
        }
        for line in lines.by_ref() {
            if line.trim_start().starts_with("pub(crate) struct")
                || line.trim_start().starts_with("struct")
            {
                break;
            }
        }
        for line in lines.by_ref() {
            let field = line.trim();
            if field == "}" {
                break;
            }
            if field.starts_with("///") || field.starts_with("#[") {
                continue;
            }
            if let Some((name, _)) = field.split_once(':') {
                names.insert(
                    name.trim_start_matches("pub(crate) ")
                        .trim_start_matches("pub ")
                        .to_string(),
                );
            }
        }
    }
    names
}

#[tokio::test]
async fn the_document_is_served_without_the_gate_as_no_store_json() {
    let (app, _up, document) = document().await;
    let response = get(&app, DOCUMENT_PATH, None).await;
    assert_eq!(response.headers()["content-type"], "application/json");
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert!(response
        .headers()
        .get("access-control-allow-origin")
        .is_none());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    println!("the document is {} bytes", bytes.len());
    // The reason for an `#[allow(dead_code)]` stays a line comment, since the derive copies a doc comment into the
    // description a reader sees.
    assert!(
        !std::str::from_utf8(&bytes)
            .unwrap()
            .contains("Declared for the document alone"),
        "the dead-code allowance's reason stays out of the document"
    );
    assert_eq!(document["openapi"], "3.1.0");
    assert_eq!(document["info"]["title"], "Phoebis Router admin API");
    assert_eq!(document["info"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(document["info"]["description"]
        .as_str()
        .unwrap()
        .starts_with("Every operation here lives under"));
    assert!(
        document["info"].get("license").is_none(),
        "the manifest's license stays out of the info block"
    );
    assert!(
        document["info"].get("contact").is_none(),
        "the info block names no contact"
    );
    assert_eq!(
        document["servers"],
        serde_json::json!([{ "url": "/", "description": "This router's admin listener" }])
    );
}

#[tokio::test]
async fn a_wrong_bearer_on_the_document_answers_200_and_never_counts_toward_the_throttle() {
    let (app, _up, _document) = document().await;
    for _ in 0..5 {
        let request = Request::builder()
            .method(Method::GET)
            .uri(DOCUMENT_PATH)
            .header("authorization", "Bearer not-the-secret")
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "the gate never sees the document's route"
        );
    }
    let login = post(&app, "/admin/session", r#"{"secret":"admin-secret"}"#, None).await;
    assert_eq!(
        login.status(),
        StatusCode::NO_CONTENT,
        "five wrong bearers on the document throttled nothing"
    );
}

#[tokio::test]
async fn the_document_route_answers_head_with_the_headers_alone_and_post_with_405() {
    let (app, _up, _document) = document().await;
    let head = Request::builder()
        .method(Method::HEAD)
        .uri(DOCUMENT_PATH)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(head).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "application/json");
    assert!(response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .is_empty());
    let response = post(&app, DOCUMENT_PATH, "{}", None).await;
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert!(response.headers()["allow"]
        .to_str()
        .unwrap()
        .contains("GET"));
}

#[tokio::test]
async fn every_route_of_the_table_is_an_operation_and_the_plain_routes_are_the_listed_ones() {
    let (_app, _up, document) = document().await;
    let mut plain = BTreeSet::new();
    let mut rest = ROUTES_RS;
    while let Some(at) = rest.find(".route(") {
        let after = &rest[at + ".route(".len()..];
        let literal = after.trim_start();
        if let Some(path) = literal.strip_prefix('"') {
            plain.insert(path[..path.find('"').unwrap()].to_string());
        }
        rest = after;
    }
    let listed: BTreeSet<String> = PLAIN_ROUTES.iter().map(|p| p.to_string()).collect();
    assert_eq!(
        plain, listed,
        "a route registered with a plain .route is outside the document"
    );
    let registrations = ROUTES_RS.matches("routes!(").count();
    assert_eq!(
        registrations,
        document["paths"].as_object().unwrap().len(),
        "one routes! call per document path"
    );
    let handlers: usize = ROUTES_RS
        .split("routes!(")
        .skip(1)
        .map(|call| {
            call[..call.find(')').unwrap()]
                .split(',')
                .filter(|h| !h.trim().is_empty())
                .count()
        })
        .sum();
    assert_eq!(
        handlers,
        operations(&document).len(),
        "one handler per operation"
    );
    assert_eq!(
        document["paths"].as_object().unwrap().len(),
        57,
        "the admin listener's 57 paths"
    );
    assert_eq!(
        operations(&document).len(),
        70,
        "the admin listener's 70 operations"
    );
}

#[tokio::test]
async fn every_operation_has_a_unique_id_and_exactly_one_of_the_tags_in_order() {
    let (_app, _up, document) = document().await;
    let declared: Vec<&str> = document["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(declared, TAGS);
    let mut ids = BTreeSet::new();
    for (path, method, operation) in operations(&document) {
        let id = operation["operationId"]
            .as_str()
            .unwrap_or_else(|| panic!("{method} {path} has no id"));
        assert!(ids.insert(id.to_string()), "{id} is declared twice");
        let tags = operation["tags"]
            .as_array()
            .unwrap_or_else(|| panic!("{id} has no tag"));
        assert_eq!(tags.len(), 1, "{id} carries one tag");
        assert!(
            TAGS.contains(&tags[0].as_str().unwrap()),
            "{id}: {}",
            tags[0]
        );
    }
}

#[tokio::test]
async fn every_operation_requires_the_bearer_except_the_login_and_the_logout() {
    let (_app, _up, document) = document().await;
    let scheme = &document["components"]["securitySchemes"]["bearer"];
    assert_eq!(scheme["type"], "http");
    assert_eq!(scheme["scheme"], "bearer");
    assert_eq!(document["security"], serde_json::json!([{ "bearer": [] }]));
    for (_, _, operation) in operations(&document) {
        let id = operation["operationId"].as_str().unwrap();
        match id {
            "session_login" | "session_logout" => {
                assert_eq!(
                    operation["security"],
                    serde_json::json!([]),
                    "{id} opts out of the bearer"
                )
            }
            _ => assert!(
                operation["security"].is_null(),
                "{id} inherits the document's bearer requirement"
            ),
        }
    }
}

#[tokio::test]
async fn every_operation_declares_one_success_and_the_refusals_of_its_row_as_error_bodies() {
    let (_app, _up, document) = document().await;
    let rows: BTreeMap<&str, &[u16]> = REFUSALS.iter().copied().collect();
    for (path, method, operation) in operations(&document) {
        let id = operation["operationId"].as_str().unwrap();
        let responses = operation["responses"].as_object().unwrap();
        let successes: Vec<&String> = responses.keys().filter(|s| s.starts_with('2')).collect();
        assert_eq!(
            successes.len(),
            1,
            "{id} declares one success: {successes:?}"
        );
        let refusals: BTreeSet<u16> = responses
            .keys()
            .filter(|s| !s.starts_with('2'))
            .map(|s| s.parse().unwrap())
            .collect();
        let expected: BTreeSet<u16> = rows
            .get(id)
            .unwrap_or_else(|| panic!("{id} ({method} {path}) has no REFUSALS row"))
            .iter()
            .copied()
            .collect();
        assert_eq!(refusals, expected, "{id}: the refusals the API page lists");
        for (status, response) in responses.iter().filter(|(s, _)| !s.starts_with('2')) {
            let shared = response["$ref"]
                .as_str()
                .map(|r| r.starts_with("#/components/responses/"))
                .unwrap_or(false);
            let inline = response["content"]["application/json"]["schema"]["$ref"]
                == "#/components/schemas/ErrorBody";
            assert!(shared || inline, "{id} {status}: the body is an ErrorBody");
            if status == "409" {
                let stale_edit = [
                    "account_patch",
                    "guardrail_patch",
                    "key_patch",
                    "pool_patch",
                ]
                .contains(&id);
                assert_eq!(
                    response["headers"]["x-phoebis-error-code"].is_object(),
                    stale_edit,
                    "{id} 409: the code header on the four stale_edit sites alone"
                );
            }
        }
    }
    assert_eq!(
        rows.len(),
        operations(&document).len(),
        "REFUSALS has one row per operation and no more"
    );
    let registry = &document["components"]["responses"]["RegistryUnavailable"];
    assert!(registry["headers"]["x-phoebis-error-code"].is_object());
    for name in [
        "StorageFailure",
        "LedgerFailure",
        "RouterUnavailable",
        "RegistryUnavailable",
    ] {
        let component = &document["components"]["responses"][name];
        assert_eq!(
            component["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/ErrorBody",
            "{name}"
        );
    }
}

#[tokio::test]
async fn every_example_is_its_fixtures_body_member_and_the_count_holds() {
    let (_app, _up, document) = document().await;
    let mut on_disk: Vec<String> = std::fs::read_dir(fixtures_dir())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .map(|e| e.path().file_stem().unwrap().to_string_lossy().into_owned())
        .filter(|stem| stem != "bound_refusals" && stem != "test_tables")
        .collect();
    on_disk.sort();
    let mut listed: Vec<String> = EXAMPLE_FILES.iter().map(|s| s.to_string()).collect();
    listed.sort();
    assert_eq!(
        listed, on_disk,
        "every route fixture on disk is an example, and no other file is"
    );
    assert_eq!(
        EXAMPLE_FILES.len(),
        47,
        "46 success answers and the replay's held answer"
    );
    // The places the fixture manifest targets, each with the stems it attaches: a place carries as many examples
    // as stems, so a fixture attached twice or to the wrong place fails here, while a hand-written example (a
    // login refusal, the TLS reload's success) sits at a place no stem targets and is held by the next case.
    let mut places: BTreeMap<(String, String), Vec<&str>> = BTreeMap::new();
    for stem in EXAMPLE_FILES {
        let (operation, status) = target(&document, stem);
        places
            .entry((
                operation["operationId"].as_str().unwrap().to_string(),
                status.to_string(),
            ))
            .or_default()
            .push(stem);
    }
    let mut attached = 0;
    for (_, _, operation) in operations(&document) {
        let id = operation["operationId"].as_str().unwrap();
        for (status, response) in operation["responses"].as_object().unwrap() {
            let Some(stems) = places.get(&(id.to_string(), status.clone())) else {
                continue;
            };
            let content = &response["content"]["application/json"];
            let count = content["example"].is_object() as usize
                + content["examples"]
                    .as_object()
                    .map(|m| m.len())
                    .unwrap_or(0);
            assert_eq!(
                count,
                stems.len(),
                "{id} {status}: one example per fixture {stems:?}"
            );
            attached += count;
        }
    }
    assert_eq!(
        attached,
        EXAMPLE_FILES.len(),
        "every listed fixture is attached once"
    );
    for stem in EXAMPLE_FILES {
        let file: Value = serde_json::from_str(
            &std::fs::read_to_string(fixtures_dir().join(format!("{stem}.json"))).unwrap(),
        )
        .unwrap();
        let (operation, status) = target(&document, stem);
        let example = example_of(operation, &status, stem);
        assert_eq!(
            example, &file["body"],
            "{stem}.json is the example as captured"
        );
        assert!(
            example.get("route").is_none(),
            "{stem}.json: the wrapper is not the example"
        );
    }
}

#[tokio::test]
async fn every_hand_written_example_parses_and_fits_its_schema() {
    let (_app, _up, document) = document().await;
    let mut targeted: BTreeSet<(String, String)> = BTreeSet::new();
    for stem in EXAMPLE_FILES {
        let (operation, status) = target(&document, stem);
        targeted.insert((
            operation["operationId"].as_str().unwrap().to_string(),
            status.to_string(),
        ));
    }
    let mut seen = 0;
    for (_, _, operation) in operations(&document) {
        let id = operation["operationId"].as_str().unwrap();
        for (status, response) in operation["responses"].as_object().unwrap() {
            if targeted.contains(&(id.to_string(), status.clone())) {
                continue;
            }
            let content = &response["content"]["application/json"];
            let examples: Vec<&Value> = content["example"]
                .as_object()
                .map(|_| &content["example"])
                .into_iter()
                .chain(
                    content["examples"]
                        .as_object()
                        .into_iter()
                        .flat_map(|m| m.values().map(|e| &e["value"])),
                )
                .collect();
            let mut schema = &content["schema"];
            if let Some(reference) = schema["$ref"].as_str() {
                schema = &document["components"]["schemas"][reference.rsplit('/').next().unwrap()];
            }
            for example in examples {
                let object = example
                    .as_object()
                    .unwrap_or_else(|| panic!("{id} {status}: the example is a JSON object"));
                let keys: BTreeSet<&str> = object.keys().map(String::as_str).collect();
                let properties: BTreeSet<&str> = schema["properties"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect();
                assert!(
                    keys.is_subset(&properties),
                    "{id} {status}: every key of the example is a property of its schema"
                );
                for required in schema["required"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or(&[])
                {
                    assert!(
                        keys.contains(required.as_str().unwrap()),
                        "{id} {status}: the example carries the required {required}"
                    );
                }
                seen += 1;
            }
        }
    }
    assert!(
        seen >= 3,
        "the three login refusals at least carry a hand-written example, {seen} found"
    );
}

#[tokio::test]
async fn every_fixture_validates_against_the_answer_schema_its_table_row_names() {
    let (_app, _up, document) = document().await;
    let table: BTreeMap<&str, Option<(&str, u16)>> = CORPUS.iter().copied().collect();
    let mut files = Vec::new();
    for dir in [fixtures_dir(), fixtures_dir().join("routing")] {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|x| x == "json") {
                files.push(path);
            }
        }
    }
    files.sort();
    let mut seen = BTreeSet::new();
    for path in &files {
        let stem = path.file_stem().unwrap().to_str().unwrap();
        seen.insert(stem);
        let row = table
            .get(stem)
            .unwrap_or_else(|| panic!("add {stem} to the corpus table"));
        let Some((operation_id, status)) = *row else {
            continue;
        };
        let file: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let route = file["route"]
            .as_str()
            .unwrap_or_else(|| panic!("{stem}.json names no route"));
        let (method, at) = route.split_once(' ').unwrap();
        let at = at.split('?').next().unwrap();
        // A literal segment wins over a parameter, as the router matches, so `/guardrails/presets` reads its own
        // operation rather than the one at `/guardrails/{id}`.
        let served = operations(&document)
            .into_iter()
            .filter(|(pattern, m, _)| m.eq_ignore_ascii_case(method) && matches(pattern, at))
            .min_by_key(|(pattern, _, _)| pattern.chars().filter(|c| *c == '{').count())
            .map(|(_, _, operation)| operation["operationId"].clone());
        assert_eq!(
            served,
            Some(Value::from(operation_id)),
            "{stem}.json: its row names the operation that serves its route, {route}"
        );
        if let Some(error) = schema_error(&document, operation_id, status, &file["body"]) {
            panic!("{stem}.json does not fit {operation_id} {status} {error}");
        }
    }
    let rows: BTreeSet<&str> = table.keys().copied().collect();
    assert_eq!(rows, seen, "every row of the corpus table names a file");

    // A captured body with one member renamed must fail, which proves the root reaches the schema it names rather
    // than accepting every body.
    let file: Value = serde_json::from_str(
        &std::fs::read_to_string(fixtures_dir().join("key_models.json")).unwrap(),
    )
    .unwrap();
    let mut renamed = file["body"].clone();
    let rule = renamed["rules"][1]
        .as_object_mut()
        .expect("key_models.json carries a second rule");
    let pattern = rule.remove("pattern").unwrap();
    rule.insert("glob".to_string(), pattern);
    let error = schema_error(&document, "key_models", 200, &renamed)
        .expect("a body with a renamed member fails its schema");
    assert!(error.starts_with("at \"/rules/1\""), "{error}");
}

#[tokio::test]
async fn every_examples_key_order_is_its_schemas_property_order() {
    let (_app, _up, document) = document().await;
    for stem in EXAMPLE_FILES {
        let (operation, status) = target(&document, stem);
        if !status.starts_with('2') {
            continue;
        }
        let example = example_of(operation, &status, stem);
        let mut schema = &operation["responses"][&status]["content"]["application/json"]["schema"];
        if let Some(reference) = schema["$ref"].as_str() {
            let name = reference.rsplit('/').next().unwrap();
            schema = &document["components"]["schemas"][name];
        }
        let keys: Vec<&String> = example.as_object().unwrap().keys().collect();
        // An optional key the capture left out (`counts` outside a `request` filter) is no order fault, so the
        // schema's properties are read down to the keys the example carries, and the required ones held apart.
        let properties: Vec<&String> = schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .filter(|p| keys.contains(p))
            .collect();
        assert_eq!(
            keys, properties,
            "{stem}.json: the schema lists the example's keys in the wire's order"
        );
        for required in schema["required"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            assert!(
                keys.iter()
                    .any(|k| k.as_str() == required.as_str().unwrap()),
                "{stem}.json: the required {required} is present"
            );
        }
    }
}

#[tokio::test]
async fn a_model_rejection_and_a_model_permit_require_every_key_they_carry() {
    let (_app, _up, document) = document().await;
    for name in ["Permit", "Rejection"] {
        let schema = &document["components"]["schemas"][name];
        let properties: BTreeSet<&str> = schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let required: BTreeSet<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r.as_str().unwrap())
            .collect();
        assert_eq!(
            required, properties,
            "{name}: the route writes every key on every row, null or not"
        );
    }
}

#[tokio::test]
async fn the_request_detail_answers_stream_cut_as_null_or_the_cuts_six_members() {
    let (_app, _up, document) = document().await;
    let schemas = &document["components"]["schemas"];
    assert!(
        admits_null(&schemas["RequestDetail"]["properties"]["stream_cut"]),
        "a request without a cut answers null: {}",
        schemas["RequestDetail"]
    );
    let properties: BTreeSet<&str> = schemas["StreamCut"]["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        properties,
        BTreeSet::from([
            "at",
            "attempt",
            "bytes_relayed",
            "code",
            "text",
            "upstream_request_id"
        ])
    );
}

#[tokio::test]
async fn a_session_row_answers_its_context_figures_and_agent_lines_as_null_or_their_values() {
    let (_app, _up, document) = document().await;
    let schemas = &document["components"]["schemas"];
    for member in [
        "agent_contexts",
        "cached_prefix_tokens",
        "context_share",
        "context_window_tokens",
    ] {
        assert!(
            admits_null(&schemas["Session"]["properties"][member]),
            "Session.{member}: a row with no figure answers null: {}",
            schemas["Session"]
        );
    }
    let properties: BTreeSet<&str> = schemas["AgentContext"]["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        properties,
        BTreeSet::from([
            "agent_id",
            "cached_prefix_tokens",
            "context_share",
            "context_window_tokens",
            "last_input_tokens",
            "model"
        ])
    );
    for member in [
        "cached_prefix_tokens",
        "context_share",
        "context_window_tokens",
    ] {
        assert!(
            admits_null(&schemas["AgentContext"]["properties"][member]),
            "AgentContext.{member}: a line with no turn answers null"
        );
    }
}

#[tokio::test]
async fn the_login_takes_stay_logged_in_false_by_default_and_the_session_answers_persistent() {
    let (_app, _up, document) = document().await;
    let schemas = &document["components"]["schemas"];
    let input = &schemas["LoginInput"];
    assert_eq!(
        input["properties"]["stay_logged_in"]["type"], "boolean",
        "{input}"
    );
    assert_eq!(input["properties"]["stay_logged_in"]["default"], false);
    assert!(
        !input["required"]
            .as_array()
            .is_some_and(|r| r.iter().any(|m| m == "stay_logged_in")),
        "a login may leave the flag out: {input}"
    );
    let view = &schemas["SessionView"];
    assert_eq!(
        view["properties"]["persistent"]["type"], "boolean",
        "{view}"
    );
    assert!(
        view["required"]
            .as_array()
            .is_some_and(|r| r.iter().any(|m| m == "persistent")),
        "every session answer carries the member: {view}"
    );
}

#[tokio::test]
async fn the_pool_policy_inputs_are_closed_as_their_parsers_refuse_an_unknown_field() {
    let (_app, _up, document) = document().await;
    for name in [
        "CacheTtlPolicyInput",
        "KeepalivePolicyInput",
        "ModelTtlInput",
        "TtlRulesInput",
    ] {
        assert_eq!(
            document["components"]["schemas"][name]["additionalProperties"], false,
            "{name}: the parser refuses an unknown field"
        );
    }
}

/// Whether a rendered schema admits null. Under OpenAPI 3.1 a nullable value is a `type` array naming `null`, and a
/// nullable reference is a `oneOf` with a `null` member.
fn admits_null(schema: &Value) -> bool {
    let typed = match &schema["type"] {
        Value::String(t) => t == "null",
        Value::Array(types) => types.iter().any(|t| t == "null"),
        _ => false,
    };
    typed
        || ["anyOf", "oneOf"].iter().any(|k| {
            schema[*k]
                .as_array()
                .is_some_and(|members| members.iter().any(admits_null))
        })
}

#[tokio::test]
async fn a_key_the_route_leaves_out_rather_than_writing_null_does_not_render_nullable() {
    let (_app, _up, document) = document().await;
    let schemas = &document["components"]["schemas"];
    let fields: [(&str, &[&str]); 6] = [
        ("ConditionView", &["name", "path", "selector", "value"]),
        ("GuardrailTestSample", &["key_id", "pool_id"]),
        ("ModelRowView", &["fast", "long_context"]),
        ("OverrideView", &["type_multiplier", "weight_by_window"]),
        ("RoutingLog", &["counts"]),
        (
            "TypeOverrideView",
            &[
                "cache_read",
                "cache_write_1h",
                "cache_write_5m",
                "input",
                "output",
            ],
        ),
    ];
    for (name, keys) in fields {
        for key in keys {
            let property = &schemas[name]["properties"][key];
            assert!(property.is_object(), "{name}.{key} is a property");
            assert!(
                !admits_null(property),
                "{name}.{key}: the key is absent or a value, never null: {property}"
            );
        }
    }
}

/// The JSON types a rendered schema names itself, from a `type` string or a `type` array.
fn types_of(schema: &Value) -> Vec<&str> {
    match &schema["type"] {
        Value::String(t) => vec![t.as_str()],
        Value::Array(types) => types.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

/// Every `oneOf` at or below `node`, each with the JSON pointer that reaches it.
fn one_ofs<'a>(node: &'a Value, pointer: &str, found: &mut Vec<(String, &'a [Value])>) {
    match node {
        Value::Object(map) => {
            for (key, value) in map {
                let below = format!("{pointer}/{key}");
                if key == "oneOf" {
                    if let Some(members) = value.as_array() {
                        found.push((below.clone(), members));
                    }
                }
                one_ofs(value, &below, found);
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                one_ofs(item, &format!("{pointer}/{index}"), found);
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn a_facts_value_is_a_boolean_a_number_a_word_or_a_list_of_words_never_an_integer() {
    let (_app, _up, document) = document().await;
    let schemas = &document["components"]["schemas"];
    let mut schema = &schemas["SummaryFact"]["properties"]["value"];
    if let Some(reference) = schema["$ref"].as_str() {
        schema = &schemas[reference.rsplit('/').next().unwrap()];
    }
    let members = schema["oneOf"]
        .as_array()
        .unwrap_or_else(|| panic!("the fact's value renders as a oneOf: {schema}"));
    let types: Vec<Vec<&str>> = members.iter().map(types_of).collect();
    assert_eq!(
        types,
        [
            vec!["boolean"],
            vec!["number"],
            vec!["string"],
            vec!["array"]
        ],
        "the fact's value is a boolean, a number, a word or a list of words: {schema}"
    );
    assert_eq!(
        types_of(&members[3]["items"]),
        ["string"],
        "the list holds words: {schema}"
    );
    assert!(
        types.iter().flatten().all(|t| *t != "integer"),
        "a whole value is a number, so no member is an integer: {schema}"
    );
}

#[tokio::test]
async fn no_one_of_in_the_components_holds_two_numeric_members_since_an_integer_matches_both() {
    let (_app, _up, document) = document().await;
    let mut found = Vec::new();
    one_ofs(
        &document["components"]["schemas"],
        "#/components/schemas",
        &mut found,
    );
    assert!(!found.is_empty(), "the components hold a oneOf to check");
    for (pointer, members) in found {
        let numeric = members
            .iter()
            .filter(|member| {
                types_of(member)
                    .iter()
                    .any(|t| *t == "integer" || *t == "number")
            })
            .count();
        assert!(
            numeric < 2,
            "{pointer}: {numeric} members take a number, so an integer matches more than one: {members:?}"
        );
    }
}

#[tokio::test]
async fn a_captured_body_outside_a_json_object_answers_as_captured_and_the_document_allows_it() {
    let parts = admin_parts_full().await;
    let now = common::NOW;
    let capture = |id: &str, request: &[u8], response: Option<&[u8]>, seen: u64| BodyRecord {
        request_id: id.into(),
        request_body: request.to_vec(),
        response_body: response.map(<[u8]>::to_vec),
        request_len_seen: seen,
        response_len_seen: response.map_or(0, |r| r.len() as u64),
        expires_at: now + 3_600_000,
    };
    let sse = br#"{"model":"claude-sonnet-5","messages":[]}"#;
    for (id, body, truncated) in [
        (
            "r-sse",
            capture(
                "r-sse",
                sse,
                Some(b"event: message_stop\ndata: {}\n\n".as_slice()),
                sse.len() as u64,
            ),
            false,
        ),
        (
            "r-cut",
            capture("r-cut", br#"{"model":"claude-son"#, None, sse.len() as u64),
            true,
        ),
        (
            "r-scalar",
            capture("r-scalar", b"42", Some(b"\"ok\"".as_slice()), 2),
            false,
        ),
    ] {
        let row = RequestRecord {
            id: id.into(),
            received_at: now,
            bodies_captured: true,
            request_truncated: truncated,
            ..Default::default()
        };
        parts.db.insert_request(&row, &[], Some(&body)).unwrap();
    }
    let cookie = login(&parts.app, "admin-secret").await;
    let read = json(get(&parts.app, "/admin/requests/r-sse/bodies", Some(&cookie)).await).await;
    assert_eq!(
        read["request"]["model"], "claude-sonnet-5",
        "a JSON capture answers as JSON"
    );
    assert!(
        read["response"]
            .as_str()
            .is_some_and(|s| s.starts_with("event: message_stop")),
        "an SSE stream answers as a string"
    );
    let cut = json(get(&parts.app, "/admin/requests/r-cut/bodies", Some(&cookie)).await).await;
    assert_eq!(
        (
            cut["request"].as_str(),
            cut["response"].is_null(),
            cut["request_truncated"].as_bool()
        ),
        (Some(r#"{"model":"claude-son"#), true, Some(true)),
        "a capture cut short answers as a string"
    );
    let scalar =
        json(get(&parts.app, "/admin/requests/r-scalar/bodies", Some(&cookie)).await).await;
    assert_eq!(
        (scalar["request"].as_i64(), scalar["response"].as_str()),
        (Some(42), Some("ok")),
        "a scalar JSON body answers as that scalar"
    );
    let (_app, _up, document) = document().await;
    let bodies = &document["components"]["schemas"]["Bodies"];
    let properties: BTreeSet<&str> = bodies["properties"]
        .as_object()
        .expect("the Bodies schema's properties")
        .keys()
        .map(String::as_str)
        .collect();
    let required: BTreeSet<&str> = bodies["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap())
        .collect();
    assert!(
        required.is_subset(&properties),
        "every required key is a property"
    );
    for answer in [&read, &cut, &scalar] {
        let keys: BTreeSet<&str> = answer
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys, properties,
            "the answer carries the schema's keys and no other"
        );
    }
    for side in ["request", "response"] {
        let schema = &bodies["properties"][side];
        assert!(
            schema.get("properties").is_none() && schema.get("required").is_none(),
            "{side}: the schema restricts no shape"
        );
        assert_ne!(
            schema["type"], "object",
            "{side}: the schema is not an object alone"
        );
        // An any-value schema carries no `type`, so it admits null along with a string, a number and an object.
        assert!(
            schema.get("type").is_none(),
            "{side}: the schema admits any value: {schema}"
        );
    }
}

struct Outcomes(Vec<(&'static str, Reloaded)>);

impl TlsReload for Outcomes {
    fn reload(&self, _now: i64) -> Vec<(&'static str, Reloaded)> {
        self.0.clone()
    }
}

#[tokio::test]
async fn the_reload_route_answers_each_listener_as_exactly_one_of_the_three_closed_shapes() {
    let (_app, _up, document) = document().await;
    let schemas = &document["components"]["schemas"];
    let shapes: Vec<BTreeSet<&str>> = schemas["ListenerOutcome"]["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .map(|member| {
            let name = member["$ref"].as_str().unwrap().rsplit('/').next().unwrap();
            assert_eq!(
                schemas[name]["additionalProperties"], false,
                "{name}: a closed shape, so no payload fits two members"
            );
            schemas[name]["properties"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect()
        })
        .collect();
    assert_eq!(shapes.len(), 3, "the oneOf names the three shapes");
    let error = "proxy.tls.key_file /etc/phoebis/proxy.key: does not match the certificate in /etc/phoebis/proxy.pem";
    let pair = Reloaded::Pair {
        cert_file: "/etc/phoebis/proxy.pem".into(),
        expires_ms: 1_822_478_400_000,
        days_left: 364,
    };
    let refused = Reloaded::Refused {
        error: error.into(),
    };
    for (outcomes, expected) in [
        (
            vec![("proxy", pair), ("admin", Reloaded::Plain)],
            [("tls", Some(true)), ("plain", None)],
        ),
        (
            vec![("proxy", refused), ("admin", Reloaded::Plain)],
            [("tls", Some(false)), ("plain", None)],
        ),
    ] {
        let (app, _up) = admin_app_reloading(Arc::new(Outcomes(outcomes))).await;
        let cookie = login(&app, "admin-secret").await;
        let answer = json(post(&app, "/admin/tls/reload", "", Some(&cookie)).await).await;
        let listeners = answer["listeners"].as_array().unwrap();
        assert_eq!(listeners.len(), 2, "one object per listener");
        assert_eq!(
            listeners
                .iter()
                .map(|listener| listener["listener"].as_str())
                .collect::<Vec<_>>(),
            [Some("proxy"), Some("admin")],
            "proxy then admin"
        );
        for (listener, (serving, reloaded)) in listeners.iter().zip(expected) {
            let keys: BTreeSet<&str> = listener
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(
                shapes.iter().filter(|shape| **shape == keys).count(),
                1,
                "{keys:?}: one shape has this field set"
            );
            assert_eq!(
                (listener["serving"].as_str(), listener["reloaded"].as_bool()),
                (Some(serving), reloaded)
            );
        }
        assert_eq!(
            listeners[0]["error"].is_string(),
            listeners[0]["reloaded"] == false,
            "a refused pair carries its line"
        );
    }
}

#[test]
fn every_query_name_a_handler_reads_is_declared_and_nothing_else() {
    for (module, source) in QUERY_MODULES {
        assert_eq!(
            reader_names(source),
            declared_names(source),
            "{module}: the names read and the names declared"
        );
    }
}

/// The query check compares names per module, and the breakdown's query declares the same names, so it would pass a
/// burn operation that left the two it reads out of the document.
#[tokio::test]
async fn the_burn_operation_declares_the_account_filter_and_the_agent_groups_it_reads() {
    let (_app, _up, document) = document().await;
    let names: BTreeSet<&str> = document["paths"]["/admin/analytics/burn"]["get"]["parameters"]
        .as_array()
        .expect("the burn operation's parameters")
        .iter()
        .filter_map(|parameter| parameter["name"].as_str())
        .collect();
    assert_eq!(
        names,
        BTreeSet::from(["accounts", "group_by", "range", "user_agent_groups"]),
        "the burn operation's query"
    );
}

#[test]
fn every_module_that_reads_a_query_is_in_the_query_check() {
    let checked: BTreeSet<&str> = QUERY_MODULES.iter().map(|(m, _)| *m).collect();
    for (module, source) in HANDLER_MODULES {
        if !checked.contains(module) {
            assert!(
                reader_names(source).is_empty(),
                "{module} reads a query and is not in QUERY_MODULES"
            );
        }
    }
}

#[test]
fn the_query_check_reads_a_direct_read_a_split_chain_and_the_fields_of_an_into_params_struct() {
    let source = r#"
async fn list(Query(q): Query<HashMap<String, String>>) -> Response {
    let limit = q.get("limit");
    let include_probes = q
        .get("include_probes")
        .is_some_and(|v| v == "true");
}

/// The list's query.
#[allow(dead_code)]
#[derive(Deserialize, IntoParams)]
pub(crate) struct ListQuery {
    /// At most this many rows.
    #[param(example = 50)]
    pub limit: Option<String>,
    /// Whether the probes count: `true` or `1`.
    pub include_probes: Option<String>,
}
"#;
    let expected: BTreeSet<String> = ["include_probes", "limit"]
        .iter()
        .map(|n| n.to_string())
        .collect();
    assert_eq!(
        reader_names(source),
        expected,
        "a direct read and a chain rustfmt split over two lines"
    );
    assert_eq!(
        declared_names(source),
        expected,
        "the fields under a doc comment and an attribute"
    );
}
