//! The cause words: why an agent's recipe is in the state it is in, and why the probes a recipe paid for went
//! unanswered. Each is a permanent counter key and a word of the saved payload.

/// Why probes a recipe paid for went unanswered: the stop that left them, or the request after them that read no
/// cache. The label the wasted count carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Wasted {
    ColdReturn,
    Evicted,
    Expired,
    Horizon,
    IdleLimit,
    ParkedLimit,
    Released,
    Replaced,
    SessionEnded,
}

impl Wasted {
    /// Every label, so a reader lists the ones no probe carried at zero rather than leaving them out.
    pub const ALL: [Wasted; 9] = [
        Wasted::ColdReturn,
        Wasted::Evicted,
        Wasted::Expired,
        Wasted::Horizon,
        Wasted::IdleLimit,
        Wasted::ParkedLimit,
        Wasted::Released,
        Wasted::Replaced,
        Wasted::SessionEnded,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Wasted::ColdReturn => "cold_return",
            Wasted::Evicted => "evicted",
            Wasted::Expired => "expired",
            Wasted::Horizon => "horizon",
            Wasted::IdleLimit => "idle_limit",
            Wasted::ParkedLimit => "parked_limit",
            Wasted::Released => "released",
            Wasted::Replaced => "replaced",
            Wasted::SessionEnded => "session_ended",
        }
    }
}

macro_rules! reasons {
    ($($variant:ident => $text:literal),* $(,)?) => {
        /// Every state an agent's recipe can be in, as the Go engine named them plus the router's own gates.
        /// A reason is a permanent counter key, so nothing mints one at runtime.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum Reason { $($variant),* }
        impl Reason {
            pub const ALL: &'static [Reason] = &[$(Reason::$variant),*];
            pub fn as_str(self) -> &'static str {
                match self { $(Reason::$variant => $text),* }
            }
        }
    };
}

reasons! {
    AffinityTooShort => "affinity_too_short",
    AgentLimit => "agent_limit",
    AgentReleased => "agent_released",
    AgentUnknown => "agent_unknown",
    AwaitingCacheEvidence => "awaiting_cache_evidence",
    AwaitingRequest => "awaiting_request",
    AwaitingSelection => "awaiting_selection",
    Budget => "budget",
    BusGap => "bus_gap",
    Compacted => "compacted",
    Cooled => "cooled",
    Credits => "credits",
    Evicted => "evicted",
    Expired => "expired",
    ExpiredAtRestart => "expired_at_restart",
    FixedThinking => "fixed_thinking",
    ForcedTool => "forced_tool",
    HeadersTooLarge => "headers_too_large",
    Horizon => "horizon",
    IdleLimit => "idle_limit",
    IneligibleCredential => "ineligible_credential",
    InstanceMismatch => "instance_mismatch",
    InstanceReplaced => "instance_replaced",
    InvalidCacheControl => "invalid_cache_control",
    InvalidModel => "invalid_model",
    InvalidProbeUsage => "invalid_probe_usage",
    InvalidRequest => "invalid_request",
    KeepaliveOff => "keepalive_off",
    KeyOptedOut => "key_opted_out",
    LeaseExpired => "lease_expired",
    LedgerBehind => "ledger_behind",
    MissingAccount => "missing_account",
    NoCacheEvidence => "no_cache_evidence",
    NoPin => "no_pin",
    NoProbeCacheEvidence => "no_probe_cache_evidence",
    OptedOut => "opted_out",
    Overage => "overage",
    ParkedLimit => "parked_limit",
    PartialCacheCoverage => "partial_cache_coverage",
    Paused => "paused",
    PinMoved => "pin_moved",
    PoolDisabled => "pool_disabled",
    ProbeAuthenticationError => "probe_authentication_error",
    ProbePoolExhausted => "probe_pool_exhausted",
    ProbeQuotaExhausted => "probe_quota_exhausted",
    ProbeRetryLimit => "probe_retry_limit",
    ProbeTransportError => "probe_transport_error",
    Probing => "probing",
    Reauth => "reauth",
    RepeatedColdProbe => "repeated_cold_probe",
    RequestCanceled => "request_canceled",
    RequestFailed => "request_failed",
    RequestLimit => "request_limit",
    RequestRejected => "request_rejected",
    RequestTooLarge => "request_too_large",
    Reset => "reset",
    RetryableProbeError => "retryable_probe_error",
    Scheduled => "scheduled",
    SessionEnded => "session_ended",
    SessionExpired => "session_expired",
    SessionLimit => "session_limit",
    SessionOptedOut => "session_opted_out",
    SlowProbe => "slow_probe",
    Spent => "spent",
    StorageLimit => "storage_limit",
    StructuredOutput => "structured_output",
    UnexpectedOutput => "unexpected_output",
    UnsupportedProbe => "unsupported_probe",
    UnsupportedShape => "unsupported_shape",
}

/// A cause leaves the engine as its word, the one the counters and the stored payload already key it by.
impl serde::Serialize for Reason {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// A stored word reads back as its cause, and a word this build does not know refuses the payload.
impl<'de> serde::Deserialize<'de> for Reason {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Reason, D::Error> {
        let word = String::deserialize(deserializer)?;
        Reason::ALL
            .iter()
            .copied()
            .find(|reason| reason.as_str() == word)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown reason {word}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ALL` is kept by hand. The cycle below names every variant with no wildcard, so a variant the enum gains stops
    /// this test compiling until it joins the cycle, and the walk then wants it in `ALL`.
    #[test]
    fn wasted_all_holds_every_variant() {
        let next = |label: Wasted| match label {
            Wasted::ColdReturn => Wasted::Evicted,
            Wasted::Evicted => Wasted::Expired,
            Wasted::Expired => Wasted::Horizon,
            Wasted::Horizon => Wasted::IdleLimit,
            Wasted::IdleLimit => Wasted::ParkedLimit,
            Wasted::ParkedLimit => Wasted::Released,
            Wasted::Released => Wasted::Replaced,
            Wasted::Replaced => Wasted::SessionEnded,
            Wasted::SessionEnded => Wasted::ColdReturn,
        };
        let mut walked = vec![Wasted::ColdReturn];
        let mut label = next(Wasted::ColdReturn);
        while label != Wasted::ColdReturn && walked.len() <= Wasted::ALL.len() {
            walked.push(label);
            label = next(label);
        }
        assert_eq!(walked, Wasted::ALL.to_vec());
    }
}
