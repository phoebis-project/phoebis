//! The cooldown classification table, partitioned by account kind, applied to every upstream head before any
//! byte reaches the caller.

use std::cell::OnceCell;

use phoebis_routing::{
    FAST_CREDITS_REASON, FAST_RATE_LIMIT_REASON, MODEL_REJECTED_REASON, RATE_LIMIT_REASON,
    RETRY_AFTER_WINDOW, SEVEN_DAY_OVERAGE_INCLUDED,
};
use phoebis_store::{AccountKind, AccountState, StateCause};
use phoebis_transport::response::ResponseHead;
use serde_json::Value;

use crate::memory::Injected;
use crate::ratelimit::{retry_after, HeadReading, UnifiedHeaders, UnifiedWindow, WindowStatus};

/// Deadlines carry a second of margin so a request sent at the reset is not refused by a clock a hair behind.
const MARGIN_MS: i64 = 1_000;
const PERMISSION_MS: i64 = 3_600_000;
const FAST_RATE_LIMIT_FALLBACK_MS: i64 = 600_000;
const FAST_CREDITS_FALLBACK_MS: i64 = 3_600_000;

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
    /// The fast lane's rate limit.
    Fast,
    /// The fast lane's usage-credits refusal, a row apart from the rate limit's: each lifts on its own evidence.
    FastCredits,
    Model,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cooldown {
    pub scope: Scope,
    pub until: i64,
    pub reason: &'static str,
    pub window: String,
}

impl Cooldown {
    /// A row that holds until `deadline` and `MARGIN_MS` past it.
    pub fn new(
        scope: Scope,
        deadline: i64,
        reason: &'static str,
        window: impl Into<String>,
    ) -> Cooldown {
        Cooldown {
            scope,
            until: deadline.saturating_add(MARGIN_MS),
            reason,
            window: window.into(),
        }
    }

    /// The wait from `now` to the deadline the row was built on, its margin taken back off.
    pub fn wait_from(&self, now: i64) -> i64 {
        self.until.saturating_sub(now).saturating_sub(MARGIN_MS)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Commit,
    FailOver {
        cooldown: Option<Cooldown>,
        /// The state the verdict marks its account with and who moved it, the word the store keeps beside it.
        mark: Option<(AccountState, StateCause)>,
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
    /// The SSE event a held stream committed on names a `rate_limit_error`, as `is_rate_limit_event` reads it.
    pub rate_limit_event: bool,
    /// The start of a non-streaming body, decoded: the caller decodes its inspection copy, so this is never
    /// compressed.
    pub body_prefix: Option<&'a [u8]>,
    /// The attempt's own additions, body and head, that a 400 might name.
    pub injected: &'a [Injected],
    /// Whether this leg has not stripped yet. A strip retry is the router correcting its own edit on the same
    /// account, never a failover; the one-round rule is the only gate.
    pub can_strip: bool,
    /// The spelling the body carried upstream, which a rejection's message names.
    pub wire_model: &'a str,
    /// `routing.model_rejection_seconds` in milliseconds.
    pub model_rejection_ms: i64,
    /// The request carried `speed: "fast"`.
    pub fast: bool,
    /// The head's rate-limit families and date, parsed once by the caller for every reader of the head.
    pub reading: &'a HeadReading,
}

pub fn classify(head: &ResponseHead, ctx: &HeadContext<'_>) -> Verdict {
    let unified = ctx.reading.unified.as_ref();
    let sse_rate_limited = head.status == 200 && ctx.rate_limit_event;
    let rate_limited = head.status == 429 || sse_rate_limited;
    // Parsed by the first predicate that reads it, so a head none of them reaches parses nothing.
    let parsed = OnceCell::new();
    let error = || {
        ctx.body_prefix
            .map(|body| parsed.get_or_init(|| ErrorBody::read(body)))
    };
    // Ahead of the per-kind table below, for either account kind: a 400 naming an item this leg added is a fact
    // about the request shape, not about the credential, and a name outside `injected` is the client's own to
    // handle.
    if ctx.can_strip {
        let named = injected_named(head.status, ctx.injected, error);
        if !named.is_empty() {
            return Verdict::StripInjected(named);
        }
    }
    // Ahead of the per-kind table, for either kind: a 404 naming the model the body carried is the upstream's word
    // that it does not serve that model, a fact about the (account, model) pair alone. A 404 naming nothing, and
    // every 400 and 422, are the client's own to read and fall through to `Commit`.
    if head.status == 404
        && error().is_some_and(|e| {
            e.error_type.as_deref() == Some("not_found_error") && names_model(e, ctx.wire_model)
        })
    {
        return Verdict::FailOver {
            cooldown: Some(Cooldown::new(
                Scope::Model,
                ctx.now.saturating_add(ctx.model_rejection_ms),
                MODEL_REJECTED_REASON,
                MODEL_REJECTION_WINDOW,
            )),
            mark: None,
        };
    }
    // A fast request draws usage credits and no plan window: a credits refusal cools its fast lane whatever the plan
    // statuses read, and so does a rate limit, whatever the account kind, unless the 5-hour or 7-day window or an
    // api_key spend cap rejected it, which are the account's and keep their own rows.
    if ctx.fast && rate_limited {
        let credits_refused = unified.is_some_and(|u| u.overage_disabled_reason.is_some());
        let plan_rejected = unified.is_some_and(plan_window_rejected)
            || (ctx.kind == AccountKind::ApiKey && error().is_some_and(names_spend_limit));
        if credits_refused || !plan_rejected {
            return Verdict::FailOver {
                cooldown: Some(fast_cooldown(head, unified, ctx)),
                mark: None,
            };
        }
    }
    match ctx.kind {
        AccountKind::Oauth if rate_limited => {
            return Verdict::FailOver {
                cooldown: Some(oauth_cooldown(head, unified, ctx)),
                mark: None,
            }
        }
        AccountKind::ApiKey if rate_limited && error().is_some_and(names_spend_limit) => {
            return Verdict::FailOver {
                cooldown: Some(Cooldown::new(
                    Scope::Account,
                    next_month_start(ctx.now),
                    SPEND_CAP_REASON,
                    "overage",
                )),
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
                && error()
                    .is_some_and(|e| e.error_type.as_deref() == Some("invalid_request_error"))
                && ctx.body_prefix.is_some_and(names_credit_balance) =>
        {
            return Verdict::FailOver {
                cooldown: None,
                mark: Some((AccountState::FundingRequired, StateCause::FundingRefused)),
            };
        }
        _ => {}
    }
    let auth = head.status == 401
        || (head.status == 403
            && error().is_some_and(|e| e.error_type.as_deref() == Some("authentication_error")));
    if auth {
        return match ctx.kind {
            AccountKind::Oauth => Verdict::Refresh,
            AccountKind::ApiKey => Verdict::FailOver {
                cooldown: None,
                mark: Some((AccountState::ReauthRequired, StateCause::AuthFailed)),
            },
        };
    }
    if head.status == 403
        && error().is_some_and(|e| e.error_type.as_deref() == Some("permission_error"))
    {
        return Verdict::FailOver {
            cooldown: Some(Cooldown::new(
                Scope::Model,
                ctx.now.saturating_add(PERMISSION_MS),
                "permission",
                RETRY_AFTER_WINDOW,
            )),
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
    let row = |scope: Scope, reset: Option<i64>| {
        let deadline = reset
            .or_else(|| {
                retry_after(head, ctx.reading.date, ctx.now).map(|d| ctx.now.saturating_add(d))
            })
            .unwrap_or(ctx.now.saturating_add(ctx.fallback_ms));
        Cooldown::new(
            scope,
            deadline,
            RATE_LIMIT_REASON,
            exhaustion_window(unified),
        )
    };
    if let Some(u) = unified {
        if shared_window_rejected(u, ctx.canonical_model) {
            let reset = [
                u.five_hour
                    .reset
                    .filter(|_| u.five_hour.status == Some(WindowStatus::Rejected)),
                u.seven_day
                    .reset
                    .filter(|_| u.seven_day.status == Some(WindowStatus::Rejected)),
                u.reset,
            ]
            .into_iter()
            .flatten()
            .max();
            return row(Scope::Account, reset);
        }
        if model_only(u, ctx.canonical_model)
            && (u.seven_day_oi.status == Some(WindowStatus::Rejected)
                || u.status == Some(WindowStatus::Rejected))
        {
            // Same deadline rule as the shared row: the later of the rejected window's own reset and the
            // top-level reset, which only ever lengthens the cooldown when it wins.
            let reset = [u.seven_day_oi.reset, u.reset].into_iter().flatten().max();
            // The row keeps the upstream's claim: it reaches the client as an exhausted pool's claim, and the
            // client resumes only on a claim it knows. Recovery reads the model's own week, not this text.
            return row(Scope::Model, reset);
        }
    }
    row(Scope::Account, None)
}

/// A shared plan window rejected: the 5-hour or the 7-day status, or the top-level status unless the requested
/// model's own week is the only one that rejected.
fn shared_window_rejected(u: &UnifiedHeaders, canonical_model: &str) -> bool {
    plan_window_rejected(u)
        || (u.status == Some(WindowStatus::Rejected) && !model_only(u, canonical_model))
}

/// The 5-hour or the 7-day status rejected. The top-level status is left out for a fast request: it draws neither
/// window, so a rejection neither window names is not theirs.
fn plan_window_rejected(u: &UnifiedHeaders) -> bool {
    u.five_hour.status == Some(WindowStatus::Rejected)
        || u.seven_day.status == Some(WindowStatus::Rejected)
}

fn fast_cooldown(
    head: &ResponseHead,
    unified: Option<&UnifiedHeaders>,
    ctx: &HeadContext<'_>,
) -> Cooldown {
    if let Some(reason) = unified.and_then(|u| u.overage_disabled_reason.as_deref()) {
        // The spend cap lifts with the month; any other refusal holds until the reset the head names, else an
        // hour, and the poll's next enabled reading lifts it earlier.
        let until = if reason == "org_spend_cap_reached" {
            next_month_start(ctx.now)
        } else {
            unified
                .and_then(|u| u.overage_reset)
                .unwrap_or(ctx.now.saturating_add(FAST_CREDITS_FALLBACK_MS))
        };
        return Cooldown::new(Scope::FastCredits, until, FAST_CREDITS_REASON, reason);
    }
    // The per-minute bucket's reset decides the normal case; a 429 with neither reset is more likely an
    // organization without fast access, and ten minutes bounds the dials wasted on it.
    let fast_reset = ctx
        .reading
        .fast
        .iter()
        .flat_map(|f| {
            [
                f.input.and_then(|b| b.reset),
                f.output.and_then(|b| b.reset),
            ]
        })
        .flatten()
        .max();
    let (until, window) = match (fast_reset, retry_after(head, ctx.reading.date, ctx.now)) {
        (Some(reset), _) => (reset, "fast"),
        (None, Some(delay)) => (ctx.now.saturating_add(delay), RETRY_AFTER_WINDOW),
        (None, None) => (ctx.now.saturating_add(FAST_RATE_LIMIT_FALLBACK_MS), "fast"),
    };
    Cooldown::new(Scope::Fast, until, FAST_RATE_LIMIT_REASON, window)
}

/// A shared window is healthy when its status reads `allowed` or `allowed_warning` (the near-cap warning is
/// still an allowance), or when the status is absent and the window's own utilization parses below 1.0: a
/// window the upstream reported under capacity is not the one that rejected. An absent status with no
/// utilization, or a third status value, is evidence of nothing, and a model-only cooldown on that basis would
/// keep sending an exhausted account traffic.
fn shared_healthy(window: &UnifiedWindow) -> bool {
    match window.status {
        Some(WindowStatus::Allowed | WindowStatus::AllowedWarning) => true,
        Some(_) => false,
        None => window.utilization.is_some_and(|u| (0.0..1.0).contains(&u)),
    }
}

/// Whether the rejection names the request model's own week and nothing shared: both shared windows healthy, the
/// model's family has a row in the window table, and the representative claim names that row's window, or names
/// the overage week or no week while the 7d_oi status is rejected. A claim naming another family's week, or one no
/// row knows, is read before the status fallback and is no model evidence: the account cools, the safe direction
/// while the claim vocabulary is not yet measured live, and a foreign week never borrows the fallback.
fn model_only(u: &UnifiedHeaders, canonical_model: &str) -> bool {
    let Some(own) = phoebis_routing::model_week(canonical_model) else {
        return false;
    };
    let shared_fine = shared_healthy(&u.five_hour) && shared_healthy(&u.seven_day);
    if !shared_fine {
        return false;
    }
    match u.representative_claim.as_deref() {
        Some(claim) if claim == own => true,
        // Another family's week, or a week no row names: the account-wide cooldown, whatever `7d_oi` reads.
        Some(claim) if claim.starts_with("seven_day_") && claim != SEVEN_DAY_OVERAGE_INCLUDED => {
            false
        }
        _ => u.seven_day_oi.status == Some(WindowStatus::Rejected),
    }
}

fn api_key_cooldown(head: &ResponseHead, ctx: &HeadContext<'_>) -> Cooldown {
    let (scope, delay) = match retry_after(head, ctx.reading.date, ctx.now) {
        Some(delay) => (Scope::Model, delay),
        None => (Scope::Account, ctx.fallback_ms),
    };
    Cooldown::new(
        scope,
        ctx.now.saturating_add(delay),
        RATE_LIMIT_REASON,
        RETRY_AFTER_WINDOW,
    )
}

pub fn exhaustion_window(unified: Option<&UnifiedHeaders>) -> String {
    let Some(u) = unified else {
        return RETRY_AFTER_WINDOW.into();
    };
    if let Some(claim) = &u.representative_claim {
        return claim.clone();
    }
    if u.five_hour.status == Some(WindowStatus::Rejected) {
        return phoebis_routing::FIVE_HOUR.into();
    }
    if u.seven_day.status == Some(WindowStatus::Rejected) {
        return phoebis_routing::SEVEN_DAY.into();
    }
    RETRY_AFTER_WINDOW.into()
}

/// The value of a `"key": "value"` member, scanned textually in a body a JSON parse could not complete: the
/// search starts after the first `"error"` key and trusts only a `"key"` that opens an object or follows a
/// sibling field, never one embedded in a string value. The value is the raw text up to its closing quote, escapes
/// left as written, and runs to the end of the text when the body was cut inside it.
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
            return Some(&after[..closing_quote(after)]);
        }
        cursor = match_at + needle.len();
    }
    None
}

/// Where a JSON string's raw text ends: at the first quote no backslash escapes, or at the end of the text. A
/// message that quotes a model name escapes its quotes, and the value goes on past them.
fn closing_quote(raw: &str) -> usize {
    let mut escaped = false;
    for (at, byte) in raw.bytes().enumerate() {
        match byte {
            b'\\' => escaped = !escaped,
            b'"' if !escaped => return at,
            _ => escaped = false,
        }
    }
    raw.len()
}

/// The error body's member at each `(pointer, key)`, off one parse. A complete body parses directly, which is what
/// keeps a `"type"` string that happens to appear inside an upstream `message` from being mistaken for the real
/// member. A prefix cut mid-document fails to parse, so each member is then located textually by its `key` through
/// `textual_member`.
fn error_members<const N: usize>(body: &[u8], members: [(&str, &str); N]) -> [Option<String>; N] {
    match serde_json::from_slice::<Value>(body) {
        Ok(value) => members.map(|(pointer, _)| {
            value
                .pointer(pointer)
                .and_then(Value::as_str)
                .map(str::to_string)
        }),
        Err(_) => {
            let text = valid_text(body);
            members.map(|(_, key)| textual_member(text, key).map(str::to_string))
        }
    }
}

fn error_member(body: &[u8], pointer: &str, key: &str) -> Option<String> {
    let [member] = error_members(body, [(pointer, key)]);
    member
}

/// The error body's members the predicates read, off one parse.
struct ErrorBody {
    error_type: Option<String>,
    message: Option<String>,
    error_code: Option<String>,
}

impl ErrorBody {
    fn read(body: &[u8]) -> ErrorBody {
        let [error_type, message, error_code] = error_members(
            body,
            [
                ("/error/type", "type"),
                ("/error/message", "message"),
                ("/error/details/error_code", "error_code"),
            ],
        );
        ErrorBody {
            error_type,
            message,
            error_code,
        }
    }
}

/// The error body's `error.type`.
pub(crate) fn error_type(body: &[u8]) -> Option<String> {
    error_member(body, "/error/type", "type")
}

/// Whether an error event's data names a `rate_limit_error`, read off its parsed `error.type`, so a type spelled with
/// an escape counts and the word inside a message does not. The verdict and the attempt's recorded outcome both read
/// this one rule.
pub(crate) fn is_rate_limit_event(data: &[u8]) -> bool {
    error_type(data).as_deref() == Some("rate_limit_error")
}

/// `body` up to its first byte that is not UTF-8: a prefix cut at the buffer bound can end inside a multi-byte
/// character, and the text before it still reads.
fn valid_text(body: &[u8]) -> &str {
    match std::str::from_utf8(body) {
        Ok(text) => text,
        Err(error) => std::str::from_utf8(&body[..error.valid_up_to()])
            .expect("the bytes before `valid_up_to` are UTF-8"),
    }
}

fn names_credit_balance(body: &[u8]) -> bool {
    valid_text(body)
        .to_ascii_lowercase()
        .contains("credit balance")
}

/// The body's `error.message`, read as far as the cut when the body was read only up to the buffer bound.
pub(crate) fn error_message(body: &[u8]) -> Option<String> {
    error_member(body, "/error/message", "message")
}

/// Whether the body names the upstream's own monthly spend-cap code in `error.details.error_code`.
fn names_spend_limit(error: &ErrorBody) -> bool {
    error.error_code.as_deref() == Some("enforced_spend_limit_reached")
}

/// Whether `error.message` names `wire_model` as a whole token: the byte before and after each occurrence are
/// absent (the message's ends) or a boundary, where a boundary is any byte that is neither ASCII alphanumeric
/// nor `-` nor `.` (quotes, whitespace, `:`, `,`, and the rest of punctuation all bound). A cut message's end
/// counts as its end, so a cut landing just past a shorter model name that prefixes the real one reads the shorter
/// name. An empty `wire_model` never matches, since every message contains the empty string.
fn names_model(error: &ErrorBody, wire_model: &str) -> bool {
    if wire_model.is_empty() {
        return false;
    }
    let Some(message) = error.message.as_deref().map(str::to_ascii_lowercase) else {
        return false;
    };
    let wire_model = wire_model.to_ascii_lowercase();
    names_token(&message, &wire_model, |c| {
        c.is_ascii_alphanumeric() || c == '-' || c == '.'
    })
}

/// `injected_named` over the body bytes the latch holds, parsed only past the gate.
pub(crate) fn refused_injected(
    status: u16,
    body: Option<&[u8]>,
    injected: &[Injected],
) -> Vec<Injected> {
    let parsed = OnceCell::new();
    injected_named(status, injected, || {
        body.map(|body| parsed.get_or_init(|| ErrorBody::read(body)))
    })
}

/// The items of `injected` an upstream 400 `invalid_request_error` names, the one gate the strip and the latch
/// both read: a 400 of another kind that happens to spell an injected name in its message never strips or
/// latches on the strength of a coincidence. `error` is asked only past the status and the empty list.
fn injected_named<'e>(
    status: u16,
    injected: &[Injected],
    error: impl FnOnce() -> Option<&'e ErrorBody>,
) -> Vec<Injected> {
    if status != 400 || injected.is_empty() {
        return Vec::new();
    }
    match error().filter(|e| e.error_type.as_deref() == Some("invalid_request_error")) {
        Some(error) => refused_names(error, injected),
        None => Vec::new(),
    }
}

/// The names an upstream 400 refused, read off `error.message` alone (never the envelope, where a field such as
/// `request_id` can spell any of these names by accident), each as a whole token: bounded on both sides by a
/// character outside `[A-Za-z0-9_-]` or the string's edge.
fn refused_names(error: &ErrorBody, candidates: &[Injected]) -> Vec<Injected> {
    let Some(message) = error.message.as_deref() else {
        return Vec::new();
    };
    candidates
        .iter()
        .copied()
        .filter(|item| {
            names_token(message, item.wire_name(), |c| {
                c.is_ascii_alphanumeric() || c == '_' || c == '-'
            })
        })
        .collect()
}

/// Whether some occurrence of `token` in `text` has, on each side, the text's edge or a character outside
/// `in_token`.
fn names_token(text: &str, token: &str, in_token: impl Fn(char) -> bool) -> bool {
    let bound = |c: Option<char>| c.is_none_or(|c| !in_token(c));
    text.match_indices(token).any(|(at, _)| {
        bound(text[..at].chars().next_back()) && bound(text[at + token.len()..].chars().next())
    })
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
            rate_limit_event: false,
            body_prefix: None,
            injected: &[],
            can_strip: false,
            wire_model: "claude-opus-5",
            model_rejection_ms: 3_600_000,
            fast: false,
            reading: &NO_READING,
        }
    }

    static NO_READING: HeadReading = HeadReading {
        unified: None,
        fast: None,
        date: None,
    };

    /// `classify` over the reading parsed off the head the case builds, the one parse the relay makes of each head.
    fn classify(head: &ResponseHead, ctx: &HeadContext<'_>) -> Verdict {
        let reading = HeadReading::parse(head);
        super::classify(
            head,
            &HeadContext {
                reading: &reading,
                ..*ctx
            },
        )
    }

    fn fast_ctx<'a>(kind: AccountKind) -> HeadContext<'a> {
        HeadContext {
            fast: true,
            ..context(kind)
        }
    }

    /// A subscription request for a model whose family has a week of its own.
    fn fable_ctx<'a>() -> HeadContext<'a> {
        HeadContext {
            canonical_model: "claude-fable-5-1",
            ..context(AccountKind::Oauth)
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
        // the textual scan is what `ErrorBody::read` falls back to.
        let cut_after_value = &full[..code_end + 1];
        assert!(serde_json::from_slice::<Value>(cut_after_value).is_err());
        assert!(names_spend_limit(&ErrorBody::read(cut_after_value)));

        // Cut mid-value: the code's own text is incomplete, so it must not match.
        let cut_mid_value = &full[..code_end - 5];
        assert!(serde_json::from_slice::<Value>(cut_mid_value).is_err());
        assert!(!names_spend_limit(&ErrorBody::read(cut_mid_value)));

        // Cut before the key ever appears: absent entirely.
        let key_at = text.find(r#""error_code""#).unwrap();
        let cut_before_key = &full[..key_at];
        assert!(serde_json::from_slice::<Value>(cut_before_key).is_err());
        assert!(!names_spend_limit(&ErrorBody::read(cut_before_key)));
    }

    /// A rejection read up to the buffer bound can end inside a multi-byte character; what came before it still
    /// names the error.
    #[test]
    fn a_prefix_cut_inside_a_multi_byte_character_still_names_its_error() {
        let auth = "{\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",\"message\":\"\u{e9}t\u{e9}";
        let cut = &auth.as_bytes()[..auth.len() - 1];
        assert!(std::str::from_utf8(cut).is_err());
        assert_eq!(error_type(cut).as_deref(), Some("authentication_error"));
        let full = spend_cap_body();
        let mut cut = full[..full.len() - 2].to_vec();
        cut.extend_from_slice(&"\u{e9}".as_bytes()[..1]);
        assert!(std::str::from_utf8(&cut).is_err());
        assert!(names_spend_limit(&ErrorBody::read(&cut)));
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
    fn a_spend_cap_rejection_of_the_overage_window_cools_only_the_model() {
        // The header set an organization at its spend cap answers, as observed: no 5h status, a 5h utilization
        // of zero, no reset header, `Retry-After` alone.
        let h = head(
            429,
            &[
                ("anthropic-ratelimit-unified-status", "rejected"),
                (
                    "anthropic-ratelimit-unified-representative-claim",
                    "seven_day_overage_included",
                ),
                ("anthropic-ratelimit-unified-7d-status", "allowed"),
                ("anthropic-ratelimit-unified-7d-utilization", "0.69"),
                ("anthropic-ratelimit-unified-5h-utilization", "0.00"),
                ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
                ("anthropic-ratelimit-unified-7d_oi-utilization", "1.02"),
                ("anthropic-ratelimit-unified-overage-status", "rejected"),
                (
                    "anthropic-ratelimit-unified-overage-disabled-reason",
                    "org_spend_cap_reached",
                ),
                ("retry-after", "121180"),
            ],
        );
        let c = cooldown_of(classify(&h, &fable_ctx()));
        assert_eq!(
            (c.scope, c.until, c.window.as_str()),
            (
                Scope::Model,
                NOW + 121_180_000 + MARGIN_MS,
                "seven_day_overage_included"
            ),
            "the overage window rejected with both shared windows healthy cools the model, not the account"
        );
    }

    #[test]
    fn a_spend_cap_rejection_cools_only_the_model_with_an_absent_seven_day_status() {
        // The mirror of `a_spend_cap_rejection_of_the_overage_window_cools_only_the_model`: the 5h status is
        // explicit this time and the 7d status is absent, so the 7d window takes the predicate's own-utilization
        // path instead of the 5h window.
        let h = head(
            429,
            &[
                ("anthropic-ratelimit-unified-status", "rejected"),
                (
                    "anthropic-ratelimit-unified-representative-claim",
                    "seven_day_overage_included",
                ),
                ("anthropic-ratelimit-unified-5h-status", "allowed"),
                ("anthropic-ratelimit-unified-7d-utilization", "0.69"),
                ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
                ("anthropic-ratelimit-unified-7d_oi-utilization", "1.02"),
                ("anthropic-ratelimit-unified-overage-status", "rejected"),
                (
                    "anthropic-ratelimit-unified-overage-disabled-reason",
                    "org_spend_cap_reached",
                ),
                ("retry-after", "121180"),
            ],
        );
        let c = cooldown_of(classify(&h, &fable_ctx()));
        assert_eq!(
            (c.scope, c.until, c.window.as_str()),
            (
                Scope::Model,
                NOW + 121_180_000 + MARGIN_MS,
                "seven_day_overage_included"
            ),
            "an absent 7d status with a utilization below 1.0 is healthy, same as the 5h side"
        );
    }

    #[test]
    fn a_model_only_rejection_under_a_five_hour_warning_cools_only_the_model() {
        let h = head(
            429,
            &[
                ("anthropic-ratelimit-unified-status", "rejected"),
                (
                    "anthropic-ratelimit-unified-representative-claim",
                    "seven_day_fable",
                ),
                ("anthropic-ratelimit-unified-5h-status", "allowed_warning"),
                ("anthropic-ratelimit-unified-7d-status", "allowed"),
                ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
                ("anthropic-ratelimit-unified-7d_oi-reset", "1700007200"),
            ],
        );
        let c = cooldown_of(classify(&h, &fable_ctx()));
        assert_eq!(
            (c.scope, c.until, c.window.as_str()),
            (
                Scope::Model,
                1_700_007_200_000 + MARGIN_MS,
                "seven_day_fable"
            )
        );
    }

    /// A rejection whose shared windows both read healthy and whose `7d_oi` status is rejected, under `claim`:
    /// the status alone would answer a model-scoped row, so the claim decides.
    fn model_week_rejection(claim: Option<&str>) -> ResponseHead {
        let mut headers = vec![
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-5h-status", "allowed"),
            ("anthropic-ratelimit-unified-7d-status", "allowed"),
            ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
            ("anthropic-ratelimit-unified-7d_oi-reset", "1700007200"),
        ];
        if let Some(claim) = claim {
            headers.push(("anthropic-ratelimit-unified-representative-claim", claim));
        }
        head(429, &headers)
    }

    #[test]
    fn a_model_week_rejection_cools_the_account_for_a_model_without_a_week_of_its_own() {
        let sonnet = HeadContext {
            canonical_model: "claude-sonnet-5",
            ..context(AccountKind::Oauth)
        };
        for claim in [
            Some("seven_day_fable"),
            Some("seven_day_overage_included"),
            None,
        ] {
            let c = cooldown_of(classify(&model_week_rejection(claim), &sonnet));
            assert_eq!(
                c.scope,
                Scope::Account,
                "a model the family table has no row for has no week to cool alone: claim {claim:?}"
            );
        }
    }

    #[test]
    fn a_claim_naming_another_familys_week_or_an_unknown_one_cools_the_account() {
        for claim in ["seven_day_opus", "seven_day_unknown"] {
            let c = cooldown_of(classify(&model_week_rejection(Some(claim)), &fable_ctx()));
            assert_eq!(
                (c.scope, c.window.as_str()),
                (Scope::Account, claim),
                "the claim names no week of the requested model's family, whatever the 7d_oi status reads"
            );
        }
        let c = cooldown_of(classify(&model_week_rejection(None), &fable_ctx()));
        assert_eq!(
            (c.scope, c.window.as_str()),
            (Scope::Model, "retry_after"),
            "with no claim the 7d_oi status cools the requested model, and the row names no claim"
        );
    }

    #[test]
    fn an_absent_shared_status_whose_utilization_reads_full_cools_the_account() {
        let h = head(
            429,
            &[
                ("anthropic-ratelimit-unified-status", "rejected"),
                ("anthropic-ratelimit-unified-5h-utilization", "1.0"),
                ("anthropic-ratelimit-unified-7d-status", "allowed"),
                // The healthy week reads a utilization of its own, which never speaks for the 5-hour window.
                ("anthropic-ratelimit-unified-7d-utilization", "0.10"),
                ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
            ],
        );
        assert_eq!(
            cooldown_of(classify(&h, &fable_ctx())).scope,
            Scope::Account,
            "a utilization at 1.0 is a spent window, whatever the missing status would have said"
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
        let c = cooldown_of(classify(&h, &fable_ctx()));
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
            cooldown_of(classify(&h, &fable_ctx())).scope,
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
        let c = cooldown_of(classify(&h, &fable_ctx()));
        assert_eq!(
            c.scope,
            Scope::Account,
            "a shared status other than allowed, allowed_warning or rejected is not evidence of a healthy window"
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
        ctx.rate_limit_event = is_rate_limit_event(
            br#"{"type":"error","error":{"type":"rate_limit_error","message":"slow"}}"#,
        );
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
        ctx.rate_limit_event = is_rate_limit_event(
            br#"{"type":"error","error":{"type":"overloaded_error","message":"x"}}"#,
        );
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
                mark: Some((AccountState::FundingRequired, StateCause::FundingRefused))
            }
        ));
    }

    #[test]
    fn the_api_key_200_sse_first_event_error_is_model_scoped_with_retry_after() {
        let mut ctx = context(AccountKind::ApiKey);
        ctx.rate_limit_event = is_rate_limit_event(
            br#"{"type":"error","error":{"type":"rate_limit_error","message":"slow"}}"#,
        );
        let h = head(
            200,
            &[("content-type", "text/event-stream"), ("retry-after", "9")],
        );
        let c = cooldown_of(classify(&h, &ctx));
        assert_eq!((c.scope, c.until), (Scope::Model, NOW + 9_000 + 1_000));
    }

    /// The six `anthropic-fast-*` headers of a fast answer, each family's reset at the epoch second given.
    fn fast_headers(
        input_reset: &'static str,
        output_reset: &'static str,
    ) -> Vec<(&'static str, &'static str)> {
        vec![
            ("anthropic-fast-input-tokens-limit", "400000"),
            ("anthropic-fast-input-tokens-remaining", "0"),
            ("anthropic-fast-input-tokens-reset", input_reset),
            ("anthropic-fast-output-tokens-limit", "80000"),
            ("anthropic-fast-output-tokens-remaining", "0"),
            ("anthropic-fast-output-tokens-reset", output_reset),
        ]
    }

    #[test]
    fn a_fast_429_with_the_overage_disabled_reason_cools_the_fast_lane_until_the_month_start_the_reset_or_an_hour(
    ) {
        let mut ctx = fast_ctx(AccountKind::Oauth);
        ctx.now = ms(2026, 9, 25, 12, 34);
        let reason = "anthropic-ratelimit-unified-overage-disabled-reason";
        let c = cooldown_of(classify(
            &head(429, &[(reason, "org_spend_cap_reached")]),
            &ctx,
        ));
        assert_eq!(
            (c.scope, c.reason, c.window.as_str(), c.until),
            (
                Scope::FastCredits,
                FAST_CREDITS_REASON,
                "org_spend_cap_reached",
                ms(2026, 10, 1, 0, 0) + MARGIN_MS
            ),
            "the spend cap lifts with the month"
        );
        let c = cooldown_of(classify(
            &head(
                429,
                &[
                    (reason, "out_of_credits"),
                    ("anthropic-ratelimit-unified-overage-reset", "1790000000"),
                    ("retry-after", "30"),
                ],
            ),
            &ctx,
        ));
        assert_eq!(
            (c.scope, c.reason, c.window.as_str(), c.until),
            (
                Scope::FastCredits,
                FAST_CREDITS_REASON,
                "out_of_credits",
                1_790_000_000_000 + MARGIN_MS
            ),
            "another reason holds until the overage reset the head names"
        );
        let c = cooldown_of(classify(&head(429, &[(reason, "out_of_credits")]), &ctx));
        assert_eq!(
            (c.scope, c.reason, c.window.as_str(), c.until),
            (
                Scope::FastCredits,
                FAST_CREDITS_REASON,
                "out_of_credits",
                ctx.now + 3_600_000 + MARGIN_MS
            ),
            "without a reset, an hour"
        );
        for rejected in [
            vec![("anthropic-ratelimit-unified-status", "rejected")],
            vec![
                ("anthropic-ratelimit-unified-status", "rejected"),
                ("anthropic-ratelimit-unified-5h-status", "rejected"),
                ("anthropic-ratelimit-unified-5h-reset", "1790000000"),
            ],
        ] {
            let mut headers = rejected.clone();
            headers.push((reason, "out_of_credits"));
            let c = cooldown_of(classify(&head(429, &headers), &ctx));
            assert_eq!(
                (c.scope, c.reason, c.window.as_str()),
                (Scope::FastCredits, FAST_CREDITS_REASON, "out_of_credits"),
                "the reason decides whatever the plan statuses read: {rejected:?}"
            );
        }
    }

    #[test]
    fn a_fast_429_without_the_reason_cools_the_fast_lane_by_the_later_fast_reset_then_retry_after_then_ten_minutes(
    ) {
        let ctx = fast_ctx(AccountKind::Oauth);
        // 1700000090 is 2023-11-14T22:14:50Z: the later of the two resets, read from RFC 3339.
        let mut both = fast_headers("1700000030", "2023-11-14T22:14:50Z");
        both.push(("retry-after", "5"));
        let c = cooldown_of(classify(&head(429, &both), &ctx));
        assert_eq!(
            (c.scope, c.reason, c.window.as_str(), c.until),
            (
                Scope::Fast,
                FAST_RATE_LIMIT_REASON,
                "fast",
                NOW + 90_000 + MARGIN_MS
            )
        );
        let c = cooldown_of(classify(&head(429, &[("retry-after", "20")]), &ctx));
        assert_eq!(
            (c.scope, c.reason, c.window.as_str(), c.until),
            (
                Scope::Fast,
                FAST_RATE_LIMIT_REASON,
                "retry_after",
                NOW + 20_000 + MARGIN_MS
            )
        );
        let c = cooldown_of(classify(&head(429, &[]), &ctx));
        assert_eq!(
            (c.scope, c.reason, c.window.as_str(), c.until),
            (
                Scope::Fast,
                FAST_RATE_LIMIT_REASON,
                "fast",
                NOW + 600_000 + MARGIN_MS
            )
        );
        let c = cooldown_of(classify(
            &head(
                429,
                &[
                    ("anthropic-ratelimit-unified-status", "rejected"),
                    ("retry-after", "20"),
                ],
            ),
            &ctx,
        ));
        assert_eq!(
            (c.scope, c.reason, c.window.as_str()),
            (Scope::Fast, FAST_RATE_LIMIT_REASON, "retry_after"),
            "a top-level rejection that names neither shared window is the fast lane's"
        );
    }

    #[test]
    fn a_fast_429_that_rejects_a_shared_window_cools_the_account_as_a_standard_one_does() {
        for shared in [
            ("anthropic-ratelimit-unified-5h-status", "rejected"),
            ("anthropic-ratelimit-unified-7d-status", "rejected"),
        ] {
            let h = head(
                429,
                &[
                    shared,
                    ("anthropic-ratelimit-unified-status", "rejected"),
                    ("anthropic-ratelimit-unified-reset", "1700003600"),
                    ("retry-after", "5"),
                ],
            );
            let fast = cooldown_of(classify(&h, &fast_ctx(AccountKind::Oauth)));
            let standard = cooldown_of(classify(&h, &context(AccountKind::Oauth)));
            assert_eq!(fast, standard, "{shared:?}");
            assert_eq!(
                (fast.scope, fast.reason, fast.until),
                (Scope::Account, "rate_limit", 1_700_003_600_000 + MARGIN_MS),
                "{shared:?}"
            );
        }
    }

    #[test]
    fn a_fast_429_on_an_api_key_account_cools_the_fast_lane_and_its_spend_cap_still_cools_the_account(
    ) {
        let mut ctx = fast_ctx(AccountKind::ApiKey);
        let c = cooldown_of(classify(&head(429, &[("retry-after", "12")]), &ctx));
        assert_eq!(
            (c.scope, c.reason, c.window.as_str(), c.until),
            (
                Scope::Fast,
                FAST_RATE_LIMIT_REASON,
                "retry_after",
                NOW + 12_000 + MARGIN_MS
            )
        );
        let body = spend_cap_body();
        ctx.body_prefix = Some(&body);
        ctx.now = ms(2026, 9, 25, 12, 34);
        let c = cooldown_of(classify(&head(429, &[]), &ctx));
        assert_eq!(
            (c.scope, c.reason, c.window.as_str(), c.until),
            (
                Scope::Account,
                SPEND_CAP_REASON,
                "overage",
                ms(2026, 10, 1, 0, 0) + MARGIN_MS
            )
        );
    }

    #[test]
    fn a_standard_429_reads_as_before_whatever_the_fast_headers_say() {
        let mut headers = fast_headers("1700000030", "1700000090");
        headers.push((
            "anthropic-ratelimit-unified-overage-disabled-reason",
            "out_of_credits",
        ));
        headers.push(("retry-after", "30"));
        let h = head(429, &headers);
        let c = cooldown_of(classify(&h, &context(AccountKind::Oauth)));
        assert_eq!(
            (c.scope, c.reason, c.window.as_str(), c.until),
            (
                Scope::Account,
                "rate_limit",
                "retry_after",
                NOW + 30_000 + MARGIN_MS
            )
        );
        let c = cooldown_of(classify(&h, &context(AccountKind::ApiKey)));
        assert_eq!(
            (c.scope, c.reason, c.until),
            (Scope::Model, "rate_limit", NOW + 30_000 + MARGIN_MS)
        );
    }

    #[test]
    fn a_fast_requests_first_sse_rate_limit_error_cools_the_fast_lane() {
        let mut ctx = fast_ctx(AccountKind::Oauth);
        ctx.rate_limit_event = is_rate_limit_event(
            br#"{"type":"error","error":{"type":"rate_limit_error","message":"slow"}}"#,
        );
        let h = head(
            200,
            &[("content-type", "text/event-stream"), ("retry-after", "9")],
        );
        let c = cooldown_of(classify(&h, &ctx));
        assert_eq!(
            (c.scope, c.reason, c.window.as_str(), c.until),
            (
                Scope::Fast,
                FAST_RATE_LIMIT_REASON,
                "retry_after",
                NOW + 9_000 + MARGIN_MS
            )
        );
        ctx.rate_limit_event = is_rate_limit_event(
            br#"{"type":"error","error":{"type":"overloaded_error","message":"x"}}"#,
        );
        assert!(matches!(classify(&h, &ctx), Verdict::Commit));
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
                mark: Some((AccountState::ReauthRequired, StateCause::AuthFailed))
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
        assert_eq!(exhaustion_window(Some(&claim)), "seven_day_opus");
        let seven = UnifiedHeaders::parse(&head(
            429,
            &[("anthropic-ratelimit-unified-7d-status", "rejected")],
        ))
        .unwrap();
        assert_eq!(exhaustion_window(Some(&seven)), "seven_day");
        assert_eq!(exhaustion_window(None), "retry_after");
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

    /// A rejection read up to the buffer bound does not parse as a whole; its `error.message` still reads as far as
    /// the cut, a quote the message escapes included, so the model, the injected items and a credit balance it names
    /// are found.
    #[test]
    fn a_prefix_cut_mid_message_still_names_its_model_its_injected_items_and_a_credit_balance() {
        let cut = |body: Vec<u8>| body[..body.len() - 8].to_vec();
        let model = cut(not_found(
            r#"The model "claude-opus-5" does not exist, and so on"#,
        ));
        assert!(serde_json::from_slice::<Value>(&model).is_err());
        assert!(names_model(&ErrorBody::read(&model), "claude-opus-5"));
        let refused = cut(refusal(
            "Unexpected value(s) `cache-diagnosis-2026-04-07` for the header, and so on",
        ));
        assert_eq!(
            refused_names(
                &ErrorBody::read(&refused),
                &[Injected::CacheDiagnosisBeta, Injected::Diagnostics]
            ),
            vec![Injected::CacheDiagnosisBeta]
        );
        let mut credit = cut(refusal("Your credit balance is too low, and so on"));
        credit.extend_from_slice(&"\u{e9}".as_bytes()[..1]);
        assert!(names_credit_balance(&credit));
    }

    #[test]
    fn refused_names_matches_whole_tokens_and_ignores_the_envelope() {
        let envelope = br#"{"type":"error","error":{"type":"invalid_request_error","message":"model not found"},"request_id":"req_cch_0001"}"#;
        assert_eq!(
            refused_names(&ErrorBody::read(envelope), &[Injected::Cch]),
            Vec::new()
        );
        let unrelated_field =
            br#"{"type":"error","error":{"type":"invalid_request_error","message":"`diagnostics_extra` is not a valid field"}}"#;
        assert_eq!(
            refused_names(&ErrorBody::read(unrelated_field), &[Injected::Diagnostics]),
            Vec::new()
        );
        let backtick_boundary = refusal("Unexpected value(s) `cache-diagnosis-2026-04-07`");
        assert_eq!(
            refused_names(
                &ErrorBody::read(&backtick_boundary),
                &[Injected::CacheDiagnosisBeta]
            ),
            vec![Injected::CacheDiagnosisBeta]
        );
        let period_boundary = refusal("unknown field diagnostics.");
        assert_eq!(
            refused_names(&ErrorBody::read(&period_boundary), &[Injected::Diagnostics]),
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
