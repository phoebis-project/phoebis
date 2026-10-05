//! The limits the bootstrap file carries and the policy a pool row carries, with the bounds of each.
use phoebis_keepalive::{
    KeepaliveLimits, KeepalivePolicy, LimitsError, PolicyError, LEASE_MS, LEASE_SECONDS,
    MAX_WORKERS,
};
use serde_json::json;

#[test]
fn the_default_limits_are_the_specs() {
    let limits = KeepaliveLimits::default();
    assert_eq!(
        (
            limits.max_agents,
            limits.max_retained_bytes,
            limits.max_sessions
        ),
        (1_024, 256 << 20, 128)
    );
    assert_eq!(limits.validate(RELAY_BOUND), Ok(()));
    assert_eq!(
        (LEASE_SECONDS, LEASE_MS, MAX_WORKERS),
        (90, 90_000, 4),
        "the lease and the worker count are fixed"
    );
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

/// The relay's per-body bound, which `validate` takes as the retained budget's floor.
const RELAY_BOUND: usize = 32 << 20;

#[test]
fn the_retained_budget_accepts_the_relays_body_bound_to_512_mib() {
    let with = |retained: usize| {
        KeepaliveLimits {
            max_retained_bytes: retained,
            ..KeepaliveLimits::default()
        }
        .validate(RELAY_BOUND)
    };
    assert_eq!(with(RELAY_BOUND), Ok(()));
    assert_eq!(with(512 << 20), Ok(()));
    for value in [RELAY_BOUND - 1, (512 << 20) + 1] {
        let refused = with(value).unwrap_err();
        assert_eq!(
            refused,
            LimitsError::OutOfRange {
                field: "max_retained_bytes",
                min: 33_554_432,
                max: 536_870_912,
                value: value as u64
            }
        );
        assert_eq!(
            refused.to_string(),
            format!(
                "keepalive.max_retained_bytes must be between 33554432 and 536870912, not {value}"
            )
        );
    }
}

#[test]
fn limits_and_policies_outside_their_bounds_are_refused() {
    let with = |f: fn(&mut KeepaliveLimits)| {
        let mut l = KeepaliveLimits::default();
        f(&mut l);
        l.validate(RELAY_BOUND)
    };
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
    // Each value sits inside its own bound; together they reach the TTL they apply to.
    let five_minutes = policy(|p| {
        p.lead_seconds_5m = 200;
        p.jitter_seconds_5m = 100;
    })
    .validate_fields();
    assert_eq!(
        five_minutes,
        Err(PolicyError::LeadPlusJitter {
            lead: "lead_seconds_5m",
            jitter: "jitter_seconds_5m"
        })
    );
    assert_eq!(
        five_minutes.unwrap_err().to_string(),
        "keepalive.lead_seconds_5m plus keepalive.jitter_seconds_5m must stay under the cache TTL they apply to"
    );
    assert_eq!(
        policy(|p| {
            p.lead_seconds_1h = 3_000;
            p.jitter_seconds_1h = 600;
        })
        .validate_fields(),
        Err(PolicyError::LeadPlusJitter {
            lead: "lead_seconds_1h",
            jitter: "jitter_seconds_1h"
        })
    );
    assert_eq!(
        policy(|p| {
            p.lead_seconds_5m = 199;
            p.jitter_seconds_5m = 100;
            p.lead_seconds_1h = 2_999;
            p.jitter_seconds_1h = 600;
        })
        .validate_fields(),
        Ok(()),
        "a sum a second under the TTL is admitted"
    );
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
fn a_retry_ceiling_past_a_week_is_refused() {
    let policy = |f: fn(&mut KeepalivePolicy)| {
        let mut p = KeepalivePolicy::default();
        f(&mut p);
        p
    };
    assert!(matches!(
        policy(|p| p.retry_max_seconds = 9_223_372_036_854_776).validate_fields(),
        Err(PolicyError::OutOfRange {
            field: "retry_max_seconds",
            ..
        })
    ));
    assert_eq!(
        policy(|p| p.retry_max_seconds = 604_800).validate_fields(),
        Ok(())
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
        KeepalivePolicy::from_json(
            r#"{"enabled":true,"lead_seconds_5m":299,"jitter_seconds_5m":5}"#
        ),
        KeepalivePolicy::default(),
        "a stored lead and jitter that together reach the TTL turn the row off"
    );
    assert_eq!(
        KeepalivePolicy::from_json("not json"),
        KeepalivePolicy::default()
    );
    assert!(KeepalivePolicy::from_json(r#"{"enabled":true,"lead_seconds_5m":90}"#).enabled);
}
