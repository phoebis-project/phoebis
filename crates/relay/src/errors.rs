//! Anthropic-shaped local failures, and the one table that maps every internal failure to a status, an
//! Anthropic error type, and the adapter's own error code.

use std::borrow::Cow;

use axum::body::Body;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;
use phoebis_bus::Outcome;
use phoebis_routing::{exhaustion_status, Exhaustion, KeyRefusal, Obstacle};
use phoebis_store::Feature;

use crate::auth::AuthError;
use crate::{BodyMember, DriftKind, RelayError};

/// Carries the adapter's own failure code, which is finer grained than the Anthropic error type a downstream
/// client reads, so an operator can tell two failures apart without a log line.
pub const ERROR_CODE_HEADER: &str = "x-phoebis-error-code";

pub(crate) const SHOULD_RETRY_HEADER: &str = "x-should-retry";

/// What a caller is told about a failure: the Anthropic error type it may branch on, the adapter code, and a
/// message that never names a credential, a host, or an internal detail.
pub(crate) struct Failure {
    pub status: StatusCode,
    pub error_type: &'static str,
    pub code: &'static str,
    pub message: Cow<'static, str>,
    /// The `Retry-After` the answer carries, in seconds.
    pub retry_after: Option<u64>,
    /// Writes `x-should-retry: false`, so a client that retries its 4xx answers resends none of the refusals that no
    /// resend heals: a refused credential, what the client gate (`client_not_allowed`), the contract, the drift check
    /// or a guardrail refused, a body above the whole body budget, and a call on the feature listener that no row of
    /// its table names, whose key resolves to no account for its feature or whose feature account cannot serve it.
    pub no_retry: bool,
    /// The outcome the ledger files the request under, carried by the answer itself so no reader infers it from the
    /// code.
    pub class: FailureClass,
}

/// The outcome class of a `Failure`, read against the attempt the answer comes at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FailureClass {
    Canceled,
    Exhausted,
    Failed,
    /// Rejected before any attempt, failed after one, since the attempt may have been billed.
    Refused,
    /// Conclusive at any attempt.
    Rejected,
}

impl FailureClass {
    pub(crate) fn outcome(self, attempt: u8) -> Outcome {
        match self {
            FailureClass::Canceled => Outcome::Canceled,
            FailureClass::Exhausted => Outcome::Exhausted,
            FailureClass::Failed => Outcome::Failed,
            FailureClass::Rejected => Outcome::Rejected,
            FailureClass::Refused if attempt == 0 => Outcome::Rejected,
            FailureClass::Refused => Outcome::Failed,
        }
    }
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

/// The status an exhaustion is answered with, the routing crate's, which the routing log's exhausted row records as
/// its `answer`, so the row and the client cannot disagree.
pub(crate) const fn exhaustion_status_code(exhaustion: &Exhaustion, at_cap: bool) -> StatusCode {
    match StatusCode::from_u16(exhaustion_status(exhaustion, at_cap)) {
        Ok(status) => status,
        Err(_) => panic!("the exhaustion statuses are valid HTTP statuses"),
    }
}

impl Failure {
    pub(crate) fn response(&self, request_id: &str) -> Response {
        let mut response = anthropic_error(
            self.status,
            self.error_type,
            self.code,
            &self.message,
            request_id,
        );
        if let Some(seconds) = self.retry_after {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
        }
        if self.no_retry {
            response
                .headers_mut()
                .insert(SHOULD_RETRY_HEADER, HeaderValue::from_static("false"));
        }
        response
    }
}

/// The error code of a request refused because the bodies in flight already hold the process's byte budget.
pub const BODY_BUDGET_EXHAUSTED: &str = "body_budget_exhausted";

/// 503 `api_error`, the message `the router holds as many request bytes as it allows`, no `Retry-After`: the
/// router refuses rather than queues, and a body freeing room says nothing about when.
pub(crate) fn body_budget_exhausted() -> Failure {
    Failure {
        status: StatusCode::SERVICE_UNAVAILABLE,
        error_type: "api_error",
        code: BODY_BUDGET_EXHAUSTED,
        message: Cow::Borrowed("the router holds as many request bytes as it allows"),
        retry_after: None,
        no_retry: false,
        class: FailureClass::Refused,
    }
}

/// The error code of a request whose body and parsed tree are more than the whole of the process's byte budget.
pub const BODY_OVER_BUDGET: &str = "body_over_budget";

/// 413 `request_too_large`, the message `the request body and its parsed tree are larger than the router's body
/// budget`, no `Retry-After` and `x-should-retry: false`: the body and the configured budget decide it, whatever the
/// bodies in flight hold, so no wait makes room for it.
pub(crate) fn body_over_budget() -> Failure {
    Failure {
        status: StatusCode::PAYLOAD_TOO_LARGE,
        error_type: "request_too_large",
        code: BODY_OVER_BUDGET,
        message: Cow::Borrowed(
            "the request body and its parsed tree are larger than the router's body budget",
        ),
        retry_after: None,
        no_retry: true,
        class: FailureClass::Refused,
    }
}

/// The error code of a request refused because the ledger's writer is behind.
pub const LEDGER_BEHIND: &str = "ledger_behind";

/// 503 `api_error`, the message `the router is not accepting requests right now`, `Retry-After: 5`: no account and
/// no reason beyond the code, since the router's own standing is nothing the caller can act on but by waiting.
pub(crate) fn ledger_behind() -> Failure {
    Failure {
        status: StatusCode::SERVICE_UNAVAILABLE,
        error_type: "api_error",
        code: LEDGER_BEHIND,
        message: Cow::Borrowed("the router is not accepting requests right now"),
        retry_after: Some(5),
        no_retry: false,
        class: FailureClass::Refused,
    }
}

/// The error code of a request refused because its key already has as many bodies arriving as it may.
pub const KEY_BODY_READS: &str = "key_body_reads";

/// 429 `rate_limit_error`, the message `this key has as many request bodies arriving as it allows`,
/// `Retry-After: 1`: a slot frees as soon as one of the key's bodies has arrived.
pub(crate) fn key_body_reads() -> Failure {
    Failure {
        status: StatusCode::TOO_MANY_REQUESTS,
        error_type: "rate_limit_error",
        code: KEY_BODY_READS,
        message: Cow::Borrowed("this key has as many request bodies arriving as it allows"),
        retry_after: Some(1),
        no_retry: false,
        class: FailureClass::Refused,
    }
}

/// The error code of a pool whose every account was held at its in-flight cap until the hold ran out.
const ACCOUNTS_AT_CAP: &str = "accounts_at_cap";
/// The error code of a pool whose every account is rate limited until a known deadline.
pub const ACCOUNTS_EXHAUSTED: &str = "accounts_exhausted";
/// The error code of a pool with no account that can serve requests, and of a bound account that cannot.
pub const ACCOUNTS_UNAVAILABLE: &str = "accounts_unavailable";

/// The longest `Retry-After` the at-cap answer names. The client waits out a `Retry-After` under 20 seconds and
/// treats a longer one as a block it does not retry, while a pool at its cap frees within seconds.
const AT_CAP_RETRY_AFTER_MAX: u64 = 19;

/// 429 `rate_limit_error`, code `accounts_at_cap`, message `every account in this pool is at its in-flight cap`,
/// `Retry-After` the pool's `retry_in_place_seconds` clamped to 1 through 19.
pub(crate) fn accounts_at_cap(retry_after_seconds: u64) -> Failure {
    Failure {
        status: exhaustion_status_code(&Exhaustion::Untimed, true),
        error_type: "rate_limit_error",
        code: ACCOUNTS_AT_CAP,
        message: Cow::Borrowed("every account in this pool is at its in-flight cap"),
        retry_after: Some(retry_after_seconds.clamp(1, AT_CAP_RETRY_AFTER_MAX)),
        no_retry: false,
        // The pool had nothing to place the request on either, so the attention gauge and the rollups count it with
        // the other exhaustions.
        class: FailureClass::Exhausted,
    }
}

/// 429 `rate_limit_error`, code `domain_checks_at_cap`, `Retry-After: 1`: a domain check whose time ran out while it
/// waited for a place. It names no key, since the check carries none, and a place frees as soon as a check ends.
pub(crate) fn domain_checks_at_cap() -> Failure {
    Failure {
        status: StatusCode::TOO_MANY_REQUESTS,
        error_type: "rate_limit_error",
        code: "domain_checks_at_cap",
        message: Cow::Borrowed(
            "as many domain checks are in flight as the router forwards at once",
        ),
        retry_after: Some(1),
        no_retry: false,
        class: FailureClass::Refused,
    }
}

/// 429 `rate_limit_error`, code `key_sockets`, no `Retry-After`: the key has as many sockets open as one key may. No
/// time bounds the wait for a place, so the answer names none.
pub(crate) fn key_sockets() -> Failure {
    Failure {
        status: StatusCode::TOO_MANY_REQUESTS,
        error_type: "rate_limit_error",
        code: "key_sockets",
        message: Cow::Borrowed("this key has as many sockets open as it allows"),
        retry_after: None,
        no_retry: false,
        class: FailureClass::Refused,
    }
}

/// 429 `rate_limit_error`, code `sockets_at_cap`, no `Retry-After`: the listener carries as many sockets as it may.
pub(crate) fn sockets_at_cap() -> Failure {
    Failure {
        status: StatusCode::TOO_MANY_REQUESTS,
        error_type: "rate_limit_error",
        code: "sockets_at_cap",
        message: Cow::Borrowed("as many sockets are open as the router carries at once"),
        retry_after: None,
        no_retry: false,
        class: FailureClass::Refused,
    }
}

/// A rejected caller credential is always a `401`: telling the caller which of the three ways it failed would
/// help an attacker enumerate. The answer also tells the client not to resend the request.
pub(crate) fn auth_failure(error: &AuthError) -> Failure {
    let (code, message) = match error {
        AuthError::Ambiguous => ("auth_ambiguous", "present exactly one key"),
        AuthError::Malformed => ("auth_malformed", "the key header is malformed"),
        AuthError::Missing => ("auth_missing", "a key is required"),
    };
    Failure {
        status: StatusCode::UNAUTHORIZED,
        error_type: "authentication_error",
        code,
        message: Cow::Borrowed(message),
        retry_after: None,
        // None of the three heals between resends, and the client's query loop stops at the first refusal it is
        // told not to resend.
        no_retry: true,
        class: FailureClass::Refused,
    }
}

/// The item's own message: what is missing, and the environment line that sends it.
fn contract_message(item: &str) -> &'static str {
    match item {
        "client_version" => {
            "the client version in the User-Agent header is not spelled as Claude Code spells one. \
             Report the client version to the operator, who updates the router"
        }
        "session_header" => {
            "the request carries no x-claude-code-session-id header the router requires of a \
             Claude Code client. Send the requests as Claude Code sends them"
        }
        "class_header" => {
            "the request carries no x-claude-code-request-class header the router requires of a \
             Claude Code client. Set CLAUDE_CODE_GATEWAY_HINT_HEADERS=1"
        }
        "attribution_line" => {
            "the request carries no attribution line at system[0] the router requires of a \
             Claude Code client. Leave CLAUDE_CODE_ATTRIBUTION_HEADER unset"
        }
        "metadata_user_id" => {
            "the request body has no metadata.user_id with an account_uuid member the router requires of a \
             Claude Code client. Send the requests as Claude Code sends them"
        }
        "agent_id_header" => {
            "the request names a subagent class and carries no \
             x-claude-code-agent-id header the router requires of a Claude Code client. \
             Send the requests as Claude Code sends them"
        }
        "compaction_header" => {
            "the request names the compaction class and carries no \
             x-claude-code-compaction header the router requires of a Claude Code client. \
             Set CLAUDE_CODE_GATEWAY_HINT_HEADERS=1"
        }
        _ => unreachable!(
            "contract_gap only ever names client_version, session_header, class_header, attribution_line, \
             metadata_user_id, agent_id_header or compaction_header"
        ),
    }
}

/// A drift refusal's message to the client and the advice its warn gives the operator, decided in one arm per case
/// so the two agree on whether a re-pin cures the drift.
pub(crate) fn drift_words(kind: DriftKind, resumed: bool) -> (&'static str, &'static str) {
    match (kind, resumed) {
        (DriftKind::VersionSuffix, true) => (
            "the conversation was resumed, and the client's version suffix does not reproduce \
             from the resume placeholder, the empty prompt or the user agent. Re-pin the router to this \
             client version",
            "Re-pin the router to that client",
        ),
        // The words name the anchors the check tried and the marks an operator reads off the refused
        // request, so a subagent that anchored its suffix elsewhere is told apart from a client that moved.
        (DriftKind::VersionSuffix, false) => (
            "the client's version suffix does not reproduce from any text block of the first user \
             message, the empty prompt or the version the user agent names, the anchors this build \
             tries for every request. Where the session's main turns pass, re-pinning the router does \
             not help: the refused body shows where this client anchored the suffix, and its billing \
             line's cc_is_subagent and the x-claude-code-agent-id, x-claude-code-request-class and \
             x-claude-code-agent-type headers, where the client sends them, tell a subagent's \
             request apart",
            "Where the session's main turns pass, re-pinning the router does not help",
        ),
        (DriftKind::Hash, _) => (
            "the client's billing hash does not reproduce under the Claude Code release this \
             build is measured against. Re-pin the router to this client version",
            "Re-pin the router to that client",
        ),
    }
}

pub(crate) fn relay_failure(error: &RelayError) -> Failure {
    match error {
        // The account's own stored uuid is what fails, which no wait and no resend heals, so the request files as
        // rejected rather than as the pool's exhaustion the code names elsewhere.
        RelayError::AccountUuid => Failure {
            status: StatusCode::SERVICE_UNAVAILABLE,
            error_type: "api_error",
            code: ACCOUNTS_UNAVAILABLE,
            message: Cow::Borrowed("the bound account cannot serve requests"),
            retry_after: None,
            no_retry: false,
            class: FailureClass::Rejected,
        },
        // The caller is already gone; the answer exists so the request still gets a row in the ledger.
        RelayError::BodyIncomplete => Failure {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            code: "request_incomplete",
            message: Cow::Borrowed("the request body ended before it was complete"),
            retry_after: None,
            no_retry: false,
            class: FailureClass::Refused,
        },
        RelayError::BodyNotGzip => Failure {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            code: "invalid_body",
            message: Cow::Borrowed("the request body is not valid gzip"),
            retry_after: None,
            no_retry: false,
            class: FailureClass::Refused,
        },
        // Naming the member is what lets a client author see which part of its body the relay has to find;
        // no value of the body is echoed back.
        RelayError::BodyShape(member) => Failure {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            code: phoebis_bus::BODY_SHAPE_ERROR_CODE,
            message: match member {
                BodyMember::BillingLine => {
                    "the request body carries a billing line the relay cannot decode".into()
                }
                BodyMember::BillingStamp => {
                    "the request body carries a billing stamp the relay can neither check nor restamp".into()
                }
                BodyMember::CacheControl => {
                    "the request body carries a cache_control marker the relay cannot place, so the pool's cache TTL policy cannot be applied to it".into()
                }
                BodyMember::MetadataUserId => {
                    "the request body has no metadata.user_id the account can be bound to".into()
                }
                BodyMember::Model => "the request body has no top-level model to resolve".into(),
                BodyMember::Repeated(member) => {
                    Cow::Owned(format!("the request body {}", member.carried_twice()))
                }
                BodyMember::Safeguards => {
                    "the request body carries a safeguards member the relay cannot place under this key's auto-mode setting".into()
                }
            },
            retry_after: None,
            no_retry: false,
            class: FailureClass::Refused,
        },
        RelayError::BodyTimeout => Failure {
            status: StatusCode::REQUEST_TIMEOUT,
            error_type: "invalid_request_error",
            code: "request_timeout",
            message: Cow::Borrowed("the request body did not arrive in time"),
            retry_after: None,
            no_retry: false,
            class: FailureClass::Refused,
        },
        RelayError::BodyTooLarge => Failure {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            error_type: "request_too_large",
            code: "request_too_large",
            message: Cow::Borrowed("the request body is too large"),
            retry_after: None,
            no_retry: false,
            class: FailureClass::Refused,
        },
        // Under the body-shape code, so the ledger files the refusal under the same standing as a body whose
        // member the relay cannot place.
        RelayError::BodyTooManyElements => Failure {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            code: phoebis_bus::BODY_SHAPE_ERROR_CODE,
            message: Cow::Borrowed("the request body holds more elements than the relay reads"),
            retry_after: None,
            no_retry: false,
            class: FailureClass::Refused,
        },
        // An unfinished scan proves nothing about the members past where it stopped, so it is refused rather
        // than admitted on what was read so far.
        RelayError::BodyTooDeep => Failure {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            code: "invalid_body",
            message: Cow::Borrowed("the request body nests deeper than the relay reads"),
            retry_after: None,
            no_retry: false,
            class: FailureClass::Refused,
        },
        RelayError::Cch(_) | RelayError::NotAnObject => Failure {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            code: "invalid_body",
            message: Cow::Borrowed("the request body is not a valid Messages request"),
            retry_after: None,
            no_retry: false,
            class: FailureClass::Refused,
        },
        // The client refusals answer the 403 `permission_error` Claude Code reads as final. The 2.1.310 client
        // heals a 400 whose message it does not recognize: it sends the request again without one feature after
        // another and keeps each feature off for the rest of the conversation, so one refusal cost the captured
        // client four posts. The gateway protocol the client embeds gives 403 to a caller that is
        // authenticated but not allowed, and 400 to a malformed body, which the body refusals keep.
        RelayError::ClientContract { item } => Failure {
            status: StatusCode::FORBIDDEN,
            error_type: "permission_error",
            code: CLIENT_CONTRACT,
            message: Cow::Borrowed(contract_message(item)),
            retry_after: None,
            // The fix is the client's environment, so a retry resends what was refused.
            no_retry: true,
            // Answered at attempt 0 and conclusive, named so no change to the attempt rule of `Refused` moves it.
            class: FailureClass::Rejected,
        },
        // Fail closed: a hash or a version suffix the pinned rules cannot reproduce means the client moved past
        // the profile. A stamp the router cannot verify never travels under an account's credential, so the
        // operator re-pins rather than the relay guessing what the upstream now expects. A resumed conversation's
        // suffix already reproduces over the empty prompt the client stamps, so its refusal is a moved client too,
        // and the words name the resume apart so the client and the operator see which conversation it was.
        RelayError::ClientDrift { kind, resumed } => Failure {
            status: StatusCode::FORBIDDEN,
            error_type: "permission_error",
            code: "client_drift",
            message: Cow::Borrowed(drift_words(*kind, *resumed).0),
            retry_after: None,
            no_retry: true,
            class: FailureClass::Refused,
        },
        RelayError::ClientRefused => Failure {
            status: StatusCode::FORBIDDEN,
            error_type: "permission_error",
            code: CLIENT_NOT_ALLOWED,
            message: Cow::Borrowed("the router admits Claude Code clients only"),
            retry_after: None,
            no_retry: true,
            // Answered at attempt 0 and conclusive, named so no change to the attempt rule of `Refused` moves it.
            class: FailureClass::Rejected,
        },
        // The client's query loop treats a 403 as final and heals none. The name rule refuses the two phrases the
        // client reads in a 403, `model:` and `OAuth token has been revoked`, since the message carries the name.
        RelayError::GuardrailBlocked { .. } => Failure {
            status: StatusCode::FORBIDDEN,
            error_type: "permission_error",
            code: GUARDRAIL_BLOCKED,
            message: error.to_string().into(),
            retry_after: None,
            no_retry: true,
            // Answered at attempt 0 and conclusive, named so no change to the attempt rule of `Refused` moves it.
            class: FailureClass::Rejected,
        },
        RelayError::InvalidModel => Failure {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            code: "invalid_model",
            message: Cow::Borrowed("the request body's model is not a usable model name"),
            retry_after: None,
            no_retry: false,
            class: FailureClass::Refused,
        },
        RelayError::LedgerBehind => ledger_behind(),
        RelayError::NoModel => Failure {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            code: "missing_model",
            message: Cow::Borrowed("the request body has no model"),
            retry_after: None,
            no_retry: false,
            class: FailureClass::Refused,
        },
        // Relaying a body the relay cannot inflate would carry bytes it can neither read nor edit under an
        // account's credential, so it is refused from the head instead.
        RelayError::UnsupportedEncoding => Failure {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            error_type: "invalid_request_error",
            code: "unsupported_content_encoding",
            message: Cow::Borrowed("the request body's content-encoding is not supported. Send gzip or identity"),
            retry_after: None,
            no_retry: false,
            class: FailureClass::Refused,
        },
    }
}

/// Every refused key is the same 401 the static table answered: telling the caller which of the ways it
/// failed would help an attacker enumerate, so the variant reaches the log line only.
pub(crate) fn key_refusal(_: &KeyRefusal) -> Failure {
    Failure {
        status: StatusCode::UNAUTHORIZED,
        error_type: "authentication_error",
        code: AUTH_UNKNOWN,
        message: Cow::Borrowed("the presented key is not valid"),
        retry_after: None,
        // No registry refusal heals between resends, and the client's query loop stops at the first refusal it is
        // told not to resend.
        no_retry: true,
        // Conclusive whether it is caught at admission or at the reselect after an attempt.
        class: FailureClass::Rejected,
    }
}

/// Sets `Cache-Control: no-store`, which every answer of a side route behind a key carries, its refusals included.
pub fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// `no_store` as a router's `map_response` layer. The header goes on the router rather than in the handlers, so an
/// answer no handler builds, such as an extractor's refusal, carries it too.
pub async fn no_store_answer(response: Response) -> Response {
    no_store(response)
}

/// The error code for a request the bound account's removal cut short.
pub(crate) const ACCOUNT_CANCELED_CODE: &str = "account_canceled";
/// The error code for a request the client's leaving cut short.
pub const CLIENT_GONE_CODE: &str = "client_gone";
/// The error code for a request the router's shutdown cut short.
pub(crate) const ROUTER_SHUTDOWN_CODE: &str = "router_shutdown";

/// The error code of a presented key the registry refused.
pub(crate) const AUTH_UNKNOWN: &str = "auth_unknown";
/// The error code of a Claude Code client that misses an item of the router's contract.
pub(crate) const CLIENT_CONTRACT: &str = "client_contract";
/// The error code of a client the router does not admit.
pub(crate) const CLIENT_NOT_ALLOWED: &str = "client_not_allowed";
/// The error code of a request a guardrail blocked.
pub(crate) const GUARDRAIL_BLOCKED: &str = "guardrail_blocked";
/// The error code of a model the key does not allow.
pub(crate) const MODEL_NOT_ALLOWED_CODE: &str = "model_not_allowed";
/// The error code of a model every account in the pool rejected.
pub(crate) const MODEL_REJECTED_CODE: &str = "model_rejected";

/// The account the request was bound to went away while it was in flight, so its credential is gone and no
/// answer about it can be trusted.
pub(crate) const ACCOUNT_CANCELED: Failure = Failure {
    status: StatusCode::SERVICE_UNAVAILABLE,
    error_type: "api_error",
    code: ACCOUNT_CANCELED_CODE,
    message: Cow::Borrowed("the bound account was disconnected while the request was in flight"),
    retry_after: None,
    no_retry: false,
    class: FailureClass::Canceled,
};

/// A keyed call on the feature listener that no row of the table names.
///
/// A 403 `permission_error` is the one answer the client takes quietly on each of its claude.ai calls, and the
/// message holds none of the phrases the client reads in a 403.
pub(crate) const FEATURE_NOT_ALLOWED: Failure = Failure {
    status: StatusCode::FORBIDDEN,
    error_type: "permission_error",
    code: "feature_not_allowed",
    message: Cow::Borrowed("the router does not forward this claude.ai call"),
    retry_after: None,
    no_retry: true,
    class: FailureClass::Rejected,
};

/// A call on the table whose feature account cannot serve it. The message names the routing crate's word for the
/// account's `obstacle`, so a key holder can tell the operator what to cure. Its 403 is `FEATURE_NOT_ALLOWED`'s, for
/// the same reason.
pub(crate) fn feature_account_unavailable(obstacle: &Obstacle) -> Failure {
    Failure {
        code: "feature_account_unavailable",
        message: Cow::Owned(format!(
            "the selected account cannot serve this feature ({})",
            obstacle.name()
        )),
        ..FEATURE_NOT_ALLOWED
    }
}

/// A call on the table whose key resolves to no account for `feature`. Its 403 is `FEATURE_NOT_ALLOWED`'s, for the same
/// reason.
pub(crate) fn feature_account_unset(feature: Feature) -> Failure {
    Failure {
        code: "feature_account_unset",
        message: Cow::Owned(format!(
            "no account is selected for {} on this key, ask the router's operator",
            feature.as_str()
        )),
        ..FEATURE_NOT_ALLOWED
    }
}

pub(crate) const METHOD_NOT_ALLOWED: Failure = Failure {
    status: StatusCode::METHOD_NOT_ALLOWED,
    error_type: "invalid_request_error",
    code: "method_not_allowed",
    message: Cow::Borrowed("the method is not supported on this route"),
    retry_after: None,
    no_retry: false,
    class: FailureClass::Refused,
};

pub(crate) const MODEL_NOT_ALLOWED: Failure = Failure {
    status: StatusCode::NOT_FOUND,
    error_type: "not_found_error",
    code: MODEL_NOT_ALLOWED_CODE,
    message: Cow::Borrowed("model not allowed for this key"),
    retry_after: None,
    no_retry: false,
    // Conclusive whether it is caught at admission or at the reselect after an attempt.
    class: FailureClass::Rejected,
};

pub(crate) const MODEL_REJECTED: Failure = Failure {
    status: exhaustion_status_code(&Exhaustion::ModelRejected, false),
    error_type: "not_found_error",
    code: MODEL_REJECTED_CODE,
    message: Cow::Borrowed("every account in this pool rejected the model"),
    retry_after: None,
    no_retry: false,
    // Caught before any attempt (an obstacle at selection) or after one, where a portable rejection fails over and
    // ends at the selector's exhaustion: the accounts that answered it are conclusive, not a transport failure.
    class: FailureClass::Rejected,
};

pub(crate) const NOT_FOUND: Failure = Failure {
    status: StatusCode::NOT_FOUND,
    error_type: "not_found_error",
    code: "not_found",
    message: Cow::Borrowed("the requested resource was not found"),
    retry_after: None,
    no_retry: false,
    class: FailureClass::Refused,
};

pub(crate) const ROUTER_UNAVAILABLE: Failure = Failure {
    status: StatusCode::SERVICE_UNAVAILABLE,
    error_type: "api_error",
    code: "router_unavailable",
    message: Cow::Borrowed("the router is not available"),
    retry_after: None,
    no_retry: false,
    class: FailureClass::Refused,
};

/// The upstream sent a head and then the connection was cut before the body ended, or the body ran past its bound.
pub(crate) const UPSTREAM_CUT: Failure = Failure {
    status: StatusCode::BAD_GATEWAY,
    error_type: "api_error",
    code: "upstream_cut",
    message: Cow::Borrowed("the upstream response could not be read after its head"),
    retry_after: None,
    no_retry: false,
    class: FailureClass::Failed,
};

/// The upstream sent a head and then went silent past the idle bound.
pub(crate) const UPSTREAM_STALLED: Failure = Failure {
    status: StatusCode::BAD_GATEWAY,
    error_type: "api_error",
    code: "upstream_stalled",
    message: Cow::Borrowed("the upstream stalled after its head"),
    retry_after: None,
    no_retry: false,
    class: FailureClass::Failed,
};

/// The upstream sent a head and then an answer the relay could not decode: a content coding it cannot read, or a
/// payload that does not decode. Nothing of it is forwarded.
pub(crate) const UPSTREAM_UNDECODABLE: Failure = Failure {
    status: StatusCode::BAD_GATEWAY,
    error_type: "api_error",
    code: "upstream_undecodable",
    message: Cow::Borrowed("the upstream response could not be decoded after its head"),
    retry_after: None,
    no_retry: false,
    class: FailureClass::Failed,
};

/// The upstream sent no head at all: it may or may not have received the request, and either way the relay has no
/// response to forward.
pub(crate) const UPSTREAM_UNREACHABLE: Failure = Failure {
    status: StatusCode::BAD_GATEWAY,
    error_type: "api_error",
    code: "upstream_unreachable",
    message: Cow::Borrowed("the upstream did not answer"),
    retry_after: None,
    no_retry: false,
    class: FailureClass::Failed,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::RepeatedMember;

    /// A drifted hash and a drifted version suffix are the same error code, but the message names which check
    /// failed, so an operator reads the right one off the response without a log line.
    #[test]
    fn a_client_drift_message_names_the_kind_that_failed() {
        let hash = relay_failure(&RelayError::ClientDrift {
            kind: DriftKind::Hash,
            resumed: false,
        });
        assert_eq!(hash.code, "client_drift");
        assert!(hash.message.contains("billing hash"), "{}", hash.message);
        let suffix = relay_failure(&RelayError::ClientDrift {
            kind: DriftKind::VersionSuffix,
            resumed: false,
        });
        assert_eq!(suffix.code, "client_drift");
        assert!(
            suffix.message.contains("version suffix"),
            "{}",
            suffix.message
        );
        assert_ne!(hash.message, suffix.message);
    }

    /// The client page quotes each message whole, so a word or a space the table's wrapped lines drop answers a text
    /// the page does not show.
    #[test]
    fn every_contract_message_reads_as_the_client_page_quotes_it() {
        use crate::identity::{
            ITEM_AGENT_ID_HEADER, ITEM_ATTRIBUTION_LINE, ITEM_CLASS_HEADER, ITEM_CLIENT_VERSION,
            ITEM_COMPACTION_HEADER, ITEM_METADATA_USER_ID, ITEM_SESSION_HEADER,
        };
        for (item, quoted) in [
            (
                ITEM_CLIENT_VERSION,
                "the client version in the User-Agent header is not spelled as Claude Code spells one. Report the \
                 client version to the operator, who updates the router",
            ),
            (
                ITEM_SESSION_HEADER,
                "the request carries no x-claude-code-session-id header the router requires of a Claude Code \
                 client. Send the requests as Claude Code sends them",
            ),
            (
                ITEM_CLASS_HEADER,
                "the request carries no x-claude-code-request-class header the router requires of a Claude Code \
                 client. Set CLAUDE_CODE_GATEWAY_HINT_HEADERS=1",
            ),
            (
                ITEM_ATTRIBUTION_LINE,
                "the request carries no attribution line at system[0] the router requires of a Claude Code \
                 client. Leave CLAUDE_CODE_ATTRIBUTION_HEADER unset",
            ),
            (
                ITEM_METADATA_USER_ID,
                "the request body has no metadata.user_id with an account_uuid member the router requires of a \
                 Claude Code client. Send the requests as Claude Code sends them",
            ),
            (
                ITEM_AGENT_ID_HEADER,
                "the request names a subagent class and carries no x-claude-code-agent-id header the router \
                 requires of a Claude Code client. Send the requests as Claude Code sends them",
            ),
            (
                ITEM_COMPACTION_HEADER,
                "the request names the compaction class and carries no x-claude-code-compaction header the \
                 router requires of a Claude Code client. Set CLAUDE_CODE_GATEWAY_HINT_HEADERS=1",
            ),
        ] {
            assert_eq!(contract_message(item), quoted, "{item}");
        }
        assert_eq!(
            relay_failure(&RelayError::ClientRefused).message,
            "the router admits Claude Code clients only",
            "the client page quotes the gate's message whole"
        );
    }

    #[test]
    fn every_body_shape_refusal_carries_the_code_the_ledger_reads_as_a_standing() {
        for member in [
            BodyMember::BillingStamp,
            BodyMember::CacheControl,
            BodyMember::MetadataUserId,
            BodyMember::Model,
            BodyMember::Repeated(RepeatedMember::Metadata),
            BodyMember::Repeated(RepeatedMember::MetadataUserId),
            BodyMember::Repeated(RepeatedMember::Model),
            BodyMember::Repeated(RepeatedMember::Safeguards),
            BodyMember::Repeated(RepeatedMember::Stream),
            BodyMember::Repeated(RepeatedMember::System),
            BodyMember::Repeated(RepeatedMember::BillingLine),
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
    }

    /// The name is the one part of the answer the operator wrote, so the arm formats it into the message and the
    /// answer says nothing else about the guardrail. The client treats a 403 as final and heals no 403 by resending
    /// the request, so a block costs one request per tick.
    #[test]
    fn the_guardrail_arm_formats_the_name() {
        let failure = relay_failure(&RelayError::GuardrailBlocked {
            name: "Large auxiliary".into(),
        });
        assert_eq!(failure.retry_after, None);
        assert_eq!(failure.message, r#"blocked by guardrail "Large auxiliary""#);
    }

    /// The client refusals, the gate and the guardrail block answer the 403 `permission_error` the client reads as
    /// final, each under its own code, and each asks the client not to resend the request. None of their messages
    /// holds a phrase the client reads in a 4xx as a cue to act on its own.
    #[test]
    fn each_403_arm_answers_permission_error_under_its_own_code_and_asks_for_no_retry() {
        use crate::identity::ITEM_CLIENT_VERSION;
        for (error, code) in [
            (
                RelayError::ClientContract {
                    item: ITEM_CLIENT_VERSION,
                },
                "client_contract",
            ),
            (
                RelayError::ClientDrift {
                    kind: DriftKind::Hash,
                    resumed: false,
                },
                "client_drift",
            ),
            (
                RelayError::ClientDrift {
                    kind: DriftKind::VersionSuffix,
                    resumed: false,
                },
                "client_drift",
            ),
            (
                RelayError::ClientDrift {
                    kind: DriftKind::VersionSuffix,
                    resumed: true,
                },
                "client_drift",
            ),
            (RelayError::ClientRefused, "client_not_allowed"),
            (
                RelayError::GuardrailBlocked {
                    name: "Large auxiliary".into(),
                },
                "guardrail_blocked",
            ),
        ] {
            let failure = relay_failure(&error);
            assert_eq!(
                (
                    failure.status,
                    failure.error_type,
                    failure.code,
                    failure.no_retry
                ),
                (StatusCode::FORBIDDEN, "permission_error", code, true),
                "{error:?}"
            );
            for phrase in [
                "prompt is too long",
                "model:",
                "OAuth token has been revoked",
            ] {
                assert!(
                    !failure.message.contains(phrase),
                    "{error:?}: the client acts on {phrase:?} in a 4xx: {}",
                    failure.message
                );
            }
        }
    }

    /// A client that retries its 4xx answers would send a blocked request, one the gate, the contract or the drift
    /// check refused, or a refused credential again unchanged, so those answers carry the header, and none of the
    /// other arms this case lists does.
    #[test]
    fn no_retry_writes_the_header_and_no_other_arm_does() {
        let errors = [
            RelayError::AccountUuid,
            RelayError::BodyIncomplete,
            RelayError::BodyNotGzip,
            RelayError::BodyShape(BodyMember::Model),
            RelayError::BodyTimeout,
            RelayError::BodyTooDeep,
            RelayError::BodyTooLarge,
            RelayError::Cch(phoebis_cch::CchError::Json(0)),
            RelayError::InvalidModel,
            RelayError::LedgerBehind,
            RelayError::NoModel,
            RelayError::NotAnObject,
            RelayError::UnsupportedEncoding,
        ];
        let others = [
            ACCOUNT_CANCELED,
            METHOD_NOT_ALLOWED,
            MODEL_NOT_ALLOWED,
            MODEL_REJECTED,
            NOT_FOUND,
            ROUTER_UNAVAILABLE,
            UPSTREAM_CUT,
            UPSTREAM_STALLED,
            UPSTREAM_UNDECODABLE,
            UPSTREAM_UNREACHABLE,
            accounts_at_cap(5),
            body_budget_exhausted(),
            key_body_reads(),
            ledger_behind(),
        ]
        .into_iter()
        .chain(errors.iter().map(relay_failure));
        for failure in others {
            assert!(!failure.no_retry, "{}", failure.code);
            assert!(
                failure
                    .response("r-1")
                    .headers()
                    .get(SHOULD_RETRY_HEADER)
                    .is_none(),
                "{}",
                failure.code
            );
        }
        let blocked = relay_failure(&RelayError::GuardrailBlocked {
            name: "Large auxiliary".into(),
        })
        .response("r-1");
        assert_eq!(blocked.headers()[SHOULD_RETRY_HEADER], "false");
        assert_eq!(blocked.headers()[ERROR_CODE_HEADER], "guardrail_blocked");
        let gate = relay_failure(&RelayError::ClientRefused).response("r-1");
        assert_eq!(gate.headers()[SHOULD_RETRY_HEADER], "false");
        assert_eq!(gate.headers()[ERROR_CODE_HEADER], "client_not_allowed");
        let contract = relay_failure(&RelayError::ClientContract {
            item: "class_header",
        })
        .response("r-1");
        assert_eq!(contract.headers()[SHOULD_RETRY_HEADER], "false");
        assert_eq!(contract.headers()[ERROR_CODE_HEADER], "client_contract");
        for (kind, resumed) in [
            (DriftKind::Hash, false),
            (DriftKind::VersionSuffix, false),
            (DriftKind::VersionSuffix, true),
        ] {
            let drift = relay_failure(&RelayError::ClientDrift { kind, resumed }).response("r-1");
            assert_eq!(
                drift
                    .headers()
                    .get(SHOULD_RETRY_HEADER)
                    .and_then(|value| value.to_str().ok()),
                Some("false"),
                "{} resumed {resumed}",
                kind.as_str()
            );
            assert_eq!(drift.headers()[ERROR_CODE_HEADER], "client_drift");
        }
        for credential in [
            auth_failure(&AuthError::Ambiguous),
            auth_failure(&AuthError::Malformed),
            auth_failure(&AuthError::Missing),
            key_refusal(&KeyRefusal::Unknown),
        ] {
            assert!(credential.no_retry, "{}", credential.code);
            let refused = credential.response("r-1");
            assert_eq!(
                refused
                    .headers()
                    .get(SHOULD_RETRY_HEADER)
                    .and_then(|value| value.to_str().ok()),
                Some("false"),
                "{}",
                credential.code
            );
            assert_eq!(refused.headers()[ERROR_CODE_HEADER], credential.code);
        }
    }

    /// The routing log's exhausted row records the routing crate's status as its `answer`, so the builder the
    /// relay answers an exhaustion with must carry the status the client reference documents for the exhaustion's
    /// kind and at-cap reading.
    #[test]
    fn every_exhaustion_builder_answers_the_status_the_exhausted_row_records() {
        for (exhaustion, status) in [
            (
                Exhaustion::FastCredits {
                    reason: "out_of_credits".into(),
                },
                429,
            ),
            (Exhaustion::FastLane { deadline: 60_000 }, 429),
            (Exhaustion::ModelRejected, 404),
            (
                Exhaustion::Pinned {
                    obstacle: phoebis_routing::PinnedObstacle::Attempted,
                },
                503,
            ),
            (
                Exhaustion::Timed {
                    deadline: 60_000,
                    window: "five_hour".into(),
                },
                429,
            ),
            (Exhaustion::Untimed, 503),
        ] {
            let failure = crate::exhaustion::exhaustion_failure(&exhaustion, 0);
            assert_eq!(
                crate::exhaustion::exhaustion_response(&exhaustion, &failure, "r-1")
                    .status()
                    .as_u16(),
                status,
                "{exhaustion:?}"
            );
        }
        assert_eq!(
            accounts_at_cap(60).response("r-1").status().as_u16(),
            429,
            "the untimed exhaustion at the cap"
        );
    }
}
