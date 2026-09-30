//! Anthropic-shaped local failures, and the one table that maps every internal failure to a status, an
//! Anthropic error type, and the adapter's own error code.

use axum::body::Body;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;
use phoebis_routing::KeyRefusal;

use crate::auth::AuthError;
use crate::{BodyMember, RelayError};

/// Carries the adapter's own failure code, which is finer grained than the Anthropic error type a downstream
/// client reads, so an operator can tell two failures apart without a log line.
pub(crate) const ERROR_CODE_HEADER: &str = "x-phoebis-error-code";

/// What a caller is told about a failure: the Anthropic error type it may branch on, the adapter code, and a
/// message that never names a credential, a host, or an internal detail.
pub(crate) struct Failure {
    pub status: StatusCode,
    pub error_type: &'static str,
    pub code: &'static str,
    pub message: &'static str,
    /// The `Retry-After` the answer carries, in seconds.
    pub retry_after: Option<u64>,
}

pub fn anthropic_error(
    status: StatusCode,
    error_type: &str,
    code: &str,
    message: &str,
    request_id: &str,
) -> Response {
    let body = serde_json::json!({
        "type": "error",
        "error": { "type": error_type, "message": message },
        "request_id": request_id,
    })
    .to_string();
    let code =
        HeaderValue::from_str(code).unwrap_or_else(|_| HeaderValue::from_static("unclassified"));
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .header(ERROR_CODE_HEADER, code)
        .body(Body::from(body))
        // Every argument above is either a validated `StatusCode` or ASCII this function just built.
        .expect("a local error response is always well formed")
}

impl Failure {
    pub(crate) fn response(&self, request_id: &str) -> Response {
        let mut response = anthropic_error(
            self.status,
            self.error_type,
            self.code,
            self.message,
            request_id,
        );
        if let Some(seconds) = self.retry_after {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
        }
        response
    }
}

/// The error code of the per-key cap refusal.
pub const KEY_IN_FLIGHT_CAP: &str = "key_in_flight_cap";

/// The per-key cap refusal: 429 `rate_limit_error`, code `key_in_flight_cap`, `Retry-After` in seconds. No
/// `x-should-retry`: the client's own retry policy for a 429 decides.
pub(crate) fn key_in_flight_cap(retry_after_seconds: u64) -> Failure {
    Failure {
        status: StatusCode::TOO_MANY_REQUESTS,
        error_type: "rate_limit_error",
        code: KEY_IN_FLIGHT_CAP,
        message: "this key has as many requests in flight as it allows",
        retry_after: Some(retry_after_seconds),
    }
}

/// The error code of a pool whose every account was held at its in-flight cap until the hold ran out.
pub const ACCOUNTS_AT_CAP: &str = "accounts_at_cap";

/// The longest `Retry-After` the at-cap answer names. The client waits out a `Retry-After` under 20 seconds and
/// treats a longer one as a block it does not retry, while a pool at its cap frees within seconds.
const AT_CAP_RETRY_AFTER_MAX: u64 = 19;

/// 429 `rate_limit_error`, code `accounts_at_cap`, message `every account in this pool is at its in-flight cap`,
/// `Retry-After` the pool's `retry_in_place_seconds` clamped to 1 through 19.
pub(crate) fn accounts_at_cap(retry_after_seconds: u64) -> Failure {
    Failure {
        status: StatusCode::TOO_MANY_REQUESTS,
        error_type: "rate_limit_error",
        code: ACCOUNTS_AT_CAP,
        message: "every account in this pool is at its in-flight cap",
        retry_after: Some(retry_after_seconds.clamp(1, AT_CAP_RETRY_AFTER_MAX)),
    }
}

/// A rejected caller credential is always a `401`: telling the caller which of the four ways it failed would
/// help an attacker enumerate, and the downstream client acts on the status alone.
pub(crate) fn auth_failure(error: &AuthError) -> Failure {
    let (code, message) = match error {
        AuthError::Ambiguous => ("auth_ambiguous", "present exactly one API key"),
        AuthError::Malformed => ("auth_malformed", "the API key header is malformed"),
        AuthError::Missing => ("auth_missing", "an API key is required"),
        AuthError::Unknown => ("auth_unknown", "the presented API key is not valid"),
    };
    Failure {
        status: StatusCode::UNAUTHORIZED,
        error_type: "authentication_error",
        code,
        message,
        retry_after: None,
    }
}

/// `Failure.message` is `&'static str`, so the four spellings `BodyMember::Repeated` names are constants
/// rather than a formatted string.
const REPEATED_METADATA: &str =
    "the request body carries the top-level member \"metadata\" more than once";
const REPEATED_MODEL: &str =
    "the request body carries the top-level member \"model\" more than once";
const REPEATED_SAFEGUARDS: &str =
    "the request body carries the top-level member \"safeguards\" more than once";
const REPEATED_STREAM: &str =
    "the request body carries the top-level member \"stream\" more than once";

pub(crate) fn relay_failure(error: &RelayError) -> Failure {
    match error {
        RelayError::AccountUuid => Failure {
            status: StatusCode::SERVICE_UNAVAILABLE,
            error_type: "api_error",
            code: "accounts_unavailable",
            message: "the bound account cannot serve requests",
            retry_after: None,
        },
        // The caller is already gone; the answer exists so the request still gets a row in the ledger.
        RelayError::BodyIncomplete => Failure {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            code: "request_incomplete",
            message: "the request body ended before it was complete",
            retry_after: None,
        },
        // Naming the member is what lets a client author see which part of its body the relay has to find;
        // no value of the body is echoed back.
        RelayError::BodyShape(member) => Failure {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            code: phoebis_bus::BODY_SHAPE_ERROR_CODE,
            message: match member {
                BodyMember::BillingStamp => {
                    "the request body carries a billing stamp the relay cannot restamp, so the edits the request needs cannot travel with a hash that matches them"
                }
                BodyMember::CacheControl => {
                    "the request body carries a cache_control marker the relay cannot place, so the pool's cache TTL policy cannot be applied to it"
                }
                BodyMember::MetadataUserId => {
                    "the request body has no metadata.user_id the account can be bound to"
                }
                BodyMember::Model => "the request body has no top-level model to resolve",
                BodyMember::Repeated(name) => match *name {
                    "metadata" => REPEATED_METADATA,
                    "model" => REPEATED_MODEL,
                    "safeguards" => REPEATED_SAFEGUARDS,
                    "stream" => REPEATED_STREAM,
                    _ => unreachable!(
                        "BodyMember::Repeated only ever names metadata, model, safeguards or stream"
                    ),
                },
                BodyMember::Safeguards => {
                    "the request body carries a safeguards member the relay cannot place under this key's auto-mode setting"
                }
            },
            retry_after: None,
        },
        RelayError::BodyTimeout => Failure {
            status: StatusCode::REQUEST_TIMEOUT,
            error_type: "invalid_request_error",
            code: "request_timeout",
            message: "the request body did not arrive in time",
            retry_after: None,
        },
        RelayError::BodyTooLarge => Failure {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            error_type: "request_too_large",
            code: "request_too_large",
            message: "the request body is too large",
            retry_after: None,
        },
        // An unfinished scan proves nothing about the members past where it stopped, so it is refused rather
        // than admitted on what was read so far.
        RelayError::BodyTooDeep => Failure {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            code: "invalid_body",
            message: "the request body nests deeper than the relay reads",
            retry_after: None,
        },
        RelayError::Cch(_) | RelayError::NotAnObject => Failure {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            code: "invalid_body",
            message: "the request body is not a valid Messages request",
            retry_after: None,
        },
        // Fail closed: a hash or a version suffix the pinned rules cannot reproduce means the client moved past
        // the profile, and the operator re-pins rather than the relay guessing what the upstream now expects.
        RelayError::ClientDrift { kind: "version_suffix" } => Failure {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            code: "client_drift",
            message: "the client's version suffix does not reproduce from the prompt and the user agent; re-pin the router to this client version",
            retry_after: None,
        },
        RelayError::ClientDrift { .. } => Failure {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            code: "client_drift",
            message: "the client's billing hash does not match the pinned profile; re-pin the router to this client version",
            retry_after: None,
        },
        RelayError::InvalidModel => Failure {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            code: "invalid_model",
            message: "the request body's model is not a usable model name",
            retry_after: None,
        },
        RelayError::NoModel => Failure {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            code: "missing_model",
            message: "the request body has no model",
            retry_after: None,
        },
    }
}

/// Every refused key is the same 401 the static table answered: telling the caller which of the ways it
/// failed would help an attacker enumerate, so the variant reaches the log line only.
pub(crate) fn key_refusal(_: &KeyRefusal) -> Failure {
    Failure {
        status: StatusCode::UNAUTHORIZED,
        error_type: "authentication_error",
        code: "auth_unknown",
        message: "the presented API key is not valid",
        retry_after: None,
    }
}

/// The 401 `handle` answers a refused credential header with, for a route outside this crate that
/// authenticates the same way and must answer the same way.
pub fn auth_error_response(error: &AuthError, request_id: &str) -> Response {
    auth_failure(error).response(request_id)
}

pub fn key_refusal_response(refusal: &KeyRefusal, request_id: &str) -> Response {
    key_refusal(refusal).response(request_id)
}

/// The account the request was bound to went away while it was in flight, so its credential is gone and no
/// answer about it can be trusted.
pub(crate) const ACCOUNT_CANCELLED: Failure = Failure {
    status: StatusCode::SERVICE_UNAVAILABLE,
    error_type: "api_error",
    code: "account_cancelled",
    message: "the bound account was disconnected while the request was in flight",
    retry_after: None,
};

pub(crate) const METHOD_NOT_ALLOWED: Failure = Failure {
    status: StatusCode::METHOD_NOT_ALLOWED,
    error_type: "invalid_request_error",
    code: "method_not_allowed",
    message: "the method is not supported on this route",
    retry_after: None,
};

pub(crate) const MODEL_NOT_ALLOWED: Failure = Failure {
    status: StatusCode::NOT_FOUND,
    error_type: "not_found_error",
    code: "model_not_allowed",
    message: "model not allowed for this key",
    retry_after: None,
};

pub(crate) const MODEL_REJECTED: Failure = Failure {
    status: StatusCode::NOT_FOUND,
    error_type: "not_found_error",
    code: "model_rejected",
    message: "every account in this pool rejected the model",
    retry_after: None,
};

pub(crate) const NOT_FOUND: Failure = Failure {
    status: StatusCode::NOT_FOUND,
    error_type: "not_found_error",
    code: "not_found",
    message: "the requested resource was not found",
    retry_after: None,
};

pub(crate) const ROUTER_UNAVAILABLE: Failure = Failure {
    status: StatusCode::SERVICE_UNAVAILABLE,
    error_type: "api_error",
    code: "router_unavailable",
    message: "the router is not available",
    retry_after: None,
};

/// The upstream may or may not have received the request; either way the relay has no response to forward.
pub(crate) const UPSTREAM_UNREACHABLE: Failure = Failure {
    status: StatusCode::BAD_GATEWAY,
    error_type: "api_error",
    code: "upstream_unreachable",
    message: "the upstream did not answer",
    retry_after: None,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// A drifted hash and a drifted version suffix are the same error code, but the message names which check
    /// failed, so an operator reads the right one off the response without a log line.
    #[test]
    fn a_client_drift_message_names_the_kind_that_failed() {
        let hash = relay_failure(&RelayError::ClientDrift { kind: "hash" });
        assert_eq!(hash.code, "client_drift");
        assert!(hash.message.contains("billing hash"), "{}", hash.message);
        let suffix = relay_failure(&RelayError::ClientDrift {
            kind: "version_suffix",
        });
        assert_eq!(suffix.code, "client_drift");
        assert!(
            suffix.message.contains("version suffix"),
            "{}",
            suffix.message
        );
        assert_ne!(hash.message, suffix.message);
    }

    #[test]
    fn every_body_shape_refusal_carries_the_code_the_ledger_reads_as_a_standing() {
        for member in [
            BodyMember::BillingStamp,
            BodyMember::CacheControl,
            BodyMember::MetadataUserId,
            BodyMember::Model,
            BodyMember::Repeated("metadata"),
            BodyMember::Repeated("model"),
            BodyMember::Repeated("safeguards"),
            BodyMember::Repeated("stream"),
            BodyMember::Safeguards,
        ] {
            let failure = relay_failure(&RelayError::BodyShape(member));
            assert_eq!(
                failure.code,
                phoebis_bus::BODY_SHAPE_ERROR_CODE,
                "{}",
                member.path()
            );
        }
        assert_eq!(
            phoebis_bus::BODY_SHAPE_ERROR_CODE,
            "body_shape",
            "the wire spelling"
        );
    }
}
