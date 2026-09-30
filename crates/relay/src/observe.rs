//! What the relay reads out of a request or a response for the bus: the published headers, the stream flag,
//! the usage block and the answer's own facts, and the shape of a content delta. Nothing here changes what
//! is relayed.

use std::collections::BTreeMap;

use axum::http::HeaderMap;
use phoebis_bus::{
    header_published, ClassifierAnswer, ResponseFacts, SafeguardResults, ToolUseVerdict, Usage,
};
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
    let results = match at.get("delta") {
        Some(delta) => delta.get("safeguard_results"),
        None => at.get("safeguard_results"),
    };
    // A whole answer names its tool calls in `content`; a delta's caller notes them from the stream.
    let whole = path.is_empty() && at.get("delta").is_none() && at.get("content").is_some();
    let safeguards = safeguard_results(results).map(|s| {
        if whole {
            s.with_tool_calls(has_tool_use(at.get("content")))
        } else {
            s
        }
    });
    // Every whole JSON answer is read, since nothing here knows which request it answers; the ledger keeps
    // the reading only for a request admission recognised as a classifier request.
    let classifier = whole.then(|| classifier_answer(&at["content"]));
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
        safeguards,
        classifier,
    };
    Answer { usage, response }
}

/// The client's bound on a flagged call's explanation; the client counts UTF-16 units, this counts characters.
const EXPLANATION_MAX_CHARS: usize = 2_000;
/// A per-call `unavailable` entry's reasons the client names; any other folds to `other`.
const CALL_UNAVAILABLE_REASONS: &[&str] =
    &["error", "input_too_long", "refused", "timeout", "truncated"];
/// A status `unavailable`'s reasons the client names; any other folds to `other`.
const STATUS_UNAVAILABLE_REASONS: &[&str] = &["disabled", "error"];

fn folded_reason(reason: Option<&Value>, known: &[&str]) -> String {
    reason
        .and_then(Value::as_str)
        .filter(|reason| known.contains(reason))
        .unwrap_or("other")
        .to_string()
}

/// The server-side check's answer, folded by the client's own schemas: absent or null is no answer; a member
/// that is not an array, answers the check more than once, or carries a status the client does not know is
/// `unrecognized`; one with no `dangerous_tool_use` entry answered nothing (`none`).
pub(crate) fn safeguard_results(member: Option<&Value>) -> Option<SafeguardResults> {
    let status_verdict = |verdict: &str, detail: Option<String>| SafeguardResults {
        verdict: verdict.into(),
        detail,
        tool_uses: BTreeMap::new(),
    };
    let entries = match member? {
        Value::Null => return None,
        Value::Array(entries) => entries,
        _ => return Some(status_verdict("unrecognized", None)),
    };
    let checks: Vec<&Value> = entries
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) == Some("dangerous_tool_use"))
        .collect();
    let check = match checks.as_slice() {
        [] => return Some(status_verdict("none", None)),
        [check] => *check,
        _ => return Some(status_verdict("unrecognized", None)),
    };
    let status = check.get("status");
    let field = |name: &str| status.and_then(|s| s.get(name));
    Some(match field("type").and_then(Value::as_str) {
        Some("available") => match field("tool_uses").and_then(Value::as_object) {
            Some(calls) => verdict_over(
                calls
                    .iter()
                    .map(|(id, entry)| (id.clone(), tool_use_verdict(entry)))
                    .collect(),
            ),
            None => status_verdict("unrecognized", None),
        },
        Some("unsupported") => status_verdict("unsupported", None),
        Some("unavailable") => status_verdict(
            "unavailable",
            Some(folded_reason(field("reason"), STATUS_UNAVAILABLE_REASONS)),
        ),
        _ => status_verdict("unrecognized", None),
    })
}

/// One entry of the `tool_uses` map; an entry the client's schema rejects becomes `unrecognized`, as the
/// client makes it.
fn tool_use_verdict(entry: &Value) -> ToolUseVerdict {
    let verdict =
        |outcome: &str, reason: Option<String>, explanation: Option<String>| ToolUseVerdict {
            outcome: outcome.into(),
            reason,
            explanation,
        };
    match entry.get("type").and_then(Value::as_str) {
        Some("evaluated") => {
            let explanation = match entry.get("explanation") {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(text)) => Ok(Some(text.as_str())),
                Some(_) => Err(()),
            };
            match (entry.get("outcome").and_then(Value::as_str), explanation) {
                // The client reads the explanation of a flagged call alone.
                (Some("flagged"), Ok(explanation)) => verdict(
                    "flagged",
                    None,
                    explanation
                        .filter(|text| !text.is_empty())
                        .map(|text| text.chars().take(EXPLANATION_MAX_CHARS).collect()),
                ),
                (Some("not_flagged"), Ok(_)) => verdict("not_flagged", None, None),
                _ => verdict("unrecognized", None, None),
            }
        }
        Some("skipped") => verdict("skipped", None, None),
        Some("unavailable") => verdict(
            "unavailable",
            Some(folded_reason(entry.get("reason"), CALL_UNAVAILABLE_REASONS)),
            None,
        ),
        _ => verdict("unrecognized", None, None),
    }
}

/// The answer's one verdict over its calls, the one that asks the most of the reader first: a flag, then an
/// entry nobody can read, then a call the check could not judge, then one it chose not to.
fn verdict_over(tool_uses: BTreeMap<String, ToolUseVerdict>) -> SafeguardResults {
    let first = |outcome: &str| tool_uses.values().find(|call| call.outcome == outcome);
    let (verdict, detail) = if first("flagged").is_some() {
        ("flagged", None)
    } else if first("unrecognized").is_some() {
        ("unrecognized", None)
    } else if let Some(call) = first("unavailable") {
        ("unavailable", call.reason.clone())
    } else if first("skipped").is_some() {
        ("skipped", None)
    } else {
        ("not_flagged", None)
    };
    SafeguardResults {
        verdict: verdict.into(),
        detail,
        tool_uses,
    }
}

/// A whole answer's text read by the client's own parse: `<block>yes` or `<block>no`, the word ending
/// at a regex word boundary, in any ASCII case. Both answers anywhere in the text, the thinking included, is
/// no answer; otherwise the one answer outside every `<thinking>` span.
pub(crate) fn classifier_answer(content: &Value) -> ClassifierAnswer {
    let text: String = content
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    let answer = without_thinking(&text);
    let verdict = match (block_answers(&text), block_answers(&answer)) {
        ((true, true), _) => "unreadable",
        (_, (true, false)) => "flagged",
        (_, (false, true)) => "not_flagged",
        _ => "unreadable",
    };
    ClassifierAnswer {
        verdict: verdict.into(),
        category: single_category(&answer),
    }
}

/// Whether the text carries a `<block>yes` and a `<block>no`, as `/<block>(yes|no)\b/gi` matches them.
fn block_answers(text: &str) -> (bool, bool) {
    const TAG: &[u8] = b"<block>";
    let bytes = text.as_bytes();
    let (mut yes, mut no) = (false, false);
    let mut at = 0;
    while at + TAG.len() <= bytes.len() {
        if !bytes[at..at + TAG.len()].eq_ignore_ascii_case(TAG) {
            at += 1;
            continue;
        }
        at += TAG.len();
        let rest = &bytes[at..];
        let word = |w: &[u8]| {
            rest.len() >= w.len()
                && rest[..w.len()].eq_ignore_ascii_case(w)
                && rest
                    .get(w.len())
                    .is_none_or(|b| !(b.is_ascii_alphanumeric() || *b == b'_'))
        };
        yes |= word(b"yes");
        no |= word(b"no");
    }
    (yes, no)
}

/// The text with every closed `<thinking>...</thinking>` span removed, as the client removes them.
fn without_thinking(text: &str) -> String {
    const OPEN: &str = "<thinking>";
    const CLOSE: &str = "</thinking>";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        let Some(len) = rest[start + OPEN.len()..].find(CLOSE) else {
            break;
        };
        out.push_str(&rest[..start]);
        rest = &rest[start + OPEN.len() + len + CLOSE.len()..];
    }
    out.push_str(rest);
    out
}

/// The content of the one `<category>...</category>` the text carries; two name none.
fn single_category(text: &str) -> Option<String> {
    let mut found = text.match_indices("<category>").filter_map(|(at, open)| {
        let body = &text[at + open.len()..];
        body.find("</category>").map(|end| body[..end].trim())
    });
    let first = found.next()?;
    if found.next().is_some() {
        return None;
    }
    (!first.is_empty()).then(|| first.to_string())
}

/// Whether a whole answer's `content` calls a tool.
pub(crate) fn has_tool_use(content: Option<&Value>) -> bool {
    content.and_then(Value::as_array).is_some_and(|blocks| {
        blocks
            .iter()
            .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
    })
}

/// A `content_block_start` that opens a `tool_use` block.
pub(crate) fn is_tool_use_block_start(data: &[u8]) -> bool {
    serde_json::from_slice::<Value>(data).is_ok_and(|event| {
        event
            .get("content_block")
            .and_then(|block| block.get("type"))
            .and_then(Value::as_str)
            == Some("tool_use")
    })
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

    const DELTA_NOT_FLAGGED: &str = r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null,"safeguard_results":[{"type":"dangerous_tool_use","status":{"type":"available","tool_uses":{"toolu_01":{"type":"evaluated","outcome":"not_flagged"}}}}]},"usage":{"output_tokens":52}}"#;
    const DELTA_END_TURN: &str = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null,"safeguard_results":[{"type":"dangerous_tool_use","status":{"type":"available","tool_uses":{}}}]},"usage":{"output_tokens":12}}"#;
    const DELTA_FLAGGED: &str = r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null,"safeguard_results":[{"type":"dangerous_tool_use","status":{"type":"available","tool_uses":{"toolu_01":{"type":"evaluated","outcome":"not_flagged"},"toolu_02":{"type":"evaluated","outcome":"flagged","explanation":"Deletes a file the task never named."}}}}]},"usage":{"output_tokens":60}}"#;

    fn checked(line: &str) -> Option<SafeguardResults> {
        answer_from_json(line.as_bytes(), &[]).response.safeguards
    }

    /// A `message_delta` whose `safeguard_results` is `results`, verbatim.
    fn delta_with(results: &str) -> String {
        format!(
            r#"{{"type":"message_delta","delta":{{"stop_reason":"tool_use","stop_sequence":null,"safeguard_results":{results}}},"usage":{{"output_tokens":5}}}}"#
        )
    }

    /// One `dangerous_tool_use` entry whose status is `status`, verbatim.
    fn with_status(status: &str) -> String {
        delta_with(&format!(
            r#"[{{"type":"dangerous_tool_use","status":{status}}}]"#
        ))
    }

    /// An available check whose `tool_uses` map is `entries`, verbatim.
    fn available(entries: &str) -> String {
        with_status(&format!(
            r#"{{"type":"available","tool_uses":{{{entries}}}}}"#
        ))
    }

    fn entry(outcome: &str, reason: Option<&str>, explanation: Option<&str>) -> ToolUseVerdict {
        ToolUseVerdict {
            outcome: outcome.into(),
            reason: reason.map(String::from),
            explanation: explanation.map(String::from),
        }
    }

    #[test]
    fn a_deltas_safeguard_results_fold_to_one_verdict_and_one_outcome_per_tool_use() {
        let passed = checked(DELTA_NOT_FLAGGED).expect("the delta carries the member");
        assert_eq!(
            (passed.verdict.as_str(), passed.detail),
            ("not_flagged", None)
        );
        assert_eq!(
            passed.tool_uses.into_iter().collect::<Vec<_>>(),
            vec![("toolu_01".to_string(), entry("not_flagged", None, None))]
        );
        let flagged = checked(DELTA_FLAGGED).unwrap();
        assert_eq!(
            flagged.verdict, "flagged",
            "one flagged call flags the answer"
        );
        assert_eq!(
            flagged.tool_uses.into_iter().collect::<Vec<_>>(),
            vec![
                ("toolu_01".to_string(), entry("not_flagged", None, None)),
                (
                    "toolu_02".to_string(),
                    entry(
                        "flagged",
                        None,
                        Some("Deletes a file the task never named.")
                    )
                ),
            ]
        );
        let long = "x".repeat(3_000);
        let cut = checked(&available(&format!(
            r#""toolu_01":{{"type":"evaluated","outcome":"flagged","explanation":"{long}"}}"#
        )))
        .unwrap();
        assert_eq!(
            cut.tool_uses["toolu_01"]
                .explanation
                .as_deref()
                .map(str::len),
            Some(2_000),
            "the explanation is cut where the client cuts it"
        );
        let skipped = checked(&available(r#""toolu_01":{"type":"skipped"}"#)).unwrap();
        assert_eq!(
            (skipped.verdict.as_str(), &skipped.tool_uses["toolu_01"]),
            ("skipped", &entry("skipped", None, None))
        );
    }

    #[test]
    fn an_end_turn_answer_evaluated_nothing_and_reads_none_only_beside_a_tool_call() {
        let quiet = checked(DELTA_END_TURN).unwrap();
        assert_eq!(quiet.verdict, "not_flagged");
        assert!(quiet.tool_uses.is_empty());
        assert_eq!(
            quiet.clone().with_tool_calls(false).verdict,
            "not_flagged",
            "an answer that called nothing had nothing to judge"
        );
        assert_eq!(
            quiet.with_tool_calls(true).verdict,
            "none",
            "an answer that called a tool and judged none of them gave no verdict"
        );
        let judged = checked(DELTA_NOT_FLAGGED).unwrap().with_tool_calls(true);
        assert_eq!(
            judged.verdict, "not_flagged",
            "a judged call keeps its verdict"
        );
    }

    #[test]
    fn a_per_call_unavailable_entry_keeps_its_reason_and_names_the_answers_detail() {
        for reason in ["timeout", "error", "refused", "input_too_long", "truncated"] {
            let answer = checked(&available(&format!(
                r#""toolu_01":{{"type":"unavailable","reason":"{reason}"}}"#
            )))
            .unwrap();
            assert_eq!(
                (answer.verdict.as_str(), answer.detail.as_deref()),
                ("unavailable", Some(reason))
            );
            assert_eq!(
                answer.tool_uses["toolu_01"],
                entry("unavailable", Some(reason), None)
            );
        }
        let unknown = checked(&available(
            r#""toolu_01":{"type":"unavailable","reason":"overloaded"}"#,
        ))
        .unwrap();
        assert_eq!(
            (unknown.verdict.as_str(), unknown.detail.as_deref()),
            ("unavailable", Some("other"))
        );
        assert_eq!(
            unknown.tool_uses["toolu_01"],
            entry("unavailable", Some("other"), None)
        );
    }

    #[test]
    fn a_status_verdict_carries_no_tool_uses() {
        let unsupported = checked(&with_status(r#"{"type":"unsupported"}"#)).unwrap();
        assert_eq!(
            (unsupported.verdict.as_str(), unsupported.detail),
            ("unsupported", None)
        );
        assert!(unsupported.tool_uses.is_empty());
        for (reason, detail) in [
            ("disabled", "disabled"),
            ("error", "error"),
            ("maintenance", "other"),
        ] {
            let answer = checked(&with_status(&format!(
                r#"{{"type":"unavailable","reason":"{reason}"}}"#
            )))
            .unwrap();
            assert_eq!(
                (answer.verdict.as_str(), answer.detail.as_deref()),
                ("unavailable", Some(detail))
            );
            assert!(answer.tool_uses.is_empty());
        }
        let odd = checked(&with_status(r#"{"type":"pending"}"#)).unwrap();
        assert_eq!(
            odd.verdict, "unrecognized",
            "a status the client does not know"
        );
    }

    #[test]
    fn a_member_the_clients_schemas_reject_reads_unrecognized_and_one_with_no_check_reads_none() {
        let twice = checked(&delta_with(
            r#"[{"type":"dangerous_tool_use","status":{"type":"unsupported"}},{"type":"dangerous_tool_use","status":{"type":"unsupported"}}]"#,
        ))
        .unwrap();
        assert_eq!(
            twice.verdict, "unrecognized",
            "two checks answer for one request"
        );
        let other = checked(&delta_with(r#"[{"type":"some_other_check","status":{}}]"#)).unwrap();
        assert_eq!(
            (other.verdict.as_str(), other.tool_uses.len()),
            ("none", 0),
            "the member answered no check this request asked for"
        );
        let scalar = checked(&delta_with(r#"{"type":"dangerous_tool_use"}"#)).unwrap();
        assert_eq!(scalar.verdict, "unrecognized", "the member is not an array");
        let odd = checked(&available(
            r#""toolu_01":{"type":"evaluated","outcome":"not_flagged"},"toolu_02":{"type":"deferred"}"#,
        ))
        .unwrap();
        assert_eq!(odd.verdict, "unrecognized");
        assert_eq!(
            odd.tool_uses["toolu_02"],
            entry("unrecognized", None, None),
            "an entry of a type the client does not know"
        );
        assert_eq!(odd.tool_uses["toolu_01"], entry("not_flagged", None, None));
        assert_eq!(
            checked(&delta_with("null")),
            None,
            "a null member is no answer"
        );
        assert_eq!(
            checked(r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#),
            None,
            "an absent member is no answer"
        );
    }

    #[test]
    fn a_root_member_beside_a_delta_is_not_read_and_a_whole_answers_root_member_is() {
        let leaked = r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"safeguard_results":[{"type":"dangerous_tool_use","status":{"type":"unsupported"}}]}"#;
        assert_eq!(checked(leaked), None);
        let whole = r#"{"id":"msg_01","type":"message","role":"assistant","model":"claude-sonnet-5","content":[{"type":"tool_use","id":"toolu_01","name":"Bash","input":{}}],"stop_reason":"tool_use","safeguard_results":[{"type":"dangerous_tool_use","status":{"type":"available","tool_uses":{}}}],"usage":{"output_tokens":9}}"#;
        let answer = checked(whole).expect("a whole answer carries the member at its root");
        assert_eq!(
            answer.verdict, "none",
            "the answer called a tool and judged none of its calls"
        );
    }

    fn classified(text_blocks: &[&str]) -> ClassifierAnswer {
        let content: Vec<Value> = text_blocks
            .iter()
            .map(|t| serde_json::json!({ "type": "text", "text": t }))
            .collect();
        let whole = serde_json::json!({
            "id": "msg_01", "type": "message", "role": "assistant", "model": "claude-sonnet-5-5",
            "content": content, "stop_reason": "end_turn", "usage": { "output_tokens": 4 },
        });
        answer_from_json(&serde_json::to_vec(&whole).unwrap(), &[])
            .response
            .classifier
            .expect("a whole JSON answer is read as a classifier answer")
    }

    fn verdict_of(text_blocks: &[&str]) -> String {
        classified(text_blocks).verdict
    }

    #[test]
    fn a_whole_answers_text_is_read_by_the_clients_own_block_parse() {
        assert_eq!(
            verdict_of(&["<block>no"]),
            "not_flagged",
            "stage 1 stops before the closing tag"
        );
        assert_eq!(verdict_of(&["<block>no</block>"]), "not_flagged");
        assert_eq!(verdict_of(&["<block>yes</block>"]), "flagged");
        assert_eq!(
            verdict_of(&["<BLOCK>Yes</block>"]),
            "flagged",
            "the tag and the word in any case"
        );
        assert_eq!(
            verdict_of(&["<thinking>maybe <block>yes</thinking><block>no</block>"]),
            "unreadable",
            "both answers anywhere, the thinking included, is no answer"
        );
        let staged = classified(&[
            "<thinking>reasoning</thinking><block>no</block><reason>r</reason><category>data_exfiltration</category>",
        ]);
        assert_eq!(
            (staged.verdict.as_str(), staged.category.as_deref()),
            ("not_flagged", Some("data_exfiltration"))
        );
        assert_eq!(
            verdict_of(&[]),
            "unreadable",
            "an empty content says nothing"
        );
        assert_eq!(verdict_of(&["I cannot tell."]), "unreadable");
        assert_eq!(
            verdict_of(&["<block>nothing</block>"]),
            "unreadable",
            "the word ends at a boundary"
        );
        assert_eq!(
            verdict_of(&["<thinking>considering</thinking>", "<block>yes</block>"]),
            "flagged",
            "the text blocks read as one"
        );
        assert_eq!(
            classified(&["<block>yes</block><category>a</category><category>b</category>"])
                .category,
            None,
            "two categories name none"
        );
    }

    #[test]
    fn a_message_start_and_a_delta_read_no_classifier_answer() {
        let start = br#"{"type":"message_start","message":{"id":"msg_1","type":"message","content":[],"usage":{"input_tokens":1}}}"#;
        assert_eq!(
            answer_from_json(start, &["message"]).response.classifier,
            None
        );
        assert_eq!(
            answer_from_json(DELTA_NOT_FLAGGED.as_bytes(), &[])
                .response
                .classifier,
            None
        );
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
