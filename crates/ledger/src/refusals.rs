//! The codes that flag a request row as refused, and the ledger's reading of a row against them. Each code names a
//! refusal the router issues before it asks any account, for the client's or a policy's cause.
//!
//! The codes that stay in the figures and off the list are `account_cancelled`, `accounts_unavailable`,
//! `body_budget_exhausted`, `key_body_reads`, `ledger_behind` and `router_unavailable`, since the router's own standing
//! caused them, and `accounts_exhausted`, `model_rejected` and every upstream code, since an account was asked.

/// One token list feeds the slice and the step's `IN (..)` text, so `is_refusal` and the step's backfill cannot
/// name different codes.
macro_rules! refusal_codes {
    ($first:literal $(, $rest:literal)* $(,)?) => {
        /// The codes that flag a rejected request with no attempt as refused, alphabetical, the same list the step's
        /// backfill SQL is generated from.
        pub const REFUSAL_CODES: &[&str] = &[$first $(, $rest)*];
        pub(crate) const REFUSED_STEP_SQL: &str = concat!(
            "ALTER TABLE requests ADD COLUMN refused INTEGER NOT NULL DEFAULT 0;\n",
            "ALTER TABLE rollups_hourly ADD COLUMN refusals_json TEXT NOT NULL DEFAULT '{}';\n",
            "UPDATE requests SET refused = 1 WHERE kind <> 'probe' AND outcome = 'rejected' AND error_code IN (",
            "'", $first, "'" $(, ", '", $rest, "'")*,
            ") AND requests.attempts = 0",
            " AND NOT EXISTS (SELECT 1 FROM attempts a WHERE a.request_id = requests.id);"
        );
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
/// `REFUSAL_CODES` and an attempt count of zero, the count the row stores. A `client_not_allowed`,
/// `model_not_allowed` or `auth_unknown` answered at a reselect after an attempt stays a rejected request, also where
/// the bus lost that attempt's selection event and the trail holds no row, and a probe the registry refused stays on
/// its probe row.
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

    #[test]
    fn the_steps_backfill_names_every_refusal_code_and_no_other() {
        for code in REFUSAL_CODES {
            assert!(
                REFUSED_STEP_SQL.contains(&format!("'{code}'")),
                "the backfill names {code}: {REFUSED_STEP_SQL}"
            );
        }
        let (_, after) = REFUSED_STEP_SQL
            .split_once("IN (")
            .expect("the backfill lists its codes");
        let (list, _) = after.split_once(')').expect("the list closes");
        let quoted = list
            .split(", ")
            .filter(|item| item.len() > 2 && item.starts_with('\'') && item.ends_with('\''))
            .count();
        assert_eq!(
            (quoted, list.split(", ").count()),
            (REFUSAL_CODES.len(), REFUSAL_CODES.len()),
            "one quoted item per code and nothing else: {list}"
        );
        assert!(
            REFUSAL_CODES.windows(2).all(|pair| pair[0] < pair[1]),
            "the list is strictly ascending: {REFUSAL_CODES:?}"
        );
        assert_eq!(REFUSAL_CODES.len(), 19, "the rule names nineteen codes");
        assert!(
            REFUSED_STEP_SQL.starts_with(
                "ALTER TABLE requests ADD COLUMN refused INTEGER NOT NULL DEFAULT 0;\n\
                 ALTER TABLE rollups_hourly ADD COLUMN refusals_json TEXT NOT NULL DEFAULT '{}';\n"
            ),
            "the step adds both columns first: {REFUSED_STEP_SQL}"
        );
        for clause in [
            "kind <> 'probe'",
            "requests.attempts = 0",
            "NOT EXISTS (SELECT 1 FROM attempts",
        ] {
            assert!(
                REFUSED_STEP_SQL.contains(clause),
                "the backfill carries {clause}: {REFUSED_STEP_SQL}"
            );
        }
    }
}
