mod common;

use phoebis_oauth::{parse_usage, ParsedUsage, SpendReading};
use phoebis_profile::fixtures::{control_response, ControlFixture};

#[test]
#[allow(clippy::bool_assert_comparison)]
fn the_synthetic_usage_body_parses_into_windows() {
    let ParsedUsage {
        windows,
        fable_window: fable,
        extra_usage: extra,
        spend,
        raw_keys: raw,
        ..
    } = parse_usage(control_response(ControlFixture::Usage)).unwrap();
    let five = windows.iter().find(|w| w.key == "five_hour").unwrap();
    assert_eq!(five.utilization, Some(12.5));
    assert_eq!(five.resets_at, Some(1_789_311_600_000));
    assert!(fable.is_none());
    assert_eq!(extra.unwrap().is_enabled, false);
    assert_eq!(
        spend, None,
        "no spend object and no amounts to build one from"
    );
    assert!(raw.contains(&"seven_day".to_string()));
}

#[test]
fn the_fable_window_comes_from_limits_regardless_of_the_named_key() {
    let body = br#"{"five_hour":{"utilization":0.1,"resets_at":"2026-09-13T15:00:00+00:00"},"cinder_cove":{"utilization":0.9,"resets_at":null},"limits":[{"kind":"weekly_scoped","group":"weekly","percent":42,"resets_at":"2026-09-18T00:00:00+00:00","scope":{"model":{"display_name":"Fable"}}}],"extra_usage":{"is_enabled":true,"monthly_limit":5000,"used_credits":1234,"utilization":0.2468,"currency":"USD"}}"#;
    let ParsedUsage {
        windows,
        fable_window: fable,
        extra_usage: extra,
        spend,
        ..
    } = parse_usage(body).unwrap();
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
    assert_eq!(
        spend, None,
        "no spend object, and amounts without their exponent build none"
    );
}

/// The Fable window's key and figure beside the keys of the other windows, in the body's order.
fn fable_and_window_keys(body: &[u8]) -> (Option<(String, Option<f64>)>, Vec<String>) {
    let parsed = parse_usage(body).unwrap();
    (
        parsed
            .fable_window
            .map(|window| (window.key, window.utilization)),
        parsed
            .windows
            .into_iter()
            .map(|window| window.key)
            .collect(),
    )
}

#[test]
fn a_body_with_no_limits_reads_the_overage_included_week_as_the_fable_week() {
    let body = br#"{"five_hour":{"utilization":10,"resets_at":null},"seven_day_overage_included":{"utilization":37,"resets_at":null}}"#;
    assert_eq!(
        fable_and_window_keys(body),
        (
            Some(("fable".to_string(), Some(37.0))),
            vec!["five_hour".to_string()]
        ),
        "the overage-included week is a spelling of the Fable week, so it stands under no raw key"
    );
}

#[test]
fn a_limits_list_with_no_fable_entry_still_reads_the_overage_included_week() {
    let body = br#"{"five_hour":{"utilization":10,"resets_at":null},"seven_day_overage_included":{"utilization":37,"resets_at":null},"limits":[{"kind":"weekly_scoped","percent":15,"scope":{"model":{"display_name":"Opus"}}}]}"#;
    assert_eq!(
        fable_and_window_keys(body),
        (
            Some(("fable".to_string(), Some(37.0))),
            vec!["five_hour".to_string()]
        ),
        "a limits list that names no Fable entry leaves the top-level spelling to stand as the Fable week"
    );
}

#[test]
fn a_fable_entry_of_limits_stands_over_the_overage_included_week() {
    let body = br#"{"five_hour":{"utilization":10,"resets_at":null},"seven_day_overage_included":{"utilization":37,"resets_at":null},"limits":[{"kind":"weekly_scoped","percent":42,"scope":{"model":{"display_name":"Fable"}}}]}"#;
    assert_eq!(
        fable_and_window_keys(body),
        (
            Some(("fable".to_string(), Some(42.0))),
            vec!["five_hour".to_string()]
        ),
        "the entry of limits is the Fable week, and the same week under its upstream name leaves the windows"
    );
}

#[test]
fn the_first_spelling_in_the_lists_order_is_the_fable_week_and_the_other_leaves() {
    let body = br#"{"five_hour":{"utilization":10,"resets_at":null},"iguana_necktie":{"utilization":37,"resets_at":null},"cinder_cove":{"utilization":21,"resets_at":null}}"#;
    assert_eq!(
        fable_and_window_keys(body),
        (
            Some(("fable".to_string(), Some(21.0))),
            vec!["five_hour".to_string()]
        ),
        "cinder_cove comes first in the list of spellings whatever the body's order, and iguana_necktie leaves"
    );
}

#[test]
fn the_fable_weeks_share_is_the_one_under_its_limit_index() {
    let body = br#"{"limits":[{"kind":"weekly_scoped","percent":10,"scope":{"model":{"display_name":"Opus"}}},{"kind":"weekly_scoped","percent":46,"scope":{"model":{"display_name":"Fable"}}}],"weekly_scoped_shares":[{"limit_index":0,"used_percent_of_weekly":7,"allowance_percent_of_weekly":50},{"limit_index":1,"used_percent_of_weekly":23,"allowance_percent_of_weekly":50}]}"#;
    let parsed = parse_usage(body).unwrap();
    assert_eq!(
        (
            parsed.fable_week_share,
            parsed.fable_window.and_then(|window| window.utilization)
        ),
        (Some(0.23), Some(46.0)),
        "the Fable week is the second limit, so its share is the one under limit_index 1"
    );
}

#[test]
fn the_seat_fixture_parses_into_no_window_and_a_spend_reading() {
    let ParsedUsage {
        windows,
        fable_window: fable,
        extra_usage: extra,
        spend,
        raw_keys: raw,
        ..
    } = parse_usage(include_bytes!("fixtures/usage_based_seat.json")).unwrap();
    assert!(windows.is_empty(), "every window key is null");
    assert!(fable.is_none(), "limits is empty and the aliases are null");
    let extra = extra.expect("the body carries extra usage");
    assert!(extra.is_enabled);
    assert_eq!(
        (extra.used_credits, extra.monthly_limit),
        (Some(102_500), Some(250_000))
    );
    assert_eq!(extra.currency.as_deref(), Some("USD"));
    assert_eq!(extra.decimal_places, Some(2));
    assert_eq!(
        spend,
        Some(SpendReading {
            used_minor: 102_500,
            limit_minor: Some(250_000),
            currency: "USD".into(),
            exponent: 2,
            percent: Some(41.0),
            from_spend: true,
        })
    );
    assert!(raw.contains(&"spend".to_string()));
}

#[test]
fn a_null_spend_limit_reads_as_no_limit() {
    let body = br#"{"spend":{"used":{"amount_minor":4200,"currency":"USD","exponent":2},"limit":null,"percent":null}}"#;
    let spend = parse_usage(body)
        .unwrap()
        .spend
        .expect("the spend object gives a reading");
    assert_eq!(spend.used_minor, 4_200);
    assert_eq!(spend.limit_minor, None, "the seat spends without a cap");
    assert!(spend.from_spend);
}

#[test]
fn a_body_without_a_spend_object_builds_the_reading_from_the_extra_usage_amounts() {
    let body = br#"{"extra_usage":{"is_enabled":true,"monthly_limit":50000,"used_credits":12500,"utilization":25.0,"currency":"EUR","decimal_places":2}}"#;
    assert_eq!(
        parse_usage(body).unwrap().spend,
        Some(SpendReading {
            used_minor: 12_500,
            limit_minor: Some(50_000),
            currency: "EUR".into(),
            exponent: 2,
            percent: Some(25.0),
            from_spend: false,
        }),
        "the extra-usage amounts build the reading, marked as not the object's"
    );
}

#[test]
fn a_spend_object_without_a_currency_or_a_whole_exponent_falls_back_to_the_extra_usage_amounts() {
    let extra = r#""extra_usage":{"is_enabled":true,"monthly_limit":50000,"used_credits":12500,"utilization":25.0,"currency":"EUR","decimal_places":2}"#;
    let fallback = Some(SpendReading {
        used_minor: 12_500,
        limit_minor: Some(50_000),
        currency: "EUR".into(),
        exponent: 2,
        percent: Some(25.0),
        from_spend: false,
    });
    for used in [
        r#"{"amount_minor":4200,"exponent":2}"#,
        r#"{"amount_minor":4200,"currency":"USD"}"#,
        r#"{"amount_minor":4200,"currency":"USD","exponent":300}"#,
    ] {
        let body = format!(r#"{{"spend":{{"used":{used},"percent":8}},{extra}}}"#);
        assert_eq!(
            parse_usage(body.as_bytes()).unwrap().spend,
            fallback,
            "a spend object whose used is {used} reads as no spend object"
        );
    }
}

#[test]
fn extra_usage_amounts_with_a_fraction_part_round_to_the_nearest_minor_unit() {
    let body = br#"{"extra_usage":{"is_enabled":true,"monthly_limit":2499.4,"used_credits":102500.6,"utilization":41.0,"currency":"USD"}}"#;
    let extra = parse_usage(body)
        .expect("an amount with a fraction part parses")
        .extra_usage
        .expect("the body carries extra usage");
    assert_eq!(extra.used_credits, Some(102_501));
    assert_eq!(extra.monthly_limit, Some(2_499));
}

#[test]
fn a_parse_failure_carries_serdes_message() {
    let body = br#"{"extra_usage":{"is_enabled":true,"monthly_limit":250000,"used_credits":"x","utilization":41.0,"currency":"USD"}}"#;
    let message = parse_usage(body).unwrap_err().to_string();
    assert!(
        message.contains(r#""x""#),
        "serde's message quotes the refused value: {message}"
    );
}

#[test]
fn a_null_alias_key_without_limits_yields_no_fable_window() {
    let parsed = parse_usage(br#"{"five_hour":null,"cinder_cove":null}"#).unwrap();
    assert!(parsed.windows.is_empty(), "a null key is no window");
    assert!(
        parsed.fable_window.is_none(),
        "the alias fallback finds nothing to take"
    );
}

mod service {
    use crate::common::wall;
    use fake_upstream::{wait_for, FakeUpstream, RecordedRequest, Respond, Rule};
    use phoebis_oauth::{
        grant_states, parse_limit_resets, BackoffEdge, BackoffEndCause, ClaimOutcome, ClaimRefusal,
        Clock, CredentialManager, GrantStatus, LimitResets, PollFault, PollIssue, PollOutcome,
        Refused, UsageError, UsageService,
    };
    use phoebis_profile::{
        claude_code_2_1_310_linux_x64,
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
        Arc, Mutex,
    };
    use std::time::Duration;

    fn store_with_account() -> Arc<Store> {
        let store = Arc::new(Store::open_in_memory(&StorageKey::generate(), wall()).unwrap());
        store
            .insert_account(&AccountRow {
                identity: Some(phoebis_store::Identity {
                    account_uuid: "00000000-0000-4000-8000-000000000001".into(),
                    email: "test@example.invalid".into(),
                    organization_uuid: None,
                }),
                ..AccountRow::for_tests("a", AccountKind::Oauth)
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
        let p = claude_code_2_1_310_linux_x64();
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
        let store = Arc::new(Store::open_in_memory(&StorageKey::generate(), wall()).unwrap());
        store
            .insert_account(&AccountRow {
                identity: Some(phoebis_store::Identity {
                    account_uuid: "00000000-0000-4000-8000-000000000001".into(),
                    email: "test@example.invalid".into(),
                    organization_uuid: None,
                }),
                ..AccountRow::for_tests("business", AccountKind::Oauth)
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
        // Set after the run's two 429s, so the hook sees the run's end alone.
        let edges = Arc::new(Mutex::new(Vec::new()));
        service.set_on_backoff(Arc::new({
            let edges = edges.clone();
            move |edge: BackoffEdge| edges.lock().unwrap().push(edge)
        }));
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
        assert_eq!(
            *edges.lock().unwrap(),
            [BackoffEdge::Ended {
                account_id: "a".into(),
                at: 1_360_000,
                since: 1_000_000,
                attempts: 2,
                resumed_at: 1_360_000,
                cause: BackoffEndCause::CleanPoll,
            }],
            "the run's end reaches the hook, from its first 429 to the clean answer, which it names"
        );

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
    async fn a_plan_ended_pause_closes_an_open_backoff_run_and_fires_its_end() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![rate_limited_bare()]).await;
        let service = service_for(&up, store_with_bundle_until(1_000_000 + 86_400_000), &now);

        service.poll("a").await.unwrap();
        now.store(1_120_000, Ordering::SeqCst);
        service.poll("a").await.unwrap();
        let edges = Arc::new(Mutex::new(Vec::new()));
        service.set_on_backoff(Arc::new({
            let edges = edges.clone();
            move |edge: BackoffEdge| edges.lock().unwrap().push(edge)
        }));
        now.store(1_200_000, Ordering::SeqCst);
        service.plan_ended("a");
        let status = service.poll_status("a").unwrap();
        assert_eq!(
            (
                status.backoff_since,
                status.rate_limited_in_a_row,
                status.last_error.as_deref()
            ),
            (None, 0, Some("plan ended")),
            "the pause closes the run and names itself"
        );
        assert_eq!(
            *edges.lock().unwrap(),
            [BackoffEdge::Ended {
                account_id: "a".into(),
                at: 1_200_000,
                since: 1_000_000,
                attempts: 2,
                resumed_at: 1_200_000,
                cause: BackoffEndCause::PlanEnded,
            }],
            "the run's end reaches the hook, from its first 429 to the pause, which it names"
        );

        service.plan_ended("a");
        assert_eq!(
            edges.lock().unwrap().len(),
            1,
            "a flagged account with no open run fires no edge"
        );
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
        let store = store_with_bundle_until(1_000_000 + 86_400_000);
        let service = service_for(&up, store.clone(), &now);
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
        let latest = service
            .latest_for(&store.account("a").unwrap().unwrap())
            .unwrap();
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
        assert!(service
            .latest_for(&store.account("a").unwrap().unwrap())
            .unwrap()
            .error
            .is_none());
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
    async fn a_failed_poll_keeps_the_previous_spend_reading_and_its_instant() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![Rule {
            path_prefix: "/api/oauth/usage".into(),
            respond: Respond::Json {
                status: 200,
                headers: vec![],
                body: include_bytes!("fixtures/usage_based_seat.json").to_vec(),
            },
        }])
        .await;
        let service = service_for(&up, store_with_bundle_until(1_000_000 + 86_400_000), &now);

        let clean = service.poll("a").await.unwrap();
        let reading = clean.spend.expect("the seat fixture gives a reading");
        assert_eq!(clean.fetched_at, 1_000_000);

        up.set_rule_after(0, "/api/oauth/usage", rate_limited_bare().respond);
        now.store(1_002_000, Ordering::SeqCst);
        let limited = service.poll("a").await.unwrap();
        assert_eq!(limited.error.as_deref(), Some("usage request rate-limited"));
        assert_eq!(
            limited.spend.as_ref(),
            Some(&reading),
            "a 429 keeps the reading"
        );
        assert_eq!(limited.fetched_at, 1_000_000, "read when it was read");

        up.set_rule_after(
            0,
            "/api/oauth/usage",
            Respond::Json {
                status: 500,
                headers: vec![],
                body: b"{}".to_vec(),
            },
        );
        // Past the 429's two-minute hold, so the poll reaches the endpoint again.
        now.store(1_125_000, Ordering::SeqCst);
        let failed = service.poll("a").await.unwrap();
        assert_eq!(
            failed.error.as_deref(),
            Some("usage request rejected with status 500")
        );
        assert_eq!(
            failed.spend.as_ref(),
            Some(&reading),
            "a failed fetch keeps the reading"
        );
        assert_eq!(failed.fetched_at, 1_000_000, "read when it was read");
        assert_eq!(usage_requests(&up), 3, "each poll reached the endpoint");
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
            .insert_account(&AccountRow::for_tests("a", AccountKind::Oauth))
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
            service
                .latest_for(&store.account("a").unwrap().unwrap())
                .is_none(),
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
            service
                .latest_for(&store.account("a").unwrap().unwrap())
                .is_some(),
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
            .insert_account(&AccountRow::for_tests("a", AccountKind::Oauth))
            .unwrap();
        assert!(
            service
                .latest_for(&store.account("a").unwrap().unwrap())
                .is_none(),
            "the cached reading belongs to the earlier holder and reads as absent"
        );
        up.release();
        assert!(
            old.await.unwrap().is_err(),
            "a fetch that ended under another identity answers no reading, cached or fetched"
        );
        assert!(service
            .latest_for(&store.account("a").unwrap().unwrap())
            .is_none());
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
        let (service, up, _store) = service_with_fake().await;
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

    /// An answer of `status` with an empty object for its body.
    fn answering(status: u16) -> Respond {
        Respond::Json {
            status,
            headers: vec![],
            body: b"{}".to_vec(),
        }
    }

    /// Commits the credential after the stored one, as a re-pair does, and answers its generation.
    fn commit_next_credential(store: &Store) -> u64 {
        let stored = store.credentials("a").unwrap().unwrap().generation;
        store
            .commit_credentials(
                "a",
                1,
                1,
                Some(stored),
                None,
                &NewBundle {
                    access_token: Secret::new("sk-ant-oat01-NEXT"),
                    refresh_token: Some(Secret::new("rt-next")),
                    issued_at: 1_000_000,
                    expires_at: 1_000_000 + 86_400_000,
                    refresh_expires_at: None,
                    scopes: "user:profile user:inference".into(),
                },
            )
            .unwrap()
    }

    #[tokio::test]
    async fn a_usage_401_answers_the_re_pair_issue_fenced_on_the_tokens_generation_and_the_lineage()
    {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![Rule {
            path_prefix: "/api/oauth/usage".into(),
            respond: answering(401),
        }])
        .await;
        let store = store_with_bundle_until(1_000_000 + 86_400_000);
        let generation = commit_next_credential(&store);
        assert_eq!(
            generation, 2,
            "the token the poll acquires is the second credential"
        );
        let service = service_for(&up, store, &now);
        let (snapshot, issue) = service.poll_reporting("a").await.unwrap();
        assert_eq!(
            issue,
            Some(PollIssue::Unauthorized(Refused {
                incarnation: 1,
                lifecycle_generation: 1,
                credential_generation: 2,
            })),
            "the 401 names the credential it refused under the lineage the fetch started on"
        );
        assert_eq!(
            snapshot.error.as_deref(),
            Some("usage request rejected with status 401"),
            "the snapshot keeps the text every refused status writes"
        );
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_usage_403_a_500_and_a_transport_failure_answer_a_passing_fault_with_their_status_and_text(
    ) {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![Rule {
            path_prefix: "/api/oauth/usage".into(),
            respond: answering(403),
        }])
        .await;
        let service = service_for(&up, store_with_bundle_until(1_000_000 + 86_400_000), &now);
        let fault = |status: u16| {
            Some(PollIssue::Fault(PollFault {
                endpoint: "usage",
                status: Some(status),
                text: format!("usage request rejected with status {status}"),
            }))
        };
        assert_eq!(service.poll_reporting("a").await.unwrap().1, fault(403));
        up.set_rule_after(0, "/api/oauth/usage", answering(500));
        assert_eq!(service.poll_reporting("a").await.unwrap().1, fault(500));
        up.set_rule_after(0, "/api/oauth/usage", Respond::CloseAfterRequest);
        let (snapshot, issue) = service.poll_reporting("a").await.unwrap();
        let Some(PollIssue::Fault(fault)) = issue else {
            panic!("a transport failure is a passing fault: {issue:?}");
        };
        assert_eq!(
            (fault.endpoint, fault.status, fault.transport()),
            ("usage", None, true)
        );
        assert!(fault.text.starts_with("transport: "), "{}", fault.text);
        assert_eq!(
            snapshot.error.as_deref(),
            Some(fault.text.as_str()),
            "the fault's text is the snapshot's"
        );
        assert_eq!(usage_requests(&up), 3, "no fault holds the account back");
        up.shutdown().await;
    }

    /// The usage endpoint's 403 for an organization that holds no plan, as the live read of an ended plan answered it.
    const PLAN_REFUSED_BODY: &[u8] = br#"{"type":"error","error":{"type":"permission_error","message":"OAuth authentication is currently not allowed for this organization.","details":{"error_code":"oauth_not_allowed_for_organization","error_visibility":"user_facing"}}}"#;

    #[tokio::test]
    async fn a_usage_403_naming_the_organizations_refusal_answers_its_own_issue_and_any_other_403_a_fault(
    ) {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![Rule {
            path_prefix: "/api/oauth/usage".into(),
            respond: Respond::Json {
                status: 403,
                headers: vec![("x-should-retry".into(), "false".into())],
                body: PLAN_REFUSED_BODY.to_vec(),
            },
        }])
        .await;
        let service = service_for(&up, store_with_bundle_until(1_000_000 + 86_400_000), &now);
        let fault = PollFault {
            endpoint: "usage",
            status: Some(403),
            text: "usage request rejected with status 403".into(),
        };
        let (snapshot, issue) = service.poll_reporting("a").await.unwrap();
        assert_eq!(
            issue,
            Some(PollIssue::PlanRefused {
                fault: fault.clone(),
                code: "oauth_not_allowed_for_organization".into(),
            }),
            "the code the organization's refusal names rides the issue"
        );
        assert_eq!(snapshot.error.as_deref(), Some(fault.text.as_str()));
        assert_eq!(
            service.poll_status("a").unwrap().next_attempt_at,
            None,
            "the refusal arms no hold"
        );
        up.set_rule_after(
            0,
            "/api/oauth/usage",
            Respond::Json {
                status: 403,
                headers: vec![],
                body: br#"{"type":"error","error":{"type":"permission_error","message":"no"}}"#
                    .to_vec(),
            },
        );
        assert_eq!(
            service.poll_reporting("a").await.unwrap().1,
            Some(PollIssue::Fault(fault)),
            "a 403 without the code is a passing fault"
        );
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_caller_joined_to_another_callers_fetch_reads_its_snapshot_and_no_issue() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![Rule {
            path_prefix: "/api/oauth/usage".into(),
            respond: Respond::Gated(Box::new(answering(500))),
        }])
        .await;
        let service = Arc::new(service_for(
            &up,
            store_with_bundle_until(1_000_000 + 86_400_000),
            &now,
        ));
        let starting = {
            let service = service.clone();
            tokio::spawn(async move { service.poll_reporting("a").await })
        };
        wait_for(Duration::from_secs(5), || {
            (usage_requests(&up) == 1).then_some(())
        })
        .await;
        let mut joined = std::pin::pin!(service.poll_reporting("a"));
        assert!(
            futures::poll!(joined.as_mut()).is_pending(),
            "the second caller joins the fetch still out"
        );
        up.release();
        let (joined_snapshot, joined_issue) = joined.await.unwrap();
        let (started_snapshot, started_issue) = starting.await.unwrap().unwrap();
        assert!(
            matches!(started_issue, Some(PollIssue::Fault(_))),
            "the caller that started the fetch reads its fault: {started_issue:?}"
        );
        assert_eq!(joined_issue, None, "one fetch reports once");
        assert_eq!(joined_snapshot.error, started_snapshot.error);
        assert_eq!(usage_requests(&up), 1);
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_usage_429_answers_no_issue_and_keeps_the_ladder() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![rate_limited_bare()]).await;
        let service = service_for(&up, store_with_bundle_until(1_000_000 + 86_400_000), &now);
        let (snapshot, issue) = service.poll_reporting("a").await.unwrap();
        assert_eq!(issue, None, "a 429 is the ladder's, never a fault");
        assert_eq!(
            snapshot.error.as_deref(),
            Some("usage request rate-limited")
        );
        let status = service.poll_status("a").unwrap();
        assert_eq!(
            (
                status.last_outcome,
                status.rate_limited_in_a_row,
                status.next_attempt_at
            ),
            (PollOutcome::RateLimited, 1, Some(1_000_000 + 120_000)),
            "the ladder's first step"
        );
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_profile_500_and_a_profile_transport_failure_answer_the_fault_with_the_refreshs_text()
    {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![Rule {
            path_prefix: "/api/oauth/profile".into(),
            respond: answering(500),
        }])
        .await;
        let service = service_for(&up, store_with_bundle_until(1_000_000 + 86_400_000), &now);
        let refused = service.profile("a").await.unwrap_err();
        assert_eq!(
            refused.to_string(),
            "profile request rejected with status 500"
        );
        let UsageError::Fault(fault) = refused else {
            panic!("a profile 500 is a passing fault: {refused:?}");
        };
        assert_eq!(
            fault,
            PollFault {
                endpoint: "profile",
                status: Some(500),
                text: "profile request rejected with status 500".into(),
            }
        );
        up.set_rule_after(0, "/api/oauth/profile", Respond::CloseAfterRequest);
        let lost = service.profile("a").await.unwrap_err();
        let text = lost.to_string();
        assert!(text.starts_with("transport: "), "{text}");
        let UsageError::Fault(fault) = lost else {
            panic!("a profile transport failure is a passing fault: {lost:?}");
        };
        assert_eq!(
            (fault.endpoint, fault.status, fault.transport(), fault.text),
            ("profile", None, true, text)
        );
        up.shutdown().await;
    }

    #[tokio::test]
    async fn the_re_pair_mark_of_a_replaced_credential_lands_nothing_and_the_current_one_lands_auth_failed(
    ) {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![]).await;
        let store = store_with_bundle_until(1_000_000 + 86_400_000);
        let current = commit_next_credential(&store);
        let service = service_for(&up, store.clone(), &now);
        let fence = |credential_generation: u64| Refused {
            incarnation: 1,
            lifecycle_generation: 1,
            credential_generation,
        };
        assert_eq!(
            service.mark_auth_failed("a", fence(current - 1)),
            None,
            "a 401 on the credential the re-pair replaced lands nothing"
        );
        let row = store.account("a").unwrap().unwrap();
        assert_eq!(row.state, AccountState::Ready);
        let mark = service
            .mark_auth_failed("a", fence(current))
            .expect("a 401 on the current credential lands the mark");
        assert_eq!(
            (mark.state, mark.cause, mark.credential_generation),
            (
                AccountState::ReauthRequired,
                StateCause::AuthFailed,
                current
            )
        );
        let row = store.account("a").unwrap().unwrap();
        assert_eq!(
            (row.state, row.state_cause),
            (AccountState::ReauthRequired, StateCause::AuthFailed)
        );
        up.shutdown().await;
    }

    /// A usage body in the client's shape: the limit reset block with one grant, and the spend members null.
    const BLOCK: &[u8] = br#"{"five_hour":{"utilization":12.5,"resets_at":null},"extra_usage":null,"spend":null,"cedar_ember":{"eligible":true,"grants":[{"id":"rg-a-1","label":"Launch reset","clears":["seven_day_overage_included"],"gives_back_weekly":true,"starts_at":"2026-01-01T00:00:00Z","ends_at":"2027-01-01T00:00:00Z","resets_left":1,"resets_total":1,"usable_now":true}]}}"#;

    /// A usage body on the plain path, with the spend and the extra usage the block's body leaves null.
    const USAGE_BODY: &[u8] = br#"{"five_hour":{"utilization":12.5,"resets_at":null},"extra_usage":{"is_enabled":true,"monthly_limit":5000,"used_credits":1234,"utilization":24.68,"currency":"USD","decimal_places":2},"spend":{"used":{"amount_minor":250,"currency":"USD","exponent":2},"limit":{"amount_minor":5000},"percent":5}}"#;

    /// The request lines of the two reads, each with the space after its target, since the plain target is a prefix
    /// of the client's shape.
    const USAGE_LINE: &str = "GET /api/oauth/usage ";
    const LIMIT_RESETS_LINE: &str = "GET /api/oauth/usage?cedar_ember=1&skip_spend=1 ";

    /// A rule for the client's shape, listed before the usage rule, so the fake answers the block to that shape
    /// alone, as the upstream fills it.
    fn limit_resets_rule(respond: Respond) -> Rule {
        Rule {
            path_prefix: "/api/oauth/usage?cedar_ember=1&skip_spend=1".into(),
            respond,
        }
    }

    fn usage_body_rule() -> Rule {
        Rule {
            path_prefix: "/api/oauth/usage".into(),
            respond: answering_with(200, USAGE_BODY),
        }
    }

    fn answering_with(status: u16, body: &[u8]) -> Respond {
        Respond::Json {
            status,
            headers: vec![],
            body: body.to_vec(),
        }
    }

    fn requests_on(up: &FakeUpstream, line: &str) -> usize {
        up.requests()
            .iter()
            .filter(|r| r.line.starts_with(line))
            .count()
    }

    #[tokio::test]
    async fn a_limit_resets_read_leaves_the_snapshot_and_stores_the_block() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![
            limit_resets_rule(answering_with(200, BLOCK)),
            usage_body_rule(),
        ])
        .await;
        let store = store_with_valid_bundle();
        let service = service_for(&up, store.clone(), &now);
        service.poll("a").await.unwrap();
        let block = service.limit_resets("a").await.unwrap();
        let row = store.account("a").unwrap().unwrap();
        let snapshot = service.latest_for(&row).unwrap();
        assert_eq!(
            (
                snapshot.spend.map(|spend| spend.used_minor),
                snapshot.extra_usage.and_then(|extra| extra.used_credits)
            ),
            (Some(250), Some(1234)),
            "the block's body, whose spend members read null, leaves the regular poll's snapshot alone"
        );
        let block = block.expect("the client's shape carries the block");
        assert_eq!(block.grants.len(), 1);
        assert_eq!(
            (block.grants[0].id.as_str(), block.grants[0].clears.clone()),
            ("rg-a-1", vec!["seven_day_fable".to_string()])
        );
        let stored: serde_json::Value = serde_json::from_str(
            &store
                .account_extras_of("a")
                .unwrap()
                .unwrap()
                .limit_resets
                .expect("the row holds the block"),
        )
        .unwrap();
        assert_eq!(stored["grants"][0]["id"], "rg-a-1");
        up.shutdown().await;
    }

    #[tokio::test]
    async fn the_hold_stops_a_limit_resets_read() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![
            limit_resets_rule(answering_with(200, BLOCK)),
            rate_limited("120"),
        ])
        .await;
        let service = service_for(&up, store_with_valid_bundle(), &now);
        service.poll("a").await.unwrap();
        let held = service.limit_resets("a").await;
        assert_eq!(
            (held.is_err(), requests_on(&up, LIMIT_RESETS_LINE)),
            (true, 0),
            "the usage 429's hold refuses the read before it sends"
        );
        now.store(1_000_000 + 120_000, Ordering::SeqCst);
        let freed = service.limit_resets("a").await;
        assert_eq!(
            (freed.is_ok(), requests_on(&up, LIMIT_RESETS_LINE)),
            (true, 1),
            "the read goes out once the hold ends"
        );
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_rate_limited_limit_resets_read_holds_the_usage_poll() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![
            limit_resets_rule(Respond::Json {
                status: 429,
                headers: vec![("Retry-After".to_string(), "120".to_string())],
                body: b"{}".to_vec(),
            }),
            usage_body_rule(),
        ])
        .await;
        let service = service_for(&up, store_with_valid_bundle(), &now);
        assert!(service.limit_resets("a").await.is_err());
        let snapshot = service.poll("a").await.unwrap();
        assert_eq!(
            (snapshot.error.is_some(), requests_on(&up, USAGE_LINE)),
            (true, 0),
            "the block read's 429 holds the usage poll, which answers the held snapshot"
        );
        up.shutdown().await;
    }

    #[tokio::test]
    async fn two_callers_share_one_limit_resets_read() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![
            limit_resets_rule(Respond::Gated(Box::new(answering_with(200, BLOCK)))),
            usage_body_rule(),
        ])
        .await;
        let service = Arc::new(service_for(&up, store_with_valid_bundle(), &now));
        let read = || {
            let service = service.clone();
            tokio::spawn(async move { service.limit_resets("a").await })
        };
        let (first, second) = (read(), read());
        wait_for(Duration::from_secs(5), || {
            (requests_on(&up, LIMIT_RESETS_LINE) >= 1).then_some(())
        })
        .await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        up.release();
        let first = first
            .await
            .unwrap()
            .unwrap()
            .expect("the first caller reads the block");
        let second = second
            .await
            .unwrap()
            .unwrap()
            .expect("the joined caller reads the block");
        assert_eq!(
            (first.grants[0].id.as_str(), second.grants[0].id.as_str()),
            ("rg-a-1", "rg-a-1")
        );
        assert_eq!(
            requests_on(&up, LIMIT_RESETS_LINE),
            1,
            "the second caller joins the read still out"
        );
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_limit_resets_read_that_was_out_does_not_end_a_due_mark() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![
            limit_resets_rule(Respond::Gated(Box::new(answering_with(200, BLOCK)))),
            usage_body_rule(),
        ])
        .await;
        let store = store_with_valid_bundle();
        let service = Arc::new(service_for(&up, store.clone(), &now));
        let reading = {
            let service = service.clone();
            tokio::spawn(async move { service.limit_resets("a").await })
        };
        wait_for(Duration::from_secs(5), || {
            (requests_on(&up, LIMIT_RESETS_LINE) == 1).then_some(())
        })
        .await;
        let row = store.account("a").unwrap().unwrap();
        service.mark_limit_resets_due(&row);
        up.release();
        let block = reading
            .await
            .unwrap()
            .unwrap()
            .expect("the held read answers the block");
        assert_eq!(block.grants[0].id, "rg-a-1");
        assert!(
            service.limit_resets_memory(&row).due,
            "a mark set while the read was out stands for the next slot"
        );
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_limit_resets_read_carries_the_usage_reads_head() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![
            limit_resets_rule(answering_with(200, BLOCK)),
            usage_body_rule(),
        ])
        .await;
        let service = service_for(&up, store_with_valid_bundle(), &now);
        service.poll("a").await.unwrap();
        service.limit_resets("a").await.unwrap();
        let requests = up.requests();
        let heads = |line: &str| {
            requests
                .iter()
                .filter(|r| r.line.starts_with(line))
                .map(|r| r.headers.clone())
                .collect::<Vec<_>>()
        };
        let (usage, limit_resets) = (heads(USAGE_LINE), heads(LIMIT_RESETS_LINE));
        assert_eq!((usage.len(), limit_resets.len()), (1, 1));
        assert_eq!(
            limit_resets[0], usage[0],
            "the block read sends the usage read's head: the same names in the same order with the same values"
        );
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_limit_resets_read_the_store_refuses_stays_due() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![
            limit_resets_rule(Respond::Gated(Box::new(answering_with(200, BLOCK)))),
            usage_body_rule(),
        ])
        .await;
        let store = store_with_valid_bundle();
        // A stored block for the fold below to rewrite, at the epoch 0 the read takes at its start.
        let no_reset =
            r#"{"cooldown_until":null,"eligible":false,"grants":[],"ineligible_reason":"tier"}"#;
        assert!(store
            .set_limit_resets("a", 1, 1, 0, Some(no_reset), 1_000_000)
            .unwrap());
        let service = Arc::new(service_for(&up, store.clone(), &now));
        let reading = {
            let service = service.clone();
            tokio::spawn(async move { service.limit_resets("a").await })
        };
        wait_for(Duration::from_secs(5), || {
            (requests_on(&up, LIMIT_RESETS_LINE) == 1).then_some(())
        })
        .await;
        // A claim's fold lands while the read is out and moves the epoch, so the read's answer writes nothing.
        assert!(store
            .fold_limit_resets("a", 1, 1, 0, |block| Some(block.to_string()))
            .unwrap());
        up.release();
        reading.await.unwrap().unwrap();
        let row = store.account("a").unwrap().unwrap();
        assert!(
            service.limit_resets_memory(&row).due,
            "a read that landed nothing stays due for the next slot"
        );
        up.shutdown().await;
    }

    /// The organization id `a`'s identity holds in the claims' rigs, invented.
    const ORGANIZATION: &str = "00000000-0000-4000-8000-0000000000aa";

    /// The prefix of every claim's target.
    const CLAIM_PREFIX: &str = "/api/organizations/";

    /// A block read in the client's shape whose grant `rg-a-1` holds two resets and waits for a limit.
    const BLOCKED_BODY: &[u8] = br#"{"cedar_ember":{"eligible":true,"grants":[{"id":"rg-a-1","label":"Launch reset","clears":["seven_day_overage_included"],"gives_back_weekly":true,"starts_at":"2026-01-01T00:00:00Z","ends_at":"2027-01-01T00:00:00Z","resets_left":2,"resets_total":2,"usable_now":false,"unavailable_reason":"needs_limit"}]}}"#;

    /// `BLOCKED_BODY` with the grant usable.
    const USABLE_BODY: &[u8] = br#"{"cedar_ember":{"eligible":true,"grants":[{"id":"rg-a-1","label":"Launch reset","clears":["seven_day_overage_included"],"gives_back_weekly":true,"starts_at":"2026-01-01T00:00:00Z","ends_at":"2027-01-01T00:00:00Z","resets_left":2,"resets_total":2,"usable_now":true}]}}"#;

    /// The client's shape of the usage read, the prefix of its rule.
    const LIMIT_RESETS_PREFIX: &str = "/api/oauth/usage?cedar_ember=1&skip_spend=1";

    /// `store_with_valid_bundle` with `organization` on `a`'s identity and `block` stored on its row.
    fn store_for_claims(organization: &str, block: &LimitResets) -> Arc<Store> {
        let store = store_with_valid_bundle();
        prepare_claims(&store, organization, block);
        store
    }

    /// Puts `organization` on `a`'s identity and stores `block` on its row.
    fn prepare_claims(store: &Store, organization: &str, block: &LimitResets) {
        store
            .set_identity(
                "a",
                &phoebis_store::Identity {
                    account_uuid: "00000000-0000-4000-8000-000000000001".into(),
                    email: "test@example.invalid".into(),
                    organization_uuid: Some(organization.into()),
                },
            )
            .unwrap();
        store_block(store, 1, block);
    }

    /// The typed block the parser answers for `BLOCK`, its grant `rg-a-1` at `left` resets of `total`.
    fn block_with(left: u32, total: u32) -> LimitResets {
        let mut block = parse_limit_resets(BLOCK).unwrap().unwrap();
        block.grants[0].resets_left = left;
        block.grants[0].resets_total = total;
        block
    }

    /// Writes `block` on `a`'s row under the holder of `incarnation`, at the epoch of a row no fold moved, as a read
    /// at the service's clock writes it.
    fn store_block(store: &Store, incarnation: u64, block: &LimitResets) {
        let text = serde_json::to_string(block).unwrap();
        assert!(store
            .set_limit_resets("a", incarnation, 1, 0, Some(&text), 1_000_000)
            .unwrap());
    }

    fn claim_rule(respond: Respond) -> Rule {
        Rule {
            path_prefix: CLAIM_PREFIX.into(),
            respond,
        }
    }

    /// Answers every claim from the next one on with `respond`.
    fn answer_claims(up: &FakeUpstream, respond: Respond) {
        up.set_rule_after(0, CLAIM_PREFIX, respond);
    }

    fn claims_sent(up: &FakeUpstream) -> Vec<RecordedRequest> {
        up.requests()
            .into_iter()
            .filter(|r| r.line.starts_with("POST /api/organizations/"))
            .collect()
    }

    /// The `request_id` of each claim the fake read, in order.
    fn request_ids(up: &FakeUpstream) -> Vec<String> {
        claims_sent(up)
            .iter()
            .map(|claim| {
                let body: serde_json::Value = serde_json::from_slice(&claim.body).unwrap();
                body["request_id"].as_str().unwrap().to_string()
            })
            .collect()
    }

    /// The block `a`'s row holds.
    fn stored_block(store: &Store) -> LimitResets {
        let stored = store
            .account_extras_of("a")
            .unwrap()
            .unwrap()
            .limit_resets
            .expect("the row holds a block");
        serde_json::from_str(&stored).unwrap()
    }

    /// Each grant's state at the service's clock and its count, in the block `a`'s row holds.
    fn stored_grants(store: &Store) -> (Vec<GrantStatus>, Vec<u32>) {
        let block = stored_block(store);
        let states = grant_states(&block, 1_000_000)
            .into_iter()
            .map(|grant| grant.state)
            .collect();
        (
            states,
            block.grants.iter().map(|grant| grant.resets_left).collect(),
        )
    }

    #[tokio::test]
    async fn a_claim_goes_out_as_the_client_sends_it() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![
            usage_body_rule(),
            claim_rule(answering_with(200, br#"{"result":"reset"}"#)),
        ])
        .await;
        let store = store_for_claims(ORGANIZATION, &block_with(3, 3));
        let service = service_for(&up, store.clone(), &now);
        service.poll("a").await.unwrap();
        let row = store.account("a").unwrap().unwrap();
        service
            .claim_limit_reset(&row, "rg-a-1", None)
            .await
            .unwrap();

        let requests = up.requests();
        let usage = requests
            .iter()
            .find(|r| r.line.starts_with(USAGE_LINE))
            .expect("the usage read went out");
        let claims = claims_sent(&up);
        assert_eq!(claims.len(), 1);
        assert_eq!(
            claims[0].line,
            "POST /api/organizations/00000000-0000-4000-8000-0000000000aa/reset_rate_limits HTTP/1.1"
        );
        let names: Vec<&str> = claims[0]
            .headers
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "Accept",
                "Content-Type",
                "User-Agent",
                "Authorization",
                "anthropic-beta",
                "Content-Length",
                "Accept-Encoding",
                "Host",
                "Connection"
            ],
            "the usage read's head with the body's length after the last header the caller sets"
        );
        let without_length: Vec<(String, String)> = claims[0]
            .headers
            .iter()
            .filter(|(name, _)| name != "Content-Length")
            .cloned()
            .collect();
        assert_eq!(
            without_length, usage.headers,
            "every other header is the usage read's, by name and value"
        );
        let id = &request_ids(&up)[0];
        assert_eq!(id.len(), 36, "a UUID's text");
        assert_eq!(
            String::from_utf8(claims[0].body.clone()).unwrap(),
            format!(r#"{{"program":"cedar_ember","grant_id":"rg-a-1","request_id":"{id}"}}"#)
        );
        up.shutdown().await;
    }

    #[tokio::test]
    async fn an_unanswered_claim_repeats_its_request_id() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![claim_rule(Respond::CloseAfterRequest)]).await;
        let store = store_for_claims(ORGANIZATION, &block_with(3, 3));
        let service = service_for(&up, store.clone(), &now);
        let row = store.account("a").unwrap().unwrap();
        service
            .claim_limit_reset(&row, "rg-a-1", None)
            .await
            .unwrap();
        answer_claims(&up, answering_with(200, br#"{"result":"reset"}"#));
        service
            .claim_limit_reset(&row, "rg-a-1", None)
            .await
            .unwrap();
        answer_claims(&up, answering_with(200, br#"{"result":"cooldown"}"#));
        service
            .claim_limit_reset(&row, "rg-a-1", None)
            .await
            .unwrap();
        answer_claims(&up, answering_with(200, br#"{"result":"reset"}"#));
        service
            .claim_limit_reset(&row, "rg-a-1", None)
            .await
            .unwrap();

        let ids = request_ids(&up);
        assert_eq!(ids.len(), 4);
        assert_eq!(
            ids[0], ids[1],
            "the second click repeats the id of a first whose answer never arrived"
        );
        assert_ne!(
            ids[2], ids[3],
            "a cooldown that answers a new id settles the claim, so the next click sends another"
        );
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_claim_that_times_out_is_sent_once() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![claim_rule(Respond::Gated(Box::new(answering_with(
            200,
            br#"{"result":"reset"}"#,
        ))))])
        .await;
        let store = store_for_claims(ORGANIZATION, &block_with(3, 3));
        let service = Arc::new(service_for(&up, store.clone(), &now));
        let row = store.account("a").unwrap().unwrap();
        let claiming = {
            let service = service.clone();
            tokio::spawn(async move { service.claim_limit_reset(&row, "rg-a-1", None).await })
        };
        wait_for(Duration::from_secs(5), || {
            (claims_sent(&up).len() == 1).then_some(())
        })
        .await;
        // The fake holds the whole request, so the bound lapses after the request was written.
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(25)).await;
        tokio::time::resume();
        let claim = claiming.await.unwrap().unwrap();
        assert_eq!(
            (claim.outcome, claim.status()),
            (ClaimOutcome::Unsettled, None)
        );
        assert_eq!(claims_sent(&up).len(), 1, "nothing sends the claim again");
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_kept_request_id_ends_after_ten_minutes() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![claim_rule(Respond::CloseAfterRequest)]).await;
        let store = store_for_claims(ORGANIZATION, &block_with(3, 3));
        let service = service_for(&up, store.clone(), &now);
        let row = store.account("a").unwrap().unwrap();
        service
            .claim_limit_reset(&row, "rg-a-1", None)
            .await
            .unwrap();
        now.store(1_599_999, Ordering::SeqCst);
        service
            .claim_limit_reset(&row, "rg-a-1", None)
            .await
            .unwrap();
        now.store(1_600_000, Ordering::SeqCst);
        service
            .claim_limit_reset(&row, "rg-a-1", None)
            .await
            .unwrap();

        let ids = request_ids(&up);
        assert_eq!(ids.len(), 3);
        assert_eq!(
            ids[0], ids[1],
            "ten minutes less a millisecond after the first outcome"
        );
        assert_ne!(ids[2], ids[0], "ten minutes after it");
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_500_keeps_the_request_id_and_a_403_drops_it() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![claim_rule(answering(500))]).await;
        let store = store_for_claims(ORGANIZATION, &block_with(3, 3));
        let service = service_for(&up, store.clone(), &now);
        let row = store.account("a").unwrap().unwrap();
        let first = service
            .claim_limit_reset(&row, "rg-a-1", None)
            .await
            .unwrap();
        answer_claims(&up, answering(403));
        let second = service
            .claim_limit_reset(&row, "rg-a-1", None)
            .await
            .unwrap();
        answer_claims(&up, answering_with(200, br#"{"result":"reset"}"#));
        service
            .claim_limit_reset(&row, "rg-a-1", None)
            .await
            .unwrap();

        assert_eq!(
            (first.outcome, second.outcome),
            (ClaimOutcome::Unsettled, ClaimOutcome::Refused)
        );
        let ids = request_ids(&up);
        assert_eq!(ids.len(), 3);
        assert_eq!(
            ids[0], ids[1],
            "the upstream may have taken the 500's claim"
        );
        assert_ne!(ids[2], ids[1], "the 403 settles the claim");
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_block_read_drops_the_kept_request_id() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![
            limit_resets_rule(answering_with(200, BLOCKED_BODY)),
            claim_rule(Respond::CloseAfterRequest),
        ])
        .await;
        up.set_rule_after(1, LIMIT_RESETS_PREFIX, answering_with(200, USABLE_BODY));
        let store = store_for_claims(ORGANIZATION, &block_with(2, 2));
        let service = service_for(&up, store.clone(), &now);
        let row = store.account("a").unwrap().unwrap();
        service
            .claim_limit_reset(&row, "rg-a-1", None)
            .await
            .unwrap();
        // One millisecond after the outcome, the first instant a read goes out after the id was kept.
        now.store(1_000_001, Ordering::SeqCst);
        service.limit_resets("a").await.unwrap();
        service.limit_resets("a").await.unwrap();
        service
            .claim_limit_reset(&row, "rg-a-1", None)
            .await
            .unwrap();

        let ids = request_ids(&up);
        assert_eq!(ids.len(), 2);
        assert_ne!(
            ids[0], ids[1],
            "the read that shows the grant blocked with no cooldown settles the first claim"
        );
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_cooldown_after_a_repeat_stays_unsettled() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![claim_rule(Respond::CloseAfterRequest)]).await;
        let store = store_for_claims(ORGANIZATION, &block_with(3, 3));
        let service = service_for(&up, store.clone(), &now);
        let row = store.account("a").unwrap().unwrap();
        service
            .claim_limit_reset(&row, "rg-a-1", None)
            .await
            .unwrap();
        answer_claims(&up, answering_with(200, br#"{"result":"cooldown"}"#));
        let second = service
            .claim_limit_reset(&row, "rg-a-1", None)
            .await
            .unwrap();
        answer_claims(&up, answering_with(200, br#"{"result":"reset"}"#));
        service
            .claim_limit_reset(&row, "rg-a-1", None)
            .await
            .unwrap();

        assert_eq!(
            second.outcome,
            ClaimOutcome::Unsettled,
            "a cooldown after a repeat can be the first attempt's own"
        );
        let ids = request_ids(&up);
        assert_eq!(ids.len(), 3);
        assert_eq!(
            (&ids[1], &ids[2]),
            (&ids[0], &ids[0]),
            "the id stays kept for the third click"
        );
        up.shutdown().await;
    }

    #[tokio::test]
    async fn an_already_used_reset_is_not_offered_again() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![claim_rule(answering_with(
            200,
            br#"{"result":"already_used"}"#,
        ))])
        .await;
        let store = store_for_claims(ORGANIZATION, &block_with(1, 1));
        let service = service_for(&up, store.clone(), &now);
        let row = store.account("a").unwrap().unwrap();
        let claim = service
            .claim_limit_reset(&row, "rg-a-1", None)
            .await
            .unwrap();

        assert_eq!(claim.outcome, ClaimOutcome::Refused);
        assert_eq!(
            stored_grants(&store),
            (vec![GrantStatus::Used], vec![0]),
            "the answer names no count, and its word says the reset is gone"
        );
        up.shutdown().await;
    }

    #[tokio::test]
    async fn an_answer_that_names_a_count_and_a_cooldown_folds_both() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![claim_rule(answering_with(
            200,
            br#"{"result":"reset","resets_left":1,"cooldown_until":"1970-01-01T01:00:00Z"}"#,
        ))])
        .await;
        let store = store_for_claims(ORGANIZATION, &block_with(3, 3));
        let service = service_for(&up, store.clone(), &now);
        let row = store.account("a").unwrap().unwrap();
        let claim = service
            .claim_limit_reset(&row, "rg-a-1", None)
            .await
            .unwrap();

        assert_eq!(claim.outcome, ClaimOutcome::Used);
        let block = stored_block(&store);
        assert_eq!(
            (block.grants[0].resets_left, block.cooldown_until),
            (1, Some(3_600_000)),
            "the count and the cooldown the answer names, 1970-01-01T01:00:00Z being 3 600 000 ms"
        );
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_grant_that_waits_is_not_claimed() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![claim_rule(answering_with(
            200,
            br#"{"result":"reset"}"#,
        ))])
        .await;
        let mut block = block_with(1, 1);
        let mut second = block.grants[0].clone();
        second.id = "rg-a-2".into();
        block.grants.push(second);
        let store = store_for_claims(ORGANIZATION, &block);
        assert_eq!(
            stored_grants(&store).0,
            vec![GrantStatus::Usable, GrantStatus::Waiting],
            "both grants give the Fable week back, so they are of one kind"
        );
        let service = service_for(&up, store.clone(), &now);
        let row = store.account("a").unwrap().unwrap();

        let waiting = service.claim_limit_reset(&row, "rg-a-2", None).await;
        assert!(
            matches!(waiting, Err(ClaimRefusal::NotUsable)),
            "{waiting:?}"
        );
        assert_eq!(up.requests().len(), 0, "the waiting grant sends nothing");
        let first = service
            .claim_limit_reset(&row, "rg-a-1", None)
            .await
            .unwrap();
        assert_eq!(first.outcome, ClaimOutcome::Used);
        let claims = claims_sent(&up);
        assert_eq!(claims.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&claims[0].body).unwrap();
        assert_eq!(body["grant_id"], "rg-a-1");
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_stored_block_that_does_not_parse_holds_no_usable_grant() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![claim_rule(answering_with(
            200,
            br#"{"result":"reset"}"#,
        ))])
        .await;
        let store = store_for_claims(ORGANIZATION, &block_with(1, 1));
        // The rig's block holds `rg-a-1` usable, so a text in its place that cuts off inside the grants is all the
        // claim can read.
        let cut_off = r#"{"eligible":true,"grants":["#;
        assert!(store
            .set_limit_resets("a", 1, 1, 0, Some(cut_off), 1_000_000)
            .unwrap());
        let service = service_for(&up, store.clone(), &now);
        let row = store.account("a").unwrap().unwrap();

        let refused = service.claim_limit_reset(&row, "rg-a-1", None).await;
        assert!(
            matches!(refused, Err(ClaimRefusal::NotUsable)),
            "{refused:?}"
        );
        up.shutdown().await;
    }

    #[tokio::test]
    async fn an_organization_id_with_a_separator_sends_nothing() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![claim_rule(answering_with(
            200,
            br#"{"result":"reset"}"#,
        ))])
        .await;
        // 36 characters, so only the character check refuses it.
        let store = store_for_claims("00000000-0000-4000-8000/../000000000", &block_with(3, 3));
        let service = service_for(&up, store.clone(), &now);
        let row = store.account("a").unwrap().unwrap();

        let refused = service.claim_limit_reset(&row, "rg-a-1", None).await;
        assert!(
            matches!(refused, Err(ClaimRefusal::NoOrganization)),
            "{refused:?}"
        );
        assert_eq!(up.requests().len(), 0);
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_second_claim_waits_for_no_first() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![claim_rule(Respond::Gated(Box::new(answering_with(
            200,
            br#"{"result":"reset"}"#,
        ))))])
        .await;
        let store = store_for_claims(ORGANIZATION, &block_with(3, 3));
        let service = Arc::new(service_for(&up, store.clone(), &now));
        let row = store.account("a").unwrap().unwrap();
        let claim = || {
            let (service, row) = (service.clone(), row.clone());
            tokio::spawn(async move { service.claim_limit_reset(&row, "rg-a-1", None).await })
        };
        let first = claim();
        wait_for(Duration::from_secs(5), || {
            (claims_sent(&up).len() == 1).then_some(())
        })
        .await;
        let second = claim();
        // Either the second answers while the first is held, or it reaches the fake beside the first.
        wait_for(Duration::from_secs(5), || {
            (second.is_finished() || claims_sent(&up).len() == 2).then_some(())
        })
        .await;
        up.release();

        let (first, second) = (first.await.unwrap(), second.await.unwrap());
        assert!(matches!(second, Err(ClaimRefusal::InFlight)), "{second:?}");
        assert_eq!(first.unwrap().outcome, ClaimOutcome::Used);
        assert_eq!(claims_sent(&up).len(), 1);
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_rate_limited_claim_arms_no_hold() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![usage_body_rule(), claim_rule(answering(429))]).await;
        let store = store_for_claims(ORGANIZATION, &block_with(3, 3));
        let service = service_for(&up, store.clone(), &now);
        let row = store.account("a").unwrap().unwrap();
        service
            .claim_limit_reset(&row, "rg-a-1", None)
            .await
            .unwrap();

        let snapshot = service.poll("a").await.unwrap();
        assert_eq!(
            (snapshot.error, requests_on(&up, USAGE_LINE)),
            (None, 1),
            "the next usage poll sends its request"
        );
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_hold_stops_no_claim() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![
            rate_limited("120"),
            claim_rule(answering_with(200, br#"{"result":"reset"}"#)),
        ])
        .await;
        let store = store_for_claims(ORGANIZATION, &block_with(3, 3));
        let service = service_for(&up, store.clone(), &now);
        service.poll("a").await.unwrap();
        let row = store.account("a").unwrap().unwrap();
        service
            .claim_limit_reset(&row, "rg-a-1", None)
            .await
            .unwrap();

        assert_eq!(
            claims_sent(&up).len(),
            1,
            "the claim goes out while the usage reads wait out the 429"
        );
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_read_from_before_a_claim_does_not_bring_the_reset_back() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![
            limit_resets_rule(Respond::Gated(Box::new(answering_with(200, BLOCK)))),
            claim_rule(answering_with(200, br#"{"result":"reset"}"#)),
        ])
        .await;
        let store = store_for_claims(ORGANIZATION, &block_with(1, 1));
        let service = Arc::new(service_for(&up, store.clone(), &now));
        let row = store.account("a").unwrap().unwrap();
        let reading = {
            let service = service.clone();
            tokio::spawn(async move { service.limit_resets("a").await })
        };
        wait_for(Duration::from_secs(5), || {
            (requests_on(&up, LIMIT_RESETS_LINE) == 1).then_some(())
        })
        .await;
        let claim = service
            .claim_limit_reset(&row, "rg-a-1", None)
            .await
            .unwrap();
        assert_eq!(claim.outcome, ClaimOutcome::Used);
        up.release();
        let read = reading
            .await
            .unwrap()
            .unwrap()
            .expect("the row holds a block");

        assert_eq!(
            stored_grants(&store),
            (vec![GrantStatus::Used], vec![0]),
            "the held read's block, one reset left, lands nothing over the claim's fold"
        );
        assert_eq!(
            read.grants[0].resets_left, 0,
            "the held read answers the block the row holds"
        );
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_claim_does_not_follow_an_id_to_another_holder() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![claim_rule(answering_with(
            200,
            br#"{"result":"reset"}"#,
        ))])
        .await;
        let store = store_for_claims(ORGANIZATION, &block_with(3, 3));
        let service = service_for(&up, store.clone(), &now);
        let before = store.account("a").unwrap().unwrap();
        store.delete_account("a", "test").unwrap();
        store
            .insert_account(&AccountRow {
                identity: Some(phoebis_store::Identity {
                    account_uuid: "00000000-0000-4000-8000-000000000001".into(),
                    email: "test@example.invalid".into(),
                    organization_uuid: Some(ORGANIZATION.into()),
                }),
                ..AccountRow::for_tests("a", AccountKind::Oauth)
            })
            .unwrap();
        let after = store.account("a").unwrap().unwrap();
        store
            .commit_credentials(
                "a",
                after.incarnation,
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
        store_block(&store, after.incarnation, &block_with(3, 3));

        let refused = service.claim_limit_reset(&before, "rg-a-1", None).await;
        assert!(
            matches!(refused, Err(ClaimRefusal::AccountChanged)),
            "{refused:?}"
        );
        assert_eq!(
            up.requests().len(),
            0,
            "the new holder's reset is not spent on the deleted holder's confirmation"
        );
        let claim = service
            .claim_limit_reset(&after, "rg-a-1", None)
            .await
            .unwrap();
        assert_eq!(claim.outcome, ClaimOutcome::Used);
        assert_eq!(claims_sent(&up).len(), 1);
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_kept_request_id_stays_with_its_holder() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![claim_rule(Respond::CloseAfterRequest)]).await;
        let store = store_for_claims(ORGANIZATION, &block_with(3, 3));
        let service = service_for(&up, store.clone(), &now);
        let before = store.account("a").unwrap().unwrap();
        service
            .claim_limit_reset(&before, "rg-a-1", None)
            .await
            .unwrap();
        // No `invalidate` tells the service of the new row, since it would drop the kept id before the holder is read.
        store.delete_account("a", "test").unwrap();
        store
            .insert_account(&AccountRow {
                identity: Some(phoebis_store::Identity {
                    account_uuid: "00000000-0000-4000-8000-000000000001".into(),
                    email: "test@example.invalid".into(),
                    organization_uuid: Some(ORGANIZATION.into()),
                }),
                ..AccountRow::for_tests("a", AccountKind::Oauth)
            })
            .unwrap();
        let after = store.account("a").unwrap().unwrap();
        store
            .commit_credentials(
                "a",
                after.incarnation,
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
        store_block(&store, after.incarnation, &block_with(3, 3));
        answer_claims(&up, answering_with(200, br#"{"result":"reset"}"#));
        service
            .claim_limit_reset(&after, "rg-a-1", None)
            .await
            .unwrap();

        let ids = request_ids(&up);
        assert_eq!(ids.len(), 2);
        assert_ne!(
            ids[0], ids[1],
            "the new holder's claim does not repeat the id the deleted holder's claim left unsettled"
        );
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_repeats_reset_with_no_count_takes_one_off_once() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        // The read between the two attempts stores one reset left of three, the grant still usable.
        let one_left = br#"{"cedar_ember":{"eligible":true,"grants":[{"id":"rg-a-1","label":"Launch reset","clears":["seven_day_overage_included"],"gives_back_weekly":true,"starts_at":"2026-01-01T00:00:00Z","ends_at":"2027-01-01T00:00:00Z","resets_left":1,"resets_total":3,"usable_now":true}]}}"#;
        let up = FakeUpstream::start(vec![
            limit_resets_rule(answering_with(200, one_left)),
            claim_rule(Respond::CloseAfterRequest),
        ])
        .await;
        let store = store_for_claims(ORGANIZATION, &block_with(3, 3));
        let service = service_for(&up, store.clone(), &now);
        let row = store.account("a").unwrap().unwrap();
        service
            .claim_limit_reset(&row, "rg-a-1", None)
            .await
            .unwrap();
        // One millisecond after the outcome, so the read follows the attempt and keeps the id for the usable grant.
        now.store(1_000_001, Ordering::SeqCst);
        service.limit_resets("a").await.unwrap();
        answer_claims(&up, answering_with(200, br#"{"result":"reset"}"#));
        service
            .claim_limit_reset(&row, "rg-a-1", None)
            .await
            .unwrap();

        let ids = request_ids(&up);
        assert_eq!(ids.len(), 2);
        assert_eq!(
            ids[0], ids[1],
            "the second claim repeats the first one's id"
        );
        assert_eq!(
            stored_block(&store).grants[0].resets_left,
            1,
            "the reset answers for the first attempt's use of three, and the read's count of one already holds it"
        );
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_re_pair_drops_the_kept_request_id() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![claim_rule(Respond::CloseAfterRequest)]).await;
        let store = store_for_claims(ORGANIZATION, &block_with(3, 3));
        let service = service_for(&up, store.clone(), &now);
        let row = store.account("a").unwrap().unwrap();
        service
            .claim_limit_reset(&row, "rg-a-1", None)
            .await
            .unwrap();
        service.invalidate("a");
        answer_claims(&up, answering_with(200, br#"{"result":"reset"}"#));
        service
            .claim_limit_reset(&row, "rg-a-1", None)
            .await
            .unwrap();

        let ids = request_ids(&up);
        assert_eq!(ids.len(), 2);
        assert_ne!(
            ids[0], ids[1],
            "an id sent under the pairing before does not go out for the next one"
        );
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_claim_that_crosses_a_re_pair_keeps_no_request_id() {
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up =
            FakeUpstream::start(vec![claim_rule(Respond::Gated(Box::new(answering(500))))]).await;
        let store = store_for_claims(ORGANIZATION, &block_with(3, 3));
        let service = Arc::new(service_for(&up, store.clone(), &now));
        let row = store.account("a").unwrap().unwrap();
        let claiming = {
            let (service, row) = (service.clone(), row.clone());
            tokio::spawn(async move { service.claim_limit_reset(&row, "rg-a-1", None).await })
        };
        wait_for(Duration::from_secs(5), || {
            (claims_sent(&up).len() == 1).then_some(())
        })
        .await;
        service.invalidate("a");
        up.release();
        let first = claiming.await.unwrap().unwrap();
        // The gate stays open, so the second claim is answered 500 at once.
        service
            .claim_limit_reset(&row, "rg-a-1", None)
            .await
            .unwrap();

        assert_eq!(first.outcome, ClaimOutcome::Unsettled);
        let ids = request_ids(&up);
        assert_eq!(ids.len(), 2);
        assert_ne!(
            ids[0], ids[1],
            "the first claim went out under the pairing before the call, so its id is not kept"
        );
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_claim_that_a_re_pair_crosses_before_it_leaves_sends_nothing() {
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
            claim_rule(answering_with(200, br#"{"result":"reset"}"#)),
        ])
        .await;
        // An expired access token, so the claim's acquisition waits on the refresh the fake holds.
        let store = store_with_bundle_until(1);
        prepare_claims(&store, ORGANIZATION, &block_with(3, 3));
        let service = Arc::new(service_for(&up, store.clone(), &now));
        let row = store.account("a").unwrap().unwrap();
        let claiming = {
            let service = service.clone();
            tokio::spawn(async move { service.claim_limit_reset(&row, "rg-a-1", None).await })
        };
        wait_for(Duration::from_secs(5), || {
            up.requests()
                .iter()
                .any(|r| r.line.starts_with("POST /v1/oauth/token"))
                .then_some(())
        })
        .await;
        service.invalidate("a");
        up.release();
        let refused = claiming.await.unwrap();

        assert!(
            matches!(refused, Err(ClaimRefusal::AccountChanged)),
            "{refused:?}"
        );
        assert_eq!(
            claims_sent(&up).len(),
            0,
            "the organization id and the confirmation were read under the pairing before"
        );
        up.shutdown().await;
    }

    #[tokio::test]
    async fn a_read_from_before_an_unsettled_claim_keeps_its_request_id() {
        // The clock stays put, so the held read goes out at the instant the claim's outcome arrives.
        let now = Arc::new(AtomicI64::new(1_000_000));
        let up = FakeUpstream::start(vec![
            limit_resets_rule(Respond::Gated(Box::new(answering_with(200, BLOCKED_BODY)))),
            claim_rule(Respond::CloseAfterRequest),
        ])
        .await;
        up.set_rule_after(1, LIMIT_RESETS_PREFIX, answering_with(200, USABLE_BODY));
        let store = store_for_claims(ORGANIZATION, &block_with(3, 3));
        let service = Arc::new(service_for(&up, store.clone(), &now));
        let row = store.account("a").unwrap().unwrap();
        let reading = {
            let service = service.clone();
            tokio::spawn(async move { service.limit_resets("a").await })
        };
        wait_for(Duration::from_secs(5), || {
            (requests_on(&up, LIMIT_RESETS_LINE) == 1).then_some(())
        })
        .await;
        let first = service
            .claim_limit_reset(&row, "rg-a-1", None)
            .await
            .unwrap();
        up.release();
        let held = reading
            .await
            .unwrap()
            .unwrap()
            .expect("the held read answers a block");
        let held: Vec<GrantStatus> = grant_states(&held, 1_000_000)
            .into_iter()
            .map(|grant| grant.state)
            .collect();
        assert_eq!(
            held,
            vec![GrantStatus::Blocked],
            "the held read's write landed, and its block is one that drops an id"
        );
        service.limit_resets("a").await.unwrap();
        let second = service
            .claim_limit_reset(&row, "rg-a-1", None)
            .await
            .unwrap();

        assert_eq!(
            (first.outcome, second.outcome),
            (ClaimOutcome::Unsettled, ClaimOutcome::Unsettled)
        );
        let ids = request_ids(&up);
        assert_eq!(ids.len(), 2);
        assert_eq!(
            ids[0], ids[1],
            "a read from before the outcome says nothing about what the attempt spent"
        );
        up.shutdown().await;
    }
}
