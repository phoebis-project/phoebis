//! The caller's client version, read for the ledger and judged by the contract's version item, which refuses a
//! `claude-cli/` version spelled any other way than the client's. The two client properties the relay refuses on
//! beside it live with the body edits: a billing hash this build cannot reproduce (`hash`), and a `cc_version`
//! suffix that does not reproduce from the prompt and the user agent (`version_suffix`).

use axum::http::HeaderMap;

use crate::identity::{user_agent_of, CLAUDE_CLI_PREFIX};

/// The version out of a `claude-cli/` user agent, or `None` for a caller that is not one.
pub(crate) fn client_version(headers: &HeaderMap) -> Option<String> {
    Some(
        user_agent_of(headers)?
            .strip_prefix(CLAUDE_CLI_PREFIX)?
            .split(' ')
            .next()?
            .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::client_version;
    use axum::http::HeaderMap;

    fn agent(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("user-agent", value.parse().unwrap());
        headers
    }

    #[test]
    fn every_claude_cli_version_is_read_and_any_other_caller_reads_none() {
        assert_eq!(
            client_version(&agent("claude-cli/2.1.269 (external, cli)")).as_deref(),
            Some("2.1.269")
        );
        assert_eq!(
            client_version(&agent("claude-cli/2.1.300 (external, cli)")).as_deref(),
            Some("2.1.300"),
            "a version the profile never measured is read the same way"
        );
        assert_eq!(client_version(&agent("curl/8.5.0")), None);
        assert_eq!(client_version(&HeaderMap::new()), None);
    }
}
