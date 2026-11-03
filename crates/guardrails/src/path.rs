use serde_json::Value;

pub const PATH_MAX_CHARS: usize = 256;
pub const PATH_MAX_SEGMENTS: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    Member(String),
    Index(usize),
    FromEnd(usize),
    Any,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BodyPath {
    pub segments: Vec<Segment>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("path is empty")]
    Empty,
    #[error("path is longer than {max} characters", max = PATH_MAX_CHARS)]
    TooLong,
    #[error("path holds more than {max} segments", max = PATH_MAX_SEGMENTS)]
    TooManySegments,
    #[error("path starts with an index")]
    StartsWithIndex,
    #[error("path has an empty member at segment {0}")]
    EmptyMember(usize),
    #[error("path has a malformed index at segment {0}")]
    MalformedIndex(usize),
    #[error("path has an unclosed index at segment {0}")]
    UnclosedIndex(usize),
}

impl BodyPath {
    pub fn parse(text: &str) -> Result<BodyPath, PathError> {
        if text.is_empty() {
            return Err(PathError::Empty);
        }
        if text.chars().count() > PATH_MAX_CHARS {
            return Err(PathError::TooLong);
        }
        if text.starts_with('[') {
            return Err(PathError::StartsWithIndex);
        }
        let mut segments = Vec::new();
        let mut rest = text;
        let mut member_next = true;
        loop {
            if member_next {
                let end = rest.find(['.', '[', ']']).unwrap_or(rest.len());
                if end == 0 {
                    return Err(PathError::EmptyMember(segments.len() + 1));
                }
                segments.push(Segment::Member(rest[..end].to_string()));
                rest = &rest[end..];
            }
            let Some(separator) = rest.chars().next() else {
                break;
            };
            match separator {
                '.' => {
                    rest = &rest[1..];
                    member_next = true;
                }
                '[' => {
                    let at = segments.len() + 1;
                    let inner = &rest[1..];
                    let close = match (inner.find(']'), inner.find('.')) {
                        (Some(close), Some(dot)) if dot < close => {
                            return Err(PathError::UnclosedIndex(at))
                        }
                        (Some(close), _) => close,
                        (None, _) => return Err(PathError::UnclosedIndex(at)),
                    };
                    segments.push(index(&inner[..close]).ok_or(PathError::MalformedIndex(at))?);
                    rest = &inner[close + 1..];
                    if !(rest.is_empty() || rest.starts_with(['.', '['])) {
                        return Err(PathError::MalformedIndex(at));
                    }
                    member_next = false;
                }
                _ => return Err(PathError::MalformedIndex(segments.len() + 1)),
            }
        }
        if segments.len() > PATH_MAX_SEGMENTS {
            return Err(PathError::TooManySegments);
        }
        Ok(BodyPath { segments })
    }

    /// Calls `found` on every value the path resolves to, in the tree's order, until one call answers true.
    pub fn any(&self, root: &Value, found: &mut dyn FnMut(&Value) -> bool) -> bool {
        walk(&self.segments, root, found)
    }
}

fn index(text: &str) -> Option<Segment> {
    let decimal = |digits: &str| {
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        digits.parse::<usize>().ok()
    };
    match text {
        "*" => Some(Segment::Any),
        _ => match text.strip_prefix('-') {
            Some(digits) => decimal(digits).filter(|&n| n >= 1).map(Segment::FromEnd),
            None => decimal(text).map(Segment::Index),
        },
    }
}

fn walk(segments: &[Segment], value: &Value, found: &mut dyn FnMut(&Value) -> bool) -> bool {
    let Some((first, rest)) = segments.split_first() else {
        return found(value);
    };
    match first {
        Segment::Member(name) => value
            .get(name.as_str())
            .is_some_and(|next| walk(rest, next, found)),
        Segment::Index(at) => value
            .as_array()
            .and_then(|items| items.get(*at))
            .is_some_and(|next| walk(rest, next, found)),
        Segment::FromEnd(back) => value
            .as_array()
            .and_then(|items| items.get(items.len().checked_sub(*back)?))
            .is_some_and(|next| walk(rest, next, found)),
        Segment::Any => value
            .as_array()
            .is_some_and(|items| items.iter().any(|next| walk(rest, next, found))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(name: &str) -> Segment {
        Segment::Member(name.to_string())
    }

    fn segments(text: &str) -> Vec<Segment> {
        BodyPath::parse(text)
            .unwrap_or_else(|error| panic!("{text}: {error}"))
            .segments
    }

    fn refusal(text: &str) -> String {
        BodyPath::parse(text).expect_err(text).to_string()
    }

    #[test]
    fn the_grammar_parses_members_indexes_and_stars_into_segments() {
        assert_eq!(segments("tools"), [member("tools")]);
        assert_eq!(segments("max_tokens"), [member("max_tokens")]);
        assert_eq!(
            segments("metadata.user_id"),
            [member("metadata"), member("user_id")]
        );
        assert_eq!(
            segments("system[0].text"),
            [member("system"), Segment::Index(0), member("text")]
        );
        assert_eq!(
            segments("messages[-1].role"),
            [member("messages"), Segment::FromEnd(1), member("role")]
        );
        assert_eq!(
            segments("tools[*].name"),
            [member("tools"), Segment::Any, member("name")]
        );
        assert_eq!(
            segments("messages[*].content[*].type"),
            [
                member("messages"),
                Segment::Any,
                member("content"),
                Segment::Any,
                member("type")
            ]
        );
        assert_eq!(
            segments("a key-with space.x"),
            [member("a key-with space"), member("x")]
        );
        assert_eq!(
            segments("a[12][-3]"),
            [member("a"), Segment::Index(12), Segment::FromEnd(3)]
        );
        assert_eq!(
            segments("a[0].b[*]"),
            [member("a"), Segment::Index(0), member("b"), Segment::Any]
        );

        let longest = vec!["a"; PATH_MAX_SEGMENTS].join(".");
        assert_eq!(segments(&longest).len(), PATH_MAX_SEGMENTS);
        let widest = "\u{e9}".repeat(PATH_MAX_CHARS);
        assert_eq!(segments(&widest), [member(&widest)]);
    }

    #[test]
    fn each_path_error_names_its_case() {
        assert_eq!(refusal(""), "path is empty");
        assert_eq!(
            refusal(&"a".repeat(257)),
            "path is longer than 256 characters"
        );
        assert_eq!(
            refusal(&vec!["a"; 33].join(".")),
            "path holds more than 32 segments"
        );
        assert_eq!(
            refusal(&format!("a{}", "[0]".repeat(32))),
            "path holds more than 32 segments"
        );
        assert_eq!(refusal("[0].a"), "path starts with an index");
        assert_eq!(refusal("a..b"), "path has an empty member at segment 2");
        assert_eq!(refusal(".a"), "path has an empty member at segment 1");
        assert_eq!(refusal("a."), "path has an empty member at segment 2");
        assert_eq!(refusal("]a"), "path has an empty member at segment 1");
        assert_eq!(refusal("a.]b"), "path has an empty member at segment 2");
        assert_eq!(refusal("a[x]"), "path has a malformed index at segment 2");
        assert_eq!(refusal("a[-0]"), "path has a malformed index at segment 2");
        assert_eq!(refusal("a[]"), "path has a malformed index at segment 2");
        assert_eq!(refusal("a[+1]"), "path has a malformed index at segment 2");
        assert_eq!(
            refusal("a.b[1x]"),
            "path has a malformed index at segment 3"
        );
        assert_eq!(refusal("a]b"), "path has a malformed index at segment 2");
        assert_eq!(refusal("a[0]b"), "path has a malformed index at segment 2");
        assert_eq!(refusal("a[1"), "path has an unclosed index at segment 2");
        assert_eq!(refusal("a[1.b]"), "path has an unclosed index at segment 2");
    }
}
