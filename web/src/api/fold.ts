import { WASTED_PROBE_REASONS, emptyCacheMissReasons, emptyWastedProbeReasons } from '@/lib/cache';
import { fundingOf } from '@/lib/costs';
import { addCount, addCounts, emptyCounts } from '@/lib/counts';
import { expiredBetweenTurns, foldClassifier, foldEffort, foldRequestClass, foldSpeed, hitUnderReason, latencySamples, servedMissReason, servingAttempt } from '@/lib/dimensions';
import { earlierHolderLabel, keyLifetime, tokensTotal } from '@/lib/format';
import { emptyHistogram, histogramOf, mergeHistograms, quartet } from '@/lib/histogram';
import { accountBound } from '@/lib/inputTransformations';
import { matchesPattern } from '@/lib/pattern';
import { iso } from './decode';
import type { AnalyticsGroup, AnalyticsGroupBy, AnalyticsPoint, FundingFilter, InputTransformations, RequestLog, RequestPage } from './types';

const METRICS = ['durationMs', 'outputTps', 'ttfbMs', 'ttftMs'] as const;

/**
 * A group as the routes answer one, every figure present and the shares null. `foldGroups` fills `costShares` under
 * the account grouping alone, where one account's windows add, since a group of any other dimension spans accounts
 * whose windows do not. The Pro-week figure adds across accounts and tiers, so it folds under every grouping.
 */
const empty = (key: string, label = key): AnalyticsGroup => ({
  attempts: 0,
  billed: { cacheRead: 0, cacheWrite: 0, input: 0, output: 0 },
  billedUsd: 0,
  cacheEquivalent: { read: 0, write: 0 },
  cacheHits: 0,
  cacheMisses: 0,
  cacheMissReasonHits: emptyCounts(),
  cacheMissReasons: emptyCacheMissReasons(),
  clientCanceled: 0,
  cost: { cacheRead: 0, cacheWrite: 0, input: 0, output: 0 },
  costEquivalent: 0,
  costShares: null,
  costUsd: 0,
  equivalentSplit: { cacheRead: 0, cacheWrite: 0, input: 0, output: 0 },
  errors: emptyCounts(),
  exhausted: 0,
  expiredBetweenTurns: 0,
  failed: 0,
  failovers: 0,
  histograms: { durationMs: emptyHistogram(), outputTps: emptyHistogram(), ttfbMs: emptyHistogram(), ttftMs: emptyHistogram() },
  incomplete: 0,
  inputTransformations: emptyCounts(),
  inputTransformationsMoved: 0,
  inputTransformationsNone: 0,
  inputTransformationsReported: 0,
  keptWarm: { planRequests: 0, planTokens1h: 0, planTokens5m: 0, requests: 0, tokens1h: 0, tokens5m: 0 },
  key,
  keyCreatedAt: null,
  keyId: null,
  label,
  percentiles: { durationMs: quartet(emptyHistogram()), outputTps: quartet(emptyHistogram()), ttfbMs: quartet(emptyHistogram()), ttftMs: quartet(emptyHistogram()) },
  planOutputTokens: null,
  planRequests: null,
  probeCostUsd: 0,
  probeEquivalent: { cost: 0, wasted: 0 },
  probes: { failed: 0, held: 0, ok: 0, planOk: 0, skipped: 0 },
  rateLimited: 0,
  refusals: emptyCounts(),
  refused: 0,
  rejected: 0,
  requests: 0,
  savingEquivalent: 0,
  savingUsd: 0,
  succeeded: 0,
  tokens: { cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: 0, output: 0, thinking: 0, uncachedInput: 0, webFetch: 0, webSearch: 0 },
  uncacheable: 0,
  uncached: 0,
  wastedProbeReasons: emptyWastedProbeReasons(),
  wastedProbeUsd: 0,
  wastedProbes: 0,
});

/**
 * A group folded from request rows. A row carries one cost total, one Pro-week figure and no probe or kept-warm facts,
 * so the splits, the cache, probe and saving figures and the kept-warm counts stay unknown rather than reading as zero,
 * and `fromRequests` marks the probe figures unmeasured.
 */
const emptyFolded = (key: string, label = key): AnalyticsGroup => ({
  ...empty(key, label),
  billed: { cacheRead: null, cacheWrite: null, input: 0, output: 0 },
  cacheEquivalent: null,
  cost: { cacheRead: null, cacheWrite: null, input: 0, output: 0 },
  equivalentSplit: null,
  fromRequests: true,
  keptWarm: null,
  planOutputTokens: 0,
  planRequests: 0,
  probeEquivalent: null,
  savingEquivalent: null,
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

/** A user agent as the ledger keys a rollup by it: its product token, `-` for a request that sent none. */
export const agentTokenOf = (userAgent: string | null): string => (userAgent === null ? '-' : foldUserAgent(userAgent));

/**
 * Whether an uncached request could never have read a cache, as the router counts `uncacheable`: a token count whatever
 * its body marked, and a request whose body carried no cache breakpoint. A row stored before the router kept the marker
 * counts by its kind alone.
 */
export const couldNotCache = (r: Pick<RequestLog, 'cacheMarker' | 'kind'>): boolean => r.kind === 'count_tokens' || r.cacheMarker === false;

/**
 * A client that left before its answer ended, which the ledger counts apart from the failures and files under no error
 * code. A probe is the keepalive's own request, whose cancel stays a failure.
 */
export const clientLeft = (r: Pick<RequestLog, 'errorCode' | 'kind' | 'outcome'>): boolean => r.outcome === 'canceled' && r.errorCode === 'client_gone' && r.kind !== 'probe';

const keyOf = (r: RequestLog, by: AnalyticsGroupBy, agentGroups: string[]): string => {
  if (by === 'account') return r.accountId ?? '-';
  if (by === 'client') return r.client ?? '-';
  // The effort and the request class key as the rollups fold them, so a session view's group is the value the
  // Requests filters match.
  if (by === 'effort') return foldEffort(r.shape.effort);
  // A key id created again is another holder, so the group is the lifetime, which a `keys=` selector names too.
  if (by === 'key') return keyLifetime(r.keyId ?? '-', r.keyGeneration);
  if (by === 'kind') return r.kind;
  // The canonical id, as the rollups file a request and the Models filter matches it, so a `[1m]` spelling or an
  // alias joins the group of the model it resolved to.
  if (by === 'model') return r.canonicalModel;
  if (by === 'pool') return r.poolId ?? '-';
  if (by === 'request_class') return foldRequestClass(r.requestClass);
  if (by === 'classifier') return foldClassifier(r.classifier);
  if (by === 'speed') return foldSpeed(r);
  if (by === 'user_agent') {
    // The rollups key an agent by its token and file a row that named no client under '-', which every route
    // answers as unattributed; under a pattern list the token falls with every other unmatched client under
    // 'other', as the breakdown route does.
    const agent = agentTokenOf(r.userAgent);
    if (agentGroups.length === 0) return agent;
    return agentGroups.find((p) => matchesPattern(p, agent)) ?? 'other';
  }
  return 'all';
};

export interface FoldOptions {
  /** Patterns naming the groups under `user_agent`; a row falls in the first that matches, the rest under `other`. */
  agentGroups?: string[];
  /** The side the kept rows were paid on, read off each row's serving attempt. Absent keeps every row. */
  funding?: FundingFilter;
  /** The Include probes toggle. False keeps a probe's request and tokens out and counts a billed probe's dollars, as
   *  the rollups do under the toggle off. Absent counts a probe row whole. */
  includeProbes?: boolean;
}

export interface PatternOptions {
  /**
   * Patterns a row's folded user agent must match one of. A row that named none is matched as `-`, as the rollups
   * file it.
   */
  agentPatterns?: string[];
  /** Patterns a row's client class must match one of. A row with none is matched as `-`, as the rollups file it. */
  clientPatterns?: string[];
}

/** The newest generation of each key id among the rows, the one the breakdown labels with the bare id. */
export function latestGenerations(rows: RequestLog[]): Map<string, number> {
  const latest = new Map<string, number>();
  for (const r of rows) if (r.keyId !== null) latest.set(r.keyId, Math.max(latest.get(r.keyId) ?? r.keyGeneration, r.keyGeneration));
  return latest;
}

/**
 * The breakdown route's labels: `unattributed` for a row no key or client was read for, `none` for the reserved id
 * under the classifier grouping, as the Requests facet names it, and under `key` the id alone for its newest generation
 * and `<id> (deleted, created Sep 14, 2026, 09:31)` for an earlier one (`<id> (deleted)` at generation 0). The rows
 * stand in for the router's registry here, so the newest generation among them reads as the live one. The model
 * grouping's empty key reads `no model`, the route's label for it.
 */
function labelOf(r: RequestLog, key: string, by: AnalyticsGroupBy, latest: Map<string, number>): string {
  if (by === 'model' && key === '') return 'no model';
  if (by === 'classifier' && key === '-') return 'none';
  if (by !== 'key') return key === '-' ? 'unattributed' : key;
  if (r.keyId === null) return 'unattributed';
  return latest.get(r.keyId) === r.keyGeneration ? r.keyId : earlierHolderLabel(r.keyId, r.keyGeneration);
}

/**
 * Who paid for a request: its serving attempt's side. A row no attempt served files under `plan`, the ledger's filing,
 * which `onSide` reads off the empty trail, so the `api_key` answer here only keeps such a row out of the plan shares
 * it has none of.
 */
const fundingOfRow = (r: RequestLog) => fundingOf(servingAttempt(r));
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

/**
 * A request row's Pro-week figure into its group's, as the ledger sums a group: a billed row's null adds nothing, and a
 * plan-funded row with tokens the cost model could not price nulls the group, so no partial sum reads as the whole. A
 * plan-funded row with no token prices nothing, as the ledger skips it.
 */
const addFigure = (sum: number | null, r: RequestLog): number | null => {
  if (sum === null) return null;
  if (r.costEquivalent !== null) return sum + r.costEquivalent;
  return onSide(r, 'plan') && tokensTotal(r.usage) > 0 ? null : sum;
};

/**
 * The plan-funded rows' shares summed per window into the group, as the route sums them under the account grouping:
 * a window no row's model applies to is no key, a window a row carries as null (the cost model could not price it) is
 * null whatever the other rows priced, and the rest sum. A row whose account the route could not price (`{}`, or
 * no map at all) nulls the map, as the route answers such an account, since the fold holds no window rule of its own
 * to say which windows that row's model applies to. No plan-funded row is no shares. A plan-funded row with no token
 * (a failed attempt the route priced nothing for) adds nothing to the shares, as the ledger's sums skip it: its empty
 * map would otherwise read as an account the route could not price and dash the priced rows' total.
 */
function foldShares(g: AnalyticsGroup, rows: RequestLog[]) {
  const plan = rows.filter((r) => fundingOfRow(r) === 'plan');
  const priced = plan.filter((r) => tokensTotal(r.usage) > 0);
  if (priced.length === 0 || priced.some((r) => r.costShares === null || Object.keys(r.costShares).length === 0)) return;
  const shares: Record<string, number | null> = {};
  for (const r of priced) {
    for (const [window, share] of Object.entries(r.costShares ?? {})) {
      const sum = shares[window];
      shares[window] = share === null || sum === null ? null : (sum ?? 0) + share;
    }
  }
  g.costShares = shares;
}

/** `read(key)` the first time `key` comes up, the answer `seen` holds for it after that. */
const once = <K, V>(seen: Map<K, V>, key: K, read: (key: K) => V): V => {
  const held = seen.get(key);
  if (held !== undefined) return held;
  const v = read(key);
  seen.set(key, v);
  return v;
};

/**
 * The rows the pattern lists keep. The agent is matched on its folded token, as the unscoped page's rollups hold it, so
 * one pattern selects the same rows under both scopes. The token is folded once per distinct header ahead of the
 * patterns, since the fold splits the header.
 */
export function keptRows(rows: RequestLog[], options: PatternOptions): RequestLog[] {
  const agentPatterns = options.agentPatterns ?? [];
  const clientPatterns = options.clientPatterns ?? [];
  if (agentPatterns.length === 0 && clientPatterns.length === 0) return rows;
  const agentOk = new Map<string | null, boolean>();
  const clientOk = new Map<string | null, boolean>();
  const agentKept = (userAgent: string | null) => {
    const agent = agentPatterns.length > 0 ? agentTokenOf(userAgent) : null;
    return agent === null || agentPatterns.some((p) => matchesPattern(p, agent));
  };
  const clientKept = (client: string | null) => !clientPatterns.length || clientPatterns.some((p) => matchesPattern(p, client ?? '-'));
  return rows.filter((r) => once(agentOk, r.userAgent, agentKept) && once(clientOk, r.client, clientKept));
}

/**
 * One request's share of the dropped-block counts, as the router's rollup folds it off the serving attempt's answer:
 * one reported for an answer that carries the member, one under each key whatever its blocks, since the readers count
 * requests, one under `none` for an answer that listed nothing, one moved where the request moved and a key names an
 * account reason, and nothing for an answer without the member.
 */
export function countDropped(into: Pick<AnalyticsGroup, 'inputTransformations' | 'inputTransformationsMoved' | 'inputTransformationsNone' | 'inputTransformationsReported'>, dropped: InputTransformations | null, moved: boolean): void {
  if (dropped === null) return;
  into.inputTransformationsReported += 1;
  const keys = Object.keys(dropped);
  for (const key of keys) addCount(into.inputTransformations, key);
  if (keys.length === 0) into.inputTransformationsNone += 1;
  if (moved && keys.some(accountBound)) into.inputTransformationsMoved += 1;
}

/**
 * Request rows summed the way the rollups sum them, for the one filter the rollups cannot answer: a session.
 * `latest` is taken over every row of the range, so a bucket holding an earlier key holder alone still labels it
 * deleted.
 */
export function foldGroups(rows: RequestLog[], by: AnalyticsGroupBy, options: FoldOptions = {}, latest = latestGenerations(rows)): AnalyticsGroup[] {
  const { funding } = options;
  const agentGroups = options.agentGroups ?? [];
  // The `user_agent` key reads the header alone, so each distinct header is folded and matched once.
  const agentKeys = new Map<string | null, string>();
  const groups = new Map<string, { group: AnalyticsGroup; rows: RequestLog[] }>();
  for (const r of funding === undefined ? rows : rows.filter((row) => onSide(row, funding))) {
    const key = by === 'user_agent' ? once(agentKeys, r.userAgent, () => keyOf(r, by, agentGroups)) : keyOf(r, by, agentGroups);
    const entry = groups.get(key) ?? { group: emptyFolded(key, labelOf(r, key, by, latest)), rows: [] };
    groups.set(key, entry);
    const g = entry.group;
    if (heldProbe(r)) {
      g.probes.held += 1;
      continue;
    }
    // The rollups' rule under the toggle off: a probe's request and tokens stay out, and a billed probe's dollars
    // count.
    if (options.includeProbes === false && r.kind === 'probe') {
      if (!onSide(r, 'plan')) {
        g.costUsd += r.costUsd;
        g.billedUsd += r.costUsd;
      }
      continue;
    }
    // The ledger's rule once more: a refused request counts under the map and `refused` and under nothing else.
    if (r.refused) {
      g.refused += 1;
      if (r.errorCode) addCount(g.refusals, r.errorCode);
      continue;
    }
    entry.rows.push(r);
    g.requests += 1;
    // A client that hangs up is no failure of the route, whatever status the row holds, so its cancel counts apart and
    // its code stays out of the errors. A probe's cancel and the router's own fall to the arms below.
    const left = clientLeft(r);
    if (r.outcome === 'succeeded') g.succeeded += 1;
    else if (r.outcome === 'exhausted') g.exhausted += 1;
    else if (r.outcome === 'rejected') g.rejected += 1;
    else if (r.outcome === 'incomplete') g.incomplete += 1;
    else if (left) g.clientCanceled += 1;
    else if (r.outcome === 'failed' && r.status === 429) g.rateLimited += 1;
    else g.failed += 1;
    if (r.errorCode && !left) addCount(g.errors, r.errorCode);
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
    // The billed side is the funding filter's complement of the plan. The route counts the plan rows under every
    // grouping, so the fold does too, and a group whose plan rows priced nothing then reads null rather than as a group
    // with no plan row.
    if (onSide(r, 'plan')) {
      g.planRequests = (g.planRequests ?? 0) + 1;
      g.planOutputTokens = (g.planOutputTokens ?? 0) + r.usage.output;
    } else {
      g.billedUsd += r.costUsd;
    }
    g.costEquivalent = addFigure(g.costEquivalent, r);
    // The ledger classifies by tokens: a read is a hit, a write without a read is a miss, neither is uncached.
    // The diagnosed reason is a breakdown of its own and decides nothing here.
    if (r.usage.cacheRead > 0) g.cacheHits += 1;
    else if (r.usage.cacheWrite5m + r.usage.cacheWrite1h > 0) g.cacheMisses += 1;
    else {
      g.uncached += 1;
      if (couldNotCache(r)) g.uncacheable += 1;
    }
    const reason = servedMissReason(r);
    if (reason) {
      addCount(g.cacheMissReasons, reason);
      if (hitUnderReason(r)) addCount(g.cacheMissReasonHits, reason);
    }
    if (expiredBetweenTurns(r)) g.expiredBetweenTurns += 1;
    const served = servingAttempt(r);
    countDropped(g, served?.response.inputTransformations ?? null, (served?.selection?.moved ?? null) !== null);
  }
  for (const { group: g, rows: list } of groups.values()) {
    const samples = list.map(latencySamples);
    for (const m of METRICS) {
      g.histograms[m] = histogramOf(samples.map((s) => s[m]).filter((v): v is number => v !== null));
      g.percentiles[m] = quartet(g.histograms[m]);
    }
    // Shares of one account's windows add, and a group of any other dimension spans accounts, whose windows do not.
    if (by === 'account') foldShares(g, list);
  }
  // Under the toggle off the route leaves out a group whose rows are all probes, unless its billed probes hold dollars.
  // A group of refused rows alone stays, as the route keeps one.
  return [...groups.values()]
    .filter((e) => options.includeProbes !== false || e.group.requests > 0 || e.group.refused > 0 || e.group.billedUsd > 0)
    .map((e) => e.group)
    .sort((a, b) => b.requests - a.requests || a.key.localeCompare(b.key));
}

/** Request rows bucketed and summed per bucket, `latest` taken over every row of the range as `foldGroups` takes it. */
export function foldRequests(rows: RequestLog[], bucketMs: number, from: string, to: string, by: AnalyticsGroupBy = 'all', options: FoldOptions = {}, latest = latestGenerations(rows)): AnalyticsPoint[] {
  const end = Date.parse(to);
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
  return buckets.map((inBucket, i) => ({ at: iso(start + i * bucketMs), groups: foldGroups(inBucket, by, options, latest) }));
}

/** A session's requests as the log answered them, partial where the page bound stopped the walk short of the end. */
export interface SessionRequests {
  partial: boolean;
  requests: RequestLog[];
}

/**
 * Every page of a session's requests, following the cursor to its end or to `maxPages`, which marks the result partial.
 * Given the session's held answer, it reads the first page alone, so a re-read costs one request. The page's rows
 * replace the held rows by id, since a request that landed since pushes a held row past the page's edge, and the held
 * rows the page did not answer stand as loaded, the partial mark with them.
 */
export async function loadSessionRequests(load: (cursor: string | undefined) => Promise<RequestPage>, maxPages: number, held?: SessionRequests): Promise<SessionRequests> {
  if (held !== undefined) {
    const first = await load(undefined);
    const answered = new Set(first.requests.map((r) => r.id));
    return { partial: held.partial, requests: [...first.requests, ...held.requests.filter((r) => !answered.has(r.id))] };
  }
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

/** Figures summed with a null on either side reading null, so no partial sum reads as the whole. */
export const plus = (a: number | null, b: number | null): number | null => (a === null || b === null ? null : a + b);
/** A count a fold may lack: it stays unknown only while no group carries one. */
const plusKnown = (a: number | null, b: number | null): number | null => (a === null ? b : b === null ? a : a + b);
const plusSplit = (a: AnalyticsGroup['cost'], b: AnalyticsGroup['cost']): AnalyticsGroup['cost'] => ({ cacheRead: plus(a.cacheRead, b.cacheRead), cacheWrite: plus(a.cacheWrite, b.cacheWrite), input: a.input + b.input, output: a.output + b.output });
/** Two figures of one shape summed member by member, null where either side is. */
const plusBoth = <K extends string>(a: Record<K, number> | null, b: Record<K, number> | null): Record<K, number> | null =>
  a === null || b === null ? null : (Object.fromEntries((Object.keys(a) as K[]).map((k) => [k, a[k] + b[k]])) as Record<K, number>);

/**
 * Several groups of one point summed into one, for the charts and tiles that draw totals. A figure one group
 * cannot supply leaves the sum unknown, so a total never passes a partial sum off as the whole: a group whose
 * Pro-week figure, cache, probe or saving figure is null nulls the sum's. The plan counts are the exception: a group
 * that carries none is skipped, and the groups that carry them still total.
 */
export function sumGroups(groups: AnalyticsGroup[]): AnalyticsGroup {
  const out = empty('all');
  for (const g of groups) {
    for (const k of ['attempts', 'billedUsd', 'cacheHits', 'cacheMisses', 'clientCanceled', 'costUsd', 'exhausted', 'expiredBetweenTurns', 'failed', 'failovers', 'incomplete', 'inputTransformationsMoved', 'inputTransformationsNone', 'inputTransformationsReported', 'probeCostUsd', 'rateLimited', 'refused', 'rejected', 'requests', 'savingUsd', 'succeeded', 'uncacheable', 'uncached', 'wastedProbeUsd', 'wastedProbes'] as const) out[k] += g[k];
    out.cost = plusSplit(out.cost, g.cost);
    out.billed = plusSplit(out.billed, g.billed);
    out.costEquivalent = plus(out.costEquivalent, g.costEquivalent);
    out.cacheEquivalent = plusBoth(out.cacheEquivalent, g.cacheEquivalent);
    out.probeEquivalent = plusBoth(out.probeEquivalent, g.probeEquivalent);
    out.savingEquivalent = plus(out.savingEquivalent, g.savingEquivalent);
    out.equivalentSplit = plusBoth(out.equivalentSplit, g.equivalentSplit);
    out.planRequests = plusKnown(out.planRequests, g.planRequests);
    out.planOutputTokens = plusKnown(out.planOutputTokens, g.planOutputTokens);
    for (const k of ['cacheRead', 'cacheWrite1h', 'cacheWrite5m', 'output', 'thinking', 'uncachedInput', 'webFetch', 'webSearch'] as const) out.tokens[k] += g.tokens[k];
    for (const k of ['cacheMissReasonHits', 'cacheMissReasons', 'errors', 'inputTransformations', 'refusals'] as const) addCounts(out[k], g[k]);
    if (g.fromRequests) out.fromRequests = true;
    out.keptWarm = plusBoth(out.keptWarm, g.keptWarm);
    for (const k of ['failed', 'held', 'ok', 'planOk', 'skipped'] as const) out.probes[k] += g.probes[k];
    for (const reason of WASTED_PROBE_REASONS) out.wastedProbeReasons[reason] += g.wastedProbeReasons[reason];
  }
  for (const m of METRICS) {
    out.histograms[m] = mergeHistograms(groups.map((g) => g.histograms[m]));
    out.percentiles[m] = quartet(out.histograms[m]);
  }
  return out;
}
