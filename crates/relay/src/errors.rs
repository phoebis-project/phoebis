//! Anthropic-shaped local failures, and the one table that maps every internal failure to a status, an
//! Anthropic error type, and the adapter's own error code.

use axum::body::Body;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;
use phoebis_oauth::AcquireError;
use phoebis_routing::KeyRefusal;

use crate::auth::AuthError;
use crate::RelayError;

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
        anthropic_error(
            self.status,
            self.error_type,
            self.code,
            self.message,
            request_id,
        )
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
    }
}

pub(crate) fn relay_failure(error: &RelayError) -> Failure {
    match error {
        RelayError::AccountUuid => Failure {
            status: StatusCode::SERVICE_UNAVAILABLE,
            error_type: "api_error",
            code: "accounts_unavailable",
            message: "the bound account cannot serve requests",
        },
        RelayError::BodyTooLarge => Failure {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            error_type: "request_too_large",
            code: "request_too_large",
            message: "the request body is too large",
        },
        RelayError::Cch(_) | RelayError::NotAnObject => Failure {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            code: "invalid_body",
            message: "the request body is not a valid Messages request",
        },
        // Fail closed: a hash the pinned rules cannot reproduce means the client moved past the profile, and
        // the operator re-pins rather than the relay guessing what the upstream now expects.
        RelayError::ClientDrift { .. } => Failure {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            code: "client_drift",
            message: "the client's billing hash does not match the pinned profile; re-pin the router to this client version",
        },
        RelayError::InvalidModel => Failure {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            code: "invalid_model",
            message: "the request body's model is not a usable model name",
        },
        RelayError::NoModel => Failure {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            code: "missing_model",
            message: "the request body has no model",
        },
    }
}

/// Every refused key is the same 401 the static table answered: telling the caller which of the six ways it
/// failed would help an attacker enumerate, so the variant reaches the log line only.
pub(crate) fn key_refusal(_: &KeyRefusal) -> Failure {
    Failure {
        status: StatusCode::UNAUTHORIZED,
        error_type: "authentication_error",
        code: "auth_unknown",
        message: "the presented API key is not valid",
    }
}

pub(crate) fn acquire_failure(error: &AcquireError) -> Failure {
    match error {
        // Both need an operator to pair the account again, and both leave the caller's own key valid; the 401
        // is what makes a downstream client stop hammering the route.
        AcquireError::Quarantined | AcquireError::ReauthRequired => Failure {
            status: StatusCode::UNAUTHORIZED,
            error_type: "authentication_error",
            code: "reauth_required",
            message: "the bound account has to be paired again",
        },
        AcquireError::Disconnected | AcquireError::Unpaired => ACCOUNTS_UNAVAILABLE,
        AcquireError::Store(_) | AcquireError::Unavailable(_) => Failure {
            status: StatusCode::SERVICE_UNAVAILABLE,
            error_type: "api_error",
            code: "credential_unavailable",
            message: "the bound account's credential is temporarily unavailable",
        },
    }
}

pub(crate) const ACCOUNTS_UNAVAILABLE: Failure = Failure {
    status: StatusCode::SERVICE_UNAVAILABLE,
    error_type: "api_error",
    code: "accounts_unavailable",
    message: "the bound account cannot serve requests",
};

/// The account the request was bound to went away while it was in flight, so its credential is gone and no
/// answer about it can be trusted.
pub(crate) const ACCOUNT_CANCELLED: Failure = Failure {
    status: StatusCode::SERVICE_UNAVAILABLE,
    error_type: "api_error",
    code: "account_cancelled",
    message: "the bound account was disconnected while the request was in flight",
};

pub(crate) const METHOD_NOT_ALLOWED: Failure = Failure {
    status: StatusCode::METHOD_NOT_ALLOWED,
    error_type: "invalid_request_error",
    code: "method_not_allowed",
    message: "the method is not supported on this route",
};

pub(crate) const MODEL_NOT_ALLOWED: Failure = Failure {
    status: StatusCode::NOT_FOUND,
    error_type: "not_found_error",
    code: "model_not_allowed",
    message: "model not allowed for this key",
};

pub(crate) const MODEL_UNSERVED: Failure = Failure {
    status: StatusCode::NOT_FOUND,
    error_type: "not_found_error",
    code: "model_unserved",
    message: "no account in this pool serves the model",
};

pub(crate) const NOT_FOUND: Failure = Failure {
    status: StatusCode::NOT_FOUND,
    error_type: "not_found_error",
    code: "not_found",
    message: "the requested resource was not found",
};

pub(crate) const ROUTER_UNAVAILABLE: Failure = Failure {
    status: StatusCode::SERVICE_UNAVAILABLE,
    error_type: "api_error",
    code: "router_unavailable",
    message: "the router is not available",
};

/// The upstream may or may not have received the request; either way the relay has no response to forward.
pub(crate) const UPSTREAM_UNREACHABLE: Failure = Failure {
    status: StatusCode::BAD_GATEWAY,
    error_type: "api_error",
    code: "upstream_unreachable",
    message: "the upstream did not answer",
};
