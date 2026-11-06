//! The answers for a pool with nothing to select: a 429 shaped like one rate-limited account so the client's
//! automatic continue arms, a 503 only an operator can clear, the same 503 with a one-second retry for a key moved
//! to another pool mid-flight, and the two 429s of a fast request no upstream's fast lane can take.

use std::borrow::Cow;

use axum::http::HeaderValue;
use axum::response::Response;
use phoebis_routing::{Exhaustion, BUDGET_WINDOW, FIVE_HOUR, RETRY_AFTER_WINDOW};

use crate::errors::{
    exhaustion_status_code, Failure, FailureClass, ACCOUNTS_EXHAUSTED, ACCOUNTS_UNAVAILABLE,
    MODEL_REJECTED,
};
use crate::ratelimit::{
    UNIFIED_OVERAGE_DISABLED_REASON, UNIFIED_REPRESENTATIVE_CLAIM, UNIFIED_RESET, UNIFIED_STATUS,
};

pub fn exhaustion_response(exhaustion: &Exhaustion, now: i64, request_id: &str) -> Response {
    let mut response = exhaustion_failure(exhaustion, now).response(request_id);
    match exhaustion {
        Exhaustion::Timed { deadline, window } => {
            // The client treats an unknown claim as no claim and then never arms its continue, so the one
            // window name it cannot know is rendered as the five-hour window, the deadline being what matters.
            // The spend window keeps its own name `spend` all the same: the client arms no continue on it, which
            // suits a usage-based seat's wait that can run to the month's end. A probe's "budget" window is the
            // selector's word to the keepalive, never a claim of its own, so the caller sees the same overage
            // claim a request at a budget-refused pool gets.
            let claim = if window == RETRY_AFTER_WINDOW {
                FIVE_HOUR
            } else if window == BUDGET_WINDOW {
                "overage"
            } else {
                window.as_str()
            };
            let headers = response.headers_mut();
            headers.insert(UNIFIED_STATUS, HeaderValue::from_static("rejected"));
            headers.insert(UNIFIED_RESET, HeaderValue::from(deadline / 1_000));
            if let Ok(value) = claim.parse() {
                headers.insert(UNIFIED_REPRESENTATIVE_CLAIM, value);
            }
        }
        Exhaustion::FastCredits { reason } => {
            // The reason is the upstream's own word, relayed as it came; one no header can carry is left off.
            if let Ok(value) = reason.parse() {
                response
                    .headers_mut()
                    .insert(UNIFIED_OVERAGE_DISABLED_REASON, value);
            }
        }
        Exhaustion::FastLane { .. }
        | Exhaustion::ModelRejected
        | Exhaustion::Pinned { .. }
        | Exhaustion::Untimed => {}
    }
    response
}

fn exhaustion_failure(exhaustion: &Exhaustion, now: i64) -> Failure {
    let (error_type, message, retry_after) = match exhaustion {
        Exhaustion::ModelRejected => return MODEL_REJECTED,
        Exhaustion::Untimed | Exhaustion::Pinned { .. } => (
            "api_error",
            "no account in this pool can serve requests",
            None,
        ),
        Exhaustion::Timed { deadline, .. } => (
            "rate_limit_error",
            "every account in this pool is rate limited",
            Some(whole_seconds(*deadline, now)),
        ),
        Exhaustion::FastLane { deadline } => (
            "rate_limit_error",
            "every account in this pool is at its fast limit",
            Some(whole_seconds(*deadline, now)),
        ),
        Exhaustion::FastCredits { .. } => (
            "rate_limit_error",
            "no account in this pool can draw usage credits for fast mode",
            None,
        ),
    };
    Failure {
        // A pool at its cap is answered by `accounts_at_cap`, so no exhaustion here is read at the cap.
        status: exhaustion_status_code(exhaustion, false),
        error_type,
        code: exhaustion_code(exhaustion),
        message: Cow::Borrowed(message),
        retry_after,
        no_retry: false,
        class: exhaustion_class(exhaustion),
    }
}

/// The class an exhaustion files under, read by the answer and the request's outcome alike.
pub(crate) fn exhaustion_class(exhaustion: &Exhaustion) -> FailureClass {
    match exhaustion {
        Exhaustion::ModelRejected => MODEL_REJECTED.class,
        // A probe answered on its pinned member's standing is refused for the probe alone, whatever its siblings
        // hold: conclusive, and no exhaustion of the pool for the attention gauge or the rollups to count.
        Exhaustion::Pinned { .. } => FailureClass::Rejected,
        Exhaustion::Timed { .. } | Exhaustion::Untimed => FailureClass::Exhausted,
        Exhaustion::FastCredits { .. } | Exhaustion::FastLane { .. } => FailureClass::Refused,
    }
}

/// The adapter code an exhaustion is answered with, read by the answer's header and the request's outcome alike, so
/// the two cannot disagree.
pub(crate) fn exhaustion_code(exhaustion: &Exhaustion) -> &'static str {
    match exhaustion {
        Exhaustion::ModelRejected => MODEL_REJECTED.code,
        Exhaustion::Untimed | Exhaustion::Pinned { .. } => ACCOUNTS_UNAVAILABLE,
        Exhaustion::Timed { .. } => ACCOUNTS_EXHAUSTED,
        Exhaustion::FastLane { .. } => "fast_lane_exhausted",
        Exhaustion::FastCredits { .. } => "fast_credits_unavailable",
    }
}

/// The answer for a key moved to another pool between its admission and its selection: the untimed exhaustion's
/// body, naming no account and no reason, with a one-second `Retry-After`, since the next request admits under the
/// key's new pool.
pub(crate) fn pool_moved_response(request_id: &str) -> Response {
    Failure {
        retry_after: Some(1),
        // The request is refused for the move alone, so the pool's exhaustion gauge and rollups never count it.
        class: FailureClass::Rejected,
        ..exhaustion_failure(&Exhaustion::Untimed, 0)
    }
    .response(request_id)
}

/// The wait until `deadline` in whole seconds, rounded up and at least one, as `Retry-After` carries it.
fn whole_seconds(deadline: i64, now: i64) -> u64 {
    (deadline.saturating_sub(now).max(1_000).saturating_add(999) / 1_000) as u64
}
