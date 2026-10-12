import type {
  AccountKind, AccountRollup, Attempt, AttemptOutcome, AttemptWindow, AutoMode, CacheMissReason, CapturedBodies, CohortHold, CostShares, CostSplit, DiagnosticsSource, LatencyHistograms, PinMigrationReason, Placement,
  QuotaSample, RateLimitWindow, RequestEvent, RequestKind, RequestLog, RequestOutcome, SelectionTrace, TrafficBucket, Usage, WastedProbeReason, WindowId, WindowSample,
} from '@/api/types';
import { emptyCacheMissReasons, emptyWastedProbeReasons, missCount } from '@/lib/cache';
import { AGENT_TYPES } from '@/lib/client';
import { fundingOf, modelWeeks, shareOf } from '@/lib/costs';
import { NO_CLASS, NO_EFFORT, foldEffort, foldRequestClass } from '@/lib/dimensions';
import { tokensTotal } from '@/lib/format';
import { emptyHistogram, histogramOf } from '@/lib/histogram';
import { aheadOf, endOf } from '@/lib/quota';
import { ACCOUNTS, QUOTA_HISTORY, accountById } from './accounts';
import type { HourlyRollup } from './analytics';
import { cellsFor } from './catalog';
import { keyById, poolById } from './pools';
import { createRng, type Rng } from './rng';
import { ACCOUNT_ROLLUP_SAMPLE, REQUEST_SAMPLE, TABLES } from './samples';
import { DAY, DELETED_HOLDER_END, HOUR, MINUTE, NOW, daysAgo, iso, startOfHour } from './time';

const rng = createRng(7_331);
// The upstream's own ids draw from a stream of their own, so they move no draw of the rows.
const upstreamIds = createRng(9_191);
const upstreamId = (prefix: string) => `${prefix}${upstreamIds.uuid().replace(/-/g, '').slice(0, 22)}`;
// The instants inside a request (the write, the first content, the stream's close) draw from a stream of their own,
// so they move no draw of the rows and no hint.
const phaseRng = createRng(3_571);
/** The stream's close lands this many milliseconds before the finish at most, so the first content never passes it. */
const STREAM_TAIL = 30;
/** The registry's kind of an account the story names. A wrong id fails the mock build rather than reading a default. */
const kindOf = (accountId: string): AccountKind => {
  const kind = ACCOUNTS.find((x) => x.id === accountId)?.kind;
  if (kind === undefined) throw new Error(`the story names an account the registry lacks: ${accountId}`);
  return kind;
};

export const SESSION_IDS = Array.from({ length: 14 }, () => rng.uuid());

/** An earlier holder of `desktop`, created long before the live one. */
export const DELETED_HOLDER_GENERATION = Date.parse(daysAgo(120));
const AGENT_IDS = ['main', 'main', 'main', 'a7f3', 'c21e', 'e90b', '4d1a'];

/** One client per key, the way a machine runs one build of Claude Code and a runner runs its own script. */
export const USER_AGENTS: Record<string, string> = {
  ci: 'phoebis-batch/0.9 (external, ci)',
  desktop: 'claude-cli/2.1.269 (external, cli)',
  laptop: 'claude-cli/2.0.44 (external, cli)',
};

/** The class the router derives from each of those agents. */
export const CLIENTS: Record<string, string> = {
  ci: 'other',
  desktop: 'claude-code/cli',
  laptop: 'claude-code/cli',
};

export interface Lane {
  accountIds: string[];
  keyId: string;
  models: { value: string; weight: number }[];
  poolId: string;
}

export const LANES: Lane[] = [
  {
    keyId: 'desktop',
    poolId: 'all',
    accountIds: ['primary', 'nightly', 'secondary', 'overflow'],
    models: [
      { value: 'claude-fable-5-1', weight: 8 },
      { value: 'claude-sonnet-5-5', weight: 5 },
      { value: 'claude-opus-5-5', weight: 1.5 },
      { value: 'claude-haiku-4-5-20251001', weight: 2 },
    ],
  },
  {
    keyId: 'laptop',
    poolId: 'all',
    accountIds: ['primary', 'nightly', 'secondary'],
    models: [
      { value: 'claude-sonnet-5-5', weight: 6 },
      { value: 'claude-haiku-4-5-20251001', weight: 3 },
    ],
  },
  {
    keyId: 'ci',
    poolId: 'jobs',
    accountIds: ['research'],
    models: [
      { value: 'claude-haiku-4-5-20251001', weight: 6 },
      { value: 'claude-sonnet-5', weight: 1 },
    ],
  },
];

/**
 * The account that serves the story's unpriced traffic: Sonnet 4.6 alone, which the cost model has no weight for,
 * whatever the lane drew, so its buckets read no share. The sessions the story moves off it carry Sonnet 4.6 to primary,
 * whose 11:00 and 14:00 buckets read no share either. Every other subscription bucket stays priced.
 */
const UNPRICED_ACCOUNT = 'secondary';
const modelOn = (accountId: string, drawn: string) => (accountId === UNPRICED_ACCOUNT ? 'claude-sonnet-4-6' : drawn);
/** The model a request asked for: Sonnet 4.6 wherever secondary served one of its attempts, else the lane's draw. */
const modelOf = (attempts: Attempt[], drawn: string) => (attempts.some((a) => a.accountId === UNPRICED_ACCOUNT && a.cooldownSet?.reason !== 'model_rejected') ? 'claude-sonnet-4-6' : drawn);

/**
 * Probes exist only where the pool keeps caches warm and the key has not opted out, and so does anything kept warm by
 * one.
 */
export function warmsCaches(keyId: string, poolId: string): boolean {
  return poolById.get(poolId)?.keepalive.enabled === true && keyById.get(keyId)?.keepaliveOptOut !== true;
}

const TRAFFIC_KINDS: { value: RequestKind; weight: number }[] = [
  { value: 'inference', weight: 12 },
  { value: 'count_tokens', weight: 2 },
];
const WARMED_KINDS: { value: RequestKind; weight: number }[] = [...TRAFFIC_KINDS, { value: 'probe', weight: 3 }];

/** How far an earlier window's reading runs ahead of its pace at most: the story's one shedding round is at 11:00. */
const EARLIER_LEAD = 0.8;

/**
 * Walks a window back so a request seen hours ago reports the window that was
 * live at that moment. `olderFactor` is a thunk because it may draw from the
 * seeded stream, and only a window older than the current one needs it. An
 * earlier window's reading stays within its lead of the share elapsed, so no
 * account but the story's leads its pace by the pool's shedding margin.
 */
const walkBack = (w: RateLimitWindow, atMs: number, olderFactor: () => number): AttemptWindow => {
  const period = w.periodHours * HOUR;
  const resetsAt = Date.parse(w.resetsAt ?? '');
  const shift = Math.ceil(Math.max(0, resetsAt - period - atMs) / period);
  const resetThen = resetsAt - shift * period;
  const phase = 1 - (resetThen - atMs) / period;
  const ceiling = shift === 0 ? 1 : Math.min(1, phase + EARLIER_LEAD);
  const utilization = Math.min(ceiling, Math.max(0.02, w.utilization * (shift === 0 ? phase / Math.max(0.05, 1 - (resetsAt - NOW) / period) : olderFactor())));
  // A walked-back reading never reaches the limit by rounding or by its factor: only a story point says a window is
  // spent. The nudge keeps a product that lands on a whole hundredth, 0.29 read as 28.999..., from losing one.
  return { id: w.id, resetsAt: iso(resetThen), utilization: Math.min(Math.floor(utilization * 100 + 1e-9) / 100, 0.99) };
};

export const oauthWindows = (accountId: string, atMs: number, olderFactor: () => number = () => rng.float(0.4, 1.1)): AttemptWindow[] => {
  const account = ACCOUNTS.find((a) => a.id === accountId);
  if (!account || account.kind !== 'oauth') return [];
  // A head reports the two shared windows and the overage week; a week only the poll reads never reaches an attempt.
  const reported = account.windows.filter((w) => w.id === '5h' || w.id === '7d').map((w) => storyReading(accountId, w, walkBack(w, atMs, olderFactor), atMs));
  // Plans that serve Fable send its weekly headers alongside the other two; the
  // rest send nothing and the detail panel reads n/a. A fixed factor rather than
  // a draw keeps the seeded stream the other fixtures read from where it was.
  if (!account.fableWindow) return reported;
  return [...reported, storyReading(accountId, account.fableWindow, walkBack(account.fableWindow, atMs, () => 0.8), atMs)];
};

/** Neither a probe nor a token count thinks or reaches a server tool, so both carry the counts at zero. */
const NO_ANSWER_FACTS = { thinking: 0, webFetch: 0, webSearch: 0 };

function usageFor(model: string, kind: RequestKind): Usage {
  if (kind === 'probe') return { input: 0, cacheRead: rng.chance(0.9) ? rng.int(30_000, 90_000) : 0, cacheWrite5m: 0, cacheWrite1h: 0, output: 1, ...NO_ANSWER_FACTS };
  if (kind === 'count_tokens') return { input: rng.int(2_000, 60_000), cacheRead: 0, cacheWrite5m: 0, cacheWrite1h: 0, output: 0, ...NO_ANSWER_FACTS };
  const big = model.includes('fable') || model.includes('opus');
  const cached = rng.chance(0.72);
  const contextTokens = rng.int(big ? 40_000 : 12_000, big ? 180_000 : 90_000);
  const cacheRead = cached ? Math.round(contextTokens * rng.float(0.8, 0.98)) : 0;
  const fresh = contextTokens - cacheRead;
  const writes1h = rng.chance(0.35);
  // One draw either way, since a request writes a 5-minute entry or a 1-hour one and never both.
  const written = Math.round(fresh * rng.float(0.6, 0.95));
  const output = rng.int(big ? 400 : 120, big ? 6_000 : 2_400);
  return {
    input: Math.round(fresh * rng.float(0.05, 0.35)),
    cacheRead,
    cacheWrite5m: writes1h ? 0 : written,
    cacheWrite1h: writes1h ? written : 0,
    output,
    // Only the larger models are asked to think in this story, and a thinking answer spends much of its output there.
    thinking: big ? Math.round(output * rng.float(0.2, 0.7)) : 0,
    webFetch: rng.chance(0.08) ? 1 : 0,
    webSearch: rng.chance(0.15) ? rng.int(1, 4) : 0,
  };
}

/** An answer prices at the cells the ledger selects for it: the model's fast cells for a fast answer on a row that
 *  has them, else the long-context cells when its prompt is over the threshold, else the standard cells. */
export function costSplitOf(model: string, u: Usage, fast = false): CostSplit {
  const p = cellsFor(model, u, fast);
  return {
    cacheRead: (u.cacheRead * p.cacheReadPerM) / 1_000_000,
    cacheWrite: (u.cacheWrite5m * p.cacheWrite5mPerM + u.cacheWrite1h * p.cacheWrite1hPerM) / 1_000_000,
    input: (u.input * p.inputPerM) / 1_000_000,
    output: (u.output * p.outputPerM) / 1_000_000,
  };
}

export function costOf(model: string, u: Usage, fast = false): number {
  const c = costSplitOf(model, u, fast);
  return c.input + c.cacheRead + c.cacheWrite + c.output;
}

const sumSplit = (parts: CostSplit[]): CostSplit => parts.reduce((s, c) => ({ cacheRead: s.cacheRead + c.cacheRead, cacheWrite: s.cacheWrite + c.cacheWrite, input: s.input + c.input, output: s.output + c.output }), { cacheRead: 0, cacheWrite: 0, input: 0, output: 0 });

// Its own stream, so these draws leave the fixtures the main stream feeds alone.
const missRng = createRng(1_517);

/**
 * What the cache diagnostics beta reported for each attempt of one request.
 * Diagnostics ride on a response body, so an attempt that failed upstream
 * carries none, and the token-count endpoint returns no usage block to compare
 * against.
 */
function diagnosed(attempts: Attempt[], kind: RequestKind): Attempt[] {
  return attempts.map((a) => {
    if (a.upstreamStatus !== 200 || kind === 'count_tokens') return a;
    // An API-key account in this fixture answers without the beta, so the client never sees a comparison.
    if (ACCOUNTS.find((x) => x.id === a.accountId)?.kind !== 'oauth') return { ...a, cacheMissReason: 'not_requested' as const };
    // A failover lands on an account that never saw the turn before this one.
    if (a.index > 0) return { ...a, cacheMissReason: 'previous_message_not_found' as const };
    const cacheMissReason = missRng.weighted<CacheMissReason>([
      { value: 'none', weight: 72 },
      { value: 'messages_changed', weight: 7 },
      { value: 'system_changed', weight: 4 },
      { value: 'tools_changed', weight: 3 },
      { value: 'model_changed', weight: 2 },
      { value: 'unavailable', weight: 2 },
    ]);
    return { ...a, cacheMissReason };
  });
}

/**
 * Who put the diagnostics member on a row's serving body: nobody where the beta compared nothing, the router for the
 * laptop's reduced-shape client, which the relay inserts the member for, and the client itself on the current build.
 */
function sourceOf(attempts: Attempt[], keyId: string): DiagnosticsSource | null {
  const reason = attempts[attempts.length - 1]?.cacheMissReason ?? null;
  if (reason === null || reason === 'not_requested') return null;
  return keyId === 'laptop' ? 'router' : 'client';
}

/**
 * The story's wall-clock time on its pinned day, 2026-09-16 at 14:30 UTC, as an offset from the instant the mock loads
 * at, so every story fact holds at whichever clock the mock is loaded at.
 */
export const clock = (h: number, m: number, s = 0): number => NOW - ((14 - h) * HOUR + (30 - m) * MINUTE - s * 1_000);

/**
 * The story's own readings of a window where the walk back's even climb would not tell it, per account and window:
 * secondary's 5-hour window runs hot from its opening at 10:42, so it leads its pace by the pool's 0.9 margin through
 * the 11:00 shedding round and the shed move after it, and reads spent from the head of 14:02:11.
 */
const STORY_READINGS: Record<string, Partial<Record<WindowId, { at: number; utilization: number }[]>>> = {
  secondary: {
    '5h': [
      { at: clock(10, 42), utilization: 0 },
      { at: clock(11, 0), utilization: 0.97 },
      { at: clock(11, 8, 30), utilization: 0.99 },
      { at: clock(14, 2, 10), utilization: 0.99 },
      { at: clock(14, 2, 11), utilization: 1 },
    ],
  },
};

/** A reading inside the live window the story's readings cover, read off them. Any other reads as walked back. */
function storyReading(accountId: string, w: RateLimitWindow, read: AttemptWindow, atMs: number): AttemptWindow {
  const points = STORY_READINGS[accountId]?.[w.id];
  if (!points || read.resetsAt !== w.resetsAt || atMs < points[0].at) return read;
  const after = points.findIndex((p) => p.at > atMs);
  if (after < 0) return { ...read, utilization: points[points.length - 1].utilization };
  const [from, to] = [points[after - 1], points[after]];
  return { ...read, utilization: Math.round((from.utilization + ((to.utilization - from.utilization) * (atMs - from.at)) / (to.at - from.at)) * 100) / 100 };
}

/** The routing log's story hour, written by hand: no drawn request falls inside it. */
export const STORY_HOUR_FROM = NOW - HOUR;

export const HAIKU = 'claude-haiku-4-5-20251001';

/**
 * Whether an account could take a request at `atMs` by the story's own account facts, so no attempt lands where the
 * decision card would show the account out: secondary, which serves Sonnet 4.6 alone, reads its 5-hour window spent
 * from 14:02:11 and cools on it from 14:09:31 to 15:42. Primary's fast lane cools from 14:27:00 to 14:42 and it
 * rejects Haiku from 14:24:10 to 15:24. Nightly's usage credits take a fast request once its spend cap is raised at
 * 11:30:12. The Overflow workspace needs funding from 11:30:12, and research spends its day's budget at 12:41:07.
 */
export function servable(accountId: string, atMs: number, model: string, fast: boolean): boolean {
  const within = (from: number, to: number) => atMs >= from && atMs < to;
  if (accountId === 'secondary') return !within(clock(14, 2, 11), clock(15, 42));
  if (accountId === 'primary') return !(fast && within(clock(14, 27), clock(14, 42))) && !(model === HAIKU && within(clock(14, 24, 10), clock(15, 24)));
  if (accountId === 'nightly') return !fast || atMs >= clock(11, 30, 12);
  if (accountId === 'overflow') return atMs < clock(11, 30, 12);
  if (accountId === 'research') return atMs < clock(12, 41, 7);
  return true;
}

/** An account a request is handed to, or a story attempt's, walks its windows back at a fixed factor, not a draw. */
const SERVED_FACTOR = 0.8;

/**
 * The order the engine takes a token count's members in: the best tier first, then under Pace the highest score off
 * the walk back. A count takes no slot, so no cap passes a member over for it and a draw cannot stand where the
 * engine picks another. Nightly reads in overage from its spend cap's raise, the earliest its overage starts. An API
 * key shares its tier with no other member here, so it keeps the pool's order, and outside Pace the drawn account
 * leads its tier.
 */
function countOrder(ids: string[], poolId: string, atMs: number, model: string, drawn: string): string[] {
  const pool = poolById.get(poolId);
  if (!pool) return ids;
  const tierOf = (id: string): number[] => {
    const overage = id === 'nightly' && atMs >= clock(11, 30, 12) ? 1 : 0;
    return [pool.preferSubscription ? (kindOf(id) === 'api_key' ? 2 : overage) : 0, pool.members.find((m) => m.accountId === id)?.priority ?? 0, overage];
  };
  const windowId = model.includes('fable') ? '7d_fable' : '7d';
  const scoreOf = (id: string): number => {
    if (pool.strategy !== 'pace') return id === drawn ? 1 : 0;
    if (kindOf(id) === 'api_key') return 0;
    const reading = oauthWindows(id, atMs, () => SERVED_FACTOR).find((w) => w.id === windowId && w.utilization !== null);
    const deadlines = [reading?.resetsAt, accountById.get(id)?.subscription?.endsAt].flatMap((x) => (x ? [Date.parse(x)] : []));
    const hours = deadlines.length ? Math.max(1, (Math.min(...deadlines) - atMs) / HOUR) : 168;
    return Math.max(0, 1 - (reading?.utilization ?? 0)) / hours;
  };
  const ranked = ids.map((id) => ({ id, score: scoreOf(id), tier: tierOf(id) }));
  ranked.sort((x, y) => {
    const i = x.tier.findIndex((n, j) => n !== y.tier[j]);
    return i >= 0 ? x.tier[i] - y.tier[i] : y.score - x.score;
  });
  return ranked.map((x) => x.id);
}

/** The story's one shedding round: secondary's 5-hour window leads its pace by the pool's margin at 11:00. */
export const SHED_ROUND = { accountId: UNPRICED_ACCOUNT, at: clock(11, 0) };

/** The plan windows a head reads spent, the later reset first, as the relay cools the account until it. */
const spentWindows = (windows: AttemptWindow[]) =>
  windows.filter((w) => (w.id === '5h' || w.id === '7d') && (w.utilization ?? 0) >= 1).sort((x, y) => Date.parse(y.resetsAt ?? '') - Date.parse(x.resetsAt ?? ''));

/**
 * The cooldown a 429 sets by the relay's rule: a head that reads a plan window spent cools the account until that
 * window's reset, and any other 429 until its retry-after.
 */
const rateLimitCooldown = (windows: AttemptWindow[], answeredAt: number, retryAfterS: number): Attempt['cooldownSet'] => {
  const spent = spentWindows(windows)[0];
  return spent?.resetsAt ? { reason: `429 on ${spent.id} window`, until: spent.resetsAt } : { reason: `429, retry after ${retryAfterS} s`, until: iso(answeredAt + retryAfterS * 1_000) };
};

/** Where the attempts a 429 added after a request's draws begin, by request id. */
const CHAIN_FROM = new Map<string, number>();

/**
 * The attempts of one request: none when no account of the lane could serve it at its time (the router answers it
 * exhausted without an upstream call), and fewer than drawn when the lane ran out of accounts it had not tried or an
 * answer ended it. `whole` says the request answered as drawn: every drawn attempt made, and none added after a 429.
 * `drawnHead` is where the draws end when secondary is barred from a fast request's earlier attempts alone and every
 * drawn attempt runs. The usage draw is sized by it, so every row keeps the draws the stream gives it.
 */
function buildAttempts(lane: Lane, atMs: number, finalStatus: number, attemptsWanted: number, model: string, fast: boolean, count: boolean): { attempts: Attempt[]; chainFrom: number; drawnHead: string | undefined; whole: boolean } {
  const attempts: Attempt[] = [];
  const trail: string[] = [];
  const drawnTrail: string[] = [];
  const pool = [...lane.accountIds];
  let cursor = atMs;
  let ended = false;
  let drawnEnded = false;
  let committed = false;
  for (let i = 0; i < attemptsWanted; i += 1) {
    const last = i === attemptsWanted - 1;
    const drawnId = pool.length > 1 && i === 0 && lane.poolId === 'all' && rng.chance(0.3) ? 'secondary' : rng.pick(pool);
    const idx = pool.indexOf(drawnId);
    if (idx >= 0 && pool.length > 1) pool.splice(idx, 1);
    // The stream draws as it always has, so a request handed off an account keeps every later draw where it was: an
    // account it tried already, or one that could not serve then, hands the attempt to the lane's first account that
    // could and was not tried, and with none left the request ends at the attempt before. A token count goes where the
    // engine's order puts it among the accounts that could take it.
    const upstreamStatus = last ? finalStatus : rng.weighted([{ value: 429, weight: 6 }, { value: 529, weight: 2 }, { value: 502, weight: 1 }]);
    const outcome: AttemptOutcome = upstreamStatus === 200 ? 'succeeded' : upstreamStatus === 429 ? 'rate_limited' : 'upstream_error';
    // Secondary serves Sonnet 4.6 alone, so it takes a request only as its last attempt, where the request is the
    // Sonnet 4.6 turn it serves rather than the model the lane drew.
    const untried = (id: string) => !trail.includes(id) && servable(id, cursor, model, fast) && !(!last && id === UNPRICED_ACCOUNT);
    const handedTo = count ? countOrder(lane.accountIds.filter(untried), lane.poolId, cursor, model, drawnId)[0] : untried(drawnId) ? drawnId : lane.accountIds.find((id) => id !== drawnId && untried(id));
    if (handedTo === undefined) ended = true;
    // The trail `drawnHead` reads, on the hand-off the usage draw is sized by.
    const drawnUntried = (id: string) => !drawnTrail.includes(id) && servable(id, cursor, model, fast) && !(fast && !last && id === UNPRICED_ACCOUNT);
    const drawnTo = drawnUntried(drawnId) ? drawnId : lane.accountIds.find((id) => id !== drawnId && drawnUntried(id));
    if (drawnTo === undefined) drawnEnded = true;
    if (!drawnEnded) drawnTrail.push(drawnTo ?? drawnId);
    const accountId = handedTo ?? drawnId;
    const durationMs = last && finalStatus === 200 ? rng.int(900, 22_000) : rng.int(120, 900);
    const drawnWindows = oauthWindows(drawnId, cursor);
    // A head reads the account's windows as they stand, a 429's as much as any other.
    const windows = accountId === drawnId ? drawnWindows : oauthWindows(accountId, cursor, () => SERVED_FACTOR);
    const rateLimitHit = upstreamStatus === 429;
    const kind = kindOf(accountId);
    const attempt: Attempt = {
      accountId,
      accountKind: kind,
      cacheMissReason: null,
      cooldownSet: rateLimitHit ? rateLimitCooldown(windows, cursor + durationMs, rng.int(20, 75)) : null,
      durationMs,
      index: i,
      outcome,
      overageInUse: kind === 'oauth' ? false : null,
      response: { clearedInputTokens: null, inferenceGeo: 'us', refusalCategory: null, refusalRecommendedModel: null, servedModel: null, speed: 'standard', stopReason: 'end_turn', stopSequence: null },
      // Drawn once the events exist, since a trace names the move an event records.
      selection: null,
      // An answer's usage names the tier it ran on, and a token count's answer carries no usage. Every drawn answer ran
      // on the standard tier.
      serviceTier: upstreamStatus === 200 && !count ? 'standard' : null,
      startedAt: iso(cursor),
      // The upstream names its request on every answer, an error document included, and a message on a 200 alone.
      upstreamMessageId: upstreamStatus === 200 ? upstreamId('msg_01') : null,
      upstreamRequestId: upstreamId('req_011C'),
      upstreamStatus,
      windows,
    };
    if (!ended) {
      trail.push(accountId);
      if (!committed) attempts.push(attempt);
      // The relay commits a 529 or a 502 to the client, so the request ends on it and fails over only on a 429.
      if (!last && upstreamStatus !== 200 && upstreamStatus !== 429) committed = true;
    }
    cursor += durationMs + rng.int(40, 400);
  }
  // A 429 fails over until no member of the pool is left that could serve, so a request that answered one tried each
  // member the draw left, on no draw, in the order the reselect takes them: the pool's tiers, which its order follows
  // (the plan-funded subscriptions, nightly once in overage, the API key), and a token count's by its Pace score
  // inside a tier. They answer 429 too, but secondary, which serves Sonnet 4.6 alone: it refuses any other model, as
  // the rejection story's account does, and its plan draws no usage credits for a fast request.
  const tail = attempts[attempts.length - 1];
  let added = 0;
  if (tail?.upstreamStatus === 429) {
    const sonnet46 = attempts.some((a) => a.accountId === UNPRICED_ACCOUNT);
    // A request secondary took is the Sonnet 4.6 turn it serves, which is no fast one.
    const stillFast = fast && !sonnet46;
    const members = (poolById.get(lane.poolId)?.members ?? []).map((m) => m.accountId);
    for (const accountId of count ? countOrder(members, lane.poolId, cursor, model, '') : members) {
      if (attempts.some((a) => a.accountId === accountId) || !servable(accountId, cursor, model, stillFast) || (stillFast && accountId === UNPRICED_ACCOUNT)) continue;
      const windows = oauthWindows(accountId, cursor, () => SERVED_FACTOR);
      const refuses = accountId === UNPRICED_ACCOUNT && !sonnet46;
      const kind = kindOf(accountId);
      attempts.push({
        ...tail,
        accountId,
        accountKind: kind,
        cooldownSet: refuses ? { reason: 'model_rejected', until: iso(cursor + 300 + HOUR) } : rateLimitCooldown(windows, cursor + 300, 30),
        durationMs: 300,
        index: attempts.length,
        outcome: refuses ? 'upstream_error' : 'rate_limited',
        overageInUse: kind === 'oauth' ? false : null,
        startedAt: iso(cursor),
        upstreamRequestId: upstreamId('req_011C'),
        upstreamStatus: refuses ? 404 : 429,
        windows,
      });
      added += 1;
      cursor += 400;
    }
  }
  return { attempts, chainFrom: attempts.length - added, drawnHead: drawnTrail[drawnTrail.length - 1], whole: !ended && !committed && added === 0 };
}

function buildRequests(): RequestLog[] {
  const rows: RequestLog[] = [];
  const span = 24 * HOUR;
  // The drawer's refusal line needs one answer that carries a refusal, so the first eligible row takes it
  // rather than leaving it to the draw below.
  let refusalOwed = true;
  // The drawer reads a row of each standing the relay publishes; the eligible rows after the refusal take them
  // in turn without a draw of their own, so no later draw moves.
  const autoModeOwed: AutoMode[] = ['server', 'stripped'];
  // A separate stream so the hint draws do not shift the rest of the row's draws.
  const hints = createRng(6_061);
  for (let i = 0; i < 232; i += 1) {
    const lane = rng.weighted([{ value: LANES[0], weight: 11 }, { value: LANES[1], weight: 5 }, { value: LANES[2], weight: 1.6 }]);
    // Skew toward recent hours so the dashboard's "recent" table is dense.
    const offset = Math.pow(rng.next(), 1.4) * span;
    // Whole milliseconds, as the ledger stamps an instant, so the figures read off the row's instants carry no
    // fraction.
    const atMs = Math.floor(NOW - offset - rng.int(1, 60) * 1000);
    const hourOfDay = new Date(atMs).getHours();
    if (hourOfDay >= 1 && hourOfDay <= 6 && rng.chance(0.65)) continue;
    const kind = rng.weighted<RequestKind>(warmsCaches(lane.keyId, lane.poolId) ? WARMED_KINDS : TRAFFIC_KINDS);
    const drawn = rng.weighted(lane.models);
    const finalStatus = kind === 'probe' ? (rng.chance(0.97) ? 200 : 502) : rng.weighted([{ value: 200, weight: 88 }, { value: 429, weight: 5 }, { value: 529, weight: 3 }, { value: 502, weight: 2 }]);
    const attemptsWanted = finalStatus === 200 ? (lane.poolId === 'all' && rng.chance(0.14) ? rng.int(2, 3) : 1) : rng.int(1, 3);
    const { attempts: tried, chainFrom, drawnHead, whole } = buildAttempts(lane, atMs, finalStatus, attemptsWanted, drawn, fastTurn(lane.keyId, drawn, kind), kind === 'count_tokens');
    // A request no account could serve still draws its answer below, so every later row keeps its draws.
    const head = tried[tried.length - 1] as Attempt | undefined;
    const model = modelOf(tried, drawn);
    // The usage draws take the size of the model the draws end on, so every later row keeps its draws. A model that
    // is not asked to think reads none.
    const drawnUsage = finalStatus === 200 ? usageFor(modelOn(drawnHead ?? '', drawn), kind) : { input: 0, cacheRead: 0, cacheWrite5m: 0, cacheWrite1h: 0, output: 0, ...NO_ANSWER_FACTS };
    const usage = model.includes('fable') || model.includes('opus') ? drawnUsage : { ...drawnUsage, thinking: 0 };
    const diagnosis = diagnosed(tried, kind);
    // The story hour's rows leave below, so the rows the drawer reads land before it.
    const owable = lane.keyId === 'laptop' && kind === 'inference' && finalStatus === 200 && whole && atMs < STORY_HOUR_FROM;
    const refused = refusalOwed && owable;
    if (refused) refusalOwed = false;
    const autoMode = !refused && owable ? (autoModeOwed.shift() ?? null) : null;
    const attempts = refused
      ? diagnosis.map((a, at) => (at === diagnosis.length - 1 ? { ...a, response: { ...a.response, refusalCategory: 'cyber', refusalRecommendedModel: 'claude-opus-5', stopReason: 'refusal' } } : a))
      : diagnosis;
    const transport = kind === 'inference' ? (rng.chance(0.86) ? 'sse' : 'json') : 'json';
    // The request runs from its arrival to its last attempt's end, the gaps between attempts included, as the router
    // stamps it.
    const durationMs = head === undefined ? 0 : Date.parse(head.startedAt) + (head.durationMs ?? 0) - atMs;
    // A 429 fails the attempt over, so a request that answers one has run its lane out and the router answers it.
    const errorCode = finalStatus === 200 ? null : finalStatus === 429 ? 'accounts_exhausted' : finalStatus === 529 ? 'overloaded_error' : 'api_error';
    // The head lands inside the serving attempt: past the write at its start, and far enough before its end for the
    // content and the stream's close to follow. One draw either way, so every later row keeps its draws.
    const [headLo, headHi] = kind === 'probe' ? [180, 900] : [420, 4_200];
    const headGap = finalStatus === 200 && kind !== 'count_tokens' ? rng.int(headLo, Math.max(headLo, Math.min(headHi, head === undefined ? headHi : (head.durationMs ?? 0) - STREAM_TAIL))) : null;
    const ttfbMs = headGap === null || head === undefined ? null : Date.parse(head.startedAt) - atMs + headGap;
    // Streams open with a message_start frame, so the first content token lands a little after the first byte and
    // before the stream's close; JSON responses arrive whole. The draw on the main stream is taken and set aside so
    // every later row keeps its draws, and the gap comes from the phase stream.
    if (headGap !== null && kind === 'inference' && transport === 'sse') rng.int(40, 600);
    const ttftMs = ttfbMs === null || kind !== 'inference' || transport !== 'sse' ? ttfbMs : ttfbMs + Math.min(phaseRng.int(40, 600), Math.max(0, durationMs - STREAM_TAIL - ttfbMs));
    const id = `req_${rng.uuid().slice(0, 12)}`;
    const sessionId = rng.pick(SESSION_IDS);
    const agentId = rng.pick(AGENT_IDS);
    rows.push({
      ...REQUEST_SAMPLE,
      id,
      at: iso(atMs),
      autoMode,
      keyGeneration: Date.parse(keyById.get(lane.keyId)!.createdAt),
      keyId: lane.keyId,
      poolId: lane.poolId,
      accountId: head?.accountId ?? null,
      model,
      canonicalModel: model,
      kind,
      outcome: finalStatus === 200 ? 'succeeded' : finalStatus === 429 ? 'exhausted' : 'failed',
      transport,
      status: finalStatus,
      attempts,
      held: null,
      ttfbMs,
      ttftMs,
      durationMs,
      usage,
      costUsd: Math.round(costOf(model, usage) * 10_000) / 10_000,
      diagnosticsSource: sourceOf(attempts, lane.keyId),
      errorCode,
      sessionId,
      agentId,
      userAgent: USER_AGENTS[lane.keyId] ?? null,
      client: CLIENTS[lane.keyId] ?? null,
      agentType: agentId === 'main' ? null : hints.pick(AGENT_TYPES),
      compaction: hints.chance(0.04) ? 'auto' : null,
      contextCompacted: hints.chance(0.04) ? 'auto' : null,
      prevToolMs: hints.chance(0.5) ? hints.int(200, 90_000) : null,
      turnOrigin: lane.keyId === 'ci' ? null : hints.chance(0.9) ? 'human' : hints.pick(['scheduled', 'auto_continuation']),
      capturedBodies: lane.keyId === 'laptop',
      shape: {
        contextEdits: ['clear_thinking_20251015'],
        effort: rng.pick<string | null>(['high', 'max', null]),
        maxTokens: 32_000,
        messageCount: rng.int(1, 40),
        requestedServiceTier: null,
        requestedSpeed: null,
        systemBlocks: 4,
        temperature: null,
        thinkingBudget: 31_999,
        thinkingType: 'enabled',
        toolCount: rng.int(8, 30),
      },
      // Only the laptop key captures bodies here, and every seventh of its rows drew an answer past the cap.
      requestTruncated: false,
      responseTruncated: lane.keyId === 'laptop' && i % 7 === 0,
    });
    if (chainFrom < tried.length) CHAIN_FROM.set(id, chainFrom);
    if (head === undefined) Object.assign(rows[rows.length - 1], unservedAnswer());
    else if (!whole) Object.assign(rows[rows.length - 1], endedAnswer(tried, atMs));
    // A dropped check belongs under notebook, the one key under a pool that turned the check off, and that pool's
    // one member served it; notebook captures no bodies. The row keeps every other value it drew on the laptop
    // lane, so this loop's later draws stay where they were, and the model the lane drew rather than the one
    // `secondary` serves, which stays on `secondary`. The passes after the loop draw per row by its key and pool (a
    // lane in `LANES`, which has none for notebook, and the pool's cache warming), so they draw less for this row and
    // their later draws move.
    if (autoMode === 'stripped') {
      const moved = rows[rows.length - 1];
      // The pool's one member takes the request once: the answering attempt is the request's only one.
      const answered = moved.attempts[moved.attempts.length - 1];
      const studio = kindOf('studio');
      Object.assign(moved, {
        accountId: 'studio',
        attempts: [
          {
            ...answered,
            accountId: 'studio',
            accountKind: studio,
            cacheMissReason: answered.cacheMissReason === 'previous_message_not_found' ? 'none' : answered.cacheMissReason,
            index: 0,
            overageInUse: studio === 'oauth' ? false : null,
            windows: oauthWindows('studio', Date.parse(answered.startedAt), () => SERVED_FACTOR),
          },
        ],
        // The answering attempt keeps its start, so the request runs from its arrival to that attempt's end.
        durationMs: Date.parse(answered.startedAt) + (answered.durationMs ?? 0) - atMs,
        canonicalModel: drawn,
        capturedBodies: false,
        costUsd: Math.round(costOf(drawn, moved.usage) * 10_000) / 10_000,
        keyGeneration: Date.parse(keyById.get('notebook')!.createdAt),
        keyId: 'notebook',
        model: drawn,
        poolId: 'work',
        responseTruncated: false,
      });
      // The key's last use is its one request; its counts derive from the rollups with every other key's.
      keyById.get('notebook')!.lastUsedAt = moved.at;
    }
  }
  // The routing log's story writes its hour by hand, so the draws inside it give way to the story's own rows, and a
  // drawn row that carries a story id gives the story its slot. The 11:00 round's move needs its source warm, so no
  // drawn 429 on secondary falls between the round and that move.
  const coolsSource = (r: RequestLog) => Date.parse(r.at) >= clock(11, 0) && Date.parse(r.at) <= clock(11, 8, 30) && r.attempts.some((a) => a.accountId === UNPRICED_ACCOUNT && a.upstreamStatus === 429);
  const drawnRows = rows.filter((r) => Date.parse(r.at) < STORY_HOUR_FROM && !STORY_IDS.has(r.id) && r.id !== REJECTION_STORY_ID && r.id !== BUDGET_CROSSING_ID && !coolsSource(r));
  rows.length = 0;
  rows.push(...drawnRows);
  rows.sort((a, b) => Date.parse(b.at) - Date.parse(a.at));
  // Every desktop row before the live key's creation is its earlier holder's, and the holder's last request ran on
  // the session the live key holds now, so the Sessions view lists that id once under each holder.
  const holderRows = rows.filter((r) => r.keyId === 'desktop' && Date.parse(r.at) < DELETED_HOLDER_END);
  for (const r of holderRows) r.keyGeneration = DELETED_HOLDER_GENERATION;
  const holderLast = holderRows.find((r) => r.kind !== 'probe');
  if (holderLast) Object.assign(holderLast, { sessionId: SESSION_IDS[0] });
  // A stream can commit its 200 head and fail after it: the newest such desktop stream keeps the status and
  // carries the error, which is the row the Requests table and the drawer must not draw as a success.
  const failedAfterHead = rows.find((r) => r.keyId === 'desktop' && r.kind === 'inference' && r.transport === 'sse' && r.status === 200);
  if (failedAfterHead) {
    failedAfterHead.errorCode = 'overloaded_error';
    failedAfterHead.outcome = 'failed';
    failedAfterHead.attempts = failedAfterHead.attempts.map((a, i, all) => (i === all.length - 1 ? { ...a, outcome: 'upstream_error' as const } : a));
  }
  // The newest desktop stream after that one ended before its `message_stop`: the row the Requests table draws as a
  // failure and the drawer names.
  const incompleteStream = rows.find((r) => r !== failedAfterHead && r.keyId === 'desktop' && r.kind === 'inference' && r.transport === 'sse' && r.status === 200);
  if (incompleteStream) {
    incompleteStream.errorCode = 'stream_incomplete';
    incompleteStream.outcome = 'incomplete';
    incompleteStream.attempts = incompleteStream.attempts.map((a, i, all) => (i === all.length - 1 ? { ...a, outcome: 'upstream_error' as const } : a));
  }
  // A header sample can carry a window's reset without its utilization; the oldest row that reported the 7-day
  // window stands in for one, so the drawer has a window to read as not reported.
  const unsampled = [...rows].reverse().find((r) => r.attempts.some((a) => a.windows.some((w) => w.id === '7d')));
  if (unsampled) {
    unsampled.attempts = unsampled.attempts.map((a) => ({ ...a, windows: a.windows.map((w) => (w.id === '7d' ? { ...w, utilization: null } : w)) }));
  }
  // Fast mode is the client's own toggle and serves Opus: the desktop key's Opus turns asked for it and were
  // answered fast by whichever account served them.
  for (const r of rows) {
    if (!fastTurn(r.keyId, r.model, r.kind)) continue;
    r.shape = { ...r.shape, requestedSpeed: 'fast' };
    r.attempts = r.attempts.map((a, i, all) => (i === all.length - 1 ? { ...a, response: { ...a.response, speed: 'fast' } } : a));
    r.costUsd = Math.round(costOf(r.model, r.usage, true) * 10_000) / 10_000;
  }
  // A stream the client closed: its 200 head went out and no upstream error followed, so the request ends canceled
  // with the usage it had reached, under the code the relay files a client's leaving with. One drawn row takes it, so
  // no count another view draws moves: a single-attempt stream off the laptop key whose captures the drawer checks
  // read, and none of the rows above. The hour bound keeps the pick out of the routing log's hand-written hour, whose
  // records name no canceled stream, should that hour ever hold a drawn row.
  const canceled = rows.find((r) => r.keyId !== 'laptop' && r.keyId !== 'notebook' && r.kind === 'inference' && r.transport === 'sse' && r.outcome === 'succeeded' && r.attempts.length === 1 && Date.parse(r.at) < NOW - HOUR);
  if (canceled) {
    canceled.outcome = 'canceled';
    canceled.errorCode = 'client_gone';
    canceled.attempts = canceled.attempts.map((a) => ({ ...a, outcome: 'canceled' as const }));
  }
  rows.push(...storyRequests());
  return rows.sort((a, b) => Date.parse(b.at) - Date.parse(a.at));
}

/**
 * A cooldown an answer set has lifted by the time another request's attempt uses its account in a way it would hold,
 * so no attempt lands where the routing state still held the account cooling: the drawn deadline is cut to that use.
 * A hold starts only where no cooldown holds its account, so it needs no lift.
 */
function liftCooldowns(rows: RequestLog[]) {
  const uses = rows
    .flatMap((r) => {
      const fast = fastTurn(r.keyId, r.model, r.kind);
      return r.attempts.map((a) => ({ accountId: a.accountId, at: Date.parse(a.startedAt), fast, model: r.canonicalModel, r }));
    })
    .sort((x, y) => x.at - y.at);
  for (const r of rows) {
    for (const a of r.attempts) {
      const set = a.cooldownSet;
      if (!set) continue;
      const setAt = Date.parse(a.startedAt) + (a.durationMs ?? 0);
      const until = Date.parse(set.until);
      const holds = (u: (typeof uses)[number]) => u.r !== r && u.accountId === a.accountId && coolingHolds(set.reason, r.canonicalModel, u.model, u.fast);
      const next = uses.find((u) => holds(u) && u.at >= setAt && u.at < until);
      if (next) a.cooldownSet = { ...set, until: iso(next.at) };
    }
  }
}

/** A turn the story runs in fast mode: the desktop key's Opus inference. */
const fastTurn = (keyId: string | null, model: string, kind: RequestKind) => keyId === 'desktop' && model.includes('opus') && kind === 'inference';

/**
 * Whether a cooldown an answer set holds a request: a model's refusal holds that model, a fast lane's a fast request,
 * a spent plan window a standard one, and a retry-after every request.
 */
const coolingHolds = (reason: string, setFor: string, model: string, fast: boolean) =>
  reason === 'model_rejected' ? model === setFor : reason === 'fast_rate_limit' ? fast : reason.includes('window') ? !fast : true;

/** Whether an answer's cooldown keeps the account from a request at `at`, before any later use lifted it. */
function coolingAt(accountId: string, at: number, model: string, fast: boolean): boolean {
  return REQUESTS.some((r) =>
    r.attempts.some((a) => {
      const set = a.cooldownSet;
      if (!set || a.accountId !== accountId) return false;
      const from = Date.parse(a.startedAt) + (a.durationMs ?? 0);
      return at >= from && at < Date.parse(set.until) && coolingHolds(set.reason, r.canonicalModel, model, fast);
    }),
  );
}

/** Whether a request could wait on the account at `at`: one it could be served by then, which no cooldown holds. */
const holdableAt = (r: RequestLog, accountId: string, at: number) => {
  const fast = fastTurn(r.keyId, r.model, r.kind);
  return servable(accountId, at, r.canonicalModel, fast) && !coolingAt(accountId, at, r.canonicalModel, fast);
};

/**
 * Seats a hold before the request's first attempt: the hold starts at the request's arrival and the attempt once it
 * ends, so the arrival moves back by the wait, and a pin the hold's selection found expired is released then.
 * Answers false, changing nothing, where the account could not have been waited on at that start.
 */
function holdFirst(r: RequestLog, hold: CohortHold): boolean {
  const first = r.attempts[0];
  const start = Date.parse(first.startedAt) - hold.heldMs;
  if (!holdableAt(r, first.accountId, start)) return false;
  const arrived = r.at;
  r.at = iso(start);
  // The wait sits before the first attempt, so the client saw it inside the head's latency and the whole duration.
  r.durationMs = (r.durationMs ?? 0) + hold.heldMs;
  if (r.ttfbMs !== null) r.ttfbMs += hold.heldMs;
  if (r.ttftMs !== null) r.ttftMs += hold.heldMs;
  const events = REQUEST_EVENTS.get(r.id);
  if (events) REQUEST_EVENTS.set(r.id, events.map((e) => (e.kind === 'pin_expired' && e.at === arrived ? { ...e, at: r.at } : e)));
  return true;
}

/** Whether the relay fails the attempt over: a 429, or a model the account refuses, each of which sets a cooldown. */
export const failsOver = (a: Attempt) => a.upstreamStatus === 429 || a.cooldownSet?.reason === 'model_rejected';

/** The rejection story's refused attempt: an account refuses a model it does not serve before any inference runs. */
const REJECTION_MS = 180;

/** What the router answers a request no account of its pool could serve: a 429 with no upstream call behind it. */
const unservedAnswer = (): Partial<RequestLog> => ({
  accountId: null,
  attempts: [],
  costUsd: 0,
  durationMs: null,
  errorCode: 'accounts_exhausted',
  outcome: 'exhausted',
  status: 429,
  ttfbMs: null,
  ttftMs: null,
  usage: { input: 0, cacheRead: 0, cacheWrite5m: 0, cacheWrite1h: 0, output: 0, ...NO_ANSWER_FACTS },
});

/**
 * What a request answers when it did not end as drawn: the upstream's own error after a 529 or a 502, which the relay
 * commits, and the router's exhaustion once a failover finds no member of its pool left to try.
 */
const endedAnswer = (attempts: Attempt[], atMs: number): Partial<RequestLog> => {
  const last = attempts[attempts.length - 1];
  const exhausted = failsOver(last);
  const status = exhausted ? 429 : (last.upstreamStatus ?? 502);
  return {
    costUsd: 0,
    durationMs: Date.parse(last.startedAt) + (last.durationMs ?? 0) - atMs,
    errorCode: exhausted ? 'accounts_exhausted' : status === 529 ? 'overloaded_error' : 'api_error',
    outcome: exhausted ? 'exhausted' : 'failed',
    status,
    ttfbMs: null,
    ttftMs: null,
    usage: { input: 0, cacheRead: 0, cacheWrite5m: 0, cacheWrite1h: 0, output: 0, ...NO_ANSWER_FACTS },
  };
};

/** One attempt of a story request: the account, when it started, how long it ran, and the upstream's answer. */
interface StoryAttempt {
  accountId: string;
  at: number;
  cooldown?: { reason: string; until: number };
  /** The 5-hour reading the answer's head reported, where it moved past the reading the decision read. */
  head5h?: number;
  ms: number;
  status: number;
}

/** The geography an answer reported, where the model does not take the parameter. */
type StoryGeo = 'not_available';

interface StorySeed {
  agentId: string;
  at: number;
  attempts: StoryAttempt[];
  canonical?: string;
  /** Whether the bodies were captured, where it is not the laptop key's rule. */
  capturedBodies?: boolean;
  clientDrift?: string;
  contextCompacted?: string;
  errorCode?: string;
  /** The answer's inference geography where it is not the story's `us`. */
  geo?: StoryGeo;
  held?: CohortHold;
  id: string;
  keyId: string;
  kind?: RequestKind;
  model: string;
  outcome?: RequestOutcome;
  requestClass: string;
  /** The client's session, a prefix of the story's own ids. */
  session: string;
  status?: number;
  /** The tier the answer ran on, where it is not the standard one. */
  tier?: 'priority';
  usage?: Partial<Usage>;
  userAgent?: string;
}

/** The routing log's hand-written requests: the story hour, the morning's shedding round and its move. */
export const STORY_IDS = new Set([
  'req_0b7c2d94-e61', 'req_1d9c3e55-0b2', 'req_1eaa9bc1-a18', 'req_2e7c90a1-6d4', 'req_31f0b7c2-6e1',
  'req_3b8e51f4-0a9', 'req_4b0e9d17-c31', 'req_51d0e2b8-7a0', 'req_5e2f7a19-b03', 'req_69e30548-32e',
  'req_6c3a9e27-5b1', 'req_7a2d4c18-e95', 'req_868264bc-611', 'req_9a07c4e1-d55', 'req_a1f04c2e-7b3',
  'req_a6c19e73-5d2', 'req_b2bed817-a58', 'req_b72e9d05-c14', 'req_c41d0311-200', 'req_c90d3f61-a28',
  'req_d4e8a7b2-916', 'req_da52d141-a5a', 'req_e0a4c6f2-118', 'req_e5a1c093-4d7', 'req_ef099b34-453',
  'req_f3b6d218-85a', 'req_f8085653-422',
]);
/** The four-attempt failover whose first account refused the model the client spelled without its date. */
export const REJECTION_STORY_ID = 'req_7015b657-290';
/** The jobs request whose completion crossed research's daily budget. */
export const BUDGET_CROSSING_ID = 'req_7c19e4a0-2f3';
/** The session a laptop client sent. */
export const CLIENT_SESSION_ID = '7d3e1c20-5b4a-4e8f-9c61-0a2b3c4d5e6f';
/** The desktop session the mod released whole after its one turn, written out as its row lists past the drawn ids. */
export const RELEASED_SESSION_ID = '3c36d663-d074-42f6-8738-f54da7c9f026';

const sessionOf = (prefix: string) => SESSION_IDS.find((id) => id.startsWith(prefix)) ?? prefix;

function storyRow(seed: StorySeed): RequestLog {
  const lane = LANES.find((l) => l.keyId === seed.keyId)!;
  const last = seed.attempts[seed.attempts.length - 1];
  const answered = last?.status === 200;
  const usage: Usage = { input: 0, cacheRead: 0, cacheWrite5m: 0, cacheWrite1h: 0, output: 0, ...NO_ANSWER_FACTS, ...(answered ? seed.usage : {}) };
  const fast = fastTurn(seed.keyId, seed.model, seed.kind ?? 'inference');
  const sessionId = sessionOf(seed.session);
  const kind = seed.kind ?? 'inference';
  const attempts = seed.attempts.map((a, index): Attempt => {
    const windows = oauthWindows(a.accountId, a.at, () => SERVED_FACTOR);
    const oauth = ACCOUNTS.find((x) => x.id === a.accountId)?.kind === 'oauth';
    return {
      accountId: a.accountId,
      accountKind: kindOf(a.accountId),
      // A failover lands on an account that never saw the turn before this one.
      cacheMissReason: a.status !== 200 || kind === 'count_tokens' ? null : !oauth ? 'not_requested' : index > 0 ? 'previous_message_not_found' : 'none',
      cooldownSet: a.cooldown ? { reason: a.cooldown.reason, until: iso(a.cooldown.until) } : null,
      durationMs: a.ms,
      index,
      outcome: a.status === 200 ? (index === seed.attempts.length - 1 && seed.errorCode ? 'upstream_error' : 'succeeded') : a.status === 429 ? 'rate_limited' : 'upstream_error',
      overageInUse: oauth ? false : null,
      response: { clearedInputTokens: null, inferenceGeo: a.status === 200 ? (seed.geo ?? 'us') : null, refusalCategory: null, refusalRecommendedModel: null, servedModel: null, speed: a.status === 200 ? (fast ? 'fast' : 'standard') : null, stopReason: a.status === 200 ? 'end_turn' : null, stopSequence: null },
      // The routing log's records write each story attempt's trace, so the drawer and the card read one decision.
      selection: null,
      // The tier is read off the answer's usage, which a token count's answer carries none of.
      serviceTier: a.status === 200 && kind !== 'count_tokens' ? (seed.tier ?? 'standard') : null,
      startedAt: iso(a.at),
      upstreamMessageId: a.status === 200 ? upstreamId('msg_01') : null,
      upstreamRequestId: upstreamId('req_011C'),
      upstreamStatus: a.status,
      // A head reads the windows as they stand, or the 5-hour reading the seed gives where the answer moved past it.
      windows: windows.map((w) => (w.id === '5h' && a.head5h !== undefined ? { ...w, utilization: a.head5h } : w)),
    };
  });
  const durationMs = seed.attempts.length ? last.at + last.ms - seed.attempts[0].at + (seed.held?.heldMs ?? 0) : null;
  // The head lands 640 ms into the serving attempt and the first content 270 ms after it, each held under the finish.
  const headRoom = durationMs === null ? 0 : durationMs - STREAM_TAIL;
  // A request the relay answered with an error of its own forwarded no head, whatever the upstream's head was.
  const ttfbMs = answered && (seed.status ?? 200) === 200 && kind !== 'count_tokens' ? Math.min(last.at - seed.at + 640, headRoom) : null;
  const ttftMs = ttfbMs === null ? null : kind === 'inference' ? Math.min(ttfbMs + 270, headRoom) : ttfbMs;
  const status = seed.status ?? last?.status ?? 429;
  return {
    ...REQUEST_SAMPLE,
    accountId: answered ? last.accountId : null,
    agentId: seed.agentId,
    agentType: seed.agentId === 'main' ? null : 'general-purpose',
    at: iso(seed.at),
    attempts,
    autoMode: null,
    canonicalModel: seed.canonical ?? seed.model,
    capturedBodies: seed.capturedBodies ?? seed.keyId === 'laptop',
    client: CLIENTS[seed.keyId] ?? null,
    clientDrift: seed.clientDrift ?? null,
    compaction: null,
    contextCompacted: seed.contextCompacted ?? null,
    costUsd: answered ? Math.round(costOf(seed.model, usage, fast) * 10_000) / 10_000 : 0,
    diagnosticsSource: sourceOf(attempts, seed.keyId),
    durationMs,
    errorCode: seed.errorCode ?? (status === 200 ? null : 'rate_limit_error'),
    forkDepth: null,
    forkedFrom: null,
    held: seed.held ?? null,
    id: seed.id,
    keyGeneration: Date.parse(keyById.get(seed.keyId)!.createdAt),
    keyId: seed.keyId,
    kind,
    model: seed.model,
    outcome: seed.outcome ?? (answered ? 'succeeded' : 'failed'),
    poolId: lane.poolId,
    prevToolMs: null,
    requestClass: seed.requestClass,
    requestTruncated: false,
    responseTruncated: false,
    sessionId,
    shape: { ...REQUEST_SAMPLE.shape, requestedSpeed: fast ? 'fast' : null },
    status,
    transport: kind === 'inference' ? 'sse' : 'json',
    ttfbMs,
    ttftMs,
    turnOrigin: seed.keyId === 'ci' ? null : 'human',
    usage,
    userAgent: seed.userAgent ?? USER_AGENTS[seed.keyId] ?? null,
  };
}

/**
 * The story's requests at their clock times: the hour before the pinned instant written by hand so every card the
 * routing log draws is on the page, the morning's shedding round and its move, the jobs request whose completion
 * crossed the budget, and the four-attempt failover whose first account refused the model.
 */
function storyRequests(): RequestLog[] {
  const t = clock;
  const rateLimited = (until: number) => ({ reason: '429 on 5h window', until });
  // A 429 whose head rejects no plan window cools its account for the retry-after alone.
  const retryAfter = (answeredAt: number) => ({ reason: '429, retry after 20 s', until: answeredAt + 20_000 });
  const seeds: StorySeed[] = [
    // An answer the upstream ran on its priority tier.
    { agentId: 'main', at: t(14, 27, 28), attempts: [{ accountId: 'primary', at: t(14, 27, 28), ms: 6_400, status: 200 }], id: 'req_f8085653-422', keyId: 'desktop', model: 'claude-sonnet-5-5', requestClass: 'main', session: 'd7167642', tier: 'priority', usage: { cacheRead: 48_100, cacheWrite1h: 2_100, input: 3_900, output: 1_250 } },
    {
      agentId: 'main',
      at: t(14, 26, 58),
      attempts: [
        { accountId: 'primary', at: t(14, 26, 58), cooldown: { reason: 'fast_rate_limit', until: t(14, 42) }, ms: 2_000, status: 429 },
        { accountId: 'nightly', at: t(14, 27, 1), ms: 9_800, status: 200 },
      ],
      id: 'req_4b0e9d17-c31',
      keyId: 'desktop',
      model: 'claude-opus-5-5',
      requestClass: 'main',
      session: '1c0f9727',
      usage: { cacheWrite1h: 61_000, input: 2_400, output: 3_200, thinking: 1_400 },
    },
    {
      agentId: '4d1a',
      at: t(14, 24, 9),
      attempts: [
        { accountId: 'primary', at: t(14, 24, 9), cooldown: { reason: 'model_rejected', until: t(15, 24) }, ms: 1_000, status: 404 },
        { accountId: 'nightly', at: t(14, 24, 10), ms: 2_000, status: 200 },
      ],
      canonical: HAIKU,
      id: 'req_2e7c90a1-6d4',
      keyId: 'laptop',
      model: 'claude-haiku-4-5',
      requestClass: 'subagent',
      session: 'c52dbc8e',
      usage: { cacheWrite5m: 14_200, input: 1_200, output: 380 },
    },
    {
      agentId: 'e90b',
      at: t(14, 21, 12),
      attempts: [{ accountId: 'primary', at: t(14, 21, 14), ms: 5_200, status: 200 }],
      held: { cohort: 'primary', heldMs: 2_000, waitedOn: ['primary'] },
      id: 'req_69e30548-32e',
      keyId: 'desktop',
      model: 'claude-sonnet-5-5',
      requestClass: 'subagent',
      session: '89cd1c92',
      usage: { input: 19_600, output: 820 },
    },
    { agentId: 'main', at: t(14, 19, 2), attempts: [{ accountId: 'primary', at: t(14, 19, 2), ms: 7_100, status: 200 }], errorCode: 'overloaded_error', id: 'req_9a07c4e1-d55', outcome: 'failed', keyId: 'desktop', model: 'claude-sonnet-5-5', requestClass: 'main', session: 'd7167642', usage: { cacheRead: 44_900, cacheWrite1h: 1_800, input: 2_600, output: 960 } },
    // The runner sends again on its key after its revocation at 14:17:30, which the selection refuses.
    { agentId: 'main', at: t(14, 18, 5), attempts: [], errorCode: 'auth_unknown', id: 'req_6c3a9e27-5b1', keyId: 'ci', model: HAIKU, outcome: 'rejected', requestClass: 'main', session: 'cd9a5904', status: 401 },
    // A compacted conversation renews its pin cold and writes its prefix afresh.
    { agentId: 'main', at: t(14, 17, 52), attempts: [{ accountId: 'primary', at: t(14, 17, 52), ms: 3_300, status: 200 }], contextCompacted: 'auto', errorCode: 'stream_incomplete', id: 'req_31f0b7c2-6e1', outcome: 'incomplete', keyId: 'laptop', model: HAIKU, requestClass: 'main', session: '4ef130f1', usage: { cacheWrite5m: 22_900, input: 1_100, output: 410 } },
    { agentId: 'main', at: t(14, 16, 40), attempts: [], errorCode: 'accounts_exhausted', id: 'req_51d0e2b8-7a0', keyId: 'ci', model: HAIKU, outcome: 'exhausted', requestClass: 'main', session: 'cd9a5904', status: 429 },
    { agentId: 'main', at: t(14, 14, 2), attempts: [], clientDrift: 'hash', errorCode: 'client_drift', id: 'req_e0a4c6f2-118', keyId: 'laptop', model: 'claude-sonnet-4-6', outcome: 'rejected', requestClass: 'main', session: '5eeed6ce', status: 400, userAgent: 'claude-cli/2.1.271 (external, cli)' },
    { agentId: 'main', at: t(14, 11, 44), attempts: [{ accountId: 'primary', at: t(14, 11, 44), ms: 8_300, status: 200 }], id: 'req_b2bed817-a58', keyId: 'desktop', model: 'claude-sonnet-5-5', requestClass: 'main', session: 'd7167642', usage: { cacheRead: 30_900, cacheWrite1h: 21_400, input: 1_900, output: 1_480 } },
    // The model takes no inference geography parameter, so its answer reports the geography as not available.
    { agentId: 'main', at: t(14, 9, 41), attempts: [{ accountId: 'primary', at: t(14, 9, 41), ms: 4_800, status: 200 }], geo: 'not_available', id: 'req_ef099b34-453', keyId: 'laptop', model: HAIKU, requestClass: 'main', session: '4ef130f1', usage: { cacheWrite5m: 31_000, input: 9_800, output: 2_100 } },
    // A token count follows its pin to the spent secondary, whose head rejects the 5-hour window: the 429 starts the
    // cooldown secondary carries, and the count fails over to primary.
    {
      agentId: 'e90b',
      at: t(14, 9, 30),
      attempts: [
        { accountId: 'secondary', at: t(14, 9, 30), cooldown: rateLimited(t(15, 42)), head5h: 1, ms: 1_000, status: 429 },
        { accountId: 'primary', at: t(14, 9, 31), ms: 300, status: 200 },
      ],
      id: 'req_3b8e51f4-0a9',
      keyId: 'desktop',
      kind: 'count_tokens',
      model: 'claude-sonnet-4-6',
      requestClass: 'subagent',
      session: '407dd39b',
      usage: { input: 41_000 },
    },
    // The keepalive's probe of a pin on secondary finds its window spent and moves the pin to primary.
    { agentId: 'main', at: t(14, 6, 30), attempts: [{ accountId: 'primary', at: t(14, 6, 30), ms: 1_400, status: 200 }], id: 'req_c41d0311-200', keyId: 'desktop', kind: 'probe', model: 'claude-sonnet-4-6', requestClass: 'main', session: '229bf72c', usage: { cacheWrite1h: 46_000, output: 1 } },
    { agentId: 'main', at: t(14, 5, 12), attempts: [{ accountId: 'primary', at: t(14, 5, 12), ms: 7_900, status: 200 }], id: 'req_da52d141-a5a', keyId: 'desktop', model: 'claude-sonnet-4-6', requestClass: 'main', session: '0d588de5', usage: { cacheRead: 30_100, cacheWrite1h: 14_800, input: 2_200, output: 1_120 } },
    // Secondary answers a 429 that rejects no plan window while its head reads the 5-hour window spent: the account
    // cools for the retry-after alone and the turn fails over.
    {
      agentId: 'main',
      at: t(14, 2, 10),
      attempts: [
        { accountId: 'secondary', at: t(14, 2, 10), cooldown: retryAfter(t(14, 2, 11)), head5h: 1, ms: 1_000, status: 429 },
        { accountId: 'primary', at: t(14, 2, 11), ms: 6_600, status: 200 },
      ],
      id: 'req_1d9c3e55-0b2',
      keyId: 'desktop',
      model: 'claude-sonnet-4-6',
      requestClass: 'main',
      session: 'bcc805b6',
      usage: { cacheWrite1h: 38_900, input: 4_100, output: 1_310 },
    },
    // The turns that place the hour's sessions on secondary before its spent edge: the pins the beats after them read.
    { agentId: 'main', at: t(14, 1, 40), attempts: [{ accountId: 'secondary', at: t(14, 1, 40), ms: 6_000, status: 200 }], id: 'req_d4e8a7b2-916', keyId: 'desktop', model: 'claude-sonnet-4-6', requestClass: 'main', session: '229bf72c', usage: { cacheWrite1h: 46_000, input: 1_200, output: 520 } },
    { agentId: 'main', at: t(14, 1, 35), attempts: [{ accountId: 'secondary', at: t(14, 1, 35), ms: 12_000, status: 200 }], id: 'req_c90d3f61-a28', keyId: 'desktop', model: 'claude-sonnet-4-6', requestClass: 'main', session: '0d588de5', usage: { cacheWrite1h: 44_600, input: 1_800, output: 870 } },
    { agentId: 'e90b', at: t(14, 1, 30), attempts: [{ accountId: 'secondary', at: t(14, 1, 30), ms: 4_000, status: 200 }], id: 'req_b72e9d05-c14', keyId: 'desktop', model: 'claude-sonnet-4-6', requestClass: 'subagent', session: '407dd39b', usage: { cacheWrite5m: 39_500, input: 1_400, output: 610 } },
    { agentId: 'main', at: t(14, 0, 10), attempts: [{ accountId: 'secondary', at: t(14, 0, 10), ms: 14_000, status: 200 }], id: 'req_a1f04c2e-7b3', keyId: 'desktop', model: 'claude-sonnet-4-6', requestClass: 'main', session: 'bcc805b6', usage: { cacheWrite1h: 58_900, input: 2_300, output: 1_040 } },
    { agentId: 'e90b', at: t(11, 0), attempts: [{ accountId: 'secondary', at: t(11, 0), ms: 6_200, status: 200 }], id: 'req_1eaa9bc1-a18', keyId: 'desktop', model: 'claude-sonnet-4-6', requestClass: 'subagent', session: '407dd39b', usage: { cacheRead: 40_800, cacheWrite5m: 1_200, input: 1_700, output: 900 } },
    { agentId: 'e90b', at: t(11, 8, 30), attempts: [{ accountId: 'primary', at: t(11, 8, 30), ms: 9_400, status: 200 }], id: 'req_868264bc-611', keyId: 'desktop', model: 'claude-sonnet-4-6', requestClass: 'subagent', session: 'c64358b5', usage: { cacheWrite5m: 72_000, input: 3_000, output: 1_600 } },
    // The subagent turns that place two of the round's sessions on secondary just before it, and the keepalive's probe
    // of the first, which renews its pin without walking past a rule.
    { agentId: 'e90b', at: t(10, 59, 30), attempts: [{ accountId: 'secondary', at: t(10, 59, 30), ms: 900, status: 200 }], id: 'req_0b7c2d94-e61', keyId: 'desktop', kind: 'probe', model: 'claude-sonnet-4-6', requestClass: 'subagent', session: '407dd39b', usage: { cacheRead: 40_800, output: 1 } },
    { agentId: 'e90b', at: t(10, 59, 10), attempts: [{ accountId: 'secondary', at: t(10, 59, 10), ms: 7_000, status: 200 }], id: 'req_f3b6d218-85a', keyId: 'desktop', model: 'claude-sonnet-4-6', requestClass: 'subagent', session: 'c64358b5', usage: { cacheWrite5m: 72_000, input: 2_600, output: 1_300 } },
    { agentId: 'e90b', at: t(10, 58, 59), attempts: [{ accountId: 'secondary', at: t(10, 58, 59), ms: 3_000, status: 200 }], id: 'req_e5a1c093-4d7', keyId: 'desktop', model: 'claude-sonnet-4-6', requestClass: 'subagent', session: '407dd39b', usage: { cacheWrite5m: 40_800, input: 1_500, output: 700 } },
    { agentId: 'main', at: t(12, 40, 58), attempts: [{ accountId: 'research', at: t(12, 40, 58), ms: 9_000, status: 200 }], id: BUDGET_CROSSING_ID, keyId: 'ci', model: HAIKU, requestClass: 'main', session: 'cd9a5904', usage: { input: 6_200, output: 2_900 } },
    {
      agentId: 'main',
      at: t(10, 58, 40),
      attempts: [
        { accountId: 'secondary', at: t(10, 58, 40), cooldown: { reason: 'model_rejected', until: t(10, 58, 40) + REJECTION_MS + HOUR }, ms: REJECTION_MS, status: 404 },
        { accountId: 'primary', at: t(10, 58, 40) + REJECTION_MS, cooldown: retryAfter(t(10, 58, 40) + REJECTION_MS + 700), ms: 700, status: 429 },
        { accountId: 'nightly', at: t(10, 58, 41), cooldown: retryAfter(t(10, 58, 41) + 800), ms: 800, status: 429 },
        { accountId: 'overflow', at: t(10, 58, 42), ms: 5_200, status: 200 },
      ],
      canonical: 'claude-sonnet-4-5-20250929',
      id: REJECTION_STORY_ID,
      keyId: 'desktop',
      model: 'claude-sonnet-4-5',
      requestClass: 'main',
      session: '0d588de5',
      usage: { cacheRead: 21_400, cacheWrite5m: 3_100, input: 1_500, output: 880 },
    },
    // Primary's head arrived and the relay could not decode the stream after it, so it forwarded none and answered 502.
    {
      agentId: 'main',
      at: t(14, 22, 48),
      attempts: [{ accountId: 'primary', at: t(14, 22, 48), ms: 1_900, status: 200 }],
      errorCode: 'stream_undecodable',
      id: 'req_5e2f7a19-b03',
      keyId: 'laptop',
      model: 'claude-sonnet-4-6',
      outcome: 'failed',
      requestClass: 'main',
      session: CLIENT_SESSION_ID,
      status: 502,
    },
    // A Fable turn on primary's plan whose bodies were captured, so the drawer reads a plan-funded answer of the model.
    { agentId: 'main', at: t(14, 23, 30), attempts: [{ accountId: 'primary', at: t(14, 23, 30), ms: 8_200, status: 200 }], capturedBodies: true, id: 'req_a6c19e73-5d2', keyId: 'desktop', model: 'claude-fable-5-1', requestClass: 'main', session: '4ef130f1', usage: { cacheRead: 52_300, cacheWrite1h: 3_400, input: 2_100, output: 1_650, thinking: 640 } },
    // The one turn of the session the mod released whole later. The release ends the keepalive record alone, so the
    // pin this turn wrote stays.
    { agentId: 'main', at: t(14, 3), attempts: [{ accountId: 'primary', at: t(14, 3), ms: 5_600, status: 200 }], id: 'req_7a2d4c18-e95', keyId: 'desktop', model: 'claude-sonnet-5', requestClass: 'main', session: RELEASED_SESSION_ID, usage: { cacheWrite1h: 15_800, input: 2_300, output: 960 } },
  ];
  return seeds.map(storyRow);
}

export const REQUESTS: RequestLog[] = buildRequests();
export const requestById = new Map(REQUESTS.map((r) => [r.id, r]));

// The ci key's last use is its newest request the key check let through: a send after the revocation is refused as an
// unknown key and uses nothing.
keyById.get('ci')!.lastUsedAt = REQUESTS.find((r) => r.keyId === 'ci' && r.errorCode !== 'auth_unknown')?.at ?? null;

/**
 * The head that first read nightly in plan overage: the first attempt the day sent it after its spend cap was raised
 * at 11:30:12, or a minute past the raise where the draws sent none. The account's own overage reads from it.
 */
export const OVERAGE_FROM = ((): number => {
  const raised = clock(11, 30, 12);
  const after = REQUESTS.flatMap((r) => r.attempts).map((a) => (a.accountId === 'nightly' ? Date.parse(a.startedAt) : Number.NaN)).filter((at) => at >= raised);
  return after.length ? Math.min(...after) : raised + MINUTE;
})();
const nightly = ACCOUNTS.find((a) => a.id === 'nightly');
if (nightly?.overage) nightly.overage.since = iso(OVERAGE_FROM);
// The attempt literals run before the instant exists, so nightly's heads from it on read their overage flag here.
for (const a of REQUESTS.flatMap((r) => r.attempts)) if (a.accountId === 'nightly' && Date.parse(a.startedAt) >= OVERAGE_FROM) a.overageInUse = true;

/**
 * The pin each request's first selection found live, by request id: where the pin's last use left it within the
 * pool's affinity window, and when it landed there, so a renewal or a move rests on a request the day holds. A
 * request leaves its pin on its last attempt's account unless that answer cooled the account, which releases the pin.
 * A token count renews a live pin and creates none.
 */
const LIVE_PIN: Map<string, { accountId: string; createdAt: number }> = (() => {
  const out = new Map<string, { accountId: string; createdAt: number }>();
  const pins = new Map<string, { accountId: string; at: number; createdAt: number }>();
  for (const r of [...REQUESTS].sort((x, y) => Date.parse(x.at) - Date.parse(y.at))) {
    const ttl = (poolById.get(r.poolId ?? '')?.affinityTtlSeconds ?? 0) * 1_000;
    const first = r.attempts[0] as Attempt | undefined;
    if (ttl === 0 || r.sessionId === null || first === undefined) continue;
    const key = `${r.keyId}|${r.sessionId}|${r.agentId}|${r.canonicalModel}`;
    const held = pins.get(key);
    const live = held !== undefined && Date.parse(first.startedAt) - held.at <= ttl ? held : undefined;
    if (live) out.set(r.id, { accountId: live.accountId, createdAt: live.createdAt });
    const last = r.attempts[r.attempts.length - 1];
    const at = Date.parse(last.startedAt);
    if (failsOver(last)) pins.delete(key);
    else if (live?.accountId === last.accountId) pins.set(key, { ...live, at });
    else if (r.kind !== 'count_tokens') pins.set(key, { accountId: last.accountId, at, createdAt: at });
  }
  return out;
})();

/** Whether an account serves the model: secondary serves Sonnet 4.6 alone. */
const serves = (accountId: string, model: string) => accountId !== UNPRICED_ACCOUNT || model === 'claude-sonnet-4-6';

/**
 * The move rule that takes a pin on `from` to the row's account in the engine's order, or null where none would. The
 * pin must be live on `from`, where an earlier request of the day left it. A spent window moves a standard request to
 * an account holding allowance outside plan overage, nightly's overage hands a standard request back to a plan-funded
 * subscription, and an API key's pin returns to one once it has sat its dwell there. The first rule whose reason
 * holds decides, so a move to an account
 * that rule would not take is no move. A drain needs a cold pin and a shed a mark, which no drawn pin carries, and a
 * token count follows its pin without walking the rules.
 */
function supportedRule(r: RequestLog, from: string): PinMigrationReason | null {
  const pool = poolById.get(r.poolId ?? '');
  const head = r.attempts[0];
  const pinned = LIVE_PIN.get(r.id);
  if (!pool || !head || r.kind === 'count_tokens' || pinned?.accountId !== from || !serves(from, r.canonicalModel)) return null;
  const at = Date.parse(head.startedAt);
  const fast = fastTurn(r.keyId, r.model, r.kind);
  const registryKind = (id: string) => ACCOUNTS.find((a) => a.id === id)?.kind;
  const inOverage = (id: string) => id === 'nightly' && at >= OVERAGE_FROM;
  const spent = (windows: AttemptWindow[]) => spentWindows(windows).length > 0;
  const known = head.windows.some((w) => w.id === '5h' && w.utilization !== null) && head.windows.some((w) => w.id === '7d' && w.utilization !== null);
  const holds = registryKind(head.accountId) === 'api_key' || (known && !spent(head.windows));
  // A move prefers a cool destination: one whose leads over pace sit under the band and that no round shed from
  // within the dwell. One that is not cool is taken only where no sibling is, which this rule leaves undecided.
  const headAccount = ACCOUNTS.find((a) => a.id === head.accountId);
  const end = headAccount === undefined ? null : endOf(headAccount, at);
  const band = pool.shedAheadOfPace === null ? null : pool.shedAheadOfPace - 0.05;
  const under = band === null || head.windows.every((w) => (w.id !== '5h' && w.id !== '7d') || w.utilization === null || aheadOf({ resetsAt: w.resetsAt, utilization: w.utilization }, w.id === '5h' ? 5 * HOUR : 7 * DAY, at, end) < band);
  const rested = !(head.accountId === SHED_ROUND.accountId && at >= SHED_ROUND.at && at - SHED_ROUND.at < pool.sourceDwellSeconds * 1_000);
  if (!under || !rested) return null;
  const toPlan = registryKind(head.accountId) === 'oauth' && !inOverage(head.accountId) && holds;
  if (!fast && pool.migrateWhenSpent && spent(oauthWindows(from, at, () => SERVED_FACTOR))) return holds && !(pool.overagePolicy === 'prefer_plan' && inOverage(head.accountId)) ? 'spent' : null;
  if (!fast && pool.overagePolicy === 'prefer_plan' && registryKind(from) === 'oauth' && inOverage(from)) return toPlan ? 'overage' : null;
  if (registryKind(from) === 'api_key') return toPlan && at - pinned.createdAt >= pool.sourceDwellSeconds * 1_000 ? 'return_to_plan' : null;
  return null;
}

/** The move reasons the stream draws for a single-attempt row. The draw is kept so every later row keeps its own. */
const MIGRATION_REASONS: { value: PinMigrationReason; weight: number }[] = [
  { value: 'drain', weight: 1 },
  { value: 'fast_lane', weight: 1 },
  { value: 'overage', weight: 2 },
  { value: 'return_to_plan', weight: 1 },
  { value: 'shed', weight: 3 },
  { value: 'spent', weight: 3 },
];

function buildRequestEvents(): Map<string, RequestEvent[]> {
  const h = createRng(8_080);
  const out = new Map<string, RequestEvent[]>();
  // The first eligible row skips the draws below, as it did when it carried the drawer's spent move, so every later
  // row keeps its draws.
  let spentOwed = true;
  let releaseOwed = true;
  for (const r of REQUESTS) {
    if (r.sessionId === null || r.kind === 'probe' || STORY_IDS.has(r.id)) continue;
    const events: RequestEvent[] = [];
    // The pin the event belongs to: the drawer shows a move only where these match the request's own.
    const pin = { keyId: r.keyId, model: r.canonicalModel, sessionId: r.sessionId };
    // A failover moves the pin off the account whose answer set a cooldown. The event lands as the next attempt
    // starts. A token count writes no pin for one to move.
    for (let i = 1; i < r.attempts.length && r.kind !== 'count_tokens'; i += 1) {
      const from = r.attempts[i - 1];
      if (!failsOver(from)) continue;
      events.push({ ...pin, accountId: r.attempts[i].accountId, at: r.attempts[i].startedAt, detail: { agent_id: r.agentId, epoch: i + 1, from_account: from.accountId, reason: 'cooldown' }, kind: 'pin_migrated' });
    }
    // An account's refusal of the model lands as its head does. The rejection story writes its own below.
    for (const a of r.id === REJECTION_STORY_ID ? [] : r.attempts) {
      if (a.cooldownSet?.reason !== 'model_rejected') continue;
      events.push({ ...pin, accountId: a.accountId, at: iso(Date.parse(a.startedAt) + (a.durationMs ?? 0)), detail: { attempt: a.index + 1, request_id: a.upstreamRequestId, source_request_id: r.id, wire_model: r.model }, kind: 'model_rejected' });
    }
    // A few single-attempt rows moved on selection instead: the migrate rule read a sample, not a 429. The reason is
    // drawn and set aside so the stream stays where it was. The move stands where a rule takes the pin there.
    const lane = LANES.find((l) => l.keyId === r.keyId);
    // One draw either way for this seed only. The chance the forced row skips comes up false here, and a seed where it
    // came up true would spend a weighted draw as well and shift every later row.
    if (events.length === 0 && r.status === 200 && lane && lane.accountIds.length > 1 && (spentOwed || h.chance(0.07))) {
      const from = h.pick(lane.accountIds.filter((id) => id !== r.accountId));
      if (!spentOwed) h.weighted(MIGRATION_REASONS);
      const reason = supportedRule(r, from);
      if (reason) events.push({ ...pin, accountId: r.accountId, at: r.at, detail: { agent_id: r.agentId, epoch: 2, from_account: from, reason }, kind: 'pin_migrated' });
      spentOwed = false;
    }
    // One release for the drawer and the smoke checks: a pin that expired on a cooled account whose window was
    // spent, with no sibling holding allowance to move to.
    if (events.length === 0 && releaseOwed && r.status === 200 && lane && lane.accountIds.length > 1) {
      events.push({ ...pin, accountId: r.accountId, at: r.at, detail: { agent_id: r.agentId, reasons: ['cooldown', 'spent'] }, kind: 'pin_expired' });
      releaseOwed = false;
    }
    if (events.length) out.set(r.id, events);
  }
  // The rejection story's two events, as the assembler writes them: the refusal when its head lands, the permit
  // when the request completes. Both carry the request's own id, which is how the drawer tells them from another
  // request's in the same window.
  const story = requestById.get(REJECTION_STORY_ID);
  if (story) {
    const refused = story.attempts[0];
    const served = story.attempts[story.attempts.length - 1];
    const pin = { keyId: story.keyId, model: story.canonicalModel, sessionId: story.sessionId };
    const detail = { request_id: null, source_request_id: story.id, wire_model: story.model };
    const events: RequestEvent[] = [
      ...(out.get(story.id) ?? []),
      { ...pin, accountId: refused.accountId, at: iso(Date.parse(refused.startedAt) + (refused.durationMs ?? 0)), detail: { attempt: 1, ...detail }, kind: 'model_rejected' },
      { ...pin, accountId: served.accountId, at: iso(Date.parse(story.at) + (story.durationMs ?? 0)), detail: { attempt: story.attempts.length, rejected_by: [refused.accountId], ...detail }, kind: 'model_permitted' },
    ];
    out.set(story.id, events.sort((a, b) => Date.parse(a.at) - Date.parse(b.at)));
  }
  for (const [id, events] of storyEvents()) out.set(id, events);
  return out;
}

/**
 * The story requests' pin moves and model answers, as the routing log's records name them: a failover moves the pin
 * when the next attempt starts, a rule's move lands with the decision, and a refused model's permit when the request
 * completes.
 */
function storyEvents(): Map<string, RequestEvent[]> {
  const out = new Map<string, RequestEvent[]>();
  const moved = (id: string, at: number, from: string, to: string, reason: PinMigrationReason) => {
    const r = requestById.get(id);
    if (!r) return;
    const event: RequestEvent = { accountId: to, at: iso(at), detail: { agent_id: r.agentId, epoch: 0, from_account: from, reason }, keyId: r.keyId, kind: 'pin_migrated', model: r.canonicalModel, sessionId: r.sessionId };
    out.set(id, [...(out.get(id) ?? []), event]);
  };
  moved('req_4b0e9d17-c31', clock(14, 27, 1), 'primary', 'nightly', 'fast_lane');
  moved('req_2e7c90a1-6d4', clock(14, 24, 10), 'primary', 'nightly', 'cooldown');
  moved('req_da52d141-a5a', clock(14, 5, 12), 'secondary', 'primary', 'spent');
  moved('req_c41d0311-200', clock(14, 6, 30), 'secondary', 'primary', 'spent');
  moved('req_1d9c3e55-0b2', clock(14, 2, 11), 'secondary', 'primary', 'cooldown');
  moved('req_868264bc-611', clock(11, 8, 30), 'secondary', 'primary', 'shed');
  const rejected = requestById.get('req_2e7c90a1-6d4');
  if (rejected) {
    const pin = { keyId: rejected.keyId, model: rejected.canonicalModel, sessionId: rejected.sessionId };
    const detail = { source_request_id: rejected.id, wire_model: 'claude-haiku-4-5' };
    out.get(rejected.id)?.push(
      { ...pin, accountId: 'primary', at: iso(clock(14, 24, 10)), detail: { attempt: 1, request_id: rejected.attempts[0].upstreamRequestId, ...detail }, kind: 'model_rejected' },
      { ...pin, accountId: 'nightly', at: iso(clock(14, 24, 12)), detail: { attempt: 2, rejected_by: ['primary'], request_id: rejected.attempts[1].upstreamRequestId, ...detail }, kind: 'model_permitted' },
    );
  }
  return out;
}

/**
 * The events the mock's request detail answers, by request id. A failover writes one, and a few rows carry a spent or
 * a hot window.
 */
export const REQUEST_EVENTS: Map<string, RequestEvent[]> = buildRequestEvents();

/**
 * The obstacles a fresh placement's passed-over members are drawn from. Being ranked behind another is the usual one.
 */
const OBSTACLE_DRAWS: { value: string; weight: number }[] = [
  { value: 'above_ceiling', weight: 2 },
  { value: 'at_cap', weight: 1 },
  { value: 'cooldown', weight: 2 },
  { value: 'floor', weight: 1 },
  { value: 'operator', weight: 1 },
  { value: 'overage', weight: 1 },
  { value: 'ranked', weight: 10 },
  { value: 'spent', weight: 1 },
  { value: 'subscription_ended', weight: 1 },
  { value: 'winding_down', weight: 1 },
];

const pinOf = (r: RequestLog) => `${r.keyId}|${r.sessionId}|${r.canonicalModel}`;

/**
 * A share of the fresh placements land where a prefix is warm: a main conversation by its messages or its static
 * prefix, a subagent beside the main conversation as well. The promised size sits near what the answer read, so
 * some placements by messages read all they were promised and some fall short.
 */
function placementOf(p: Rng, r: RequestLog): Placement | null {
  if (r.kind !== 'inference' || !p.chance(0.4)) return null;
  const rung = p.weighted<Placement['rung']>(
    r.agentId === 'main'
      ? [{ value: 'content', weight: 3 }, { value: 'static', weight: 1 }]
      : [{ value: 'content', weight: 2 }, { value: 'static', weight: 2 }, { value: 'with_main', weight: 1 }],
  );
  if (rung === 'with_main') return { depth: 0, prefixTokens: 0, rung };
  const read = r.usage.cacheRead > 0 ? r.usage.cacheRead : p.int(20_000, 140_000);
  return { depth: rung === 'content' ? p.int(2, 9) : p.int(1, 3), prefixTokens: Math.round(read * p.float(0.85, 1.12)), rung };
}

/**
 * Half the subagents placed afresh first waited on their cohort's account, every account at its cap. The wait
 * draws from a stream of its own, so every placement drawn after a hold keeps its values.
 */
const cohortHoldOf = (p: Rng, w: Rng, r: RequestLog, placement: Placement | null, accountId: string): CohortHold | null =>
  placement !== null && r.agentId !== 'main' && p.chance(0.5) ? { cohort: placement.rung === 'with_main' ? null : accountId, heldMs: w.int(180, 4_000), waitedOn: [accountId] } : null;

/**
 * The drawer's shed move and the two newer release reasons, each on a pin no other event touches, so the checks
 * that look for an unmoved request beside a moved one still find one.
 */
function addStoryEvents() {
  const busy = new Set([...REQUEST_EVENTS.keys(), ...STORY_IDS].flatMap((id) => { const r = requestById.get(id); return r ? [pinOf(r)] : []; }));
  const owed: ((r: RequestLog, from: string) => RequestEvent)[] = [];
  const pinned = (r: RequestLog) => ({ keyId: r.keyId, model: r.canonicalModel, sessionId: r.sessionId });
  const shedMoved = [...REQUEST_EVENTS.values()].some((events) => events.some((e) => e.detail?.reason === 'shed'));
  if (!shedMoved) owed.push((r, from) => ({ ...pinned(r), accountId: r.accountId, at: r.at, detail: { agent_id: r.agentId, epoch: 2, from_account: from, reason: 'shed' }, kind: 'pin_migrated' }));
  owed.push((r) => ({ ...pinned(r), accountId: r.accountId, at: r.at, detail: { agent_id: r.agentId, reasons: ['client_error'] }, kind: 'pin_expired' }));
  owed.push((r) => ({ ...pinned(r), accountId: r.accountId, at: r.at, detail: { agent_id: r.agentId, reasons: ['subscription_ended'] }, kind: 'pin_expired' }));
  for (const r of REQUESTS) {
    const lane = LANES.find((l) => l.keyId === r.keyId);
    const next = owed[0];
    if (!next) break;
    const from = lane?.accountIds.find((id) => id !== r.accountId);
    if (r.sessionId === null || r.accountId === null || r.kind === 'probe' || r.status !== 200 || r.attempts.length !== 1 || from === undefined || busy.has(pinOf(r)) || STORY_IDS.has(r.id)) continue;
    REQUEST_EVENTS.set(r.id, [next(r, from)]);
    busy.add(pinOf(r));
    owed.shift();
  }
}

/**
 * Every request its class and every attempt its trace, on a stream of their own so each row and event above keeps
 * its draws. A move the events record is the trace's `moved`; a first attempt no event moved either renewed its pin
 * or placed the session afresh, and a fresh placement names every member it passed over.
 */
function traceRequests() {
  const h = createRng(2_909);
  // The placements draw from a stream of their own, so every trace drawn before them keeps its values.
  const p = createRng(4_111);
  const w = createRng(3_119);
  const accountOf = new Map(ACCOUNTS.map((a) => [a.id, a]));
  for (const r of REQUESTS) {
    // The routing log's records write the story's traces, so no obstacle is drawn against what its cards show.
    if (STORY_IDS.has(r.id)) continue;
    const pool = poolById.get(r.poolId ?? '');
    const members = LANES.find((l) => l.keyId === r.keyId)?.accountIds ?? [];
    const events = REQUEST_EVENTS.get(r.id) ?? [];
    const drawnClass = r.compaction ? 'compaction' : r.agentId !== 'main' && r.kind !== 'probe' ? 'subagent' : h.weighted([{ value: 'auxiliary', weight: 3 }, { value: 'main', weight: 16 }, { value: 'workflow', weight: 1 }]);
    r.requestClass = drawnClass;
    r.attempts = r.attempts.map((a, i, all): Attempt => {
      const moved = events.find((e) => e.kind === 'pin_migrated' && e.at === a.startedAt && e.accountId === a.accountId)?.detail?.reason;
      // The ledger folds the cache read into the trace of the attempt that answered, and only once it did.
      const cacheRead = i === all.length - 1 && r.status === 200 ? r.usage.cacheRead : null;
      const strategy = pool?.strategy ?? 'pace';
      const account = accountOf.get(a.accountId);
      const overage = account?.overage?.inUse === true ? 1 : 0;
      // The kind order walks the plan-funded subscriptions, the ones in overage, then the API keys.
      const group = pool?.preferSubscription ? (account?.kind === 'api_key' ? 2 : overage) : 0;
      const priority = pool?.members.find((m) => m.accountId === a.accountId)?.priority ?? 0;
      // A request renews the pin its first selection finds live on the account it went to, and only then. The renewal
      // draw is taken either way, so the stream stays where it was, and a drawn renewal with no live pin to renew is
      // placed afresh on no further draw.
      const first = i === 0 && moved === undefined;
      const drawnRenewal = first && h.chance(0.55);
      if (first && (pool?.affinityTtlSeconds ?? 0) > 0 && LIVE_PIN.get(r.id)?.accountId === a.accountId) {
        return { ...a, selection: { cacheRead, cohortHold: null, moved: null, placement: null, renewed: true, skipped: [], strategy, tier: null } };
      }
      if (drawnRenewal) return { ...a, selection: { cacheRead, cohortHold: null, moved: null, placement: null, renewed: false, skipped: [], strategy, tier: [group, priority, overage] } };
      const tried = new Set(all.slice(0, i).map((x) => x.accountId));
      const skipped = members.filter((id) => id !== a.accountId).map((accountId) => ({ accountId, obstacle: tried.has(accountId) ? 'attempted' : h.weighted(OBSTACLE_DRAWS) }));
      const placement = moved === undefined ? placementOf(p, r) : null;
      // Every placement draws its chance of a hold so the streams stay where they were. A request holds before its
      // first attempt alone, and only where it could have waited.
      const drawnHold = cohortHoldOf(p, w, r, placement, a.accountId);
      const cohortHold = drawnHold !== null && i === 0 && holdFirst(r, drawnHold) ? drawnHold : null;
      if (placement !== null) {
        // A placement runs no ranking, so it names no tier and passes nobody over as ranked.
        const passed = skipped.filter((s) => s.obstacle !== 'ranked');
        return { ...a, selection: { cacheRead, cohortHold, moved: null, placement, renewed: false, skipped: passed, strategy, tier: null } };
      }
      const traced = { cacheRead, cohortHold, moved: typeof moved === 'string' ? moved : null, placement, renewed: false, skipped, strategy };
      return { ...a, selection: { ...traced, tier: [group, priority, overage] } };
    });
  }
}

/**
 * One Claude Code session's first request branched from another seeded session.
 */
function identifyRequests() {
  for (const r of REQUESTS) {
    if (STORY_IDS.has(r.id)) continue;
    Object.assign(r, { forkDepth: null, forkedFrom: null });
  }
  // A session's oldest row is its first request, and the rows run newest first, so the last one set per session
  // stays. A fork is a turn the log lists, never a probe, and its base another session the story seeds. Both forks
  // are pinned by identity, so neither moves with the load instant and the two never land on one row.
  const oldest = new Map<string, RequestLog>();
  for (const r of REQUESTS) if (r.sessionId !== null && r.keyId !== 'ci' && !STORY_IDS.has(r.id)) oldest.set(r.sessionId, r);
  const turnOf = (sessionId: string) => {
    const r = oldest.get(sessionId);
    return r?.kind === 'inference' ? r : undefined;
  };
  // A fork on another account than its base's, so the Sessions table reads "on another account".
  const first = turnOf(SESSION_IDS[2]);
  if (first) Object.assign(first, { forkDepth: 3, forkedFrom: SESSION_IDS[1] });
  // A second fork on its base's own account, so the Sessions table reads both relations.
  const second = turnOf(SESSION_IDS[5]);
  if (second) Object.assign(second, { forkDepth: 2, forkedFrom: SESSION_IDS[3] });
  // A fork of an unpinned session, so the Sessions table reads one that holds no pin. The draw skips by the local hour,
  // so either of two such sessions can open on a token count: the other takes the fork then, its base a session of its
  // own key.
  const pinless = [{ base: SESSION_IDS[7], r: turnOf(SESSION_IDS[12]) }, { base: SESSION_IDS[1], r: turnOf(SESSION_IDS[13]) }].find((f) => f.r !== undefined);
  if (pinless?.r) Object.assign(pinless.r, { forkDepth: 2, forkedFrom: pinless.base });
}

/**
 * The drawer lines the draws above leave out, each on an answered single-attempt inference: a placement beside the
 * move that led to it, a placement with the main conversation and no hold, and a hold on the strategy's choice,
 * which names no cohort. Nothing is drawn, so every seeded stream keeps its values.
 */
function seedDrawerBranches() {
  const taken = new Set<string>();
  const seed = (want: (s: SelectionTrace, r: RequestLog) => boolean, trace: (s: SelectionTrace, r: RequestLog) => SelectionTrace) => {
    const r = REQUESTS.find((x) => !taken.has(x.id) && x.kind === 'inference' && x.status === 200 && x.attempts.length === 1 && x.attempts[0].selection !== null && want(x.attempts[0].selection, x));
    if (!r) return;
    taken.add(r.id);
    r.attempts = [{ ...r.attempts[0], selection: trace(r.attempts[0].selection as SelectionTrace, r) }];
  };
  // A placement ranks nothing, so it names no tier and passes nobody over as ranked.
  const placed = (s: SelectionTrace, placement: Placement): SelectionTrace => ({ ...s, placement, skipped: s.skipped.filter((x) => x.obstacle !== 'ranked'), tier: null });
  seed(
    (s) => s.moved === 'spent' && s.placement === null,
    (s) => placed(s, { depth: 2, prefixTokens: s.cacheRead !== null && s.cacheRead > 0 ? Math.round(s.cacheRead * 0.9) : 30_000, rung: 'static' }),
  );
  seed(
    (s, r) => r.agentId !== 'main' && s.moved === null && s.placement === null && s.cohortHold === null && (s.cacheRead ?? 0) > 0 && !REQUEST_EVENTS.has(r.id),
    (s) => ({ ...placed(s, { depth: 0, prefixTokens: 0, rung: 'with_main' }), renewed: false }),
  );
  const strategyHold = (r: RequestLog): CohortHold => ({ cohort: null, heldMs: 1250, waitedOn: [r.attempts[0].accountId] });
  seed(
    (s, r) => s.moved === null && s.placement === null && !s.renewed && s.cohortHold === null && s.skipped.length > 0 && !REQUEST_EVENTS.has(r.id) && holdFirst(r, strategyHold(r)),
    (s, r) => ({ ...s, cohortHold: strategyHold(r) }),
  );
}

/**
 * The hold a request waited through is its own fact, as the router writes it at completion: the one its serving
 * attempt followed, and on the newest exhausted desktop turn a hold that ran its pool's bound out with every
 * account still at its cap, answered without a dispatch, so the log shows a hold no attempt names. One more failed
 * turn failed over from its attempts and then waited on an account of its lane it had not tried until the bound ran
 * out, so no attempt's trace names the whole wait. Nothing is drawn, so every seeded stream keeps its values.
 */
function seedHolds() {
  for (const r of REQUESTS) if (!STORY_IDS.has(r.id)) r.held = [...r.attempts].reverse().find((a) => a.selection?.cohortHold)?.selection?.cohortHold ?? null;
  const boundOf = (r: RequestLog) => (poolById.get(r.poolId ?? '')?.retryInPlaceSeconds ?? 60) * 1_000;
  // The attempts the draws made, before any a 429 added for the members it left untried.
  const drawnPart = (r: RequestLog) => r.attempts.slice(0, CHAIN_FROM.get(r.id) ?? r.attempts.length);
  // An attempt that never ran takes the failover that led to it along.
  const keepAttempts = (r: RequestLog, kept: number) => {
    const gone = new Set(r.attempts.slice(kept).map((a) => a.startedAt));
    r.attempts = r.attempts.slice(0, kept);
    const events = (REQUEST_EVENTS.get(r.id) ?? []).filter((e) => !(e.kind === 'pin_migrated' && gone.has(e.at)));
    if (events.length) REQUEST_EVENTS.set(r.id, events);
    else REQUEST_EVENTS.delete(r.id);
  };
  const failoversOnly = (r: RequestLog) => (REQUEST_EVENTS.get(r.id) ?? []).every((e) => e.kind === 'pin_migrated' && e.detail?.reason === 'cooldown');
  const atCap = REQUESTS.find((r) => r.keyId === 'desktop' && r.kind === 'inference' && r.status === 429 && failoversOnly(r) && !STORY_IDS.has(r.id));
  // A hold waits on an account that could serve the request then, at its cap.
  const servableFor = (r: RequestLog, at: number) => (id: string) => holdableAt(r, id, at);
  if (atCap) {
    const first = LANES.find((l) => l.keyId === atCap.keyId)?.accountIds.find(servableFor(atCap, Date.parse(atCap.at)));
    const waitedOn = first === undefined ? [] : [first];
    const heldMs = boundOf(atCap);
    keepAttempts(atCap, 0);
    Object.assign(atCap, { accountId: null, costUsd: 0, durationMs: heldMs, errorCode: 'accounts_at_cap', held: { cohort: null, heldMs, waitedOn } });
  }
  const heldAt = (r: RequestLog) => {
    const drawnAttempts = drawnPart(r);
    const last = drawnAttempts[drawnAttempts.length - 1] as Attempt | undefined;
    return last ? Date.parse(last.startedAt) + (last.durationMs ?? 0) : Date.parse(r.at);
  };
  // The reselect after the draws' last 429 meets the members left untried, the first of which it waits on.
  const untried = (r: RequestLog) => {
    const tried = drawnPart(r).map((a) => a.accountId);
    const members = (poolById.get(r.poolId ?? '')?.members ?? []).map((m) => m.accountId).sort((x, y) => Number(x === UNPRICED_ACCOUNT) - Number(y === UNPRICED_ACCOUNT));
    return members.find((id) => !tried.includes(id) && serves(id, r.canonicalModel) && servableFor(r, heldAt(r))(id));
  };
  // A 429 fails the attempt over to a reselect, which can find every account at its cap. A 529 or a 502 is committed.
  const failedTurn = (r: RequestLog) => r !== atCap && !STORY_IDS.has(r.id) && r.kind === 'inference' && r.status === 429 && drawnPart(r).slice(-1)[0]?.upstreamStatus === 429 && untried(r) !== undefined;
  // The draw skips by the local hour, so at some clocks no turn was exhausted after one attempt. The oldest turn
  // that failed over then takes the story, which keeps it at every clock.
  const oldest = [...REQUESTS].reverse();
  const afterAttempt = oldest.find((r) => failedTurn(r) && drawnPart(r).length === 1) ?? oldest.find(failedTurn);
  const next = afterAttempt && untried(afterAttempt);
  if (!afterAttempt || !next) return;
  keepAttempts(afterAttempt, drawnPart(afterAttempt).length);
  // The router's one builder sums every hold into the row, so the row keeps the hold its last attempt's trace
  // already named, whichever turn the clock drew, and adds the bound's wait.
  const before = afterAttempt.held;
  const bound = boundOf(afterAttempt);
  const lastKept = afterAttempt.attempts[afterAttempt.attempts.length - 1];
  Object.assign(afterAttempt, {
    accountId: null,
    durationMs: Date.parse(lastKept.startedAt) + (lastKept.durationMs ?? 0) - Date.parse(afterAttempt.at) + bound,
    errorCode: 'accounts_at_cap',
    held: { cohort: null, heldMs: (before?.heldMs ?? 0) + bound, waitedOn: [...(before?.waitedOn ?? []), next] },
    outcome: 'exhausted',
    status: 429,
  });
}

/**
 * A row's shares of its account's windows, as the request route answers them: null for a request no subscription
 * window paid, else each window the model applies to under the account's tier at the request's completion, a window
 * the cost model cannot price carried as null and an account without a tier an empty map.
 */
function costSharesOf(r: RequestLog): CostShares | null {
  const serving = r.attempts[r.attempts.length - 1] ?? null;
  if (serving === null || fundingOf(serving) !== 'plan') return null;
  const tier = accountById.get(serving.accountId)?.subscription?.rateLimitTier;
  if (tier === undefined) return {};
  const finishedMs = Date.parse(r.at) + (r.durationMs ?? 0);
  const windows = ['five_hour', 'seven_day', ...modelWeeks(TABLES, r.canonicalModel)];
  return Object.fromEntries(windows.map((w) => [w, shareOf(TABLES, w, tier, r.canonicalModel, r.usage, finishedMs)]));
}

/**
 * Every row's phases and shares, read off the row as it finally stands: the passes above move an arrival, drop
 * attempts and set nightly's overage flag, so the instants and the funding are read after the last of them. The
 * write lands a few milliseconds into the last attempt, as the route reads it, and the stream's close a few
 * milliseconds before the finish. A phase the row never reached stays null: no head without a serving attempt, no
 * content or close on a stream that failed past its head, no close on one that ended short or was canceled, no
 * content or close on a token count, and no close on a JSON answer.
 */
function phaseRequests() {
  for (const r of REQUESTS) {
    const atMs = Date.parse(r.at);
    const first = r.attempts[0] as Attempt | undefined;
    const last = r.attempts[r.attempts.length - 1] as Attempt | undefined;
    const write = phaseRng.int(8, 40);
    const tail = phaseRng.int(5, STREAM_TAIL);
    // A stream that failed past its head errored before any content token reached the client.
    if (r.status === 200 && r.outcome === 'failed') r.ttftMs = null;
    const finishedMs = r.durationMs === null ? null : atMs + r.durationMs;
    const headMs = r.ttfbMs === null ? null : atMs + r.ttfbMs;
    const contentMs = headMs === null || r.ttftMs === null || r.kind === 'count_tokens' ? null : atMs + r.ttftMs;
    const closes = contentMs !== null && finishedMs !== null && r.transport === 'sse' && r.outcome !== 'incomplete' && r.outcome !== 'canceled';
    r.phases = {
      finishedAt: finishedMs === null ? null : iso(finishedMs),
      firstContentAt: contentMs === null ? null : iso(contentMs),
      headAt: headMs === null ? null : iso(headMs),
      receivedAt: r.at,
      selectedAt: first === undefined ? null : first.startedAt,
      streamEndAt: closes ? iso(finishedMs - tail) : null,
      writtenAt: last === undefined ? null : iso(Date.parse(last.startedAt) + write),
    };
    r.costShares = costSharesOf(r);
  }
}

addStoryEvents();
traceRequests();
seedDrawerBranches();
seedHolds();
liftCooldowns(REQUESTS);
identifyRequests();
phaseRequests();

const outputTps = (r: RequestLog): number | null => {
  if (r.status !== 200 || r.kind !== 'inference' || r.ttftMs === null) return null;
  const streaming = Math.max(50, (r.durationMs ?? 0) - r.ttftMs);
  return (r.usage.output / streaming) * 1000;
};

function histogramsOf(rows: RequestLog[]): LatencyHistograms {
  const okRows = rows.filter((r) => r.status === 200);
  const pick = (f: (r: RequestLog) => number | null) => histogramOf(okRows.map(f).filter((v): v is number => v !== null && v > 0));
  return {
    durationMs: pick((r) => (r.kind === 'count_tokens' ? null : r.durationMs)),
    outputTps: pick(outputTps),
    ttfbMs: pick((r) => r.ttfbMs),
    ttftMs: pick((r) => r.ttftMs),
  };
}

/**
 * Probes an hour paid for that no real turn ever used. Idle limits dominate on
 * purpose: that is the share the analytics panel tells the operator to act on.
 */
const WASTE_SPLIT: { reason: WastedProbeReason; weight: number }[] = [
  { reason: 'cold_return', weight: 1 },
  { reason: 'evicted', weight: 0.5 },
  { reason: 'expired', weight: 0.5 },
  { reason: 'idle_limit', weight: 13 },
  { reason: 'released', weight: 1 },
  { reason: 'replaced', weight: 2 },
  { reason: 'session_ended', weight: 4 },
];

function wastedProbes(h: Rng, probes: number, probeCostUsd: number): Pick<HourlyRollup, 'wastedProbeReasons' | 'wastedProbeUsd' | 'wastedProbes'> {
  const wastedProbeReasons = emptyWastedProbeReasons();
  const wasted = probes === 0 ? 0 : h.chance(0.6) ? Math.min(probes, h.int(1, Math.max(1, Math.round(probes * 0.45)))) : 0;
  for (let i = 0; i < wasted; i += 1) wastedProbeReasons[h.weighted(WASTE_SPLIT.map((w) => ({ value: w.reason, weight: w.weight })))] += 1;
  // A wasted probe cost what an average probe of that hour cost.
  return { wastedProbeReasons, wastedProbeUsd: probes === 0 ? 0 : (probeCostUsd / probes) * wasted, wastedProbes: wasted };
}

const ZERO_WASTE: Pick<HourlyRollup, 'wastedProbeReasons' | 'wastedProbeUsd' | 'wastedProbes'> = { wastedProbeReasons: emptyWastedProbeReasons(), wastedProbeUsd: 0, wastedProbes: 0 };

/** The classes a synthesized hour's traffic sent, weighted toward the main conversation. */
const HOUR_CLASSES: [string, number][] = [['auxiliary', 2], ['compaction', 1], ['main', 12], ['subagent', 5], ['workflow', 1]];

/** A class picked from the row's own identity rather than drawn, so no seeded stream moves. */
function classOfHour(seed: string): string {
  let hash = 2_166_136_261;
  for (let i = 0; i < seed.length; i += 1) hash = Math.imul(hash ^ seed.charCodeAt(i), 16_777_619) >>> 0;
  let roll = hash % HOUR_CLASSES.reduce((n, [, w]) => n + w, 0);
  for (const [name, weight] of HOUR_CLASSES) {
    if (roll < weight) return name;
    roll -= weight;
  }
  return 'main';
}

/**
 * The first hour the dashboard draws: 24 hourly buckets, the current hour last. An account's or a pool's day total,
 * and a pool's 25 hourly buckets, start an hour earlier, on the hour a full day back, and the week on the hour seven
 * days back, as the routes total their rollups.
 */
export const DAY_FROM = startOfHour(NOW) - 23 * HOUR;
const TOTALS_FROM = startOfHour(NOW) - 24 * HOUR;
const WEEK_FROM = startOfHour(NOW) - 7 * DAY;

// The probes the router held while its ledger caught up draw from a stream of their own, so they shift no other draw.
const heldRng = createRng(6_133);

/**
 * Who paid for a request, as the ledger keys its rollups off the answer that served it: `billed` for an API key
 * account's answer before anything, then a fast answer's credits, then the head's overage flag, else the plan. A
 * request no attempt served files under `plan` whatever speed it asked for.
 */
const fundingKey = (r: RequestLog): HourlyRollup['funding'] => {
  const last = r.attempts[r.attempts.length - 1] as Attempt | undefined;
  if (last?.accountKind === 'api_key') return 'billed';
  if (last?.response.speed === 'fast') return 'fast';
  return last?.overageInUse === true ? 'overage' : 'plan';
};

/**
 * An hour's plan-window shares: each request priced at its own band and completion, then summed, so a request across a
 * band edge reads as its own shares do. A request is priced here rather than its cost shares read, which are null on
 * every request the plan did not fund. A window any request prices null reads null, and a request with no token, or on
 * an account without a tier, prices nothing.
 */
function hourShares(accountId: string, model: string, priced: { at: number; usage: Usage }[]): Record<string, number | null> {
  const tier = accountById.get(accountId)?.subscription?.rateLimitTier;
  const windows = ['five_hour', 'seven_day', ...modelWeeks(TABLES, model)];
  const out: Record<string, number | null> = Object.fromEntries(windows.map((w) => [w, 0]));
  for (const { at, usage } of priced) {
    if (tier === undefined || tokensTotal(usage) === 0) continue;
    for (const w of windows) {
      const share = shareOf(TABLES, w, tier, model, usage, at);
      const sum = out[w];
      out[w] = share === null || sum === null ? null : sum + share;
    }
  }
  return out;
}

const upstreamFailed = (r: RequestLog) => r.status !== 200 && r.status !== 429 && r.status !== 529;

export function rollups(rows: RequestLog[]): HourlyRollup[] {
  const h = createRng(4_242);
  const groups = new Map<string, RequestLog[]>();
  for (const r of rows) {
    const hour = iso(startOfHour(Date.parse(r.at)));
    // A request answered at the cap before any account served it files under `-`, as the ledger files what it could
    // not attribute. The funding is part of the key, so a lane-hour on both sides of nightly's overage raise splits.
    const key = [hour, r.keyId, r.poolId, r.accountId ?? '-', r.model, r.kind, r.sessionId, fundingKey(r)].join('|');
    const list = groups.get(key) ?? [];
    list.push(r);
    groups.set(key, list);
  }
  const out: HourlyRollup[] = [];
  for (const [key, list] of groups) {
    const [hour, keyId, poolId, accountId, model, kind, sessionId, funding] = key.split('|') as [string, string, string, string, string, RequestKind, string, HourlyRollup['funding']];
    const okRows = list.filter((r) => r.status === 200);
    const isProbe = kind === 'probe';
    // The seeded draws are the lane hour's, taken once each in this order, so splitting its rows by effort and class
    // below moves no drawn figure.
    // Roughly a third of warm reads on the desktop and laptop keys follow a probe
    // closely enough to credit the probe with the hit.
    const kept = new Set(isProbe || !warmsCaches(keyId, poolId) ? [] : okRows.filter((r) => r.usage.cacheRead > 0 && h.chance(0.35)));
    const moved = list.filter((r) => r.attempts.length > 1 && r.status === 200);
    const migrations = moved.length > 0 && h.chance(0.5) ? 1 : 0;
    const probeCostUsd = isProbe ? list.reduce((s, r) => s + r.costUsd, 0) : 0;
    const probes = isProbe ? { ok: okRows.length, failed: list.length - okRows.length, held: 0, skipped: h.int(0, 2) } : null;
    const probeMigrated = isProbe && h.chance(0.35) ? Math.min(okRows.length, h.int(1, 2)) : 0;
    const waste = isProbe ? wastedProbes(h, okRows.length, probeCostUsd) : ZERO_WASTE;
    // A lane's rows share its key, model and kind, which is what decides fast mode in this story; a row refused
    // before any attempt files under the speed it asked for, as the ledger files the speed key alone.
    const speed = (list[0].attempts[list[0].attempts.length - 1]?.response.speed ?? list[0].shape.requestedSpeed) === 'fast' ? 'fast' : 'standard';
    // The rollups file each request under its effort and class as they fold them, and a probe under no class. The
    // story files a probe under no effort either, so a lane hour's probes stay one row with the probe figures on it.
    const parts = new Map<string, RequestLog[]>();
    for (const r of list) {
      const part = isProbe ? `${NO_EFFORT}|${NO_CLASS}` : `${foldEffort(r.shape.effort)}|${foldRequestClass(r.requestClass)}`;
      parts.set(part, [...(parts.get(part) ?? []), r]);
    }
    for (const [part, partRows] of parts) {
      const [effort, requestClass] = part.split('|');
      const ok = partRows.filter((r) => r.status === 200);
      const cacheMissReasons = emptyCacheMissReasons();
      for (const r of partRows) for (const a of r.attempts) if (a.cacheMissReason) cacheMissReasons[a.cacheMissReason] = missCount(cacheMissReasons, a.cacheMissReason) + 1;
      const keptHere = partRows.filter((r) => kept.has(r));
      out.push({
        hour,
        client: CLIENTS[keyId] ?? '-',
        keyId,
        // One hour never holds two holders of a key, since the live key starts on the hour.
        keyGeneration: list[0].keyGeneration,
        kind,
        poolId,
        accountId,
        model,
        requestClass,
        sessionId,
        funding,
        speed,
        requests: {
          // A stream that ended short committed its 200 and still is no success, so it counts on its own.
          incomplete: partRows.filter((r) => r.outcome === 'incomplete').length,
          ok: ok.filter((r) => r.outcome === 'succeeded').length,
          rate_limited: partRows.filter((r) => r.status === 429).length,
          // A stream that failed after its 200 head is a failure, as the ledger files it, under the error it carried.
          overloaded: partRows.filter((r) => r.status === 529 || (r.status === 200 && r.errorCode === 'overloaded_error')).length,
          // A stream the relay could not decode is an upstream error of its own, counted inside `upstream_error` under
          // the same status filter, so the error split that subtracts it never reads negative.
          undecodable: partRows.filter((r) => upstreamFailed(r) && r.errorCode === 'stream_undecodable').length,
          upstream_error: partRows.filter(upstreamFailed).length,
          // The ledger files a stream the client closed as failed, under the code the relay gave it.
          client_gone: partRows.filter((r) => r.errorCode === 'client_gone').length,
        },
        // Every attempt but the last one moved the request to another member.
        failovers: partRows.reduce((s, r) => s + Math.max(0, r.attempts.length - 1), 0),
        // The lane hour draws one migration, filed with the first request that moved.
        migrations: partRows.includes(moved[0]) ? migrations : 0,
        effort,
        tokens: {
          uncachedInput: partRows.reduce((s, r) => s + r.usage.input, 0),
          cacheRead: partRows.reduce((s, r) => s + r.usage.cacheRead, 0),
          cacheWrite5m: partRows.reduce((s, r) => s + r.usage.cacheWrite5m, 0),
          cacheWrite1h: partRows.reduce((s, r) => s + r.usage.cacheWrite1h, 0),
          output: partRows.reduce((s, r) => s + r.usage.output, 0),
          thinking: partRows.reduce((s, r) => s + r.usage.thinking, 0),
          webFetch: partRows.reduce((s, r) => s + r.usage.webFetch, 0),
          webSearch: partRows.reduce((s, r) => s + r.usage.webSearch, 0),
        },
        cacheHits: ok.filter((r) => r.usage.cacheRead > 0).length,
        cacheMisses: ok.filter((r) => r.usage.cacheRead === 0 && r.usage.cacheWrite5m + r.usage.cacheWrite1h > 0).length,
        cacheMissReasons,
        expiredBetweenTurns: ok.filter((r) => r.usage.cacheRead === 0 && r.attempts[r.attempts.length - 1].cacheMissReason === 'none').length,
        uncached: ok.filter((r) => r.usage.cacheRead === 0 && r.usage.cacheWrite5m + r.usage.cacheWrite1h === 0).length,
        costUsd: partRows.reduce((s, r) => s + r.costUsd, 0),
        cost: sumSplit(partRows.map((r) => costSplitOf(r.model, r.usage, r.attempts[r.attempts.length - 1]?.response.speed === 'fast'))),
        // At each answer's completion, the instant its own shares price it at.
        shares: hourShares(accountId, partRows[0].canonicalModel, partRows.map((r) => ({ at: Date.parse(r.at) + (r.durationMs ?? 0), usage: r.usage }))),
        histograms: histogramsOf(partRows),
        keptWarm: { requests: keptHere.length, tokens: keptHere.reduce((s, r) => s + r.usage.cacheRead, 0) },
        probes: probes ?? { ok: 0, failed: 0, held: 0, skipped: 0 },
        probeMigrated,
        probeCostUsd,
        ...waste,
      });
    }
  }
  // The day holds one probe the router held while its ledger caught up, from the default range's first hour on, so the
  // default range draws the slice.
  const probeRows = out.filter((r) => r.kind === 'probe' && Date.parse(r.hour) >= DAY_FROM);
  if (probeRows.length > 0) heldRng.pick(probeRows).probes.held = 1;
  return out.sort((a, b) => Date.parse(a.hour) - Date.parse(b.hour));
}

export const ROLLUPS: HourlyRollup[] = rollups(REQUESTS);

/** Log-normal-ish samples so the synthesized hours have the same tail shape as the logged ones. */
function sampled(h: Rng, n: number, median: number, spread: number): number[] {
  return Array.from({ length: n }, () => median * Math.exp(spread * (h.next() + h.next() + h.next() - 1.5)));
}

// Only the last 24h has row-level logs; older hours are synthesized so the
// 7d and 30d analytics ranges have a shape to draw.
/** Splits an hour's requests across the diagnosed reasons, most of them clean. */
function synthesizeMissReasons(hm: Rng, ok: number, asksForDiagnostics: boolean): { cacheMissReasons: Record<CacheMissReason, number>; expiredBetweenTurns: number } {
  const cacheMissReasons = emptyCacheMissReasons();
  if (!asksForDiagnostics) {
    cacheMissReasons.not_requested = ok;
    return { cacheMissReasons, expiredBetweenTurns: 0 };
  }
  let left = ok;
  const take = (n: number) => {
    const taken = Math.max(0, Math.min(left, n));
    left -= taken;
    return taken;
  };
  cacheMissReasons.messages_changed = take(hm.chance(0.45) ? hm.int(1, 2) : 0);
  cacheMissReasons.system_changed = take(hm.chance(0.25) ? 1 : 0);
  cacheMissReasons.tools_changed = take(hm.chance(0.18) ? 1 : 0);
  cacheMissReasons.model_changed = take(hm.chance(0.12) ? 1 : 0);
  cacheMissReasons.previous_message_not_found = take(hm.chance(0.2) ? 1 : 0);
  cacheMissReasons.unavailable = take(hm.chance(0.1) ? 1 : 0);
  cacheMissReasons.none = left;
  return { cacheMissReasons, expiredBetweenTurns: Math.min(cacheMissReasons.none, hm.chance(0.35) ? hm.int(1, 2) : 0) };
}

// The smoke run reads the holder's day through this module.
export { DELETED_HOLDER_DAY } from './time';

function historicalRollups(): HourlyRollup[] {
  const h = createRng(99);
  // A separate stream so the reason split does not shift the rest of the hour's draws.
  const hm = createRng(521);
  // Its own stream as well: the desktop key's Opus hours ran part in fast mode.
  const hf = createRng(7_373);
  // The desktop key asks for high effort throughout; one laptop hour asked for the most, so a grouping on
  // effort has a third group to show. The first eligible hour takes it rather than leaving it to a draw.
  let maxEffortOwed = true;
  // One desktop hour reports a reason the console has no label for, as a later beta may send one. Twelve days
  // back puts it in the month alone, so the day and the week draw the eight known reasons.
  let unknownReasonOwed = true;
  const out: HourlyRollup[] = [];
  const start = startOfHour(NOW) - 30 * DAY;
  const end = startOfHour(NOW) - 24 * HOUR;
  for (let t = start; t < end; t += HOUR) {
    const hourOfDay = new Date(t).getHours();
    const weekend = [0, 6].includes(new Date(t).getDay());
    const daylight = hourOfDay >= 8 && hourOfDay <= 19 ? 1 : 0.15;
    const volume = daylight * (weekend ? 0.35 : 1);
    for (const lane of LANES) {
      // Every desktop hour before the live key's creation belongs to the earlier holder of its id, so every range
      // that reaches past that hour answers the pair of key groups, and no hour carries both generations.
      const earlierHolder = lane.keyId === 'desktop' && t < DELETED_HOLDER_END;
      const generation = earlierHolder ? DELETED_HOLDER_GENERATION : Date.parse(keyById.get(lane.keyId)!.createdAt);
      const laneWeight = lane.keyId === 'desktop' ? 1 : lane.keyId === 'laptop' ? 0.45 : 0.14;
      const n = Math.round(volume * laneWeight * h.float(3, 9));
      if (n === 0) continue;
      const drawn = h.weighted(lane.models);
      const accountId = h.pick(lane.accountIds);
      const model = modelOn(accountId, drawn);
      const sessionId = h.pick(SESSION_IDS);
      const ok = Math.max(0, n - (h.chance(0.2) ? h.int(1, 2) : 0));
      const err = n - ok;
      const big = model.includes('fable') || model.includes('opus');
      const perReq = big ? 95_000 : 40_000;
      const cacheRead = Math.round(ok * perReq * h.float(0.6, 0.85));
      const fresh = Math.round(ok * perReq * h.float(0.12, 0.3));
      const output = ok * h.int(600, 2_800);
      const tokens = {
        uncachedInput: Math.round(fresh * 0.2),
        cacheRead,
        cacheWrite5m: Math.round(fresh * 0.55),
        cacheWrite1h: Math.round(fresh * 0.25),
        output,
        thinking: big ? Math.round(output * h.float(0.2, 0.7)) : 0,
        webFetch: h.chance(0.3) ? h.int(1, 2) : 0,
        webSearch: h.chance(0.5) ? h.int(1, ok + 2) : 0,
      };
      const usage: Usage = { input: tokens.uncachedInput, cacheRead: tokens.cacheRead, cacheWrite5m: tokens.cacheWrite5m, cacheWrite1h: tokens.cacheWrite1h, output: tokens.output, thinking: tokens.thinking, webFetch: tokens.webFetch, webSearch: tokens.webSearch };
      const ttfb = sampled(h, ok, big ? 1_300 : 800, 0.5);
      const ttft = ttfb.map((v) => v + h.int(40, 500));
      const duration = sampled(h, ok, big ? 14_000 : 6_000, 0.6);
      const keptRequests = warmsCaches(lane.keyId, lane.poolId) ? Math.round(ok * h.float(0.2, 0.45)) : 0;
      const keptTokens = Math.round((cacheRead / Math.max(1, ok)) * keptRequests);
      // Six days back, so the hour sits well inside the week and the month the console offers rather than on
      // the edge of the synthesized month, where a range bound would cut it.
      const takesMaxEffort = lane.keyId === 'laptop' && maxEffortOwed && t >= startOfHour(NOW) - 6 * DAY;
      if (takesMaxEffort) maxEffortOwed = false;
      const fast = fastTurn(lane.keyId, model, 'inference') && hf.chance(0.5);
      const inference: HourlyRollup = {
        hour: iso(t),
        client: CLIENTS[lane.keyId] ?? '-',
        effort: lane.keyId === 'desktop' ? 'high' : takesMaxEffort ? 'max' : 'none',
        keyId: lane.keyId,
        keyGeneration: generation,
        kind: 'inference',
        poolId: lane.poolId,
        accountId,
        model,
        requestClass: classOfHour(`${t}|${lane.keyId}|${sessionId}`),
        sessionId,
        funding: kindOf(accountId) === 'api_key' ? 'billed' : fast ? 'fast' : 'plan',
        speed: fast ? 'fast' : 'standard',
        requests: { client_gone: 0, incomplete: 0, ok, rate_limited: err > 0 && h.chance(0.6) ? err : 0, overloaded: err > 0 && h.chance(0.3) ? 1 : 0, undecodable: 0, upstream_error: 0 },
        failovers: err > 0 ? h.int(1, 3) : h.chance(0.12) ? 1 : 0,
        migrations: h.chance(0.08) ? 1 : 0,
        tokens,
        cacheHits: Math.round(ok * h.float(0.6, 0.8)),
        cacheMisses: Math.round(ok * h.float(0.1, 0.25)),
        ...synthesizeMissReasons(hm, ok, lane.keyId !== 'ci'),
        uncached: Math.max(0, ok - Math.round(ok * 0.85)),
        costUsd: costOf(model, usage, fast),
        cost: costSplitOf(model, usage, fast),
        // A synthesized hour holds no request of its own, so its tokens price as one answer at the hour, as its dollars
        // do.
        shares: hourShares(accountId, model, [{ at: t, usage }]),
        histograms: {
          durationMs: histogramOf(duration),
          outputTps: histogramOf(sampled(h, ok, big ? 55 : 80, 0.35)),
          ttfbMs: histogramOf(ttfb),
          ttftMs: histogramOf(ttft),
        },
        keptWarm: { requests: keptRequests, tokens: keptTokens },
        probes: { ok: 0, failed: 0, held: 0, skipped: 0 },
        probeMigrated: 0,
        probeCostUsd: 0,
        ...ZERO_WASTE,
      };
      if (unknownReasonOwed && lane.keyId === 'desktop' && t >= startOfHour(NOW) - 12 * DAY && inference.cacheMissReasons.none > inference.expiredBetweenTurns) {
        unknownReasonOwed = false;
        inference.cacheMissReasons.future_reason = 1;
        inference.cacheMissReasons.none -= 1;
      }
      out.push(inference);
      // Keepalive probes ride on the pinned key; one row per lane-hour keeps
      // the "include probes" toggle meaningful across the longer ranges.
      if (warmsCaches(lane.keyId, lane.poolId) && daylight === 1) {
        const probes = h.int(2, 6);
        const failed = h.chance(0.08) ? 1 : 0;
        const probeUsage: Usage = { input: 0, cacheRead: probes * h.int(30_000, 80_000), cacheWrite5m: 0, cacheWrite1h: 0, output: probes, ...NO_ANSWER_FACTS };
        const probeCost = costOf(model, probeUsage);
        out.push({
          hour: iso(t),
          client: CLIENTS[lane.keyId] ?? '-',
          effort: 'none',
          keyId: lane.keyId,
          keyGeneration: generation,
          kind: 'probe',
          poolId: lane.poolId,
          accountId,
          model,
          requestClass: '-',
          sessionId,
          funding: kindOf(accountId) === 'api_key' ? 'billed' : 'plan',
          speed: 'standard',
          requests: { client_gone: 0, incomplete: 0, ok: probes, rate_limited: 0, overloaded: 0, undecodable: 0, upstream_error: failed },
          failovers: 0,
          migrations: 0,
          tokens: { uncachedInput: 0, cacheRead: probeUsage.cacheRead, cacheWrite5m: 0, cacheWrite1h: 0, output: probes, ...NO_ANSWER_FACTS },
          cacheHits: probes,
          cacheMisses: 0,
          ...synthesizeMissReasons(hm, probes, true),
          uncached: 0,
          costUsd: probeCost,
          cost: costSplitOf(model, probeUsage),
          shares: hourShares(accountId, model, [{ at: t, usage: probeUsage }]),
          histograms: { durationMs: histogramOf(sampled(h, probes, 600, 0.3)), outputTps: emptyHistogram(), ttfbMs: histogramOf(sampled(h, probes, 320, 0.3)), ttftMs: emptyHistogram() },
          keptWarm: { requests: 0, tokens: 0 },
          probes: { ok: probes, failed, held: heldRng.chance(0.04) ? 1 : 0, skipped: h.int(0, 2) },
          probeMigrated: h.chance(0.3) ? 1 : 0,
          probeCostUsd: probeCost,
          ...wastedProbes(h, probes, probeCost),
        });
      }
    }
  }
  return out;
}

export const ALL_ROLLUPS: HourlyRollup[] = [...historicalRollups(), ...ROLLUPS];

const counted = (from: number) => REQUESTS.filter((r) => r.kind !== 'probe' && Date.parse(r.at) >= from);
/** The rows the dashboard counts. A probe is the router's own traffic, which the routes leave out. */
export const DAY_ROWS: RequestLog[] = counted(DAY_FROM);
/** The rows an account's or a pool's day total counts, and a pool's hourly buckets. */
const TOTAL_ROWS: RequestLog[] = counted(TOTALS_FROM);
const COUNTED_ROLLUPS = ALL_ROLLUPS.filter((r) => r.kind !== 'probe');

const sumOf = <T,>(list: T[], read: (x: T) => number) => list.reduce((n, x) => n + read(x), 0);
const requestsIn = (r: HourlyRollup) => r.requests.ok + r.requests.incomplete + r.requests.rate_limited + r.requests.overloaded + r.requests.upstream_error + r.requests.client_gone;
const tokensIn = (r: HourlyRollup) => r.tokens.uncachedInput + r.tokens.cacheRead + r.tokens.cacheWrite5m + r.tokens.cacheWrite1h + r.tokens.output;
const inWeek = (r: HourlyRollup) => Date.parse(r.hour) >= WEEK_FROM;

/**
 * The accounts', keys' and pools' figures, read off the rows and the rollups once both are drawn, since neither
 * `accounts.ts` nor `pools.ts` can import this module. A lifetime reaches past the month the rollups hold, so it is the
 * row's literal, raised to the rollups' sum wherever the literal reads under it.
 */
function deriveFigures() {
  for (const a of ACCOUNTS) {
    const mine = COUNTED_ROLLUPS.filter((r) => r.accountId === a.id);
    Object.assign(a, {
      requests: {
        last24h: TOTAL_ROWS.filter((r) => r.accountId === a.id).length,
        last7d: sumOf(mine.filter(inWeek), requestsIn),
        lifetime: Math.max(a.requests.lifetime, sumOf(mine, requestsIn)),
      },
    });
  }
  for (const k of keyById.values()) {
    // A bare id totals its live key alone, so an earlier holder's rows count for neither figure.
    const mine = COUNTED_ROLLUPS.filter((r) => r.keyId === k.id && r.keyGeneration === Date.parse(k.createdAt));
    const week = mine.filter(inWeek);
    Object.assign(k, {
      lifetime: { cost: Math.max(k.lifetime.cost, sumOf(mine, (r) => r.costUsd)), requests: Math.max(k.lifetime.requests, sumOf(mine, requestsIn)) },
      usage7d: { cost: sumOf(week, (r) => r.costUsd), requests: sumOf(week, requestsIn), tokens: sumOf(week, tokensIn) },
    });
  }
  for (const p of poolById.values()) {
    const mine = TOTAL_ROWS.filter((r) => r.poolId === p.id);
    const traffic24h: TrafficBucket[] = Array.from({ length: 25 }, (_, i) => ({ at: iso(TOTALS_FROM + i * HOUR), costUsd: 0, failed: 0, requests: 0, tokens: 0 }));
    for (const r of mine) {
      const bucket = traffic24h[Math.floor((Date.parse(r.at) - TOTALS_FROM) / HOUR)];
      bucket.costUsd += r.costUsd;
      bucket.failed += r.outcome === 'succeeded' ? 0 : 1;
      bucket.requests += 1;
      bucket.tokens += tokensTotal(r.usage);
    }
    Object.assign(p, { cost24h: sumOf(mine, (r) => r.costUsd), requests24h: mine.length, traffic24h });
  }
}

deriveFigures();

/** The ledger's id of each window the quota samples carry, which the Accounts cards key an hour's samples by. */
const LEDGER_WINDOW: Partial<Record<string, string>> = { '5h': 'five_hour', '7d': 'seven_day', '7d_fable': 'seven_day_fable' };

// Hourly account facts: peak utilization per window, time spent ineligible, and
// pin churn. The last seven days follow the sampled quota history; older hours
// are synthesized with the same shape.
function accountRollups(): AccountRollup[] {
  const h = createRng(7_777);
  const out: AccountRollup[] = [];
  const start = startOfHour(NOW) - 30 * DAY;
  const end = startOfHour(NOW);
  const sampleIndex = new Map<string, Map<number, number>>();
  // Each hour's last sample, which the route answers with its instant, beside the peak the hour's rows read.
  const lastSample = new Map<string, Map<number, QuotaSample>>();
  for (const hist of QUOTA_HISTORY) {
    const byHour = new Map<number, number>();
    const last = new Map<number, QuotaSample>();
    for (const sm of hist.samples) {
      const hour = startOfHour(Date.parse(sm.at));
      byHour.set(hour, Math.max(byHour.get(hour) ?? 0, sm.utilization));
      last.set(hour, sm);
    }
    sampleIndex.set(`${hist.accountId}|${hist.windowId}`, byHour);
    lastSample.set(`${hist.accountId}|${hist.windowId}`, last);
  }
  // An hour's last sample of each window the router reads, with the reset live at the sample's instant and the lead
  // the router would read then, to the account's end where it comes first, so the Pace card draws leads on both sides
  // of zero across the story's accounts.
  const sampledWindows = (account: (typeof ACCOUNTS)[number], t: number): AccountRollup['windows'] => {
    const windows: AccountRollup['windows'] = {};
    for (const w of [...account.windows, ...(account.fableWindow ? [account.fableWindow] : [])]) {
      const key = LEDGER_WINDOW[w.id];
      const sample = lastSample.get(`${account.id}|${w.id}`)?.get(t);
      if (key === undefined || sample === undefined) continue;
      const sampledMs = Date.parse(sample.at);
      const { resetsAt } = walkBack(w, sampledMs, () => SERVED_FACTOR);
      const read: WindowSample = { lead: aheadOf({ resetsAt, utilization: sample.utilization }, w.periodHours * HOUR, sampledMs, endOf(account, sampledMs)), resetsAt, sampledAt: sample.at, utilization: sample.utilization };
      windows[key] = read;
    }
    return windows;
  };
  for (const account of ACCOUNTS) {
    // An account never paired has never been sampled, so it has no hours to report.
    if (account.kind !== 'oauth' || account.status === 'unpaired') continue;
    let drift5 = h.float(0.2, 0.7);
    let drift7 = h.float(0.3, 0.8);
    let driftF = h.float(0.05, 0.4);
    // An hour holds facts from the one the plan started in to the one of the newest window reading, since a peak needs
    // a reading behind it. Every hour is still drawn, so cutting one account's hours moves no other account's values.
    // With no reading `Math.max` answers -Infinity, so such an account holds no hour.
    const since = account.subscription?.since;
    const from = since ? startOfHour(Date.parse(since)) : start;
    const readings = [...account.windows, ...(account.fableWindow ? [account.fableWindow] : [])].map((w) => Date.parse(w.observedAt));
    const until = Math.max(...readings);
    for (let t = start; t < end; t += HOUR) {
      const sampled5 = sampleIndex.get(`${account.id}|5h`)?.get(t);
      const sampled7 = sampleIndex.get(`${account.id}|7d`)?.get(t);
      const sampledF = sampleIndex.get(`${account.id}|7d_fable`)?.get(t);
      drift5 = Math.max(0, Math.min(1, drift5 + h.float(-0.12, 0.12)));
      drift7 = Math.max(0, Math.min(1, drift7 + h.float(-0.03, 0.03)));
      driftF = Math.max(0, Math.min(1, driftF + h.float(-0.03, 0.03)));
      const peak5 = sampled5 ?? drift5;
      const peak7 = sampled7 ?? drift7;
      const peakF = account.fableWindow ? sampledF ?? driftF : null;
      const cooldown = peak5 >= 0.97 ? h.int(600, 3_600) : 0;
      const overage = account.extraUsage?.enabled && peak5 >= 0.9 ? h.int(300, 3_600) : 0;
      const busy = new Date(t).getHours() >= 8 && new Date(t).getHours() <= 19;
      const row: AccountRollup = {
        ...ACCOUNT_ROLLUP_SAMPLE,
        accountId: account.id,
        hour: iso(t),
        peak: { '5h': Math.round(peak5 * 1000) / 1000, '7d': Math.round(peak7 * 1000) / 1000, '7d_fable': peakF === null ? null : Math.round(peakF * 1000) / 1000 },
        cooldownSeconds: cooldown,
        overageSeconds: overage,
        pins: { created: busy ? h.int(0, 3) : h.chance(0.2) ? 1 : 0, migratedIn: busy && h.chance(0.25) ? 1 : 0, migratedOut: cooldown > 0 ? h.int(1, 3) : 0 },
        windows: sampledWindows(account, t),
      };
      if (t >= from && t <= until) out.push(row);
    }
  }
  return out;
}

export const ACCOUNT_ROLLUPS: AccountRollup[] = accountRollups();

/** What the ledger counted as it read the body, which is the only figure the drawer's "bytes dropped" may use. */
const byteLength = (v: unknown): number => new TextEncoder().encode(typeof v === 'string' ? v : JSON.stringify(v)).length;

/** One frame of a stored stream, as the upstream sends it and the ledger keeps it. */
const sseFrame = (event: string, data: unknown): string => `event: ${event}\ndata: ${JSON.stringify(data)}\n\n`;

/** The answer's text, which a stream delivers in two deltas. */
const ANSWER_TEXT = 'The hook awaits a promise that resolves before the writer flushes. Moving the flush into the same tick fixes the race.';

/** A cut answer ran a megabyte past the cap before the ledger stopped storing it. */
const DROPPED_BYTES = 1_048_576;

export function capturedBodiesFor(request: RequestLog): CapturedBodies | null {
  if (!request.capturedBodies) return null;
  const isClaude = request.model.startsWith('claude');
  const bodies = {
    requestId: request.id,
    request: {
      model: request.model,
      max_tokens: 32_000,
      stream: request.transport === 'sse',
      system: [
        { type: 'text', text: 'You are Claude Code, Anthropic\'s official CLI for Claude.', cache_control: { type: 'ephemeral' } },
        { type: 'text', text: '[project instructions omitted from fixture: 4,812 tokens]' },
      ],
      messages: [
        { role: 'user', content: [{ type: 'text', text: 'Fix the flaky pre-commit hook and add a test.' }] },
        { role: 'assistant', content: [{ type: 'text', text: 'Reading the hook first.' }, { type: 'tool_use', id: 'toolu_01', name: 'Read', input: { file_path: 'hooks/pre-commit.ts' } }] },
        { role: 'user', content: [{ type: 'tool_result', tool_use_id: 'toolu_01', content: '[file contents: 212 lines]' }] },
      ],
      metadata: { user_id: `session_${request.sessionId}` },
      tools: [{ name: 'Read', description: 'Reads a file', input_schema: { type: 'object', properties: { file_path: { type: 'string' } } } }],
    },
  };
  const message = {
    id: `msg_${request.id.slice(4)}`,
    type: 'message',
    role: 'assistant',
    model: isClaude ? request.model : `${request.model}-2026-05-01`,
    stop_reason: request.attempts[request.attempts.length - 1]?.response.stopReason ?? 'end_turn',
    stop_sequence: null,
    usage: {
      input_tokens: request.usage.input,
      cache_read_input_tokens: request.usage.cacheRead,
      cache_creation_input_tokens: request.usage.cacheWrite5m + request.usage.cacheWrite1h,
      cache_creation: { ephemeral_5m_input_tokens: request.usage.cacheWrite5m, ephemeral_1h_input_tokens: request.usage.cacheWrite1h },
      output_tokens: request.usage.output,
    },
  };
  // A stream is stored as the text of its events, the way the ledger keeps the bytes it saw, and a JSON answer as
  // its object. The opening frame carries the message with its usage, no content and no stop reason, as the
  // upstream sends it, the reason arriving on the message_delta frame.
  const cut = ANSWER_TEXT.indexOf('Moving');
  // A stream that did not succeed stopped after the content it reached, so its capture holds no close.
  const closed = request.outcome === 'succeeded';
  const response =
    request.transport === 'sse'
      ? [
          sseFrame('message_start', { type: 'message_start', message: { ...message, content: [], stop_reason: null } }),
          sseFrame('content_block_start', { type: 'content_block_start', index: 0, content_block: { type: 'text', text: '' } }),
          sseFrame('content_block_delta', { type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text: ANSWER_TEXT.slice(0, cut) } }),
          sseFrame('content_block_delta', { type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text: ANSWER_TEXT.slice(cut) } }),
          ...(closed
            ? [
                sseFrame('content_block_stop', { type: 'content_block_stop', index: 0 }),
                sseFrame('message_delta', { type: 'message_delta', delta: { stop_reason: message.stop_reason, stop_sequence: null }, usage: { output_tokens: request.usage.output } }),
                sseFrame('message_stop', { type: 'message_stop' }),
              ]
            : []),
        ].join('')
      : { ...message, content: [{ type: 'text', text: ANSWER_TEXT }] };
  // Counted once here, as the router counts the vectors it stored; the drawer reads these and measures nothing.
  const requestBytes = byteLength(bodies.request);
  const responseBytes = byteLength(response);
  return {
    ...bodies,
    response,
    requestBytesSeen: requestBytes,
    requestBytesStored: requestBytes,
    requestTruncated: request.requestTruncated,
    responseBytesSeen: responseBytes + (request.responseTruncated ? DROPPED_BYTES : 0),
    responseBytesStored: responseBytes,
    responseTruncated: request.responseTruncated,
  };
}
