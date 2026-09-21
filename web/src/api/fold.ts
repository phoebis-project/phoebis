import { CACHE_MISS_REASONS, WASTED_PROBE_REASONS, emptyCacheMissReasons, emptyWastedProbeReasons } from '@/lib/cache';
import { emptyHistogram, histogramOf, mergeHistograms, percentileFromHistogram } from '@/lib/histogram';
import { matchesPattern } from '@/lib/pattern';
import type { AnalyticsGroup, AnalyticsGroupBy, AnalyticsPoint, RequestLog, RequestPage } from './types';

const METRICS = ['durationMs', 'outputTps', 'ttfbMs', 'ttftMs'] as const;
const quartet = (h: readonly number[]) => ({ p50: percentileFromHistogram(h, 0.5), p90: percentileFromHistogram(h, 0.9), p95: percentileFromHistogram(h, 0.95), p99: percentileFromHistogram(h, 0.99) });

const empty = (key: string, label = key): AnalyticsGroup => ({
  attempts: 0,
  cacheHits: 0,
  cacheMisses: 0,
  cacheMissReasons: emptyCacheMissReasons(),
  cost: { cacheRead: 0, cacheWrite: 0, input: 0, output: 0 },
  costUsd: 0,
  errors: {},
  exhausted: 0,
  expiredBetweenTurns: 0,
  failed: 0,
  failovers: 0,
  histograms: { durationMs: emptyHistogram(), outputTps: emptyHistogram(), ttfbMs: emptyHistogram(), ttftMs: emptyHistogram() },
  keptWarm: { requests: 0, tokens: 0 },
  key,
  label,
  percentiles: { durationMs: quartet(emptyHistogram()), outputTps: quartet(emptyHistogram()), ttfbMs: quartet(emptyHistogram()), ttftMs: quartet(emptyHistogram()) },
  probes: { failed: 0, migrated: 0, ok: 0, skipped: 0 },
  rateLimited: 0,
  rejected: 0,
  requests: 0,
  savings: { grossUsd: 0, probeCostUsd: 0, wastedProbeReasons: emptyWastedProbeReasons(), wastedProbeUsd: 0, wastedProbes: 0 },
  succeeded: 0,
  tokens: { cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: 0, output: 0, thinking: 0, uncachedInput: 0, webFetch: 0, webSearch: 0 },
  uncached: 0,
});

const keyOf = (r: RequestLog, by: AnalyticsGroupBy, agentGroups: string[]): string => {
  if (by === 'account') return r.accountId ?? '-';
  if (by === 'effort') return r.shape.effort ?? 'none';
  if (by === 'key') return r.keyId ?? '-';
  if (by === 'kind') return r.kind;
  if (by === 'model') return r.model;
  if (by === 'pool') return r.poolId ?? '-';
  if (by === 'user_agent') {
    // The rollups file a row that named no client under 'unknown'; under a pattern list it falls with every
    // other unmatched client under 'other', as the breakdown route files it.
    if (agentGroups.length === 0) return r.userAgent ?? 'unknown';
    return agentGroups.find((p) => matchesPattern(p, r.userAgent ?? '')) ?? 'other';
  }
  return 'all';
};

export interface FoldOptions {
  /** Patterns naming the groups under `user_agent`; a row falls in the first that matches, the rest under `other`. */
  agentGroups?: string[];
  /** Patterns a row's client must match one of; a row with no client matches none. */
  agentPatterns?: string[];
}

/** Throughput is measured after the first content token, as the ledger measures it; without both timings there is no sample. */
const outputTps = (r: RequestLog): number | null => (r.durationMs !== null && r.ttftMs !== null && r.durationMs - r.ttftMs > 0 && r.usage.output > 0 ? r.usage.output / ((r.durationMs - r.ttftMs) / 1000) : null);

/** Request rows summed the way the rollups sum them, for the one filter the rollups cannot answer: a session. */
export function foldGroups(rows: RequestLog[], by: AnalyticsGroupBy, options: FoldOptions = {}): AnalyticsGroup[] {
  const groups = new Map<string, AnalyticsGroup>();
  const lists = new Map<string, RequestLog[]>();
  const patterns = options.agentPatterns ?? [];
  const kept = patterns.length ? rows.filter((r) => patterns.some((p) => matchesPattern(p, r.userAgent ?? ''))) : rows;
  for (const r of kept) {
    const key = keyOf(r, by, options.agentGroups ?? []);
    const g = groups.get(key) ?? empty(key, key === '-' ? 'unattributed' : key);
    groups.set(key, g);
    const list = lists.get(key) ?? [];
    list.push(r);
    lists.set(key, list);
    g.requests += 1;
    if (r.outcome === 'succeeded') g.succeeded += 1;
    else if (r.outcome === 'exhausted') g.exhausted += 1;
    else if (r.outcome === 'rejected') g.rejected += 1;
    else if (r.status === 429) g.rateLimited += 1;
    else g.failed += 1;
    if (r.errorCode) g.errors[r.errorCode] = (g.errors[r.errorCode] ?? 0) + 1;
    g.attempts += r.attempts.length;
    // A failover is an account change between adjacent attempts, as the ledger's delta_of counts it; a retry
    // on the same account is none.
    g.failovers += r.attempts.filter((a, i) => i > 0 && a.accountId !== r.attempts[i - 1].accountId).length;
    g.tokens.cacheRead += r.usage.cacheRead;
    g.tokens.cacheWrite1h += r.usage.cacheWrite1h;
    g.tokens.cacheWrite5m += r.usage.cacheWrite5m;
    g.tokens.output += r.usage.output;
    g.tokens.thinking += r.usage.thinking;
    g.tokens.uncachedInput += r.usage.input;
    g.tokens.webFetch += r.usage.webFetch;
    g.tokens.webSearch += r.usage.webSearch;
    g.costUsd += r.costUsd;
    // The ledger classifies by tokens: a read is a hit, a write without a read is a miss, neither is uncached.
    // The diagnosed reason is a breakdown of its own and decides nothing here.
    if (r.usage.cacheRead > 0) g.cacheHits += 1;
    else if (r.usage.cacheWrite5m + r.usage.cacheWrite1h > 0) g.cacheMisses += 1;
    else g.uncached += 1;
    const reason = r.attempts[r.attempts.length - 1]?.cacheMissReason ?? null;
    if (reason && CACHE_MISS_REASONS.includes(reason)) g.cacheMissReasons[reason] += 1;
    if (reason === 'none' && r.usage.cacheRead === 0) g.expiredBetweenTurns += 1;
  }
  for (const [key, g] of groups) {
    const list = lists.get(key) ?? [];
    const pick = (read: (r: RequestLog) => number | null) => list.map(read).filter((v): v is number => v !== null);
    g.histograms = { durationMs: histogramOf(pick((r) => r.durationMs)), outputTps: histogramOf(pick(outputTps)), ttfbMs: histogramOf(pick((r) => r.ttfbMs)), ttftMs: histogramOf(pick((r) => r.ttftMs)) };
    for (const m of METRICS) g.percentiles[m] = quartet(g.histograms[m]);
  }
  return [...groups.values()].sort((a, b) => b.requests - a.requests || a.key.localeCompare(b.key));
}

export function foldRequests(rows: RequestLog[], bucketMs: number, from: string, to: string, by: AnalyticsGroupBy = 'all', options: FoldOptions = {}): AnalyticsPoint[] {
  const end = Date.parse(to);
  const points: AnalyticsPoint[] = [];
  // A range ending on a bucket start owns no part of that bucket, which is where the series route stops too.
  for (let at = Math.floor(Date.parse(from) / bucketMs) * bucketMs; at < end; at += bucketMs) {
    const inBucket = rows.filter((r) => {
      const t = Date.parse(r.at);
      return t >= at && t < at + bucketMs;
    });
    points.push({ at: new Date(at).toISOString(), groups: foldGroups(inBucket, by, options) });
  }
  return points;
}

/** Every page of a session's requests, following the cursor to its end or to `maxPages`, which marks the result partial. */
export async function loadSessionRequests(load: (cursor: string | undefined) => Promise<RequestPage>, maxPages: number): Promise<{ partial: boolean; requests: RequestLog[] }> {
  const requests: RequestLog[] = [];
  let cursor: string | undefined;
  for (let page = 0; page < maxPages; page += 1) {
    const answer = await load(cursor);
    requests.push(...answer.requests);
    if (answer.nextCursor === null) return { partial: false, requests };
    cursor = answer.nextCursor;
  }
  return { partial: true, requests };
}

/** Several groups of one point summed into one, for the charts and tiles that draw totals. */
export function sumGroups(groups: AnalyticsGroup[]): AnalyticsGroup {
  const out = empty('all');
  for (const g of groups) {
    for (const k of ['attempts', 'cacheHits', 'cacheMisses', 'costUsd', 'exhausted', 'expiredBetweenTurns', 'failed', 'failovers', 'rateLimited', 'rejected', 'requests', 'succeeded', 'uncached'] as const) out[k] += g[k];
    for (const k of ['cacheRead', 'cacheWrite', 'input', 'output'] as const) out.cost[k] += g.cost[k];
    for (const k of ['cacheRead', 'cacheWrite1h', 'cacheWrite5m', 'output', 'thinking', 'uncachedInput', 'webFetch', 'webSearch'] as const) out.tokens[k] += g.tokens[k];
    for (const [code, n] of Object.entries(g.errors)) out.errors[code] = (out.errors[code] ?? 0) + n;
    for (const reason of CACHE_MISS_REASONS) out.cacheMissReasons[reason] += g.cacheMissReasons[reason];
    out.keptWarm.requests += g.keptWarm.requests;
    out.keptWarm.tokens += g.keptWarm.tokens;
    for (const k of ['failed', 'migrated', 'ok', 'skipped'] as const) out.probes[k] += g.probes[k];
    for (const k of ['grossUsd', 'probeCostUsd', 'wastedProbeUsd', 'wastedProbes'] as const) out.savings[k] += g.savings[k];
    for (const reason of WASTED_PROBE_REASONS) out.savings.wastedProbeReasons[reason] += g.savings.wastedProbeReasons[reason];
  }
  for (const m of METRICS) {
    out.histograms[m] = mergeHistograms(groups.map((g) => g.histograms[m]));
    out.percentiles[m] = quartet(out.histograms[m]);
  }
  return out;
}
