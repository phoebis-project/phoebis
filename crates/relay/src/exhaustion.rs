//! The two answers for a pool with nothing to select: a 429 shaped like one rate-limited account so the
//! client's automatic continue arms, or a 503 only an operator can clear.

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
            let seconds = deadline.saturating_sub(now).max(1_000).saturating_add(999) / 1_000;
            // The client treats an unknown claim as no claim and then never arms its continue, so the one
            // window name it cannot know is rendered as the five-hour window; the deadline is what matters.
            let claim = if window == "retry_after" {
                "five_hour"
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
    }
}
