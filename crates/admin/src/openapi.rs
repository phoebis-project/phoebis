//! The OpenAPI document of the admin listener: the info block and the introduction, the tags, the bearer scheme,
//! the shared error body and responses, the captured examples, and the route that serves the document.

use std::collections::BTreeMap;

use axum::body::Bytes;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use phoebis_guardrails::{Condition, ConditionWords};
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

Log in with `POST /admin/session` and the admin secret. The answer sets the `phoebis_admin` cookie, which every later call carries. Five failed logins in a minute refuse every login for the rest of that minute (`429 too many failed logins, try again in a minute`), and a wrong secret answers `401 secret does not match`. A session ends after 15 minutes idle or 12 hours in all. A login with `stay_logged_in` true keeps its session for 30 days from its last read, across a restart: every answer to it but its logout's sets a renewed cookie of the same 30 days, and a secret rotation or the logout of a kept login ends it, the logout ending every kept login of the install. Every `DELETE`, `PATCH`, `POST` or `PUT` that carries an `Origin` must name the request's own `Host` with it, the authority equal ignoring case, port included, and the scheme `https` where the admin listener terminates TLS or the request carries a front's `X-Forwarded-Proto: https`, `http` or `https` otherwise (`403 origin is not allowed`). One with no `Origin` is admitted. Behind a front that terminates TLS the front passes `Host` through and sets the header, so both headers name it and the scheme is required. Every `PATCH`, `POST` or `PUT` that carries a body must send `Content-Type: application/json` (`415 content type must be application/json`). A missing or unknown cookie answers `401 no admin session`, except on `DELETE /admin/session`, which answers its clear whatever the cookie names. Every answer carries `Cache-Control: no-store`, `X-Content-Type-Options: nosniff` and `x-phoebis-build`, the full commit the router was built from or `unknown` for a build that had none.

A request that carries `Authorization: Bearer` with the admin secret is admitted without a cookie and without the `Origin` check. A wrong bearer answers `401 secret does not match` and counts toward the login throttle, so a throttled process answers it `429` too. A bearer on `POST /admin/session` is ignored: the login stays the cookie's door. This page's Authorization input sends the admin secret as that bearer on every call it makes and keeps it in memory alone, so the secret is gone with the tab.

## Shapes

Every error is `{"error": "<message>"}`. A `503` from a registry reload also carries the header `x-phoebis-error-code: registry_unavailable` and the message "the registry was written but could not be reloaded, and the next successful reload publishes it", a `409` from an account, guardrail, key or pool edit over a row that changed since it was read carries `x-phoebis-error-code: stale_edit`, a store failure is `500 storage failure`, and a ledger failure is `500 ledger failure`. `expected_revision` on `PATCH /admin/accounts/{id}`, `PATCH /admin/guardrails/{id}`, `PATCH /admin/keys/{id}` and `PATCH /admin/pools/{id}` is optional: its value is the row's `revision` as the last answer carried it. A malformed value answers `400` and a stale one `409` under `stale_edit`, but on `PATCH /admin/keys/{id}` a pool the body names is read first, so a missing pool answers `404` ahead of both. A body without `expected_revision` fences on the route's own read. Every time is Unix milliseconds. An `id` given on creation matches `[a-z0-9][a-z0-9-]{0,63}`. A `range` parameter is `1h`, `24h`, `7d`, `30d`, or `<from>,<to>` in Unix milliseconds spanning at most 400 days. The default differs per route and each operation says which. A list of ids in a query parameter is comma-separated.

## Reading an operation

Each operation names its request (path segments, query parameters, or body fields with their bounds), its answer (the top-level keys, with an example captured from the route where one exists) and its refusals (status and message per refusal). The gate's own refusals above apply to every operation and are not repeated under each. Any other path under `/admin/` answers `404 not found`, behind the same gate: an anonymous caller sees the gate's `401` first, whatever the path, and a session that reaches the `404` learns only that nothing serves the path. The proxy listener's routes, `GET /v1/usage` among them, and the metrics listener are not part of this document.
"#;

/// The document's static half: the info block, the one relative server, the seventeen tags in the API page's area order
/// (`docs/site/api-reference.md`), the bearer requirement on every operation, and the four shared responses. The paths
/// join it in `routes::parts()`.
#[derive(OpenApi)]
#[openapi(
    info(title = "Phoebis Router admin API"),
    servers((url = "/", description = "This router's admin listener")),
    tags(
        (name = "Accounts"),
        (name = "Analytics", description = "`GET /admin/analytics/breakdown` and `/series` share `range` (default \
            `24h`), `group_by` (`account`, `all`, `classifier`, `client`, `effort`, `key`, `kind`, `model`, `pool`, \
            `request_class`, `speed`, or `user_agent`), the filters `keys`, `pools`, `accounts`, `models`, `kinds` \
            (comma-separated lists), `user_agents` (newline-separated patterns over the agent's first product token, \
            `claude-cli/2.1.286` for `claude-cli/2.1.286 (external, cli)`: `*` matches any run of characters, `?` one, \
            and a backslash quotes either, so a value with no wildcard matches in full), `user_agent_groups` \
            (newline-separated patterns. Under `group_by=user_agent` each names a group, a row falls in the first that \
            matches its agent's token, and the rest group under `other`), `clients` (newline-separated patterns over \
            the client class, the grammar and bound of `user_agents`), and `include_probes` (`true`/`1`). Both also \
            take `funding` (`billed` keeps the overage and fast rows, every row an API key account served and every \
            row a usage-based seat served, `plan` the rows a subscription window paid for and the rows of a request \
            no account served. Each row files under the funding its serving attempt had when the ledger recorded it, \
            so deleting an account, or recreating its id as the other kind, moves none of its rows. A probe's cost \
            files under the funding of the probe's own request, while a recipe's settlement, which carries the \
            probes' waste, files under the kind and the class its record carries: `billed` for an API key account, \
            `usage_based` for a usage-based seat, else `fast` for a fast recipe, else `plan`. Absent keeps every \
            row. The two subsets partition the rows). `/accounts`, `/guardrails` and `/rewrites` take `range` alone. \
            `GET /admin/analytics/budgets`, `/burn`, `/placements`, `/reconciliations`, `/refusal_categories`, and \
            `/unused_allowance`, elsewhere under this tag, take their own request shapes."),
        (name = "Attention"),
        (name = "Dashboard"),
        (name = "Guardrails", description = "A guardrail is a named list of conditions a request is checked against \
            before any account is selected, attached to pools and keys in order. `observe` records each match on the \
            request, `enforce` refuses a matching request with `403 guardrail_blocked`."),
        (name = "Keepalive"),
        (name = "Keys", description = "The secret is answered on `POST /admin/keys` and `POST /admin/keys/{id}/rotate` \
            and nowhere else. A `key_json` answer never carries the digest either."),
        (name = "Ledger"),
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
            `historical` and reads `companion` `none` with no recipe. The session of a pinned or \
            active row is the one the router keys pins by, the client's own session id. A row the keepalive mod \
            registered keeps the client's own session, the one the mod registered under. The row's `spend`, `day` \
            and `agent_requests` read one fold over the session's life under its key, the count of its requests \
            `spend.requests`, so Requests and Cost agree."),
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

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentCountsView {
    pub busy: u64,
    pub idle: u64,
    pub parked: u64,
}

/// The probe counts by outcome.
#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProbeCountsView {
    pub failed: u64,
    pub held: u64,
    pub migrated: u64,
    pub ok: u64,
    /// The plan-funded answered probes, a part of `ok`, so a reader tells a zero plan saving from an unavailable
    /// price.
    pub plan_ok: u64,
    pub skipped: u64,
}

/// An agent's recipe. `probes_today` and `credited_today` count from midnight UTC and read 0 on a day that has
/// neither yet.
#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecipeViewSchema {
    /// The pinned account.
    pub account_id: Option<String>,
    pub agent_id: String,
    /// The agent's lifecycle word: `busy`, `idle`, `parked`, `registered`, `released` or `unknown`, where
    /// `registered` is a record busy awaiting a real request since the mod registered it or started it again.
    #[schema(example = "busy")]
    pub agent_state: String,
    /// The engine's own reason, shown beside the state (the keepalive page's five states).
    pub cause: String,
    pub credited_today: u64,
    pub deadline: Option<i64>,
    pub key_id: String,
    /// The guard's verdict on the last request the agent observed, null before the first one.
    pub last_guard: Option<String>,
    pub last_probe_at: Option<i64>,
    pub last_request_at: Option<i64>,
    pub model: String,
    pub next_probe_at: Option<i64>,
    /// `background`, `both` or `scheduled`, null off a park.
    #[schema(example = "background")]
    pub park_kind: Option<String>,
    pub pool_id: String,
    pub probes_today: u64,
    pub session_id: String,
    /// `retained`, `held`, `paused`, `not_retained` or `ended`.
    #[schema(example = "retained")]
    pub state: String,
    pub ttl_seconds: i64,
    pub wake_at: Option<i64>,
}

/// One entry of a model rule list: the pattern, checked as the routing page's model rules say, and whether it
/// denies the models it matches.
#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelRuleView {
    pub pattern: String,
    pub not: bool,
}

/// One condition of a guardrail, every member a string, so an unknown word is refused with its reason. `name` is the
/// `header` subject's, `path` the `body` subject's, `selector` the `message` subject's, and `value` the `equals`,
/// `gt`, `lt` and `matches` operators'.
#[derive(Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConditionView {
    /// `agent_type`, `body`, `client`, `header`, `kind`, `message`, `model`, `request_class`, or `user_agent`.
    #[schema(example = "request_class")]
    pub subject: String,
    /// `absent`, `equals`, `gt`, `lt`, `matches`, or `present`, as the subject allows.
    #[schema(example = "equals")]
    pub operator: String,
    /// The text `equals` compares, the number `gt` and `lt` compare against, or the pattern `matches` searches for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub value: Option<String>,
    /// The header's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub name: Option<String>,
    /// A member name followed by `.member`, `[n]`, `[-n]` or `[*]` segments, at most 256 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub path: Option<String>,
    /// `any`, `any_assistant`, `any_user`, `first_user`, `last`, `last_assistant`, or `last_user`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false, example = "last_user")]
    pub selector: Option<String>,
}

impl From<&ConditionView> for ConditionWords {
    fn from(view: &ConditionView) -> ConditionWords {
        ConditionWords {
            subject: view.subject.clone(),
            operator: view.operator.clone(),
            value: view.value.clone(),
            name: view.name.clone(),
            path: view.path.clone(),
            selector: view.selector.clone(),
        }
    }
}

impl From<&Condition> for ConditionView {
    fn from(condition: &Condition) -> ConditionView {
        let words = condition.words();
        ConditionView {
            subject: words.subject,
            operator: words.operator,
            value: words.value,
            name: words.name,
            path: words.path,
            selector: words.selector,
        }
    }
}

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeepalivePolicyView {
    pub enabled: bool,
    /// The idle window of the main conversation, taken as set, above the recipe's break-even too. 0 resolves to the
    /// break-even.
    pub max_idle_seconds: u64,
    /// How long a park on a background command or a subagent is probed. Null is the recipe's break-even.
    pub max_parked_seconds: Option<u64>,
    /// A park on a scheduled wakeup is probed while the wake is at most this far away. Null is the recipe's
    /// break-even.
    pub scheduled_horizon_seconds: Option<u64>,
}

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResolvedCeilingsView {
    pub break_even_seconds: RangeView,
    pub horizon_seconds: RangeView,
    pub idle_seconds: RangeView,
    pub idle_share: RangeView,
    pub parked_seconds: RangeView,
}

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RangeView {
    pub min: f64,
    pub max: f64,
}

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CacheTtlPolicyView {
    pub enabled: bool,
    /// Whether the rewrite adds a top-level `cache_control` to a body that carries no marker.
    pub inject_when_absent: bool,
    /// The rules while a write costs money: an API key account, or a subscription account in overage.
    pub metered: TtlRulesView,
    pub plan: TtlRulesView,
}

/// One rule set: the TTL by branch, with per-model overrides.
#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TtlRulesView {
    /// `5m` or `1h`.
    #[schema(example = "5m")]
    pub main: String,
    /// Per-model overrides on the canonical model id. A model absent here takes `main` and `subagent`.
    pub models: Vec<ModelTtlView>,
    /// `5m` or `1h`.
    #[schema(example = "5m")]
    pub subagent: String,
}

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelTtlView {
    /// `5m` or `1h`.
    #[schema(example = "5m")]
    pub main: String,
    pub model: String,
    /// `5m` or `1h`.
    #[schema(example = "5m")]
    pub subagent: String,
}

/// One bucket of a pool's traffic.
#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TrafficBucketView {
    pub at: i64,
    pub requests: u64,
    pub failed: u64,
    pub tokens: u64,
    pub cost_usd: f64,
    /// The dollars of the billed rows alone (fast mode, overage, API key accounts and usage-based seats), a part of
    /// `cost_usd`.
    pub billed_usd: f64,
    /// The plan-funded rows as the Pro-week figure, null where a row priced nothing.
    pub equivalent: Option<f64>,
}

/// One model's list prices, each cell per million tokens.
#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelRowView {
    pub input: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    pub cache_read: f64,
    pub output: f64,
    /// The fast-mode cells under the same five names, on a model that serves fast mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub fast: Option<ModelPriceView>,
    /// The threshold `over` in tokens and the five cells under the same names, on a model priced by prompt length.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub long_context: Option<LongContextView>,
}

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelPriceView {
    pub input: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    pub cache_read: f64,
    pub output: f64,
}

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct LongContextView {
    /// The prompt length in tokens past which the five cells apply.
    pub over: u64,
    pub input: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    pub cache_read: f64,
    pub output: f64,
}

/// Every key of the subscription cost model but `unit`, which the envelope carries once.
#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubscriptionCostsView {
    /// The revision's `effective` as its file wrote it, a date `YYYY-MM-DD` or an instant `YYYY-MM-DDTHH:MM:SSZ`.
    pub effective: String,
    /// Keyed by the tier the account's profile reports.
    pub capacity: BTreeMap<String, CapacityView>,
    pub peak: PeakView,
    pub type_multiplier: TypeMultiplierView,
    /// Units per plain input token, keyed by undated model id. A model without a weight, or of no model tier, prices
    /// nothing.
    pub model_weight: BTreeMap<String, Option<f64>>,
    pub model_windows: BTreeMap<String, ModelWindowView>,
    pub overrides: BTreeMap<String, OverrideView>,
    /// The model tiers in display order.
    pub tier_order: Vec<String>,
    /// Undated model id to the name of its model tier in `tier_order`.
    pub model_tier: BTreeMap<String, String>,
    /// Keyed by the regime's name, each listing the entrypoints it prices and the capacity rows and band factor it
    /// sets for them.
    pub regimes: BTreeMap<String, RegimeView>,
}

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct UnitView {
    pub tier: String,
    pub window: String,
}

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CapacityView {
    pub five_hour: Option<f64>,
    pub seven_day: Option<f64>,
}

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PeakView {
    pub capacity_factor: PerWindowView,
    pub from_hour: u8,
    pub to_hour: u8,
    pub weekdays: bool,
    pub zone: String,
}

/// The peak's factor on each shared window, both always set.
#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PerWindowView {
    pub five_hour: f64,
    pub seven_day: f64,
}

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TypeMultiplierView {
    pub cache_read: f64,
    pub cache_write_1h: f64,
    pub cache_write_5m: f64,
    pub input: f64,
    pub output: f64,
}

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelWindowView {
    pub capacity_factor: f64,
    pub of: String,
}

/// A regime: the entrypoints it prices, and the capacity rows and band factor it sets in place of the base ones. A
/// tier it leaves out prices at the base row.
#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RegimeView {
    pub entrypoints: Vec<String>,
    /// Keyed by the tier the account's profile reports.
    pub capacity: BTreeMap<String, CapacityView>,
    pub peak: RegimePeakView,
}

/// The band factor a regime sets on each shared window, read inside the base band's hours.
#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RegimePeakView {
    pub capacity_factor: PerWindowView,
}

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct OverrideView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub type_multiplier: Option<TypeOverrideView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub weight_by_window: Option<BTreeMap<String, f64>>,
}

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TypeOverrideView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub cache_write_1h: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub cache_write_5m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub input: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub output: Option<f64>,
}

/// One group of the breakdown.
#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct BreakdownRow {
    /// Under `group_by=key` the key's id for the key that holds it now and `<id>@<generation>` for rows an earlier
    /// key filed under the same id, the generation being that key's `created_at`. Under `group_by=effort` the
    /// request's effort as the drawer shows it (the last per-turn control entry's level, else the top-level
    /// `output_config.effort`), `none` for a request that asked for none, or `other` for a level the API does not
    /// name. Under `group_by=user_agent` the agent's first product token cut to 64 bytes, `other` for an agent
    /// without one, and `-` for a request that sent none. Under `group_by=client` the request's client class, `-`
    /// for a row folded before the class was recorded. Under `group_by=speed` the speed the attempts' answers
    /// reported, `fast` or `standard`, `standard` for an answer that named none whatever the request asked for. A
    /// request no attempt answered files under the speed its last attempt reported, else under `fast` when it asked
    /// for `fast` and under `standard` otherwise.
    pub key: String,
    /// Under `group_by=key` an earlier holder's rows read `<id> (deleted)` whatever the generation, the creation
    /// instant being `key_created_at`'s to format. Under `group_by=model` the empty key, the requests the router
    /// rejected before routing, reads `no model`. Under `group_by=client` the `-` row reads `unattributed`. Under
    /// `group_by=classifier` the `-` row reads `none`.
    pub label: String,
    /// The key's id under `group_by=key`. Absent for another dimension.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
    /// An earlier holder's `created_at` in Unix milliseconds, 0 for a generation with no creation time. The label
    /// carries no instant, so a reader formats this one in its own zone. Absent for the key that holds the id now and
    /// for another dimension.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_created_at: Option<i64>,
    pub requests: u64,
    pub succeeded: u64,
    pub rate_limited: u64,
    pub failed: u64,
    pub exhausted: u64,
    pub rejected: u64,
    pub attempts: u64,
    pub failovers: u64,
    pub incomplete: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub uncached: u64,
    pub expired_between_turns: u64,
    pub tokens: TokenSplitView,
    pub cost: CostSplitView,
    pub cost_usd: f64,
    /// By error code.
    pub errors: BTreeMap<String, u64>,
    /// The refused requests of the group, the sum of `refusals`, not in `requests`.
    pub refused: u64,
    /// The refused requests of the group by error code.
    pub refusals: BTreeMap<String, u64>,
    pub cache_miss_reasons: BTreeMap<String, u64>,
    pub histograms: HistogramsView,
    pub percentiles: PercentilesView,
    pub kept_warm: KeptWarmView,
    /// `ok`, `plan_ok`, `failed`, `held`, `migrated`, and `skipped`, where `held` counts the probes the router refused
    /// with `503 ledger_behind` while its ledger caught up, outside `failed`, the row's `requests` and its error codes.
    pub probes: ProbeCountsView,
    pub probe_cost_usd: f64,
    pub wasted_probes: u64,
    pub wasted_probe_usd: f64,
    pub wasted_probe_reasons: BTreeMap<String, u64>,
    /// Where each group holds one account's rows, under `group_by=account` or with an `accounts` filter naming
    /// exactly one account, a subscription account's plan-funded rows priced at their hour under the account tier
    /// each row recorded when it landed and summed, keyed by the account's window ids (`five_hour`, `seven_day` and
    /// the model's own week where the model has one), the probe rows joining under the `include_probes` rule, and a
    /// window absent where a row could not be priced on it. Null for a group whose rows recorded no tier (an API key
    /// account's group, a subscription account whose profile was not yet read), and under every grouping but
    /// `account` without such a filter, since shares of different accounts' windows do not add. Bounded by the
    /// account's shared window set as the router holds it now: a shared window the set lacks is absent from the map,
    /// the model's own week staying.
    pub cost_shares: Option<BTreeMap<String, f64>>,
    /// The plan-funded requests the shares and the equivalent were summed over, under every grouping.
    pub plan_requests: u64,
    /// The plan-funded output tokens the shares and the equivalent were summed over, under every grouping.
    pub plan_output_tokens: u64,
    /// The plan-funded rows as the Pro-week figure, whatever tier served them, summed under every grouping, the probe
    /// rows joining under the `include_probes` rule: null where a plan-funded row with tokens priced nothing, a model
    /// without a weight, and 0 for a group with no plan-funded row.
    pub cost_equivalent: Option<f64>,
    /// The plan-funded rows' cache reads and writes in the Pro-week figure, null as `cost_equivalent` is.
    pub cache_equivalent: Option<CacheEquivalentView>,
    /// The plan-funded probe rows and their wasted counts in the Pro-week figure, the probe rows always joining, null
    /// where one of them priced nothing.
    pub probe_equivalent: Option<ProbeEquivalentView>,
    /// The kept-warm requests' saving, gross of the probes: the write each count avoided at its TTL's rate less the
    /// read it paid, summed over the group's rows.
    pub saving_usd: f64,
    /// The saving in the Pro-week figure, under every grouping, null where a kept-warm count priced nothing.
    pub saving_equivalent: Option<f64>,
    /// The dollars of the billed rows alone (fast mode, overage, API key accounts and usage-based seats), a part of
    /// `cost_usd`.
    pub billed_usd: f64,
    /// `billed_usd` by token kind.
    pub billed: CostSplitView,
    /// The plan-funded rows' Pro-week figure by token kind, `input`, `cache_read`, `cache_write` and `output`, null
    /// where a row priced nothing, summing to `cost_equivalent`.
    pub equivalent_split: Option<BTreeMap<String, f64>>,
}

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TokenSplitView {
    pub cache_read: u64,
    pub cache_write_1h: u64,
    pub cache_write_5m: u64,
    pub output: u64,
    /// The share of `output` spent thinking.
    pub thinking: u64,
    pub uncached_input: u64,
    /// Server-tool calls.
    pub web_fetch: u64,
    /// Server-tool calls.
    pub web_search: u64,
}

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CostSplitView {
    pub cache_read: f64,
    pub cache_write: f64,
    pub input: f64,
    pub output: f64,
}

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct HistogramsView {
    pub duration_ms: Vec<u64>,
    pub output_tps: Vec<u64>,
    pub ttfb_ms: Vec<u64>,
    pub ttft_ms: Vec<u64>,
}

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct QuartetView {
    pub p50: Option<f64>,
    pub p90: Option<f64>,
    pub p95: Option<f64>,
    pub p99: Option<f64>,
}

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PercentilesView {
    pub duration_ms: QuartetView,
    pub output_tps: QuartetView,
    pub ttfb_ms: QuartetView,
    pub ttft_ms: QuartetView,
}

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeptWarmView {
    pub requests: u64,
    /// The kept-warm requests' cache reads booked under any TTL but an hour.
    pub tokens_5m: u64,
    /// The kept-warm requests' cache reads booked under an hour's TTL.
    pub tokens_1h: u64,
    /// The plan-funded kept-warm requests, a part of `requests`, so a reader tells an unpriced plan-funded credit from
    /// billed ones.
    pub plan_requests: u64,
    /// Their cache reads, a part of `tokens_5m`.
    pub plan_tokens_5m: u64,
    /// Their cache reads, a part of `tokens_1h`.
    pub plan_tokens_1h: u64,
}

/// The read and write halves of a group's cache in the Pro-week figure.
#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CacheEquivalentView {
    pub read: f64,
    pub write: f64,
}

/// The probe rows in the Pro-week figure and the wasted probes' own counts priced in it, 0 where none was wasted.
#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProbeEquivalentView {
    pub cost: f64,
    pub wasted: f64,
}

/// One bucket of the series: the groups of the bucket, each a breakdown row.
#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SeriesPointView {
    pub at: i64,
    pub groups: Vec<BreakdownRow>,
}

/// One hour of the dashboard's last day.
#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct HourPointView {
    pub at: i64,
    /// The dollars of the hour's billed rows alone (fast mode, overage, API key accounts and usage-based seats), a
    /// part of `cost`.
    pub billed: f64,
    pub cost: f64,
    /// The hour's plan-funded rows as the Pro-week figure, whatever tier served them, null where a row priced nothing.
    pub equivalent: Option<f64>,
    pub errors: u64,
    pub refused: u64,
    pub requests: u64,
    pub tokens: u64,
}

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct FastCells {
    pub input_per_m: f64,
    pub cache_write_5m_per_m: f64,
    pub cache_write_1h_per_m: f64,
    pub cache_read_per_m: f64,
    pub output_per_m: f64,
}

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct LongContextCells {
    /// The prompt length in tokens past which the five cells apply.
    pub over: u64,
    pub input_per_m: f64,
    pub cache_write_5m_per_m: f64,
    pub cache_write_1h_per_m: f64,
    pub cache_read_per_m: f64,
    pub output_per_m: f64,
}

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
const FIXTURES: &[(&str, &str, &str)] = &[
    fixture!("session", "200"),
    fixture!("accounts", "200"),
    fixture!("account", "200"),
    fixture!("account_catalog", "200"),
    fixture!("account_limit_reset_use", "200"),
    fixture!("account_limit_resets", "200"),
    fixture!("account_quota", "200"),
    fixture!("account_usage", "200"),
    fixture!("analytics_accounts", "200"),
    fixture!("analytics_breakdown", "200"),
    fixture!("analytics_breakdown_accounts", "200"),
    fixture!("analytics_breakdown_speed", "200"),
    fixture!("analytics_budgets", "200"),
    fixture!("analytics_burn", "200"),
    fixture!("analytics_guardrails", "200"),
    fixture!("analytics_placements", "200"),
    fixture!("analytics_reconciliations", "200"),
    fixture!("analytics_refusal_categories", "200"),
    fixture!("analytics_rewrites", "200"),
    fixture!("analytics_series", "200"),
    fixture!("analytics_series_accounts", "200"),
    fixture!("analytics_unused_allowance", "200"),
    fixture!("attention", "200"),
    fixture!("dashboard", "200"),
    fixture!("guardrails", "200"),
    fixture!("guardrail_created", "201"),
    fixture!("guardrail_presets", "200"),
    fixture!("guardrail_show", "200"),
    fixture!("guardrail_test", "200"),
    fixture!("keepalive_overview", "200"),
    fixture!("keepalive_recipes", "200"),
    fixture!("keys", "200"),
    fixture!("key_created", "201"),
    fixture!("key_models", "200"),
    fixture!("replay", "200"),
    fixture!("replay_held", "503"),
    fixture!("pairing_started", "201"),
    fixture!("oauth_session", "200"),
    fixture!("pools", "200"),
    fixture!("pool_traffic", "200"),
    fixture!("pricing", "200"),
    fixture!("requests", "200"),
    fixture!("request", "200"),
    fixture!("request_bodies", "200"),
    fixture!("decision", "200"),
    fixture!("routing_log", "200"),
    fixture!("sessions", "200"),
    fixture!("sessions_costs", "200"),
    fixture!("settings", "200"),
    fixture!("system", "200"),
];

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

/// The header naming the commit the binary was built from on every admin answer.
pub(crate) const BUILD_HEADER: &str = "x-phoebis-build";

/// `build` as the header's value. A hand-set `PHOEBIS_BUILD_COMMIT` can hold a byte no header carries, and such a
/// build answers `unknown` rather than fail every answer.
pub(crate) fn build_value(build: &str) -> HeaderValue {
    HeaderValue::from_str(build).unwrap_or_else(|_| HeaderValue::from_static("unknown"))
}

/// The document's answer. The gate's middleware never runs on this route, and the layer outside both sets the headers
/// every admin answer carries. Its `no-store` serves the document too, which changes with every binary and names the
/// version, so a browser holding an old copy across an upgrade would list operations the new binary no longer serves.
pub(crate) fn document(text: Bytes) -> Response {
    (
        StatusCode::OK,
        [(CONTENT_TYPE, HeaderValue::from_static("application/json"))],
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
        let (_, _, text) = super::FIXTURES
            .iter()
            .find(|(name, ..)| *name == stem)
            .unwrap_or_else(|| panic!("{stem}.json is not among the embedded fixtures"));
        let file: Value = serde_json::from_str(text).unwrap();
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

    /// Every value at `path` in the fixture's body (a key per segment, `*` for each element of an array or each
    /// value of an object) round trips through `T` with no unknown field: the mirror of a type another crate owns is
    /// held to that type here, since the view's own field keeps the source type and serializes it as it is.
    pub(crate) fn round_trips_each<T: DeserializeOwned + Serialize>(stem: &str, path: &[&str]) {
        fn walk<'a>(v: &'a Value, path: &[&str], out: &mut Vec<&'a Value>) {
            match path.split_first() {
                None => out.push(v),
                Some((&"*", rest)) => match v {
                    Value::Array(items) => items.iter().for_each(|e| walk(e, rest, out)),
                    Value::Object(members) => members.values().for_each(|e| walk(e, rest, out)),
                    _ => panic!("{v} is neither an array nor an object"),
                },
                Some((key, rest)) => walk(&v[*key], rest, out),
            }
        }
        let body = fixture(stem);
        let mut values = Vec::new();
        walk(&body, path, &mut values);
        assert!(!values.is_empty(), "{stem}.json holds nothing at {path:?}");
        for value in values {
            let typed: T = serde_json::from_value(value.clone())
                .unwrap_or_else(|e| panic!("{stem}.json at {path:?} does not fit the mirror: {e}"));
            assert_eq!(
                &serde_json::to_value(&typed).unwrap(),
                value,
                "{stem}.json at {path:?}"
            );
        }
    }

    /// A fully set `source` serializes and deserializes through the mirror `T` with no unknown field and back to the
    /// same value, so a mirror the capture never reaches is still held to its source type.
    pub(crate) fn round_trips_source<S: Serialize, T: DeserializeOwned + Serialize>(source: S) {
        let value = serde_json::to_value(&source).unwrap();
        let typed: T = serde_json::from_value(value.clone())
            .unwrap_or_else(|e| panic!("the source does not fit the mirror: {e}"));
        assert_eq!(serde_json::to_value(&typed).unwrap(), value);
    }
}

#[cfg(test)]
mod tests {
    use super::round_trip::round_trips_source;
    use super::{ModelTtlView, OverrideView, TypeOverrideView};
    use indexmap::IndexMap;
    use phoebis_ledger::{Override, TypeOverride};
    use phoebis_relay::{CacheTtl, ModelTtl};

    fn every_cell() -> TypeOverride {
        TypeOverride {
            cache_read: Some(0.1),
            cache_write_1h: Some(2.0),
            cache_write_5m: Some(1.25),
            input: Some(1.5),
            output: Some(5.0),
        }
    }

    #[test]
    fn a_model_ttl_with_every_field_set_round_trips_through_its_mirror() {
        round_trips_source::<_, ModelTtlView>(ModelTtl {
            main: CacheTtl::OneHour,
            model: "claude-opus-4-6".to_string(),
            subagent: CacheTtl::FiveMinutes,
        });
    }

    /// The members a source serializes. An all-optional mirror reads a missing key as unset, so a member the source
    /// drops still round trips, and the count of a fully set source is what catches it.
    fn members(source: &impl serde::Serialize) -> usize {
        serde_json::to_value(source)
            .unwrap()
            .as_object()
            .map_or(0, serde_json::Map::len)
    }

    #[test]
    fn an_override_with_both_members_set_round_trips_through_its_mirror() {
        let source = Override {
            type_multiplier: Some(every_cell()),
            weight_by_window: Some(IndexMap::from([
                ("five_hour".to_string(), 1.5),
                ("seven_day".to_string(), 0.75),
            ])),
        };
        assert_eq!(
            members(&source),
            2,
            "the two members of Override, each set here"
        );
        round_trips_source::<_, OverrideView>(source);
    }

    #[test]
    fn a_type_override_with_every_cell_set_round_trips_through_its_mirror() {
        assert_eq!(
            members(&every_cell()),
            5,
            "the five cells of TypeOverride, each set here"
        );
        round_trips_source::<_, TypeOverrideView>(every_cell());
    }

    #[test]
    fn a_build_holding_a_byte_no_header_carries_answers_unknown_in_the_build_header() {
        assert_eq!(super::build_value("0123abc\n4567def"), "unknown");
        assert_eq!(
            super::build_value("0123abc4567def"),
            "0123abc4567def",
            "a build every header carries is its own value"
        );
    }
}
