//! The Claude Code billing-header hash (`cch=xxxxx;`) that must be recomputed after any body edit.

use std::borrow::Cow;

use xxhash_rust::xxh64::xxh64;

pub const SEED: u64 = 0x4D65_9218_E32A_3268;
const LEN: usize = 5;

/// How deep the scanner follows nesting. The scanners recurse, and this runs on an authenticated but still
/// caller-controlled body, so without a bound a few thousand nested arrays end the process rather than the
/// request. Real Messages bodies nest a handful of levels.
const MAX_DEPTH: usize = 128;

#[derive(Debug, thiserror::Error)]
pub enum CchError {
    #[error("body is not valid JSON at byte {0}")]
    Json(usize),
    #[error("overlapping normalization edit at byte {0}")]
    Overlap(usize),
    #[error("body nests deeper than the scanner reads at byte {0}")]
    TooDeep(usize),
}

pub struct CchLocation {
    pub digits_start: usize,
}

/// Finds the five digits after `cch=` inside the top-level `system[0].text`, when that text is a billing header.
///
/// The span comes from the scanner, not from a search for the text: a tool-use input can carry its own `"system"`
/// member holding a string that matches the billing header, and it can sit earlier in the body than the real one.
pub fn find_cch(body: &[u8]) -> Option<CchLocation> {
    let (start, end) = string_member_span(body, &["system", "0", "text"])?;
    let raw = &body[start..end];
    if !raw.starts_with(b"\"x-anthropic-billing-header:") {
        return None;
    }
    let mut from = 0;
    while let Some(rel) = find(&raw[from..], b"cch=") {
        let digits = from + rel + 4;
        if digits + LEN < raw.len()
            && raw[digits + LEN] == b';'
            && raw[digits..digits + LEN]
                .iter()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
        {
            return Some(CchLocation {
                digits_start: start + digits,
            });
        }
        from += rel + 4;
    }
    None
}

/// The digits the client writes after `cch=` wherever its native patch does not run.
const PLACEHOLDER: &[u8; LEN] = b"00000";

/// Whether the billing line's one and only `cch=` field is the client's unsigned placeholder, which claims no
/// hash to verify or re-sign.
pub fn carries_only_the_placeholder(body: &[u8]) -> bool {
    let Some(location) = find_cch(body) else {
        return false;
    };
    if body[location.digits_start..location.digits_start + LEN] != *PLACEHOLDER {
        return false;
    }
    // Beside a second `cch=`, which `find_cch` may have skipped or not reached, the placeholder vouches for
    // nothing: the line claims a hash. Counted over the decoded line, so a field spelled with an escape counts as
    // the server reads it, and a line that does not decode is not taken for the placeholder.
    string_member_span(body, &["system", "0", "text"])
        .and_then(|(start, end)| serde_json::from_slice::<String>(&body[start..end]).ok())
        .is_some_and(|line| line.matches("cch=").count() == 1)
}

/// The array and object walk every span helper needs before reading its own terminal value; kept in one place
/// so a fix to array indexing or member matching (an escaped key, say) lands once instead of once per helper.
/// `Some(true)` once `s` sits at the last segment's value, `Some(false)` for an empty path, `None` on mismatch.
fn walk_to_last_segment(s: &mut Scanner<'_>, path: &[&str]) -> Option<bool> {
    s.ws();
    for (depth, key) in path.iter().enumerate() {
        let last = depth + 1 == path.len();
        if let Ok(index) = key.parse::<usize>() {
            if !s.eat(b'[') {
                return None;
            }
            for _ in 0..index {
                s.value(false).ok()?;
                s.ws();
                if !s.eat(b',') {
                    return None;
                }
            }
            s.ws();
        } else {
            scan_object(s, |member| member == key.as_bytes())?;
        }
        if last {
            return Some(true);
        }
        s.ws();
    }
    Some(false)
}

/// Walks the object at `s`'s current position (its opening `{` not yet eaten), calling `matches` with each
/// member's key bytes in turn. Stops on the first member `matches` answers true for, `s` left positioned right
/// after that member's colon, its value not yet read; returns the key's opening quote. `None` on a syntax
/// error, or an object that closes with no match. The one object-member scan every span helper walks through,
/// so a fix to key matching lands once.
fn scan_object(s: &mut Scanner<'_>, mut matches: impl FnMut(&[u8]) -> bool) -> Option<usize> {
    if !s.eat(b'{') {
        return None;
    }
    loop {
        s.ws();
        let member_start = s.pos;
        let (ks, ke) = s.string().ok()?;
        s.ws();
        if !s.eat(b':') {
            return None;
        }
        s.ws();
        if matches(&s.body[ks + 1..ke - 1]) {
            return Some(member_start);
        }
        s.value(false).ok()?;
        s.ws();
        if !s.eat(b',') {
            return None;
        }
    }
}

/// Hands `visit` each top-level member's key in body order, decoded, until `visit` answers true. Keys are
/// decoded before they are compared, so an escaped spelling of a name counts as that name: a parser upstream
/// would read it that way, and the span helpers above match raw bytes on purpose (they edit what they find and
/// never a decoded copy). Values are skipped by the scanner the span helpers walk, so a body a strict decoder
/// refuses as a whole (a lone surrogate escape in a string value) is still read to its end.
fn visit_top_level_keys(body: &[u8], mut visit: impl FnMut(&str) -> bool) -> Result<(), CchError> {
    let mut s = Scanner {
        body,
        pos: 0,
        depth: 0,
        edits: Vec::new(),
    };
    s.ws();
    if !s.eat(b'{') {
        return Err(CchError::Json(s.pos));
    }
    loop {
        s.ws();
        if s.eat(b'}') {
            return Ok(());
        }
        let (ks, ke) = s.string()?;
        let key: String = serde_json::from_slice(&body[ks..ke]).map_err(|_| CchError::Json(ks))?;
        if visit(&key) {
            return Ok(());
        }
        s.ws();
        if !s.eat(b':') {
            return Err(CchError::Json(s.pos));
        }
        s.value(false)?;
        s.ws();
        if s.eat(b',') {
            continue;
        }
        if s.eat(b'}') {
            return Ok(());
        }
        return Err(CchError::Json(s.pos));
    }
}

/// The first of `names` whose decoded key appears more than once among the top-level object's members.
pub fn repeated_top_level_member(
    body: &[u8],
    names: &[&'static str],
) -> Result<Option<&'static str>, CchError> {
    let mut seen: Vec<&'static str> = Vec::new();
    let mut repeated = None;
    visit_top_level_keys(body, |key| {
        let Some(name) = names.iter().copied().find(|n| *n == key) else {
            return false;
        };
        if seen.contains(&name) {
            repeated = Some(name);
            return true;
        }
        seen.push(name);
        false
    })?;
    Ok(repeated)
}

/// How many of the top-level object's members carry `name` once their keys are decoded.
pub fn top_level_member_count(body: &[u8], name: &str) -> Result<usize, CchError> {
    let mut count = 0;
    visit_top_level_keys(body, |key| {
        count += usize::from(key == name);
        false
    })?;
    Ok(count)
}

/// Raw span, quotes included, of the string value at `path` from the top-level object. Array indexes are decimal
/// strings. The relay uses this for its body edits so only one JSON scanner exists in the workspace.
pub fn string_member_span(body: &[u8], path: &[&str]) -> Option<(usize, usize)> {
    let mut s = Scanner {
        body,
        pos: 0,
        depth: 0,
        edits: Vec::new(),
    };
    if !walk_to_last_segment(&mut s, path)? {
        return None;
    }
    if s.body.get(s.pos) != Some(&b'"') {
        return None;
    }
    s.string().ok()
}

/// Raw span of the `true` or `false` literal at `path` from the top-level object, walking the body as
/// `string_member_span` does: a member of the same name inside a nested value is skipped with that value, so a
/// tool schema cannot stand in for the request's own flag.
pub fn bool_member_span(body: &[u8], path: &[&str]) -> Option<(usize, usize)> {
    let mut s = Scanner {
        body,
        pos: 0,
        depth: 0,
        edits: Vec::new(),
    };
    if !walk_to_last_segment(&mut s, path)? {
        return None;
    }
    let start = s.pos;
    for literal in [&b"true"[..], &b"false"[..]] {
        if s.body[start..].starts_with(literal) {
            let end = start + literal.len();
            let terminated = s
                .body
                .get(end)
                .is_none_or(|c| matches!(c, b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n'));
            return terminated.then_some((start, end));
        }
    }
    None
}

/// Raw span of the number literal at `path`, walked as `bool_member_span` walks. The keepalive replay edits
/// `max_tokens` in place, and the hash view excludes that member, so the edit must not touch anything else.
pub fn number_member_span(body: &[u8], path: &[&str]) -> Option<(usize, usize)> {
    let mut s = Scanner {
        body,
        pos: 0,
        depth: 0,
        edits: Vec::new(),
    };
    if !walk_to_last_segment(&mut s, path)? {
        return None;
    }
    let start = s.pos;
    let mut end = start;
    while end < body.len() && matches!(body[end], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E') {
        end += 1;
    }
    (end > start && body[start].is_ascii_digit()
        || body.get(start) == Some(&b'-') && end > start + 1)
        .then_some((start, end))
}

/// Raw span of element `index` inside the array at `path`, walked as `string_member_span` walks:
/// `walk_to_last_segment` positions at the array's `[`, then this skips to `index` the way an array-index path
/// segment already does, and `Scanner::value(false)` reads the element's own span. A caller deletes the span
/// itself and its trailing comma.
pub fn array_member_span(body: &[u8], path: &[&str], index: usize) -> Option<(usize, usize)> {
    let mut s = Scanner {
        body,
        pos: 0,
        depth: 0,
        edits: Vec::new(),
    };
    if !walk_to_last_segment(&mut s, path)? {
        return None;
    }
    if !s.eat(b'[') {
        return None;
    }
    for _ in 0..index {
        s.value(false).ok()?;
        s.ws();
        if !s.eat(b',') {
            return None;
        }
    }
    s.ws();
    let start = s.pos;
    s.value(false).ok()?;
    Some((start, s.pos))
}

/// The position of the closing `]` of the array at `path` when it holds no element, or `None` when it holds one
/// or `path` does not resolve to an array. `array_member_span` already covers an array with at least one
/// element; this is the empty case it has nothing to return for.
pub fn empty_array_member_end(body: &[u8], path: &[&str]) -> Option<usize> {
    let mut s = Scanner {
        body,
        pos: 0,
        depth: 0,
        edits: Vec::new(),
    };
    if !walk_to_last_segment(&mut s, path)? {
        return None;
    }
    if !s.eat(b'[') {
        return None;
    }
    s.ws();
    (s.body.get(s.pos) == Some(&b']')).then_some(s.pos)
}

/// Raw span of the whole member (key quote to value end) at `path`, so a caller can insert a sibling before it.
/// Walked as `walk_to_last_segment` walks: a decoy sharing the key's bytes inside an earlier string, or nested
/// under another object, is skipped as part of the value it sits in rather than matched.
pub fn object_member_span(body: &[u8], path: &[&str]) -> Option<(usize, usize)> {
    let mut s = Scanner {
        body,
        pos: 0,
        depth: 0,
        edits: Vec::new(),
    };
    let key = path.last()?;
    walk_to_last_segment(&mut s, &path[..path.len() - 1])?;
    let member_start = scan_object(&mut s, |member| member == key.as_bytes())?;
    s.value(false).ok()?;
    Some((member_start, s.pos))
}

/// Cuts the top-level member `key` with one comma by the hash view's rules (the following comma when something
/// follows, else the preceding one when something precedes, else neither) and answers the span it removed;
/// `None` when the walk finds no such member. Matches the raw key bytes as `object_member_span` does, so an
/// escaped spelling is not found and a second spelling survives the cut: the caller decides both.
pub fn remove_top_level_member(body: &mut Vec<u8>, key: &str) -> Option<(usize, usize)> {
    let (start, end) = object_member_span(body, &[key])?;
    let ws = |b: &u8| matches!(b, b' ' | b'\t' | b'\r' | b'\n');
    let after = body[end..].iter().position(|b| !ws(b)).map(|i| end + i);
    let (cut_start, cut_end) = match after {
        Some(i) if body[i] == b',' => (start, i + 1),
        _ => match body[..start].iter().rposition(|b| !ws(b)) {
            Some(i) if body[i] == b',' => (i, end),
            _ => (start, end),
        },
    };
    body.drain(cut_start..cut_end);
    Some((cut_start, cut_end))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Raw span of every `cache_control` value the Messages API reads: the top-level member, one per `system[]` and
/// `tools[]` entry, one per `messages[].content[]` block, and one per block inside a content block's own
/// `content` array (a `tool_result`'s blocks), in body order. One pass; a member of that name anywhere else (a
/// tool schema, a tool input, a block nested deeper than that) is skipped with the value that holds it.
pub fn cache_control_spans(body: &[u8]) -> Result<Vec<(usize, usize)>, CchError> {
    cache_control_walk(body, Keys::Raw)
}

/// How many `cache_control` members sit where `cache_control_spans` looks once every key on the way is decoded.
/// The two differ exactly where a key the Messages API reads (the marker's own, or a container's above it) is
/// spelled with an escape: a decoder reaches that marker and the raw walk, whose spans a body edit splices, does
/// not. Walked by the same scanner rather than a parse of the whole body, so a body a strict decoder refuses (a
/// lone surrogate escape in a string value) is counted like any other.
pub fn decoded_cache_control_count(body: &[u8]) -> Result<usize, CchError> {
    cache_control_walk(body, Keys::Decoded).map(|spans| spans.len())
}

/// How a walk reads a member's key.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Keys {
    /// The bytes between the quotes, as the client spelled them.
    Raw,
    /// What a decoder reads through the key's escapes.
    Decoded,
}

/// A member key as `keys` reads it. A key that does not decode names none of the members a walk looks for, all
/// plain ASCII, so it reads as no name at all.
fn key_name(body: &[u8], ks: usize, ke: usize, keys: Keys) -> Cow<'_, [u8]> {
    let raw = &body[ks + 1..ke - 1];
    if keys == Keys::Raw || !raw.contains(&b'\\') {
        return Cow::Borrowed(raw);
    }
    serde_json::from_slice::<String>(&body[ks..ke])
        .map_or(Cow::Borrowed(&[][..]), |key| Cow::Owned(key.into_bytes()))
}

fn cache_control_walk(body: &[u8], keys: Keys) -> Result<Vec<(usize, usize)>, CchError> {
    let mut s = Scanner {
        body,
        pos: 0,
        depth: 0,
        edits: Vec::new(),
    };
    let mut spans = Vec::new();
    s.ws();
    if !s.eat(b'{') {
        return Err(CchError::Json(s.pos));
    }
    s.ws();
    if s.eat(b'}') {
        return Ok(spans);
    }
    loop {
        s.ws();
        let (ks, ke) = s.string()?;
        s.ws();
        if !s.eat(b':') {
            return Err(CchError::Json(s.pos));
        }
        s.ws();
        match key_name(body, ks, ke, keys).as_ref() {
            b"cache_control" => {
                let start = s.pos;
                s.value(false)?;
                spans.push((start, s.pos));
            }
            b"system" | b"tools" => block_array(&mut s, &mut spans, false, keys)?,
            b"messages" => message_array(&mut s, &mut spans, keys)?,
            _ => s.value(false)?,
        }
        s.ws();
        if s.eat(b',') {
            continue;
        }
        if !s.eat(b'}') {
            return Err(CchError::Json(s.pos));
        }
        break;
    }
    Ok(spans)
}

/// The array behind `system` or `tools`, or a `messages[]` element's `content`: each object entry's own
/// members are scanned for `cache_control`; anything not an array is skipped whole, as a value at that position
/// with the wrong shape gives the scanner nothing to walk. `inner` says whether this array is already a block's
/// own `content`, in which case its objects' `content` is skipped whole.
fn block_array(
    s: &mut Scanner<'_>,
    spans: &mut Vec<(usize, usize)>,
    inner: bool,
    keys: Keys,
) -> Result<(), CchError> {
    s.ws();
    if s.body.get(s.pos) != Some(&b'[') {
        return s.value(false);
    }
    s.pos += 1;
    s.ws();
    if s.eat(b']') {
        return Ok(());
    }
    loop {
        s.ws();
        if s.body.get(s.pos) == Some(&b'{') {
            block_object(s, spans, inner, keys)?;
        } else {
            s.value(false)?;
        }
        s.ws();
        if s.eat(b',') {
            continue;
        }
        if !s.eat(b']') {
            return Err(CchError::Json(s.pos));
        }
        break;
    }
    Ok(())
}

/// One `system[]`, `tools[]`, or content-block object: its own `cache_control` member is recorded; its `content`
/// member, when it is not itself inside one, is walked one level as `block_array` walks `messages[].content`;
/// every other member is skipped whole.
fn block_object(
    s: &mut Scanner<'_>,
    spans: &mut Vec<(usize, usize)>,
    inner: bool,
    keys: Keys,
) -> Result<(), CchError> {
    s.pos += 1;
    s.ws();
    if s.eat(b'}') {
        return Ok(());
    }
    loop {
        s.ws();
        let (ks, ke) = s.string()?;
        s.ws();
        if !s.eat(b':') {
            return Err(CchError::Json(s.pos));
        }
        s.ws();
        let body = s.body;
        match key_name(body, ks, ke, keys).as_ref() {
            b"cache_control" => {
                let start = s.pos;
                s.value(false)?;
                spans.push((start, s.pos));
            }
            // A `tool_result` is the one block the API gives a `content` array of blocks, and its `type` may
            // follow `content` in body order, so the member is read on its name alone and one level only.
            b"content" if !inner => block_array(s, spans, true, keys)?,
            _ => s.value(false)?,
        }
        s.ws();
        if s.eat(b',') {
            continue;
        }
        if !s.eat(b'}') {
            return Err(CchError::Json(s.pos));
        }
        break;
    }
    Ok(())
}

/// `messages[]`: each object element's `content` is walked as `block_array` walks `system` and `tools`, when it
/// is an array; every other member, and a non-array `content`, is skipped whole.
fn message_array(
    s: &mut Scanner<'_>,
    spans: &mut Vec<(usize, usize)>,
    keys: Keys,
) -> Result<(), CchError> {
    s.ws();
    if s.body.get(s.pos) != Some(&b'[') {
        return s.value(false);
    }
    s.pos += 1;
    s.ws();
    if s.eat(b']') {
        return Ok(());
    }
    loop {
        s.ws();
        if s.body.get(s.pos) == Some(&b'{') {
            message_object(s, spans, keys)?;
        } else {
            s.value(false)?;
        }
        s.ws();
        if s.eat(b',') {
            continue;
        }
        if !s.eat(b']') {
            return Err(CchError::Json(s.pos));
        }
        break;
    }
    Ok(())
}

fn message_object(
    s: &mut Scanner<'_>,
    spans: &mut Vec<(usize, usize)>,
    keys: Keys,
) -> Result<(), CchError> {
    s.pos += 1;
    s.ws();
    if s.eat(b'}') {
        return Ok(());
    }
    loop {
        s.ws();
        let (ks, ke) = s.string()?;
        s.ws();
        if !s.eat(b':') {
            return Err(CchError::Json(s.pos));
        }
        s.ws();
        if key_name(s.body, ks, ke, keys).as_ref() == b"content" {
            block_array(s, spans, false, keys)?;
        } else {
            s.value(false)?;
        }
        s.ws();
        if s.eat(b',') {
            continue;
        }
        if !s.eat(b'}') {
            return Err(CchError::Json(s.pos));
        }
        break;
    }
    Ok(())
}

/// How many billing stamps sit where `find_cch` looks, `system[0].text` of the top-level object, once every key
/// on the way and the line itself are decoded, each spelling of a repeated key counted. It exceeds what the raw
/// walk reaches exactly where one of those keys or the line is spelled with an escape, or where a second spelling
/// follows the one the raw walk reads: a stamp `restamp_if_present` never rewrites. The scanner reads as far as
/// the body is well formed and counts what it passed, as `string_member_span` finds a member ahead of a syntax
/// error.
pub fn decoded_stamp_count(body: &[u8]) -> usize {
    let mut s = Scanner {
        body,
        pos: 0,
        depth: 0,
        edits: Vec::new(),
    };
    let mut count = 0;
    s.ws();
    let _ = each_decoded_member(&mut s, |s, key| {
        if key == b"system" {
            first_block_stamps(s, &mut count)
        } else {
            s.value(false)
        }
    });
    count
}

/// Walks the object at `s` (its `{` not yet eaten), handing `member` each member's decoded key with `s` at the
/// value, which `member` reads.
fn each_decoded_member<'a>(
    s: &mut Scanner<'a>,
    mut member: impl FnMut(&mut Scanner<'a>, &[u8]) -> Result<(), CchError>,
) -> Result<(), CchError> {
    if !s.eat(b'{') {
        return Err(CchError::Json(s.pos));
    }
    s.ws();
    if s.eat(b'}') {
        return Ok(());
    }
    loop {
        s.ws();
        let (ks, ke) = s.string()?;
        s.ws();
        if !s.eat(b':') {
            return Err(CchError::Json(s.pos));
        }
        s.ws();
        let body = s.body;
        member(s, key_name(body, ks, ke, Keys::Decoded).as_ref())?;
        s.ws();
        if s.eat(b',') {
            continue;
        }
        if !s.eat(b'}') {
            return Err(CchError::Json(s.pos));
        }
        return Ok(());
    }
}

/// A `system` value: when it is an array whose first element is an object, each of that object's `text` members
/// that reads as a stamp is counted; every other element and member, and a `system` of another shape, is skipped
/// whole.
fn first_block_stamps(s: &mut Scanner<'_>, count: &mut usize) -> Result<(), CchError> {
    s.ws();
    if s.body.get(s.pos) != Some(&b'[') {
        return s.value(false);
    }
    s.pos += 1;
    s.ws();
    if s.eat(b']') {
        return Ok(());
    }
    if s.body.get(s.pos) == Some(&b'{') {
        each_decoded_member(s, |s, key| {
            if key == b"text" && s.body.get(s.pos) == Some(&b'"') {
                let (start, end) = s.string()?;
                *count += usize::from(reads_as_a_stamp(&s.body[start..end]));
                Ok(())
            } else {
                s.value(false)
            }
        })?;
    } else {
        s.value(false)?;
    }
    loop {
        s.ws();
        if s.eat(b']') {
            return Ok(());
        }
        if !s.eat(b',') {
            return Err(CchError::Json(s.pos));
        }
        s.value(false)?;
    }
}

/// Whether a string token, quotes included, is a billing line claiming a hash once decoded. A token a strict
/// decoder refuses (a lone surrogate escape) is read by its bytes, as `find_cch` reads every line, so this count
/// never falls below the raw walk's.
fn reads_as_a_stamp(token: &[u8]) -> bool {
    const PREFIX: &str = "x-anthropic-billing-header:";
    match serde_json::from_slice::<String>(token) {
        Ok(line) => line.starts_with(PREFIX) && line.contains("cch="),
        Err(_) => token[1..].starts_with(PREFIX.as_bytes()) && find(token, b"cch=").is_some(),
    }
}

pub fn sign(body: &[u8]) -> Result<Vec<u8>, CchError> {
    let Some(loc) = find_cch(body) else {
        return Ok(body.to_vec());
    };
    let mut out = body.to_vec();
    out[loc.digits_start..loc.digits_start + LEN].copy_from_slice(b"00000");
    let view = normalize(&out)?;
    let h = xxh64(&view, SEED) & 0xF_FFFF;
    out[loc.digits_start..loc.digits_start + LEN].copy_from_slice(format!("{h:05x}").as_bytes());
    Ok(out)
}

pub fn restamp_if_present(body: &mut Vec<u8>) -> Result<bool, CchError> {
    if find_cch(body).is_none() {
        return Ok(false);
    }
    *body = sign(body)?;
    Ok(true)
}

struct Edit {
    start: usize,
    end: usize,
}
struct Member {
    start: usize,
    end: usize,
    comma_before: Option<usize>,
    comma_after: Option<usize>,
    excluded: bool,
}

struct Scanner<'a> {
    body: &'a [u8],
    pos: usize,
    depth: usize,
    edits: Vec<Edit>,
}

fn excluded_key(key: &[u8]) -> bool {
    matches!(
        key,
        b"\"max_tokens\"" | b"\"fallbacks\"" | b"\"fallback_credit_token\""
    )
}

/// Builds the hash view without re-serializing: model string values are emptied and dispatch-only members are cut.
pub fn normalize(body: &[u8]) -> Result<Vec<u8>, CchError> {
    serde_json::from_slice::<serde::de::IgnoredAny>(body)
        .map_err(|e| CchError::Json(e.column()))?;
    let mut s = Scanner {
        body,
        pos: 0,
        depth: 0,
        edits: Vec::new(),
    };
    s.value(true)?;
    s.ws();
    if s.pos != body.len() {
        return Err(CchError::Json(s.pos));
    }
    s.edits.sort_by_key(|e| e.start);
    let mut out = Vec::with_capacity(body.len());
    let mut last = 0;
    for e in &s.edits {
        if e.start < last || e.end > body.len() {
            return Err(CchError::Overlap(e.start));
        }
        out.extend_from_slice(&body[last..e.start]);
        last = e.end;
    }
    out.extend_from_slice(&body[last..]);
    Ok(out)
}

impl<'a> Scanner<'a> {
    fn ws(&mut self) {
        while self.pos < self.body.len()
            && matches!(self.body[self.pos], b' ' | b'\t' | b'\r' | b'\n')
        {
            self.pos += 1;
        }
    }
    fn eat(&mut self, c: u8) -> bool {
        if self.pos < self.body.len() && self.body[self.pos] == c {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn value(&mut self, collect: bool) -> Result<(), CchError> {
        self.ws();
        match self.body.get(self.pos) {
            // The one place either container is entered from, so counting here bounds the recursion for the
            // collecting and the non-collecting walk alike.
            Some(b'{') | Some(b'[') => {
                if self.depth >= MAX_DEPTH {
                    return Err(CchError::TooDeep(self.pos));
                }
                self.depth += 1;
                let scanned = if self.body[self.pos] == b'{' {
                    self.object(collect)
                } else {
                    self.array(collect)
                };
                self.depth -= 1;
                scanned
            }
            Some(b'"') => self.string().map(|_| ()),
            Some(_) => {
                let start = self.pos;
                while self.pos < self.body.len()
                    && !matches!(
                        self.body[self.pos],
                        b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n'
                    )
                {
                    self.pos += 1;
                }
                if self.pos == start {
                    Err(CchError::Json(start))
                } else {
                    Ok(())
                }
            }
            None => Err(CchError::Json(self.pos)),
        }
    }
    fn string(&mut self) -> Result<(usize, usize), CchError> {
        let start = self.pos;
        if !self.eat(b'"') {
            return Err(CchError::Json(start));
        }
        while self.pos < self.body.len() {
            match self.body[self.pos] {
                b'\\' => self.pos += 2,
                b'"' => {
                    self.pos += 1;
                    return Ok((start, self.pos));
                }
                _ => self.pos += 1,
            }
        }
        Err(CchError::Json(start))
    }
    fn array(&mut self, collect: bool) -> Result<(), CchError> {
        self.pos += 1;
        self.ws();
        if self.eat(b']') {
            return Ok(());
        }
        loop {
            self.value(collect)?;
            self.ws();
            if self.eat(b',') {
                continue;
            }
            if !self.eat(b']') {
                return Err(CchError::Json(self.pos));
            }
            return Ok(());
        }
    }
    fn object(&mut self, collect: bool) -> Result<(), CchError> {
        self.pos += 1;
        self.ws();
        if self.eat(b'}') {
            return Ok(());
        }
        let mut members: Vec<Member> = Vec::new();
        let mut comma_before = None;
        loop {
            self.ws();
            let member_start = self.pos;
            let (ks, ke) = self.string()?;
            self.ws();
            if !self.eat(b':') {
                return Err(CchError::Json(self.pos));
            }
            self.ws();
            let key = &self.body[ks..ke];
            let excluded = collect && excluded_key(key);
            if collect && key == b"\"model\"" && self.body.get(self.pos) == Some(&b'"') {
                let (vs, ve) = self.string()?;
                self.edits.push(Edit {
                    start: vs + 1,
                    end: ve - 1,
                });
            } else {
                self.value(collect && !excluded)?;
            }
            let member_end = self.pos;
            self.ws();
            let comma_after = if self.eat(b',') {
                Some(self.pos - 1)
            } else {
                None
            };
            members.push(Member {
                start: member_start,
                end: member_end,
                comma_before,
                comma_after,
                excluded,
            });
            if let Some(c) = comma_after {
                comma_before = Some(c);
                continue;
            }
            if !self.eat(b'}') {
                return Err(CchError::Json(self.pos));
            }
            break;
        }
        if collect {
            self.excluded_edits(&members);
        }
        Ok(())
    }
    /// A run of excluded members is cut with one comma: the following comma when something follows, else the
    /// preceding comma when something precedes, else nothing (the reference's exact rules).
    fn excluded_edits(&mut self, members: &[Member]) {
        let mut start = 0;
        while start < members.len() {
            if !members[start].excluded {
                start += 1;
                continue;
            }
            let mut end = start;
            while end + 1 < members.len() && members[end + 1].excluded {
                end += 1;
            }
            let edit = if end + 1 < members.len() {
                Edit {
                    start: members[start].start,
                    end: members[end]
                        .comma_after
                        .expect("followed member has a comma")
                        + 1,
                }
            } else if start > 0 && end > start {
                Edit {
                    start: members[start].start,
                    end: members[end].end,
                }
            } else if start > 0 {
                Edit {
                    start: members[start]
                        .comma_before
                        .expect("preceded member has a comma"),
                    end: members[end].end,
                }
            } else {
                Edit {
                    start: members[start].start,
                    end: members[end].end,
                }
            };
            self.edits.push(edit);
            start = end + 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The billing block a first-party client stamps, which is what makes `sign` walk the whole body.
    const BILLING: &str = r#""system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cc_entrypoint=sdk-cli; cch=00000;"}]"#;

    /// A body whose `model` sits behind `depth` nested arrays, with the billing block ahead of them so the
    /// scanner reaches the nesting rather than stopping short of it.
    fn nested(depth: usize) -> Vec<u8> {
        format!(
            r#"{{{BILLING},"deep":{}1{},"model":"model-a"}}"#,
            "[".repeat(depth),
            "]".repeat(depth)
        )
        .into_bytes()
    }

    #[test]
    fn a_body_nested_past_the_bound_is_refused_rather_than_overflowing_the_stack() {
        let body = nested(10_000);
        assert!(matches!(sign(&body), Err(CchError::TooDeep(_))));
        assert_eq!(
            string_member_span(&body, &["model"]),
            None,
            "the span scanner gives up at the same depth instead of recursing"
        );
    }

    /// The same body with the nesting inside `fallbacks`, a member the hash view cuts, so no depth of arrays
    /// under it can reach the signature.
    fn nested_in_an_excluded_member(depth: usize) -> Vec<u8> {
        format!(
            r#"{{{BILLING},"fallbacks":{}1{},"model":"model-a"}}"#,
            "[".repeat(depth),
            "]".repeat(depth)
        )
        .into_bytes()
    }

    fn digits(signed: &[u8]) -> &[u8] {
        let at = find_cch(signed)
            .expect("the billing block carries a hash")
            .digits_start;
        &signed[at..at + LEN]
    }

    #[test]
    fn a_body_within_the_bound_still_scans() {
        let body = nested(100);
        let signed = sign(&body).expect("a hundred levels is a body, not an attack");
        assert_eq!(signed.len(), body.len());
        let (start, end) = string_member_span(&body, &["model"]).expect("the model is readable");
        assert_eq!(&body[start..end], br#""model-a""#);

        // Counting depth must not move what the scan produces. These two differ only inside `fallbacks`,
        // which the hash view cuts whole, so the digits have to agree; a deep walk that lost its place would
        // cut the wrong span and sign something else.
        let deep = sign(&nested_in_an_excluded_member(100)).expect("depth alone is not a refusal");
        let shallow = sign(&nested_in_an_excluded_member(1)).expect("depth alone is not a refusal");
        assert_ne!(
            digits(&deep),
            b"00000".as_slice(),
            "the placeholder was replaced, so the comparison below is not two unsigned bodies"
        );
        assert_eq!(digits(&deep), digits(&shallow));
    }

    #[test]
    fn a_boolean_member_is_found_at_the_top_level_and_not_inside_a_nested_value() {
        let body = br#"{"model":"m","tools":[{"input_schema":{"properties":{"stream":{"default":false}}}}],"stream": true}"#;
        let (start, end) = bool_member_span(body, &["stream"]).unwrap();
        assert_eq!(
            &body[start..end],
            b"true",
            "the nested stream member is skipped as part of the tools value"
        );
        assert_eq!(
            bool_member_span(br#"{"stream":false,"model":"m"}"#, &["stream"]).map(|(s, e)| s..e),
            Some(1 + 9..1 + 14)
        );
        assert!(
            bool_member_span(br#"{"stream":"true"}"#, &["stream"]).is_none(),
            "a string is not a boolean"
        );
        assert!(bool_member_span(br#"{"model":"m"}"#, &["stream"]).is_none());
        assert!(
            bool_member_span(br#"{"metadata":{"stream":true}}"#, &["metadata", "stream"]).is_some()
        );
    }

    #[test]
    fn an_array_element_span_covers_nested_objects_and_strings_holding_brackets() {
        let body = br#"{"system":[{"type":"text","text":"[not] {a} bracket"},{"type":"text","text":"kept","meta":{"tags":["a]b","c}d"]}}],"model":"m"}"#;
        let (start, end) = array_member_span(body, &["system"], 0).unwrap();
        assert_eq!(
            &body[start..end],
            br#"{"type":"text","text":"[not] {a} bracket"}"#,
            "a bracket inside a string does not end the element early"
        );
        let (start, end) = array_member_span(body, &["system"], 1).unwrap();
        assert_eq!(
            &body[start..end],
            br#"{"type":"text","text":"kept","meta":{"tags":["a]b","c}d"]}}"#,
            "a nested object and array are walked by depth, not skipped"
        );
        assert!(
            array_member_span(body, &["system"], 2).is_none(),
            "past the last element"
        );
    }

    #[test]
    fn an_object_member_span_covers_the_key_and_the_value_past_a_decoy_and_a_nested_match() {
        let body = br#"{"outer":{"stream":"decoy"},"diagnostics":{"previous_message_id":null},"stream":true}"#;
        let (start, end) = object_member_span(body, &["diagnostics"]).unwrap();
        assert_eq!(
            &body[start..end],
            br#""diagnostics":{"previous_message_id":null}"#,
            "the nested decoy under \"outer\" is skipped as part of its value"
        );
        let (start, end) = object_member_span(body, &["outer", "stream"]).unwrap();
        assert_eq!(&body[start..end], br#""stream":"decoy""#);
        assert!(object_member_span(body, &["missing"]).is_none());

        let string_decoy =
            br#"{"note":"the diagnostics field matters","diagnostics":{"previous_message_id":"x"}}"#;
        let (start, end) = object_member_span(string_decoy, &["diagnostics"]).unwrap();
        assert_eq!(
            &string_decoy[start..end],
            br#""diagnostics":{"previous_message_id":"x"}"#,
            "the key's bytes sitting inside an earlier string value is not a match"
        );
    }

    #[test]
    fn removing_a_top_level_member_cuts_it_with_one_comma_wherever_it_sits() {
        let cut = |body: &[u8]| {
            let mut body = body.to_vec();
            let span = remove_top_level_member(&mut body, "safeguards");
            (String::from_utf8(body).unwrap(), span)
        };
        assert_eq!(
            cut(br#"{"safeguards":[],"model":"m"}"#),
            (r#"{"model":"m"}"#.into(), Some((1, 17))),
            "first: the following comma goes"
        );
        assert_eq!(
            cut(br#"{"model":"m","safeguards":[]}"#),
            (r#"{"model":"m"}"#.into(), Some((12, 28))),
            "last: the preceding comma goes"
        );
        assert_eq!(
            cut(br#"{"model":"m","safeguards":[],"stream":true}"#),
            (r#"{"model":"m","stream":true}"#.into(), Some((13, 29))),
            "middle: the following comma goes"
        );
        assert_eq!(
            cut(br#"{"safeguards":[]}"#),
            ("{}".into(), Some((1, 16))),
            "alone: no comma to take"
        );
        assert_eq!(
            cut(b"{ \"model\":\"m\" ,\n \"safeguards\" : [ ] ,\t\"stream\":true }").0,
            "{ \"model\":\"m\" ,\n \t\"stream\":true }",
            "whitespace before the following comma is cut with it"
        );
        assert_eq!(
            cut(b"{\"model\":\"m\" ,\r\n \"safeguards\":[] }").0,
            "{\"model\":\"m\"  }",
            "whitespace after the preceding comma is cut with it"
        );
    }

    #[test]
    fn removing_a_top_level_member_skips_a_decoy_in_a_string_and_one_nested_under_another_object() {
        let mut body = br#"{"note":"\"safeguards\":[]","outer":{"safeguards":[1]},"safeguards":[2],"stream":true}"#.to_vec();
        assert!(remove_top_level_member(&mut body, "safeguards").is_some());
        assert_eq!(
            String::from_utf8(body).unwrap(),
            r#"{"note":"\"safeguards\":[]","outer":{"safeguards":[1]},"stream":true}"#
        );
    }

    #[test]
    fn removing_an_absent_member_answers_none_and_leaves_the_bytes() {
        let before = br#"{"model":"m","outer":{"safeguards":[]}}"#.to_vec();
        let mut body = before.clone();
        assert_eq!(remove_top_level_member(&mut body, "safeguards"), None);
        assert_eq!(body, before);
    }

    #[test]
    fn a_second_top_level_spelling_survives_the_first_cut() {
        let mut body = br#"{"safeguards":[1],"model":"m","safeguards":[2]}"#.to_vec();
        assert!(remove_top_level_member(&mut body, "safeguards").is_some());
        assert_eq!(
            String::from_utf8(body.clone()).unwrap(),
            r#"{"model":"m","safeguards":[2]}"#,
            "the caller decides what a second spelling means"
        );
        assert_eq!(
            repeated_top_level_member(
                br#"{"safeguards":[1],"model":"m","safeguards":[2]}"#,
                &["safeguards"]
            )
            .unwrap(),
            Some("safeguards")
        );
    }

    #[test]
    fn an_empty_array_end_is_found_by_depth_past_an_earlier_decoy_and_nowhere_for_a_non_empty_one()
    {
        let decoy = br#"{"messages":[{"role":"system","content":"x"}],"system":[]}"#;
        let (start, end) = string_member_span(decoy, &["messages", "0", "role"]).unwrap();
        assert_eq!(
            &decoy[start..end],
            br#""system""#,
            "the decoy really does carry the same bytes"
        );
        let at = empty_array_member_end(decoy, &["system"]).unwrap();
        assert_eq!(
            &decoy[at..],
            b"]}",
            "the top-level array's own bracket, not the decoy's"
        );
        assert!(
            empty_array_member_end(br#"{"system":[1]}"#, &["system"]).is_none(),
            "an array holding an element is not empty"
        );
        assert!(
            empty_array_member_end(br#"{"system":"x"}"#, &["system"]).is_none(),
            "a string is not an array"
        );
    }

    #[test]
    fn cache_control_spans_finds_every_wire_position_and_skips_a_tool_schema_decoy() {
        let body = br#"{"cache_control":{"type":"ephemeral"},"system":[{"type":"text","text":"s","cache_control":{"type":"ephemeral","ttl":"1h"}}],"tools":[{"name":"t","input_schema":{"properties":{"cache_control":{"type":"ephemeral"}}},"cache_control":{"type":"ephemeral"}}],"messages":[{"role":"user","content":[{"type":"text","text":"m","cache_control":{"type":"ephemeral"}}]}],"model":"m"}"#;
        let spans = cache_control_spans(body).unwrap();
        let texts: Vec<&[u8]> = spans.iter().map(|(s, e)| &body[*s..*e]).collect();
        assert_eq!(
            texts,
            vec![
                br#"{"type":"ephemeral"}"#.as_slice(),
                br#"{"type":"ephemeral","ttl":"1h"}"#.as_slice(),
                br#"{"type":"ephemeral"}"#.as_slice(),
                br#"{"type":"ephemeral"}"#.as_slice(),
            ],
            "the tool's own marker is found, the one inside its input_schema is skipped with the schema"
        );
    }

    #[test]
    fn the_decoded_marker_count_reaches_an_escaped_key_the_raw_walk_misses() {
        let escaped_marker =
            br#"{"system":[{"type":"text","text":"s","cache_contr\u006fl":{"type":"ephemeral"}}]}"#;
        assert_eq!(decoded_cache_control_count(escaped_marker).unwrap(), 1);
        assert!(cache_control_spans(escaped_marker).unwrap().is_empty());

        let escaped_container =
            br#"{"sy\u0073tem":[{"type":"text","text":"s","cache_control":{"type":"ephemeral"}}]}"#;
        assert_eq!(decoded_cache_control_count(escaped_container).unwrap(), 1);
        assert!(cache_control_spans(escaped_container).unwrap().is_empty());

        let schema = br#"{"tools":[{"name":"t","input_schema":{"properties":{"cache_contr\u006fl":{}}}}],"messages":[{"role":"user","content":[{"type":"text","text":"\udc00","cache_control":{"type":"ephemeral"}}]}]}"#;
        assert_eq!(
            decoded_cache_control_count(schema).unwrap(),
            1,
            "a tool schema's property is the client's data, and a lone surrogate in a value is walked past"
        );
        assert_eq!(cache_control_spans(schema).unwrap().len(), 1);
    }

    #[test]
    fn the_decoded_stamp_count_reaches_a_stamp_the_raw_walk_misses() {
        const LINE: &str =
            "x-anthropic-billing-header: cc_version=2.1.220.test; cc_entrypoint=sdk-cli; cch=00000;";
        let body = |template: &str| template.replace("LINE", LINE).into_bytes();

        let plain = body(r#"{"system":[{"type":"text","text":"LINE"}],"model":"m"}"#);
        assert_eq!(decoded_stamp_count(&plain), 1);
        assert!(find_cch(&plain).is_some());

        for missed in [
            r#"{"sy\u0073tem":[{"type":"text","text":"LINE"}],"model":"m"}"#,
            r#"{"system":[{"type":"text","te\u0078t":"LINE"}],"model":"m"}"#,
            r#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cc_entrypoint=sdk-cli; cch\u003d00000;"}],"model":"m"}"#,
            r#"{"system":[{"type":"text","text":"s","te\u0078t":"LINE"}],"model":"m"}"#,
            r#"{"system":[{"type":"text","text":"s"}],"model":"m","system":[{"type":"text","text":"LINE"}]}"#,
        ] {
            let missed = body(missed);
            let shown = String::from_utf8_lossy(&missed).into_owned();
            assert!(find_cch(&missed).is_none(), "{shown}");
            assert_eq!(decoded_stamp_count(&missed), 1, "{shown}");
        }

        let both =
            body(r#"{"system":[{"type":"text","text":"LINE","te\u0078t":"LINE"}],"model":"m"}"#);
        assert!(find_cch(&both).is_some());
        assert_eq!(
            decoded_stamp_count(&both),
            2,
            "a second spelling after the one the raw walk reads"
        );

        for elsewhere in [
            r#"{"system":[{"type":"text","text":"s"},{"type":"text","text":"LINE"}],"model":"m"}"#,
            r#"{"tools":[{"name":"t","input_schema":{"system":[{"text":"LINE"}]}}],"model":"m"}"#,
            r#"{"system":"LINE","model":"m"}"#,
            r#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_entrypoint=sdk-cli;"}]}"#,
        ] {
            let elsewhere = body(elsewhere);
            assert_eq!(
                decoded_stamp_count(&elsewhere),
                0,
                "{}",
                String::from_utf8_lossy(&elsewhere)
            );
        }

        let surrogate = body(r#"{"system":[{"type":"text","text":"LINE \udc00"}],"model":"m"}"#);
        assert!(serde_json::from_slice::<serde_json::Value>(&surrogate).is_err());
        assert!(find_cch(&surrogate).is_some());
        assert_eq!(
            decoded_stamp_count(&surrogate),
            1,
            "a line a strict decoder refuses is read by its bytes, as the raw walk reads it"
        );
    }

    #[test]
    fn a_placeholder_beside_an_escaped_second_cch_field_is_not_the_placeholder_alone() {
        let body = br#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cch=00000; c\u0063h=12345;"}],"model":"m"}"#;
        assert_eq!(
            String::from_utf8_lossy(body).matches("cch=").count(),
            1,
            "the raw line spells its second field with an escape"
        );
        assert!(!carries_only_the_placeholder(body));
    }

    #[test]
    fn a_placeholder_on_a_line_that_does_not_decode_is_not_the_placeholder_alone() {
        // A lone surrogate escape: the scanner walks it, a strict decoder refuses the line.
        let body = br#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cch=00000; \ud800"}],"model":"m"}"#;
        assert!(
            find_cch(body).is_some(),
            "the raw walk reads the placeholder"
        );
        assert!(!carries_only_the_placeholder(body));
    }

    #[test]
    fn cache_control_spans_walks_one_level_into_a_tool_results_content_and_no_further() {
        let body = br#"{"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"a","cache_control":{"type":"ephemeral"}},{"type":"text","text":"b"}],"cache_control":{"type":"ephemeral","ttl":"1h"}},{"type":"text","text":"c","content":[{"type":"text","content":[{"cache_control":{"type":"ephemeral"}}]}]}]}],"model":"m"}"#;
        let spans = cache_control_spans(body).unwrap();
        let texts: Vec<&[u8]> = spans.iter().map(|(s, e)| &body[*s..*e]).collect();
        assert_eq!(
            texts,
            vec![
                br#"{"type":"ephemeral"}"#.as_slice(),
                br#"{"type":"ephemeral","ttl":"1h"}"#.as_slice(),
            ],
            "the marker inside the tool result is found ahead of the block's own; one two levels down is skipped"
        );
    }

    #[test]
    fn a_raw_repeat_of_a_top_level_member_is_found() {
        let body = br#"{"model":"a","messages":[],"model":"b"}"#;
        assert_eq!(
            repeated_top_level_member(body, &["metadata", "model", "stream"]).unwrap(),
            Some("model")
        );
    }

    #[test]
    fn an_escaped_repeat_of_a_top_level_member_is_found_by_its_decoded_key() {
        let body = "{\"model\":\"a\",\"messages\":[],\"\\u006dodel\":\"b\"}".as_bytes();
        assert_eq!(
            repeated_top_level_member(body, &["metadata", "model", "stream"]).unwrap(),
            Some("model")
        );
    }

    #[test]
    fn a_top_level_key_is_counted_by_its_decoded_spelling() {
        assert_eq!(
            top_level_member_count(br#"{"model":"m","safeguards":[]}"#, "safeguards").unwrap(),
            1
        );
        assert_eq!(
            top_level_member_count(br#"{"model":"m","safeg\u0075ards":[]}"#, "safeguards").unwrap(),
            1,
            "an escaped spelling counts as the name"
        );
        assert_eq!(
            top_level_member_count(
                br#"{"safeguards":[1],"model":"m","safeguards":[2]}"#,
                "safeguards"
            )
            .unwrap(),
            2
        );
    }

    #[test]
    fn a_repeat_inside_a_nested_object_is_not_a_top_level_repeat() {
        let body = br#"{"model":"a","metadata":{"model":"x","model":"y"},"messages":[]}"#;
        assert_eq!(
            repeated_top_level_member(body, &["metadata", "model", "stream"]).unwrap(),
            None
        );
    }

    #[test]
    fn a_body_whose_top_level_is_an_array_is_refused() {
        let body = br#"[{"model":"a"}]"#;
        assert!(repeated_top_level_member(body, &["metadata", "model", "stream"]).is_err());
    }

    #[test]
    fn a_single_occurrence_of_each_named_member_is_not_a_repeat() {
        let body = br#"{"metadata":{"user_id":"u"},"model":"a","stream":true,"messages":[]}"#;
        assert_eq!(
            repeated_top_level_member(body, &["metadata", "model", "stream"]).unwrap(),
            None
        );
    }
}
