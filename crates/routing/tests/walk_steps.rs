//! Writes the walk's step ids and verdict words to the console's fixture under `PHOEBIS_WRITE_FIXTURES`, and without
//! the variable fails when the committed file differs from the engine's table. The admin fixtures test is the
//! precedent for the variable and the path.
use std::path::PathBuf;

use phoebis_routing::{
    ChainFacts, ClientClass, DecisionRecord, LaneFacts, RefusalFacts, RefusedRequest,
    RegistrySnapshot, RequestFlags, PARTITION_VALUES, WALK_STEPS,
};
use phoebis_store::RegistryLoad;
use serde_json::{json, Value};

fn path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../web/test/fixtures/routing/walk_steps.json")
}

fn fixture() -> Value {
    json!({
        "source": "phoebis-routing",
        "steps": WALK_STEPS
            .iter()
            .map(|(step, verdicts)| json!({ "step": step, "verdicts": verdicts }))
            .collect::<Vec<_>>(),
        "partition_values": PARTITION_VALUES,
    })
}

fn listed(step: &str, verdict: &str) -> bool {
    WALK_STEPS
        .iter()
        .any(|(id, verdicts)| *id == step && verdicts.contains(&verdict))
}

#[test]
fn the_fixture_lists_every_step_the_router_records_with_its_verdicts() {
    let captured = fixture();
    if std::env::var_os("PHOEBIS_WRITE_FIXTURES").is_some() {
        std::fs::create_dir_all(path().parent().unwrap()).unwrap();
        std::fs::write(
            path(),
            serde_json::to_string_pretty(&captured).unwrap() + "\n",
        )
        .unwrap();
        return;
    }
    let committed: Value =
        serde_json::from_str(&std::fs::read_to_string(path()).unwrap_or_else(|_| {
            panic!("walk_steps.json is missing. Run with PHOEBIS_WRITE_FIXTURES=1.")
        }))
        .unwrap();
    assert_eq!(
        committed, captured,
        "the committed fixture differs from WALK_STEPS. Re-capture it and update the console's station table"
    );
}

#[test]
fn the_steps_written_without_the_recorder_are_in_the_table() {
    // The cool reading is appended as a whole step after the fired rule, so no recorder call asserts it.
    assert!(listed("cool", "fell_through") && listed("cool", "preferred"));
    assert!(PARTITION_VALUES.contains(&"held") && PARTITION_VALUES.contains(&"fell_through"));
    // The refusal record writes its two steps by hand, before the engine runs.
    let snapshot = RegistrySnapshot::from_load(RegistryLoad::default(), [0; 32], 0, 1);
    let request = RefusedRequest {
        request_id: "r-1".into(),
        attempt: 1,
        seq: 1,
        at: 1_000,
        key_id: "k".into(),
        key_generation: 0,
        agent_id: "main".into(),
        parent_agent_id: None,
        session_id: None,
        request_class: None,
        canonical_model: "claude-sonnet-5".into(),
        lane: LaneFacts {
            speed: "standard".into(),
            thinking: "none".into(),
            effort: "-".into(),
        },
        flags: RequestFlags {
            fast: false,
            count: false,
            probe: false,
            portable: true,
            marker: false,
            context_compacted: false,
        },
        chain: ChainFacts {
            static_boundaries: 0,
            boundaries: 0,
            static_hash: false,
        },
        attempted: Vec::new(),
        holds: Vec::new(),
        client: ClientClass::ClaudeCode,
        client_version: None,
    };
    let refusal = RefusalFacts {
        kind: "key",
        code: "revoked",
        reason: "the key was revoked".into(),
    };
    let (record, _) = DecisionRecord::refused(&request, &snapshot, &refusal);
    assert_eq!(record.walk.len(), 2);
    for step in &record.walk {
        assert!(
            listed(step.step, step.verdict),
            "{}/{} is not in WALK_STEPS",
            step.step,
            step.verdict
        );
    }
}
