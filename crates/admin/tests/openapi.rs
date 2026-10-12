//! The admin listener's OpenAPI document: served without the gate, complete over the route table, one tag per
//! operation, the bearer scheme on every operation but the login and the logout, every refusal an `ErrorBody`, and
//! the capture's answers as the examples.
mod common;
use std::collections::{BTreeMap, BTreeSet};

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use common::{admin_parts_full, get, json, login, post};
use http_body_util::BodyExt;
use phoebis_ledger::{BodyRecord, RequestRecord};
use serde_json::Value;
use tower::ServiceExt;

const DOCUMENT_PATH: &str = "/api-reference/openapi.json";

/// The route table's source, read as text so a route added as a plain `.route` cannot escape the document.
const ROUTES_RS: &str = include_str!("../src/routes.rs");

/// The paths the table registers with a plain `.route(`: outside the document until their area declares them, and
/// the two catch-alls for good, which the introduction describes.
const PLAIN_ROUTES: &[&str] = &[
    "/admin/analytics/accounts",
    "/admin/analytics/breakdown",
    "/admin/analytics/budgets",
    "/admin/analytics/burn",
    "/admin/analytics/placements",
    "/admin/analytics/rewrites",
    "/admin/analytics/series",
    "/admin/attention",
    "/admin/dashboard",
    "/admin/sessions",
    "/admin/sessions/{key}/{session}/release",
    "/admin/sessions/costs",
    "/admin/settings",
    "/admin/system",
    "/admin/tls/reload",
    "/admin",
    "/admin/{*rest}",
];

/// The seventeen tags in the order the document declares them, the seed's area order.
const TAGS: [&str; 17] = [
    "Accounts",
    "Analytics",
    "Attention",
    "Dashboard",
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
    ("session_logout", &[]),
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
];

/// The fixture stems the document carries as examples, each file's `body` member on its operation's answer.
const EXAMPLE_FILES: &[&str] = &[
    "session",
    "accounts",
    "account",
    "account_catalog",
    "account_quota",
    "account_usage",
    "keepalive_overview",
    "keepalive_recipes",
    "keys",
    "key_created",
    "key_models",
    "replay",
    "replay_held",
    "models_state",
    "pairing_started",
    "pools",
    "pool_traffic",
    "pricing",
    "requests",
    "request",
    "request_bodies",
    "decision",
    "routing_log",
];

/// Every handler module that reads a query map, with its source, for the check that the names it reads are the
/// names its parameter types declare.
const QUERY_MODULES: &[(&str, &str)] = &[
    ("accounts", include_str!("../src/accounts.rs")),
    ("pools", include_str!("../src/pools.rs")),
    ("requests", include_str!("../src/requests.rs")),
    ("routing_log", include_str!("../src/routing_log.rs")),
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
        let literal = after.trim_start().trim_start_matches(['\n', ' ']);
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
        assert_eq!(refusals, expected, "{id}: the refusals the seed names");
        for (status, response) in responses.iter().filter(|(s, _)| !s.starts_with('2')) {
            let shared = response["$ref"]
                .as_str()
                .map(|r| r.starts_with("#/components/responses/"))
                .unwrap_or(false);
            let inline = response["content"]["application/json"]["schema"]["$ref"]
                == "#/components/schemas/ErrorBody";
            assert!(shared || inline, "{id} {status}: the body is an ErrorBody");
            if status == "409" {
                let stale_edit = ["account_patch", "key_patch", "pool_patch"].contains(&id);
                assert_eq!(
                    response["headers"]["x-phoebis-error-code"].is_object(),
                    stale_edit,
                    "{id} 409: the code header on the three stale_edit sites alone"
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
    }
    // An any-value schema carries no `type`, so it admits null along with a string, a number and an object.
    assert!(
        bodies["properties"]["response"].get("type").is_none(),
        "response admits null: {}",
        bodies["properties"]["response"]
    );
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
