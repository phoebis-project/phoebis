//! What the relay reads out of a request or a response for the bus: the published headers, the stream flag,
//! the usage block and the answer's own facts, and the shape of a content delta. Nothing here changes what
//! is relayed.

use std::collections::BTreeMap;

use axum::http::{header, HeaderMap};
use phoebis_bus::{header_published, InputTransformation, ResponseFacts, Usage};
use serde_json::Value;

use crate::credit::{digest_of, hex_of, CreditDigest};
use crate::identity::is_filed_word;

/// The caller's headers as the bus carries them: everything the denylist does not name, which is what a
/// keepalive recipe replays. A request the relay inflated is published as the plain body it holds, so a recipe
/// replays plain: with `inflated` its `content-encoding` line is left out.
pub(crate) fn published_headers(headers: &HeaderMap, inflated: bool) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| header_published(name.as_str()))
        .filter(|(name, _)| !(inflated && *name == header::CONTENT_ENCODING))
        .filter_map(|(name, value)| {
            Some((name.as_str().to_string(), value.to_str().ok()?.to_string()))
        })
        .collect()
}

/// The top-level `stream` flag, through the one JSON scanner the workspace has: a `"stream"` member inside a
/// tool schema or a tool input is skipped with the value that holds it.
pub(crate) fn wants_stream(members: &phoebis_cch::TopLevelMembers<'_>) -> bool {
    members
        .bool_member_span("stream")
        .is_some_and(|(start, end)| &members.body()[start..end] == b"true")
}

#[derive(Default)]
pub(crate) struct Answer {
    pub usage: Usage,
    pub response: ResponseFacts,
    /// The digest of `stop_details.fallback_credit_token`, taken here so the bytes never leave the read.
    pub credit_digest: Option<CreditDigest>,
    /// The answer's string `id` under the path.
    pub message_id: Option<String>,
}

/// The usage block, the cache diagnostics beside it, and what the answer says about itself, from one parse.
/// `path` names the object that holds them (`["message"]` for `message_start`, empty for a JSON answer or
/// `message_delta`). A document that does not parse, or has none of them, yields empty members, never zeros.
/// `named` says whether the pass's body carried a `diagnostics` member naming a previous message, the one case where
/// a null answer member reads `none`.
pub(crate) fn answer_from_json(body: &[u8], path: &[&str], named: bool) -> Answer {
    let Ok(root) = serde_json::from_slice::<Value>(body) else {
        return Answer::default();
    };
    let mut at = &root;
    for key in path {
        let Some(next) = at.get(*key) else {
            return Answer::default();
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
    usage.cache_miss_reason = match at.get("diagnostics") {
        None => None,
        // A null member answers the message the body named, and says nothing where the body named none.
        Some(Value::Null) => named.then(|| "none".to_string()),
        Some(diagnostics) => match diagnostics.get("cache_miss_reason") {
            // The upstream's comparison was still running when the answer went out.
            Some(Value::Null) => Some("pending".to_string()),
            reason => text(reason.and_then(|r| r.get("type"))),
        },
    };
    // A `message_delta` nests the stop under `delta`; a whole answer carries it at its own level.
    let stop =
        |key: &str| text(at.get(key)).or_else(|| text(at.get("delta").and_then(|d| d.get(key))));
    // A delta's own refusal details sit under `delta.stop_details`, never at the root alongside it; reading
    // both would let a whole answer's root member leak onto a delta that never carried one.
    let details = match at.get("delta") {
        Some(delta) => delta.get("stop_details"),
        None => at.get("stop_details"),
    };
    let credit_digest = details
        .and_then(|d| d.get("fallback_credit_token"))
        .and_then(Value::as_str)
        .map(digest_of);
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
        issued_digest: credit_digest.as_ref().map(hex_of),
        input_transformations: input_transformations_of(at),
    };
    Answer {
        usage,
        response,
        credit_digest,
        message_id: text(at.get("id")),
    }
}

/// Files the relay's own word on an answer's usage once every event of it merged, where the usage holds a report and
/// no reason: `not_requested` where the leg's body carried no `diagnostics` member, `not_returned` where it carried
/// one, a member that named no message included. A word read off one event would replace the reason an earlier event
/// reported, since the later value of a merge wins, and an answer that held no report keeps its null.
pub(crate) fn close_miss_reason(usage: &mut Usage, diagnostics_sent: bool) {
    let reported = usage.input_tokens.is_some()
        || usage.output_tokens.is_some()
        || usage.cache_read_tokens.is_some()
        || usage.cache_write_5m_tokens.is_some()
        || usage.cache_write_1h_tokens.is_some();
    if reported && usage.cache_miss_reason.is_none() {
        let word = if diagnostics_sent {
            "not_returned"
        } else {
            "not_requested"
        };
        usage.cache_miss_reason = Some(word.to_string());
    }
}

/// The `input_transformations` array beside `usage`, per `<type>.<reason>` key. An entry is read only where its two
/// words are strings and its `path` places it in a message, so each key holds a message index.
fn input_transformations_of(at: &Value) -> Option<BTreeMap<String, InputTransformation>> {
    let entries = at.get("input_transformations")?.as_array()?;
    let word = |w: &str| {
        if is_filed_word(w) {
            w.to_string()
        } else {
            "other".to_string()
        }
    };
    let mut map = BTreeMap::<String, InputTransformation>::new();
    for entry in entries {
        let (Some(kind), Some(reason), Some(index)) = (
            entry.get("type").and_then(Value::as_str),
            entry.get("reason").and_then(Value::as_str),
            entry
                .get("path")
                .and_then(Value::as_str)
                .and_then(message_index),
        ) else {
            continue;
        };
        let dropped = map
            .entry(format!("{}.{}", word(kind), word(reason)))
            .or_insert(InputTransformation {
                blocks: 0,
                first_message: index,
            });
        dropped.blocks += 1;
        dropped.first_message = dropped.first_message.min(index);
    }
    Some(map)
}

/// The `i` of a path that reads `messages.{i}`, alone or followed by a `.`.
fn message_index(path: &str) -> Option<u64> {
    let rest = path.strip_prefix("messages.")?;
    let digits = rest.split_once('.').map_or(rest, |(index, _)| index);
    // `parse` alone would take a leading `+`.
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// A `content_block_delta` whose delta is tool input, which is what the keepalive view counts as tool use.
pub(crate) fn is_tool_use_delta(data: &[u8]) -> bool {
    phoebis_cch::find(data, br#""input_json_delta""#).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_is_read_from_the_root_or_under_message_and_a_missing_block_is_empty() {
        let json = br#"{"type":"message","usage":{"input_tokens":10,"output_tokens":2,"cache_read_input_tokens":4,"cache_creation_input_tokens":6,"service_tier":"standard"},"diagnostics":{"cache_miss_reason":{"type":"tools_changed"}}}"#;
        let usage = answer_from_json(json, &[], false).usage;
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
        let usage = answer_from_json(start, &["message"], false).usage;
        assert_eq!(
            (usage.cache_write_5m_tokens, usage.cache_write_1h_tokens),
            (Some(8), Some(9)),
            "the split object wins over the total"
        );
        assert!(answer_from_json(br#"{"type":"message"}"#, &[], false)
            .usage
            .is_empty());
        assert!(answer_from_json(b"not json", &[], false).usage.is_empty());
    }

    #[test]
    fn a_null_diagnostics_member_reads_none_where_the_body_named_a_message_and_a_null_reason_reads_pending(
    ) {
        let reason =
            |json: &[u8], named: bool| answer_from_json(json, &[], named).usage.cache_miss_reason;
        let absent = br#"{"type":"message","usage":{"input_tokens":1}}"#;
        let null = br#"{"type":"message","usage":{"input_tokens":1},"diagnostics":null}"#;
        let pending = br#"{"type":"message","usage":{"input_tokens":1},"diagnostics":{"cache_miss_reason":null}}"#;
        let typed = br#"{"type":"message","usage":{"input_tokens":1},"diagnostics":{"cache_miss_reason":{"type":"none"}}}"#;
        for named in [true, false] {
            assert_eq!(reason(absent, named), None, "named {named}");
            assert_eq!(
                reason(pending, named).as_deref(),
                Some("pending"),
                "named {named}"
            );
            assert_eq!(
                reason(typed, named).as_deref(),
                Some("none"),
                "a type the upstream names is recorded as sent, named {named}"
            );
        }
        assert_eq!(reason(null, true).as_deref(), Some("none"));
        assert_eq!(
            reason(null, false),
            None,
            "a null member answering a body that named no message says nothing"
        );
        let start = br#"{"type":"message_start","message":{"usage":{"input_tokens":1},"diagnostics":null}}"#;
        assert_eq!(
            answer_from_json(start, &["message"], true)
                .usage
                .cache_miss_reason
                .as_deref(),
            Some("none"),
            "the head carries the member under its message"
        );
    }

    #[test]
    fn the_answers_own_facts_are_read_from_the_head_the_delta_and_a_json_answer() {
        let start = br#"{"type":"message_start","message":{"id":"msg_1","model":"claude-opus-5","usage":{"input_tokens":1,"speed":"fast","inference_geo":"us"}}}"#;
        let head = answer_from_json(start, &["message"], false);
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
        let tail = answer_from_json(delta, &[], false);
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
        let answer = answer_from_json(refused, &[], false);
        assert_eq!(
            (
                answer.response.stop_reason.as_deref(),
                answer.response.refusal_category.as_deref(),
                answer.response.refusal_recommended_model.as_deref()
            ),
            (Some("refusal"), Some("cyber"), Some("claude-opus-5"))
        );
        assert!(answer_from_json(br#"{"type":"message"}"#, &[], false)
            .response
            .is_empty());
    }

    fn dropped(blocks: u64, first_message: u64) -> InputTransformation {
        InputTransformation {
            blocks,
            first_message,
        }
    }

    #[test]
    fn a_dropped_blocks_key_counts_its_entries_and_keeps_their_lowest_message_index() {
        let answer = br#"{"type":"message","usage":{"input_tokens":1},"input_transformations":[{"type":"thinking_dropped","reason":"end_user_binding_mismatch","path":"messages.9"},{"type":"thinking_dropped","reason":"end_user_binding_mismatch","path":"messages.2"},{"type":"thinking_dropped","reason":"prefix_binding_mismatch","path":"messages.4"}]}"#;
        assert_eq!(
            answer_from_json(answer, &[], false)
                .response
                .input_transformations,
            Some(BTreeMap::from([
                (
                    "thinking_dropped.end_user_binding_mismatch".to_string(),
                    dropped(2, 2)
                ),
                (
                    "thinking_dropped.prefix_binding_mismatch".to_string(),
                    dropped(1, 4)
                ),
            ]))
        );
    }

    #[test]
    fn a_word_of_64_bytes_files_as_sent_and_one_of_65_bytes_reads_other() {
        let at_bound = "t".repeat(64);
        let past_bound = "t".repeat(65);
        let answer = format!(
            r#"{{"type":"message","input_transformations":[{{"type":"{at_bound}","reason":"end_user_binding_mismatch","path":"messages.1"}},{{"type":"{past_bound}","reason":"end_user_binding_mismatch","path":"messages.3"}}]}}"#
        );
        assert_eq!(
            answer_from_json(answer.as_bytes(), &[], false)
                .response
                .input_transformations,
            Some(BTreeMap::from([
                (
                    format!("{at_bound}.end_user_binding_mismatch"),
                    dropped(1, 1)
                ),
                ("other.end_user_binding_mismatch".to_string(), dropped(1, 3)),
            ]))
        );
    }

    #[test]
    fn an_entry_without_a_reason_or_a_message_path_is_skipped() {
        let answer = br#"{"type":"message","input_transformations":[{"type":"thinking_dropped","reason":"organization_binding_mismatch","path":"messages.3.content.1"},{"type":"thinking_dropped","path":"messages.1"},{"type":"thinking_dropped","reason":"organization_binding_mismatch","path":"system.0"}]}"#;
        assert_eq!(
            answer_from_json(answer, &[], false)
                .response
                .input_transformations,
            Some(BTreeMap::from([(
                "thinking_dropped.organization_binding_mismatch".to_string(),
                dropped(1, 3)
            )]))
        );
    }

    #[test]
    fn a_deltas_refusal_details_are_read_from_delta_stop_details_not_the_root() {
        let delta = br#"{"type":"message_delta","delta":{"stop_reason":"refusal","stop_sequence":null,"stop_details":{"type":"refusal","category":"cyber","recommended_model":"claude-opus-5"}},"usage":{"output_tokens":3}}"#;
        let answer = answer_from_json(delta, &[], false);
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
        let answer = answer_from_json(root_only, &[], false);
        assert_eq!(answer.response.refusal_category, None);
    }

    #[test]
    fn a_stream_flag_and_a_tool_use_delta_are_recognised() {
        let wants_stream = |body: &[u8]| wants_stream(&phoebis_cch::TopLevelMembers::read(body));
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
            answer_from_json(
                br#"{"type":"message_start","message":{"id":"msg_01","usage":{}}}"#,
                &["message"],
                false
            )
            .message_id
            .as_deref(),
            Some("msg_01")
        );
        assert_eq!(
            answer_from_json(br#"{"id":"msg_02","type":"message"}"#, &[], false)
                .message_id
                .as_deref(),
            Some("msg_02")
        );
        assert_eq!(
            answer_from_json(
                br#"{"type":"message_start","message":{"id":7}}"#,
                &["message"],
                false
            )
            .message_id,
            None,
            "a non-string id is no id"
        );
        assert_eq!(
            answer_from_json(br#"{"type":"message_start"}"#, &["message"], false).message_id,
            None
        );
    }

    #[test]
    fn an_unparseable_document_yields_no_id() {
        assert_eq!(answer_from_json(b"{not json", &[], false).message_id, None);
    }
}
