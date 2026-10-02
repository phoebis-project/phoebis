mod common;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use phoebis_alerts::{
    admitted, body, signature, Kind, ReqwestWebhook, ScopeType, Webhook, BACKOFF, LINK_LOCAL,
    LOOPBACK, UNSPECIFIED,
};
use phoebis_store::{Secret, StorageKey, Store};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn a_secret_signs_the_body_a_failing_endpoint_gets_three_attempts_and_a_test_posts_once() {
    let rig = rig().await;
    rig.store
        .upsert_alert_rule(
            &rule("signed", Kind::FundingRequired, ScopeType::AnyAccount, None),
            Some(Some(&Secret::new("s3cret"))),
            "test",
        )
        .unwrap();
    rig.alerts.reload().unwrap();
    rig.webhook.answer(Err("connect timeout".into()));
    rig.webhook.answer(Ok(503));
    rig.webhook.answer(Ok(200));
    rig.alerts.signal(&signal(
        Kind::FundingRequired,
        Some("direct"),
        None,
        None,
        "direct",
    ));
    settled(&rig).await;
    let posts = rig.webhook.posts();
    assert_eq!(posts.len(), 3, "three attempts until the 200");
    assert_eq!(
        posts[0].signature.as_deref(),
        Some(signature(&Secret::new("s3cret"), &posts[0].raw).as_str()),
        "the signature covers the bytes that were posted"
    );
    assert!(posts[0]
        .signature
        .as_deref()
        .unwrap()
        .starts_with("sha256="));
    let event = &rig.store.open_alert_events().unwrap()[0];
    let rows = rig.store.alert_deliveries(&event.id).unwrap();
    assert_eq!(
        rows.iter()
            .map(|d| (d.attempt, d.status, d.error.as_deref()))
            .collect::<Vec<_>>(),
        [
            (1, None, Some("connect timeout")),
            (2, Some(503), None),
            (3, Some(200), None)
        ]
    );
    assert!(rows.iter().all(|d| d.state == "opened"));
    let text = rig
        .metrics
        .render(&phoebis_ledger::RoutingGauges::default(), &[], 0);
    assert!(
        text.contains(r#"phoebis_alert_deliveries_total{outcome="delivered",rule="signed"} 1"#),
        "{text}"
    );
    assert!(
        text.contains(r#"phoebis_alert_deliveries_total{outcome="failed",rule="signed"} 2"#),
        "{text}"
    );
    // A dead endpoint: three attempts, all failed, and the event stays open with its rows.
    rig.webhook.answer(Err("refused".into()));
    rig.webhook.answer(Err("refused".into()));
    rig.webhook.answer(Err("refused".into()));
    rig.alerts.signal(&signal(
        Kind::FundingRequired,
        Some("business"),
        None,
        None,
        "business",
    ));
    settled(&rig).await;
    let dead = rig
        .store
        .open_alert_events()
        .unwrap()
        .into_iter()
        .find(|e| e.subject == "business")
        .unwrap();
    assert_eq!(rig.store.alert_deliveries(&dead.id).unwrap().len(), 3);
    assert_eq!(rig.webhook.posts().len(), 6);
    // The test delivery: one post, state test, no event row.
    rig.webhook.answer(Ok(204));
    let result = rig.alerts.test("signed").await;
    assert_eq!((result.status, result.error.as_deref()), (Some(204), None));
    let last = rig.webhook.posts().pop().unwrap();
    assert_eq!(
        (last.body["state"].as_str(), last.body["subject"].as_str()),
        (Some("test"), Some("test"))
    );
    assert!(last.signature.is_some());
    assert_eq!(
        rig.store.open_alert_events().unwrap().len(),
        2,
        "a test opens nothing"
    );
    assert_eq!(BACKOFF.map(|d| d.as_secs()), [1, 4, 16]);
}

#[tokio::test]
async fn a_resolve_waits_behind_the_open_it_follows() {
    let rig = rig().await;
    rig.store
        .upsert_alert_rule(
            &rule("ordered", Kind::ReauthRequired, ScopeType::AnyAccount, None),
            None,
            "test",
        )
        .unwrap();
    rig.alerts.reload().unwrap();
    // The open's first attempt hangs in the receiver; the event resolves meanwhile; the resolve must not overtake.
    let hold = rig.webhook.hold_posts();
    rig.alerts.signal(&signal(
        Kind::ReauthRequired,
        Some("business"),
        None,
        None,
        "business",
    ));
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        rig.webhook.posts().len(),
        1,
        "the open is in flight and held"
    );
    rig.alerts.clear(Kind::ReauthRequired, "business");
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        rig.webhook.posts().len(),
        1,
        "the resolve waits in the event's queue while the open is held"
    );
    assert_eq!(
        rig.store.alert_events(Some("resolved"), 10).unwrap().len(),
        1,
        "the store already says resolved"
    );
    rig.webhook.release_posts(&hold);
    settled(&rig).await;
    let states: Vec<String> = rig
        .webhook
        .posts()
        .iter()
        .map(|p| p.body["state"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        states,
        ["opened", "resolved"],
        "the receiver saw the open before the resolve"
    );
    let event = &rig.store.alert_events(Some("resolved"), 10).unwrap()[0];
    let rows = rig.store.alert_deliveries(&event.id).unwrap();
    assert_eq!(
        rows.iter().map(|d| d.state.as_str()).collect::<Vec<_>>(),
        ["opened", "resolved"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resolve_that_commits_mid_transition_still_reaches_the_receiver_second() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::OnceLock;

    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    let engine: Arc<OnceLock<Arc<phoebis_alerts::Alerts>>> = Arc::new(OnceLock::new());
    let armed = Arc::new(AtomicBool::new(false));
    let fired = Arc::new(AtomicBool::new(false));
    let resolver: Arc<Mutex<Option<std::thread::JoinHandle<()>>>> = Arc::new(Mutex::new(None));
    let runtime = tokio::runtime::Handle::current();
    let hook: ClockHook = {
        let (store, engine, armed, fired, resolver) = (
            store.clone(),
            engine.clone(),
            armed.clone(),
            fired.clone(),
            resolver.clone(),
        );
        Arc::new(move || {
            // The first armed read that finds the row is the one inside the open's own ledger record: the
            // event is committed and its opened transition is not queued yet, which is the interval a
            // resolve could otherwise be enqueued in front of.
            if !armed.load(Ordering::SeqCst)
                || store.open_alert_events().is_ok_and(|o| o.is_empty())
            {
                return;
            }
            armed.store(false, Ordering::SeqCst);
            fired.store(true, Ordering::SeqCst);
            let alerts = engine.get().expect("the rig is built").clone();
            let runtime = runtime.clone();
            let thread = std::thread::spawn(move || {
                let _entered = runtime.enter();
                alerts.clear(Kind::ReauthRequired, "business");
            });
            // Long enough for that resolve to commit and enqueue if nothing holds it behind this open.
            std::thread::sleep(Duration::from_millis(200));
            *resolver.lock().unwrap() = Some(thread);
        })
    };
    let rig = rig_with_hook(store, hook).await;
    engine.set(rig.alerts.clone()).ok();
    rig.store
        .upsert_alert_rule(
            &rule("ordered", Kind::ReauthRequired, ScopeType::AnyAccount, None),
            None,
            "test",
        )
        .unwrap();
    rig.alerts.reload().unwrap();
    armed.store(true, Ordering::SeqCst);
    rig.alerts.signal(&signal(
        Kind::ReauthRequired,
        Some("business"),
        None,
        None,
        "business",
    ));
    let thread = resolver.lock().unwrap().take().expect("the resolve ran");
    thread.join().unwrap();
    settled(&rig).await;
    assert!(
        fired.load(Ordering::SeqCst),
        "the hook never found its window, so this test proved nothing"
    );
    let states: Vec<String> = rig
        .webhook
        .posts()
        .iter()
        .map(|p| p.body["state"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        states,
        ["opened", "resolved"],
        "the queue's order is the order the two transitions committed in"
    );
    assert_eq!(
        rig.store.alert_events(Some("resolved"), 10).unwrap().len(),
        1,
        "the store ends where the receiver does"
    );
}

#[test]
fn the_destination_filter_keeps_what_the_policy_allows_out_of_an_answer() {
    let at = |a: &str| a.parse::<SocketAddr>().unwrap();
    // A name that answers only with addresses of the refused classes leaves nothing to connect to.
    assert_eq!(admitted([at("127.0.0.1:443")], false), Err(LOOPBACK));
    assert_eq!(admitted([at("[::1]:443")], false), Err(LOOPBACK));
    assert_eq!(
        admitted([at("[::ffff:127.0.0.1]:443")], false),
        Err(LOOPBACK),
        "an IPv4-mapped answer reaches the IPv4 host"
    );
    assert_eq!(admitted([at("[::127.0.0.1]:443")], false), Err(LOOPBACK));
    assert_eq!(admitted([at("169.254.169.254:80")], false), Err(LINK_LOCAL));
    assert_eq!(admitted([at("[fe80::1]:80")], false), Err(LINK_LOCAL));
    assert_eq!(admitted([at("0.0.0.0:80")], false), Err(UNSPECIFIED));
    assert_eq!(admitted([at("[::]:80")], false), Err(UNSPECIFIED));
    // A mixed answer is reached at the address the policy allows, without the one it does not.
    assert_eq!(
        admitted([at("198.51.100.7:443"), at("127.0.0.1:443")], false),
        Ok(vec![at("198.51.100.7:443")])
    );
    // A name rebinding between attempts: the second answer is refused however the first went.
    assert_eq!(
        admitted([at("198.51.100.7:443")], false),
        Ok(vec![at("198.51.100.7:443")])
    );
    assert_eq!(admitted([at("127.0.0.1:443")], false), Err(LOOPBACK));
    // With local destinations allowed, the answer is passed through as it came.
    assert_eq!(
        admitted([at("127.0.0.1:443")], true),
        Ok(vec![at("127.0.0.1:443")])
    );
}

#[tokio::test]
async fn a_webhook_whose_name_resolves_locally_is_refused_before_it_connects() {
    // `localhost` is resolved out of the host's own configuration and reaches no network; every address it
    // can answer with is loopback, which is what the deliverer refuses.
    let webhook = ReqwestWebhook::new(false);
    let (addr, seen) = loopback("202 Accepted", false).await;
    let refused = webhook
        .post(
            format!("http://localhost:{}/hook", addr.port()),
            br#"{"state":"opened"}"#.to_vec(),
            None,
        )
        .await
        .expect_err("the destination is refused");
    assert!(
        refused.contains("loopback"),
        "the attempt records why it failed: {refused}"
    );
    assert!(
        seen.lock().unwrap().is_empty(),
        "and the receiver was never reached"
    );
}

#[test]
fn the_body_is_the_spec_shape_and_the_signature_is_hmac_sha256_hex() {
    use phoebis_alerts::{Rule, Scope};
    let rule = Rule {
        id: "r1".into(),
        kind: Kind::ReauthRequired,
        scope: Scope {
            kind: ScopeType::AnyAccount,
            id: None,
        },
        threshold: None,
        window: None,
        webhook_url: "u".into(),
        has_secret: true,
        enabled: true,
    };
    let event = phoebis_store::AlertEventRow {
        id: "e1".into(),
        rule_id: "r1".into(),
        kind: "reauth_required".into(),
        subject: "business".into(),
        state: "open".into(),
        opened_at: 5,
        resolved_at: None,
        detail_json: Some(r#"{"account":"business"}"#.into()),
    };
    let raw = body(&rule, &event, "opened", "phoebis", 0);
    let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(
        v.as_object().unwrap().keys().cloned().collect::<Vec<_>>(),
        [
            "detail",
            "kind",
            "opened_at",
            "resolved_at",
            "router",
            "rule",
            "state",
            "subject"
        ]
    );
    assert_eq!(v["detail"]["account"], "business");
    assert_eq!(
        signature(
            &Secret::new("key"),
            b"The quick brown fox jumps over the lazy dog"
        ),
        "sha256=f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
    );
}

#[tokio::test]
async fn a_signed_rule_whose_secret_cannot_be_read_posts_nothing_and_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("registry.sqlite");
    let rig = rig_with_store(Arc::new(
        Store::open(&path, &StorageKey::generate()).unwrap(),
    ))
    .await;
    rig.store
        .upsert_alert_rule(
            &rule("signed", Kind::FundingRequired, ScopeType::AnyAccount, None),
            Some(Some(&Secret::new("s3cret"))),
            "test",
        )
        .unwrap();
    rig.alerts.reload().unwrap();
    // The sealed bytes rot under a running engine: a restore from a backup taken under another storage key, or
    // a byte the disk lost. The rule still says it is signed, and the receiver still expects a signature.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE alert_rules SET secret_sealed = ?1 WHERE id = 'signed'",
        [b"garbage".to_vec()],
    )
    .unwrap();
    drop(conn);
    rig.alerts.signal(&signal(
        Kind::FundingRequired,
        Some("direct"),
        None,
        None,
        "direct",
    ));
    settled(&rig).await;
    assert!(
        rig.webhook.posts().is_empty(),
        "a rule the operator signed never posts unsigned"
    );
    let event = &rig.store.open_alert_events().unwrap()[0];
    assert_eq!(
        rig.store
            .alert_deliveries(&event.id)
            .unwrap()
            .iter()
            .map(|d| (d.attempt, d.status, d.error.as_deref()))
            .collect::<Vec<_>>(),
        [
            (1, None, Some("secret unavailable")),
            (2, None, Some("secret unavailable")),
            (3, None, Some("secret unavailable"))
        ],
        "every attempt says why it did not go out"
    );
    let text = rig
        .metrics
        .render(&phoebis_ledger::RoutingGauges::default(), &[], 0);
    assert!(
        text.contains(r#"phoebis_alert_deliveries_total{outcome="failed",rule="signed"} 3"#),
        "{text}"
    );
    let result = rig.alerts.test("signed").await;
    assert_eq!(
        (result.status, result.error.as_deref()),
        (None, Some("secret unavailable")),
        "the operator's test delivery answers the failure rather than posting unsigned"
    );
    assert!(rig.webhook.posts().is_empty());
}

#[derive(Clone)]
struct Received {
    body: Vec<u8>,
    headers: Vec<(String, String)>,
}

impl Received {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// An HTTP/1.1 receiver on the loopback that records every request it is handed and answers `status_line`,
/// with a `Location` back to itself when `redirect` is set, so a client that followed it is recorded twice.
async fn loopback(status_line: &str, redirect: bool) -> (SocketAddr, Arc<Mutex<Vec<Received>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = seen.clone();
    let answer = format!(
        "HTTP/1.1 {status_line}\r\n{}content-length: 0\r\nconnection: close\r\n\r\n",
        if redirect {
            format!("location: http://{addr}/hook\r\n")
        } else {
            String::new()
        }
    );
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut raw = Vec::new();
            let mut chunk = [0u8; 1024];
            let head_end = loop {
                if let Some(at) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    break at + 4;
                }
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0, "the connection closed before the request head");
                raw.extend_from_slice(&chunk[..read]);
            };
            // The request line carries no colon of its own, but skipping it keeps the split unambiguous.
            let headers: Vec<(String, String)> = String::from_utf8_lossy(&raw[..head_end])
                .lines()
                .skip(1)
                .filter_map(|line| line.split_once(':'))
                .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
                .collect();
            let length: usize = headers
                .iter()
                .find(|(name, _)| name == "content-length")
                .and_then(|(_, value)| value.parse().ok())
                .unwrap_or(0);
            while raw.len() < head_end + length {
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0, "the connection closed before the request body");
                raw.extend_from_slice(&chunk[..read]);
            }
            recorder.lock().unwrap().push(Received {
                body: raw[head_end..head_end + length].to_vec(),
                headers,
            });
            socket.write_all(answer.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
        }
    });
    (addr, seen)
}

#[tokio::test]
async fn the_reqwest_webhook_posts_the_signed_bytes_and_reports_a_redirect_rather_than_following_it(
) {
    let webhook = ReqwestWebhook::new(true);
    let raw = br#"{"state":"opened","subject":"business"}"#.to_vec();
    let signed = signature(&Secret::new("s3cret"), &raw);

    let (addr, seen) = loopback("202 Accepted", false).await;
    let status = webhook
        .post(
            format!("http://{addr}/hook"),
            raw.clone(),
            Some(signed.clone()),
        )
        .await
        .unwrap();
    assert_eq!(status, 202, "a 2xx comes back as its status");
    let got = seen
        .lock()
        .unwrap()
        .first()
        .cloned()
        .expect("the receiver saw the post");
    // Header names travel case-insensitively; the receiver lowercased what it read off the wire.
    assert_eq!(got.header("x-phoebis-signature"), Some(signed.as_str()));
    assert_eq!(got.header("content-type"), Some("application/json"));
    assert_eq!(
        got.body, raw,
        "the bytes that were signed are the bytes that arrived"
    );
    assert!(got
        .header("user-agent")
        .is_some_and(|agent| agent.starts_with("phoebis/")));

    let (addr, seen) = loopback("302 Found", true).await;
    let status = webhook
        .post(format!("http://{addr}/hook"), raw.clone(), None)
        .await
        .unwrap();
    assert_eq!(
        status, 302,
        "a redirect is a status to report, not a hop to take"
    );
    fake_upstream::stays_for(Duration::from_millis(100), || {
        seen.lock().unwrap().len() == 1
    })
    .await;
}
