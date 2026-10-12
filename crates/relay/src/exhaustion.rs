//! The answers for a pool with nothing to select: a 429 shaped like one rate-limited account so the client's
//! automatic continue arms, a 503 only an operator can clear, the same 503 with a one-second retry for a key moved
//! to another pool mid-flight, and the two 429s of a fast request no upstream's fast lane can take.

use axum::response::Response;
use phoebis_routing::{Exhaustion, BUDGET_WINDOW};

use crate::anthropic_error;
use crate::errors::{exhaustion_status_code, MODEL_REJECTED};

pub fn exhaustion_response(exhaustion: &Exhaustion, now: i64, request_id: &str) -> Response {
    // A pool at its cap is answered by `accounts_at_cap`, so no exhaustion here is read at the cap.
    let status = exhaustion_status_code(exhaustion, false);
    match exhaustion {
        Exhaustion::ModelRejected => MODEL_REJECTED.response(request_id),
        Exhaustion::Untimed | Exhaustion::Pinned { .. } => anthropic_error(
            status,
            "api_error",
            "accounts_unavailable",
            "no account in this pool can serve requests",
            request_id,
        ),
        Exhaustion::Timed { deadline, window } => {
            let mut response = anthropic_error(
                status,
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
            } else if window == BUDGET_WINDOW {
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
                status,
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
                status,
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

/// The answer for a key moved to another pool between its admission and its selection: the untimed exhaustion's
/// body, naming no account and no reason, with a one-second `Retry-After`, since the next request admits under the
/// key's new pool.
pub(crate) fn pool_moved_response(request_id: &str) -> Response {
    let mut response = exhaustion_response(&Exhaustion::Untimed, 0, request_id);
    response
        .headers_mut()
        .insert("retry-after", "1".parse().expect("digits"));
    response
}

/// The wait until `deadline` in whole seconds, rounded up and at least one, as `Retry-After` carries it.
fn whole_seconds(deadline: i64, now: i64) -> i64 {
    deadline.saturating_sub(now).max(1_000).saturating_add(999) / 1_000
}
