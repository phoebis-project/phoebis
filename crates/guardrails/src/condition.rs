use serde::{Deserialize, Serialize};

use crate::compile::PATTERN_MAX_CHARS;
use crate::path::BodyPath;

pub const MAX_CONDITIONS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Subject {
    AgentType,
    Body,
    Client,
    Header,
    Kind,
    Message,
    Model,
    RequestClass,
    UserAgent,
}

impl Subject {
    const ALL: &[Subject] = &[
        Subject::AgentType,
        Subject::Body,
        Subject::Client,
        Subject::Header,
        Subject::Kind,
        Subject::Message,
        Subject::Model,
        Subject::RequestClass,
        Subject::UserAgent,
    ];

    /// The JSON word.
    pub fn as_str(self) -> &'static str {
        match self {
            Subject::AgentType => "agent_type",
            Subject::Body => "body",
            Subject::Client => "client",
            Subject::Header => "header",
            Subject::Kind => "kind",
            Subject::Message => "message",
            Subject::Model => "model",
            Subject::RequestClass => "request_class",
            Subject::UserAgent => "user_agent",
        }
    }

    pub fn parse(word: &str) -> Option<Subject> {
        word_of(Subject::ALL, Subject::as_str, word)
    }

    /// The operators the subject takes, alphabetical.
    pub fn operators(self) -> &'static [Operator] {
        use Operator::{Absent, Equals, Gt, Lt, Matches, Present};
        match self {
            Subject::AgentType | Subject::RequestClass | Subject::UserAgent => {
                &[Absent, Equals, Matches]
            }
            Subject::Body => &[Absent, Equals, Gt, Lt, Matches, Present],
            Subject::Client | Subject::Model => &[Equals, Matches],
            Subject::Header => &[Absent, Equals, Matches, Present],
            Subject::Kind => &[Equals],
            Subject::Message => &[Matches],
        }
    }

    /// The evaluation order, the cheap subjects first, then the body walk, then the message regex: kind 0,
    /// request_class 1, agent_type 2, client 3, model 4, user_agent 5, header 6, body 7, message 8.
    pub fn rank(self) -> u8 {
        match self {
            Subject::Kind => 0,
            Subject::RequestClass => 1,
            Subject::AgentType => 2,
            Subject::Client => 3,
            Subject::Model => 4,
            Subject::UserAgent => 5,
            Subject::Header => 6,
            Subject::Body => 7,
            Subject::Message => 8,
        }
    }

    /// Whether a condition on the subject reads the body itself: the body walk and the message regex do, and every
    /// other subject reads a fact the request carries beside it.
    pub fn reads_body(self) -> bool {
        match self {
            Subject::Body | Subject::Message => true,
            Subject::AgentType
            | Subject::Client
            | Subject::Header
            | Subject::Kind
            | Subject::Model
            | Subject::RequestClass
            | Subject::UserAgent => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operator {
    Absent,
    Equals,
    Gt,
    Lt,
    Matches,
    Present,
}

impl Operator {
    const ALL: &[Operator] = &[
        Operator::Absent,
        Operator::Equals,
        Operator::Gt,
        Operator::Lt,
        Operator::Matches,
        Operator::Present,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Operator::Absent => "absent",
            Operator::Equals => "equals",
            Operator::Gt => "gt",
            Operator::Lt => "lt",
            Operator::Matches => "matches",
            Operator::Present => "present",
        }
    }

    pub fn parse(word: &str) -> Option<Operator> {
        word_of(Operator::ALL, Operator::as_str, word)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Selector {
    Any,
    AnyAssistant,
    AnyUser,
    FirstUser,
    Last,
    LastAssistant,
    LastUser,
}

impl Selector {
    const ALL: &[Selector] = &[
        Selector::Any,
        Selector::AnyAssistant,
        Selector::AnyUser,
        Selector::FirstUser,
        Selector::Last,
        Selector::LastAssistant,
        Selector::LastUser,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Selector::Any => "any",
            Selector::AnyAssistant => "any_assistant",
            Selector::AnyUser => "any_user",
            Selector::FirstUser => "first_user",
            Selector::Last => "last",
            Selector::LastAssistant => "last_assistant",
            Selector::LastUser => "last_user",
        }
    }

    pub fn parse(word: &str) -> Option<Selector> {
        word_of(Selector::ALL, Selector::as_str, word)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardrailMode {
    Enforce,
    Observe,
}

impl GuardrailMode {
    const ALL: &[GuardrailMode] = &[GuardrailMode::Enforce, GuardrailMode::Observe];

    pub fn as_str(self) -> &'static str {
        match self {
            GuardrailMode::Enforce => "enforce",
            GuardrailMode::Observe => "observe",
        }
    }

    pub fn parse(word: &str) -> Option<GuardrailMode> {
        word_of(GuardrailMode::ALL, GuardrailMode::as_str, word)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardrailAction {
    Block,
}

impl GuardrailAction {
    const ALL: &[GuardrailAction] = &[GuardrailAction::Block];

    pub fn as_str(self) -> &'static str {
        match self {
            GuardrailAction::Block => "block",
        }
    }

    pub fn parse(word: &str) -> Option<GuardrailAction> {
        word_of(GuardrailAction::ALL, GuardrailAction::as_str, word)
    }
}

/// A word read back through the enum's own `as_str`, so each word is spelled once.
fn word_of<T: Copy>(all: &[T], as_str: fn(T) -> &'static str, word: &str) -> Option<T> {
    all.iter().copied().find(|v| as_str(*v) == word)
}

/// One condition as the row stores it: the typed words, the optional members as the subject or the operator takes
/// them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Condition {
    pub subject: Subject,
    pub operator: Operator,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<Selector>,
}

/// A condition as the API and the console spell it, every member a string, so an unknown word is a reason rather
/// than a deserialization failure.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConditionWords {
    pub subject: String,
    pub operator: String,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub selector: Option<String>,
}

/// The reason a condition is refused, the text after `conditions[{i}]: `.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Reason(pub String);

impl Condition {
    /// `unknown subject {word}`, `unknown operator {word}` or `unknown selector {word}`, else the typed condition.
    pub fn from_words(words: &ConditionWords) -> Result<Condition, Reason> {
        let subject = Subject::parse(&words.subject)
            .ok_or_else(|| Reason(format!("unknown subject {}", words.subject)))?;
        let operator = Operator::parse(&words.operator)
            .ok_or_else(|| Reason(format!("unknown operator {}", words.operator)))?;
        let selector = match &words.selector {
            Some(word) => Some(
                Selector::parse(word).ok_or_else(|| Reason(format!("unknown selector {word}")))?,
            ),
            None => None,
        };
        Ok(Condition {
            subject,
            operator,
            value: words.value.clone(),
            name: words.name.clone(),
            path: words.path.clone(),
            selector,
        })
    }

    pub fn words(&self) -> ConditionWords {
        ConditionWords {
            subject: self.subject.as_str().to_string(),
            operator: self.operator.as_str().to_string(),
            value: self.value.clone(),
            name: self.name.clone(),
            path: self.path.clone(),
            selector: self.selector.map(|selector| selector.as_str().to_string()),
        }
    }

    /// Every validation rule but the compile: the member rules in the order name, path, selector, then the
    /// operator's row, then the value. The probe comes back in the one shape the subject admits, the header name
    /// lowercased, the path parsed, the number parsed.
    pub fn check(&self) -> Result<Probe<String>, Reason> {
        let subject = self.subject;
        let header = match &self.name {
            Some(_) if subject != Subject::Header => {
                return Err(not_applicable("name", subject.as_str()))
            }
            Some(name) => {
                // `HeaderMap::get` takes lowercase names, and a name outside the token charset can never arrive.
                let name = name.to_ascii_lowercase();
                if !is_header_token(&name) {
                    return Err(reason("name is not a header token"));
                }
                Some(name)
            }
            None if subject == Subject::Header => return Err(reason("name is required")),
            None => None,
        };
        let path = match &self.path {
            Some(_) if subject != Subject::Body => {
                return Err(not_applicable("path", subject.as_str()))
            }
            Some(text) => Some(BodyPath::parse(text).map_err(|error| Reason(error.to_string()))?),
            None if subject == Subject::Body => return Err(reason("path is required")),
            None => None,
        };
        let selector = match self.selector {
            Some(_) if subject != Subject::Message => {
                return Err(not_applicable("selector", subject.as_str()))
            }
            None if subject == Subject::Message => return Err(reason("selector is required")),
            selector => selector,
        };
        let operator = self.operator;
        if !subject.operators().contains(&operator) {
            return Err(not_applicable(
                &format!("operator {}", operator.as_str()),
                subject.as_str(),
            ));
        }
        let test = match (operator, self.value.as_deref()) {
            (Operator::Absent | Operator::Present, Some(_)) => {
                return Err(not_applicable("value", operator.as_str()));
            }
            (Operator::Absent, None) => BodyTest::Absent,
            (Operator::Present, None) => BodyTest::Present,
            (_, None) => return Err(reason("value is required")),
            (Operator::Gt, Some(value)) => BodyTest::Gt(number(value)?),
            (Operator::Lt, Some(value)) => BodyTest::Lt(number(value)?),
            (Operator::Matches, Some(pattern)) => {
                if pattern.chars().count() > PATTERN_MAX_CHARS {
                    return Err(Reason(format!(
                        "pattern is longer than {PATTERN_MAX_CHARS} characters"
                    )));
                }
                BodyTest::Matches(pattern.to_string())
            }
            (Operator::Equals, Some(value)) => {
                if subject == Subject::Kind && !matches!(value, "inference" | "count_tokens") {
                    return Err(reason("kind is inference or count_tokens"));
                }
                BodyTest::Equals(value.to_string())
            }
        };
        // The rules above admit each member on its own subject alone and each operator on its row alone, so every
        // arm below finds what it reads. An arm that did not would refuse as the rule it stands behind does.
        let row = || not_applicable(&format!("operator {}", operator.as_str()), subject.as_str());
        let fact_test = |test: BodyTest<String>| match test {
            BodyTest::Absent => Ok(FactTest::Absent),
            BodyTest::Present => Ok(FactTest::Present),
            BodyTest::Equals(value) => Ok(FactTest::Equals(value)),
            BodyTest::Matches(pattern) => Ok(FactTest::Matches(pattern)),
            BodyTest::Gt(_) | BodyTest::Lt(_) => Err(row()),
        };
        Ok(match subject {
            Subject::Header => Probe::Header(
                header.ok_or_else(|| reason("name is required"))?,
                fact_test(test)?,
            ),
            Subject::Body => Probe::Body(path.ok_or_else(|| reason("path is required"))?, test),
            Subject::Message => {
                let selector = selector.ok_or_else(|| reason("selector is required"))?;
                match test {
                    BodyTest::Matches(pattern) => Probe::Message(selector, pattern),
                    _ => return Err(row()),
                }
            }
            Subject::AgentType => Probe::Fact(FactSubject::AgentType, fact_test(test)?),
            Subject::Client => Probe::Fact(FactSubject::Client, fact_test(test)?),
            Subject::Kind => Probe::Fact(FactSubject::Kind, fact_test(test)?),
            Subject::Model => Probe::Fact(FactSubject::Model, fact_test(test)?),
            Subject::RequestClass => Probe::Fact(FactSubject::RequestClass, fact_test(test)?),
            Subject::UserAgent => Probe::Fact(FactSubject::UserAgent, fact_test(test)?),
        })
    }
}

const HEADER_NAME_MAX_BYTES: usize = 128;

fn is_header_token(name: &str) -> bool {
    (1..=HEADER_NAME_MAX_BYTES).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

// `str::parse` accepts `NaN` and the infinities, which bound nothing, so only a finite value is a number.
fn number(value: &str) -> Result<f64, Reason> {
    value
        .parse::<f64>()
        .ok()
        .filter(|number| number.is_finite())
        .ok_or_else(|| reason("value is not a number"))
}

fn reason(text: &str) -> Reason {
    Reason(text.to_string())
}

fn not_applicable(member: &str, word: &str) -> Reason {
    Reason(format!("{member} does not apply to {word}"))
}

/// A checked condition in the one shape its subject admits, generic over its pattern: the written text after `check`,
/// the compiled regex after the compile. A header always has its name, a body test its path and a message its selector
/// and its pattern, so the evaluator reads no shape `check` refuses.
#[derive(Clone, Debug, PartialEq)]
pub enum Probe<P> {
    Fact(FactSubject, FactTest<P>),
    Header(String, FactTest<P>),
    Body(BodyPath, BodyTest<P>),
    Message(Selector, P),
}

impl<P> Probe<P> {
    pub fn subject(&self) -> Subject {
        match self {
            Probe::Fact(subject, _) => Subject::from(*subject),
            Probe::Header(..) => Subject::Header,
            Probe::Body(..) => Subject::Body,
            Probe::Message(..) => Subject::Message,
        }
    }

    /// The same probe over the pattern `compile` makes of the written one.
    pub(crate) fn try_map<Q, E>(
        self,
        compile: impl FnOnce(P) -> Result<Q, E>,
    ) -> Result<Probe<Q>, E> {
        Ok(match self {
            Probe::Fact(subject, test) => Probe::Fact(subject, test.try_map(compile)?),
            Probe::Header(name, test) => Probe::Header(name, test.try_map(compile)?),
            Probe::Body(path, test) => Probe::Body(path, test.try_map(compile)?),
            Probe::Message(selector, pattern) => Probe::Message(selector, compile(pattern)?),
        })
    }
}

/// The six subjects `RequestFacts` answers by name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FactSubject {
    AgentType,
    Client,
    Kind,
    Model,
    RequestClass,
    UserAgent,
}

impl From<FactSubject> for Subject {
    fn from(subject: FactSubject) -> Subject {
        match subject {
            FactSubject::AgentType => Subject::AgentType,
            FactSubject::Client => Subject::Client,
            FactSubject::Kind => Subject::Kind,
            FactSubject::Model => Subject::Model,
            FactSubject::RequestClass => Subject::RequestClass,
            FactSubject::UserAgent => Subject::UserAgent,
        }
    }
}

/// A test on one value that is there or not, a fact's or a header's.
#[derive(Clone, Debug, PartialEq)]
pub enum FactTest<P> {
    Absent,
    Present,
    Equals(String),
    Matches(P),
}

impl<P> FactTest<P> {
    fn try_map<Q, E>(self, compile: impl FnOnce(P) -> Result<Q, E>) -> Result<FactTest<Q>, E> {
        Ok(match self {
            FactTest::Absent => FactTest::Absent,
            FactTest::Present => FactTest::Present,
            FactTest::Equals(value) => FactTest::Equals(value),
            FactTest::Matches(pattern) => FactTest::Matches(compile(pattern)?),
        })
    }
}

/// A test on the values a body path resolves to, the one subject that compares numbers.
#[derive(Clone, Debug, PartialEq)]
pub enum BodyTest<P> {
    Absent,
    Present,
    Equals(String),
    Gt(f64),
    Lt(f64),
    Matches(P),
}

impl<P> BodyTest<P> {
    fn try_map<Q, E>(self, compile: impl FnOnce(P) -> Result<Q, E>) -> Result<BodyTest<Q>, E> {
        Ok(match self {
            BodyTest::Absent => BodyTest::Absent,
            BodyTest::Present => BodyTest::Present,
            BodyTest::Equals(value) => BodyTest::Equals(value),
            BodyTest::Gt(bound) => BodyTest::Gt(bound),
            BodyTest::Lt(bound) => BodyTest::Lt(bound),
            BodyTest::Matches(pattern) => BodyTest::Matches(compile(pattern)?),
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn words(subject: &str, operator: &str) -> ConditionWords {
        ConditionWords {
            subject: subject.to_string(),
            operator: operator.to_string(),
            ..ConditionWords::default()
        }
    }

    fn condition(subject: Subject, operator: Operator) -> Condition {
        Condition {
            subject,
            operator,
            value: None,
            name: None,
            path: None,
            selector: None,
        }
    }

    fn refusal(condition: Condition) -> String {
        condition
            .check()
            .expect_err("the condition is refused")
            .to_string()
    }

    fn with_value(condition: Condition, value: &str) -> Condition {
        Condition {
            value: Some(value.to_string()),
            ..condition
        }
    }

    fn with_name(condition: Condition, name: &str) -> Condition {
        Condition {
            name: Some(name.to_string()),
            ..condition
        }
    }

    fn with_path(condition: Condition, path: &str) -> Condition {
        Condition {
            path: Some(path.to_string()),
            ..condition
        }
    }

    fn with_selector(condition: Condition, selector: Selector) -> Condition {
        Condition {
            selector: Some(selector),
            ..condition
        }
    }

    #[test]
    fn every_reason_of_the_validation_table_is_answered_for_its_case() {
        use Operator::{Absent, Equals, Gt, Lt, Matches, Present};
        use Subject::{Body, Header, Kind, Message, Model};

        assert_eq!(
            refusal(with_value(condition(Kind, Equals), "probe")),
            "kind is inference or count_tokens"
        );
        assert_eq!(
            refusal(with_name(
                with_path(condition(Body, Present), "tools"),
                "x-test"
            )),
            "name does not apply to body"
        );
        for name in ["x test", "", "x-t\u{e9}st", "x:test", &"x".repeat(129)] {
            assert_eq!(
                refusal(with_name(condition(Header, Present), name)),
                "name is not a header token",
                "{name}"
            );
        }
        assert_eq!(refusal(condition(Header, Present)), "name is required");
        assert_eq!(
            refusal(with_value(
                with_selector(condition(Message, Equals), Selector::Last),
                "x"
            )),
            "operator equals does not apply to message"
        );
        assert_eq!(
            refusal(with_value(condition(Kind, Matches), "x")),
            "operator matches does not apply to kind"
        );
        assert_eq!(
            refusal(with_path(condition(Body, Present), "")),
            "path is empty"
        );
        assert_eq!(
            refusal(with_path(condition(Body, Present), "a..b")),
            "path has an empty member at segment 2"
        );
        assert_eq!(
            refusal(with_path(
                with_value(condition(Model, Equals), "x"),
                "model"
            )),
            "path does not apply to model"
        );
        assert_eq!(refusal(condition(Body, Present)), "path is required");
        assert_eq!(
            refusal(with_value(condition(Model, Matches), &"a".repeat(2_049))),
            "pattern is longer than 2048 characters"
        );
        assert_eq!(
            refusal(with_selector(
                with_path(condition(Body, Present), "tools"),
                Selector::Any
            )),
            "selector does not apply to body"
        );
        assert_eq!(
            refusal(with_value(condition(Message, Matches), "x")),
            "selector is required"
        );
        assert_eq!(
            refusal(with_value(
                with_path(condition(Body, Present), "tools"),
                "x"
            )),
            "value does not apply to present"
        );
        assert_eq!(
            refusal(with_value(
                with_name(condition(Header, Absent), "x-test"),
                ""
            )),
            "value does not apply to absent"
        );
        for (operator, value) in [
            (Gt, "many"),
            (Lt, ""),
            (Gt, "NaN"),
            (Lt, "inf"),
            (Gt, "-infinity"),
        ] {
            assert_eq!(
                refusal(with_value(
                    with_path(condition(Body, operator), "max_tokens"),
                    value
                )),
                "value is not a number",
                "{value}"
            );
        }
        for operator in [Equals, Gt, Lt, Matches] {
            assert_eq!(
                refusal(with_path(condition(Body, operator), "max_tokens")),
                "value is required"
            );
        }

        for (written, reason) in [
            (words("cookie", "equals"), "unknown subject cookie"),
            (words("body", "contains"), "unknown operator contains"),
            (
                ConditionWords {
                    selector: Some("middle".to_string()),
                    ..words("message", "matches")
                },
                "unknown selector middle",
            ),
        ] {
            assert_eq!(
                Condition::from_words(&written)
                    .expect_err("the word is refused")
                    .to_string(),
                reason
            );
        }

        let unknown_member =
            json!({"subject": "kind", "operator": "equals", "value": "inference", "flags": "x"});
        assert!(serde_json::from_value::<Condition>(unknown_member.clone()).is_err());
        assert!(serde_json::from_value::<ConditionWords>(unknown_member).is_err());

        let Probe::Body(_, empty) = with_value(with_path(condition(Body, Equals), "model"), "")
            .check()
            .expect("an empty value is a value")
        else {
            panic!("a body condition checks to a body probe");
        };
        assert_eq!(empty, BodyTest::Equals(String::new()));
        let Probe::Header(header, _) = with_name(condition(Header, Present), "X-Test")
            .check()
            .expect("a header token checks")
        else {
            panic!("a header condition checks to a header probe");
        };
        assert_eq!(header, "x-test");
        let Probe::Body(path, number) =
            with_value(with_path(condition(Body, Gt), "max_tokens"), "1e2")
                .check()
                .expect("a number checks")
        else {
            panic!("a body condition checks to a body probe");
        };
        assert_eq!(number, BodyTest::Gt(100.0));
        assert_eq!(path, BodyPath::parse("max_tokens").unwrap());
        assert!(with_value(condition(Kind, Equals), "count_tokens")
            .check()
            .is_ok());
        assert!(with_name(condition(Header, Present), &"x".repeat(128))
            .check()
            .is_ok());
        assert!(
            with_name(condition(Header, Present), "!#$%&'*+.^_`|~09AZaz-")
                .check()
                .is_ok()
        );
    }

    #[test]
    fn from_words_and_words_round_trip_a_subject_an_operator_and_a_selector() {
        // The subject and the selector are words of two parts, so serde's snake_case spelling meets `parse` and
        // `as_str` at the underscore.
        let subject = Subject::parse("request_class").expect("a subject word parses");
        assert_eq!(subject.as_str(), "request_class");
        assert_eq!(
            serde_json::to_value(subject).unwrap(),
            json!("request_class")
        );
        assert_eq!(Subject::parse("Kind"), None);
        let operator = Operator::parse("present").expect("an operator word parses");
        assert_eq!(operator.as_str(), "present");
        assert_eq!(serde_json::to_value(operator).unwrap(), json!("present"));
        assert_eq!(Operator::parse("contains"), None);
        let selector = Selector::parse("last_assistant").expect("a selector word parses");
        assert_eq!(selector.as_str(), "last_assistant");
        assert_eq!(
            serde_json::to_value(selector).unwrap(),
            json!("last_assistant")
        );
        assert_eq!(Selector::parse("middle"), None);
        for mode in [GuardrailMode::Enforce, GuardrailMode::Observe] {
            assert_eq!(GuardrailMode::parse(mode.as_str()), Some(mode));
            assert_eq!(serde_json::to_value(mode).unwrap(), json!(mode.as_str()));
        }
        assert_eq!(GuardrailMode::parse("audit"), None);
        assert_eq!(
            GuardrailAction::parse("block"),
            Some(GuardrailAction::Block)
        );
        assert_eq!(GuardrailAction::Block.as_str(), "block");
        assert_eq!(GuardrailAction::parse("allow"), None);

        let written = ConditionWords {
            subject: "request_class".to_string(),
            operator: "present".to_string(),
            value: Some("v".to_string()),
            name: Some("n".to_string()),
            path: Some("p".to_string()),
            selector: Some("last_assistant".to_string()),
        };
        let condition = Condition::from_words(&written).expect("known words parse");
        assert_eq!(condition.words(), written);
        let bare = words("kind", "equals");
        assert_eq!(Condition::from_words(&bare).unwrap().words(), bare);
    }
}
