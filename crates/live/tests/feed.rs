//! The acceptance row "Live feed": the snapshot's in-flight set equals the relay's; started, head, progress,
//! finished arrive in order; a client that stops reading is disconnected after 1,024 queued messages and the
//! relay is unaffected; the viewer role never receives a preview.
mod common;
use std::time::Duration;

use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule, SseEvent};
use futures::StreamExt;
use http_body_util::BodyExt;
use tower::ServiceExt;

fn events() -> Vec<SseEvent> {
    vec![
        SseEvent { event: "message_start".into(), data: r#"{"type":"message_start","message":{"usage":{"input_tokens":5,"cache_read_input_tokens":2}}}"#.into() },
        SseEvent { event: "content_block_delta".into(), data: r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello "}}"#.into() },
        SseEvent { event: "content_block_delta".into(), data: r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"world"}}"#.into() },
        SseEvent { event: "message_delta".into(), data: r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}"#.into() },
        SseEvent { event: "message_stop".into(), data: r#"{"type":"message_stop"}"#.into() },
    ]
}

fn observed(id: &str, at: i64, capture_hours: u32) -> phoebis_bus::Event {
    phoebis_bus::Event::RequestObserved(phoebis_bus::RequestObserved {
        request_id: id.into(),
        origin: phoebis_bus::Origin::Client,
        kind: phoebis_bus::Kind::Inference,
        key_id: Some("desktop".into()),
        pool_id: Some("main".into()),
        session_id: Some("s".into()),
        agent_id: Some("main".into()),
        parent_agent_id: None,
        owner_id: None,
        model: "claude-sonnet-4-5".into(),
        canonical_model: "claude-sonnet-4-5".into(),
        stream: true,
        headers: vec![],
        body: bytes::Bytes::from_static(b"{}"),
        body_capture_hours: capture_hours,
        received_at: at,
    })
}

fn selected(id: &str, at: i64) -> phoebis_bus::Event {
    phoebis_bus::Event::AccountSelected(phoebis_bus::AccountSelected {
        request_id: id.into(),
        attempt: 1,
        account_id: "a".into(),
        account_kind: "oauth".into(),
        pin_epoch: None,
        started_at: at,
        cache_ttl_ms: None,
    })
}

fn head(id: &str, at: i64) -> phoebis_bus::Event {
    phoebis_bus::Event::ResponseHead(phoebis_bus::ResponseHead {
        request_id: id.into(),
        attempt: 1,
        status: 200,
        headers: vec![],
        first_event: None,
        outcome: phoebis_bus::AttemptOutcome::Succeeded,
        cooldown: None,
        written_at: at,
        observed_at: at,
    })
}

fn chunk(id: &str, index: u32, body: bytes::Bytes) -> phoebis_bus::Event {
    phoebis_bus::Event::ResponseChunk(phoebis_bus::ResponseChunk {
        request_id: id.into(),
        attempt: 1,
        index,
        body,
        phase: None,
    })
}

#[tokio::test]
async fn a_client_gets_the_relays_in_flight_set_then_the_deltas_of_a_streamed_request_in_order() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Stall {
            status: 200,
            headers: vec![],
            events: events(),
        },
    }])
    .await;
    let h = harness(&up, 0).await;
    let app = h.app.clone();
    let stalled = tokio::spawn(async move {
        app.oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
            .await
            .unwrap()
    });
    wait_for(Duration::from_secs(5), || {
        (h.feed.in_flight() == 1).then_some(())
    })
    .await;
    let mut client = connect(&h, true).await;
    let first = next_event(&mut client).await;
    assert_live_event_shape(&first);
    assert_eq!(first["kind"], "snapshot");
    let snapshot = &first["snapshot"];
    assert_eq!(
        snapshot["in_flight"].as_array().unwrap().len(),
        1,
        "the relay holds one request"
    );
    let entry = &snapshot["in_flight"][0];
    assert_eq!(
        (
            entry["state"].as_str(),
            entry["transport"].as_str(),
            entry["key_id"].as_str(),
            entry["user_agent"].as_str(),
            entry["attempts"][0]["account_id"].as_str(),
            entry["attempts"][0]["outcome"].as_str()
        ),
        (
            Some("in_flight"),
            Some("sse"),
            Some("desktop"),
            Some("claude-cli/2.1.269 (external, sdk-cli)"),
            Some("a"),
            Some("in_flight")
        )
    );
    assert!(
        entry["phases"]["received_at"].is_i64()
            && entry["phases"]["selected_at"].is_i64()
            && entry["phases"].get("head_at").is_none(),
        "a phase that has not happened is absent"
    );
    assert!(
        snapshot["tick"]["metrics"]["active_streams"]["series"]
            .as_array()
            .unwrap()
            .len()
            == 60
    );
    let stalled_id = entry["id"].as_str().unwrap().to_string();
    up.release();
    let resp = stalled.await.unwrap();
    let _ = resp.into_body().collect().await.unwrap();
    let deltas = until_delta(&mut client, &stalled_id, "finished").await;
    let kinds: Vec<&str> = deltas
        .iter()
        .filter(|d| d["type"] != "tick")
        .map(|d| d["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds.first(),
        Some(&"head"),
        "the stalled request's head arrives once the stream commits"
    );
    assert!(kinds.contains(&"progress") && kinds.last() == Some(&"finished"));
    // A whole request under the connection: started, selected, head, progress, finished, in that order.
    up.set_rule_after(
        0,
        "/v1/messages",
        Respond::Sse {
            status: 200,
            headers: vec![],
            events: events(),
            delay_between: Duration::from_millis(2),
        },
    );
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s2"))
        .await
        .unwrap();
    let id = request_id(&resp);
    let _ = resp.into_body().collect().await.unwrap();
    let deltas = until_delta(&mut client, &id, "finished").await;
    let kinds: Vec<&str> = deltas
        .iter()
        .filter(|d| d["type"] != "tick")
        .map(|d| d["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["started", "selected", "head", "progress", "finished"]
    );
    let cursors: Vec<u64> = deltas
        .iter()
        .map(|d| d["cursor"].as_u64().unwrap())
        .collect();
    assert!(cursors.windows(2).all(|w| w[0] < w[1]), "cursors increase");
    let finished = deltas.last().unwrap();
    assert_eq!(
        (
            finished["status"].as_u64(),
            finished["usage"]["input"].as_u64(),
            finished["usage"]["output"].as_u64(),
            finished["usage"]["cache_read"].as_u64()
        ),
        (Some(200), Some(5), Some(7), Some(2))
    );
    assert!(
        finished["phases"]["first_content_at"].is_i64()
            && finished["phases"]["stream_end_at"].is_i64()
            && finished["phases"]["finished_at"].is_i64()
    );
    assert!(
        finished["cost_usd"].as_f64().unwrap() > 0.0,
        "sonnet is priced"
    );
    assert!(
        deltas.iter().all(|d| d["type"] != "preview"),
        "the pool captures nothing, so no preview"
    );
    let tick = until_delta(&mut client, "", "tick").await.pop().unwrap();
    assert_eq!(
        tick["tick"]["metrics"]["requests_per_second"]["quality"],
        "measured"
    );
    assert_eq!(
        tick["tick"]["metrics"]["cost_per_minute"]["quality"],
        "estimated"
    );
    assert_eq!(tick["tick"]["metrics"]["error_rate"]["quality"], "derived");
    assert!(
        tick["tick"]["metrics"]["ttfb_p50"]["series"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .is_f64(),
        "every metric keeps a per-second history"
    );
    assert_eq!(
        tick["tick"]["metrics"]["error_rate"]["series"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .as_f64(),
        Some(0.0)
    );
    send_close(&mut client).await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_client_that_stops_reading_is_disconnected_and_the_relay_is_unaffected() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"id":"m","type":"message","usage":{"input_tokens":1,"output_tokens":1}}"#
                .to_vec(),
        },
    }])
    .await;
    let h = harness(&up, 0).await;
    let mut client = connect(&h, true).await;
    let _snapshot = next_event(&mut client).await;
    wait_for(Duration::from_secs(5), || {
        (h.feed.clients() == 1).then_some(())
    })
    .await;
    // A captured request streams 8 KiB of text per chunk and the client reads none of the preview deltas: the
    // socket buffers on both sides fill within a few hundred messages and the feed's queue fills after them,
    // and the count is well past what any kernel buffers, so the disconnect does not depend on their size.
    // The publish yields every so often so the bus queue never drops a chunk.
    h.bus.publish(observed("noisy", NOW, 24));
    let text = "x".repeat(8 * 1024);
    let body = bytes::Bytes::from(format!("event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n"));
    for index in 0..4_000u32 {
        h.bus.publish(chunk("noisy", index, body.clone()));
        if index % 64 == 63 {
            tokio::task::yield_now().await;
        }
    }
    wait_for(Duration::from_secs(10), || {
        (h.feed.clients() == 0).then_some(())
    })
    .await;
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.next().await {
                None => return true,
                Some(Err(_)) => return true,
                Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) => return true,
                Some(Ok(_)) => {}
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(
        ended,
        "the server closed the socket of the client that fell behind"
    );
    // The relay answers whatever the feed's clients do, and a fresh client gets a fresh snapshot.
    for i in 0..20 {
        let resp = h
            .app
            .clone()
            .oneshot(request_for(KEY, "claude-sonnet-4-5", &format!("s{i}")))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let _ = resp.into_body().collect().await.unwrap();
    }
    let mut again = connect(&h, true).await;
    let snapshot = next_event(&mut again).await;
    assert_eq!(snapshot["snapshot"]["recent"].as_array().unwrap().len(), 20);
    assert_eq!(
        snapshot["snapshot"]["in_flight"].as_array().unwrap().len(),
        1,
        "the synthetic request never completed"
    );
    send_close(&mut again).await;
    up.shutdown().await;
}

#[tokio::test]
async fn the_preview_reaches_admin_clients_only_and_is_dropped_at_completion() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Sse {
            status: 200,
            headers: vec![],
            events: events(),
            delay_between: Duration::from_millis(2),
        },
    }])
    .await;
    let h = harness(&up, 24).await;
    // Preview text accumulates on a stalled request before any viewer connects: the snapshot a viewer then
    // receives must not carry it, while an admin's does.
    up.set_rule_after(
        0,
        "/v1/messages",
        Respond::Stall {
            status: 200,
            headers: vec![],
            events: events(),
        },
    );
    let app = h.app.clone();
    let stalled = tokio::spawn(async move {
        app.oneshot(request_for(KEY, "claude-sonnet-4-5", "s0"))
            .await
            .unwrap()
    });
    wait_for(Duration::from_secs(5), || {
        (h.feed.in_flight() == 1).then_some(())
    })
    .await;
    h.bus.publish(phoebis_bus::Event::ResponseChunk(phoebis_bus::ResponseChunk { request_id: h.feed.snapshot(true).in_flight[0].id.clone(), attempt: 1, index: 0, body: bytes::Bytes::from_static(b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"already streamed\"}}\n\n"), phase: None }));
    wait_for(Duration::from_secs(5), || {
        h.feed.snapshot(true).in_flight[0].preview.clone()
    })
    .await;
    let mut late_viewer = connect(&h, false).await;
    let first = next_event(&mut late_viewer).await;
    assert!(
        first["snapshot"]["in_flight"][0]["preview"].is_null(),
        "a viewer's snapshot carries no preview text"
    );
    let mut late_admin = connect(&h, true).await;
    let first = next_event(&mut late_admin).await;
    assert_eq!(
        first["snapshot"]["in_flight"][0]["preview"],
        "already streamed"
    );
    send_close(&mut late_viewer).await;
    send_close(&mut late_admin).await;
    up.release();
    let _ = stalled.await.unwrap().into_body().collect().await;
    up.set_rule_after(
        0,
        "/v1/messages",
        Respond::Sse {
            status: 200,
            headers: vec![],
            events: events(),
            delay_between: Duration::from_millis(2),
        },
    );
    let mut admin = connect(&h, true).await;
    let mut viewer = connect(&h, false).await;
    let _ = next_event(&mut admin).await;
    let _ = next_event(&mut viewer).await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
        .await
        .unwrap();
    let id = request_id(&resp);
    let _ = resp.into_body().collect().await.unwrap();
    let admin_deltas = until_delta(&mut admin, &id, "finished").await;
    let previews: String = admin_deltas
        .iter()
        .filter(|d| d["type"] == "preview")
        .map(|d| d["chunk"].as_str().unwrap())
        .collect();
    assert_eq!(previews, "hello world", "the output text, as it streamed");
    let viewer_deltas = until_delta(&mut viewer, &id, "finished").await;
    assert!(
        viewer_deltas.iter().all(|d| d["type"] != "preview"),
        "the viewer role never receives a preview"
    );
    assert!(
        viewer_deltas.iter().any(|d| d["type"] == "progress"),
        "everything else reaches the viewer"
    );
    let snapshot = h.feed.snapshot(true);
    assert!(
        snapshot.recent.iter().all(|r| r.preview.is_none()),
        "the preview is dropped at completion"
    );
    let past = h.feed.past(0, i64::MAX);
    assert_eq!(past.entries.len(), 2);
    assert!(past.density.iter().map(|b| b.requests).sum::<u64>() == 2);
    send_close(&mut admin).await;
    send_close(&mut viewer).await;
    up.shutdown().await;
}

#[tokio::test]
async fn a_request_whose_completion_the_bus_dropped_is_lost_after_the_ttl_and_a_live_stream_is_kept(
) {
    let up = FakeUpstream::start(vec![]).await;
    let h = harness(&up, 0).await;
    let mut client = connect(&h, true).await;
    let _ = next_event(&mut client).await;
    // Two requests observed and headed; their completions never come. One keeps sending chunks.
    for id in ["lost", "busy"] {
        h.bus.publish(observed(id, NOW, 0));
        h.bus.publish(selected(id, NOW + 1));
        h.bus.publish(head(id, NOW + 2));
    }
    let _ = until_delta(&mut client, "busy", "head").await;
    assert_eq!(h.feed.in_flight(), 2);
    for minute in [5, 10, 14] {
        h.clock
            .store(NOW + minute * 60_000, std::sync::atomic::Ordering::SeqCst);
        h.bus.publish(chunk(
            "busy",
            minute as u32,
            bytes::Bytes::from_static(b"x"),
        ));
        let _ = until_delta(&mut client, "", "tick").await;
    }
    // Fifteen minutes past the last activity of `lost`; `busy` was touched a minute ago.
    h.clock.store(
        NOW + phoebis_live::IN_FLIGHT_TTL_MS + 1,
        std::sync::atomic::Ordering::SeqCst,
    );
    let deltas = until_delta(&mut client, "lost", "finished").await;
    let lost = deltas.last().unwrap();
    assert_eq!(
        (lost["error_code"].as_str(), lost["status"].is_null()),
        (Some("lost"), true),
        "clients drop the row; nothing claims a status"
    );
    assert_eq!(
        h.feed.in_flight(),
        1,
        "the stream that kept sending is still in flight"
    );
    let snapshot = h.feed.snapshot(true);
    assert_eq!(snapshot.in_flight[0].id, "busy");
    assert!(
        snapshot.recent.iter().all(|r| r.id != "lost"),
        "a lost request is not a recent one"
    );
    send_close(&mut client).await;
    // The table is capped: past the limit the oldest silent entry goes first. The fillers are published in
    // batches with a yield between them so the feed keeps up and the bus queue never drops one; `busy` is
    // touched between two batches, so when the cap bites it is newer than the first batch and older than the
    // second, and the first batch is what goes.
    let filler_at = NOW + phoebis_live::IN_FLIGHT_TTL_MS + 2;
    for i in 0..(phoebis_live::IN_FLIGHT_LIMIT - 1) {
        h.bus
            .publish(observed(&format!("f{i}"), filler_at + i as i64, 0));
        if i % 64 == 63 {
            tokio::task::yield_now().await;
        }
    }
    wait_for(Duration::from_secs(10), || {
        (h.feed.in_flight() == phoebis_live::IN_FLIGHT_LIMIT).then_some(())
    })
    .await;
    h.clock.store(
        NOW + phoebis_live::IN_FLIGHT_TTL_MS + 5_000,
        std::sync::atomic::Ordering::SeqCst,
    );
    h.bus
        .publish(chunk("busy", 15, bytes::Bytes::from_static(b"x")));
    wait_for(Duration::from_secs(5), || {
        h.feed
            .snapshot(true)
            .in_flight
            .iter()
            .any(|r| r.id == "busy" && r.bytes_relayed >= 4)
            .then_some(())
    })
    .await;
    h.clock.store(
        NOW + phoebis_live::IN_FLIGHT_TTL_MS + 6_000,
        std::sync::atomic::Ordering::SeqCst,
    );
    for i in 0..6 {
        h.bus.publish(observed(
            &format!("g{i}"),
            NOW + phoebis_live::IN_FLIGHT_TTL_MS + 6_000 + i,
            0,
        ));
    }
    let ids = wait_for(Duration::from_secs(10), || {
        let ids: Vec<String> = h
            .feed
            .snapshot(true)
            .in_flight
            .iter()
            .map(|r| r.id.clone())
            .collect();
        (ids.len() == phoebis_live::IN_FLIGHT_LIMIT && ids.iter().any(|id| id == "g5")).then_some(ids)
    })
    .await;
    assert!(
        ids.iter().any(|id| id == "busy"),
        "the entry with recent activity survives the cap"
    );
    assert!(
        ids.iter().all(|id| id != "f0"),
        "the oldest silent entry went first"
    );
    up.shutdown().await;
}
