import type {
  AccountRollup, AnalyticsGroup, AnalyticsGroupBy, AnalyticsPoint, AnalyticsQuery, BurnGroup, BurnPoint, CacheMissReason, CostSplit, HeldPoint, LatencyHistograms, PinMigrationReason, PlacementRow, Reconciliation, ReconciliationsAnswer, RequestKind, RequestLog,
  RewritePoint, RewritesAnswer, WastedProbeReason,
} from '@/api/types';
import { bucketShare } from '@/lib/analytics';
import { WASTED_PROBE_REASONS, emptyCacheMissReasons, emptyWastedProbeReasons, missCount } from '@/lib/cache';
import { equivalentOf, modelWeeks, pricedWindow, shareOrFallback, tierCapacity, type Usage } from '@/lib/costs';
import { emptyCounts } from '@/lib/counts';
import { earlierHolderLabel, placementHit } from '@/lib/format';
import { emptyHistogram, mergeHistograms, percentileFromHistogram } from '@/lib/histogram';
import { matchesPattern } from '@/lib/pattern';
import { ACCOUNTS } from './accounts';
import { keyById } from './pools';
import { USER_AGENTS, clock } from './requests';
import { TABLES } from './samples';
import { DAY, HOUR, MINUTE, NOW, iso } from './time';

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
  /** `client` for the client's own auto-mode classifier calls, which take no share of a plan window, else `-`. */
  classifier: string;
  /** The class the router derived for the lane's agent; the rollups file a row with none under `-`. */
  client: string;
  cost: CostSplit;
  costUsd: number;
  /** The effort the hour's requests asked for; the rollups file an hour that asked for none under `none`. */
  effort: string;
  /** The hour's 20x window equivalents, priced and summed as `shares` are, under no account tier. */
  equivalents: Record<string, number | null>;
  /** True when a request of the hour priced at the model tier's fallback weight. */
  estimated: boolean;
  /**
   * Requests that diagnosed no divergence yet read no cache, so the entry
   * expired between turns. The reason counts cannot be split this way once
   * summed, which is why the ledger carries it.
   */
  expiredBetweenTurns: number;
  /** Attempts that moved to another member before the request succeeded. */
  failovers: number;
  /**
   * Who paid for the hour's requests, as the ledger keys its rollups off the served answer: `billed` for an API key
   * account's answer whatever its speed, else a fast answer's credits, else the head's overage, else the plan, which a
   * request no attempt served files under too.
   */
  funding: 'billed' | 'fast' | 'overage' | 'plan';
  histograms: LatencyHistograms;
  hour: string;
  /** Requests whose cache read was kept alive by a probe, and the tokens they read under each TTL. */
  keptWarm: { requests: number; tokens1h: number; tokens5m: number };
  keyId: string;
  /** The `created_at` of the key the row belongs to; the live key's for a current row, an earlier one's for a deleted holder's. */
  keyGeneration: number;
  kind: RequestKind;
  /** Session pins moved to another member during the hour. */
  migrations: number;
  model: string;
  poolId: string;
  probeCostUsd: number;
  /** Probes served by another account than the recipe's; they do not count toward the cold streak. */
  probeMigrated: number;
  /** `held` counts the probes the router refused with `503 ledger_behind` while its ledger caught up, outside `failed`. */
  probes: { failed: number; held: number; ok: number; skipped: number };
  /** The class the lane's requests sent; the rollups file a row with none, a probe's among them, under `-`. */
  requestClass: string;
  /** `client_gone` counts the streams the client closed, which the relay files as failed under that code. */
  requests: { client_gone: number; incomplete: number; ok: number; overloaded: number; rate_limited: number; undecodable: number; upstream_error: number };
  /** The kept-warm requests' saving in dollars, each priced at the cells its own prompt selects. */
  savingUsd: number;
  /** The fixture rolls up per session, which keeps its lanes as fine as the ledger's; no route groups on one. */
  sessionId: string;
  /**
   * The hour's plan-window shares, each request priced at its own band and completion and summed. A window any request
   * prices null reads null.
   */
  shares: Record<string, number | null>;
  /** The speed the hour's answers reported, else `fast` where its requests asked for it, else `standard`. */
  speed: 'fast' | 'standard';
  /** The account tier the hour's requests landed under, null for an API key account. */
  tier: string | null;
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
  classifier: (r) => r.classifier,
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

const KIND_OF = new Map(ACCOUNTS.map((a) => [a.id, a.kind]));
const TIER_OF = new Map(ACCOUNTS.map((a) => [a.id, a.subscription?.rateLimitTier]));

/**
 * Whether a row falls on the side the funding filter keeps, as the route reads it off the row's own funding: `plan` is
 * a plan-funded row, `billed` its complement, and the registry's current kinds move no row.
 */
function passesFunding(r: HourlyRollup, funding: AnalyticsQuery['funding']): boolean {
  if (funding === undefined) return true;
  return funding === 'plan' ? r.funding === 'plan' : r.funding !== 'plan';
}

function inQuery(r: HourlyRollup, q: AnalyticsQuery, fromMs: number, toMs: number): boolean {
  const hour = Date.parse(r.hour);
  if (hour < fromMs || hour > toMs) return false;
  if (!passesFunding(r, q.funding)) return false;
  if (q.accountIds?.length && !q.accountIds.includes(r.accountId)) return false;
  if (q.keyIds?.length && !q.keyIds.some((selector) => selectsKey(selector, r))) return false;
  if (q.kinds?.length && !q.kinds.includes(r.kind)) return false;
  if (q.models?.length && !q.models.includes(r.model)) return false;
  if (q.poolIds?.length && !q.poolIds.includes(r.poolId)) return false;
  if (q.userAgents?.length && !q.userAgents.some((p) => matchesPattern(p, USER_AGENTS[r.keyId] ?? ''))) return false;
  if (q.clients?.length && !q.clients.some((p) => matchesPattern(p, r.client))) return false;
  return true;
}

/** One window's running share and whether a row could not be priced on it, which drops it from the answer. */
interface WindowSum {
  sum: number;
  unpriced: boolean;
}

/** The four share members of a group, folded from its plan-funded rows or all null. */
type GroupShares = Pick<AnalyticsGroup, 'cacheShares' | 'costShares' | 'probeShares' | 'savingShares'>;

const NO_SHARES: GroupShares = { cacheShares: null, costShares: null, probeShares: null, savingShares: null };

const tokensOfRow = (r: HourlyRollup) => r.tokens.uncachedInput + r.tokens.cacheRead + r.tokens.cacheWrite5m + r.tokens.cacheWrite1h + r.tokens.output;
const planRows = (list: HourlyRollup[]) => list.filter((r) => r.funding === 'plan');

/** A group's window sums over its plan-funded rows: `own` the row's figure per window, its requests priced one by one,
 *  and `cache` its read and write figures, which have no figure per request on the row and price at its hour. */
function windowSums(counted: HourlyRollup[], probeRows: HourlyRollup[], own: (r: HourlyRollup) => Record<string, number | null>, cache: (r: HourlyRollup) => Record<'read' | 'write', Record<string, number | null>>) {
  const cost = new Map<string, WindowSum>();
  const read = new Map<string, WindowSum>();
  const write = new Map<string, WindowSum>();
  const probe = new Map<string, WindowSum>();
  const add = (into: Map<string, WindowSum>, window: string, figure: number | null) => {
    const s = into.get(window) ?? { sum: 0, unpriced: false };
    if (figure === null) s.unpriced = true;
    else s.sum += figure;
    into.set(window, s);
  };
  for (const r of planRows(counted)) {
    if (tokensOfRow(r) === 0) continue;
    const c = cache(r);
    for (const [window, figure] of Object.entries(own(r))) add(cost, window, figure);
    for (const [window, figure] of Object.entries(c.read)) add(read, window, figure);
    for (const [window, figure] of Object.entries(c.write)) add(write, window, figure);
  }
  let probeUsd = 0;
  let wastedUsd = 0;
  for (const r of planRows(probeRows)) {
    probeUsd += r.probeCostUsd;
    wastedUsd += r.wastedProbeUsd;
    if (tokensOfRow(r) === 0) continue;
    for (const [window, figure] of Object.entries(own(r))) add(probe, window, figure);
  }
  const priced = (map: Map<string, WindowSum>) => [...map.entries()].filter(([, s]) => !s.unpriced);
  return {
    cache: Object.fromEntries(priced(read).flatMap(([window, r]) => { const w = write.get(window); return w === undefined || w.unpriced ? [] : [[window, { read: r.sum, write: w.sum }]]; })) as Record<string, { read: number; write: number }>,
    cost: Object.fromEntries(priced(cost).map(([window, s]) => [window, s.sum])) as Record<string, number>,
    probe: Object.fromEntries(priced(probe).map(([window, s]) => [window, s.sum])) as Record<string, number>,
    ratio: probeUsd > 0 ? wastedUsd / probeUsd : null,
  };
}

/**
 * A subscription account's shares of its windows over its plan-funded rows, summed as the route sums a group: the
 * cost and cache shares over the rows the Include probes rule admits, the probe share over the probe rows apart, and
 * the wasted share the probe share times the group's wasted-probe dollars over its probe-cost dollars, the
 * plan-funded probe rows alone on both sides. A window any row prices null is left out of all three maps, as the
 * route answers them (a request row keeps such a window as a key with null), and a row with no token prices nothing.
 */
function sharesOfGroup(counted: HourlyRollup[], probeRows: HourlyRollup[], tier: string): GroupShares {
  const s = windowSums(counted, probeRows, (r) => r.shares, (r) => sharesOf(r, tier));
  return {
    cacheShares: s.cache,
    costShares: s.cost,
    probeShares: Object.fromEntries(Object.entries(s.probe).map(([window, cost]) => [window, { cost, wasted: s.ratio === null ? null : cost * s.ratio }])),
    savingShares: savingOf(counted, (r) => sharesOf(r, tier, keptUsageOf(r))),
  };
}

/** A row's kept-warm counts as the usage their saving prices: each count the write it avoided at its TTL, and the two
 *  together the read the requests paid instead. */
const keptUsageOf = (r: HourlyRollup): Usage => ({ cacheRead: r.keptWarm.tokens1h + r.keptWarm.tokens5m, cacheWrite1h: r.keptWarm.tokens1h, cacheWrite5m: r.keptWarm.tokens5m, input: 0, output: 0 });

/**
 * The kept-warm saving per window over a group's plan-funded rows, the write half of each row's kept-warm counts less
 * its read half (`halves`), at the row's hour. A window any credit prices null is left out, as the route answers it.
 */
function savingOf(counted: HourlyRollup[], halves: (r: HourlyRollup) => Record<'read' | 'write', Record<string, number | null>>): Record<string, number> {
  const sums = new Map<string, WindowSum>();
  for (const r of planRows(counted)) {
    if (r.keptWarm.tokens1h + r.keptWarm.tokens5m === 0) continue;
    const { read, write } = halves(r);
    for (const [window, paid] of Object.entries(read)) {
      const avoided = write[window] ?? null;
      const s = sums.get(window) ?? { sum: 0, unpriced: false };
      if (paid === null || avoided === null) s.unpriced = true;
      else s.sum += avoided - paid;
      sums.set(window, s);
    }
  }
  return Object.fromEntries([...sums.entries()].filter(([, s]) => !s.unpriced).map(([window, s]) => [window, s.sum]));
}

const TOKEN_KINDS: Record<keyof CostSplit, (u: Usage) => Usage> = {
  cacheRead: (u) => ({ cacheRead: u.cacheRead, cacheWrite1h: 0, cacheWrite5m: 0, input: 0, output: 0 }),
  cacheWrite: (u) => ({ cacheRead: 0, cacheWrite1h: u.cacheWrite1h, cacheWrite5m: u.cacheWrite5m, input: 0, output: 0 }),
  input: (u) => ({ cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: 0, input: u.input, output: 0 }),
  output: (u) => ({ cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: 0, input: 0, output: u.output }),
};

/**
 * The group's 20x window equivalents over its plan-funded rows, summed as the shares are but under no account tier,
 * so they add across accounts and read under every grouping, with the 7-day equivalent split by token kind at each
 * row's hour and the wasted probes' equivalent at the probe share's ratio.
 */
function equivalentsOfGroup(counted: HourlyRollup[], probeRows: HourlyRollup[]): Pick<AnalyticsGroup, 'cacheEquivalent' | 'costEquivalent' | 'equivalentSplit' | 'estimated' | 'probeEquivalent' | 'savingEquivalent'> {
  const s = windowSums(counted, probeRows, (r) => r.equivalents, equivalentsOf);
  const split: CostSplit = { cacheRead: 0, cacheWrite: 0, input: 0, output: 0 };
  for (const r of planRows(counted)) {
    // A client classifier call's split reads zero, as its equivalents do.
    if (tokensOfRow(r) === 0 || r.classifier === 'client') continue;
    for (const kind of Object.keys(TOKEN_KINDS) as (keyof CostSplit)[]) split[kind] += equivalentOf(TABLES, 'seven_day', r.model, TOKEN_KINDS[kind](usageOf(r)), Date.parse(r.hour))?.equivalent ?? 0;
  }
  return {
    cacheEquivalent: s.cache,
    costEquivalent: s.cost,
    equivalentSplit: split,
    estimated: [...planRows(counted), ...planRows(probeRows)].some((r) => r.estimated),
    probeEquivalent: Object.fromEntries(Object.entries(s.probe).map(([window, cost]) => [window, { cost, wasted: s.ratio === null ? 0 : cost * s.ratio }])),
    savingEquivalent: savingOf(counted, (r) => equivalentsOf(r, keptUsageOf(r))),
  };
}

/**
 * The account a group's shares price under: the group's own under the account grouping, else the one account the
 * query names, as the route's gate reads it, since shares of different accounts' windows do not add. Null otherwise.
 */
const shareAccountOf = (key: string, groupBy: AnalyticsGroupBy, accountIds: string[] | undefined): string | null => (groupBy === 'account' ? key : accountIds?.length === 1 ? accountIds[0] : null);

/**
 * Two sums per group, the way the route splits them: the request side, which a probe row joins only when probes
 * count as requests, and the probe rows alone, which the probe counts, the probe cost and the wasted probes read.
 * The shares are a subscription account's with a tier, under the account grouping or where the query names that one
 * account, and null otherwise. The equivalents, the billed side and the plan counts read under every grouping.
 */
function groupOf(key: string, label: string, rows: HourlyRollup[], includeProbes: boolean, groupBy: AnalyticsGroupBy, accountIds: string[] | undefined): AnalyticsGroup {
  const counted = includeProbes ? rows : rows.filter((r) => r.kind !== 'probe');
  const probeRows = rows.filter((r) => r.kind === 'probe');
  const sum = (list: HourlyRollup[], read: (r: HourlyRollup) => number) => list.reduce((n, r) => n + read(r), 0);
  const account = shareAccountOf(key, groupBy, accountIds);
  const tier = account === null ? undefined : TIER_OF.get(account);
  const shares = account !== null && KIND_OF.get(account) === 'oauth' && tier !== undefined ? sharesOfGroup(counted, probeRows, tier) : NO_SHARES;
  const countedPlan = planRows(counted);
  const billed = counted.filter((r) => r.funding !== 'plan');

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
  const undecodable = sum(counted, (r) => r.requests.undecodable);
  const clientGone = sum(counted, (r) => r.requests.client_gone);
  const incomplete = sum(counted, (r) => r.requests.incomplete);
  const succeeded = sum(counted, (r) => r.requests.ok);
  const requests = succeeded + rateLimited + overloaded + upstreamError + clientGone + incomplete;
  const failovers = sum(counted, (r) => r.failovers);
  const errors = emptyCounts();
  if (rateLimited) errors.rate_limit_error = rateLimited;
  if (overloaded) errors.overloaded_error = overloaded;
  // A stream the relay could not decode counts under its own code, apart from the upstream's errors around it.
  if (upstreamError - undecodable) errors.api_error = upstreamError - undecodable;
  if (undecodable) errors.stream_undecodable = undecodable;
  if (clientGone) errors.client_gone = clientGone;
  if (incomplete) errors.stream_incomplete = incomplete;

  return {
    ...equivalentsOfGroup(counted, probeRows),
    // Every attempt past the first on a request is a failover, so the two together are what the accounts saw.
    attempts: requests + failovers,
    billed: { cacheRead: sum(billed, (r) => r.cost.cacheRead), cacheWrite: sum(billed, (r) => r.cost.cacheWrite), input: sum(billed, (r) => r.cost.input), output: sum(billed, (r) => r.cost.output) },
    billedUsd: sum(billed, (r) => r.costUsd),
    cacheHits: sum(counted, (r) => r.cacheHits),
    cacheMisses: sum(counted, (r) => r.cacheMisses),
    cacheMissReasons,
    cacheShares: shares.cacheShares,
    cost: { cacheRead: sum(counted, (r) => r.cost.cacheRead), cacheWrite: sum(counted, (r) => r.cost.cacheWrite), input: sum(counted, (r) => r.cost.input), output: sum(counted, (r) => r.cost.output) },
    costShares: shares.costShares,
    costUsd: sum(counted, (r) => r.costUsd),
    errors,
    // The fixture ledger records neither an exhausted pool nor a rejected request.
    exhausted: 0,
    expiredBetweenTurns: sum(counted, (r) => r.expiredBetweenTurns),
    failed: overloaded + upstreamError + clientGone,
    failovers,
    histograms,
    incomplete,
    keptWarm: { requests: sum(counted, (r) => r.keptWarm.requests), tokens1h: sum(counted, (r) => r.keptWarm.tokens1h), tokens5m: sum(counted, (r) => r.keptWarm.tokens5m) },
    key,
    keyCreatedAt: null,
    keyId: null,
    label,
    percentiles: { durationMs: quartet(histograms.durationMs), outputTps: quartet(histograms.outputTps), ttfbMs: quartet(histograms.ttfbMs), ttftMs: quartet(histograms.ttftMs) },
    planOutputTokens: sum(countedPlan, (r) => r.tokens.output),
    planRequests: sum(countedPlan, (r) => r.requests.ok + r.requests.incomplete + r.requests.rate_limited + r.requests.overloaded + r.requests.upstream_error + r.requests.client_gone),
    probeCostUsd: sum(probeRows, (r) => r.probeCostUsd),
    probes: { failed: sum(probeRows, (r) => r.probes.failed), held: sum(probeRows, (r) => r.probes.held), migrated: sum(probeRows, (r) => r.probeMigrated), ok: sum(probeRows, (r) => r.probes.ok), skipped: sum(probeRows, (r) => r.probes.skipped) },
    probeShares: shares.probeShares,
    rateLimited,
    refusals: emptyCounts(),
    refused: 0,
    rejected: 0,
    requests,
    savingShares: shares.savingShares,
    savingUsd: sum(counted, (r) => r.savingUsd),
    succeeded,
    tokens: {
      cacheRead: sum(counted, (r) => r.tokens.cacheRead), cacheWrite1h: sum(counted, (r) => r.tokens.cacheWrite1h), cacheWrite5m: sum(counted, (r) => r.tokens.cacheWrite5m), output: sum(counted, (r) => r.tokens.output),
      thinking: sum(counted, (r) => r.tokens.thinking), uncachedInput: sum(counted, (r) => r.tokens.uncachedInput), webFetch: sum(counted, (r) => r.tokens.webFetch), webSearch: sum(counted, (r) => r.tokens.webSearch),
    },
    uncached: sum(counted, (r) => r.uncached),
    wastedProbeReasons,
    wastedProbeUsd: sum(probeRows, (r) => r.wastedProbeUsd),
    wastedProbes: sum(probeRows, (r) => r.wastedProbes),
  };
}

/**
 * One split of a key group's key: the id, and the earlier holder's generation, null for the live key and the reserved
 * id.
 */
const holderKeyOf = (key: string): { generation: number | null; id: string } => {
  const [id, generation] = key.split('@');
  return { generation: generation === undefined ? null : Number(generation), id };
};

/** The key dimension alone carries a generation in its group key; the reserved id reads out in every dimension. */
const labelOf = (key: string, groupBy: AnalyticsGroupBy) => {
  if (key === '-') return 'unattributed';
  const holder = holderKeyOf(key);
  if (groupBy !== 'key' || holder.generation === null) return key;
  return earlierHolderLabel(holder.id, holder.generation);
};

/**
 * The holder a key group names beside its label, as the route answers it: the id, and an earlier holder's generation
 * as its creation instant. The live key and the reserved id carry no instant, and every other dimension carries null.
 */
const holderOf = (key: string, groupBy: AnalyticsGroupBy): Pick<AnalyticsGroup, 'keyCreatedAt' | 'keyId'> => {
  if (groupBy !== 'key') return { keyCreatedAt: null, keyId: null };
  const holder = holderKeyOf(key);
  return { keyCreatedAt: holder.generation, keyId: holder.id };
};

/**
 * A row's group key under a grouping. A pattern list names the groups under the user agent dimension, and every agent
 * none of them names falls under the one bucket, as the breakdown route files it.
 */
const groupKeyOf = (groupBy: AnalyticsGroupBy, named: string[]): ((r: HourlyRollup) => string) =>
  groupBy === 'user_agent' && named.length ? (r) => named.find((p) => matchesPattern(p, USER_AGENTS[r.keyId] ?? '')) ?? 'other' : GROUP_KEY[groupBy];

export function groupsOf(rows: HourlyRollup[], q: AnalyticsQuery): AnalyticsGroup[] {
  const of = groupKeyOf(q.groupBy, q.userAgentGroups ?? []);
  const buckets = new Map<string, HourlyRollup[]>();
  for (const r of rows) buckets.set(of(r), [...(buckets.get(of(r)) ?? []), r]);
  return [...buckets.entries()].map(([key, list]) => ({ ...groupOf(key, labelOf(key, q.groupBy), list, q.includeProbes ?? false, q.groupBy, q.accountIds), ...holderOf(key, q.groupBy) })).sort((a, b) => a.label.localeCompare(b.label) || (a.keyCreatedAt ?? 0) - (b.keyCreatedAt ?? 0) || a.key.localeCompare(b.key));
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

const usageOf = (r: HourlyRollup): Usage => ({ cacheRead: r.tokens.cacheRead, cacheWrite1h: r.tokens.cacheWrite1h, cacheWrite5m: r.tokens.cacheWrite5m, input: r.tokens.uncachedInput, output: r.tokens.output });

/**
 * A row's figure on each window its model applies to (the two shared windows and its model weeks) through `price`,
 * completing at the row's hour: the whole usage's, the cache read's alone and the cache writes' alone. `usage` is the
 * row's own unless a caller prices other counts of it, as the kept-warm saving does.
 */
function atHour(r: HourlyRollup, price: (window: string, usage: Usage, at: number) => number | null, usage = usageOf(r)): Record<'cost' | 'read' | 'write', Record<string, number | null>> {
  const windows = ['five_hour', 'seven_day', ...modelWeeks(TABLES, r.model)];
  // A client classifier call takes no share of a plan window, so its cache read, cache write and kept-warm credit read
  // zero too, as the route answers them.
  const free = r.classifier === 'client';
  const over = (u: Usage) => Object.fromEntries(windows.map((window) => [window, free ? 0 : price(window, u, Date.parse(r.hour))]));
  return { cost: over(usage), read: over({ ...usage, cacheWrite1h: 0, cacheWrite5m: 0, input: 0, output: 0 }), write: over({ ...usage, cacheRead: 0, input: 0, output: 0 }) };
}

/** A subscription row's shares through the cost model, the model tier's fallback weight standing in for a model with
 *  none, each null for an account tier or a model the cost model does not price. */
const sharesOf = (r: HourlyRollup, tier: string, usage = usageOf(r)) => atHour(r, (window, u, at) => shareOrFallback(TABLES, window, tier, r.model, u, at)?.share ?? null, usage);

/** A row's 20x window equivalents, the shares under the anchor tier, each null for a model the cost model does not
 *  price. */
const equivalentsOf = (r: HourlyRollup, usage = usageOf(r)) => atHour(r, (window, u, at) => equivalentOf(TABLES, window, r.model, u, at)?.equivalent ?? null, usage);

/** A bucket's sums, each window's shares and equivalents kept per row so one unpriced row nulls the window, as the
 *  route reads it. The model week's lists hold the rows on a model with a week of its own alone. */
interface BurnSum {
  cached: number;
  estimated: boolean;
  fiveHour: (number | null)[];
  fiveHourEquivalent: (number | null)[];
  sevenDay: (number | null)[];
  sevenDayEquivalent: (number | null)[];
  tokens: number;
  uncached: number;
  usd: number;
  week: (number | null)[];
  weekEquivalent: (number | null)[];
}
const emptyBurn = (): BurnSum => ({ cached: 0, estimated: false, fiveHour: [], fiveHourEquivalent: [], sevenDay: [], sevenDayEquivalent: [], tokens: 0, uncached: 0, usd: 0, week: [], weekEquivalent: [] });

/** How many sessions the session grouping keeps, the route's cut. */
const BURN_SESSIONS = 20;

/**
 * A row's group under a burn grouping: the breakdown's key for every dimension it shares, so an earlier key holder
 * reads `<id>@<generation>` as the breakdown names it, a user agent pattern list's group under the user agent
 * dimension, and the row's session under the burn's own session grouping.
 */
const burnGroupOf = (groupBy: BurnGroup, named: string[]): ((r: HourlyRollup) => string) => (groupBy === 'session' ? (r) => r.sessionId : groupKeyOf(groupBy, named));

export function burnOf(rows: HourlyRollup[], range: Range, groupBy: BurnGroup, filters: Pick<AnalyticsQuery, 'accountIds' | 'userAgentGroups'> = {}): { bucketSeconds: number; points: BurnPoint[] } {
  const { bucketMs, starts } = bucketsOf(range);
  const kinds = new Map(ACCOUNTS.map((a) => [a.id, a.kind]));
  const tiers = new Map(ACCOUNTS.map((a) => [a.id, a.subscription?.rateLimitTier]));
  const first = starts[0] ?? 0;
  const end = (starts[starts.length - 1] ?? 0) + bucketMs;
  const accounts = filters.accountIds ?? [];
  // The route prices a row by its account's kind and skips a row whose account left the registry.
  const kept = rows.filter((r) => Date.parse(r.hour) >= first && Date.parse(r.hour) < end && kinds.has(r.accountId) && (accounts.length === 0 || accounts.includes(r.accountId)));
  const groupOf = burnGroupOf(groupBy, filters.userAgentGroups ?? []);
  const sums = new Map<string, Map<number, BurnSum>>();
  for (const r of kept) {
    const at = Math.floor((Date.parse(r.hour) - first) / bucketMs) * bucketMs + first;
    const oauth = kinds.get(r.accountId) === 'oauth';
    // A fast answer spends usage credits and an overage row the account's overage, each in dollars on either kind
    // and off every plan window: on a tier the cost model sizes they read as a zero share, so the account is still a
    // priced one.
    const billed = r.funding !== 'plan';
    const group = groupOf(r);
    const byAt = sums.get(group) ?? new Map();
    sums.set(group, byAt);
    const s = byAt.get(at) ?? emptyBurn();
    byAt.set(at, s);
    const tokens = r.tokens.uncachedInput + r.tokens.cacheRead + r.tokens.cacheWrite5m + r.tokens.cacheWrite1h + r.tokens.output;
    s.cached += r.tokens.cacheRead;
    s.tokens += tokens;
    s.uncached += r.tokens.uncachedInput;
    s.usd += oauth && !billed ? 0 : r.costUsd;
    // A row with no token has nothing to price, as the route reads it: a failed request on a model the cost model has
    // no weight for never nulls a priced bucket.
    if (tokens === 0) continue;
    const week = modelWeeks(TABLES, r.model)[0];
    const tier = oauth ? tiers.get(r.accountId) : undefined;
    if (billed) {
      // Every billed row adds a zero equivalent, so a bucket with tokens and no plan row reads 0 and only an idle one
      // null. The zero share rests on the tier alone, as the route reads a dollar-billed row, and an API key account
      // has no window to share.
      s.fiveHourEquivalent.push(0);
      s.sevenDayEquivalent.push(0);
      if (week !== undefined) s.weekEquivalent.push(0);
      const sized = (window: 'five_hour' | 'seven_day') => tier !== undefined && tierCapacity(TABLES, tier, window) !== null;
      if (sized('five_hour')) s.fiveHour.push(0);
      if (sized('seven_day')) s.sevenDay.push(0);
      // The week's own capacity, as the route reads it: the shared window the week prices on.
      const weekWindow = week === undefined ? null : pricedWindow(TABLES, week);
      if (weekWindow !== null && sized(weekWindow.priced)) s.week.push(0);
      continue;
    }
    // The row's own shares, its requests priced one by one at their completions, and its equivalents, which read no
    // account tier.
    s.fiveHour.push(tier === undefined ? null : (r.shares.five_hour ?? null));
    s.sevenDay.push(tier === undefined ? null : (r.shares.seven_day ?? null));
    s.fiveHourEquivalent.push(r.equivalents.five_hour ?? null);
    s.sevenDayEquivalent.push(r.equivalents.seven_day ?? null);
    if (week !== undefined) {
      s.week.push(tier === undefined ? null : (r.shares[week] ?? null));
      s.weekEquivalent.push(r.equivalents[week] ?? null);
    }
    if (r.estimated) s.estimated = true;
  }
  const tokensOf = (group: string) => [...(sums.get(group)?.values() ?? [])].reduce((n, s) => n + s.cached + s.uncached, 0);
  let groups = [...sums.keys()].sort();
  if (groupBy === 'session') groups = [...groups].sort((a, b) => tokensOf(b) - tokensOf(a)).slice(0, BURN_SESSIONS).sort();
  // The route answers the shares and the unit where each group holds one account's rows: the account grouping, or a
  // filter naming exactly one account.
  const accountOf = (group: string) => (groupBy === 'account' ? group : accounts.length === 1 ? accounts[0] : null);
  const points = groups.flatMap((group) =>
    starts.map((at): BurnPoint => {
      const s = sums.get(group)?.get(at) ?? emptyBurn();
      const account = accountOf(group);
      const kind = account === null ? undefined : kinds.get(account);
      const share = (list: (number | null)[]) => (account === null ? null : bucketShare(list));
      return {
        at: iso(at),
        cachedTokens: Math.round(s.cached),
        estimated: s.estimated,
        fiveHourEquivalent: bucketShare(s.fiveHourEquivalent),
        fiveHourShare: share(s.fiveHour),
        group,
        modelWeekEquivalent: bucketShare(s.weekEquivalent),
        modelWeekShare: share(s.week),
        sevenDayEquivalent: bucketShare(s.sevenDayEquivalent),
        sevenDayShare: share(s.sevenDay),
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

/**
 * The destination fit each hour's moves are filed under, in turn beside the reason. A turn of another length than the
 * reasons' gives two reasons different fits over a day.
 */
const FIT_TURNS = ['fits', 'fits', 'rewrite_only', 'fits', 'least_bad', 'fits', 'unknown'];

/**
 * The account rollups' moves into each account by reason and by the destination's fit, with the cache the first
 * request after each one wrote again. `moves` is each reason's sum over its fits, as the route answers it. The held
 * time per bucket folds the request rows as the ledger folds the completions, since no account rollup the console
 * reads carries it: the slow-down waits of the bucket's requests summed, and the requests that waited one.
 */
export function rewritesOf(rollups: AccountRollup[], requests: RequestLog[], range: Range): RewritesAnswer {
  const { bucketMs, starts } = bucketsOf(range);
  const movesByReasonAndFit: Record<string, Record<string, number>> = {};
  const points = starts.map((at): RewritePoint => {
    let rewriteTokens = 0;
    for (const r of rollups) {
      const hour = Date.parse(r.hour);
      if (hour < at || hour >= at + bucketMs || r.pins.migratedIn === 0) continue;
      const turn = Math.floor(hour / HOUR);
      rewriteTokens += r.pins.migratedIn * (18_000 + (turn % 5) * 9_000);
      const reason = MOVE_TURNS[turn % MOVE_TURNS.length];
      const fit = FIT_TURNS[turn % FIT_TURNS.length];
      const fits = (movesByReasonAndFit[reason] ??= {});
      fits[fit] = (fits[fit] ?? 0) + r.pins.migratedIn;
    }
    return { at: iso(at), rewriteTokens };
  });
  const moves = Object.fromEntries(Object.entries(movesByReasonAndFit).map(([reason, fits]) => [reason, Object.values(fits).reduce((n, x) => n + x, 0)]));
  const held = starts.map((at): HeldPoint => {
    let heldMs = 0;
    let waited = 0;
    for (const r of requests) {
      const t = Date.parse(r.at);
      const slowed = t >= at && t < at + bucketMs ? (r.held?.waits ?? []).filter((w) => w.reason === 'slow_down') : [];
      if (slowed.length === 0) continue;
      heldMs += slowed.reduce((n, w) => n + w.heldMs, 0);
      waited += 1;
    }
    return { at: iso(at), heldMs, requests: waited };
  });
  return { held, moves, movesByReasonAndFit, points };
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

/**
 * The days the ledger keeps a request row, as the mock answers it beside its windows. The settings carry no request
 * retention in days, so the figure is the mock's own, long enough that every window below finds its requests.
 */
export const REQUEST_RETENTION_DAYS = 90;

/** A window the hourly fold closed within the hour after its reset, its ratio the rise over the share as stored. */
const reconciled = (accountId: string, window: string, endMs: number, readings: [number, number], observedRise: number, predictedShare: number, requests: number): Reconciliation => ({
  accountId,
  computedAt: iso(endMs + 37 * MINUTE),
  estimated: 0,
  firstReadingAt: iso(readings[0]),
  lastReadingAt: iso(readings[1]),
  observedRise,
  predictedShare,
  ratio: observedRise / predictedShare,
  requests,
  window,
  windowEnd: iso(endMs),
});

/**
 * The story's completed windows, newest first as the route orders them, an account's newest of each window ending where
 * its current one began. Every ratio sits near 1 but nightly's newest 5-hour window, which ended at the 13:11 reset of
 * the window the poll read at its limit at 12:01: its readings rose 0.3 against the 0.15 the router priced, twice the
 * table.
 */
export const RECONCILIATIONS: Reconciliation[] = [
  reconciled('nightly', 'five_hour', clock(13, 11), [clock(10, 52), clock(13, 10)], 0.3, 0.15, 64),
  reconciled('primary', 'five_hour', clock(11, 44), [clock(7, 3), clock(11, 38)], 0.52, 0.5, 212),
  reconciled('studio', 'five_hour', clock(10, 29), [clock(6, 9), clock(10, 26)], 0.38, 0.4, 37),
  reconciled('nightly', 'five_hour', NOW - 15 * HOUR - 50 * MINUTE, [NOW - 20 * HOUR - 20 * MINUTE, NOW - 16 * HOUR], 0.41, 0.43, 88),
  reconciled('studio', 'seven_day_fable', NOW - 2 * DAY - 14 * HOUR - 40 * MINUTE, [NOW - 9 * DAY - 8 * HOUR, NOW - 2 * DAY - 15 * HOUR], 0.21, 0.2, 19),
  reconciled('primary', 'seven_day', NOW - 3 * DAY - 18 * HOUR - 20 * MINUTE, [NOW - 10 * DAY - 16 * HOUR, NOW - 3 * DAY - 18 * HOUR - 32 * MINUTE], 0.64, 0.62, 2_350),
  reconciled('nightly', 'seven_day', NOW - 5 * DAY - 5 * HOUR - 48 * MINUTE, [NOW - 12 * DAY - 2 * HOUR, NOW - 5 * DAY - 6 * HOUR - 13 * MINUTE], 0.71, 0.69, 1_480),
];

/** The reconciled windows as the route answers them, narrowed to the Accounts filter, every account's without one. */
export function reconciliationsOf(rows: Reconciliation[], filters: Pick<AnalyticsQuery, 'accountIds'> = {}): ReconciliationsAnswer {
  const accounts = filters.accountIds ?? [];
  return { reconciliations: rows.filter((r) => accounts.length === 0 || accounts.includes(r.accountId)).map((r) => ({ ...r })), requestRetentionDays: REQUEST_RETENTION_DAYS };
}
