//! What a probe replays: the request body with its output controls zeroed and its `safeguards` member cut, the headers
//! the bus published, and the TTL the body asks for. The output-control exclusions are the Go engine's.

use phoebis_cch::Stamp;
use serde_json::Value;

use crate::config::{FIVE_MINUTES_MS, ONE_HOUR_MS};

pub const MAX_HEADER_VALUE: usize = 8_192;
pub const MAX_HEADER_BYTES: usize = 16_384;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recipe {
    pub body: Vec<u8>,
    pub headers: Vec<(String, String)>,
    pub model: String,
    /// What the body's own markers ask for; 0 when it carries none, and the response decides.
    pub ttl_ms: i64,
    /// The longest TTL a marker in the body asks for, 0 with none: a probe that finds the prefix cold writes each
    /// marked part at its own TTL, so one hour marker anywhere means the probe can write for an hour.
    pub longest_ttl_ms: i64,
    pub size: usize,
    /// The observation's own two hashes, stamped by the caller: `make_recipe` always leaves them `None`, since
    /// it never sees the request facts they come from.
    pub static_hash: Option<String>,
    pub tail_hash: Option<String>,
    /// The body carries `speed: "fast"`: the replay runs fast and settles in dollars.
    pub fast: bool,
}

pub fn valid_id(s: &str) -> bool {
    (1..=256).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':' | b'/'))
}

pub fn valid_opaque(s: &str) -> bool {
    !s.trim().is_empty() && s.len() <= 1_024 && !s.chars().any(char::is_control)
}

/// Whether every object in the document, at any depth, names each key once. A parsed tree keeps the last value
/// of a repeated key and cannot show the repeat, so this walks the raw bytes once; the upstream reads one of
/// the two values and the replay might edit the other, which is why an ambiguous body is refused whole.
fn unique_keys_deep(raw: &[u8]) -> bool {
    use serde::de::{DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
    struct Unique;
    impl<'de> Visitor<'de> for Unique {
        type Value = ();
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a document whose objects name each key once")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
            let mut keys = std::collections::HashSet::new();
            while let Some(key) = map.next_key::<String>()? {
                if !keys.insert(key) {
                    return Err(serde::de::Error::custom("duplicate key"));
                }
                map.next_value_seed(UniqueSeed)?;
            }
            Ok(())
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
            while seq.next_element_seed(UniqueSeed)?.is_some() {}
            Ok(())
        }
        fn visit_bool<E>(self, _: bool) -> Result<(), E> {
            Ok(())
        }
        fn visit_i64<E>(self, _: i64) -> Result<(), E> {
            Ok(())
        }
        fn visit_u64<E>(self, _: u64) -> Result<(), E> {
            Ok(())
        }
        fn visit_f64<E>(self, _: f64) -> Result<(), E> {
            Ok(())
        }
        fn visit_str<E>(self, _: &str) -> Result<(), E> {
            Ok(())
        }
        fn visit_unit<E>(self) -> Result<(), E> {
            Ok(())
        }
        fn visit_none<E>(self) -> Result<(), E> {
            Ok(())
        }
    }
    struct UniqueSeed;
    impl<'de> DeserializeSeed<'de> for UniqueSeed {
        type Value = ();
        fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
            d.deserialize_any(Unique)
        }
    }
    let mut de = serde_json::Deserializer::from_slice(raw);
    UniqueSeed.deserialize(&mut de).is_ok() && de.end().is_ok()
}

pub fn make_recipe(
    headers: &[(String, String)],
    body: &[u8],
    model: &str,
    limit: usize,
) -> Result<Recipe, &'static str> {
    if body.len() > limit {
        return Err("request_too_large");
    }
    let root: Value = serde_json::from_slice(body).map_err(|_| "invalid_request")?;
    // One raw walk for repeated keys at every depth, which covers the top level and the four output-control
    // objects the Go engine checked one by one.
    if !root.is_object() || !unique_keys_deep(body) {
        return Err("invalid_request");
    }
    let messages = root.get("messages").and_then(Value::as_array);
    if messages.is_none_or(|m| m.is_empty())
        || root.get("max_tokens").and_then(Value::as_u64).unwrap_or(0) == 0
    {
        return Err("unsupported_shape");
    }
    // The edits below and the restamp read raw keys and values; a member the decoded walk finds and the raw edits
    // cannot read (an escaped key, a stamp `find_cch` cannot parse, a `stream` that is no boolean) would gain a
    // second `stream` beside it or keep a stale hash, so such a body is refused rather than retained. Read after the
    // shape check, so a `max_tokens` that is no positive number stays an unsupported shape.
    let decoded =
        |name: &str| phoebis_cch::top_level_member_count(body, name).map_err(|_| "invalid_request");
    let stamp = phoebis_cch::stamp(body);
    let stamp_hidden = phoebis_cch::decoded_stamp_count(body)
        > usize::from(matches!(stamp, Stamp::Placeholder(_) | Stamp::Hash(_)));
    let stream_hidden = decoded("stream")?
        > usize::from(phoebis_cch::bool_member_span(body, &["stream"]).is_some());
    // Read off the client's bytes, which the copy edited below still holds unchanged when the span is used.
    let max_tokens = phoebis_cch::number_member_span(body, &["max_tokens"]);
    let max_tokens_hidden = decoded("max_tokens")? > usize::from(max_tokens.is_some());
    if stamp_hidden || stream_hidden || max_tokens_hidden {
        return Err("escaped_key");
    }
    for key in ["cache_control", "output_config", "thinking", "tool_choice"] {
        if root.get(key).is_some_and(|field| !field.is_object()) {
            return Err("unsupported_shape");
        }
    }
    // A reference to account-scoped state belongs to the account that served the request, while a probe
    // follows a pin a sibling's request may have moved since; the relay checks portability against the pin it
    // finds, so such a body must never be retained in the first place.
    if !phoebis_relay::portable_of(&root) {
        return Err("unsupported_shape");
    }
    let thinking = root
        .pointer("/thinking/type")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !matches!(thinking, "" | "adaptive" | "disabled") {
        return Err("fixed_thinking");
    }
    let fast = root.get("speed").and_then(Value::as_str) == Some("fast");
    let choice = root
        .pointer("/tool_choice/type")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !matches!(choice, "" | "auto" | "none") {
        return Err("forced_tool");
    }
    if root.pointer("/output_config/format").is_some() || root.get("output_format").is_some() {
        return Err("structured_output");
    }
    let strict = root
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| {
            tools
                .iter()
                .any(|t| t.get("strict").and_then(Value::as_bool) == Some(true))
        });
    if strict {
        return Err("structured_output");
    }
    let (ttl_ms, longest_ttl_ms) = marker_ttls(&root).ok_or("invalid_cache_control")?;
    if !valid_opaque(model) {
        return Err("invalid_model");
    }
    // The relay's own rule for what counts as a client hash, so a probe is re-signed exactly where its request was.
    let client_hash = matches!(stamp, Stamp::Hash(_));
    let mut edited = body.to_vec();
    let (start, end) = max_tokens.ok_or("invalid_request")?;
    edited.splice(start..end, b"0".iter().copied());
    match phoebis_cch::bool_member_span(&edited, &["stream"]) {
        Some((start, end)) => {
            edited.splice(start..end, b"false".iter().copied());
        }
        // No flag means a JSON answer already, but the replay says so explicitly, as the Go engine did.
        None => {
            let at = edited
                .iter()
                .position(|b| *b == b'{')
                .ok_or("invalid_request")?
                + 1;
            edited.splice(at..at, b"\"stream\":false,".iter().copied());
        }
    }
    // A retained body carries the turn's own pending tool use in its `safeguards` member: a probe replaying it
    // would ask for a verdict on an action already decided, whatever the pool's setting says. The decoded key
    // count tells an absent member from one the raw walk cannot reach (an escaped spelling) or would leave behind
    // (a second one), and neither is retained.
    let spellings = phoebis_cch::top_level_member_count(&edited, "safeguards")
        .map_err(|_| "invalid_request")?;
    let cut = phoebis_cch::remove_top_level_member(&mut edited, "safeguards").is_some();
    if spellings > 1 || (spellings == 1 && !cut) {
        return Err("invalid_request");
    }
    // `stream` is inside the billing hash view and `max_tokens` outside it: a client-stamped body is re-signed
    // over the edited bytes, since the relay would refuse the client's hash as drift. The body is the one the bus
    // published, the client's bytes before any edit, so a placeholder here is one the client sent, and it stays:
    // the relay relays a client-sent placeholder unsigned whatever `proxy.sign_cch_placeholder` says, and a probe
    // signed here would travel signed where the request it replays went out unsigned. A reduced-shape line
    // carries no stamp to touch, since the relay restores its fields on the request's way out, after the bus
    // published the body, and never on a probe's; a body without a stamp is left alone.
    if client_hash {
        phoebis_cch::restamp_if_present(&mut edited).map_err(|_| "invalid_request")?;
    }
    // Everything the bus published, in the order it published it: the denylist there is the one gate, so a
    // header the client starts sending reaches the upstream on a probe as it does on the request. The relay's
    // outbound builder places the account credential after `anthropic-version`, so this order is the wire's.
    let mut kept: Vec<(String, String)> = Vec::new();
    // The budget is the retained bytes beyond the body, and the model name is among them.
    let mut header_bytes = model.len();
    for (name, value) in headers {
        if value.len() > MAX_HEADER_VALUE {
            return Err("headers_too_large");
        }
        kept.push((name.clone(), value.clone()));
        header_bytes += name.len() + value.len();
        if header_bytes > MAX_HEADER_BYTES {
            return Err("headers_too_large");
        }
    }
    let size = edited.len() + header_bytes;
    Ok(Recipe {
        body: edited,
        headers: kept,
        model: model.to_string(),
        ttl_ms,
        longest_ttl_ms,
        size,
        static_hash: None,
        tail_hash: None,
        fast,
    })
}

/// The shortest TTL any cache marker in the body asks for: `Some(0)` when there is none, `None` when a marker
/// is not an ephemeral one with a known TTL.
pub fn request_ttl_ms(root: &Value) -> Option<i64> {
    marker_ttls(root).map(|(shortest, _)| shortest)
}

/// The shortest and the longest TTL the body's cache markers ask for, `Some((0, 0))` with none and `None` when a
/// marker is not an ephemeral one with a known TTL. The markers are the ones the relay's TTL rewrite edits, so a
/// recipe's TTLs and the rewrite read one set of positions.
fn marker_ttls(root: &Value) -> Option<(i64, i64)> {
    let (mut shortest, mut longest) = (0i64, 0i64);
    let markers = root
        .as_object()
        .map(phoebis_relay::schema_cache_controls)
        .unwrap_or_default();
    for marker in markers {
        if marker.get("type").and_then(Value::as_str) != Some("ephemeral") {
            return None;
        }
        let this = match marker.get("ttl").and_then(Value::as_str).unwrap_or("") {
            "" | "5m" => FIVE_MINUTES_MS,
            "1h" => ONE_HOUR_MS,
            _ => return None,
        };
        if shortest == 0 || this < shortest {
            shortest = this;
        }
        longest = longest.max(this);
    }
    Some((shortest, longest))
}
