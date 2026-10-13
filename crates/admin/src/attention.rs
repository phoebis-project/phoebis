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
    /// Null for a drift item whose refused request named no key, a router subject and a deleted pool.
    pub link: Option<AttentionLink>,
    /// The pools the item belongs to: an account's memberships, a pool's own id, the pool of a drift item's key.
    /// Empty for a router subject, a model and a deleted pool.
    pub pools: Vec<String>,
    /// Unix milliseconds: the start of the backoff run, the first failed attempt of a refresh run, the first
    /// exhaustion or refusal of a run, the oldest parked row, the refused certificate reload, 30 days before a
    /// certificate's end, the first plan-funded landing of an unweighted model's run, or for the two account states
    /// and a degraded writer the first read of this process that saw the state.
    pub since: Option<i64>,
    /// `account` and `label`, or `account`, `failures`, `last_error` and `outcome`, or `account`, `backoff_since`,
    /// `next_attempt_at` and `rate_limited_in_a_row`, or `pool` and `key`, or `client_version`, `trigger` and `key`,
    /// or `component`, or `accounts`, `count` and `pool`. In that last set the pool is null for a record without
    /// one. The two certificate items carry `cert_file`. `certificate_reload_failed` adds `key_file`, `error` (the
    /// loader's line) and `serving_since` (when the pair the listener still serves was loaded).
    /// `certificate_expiring` adds `expires` (the leaf's end) and `days_left` (whole days from the read, negative
    /// once past). `model_unweighted` carries `model`, `requests` (the plan-funded requests its run served), `tier`
    /// (its model tier) and `weight_of` (the tier's latest weighted model, whose weight prices it).
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
    /// (`ledger`, `routing`, a listener's pair as `proxy.tls` or `admin.tls`, or `-` for a parked record without a
    /// pool), or a model's undated id.
    pub label: String,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AttentionLink {
    /// `accounts`, `pools`, `keys` or `models`.
    #[schema(example = "accounts")]
    pub page: String,
    /// Where the operator fixes it.
    pub id: String,
}

/// Read the conditions that hold now.
///
/// Answers the operator conditions that hold now, worst first (`pool_exhausted`, `client_drift`, `writer_degraded`,
/// `certificate_reload_failed`, `reauth_required`, `funding_required`, `refresh_failed`, `certificate_expiring`,
/// `ledger_record_parked`, `poll_backoff`, `model_unweighted`) and the oldest first within a kind. The same reader
/// feeds the `phoebis_attention` gauge on the metrics listener, so a scrape and this answer never disagree.
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
    let mut held: Vec<_> = match state.attention.conditions((state.clock)()) {
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

#[cfg(test)]
mod tests {
    use super::Attention;
    use crate::openapi::round_trip::round_trips;

    #[test]
    fn the_attention_fixture_round_trips_through_its_declared_type() {
        round_trips::<Attention>("attention");
    }
}
