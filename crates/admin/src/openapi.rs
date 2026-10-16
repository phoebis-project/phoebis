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

/// The document's static half: the info block, the one relative server, the seventeen tags in the API page's area order
/// (`docs/site/api.md`), the bearer requirement on every operation, and the four shared responses. The paths join it in
/// `router()`.
#[derive(OpenApi)]
#[openapi(
    info(title = "Phoebis Router admin API"),
    servers((url = "/", description = "This router's admin listener")),
    tags(
        (name = "Accounts"),
        (name = "Analytics", description = "`GET /admin/analytics/breakdown` and `/series` share `range` \
            (default `24h`), `group_by` (`account`, `all`, `client`, `effort`, `key`, `kind`, `model`, `pool`, \
            `request_class`, `speed`, or `user_agent`), and filters `keys`, `pools`, `accounts`, `models`, `kinds` \
            (comma-separated lists), `user_agents` (newline-separated patterns over the agent's first product token, \
            `claude-cli/2.1.286` for `claude-cli/2.1.286 (external, cli)`: `*` matches any run of characters, `?` one, \
            and a backslash quotes either, so a value with no wildcard matches in full), `user_agent_groups` \
            (newline-separated patterns. Under `group_by=user_agent` each names a group, a row falls in the first that \
            matches its agent's token, and the rest group under `other`), `clients` (newline-separated patterns over \
            the client class, the grammar and bound of `user_agents`), and `include_probes` (`true`/`1`), and \
            `/accounts` takes `range` alone. `/breakdown` and `/series` also take `funding` (`billed` keeps the \
            overage and fast rows and every row an API key account served, `plan` the rows a subscription window paid \
            for and the rows of a request no account served. Each row files under the funding its serving attempt \
            had when the ledger recorded it, so deleting an account, or recreating its id as the other kind, moves \
            none of its rows. A probe's cost files under the funding of the probe's own request, while a recipe's \
            settlement, which carries the probes' waste, files under the kind its record carries: `billed` for an API \
            key account, else `fast` for a fast recipe, else `plan`. Absent keeps every row. The two subsets \
            partition the rows). `GET /admin/analytics/budgets`, `/burn`, `/placements`, `/reconciliations`, and \
            `/rewrites`, elsewhere under this tag, take their own request shapes."),
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
    /// The mod's lifecycle word: `busy`, `idle`, `parked`, `released` or `unknown`.
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

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeepalivePolicyView {
    pub enabled: bool,
    /// The idle window of the main conversation. 0 resolves to the recipe's break-even, as does any value above
    /// it.
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

// No captured pool carries a per-model override, so the round trip never reaches this type: its fields are held to
// the relay crate's `ModelTtl` by reading the two side by side.
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
    /// The dollars of the billed rows alone (fast mode, overage and API key accounts), a part of `cost_usd`.
    pub billed_usd: f64,
    /// The plan-funded rows as the share of a Max 20x account's two shared windows, whatever tier served them.
    pub equivalent: SharedEquivalentView,
    /// Whether a plan-funded row of the bucket priced at its model tier's latest weight, for a model the cost model
    /// does not weight.
    pub estimated: bool,
}

/// A figure as the share of a Max 20x account's 5-hour and 7-day windows.
#[derive(Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SharedEquivalentView {
    pub five_hour: f64,
    pub seven_day: f64,
}

impl From<&phoebis_ledger::SharedEquivalent> for SharedEquivalentView {
    fn from(equivalent: &phoebis_ledger::SharedEquivalent) -> Self {
        SharedEquivalentView {
            five_hour: equivalent.five_hour,
            seven_day: equivalent.seven_day,
        }
    }
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

/// Every key of the subscription cost model.
#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubscriptionCostsView {
    pub effective: String,
    pub unit: UnitView,
    /// Keyed by the tier the account's profile reports.
    pub capacity: BTreeMap<String, CapacityView>,
    pub peak: PeakView,
    pub type_multiplier: TypeMultiplierView,
    /// Units per plain input token, keyed by undated model id. A null weight, like a model left out, prices at the
    /// weight of its model tier's latest weighted model as an estimate, and a model of no model tier, or of one with
    /// no weighted model, prices nothing.
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
    pub defined_by: DefinedByView,
    pub units_per_window: u64,
}

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DefinedByView {
    pub band: String,
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

// The shipped cost model carries no override, so the round trip never reaches this type or `TypeOverrideView`:
// their fields are held to the ledger's `Override` and `TypeOverride` by reading the two side by side.
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
    /// Under `group_by=key` an earlier holder's rows read `<id> (deleted, created 2026-09-14 09:31 UTC)`, the
    /// generation's date and time at minute precision in UTC, and `<id> (deleted)` at generation 0, which holds no
    /// creation time. Under `group_by=client` the `-` row reads `unattributed`.
    pub label: String,
    /// The key's id under `group_by=key`. Absent for another dimension.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
    /// An earlier holder's `created_at` in Unix milliseconds, 0 for a generation with no creation time. The console
    /// prints it in the viewer's zone. Absent for the key that holds the id now and for another dimension.
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
    pub cache_miss_reasons: BTreeMap<String, u64>,
    pub histograms: HistogramsView,
    pub percentiles: PercentilesView,
    pub kept_warm: KeptWarmView,
    /// `ok`, `failed`, `held`, `migrated`, and `skipped`, where `held` counts the probes the router refused with
    /// `503 ledger_behind` while its ledger caught up, outside `failed`, the row's `requests` and its error codes.
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
    /// `account` without such a filter, since shares of different accounts' windows do not add.
    pub cost_shares: Option<BTreeMap<String, f64>>,
    /// Each `read` and `write`, keyed and summed as `cost_shares` and null under the same rule, the probe rows
    /// joining under the `include_probes` rule.
    pub cache_shares: Option<BTreeMap<String, CacheShareView>>,
    /// Each `cost` and `wasted`, keyed and summed as `cost_shares` and null under the same rule, the probe rows
    /// always joining.
    pub probe_shares: Option<BTreeMap<String, ProbeShareView>>,
    /// The plan-funded requests the shares and the equivalent were summed over, under every grouping.
    pub plan_requests: Option<u64>,
    /// The plan-funded output tokens the shares and the equivalent were summed over, under every grouping.
    pub plan_output_tokens: Option<u64>,
    /// The plan-funded rows priced as the share of a Max 20x account's windows, whatever tier served them, keyed as
    /// `cost_shares` and summed under every grouping, the probe rows joining under the `include_probes` rule, and a
    /// window absent where a row could not be priced on it.
    pub cost_equivalent: BTreeMap<String, f64>,
    /// Each `read` and `write`, keyed and summed as `cost_equivalent`.
    pub cache_equivalent: BTreeMap<String, CacheEquivalentView>,
    /// Each `cost` and `wasted`, keyed and summed as `cost_equivalent`, the probe rows always joining.
    pub probe_equivalent: BTreeMap<String, ProbeEquivalentView>,
    /// The kept-warm requests' saving, gross of the probes: the write each count avoided at its TTL's rate less the
    /// read it paid, summed over the group's rows.
    pub saving_usd: f64,
    /// The saving as a share of the account tier's windows, keyed and summed as `cost_shares` and null under the same
    /// rule.
    pub saving_shares: Option<BTreeMap<String, f64>>,
    /// The saving in the 20x window equivalent per window, under every grouping, a window absent where a row could not
    /// be priced on it.
    pub saving_equivalent: BTreeMap<String, f64>,
    /// The dollars of the billed rows alone (fast mode, overage and API key accounts), a part of `cost_usd`.
    pub billed_usd: f64,
    /// `billed_usd` by token kind.
    pub billed: CostSplitView,
    /// The plan-funded rows' 7-day equivalent by token kind, `input`, `cache_read`, `cache_write` and `output`,
    /// which sum to `cost_equivalent`'s `seven_day`. Empty where that window is.
    pub equivalent_split: BTreeMap<String, f64>,
    /// Whether a plan-funded row of the group priced at its model tier's latest weight, for a model the cost model
    /// does not weight.
    pub estimated: bool,
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
}

/// The read and write halves of a window's cache share, as fractions of the window.
#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CacheShareView {
    pub read: f64,
    pub write: f64,
}

/// The probe rows' share of a window and the wasted probes' part of it.
// No captured row carries a probe share, so the round trip never reaches this type: its fields are held to the
// ledger's `ProbeShare` by reading the two side by side.
#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProbeShareView {
    pub cost: f64,
    /// The wasted probes' part of the probe share, by the group's wasted-probe dollars over its plan-funded
    /// probe-cost dollars, null while those dollars are zero.
    pub wasted: Option<f64>,
}

/// The read and write halves of a window's equivalent.
#[derive(Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CacheEquivalentView {
    pub read: f64,
    pub write: f64,
}

/// The probe rows' equivalent of a window and the wasted probes' own counts priced on it, 0 where none was wasted.
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
    /// The dollars of the hour's billed rows alone (fast mode, overage and API key accounts), a part of `cost`.
    pub billed: f64,
    pub cost: f64,
    /// The hour's plan-funded rows as the share of a Max 20x account's two shared windows, whatever tier served them.
    pub equivalent: SharedEquivalentView,
    pub errors: u64,
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
    fixture!("account_quota", "200"),
    fixture!("account_usage", "200"),
    fixture!("analytics_accounts", "200"),
    fixture!("analytics_breakdown", "200"),
    fixture!("analytics_breakdown_accounts", "200"),
    fixture!("analytics_breakdown_speed", "200"),
    fixture!("analytics_budgets", "200"),
    fixture!("analytics_burn", "200"),
    fixture!("analytics_placements", "200"),
    fixture!("analytics_reconciliations", "200"),
    fixture!("analytics_rewrites", "200"),
    fixture!("analytics_series", "200"),
    fixture!("analytics_series_accounts", "200"),
    fixture!("attention", "200"),
    fixture!("dashboard", "200"),
    fixture!("keepalive_overview", "200"),
    fixture!("keepalive_recipes", "200"),
    fixture!("keys", "200"),
    fixture!("key_created", "201"),
    fixture!("key_models", "200"),
    fixture!("replay", "200"),
    fixture!("replay_held", "503"),
    fixture!("models_state", "200"),
    fixture!("pairing_started", "201"),
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
}

#[cfg(test)]
mod tests {
    use super::round_trip::round_trips;

    #[test]
    fn the_session_answer_round_trips_through_its_declared_type() {
        round_trips::<crate::routes::SessionView>("session");
    }
}
