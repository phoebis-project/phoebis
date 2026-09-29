//! The two configuration records: the process-wide limits the bootstrap file carries, read once at start, and
//! the per-pool policy a pool row carries, parsed on every read.

use serde::{Deserialize, Serialize};

pub const FIVE_MINUTES_MS: i64 = 300_000;
pub const ONE_HOUR_MS: i64 = 3_600_000;

/// A probe follows the session pin, and a pin shorter than the shortest cache TTL cannot keep the session on
/// the account the probe warmed.
pub const MIN_AFFINITY_SECONDS: u64 = 300;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct KeepaliveLimits {
    pub lease_seconds: u64,
    pub max_agents: usize,
    pub max_request_bytes: usize,
    pub max_retained_bytes: usize,
    pub max_sessions: usize,
    pub max_workers: usize,
    pub oauth_prewarm_verified: bool,
}

impl Default for KeepaliveLimits {
    fn default() -> Self {
        KeepaliveLimits {
            lease_seconds: 90,
            max_agents: 1_024,
            max_request_bytes: 8 << 20,
            max_retained_bytes: 256 << 20,
            max_sessions: 128,
            max_workers: 4,
            oauth_prewarm_verified: false,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LimitsError {
    #[error("keepalive.{field} must be between {min} and {max}, not {value}")]
    OutOfRange {
        field: &'static str,
        min: u64,
        max: u64,
        value: u64,
    },
    #[error("keepalive.max_retained_bytes must be at least keepalive.max_request_bytes")]
    RetainedBelowRequest,
}

fn bound<E>(
    field: &'static str,
    value: u64,
    min: u64,
    max: u64,
    err: impl Fn(&'static str, u64, u64, u64) -> E,
) -> Result<(), E> {
    if (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(err(field, min, max, value))
    }
}

impl KeepaliveLimits {
    pub fn validate(&self) -> Result<(), LimitsError> {
        let range = |field, min, max, value| LimitsError::OutOfRange {
            field,
            min,
            max,
            value,
        };
        bound("lease_seconds", self.lease_seconds, 10, 3_600, range)?;
        bound("max_agents", self.max_agents as u64, 1, 16_384, range)?;
        bound("max_sessions", self.max_sessions as u64, 1, 4_096, range)?;
        bound("max_workers", self.max_workers as u64, 1, 64, range)?;
        bound(
            "max_request_bytes",
            self.max_request_bytes as u64,
            1_024,
            8 << 20,
            range,
        )?;
        bound(
            "max_retained_bytes",
            self.max_retained_bytes as u64,
            1_024,
            512 << 20,
            range,
        )?;
        if self.max_retained_bytes < self.max_request_bytes {
            return Err(LimitsError::RetainedBelowRequest);
        }
        Ok(())
    }

    pub fn lease_ms(&self) -> i64 {
        self.lease_seconds as i64 * 1_000
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct KeepalivePolicy {
    pub cold_probe_limit: u32,
    pub enabled: bool,
    pub jitter_seconds_1h: u64,
    pub jitter_seconds_5m: u64,
    pub lead_seconds_1h: u64,
    pub lead_seconds_5m: u64,
    pub max_idle_seconds: u64,
    pub max_idle_ticks: u64,
    pub net_negative_grace_probes: u32,
    /// Probes stop for the day once a subscription account's probes have consumed this share of its 5-hour
    /// window; `None` is uncapped. Between 0 and 1.
    pub probe_cap_window_share_per_day: Option<f64>,
    pub probe_cost_cap_usd_per_day: Option<f64>,
    pub retry_max_seconds: u64,
    pub retry_seconds: u64,
    pub stop_when_net_negative: bool,
    /// Off retains no recipe for an agent other than `main`; a subagent's requests still count as real traffic
    /// for the main agent's idle limits, since the session is alive.
    pub warm_subagents: bool,
}

impl Default for KeepalivePolicy {
    fn default() -> Self {
        KeepalivePolicy {
            cold_probe_limit: 2,
            enabled: false,
            jitter_seconds_1h: 30,
            jitter_seconds_5m: 5,
            lead_seconds_1h: 360,
            lead_seconds_5m: 60,
            max_idle_seconds: 3_600,
            max_idle_ticks: 0,
            net_negative_grace_probes: 2,
            probe_cap_window_share_per_day: None,
            probe_cost_cap_usd_per_day: None,
            retry_max_seconds: 300,
            retry_seconds: 30,
            stop_when_net_negative: true,
            warm_subagents: true,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PolicyError {
    #[error("keepalive.enabled requires affinity_ttl_seconds of at least 300: a probe follows the session pin, and a pin shorter than the shortest cache TTL cannot keep the session on the account it warmed")]
    AffinityTooShort,
    #[error("keepalive.probe_cost_cap_usd_per_day must be null or a positive number")]
    CapNotPositive,
    #[error("keepalive.{field} must be between {min} and {max}, not {value}")]
    OutOfRange {
        field: &'static str,
        min: u64,
        max: u64,
        value: u64,
    },
    #[error("keepalive.retry_max_seconds must be at least keepalive.retry_seconds")]
    RetryMaxBelowRetry,
    #[error("keepalive.probe_cap_window_share_per_day must be null or between 0 and 1")]
    ShareOutOfRange,
    #[error("keepalive is not a policy object: {0}")]
    Shape(String),
}

impl KeepalivePolicy {
    /// What the admin API accepts: the shape and every field bound. The affinity rule needs the pool row and is
    /// checked by `validate`.
    pub fn parse(raw: &serde_json::Value) -> Result<KeepalivePolicy, PolicyError> {
        // The container-level `default` makes a JSON array deserialize element by element, filling anything
        // past the end from the default rather than erroring, so a non-object value has to be caught here.
        if !raw.is_object() {
            return Err(PolicyError::Shape(
                "keepalive policy must be a JSON object".to_string(),
            ));
        }
        let policy: KeepalivePolicy =
            serde_json::from_value(raw.clone()).map_err(|e| PolicyError::Shape(e.to_string()))?;
        policy.validate_fields()?;
        Ok(policy)
    }

    /// What a stored row reads as. A row written before the policy was typed may hold anything, and a value
    /// outside its bounds could only have got there the same way; either reads as probing off rather than as an
    /// error every reader would have to handle.
    pub fn from_json(raw: &str) -> KeepalivePolicy {
        serde_json::from_str::<KeepalivePolicy>(raw)
            .ok()
            .filter(|policy| policy.validate_fields().is_ok())
            .unwrap_or_default()
    }

    pub fn validate_fields(&self) -> Result<(), PolicyError> {
        let range = |field, min, max, value| PolicyError::OutOfRange {
            field,
            min,
            max,
            value,
        };
        bound(
            "cold_probe_limit",
            u64::from(self.cold_probe_limit),
            1,
            20,
            range,
        )?;
        bound("retry_seconds", self.retry_seconds, 1, 3_600, range)?;
        bound(
            "retry_max_seconds",
            self.retry_max_seconds,
            1,
            604_800,
            range,
        )?;
        if self.retry_max_seconds < self.retry_seconds {
            return Err(PolicyError::RetryMaxBelowRetry);
        }
        bound(
            "max_idle_seconds",
            self.max_idle_seconds,
            0,
            31_536_000,
            range,
        )?;
        bound("max_idle_ticks", self.max_idle_ticks, 0, 1_000_000, range)?;
        // A lead or a jitter at or past the TTL it applies to would put the due time before the cache was even
        // written.
        bound("lead_seconds_5m", self.lead_seconds_5m, 0, 299, range)?;
        bound("lead_seconds_1h", self.lead_seconds_1h, 0, 3_599, range)?;
        bound("jitter_seconds_5m", self.jitter_seconds_5m, 0, 299, range)?;
        bound("jitter_seconds_1h", self.jitter_seconds_1h, 0, 3_599, range)?;
        bound(
            "net_negative_grace_probes",
            u64::from(self.net_negative_grace_probes),
            0,
            100,
            range,
        )?;
        if self
            .probe_cost_cap_usd_per_day
            .is_some_and(|cap| cap <= 0.0 || !cap.is_finite())
        {
            return Err(PolicyError::CapNotPositive);
        }
        if self
            .probe_cap_window_share_per_day
            .is_some_and(|share| !(0.0..=1.0).contains(&share))
        {
            return Err(PolicyError::ShareOutOfRange);
        }
        Ok(())
    }

    pub fn validate(&self, affinity_ttl_seconds: u64) -> Result<(), PolicyError> {
        self.validate_fields()?;
        if self.enabled && affinity_ttl_seconds < MIN_AFFINITY_SECONDS {
            return Err(PolicyError::AffinityTooShort);
        }
        Ok(())
    }

    pub fn lead_ms(&self, ttl_ms: i64) -> i64 {
        (if ttl_ms > FIVE_MINUTES_MS {
            self.lead_seconds_1h
        } else {
            self.lead_seconds_5m
        }) as i64
            * 1_000
    }

    pub fn jitter_ms(&self, ttl_ms: i64) -> i64 {
        (if ttl_ms > FIVE_MINUTES_MS {
            self.jitter_seconds_1h
        } else {
            self.jitter_seconds_5m
        }) as i64
            * 1_000
    }

    pub fn retry_ms(&self) -> i64 {
        self.retry_seconds as i64 * 1_000
    }

    pub fn retry_max_ms(&self) -> i64 {
        self.retry_max_seconds as i64 * 1_000
    }

    pub fn max_idle_ms(&self) -> Option<i64> {
        (self.max_idle_seconds > 0).then(|| self.max_idle_seconds as i64 * 1_000)
    }
}
