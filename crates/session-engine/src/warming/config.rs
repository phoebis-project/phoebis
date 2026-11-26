//! The cache warming's configuration: the process-wide byte cap that the bootstrap file carries, read once at start,
//! the per-pool policy a pool row carries, parsed on every read, and the schedule's fixed timing.

use serde::{Deserialize, Serialize};

pub use phoebis_routing::{FIVE_MINUTES_MS, ONE_HOUR_MS};

/// An agent's TTL as its schedule, its ceilings and its credit read it: an entry whose TTL no evidence named yet, 0,
/// lives the API's default five minutes.
pub(crate) fn floor_ttl_ms(ttl_ms: i64) -> i64 {
    ttl_ms.max(FIVE_MINUTES_MS)
}

/// A probe follows the session pin, and a pin shorter than the shortest cache TTL cannot keep the session on
/// the account the probe warmed.
pub const MIN_AFFINITY_SECONDS: u64 = 300;
/// The most probes running at once.
pub const MAX_WORKERS: usize = 4;

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

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct KeepaliveLimits {
    pub max_retained_bytes: usize,
}

impl Default for KeepaliveLimits {
    fn default() -> Self {
        KeepaliveLimits {
            max_retained_bytes: 256 << 20,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LimitsError {
    #[error("keepalive.{field} must be between {min} and {max}, not {value}")]
    OutOfRange {
        field: &'static str,
        min: u64,
        max: u64,
        value: u64,
    },
}

impl KeepaliveLimits {
    /// `max_request_bytes` is the relay's per-body bound, which is also the largest recipe the engine retains. A
    /// retained budget below it could not hold one recipe the relay accepted.
    pub fn validate(&self, max_request_bytes: usize) -> Result<(), LimitsError> {
        let range = |field, min, max, value| LimitsError::OutOfRange {
            field,
            min,
            max,
            value,
        };
        bound(
            "max_retained_bytes",
            self.max_retained_bytes as u64,
            max_request_bytes as u64,
            512 << 20,
            range,
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct KeepalivePolicy {
    pub enabled: bool,
    /// The idle window of the main conversation, taken as set, above the recipe's break-even too. 0 resolves to the
    /// break-even.
    pub max_idle_seconds: u64,
    /// How long a park on a background command or a subagent is probed. `None` is the recipe's break-even.
    pub max_parked_seconds: Option<u64>,
    /// A park on a scheduled wakeup is probed while the wake is at most this far away. `None` is the recipe's
    /// break-even.
    pub scheduled_horizon_seconds: Option<u64>,
}

impl Default for KeepalivePolicy {
    fn default() -> Self {
        KeepalivePolicy {
            enabled: false,
            max_idle_seconds: 3_600,
            max_parked_seconds: None,
            scheduled_horizon_seconds: None,
        }
    }
}

/// The members a stored row is read for. A row saved before a field left still names it, and only the admin path
/// refuses an unknown name: a stored one is ignored so the pool keeps the four fields it had.
const POLICY_MEMBERS: [&str; 4] = [
    "enabled",
    "max_idle_seconds",
    "max_parked_seconds",
    "scheduled_horizon_seconds",
];

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PolicyError {
    #[error("keepalive.enabled requires affinity_ttl_seconds of at least 300: a probe follows the session pin, and a pin shorter than the shortest cache TTL cannot keep the session on the account it warmed")]
    AffinityTooShort,
    #[error("keepalive.{field} must be between {min} and {max}, not {value}")]
    OutOfRange {
        field: &'static str,
        min: u64,
        max: u64,
        value: u64,
    },
    #[error("keepalive is not a policy object: {0}")]
    Shape(String),
}

impl KeepalivePolicy {
    /// What the admin API accepts: the shape and every field bound. The affinity rule needs the pool row and is
    /// checked by `validate`.
    pub fn parse(raw: &serde_json::Value) -> Result<KeepalivePolicy, PolicyError> {
        KeepalivePolicy::of_value(raw.clone())
    }

    /// `parse` over a value it may consume, so `read` hands over the members it kept without a copy.
    fn of_value(raw: serde_json::Value) -> Result<KeepalivePolicy, PolicyError> {
        // The container-level `default` makes a JSON array deserialize element by element, filling anything
        // past the end from the default rather than erroring, so a non-object value has to be caught here.
        if !raw.is_object() {
            return Err(PolicyError::Shape(
                "keepalive policy must be a JSON object".to_string(),
            ));
        }
        let policy: KeepalivePolicy =
            serde_json::from_value(raw).map_err(|e| PolicyError::Shape(e.to_string()))?;
        policy.validate_fields()?;
        Ok(policy)
    }

    /// What a stored row reads as. A row written before the policy was typed may hold anything, and one written
    /// before a bound it now fails was added holds a value the admin API no longer accepts; either reads as
    /// probing off rather than as an error every reader would have to handle. `read` names the refusal.
    pub fn from_json(raw: &str) -> KeepalivePolicy {
        KeepalivePolicy::read(raw).unwrap_or_default()
    }

    /// A stored row with the refusal that turns it off, for the one place that reports it.
    pub fn read(raw: &str) -> Result<KeepalivePolicy, PolicyError> {
        let mut value: serde_json::Value =
            serde_json::from_str(raw).map_err(|e| PolicyError::Shape(e.to_string()))?;
        if let Some(members) = value.as_object_mut() {
            members.retain(|name, _| POLICY_MEMBERS.contains(&name.as_str()));
        }
        KeepalivePolicy::of_value(value)
    }

    pub fn validate_fields(&self) -> Result<(), PolicyError> {
        let range = |field, min, max, value| PolicyError::OutOfRange {
            field,
            min,
            max,
            value,
        };
        bound(
            "max_idle_seconds",
            self.max_idle_seconds,
            0,
            31_536_000,
            range,
        )?;
        if let Some(seconds) = self.max_parked_seconds {
            bound("max_parked_seconds", seconds, 60, 604_800, range)?;
        }
        if let Some(seconds) = self.scheduled_horizon_seconds {
            bound("scheduled_horizon_seconds", seconds, 60, 604_800, range)?;
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
}

/// The schedule's fixed numbers: not an operator's setting, and read by the binary through `for_start`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KeepaliveTiming {
    /// A probe lands with a minute to spare on a five-minute entry.
    pub lead_5m_ms: i64,
    /// Six minutes on a one-hour entry, the Go engine's margin.
    pub lead_1h_ms: i64,
    /// Spreads a pool's probes so one tick never bursts.
    pub jitter_5m_ms: i64,
    /// The same spread on a one-hour entry, wider since its six-minute lead leaves the room.
    pub jitter_1h_ms: i64,
    /// Two cold probes in a row and the entry is gone for good.
    pub cold_probe_limit: u32,
    /// The first retry's wait, which each further failure doubles up to `retry_max_ms`.
    pub retry_ms: i64,
    /// Caps the doubling and an upstream `Retry-After` so one wait stays inside a one-hour entry's six-minute lead.
    pub retry_max_ms: i64,
    /// The UTC days of idle-return counters the engine keeps, the current day among them, 0 keeping every day. The
    /// overview's longest range is 30 days, so the default covers it.
    pub idle_return_retention_days: u32,
}

impl Default for KeepaliveTiming {
    fn default() -> Self {
        KeepaliveTiming {
            lead_5m_ms: 60_000,
            lead_1h_ms: 360_000,
            jitter_5m_ms: 5_000,
            jitter_1h_ms: 30_000,
            cold_probe_limit: 2,
            retry_ms: 30_000,
            retry_max_ms: 300_000,
            idle_return_retention_days: 30,
        }
    }
}

impl KeepaliveTiming {
    /// The default. Under the `debug-hooks` feature, which only the router's test build turns on,
    /// `PHOEBIS_DEBUG_KEEPALIVE_LEAD_5M_SECONDS` and `PHOEBIS_DEBUG_KEEPALIVE_JITTER_5M_SECONDS` replace the
    /// five-minute lead and jitter, so an end-to-end test sees a probe a second after its request rather than
    /// four minutes later.
    pub fn for_start() -> KeepaliveTiming {
        #[allow(unused_mut)]
        let mut timing = KeepaliveTiming::default();
        #[cfg(feature = "debug-hooks")]
        {
            let seconds = |name: &str| {
                std::env::var(name)
                    .ok()
                    .and_then(|value| value.parse::<i64>().ok())
            };
            if let Some(lead) = seconds("PHOEBIS_DEBUG_KEEPALIVE_LEAD_5M_SECONDS") {
                timing.lead_5m_ms = lead * 1_000;
            }
            if let Some(jitter) = seconds("PHOEBIS_DEBUG_KEEPALIVE_JITTER_5M_SECONDS") {
                timing.jitter_5m_ms = jitter * 1_000;
            }
        }
        timing
    }

    pub fn lead_ms(&self, ttl_ms: i64) -> i64 {
        if ttl_ms > FIVE_MINUTES_MS {
            self.lead_1h_ms
        } else {
            self.lead_5m_ms
        }
    }

    pub fn jitter_ms(&self, ttl_ms: i64) -> i64 {
        if ttl_ms > FIVE_MINUTES_MS {
            self.jitter_1h_ms
        } else {
            self.jitter_5m_ms
        }
    }

    /// Probes an hour on an entry of `ttl_ms`: one every TTL less the lead, 15 on five minutes and 1.11 on an hour.
    pub fn cadence_per_hour(&self, ttl_ms: i64) -> f64 {
        ONE_HOUR_MS as f64 / (ttl_ms - self.lead_ms(ttl_ms)) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A field the struct gains without a name in `POLICY_MEMBERS` would read as its default from every stored row.
    #[test]
    fn the_stored_read_names_every_field_of_the_policy() {
        let serialized = serde_json::to_value(KeepalivePolicy::default()).unwrap();
        let fields: std::collections::BTreeSet<&str> = serialized
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(fields, POLICY_MEMBERS.into_iter().collect());
    }
}
