//! The Claude Code billing-header hash (`cch=xxxxx;`) that must be recomputed after any body edit.

use xxhash_rust::xxh64::xxh64;

pub const SEED: u64 = 0x4D65_9218_E32A_3268;
const LEN: usize = 5;

#[derive(Debug, thiserror::Error)]
pub enum CchError {
    #[error("body is not valid JSON at byte {0}")]
    Json(usize),
    #[error("overlapping normalization edit at byte {0}")]
    Overlap(usize),
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

/// Raw span, quotes included, of the string value at `path` from the top-level object. Array indexes are decimal
/// strings. The relay uses this for its body edits so only one JSON scanner exists in the workspace.
pub fn string_member_span(body: &[u8], path: &[&str]) -> Option<(usize, usize)> {
    let mut s = Scanner {
        body,
        pos: 0,
        edits: Vec::new(),
    };
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
            if s.body.get(s.pos) != Some(&b'"') {
                return None;
            }
            return s.string().ok();
        }
        s.ws();
    }
    None
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
            Some(b'{') => self.object(collect),
            Some(b'[') => self.array(collect),
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
