//! The conditions that hold now, worst first: what the dashboard's attention line and a pool card's attention rows
//! draw.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::Value;
use utoipa::ToSchema;

use crate::openapi::StorageFailure;
use crate::routes::failure;
use crate::AdminState;

/// `{"items": []}` when nothing holds.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Attention {
    pub items: Vec<AttentionItem>,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AttentionItem {
    #[schema(example = "pool_exhausted")]
    pub kind: String,
    pub subject: AttentionSubject,
    /// Null for a drift item whose refused request named no key, a router subject, a deleted pool, a routing condition
    /// whose account or pool the registry no longer holds, and an unpriced model.
    pub link: Option<AttentionLink>,
    /// The pools the item belongs to: an account's memberships, a pool's own id, the pool of a drift item's key.
    /// Empty for a router subject, a model, a deleted pool, and a routing condition whose account or pool the
    /// registry no longer holds.
    pub pools: Vec<String>,
    /// Unix milliseconds: the start of the backoff run, the first failed attempt of a refresh run, the first exhaustion
    /// or refusal of a run, the oldest parked row, the refused certificate reload, 3 days before a refresh token's end,
    /// 30 days before a certificate's end, the catalog listing that added an unpriced model or the first plan-funded
    /// landing of its run, whichever came first, the `window_end` of the oldest window the detail's `windows` lists, or
    /// the fold's instant where that end lies after it, for `meter_drift`, the oldest qualifying event of the trailing
    /// 24 hours for `wall_reached`, `destination_spent` and `shed_no_runway`, the own start of the oldest slow-down span
    /// counted for `slowed_long`, which can predate the day for a span open from before it, or for the two account
    /// states, a degraded writer, `keepalive_bus_gaps`, `keepalive_cap` and `expired_between_turns` the first read of
    /// this process that saw the state.
    pub since: Option<i64>,
    /// `account` and `label`, or `account`, `failures`, `last_error` and `outcome`, or `account`, `backoff_since`,
    /// `next_attempt_at` and `rate_limited_in_a_row`, or `pool` and `key`, or `client_version`, `trigger` and `key`, or
    /// `component`, or `accounts`, `count` and `pool`. In that last set the pool is null for a record without one. The
    /// two certificate items carry `cert_file`. `certificate_reload_failed` adds `key_file`, `error` (the loader's
    /// line) and `serving_since` (when the pair the listener still serves was loaded). `certificate_expiring` adds
    /// `expires` (the leaf's end) and `days_left` (whole days from the read, negative once past). `pairing_expiring`
    /// carries `account`, `label`, `expires` (the refresh token's end) and `days_left` (whole days from the read,
    /// negative once past). `model_unpriced` carries `model` and `requests` (the plan-funded requests its run served, 0
    /// for a model a catalog listing added that no request has used yet).
    /// `meter_drift` carries `windows` (per window whose newest reconciled row among
    /// those that ended in the last eight days with a rise of at least 0.05 read a ratio under 0.8 or over 1.25 over a
    /// priced share of at least 0.05, so a newer row with a smaller rise leaves it standing: `window`, `ratio`,
    /// `observed_rise`, `predicted_share`, `window_end` and `requests`),
    /// `source` (`shipped` or `file`, where the cost model came from) and `effective` (the `effective` of the revision
    /// in force as its file wrote it, a date `YYYY-MM-DD` or an instant `YYYY-MM-DDTHH:MM:SSZ`), and names no setting,
    /// since the fix is the cost model file. The other routing conditions carry
    /// `setting`, the names of the settings they read against: `wall_reached` with `walls`, `windows`,
    /// `projection_error` and `early_ms`, read off the newest shedding round on the account on the wall's window inside
    /// the window's length before the wall (`projection_error` one less a projection round's projection, `early_ms`
    /// after a stretch round the milliseconds the newest wall arrived before its window's reset, 0 at or after it, each
    /// null after the other trigger's round and both null with none), `destination_spent` with `moves`, `fits` (the
    /// moves' destination fits with counts) and `windows`, `shed_no_runway` with `rounds` and `accounts`, and
    /// `slowed_long` with `spans` (the slow-down spans counted), `held_ms` (the slow-down's waits of the day's
    /// requests, summed), `held` (the requests held), `bar_ms` (the bar the held time passed, twice the stretch
    /// threshold of the newest counted span's start and at least an hour) and `open` (whether a span still stands).
    /// `shed_no_runway` and `slowed_long` add `setting_values` (the pool's value of each setting `setting` names,
    /// present only for a pool the registry still holds). `keepalive_bus_gaps` carries `gaps` (the engine's gaps since
    /// the start) and `last_at` (the last gap's instant), `keepalive_cap` carries `cap` (`sessions`, `agents` or
    /// `retained`), `count`, `max` and `pct` (`count` over `max` as a fraction, 0 where `max` is 0), and
    /// `expired_between_turns` carries `expired` (the cache entries that expired between turns in the trailing 24
    /// hours).
    #[schema(value_type = Object)]
    pub detail: Value,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AttentionSubject {
    /// `account`, `pool`, `client`, `router` or `model`.
    #[serde(rename = "type")]
    #[schema(example = "account")]
    pub kind: String,
    pub id: String,
    /// The account's or the pool's label, a deleted pool's id, the client version, the router's component
    /// (`ledger`, `routing`, a listener's pair as `proxy.tls` or `admin.tls`, `Keepalive` for the keepalive engine and
    /// its caps, or `-` for a parked record without a pool), or a model's undated id.
    pub label: String,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AttentionLink {
    /// `accounts`, `keys` or `pools`.
    #[schema(example = "accounts")]
    pub page: String,
    /// Where the operator fixes it.
    pub id: String,
}

/// Read the conditions that hold now.
///
/// Answers the operator conditions that hold now, worst first (`pool_exhausted`, `client_drift`, `writer_degraded`,
/// `certificate_reload_failed`, `reauth_required`, `funding_required`, `refresh_failed`, `pairing_expiring`,
/// `certificate_expiring`, `ledger_record_parked`, `keepalive_bus_gaps`, `poll_backoff`, `model_unpriced`,
/// `meter_drift`, `wall_reached`, `destination_spent`, `shed_no_runway`, `slowed_long`, `keepalive_cap`,
/// `expired_between_turns`) and the oldest first within a kind. `meter_drift`, `wall_reached`, `destination_spent`,
/// `shed_no_runway` and `slowed_long` read the ledger: the reconciliation rows the hourly fold writes and the routing
/// log of the trailing 24 hours. `expired_between_turns` reads the ledger's rollups of the trailing 24 hours. The same
/// reader feeds the `phoebis_attention` gauge on the metrics listener, so a scrape and this answer never disagree.
#[utoipa::path(
    get,
    path = "/admin/attention",
    operation_id = "attention_show",
    tag = "Attention",
    responses(
        (status = 200, description = "The conditions that hold now.", body = Attention),
        (status = 500, response = StorageFailure),
    ),
)]
pub(crate) async fn show(State(state): State<Arc<AdminState>>) -> Response {
    // A failed read answers 500 and never an empty list, which the dashboard would draw as the quiet state.
    // The read waits for its ledger connection on the runtime, where a wait holds no blocking worker, and runs on a
    // blocking worker, so it parks no runtime thread.
    let now = (state.clock)();
    let mut held: Vec<_> = match state.attention.conditions(now).await {
        Ok(conditions) => conditions.into_iter().filter(|c| c.holds).collect(),
        Err(error) => {
            tracing::warn!(%error, "the attention read failed, so the route answers 500");
            return failure(StatusCode::INTERNAL_SERVER_ERROR, "storage failure");
        }
    };
    held.sort_by(|a, b| (a.kind, a.since, &a.subject.id).cmp(&(b.kind, b.since, &b.subject.id)));
    let items = held
        .into_iter()
        .map(|c| AttentionItem {
            kind: c.kind.as_str().into(),
            subject: AttentionSubject {
                kind: c.subject.kind.as_str().into(),
                id: c.subject.id,
                label: c.subject.label,
            },
            link: c.link.map(|l| AttentionLink {
                page: l.page.into(),
                id: l.id,
            }),
            pools: c.pools,
            since: c.since,
            detail: c.detail,
        })
        .collect();
    Json(Attention { items }).into_response()
}
