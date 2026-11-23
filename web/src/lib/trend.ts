import type { QuotaHistory } from '@/api/types';
import { MINUTE } from './time';

/** Percents. A value is null before its series' first sample, after its last one and from a sample with no figure to
 *  the series' next sample. */
export interface TrendPoint {
  at: number;
  u5: number | null;
  u7: number | null;
  uf: number | null;
}

const BUCKET_MS = 5 * MINUTE;

/** The same boundaries as the ledger's `bucket_of`, which answers the bucket's start in seconds. */
export const bucketOf = (iso: string) => Math.floor(Date.parse(iso) / BUCKET_MS);

/**
 * Joins the 5-hour, 7-day and Fable series on the union of their samples' 5-minute buckets, the ledger's, each point at
 * its bucket's start, so an account whose plan reports no 5-hour window still draws its week. A series carries its last
 * value between its own samples and ends at its last one, so a window that stopped draws no line on to the week's end.
 * A sample with no figure carries its null, so the line breaks where the upstream reported none.
 * `every` thins the joint base: 7 days of buckets is 2,016 points, and every third is plenty for a 900px plot.
 */
export function joinTrend(history: QuotaHistory[], every = 3): TrendPoint[] {
  const series = (id: string) => (history.find((h) => h.windowId === id)?.samples ?? []).map((s) => ({ bucket: bucketOf(s.at), value: s.utilization === null ? null : s.utilization * 100 }));
  const h5 = series('5h');
  const h7 = series('7d');
  const hF = series('7d_fable');
  // Each series arrives in time order and the joint base is walked in bucket order, so one cursor per series suffices.
  const carry = (samples: { bucket: number; value: number | null }[]) => {
    let i = 0;
    let last: number | null = null;
    const end = samples.at(-1)?.bucket ?? -Infinity;
    return (bucket: number) => {
      while (i < samples.length && samples[i].bucket <= bucket) last = samples[i++].value;
      return bucket > end ? null : last;
    };
  };
  const at5 = carry(h5);
  const at7 = carry(h7);
  const atF = carry(hF);
  const buckets = [...new Set([...h5, ...h7, ...hF].map((s) => s.bucket))].sort((a, b) => a - b);
  return buckets
    .filter((_, i) => i % every === 0)
    .map((bucket) => ({ at: bucket * BUCKET_MS, u5: at5(bucket), u7: at7(bucket), uf: atF(bucket) }));
}
