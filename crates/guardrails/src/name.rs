//! The phrases Claude Code reads specially in an error's message, which a guardrail's name may not carry: a
//! block's message carries the name, and the name is the one part of it the operator writes.

/// "model:" in a 403 `permission_error` is the client's model-access switch, "OAuth token has been revoked" in a
/// 403 its token refresh, and "prompt is too long" in a 400 its compaction trigger.
const PHRASES: &[&str] = &[
    "model:",
    "OAuth token has been revoked",
    "prompt is too long",
];

/// The phrase the name carries in any case, spelled as the list spells it, or `None`.
pub fn client_phrase(name: &str) -> Option<&'static str> {
    let name = name.to_lowercase();
    PHRASES
        .iter()
        .copied()
        .find(|phrase| name.contains(&phrase.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_model_access_phrase_is_found_in_any_case_and_answered_as_listed() {
        assert_eq!(client_phrase("Model: Opus Only"), Some("model:"));
        assert_eq!(
            client_phrase("Opus model only"),
            None,
            "the word without its colon is no switch"
        );
    }

    #[test]
    fn the_token_refresh_phrase_is_found_in_any_case_and_answered_as_listed() {
        assert_eq!(
            client_phrase("oauth TOKEN has been REVOKED"),
            Some("OAuth token has been revoked")
        );
        assert_eq!(client_phrase("Revoked tokens"), None);
    }

    #[test]
    fn the_compaction_phrase_is_found_in_any_case_and_answered_as_listed() {
        assert_eq!(
            client_phrase("Prompt Is Too Long!"),
            Some("prompt is too long")
        );
        assert_eq!(client_phrase("Large auxiliary"), None);
    }
}
