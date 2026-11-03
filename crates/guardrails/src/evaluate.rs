use serde_json::Value;

use crate::compile::{CompiledCondition, CompiledGuardrail, CompiledTest};
use crate::condition::{Selector, Subject};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup<'a> {
    Value(&'a str),
    Absent,
    Unknown,
}

pub trait HeaderLookup {
    fn get(&self, name: &str) -> Lookup<'_>;
}

// The client's hint headers, under the names the relay reads them by and `HintLookup` answers for, so a guardrail
// run over recorded rows reads each hint from the header the live evaluation read it from.
/// The hint that names a subagent's type, which the client sends on the subagent's own turns.
pub const AGENT_TYPE_HEADER: &str = "x-claude-code-agent-type";
/// The hint the client sends on the summarization request of a compaction.
pub const COMPACTION_HEADER: &str = "x-claude-code-compaction";
/// The hint the client sends once, on the first request after a compaction.
pub const CONTEXT_COMPACTED_HEADER: &str = "x-claude-code-context-compacted";
/// The hint that names the request's class, such as `compaction`, `main` or `subagent`.
pub const REQUEST_CLASS_HEADER: &str = "x-claude-code-request-class";

/// The five hints a request row keeps, answering `Unknown` for every other header name.
#[derive(Clone, Copy, Debug, Default)]
pub struct HintLookup<'a> {
    pub request_class: Option<&'a str>,
    pub agent_type: Option<&'a str>,
    pub user_agent: Option<&'a str>,
    pub compaction: Option<&'a str>,
    pub context_compacted: Option<&'a str>,
}

impl HeaderLookup for HintLookup<'_> {
    fn get(&self, name: &str) -> Lookup<'_> {
        let hint = match name {
            REQUEST_CLASS_HEADER => self.request_class,
            AGENT_TYPE_HEADER => self.agent_type,
            "user-agent" => self.user_agent,
            COMPACTION_HEADER => self.compaction,
            CONTEXT_COMPACTED_HEADER => self.context_compacted,
            _ => return Lookup::Unknown,
        };
        hint.map_or(Lookup::Absent, Lookup::Value)
    }
}

pub struct RequestFacts<'a> {
    /// The kind's word, `inference` or `count_tokens`, as `phoebis_bus::Kind::as_str` spells it and the row stores it.
    pub kind: &'a str,
    pub request_class: Option<&'a str>,
    pub agent_type: Option<&'a str>,
    pub client: &'a str,
    pub model: &'a str,
    pub user_agent: Option<&'a str>,
    pub headers: &'a dyn HeaderLookup,
    pub body: Option<&'a Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Matched,
    Unmatched { failed: usize },
    Indeterminate { condition: usize },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    pub id: String,
    pub position: u32,
    pub blocked: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Evaluation {
    pub matched: Vec<Match>,
    /// The index into `matched` of the entry marked `blocked: true`, the first enforcing guardrail that matched. Its
    /// set position is `matched[i].position`, so no caller indexes the set with it.
    pub blocked: Option<usize>,
}

pub fn evaluate(guardrail: &CompiledGuardrail, facts: &RequestFacts<'_>) -> Verdict {
    let mut unknown = None;
    for condition in &guardrail.conditions {
        match decides(condition, facts) {
            Some(true) => {}
            Some(false) => {
                return Verdict::Unmatched {
                    failed: condition.written,
                }
            }
            // Set aside rather than stopping, since a later failure leaves the request unmatched whatever the
            // header reads.
            None => {
                unknown.get_or_insert(condition.written);
            }
        }
    }
    match unknown {
        Some(condition) => Verdict::Indeterminate { condition },
        None => Verdict::Matched,
    }
}

pub fn evaluate_all<'g>(
    set: impl IntoIterator<Item = &'g CompiledGuardrail>,
    facts: &RequestFacts<'_>,
) -> Evaluation {
    let mut evaluation = Evaluation::default();
    for (position, guardrail) in set.into_iter().enumerate() {
        if evaluate(guardrail, facts) != Verdict::Matched {
            continue;
        }
        let blocked = evaluation.blocked.is_none() && guardrail.enforcing();
        if blocked {
            evaluation.blocked = Some(evaluation.matched.len());
        }
        evaluation.matched.push(Match {
            id: guardrail.id.clone(),
            position: position as u32,
            blocked,
        });
    }
    evaluation
}

/// The `text` blocks of a message in order: a string content is one text, an array yields each `text` block.
pub fn texts(message: &Value) -> Box<dyn Iterator<Item = &str> + '_> {
    match message.get("content") {
        Some(Value::String(s)) => Box::new(std::iter::once(s.as_str())),
        Some(Value::Array(blocks)) => Box::new(
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str)),
        ),
        _ => Box::new(std::iter::empty()),
    }
}

/// `None` where the condition reads a header the lookup cannot answer.
fn decides(condition: &CompiledCondition, facts: &RequestFacts<'_>) -> Option<bool> {
    match condition.subject {
        Subject::Body => Some(body_holds(condition, facts.body)),
        Subject::Message => Some(message_holds(condition, facts.body)),
        _ => holds(&condition.test, fact(condition, facts)),
    }
}

fn fact<'f>(condition: &CompiledCondition, facts: &'f RequestFacts<'_>) -> Lookup<'f> {
    let optional = |value: Option<&'f str>| value.map_or(Lookup::Absent, Lookup::Value);
    match condition.subject {
        Subject::Kind => Lookup::Value(facts.kind),
        Subject::RequestClass => optional(facts.request_class),
        Subject::AgentType => optional(facts.agent_type),
        Subject::Client => Lookup::Value(facts.client),
        Subject::Model => Lookup::Value(facts.model),
        Subject::UserAgent => optional(facts.user_agent),
        Subject::Header => match &condition.header {
            Some(name) => facts.headers.get(name),
            None => Lookup::Absent,
        },
        Subject::Body | Subject::Message => Lookup::Absent,
    }
}

fn holds(test: &CompiledTest, lookup: Lookup<'_>) -> Option<bool> {
    let value = match lookup {
        Lookup::Value(value) => Some(value),
        Lookup::Absent => None,
        Lookup::Unknown => return None,
    };
    Some(match test {
        CompiledTest::Absent => value.is_none(),
        CompiledTest::Present => value.is_some(),
        CompiledTest::Equals(expected) => value == Some(expected.as_str()),
        CompiledTest::Matches(pattern) => value.is_some_and(|value| pattern.is_match(value)),
        CompiledTest::Gt(_) | CompiledTest::Lt(_) => false,
    })
}

fn body_holds(condition: &CompiledCondition, body: Option<&Value>) -> bool {
    let Some(path) = &condition.path else {
        return false;
    };
    let resolves =
        |found: &mut dyn FnMut(&Value) -> bool| body.is_some_and(|root| path.any(root, found));
    match &condition.test {
        CompiledTest::Absent => !resolves(&mut |_| true),
        CompiledTest::Present => resolves(&mut |_| true),
        CompiledTest::Equals(expected) => resolves(&mut |value| scalar_equals(value, expected)),
        CompiledTest::Gt(bound) => {
            resolves(&mut |value| value.as_f64().is_some_and(|number| number > *bound))
        }
        CompiledTest::Lt(bound) => {
            resolves(&mut |value| value.as_f64().is_some_and(|number| number < *bound))
        }
        CompiledTest::Matches(pattern) => {
            resolves(&mut |value| value.as_str().is_some_and(|text| pattern.is_match(text)))
        }
    }
}

fn scalar_equals(value: &Value, expected: &str) -> bool {
    match value {
        Value::String(text) => text == expected,
        Value::Number(number) => number.to_string() == expected,
        Value::Bool(true) => expected == "true",
        Value::Bool(false) => expected == "false",
        Value::Null => expected == "null",
        Value::Array(_) | Value::Object(_) => false,
    }
}

fn message_holds(condition: &CompiledCondition, body: Option<&Value>) -> bool {
    let (CompiledTest::Matches(pattern), Some(selector)) = (&condition.test, condition.selector)
    else {
        return false;
    };
    let Some(messages) = body
        .and_then(|root| root.get("messages"))
        .and_then(Value::as_array)
    else {
        return false;
    };
    let matches = |entry: &Value| texts(entry).any(|text| pattern.is_match(text));
    let mut entries = messages.iter();
    match selector {
        Selector::Any => entries.any(matches),
        Selector::AnyAssistant => entries
            .filter(|entry| has_role(entry, "assistant"))
            .any(matches),
        Selector::AnyUser => entries.filter(|entry| has_role(entry, "user")).any(matches),
        Selector::FirstUser => entries
            .find(|entry| has_role(entry, "user"))
            .is_some_and(matches),
        Selector::Last => entries.next_back().is_some_and(matches),
        Selector::LastAssistant => entries
            .rfind(|entry| has_role(entry, "assistant"))
            .is_some_and(matches),
        // The client appends its per-turn control entry, role `system`, after the user's turn, so the last `user`
        // entry is the turn whatever follows it.
        Selector::LastUser => entries
            .rfind(|entry| has_role(entry, "user"))
            .is_some_and(matches),
    }
}

fn has_role(entry: &Value, role: &str) -> bool {
    entry.get("role").and_then(Value::as_str) == Some(role)
}
