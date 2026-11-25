//! The two cumulative boundary-hash chains a request body reads as facts: a static chain over the tools array
//! and the system blocks, and a message chain with one boundary per entry of `messages`. Both travel with the
//! observation and the selection, so a warm pin's continuation and a keepalive's guard read the same hashes.

use std::sync::Arc;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::billing::ATTRIBUTION_PREFIX;

/// Seeds the two chains so they never collide: an MCP server joining mid-session changes the tools array and
/// every static boundary, and leaves every message boundary equal, which only holds when the two chains start
/// from different bytes.
const STATIC_SEED: &[u8] = b"phoebis-static-chain-v1";
const MESSAGE_SEED: &[u8] = b"phoebis-message-chain-v1";

type Link = sha2::digest::Output<Sha256>;

/// The two cumulative boundary-hash chains of a request body, lowercase hex, computed on the client's bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Chains {
    /// The tools array as one unit, then each system block in order, the attribution line skipped; the last
    /// entry is the request's static hash. Shared by pointer with every attempt's selection and the answer's
    /// affinity report.
    pub static_boundaries: Arc<[String]>,
    /// One boundary per entry of `messages`, on a seed of its own; shared as the static chain is.
    pub message_boundaries: Arc<[String]>,
    /// The positions in `message_boundaries` of the messages that carry a `cache_control` marker, ascending: on a
    /// block of the message's content or on a block of such a block's own `content` (a `tool_result`'s), the two
    /// levels the upstream caches through.
    pub marked: Arc<[usize]>,
    /// Parallel to `static_boundaries`: the estimated token size of the blocks up to and including each
    /// boundary, the canonical bytes the chain hashed over four; 0 at the empty-tools boundary of a request
    /// without tools.
    pub static_sizes: Vec<u64>,
    /// The index in `static_boundaries` of the deepest boundary that a cache marker on a tool or on a system block
    /// covers, the tools boundary for a tool's: the upstream writes a cache entry at a marker alone, so a boundary past
    /// it names a prefix that no cache holds. `None` where no tool and no system block carries a marker, so a body
    /// whose one marker is top-level names none, since that marker's entry ends inside `messages`.
    pub static_marked: Option<usize>,
}

impl Chains {
    /// `None` for a body that is not a JSON object.
    pub fn read(root: &Value) -> Option<Chains> {
        if !root.is_object() {
            return None;
        }
        let empty_array = Value::Array(Vec::new());
        let tools = root.get("tools").unwrap_or(&empty_array);
        let mut static_boundaries = Vec::new();
        let mut static_sizes = Vec::new();
        let mut running_bytes: u64 = 0;
        let mut scratch = Vec::new();
        let mut previous = Sha256::digest(STATIC_SEED);
        let mut static_marked = None;
        for block in std::iter::once(tools).chain(system_blocks(root.get("system"))) {
            let (next, len) = boundary(&previous, block, &mut scratch);
            previous = next;
            running_bytes += len;
            if carries_marker(block) {
                static_marked = Some(static_boundaries.len());
            }
            static_boundaries.push(format!("{previous:x}"));
            static_sizes.push(running_bytes / 4);
        }
        let mut message_boundaries = Vec::new();
        let mut marked = Vec::new();
        let mut previous = Sha256::digest(MESSAGE_SEED);
        if let Some(Value::Array(messages)) = root.get("messages") {
            for (at, message) in messages.iter().enumerate() {
                previous = boundary(&previous, message, &mut scratch).0;
                message_boundaries.push(format!("{previous:x}"));
                if crate::body::message_cache_controls(message)
                    .next()
                    .is_some()
                {
                    marked.push(at);
                }
            }
        }
        Some(Chains {
            static_boundaries: static_boundaries.into(),
            message_boundaries: message_boundaries.into(),
            marked: marked.into(),
            static_sizes,
            static_marked,
        })
    }

    pub fn static_hash(&self) -> Option<&str> {
        self.static_boundaries.last().map(String::as_str)
    }

    pub fn tail_hash(&self) -> Option<&str> {
        self.message_boundaries.last().map(String::as_str)
    }
}

/// `value` as canonical JSON: members in key order, no whitespace, every `cache_control` member removed, and a
/// protocol content array whose blocks are all text blocks with no member beyond `type`, `text` and `cache_control`
/// cut to the blocks up to the last one carrying `cache_control`, a single remaining block as the string of its text,
/// so the two shapes the client sends for one message hash alike and a message's hash names what the upstream cached.
/// A protocol content array sits at one of two places alone: the `content` member of `value` itself (a message's) and
/// the `content` member of a block that is a direct item of that array (a `tool_result` block's). A `content` member
/// any deeper (inside a tool's input or a tool definition) and every other array keep their shape, so a tool input
/// holding text blocks never hashes as one holding their text.
pub fn canonical(value: &Value, out: &mut Vec<u8>) {
    write_canonical(value, out, Place::Entry);
}

/// Where a value sits below the hashed entry, in depth order, which bounds where a content array collapses.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    /// The hashed entry itself.
    Entry,
    /// The value of the entry's `content` member.
    EntryContent,
    /// A block that is a direct item of the entry's content array.
    Block,
    /// The value of such a block's `content` member.
    BlockContent,
    /// Anywhere deeper than the places above.
    Deeper,
}

/// `canonical` over `value` sitting at `place`.
fn write_canonical(value: &Value, out: &mut Vec<u8>, place: Place) {
    match value {
        Value::Object(map) => {
            out.push(b'{');
            let mut members: Vec<(&str, &Value)> = map
                .iter()
                .map(|(key, member)| (key.as_str(), member))
                .filter(|(key, _)| *key != "cache_control")
                .collect();
            members.sort_unstable_by_key(|(key, _)| *key);
            for (i, (key, member)) in members.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                serde_json::to_writer(&mut *out, key).expect("a JSON key always encodes");
                out.push(b':');
                let below = match (place, key) {
                    (Place::Entry, "content") => Place::EntryContent,
                    (Place::Block, "content") => Place::BlockContent,
                    _ => Place::Deeper,
                };
                write_canonical(member, out, below);
            }
            out.push(b'}');
        }
        Value::Array(items) => {
            let kept = match place {
                Place::EntryContent | Place::BlockContent => kept_text_blocks(items),
                _ => None,
            };
            let items = match kept {
                Some([only]) => return write_canonical(&only["text"], out, Place::Deeper),
                Some(kept) => kept,
                None => items.as_slice(),
            };
            let below = if place == Place::EntryContent {
                Place::Block
            } else {
                Place::Deeper
            };
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(item, out, below);
            }
            out.push(b']');
        }
        scalar => serde_json::to_writer(&mut *out, scalar).expect("a JSON scalar always encodes"),
    }
}

/// The blocks a text-only content array keeps, read alone for an array at one of the two places `canonical` names,
/// the entry's `content` or the `content` of a direct item of that array, and never deeper: those up to the last one
/// carrying `cache_control`, every block where none does. `None` where a block is not a text block or carries another
/// member, so the array keeps its shape.
fn kept_text_blocks(items: &[Value]) -> Option<&[Value]> {
    if items.is_empty() {
        return None;
    }
    let text_only = items.iter().all(|item| {
        item.as_object().is_some_and(|block| {
            block.get("type").and_then(Value::as_str) == Some("text")
                && block.get("text").is_some_and(Value::is_string)
                && block
                    .keys()
                    .all(|key| matches!(key.as_str(), "type" | "text" | "cache_control"))
        })
    });
    if !text_only {
        return None;
    }
    // The client appends its per-turn steering text after the marked block and drops it from the next request, and
    // the marker bounds what the upstream cached, so the blocks past the last marker are no part of the message's
    // cached prefix.
    let end = items
        .iter()
        .rposition(|item| item.get("cache_control").is_some())
        .map_or(items.len(), |last| last + 1);
    Some(&items[..end])
}

/// One link of a chain: the digest of the previous boundary's bytes and the block's canonical form, plus the
/// canonical byte length the static chain's promised-size estimate needs, so a caller never canonicalizes the
/// same block twice. `scratch` holds the canonical form, cleared first, so one buffer serves a whole chain.
fn boundary(previous: &Link, block: &Value, scratch: &mut Vec<u8>) -> (Link, u64) {
    scratch.clear();
    canonical(block, scratch);
    let mut hasher = Sha256::new();
    hasher.update(previous);
    hasher.update(&*scratch);
    (hasher.finalize(), scratch.len() as u64)
}

/// `system`'s blocks in the order the static chain hashes them: a string is one block, an array's own blocks in
/// order with the attribution line dropped wherever it sits, an absent or otherwise-shaped member none at all.
fn system_blocks(system: Option<&Value>) -> Vec<&Value> {
    match system {
        Some(s @ Value::String(_)) => vec![s],
        Some(Value::Array(blocks)) => blocks.iter().filter(|b| !is_attribution_line(b)).collect(),
        _ => Vec::new(),
    }
}

/// Whether a unit of the static chain carries a marker where the upstream reads one: on an entry of the tools array,
/// or on the system block itself.
fn carries_marker(unit: &Value) -> bool {
    match unit {
        Value::Array(tools) => tools.iter().any(|tool| tool.get("cache_control").is_some()),
        block => block.get("cache_control").is_some(),
    }
}

fn is_attribution_line(block: &Value) -> bool {
    block
        .get("text")
        .and_then(Value::as_str)
        .is_some_and(|t| t.starts_with(ATTRIBUTION_PREFIX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read(body: Value) -> Chains {
        Chains::read(&body).expect("a JSON object always yields a chain")
    }

    #[test]
    fn cache_control_is_dropped_at_every_level_so_a_marker_move_changes_no_boundary() {
        let plain = read(json!({
            "system": [{"type": "text", "text": "be helpful"}],
            "tools": [{"name": "a", "input_schema": {"type": "object"}}],
            "messages": [{"role": "user", "content": "hi"}],
        }));
        let marked = read(json!({
            "system": [{"type": "text", "text": "be helpful", "cache_control": {"type": "ephemeral"}}],
            "tools": [{"name": "a", "input_schema": {"type": "object", "cache_control": {"type": "ephemeral"}}}],
            "messages": [{"role": "user", "content": "hi", "cache_control": {"type": "ephemeral"}}],
        }));
        // The one member that reads where the markers stand.
        let marked = Chains {
            static_marked: None,
            ..marked
        };
        assert_eq!(plain, marked);
    }

    #[test]
    fn the_attribution_line_is_skipped_wherever_it_sits() {
        let line = json!({
            "type": "text",
            "text": "x-anthropic-billing-header: cc_version=2.1.269; cc_entrypoint=cli;",
        });
        let block = json!({"type": "text", "text": "be helpful"});
        let no_line = read(json!({"system": [block.clone()]}));
        let line_first = read(json!({"system": [line.clone(), block.clone()]}));
        let line_last = read(json!({"system": [block.clone(), line.clone()]}));
        assert_eq!(no_line, line_first);
        assert_eq!(no_line, line_last);
    }

    #[test]
    fn a_tools_change_moves_every_static_boundary_and_no_message_boundary() {
        let base = json!({
            "system": [{"type": "text", "text": "be helpful"}],
            "tools": [{"name": "a"}],
            "messages": [{"role": "user", "content": "hi"}],
        });
        let changed = json!({
            "system": [{"type": "text", "text": "be helpful"}],
            "tools": [{"name": "b"}],
            "messages": [{"role": "user", "content": "hi"}],
        });
        let a = read(base);
        let b = read(changed);
        assert_eq!(a.static_boundaries.len(), b.static_boundaries.len());
        assert!(
            a.static_boundaries
                .iter()
                .zip(b.static_boundaries.iter())
                .all(|(x, y)| x != y),
            "every static boundary moves, not only the first"
        );
        assert_eq!(a.message_boundaries, b.message_boundaries);
    }

    #[test]
    fn a_message_that_differs_moves_every_later_boundary_even_when_a_later_message_repeats() {
        let shared = json!({"role": "assistant", "content": "hello"});
        let a = read(json!({"messages": [{"role": "user", "content": "hi"}, shared.clone()]}));
        let b = read(json!({"messages": [{"role": "user", "content": "hey"}, shared.clone()]}));
        assert_ne!(a.message_boundaries[0], b.message_boundaries[0]);
        assert_ne!(
            a.message_boundaries[1], b.message_boundaries[1],
            "boundary 1 must move too, since it chains over boundary 0 and not just message 1's own content"
        );
    }

    /// `value` canonicalized as the value of a message's `content` member.
    fn canon(value: Value) -> Vec<u8> {
        let mut out = Vec::new();
        write_canonical(&value, &mut out, Place::EntryContent);
        out
    }

    fn marked(text: &str) -> Value {
        json!({"type": "text", "text": text, "cache_control": {"type": "ephemeral"}})
    }

    fn unmarked(text: &str) -> Value {
        json!({"type": "text", "text": text})
    }

    /// Request N whose last message is `last`, and request N + 1 carrying `resent` at that index, an answer, and a new
    /// last message with the marker, as the client sends the pair.
    fn the_pair(last: Value, resent: Value) -> (Chains, Chains) {
        let first = json!({"role": "user", "content": "hi"});
        let answer = json!({"role": "assistant", "content": "hello"});
        let n = read(json!({"messages": [first.clone(), answer.clone(), last]}));
        let next = read(json!({"messages": [
            first,
            answer,
            resent,
            {"role": "assistant", "content": "done"},
            {"role": "user", "content": [marked("b"), unmarked("steer")]},
        ]}));
        (n, next)
    }

    #[test]
    fn a_marked_block_and_its_steering_text_hash_as_the_string_the_next_request_sends() {
        let (n, next) = the_pair(
            json!({"role": "user", "content": [marked("a"), unmarked("steer")]}),
            json!({"role": "user", "content": "a"}),
        );
        let last = n.message_boundaries.last().unwrap();
        assert!(
            next.message_boundaries.contains(last),
            "request N + 1 holds request N's last boundary"
        );
        assert_eq!(next.message_boundaries[2], *last);
    }

    #[test]
    fn a_marked_one_block_message_hashes_as_the_string_the_next_request_sends() {
        let (n, next) = the_pair(
            json!({"role": "user", "content": [marked("a")]}),
            json!({"role": "user", "content": "a"}),
        );
        assert!(next
            .message_boundaries
            .contains(n.message_boundaries.last().unwrap()));
    }

    #[test]
    fn a_marked_first_block_followed_by_two_unmarked_blocks_hashes_as_its_string() {
        assert_eq!(
            canon(json!([marked("a"), unmarked("steer"), unmarked("more")])),
            canon(json!("a"))
        );
    }

    #[test]
    fn two_marked_blocks_followed_by_an_unmarked_one_hash_as_the_two_blocks_without_markers() {
        assert_eq!(
            canon(json!([marked("a"), marked("b"), unmarked("steer")])),
            canon(json!([unmarked("a"), unmarked("b")]))
        );
    }

    #[test]
    fn the_clients_first_message_keeps_its_nine_blocks_since_its_last_block_carries_the_marker() {
        let mut blocks: Vec<Value> = (1..=8)
            .map(|i| unmarked(&format!("<system-reminder>{i}</system-reminder>")))
            .collect();
        blocks.push(marked("hi"));
        let message = canon(json!(blocks));
        assert_ne!(
            message,
            canon(json!("<system-reminder>1</system-reminder>"))
        );
        assert_ne!(message, canon(json!("hi")));
        assert_eq!(message.first(), Some(&b'['));
    }

    #[test]
    fn an_unmarked_block_before_a_marked_one_keeps_the_array_apart_from_either_string() {
        let message = canon(json!([unmarked("a"), marked("b")]));
        assert_ne!(message, canon(json!("a")));
        assert_ne!(message, canon(json!("b")));
    }

    #[test]
    fn a_block_with_another_member_keeps_the_pair_apart() {
        let (n, next) = the_pair(
            json!({"role": "user", "content": [
                {"type": "text", "text": "a", "citations": [], "cache_control": {"type": "ephemeral"}},
                unmarked("steer"),
            ]}),
            json!({"role": "user", "content": "a"}),
        );
        assert!(!next
            .message_boundaries
            .contains(n.message_boundaries.last().unwrap()));
    }

    #[test]
    fn a_non_text_block_among_the_blocks_keeps_the_arrays_shape() {
        let image = json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA=="}});
        let message = canon(json!([marked("a"), image.clone(), unmarked("steer")]));
        assert_eq!(
            message,
            canon(json!([unmarked("a"), image, unmarked("steer")]))
        );
        assert_ne!(message, canon(json!("a")));
    }

    fn tool_call(input: Value) -> Chains {
        read(json!({"messages": [{
            "role": "assistant",
            "content": [{"type": "tool_use", "id": "t1", "name": "x", "input": input}],
        }]}))
    }

    #[test]
    fn a_text_block_array_in_a_tool_input_hashes_apart_from_its_text() {
        assert_ne!(
            tool_call(json!([unmarked("x")])).message_boundaries,
            tool_call(json!("x")).message_boundaries
        );
        assert_ne!(
            tool_call(json!({"payload": [marked("x")]})).message_boundaries,
            tool_call(json!({"payload": "x"})).message_boundaries
        );
    }

    #[test]
    fn a_content_member_in_a_tool_input_keeps_its_text_block_array_apart_from_its_text() {
        assert_ne!(
            tool_call(json!({"content": [unmarked("x")]})).message_boundaries,
            tool_call(json!({"content": "x"})).message_boundaries
        );
    }

    #[test]
    fn a_tool_results_one_text_block_content_hashes_as_its_string() {
        let result = |content: Value| {
            read(json!({"messages": [{
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "t1", "content": content}],
            }]}))
        };
        assert_eq!(
            result(json!([marked("ok")])).message_boundaries,
            result(json!("ok")).message_boundaries
        );
    }

    #[test]
    fn a_marker_on_a_tool_results_own_block_marks_its_message_and_an_unmarked_message_stays_unmarked(
    ) {
        let chains = read(json!({"messages": [
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": [marked("ok")]}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t2", "content": [unmarked("ok")]}]},
        ]}));
        assert_eq!(chains.marked[..], [0]);
    }

    #[test]
    fn an_empty_content_array_stays_an_empty_array() {
        assert_eq!(canon(json!([])), b"[]".to_vec());
    }

    #[test]
    fn two_bodies_parsed_from_strings_with_different_key_orders_hash_alike() {
        let a: Value =
            serde_json::from_str(r#"{"tools":[{"name":"x","input_schema":{"type":"object"}}]}"#)
                .unwrap();
        let b: Value =
            serde_json::from_str(r#"{"tools":[{"input_schema":{"type":"object"},"name":"x"}]}"#)
                .unwrap();
        assert_eq!(read(a), read(b));
    }

    #[test]
    fn a_tool_round_adds_two_message_boundaries_and_a_content_block_adds_none() {
        let base = vec![json!({"role": "user", "content": "hi"})];
        let one = read(json!({"messages": base.clone()}));
        let mut with_round = base.clone();
        with_round.push(json!({
            "role": "assistant",
            "content": [{"type": "tool_use", "id": "t1", "name": "x", "input": {}}],
        }));
        with_round.push(json!({
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}],
        }));
        let three = read(json!({"messages": with_round.clone()}));
        assert_eq!(
            three.message_boundaries.len(),
            one.message_boundaries.len() + 2
        );
        let mut extended = with_round.clone();
        let last = extended.last_mut().unwrap();
        last["content"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "text", "text": "noted"}));
        let extended = read(json!({"messages": extended}));
        assert_eq!(
            extended.message_boundaries.len(),
            three.message_boundaries.len()
        );
    }

    #[test]
    fn a_string_system_is_one_block_and_an_absent_tools_member_hashes_as_an_empty_array() {
        let string_system = read(json!({"system": "be helpful"}));
        assert_eq!(
            string_system.static_boundaries.len(),
            2,
            "the tools unit plus the one string block"
        );
        let explicit_empty_tools = read(json!({"tools": [], "system": "be helpful"}));
        assert_eq!(
            string_system.static_boundaries, explicit_empty_tools.static_boundaries,
            "an absent tools member hashes exactly as an empty array"
        );
    }

    #[test]
    fn the_boundaries_of_a_prefix_are_a_prefix_of_the_boundaries() {
        let first = json!({"role": "user", "content": "hi"});
        let second = json!({"role": "assistant", "content": "hello"});
        let one = read(json!({"messages": [first.clone()]}));
        let two = read(json!({"messages": [first.clone(), second.clone()]}));
        assert_eq!(
            two.message_boundaries[..one.message_boundaries.len()],
            one.message_boundaries[..]
        );
    }

    #[test]
    fn an_unparseable_body_has_no_chain() {
        assert_eq!(Chains::read(&json!(["not", "an", "object"])), None);
        assert_eq!(Chains::read(&json!("just a string")), None);
        assert_eq!(Chains::read(&json!(42)), None);
        assert_eq!(Chains::read(&json!(null)), None);
    }

    #[test]
    fn the_deepest_marker_on_a_tool_or_a_system_block_names_its_boundary_and_a_top_level_one_names_none(
    ) {
        let system = |first: Value, second: Value| json!([first, second]);
        let on_tool = read(json!({
            "tools": [{"name": "a", "cache_control": {"type": "ephemeral"}}],
            "system": system(unmarked("a"), unmarked("b")),
        }));
        let on_first = read(json!({"system": system(marked("a"), unmarked("b"))}));
        let on_second = read(json!({"system": system(unmarked("a"), marked("b"))}));
        let on_both = read(json!({"system": system(marked("a"), marked("b"))}));
        let top_level = read(json!({
            "system": system(unmarked("a"), unmarked("b")),
            "cache_control": {"type": "ephemeral"},
        }));
        assert_eq!(
            (
                on_tool.static_marked,
                on_first.static_marked,
                on_second.static_marked,
                on_both.static_marked,
                top_level.static_marked
            ),
            (Some(0), Some(1), Some(2), Some(2), None),
            "the tools boundary stands at 0 ahead of the two system blocks"
        );
    }

    #[test]
    fn the_static_sizes_grow_with_the_blocks_and_the_empty_tools_boundary_is_zero() {
        let no_tools = read(json!({"system": [{"type": "text", "text": "be helpful"}]}));
        assert_eq!(
            no_tools.static_sizes[0], 0,
            "an empty tools array's canonical bytes are too short to be even one token"
        );
        assert_eq!(
            no_tools.static_sizes.len(),
            no_tools.static_boundaries.len()
        );
        assert!(
            no_tools.static_sizes[1] > no_tools.static_sizes[0],
            "the system block adds to the running count"
        );
        let with_tools = read(json!({
            "tools": [{"name": "a", "input_schema": {"type": "object"}}],
            "system": [
                {"type": "text", "text": "be helpful"},
                {"type": "text", "text": "be very precise and thorough about it"},
            ],
        }));
        assert!(with_tools.static_sizes[0] > 0, "a real tools array counts");
        assert!(
            with_tools.static_sizes[1] > with_tools.static_sizes[0]
                && with_tools.static_sizes[2] > with_tools.static_sizes[1],
            "each boundary's size is cumulative over the ones before it: {:?}",
            with_tools.static_sizes
        );
    }
}
