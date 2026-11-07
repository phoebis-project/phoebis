//! The path-style pattern grammar the model rules share: `*` for any run of characters other than `/`, `**` for
//! any run including `/`, `?` for one character other than `/`, and a backslash to quote any of them. A pattern
//! with no wildcard matches its own text alone. ASCII case folds, since the name a rule is judged against is the
//! folded canonical id. The ledger's request-log matcher is another grammar, where `*` crosses everything.

/// The longest pattern an entry accepts, counted in characters: [`check`] refuses a longer one as
/// [`PatternError::TooLong`]. The matcher answers no match for a pattern of more tokens than this, which bounds the
/// rows of a match's table.
pub const MAX_PATTERN_CHARS: usize = 256;
/// The longest name the matcher walks. A name past it answers no match, so the cells a match decides are at most
/// the product of this bound and [`MAX_PATTERN_CHARS`].
pub const MAX_NAME_CHARS: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PatternError {
    #[error("pattern is empty")]
    Empty,
    #[error("pattern is longer than 256 characters")]
    TooLong,
    #[error("pattern ends in an unquoted backslash")]
    TrailingBackslash,
    #[error("pattern holds three stars in a row")]
    TripleStar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token {
    AnyOne,
    AnyPath,
    AnyRun,
    Literal(char),
}

fn tokenize(pattern: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => tokens.push(Token::Literal(chars.next().unwrap_or('\\'))),
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                tokens.push(Token::AnyPath);
            }
            '*' => tokens.push(Token::AnyRun),
            '?' => tokens.push(Token::AnyOne),
            other => tokens.push(Token::Literal(other)),
        }
    }
    tokens
}

/// Refuses, in order, an empty pattern, one over [`MAX_PATTERN_CHARS`], a trailing backslash the grammar cannot
/// quote (an odd run of trailing backslashes leaves the last one unquoted), and three unquoted stars in a row
/// (ambiguous between an `AnyPath` and an `AnyRun` split).
pub fn check(pattern: &str) -> Result<(), PatternError> {
    if pattern.is_empty() {
        return Err(PatternError::Empty);
    }
    if pattern.chars().count() > MAX_PATTERN_CHARS {
        return Err(PatternError::TooLong);
    }
    let trailing_backslashes = pattern.chars().rev().take_while(|&c| c == '\\').count();
    if trailing_backslashes % 2 == 1 {
        return Err(PatternError::TrailingBackslash);
    }
    let mut run = 0usize;
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                chars.next();
                run = 0;
            }
            '*' => {
                run += 1;
                if run >= 3 {
                    return Err(PatternError::TripleStar);
                }
            }
            _ => run = 0,
        }
    }
    Ok(())
}

/// Whether `name` matches `pattern` under the grammar above. ASCII case folds. The match is a table over token
/// index and character index filled from the end, so every (token, position) pair is decided once and the work
/// is bounded by `tokens * chars`, never by the number of ways stars could split the name. A token's row reads only
/// its own cells and the next token's row, so two rows hold the table.
pub fn matches(pattern: &str, name: &str) -> bool {
    let tokens = tokenize(pattern);
    let chars: Vec<char> = name.chars().collect();
    if tokens.len() > MAX_PATTERN_CHARS || chars.len() > MAX_NAME_CHARS {
        return false;
    }
    // row[c]: whether tokens[t..] matches chars[c..], for the token t being filled. next[c]: the same for t + 1.
    let mut next = vec![false; chars.len() + 1];
    next[chars.len()] = true;
    let mut row = vec![false; chars.len() + 1];
    for token in tokens.iter().rev() {
        for c in (0..=chars.len()).rev() {
            let at = chars.get(c).copied();
            row[c] = match *token {
                Token::AnyPath => next[c] || at.is_some_and(|_| row[c + 1]),
                Token::AnyRun => next[c] || at.is_some_and(|ch| ch != '/' && row[c + 1]),
                Token::AnyOne => at.is_some_and(|ch| ch != '/' && next[c + 1]),
                Token::Literal(want) => {
                    at.is_some_and(|ch| ch.eq_ignore_ascii_case(&want) && next[c + 1])
                }
            };
        }
        std::mem::swap(&mut row, &mut next);
    }
    next[0]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const CASES: &str = include_str!("../tests/fixtures/model_rules_cases.json");

    fn cases() -> Value {
        serde_json::from_str(CASES).expect("fixture parses")
    }

    fn matcher_group(name: &str) -> Vec<Value> {
        cases()["matcher"][name]
            .as_array()
            .unwrap_or_else(|| panic!("missing matcher group {name}"))
            .clone()
    }

    fn assert_matcher_group(name: &str) {
        for case in matcher_group(name) {
            let pattern = case["pattern"].as_str().expect("pattern");
            let target = case["name"].as_str().expect("name");
            let want = case["matches"].as_bool().expect("matches");
            assert_eq!(
                matches(pattern, target),
                want,
                "{pattern:?} against {target:?}"
            );
        }
    }

    #[test]
    fn a_backslash_quotes_the_next_character() {
        assert_matcher_group("a_backslash_quotes_the_next_character");
    }

    #[test]
    fn a_double_star_crosses_slashes() {
        assert_matcher_group("a_double_star_crosses_slashes");
    }

    #[test]
    fn a_pattern_without_a_wildcard_matches_its_own_text_alone() {
        assert_matcher_group("a_pattern_without_a_wildcard_matches_its_own_text_alone");
    }

    #[test]
    fn a_question_mark_takes_one_character_other_than_a_slash() {
        assert_matcher_group("a_question_mark_takes_one_character_other_than_a_slash");
    }

    #[test]
    fn a_single_star_stops_at_a_slash() {
        assert_matcher_group("a_single_star_stops_at_a_slash");
    }

    #[test]
    fn check_refuses_what_the_grammar_cannot_hold() {
        let cases = cases();
        for case in cases["check"].as_array().expect("check list") {
            let pattern = case["pattern"].as_str().expect("pattern");
            let result = check(pattern);
            match case["error"].as_str() {
                Some(text) => {
                    assert_eq!(result.unwrap_err().to_string(), text, "pattern {pattern:?}")
                }
                None => assert!(
                    result.is_ok(),
                    "pattern {pattern:?} should pass: {result:?}"
                ),
            }
        }
    }

    /// The bound test is built by length rather than spelled in the table, since its point is the size of the
    /// input, not its content.
    #[test]
    fn the_matcher_answers_no_match_past_its_bounds() {
        let within_pattern: String = "*a".repeat(128);
        let within_name = "a".repeat(256);
        assert_eq!(within_pattern.chars().count(), MAX_PATTERN_CHARS);
        assert_eq!(within_name.chars().count(), MAX_NAME_CHARS);

        assert!(
            matches(&within_pattern, &within_name),
            "a pattern and a name at the bound fill the table and match"
        );
        // 256 tokens, the last a literal b the name never holds. No split of the stars matches it, so this pair forces
        // every backtracking matcher through its full walk.
        let missing = format!("{}*b", "*a".repeat(127));
        let start = std::time::Instant::now();
        let missed = matches(&missing, &within_name);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "a table-filled match must stay bounded, never explore one path per star split"
        );
        assert!(
            !missed,
            "a pattern at the bound whose last literal the name lacks answers no match"
        );

        let over_pattern = format!("{within_pattern}a");
        assert!(
            !matches(&over_pattern, &within_name),
            "a pattern past MAX_PATTERN_CHARS answers no match"
        );

        let over_name = "a".repeat(257);
        assert!(
            !matches(&within_pattern, &over_name),
            "a name past MAX_NAME_CHARS answers no match"
        );
    }
}
