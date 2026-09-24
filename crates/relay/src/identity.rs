//! Who and what a request is: the session, the agent, the owner, and the model it asks for.

use axum::http::HeaderMap;
use phoebis_bus::OTHER_CLIENT;

use crate::billing::entrypoint_of;
use crate::RelayError;

/// The agent a request without an agent header belongs to.
const MAIN_AGENT: &str = "main";

/// Marker a downstream client appends to ask for the long-context tier. It is a client-side routing hint, so
/// it is folded out of the canonical model the registry resolves against.
const LONG_CONTEXT_SUFFIX: &str = "[1m]";

/// Bound on an identifier or model name the relay repeats into logs and ledger keys.
pub(crate) const MAX_ID_LEN: usize = 256;

#[derive(Debug)]
pub struct RequestIdentity {
    pub session_id: Option<String>,
    pub agent_id: String,
    pub parent_agent_id: Option<String>,
    pub owner_id: Option<String>,
    pub model: String,
    /// The client's spelling with the `[1m]` suffix removed and ASCII lowercased: what the body carries
    /// upstream, and the resolution map's own key.
    pub folded_model: String,
    /// `folded_model`, or the dated id the catalog cache resolved it to: the routing, ledger, and pricing key.
    pub canonical_model: String,
    /// `claude-code/<entrypoint>` or `other`: a log label, never a gate.
    pub client: String,
}

pub fn identify(headers: &HeaderMap, body: &[u8]) -> Result<RequestIdentity, RelayError> {
    let model = string_member(body, &["model"]).ok_or(RelayError::NoModel)?;
    let folded_model = {
        let lowered = model.to_ascii_lowercase();
        lowered
            .strip_suffix(LONG_CONTEXT_SUFFIX)
            .unwrap_or(&lowered)
            .to_string()
    };
    // The model reaches log lines and ledger keys, so it is bounded and charset-checked here rather than
    // wherever it is printed. Unlike the identifiers below it cannot be dropped: a request without a usable
    // model has nothing to relay.
    if !is_valid_model(&folded_model) {
        return Err(RelayError::InvalidModel);
    }
    // The header is the client's own statement of the session.
    let session_id = header_id(headers, "x-claude-code-session-id");
    Ok(RequestIdentity {
        session_id,
        agent_id: header_id(headers, "x-claude-code-agent-id")
            .unwrap_or_else(|| MAIN_AGENT.to_string()),
        parent_agent_id: header_id(headers, "x-claude-code-parent-agent-id"),
        owner_id: header_id(headers, "x-keepalive-owner-id"),
        model,
        canonical_model: folded_model.clone(),
        folded_model,
        client: client_class(headers),
    })
}

/// `claude-code/<entrypoint>` for a `claude-cli/` user agent (the token `entrypoint_of` reads, `cli` when it
/// reads none), `other` for anything else. The entrypoint is bounded by `entrypoint_of`, so the class is safe
/// for a log line and a ledger key.
pub fn client_class(headers: &HeaderMap) -> String {
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if user_agent.starts_with("claude-cli/") {
        return format!("claude-code/{}", entrypoint_of(user_agent));
    }
    OTHER_CLIENT.to_string()
}

/// Brackets are allowed on top of the identifier charset because a client may name a tier with them, as the
/// long-context suffix does.
fn is_valid_model(value: &str) -> bool {
    (1..=MAX_ID_LEN).contains(&value.len())
        && value.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'/' | b':' | b'[' | b']' | b'_')
        })
}

pub(crate) fn is_valid_id(value: &str) -> bool {
    (1..=MAX_ID_LEN).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'/' | b':' | b'_'))
}

/// A malformed identifier is treated as absent: it is a label, never an authorization decision, so a caller
/// cannot gain anything by sending one the relay would otherwise have to reject the whole request over.
pub(crate) fn header_id(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| is_valid_id(value))
        .map(str::to_string)
}

/// Decodes a string member of the top-level object without re-serializing the body around it.
fn string_member(body: &[u8], path: &[&str]) -> Option<String> {
    let (start, end) = phoebis_cch::string_member_span(body, path)?;
    serde_json::from_slice::<String>(&body[start..end]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &[u8] = br#"{"model":"claude-sonnet-4-5[1M]","metadata":{"user_id":"{\"account_uuid\":\"\",\"session_id\":\"s-1\"}"}}"#;

    #[test]
    fn the_model_suffix_is_folded_and_the_metadata_session_is_not_read() {
        let identity = identify(&HeaderMap::new(), BODY).expect("the body has a model");
        assert_eq!(identity.session_id, None);
        assert_eq!(identity.agent_id, "main");
        assert_eq!(identity.folded_model, "claude-sonnet-4-5");
        assert_eq!(identity.canonical_model, "claude-sonnet-4-5");
        assert_eq!(identity.model, "claude-sonnet-4-5[1M]");
    }

    #[test]
    fn a_model_outside_the_charset_or_over_the_bound_is_rejected() {
        let euro = br#"{"model":"claude-sonnet-4-5\u20ac\u20ac"}"#;
        assert!(matches!(
            identify(&HeaderMap::new(), euro),
            Err(RelayError::InvalidModel)
        ));
        let newline = br#"{"model":"claude\nsonnet"}"#;
        assert!(matches!(
            identify(&HeaderMap::new(), newline),
            Err(RelayError::InvalidModel)
        ));
        let long = format!(r#"{{"model":"{}"}}"#, "m".repeat(257));
        assert!(matches!(
            identify(&HeaderMap::new(), long.as_bytes()),
            Err(RelayError::InvalidModel)
        ));
        let empty = br#"{"model":""}"#;
        assert!(matches!(
            identify(&HeaderMap::new(), empty),
            Err(RelayError::InvalidModel)
        ));
    }

    #[test]
    fn a_malformed_header_id_is_absent_and_a_bodyless_model_is_an_error() {
        let mut headers = HeaderMap::new();
        headers.insert("x-claude-code-agent-id", "bad id!".parse().unwrap());
        headers.insert("x-claude-code-session-id", "s-2".parse().unwrap());
        let identity = identify(&headers, BODY).expect("the body has a model");
        assert_eq!(identity.agent_id, "main");
        assert_eq!(identity.session_id.as_deref(), Some("s-2"));
        assert!(matches!(
            identify(&headers, b"{\"messages\":[]}"),
            Err(RelayError::NoModel)
        ));
    }

    #[test]
    fn the_client_class_reads_the_entrypoint_or_other() {
        let agent = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(axum::http::header::USER_AGENT, value.parse().unwrap());
            headers
        };
        assert_eq!(
            client_class(&agent("claude-cli/2.1.269 (external, sdk-cli)")),
            "claude-code/sdk-cli"
        );
        assert_eq!(
            client_class(&agent("claude-cli/2.1.269")),
            "claude-code/cli"
        );
        assert_eq!(
            client_class(&agent("claude-cli/2.1.269;x (external, cli; y)")),
            "claude-code/cli",
            "the class is a label and never the version gate"
        );
        let over_bound = format!(
            "claude-cli/2.1.269 (external, {})",
            "a".repeat(MAX_ID_LEN + 1)
        );
        assert_eq!(
            client_class(&agent(&over_bound)),
            "claude-code/cli",
            "an entrypoint token past the bound reads as unknown rather than widening the class"
        );
        let mut headers = agent("anthropic-sdk-python/0.40.0");
        headers.insert("x-stainless-lang", "python".parse().unwrap());
        assert_eq!(client_class(&headers), OTHER_CLIENT);
        assert_eq!(
            client_class(&agent("anthropic-sdk-python/0.40.0")),
            OTHER_CLIENT
        );
        let mut headers = agent("anthropic-sdk-python/0.40.0");
        headers.insert("x-stainless-lang", "py thon".parse().unwrap());
        assert_eq!(
            client_class(&headers),
            OTHER_CLIENT,
            "a stainless header on an agent outside Claude Code does not change the label"
        );
    }
}
