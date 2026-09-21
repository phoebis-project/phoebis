//! What the restoration reads off the body in one parse: the turn boundary, the version suffix, and the
//! prompt id a first-party line already carries.

use phoebis_bus::RequestShape;
use serde_json::Value;

pub struct BodyFacts {
    /// The last `messages` entry has role `user` and carries no `tool_result` block.
    pub fresh_user_turn: bool,
    /// Characters 4, 7, and 20 of the user's own prompt (the last `text` block of the first `user` message; the
    /// system-reminder blocks precede it), `'0'` where the text is shorter, for the version suffix.
    pub prompt_chars: [char; 3],
    /// The `cc_prompt_id` the caller's own attribution line carries, absent from a reduced-shape request and
    /// from a value spelled any other way than the client spells one.
    pub prompt_id: Option<String>,
    pub shape: RequestShape,
}

impl BodyFacts {
    pub fn read(body: &[u8]) -> BodyFacts {
        let Ok(root) = serde_json::from_slice::<Value>(body) else {
            return BodyFacts {
                fresh_user_turn: false,
                prompt_chars: ['0'; 3],
                prompt_id: None,
                shape: RequestShape::default(),
            };
        };
        let messages = root.get("messages").and_then(Value::as_array);
        let fresh_user_turn = messages
            .and_then(|m| m.last())
            .is_some_and(is_fresh_user_turn);
        let prompt_chars = messages
            .and_then(|m| {
                m.iter()
                    .find(|msg| msg.get("role").and_then(Value::as_str) == Some("user"))
            })
            .and_then(last_text)
            .map(prompt_chars_of)
            .unwrap_or(['0'; 3]);
        let prompt_id = root
            .get("system")
            .and_then(Value::as_array)
            .and_then(|blocks| blocks.first())
            .and_then(|block| block.get("text"))
            .and_then(Value::as_str)
            .and_then(carried_prompt_id)
            .map(String::from);
        let count = |v: Option<&Value>| v.and_then(Value::as_array).map_or(0, |a| a.len() as u32);
        let text = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_string);
        let thinking = root.get("thinking");
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
        };
        BodyFacts {
            fresh_user_turn,
            prompt_chars,
            prompt_id,
            shape,
        }
    }
}

/// What a carried id may weigh. A UUID is 36; the session memory retains one per entry for a day, so a value
/// past this is read as absent rather than held.
const PROMPT_ID_MAX: usize = 64;

/// The `cc_prompt_id` of an attribution line, accepted only as the client's own spelling of an id: the value
/// is recorded and then restored onto a later turn of the same session, in a line this build signs, so a
/// separator smuggled into it would become a field of that line.
fn carried_prompt_id(text: &str) -> Option<&str> {
    let value = text
        .strip_prefix("x-anthropic-billing-header: ")?
        .split("; ")
        .find_map(|field| field.strip_prefix("cc_prompt_id="))?
        .trim_end_matches(';');
    let spelled = (1..=PROMPT_ID_MAX).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-');
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

fn prompt_chars_of(text: &str) -> [char; 3] {
    let chars: Vec<char> = text.chars().collect();
    let at = |i: usize| chars.get(i).copied().unwrap_or('0');
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
    fn the_prompt_characters_of_a_short_prompt_pad_with_zero() {
        let body = br#"{"messages":[{"role":"user","content":"hi"}]}"#;
        assert_eq!(BodyFacts::read(body).prompt_chars, ['0', '0', '0']);
    }

    #[test]
    fn the_last_text_block_of_the_first_user_message_is_read_past_system_reminders() {
        let body = br#"{"messages":[{"role":"user","content":[
            {"type":"text","text":"<system-reminder>ignored</system-reminder>"},
            {"type":"text","text":"01234567890123456789X1"}
        ]}]}"#;
        assert_eq!(BodyFacts::read(body).prompt_chars, ['4', '7', 'X']);
    }

    #[test]
    fn an_unparseable_body_reads_as_not_a_fresh_turn_with_zeroed_characters() {
        let facts = BodyFacts::read(b"not json");
        assert!(!facts.fresh_user_turn);
        assert_eq!(facts.prompt_chars, ['0', '0', '0']);
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
