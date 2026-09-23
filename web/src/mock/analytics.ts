import type { AnalyticsGroup, AnalyticsGroupBy, AnalyticsPoint, AnalyticsQuery, CacheMissReason, CostSplit, LatencyHistograms, RequestKind, WastedProbeReason } from '@/api/types';
import { WASTED_PROBE_REASONS, emptyCacheMissReasons, emptyWastedProbeReasons } from '@/lib/cache';
import { emptyHistogram, mergeHistograms, percentileFromHistogram } from '@/lib/histogram';
import { matchesPattern } from '@/lib/pattern';
import { keyById } from './pools';
import { USER_AGENTS } from './requests';
import { DAY, HOUR } from './time';

/**
 * One hour of one lane of the fixture ledger. The admin routes answer buckets and groups rather than rows,
 * so this shape is the mock's own raw material for the fold the router does server-side.
 */
export interface HourlyRollup {
  accountId: string;
  cacheHits: number;
  cacheMisses: number;
  /** Attempts that carried diagnostics, counted by what they reported. */
  cacheMissReasons: Record<CacheMissReason, number>;
  cost: CostSplit;
  costUsd: number;
  /** The effort the hour's requests asked for; the rollups file an hour that asked for none under `none`. */
  effort: string;
  /**
   * Requests that diagnosed no divergence yet read no cache, so the entry
   * expired between turns. The reason counts cannot be split this way once
   * summed, which is why the ledger carries it.
   */
  expiredBetweenTurns: number;
  /** Attempts that moved to another member before the request succeeded. */
  failovers: number;
  histograms: LatencyHistograms;
  hour: string;
  /** Requests whose cache read was kept alive by a probe, and the tokens they read. */
  keptWarm: { requests: number; tokens: number };
  keyId: string;
  /** The `created_at` of the key the row belongs to; the live key's for a current row, an earlier one's for a deleted holder's. */
  keyGeneration: number;
  kind: RequestKind;
  /** Session pins moved to another member during the hour. */
  migrations: number;
  model: string;
  poolId: string;
  /** Probes served by another account than the recipe's; they count for neither the cold streak nor the net. */
  probeMigrated: number;
  probes: { failed: number; ok: number; skipped: number };
  requests: { ok: number; overloaded: number; rate_limited: number; upstream_error: number };
  savings: { grossUsd: number; probeCostUsd: number };
  /** The fixture rolls up per session, which keeps its lanes as fine as the ledger's; no route groups on one. */
  sessionId: string;
  tokens: { cacheRead: number; cacheWrite1h: number; cacheWrite5m: number; output: number; thinking: number; uncachedInput: number; webFetch: number; webSearch: number };
  uncached: number;
  /** Probes no real request ever followed, settled when the recipe stopped. */
  wastedProbes: number;
  wastedProbeReasons: Record<WastedProbeReason, number>;
  wastedProbeUsd: number;
}

/** The widths the analytics route buckets a range at, so a mock point covers the same hours the router's does. */
const bucketMsFor = (spanMs: number) => (spanMs <= 48 * HOUR ? HOUR : spanMs <= 30 * DAY ? 6 * HOUR : DAY);

const GROUP_KEY: Record<AnalyticsGroupBy, (r: HourlyRollup) => string> = {
  account: (r) => r.accountId,
  all: () => 'all',
  effort: (r) => r.effort,
  key: (r) => {
    // What the router never attributed is one group whatever the generation column says.
    if (r.keyId === '-') return r.keyId;
    const live = keyById.get(r.keyId);
    return live && Date.parse(live.createdAt) === r.keyGeneration ? r.keyId : `${r.keyId}@${r.keyGeneration}`;
  },
  kind: (r) => r.kind,
  model: (r) => r.model,
  pool: (r) => r.poolId,
  // Each key runs one client in this story, so the key names the client its rows were sent by.
  user_agent: (r) => USER_AGENTS[r.keyId] ?? '-',
};

/**
 * The route's selector rule: `<id>@<generation>` names one lifetime, a bare id the live key's, and `-` the rows
 * the router never attributed. A bare id the story does not hold selects nothing, as the route answers it.
 */
function selectsKey(selector: string, r: HourlyRollup): boolean {
  if (selector === '-') return r.keyId === '-';
  if (selector.includes('@')) return `${r.keyId}@${r.keyGeneration}` === selector;
  const live = keyById.get(selector);
  return live !== undefined && r.keyId === selector && Date.parse(live.createdAt) === r.keyGeneration;
}

const METRICS: (keyof LatencyHistograms)[] = ['durationMs', 'outputTps', 'ttfbMs', 'ttftMs'];
const PERCENTILES = { p50: 0.5, p90: 0.9, p95: 0.95, p99: 0.99 };
const quartet = (h: readonly number[]) => ({ p50: percentileFromHistogram(h, PERCENTILES.p50), p90: percentileFromHistogram(h, PERCENTILES.p90), p95: percentileFromHistogram(h, PERCENTILES.p95), p99: percentileFromHistogram(h, PERCENTILES.p99) });

function inQuery(r: HourlyRollup, q: AnalyticsQuery, fromMs: number, toMs: number): boolean {
  const hour = Date.parse(r.hour);
  if (hour < fromMs || hour > toMs) return false;
  if (q.accountIds?.length && !q.accountIds.includes(r.accountId)) return false;
  if (q.keyIds?.length && !q.keyIds.some((selector) => selectsKey(selector, r))) return false;
  if (q.kinds?.length && !q.kinds.includes(r.kind)) return false;
  if (q.models?.length && !q.models.includes(r.model)) return false;
  if (q.poolIds?.length && !q.poolIds.includes(r.poolId)) return false;
  if (q.userAgents?.length && !q.userAgents.some((p) => matchesPattern(p, USER_AGENTS[r.keyId] ?? ''))) return false;
  return true;
}

/**
 * Two sums per group, the way the route splits them: the request side, which a probe row joins only when probes
 * count as requests, and the probe rows alone, which the probe counts and the probe half of the savings read.
 */
function groupOf(key: string, label: string, rows: HourlyRollup[], includeProbes: boolean): AnalyticsGroup {
  const counted = includeProbes ? rows : rows.filter((r) => r.kind !== 'probe');
  const probeRows = rows.filter((r) => r.kind === 'probe');
  const sum = (list: HourlyRollup[], read: (r: HourlyRollup) => number) => list.reduce((n, r) => n + read(r), 0);

  const cacheMissReasons = emptyCacheMissReasons();
  // Every name a row carries, as the ledger rolls up whatever the beta sent, not only the ones the console knows.
  for (const r of counted) for (const [reason, n] of Object.entries(r.cacheMissReasons)) cacheMissReasons[reason] = (cacheMissReasons[reason] ?? 0) + n;
  const wastedProbeReasons = emptyWastedProbeReasons();
  for (const r of probeRows) for (const reason of WASTED_PROBE_REASONS) wastedProbeReasons[reason] += r.wastedProbeReasons[reason];

  const histograms = { durationMs: emptyHistogram(), outputTps: emptyHistogram(), ttfbMs: emptyHistogram(), ttftMs: emptyHistogram() };
  for (const m of METRICS) histograms[m] = mergeHistograms(counted.map((r) => r.histograms[m]));

  const rateLimited = sum(counted, (r) => r.requests.rate_limited);
  const overloaded = sum(counted, (r) => r.requests.overloaded);
  const upstreamError = sum(counted, (r) => r.requests.upstream_error);
  const succeeded = sum(counted, (r) => r.requests.ok);
  const requests = succeeded + rateLimited + overloaded + upstreamError;
  const failovers = sum(counted, (r) => r.failovers);
  const errors: Record<string, number> = {};
  if (rateLimited) errors.rate_limit_error = rateLimited;
  if (overloaded) errors.overloaded_error = overloaded;
  if (upstreamError) errors.api_error = upstreamError;

  return {
    // Every attempt past the first on a request is a failover, so the two together are what the upstreams saw.
    attempts: requests + failovers,
    cacheHits: sum(counted, (r) => r.cacheHits),
    cacheMisses: sum(counted, (r) => r.cacheMisses),
    cacheMissReasons,
    cost: { cacheRead: sum(counted, (r) => r.cost.cacheRead), cacheWrite: sum(counted, (r) => r.cost.cacheWrite), input: sum(counted, (r) => r.cost.input), output: sum(counted, (r) => r.cost.output) },
    costUsd: sum(counted, (r) => r.costUsd),
    errors,
    // The fixture ledger records neither an exhausted pool nor a rejected request.
    exhausted: 0,
    expiredBetweenTurns: sum(counted, (r) => r.expiredBetweenTurns),
    failed: overloaded + upstreamError,
    failovers,
    histograms,
    keptWarm: { requests: sum(counted, (r) => r.keptWarm.requests), tokens: sum(counted, (r) => r.keptWarm.tokens) },
    key,
    label,
    percentiles: { durationMs: quartet(histograms.durationMs), outputTps: quartet(histograms.outputTps), ttfbMs: quartet(histograms.ttfbMs), ttftMs: quartet(histograms.ttftMs) },
    probes: { failed: sum(probeRows, (r) => r.probes.failed), migrated: sum(probeRows, (r) => r.probeMigrated), ok: sum(probeRows, (r) => r.probes.ok), skipped: sum(probeRows, (r) => r.probes.skipped) },
    rateLimited,
    rejected: 0,
    requests,
    savings: {
      // The gross saving rides on the inference rows and what the cache cost to keep warm on the probe rows.
      grossUsd: sum(counted, (r) => r.savings.grossUsd),
      probeCostUsd: sum(probeRows, (r) => r.savings.probeCostUsd),
      wastedProbeReasons,
      wastedProbeUsd: sum(probeRows, (r) => r.wastedProbeUsd),
      wastedProbes: sum(probeRows, (r) => r.wastedProbes),
    },
    succeeded,
    tokens: {
      cacheRead: sum(counted, (r) => r.tokens.cacheRead), cacheWrite1h: sum(counted, (r) => r.tokens.cacheWrite1h), cacheWrite5m: sum(counted, (r) => r.tokens.cacheWrite5m), output: sum(counted, (r) => r.tokens.output),
      thinking: sum(counted, (r) => r.tokens.thinking), uncachedInput: sum(counted, (r) => r.tokens.uncachedInput), webFetch: sum(counted, (r) => r.tokens.webFetch), webSearch: sum(counted, (r) => r.tokens.webSearch),
    },
    uncached: sum(counted, (r) => r.uncached),
  };
}

/** The key dimension alone carries a generation in its group key; the reserved id reads out in every dimension. */
const labelOf = (key: string, groupBy: AnalyticsGroupBy) => (key === '-' ? 'unattributed' : groupBy === 'key' && key.includes('@') ? `${key.split('@')[0]} (deleted)` : key);

function groupsOf(rows: HourlyRollup[], q: AnalyticsQuery): AnalyticsGroup[] {
  const named = q.userAgentGroups ?? [];
  // A pattern list names the groups under the client dimension; every client none of them names falls under
  // the one bucket, as the breakdown route files it.
  const of = q.groupBy === 'user_agent' && named.length ? (r: HourlyRollup) => named.find((p) => matchesPattern(p, USER_AGENTS[r.keyId] ?? '')) ?? 'other' : GROUP_KEY[q.groupBy];
  const buckets = new Map<string, HourlyRollup[]>();
  for (const r of rows) buckets.set(of(r), [...(buckets.get(of(r)) ?? []), r]);
  return [...buckets.entries()].map(([key, list]) => groupOf(key, labelOf(key, q.groupBy), list, q.includeProbes ?? false)).sort((a, b) => a.label.localeCompare(b.label) || a.key.localeCompare(b.key));
}

export function breakdownOf(rows: HourlyRollup[], q: AnalyticsQuery): AnalyticsGroup[] {
  const fromMs = Date.parse(q.from);
  const toMs = Date.parse(q.to);
  return groupsOf(
    rows.filter((r) => inQuery(r, q, fromMs, toMs)),
    q,
  );
}

export function seriesOf(rows: HourlyRollup[], q: AnalyticsQuery): AnalyticsPoint[] {
  const fromMs = Date.parse(q.from);
  const toMs = Date.parse(q.to);
  const bucketMs = bucketMsFor(toMs - fromMs);
  const kept = rows.filter((r) => inQuery(r, q, fromMs, toMs));
  const points: AnalyticsPoint[] = [];
  // Buckets start on a whole multiple of their own width and run to the hour after the range ends, as the route's do.
  for (let at = Math.floor(fromMs / bucketMs) * bucketMs; at < Math.ceil(toMs / HOUR) * HOUR; at += bucketMs) {
    const inBucket = kept.filter((r) => Date.parse(r.hour) >= at && Date.parse(r.hour) < at + bucketMs);
    points.push({ at: new Date(at).toISOString(), groups: groupsOf(inBucket, q) });
  }
  return points;
}
