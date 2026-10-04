import type { QuotaHistory } from '@/api/types';

/**
 * Percents; a 7-day or Fable value is null before that series' first sample, and every Fable value is null for an
 * account without a Fable window.
 */
export interface TrendPoint {
  at: number;
  u5: number;
  u7: number | null;
  uf: number | null;
}

/** The same boundaries as the ledger's `bucket_of`, which answers the bucket's start in seconds. */
export const bucketOf = (iso: string) => Math.floor(Date.parse(iso) / 300_000);

/**
 * Joins the 7-day and Fable series onto the 5-hour samples by the ledger's 5-minute bucket, carrying each series' last
 * value forward. `every` thins the 5-hour samples: 7 days of them is 2,016 points, and every third is plenty for a
 * 900px plot.
 */
export function joinTrend(history: QuotaHistory[], hasFable: boolean, every = 3): TrendPoint[] {
  const series = (id: string) => (history.find((h) => h.windowId === id)?.samples ?? []).map((s) => ({ bucket: bucketOf(s.at), value: s.utilization * 100 }));
  const h5 = history.find((h) => h.windowId === '5h')?.samples ?? [];
  // Each series arrives in time order and the 5-hour samples are walked in order, so one cursor per series suffices.
  const carry = (samples: { bucket: number; value: number }[]) => {
    let i = 0;
    let last: number | null = null;
    return (bucket: number) => {
      while (i < samples.length && samples[i].bucket <= bucket) last = samples[i++].value;
      return last;
    };
  };
  const at7 = carry(series('7d'));
  const atF = carry(series('7d_fable'));
  return h5
    .filter((_, i) => i % every === 0)
    .map((s) => {
      const bucket = bucketOf(s.at);
      return { at: Date.parse(s.at), u5: s.utilization * 100, u7: at7(bucket), uf: hasFable ? atF(bucket) : null };
    });
}
