// One shared edge set (84 log-spaced buckets from 1, edge 64 at 100,000 and the last edge near 3,652,000) covers every
// latency metric in milliseconds and output tokens per second, so histograms from different hours and metrics merge
// without re-bucketing.
export const BUCKET_COUNT = 84;
const MIN = 1;
const MAX = 100_000;
// The ratio is fixed by the 64 steps from 1 to 100,000. The 20 buckets past them keep it and reach about 61 minutes.
const BASE_STEPS = 64;
const RATIO = Math.pow(MAX / MIN, 1 / BASE_STEPS);

export const BUCKET_EDGES: readonly number[] = Array.from({ length: BUCKET_COUNT + 1 }, (_, i) => MIN * Math.pow(RATIO, i));

/** Every percentile read from a histogram is exact only to the bucket it lands in. */
export const PERCENTILE_HINT = '±1 bucket';

export const emptyHistogram = (): number[] => new Array<number>(BUCKET_COUNT).fill(0);

export function bucketIndex(value: number): number {
  if (!(value > MIN)) return 0;
  const i = Math.floor(Math.log(value / MIN) / Math.log(RATIO));
  return Math.max(0, Math.min(BUCKET_COUNT - 1, i));
}

export function histogramOf(values: readonly number[]): number[] {
  const h = emptyHistogram();
  for (const v of values) h[bucketIndex(v)] += 1;
  return h;
}

export function mergeHistograms(histograms: readonly (readonly number[])[]): number[] {
  const out = emptyHistogram();
  for (const h of histograms) {
    for (let i = 0; i < BUCKET_COUNT; i += 1) out[i] += h[i] ?? 0;
  }
  return out;
}

export const histogramCount = (h: readonly number[]) => h.reduce((s, n) => s + n, 0);

/** Geometric midpoint of the bucket holding the p-th value, or null for an empty histogram. */
export function percentileFromHistogram(h: readonly number[], p: number): number | null {
  const total = histogramCount(h);
  if (total === 0) return null;
  const target = Math.max(1, Math.ceil(p * total));
  let seen = 0;
  for (let i = 0; i < BUCKET_COUNT; i += 1) {
    seen += h[i];
    if (seen >= target) return Math.sqrt(BUCKET_EDGES[i] * BUCKET_EDGES[i + 1]);
  }
  return Math.sqrt(BUCKET_EDGES[BUCKET_COUNT - 1] * BUCKET_EDGES[BUCKET_COUNT]);
}

/** The four percentiles a group reads off a histogram, one reading for the session fold and the mock's routes. */
export const quartet = (h: readonly number[]) => ({ p50: percentileFromHistogram(h, 0.5), p90: percentileFromHistogram(h, 0.9), p95: percentileFromHistogram(h, 0.95), p99: percentileFromHistogram(h, 0.99) });
