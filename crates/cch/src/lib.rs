//! The Claude Code billing-header hash (`cch=xxxxx;`) that must be recomputed after any body edit.

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
            if !s.eat(b'{') {
                return None;
            }
            loop {
                s.ws();
                let (ks, ke) = s.string().ok()?;
                s.ws();
                if !s.eat(b':') {
                    return None;
                }
                s.ws();
                if s.body[ks + 1..ke - 1] == *key.as_bytes() {
                    break;
                }
                s.value(false).ok()?;
                s.ws();
                if !s.eat(b',') {
                    return None;
                }
            }
        }
        if last {
            return Some(true);
        }
        s.ws();
    }
    Some(false)
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
        if s.body[ks + 1..ke - 1] == *key.as_bytes() {
            s.value(false).ok()?;
            return Some((member_start, s.pos));
        }
        s.value(false).ok()?;
        s.ws();
        if !s.eat(b',') {
            return None;
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
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
}
