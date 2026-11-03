//! The codes that flag a request row as refused, and the ledger's reading of a row against them. Each code names a
//! refusal the router issues before it asks any account, for the client's or a policy's cause.
//!
//! The codes that stay in the figures and off the list are `account_canceled`, `accounts_unavailable`,
//! `body_budget_exhausted`, `key_body_reads`, `ledger_behind` and `router_unavailable`, since the router's own standing
//! caused them, and `accounts_exhausted`, `model_rejected` and every upstream code, since an account was asked.

macro_rules! refusal_codes {
    ($first:literal $(, $rest:literal)* $(,)?) => {
        /// The codes that flag a rejected request with no attempt as refused, alphabetical.
        pub const REFUSAL_CODES: &[&str] = &[$first $(, $rest)*];
    };
}
refusal_codes!(
    "auth_ambiguous",
    "auth_malformed",
    "auth_missing",
    "auth_unknown",
    "body_shape",
    "client_contract",
    "client_drift",
    "client_not_allowed",
    "guardrail_blocked",
    "invalid_body",
    "invalid_model",
    "method_not_allowed",
    "missing_model",
    "model_not_allowed",
    "not_found",
    "request_incomplete",
    "request_timeout",
    "request_too_large",
    "unsupported_content_encoding",
);

/// The ledger's reading of a row's own facts: a kind other than `probe`, the outcome `rejected`, a code of
/// `REFUSAL_CODES` and an attempt count of zero, the count the row stores. An `auth_unknown` or a `model_not_allowed`
/// answered at a reselect after an attempt stays a rejected request, also where the bus lost that attempt's selection
/// event and the trail holds no row, and a probe the registry refused stays on its probe row.
pub fn is_refusal(kind: &str, outcome: &str, error_code: Option<&str>, attempts: u32) -> bool {
    kind != "probe"
        && outcome == "rejected"
        && attempts == 0
        && error_code.is_some_and(|code| REFUSAL_CODES.contains(&code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_refusal_flags_a_rejected_client_request_with_a_listed_code_and_no_attempt_and_nothing_else(
    ) {
        let cases: [(&str, &str, Option<&str>, u32, bool); 7] = [
            ("inference", "rejected", Some("guardrail_blocked"), 0, true),
            ("count_tokens", "rejected", Some("auth_missing"), 0, true),
            ("probe", "rejected", Some("auth_unknown"), 0, false),
            (
                "inference",
                "rejected",
                Some("client_not_allowed"),
                1,
                false,
            ),
            (
                "inference",
                "rejected",
                Some("accounts_unavailable"),
                0,
                false,
            ),
            ("inference", "failed", Some("guardrail_blocked"), 0, false),
            ("inference", "rejected", None, 0, false),
        ];
        for (kind, outcome, code, attempts, want) in cases {
            assert_eq!(
                is_refusal(kind, outcome, code, attempts),
                want,
                "a {kind} request {outcome} with {code:?} after {attempts} attempts"
            );
        }
    }
}
