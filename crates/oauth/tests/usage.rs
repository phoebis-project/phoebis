use phoebis_oauth::parse_usage;
use phoebis_profile::fixtures::{control_response, ControlFixture};

#[test]
#[allow(clippy::bool_assert_comparison)]
fn the_synthetic_usage_body_parses_into_windows() {
    let (windows, fable, extra, raw) =
        parse_usage(control_response(ControlFixture::Usage)).unwrap();
    let five = windows.iter().find(|w| w.key == "five_hour").unwrap();
    assert_eq!(five.utilization, Some(12.5));
    assert!(five.resets_at.is_some());
    assert!(fable.is_none());
    assert_eq!(extra.unwrap().is_enabled, false);
    assert!(raw.contains(&"seven_day".to_string()));
}

#[test]
fn the_fable_window_comes_from_limits_regardless_of_the_named_key() {
    let body = br#"{"five_hour":{"utilization":0.1,"resets_at":"2026-09-13T15:00:00+00:00"},"cinder_cove":{"utilization":0.9,"resets_at":null},"limits":[{"kind":"weekly_scoped","group":"weekly","percent":42,"resets_at":"2026-09-18T00:00:00+00:00","scope":{"model":{"display_name":"Fable"}}}],"extra_usage":{"is_enabled":true,"monthly_limit":5000,"used_credits":1234,"utilization":0.2468,"currency":"USD"}}"#;
    let (windows, fable, extra, _) = parse_usage(body).unwrap();
    let f = fable.unwrap();
    assert_eq!(f.utilization, Some(42.0));
    assert!(
        windows.iter().all(|w| w.key != "extra_usage"),
        "purchased credit is not a quota window"
    );
    let e = extra.unwrap();
    assert_eq!(e.used_credits, Some(1234));
    assert_eq!(
        e.utilization,
        Some(0.2468),
        "the provider's value is passed through unscaled"
    );
}

mod service {
    use phoebis_oauth::{Clock, CredentialManager, UsageService};
    use phoebis_profile::{
        claude_code_2_1_269_linux_x64,
        fixtures::{control_response, ControlFixture},
    };
    use phoebis_store::{AccountKind, AccountRow, AccountState, NewBundle, Secret, StorageKey, Store};
    use phoebis_transport::{
        connector_builder,
        control::{ControlClient, Resolver},
        Plane,
    };
    use fake_upstream::{FakeUpstream, Respond, Rule};
    use std::sync::{
        atomic::{AtomicI64, Ordering},
        Arc,
    };
    use std::time::Duration;

    fn store_with_account() -> Arc<Store> {
        let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
        store
            .insert_account(&AccountRow {
                id: "a".into(),
                kind: AccountKind::Oauth,
                label: "a".into(),
                enabled: true,
                identity: Some(phoebis_store::Identity {
                    account_uuid: "00000000-0000-4000-8000-000000000001".into(),
                    email: "test@example.invalid".into(),
                    organization_uuid: None,
                }),
                lifecycle_generation: 1,
                state: AccountState::Ready,
            })
            .unwrap();
        store
    }

    #[tokio::test]
    async fn usage_is_cached_and_survives_a_rate_limited_refresh() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let usage_rule = Rule {
            path_prefix: "/api/oauth/usage".into(),
            respond: Respond::Json {
                status: 200,
                headers: vec![],
                body: control_response(ControlFixture::Usage).to_vec(),
            },
        };
        let up = FakeUpstream::start(vec![usage_rule]).await;
        let p = claude_code_2_1_269_linux_x64();
        let mut b = connector_builder(p, Plane::Control).unwrap();
        phoebis_transport::trust_extra_ca(&mut b, up.ca_pem()).unwrap();
        let addr = up.addr();
        let resolver: Resolver = Arc::new(move |_| Some(addr));
        let control = Arc::new(ControlClient::new(p, b.build(), resolver));
        let store = store_with_account();
        // A bundle far from expiry, so `acquire` never triggers a refresh of its own against this fake.
        store
            .commit_credentials(
                "a",
                1,
                None,
                None,
                &NewBundle {
                    access_token: Secret::new("sk-ant-oat01-PLACEHOLDER"),
                    refresh_token: Some(Secret::new("rt")),
                    issued_at: 1_000_000,
                    expires_at: 1_000_000 + 3_600_000,
                    refresh_expires_at: None,
                    scopes: "user:profile user:inference".into(),
                },
            )
            .unwrap();
        let n = now.clone();
        let clock: Clock = Arc::new(move || n.load(Ordering::SeqCst));
        let manager = Arc::new(CredentialManager::new(
            store.clone(),
            control.clone(),
            p,
            clock.clone(),
        ));
        let service = UsageService::new(manager, control, p, Duration::from_secs(60), 4, clock);

        let requests = || {
            up.requests()
                .iter()
                .filter(|r| r.line.starts_with("GET /api/oauth/usage"))
                .count()
        };

        service.snapshot("a").await.unwrap();
        service.snapshot("a").await.unwrap();
        assert_eq!(requests(), 1, "two calls within the TTL share one fetch");

        now.store(1_061_000, Ordering::SeqCst); // past the 60 s TTL
        service.snapshot("a").await.unwrap();
        assert_eq!(requests(), 2, "a call past the TTL fetches again");

        up.set_rule_after(
            0,
            "/api/oauth/usage",
            Respond::Json {
                status: 429,
                headers: vec![("Retry-After".to_string(), "120".to_string())],
                body: b"{}".to_vec(),
            },
        );

        now.store(1_122_000, Ordering::SeqCst); // past the TTL again
        let fourth = service.snapshot("a").await.unwrap();
        let fifth = service.snapshot("a").await.unwrap();
        assert_eq!(
            requests(),
            3,
            "the 429 is fetched once and then honored until Retry-After"
        );
        for result in [&fourth, &fifth] {
            assert!(
                result.error.is_some(),
                "the rate limit is surfaced as an error"
            );
            let five_hour = result
                .windows
                .iter()
                .find(|w| w.key == "five_hour")
                .unwrap();
            assert_eq!(
                five_hour.utilization,
                Some(12.5),
                "the previous windows are kept while the account is rate-limited"
            );
        }
        up.shutdown().await;
    }
}
