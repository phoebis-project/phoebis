//! The Claude Code billing-header hash (`cch=xxxxx;`) that must be recomputed after any body edit.

use std::borrow::Cow;

use xxhash_rust::xxh64::xxh64;

pub const SEED: u64 = 0x4D65_9218_E32A_3268;
const LEN: usize = 5;

/// How deep the scanner follows nesting. The scanners recurse, and this runs on an authenticated but still
/// caller-controlled body, so without a bound a few thousand nested arrays end the process rather than the
/// request. Real Messages bodies nest a handful of levels. Counted with the root, as serde's recursion limit
/// counts it, so admission refuses what the guardrails' `parse_body` cannot read.
const MAX_DEPTH: usize = 128;

/// The four bytes RFC 8259 allows as whitespace between tokens, for every walk that skips it.
pub const JSON_WS: &[u8] = b" \t\r\n";

/// Opening quote and prefix of the billing block as it appears in `system[0].text`.
pub const BILLING_BLOCK: &[u8] = br#""x-anthropic-billing-header:"#;

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
    /// The span of the billing line's JSON string, quotes included, so a reader of the line needs no second walk.
    pub line: (usize, usize),
}

/// What the billing line in `system[0].text` stamps, read in one raw walk, so every caller judges a stamp by one rule.
pub enum Stamp {
    /// No billing line, or one that holds no `cch=`.
    None,
    /// A billing line holding a `cch=` whose digits `find_cch` cannot read.
    Unreadable,
    /// The client's unsigned placeholder as the line's only `cch=`, which claims no hash to verify or re-sign.
    Placeholder(CchLocation),
    /// Every other readable stamp: a hash to verify and re-sign.
    Hash(CchLocation),
}

/// Finds the five digits after `cch=` inside the top-level `system[0].text`, when that text is a billing header.
///
/// The span comes from the scanner, not from a search for the text: a tool-use input can carry its own `"system"`
/// member holding a string that matches the billing header, and it can sit earlier in the body than the real one.
pub fn find_cch(body: &[u8]) -> Option<CchLocation> {
    let (line, digits_start) = billing_line(body, string_member_span(body, &LINE_PATH))?;
    Some(CchLocation {
        digits_start: digits_start?,
        line,
    })
}

/// Where the billing line sits, `system[0].text`.
const LINE_PATH: [&str; 3] = ["system", "0", "text"];

/// The billing line's span, quotes included, and where the digits of its first readable `cch=` field start, `None`
/// where `text`, the span of the string at `system[0].text`, is absent or no billing line.
fn billing_line(
    body: &[u8],
    text: Option<(usize, usize)>,
) -> Option<((usize, usize), Option<usize>)> {
    let (start, end) = text?;
    let raw = &body[start..end];
    if !raw.starts_with(BILLING_BLOCK) {
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
            return Some(((start, end), Some(start + digits)));
        }
        from += rel + 4;
    }
    Some(((start, end), None))
}

pub fn stamp(body: &[u8]) -> Stamp {
    stamp_in(body, string_member_span(body, &LINE_PATH))
}

/// The stamp of the billing line at `text`, the span of the string at `system[0].text`.
fn stamp_in(body: &[u8], text: Option<(usize, usize)>) -> Stamp {
    let Some((line, digits_start)) = billing_line(body, text) else {
        return Stamp::None;
    };
    let Some(digits_start) = digits_start else {
        return if find(&body[line.0..line.1], b"cch=").is_some() {
            Stamp::Unreadable
        } else {
            Stamp::None
        };
    };
    let location = CchLocation { digits_start, line };
    if placeholder_alone(body, &location) {
        Stamp::Placeholder(location)
    } else {
        Stamp::Hash(location)
    }
}

/// The digits the client writes after `cch=` wherever its native patch does not run.
const PLACEHOLDER: &[u8; LEN] = b"00000";

fn placeholder_alone(body: &[u8], location: &CchLocation) -> bool {
    if body[location.digits_start..location.digits_start + LEN] != *PLACEHOLDER {
        return false;
    }
    // Beside a second `cch=`, which `find_cch` may have skipped or not reached, the placeholder vouches for
    // nothing: the line claims a hash. Counted over the decoded line, so a field spelled with an escape counts as
    // the server reads it, and a line that does not decode is not taken for the placeholder.
    let (start, end) = location.line;
    serde_json::from_slice::<String>(&body[start..end])
        .is_ok_and(|line| line.matches("cch=").count() == 1)
}

/// The array and object walk every span helper needs before reading its own terminal value; kept in one place
/// so a fix to array indexing or member matching (an escaped key, say) lands once instead of once per helper.
/// `Some(true)` once `s` sits at the last segment's value, `Some(false)` for an empty path, `None` on mismatch.
fn walk_to_last_segment(s: &mut Scanner<'_>, path: &[&str]) -> Option<bool> {
    s.ws();
    for (depth, key) in path.iter().enumerate() {
        let last = depth + 1 == path.len();
        if let Ok(index) = key.parse::<usize>() {
            enter_element(s, index)?;
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

/// Steps into the array at `s` and past its first `index` elements, `s` left at the element's value. The one
/// array-index step of the path walk and the span helpers that read inside an array.
fn enter_element(s: &mut Scanner<'_>, index: usize) -> Option<()> {
    if s.body.get(s.pos) != Some(&b'[') {
        return None;
    }
    // Counted until the scanner is dropped, since the rest of the path and the caller's read stand inside this
    // array.
    s.enter().ok()?;
    s.pos += 1;
    for _ in 0..index {
        s.value(false).ok()?;
        s.ws();
        if !s.eat(b',') {
            return None;
        }
    }
    s.ws();
    Some(())
}

/// Walks the object at `s`'s current position (its opening `{` not yet eaten), calling `matches` with each
/// member's key bytes in turn. Stops on the first member `matches` answers true for, `s` left positioned right
/// after that member's colon, its value not yet read. Returns the key's opening quote. `None` on a syntax
/// error, or an object that closes with no match. The one object-member scan every span helper walks through,
/// so a fix to key matching lands once. The object counts as one level for as long as the scanner stands inside
/// it, as `sign`'s walk counts it, so it stays counted on a match while the caller reads the member's value, and a
/// span helper gives up on a member at the depth `sign` refuses.
fn scan_object(s: &mut Scanner<'_>, mut matches: impl FnMut(&[u8]) -> bool) -> Option<usize> {
    if s.body.get(s.pos) != Some(&b'{') {
        return None;
    }
    s.enter().ok()?;
    s.pos += 1;
    s.ws();
    if s.eat(b'}') {
        s.leave();
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
            s.leave();
            return None;
        }
    }
}

/// The top-level members of a body, each lookup walking the top-level object itself in body order. No table of the
/// members is kept: a body at the request limit made of the shortest members holds millions of them, and a table
/// of them would outgrow the bytes the body budget reserves for the body many times over, so a lookup holds one
/// scanner and the member under it. A member is read once its key is, its value's start once its colon is, and a
/// walk ends where the object closes or at the error that stopped it. The decoded lookups compare keys decoded, so
/// an escaped spelling of a name counts as that name, as a parser upstream would read it. The raw lookups match a
/// key's own bytes and stop at the first match, as the span helpers do on purpose (they edit what they find and
/// never a decoded copy), and read the matched value with the reader the span helper of that name uses. Values are
/// skipped by the scanner the span helpers walk, so a body a strict decoder refuses as a whole (a lone surrogate
/// escape in a string value) is still read to its end.
pub struct TopLevelMembers<'a> {
    body: &'a [u8],
}

struct TopLevelMember<'a> {
    /// The key's span, quotes included.
    key: (usize, usize),
    /// The key's decoded text, `None` where a decoder refuses the key.
    decoded: Option<Cow<'a, str>>,
    /// Where the value starts, `None` where the walk stopped ahead of the member's colon.
    value_start: Option<usize>,
    /// Where the value ends, `None` where the walk stopped ahead of the value's end.
    value_end: Option<usize>,
}

/// Where the walk of the top-level object ended.
enum TopLevelEnd {
    /// At the object's closing brace.
    Closed,
    /// At a closing brace right after a comma, at this offset. The decoded count and the repeat check read it as the
    /// object's end, and the lookups that walk into a member's value refuse it there, as their member walk refuses it
    /// inside any object.
    ClosedAfterAComma(usize),
    /// At the error that stopped the walk, its kind kept, since admission refuses a body nested deeper than the
    /// scanner reads apart from one that is no JSON.
    Stopped(CchError),
}

impl<'a> TopLevelMembers<'a> {
    pub fn read(body: &'a [u8]) -> Self {
        TopLevelMembers { body }
    }

    pub fn body(&self) -> &'a [u8] {
        self.body
    }

    /// How many top-level members carry `name` once their keys are decoded.
    pub fn top_level_member_count(&self, name: &str) -> Result<usize, CchError> {
        let mut count = 0;
        for key in self.decoded_keys() {
            count += usize::from(key? == name);
        }
        Ok(count)
    }

    /// The index in `names` of the first name whose decoded key appears more than once among the top-level members.
    /// A repeat read ahead of the error that stopped the walk is the answer, so such a body is refused for its repeat.
    pub fn repeated_top_level_member(&self, names: &[&str]) -> Result<Option<usize>, CchError> {
        let mut seen: Vec<usize> = Vec::new();
        for key in self.decoded_keys() {
            let key = key?;
            let Some(index) = names.iter().position(|n| *n == key) else {
                continue;
            };
            if seen.contains(&index) {
                return Ok(Some(index));
            }
            seen.push(index);
        }
        Ok(None)
    }

    /// How many members of the first `system` block carry `name` once their keys are decoded, the first block of
    /// every top-level spelling of `system` walked. An error means the walk stopped before the body's end, so a
    /// caller refusing on the count does not admit a body on what was read ahead of the stop.
    pub fn first_system_block_member_count(&self, name: &str) -> Result<usize, CchError> {
        self.strict_end()?;
        let mut count = 0;
        for mut s in self.values_named("system") {
            each_first_block_member(&mut s, |s, key| {
                count += usize::from(key == name.as_bytes());
                s.value(false)
            })?;
        }
        Ok(count)
    }

    /// The first of `names` whose decoded key appears more than once among the members of the top-level object
    /// `parent`, answered as the bare key, `Ok(None)` when `parent` is absent or not an object. Only the parent's own
    /// members are compared: a repeat deeper in the body, a tool input's included, is the client's data.
    pub fn repeated_member_within(
        &self,
        parent: &str,
        names: &[&'static str],
    ) -> Result<Option<&'static str>, CchError> {
        self.strict_end()?;
        let mut repeated = None;
        for mut s in self.values_named(parent) {
            if s.body.get(s.pos) != Some(&b'{') {
                continue;
            }
            let mut seen: Vec<&'static str> = Vec::new();
            each_member(&mut s, Keys::Decoded, |s, key| {
                if let Some(name) = names.iter().copied().find(|n| n.as_bytes() == key) {
                    if seen.contains(&name) {
                        repeated.get_or_insert(name);
                    } else {
                        seen.push(name);
                    }
                }
                s.value(false)
            })?;
        }
        Ok(repeated)
    }

    /// `string_member_span`, the path's first segment naming a top-level member.
    pub fn string_member_span(&self, path: &[&str]) -> Option<(usize, usize)> {
        let (name, rest) = path.split_first()?;
        let (_, mut s) = self.raw_value(name)?;
        if !rest.is_empty() {
            walk_to_last_segment(&mut s, rest)?;
        }
        string_at(s)
    }

    /// `bool_member_span` for the top-level member `name`.
    pub fn bool_member_span(&self, name: &str) -> Option<(usize, usize)> {
        bool_at(self.raw_value(name)?.1)
    }

    /// `number_member_span` for the top-level member `name`.
    pub fn number_member_span(&self, name: &str) -> Option<(usize, usize)> {
        number_at(&self.raw_value(name)?.1)
    }

    /// The stamp the body carries as the relay reads it: the raw walk's stamp, `Unreadable` where the raw bytes hold a
    /// `cch=` (readable or not) while the decoded line holds none, since that `cch=` begins inside an escape and the
    /// server reads no stamp where a restamp would write one. A stamp only the decoded walk reaches answers as the raw
    /// walk reads the line, so refusing it is the caller's count check (`decoded_stamp_count`).
    pub fn reachable_stamp(&self) -> Stamp {
        let text = self.string_member_span(&LINE_PATH);
        let stamp = stamp_in(self.body, text);
        match text {
            Some((start, end))
                if !matches!(stamp, Stamp::None) && !reads_as_a_stamp(&self.body[start..end]) =>
            {
                Stamp::Unreadable
            }
            _ => stamp,
        }
    }

    /// `decoded_stamp_count` of the body.
    pub fn decoded_stamp_count(&self) -> usize {
        let mut count = 0;
        for mut s in self.values_named("system") {
            let _ = each_first_block_member(&mut s, |s, key| {
                if key == b"text" && s.body.get(s.pos) == Some(&b'"') {
                    let (start, end) = s.string()?;
                    count += usize::from(reads_as_a_stamp(&s.body[start..end]));
                    Ok(())
                } else {
                    s.value(false)
                }
            });
        }
        count
    }

    /// `object_member_span` for the top-level member `name`.
    pub fn object_member_span(&self, name: &str) -> Option<(usize, usize)> {
        let member = self.raw_member(name)?;
        Some((member.key.0, member.value_end?))
    }

    /// A walk of the top-level members in body order.
    fn members(&self) -> Members<'a> {
        Members {
            s: Scanner::new(self.body),
            at: WalkAt::Start,
        }
    }

    /// The decoded keys in body order, a key that does not decode answered as the error a decoder gives at it, and
    /// after the last one the error that stopped the walk, where one did.
    fn decoded_keys(&self) -> impl Iterator<Item = Result<Cow<'a, str>, CchError>> {
        let mut walk = Some(self.members());
        std::iter::from_fn(move || match walk.as_mut()?.next() {
            Some(member) => Some(member.decoded.ok_or(CchError::Json(member.key.0))),
            None => match walk.take()?.end() {
                TopLevelEnd::Stopped(error) => Some(Err(error)),
                TopLevelEnd::Closed | TopLevelEnd::ClosedAfterAComma(_) => None,
            },
        })
    }

    /// The end as a walk that reads into a member's value reads it.
    fn strict_end(&self) -> Result<(), CchError> {
        match self.members().end() {
            TopLevelEnd::Closed => Ok(()),
            TopLevelEnd::ClosedAfterAComma(at) => Err(CchError::Json(at)),
            TopLevelEnd::Stopped(error) => Err(error),
        }
    }

    /// A scanner at the value of each top-level member whose decoded key is `name`. A key that does not decode
    /// names no member here.
    fn values_named<'s>(&'s self, name: &'s str) -> impl Iterator<Item = Scanner<'a>> + 's {
        self.members()
            .filter(move |member| member.decoded.as_deref() == Some(name))
            .filter_map(|member| member.value_start)
            .map(|pos| self.value_at(pos))
    }

    /// The key's opening quote and a scanner at the value of the first top-level member whose key's own bytes are
    /// `name`, `None` where the walk met no such member or stopped ahead of its colon.
    fn raw_value(&self, name: &str) -> Option<(usize, Scanner<'a>)> {
        let member = self.raw_member(name)?;
        Some((member.key.0, self.value_at(member.value_start?)))
    }

    /// The first top-level member whose key's own bytes are `name`.
    fn raw_member(&self, name: &str) -> Option<TopLevelMember<'a>> {
        self.members()
            .find(|member| self.body[member.key.0 + 1..member.key.1 - 1] == *name.as_bytes())
    }

    /// A scanner at a top-level member's value, one level down, as a walk stands inside the top-level object.
    fn value_at(&self, pos: usize) -> Scanner<'a> {
        Scanner {
            pos,
            depth: 1,
            ..Scanner::new(self.body)
        }
    }
}

/// A walk of the top-level object, one member per step, in body order.
struct Members<'a> {
    s: Scanner<'a>,
    at: WalkAt,
}

/// Where a walk of the top-level object stands between two steps.
enum WalkAt {
    /// Ahead of the object's opening brace.
    Start,
    /// Past a member whose value was read to its end.
    AfterMember,
    /// Past the object's end or the error that stopped the walk.
    Ended(TopLevelEnd),
}

impl Members<'_> {
    /// Walks the members left and answers how the walk ended.
    fn end(mut self) -> TopLevelEnd {
        loop {
            if let WalkAt::Ended(end) = self.at {
                return end;
            }
            self.next();
        }
    }
}

impl<'a> Iterator for Members<'a> {
    type Item = TopLevelMember<'a>;

    /// The next member. One whose value stops the walk is still answered, with no value end, and the stop is how the
    /// walk ended, so a lookup reads every member ahead of the stop as the walk met it.
    fn next(&mut self) -> Option<TopLevelMember<'a>> {
        let at_start = match self.at {
            WalkAt::Start => true,
            WalkAt::AfterMember => false,
            WalkAt::Ended(_) => return None,
        };
        let s = &mut self.s;
        let (ks, ke) = match next_top_level_key(s, at_start) {
            Ok(key) => key,
            Err(end) => {
                self.at = WalkAt::Ended(end);
                return None;
            }
        };
        let decoded = decoded_key(s.body, ks, ke);
        s.ws();
        let value_start = s.eat(b':').then(|| {
            s.ws();
            s.pos
        });
        let value_end = match value_start {
            Some(_) => s.value(false).map(|()| s.pos),
            None => Err(CchError::Json(s.pos)),
        };
        let value_end = match value_end {
            Ok(end) => {
                self.at = WalkAt::AfterMember;
                Some(end)
            }
            Err(error) => {
                self.at = WalkAt::Ended(TopLevelEnd::Stopped(error));
                None
            }
        };
        Some(TopLevelMember {
            key: (ks, ke),
            decoded,
            value_start,
            value_end,
        })
    }
}

/// Reads a walk of the top-level object up to its next key, from ahead of the object's `{` or from past a member,
/// and answers the key's span, `Err` with how the walk ended where no key follows.
fn next_top_level_key(s: &mut Scanner<'_>, at_start: bool) -> Result<(usize, usize), TopLevelEnd> {
    s.ws();
    if at_start {
        if !s.eat(b'{') {
            return Err(TopLevelEnd::Stopped(CchError::Json(s.pos)));
        }
        // Never left: the scanner stands inside the object for the walk's life, so a value counts toward the depth
        // as it does inside any object.
        s.enter().map_err(TopLevelEnd::Stopped)?;
        s.ws();
        if s.eat(b'}') {
            return Err(TopLevelEnd::Closed);
        }
    } else if s.eat(b',') {
        s.ws();
        if s.body.get(s.pos) == Some(&b'}') {
            return Err(TopLevelEnd::ClosedAfterAComma(s.pos));
        }
    } else if s.eat(b'}') {
        return Err(TopLevelEnd::Closed);
    } else {
        return Err(TopLevelEnd::Stopped(CchError::Json(s.pos)));
    }
    s.ws();
    s.string().map_err(TopLevelEnd::Stopped)
}

/// A key's decoded text, `None` where a decoder refuses it. A key with no escape and no control byte is its own
/// decoded text, so it is read in place. Every other key goes through serde, which refuses a raw control byte,
/// invalid UTF-8 and a lone surrogate escape.
fn decoded_key(body: &[u8], ks: usize, ke: usize) -> Option<Cow<'_, str>> {
    let raw = &body[ks + 1..ke - 1];
    match std::str::from_utf8(raw) {
        Ok(key) if !raw.iter().any(|&b| b == b'\\' || b < 0x20) => Some(Cow::Borrowed(key)),
        _ => serde_json::from_slice(&body[ks..ke]).ok().map(Cow::Owned),
    }
}

/// How many of the top-level object's members carry `name` once their keys are decoded.
pub fn top_level_member_count(body: &[u8], name: &str) -> Result<usize, CchError> {
    TopLevelMembers::read(body).top_level_member_count(name)
}

/// Raw span, quotes included, of the string value at `path` from the top-level object. Array indexes are decimal
/// strings. The relay uses this for its body edits so only one JSON scanner exists in the workspace.
pub fn string_member_span(body: &[u8], path: &[&str]) -> Option<(usize, usize)> {
    string_at(Scanner::at_path(body, path)?)
}

/// The span of the string value at `s`, quotes included.
fn string_at(mut s: Scanner<'_>) -> Option<(usize, usize)> {
    if s.body.get(s.pos) != Some(&b'"') {
        return None;
    }
    s.string().ok()
}

/// Raw span of the `true` or `false` literal at `path` from the top-level object, walking the body as
/// `string_member_span` does: a member of the same name inside a nested value is skipped with that value, so a
/// tool schema cannot stand in for the request's own flag.
pub fn bool_member_span(body: &[u8], path: &[&str]) -> Option<(usize, usize)> {
    bool_at(Scanner::at_path(body, path)?)
}

/// The span of the `true` or `false` literal at `s`, a literal running into anything but a separator no boolean.
fn bool_at(mut s: Scanner<'_>) -> Option<(usize, usize)> {
    // Only a literal is scanned, so a container value is never walked.
    if !matches!(s.body.get(s.pos), Some(b't' | b'f')) {
        return None;
    }
    let start = s.pos;
    s.value(false).ok()?;
    matches!(&s.body[start..s.pos], b"true" | b"false").then_some((start, s.pos))
}

/// Raw span of the number literal at `path`, walked as `bool_member_span` walks. The keepalive replay edits
/// `max_tokens` in place, and the hash view excludes that member, so the edit must not touch anything else.
pub fn number_member_span(body: &[u8], path: &[&str]) -> Option<(usize, usize)> {
    number_at(&Scanner::at_path(body, path)?)
}

/// The span of the number literal at `s`.
fn number_at(s: &Scanner<'_>) -> Option<(usize, usize)> {
    let (body, start) = (s.body, s.pos);
    let mut end = start;
    while end < body.len() && matches!(body[end], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E') {
        end += 1;
    }
    (end > start && body[start].is_ascii_digit()
        || body.get(start) == Some(&b'-') && end > start + 1)
        .then_some((start, end))
}

/// Raw span of the whole member (key quote to value end) at `path`, so a caller can insert a sibling before it.
/// Walked as `walk_to_last_segment` walks: a decoy sharing the key's bytes inside an earlier string, or nested
/// under another object, is skipped as part of the value it sits in rather than matched.
pub fn object_member_span(body: &[u8], path: &[&str]) -> Option<(usize, usize)> {
    let mut s = Scanner::new(body);
    let key = path.last()?;
    walk_to_last_segment(&mut s, &path[..path.len() - 1])?;
    let member_start = scan_object(&mut s, |member| member == key.as_bytes())?;
    member_span(s, member_start)
}

/// The member whose key's opening quote is `member_start` through the end of its value at `s`.
fn member_span(mut s: Scanner<'_>, member_start: usize) -> Option<(usize, usize)> {
    Some((member_start, s.value_span().ok()?.1))
}

/// Cuts the top-level member `key` with one comma by the hash view's rules (the following comma when something
/// follows, else the preceding one when something precedes, else neither) and answers the span it removed;
/// `None` when the walk finds no such member. Matches the raw key bytes as `object_member_span` does, so an
/// escaped spelling is not found and a second spelling survives the cut: the caller decides both.
pub fn remove_top_level_member(body: &mut Vec<u8>, key: &str) -> Option<(usize, usize)> {
    let (start, end) = object_member_span(body, &[key])?;
    let ws = |b: &u8| JSON_WS.contains(b);
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

/// What `cut_sole_top_level_member` did with the member it was asked to cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cut {
    /// No top-level member carries the name, raw or escaped.
    Absent,
    /// The member's one spelling was cut.
    Removed,
    /// More than one top-level member carries the name, and nothing was cut: a second spelling would survive the
    /// cut.
    Repeated,
    /// The member's one spelling is one the raw walk the cut edits through cannot reach, an escaped key.
    Unreachable,
}

/// Cuts the top-level member `key` as `remove_top_level_member` cuts it where the body carries it once, and answers
/// why it could not where the body carries it more than once or escaped. Presence is counted over decoded keys, not
/// read from a parse of the whole body: a body a strict decoder refuses (a lone surrogate escape in a string value)
/// is one the cut still walks.
pub fn cut_sole_top_level_member(body: &mut Vec<u8>, key: &str) -> Result<Cut, CchError> {
    let spellings = top_level_member_count(body, key)?;
    if spellings > 1 {
        return Ok(Cut::Repeated);
    }
    Ok(if remove_top_level_member(body, key).is_some() {
        Cut::Removed
    } else if spellings == 1 {
        Cut::Unreachable
    } else {
        Cut::Absent
    })
}

/// The offset of the first `needle` in `hay`.
/// Panics on an empty `needle`, since `slice::windows` takes no window of size zero.
pub fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
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
    if keys == Keys::Raw {
        return Cow::Borrowed(&body[ks + 1..ke - 1]);
    }
    match decoded_key(body, ks, ke) {
        Some(Cow::Borrowed(key)) => Cow::Borrowed(key.as_bytes()),
        Some(Cow::Owned(key)) => Cow::Owned(key.into_bytes()),
        None => Cow::Borrowed(&[][..]),
    }
}

fn cache_control_walk(body: &[u8], keys: Keys) -> Result<Vec<(usize, usize)>, CchError> {
    let mut s = Scanner::new(body);
    let mut spans = Vec::new();
    s.ws();
    each_member(&mut s, keys, |s, key| match key {
        b"cache_control" => s.value_span().map(|span| spans.push(span)),
        b"system" | b"tools" => block_array(s, &mut spans, false, keys),
        b"messages" => message_array(s, &mut spans, keys),
        _ => s.value(false),
    })?;
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
    each_object_element(s, |s| block_object(s, spans, inner, keys))
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
    each_member(s, keys, |s, key| match key {
        b"cache_control" => s.value_span().map(|span| spans.push(span)),
        // A `tool_result` is the one block the API gives a `content` array of blocks, and its `type` may
        // follow `content` in body order, so the member is read on its name alone and one level only.
        b"content" if !inner => block_array(s, spans, true, keys),
        _ => s.value(false),
    })
}

/// `messages[]`: each object element's `content` is walked as `block_array` walks `system` and `tools`, when it
/// is an array; every other member, and a non-array `content`, is skipped whole.
fn message_array(
    s: &mut Scanner<'_>,
    spans: &mut Vec<(usize, usize)>,
    keys: Keys,
) -> Result<(), CchError> {
    each_object_element(s, |s| message_object(s, spans, keys))
}

fn message_object(
    s: &mut Scanner<'_>,
    spans: &mut Vec<(usize, usize)>,
    keys: Keys,
) -> Result<(), CchError> {
    each_member(s, keys, |s, key| {
        if key == b"content" {
            block_array(s, spans, false, keys)
        } else {
            s.value(false)
        }
    })
}

/// Walks the array at `s`, handing `element` each object element with `s` at its `{` and skipping every other
/// element whole. A value at that position that is not an array is skipped whole too. The array counts toward the
/// depth bound while the walk stands inside it, and on every way out, an error included, the depth is back where it
/// was.
fn each_object_element<'a>(
    s: &mut Scanner<'a>,
    mut element: impl FnMut(&mut Scanner<'a>) -> Result<(), CchError>,
) -> Result<(), CchError> {
    s.ws();
    if s.body.get(s.pos) != Some(&b'[') {
        return s.value(false);
    }
    s.inside(|s| {
        s.pos += 1;
        s.ws();
        if s.eat(b']') {
            return Ok(());
        }
        loop {
            s.ws();
            if s.body.get(s.pos) == Some(&b'{') {
                element(s)?;
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
            return Ok(());
        }
    })
}

/// How many billing stamps sit where `find_cch` looks, `system[0].text` of the top-level object, once every key
/// on the way and the line itself are decoded, each spelling of a repeated key counted. It exceeds what the raw
/// walk reaches exactly where one of those keys or the line is spelled with an escape, or where a second spelling
/// follows the one the raw walk reads: a stamp `restamp_if_present` never rewrites. The scanner reads as far as
/// the body is well formed and counts what it passed, as `string_member_span` finds a member ahead of a syntax
/// error.
pub fn decoded_stamp_count(body: &[u8]) -> usize {
    TopLevelMembers::read(body).decoded_stamp_count()
}

/// Walks the object at `s` (its `{` not yet eaten), handing `member` each member's key as `keys` reads it with `s` at
/// the value, which `member` reads. The object counts toward the depth bound while the walk stands inside it, and on
/// every way out, an error included, the depth is back where it was.
fn each_member<'a>(
    s: &mut Scanner<'a>,
    keys: Keys,
    mut member: impl FnMut(&mut Scanner<'a>, &[u8]) -> Result<(), CchError>,
) -> Result<(), CchError> {
    if s.body.get(s.pos) != Some(&b'{') {
        return Err(CchError::Json(s.pos));
    }
    s.inside(|s| {
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
            member(s, key_name(body, ks, ke, keys).as_ref())?;
            s.ws();
            if s.eat(b',') {
                continue;
            }
            if !s.eat(b'}') {
                return Err(CchError::Json(s.pos));
            }
            return Ok(());
        }
    })
}

/// A `system` value: when it is an array whose first element is an object, `member` is handed each of that object's
/// members as `each_member` hands them. Every other element, and a `system` of another shape, is skipped whole.
fn each_first_block_member<'a>(
    s: &mut Scanner<'a>,
    member: impl FnMut(&mut Scanner<'a>, &[u8]) -> Result<(), CchError>,
) -> Result<(), CchError> {
    s.ws();
    if s.body.get(s.pos) != Some(&b'[') {
        return s.value(false);
    }
    s.pos += 1;
    s.inside(|s| {
        s.ws();
        if s.eat(b']') {
            return Ok(());
        }
        if s.body.get(s.pos) == Some(&b'{') {
            each_member(s, Keys::Decoded, member)?;
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
    })
}

/// Whether a string token, quotes included, is a billing line claiming a hash once decoded. A token a strict
/// decoder refuses for a lone surrogate escape is read through the same mapping the relay reads a body through, so
/// an escaped header name or `cch=` beside one is decoded and counted. A token nothing can decode is read by its
/// bytes, as `find_cch` reads every line, so this count never falls below the raw walk's.
fn reads_as_a_stamp(token: &[u8]) -> bool {
    match decode_tolerant::<String>(token) {
        Some(line) => line.as_bytes().starts_with(&BILLING_BLOCK[1..]) && line.contains("cch="),
        None => token.starts_with(BILLING_BLOCK) && find(token, b"cch=").is_some(),
    }
}

/// The strict decode of `bytes`, else the decode of the reading copy `lone_surrogates_mapped` makes, so the relay's
/// body read and the stamp count read a client's cut string through one mapping. `None` where neither decodes.
pub fn decode_tolerant<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Option<T> {
    serde_json::from_slice(bytes).ok().or_else(|| {
        let copy = lone_surrogates_mapped(bytes)?;
        serde_json::from_slice(&copy).ok()
    })
}

/// A reading copy of `body` in which every lone surrogate escape inside a string token, a `\uD800` to `\uDFFF`
/// without its other half beside it, reads as U+FFFD; `None` when the body carries none, or when the scanner
/// cannot walk it to its end. The client cuts a string between the two halves of a character and sends one half
/// alone, which a strict decoder refuses: the copy is what the router reads such a body through, and never what
/// it sends. The two escapes are the same length, so every offset in the copy is the body's own.
fn lone_surrogates_mapped(body: &[u8]) -> Option<Vec<u8>> {
    let mut s = Scanner::new(body);
    let mut lone = Vec::new();
    each_string_token(&mut s, &mut |start, end| {
        lone_surrogate_escapes(&body[start..end], start, &mut lone)
    })
    .ok()?;
    s.ws();
    if s.pos != body.len() || lone.is_empty() {
        return None;
    }
    let mut copy = body.to_vec();
    for at in lone {
        copy[at + 2..at + 6].copy_from_slice(b"FFFD");
    }
    Some(copy)
}

/// Hands `visit` the span of every string token of the value at `s`, keys included, walking containers as
/// `Scanner::value` walks them and under the same depth bound.
fn each_string_token(
    s: &mut Scanner<'_>,
    visit: &mut impl FnMut(usize, usize),
) -> Result<(), CchError> {
    s.ws();
    let close = match s.body.get(s.pos) {
        Some(b'"') => {
            let (start, end) = s.string()?;
            visit(start, end);
            return Ok(());
        }
        Some(b'{') => b'}',
        Some(b'[') => b']',
        _ => return s.value(false),
    };
    s.inside(|s| {
        s.pos += 1;
        s.ws();
        if !s.eat(close) {
            loop {
                if close == b'}' {
                    s.ws();
                    let (start, end) = s.string()?;
                    visit(start, end);
                    s.ws();
                    if !s.eat(b':') {
                        return Err(CchError::Json(s.pos));
                    }
                }
                each_string_token(s, visit)?;
                s.ws();
                if s.eat(b',') {
                    continue;
                }
                if !s.eat(close) {
                    return Err(CchError::Json(s.pos));
                }
                break;
            }
        }
        Ok(())
    })
}

/// Pushes the offset, `offset` plus its place in `token`, of each `\u` escape in the string token that names a
/// surrogate half without its other half beside it.
fn lone_surrogate_escapes(token: &[u8], offset: usize, out: &mut Vec<usize>) {
    let unit = |at: usize| -> Option<u16> {
        let escape = token.get(at..at + 6)?;
        if !escape.starts_with(b"\\u") {
            return None;
        }
        escape[2..].iter().try_fold(0u16, |unit, &digit| {
            Some(unit << 4 | (digit as char).to_digit(16)? as u16)
        })
    };
    let mut at = 1;
    while at + 1 < token.len() {
        if token[at] != b'\\' {
            at += 1;
            continue;
        }
        match unit(at) {
            Some(0xD800..=0xDBFF) if matches!(unit(at + 6), Some(0xDC00..=0xDFFF)) => at += 12,
            Some(0xD800..=0xDFFF) => {
                out.push(offset + at);
                at += 6;
            }
            Some(_) => at += 6,
            None => at += 2,
        }
    }
}

pub fn sign(body: &[u8]) -> Result<Vec<u8>, CchError> {
    let mut out = body.to_vec();
    restamp_if_present(&mut out)?;
    Ok(out)
}

/// Signs the body in place where it carries a stamp, answering whether it did, as `sign_at` signs it.
pub fn restamp_if_present(body: &mut [u8]) -> Result<bool, CchError> {
    let Some(location) = find_cch(body) else {
        return Ok(false);
    };
    sign_at(body, &location)?;
    Ok(true)
}

/// Signs the stamp at `location` in place. The body is written only once the digits are computed, so a failed
/// signing hands the caller the body it passed in.
pub fn sign_at(body: &mut [u8], location: &CchLocation) -> Result<(), CchError> {
    let digits = expected_digits(body, location)?;
    body[location.digits_start..location.digits_start + LEN].copy_from_slice(&digits);
    Ok(())
}

/// The digits `sign_at` writes at `location`, computed off the borrowed body so a check of the client's stamp needs
/// no signed copy. No edit of the hash view reaches inside `system[0].text`, so the digits sit in the view shifted by
/// the bytes the edits ahead of them cut, and the view's validation reads the same JSON whether the digits are the
/// client's or the placeholder.
pub fn expected_digits(body: &[u8], location: &CchLocation) -> Result<[u8; LEN], CchError> {
    let (mut view, edits) = hash_view(body)?;
    let cut: usize = edits
        .iter()
        .take_while(|e| e.start < location.digits_start)
        .map(|e| e.end - e.start)
        .sum();
    let at = location.digits_start - cut;
    view[at..at + LEN].copy_from_slice(PLACEHOLDER);
    let h = xxh64(&view, SEED) & 0xF_FFFF;
    let mut digits = [0; LEN];
    digits.copy_from_slice(format!("{h:05x}").as_bytes());
    Ok(digits)
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
    hash_view(body).map(|(view, _)| view)
}

/// `normalize`'s view and the edits that built it, sorted by their start.
fn hash_view(body: &[u8]) -> Result<(Vec<u8>, Vec<Edit>), CchError> {
    serde_json::from_slice::<serde::de::IgnoredAny>(body)
        .map_err(|e| CchError::Json(e.column()))?;
    let mut s = Scanner::new(body);
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
    Ok((out, s.edits))
}

impl<'a> Scanner<'a> {
    fn new(body: &'a [u8]) -> Self {
        Scanner {
            body,
            pos: 0,
            depth: 0,
            edits: Vec::new(),
        }
    }
    /// A scanner at the value `path` names, `None` where the walk does not reach the path's last segment.
    fn at_path(body: &'a [u8], path: &[&str]) -> Option<Self> {
        let mut s = Scanner::new(body);
        walk_to_last_segment(&mut s, path)?.then_some(s)
    }
    fn ws(&mut self) {
        while self.body.get(self.pos).is_some_and(|b| JSON_WS.contains(b)) {
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
    /// Counts the bracket at `pos` a walker eats itself, refusing it where `value` would: the container counts toward
    /// the depth for as long as the scanner stands inside it.
    fn enter(&mut self) -> Result<(), CchError> {
        if self.depth + 1 >= MAX_DEPTH {
            return Err(CchError::TooDeep(self.pos));
        }
        self.depth += 1;
        Ok(())
    }
    /// Counts back out at the container's closing bracket.
    fn leave(&mut self) {
        self.depth -= 1;
    }
    /// Runs `walk` one level down, for a walker that eats a container's opening bracket itself rather than
    /// entering it through `value`, so the container counts toward the depth `value` checks and is refused at the
    /// bound `value` refuses it at, and on every way out the depth is back where it was.
    fn inside<T>(
        &mut self,
        walk: impl FnOnce(&mut Self) -> Result<T, CchError>,
    ) -> Result<T, CchError> {
        self.enter()?;
        let walked = walk(self);
        self.leave();
        walked
    }
    fn value(&mut self, collect: bool) -> Result<(), CchError> {
        self.ws();
        match self.body.get(self.pos) {
            // The container counts for as long as the scanner stands inside it, arrays and objects alike, which
            // bounds the recursion for the collecting and the non-collecting walk.
            Some(b'{') | Some(b'[') => self.inside(|s| {
                if s.body[s.pos] == b'{' {
                    s.object(collect)
                } else {
                    s.array(collect)
                }
            }),
            Some(b'"') => self.string().map(|_| ()),
            Some(_) => {
                let start = self.pos;
                while self
                    .body
                    .get(self.pos)
                    .is_some_and(|c| !(matches!(c, b',' | b'}' | b']') || JSON_WS.contains(c)))
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
    fn value_span(&mut self) -> Result<(usize, usize), CchError> {
        let start = self.pos;
        self.value(false)?;
        Ok((start, self.pos))
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
            if collect {
                members.push(Member {
                    start: member_start,
                    end: member_end,
                    comma_before,
                    comma_after,
                    excluded,
                });
            }
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

    /// The guardrails read a body through serde, so a body the scanners admit past serde's limit reaches a `body`
    /// condition as no body at all.
    #[test]
    fn the_scanners_refuse_the_depth_serde_refuses_and_admit_the_depth_it_reads() {
        // The root and 126 arrays are 127 containers, the most serde's recursion limit of 128 reads.
        let body = nested(MAX_DEPTH - 2);
        let admitted = "serde reads 126 arrays under the root, so the scanners do";
        assert!(serde_reads(&body), "{admitted}");
        assert!(sign(&body).is_ok(), "{admitted}");
        assert!(
            string_member_span(&body, &["model"]).is_some(),
            "{admitted}"
        );
        assert!(
            matches!(
                TopLevelMembers::read(&body).repeated_top_level_member(&["model"]),
                Ok(None)
            ),
            "{admitted}"
        );
        assert!(
            matches!(
                TopLevelMembers::read(&body).first_system_block_member_count("text"),
                Ok(1)
            ),
            "{admitted}"
        );
        assert!(
            matches!(cache_control_spans(&body), Ok(spans) if spans.is_empty()),
            "{admitted}"
        );
        for depth in [MAX_DEPTH - 1, MAX_DEPTH] {
            let body = nested(depth);
            let refused =
                format!("serde refuses {depth} arrays under the root, so the scanners do");
            assert!(!serde_reads(&body), "{refused}");
            assert!(
                matches!(sign(&body), Err(CchError::TooDeep(_))),
                "{refused}"
            );
            assert_eq!(string_member_span(&body, &["model"]), None, "{refused}");
            assert!(
                matches!(
                    TopLevelMembers::read(&body).repeated_top_level_member(&["model"]),
                    Err(CchError::TooDeep(_))
                ),
                "{refused}"
            );
            assert!(
                matches!(
                    TopLevelMembers::read(&body).first_system_block_member_count("text"),
                    Err(CchError::TooDeep(_))
                ),
                "{refused}"
            );
            assert!(
                matches!(cache_control_spans(&body), Err(CchError::TooDeep(_))),
                "{refused}"
            );
        }
    }

    /// `depth` nested arrays around a number, the element of a container a walker eats the bracket of itself. The
    /// walker counts each container it stands inside as `value` counts each container it enters, so it refuses the
    /// element at the depth a read of the whole body through `value` refuses it, the root included.
    fn arrays(depth: usize) -> String {
        format!("{}1{}", "[".repeat(depth), "]".repeat(depth))
    }

    fn serde_reads(body: &[u8]) -> bool {
        serde_json::from_slice::<serde_json::Value>(body).is_ok()
    }

    fn scanner_at(body: &[u8], bracket: usize, depth: usize) -> Scanner<'_> {
        Scanner {
            pos: bracket,
            depth,
            ..Scanner::new(body)
        }
    }

    #[test]
    fn a_numeric_path_segment_counts_the_array_it_skips_through_toward_the_depth_bound() {
        let body = |depth: usize| {
            format!(r#"{{"system":[{},{{"text":"x"}}]}}"#, arrays(depth)).into_bytes()
        };
        let path = ["system", "1", "text"];
        let at_the_bound = body(MAX_DEPTH - 2);
        assert!(!serde_reads(&at_the_bound));
        assert_eq!(
            string_member_span(&at_the_bound, &path),
            None,
            "the root, the walked array and the arrays of the skipped element reach the bound"
        );
        let under_it = body(MAX_DEPTH - 3);
        assert!(serde_reads(&under_it));
        assert!(
            string_member_span(&under_it, &path).is_some(),
            "one level less reads as serde reads it"
        );
        let mut s = scanner_at(&under_it, 0, 0);
        assert_eq!(walk_to_last_segment(&mut s, &path), Some(true));
        assert_eq!(
            s.depth, 3,
            "the root, the walked array and the element stand counted at the member's value"
        );
    }

    /// The root counts here, as the member walk enters it through `inside`, so the system array's count brings the
    /// walk to the depth serde reads.
    #[test]
    fn the_first_system_block_walk_counts_the_system_array_toward_the_depth_bound() {
        let at_the_bound =
            format!(r#"{{"system":[{{"text":"x"}},{}]}}"#, arrays(MAX_DEPTH - 2)).into_bytes();
        assert!(serde_json::from_slice::<serde_json::Value>(&at_the_bound).is_err());
        assert!(
            matches!(
                TopLevelMembers::read(&at_the_bound).first_system_block_member_count("text"),
                Err(CchError::TooDeep(_))
            ),
            "the root, the system array and the element's arrays reach the bound"
        );
        let under_it =
            format!(r#"{{"system":[{{"text":"x"}},{}]}}"#, arrays(MAX_DEPTH - 3)).into_bytes();
        assert!(serde_json::from_slice::<serde_json::Value>(&under_it).is_ok());
        assert!(
            matches!(
                TopLevelMembers::read(&under_it).first_system_block_member_count("text"),
                Ok(1)
            ),
            "one level less reads as serde reads it"
        );
    }

    /// The nesting sits past the numeric segment, in the element the path walks into, so the refusal reaches
    /// serde's depth only while the segment's array stays counted under the rest of the walk.
    #[test]
    fn a_numeric_path_segment_keeps_its_array_counted_while_the_path_walks_on_inside_it() {
        let body = |depth: usize| {
            format!(r#"{{"system":[1,{{"pad":{},"text":"x"}}]}}"#, arrays(depth)).into_bytes()
        };
        let path = ["system", "1", "text"];
        let at_the_bound = body(MAX_DEPTH - 3);
        assert!(!serde_reads(&at_the_bound));
        assert_eq!(
            string_member_span(&at_the_bound, &path),
            None,
            "the root, the walked array, the element and the arrays of its skipped member reach the bound"
        );
        let under_it = body(MAX_DEPTH - 4);
        assert!(serde_reads(&under_it));
        assert!(
            string_member_span(&under_it, &path).is_some(),
            "one level less reads as serde reads it"
        );
        let mut s = scanner_at(&under_it, 0, 0);
        assert_eq!(walk_to_last_segment(&mut s, &path), Some(true));
        assert_eq!(
            s.depth, 3,
            "the root, the walked array and the element stand counted at the member's value"
        );
    }

    #[test]
    fn a_matched_member_is_read_inside_every_object_on_its_path_toward_the_depth_bound() {
        let body = |depth: usize| format!(r#"{{"a":{{"b":{}}}}}"#, arrays(depth)).into_bytes();
        let path = ["a", "b"];
        let at_the_bound = body(MAX_DEPTH - 2);
        assert!(!serde_reads(&at_the_bound));
        assert_eq!(
            object_member_span(&at_the_bound, &path),
            None,
            "the root, the inner object and the arrays of the matched member reach the bound"
        );
        let under_it = body(MAX_DEPTH - 3);
        assert!(serde_reads(&under_it));
        assert!(
            object_member_span(&under_it, &path).is_some(),
            "one level less reads as serde reads it"
        );
        let mut s = scanner_at(&under_it, 0, 0);
        assert!(scan_object(&mut s, |key| key == b"a").is_some());
        assert_eq!(
            s.depth, 1,
            "the object stays counted while the matched member's value is read"
        );
        for object in [&br#"{"a":1}"#[..], b"{}"] {
            let mut s = scanner_at(object, 0, 0);
            assert_eq!(scan_object(&mut s, |key| key == b"b"), None);
            assert_eq!(
                s.depth,
                0,
                "an object that closes with no match counts back out: {}",
                String::from_utf8_lossy(object)
            );
        }
    }

    #[test]
    fn a_system_block_array_counts_its_bracket_toward_the_depth_bound() {
        let body = |depth: usize| format!(r#"{{"system":[{}]}}"#, arrays(depth)).into_bytes();
        let at_the_bound = body(MAX_DEPTH - 2);
        assert!(!serde_reads(&at_the_bound));
        assert!(
            matches!(
                cache_control_spans(&at_the_bound),
                Err(CchError::TooDeep(_))
            ),
            "the root, the system array and the arrays of its element reach the bound"
        );
        let under_it = body(MAX_DEPTH - 3);
        assert!(serde_reads(&under_it));
        assert!(
            matches!(cache_control_spans(&under_it), Ok(spans) if spans.is_empty()),
            "one level less reads as serde reads it"
        );
        let mut s = scanner_at(&under_it, br#"{"system":"#.len(), 1);
        block_array(&mut s, &mut Vec::new(), false, Keys::Raw).unwrap();
        assert_eq!(
            s.depth, 1,
            "the walk counts back out at the closing bracket"
        );
    }

    #[test]
    fn a_walker_one_level_under_the_bound_refuses_its_own_bracket_at_the_bracket() {
        assert!(
            matches!(
                block_array(
                    &mut scanner_at(b"[]", 0, MAX_DEPTH - 1),
                    &mut Vec::new(),
                    false,
                    Keys::Raw
                ),
                Err(CchError::TooDeep(0))
            ),
            "the bracket the walker eats itself reaches the bound, with nothing below it for `value` to refuse"
        );
    }

    #[test]
    fn a_system_block_object_counts_its_bracket_toward_the_depth_bound() {
        let body =
            |depth: usize| format!(r#"{{"system":[{{"x":{}}}]}}"#, arrays(depth)).into_bytes();
        let at_the_bound = body(MAX_DEPTH - 3);
        assert!(!serde_reads(&at_the_bound));
        assert!(
            matches!(
                cache_control_spans(&at_the_bound),
                Err(CchError::TooDeep(_))
            ),
            "the root, the system array, the block and the arrays of its member reach the bound"
        );
        let under_it = body(MAX_DEPTH - 4);
        assert!(serde_reads(&under_it));
        assert!(
            matches!(cache_control_spans(&under_it), Ok(spans) if spans.is_empty()),
            "one level less reads as serde reads it"
        );
        let mut s = scanner_at(&under_it, br#"{"system":["#.len(), 2);
        block_object(&mut s, &mut Vec::new(), false, Keys::Raw).unwrap();
        assert_eq!(
            s.depth, 2,
            "the walk counts back out at the closing bracket"
        );
    }

    #[test]
    fn a_messages_array_counts_its_bracket_toward_the_depth_bound() {
        let body = |depth: usize| format!(r#"{{"messages":[{}]}}"#, arrays(depth)).into_bytes();
        let at_the_bound = body(MAX_DEPTH - 2);
        assert!(!serde_reads(&at_the_bound));
        assert!(
            matches!(
                cache_control_spans(&at_the_bound),
                Err(CchError::TooDeep(_))
            ),
            "the root, the messages array and the arrays of its element reach the bound"
        );
        let under_it = body(MAX_DEPTH - 3);
        assert!(serde_reads(&under_it));
        assert!(
            matches!(cache_control_spans(&under_it), Ok(spans) if spans.is_empty()),
            "one level less reads as serde reads it"
        );
        let mut s = scanner_at(&under_it, br#"{"messages":"#.len(), 1);
        message_array(&mut s, &mut Vec::new(), Keys::Raw).unwrap();
        assert_eq!(
            s.depth, 1,
            "the walk counts back out at the closing bracket"
        );
    }

    #[test]
    fn a_message_object_counts_its_bracket_toward_the_depth_bound() {
        let body =
            |depth: usize| format!(r#"{{"messages":[{{"x":{}}}]}}"#, arrays(depth)).into_bytes();
        let at_the_bound = body(MAX_DEPTH - 3);
        assert!(!serde_reads(&at_the_bound));
        assert!(
            matches!(
                cache_control_spans(&at_the_bound),
                Err(CchError::TooDeep(_))
            ),
            "the root, the messages array, the message and the arrays of its member reach the bound"
        );
        let under_it = body(MAX_DEPTH - 4);
        assert!(serde_reads(&under_it));
        assert!(
            matches!(cache_control_spans(&under_it), Ok(spans) if spans.is_empty()),
            "one level less reads as serde reads it"
        );
        let mut s = scanner_at(&under_it, br#"{"messages":["#.len(), 2);
        message_object(&mut s, &mut Vec::new(), Keys::Raw).unwrap();
        assert_eq!(
            s.depth, 2,
            "the walk counts back out at the closing bracket"
        );
    }

    /// An empty `system` or `tools` is common, and a walk that counted into it and never out would read every later
    /// member one level nearer the bound.
    #[test]
    fn an_empty_block_array_counts_back_out_of_its_bracket() {
        let body = format!(r#"{{"system":[],"x":{}}}"#, arrays(MAX_DEPTH - 2)).into_bytes();
        assert!(serde_reads(&body));
        assert!(
            matches!(cache_control_spans(&body), Ok(spans) if spans.is_empty()),
            "the walk counts back out at the closing bracket"
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

    /// The scanner walks a bare `tru` as a literal and reaches the stamp, while the view's serde pass refuses it, so
    /// the restamp fails after it zeroed the digits.
    #[test]
    fn a_restamp_whose_view_cannot_be_built_hands_back_the_body_with_its_digits() {
        let body = br#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cch=abcde;"}],"stream":tru}"#;
        let mut restamped = body.to_vec();
        assert!(matches!(
            restamp_if_present(&mut restamped),
            Err(CchError::Json(_))
        ));
        assert_eq!(
            String::from_utf8_lossy(&restamped),
            String::from_utf8_lossy(body)
        );
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
            TopLevelMembers::read(br#"{"safeguards":[1],"model":"m","safeguards":[2]}"#)
                .repeated_top_level_member(&["safeguards"])
                .unwrap(),
            Some(0)
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
            "a line a strict decoder refuses for a lone surrogate counts, as the raw walk counts it"
        );
    }

    #[test]
    fn a_lone_surrogate_beside_an_escaped_cch_counts_as_a_stamp_in_the_decoded_walk() {
        let body = |line: &str| {
            format!(r#"{{"system":[{{"type":"text","text":"{line}"}}],"model":"m"}}"#).into_bytes()
        };

        let escaped_cch = body(r"x-anthropic-billing-header: v=1 \udc00 cch\u003d01234;");
        assert!(
            serde_json::from_slice::<serde_json::Value>(&escaped_cch).is_err(),
            "a strict decoder refuses the line"
        );
        assert!(
            find_cch(&escaped_cch).is_none(),
            "the raw bytes spell no cch="
        );
        assert_eq!(
            decoded_stamp_count(&escaped_cch),
            1,
            "read through the lone surrogate mapping, the line decodes to a stamp"
        );

        let escaped_colon = body(r"x-anthropic-billing-header\u003a v=1 \udc00 cch=01234;");
        assert!(
            serde_json::from_slice::<serde_json::Value>(&escaped_colon).is_err(),
            "a strict decoder refuses the line"
        );
        assert!(find_cch(&escaped_colon).is_none());
        assert_eq!(decoded_stamp_count(&escaped_colon), 1);

        let plain = body("x-anthropic-billing-header: v=1 cch=01234;");
        assert!(find_cch(&plain).is_some());
        assert_eq!(decoded_stamp_count(&plain), 1);
    }

    /// `\u00cc` decodes to one character, and the last two hex digits of the escape spell the `cc` of a raw `cch=`
    /// whose digits read, so the raw walk alone would answer a hash the server never sees.
    #[test]
    fn a_cch_an_escape_swallows_is_unreadable_where_the_decoded_line_holds_none() {
        let body = |line: &str| {
            format!(r#"{{"system":[{{"type":"text","text":"{line}"}}],"model":"m"}}"#).into_bytes()
        };
        let swallowed =
            body(r"x-anthropic-billing-header: cc_version=2.1.220.test; \u00cch=01234;");
        assert!(
            matches!(stamp(&swallowed), Stamp::Hash(_)),
            "the raw bytes spell a readable cch="
        );
        assert_eq!(
            decoded_stamp_count(&swallowed),
            0,
            "the decoded line holds none"
        );
        assert!(matches!(
            TopLevelMembers::read(&swallowed).reachable_stamp(),
            Stamp::Unreadable
        ));

        let plain = body("x-anthropic-billing-header: cc_version=2.1.220.test; cch=01234;");
        assert!(matches!(
            TopLevelMembers::read(&plain).reachable_stamp(),
            Stamp::Hash(_)
        ));
    }

    #[test]
    fn a_placeholder_beside_an_escaped_second_cch_field_is_not_the_placeholder_alone() {
        let body = br#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cch=00000; c\u0063h=12345;"}],"model":"m"}"#;
        assert_eq!(
            String::from_utf8_lossy(body).matches("cch=").count(),
            1,
            "the raw line spells its second field with an escape"
        );
        assert!(!matches!(stamp(body), Stamp::Placeholder(_)));
    }

    #[test]
    fn a_placeholder_on_a_line_that_does_not_decode_is_not_the_placeholder_alone() {
        // A lone surrogate escape: the scanner walks it, a strict decoder refuses the line.
        let body = br#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cch=00000; \ud800"}],"model":"m"}"#;
        assert!(
            find_cch(body).is_some(),
            "the raw walk reads the placeholder"
        );
        assert!(!matches!(stamp(body), Stamp::Placeholder(_)));
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
            "the marker inside the tool result is found ahead of the block's own, and one two levels down is skipped"
        );
    }

    #[test]
    fn an_escaped_repeat_of_a_top_level_member_is_found_by_its_decoded_key() {
        let body = "{\"model\":\"a\",\"messages\":[],\"\\u006dodel\":\"b\"}".as_bytes();
        assert_eq!(
            TopLevelMembers::read(body)
                .repeated_top_level_member(&["metadata", "model", "stream"])
                .unwrap(),
            Some(1)
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

    /// A raw control byte inside a key is no JSON string, so the walk refuses it as a decoder would rather than read
    /// the key past it.
    #[test]
    fn a_top_level_key_holding_a_raw_control_byte_is_refused() {
        assert!(matches!(
            top_level_member_count(b"{\"safe\x01guards\":[]}", "safeguards"),
            Err(CchError::Json(_))
        ));
    }

    #[test]
    fn a_repeat_inside_a_nested_object_is_not_a_top_level_repeat() {
        let body = br#"{"model":"a","metadata":{"model":"x","model":"y"},"messages":[]}"#;
        assert_eq!(
            TopLevelMembers::read(body)
                .repeated_top_level_member(&["metadata", "model", "stream"])
                .unwrap(),
            None
        );
    }

    #[test]
    fn a_repeated_member_of_the_parent_object_is_found_raw_or_escaped() {
        for body in [
            br#"{"model":"a","metadata":{"user_id":"a","user_id":"b"},"messages":[]}"#.as_slice(),
            br#"{"model":"a","metadata":{"user_id":"a","\u0075ser_id":"b"},"messages":[]}"#,
            br#"{"model":"a","metadata":{"\u0075ser_id":"a","user_id":"b"},"messages":[]}"#,
        ] {
            assert_eq!(
                TopLevelMembers::read(body)
                    .repeated_member_within("metadata", &["user_id"])
                    .unwrap(),
                Some("user_id"),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
        assert_eq!(
            TopLevelMembers::read(br#"{"metadata":{"user_id":"a","model":"x","model":"y"}}"#)
                .repeated_member_within("metadata", &["user_id"])
                .unwrap(),
            None,
            "a member not named is not looked at"
        );
    }

    #[test]
    fn a_repeat_below_the_parents_own_members_is_not_found() {
        let deeper = br#"{"metadata":{"user_id":"a","extra":{"user_id":"b","user_id":"c"}},"messages":[{"role":"user","content":[{"type":"tool_use","input":{"metadata":{"user_id":"d","user_id":"e"}}}]}]}"#;
        assert_eq!(
            TopLevelMembers::read(deeper)
                .repeated_member_within("metadata", &["user_id"])
                .unwrap(),
            None
        );
        for absent_or_not_an_object in [
            br#"{"model":"a"}"#.as_slice(),
            br#"{"metadata":"user_id"}"#,
            br#"{"metadata":["user_id","user_id"]}"#,
        ] {
            assert_eq!(
                TopLevelMembers::read(absent_or_not_an_object)
                    .repeated_member_within("metadata", &["user_id"])
                    .unwrap(),
                None
            );
        }
    }

    #[test]
    fn a_lone_surrogate_escape_reads_as_the_replacement_character_in_the_copy_alone() {
        let body = br#"{"a":"cut \ud83d","b":"\udc00 low","c":"pair \ud83d\ude00","d":"\\ud83d","e":"\uD83D\uD83D","\ud800":1}"#;
        assert!(serde_json::from_slice::<serde_json::Value>(body).is_err());
        let copy = lone_surrogates_mapped(body).expect("the body carries lone escapes");
        assert_eq!(copy.len(), body.len(), "the copy keeps every offset");
        let read: serde_json::Value = serde_json::from_slice(&copy).unwrap();
        assert_eq!(read["a"], "cut \u{fffd}");
        assert_eq!(read["b"], "\u{fffd} low");
        assert_eq!(read["c"], "pair \u{1f600}", "a pair is left whole");
        assert_eq!(
            read["d"], "\\ud83d",
            "an escaped backslash starts no escape"
        );
        assert_eq!(
            read["e"], "\u{fffd}\u{fffd}",
            "two high halves pair with nothing"
        );
        assert_eq!(read["\u{fffd}"], 1, "a key is a string token too");
    }

    #[test]
    fn a_body_with_no_lone_surrogate_or_one_the_scanner_cannot_walk_has_no_copy() {
        assert_eq!(
            lone_surrogates_mapped(br#"{"a":"pair \ud83d\ude00","b":"\n"}"#),
            None
        );
        assert_eq!(
            lone_surrogates_mapped(br#"{"a":"cut \ud83d""#),
            None,
            "an unterminated object"
        );
    }

    #[test]
    fn a_body_whose_top_level_is_an_array_is_refused() {
        let body = br#"[{"model":"a"}]"#;
        assert!(matches!(
            TopLevelMembers::read(body).repeated_top_level_member(&["metadata", "model", "stream"]),
            Err(CchError::Json(0))
        ));
    }
}
