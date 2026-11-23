import { agentTokenOf, plus } from '@/api/fold';
import type {
  Account, AccountRollup, AllowanceReset, AnalyticsGroup, AnalyticsGroupBy, AnalyticsPoint, AnalyticsQuery, BurnGroup, BurnPoint, CacheMissReason, CostSplit, HeldPoint, LatencyHistograms, PinMigrationReason, PlacementRow, QuotaHistory, Reconciliation, ReconciliationsAnswer,
  RefusalCategoriesAnswer, RefusalCategoryRow, RequestKind, RequestLog, RewritePoint, RewritesAnswer, RoutingEvent, SubscriptionCosts, UnusedAllowanceAnswer, WastedProbeReason,
} from '@/api/types';
import { bucketShare, keptWarmTokens } from '@/lib/analytics';
import { bucketSizeFor } from '@/lib/buckets';
import { WASTED_PROBE_REASONS, emptyCacheMissReasons, emptyWastedProbeReasons } from '@/lib/cache';
import { anchorTier, equivalentIn, modelWeeks, pricedWindow, shareIn, tableAt, tierCapacity, type CostWindow, type Usage } from '@/lib/costs';
import { addCounts, emptyCounts, groupInto } from '@/lib/counts';
import { foldRequestClass, servingResponse } from '@/lib/dimensions';
import { byteOrder, earlierHolderLabel, placementHit } from '@/lib/format';
import { mergeHistograms, quartet } from '@/lib/histogram';
import { matchesPattern } from '@/lib/pattern';
import { endOf } from '@/lib/quota';
import { WINDOW_KEY, hoursOfKey } from '@/lib/windows';
import { accountById, burnUnitOf, sharedWindowsOf } from './accounts';
import { earliest } from './lead';
import { keyById } from './pools';
import { ZERO_OUTCOMES, bandAt, freedPart, requestsIn, sumEach, sumOf, sumSplit, tokensIn } from './requests';
import { createRng } from './rng';
import { round } from './round';
import { WINDOW_DROP_MARGIN, forcedReasons, readingPairAt, subscriptionPair, wholePoints, type ClaimDetail, type DropDetail, type EventSeed } from './routingLog';
import { TABLES, tablesFor } from './samples';
import { DAY, HOUR, MINUTE, NOW, clock, hourCeil, iso, startOfHour } from './time';

/**
 * One hour of one lane of the fixture ledger. The admin routes answer buckets and groups rather than rows,
 * so this shape is the mock's own raw material for the fold the router does server-side.
 */
export interface HourlyRollup {
  accountId: string;
  /** Every attempt of the row's requests, a retry on the same account among them, as the ledger counts them. */
  attempts: number;
  cacheHits: number;
  cacheMisses: number;
  /** Per reason, the requests under it that `hitUnderReason` reads as a hit. A reason with none is no key. */
  cacheMissReasonHits: Record<CacheMissReason, number>;
  /** Answered attempts, counted by the word each filed: the upstream's reason or the relay's own. */
  cacheMissReasons: Record<CacheMissReason, number>;
  /**
   * `client` for the client's own classifier call on the account and incarnation its judged turn ended on, `client_off`
   * where it ran off them or in a session with no turn to judge, else `-`.
   */
  classifier: string;
  /** The class the router derived from the requests' own agent, `-` for a row whose requests carry none. */
  client: string;
  cost: CostSplit;
  costUsd: number;
  /** The effort the hour's requests asked for; the rollups file an hour that asked for none under `none`. */
  effort: string;
  /** The hour's Pro-week figure, priced and summed as `shares` are, under no account tier: null where a request with
   *  tokens priced nothing. */
  equivalent: number | null;
  /**
   * The hour's served requests by the code each carries, as the ledger's errors map counts them: a refused request
   * files under `refusals`, and a client's cancel of a request that is no probe under no code.
   */
  errors: Record<string, number>;
  /**
   * Requests that `expiredBetweenTurns` reads as a turn whose cache expired: under half the size the pin's last report
   * left. The reason counts cannot be split this way once summed, which is why the ledger carries it.
   */
  expiredBetweenTurns: number;
  /** Attempts that moved to another member before the request succeeded. */
  failovers: number;
  /**
   * Who paid for the hour's requests, as the ledger keys its rollups off the served answer: `billed` for an API key
   * account's answer whatever its speed, else `usage_based` for a usage-based seat's, else a fast answer's credits,
   * else the head's overage, else the plan, which a request no attempt served files under too.
   */
  funding: 'billed' | 'fast' | 'overage' | 'plan' | 'usage_based';
  histograms: LatencyHistograms;
  hour: string;
  /** The requests per key of the blocks their serving attempt's answer dropped, each counted once under each key. */
  inputTransformations: Record<string, number>;
  /** The moved requests whose serving attempt's answer dropped a block under one of the two account reasons. */
  inputTransformationsMoved: number;
  /** The requests whose serving attempt's answer listed no dropped block. */
  inputTransformationsNone: number;
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
  /** The refused requests of the row by error code, counted here and under none of `requests`. */
  refusals: Record<string, number>;
  /** The class the lane's requests sent; the rollups file a row with none, a probe's among them, under `-`. */
  requestClass: string;
  /** `client_gone` counts the streams the client closed and `router_shutdown` the requests the router's own stop cut,
   *  with or without a head, both filed under the canceled outcome as the ledger files them. `rejected` counts the
   *  requests the relay rejected and `exhausted` those their pool exhausted, which the counts by status leave out. */
  requests: { client_gone: number; exhausted: number; incomplete: number; ok: number; overloaded: number; rate_limited: number; rejected: number; router_shutdown: number; undecodable: number; upstream_error: number };
  /** The kept-warm requests' saving in dollars, each priced at the cells its own prompt selects. */
  savingUsd: number;
  /** The fixture rolls up per session, which keeps its lanes as fine as the ledger's; no route groups on one. */
  sessionId: string;
  /**
   * The hour's plan-window shares on the windows the account's set has and the model's weeks, each request priced at
   * its own band and completion and summed. A window any request prices null reads null.
   */
  shares: Record<string, number | null>;
  /** The speed the hour's answers reported, else `fast` where its requests asked for it, else `standard`. */
  speed: 'fast' | 'standard';
  /** The account tier the hour's requests landed under, null for an API key account. */
  tier: string | null;
  tokens: { cacheRead: number; cacheWrite1h: number; cacheWrite5m: number; output: number; thinking: number; uncachedInput: number; webFetch: number; webSearch: number };
  /** The part of `uncached` that `couldNotCache` reads as requests that could never read a cache. */
  uncacheable: number;
  uncached: number;
  /** The requests' own agent folded to its product token, as the ledger keys a row, `-` for none. */
  userAgent: string;
  /** Probes no real request came back for, settled when the recipe stopped or the request after them read no cache. */
  wastedProbes: number;
  wastedProbeReasons: Record<WastedProbeReason, number>;
  wastedProbeUsd: number;
}

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
  user_agent: (r) => r.userAgent,
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

/**
 * Whether a row falls on the side the funding filter keeps, as the route reads it off the row's own funding: `plan` is
 * a plan-funded row, `billed` its complement, and the registry's current kinds move no row.
 */
function passesFunding(r: HourlyRollup, funding: AnalyticsQuery['funding']): boolean {
  if (funding === undefined) return true;
  return funding === 'plan' ? r.funding === 'plan' : r.funding !== 'plan';
}

/** Whether a row passes the account, key and pool filters, an empty or missing list passing every row. */
function passesMembers(r: HourlyRollup, q: Pick<AnalyticsQuery, 'accountIds' | 'keyIds' | 'poolIds'>): boolean {
  if (q.accountIds?.length && !q.accountIds.includes(r.accountId)) return false;
  if (q.keyIds?.length && !q.keyIds.some((selector) => selectsKey(selector, r))) return false;
  if (q.poolIds?.length && !q.poolIds.includes(r.poolId)) return false;
  return true;
}

function inQuery(r: HourlyRollup, q: AnalyticsQuery, hour: number, fromHour: number, toCeiling: number): boolean {
  // As the route reads a range: from the hour holding the start up to the hour ceiling of the end, exclusive.
  if (hour < fromHour || hour >= toCeiling) return false;
  if (!passesFunding(r, q.funding)) return false;
  if (!passesMembers(r, q)) return false;
  if (q.kinds?.length && !q.kinds.includes(r.kind)) return false;
  if (q.models?.length && !q.models.includes(r.model)) return false;
  if (q.userAgents?.length && !q.userAgents.some((p) => matchesPattern(p, r.userAgent))) return false;
  if (q.clients?.length && !q.clients.some((p) => matchesPattern(p, r.client))) return false;
  return true;
}

/** One window's running share and whether a row could not be priced on it, which drops it from the answer. */
interface WindowSum {
  sum: number;
  unpriced: boolean;
}

const planRows = (list: HourlyRollup[]) => list.filter((r) => r.funding === 'plan');

/**
 * A subscription account's own-window shares over its plan-funded rows with tokens, summed as the route sums a group: a
 * window any row prices null is left out, as the route answers it (a request row keeps such a window as a key with
 * null), and a row with no token prices nothing.
 */
function costSharesOf(plan: HourlyRollup[]): Record<string, number> {
  const sums = new Map<string, WindowSum>();
  for (const r of plan) {
    if (tokensIn(r) === 0) continue;
    for (const [window, share] of Object.entries(r.shares)) {
      const s = sums.get(window) ?? { sum: 0, unpriced: false };
      if (share === null) s.unpriced = true;
      else s.sum += share;
      sums.set(window, s);
    }
  }
  return Object.fromEntries([...sums.entries()].filter(([, s]) => !s.unpriced).map(([window, s]) => [window, s.sum]));
}

/** A row's kept-warm counts as the usage their saving prices: each count the write it avoided at its TTL, and the two
 *  together the read the requests paid instead. */
const keptUsageOf = (r: HourlyRollup): Usage => ({ cacheRead: keptWarmTokens(r.keptWarm), cacheWrite1h: r.keptWarm.tokens1h, cacheWrite5m: r.keptWarm.tokens5m, input: 0, output: 0 });

const TOKEN_KINDS: Record<keyof CostSplit, (u: Usage) => Usage> = {
  cacheRead: (u) => ({ cacheRead: u.cacheRead, cacheWrite1h: 0, cacheWrite5m: 0, input: 0, output: 0 }),
  cacheWrite: (u) => ({ cacheRead: 0, cacheWrite1h: u.cacheWrite1h, cacheWrite5m: u.cacheWrite5m, input: 0, output: 0 }),
  input: (u) => ({ cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: 0, input: u.input, output: 0 }),
  output: (u) => ({ cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: 0, input: 0, output: u.output }),
};

/** The rows' figures, priced once: neither a row nor the cost model changes after load, and a hand-built row is a key
 *  of its own. */
const pricedRows = new WeakMap<HourlyRollup, Record<keyof CostSplit | 'keptRead' | 'keptWrite', number | null>>();
const fiveHourRows = new WeakMap<HourlyRollup, number | null>();
const tierRows = new WeakMap<HourlyRollup, Record<'equivalent' | 'fable' | 'fiveHour', Record<string, number | null>>>();

/**
 * A row's Pro-week figures at the row's hour, under the regime its client label names: its four token kinds, and the
 * kept-warm credit's cache read and cache writes. The band is read once where the model has a 7-day weight, and a
 * figure is null where the weight or the band's reading is missing. A row of freed classifier calls reads zero on each
 * figure, as the route answers it.
 */
function figuresOf(r: HourlyRollup): Record<keyof CostSplit | 'keptRead' | 'keptWrite', number | null> {
  if (freedPart(r)) return { cacheRead: 0, cacheWrite: 0, input: 0, keptRead: 0, keptWrite: 0, output: 0 };
  const known = pricedRows.get(r);
  if (known) return known;
  const inside = bandAt(r.model, Date.parse(r.hour));
  const tables = tablesFor(r.client);
  const figure = (part: Usage) => (inside === null ? null : equivalentIn(tables, r.model, part, inside));
  const usage = usageOf(r);
  const kept = keptUsageOf(r);
  const figures = {
    cacheRead: figure(TOKEN_KINDS.cacheRead(usage)),
    cacheWrite: figure(TOKEN_KINDS.cacheWrite(usage)),
    input: figure(TOKEN_KINDS.input(usage)),
    keptRead: figure(TOKEN_KINDS.cacheRead(kept)),
    keptWrite: figure(TOKEN_KINDS.cacheWrite(kept)),
    output: figure(TOKEN_KINDS.output(usage)),
  };
  pricedRows.set(r, figures);
  return figures;
}

/**
 * The group's Pro-week figures over its plan-funded rows, summed under no account tier, so they add across accounts
 * and read under every grouping: the rows' own figures, each request priced at its completion, the cache read and
 * write and the split by token kind priced at each row's hour, the plan probe rows apart with their wasted part at the
 * wasted-probe dollars over the probe-cost dollars, and the kept-warm saving, the writes the credits avoided less the
 * reads they paid. A plan row with tokens on a model without a weight nulls each figure it joins, as the route answers
 * it, and a row with no token prices nothing.
 */
function equivalentsOfGroup(countedPlan: HourlyRollup[], probePlan: HourlyRollup[]): Pick<AnalyticsGroup, 'cacheEquivalent' | 'costEquivalent' | 'equivalentSplit' | 'probeEquivalent' | 'savingEquivalent'> {
  let cost: number | null = 0;
  let read: number | null = 0;
  let write: number | null = 0;
  let saving: number | null = 0;
  const split: CostSplit = { cacheRead: 0, cacheWrite: 0, input: 0, output: 0 };
  for (const r of countedPlan) {
    if (tokensIn(r) > 0) {
      cost = plus(cost, r.equivalent);
      const figures = figuresOf(r);
      read = plus(read, figures.cacheRead);
      write = plus(write, figures.cacheWrite);
      for (const kind of Object.keys(TOKEN_KINDS) as (keyof CostSplit)[]) split[kind] += figures[kind] ?? 0;
    }
    if (keptWarmTokens(r.keptWarm) > 0) {
      const { keptRead, keptWrite } = figuresOf(r);
      saving = keptRead === null || keptWrite === null ? null : plus(saving, keptWrite - keptRead);
    }
  }
  let probe: number | null = 0;
  let probeUsd = 0;
  let wastedUsd = 0;
  for (const r of probePlan) {
    probeUsd += r.probeCostUsd;
    wastedUsd += r.wastedProbeUsd;
    if (tokensIn(r) > 0) probe = plus(probe, r.equivalent);
  }
  const ratio = probeUsd > 0 ? wastedUsd / probeUsd : 0;
  return {
    cacheEquivalent: read === null || write === null ? null : { read, write },
    costEquivalent: cost,
    equivalentSplit: cost === null ? null : split,
    probeEquivalent: probe === null ? null : { cost: probe, wasted: probe * ratio },
    savingEquivalent: saving,
  };
}

/**
 * The account a group's shares price under: the group's own under the account grouping, else the one account the
 * query names, as the route's gate reads it, since shares of different accounts' windows do not add. Null otherwise.
 */
const shareAccountOf = (key: string, groupBy: AnalyticsGroupBy | BurnGroup, accountIds: string[] | undefined): string | null => (groupBy === 'account' ? key : accountIds?.length === 1 ? accountIds[0] : null);

// The zeros a group's member-wise sums start from, the token one in the key order the answer carries.
const ZERO_KEPT: HourlyRollup['keptWarm'] = { requests: 0, tokens1h: 0, tokens5m: 0 };
const ZERO_PROBES: HourlyRollup['probes'] = { failed: 0, held: 0, ok: 0, skipped: 0 };
const ZERO_TOKENS: HourlyRollup['tokens'] = { cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: 0, output: 0, thinking: 0, uncachedInput: 0, webFetch: 0, webSearch: 0 };

/**
 * Two sums per group, the way the route splits them: the request side, which a probe row joins only when probes
 * count as requests, and the probe rows alone, which the probe counts, the probe cost and the wasted probes read. A
 * billed probe draws real dollars, so they join the cost and billed sums whatever the flag says, while its request and
 * its tokens follow the flag. The shares are a subscription account's with a tier, under the account grouping or where
 * the query names that one account, and null otherwise. The equivalents, the billed side and the plan counts read
 * under every grouping.
 */
function groupOf(key: string, label: string, rows: HourlyRollup[], includeProbes: boolean, groupBy: AnalyticsGroupBy, accountIds: string[] | undefined): AnalyticsGroup {
  const counted = includeProbes ? rows : rows.filter((r) => r.kind !== 'probe');
  const priced = includeProbes ? rows : rows.filter((r) => r.kind !== 'probe' || r.funding !== 'plan');
  const probeRows = rows.filter((r) => r.kind === 'probe');
  const account = shareAccountOf(key, groupBy, accountIds);
  const tier = account === null ? undefined : accountById.get(account)?.subscription?.rateLimitTier;
  const countedPlan = planRows(counted);
  const probePlan = planRows(probeRows);
  const costShares = account !== null && accountById.get(account)?.kind === 'oauth' && tier !== undefined ? costSharesOf(countedPlan) : null;
  const billed = priced.filter((r) => r.funding !== 'plan');

  const cacheMissReasons = emptyCacheMissReasons();
  // Every name a row carries, as the ledger rolls up whatever the beta sent, not only the ones the console knows.
  for (const r of counted) addCounts(cacheMissReasons, r.cacheMissReasons);
  const cacheMissReasonHits = emptyCounts();
  for (const r of counted) addCounts(cacheMissReasonHits, r.cacheMissReasonHits);
  const inputTransformations = emptyCounts();
  for (const r of counted) addCounts(inputTransformations, r.inputTransformations);
  const refusals = emptyCounts();
  for (const r of counted) addCounts(refusals, r.refusals);
  const wastedProbeReasons = emptyWastedProbeReasons();
  for (const r of probeRows) for (const reason of WASTED_PROBE_REASONS) wastedProbeReasons[reason] += r.wastedProbeReasons[reason];

  const merged = (m: keyof LatencyHistograms) => mergeHistograms(counted.map((r) => r.histograms[m]));
  const histograms: LatencyHistograms = { durationMs: merged('durationMs'), outputTps: merged('outputTps'), ttfbMs: merged('ttfbMs'), ttftMs: merged('ttftMs') };

  const outcomes = sumEach(counted.map((r) => r.requests), ZERO_OUTCOMES);
  // A client that hangs up is no failure of the route, so the cancels on the rows that are no probe count apart. A
  // probe is the keepalive's own request, whose cancel stays a failure.
  const clientCanceled = sumOf(counted.filter((r) => r.kind !== 'probe'), (r) => r.requests.client_gone);
  const requests = sumOf(counted, requestsIn);
  const failovers = sumOf(counted, (r) => r.failovers);
  const errors = emptyCounts();
  for (const r of counted) addCounts(errors, r.errors);

  const kept = sumEach(counted.map((r) => r.keptWarm), ZERO_KEPT);
  const planKept = sumEach(countedPlan.map((r) => r.keptWarm), ZERO_KEPT);
  const probes = sumEach(probeRows.map((r) => r.probes), ZERO_PROBES);

  return {
    ...equivalentsOfGroup(countedPlan, probePlan),
    attempts: sumOf(counted, (r) => r.attempts),
    billed: sumSplit(billed.map((r) => r.cost)),
    billedUsd: sumOf(billed, (r) => r.costUsd),
    cacheHits: sumOf(counted, (r) => r.cacheHits),
    cacheMisses: sumOf(counted, (r) => r.cacheMisses),
    cacheMissReasonHits,
    cacheMissReasons,
    clientCanceled,
    cost: sumSplit(priced.map((r) => r.cost)),
    costShares,
    costUsd: sumOf(priced, (r) => r.costUsd),
    errors,
    exhausted: outcomes.exhausted,
    expiredBetweenTurns: sumOf(counted, (r) => r.expiredBetweenTurns),
    failed: outcomes.overloaded + outcomes.upstream_error + outcomes.router_shutdown + outcomes.client_gone - clientCanceled,
    failovers,
    histograms,
    incomplete: outcomes.incomplete,
    inputTransformations,
    inputTransformationsMoved: sumOf(counted, (r) => r.inputTransformationsMoved),
    inputTransformationsNone: sumOf(counted, (r) => r.inputTransformationsNone),
    // A row's funding is its credited request's, so the plan-funded credits are the plan rows' own.
    keptWarm: { planRequests: planKept.requests, planTokens1h: planKept.tokens1h, planTokens5m: planKept.tokens5m, ...kept },
    key,
    keyCreatedAt: null,
    keyId: null,
    label,
    percentiles: { durationMs: quartet(histograms.durationMs), outputTps: quartet(histograms.outputTps), ttfbMs: quartet(histograms.ttfbMs), ttftMs: quartet(histograms.ttftMs) },
    planOutputTokens: sumOf(countedPlan, (r) => r.tokens.output),
    planRequests: sumOf(countedPlan, requestsIn),
    probeCostUsd: sumOf(probeRows, (r) => r.probeCostUsd),
    probes: { failed: probes.failed, held: probes.held, migrated: sumOf(probeRows, (r) => r.probeMigrated), ok: probes.ok, planOk: sumOf(probePlan, (r) => r.probes.ok), skipped: probes.skipped },
    rateLimited: outcomes.rate_limited,
    refusals,
    refused: Object.values(refusals).reduce((n, c) => n + c, 0),
    // A refused request files under the refusals map alone, so Rejected counts the requests the relay rejected.
    rejected: outcomes.rejected,
    requests,
    savingUsd: sumOf(counted, (r) => r.savingUsd),
    succeeded: outcomes.ok,
    tokens: sumEach(counted.map((r) => r.tokens), ZERO_TOKENS),
    uncacheable: sumOf(counted, (r) => r.uncacheable),
    uncached: sumOf(counted, (r) => r.uncached),
    wastedProbeReasons,
    wastedProbeUsd: sumOf(probeRows, (r) => r.wastedProbeUsd),
    wastedProbes: sumOf(probeRows, (r) => r.wastedProbes),
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

/**
 * The key dimension alone carries a generation in its group key. The reserved id reads `none` under the classifier
 * grouping, as the Requests facet names it, and `unattributed` under every other. The model grouping's empty key reads
 * `no model`, as the route labels it.
 */
const labelOf = (key: string, groupBy: AnalyticsGroupBy) => {
  if (key === '-') return groupBy === 'classifier' ? 'none' : 'unattributed';
  if (groupBy === 'model' && key === '') return 'no model';
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
  groupBy === 'user_agent' && named.length ? (r) => named.find((p) => matchesPattern(p, r.userAgent)) ?? 'other' : GROUP_KEY[groupBy];

/**
 * A group stays where probes count, where a row that is no probe joined it, or where its billed probes drew dollars,
 * as the route keeps one: a read that does not count probes drops a group of plan probes alone, and answers a group of
 * billed probes alone with its dollars and no request.
 */
function groupsOf(rows: HourlyRollup[], q: AnalyticsQuery): AnalyticsGroup[] {
  const of = groupKeyOf(q.groupBy, q.userAgentGroups ?? []);
  const buckets = groupInto(rows, of);
  const includeProbes = q.includeProbes ?? false;
  return [...buckets.entries()]
    .flatMap(([key, list]) => {
      const group = groupOf(key, labelOf(key, q.groupBy), list, includeProbes, q.groupBy, q.accountIds);
      return includeProbes || list.some((r) => r.kind !== 'probe') || group.billedUsd > 0 ? [{ ...group, ...holderOf(key, q.groupBy) }] : [];
    })
    .sort((a, b) => a.label.localeCompare(b.label) || (a.keyCreatedAt ?? 0) - (b.keyCreatedAt ?? 0) || a.key.localeCompare(b.key));
}

export function breakdownOf(rows: HourlyRollup[], q: AnalyticsQuery): AnalyticsGroup[] {
  const { fromMs, toMs } = wholeHoursOf(q);
  return groupsOf(
    rows.filter((r) => inQuery(r, q, Date.parse(r.hour), fromMs, toMs)),
    q,
  );
}

export function seriesOf(rows: HourlyRollup[], q: AnalyticsQuery): AnalyticsPoint[] {
  const { fromMs, toMs } = wholeHoursOf(q);
  const { slotOf, starts } = bucketsOf(q);
  // Each row is placed in its bucket in one pass, and a bucket keeps the rows' input order, so its sums add as a scan
  // of the whole list would. An index outside the starts names no bucket.
  const byBucket = starts.map((): HourlyRollup[] => []);
  for (const r of rows) {
    const hour = Date.parse(r.hour);
    if (inQuery(r, q, hour, fromMs, toMs)) byBucket[slotOf(hour)]?.push(r);
  }
  return starts.map((at, i) => ({ at: iso(at), groups: groupsOf(byBucket[i], q) }));
}

type Range = Pick<AnalyticsQuery, 'from' | 'to'>;

/**
 * The bucket starts the analytics routes answer a range in, each bucket's width, and the index of the bucket an
 * instant falls in. Buckets start on a whole multiple of their own width and run to the hour after the range ends, as
 * the route's do.
 */
function bucketsOf(range: Range): { bucketMs: number; slotOf: (ms: number) => number; starts: number[] } {
  const fromMs = Date.parse(range.from);
  const toMs = Date.parse(range.to);
  const bucketMs = bucketSizeFor(toMs - fromMs);
  const starts: number[] = [];
  for (let at = Math.floor(fromMs / bucketMs) * bucketMs; at < hourCeil(toMs); at += bucketMs) starts.push(at);
  return { bucketMs, slotOf: (ms) => Math.floor((ms - starts[0]) / bucketMs), starts };
}

/**
 * The whole hours a range touches, as the routes bound the request rows and the rollup hours they read: from the start
 * of the hour that holds the range's start up to the hour ceiling of its end, which the window leaves out. A six-hour
 * or daily bucket can start before the first of these hours and end past the last.
 */
export const wholeHoursOf = (range: Range): { fromMs: number; toMs: number } => ({ fromMs: startOfHour(Date.parse(range.from)), toMs: hourCeil(Date.parse(range.to)) });

const usageOf = (r: HourlyRollup): Usage => ({ cacheRead: r.tokens.cacheRead, cacheWrite1h: r.tokens.cacheWrite1h, cacheWrite5m: r.tokens.cacheWrite5m, input: r.tokens.uncachedInput, output: r.tokens.output });

/**
 * A plan row's Pro 5-hour figure, which a rollup row does not store: the share of the anchor tier's 5-hour window its
 * usage spends under the band at its hour, as the route bands a rollup row, on the rows of the regime its client label
 * names as the stored Pro-week figure is. A freed classifier part reads zero, and a model without a weight null.
 */
const fiveHourEquivalentOf = (r: HourlyRollup): number | null => {
  if (freedPart(r)) return 0;
  if (fiveHourRows.has(r)) return fiveHourRows.get(r) ?? null;
  const inside = bandAt(r.model, Date.parse(r.hour));
  const tables = tablesFor(r.client);
  const figure = inside === null ? null : shareIn(tables, 'five_hour', anchorTier(tables), r.model, usageOf(r), inside);
  fiveHourRows.set(r, figure);
  return figure;
};

/** The plan tiers a burn answer keys its figures by, as the route reads them: every tier of a cost revision's base
 *  capacity rows, so each point holds the same keys whatever revision priced it. */
const BURN_TIERS = [...new Set(TABLES.revisions.flatMap((r) => Object.keys(r.content.capacity)))];
const emptyTiers = (): Record<string, (number | null)[]> => Object.fromEntries(BURN_TIERS.map((tier) => [tier, []]));
const ZERO_TIERS: Record<string, number | null> = Object.fromEntries(BURN_TIERS.map((tier) => [tier, 0]));

/**
 * The cost model a row's tiers scale under, as the route prices a whole rollup row: the revision in force at the last
 * millisecond of the row's hour, under that revision's regime whose entrypoints name the row's client.
 */
const tierTablesOf = (r: HourlyRollup): SubscriptionCosts => tablesFor(r.client, tableAt(TABLES, Date.parse(r.hour) + HOUR - 1));
/**
 * A row's base figure, a share of the anchor tier's `window`, as each plan tier's share of its own window over
 * `divisor`, the row's units over the tier's capacity in place of the anchor's. Null on every tier where the base is
 * and on a tier the table sizes no `window` for.
 */
const byTier = (table: SubscriptionCosts, base: number | null, window: CostWindow, divisor = 1): Record<string, number | null> => {
  const anchor = tierCapacity(table, anchorTier(table), window);
  return Object.fromEntries(
    BURN_TIERS.map((tier) => {
      const capacity = tierCapacity(table, tier, window);
      return [tier, base === null || anchor === null || capacity === null ? null : (base * anchor) / capacity / divisor];
    }),
  );
};
const pushTiers = (lists: Record<string, (number | null)[]>, figures: Record<string, number | null>) => {
  for (const tier of BURN_TIERS) lists[tier].push(figures[tier]);
};
/** A row that spends no plan window adds a zero on every tier of each plan figure, a tier no revision sizes too. */
const addZeros = (s: BurnSum) => {
  for (const lists of [s.equivalentByTier, s.fableEquivalentByTier, s.fiveHourEquivalentByTier]) pushTiers(lists, ZERO_TIERS);
};
const bucketTiers = (lists: Record<string, (number | null)[]>): Record<string, number | null> => Object.fromEntries(BURN_TIERS.map((tier) => [tier, bucketShare(lists[tier])]));

/** A bucket's sums, each window's shares and each plan figure kept per row and per tier, so one row priced nothing
 *  nulls the window or the tier, as the route reads it. The model week's list holds the rows on a model with a week of
 *  its own alone, and the Fable figures' lists a zero for every row on another model. `unpriced` is the route's flag,
 *  set by a plan row with tokens whose base week figure is null, which reads the model's weight and no tier's size. */
interface BurnSum {
  cached: number;
  equivalentByTier: Record<string, (number | null)[]>;
  fableEquivalentByTier: Record<string, (number | null)[]>;
  fiveHour: (number | null)[];
  fiveHourEquivalentByTier: Record<string, (number | null)[]>;
  planUsd: number;
  sevenDay: (number | null)[];
  tokens: number;
  uncached: number;
  unpriced: boolean;
  usd: number;
  week: (number | null)[];
}
/**
 * A plan row's figures on each tier, as the route prices them. A free classifier call spends no plan window, so it adds
 * a billed row's zeros, and every other row its own units over each tier's capacity under its revision and regime. The
 * Fable week takes its share of the 7-day window off that revision, the whole window where it holds no Fable week, and
 * a row off the week adds a zero. A null base week figure sets the route's flag.
 */
const addPlanFigures = (s: BurnSum, r: HourlyRollup, week: string | undefined) => {
  if (freedPart(r)) {
    addZeros(s);
    return;
  }
  // The week reads the row's model alone, so the row's records hold for every read.
  let tiers = tierRows.get(r);
  if (tiers === undefined) {
    const table = tierTablesOf(r);
    const fable = pricedWindow(table, 'seven_day_fable');
    tiers = {
      equivalent: byTier(table, r.equivalent, 'seven_day'),
      fable: week !== 'seven_day_fable' ? ZERO_TIERS : byTier(table, fable === null ? null : r.equivalent, 'seven_day', fable?.divisor),
      fiveHour: byTier(table, fiveHourEquivalentOf(r), 'five_hour'),
    };
    tierRows.set(r, tiers);
  }
  pushTiers(s.equivalentByTier, tiers.equivalent);
  pushTiers(s.fableEquivalentByTier, tiers.fable);
  pushTiers(s.fiveHourEquivalentByTier, tiers.fiveHour);
  s.unpriced ||= r.equivalent === null;
};
const emptyBurn = (): BurnSum => ({
  cached: 0,
  equivalentByTier: emptyTiers(),
  fableEquivalentByTier: emptyTiers(),
  fiveHour: [],
  fiveHourEquivalentByTier: emptyTiers(),
  planUsd: 0,
  sevenDay: [],
  tokens: 0,
  uncached: 0,
  unpriced: false,
  usd: 0,
  week: [],
});

/** How many sessions the session grouping keeps, the route's cut. */
const BURN_SESSIONS = 20;

/**
 * A row's group under a burn grouping: the breakdown's key for every dimension it shares, so an earlier key holder
 * reads `<id>@<generation>` as the breakdown names it, a user agent pattern list's group under the user agent
 * dimension, and the row's session under the burn's own session grouping.
 */
const burnGroupOf = (groupBy: BurnGroup, named: string[]): ((r: HourlyRollup) => string) => (groupBy === 'session' ? (r) => r.sessionId : groupKeyOf(groupBy, named));

export function burnOf(rows: HourlyRollup[], range: Range, groupBy: BurnGroup, filters: Pick<AnalyticsQuery, 'accountIds' | 'keyIds' | 'poolIds' | 'userAgentGroups'> = {}): { bucketSeconds: number; points: BurnPoint[] } {
  const { bucketMs, slotOf, starts } = bucketsOf(range);
  const first = starts[0] ?? 0;
  const end = (starts[starts.length - 1] ?? 0) + bucketMs;
  const hours = wholeHoursOf(range);
  const groupOf = burnGroupOf(groupBy, filters.userAgentGroups ?? []);
  const sums = new Map<string, Map<number, BurnSum>>();
  // Per group, whether a row of the range's own hours holds a token and whether a row with a token was plan funded.
  const tokened = new Map<string, { inRange: boolean; plan: boolean }>();
  // Per group, the five counts of the range's own hours, which the route ranks the sessions by, so a session's tokens
  // in the first bucket's reach back take no part in its slot.
  const ranked = new Map<string, number>();
  for (const r of rows) {
    const hour = Date.parse(r.hour);
    // The route prices each row by the funding it recorded whatever the registry holds now, so a row whose account left
    // the registry stays.
    const kept = hour >= first && hour < end && passesMembers(r, filters);
    if (!kept) continue;
    const at = starts[slotOf(hour)];
    // A fast answer spends usage credits and an overage row the account's overage, each in dollars on either kind
    // and off every plan window: on a tier the cost model sizes they read as a zero share, so the account is still a
    // priced one.
    const billed = r.funding !== 'plan';
    const group = groupOf(r);
    const byAt = sums.get(group) ?? new Map();
    sums.set(group, byAt);
    const s = byAt.get(at) ?? emptyBurn();
    byAt.set(at, s);
    const tokens = tokensIn(r);
    s.cached += r.tokens.cacheRead;
    s.tokens += tokens;
    s.uncached += r.tokens.uncachedInput;
    if (hour >= hours.fromMs && hour < hours.toMs) ranked.set(group, (ranked.get(group) ?? 0) + tokens);
    // The route prices each row at the read, at the revision in force at the row, and splits the dollars by the row's
    // funding: a plan row's list dollars to the plan side and every other row's to the billed side. A free classifier
    // call prices at zero, so it adds nothing.
    if (!billed) s.planUsd += r.costUsd;
    else s.usd += r.costUsd;
    // A row with no token has nothing to price, as the route reads it: a failed request on a model the cost model has
    // no weight for never nulls a priced bucket.
    if (tokens === 0) continue;
    // A token is what the route prices, whatever the tables then answer, so a group whose model has no weight stays.
    // Only a token in the range's own hours answers a group, while one in the first bucket's reach back still names
    // its unit.
    const held = tokened.get(group) ?? { inRange: false, plan: false };
    held.inRange ||= hour >= hours.fromMs && hour < hours.toMs;
    held.plan ||= !billed;
    tokened.set(group, held);
    const week = modelWeeks(TABLES, r.model)[0];
    // The tier the row recorded, as the router reads it, so a plan row of an account that left the registry keeps its
    // shares.
    const tier = r.tier ?? undefined;
    if (billed) {
      // Every billed row adds a zero figure, so a bucket with tokens and no plan row reads 0 and only an idle one null.
      // The zero share rests on the tier alone, as the route reads a dollar-billed row, and an API key account has no
      // window to share.
      addZeros(s);
      const sized = (window: 'five_hour' | 'seven_day') => tier !== undefined && tierCapacity(TABLES, tier, window) !== null;
      if (sized('five_hour')) s.fiveHour.push(0);
      if (sized('seven_day')) s.sevenDay.push(0);
      // The week's own capacity, as the route reads it: the shared window the week prices on.
      const weekWindow = week === undefined ? null : pricedWindow(TABLES, week);
      if (weekWindow !== null && sized(weekWindow.priced)) s.week.push(0);
      continue;
    }
    // The row's own shares, its requests priced one by one at their completions, and its figure, which reads no
    // account tier.
    s.fiveHour.push(tier === undefined ? null : (r.shares.five_hour ?? null));
    s.sevenDay.push(tier === undefined ? null : (r.shares.seven_day ?? null));
    addPlanFigures(s, r, week);
    if (week !== undefined) s.week.push(tier === undefined ? null : (r.shares[week] ?? null));
  }
  let groups = [...sums.keys()].filter((group) => tokened.get(group)?.inRange === true).sort();
  if (groupBy === 'session') groups = [...groups].sort((a, b) => (ranked.get(b) ?? 0) - (ranked.get(a) ?? 0)).slice(0, BURN_SESSIONS).sort();
  // The route answers the shares and the unit where each group holds one account's rows: the account grouping, or a
  // filter naming exactly one account. A group of an account the registry no longer holds takes its unit from its own
  // rows: the window share where a row with a token was plan funded, dollars where each such row was billed.
  const points = groups.flatMap((group) => {
    const byAt = sums.get(group);
    const account = shareAccountOf(group, groupBy, filters.accountIds);
    const share = (list: (number | null)[]) => (account === null ? null : bucketShare(list));
    // The route answers no 5-hour or 7-day share where the account's window set lacks that window, a billed row's zero
    // included, and keeps the model week's share.
    const shared = account === null ? null : sharedWindowsOf(account);
    const lacks = (w: 'five_hour' | 'seven_day') => shared !== null && !shared.includes(w);
    // A usage-based seat is a paid source, so its burn reads in dollars as an API key's does.
    const unit = account === null ? null : (burnUnitOf(account) ?? (tokened.get(group)?.plan ? 'window_share' : 'usd'));
    return starts.map((at): BurnPoint => {
      const s = byAt?.get(at) ?? emptyBurn();
      return {
        at: iso(at),
        cachedTokens: Math.round(s.cached),
        equivalentByTier: bucketTiers(s.equivalentByTier),
        fableEquivalentByTier: bucketTiers(s.fableEquivalentByTier),
        fiveHourEquivalentByTier: bucketTiers(s.fiveHourEquivalentByTier),
        fiveHourShare: lacks('five_hour') ? null : share(s.fiveHour),
        group,
        modelWeekShare: share(s.week),
        planUsd: s.planUsd,
        sevenDayShare: lacks('seven_day') ? null : share(s.sevenDay),
        tokens: Math.round(s.tokens),
        uncachedTokens: Math.round(s.uncached),
        unit,
        unpriced: s.unpriced,
        usd: s.usd,
      };
    });
  });
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
 * request after each one wrote again, and beside them the moves an account's state forced among the ledger's `events`,
 * each under its own word and fit in its hour's bucket with the cache its request wrote. A cause filed under a turn
 * would tell a lapse the story holds nowhere, so no cause joins the turns. `moves` is each reason's sum over its fits,
 * as the route answers it. The held time per bucket folds the request rows as the ledger folds the completions, since
 * no account rollup the console reads carries it: the slow-down waits of the bucket's requests summed, and the
 * requests that waited one. The moves and the rewrite tokens keep the named accounts' rows, and the held time the named
 * accounts' and pools', since the account rollups carry no pool. All three read the whole hours the range touches, as
 * the route reads its rollup hours, and keep their points on the bucket grid.
 */
export function rewritesOf(rollups: AccountRollup[], requests: RequestLog[], events: readonly RoutingEvent[], range: Range, filters: Pick<AnalyticsQuery, 'accountIds' | 'poolIds'> = {}): RewritesAnswer {
  const { slotOf, starts } = bucketsOf(range);
  const hours = wholeHoursOf(range);
  const inHours = (ms: number) => ms >= hours.fromMs && ms < hours.toMs;
  const accounts = filters.accountIds ?? [];
  const pools = filters.poolIds ?? [];
  const ofAccounts = (accountId: string) => accounts.length === 0 || accounts.includes(accountId);
  // Placed in their buckets in one pass, as the series places its rows: the rollups that moved a pin in, each with its
  // parsed hour, and the requests that waited on a slow-down, each with its summed wait.
  const moved = starts.map((): { hour: number; r: AccountRollup }[] => []);
  for (const r of rollups) {
    const hour = Date.parse(r.hour);
    if (r.pins.migratedIn === 0 || !ofAccounts(r.accountId) || !inHours(hour)) continue;
    moved[slotOf(hour)]?.push({ hour, r });
  }
  const slowedMs = starts.map((): number[] => []);
  for (const r of requests) {
    const at = Date.parse(r.at);
    // The ledger files a request no attempt served under the unattributed id, as its rollups key the row.
    if (!inHours(at) || !ofAccounts(r.accountId ?? '-') || (pools.length > 0 && !pools.includes(r.poolId ?? '-'))) continue;
    const slowed = (r.held?.waits ?? []).filter((w) => w.reason === 'slow_down');
    if (slowed.length > 0) slowedMs[slotOf(at)]?.push(sumOf(slowed, (w) => w.heldMs));
  }
  const movesByReasonAndFit: Record<string, Record<string, number>> = {};
  const addMove = (reason: string, fit: string, n: number) => {
    const fits = (movesByReasonAndFit[reason] ??= {});
    fits[fit] = (fits[fit] ?? 0) + n;
  };
  const points = starts.map((at, i): RewritePoint => {
    let rewriteTokens = 0;
    for (const { hour, r } of moved[i]) {
      const turn = Math.floor(hour / HOUR);
      rewriteTokens += r.pins.migratedIn * (18_000 + (turn % 5) * 9_000);
      const reason = MOVE_TURNS[turn % MOVE_TURNS.length];
      const fit = FIT_TURNS[turn % FIT_TURNS.length];
      addMove(reason, fit, r.pins.migratedIn);
    }
    return { at: iso(at), rewriteTokens };
  });
  // The ledger files a move in its hour's row, so the range keeps it wherever it keeps that hour's synthetic moves. The
  // move's word stands in its serving attempt's trace, so the ledger books that attempt's cache write as rewrite tokens.
  for (const e of events) {
    const hour = startOfHour(Date.parse(e.at));
    if (forcedReasons(e) === null || !inHours(hour) || !ofAccounts(e.accountId ?? '-')) continue;
    const slot = slotOf(hour);
    addMove(typeof e.detail.reason === 'string' ? e.detail.reason : 'unknown', typeof e.detail.fit === 'string' ? e.detail.fit : 'unknown', 1);
    const served = requests.find((r) => r.keyId === e.keyId && r.sessionId === e.sessionId && r.canonicalModel === e.model && r.at === e.at);
    if (served) points[slot].rewriteTokens += served.usage.cacheWrite5m + served.usage.cacheWrite1h;
  }
  const moves = Object.fromEntries(Object.entries(movesByReasonAndFit).map(([reason, fits]) => [reason, Object.values(fits).reduce((n, x) => n + x, 0)]));
  const held = starts.map((at, i): HeldPoint => ({ at: iso(at), heldMs: sumOf(slowedMs[i], (x) => x), requests: slowedMs[i].length }));
  return { held, moves, movesByReasonAndFit, points };
}

/**
 * Whether a request row passes the account, key, pool, model and pattern filters as the request routes match a row:
 * the serving attempt's account, with a request no attempt served under the unattributed id as the request log files
 * it, the key by the selector rule, the pool with a request filed under none under `-`, the canonical model, and a
 * pattern as the rollups key a row, the client folded as the rollups fold a class (`foldRequestClass`) and the user
 * agent folded to its first product token, each `-` where the request named none.
 */
function passesRequestFilters(r: RequestLog, q: Pick<AnalyticsQuery, 'accountIds' | 'clients' | 'keyIds' | 'models' | 'poolIds' | 'userAgents'>): boolean {
  if (q.accountIds?.length && !q.accountIds.includes(r.accountId ?? '-')) return false;
  if (q.keyIds?.length && !q.keyIds.some((selector) => selectsKey(selector, { keyGeneration: r.keyGeneration, keyId: r.keyId ?? '-' }))) return false;
  if (q.poolIds?.length && !q.poolIds.includes(r.poolId ?? '-')) return false;
  if (q.models?.length && !q.models.some((m) => m.toLowerCase() === r.canonicalModel)) return false;
  if (q.userAgents?.length && !q.userAgents.some((p) => matchesPattern(p, agentTokenOf(r.userAgent)))) return false;
  if (q.clients?.length && !q.clients.some((p) => matchesPattern(p, foldRequestClass(r.client)))) return false;
  return true;
}

/**
 * Placements by rung off the request rows' traces, as the route counts them: each attempt that placed and carried
 * usage, a hit by the rule the request drawer reads its cache line by. The route reads the request rows of the whole
 * hours the range touches and narrows them by the accounts, the keys, the pools, the models and the client and user
 * agent patterns here. It leaves the probes out too, and no probe places in the story.
 */
export function placementsOf(requests: RequestLog[], q: Pick<AnalyticsQuery, 'accountIds' | 'clients' | 'from' | 'keyIds' | 'models' | 'poolIds' | 'to' | 'userAgents'>): PlacementRow[] {
  const { fromMs, toMs } = wholeHoursOf(q);
  const byRung = new Map<string, Omit<PlacementRow, 'hitShare'>>();
  for (const r of requests) {
    const at = Date.parse(r.at);
    if (at < fromMs || at >= toMs) continue;
    if (!passesRequestFilters(r, q)) continue;
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
 * The refused answers by model and category off the request rows, as the route counts them: each request in range
 * whose serving attempt the upstream answered with the stop reason `refusal`, `-` for a refusal with no category, the
 * largest count first and then the model and the category in byte order, where the dash sorts first, and beside them
 * the requests in range that presented a fallback digest. The route reads the request rows of the whole hours the range
 * touches, narrows them by the accounts, the keys, the pools, the models, the kinds, the probes switch and the client
 * and user agent patterns as `passesRequestFilters` reads them, and echoes the range as asked. The user agent groups
 * partition the rows and narrow nothing.
 */
export function refusalCategoriesOf(requests: RequestLog[], q: AnalyticsQuery): RefusalCategoriesAnswer {
  const fromMs = Date.parse(q.from);
  const toMs = Date.parse(q.to);
  const hours = wholeHoursOf(q);
  const counts = new Map<string, RefusalCategoryRow>();
  let fallbacks = 0;
  for (const r of requests) {
    const at = Date.parse(r.at);
    if (at < hours.fromMs || at >= hours.toMs) continue;
    if (!passesRequestFilters(r, q)) continue;
    if (q.kinds?.length && !q.kinds.includes(r.kind)) continue;
    if (r.kind === 'probe' && !q.includeProbes && !q.kinds?.includes('probe')) continue;
    if (r.fallbackDigest !== null) fallbacks += 1;
    const answer = servingResponse(r);
    if (answer?.stopReason !== 'refusal') continue;
    const category = answer.refusalCategory ?? '-';
    const key = `${r.canonicalModel}|${category}`;
    counts.set(key, { category, model: r.canonicalModel, requests: (counts.get(key)?.requests ?? 0) + 1 });
  }
  const rows = [...counts.values()].sort((a, b) => b.requests - a.requests || byteOrder(a.model, b.model) || byteOrder(a.category, b.category));
  return { fallbacks, from: iso(fromMs), rows, to: iso(toMs) };
}

/**
 * The days the ledger keeps a request row, as the mock answers it beside its windows: the mock settings'
 * `retention_days`, long enough that every window below finds its requests. Written out rather than read off
 * `SETTINGS_WIRE`, since its module imports this one through the requests module.
 */
export const REQUEST_RETENTION_DAYS = 365;

/** A window the hourly fold closed within the hour after its reset, its ratio the rise over the share as stored. */
const reconciled = (accountId: string, window: string, endMs: number, readings: [number, number], observedRise: number, predictedShare: number, requests: number): Reconciliation => ({
  accountId,
  computedAt: iso(endMs + 37 * MINUTE),
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
 * The story's own completed windows, an account's newest of each window ending where its current one began. Every
 * ratio sits near 1 but nightly's newest 5-hour window, which ended at the 13:11 reset of the window the poll read at
 * its limit at 12:01: its readings rose 0.3 against the 0.15 the router priced, twice the table.
 */
const STORY_ROWS: Reconciliation[] = [
  reconciled('nightly', 'five_hour', clock(13, 11), [clock(10, 52), clock(13, 10)], 0.3, 0.15, 64),
  reconciled('primary', 'five_hour', clock(11, 44), [clock(7, 3), clock(11, 38)], 0.52, 0.5, 212),
  reconciled('studio', 'five_hour', clock(10, 29), [clock(6, 9), clock(10, 26)], 0.38, 0.4, 37),
  reconciled('nightly', 'five_hour', NOW - 15 * HOUR - 50 * MINUTE, [NOW - 20 * HOUR - 20 * MINUTE, NOW - 16 * HOUR], 0.41, 0.43, 88),
  reconciled('studio', 'seven_day_fable', NOW - 2 * DAY - 14 * HOUR - 40 * MINUTE, [NOW - 9 * DAY - 8 * HOUR, NOW - 2 * DAY - 15 * HOUR], 0.21, 0.2, 19),
  reconciled('primary', 'seven_day', NOW - 3 * DAY - 18 * HOUR - 20 * MINUTE, [NOW - 10 * DAY - 16 * HOUR, NOW - 3 * DAY - 18 * HOUR - 32 * MINUTE], 0.64, 0.62, 2_350),
  reconciled('nightly', 'seven_day', NOW - 5 * DAY - 5 * HOUR - 48 * MINUTE, [NOW - 12 * DAY - 2 * HOUR, NOW - 5 * DAY - 6 * HOUR - 13 * MINUTE], 0.71, 0.69, 1_480),
];

/** One window kind's history before the story's rows: where its newest drawn window ends and the figures it draws. */
interface History {
  accountId: string;
  window: string;
  /** Where the newest drawn window ends: a period before the story's own row of the kind, else where the current window
   *  began, or where a history that stopped ends. */
  newestEnd: number;
  /** The range a drawn window's rise and its requests fall in, as the account's sessions burn the window. */
  rise: [number, number];
  requests: [number, number];
  /** The chance a 5-hour gap is an idle stretch, then the fewest and the most hours one runs. */
  idle?: [number, number, number];
  /** The drawn windows, by their place in the chain from the newest, that read off the band, at these ratios. */
  off?: Record<number, number>;
  /** The drawn 5-hour windows whose rise stays under the band's 5% floor, by their place. */
  quiet?: number[];
}

const FIVE_HOURS = 5 * HOUR;
const WEEK = 7 * DAY;
/** The span the route answers: a window that ended before it has left the answer. */
const SPAN_START = NOW - 30 * DAY;
/** The rise in whole percents, as the readings move, and the priced share at four decimals. */
const percent = (n: number) => round(n, 2);
const fourth = (n: number) => round(n, 4);

const HISTORIES: History[] = [
  { accountId: 'primary', window: 'five_hour', newestEnd: clock(11, 44) - FIVE_HOURS, rise: [0.28, 0.66], requests: [90, 240], idle: [0.25, 4, 11], off: { 13: 1.34 }, quiet: [21] },
  { accountId: 'secondary', window: 'five_hour', newestEnd: NOW - 4 * HOUR - 35 * MINUTE, rise: [0.3, 0.8], requests: [40, 160], idle: [0.3, 5, 14], quiet: [9, 26] },
  // Nightly's batch runs the window hard, and the meter charged it off the table twice more in the month.
  { accountId: 'nightly', window: 'five_hour', newestEnd: NOW - 15 * HOUR - 50 * MINUTE - FIVE_HOURS, rise: [0.35, 0.82], requests: [50, 190], idle: [0.2, 3, 9], off: { 3: 1.46, 17: 0.72 }, quiet: [6] },
  { accountId: 'studio', window: 'five_hour', newestEnd: clock(10, 29) - FIVE_HOURS, rise: [0.12, 0.45], requests: [5, 40], idle: [0.6, 10, 30], quiet: [2, 8] },
  // Legacy's credential lapsed two days back and its account served nothing for a week, so its history stopped there.
  { accountId: 'legacy', window: 'five_hour', newestEnd: NOW - 8 * DAY - HOUR, rise: [0.2, 0.55], requests: [20, 90], idle: [0.35, 6, 16] },
  { accountId: 'primary', window: 'seven_day', newestEnd: NOW - 3 * DAY - 18 * HOUR - 20 * MINUTE - WEEK, rise: [0.55, 0.8], requests: [1_900, 2_600] },
  { accountId: 'primary', window: 'seven_day_fable', newestEnd: NOW - 3 * DAY - 18 * HOUR - 20 * MINUTE, rise: [0.08, 0.2], requests: [120, 340] },
  { accountId: 'secondary', window: 'seven_day', newestEnd: NOW - DAY - 21 * HOUR - 55 * MINUTE, rise: [0.6, 0.9], requests: [700, 1_300], off: { 2: 1.31 } },
  { accountId: 'nightly', window: 'seven_day', newestEnd: NOW - 5 * DAY - 5 * HOUR - 48 * MINUTE - WEEK, rise: [0.6, 0.85], requests: [1_200, 1_700] },
  { accountId: 'nightly', window: 'seven_day_fable', newestEnd: NOW - 5 * DAY - 5 * HOUR - 48 * MINUTE, rise: [0.45, 0.7], requests: [300, 520] },
  { accountId: 'studio', window: 'seven_day', newestEnd: NOW - 2 * DAY - 14 * HOUR - 40 * MINUTE, rise: [0.25, 0.45], requests: [60, 140] },
  { accountId: 'studio', window: 'seven_day_fable', newestEnd: NOW - 2 * DAY - 14 * HOUR - 40 * MINUTE - WEEK, rise: [0.15, 0.3], requests: [14, 40] },
  { accountId: 'legacy', window: 'seven_day', newestEnd: NOW - 9 * DAY - 2 * HOUR, rise: [0.4, 0.7], requests: [500, 900] },
];

/**
 * Each history's windows from its newest back over the trailing 30 days, drawn from one seed so every load reads one
 * set: a 5-hour window starts at its predecessor's reset or after an idle stretch, so consecutive ends lie at least
 * five hours apart, and a week ends a week after the one before. A ratio sits near the table unless the history marks
 * the window off, and a quiet window's rise stays under the 5% floor the band judges from.
 */
function drawHistories(): Reconciliation[] {
  const rng = createRng(0x6d65_7465);
  return HISTORIES.flatMap((h) => {
    const weekly = h.window !== 'five_hour';
    const period = hoursOfKey(h.window) * HOUR;
    const rows: Reconciliation[] = [];
    for (let end = h.newestEnd, place = 0; end >= SPAN_START; place += 1) {
      const quiet = h.quiet?.includes(place) ?? false;
      const rise = quiet ? percent(rng.float(0.01, 0.04)) : percent(rng.float(h.rise[0], h.rise[1]));
      const ratio = h.off?.[place] ?? (quiet ? rng.float(0.5, 2.2) : rng.float(0.9, 1.12));
      const share = fourth(rise / ratio);
      const first = end - period + (weekly ? rng.int(1, 8) * HOUR : rng.int(2, 30) * MINUTE);
      const last = end - (weekly ? rng.int(10, 60) : rng.int(1, 8)) * MINUTE;
      const requests = quiet ? rng.int(1, 6) : rng.int(h.requests[0], h.requests[1]);
      rows.push(reconciled(h.accountId, h.window, end, [first, last], rise, share, requests));
      const idle = !weekly && h.idle !== undefined && rng.chance(h.idle[0]);
      end -= period + (weekly ? 0 : idle ? rng.int(h.idle![1], h.idle![2]) * HOUR : rng.int(0, 40) * MINUTE);
    }
    return rows;
  });
}

/**
 * The completed windows of the story's priced accounts over the trailing 30 days, newest first and then by account and
 * window as the route orders them: the story's own rows the newest of their windows, and before them each account's
 * history, every ratio the rise over the share as stored. Legacy's history stops before its idle week, and reserve,
 * spare, the seat, sunset and the API keys priced nothing the ledger could reconcile.
 */
export const RECONCILIATIONS: Reconciliation[] = [...STORY_ROWS, ...drawHistories()].sort(
  (x, y) => Date.parse(y.windowEnd) - Date.parse(x.windowEnd) || byteOrder(x.accountId, y.accountId) || byteOrder(x.window, y.window),
);

/** The reconciled windows as the route answers them, narrowed to the Accounts filter, every account's without one. */
export function reconciliationsOf(rows: Reconciliation[], filters: Pick<AnalyticsQuery, 'accountIds'> = {}): ReconciliationsAnswer {
  const accounts = filters.accountIds ?? [];
  return { reconciliations: rows.filter((r) => accounts.length === 0 || accounts.includes(r.accountId)).map((r) => ({ ...r })), requestRetentionDays: REQUEST_RETENTION_DAYS };
}

/**
 * The term's end the route reads at `at`: the saved end while canceled, else a trial end still ahead, and the
 * plan-ended flag where it comes first, so a trial end that passed while the plan carried on ends no week.
 */
export function termEndOf(a: Account, at: number): number | null {
  const flagged = a.planEndedAt === null ? null : Date.parse(a.planEndedAt);
  return earliest(endOf(a, at), flagged);
}

/** How long before a drop a used claim of its account still joins it: the router dates the drop at the first poll
 *  after the use. */
const CLAIM_JOIN_MS = HOUR;

/**
 * Whether a `window_dropped` row cleared its window, as the ledger joins it: the newest `used` claim of the account in
 * the hour before the drop decides through its `cleared` list where it holds a parsed answer, and with no such claim,
 * or one with no parsed answer, the drop cleared where the poll after it reads under the margin.
 */
function dropCleared(drop: EventSeed, window: string, utilization: number, events: EventSeed[]): boolean {
  const at = Date.parse(drop.at);
  const claim = events
    .filter((e) => e.kind === 'limit_reset_claimed' && e.accountId === drop.accountId && e.detail.outcome === 'used' && Date.parse(e.at) <= at && at - Date.parse(e.at) <= CLAIM_JOIN_MS)
    .reduce<EventSeed | null>((newest, e) => (newest === null || Date.parse(e.at) > Date.parse(newest.at) ? e : newest), null);
  if (claim !== null && claim.detail.result !== null) return (claim.detail as ClaimDetail).cleared.includes(window);
  return wholePoints(utilization) < wholePoints(WINDOW_DROP_MARGIN);
}

/**
 * The weekly resets in range per subscription account as the route answers them, from the story's `window_reset` rows
 * with the pair each event carries, a `claim` row for each `window_dropped` row in range that cleared its window, at
 * the drop's instant with the share left before it and the week's unchanged reset, and, for an account whose term
 * ended in range, one `term_ended` reset per weekly window at the end, its share the last reading of the window's quota
 * history strictly before the end, as the ledger reads `observed_at < end`. Its pair is the one the router's stored
 * reading of the window carries, for which the window's newest sample of the history stands in: the pair the story
 * stores for a reading at that sample's instant, and null for both where the history holds no sample. A reset at or
 * after an end the read has reached is left out, an end before the range's start included, as the ledger drops the
 * resets at or after every end it lists, so the week the end cut short counts once, and so is a drop at or after it.
 * The `term_ended` rows build for an end inside the range alone. The term is read at the range's end capped at the
 * story's now, the windows run in key order, the 7-day window first, and each window answers two lists as the decoder
 * parts the route's rows: `claims` by the drop's instant and `resets` by the reset's, each newest first, with two rows
 * of one instant in the route's order.
 */
export function unusedAllowanceOf(events: EventSeed[], accounts: Account[], history: QuotaHistory[], span: Range, filters: Pick<AnalyticsQuery, 'accountIds'> = {}): UnusedAllowanceAnswer {
  const fromMs = Date.parse(span.from);
  const toMs = Date.parse(span.to);
  const listed = filters.accountIds ?? [];
  const readAt = Math.min(toMs, NOW);
  const leftOf = (utilization: number) => Math.min(1, Math.max(0, 1 - utilization));
  // Grouped once, so each account and each of its drops reads its own rows rather than the whole log.
  const own = groupInto(events, (e) => (e.kind === 'window_reset' || e.kind === 'window_dropped' ? e.accountId : null));
  const claims = groupInto(events, (e) => (e.kind === 'limit_reset_claimed' && e.detail.outcome === 'used' ? e.accountId : null));
  const answered = accounts
    .filter((a) => a.kind === 'oauth' && (listed.length === 0 || listed.includes(a.id)))
    .sort((a, b) => byteOrder(a.id, b.id))
    .flatMap((a) => {
      const end = termEndOf(a, readAt);
      const elapsed = end !== null && end <= readAt ? end : null;
      const ended = elapsed !== null && fromMs <= elapsed ? elapsed : null;
      const byWindow = new Map<string, AllowanceReset[]>();
      const add = (window: string, reset: AllowanceReset) => byWindow.set(window, [...(byWindow.get(window) ?? []), reset]);
      for (const e of own.get(a.id) ?? []) {
        const at = Date.parse(e.at);
        if (at < fromMs || at > toMs) continue;
        if (e.kind === 'window_reset') {
          const d = e.detail as { observed: string; plan: string | null; reset_at: number; tier: string | null; utilization_before: number; window: string };
          if (hoursOfKey(d.window) !== 168 || (elapsed !== null && d.reset_at >= elapsed)) continue;
          add(d.window, { observedAt: e.at, plan: d.plan, resetAt: iso(d.reset_at), shareLeft: leftOf(d.utilization_before), source: d.observed, tier: d.tier });
        } else if (e.kind === 'window_dropped') {
          const d = e.detail as DropDetail;
          if (hoursOfKey(d.window) !== 168 || (elapsed !== null && at >= elapsed) || !dropCleared(e, d.window, d.utilization, claims.get(a.id) ?? [])) continue;
          add(d.window, { observedAt: e.at, plan: d.plan, resetAt: iso(d.reset), shareLeft: leftOf(d.utilization_before), source: 'claim', tier: d.tier });
        }
      }
      if (ended !== null) {
        for (const h of history) {
          const window = WINDOW_KEY[h.windowId] ?? h.windowId;
          if (h.accountId !== a.id || hoursOfKey(window) !== 168) continue;
          // The ledger reads the newest sample with a figure, past one that reported the reset alone.
          const before = h.samples.filter((x): x is { at: string; utilization: number } => x.utilization !== null && Date.parse(x.at) < ended);
          const last = before[before.length - 1];
          const stored = h.samples.length === 0 ? null : Date.parse(h.samples[h.samples.length - 1].at);
          const { plan, tier } = stored === null ? { plan: null, tier: null } : readingPairAt(a.id, stored);
          if (last) add(window, { observedAt: last.at, plan, resetAt: iso(ended), shareLeft: leftOf(last.utilization), source: 'term_ended', tier });
        }
      }
      const windows = [...byWindow]
        .sort(([x], [y]) => byteOrder(x, y))
        .map(([window, listedResets]) => {
          const ordered = [...listedResets].sort((x, y) => Date.parse(y.resetAt) - Date.parse(x.resetAt) || Date.parse(y.observedAt) - Date.parse(x.observedAt));
          const newest = (key: 'observedAt' | 'resetAt') => (x: AllowanceReset, y: AllowanceReset) => Date.parse(y[key]) - Date.parse(x[key]);
          const claims = ordered.filter((r) => r.source === 'claim').sort(newest('observedAt'));
          const resets = ordered.filter((r) => r.source !== 'claim').sort(newest('resetAt'));
          return { claims, resets, totalShareLeft: ordered.reduce((sum, r) => sum + r.shareLeft, 0), window };
        });
      return windows.length === 0 ? [] : [{ accountId: a.id, ...subscriptionPair(a.subscription), windows }];
    });
  return { accounts: answered, from: iso(fromMs), to: iso(toMs) };
}
