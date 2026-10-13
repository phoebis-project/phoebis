import { WASTED_PROBE_REASONS, emptyCacheMissReasons, emptyWastedProbeReasons, missCount } from '@/lib/cache';
import { fundingOf } from '@/lib/costs';
import { countOf, emptyCounts } from '@/lib/counts';
import { foldEffort, foldRequestClass } from '@/lib/dimensions';
import { earlierHolderLabel } from '@/lib/format';
import { emptyHistogram, histogramOf, mergeHistograms, percentileFromHistogram } from '@/lib/histogram';
import { matchesPattern } from '@/lib/pattern';
import type { AnalyticsGroup, AnalyticsGroupBy, AnalyticsPoint, FundingFilter, RequestLog, RequestPage } from './types';

const METRICS = ['durationMs', 'outputTps', 'ttfbMs', 'ttftMs'] as const;
const quartet = (h: readonly number[]) => ({ p50: percentileFromHistogram(h, 0.5), p90: percentileFromHistogram(h, 0.9), p95: percentileFromHistogram(h, 0.95), p99: percentileFromHistogram(h, 0.99) });

/**
 * A group as the routes answer one, every figure present and the shares null. `foldGroups` fills `costShares` under
 * the account grouping alone, where one account's windows add, since a group of any other dimension spans accounts
 * whose windows do not. The 20x equivalents add across accounts, so they fold under every grouping.
 */
const empty = (key: string, label = key): AnalyticsGroup => ({
  attempts: 0,
  billed: { cacheRead: 0, cacheWrite: 0, input: 0, output: 0 },
  billedUsd: 0,
  cacheEquivalent: {},
  cacheHits: 0,
  cacheMisses: 0,
  cacheMissReasons: emptyCacheMissReasons(),
  cacheShares: null,
  cost: { cacheRead: 0, cacheWrite: 0, input: 0, output: 0 },
  costEquivalent: {},
  costShares: null,
  costUsd: 0,
  equivalentSplit: { cacheRead: 0, cacheWrite: 0, input: 0, output: 0 },
  errors: emptyCounts(),
  estimated: false,
  exhausted: 0,
  expiredBetweenTurns: 0,
  failed: 0,
  failovers: 0,
  histograms: { durationMs: emptyHistogram(), outputTps: emptyHistogram(), ttfbMs: emptyHistogram(), ttftMs: emptyHistogram() },
  incomplete: 0,
  keptWarm: { requests: 0, tokens: 0 },
  key,
  keyCreatedAt: null,
  keyId: null,
  label,
  percentiles: { durationMs: quartet(emptyHistogram()), outputTps: quartet(emptyHistogram()), ttfbMs: quartet(emptyHistogram()), ttftMs: quartet(emptyHistogram()) },
  planOutputTokens: null,
  planRequests: null,
  probeCostUsd: 0,
  probeEquivalent: {},
  probes: { failed: 0, held: 0, migrated: 0, ok: 0, skipped: 0 },
  probeShares: null,
  rateLimited: 0,
  rejected: 0,
  requests: 0,
  succeeded: 0,
  tokens: { cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: 0, output: 0, thinking: 0, uncachedInput: 0, webFetch: 0, webSearch: 0 },
  uncached: 0,
  wastedProbeReasons: emptyWastedProbeReasons(),
  wastedProbeUsd: 0,
  wastedProbes: 0,
});

/**
 * A group folded from request rows. A row carries one cost total, one equivalent per window and no probe or kept-warm
 * facts, so the splits and the kept-warm figures stay unknown rather than reading as zero, and `fromRequests` marks the
 * probe figures unmeasured.
 */
const emptyFolded = (key: string, label = key): AnalyticsGroup => ({
  ...empty(key, label),
  billed: { cacheRead: null, cacheWrite: null, input: 0, output: 0 },
  cost: { cacheRead: null, cacheWrite: null, input: 0, output: 0 },
  equivalentSplit: null,
  fromRequests: true,
  keptWarm: null,
});

/**
 * The header's first product token (`name/version`, up to the first space) cut to 64 bytes, else `other`, as the
 * ledger keys a user agent in its hourly rollups: the build of a client is in the token, and the comment after it
 * is free text.
 */
export function foldUserAgent(raw: string): string {
  const token = raw.split(' ')[0];
  const parts = token.split('/');
  return parts.length === 2 && parts.every((part) => /^[A-Za-z0-9.:_-]+$/.test(part)) ? token.slice(0, 64) : 'other';
}

const keyOf = (r: RequestLog, by: AnalyticsGroupBy, agentGroups: string[]): string => {
  if (by === 'account') return r.accountId ?? '-';
  if (by === 'client') return r.client ?? '-';
  // The effort and the request class key as the rollups fold them, so a session view's group is the value the
  // Requests filters match.
  if (by === 'effort') return foldEffort(r.shape.effort);
  // A key id created again is another holder, so the group is the lifetime, which a `keys=` selector names too.
  if (by === 'key') return `${r.keyId ?? '-'}@${r.keyGeneration}`;
  if (by === 'kind') return r.kind;
  // The canonical id, as the rollups file a request and the Models filter matches it, so a `[1m]` spelling or an
  // alias joins the group of the model it resolved to.
  if (by === 'model') return r.canonicalModel;
  if (by === 'pool') return r.poolId ?? '-';
  if (by === 'request_class') return foldRequestClass(r.requestClass);
  // A fast request refused before any attempt served still asked for fast, and the rollups file it so.
  if (by === 'speed') return r.attempts[r.attempts.length - 1]?.response.speed ?? (r.shape.requestedSpeed === 'fast' ? 'fast' : 'standard');
  if (by === 'user_agent') {
    // The rollups key an agent by its token and file a row that named no client under '-', which every route
    // answers as unattributed; under a pattern list the token falls with every other unmatched client under
    // 'other', as the breakdown route does.
    const agent = r.userAgent === null ? '-' : foldUserAgent(r.userAgent);
    if (agentGroups.length === 0) return agent;
    return agentGroups.find((p) => matchesPattern(p, agent)) ?? 'other';
  }
  return 'all';
};

export interface FoldOptions {
  /** Patterns naming the groups under `user_agent`; a row falls in the first that matches, the rest under `other`. */
  agentGroups?: string[];
  /** Patterns a row's folded user agent must match one of; a row that named none is matched as `-`, as the rollups file it. */
  agentPatterns?: string[];
  /** Patterns a row's client class must match one of; a row with none is matched as `-`, as the rollups file it. */
  clientPatterns?: string[];
  /** The side the kept rows were paid on, read off each row's serving attempt. Absent keeps every row. */
  funding?: FundingFilter;
}

/** The newest generation of each key id among the rows, the one the breakdown labels with the bare id. */
function latestGenerations(rows: RequestLog[]): Map<string, number> {
  const latest = new Map<string, number>();
  for (const r of rows) if (r.keyId !== null) latest.set(r.keyId, Math.max(latest.get(r.keyId) ?? r.keyGeneration, r.keyGeneration));
  return latest;
}

/**
 * The breakdown route's labels: `unattributed` for a row no key or client was read for, and under `key` the id
 * alone for its newest generation and `<id> (deleted, created Sep 14, 2026, 09:31)` for an earlier one
 * (`<id> (deleted)` at generation 0). The rows stand in for the router's registry here, so the newest generation
 * among them reads as the live one.
 */
function labelOf(r: RequestLog, key: string, by: AnalyticsGroupBy, latest: Map<string, number>): string {
  if (by !== 'key') return key === '-' ? 'unattributed' : key;
  if (r.keyId === null) return 'unattributed';
  return latest.get(r.keyId) === r.keyGeneration ? r.keyId : earlierHolderLabel(r.keyId, r.keyGeneration);
}

/** Throughput is measured after the first content token, as the ledger measures it; without both timings there is no sample. */
const outputTps = (r: RequestLog): number | null => (r.durationMs !== null && r.ttftMs !== null && r.durationMs - r.ttftMs > 0 && r.usage.output > 0 ? r.usage.output / ((r.durationMs - r.ttftMs) / 1000) : null);

/**
 * Who paid for a request: its serving attempt's side. A row no attempt served files under `plan`, the ledger's filing,
 * which `onSide` reads off the empty trail, so the `api_key` answer here only keeps such a row out of the plan shares
 * it has none of.
 */
const fundingOfRow = (r: RequestLog) => fundingOf(r.attempts[r.attempts.length - 1] ?? null);
/**
 * Whether a row falls on the funding filter's side. The ledger files a request no attempt served under `plan` on the
 * unattributed account, so the filter follows that filing rather than the cell's dollar unit: a session view and the
 * grouped routes then count the same rows under each position.
 */
const onSide = (r: RequestLog, side: FundingFilter) => (r.attempts.length === 0 || fundingOfRow(r) === 'plan') === (side === 'plan');

/**
 * A probe the router refused with `503 ledger_behind` while its ledger caught up. No account was asked, so the ledger
 * files it under its held count alone, outside the requests, their outcomes and the error codes.
 */
const heldProbe = (r: RequestLog) => r.kind === 'probe' && r.status === 503 && r.errorCode === 'ledger_behind';

/** The five token counts a share is priced from, as the ledger sums them before it prices a row. */
const tokensOf = (r: RequestLog) => r.usage.input + r.usage.cacheRead + r.usage.cacheWrite5m + r.usage.cacheWrite1h + r.usage.output;

/**
 * The plan-funded rows' shares summed per window into the group, as the route sums them under the account grouping:
 * a window no row's model applies to is no key, a window a row carries as null (the cost model could not price it) is
 * null whatever the other rows priced, and the rest sum. A row whose account the route could not price (`{}`, or
 * no map at all) nulls the map and both counts, as the route answers such an account, since the fold holds no window
 * rule of its own to say which windows that row's model applies to. No plan-funded row is no shares. A plan-funded
 * row with no token (a failed attempt the route priced nothing for) joins the counts and not the shares, as the
 * ledger's sums skip it: its empty map would otherwise read as an account the route could not price and dash the
 * priced rows' total.
 */
function foldShares(g: AnalyticsGroup, rows: RequestLog[]) {
  const plan = rows.filter((r) => fundingOfRow(r) === 'plan');
  const priced = plan.filter((r) => tokensOf(r) > 0);
  if (priced.length === 0 || priced.some((r) => r.costShares === null || Object.keys(r.costShares).length === 0)) return;
  const shares: Record<string, number | null> = {};
  for (const r of priced) {
    for (const [window, share] of Object.entries(r.costShares ?? {})) {
      const sum = shares[window];
      shares[window] = share === null || sum === null ? null : (sum ?? 0) + share;
    }
  }
  g.costShares = shares;
  g.planRequests = plan.length;
  g.planOutputTokens = plan.reduce((n, r) => n + r.usage.output, 0);
}

/**
 * Request rows summed the way the rollups sum them, for the one filter the rollups cannot answer: a session.
 * `latest` is taken over every row of the range, so a bucket holding an earlier key holder alone still labels it
 * deleted.
 */
export function foldGroups(rows: RequestLog[], by: AnalyticsGroupBy, options: FoldOptions = {}, latest = latestGenerations(rows)): AnalyticsGroup[] {
  const groups = new Map<string, AnalyticsGroup>();
  const lists = new Map<string, RequestLog[]>();
  const agentPatterns = options.agentPatterns ?? [];
  const clientPatterns = options.clientPatterns ?? [];
  const { funding } = options;
  // The agent is matched on its folded token, as the unscoped page's rollups hold it, so one pattern selects the same
  // rows under both scopes.
  const kept = rows.filter(
    (r) =>
      (!agentPatterns.length || agentPatterns.some((p) => matchesPattern(p, r.userAgent === null ? '-' : foldUserAgent(r.userAgent)))) &&
      (!clientPatterns.length || clientPatterns.some((p) => matchesPattern(p, r.client ?? '-'))) &&
      (funding === undefined || onSide(r, funding)),
  );
  for (const r of kept) {
    const key = keyOf(r, by, options.agentGroups ?? []);
    const g = groups.get(key) ?? emptyFolded(key, labelOf(r, key, by, latest));
    groups.set(key, g);
    if (heldProbe(r)) {
      g.probes.held += 1;
      continue;
    }
    const list = lists.get(key) ?? [];
    list.push(r);
    lists.set(key, list);
    g.requests += 1;
    if (r.outcome === 'succeeded') g.succeeded += 1;
    else if (r.outcome === 'exhausted') g.exhausted += 1;
    else if (r.outcome === 'rejected') g.rejected += 1;
    else if (r.outcome === 'incomplete') g.incomplete += 1;
    else if (r.status === 429) g.rateLimited += 1;
    else g.failed += 1;
    if (r.errorCode) g.errors[r.errorCode] = countOf(g.errors, r.errorCode) + 1;
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
    // The billed side is the funding filter's complement of the plan, and a window the cost model could not price
    // adds nothing to the equivalent, as the route's sum skips it.
    if (!onSide(r, 'plan')) g.billedUsd += r.costUsd;
    for (const [window, equivalent] of Object.entries(r.costEquivalent ?? {})) if (equivalent !== null) g.costEquivalent[window] = (g.costEquivalent[window] ?? 0) + equivalent;
    if (r.estimated) g.estimated = true;
    // The ledger classifies by tokens: a read is a hit, a write without a read is a miss, neither is uncached.
    // The diagnosed reason is a breakdown of its own and decides nothing here.
    if (r.usage.cacheRead > 0) g.cacheHits += 1;
    else if (r.usage.cacheWrite5m + r.usage.cacheWrite1h > 0) g.cacheMisses += 1;
    else g.uncached += 1;
    const reason = r.attempts[r.attempts.length - 1]?.cacheMissReason ?? null;
    if (reason) g.cacheMissReasons[reason] = missCount(g.cacheMissReasons, reason) + 1;
    if (reason === 'none' && r.usage.cacheRead === 0) g.expiredBetweenTurns += 1;
  }
  for (const [key, g] of groups) {
    const list = lists.get(key) ?? [];
    const pick = (read: (r: RequestLog) => number | null) => list.map(read).filter((v): v is number => v !== null);
    g.histograms = { durationMs: histogramOf(pick((r) => r.durationMs)), outputTps: histogramOf(pick(outputTps)), ttfbMs: histogramOf(pick((r) => r.ttfbMs)), ttftMs: histogramOf(pick((r) => r.ttftMs)) };
    for (const m of METRICS) g.percentiles[m] = quartet(g.histograms[m]);
    // Shares of one account's windows add, and a group of any other dimension spans accounts, whose windows do not.
    if (by === 'account') foldShares(g, list);
  }
  return [...groups.values()].sort((a, b) => b.requests - a.requests || a.key.localeCompare(b.key));
}

export function foldRequests(rows: RequestLog[], bucketMs: number, from: string, to: string, by: AnalyticsGroupBy = 'all', options: FoldOptions = {}): AnalyticsPoint[] {
  const end = Date.parse(to);
  const latest = latestGenerations(rows);
  const start = Math.floor(Date.parse(from) / bucketMs) * bucketMs;
  // A range ending on a bucket start owns no part of that bucket, which is where the series route stops too. The last
  // bucket runs to its own end, past `to`.
  const count = Math.max(0, Math.ceil((end - start) / bucketMs));
  const buckets: RequestLog[][] = Array.from({ length: count }, () => []);
  // Each row is parsed once and placed by index: a filter per bucket parses every row once per bucket.
  for (const r of rows) {
    const i = Math.floor((Date.parse(r.at) - start) / bucketMs);
    if (i >= 0 && i < count) buckets[i].push(r);
  }
  return buckets.map((inBucket, i) => ({ at: new Date(start + i * bucketMs).toISOString(), groups: foldGroups(inBucket, by, options, latest) }));
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

const plus = (a: number | null, b: number | null): number | null => (a === null || b === null ? null : a + b);
/** A count a fold may lack: it stays unknown only while no group carries one. */
const plusKnown = (a: number | null, b: number | null): number | null => (a === null ? b : b === null ? a : a + b);
const plusSplit = (a: AnalyticsGroup['cost'], b: AnalyticsGroup['cost']): AnalyticsGroup['cost'] => ({ cacheRead: plus(a.cacheRead, b.cacheRead), cacheWrite: plus(a.cacheWrite, b.cacheWrite), input: a.input + b.input, output: a.output + b.output });
/** Window-keyed entries summed per window, every window either side carries. */
function addByWindow<T>(into: Record<string, T>, from: Record<string, T>, add: (a: T, b: T) => T) {
  for (const [window, entry] of Object.entries(from)) into[window] = own(into, window) ? add(into[window], entry) : entry;
}
const own = (map: object, key: string): boolean => Object.prototype.hasOwnProperty.call(map, key);

/** The windows every plan row prices on. A model week is not one: a group off such a model carries none. */
const SHARED_WINDOWS = ['five_hour', 'seven_day'];
/**
 * Whether a group's plan rows oblige its equivalents to carry both shared windows, so that one it lacks is a window
 * the ledger could not price a plan row on. The ledger prices nothing on a row with no token, so a group whose plan
 * rows wrote no output token is let off, and a fold of request rows, which counts no plan request, has plan rows
 * where it carries a window at all, as `sidesOf` reads it.
 */
const carriesShared = (g: AnalyticsGroup): boolean => (g.planRequests === null ? Object.keys(g.costEquivalent).length > 0 : g.planRequests > 0 && g.planOutputTokens !== 0);
/**
 * Whether a group's probes oblige its probe equivalents to carry both shared windows: answered probes alone, since a
 * held or skipped probe never ran, on a group with plan rows, since a group of billed traffic alone (an API-key
 * account's, or one in overage) runs billed probes, which carry no plan window to lack.
 */
const probesCarryShared = (g: AnalyticsGroup): boolean => g.probes.ok > 0 && carriesShared(g);

/**
 * Several groups of one point summed into one, for the charts and tiles that draw totals. A figure one group
 * cannot supply leaves the sum unknown, so a total never passes a partial sum off as the whole: a shared window a
 * group with plan rows lacks leaves the equivalents, and one a group with plan probes lacks the probe equivalents, as
 * each leaves the one account's shares. The plan counts are the exception: a fold outside the account grouping counts
 * none, and the groups that carry them still total.
 */
export function sumGroups(groups: AnalyticsGroup[]): AnalyticsGroup {
  const out = empty('all');
  for (const g of groups) {
    for (const k of ['attempts', 'billedUsd', 'cacheHits', 'cacheMisses', 'costUsd', 'exhausted', 'expiredBetweenTurns', 'failed', 'failovers', 'incomplete', 'probeCostUsd', 'rateLimited', 'rejected', 'requests', 'succeeded', 'uncached', 'wastedProbeUsd', 'wastedProbes'] as const) out[k] += g[k];
    out.cost = plusSplit(out.cost, g.cost);
    out.billed = plusSplit(out.billed, g.billed);
    addByWindow(out.costEquivalent, g.costEquivalent, (a, b) => a + b);
    addByWindow(out.cacheEquivalent, g.cacheEquivalent, (a, b) => ({ read: a.read + b.read, write: a.write + b.write }));
    addByWindow(out.probeEquivalent, g.probeEquivalent, (a, b) => ({ cost: a.cost + b.cost, wasted: a.wasted + b.wasted }));
    const split = out.equivalentSplit;
    out.equivalentSplit = split === null || g.equivalentSplit === null ? null : { cacheRead: split.cacheRead + g.equivalentSplit.cacheRead, cacheWrite: split.cacheWrite + g.equivalentSplit.cacheWrite, input: split.input + g.equivalentSplit.input, output: split.output + g.equivalentSplit.output };
    if (g.estimated) out.estimated = true;
    out.planRequests = plusKnown(out.planRequests, g.planRequests);
    out.planOutputTokens = plusKnown(out.planOutputTokens, g.planOutputTokens);
    for (const k of ['cacheRead', 'cacheWrite1h', 'cacheWrite5m', 'output', 'thinking', 'uncachedInput', 'webFetch', 'webSearch'] as const) out.tokens[k] += g.tokens[k];
    for (const [code, n] of Object.entries(g.errors)) out.errors[code] = countOf(out.errors, code) + n;
    for (const [reason, n] of Object.entries(g.cacheMissReasons)) out.cacheMissReasons[reason] = missCount(out.cacheMissReasons, reason) + n;
    if (g.fromRequests) out.fromRequests = true;
    out.keptWarm = out.keptWarm && g.keptWarm ? { requests: out.keptWarm.requests + g.keptWarm.requests, tokens: out.keptWarm.tokens + g.keptWarm.tokens } : null;
    for (const k of ['failed', 'held', 'migrated', 'ok', 'skipped'] as const) out.probes[k] += g.probes[k];
    for (const reason of WASTED_PROBE_REASONS) out.wastedProbeReasons[reason] += g.wastedProbeReasons[reason];
  }
  for (const g of groups.filter(carriesShared)) {
    for (const window of SHARED_WINDOWS) {
      if (!own(g.costEquivalent, window)) delete out.costEquivalent[window];
      if (!own(g.cacheEquivalent, window)) delete out.cacheEquivalent[window];
    }
  }
  for (const g of groups.filter(probesCarryShared)) for (const window of SHARED_WINDOWS) if (!own(g.probeEquivalent, window)) delete out.probeEquivalent[window];
  for (const m of METRICS) {
    out.histograms[m] = mergeHistograms(groups.map((g) => g.histograms[m]));
    out.percentiles[m] = quartet(out.histograms[m]);
  }
  return out;
}
