//! The wildcard grammar the user-agent filters share: `*` for any run of characters, `?` for one, and a
//! backslash to quote either. The request log hands `to_like` to SQLite; the analytics test rollup rows in
//! memory with `matches`, so the two views cannot drift apart on what a pattern means.

use std::ops::Deref;

/// The escape character every `LIKE` pattern of the ledger names, written into the statement as `ESCAPE '\'`. A
/// backslash rather than SQLite's absent default, since a backslash is what the grammar itself uses and no measured
/// user agent carries one.
pub(crate) const ESCAPE: char = '\\';

/// Pushes `c` onto a `LIKE` pattern as itself, escaped where `LIKE` would read it as a wildcard or as the escape.
pub(crate) fn push_literal(out: &mut String, c: char) {
    if matches!(c, '%' | '_' | ESCAPE) {
        out.push(ESCAPE);
    }
    out.push(c);
}

/// `text` with `\`, `%` and `_` escaped for `LIKE .. ESCAPE '\'`, so every typed character matches itself.
pub fn literal_like(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        push_literal(&mut out, c);
    }
    out
}

/// The `LIKE` pattern to pass with `ESCAPE`: `*` becomes `%`, `?` becomes `_`, and a literal `%`, `_`, or the escape
/// character itself is escaped, so a pattern with no wildcard matches its own text alone.
pub fn to_like(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len() + 4);
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '*' => out.push('%'),
            '?' => out.push('_'),
            // A quoted character reaches SQLite as itself. A trailing backslash quotes nothing and matches nothing
            // more.
            '\\' => {
                if let Some(next) = chars.next() {
                    push_literal(&mut out, next);
                }
            }
            other => push_literal(&mut out, other),
        }
    }
    out
}

/// The longest pattern and the longest pattern list a route accepts. The matcher below is linear per star
/// run and at worst `pattern * value` steps, so the bound keeps one analytics read over a few thousand
/// rollup rows within a fraction of a worker's time whatever the caller typed.
pub const MAX_PATTERN_CHARS: usize = 256;
pub const MAX_PATTERNS: usize = 32;
/// The refusal a route answers for a pattern or a list past the bounds.
pub const PATTERNS_TOO_LONG: &str = "patterns are at most 256 characters, at most 32 of them";

pub fn within_bounds(patterns: &[String]) -> bool {
    patterns.len() <= MAX_PATTERNS
        && patterns
            .iter()
            .all(|p| p.chars().count() <= MAX_PATTERN_CHARS)
}

/// A pattern list within the bounds, built only through `Patterns::new`, so a filter that holds one was checked once,
/// where the route read it.
#[derive(Clone, Debug, Default)]
pub struct Patterns(Vec<String>);

impl Patterns {
    /// `PATTERNS_TOO_LONG` for a list past the bounds.
    pub fn new(patterns: Vec<String>) -> Result<Patterns, &'static str> {
        if within_bounds(&patterns) {
            Ok(Patterns(patterns))
        } else {
            Err(PATTERNS_TOO_LONG)
        }
    }
}

impl Deref for Patterns {
    type Target = [String];

    fn deref(&self) -> &[String] {
        &self.0
    }
}

impl From<Patterns> for Vec<String> {
    fn from(patterns: Patterns) -> Vec<String> {
        patterns.0
    }
}

/// Whether `value` matches `pattern` under the same grammar, character by character; `*` is greedy over any run
/// (including none), `?` consumes exactly one character, and `\` quotes the next character. Linear per star
/// run, `pattern * value` steps at worst.
pub fn matches(pattern: &str, value: &str) -> bool {
    matches_counted(pattern, value).0
}

/// The match and the number of steps it took, the count being what the tests bound. Two cursors walk the
/// pattern and the value; a `*` records where it stood and where the value stood, and a later mismatch resumes
/// after that star with the value one character further on, so no split is tried twice and the work is
/// `pattern * value` at worst rather than one path per way of splitting the value among the stars. ASCII case
/// folds, as SQLite's `LIKE` folds it on the SQL side.
fn matches_counted(pattern: &str, value: &str) -> (bool, usize) {
    let first = |s: &str, at: usize| s[at..].chars().next();
    let (mut pi, mut vi, mut steps) = (0, 0, 0);
    // The pattern offset after the last star seen and the value offset that star currently covers up to, both in
    // bytes, each moved a whole character at a time.
    let mut star: Option<(usize, usize)> = None;
    while let Some(c) = first(value, vi) {
        steps += 1;
        // How many pattern bytes the next token takes when it fits `c`.
        let fit = match first(pattern, pi) {
            Some('*') => {
                star = Some((pi + 1, vi));
                pi += 1;
                continue;
            }
            Some('?') => Some(1),
            Some('\\') => first(pattern, pi + 1)
                .filter(|quoted| quoted.eq_ignore_ascii_case(&c))
                .map(|quoted| 1 + quoted.len_utf8()),
            Some(literal) if literal.eq_ignore_ascii_case(&c) => Some(literal.len_utf8()),
            _ => None,
        };
        match fit {
            Some(width) => {
                pi += width;
                vi += c.len_utf8();
            }
            None => match star {
                Some((after_star, covered)) => {
                    pi = after_star;
                    vi = covered + first(value, covered).map_or(0, char::len_utf8);
                    star = Some((after_star, vi));
                }
                None => return (false, steps),
            },
        }
    }
    // The value is spent; what is left of the pattern must match nothing: stars, or a trailing backslash,
    // which quotes nothing. Both are one byte, and no byte of a wider character equals either.
    let p = pattern.as_bytes();
    while pi < p.len() {
        match p[pi] {
            b'*' => pi += 1,
            b'\\' if pi + 1 == p.len() => pi += 1,
            _ => return (false, steps),
        }
    }
    (true, steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLI: &str = "claude-cli/2.1.269 (external, cli)";
    const SDK: &str = "claude-cli/2.1.269 (external, sdk-cli)";

    #[test]
    fn a_star_matches_any_run_and_a_question_mark_one_character() {
        assert!(matches("claude-cli/*", CLI));
        assert!(matches("claude-cli/*", SDK));
        assert!(matches("*sdk-cli*", SDK));
        assert!(!matches("*sdk-cli*", CLI));
        assert!(matches("claude-cli/2.1.2?? (external, cli)", CLI));
        assert!(
            !matches("claude-cli/2.1.2? (external, cli)", CLI),
            "one mark is one character"
        );
        assert!(matches("*", ""), "a star alone matches the empty value");
        assert!(!matches("?", ""), "a mark needs a character");
        assert!(
            matches("CLAUDE-CLI/*", CLI),
            "ASCII case folds as LIKE folds it"
        );
        assert!(
            matches("*cli)", CLI),
            "a star before a literal tail backtracks to the last fit"
        );
        assert!(
            matches("a*b*c", "aXbYbZc"),
            "a later star resumes after the earlier one's last fit"
        );
        assert!(
            !matches("a*b*c", "aXbYbZ"),
            "and fails when the tail never lands"
        );
    }

    /// A pattern of many stars against a value that cannot match is where a split-per-star matcher goes
    /// exponential; the two-cursor walk answers in `pattern * value` steps at most.
    #[test]
    fn a_multi_byte_character_is_one_character_under_a_mark_a_star_and_a_quote() {
        assert!(
            matches("caf?", "café"),
            "a mark takes a two-byte character whole"
        );
        assert!(
            matches("*é!", "éé!"),
            "a star resumes past a two-byte character"
        );
        assert!(matches("\\é?", "éx"), "a quoted two-byte character is one");
    }

    #[test]
    fn a_failing_pattern_of_many_stars_answers_in_bounded_steps() {
        let pattern = "*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*b";
        let value = "a".repeat(30);
        let (matched, steps) = matches_counted(pattern, &value);
        assert!(!matched);
        assert!(
            steps < 100_000,
            "{steps} steps for a fifteen-star pattern over thirty characters"
        );
        let alternating = "*?*?*?*?*?*?*?*?*?*?*?*?*?*?*?*?b";
        let (matched, steps) = matches_counted(alternating, &"a".repeat(32));
        assert!(!matched);
        assert!(
            steps < 100_000,
            "{steps} steps for an alternating pattern over thirty-two characters"
        );
    }

    #[test]
    fn a_pattern_list_is_bounded_in_length_and_in_count() {
        assert!(within_bounds(&vec!["claude-cli/*".to_string(); 32]));
        assert!(
            !within_bounds(&vec!["claude-cli/*".to_string(); 33]),
            "one entry too many"
        );
        assert!(within_bounds(&["a".repeat(256)]));
        assert!(!within_bounds(&["a".repeat(257)]), "one character too many");
        assert!(within_bounds(&[]));
    }

    #[test]
    fn a_pattern_with_no_wildcard_is_an_exact_match() {
        assert!(matches(CLI, CLI));
        assert!(!matches(CLI, SDK));
        assert!(
            !matches("claude-cli/2.1.269", CLI),
            "no implicit trailing star"
        );
    }

    #[test]
    fn a_backslash_quotes_the_next_character() {
        assert!(matches(r"a\*b", "a*b"));
        assert!(!matches(r"a\*b", "aXb"));
        assert!(matches(r"a\?b", "a?b"));
        assert!(!matches(r"a\?b", "aXb"));
        assert!(matches(r"a\\b", r"a\b"));
        assert!(matches(r"a\", "a"), "a trailing backslash quotes nothing");
    }

    #[test]
    fn the_like_form_maps_the_grammar_onto_sqlite_and_escapes_its_own_wildcards() {
        assert_eq!(to_like("claude-cli/*"), "claude-cli/%");
        assert_eq!(to_like("claude-cli/2.1.2??"), "claude-cli/2.1.2__");
        assert_eq!(to_like("100%_sure"), r"100\%\_sure");
        assert_eq!(
            to_like(r"a\*b"),
            "a*b",
            "a quoted star is a plain star to LIKE"
        );
        assert_eq!(
            to_like(r"a\\b"),
            r"a\\b",
            "a quoted backslash is escaped for LIKE"
        );
        assert_eq!(to_like(CLI), CLI, "no wildcard, no change");
    }

    /// The two halves agree: what `matches` accepts, SQLite's `LIKE ... ESCAPE` accepts too.
    #[test]
    fn the_like_form_and_the_memory_match_agree_on_sqlite() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let values = [
            CLI,
            SDK,
            "curl/8.5.0",
            "a*b",
            "a?b",
            r"a\b",
            "100%_sure",
            "",
        ];
        let patterns = [
            "claude-cli/*",
            "*sdk-cli*",
            "claude-cli/2.1.2??*",
            r"a\*b",
            r"a\?b",
            r"a\\b",
            "100%_sure",
            "*",
            "?",
            CLI,
            "curl/8.5.?",
        ];
        for pattern in patterns {
            let like = to_like(pattern);
            for value in values {
                let sql: bool = conn
                    .query_row(
                        &format!("SELECT ?1 LIKE ?2 ESCAPE '{ESCAPE}'"),
                        rusqlite::params![value, like],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(
                    sql,
                    matches(pattern, value),
                    "pattern {pattern:?} over {value:?}"
                );
            }
        }
    }
}
