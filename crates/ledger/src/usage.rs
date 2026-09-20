//! A key holder's view of their own key: the usage the rollups hold under its id, on the proxy listener behind
//! the key itself, so nobody needs the admin session to see what a key spent.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderValue, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use phoebis_oauth::Clock;
use phoebis_relay::{auth_error_response, key_refusal_response, new_request_id, presented_key};
use phoebis_routing::RouterHandle;
use rusqlite::Connection;
use serde::Serialize;

use crate::query::{apply_filter, group_of, hour_ceil, RollupFilter};
use crate::rollup::{hour_of, read_rollups, RollupRow};
use crate::{LedgerDb, LedgerError};

const DAY_MS: i64 = 86_400_000;

pub const USAGE_PATH: &str = "/v1/usage";

/// One window of a key's own usage, summed from the hourly rollups under its `key_id`; probe rows are left out
/// as the key list's totals leave them out.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct KeyUsageWindow {
    pub from: i64,
    pub to: i64,
    pub requests: u64,
    pub succeeded: u64,
    /// Everything that was not a success: failed, rate limited, exhausted, and rejected.
    pub failed: u64,
    pub tokens: UsageTokens,
    pub cost_usd: f64,
    /// Per canonical model, sorted by model id.
    pub models: Vec<ModelUsage>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct UsageTokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ModelUsage {
    pub model: String,
    pub requests: u64,
    pub tokens: UsageTokens,
    pub cost_usd: f64,
}

fn add(tokens: &mut UsageTokens, row: &RollupRow) {
    tokens.input += row.input_tokens;
    tokens.output += row.output_tokens;
    tokens.cache_read += row.cache_read_tokens;
    tokens.cache_write += row.cache_write_5m_tokens + row.cache_write_1h_tokens;
}

/// The key's usage over `[from, to)`, read from the rollups under its id and nothing else.
pub fn key_usage(
    conn: &Connection,
    key_id: &str,
    from: i64,
    to: i64,
) -> Result<KeyUsageWindow, LedgerError> {
    let rows = read_rollups(conn, hour_of(from), hour_ceil(to))?;
    // The filter is built here from the authenticated key and from nothing the caller sent: the ceiling on
    // what this route may show is this line.
    let filter = RollupFilter {
        keys: vec![key_id.to_string()],
        ..RollupFilter::default()
    };
    let mut out = KeyUsageWindow {
        from,
        to,
        ..KeyUsageWindow::default()
    };
    let mut models: std::collections::BTreeMap<String, ModelUsage> = Default::default();
    for row in apply_filter(rows, &filter) {
        if row.kind == "probe" {
            continue;
        }
        out.requests += row.requests;
        out.succeeded += row.succeeded;
        out.failed += row.failed + row.rate_limited + row.exhausted + row.rejected;
        out.cost_usd += row.estimated_usd;
        add(&mut out.tokens, &row);
        let model = group_of(&row, "model");
        let entry = models.entry(model.clone()).or_insert_with(|| ModelUsage {
            model,
            ..ModelUsage::default()
        });
        entry.requests += row.requests;
        entry.cost_usd += row.estimated_usd;
        add(&mut entry.tokens, &row);
    }
    out.models = models.into_values().collect();
    Ok(out)
}

#[derive(Serialize)]
struct KeyView {
    id: String,
    label: String,
    pool_id: String,
}

#[derive(Serialize)]
struct UsageAnswer {
    key: KeyView,
    now: i64,
    windows: Windows,
}

#[derive(Serialize)]
struct Windows {
    #[serde(rename = "24h")]
    day: KeyUsageWindow,
    #[serde(rename = "7d")]
    week: KeyUsageWindow,
}

struct UsageState {
    clock: Clock,
    db: Arc<LedgerDb>,
    router: RouterHandle,
}

/// The self-service route on the proxy listener: `GET /v1/usage` behind the caller's own API key.
pub fn usage_router(db: Arc<LedgerDb>, router: RouterHandle, clock: Clock) -> Router {
    Router::new()
        .route(USAGE_PATH, get(usage))
        .with_state(Arc::new(UsageState { clock, db, router }))
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn usage(State(state): State<Arc<UsageState>>, request: Request<Body>) -> Response {
    let request_id = new_request_id();
    let now = (state.clock)();
    // The key is resolved exactly as `/v1/messages` resolves it, refusals included, so a wrong key learns
    // nothing here it would not learn there.
    let presented = match presented_key(request.headers()) {
        Ok(presented) => presented,
        Err(error) => return no_store(auth_error_response(&error, &request_id)),
    };
    let snapshot = state.router.snapshot();
    let key = match snapshot.authenticate(presented, now) {
        Ok(key) => KeyView {
            id: key.id.clone(),
            label: key.label.clone(),
            pool_id: key.pool_id.clone(),
        },
        Err(refusal) => return no_store(key_refusal_response(&refusal, &request_id)),
    };
    let key_id = key.id.clone();
    let read = state.db.read(|c| {
        Ok((
            key_usage(c, &key_id, now - DAY_MS, now)?,
            key_usage(c, &key_id, now - 7 * DAY_MS, now)?,
        ))
    });
    let (day, week) = match read {
        Ok(windows) => windows,
        Err(error) => {
            tracing::warn!(%error, "the usage route could not read the ledger");
            return no_store(
                (StatusCode::INTERNAL_SERVER_ERROR, [(CONTENT_TYPE, "application/json")], r#"{"type":"error","error":{"type":"api_error","message":"the usage ledger could not be read"}}"#).into_response(),
            );
        }
    };
    let answer = UsageAnswer {
        key,
        now,
        windows: Windows { day, week },
    };
    no_store(
        (
            StatusCode::OK,
            [(CONTENT_TYPE, "application/json")],
            serde_json::to_string(&answer).unwrap_or_else(|_| "{}".into()),
        )
            .into_response(),
    )
}
