import type {
  AccountRollup, AnalyticsGroup, AnalyticsGroupBy, AnalyticsPoint, AnalyticsQuery, BurnGroup, BurnPoint, CacheMissReason, CostSplit, LatencyHistograms, PinMigrationReason, PlacementRow, RequestKind, RequestLog, RewritePoint,
  WastedProbeReason,
} from '@/api/types';
import { bucketShare } from '@/lib/analytics';
import { WASTED_PROBE_REASONS, emptyCacheMissReasons, emptyWastedProbeReasons, missCount } from '@/lib/cache';
import { shareOf, tierCapacity } from '@/lib/costs';
import { emptyCounts } from '@/lib/counts';
import { earlierHolderLabel, placementHit } from '@/lib/format';
import { emptyHistogram, mergeHistograms, percentileFromHistogram } from '@/lib/histogram';
import { matchesPattern } from '@/lib/pattern';
import { ACCOUNTS } from './accounts';
import { keyById } from './pools';
import { USER_AGENTS } from './requests';
import { TABLES } from './samples';
import { DAY, HOUR, iso } from './time';

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
  /** `held` counts the probes the router refused with `503 ledger_behind` while its ledger caught up, outside `failed`. */
  probes: { failed: number; held: number; ok: number; skipped: number };
  /** The class the lane's requests sent; the rollups file a row with none, a probe's among them, under `-`. */
  requestClass: string;
  requests: { incomplete: number; ok: number; overloaded: number; rate_limited: number; upstream_error: number };
  savings: { grossUsd: number; probeCostUsd: number };
  /** The fixture rolls up per session, which keeps its lanes as fine as the ledger's; no route groups on one. */
  sessionId: string;
  /** The speed the hour's answers reported, else `fast` where its requests asked for it, else `standard`. */
  speed: 'fast' | 'standard';
  tokens: { cacheRead: number; cacheWrite1h: number; cacheWrite5m: number; output: number; thinking: number; uncachedInput: number; webFetch: number; webSearch: number };
  uncached: number;
  /** Probes no real request came back for, settled when the recipe stopped or the request after them read no cache. */
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
  const incomplete = sum(counted, (r) => r.requests.incomplete);
  const succeeded = sum(counted, (r) => r.requests.ok);
  const requests = succeeded + rateLimited + overloaded + upstreamError + incomplete;
  const failovers = sum(counted, (r) => r.failovers);
  const errors = emptyCounts();
  if (rateLimited) errors.rate_limit_error = rateLimited;
  if (overloaded) errors.overloaded_error = overloaded;
  if (upstreamError) errors.api_error = upstreamError;
  if (incomplete) errors.stream_incomplete = incomplete;

  return {
    // Every attempt past the first on a request is a failover, so the two together are what the accounts saw.
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
    incomplete,
    keptWarm: { requests: sum(counted, (r) => r.keptWarm.requests), tokens: sum(counted, (r) => r.keptWarm.tokens) },
    key,
    keyCreatedAt: null,
    keyId: null,
    label,
    percentiles: { durationMs: quartet(histograms.durationMs), outputTps: quartet(histograms.outputTps), ttfbMs: quartet(histograms.ttfbMs), ttftMs: quartet(histograms.ttftMs) },
    probes: { failed: sum(probeRows, (r) => r.probes.failed), held: sum(probeRows, (r) => r.probes.held), migrated: sum(probeRows, (r) => r.probeMigrated), ok: sum(probeRows, (r) => r.probes.ok), skipped: sum(probeRows, (r) => r.probes.skipped) },
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

/**
 * The holder a key group names beside its label, as the route answers it: the id, and an earlier holder's generation
 * as its creation instant. The live key and the reserved id carry no instant, and every other dimension carries null.
 */
const holderOf = (key: string, groupBy: AnalyticsGroupBy): Pick<AnalyticsGroup, 'keyCreatedAt' | 'keyId'> => {
  if (groupBy !== 'key') return { keyCreatedAt: null, keyId: null };
  if (!key.includes('@')) return { keyCreatedAt: null, keyId: key };
  const [id, generation] = key.split('@');
  return { keyCreatedAt: Number(generation), keyId: id };
};

export function groupsOf(rows: HourlyRollup[], q: AnalyticsQuery): AnalyticsGroup[] {
  const named = q.userAgentGroups ?? [];
  // A pattern list names the groups under the client dimension; every client none of them names falls under
  // the one bucket, as the breakdown route files it.
  const of = q.groupBy === 'user_agent' && named.length ? (r: HourlyRollup) => named.find((p) => matchesPattern(p, USER_AGENTS[r.keyId] ?? '')) ?? 'other' : GROUP_KEY[q.groupBy];
  const buckets = new Map<string, HourlyRollup[]>();
  for (const r of rows) buckets.set(of(r), [...(buckets.get(of(r)) ?? []), r]);
  return [...buckets.entries()].map(([key, list]) => ({ ...groupOf(key, labelOf(key, q.groupBy), list, q.includeProbes ?? false), ...holderOf(key, q.groupBy) })).sort((a, b) => a.label.localeCompare(b.label) || a.key.localeCompare(b.key));
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

type Window = 'five_hour' | 'seven_day';

/**
 * A subscription row's share of each of its account's windows through the cost model, completing at the row's hour;
 * null for a tier or a model the model does not price.
 */
function sharesOf(r: HourlyRollup, tier: string | undefined): Record<Window, number | null> {
  const usage = { cacheRead: r.tokens.cacheRead, cacheWrite1h: r.tokens.cacheWrite1h, cacheWrite5m: r.tokens.cacheWrite5m, input: r.tokens.uncachedInput, output: r.tokens.output };
  const one = (window: Window) => (tier === undefined ? null : shareOf(TABLES, window, tier, r.model, usage, Date.parse(r.hour)));
  return { five_hour: one('five_hour'), seven_day: one('seven_day') };
}

/** A bucket's sums, each window's shares kept per row so one unpriced row nulls the window, as the route reads it. */
interface BurnSum {
  cached: number;
  fiveHour: (number | null)[];
  sevenDay: (number | null)[];
  tokens: number;
  uncached: number;
  usd: number;
}
const emptyBurn = (): BurnSum => ({ cached: 0, fiveHour: [], sevenDay: [], tokens: 0, uncached: 0, usd: 0 });

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
    // A fast answer spends usage credits in dollars on either kind and draws no plan window: on a tier the cost model
    // sizes it reads as a zero share, so the account is still a priced one.
    const fast = r.speed === 'fast';
    const group = groupOf(r);
    const byAt = sums.get(group) ?? new Map();
    sums.set(group, byAt);
    const s = byAt.get(at) ?? emptyBurn();
    byAt.set(at, s);
    const tokens = r.tokens.uncachedInput + r.tokens.cacheRead + r.tokens.cacheWrite5m + r.tokens.cacheWrite1h + r.tokens.output;
    s.cached += r.tokens.cacheRead;
    s.tokens += tokens;
    s.uncached += r.tokens.uncachedInput;
    s.usd += oauth && !fast ? 0 : r.costUsd;
    // A row with no token has nothing to price, as the route reads it: a failed request on a model the cost model has
    // no weight for never nulls a priced bucket.
    if (!oauth || tokens === 0) continue;
    if (fast) {
      // The zero rests on the tier alone, as the route reads a fast row, and never nulls a bucket.
      const tier = tiers.get(r.accountId);
      if (tier !== undefined && tierCapacity(TABLES, tier, 'five_hour') !== null) s.fiveHour.push(0);
      if (tier !== undefined && tierCapacity(TABLES, tier, 'seven_day') !== null) s.sevenDay.push(0);
      continue;
    }
    const shares = sharesOf(r, tiers.get(r.accountId));
    s.fiveHour.push(shares.five_hour);
    s.sevenDay.push(shares.seven_day);
  }
  const tokensOf = (group: string) => [...(sums.get(group)?.values() ?? [])].reduce((n, s) => n + s.cached + s.uncached, 0);
  let groups = [...sums.keys()].sort();
  if (groupBy === 'session') groups = [...groups].sort((a, b) => tokensOf(b) - tokensOf(a)).slice(0, BURN_SESSIONS).sort();
  // The route answers the shares and the unit under the account grouping alone.
  const byAccount = groupBy === 'account';
  const points = groups.flatMap((group) =>
    starts.map((at): BurnPoint => {
      const s = sums.get(group)?.get(at) ?? emptyBurn();
      const kind = byAccount ? kinds.get(group) : undefined;
      return {
        at: iso(at),
        cachedTokens: Math.round(s.cached),
        fiveHourShare: byAccount ? bucketShare(s.fiveHour) : null,
        group,
        sevenDayShare: byAccount ? bucketShare(s.sevenDay) : null,
        tokens: Math.round(s.tokens),
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
