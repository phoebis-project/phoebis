//! Builders shared by the relay's fixture and streaming tests.
//!
//! Each integration test binary compiles this module separately, so a helper only one binary uses looks dead to
//! the other.
#![allow(dead_code)]

use axum::body::Body;
use axum::http::Request;
use phoebis_profile::{
    claude_code_2_1_269_linux_x64,
    fixtures::{body, inference_head, BodyFixture, FixtureKind},
};
use phoebis_relay::{router, KeyTable, Limits, Relay, Route};
use phoebis_store::{
    AccountKind, AccountRow, AccountState, Identity, NewBundle, Secret, StorageKey, Store,
};
use phoebis_transport::request::parse_head;
use fake_upstream::FakeUpstream;
use std::{sync::Arc, time::Duration};

pub const ACCOUNT_UUID: &str = "00000000-0000-4000-8000-000000000001";

pub async fn relay_with(kind: AccountKind, up: &FakeUpstream) -> axum::Router {
    relay_with_limits(kind, up, default_limits()).await
}

/// The same relay with the request limit set by the caller, for the tests that have to send past it.
pub async fn relay_with_limit(
    kind: AccountKind,
    up: &FakeUpstream,
    max_request_bytes: usize,
) -> axum::Router {
    relay_with_limits(
        kind,
        up,
        Limits {
            max_request_bytes,
            ..default_limits()
        },
    )
    .await
}

/// The same relay with a head timeout a test can actually reach, for the cases about a stalled upstream.
pub async fn relay_with_head_timeout(
    kind: AccountKind,
    up: &FakeUpstream,
    head_timeout: Duration,
) -> axum::Router {
    relay_with_limits(
        kind,
        up,
        Limits {
            head_timeout,
            ..default_limits()
        },
    )
    .await
}

/// Bounds no test is meant to reach unless it asks for them.
fn default_limits() -> Limits {
    Limits {
        max_request_bytes: 32 << 20,
        head_timeout: Duration::from_secs(10),
        idle_timeout: Duration::from_secs(600),
    }
}

pub async fn relay_with_limits(
    kind: AccountKind,
    up: &FakeUpstream,
    limits: Limits,
) -> axum::Router {
    let p = claude_code_2_1_269_linux_x64();
    let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
    let identity = if matches!(kind, AccountKind::Oauth) {
        Some(Identity {
            account_uuid: ACCOUNT_UUID.into(),
            email: "test@example.invalid".into(),
            organization_uuid: None,
        })
    } else {
        None
    };
    store
        .insert_account(&AccountRow {
            id: "acct".into(),
            kind,
            label: "acct".into(),
            enabled: true,
            identity,
            lifecycle_generation: 1,
            state: AccountState::Ready,
        })
        .unwrap();
    match kind {
        // The refresh token is seeded because the 401 replay test drives a real refresh through this account.
        AccountKind::Oauth => {
            store
                .commit_credentials(
                    "acct",
                    1,
                    None,
                    None,
                    &NewBundle {
                        access_token: Secret::new("sk-ant-oat01-PLACEHOLDER"),
                        refresh_token: Some(Secret::new("sk-ant-ort01-PLACEHOLDER")),
                        issued_at: 1_000_000,
                        expires_at: i64::MAX / 2,
                        refresh_expires_at: None,
                        scopes: "user:inference".into(),
                    },
                )
                .unwrap();
        }
        AccountKind::ApiKey => {
            store
                .set_api_key_secret("acct", &Secret::new("sk-ant-api03-PLACEHOLDER"))
                .unwrap();
        }
    }
    let mut b = phoebis_transport::connector_builder(p, phoebis_transport::Plane::Inference).unwrap();
    phoebis_transport::trust_extra_ca(&mut b, up.ca_pem()).unwrap();
    let addr = up.addr();
    let resolver: phoebis_transport::control::Resolver = Arc::new(move |_| Some(addr));
    let mut cb = phoebis_transport::connector_builder(p, phoebis_transport::Plane::Control).unwrap();
    phoebis_transport::trust_extra_ca(&mut cb, up.ca_pem()).unwrap();
    let control = Arc::new(phoebis_transport::control::ControlClient::new(
        p,
        cb.build(),
        resolver.clone(),
    ));
    let clock: phoebis_oauth::Clock = Arc::new(|| 1_000_000);
    let manager = Arc::new(phoebis_oauth::CredentialManager::new(
        store.clone(),
        control,
        p,
        clock,
    ));
    let pool = Arc::new(phoebis_transport::pool::Pool::new(
        b.build(),
        4,
        Duration::from_secs(60),
    ));
    let keys = KeyTable::from_routes(&[Route {
        key_id: "desktop".into(),
        secret: Secret::new("phoebis-testkey"),
        account_id: "acct".into(),
    }])
    .unwrap();
    router(Arc::new(Relay::new(
        p, store, manager, pool, keys, resolver, limits,
    )))
}

pub fn request_from_fixture(head: &[u8], body: &[u8], key: &str) -> Request<Body> {
    let (line, headers) = parse_head(head).unwrap();
    let target = line.split(' ').nth(1).unwrap();
    let mut req = Request::builder().method("POST").uri(target);
    for (n, v) in headers {
        // The caller presents the router's key, not the upstream credential; Host and Content-Length are transport-owned.
        match n.to_ascii_lowercase().as_str() {
            "authorization" => {
                req = req.header("Authorization", format!("Bearer {key}"));
            }
            "x-api-key" => {
                req = req.header("x-api-key", key);
            }
            "host" | "content-length" => {}
            _ => {
                req = req.header(n, v);
            }
        }
    }
    req.body(Body::from(body.to_vec())).unwrap()
}

pub fn wire_of(r: &fake_upstream::RecordedRequest) -> Vec<u8> {
    let mut w = r.line.clone().into_bytes();
    w.extend_from_slice(b"\r\n");
    for (n, v) in &r.headers {
        w.extend_from_slice(format!("{n}: {v}\r\n").as_bytes());
    }
    w.extend_from_slice(b"\r\n");
    w.extend_from_slice(&r.body);
    w
}

/// A streaming request as the native client sends it: the OAuth non-first-party fixture, whose body already
/// carries `"stream":true`.
pub fn stream_request(key: &str) -> Request<Body> {
    request_from_fixture(
        inference_head(FixtureKind::OauthNonFirstParty),
        body(BodyFixture::Oauth),
        key,
    )
}
