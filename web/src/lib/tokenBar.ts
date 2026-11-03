import type { Usage } from '@/api/types';
import { TOKEN_SERIES } from '@/components/charts/theme';
import { fmtInt, tokensTotal } from '@/lib/format';

export interface TokenSegment {
  count: number;
  key: (typeof TOKEN_SERIES)[number]['key'];
  label: string;
  share: number;
  slot: number;
}

/** The routes answer one output sum, so each view that splits it into thinking and plain output reads the rest here. */
export const plainOutputOf = (t: { output: number; thinking: number }): number => Math.max(0, t.output - t.thinking);

/** The counts behind the Tokens chart's kinds: the plain output is the output less its thinking share. */
const countsOf = (usage: Usage): Record<TokenSegment['key'], number> => ({
  cacheRead: usage.cacheRead,
  cacheWrite1h: usage.cacheWrite1h,
  cacheWrite5m: usage.cacheWrite5m,
  plainOutput: plainOutputOf(usage),
  thinking: usage.thinking,
  uncachedInput: usage.input,
});

/**
 * The request's token kinds in the Tokens chart's order, each with its count and its share of the total. A kind with
 * no tokens is left out, and the plain output is the output less its thinking share.
 */
export function tokenSegments(usage: Usage): TokenSegment[] {
  const total = tokensTotal(usage);
  if (total <= 0) return [];
  const counts = countsOf(usage);
  return TOKEN_SERIES.filter((s) => counts[s.key] > 0).map((s) => ({ count: counts[s.key], key: s.key, label: s.label, share: counts[s.key] / total, slot: s.slot }));
}

/** The bar's spoken label: the total and each kind's count, "no tokens" at zero. */
export function tokenBarLabel(usage: Usage): string {
  const total = tokensTotal(usage);
  if (total <= 0) return 'no tokens';
  return `${fmtInt(total)} tokens: ${tokenSegments(usage)
    .map((s) => `${s.label} ${fmtInt(s.count)}`)
    .join(', ')}`;
}
