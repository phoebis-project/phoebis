mod common;

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
    assert!(
        windows.iter().all(|w| w.key != "cinder_cove"),
        "the alias is the same quota under an older name, so it is not a window of its own"
    );
    assert_eq!(
        windows.len(),
        1,
        "five_hour is all that is left once the alias is folded into the Fable window"
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
    use phoebis_oauth::{Clock, CredentialManager, PollOutcome, UsageService};
    use phoebis_profile::{
        claude_code_2_1_269_linux_x64,
        fixtures::{control_response, ControlFixture},
    };
    use phoebis_store::{AccountKind, AccountRow, AccountState, NewBundle, Secret, StorageKey, Store};
    use phoebis_transport::{
        connector_builder,
        control::{ControlClient, ResolveFuture, Resolver},
        Plane,
    };
    use fake_upstream::{wait_for, FakeUpstream, Respond, Rule};
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
                incarnation: 1,
                revision: 1,
                state: AccountState::Ready,
            })
            .unwrap();
        store
    }

    /// A `Ready` account with a bundle far from expiry, so `acquire` never triggers a refresh of its own
    /// against the fake upstream under test.
    fn store_with_valid_bundle() -> Arc<Store> {
        store_with_bundle_until(1_000_000 + 3_600_000)
    }

    /// The same, for a test whose clock has to run further than an hour.
    fn store_with_bundle_until(expires_at: i64) -> Arc<Store> {
        let store = store_with_account();
        store
            .commit_credentials(
                "a",
                1,
                1,
                None,
                None,
                &NewBundle {
                    access_token: Secret::new("sk-ant-oat01-PLACEHOLDER"),
                    refresh_token: Some(Secret::new("rt")),
                    issued_at: 1_000_000,
                    expires_at,
                    refresh_expires_at: None,
                    scopes: "user:profile user:inference".into(),
                },
            )
            .unwrap();
        store
    }

    fn service_for(
        up: &FakeUpstream,
        store: Arc<Store>,
        cache_ttl: Duration,
        now: &Arc<AtomicI64>,
    ) -> UsageService {
        let p = claude_code_2_1_269_linux_x64();
        let mut b = connector_builder(p, Plane::Control).unwrap();
        phoebis_transport::trust_extra_ca(&mut b, up.ca_pem()).unwrap();
        let addr = up.addr();
        let resolver: Resolver =
            Arc::new(move |_| Box::pin(async move { Some(addr) }) as ResolveFuture);
        let control = Arc::new(ControlClient::new(p, b.build(), resolver));
        let n = now.clone();
        let clock: Clock = Arc::new(move || n.load(Ordering::SeqCst));
        let manager = Arc::new(CredentialManager::new(
            store,
            control.clone(),
            p,
            clock.clone(),
        ));
        UsageService::new(manager, control, p, cache_ttl, 4, clock)
    }

    /// A service over a fake control plane that answers both `/api/oauth/profile` and `/api/oauth/usage` with
    /// their fixtures, for the account `business`, at 200. Both rules exist from the start because
    /// `set_rule_after` panics for a prefix that has no rule, and a test that switches one endpoint's response
    /// mid-run needs the other endpoint still able to succeed.
    async fn service_with_fake() -> (UsageService, FakeUpstream, Arc<Store>) {
        let up = FakeUpstream::start(vec![
            Rule {
                path_prefix: "/api/oauth/profile".into(),
                respond: Respond::Json {
                    status: 200,
                    headers: vec![],
                    body: control_response(ControlFixture::Profile).to_vec(),
                },
            },
            Rule {
                path_prefix: "/api/oauth/usage".into(),
                respond: Respond::Json {
                    status: 200,
                    headers: vec![],
                    body: control_response(ControlFixture::Usage).to_vec(),
                },
            },
        ])
        .await;
        let store = Arc::new(Store::open_in_memory(&StorageKey::generate()).unwrap());
        store
            .insert_account(&AccountRow {
                id: "business".into(),
                kind: AccountKind::Oauth,
                label: "business".into(),
                enabled: true,
                identity: Some(phoebis_store::Identity {
                    account_uuid: "00000000-0000-4000-8000-000000000001".into(),
                    email: "test@example.invalid".into(),
                    organization_uuid: None,
                }),
                lifecycle_generation: 1,
                incarnation: 1,
                revision: 1,
                state: AccountState::Ready,
            })
            .unwrap();
        store
            .commit_credentials(
                "business",
                1,
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
        let now = Arc::new(AtomicI64::new(1_000_000));
        let service = service_for(&up, store.clone(), Duration::from_secs(60), &now);
        (service, up, store)
    }

    /// A usage fetch that is already out when a profile 429 arms the hold: its commit replaces the cache
    /// entry the hold lives in, so the hold has to outlive the replacement or the account is free to fetch
    /// again after the ordinary TTL.
    #[tokio::test]
    async fn a_hold_armed_while_a_usage_fetch_was_out_survives_its_commit() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![
            Rule {
                path_prefix: "/api/oauth/profile".into(),
                respond: Respond::Json {
                    status: 200,
                    headers: vec![],
                    body: control_response(ControlFixture::Profile).to_vec(),
                },
            },
            Rule {
                path_prefix: "/api/oauth/usage".into(),
                respond: Respond::Gated(Box::new(Respond::Json {
                    status: 200,
                    headers: vec![],
                    body: control_response(ControlFixture::Usage).to_vec(),
                })),
            },
        ])
        .await;
        let service = Arc::new(service_for(
            &up,
            store_with_valid_bundle(),
            Duration::from_secs(60),
            &now,
        ));
        let fetching = {
            let service = service.clone();
            tokio::spawn(async move { service.snapshot("a").await })
        };
        let usage_requests = || {
            up.requests()
                .iter()
                .filter(|r| r.line.starts_with("GET /api/oauth/usage"))
                .count()
        };
        wait_for(Duration::from_secs(5), || {
            (usage_requests() == 1).then_some(())
        })
        .await;
        up.set_rule_after(
            0,
            "/api/oauth/profile",
            Respond::Json {
                status: 429,
                headers: vec![("retry-after".into(), "3600".into())],
                body: b"{}".to_vec(),
            },
        );
        assert!(service.profile("a").await.is_err(), "the hold is armed");
        up.release();
        assert!(
            fetching.await.unwrap().is_ok(),
            "the fetch that was already out commits its snapshot"
        );
        // Past the 60 s cache TTL, one second short of the fifteen minutes the hour asked for is capped to.
        now.store(1_000_000 + 899_000, Ordering::SeqCst);
        assert!(service.snapshot("a").await.unwrap().error.is_some());
        assert_eq!(
            usage_requests(),
            1,
            "the commit behind the hold did not shorten it"
        );
        now.store(1_000_000 + 900_000, Ordering::SeqCst);
        assert!(service.snapshot("a").await.unwrap().error.is_none());
        assert_eq!(usage_requests(), 2, "the hold ends at the cap");
        up.shutdown().await;
    }

    fn rate_limited(retry_after_seconds: &str) -> Rule {
        Rule {
            path_prefix: "/api/oauth/usage".into(),
            respond: Respond::Json {
                status: 429,
                headers: vec![("Retry-After".to_string(), retry_after_seconds.to_string())],
                body: b"{}".to_vec(),
            },
        }
    }

    /// `rate_limited` without the header, so the wait is the ladder's.
    fn rate_limited_bare() -> Rule {
        Rule {
            path_prefix: "/api/oauth/usage".into(),
            respond: Respond::Json {
                status: 429,
                headers: vec![],
                body: b"{}".to_vec(),
            },
        }
    }

    fn usage_requests(up: &FakeUpstream) -> usize {
        up.requests()
            .iter()
            .filter(|r| r.line.starts_with("GET /api/oauth/usage"))
            .count()
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
        let resolver: Resolver =
            Arc::new(move |_| Box::pin(async move { Some(addr) }) as ResolveFuture);
        let control = Arc::new(ControlClient::new(p, b.build(), resolver));
        let store = store_with_account();
        // A bundle far from expiry, so `acquire` never triggers a refresh of its own against this fake.
        store
            .commit_credentials(
                "a",
                1,
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

    #[tokio::test]
    async fn a_first_fetch_that_is_rate_limited_backs_off_with_no_prior_snapshot() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![rate_limited("120")]).await;
        let service = service_for(
            &up,
            store_with_valid_bundle(),
            Duration::from_secs(60),
            &now,
        );
        let requests = || {
            up.requests()
                .iter()
                .filter(|r| r.line.starts_with("GET /api/oauth/usage"))
                .count()
        };

        let first = service.snapshot("a").await.unwrap();
        assert!(
            first.error.is_some(),
            "nothing was ever cached to fall back on"
        );
        assert_eq!(requests(), 1);

        now.store(1_030_000, Ordering::SeqCst); // 30 s later, still inside the 60 s TTL
        let second = service.snapshot("a").await.unwrap();
        now.store(1_090_000, Ordering::SeqCst); // 90 s later: past the TTL, still inside the 120 s Retry-After
        let third = service.snapshot("a").await.unwrap();

        assert_eq!(
            requests(),
            1,
            "the not-before survives a first fetch with no prior snapshot to attach it to"
        );
        assert!(second.error.is_some());
        assert!(third.error.is_some());
        up.shutdown().await;
    }

    #[tokio::test]
    async fn the_retry_after_outlasts_a_shorter_cache_ttl() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![rate_limited("120")]).await;
        let service = service_for(&up, store_with_valid_bundle(), Duration::from_secs(1), &now);
        let requests = || {
            up.requests()
                .iter()
                .filter(|r| r.line.starts_with("GET /api/oauth/usage"))
                .count()
        };

        service.snapshot("a").await.unwrap();
        assert_eq!(requests(), 1);

        now.store(1_005_000, Ordering::SeqCst); // past the 1 s TTL, well before the 120 s Retry-After
        let second = service.snapshot("a").await.unwrap();
        assert_eq!(
            requests(),
            1,
            "the retry-after, not the cache TTL, governs when the next fetch is allowed"
        );
        assert!(second.error.is_some());
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_retry_after_past_fifteen_minutes_waits_fifteen() {
        // An hour asked for parks the account's readings for an hour; fifteen minutes is the most the poll
        // waits, as the token refresh does, and a 429 that keeps coming keeps it there.
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![rate_limited("3600")]).await;
        let service = service_for(
            &up,
            store_with_bundle_until(1_000_000 + 86_400_000),
            Duration::from_secs(1),
            &now,
        );

        service.snapshot("a").await.unwrap();
        assert_eq!(usage_requests(&up), 1);

        now.store(1_000_000 + 899_000, Ordering::SeqCst);
        assert!(service.snapshot("a").await.unwrap().error.is_some());
        assert_eq!(usage_requests(&up), 1, "held one second short of the cap");

        now.store(1_000_000 + 900_000, Ordering::SeqCst);
        service.snapshot("a").await.unwrap();
        assert_eq!(usage_requests(&up), 2, "the wait ends at the cap");
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_429_without_retry_after_backs_off_at_one_two_four_eight_and_then_fifteen_minutes() {
        // The fake answers 429 with no header; the clock is the service's, so each step is one fetch past the
        // cache TTL of one second and one more inside the wait.
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![rate_limited_bare()]).await;
        let service = service_for(
            &up,
            store_with_bundle_until(1_000_000 + 86_400_000),
            Duration::from_secs(1),
            &now,
        );
        let (lines, _sink) = crate::common::capture_tracing();
        let mut at = 1_000_000;
        for (step, wait_ms) in [60_000, 120_000, 240_000, 480_000, 900_000, 900_000]
            .into_iter()
            .enumerate()
        {
            now.store(at, Ordering::SeqCst);
            service.snapshot("a").await.unwrap();
            let status = service.poll_status("a").unwrap();
            assert_eq!(
                (
                    status.last_outcome,
                    status.rate_limited_in_a_row,
                    status.next_attempt_at,
                    status.backoff_since
                ),
                (
                    PollOutcome::RateLimited,
                    step as u32 + 1,
                    Some(at + wait_ms),
                    Some(1_000_000)
                ),
                "step {step}"
            );
            if step == 0 {
                assert_eq!(
                    service.snapshot("a").await.unwrap().error.as_deref(),
                    Some("the usage endpoint rate-limited the account; next attempt at 1970-01-01 00:17:40 UTC"),
                    "the held answer names the endpoint and when it may be asked again"
                );
            }
            now.store(at + wait_ms - 1, Ordering::SeqCst);
            assert!(
                service.snapshot("a").await.unwrap().error.is_some(),
                "held one ms short of the wait"
            );
            assert_eq!(usage_requests(&up), step + 1, "no request inside the wait");
            at += wait_ms;
        }
        let warnings = lines
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.contains("rate-limited") && l.contains("account=a"))
            .count();
        assert_eq!(warnings, 6, "one warning per 429, naming the account");
        assert!(
            !lines
                .lock()
                .unwrap()
                .iter()
                .any(|l| l.contains("sk-ant-oat01")),
            "no token reaches a line"
        );
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_clean_answer_ends_the_backoff_and_a_transport_failure_keeps_the_cadence() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![rate_limited_bare()]).await;
        up.set_rule_after(
            2,
            "/api/oauth/usage",
            Respond::Json {
                status: 200,
                headers: vec![],
                body: control_response(ControlFixture::Usage).to_vec(),
            },
        );
        let service = service_for(
            &up,
            store_with_bundle_until(1_000_000 + 86_400_000),
            Duration::from_secs(1),
            &now,
        );

        service.snapshot("a").await.unwrap();
        now.store(1_060_000, Ordering::SeqCst);
        service.snapshot("a").await.unwrap();
        assert_eq!(
            service.poll_status("a").unwrap().rate_limited_in_a_row,
            2,
            "two 429s in a row"
        );
        now.store(1_180_000, Ordering::SeqCst);
        assert!(service.snapshot("a").await.unwrap().error.is_none());
        let status = service.poll_status("a").unwrap();
        assert_eq!(
            (
                status.last_outcome,
                status.rate_limited_in_a_row,
                status.next_attempt_at,
                status.backoff_since,
                status.last_clean_at
            ),
            (PollOutcome::Clean, 0, None, None, Some(1_180_000)),
            "the clean answer ends the run"
        );
        assert_eq!(service.held_for("a"), None);

        up.set_rule_after(
            3,
            "/api/oauth/usage",
            Respond::Json {
                status: 500,
                headers: vec![],
                body: b"{}".to_vec(),
            },
        );
        now.store(1_181_000, Ordering::SeqCst);
        assert!(service.snapshot("a").await.unwrap().error.is_some());
        let status = service.poll_status("a").unwrap();
        assert_eq!(
            (
                status.last_outcome,
                status.rate_limited_in_a_row,
                status.next_attempt_at,
                status.last_clean_at
            ),
            (PollOutcome::Failed, 0, None, Some(1_180_000)),
            "a 5xx is a failed attempt, not a backoff"
        );
        assert_eq!(service.held_for("a"), None);

        up.set_rule_after(4, "/api/oauth/usage", Respond::CloseAfterRequest);
        now.store(1_182_000, Ordering::SeqCst);
        assert!(service.snapshot("a").await.unwrap().error.is_some());
        let status = service.poll_status("a").unwrap();
        assert_eq!(
            (
                status.last_outcome,
                status.rate_limited_in_a_row,
                status.next_attempt_at,
                status.last_attempt_at
            ),
            (PollOutcome::Failed, 0, None, 1_182_000),
            "a transport failure is a failed attempt, not a backoff"
        );
        assert_eq!(service.held_for("a"), None);
        assert_eq!(usage_requests(&up), 5);
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_profile_429_leaves_the_next_usage_answer_marked_as_no_new_reading() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![
            Rule {
                path_prefix: "/api/oauth/profile".into(),
                respond: Respond::Json {
                    status: 429,
                    headers: vec![("retry-after".into(), "120".into())],
                    body: b"{}".to_vec(),
                },
            },
            Rule {
                path_prefix: "/api/oauth/usage".into(),
                respond: Respond::Json {
                    status: 200,
                    headers: vec![],
                    body: control_response(ControlFixture::Usage).to_vec(),
                },
            },
        ])
        .await;
        let service = service_for(
            &up,
            store_with_bundle_until(1_000_000 + 86_400_000),
            Duration::from_secs(3_600),
            &now,
        );
        let (lines, _sink) = crate::common::capture_tracing();
        let clean = service.snapshot("a").await.unwrap();
        assert!(clean.error.is_none());
        assert!(
            service.profile("a").await.is_err(),
            "the profile is rate-limited"
        );
        assert_eq!(
            lines
                .lock()
                .unwrap()
                .iter()
                .filter(|l| l.contains("the profile endpoint rate-limited the account"))
                .count(),
            1,
            "the warning names the endpoint that answered 429"
        );

        // Inside both the cache TTL and the profile's hold: the cached windows are no new reading.
        now.store(1_060_000, Ordering::SeqCst);
        let held = service.snapshot("a").await.unwrap();
        assert_eq!(
            held.error.as_deref(),
            Some("the profile endpoint rate-limited the account; next attempt at 1970-01-01 00:18:40 UTC"),
            "a cache hit inside the hold carries the error"
        );
        assert_eq!(held.windows.len(), clean.windows.len(), "the old windows");
        assert_eq!(held.fetched_at, 1_000_000, "read when they were read");
        assert_eq!(usage_requests(&up), 1, "no request inside the hold");
        let latest = service.latest("a").unwrap();
        assert!(
            latest.error.is_none(),
            "the snapshot held is still the clean one"
        );

        // Past both the 120 s hold and the 3 600 s cache TTL: the endpoint is read again.
        now.store(1_000_000 + 3_601_000, Ordering::SeqCst);
        let fresh = service.snapshot("a").await.unwrap();
        assert!(fresh.error.is_none());
        assert_eq!(fresh.fetched_at, 1_000_000 + 3_601_000);
        assert_eq!(
            usage_requests(&up),
            2,
            "a fresh request past the hold and the TTL"
        );
        assert!(service.latest("a").unwrap().error.is_none());
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_profile_retry_after_past_fifteen_minutes_holds_the_account_fifteen() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![
            Rule {
                path_prefix: "/api/oauth/profile".into(),
                respond: Respond::Json {
                    status: 429,
                    headers: vec![("retry-after".into(), "7200".into())],
                    body: b"{}".to_vec(),
                },
            },
            Rule {
                path_prefix: "/api/oauth/usage".into(),
                respond: Respond::Json {
                    status: 200,
                    headers: vec![],
                    body: control_response(ControlFixture::Usage).to_vec(),
                },
            },
        ])
        .await;
        let service = service_for(
            &up,
            store_with_bundle_until(1_000_000 + 86_400_000),
            Duration::from_secs(1),
            &now,
        );
        let profile_requests = || {
            up.requests()
                .iter()
                .filter(|r| r.line.starts_with("GET /api/oauth/profile"))
                .count()
        };
        assert!(service.profile("a").await.is_err());
        assert_eq!(profile_requests(), 1);

        now.store(1_000_000 + 899_000, Ordering::SeqCst);
        assert!(service.snapshot("a").await.unwrap().error.is_some());
        assert!(service.profile("a").await.is_err());
        assert_eq!(
            (usage_requests(&up), profile_requests()),
            (0, 1),
            "both endpoints are held one second short of the cap"
        );

        now.store(1_000_000 + 901_000, Ordering::SeqCst);
        assert!(service.snapshot("a").await.unwrap().error.is_none());
        assert_eq!(
            usage_requests(&up),
            1,
            "the two hours asked for are capped to fifteen minutes"
        );
        up.shutdown().await;
    }

    /// A fetch whose account is disconnected while it is out is discarded, and the caller gets the cached
    /// entry instead; when a hold was armed meanwhile, that answer is the held one, never the clean windows.
    #[tokio::test]
    async fn a_lifecycle_change_during_a_fetch_under_a_new_hold_answers_the_held_snapshot() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![
            Rule {
                path_prefix: "/api/oauth/profile".into(),
                respond: Respond::Json {
                    status: 429,
                    headers: vec![("retry-after".into(), "600".into())],
                    body: b"{}".to_vec(),
                },
            },
            Rule {
                path_prefix: "/api/oauth/usage".into(),
                respond: Respond::Json {
                    status: 200,
                    headers: vec![],
                    body: control_response(ControlFixture::Usage).to_vec(),
                },
            },
        ])
        .await;
        let store = store_with_bundle_until(1_000_000 + 86_400_000);
        let service = Arc::new(service_for(
            &up,
            store.clone(),
            Duration::from_secs(1),
            &now,
        ));
        assert!(service.snapshot("a").await.unwrap().error.is_none());
        up.set_rule_after(
            1,
            "/api/oauth/usage",
            Respond::Gated(Box::new(Respond::Json {
                status: 200,
                headers: vec![],
                body: control_response(ControlFixture::Usage).to_vec(),
            })),
        );
        now.store(1_010_000, Ordering::SeqCst);
        let fetching = {
            let service = service.clone();
            tokio::spawn(async move { service.snapshot("a").await })
        };
        wait_for(Duration::from_secs(5), || {
            (usage_requests(&up) == 2).then_some(())
        })
        .await;
        assert!(service.profile("a").await.is_err(), "the hold is armed");
        store.bump_lifecycle("a").unwrap();
        up.release();
        let answer = fetching.await.unwrap().unwrap();
        assert_eq!(
            answer.error.as_deref(),
            Some("the profile endpoint rate-limited the account; next attempt at 1970-01-01 00:26:50 UTC"),
            "the discarded fetch answers the held snapshot"
        );
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_poll_reads_the_endpoint_inside_the_cache_ttl_and_joins_a_fetch_already_out() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![Rule {
            path_prefix: "/api/oauth/usage".into(),
            respond: Respond::Json {
                status: 200,
                headers: vec![],
                body: control_response(ControlFixture::Usage).to_vec(),
            },
        }])
        .await;
        let service = service_for(
            &up,
            store_with_bundle_until(1_000_000 + 86_400_000),
            Duration::from_secs(3_600),
            &now,
        );
        service.snapshot("a").await.unwrap();
        now.store(1_010_000, Ordering::SeqCst);
        service.snapshot("a").await.unwrap();
        assert_eq!(
            usage_requests(&up),
            1,
            "a snapshot inside the TTL is the cache's"
        );
        let polled = service.poll("a").await.unwrap();
        assert_eq!(
            (
                usage_requests(&up),
                polled.error.is_none(),
                polled.fetched_at
            ),
            (2, true, 1_010_000),
            "a poll inside the TTL reads the endpoint"
        );
        assert_eq!(
            service.snapshot("a").await.unwrap().fetched_at,
            1_010_000,
            "the poll's answer is the cache's"
        );

        now.store(1_020_000, Ordering::SeqCst);
        let (first, second) = tokio::join!(service.poll("a"), service.poll("a"));
        assert!(first.is_ok() && second.is_ok());
        assert_eq!(
            usage_requests(&up),
            3,
            "two polls at once share one request"
        );

        up.set_rule_after(
            3,
            "/api/oauth/usage",
            Respond::Json {
                status: 429,
                headers: vec![("retry-after".into(), "120".into())],
                body: b"{}".to_vec(),
            },
        );
        now.store(1_030_000, Ordering::SeqCst);
        service.poll("a").await.unwrap();
        assert_eq!(usage_requests(&up), 4);
        now.store(1_040_000, Ordering::SeqCst);
        let held = service.poll("a").await.unwrap();
        assert!(
            held.error.is_some(),
            "a held poll answers the held snapshot"
        );
        assert_eq!(
            held.fetched_at, 1_020_000,
            "the windows of the last clean read"
        );
        assert_eq!(usage_requests(&up), 4, "and makes no request");
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_failed_fetch_keeps_the_timestamp_of_the_last_successful_one() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![Rule {
            path_prefix: "/api/oauth/usage".into(),
            respond: Respond::Json {
                status: 200,
                headers: vec![],
                body: control_response(ControlFixture::Usage).to_vec(),
            },
        }])
        .await;
        let service = service_for(
            &up,
            store_with_valid_bundle(),
            Duration::from_secs(60),
            &now,
        );

        let first = service.snapshot("a").await.unwrap();
        assert_eq!(first.fetched_at, 1_000_000);
        assert!(first.error.is_none());

        up.set_rule_after(
            0,
            "/api/oauth/usage",
            Respond::Json {
                status: 503,
                headers: vec![],
                body: b"{}".to_vec(),
            },
        );
        now.store(1_061_000, Ordering::SeqCst); // past the 60 s TTL, so the next call really fetches
        let second = service.snapshot("a").await.unwrap();
        assert!(second.error.is_some(), "the failure is surfaced");
        assert_eq!(
            second.fetched_at, 1_000_000,
            "these windows were read at t0 and a failed attempt read none"
        );
        assert_eq!(
            second.windows.len(),
            first.windows.len(),
            "the last good windows are what is still on offer"
        );
        up.shutdown().await;
    }

    #[tokio::test]
    async fn the_profile_is_fetched_on_request_under_the_usage_endpoints_limits() {
        let (service, up, store) = service_with_fake().await;
        let profile = service.profile("business").await.unwrap();
        assert_eq!(profile.email, "test@example.invalid");
        assert_eq!(
            profile.raw["organization"]["rate_limit_tier"], "default_claude_max_5x",
            "the whole document travels"
        );
        assert_eq!(
            up.requests()
                .iter()
                .filter(|r| r.line.starts_with("GET /api/oauth/profile"))
                .count(),
            1
        );
        store
            .set_profile("business", &profile.raw.to_string(), &profile.subscription)
            .unwrap();
        assert_eq!(
            store.profile_json("business").unwrap().as_deref(),
            Some(profile.raw.to_string().as_str())
        );
        // A 429 on the profile endpoint arms the same hold the usage fetch honours. The usage route is made to
        // answer so the assertion can only pass through the hold: a fetch that ran would succeed, and
        // `invalidate` is never called here because it drops the cache entry the hold lives in.
        up.set_rule_after(
            0,
            "/api/oauth/profile",
            Respond::Json {
                status: 429,
                headers: vec![("retry-after".into(), "3600".into())],
                body: b"{}".to_vec(),
            },
        );
        up.set_rule_after(
            0,
            "/api/oauth/usage",
            Respond::Json {
                status: 200,
                headers: vec![],
                body: control_response(ControlFixture::Usage).to_vec(),
            },
        );
        assert!(service.profile("business").await.is_err());
        assert_eq!(
            service.poll_status("business").unwrap().next_attempt_at,
            Some(1_000_000 + 900_000),
            "the hour asked for is held to the fifteen-minute cap"
        );
        let usage = service.snapshot("business").await.unwrap();
        assert!(
            usage.error.is_some(),
            "the usage fetch waits out the profile endpoint's Retry-After"
        );
        assert_eq!(
            up.requests()
                .iter()
                .filter(|r| r.line.starts_with("GET /api/oauth/usage"))
                .count(),
            0,
            "held, not fetched"
        );
    }
}
