//! What a probe replays: the request body with its output controls zeroed, the headers the bus published, and
//! the TTL the body asks for. The body exclusions are the Go engine's.

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
    pub size: usize,
}

pub fn valid_id(s: &str) -> bool {
    (1..=256).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':' | b'/'))
}

pub fn valid_opaque(s: &str) -> bool {
    !s.trim().is_empty() && s.len() <= 1_024 && !s.chars().any(char::is_control)
}

fn header_size(headers: &[(String, String)]) -> usize {
    headers.iter().map(|(n, v)| n.len() + v.len()).sum()
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
    for key in ["cache_control", "output_config", "thinking", "tool_choice"] {
        if root.get(key).is_some_and(|field| !field.is_object()) {
            return Err("unsupported_shape");
        }
    }
    let thinking = root
        .pointer("/thinking/type")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !matches!(thinking, "" | "adaptive" | "disabled") {
        return Err("fixed_thinking");
    }
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
    let ttl_ms = request_ttl_ms(&root).ok_or("invalid_cache_control")?;
    if !valid_opaque(model) {
        return Err("invalid_model");
    }
    let mut edited = body.to_vec();
    let (start, end) =
        phoebis_cch::number_member_span(&edited, &["max_tokens"]).ok_or("invalid_request")?;
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
    // `stream` is inside the billing hash view and `max_tokens` outside it: the client's stamp no longer
    // matches the edited body, and the relay would refuse it as drift. A body without a stamp is left alone.
    phoebis_cch::restamp_if_present(&mut edited).map_err(|_| "invalid_request")?;
    // Everything the bus published, in the order it published it: the denylist there is the one gate, so a
    // header the client starts sending reaches the upstream on a probe as it does on the request. The relay's
    // outbound builder places the account credential after `anthropic-version`, so this order is the wire's.
    let mut kept: Vec<(String, String)> = Vec::new();
    for (name, value) in headers {
        if value.len() > MAX_HEADER_VALUE {
            return Err("headers_too_large");
        }
        kept.push((name.clone(), value.clone()));
        // The budget is the retained bytes beyond the body, and the model name is among them.
        if model.len() + header_size(&kept) > MAX_HEADER_BYTES {
            return Err("headers_too_large");
        }
    }
    let size = edited.len() + model.len() + header_size(&kept);
    Ok(Recipe {
        body: edited,
        headers: kept,
        model: model.to_string(),
        ttl_ms,
        size,
    })
}

/// The shortest TTL any cache marker in the body asks for: `Some(0)` when there is none, `None` when a marker
/// is not an ephemeral one with a known TTL.
pub fn request_ttl_ms(root: &Value) -> Option<i64> {
    let mut ttl = 0i64;
    let mut visit = |marker: Option<&Value>| -> bool {
        let Some(marker) = marker else { return true };
        if marker.get("type").and_then(Value::as_str) != Some("ephemeral") {
            return false;
        }
        let this = match marker.get("ttl").and_then(Value::as_str).unwrap_or("") {
            "" | "5m" => FIVE_MINUTES_MS,
            "1h" => ONE_HOUR_MS,
            _ => return false,
        };
        if ttl == 0 || this < ttl {
            ttl = this;
        }
        true
    };
    if !visit(root.get("cache_control")) {
        return None;
    }
    for field in ["system", "tools"] {
        for item in root
            .get(field)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if !visit(item.get("cache_control")) {
                return None;
            }
        }
    }
    for message in root
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for block in message
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if !visit(block.get("cache_control")) {
                return None;
            }
        }
    }
    Some(ttl)
}
