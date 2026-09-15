//! The cooldown classification table, partitioned by account kind, applied to every upstream head before any
//! byte reaches the caller.

use phoebis_store::{AccountKind, AccountState};
use phoebis_transport::response::ResponseHead;

use crate::ratelimit::{retry_after, UnifiedHeaders};

/// Deadlines carry a second of margin so a request sent at the reset is not refused by a clock a hair behind.
const MARGIN_MS: i64 = 1_000;
const PERMISSION_MS: i64 = 3_600_000;

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
    RetrySameAccount,
}

pub struct HeadContext<'a> {
    pub kind: AccountKind,
    pub canonical_model: &'a str,
    pub now: i64,
    pub fallback_ms: i64,
    pub first_event: Option<&'a [u8]>,
    pub body_prefix: Option<&'a [u8]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportPhase {
    BeforeWritten,
    AfterWritten,
}

pub fn classify_transport(phase: TransportPhase) -> Verdict {
    match phase {
        TransportPhase::BeforeWritten => Verdict::RetrySameAccount,
        TransportPhase::AfterWritten => Verdict::Commit,
    }
}

pub fn classify(head: &ResponseHead, ctx: &HeadContext<'_>) -> Verdict {
    let unified = UnifiedHeaders::parse(head);
    let sse_rate_limited = head.status == 200
        && ctx
            .first_event
            .is_some_and(|e| error_type(e) == Some("rate_limit_error"));
    let rate_limited = head.status == 429 || sse_rate_limited;
    match ctx.kind {
        AccountKind::Oauth if rate_limited => {
            return Verdict::FailOver {
                cooldown: Some(oauth_cooldown(head, unified.as_ref(), ctx)),
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
                    error_type(b) == Some("invalid_request_error") && names_credit_balance(b)
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
                .is_some_and(|b| error_type(b) == Some("authentication_error")));
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
            .is_some_and(|b| error_type(b) == Some("permission_error"))
    {
        return Verdict::FailOver {
            cooldown: Some(Cooldown {
                scope: Scope::Model,
                until: ctx.now + PERMISSION_MS + MARGIN_MS,
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
                .or_else(|| retry_after(head, ctx.now).map(|d| ctx.now + d))
                .unwrap_or(ctx.now + ctx.fallback_ms);
            return Cooldown {
                scope: Scope::Account,
                until: until + MARGIN_MS,
                reason: "rate_limit",
                window: exhaustion_window(Some(u), AccountKind::Oauth),
            };
        }
        if fable_only(u) && (rejected(&u.seven_day_oi_status) || rejected(&u.status)) {
            let until = u
                .seven_day_oi_reset
                .or(u.reset)
                .or_else(|| retry_after(head, ctx.now).map(|d| ctx.now + d))
                .unwrap_or(ctx.now + ctx.fallback_ms);
            return Cooldown {
                scope: Scope::Model,
                until: until + MARGIN_MS,
                reason: "rate_limit",
                window: exhaustion_window(Some(u), AccountKind::Oauth),
            };
        }
    }
    let until = retry_after(head, ctx.now)
        .map(|d| ctx.now + d)
        .unwrap_or(ctx.now + ctx.fallback_ms);
    Cooldown {
        scope: Scope::Account,
        until: until + MARGIN_MS,
        reason: "rate_limit",
        window: exhaustion_window(unified, AccountKind::Oauth),
    }
}

/// A rejection whose only rejected window is the model-scoped one. Both shared statuses have to be present and
/// not rejected: an absent status is no evidence that the shared window is fine, and a model-only cooldown on
/// that basis would keep sending an exhausted account traffic.
fn fable_only(u: &UnifiedHeaders) -> bool {
    let allowed = |s: &Option<String>| s.as_deref().is_some_and(|v| v != "rejected");
    let shared_fine = allowed(&u.five_hour_status) && allowed(&u.seven_day_status);
    shared_fine
        && (u.seven_day_oi_status.as_deref() == Some("rejected")
            || u.representative_claim
                .as_deref()
                .is_some_and(|c| c.starts_with("seven_day_") && c != "seven_day"))
}

fn api_key_cooldown(head: &ResponseHead, ctx: &HeadContext<'_>) -> Cooldown {
    match retry_after(head, ctx.now) {
        Some(delay) => Cooldown {
            scope: Scope::Model,
            until: ctx.now + delay + MARGIN_MS,
            reason: "rate_limit",
            window: "retry_after".into(),
        },
        None => Cooldown {
            scope: Scope::Account,
            until: ctx.now + ctx.fallback_ms + MARGIN_MS,
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

fn error_type(body: &[u8]) -> Option<&str> {
    // The prefix may be cut mid-document, so the type is located textually rather than parsed.
    let text = std::str::from_utf8(body).ok()?;
    let at = text.find(r#""error""#)?;
    let rest = &text[at..];
    let t = rest.find(r#""type""#)?;
    let after = rest[t + 6..]
        .trim_start()
        .strip_prefix(':')?
        .trim_start()
        .strip_prefix('"')?;
    after.split('"').next()
}

fn names_credit_balance(body: &[u8]) -> bool {
    std::str::from_utf8(body).is_ok_and(|t| t.to_ascii_lowercase().contains("credit balance"))
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

    fn context(kind: AccountKind) -> HeadContext<'static> {
        HeadContext {
            kind,
            canonical_model: "claude-opus-5",
            now: NOW,
            fallback_ms: FALLBACK,
            first_event: None,
            body_prefix: None,
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
        assert!(matches!(
            classify_transport(TransportPhase::BeforeWritten),
            Verdict::RetrySameAccount
        ));
        assert!(matches!(
            classify_transport(TransportPhase::AfterWritten),
            Verdict::Commit
        ));
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
}
