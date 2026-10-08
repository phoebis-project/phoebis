//! The conversation climb end to end: what the relay resolves, what lands in the affinity map, and what the pin
//! sees, for the shapes a conversation's boundaries take across turns, forks, and agents.
mod common;

use axum::body::Body;
use axum::http::Request;
use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule};
use http_body_util::BodyExt;
use phoebis_bus::{Bus, Delivery, Event, Fork};
use serde_json::{json, Value};
use tower::ServiceExt;

const MODEL: &str = "claude-sonnet-4-5";

fn body(messages: Vec<Value>) -> Value {
    json!({"model": MODEL, "messages": messages})
}

fn req(value: &Value) -> Request<Body> {
    use phoebis_profile::fixtures::{inference_head, FixtureKind};
    let bytes = serde_json::to_vec(value).unwrap();
    let mut request = request_from_fixture(inference_head(FixtureKind::ApiKey), &bytes, KEY);
    remove_header(&mut request, "x-claude-code-session-id");
    request
}

/// A 200 answer naming `id`, the given `request-id` header when one is asked for, and a cache write in its
/// usage unless `cache` says otherwise.
fn ok(id: &str, request_id: Option<&str>, cache: bool) -> Respond {
    let mut usage = json!({"input_tokens": 10, "output_tokens": 5});
    if cache {
        usage["cache_creation_input_tokens"] = json!(200);
    }
    let headers = request_id
        .map(|rid| vec![("request-id".to_string(), rid.to_string())])
        .unwrap_or_default();
    Respond::Json {
        status: 200,
        headers,
        body: json!({"id": id, "type": "message", "content": [], "usage": usage})
            .to_string()
            .into_bytes(),
    }
}

/// A 200 answer naming `id` whose usage reports its cache writes in the split shape alone, five minutes and one
/// hour as given.
fn ok_split(id: &str, five_minute: u64, one_hour: u64) -> Respond {
    let usage = json!({
        "input_tokens": 10,
        "output_tokens": 5,
        "cache_creation": {
            "ephemeral_5m_input_tokens": five_minute,
            "ephemeral_1h_input_tokens": one_hour,
        },
    });
    Respond::Json {
        status: 200,
        headers: vec![],
        body: json!({"id": id, "type": "message", "content": [], "usage": usage})
            .to_string()
            .into_bytes(),
    }
}

/// A generic 400 that names no injected field: it classifies as a straight commit, forwarded as it came.
fn a_400() -> Respond {
    Respond::Json {
        status: 400,
        headers: vec![],
        body: br#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#
            .to_vec(),
    }
}

/// One harness with its own bus subscriber, so a test reads the fork and the router's own state side by side
/// without repeating the wiring.
async fn setup(up: &FakeUpstream, accounts: &[TestAccount]) -> (Harness, phoebis_bus::Subscriber) {
    let bus = Bus::new();
    let sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(up, accounts, PoolSpec::default(), bus).await;
    (h, sub)
}

fn one() -> Vec<TestAccount> {
    vec![api_key("a", "sk-ant-api03-CLIMBONE", 0)]
}

fn two() -> Vec<TestAccount> {
    vec![
        api_key("a", "sk-ant-api03-CLIMBTWOA", 0),
        api_key("b", "sk-ant-api03-CLIMBTWOB", 1),
    ]
}

/// Drives the request to completion the way a real client would: dropping the response without reading its
/// body closes the channel the body-relay task forwards chunks through, which the task reads as the caller
/// having gone, aborting the read before the answer's `id` or usage ever arrives.
async fn send(h: &Harness, request: Request<Body>) {
    let resp = h.app.clone().oneshot(request).await.unwrap();
    let _ = resp.into_body().collect().await;
}

/// Forces a round trip through the router task's own command queue: since a report only reaches the affinity
/// map when the task drains it, and the relay's own lookup reads the map directly with no round trip of its
/// own, a later request's climb is only guaranteed to see an earlier one's report once something ordered on
/// the same queue (a pins read answers) has come back.
async fn settle(h: &Harness) {
    let _ = h.router.pins().await;
}

/// The fork facts of the request's own observed event, once its response has landed.
async fn identified(sub: &mut phoebis_bus::Subscriber) -> Fork {
    let events = drain_until_complete(sub).await;
    let Some(Event::RequestObserved(observed)) = events
        .iter()
        .find(|e| matches!(e, Event::RequestObserved(_)))
    else {
        panic!("{events:?}")
    };
    observed.fork.clone()
}

#[tokio::test]
async fn a_one_hour_static_tier_and_a_five_minute_tail_record_both_lifetimes() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: ok("msg_1", None, true),
    }])
    .await;
    let (h, mut sub) = setup(&up, &one()).await;

    let mut value = body(vec![user("hi"), assistant("hello")]);
    value["system"] = json!([
        {"type": "text", "text": "be helpful"},
        {"type": "text", "text": "a second block", "cache_control": {"type": "ephemeral", "ttl": "1h"}},
    ]);
    value["messages"][1]["content"] =
        json!([{"type": "text", "text": "hello", "cache_control": {"type": "ephemeral"}}]);
    let chains = phoebis_relay::Chains::read(&value).unwrap();
    let static_hash = chains.static_hash().unwrap().to_string();
    let tail_hash = chains.tail_hash().unwrap().to_string();

    send(&h, req(&value)).await;
    identified(&mut sub).await;
    settle(&h).await;
    {
        let map = h.router.affinity();
        let guard = map.read().unwrap();
        assert_eq!(
            guard.entry(MODEL, &static_hash).unwrap().ttl_ms,
            3_600_000,
            "the last system block's hour marker is the static tier's"
        );
        assert_eq!(
            guard.entry(MODEL, &tail_hash).unwrap().ttl_ms,
            300_000,
            "the last message's marker with no ttl member is the five-minute default"
        );
    }
    up.shutdown().await;
}

#[tokio::test]
async fn a_one_hour_marker_answered_with_a_five_minute_write_records_five_minutes() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: ok_split("msg_1", 200, 0),
    }])
    .await;
    let (h, mut sub) = setup(&up, &one()).await;
    let mut value = body(vec![user("hi"), assistant("hello")]);
    value["messages"][1]["content"] = json!([{"type": "text", "text": "hello", "cache_control": {"type": "ephemeral", "ttl": "1h"}}]);
    let tail_hash = phoebis_relay::Chains::read(&value)
        .unwrap()
        .tail_hash()
        .unwrap()
        .to_string();
    send(&h, req(&value)).await;
    identified(&mut sub).await;
    settle(&h).await;
    let map = h.router.affinity();
    assert_eq!(
        map.read().unwrap().entry(MODEL, &tail_hash).unwrap().ttl_ms,
        300_000,
        "the upstream wrote at five minutes, whatever the marker asked"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_mixed_marker_request_with_equal_deepest_ttls_keeps_its_markers_lifetimes() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: ok_split("msg_1", 0, 200),
    }])
    .await;
    let (h, mut sub) = setup(&up, &one()).await;
    let mut value = body(vec![user("hi"), assistant("hello")]);
    value["tools"] = json!([{
        "name": "t",
        "input_schema": {"type": "object"},
        "cache_control": {"type": "ephemeral", "ttl": "1h"},
    }]);
    value["system"] =
        json!([{"type": "text", "text": "be helpful", "cache_control": {"type": "ephemeral"}}]);
    value["messages"][1]["content"] =
        json!([{"type": "text", "text": "hello", "cache_control": {"type": "ephemeral"}}]);
    let tail_hash = phoebis_relay::Chains::read(&value)
        .unwrap()
        .tail_hash()
        .unwrap()
        .to_string();
    send(&h, req(&value)).await;
    identified(&mut sub).await;
    settle(&h).await;
    let map = h.router.affinity();
    assert_eq!(
        map.read().unwrap().entry(MODEL, &tail_hash).unwrap().ttl_ms,
        300_000,
        "a request marked at both TTLs keeps each tier's marker"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_marker_on_the_tail_alone_records_the_static_boundaries_as_links_without_warmth() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: ok("msg_1", None, true),
    }])
    .await;
    let (h, mut sub) = setup(&up, &one()).await;

    let mut value = body(vec![user("a body with a marker on its own tail alone")]);
    value["messages"][0]["content"] = json!([{
        "type": "text",
        "text": "a body with a marker on its own tail alone",
        "cache_control": {"type": "ephemeral"},
    }]);
    let chains = phoebis_relay::Chains::read(&value).unwrap();
    let static_hash = chains.static_hash().unwrap().to_string();
    let tail_hash = chains.tail_hash().unwrap().to_string();

    send(&h, req(&value)).await;
    identified(&mut sub).await;
    settle(&h).await;
    {
        let map = h.router.affinity();
        let guard = map.read().unwrap();
        let now = h.clock.load(std::sync::atomic::Ordering::SeqCst);
        let static_entry = guard
            .entry(MODEL, &static_hash)
            .expect("the links are always recorded, even without a marker of their own");
        assert_eq!(
            static_entry.ttl_ms, 0,
            "no marker on the static tier confirms no warmth"
        );
        assert!(
            !static_entry.live(now),
            "an unconfirmed link is never live, so the static tier is never placed by"
        );
        let message_entry = guard
            .entry(MODEL, &tail_hash)
            .expect("the tail's own marker lands on the message tier");
        assert_eq!(
            message_entry.ttl_ms, 300_000,
            "the tail's marker with no ttl member is the five-minute default"
        );
    }
    up.shutdown().await;
}

#[tokio::test]
async fn a_completed_response_lands_its_boundaries_in_the_affinity_map() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: ok("msg_1", Some("req_up_0001"), true),
    }])
    .await;
    let (h, mut sub) = setup(&up, &one()).await;

    let mut value = body(vec![user("first"), assistant("second")]);
    value["system"] =
        json!([{"type": "text", "text": "be helpful", "cache_control": {"type": "ephemeral"}}]);
    value["messages"][1]["content"] =
        json!([{"type": "text", "text": "second", "cache_control": {"type": "ephemeral"}}]);
    let chains = phoebis_relay::Chains::read(&value).unwrap();
    let static_hash = chains.static_hash().unwrap().to_string();
    let message_boundaries = chains.message_boundaries.clone();

    send(&h, req(&value)).await;
    identified(&mut sub).await;
    settle(&h).await;

    let account_id = "a";
    let key = h.store.api_key("desktop").unwrap().unwrap();
    let now = h.clock.load(std::sync::atomic::Ordering::SeqCst);
    {
        let map = h.router.affinity();
        let guard = map.read().unwrap();
        for boundary in message_boundaries.iter() {
            let entry = guard
                .entry(MODEL, boundary)
                .expect("every message boundary lands in the map");
            assert_eq!(entry.chain, phoebis_routing::AffinityChain::Message);
            assert!(entry.live(now));
            assert_eq!(entry.account_id, account_id);
            assert_eq!(entry.key_id, key.id);
            assert_eq!(entry.key_generation, key.created_at);
        }
        let static_entry = guard
            .entry(MODEL, &static_hash)
            .expect("the static boundary lands too, under the same cache evidence");
        assert_eq!(static_entry.chain, phoebis_routing::AffinityChain::Static);
    }
    up.shutdown().await;
}

#[tokio::test]
async fn a_rejected_answer_and_a_token_count_record_no_affinity_entry() {
    let up = FakeUpstream::start(vec![
        Rule {
            path_prefix: "/v1/messages/count_tokens".into(),
            respond: Respond::Json {
                status: 200,
                headers: vec![],
                body: br#"{"input_tokens":7}"#.to_vec(),
            },
        },
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: a_400(),
        },
    ])
    .await;
    let (h, mut sub) = setup(&up, &one()).await;

    let rejected = body(vec![user("a request the account rejects")]);
    let chains = phoebis_relay::Chains::read(&rejected).unwrap();
    let rejected_boundary = chains.message_boundaries[0].clone();
    send(&h, req(&rejected)).await;
    identified(&mut sub).await;
    settle(&h).await;
    {
        let map = h.router.affinity();
        let guard = map.read().unwrap();
        assert!(
            guard.entry(MODEL, &rejected_boundary).is_none(),
            "a rejected answer continues no conversation"
        );
    }

    use phoebis_profile::fixtures::{inference_head, FixtureKind};
    let counted = body(vec![user("a token count of its own content")]);
    let count_chains = phoebis_relay::Chains::read(&counted).unwrap();
    let counted_boundary = count_chains.message_boundaries[0].clone();
    let count_bytes = serde_json::to_vec(&counted).unwrap();
    let count_req =
        request_from_fixture(inference_head(FixtureKind::CountApiKey), &count_bytes, KEY);
    send(&h, count_req).await;
    settle(&h).await;
    {
        let map = h.router.affinity();
        let guard = map.read().unwrap();
        assert!(
            guard.entry(MODEL, &counted_boundary).is_none(),
            "a token count writes no cache and records nothing"
        );
    }
    up.shutdown().await;
}

// -- The fork detection cases --

#[tokio::test]
async fn a_subagents_first_request_identifies_by_its_session_under_its_own_agent() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: ok("msg_1", None, true),
    }])
    .await;
    let (h, mut sub) = setup(&up, &one()).await;

    let mut value = body(vec![user("a subagent's own opening turn")]);
    value["system"] = json!([{
        "type": "text",
        "text": "the subagent's own static prefix",
        "cache_control": {"type": "ephemeral"},
    }]);
    let static_hash = phoebis_relay::Chains::read(&value)
        .unwrap()
        .static_hash()
        .unwrap()
        .to_string();
    let mut request = req(&value);
    set_header(&mut request, "x-claude-code-session-id", "s1");
    set_header(&mut request, "x-claude-code-agent-id", "sub-1");
    set_header(&mut request, "x-claude-code-parent-agent-id", "main");
    send(&h, request).await;
    assert_eq!(identified(&mut sub).await, Fork::default());
    settle(&h).await;
    let pins = h.router.pins().await.unwrap();
    assert_eq!(pins.len(), 1);
    assert_eq!(pins[0].agent_id, "sub-1", "a pin under its own agent");
    {
        let map = h.router.affinity();
        let guard = map.read().unwrap();
        assert!(
            guard.entry(MODEL, &static_hash).is_some(),
            "the map holds the subagent's own static boundaries"
        );
    }
    up.shutdown().await;
}

#[tokio::test]
async fn a_second_session_id_sharing_the_firsts_prefix_and_diverging_is_a_fork_of_it() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: ok("msg_1", None, true),
    }])
    .await;
    let (h, mut sub) = setup(&up, &two()).await;

    // A marker on s1's own last turn, so its answer's cache write confirms a live entry at the shared prefix
    // for the fork's placement to find; it sits past the three shared messages, so the shared boundaries
    // (and the fork's own identity) hash exactly as they would without it.
    let four = vec![
        user("one"),
        assistant("two"),
        user("three"),
        json!({"role": "assistant", "content": [
            {"type": "text", "text": "four", "cache_control": {"type": "ephemeral"}},
        ]}),
    ];
    // s1's own conversation is served by account "b" alone, so the fork's own pin below (on "a" once it joins
    // the pool) would show up plainly if placement ever ignored the shared prefix: a fork lands with its base.
    h.store
        .set_pool_members("main", &[member("b", 1)], "test")
        .unwrap();
    h.router.reload().await.unwrap();
    let mut s1_req = req(&body(four.clone()));
    set_header(&mut s1_req, "x-claude-code-session-id", "s1");
    send(&h, s1_req).await;
    assert_eq!(identified(&mut sub).await, Fork::default());
    settle(&h).await;

    h.store
        .set_pool_members("main", &[member("a", 0), member("b", 1)], "test")
        .unwrap();
    h.router.reload().await.unwrap();
    let mut s2_messages = four[..3].to_vec();
    s2_messages.push(assistant("a different fourth"));
    let mut s2_req = req(&body(s2_messages.clone()));
    set_header(&mut s2_req, "x-claude-code-session-id", "s2");
    send(&h, s2_req).await;
    assert_eq!(
        identified(&mut sub).await,
        Fork {
            forked_from: Some("s1".into()),
            fork_depth: Some(3),
        }
    );
    settle(&h).await;
    let pins = h.router.pins().await.unwrap();
    let fork_pin = pins
        .iter()
        .find(|p| p.session_id == "s2")
        .expect("s2 pins under its own id");
    assert_eq!(
        fork_pin.account_id, "b",
        "a fork lands with its base: rung 3 places it on the account serving s1's shared prefix"
    );

    let mut s2_extended = s2_messages;
    s2_extended.push(user("continuing s2's own branch"));
    let mut s2_req2 = req(&body(s2_extended));
    set_header(&mut s2_req2, "x-claude-code-session-id", "s2");
    send(&h, s2_req2).await;
    assert_eq!(
        identified(&mut sub).await,
        Fork::default(),
        "s2's own continuation carries no fork"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_quota_probe_names_no_fork_and_leaves_no_content_entry_and_a_fork_needs_depth_2() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: ok("msg_1", None, true),
    }])
    .await;
    let (h, mut sub) = setup(&up, &one()).await;

    // The client's quota probe: one message every probe of the key shares, sent under the session id at a
    // session's start, so the second session's probe matches the first's at depth 1.
    let probe = json!({"model": MODEL, "max_tokens": 1, "messages": [user("quota")]});
    let probe_chains = phoebis_relay::Chains::read(&probe).unwrap();
    let classed = |value: &Value, session: &str, class: &str| {
        let mut request = req(value);
        set_header(&mut request, "x-claude-code-session-id", session);
        set_header(&mut request, "x-claude-code-request-class", class);
        request
    };

    send(&h, classed(&probe, "s1", "auxiliary")).await;
    assert_eq!(identified(&mut sub).await, Fork::default());
    settle(&h).await;
    send(&h, classed(&probe, "s2", "auxiliary")).await;
    assert_eq!(
        identified(&mut sub).await,
        Fork::default(),
        "an auxiliary request names no fork, whatever its one message matches"
    );
    settle(&h).await;
    {
        let map = h.router.affinity();
        let guard = map.read().unwrap();
        assert_eq!(
            probe_chains.message_boundaries.len(),
            1,
            "the probe's one message is one boundary, so the loop below checks something"
        );
        for boundary in probe_chains.message_boundaries.iter() {
            assert!(
                guard.entry(MODEL, boundary).is_none(),
                "an auxiliary request's report lands no message boundary"
            );
        }
        let static_entry = guard
            .entry(MODEL, &probe_chains.static_boundaries[0])
            .expect("the probe's static boundary lands, a cache of its own");
        assert_eq!(
            static_entry.root, "s1",
            "rooted at the first probe's session, as any static entry of the key is"
        );
    }

    let three = vec![user("one"), assistant("two"), user("three")];
    send(&h, classed(&body(three.clone()), "s1", "main")).await;
    assert_eq!(identified(&mut sub).await, Fork::default());
    settle(&h).await;

    send(&h, classed(&body(vec![user("one")]), "s2", "main")).await;
    assert_eq!(
        identified(&mut sub).await,
        Fork::default(),
        "a shared first message is depth 1, a template rather than a conversation"
    );
    settle(&h).await;

    let mut four = three;
    four.push(assistant("a different fourth"));
    send(&h, classed(&body(four), "s4", "main")).await;
    assert_eq!(
        identified(&mut sub).await,
        Fork {
            forked_from: Some("s1".into()),
            fork_depth: Some(3),
        },
        "a conversation that parts after the first answer is a fork at the depth where it parts"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn the_base_sessions_own_retry_after_a_fork_reads_no_fork_and_its_own_root() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: ok("msg_1", None, true),
    }])
    .await;
    let (h, mut sub) = setup(&up, &one()).await;

    let four = vec![
        user("one"),
        assistant("two"),
        user("three"),
        assistant("four"),
    ];
    let mut s1_req = req(&body(four.clone()));
    set_header(&mut s1_req, "x-claude-code-session-id", "s1");
    send(&h, s1_req).await;
    identified(&mut sub).await;
    settle(&h).await;

    let mut s2_messages = four[..3].to_vec();
    s2_messages.push(assistant("a different fourth"));
    let mut s2_req = req(&body(s2_messages));
    set_header(&mut s2_req, "x-claude-code-session-id", "s2");
    send(&h, s2_req).await;
    identified(&mut sub).await;
    settle(&h).await;

    // s1's own retry of its fourth message shares a boundary s2's fork also wrote (both on the one account this
    // harness has): the shared boundary must keep s1's own root, not the latest writer's.
    let mut s1_retry = four[..3].to_vec();
    s1_retry.push(assistant("a retried fourth"));
    let mut s1_retry_req = req(&body(s1_retry));
    set_header(&mut s1_retry_req, "x-claude-code-session-id", "s1");
    send(&h, s1_retry_req).await;
    assert_eq!(
        identified(&mut sub).await,
        Fork::default(),
        "s1's own retry after s2's fork reads no false fork"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_fork_that_lands_on_another_account_still_leaves_the_base_its_own_root() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: ok("msg_1", None, true),
    }])
    .await;
    let (h, mut sub) = setup(&up, &two()).await;

    let four = vec![
        user("one"),
        assistant("two"),
        user("three"),
        assistant("four"),
    ];
    h.store
        .set_pool_members("main", &[member("a", 0)], "test")
        .unwrap();
    h.router.reload().await.unwrap();
    let mut s1_req = req(&body(four.clone()));
    set_header(&mut s1_req, "x-claude-code-session-id", "s1");
    send(&h, s1_req).await;
    identified(&mut sub).await;
    settle(&h).await;

    // s2's fork writes the shared boundaries from the other account: the root belongs to s1's conversation
    // regardless, since the writer is still the same key and generation, only a different account.
    h.store
        .set_pool_members("main", &[member("b", 1)], "test")
        .unwrap();
    h.router.reload().await.unwrap();
    let mut s2_messages = four[..3].to_vec();
    s2_messages.push(assistant("a different fourth"));
    let mut s2_req = req(&body(s2_messages));
    set_header(&mut s2_req, "x-claude-code-session-id", "s2");
    send(&h, s2_req).await;
    identified(&mut sub).await;
    settle(&h).await;

    h.store
        .set_pool_members("main", &[member("a", 0), member("b", 1)], "test")
        .unwrap();
    h.router.reload().await.unwrap();
    let mut s1_retry = four[..3].to_vec();
    s1_retry.push(assistant("a retried fourth"));
    let mut s1_retry_req = req(&body(s1_retry));
    set_header(&mut s1_retry_req, "x-claude-code-session-id", "s1");
    send(&h, s1_retry_req).await;
    assert_eq!(
        identified(&mut sub).await,
        Fork::default(),
        "s1's own retry after a fork on another account reads no false fork"
    );
    up.shutdown().await;
}
