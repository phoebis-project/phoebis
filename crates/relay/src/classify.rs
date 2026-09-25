//! The cooldown classification table, partitioned by account kind, applied to every upstream head before any
//! byte reaches the caller.

use phoebis_routing::MODEL_REJECTED_REASON;
use phoebis_store::{AccountKind, AccountState};
use phoebis_transport::response::ResponseHead;
use serde_json::Value;

use crate::memory::Injected;
use crate::ratelimit::{retry_after, UnifiedHeaders};

/// Deadlines carry a second of margin so a request sent at the reset is not refused by a clock a hair behind.
const MARGIN_MS: i64 = 1_000;
const PERMISSION_MS: i64 = 3_600_000;

/// The window name a rejection row carries; the console shows it beside the deadline.
pub const MODEL_REJECTION_WINDOW: &str = "model_rejection";

/// The reason row an api_key account's monthly spend cap cools under.
pub const SPEND_CAP_REASON: &str = "spend_cap";

/// 00:00 UTC on the first day of the month after the one `now` falls in.
pub fn next_month_start(now: i64) -> i64 {
    let date = jiff::Timestamp::from_millisecond(now)
        .expect("a routed clock value stays in jiff's timestamp range")
        .to_zoned(jiff::tz::TimeZone::UTC)
        .date();
    let next = date
        .first_of_month()
        .checked_add(jiff::Span::new().months(1))
        .expect("one month past any first-of-month date stays in jiff's date range");
    next.to_zoned(jiff::tz::TimeZone::UTC)
        .expect("every civil midnight exists in UTC")
        .timestamp()
        .as_millisecond()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Account,
    Model,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cooldown {
    pub scope: Scope,
    pub until: i64,
    pub reason: &'static str,
    pub window: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Commit,
    FailOver {
        cooldown: Option<Cooldown>,
        mark: Option<AccountState>,
    },
    Refresh,
    /// An upstream 400 `invalid_request_error` whose message names one or more items the relay injected: the
    /// items to strip before one more try on the same account.
    StripInjected(Vec<Injected>),
}

pub struct HeadContext<'a> {
    pub kind: AccountKind,
    /// The model the caller writes a model-scoped `Cooldown` row under.
    pub canonical_model: &'a str,
    pub now: i64,
    pub fallback_ms: i64,
    /// The first SSE event's data, decoded: the caller decodes its inspection copy, so this is never compressed.
    pub first_event: Option<&'a [u8]>,
    /// The start of a non-streaming body, decoded: the caller decodes its inspection copy, so this is never
    /// compressed.
    pub body_prefix: Option<&'a [u8]>,
    /// The attempt's own additions, body and head, that a 400 might name.
    pub injected: &'a [Injected],
    /// Whether this leg has not stripped yet. A strip retry spends no slot of `max_attempts`, so the budget is
    /// not read here; the one-round rule is the only gate.
    pub can_strip: bool,
    /// The spelling the body carried upstream, which a rejection's message names.
    pub wire_model: &'a str,
    /// `routing.model_rejection_seconds` in milliseconds.
    pub model_rejection_ms: i64,
}

pub fn classify(head: &ResponseHead, ctx: &HeadContext<'_>) -> Verdict {
    let unified = UnifiedHeaders::parse(head);
    let sse_rate_limited = head.status == 200
        && ctx
            .first_event
            .is_some_and(|e| error_type(e).as_deref() == Some("rate_limit_error"));
    let rate_limited = head.status == 429 || sse_rate_limited;
    // Ahead of the per-kind table below, for either account kind: a 400 naming an item this leg added is a fact
    // about the request shape, not about the credential, and a name outside `injected` is the client's own to
    // handle.
    if head.status == 400 && ctx.can_strip && !ctx.injected.is_empty() {
        if let Some(body) = ctx
            .body_prefix
            .filter(|b| error_type(b).as_deref() == Some("invalid_request_error"))
        {
            let named = refused_names(body, ctx.injected);
            if !named.is_empty() {
                return Verdict::StripInjected(named);
            }
        }
    }
    // Ahead of the per-kind table, for either kind: a 404 naming the model the body carried is the upstream's word
    // that it does not serve that model, a fact about the (account, model) pair alone. A 404 naming nothing, and
    // every 400 and 422, are the client's own to read and fall through to `Commit`.
    if head.status == 404
        && ctx.body_prefix.is_some_and(|b| {
            error_type(b).as_deref() == Some("not_found_error") && names_model(b, ctx.wire_model)
        })
    {
        return Verdict::FailOver {
            cooldown: Some(Cooldown {
                scope: Scope::Model,
                until: ctx
                    .now
                    .saturating_add(ctx.model_rejection_ms)
                    .saturating_add(MARGIN_MS),
                reason: MODEL_REJECTED_REASON,
                window: MODEL_REJECTION_WINDOW.into(),
            }),
            mark: None,
        };
    }
    match ctx.kind {
        AccountKind::Oauth if rate_limited => {
            return Verdict::FailOver {
                cooldown: Some(oauth_cooldown(head, unified.as_ref(), ctx)),
                mark: None,
            }
        }
        AccountKind::ApiKey if rate_limited && ctx.body_prefix.is_some_and(names_spend_limit) => {
            return Verdict::FailOver {
                cooldown: Some(Cooldown {
                    scope: Scope::Account,
                    until: next_month_start(ctx.now).saturating_add(MARGIN_MS),
                    reason: SPEND_CAP_REASON,
                    window: "overage".into(),
                }),
                mark: None,
            }
        }
        AccountKind::ApiKey if rate_limited => {
            return Verdict::FailOver {
                cooldown: Some(api_key_cooldown(head, ctx)),
                mark: None,
            }
        }
        AccountKind::ApiKey
            if head.status == 400
                && ctx.body_prefix.is_some_and(|b| {
                    error_type(b).as_deref() == Some("invalid_request_error")
                        && names_credit_balance(b)
                }) =>
        {
            return Verdict::FailOver {
                cooldown: None,
                mark: Some(AccountState::FundingRequired),
            };
        }
        _ => {}
    }
    let auth = head.status == 401
        || (head.status == 403
            && ctx
                .body_prefix
                .is_some_and(|b| error_type(b).as_deref() == Some("authentication_error")));
    if auth {
        return match ctx.kind {
            AccountKind::Oauth => Verdict::Refresh,
            AccountKind::ApiKey => Verdict::FailOver {
                cooldown: None,
                mark: Some(AccountState::ReauthRequired),
            },
        };
    }
    if head.status == 403
        && ctx
            .body_prefix
            .is_some_and(|b| error_type(b).as_deref() == Some("permission_error"))
    {
        return Verdict::FailOver {
            cooldown: Some(Cooldown {
                scope: Scope::Model,
                until: ctx
                    .now
                    .saturating_add(PERMISSION_MS)
                    .saturating_add(MARGIN_MS),
                reason: "permission",
                window: "retry_after".into(),
            }),
            mark: None,
        };
    }
    Verdict::Commit
}

fn oauth_cooldown(
    head: &ResponseHead,
    unified: Option<&UnifiedHeaders>,
    ctx: &HeadContext<'_>,
) -> Cooldown {
    let rejected = |s: &Option<String>| s.as_deref() == Some("rejected");
    if let Some(u) = unified {
        let shared = rejected(&u.five_hour_status)
            || rejected(&u.seven_day_status)
            || (rejected(&u.status) && !fable_only(u));
        if shared {
            let reset = [
                u.five_hour_reset.filter(|_| rejected(&u.five_hour_status)),
                u.seven_day_reset.filter(|_| rejected(&u.seven_day_status)),
                u.reset,
            ]
            .into_iter()
            .flatten()
            .max();
            let until = reset
                .or_else(|| retry_after(head, ctx.now).map(|d| ctx.now.saturating_add(d)))
                .unwrap_or(ctx.now.saturating_add(ctx.fallback_ms));
            return Cooldown {
                scope: Scope::Account,
                until: until.saturating_add(MARGIN_MS),
                reason: "rate_limit",
                window: exhaustion_window(Some(u), AccountKind::Oauth),
            };
        }
        if fable_only(u) && (rejected(&u.seven_day_oi_status) || rejected(&u.status)) {
            // Same deadline rule as the shared row: the later of the rejected window's own reset and the
            // top-level reset, which only ever lengthens the cooldown when it wins.
            let reset = [u.seven_day_oi_reset, u.reset].into_iter().flatten().max();
            let until = reset
                .or_else(|| retry_after(head, ctx.now).map(|d| ctx.now.saturating_add(d)))
                .unwrap_or(ctx.now.saturating_add(ctx.fallback_ms));
            return Cooldown {
                scope: Scope::Model,
                until: until.saturating_add(MARGIN_MS),
                reason: "rate_limit",
                window: exhaustion_window(Some(u), AccountKind::Oauth),
            };
        }
    }
    let until = retry_after(head, ctx.now)
        .map(|d| ctx.now.saturating_add(d))
        .unwrap_or(ctx.now.saturating_add(ctx.fallback_ms));
    Cooldown {
        scope: Scope::Account,
        until: until.saturating_add(MARGIN_MS),
        reason: "rate_limit",
        window: exhaustion_window(unified, AccountKind::Oauth),
    }
}

/// A rejection whose only rejected window is the model-scoped one. Both shared statuses have to be present and
/// exactly `allowed`: neither an absent status nor an unrecognized third value is evidence that the shared
/// window is fine, and a model-only cooldown on that basis would keep sending an exhausted account traffic.
fn fable_only(u: &UnifiedHeaders) -> bool {
    let allowed = |s: &Option<String>| s.as_deref() == Some("allowed");
    let shared_fine = allowed(&u.five_hour_status) && allowed(&u.seven_day_status);
    shared_fine
        && (u.seven_day_oi_status.as_deref() == Some("rejected")
            || u.representative_claim
                .as_deref()
                .is_some_and(|c| c.starts_with("seven_day_")))
}

fn api_key_cooldown(head: &ResponseHead, ctx: &HeadContext<'_>) -> Cooldown {
    match retry_after(head, ctx.now) {
        Some(delay) => Cooldown {
            scope: Scope::Model,
            until: ctx.now.saturating_add(delay).saturating_add(MARGIN_MS),
            reason: "rate_limit",
            window: "retry_after".into(),
        },
        None => Cooldown {
            scope: Scope::Account,
            until: ctx
                .now
                .saturating_add(ctx.fallback_ms)
                .saturating_add(MARGIN_MS),
            reason: "rate_limit",
            window: "retry_after".into(),
        },
    }
}

pub fn exhaustion_window(unified: Option<&UnifiedHeaders>, kind: AccountKind) -> String {
    if kind == AccountKind::Oauth {
        if let Some(u) = unified {
            if let Some(claim) = &u.representative_claim {
                return claim.clone();
            }
            if u.five_hour_status.as_deref() == Some("rejected") {
                return "five_hour".into();
            }
            if u.seven_day_status.as_deref() == Some("rejected") {
                return "seven_day".into();
            }
        }
    }
    "retry_after".into()
}

/// The value of a `"key": "value"` member, scanned textually in a body a JSON parse could not complete: the
/// search starts after the first `"error"` key and trusts only a `"key"` that opens an object or follows a
/// sibling field, never one embedded in a string value.
fn textual_member<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!(r#""{key}""#);
    let error_at = text.find(r#""error""#)?;
    let mut cursor = error_at;
    while let Some(offset) = text[cursor..].find(needle.as_str()) {
        let match_at = cursor + offset;
        let before = text[..match_at].trim_end();
        if before.ends_with('{') || before.ends_with(',') {
            let after = text[match_at + needle.len()..]
                .trim_start()
                .strip_prefix(':')?
                .trim_start()
                .strip_prefix('"')?;
            return after.split('"').next();
        }
        cursor = match_at + needle.len();
    }
    None
}

/// The error body's `error.type`. A complete body parses directly, which is what keeps a `"type"` string that
/// happens to appear inside an upstream `message` from being mistaken for the real type. A prefix cut
/// mid-document fails to parse, so the type is then located textually through `textual_member`.
pub(crate) fn error_type(body: &[u8]) -> Option<String> {
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        return value
            .get("error")
            .and_then(|e| e.get("type"))
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    let text = std::str::from_utf8(body).ok()?;
    textual_member(text, "type").map(str::to_string)
}

fn names_credit_balance(body: &[u8]) -> bool {
    std::str::from_utf8(body).is_ok_and(|t| t.to_ascii_lowercase().contains("credit balance"))
}

/// Whether the body names the upstream's own monthly spend-cap code: a complete body reads
/// `error.details.error_code`, a body cut mid-document falls back to `textual_member`.
fn names_spend_limit(body: &[u8]) -> bool {
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        return value["error"]["details"]["error_code"].as_str()
            == Some("enforced_spend_limit_reached");
    }
    let Ok(text) = std::str::from_utf8(body) else {
        return false;
    };
    textual_member(text, "error_code") == Some("enforced_spend_limit_reached")
}

/// Whether `error.message` names `wire_model` as a whole token: the byte before and after each occurrence are
/// absent (the message's ends) or a boundary, where a boundary is any byte that is neither ASCII alphanumeric
/// nor `-` nor `.` (quotes, whitespace, `:`, `,`, and the rest of punctuation all bound). An empty `wire_model`
/// never matches, since every message contains the empty string.
fn names_model(body: &[u8], wire_model: &str) -> bool {
    if wire_model.is_empty() {
        return false;
    }
    let Some(message) = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_ascii_lowercase))
    else {
        return false;
    };
    let wire_model = wire_model.to_ascii_lowercase();
    let boundary =
        |c: Option<char>| c.is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '.'));
    message.match_indices(&wire_model).any(|(at, _)| {
        boundary(message[..at].chars().next_back())
            && boundary(message[at + wire_model.len()..].chars().next())
    })
}

/// The names an upstream 400 refused, read off `error.message` alone (never the envelope, where a field such as
/// `request_id` can spell any of these names by accident), each as a whole token: bounded on both sides by a
/// character outside `[A-Za-z0-9_-]` or the string's edge.
pub(crate) fn refused_names(body: &[u8], candidates: &[Injected]) -> Vec<Injected> {
    let Some(message) = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
    else {
        return Vec::new();
    };
    let boundary =
        |c: Option<char>| c.is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'));
    candidates
        .iter()
        .copied()
        .filter(|item| {
            let name = item.wire_name();
            message.match_indices(name).any(|(at, _)| {
                boundary(message[..at].chars().next_back())
                    && boundary(message[at + name.len()..].chars().next())
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoebis_store::AccountKind;
    use phoebis_transport::response::ResponseHead;

    const NOW: i64 = 1_700_000_000_000;
    const FALLBACK: i64 = 60_000;

    fn head(status: u16, headers: &[(&str, &str)]) -> ResponseHead {
        ResponseHead {
            status,
            headers: headers
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
        }
    }

    fn context<'a>(kind: AccountKind) -> HeadContext<'a> {
        HeadContext {
            kind,
            canonical_model: "claude-opus-5",
            now: NOW,
            fallback_ms: FALLBACK,
            first_event: None,
            body_prefix: None,
            injected: &[],
            can_strip: false,
            wire_model: "claude-opus-5",
            model_rejection_ms: 3_600_000,
        }
    }

    fn cooldown_of(verdict: Verdict) -> Cooldown {
        match verdict {
            Verdict::FailOver {
                cooldown: Some(c), ..
            } => c,
            other => panic!("expected a cooldown, got {other:?}"),
        }
    }

    fn ms(y: i16, m: i8, d: i8, h: i8, min: i8) -> i64 {
        jiff::civil::date(y, m, d)
            .at(h, min, 0, 0)
            .to_zoned(jiff::tz::TimeZone::UTC)
            .unwrap()
            .timestamp()
            .as_millisecond()
    }

    fn spend_cap_body() -> Vec<u8> {
        serde_json::json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": "This organization has reached its spend limit.",
                "details": {"error_code": "enforced_spend_limit_reached"}
            }
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn the_spend_cap_429_cools_the_whole_account_until_the_first_of_next_month_under_the_overage_window(
    ) {
        let body = spend_cap_body();
        let mut ctx = context(AccountKind::ApiKey);
        ctx.now = ms(2026, 9, 25, 12, 34);
        ctx.body_prefix = Some(&body);
        let c = cooldown_of(classify(&head(429, &[]), &ctx));
        assert_eq!(
            (c.scope, c.reason, c.window.as_str()),
            (Scope::Account, SPEND_CAP_REASON, "overage")
        );
        assert_eq!(c.until, ms(2026, 10, 1, 0, 0) + MARGIN_MS);
        // A rejection in December wraps into January of the following year.
        ctx.now = ms(2026, 12, 31, 23, 59);
        let c = cooldown_of(classify(&head(429, &[]), &ctx));
        assert_eq!(c.until, ms(2027, 1, 1, 0, 0) + MARGIN_MS);
        // A rejection landing exactly at the first of the month still cools until the next month's first.
        ctx.now = ms(2026, 9, 1, 0, 0);
        let c = cooldown_of(classify(&head(429, &[]), &ctx));
        assert_eq!(c.until, ms(2026, 10, 1, 0, 0) + MARGIN_MS);
        // A plain rate limit that names no spend-cap code falls through to the ordinary api_key rule.
        ctx.body_prefix = None;
        let c = cooldown_of(classify(&head(429, &[]), &ctx));
        assert_ne!(c.reason, SPEND_CAP_REASON);
    }

    /// The three shapes a 429 body reaches the classifier in when the upstream cuts it off mid-document: the
    /// code fully written and quoted, the code's own text cut short, and the code cut before it ever appears.
    #[test]
    fn a_cut_prefix_names_the_spend_cap_code_once_its_value_closes_and_not_before() {
        let full = spend_cap_body();
        let text = std::str::from_utf8(&full).unwrap();
        const CODE: &str = "enforced_spend_limit_reached";
        let code_at = text.find(CODE).unwrap();
        let code_end = code_at + CODE.len();

        // Cut right after the value's closing quote: the object never closes, so this is not complete JSON, and
        // the textual scan is what `names_spend_limit` falls back to.
        let cut_after_value = &full[..code_end + 1];
        assert!(serde_json::from_slice::<Value>(cut_after_value).is_err());
        assert!(names_spend_limit(cut_after_value));

        // Cut mid-value: the code's own text is incomplete, so it must not match.
        let cut_mid_value = &full[..code_end - 5];
        assert!(serde_json::from_slice::<Value>(cut_mid_value).is_err());
        assert!(!names_spend_limit(cut_mid_value));

        // Cut before the key ever appears: absent entirely.
        let key_at = text.find(r#""error_code""#).unwrap();
        let cut_before_key = &full[..key_at];
        assert!(serde_json::from_slice::<Value>(cut_before_key).is_err());
        assert!(!names_spend_limit(cut_before_key));
    }

    #[test]
    fn an_oauth_429_with_a_rejected_shared_window_cools_the_account_until_its_reset() {
        let h = head(
            429,
            &[
                ("anthropic-ratelimit-unified-5h-status", "rejected"),
                ("anthropic-ratelimit-unified-5h-reset", "1700003600"),
                ("retry-after", "5"),
            ],
        );
        let c = cooldown_of(classify(&h, &context(AccountKind::Oauth)));
        assert_eq!(
            (c.scope, c.until, c.window.as_str()),
            (Scope::Account, 1_700_003_600_000 + 1_000, "five_hour")
        );
    }

    #[test]
    fn a_fable_only_rejection_cools_only_the_requested_model() {
        let h = head(
            429,
            &[
                ("anthropic-ratelimit-unified-status", "rejected"),
                (
                    "anthropic-ratelimit-unified-representative-claim",
                    "seven_day_fable",
                ),
                ("anthropic-ratelimit-unified-5h-status", "allowed"),
                ("anthropic-ratelimit-unified-7d-status", "allowed"),
                ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
                ("anthropic-ratelimit-unified-7d_oi-reset", "1700007200"),
            ],
        );
        let c = cooldown_of(classify(&h, &context(AccountKind::Oauth)));
        assert_eq!(
            (c.scope, c.until, c.window.as_str()),
            (Scope::Model, 1_700_007_200_000 + 1_000, "seven_day_fable")
        );
    }

    #[test]
    fn a_model_only_rejection_without_both_shared_statuses_cools_the_account() {
        let h = head(
            429,
            &[
                ("anthropic-ratelimit-unified-status", "rejected"),
                ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
                ("anthropic-ratelimit-unified-7d_oi-reset", "1700007200"),
            ],
        );
        let c = cooldown_of(classify(&h, &context(AccountKind::Oauth)));
        assert_eq!(
            c.scope,
            Scope::Account,
            "absent shared statuses are not evidence that the shared windows are fine"
        );
        let h = head(
            429,
            &[
                ("anthropic-ratelimit-unified-5h-status", "allowed"),
                ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
            ],
        );
        assert_eq!(
            cooldown_of(classify(&h, &context(AccountKind::Oauth))).scope,
            Scope::Account,
            "one shared status is not both"
        );
    }

    #[test]
    fn a_third_shared_status_value_is_not_treated_as_allowed() {
        let h = head(
            429,
            &[
                ("anthropic-ratelimit-unified-status", "rejected"),
                ("anthropic-ratelimit-unified-5h-status", "throttled"),
                ("anthropic-ratelimit-unified-7d-status", "allowed"),
                ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
                ("anthropic-ratelimit-unified-7d_oi-reset", "1700007200"),
            ],
        );
        let c = cooldown_of(classify(&h, &context(AccountKind::Oauth)));
        assert_eq!(
            c.scope,
            Scope::Account,
            "a shared status other than allowed or rejected is not evidence the window is fine"
        );
    }

    #[test]
    fn an_oauth_429_without_unified_headers_takes_retry_after_then_the_fallback() {
        let c = cooldown_of(classify(
            &head(429, &[("retry-after", "30")]),
            &context(AccountKind::Oauth),
        ));
        assert_eq!(
            (c.scope, c.until, c.window.as_str()),
            (Scope::Account, NOW + 30_000 + 1_000, "retry_after")
        );
        let c = cooldown_of(classify(&head(429, &[]), &context(AccountKind::Oauth)));
        assert_eq!(c.until, NOW + FALLBACK + 1_000);
    }

    #[test]
    fn an_api_key_429_is_model_scoped_with_retry_after_and_account_wide_without_a_valid_one() {
        let c = cooldown_of(classify(
            &head(429, &[("retry-after", "12")]),
            &context(AccountKind::ApiKey),
        ));
        assert_eq!((c.scope, c.until), (Scope::Model, NOW + 12_000 + 1_000));
        let c = cooldown_of(classify(
            &head(429, &[("retry-after", "later")]),
            &context(AccountKind::ApiKey),
        ));
        assert_eq!((c.scope, c.until), (Scope::Account, NOW + FALLBACK + 1_000));
        let c = cooldown_of(classify(
            &head(
                429,
                &[
                    ("anthropic-ratelimit-unified-5h-status", "rejected"),
                    ("anthropic-ratelimit-unified-5h-reset", "1700003600"),
                ],
            ),
            &context(AccountKind::ApiKey),
        ));
        assert_eq!(
            c.scope,
            Scope::Account,
            "unified headers do not apply to the api_key partition"
        );
    }

    #[test]
    fn an_extreme_retry_after_saturates_instead_of_overflowing() {
        let c = cooldown_of(classify(
            &head(429, &[("retry-after", "1e18")]),
            &context(AccountKind::ApiKey),
        ));
        assert_eq!(c.until, i64::MAX);
    }

    #[test]
    fn the_first_sse_event_error_uses_the_partition_rows_and_a_credit_400_marks_funding() {
        let mut ctx = context(AccountKind::Oauth);
        ctx.first_event =
            Some(br#"{"type":"error","error":{"type":"rate_limit_error","message":"slow"}}"#);
        let h = head(
            200,
            &[
                ("content-type", "text/event-stream"),
                ("anthropic-ratelimit-unified-7d-status", "rejected"),
                ("anthropic-ratelimit-unified-7d-reset", "1700086400"),
            ],
        );
        let c = cooldown_of(classify(&h, &ctx));
        assert_eq!((c.scope, c.window.as_str()), (Scope::Account, "seven_day"));
        ctx.first_event =
            Some(br#"{"type":"error","error":{"type":"overloaded_error","message":"x"}}"#);
        assert!(
            matches!(classify(&h, &ctx), Verdict::Commit),
            "any other pre-commit error commits"
        );
        let mut api = context(AccountKind::ApiKey);
        api.body_prefix = Some(br#"{"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low"}}"#);
        assert!(matches!(
            classify(&head(400, &[]), &api),
            Verdict::FailOver {
                cooldown: None,
                mark: Some(AccountState::FundingRequired)
            }
        ));
    }

    #[test]
    fn the_api_key_200_sse_first_event_error_is_model_scoped_with_retry_after() {
        let mut ctx = context(AccountKind::ApiKey);
        ctx.first_event =
            Some(br#"{"type":"error","error":{"type":"rate_limit_error","message":"slow"}}"#);
        let h = head(
            200,
            &[("content-type", "text/event-stream"), ("retry-after", "9")],
        );
        let c = cooldown_of(classify(&h, &ctx));
        assert_eq!((c.scope, c.until), (Scope::Model, NOW + 9_000 + 1_000));
    }

    #[test]
    fn shared_rows_refresh_permission_and_forward() {
        assert!(matches!(
            classify(&head(401, &[]), &context(AccountKind::Oauth)),
            Verdict::Refresh
        ));
        assert!(matches!(
            classify(&head(401, &[]), &context(AccountKind::ApiKey)),
            Verdict::FailOver {
                cooldown: None,
                mark: Some(AccountState::ReauthRequired)
            }
        ));
        let mut ctx = context(AccountKind::Oauth);
        ctx.body_prefix =
            Some(br#"{"type":"error","error":{"type":"permission_error","message":"no"}}"#);
        let c = cooldown_of(classify(&head(403, &[]), &ctx));
        assert_eq!(
            (c.scope, c.until, c.reason),
            (Scope::Model, NOW + 3_600_000 + 1_000, "permission")
        );
        ctx.body_prefix =
            Some(br#"{"type":"error","error":{"type":"authentication_error","message":"no"}}"#);
        assert!(matches!(classify(&head(403, &[]), &ctx), Verdict::Refresh));
        assert!(matches!(
            classify(&head(529, &[]), &context(AccountKind::Oauth)),
            Verdict::Commit
        ));
        assert!(matches!(
            classify(&head(500, &[]), &context(AccountKind::ApiKey)),
            Verdict::Commit
        ));
        assert!(matches!(
            classify(&head(200, &[]), &context(AccountKind::Oauth)),
            Verdict::Commit
        ));
    }

    #[test]
    fn a_message_mentioning_type_text_does_not_steer_a_complete_body() {
        let mut ctx = context(AccountKind::Oauth);
        ctx.body_prefix = Some(br#"{"type":"error","error":{"type":"invalid_request_error","message":"contains \"type\": \"permission_error\" in the text"}}"#);
        assert!(
            matches!(classify(&head(403, &[]), &ctx), Verdict::Commit),
            "the real type comes from the parsed body, not the first \"type\" match"
        );
    }

    #[test]
    fn the_exhaustion_window_prefers_the_claim_then_the_rejected_window_then_retry_after() {
        let claim = UnifiedHeaders::parse(&head(
            429,
            &[
                (
                    "anthropic-ratelimit-unified-representative-claim",
                    "seven_day_opus",
                ),
                ("anthropic-ratelimit-unified-5h-status", "rejected"),
            ],
        ))
        .unwrap();
        assert_eq!(
            exhaustion_window(Some(&claim), AccountKind::Oauth),
            "seven_day_opus"
        );
        let seven = UnifiedHeaders::parse(&head(
            429,
            &[("anthropic-ratelimit-unified-7d-status", "rejected")],
        ))
        .unwrap();
        assert_eq!(
            exhaustion_window(Some(&seven), AccountKind::Oauth),
            "seven_day"
        );
        assert_eq!(exhaustion_window(None, AccountKind::ApiKey), "retry_after");
    }

    fn refusal(message: &str) -> Vec<u8> {
        format!(
            r#"{{"type":"error","error":{{"type":"invalid_request_error","message":"{message}"}}}}"#
        )
        .into_bytes()
    }

    /// Built through `serde_json::json!` rather than a format string, so a message that itself quotes the
    /// model still produces a valid body.
    fn not_found(message: &str) -> Vec<u8> {
        serde_json::json!({"type": "error", "error": {"type": "not_found_error", "message": message}})
            .to_string()
            .into_bytes()
    }

    #[test]
    fn a_400_naming_one_or_two_injected_items_strips_exactly_those() {
        let one = refusal(
            "Unexpected value(s) `cache-diagnosis-2026-04-07` for the `anthropic-beta` header.",
        );
        let mut ctx = context(AccountKind::Oauth);
        ctx.can_strip = true;
        ctx.injected = &[Injected::CacheDiagnosisBeta, Injected::Diagnostics];
        ctx.body_prefix = Some(&one);
        assert_eq!(
            classify(&head(400, &[]), &ctx),
            Verdict::StripInjected(vec![Injected::CacheDiagnosisBeta])
        );
        let both = refusal("`cache-diagnosis-2026-04-07` and `diagnostics` are not recognized.");
        ctx.body_prefix = Some(&both);
        assert_eq!(
            classify(&head(400, &[]), &ctx),
            Verdict::StripInjected(vec![Injected::CacheDiagnosisBeta, Injected::Diagnostics])
        );
    }

    #[test]
    fn a_400_naming_a_name_outside_injected_or_of_another_type_commits() {
        let unnamed = refusal("model not found");
        let mut ctx = context(AccountKind::Oauth);
        ctx.can_strip = true;
        ctx.injected = &[Injected::CacheDiagnosisBeta];
        ctx.body_prefix = Some(&unnamed);
        assert!(matches!(classify(&head(400, &[]), &ctx), Verdict::Commit));
        let other_type = br#"{"type":"error","error":{"type":"overloaded_error","message":"cache-diagnosis-2026-04-07"}}"#;
        ctx.body_prefix = Some(other_type);
        assert!(matches!(classify(&head(400, &[]), &ctx), Verdict::Commit));
    }

    #[test]
    fn a_400_on_a_leg_that_already_stripped_commits_whatever_the_message_names() {
        let message = refusal("Unexpected value(s) `cache-diagnosis-2026-04-07`");
        let mut ctx = context(AccountKind::Oauth);
        ctx.can_strip = false;
        ctx.injected = &[Injected::CacheDiagnosisBeta];
        ctx.body_prefix = Some(&message);
        assert!(matches!(classify(&head(400, &[]), &ctx), Verdict::Commit));
    }

    #[test]
    fn refused_names_matches_whole_tokens_and_ignores_the_envelope() {
        let envelope = br#"{"type":"error","error":{"type":"invalid_request_error","message":"model not found"},"request_id":"req_cch_0001"}"#;
        assert_eq!(refused_names(envelope, &[Injected::Cch]), Vec::new());
        let unrelated_field =
            br#"{"type":"error","error":{"type":"invalid_request_error","message":"`diagnostics_extra` is not a valid field"}}"#;
        assert_eq!(
            refused_names(unrelated_field, &[Injected::Diagnostics]),
            Vec::new()
        );
        let backtick_boundary = refusal("Unexpected value(s) `cache-diagnosis-2026-04-07`");
        assert_eq!(
            refused_names(&backtick_boundary, &[Injected::CacheDiagnosisBeta]),
            vec![Injected::CacheDiagnosisBeta]
        );
        let period_boundary = refusal("unknown field diagnostics.");
        assert_eq!(
            refused_names(&period_boundary, &[Injected::Diagnostics]),
            vec![Injected::Diagnostics]
        );
    }

    #[test]
    fn a_404_naming_the_wire_model_fails_over_with_a_model_scoped_rejection_cooldown() {
        for kind in [AccountKind::Oauth, AccountKind::ApiKey] {
            for message in [
                "model: claude-opus-5",
                r#"The model "claude-opus-5" does not exist."#,
                "claude-opus-5 is not a model this endpoint serves.",
            ] {
                let body = not_found(message);
                let mut ctx = context(kind);
                ctx.body_prefix = Some(&body);
                let c = cooldown_of(classify(&head(404, &[]), &ctx));
                assert_eq!(
                    (c.scope, c.until, c.reason, c.window.as_str()),
                    (
                        Scope::Model,
                        NOW + 3_600_000 + 1_000,
                        "model_rejected",
                        "model_rejection"
                    ),
                    "{message}"
                );
            }
        }
    }

    #[test]
    fn a_404_naming_another_model_or_nothing_commits() {
        for message in [
            "model: claude-haiku-4-5",
            "not found",
            "model: claude-opus-5-1",
        ] {
            let body = not_found(message);
            let mut ctx = context(AccountKind::Oauth);
            ctx.body_prefix = Some(&body);
            assert!(
                matches!(classify(&head(404, &[]), &ctx), Verdict::Commit),
                "{message}"
            );
        }
        let mut ctx = context(AccountKind::Oauth);
        ctx.wire_model = "claude-opus-5-1";
        let body = not_found("model: claude-opus-5");
        ctx.body_prefix = Some(&body);
        assert!(
            matches!(classify(&head(404, &[]), &ctx), Verdict::Commit),
            "a longer wire model is not named by a message naming only its prefix"
        );
    }

    #[test]
    fn a_400_and_a_422_naming_the_model_commit() {
        let mut ctx = context(AccountKind::Oauth);
        let body = refusal("model: claude-opus-5");
        ctx.body_prefix = Some(&body);
        assert!(matches!(classify(&head(400, &[]), &ctx), Verdict::Commit));
        ctx.body_prefix = Some(&body);
        assert!(matches!(classify(&head(422, &[]), &ctx), Verdict::Commit));
    }
}
