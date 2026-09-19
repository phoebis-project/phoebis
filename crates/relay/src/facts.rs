//! What the restoration reads off the body in one parse: the turn boundary and the version suffix.

use serde_json::Value;

pub struct BodyFacts {
    /// The last `messages` entry has role `user` and carries no `tool_result` block.
    pub fresh_user_turn: bool,
    /// Characters 4, 7, and 20 of the user's own prompt (the last `text` block of the first `user` message; the
    /// system-reminder blocks precede it), `'0'` where the text is shorter, for the version suffix.
    pub prompt_chars: [char; 3],
}

impl BodyFacts {
    pub fn read(body: &[u8]) -> BodyFacts {
        let Ok(root) = serde_json::from_slice::<Value>(body) else {
            return BodyFacts {
                fresh_user_turn: false,
                prompt_chars: ['0'; 3],
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
        BodyFacts {
            fresh_user_turn,
            prompt_chars,
        }
    }
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
            {"type":"text","text":"0123456789012345678901"}
        ]}]}"#;
        assert_eq!(BodyFacts::read(body).prompt_chars, ['4', '7', '0']);
    }

    #[test]
    fn an_unparseable_body_reads_as_not_a_fresh_turn_with_zeroed_characters() {
        let facts = BodyFacts::read(b"not json");
        assert!(!facts.fresh_user_turn);
        assert_eq!(facts.prompt_chars, ['0', '0', '0']);
    }
}
