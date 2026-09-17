//! The limits the bootstrap file carries and the policy a pool row carries, with the bounds of each.
use phoebis_keepalive::{KeepaliveLimits, KeepalivePolicy, LimitsError, PolicyError};
use serde_json::json;

#[test]
fn the_default_limits_are_the_specs() {
    let limits = KeepaliveLimits::default();
    assert_eq!(
        (
            limits.lease_seconds,
            limits.max_agents,
            limits.max_request_bytes,
            limits.max_retained_bytes,
            limits.max_sessions,
            limits.max_workers,
            limits.oauth_prewarm_verified
        ),
        (90, 1_024, 8 << 20, 256 << 20, 128, 4, false)
    );
    assert_eq!(limits.validate(), Ok(()));
    let policy = KeepalivePolicy::default();
    assert_eq!(
        (
            policy.cold_probe_limit,
            policy.enabled,
            policy.jitter_seconds_1h,
            policy.jitter_seconds_5m,
            policy.lead_seconds_1h,
            policy.lead_seconds_5m
        ),
        (2, false, 30, 5, 360, 60)
    );
    assert_eq!(
        (
            policy.max_idle_seconds,
            policy.max_idle_ticks,
            policy.net_negative_grace_probes,
            policy.probe_cost_cap_usd_per_day,
            policy.retry_max_seconds,
            policy.retry_seconds,
            policy.stop_when_net_negative
        ),
        (3_600, 0, 2, None, 300, 30, true)
    );
    assert_eq!(policy.validate(3_600), Ok(()));
    assert_eq!(
        policy.validate(0),
        Ok(()),
        "a disabled policy has no affinity requirement"
    );
    assert_eq!(
        (
            policy.lead_ms(300_000),
            policy.lead_ms(3_600_000),
            policy.jitter_ms(300_000),
            policy.jitter_ms(3_600_000)
        ),
        (60_000, 360_000, 5_000, 30_000)
    );
    assert_eq!(
        (
            policy.retry_ms(),
            policy.retry_max_ms(),
            policy.max_idle_ms()
        ),
        (30_000, 300_000, Some(3_600_000))
    );
    let parsed: KeepalivePolicy = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
    assert!(
        parsed.enabled && parsed.lead_seconds_5m == 60,
        "every absent field takes its default"
    );
    let text = serde_json::to_string(&parsed).unwrap();
    assert!(
        text.contains("\"probe_cost_cap_usd_per_day\":null")
            && text.contains("\"stop_when_net_negative\":true")
    );
}

#[test]
fn limits_and_policies_outside_their_bounds_are_refused() {
    let with = |f: fn(&mut KeepaliveLimits)| {
        let mut l = KeepaliveLimits::default();
        f(&mut l);
        l.validate()
    };
    assert_eq!(
        with(|l| l.lease_seconds = 9),
        Err(LimitsError::OutOfRange {
            field: "lease_seconds",
            min: 10,
            max: 3_600,
            value: 9
        })
    );
    assert!(matches!(
        with(|l| l.lease_seconds = 3_601),
        Err(LimitsError::OutOfRange {
            field: "lease_seconds",
            ..
        })
    ));
    assert!(matches!(
        with(|l| l.max_agents = 0),
        Err(LimitsError::OutOfRange {
            field: "max_agents",
            ..
        })
    ));
    assert!(matches!(
        with(|l| l.max_agents = 16_385),
        Err(LimitsError::OutOfRange {
            field: "max_agents",
            ..
        })
    ));
    assert!(matches!(
        with(|l| l.max_sessions = 4_097),
        Err(LimitsError::OutOfRange {
            field: "max_sessions",
            ..
        })
    ));
    assert!(matches!(
        with(|l| l.max_workers = 65),
        Err(LimitsError::OutOfRange {
            field: "max_workers",
            ..
        })
    ));
    assert!(matches!(
        with(|l| l.max_request_bytes = 1_023),
        Err(LimitsError::OutOfRange {
            field: "max_request_bytes",
            ..
        })
    ));
    assert!(matches!(
        with(|l| l.max_request_bytes = (8 << 20) + 1),
        Err(LimitsError::OutOfRange {
            field: "max_request_bytes",
            ..
        })
    ));
    assert!(matches!(
        with(|l| l.max_retained_bytes = (512 << 20) + 1),
        Err(LimitsError::OutOfRange {
            field: "max_retained_bytes",
            ..
        })
    ));
    assert_eq!(
        with(|l| {
            l.max_request_bytes = 4 << 20;
            l.max_retained_bytes = 2 << 20;
        }),
        Err(LimitsError::RetainedBelowRequest)
    );
    assert!(
        serde_json::from_str::<KeepaliveLimits>(r#"{"probe_enabled":true}"#).is_err(),
        "unknown fields reject the block"
    );

    let policy = |f: fn(&mut KeepalivePolicy)| {
        let mut p = KeepalivePolicy::default();
        f(&mut p);
        p
    };
    assert_eq!(
        policy(|p| p.enabled = true).validate(299),
        Err(PolicyError::AffinityTooShort)
    );
    assert_eq!(
        policy(|p| p.enabled = true).validate(0),
        Err(PolicyError::AffinityTooShort)
    );
    assert_eq!(policy(|p| p.enabled = true).validate(300), Ok(()));
    assert_eq!(
        policy(|p| p.cold_probe_limit = 0).validate_fields(),
        Err(PolicyError::OutOfRange {
            field: "cold_probe_limit",
            min: 1,
            max: 20,
            value: 0
        })
    );
    assert!(matches!(
        policy(|p| p.cold_probe_limit = 21).validate_fields(),
        Err(PolicyError::OutOfRange {
            field: "cold_probe_limit",
            ..
        })
    ));
    assert!(matches!(
        policy(|p| p.retry_seconds = 0).validate_fields(),
        Err(PolicyError::OutOfRange {
            field: "retry_seconds",
            ..
        })
    ));
    assert!(matches!(
        policy(|p| p.retry_seconds = 3_601).validate_fields(),
        Err(PolicyError::OutOfRange {
            field: "retry_seconds",
            ..
        })
    ));
    assert_eq!(
        policy(|p| p.retry_max_seconds = 29).validate_fields(),
        Err(PolicyError::RetryMaxBelowRetry)
    );
    assert!(matches!(
        policy(|p| p.max_idle_seconds = 31_536_001).validate_fields(),
        Err(PolicyError::OutOfRange {
            field: "max_idle_seconds",
            ..
        })
    ));
    assert!(matches!(
        policy(|p| p.max_idle_ticks = 1_000_001).validate_fields(),
        Err(PolicyError::OutOfRange {
            field: "max_idle_ticks",
            ..
        })
    ));
    assert!(matches!(
        policy(|p| p.lead_seconds_5m = 300).validate_fields(),
        Err(PolicyError::OutOfRange {
            field: "lead_seconds_5m",
            ..
        })
    ));
    assert!(matches!(
        policy(|p| p.lead_seconds_1h = 3_600).validate_fields(),
        Err(PolicyError::OutOfRange {
            field: "lead_seconds_1h",
            ..
        })
    ));
    assert!(matches!(
        policy(|p| p.jitter_seconds_5m = 300).validate_fields(),
        Err(PolicyError::OutOfRange {
            field: "jitter_seconds_5m",
            ..
        })
    ));
    assert!(matches!(
        policy(|p| p.jitter_seconds_1h = 3_600).validate_fields(),
        Err(PolicyError::OutOfRange {
            field: "jitter_seconds_1h",
            ..
        })
    ));
    assert!(matches!(
        policy(|p| p.net_negative_grace_probes = 101).validate_fields(),
        Err(PolicyError::OutOfRange {
            field: "net_negative_grace_probes",
            ..
        })
    ));
    assert_eq!(
        policy(|p| p.probe_cost_cap_usd_per_day = Some(0.0)).validate_fields(),
        Err(PolicyError::CapNotPositive)
    );
    assert_eq!(
        policy(|p| p.probe_cost_cap_usd_per_day = Some(-1.0)).validate_fields(),
        Err(PolicyError::CapNotPositive)
    );
    assert_eq!(
        policy(|p| p.probe_cost_cap_usd_per_day = Some(f64::NAN)).validate_fields(),
        Err(PolicyError::CapNotPositive)
    );
    assert_eq!(
        policy(|p| p.probe_cost_cap_usd_per_day = Some(2.5)).validate_fields(),
        Ok(())
    );
    assert!(
        matches!(
            KeepalivePolicy::parse(&json!({"enabled": true, "affinity_mode": "session"})),
            Err(PolicyError::Shape(_))
        ),
        "the dropped Go field is unknown"
    );
    assert!(matches!(
        KeepalivePolicy::parse(&json!([])),
        Err(PolicyError::Shape(_))
    ));
    assert!(
        matches!(
            KeepalivePolicy::parse(&json!({"cold_probe_limit": 0})),
            Err(PolicyError::OutOfRange { .. })
        ),
        "parse applies the field bounds"
    );
}

#[test]
fn a_stored_policy_that_does_not_parse_reads_as_off() {
    assert_eq!(KeepalivePolicy::from_json("{}"), KeepalivePolicy::default());
    assert_eq!(
        KeepalivePolicy::from_json(r#"{"enabled":true,"probe_enabled":true}"#),
        KeepalivePolicy::default(),
        "an unknown field turns the row off rather than guessing"
    );
    assert_eq!(
        KeepalivePolicy::from_json(r#"{"enabled":true,"cold_probe_limit":0}"#),
        KeepalivePolicy::default(),
        "a stored value outside its bounds turns the row off"
    );
    assert_eq!(
        KeepalivePolicy::from_json("not json"),
        KeepalivePolicy::default()
    );
    assert!(KeepalivePolicy::from_json(r#"{"enabled":true,"lead_seconds_5m":90}"#).enabled);
}
