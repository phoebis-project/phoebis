//! A key holder's view of their own key: the usage the rollups hold under its id, on the proxy listener behind
//! the key itself, so nobody needs the admin session to see what a key spent.

use std::sync::Arc;

use axum::extract::State;
use axum::http::header::{ACCEPT, CONTENT_SECURITY_POLICY, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::map_response;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use phoebis_oauth::Clock;
use phoebis_relay::{anthropic_error, authenticated_key, new_request_id, no_store_answer};
use phoebis_routing::{RouterHandle, DAY_MS, WEEK_MS};
use rusqlite::Connection;
use serde::Serialize;

use crate::price::price_usd;
use crate::query::hour_ceil;
use crate::rollup::{hour_of, read_rollups_for_key, RollupRow};
use crate::{LedgerDb, LedgerError, PriceRevisions, Pricing};

pub const USAGE_PATH: &str = "/v1/usage";

/// One window of a key's own usage, summed from the hourly rollups filed under its `(key_id, key_generation)`;
/// probe rows are left out as the key list's totals leave them out.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct KeyUsageWindow {
    pub from: i64,
    pub to: i64,
    pub requests: u64,
    pub succeeded: u64,
    /// Every request that was neither a success nor incomplete: failed, rate limited, exhausted, and rejected.
    pub failed: u64,
    /// The committed answers whose body ended before its end, counted apart from `failed` since the client
    /// received part of the answer.
    pub incomplete: u64,
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

/// The two windows the self-service answer carries, from one read: the day is the tail of the week's own
/// rows, so a caller polling the route costs one scan of its own key rather than two of every key's.
///
/// The key is bound in SQL from the authenticated id and from nothing the caller sent: the ceiling on what
/// this route may show is that one argument. The ledger files what it could not attribute under `-`, which
/// the admin API's slug rule (`[a-z0-9][a-z0-9-]{0,63}`) cannot produce, so no key ever matches it.
///
/// The generation is the presenting key's own `created_at`, so an id an earlier key used answers only what
/// this key spent, whole hours included: nothing is left out and each window starts where its name says.
///
/// Each row's dollars price from its counts at the revision in force at its hour, once for both windows, and probe
/// rows are left out as the key list's totals leave them out.
pub fn key_usage_windows(
    conn: &Connection,
    key_id: &str,
    key_generation: i64,
    now: i64,
    prices: &PriceRevisions,
) -> Result<(KeyUsageWindow, KeyUsageWindow), LedgerError> {
    let day_hour = hour_of(now - DAY_MS);
    let week_hour = hour_of(now - WEEK_MS);
    let rows = read_rollups_for_key(conn, key_id, key_generation, week_hour, hour_ceil(now))?;
    let (mut day, mut week) = (
        WindowSum::new(day_hour, now),
        WindowSum::new(week_hour, now),
    );
    for row in &rows {
        let usd =
            price_usd(prices, &row.price_key(now), row.usage()).map_or(0.0, |usd| usd.total());
        week.add(row, usd);
        if row.hour >= day_hour {
            day.add(row, usd);
        }
    }
    Ok((day.finish(), week.finish()))
}

/// One window's sums as the rows fold into it, each model summed once under its id.
struct WindowSum {
    window: KeyUsageWindow,
    models: std::collections::BTreeMap<String, ModelUsage>,
}

impl WindowSum {
    fn new(from_hour: i64, to: i64) -> WindowSum {
        WindowSum {
            window: KeyUsageWindow {
                from: from_hour * 1_000,
                to,
                ..KeyUsageWindow::default()
            },
            models: Default::default(),
        }
    }

    fn add(&mut self, row: &RollupRow, usd: f64) {
        let out = &mut self.window;
        out.requests += row.requests;
        out.succeeded += row.succeeded;
        out.failed += row.failed + row.rate_limited + row.exhausted + row.rejected;
        out.incomplete += row.incomplete;
        out.cost_usd += usd;
        add(&mut out.tokens, row);
        let entry = self
            .models
            .entry(row.canonical_model.clone())
            .or_insert_with_key(|model| ModelUsage {
                model: model.clone(),
                ..ModelUsage::default()
            });
        entry.requests += row.requests;
        entry.cost_usd += usd;
        add(&mut entry.tokens, row);
    }

    fn finish(self) -> KeyUsageWindow {
        KeyUsageWindow {
            models: self.models.into_values().collect(),
            ..self.window
        }
    }
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
    pricing: Arc<Pricing>,
    router: RouterHandle,
}

/// The self-service route on the proxy listener: `GET /v1/usage` behind the caller's own API key.
pub fn usage_router(
    db: Arc<LedgerDb>,
    router: RouterHandle,
    clock: Clock,
    pricing: Arc<Pricing>,
) -> Router {
    Router::new()
        .route(USAGE_PATH, get(usage))
        .layer(map_response(no_store_answer))
        .with_state(Arc::new(UsageState {
            clock,
            db,
            pricing,
            router,
        }))
}

async fn usage(State(state): State<Arc<UsageState>>, headers: HeaderMap) -> Response {
    let request_id = new_request_id();
    let now = (state.clock)();
    let html = wants_html(headers.get(ACCEPT).and_then(|v| v.to_str().ok()));
    let snapshot = state.router.snapshot();
    let row = match authenticated_key(&snapshot, &headers, now, &request_id) {
        Ok(row) => row,
        Err(refused) => return refused,
    };
    let generation = row.created_at;
    let key = KeyView {
        id: row.id.clone(),
        label: row.label.clone(),
        pool_id: row.pool_id.clone(),
    };
    let key_id = key.id.clone();
    let prices = state.pricing.revisions();
    let (day, week) = match state
        .db
        .read(move |c| key_usage_windows(c, &key_id, generation, now, &prices))
        .await
    {
        Ok(windows) => windows,
        Err(error) => {
            tracing::warn!(%error, "the usage route could not read the ledger");
            // The relay's own shape, so this answer carries the request id and the error code header every
            // other refusal on the proxy listener carries.
            return anthropic_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                "usage_unavailable",
                "the usage ledger could not be read",
                &request_id,
            );
        }
    };
    let answer = UsageAnswer {
        key,
        now,
        windows: Windows { day, week },
    };
    if html {
        return (
            StatusCode::OK,
            [
                (CONTENT_TYPE, "text/html; charset=utf-8"),
                (
                    CONTENT_SECURITY_POLICY,
                    "default-src 'none'; style-src 'unsafe-inline'",
                ),
                (X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            render_html(&answer),
        )
            .into_response();
    }
    (
        StatusCode::OK,
        [
            (CONTENT_TYPE, "application/json"),
            (X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        serde_json::to_string(&answer).unwrap_or_else(|_| "{}".into()),
    )
        .into_response()
}

/// Whether the caller asked for the page rather than the document. The header is read as media ranges with
/// their quality; a type's effective quality is that of the most specific range matching it (the type itself,
/// then its `<kind>/*`, then `*/*`), and HTML is chosen only when `text/html` is worth more than zero and more
/// than `application/json`. A tie, a missing header, or a wildcard-only header reads as JSON, so a script that
/// sets nothing gets the document.
fn wants_html(accept: Option<&str>) -> bool {
    let Some(accept) = accept else {
        return false;
    };
    // One media range per comma-separated member: the first `;` piece is the type (trimmed, lowercased, so
    // `TEXT/HTML` is `text/html`), a `q=` parameter its quality, absent 1.0, unparsable 0.
    let ranges: Vec<(String, f32)> = accept
        .split(',')
        .filter_map(|range| {
            let mut pieces = range.split(';');
            let media = pieces.next()?.trim().to_ascii_lowercase();
            if media.is_empty() {
                return None;
            }
            let quality = pieces
                .map(str::trim)
                .find_map(|p| p.strip_prefix("q=").or_else(|| p.strip_prefix("Q=")))
                .map_or(1.0, |q| q.parse::<f32>().unwrap_or(0.0));
            Some((media, quality))
        })
        .collect();
    // The most specific matching range decides, as RFC 9110 orders them; an exact type is never matched by
    // a substring, so `text/xhtml` says nothing about `text/html`.
    let quality_of = |media: &str| -> f32 {
        let kind = media.split_once('/').map_or(media, |(kind, _)| kind);
        let wildcard = format!("{kind}/*");
        [media, wildcard.as_str(), "*/*"]
            .iter()
            .find_map(|wanted| {
                ranges
                    .iter()
                    .find(|(m, _)| m.as_str() == *wanted)
                    .map(|(_, q)| *q)
            })
            .unwrap_or(0.0)
    };
    let html = quality_of("text/html");
    html > 0.0 && html > quality_of("application/json")
}

/// The five characters HTML needs escaped, so a key label chosen by an operator never becomes markup.
fn escape_html(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn window_html(title: &str, w: &KeyUsageWindow) -> String {
    let mut rows = String::new();
    for m in &w.models {
        rows.push_str(&format!(
            "<tr><td>{}</td><td class=\"n\">{}</td><td class=\"n\">{}</td><td class=\"n\">{}</td><td class=\"n\">{}</td><td class=\"n\">{}</td><td class=\"n\">${:.4}</td></tr>",
            escape_html(&m.model), m.requests, m.tokens.input, m.tokens.output, m.tokens.cache_read, m.tokens.cache_write, m.cost_usd
        ));
    }
    format!(
        "<section><h2>{title}</h2><p class=\"sum\">{} requests, {} succeeded, {} failed, {} incomplete, ${:.4}</p>\
         <table><thead><tr><th>Model</th><th>Requests</th><th>Input</th><th>Output</th><th>Cache read</th><th>Cache write</th><th>Cost</th></tr></thead>\
         <tbody>{rows}</tbody></table></section>",
        w.requests, w.succeeded, w.failed, w.incomplete, w.cost_usd
    )
}

/// When the figures were read, for a page the holder saves to a file and reads later; the raw milliseconds
/// stand in for an instant no calendar can render.
fn generated_at(now: i64) -> String {
    jiff::Timestamp::from_millisecond(now)
        .map(|t| t.to_string())
        .unwrap_or_else(|_| now.to_string())
}

fn render_html(answer: &UsageAnswer) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Usage of {label}</title>\
         <style>body{{font:15px/1.45 system-ui,sans-serif;margin:2rem auto;max-width:56rem;padding:0 1rem;color:#1c1c1c;background:#fff}}\
         h1{{font-size:1.5rem;margin:0 0 .25rem}}h2{{font-size:1.15rem;margin:1.5rem 0 .25rem}}\
         .key{{color:#555;margin:0 0 1rem}}.sum{{margin:0 0 .5rem}}table{{border-collapse:collapse;width:100%}}\
         th,td{{border-bottom:1px solid #ddd;padding:.35rem .5rem;text-align:left}}.n{{text-align:right;font-variant-numeric:tabular-nums}}\
         @media (prefers-color-scheme:dark){{body{{color:#e6e6e6;background:#151515}}th,td{{border-color:#333}}.key{{color:#aaa}}}}</style></head>\
         <body><h1>Usage of {label}</h1><p class=\"key\">key <code>{id}</code>, pool <code>{pool}</code>, \
         figures from the router's hourly rollups with probes left out, generated <time>{now}</time></p>\
         {day}{week}</body></html>",
        label = escape_html(&answer.key.label),
        id = escape_html(&answer.key.id),
        pool = escape_html(&answer.key.pool_id),
        now = escape_html(&generated_at(answer.now)),
        day = window_html("Last 24 hours", &answer.windows.day),
        week = window_html("Last 7 days", &answer.windows.week),
    )
}

#[cfg(test)]
mod tests {
    use super::{wants_html, window_html, KeyUsageWindow};

    #[test]
    fn a_windows_sum_line_names_its_incomplete_count_beside_failed() {
        let window = KeyUsageWindow {
            requests: 6,
            succeeded: 3,
            failed: 2,
            incomplete: 1,
            cost_usd: 0.25,
            ..KeyUsageWindow::default()
        };
        let html = window_html("Last 24 hours", &window);
        assert!(
            html.contains("2 failed") && html.contains("1 incomplete"),
            "{html}"
        );
    }

    #[test]
    fn wants_html_weighs_the_accept_headers_media_ranges() {
        let cases: &[(Option<&str>, bool)] = &[
            (None, false),
            (Some("*/*"), false),
            (Some("application/json"), false),
            (Some("application/json, text/html"), false),
            (Some("text/html"), true),
            (Some("text/html, application/json;q=0.9"), true),
            (Some("text/html;q=0, application/json"), false),
            (Some("application/json;q=0.5, text/html"), true),
            (Some("TEXT/HTML"), true),
            (Some("text/xhtml"), false),
            (Some("text/*"), true),
            (Some("text/*;q=0.5, application/json"), false),
        ];
        for (input, expected) in cases {
            assert_eq!(wants_html(*input), *expected, "{input:?}");
        }
    }
}
