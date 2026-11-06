//! What the relay refuses before any dial: the ledger behind its writer at admission, a body holding more elements
//! than the relay reads, refused `400` `body_shape` before its parse, and a key moved to another pool between its
//! admission and its selection, which answers the exhaustion `503` with `Retry-After: 1` under the outcome's obstacle
//! `pool_moved` and files the request `rejected` with `accounts_unavailable`, as an account whose stored uuid the body
//! cannot carry does.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use bytes::Bytes;
use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule};
use http_body_util::BodyExt;
use phoebis_bus::{Bus, Delivery, Event, Outcome, ResponseChunk, QUEUE_DEPTH};
use phoebis_relay::{Accounting, AccountingSource, Limits, Probe, ProbeObstacle};
use tower::ServiceExt;

const A: &str = "sk-ant-oat01-AAAA";
const B: &str = "sk-ant-oat01-BBBB";

/// A Claude Code caller's body, which an OAuth-only pool serves: the session rides in `metadata.user_id`.
const BODY: &str = r#"{"model":"claude-sonnet-4-5","messages":[{"role":"user","content":"hi"}],"max_tokens":8,"metadata":{"user_id":"{\"account_uuid\":\"\",\"session_id\":\"s-1\"}"}}"#;

fn ok_rule() -> Rule {
    Rule {
        path_prefix: "/v1/messages".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"id":"msg_1","type":"message","content":[],"usage":{"input_tokens":1,"output_tokens":1}}"#
                .to_vec(),
        },
    }
}

/// Ahead of `ok_rule` in a test that serves both, since the count path matches that prefix too.
fn count_rule() -> Rule {
    Rule {
        path_prefix: "/v1/messages/count_tokens".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: br#"{"input_tokens":7}"#.to_vec(),
        },
    }
}

/// A JSON `POST` of `BODY` to `path` under the router key, from the Claude Code CLI's user agent with the session
/// and class headers the client sends, with its length declared as a socket would carry it.
fn request_to(path: &str) -> Request<Body> {
    Request::post(path)
        .header("authorization", format!("Bearer {KEY}"))
        .header("content-type", "application/json")
        .header("content-length", BODY.len())
        .header("user-agent", CLAUDE_CLI_USER_AGENT)
        .header("x-claude-code-session-id", "s-1")
        .header("x-claude-code-request-class", "main")
        .body(Body::from(BODY))
        .unwrap()
}

async fn text_of(response: axum::response::Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn probe_of(h: &Harness) -> Probe {
    Probe {
        key_id: "desktop".into(),
        key_generation: h.store.api_key("desktop").unwrap().unwrap().created_at,
    }
}

fn reading(behind: &Arc<AtomicBool>) -> AccountingSource {
    let behind = behind.clone();
    Arc::new(move |_| Accounting {
        behind: behind.load(Ordering::SeqCst),
    })
}

#[tokio::test]
async fn an_inference_a_count_and_a_probe_are_refused_with_ledger_behind_while_the_ledger_is_behind(
) {
    let up = FakeUpstream::start(vec![count_rule(), ok_rule()]).await;
    let behind = Arc::new(AtomicBool::new(true));
    let h = harness_with_accounting(&up, &[oauth("a", A, 0)], reading(&behind)).await;
    for path in ["/v1/messages", "/v1/messages/count_tokens"] {
        let response = h.app.clone().oneshot(request_to(path)).await.unwrap();
        assert_eq!(response.status(), 503, "{path}");
        assert_eq!(response.headers()["retry-after"], "5");
        assert_eq!(response.headers()["x-phoebis-error-code"], "ledger_behind");
        let text = text_of(response).await;
        assert!(
            text.contains("not accepting requests right now") && !text.contains("\"a\""),
            "{text}"
        );
    }
    let probed = h
        .relay
        .probe(probe_of(&h), request_to("/v1/messages"))
        .await;
    assert_eq!(
        probed.status(),
        503,
        "a probe is never paid for while the ledger is behind"
    );
    assert_eq!(probed.headers()["x-phoebis-error-code"], "ledger_behind");
    assert!(up.requests().is_empty(), "nothing reached the upstream");
    behind.store(false, Ordering::SeqCst);
    let response = h
        .app
        .clone()
        .oneshot(request_to("/v1/messages"))
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", text_of(response).await);
    up.shutdown().await;
}

/// Each refusal the relay answers before any dial, with the request that draws it and the status and code it
/// answers.
fn refusals() -> Vec<(&'static str, Request<Body>, u16, &'static str)> {
    let with = |mut request: Request<Body>, name: &'static str, value: &str| {
        request.headers_mut().insert(name, value.parse().unwrap());
        request
    };
    let without = |mut request: Request<Body>, name: &'static str| {
        request.headers_mut().remove(name);
        request
    };
    vec![
        (
            "inference",
            request_to("/v1/messages"),
            503,
            "ledger_behind",
        ),
        (
            "missing class header",
            without(request_to("/v1/messages"), "x-claude-code-request-class"),
            503,
            "ledger_behind",
        ),
        (
            "count",
            request_to("/v1/messages/count_tokens"),
            503,
            "ledger_behind",
        ),
        (
            "unknown key",
            with(
                request_to("/v1/messages"),
                "authorization",
                "Bearer phoebis-not-a-key",
            ),
            401,
            "auth_unknown",
        ),
        (
            "missing key",
            without(request_to("/v1/messages"), "authorization"),
            401,
            "auth_missing",
        ),
        (
            "unsupported coding",
            with(request_to("/v1/messages"), "content-encoding", "br"),
            415,
            "unsupported_content_encoding",
        ),
        (
            "past the per-body bound",
            with(request_to("/v1/messages"), "content-length", "40000000"),
            413,
            "request_too_large",
        ),
        (
            "past the byte budget",
            with(request_to("/v1/messages"), "content-length", "2000000"),
            503,
            "body_budget_exhausted",
        ),
        (
            "repeated member",
            {
                let body = BODY.replacen(
                    r#""max_tokens":8"#,
                    r#""max_tokens":8,"model":"claude-opus-5""#,
                    1,
                );
                Request::post("/v1/messages")
                    .header("authorization", format!("Bearer {KEY}"))
                    .header("content-type", "application/json")
                    .header("content-length", body.len())
                    .header("user-agent", CLAUDE_CLI_USER_AGENT)
                    .body(Body::from(body))
                    .unwrap()
            },
            503,
            "ledger_behind",
        ),
    ]
}

/// A writer that stopped draining fills the bus queue behind its channel; every refusal answered before a dial
/// publishes its own facts best-effort then, so none of them holds its connection on the full queue.
#[tokio::test]
async fn every_refusal_answers_within_the_deadline_while_the_writer_channel_and_the_bus_queue_are_full(
) {
    let up = FakeUpstream::start(vec![count_rule(), ok_rule()]).await;
    let bus = Bus::new();
    let sleeper = bus.subscribe("ledger", Delivery::Durable);
    for index in 0..QUEUE_DEPTH {
        bus.publish_fact(Event::ResponseChunk(ResponseChunk {
            request_id: "filler".into(),
            attempt: 1,
            index: index as u32,
            body: Bytes::from_static(b"x"),
            phase: None,
        }))
        .await;
    }
    let behind = Arc::new(AtomicBool::new(true));
    let h = harness_with_bus_and_accounting(
        &up,
        &[oauth("a", A, 0)],
        bus,
        Limits {
            body_budget_bytes: 1 << 20,
            ..default_limits()
        },
        reading(&behind),
    )
    .await;
    const ROUNDS: usize = 50;
    let mut answered = 0u64;
    for _ in 0..ROUNDS {
        for (name, request, status, code) in refusals() {
            let response =
                tokio::time::timeout(Duration::from_secs(1), h.app.clone().oneshot(request))
                    .await
                    .unwrap_or_else(|_| panic!("the {name} refusal waited on the full queue"))
                    .unwrap();
            assert_eq!(response.status(), status, "{name}");
            assert_eq!(response.headers()["x-phoebis-error-code"], code, "{name}");
            answered += 1;
        }
        let probed = tokio::time::timeout(
            Duration::from_secs(1),
            h.relay.probe(probe_of(&h), request_to("/v1/messages")),
        )
        .await
        .expect("the probe's refusal waited on the full queue");
        assert_eq!(probed.status(), 503);
        assert_eq!(probed.headers()["x-phoebis-error-code"], "ledger_behind");
        answered += 1;
    }
    assert!(up.requests().is_empty(), "nothing reached the upstream");
    // An observation and a completion per refusal, both refused by the full queue and counted.
    assert!(
        sleeper.stats.dropped.load(Ordering::SeqCst) >= 2 * answered,
        "{} dropped for {answered} refusals",
        sleeper.stats.dropped.load(Ordering::SeqCst)
    );
    up.shutdown().await;
}

/// The table above holds the ledger behind, where the missing class header answers `ledger_behind` first. On a
/// current ledger the same request meets the contract.
#[tokio::test]
async fn a_claude_code_inference_request_without_the_class_header_is_refused_for_the_contract_on_a_current_ledger(
) {
    let up = FakeUpstream::start(vec![count_rule(), ok_rule()]).await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let behind = Arc::new(AtomicBool::new(false));
    let h = harness_with_bus_and_accounting(
        &up,
        &[oauth("a", A, 0)],
        bus,
        default_limits(),
        reading(&behind),
    )
    .await;
    let mut request = request_to("/v1/messages");
    request.headers_mut().remove("x-claude-code-request-class");
    let response = h.app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), 403);
    assert_eq!(
        response.headers()["x-phoebis-error-code"],
        "client_contract"
    );
    let text = text_of(response).await;
    assert!(text.contains("x-claude-code-request-class"), "{text}");
    let complete = drain_until_complete(&mut sub)
        .await
        .into_iter()
        .find_map(|event| match event {
            Event::RequestComplete(complete) => Some(complete),
            _ => None,
        })
        .unwrap();
    assert_eq!(complete.client_contract.as_deref(), Some("class_header"));
    assert!(up.requests().is_empty(), "nothing reached the upstream");
    up.shutdown().await;
}

#[tokio::test]
async fn a_key_moved_between_pools_mid_flight_is_refused_and_the_next_request_admits_under_the_new_pool(
) {
    let up = FakeUpstream::start(vec![ok_rule()]).await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let h = harness_with_bus(
        &up,
        &[oauth("a", A, 0), oauth("b", B, 1)],
        PoolSpec::default(),
        bus,
    )
    .await;
    // `main` keeps a and `q` holds b alone, so the account that serves names the pool the request went out under.
    h.store
        .set_pool_members("main", &[member("a", 0)], "test")
        .unwrap();
    let mut q = h.store.pool("main").unwrap().unwrap();
    q.id = "q".into();
    q.label = "q".into();
    h.store.insert_pool(&q, "test").unwrap();
    h.store
        .set_pool_members("q", &[member("b", 0)], "test")
        .unwrap();
    h.router.reload().await.unwrap();
    // One slot: `reserve` returns once the relay took the first chunk, which it reads after it pinned its snapshot
    // and authenticated the key under `main`.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(1);
    let (head, tail) = BODY.split_at(BODY.len() / 2);
    tx.send(Ok(Bytes::from_static(head.as_bytes())))
        .await
        .unwrap();
    let request = Request::post("/v1/messages")
        .header("authorization", format!("Bearer {KEY}"))
        .header("content-type", "application/json")
        .header("content-length", BODY.len())
        .header("user-agent", CLAUDE_CLI_USER_AGENT)
        .header("x-claude-code-session-id", "s-1")
        .header("x-claude-code-request-class", "main")
        .body(Body::from_stream(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))
        .unwrap();
    let app = h.app.clone();
    let inflight = tokio::spawn(async move { app.oneshot(request).await.unwrap() });
    let permit = tx.reserve().await.unwrap();
    let mut desktop = h.store.api_key("desktop").unwrap().unwrap();
    desktop.pool_id = "q".into();
    h.store
        .update_api_key(&desktop, desktop.updated_at, "test")
        .unwrap();
    h.router.reload().await.unwrap();
    permit.send(Ok(Bytes::from_static(tail.as_bytes())));
    drop(tx);
    let response = inflight.await.unwrap();
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .map(|value| value.to_str().unwrap().to_string())
    };
    assert_eq!(
        (
            response.status().as_u16(),
            header("retry-after"),
            header("x-phoebis-error-code")
        ),
        (503, Some("1".into()), Some("accounts_unavailable".into()))
    );
    assert_eq!(outcome(&response).obstacle, Some(ProbeObstacle::PoolMoved));
    let text = text_of(response).await;
    assert!(
        text.contains("no account in this pool can serve requests"),
        "{text}"
    );
    for named in ["\"b\"", "\"q\"", "\"main\""] {
        assert!(
            !text.contains(named),
            "the refusal names neither the account nor a pool: {text}"
        );
    }
    assert!(up.requests().is_empty(), "nothing reached the upstream");
    let complete = drain_until_complete(&mut sub)
        .await
        .into_iter()
        .find_map(|event| match event {
            Event::RequestComplete(complete) => Some(complete),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        (complete.outcome, complete.error_code.as_deref()),
        (Outcome::Rejected, Some("accounts_unavailable")),
        "a moved key is no exhaustion of the pool"
    );
    let response = h
        .app
        .clone()
        .oneshot(request_to("/v1/messages"))
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", text_of(response).await);
    assert_eq!(credentials_seen(&up), [B], "served under q's member");
    up.shutdown().await;
}

/// The uuid is spliced into an escaped string, so a stored one holding a space refuses the leg rather than corrupting
/// the body, and the refusal is the account's own standing, never the pool's exhaustion its code names elsewhere.
#[tokio::test]
async fn an_oauth_account_whose_stored_uuid_is_malformed_answers_503_and_files_rejected() {
    let up = FakeUpstream::start(vec![ok_rule()]).await;
    let bus = Bus::new();
    let mut sub = bus.subscribe("test", Delivery::Durable);
    let malformed = TestAccount {
        uuid: Some("00000000-0000 4000-8000-000000000001"),
        ..oauth("a", A, 0)
    };
    let h = harness_with_bus(&up, &[malformed], PoolSpec::default(), bus).await;
    let response = h
        .app
        .clone()
        .oneshot(request_to("/v1/messages"))
        .await
        .unwrap();
    assert_eq!(
        (
            response.status().as_u16(),
            response.headers()["x-phoebis-error-code"].to_str().unwrap()
        ),
        (503, "accounts_unavailable")
    );
    assert!(up.requests().is_empty(), "nothing reached the upstream");
    let complete = drain_until_complete(&mut sub)
        .await
        .into_iter()
        .find_map(|event| match event {
            Event::RequestComplete(complete) => Some(complete),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        (complete.outcome, complete.error_code.as_deref()),
        (Outcome::Rejected, Some("accounts_unavailable"))
    );
    up.shutdown().await;
}

/// Mirrors `MAX_BODY_ELEMENTS`, which no reader outside the relay's crate reaches, so a change to the bound turns the
/// case below red until this follows it.
const ELEMENT_BOUND: usize = 2_000_000;

/// `BODY`'s members with a lone quote for the prompt, so an escaped quote stands ahead of the elements, and a top-level
/// `filler` array of `empties` empty arrays, then `extra`, then a last string holding brackets and commas. Outside
/// strings the body counts `BODY`'s eight marks, the comma ahead of `filler`, the array's bracket, two marks per empty
/// array and `extra`'s own.
fn crowded(empties: usize, extra: &str) -> String {
    format!(
        r#"{{"model":"claude-sonnet-4-5","messages":[{{"role":"user","content":"\""}}],"max_tokens":8,"metadata":{{"user_id":"{{\"account_uuid\":\"\",\"session_id\":\"s-1\"}}"}},"filler":[{}{extra}"[[,,]]"]}}"#,
        "[],".repeat(empties)
    )
}

#[tokio::test]
async fn a_body_one_element_past_the_bound_is_refused_for_its_shape_and_one_at_the_bound_is_admitted(
) {
    let up = FakeUpstream::start(vec![ok_rule()]).await;
    let h = harness(&up, &[oauth("a", A, 0)], PoolSpec::default()).await;
    let post = |body: String| {
        Request::post("/v1/messages")
            .header("authorization", format!("Bearer {KEY}"))
            .header("content-type", "application/json")
            .header("content-length", body.len())
            .header("user-agent", CLAUDE_CLI_USER_AGENT)
            .header("x-claude-code-session-id", "s-1")
            .header("x-claude-code-request-class", "main")
            .body(Body::from(body))
            .unwrap()
    };
    // The ten marks ahead of the empty arrays and two per array land on the bound, and the extra `1,` adds its comma.
    let empties = (ELEMENT_BOUND - 10) / 2;
    let over = h
        .app
        .clone()
        .oneshot(post(crowded(empties, "1,")))
        .await
        .unwrap();
    assert_eq!(
        (
            over.status().as_u16(),
            over.headers()
                .get("x-phoebis-error-code")
                .map(|code| code.to_str().unwrap())
        ),
        (400, Some("body_shape"))
    );
    assert!(up.requests().is_empty(), "nothing reached the upstream");
    let at = h
        .app
        .clone()
        .oneshot(post(crowded(empties, "")))
        .await
        .unwrap();
    assert_eq!(at.status(), 200, "{}", text_of(at).await);
    up.shutdown().await;
}
