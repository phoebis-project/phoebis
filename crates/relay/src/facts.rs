//! What the restoration reads off the body in one parse: the turn boundary, the version suffix, and the
//! prompt id a first-party line already carries.

use axum::http::HeaderMap;
use phoebis_bus::RequestShape;
use serde_json::Value;

use crate::chains::Chains;

#[derive(Debug, PartialEq)]
pub struct BodyFacts {
    /// The last `messages` entry whose role is not `system` has role `user` and carries no `tool_result`
    /// block: a per-turn control entry after the user's turn is the client's, not a turn of its own.
    pub fresh_user_turn: bool,
    /// UTF-16 code units 4, 7, and 20 of each `text` block of the first `user` message, in the wire's order,
    /// matching the client's own JavaScript string indexing; `b'0' as u16` where a text is shorter. The client
    /// hashes the first text of its first non-meta user message for the version suffix, and the meta flag never
    /// reaches the wire: the blocks it prepends (a system reminder, a slash command's caveat and output) and the
    /// user's own words all land as text blocks of one message, so every block is a candidate.
    pub prompt_units: Vec<[u16; 3]>,
    /// The `cc_prompt_id` the caller's own attribution line carries, absent from a reduced-shape request and
    /// from a value spelled any other way than the client spells one.
    pub prompt_id: Option<String>,
    /// The `cc_turn_origin` the line carries, in the client's own spelling (a token of lowercase ASCII letters
    /// and underscores, at most 32 bytes); absent from a reduced-shape request.
    pub turn_origin: Option<String>,
    /// The `cc_entrypoint` token of the billing line under the entrypoint token rule, `None` for a body without a
    /// line, a line that names none, a value outside the rule, and a body no reader parses.
    pub entrypoint: Option<String>,
    /// Whether `system[0]` is an attribution line at all, stamped or reduced: what the restoration fills.
    pub carries_line: bool,
    pub shape: RequestShape,
    /// The top-level `output_config.effort` alone, whatever a control entry says: the lane of a request without
    /// the per-turn beta reads it, since a control entry left in the history sets no level there.
    pub top_level_effort: Option<String>,
    /// `None` for a body that is not a JSON object; the same parse `shape` and the rest above read.
    pub chains: Option<Chains>,
    /// The largest `cache_control.ttl` at the API's marker positions, `None` without a marker there.
    pub markers_ttl_ms: Option<i64>,
    /// `system` is an array: what the billing-line restoration needs to know before it reads the raw span.
    pub system_is_array: bool,
    /// The body references no account-scoped state (`container`, `file_id`, a server-tool result).
    pub portable: bool,
}

/// The gateway hint headers, each optional, read the way `identify` reads an identifier: bounded and
/// charset-checked, a malformed value absent rather than refused.
pub struct ClientHints {
    pub agent_type: Option<String>,
    /// `anthropic-beta` carries `auto-mode-classifier-2026-07-16`, on any of its lines.
    pub classifier_beta: bool,
    pub compaction: Option<String>,
    pub context_compacted: Option<String>,
    /// `anthropic-beta` carries `per-turn-control-2026-07-01`, on any of its lines.
    pub per_turn_effort: bool,
    /// `<name>=<ms>;...` summed; absent when no entry parses or the value runs past 4 KiB.
    pub prev_tool_ms: Option<i64>,
    /// `x-claude-code-request-class`.
    pub request_class: Option<String>,
}

impl BodyFacts {
    /// The facts and the parsed root they were read from, for a caller that reads the tree once more and drops it.
    pub fn read_with_tree(body: &[u8]) -> (BodyFacts, Option<Value>) {
        // Counted over decoded keys on the walk the cut runs on rather than read from the parse below, so the
        // request row says what the cut acts on under an off setting whatever the parse makes of the body.
        let safeguards =
            phoebis_cch::top_level_member_count(body, "safeguards").is_ok_and(|n| n >= 1);
        let Some(root) = phoebis_guardrails::parse_body(body) else {
            let facts = BodyFacts {
                fresh_user_turn: false,
                prompt_units: vec![[b'0' as u16; 3]],
                prompt_id: None,
                turn_origin: None,
                entrypoint: None,
                carries_line: false,
                shape: RequestShape {
                    safeguards,
                    ..RequestShape::default()
                },
                top_level_effort: None,
                chains: None,
                markers_ttl_ms: None,
                system_is_array: false,
                // Fails open as `is_portable` does: a body no reader parses carries no reference the router could
                // prove, and the upstream refuses it anyway.
                portable: true,
            };
            return (facts, None);
        };
        let messages = root.get("messages").and_then(Value::as_array);
        let is_system = |entry: &Value| entry.get("role").and_then(Value::as_str) == Some("system");
        // The client appends its per-turn control entry after the user's turn, so the turn is judged on the last
        // entry the user or the assistant wrote, not on the client's own.
        let fresh_user_turn = messages
            .and_then(|m| m.iter().rev().find(|entry| !is_system(entry)))
            .is_some_and(is_fresh_user_turn);
        // The upstream applies the newest control entry's effort over the top-level member.
        let control_effort = messages.and_then(|m| {
            m.iter()
                .rev()
                .filter(|entry| is_system(entry))
                .find_map(|entry| entry.get("output_config")?.get("effort")?.as_str())
                .map(str::to_string)
        });
        let prompt_units = messages
            .and_then(|m| {
                m.iter()
                    .find(|msg| msg.get("role").and_then(Value::as_str) == Some("user"))
            })
            .map(|message| texts(message).map(prompt_units_of).collect::<Vec<_>>())
            .filter(|candidates| !candidates.is_empty())
            .unwrap_or_else(|| vec![[b'0' as u16; 3]]);
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
        let entrypoint = first_system_text
            .and_then(carried_entrypoint)
            .map(String::from);
        let count = |v: Option<&Value>| v.and_then(Value::as_array).map_or(0, |a| a.len() as u32);
        let text = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_string);
        let thinking = root.get("thinking");
        let top_level_effort = text(root.get("output_config").and_then(|o| o.get("effort")));
        let shape = RequestShape {
            effort: control_effort.or_else(|| top_level_effort.clone()),
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
        };
        let chains = Chains::read(&root);
        let markers_ttl_ms = markers_ttl_ms(&root);
        let facts = BodyFacts {
            fresh_user_turn,
            prompt_units,
            prompt_id,
            turn_origin,
            entrypoint,
            carries_line,
            shape,
            top_level_effort,
            chains,
            markers_ttl_ms,
            system_is_array: matches!(root.get("system"), Some(Value::Array(_))),
            portable: crate::portability::portable_of(&root),
        };
        (facts, Some(root))
    }

    /// The handler keeps the tree for the guardrail step, so the facts alone are what the tests read.
    #[cfg(test)]
    pub fn read(body: &[u8]) -> BodyFacts {
        Self::read_with_tree(body).0
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
            classifier_beta: carries_beta(headers, crate::body::CLASSIFIER_BETA),
            compaction: crate::identity::header_id(headers, "x-claude-code-compaction"),
            context_compacted: crate::identity::header_id(
                headers,
                "x-claude-code-context-compacted",
            ),
            per_turn_effort: carries_beta(headers, crate::lane::PER_TURN_BETA),
            prev_tool_ms: headers
                .get("x-claude-code-prev-tool-durations")
                .and_then(|v| v.to_str().ok())
                .and_then(tool_ms_sum),
            request_class: crate::identity::header_id(headers, "x-claude-code-request-class"),
        }
    }
}

/// Whether any `anthropic-beta` line names `beta` among its comma-separated tokens.
fn carries_beta(headers: &HeaderMap, beta: &str) -> bool {
    headers
        .get_all("anthropic-beta")
        .iter()
        .filter_map(|line| line.to_str().ok())
        .flat_map(|line| line.split(','))
        .any(|token| token.trim() == beta)
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

/// The `cc_entrypoint` of an attribution line under the alphabet the user agent's token is read by, so the
/// cross-check compares the two token for token and a separator never reaches its log line.
fn carried_entrypoint(text: &str) -> Option<&str> {
    carried_field(text, "cc_entrypoint").filter(|value| crate::billing::is_entrypoint_token(value))
}

/// One `info` line where the body's line names another entrypoint than the user agent did, with the request id and
/// both tokens, and nothing where they agree or either is absent. The user agent's token prices: the line is the
/// client's own attribution and the regime a pricing fact, so a disagreement is something to read, never a refusal.
pub(crate) fn note_entrypoint_disagreement(
    request_id: &str,
    user_agent: Option<&str>,
    line: Option<&str>,
) {
    if let (Some(agent), Some(line)) = (user_agent, line) {
        if agent != line {
            tracing::info!(
                request = %request_id,
                user_agent_entrypoint = agent,
                billing_entrypoint = line,
                "the billing line names another entrypoint than the user agent, which prices"
            );
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

/// The `text` blocks of a message in order: a string content is one text, an array yields each `text` block.
fn texts(message: &Value) -> Box<dyn Iterator<Item = &str> + '_> {
    match message.get("content") {
        Some(Value::String(s)) => Box::new(std::iter::once(s.as_str())),
        Some(Value::Array(blocks)) => Box::new(
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str)),
        ),
        _ => Box::new(std::iter::empty()),
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
    use crate::body::CLASSIFIER_BETA;

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
    fn a_control_entry_after_the_user_turn_keeps_it_a_fresh_turn() {
        let body = br#"{"messages":[
            {"role":"user","content":"say hi again"},
            {"role":"system","content":[{"type":"text","text":"hook context"}],"output_config":{"effort":"high"}}
        ]}"#;
        assert!(BodyFacts::read(body).fresh_user_turn);
    }

    #[test]
    fn a_control_entry_after_a_tool_result_is_not_a_fresh_turn() {
        let body = br#"{"messages":[
            {"role":"user","content":"hi"},
            {"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"x","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]},
            {"role":"system","content":[{"type":"text","text":"hook context"}],"output_config":{"effort":"low"}}
        ]}"#;
        assert!(!BodyFacts::read(body).fresh_user_turn);
    }

    #[test]
    fn the_last_control_entrys_effort_is_the_effective_one_and_the_top_level_member_is_the_fallback(
    ) {
        let two_entries = br#"{"output_config":{"effort":"medium"},"messages":[
            {"role":"user","content":"first"},
            {"role":"system","content":[{"type":"text","text":"a"}],"output_config":{"effort":"low"}},
            {"role":"assistant","content":"hi"},
            {"role":"user","content":"say hi again"},
            {"role":"system","content":[{"type":"text","text":"b"}],"output_config":{"effort":"high"}}
        ]}"#;
        assert_eq!(
            BodyFacts::read(two_entries).shape.effort.as_deref(),
            Some("high")
        );
        let control_only_differs = br#"{"output_config":{"effort":"low"},"messages":[
            {"role":"user","content":"hi"},
            {"role":"system","content":[{"type":"text","text":"a"}],"output_config":{"effort":"max"}}
        ]}"#;
        assert_eq!(
            BodyFacts::read(control_only_differs)
                .shape
                .effort
                .as_deref(),
            Some("max")
        );
        let top_level_alone = br#"{"output_config":{"effort":"medium"},"messages":[
            {"role":"user","content":"hi"},
            {"role":"system","content":[{"type":"text","text":"a"}]}
        ]}"#;
        assert_eq!(
            BodyFacts::read(top_level_alone).shape.effort.as_deref(),
            Some("medium")
        );
        let neither = br#"{"messages":[{"role":"user","content":"hi"}]}"#;
        assert_eq!(BodyFacts::read(neither).shape.effort, None);
        let config_less_last = br#"{"messages":[
            {"role":"user","content":"hi"},
            {"role":"system","content":[{"type":"text","text":"a"}],"output_config":{"effort":"low"}},
            {"role":"system","content":[{"type":"text","text":"b"}]}
        ]}"#;
        assert_eq!(
            BodyFacts::read(config_less_last).shape.effort.as_deref(),
            Some("low"),
            "a system entry without a config steps back to the earlier control entry"
        );
        let null_last = br#"{"messages":[
            {"role":"user","content":"hi"},
            {"role":"system","content":[{"type":"text","text":"a"}],"output_config":{"effort":"low"}},
            {"role":"system","content":[{"type":"text","text":"b"}],"output_config":{"effort":null}}
        ]}"#;
        assert_eq!(
            BodyFacts::read(null_last).shape.effort.as_deref(),
            Some("low"),
            "a level that is no string steps back as well"
        );
        let system_alone = br#"{"messages":[
            {"role":"system","content":[{"type":"text","text":"a"}],"output_config":{"effort":"low"}}
        ]}"#;
        assert!(!BodyFacts::read(system_alone).fresh_user_turn);
    }

    #[test]
    fn the_prompt_units_of_a_short_prompt_pad_with_zero() {
        let body = br#"{"messages":[{"role":"user","content":"hi"}]}"#;
        assert_eq!(BodyFacts::read(body).prompt_units, vec![[b'0' as u16; 3]]);
    }

    #[test]
    fn every_text_block_of_the_first_user_message_is_a_candidate() {
        let body = br#"{"messages":[{"role":"user","content":[
            {"type":"text","text":"<system-reminder>ignored</system-reminder>"},
            {"type":"image","source":{}},
            {"type":"text","text":"01234567890123456789X1"}
        ]},{"role":"user","content":"a later message is not read"}]}"#;
        assert_eq!(
            BodyFacts::read(body).prompt_units,
            vec![
                [b't' as u16, b'-' as u16, b'o' as u16],
                [b'4' as u16, b'7' as u16, b'X' as u16]
            ]
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
            vec![[b'c' as u16, b'f' as u16, b's' as u16]]
        );
    }

    #[test]
    fn an_unparseable_body_reads_as_not_a_fresh_turn_with_zeroed_units() {
        let facts = BodyFacts::read(b"not json");
        assert!(!facts.fresh_user_turn);
        assert_eq!(facts.prompt_units, vec![[b'0' as u16; 3]]);
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
    fn cc_entrypoint_is_read_off_the_line_under_the_entrypoint_tokens_rule() {
        let line = |entrypoint: &str| {
            format!(
                r#"{{"system":[{{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.295.517; cc_entrypoint={entrypoint}; cc_turn_origin=human;"}}]}}"#
            )
        };
        let read = |body: &str| BodyFacts::read(body.as_bytes()).entrypoint;
        assert_eq!(
            read(&line("claude-desktop-3p")).as_deref(),
            Some("claude-desktop-3p")
        );
        assert_eq!(read(&line("local_agent")).as_deref(), Some("local_agent"));
        assert_eq!(
            read(&line("Desk top")),
            None,
            "a value outside the token alphabet is absent"
        );
        let over_bound = "a".repeat(crate::identity::MAX_ID_LEN + 1);
        assert_eq!(
            read(&line(&over_bound)),
            None,
            "past the bound the value is absent"
        );
        assert_eq!(
            BodyFacts::read(phoebis_profile::fixtures::body(
                phoebis_profile::fixtures::BodyFixture::Oauth
            ))
            .entrypoint
            .as_deref(),
            Some("sdk-cli"),
            "the reduced shape's line names its entrypoint too"
        );
        assert_eq!(
            read(
                r#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.295.517;"}]}"#
            ),
            None,
            "a line that names no entrypoint"
        );
        assert_eq!(
            read(r#"{"system":[{"type":"text","text":"be helpful"}]}"#),
            None
        );
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
        assert!(!hints.per_turn_effort);
        assert!(!hints.classifier_beta);
        let mut betas = HeaderMap::new();
        betas.append(
            "anthropic-beta",
            "effort-2025-11-24,mid-conversation-system-2026-04-07"
                .parse()
                .unwrap(),
        );
        assert!(
            !ClientHints::read(&betas).per_turn_effort,
            "a line without the per-turn beta"
        );
        assert!(
            !ClientHints::read(&betas).classifier_beta,
            "a line without the classifier beta"
        );
        betas.append(
            "anthropic-beta",
            "per-turn-control-2026-07-01".parse().unwrap(),
        );
        assert!(
            ClientHints::read(&betas).per_turn_effort,
            "the per-turn beta on a line of its own"
        );
        betas.append("anthropic-beta", CLASSIFIER_BETA.parse().unwrap());
        let hints = ClientHints::read(&betas);
        assert!(
            hints.classifier_beta,
            "the classifier beta on a line of its own"
        );
        assert!(
            hints.per_turn_effort,
            "the per-turn beta still read beside a third line"
        );
        let mut spaced = HeaderMap::new();
        spaced.insert(
            "anthropic-beta",
            "effort-2025-11-24, per-turn-control-2026-07-01"
                .parse()
                .unwrap(),
        );
        assert!(
            ClientHints::read(&spaced).per_turn_effort,
            "the per-turn beta after a comma and a space"
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

    #[test]
    fn a_body_with_the_safeguards_member_asks_for_the_server_check() {
        let body = phoebis_profile::fixtures::with_safeguards(phoebis_profile::fixtures::body(
            phoebis_profile::fixtures::BodyFixture::Oauth,
        ));
        assert!(BodyFacts::read(&body).shape.safeguards);
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
    fn a_body_cut_mid_pair_still_reads_its_line_its_turn_its_chain_and_its_markers() {
        // The client sends a string cut between the two halves of a character as a lone surrogate escape, which a
        // strict decoder refuses.
        let body = br#"{"model":"m","system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.269.517; cc_entrypoint=cli; cc_prompt_id=11111111-1111-4111-8111-111111111111;"}],"messages":[{"role":"user","content":[{"type":"text","text":"cut \ud83d","cache_control":{"type":"ephemeral","ttl":"1h"}}]}]}"#;
        assert!(serde_json::from_slice::<Value>(body).is_err());
        let facts = BodyFacts::read(body);
        assert!(facts.carries_line);
        assert_eq!(
            facts.prompt_id.as_deref(),
            Some("11111111-1111-4111-8111-111111111111")
        );
        assert!(facts.fresh_user_turn);
        assert!(facts.chains.is_some());
        assert_eq!(facts.markers_ttl_ms, Some(3_600_000));
        assert_eq!(facts.shape.message_count, 1);
    }

    /// A lone surrogate at a sampled index reads as U+FFFD off the reading copy. The client's suffix decodes its
    /// own units lossily, so the lone unit reads the same there and the cut conversation is no drift.
    #[test]
    fn a_lone_surrogate_at_a_sampled_index_reads_as_the_replacement_and_its_suffix_reproduces() {
        let client_units = [0xD83D, u16::from(b'g'), u16::from(b't')];
        let line = format!(
            "x-anthropic-billing-header: cc_version={}; cc_entrypoint=cli;",
            crate::billing::cc_version("2.1.269", client_units)
        );
        let body = format!(
            r#"{{"model":"m","system":[{{"type":"text","text":"{line}"}}],"messages":[{{"role":"user","content":"abcd\ud83defghijklmnopqrstuvwxyz"}}]}}"#
        );
        assert!(serde_json::from_slice::<Value>(body.as_bytes()).is_err());
        let facts = BodyFacts::read(body.as_bytes());
        assert!(facts.carries_line);
        assert_eq!(
            facts.prompt_units,
            vec![[0xFFFD, u16::from(b'g'), u16::from(b't')]]
        );
        assert_eq!(
            crate::body::version_drift(
                body.as_bytes(),
                crate::body::VersionCheck {
                    client_version: "2.1.269",
                    prompt_units: &facts.prompt_units,
                }
            ),
            None
        );
    }

    #[test]
    fn whether_system_is_an_array_is_read_off_the_one_parse() {
        assert!(BodyFacts::read(br#"{"system":[],"messages":[]}"#).system_is_array);
        assert!(!BodyFacts::read(br#"{"system":"be brief","messages":[]}"#).system_is_array);
        assert!(!BodyFacts::read(br#"{"messages":[]}"#).system_is_array);
        // The parse reads the last spelling of a repeated key, so the flag says array while the billing line's
        // scanner reads the first spelling, a string, and finds no line to fill. Admission refuses a repeated
        // `system` before either reads it.
        assert!(
            BodyFacts::read(br#"{"model":"m","system":"be helpful","messages":[],"system":[]}"#)
                .system_is_array
        );
    }

    #[test]
    fn portability_is_read_off_the_one_parse_and_a_body_no_reader_parses_fails_open() {
        assert!(
            BodyFacts::read(br#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#)
                .portable
        );
        assert!(
            !BodyFacts::read(
                br#"{"model":"m","container":"c","messages":[{"role":"user","content":"cut \ud83d"}]}"#
            )
            .portable
        );
        let unread = BodyFacts::read(b"not json");
        assert!(unread.portable);
        assert!(!unread.system_is_array);
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

    #[test]
    fn read_with_tree_answers_the_tree_for_a_parsed_body_and_none_for_one_that_does_not_parse() {
        let body = br#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;
        let (_, tree) = BodyFacts::read_with_tree(body);
        assert_eq!(tree, Some(serde_json::from_slice::<Value>(body).unwrap()));
        // The client cuts a string mid-pair, and the reading copy still yields a tree.
        let cut = br#"{"model":"m","messages":[{"role":"user","content":"a\ud83d"}]}"#;
        assert!(serde_json::from_slice::<Value>(cut).is_err());
        assert!(BodyFacts::read_with_tree(cut).1.is_some());
        let (facts, tree) = BodyFacts::read_with_tree(b"not json");
        assert_eq!(tree, None);
        assert!(facts.chains.is_none());
    }

    /// The guardrails crate carries its own copy of `texts`, and a message condition has to read the text blocks
    /// the version suffix reads.
    #[test]
    fn the_relays_texts_and_the_crates_agree_on_both_fixtures() {
        for fixture in [
            include_str!("../../guardrails/tests/fixtures/subagent_summary_fork.json"),
            include_str!("../../guardrails/tests/fixtures/auto_mode_classifier.json"),
        ] {
            let root: Value = serde_json::from_str(fixture).unwrap();
            let messages = root["messages"].as_array().unwrap();
            assert!(!messages.is_empty());
            for message in messages {
                assert_eq!(
                    texts(message).collect::<Vec<_>>(),
                    phoebis_guardrails::texts(message).collect::<Vec<_>>()
                );
            }
        }
    }
}
