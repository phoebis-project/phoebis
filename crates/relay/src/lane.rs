//! The upstream cache lane a request writes to, read from the bytes the client sent and changing none of them.

use crate::facts::BodyFacts;

/// The beta under which client 2.1.284 sends its effort as a per-turn control entry inside `messages`.
pub const PER_TURN_BETA: &str = "per-turn-control-2026-07-01";

/// The upstream cache lane a request writes to, beside its conversation: `<model>|<speed>|<thinking>|<effort>`.
/// `speed` is the body's or `standard`; `thinking` is `none` without a `thinking` object, else its `type` and,
/// when set, `:<budget_tokens>` (`display` is not a member); `effort` is `*` under the per-turn beta, since the
/// upstream keeps the cache across a per-turn change, else the top-level `output_config.effort` or `-` when the
/// body sets none. A control entry an earlier model's turn left in the history puts no level into the lane:
/// without the per-turn form the prefix is written under the top-level member alone. The upstream shares no
/// cached prefix across speeds, and a thinking or effort change rewrites the prefix on a model without the
/// per-turn form, so a lane change reads as a cold cache. The four parts go through `phoebis_routing::lane_key`.
pub fn lane_of(canonical_model: &str, facts: &BodyFacts, per_turn_effort: bool) -> String {
    let shape = &facts.shape;
    let speed = shape.speed.as_deref().map_or("standard", token);
    let thinking = match (
        shape.thinking_type.as_deref().map(token),
        shape.thinking_budget,
    ) {
        (None, _) => "none".to_string(),
        (Some(kind), None) => kind.to_string(),
        (Some(kind), Some(budget)) => format!("{kind}:{budget}"),
    };
    let effort = if per_turn_effort {
        "*"
    } else {
        facts.top_level_effort.as_deref().map_or("-", token)
    };
    phoebis_routing::lane_key(canonical_model, speed, &thinking, effort)
}

/// A part the client wrote, as sent when it is a short token and `other` otherwise: the lane lands on every pin and
/// entry the registry keeps, and a `|` would shift the parts after it.
fn token(part: &str) -> &str {
    let short = (1..=32).contains(&part.len())
        && part
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
    if short {
        part
    } else {
        "other"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lane_is_the_model_the_speed_the_thinking_and_the_effort_or_a_star_under_the_per_turn_beta(
    ) {
        let fast = BodyFacts::read(
            br#"{"speed":"fast","thinking":{"type":"adaptive"},"output_config":{"effort":"low"},"messages":[{"role":"user","content":"hi"}]}"#,
        );
        assert_eq!(
            lane_of("claude-opus-5-5", &fast, true),
            "claude-opus-5-5|fast|adaptive|*"
        );
        assert_eq!(
            lane_of("claude-opus-5-5", &fast, false),
            "claude-opus-5-5|fast|adaptive|low"
        );
        let budgeted = BodyFacts::read(
            br#"{"thinking":{"type":"enabled","budget_tokens":31999},"messages":[{"role":"user","content":"hi"}]}"#,
        );
        assert_eq!(
            lane_of("claude-sonnet-4-5", &budgeted, false),
            "claude-sonnet-4-5|standard|enabled:31999|-"
        );
    }

    #[test]
    fn a_body_without_speed_or_thinking_reads_standard_and_none() {
        let bare = BodyFacts::read(br#"{"messages":[{"role":"user","content":"hi"}]}"#);
        assert_eq!(
            lane_of("claude-opus-5-5", &bare, false),
            "claude-opus-5-5|standard|none|-"
        );
    }

    #[test]
    fn a_lane_part_that_is_no_short_token_collapses_to_other() {
        let body = |speed: &str, thinking: &str, effort: &str| {
            BodyFacts::read(
                serde_json::json!({
                    "speed": speed,
                    "thinking": {"type": thinking, "budget_tokens": 1024},
                    "output_config": {"effort": effort},
                    "messages": [{"role": "user", "content": "hi"}],
                })
                .to_string()
                .as_bytes(),
            )
        };
        let longest = "a".repeat(32);
        assert_eq!(
            lane_of("m", &body(&longest, "enabled", "x-high.2_B"), false),
            format!("m|{longest}|enabled:1024|x-high.2_B"),
            "32 bytes of letters, digits, `_`, `-` and `.` stand as sent"
        );
        assert_eq!(
            lane_of("m", &body(&"a".repeat(33), "en abled", "hi|gh"), false),
            "m|other|other:1024|other"
        );
        assert_eq!(
            lane_of("m", &body("", "é", "high"), false),
            "m|other|other:1024|high"
        );
    }

    #[test]
    fn a_control_entry_left_in_the_history_puts_no_level_into_the_lane_without_the_per_turn_beta() {
        let stale = |top_level: &str| {
            BodyFacts::read(
                format!(
                    r#"{{{top_level}"messages":[
                        {{"role":"user","content":"first"}},
                        {{"role":"system","content":[{{"type":"text","text":"a"}}],"output_config":{{"effort":"high"}}}},
                        {{"role":"assistant","content":"hi"}},
                        {{"role":"user","content":"again"}}
                    ]}}"#
                )
                .as_bytes(),
            )
        };
        let with_top_level = stale(r#""output_config":{"effort":"low"},"#);
        assert_eq!(
            with_top_level.shape.effort.as_deref(),
            Some("high"),
            "the effective effort still reads the control entry"
        );
        assert_eq!(
            lane_of("claude-sonnet-5", &with_top_level, false),
            "claude-sonnet-5|standard|none|low"
        );
        assert_eq!(
            lane_of("claude-sonnet-5", &stale(""), false),
            "claude-sonnet-5|standard|none|-"
        );
    }
}
