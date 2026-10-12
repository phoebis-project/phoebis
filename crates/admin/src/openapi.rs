//! The OpenAPI document of the admin listener: the info block and the introduction, the tags, the bearer scheme,
//! the shared error body and responses, the captured examples, and the route that serves the document.

use std::collections::BTreeMap;

use axum::body::Bytes;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::openapi::example::ExampleBuilder;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::openapi::{Components, RefOr};
use utoipa::{Modify, OpenApi, ToResponse, ToSchema};

/// Where the document is served on the admin listener, outside `/admin/` so the page loads it before a token is
/// entered.
pub(crate) const DOCUMENT_PATH: &str = "/api-reference/openapi.json";

/// The document's lead: the access rules, the shared shapes and how to read an operation. The site's pointer page
/// carries none of it, so a rule lives here once.
const INTRODUCTION: &str = r#"Every operation here lives under `/admin/` on the admin listener, behind one gate: a session cookie from `POST /admin/session`, or `Authorization: Bearer <admin secret>` on a scripted call, and an `Origin` check on every browser mutation. The operations are grouped by area, and each names its method, path, request, answer and refusals. The console uses these routes and nothing else.

## Access

Log in with `POST /admin/session` and the admin secret. The answer sets the `phoebis_admin` cookie, which every later call carries. Five failed logins in a minute refuse every login for the rest of that minute (`429 too many failed logins, try again in a minute`), and a wrong secret answers `401 secret does not match`. A session ends after 15 minutes idle or 12 hours in all. Every `DELETE`, `PATCH`, `POST` or `PUT` that carries an `Origin` must name the request's own `Host` with it, the authority equal ignoring case, port included, and the scheme `https` where the admin listener terminates TLS or the request carries a front's `X-Forwarded-Proto: https`, `http` or `https` otherwise (`403 origin is not allowed`). One with no `Origin` is admitted. Behind a front that terminates TLS the front passes `Host` through and sets the header, so both headers name it and the scheme is required. Every `PATCH`, `POST` or `PUT` that carries a body must send `Content-Type: application/json` (`415 content type must be application/json`). A missing or unknown cookie answers `401 no admin session`, except on `DELETE /admin/session`, which answers its clear whatever the cookie names. Every answer carries `Cache-Control: no-store` and `X-Content-Type-Options: nosniff`.

A request that carries `Authorization: Bearer` with the admin secret is admitted without a cookie and without the `Origin` check. A wrong bearer answers `401 secret does not match` and counts toward the login throttle, so a throttled process answers it `429` too. A bearer on `POST /admin/session` is ignored: the login stays the cookie's door. This page's Authorization input sends the admin secret as that bearer on every call it makes and keeps it in memory alone, so the secret is gone with the tab.

## Shapes

Every error is `{"error": "<message>"}`. A `503` from a registry reload also carries the header `x-phoebis-error-code: registry_unavailable` and the message "the registry was written but could not be reloaded, and the next successful reload publishes it", a `409` from a pool, key or account edit over a row that changed since it was read carries `x-phoebis-error-code: stale_edit`, a store failure is `500 storage failure`, and a ledger failure is `500 ledger failure`. `expected_revision` on `PATCH /admin/accounts/{id}`, `PATCH /admin/keys/{id}` and `PATCH /admin/pools/{id}` is optional: its value is the row's `revision` as the last answer carried it. A malformed value answers `400`, a stale one `409` under `stale_edit`, and a body without it fences on the route's own read. Every time is Unix milliseconds. An `id` given on creation matches `[a-z0-9][a-z0-9-]{0,63}`. A `range` parameter is `1h`, `24h`, `7d`, `30d`, or `<from>,<to>` in Unix milliseconds spanning at most 400 days. The default differs per route and each operation says which. A list of ids in a query parameter is comma-separated.

## Reading an operation

Each operation names its request (path segments, query parameters, or body fields with their bounds), its answer (the top-level keys, with an example captured from the route where one exists) and its refusals (status and message per refusal). The gate's own refusals above apply to every operation and are not repeated under each. Any other path under `/admin/` answers `404 not found`, behind the same gate: an anonymous caller sees the gate's `401` first, whatever the path, and a session that reaches the `404` learns only that nothing serves the path. The proxy listener's routes, `GET /v1/usage` among them, and the metrics listener are not part of this document.
"#;

/// The document's static half: the info block, the one relative server, the seventeen tags in the seed's order,
/// the bearer requirement on every operation, and the four shared responses. The paths join it in `router()`.
#[derive(OpenApi)]
#[openapi(
    info(title = "Phoebis Router admin API"),
    servers((url = "/", description = "This router's admin listener")),
    tags(
        (name = "Accounts"),
        (name = "Analytics", description = "`GET /admin/analytics/accounts`, `/breakdown`, and `/series` share `range` \
            (default `24h`), `group_by` (`account`, `all`, `client`, `effort`, `key`, `kind`, `model`, `pool`, \
            `request_class`, `speed`, or `user_agent`), and filters `keys`, `pools`, `accounts`, `models`, `kinds` \
            (comma-separated lists), `user_agents` (newline-separated patterns over the agent's first product token, \
            `claude-cli/2.1.286` for `claude-cli/2.1.286 (external, cli)`: `*` matches any run of characters, `?` one, \
            and a backslash quotes either, so a value with no wildcard matches in full), `user_agent_groups` \
            (newline-separated patterns. Under `group_by=user_agent` each names a group, a row falls in the first that \
            matches its agent's token, and the rest group under `other`), `clients` (newline-separated patterns over \
            the client class, the grammar and bound of `user_agents`), and `include_probes` (`true`/`1`). `/breakdown` \
            and `/series` also take `funding` (`billed` keeps the overage and fast rows and every row an API key \
            account served, `plan` the rows a subscription window paid for and the rows of a request no account \
            served. Each row files under the funding its serving attempt had when the ledger recorded it, so deleting \
            an account, or recreating its id as the other kind, moves none of its rows. A probe's cost files under the \
            funding of the probe's own request, while a recipe's settlement, which carries the probes' waste, files \
            under the kind its record carries: `billed` for an API key account, else `fast` for a fast recipe, else \
            `plan`. Absent keeps every row. The two subsets partition the rows). `GET /admin/analytics/budgets`, \
            `/burn`, `/placements`, and `/rewrites`, elsewhere under this tag, take their own request shapes."),
        (name = "Attention"),
        (name = "Dashboard"),
        (name = "Keepalive"),
        (name = "Keys", description = "The secret is answered on `POST /admin/keys` and `POST /admin/keys/{id}/rotate` \
            and nowhere else. A `key_json` answer never carries the digest either."),
        (name = "Ledger"),
        (name = "Models"),
        (name = "Pairing", description = "Pairing is a stateful, multi-step flow: start a session, receive the upstream's \
            callback, confirm it into a committed credential. The documentation site's pairing page works the flow \
            through with `curl`."),
        (name = "Pools"),
        (name = "Pricing"),
        (name = "Requests"),
        (name = "Routing log"),
        (name = "Session"),
        (name = "Sessions", description = "Rows group by key, key generation, and session, so a key id created again \
            never merges its sessions with an earlier holder's. A row whose generation no live key holds is \
            `historical`, reads `companion` `none` with no recipe, and cannot be released. The session of a pinned or \
            active row is the one the router keys pins by (a request's `routing_session_id`). A row the keepalive mod \
            registered keeps the client's own session, the one the mod registered under."),
        (name = "Settings"),
        (name = "System"),
    ),
    modifiers(&Bearer),
    security(("bearer" = [])),
    components(responses(StorageFailure, LedgerFailure, RouterUnavailable, RegistryUnavailable))
)]
pub(crate) struct Document;

/// The introduction and the bearer scheme, added at build: the derive takes the description from the crate's
/// manifest and the scheme's description is prose, and both belong beside the text they describe.
pub(crate) struct Bearer;

impl Modify for Bearer {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        openapi.info.description = Some(INTRODUCTION.to_string());
        // The derive fills the license from the crate's manifest too, and the info block names none: the product's
        // license is the repository's, and the page is not public.
        openapi.info.license = None;
        openapi.components.get_or_insert_with(Components::new).add_security_scheme(
            "bearer",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some(
                        "The admin secret, as `Authorization: Bearer <secret>`. A wrong secret counts toward the \
                         login throttle, and a bearer request skips the Origin check.",
                    ))
                    .build(),
            ),
        );
    }
}

/// Every refusal's body.
#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ErrorBody {
    /// The message, as the refusal spells it.
    #[schema(example = "no admin session")]
    pub error: String,
}

/// `500 storage failure`: the store failed the read or the write, or a create lost the race for its id.
// Declared for the document alone, since the handler builds the answer by hand.
#[allow(dead_code)]
#[derive(ToResponse)]
#[response(example = json!({"error": "storage failure"}))]
pub(crate) struct StorageFailure(ErrorBody);

/// `500 ledger failure`: the ledger could not answer the read.
// Declared for the document alone, since the handler builds the answer by hand.
#[allow(dead_code)]
#[derive(ToResponse)]
#[response(example = json!({"error": "ledger failure"}))]
pub(crate) struct LedgerFailure(ErrorBody);

/// `503 the router is not available`: the routing task is gone, so nothing can be published or read from it.
// Declared for the document alone, since the handler builds the answer by hand.
#[allow(dead_code)]
#[derive(ToResponse)]
#[response(example = json!({"error": "the router is not available"}))]
pub(crate) struct RouterUnavailable(ErrorBody);

/// `503`, one of two messages. `the registry was written but could not be reloaded, and the next successful reload
/// publishes it`, with the header: the row is already written, and until that reload the router still
/// authenticates and routes by the old registry. `the router is not available`, with no header: the routing task is
/// gone, so nothing can be published.
// Declared for the document alone, since the handler builds the answer by hand.
#[allow(dead_code)]
#[derive(ToResponse)]
#[response(
    example = json!({"error": "the registry was written but could not be reloaded, and the next successful reload publishes it"}),
    headers(("x-phoebis-error-code" = String, description = "`registry_unavailable`: the write landed and routing has not been told"))
)]
pub(crate) struct RegistryUnavailable(ErrorBody);

macro_rules! fixture {
    ($name:literal, $status:literal) => {
        (
            $name,
            $status,
            include_str!(concat!("../../../web/test/fixtures/", $name, ".json")),
        )
    };
}

/// The fixture files the document carries as examples: the file stem, the status each is the example of, and the
/// file, embedded so the document carries what the route answered. `bound_refusals.json` (the bound probes) and
/// `test_tables.json` (no route) attach to nothing and are not listed.
const FIXTURES: &[(&str, &str, &str)] = &[fixture!("session", "200")];

/// The bodies the fixtures capture for one answer, each with its file stem.
type Bodies = Vec<(&'static str, Value)>;

/// The captured answers onto their operations, once the paths are registered: a fixture names its route, the
/// route's path pattern names the operation, and several fixtures on one answer become named examples by stem.
pub(crate) struct Examples;

impl Modify for Examples {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let mut by_answer: BTreeMap<(String, String, &str), Bodies> = BTreeMap::new();
        for (name, status, text) in FIXTURES {
            let file: Value =
                serde_json::from_str(text).unwrap_or_else(|e| panic!("{name}.json: {e}"));
            let route = file["route"]
                .as_str()
                .unwrap_or_else(|| panic!("{name}.json names no route"));
            let (method, path) = route
                .split_once(' ')
                .unwrap_or_else(|| panic!("{name}.json: {route}"));
            let path = path.split('?').next().unwrap_or(path);
            let pattern = openapi
                .paths
                .paths
                .keys()
                .find(|pattern| matches(pattern, path))
                .unwrap_or_else(|| panic!("{name}.json: no operation at {route}"))
                .clone();
            by_answer
                .entry((pattern, method.to_string(), status))
                .or_default()
                .push((name, file["body"].clone()));
        }
        for ((pattern, method, status), bodies) in by_answer {
            let item = openapi
                .paths
                .paths
                .get_mut(&pattern)
                .expect("the pattern was found above");
            let operation = match method.as_str() {
                "GET" => item.get.as_mut(),
                "POST" => item.post.as_mut(),
                "PATCH" => item.patch.as_mut(),
                "PUT" => item.put.as_mut(),
                "DELETE" => item.delete.as_mut(),
                _ => None,
            }
            .unwrap_or_else(|| panic!("no {method} at {pattern}"));
            let response = match operation.responses.responses.get_mut(status) {
                Some(RefOr::T(response)) => response,
                _ => panic!("{method} {pattern} declares no {status} response of its own"),
            };
            let content = match response.content.get_mut("application/json") {
                Some(RefOr::T(content)) => content,
                _ => panic!("{method} {pattern} {status} declares no JSON body"),
            };
            if let [(_, body)] = bodies.as_slice() {
                content.example = Some(body.clone());
            } else {
                for (name, body) in bodies {
                    content.examples.insert(
                        name.to_string(),
                        RefOr::T(ExampleBuilder::new().value(Some(body)).build()),
                    );
                }
            }
        }
    }
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

/// The document's answer. The gate's middleware never runs on this route, so the headers every admin answer carries
/// are set here. `no-store`, because the document changes with every binary and names the version, and a browser
/// holding an old copy across an upgrade would list operations the new binary no longer serves.
pub(crate) fn document(text: Bytes) -> Response {
    (
        StatusCode::OK,
        [
            (CONTENT_TYPE, HeaderValue::from_static("application/json")),
            (CACHE_CONTROL, HeaderValue::from_static("no-store")),
            (X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")),
        ],
        text,
    )
        .into_response()
}

#[cfg(test)]
pub(crate) mod round_trip {
    use serde::de::DeserializeOwned;
    use serde::Serialize;
    use serde_json::Value;

    /// The committed body of a route fixture.
    pub(crate) fn fixture(stem: &str) -> Value {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../web/test/fixtures")
            .join(format!("{stem}.json"));
        let file: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        file["body"].clone()
    }

    /// The fixture's body deserializes into the declared type with no unknown field and serializes back to the
    /// same value, so the declared type and the captured answer cannot part. The compare is `Value` equality,
    /// order-insensitive: the key order is the capture harness's business.
    pub(crate) fn round_trips<T: DeserializeOwned + Serialize>(stem: &str) {
        let body = fixture(stem);
        let typed: T = serde_json::from_value(body.clone())
            .unwrap_or_else(|e| panic!("{stem}.json does not fit the declared type: {e}"));
        assert_eq!(serde_json::to_value(&typed).unwrap(), body, "{stem}.json");
    }
}

#[cfg(test)]
mod tests {
    use super::round_trip::round_trips;

    #[test]
    fn the_session_answer_round_trips_through_its_declared_type() {
        round_trips::<crate::routes::SessionView>("session");
    }
}
