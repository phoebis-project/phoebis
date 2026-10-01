//! `GET /v1/models` and `GET /v1/models/{*id}`: the caller's key answered with what its pool may route to,
//! in the Anthropic list shape, with a `[1m]` twin beside every long-context model.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::Response;
use futures::future::join_all;
use phoebis_routing::listing::{find, listing, page, CatalogRow};
use phoebis_routing::rules::{process, ProcessedRule};
use phoebis_store::{AccountRow, AccountState, ModelRule};
use serde_json::json;

use crate::{
    anthropic_error, auth_error_response, key_refusal_response, new_request_id, presented_key,
    Relay,
};

/// The default `limit`, and the bounds a query outside them is refused for.
const DEFAULT_LIMIT: usize = 20;
const MIN_LIMIT: usize = 1;
const MAX_LIMIT: usize = 1000;

/// What the caller's key is entitled to before a page is cut from it: the processed rule list and the accounts
/// worth asking for a catalog.
struct Access {
    accounts: Vec<AccountRow>,
    long_context_twins: bool,
    processed: Vec<ProcessedRule>,
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn query_of(uri: &Uri) -> HashMap<String, String> {
    uri.query()
        .map(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .into_owned()
                .collect()
        })
        .unwrap_or_default()
}

fn json_response(status: StatusCode, value: serde_json::Value) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(value.to_string()))
        // `value` is built from strings this module owns; serializing it never fails.
        .expect("a models JSON body is always well formed")
}

fn row_json(row: &CatalogRow) -> serde_json::Value {
    json!({
        "id": row.id,
        "type": "model",
        "display_name": row.display_name,
        "created_at": row.created_at,
        "max_input_tokens": row.max_input_tokens,
        "max_tokens": row.max_tokens,
    })
}

fn invalid_limit(request_id: &str) -> Response {
    no_store(anthropic_error(
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        "invalid_limit",
        "limit must be between 1 and 1000",
        request_id,
    ))
}

fn parse_limit(raw: Option<&String>) -> Result<usize, ()> {
    match raw {
        None => Ok(DEFAULT_LIMIT),
        Some(raw) => match raw.parse::<usize>() {
            Ok(limit) if (MIN_LIMIT..=MAX_LIMIT).contains(&limit) => Ok(limit),
            _ => Err(()),
        },
    }
}

/// Authenticates exactly as `GET /v1/usage` does, then gathers what the key's rules and pool membership say the
/// caller may be answered, ahead of the catalog fetch every route needs.
async fn authenticate(
    relay: &Relay,
    headers: &HeaderMap,
    request_id: &str,
) -> Result<Access, Response> {
    let presented = presented_key(headers)
        .map_err(|error| no_store(auth_error_response(&error, request_id)))?;
    let snapshot = relay.router().snapshot();
    let now = (relay.clock)();
    let (key, _) = snapshot
        .authenticate(presented, now)
        .map_err(|refusal| no_store(key_refusal_response(&refusal, request_id)))?;
    let pool_rules: &[ModelRule] = snapshot
        .pool(&key.pool_id)
        .map(|pool| pool.model_rules.as_slice())
        .unwrap_or(&[]);
    let processed = process(&key.model_rules, key.model_rules_mode, pool_rules);
    let accounts = snapshot
        .members(&key.pool_id)
        .iter()
        .filter_map(|member| snapshot.account(&member.account_id))
        .filter(|account| account.enabled && account.state == AccountState::Ready)
        .cloned()
        .collect();
    Ok(Access {
        accounts,
        long_context_twins: key.long_context_twins,
        processed,
    })
}

/// Every candidate member's catalog, fetched concurrently through the router's cache: a cooling-down model
/// still shows, since the listing never reads the routing state, and a member whose fetch failed hands in no
/// rows and is named nowhere in the answer.
async fn caller_listing(relay: &Relay, access: &Access) -> Vec<CatalogRow> {
    let catalog = relay.catalog();
    let members: Vec<Vec<CatalogRow>> =
        join_all(access.accounts.iter().map(|account| catalog.fetch(account)))
            .await
            .into_iter()
            .map(|catalog| catalog.models)
            .collect();
    listing(&members, &access.processed, access.long_context_twins)
}

pub(crate) async fn list(State(relay): State<Arc<Relay>>, request: Request) -> Response {
    let request_id = new_request_id();
    let query = query_of(request.uri());
    let access = match authenticate(&relay, request.headers(), &request_id).await {
        Ok(access) => access,
        Err(response) => return response,
    };
    let limit = match parse_limit(query.get("limit")) {
        Ok(limit) => limit,
        Err(()) => return invalid_limit(&request_id),
    };
    let rows = caller_listing(&relay, &access).await;
    let paged = page(
        &rows,
        limit,
        query.get("after_id").map(String::as_str),
        query.get("before_id").map(String::as_str),
    );
    no_store(json_response(
        StatusCode::OK,
        json!({
            "data": paged.data.iter().map(row_json).collect::<Vec<_>>(),
            "first_id": paged.first_id,
            "last_id": paged.last_id,
            "has_more": paged.has_more,
        }),
    ))
}

pub(crate) async fn show(
    State(relay): State<Arc<Relay>>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let request_id = new_request_id();
    let access = match authenticate(&relay, request.headers(), &request_id).await {
        Ok(access) => access,
        Err(response) => return response,
    };
    let rows = caller_listing(&relay, &access).await;
    match find(&rows, &id) {
        Some(row) => no_store(json_response(StatusCode::OK, row_json(row))),
        None => no_store(anthropic_error(
            StatusCode::NOT_FOUND,
            "not_found_error",
            "model_not_found",
            &format!("model: {id}"),
            &request_id,
        )),
    }
}
