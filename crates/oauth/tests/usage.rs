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
    use fake_upstream::{wait_for, FakeUpstream, Respond, Rule};
    use phoebis_oauth::{Clock, CredentialManager, PollOutcome, UsageService};
    use phoebis_profile::{
        claude_code_2_1_269_linux_x64,
        fixtures::{control_response, ControlFixture},
    };
    use phoebis_store::{
        AccountKind, AccountRow, AccountState, NewBundle, Secret, StateCause, StorageKey, Store,
    };
    use phoebis_transport::{
        connector_builder,
        control::{ControlClient, ResolveFuture, Resolver},
        Plane,
    };
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
                state_cause: StateCause::Operator,
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

    fn service_for(up: &FakeUpstream, store: Arc<Store>, now: &Arc<AtomicI64>) -> UsageService {
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
        UsageService::new(manager, control, p, 4, clock)
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
                state_cause: StateCause::Operator,
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
        let service = service_for(&up, store.clone(), &now);
        (service, up, store)
    }

    /// A usage fetch that is already out when a profile 429 arms the hold: its commit replaces the cache
    /// entry the hold lives in, so the hold has to outlive the replacement or the account is free to fetch
    /// again at once.
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
        let service = Arc::new(service_for(&up, store_with_valid_bundle(), &now));
        let fetching = {
            let service = service.clone();
            tokio::spawn(async move { service.poll("a").await })
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
        // One second short of the fifteen minutes the hour asked for is capped to.
        now.store(1_000_000 + 899_000, Ordering::SeqCst);
        assert!(service.poll("a").await.unwrap().error.is_some());
        assert_eq!(
            usage_requests(),
            1,
            "the commit behind the hold did not shorten it"
        );
        now.store(1_000_000 + 900_000, Ordering::SeqCst);
        assert!(service.poll("a").await.unwrap().error.is_none());
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
    async fn a_first_fetch_that_is_rate_limited_backs_off_with_no_prior_snapshot() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![rate_limited("120")]).await;
        let service = service_for(&up, store_with_valid_bundle(), &now);
        let requests = || {
            up.requests()
                .iter()
                .filter(|r| r.line.starts_with("GET /api/oauth/usage"))
                .count()
        };

        let first = service.poll("a").await.unwrap();
        assert!(
            first.error.is_some(),
            "nothing was ever cached to fall back on"
        );
        assert_eq!(requests(), 1);

        now.store(1_030_000, Ordering::SeqCst); // 30 s later, inside the 120 s Retry-After
        let second = service.poll("a").await.unwrap();
        now.store(1_090_000, Ordering::SeqCst); // 90 s later, still inside it
        let third = service.poll("a").await.unwrap();

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
    async fn a_retry_after_past_fifteen_minutes_waits_fifteen() {
        // An hour asked for parks the account's readings for an hour; fifteen minutes is the most the poll
        // waits, as the token refresh does, and a 429 that keeps coming keeps it there.
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![rate_limited("3600")]).await;
        let service = service_for(&up, store_with_bundle_until(1_000_000 + 86_400_000), &now);

        service.poll("a").await.unwrap();
        assert_eq!(usage_requests(&up), 1);

        now.store(1_000_000 + 899_000, Ordering::SeqCst);
        assert!(service.poll("a").await.unwrap().error.is_some());
        assert_eq!(usage_requests(&up), 1, "held one second short of the cap");

        now.store(1_000_000 + 900_000, Ordering::SeqCst);
        service.poll("a").await.unwrap();
        assert_eq!(usage_requests(&up), 2, "the wait ends at the cap");
        up.shutdown().await;
    }

    /// The hold a 429 asks for runs from the answer's arrival, so a request that sat at the endpoint does not
    /// shorten it.
    #[tokio::test]
    async fn a_retry_after_hold_starts_when_the_429_arrives() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![Rule {
            path_prefix: "/api/oauth/usage".into(),
            respond: Respond::Gated(Box::new(rate_limited("120").respond)),
        }])
        .await;
        let service = Arc::new(service_for(
            &up,
            store_with_bundle_until(1_000_000 + 86_400_000),
            &now,
        ));
        let polling = {
            let service = service.clone();
            tokio::spawn(async move { service.poll("a").await })
        };
        wait_for(Duration::from_secs(5), || {
            (usage_requests(&up) == 1).then_some(())
        })
        .await;
        now.store(1_004_000, Ordering::SeqCst);
        up.release();
        assert!(polling.await.unwrap().unwrap().error.is_some());
        let status = service.poll_status("a").unwrap();
        assert_eq!(
            (status.backoff_since, status.next_attempt_at),
            (Some(1_004_000), Some(1_124_000)),
            "the backoff and its two minutes start at the 429's arrival"
        );
        up.shutdown().await;
    }

    /// A reading holds whatever the endpoint counted when the request went out, a request that completed while
    /// the poll waited on its credential among it, so the reading is stamped at the send.
    #[tokio::test]
    async fn a_reading_is_stamped_at_its_send_after_the_credential_wait() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![
            Rule {
                path_prefix: "/v1/oauth/token".into(),
                respond: Respond::Gated(Box::new(Respond::Json {
                    status: 200,
                    headers: vec![],
                    body: control_response(ControlFixture::Token).to_vec(),
                })),
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
        // An expired access token: the poll's acquire refreshes it before the usage request can go out.
        let service = Arc::new(service_for(&up, store_with_bundle_until(1), &now));
        let polling = {
            let service = service.clone();
            tokio::spawn(async move { service.poll("a").await })
        };
        wait_for(Duration::from_secs(5), || {
            up.requests()
                .iter()
                .any(|r| r.line.starts_with("POST /v1/oauth/token"))
                .then_some(())
        })
        .await;
        now.store(1_030_000, Ordering::SeqCst);
        up.release();
        let reading = polling.await.unwrap().unwrap();
        assert!(reading.error.is_none(), "{:?}", reading.error);
        assert_eq!(
            (
                reading.fetched_at,
                service.poll_status("a").unwrap().last_clean_at
            ),
            (1_030_000, Some(1_030_000)),
            "the reading is stamped when its request went out, after the refresh"
        );
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_429_without_retry_after_backs_off_at_two_four_eight_and_then_fifteen_minutes() {
        // The fake answers 429 with no header, and the clock is the service's, so each step is one fetch at the
        // step's start and one more read inside the wait.
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![rate_limited_bare()]).await;
        let service = service_for(&up, store_with_bundle_until(1_000_000 + 86_400_000), &now);
        let (lines, _sink) = crate::common::capture_tracing();
        let mut at = 1_000_000;
        for (step, wait_ms) in [120_000, 240_000, 480_000, 900_000, 900_000, 900_000]
            .into_iter()
            .enumerate()
        {
            now.store(at, Ordering::SeqCst);
            service.poll("a").await.unwrap();
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
                    service.poll("a").await.unwrap().error.as_deref(),
                    Some("the usage endpoint rate-limited the account"),
                    "the held answer names the endpoint, and the status says when it may be asked again"
                );
            }
            now.store(at + wait_ms - 1, Ordering::SeqCst);
            assert!(
                service.poll("a").await.unwrap().error.is_some(),
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
        let service = service_for(&up, store_with_bundle_until(1_000_000 + 86_400_000), &now);

        service.poll("a").await.unwrap();
        now.store(1_120_000, Ordering::SeqCst);
        service.poll("a").await.unwrap();
        assert_eq!(
            service.poll_status("a").unwrap().rate_limited_in_a_row,
            2,
            "two 429s in a row"
        );
        now.store(1_360_000, Ordering::SeqCst);
        assert!(service.poll("a").await.unwrap().error.is_none());
        let status = service.poll_status("a").unwrap();
        assert_eq!(
            (
                status.last_outcome,
                status.rate_limited_in_a_row,
                status.next_attempt_at,
                status.backoff_since,
                status.last_clean_at
            ),
            (PollOutcome::Clean, 0, None, None, Some(1_360_000)),
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
        now.store(1_361_000, Ordering::SeqCst);
        assert!(service.poll("a").await.unwrap().error.is_some());
        let status = service.poll_status("a").unwrap();
        assert_eq!(
            (
                status.last_outcome,
                status.rate_limited_in_a_row,
                status.next_attempt_at,
                status.last_clean_at
            ),
            (PollOutcome::Failed, 0, None, Some(1_360_000)),
            "a 5xx is a failed attempt, not a backoff"
        );
        assert_eq!(service.held_for("a"), None);

        up.set_rule_after(4, "/api/oauth/usage", Respond::CloseAfterRequest);
        now.store(1_362_000, Ordering::SeqCst);
        assert!(service.poll("a").await.unwrap().error.is_some());
        let status = service.poll_status("a").unwrap();
        assert_eq!(
            (
                status.last_outcome,
                status.rate_limited_in_a_row,
                status.next_attempt_at,
                status.last_attempt_at
            ),
            (PollOutcome::Failed, 0, None, 1_362_000),
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
        let service = service_for(&up, store_with_bundle_until(1_000_000 + 86_400_000), &now);
        let (lines, _sink) = crate::common::capture_tracing();
        let clean = service.poll("a").await.unwrap();
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

        // Inside the profile's hold: the cached windows are no new reading.
        now.store(1_060_000, Ordering::SeqCst);
        let held = service.poll("a").await.unwrap();
        assert_eq!(
            held.error.as_deref(),
            Some("the profile endpoint rate-limited the account"),
            "a read inside the hold carries the error"
        );
        assert_eq!(
            service.poll_status("a").unwrap().next_attempt_at,
            Some(1_120_000),
            "the status, not the sentence, says when the account may be asked again"
        );
        assert_eq!(held.windows.len(), clean.windows.len(), "the old windows");
        assert_eq!(held.fetched_at, 1_000_000, "read when they were read");
        assert_eq!(usage_requests(&up), 1, "no request inside the hold");
        let latest = service.latest("a").unwrap();
        assert!(
            latest.error.is_none(),
            "the snapshot held is still the clean one"
        );

        // Past the 120 s hold: the endpoint is read again.
        now.store(1_120_000, Ordering::SeqCst);
        let fresh = service.poll("a").await.unwrap();
        assert!(fresh.error.is_none());
        assert_eq!(fresh.fetched_at, 1_120_000);
        assert_eq!(usage_requests(&up), 2, "a fresh request at the hold's end");
        assert!(service.latest("a").unwrap().error.is_none());
        up.shutdown().await;
    }

    #[tokio::test]
    async fn the_last_error_names_a_failed_attempt_and_a_usage_429_until_a_clean_poll_clears_it() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![Rule {
            path_prefix: "/api/oauth/usage".into(),
            respond: Respond::Json {
                status: 503,
                headers: vec![],
                body: b"{}".to_vec(),
            },
        }])
        .await;
        let service = service_for(&up, store_with_bundle_until(1_000_000 + 86_400_000), &now);
        let last_error = |service: &UsageService| service.poll_status("a").unwrap().last_error;
        let clean = || Respond::Json {
            status: 200,
            headers: vec![],
            body: control_response(ControlFixture::Usage).to_vec(),
        };

        service.poll("a").await.unwrap();
        assert_eq!(
            last_error(&service).as_deref(),
            Some("usage request rejected with status 503"),
            "a failed attempt keeps its message"
        );

        up.set_rule_after(0, "/api/oauth/usage", clean());
        now.store(1_002_000, Ordering::SeqCst);
        assert!(service.poll("a").await.unwrap().error.is_none());
        assert_eq!(last_error(&service), None, "a clean poll clears it");

        up.set_rule_after(0, "/api/oauth/usage", rate_limited_bare().respond);
        now.store(1_004_000, Ordering::SeqCst);
        service.poll("a").await.unwrap();
        assert_eq!(
            last_error(&service).as_deref(),
            Some("the usage endpoint rate-limited the account"),
            "a usage 429 keeps the held answer's sentence"
        );

        up.set_rule_after(0, "/api/oauth/usage", clean());
        now.store(1_125_000, Ordering::SeqCst);
        assert!(service.poll("a").await.unwrap().error.is_none());
        assert_eq!(
            last_error(&service),
            None,
            "the clean poll past the hold clears it"
        );
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_profile_429_sets_the_last_error_to_its_hold_with_or_without_a_reading_held() {
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
        let service = service_for(&up, store_with_bundle_until(1_000_000 + 86_400_000), &now);
        let last_error = |service: &UsageService| service.poll_status("a").unwrap().last_error;

        assert!(service.profile("a").await.is_err());
        assert_eq!(
            last_error(&service).as_deref(),
            Some("the profile endpoint rate-limited the account"),
            "a profile 429 before any reading sets the hold's sentence"
        );

        now.store(1_121_000, Ordering::SeqCst);
        assert!(service.poll("a").await.unwrap().error.is_none());
        assert_eq!(
            last_error(&service),
            None,
            "a clean poll past the hold clears it"
        );

        now.store(1_200_000, Ordering::SeqCst);
        assert!(service.profile("a").await.is_err());
        assert_eq!(
            last_error(&service).as_deref(),
            Some("the profile endpoint rate-limited the account"),
            "a profile 429 over a clean reading sets it again"
        );
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
        let service = service_for(&up, store_with_bundle_until(1_000_000 + 86_400_000), &now);
        let profile_requests = || {
            up.requests()
                .iter()
                .filter(|r| r.line.starts_with("GET /api/oauth/profile"))
                .count()
        };
        assert!(service.profile("a").await.is_err());
        assert_eq!(profile_requests(), 1);

        now.store(1_000_000 + 899_000, Ordering::SeqCst);
        assert!(service.poll("a").await.unwrap().error.is_some());
        assert!(service.profile("a").await.is_err());
        assert_eq!(
            (usage_requests(&up), profile_requests()),
            (0, 1),
            "both endpoints are held one second short of the cap"
        );

        now.store(1_000_000 + 901_000, Ordering::SeqCst);
        assert!(service.poll("a").await.unwrap().error.is_none());
        assert_eq!(
            usage_requests(&up),
            1,
            "the two hours asked for are capped to fifteen minutes"
        );
        up.shutdown().await;
    }

    /// A profile 429's hold runs from the answer's arrival, as a usage 429's does, so a request that sat at the
    /// endpoint does not shorten the wait both endpoints share.
    #[tokio::test]
    async fn a_profile_retry_after_hold_starts_when_the_429_arrives() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![Rule {
            path_prefix: "/api/oauth/profile".into(),
            respond: Respond::Gated(Box::new(Respond::Json {
                status: 429,
                headers: vec![("retry-after".into(), "120".into())],
                body: b"{}".to_vec(),
            })),
        }])
        .await;
        let service = Arc::new(service_for(
            &up,
            store_with_bundle_until(1_000_000 + 86_400_000),
            &now,
        ));
        let asking = {
            let service = service.clone();
            tokio::spawn(async move { service.profile("a").await })
        };
        wait_for(Duration::from_secs(5), || {
            up.requests()
                .iter()
                .any(|r| r.line.starts_with("GET /api/oauth/profile"))
                .then_some(())
        })
        .await;
        now.store(1_004_000, Ordering::SeqCst);
        up.release();
        assert!(asking.await.unwrap().is_err());
        let status = service.poll_status("a").unwrap();
        assert_eq!(
            (status.backoff_since, status.next_attempt_at),
            (Some(1_004_000), Some(1_124_000)),
            "the backoff and its two minutes start at the 429's arrival"
        );
        up.shutdown().await;
    }

    /// A fetch whose account is disconnected and paired again while it is out is discarded, and the caller gets
    /// the new pairing's cached entry instead; when a hold was armed meanwhile, that answer is the held one,
    /// never the clean windows.
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
        let service = Arc::new(service_for(&up, store.clone(), &now));
        assert!(service.poll("a").await.unwrap().error.is_none());
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
            tokio::spawn(async move { service.poll("a").await })
        };
        wait_for(Duration::from_secs(5), || {
            (usage_requests(&up) == 2).then_some(())
        })
        .await;
        let generation = store.bump_lifecycle("a", StateCause::Operator).unwrap();
        service.invalidate("a");
        store
            .commit_credentials(
                "a",
                1,
                generation,
                None,
                None,
                &NewBundle {
                    access_token: Secret::new("sk-ant-oat01-PLACEHOLDER"),
                    refresh_token: Some(Secret::new("rt")),
                    issued_at: 1_000_000,
                    expires_at: 1_000_000 + 86_400_000,
                    refresh_expires_at: None,
                    scopes: "user:profile user:inference".into(),
                },
            )
            .unwrap();
        store
            .set_state("a", AccountState::Ready, StateCause::Operator)
            .unwrap();
        assert!(service.profile("a").await.is_err(), "the hold is armed");
        up.release();
        let answer = fetching.await.unwrap().unwrap();
        assert_eq!(
            answer.error.as_deref(),
            Some("the profile endpoint rate-limited the account"),
            "the discarded fetch answers the held snapshot"
        );
        up.shutdown().await;
    }

    /// A holder deleted and created again under its id starts over at lifecycle generation 1, so only its new
    /// incarnation tells it from the holder whose fetch is still out: that answer is the old credential's, and
    /// the replacement's poll joined to it must not take it for its own.
    #[tokio::test]
    async fn a_fetch_out_across_a_delete_and_a_new_holder_of_the_id_is_discarded() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![Rule {
            path_prefix: "/api/oauth/usage".into(),
            respond: Respond::Gated(Box::new(Respond::Json {
                status: 200,
                headers: vec![],
                body: control_response(ControlFixture::Usage).to_vec(),
            })),
        }])
        .await;
        let store = store_with_bundle_until(1_000_000 + 86_400_000);
        let service = Arc::new(service_for(&up, store.clone(), &now));
        let old = {
            let service = service.clone();
            tokio::spawn(async move { service.poll("a").await })
        };
        wait_for(Duration::from_secs(5), || {
            (usage_requests(&up) == 1).then_some(())
        })
        .await;
        store.delete_account("a", "test").unwrap();
        service.invalidate("a");
        store
            .insert_account(&AccountRow {
                id: "a".into(),
                kind: AccountKind::Oauth,
                label: "a".into(),
                enabled: true,
                identity: None,
                lifecycle_generation: 1,
                incarnation: 1,
                revision: 1,
                state: AccountState::Ready,
                state_cause: StateCause::Operator,
            })
            .unwrap();
        let replacement = store.account("a").unwrap().unwrap();
        assert_eq!(
            (replacement.incarnation, replacement.lifecycle_generation),
            (2, 1),
            "the same generation under a new incarnation"
        );
        let mut joined = std::pin::pin!(service.poll("a"));
        assert!(
            futures::poll!(joined.as_mut()).is_pending(),
            "the replacement's poll joins the fetch still out"
        );
        up.release();
        assert!(
            joined.await.is_err(),
            "the replacement's poll answers no reading of the old holder"
        );
        assert!(old.await.unwrap().is_err());
        assert!(
            service.latest("a").is_none(),
            "the cache holds no snapshot of the old holder"
        );
        assert_eq!(usage_requests(&up), 1);
        up.shutdown().await;
    }

    /// A delete and a new holder under the id that no route invalidated leaves the old holder's entry in the
    /// cache: neither the latest reading nor a fetch that ended under the new identity may answer it.
    #[tokio::test]
    async fn an_entry_of_an_earlier_holder_of_the_id_reads_as_absent() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let usage = || Respond::Json {
            status: 200,
            headers: vec![],
            body: control_response(ControlFixture::Usage).to_vec(),
        };
        let up = FakeUpstream::start(vec![Rule {
            path_prefix: "/api/oauth/usage".into(),
            respond: usage(),
        }])
        .await;
        let store = store_with_bundle_until(1_000_000 + 86_400_000);
        let service = Arc::new(service_for(&up, store.clone(), &now));
        service.poll("a").await.unwrap();
        assert!(
            service.latest("a").is_some(),
            "the holder's own reading is cached"
        );

        // The next fetch is held out while the row changes hands; no route invalidates the entry.
        up.set_rule_after(1, "/api/oauth/usage", Respond::Gated(Box::new(usage())));
        let old = {
            let service = service.clone();
            tokio::spawn(async move { service.poll("a").await })
        };
        wait_for(Duration::from_secs(5), || {
            (usage_requests(&up) == 2).then_some(())
        })
        .await;
        store.delete_account("a", "test").unwrap();
        store
            .insert_account(&AccountRow {
                id: "a".into(),
                kind: AccountKind::Oauth,
                label: "a".into(),
                enabled: true,
                identity: None,
                lifecycle_generation: 1,
                incarnation: 1,
                revision: 1,
                state: AccountState::Ready,
                state_cause: StateCause::Operator,
            })
            .unwrap();
        assert!(
            service.latest("a").is_none(),
            "the cached reading belongs to the earlier holder and reads as absent"
        );
        up.release();
        assert!(
            old.await.unwrap().is_err(),
            "a fetch that ended under another identity answers no reading, cached or fetched"
        );
        assert!(service.latest("a").is_none());
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
        let service = service_for(&up, store_with_valid_bundle(), &now);

        let first = service.poll("a").await.unwrap();
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
        // A minute on, so the failed attempt's instant differs from the reading's.
        now.store(1_061_000, Ordering::SeqCst);
        let second = service.poll("a").await.unwrap();
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
        let usage = service.poll("business").await.unwrap();
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
