//! What the relay reads out of a request or a response for the bus: the published headers, the stream flag,
//! the usage block and the answer's own facts, and the shape of a content delta. Nothing here changes what
//! is relayed.

use axum::http::HeaderMap;
use phoebis_bus::{header_published, ResponseFacts, Usage};
use serde_json::Value;

/// The caller's headers as the bus carries them: everything the denylist does not name, which is what a
/// keepalive recipe replays.
pub(crate) fn published_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| header_published(name.as_str()))
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

pub(crate) struct Answer {
    pub usage: Usage,
    pub response: ResponseFacts,
}

/// The usage block, the cache diagnostics beside it, and what the answer says about itself, from one parse.
/// `path` names the object that holds them (`["message"]` for `message_start`, empty for a JSON answer or
/// `message_delta`). A document that does not parse, or has none of them, yields empty members, never zeros.
pub(crate) fn answer_from_json(body: &[u8], path: &[&str]) -> Answer {
    let empty = || Answer {
        usage: Usage::default(),
        response: ResponseFacts::default(),
    };
    let Ok(root) = serde_json::from_slice::<Value>(body) else {
        return empty();
    };
    let mut at = &root;
    for key in path {
        let Some(next) = at.get(*key) else {
            return empty();
        };
        at = next;
    }
    let text = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_string);
    let count = |v: Option<&Value>| v.and_then(Value::as_u64);
    let mut usage = Usage::default();
    if let Some(block) = at.get("usage") {
        usage.input_tokens = count(block.get("input_tokens"));
        usage.output_tokens = count(block.get("output_tokens"));
        usage.cache_read_tokens = count(block.get("cache_read_input_tokens"));
        usage.service_tier = text(block.get("service_tier"));
        match block.get("cache_creation") {
            // The split object is the newer shape and says which TTL was written; the total is the older one.
            Some(split) => {
                usage.cache_write_5m_tokens = count(split.get("ephemeral_5m_input_tokens"));
                usage.cache_write_1h_tokens = count(split.get("ephemeral_1h_input_tokens"));
            }
            None => {
                usage.cache_write_5m_tokens = count(block.get("cache_creation_input_tokens"));
            }
        }
        usage.thinking_tokens = count(
            block
                .get("output_tokens_details")
                .and_then(|d| d.get("thinking_tokens")),
        );
        let tools = block.get("server_tool_use");
        usage.web_search_requests = count(tools.and_then(|t| t.get("web_search_requests")));
        usage.web_fetch_requests = count(tools.and_then(|t| t.get("web_fetch_requests")));
    }
    usage.cache_miss_reason = text(
        at.get("diagnostics")
            .and_then(|d| d.get("cache_miss_reason"))
            .and_then(|r| r.get("type")),
    );
    // A `message_delta` nests the stop under `delta`; a whole answer carries it at its own level.
    let stop =
        |key: &str| text(at.get(key)).or_else(|| text(at.get("delta").and_then(|d| d.get(key))));
    // A delta's own refusal details sit under `delta.stop_details`, never at the root alongside it; reading
    // both would let a whole answer's root member leak onto a delta that never carried one.
    let details = match at.get("delta") {
        Some(delta) => delta.get("stop_details"),
        None => at.get("stop_details"),
    };
    let response = ResponseFacts {
        stop_reason: stop("stop_reason"),
        stop_sequence: stop("stop_sequence"),
        model: text(at.get("model")),
        speed: text(at.get("usage").and_then(|u| u.get("speed"))),
        inference_geo: text(at.get("usage").and_then(|u| u.get("inference_geo"))),
        // One entry per edit the upstream applied; the sum is what left the context this turn.
        cleared_input_tokens: at
            .get("context_management")
            .and_then(|c| c.get("applied_edits"))
            .and_then(Value::as_array)
            .map(|edits| {
                edits
                    .iter()
                    .filter_map(|e| count(e.get("cleared_input_tokens")))
                    .sum()
            }),
        refusal_category: text(details.and_then(|d| d.get("category"))),
        refusal_recommended_model: text(details.and_then(|d| d.get("recommended_model"))),
    };
    Answer { usage, response }
}

/// A `content_block_delta` whose delta is tool input, which is what the keepalive view counts as tool use.
pub(crate) fn is_tool_use_delta(data: &[u8]) -> bool {
    data.windows(18).any(|w| w == br#""input_json_delta""#)
}

/// The message id of a Messages answer: `message.id` under `path` (`["message"]` for `message_start`, empty for a
/// JSON answer). `None` when the document does not parse or carries no string id.
pub(crate) fn message_id_from_json(body: &[u8], path: &[&str]) -> Option<String> {
    let root = serde_json::from_slice::<Value>(body).ok()?;
    let mut at = &root;
    for key in path {
        at = at.get(*key)?;
    }
    at.get("id").and_then(Value::as_str).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_is_read_from_the_root_or_under_message_and_a_missing_block_is_empty() {
        let json = br#"{"type":"message","usage":{"input_tokens":10,"output_tokens":2,"cache_read_input_tokens":4,"cache_creation_input_tokens":6,"service_tier":"standard"},"diagnostics":{"cache_miss_reason":{"type":"tools_changed"}}}"#;
        let usage = answer_from_json(json, &[]).usage;
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
        let usage = answer_from_json(start, &["message"]).usage;
        assert_eq!(
            (usage.cache_write_5m_tokens, usage.cache_write_1h_tokens),
            (Some(8), Some(9)),
            "the split object wins over the total"
        );
        assert!(answer_from_json(br#"{"type":"message"}"#, &[])
            .usage
            .is_empty());
        assert!(answer_from_json(b"not json", &[]).usage.is_empty());
    }

    #[test]
    fn the_answers_own_facts_are_read_from_the_head_the_delta_and_a_json_answer() {
        let start = br#"{"type":"message_start","message":{"id":"msg_1","model":"claude-opus-5","usage":{"input_tokens":1,"speed":"fast","inference_geo":"us"}}}"#;
        let head = answer_from_json(start, &["message"]);
        assert_eq!(
            (
                head.response.model.as_deref(),
                head.response.speed.as_deref(),
                head.response.inference_geo.as_deref()
            ),
            (Some("claude-opus-5"), Some("fast"), Some("us"))
        );
        assert!(
            head.response.stop_reason.is_none(),
            "the head has not stopped yet"
        );
        let delta = br#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":40,"output_tokens_details":{"thinking_tokens":25},"server_tool_use":{"web_search_requests":2,"web_fetch_requests":1}},"context_management":{"applied_edits":[{"type":"clear_tool_uses_20250919","cleared_tool_uses":3,"cleared_input_tokens":1200},{"type":"clear_thinking_20251015","cleared_thinking_turns":2}]}}"#;
        let tail = answer_from_json(delta, &[]);
        assert_eq!(
            (
                tail.usage.output_tokens,
                tail.usage.thinking_tokens,
                tail.usage.web_search_requests,
                tail.usage.web_fetch_requests
            ),
            (Some(40), Some(25), Some(2), Some(1))
        );
        assert_eq!(
            tail.response.stop_reason.as_deref(),
            Some("end_turn"),
            "the delta nests the stop under its own member and nowhere else"
        );
        assert_eq!(tail.response.stop_sequence, None);
        assert_eq!(
            tail.response.cleared_input_tokens,
            Some(1_200),
            "the edit with no token count adds nothing"
        );
        let refused = br#"{"id":"msg_2","type":"message","model":"claude-fable-5-1","stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber","recommended_model":"claude-opus-5"},"usage":{"input_tokens":5,"output_tokens":0}}"#;
        let answer = answer_from_json(refused, &[]);
        assert_eq!(
            (
                answer.response.stop_reason.as_deref(),
                answer.response.refusal_category.as_deref(),
                answer.response.refusal_recommended_model.as_deref()
            ),
            (Some("refusal"), Some("cyber"), Some("claude-opus-5"))
        );
        assert!(answer_from_json(br#"{"type":"message"}"#, &[])
            .response
            .is_empty());
    }

    #[test]
    fn a_deltas_refusal_details_are_read_from_delta_stop_details_not_the_root() {
        let delta = br#"{"type":"message_delta","delta":{"stop_reason":"refusal","stop_sequence":null,"stop_details":{"type":"refusal","category":"cyber","recommended_model":"claude-opus-5"}},"usage":{"output_tokens":3}}"#;
        let answer = answer_from_json(delta, &[]);
        assert_eq!(
            (
                answer.response.stop_reason.as_deref(),
                answer.response.refusal_category.as_deref(),
                answer.response.refusal_recommended_model.as_deref()
            ),
            (Some("refusal"), Some("cyber"), Some("claude-opus-5"))
        );
        // A root `stop_details` beside a `delta` object is never a whole answer's own member landing on a
        // delta by coincidence, so it must not be read once the delta has its own (possibly empty) member.
        let root_only = br#"{"type":"message_delta","delta":{"stop_reason":"refusal"},"stop_details":{"type":"refusal","category":"root-leaked"}}"#;
        let answer = answer_from_json(root_only, &[]);
        assert_eq!(answer.response.refusal_category, None);
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

    #[test]
    fn the_message_id_is_read_under_the_path_and_nowhere_else() {
        assert_eq!(
            message_id_from_json(
                br#"{"type":"message_start","message":{"id":"msg_01","usage":{}}}"#,
                &["message"]
            )
            .as_deref(),
            Some("msg_01")
        );
        assert_eq!(
            message_id_from_json(br#"{"id":"msg_02","type":"message"}"#, &[]).as_deref(),
            Some("msg_02")
        );
        assert_eq!(
            message_id_from_json(
                br#"{"type":"message_start","message":{"id":7}}"#,
                &["message"]
            ),
            None,
            "a non-string id is no id"
        );
        assert_eq!(
            message_id_from_json(br#"{"type":"message_start"}"#, &["message"]),
            None
        );
    }

    #[test]
    fn an_unparseable_document_yields_no_id() {
        assert_eq!(message_id_from_json(b"{not json", &[]), None);
    }
}
