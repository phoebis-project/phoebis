//! The guardrails: the condition model and its validation, the body path grammar, the compiled form and the
//! evaluator the relay and the admin API share, and the presets the binary ships.

pub mod body;
pub mod compile;
pub mod condition;
pub mod evaluate;
pub mod name;
pub mod path;
pub mod presets;

pub use body::parse_body;
pub use compile::{
    CompileError, CompiledCondition, CompiledGuardrail, COMPILED_SIZE_LIMIT, PATTERN_MAX_CHARS,
};
pub use condition::{
    BodyTest, Condition, ConditionWords, FactSubject, FactTest, GuardrailAction, GuardrailMode,
    Operator, Probe, Reason, Selector, Subject, MAX_CONDITIONS,
};
pub use evaluate::{
    evaluate, evaluate_all, texts, Evaluation, HeaderLookup, HintLookup, Lookup, Match,
    RequestFacts, Verdict, AGENT_TYPE_HEADER, COMPACTION_HEADER, CONTEXT_COMPACTED_HEADER,
    REQUEST_CLASS_HEADER,
};
pub use name::client_phrase;
pub use path::{BodyPath, PathError, Segment, PATH_MAX_CHARS, PATH_MAX_SEGMENTS};
pub use presets::{presets, Preset};
