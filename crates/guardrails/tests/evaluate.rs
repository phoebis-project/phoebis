use phoebis_guardrails::{
    evaluate, evaluate_all, parse_body, texts, CompiledGuardrail, Condition, Evaluation,
    GuardrailAction, GuardrailMode, HeaderLookup, HintLookup, Lookup, Match, Operator, Probe,
    RequestFacts, Selector, Subject, Verdict,
};
use serde_json::{json, Value};

const FORK: &str = include_str!("fixtures/subagent_summary_fork.json");
const CLASSIFIER: &str = include_str!("fixtures/auto_mode_classifier.json");

fn fork() -> Value {
    serde_json::from_str(FORK).expect("the fork fixture parses")
}

fn classifier() -> Value {
    serde_json::from_str(CLASSIFIER).expect("the classifier fixture parses")
}

struct Headers(Vec<(&'static str, &'static str)>);

impl HeaderLookup for Headers {
    fn get(&self, name: &str) -> Lookup<'_> {
        self.0
            .iter()
            .find(|(listed, _)| *listed == name)
            .map_or(Lookup::Absent, |(_, value)| Lookup::Value(value))
    }
}

static NO_HEADERS: Headers = Headers(Vec::new());

fn facts<'a>(kind: &'a str, class: Option<&'a str>, body: Option<&'a Value>) -> RequestFacts<'a> {
    RequestFacts {
        kind,
        request_class: class,
        agent_type: None,
        client: "claude-code/cli",
        model: "claude-opus-5",
        user_agent: Some("claude-cli/2.1.300 (external, cli)"),
        headers: &NO_HEADERS,
        body,
    }
}

fn cond(
    subject: Subject,
    operator: Operator,
    value: Option<&str>,
    name: Option<&str>,
    path: Option<&str>,
    selector: Option<Selector>,
) -> Condition {
    Condition {
        subject,
        operator,
        value: value.map(str::to_string),
        name: name.map(str::to_string),
        path: path.map(str::to_string),
        selector,
    }
}

fn compiled(conditions: &[Condition], mode: GuardrailMode) -> CompiledGuardrail {
    CompiledGuardrail::compile("g", "G", mode, GuardrailAction::Block, conditions)
        .expect("the guardrail compiles")
}

fn holds(condition: Condition, facts: &RequestFacts<'_>) -> bool {
    match evaluate(&compiled(&[condition], GuardrailMode::Observe), facts) {
        Verdict::Matched => true,
        Verdict::Unmatched { failed: 0 } => false,
        other => panic!("one condition answers matched or its own failure, not {other:?}"),
    }
}

fn body(path: &str, operator: Operator, value: Option<&str>) -> Condition {
    cond(Subject::Body, operator, value, None, Some(path), None)
}

fn message(selector: Selector, pattern: &str) -> Condition {
    cond(
        Subject::Message,
        Operator::Matches,
        Some(pattern),
        None,
        None,
        Some(selector),
    )
}

fn preset_conditions() -> Vec<Condition> {
    vec![
        cond(
            Subject::RequestClass,
            Operator::Equals,
            Some("auxiliary"),
            None,
            None,
            None,
        ),
        body("tools", Operator::Present, None),
        message(
            Selector::LastUser,
            "^Describe your most recent action in 3-5 words",
        ),
    ]
}

#[test]
fn the_three_preset_conditions_built_by_hand_match_the_fork_and_not_the_classifier() {
    let guardrail = compiled(&preset_conditions(), GuardrailMode::Observe);
    let fork = fork();
    let classifier = classifier();
    assert_eq!(
        evaluate(
            &guardrail,
            &facts("inference", Some("auxiliary"), Some(&fork))
        ),
        Verdict::Matched
    );
    assert_eq!(
        evaluate(&guardrail, &facts("inference", None, Some(&fork))),
        Verdict::Unmatched { failed: 0 }
    );
    assert_eq!(
        evaluate(
            &guardrail,
            &facts("inference", Some("auxiliary"), Some(&classifier))
        ),
        Verdict::Unmatched { failed: 1 }
    );
}

#[test]
fn a_fork_without_the_class_header_matches_the_two_body_conditions_alone() {
    let guardrail = compiled(&preset_conditions()[1..], GuardrailMode::Observe);
    let fork = fork();
    let classifier = classifier();
    assert_eq!(
        evaluate(&guardrail, &facts("inference", None, Some(&fork))),
        Verdict::Matched
    );
    assert_eq!(
        evaluate(&guardrail, &facts("inference", None, Some(&classifier))),
        Verdict::Unmatched { failed: 0 }
    );
}

#[test]
fn every_selector_reads_its_entries_of_the_fork() {
    let fork = fork();
    let facts = facts("inference", None, Some(&fork));
    let reads = |selector, pattern| holds(message(selector, pattern), &facts);

    assert!(reads(Selector::Last, "^control"));
    assert!(!reads(Selector::Last, "^Describe"));

    assert!(reads(Selector::LastUser, "^Describe"));
    assert!(!reads(Selector::LastUser, "^control"));
    assert!(!reads(Selector::LastUser, "^Read the notes"));

    assert!(reads(Selector::FirstUser, "^Read the notes"));
    assert!(!reads(Selector::FirstUser, "^Describe"));

    assert!(reads(Selector::LastAssistant, "^Opening the notes"));
    assert!(!reads(Selector::LastAssistant, "^Read the notes"));
    assert!(reads(Selector::AnyAssistant, "^Opening the notes"));
    assert!(!reads(Selector::AnyAssistant, "^control"));

    assert!(reads(Selector::AnyUser, "^Read the notes"));
    assert!(reads(Selector::AnyUser, "^Describe"));
    assert!(!reads(Selector::AnyUser, "^control"));
    assert!(!reads(Selector::AnyUser, "^Opening the notes"));

    assert!(reads(Selector::Any, "^Read the notes"));
    assert!(reads(Selector::Any, "^Opening the notes"));
    assert!(reads(Selector::Any, "^Describe"));
    assert!(reads(Selector::Any, "^control"));
    // The tool result's text is not a text block of its entry.
    assert!(!reads(Selector::Any, "three items"));
}

#[test]
fn a_message_condition_fails_without_a_messages_array_or_a_selected_entry() {
    let empty = json!({});
    assert!(!holds(
        message(Selector::Any, ".*"),
        &facts("inference", None, Some(&empty))
    ));
    let not_an_array = json!({"messages": "Describe"});
    assert!(!holds(
        message(Selector::Any, ".*"),
        &facts("inference", None, Some(&not_an_array))
    ));
    let assistant_only = json!({"messages": [{"role": "assistant", "content": "hello"}]});
    let facts = facts("inference", None, Some(&assistant_only));
    assert!(!holds(message(Selector::FirstUser, ".*"), &facts));
    assert!(!holds(message(Selector::LastUser, ".*"), &facts));
    assert!(!holds(message(Selector::AnyUser, ".*"), &facts));
    assert!(holds(message(Selector::LastAssistant, ".*"), &facts));
}

#[test]
fn every_operator_holds_and_fails_on_the_subjects_it_applies_to() {
    use Operator::{Absent, Equals, Gt, Lt, Matches, Present};
    let fact =
        |subject, operator, value: Option<&str>| cond(subject, operator, value, None, None, None);

    let without = facts("inference", None, None);
    let with = RequestFacts {
        agent_type: Some("explorer"),
        request_class: Some("auxiliary"),
        ..facts("inference", None, None)
    };
    let without_agent = RequestFacts {
        user_agent: None,
        ..facts("inference", None, None)
    };

    for (subject, word) in [
        (Subject::AgentType, "explorer"),
        (Subject::RequestClass, "auxiliary"),
    ] {
        assert!(holds(fact(subject, Absent, None), &without));
        assert!(!holds(fact(subject, Absent, None), &with));
        assert!(holds(fact(subject, Equals, Some(word)), &with));
        assert!(!holds(fact(subject, Equals, Some("other")), &with));
        assert!(!holds(fact(subject, Equals, Some(word)), &without));
        assert!(holds(fact(subject, Matches, Some("^[a-z]+$")), &with));
        assert!(!holds(fact(subject, Matches, Some("^[a-z]+$")), &without));
    }

    let agent = "claude-cli/2.1.300 (external, cli)";
    assert!(holds(
        fact(Subject::UserAgent, Absent, None),
        &without_agent
    ));
    assert!(!holds(fact(Subject::UserAgent, Absent, None), &without));
    assert!(holds(
        fact(Subject::UserAgent, Equals, Some(agent)),
        &without
    ));
    assert!(!holds(
        fact(Subject::UserAgent, Equals, Some("claude-cli")),
        &without
    ));
    assert!(!holds(
        fact(Subject::UserAgent, Equals, Some(agent)),
        &without_agent
    ));
    assert!(holds(
        fact(Subject::UserAgent, Matches, Some("^claude-cli/")),
        &without
    ));
    assert!(!holds(
        fact(Subject::UserAgent, Matches, Some("^claude-cli/")),
        &without_agent
    ));

    assert!(holds(
        fact(Subject::Client, Equals, Some("claude-code/cli")),
        &without
    ));
    assert!(!holds(
        fact(Subject::Client, Equals, Some("other")),
        &without
    ));
    assert!(holds(
        fact(Subject::Client, Matches, Some("^claude-code/")),
        &without
    ));
    assert!(!holds(
        fact(Subject::Client, Matches, Some("^other")),
        &without
    ));

    assert!(holds(
        fact(Subject::Model, Equals, Some("claude-opus-5")),
        &without
    ));
    assert!(!holds(
        fact(Subject::Model, Equals, Some("Claude-Opus-5")),
        &without
    ));
    assert!(holds(fact(Subject::Model, Matches, Some("opus")), &without));
    assert!(!holds(
        fact(Subject::Model, Matches, Some("haiku")),
        &without
    ));

    let count_tokens = facts("count_tokens", None, None);
    assert!(holds(
        fact(Subject::Kind, Equals, Some("inference")),
        &without
    ));
    assert!(!holds(
        fact(Subject::Kind, Equals, Some("inference")),
        &count_tokens
    ));
    assert!(holds(
        fact(Subject::Kind, Equals, Some("count_tokens")),
        &count_tokens
    ));
    assert!(!holds(
        fact(Subject::Kind, Equals, Some("count_tokens")),
        &without
    ));

    let headers = Headers(vec![("x-test", "on")]);
    let carried = RequestFacts {
        headers: &headers,
        ..facts("inference", None, None)
    };
    let header = |operator, value: Option<&str>| {
        cond(Subject::Header, operator, value, Some("x-test"), None, None)
    };
    assert!(!holds(header(Absent, None), &carried));
    assert!(holds(header(Absent, None), &without));
    assert!(holds(header(Present, None), &carried));
    assert!(!holds(header(Present, None), &without));
    assert!(holds(header(Equals, Some("on")), &carried));
    assert!(!holds(header(Equals, Some("off")), &carried));
    assert!(!holds(header(Equals, Some("on")), &without));
    assert!(holds(header(Matches, Some("^o")), &carried));
    assert!(!holds(header(Matches, Some("^o")), &without));

    let fork = fork();
    let tree = facts("inference", None, Some(&fork));
    assert!(holds(body("missing", Absent, None), &tree));
    assert!(!holds(body("model", Absent, None), &tree));
    assert!(holds(body("model", Present, None), &tree));
    assert!(!holds(body("missing", Present, None), &tree));
    assert!(holds(body("model", Equals, Some("claude-opus-5")), &tree));
    assert!(!holds(
        body("model", Equals, Some("claude-haiku-4-5")),
        &tree
    ));
    assert!(holds(body("max_tokens", Gt, Some("100")), &tree));
    assert!(!holds(body("max_tokens", Gt, Some("1000")), &tree));
    assert!(holds(body("max_tokens", Lt, Some("1000")), &tree));
    assert!(!holds(body("max_tokens", Lt, Some("100")), &tree));
    assert!(holds(body("model", Matches, Some("^claude-opus")), &tree));
    assert!(!holds(body("model", Matches, Some("^claude-haiku")), &tree));
}

#[test]
fn a_star_index_fans_out_and_holds_when_any_element_holds() {
    let fork = fork();
    let tree = facts("inference", None, Some(&fork));
    assert!(holds(
        body("tools[*].name", Operator::Equals, Some("tool_seven")),
        &tree
    ));
    assert!(!holds(
        body("tools[*].name", Operator::Equals, Some("tool_eleven")),
        &tree
    ));
    assert!(holds(
        body("tools[*].missing", Operator::Absent, None),
        &tree
    ));
    assert!(!holds(body("tools[*].name", Operator::Absent, None), &tree));
    assert!(holds(
        body(
            "messages[*].content[*].type",
            Operator::Equals,
            Some("tool_use")
        ),
        &tree
    ));
    assert!(!holds(
        body(
            "messages[*].content[*].type",
            Operator::Equals,
            Some("image")
        ),
        &tree
    ));
}

#[test]
fn a_from_end_index_and_an_index_past_the_end() {
    let fork = fork();
    let tree = facts("inference", None, Some(&fork));
    assert!(holds(
        body("messages[-1].role", Operator::Equals, Some("system")),
        &tree
    ));
    assert!(holds(
        body("messages[-4].role", Operator::Equals, Some("user")),
        &tree
    ));
    assert!(holds(
        body("messages[0].role", Operator::Equals, Some("user")),
        &tree
    ));
    assert!(holds(
        body("messages[3].role", Operator::Equals, Some("system")),
        &tree
    ));
    assert!(holds(
        body("messages[9].role", Operator::Absent, None),
        &tree
    ));
    assert!(holds(
        body("messages[-9].role", Operator::Absent, None),
        &tree
    ));
    assert!(holds(body("messages[4]", Operator::Absent, None), &tree));
    assert!(holds(body("messages[-5]", Operator::Absent, None), &tree));
}

#[test]
fn a_member_segment_on_a_non_object_and_an_index_on_a_non_array_read_absent() {
    let fork = fork();
    let tree = facts("inference", None, Some(&fork));
    assert!(holds(body("max_tokens.x", Operator::Absent, None), &tree));
    assert!(!holds(body("max_tokens.x", Operator::Present, None), &tree));
    assert!(holds(body("model[0]", Operator::Absent, None), &tree));
    assert!(!holds(body("model[0]", Operator::Present, None), &tree));
    assert!(holds(body("system.text", Operator::Absent, None), &tree));
    assert!(holds(body("model[*]", Operator::Absent, None), &tree));
}

#[test]
fn equals_compares_each_scalar_kinds_json_text_and_never_an_object() {
    let tree = json!({
        "speed": "fast",
        "empty": "",
        "max_tokens": 512,
        "ratio": 0.5,
        "stream": true,
        "thinking": false,
        "temperature": null,
        "metadata": {"user_id": "u1"},
        "stop_sequences": ["end"]
    });
    let facts = facts("inference", None, Some(&tree));
    let equals = |path, value| holds(body(path, Operator::Equals, Some(value)), &facts);

    assert!(equals("speed", "fast"));
    assert!(!equals("speed", "\"fast\""));
    assert!(equals("empty", ""));
    assert!(!equals("speed", ""));
    assert!(equals("max_tokens", "512"));
    assert!(!equals("max_tokens", "512.0"));
    assert!(equals("ratio", "0.5"));
    assert!(equals("stream", "true"));
    assert!(!equals("stream", "1"));
    assert!(equals("thinking", "false"));
    assert!(equals("temperature", "null"));
    assert!(!equals("temperature", ""));
    assert!(holds(body("temperature", Operator::Present, None), &facts));
    assert!(!holds(body("temperature", Operator::Absent, None), &facts));

    assert!(!equals("metadata", r#"{"user_id":"u1"}"#));
    assert!(!equals("metadata", r#"{"user_id": "u1"}"#));
    assert!(!equals("metadata", ""));
    assert!(holds(body("metadata", Operator::Present, None), &facts));
    assert!(equals("metadata.user_id", "u1"));

    assert!(!equals("stop_sequences", r#"["end"]"#));
    assert!(!equals("stop_sequences", "end"));
    assert!(!equals("stop_sequences", ""));
}

#[test]
fn gt_and_lt_compare_numbers_and_fail_on_anything_else() {
    let fork = fork();
    let tree = facts("inference", None, Some(&fork));
    assert!(holds(body("max_tokens", Operator::Gt, Some("100")), &tree));
    assert!(holds(
        body("max_tokens", Operator::Gt, Some("511.5")),
        &tree
    ));
    assert!(!holds(body("max_tokens", Operator::Gt, Some("512")), &tree));
    assert!(!holds(body("max_tokens", Operator::Lt, Some("100")), &tree));
    assert!(!holds(body("max_tokens", Operator::Lt, Some("512")), &tree));
    assert!(holds(
        body("max_tokens", Operator::Lt, Some("512.5")),
        &tree
    ));
    assert!(!holds(body("model", Operator::Gt, Some("0")), &tree));
    assert!(!holds(body("model", Operator::Lt, Some("1e300")), &tree));
    assert!(!holds(body("stream", Operator::Gt, Some("0")), &tree));
    assert!(!holds(
        body("max_tokens", Operator::Matches, Some("512")),
        &tree
    ));
    assert!(!holds(
        body("max_tokens", Operator::Matches, Some("")),
        &tree
    ));
}

#[test]
fn a_header_name_is_lowercased_at_validation_and_read_through_the_lookup() {
    let written = cond(
        Subject::Header,
        Operator::Equals,
        Some("explorer"),
        Some("X-Claude-Code-Agent-Type"),
        None,
        None,
    );
    let Probe::Header(name, _) = written.check().expect("the condition checks") else {
        panic!("a header condition checks to a header probe");
    };
    assert_eq!(name, "x-claude-code-agent-type");

    let headers = Headers(vec![("x-claude-code-agent-type", "explorer")]);
    let facts = RequestFacts {
        headers: &headers,
        ..facts("inference", None, None)
    };
    assert!(holds(written, &facts));
    assert!(holds(
        cond(
            Subject::Header,
            Operator::Present,
            None,
            Some("X-CLAUDE-CODE-AGENT-TYPE"),
            None,
            None
        ),
        &facts
    ));
}

#[test]
fn a_body_that_did_not_parse_fails_body_and_message_conditions_and_holds_absent() {
    let facts = facts("inference", None, None);
    assert!(holds(body("tools", Operator::Absent, None), &facts));
    assert!(holds(body("tools[*].name", Operator::Absent, None), &facts));
    assert!(!holds(body("tools", Operator::Present, None), &facts));
    assert!(!holds(body("model", Operator::Equals, Some("")), &facts));
    assert!(!holds(body("max_tokens", Operator::Gt, Some("-1")), &facts));
    assert!(!holds(body("max_tokens", Operator::Lt, Some("1")), &facts));
    assert!(!holds(body("model", Operator::Matches, Some("")), &facts));
    assert!(!holds(message(Selector::Any, ""), &facts));
}

#[test]
fn the_cost_order_runs_the_cheap_subjects_first_and_names_the_failing_index_in_the_written_order() {
    let fork = fork();
    let written = [
        message(Selector::LastUser, "^Describe"),
        cond(
            Subject::Kind,
            Operator::Equals,
            Some("count_tokens"),
            None,
            None,
            None,
        ),
    ];
    assert_eq!(
        evaluate(
            &compiled(&written, GuardrailMode::Observe),
            &facts("inference", None, Some(&fork))
        ),
        Verdict::Unmatched { failed: 1 }
    );

    // Both conditions fail on this body, so the index the verdict names tells which one ran first.
    let empty = json!({});
    let probe = [
        message(Selector::Any, ""),
        cond(
            Subject::Kind,
            Operator::Equals,
            Some("count_tokens"),
            None,
            None,
            None,
        ),
    ];
    assert_eq!(
        evaluate(
            &compiled(&probe, GuardrailMode::Observe),
            &facts("inference", None, Some(&empty))
        ),
        Verdict::Unmatched { failed: 1 }
    );

    let cheap = [
        cond(
            Subject::Model,
            Operator::Equals,
            Some("claude-haiku-4-5"),
            None,
            None,
            None,
        ),
        body("missing", Operator::Present, None),
        cond(
            Subject::Kind,
            Operator::Equals,
            Some("count_tokens"),
            None,
            None,
            None,
        ),
    ];
    assert_eq!(
        evaluate(
            &compiled(&cheap, GuardrailMode::Observe),
            &facts("inference", None, Some(&empty))
        ),
        Verdict::Unmatched { failed: 2 }
    );
}

#[test]
fn an_unknown_header_is_set_aside_and_decides_only_when_every_other_condition_held() {
    let hints = HintLookup {
        request_class: Some("auxiliary"),
        ..HintLookup::default()
    };
    let fork = fork();
    let classifier = classifier();
    let with = |body| RequestFacts {
        headers: &hints,
        ..facts("inference", None, Some(body))
    };
    let other = cond(
        Subject::Header,
        Operator::Present,
        None,
        Some("x-other"),
        None,
        None,
    );
    let kind = |word| {
        cond(
            Subject::Kind,
            Operator::Equals,
            Some(word),
            None,
            None,
            None,
        )
    };
    let verdict = |conditions: &[Condition], body| {
        evaluate(&compiled(conditions, GuardrailMode::Observe), &with(body))
    };

    assert_eq!(
        verdict(&[other.clone(), kind("inference")], &fork),
        Verdict::Indeterminate { condition: 0 }
    );
    assert_eq!(
        verdict(&[other.clone(), kind("count_tokens")], &fork),
        Verdict::Unmatched { failed: 1 }
    );
    let tools = body("tools", Operator::Present, None);
    assert_eq!(
        verdict(&[other.clone(), tools.clone()], &fork),
        Verdict::Indeterminate { condition: 0 }
    );
    assert_eq!(
        verdict(&[other.clone(), tools], &classifier),
        Verdict::Unmatched { failed: 1 }
    );
    let kept = cond(
        Subject::Header,
        Operator::Equals,
        Some("auxiliary"),
        Some("x-claude-code-request-class"),
        None,
        None,
    );
    assert_eq!(
        verdict(std::slice::from_ref(&kept), &fork),
        Verdict::Matched
    );
    assert_eq!(
        verdict(&[kept, other], &fork),
        Verdict::Indeterminate { condition: 1 }
    );

    let names = [
        "x-claude-code-request-class",
        "x-claude-code-agent-type",
        "user-agent",
        "x-claude-code-compaction",
        "x-claude-code-context-compacted",
    ];
    let full = HintLookup {
        request_class: Some("auxiliary"),
        agent_type: Some("explorer"),
        user_agent: Some("claude-cli/2.1.300 (external, cli)"),
        compaction: Some("auto"),
        context_compacted: Some("true"),
    };
    let answers: Vec<Lookup<'_>> = names.iter().map(|name| full.get(name)).collect();
    assert_eq!(
        answers,
        [
            Lookup::Value("auxiliary"),
            Lookup::Value("explorer"),
            Lookup::Value("claude-cli/2.1.300 (external, cli)"),
            Lookup::Value("auto"),
            Lookup::Value("true"),
        ]
    );
    let empty = HintLookup::default();
    for name in names {
        assert_eq!(empty.get(name), Lookup::Absent, "{name}");
    }
    assert_eq!(full.get("x-other"), Lookup::Unknown);
    assert_eq!(empty.get("anthropic-beta"), Lookup::Unknown);
}

#[test]
fn evaluate_all_records_every_match_in_set_order_and_blocks_at_the_first_enforcing_one() {
    let fork = fork();
    let facts = facts("inference", Some("auxiliary"), Some(&fork));
    let guardrail = |id: &str, mode, word: &str| {
        let kind = cond(
            Subject::Kind,
            Operator::Equals,
            Some(word),
            None,
            None,
            None,
        );
        CompiledGuardrail::compile(id, id, mode, GuardrailAction::Block, &[kind])
            .expect("the guardrail compiles")
    };
    let set = [
        guardrail("unmatched", GuardrailMode::Enforce, "count_tokens"),
        guardrail("observed", GuardrailMode::Observe, "inference"),
        guardrail("enforced", GuardrailMode::Enforce, "inference"),
        guardrail("second", GuardrailMode::Enforce, "inference"),
    ];
    let evaluation = evaluate_all(&set, &facts);
    let entry = |id: &str, position, blocked| Match {
        id: id.to_string(),
        position,
        blocked,
    };
    assert_eq!(
        evaluation,
        Evaluation {
            matched: vec![
                entry("observed", 1, false),
                entry("enforced", 2, true),
                entry("second", 3, false)
            ],
            blocked: Some(1),
        }
    );
    assert_eq!(evaluation.matched[evaluation.blocked.unwrap()].position, 2);

    let observed_only = evaluate_all(set.iter().take(2), &facts);
    assert_eq!(observed_only.blocked, None);
    assert_eq!(observed_only.matched, vec![entry("observed", 1, false)]);
}

#[test]
fn texts_reads_a_string_content_as_one_block_and_an_array_by_its_text_blocks() {
    let string = json!({"role": "user", "content": "hello"});
    assert_eq!(texts(&string).collect::<Vec<_>>(), ["hello"]);
    let array = json!({"role": "user", "content": [
        {"type": "text", "text": "first"},
        {"type": "tool_result", "tool_use_id": "t", "content": "skipped"},
        {"type": "image", "source": {"type": "base64", "data": ""}},
        {"type": "text", "text": "second"}
    ]});
    assert_eq!(texts(&array).collect::<Vec<_>>(), ["first", "second"]);
    assert_eq!(texts(&json!({"role": "user"})).count(), 0);
    assert_eq!(texts(&json!({"role": "user", "content": 5})).count(), 0);
}

#[test]
fn parse_body_reads_a_strict_body_a_lone_surrogate_body_and_nothing_else() {
    assert_eq!(parse_body(br#"{"a":"b"}"#), Some(json!({"a": "b"})));
    assert_eq!(
        parse_body(b"{\"a\":\"\\ud83d\"}"),
        Some(json!({"a": "\u{FFFD}"}))
    );
    assert_eq!(parse_body(b"not json"), None);
}
