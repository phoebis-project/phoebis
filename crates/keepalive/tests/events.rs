//! The Go events and lifecycle suites over the reporting protocol: registration, sequences, generations and leases.
mod common;
use common::*;
use phoebis_keepalive::{
    decode_json_event, AgentState, CompanionState, Event, EventError, KeepaliveLimits, Park,
    ParkKind, Reason, RecipeState, Reported, SessionFacts,
};

/// A real request on main of `session` under `key_id`, the first of which gives the session its slot.
fn first_request(r: &Rig, key_id: &str, session: &str) {
    let mut observed = r.observed(&format!("{key_id}/{session}"), "main", BODY);
    if let phoebis_bus::Event::RequestObserved(request) = &mut observed {
        request.key_id = Some(key_id.into());
        request.key_generation = r.generation(key_id);
        request.session_id = Some(session.into());
    }
    r.engine.observe(observed);
}

#[tokio::test]
async fn the_session_bound_counts_live_sessions_only() {
    let mut rig = rig(RigSpec {
        limits: KeepaliveLimits {
            max_sessions: 128,
            ..RigSpec::default().limits
        },
        ..RigSpec::default()
    })
    .await;
    // Each sends a request before its end, so the end alone is what leaves it no slot.
    for i in 0..128 {
        rig.event(session_start(&format!("s-{i}"))).unwrap();
        first_request(&rig, "desktop", &format!("s-{i}"));
        rig.event(session_end(&format!("s-{i}"))).unwrap();
    }
    let ack = rig.event(session_start("s-128"));
    assert!(ack.is_ok(), "128 ended sessions hold no slot: {ack:?}");
    // The rig's own bootstrap session and "s-128" hold two of the 128 slots once each sends a request, so the rest
    // of the budget is 126 before the next live-bringing start is refused.
    first_request(&rig, "desktop", "session");
    first_request(&rig, "desktop", "s-128");
    for i in 0..126 {
        rig.event(session_start(&format!("t-{i}"))).unwrap();
        first_request(&rig, "desktop", &format!("t-{i}"));
    }
    let refused = rig.event(session_start("t-126")).unwrap_err();
    assert_eq!(refused.code, "session_limit");
    assert!(
        rig.engine
            .sessions()
            .iter()
            .any(|s| s.session_id == "s-0" && s.companion == CompanionState::None),
        "the tombstone is still read"
    );
}

#[tokio::test]
async fn a_restart_of_an_ended_session_under_a_new_instance_takes_a_slot() {
    // The rig's own bootstrap session holds one slot once it sends a request, so the cap here is two: one for
    // it, one for "a" or "b".
    let mut rig = rig(RigSpec {
        limits: KeepaliveLimits {
            max_sessions: 2,
            ..RigSpec::default().limits
        },
        ..RigSpec::default()
    })
    .await;
    first_request(&rig, "desktop", "session");
    rig.event(session_start_as("a", "i1", None)).unwrap();
    rig.event(session_end("a")).unwrap();
    rig.event(session_start_as("b", "i-b", None)).unwrap();
    first_request(&rig, "desktop", "b");
    // The tombstone's entry exists, so a bound over "adds an entry" would let this through.
    let refused = rig
        .event(session_start_as("a", "i2", Some("i1")))
        .unwrap_err();
    assert_eq!(refused.code, "session_limit");
}

#[tokio::test]
async fn a_same_instance_recovery_of_a_live_session_takes_no_slot() {
    let mut rig = rig(RigSpec {
        limits: KeepaliveLimits {
            max_sessions: 2,
            ..RigSpec::default().limits
        },
        ..RigSpec::default()
    })
    .await;
    first_request(&rig, "desktop", "session");
    rig.event(session_start_as("a", "i1", None)).unwrap();
    first_request(&rig, "desktop", "a");
    rig.advance(rig.lease_ms() + 1_000); // the lease lapsed; the session is live and unattended
    let ack = rig.event(session_start_as("a", "i1", None));
    assert!(
        ack.is_ok(),
        "a live session's own instance re-adopts it: {ack:?}"
    );
}

fn refusal(code: &'static str, status: u16) -> EventError {
    EventError {
        code,
        instance_id: None,
        request_generation: None,
        status,
    }
}

#[tokio::test]
async fn registration_sequences_and_generations_follow_the_protocol() {
    let mut r = rig(RigSpec::default()).await;
    // A same-instance re-registration with a higher sequence renews the lease and keeps agent state.
    r.event(Event {
        event: "agent_start".into(),
        agent_id: Some("worker".into()),
        ..Default::default()
    })
    .unwrap();
    r.state("worker", "parked");
    r.advance(1_000);
    let renewed = r
        .event(Event {
            event: "session_start".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(renewed.lease_expires_ms, r.now() + r.lease_ms());
    assert_eq!(
        (r.agent("worker").state, r.agent("worker").park),
        (
            AgentState::Parked,
            Some(Park::new(ParkKind::Background, None, None))
        )
    );
    // The lease renews on a heartbeat or a same-instance session_start only; an event about an agent
    // leaves the companion's own lease alone.
    r.advance(1_000);
    let untouched = r
        .event(Event {
            event: "activity".into(),
            agent_id: Some("worker".into()),
            kind: Some("prompt".into()),
            request_generation: Some(r.agent("worker").request_generation),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(untouched.lease_expires_ms, renewed.lease_expires_ms);
    let reheartbeat = r
        .event(Event {
            event: "heartbeat".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(reheartbeat.lease_expires_ms, r.now() + r.lease_ms());
    // Agents: a restart of a released agent needs the generation its release named.
    r.state("worker", "released");
    let generation = r.agent("worker").request_generation;
    assert_eq!(
        r.event(Event {
            event: "agent_start".into(),
            agent_id: Some("worker".into()),
            ..Default::default()
        })
        .unwrap_err(),
        refusal("agent_exists", 409),
        "a restart without the generation"
    );
    let restarted = r
        .event(Event {
            event: "agent_start".into(),
            agent_id: Some("worker".into()),
            request_generation: Some(generation),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        (restarted.agent_id.as_deref(), r.agent("worker").state),
        (Some("worker"), AgentState::Busy)
    );
    // The failed start did not advance the sequence: the next accepted one is one past the last accepted.
    assert_eq!(restarted.sequence, r.sequence);
    // session_end releases every agent and ends the instance; anything after it is refused.
    r.event(Event {
        event: "session_end".into(),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        (
            r.engine.sessions()[0].companion,
            r.engine.sessions()[0].agents
        ),
        (CompanionState::None, 2)
    );
    assert!(["main", "worker"]
        .iter()
        .all(|id| r.agent(id).state == AgentState::Released));
    assert_eq!(
        r.event(Event {
            event: "heartbeat".into(),
            ..Default::default()
        })
        .unwrap_err(),
        refusal("instance_ended", 409)
    );
    // A replacement that names the ended instance takes over with a fresh main agent.
    let replaced = r
        .engine
        .apply_event(
            "desktop",
            r.generation("desktop"),
            Event {
                event: "session_start".into(),
                session_id: "session".into(),
                instance_id: "next".into(),
                sequence: 1,
                previous_instance_id: Some("instance".into()),
                ..Default::default()
            }
            .try_into()
            .unwrap(),
        )
        .unwrap();
    assert_eq!(
        (
            replaced.instance_id.as_str(),
            replaced.request_generation,
            r.engine.sessions()[0].agents
        ),
        ("next", 0, 1)
    );
}

#[tokio::test]
async fn sessions_are_scoped_to_the_key_and_bounded_process_wide() {
    let mut r = rig(RigSpec {
        limits: KeepaliveLimits {
            max_sessions: 2,
            max_agents: 3,
            ..KeepaliveLimits::default()
        },
        ..RigSpec::default()
    })
    .await;
    // The same session id under another key is another session: neither key sees the other's.
    let laptop = r
        .engine
        .apply_event(
            "laptop",
            r.generation("laptop"),
            Event {
                event: "session_start".into(),
                session_id: "session".into(),
                instance_id: "elsewhere".into(),
                sequence: 1,
                ..Default::default()
            }
            .try_into()
            .unwrap(),
        )
        .unwrap();
    assert_eq!(laptop.instance_id, "elsewhere");
    first_request(&r, "desktop", "session");
    first_request(&r, "laptop", "session");
    assert_eq!(
        r.engine
            .sessions()
            .iter()
            .map(|s| (s.key_id.as_str(), s.session_id.as_str()))
            .collect::<Vec<_>>(),
        [("desktop", "session"), ("laptop", "session")]
    );
    assert_eq!(
        r.engine
            .apply_event(
                "desktop",
                r.generation("desktop"),
                Event {
                    event: "session_start".into(),
                    session_id: "third".into(),
                    instance_id: "i3".into(),
                    sequence: 1,
                    ..Default::default()
                }
                .try_into()
                .unwrap()
            )
            .unwrap_err(),
        refusal("session_limit", 429)
    );
    r.event(Event {
        event: "agent_start".into(),
        agent_id: Some("w1".into()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        r.event(Event {
            event: "agent_start".into(),
            agent_id: Some("w2".into()),
            ..Default::default()
        })
        .unwrap_err(),
        refusal("agent_limit", 429),
        "three agents across two sessions"
    );
    assert_eq!(r.engine.stats().sessions, 2);
    assert_eq!(r.engine.stats().agents, 3);
}

#[tokio::test]
async fn a_request_of_a_later_generation_arms_nothing_on_the_previous_holders_session() {
    let r = rig(RigSpec::default()).await;
    let before = r.status("main").reason;
    let mut observed = r.observed("r1", "main", BODY);
    if let phoebis_bus::Event::RequestObserved(request) = &mut observed {
        request.key_generation += 1;
    }
    r.engine.observe(observed);
    assert!(
        r.engine.recipes_for("desktop", "session").is_empty(),
        "a key id created again finds the previous holder's session, and arms nothing on it"
    );
    assert_eq!(
        (
            r.engine.stats().retained_bytes,
            r.agent("main").state,
            r.status("main").reason
        ),
        (0, AgentState::Busy, before),
        "and leaves the agent it names exactly as it was"
    );
}

#[tokio::test]
async fn a_lapsed_lease_marks_the_session_unattended_and_a_same_instance_start_re_adopts_it() {
    let mut r = rig(RigSpec::default()).await;
    r.observe("main").await;
    let armed = r.engine.recipes();
    let reason = r.status("main").reason;
    assert_eq!(
        (armed.len(), armed[0].state, armed[0].cause),
        (1, RecipeState::Retained, Reason::Scheduled)
    );
    r.advance(90_000);
    r.engine.expire();
    assert_eq!(
        r.engine.sessions()[0].companion,
        CompanionState::Silent,
        "the lapse leaves the session unattended, and the mod re-registers"
    );
    // A silent companion says nothing about the recipe: the lapse is counted, and the views keep reading the
    // reason the recipe's own last probe left.
    assert_eq!(r.status("main").reason, reason);
    assert_eq!(r.engine.recipes(), armed);
    assert_eq!(
        r.event(Event {
            event: "heartbeat".into(),
            ..Default::default()
        })
        .unwrap_err(),
        refusal("instance_ended", 409)
    );
    let again = r
        .event(Event {
            event: "session_start".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(again.instance_id, "instance");
    assert_eq!(
        r.engine.sessions()[0].companion,
        CompanionState::Reporting,
        "the same instance re-adopted the session"
    );
    let counts = r.engine.reasons();
    assert_eq!(
        counts
            .get(&phoebis_keepalive::Reason::LeaseExpired)
            .copied(),
        Some(1)
    );
}

#[tokio::test]
async fn a_released_subagent_reaches_the_router_so_its_pin_leaves_the_next_round() {
    use phoebis_routing::{
        burn_average_ms, Report, RouterEvent, UtilizationSample, WindowReading, WindowSource,
        FIVE_HOUR,
    };
    let mut r = rig(RigSpec {
        accounts: vec![oauth("a", "sk-ant-oat01-AAAA", 0)],
        tier: Some("default_claude_max_20x"),
        ..RigSpec::default()
    })
    .await;
    let pool = r.h.store.pool("main").unwrap().expect("the rig's pool");
    // Two burn averages on, so the window's stamp below stands past the trigger's wait at the round.
    let since = r.now();
    r.advance_attended(2 * burn_average_ms(&pool));
    r.event(Event {
        event: "agent_start".into(),
        agent_id: Some("sub".into()),
        ..Default::default()
    })
    .unwrap();
    r.observe("main").await;
    r.observe("sub").await;
    r.state("sub", "released");
    r.h.router
        .report(Report::Sample {
            account_id: "a".into(),
            sample: UtilizationSample::default().with(
                FIVE_HOUR,
                WindowReading {
                    utilization: 0.99,
                    reset: None,
                    observed_at: r.now(),
                    source: WindowSource::Poll,
                    plan: None,
                    tier: None,
                },
            ),
            incarnation: 1,
            source_request_id: None,
        })
        .await;
    r.h.router.samples().await.unwrap();
    r.h.router
        .stamp_projection_over("main", "a", FIVE_HOUR, since, None)
        .await;
    let mut events = r.h.router.subscribe_events();
    r.observe("main").await;
    let seen = relay_common::router_events_until(&mut events, |event| {
        matches!(event, RouterEvent::SheddingRound { .. })
    })
    .await;
    let Some(RouterEvent::SheddingRound { round, .. }) = seen.last() else {
        unreachable!("the wait ends on the round");
    };
    assert_eq!(
        round
            .pins
            .iter()
            .find(|p| p.agent_id == "sub")
            .map(|p| p.skip),
        Some(Some("ended")),
        "the release reached the router as the agent's end: {round:?}"
    );
}

/// The session's facts as the conversion of a report keeps them.
fn kept(fields: serde_json::Value) -> SessionFacts {
    let mut body = serde_json::json!({
        "protocol": 1,
        "event": "session_facts",
        "session_id": "session",
        "instance_id": "instance",
        "sequence": 1,
    });
    body.as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    match decode_json_event(&serde_json::to_vec(&body).unwrap()) {
        Ok(report) => match report.event {
            Reported::SessionFacts { facts } => facts,
            other => panic!("a session_facts report, not {other:?}"),
        },
        Err(refused) => panic!("the report is taken, not refused as {}", refused.code),
    }
}

#[test]
fn a_title_is_cut_at_a_character_boundary() {
    let title = format!("{}é", "a".repeat(255));
    assert_eq!(
        kept(serde_json::json!({ "title": title })).title(),
        Some("a".repeat(255).as_str())
    );
}

#[test]
fn a_title_loses_its_control_and_format_characters() {
    assert_eq!(
        kept(serde_json::json!({ "title": "fix\u{1b}[31m\u{202e}the\n\tcart" })).title(),
        Some("fix[31mthe cart")
    );
}

#[test]
fn a_path_over_its_cap_is_absent() {
    let whole = format!("/{}", "p".repeat(1_023));
    assert_eq!(
        kept(serde_json::json!({ "project": whole })).project(),
        Some(whole.as_str()),
        "a path of 1,024 bytes is kept"
    );
    let over = format!("/{}", "p".repeat(1_024));
    assert_eq!(
        kept(serde_json::json!({ "project": over })).project(),
        None,
        "a path of 1,025 bytes would name another directory if cut"
    );
}

#[test]
fn a_remote_is_reduced_to_host_and_path() {
    let long_raw = format!(
        "https://git.example/acme/storefront.git?{}",
        "x".repeat(1_000)
    );
    let long_reduced = format!("git.example/{}", "p".repeat(1_013));
    let rows: [(&str, Option<&str>); 29] = [
        (
            "https://user:token@Git.Example/acme/storefront.git?x=1#f",
            Some("git.example/acme/storefront"),
        ),
        (
            "git@git.example:acme/storefront.git",
            Some("git.example/acme/storefront"),
        ),
        (
            "ssh://git@git.example:2222/acme/storefront/",
            Some("git.example/acme/storefront"),
        ),
        (
            "git.example/acme/storefront",
            Some("git.example/acme/storefront"),
        ),
        ("file:///srv/git/storefront.git", None),
        ("/srv/git/storefront.git", None),
        ("C:\\work\\storefront", None),
        ("https://user:pa/ss@git.example/acme/storefront.git", None),
        (
            "ssh://git@[2001:db8::1]:2222/acme/storefront.git",
            Some("[2001:db8::1]/acme/storefront"),
        ),
        ("https://to/ken?x@git.example/acme/storefront.git", None),
        (
            "https://user:8080/pa#ss@git.example/acme/storefront.git",
            None,
        ),
        ("user:pa/ss?x@git.example:acme/storefront.git", None),
        ("file://localhost/srv/git/notes.git", None),
        (
            "https://git.example/acme/tools.git.git",
            Some("git.example/acme/tools.git"),
        ),
        (
            "git.example/acme/tools.git",
            Some("git.example/acme/tools.git"),
        ),
        (long_raw.as_str(), Some("git.example/acme/storefront")),
        ("https://git.example:abc/acme/storefront.git", None),
        (
            "[2001:db8::1]/acme/storefront",
            Some("[2001:db8::1]/acme/storefront"),
        ),
        (long_reduced.as_str(), None),
        ("C:/work/storefront", None),
        ("ssh://git@[2001:db8::1:2222/acme/storefront.git", None),
        (
            "https://Git.\u{c9}xample/acme/storefront.git",
            Some("git.\u{c9}xample/acme/storefront"),
        ),
        (
            "https://git.example/acme/storefront.git#f",
            Some("git.example/acme/storefront"),
        ),
        (
            "git@git.example:Acme/Storefront.git",
            Some("git.example/Acme/Storefront"),
        ),
        (
            "git.example/Acme/Storefront",
            Some("git.example/Acme/Storefront"),
        ),
        ("https://git.example/acme/store front.git", None),
        ("https://git.example/acme\\storefront.git", None),
        ("../repos/storefront", None),
        ("~/repos/storefront", None),
    ];
    let wrong: Vec<String> = rows
        .iter()
        .filter_map(|(remote, want)| {
            let facts = kept(serde_json::json!({ "repository": remote }));
            (facts.repository() != *want)
                .then(|| format!("{remote}: got {:?}, want {want:?}", facts.repository()))
        })
        .collect();
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[test]
fn a_title_of_control_characters_alone_is_absent() {
    assert_eq!(
        kept(serde_json::json!({ "title": "\u{1b}\u{202e}\u{200b}\n" })).title(),
        None
    );
}
