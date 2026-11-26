//! The cache evidence a response carries, in the shape the Go engine read it: the read and write token counts
//! by TTL, and the TTL those counts prove.

use phoebis_ledger::Speed;
use serde_json::Value;

use crate::warming::config::{FIVE_MINUTES_MS, ONE_HOUR_MS};

pub const MAX_USAGE_BYTES: usize = 256 << 10;

/// Past this a count is not a token count the upstream would report; the check keeps an arithmetic overflow
/// out of the accounting.
const MAX_TOKENS: f64 = 1e12;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProbeUsage {
    pub read: u64,
    pub write: u64,
    pub output: u64,
    pub five_minutes: u64,
    pub one_hour: u64,
    /// Uncached input, which the Go engine never read: the probe cost estimate needs it.
    pub input: u64,
    /// The answer's `usage.speed`, which selects the cells the probe is priced at and the funding it files under.
    /// `Standard` when the answer named none, as the ledger reads the speed of the probe's own request row.
    pub speed: Speed,
}

impl ProbeUsage {
    /// The usage object of a JSON answer or of a `message_start` event. Any field that is not a non-negative
    /// integer refuses the whole block: a probe that reports nonsense must not arm a schedule.
    pub fn parse(raw: &[u8]) -> Option<ProbeUsage> {
        if raw.len() > MAX_USAGE_BYTES {
            return None;
        }
        let root: Value = serde_json::from_slice(raw).ok()?;
        let node = if root.get("type").and_then(Value::as_str) == Some("message_start") {
            root.get("message")?.get("usage")?
        } else {
            root.get("usage")?
        };
        if !node.is_object() {
            return None;
        }
        let mut usage = ProbeUsage::default();
        for (pointer, slot) in [
            (
                "/cache_creation/ephemeral_1h_input_tokens",
                &mut usage.one_hour,
            ),
            (
                "/cache_creation/ephemeral_5m_input_tokens",
                &mut usage.five_minutes,
            ),
            ("/cache_creation_input_tokens", &mut usage.write),
            ("/cache_read_input_tokens", &mut usage.read),
            ("/input_tokens", &mut usage.input),
            ("/output_tokens", &mut usage.output),
        ] {
            let Some(at) = node.pointer(pointer) else {
                continue;
            };
            let number = at.as_f64()?;
            if !(0.0..=MAX_TOKENS).contains(&number) || number.fract() != 0.0 {
                return None;
            }
            *slot = number as u64;
        }
        if usage.write == 0 {
            usage.write = usage.five_minutes + usage.one_hour;
        }
        usage.speed = Speed::of(node.get("speed").and_then(Value::as_str));
        Some(usage)
    }

    /// The bus's usage of a stream phase or a completion. A five-minute column without a one-hour column is the
    /// relay's filing of a bare `cache_creation_input_tokens`, the older wire shape, which the stream's
    /// `message_delta` repeats without the split the `message_start` gave: the column alone proves no TTL, so it
    /// reads as a write of unknown TTL, as `Usage::merge` reads it. With the one-hour column there, the two
    /// columns are the split.
    pub fn from_bus(usage: &phoebis_bus::Usage) -> ProbeUsage {
        let (five_minutes, one_hour, write) = match usage.cache_write_1h_tokens {
            Some(one_hour) => {
                let five_minutes = usage.cache_write_5m_tokens.unwrap_or(0);
                (five_minutes, one_hour, five_minutes + one_hour)
            }
            None => (0, 0, usage.cache_write_5m_tokens.unwrap_or(0)),
        };
        ProbeUsage {
            read: usage.cache_read_tokens.unwrap_or(0),
            write,
            output: usage.output_tokens.unwrap_or(0),
            five_minutes,
            one_hour,
            input: usage.input_tokens.unwrap_or(0),
            // The bus's usage carries no speed; the kept-warm observation this reads never prices a probe, whose
            // own answer goes through `parse`.
            speed: Speed::Standard,
        }
    }

    /// The TTL this usage proves. A five-minute write is decisive; a one-hour write without reads, or without
    /// a prior TTL, proves an hour; a read alone keeps what was known; the request's own marker caps it; and
    /// nothing known means five minutes, the shorter of the two.
    pub fn ttl_ms(&self, previous_ms: i64, requested_ms: i64) -> i64 {
        let mut ttl = previous_ms;
        if self.five_minutes > 0 {
            ttl = FIVE_MINUTES_MS;
        } else if self.one_hour > 0 && (self.read == 0 || ttl == 0) {
            ttl = ONE_HOUR_MS;
        }
        if requested_ms > 0 && (ttl == 0 || requested_ms < ttl) {
            ttl = requested_ms;
        }
        if ttl == 0 {
            ttl = FIVE_MINUTES_MS;
        }
        ttl
    }

    pub fn add(&mut self, other: ProbeUsage) {
        self.read += other.read;
        self.write += other.write;
        self.output += other.output;
        self.five_minutes += other.five_minutes;
        self.one_hour += other.one_hour;
        self.input += other.input;
    }

    pub fn evidence(&self) -> u64 {
        self.read + self.write
    }
}
