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
    compile_pattern, CompileError, CompiledCondition, CompiledGuardrail, CompiledTest,
    COMPILED_SIZE_LIMIT, PATTERN_MAX_CHARS,
};
pub use condition::{
    Checked, Condition, ConditionWords, GuardrailAction, GuardrailMode, Operator, Reason, Selector,
    Subject, Test, MAX_CONDITIONS,
};
pub use evaluate::{
    evaluate, evaluate_all, texts, Evaluation, HeaderLookup, HintLookup, Lookup, Match,
    RequestFacts, Verdict,
};
pub use name::client_phrase;
pub use path::{BodyPath, PathError, Segment, PATH_MAX_CHARS, PATH_MAX_SEGMENTS};
pub use presets::{presets, Preset};
