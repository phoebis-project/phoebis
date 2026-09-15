mod common;
use phoebis_relay::DiscoveryError;
use phoebis_store::{ServedModelWrite, StorageKey, Store};
use common::*;
use fake_upstream::{FakeUpstream, Respond, Rule};
use std::sync::Arc;
use tower::ServiceExt;

const A: &str = "sk-ant-oat01-AAAA";

fn models(entries: &[(&str, Option<u64>)]) -> Vec<u8> {
    let data: Vec<serde_json::Value> = entries
        .iter()
        .map(|(id, max)| {
            let mut v = serde_json::json!({ "id": id, "type": "model" });
            if let Some(m) = max {
                v["max_input_tokens"] = serde_json::json!(m);
            }
            v
        })
        .collect();
    serde_json::json!({ "data": data, "has_more": false })
        .to_string()
        .into_bytes()
}

fn gzip(payload: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(payload).unwrap();
    e.finish().unwrap()
}

#[tokio::test]
async fn a_sync_adds_new_ids_unselected_flags_vanished_ids_and_keeps_manual_rows_across_a_restart()
{
    let up = FakeUpstream::start(vec![
        Rule {
            path_prefix: "/v1/messages".into(),
            respond: json_200(),
        },
        Rule {
            path_prefix: "/v1/models".into(),
            respond: Respond::Json {
                status: 200,
                headers: vec![],
                body: models(&[
                    ("claude-sonnet-4-5", Some(200_000)),
                    ("claude-opus-5", Some(1_000_000)),
                ]),
            },
        },
    ])
    .await;
    up.set_rule_after(
        1,
        "/v1/models",
        Respond::Json {
            status: 200,
            headers: vec![("Content-Encoding".into(), "gzip".into())],
            body: gzip(&models(&[
                ("claude-sonnet-4-5", None),
                ("claude-haiku-4-5", None),
            ])),
        },
    );
    let dir = tempfile::tempdir().unwrap();
    let key = StorageKey::generate();
    let store = Arc::new(Store::open(&dir.path().join("router.sqlite"), &key).unwrap());
    let h = harness_on(
        store.clone(),
        &up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
        &[],
        default_limits(),
    )
    .await;
    let first = h.catalog.sync("a", "test").await.unwrap();
    assert!(
        first.delta.as_ref().is_some_and(|d| d.first_sync)
            && first.error.is_none()
            && !first.seeded
    );
    let rows = store.served_models("a").unwrap();
    assert!(rows.iter().all(|m| m.selected));
    assert!(rows
        .iter()
        .any(|m| m.upstream_id == "claude-opus-5" && m.long_context));
    assert_eq!(
        outcome(
            &h.app
                .clone()
                .oneshot(request_for(KEY, "claude-opus-5", "s1"))
                .await
                .unwrap()
        )
        .account_id,
        "a",
        "the reload made the catalog routable"
    );
    store
        .put_served_models(
            "a",
            &[
                ServedModelWrite {
                    upstream_id: "claude-sonnet-4-5".into(),
                    selected: true,
                    long_context: false,
                },
                ServedModelWrite {
                    upstream_id: "claude-opus-5".into(),
                    selected: true,
                    long_context: true,
                },
                ServedModelWrite {
                    upstream_id: "Vendor/Model-A".into(),
                    selected: true,
                    long_context: false,
                },
            ],
            "admin",
        )
        .unwrap();
    let second = h.catalog.sync("a", "test").await.unwrap();
    let delta = second.delta.unwrap();
    assert_eq!(
        (delta.added, delta.missing),
        (
            vec!["claude-haiku-4-5".to_string()],
            vec!["claude-opus-5".to_string()]
        )
    );
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-haiku-4-5", "s2"))
        .await
        .unwrap();
    assert_eq!(
        (
            resp.status().as_u16(),
            resp.headers()
                .get("x-phoebis-error-code")
                .unwrap()
                .to_str()
                .unwrap()
        ),
        (404, "model_unserved")
    );
    assert_eq!(
        outcome(
            &h.app
                .clone()
                .oneshot(request_for(KEY, "vendor/model-a", "s3"))
                .await
                .unwrap()
        )
        .account_id,
        "a",
        "the manual row survived the sync"
    );
    let messages_before = up
        .requests()
        .iter()
        .filter(|r| r.line.starts_with("POST /v1/messages"))
        .count();
    let (synced_at, error) = store.catalog_status("a").unwrap();
    assert!(synced_at.is_some() && error.is_none());
    drop(h);
    drop(store);
    // The router task and its writer let go of the store once their last handle is gone, on their next turn.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let store = Arc::new(Store::open(&dir.path().join("router.sqlite"), &key).unwrap());
    let h = harness_on(
        store.clone(),
        &up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
        &[],
        default_limits(),
    )
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(request_for(KEY, "claude-haiku-4-5", "s2"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 404, "still unserved after the restart");
    assert_eq!(
        up.requests()
            .iter()
            .filter(|r| r.line.starts_with("POST /v1/messages"))
            .count(),
        messages_before,
        "no upstream call for an unselected model"
    );
    store
        .put_served_models(
            "a",
            &[ServedModelWrite {
                upstream_id: "claude-haiku-4-5".into(),
                selected: true,
                long_context: false,
            }],
            "admin",
        )
        .unwrap();
    h.router.reload().await.unwrap();
    assert_eq!(
        h.app
            .clone()
            .oneshot(request_for(KEY, "claude-haiku-4-5", "s2"))
            .await
            .unwrap()
            .status(),
        200
    );
    up.shutdown().await;
}

#[tokio::test]
async fn a_failed_sync_on_an_oauth_account_seeds_the_built_in_catalog_until_a_sync_succeeds() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/messages".into(),
        respond: json_200(),
    }])
    .await;
    let h = harness_with(
        &up,
        &[oauth("a", A, 0)],
        PoolSpec::default(),
        &[],
        default_limits(),
    )
    .await;
    let outcome_ = h.catalog.sync("a", "test").await.unwrap();
    assert!(outcome_.seeded && outcome_.delta.is_none());
    assert!(outcome_.error.as_deref().is_some_and(|e| e.contains("404")));
    let ids: Vec<String> = h
        .store
        .served_models("a")
        .unwrap()
        .into_iter()
        .map(|m| m.upstream_id)
        .collect();
    assert_eq!(
        ids,
        phoebis_profile::catalog::built_in()
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        h.app
            .clone()
            .oneshot(request_for(KEY, "claude-sonnet-4-5", "s1"))
            .await
            .unwrap()
            .status(),
        200,
        "the fixture model is in the seed"
    );
    assert!(h.store.catalog_status("a").unwrap().1.is_some());
    up.add_credential_rule(
        A,
        "/v1/models",
        Respond::Json {
            status: 200,
            headers: vec![],
            body: models(&[("claude-sonnet-4-5", None)]),
        },
    );
    let again = h.catalog.sync("a", "test").await.unwrap();
    assert!(!again.seeded && again.error.is_none());
    assert!(
        h.store.catalog_status("a").unwrap().1.is_none(),
        "a successful sync clears the error"
    );
    assert!(
        h.store
            .served_models("a")
            .unwrap()
            .iter()
            .any(|m| m.upstream_id == "claude-opus-5" && m.missing),
        "the seed rows are discovered rows and go missing like any other"
    );
    assert!(
        !h.catalog.sync("a", "test").await.unwrap().seeded,
        "a populated catalog is never re-seeded"
    );
    up.shutdown().await;
}

#[tokio::test]
async fn discovery_sends_the_measured_request_shape_on_each_dispatch_path_and_a_401_is_unauthorized(
) {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/models".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: models(&[("claude-sonnet-4-5", None)]),
        },
    }])
    .await;
    up.add_credential_rule(
        "sk-ant-api03-BAD",
        "/v1/models",
        Respond::Json {
            status: 401,
            headers: vec![],
            body: b"{}".to_vec(),
        },
    );
    let h = harness_with(
        &up,
        &[
            oauth("a", A, 0),
            api_key("k", "sk-ant-api03-KEY", 0),
            api_key("bad", "sk-ant-api03-BAD", 0),
        ],
        PoolSpec::default(),
        &[],
        default_limits(),
    )
    .await;
    let a = h.store.account("a").unwrap().unwrap();
    assert_eq!(
        h.catalog.discover(&a).await.unwrap().models,
        [("claude-sonnet-4-5".to_string(), false)]
    );
    let got = &up.requests()[0];
    assert_eq!(got.line, "GET /v1/models?limit=1000 HTTP/1.1");
    let names: Vec<&str> = got.headers.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "Authorization",
            "User-Agent",
            "anthropic-version",
            "Connection",
            "Accept",
            "Host",
            "Accept-Encoding"
        ]
    );
    assert_eq!(got.headers[0].1, format!("Bearer {A}"));
    assert_eq!(got.headers[1].1, "claude-code/2.1.269");
    assert_eq!(got.headers[4].1, "*/*");
    h.store.bump_lifecycle("a").unwrap();
    let disconnected = h.store.account("a").unwrap().unwrap();
    assert!(matches!(
        h.catalog.discover(&disconnected).await,
        Err(DiscoveryError::NoCredential)
    ));
    let k = h.store.account("k").unwrap().unwrap();
    h.catalog.discover(&k).await.unwrap();
    let got = &up.requests()[1];
    assert_eq!(
        got.headers[0],
        ("x-api-key".to_string(), "sk-ant-api03-KEY".to_string())
    );
    assert!(!got.headers.iter().any(|(n, _)| n == "Authorization"));
    let bad = h.store.account("bad").unwrap().unwrap();
    assert!(matches!(
        h.catalog.discover(&bad).await,
        Err(DiscoveryError::Unauthorized)
    ));
    up.shutdown().await;
}

#[tokio::test]
async fn the_scheduler_syncs_every_account_including_a_paused_one() {
    let up = FakeUpstream::start(vec![Rule {
        path_prefix: "/v1/models".into(),
        respond: Respond::Json {
            status: 200,
            headers: vec![],
            body: models(&[("claude-sonnet-4-5", None)]),
        },
    }])
    .await;
    let h = harness_with(
        &up,
        &[oauth("a", A, 0), oauth("b", "sk-ant-oat01-BBBB", 0)],
        PoolSpec::default(),
        &[],
        default_limits(),
    )
    .await;
    h.store.set_enabled("b", false).unwrap();
    let scheduler = h
        .catalog
        .clone()
        .spawn_scheduler(std::time::Duration::from_millis(50));
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    scheduler.abort();
    let seen = up.requests();
    assert!(seen.iter().any(|r| r.credential() == Some(A)));
    assert!(
        seen.iter()
            .any(|r| r.credential() == Some("sk-ant-oat01-BBBB")),
        "a paused account keeps syncing its catalog"
    );
    assert!(h.store.catalog_status("b").unwrap().0.is_some());
    up.shutdown().await;
}
