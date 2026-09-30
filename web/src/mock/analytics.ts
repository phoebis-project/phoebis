import type {
  AccountRollup, AnalyticsGroup, AnalyticsGroupBy, AnalyticsPoint, AnalyticsQuery, AnalyticsReconciliations, BurnGroup, BurnPoint, CacheMissReason, CostSplit, LatencyHistograms, PinMigrationReason, PlacementRow, PlanModelCells, RequestKind, RequestLog, RewritePoint,
  TierFactor, WastedProbeReason,
} from '@/api/types';
import { WASTED_PROBE_REASONS, emptyCacheMissReasons, emptyWastedProbeReasons, missCount } from '@/lib/cache';
import { emptyCounts } from '@/lib/counts';
import { earlierHolderLabel, placementHit } from '@/lib/format';
import { emptyHistogram, mergeHistograms, percentileFromHistogram } from '@/lib/histogram';
import { matchesPattern } from '@/lib/pattern';
import { ACCOUNTS } from './accounts';
import { keyById } from './pools';
import { USER_AGENTS } from './requests';
import { SETTINGS_SAMPLE } from './samples';
import { DAY, HOUR, MINUTE, ago, iso } from './time';

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
  /** The class the router derived for the lane's agent; the rollups file a row with none under `-`. */
  client: string;
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
  /** The class the lane's requests sent; the rollups file a row with none, a probe's among them, under `-`. */
  requestClass: string;
  requests: { ok: number; overloaded: number; rate_limited: number; upstream_error: number };
  savings: { grossUsd: number; probeCostUsd: number };
  /** The fixture rolls up per session, which keeps its lanes as fine as the ledger's; no route groups on one. */
  sessionId: string;
  /** The speed the hour's answers reported; the rollups file an answer that named none as `standard`. */
  speed: 'fast' | 'standard';
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
  client: (r) => r.client,
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
  request_class: (r) => r.requestClass,
  speed: (r) => r.speed,
  // Each key runs one client in this story, so the key names the client its rows were sent by.
  user_agent: (r) => USER_AGENTS[r.keyId] ?? '-',
};

/**
 * The route's selector rule: `<id>@<generation>` names one lifetime, a bare id the live key's, and `-` the rows
 * the router never attributed. A bare id the story does not hold selects nothing, as the route answers it.
 */
function selectsKey(selector: string, r: Pick<HourlyRollup, 'keyGeneration' | 'keyId'>): boolean {
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
  if (q.clients?.length && !q.clients.some((p) => matchesPattern(p, r.client))) return false;
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
  for (const r of counted) for (const [reason, n] of Object.entries(r.cacheMissReasons)) cacheMissReasons[reason] = missCount(cacheMissReasons, reason) + n;
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
  const errors = emptyCounts();
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
const labelOf = (key: string, groupBy: AnalyticsGroupBy) => {
  if (key === '-') return 'unattributed';
  if (groupBy !== 'key' || !key.includes('@')) return key;
  const [id, generation] = key.split('@');
  return earlierHolderLabel(id, Number(generation));
};

export function groupsOf(rows: HourlyRollup[], q: AnalyticsQuery): AnalyticsGroup[] {
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

type Range = Pick<AnalyticsQuery, 'from' | 'to'>;

/** The bucket starts the analytics routes answer a range in, and each bucket's width. */
function bucketsOf(range: Range): { bucketMs: number; starts: number[] } {
  const fromMs = Date.parse(range.from);
  const toMs = Date.parse(range.to);
  const bucketMs = bucketMsFor(toMs - fromMs);
  const starts: number[] = [];
  for (let at = Math.floor(fromMs / bucketMs) * bucketMs; at < Math.ceil(toMs / HOUR) * HOUR; at += bucketMs) starts.push(at);
  return { bucketMs, starts };
}

/**
 * Three completed windows: one inside the band, the 5-hour window past it that the open `meter_drift` event of the
 * operations story names, and a 7-day one past it priced through unmeasured cells, so the table has ratios to mark
 * and a flag to show.
 */
export const RECONCILIATIONS: AnalyticsReconciliations = {
  reconciliations: [
    { accountId: 'business', computedAt: ago(12 * MINUTE), firstReadingAt: ago(5 * HOUR + 20 * MINUTE), lastReadingAt: ago(25 * MINUTE), observedRise: 0.24, predictedShare: 0.226, ratio: 0.24 / 0.226, requests: 61, unmeasured: false, window: 'five_hour', windowEnd: ago(20 * MINUTE) },
    { accountId: 'personal', computedAt: ago(12 * MINUTE), firstReadingAt: ago(7 * DAY + 30 * MINUTE), lastReadingAt: ago(40 * MINUTE), observedRise: 0.38, predictedShare: 0.27, ratio: 0.38 / 0.27, requests: 412, unmeasured: true, window: 'seven_day', windowEnd: ago(35 * MINUTE) },
    { accountId: 'personal', computedAt: ago(28 * MINUTE), firstReadingAt: ago(5 * HOUR + 30 * MINUTE), lastReadingAt: ago(45 * MINUTE), observedRise: 0.31, predictedShare: 0.22, ratio: 0.31 / 0.22, requests: 48, unmeasured: false, window: 'five_hour', windowEnd: ago(40 * MINUTE) },
  ],
  requestRetentionDays: 30,
};

const TABLES = SETTINGS_SAMPLE.planTables;
type Window = 'five_hour' | 'seven_day';

/**
 * The shipped cost table's row for a model, matched by the family its id names: the longest table id the model's
 * own id includes, so a dated id reads its undated row and Fable 5.1 does not read Fable 5's.
 */
function cellsOf(window: Window, model: string): PlanModelCells | undefined {
  const rows = TABLES.cost.windows[window] ?? {};
  const id = Object.keys(rows)
    .filter((k) => model.includes(k))
    .sort((a, b) => b.length - a.length)[0];
  return id === undefined ? undefined : rows[id];
}

/** The mock's tier spellings as the tier factor table keys them; the mock's `pro_5x` is the table's Pro row. */
function tierFactorOf(tier: string | undefined, window: Window): TierFactor | undefined {
  if (tier === undefined) return undefined;
  const row = TABLES.tiers.tiers[`default_claude_${tier === 'pro_5x' ? 'pro' : tier}`];
  return window === 'five_hour' ? row?.fiveHour : row?.sevenDay;
}

/** The in-force band row's factor on `window` at `at` when its hours cover it in its zone, else 1, as the route reads it. */
function bandAt(window: Window, at: number): number {
  const band = TABLES.cost.bands
    .filter((b) => b.windows.includes(window) && (b.effectiveFrom === null || Date.parse(b.effectiveFrom) <= at))
    .sort((a, b) => (b.effectiveFrom ?? '').localeCompare(a.effectiveFrom ?? ''))[0];
  if (band === undefined) return 1;
  const parts = new Intl.DateTimeFormat('en-US', { hour: 'numeric', hourCycle: 'h23', timeZone: band.zone, weekday: 'short' }).formatToParts(at);
  const hour = Number(parts.find((p) => p.type === 'hour')?.value);
  const weekend = ['Sat', 'Sun'].includes(parts.find((p) => p.type === 'weekday')?.value ?? '');
  return (!band.weekdays || !weekend) && band.fromHour <= hour && hour < band.toHour ? band.factor : 1;
}

/**
 * A subscription row's share of each of its account's windows: the table's cells for its model over the tier's
 * factor, the 5-hour share weighed by the band at the row's hour; null for a tier or a model the tables lack.
 */
function sharesOf(r: HourlyRollup, tier: string | undefined): Record<Window, { share: number; unmeasured: boolean } | null> {
  const one = (window: Window) => {
    const cells = cellsOf(window, r.model);
    const factor = tierFactorOf(tier, window);
    if (cells === undefined || factor === undefined) return null;
    const read = [
      [r.tokens.uncachedInput, cells.input],
      [r.tokens.cacheRead, cells.cacheRead],
      [r.tokens.cacheWrite5m, cells.cacheWrite5m],
      [r.tokens.cacheWrite1h, cells.cacheWrite1h],
      [r.tokens.output, cells.output],
    ] as const;
    const onMax5x = read.reduce((n, [tokens, cell]) => n + (tokens * cell.sharePerMillion) / 1_000_000, 0);
    const band = window === 'five_hour' ? bandAt(window, Date.parse(r.hour)) : 1;
    return { share: (onMax5x / factor.factor) * band, unmeasured: factor.unmeasured || read.some(([tokens, cell]) => tokens > 0 && cell.unmeasured) };
  };
  return { five_hour: one('five_hour'), seven_day: one('seven_day') };
}

interface BurnSum {
  cached: number;
  fiveHour: number | null;
  fiveHourUnmeasured: boolean;
  sevenDay: number | null;
  sevenDayUnmeasured: boolean;
  uncached: number;
  usd: number;
}
const emptyBurn = (): BurnSum => ({ cached: 0, fiveHour: null, fiveHourUnmeasured: false, sevenDay: null, sevenDayUnmeasured: false, uncached: 0, usd: 0 });

/** How many sessions the session grouping keeps, the route's cut. */
const BURN_SESSIONS = 20;

export function burnOf(rows: HourlyRollup[], range: Range, groupBy: BurnGroup): { bucketSeconds: number; points: BurnPoint[] } {
  const { bucketMs, starts } = bucketsOf(range);
  const kinds = new Map(ACCOUNTS.map((a) => [a.id, a.kind]));
  const tiers = new Map(ACCOUNTS.map((a) => [a.id, a.subscription?.rateLimitTier]));
  const first = starts[0] ?? 0;
  const end = (starts[starts.length - 1] ?? 0) + bucketMs;
  // The route prices a row by its account's kind and skips a row whose account left the registry.
  const kept = rows.filter((r) => Date.parse(r.hour) >= first && Date.parse(r.hour) < end && kinds.has(r.accountId));
  const groupOf = (r: HourlyRollup) => (groupBy === 'account' ? r.accountId : groupBy === 'key' ? r.keyId : groupBy === 'session' ? r.sessionId : r.requestClass);
  const sums = new Map<string, Map<number, BurnSum>>();
  for (const r of kept) {
    const at = Math.floor((Date.parse(r.hour) - first) / bucketMs) * bucketMs + first;
    const oauth = kinds.get(r.accountId) === 'oauth';
    // A fast answer spends usage credits in dollars on either kind and draws no plan window: on a tier the tables
    // size it reads as a zero share, so the upstream is still a priced one.
    const fast = r.speed === 'fast';
    const group = groupOf(r);
    const byAt = sums.get(group) ?? new Map();
    sums.set(group, byAt);
    const s = byAt.get(at) ?? emptyBurn();
    byAt.set(at, s);
    s.cached += r.tokens.cacheRead;
    s.uncached += r.tokens.uncachedInput;
    s.usd += oauth && !fast ? 0 : r.costUsd;
    if (!oauth) continue;
    const shares = sharesOf(r, tiers.get(r.accountId));
    if (shares.five_hour !== null) {
      s.fiveHour = (s.fiveHour ?? 0) + (fast ? 0 : shares.five_hour.share);
      s.fiveHourUnmeasured ||= !fast && shares.five_hour.unmeasured;
    }
    if (shares.seven_day !== null) {
      s.sevenDay = (s.sevenDay ?? 0) + (fast ? 0 : shares.seven_day.share);
      s.sevenDayUnmeasured ||= !fast && shares.seven_day.unmeasured;
    }
  }
  const tokensOf = (group: string) => [...(sums.get(group)?.values() ?? [])].reduce((n, s) => n + s.cached + s.uncached, 0);
  let groups = [...sums.keys()].sort();
  if (groupBy === 'session') groups = [...groups].sort((a, b) => tokensOf(b) - tokensOf(a)).slice(0, BURN_SESSIONS).sort();
  // The route answers the shares, their flags and the unit under the upstream grouping alone.
  const byUpstream = groupBy === 'account';
  const points = groups.flatMap((group) =>
    starts.map((at): BurnPoint => {
      const s = sums.get(group)?.get(at) ?? emptyBurn();
      const kind = byUpstream ? kinds.get(group) : undefined;
      return {
        at: iso(at),
        cachedTokens: Math.round(s.cached),
        fiveHourShare: byUpstream ? s.fiveHour : null,
        fiveHourUnmeasured: byUpstream && s.fiveHourUnmeasured,
        group,
        sevenDayShare: byUpstream ? s.sevenDay : null,
        sevenDayUnmeasured: byUpstream && s.sevenDayUnmeasured,
        uncachedTokens: Math.round(s.uncached),
        unit: kind === undefined ? null : kind === 'oauth' ? 'window_share' : 'usd',
        usd: s.usd,
      };
    }),
  );
  return { bucketSeconds: bucketMs / 1_000, points };
}

/** The reason each hour's moves are filed under, in turn, weighted toward the failover and the shedding round. */
const MOVE_TURNS: PinMigrationReason[] = ['cooldown', 'shed', 'cooldown', 'spent', 'shed', 'overage', 'cooldown', 'return_to_plan', 'drain', 'mark', 'fast_lane'];

/** The account rollups' moves into each account, with the cache the first request after each one wrote again. */
export function rewritesOf(rollups: AccountRollup[], range: Range): { moves: Record<string, number>; points: RewritePoint[] } {
  const { bucketMs, starts } = bucketsOf(range);
  const moves: Record<string, number> = {};
  const points = starts.map((at): RewritePoint => {
    let rewriteTokens = 0;
    for (const r of rollups) {
      const hour = Date.parse(r.hour);
      if (hour < at || hour >= at + bucketMs || r.pins.migratedIn === 0) continue;
      const turn = Math.floor(hour / HOUR);
      rewriteTokens += r.pins.migratedIn * (18_000 + (turn % 5) * 9_000);
      const reason = MOVE_TURNS[turn % MOVE_TURNS.length];
      moves[reason] = (moves[reason] ?? 0) + r.pins.migratedIn;
    }
    return { at: iso(at), rewriteTokens };
  });
  return { moves, points };
}

/**
 * Placements by rung off the request rows' traces, as the route counts them: each attempt that placed and carried
 * usage, a hit by the rule the request drawer reads its cache line by. The route narrows by the range and the key
 * and pool lists here.
 */
export function placementsOf(requests: RequestLog[], q: Pick<AnalyticsQuery, 'from' | 'keyIds' | 'poolIds' | 'to'>): PlacementRow[] {
  const fromMs = Date.parse(q.from);
  const toMs = Date.parse(q.to);
  const byRung = new Map<string, Omit<PlacementRow, 'hitShare'>>();
  for (const r of requests) {
    const at = Date.parse(r.at);
    if (at < fromMs || at > toMs) continue;
    if (q.keyIds?.length && !q.keyIds.some((selector) => selectsKey(selector, { keyGeneration: r.keyGeneration, keyId: r.keyId ?? '-' }))) continue;
    if (q.poolIds?.length && !q.poolIds.includes(r.poolId ?? '-')) continue;
    for (const a of r.attempts) {
      const placement = a.selection?.placement ?? null;
      const read = a.selection?.cacheRead ?? null;
      if (placement === null || read === null) continue;
      const row = byRung.get(placement.rung) ?? { cacheReadTokens: 0, hits: 0, placements: 0, promisedTokens: 0, rung: placement.rung };
      row.placements += 1;
      if (placementHit(placement, read)) row.hits += 1;
      row.cacheReadTokens += read;
      row.promisedTokens += placement.prefixTokens;
      byRung.set(placement.rung, row);
    }
  }
  return [...byRung.values()].sort((a, b) => a.rung.localeCompare(b.rung)).map((row) => ({ ...row, hitShare: row.placements > 0 ? row.hits / row.placements : null }));
}
