//! The caller's client version, read for the ledger and judged by the contract's version item, which refuses a
//! `claude-cli/` version spelled any other way than the client's. The two client properties the relay refuses on
//! beside it live with the body edits: a billing hash this build cannot reproduce (`hash`), and a `cc_version`
//! suffix that does not reproduce from the prompt and the user agent (`version_suffix`).

use axum::http::{header, HeaderMap};

pub struct DriftGuard;

impl DriftGuard {
    /// The version out of a `claude-cli/` user agent, or `None` for a caller that is not one.
    pub fn check(&self, headers: &HeaderMap) -> Option<String> {
        let agent = headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())?;
        Some(agent.strip_prefix("claude-cli/")?.split(' ').next()?.into())
    }
}

#[cfg(test)]
mod tests {
    use super::DriftGuard;
    use axum::http::HeaderMap;

    fn agent(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("user-agent", value.parse().unwrap());
        headers
    }

    #[test]
    fn every_claude_cli_version_is_read_and_any_other_caller_reads_none() {
        let guard = DriftGuard;
        assert_eq!(
            guard
                .check(&agent("claude-cli/2.1.269 (external, cli)"))
                .as_deref(),
            Some("2.1.269")
        );
        assert_eq!(
            guard
                .check(&agent("claude-cli/2.1.300 (external, cli)"))
                .as_deref(),
            Some("2.1.300"),
            "a version the profile never measured is read the same way"
        );
        assert_eq!(guard.check(&agent("curl/8.5.0")), None);
        assert_eq!(guard.check(&HeaderMap::new()), None);
    }
}
