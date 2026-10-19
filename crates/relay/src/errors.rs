//! Anthropic-shaped local failures, and the one table that maps every internal failure to a status, an
//! Anthropic error type, and the adapter's own error code.

use std::borrow::Cow;

use axum::body::Body;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;
use phoebis_routing::{exhaustion_status, Exhaustion, KeyRefusal};

use crate::auth::AuthError;
use crate::{BodyMember, RelayError};

/// Carries the adapter's own failure code, which is finer grained than the Anthropic error type a downstream
/// client reads, so an operator can tell two failures apart without a log line.
pub(crate) const ERROR_CODE_HEADER: &str = "x-phoebis-error-code";

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
    /// Writes `x-should-retry: false`, so a client that retries its 4xx answers does not resend what a guardrail
    /// or the contract refused.
    pub no_retry: bool,
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
        status: exhaustion_status_code(&Exhaustion::Untimed, true),
        error_type: "rate_limit_error",
        code: ACCOUNTS_AT_CAP,
        message: Cow::Borrowed("every account in this pool is at its in-flight cap"),
        retry_after: Some(retry_after_seconds.clamp(1, AT_CAP_RETRY_AFTER_MAX)),
        no_retry: false,
    }
}

/// A rejected caller credential is always a `401`: telling the caller which of the four ways it failed would
/// help an attacker enumerate, and the downstream client acts on the status alone.
pub(crate) fn auth_failure(error: &AuthError) -> Failure {
    let (code, message) = match error {
        AuthError::Ambiguous => ("auth_ambiguous", "present exactly one key"),
        AuthError::Malformed => ("auth_malformed", "the key header is malformed"),
        AuthError::Missing => ("auth_missing", "a key is required"),
        AuthError::Unknown => ("auth_unknown", "the presented key is not valid"),
    };
    Failure {
        status: StatusCode::UNAUTHORIZED,
        error_type: "authentication_error",
        code,
        message: Cow::Borrowed(message),
        retry_after: None,
        no_retry: false,
    }
}

/// The spellings `BodyMember::Repeated` names are constants so each member's spelling is written once and tested
/// once.
const REPEATED_BILLING_LINE: &str =
    "the request body carries the member \"system[0].text\" more than once";
const REPEATED_METADATA: &str =
    "the request body carries the top-level member \"metadata\" more than once";
const REPEATED_METADATA_USER_ID: &str =
    "the request body carries the member \"metadata.user_id\" more than once";
const REPEATED_MODEL: &str =
    "the request body carries the top-level member \"model\" more than once";
const REPEATED_SAFEGUARDS: &str =
    "the request body carries the top-level member \"safeguards\" more than once";
const REPEATED_STREAM: &str =
    "the request body carries the top-level member \"stream\" more than once";
const REPEATED_SYSTEM: &str =
    "the request body carries the top-level member \"system\" more than once";

/// The item's own message: what is missing, and the environment line that sends it.
fn contract_message(item: &str) -> &'static str {
    match item {
        "session_header" => "the request carries no x-claude-code-session-id header the router requires of a Claude Code client. Send the requests as Claude Code sends them",
        "class_header" => "the request carries no x-claude-code-request-class header the router requires of a Claude Code client. Set CLAUDE_CODE_GATEWAY_HINT_HEADERS=1",
        "attribution_line" => "the request carries no attribution line at system[0] the router requires of a Claude Code client. Leave CLAUDE_CODE_ATTRIBUTION_HEADER unset",
        "metadata_user_id" => "the request body has no metadata.user_id the router requires of a Claude Code client. Send the body as Claude Code sends it",
        "agent_id_header" => "the request names a subagent class and carries no x-claude-code-agent-id header the router requires of a Claude Code client. Send the requests as Claude Code sends them",
        "compaction_header" => "the request names the compaction class and carries no x-claude-code-compaction header the router requires of a Claude Code client. Set CLAUDE_CODE_GATEWAY_HINT_HEADERS=1",
        _ => unreachable!(
            "contract_gap only ever names session_header, class_header, attribution_line, metadata_user_id, agent_id_header or compaction_header"
        ),
    }
}

pub(crate) fn relay_failure(error: &RelayError) -> Failure {
    match error {
        RelayError::AccountUuid => Failure {
            status: StatusCode::SERVICE_UNAVAILABLE,
            error_type: "api_error",
            code: "accounts_unavailable",
            message: Cow::Borrowed("the bound account cannot serve requests"),
            retry_after: None,
            no_retry: false,
        },
        // The caller is already gone; the answer exists so the request still gets a row in the ledger.
        RelayError::BodyIncomplete => Failure {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            code: "request_incomplete",
            message: Cow::Borrowed("the request body ended before it was complete"),
            retry_after: None,
            no_retry: false,
        },
        RelayError::BodyNotGzip => Failure {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            code: "invalid_body",
            message: Cow::Borrowed("the request body is not valid gzip"),
            retry_after: None,
            no_retry: false,
        },
        // Naming the member is what lets a client author see which part of its body the relay has to find;
        // no value of the body is echoed back.
        RelayError::BodyShape(member) => Failure {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            code: phoebis_bus::BODY_SHAPE_ERROR_CODE,
            message: Cow::Borrowed(match member {
                BodyMember::BillingStamp => {
                    "the request body carries a billing stamp the relay can neither check nor restamp"
                }
                BodyMember::CacheControl => {
                    "the request body carries a cache_control marker the relay cannot place, so the pool's cache TTL policy cannot be applied to it"
                }
                BodyMember::MetadataUserId => {
                    "the request body has no metadata.user_id the account can be bound to"
                }
                BodyMember::Model => "the request body has no top-level model to resolve",
                BodyMember::Repeated(name) => match *name {
                    crate::body::BILLING_LINE_PATH => REPEATED_BILLING_LINE,
                    "metadata" => REPEATED_METADATA,
                    "metadata.user_id" => REPEATED_METADATA_USER_ID,
                    "model" => REPEATED_MODEL,
                    "safeguards" => REPEATED_SAFEGUARDS,
                    "stream" => REPEATED_STREAM,
                    "system" => REPEATED_SYSTEM,
                    _ => unreachable!(
                        "BodyMember::Repeated only ever names metadata, metadata.user_id, model, safeguards, stream, system or the billing line"
                    ),
                },
                BodyMember::Safeguards => {
                    "the request body carries a safeguards member the relay cannot place under this key's auto-mode setting"
                }
            }),
            retry_after: None,
            no_retry: false,
        },
        RelayError::BodyTimeout => Failure {
            status: StatusCode::REQUEST_TIMEOUT,
            error_type: "invalid_request_error",
            code: "request_timeout",
            message: Cow::Borrowed("the request body did not arrive in time"),
            retry_after: None,
            no_retry: false,
        },
        RelayError::BodyTooLarge => Failure {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            error_type: "request_too_large",
            code: "request_too_large",
            message: Cow::Borrowed("the request body is too large"),
            retry_after: None,
            no_retry: false,
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
        },
        RelayError::Cch(_) | RelayError::NotAnObject => Failure {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            code: "invalid_body",
            message: Cow::Borrowed("the request body is not a valid Messages request"),
            retry_after: None,
            no_retry: false,
        },
        RelayError::ClientContract { item } => Failure {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            code: "client_contract",
            message: Cow::Borrowed(contract_message(item)),
            retry_after: None,
            // The fix is the client's environment, so a retry resends what was refused.
            no_retry: true,
        },
        // Fail closed: a hash or a version suffix the pinned rules cannot reproduce means the client moved past
        // the profile. A stamp the router cannot verify never travels under an account's credential, so the
        // operator re-pins rather than the relay guessing what the upstream now expects. A resumed conversation's
        // suffix already reproduces over the empty prompt the client stamps, so its refusal is a moved client too,
        // and the words name the resume apart so the client and the operator see which conversation it was.
        RelayError::ClientDrift {
            kind: "version_suffix",
            resumed: true,
        } => Failure {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            code: "client_drift",
            message: Cow::Borrowed("the conversation was resumed, and the client's version suffix does not reproduce \
                      from the resume placeholder, the empty prompt or the user agent. Re-pin the router to this \
                      client version"),
            retry_after: None,
            no_retry: false,
        },
        RelayError::ClientDrift {
            kind: "version_suffix",
            resumed: false,
        } => Failure {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            code: "client_drift",
            message: Cow::Borrowed("the client's version suffix does not reproduce from the prompt and the user agent. \
                      Re-pin the router to this client version"),
            retry_after: None,
            no_retry: false,
        },
        RelayError::ClientDrift { .. } => Failure {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            code: "client_drift",
            message: Cow::Borrowed("the client's billing hash does not match the pinned profile. \
                      Re-pin the router to this client version"),
            retry_after: None,
            no_retry: false,
        },
        RelayError::GuardrailBlocked { .. } => Failure {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            code: "guardrail_blocked",
            message: error.to_string().into(),
            retry_after: None,
            no_retry: true,
        },
        RelayError::InvalidModel => Failure {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            code: "invalid_model",
            message: Cow::Borrowed("the request body's model is not a usable model name"),
            retry_after: None,
            no_retry: false,
        },
        RelayError::LedgerBehind => ledger_behind(),
        RelayError::NoModel => Failure {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            code: "missing_model",
            message: Cow::Borrowed("the request body has no model"),
            retry_after: None,
            no_retry: false,
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
        message: Cow::Borrowed("the presented key is not valid"),
        retry_after: None,
        no_retry: false,
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
    message: Cow::Borrowed("the bound account was disconnected while the request was in flight"),
    retry_after: None,
    no_retry: false,
};

pub(crate) const METHOD_NOT_ALLOWED: Failure = Failure {
    status: StatusCode::METHOD_NOT_ALLOWED,
    error_type: "invalid_request_error",
    code: "method_not_allowed",
    message: Cow::Borrowed("the method is not supported on this route"),
    retry_after: None,
    no_retry: false,
};

pub(crate) const MODEL_NOT_ALLOWED: Failure = Failure {
    status: StatusCode::NOT_FOUND,
    error_type: "not_found_error",
    code: "model_not_allowed",
    message: Cow::Borrowed("model not allowed for this key"),
    retry_after: None,
    no_retry: false,
};

pub(crate) const MODEL_REJECTED: Failure = Failure {
    status: exhaustion_status_code(&Exhaustion::ModelRejected, false),
    error_type: "not_found_error",
    code: "model_rejected",
    message: Cow::Borrowed("every account in this pool rejected the model"),
    retry_after: None,
    no_retry: false,
};

pub(crate) const NOT_FOUND: Failure = Failure {
    status: StatusCode::NOT_FOUND,
    error_type: "not_found_error",
    code: "not_found",
    message: Cow::Borrowed("the requested resource was not found"),
    retry_after: None,
    no_retry: false,
};

pub(crate) const ROUTER_UNAVAILABLE: Failure = Failure {
    status: StatusCode::SERVICE_UNAVAILABLE,
    error_type: "api_error",
    code: "router_unavailable",
    message: Cow::Borrowed("the router is not available"),
    retry_after: None,
    no_retry: false,
};

/// The upstream sent a head and then the connection was cut before the body ended, or the body ran past its bound.
pub(crate) const UPSTREAM_CUT: Failure = Failure {
    status: StatusCode::BAD_GATEWAY,
    error_type: "api_error",
    code: "upstream_cut",
    message: Cow::Borrowed("the upstream response could not be read after its head"),
    retry_after: None,
    no_retry: false,
};

/// The upstream sent a head and then went silent past the idle bound.
pub(crate) const UPSTREAM_STALLED: Failure = Failure {
    status: StatusCode::BAD_GATEWAY,
    error_type: "api_error",
    code: "upstream_stalled",
    message: Cow::Borrowed("the upstream stalled after its head"),
    retry_after: None,
    no_retry: false,
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
};

#[cfg(test)]
mod tests {
    use super::*;

    /// A drifted hash and a drifted version suffix are the same error code, but the message names which check
    /// failed, so an operator reads the right one off the response without a log line.
    #[test]
    fn a_client_drift_message_names_the_kind_that_failed() {
        let hash = relay_failure(&RelayError::ClientDrift {
            kind: "hash",
            resumed: false,
        });
        assert_eq!(hash.code, "client_drift");
        assert!(hash.message.contains("billing hash"), "{}", hash.message);
        let suffix = relay_failure(&RelayError::ClientDrift {
            kind: "version_suffix",
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

    /// A contract refusal's fix is the client's environment, so each message names the item the request lacks and
    /// the answer tells the client not to resend it unchanged. The words are the identity's own, so a word the
    /// message table spells differently answers the wrong message here.
    #[test]
    fn every_contract_message_names_its_item_and_ends_without_a_semicolon() {
        use crate::identity::{
            ITEM_AGENT_ID_HEADER, ITEM_ATTRIBUTION_LINE, ITEM_CLASS_HEADER, ITEM_COMPACTION_HEADER,
            ITEM_METADATA_USER_ID, ITEM_SESSION_HEADER,
        };
        for (item, name) in [
            (ITEM_SESSION_HEADER, "x-claude-code-session-id"),
            (ITEM_CLASS_HEADER, "x-claude-code-request-class"),
            (ITEM_ATTRIBUTION_LINE, "system[0]"),
            (ITEM_METADATA_USER_ID, "metadata.user_id"),
            (ITEM_AGENT_ID_HEADER, "x-claude-code-agent-id"),
            (ITEM_COMPACTION_HEADER, "x-claude-code-compaction"),
        ] {
            let failure = relay_failure(&RelayError::ClientContract { item });
            assert_eq!(
                (
                    failure.status,
                    failure.error_type,
                    failure.code,
                    failure.retry_after,
                    failure.no_retry
                ),
                (
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "client_contract",
                    None,
                    true
                ),
                "{item}"
            );
            assert!(
                failure.message.contains(name),
                "{item}: {}",
                failure.message
            );
            assert!(
                !failure.message.contains(';'),
                "{item}: {}",
                failure.message
            );
        }
    }

    #[test]
    fn every_body_shape_refusal_carries_the_code_the_ledger_reads_as_a_standing() {
        for member in [
            BodyMember::BillingStamp,
            BodyMember::CacheControl,
            BodyMember::MetadataUserId,
            BodyMember::Model,
            BodyMember::Repeated("metadata"),
            BodyMember::Repeated("metadata.user_id"),
            BodyMember::Repeated("model"),
            BodyMember::Repeated("safeguards"),
            BodyMember::Repeated("stream"),
            BodyMember::Repeated("system"),
            BodyMember::Repeated("system[0].text"),
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

    /// The name is the one part of the answer the operator wrote, so the arm formats it into the message and the
    /// answer says nothing else about the guardrail.
    #[test]
    fn the_guardrail_arm_formats_the_name() {
        let failure = relay_failure(&RelayError::GuardrailBlocked {
            name: "Large auxiliary".into(),
        });
        assert_eq!(
            (
                failure.status,
                failure.error_type,
                failure.code,
                failure.retry_after,
                failure.no_retry
            ),
            (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "guardrail_blocked",
                None,
                true
            )
        );
        assert_eq!(failure.message, r#"blocked by guardrail "Large auxiliary""#);
        assert!(
            !failure.message.contains("prompt is too long"),
            "the client reads that phrase in a 400 as its compaction trigger"
        );
    }

    /// A client that retries its 4xx answers would send a blocked request or one the contract refused again
    /// unchanged, and no other refusal asks the client to stop, so the header rides those two answers alone.
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
            RelayError::ClientDrift {
                kind: "hash",
                resumed: false,
            },
            RelayError::ClientDrift {
                kind: "version_suffix",
                resumed: false,
            },
            RelayError::ClientDrift {
                kind: "version_suffix",
                resumed: true,
            },
            RelayError::InvalidModel,
            RelayError::LedgerBehind,
            RelayError::NoModel,
            RelayError::NotAnObject,
            RelayError::UnsupportedEncoding,
        ];
        let others = [
            ACCOUNT_CANCELLED,
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
            auth_failure(&AuthError::Ambiguous),
            auth_failure(&AuthError::Malformed),
            auth_failure(&AuthError::Missing),
            auth_failure(&AuthError::Unknown),
            body_budget_exhausted(),
            key_body_reads(),
            key_refusal(&KeyRefusal::Unknown),
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
        let contract = relay_failure(&RelayError::ClientContract {
            item: "class_header",
        })
        .response("r-1");
        assert_eq!(contract.headers()[SHOULD_RETRY_HEADER], "false");
        assert_eq!(contract.headers()[ERROR_CODE_HEADER], "client_contract");
    }

    /// The routing log's exhausted row records the routing crate's status as its `answer`, so the builder the
    /// relay answers an exhaustion with must carry that same status for the exhaustion's kind and at-cap reading.
    #[test]
    fn every_exhaustion_builder_answers_the_status_the_exhausted_row_records() {
        for exhaustion in [
            Exhaustion::FastCredits {
                reason: "out_of_credits".into(),
            },
            Exhaustion::FastLane { deadline: 60_000 },
            Exhaustion::ModelRejected,
            Exhaustion::Pinned {
                obstacle: "attempted",
            },
            Exhaustion::Timed {
                deadline: 60_000,
                window: "five_hour".into(),
            },
            Exhaustion::Untimed,
        ] {
            for at_cap in [false, true] {
                let response = if exhaustion == Exhaustion::Untimed && at_cap {
                    accounts_at_cap(60).response("r-1")
                } else {
                    crate::exhaustion::exhaustion_response(&exhaustion, 0, "r-1")
                };
                assert_eq!(
                    response.status().as_u16(),
                    exhaustion_status(&exhaustion, at_cap),
                    "{exhaustion:?} at_cap {at_cap}"
                );
            }
        }
    }
}
