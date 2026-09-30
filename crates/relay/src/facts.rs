//! What the restoration reads off the body in one parse: the turn boundary, the version suffix, and the
//! prompt id a first-party line already carries.

use axum::http::HeaderMap;
use phoebis_bus::RequestShape;
use serde_json::Value;

use crate::chains::Chains;

/// The opening of the client's own classifier instruction, the one marker both stages share.
pub const SECURITY_MONITOR_PREFIX: &str =
    "You are a security monitor for autonomous AI coding agents.";

pub struct BodyFacts {
    /// The last `messages` entry has role `user` and carries no `tool_result` block.
    pub fresh_user_turn: bool,
    /// UTF-16 code units 4, 7, and 20 of the user's own prompt (the last `text` block of the first `user`
    /// message; the system-reminder blocks precede it), matching the client's own JavaScript string indexing;
    /// `b'0' as u16` where the text is shorter, for the version suffix.
    pub prompt_units: [u16; 3],
    /// The `cc_prompt_id` the caller's own attribution line carries, absent from a reduced-shape request and
    /// from a value spelled any other way than the client spells one.
    pub prompt_id: Option<String>,
    /// The `cc_turn_origin` the line carries, in the client's own spelling (a token of lowercase ASCII letters
    /// and underscores, at most 32 bytes); absent from a reduced-shape request.
    pub turn_origin: Option<String>,
    /// Whether `system[0]` is an attribution line at all, stamped or reduced: what the restoration fills.
    pub carries_line: bool,
    pub shape: RequestShape,
    /// `None` for a body that is not a JSON object; the same parse `shape` and the rest above read.
    pub chains: Option<Chains>,
    /// The largest `cache_control.ttl` at the API's marker positions, `None` without a marker there.
    pub markers_ttl_ms: Option<i64>,
}

/// The gateway hint headers, each optional, read the way `identify` reads an identifier: bounded and
/// charset-checked, a malformed value absent rather than refused.
pub struct ClientHints {
    pub agent_type: Option<String>,
    pub compaction: Option<String>,
    pub context_compacted: Option<String>,
    /// `<name>=<ms>;...` summed; absent when no entry parses or the value runs past 4 KiB.
    pub prev_tool_ms: Option<i64>,
    /// `x-claude-code-request-class`.
    pub request_class: Option<String>,
}

impl BodyFacts {
    pub fn read(body: &[u8]) -> BodyFacts {
        // Counted over decoded keys on the walk the cut runs on, not read from the parse below: a body a strict
        // decoder refuses (a lone surrogate escape in a string value) still has its member cut under an off
        // setting, and the request row has to say so.
        let safeguards = phoebis_cch::top_level_member_count(body, "safeguards").is_ok_and(|n| n >= 1);
        let Ok(root) = serde_json::from_slice::<Value>(body) else {
            return BodyFacts {
                fresh_user_turn: false,
                prompt_units: [b'0' as u16; 3],
                prompt_id: None,
                turn_origin: None,
                carries_line: false,
                shape: RequestShape {
                    safeguards,
                    ..RequestShape::default()
                },
                chains: None,
                markers_ttl_ms: None,
            };
        };
        let messages = root.get("messages").and_then(Value::as_array);
        let fresh_user_turn = messages
            .and_then(|m| m.last())
            .is_some_and(is_fresh_user_turn);
        let prompt_units = messages
            .and_then(|m| {
                m.iter()
                    .find(|msg| msg.get("role").and_then(Value::as_str) == Some("user"))
            })
            .and_then(last_text)
            .map(prompt_units_of)
            .unwrap_or([b'0' as u16; 3]);
        // Read once and shared by every field the line may carry, so a change to one field's spelling rule
        // never has to re-walk the body to find the text the others already have in hand.
        let first_system_text = root
            .get("system")
            .and_then(Value::as_array)
            .and_then(|blocks| blocks.first())
            .and_then(|block| block.get("text"))
            .and_then(Value::as_str);
        let carries_line =
            first_system_text.is_some_and(|text| text.starts_with("x-anthropic-billing-header: "));
        let prompt_id = first_system_text
            .and_then(carried_prompt_id)
            .map(String::from);
        let turn_origin = first_system_text
            .and_then(carried_turn_origin)
            .map(String::from);
        let count = |v: Option<&Value>| v.and_then(Value::as_array).map_or(0, |a| a.len() as u32);
        let text = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_string);
        let thinking = root.get("thinking");
        // The client's own classifier request, by its body alone: the classifier beta and the `auxiliary` class
        // header appear in first-party mode only. Stage 1 stops at a closing tag; stage 2 runs it to the end.
        let monitor = root
            .get("system")
            .and_then(Value::as_array)
            .is_some_and(|blocks| {
                blocks.iter().take(2).any(|block| {
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| text.starts_with(SECURITY_MONITOR_PREFIX))
                })
            });
        let classifier_stage = (monitor
            && root.get("stream").is_none()
            && root.get("tools").is_none()
            && root.get("max_tokens").is_some())
        .then(|| match root.get("stop_sequences") {
            None => Some(2),
            Some(Value::Array(stops))
                if stops
                    .iter()
                    .any(|s| matches!(s.as_str(), Some("</block>" | "</severity>"))) =>
            {
                Some(1)
            }
            Some(_) => None,
        })
        .flatten();
        let shape = RequestShape {
            effort: text(root.get("output_config").and_then(|o| o.get("effort"))),
            thinking_type: text(thinking.and_then(|t| t.get("type"))),
            thinking_budget: thinking
                .and_then(|t| t.get("budget_tokens"))
                .and_then(Value::as_u64),
            max_tokens: root.get("max_tokens").and_then(Value::as_u64),
            tool_count: count(root.get("tools")),
            // A string `system` is one block; the client sends an array.
            system_blocks: match root.get("system") {
                Some(Value::String(_)) => 1,
                other => count(other),
            },
            message_count: count(root.get("messages")),
            context_edits: root
                .get("context_management")
                .and_then(|c| c.get("edits"))
                .and_then(Value::as_array)
                .map(|edits| edits.iter().filter_map(|e| text(e.get("type"))).collect())
                .unwrap_or_default(),
            temperature: root.get("temperature").and_then(Value::as_f64),
            speed: text(root.get("speed")),
            service_tier: text(root.get("service_tier")),
            safeguards,
            classifier_stage,
        };
        let chains = Chains::read(&root);
        let markers_ttl_ms = markers_ttl_ms(&root);
        BodyFacts {
            fresh_user_turn,
            prompt_units,
            prompt_id,
            turn_origin,
            carries_line,
            shape,
            chains,
            markers_ttl_ms,
        }
    }
}

/// A marker without a `ttl` is the upstream's five-minute default; `1h` is the one other value it takes.
pub(crate) fn marker_ttl_ms(marker: &Value) -> i64 {
    match marker.get("ttl").and_then(Value::as_str) {
        Some("1h") => 3_600_000,
        _ => 300_000,
    }
}

/// The largest TTL of a `cache_control` object at the positions the TTL editor scans: the tools, the system
/// blocks, the messages' blocks and a top-level marker each write an entry the pin's cache lives by.
fn markers_ttl_ms(root: &Value) -> Option<i64> {
    crate::body::schema_cache_controls(root.as_object()?)
        .into_iter()
        .filter(|marker| marker.is_object())
        .map(marker_ttl_ms)
        .max()
}

impl ClientHints {
    pub fn read(headers: &HeaderMap) -> ClientHints {
        ClientHints {
            agent_type: crate::identity::header_id(headers, "x-claude-code-agent-type"),
            compaction: crate::identity::header_id(headers, "x-claude-code-compaction"),
            context_compacted: crate::identity::header_id(
                headers,
                "x-claude-code-context-compacted",
            ),
            prev_tool_ms: headers
                .get("x-claude-code-prev-tool-durations")
                .and_then(|v| v.to_str().ok())
                .and_then(tool_ms_sum),
            request_class: crate::identity::header_id(headers, "x-claude-code-request-class"),
        }
    }
}

/// Bound past which the header is not read at all: the sum is what the restoration surface reports, never the
/// per-tool breakdown, so there is nothing this build needs from a value a caller could pad without limit.
const PREV_TOOL_DURATIONS_MAX_BYTES: usize = 4_096;

/// `<name>=<ms>` entries separated by `;`, each split at its last `=`; an entry that does not parse is skipped
/// rather than refusing the rest, so one caller-side glitch does not blank the whole sum.
fn tool_ms_sum(value: &str) -> Option<i64> {
    if value.len() > PREV_TOOL_DURATIONS_MAX_BYTES {
        return None;
    }
    let mut sum: i64 = 0;
    let mut parsed = false;
    for entry in value.split(';') {
        if entry.is_empty() {
            continue;
        }
        let Some((_, ms)) = entry.rsplit_once('=') else {
            continue;
        };
        let Ok(ms) = ms.parse::<u64>() else {
            continue;
        };
        sum = sum.saturating_add(i64::try_from(ms).unwrap_or(i64::MAX));
        parsed = true;
    }
    parsed.then_some(sum)
}

/// The value of `<name>=` on the attribution line at `system[0]`, whatever its spelling; the caller applies the
/// charset and bound its own field carries, since a separator let through by the wrong rule would become a
/// field of its own once the value is restored onto a line this build later signs.
fn carried_field<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let prefix = format!("{name}=");
    let value = text
        .strip_prefix("x-anthropic-billing-header: ")?
        .split("; ")
        .find_map(|field| field.strip_prefix(prefix.as_str()))?
        .trim_end_matches(';');
    Some(value)
}

/// What a carried id may weigh. A UUID is 36; the session memory retains one per entry for a day, so a value
/// past this is read as absent rather than held.
const PROMPT_ID_MAX: usize = 64;

/// The `cc_prompt_id` of an attribution line, accepted only as the client's own spelling of an id: the value
/// is recorded and then restored onto a later turn of the same session, in a line this build signs, so a
/// separator smuggled into it would become a field of that line.
fn carried_prompt_id(text: &str) -> Option<&str> {
    let value = carried_field(text, "cc_prompt_id")?;
    let spelled = (1..=PROMPT_ID_MAX).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-');
    spelled.then_some(value)
}

/// Bound on `cc_turn_origin`: the client's own token set, never wide enough to carry a separator that could
/// smuggle a field of its own into a line this build later signs.
const TURN_ORIGIN_MAX: usize = 32;

/// The `cc_turn_origin` of an attribution line, accepted only as the client's own spelling: lowercase ASCII
/// letters and underscores, the shape `human` and its siblings take.
fn carried_turn_origin(text: &str) -> Option<&str> {
    let value = carried_field(text, "cc_turn_origin")?;
    let spelled = (1..=TURN_ORIGIN_MAX).contains(&value.len())
        && value.bytes().all(|b| matches!(b, b'a'..=b'z' | b'_'));
    spelled.then_some(value)
}

fn is_fresh_user_turn(message: &Value) -> bool {
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    match message.get("content") {
        Some(Value::String(_)) => true,
        Some(Value::Array(blocks)) => !blocks
            .iter()
            .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result")),
        _ => false,
    }
}

/// The last `text` block of a message: a string content is the text itself, an array is scanned back to front
/// since the system-reminder blocks a client prepends precede the user's own words.
fn last_text(message: &Value) -> Option<&str> {
    match message.get("content") {
        Some(Value::String(s)) => Some(s.as_str()),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .rev()
            .find(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .and_then(|b| b.get("text"))
            .and_then(Value::as_str),
        _ => None,
    }
}

/// UTF-16 code units, not Unicode scalar values: the client indexes its prompt string as JavaScript does, so a
/// supplementary-plane character ahead of a sampled position has to shift the same way here or the suffix the
/// two sides compute diverges on a legitimate prompt.
fn prompt_units_of(text: &str) -> [u16; 3] {
    let units: Vec<u16> = text.encode_utf16().collect();
    let at = |i: usize| units.get(i).copied().unwrap_or(b'0' as u16);
    [at(4), at(7), at(20)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_string_content_last_message_is_a_fresh_turn() {
        let body = br#"{"messages":[{"role":"user","content":"hi"}]}"#;
        assert!(BodyFacts::read(body).fresh_user_turn);
    }

    #[test]
    fn a_tool_result_block_in_the_last_message_is_not_a_fresh_turn() {
        let body = br#"{"messages":[
            {"role":"user","content":"hi"},
            {"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"x","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}
        ]}"#;
        assert!(!BodyFacts::read(body).fresh_user_turn);
    }

    #[test]
    fn an_assistant_last_message_is_not_a_fresh_turn() {
        let body = br#"{"messages":[
            {"role":"user","content":"hi"},
            {"role":"assistant","content":"hello"}
        ]}"#;
        assert!(!BodyFacts::read(body).fresh_user_turn);
    }

    #[test]
    fn the_prompt_units_of_a_short_prompt_pad_with_zero() {
        let body = br#"{"messages":[{"role":"user","content":"hi"}]}"#;
        assert_eq!(BodyFacts::read(body).prompt_units, [b'0' as u16; 3]);
    }

    #[test]
    fn the_last_text_block_of_the_first_user_message_is_read_past_system_reminders() {
        let body = br#"{"messages":[{"role":"user","content":[
            {"type":"text","text":"<system-reminder>ignored</system-reminder>"},
            {"type":"text","text":"01234567890123456789X1"}
        ]}]}"#;
        assert_eq!(
            BodyFacts::read(body).prompt_units,
            [b'4' as u16, b'7' as u16, b'X' as u16]
        );
    }

    #[test]
    fn a_supplementary_plane_character_shifts_the_utf16_sample_the_way_the_client_shifts_it() {
        // The character before position 4 is one Unicode scalar value but two UTF-16 code units, so the
        // client's sample (units 4, 7, 20) and a scalar-value sample (chars 4, 7, 20) read different letters.
        let text = "a\u{1D11E}bcdefghijklmnopqrstuvwxyz";
        let body = format!(r#"{{"messages":[{{"role":"user","content":"{text}"}}]}}"#);
        assert_eq!(
            BodyFacts::read(body.as_bytes()).prompt_units,
            [b'c' as u16, b'f' as u16, b's' as u16]
        );
    }

    #[test]
    fn an_unparseable_body_reads_as_not_a_fresh_turn_with_zeroed_units() {
        let facts = BodyFacts::read(b"not json");
        assert!(!facts.fresh_user_turn);
        assert_eq!(facts.prompt_units, [b'0' as u16; 3]);
    }

    #[test]
    fn cc_turn_origin_is_read_off_the_line_in_the_clients_own_spelling() {
        let line = |origin: &str| {
            format!(
                r#"{{"system":[{{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=cli; cc_turn_origin={origin};"}}]}}"#
            )
        };
        assert_eq!(
            BodyFacts::read(line("human").as_bytes())
                .turn_origin
                .as_deref(),
            Some("human")
        );
        assert_eq!(
            BodyFacts::read(line("Hum an").as_bytes()).turn_origin,
            None,
            "a value outside the token charset is absent"
        );
        let over_bound = "a".repeat(33);
        assert_eq!(
            BodyFacts::read(line(&over_bound).as_bytes()).turn_origin,
            None,
            "past the 32-byte bound the value is absent"
        );
        let reduced = br#"{"system":[{"type":"text","text":"be helpful"}]}"#;
        assert_eq!(BodyFacts::read(reduced).turn_origin, None);
    }

    #[test]
    fn carries_line_is_true_for_an_attribution_line_and_false_otherwise() {
        assert!(
            BodyFacts::read(phoebis_profile::fixtures::body(
                phoebis_profile::fixtures::BodyFixture::Oauth
            ))
            .carries_line,
            "the reduced shape still carries the block, unstamped"
        );
        let plain_first_block = br#"{"system":[{"type":"text","text":"be helpful"}]}"#;
        assert!(!BodyFacts::read(plain_first_block).carries_line);
        let string_system = br#"{"system":"be helpful"}"#;
        assert!(!BodyFacts::read(string_system).carries_line);
    }

    #[test]
    fn the_hint_headers_read_when_sent_and_absent_otherwise() {
        let mut headers = HeaderMap::new();
        headers.insert("x-claude-code-agent-type", "Explore".parse().unwrap());
        headers.insert("x-claude-code-compaction", "auto".parse().unwrap());
        headers.insert("x-claude-code-context-compacted", "manual".parse().unwrap());
        headers.insert(
            "x-claude-code-prev-tool-durations",
            "Bash=1200;Read=35;".parse().unwrap(),
        );
        let hints = ClientHints::read(&headers);
        assert_eq!(
            (
                hints.agent_type.as_deref(),
                hints.compaction.as_deref(),
                hints.context_compacted.as_deref(),
                hints.prev_tool_ms
            ),
            (Some("Explore"), Some("auto"), Some("manual"), Some(1_235))
        );
        let mut unparseable_entry = HeaderMap::new();
        unparseable_entry.insert(
            "x-claude-code-prev-tool-durations",
            "Bash=x;".parse().unwrap(),
        );
        assert_eq!(ClientHints::read(&unparseable_entry).prev_tool_ms, None);
        let mut empty_value = HeaderMap::new();
        empty_value.insert("x-claude-code-prev-tool-durations", "".parse().unwrap());
        assert_eq!(ClientHints::read(&empty_value).prev_tool_ms, None);
        let hints = ClientHints::read(&HeaderMap::new());
        assert_eq!(
            (
                hints.agent_type,
                hints.compaction,
                hints.context_compacted,
                hints.prev_tool_ms
            ),
            (None, None, None, None)
        );
    }

    #[test]
    fn a_cache_control_object_outside_the_apis_marker_positions_is_no_marker() {
        let ttl = |body: &str| BodyFacts::read(body.as_bytes()).markers_ttl_ms;
        assert_eq!(
            ttl(
                r#"{"tools":[{"name":"t","input_schema":{"type":"object","properties":{"cache_control":{"type":"object"}}}}],"messages":[{"role":"user","content":"hi"}]}"#
            ),
            None,
            "a tool's input schema may name a property cache_control"
        );
        assert_eq!(
            ttl(
                r#"{"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"x","input":{"cache_control":{"type":"ephemeral"}}}]}]}"#
            ),
            None,
            "so may a tool call's input"
        );
        assert_eq!(
            ttl(
                r#"{"messages":[{"role":"user","content":[{"type":"text","text":"x","cache_control":{"type":"ephemeral"}}]}]}"#
            ),
            Some(300_000),
            "a content block's own marker is one"
        );
    }

    #[test]
    fn the_markers_ttl_is_the_largest_marker_at_the_apis_positions_and_absent_without_one() {
        let ttl = |body: &str| BodyFacts::read(body.as_bytes()).markers_ttl_ms;
        assert_eq!(
            ttl(r#"{"messages":[{"role":"user","content":"hi"}]}"#),
            None
        );
        assert_eq!(
            ttl(
                r#"{"system":[{"type":"text","text":"a","cache_control":{"type":"ephemeral"}}],"messages":[]}"#
            ),
            Some(300_000),
            "a marker without a ttl is the five-minute default"
        );
        assert_eq!(
            ttl(
                r#"{"tools":[{"name":"t","cache_control":{"type":"ephemeral","ttl":"5m"}}],"messages":[{"role":"user","content":[{"type":"text","text":"x","cache_control":{"type":"ephemeral","ttl":"1h"}}]}]}"#
            ),
            Some(3_600_000),
            "the largest wins wherever it sits"
        );
        assert_eq!(
            ttl(r#"{"cache_control":{"type":"ephemeral"},"messages":[]}"#),
            Some(300_000),
            "a top-level marker counts"
        );
        assert_eq!(
            BodyFacts::read(phoebis_profile::fixtures::body(
                phoebis_profile::fixtures::BodyFixture::Oauth
            ))
            .markers_ttl_ms,
            Some(3_600_000)
        );
        assert_eq!(BodyFacts::read(b"not json").markers_ttl_ms, None);
    }

    #[test]
    fn the_shape_is_read_from_the_recorded_first_party_body() {
        let facts = BodyFacts::read(phoebis_profile::fixtures::body(
            phoebis_profile::fixtures::BodyFixture::FirstParty9b244,
        ));
        let shape = facts.shape;
        assert_eq!(
            (
                shape.thinking_type.as_deref(),
                shape.thinking_budget,
                shape.max_tokens
            ),
            (Some("enabled"), Some(31_999), Some(32_000))
        );
        assert_eq!(
            (shape.tool_count, shape.system_blocks, shape.message_count),
            (12, 4, 1)
        );
        assert_eq!(
            shape.context_edits,
            vec!["clear_thinking_20251015".to_string()]
        );
        assert_eq!(
            (
                shape.effort,
                shape.temperature,
                shape.speed,
                shape.service_tier
            ),
            (None, None, None, None),
            "the 2.1.269 client sends none of these"
        );
    }

    /// A classifier request as the client builds one: the billing line, the monitor instruction, the session's
    /// context, one user turn, and no `stream` or `tools`.
    fn classifier_body(billing: Option<&str>, max_tokens: u64, stops: Option<&[&str]>) -> Value {
        let mut system = Vec::new();
        if let Some(line) = billing {
            system.push(serde_json::json!({ "type": "text", "text": line }));
        }
        system.push(serde_json::json!({
            "type": "text",
            "text": format!("{SECURITY_MONITOR_PREFIX}\n\n## Context\n\nThe agent you are monitoring is an autonomous coding agent."),
            "cache_control": { "type": "ephemeral", "ttl": "1h" },
        }));
        system.push(
            serde_json::json!({ "type": "text", "text": "\n\n## Session Context\n\n<context>" }),
        );
        let mut body = serde_json::json!({
            "model": "claude-sonnet-5-5",
            "max_tokens": max_tokens,
            "system": system,
            "messages": [{ "role": "user", "content": [{ "type": "text", "text": "<transcript>" }] }],
            "metadata": { "user_id": "{\"device_id\":\"<device>\",\"account_uuid\":\"\",\"session_id\":\"<uuid>\"}" },
        });
        if let Some(stops) = stops {
            body["stop_sequences"] = serde_json::json!(stops);
        }
        body
    }

    fn stage(body: &Value) -> Option<u8> {
        BodyFacts::read(&serde_json::to_vec(body).unwrap())
            .shape
            .classifier_stage
    }

    const THIRD_PARTY_LINE: &str =
        "x-anthropic-billing-header: cc_version=2.1.284.e3c; cc_entrypoint=sdk-cli;";
    const FIRST_PARTY_LINE: &str =
        "x-anthropic-billing-header: cc_version=2.1.284.e3c; cc_entrypoint=sdk-cli; cch=00000;";

    #[test]
    fn the_clients_classifier_request_is_read_by_its_body_markers_at_either_stage() {
        for line in [THIRD_PARTY_LINE, FIRST_PARTY_LINE] {
            assert_eq!(
                stage(&classifier_body(Some(line), 2112, Some(&["</block>"]))),
                Some(1),
                "stage 1 stops at its closing tag, whichever billing line it carries"
            );
        }
        assert_eq!(
            stage(&classifier_body(
                Some(THIRD_PARTY_LINE),
                2112,
                Some(&["</severity>"])
            )),
            Some(1),
            "the severity protocol's stop is stage 1's as well"
        );
        assert_eq!(
            stage(&classifier_body(Some(THIRD_PARTY_LINE), 10240, None)),
            Some(2),
            "stage 2 runs to the end"
        );
        assert_eq!(
            stage(&classifier_body(None, 2112, Some(&["</block>"]))),
            Some(1),
            "the instruction may open the system list when no billing line precedes it"
        );
    }

    #[test]
    fn a_body_that_is_not_the_classifiers_reads_no_stage() {
        let mut session = serde_json::from_slice::<Value>(phoebis_profile::fixtures::body(
            phoebis_profile::fixtures::BodyFixture::Oauth,
        ))
        .unwrap();
        session["stop_sequences"] = serde_json::json!(["</block>"]);
        assert_eq!(
            stage(&session),
            None,
            "a session post with tools and a stop"
        );
        let mut streamed = classifier_body(Some(THIRD_PARTY_LINE), 2112, Some(&["</block>"]));
        streamed["stream"] = serde_json::json!(true);
        assert_eq!(stage(&streamed), None, "the classifier never streams");
        let mut with_tools = classifier_body(Some(THIRD_PARTY_LINE), 2112, Some(&["</block>"]));
        with_tools["tools"] = serde_json::json!([{ "name": "Bash" }]);
        assert_eq!(stage(&with_tools), None, "nor does it offer tools");
        assert_eq!(
            stage(&classifier_body(Some(THIRD_PARTY_LINE), 2112, Some(&["x"]))),
            None,
            "a stop that is neither closing tag belongs to no stage"
        );
        let mut unbounded = classifier_body(Some(THIRD_PARTY_LINE), 2112, Some(&["</block>"]));
        unbounded.as_object_mut().unwrap().remove("max_tokens");
        assert_eq!(
            stage(&unbounded),
            None,
            "the classifier always bounds its output"
        );
    }

    #[test]
    fn a_body_with_the_safeguards_member_asks_for_the_server_check_and_is_no_classifier_request() {
        let body = phoebis_profile::fixtures::with_safeguards(phoebis_profile::fixtures::body(
            phoebis_profile::fixtures::BodyFixture::Oauth,
        ));
        let shape = BodyFacts::read(&body).shape;
        assert!(shape.safeguards);
        assert_eq!(shape.classifier_stage, None);
    }

    #[test]
    fn the_safeguards_member_is_read_in_a_body_a_strict_parser_refuses() {
        // A lone surrogate escape, which a client emits for a string cut mid-pair: serde refuses the body while
        // the relay's own walk, the one the cut runs on, reads it.
        let body = br#"{"model":"m","messages":[{"role":"user","content":"\ud800"}],"safeguards":[],"stream":true}"#;
        assert!(serde_json::from_slice::<Value>(body).is_err());
        assert!(BodyFacts::read(body).shape.safeguards);
        let escaped = br#"{"model":"m","messages":[{"role":"user","content":"\ud800"}],"safeg\u0075ards":[]}"#;
        assert!(
            BodyFacts::read(escaped).shape.safeguards,
            "a key spelled with an escape is the member all the same"
        );
    }

    #[test]
    fn a_shape_with_every_member_and_a_string_system_reads_whole() {
        let body = br#"{"model":"m","system":"be brief","max_tokens":1024,"temperature":0.5,"speed":"fast","service_tier":"auto","output_config":{"effort":"xhigh"},"thinking":{"type":"adaptive"},"tools":[{"name":"a"},{"name":"b"}],"context_management":{"edits":[{"type":"clear_tool_uses_20250919"},{"type":"clear_thinking_20251015"}]},"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":"hello"},{"role":"user","content":"again"}]}"#;
        let shape = BodyFacts::read(body).shape;
        assert_eq!(shape.effort.as_deref(), Some("xhigh"));
        assert_eq!(
            (shape.thinking_type.as_deref(), shape.thinking_budget),
            (Some("adaptive"), None)
        );
        assert_eq!(
            (shape.tool_count, shape.system_blocks, shape.message_count),
            (2, 1, 3)
        );
        assert_eq!(
            shape.context_edits,
            vec!["clear_tool_uses_20250919", "clear_thinking_20251015"]
        );
        assert_eq!(
            (
                shape.temperature,
                shape.speed.as_deref(),
                shape.service_tier.as_deref()
            ),
            (Some(0.5), Some("fast"), Some("auto"))
        );
        assert_eq!(BodyFacts::read(b"not json").shape, RequestShape::default());
    }
}
