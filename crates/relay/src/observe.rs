//! What the relay reads out of a request or a response for the bus: the allowlisted headers, the stream flag,
//! the usage block, and the shape of a content delta. Nothing here changes what is relayed.

use axum::http::HeaderMap;
use phoebis_bus::{header_allowed, Usage};
use serde_json::Value;

pub(crate) fn allowlisted_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| header_allowed(name.as_str()))
        .filter_map(|(name, value)| {
            Some((name.as_str().to_string(), value.to_str().ok()?.to_string()))
        })
        .collect()
}

/// The top-level `stream` flag, through the one JSON scanner the workspace has: a `"stream"` member inside a
/// tool schema or a tool input is skipped with the value that holds it.
pub(crate) fn wants_stream(body: &[u8]) -> bool {
    phoebis_cch::bool_member_span(body, &["stream"])
        .is_some_and(|(start, end)| &body[start..end] == b"true")
}

/// The usage block of a JSON document, plus the cache diagnostics beside it. `path` names the object that holds
/// both (`["message"]` for `message_start`, empty for a JSON answer or `message_delta`). A document that does not
/// parse, or has no usage, yields an empty `Usage`, never zeros.
pub(crate) fn usage_from_json(body: &[u8], path: &[&str]) -> Usage {
    let Ok(root) = serde_json::from_slice::<Value>(body) else {
        return Usage::default();
    };
    let mut at = &root;
    for key in path {
        let Some(next) = at.get(*key) else {
            return Usage::default();
        };
        at = next;
    }
    let mut usage = Usage::default();
    if let Some(block) = at.get("usage") {
        usage.input_tokens = block.get("input_tokens").and_then(Value::as_u64);
        usage.output_tokens = block.get("output_tokens").and_then(Value::as_u64);
        usage.cache_read_tokens = block.get("cache_read_input_tokens").and_then(Value::as_u64);
        usage.service_tier = block
            .get("service_tier")
            .and_then(Value::as_str)
            .map(str::to_string);
        match block.get("cache_creation") {
            // The split object is the newer shape and says which TTL was written; the total is the older one.
            Some(split) => {
                usage.cache_write_5m_tokens = split
                    .get("ephemeral_5m_input_tokens")
                    .and_then(Value::as_u64);
                usage.cache_write_1h_tokens = split
                    .get("ephemeral_1h_input_tokens")
                    .and_then(Value::as_u64);
            }
            None => {
                usage.cache_write_5m_tokens = block
                    .get("cache_creation_input_tokens")
                    .and_then(Value::as_u64);
            }
        }
    }
    usage.cache_miss_reason = at
        .get("diagnostics")
        .and_then(|d| d.get("cache_miss_reason"))
        .and_then(|r| r.get("type"))
        .and_then(Value::as_str)
        .map(str::to_string);
    usage
}

/// A `content_block_delta` whose delta is tool input, which is what the keepalive view counts as tool use.
pub(crate) fn is_tool_use_delta(data: &[u8]) -> bool {
    data.windows(18).any(|w| w == br#""input_json_delta""#)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_is_read_from_the_root_or_under_message_and_a_missing_block_is_empty() {
        let json = br#"{"type":"message","usage":{"input_tokens":10,"output_tokens":2,"cache_read_input_tokens":4,"cache_creation_input_tokens":6,"service_tier":"standard"},"diagnostics":{"cache_miss_reason":{"type":"tools_changed"}}}"#;
        let usage = usage_from_json(json, &[]);
        assert_eq!(
            (
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_tokens,
                usage.cache_write_5m_tokens,
                usage.cache_write_1h_tokens
            ),
            (Some(10), Some(2), Some(4), Some(6), None)
        );
        assert_eq!(
            (
                usage.service_tier.as_deref(),
                usage.cache_miss_reason.as_deref()
            ),
            (Some("standard"), Some("tools_changed"))
        );
        let start = br#"{"type":"message_start","message":{"usage":{"input_tokens":1,"cache_creation":{"ephemeral_5m_input_tokens":8,"ephemeral_1h_input_tokens":9}}}}"#;
        let usage = usage_from_json(start, &["message"]);
        assert_eq!(
            (usage.cache_write_5m_tokens, usage.cache_write_1h_tokens),
            (Some(8), Some(9)),
            "the split object wins over the total"
        );
        assert!(usage_from_json(br#"{"type":"message"}"#, &[]).is_empty());
        assert!(usage_from_json(b"not json", &[]).is_empty());
    }

    #[test]
    fn a_stream_flag_and_a_tool_use_delta_are_recognised() {
        assert!(wants_stream(br#"{"model":"m","stream":true}"#));
        assert!(wants_stream(br#"{"model":"m", "stream": true}"#));
        assert!(!wants_stream(br#"{"model":"m","stream":false}"#));
        assert!(!wants_stream(br#"{"model":"m"}"#));
        assert!(wants_stream(br#"{"model":"m","tools":[{"input_schema":{"properties":{"stream":{"default":false}}}}],"stream":true}"#), "a nested member is not the flag");
        assert!(!wants_stream(br#"{"model":"m","stream":"true"}"#));
        assert!(is_tool_use_delta(br#"{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{"}}"#));
        assert!(!is_tool_use_delta(
            br#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"x"}}"#
        ));
    }
}
