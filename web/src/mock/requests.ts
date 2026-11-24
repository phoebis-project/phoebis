import { agentTokenOf, clientLeft, couldNotCache, countDropped, plus } from '@/api/fold';
import type {
  AccountKind, AccountRollup, ApiKey, Attempt, AttemptOutcome, AttemptWindow, AutoMode, CacheMissReason, CapturedBodies, CohortHold, CostShares, CostSplit, DiagnosticsSource, FeatureCall, InputTransformations, LatencyHistograms, PinMigrationReason, Placement,
  PriceCells, QuotaSample, RateLimitWindow, RequestEvent, RequestKind, RequestLog, RequestOutcome, SelectionTrace, TrafficBucket, Usage, WastedProbeReason, WindowId, WindowSample,
} from '@/api/types';
import type { GuardrailMatch } from '@/api/types';
import { CONVERSATION_CLASSES, keptWarmTokens } from '@/lib/analytics';
import { emptyCacheMissReasons, emptyWastedProbeReasons } from '@/lib/cache';
import { AGENT_TYPES } from '@/lib/client';
import { equivalentIn, fundingOf, inBand, modelWeeks, pricedWindow, shareIn, tierCapacity, weightOf } from '@/lib/costs';
import { addCount, emptyCounts } from '@/lib/counts';
import { NO_CLASS, NO_EFFORT, expiredBetweenTurns, foldClassifier, foldEffort, foldRequestClass, foldSpeed, hitUnderReason, latencySamples, servedMissReason, servingAttempt, servingResponse } from '@/lib/dimensions';
import { byteOrder, tokensTotal } from '@/lib/format';
import { fnv1a } from '@/lib/hash';
import { emptyHistogram, histogramOf } from '@/lib/histogram';
import { TRANSFORMATION_KEYS } from '@/lib/inputTransformations';
import { MODEL_SCOPE_PREFIX, endOf, isPaidSource, readWindows } from '@/lib/quota';
import { WINDOW_KEY } from '@/lib/windows';
import { ACCOUNTS, QUOTA_HISTORY, SHARED_WINDOWS, accountById, lastReadingOf, rankTier, sharedWindowsOf } from './accounts';
import type { HourlyRollup } from './analytics';
import { cellsFor, promptOf } from './catalog';
import { leadOf, leftPerHour, paceDeadline } from './lead';
import { RETIRED, keyById, poolById, storyAccountOf } from './pools';
import { createRng, type Rng } from './rng';
import { round } from './round';
import { ACCOUNT_ROLLUP_SAMPLE, REQUEST_SAMPLE, TABLES, tablesFor } from './samples';
import { DAY, DELETED_HOLDER_END, HOUR, MINUTE, NIGHTLY_CAP_RAISED, NOW, OVERFLOW_LAPSED_AT, OVERFLOW_PAUSED_AT, RESEARCH_BUDGET_AT, clock, daysAgo, iso, startOfHour } from './time';

const rng = createRng(7_331);
// The upstream's own ids draw from a stream of their own, so they move no draw of the rows.
const upstreamIds = createRng(9_191);
const upstreamIdFrom = (stream: Rng) => (prefix: string) => `${prefix}${stream.uuid().replace(/-/g, '').slice(0, 22)}`;
const upstreamId = upstreamIdFrom(upstreamIds);

/**
 * A story attempt's upstream id, drawn from a stream seeded by the seed's id and the attempt's index, so a seed added
 * anywhere in the list moves no other row's ids. `storyRow` asks the message id first, then the request id.
 */
const storyUpstreamIds = (seedId: string, attempt: number): ((prefix: string) => string) => upstreamIdFrom(createRng(fnv1a(`${seedId}/${attempt}`)));
// The instants inside a request (the write, the first content, the stream's close) draw from a stream of their own,
// so they move no draw of the rows and no hint.
const phaseRng = createRng(3_571);
/** The stream's close lands this many milliseconds before the finish at most, so the first content never passes it. */
const STREAM_TAIL = 30;
/**
 * The registry's kind of an account the story names, or a deleted one's. A wrong id fails the mock build rather than
 * reading a default.
 */
const kindOf = (accountId: string): AccountKind => {
  const kind = storyAccountOf(accountId)?.kind;
  if (kind === undefined) throw new Error(`the story names an account the registry lacks: ${accountId}`);
  return kind;
};
/**
 * The windows a request's shares are keyed by: the shared windows the serving account's set has, then the model's
 * weeks, which no set bounds.
 */
const shareWindowsOf = (accountId: string, model: string): string[] => [...sharedWindowsOf(accountId), ...modelWeeks(TABLES, model)];

/** The account tier an attempt lands under: the subscription's, none for an API key account. */
const accountTier = (accountId: string): string | null => accountById.get(accountId)?.subscription?.rateLimitTier ?? null;
/** The serving account's usage-based class, as the ledger stores it on the attempt. */
const usageBasedOf = (accountId: string): boolean => accountById.get(accountId)?.usageBased ?? false;

export const SESSION_IDS = Array.from({ length: 14 }, () => rng.uuid());

/** An earlier holder of `desktop`, created long before the live one. */
export const DELETED_HOLDER_GENERATION = Date.parse(daysAgo(120));

/**
 * Each session story's subagents, by the story's index in `sessionSpec`, each with the type its rows carry, so the
 * story's pins, its recipes and its request rows name one set. The first `agents - 1` entries are the subagents
 * `RECIPES` pins, and an entry past them has rows and no pin, an earlier agent. A story whose main runs alone, and a
 * fixed row, whose agents its figures write, hold none. Story 0 is the busy desktop session, story 4 carries an earlier
 * agent beside its pin, story 9 is the written session, and story 15 is the session the mod released whole.
 */
export const STORY_AGENTS: Readonly<Partial<Record<number, readonly { id: string; type: string }[]>>> = {
  0: [
    { id: 'c21e', type: 'fork' },
    { id: 'e4b7', type: 'general-purpose' },
  ],
  2: [{ id: 'a7f3', type: 'Plan' }],
  4: [
    { id: 'e90b', type: 'general-purpose' },
    { id: '0b7f', type: 'Explore' },
  ],
  7: [{ id: 'e90b', type: 'general-purpose' }],
  9: [
    { id: '4d1a', type: 'teammate' },
    { id: 'e7c1', type: 'general-purpose' },
    // An id of the length Claude Code gives a subagent.
    { id: '4f0c2a9e7b61d3c58', type: 'general-purpose' },
    { id: 'b2d8', type: 'general-purpose' },
    { id: '91aa', type: 'general-purpose' },
    { id: 'c3a9', type: 'general-purpose' },
  ],
  15: [
    { id: '2f9d', type: 'general-purpose' },
    { id: '8c1a', type: 'Explore' },
  ],
};
const storyAgentsOf = (sessionId: string | null) => (sessionId === null ? undefined : STORY_AGENTS[SESSION_IDS.indexOf(sessionId)]) ?? [];
/**
 * A drawn row's agent on its session's story, from one draw per row so every later roll stays: main on three slots of
 * seven, else the story's subagent the slot names, main where the story holds none.
 */
function storyAgent(sessionId: string, hints: Rng): { id: string; type: string | null } {
  const slot = rng.int(0, 6);
  // The hint stream draws a type on every subagent slot and leaves it unused, the story naming the type, since the
  // hints drawn after it read the stream from there.
  if (slot >= 3) hints.pick(AGENT_TYPES);
  const agents = storyAgentsOf(sessionId);
  const agent = slot < 3 || agents.length === 0 ? null : agents[(slot - 3) % agents.length];
  return { id: agent?.id ?? 'main', type: agent?.type ?? null };
}

/** One client per key, the way a machine runs one build of Claude Code and a runner its job on the Agent SDK. */
export const USER_AGENTS: Record<string, string> = {
  ci: 'claude-cli/2.1.302 (external, sdk-cli)',
  desktop: 'claude-cli/2.1.310 (external, cli)',
  laptop: 'claude-cli/2.0.44 (external, cli)',
};

/** The class the router derives from each of those agents. */
export const CLIENTS: Record<string, string> = {
  ci: 'claude-code/sdk-cli',
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
 * The account that serves Sonnet 4.5 alone, whatever the lane drew: the cost model has no weight of its own for it, so
 * its rows price nothing, which the attention item names. The sessions the story moves off it carry Sonnet 4.5 to
 * primary, whose 11:00 and 14:00 buckets price nothing either.
 */
const UNPRICED_ACCOUNT = 'secondary';
const modelOn = (accountId: string, drawn: string) => (accountId === UNPRICED_ACCOUNT ? 'claude-sonnet-4-5' : drawn);
/** The model a request asked for: Sonnet 4.5 wherever secondary served one of its attempts, else the lane's draw. */
const modelOf = (attempts: Attempt[], drawn: string) => (attempts.some((a) => a.accountId === UNPRICED_ACCOUNT && a.cooldownSet?.reason !== 'model_rejected') ? 'claude-sonnet-4-5' : drawn);

/**
 * Probes exist only where the pool keeps caches warm and the key permits cache warming, and so does anything kept warm
 * by one.
 */
export function warmsCaches(keyId: string, poolId: string): boolean {
  return poolById.get(poolId)?.keepalive.enabled === true && keyById.get(keyId)?.companionPermissions.includes('keepalive') !== false;
}

const TRAFFIC_KINDS: { value: RequestKind; weight: number }[] = [
  { value: 'inference', weight: 12 },
  { value: 'count_tokens', weight: 2 },
];
const WARMED_KINDS: { value: RequestKind; weight: number }[] = [...TRAFFIC_KINDS, { value: 'probe', weight: 3 }];

/**
 * How far an earlier window's reading runs ahead of its pace at most: the story's one shedding round is at 11:00, and
 * nightly's window, ten minutes old when the morning's Fable session is placed there, keeps room for it.
 */
const EARLIER_LEAD = 0.56;

/**
 * Walks a window back so a request seen hours ago reports the window that was
 * live at that moment. `olderFactor` is a thunk because it may draw from the
 * seeded stream, and only a window older than the current one needs it. An
 * earlier window's reading stays within its lead of the share elapsed, so no
 * account but the story's leads its pace by the pool's shedding margin.
 */
const walkBack = (w: RateLimitWindow, atMs: number, olderFactor: () => number): AttemptWindow => {
  // A window the upstream answers with no reset has no period to walk back through, so its reading stands as read.
  if (w.resetsAt === null) return { id: w.id, resetsAt: null, utilization: w.utilization };
  const period = w.periodHours * HOUR;
  const resetsAt = Date.parse(w.resetsAt);
  const shift = Math.ceil(Math.max(0, resetsAt - period - atMs) / period);
  const resetThen = resetsAt - shift * period;
  // A window with no share has no figure to walk back, so its head carried the reset alone.
  if (w.utilization === null) return { id: w.id, resetsAt: iso(resetThen), utilization: null };
  const phase = 1 - (resetThen - atMs) / period;
  const ceiling = shift === 0 ? 1 : Math.min(1, phase + EARLIER_LEAD);
  const utilization = Math.min(ceiling, Math.max(0.02, w.utilization * (shift === 0 ? phase / Math.max(0.05, 1 - (resetsAt - NOW) / period) : olderFactor())));
  // A walked-back reading never reaches the limit by rounding or by its factor: only a story point says a window is
  // spent. The nudge keeps a product that lands on a whole hundredth, 0.29 read as 28.999..., from losing one.
  return { id: w.id, resetsAt: iso(resetThen), utilization: Math.min(Math.floor(utilization * 100 + 1e-9) / 100, 0.99) };
};

export const oauthWindows = (accountId: string, atMs: number, olderFactor: () => number = () => rng.float(0.4, 1.1)): AttemptWindow[] => {
  const account = accountById.get(accountId);
  if (!account || account.kind !== 'oauth') return [];
  // The drawn day's ids are pinned by the checks and the site's screenshots, so a story edit that moves a request
  // between windows keeps the stream's number of draws. The drawn day was seeded while secondary's 5-hour window opened
  // at 10:42, and the story opens it at 09:55, so a walk back of secondary between the two reads the live window and
  // draws nothing: it draws one older factor here and drops it. Moving either bound shifts every later drawn id.
  if (accountId === 'secondary' && atMs >= clock(9, 55) && atMs < clock(10, 42)) olderFactor();
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

/** An answer prices at the cells the ledger selects for it in the price entry in force at `at`, its arrival: the
 *  model's fast cells for a fast answer on a row that has them, else the long-context cells when its prompt is over
 *  the threshold, else the standard cells. A model the entry lacks prices at nothing. */
export function costSplitOf(model: string, u: Usage, fast: boolean, at: string | number): CostSplit {
  const p = cellsFor(model, u, fast, at);
  if (p === null) return { cacheRead: 0, cacheWrite: 0, input: 0, output: 0 };
  return {
    cacheRead: (u.cacheRead * p.cacheReadPerM) / 1_000_000,
    cacheWrite: (u.cacheWrite5m * p.cacheWrite5mPerM + u.cacheWrite1h * p.cacheWrite1hPerM) / 1_000_000,
    input: (u.input * p.inputPerM) / 1_000_000,
    output: (u.output * p.outputPerM) / 1_000_000,
  };
}

export function costOf(model: string, u: Usage, fast: boolean, at: string | number): number {
  const c = costSplitOf(model, u, fast, at);
  return c.input + c.cacheRead + c.cacheWrite + c.output;
}

/** The cells `cellsFor` selects for a prompt of `prompt` tokens, whatever tokens the figure it prices counts. Null for
 *  a model the entry lacks. */
function cellsAtPrompt(model: string, prompt: number, fast: boolean, at: string | number): PriceCells | null {
  return cellsFor(model, { cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: 0, input: prompt }, fast, at);
}

/**
 * A kept-warm credit's saving in dollars, the write each count avoided at its TTL's rate less the read the two paid, at
 * the cells the credited requests' own prompt of `prompt` tokens selects, as the ledger keys the credit by its
 * request's band. The saving's tokens select nothing themselves: the read and the writes count the kept tokens twice.
 */
function savingUsdOf(model: string, kept: { tokens1h: number; tokens5m: number }, prompt: number, fast: boolean, at: string | number): number {
  const p = cellsAtPrompt(model, prompt, fast, at);
  if (p === null) return 0;
  return (kept.tokens5m * p.cacheWrite5mPerM + kept.tokens1h * p.cacheWrite1hPerM - keptWarmTokens(kept) * p.cacheReadPerM) / 1_000_000;
}

/** A kept request's read under the TTL its own write took, five minutes for a request that wrote none. */
const keptOf = (r: RequestLog) => (r.usage.cacheWrite1h > 0 ? { tokens1h: r.usage.cacheRead, tokens5m: 0 } : { tokens1h: 0, tokens5m: r.usage.cacheRead });

export const sumSplit = (parts: CostSplit[]): CostSplit => sumEach(parts, { cacheRead: 0, cacheWrite: 0, input: 0, output: 0 });

// Its own stream, so these draws leave the fixtures the main stream feeds alone.
const missRng = createRng(1_517);

/**
 * The keys whose client sends a diagnostics member and a thinking-binding token of its own: the current Claude Code
 * builds on `ci` and `desktop`. The laptop's older build sends neither, and the other keys' rows name no Claude Code
 * client, so none of their own.
 */
const MEMBER_CLIENT_KEYS: ReadonlySet<string> = new Set(['ci', 'desktop']);

/**
 * Whether a request's body carried a cache breakpoint of its own. The story's clients mark a classifier call and a
 * turn whose session a pinning pool holds, a probe replays a marked turn, and a token count marks none.
 */
function ownMarkerOf(r: Pick<RequestLog, 'classifier' | 'kind' | 'poolId' | 'sessionId'>): boolean {
  if (r.kind === 'probe') return true;
  return r.kind === 'inference' && (r.classifier !== null || ((poolById.get(r.poolId ?? '')?.affinityTtlSeconds ?? 0) > 0 && r.sessionId !== null));
}

/**
 * The one marker fact the relay keeps for its selection and for the request's row: the body's own marker, or the one
 * the pool's cache TTL policy adds to a body that carries none, which it adds to an inference or a probe of no
 * auxiliary class where the policy is on with `inject_when_absent`.
 */
function markerOf(r: Pick<RequestLog, 'kind' | 'poolId' | 'requestClass'>, own: boolean): boolean {
  const policy = poolById.get(r.poolId ?? '')?.cacheTtl;
  const auxiliary = r.requestClass !== null && !CONVERSATION_CLASSES.some((c) => c === r.requestClass);
  return own || ((r.kind === 'inference' || r.kind === 'probe') && policy?.enabled === true && policy.injectWhenAbsent && !auxiliary);
}

/**
 * Who put the diagnostics member on a request's serving body, as the relay reads it: the client where its build sends
 * one, else the router where the pool's `cacheDiagnostics` is on and the body marked a cache breakpoint. A token count
 * carries neither, since its schema has no slot for the member, and a request no attempt served sent no body.
 */
function sourceOf(keyId: string | null, poolId: string | null, kind: RequestKind, attempted: boolean, marked: boolean): DiagnosticsSource | null {
  if (kind === 'count_tokens' || !attempted) return null;
  if (MEMBER_CLIENT_KEYS.has(keyId ?? '')) return 'client';
  return marked && poolById.get(poolId ?? '')?.cacheDiagnostics === true ? 'router' : null;
}

/**
 * Who sent the thinking-binding token on a request, as the relay reads it: the client where its build sends its own,
 * a token count's included, else the router where the pool's `thinkingDiagnostics` is on and the request is no token
 * count. Every row asks for thinking, so the thinking type the router's gate also reads holds on each.
 */
function thinkingSourceOf(keyId: string | null, poolId: string | null, kind: RequestKind): DiagnosticsSource | null {
  if (MEMBER_CLIENT_KEYS.has(keyId ?? '')) return 'client';
  return kind !== 'count_tokens' && poolById.get(poolId ?? '')?.thinkingDiagnostics === true ? 'router' : null;
}

/** The reason an OAuth account's answer draws where the body named a message the account holds. */
const compare = (): CacheMissReason =>
  missRng.weighted<CacheMissReason>([
    { value: 'none', weight: 72 },
    { value: 'messages_changed', weight: 7 },
    { value: 'system_changed', weight: 4 },
    { value: 'tools_changed', weight: 3 },
    { value: 'model_changed', weight: 2 },
    { value: 'unavailable', weight: 2 },
  ]);

/**
 * What the upstream compared for each attempt of one request, which `walkSessionTurns` files. Diagnostics ride on a
 * response body, so an attempt that failed upstream carries none, and the token-count endpoint returns no usage block
 * to compare against. An API-key account in this fixture answers without the beta, so it compares nothing. A failover's
 * leg waits for the walk, which reads it off the account of the leg before it.
 */
function diagnosed(attempts: Attempt[], kind: RequestKind): Attempt[] {
  return attempts.map((a) => {
    if (a.upstreamStatus !== 200 || kind === 'count_tokens' || a.index > 0) return a;
    // The draw stands wherever an OAuth account's first answer stands, whatever word the attempt files, so every later
    // draw keeps its place.
    return { ...a, cacheMissReason: a.accountKind === 'oauth' ? compare() : null };
  });
}

/** The instant the story's reading of secondary's 7-day window first reads over all's ceiling, inside the hour before
 *  the renewal of 14:02:10. */
export const SECONDARY_WEEK_OVER_AT = clock(13, 3);

/**
 * The story's own readings of a window where the walk back's even climb would not tell it, per account and window.
 * Nightly's 5-hour window, fresh at its 13:11 reset, climbs fast enough that a fresh session no longer fits there by
 * 14:00, so the hour's placements are Pace's to make between primary and secondary. Secondary's 5-hour window opens at
 * 09:55 and reads 70% from the 640k-token conversation's placement at 10:52:30, too far along for its rewrite but with
 * room for the two sessions placed just before 11:00, then runs hot, so it leads its pace by the pool's 0.9 margin
 * through the 11:00 shedding round and the shed move after it, and reads spent from the head of 14:02:11. Its 7-day
 * window holds all's ceiling of 0.75 from the walk back's last reading there at 11:46:20 until `SECONDARY_WEEK_OVER_AT`,
 * and reads the walk back's figures from then on: a week over the ceiling with no burn waits an hour before it fires,
 * so the renewal of 14:02:10 reads it inside that wait.
 */
const STORY_READINGS: Record<string, Partial<Record<WindowId, { at: number; utilization: number }[]>>> = {
  nightly: {
    '5h': [
      { at: clock(13, 11), utilization: 0 },
      { at: clock(14, 2, 11), utilization: 0.68 },
      { at: clock(14, 30), utilization: 0.97 },
    ],
  },
  secondary: {
    '5h': [
      { at: clock(9, 55), utilization: 0 },
      { at: clock(10, 52, 30), utilization: 0.7 },
      { at: clock(10, 59, 10), utilization: 0.7 },
      { at: clock(11, 0), utilization: 0.97 },
      { at: clock(11, 8, 30), utilization: 0.99 },
      { at: clock(14, 2, 10), utilization: 0.99 },
      { at: clock(14, 2, 11), utilization: 1 },
    ],
    // The walk back reads 0.78 from 12:54:32, 0.79 from 13:28:38 and 0.80 from 14:02:44.
    '7d': [
      { at: clock(11, 46, 20), utilization: 0.75 },
      { at: SECONDARY_WEEK_OVER_AT - 1_000, utilization: 0.75 },
      { at: SECONDARY_WEEK_OVER_AT, utilization: 0.78 },
      { at: clock(13, 28, 37), utilization: 0.78 },
      { at: clock(13, 28, 38), utilization: 0.79 },
      { at: clock(14, 2, 43), utilization: 0.79 },
      { at: clock(14, 2, 44), utilization: 0.8 },
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
  return { ...read, utilization: round(from.utilization + ((to.utilization - from.utilization) * (atMs - from.at)) / (to.at - from.at), 2) };
}

/** The routing log's story hour, written by hand: no drawn request falls inside it. */
export const STORY_HOUR_FROM = NOW - HOUR;

export const HAIKU = 'claude-haiku-4-5-20251001';

/**
 * Whether an account could take a request at `atMs` by the story's own account facts, so no attempt lands where the
 * decision card would show the account out: secondary, which serves Sonnet 4.5 alone, reads its 5-hour window spent
 * from 14:02:11 and cools on it from 14:07:51 to 14:55. Primary's fast lane cools from 14:27:00 to 14:42 and it
 * rejects Haiku from 14:24:10 to 15:24. Nightly's usage credits take a fast request once its spend cap is raised at
 * 11:30:12. The Overflow workspace needs funding from 11:30:12, and research spends its day's budget at 12:41:07.
 */
export function servable(accountId: string, atMs: number, model: string, fast: boolean): boolean {
  const within = (from: number, to: number) => atMs >= from && atMs < to;
  if (accountId === 'secondary') return !within(clock(14, 2, 11), clock(14, 55));
  // Primary's two spans are its seed's cooldowns: a fast scope holds a fast request, a model scope that model.
  if (accountId === 'primary') return !accountById.get('primary')!.cooldowns.some((c) => (c.scope === 'fast' ? fast : c.scope === `${MODEL_SCOPE_PREFIX}${model}`) && within(Date.parse(c.setAt), Date.parse(c.until)));
  if (accountId === 'nightly') return !fast || atMs >= NIGHTLY_CAP_RAISED;
  if (accountId === 'overflow') return atMs < OVERFLOW_LAPSED_AT;
  if (accountId === 'research') return atMs < RESEARCH_BUDGET_AT;
  return true;
}

/** An account a request is handed to, or a story attempt's, walks its windows back at a fixed factor, not a draw. */
const SERVED_FACTOR = 0.8;

/** An attempt handed to `accountId` at `at`, which reads that account's kind, tier, usage-based class and windows. */
const onAccount = (a: Attempt, accountId: string, at: number): Attempt => {
  const kind = kindOf(accountId);
  return { ...a, accountId, accountKind: kind, overageInUse: kind === 'oauth' ? false : null, tier: accountTier(accountId), usageBased: usageBasedOf(accountId), windows: oauthWindows(accountId, at, () => SERVED_FACTOR) };
};

/**
 * The order the engine takes a token count's members in: the best tier first, then under Pace the highest score off
 * the walk back. A count takes no slot, so no cap passes a member over for it and a draw cannot stand where the
 * engine picks another. Nightly reads in overage from its spend cap's raise: `OVERAGE_FROM` is built from the attempts
 * this order places, and no token count starts between the two instants, so both read nightly's overage alike. An API
 * key shares its tier with no other member here, so it keeps the pool's order, and outside Pace the drawn account
 * leads its tier.
 */
function countOrder(ids: string[], poolId: string, atMs: number, model: string, drawn: string): string[] {
  const pool = poolById.get(poolId);
  if (!pool) return ids;
  const tierOf = (id: string): number[] => {
    const overage = id === 'nightly' && atMs >= NIGHTLY_CAP_RAISED;
    return rankTier(pool.preferSubscription, isPaidSource({ kind: kindOf(id), usageBased: usageBasedOf(id) }), overage, pool.members.find((m) => m.accountId === id)?.priority ?? 0);
  };
  const windowId = model.includes('fable') ? '7d_fable' : '7d';
  const scoreOf = (id: string): number => {
    if (pool.strategy !== 'pace') return id === drawn ? 1 : 0;
    if (kindOf(id) === 'api_key') return 0;
    const reading = oauthWindows(id, atMs, () => SERVED_FACTOR).find((w) => w.id === windowId && w.utilization !== null);
    const reset = reading?.resetsAt ? Date.parse(reading.resetsAt) : null;
    return leftPerHour(reading?.utilization ?? 0, paceDeadline(accountById.get(id)!, reset, atMs), atMs);
  };
  const ranked = ids.map((id) => ({ id, score: scoreOf(id), tier: tierOf(id) }));
  ranked.sort((x, y) => {
    const i = x.tier.findIndex((n, j) => n !== y.tier[j]);
    return i >= 0 ? x.tier[i] - y.tier[i] : y.score - x.score;
  });
  return ranked.map((x) => x.id);
}

/** The morning's shedding round: secondary's 5-hour window projects past the pool's ceiling at 11:00. */
export const SHED_ROUND = { accountId: UNPRICED_ACCOUNT, at: clock(11, 0) };
/** Studio's round fired by the stretch, at the renewal its slowed request's hold led to. */
export const STRETCH_ROUND = { accountId: 'studio', at: clock(14, 5) };

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
 * The relay's estimate of a token count's body, by request id, for a count that answered as drawn. The router stores
 * no usage for a token count, while its decision record reads the estimate.
 */
export const COUNTED_BODY_TOKENS = new Map<string, number>();

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
    // Secondary serves Sonnet 4.5 alone, so it takes a request only as its last attempt, where the request is the
    // Sonnet 4.5 turn it serves rather than the model the lane drew.
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
      // Read off the day's rows with the previous size (`walkSessionTurns`).
      entryExpired: null,
      index: i,
      issuedDigest: null,
      outcome,
      overageInUse: kind === 'oauth' ? false : null,
      // Read off the day's rows once each stands as it ends (`walkSessionTurns`).
      previousInputTokens: null,
      response: { clearedInputTokens: null, inferenceGeo: 'us', inputTransformations: null, refusalCategory: null, refusalRecommendedModel: null, servedModel: null, speed: 'standard', stopReason: 'end_turn', stopSequence: null },
      // Drawn once the events exist, since a trace names the move an event records.
      selection: null,
      // An answer's usage names the tier it ran on, and a token count's answer carries no usage. Every drawn answer ran
      // on the standard tier.
      serviceTier: upstreamStatus === 200 && !count ? 'standard' : null,
      startedAt: iso(cursor),
      tier: accountTier(accountId),
      // The upstream names its request on every answer, an error document included, and a message on a 200 alone.
      upstreamMessageId: upstreamStatus === 200 ? upstreamId('msg_01') : null,
      upstreamRequestId: upstreamId('req_011C'),
      upstreamStatus,
      usageBased: usageBasedOf(accountId),
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
  // inside a tier. They answer 429 too, but secondary, which serves Sonnet 4.5 alone: it refuses any other model, as
  // the rejection story's account does, and its plan draws no usage credits for a fast request.
  const tail = attempts[attempts.length - 1];
  let added = 0;
  if (tail?.upstreamStatus === 429) {
    const sonnet45 = attempts.some((a) => a.accountId === UNPRICED_ACCOUNT);
    // A request secondary took is the Sonnet 4.5 turn it serves, which is no fast one.
    const stillFast = fast && !sonnet45;
    const members = (poolById.get(lane.poolId)?.members ?? []).map((m) => m.accountId);
    for (const accountId of count ? countOrder(members, lane.poolId, cursor, model, '') : members) {
      if (attempts.some((a) => a.accountId === accountId) || !servable(accountId, cursor, model, stillFast) || (stillFast && accountId === UNPRICED_ACCOUNT)) continue;
      const leg = onAccount(tail, accountId, cursor);
      const refuses = accountId === UNPRICED_ACCOUNT && !sonnet45;
      attempts.push({
        ...leg,
        cooldownSet: refuses ? { reason: 'model_rejected', until: iso(cursor + 300 + HOUR) } : rateLimitCooldown(leg.windows, cursor + 300, 30),
        durationMs: 300,
        index: attempts.length,
        outcome: refuses ? 'upstream_error' : 'rate_limited',
        startedAt: iso(cursor),
        upstreamRequestId: upstreamId('req_011C'),
        upstreamStatus: refuses ? 404 : 429,
      });
      added += 1;
      cursor += 400;
    }
  }
  return { attempts, chainFrom: attempts.length - added, drawnHead: drawnTrail[drawnTrail.length - 1], whole: !ended && !committed && added === 0 };
}

/** The digest of the story's one fallback credit token, in the capture's shape: 64 hex characters. */
const FALLBACK_DIGEST = '3b7e1f9a5c2d8e4b6a0f7c3d9e1b5a8f2c6d0e4a7b9f1c3e5d8a2b6f0c4e7a9d';

/** A request's attempts with the serving one, the last, put through `edit`. */
const onServing = (attempts: Attempt[], edit: (a: Attempt) => Attempt): Attempt[] => attempts.map((a, i, all) => (i === all.length - 1 ? edit(a) : a));

function buildRequests(): RequestLog[] {
  const rows: RequestLog[] = [];
  const span = 24 * HOUR;
  // The eligible rows take in turn what the drawer and the refusal views read, without a draw of their own, so no later
  // draw moves: the drawer's refusal line first, then a row of each auto-mode standing the relay publishes, then the
  // refusals the Refusals by category card ranks and the Requests filters keep, `cyber` three times in all, `harmful`
  // twice and one the upstream gave no category. The standings keep the second and the third eligible rows, so the row
  // the stripped standing moves to notebook, which the passes after the loop draw less for, stays the one it is.
  const owed: ({ autoMode: AutoMode } | { refusal: string | null })[] = [
    { refusal: 'cyber' },
    { autoMode: 'server' },
    { autoMode: 'stripped' },
    { refusal: 'harmful' },
    { refusal: 'cyber' },
    { refusal: null },
    { refusal: 'harmful' },
    { refusal: 'cyber' },
  ];
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
    // is not asked to think reads none. The router stores no usage for a token count, so its usage reads zeros, while
    // its one draw is still taken where the usage would be and stands for the relay's estimate of the body.
    const counted = finalStatus === 200 && kind === 'count_tokens' ? rng.int(2_000, 60_000) : null;
    const drawnUsage = finalStatus === 200 && counted === null ? usageFor(modelOn(drawnHead ?? '', drawn), kind) : { input: 0, cacheRead: 0, cacheWrite5m: 0, cacheWrite1h: 0, output: 0, ...NO_ANSWER_FACTS };
    const usage = model.includes('fable') || model.includes('opus') ? drawnUsage : { ...drawnUsage, thinking: 0 };
    // The story hour's rows leave below, so the rows the drawer reads land before it.
    const owable = lane.keyId === 'laptop' && kind === 'inference' && finalStatus === 200 && whole && atMs < STORY_HOUR_FROM;
    const turn = owable ? owed.shift() : undefined;
    const autoMode = turn !== undefined && 'autoMode' in turn ? turn.autoMode : null;
    const transport = kind === 'inference' ? (rng.chance(0.86) ? 'sse' : 'json') : 'json';
    // The request runs from its arrival to its last attempt's end, the gaps between attempts included, as the router
    // stamps it.
    const durationMs = head === undefined ? 0 : attemptEnd(head) - atMs;
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
    const agent = storyAgent(sessionId, hints);
    // The marker reads the session, so the reasons the member decides follow its draw. They draw from a stream of their
    // own, so their place moves no draw of the main stream.
    const ownMarker = ownMarkerOf({ classifier: REQUEST_SAMPLE.classifier, kind, poolId: lane.poolId, sessionId });
    const source = sourceOf(lane.keyId, lane.poolId, kind, tried.length > 0, ownMarker);
    const diagnosis = diagnosed(tried, kind);
    const attempts =
      turn !== undefined && 'refusal' in turn
        ? onServing(diagnosis, (a) => ({ ...a, response: { ...a.response, refusalCategory: turn.refusal, refusalRecommendedModel: 'claude-opus-5', stopReason: 'refusal' } }))
        : diagnosis;
    rows.push({
      ...REQUEST_SAMPLE,
      guardrailId: null,
      guardrails: [],
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
      costUsd: round(costOf(model, usage, false, atMs), 4),
      diagnosticsSource: source,
      errorCode,
      sessionId,
      agentId: agent.id,
      userAgent: USER_AGENTS[lane.keyId] ?? null,
      client: CLIENTS[lane.keyId] ?? null,
      agentType: agent.type,
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
      responseTruncated: lane.keyId === 'laptop' && i % 7 === 0,
    });
    if (chainFrom < tried.length) CHAIN_FROM.set(id, chainFrom);
    if (head === undefined) Object.assign(rows[rows.length - 1], unservedAnswer());
    else if (!whole) Object.assign(rows[rows.length - 1], endedAnswer(tried, atMs));
    else if (counted !== null) COUNTED_BODY_TOKENS.set(id, counted);
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
      const notebookOwnMarker = ownMarkerOf({ ...moved, poolId: 'work' });
      const notebookSource = sourceOf('notebook', 'work', kind, true, notebookOwnMarker);
      Object.assign(moved, {
        accountId: 'studio',
        attempts: [{ ...onAccount(answered, 'studio', Date.parse(answered.startedAt)), cacheMissReason: answered.cacheMissReason, index: 0 }],
        // The answering attempt keeps its start, so the request runs from its arrival to that attempt's end.
        durationMs: attemptEnd(answered) - atMs,
        canonicalModel: drawn,
        capturedBodies: false,
        costUsd: round(costOf(drawn, moved.usage, false, atMs), 4),
        diagnosticsSource: notebookSource,
        keyGeneration: Date.parse(keyById.get('notebook')!.createdAt),
        keyId: 'notebook',
        model: drawn,
        poolId: 'work',
        responseTruncated: false,
      });
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
  // the session the live key holds now, so the request log shows that session id under each holder's generation while
  // the Sessions view lists it once, under the live key, the holder's last use having passed the non-companion timeout.
  const holderRows = rows.filter((r) => r.keyId === 'desktop' && Date.parse(r.at) < DELETED_HOLDER_END);
  for (const r of holderRows) r.keyGeneration = DELETED_HOLDER_GENERATION;
  const holderLast = holderRows.find((r) => r.kind !== 'probe');
  if (holderLast) Object.assign(holderLast, { sessionId: SESSION_IDS[0] });
  // The newest desktop stream with a 200 head other than `skip` ends on the code and the outcome given, its serving
  // attempt failed upstream. Neither pass draws, so every row keeps its draws.
  const failStream = (skip: RequestLog | undefined, errorCode: string, outcome: RequestOutcome): RequestLog | undefined => {
    const row = rows.find((r) => r !== skip && r.keyId === 'desktop' && r.kind === 'inference' && r.transport === 'sse' && r.status === 200);
    if (row) {
      row.errorCode = errorCode;
      row.outcome = outcome;
      row.attempts = onServing(row.attempts, (a) => ({ ...a, outcome: 'upstream_error' }));
    }
    return row;
  };
  // A stream can commit its 200 head and fail after it: the newest such desktop stream keeps the status and
  // carries the error, which is the row the Requests table and the drawer must not draw as a success.
  const failedAfterHead = failStream(undefined, 'overloaded_error', 'failed');
  // The newest desktop stream after that one ended before its `message_stop`: the row the Requests table draws as a
  // failure and the drawer names.
  failStream(failedAfterHead, 'stream_incomplete', 'incomplete');
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
    r.attempts = onServing(r.attempts, (a) => ({ ...a, response: { ...a.response, speed: 'fast' } }));
    r.costUsd = round(costOf(r.model, r.usage, true, r.at), 4);
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
  // The day's one fallback: the oldest refused laptop turn issued a credit token, and the session's next inference
  // turn on the key presented it. Neither pass draws, so every row keeps its draws.
  const oldestFirst = [...rows].reverse();
  const issuer = oldestFirst.find((r) => r.keyId === 'laptop' && servingResponse(r)?.stopReason === 'refusal');
  const presenter = issuer && oldestFirst.find((r) => r.keyId === issuer.keyId && r.sessionId === issuer.sessionId && r.kind === 'inference' && Date.parse(r.at) > Date.parse(issuer.at));
  if (issuer && presenter) {
    issuer.attempts = onServing(issuer.attempts, (a) => ({ ...a, issuedDigest: FALLBACK_DIGEST }));
    presenter.fallbackDigest = FALLBACK_DIGEST;
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
      const fast = r.shape.requestedSpeed === 'fast';
      return r.attempts.map((a) => ({ accountId: a.accountId, at: Date.parse(a.startedAt), fast, model: r.canonicalModel, r }));
    })
    .sort((x, y) => x.at - y.at);
  for (const r of rows) {
    for (const a of r.attempts) {
      const set = a.cooldownSet;
      if (!set) continue;
      const setAt = attemptEnd(a);
      const until = Date.parse(set.until);
      const wouldHold = (u: (typeof uses)[number]) => u.r !== r && u.accountId === a.accountId && coolingHolds(set.reason, r.canonicalModel, u.model, u.fast);
      const next = uses.find((u) => wouldHold(u) && u.at >= setAt && u.at < until);
      if (next) a.cooldownSet = { ...set, until: iso(next.at) };
    }
  }
}

/**
 * A turn the story runs in fast mode: the desktop key's Opus inference. A pin of that key and model rides the fast lane
 * by the same rule, so the session rows read their lane here.
 */
export const fastTurn = (keyId: string | null, model: string, kind: RequestKind) => keyId === 'desktop' && model.includes('opus') && kind === 'inference';

/** The window a 429's cooldown names: the spent plan window its head read, else the retry-after. */
const windowOfReason = (reason: string) => (reason.startsWith('429 on 5h') ? 'five_hour' : reason.startsWith('429 on 7d') ? 'seven_day' : 'retry_after');

/**
 * A cooldown an answer set, in the reason, the scope and the window its routing-state row carries: a model's refusal
 * scopes to that model, a fast lane's limit to the fast lane, and any other 429 to the account.
 */
export const cooldownOf = (reason: string, model: string): { reason: string; scope: string; window: string } =>
  reason === 'model_rejected'
    ? { reason, scope: `${MODEL_SCOPE_PREFIX}${model}`, window: 'model_rejection' }
    : reason === 'fast_rate_limit'
      ? { reason, scope: 'fast', window: 'fast' }
      : { reason: 'rate_limit', scope: 'account', window: windowOfReason(reason) };

/** Whether a live row keeps this request off its account: a plan-window row holds a standard request alone. */
export const holds = (c: { scope: string; window: string }, model: string, fast: boolean) => (c.scope === 'account' ? !(fast && SHARED_WINDOWS.some((w) => w === c.window)) : c.scope === 'fast' ? fast : c.scope === `${MODEL_SCOPE_PREFIX}${model}`);

/**
 * Whether a cooldown an answer set holds a request: a model's refusal holds that model, a fast lane's a fast request,
 * a spent plan window a standard one, and a retry-after every request.
 */
const coolingHolds = (reason: string, setFor: string, model: string, fast: boolean) => holds(cooldownOf(reason, setFor), model, fast);

/** Whether an answer's cooldown keeps the account from a request at `at`, before any later use lifted it. */
function coolingAt(accountId: string, at: number, model: string, fast: boolean): boolean {
  return REQUESTS.some((r) =>
    r.attempts.some((a) => {
      const set = a.cooldownSet;
      if (!set || a.accountId !== accountId) return false;
      const from = attemptEnd(a);
      return at >= from && at < Date.parse(set.until) && coolingHolds(set.reason, r.canonicalModel, model, fast);
    }),
  );
}

/** Whether a request could wait on the account at `at`: one it could be served by then, which no cooldown holds. */
const holdableAt = (r: RequestLog, accountId: string, at: number) => {
  const fast = r.shape.requestedSpeed === 'fast';
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
/**
 * Whether attempt `i` is the relay's resend of an attempt that never reached the upstream: it goes out once more on the
 * same account under the decision that chose it, so the router writes no decision for it and the ledger counts no
 * failover.
 */
export const resentInPlace = (r: Pick<RequestLog, 'attempts'>, i: number): boolean => i > 0 && r.attempts[i - 1].upstreamStatus === null && r.attempts[i - 1].accountId === r.attempts[i].accountId;
/** The instant an attempt's answer lands, which its cooldown, its events and its completion date from. */
export const attemptEnd = (a: Pick<Attempt, 'durationMs' | 'startedAt'>): number => Date.parse(a.startedAt) + (a.durationMs ?? 0);

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
    durationMs: attemptEnd(last) - atMs,
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
  /**
   * The 5-hour reading the answer's head reported, where it moved past the reading the decision read or where the
   * story reads its own figure rather than the walk back's.
   */
  head5h?: number;
  /**
   * An attempt no head reached: the router's own stop cut it first, whatever status its seed names, or its connection
   * failed before the body went out, which names none.
   */
  headless?: true;
  ms: number;
  status?: number;
}

/** The geography an answer reported, where the model does not take the parameter. */
type StoryGeo = 'not_available';

interface StorySeed {
  /** The agent the client named, null for a request refused before its body was read. */
  agentId: string | null;
  at: number;
  attempts: StoryAttempt[];
  /** A token count's body as the relay estimated it, which the router stores no usage for. */
  bodyTokens?: number;
  /** A body that marked no cache breakpoint, written by hand whatever `markerOf` reads for the turn. */
  cacheMarker?: false;
  canonical?: string;
  /** Whether the bodies were captured, where it is not the laptop key's rule. */
  capturedBodies?: boolean;
  /** The client's own auto-mode classifier call, which the class header and the beta mark: `client` on the account and
   * incarnation its judged turn ended on, `client_off` off them. */
  classifier?: NonNullable<RequestLog['classifier']>;
  /** The label the ledger wrote, where it is not the lane's. */
  client?: string | null;
  /** The item a contract refusal named. */
  clientContract?: string;
  clientDrift?: string;
  contextCompacted?: string;
  errorCode?: string;
  /** The client's subagent summary fork: its captured body carries the fork's tools and its question last. */
  fork?: boolean;
  /** The answer's inference geography where it is not the story's `us`. */
  geo?: StoryGeo;
  /** The guardrails that matched, in the order they ran. The one marked blocked refused the request. */
  guardrails?: GuardrailMatch[];
  held?: CohortHold;
  id: string;
  keyId: string;
  kind?: RequestKind;
  /**
   * A first placement after the session's row expired, on an account that never saw the conversation, which the cache
   * diagnostics read as a failover's landing reads.
   */
  landsAfresh?: true;
  model: string;
  outcome?: RequestOutcome;
  /** The stream ended before the answer reported its output count, so the serving attempt stores none. */
  outputUnreported?: true;
  /** The upstream's comparison was still running when its one attempt's answer went out. */
  pending?: true;
  /** The router refused the request before any account was asked, which the ledger's landing would have flagged. */
  refused?: true;
  /** The class header as the client sent it, null for a request that sent none. */
  requestClass: string | null;
  /** The client's session, a prefix of the story's own ids, null for a request that sent none. */
  session: string | null;
  status?: number;
  /** The tier the answer ran on, where it is not the standard one. */
  tier?: 'priority';
  /**
   * A row the ledger stored before the relay filed its own words on an answer that named no reason, so its serving
   * attempt keeps the null the route still answers for it.
   */
  unfiled?: true;
  /**
   * A story row that takes no draw from the streams the drawn rows read, so it keeps its written class and instants
   * and every drawn figure stays where it was. The routing log derives its records as it does a drawn row's, or writes
   * them by hand where `STORY_IDS` names it, as it does for the rows that stamp a round's window and keep its account
   * pinned to the round. The derived ones: the
   * contract seat's rows on fleet, the approved Ultra's rows on bench, the last turns of the desktop session the mod
   * ended, the one turn of story 4's Explore subagent, the classifier call its judged account could not take, the turn
   * whose placement replaced an expired row, the three turns of the conversation whose pin the purge takes twice, the
   * two turns of the conversation the Overflow workspace's lapse moves, the two subagent turns the laptop session's
   * classifier calls judge, the first turn of the subagent `f2c8` that the 11:00 round reads, the turn the upstream
   * answered 400 and the two turns of the conversation that moves off the deleted account.
   */
  unseeded?: true;
  usage?: Partial<Usage>;
  userAgent?: string;
}

/**
 * The routing log's hand-written requests: the story hour, the morning's shedding round and its move, nightly's
 * Fable-week round, the least-bad move onto nightly and the wait for its head, the hold for nightly's reset,
 * studio's slowed sessions in work with the round their stretch fires, and the client's two classifier calls. The
 * gate's two refusals on ci sit among them with no decision, since the gate writes none.
 */
export const STORY_IDS = new Set([
  'req_0b7c2d94-e61', 'req_1d9c3e55-0b2', 'req_1eaa9bc1-a18', 'req_2b9e4c7f-d36', 'req_2c7d9f15-b63', 'req_2d8e5b13-f40', 'req_2d8f6b14-a79', 'req_2e7c90a1-6d4',
  'req_2f9a6c4e-d84', 'req_31f0b7c2-6e1', 'req_3b8e51f4-0a9', 'req_3d7b9e2f-a41', 'req_3e2b9d47-c18', 'req_3e7a5c19-f84', 'req_3f8c1d94-b57',
  'req_4a9c3e7b-a27', 'req_4b0e9d17-c31', 'req_4b9e2c71-a35',
  'req_4d9a1e6b-c20',
  'req_4e8b1d36-f72', 'req_4f2a8d6e-b7a', 'req_51d0e2b8-7a0', 'req_5b2e8d47-c19', 'req_5c7e2a9d-b14', 'req_5d1f8c3a-b27',
  'req_5d6f1b3a-e27', 'req_5e2f7a19-b03',
  'req_5e3c8a41-d27', 'req_5f2c8b19-d73',
  'req_63a8d2f5-b17',
  'req_69e30548-32e', 'req_6a1c9e47-b90', 'req_6b3e8d1a-c73', 'req_6c3a9e27-5b1', 'req_6d0b3f85-a14', 'req_6e1f3b98-a24', 'req_71b5f2c8-e40', 'req_7a2d4c18-e95', 'req_7a3f0c26-d81',
  'req_7e2b9f40-c81', 'req_7e4f1a9c-f16', 'req_868264bc-611', 'req_8c41f2d7-a93', 'req_8d4b7f2c-f58', 'req_8e1c4a6d-b52', 'req_8e1d5a36-c92', 'req_9a07c4e1-d55', 'req_9b4f1c62-a07', 'req_9c4a7e12-f08', 'req_9d4c1a73-e58',
  'req_9f2e7c5a-c49', 'req_a1f04c2e-7b3', 'req_a2c94e7b-f15', 'req_a3d8f1b6-c25', 'req_a6c19e73-5d2', 'req_a96e3d52-b18', 'req_b1e7c4a9-a69', 'req_b2bed817-a58',
  'req_b72e9d05-c14', 'req_b7d43a90-e26', 'req_b8c3e9d4-e61', 'req_c3a71e58-b46', 'req_c41d0311-200', 'req_c5d8e2b7-e95', 'req_c8f2a61d-b74', 'req_c90d3f61-a28', 'req_d06a4e73-c29', 'req_d27f5a83-e14', 'req_d2a8f6c1-d50',
  'req_d4e8a7b2-916', 'req_da52d141-a5a', 'req_e0a4c6f2-118', 'req_e1b6d4f8-b38', 'req_e2c7a4f9-d13', 'req_e4a92c17-b60', 'req_e5a1c093-4d7', 'req_e83b2d65-c57',
  'req_ef099b34-453', 'req_f15c7a28-d63',
  'req_f1c8e3a5-29d',
  'req_f3b6d218-85a', 'req_f6a1d3e8-e47', 'req_f8085653-422',
]);
/** The four-attempt failover whose first account refused a model it does not serve. */
export const REJECTION_STORY_ID = 'req_7015b657-290';
/** The failover whose pick ties two tier mates on Pace, which the runway breaks. */
export const TIE_STORY_ID = 'req_8c41f2d7-a93';
/** The fresh desktop session of that failover. */
const TIED_SESSION_ID = 'a9d3f6b2-4e17-4c58-8b20-6f1e3c9d7a45';
/** The least-bad move onto nightly. */
export const FORCED_MOVE_ID = 'req_5b2e8d47-c19';
/** The conversation the least-bad move takes from the Overflow workspace to nightly. */
const FORCED_SESSION_ID = 'a3e9c5d1-6b2f-4c8e-9d07-5f1a2b3c4d5e';
/** The conversation's next request, held for the head of the least-bad move. */
export const REWRITE_HELD_ID = 'req_5d6f1b3a-e27';
/** The jobs request whose completion crossed research's daily budget. */
export const BUDGET_CROSSING_ID = 'req_7c19e4a0-2f3';
/**
 * Studio's notebook sessions: the two whose first turn after their placement read the account and opened each slow-down
 * span, the morning's slowed one, the slowed one of the hour, and the two the round marks.
 */
export const OPENING_SESSION_IDS = ['6e3b9d1f-4a27-4c58-b8e1-9f2d7a4c3e06', 'c8f4a2e7-5d19-4b36-a7c2-1e9b3d6f8a24'] as const;
export const SLOWED_MORNING_SESSION_ID = 'b4e2c9a7-1f3d-4c86-9a05-7e2d1c8b6f40';
export const SLOWED_SESSION_ID = '5f8a3c1e-9d27-4b6e-8c04-2a7e9b3d1f65';
export const MARKED_SESSION_IDS = ['9c2d7e4a-3b81-4f59-a6e0-1d8c5b7a2e93', 'e7a4b2d9-6c15-4e83-b9f2-0a3d8c6e1b57'] as const;
/** The long-context notebook session that went idle after its one turn, apart from studio's slowed sessions. */
const IDLE_NOTEBOOK_SESSION_ID = '2d8b5f1a-7c4e-4e92-b6a3-8f0d1c9e5a47';
/** That session's one request, the turn the work pool's hour recipe probes after. */
const IDLE_NOTEBOOK_REQUEST_ID = 'req_3c8e1f7a-b29';
/** The desktop session the 5-hour 429 on secondary fails over to primary. */
const FAILED_OVER_SESSION_ID = 'd6f1a3c8-2e94-4b07-8a5d-3c9e7f2b1a64';
/** The conversation the 429 held on nightly for its reset at 13:11. */
const RESET_HELD_SESSION_ID = 'f19a6c2e-8b4d-4e73-a5c0-3d2b7e1f9a08';
/**
 * That conversation's next turn, whose placement finds its row on nightly idle past all's window and not yet purged.
 * The story writes it by hand, since no drawn turn's idle gap falls inside the purge's minute past its pool's window.
 */
const REPLACED_PIN_ID = 'req_6f3e1b84-d29';
/**
 * A desktop conversation whose pin the purge takes twice, written by hand since the drawn rows pick their session from
 * `SESSION_IDS`. Its first turn ends on nightly when the held conversation's does, so both pins reach one deadline, and
 * its later turns each come behind the sweep that takes the pin before them.
 */
const PURGED_SESSION_ID = 'b9e3a1f7-4c28-4d65-8e10-2f7a6c9d3b54';
/**
 * The desktop conversation the Overflow workspace answers before its credit balance runs out at 11:30:12. The story
 * writes its two turns by hand, since no drawn session holds a live pin across the lapse.
 */
const LAPSED_SESSION_ID = 'e8c3a6f1-2d94-4b57-9a1e-6f0d3c7b2a85';
const LAPSED_TURN_ID = 'req_5f2a9c64-b18';
/** That conversation's next turn, whose placement moves the pin off the workspace with its state as the cause. */
const CAUSED_MOVE_ID = 'req_c7e41d93-a26';
/** The desktop conversation the retired workspace answers in the morning, before the operator deletes the account. */
const RETIRED_SESSION_ID = 'c2a7e5f1-8d34-4b96-a0e7-5b3f9d1c6a28';
const RETIRED_TURN_ID = 'req_9e4b2d71-c35';
/** That conversation's next turn, which lands the session as a move off the deleted account. */
const DELETED_MOVE_ID = 'req_d71c5a3e-f48';
/**
 * The first turn of a desktop conversation that the upstream answered 400, so the router released the pin its
 * placement created as the request ended, the release's reason `client_error`.
 */
const CLIENT_ERROR_TURN_ID = 'req_c1e40a95-b28';
const CLIENT_ERROR_SESSION_ID = 'f40a2c7e-9b31-4d58-8e06-1a7c3b5d9e24';
/**
 * A laptop turn of the day before that the relay rejected on secondary, the account it chose, since it could not write
 * the account's id into the body. No attempt went upstream, so the row names no account, while the routing log's
 * placement names secondary.
 */
export const UNAVAILABLE_TURN_ID = 'req_4c1e8b27-d93';
/** A laptop turn of the day before whose upstream answered 502 with a body that names no error type. */
const BARE_502_TURN_ID = 'req_8f5a2d16-c47';
/**
 * A laptop request of the day before whose connection to primary failed before its body went out, which the relay sent
 * once more on primary a second later.
 */
const RESENT_REQUEST_ID = 'req_0e5a7c3b-f92';
/** The laptop session of the relay's 502 and the client's retry of that turn. */
export const CLIENT_SESSION_ID = '7d3e1c20-5b4a-4e8f-9c61-0a2b3c4d5e6f';
// The conversations that keep a round's account pinned from its stamp to the round run on the four sessions below,
// written ids outside `SESSION_IDS`. A drawn row takes its session from that list alone, so no drawn row writes one of
// these conversations' pin keys, and each one holds its account at whichever clock the mock loads at.
/** The desktop conversation that works on Fable on nightly through the hour ahead of its Fable-week round. */
const FABLE_MORNING_SESSION_ID = 'a4c81f6e-2d97-4b35-9e0a-7f3c5d1b8e62';
/** The laptop conversation on Opus whose pin nightly's Fable-week round leaves off its window. */
export const OPUS_MORNING_SESSION_ID = 'e3b8d1f6-4a29-4c71-9d05-6b2a8f3c7e14';
/** The laptop conversation on secondary that stamps its 5-hour window ahead of the 11:00 round, which walks its pin. */
export const CLIMB_SESSION_ID = '5c9e7a31-8d24-4f60-b3a7-1e6d9c2f4b85';
/** The desktop Fable conversation on primary whose demand puts primary's 7-day room under its Fable reservation. */
export const RESERVATION_FABLE_SESSION_ID = 'd3a86b1f-7e42-4c95-a8d0-2b5f9e1c6a73';
/** The desktop session whose cold Sonnet 5.5 pin primary's Fable reservation round marks. */
export const RESERVATION_MARKED_SESSION_ID = 'f6c2a8d4-3b71-4e95-8a0c-5d9e1b7f2c63';
/**
 * The desktop session the mod released whole, its main agent after one turn, written out as its row lists past the
 * drawn ids.
 */
export const RELEASED_SESSION_ID = '3c36d663-d074-42f6-8738-f54da7c9f026';
/** The desktop session the mod ended on nightly, whose row writes its agents' figures out. */
export const FINISHED_SESSION_ID = '9b4e2f71-6c3d-4a85-b0e2-5d7f8a1c3e96';
/**
 * That session's pinned agents, main first, each with its last turn in minutes before the mock's now, the one table its
 * seeds and its row's written figures read.
 */
export const FINISHED_AGENTS = [
  { agentId: 'main', minutes: 21 },
  { agentId: 'a7f3', minutes: 34 },
  { agentId: '5d0e', minutes: 47 },
] as const;
/** The contract seat's batch session on fleet. */
export const FLEET_SESSION_ID = '8b1f4c2e-6a93-4d07-b5e2-3c9a7f1d4e68';
/** The approved Ultra's session on bench, on the 7-day window and the Fable week alone. */
export const WORKSHOP_SESSION_ID = '4a7c2e9b-1d58-4f36-b0e3-7c5a9d2f1e84';
/** The sessions whose pins' cache entries live for an hour: four of studio's notebook sessions and the workshop's. */
const HOUR_ENTRY_SESSION_IDS: ReadonlySet<string> = new Set([MARKED_SESSION_IDS[0], MARKED_SESSION_IDS[1], OPENING_SESSION_IDS[1], SLOWED_SESSION_ID, WORKSHOP_SESSION_ID]);
/** The TTL of the cache entries of a session's pins, as each pin's line carries it in `cacheTtlMs`. The walk's
 *  expiry and the lines both read it, so the fact on an attempt and the line cannot part. */
export const pinCacheTtlOf = (sessionId: string | null): number => (sessionId !== null && HOUR_ENTRY_SESSION_IDS.has(sessionId) ? HOUR : 5 * MINUTE);
/**
 * The ids of the requests whose pin's row carried the keepalive's release at the request. The router states the fact
 * on the attempt and no view reads it, so the mock keeps it beside its rows for its two counts. The story holds one, a
 * laptop subagent's turn of the night: the mod released the agent behind the turn before, and the request is the
 * agent's return. A drawn row's id follows the clock the mock loads at, so it names that turn at the instant the smoke
 * and the browser pass pin.
 */
export const RELEASED_AT_REQUEST: ReadonlySet<string> = new Set(['req_f05195b5-f2f']);
const sessionOf = (prefix: string) => SESSION_IDS.find((id) => id.startsWith(prefix)) ?? prefix;

function storyRow(seed: StorySeed): RequestLog {
  const poolId = keyById.get(seed.keyId)!.poolId;
  const last = seed.attempts[seed.attempts.length - 1];
  const answered = last?.status === 200;
  const usage: Usage = { input: 0, cacheRead: 0, cacheWrite5m: 0, cacheWrite1h: 0, output: 0, ...NO_ANSWER_FACTS, ...(answered ? seed.usage : {}) };
  const fast = fastTurn(seed.keyId, seed.model, seed.kind ?? 'inference');
  const sessionId = seed.session === null ? null : sessionOf(seed.session);
  const kind = seed.kind ?? 'inference';
  // A canceled seed was cut by the router's own stop or the client's leaving, so its attempts end canceled with no stop
  // reason.
  const cut = seed.outcome === 'canceled';
  // An incomplete stream never reached the message_delta that carries the stop reason either, while the ledger still
  // files its attempt as an upstream error, so the stop reason reads a predicate of its own beside the outcome's.
  const stopless = cut || seed.outcome === 'incomplete';
  // The relay decides a refused key and the client gate on the head ahead of its read of the body, so it publishes no
  // marker for either, and their codes name them. A refusal it decides behind that read carries the marker it computed
  // from the body, as an answer does. The diagnostics gate reads the body's own marker, ahead of the pool's policy.
  const beforeRead = seed.refused === true && (seed.errorCode === 'auth_unknown' || seed.errorCode === 'client_not_allowed');
  const ownMarker = seed.cacheMarker ?? (!beforeRead && ownMarkerOf({ classifier: seed.classifier ?? null, kind, poolId, sessionId }));
  const cacheMarker = !beforeRead && markerOf({ kind, poolId, requestClass: seed.requestClass }, ownMarker);
  const source = sourceOf(seed.keyId, poolId, kind, seed.attempts.length > 0, ownMarker);
  const attempts = seed.attempts.map((a, index): Attempt => {
    const windows = oauthWindows(a.accountId, a.at, () => SERVED_FACTOR);
    const oauth = accountById.get(a.accountId)?.kind === 'oauth';
    const usageBased = usageBasedOf(a.accountId);
    // A headless attempt read no head, so it carries none of an answer's facts.
    const answer = a.status === 200 && !a.headless;
    const upstreamIdOf = storyUpstreamIds(seed.id, index);
    return {
      accountId: a.accountId,
      accountKind: kindOf(a.accountId),
      // A failover's leg and a placement after the session's row expired wait for `walkSessionTurns`, which reads where
      // they landed.
      cacheMissReason: !answer || kind === 'count_tokens' || index > 0 || seed.landsAfresh || seed.unfiled ? null : !oauth ? null : seed.pending ? 'pending' : 'none',
      cooldownSet: a.cooldown ? { reason: a.cooldown.reason, until: iso(a.cooldown.until) } : null,
      durationMs: a.ms,
      // Read off the day's rows with the previous size (`walkSessionTurns`).
      entryExpired: null,
      index,
      issuedDigest: null,
      // The ledger files an attempt with no head as a transport failure unless the request ended canceled.
      outcome: cut ? 'canceled' : a.headless ? 'transport_error' : a.status === 200 ? (index === seed.attempts.length - 1 && seed.errorCode ? 'upstream_error' : 'succeeded') : a.status === 429 ? 'rate_limited' : 'upstream_error',
      // A usage-based seat's head names no overage, so its attempt stores no flag, as an API key's does. The ledger
      // fills the flag and the windows from a head alone, so a headless attempt stores neither.
      overageInUse: oauth && !usageBased && !a.headless ? false : null,
      // Read off the day's rows once each stands as it ends (`walkSessionTurns`).
      previousInputTokens: null,
      response: { clearedInputTokens: null, inferenceGeo: answer ? (seed.geo ?? 'us') : null, inputTransformations: null, refusalCategory: null, refusalRecommendedModel: null, servedModel: null, speed: answer ? (fast ? 'fast' : 'standard') : null, stopReason: answer && !stopless ? 'end_turn' : null, stopSequence: null },
      // The routing log's records write each story attempt's trace, so the drawer and the card read one decision.
      selection: null,
      // The tier is read off the answer's usage, which a token count's answer carries none of.
      serviceTier: answer && kind !== 'count_tokens' ? (seed.tier ?? 'standard') : null,
      startedAt: iso(a.at),
      tier: accountTier(a.accountId),
      upstreamMessageId: answer ? upstreamIdOf('msg_01') : null,
      // The upstream's request id rides on its head.
      upstreamRequestId: a.headless ? null : upstreamIdOf('req_011C'),
      upstreamStatus: a.headless ? null : (a.status ?? null),
      usageBased,
      // A head reads the windows as they stand, or the 5-hour reading the seed gives where the answer moved past it.
      windows: a.headless ? [] : windows.map((w) => (w.id === '5h' && a.head5h !== undefined ? { ...w, utilization: a.head5h } : w)),
    };
  });
  const durationMs = seed.attempts.length ? last.at + last.ms - seed.attempts[0].at + (seed.held?.heldMs ?? 0) : null;
  // The head lands 640 ms into the serving attempt and the first content 270 ms after it, each held under the finish.
  const headRoom = durationMs === null ? 0 : durationMs - STREAM_TAIL;
  // A request the relay answered with an error of its own forwarded no head, whatever the upstream's head was, and a
  // headless cut had none to forward.
  const ttfbMs = answered && !last.headless && (seed.status ?? 200) === 200 && kind !== 'count_tokens' ? Math.min(last.at - seed.at + 640, headRoom) : null;
  const ttftMs = ttfbMs === null ? null : kind === 'inference' ? Math.min(ttfbMs + 270, headRoom) : ttfbMs;
  const status = last?.headless ? null : (seed.status ?? last?.status ?? 429);
  return {
    ...REQUEST_SAMPLE,
    // The account whose answer the request ended on, a refusal's as much as a success's, as a drawn row reads it.
    accountId: last?.accountId ?? null,
    agentId: seed.agentId,
    agentType: seed.agentId === null || seed.agentId === 'main' ? null : (storyAgentsOf(sessionId).find((a) => a.id === seed.agentId)?.type ?? 'general-purpose'),
    at: iso(seed.at),
    attempts,
    autoMode: null,
    cacheMarker,
    canonicalModel: seed.canonical ?? seed.model,
    capturedBodies: seed.capturedBodies ?? seed.keyId === 'laptop',
    classifier: seed.classifier ?? null,
    client: seed.client === undefined ? (CLIENTS[seed.keyId] ?? null) : seed.client,
    clientContract: seed.clientContract ?? null,
    clientDrift: seed.clientDrift ?? null,
    compaction: null,
    contextCompacted: seed.contextCompacted ?? null,
    costUsd: !answered || freeCall(seed.classifier ?? null, attempts[attempts.length - 1] ?? null) ? 0 : round(costOf(seed.model, usage, fast, seed.at), 4),
    diagnosticsSource: source,
    durationMs,
    errorCode: seed.errorCode ?? (status === 200 ? null : 'rate_limit_error'),
    fallbackDigest: null,
    forkDepth: null,
    forkedFrom: null,
    guardrailId: seed.guardrails?.find((m) => m.blocked)?.id ?? null,
    guardrails: seed.guardrails ?? [],
    held: seed.held ?? null,
    id: seed.id,
    keyGeneration: Date.parse(keyById.get(seed.keyId)!.createdAt),
    keyId: seed.keyId,
    kind,
    model: seed.model,
    outcome: seed.outcome ?? (answered ? 'succeeded' : 'failed'),
    poolId,
    prevToolMs: null,
    refused: seed.refused ?? false,
    requestClass: seed.requestClass,
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
function storySeeds(): StorySeed[] {
  const t = clock;
  const firstHour = startOfHour(NOW) - 24 * HOUR;
  const rateLimited = (until: number) => ({ reason: '429 on 5h window', until });
  // A 429 whose head rejects no plan window cools its account for the retry-after alone.
  const retryAfter = (answeredAt: number) => ({ reason: '429, retry after 20 s', until: answeredAt + 20_000 });
  const finishedTurn = (agentId: string, minutes: number, ms: number, id: string, usage: Partial<Usage>): StorySeed => ({
    agentId,
    at: NOW - minutes * MINUTE,
    attempts: [{ accountId: 'nightly', at: NOW - minutes * MINUTE, ms, status: 200 }],
    id,
    keyId: 'desktop',
    model: 'claude-opus-5',
    requestClass: agentId === 'main' ? 'main' : 'subagent',
    session: FINISHED_SESSION_ID,
    unseeded: true,
    usage,
  });
  // The job's earlier runner, a raw Messages client the gate refuses before reading its body: the row carries
  // the code, the user agent and the label, no model and no session, as the relay lands one.
  const BATCH_AGENT = 'phoebis-batch/0.9 (external, ci)';
  return [
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
      cacheMarker: false,
      held: { cohort: 'primary', heldMs: 2_000, waitedOn: ['primary'], waits: [{ heldMs: 2_000, reason: 'cap' }] },
      id: 'req_69e30548-32e',
      keyId: 'desktop',
      model: 'claude-sonnet-5-5',
      requestClass: 'subagent',
      session: '89cd1c92',
      usage: { input: 19_600, output: 820 },
    },
    { agentId: 'main', at: t(14, 19, 2), attempts: [{ accountId: 'primary', at: t(14, 19, 2), ms: 7_100, status: 200 }], errorCode: 'overloaded_error', id: 'req_9a07c4e1-d55', outcome: 'failed', keyId: 'desktop', model: 'claude-sonnet-5-5', requestClass: 'main', session: 'd7167642', usage: { cacheRead: 44_900, cacheWrite1h: 1_800, input: 2_600, output: 960 } },
    // The runner sends again on its key after its revocation at 14:17:30, which the selection refuses. Refused before
    // it resolved a model, the request carries no model, as the router records it.
    { agentId: 'main', at: t(14, 18, 5), attempts: [], errorCode: 'auth_unknown', id: 'req_6c3a9e27-5b1', keyId: 'ci', model: '', outcome: 'rejected', refused: true, requestClass: 'main', session: 'cd9a5904', status: 401 },
    // A compacted conversation renews its pin cold and writes its prefix afresh.
    { agentId: 'main', at: t(14, 17, 52), attempts: [{ accountId: 'primary', at: t(14, 17, 52), ms: 3_300, status: 200 }], contextCompacted: 'auto', errorCode: 'stream_incomplete', id: 'req_31f0b7c2-6e1', outcome: 'incomplete', keyId: 'laptop', model: HAIKU, requestClass: 'main', session: '4ef130f1', usage: { cacheWrite5m: 22_900, input: 1_100, output: 410 } },
    { agentId: 'main', at: t(14, 16, 40), attempts: [], cacheMarker: false, errorCode: 'accounts_exhausted', id: 'req_51d0e2b8-7a0', keyId: 'ci', model: HAIKU, outcome: 'exhausted', requestClass: 'main', session: 'cd9a5904', status: 429 },
    { agentId: null, at: t(14, 15, 48), attempts: [], client: 'other', errorCode: 'client_not_allowed', id: 'req_9b4f1c62-a07', keyId: 'ci', model: '', outcome: 'rejected', refused: true, requestClass: null, session: null, status: 403, userAgent: BATCH_AGENT },
    { agentId: null, at: t(14, 15, 12), attempts: [], client: 'other', errorCode: 'client_not_allowed', id: 'req_2d8e5b13-f40', keyId: 'ci', model: '', outcome: 'rejected', refused: true, requestClass: null, session: null, status: 403, userAgent: BATCH_AGENT },
    { agentId: 'main', at: t(14, 14, 2), attempts: [], clientDrift: 'hash', errorCode: 'client_drift', id: 'req_e0a4c6f2-118', keyId: 'laptop', model: 'claude-sonnet-4-5', outcome: 'rejected', refused: true, requestClass: 'main', session: '5eeed6ce', status: 403, userAgent: 'claude-cli/2.1.271 (external, cli)' },
    // The same client's next request carried no class header, so the contract refused it before any account was chosen.
    { agentId: 'main', at: t(14, 14, 20), attempts: [], clientContract: 'class_header', errorCode: 'client_contract', id: 'req_f1c8e3a5-29d', keyId: 'laptop', model: 'claude-sonnet-4-5', outcome: 'rejected', refused: true, requestClass: null, session: '5eeed6ce', status: 403, userAgent: 'claude-cli/2.1.271 (external, cli)' },
    { agentId: 'main', at: t(14, 11, 44), attempts: [{ accountId: 'primary', at: t(14, 11, 44), ms: 8_300, status: 200 }], id: 'req_b2bed817-a58', keyId: 'desktop', model: 'claude-sonnet-5-5', requestClass: 'main', session: 'd7167642', usage: { cacheRead: 30_900, cacheWrite1h: 21_400, input: 1_900, output: 1_480 } },
    // The model takes no inference geography parameter, so its answer reports the geography as not available.
    { agentId: 'main', at: t(14, 9, 41), attempts: [{ accountId: 'primary', at: t(14, 9, 41), ms: 4_800, status: 200 }], geo: 'not_available', id: 'req_ef099b34-453', keyId: 'laptop', model: HAIKU, requestClass: 'main', session: '4ef130f1', usage: { cacheWrite5m: 31_000, input: 9_800, output: 2_100 } },
    // A token count follows its subagent's pin to secondary before its window is spent, so it never meets the cooldown
    // the 429 at 14:07:50 starts.
    {
      agentId: 'e90b',
      at: t(14, 1, 50),
      attempts: [{ accountId: 'secondary', at: t(14, 1, 50), ms: 300, status: 200 }],
      bodyTokens: 41_000,
      id: 'req_3b8e51f4-0a9',
      keyId: 'desktop',
      kind: 'count_tokens',
      model: 'claude-sonnet-4-5',
      requestClass: 'subagent',
      session: '407dd39b',
    },
    // The keepalive's probe follows its pin to secondary, whose 429 cools the account, and the router then answers the
    // probe its pinned member's cooldown.
    {
      agentId: 'main',
      at: t(14, 6, 30),
      attempts: [{ accountId: 'secondary', at: t(14, 6, 30), cooldown: retryAfter(t(14, 6, 31)), head5h: 1, ms: 1_000, status: 429 }],
      errorCode: 'accounts_exhausted',
      id: 'req_c41d0311-200',
      keyId: 'desktop',
      kind: 'probe',
      model: 'claude-sonnet-4-5',
      outcome: 'rejected',
      requestClass: 'main',
      session: '229bf72c',
      status: 429,
    },
    { agentId: 'main', at: t(14, 5, 12), attempts: [{ accountId: 'primary', at: t(14, 5, 12), ms: 7_900, status: 200 }], id: 'req_da52d141-a5a', keyId: 'desktop', model: 'claude-sonnet-4-5', requestClass: 'main', session: '0d588de5', usage: { cacheRead: 30_100, cacheWrite1h: 14_800, input: 2_200, output: 1_120 } },
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
      model: 'claude-sonnet-4-5',
      requestClass: 'main',
      session: 'bcc805b6',
      usage: { cacheWrite1h: 58_900, input: 4_100, output: 1_310 },
    },
    // The turns that place the hour's sessions on secondary before its spent edge: the pins the beats after them read.
    { agentId: 'main', at: t(14, 1, 40), attempts: [{ accountId: 'secondary', at: t(14, 1, 40), ms: 6_000, status: 200 }], id: 'req_d4e8a7b2-916', keyId: 'desktop', model: 'claude-sonnet-4-5', requestClass: 'main', session: '229bf72c', usage: { cacheWrite1h: 46_000, input: 1_200, output: 520 } },
    { agentId: 'main', at: t(14, 1, 35), attempts: [{ accountId: 'secondary', at: t(14, 1, 35), ms: 12_000, status: 200 }], id: 'req_c90d3f61-a28', keyId: 'desktop', model: 'claude-sonnet-4-5', requestClass: 'main', session: '0d588de5', usage: { cacheWrite1h: 44_600, input: 1_800, output: 870 } },
    { agentId: 'e90b', at: t(14, 1, 30), attempts: [{ accountId: 'secondary', at: t(14, 1, 30), ms: 4_000, status: 200 }], id: 'req_b72e9d05-c14', keyId: 'desktop', model: 'claude-sonnet-4-5', requestClass: 'subagent', session: '407dd39b', usage: { cacheWrite5m: 39_500, input: 1_400, output: 610 } },
    { agentId: 'main', at: t(14, 0, 10), attempts: [{ accountId: 'secondary', at: t(14, 0, 10), ms: 14_000, status: 200 }], id: 'req_a1f04c2e-7b3', keyId: 'desktop', model: 'claude-sonnet-4-5', requestClass: 'main', session: 'bcc805b6', usage: { cacheWrite1h: 58_900, input: 2_300, output: 1_040 } },
    { agentId: 'e90b', at: t(11, 0), attempts: [{ accountId: 'secondary', at: t(11, 0), ms: 6_200, status: 200 }], id: 'req_1eaa9bc1-a18', keyId: 'desktop', model: 'claude-sonnet-4-5', requestClass: 'subagent', session: '407dd39b', usage: { cacheRead: 40_800, cacheWrite5m: 1_200, input: 1_700, output: 900 } },
    { agentId: 'e90b', at: t(11, 8, 30), attempts: [{ accountId: 'primary', at: t(11, 8, 30), ms: 9_400, status: 200 }], id: 'req_868264bc-611', keyId: 'desktop', model: 'claude-sonnet-4-5', requestClass: 'subagent', session: 'c64358b5', usage: { cacheWrite5m: 72_000, input: 3_000, output: 1_600 } },
    // A laptop conversation's three turns on secondary, each inside all's affinity window of the one ahead. The second
    // is the first selection that reads secondary's 5-hour window over all's ceiling, which stamps it a wait ahead of
    // the 11:00 round. The third runs to 10:50:08, so the conversation's pin stands on secondary at every purge sweep
    // from the stamp to the round, which walks it.
    { agentId: 'main', at: t(10, 29, 40), attempts: [{ accountId: 'secondary', at: t(10, 29, 40), ms: 9_200, status: 200 }], id: 'req_9e6b3d07-a52', keyId: 'laptop', model: 'claude-sonnet-4-5', requestClass: 'main', session: CLIMB_SESSION_ID, unseeded: true, usage: { cacheWrite1h: 58_900, input: 1_900, output: 880 } },
    { agentId: 'main', at: t(10, 39, 30), attempts: [{ accountId: 'secondary', at: t(10, 39, 30), ms: 8_600, status: 200 }], id: 'req_5f2c8b19-d73', keyId: 'laptop', model: 'claude-sonnet-4-5', requestClass: 'main', session: CLIMB_SESSION_ID, unseeded: true, usage: { cacheRead: 60_800, cacheWrite1h: 900, input: 700, output: 720 } },
    { agentId: 'main', at: t(10, 49, 10), attempts: [{ accountId: 'secondary', at: t(10, 49, 10), ms: 58_000, status: 200 }], id: 'req_b7d43a90-e26', keyId: 'laptop', model: 'claude-sonnet-4-5', requestClass: 'main', session: CLIMB_SESSION_ID, unseeded: true, usage: { cacheRead: 62_400, cacheWrite1h: 900, input: 700, output: 3_100 } },
    // The subagent turns that place two of the round's sessions on secondary just before it, and the keepalive's probe
    // of the first, which renews its pin without walking past a rule.
    { agentId: 'e90b', at: t(10, 59, 30), attempts: [{ accountId: 'secondary', at: t(10, 59, 30), ms: 900, status: 200 }], id: 'req_0b7c2d94-e61', keyId: 'desktop', kind: 'probe', model: 'claude-sonnet-4-5', requestClass: 'subagent', session: '407dd39b', usage: { cacheRead: 40_800, output: 1 } },
    { agentId: 'e90b', at: t(10, 59, 10), attempts: [{ accountId: 'secondary', at: t(10, 59, 10), ms: 7_000, status: 200 }], id: 'req_f3b6d218-85a', keyId: 'desktop', model: 'claude-sonnet-4-5', requestClass: 'subagent', session: 'c64358b5', usage: { cacheWrite5m: 72_000, input: 2_600, output: 1_300 } },
    { agentId: 'e90b', at: t(10, 58, 59), attempts: [{ accountId: 'secondary', at: t(10, 58, 59), ms: 3_000, status: 200 }], id: 'req_e5a1c093-4d7', keyId: 'desktop', model: 'claude-sonnet-4-5', requestClass: 'subagent', session: '407dd39b', usage: { cacheWrite5m: 40_800, input: 1_500, output: 700 } },
    // The first turn of the session's subagent f2c8, placed on secondary in the minute before the 11:00 round and still
    // running through it, so the round reads its pin with no prompt size and no burn. It reads no cache, so the rollups'
    // draws for the hour stay where they were.
    { agentId: 'f2c8', at: t(10, 59, 40), attempts: [{ accountId: 'secondary', at: t(10, 59, 40), ms: 30_000, status: 200 }], id: 'req_f2c8d417-a60', keyId: 'desktop', model: 'claude-sonnet-4-5', requestClass: 'subagent', session: '407dd39b', unseeded: true, usage: { cacheWrite5m: 24_300, input: 1_900, output: 1_150 } },
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
      canonical: 'claude-sonnet-4-6',
      id: REJECTION_STORY_ID,
      keyId: 'desktop',
      model: 'claude-sonnet-4-6',
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
      model: 'claude-sonnet-4-5',
      outcome: 'failed',
      requestClass: 'main',
      session: CLIENT_SESSION_ID,
      status: 502,
    },
    // The client's retry of that turn: primary's head arrived and went out under its 200, and the body ended short
    // where the relay could not decode the stream past what it had forwarded, so the request is incomplete.
    {
      agentId: 'main',
      at: t(14, 22, 53),
      attempts: [{ accountId: 'primary', at: t(14, 22, 53), ms: 2_400, status: 200 }],
      errorCode: 'stream_undecodable',
      id: 'req_5e3c8a41-d27',
      keyId: 'laptop',
      model: 'claude-sonnet-4-5',
      outcome: 'incomplete',
      outputUnreported: true,
      requestClass: 'main',
      session: CLIENT_SESSION_ID,
      usage: { input: 3_200 },
    },
    // A Fable turn on primary's plan whose bodies were captured, so the drawer reads a plan-funded answer of the model.
    { agentId: 'main', at: t(14, 23, 30), attempts: [{ accountId: 'primary', at: t(14, 23, 30), ms: 8_200, status: 200 }], capturedBodies: true, id: 'req_a6c19e73-5d2', keyId: 'desktop', model: 'claude-fable-5-1', requestClass: 'main', session: '4ef130f1', usage: { cacheRead: 52_300, cacheWrite1h: 3_400, input: 2_100, output: 1_650, thinking: 640 } },
    // The main agent's one turn in the session the mod released whole later. The release ends the keepalive record
    // alone, so the pin this turn wrote stays.
    { agentId: 'main', at: t(14, 3), attempts: [{ accountId: 'primary', at: t(14, 3), ms: 5_600, status: 200 }], id: 'req_7a2d4c18-e95', keyId: 'desktop', model: 'claude-sonnet-5', requestClass: 'main', session: RELEASED_SESSION_ID, usage: { cacheWrite1h: 15_800, input: 2_300, output: 960 } },
    // A long conversation's turn the Overflow workspace answers, and its next turn once the cache there has gone cold,
    // which returns it to a plan: primary fits but is full and the rest fall short of the ceiling with its 640k-token
    // rewrite, so the move lands on the least bad one, and nightly's window is spent within the hour. The first turn
    // thinks, so the move has a block to drop.
    { agentId: 'main', at: t(10, 52, 30), attempts: [{ accountId: 'overflow', at: t(10, 52, 30), ms: 14_000, status: 200 }], id: 'req_4e8b1d36-f72', keyId: 'desktop', model: 'claude-sonnet-5-5', requestClass: 'main', session: FORCED_SESSION_ID, usage: { cacheRead: 612_000, cacheWrite5m: 24_000, input: 2_800, output: 1_900, thinking: 700 } },
    { agentId: 'main', at: t(11, 1, 50), attempts: [{ accountId: 'nightly', at: t(11, 1, 50), ms: 21_000, status: 200 }], id: FORCED_MOVE_ID, keyId: 'desktop', model: 'claude-sonnet-5-5', requestClass: 'main', session: FORCED_SESSION_ID, usage: { cacheWrite1h: 640_000, input: 3_100, output: 2_400 } },
    // The conversation's next request a second later waits on nightly for the move's head, which comes a second on,
    // and then reads the prefix the move wrote.
    {
      agentId: 'main',
      at: t(11, 1, 51),
      attempts: [{ accountId: 'nightly', at: t(11, 1, 52), ms: 9_800, status: 200 }],
      held: { cohort: null, heldMs: 1_000, waitedOn: ['nightly'], waits: [{ heldMs: 1_000, reason: 'rewrite' }] },
      id: REWRITE_HELD_ID,
      keyId: 'desktop',
      model: 'claude-sonnet-5-5',
      requestClass: 'main',
      session: FORCED_SESSION_ID,
      usage: { cacheRead: 643_100, cacheWrite1h: 1_900, input: 1_200, output: 1_050 },
    },
    // A new conversation lands on nightly 40 s before its spent window resets, since primary is full. The 429 that
    // answers cools nightly to the reset, and the spent rule holds the request in place for it rather than move the
    // pin, so it renews on the fresh window.
    {
      agentId: 'main',
      at: t(13, 10, 20),
      attempts: [
        { accountId: 'nightly', at: t(13, 10, 20), cooldown: rateLimited(t(13, 11)), head5h: 1, ms: 900, status: 429 },
        { accountId: 'nightly', at: t(13, 11), ms: 9_000, status: 200 },
      ],
      held: { cohort: null, heldMs: 39_000, waitedOn: ['nightly'], waits: [{ heldMs: 39_000, reason: 'reset' }] },
      id: 'req_9d4c1a73-e58',
      keyId: 'desktop',
      model: 'claude-sonnet-5-5',
      requestClass: 'main',
      session: RESET_HELD_SESSION_ID,
      usage: { cacheWrite1h: 41_200, input: 1_800, output: 1_350 },
    },
    // A Fable conversation's morning on nightly, ahead of the Fable session's own. Its first turn lands there while
    // nightly's three siblings sit at their in-flight caps. Its second is the first selection that reads nightly's
    // 7-day Fable window over all's weekly ceiling, which stamps the window, and each turn after it comes inside all's
    // affinity window, so its pin stands on nightly at every purge sweep until the Opus conversation below lands
    // there.
    { agentId: 'main', at: t(7, 20), attempts: [{ accountId: 'nightly', at: t(7, 20), ms: 10_400, status: 200 }], id: 'req_9c4a7e12-f08', keyId: 'desktop', model: 'claude-fable-5-1', requestClass: 'main', session: FABLE_MORNING_SESSION_ID, unseeded: true, usage: { cacheWrite1h: 69_200, input: 2_500, output: 1_540, thinking: 480 } },
    { agentId: 'main', at: t(7, 29, 30), attempts: [{ accountId: 'nightly', at: t(7, 29, 30), ms: 8_800, status: 200 }], id: 'req_4b9e2c71-a35', keyId: 'desktop', model: 'claude-fable-5-1', requestClass: 'main', session: FABLE_MORNING_SESSION_ID, unseeded: true, usage: { cacheRead: 71_700, cacheWrite1h: 2_300, input: 2_200, output: 1_310, thinking: 450 } },
    { agentId: 'main', at: t(7, 39, 20), attempts: [{ accountId: 'nightly', at: t(7, 39, 20), ms: 9_100, status: 200 }], id: 'req_d27f5a83-e14', keyId: 'desktop', model: 'claude-fable-5-1', requestClass: 'main', session: FABLE_MORNING_SESSION_ID, unseeded: true, usage: { cacheRead: 74_000, cacheWrite1h: 2_100, input: 2_000, output: 1_260, thinking: 430 } },
    { agentId: 'main', at: t(7, 49, 10), attempts: [{ accountId: 'nightly', at: t(7, 49, 10), ms: 8_600, status: 200 }], id: 'req_6a1c9e47-b90', keyId: 'desktop', model: 'claude-fable-5-1', requestClass: 'main', session: FABLE_MORNING_SESSION_ID, unseeded: true, usage: { cacheRead: 76_100, cacheWrite1h: 1_900, input: 2_100, output: 1_190, thinking: 410 } },
    { agentId: 'main', at: t(7, 59), attempts: [{ accountId: 'nightly', at: t(7, 59), ms: 9_300, status: 200 }], id: 'req_e83b2d65-c57', keyId: 'desktop', model: 'claude-fable-5-1', requestClass: 'main', session: FABLE_MORNING_SESSION_ID, unseeded: true, usage: { cacheRead: 78_000, cacheWrite1h: 2_200, input: 2_300, output: 1_340, thinking: 460 } },
    { agentId: 'main', at: t(8, 8, 50), attempts: [{ accountId: 'nightly', at: t(8, 8, 50), ms: 9_000, status: 200 }], id: 'req_c8f2a61d-b74', keyId: 'desktop', model: 'claude-fable-5-1', requestClass: 'main', session: FABLE_MORNING_SESSION_ID, unseeded: true, usage: { cacheRead: 80_200, cacheWrite1h: 2_000, input: 2_200, output: 1_280, thinking: 440 } },
    // A laptop conversation's one Opus turn on nightly, ahead of the sweep that takes the Fable conversation's pin. It
    // runs to 08:20:20, so its pin stands there at every purge sweep to the round of 08:30 and the router keeps the
    // Fable week's stamp, and the round finds the pin and leaves it off that window.
    { agentId: 'main', at: t(8, 19, 5), attempts: [{ accountId: 'nightly', at: t(8, 19, 5), ms: 75_000, status: 200 }], id: 'req_f4a17c3e-d28', keyId: 'laptop', model: 'claude-opus-5', requestClass: 'main', session: OPUS_MORNING_SESSION_ID, unseeded: true, usage: { cacheWrite1h: 58_400, input: 2_600, output: 2_900, thinking: 1_200 } },
    // Two turns of nightly's Fable session in the morning, before its overage: the first places the session, and the
    // second's renewal runs the round its 7-day Fable window fires while the 5-hour window is under the hold.
    { agentId: 'main', at: t(8, 21, 40), attempts: [{ accountId: 'nightly', at: t(8, 21, 40), ms: 11_200, status: 200 }], id: 'req_3e2b9d47-c18', keyId: 'desktop', model: 'claude-fable-5-1', requestClass: 'main', session: '89cd1c92', unfiled: true, usage: { cacheWrite1h: 78_400, input: 2_600, output: 1_720, thinking: 610 } },
    { agentId: 'main', at: t(8, 30), attempts: [{ accountId: 'nightly', at: t(8, 30), ms: 9_400, status: 200 }], id: 'req_3e7a5c19-f84', keyId: 'desktop', model: 'claude-fable-5-1', requestClass: 'main', session: '89cd1c92', usage: { cacheRead: 81_000, cacheWrite1h: 2_600, input: 2_400, output: 1_480, thinking: 520 } },
    // A fresh session placed by the spare rate: secondary falls short of the ceiling and its burn outruns its pace.
    { agentId: 'main', at: t(13, 45), attempts: [{ accountId: 'primary', at: t(13, 45), ms: 3_800, status: 200 }], id: 'req_2c7d9f15-b63', keyId: 'laptop', model: HAIKU, requestClass: 'main', session: 'b81e4f2a-7c3d-4a90-8e65-2d1f0c9b7a34', usage: { cacheWrite5m: 24_500, input: 2_900, output: 640 } },
    // A fresh placement whose reservation the answer's usage replaces.
    { agentId: 'main', at: t(13, 41, 15), attempts: [{ accountId: 'primary', at: t(13, 41, 15), ms: 7_200, status: 200 }], id: 'req_7a3f0c26-d81', keyId: 'desktop', model: 'claude-sonnet-5-5', requestClass: 'main', session: 'c5d2e8a1-4f6b-4b17-a9c3-6e0d2f8b1a57', usage: { cacheWrite1h: 38_400, input: 2_600, output: 1_150 } },
    // A renewal on primary whose round finds no sibling with runway for either pin. It follows the placement of
    // 14:11:44 on the same account, so its member names a message primary holds, and the upstream's comparison was
    // still running when its answer went out.
    { agentId: 'main', at: t(14, 12, 30), attempts: [{ accountId: 'primary', at: t(14, 12, 30), ms: 5_600, status: 200 }], id: 'req_6e1f3b98-a24', keyId: 'desktop', model: 'claude-sonnet-5-5', pending: true, requestClass: 'main', session: 'd7167642', usage: { cacheRead: 52_300, cacheWrite1h: 900, input: 2_100, output: 870 } },
    // A desktop session placed on secondary before its spent edge, whose next turn secondary answers with a 429 on the
    // spent 5-hour window while primary is full, so the turn fails over to primary.
    { agentId: 'main', at: t(14, 1, 45), attempts: [{ accountId: 'secondary', at: t(14, 1, 45), ms: 5_000, status: 200 }], id: 'req_d2a8f6c1-d50', keyId: 'desktop', model: 'claude-sonnet-4-5', requestClass: 'main', session: FAILED_OVER_SESSION_ID, usage: { cacheWrite1h: 52_000, input: 1_600, output: 700 } },
    {
      agentId: 'main',
      at: t(14, 7, 50),
      attempts: [
        { accountId: 'secondary', at: t(14, 7, 50), cooldown: rateLimited(t(14, 55)), head5h: 1, ms: 900, status: 429 },
        { accountId: 'primary', at: t(14, 7, 51), ms: 7_400, status: 200 },
      ],
      id: 'req_b8c3e9d4-e61',
      keyId: 'desktop',
      model: 'claude-sonnet-4-5',
      requestClass: 'main',
      session: FAILED_OVER_SESSION_ID,
      usage: { cacheWrite1h: 54_800, input: 1_900, output: 1_020 },
    },
    // Studio's sessions in work, each 5-hour reading the story's own. The closed span: a session placed at 09:11:24
    // whose first turn after the placement reads the account and opens the span in the last 77 minutes of the window
    // that resets at 10:29.
    { agentId: 'main', at: t(9, 11, 24), attempts: [{ accountId: 'studio', at: t(9, 11, 24), head5h: 0.7987, ms: 6_000, status: 200 }], id: 'req_5c7e2a9d-b14', keyId: 'notebook', model: 'claude-sonnet-5-5', requestClass: 'main', session: OPENING_SESSION_IDS[0], unfiled: true, usage: { cacheWrite1h: 33_000, input: 1_900, output: 820 } },
    { agentId: 'main', at: t(9, 12), attempts: [{ accountId: 'studio', at: t(9, 12), head5h: 0.8, ms: 5_800, status: 200 }], id: 'req_a3d8f1b6-c25', keyId: 'notebook', model: 'claude-sonnet-5-5', requestClass: 'main', session: OPENING_SESSION_IDS[0], usage: { cacheRead: 34_900, cacheWrite1h: 1_600, input: 1_500, output: 640 } },
    // The morning session: its placement's head reads room for a fresh session to the 10:29 reset, so the engine
    // keeps it on studio over the annex. Its second turn after the placement waits 36 s for the slow-down and then
    // renews on its pin, and its turns after the reset read the account at full speed again.
    { agentId: 'main', at: t(10, 11), attempts: [{ accountId: 'studio', at: t(10, 11), head5h: 0.905, ms: 6_200, status: 200 }], id: 'req_3d7b9e2f-a41', keyId: 'notebook', model: 'claude-sonnet-5-5', requestClass: 'main', session: SLOWED_MORNING_SESSION_ID, usage: { cacheWrite1h: 31_000, input: 1_800, output: 760 } },
    { agentId: 'main', at: t(10, 11, 36), attempts: [{ accountId: 'studio', at: t(10, 11, 36), head5h: 0.9345, ms: 4_900, status: 200 }], id: 'req_2b9e4c7f-d36', keyId: 'notebook', model: 'claude-sonnet-5-5', requestClass: 'main', session: SLOWED_MORNING_SESSION_ID, usage: { cacheRead: 32_800, cacheWrite1h: 700, input: 1_100, output: 520 } },
    {
      agentId: 'main',
      at: t(10, 12),
      attempts: [{ accountId: 'studio', at: t(10, 12, 36), head5h: 0.9377, ms: 5_400, status: 200 }],
      held: { cohort: null, heldMs: 36_000, waitedOn: ['studio'], waits: [{ heldMs: 36_000, reason: 'slow_down' }] },
      id: 'req_8e1c4a6d-b52',
      keyId: 'notebook',
      model: 'claude-sonnet-5-5',
      requestClass: 'main',
      session: SLOWED_MORNING_SESSION_ID,
      usage: { cacheRead: 34_600, cacheWrite1h: 1_200, input: 1_400, output: 690 },
    },
    { agentId: 'main', at: t(10, 38, 20), attempts: [{ accountId: 'studio', at: t(10, 38, 20), head5h: 0.01, ms: 5_600, status: 200 }], id: 'req_f6a1d3e8-e47', keyId: 'notebook', model: 'claude-sonnet-5-5', requestClass: 'main', session: SLOWED_MORNING_SESSION_ID, usage: { cacheRead: 37_200, cacheWrite1h: 1_300, input: 1_500, output: 710 } },
    { agentId: 'main', at: t(10, 40, 36), attempts: [{ accountId: 'studio', at: t(10, 40, 36), head5h: 0.02, ms: 5_000, status: 200 }], id: 'req_8d4b7f2c-f58', keyId: 'notebook', model: 'claude-sonnet-5-5', requestClass: 'main', session: SLOWED_MORNING_SESSION_ID, usage: { cacheRead: 40_000, cacheWrite1h: 900, input: 1_200, output: 560 } },
    // The open span: a session placed at 13:47:24, whose head reads room for it to the 15:29 reset and whose first turn
    // after the placement reads the account and opens the span, then the two the round marks, then the slowed one's
    // main and subagent turns.
    { agentId: 'main', at: t(13, 47, 24), attempts: [{ accountId: 'studio', at: t(13, 47, 24), head5h: 0.822, ms: 6_900, status: 200 }], id: 'req_b1e7c4a9-a69', keyId: 'notebook', model: 'claude-sonnet-5-5', requestClass: 'main', session: OPENING_SESSION_IDS[1], usage: { cacheWrite1h: 48_600, input: 2_200, output: 930 } },
    { agentId: 'main', at: t(13, 48), attempts: [{ accountId: 'studio', at: t(13, 48), head5h: 0.8267, ms: 5_200, status: 200 }], id: 'req_4f2a8d6e-b7a', keyId: 'notebook', model: 'claude-sonnet-5-5', requestClass: 'main', session: OPENING_SESSION_IDS[1], usage: { cacheRead: 50_800, input: 1_200, output: 610 } },
    { agentId: 'main', at: t(13, 50, 30), attempts: [{ accountId: 'studio', at: t(13, 50, 30), head5h: 0.832, ms: 6_800, status: 200 }], id: 'req_6b3e8d1a-c73', keyId: 'notebook', model: 'claude-sonnet-5-5', requestClass: 'main', session: MARKED_SESSION_IDS[0], usage: { cacheWrite1h: 36_000, input: 2_000, output: 820 } },
    { agentId: 'main', at: t(13, 53, 10), attempts: [{ accountId: 'studio', at: t(13, 53, 10), head5h: 0.8376, ms: 5_900, status: 200 }], id: 'req_2f9a6c4e-d84', keyId: 'notebook', model: 'claude-sonnet-5-5', requestClass: 'main', session: MARKED_SESSION_IDS[1], usage: { cacheWrite1h: 28_000, input: 2_000, output: 640, thinking: 480 } },
    { agentId: 'main', at: t(13, 58, 40), attempts: [{ accountId: 'studio', at: t(13, 58, 40), head5h: 0.8491, ms: 7_300, status: 200 }], id: 'req_c5d8e2b7-e95', keyId: 'notebook', model: 'claude-sonnet-5-5', requestClass: 'main', session: SLOWED_SESSION_ID, usage: { cacheWrite1h: 64_000, input: 2_400, output: 910 } },
    { agentId: '7b2e', at: t(13, 59, 5), attempts: [{ accountId: 'studio', at: t(13, 59, 5), head5h: 0.85, ms: 3_100, status: 200 }], canonical: HAIKU, id: 'req_7e4f1a9c-f16', keyId: 'notebook', model: 'claude-haiku-4-5', requestClass: 'subagent', session: SLOWED_SESSION_ID, usage: { cacheWrite1h: 58_600, input: 2_400, output: 380 } },
    // The first marked session's next turn reads the stretch past the threshold, which starts the safety wait.
    { agentId: 'main', at: t(14, 2, 30), attempts: [{ accountId: 'studio', at: t(14, 2, 30), head5h: 0.8572, ms: 6_100, status: 200 }], id: 'req_4a9c3e7b-a27', keyId: 'notebook', model: 'claude-sonnet-5-5', requestClass: 'main', session: MARKED_SESSION_IDS[0], usage: { cacheRead: 36_000, cacheWrite1h: 2_500, input: 1_500, output: 700 } },
    { agentId: 'main', at: t(14, 3, 36), attempts: [{ accountId: 'studio', at: t(14, 3, 36), head5h: 0.8584, ms: 6_600, status: 200 }], id: 'req_e1b6d4f8-b38', keyId: 'notebook', model: 'claude-sonnet-5-5', requestClass: 'main', session: SLOWED_SESSION_ID, usage: { cacheRead: 64_000, cacheWrite1h: 3_200, input: 1_800, output: 880 } },
    // The slowed request: its pin's cycle is 42 s, so at half speed it waits 42 s before it is forwarded, and its
    // renewal runs the round the stretch fires once the safety wait has passed.
    {
      agentId: 'main',
      at: t(14, 4, 18),
      attempts: [{ accountId: 'studio', at: t(14, 5), head5h: 0.86, ms: 7_000, status: 200 }],
      held: { cohort: null, heldMs: 42_000, waitedOn: ['studio'], waits: [{ heldMs: 42_000, reason: 'slow_down' }] },
      id: 'req_9f2e7c5a-c49',
      keyId: 'notebook',
      model: 'claude-sonnet-5-5',
      requestClass: 'main',
      session: SLOWED_SESSION_ID,
      usage: { cacheRead: 67_200, cacheWrite1h: 2_800, input: 2_000, output: 940 },
    },
    // A long-context notebook session early in the day, whose one turn writes its prefix on the hour and which never
    // comes back: the turn `idleWarming` probes after.
    {
      agentId: 'main',
      at: NOW - 20 * HOUR,
      attempts: [{ accountId: 'studio', at: NOW - 20 * HOUR, ms: 21_000, status: 200 }],
      id: IDLE_NOTEBOOK_REQUEST_ID,
      keyId: 'notebook',
      model: 'claude-opus-5-5',
      requestClass: 'main',
      session: IDLE_NOTEBOOK_SESSION_ID,
      usage: { cacheWrite1h: 150_000, input: 3_100, output: 1_700 },
    },
    // A desktop Fable conversation's morning on primary, and the pins the reservation round of 11:13:40 lists beside
    // its current one. The conversation's renewal of 10:13:30 is the first selection that reads primary's 7-day room
    // under its Fable reservation, which stamps the test an hour ahead of the round. Each turn after it comes inside
    // all's affinity window and the last runs to 11:04:10, so the conversation's Fable pin stands on primary at every
    // purge sweep from the stamp to the round, which finds it there. The one turn of the session whose Sonnet 5.5 pin
    // the round marks comes 8 min 30 s ahead of it, past the cache's five minutes and inside all's affinity window.
    { agentId: 'main', at: t(10, 4), attempts: [{ accountId: 'primary', at: t(10, 4), ms: 11_800, status: 200 }], id: 'req_2d8f6b14-a79', keyId: 'desktop', model: 'claude-fable-5-1', requestClass: 'main', session: RESERVATION_FABLE_SESSION_ID, unseeded: true, usage: { cacheWrite1h: 40_100, input: 2_300, output: 1_420, thinking: 520 } },
    { agentId: 'main', at: t(10, 13, 30), attempts: [{ accountId: 'primary', at: t(10, 13, 30), ms: 9_600, status: 200 }], id: 'req_8e1d5a36-c92', keyId: 'desktop', model: 'claude-fable-5-1', requestClass: 'main', session: RESERVATION_FABLE_SESSION_ID, unseeded: true, usage: { cacheRead: 42_400, cacheWrite1h: 900, input: 700, output: 1_180, thinking: 440 } },
    { agentId: 'main', at: t(10, 23, 20), attempts: [{ accountId: 'primary', at: t(10, 23, 20), ms: 8_900, status: 200 }], id: 'req_3f8c1d94-b57', keyId: 'desktop', model: 'claude-fable-5-1', requestClass: 'main', session: RESERVATION_FABLE_SESSION_ID, unseeded: true, usage: { cacheRead: 44_000, cacheWrite1h: 900, input: 700, output: 1_090, thinking: 410 } },
    { agentId: 'main', at: t(10, 33, 10), attempts: [{ accountId: 'primary', at: t(10, 33, 10), ms: 9_400, status: 200 }], id: 'req_a2c94e7b-f15', keyId: 'desktop', model: 'claude-fable-5-1', requestClass: 'main', session: RESERVATION_FABLE_SESSION_ID, unseeded: true, usage: { cacheRead: 45_600, cacheWrite1h: 900, input: 700, output: 1_210, thinking: 450 } },
    { agentId: 'main', at: t(10, 43), attempts: [{ accountId: 'primary', at: t(10, 43), ms: 10_200, status: 200 }], id: 'req_d06a4e73-c29', keyId: 'desktop', model: 'claude-fable-5-1', requestClass: 'main', session: RESERVATION_FABLE_SESSION_ID, unseeded: true, usage: { cacheRead: 47_200, cacheWrite1h: 900, input: 700, output: 1_230, thinking: 460 } },
    { agentId: 'main', at: t(10, 52, 50), attempts: [{ accountId: 'primary', at: t(10, 52, 50), ms: 9_000, status: 200 }], id: 'req_71b5f2c8-e40', keyId: 'desktop', model: 'claude-fable-5-1', requestClass: 'main', session: RESERVATION_FABLE_SESSION_ID, unseeded: true, usage: { cacheRead: 48_800, cacheWrite1h: 900, input: 700, output: 1_150, thinking: 430 } },
    { agentId: 'main', at: t(11, 2, 40), attempts: [{ accountId: 'primary', at: t(11, 2, 40), ms: 90_000, status: 200 }], id: 'req_f15c7a28-d63', keyId: 'desktop', model: 'claude-fable-5-1', requestClass: 'main', session: RESERVATION_FABLE_SESSION_ID, unseeded: true, usage: { cacheRead: 50_400, cacheWrite1h: 900, input: 700, output: 6_800, thinking: 5_200 } },
    { agentId: 'main', at: t(11, 5, 10), attempts: [{ accountId: 'primary', at: t(11, 5, 10), ms: 7_400, status: 200 }], id: 'req_a96e3d52-b18', keyId: 'desktop', model: 'claude-sonnet-5-5', requestClass: 'main', session: RESERVATION_MARKED_SESSION_ID, unseeded: true, usage: { cacheWrite5m: 34_000, input: 2_000, output: 980 } },
    // The shed session's next turn inside all's affinity window, which renews the pin the round's move wrote on primary
    // and reads the source the move left on it.
    { agentId: 'e90b', at: t(11, 13, 40), attempts: [{ accountId: 'primary', at: t(11, 13, 40), ms: 5_600, status: 200 }], id: 'req_5d1f8c3a-b27', keyId: 'desktop', model: 'claude-sonnet-4-5', requestClass: 'subagent', session: 'c64358b5', usage: { cacheRead: 75_000, cacheWrite5m: 1_900, input: 1_400, output: 820 } },
    // The failed-over session's next turn inside the window, which renews the pin the failover landed on primary and
    // reads the source the landing left on it.
    { agentId: 'main', at: t(14, 13, 5), attempts: [{ accountId: 'primary', at: t(14, 13, 5), ms: 6_100, status: 200 }], id: 'req_e2c7a4f9-d13', keyId: 'desktop', model: 'claude-sonnet-4-5', requestClass: 'main', session: FAILED_OVER_SESSION_ID, usage: { cacheRead: 56_700, cacheWrite1h: 1_300, input: 1_500, output: 760 } },
    // The two subagent turns the classifier calls below judge, on primary, each ending before its call: a5e1's before
    // 12:41:08 and d8f3's between that call and its own at 12:41:10, so each call's last ended turn is its own agent's.
    // They read no cache, so the rollups' draws for the hour stay where they were.
    { agentId: 'a5e1', at: t(12, 40, 52), attempts: [{ accountId: 'primary', at: t(12, 40, 52), ms: 15_000, status: 200 }], id: JUDGED_TURN_IDS[0], keyId: 'laptop', model: HAIKU, requestClass: 'subagent', session: FORK_SESSION_ID, unseeded: true, usage: { cacheWrite5m: 18_400, input: 2_100, output: 640 } },
    { agentId: 'd8f3', at: t(12, 40, 56), attempts: [{ accountId: 'primary', at: t(12, 40, 56), ms: 13_000, status: 200 }], id: JUDGED_TURN_IDS[1], keyId: 'laptop', model: HAIKU, requestClass: 'subagent', session: FORK_SESSION_ID, unseeded: true, usage: { cacheWrite5m: 16_900, input: 1_800, output: 520 } },
    // Two auto-mode classifier calls of the laptop session the summary forks run in, each judging a subagent's tool
    // call on primary's plan. They read no cache, so the rollups' draws for the hour stay where they were.
    { agentId: 'a5e1', at: t(12, 41, 8), attempts: [{ accountId: 'primary', at: t(12, 41, 8), ms: 1_100, status: 200 }], classifier: 'client', id: 'req_4d9a1e6b-c20', keyId: 'laptop', model: 'claude-sonnet-5', requestClass: 'auxiliary', session: FORK_SESSION_ID, usage: { input: 3_400, output: 14 } },
    { agentId: 'd8f3', at: t(12, 41, 10), attempts: [{ accountId: 'primary', at: t(12, 41, 10), ms: 2_300, status: 200 }], classifier: 'client', id: 'req_63a8d2f5-b17', keyId: 'laptop', model: 'claude-sonnet-5', requestClass: 'auxiliary', session: FORK_SESSION_ID, usage: { input: 3_900, output: 260 } },
    ...guardrailSeeds(),
    // The router's one stop of the day cut two laptop requests as its drain window closed, before it went down at
    // 06:11:58: a stream whose head had gone out, and a request no head had reached yet.
    { agentId: 'main', at: t(6, 11, 55), attempts: [{ accountId: 'primary', at: t(6, 11, 55), ms: 2_900, status: 200 }], errorCode: 'router_shutdown', id: 'req_7a2c4e91-d03', keyId: 'laptop', model: HAIKU, outcome: 'canceled', requestClass: 'main', session: '4ef130f1', usage: { cacheRead: 18_200, input: 900, output: 140 } },
    { agentId: 'main', at: t(6, 11, 57), attempts: [{ accountId: 'primary', at: t(6, 11, 57), headless: true, ms: 400, status: 200 }], capturedBodies: false, errorCode: 'router_shutdown', id: 'req_c58d1f3a-e67', keyId: 'laptop', model: HAIKU, outcome: 'canceled', requestClass: 'main', session: '4ef130f1' },
    // A fresh desktop session the Overflow workspace's static prefix places there, whose 429 fails it over to primary
    // on a tie with nightly the runway breaks.
    {
      agentId: 'main',
      at: t(8, 16, 20),
      attempts: [
        { accountId: 'overflow', at: t(8, 16, 20), cooldown: retryAfter(t(8, 16, 20) + 600), ms: 600, status: 429 },
        { accountId: 'primary', at: t(8, 16, 21), ms: 6_900, status: 200 },
      ],
      id: TIE_STORY_ID,
      keyId: 'desktop',
      model: 'claude-sonnet-5-5',
      requestClass: 'main',
      session: TIED_SESSION_ID,
      usage: { cacheWrite1h: 36_200, input: 2_300, output: 980 },
    },
    // The contract seat's batch workers on fleet, billed in dollars at API rates. The first reads the workers' shared
    // prefix, so no row of the seat counts as a miss between turns.
    { agentId: 'main', at: t(14, 8, 20), attempts: [{ accountId: 'contract', at: t(14, 8, 20), ms: 7_400, status: 200 }], id: 'req_c94d1e07-b35', keyId: 'fleet', model: 'claude-sonnet-5-5', requestClass: 'main', session: FLEET_SESSION_ID, unseeded: true, usage: { cacheRead: 12_400, cacheWrite5m: 6_200, input: 2_900, output: 1_120 } },
    { agentId: 'main', at: t(14, 19, 30), attempts: [{ accountId: 'contract', at: t(14, 19, 30), ms: 5_900, status: 200 }], id: 'req_7b2e5d93-a16', keyId: 'fleet', model: 'claude-sonnet-5-5', requestClass: 'main', session: FLEET_SESSION_ID, unseeded: true, usage: { cacheRead: 18_600, cacheWrite5m: 1_400, input: 1_700, output: 860 } },
    { agentId: 'main', at: t(14, 25, 30), attempts: [{ accountId: 'contract', at: t(14, 25, 30), ms: 11_200, status: 200 }], id: 'req_3f6a9c21-d48', keyId: 'fleet', model: 'claude-opus-5-5', requestClass: 'main', session: FLEET_SESSION_ID, unseeded: true, usage: { cacheRead: 20_000, cacheWrite5m: 2_200, input: 2_400, output: 1_540 } },
    // The approved Ultra's session on bench, a Fable turn and a Sonnet one, priced on its 7-day window and the Fable
    // week alone, since its poll names no 5-hour window.
    { agentId: 'main', at: t(14, 16, 30), attempts: [{ accountId: 'workshop', at: t(14, 16, 30), ms: 9_800, status: 200 }], id: 'req_1c7e4a9d-f52', keyId: 'bench', model: 'claude-fable-5-1', requestClass: 'main', session: WORKSHOP_SESSION_ID, unseeded: true, usage: { cacheWrite1h: 42_600, input: 2_100, output: 1_380 } },
    { agentId: 'main', at: t(14, 24, 10), attempts: [{ accountId: 'workshop', at: t(14, 24, 10), ms: 6_300, status: 200 }], id: 'req_9d3b6f2a-c81', keyId: 'bench', model: 'claude-sonnet-5-5', requestClass: 'main', session: WORKSHOP_SESSION_ID, unseeded: true, usage: { cacheRead: 44_000, cacheWrite1h: 1_900, input: 1_600, output: 720 } },
    // The last turns of the desktop session the mod ended, on nightly at the minutes its written figures date main's
    // and its subagents' last requests, and the one turn of a subagent the mod never pinned there, before the others.
    // Each reads a cached prefix, so none counts as a miss between turns.
    finishedTurn(FINISHED_AGENTS[0].agentId, FINISHED_AGENTS[0].minutes, 5_200, 'req_4c8e2a17-b39', { cacheRead: 13_400, input: 800, output: 240 }),
    finishedTurn(FINISHED_AGENTS[1].agentId, FINISHED_AGENTS[1].minutes, 3_800, 'req_7f3d1b95-e62', { cacheRead: 9_600, input: 700, output: 180 }),
    finishedTurn(FINISHED_AGENTS[2].agentId, FINISHED_AGENTS[2].minutes, 4_100, 'req_a2e9c6d4-f17', { cacheRead: 8_900, input: 600, output: 210 }),
    finishedTurn('d41e', 60, 3_300, 'req_d41e5b80-c26', { cacheRead: 7_200, input: 500, output: 160 }),
    // The held conversation's two turns on primary behind its landing of 13:21:39. The first renews the pin inside
    // all's affinity window and is the first selection that reads primary's 5-hour window over all's ceiling, which
    // stamps it a wait ahead of the afternoon round. The second comes behind the sweep that takes that pin and lands
    // on primary again, so a pin of all stands there at every purge sweep until the failover of 14:02:11.
    { agentId: 'main', at: t(13, 31, 30), attempts: [{ accountId: 'primary', at: t(13, 31, 30), ms: 6_400, status: 200 }], id: 'req_c3a71e58-b46', keyId: 'desktop', model: 'claude-sonnet-5-5', requestClass: 'main', session: RESET_HELD_SESSION_ID, unseeded: true, usage: { cacheRead: 44_100, cacheWrite1h: 1_300, input: 1_400, output: 800 } },
    { agentId: 'main', at: t(13, 52, 30), attempts: [{ accountId: 'primary', at: t(13, 52, 30), ms: 5_900, status: 200 }], id: 'req_7e2b9f40-c81', keyId: 'desktop', model: 'claude-sonnet-5-5', requestClass: 'main', session: RESET_HELD_SESSION_ID, unseeded: true, usage: { cacheRead: 45_400, cacheWrite1h: 1_100, input: 1_200, output: 760 } },
    // The keepalive's two answered probes of the session that failed over to primary at 14:02:11, each inside all's
    // affinity window of the use ahead of it, so the session's pin still stands where its recipe has a third probe in
    // flight at the mock's instant.
    { agentId: 'main', at: t(14, 11, 40), attempts: [{ accountId: 'primary', at: t(14, 11, 40), ms: 900, status: 200 }], id: 'req_6d0b3f85-a14', keyId: 'desktop', kind: 'probe', model: 'claude-sonnet-4-5', requestClass: 'main', session: 'bcc805b6', unseeded: true, usage: { cacheRead: 63_000, output: 1 } },
    { agentId: 'main', at: t(14, 20, 40), attempts: [{ accountId: 'primary', at: t(14, 20, 40), ms: 900, status: 200 }], id: 'req_e4a92c17-b60', keyId: 'desktop', kind: 'probe', model: 'claude-sonnet-4-5', requestClass: 'main', session: 'bcc805b6', unseeded: true, usage: { cacheRead: 63_000, output: 1 } },
    // The parked desktop session's Opus turn on primary, which fast mode serves, 6 min 30 s ahead of the afternoon
    // round, so the round finds the session's fast-lane pin there.
    { agentId: 'main', at: t(14, 6), attempts: [{ accountId: 'primary', at: t(14, 6), ms: 4_300, status: 200 }], id: 'req_b4d92e06-a17', keyId: 'desktop', model: 'claude-opus-5', requestClass: 'main', session: 'd7167642', unseeded: true, usage: { cacheRead: 9_400, input: 1_257, output: 420 } },
    // The last request of each drawn subagent recipe that no row stood for, at the instant its recipe rolled and on
    // the account its line names, so the recipe, the line and the Requests list name one turn. Each is the first row
    // under its pin key inside all's affinity window, so the selection finds no pin and places it.
    { agentId: 'c21e', at: NOW - 34 * MINUTE, attempts: [{ accountId: 'nightly', at: NOW - 34 * MINUTE, ms: 3_900, status: 200 }], id: 'req_1a8d4f63-e27', keyId: 'desktop', model: 'claude-sonnet-5', requestClass: 'subagent', session: 'fa23a6da', unseeded: true, usage: { cacheWrite5m: 14_200, input: 800, output: 230 } },
    { agentId: 'e4b7', at: NOW - 7 * MINUTE, attempts: [{ accountId: 'nightly', at: NOW - 7 * MINUTE, ms: 3_400, status: 200 }], id: 'req_c62e9b05-a41', keyId: 'desktop', model: 'claude-sonnet-5', requestClass: 'subagent', session: 'fa23a6da', unseeded: true, usage: { cacheWrite5m: 12_600, input: 700, output: 210 } },
    { agentId: 'a7f3', at: NOW - 30 * MINUTE, attempts: [{ accountId: 'secondary', at: NOW - 30 * MINUTE, ms: 4_200, status: 200 }], id: 'req_48f1d7a9-b36', keyId: 'desktop', model: 'claude-sonnet-4-5', requestClass: 'subagent', session: '229bf72c', unseeded: true, usage: { cacheWrite5m: 16_800, input: 900, output: 260 } },
    { agentId: 'e90b', at: NOW - 3 * MINUTE - 30_000, attempts: [{ accountId: 'nightly', at: NOW - 3 * MINUTE - 30_000, ms: 2_800, status: 200 }], id: 'req_e07b3c82-d95', keyId: 'laptop', model: HAIKU, requestClass: 'subagent', session: 'c64358b5', unseeded: true, usage: { cacheWrite5m: 9_400, input: 600, output: 170 } },
    { agentId: '4d1a', at: NOW - 31 * MINUTE, attempts: [{ accountId: 'primary', at: NOW - 31 * MINUTE, ms: 8_200, status: 200 }], id: 'req_93a5e6f1-c04', keyId: 'desktop', model: 'claude-fable-5-1', requestClass: 'subagent', session: '7bea4514', unseeded: true, usage: { cacheWrite5m: 21_400, input: 1_100, output: 540 } },
    { agentId: '2f9d', at: NOW - 2 * MINUTE, attempts: [{ accountId: 'primary', at: NOW - 2 * MINUTE, ms: 3_100, status: 200 }], id: 'req_5b0d8e27-f63', keyId: 'desktop', model: 'claude-sonnet-5', requestClass: 'subagent', session: RELEASED_SESSION_ID, unseeded: true, usage: { cacheWrite5m: 11_900, input: 700, output: 200 } },
    { agentId: '8c1a', at: NOW - 6 * MINUTE, attempts: [{ accountId: 'primary', at: NOW - 6 * MINUTE, ms: 3_600, status: 200 }], id: 'req_d74c1a96-e58', keyId: 'desktop', model: 'claude-sonnet-5', requestClass: 'subagent', session: RELEASED_SESSION_ID, unseeded: true, usage: { cacheWrite5m: 13_300, input: 800, output: 240 } },
    // A turn of story 4's Explore subagent on its desktop row, a subagent its recipes never pin, so the row lists it
    // as an earlier agent. It reads a cached prefix, so it counts as no miss between turns.
    { agentId: '0b7f', at: NOW - 40 * MINUTE, attempts: [{ accountId: 'nightly', at: NOW - 40 * MINUTE, ms: 3_600, status: 200 }], id: 'req_0b7f2c58-e43', keyId: 'desktop', model: 'claude-sonnet-5', requestClass: 'subagent', session: '89cd1c92', unseeded: true, usage: { cacheRead: 11_800, input: 600, output: 190 } },
    // A classifier call judging a tool call of the desktop turn that ended on secondary at 14:01:47, sent 53 s later,
    // after secondary's head at 14:02:11 read its 5-hour window spent. The judged account holds no allowance and the
    // judged turn's pin still stands there, so the router places the call by the strategy, on primary's plan tier, off
    // the judged account. It prices from its usage on primary's plan.
    { agentId: 'main', at: t(14, 2, 40), attempts: [{ accountId: 'primary', at: t(14, 2, 40), ms: 1_400, status: 200 }], classifier: 'client_off', id: 'req_7c2e9f41-d03', keyId: 'desktop', model: 'claude-sonnet-5', requestClass: 'auxiliary', session: '0d588de5', unseeded: true, usage: { input: 3_600, output: 22 } },
    // The held conversation's next turn, 630 s after its turn on nightly ended at 13:11:09 and so the row's last use,
    // inside the purge's minute past all's 600 s: the placement finds the row expired, releases it for that and for
    // nightly's overage under all's preference for a plan, and lands on primary, which holds its plan tier, writing the
    // conversation's prefix there afresh.
    { agentId: 'main', at: t(13, 21, 39), attempts: [{ accountId: 'primary', at: t(13, 21, 39), ms: 6_800, status: 200 }], id: REPLACED_PIN_ID, keyId: 'desktop', landsAfresh: true, model: 'claude-sonnet-5-5', requestClass: 'main', session: RESET_HELD_SESSION_ID, unseeded: true, usage: { cacheWrite1h: 42_600, input: 1_500, output: 900 } },
    // The conversation the purge takes twice. Its first turn ends at 13:11:09, the held conversation's end. Its second
    // comes behind the sweep at 13:21:40 that takes that pin and ahead of the wall clock's next minute, finds no row
    // and lands on primary, while its member names the message nightly holds. Its third comes behind the sweep at
    // 13:32:40 that takes the second's pin and lands on primary again, where the cache the second wrote for an hour
    // still stands. The first turn thought on nightly, so both answers on primary drop that block with no move event.
    { agentId: 'main', at: t(13, 11), attempts: [{ accountId: 'nightly', at: t(13, 11), ms: 9_000, status: 200 }], id: 'req_3a9d6e14-b72', keyId: 'desktop', model: 'claude-sonnet-5-5', requestClass: 'main', session: PURGED_SESSION_ID, unseeded: true, usage: { cacheWrite1h: 39_800, input: 1_700, output: 1_020, thinking: 480 } },
    { agentId: 'main', at: t(13, 21, 50), attempts: [{ accountId: 'primary', at: t(13, 21, 50), ms: 6_000, status: 200 }], id: 'req_8b2f4c71-e05', keyId: 'desktop', landsAfresh: true, model: 'claude-sonnet-5-5', requestClass: 'main', session: PURGED_SESSION_ID, unseeded: true, usage: { cacheWrite1h: 41_300, input: 1_500, output: 880 } },
    { agentId: 'main', at: t(13, 33, 10), attempts: [{ accountId: 'primary', at: t(13, 33, 10), ms: 5_000, status: 200 }], id: 'req_d5e17a39-c48', keyId: 'desktop', model: 'claude-sonnet-5-5', requestClass: 'main', session: PURGED_SESSION_ID, unseeded: true, usage: { cacheRead: 41_300, cacheWrite1h: 1_200, input: 1_300, output: 760 } },
    // A new conversation's first turn, which primary's upstream answered 400.
    { agentId: 'main', at: t(12, 58, 14), attempts: [{ accountId: 'primary', at: t(12, 58, 14), ms: 640, status: 400 }], errorCode: 'invalid_request_error', id: CLIENT_ERROR_TURN_ID, keyId: 'desktop', model: 'claude-sonnet-5-5', requestClass: 'main', session: CLIENT_ERROR_SESSION_ID, unseeded: true },
    // A new conversation the Overflow workspace takes with the plan accounts full, while its balance still holds.
    { agentId: 'main', at: t(11, 27, 40), attempts: [{ accountId: 'overflow', at: t(11, 27, 40), ms: 9_200, status: 200 }], id: LAPSED_TURN_ID, keyId: 'desktop', model: 'claude-sonnet-5-5', requestClass: 'main', session: LAPSED_SESSION_ID, unseeded: true, usage: { cacheWrite5m: 36_400, input: 1_900, output: 1_100 } },
    // Its next turn 210 s on, after the balance ran out at 11:30:12 and before the workspace's funding retry falls due
    // five minutes after it. The pin is live and younger than all's return dwell, so no rule moves it. The workspace
    // is out of the eligible set, so the placement moves the pin to primary and writes the conversation's prefix there.
    { agentId: 'main', at: t(11, 31, 10), attempts: [{ accountId: 'primary', at: t(11, 31, 10), ms: 8_600, status: 200 }], id: CAUSED_MOVE_ID, keyId: 'desktop', landsAfresh: true, model: 'claude-sonnet-5-5', requestClass: 'main', session: LAPSED_SESSION_ID, unseeded: true, usage: { cacheWrite1h: 38_200, input: 1_600, output: 940 } },
    // A new conversation the retired workspace takes in the morning with the plan accounts full, inside the span the
    // account stood in all (`RETIRED`).
    { agentId: 'main', at: t(9, 40, 20), attempts: [{ accountId: RETIRED.account.id, at: t(9, 40, 20), ms: 7_800, status: 200 }], id: RETIRED_TURN_ID, keyId: 'desktop', model: 'claude-sonnet-5', requestClass: 'main', session: RETIRED_SESSION_ID, unseeded: true, usage: { cacheWrite5m: 33_600, input: 2_100, output: 1_050 } },
    // Its next turn 430 s on, inside all's affinity window and after the delete. The reload's purge took the pin and
    // kept its trace, so the placement lands the session on primary as a move off the deleted account and writes the
    // conversation's prefix there afresh.
    { agentId: 'main', at: t(9, 47, 30), attempts: [{ accountId: 'primary', at: t(9, 47, 30), ms: 8_100, status: 200 }], id: DELETED_MOVE_ID, keyId: 'desktop', landsAfresh: true, model: 'claude-sonnet-5', requestClass: 'main', session: RETIRED_SESSION_ID, unseeded: true, usage: { cacheWrite1h: 41_800, input: 1_700, output: 980 } },
    // Two laptop turns in the request log's first hour, which the Dashboard's day leaves out. The relay answers the
    // rejection with the 503 it raises on the chosen account's body, and the router files the bare 502 under its
    // status, since the body names no error type.
    { agentId: 'main', at: firstHour + 20 * MINUTE, attempts: [], capturedBodies: false, errorCode: 'accounts_unavailable', id: UNAVAILABLE_TURN_ID, keyId: 'laptop', model: 'claude-sonnet-5-5', outcome: 'rejected', requestClass: 'main', session: 'b3e8f14c-6a27-4d95-8c10-7e2a5d9f3b61', status: 503 },
    { agentId: 'main', at: firstHour + 35 * MINUTE, attempts: [{ accountId: 'primary', at: firstHour + 35 * MINUTE, ms: 1_200, status: 502 }], capturedBodies: false, errorCode: 'upstream_502', id: BARE_502_TURN_ID, keyId: 'laptop', model: 'claude-sonnet-5-5', requestClass: 'main', session: 'e5c27a9d-1f48-4b63-a0d7-9c3e6b2f8a15', unseeded: true },
    // The log's oldest row, in a lane hour of its own, the one turn of a session no other row names, so no later row
    // reads its pin and every seeded draw stays where it was. The relay sends a request that never reached the upstream
    // once more on the same account, a second after a failed connect, under the one decision that chose the account.
    {
      agentId: 'main',
      at: firstHour + 10 * MINUTE,
      attempts: [
        { accountId: 'primary', at: firstHour + 10 * MINUTE, headless: true, ms: 1_000 },
        { accountId: 'primary', at: firstHour + 10 * MINUTE + 1_000, ms: 6_200, status: 200 },
      ],
      capturedBodies: false,
      id: RESENT_REQUEST_ID,
      keyId: 'laptop',
      model: 'claude-sonnet-5-5',
      requestClass: 'main',
      session: '2a6f9d3c-8e41-4b75-a1c0-6d3e9f7b2a58',
      unseeded: true,
      usage: { input: 2_400, output: 610 },
    },
  ];
}
const storyRequests = () => storySeeds().map(storyRow);

/** The laptop session whose subagent the client forks every 30 seconds for a status line. */
const FORK_SESSION_ID = '9e4c2b7a-5d31-4f86-a0b9-3c7e1d5f2a48';
/** That session's two subagent turns the client's two classifier calls judge, `a5e1`'s and `d8f3`'s. */
export const JUDGED_TURN_IDS = ['req_a5e17c42-d19', 'req_d8f3b6e0-a74'] as const;
/** The desktop session whose summary forks the key's own guardrail refused. */
export const BLOCKED_SESSION_ID = '2b8f5e1c-7a46-4d93-b1e0-6c9a3f4d7e25';
/** The pool's guardrail matched first, at its pool position, and the key's own refused the request. */
const BLOCKED_MATCHES: GuardrailMatch[] = [
  { blocked: false, id: 'subagent-status-summary', position: 0 },
  { blocked: true, id: 'large-auxiliary', position: 1 },
];

/**
 * The guardrails' story: the summary forks of a subagent on the laptop key, which the pool's guardrail observes, each a
 * full cached context read for a few words, and three forks on the desktop key that its own guardrail refused over the
 * day, before any account was selected. The forks sit on laptop alone, so a dry run of the desktop key's guardrail
 * reads the three it refused and no fork it let through.
 */
function guardrailSeeds(): StorySeed[] {
  const t = clock;
  const observed: GuardrailMatch[] = [{ blocked: false, id: 'subagent-status-summary', position: 0 }];
  const fork = (at: number, id: string, cacheRead: number): StorySeed => ({
    agentId: 'b7d3',
    at,
    attempts: [{ accountId: 'primary', at, ms: 1_400, status: 200 }],
    capturedBodies: true,
    fork: true,
    guardrails: observed,
    id,
    keyId: 'laptop',
    model: HAIKU,
    requestClass: 'auxiliary',
    session: FORK_SESSION_ID,
    usage: { cacheRead, input: 60, output: 24 },
  });
  const blocked = (at: number, id: string): StorySeed => ({
    agentId: 'c6a2',
    at,
    attempts: [],
    capturedBodies: true,
    errorCode: 'guardrail_blocked',
    fork: true,
    guardrails: BLOCKED_MATCHES,
    id,
    keyId: 'desktop',
    model: 'claude-sonnet-5-5',
    outcome: 'rejected',
    refused: true,
    requestClass: 'auxiliary',
    session: BLOCKED_SESSION_ID,
    status: 403,
  });
  return [
    fork(t(12, 40), 'req_3f7a2c91-d05', 84_200),
    fork(t(12, 40, 30), 'req_8b4e6d12-a37', 84_900),
    fork(t(12, 41), 'req_c25f9e48-b81', 85_600),
    fork(t(12, 41, 30), 'req_e916a3d7-c42', 86_300),
    blocked(t(1, 12, 40), 'req_0c4e7a91-b5d'),
    blocked(t(7, 33, 10), 'req_5d1b8e2f-c64'),
    blocked(t(12, 52, 20), 'req_a8f3c6d0-e17'),
  ];
}

/** The ids of the story's seeds that carry a match, the fork rows and the blocked rows: the one source of a mock match. */
export const GUARDRAIL_STORY_IDS: ReadonlySet<string> = new Set(guardrailSeeds().filter((s) => (s.guardrails ?? []).length > 0).map((s) => s.id));
/** The seeds in the fork's shape, whose captured body is the fork's. */
const FORK_IDS: ReadonlySet<string> = new Set(guardrailSeeds().filter((s) => s.fork).map((s) => s.id));
// Seeded by hand as the routing log's rows are, so the draws after the rows leave them as the story wrote them, and the
// routing log derives no decision for them: a block is refused before any selection.
for (const id of GUARDRAIL_STORY_IDS) STORY_IDS.add(id);
// The rejection is written whole too, so the draws after the rows leave it as the story wrote it. The routing log
// derives the record of a request no attempt reached as an exhaustion, while the router placed this one before the
// relay rejected it, so its record is the story's.
STORY_IDS.add(UNAVAILABLE_TURN_ID);
const STORY_SEEDS = storySeeds();
export const UNSEEDED_IDS: ReadonlySet<string> = new Set(STORY_SEEDS.filter((s) => s.unseeded).map((s) => s.id));
/** The story's rows whose stream ended before the answer reported its output count. */
const OUTPUT_UNREPORTED_IDS: ReadonlySet<string> = new Set(STORY_SEEDS.filter((s) => s.outputUnreported).map((s) => s.id));
/** The story's rows the ledger stored before the relay filed its own words. */
const UNFILED_IDS: ReadonlySet<string> = new Set(STORY_SEEDS.filter((s) => s.unfiled).map((s) => s.id));
/**
 * A row the seeded passes may draw for or seat a story fact on: neither a routing-log story row nor one written whole.
 */
const drawnRow = (r: Pick<RequestLog, 'id'>): boolean => !STORY_IDS.has(r.id) && !UNSEEDED_IDS.has(r.id);

/** A summary fork rides its subagent's pin, so each fork's one attempt renewed the pin the subagent's turns hold. */
function traceForks() {
  for (const r of REQUESTS) {
    if (!FORK_IDS.has(r.id) || r.attempts.length === 0) continue;
    const strategy = poolById.get(r.poolId ?? '')?.strategy ?? 'pace';
    r.attempts = r.attempts.map((a) => ({ ...a, selection: { cacheRead: r.usage.cacheRead, cohortHold: null, moved: null, placement: null, renewed: true, skipped: [], strategy, tier: null } }));
  }
}

export const REQUESTS: RequestLog[] = buildRequests();
export const requestById = new Map(REQUESTS.map((r) => [r.id, r]));
// A drawn row can give the story its slot, so the story's own estimates land after the drawn rows' and stand.
for (const s of STORY_SEEDS) if (s.bodyTokens !== undefined) COUNTED_BODY_TOKENS.set(s.id, s.bodyTokens);

/** The router's pin key: the request's key, session, agent and canonical model. */
export const pinKeyOf = (r: RequestLog) => `${r.keyId}|${r.sessionId}|${r.agentId}|${r.canonicalModel}`;
/** The rows oldest first by arrival, the order the router's pins and a session's turns see them in. */
const byArrival = () => [...REQUESTS].sort((a, b) => Date.parse(a.at) - Date.parse(b.at));

/**
 * The story router's restart, which its event in the routing log and the phase of its purge sweeps both read. The
 * process before the restart swept on the same phase.
 */
export const ROUTER_RESTARTED_AT = clock(6, 12, 40);
/** The first purge sweep strictly after `at`: the router sweeps once a minute from its start, and a sweep keeps a pin
 *  whose age equals its TTL. */
export const sweepAfter = (at: number): number => ROUTER_RESTARTED_AT + (Math.floor((at - ROUTER_RESTARTED_AT) / MINUTE) + 1) * MINUTE;
/** A session's pin as a request's first selection reads it: `live` inside its pool's affinity window,
 *  `expired_idle` past the window and ahead of the sweep that purges the row, `none` behind that sweep, and `none` on
 *  a pool whose TTL is zero, which pins no session. */
type PinRead = 'expired_idle' | 'live' | 'none';
export const pinReadAt = (lastUsedAt: number, ttlMs: number, at: number): PinRead =>
  ttlMs === 0 ? 'none' : at - lastUsedAt <= ttlMs ? 'live' : at < sweepAfter(lastUsedAt + ttlMs) ? 'expired_idle' : 'none';
/** The client errors that release a pin the request's own selection placed, as the relay answers them. */
const RELEASING_STATUSES: readonly (number | null)[] = [400, 401, 403, 404];
/**
 * Whether the relay reports a renewal at the end of a request's last attempt, where `renews` says the pin stood before
 * the request: where the request is no token count and that attempt answered 200, or answered a releasing client error
 * on a pin it did not place. A count sends no report.
 */
const reportsRenewal = (r: Pick<RequestLog, 'attempts' | 'kind'>, renews: boolean): boolean => {
  const last = r.attempts[r.attempts.length - 1];
  return r.kind !== 'count_tokens' && (last.upstreamStatus === 200 || (renews && RELEASING_STATUSES.includes(last.upstreamStatus)));
};
/**
 * The instant a request leaves its pin's last use at, where `renews` says the pin stood before the request: the end of
 * its last attempt where the relay reports a renewal there (`reportsRenewal`), since the router stamps the pin again
 * with that report, and the start of its last attempt otherwise, the selection's own stamp.
 */
export const pinStampOf = (r: Pick<RequestLog, 'attempts' | 'kind'>, renews: boolean): number => {
  const last = r.attempts[r.attempts.length - 1];
  return reportsRenewal(r, renews) ? attemptEnd(last) : Date.parse(last.startedAt);
};

/**
 * The shortest lifetime of a cache entry, the router's and the upstream's: no entry ends inside it, so a turn that
 * reads its cache expired stands at least that far behind the report that wrote the entry.
 */
const SHORTEST_CACHE_LIFETIME = 5 * MINUTE;
/** The rows in the order the streams drew them, which the passes that draw per row walk, so a seated row keeps its
 *  draws. */
const DRAWN_ORDER: readonly RequestLog[] = [...REQUESTS];
/**
 * The drawn turns seated inside their pin's window, by id. A drawn answered turn of one attempt on a pool that pins
 * longer than `SHORTEST_CACHE_LIFETIME`, with no compaction, whose cache read falls under half the size its pin key's
 * last report under the same key holder left on the account it lands on, sits that lifetime behind the report's stamp
 * plus the part of its drawn gap past whole spans of the window less that lifetime, where the account could take it
 * there: the turn the router finds its pin live for and hands that size, so its expiry between turns is one the router
 * counts. Its attempt moves with it and reads its windows at the new instant, and no stream draws for the seat.
 */
const SEATED = new Set<string>();
{
  const reports = new Map<string, { accountId: string; at: number; generation: number; size: number }>();
  for (const r of byArrival()) {
    const key = pinKeyOf(r);
    const ttl = (poolById.get(r.poolId ?? '')?.affinityTtlSeconds ?? 0) * 1_000;
    const report = reports.get(key);
    const head = r.attempts[0] as Attempt | undefined;
    if (drawnRow(r) && head !== undefined && report !== undefined && report.generation === r.keyGeneration && ttl > SHORTEST_CACHE_LIFETIME && r.sessionId !== null && r.kind === 'inference' && r.status === 200 && r.attempts.length === 1 && r.contextCompacted === null && head.accountId === report.accountId && 2 * r.usage.cacheRead < report.size) {
      const start = Date.parse(head.startedAt);
      const seat = report.at + SHORTEST_CACHE_LIFETIME + ((start - report.at) % (ttl - SHORTEST_CACHE_LIFETIME));
      if (start - report.at > ttl && holdableAt(r, head.accountId, seat)) {
        r.at = iso(Date.parse(r.at) + seat - start);
        r.attempts = [{ ...onAccount({ ...head, startedAt: iso(seat) }, head.accountId, seat), cacheMissReason: head.cacheMissReason }];
        r.costUsd = round(costOf(r.model, r.usage, r.shape.requestedSpeed === 'fast', Date.parse(r.at)), 4);
        SEATED.add(r.id);
      }
    }
    const last = r.attempts[r.attempts.length - 1] as Attempt | undefined;
    if (r.sessionId !== null && last?.upstreamStatus === 200 && r.kind !== 'probe' && r.kind !== 'count_tokens') reports.set(key, { accountId: last.accountId, at: pinStampOf(r, false), generation: r.keyGeneration, size: promptOf(r.usage) });
  }
  REQUESTS.sort((a, b) => Date.parse(b.at) - Date.parse(a.at));
}

/**
 * The story's late request, an arrival among the rows a page of 100 shows that ran long, so the ledger writes it a few
 * seconds after the load, below rows a page read at the load already shows.
 */
const LATE_REQUEST_ID = 'req_7a3f0c26-d81';
/** Past the first auto reload poll of a page read as the mock loads, so that poll lands nothing and a later one lands it. */
const LATE_LANDING_MS = 6_000;
let landings: Map<string, number> | null = null;

/**
 * The instant the ledger wrote a story row: its finish on the story's clock, strictly increasing in the order the rows
 * finished, one past the row before where two land in one millisecond, as the ledger's writer stamps them, and the late
 * request's after every other. Read on first use, once the story's passes have set each row's last fields.
 */
export function landedAt(row: RequestLog): number {
  if (landings === null) {
    const finish = (r: RequestLog) => (r.id === LATE_REQUEST_ID ? NOW + LATE_LANDING_MS : Date.parse(r.at) + (r.durationMs ?? 0));
    let previous = 0;
    landings = new Map(
      [...REQUESTS]
        .sort((a, b) => finish(a) - finish(b) || byteOrder(a.id, b.id))
        .map((r) => {
          previous = Math.max(finish(r), previous + 1);
          return [r.id, previous];
        }),
    );
  }
  return landings.get(row.id) ?? Date.parse(row.at) + (row.durationMs ?? 0);
}

// A key whose seed leaves its last use to the rows reads its newest request the key check let through: the ci send
// after the revocation is refused as an unknown key and uses nothing.
for (const k of keyById.values()) if (k.lastUsedAt === null) k.lastUsedAt = REQUESTS.find((r) => r.keyId === k.id && r.errorCode !== 'auth_unknown')?.at ?? null;

/** Whether an account serves the model: secondary serves Sonnet 4.5 alone. */
const serves = (accountId: string, model: string) => accountId !== UNPRICED_ACCOUNT || model === 'claude-sonnet-4-5';

/** The pin a request's first selection found live: the account it stood on and when it landed there. */
interface LivePin {
  accountId: string;
  createdAt: number;
}

/**
 * Walks the requests oldest first, handing `visit` the pin each one's first selection found live, where the pin's
 * stamp (`pinStampOf`) left it within the pool's affinity window, with the selection's read of the row (`pinReadAt`)
 * and the account of a row it found at all. A sweep past the window takes the row. A request leaves its pin on its
 * last attempt's account unless that answer cooled the account, which releases the pin, or answered a releasing client
 * error on a pin it placed. A token count renews a live pin and creates none, and a renewal keeps the later of its
 * stamp and the one before it. The pin moves after the visit, so an attempt the visit moves takes the pin along, and a
 * request the visit leaves with no attempt leaves it alone.
 */
function walkLivePins(visit: (r: RequestLog, live: LivePin | undefined, read: PinRead, rowOn: string | undefined) => void) {
  const pins = new Map<string, LivePin & { at: number }>();
  for (const r of byArrival()) {
    const ttl = (poolById.get(r.poolId ?? '')?.affinityTtlSeconds ?? 0) * 1_000;
    const first = r.attempts[0] as Attempt | undefined;
    if (ttl === 0 || r.sessionId === null || first === undefined) continue;
    const key = pinKeyOf(r);
    const held = pins.get(key);
    const read = held === undefined ? 'none' : pinReadAt(held.at, ttl, Date.parse(first.startedAt));
    if (read === 'none') pins.delete(key);
    const live = read === 'live' ? held : undefined;
    visit(r, live && { accountId: live.accountId, createdAt: live.createdAt }, read, read === 'none' ? undefined : held?.accountId);
    const last = r.attempts[r.attempts.length - 1] as Attempt | undefined;
    if (last === undefined) continue;
    const at = pinStampOf(r, live?.accountId === last.accountId);
    const placed = live?.accountId !== last.accountId && r.kind !== 'count_tokens';
    if (failsOver(last) || (placed && RELEASING_STATUSES.includes(last.upstreamStatus))) pins.delete(key);
    else if (live?.accountId === last.accountId) pins.set(key, { ...live, at: Math.max(live.at, at) });
    else if (placed) pins.set(key, { accountId: last.accountId, at, createdAt: Date.parse(last.startedAt) });
  }
}

/**
 * Whether the router's step 4 moves a live pin off its account before a renewal, to the account the request's drawn
 * head stands on, which holds allowance on a plan outside overage: nightly's overage hands a standard request back
 * where the pool prefers the plan, and an API key's pin returns once it has sat the pool's dwell there. The overage
 * reads the instant the drawn heads give, since the renewal pass runs ahead of nightly's own.
 */
function stepFourMoves(r: RequestLog, live: LivePin, at: number, overageFrom: number): boolean {
  const pool = poolById.get(r.poolId ?? '');
  const head = r.attempts[0];
  if (!pool || r.kind === 'count_tokens' || r.shape.requestedSpeed === 'fast') return false;
  const kind = (id: string) => accountById.get(id)?.kind;
  const inOverage = (id: string) => id === 'nightly' && at >= overageFrom;
  const known = head.windows.some((w) => w.id === '5h' && w.utilization !== null) && head.windows.some((w) => w.id === '7d' && w.utilization !== null);
  const toPlan = kind(head.accountId) === 'oauth' && !inOverage(head.accountId) && known && spentWindows(head.windows).length === 0;
  if (pool.overagePolicy === 'prefer_plan' && kind(live.accountId) === 'oauth' && inOverage(live.accountId)) return toPlan;
  return kind(live.accountId) === 'api_key' && toPlan && at - live.createdAt >= pool.shedDwellSeconds * 1_000;
}

/**
 * The attempts each drawn probe drew that found no live pin and so sent none, by request id: its trace and its record
 * take their draws over them, so every later row keeps its own.
 */
export const NO_PIN_DRAWS = new Map<string, Attempt[]>();

/**
 * Answers a drawn probe whose session holds no live pin as the selector does: the pinned obstacle `no_pin`, which no
 * wait clears, so the relay answers 503 with no attempt, in the form of the story's turn no account answered
 * (`UNAVAILABLE_TURN_ID`). The probe counts file it under failed.
 */
function answerNoPin(r: RequestLog) {
  NO_PIN_DRAWS.set(r.id, r.attempts);
  CHAIN_FROM.delete(r.id);
  Object.assign(r, { ...unservedAnswer(), capturedBodies: false, diagnosticsSource: null, errorCode: 'accounts_unavailable', outcome: 'rejected', responseTruncated: false, status: 503 });
}

/**
 * The drawn requests moved onto the pin their first selection found live on another account, by id, each with the
 * account its draw had chosen. The router renews a live pin on an account that can take the request ahead of any
 * placement, where step 4 moves the pin nowhere, so the request's one attempt lands there with the answer it drew. A
 * request whose answer cooled its account keeps its draw, since that cooldown and the failover after it belong to the
 * drawn account. A drawn probe warms the pin it was sent for, so it stands on its live pin's account whatever it drew,
 * and one whose session holds no live pin sends nothing. The move runs ahead of nightly's overage instant and the live
 * pins below, which read the moved attempts.
 */
export const RENEWED_ON_PIN: ReadonlyMap<string, string> = (() => {
  const renewed = new Map<string, string>();
  const drawnHeads = REQUESTS.flatMap((r) => r.attempts).map((a) => (a.accountId === 'nightly' ? Date.parse(a.startedAt) : Number.NaN)).filter((at) => at >= NIGHTLY_CAP_RAISED);
  const overageFrom = drawnHeads.length ? Math.min(...drawnHeads) : NIGHTLY_CAP_RAISED + MINUTE;
  walkLivePins((r, live) => {
    const head = r.attempts[0];
    if (drawnRow(r) && r.kind === 'probe') {
      if (live === undefined) answerNoPin(r);
      else if (r.attempts.some((a) => a.accountId !== live.accountId)) {
        r.attempts = r.attempts.map((a) => onAccount(a, live.accountId, Date.parse(a.startedAt)));
        r.accountId = live.accountId;
        renewed.set(r.id, head.accountId);
      }
      return;
    }
    if (!drawnRow(r) || live === undefined || live.accountId === head.accountId || r.attempts.length > 1 || head.cooldownSet !== null) return;
    const to = live.accountId;
    const at = Date.parse(head.startedAt);
    if (!serves(to, r.canonicalModel) || !holdableAt(r, to, at) || stepFourMoves(r, live, at, overageFrom)) return;
    const moved = onAccount(head, to, at);
    r.accountId = to;
    // The cache comparison follows the account's kind, since an API key answers without the beta.
    if (moved.accountKind === head.accountKind) r.attempts = [moved];
    else r.attempts = diagnosed([{ ...moved, cacheMissReason: null }], r.kind);
    renewed.set(r.id, head.accountId);
  });
  // A seated turn renews the pin its drawn placement stood beside, so it takes that placement's draws as a moved row
  // does.
  for (const id of SEATED) renewed.set(id, requestById.get(id)?.attempts[0].accountId ?? '');
  return renewed;
})();

/**
 * The head that first read nightly in plan overage: the first attempt the day sent it after its spend cap was raised
 * at 11:30:12, or a minute past the raise where the draws sent none. The account's own overage reads from it.
 */
export const OVERAGE_FROM = ((): number => {
  const after = REQUESTS.flatMap((r) => r.attempts).map((a) => (a.accountId === 'nightly' ? Date.parse(a.startedAt) : Number.NaN)).filter((at) => at >= NIGHTLY_CAP_RAISED);
  return after.length ? Math.min(...after) : NIGHTLY_CAP_RAISED + MINUTE;
})();
const nightly = accountById.get('nightly');
if (nightly?.overage) nightly.overage.since = iso(OVERAGE_FROM);
// The attempt literals run before the instant exists, so nightly's heads from it on read their overage flag here.
for (const a of REQUESTS.flatMap((r) => r.attempts)) if (a.accountId === 'nightly' && Date.parse(a.startedAt) >= OVERAGE_FROM) a.overageInUse = true;

/**
 * The pin each request's first selection found live, by request id, so a renewal or a move rests on a request the day
 * holds, and the account of the row each one found idle past its window and not yet purged, which its placement
 * releases.
 */
const LIVE_PIN = new Map<string, LivePin>();
const EXPIRED_ROW = new Map<string, string>();
/**
 * Each drawn request whose first attempt stands off the live pin its first selection found, by id, with the pin's
 * account and the router's cause for taking the request off it: the pin's account cannot serve or hold the request
 * (`obstacle`), step 4 moves the pin (`move`), or no cause the router holds (`none`).
 */
const DECLINED = new Map<string, { kind: 'move' | 'none' | 'obstacle'; pinOn: string }>();
walkLivePins((r, live, read, rowOn) => {
  if (live) LIVE_PIN.set(r.id, live);
  if (read === 'expired_idle' && rowOn !== undefined) EXPIRED_ROW.set(r.id, rowOn);
  const head = r.attempts[0];
  if (!drawnRow(r) || r.kind === 'probe' || live === undefined || live.accountId === head.accountId) return;
  const at = Date.parse(head.startedAt);
  const kind = !serves(live.accountId, r.canonicalModel) || !holdableAt(r, live.accountId, at) ? 'obstacle' : stepFourMoves(r, live, at, OVERAGE_FROM) ? 'move' : 'none';
  DECLINED.set(r.id, { kind, pinOn: live.accountId });
});

/**
 * The move rule that takes a pin on `from` to the row's account in the engine's order, or null where none would. The
 * pin must be live on `from`, where an earlier request of the day left it. A spent window moves a standard request to
 * an account holding allowance outside plan overage, nightly's overage hands a standard request back to a plan-funded
 * subscription, and an API key's pin returns to one once it has sat its dwell there. The first rule whose reason holds
 * decides, so a move to an account that rule would not take is no move. A drain needs a cold pin and a shed a mark,
 * which no drawn pin carries, and a token count follows its pin without walking the rules.
 */
function supportedRule(r: RequestLog, from: string): PinMigrationReason | null {
  const pool = poolById.get(r.poolId ?? '');
  const head = r.attempts[0];
  const pinned = LIVE_PIN.get(r.id);
  if (!pool || !head || r.kind === 'count_tokens' || pinned?.accountId !== from || !serves(from, r.canonicalModel)) return null;
  const at = Date.parse(head.startedAt);
  const fast = r.shape.requestedSpeed === 'fast';
  const registryKind = (id: string) => accountById.get(id)?.kind;
  const inOverage = (id: string) => id === 'nightly' && at >= OVERAGE_FROM;
  const spent = (windows: AttemptWindow[]) => spentWindows(windows).length > 0;
  const known = head.windows.some((w) => w.id === '5h' && w.utilization !== null) && head.windows.some((w) => w.id === '7d' && w.utilization !== null);
  const headHolds = registryKind(head.accountId) === 'api_key' || (known && !spent(head.windows));
  // The gate answers a destination that does not fit where no sibling at room fits, so any sibling the rule keeps can
  // take the move, and the record reads its fitting rivals full.
  const toPlan = registryKind(head.accountId) === 'oauth' && !inOverage(head.accountId) && headHolds;
  if (!fast && pool.migrateWhenSpent && spent(oauthWindows(from, at, () => SERVED_FACTOR))) return headHolds && !(pool.overagePolicy === 'prefer_plan' && inOverage(head.accountId)) ? 'spent' : null;
  if (!fast && pool.overagePolicy === 'prefer_plan' && registryKind(from) === 'oauth' && inOverage(from)) return toPlan ? 'overage' : null;
  if (registryKind(from) === 'api_key') return toPlan && at - pinned.createdAt >= pool.shedDwellSeconds * 1_000 ? 'return_to_plan' : null;
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

/**
 * The reasons the router's release names for the row on `accountId` a request's first selection read, in the router's
 * order, as the story's account facts give them: the row idle past its pool's window, an account that does not serve
 * the model, the operator's pause, the account's state, a cooldown that holds the request, the usage credits a fast
 * request lacks, a reached budget, plan overage under a pool that does not allow it, and a spent plan window. A row
 * that none of them holds is `released`.
 */
function releaseReasonsOf(r: RequestLog, accountId: string, idle: boolean): string[] {
  const at = Date.parse(r.attempts[0].startedAt);
  const fast = r.shape.requestedSpeed === 'fast';
  const overage = accountId === 'nightly' && at >= OVERAGE_FROM && poolById.get(r.poolId ?? '')?.overagePolicy !== 'allow';
  const reasons = [
    ...(idle ? ['expired'] : []),
    ...(serves(accountId, r.canonicalModel) ? [] : ['unserved']),
    ...(accountId === 'overflow' && at >= OVERFLOW_PAUSED_AT ? ['paused'] : []),
    ...(accountId === 'overflow' && at >= OVERFLOW_LAPSED_AT ? ['funding_required'] : []),
    ...(coolingAt(accountId, at, r.canonicalModel, fast) ? ['cooldown'] : []),
    ...(fast && accountId === 'nightly' && at < NIGHTLY_CAP_RAISED ? ['fast_credits'] : []),
    ...(accountId === 'research' && at >= RESEARCH_BUDGET_AT ? ['budget'] : []),
    ...(!fast && overage ? ['overage'] : []),
    ...(!fast && spentWindows(oauthWindows(accountId, at, () => SERVED_FACTOR)).length > 0 ? ['spent'] : []),
  ];
  return reasons.length > 0 ? reasons : ['released'];
}

/** The order that picks the one word a live pin's forced move carries out of its release's reasons, as the router's. */
const MOVE_CAUSE_ORDER = ['deleted', 'paused', 'disconnected', 'funding_required', 'reauth_required', 'unpaired', 'subscription_ended', 'unserved', 'cooldown', 'fast_rate_limit', 'fast_credits', 'budget_unknown', 'budget', 'overage', 'attempted', 'spent', 'released'];

function buildRequestEvents(): Map<string, RequestEvent[]> {
  const h = createRng(8_080);
  const out = new Map<string, RequestEvent[]>();
  // The first eligible row skips the draws below, as it did when it carried the drawer's spent move, so every later
  // row keeps its draws.
  let spentOwed = true;
  for (const r of DRAWN_ORDER) {
    if (r.sessionId === null || r.kind === 'probe' || !drawnRow(r)) continue;
    const events: RequestEvent[] = [];
    // The pin the event belongs to: the drawer shows a move only where these match the request's own.
    const pin = pinFieldsOf(r);
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
      events.push({ ...pin, accountId: a.accountId, at: iso(attemptEnd(a)), detail: { attempt: a.index + 1, request_id: a.upstreamRequestId, source_request_id: r.id, wire_model: r.model }, kind: 'model_rejected' });
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
    // A placement that finds its row idle past the window and not yet purged releases it first. A token count places
    // no pin, so it releases none. The release and the moves below join after the draws above, which read the events a
    // drawn row holds.
    const expiredOn = EXPIRED_ROW.get(r.id);
    if (expiredOn !== undefined && r.kind !== 'count_tokens') events.unshift({ ...pin, accountId: expiredOn, at: r.at, detail: { agent_id: r.agentId, reasons: releaseReasonsOf(r, expiredOn, true) }, kind: 'pin_expired' });
    // A row off its live pin takes the router's cause where one holds: the pin's account standing against the request
    // forces the move with the release's whole list, and step 4 moves it under the rule it reads.
    const declined = DECLINED.get(r.id);
    const head = r.attempts[0];
    if (declined !== undefined && declined.kind !== 'none' && !events.some((e) => e.kind === 'pin_migrated' && e.at === head.startedAt)) {
      const reasons = releaseReasonsOf(r, declined.pinOn, false);
      const word = declined.kind === 'obstacle' ? (MOVE_CAUSE_ORDER.find((w) => reasons.includes(w)) ?? 'released') : declined.pinOn === 'nightly' ? 'overage' : 'return_to_plan';
      const detail = { agent_id: r.agentId, epoch: 0, from_account: declined.pinOn, reason: word, ...(declined.kind === 'obstacle' ? { reasons } : {}) };
      events.unshift({ ...pin, accountId: head.accountId, at: head.startedAt, detail, kind: 'pin_migrated' });
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
    const pin = pinFieldsOf(story);
    const detail = { request_id: null, source_request_id: story.id, wire_model: story.model };
    const events: RequestEvent[] = [
      ...(out.get(story.id) ?? []),
      { ...pin, accountId: refused.accountId, at: iso(attemptEnd(refused)), detail: { attempt: 1, ...detail }, kind: 'model_rejected' },
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
    const event: RequestEvent = { ...pinFieldsOf(r), accountId: to, at: iso(at), detail: { agent_id: r.agentId, epoch: 0, from_account: from, reason }, kind: 'pin_migrated' };
    out.set(id, [...(out.get(id) ?? []), event]);
  };
  moved('req_4b0e9d17-c31', clock(14, 27, 1), 'primary', 'nightly', 'fast_lane');
  moved('req_2e7c90a1-6d4', clock(14, 24, 10), 'primary', 'nightly', 'cooldown');
  moved('req_da52d141-a5a', clock(14, 5, 12), 'secondary', 'primary', 'spent');
  moved('req_b8c3e9d4-e61', clock(14, 7, 51), 'secondary', 'primary', 'cooldown');
  moved('req_1d9c3e55-0b2', clock(14, 2, 11), 'secondary', 'primary', 'cooldown');
  moved(TIE_STORY_ID, clock(8, 16, 21), 'overflow', 'primary', 'cooldown');
  moved('req_868264bc-611', clock(11, 8, 30), 'secondary', 'primary', 'shed');
  moved(FORCED_MOVE_ID, clock(11, 1, 50), 'overflow', 'nightly', 'return_to_plan');
  // The placement's release of the expired row on nightly, which it writes before the pin it creates, its reasons in
  // the router's order: the idle row, and nightly's overage under all's preference for a plan.
  const replaced = requestById.get(REPLACED_PIN_ID);
  if (replaced) out.set(replaced.id, [{ ...pinFieldsOf(replaced), accountId: 'nightly', at: replaced.at, detail: { agent_id: replaced.agentId, reasons: ['expired', 'overage'] }, kind: 'pin_expired' }]);
  // The release of the pin the 400's placement created, which the router writes once the answer ends the request.
  const refused = requestById.get(CLIENT_ERROR_TURN_ID);
  const refusedEnd = refused?.attempts[refused.attempts.length - 1];
  if (refused && refusedEnd) out.set(refused.id, [{ ...pinFieldsOf(refused), accountId: refusedEnd.accountId, at: iso(attemptEnd(refusedEnd)), detail: { agent_id: refused.agentId, reasons: ['client_error'] }, kind: 'pin_expired' }]);
  // The move the workspace's state forced, which carries the live pin's whole list and takes its word from it.
  const caused = requestById.get(CAUSED_MOVE_ID);
  if (caused) out.set(caused.id, [{ ...pinFieldsOf(caused), accountId: 'primary', at: caused.at, detail: { agent_id: caused.agentId, epoch: 0, from_account: 'overflow', reason: 'funding_required', reasons: ['funding_required'] }, kind: 'pin_migrated' }]);
  // The move off the deleted account: the reload's purge kept the pin's trace with its word and its list, and the
  // session's next request lands on it.
  const deletedMove = requestById.get(DELETED_MOVE_ID);
  if (deletedMove) out.set(deletedMove.id, [{ ...pinFieldsOf(deletedMove), accountId: 'primary', at: deletedMove.at, detail: { agent_id: deletedMove.agentId, epoch: 0, from_account: RETIRED.account.id, reason: 'deleted', reasons: ['deleted'] }, kind: 'pin_migrated' }]);
  const rejected = requestById.get('req_2e7c90a1-6d4');
  if (rejected) {
    const pin = pinFieldsOf(rejected);
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

/** The pin move a request's events record for an attempt: the one that lands on its account as the attempt starts. */
export const migrationAt = (r: Pick<RequestLog, 'id'>, a: Pick<Attempt, 'accountId' | 'startedAt'>): RequestEvent | undefined =>
  REQUEST_EVENTS.get(r.id)?.find((e) => e.kind === 'pin_migrated' && e.at === a.startedAt && e.accountId === a.accountId);

/**
 * The obstacles a fresh placement's passed-over members are drawn from. Being ranked behind another is the usual one.
 */
const OBSTACLE_DRAWS: { value: string; weight: number }[] = [
  { value: 'at_cap', weight: 1 },
  { value: 'cooldown', weight: 2 },
  { value: 'no_fit', weight: 3 },
  { value: 'operator', weight: 1 },
  { value: 'overage', weight: 1 },
  { value: 'ranked', weight: 10 },
  { value: 'spent', weight: 1 },
  { value: 'subscription_ended', weight: 1 },
  { value: 'winding_down', weight: 1 },
];

/** An event's pin fields. A declaration, since `REQUEST_EVENTS` above reads it at load. */
function pinFieldsOf(r: RequestLog): Pick<RequestEvent, 'keyId' | 'model' | 'sessionId'> {
  return { keyId: r.keyId, model: r.canonicalModel, sessionId: r.sessionId };
}

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

/** A wait for a slot, as the router writes it: one cap wait of the hold's whole length. */
const slotHold = (cohort: string | null, heldMs: number, waitedOn: string[]): CohortHold => ({ cohort, heldMs, waitedOn, waits: [{ heldMs, reason: 'cap' }] });

/**
 * Half the subagents placed afresh first waited on their cohort's account, every account at its cap. The wait
 * draws from a stream of its own, so every placement drawn after a hold keeps its values.
 */
const cohortHoldOf = (p: Rng, w: Rng, r: RequestLog, placement: Placement | null, accountId: string): CohortHold | null =>
  placement !== null && r.agentId !== 'main' && p.chance(0.5) ? slotHold(placement.rung === 'with_main' ? null : accountId, w.int(180, 4_000), [accountId]) : null;

/**
 * The trace of a row outside the seeded streams: the strategy's pick in its account's tier. The members it passed over
 * are the ones its decision's record holds out or ranked, which the routing log fills in once it builds that record.
 */
function undrawnTrace(r: RequestLog, a: Attempt, last: boolean): SelectionTrace {
  const pool = poolById.get(r.poolId ?? '');
  const account = accountById.get(a.accountId);
  const priority = pool?.members.find((m) => m.accountId === a.accountId)?.priority ?? 0;
  const overage = a.overageInUse === true;
  return { cacheRead: last && r.status === 200 ? r.usage.cacheRead : null, cohortHold: null, moved: null, placement: null, renewed: false, skipped: [], strategy: pool?.strategy ?? 'pace', tier: rankTier(pool?.preferSubscription ?? false, account !== undefined && isPaidSource(account), overage, priority) };
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
  for (const r of DRAWN_ORDER) {
    // The routing log's records write the story's traces, so no obstacle is drawn against what its cards show.
    if (STORY_IDS.has(r.id)) continue;
    if (UNSEEDED_IDS.has(r.id)) {
      // A written row's move is the one its own event records, as a drawn row's is.
      r.attempts = r.attempts.map((a, i, all) => {
        const moved = migrationAt(r, a)?.detail?.reason;
        return { ...a, selection: { ...undrawnTrace(r, a, i === all.length - 1), moved: typeof moved === 'string' ? moved : null } };
      });
      continue;
    }
    const pool = poolById.get(r.poolId ?? '');
    const members = LANES.find((l) => l.keyId === r.keyId)?.accountIds ?? [];
    const drawnClass = r.compaction ? 'compaction' : r.agentId !== 'main' && r.kind !== 'probe' ? 'subagent' : h.weighted([{ value: 'auxiliary', weight: 3 }, { value: 'main', weight: 16 }, { value: 'workflow', weight: 1 }]);
    r.requestClass = drawnClass;
    // The pool's policy adds no marker to an auxiliary class, so the marker waits for the class drawn here.
    r.cacheMarker = markerOf(r, ownMarkerOf(r));
    // A probe that sent nothing traces the attempts it drew and keeps none, so every later row keeps its draws.
    const drawnAttempts = NO_PIN_DRAWS.get(r.id);
    const traced = (drawnAttempts ?? r.attempts).map((a, i, all): Attempt => {
      const moved = migrationAt(r, a)?.detail?.reason;
      // The ledger folds the cache read into the trace of the attempt that answered, and only once it did.
      const cacheRead = i === all.length - 1 && r.status === 200 ? r.usage.cacheRead : null;
      const strategy = pool?.strategy ?? 'pace';
      // A request renews the pin its first selection finds live on the account it went to, and only then. The renewal
      // draw is taken either way, so the stream stays where it was, and a drawn renewal with no live pin to renew is
      // placed afresh on no further draw.
      const first = i === 0 && moved === undefined;
      const drawnRenewal = first && h.chance(0.55);
      if (first && (pool?.affinityTtlSeconds ?? 0) > 0 && LIVE_PIN.get(r.id)?.accountId === a.accountId) {
        // A request moved onto its live pin takes the draws its drawn placement took, so every later row keeps its own.
        const drawnOn = RENEWED_ON_PIN.get(r.id);
        if (drawnOn !== undefined && !drawnRenewal) {
          for (const id of members) if (id !== drawnOn) h.weighted(OBSTACLE_DRAWS);
          cohortHoldOf(p, w, r, placementOf(p, r), drawnOn);
        }
        return { ...a, selection: { cacheRead, cohortHold: null, moved: null, placement: null, renewed: true, skipped: [], strategy, tier: null } };
      }
      if (drawnRenewal) return { ...a, selection: undrawnTrace(r, a, i === all.length - 1) };
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
      return { ...a, selection: { ...undrawnTrace(r, a, i === all.length - 1), cohortHold, moved: typeof moved === 'string' ? moved : null, skipped } };
    });
    if (drawnAttempts === undefined) r.attempts = traced;
  }
}

/**
 * The fork facts the ladder records on each drawn request: none, but on the first turns of three seeded sessions, each
 * branched from another session the story seeds.
 */
function identifyRequests() {
  for (const r of REQUESTS) if (drawnRow(r)) Object.assign(r, { forkDepth: null, forkedFrom: null });
  // A session's oldest row is its first request, and the rows run newest first, so the last one set per session
  // stays. A fork is a turn the log lists, never a probe, and its base another session the story seeds. Both forks
  // are pinned by identity, so neither moves with the load instant and the two never land on one row.
  // An unattended job's rows open no fork, since a fork is a turn an interactive session made.
  const oldest = new Map<string, RequestLog>();
  for (const r of REQUESTS) if (r.sessionId !== null && r.keyId !== 'ci' && drawnRow(r)) oldest.set(r.sessionId, r);
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
    const r = REQUESTS.find((x) => !taken.has(x.id) && drawnRow(x) && x.kind === 'inference' && x.status === 200 && x.attempts.length === 1 && x.attempts[0].selection !== null && want(x.attempts[0].selection, x));
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
  const strategyHold = (r: RequestLog): CohortHold => slotHold(null, 1250, [r.attempts[0].accountId]);
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
  const atCap = REQUESTS.find((r) => r.keyId === 'desktop' && r.kind === 'inference' && r.status === 429 && failoversOnly(r) && drawnRow(r));
  // A hold waits on an account that could serve the request then, at its cap.
  const servableFor = (r: RequestLog, at: number) => (id: string) => holdableAt(r, id, at);
  if (atCap) {
    const first = LANES.find((l) => l.keyId === atCap.keyId)?.accountIds.find(servableFor(atCap, Date.parse(atCap.at)));
    const waitedOn = first === undefined ? [] : [first];
    const heldMs = boundOf(atCap);
    keepAttempts(atCap, 0);
    // No body went upstream, so none carried a diagnostics member.
    Object.assign(atCap, { accountId: null, costUsd: 0, diagnosticsSource: null, durationMs: heldMs, errorCode: 'accounts_at_cap', held: slotHold(null, heldMs, waitedOn) });
  }
  const heldAt = (r: RequestLog) => {
    const drawnAttempts = drawnPart(r);
    const last = drawnAttempts[drawnAttempts.length - 1] as Attempt | undefined;
    return last ? attemptEnd(last) : Date.parse(r.at);
  };
  // The reselect after the draws' last 429 meets the members left untried, the first of which it waits on.
  const untried = (r: RequestLog) => {
    const tried = drawnPart(r).map((a) => a.accountId);
    const members = (poolById.get(r.poolId ?? '')?.members ?? []).map((m) => m.accountId).sort((x, y) => Number(x === UNPRICED_ACCOUNT) - Number(y === UNPRICED_ACCOUNT));
    return members.find((id) => !tried.includes(id) && serves(id, r.canonicalModel) && servableFor(r, heldAt(r))(id));
  };
  // A 429 fails the attempt over to a reselect, which can find every account at its cap. A 529 or a 502 is committed.
  const failedTurn = (r: RequestLog) => r !== atCap && drawnRow(r) && r.kind === 'inference' && r.status === 429 && drawnPart(r).slice(-1)[0]?.upstreamStatus === 429 && untried(r) !== undefined;
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
    durationMs: attemptEnd(lastKept) - Date.parse(afterAttempt.at) + bound,
    errorCode: 'accounts_at_cap',
    held: { cohort: null, heldMs: (before?.heldMs ?? 0) + bound, waitedOn: [...(before?.waitedOn ?? []), next], waits: [...(before?.waits ?? []), { heldMs: bound, reason: 'cap' }] },
    outcome: 'exhausted',
    status: 429,
  });
}

/**
 * The band's state at `at` for a model with a 7-day weight, null for a model without one, whose figure and shares
 * price nothing, and where the band cannot be read, since a share on an assumed state could read half what the ledger
 * charges. A site reads it once for the figure and every window, each priced as the equivalent and the share read it.
 */
export const bandAt = (model: string, at: number): boolean | null => (weightOf(TABLES, model, 'seven_day') !== null ? inBand(TABLES, at) : null);

/**
 * The router frees only the classifier call that landed on the judged turn's account, the one it reads `client`. A
 * declaration, since the story's rows read it while the module loads, above this line.
 */
function freesPlanShare(classifier: string | null): boolean {
  return classifier === 'client';
}

/**
 * The classifier call the router prices at zero in every unit, its dollars too: one it frees, on a serving attempt a
 * subscription window paid. A `client_off` call, and a `client` call in the overage, on an API key or on a
 * usage-based seat, keep their price. A declaration, since the story's rows read it while the module loads, above this
 * line.
 */
function freeCall(classifier: string | null, serving: Parameters<typeof fundingOf>[0]): boolean {
  return freesPlanShare(classifier) && fundingOf(serving) === 'plan';
}

/** `freeCall` over a rollup part, which files its rows under one classifier and one funding. */
export const freedPart = (r: Pick<HourlyRollup, 'classifier' | 'funding'>): boolean => freesPlanShare(r.classifier) && r.funding === 'plan';

/**
 * A row's Pro-week figure and its shares of its account's windows, as the request route answers them, priced under the
 * regime the row's client label names: null for a request no subscription window paid, else the figure at the
 * request's completion, null for a model without a weight of its own, and a share on each window of the account's set
 * and each week the model applies to, under the account's tier, a window the cost model cannot price carried as null
 * and an account without a tier an empty share map. A classifier call `freeCall` frees reads zero, on each such window
 * the tier sizes, with the share reason, while any other classifier call and one billed in dollars carry no reason, as
 * the route answers them.
 */
function costFiguresOf(r: RequestLog): Pick<RequestLog, 'costEquivalent' | 'costShares' | 'shareReason'> {
  if (!planPaid(r)) return { costEquivalent: null, costShares: null, shareReason: null };
  const serving = servingAttempt(r)!;
  const tier = accountById.get(serving.accountId)?.subscription?.rateLimitTier;
  const finishedMs = Date.parse(r.at) + (r.durationMs ?? 0);
  const windows = shareWindowsOf(serving.accountId, r.canonicalModel);
  const tables = tablesFor(r.client);
  if (freeCall(r.classifier, serving)) {
    const zeros = (sizing: string) =>
      Object.fromEntries(
        windows.filter((w) => {
          const priced = pricedWindow(tables, w);
          return priced !== null && tierCapacity(tables, sizing, priced.priced) !== null;
        }).map((w) => [w, 0]),
      );
    return { costEquivalent: 0, costShares: tier === undefined ? {} : zeros(tier), shareReason: 'client_classifier' };
  }
  const inside = bandAt(r.canonicalModel, finishedMs);
  const shares: CostShares = tier === undefined ? {} : Object.fromEntries(windows.map((w) => [w, inside === null ? null : shareIn(tables, w, tier, r.canonicalModel, r.usage, inside)]));
  return { costEquivalent: inside === null ? null : equivalentIn(tables, r.canonicalModel, r.usage, inside), costShares: shares, shareReason: null };
}

/**
 * The serving attempt's two counts as the route answers them: none for a token count, whose answer the router stores
 * no usage for, none for a request whose serving attempt answered no 200 or that no attempt served, and no output
 * count for a stream that ended before its answer reported one.
 */
function storedTokensOf(r: RequestLog, last: Attempt | null): RequestLog['storedTokens'] {
  if (r.kind === 'count_tokens' || last?.upstreamStatus !== 200) return { input: null, output: null };
  return { input: r.usage.input, output: OUTPUT_UNREPORTED_IDS.has(r.id) ? null : r.usage.output };
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
  for (const r of DRAWN_ORDER) {
    const atMs = Date.parse(r.at);
    const first = r.attempts[0] as Attempt | undefined;
    const last = servingAttempt(r);
    // A row outside the seeded streams takes fixed instants, so every row after it keeps its draws.
    const unseeded = UNSEEDED_IDS.has(r.id);
    const write = unseeded ? 20 : phaseRng.int(8, 40);
    const tail = unseeded ? 12 : phaseRng.int(5, STREAM_TAIL);
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
      writtenAt: last === null ? null : iso(Date.parse(last.startedAt) + write),
    };
    Object.assign(r, costFiguresOf(r), { storedTokens: storedTokensOf(r, last), tier: last === null ? null : accountTier(last.accountId) });
  }
}

/**
 * The session's turns as the router and the relay see them, over the rows' last fields, since the passes above move
 * arrivals. Each pin key (key, session, agent and canonical model) holds the router's row: its account, its stamp
 * (`pinStampOf`), its last refresh, whether only auxiliary requests used it and the size of its last usage report. A
 * request's first selection reads the row as `pinReadAt` does, and a row a sweep has taken is gone, so a pool that pins
 * nothing holds none. A first attempt takes its previous size as the selection hands it on: the row's size, live or
 * idle past the window, where the attempt lands on the row's account, a token count's renewal among them, and none on a
 * compacted turn, on a row whose last report was empty or absent, off that account and on a request with no row. A
 * probe's usage is the keepalive's cost and renews no size, and a token count stores no report. A failover's leg finds
 * no row of its own, since the failover took it, so it takes no size, and a resend in place goes out under the decision
 * of the attempt it repeats, its size and the account it left included.
 *
 * An attempt that takes a size takes beside it whether the row's entry was past its lifetime at the selection: at or
 * past the row's last refresh, or its last use for a row only auxiliary requests used, plus the session's cache TTL
 * (`pinCacheTtlOf`). Each other attempt reads null, and a resend in place copies the fact with the size. A placement
 * stamps the refresh at its selection and marks a row that a request of an auxiliary class placed, a turn's renewal
 * stamps it at its selection and clears the mark, a compacted turn's renewal zeroes it, a probe's renewal stamps it at
 * the report, and a token count and an auxiliary call leave both.
 *
 * The account a first attempt leaves is its agent's newest landing under the key and session, whatever the model, while
 * the sweeps have not taken it: a placement, a renewal and a probe's renewal stamp the pin they land on. An auxiliary
 * call reads its own pin's account, and its pins answer no other request. A token count creates no pin: one that finds
 * its pin live stamps it and becomes the agent's newest landing. A releasing client error releases the pin its
 * placement created, while a renewal that answered one keeps its pin, its landing and its stamp. A failover's leg of a
 * row with a session leaves the account of the leg before it, since the router gives a request that takes no pin no
 * failover entry.
 *
 * A body's member names the message its agent's last answer carried, a fork's its parent's while it has none of its
 * own, so an agent's first answered turn that is no fork names none. The relay detaches the member's message on an
 * attempt that lands off the account it leaves and on every probe. An answered attempt whose body carried a member and
 * named no message files `not_returned`. One the build left unfiled (a leg, a placement after the session's row
 * expired) that named one draws the comparison a turn that stayed draws, `none` on a story row, which draws nothing. A
 * turn behind a sweep leaves no account, so its member travels as sent, and where it lands off the account that
 * answered its agent's last turn, that account alone holds the message it names: it files `previous_message_not_found`,
 * the comparison drawn and set aside so every later draw keeps its place. A row stored before the relay filed its words
 * keeps its null.
 */
function walkSessionTurns() {
  const rows = new Map<string, { accountId: string; at: number; auxiliaryOnly: boolean; refreshedAt: number; size: number | null }>();
  const newest = new Map<string, { accountId: string; at: number }>();
  const answered = new Map<string, string>();
  for (const r of byArrival()) {
    const pin = pinKeyOf(r);
    const ttl = r.sessionId === null ? 0 : (poolById.get(r.poolId ?? '')?.affinityTtlSeconds ?? 0) * 1_000;
    const first = r.attempts[0] as Attempt | undefined;
    const selectedAt = Date.parse(first?.startedAt ?? r.at);
    const held = rows.get(pin);
    const read = held === undefined ? 'none' : pinReadAt(held.at, ttl, selectedAt);
    if (read === 'none') rows.delete(pin);
    const row = read === 'none' ? undefined : held;
    const landing = `${r.keyId}|${r.sessionId}|${r.agentId}`;
    const auxiliary = r.requestClass === 'auxiliary';
    const agent = `${r.sessionId}|${r.agentId}`;
    const turn = r.sessionId !== null && r.kind === 'inference';
    const forked = r.agentType === 'fork' || r.forkedFrom !== null;
    const firstTurn = turn && !answered.has(agent) && !forked;
    const parent = r.agentType === 'fork' ? `${r.sessionId}|main` : r.forkedFrom === null ? undefined : `${r.forkedFrom}|main`;
    const lastAnswer = answered.get(agent) ?? (parent === undefined ? undefined : answered.get(parent));
    const prior = newest.get(landing);
    // A pin only auxiliary requests used is kept warm by them, so its last use is the measure, as the router's `warm`
    // reads its row.
    const warmUntil = row === undefined ? null : (row.auxiliaryOnly ? row.at : row.refreshedAt) + pinCacheTtlOf(r.sessionId);
    let left: string | undefined;
    r.attempts.forEach((a, i) => {
      if (resentInPlace(r, i)) {
        if (r.sessionId !== null) {
          a.previousInputTokens = r.attempts[i - 1].previousInputTokens;
          a.entryExpired = r.attempts[i - 1].entryExpired;
        }
      } else if (i > 0) {
        if (r.sessionId !== null) left = r.attempts[i - 1].accountId;
        a.previousInputTokens = null;
        a.entryExpired = null;
      } else if (r.sessionId !== null) {
        left = auxiliary ? row?.accountId : prior !== undefined && pinReadAt(prior.at, ttl, selectedAt) !== 'none' ? prior.accountId : undefined;
        a.previousInputTokens = row !== undefined && r.contextCompacted === null && (row.size ?? 0) > 0 && a.accountId === row.accountId ? row.size : null;
        a.entryExpired = a.previousInputTokens === null || warmUntil === null ? null : selectedAt >= warmUntil;
      }
      if (a.upstreamStatus !== 200 || r.kind === 'count_tokens' || UNFILED_IDS.has(r.id)) return;
      const named = !firstTurn && r.kind !== 'probe' && (left === undefined || left === a.accountId);
      // As the relay closes an answered attempt: `not_requested` where its body carried no diagnostics member,
      // `not_returned` where it carried one and the answer drew no reason, else the reason the upstream compared. An
      // API-key account in this fixture answers without the beta, so it compares nothing, and neither does the upstream
      // on a probe, whose member the relay detaches whoever put it there.
      if (r.diagnosticsSource === null) a.cacheMissReason = 'not_requested';
      else if (!named || a.accountKind !== 'oauth') a.cacheMissReason = 'not_returned';
      else {
        if (a.cacheMissReason === null) a.cacheMissReason = drawnRow(r) ? compare() : 'none';
        if (i === 0 && read === 'none' && lastAnswer !== undefined && lastAnswer !== a.accountId) a.cacheMissReason = 'previous_message_not_found';
      }
    });
    const last = r.attempts[r.attempts.length - 1] as Attempt | undefined;
    if (r.sessionId !== null && last !== undefined) {
      const renews = row !== undefined && read === 'live' && row.accountId === last.accountId;
      const stamp = pinStampOf(r, renews);
      const released = RELEASING_STATUSES.includes(last.upstreamStatus);
      const places = !renews && r.kind !== 'count_tokens' && !released;
      const refreshing = r.requestClass === null || CONVERSATION_CLASSES.some((c) => c === r.requestClass);
      if (failsOver(last) || (!renews && r.kind !== 'count_tokens' && released)) rows.delete(pin);
      else if (renews) {
        row.at = Math.max(row.at, stamp);
        // A refresh keeps the later of its stamp and the one before it, as the last use does, since a turn and a probe
        // that overlap land their stamps out of arrival order. A compacted turn's zero alone moves it back.
        if (r.kind === 'probe') {
          if (reportsRenewal(r, renews)) row.refreshedAt = Math.max(row.refreshedAt, stamp);
        } else if (r.kind !== 'count_tokens' && r.contextCompacted !== null) row.refreshedAt = 0;
        else if (r.kind !== 'count_tokens' && refreshing) Object.assign(row, { auxiliaryOnly: false, refreshedAt: Math.max(row.refreshedAt, selectedAt) });
      } else if (places) {
        // The selection that placed the pin is the one the last attempt went out under.
        let decided = r.attempts.length - 1;
        while (resentInPlace(r, decided)) decided -= 1;
        rows.set(pin, { accountId: last.accountId, at: stamp, auxiliaryOnly: !refreshing, refreshedAt: Date.parse(r.attempts[decided].startedAt), size: null });
      }
      if (!auxiliary && (renews || places)) newest.set(landing, { accountId: last.accountId, at: renews ? row.at : stamp });
    }
    const serving = servingAttempt(r);
    if (turn && serving?.upstreamStatus === 200) answered.set(agent, serving.accountId);
    const written = rows.get(pin);
    if (written !== undefined && serving !== null && written.accountId === serving.accountId && r.kind !== 'probe' && r.storedTokens.input !== null) written.size = promptOf(r.usage);
  }
}

/** The entry the upstream lists for the client's standing block once its session compacted. */
const PREFIX_KEY: (typeof TRANSFORMATION_KEYS)[number] = 'thinking_dropped.prefix_binding_mismatch';
/** A block that another end user's account wrote, which the upstream drops from an answer on this one. */
const ACCOUNT_KEY: (typeof TRANSFORMATION_KEYS)[number] = 'thinking_dropped.end_user_binding_mismatch';
/** Where a session's first block sits, since the client's first request sends two messages. */
const FIRST_BLOCK_MESSAGE = 2;

/**
 * Whether the model binds a thinking block to the account that wrote it, so that an answer on another account drops
 * it: the vendor's four, of which the catalog serves Sonnet 5.5 alone. The router holds no such list and learns each
 * pair of accounts from an answer.
 */
export function bindsThinking(model: string): boolean {
  return ['claude-fable-5-2', 'claude-haiku-5-5', 'claude-mythos-5-2', 'claude-sonnet-5-5'].includes(model);
}

/** What one pin key's history holds for the upstream's binding checks. */
interface ThinkingHistory {
  /**
   * The account that wrote the block the last compaction carries, null before a compaction and, after one, until an
   * account answers.
   */
  carriedBy: string | null;
  /** A compaction stands behind the turn, so the client sends its standing block. */
  compacted: boolean;
  /** The account that answered the pin key's last answered inference turn, whatever compaction came between. */
  lastAnswered: string | null;
  /**
   * The serving account of each answered turn that thought since the compaction, except the turn that wrote the
   * carried block.
   */
  thinkers: string[];
}

/**
 * The blocks each answered attempt's upstream dropped from the request's history, read off the rows as they finally
 * stand, per pin key (key, session, agent and canonical model) in arrival order, since a block binds by the model that
 * wrote it. An attempt reports where the request's head carried the thinking-binding token and the attempt answered,
 * a token count aside, whose answer holds no list.
 *
 * From a compaction on, the client sends one standing block, which the account that answered the pin key's last
 * answered turn ahead of the compaction wrote, or the compaction turn's own where none stands. A report on that account
 * lists the prefix key. Where the model binds its thinking, a report on any other account drops the block under the
 * account reason, so it lists the account key with that block in place of the prefix key, with or without a move
 * between, since the upstream reads the block's account and never the router's events. By the same rule, where the
 * model binds its thinking, a report adds to the account key one block for each answered turn since the last
 * compaction that thought on another account than the one that answers, and the turn that wrote the carried block adds
 * none, since that block counts once. A row names the token's sender beside a reported map alone.
 */
function fillDroppedBlocks() {
  const fresh = (): ThinkingHistory => ({ carriedBy: null, compacted: false, lastAnswered: null, thinkers: [] });
  const histories = new Map<string, ThinkingHistory>();
  for (const r of byArrival()) {
    const pin = pinKeyOf(r);
    const history = r.sessionId === null ? fresh() : (histories.get(pin) ?? fresh());
    if (r.sessionId !== null) histories.set(pin, history);
    if (r.contextCompacted !== null) Object.assign(history, { carriedBy: history.lastAnswered, compacted: true, thinkers: [] });
    // Only an answer to a head that carried the token reports, and the rule says whether one did.
    const token = thinkingSourceOf(r.keyId, r.poolId, r.kind);
    let carries = false;
    for (const a of r.attempts) {
      // A compaction with no answer behind it carries the block the first answer after it writes.
      if (history.compacted && history.carriedBy === null && a.upstreamStatus === 200) {
        history.carriedBy = a.accountId;
        carries = true;
      }
      let dropped: InputTransformations | null = null;
      if (token !== null && r.kind !== 'count_tokens' && a.upstreamStatus === 200) {
        const binds = bindsThinking(r.canonicalModel);
        const away = binds && history.compacted && history.carriedBy !== a.accountId;
        const left = binds ? history.thinkers.filter((id) => id !== a.accountId).length : 0;
        const blocks = (away ? 1 : 0) + left;
        dropped = {};
        if (history.compacted && !away) dropped[PREFIX_KEY] = { blocks: 1, firstMessage: FIRST_BLOCK_MESSAGE };
        if (blocks > 0) dropped[ACCOUNT_KEY] = { blocks, firstMessage: FIRST_BLOCK_MESSAGE };
      }
      // A failover's added legs share their response with the leg they copy, so each takes one of its own.
      a.response = { ...a.response, inputTransformations: dropped };
    }
    const serving = servingAttempt(r);
    if (serving?.upstreamStatus === 200 && r.kind === 'inference') history.lastAnswered = serving.accountId;
    if (serving?.upstreamStatus === 200 && r.usage.thinking > 0 && !carries) history.thinkers.push(serving.accountId);
    r.thinkingDiagnosticsSource = (serving?.response.inputTransformations ?? null) === null ? null : token;
  }
}

traceRequests();
seedDrawerBranches();
seedHolds();
liftCooldowns(REQUESTS);
identifyRequests();
phaseRequests();
traceForks();
walkSessionTurns();
fillDroppedBlocks();

/** A cooldown row as the routing state holds it: the accounts' own rows and every row an attempt's answer set. */
export interface CooldownRow {
  accountId: string;
  reason: string;
  scope: string;
  setAt: number;
  until: number;
  window: string;
}

/** Every row an attempt's answer set, with the model it answered and its request, which the row's event names. */
export const ATTEMPT_COOLDOWNS: (CooldownRow & { model: string; requestId: string })[] = REQUESTS.flatMap((r) =>
  r.attempts.flatMap((a) => (a.cooldownSet ? [{ accountId: a.accountId, ...cooldownOf(a.cooldownSet.reason, r.canonicalModel), setAt: attemptEnd(a), until: Date.parse(a.cooldownSet.until), model: r.canonicalModel, requestId: r.id }] : [])),
);

function histogramsOf(rows: RequestLog[]): LatencyHistograms {
  const samples = rows.filter((r) => r.status === 200).map((r) => ({ kind: r.kind, ...latencySamples(r) }));
  const pick = (f: (s: (typeof samples)[number]) => number | null) => histogramOf(samples.map(f).filter((v): v is number => v !== null && v > 0));
  return {
    durationMs: pick((s) => (s.kind === 'count_tokens' ? null : s.durationMs)),
    outputTps: pick((s) => s.outputTps),
    ttfbMs: pick((s) => s.ttfbMs),
    ttftMs: pick((s) => s.ttftMs),
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
  let roll = fnv1a(seed) % HOUR_CLASSES.reduce((n, [, w]) => n + w, 0);
  for (const [name, weight] of HOUR_CLASSES) {
    if (roll < weight) return name;
    roll -= weight;
  }
  return 'main';
}

/**
 * The first hour the day's held probe is drawn from, the hour 23 hours before the current one. The dashboard's day, an
 * account's or a pool's day total, and the dashboard's and a pool's 25 hourly points start an hour earlier, on the
 * hour a full day back, and the week on the hour seven days back, as the routes total their rollups.
 */
export const DAY_FROM = startOfHour(NOW) - 23 * HOUR;
const TOTALS_FROM = startOfHour(NOW) - 24 * HOUR;
const WEEK_FROM = startOfHour(NOW) - 7 * DAY;

// The probes the router held while its ledger caught up draw from a stream of their own, so they shift no other draw.
const heldRng = createRng(6_133);

/**
 * Who paid for a request, as the ledger keys its rollups off the answer that served it: `billed` for an API key
 * account's answer before anything, then `usage_based` for a usage-based seat's, then a fast answer's credits, then the
 * head's overage flag, else the plan. A request no attempt served files under `plan` whatever speed it asked for.
 */
const fundingKey = (r: RequestLog): HourlyRollup['funding'] => {
  const last = servingAttempt(r);
  if (last === null) return 'plan';
  const funding = fundingOf(last);
  return funding === 'api_key' ? 'billed' : funding;
};

/**
 * An hour's Pro-week figure and its plan-window shares: each request priced at its own band and completion, then
 * summed, so a request across a band edge reads as its own shares do. A request is priced here rather than its cost
 * shares read, which are null on every request the plan did not fund. A figure or a window any request prices null
 * reads null, a model without a weight of its own prices nothing, a request with no token prices nothing, and the
 * shares, keyed by the windows the account's set has and the model's weeks, read nothing for an account without a tier.
 * The hour's rows share one client label, whose regime prices them.
 */
function hourShares(accountId: string, model: string, client: string | null, priced: { at: number; usage: Usage }[]): Pick<HourlyRollup, 'equivalent' | 'shares'> {
  const tier = accountById.get(accountId)?.subscription?.rateLimitTier;
  const windows = shareWindowsOf(accountId, model);
  const tables = tablesFor(client);
  const shares: Record<string, number | null> = Object.fromEntries(windows.map((w) => [w, 0]));
  let equivalent: number | null = 0;
  for (const { at, usage } of priced) {
    if (tokensTotal(usage) === 0) continue;
    const inside = bandAt(model, at);
    equivalent = plus(equivalent, inside === null ? null : equivalentIn(tables, model, usage, inside));
    if (tier !== undefined) for (const w of windows) shares[w] = plus(shares[w], inside === null ? null : shareIn(tables, w, tier, model, usage, inside));
  }
  return { equivalent, shares };
}

// A canceled row is the client's or the router's cut and never an upstream failure, so a headless row with no status
// counts once, under its code.
const upstreamFailed = (r: RequestLog) => r.outcome !== 'canceled' && r.status !== 200 && r.status !== 429 && r.status !== 529;

/** A request's failovers: each attempt on another account than the one before it, as the ledger counts a move. */
const failoversOf = (r: RequestLog): number => r.attempts.filter((a, i) => i > 0 && a.accountId !== r.attempts[i - 1].accountId).length;

/** One count under each key `keyOf` answers for an item, and none for an item it answers null for. */
const tally = <T>(items: readonly T[], keyOf: (item: T) => string | null, into: Record<string, number> = emptyCounts()): Record<string, number> => {
  for (const item of items) {
    const key = keyOf(item);
    if (key !== null) addCount(into, key);
  }
  return into;
};

export function rollups(rows: RequestLog[]): HourlyRollup[] {
  const h = createRng(4_242);
  const groups = new Map<string, RequestLog[]>();
  for (const r of rows) {
    const hour = iso(startOfHour(Date.parse(r.at)));
    // A request answered at the cap before any account served it files under `-`, as the ledger files what it could
    // not attribute. The funding is part of the key, so a lane-hour on both sides of nightly's overage raise splits.
    // The model is the one the router resolved, as the ledger keys it, so an alias files under its resolved id. The
    // client and the agent are the request's own, so a refusal the gate answered under another agent files apart.
    const key = [hour, r.keyId, r.poolId, r.accountId ?? '-', r.canonicalModel, r.kind, r.sessionId, fundingKey(r), r.client ?? '-', agentTokenOf(r.userAgent)].join('|');
    const list = groups.get(key) ?? [];
    list.push(r);
    groups.set(key, list);
  }
  const out: HourlyRollup[] = [];
  for (const [key, list] of groups) {
    const [hour, keyId, poolId, accountId, model, kind, sessionId, funding, client, userAgent] = key.split('|') as [string, string, string, string, string, RequestKind, string, HourlyRollup['funding'], string, string];
    const okRows = list.filter((r) => r.status === 200);
    const isProbe = kind === 'probe';
    // The seeded draws are the lane hour's, taken once each in this order, so splitting its rows by effort and class
    // below moves no drawn figure.
    // Roughly a third of warm reads on the desktop and laptop keys follow a probe
    // closely enough to credit the probe with the hit.
    const kept = new Set(isProbe || !warmsCaches(keyId, poolId) ? [] : okRows.filter((r) => r.usage.cacheRead > 0 && h.chance(0.35)));
    const moved = list.filter((r) => failoversOf(r) > 0 && r.status === 200);
    // The stream was seeded while a retry on the same account counted as a failover, so a lane hour whose answered
    // request retried there still draws the chance, and drops it, which keeps every later draw in its place.
    const retried = list.some((r) => r.status === 200 && r.attempts.some((_, i) => i > 0 && !resentInPlace(r, i)));
    const migrations = retried && h.chance(0.5) && moved.length > 0 ? 1 : 0;
    const probeCostUsd = isProbe ? sumOf(list, (r) => r.costUsd) : 0;
    const probes = isProbe ? { ok: okRows.length, failed: list.length - okRows.length, held: 0, skipped: h.int(0, 2) } : null;
    // The stream was seeded while a probe lane hour drew probes served by another account, so the hour still draws the
    // chance and the count, and drops both, which keeps every later draw in its place.
    if (isProbe && h.chance(0.35)) h.int(1, 2);
    const waste = isProbe ? wastedProbes(h, okRows.length, probeCostUsd) : ZERO_WASTE;
    // A lane's rows share its key, model and kind, which is what decides fast mode in this story; a row refused
    // before any attempt files under the speed it asked for, as the ledger files the speed key alone.
    const speed = foldSpeed(list[0]) === 'fast' ? 'fast' : 'standard';
    // The rollups file each request under its effort, its class and its classifier standing as they fold them, and a
    // probe under no class. The story files a probe under no effort either, so a lane hour's probes stay one row with
    // the probe figures on it.
    const parts = new Map<string, RequestLog[]>();
    for (const r of list) {
      const part = isProbe ? `${NO_EFFORT}|${NO_CLASS}|${NO_CLASS}` : `${foldEffort(r.shape.effort)}|${foldRequestClass(r.requestClass)}|${foldClassifier(r.classifier)}`;
      parts.set(part, [...(parts.get(part) ?? []), r]);
    }
    for (const [part, partRows] of parts) {
      const [effort, requestClass, classifier] = part.split('|');
      // The ledger files a refused request under the refusals map alone, so the counts read the served rows.
      const served = partRows.filter((r) => !r.refused);
      const refused = partRows.filter((r) => r.refused);
      const refusals = tally(refused, (r) => r.errorCode);
      // The ledger counts a served request's own code whatever its outcome, a client's cancel aside.
      const errors = tally(served, (r) => (clientLeft(r) ? null : r.errorCode));
      // The ledger reads a rejected and an exhausted request off its outcome ahead of its status, so the counts by
      // status leave both out and each request counts once.
      const byStatus = served.filter((r) => r.outcome !== 'rejected' && r.outcome !== 'exhausted');
      const ok = served.filter((r) => r.status === 200);
      const cacheMissReasons = tally(partRows.flatMap((r) => r.attempts), (a) => a.cacheMissReason, emptyCacheMissReasons());
      // A hit counts under the serving attempt's reason, the one the route counts a request's reason on.
      const cacheMissReasonHits = tally(partRows, (r) => (hitUnderReason(r) ? servedMissReason(r) : null));
      const dropped: Pick<HourlyRollup, 'inputTransformations' | 'inputTransformationsMoved' | 'inputTransformationsNone'> = { inputTransformations: emptyCounts(), inputTransformationsMoved: 0, inputTransformationsNone: 0 };
      for (const r of partRows) {
        const serving = servingAttempt(r);
        // The routing log writes the story's traces after these rows, so a move reads off the event a trace names.
        countDropped(dropped, serving?.response.inputTransformations ?? null, serving !== null && migrationAt(r, serving) !== undefined);
      }
      const keptHere = partRows.filter((r) => kept.has(r));
      // The ledger gives each counted request one cache class by what it read and wrote, so a request that failed
      // before an answer reads as uncached.
      const uncached = served.filter((r) => r.usage.cacheRead === 0 && r.usage.cacheWrite5m + r.usage.cacheWrite1h === 0);
      const freed = freedPart({ classifier, funding });
      out.push({
        hour,
        client,
        userAgent,
        keyId,
        // One hour never holds two holders of a key, since the live key starts on the hour.
        keyGeneration: list[0].keyGeneration,
        kind,
        poolId,
        accountId,
        model,
        requestClass,
        classifier,
        sessionId,
        funding,
        speed,
        requests: {
          // A stream that ended short committed its 200 and still is no success, so it counts on its own.
          incomplete: served.filter((r) => r.outcome === 'incomplete').length,
          ok: ok.filter((r) => r.outcome === 'succeeded').length,
          rate_limited: byStatus.filter((r) => r.status === 429).length,
          // A stream that failed after its 200 head is a failure, as the ledger files it, under the error it carried.
          overloaded: byStatus.filter((r) => r.status === 529 || (r.status === 200 && r.errorCode === 'overloaded_error')).length,
          // A stream the relay could not decode is an upstream error of its own, counted inside `upstream_error` under
          // the same status filter.
          undecodable: byStatus.filter((r) => upstreamFailed(r) && r.errorCode === 'stream_undecodable').length,
          upstream_error: byStatus.filter(upstreamFailed).length,
          rejected: served.filter((r) => r.outcome === 'rejected').length,
          exhausted: served.filter((r) => r.outcome === 'exhausted').length,
          // The ledger files a stream the client closed, and one the router's own stop cut, under the canceled outcome,
          // each under the code the relay gave it.
          client_gone: served.filter((r) => r.errorCode === 'client_gone').length,
          router_shutdown: served.filter((r) => r.errorCode === 'router_shutdown').length,
        },
        errors,
        refusals,
        attempts: sumOf(partRows, (r) => r.attempts.length),
        failovers: sumOf(served, failoversOf),
        // The lane hour draws one migration, filed with the first request that moved.
        migrations: partRows.includes(moved[0]) ? migrations : 0,
        effort,
        tokens: {
          uncachedInput: sumOf(partRows, (r) => r.usage.input),
          cacheRead: sumOf(partRows, (r) => r.usage.cacheRead),
          cacheWrite5m: sumOf(partRows, (r) => r.usage.cacheWrite5m),
          cacheWrite1h: sumOf(partRows, (r) => r.usage.cacheWrite1h),
          output: sumOf(partRows, (r) => r.usage.output),
          thinking: sumOf(partRows, (r) => r.usage.thinking),
          webFetch: sumOf(partRows, (r) => r.usage.webFetch),
          webSearch: sumOf(partRows, (r) => r.usage.webSearch),
        },
        cacheHits: served.filter((r) => r.usage.cacheRead > 0).length,
        cacheMisses: served.filter((r) => r.usage.cacheRead === 0 && r.usage.cacheWrite5m + r.usage.cacheWrite1h > 0).length,
        cacheMissReasonHits,
        cacheMissReasons,
        expiredBetweenTurns: ok.filter(expiredBetweenTurns).length,
        ...dropped,
        uncacheable: uncached.filter(couldNotCache).length,
        uncached: uncached.length,
        costUsd: sumOf(partRows, (r) => r.costUsd),
        // The router answers a zero split for a call it frees, as it answers its dollars.
        cost: freed ? sumSplit([]) : sumSplit(partRows.map((r) => costSplitOf(r.model, r.usage, servingResponse(r)?.speed === 'fast', r.at))),
        // At each answer's completion, the instant its own shares price it at. A part of freed classifier calls prices
        // nothing and reads zero.
        ...hourShares(accountId, partRows[0].canonicalModel, client, freed ? [] : partRows.map((r) => ({ at: Date.parse(r.at) + (r.durationMs ?? 0), usage: r.usage }))),
        tier: accountTier(accountId),
        histograms: histogramsOf(served),
        keptWarm: { requests: keptHere.length, tokens1h: sumOf(keptHere, (r) => keptOf(r).tokens1h), tokens5m: sumOf(keptHere, (r) => keptOf(r).tokens5m) },
        // The ledger prices a freed call's kept tokens under the row's own key, which reads zero, as its cost does.
        savingUsd: freed ? 0 : sumOf(keptHere, (r) => savingUsdOf(r.model, keptOf(r), promptOf(r.usage), servingResponse(r)?.speed === 'fast', r.at)),
        probes: probes ?? { ok: 0, failed: 0, held: 0, skipped: 0 },
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
/**
 * Splits an hour's requests across the diagnosed reasons, most of them clean. Every lane's body carries a member, the
 * client's or the router's, so an hour whose answers draw no comparison (`compares` false) files each under
 * `not_returned`, as the day's rows do: an hour on an API-key account, which answers without the beta, and a probe
 * hour, whose member the relay detaches. Such an hour still `draws` its split and keeps its expiries, so every later
 * draw keeps its place, but on the `ci` lane, whose hours never drew one. A probe hour holds no turn, and the ledger
 * counts an expiry between turns for a turn alone, so it answers none and still takes the draw.
 */
function synthesizeMissReasons(hm: Rng, ok: number, compares: boolean, draws = true, turns = true): { cacheMissReasons: Record<CacheMissReason, number>; expiredBetweenTurns: number } {
  const uncompared = () => Object.assign(emptyCacheMissReasons(), { not_returned: ok });
  if (!draws) return { cacheMissReasons: uncompared(), expiredBetweenTurns: 0 };
  const cacheMissReasons = emptyCacheMissReasons();
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
  const expired = Math.min(cacheMissReasons.none, hm.chance(0.35) ? hm.int(1, 2) : 0);
  return { cacheMissReasons: compares ? cacheMissReasons : uncompared(), expiredBetweenTurns: turns ? expired : 0 };
}

/**
 * An hour's dropped thinking blocks, inside the bounds the router's fold keeps: each of the `answered` requests counts
 * once, under the client's standing prefix key where its session had compacted, else under none, and an hour whose
 * key sent no token reports nothing. No history row of a model that binds its thinking holds a thinking figure in its
 * usage, so no hour lists the account key or counts a move.
 */
function synthesizeDropped(hd: Rng, lane: Lane, kind: RequestKind, answered: number): Pick<HourlyRollup, 'inputTransformations' | 'inputTransformationsMoved' | 'inputTransformationsNone'> {
  const inputTransformations = emptyCounts();
  const reported = thinkingSourceOf(lane.keyId, lane.poolId, kind) === null ? 0 : answered;
  const prefix = reported > 0 && hd.chance(0.2) ? hd.int(1, Math.min(2, reported)) : 0;
  if (prefix > 0) inputTransformations[PREFIX_KEY] = prefix;
  return { inputTransformations, inputTransformationsMoved: 0, inputTransformationsNone: reported - prefix };
}

// The smoke run reads the holder's day and the mock consistency tests read the cap raise through this module.
export { DELETED_HOLDER_DAY, NIGHTLY_CAP_RAISED } from './time';

/**
 * A synthesized hour's error codes, which no request row carries: each count under the word the relay files it with. A
 * client's cancel files under no code on a row that is no probe, as the ledger leaves it out.
 */
function synthesizedErrors(kind: RequestKind, o: HourlyRollup['requests']): Record<string, number> {
  const errors = emptyCounts();
  const words: [string, number][] = [
    ['rate_limit_error', o.rate_limited],
    ['overloaded_error', o.overloaded],
    ['api_error', o.upstream_error - o.undecodable],
    ['stream_undecodable', o.undecodable],
    ['router_shutdown', o.router_shutdown],
    ['client_gone', kind === 'probe' ? o.client_gone : 0],
  ];
  for (const [code, n] of words) if (n > 0) errors[code] = n;
  return errors;
}

/** An hour's outcome counts at zero, which a synthesized hour sets its own over and a group's sums start from. */
export const ZERO_OUTCOMES: HourlyRollup['requests'] = { client_gone: 0, exhausted: 0, incomplete: 0, ok: 0, overloaded: 0, rate_limited: 0, rejected: 0, router_shutdown: 0, undecodable: 0, upstream_error: 0 };

function historicalRollups(): HourlyRollup[] {
  const h = createRng(99);
  // A separate stream so the reason split does not shift the rest of the hour's draws.
  const hm = createRng(521);
  // Its own stream as well: the desktop key's Opus hours ran part in fast mode.
  const hf = createRng(7_373);
  // And the dropped thinking blocks, so every draw of the streams above keeps its place.
  const hd = createRng(5_519);
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
      // The hour's requests are one aggregate, so its kept tokens split by the share its hour entries wrote.
      const written = tokens.cacheWrite1h + tokens.cacheWrite5m;
      const kept1h = written === 0 ? 0 : Math.round((keptTokens * tokens.cacheWrite1h) / written);
      const keptSplit = { tokens1h: kept1h, tokens5m: keptTokens - kept1h };
      // Six days back, so the hour sits well inside the week and the month the console offers rather than on
      // the edge of the synthesized month, where a range bound would cut it.
      const takesMaxEffort = lane.keyId === 'laptop' && maxEffortOwed && t >= startOfHour(NOW) - 6 * DAY;
      if (takesMaxEffort) maxEffortOwed = false;
      const fast = fastTurn(lane.keyId, model, 'inference') && hf.chance(0.5);
      const outcomes: HourlyRollup['requests'] = { ...ZERO_OUTCOMES, ok, rate_limited: err > 0 && h.chance(0.6) ? err : 0, overloaded: err > 0 && h.chance(0.3) ? 1 : 0 };
      // A failed request read and wrote no cache, so it counts as uncached beside the answers that read and wrote none.
      const uncached = Math.max(0, ok - Math.round(ok * 0.85)) + outcomes.rate_limited + outcomes.overloaded;
      // The hour's first draw after its outcomes, ahead of the literal's own, so the stream keeps its order.
      const failovers = err > 0 ? h.int(1, 3) : h.chance(0.12) ? 1 : 0;
      const inference: HourlyRollup = {
        hour: iso(t),
        client: CLIENTS[lane.keyId] ?? '-',
        userAgent: agentTokenOf(USER_AGENTS[lane.keyId] ?? null),
        effort: lane.keyId === 'desktop' ? 'high' : takesMaxEffort ? 'max' : 'none',
        keyId: lane.keyId,
        keyGeneration: generation,
        kind: 'inference',
        poolId: lane.poolId,
        accountId,
        model,
        requestClass: classOfHour(`${t}|${lane.keyId}|${sessionId}`),
        classifier: NO_CLASS,
        sessionId,
        funding: kindOf(accountId) === 'api_key' ? 'billed' : fast ? 'fast' : 'plan',
        speed: fast ? 'fast' : 'standard',
        requests: outcomes,
        errors: synthesizedErrors('inference', outcomes),
        refusals: {},
        // The hour holds no request rows, so each request it counts made one attempt and each failover one more.
        attempts: ok + outcomes.rate_limited + outcomes.overloaded + failovers,
        failovers,
        migrations: h.chance(0.08) ? 1 : 0,
        tokens,
        cacheHits: Math.round(ok * h.float(0.6, 0.8)),
        cacheMisses: Math.round(ok * h.float(0.1, 0.25)),
        ...synthesizeMissReasons(hm, ok, kindOf(accountId) === 'oauth', lane.keyId !== 'ci'),
        ...synthesizeDropped(hd, lane, 'inference', ok),
        // The route answers no hit map for an hour folded before it counted one.
        cacheMissReasonHits: emptyCounts(),
        // The hour holds the lane's inference turns alone, so its uncached turns count where the lane's turns mark no
        // breakpoint.
        uncacheable: markerOf({ kind: 'inference', poolId: lane.poolId, requestClass: classOfHour(`${t}|${lane.keyId}|${sessionId}`) }, ownMarkerOf({ classifier: null, kind: 'inference', poolId: lane.poolId, sessionId })) ? 0 : uncached,
        uncached,
        costUsd: costOf(model, usage, fast, t),
        cost: costSplitOf(model, usage, fast, t),
        // A synthesized hour holds no request of its own, so its tokens price as one answer at the hour, as its dollars
        // do.
        ...hourShares(accountId, model, CLIENTS[lane.keyId] ?? null, [{ at: t, usage }]),
        tier: accountTier(accountId),
        histograms: {
          durationMs: histogramOf(duration),
          outputTps: histogramOf(sampled(h, ok, big ? 55 : 80, 0.35)),
          ttfbMs: histogramOf(ttfb),
          ttftMs: histogramOf(ttft),
        },
        keptWarm: { requests: keptRequests, ...keptSplit },
        // A synthesized hour's kept requests carry its mean prompt, which selects the cells their saving prices at.
        savingUsd: savingUsdOf(model, keptSplit, ok === 0 ? 0 : promptOf(usage) / ok, fast, t),
        probes: { ok: 0, failed: 0, held: 0, skipped: 0 },
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
        const probeCost = costOf(model, probeUsage, false, t);
        const probeOutcomes: HourlyRollup['requests'] = { ...ZERO_OUTCOMES, ok: probes, upstream_error: failed };
        const histograms = { durationMs: histogramOf(sampled(h, probes, 600, 0.3)), outputTps: emptyHistogram(), ttfbMs: histogramOf(sampled(h, probes, 320, 0.3)), ttftMs: emptyHistogram() };
        const skipped = h.int(0, 2);
        // The stream was seeded while a probe hour drew a probe served by another account between `skipped` and the
        // wasted probes, so the hour still draws that chance there, and drops it, which keeps every later draw in its
        // place.
        h.chance(0.3);
        out.push({
          hour: iso(t),
          client: CLIENTS[lane.keyId] ?? '-',
          userAgent: agentTokenOf(USER_AGENTS[lane.keyId] ?? null),
          effort: 'none',
          keyId: lane.keyId,
          keyGeneration: generation,
          kind: 'probe',
          poolId: lane.poolId,
          accountId,
          model,
          requestClass: '-',
          classifier: NO_CLASS,
          sessionId,
          funding: kindOf(accountId) === 'api_key' ? 'billed' : 'plan',
          speed: 'standard',
          requests: probeOutcomes,
          errors: synthesizedErrors('probe', probeOutcomes),
          refusals: {},
          attempts: probes + failed,
          failovers: 0,
          migrations: 0,
          tokens: { uncachedInput: 0, cacheRead: probeUsage.cacheRead, cacheWrite5m: 0, cacheWrite1h: 0, output: probes, ...NO_ANSWER_FACTS },
          cacheHits: probes,
          cacheMisses: 0,
          ...synthesizeMissReasons(hm, probes, false, true, false),
          ...synthesizeDropped(hd, lane, 'probe', probes),
          cacheMissReasonHits: emptyCounts(),
          uncacheable: 0,
          uncached: failed,
          costUsd: probeCost,
          cost: costSplitOf(model, probeUsage, false, t),
          ...hourShares(accountId, model, CLIENTS[lane.keyId] ?? null, [{ at: t, usage: probeUsage }]),
          tier: accountTier(accountId),
          histograms,
          keptWarm: { requests: 0, tokens1h: 0, tokens5m: 0 },
          savingUsd: 0,
          probes: { ok: probes, failed, held: heldRng.chance(0.04) ? 1 : 0, skipped },
          probeCostUsd: probeCost,
          ...wastedProbes(h, probes, probeCost),
        });
      }
    }
  }
  return out;
}

/** How long after a request's answer its hour-TTL recipe probes, ahead of the entry's expiry. */
const HOUR_PROBE_LEAD = 55 * MINUTE;

/**
 * The work pool keeps the idle notebook session's cache warm after its one long-context turn, and the session never
 * comes back: the hour recipe's one probe finds the entry gone ahead of its expiry and writes the prefix again on the
 * hour, the pool's one-hour idle window then runs out, and the probe settles wasted on that limit. That write costs more
 * than the credits studio's sessions earn in the pool, so the pool's probes cost more than they saved, the story's one
 * pool whose net saving reads under zero.
 */
function idleWarming(rows: RequestLog[]): HourlyRollup[] {
  const r = rows.find((x) => x.id === IDLE_NOTEBOOK_REQUEST_ID);
  if (r === undefined || r.accountId === null) return [];
  const at = Date.parse(r.at) + (r.durationMs ?? 0) + HOUR_PROBE_LEAD;
  const prefix = r.usage.cacheRead + r.usage.cacheWrite5m + r.usage.cacheWrite1h;
  const usage: Usage = { cacheRead: 0, cacheWrite1h: prefix, cacheWrite5m: 0, input: 0, output: 1, ...NO_ANSWER_FACTS };
  const costUsd = costOf(r.model, usage, false, at);
  const wastedProbeReasons = emptyWastedProbeReasons();
  wastedProbeReasons.idle_limit = 1;
  const outcomes: HourlyRollup['requests'] = { ...ZERO_OUTCOMES, ok: 1 };
  return [
    {
      hour: iso(startOfHour(at)),
      client: r.client ?? '-',
      userAgent: agentTokenOf(r.userAgent),
      effort: NO_EFFORT,
      keyId: 'notebook',
      keyGeneration: r.keyGeneration,
      kind: 'probe',
      poolId: r.poolId ?? '-',
      accountId: r.accountId,
      model: r.model,
      requestClass: NO_CLASS,
      classifier: NO_CLASS,
      sessionId: r.sessionId ?? '-',
      funding: kindOf(r.accountId) === 'api_key' ? 'billed' : 'plan',
      speed: 'standard',
      requests: outcomes,
      errors: synthesizedErrors('probe', outcomes),
      refusals: {},
      attempts: 1,
      failovers: 0,
      migrations: 0,
      tokens: { cacheRead: 0, cacheWrite1h: prefix, cacheWrite5m: 0, output: 1, ...NO_ANSWER_FACTS, uncachedInput: 0 },
      cacheHits: 0,
      cacheMisses: 1,
      cacheMissReasonHits: emptyCounts(),
      cacheMissReasons: emptyCacheMissReasons(),
      expiredBetweenTurns: 0,
      // The one probe answered, and it reports under the token rule: none where the key sends no token.
      inputTransformations: emptyCounts(),
      inputTransformationsMoved: 0,
      inputTransformationsNone: thinkingSourceOf(r.keyId, r.poolId, 'probe') === null ? 0 : 1,
      uncacheable: 0,
      uncached: 0,
      costUsd,
      cost: costSplitOf(r.model, usage, false, at),
      ...hourShares(r.accountId, r.canonicalModel, r.client, [{ at, usage }]),
      tier: accountTier(r.accountId),
      histograms: { durationMs: histogramOf([600]), outputTps: emptyHistogram(), ttfbMs: histogramOf([320]), ttftMs: emptyHistogram() },
      keptWarm: { requests: 0, tokens1h: 0, tokens5m: 0 },
      savingUsd: 0,
      probes: { failed: 0, held: 0, ok: 1, skipped: 0 },
      probeCostUsd: costUsd,
      wastedProbeReasons,
      wastedProbeUsd: costUsd,
      wastedProbes: 1,
    },
  ];
}

// The idle warming sits before the logged hours, so the rows' last entry stays the current hour's.
export const ALL_ROLLUPS: HourlyRollup[] = [...historicalRollups(), ...idleWarming(REQUESTS), ...ROLLUPS];

// A probe is the router's own traffic, which the routes count nowhere, but a billed probe draws real dollars, so the
// day's rows hold its row for the cost and billed sums alone, and every count over them skips it.
/**
 * The day's rows from the hour a full day back, which the dashboard's day reads through the registry's `dayRows`, and
 * an account's or a pool's day total and a pool's hourly buckets through `deriveFigures`.
 */
export const DAY_ROWS: RequestLog[] = REQUESTS.filter((r) => (r.kind !== 'probe' || !planPaid(r)) && Date.parse(r.at) >= TOTALS_FROM);
/** The day's rows the counts read, every row but a probe's. */
const DAY_COUNTED_ROWS = DAY_ROWS.filter((r) => r.kind !== 'probe');
const COUNTED_ROLLUPS = ALL_ROLLUPS.filter((r) => r.kind !== 'probe');
/** The rollups a key's totals read, every row but a plan probe's. */
const PRICED_ROLLUPS = ALL_ROLLUPS.filter((r) => r.kind !== 'probe' || r.funding !== 'plan');

/** A sum over a list in its order. A declaration, since the rollups fold reads it while the module loads, above this
 *  line. */
export function sumOf<T>(list: T[], read: (x: T) => number): number {
  return list.reduce((n, x) => n + read(x), 0);
}
/** The member-wise sum of records, each member added from the zero's in list order, the answer in the zero's key
 *  order. A declaration, since `sumSplit` reads it while the module loads, above this line. */
export function sumEach<T extends Record<string, number>>(parts: readonly T[], zero: T): T {
  const s = { ...zero };
  for (const p of parts) for (const k of Object.keys(zero) as (keyof T)[]) s[k] = (s[k] + p[k]) as T[keyof T];
  return s;
}
/** Rollups' billed dollars and their plan rows' Pro-week figure, as the keys route sums a key's rows: an hour's figure
 *  is null where a request with tokens priced nothing, which nulls the sum, and 0 with no plan row. */
const sidesOf = (rows: HourlyRollup[]): Pick<ApiKey['usage7d'], 'billedUsd' | 'equivalent'> => {
  const plan = rows.filter((r) => r.funding === 'plan');
  return {
    billedUsd: sumOf(rows.filter((r) => r.funding !== 'plan'), (r) => r.costUsd),
    equivalent: plan.some((r) => r.equivalent === null) ? null : sumOf(plan, (r) => r.equivalent ?? 0),
  };
};
/**
 * Whether a subscription window paid the request: its serving attempt's funding, which a request no account served has
 * none of. A declaration, since the cost figures read it while the module loads, above this line.
 */
export function planPaid(r: RequestLog): boolean {
  return fundingOf(servingAttempt(r)) === 'plan';
}
/** The same over request rows, as the pools and dashboard routes sum theirs: a plan request with tokens that priced
 *  nothing nulls the figure, and one with no token prices nothing. */
export const requestSidesOf = (rows: RequestLog[]): Pick<TrafficBucket, 'billedUsd' | 'equivalent'> => {
  const plan = rows.filter(planPaid);
  return {
    billedUsd: sumOf(rows.filter((r) => !planPaid(r)), (r) => r.costUsd),
    equivalent: plan.some((r) => tokensTotal(r.usage) > 0 && r.costEquivalent === null) ? null : sumOf(plan, (r) => r.costEquivalent ?? 0),
  };
};
export const requestsIn = (r: HourlyRollup) => r.requests.ok + r.requests.incomplete + r.requests.rate_limited + r.requests.overloaded + r.requests.upstream_error + r.requests.client_gone + r.requests.router_shutdown + r.requests.rejected + r.requests.exhausted;
export const tokensIn = (r: HourlyRollup) => r.tokens.uncachedInput + r.tokens.cacheRead + r.tokens.cacheWrite5m + r.tokens.cacheWrite1h + r.tokens.output;
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
        last24h: DAY_COUNTED_ROWS.filter((r) => r.accountId === a.id).length,
        last7d: sumOf(mine.filter(inWeek), requestsIn),
        lifetime: Math.max(a.requests.lifetime, sumOf(mine, requestsIn)),
      },
    });
  }
  for (const k of keyById.values()) {
    // A bare id totals its live key alone, so an earlier holder's rows count for neither figure. A billed probe's
    // dollars join both dollar sums, and neither its requests nor its tokens count.
    const live = (r: HourlyRollup) => r.keyId === k.id && r.keyGeneration === Date.parse(k.createdAt);
    const mine = PRICED_ROLLUPS.filter(live);
    const counted = COUNTED_ROLLUPS.filter(live);
    const week = mine.filter(inWeek);
    const countedWeek = counted.filter(inWeek);
    const life = sidesOf(mine);
    Object.assign(k, {
      lifetime: {
        billedUsd: Math.max(k.lifetime.billedUsd, life.billedUsd),
        cost: Math.max(k.lifetime.cost, sumOf(mine, (r) => r.costUsd)),
        equivalent: k.lifetime.equivalent === null || life.equivalent === null ? null : Math.max(k.lifetime.equivalent, life.equivalent),
        requests: Math.max(k.lifetime.requests, sumOf(counted, requestsIn)),
      },
      usage7d: { ...sidesOf(week), cost: sumOf(week, (r) => r.costUsd), requests: sumOf(countedWeek, requestsIn), tokens: sumOf(countedWeek, tokensIn) },
    });
  }
  for (const p of poolById.values()) {
    // The route reads the fold, which books a refused request under none of the pool's traffic figures.
    const ofPool = (r: RequestLog) => r.poolId === p.id && !r.refused;
    const mine = DAY_ROWS.filter(ofPool);
    const counted = DAY_COUNTED_ROWS.filter(ofPool);
    const traffic24h: TrafficBucket[] = Array.from({ length: 25 }, (_, i) => {
      const at = TOTALS_FROM + i * HOUR;
      const hour = mine.filter((r) => Date.parse(r.at) >= at && Date.parse(r.at) < at + HOUR);
      return { ...requestSidesOf(hour), at: iso(at), costUsd: 0, failed: 0, requests: 0, tokens: 0 };
    });
    const bucketOf = (r: RequestLog) => traffic24h[Math.floor((Date.parse(r.at) - TOTALS_FROM) / HOUR)];
    for (const r of mine) bucketOf(r).costUsd += r.costUsd;
    for (const r of counted) {
      const bucket = bucketOf(r);
      // A client that hangs up is no failure of the pool, as the route's count reads it.
      bucket.failed += r.outcome === 'succeeded' || clientLeft(r) ? 0 : 1;
      bucket.requests += 1;
      bucket.tokens += tokensTotal(r.usage);
    }
    const day = requestSidesOf(mine);
    Object.assign(p, { day24h: { billedUsd: day.billedUsd, cost: sumOf(mine, (r) => r.costUsd), equivalent: day.equivalent }, requests24h: counted.length, traffic24h });
  }
}

deriveFigures();

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
      // The ledger's highest skips a sample with no figure, which still stands as the hour's last.
      if (sm.utilization !== null) byHour.set(hour, Math.max(byHour.get(hour) ?? 0, sm.utilization));
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
    for (const w of readWindows(lastReadingOf(account))) {
      const key = WINDOW_KEY[w.id];
      const sample = lastSample.get(`${account.id}|${w.id}`)?.get(t);
      if (key === undefined || sample === undefined) continue;
      const sampledMs = Date.parse(sample.at);
      const { resetsAt } = walkBack(w, sampledMs, () => SERVED_FACTOR);
      const accountEnd = endOf(account, sampledMs);
      const reset = resetsAt === null ? null : Date.parse(resetsAt);
      // The route answers no deadline without a reset, even where the account's end stands.
      const deadline = reset === null ? null : iso(Math.min(reset, accountEnd ?? reset));
      const read: WindowSample = { deadline, lead: sample.utilization === null ? null : leadOf({ resetsAt, utilization: sample.utilization }, w.periodHours * HOUR, sampledMs, accountEnd), resetsAt, sampledAt: sample.at, utilization: sample.utilization };
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
    // The ledger keeps the readings of a row whose windows the answer withholds, so its last reading stands in. With no
    // reading `Math.max` answers -Infinity, so such an account holds no hour.
    const since = account.subscription?.since;
    const from = since ? startOfHour(Date.parse(since)) : start;
    const last = lastReadingOf(account);
    const readings = readWindows(last).map((w) => Date.parse(w.observedAt));
    const until = Math.max(...readings);
    // The ledger reads the 5-hour peak off the heads' 5-hour readings, which an account whose set lacks the window
    // never sends. Its drift is still drawn, so every account after it keeps its stream.
    const fiveHour = last.windows.some((w) => w.id === '5h');
    for (let t = start; t < end; t += HOUR) {
      const sampled5 = sampleIndex.get(`${account.id}|5h`)?.get(t);
      const sampled7 = sampleIndex.get(`${account.id}|7d`)?.get(t);
      const sampledF = sampleIndex.get(`${account.id}|7d_fable`)?.get(t);
      drift5 = Math.max(0, Math.min(1, drift5 + h.float(-0.12, 0.12)));
      drift7 = Math.max(0, Math.min(1, drift7 + h.float(-0.03, 0.03)));
      driftF = Math.max(0, Math.min(1, driftF + h.float(-0.03, 0.03)));
      // The drawn figure stands for the heads' readings, so an hour's peak is the larger of it and the hour's highest
      // sample, as the ledger takes both. The cooldown and overage draws key off the sampled figure where one stands,
      // so a peak the drawn figure lifts moves no draw of the stream and no later account's hours.
      const keyed5 = fiveHour ? (sampled5 ?? drift5) : null;
      const peak5 = fiveHour ? Math.max(drift5, sampled5 ?? 0) : null;
      const peak7 = Math.max(drift7, sampled7 ?? 0);
      const peakF = last.fableWindow ? Math.max(driftF, sampledF ?? 0) : null;
      const cooldown = keyed5 !== null && keyed5 >= 0.97 ? h.int(600, 3_600) : 0;
      const overage = account.extraUsage?.enabled && keyed5 !== null && keyed5 >= 0.9 ? h.int(300, 3_600) : 0;
      const busy = new Date(t).getHours() >= 8 && new Date(t).getHours() <= 19;
      const row: AccountRollup = {
        ...ACCOUNT_ROLLUP_SAMPLE,
        accountId: account.id,
        hour: iso(t),
        peak: { '5h': peak5 === null ? null : round(peak5, 3), '7d': round(peak7, 3), '7d_fable': peakF === null ? null : round(peakF, 3) },
        cooldownSeconds: cooldown,
        overageSeconds: overage,
        pins: { created: busy ? h.int(0, 3) : h.chance(0.2) ? 1 : 0, migratedIn: busy && h.chance(0.25) ? 1 : 0, migratedOut: cooldown > 0 ? h.int(1, 3) : 0 },
        windows: sampledWindows(account, t),
      };
      if (t >= from && t <= until) out.push(row);
    }
    // The current hour holds no attempt, so the route lists it with zero counts and its samples' peaks. Its cooldown
    // counts each row the account holds, its own and each one a story attempt's answer set, in the order they were
    // set, from the latest of its start, the end its scope already counted and the hour's start to the earlier of its
    // end and the read. The ledger books a scope's later row only past the end that scope counted, and the route cuts
    // the part it booked past the clock. It draws nothing, so the stream of the hours before it stays.
    if (end >= from && end <= until) {
      const hourPeak = (id: WindowId): number | null => {
        const peak = sampleIndex.get(`${account.id}|${id}`)?.get(end);
        return peak === undefined ? null : round(peak, 3);
      };
      const own = account.cooldowns.map((c) => ({ scope: c.scope, setAt: Date.parse(c.setAt), until: Date.parse(c.until) }));
      const counted = new Map<string, number>();
      const cooledMs = [...own, ...ATTEMPT_COOLDOWNS.filter((c) => c.accountId === account.id)]
        .sort((a, b) => a.setAt - b.setAt)
        .reduce((n, c) => {
          const countedFrom = Math.max(c.setAt, counted.get(c.scope) ?? c.setAt, end);
          counted.set(c.scope, Math.max(counted.get(c.scope) ?? c.until, c.until));
          return n + Math.max(0, Math.min(c.until, NOW) - countedFrom);
        }, 0);
      out.push({
        ...ACCOUNT_ROLLUP_SAMPLE,
        accountId: account.id,
        hour: iso(end),
        peak: { '5h': fiveHour ? hourPeak('5h') : null, '7d': hourPeak('7d'), '7d_fable': last.fableWindow ? hourPeak('7d_fable') : null },
        cooldownSeconds: Math.round(cooledMs / 1_000),
        overageSeconds: 0,
        pins: { created: 0, migratedIn: 0, migratedOut: 0 },
        windows: sampledWindows(account, end),
      });
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

/** The question the client's summary fork asks after the subagent's last turn. */
const FORK_QUESTION = 'Describe your most recent action in 3-5 words using present tense (-ing). Output only the words.';

/** A cut answer ran a megabyte past the cap before the ledger stopped storing it. */
const DROPPED_BYTES = 1_048_576;

/**
 * The page an edge answers an upstream 502 with in place of the API's JSON error, indented with tabs, so a captured
 * body holds a line whose tab stops draw it wider than its character count.
 */
const EDGE_PAGE = [
  '<!DOCTYPE html>',
  '<html lang="en">',
  '<head>',
  '\t<meta charset="utf-8">',
  '\t<title>502 Bad Gateway</title>',
  '</head>',
  '<body>',
  '\t<div class="page">',
  '\t\t<main>',
  '\t\t\t<section class="notice">',
  '\t\t\t\t<div class="notice__text">',
  '\t\t\t\t\t<h1>502 Bad Gateway</h1>',
  "\t\t\t\t\t<p>The edge reached no healthy origin for this request. Try it again in a few moments, and contact the site's owner if it persists.</p>",
  '\t\t\t\t</div>',
  '\t\t\t</section>',
  '\t\t</main>',
  '\t</div>',
  '</body>',
  '</html>',
].join('\n');

export function capturedBodiesFor(request: RequestLog): CapturedBodies | null {
  if (!request.capturedBodies) return null;
  const isClaude = request.model.startsWith('claude');
  const toolResult = { type: 'tool_result', tool_use_id: 'toolu_01', content: '[file contents: 212 lines]' };
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
        // The summary fork copies the subagent's conversation and asks its question after the last tool result.
        { role: 'user', content: FORK_IDS.has(request.id) ? [toolResult, { type: 'text', text: FORK_QUESTION }] : [toolResult] },
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
    stop_reason: servingResponse(request)?.stopReason ?? 'end_turn',
    stop_sequence: null,
    usage: {
      input_tokens: request.usage.input,
      cache_read_input_tokens: request.usage.cacheRead,
      cache_creation_input_tokens: request.usage.cacheWrite5m + request.usage.cacheWrite1h,
      cache_creation: { ephemeral_5m_input_tokens: request.usage.cacheWrite5m, ephemeral_1h_input_tokens: request.usage.cacheWrite1h },
      output_tokens: request.usage.output,
    },
  };
  // A guardrail refused the request before any account answered it, and the ledger keeps no answer the router wrote
  // itself, so the capture holds the request alone.
  if (request.errorCode === 'guardrail_blocked') {
    const requestBytes = byteLength(bodies.request);
    return { ...bodies, response: null, requestBytesSeen: requestBytes, responseBytesSeen: 0, responseBytesStored: 0, responseTruncated: false };
  }
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
  const stored = servingAttempt(request)?.upstreamStatus === 502 ? EDGE_PAGE : response;
  // Counted once here, as the router counts the vectors it stored; the drawer reads these and measures nothing.
  const requestBytes = byteLength(bodies.request);
  const responseBytes = byteLength(stored);
  return {
    ...bodies,
    response: stored,
    requestBytesSeen: requestBytes,
    responseBytesSeen: responseBytes + (request.responseTruncated ? DROPPED_BYTES : 0),
    responseBytesStored: responseBytes,
    responseTruncated: request.responseTruncated,
  };
}

/**
 * The calls the router sent to a feature host, newest first, written out rather than drawn so no row moves a draw of
 * the request rows. Each pairs a key with the account its pool's or its own selection resolves for the feature: desktop
 * with studio for artifacts, laptop with contract for artifacts and with reserve for connectors, ci with reserve for
 * artifacts before its revocation, and notebook with reserve for connectors. A key whose calls are refused (batch's
 * signed-out legacy) leaves no row. The 65 rows run past the page of 50, and the newest 50 hold both shapes of a
 * failed call, canceled calls, the statuses 101, 200, 204, 304, 401, 404 and 429, which a status badge draws in four
 * of its five tones, and a path of 48 characters beside one of 49, which the list cuts. Every row lies inside the
 * default day and after the desktop key's creation.
 */
export const FEATURE_CALLS: FeatureCall[] = [
  { accountId: 'studio', durationMs: 1_284, errorCode: null, feature: 'artifacts', id: 'req_phoebis_73605ad9e70226ee8098aaa84904e49e', keyId: 'desktop', method: 'POST', outcome: 'forwarded', path: '/api/frame/upload', receivedAt: iso(clock(14, 26, 5)), status: 200 },
  { accountId: 'reserve', durationMs: 412, errorCode: null, feature: 'connectors', id: 'req_phoebis_2b4989b8670bb5019fa4cf9e7e106140', keyId: 'notebook', method: 'POST', outcome: 'forwarded', path: '/v1/mcp/srv_7k2m9q4x', receivedAt: iso(clock(14, 25, 41)), status: 200 },
  { accountId: 'studio', durationMs: 236, errorCode: null, feature: 'artifacts', id: 'req_phoebis_be42d733415ac595039b44ca0bb5260d', keyId: 'desktop', method: 'GET', outcome: 'forwarded', path: '/api/frame/versions/quarterly-revenue-dashboards', receivedAt: iso(clock(14, 24, 58)), status: 200 },
  // The answer stopped after its 200 head went back.
  { accountId: 'reserve', durationMs: 31_502, errorCode: null, feature: 'connectors', id: 'req_phoebis_b35ad6164b33b2fb742c08bdb2606e04', keyId: 'notebook', method: 'POST', outcome: 'failed', path: '/v1/mcp/srv_7k2m9q4x', receivedAt: iso(clock(14, 23, 17)), status: 200 },
  { accountId: 'studio', durationMs: 45_093, errorCode: null, feature: 'artifacts', id: 'req_phoebis_c2099984db27a2611353d443a8e9981c', keyId: 'desktop', method: 'GET', outcome: 'forwarded', path: '/api/artifacts/mcp', receivedAt: iso(clock(14, 22, 40)), status: 200 },
  // No upstream head arrived, so the router answered the 502 itself.
  { accountId: 'reserve', durationMs: 10_004, errorCode: 'upstream_unreachable', feature: 'connectors', id: 'req_phoebis_9073a3408c08f2dbb0f9bd0c0c1d6aa7', keyId: 'notebook', method: 'GET', outcome: 'failed', path: '/v1/mcp_servers', receivedAt: iso(clock(14, 21, 3)), status: null },
  { accountId: 'studio', durationMs: 1_903, errorCode: null, feature: 'artifacts', id: 'req_phoebis_b7d2004350f7794871dac7ab2f303f1e', keyId: 'desktop', method: 'POST', outcome: 'forwarded', path: '/api/artifacts/mcp', receivedAt: iso(clock(14, 20, 22)), status: 200 },
  { accountId: 'studio', durationMs: 318, errorCode: null, feature: 'artifacts', id: 'req_phoebis_a748d522f0c028b6bd857780f83db477', keyId: 'desktop', method: 'GET', outcome: 'forwarded', path: '/api/frame/content/onboarding-checklist-for-teams', receivedAt: iso(clock(14, 19, 36)), status: 200 },
  { accountId: 'reserve', durationMs: 96, errorCode: null, feature: 'connectors', id: 'req_phoebis_c2d0cfefc1cd4ffb982d93fde0ddee67', keyId: 'notebook', method: 'GET', outcome: 'forwarded', path: '/v1/mcp_servers', receivedAt: iso(clock(14, 18, 10)), status: 429 },
  { accountId: 'reserve', durationMs: 1_122, errorCode: null, feature: 'artifacts', id: 'req_phoebis_c92f33ee3b33a5eac5fbe885a2b79724', keyId: 'ci', method: 'POST', outcome: 'forwarded', path: '/api/frame/upload', receivedAt: iso(clock(14, 16, 12)), status: 200 },
  { accountId: 'studio', durationMs: 274, errorCode: null, feature: 'artifacts', id: 'req_phoebis_2262fa17c2b6d80ae84f7475181cdd30', keyId: 'desktop', method: 'PATCH', outcome: 'forwarded', path: '/api/frame/perm/art_5d1f8e2b', receivedAt: iso(clock(14, 15, 30)), status: 200 },
  { accountId: 'reserve', durationMs: 133, errorCode: null, feature: 'artifacts', id: 'req_phoebis_74f8851e07ce3b5668f02f61b6e3d78f', keyId: 'ci', method: 'GET', outcome: 'forwarded', path: '/api/frame/read/build-matrix-summary', receivedAt: iso(clock(14, 14, 47)), status: 404 },
  { accountId: 'reserve', durationMs: 8_740, errorCode: null, feature: 'connectors', id: 'req_phoebis_51cc659e7153172c25036a480b2d61b8', keyId: 'notebook', method: 'POST', outcome: 'canceled', path: '/v1/mcp/srv_7k2m9q4x', receivedAt: iso(clock(14, 13, 5)), status: 200 },
  { accountId: 'contract', durationMs: 1_456, errorCode: null, feature: 'artifacts', id: 'req_phoebis_f8af36fc29002b516780bdc2c7d555a7', keyId: 'laptop', method: 'POST', outcome: 'forwarded', path: '/api/frame/upload', receivedAt: iso(clock(14, 11, 52)), status: 200 },
  { accountId: 'reserve', durationMs: 207, errorCode: null, feature: 'connectors', id: 'req_phoebis_48d2c69f2f7fb37368035b1e6aa61e54', keyId: 'laptop', method: 'POST', outcome: 'forwarded', path: '/v1/mcp/srv_3h8w1c6v', receivedAt: iso(clock(14, 10, 33)), status: 401 },
  { accountId: 'studio', durationMs: 162, errorCode: null, feature: 'artifacts', id: 'req_phoebis_42bc9f7e9c75f371493f03b773f50ef5', keyId: 'desktop', method: 'DELETE', outcome: 'forwarded', path: '/api/frame/favorite/art_5d1f8e2b', receivedAt: iso(clock(14, 9, 18)), status: 204 },
  { accountId: 'reserve', durationMs: 341, errorCode: null, feature: 'connectors', id: 'req_phoebis_9e72e91af60e0b4b2d237aa568b778e0', keyId: 'laptop', method: 'GET', outcome: 'forwarded', path: '/v1/mcp_servers', receivedAt: iso(clock(14, 8, 2)), status: 200 },
  { accountId: 'reserve', durationMs: 205, errorCode: null, feature: 'artifacts', id: 'req_phoebis_efe73aa8ee2a9c83f30de01600cb3220', keyId: 'ci', method: 'GET', outcome: 'forwarded', path: '/api/frame/frames', receivedAt: iso(clock(14, 6, 45)), status: 200 },
  { accountId: 'contract', durationMs: 251, errorCode: null, feature: 'artifacts', id: 'req_phoebis_de265a26e462f0e105484b050d3984fa', keyId: 'laptop', method: 'GET', outcome: 'forwarded', path: '/api/frame/versions/release-notes-draft', receivedAt: iso(clock(14, 5, 21)), status: 200 },
  { accountId: 'reserve', durationMs: 588, errorCode: null, feature: 'connectors', id: 'req_phoebis_10ae9abe947db52cec16bb5a4a64e369', keyId: 'laptop', method: 'POST', outcome: 'forwarded', path: '/v1/mcp/srv_3h8w1c6v', receivedAt: iso(clock(14, 3, 58)), status: 200 },
  { accountId: 'studio', durationMs: 690, errorCode: null, feature: 'artifacts', id: 'req_phoebis_6a48b2cfcec74645072c5151f7cecad1', keyId: 'desktop', method: 'POST', outcome: 'forwarded', path: '/api/frame/deploy/direct', receivedAt: iso(clock(14, 2, 14)), status: 200 },
  { accountId: 'reserve', durationMs: 377, errorCode: null, feature: 'connectors', id: 'req_phoebis_ccaa0f1c96d83eb2810c64b0ee003e63', keyId: 'notebook', method: 'POST', outcome: 'forwarded', path: '/v1/mcp/srv_9p4t2n8r', receivedAt: iso(clock(14, 0, 47)), status: 200 },
  { accountId: 'contract', durationMs: 12_406, errorCode: null, feature: 'artifacts', id: 'req_phoebis_9c4cc75281d40ebc3ebc7483cc9cffab', keyId: 'laptop', method: 'GET', outcome: 'canceled', path: '/api/artifacts/mcp', receivedAt: iso(clock(13, 58, 30)), status: 200 },
  { accountId: 'reserve', durationMs: 2_118, errorCode: null, feature: 'artifacts', id: 'req_phoebis_e899f2ebf1501301e8b0301b629773b1', keyId: 'ci', method: 'POST', outcome: 'forwarded', path: '/api/artifacts/mcp', receivedAt: iso(clock(13, 56, 9)), status: 200 },
  { accountId: 'studio', durationMs: 214, errorCode: null, feature: 'artifacts', id: 'req_phoebis_3688129c616fd8f2beac546e169149dd', keyId: 'desktop', method: 'GET', outcome: 'forwarded', path: '/api/frame/content/sprint-retro', receivedAt: iso(clock(13, 54, 41)), status: 304 },
  { accountId: 'reserve', durationMs: 30_115, errorCode: null, feature: 'connectors', id: 'req_phoebis_ed0a86d3f4047cccd8d31c70b57606d0', keyId: 'laptop', method: 'GET', outcome: 'forwarded', path: '/v1/mcp/srv_3h8w1c6v', receivedAt: iso(clock(13, 52, 16)), status: 200 },
  { accountId: 'reserve', durationMs: 288, errorCode: null, feature: 'connectors', id: 'req_phoebis_f17b931053e1b8a4a44b887d859c91d6', keyId: 'notebook', method: 'GET', outcome: 'forwarded', path: '/v1/mcp_servers', receivedAt: iso(clock(13, 49, 55)), status: 200 },
  { accountId: 'studio', durationMs: 119, errorCode: null, feature: 'artifacts', id: 'req_phoebis_fa33e7fd72f8b36b6a1b9bc386a232bf', keyId: 'desktop', method: 'GET', outcome: 'forwarded', path: '/api/frame/read/sprint-retro', receivedAt: iso(clock(13, 47, 23)), status: 404 },
  { accountId: 'contract', durationMs: 84, errorCode: null, feature: 'artifacts', id: 'req_phoebis_69686d82699635217e0200bd3f0b5c4c', keyId: 'laptop', method: 'POST', outcome: 'forwarded', path: '/api/frame/upload', receivedAt: iso(clock(13, 44, 2)), status: 429 },
  { accountId: 'reserve', durationMs: 197, errorCode: null, feature: 'artifacts', id: 'req_phoebis_5f4b5ee58ebb8e9d952454f948f6bb60', keyId: 'ci', method: 'GET', outcome: 'forwarded', path: '/api/frame/versions/build-matrix-summary', receivedAt: iso(clock(13, 41, 37)), status: 200 },
  { accountId: 'reserve', durationMs: 402, errorCode: null, feature: 'connectors', id: 'req_phoebis_1c0bbbe6d765c6e07456e97f5eb769a8', keyId: 'notebook', method: 'POST', outcome: 'forwarded', path: '/v1/mcp/srv_9p4t2n8r', receivedAt: iso(clock(13, 38, 12)), status: 200 },
  { accountId: 'studio', durationMs: 1_377, errorCode: null, feature: 'artifacts', id: 'req_phoebis_6e65d19dd0d942426a3888d9144eff53', keyId: 'desktop', method: 'POST', outcome: 'forwarded', path: '/api/frame/upload', receivedAt: iso(clock(13, 35, 48)), status: 200 },
  { accountId: 'reserve', durationMs: 455, errorCode: null, feature: 'connectors', id: 'req_phoebis_8a6c45cda8de89a9e4ae5f6fe9ff488c', keyId: 'laptop', method: 'POST', outcome: 'forwarded', path: '/v1/mcp/srv_3h8w1c6v', receivedAt: iso(clock(13, 32, 20)), status: 200 },
  { accountId: 'studio', durationMs: 38_640, errorCode: null, feature: 'artifacts', id: 'req_phoebis_ccb00d315dc2f02552ae0cec47e796e7', keyId: 'desktop', method: 'GET', outcome: 'forwarded', path: '/api/artifacts/mcp', receivedAt: iso(clock(13, 29, 4)), status: 200 },
  { accountId: 'reserve', durationMs: 512, errorCode: null, feature: 'artifacts', id: 'req_phoebis_e0699557c291c397c6bd2c77c6c10e69', keyId: 'ci', method: 'POST', outcome: 'forwarded', path: '/api/frame/deploy/prepare', receivedAt: iso(clock(13, 25, 39)), status: 200 },
  { accountId: 'contract', durationMs: 263, errorCode: null, feature: 'artifacts', id: 'req_phoebis_97e7ef588c6ae86e76f349abf72cc09f', keyId: 'laptop', method: 'PATCH', outcome: 'forwarded', path: '/api/frame/perm/art_2c9e7a14', receivedAt: iso(clock(13, 21, 11)), status: 200 },
  { accountId: 'reserve', durationMs: 301, errorCode: null, feature: 'connectors', id: 'req_phoebis_8755fbad8fb6fa52cd3e5d2e4c3ce0e6', keyId: 'notebook', method: 'GET', outcome: 'forwarded', path: '/v1/mcp_servers', receivedAt: iso(clock(13, 17, 45)), status: 200 },
  // A watch's socket, 200 ms behind the page read of its key that handed out its token. Its row ends at the 101, so
  // its duration holds none of the time the socket stays open.
  { accountId: 'studio', durationMs: 142, errorCode: null, feature: 'artifacts', id: 'req_phoebis_3f9d2c71e6a84b05c8e1a7d4b2f60e93', keyId: 'desktop', method: 'GET', outcome: 'forwarded', path: '/api/frame/sync', receivedAt: iso(clock(13, 13, 26) + 200), status: 101 },
  { accountId: 'studio', durationMs: 188, errorCode: null, feature: 'artifacts', id: 'req_phoebis_201bef4cb01b094530575e9dc296f0f8', keyId: 'desktop', method: 'GET', outcome: 'forwarded', path: '/api/frame/frames', receivedAt: iso(clock(13, 13, 26)), status: 200 },
  { accountId: 'reserve', durationMs: 276, errorCode: null, feature: 'connectors', id: 'req_phoebis_e04fd86caed31fffef8721a981a077fd', keyId: 'laptop', method: 'GET', outcome: 'forwarded', path: '/v1/mcp_servers', receivedAt: iso(clock(13, 9, 2)), status: 200 },
  // The feature host answered the first send with a 401, and the client left during the forced refresh behind it,
  // before any head went back.
  { accountId: 'studio', durationMs: 2_904, errorCode: null, feature: 'artifacts', id: 'req_phoebis_4ef85e0b213f8b4155bd28e17e2d279c', keyId: 'desktop', method: 'POST', outcome: 'canceled', path: '/api/frame/upload', receivedAt: iso(clock(13, 4, 37)), status: null },
  { accountId: 'reserve', durationMs: 226, errorCode: null, feature: 'artifacts', id: 'req_phoebis_1521f4281cc38644ed48b04dc328ada0', keyId: 'ci', method: 'GET', outcome: 'forwarded', path: '/api/frame/content/build-matrix-summary', receivedAt: iso(clock(12, 59, 15)), status: 200 },
  { accountId: 'reserve', durationMs: 391, errorCode: null, feature: 'connectors', id: 'req_phoebis_a11a694caa9f909058547b88ff660cd5', keyId: 'notebook', method: 'POST', outcome: 'forwarded', path: '/v1/mcp/srv_9p4t2n8r', receivedAt: iso(clock(12, 53, 48)), status: 200 },
  { accountId: 'contract', durationMs: 174, errorCode: null, feature: 'artifacts', id: 'req_phoebis_f52dc329529094cb770a76df5ac36089', keyId: 'laptop', method: 'GET', outcome: 'forwarded', path: '/api/frame/frames', receivedAt: iso(clock(12, 48, 20)), status: 200 },
  { accountId: 'studio', durationMs: 1_655, errorCode: null, feature: 'artifacts', id: 'req_phoebis_4c42ec0c863e5d49ce0eca5151b246af', keyId: 'desktop', method: 'POST', outcome: 'forwarded', path: '/api/artifacts/mcp', receivedAt: iso(clock(12, 42, 3)), status: 200 },
  { accountId: 'reserve', durationMs: 517, errorCode: null, feature: 'connectors', id: 'req_phoebis_f57d1821ead714fe5841127d2c58c049', keyId: 'laptop', method: 'POST', outcome: 'forwarded', path: '/v1/mcp/srv_3h8w1c6v', receivedAt: iso(clock(12, 35, 41)), status: 200 },
  { accountId: 'reserve', durationMs: 1_048, errorCode: null, feature: 'artifacts', id: 'req_phoebis_d1f79c4c359510e84b63e16ec58006b0', keyId: 'ci', method: 'POST', outcome: 'forwarded', path: '/api/frame/upload', receivedAt: iso(clock(12, 29, 12)), status: 200 },
  { accountId: 'studio', durationMs: 243, errorCode: null, feature: 'artifacts', id: 'req_phoebis_b4aacd947c223da88fb3f208fb31b05c', keyId: 'desktop', method: 'GET', outcome: 'forwarded', path: '/api/frame/versions/sprint-retro', receivedAt: iso(clock(12, 22, 50)), status: 200 },
  { accountId: 'reserve', durationMs: 312, errorCode: null, feature: 'connectors', id: 'req_phoebis_bd8471e1bb77532ff2ccec08744c5950', keyId: 'notebook', method: 'GET', outcome: 'forwarded', path: '/v1/mcp_servers', receivedAt: iso(clock(12, 16, 27)), status: 200 },
  { accountId: 'contract', durationMs: 2_231, errorCode: null, feature: 'artifacts', id: 'req_phoebis_ac496e5e35ca065007c963e3c8b7956e', keyId: 'laptop', method: 'POST', outcome: 'forwarded', path: '/api/artifacts/mcp', receivedAt: iso(clock(12, 9, 5)), status: 200 },
  { accountId: 'studio', durationMs: 207, errorCode: null, feature: 'artifacts', id: 'req_phoebis_afb3df35174a0f729f8e67da8cbe4dcf', keyId: 'desktop', method: 'GET', outcome: 'forwarded', path: '/api/frame/content/sprint-retro', receivedAt: iso(clock(12, 2, 38)), status: 200 },
  { accountId: 'reserve', durationMs: 10_002, errorCode: 'upstream_unreachable', feature: 'connectors', id: 'req_phoebis_07f63543c28061670f9a2fae3e84b628', keyId: 'laptop', method: 'GET', outcome: 'failed', path: '/v1/mcp_servers', receivedAt: iso(clock(11, 55, 14)), status: null },
  { accountId: 'reserve', durationMs: 41_870, errorCode: null, feature: 'artifacts', id: 'req_phoebis_f9730fafa2920e2d2da0b4b3df921f0f', keyId: 'ci', method: 'GET', outcome: 'forwarded', path: '/api/artifacts/mcp', receivedAt: iso(clock(11, 47, 52)), status: 200 },
  { accountId: 'reserve', durationMs: 366, errorCode: null, feature: 'connectors', id: 'req_phoebis_523d678fea856e5318acdabe4de2de6a', keyId: 'notebook', method: 'POST', outcome: 'forwarded', path: '/v1/mcp/srv_9p4t2n8r', receivedAt: iso(clock(11, 40, 29)), status: 200 },
  { accountId: 'studio', durationMs: 604, errorCode: null, feature: 'artifacts', id: 'req_phoebis_dcf7ce17f265044fad5d75f307929031', keyId: 'desktop', method: 'POST', outcome: 'forwarded', path: '/api/frame/deploy/direct', receivedAt: iso(clock(11, 32, 6)), status: 200 },
  { accountId: 'contract', durationMs: 158, errorCode: null, feature: 'artifacts', id: 'req_phoebis_18f113e216c3ae065757057396b4a0a1', keyId: 'laptop', method: 'GET', outcome: 'forwarded', path: '/api/frame/read/release-notes-draft', receivedAt: iso(clock(11, 23, 44)), status: 200 },
  { accountId: 'reserve', durationMs: 295, errorCode: null, feature: 'connectors', id: 'req_phoebis_9ea1e966430c4d8390c7774bbf4ebbcb', keyId: 'notebook', method: 'GET', outcome: 'forwarded', path: '/v1/mcp_servers', receivedAt: iso(clock(11, 15, 21)), status: 200 },
  { accountId: 'studio', durationMs: 1_512, errorCode: null, feature: 'artifacts', id: 'req_phoebis_b2fb52d1667856bc4c0e14312cab3743', keyId: 'desktop', method: 'POST', outcome: 'forwarded', path: '/api/frame/upload', receivedAt: iso(clock(11, 6, 58)), status: 200 },
  { accountId: 'reserve', durationMs: 6_215, errorCode: null, feature: 'connectors', id: 'req_phoebis_3a9c8941f571485f149bde31819eb6c0', keyId: 'laptop', method: 'POST', outcome: 'canceled', path: '/v1/mcp/srv_3h8w1c6v', receivedAt: iso(clock(10, 58, 33)), status: 200 },
  { accountId: 'reserve', durationMs: 192, errorCode: null, feature: 'artifacts', id: 'req_phoebis_803321937ad7cda1bf98893cc38954b1', keyId: 'ci', method: 'GET', outcome: 'forwarded', path: '/api/frame/frames', receivedAt: iso(clock(10, 49, 10)), status: 200 },
  { accountId: 'studio', durationMs: 52_408, errorCode: null, feature: 'artifacts', id: 'req_phoebis_c55a4f97822853029017aec826dc3fc0', keyId: 'desktop', method: 'GET', outcome: 'forwarded', path: '/api/artifacts/mcp', receivedAt: iso(clock(10, 39, 47)), status: 200 },
  { accountId: 'reserve', durationMs: 384, errorCode: null, feature: 'connectors', id: 'req_phoebis_5d138365d4edf5f72d301928351e3868', keyId: 'notebook', method: 'POST', outcome: 'forwarded', path: '/v1/mcp/srv_9p4t2n8r', receivedAt: iso(clock(10, 30, 25)), status: 200 },
  { accountId: 'contract', durationMs: 1_390, errorCode: null, feature: 'artifacts', id: 'req_phoebis_bdfc1a2b1a61828d1f7b81a675d1c4d6', keyId: 'laptop', method: 'POST', outcome: 'forwarded', path: '/api/frame/upload', receivedAt: iso(clock(10, 21, 2)), status: 200 },
  { accountId: 'studio', durationMs: 241, errorCode: null, feature: 'artifacts', id: 'req_phoebis_0ac783b1f5df885ca3c28006e4ba03f7', keyId: 'desktop', method: 'DELETE', outcome: 'forwarded', path: '/api/frame/art_8b3d6f01', receivedAt: iso(clock(10, 11, 38)), status: 200 },
  { accountId: 'reserve', durationMs: 1_977, errorCode: null, feature: 'artifacts', id: 'req_phoebis_6f55e84fbfca0ab0687197e354bc94d0', keyId: 'ci', method: 'POST', outcome: 'forwarded', path: '/api/artifacts/mcp', receivedAt: iso(clock(10, 2, 15)), status: 200 },
];
