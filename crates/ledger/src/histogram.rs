//! Fixed log-spaced histograms: the web mock's 64 buckets from 1 to 100,000, shared by every latency in
//! milliseconds and by output tokens per second, so any two histograms merge by bucket-wise addition.

pub const BUCKET_COUNT: usize = 64;
const MIN: f64 = 1.0;
const MAX: f64 = 100_000.0;

fn ratio() -> f64 {
    (MAX / MIN).powf(1.0 / BUCKET_COUNT as f64)
}

/// The last edge, which is the bound of the last finite bucket.
pub fn top_edge() -> f64 {
    MAX
}

pub fn bucket_edges() -> [f64; BUCKET_COUNT + 1] {
    let mut edges = [0.0; BUCKET_COUNT + 1];
    for (i, edge) in edges.iter_mut().enumerate() {
        *edge = MIN * ratio().powi(i as i32);
    }
    edges
}

/// The mock's rule exactly: anything at or below the first edge lands in bucket 0, and nothing lands past the
/// last bucket, so a percentile read here and one read in the browser agree. The clamp stays for the stored
/// histograms, whose buckets have to keep merging with the UI's edge set; the Prometheus series counts an
/// observation past the last edge separately, where `+Inf` can carry it honestly.
pub fn bucket_index(value: f64) -> usize {
    if value.partial_cmp(&MIN) != Some(std::cmp::Ordering::Greater) {
        return 0;
    }
    let i = ((value / MIN).ln() / ratio().ln()).floor();
    (i.max(0.0) as usize).min(BUCKET_COUNT - 1)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Histogram(pub [u32; BUCKET_COUNT]);

impl Default for Histogram {
    fn default() -> Self {
        Histogram::new()
    }
}

impl Histogram {
    pub fn new() -> Histogram {
        Histogram([0; BUCKET_COUNT])
    }

    pub fn record(&mut self, value: f64) {
        self.0[bucket_index(value)] = self.0[bucket_index(value)].saturating_add(1);
    }

    pub fn merge(&mut self, other: &Histogram) {
        for (mine, theirs) in self.0.iter_mut().zip(other.0.iter()) {
            *mine = mine.saturating_add(*theirs);
        }
    }

    pub fn subtract(&mut self, other: &Histogram) {
        for (mine, theirs) in self.0.iter_mut().zip(other.0.iter()) {
            *mine = mine.saturating_sub(*theirs);
        }
    }

    pub fn count(&self) -> u64 {
        self.0.iter().map(|&n| n as u64).sum()
    }

    pub fn buckets(&self) -> Vec<u64> {
        self.0.iter().map(|&n| n as u64).collect()
    }

    /// Geometric midpoint of the bucket holding the p-th value: exact to the bucket, which is the error the
    /// UI labels.
    pub fn percentile(&self, p: f64) -> Option<f64> {
        let total = self.count();
        if total == 0 {
            return None;
        }
        let target = ((p * total as f64).ceil() as u64).max(1);
        let edges = bucket_edges();
        let mut seen = 0u64;
        for (i, &n) in self.0.iter().enumerate() {
            seen += n as u64;
            if seen >= target {
                return Some((edges[i] * edges[i + 1]).sqrt());
            }
        }
        Some((edges[BUCKET_COUNT - 1] * edges[BUCKET_COUNT]).sqrt())
    }

    pub fn to_le_bytes(&self) -> Vec<u8> {
        self.0.iter().flat_map(|n| n.to_le_bytes()).collect()
    }

    pub fn from_le_bytes(bytes: &[u8]) -> Histogram {
        let mut out = Histogram::new();
        for (i, chunk) in bytes.chunks_exact(4).take(BUCKET_COUNT).enumerate() {
            out.0[i] = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_edges_are_the_mocks_and_a_value_lands_in_its_bucket() {
        let edges = bucket_edges();
        assert_eq!(edges[0], 1.0);
        assert!((edges[64] - 100_000.0).abs() < 1e-6);
        assert_eq!(bucket_index(0.0), 0);
        assert_eq!(bucket_index(1.0), 0);
        assert_eq!(
            bucket_index(1.5),
            2,
            "ln(1.5)/ln(ratio) with ratio = 100000^(1/64)"
        );
        assert_eq!(bucket_index(99_999.0), 63);
        assert_eq!(bucket_index(1e9), 63);
        for i in 0..BUCKET_COUNT {
            let mid = (edges[i] * edges[i + 1]).sqrt();
            assert_eq!(bucket_index(mid), i, "midpoint of bucket {i}");
        }
    }

    #[test]
    fn percentiles_come_from_merged_buckets_and_round_trip_little_endian() {
        let mut a = Histogram::new();
        let mut b = Histogram::new();
        for v in [10.0, 20.0, 30.0, 40.0, 50.0] {
            a.record(v);
        }
        for v in [60.0, 70.0, 80.0, 90.0, 100.0] {
            b.record(v);
        }
        assert_eq!(Histogram::new().percentile(0.5), None);
        let mut merged = a.clone();
        merged.merge(&b);
        assert_eq!(merged.count(), 10);
        let p50 = merged.percentile(0.5).unwrap();
        let edges = bucket_edges();
        let i = bucket_index(50.0);
        assert!(
            p50 >= edges[i] && p50 <= edges[i + 1],
            "p50 {p50} sits in the bucket of the 5th value"
        );
        let p99 = merged.percentile(0.99).unwrap();
        let i = bucket_index(100.0);
        assert!(p99 >= edges[i] && p99 <= edges[i + 1]);
        let bytes = merged.to_le_bytes();
        assert_eq!(bytes.len(), 256);
        assert_eq!(Histogram::from_le_bytes(&bytes), merged);
        let mut back = merged.clone();
        back.subtract(&b);
        assert_eq!(
            back, a,
            "subtracting a merged contribution restores the other"
        );
        assert_eq!(
            Histogram::from_le_bytes(&bytes[..8]).count(),
            merged.buckets()[..2].iter().sum::<u64>()
        );
        assert_eq!(
            &bytes[4 * bucket_index(10.0)..4 * bucket_index(10.0) + 4],
            1u32.to_le_bytes()
        );
    }
}
