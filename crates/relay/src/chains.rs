//! The two cumulative boundary-hash chains a request body reads as facts: a static chain over the tools array
//! and the system blocks, and a message chain with one boundary per entry of `messages`. Both travel with the
//! observation and the selection, so a warm pin's continuation and a keepalive's guard read the same hashes.

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Seeds the two chains so they never collide: an MCP server joining mid-session changes the tools array and
/// every static boundary, and leaves every message boundary equal, which only holds when the two chains start
/// from different bytes.
const STATIC_SEED: &[u8] = b"phoebis-static-chain-v1";
const MESSAGE_SEED: &[u8] = b"phoebis-message-chain-v1";
const ATTRIBUTION_PREFIX: &str = "x-anthropic-billing-header: ";

/// The two cumulative boundary-hash chains of a request body, lowercase hex, computed on the client's bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Chains {
    /// The tools array as one unit, then each system block in order, the attribution line skipped; the last
    /// entry is the request's static hash.
    pub static_boundaries: Vec<String>,
    /// One boundary per entry of `messages`, on a seed of its own.
    pub message_boundaries: Vec<String>,
    /// Parallel to `static_boundaries`: the estimated token size of the blocks up to and including each
    /// boundary, the canonical bytes the chain hashed over four; 0 at the empty-tools boundary of a request
    /// without tools.
    pub static_sizes: Vec<u64>,
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
        let mut previous = Sha256::digest(STATIC_SEED).to_vec();
        let (next, len) = boundary(&previous, tools);
        previous = next;
        running_bytes += len;
        static_boundaries.push(hex(&previous));
        static_sizes.push(running_bytes / 4);
        for block in system_blocks(root.get("system")) {
            let (next, len) = boundary(&previous, block);
            previous = next;
            running_bytes += len;
            static_boundaries.push(hex(&previous));
            static_sizes.push(running_bytes / 4);
        }
        let mut message_boundaries = Vec::new();
        let mut previous = Sha256::digest(MESSAGE_SEED).to_vec();
        if let Some(Value::Array(messages)) = root.get("messages") {
            for message in messages {
                previous = boundary(&previous, message).0;
                message_boundaries.push(hex(&previous));
            }
        }
        Some(Chains {
            static_boundaries,
            message_boundaries,
            static_sizes,
        })
    }

    pub fn static_hash(&self) -> Option<&str> {
        self.static_boundaries.last().map(String::as_str)
    }

    pub fn tail_hash(&self) -> Option<&str> {
        self.message_boundaries.last().map(String::as_str)
    }
}

/// `value` as canonical JSON: members in key order, no whitespace, every `cache_control` member removed.
pub fn canonical(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Object(map) => {
            out.push(b'{');
            let mut keys: Vec<&str> = map
                .keys()
                .map(String::as_str)
                .filter(|k| *k != "cache_control")
                .collect();
            keys.sort_unstable();
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                serde_json::to_writer(&mut *out, key).expect("a JSON key always encodes");
                out.push(b':');
                canonical(&map[*key], out);
            }
            out.push(b'}');
        }
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                canonical(item, out);
            }
            out.push(b']');
        }
        scalar => serde_json::to_writer(&mut *out, scalar).expect("a JSON scalar always encodes"),
    }
}

/// One link of a chain: the digest of the previous boundary's bytes and the block's canonical form, plus the
/// canonical byte length the static chain's promised-size estimate needs, so a caller never canonicalizes the
/// same block twice.
fn boundary(previous: &[u8], block: &Value) -> (Vec<u8>, u64) {
    let mut bytes = Vec::new();
    canonical(block, &mut bytes);
    let mut hasher = Sha256::new();
    hasher.update(previous);
    hasher.update(&bytes);
    (hasher.finalize().to_vec(), bytes.len() as u64)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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
                .zip(&b.static_boundaries)
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
