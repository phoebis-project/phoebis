//! Whether a request body can move between accounts: a reference to account-scoped state binds it to the
//! account that holds that state.

use serde_json::Value;

/// Whether the parsed body can move between accounts. The relay's `BodyFacts::portable` and the keepalive's recipe
/// check each read it off the parse they already ran.
pub fn portable_of(root: &Value) -> bool {
    let Some(object) = root.as_object() else {
        return true;
    };
    if object.contains_key("container") {
        return false;
    }
    object
        .get("messages")
        .and_then(Value::as_array)
        .is_none_or(|messages| !messages.iter().any(references_account_state))
}

fn references_account_state(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            if map.contains_key("file_id") || map.contains_key("container") {
                return true;
            }
            if map
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| t.ends_with("_tool_result"))
            {
                return true;
            }
            map.values().any(references_account_state)
        }
        Value::Array(items) => items.iter().any(references_account_state),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::portable_of;

    /// The body as the relay's facts read it: through the lenient parse, a body no reader parses read as portable.
    fn is_portable(body: &[u8]) -> bool {
        phoebis_guardrails::parse_body(body).is_none_or(|root| portable_of(&root))
    }

    #[test]
    fn account_scoped_references_make_a_request_non_portable() {
        assert!(is_portable(
            br#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#
        ));
        assert!(!is_portable(
            br#"{"model":"m","container":"cont_1","messages":[]}"#
        ));
        assert!(!is_portable(br#"{"model":"m","messages":[{"role":"user","content":[{"type":"document","source":{"type":"file","file_id":"file_1"}}]}]}"#));
        assert!(!is_portable(br#"{"model":"m","messages":[{"role":"user","content":[{"type":"code_execution_tool_result","tool_use_id":"x","content":{}}]}]}"#));
        assert!(is_portable(br#"{"model":"m","messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"x","content":"ok"}]}]}"#), "a client tool result is portable");
        assert!(is_portable(br#"{"model":"m","messages":[{"role":"user","content":"the word file_id in text"}]}"#), "a string mentioning the key is not a reference");
    }

    #[test]
    fn a_body_cut_mid_pair_is_read_for_its_references_and_one_no_reader_parses_fails_open() {
        // A lone surrogate escape, which the client sends for a string cut mid-pair: a strict decoder refuses the
        // body, and the container it names binds it all the same.
        let cut = br#"{"model":"m","container":"cont_1","messages":[{"role":"user","content":"cut \ud83d"}]}"#;
        assert!(serde_json::from_slice::<serde_json::Value>(cut).is_err());
        assert!(!is_portable(cut));
        assert!(is_portable(
            br#"{"model":"m","messages":[{"role":"user","content":"cut \ud83d"}]}"#
        ));
        assert!(
            is_portable(b"not json"),
            "a body no reader parses fails open"
        );
    }
}
