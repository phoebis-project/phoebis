use crate::condition::{
    Condition, GuardrailAction, GuardrailMode, Reason, Selector, Subject, Test, MAX_CONDITIONS,
};
use crate::path::BodyPath;

pub const PATTERN_MAX_CHARS: usize = 2_048;
pub const COMPILED_SIZE_LIMIT: usize = 1 << 20;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CompileError {
    #[error("conditions holds 1 to {max} entries", max = MAX_CONDITIONS)]
    Count,
    #[error("conditions[{index}]: {reason}")]
    Condition { index: usize, reason: String },
}

/// `pattern does not compile: {message}` with the display's whitespace runs collapsed to one space, else the regex
/// under the size limit. The length bound is `Condition::check`'s, which every condition passes before it compiles.
pub(crate) fn compile_pattern(pattern: &str) -> Result<regex::Regex, Reason> {
    regex::RegexBuilder::new(pattern)
        .size_limit(COMPILED_SIZE_LIMIT)
        .build()
        .map_err(|error| {
            // The syntax error prints the pattern with a caret over several lines, and the API's sentence is one.
            let message = error
                .to_string()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            Reason(format!("pattern does not compile: {message}"))
        })
}

#[derive(Clone, Debug)]
pub enum CompiledTest {
    Absent,
    Present,
    Equals(String),
    Gt(f64),
    Lt(f64),
    Matches(regex::Regex),
}

#[derive(Clone, Debug)]
pub struct CompiledCondition {
    pub written: usize,
    pub subject: Subject,
    pub header: Option<String>,
    pub path: Option<BodyPath>,
    pub selector: Option<Selector>,
    pub test: CompiledTest,
}

#[derive(Clone, Debug)]
pub struct CompiledGuardrail {
    pub id: String,
    pub name: String,
    pub mode: GuardrailMode,
    pub action: GuardrailAction,
    /// Sorted by `Subject::rank`, stable, so conditions of one subject keep their written order.
    pub conditions: Vec<CompiledCondition>,
}

impl CompiledGuardrail {
    pub fn compile(
        id: &str,
        name: &str,
        mode: GuardrailMode,
        action: GuardrailAction,
        conditions: &[Condition],
    ) -> Result<CompiledGuardrail, CompileError> {
        if conditions.is_empty() || conditions.len() > MAX_CONDITIONS {
            return Err(CompileError::Count);
        }
        let mut compiled = Vec::with_capacity(conditions.len());
        for (index, condition) in conditions.iter().enumerate() {
            let refused = |reason: Reason| CompileError::Condition {
                index,
                reason: reason.0,
            };
            let checked = condition.check().map_err(refused)?;
            let test = match checked.test {
                Test::Absent => CompiledTest::Absent,
                Test::Present => CompiledTest::Present,
                Test::Equals(value) => CompiledTest::Equals(value),
                Test::Gt(bound) => CompiledTest::Gt(bound),
                Test::Lt(bound) => CompiledTest::Lt(bound),
                Test::Matches(pattern) => {
                    CompiledTest::Matches(compile_pattern(&pattern).map_err(refused)?)
                }
            };
            compiled.push(CompiledCondition {
                written: index,
                subject: checked.subject,
                header: checked.header,
                path: checked.path,
                selector: checked.selector,
                test,
            });
        }
        compiled.sort_by_key(|condition| condition.subject.rank());
        Ok(CompiledGuardrail {
            id: id.to_string(),
            name: name.to_string(),
            mode,
            action,
            conditions: compiled,
        })
    }

    /// `Enforce` and `Block`.
    pub fn enforcing(&self) -> bool {
        self.mode == GuardrailMode::Enforce && self.action == GuardrailAction::Block
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::condition::Operator;

    fn condition(subject: Subject, operator: Operator, value: Option<&str>) -> Condition {
        Condition {
            subject,
            operator,
            value: value.map(str::to_string),
            name: (subject == Subject::Header).then(|| "x-test".to_string()),
            path: (subject == Subject::Body).then(|| "tools".to_string()),
            selector: (subject == Subject::Message).then_some(Selector::LastUser),
        }
    }

    fn refusal(pattern: &str) -> String {
        compile_pattern(pattern).expect_err(pattern).to_string()
    }

    fn compile(conditions: &[Condition]) -> Result<CompiledGuardrail, CompileError> {
        CompiledGuardrail::compile(
            "g",
            "G",
            GuardrailMode::Enforce,
            GuardrailAction::Block,
            conditions,
        )
    }

    #[test]
    fn a_pattern_of_2048_characters_compiles_and_one_of_2049_is_refused() {
        let matching = |pattern: &str| condition(Subject::Model, Operator::Matches, Some(pattern));
        assert!(compile(&[matching(&"a".repeat(2_048))]).is_ok());
        assert!(compile(&[matching(&"\u{e9}".repeat(2_048))]).is_ok());
        assert_eq!(
            matching(&"a".repeat(2_049))
                .check()
                .unwrap_err()
                .to_string(),
            "pattern is longer than 2048 characters"
        );
        assert_eq!(
            matching(&"\u{e9}".repeat(2_049))
                .check()
                .unwrap_err()
                .to_string(),
            "pattern is longer than 2048 characters"
        );
    }

    #[test]
    fn a_pattern_past_the_size_limit_is_refused_with_a_one_line_message() {
        // The regex crate's default size limit admits the pattern, so the refusal below is the 1 MiB limit's own.
        assert!(regex::Regex::new(r"\w{100}").is_ok());
        let message = refusal(r"\w{100}");
        assert!(
            message.starts_with("pattern does not compile: "),
            "{message}"
        );
        assert!(!message.contains('\n'), "{message}");
        assert!(compile_pattern("[a-z]{1000}[a-z]{1000}").is_ok());
    }

    #[test]
    fn a_lookaround_is_refused_and_an_inline_flag_matches_across_case() {
        let message = refusal("(?=a)");
        assert!(
            message.starts_with("pattern does not compile: "),
            "{message}"
        );
        assert!(!message.contains('\n'), "{message}");
        assert!(!message.contains("  "), "{message}");
        assert!(compile_pattern("(?i)describe")
            .unwrap()
            .is_match("Describe"));
        assert!(!compile_pattern("describe").unwrap().is_match("Describe"));

        let written = [
            condition(Subject::Kind, Operator::Equals, Some("inference")),
            condition(Subject::Message, Operator::Matches, Some("(?=a)")),
        ];
        let refused = compile(&written)
            .expect_err("the lookaround is refused")
            .to_string();
        assert!(
            refused.starts_with("conditions[1]: pattern does not compile: "),
            "{refused}"
        );
    }

    #[test]
    fn the_compiler_sorts_by_the_cost_rank_and_keeps_the_written_index() {
        let written = [
            condition(Subject::Message, Operator::Matches, Some("^a")),
            condition(Subject::Body, Operator::Present, None),
            condition(Subject::Header, Operator::Present, None),
            condition(Subject::UserAgent, Operator::Absent, None),
            condition(Subject::Model, Operator::Equals, Some("m")),
            condition(Subject::Client, Operator::Equals, Some("c")),
            condition(Subject::AgentType, Operator::Absent, None),
            condition(Subject::RequestClass, Operator::Absent, None),
            condition(Subject::Kind, Operator::Equals, Some("inference")),
            condition(Subject::Body, Operator::Absent, None),
            condition(Subject::Kind, Operator::Equals, Some("count_tokens")),
        ];
        let guardrail = compile(&written).expect("the guardrail compiles");
        let order: Vec<(Subject, usize)> = guardrail
            .conditions
            .iter()
            .map(|c| (c.subject, c.written))
            .collect();
        assert_eq!(
            order,
            [
                (Subject::Kind, 8),
                (Subject::Kind, 10),
                (Subject::RequestClass, 7),
                (Subject::AgentType, 6),
                (Subject::Client, 5),
                (Subject::Model, 4),
                (Subject::UserAgent, 3),
                (Subject::Header, 2),
                (Subject::Body, 1),
                (Subject::Body, 9),
                (Subject::Message, 0),
            ]
        );
        assert_eq!((guardrail.id.as_str(), guardrail.name.as_str()), ("g", "G"));
        assert!(guardrail.enforcing());
    }

    #[test]
    fn the_count_is_refused_at_zero_and_at_seventeen() {
        let one = condition(Subject::Kind, Operator::Equals, Some("inference"));
        assert_eq!(compile(&[]).unwrap_err(), CompileError::Count);
        assert_eq!(
            CompileError::Count.to_string(),
            "conditions holds 1 to 16 entries"
        );
        assert_eq!(
            compile(&vec![one.clone(); MAX_CONDITIONS + 1]).unwrap_err(),
            CompileError::Count
        );
        assert!(compile(&vec![one.clone(); MAX_CONDITIONS]).is_ok());
        assert!(compile(std::slice::from_ref(&one)).is_ok());

        let unnamed = Condition {
            name: None,
            ..condition(Subject::Header, Operator::Present, None)
        };
        assert_eq!(
            compile(&[one, unnamed]).unwrap_err().to_string(),
            "conditions[1]: name is required"
        );
    }
}
