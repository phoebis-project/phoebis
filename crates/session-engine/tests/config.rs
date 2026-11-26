//! The limits the bootstrap file carries and the policy a pool row carries, with the bounds of each.
use phoebis_session_engine::{
    KeepaliveLimits, KeepalivePolicy, KeepaliveTiming, LimitsError, PolicyError, SessionCaps,
    FIVE_MINUTES_MS, LEASE_MS, LEASE_SECONDS, MAX_WORKERS, ONE_HOUR_MS,
};
use serde_json::json;

/// The timing seam's two variables, which `for_start` reads under the `debug-hooks` feature this test build turns on.
const TIMING_SEAM: [&str; 2] = [
    "PHOEBIS_DEBUG_KEEPALIVE_JITTER_5M_SECONDS",
    "PHOEBIS_DEBUG_KEEPALIVE_LEAD_5M_SECONDS",
];

/// Unsets the timing seam for its lifetime and puts back what the shell had set, so the defaults read below are
/// the binary's own whatever a developer's environment holds.
struct SeamCleared(Vec<(&'static str, Option<String>)>);

impl SeamCleared {
    fn new() -> SeamCleared {
        let saved = TIMING_SEAM
            .iter()
            .map(|name| (*name, std::env::var(name).ok()))
            .collect();
        for name in TIMING_SEAM {
            std::env::remove_var(name);
        }
        SeamCleared(saved)
    }
}

impl Drop for SeamCleared {
    fn drop(&mut self) {
        for (name, value) in &self.0 {
            if let Some(value) = value {
                std::env::set_var(name, value);
            }
        }
    }
}

#[test]
fn the_default_limits_are_the_specs() {
    let _seam = SeamCleared::new();
    let caps = SessionCaps::default();
    let limits = KeepaliveLimits::default();
    assert_eq!(
        (
            caps.max_agents,
            limits.max_retained_bytes,
            caps.max_sessions
        ),
        (1_024, 256 << 20, 128)
    );
    assert!(matches!(limits.validate(RELAY_BOUND), Ok(())));
    assert_eq!(
        (LEASE_SECONDS, LEASE_MS, MAX_WORKERS),
        (90, 90_000, 4),
        "the lease and the worker count are fixed"
    );
    let policy = KeepalivePolicy::default();
    assert_eq!(
        policy,
        KeepalivePolicy {
            enabled: false,
            max_idle_seconds: 3_600,
            max_parked_seconds: None,
            scheduled_horizon_seconds: None,
        }
    );
    assert_eq!(policy.validate(3_600), Ok(()));
    assert_eq!(
        policy.validate(0),
        Ok(()),
        "a disabled policy has no affinity requirement"
    );
    let timing = KeepaliveTiming::default();
    assert_eq!(
        (
            timing.cold_probe_limit,
            timing.retry_ms,
            timing.retry_max_ms
        ),
        (2, 30_000, 300_000)
    );
    assert_eq!(
        (
            timing.lead_ms(FIVE_MINUTES_MS),
            timing.lead_ms(ONE_HOUR_MS),
            timing.jitter_ms(FIVE_MINUTES_MS),
            timing.jitter_ms(ONE_HOUR_MS)
        ),
        (60_000, 360_000, 5_000, 30_000),
        "each TTL reads its own lead and jitter"
    );
    assert_eq!(
        KeepaliveTiming::for_start(),
        timing,
        "the binary starts on the defaults"
    );
    let parsed: KeepalivePolicy = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
    assert!(
        parsed.enabled && parsed.max_idle_seconds == 3_600,
        "every absent field takes its default"
    );
    assert_eq!(
        serde_json::to_string(&parsed).unwrap(),
        r#"{"enabled":true,"max_idle_seconds":3600,"max_parked_seconds":null,"scheduled_horizon_seconds":null}"#,
        "a null ceiling is written as null"
    );
}

/// The relay's per-body bound, which `validate` takes as the retained budget's floor.
const RELAY_BOUND: usize = 32 << 20;

#[test]
fn the_retained_budget_accepts_the_relays_body_bound_to_512_mib() {
    let with = |retained: usize| {
        KeepaliveLimits {
            max_retained_bytes: retained,
        }
        .validate(RELAY_BOUND)
    };
    assert!(matches!(with(RELAY_BOUND), Ok(())));
    assert!(matches!(with(512 << 20), Ok(())));
    for value in [RELAY_BOUND - 1, (512 << 20) + 1] {
        let refused = with(value).unwrap_err();
        assert!(
            matches!(
                refused,
                LimitsError::OutOfRange {
                    field: "max_retained_bytes",
                    min: 33_554_432,
                    max: 536_870_912,
                    value: refused_value
                } if refused_value == value as u64
            ),
            "{refused:?}"
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
        policy(|p| p.max_idle_seconds = 31_536_001).validate_fields(),
        Err(PolicyError::OutOfRange {
            field: "max_idle_seconds",
            min: 0,
            max: 31_536_000,
            value: 31_536_001
        })
    );
    assert_eq!(
        policy(|p| p.max_idle_seconds = 0).validate_fields(),
        Ok(()),
        "a zero idle window is the break-even"
    );
    let parked = |seconds| {
        KeepalivePolicy {
            max_parked_seconds: Some(seconds),
            ..KeepalivePolicy::default()
        }
        .validate_fields()
    };
    assert_eq!(
        parked(59),
        Err(PolicyError::OutOfRange {
            field: "max_parked_seconds",
            min: 60,
            max: 604_800,
            value: 59
        })
    );
    assert_eq!(parked(60), Ok(()));
    assert_eq!(parked(604_800), Ok(()));
    assert_eq!(
        parked(604_801),
        Err(PolicyError::OutOfRange {
            field: "max_parked_seconds",
            min: 60,
            max: 604_800,
            value: 604_801
        })
    );
    let horizon = |seconds| {
        KeepalivePolicy {
            scheduled_horizon_seconds: Some(seconds),
            ..KeepalivePolicy::default()
        }
        .validate_fields()
    };
    assert_eq!(
        horizon(59),
        Err(PolicyError::OutOfRange {
            field: "scheduled_horizon_seconds",
            min: 60,
            max: 604_800,
            value: 59
        })
    );
    assert_eq!(horizon(60), Ok(()));
    assert_eq!(horizon(604_800), Ok(()));
    assert_eq!(
        horizon(604_801),
        Err(PolicyError::OutOfRange {
            field: "scheduled_horizon_seconds",
            min: 60,
            max: 604_800,
            value: 604_801
        })
    );
    assert_eq!(
        KeepalivePolicy::parse(&json!({"max_parked_seconds": 59}))
            .unwrap_err()
            .to_string(),
        "keepalive.max_parked_seconds must be between 60 and 604800, not 59"
    );
    assert!(
        matches!(
            KeepalivePolicy::parse(&json!({"enabled": true, "affinity_mode": "session"})),
            Err(PolicyError::Shape(_))
        ),
        "the dropped Go field is unknown"
    );
    let removed = KeepalivePolicy::parse(&json!({"enabled": true, "lead_seconds_5m": 90}))
        .unwrap_err()
        .to_string();
    assert!(
        removed.starts_with("keepalive is not a policy object: unknown field `lead_seconds_5m`"),
        "a removed field is refused as any unknown field: {removed}"
    );
    assert!(matches!(
        KeepalivePolicy::parse(&json!([])),
        Err(PolicyError::Shape(_))
    ));
    assert!(
        matches!(
            KeepalivePolicy::parse(&json!({"scheduled_horizon_seconds": 0})),
            Err(PolicyError::OutOfRange { .. })
        ),
        "parse applies the field bounds"
    );
    assert_eq!(
        KeepalivePolicy::parse(&json!({"max_parked_seconds": null})),
        Ok(KeepalivePolicy::default()),
        "a null ceiling is the break-even"
    );
}

#[test]
fn a_stored_policy_with_removed_names_reads_as_the_four_fields_and_one_out_of_bounds_reads_as_off()
{
    assert_eq!(KeepalivePolicy::from_json("{}"), KeepalivePolicy::default());
    assert_eq!(
        KeepalivePolicy::from_json(r#"{"enabled":true,"lead_seconds_5m":90}"#),
        KeepalivePolicy {
            enabled: true,
            ..KeepalivePolicy::default()
        },
        "a removed name is ignored and the row stays on"
    );
    assert_eq!(
        KeepalivePolicy::from_json(
            r#"{"enabled":true,"lead_seconds_5m":0,"max_idle_seconds":900,"max_parked_seconds":7200,"scheduled_horizon_seconds":86400}"#
        ),
        KeepalivePolicy {
            enabled: true,
            max_idle_seconds: 900,
            max_parked_seconds: Some(7_200),
            scheduled_horizon_seconds: Some(86_400),
        },
        "the four known members keep their stored values, whatever the removed ones held"
    );
    assert_eq!(
        KeepalivePolicy::read(r#"{"enabled":true,"probe_enabled":true}"#).map(|p| p.enabled),
        Ok(true),
        "the stored read keeps the four members and ignores the rest"
    );
    assert_eq!(
        KeepalivePolicy::from_json(r#"{"enabled":true,"max_parked_seconds":30}"#),
        KeepalivePolicy::default(),
        "a stored value outside its bounds turns the row off"
    );
    assert!(matches!(
        KeepalivePolicy::read(r#"{"enabled":true,"max_parked_seconds":30}"#),
        Err(PolicyError::OutOfRange {
            field: "max_parked_seconds",
            ..
        })
    ));
    assert_eq!(
        KeepalivePolicy::from_json(r#"{"enabled":"yes"}"#),
        KeepalivePolicy::default(),
        "a known member of the wrong type turns the row off"
    );
    assert_eq!(
        KeepalivePolicy::from_json("not json"),
        KeepalivePolicy::default()
    );
    assert_eq!(
        KeepalivePolicy::from_json("[true]"),
        KeepalivePolicy::default(),
        "a stored value that is not an object turns the row off"
    );
}
