//! The answers for a pool with nothing to select: a 429 shaped like one rate-limited account so the client's
//! automatic continue arms, a 503 only an operator can clear, and the two 429s of a fast request no upstream's
//! fast lane can take.

use axum::http::StatusCode;
use axum::response::Response;
use phoebis_routing::Exhaustion;

use crate::anthropic_error;
use crate::errors::MODEL_REJECTED;

pub fn exhaustion_response(exhaustion: &Exhaustion, now: i64, request_id: &str) -> Response {
    match exhaustion {
        Exhaustion::ModelRejected => MODEL_REJECTED.response(request_id),
        Exhaustion::Untimed => anthropic_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
            "accounts_unavailable",
            "no account in this pool can serve requests",
            request_id,
        ),
        Exhaustion::Timed { deadline, window } => {
            let mut response = anthropic_error(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "accounts_exhausted",
                "every account in this pool is rate limited",
                request_id,
            );
            let seconds = whole_seconds(*deadline, now);
            // The client treats an unknown claim as no claim and then never arms its continue, so the one
            // window name it cannot know is rendered as the five-hour window; the deadline is what matters.
            // A probe's "budget" window is the selector's word to the keepalive, never a claim of its own, so
            // the caller sees the same overage claim a request at a budget-refused pool gets.
            let claim = if window == "retry_after" {
                "five_hour"
            } else if window == "budget" {
                "overage"
            } else {
                window.as_str()
            };
            let headers = response.headers_mut();
            headers.insert("retry-after", seconds.to_string().parse().expect("digits"));
            headers.insert(
                "anthropic-ratelimit-unified-status",
                "rejected".parse().expect("ascii"),
            );
            headers.insert(
                "anthropic-ratelimit-unified-reset",
                (deadline / 1_000).to_string().parse().expect("digits"),
            );
            if let Ok(value) = claim.parse() {
                headers.insert("anthropic-ratelimit-unified-representative-claim", value);
            }
            response
        }
        Exhaustion::FastLane { deadline } => {
            let mut response = anthropic_error(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "fast_lane_exhausted",
                "every account in this pool is at its fast limit",
                request_id,
            );
            response.headers_mut().insert(
                "retry-after",
                whole_seconds(*deadline, now)
                    .to_string()
                    .parse()
                    .expect("digits"),
            );
            response
        }
        Exhaustion::FastCredits { reason } => {
            let mut response = anthropic_error(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "fast_credits_unavailable",
                "no account in this pool can draw usage credits for fast mode",
                request_id,
            );
            // The reason is the upstream's own word, relayed as it came; one no header can carry is left off.
            if let Ok(value) = reason.parse() {
                response
                    .headers_mut()
                    .insert("anthropic-ratelimit-unified-overage-disabled-reason", value);
            }
            response
        }
    }
}

/// The wait until `deadline` in whole seconds, rounded up and at least one, as `Retry-After` carries it.
fn whole_seconds(deadline: i64, now: i64) -> i64 {
    deadline.saturating_sub(now).max(1_000).saturating_add(999) / 1_000
}
