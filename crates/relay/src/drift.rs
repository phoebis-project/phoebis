//! The caller's client version, read for the ledger. Every version is relayed the same way, so the version is
//! recorded rather than judged: the only client property the relay still refuses on is a billing hash it
//! cannot reproduce, and that lives with the body edits.

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
    fn every_claude_cli_version_is_read_and_none_is_judged() {
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
