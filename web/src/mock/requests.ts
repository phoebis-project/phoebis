import type { AccountRollup, Attempt, AttemptOutcome, AttemptWindow, CacheMissReason, CapturedBodies, CostSplit, LatencyHistograms, PinMigrationReason, RateLimitWindow, RequestEvent, RequestKind, RequestLog, Usage, WastedProbeReason } from '@/api/types';
import { emptyCacheMissReasons, emptyWastedProbeReasons, missCount } from '@/lib/cache';
import { emptyHistogram, histogramOf } from '@/lib/histogram';
import { ACCOUNTS, QUOTA_HISTORY } from './accounts';
import type { HourlyRollup } from './analytics';
import { priceFor } from './catalog';
import { keyById, poolById } from './pools';
import { createRng, type Rng } from './rng';
import { ACCOUNT_ROLLUP_SAMPLE, REQUEST_SAMPLE } from './samples';
import { DAY, HOUR, MINUTE, NOW, daysAgo, iso, startOfHour } from './time';

const rng = createRng(7_331);

export const SESSION_IDS = Array.from({ length: 14 }, () => rng.uuid());

/** A holder of `desktop` created long before the live one and deleted before it, so the two never overlap. */
export const DELETED_HOLDER_GENERATION = Date.parse(daysAgo(120));
const AGENT_IDS = ['main', 'main', 'main', 'a7f3', 'c21e', 'e90b', '4d1a'];

/** One client per key, the way a machine runs one build of Claude Code and a runner runs its own script. */
export const USER_AGENTS: Record<string, string> = {
  ci: 'phoebis-batch/0.9 (external, ci)',
  desktop: 'claude-cli/2.1.269 (external, cli)',
  laptop: 'claude-cli/2.0.44 (external, cli)',
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
    accountIds: ['business', 'personal', 'mail', 'console'],
    models: [
      { value: 'claude-fable-5-1', weight: 8 },
      { value: 'claude-sonnet-5', weight: 6 },
      { value: 'claude-opus-5', weight: 1.5 },
      { value: 'claude-haiku-4-5-20251001', weight: 2 },
    ],
  },
  {
    keyId: 'laptop',
    poolId: 'all',
    accountIds: ['business', 'personal', 'mail'],
    models: [
      { value: 'claude-sonnet-5', weight: 7 },
      { value: 'claude-haiku-4-5-20251001', weight: 3 },
    ],
  },
  {
    keyId: 'ci',
    poolId: 'codex',
    accountIds: ['evals'],
    models: [
      { value: 'claude-haiku-4-5-20251001', weight: 6 },
      { value: 'claude-sonnet-5', weight: 1 },
    ],
  },
];

/** Probes exist only where the pool keeps caches warm and the key has not opted out, and so does anything kept warm by one. */
export function warmsCaches(keyId: string, poolId: string): boolean {
  return poolById.get(poolId)?.keepalive.enabled === true && keyById.get(keyId)?.keepaliveOptOut !== true;
}

const TRAFFIC_KINDS: { value: RequestKind; weight: number }[] = [
  { value: 'inference', weight: 12 },
  { value: 'count_tokens', weight: 2 },
];
const WARMED_KINDS: { value: RequestKind; weight: number }[] = [...TRAFFIC_KINDS, { value: 'probe', weight: 3 }];

/**
 * Walks a window back so a request seen hours ago reports the window that was
 * live at that moment. `olderFactor` is a thunk because it may draw from the
 * seeded stream, and only a window older than the current one needs it.
 */
const walkBack = (w: RateLimitWindow, atMs: number, olderFactor: () => number): AttemptWindow => {
  const period = w.periodHours * HOUR;
  const resetsAt = Date.parse(w.resetsAt ?? '');
  const shift = Math.ceil(Math.max(0, resetsAt - period - atMs) / period);
  const resetThen = resetsAt - shift * period;
  const phase = 1 - (resetThen - atMs) / period;
  const utilization = Math.min(1, Math.max(0.02, w.utilization * (shift === 0 ? phase / Math.max(0.05, 1 - (resetsAt - NOW) / period) : olderFactor())));
  return { id: w.id, resetsAt: iso(resetThen), utilization: Math.round(utilization * 100) / 100 };
};

const oauthWindows = (accountId: string, atMs: number): AttemptWindow[] => {
  const account = ACCOUNTS.find((a) => a.id === accountId);
  if (!account || account.kind !== 'oauth') return [];
  const reported = account.windows.filter((w) => w.id !== '7d_fable').map((w) => walkBack(w, atMs, () => rng.float(0.4, 1.1)));
  // Plans that serve Fable send its weekly headers alongside the other two; the
  // rest send nothing and the detail panel reads n/a. A fixed factor rather than
  // a draw keeps the seeded stream the other fixtures read from where it was.
  if (!account.fableWindow) return reported;
  return [...reported, walkBack(account.fableWindow, atMs, () => 0.8)];
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

export function costSplitOf(model: string, u: Usage): CostSplit {
  const p = priceFor(model);
  return {
    cacheRead: (u.cacheRead * p.cacheReadPerM) / 1_000_000,
    cacheWrite: (u.cacheWrite5m * p.cacheWrite5mPerM + u.cacheWrite1h * p.cacheWrite1hPerM) / 1_000_000,
    input: (u.input * p.inputPerM) / 1_000_000,
    output: (u.output * p.outputPerM) / 1_000_000,
  };
}

export function costOf(model: string, u: Usage): number {
  const c = costSplitOf(model, u);
  return c.input + c.cacheRead + c.cacheWrite + c.output;
}

const sumSplit = (parts: CostSplit[]): CostSplit => parts.reduce((s, c) => ({ cacheRead: s.cacheRead + c.cacheRead, cacheWrite: s.cacheWrite + c.cacheWrite, input: s.input + c.input, output: s.output + c.output }), { cacheRead: 0, cacheWrite: 0, input: 0, output: 0 });

/** What the cache read would have cost as a 5-minute write had the probe not kept it warm. */
function keptWarmSaving(model: string, cacheReadTokens: number): number {
  const p = priceFor(model);
  return (cacheReadTokens * Math.max(0, p.cacheWrite5mPerM - p.cacheReadPerM)) / 1_000_000;
}

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
    // An API-key upstream in this fixture answers without the beta, so the client never sees a comparison.
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

function buildAttempts(lane: Lane, atMs: number, finalStatus: number, attemptsWanted: number): Attempt[] {
  const attempts: Attempt[] = [];
  const pool = [...lane.accountIds];
  let cursor = atMs;
  for (let i = 0; i < attemptsWanted; i += 1) {
    const last = i === attemptsWanted - 1;
    const accountId = pool.length > 1 && i === 0 && lane.poolId === 'all' && rng.chance(0.3) ? 'mail' : rng.pick(pool);
    const idx = pool.indexOf(accountId);
    if (idx >= 0 && pool.length > 1) pool.splice(idx, 1);
    let outcome: AttemptOutcome;
    let upstreamStatus: number;
    if (last) {
      upstreamStatus = finalStatus;
      outcome = finalStatus === 200 ? 'succeeded' : finalStatus === 429 ? 'rate_limited' : 'upstream_error';
    } else {
      upstreamStatus = rng.weighted([{ value: 429, weight: 6 }, { value: 529, weight: 2 }, { value: 502, weight: 1 }]);
      outcome = 'rate_limited';
    }
    const durationMs = last && finalStatus === 200 ? rng.int(900, 22_000) : rng.int(120, 900);
    const windows = oauthWindows(accountId, cursor);
    const rateLimitHit = upstreamStatus === 429;
    attempts.push({
      accountId,
      cacheMissReason: null,
      cooldownSet: rateLimitHit
        ? { reason: `429 on ${windows.find((w) => (w.utilization ?? 0) >= 0.95)?.id ?? '5h'} window`, until: iso(cursor + rng.int(20, 75) * MINUTE) }
        : null,
      durationMs,
      index: i,
      outcome,
      response: { clearedInputTokens: null, inferenceGeo: 'us', refusalCategory: null, refusalRecommendedModel: null, servedModel: null, speed: 'standard', stopReason: 'end_turn', stopSequence: null },
      startedAt: iso(cursor),
      upstreamStatus,
      windows: rateLimitHit ? windows.map((w) => (w.id === '5h' ? { ...w, utilization: 1 } : w)) : windows,
    });
    cursor += durationMs + rng.int(40, 400);
  }
  return attempts;
}

function buildRequests(): RequestLog[] {
  const rows: RequestLog[] = [];
  const span = 24 * HOUR;
  // The drawer's refusal line needs one answer that carries a refusal, so the first eligible row takes it
  // rather than leaving it to the draw below.
  let refusalOwed = true;
  for (let i = 0; i < 232; i += 1) {
    const lane = rng.weighted([{ value: LANES[0], weight: 11 }, { value: LANES[1], weight: 5 }, { value: LANES[2], weight: 1.6 }]);
    // Skew toward recent hours so the dashboard's "recent" table is dense.
    const offset = Math.pow(rng.next(), 1.4) * span;
    const atMs = NOW - offset - rng.int(1, 60) * 1000;
    const hourOfDay = new Date(atMs).getHours();
    if (hourOfDay >= 1 && hourOfDay <= 6 && rng.chance(0.65)) continue;
    const kind = rng.weighted<RequestKind>(warmsCaches(lane.keyId, lane.poolId) ? WARMED_KINDS : TRAFFIC_KINDS);
    const model = rng.weighted(lane.models);
    const finalStatus = kind === 'probe' ? (rng.chance(0.97) ? 200 : 502) : rng.weighted([{ value: 200, weight: 88 }, { value: 429, weight: 5 }, { value: 529, weight: 3 }, { value: 502, weight: 2 }]);
    const attemptsWanted = finalStatus === 200 ? (lane.poolId === 'all' && rng.chance(0.14) ? rng.int(2, 3) : 1) : rng.int(1, 3);
    const tried = buildAttempts(lane, atMs, finalStatus, attemptsWanted);
    const head = tried[tried.length - 1];
    const usage = finalStatus === 200 ? usageFor(model, kind) : { input: 0, cacheRead: 0, cacheWrite5m: 0, cacheWrite1h: 0, output: 0, ...NO_ANSWER_FACTS };
    const diagnosis = diagnosed(tried, kind);
    const refused = refusalOwed && lane.keyId === 'laptop' && kind === 'inference' && finalStatus === 200;
    if (refused) refusalOwed = false;
    const attempts = refused
      ? diagnosis.map((a, at) => (at === diagnosis.length - 1 ? { ...a, response: { ...a.response, refusalCategory: 'cyber', refusalRecommendedModel: 'claude-opus-5', stopReason: 'refusal' } } : a))
      : diagnosis;
    const transport = kind === 'inference' ? (rng.chance(0.86) ? 'sse' : 'json') : 'json';
    const durationMs = attempts.reduce((s, a) => s + (a.durationMs ?? 0), 0);
    const errorCode = finalStatus === 200 ? null : finalStatus === 429 ? 'rate_limit_error' : finalStatus === 529 ? 'overloaded_error' : 'api_error';
    const ttfbMs = finalStatus === 200 && kind !== 'count_tokens' ? rng.int(kind === 'probe' ? 180 : 420, kind === 'probe' ? 900 : 4_200) : null;
    // Streams open with a message_start frame, so the first content token lands a
    // little after the first byte; JSON responses arrive whole.
    const ttftMs = ttfbMs === null || kind !== 'inference' ? ttfbMs : ttfbMs + (transport === 'sse' ? rng.int(40, 600) : 0);
    rows.push({
      ...REQUEST_SAMPLE,
      id: `req_${rng.uuid().slice(0, 12)}`,
      at: iso(atMs),
      keyGeneration: Date.parse(keyById.get(lane.keyId)!.createdAt),
      keyId: lane.keyId,
      poolId: lane.poolId,
      accountId: head.accountId,
      model,
      canonicalModel: model,
      kind,
      outcome: finalStatus === 200 ? 'succeeded' : finalStatus === 429 ? 'exhausted' : 'failed',
      transport,
      status: finalStatus,
      attempts,
      ttfbMs,
      ttftMs,
      durationMs,
      usage,
      costUsd: Math.round(costOf(model, usage) * 10_000) / 10_000,
      errorCode,
      sessionId: rng.pick(SESSION_IDS),
      agentId: rng.pick(AGENT_IDS),
      userAgent: USER_AGENTS[lane.keyId] ?? null,
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
  }
  rows.sort((a, b) => Date.parse(b.at) - Date.parse(a.at));
  // The earlier holder of `desktop` spent the first half of the day on the session the live key now holds, so
  // a session view grouped by key has two lifetimes of one id to keep apart. A probe replays the live key's
  // recipe alone, so none of those rows is one.
  for (const r of rows) {
    if (r.keyId === 'desktop' && r.sessionId === SESSION_IDS[0] && r.kind !== 'probe' && Date.parse(r.at) < NOW - 12 * HOUR) r.keyGeneration = DELETED_HOLDER_GENERATION;
  }
  // A stream can commit its 200 head and fail after it: the newest such desktop stream keeps the status and
  // carries the error, which is the row Observe and the drawer must not draw as a success.
  const failedAfterHead = rows.find((r) => r.keyId === 'desktop' && r.kind === 'inference' && r.transport === 'sse' && r.status === 200);
  if (failedAfterHead) {
    failedAfterHead.errorCode = 'overloaded_error';
    failedAfterHead.outcome = 'failed';
    failedAfterHead.attempts = failedAfterHead.attempts.map((a, i, all) => (i === all.length - 1 ? { ...a, outcome: 'upstream_error' as const } : a));
  }
  // A header sample can carry a window's reset without its utilization; the oldest row that reported the 7-day
  // window stands in for one, so the drawer has a window to read as not reported.
  const unsampled = [...rows].reverse().find((r) => r.attempts.some((a) => a.windows.some((w) => w.id === '7d')));
  if (unsampled) {
    unsampled.attempts = unsampled.attempts.map((a) => ({ ...a, windows: a.windows.map((w) => (w.id === '7d' ? { ...w, utilization: null } : w)) }));
  }
  return rows;
}

export const REQUESTS: RequestLog[] = buildRequests();
export const requestById = new Map(REQUESTS.map((r) => [r.id, r]));

/** A migration reason the fixture draws for a single-attempt row, weighted toward the two the pool's samples drive. */
const MIGRATION_REASONS: { value: PinMigrationReason; weight: number }[] = [
  { value: 'overage', weight: 2 },
  { value: 'spent', weight: 3 },
  { value: 'utilization', weight: 3 },
];

function buildRequestEvents(): Map<string, RequestEvent[]> {
  const h = createRng(8_080);
  const out = new Map<string, RequestEvent[]>();
  // The smoke checks and the drawer need a row that moved for a spent window, so the first eligible row carries one rather than leaving it to the draw below.
  let spentOwed = true;
  let releaseOwed = true;
  for (const r of REQUESTS) {
    if (r.sessionId === null || r.kind === 'probe') continue;
    const events: RequestEvent[] = [];
    // The pin the event belongs to: the drawer shows a move only where these match the request's own.
    const pin = { keyId: r.keyId, model: r.canonicalModel, sessionId: r.sessionId };
    // A failover moves the pin off the account that answered 429; the event lands as the next attempt starts.
    for (let i = 1; i < r.attempts.length; i += 1) {
      const from = r.attempts[i - 1];
      if (from.outcome !== 'rate_limited') continue;
      events.push({ ...pin, accountId: r.attempts[i].accountId, at: r.attempts[i].startedAt, detail: { epoch: i + 1, from_account: from.accountId, reason: 'cooldown' }, kind: 'pin_migrated' });
    }
    // A few single-attempt rows moved on selection instead: the migrate rule read a sample, not a 429.
    const lane = LANES.find((l) => l.keyId === r.keyId);
    // One draw either way for this seed only: the chance the forced row skips comes up false here, and a seed where it came up true would spend a weighted draw as well and shift every later row.
    if (events.length === 0 && r.status === 200 && lane && lane.accountIds.length > 1 && (spentOwed || h.chance(0.07))) {
      const from = h.pick(lane.accountIds.filter((id) => id !== r.accountId));
      events.push({ ...pin, accountId: r.accountId, at: r.at, detail: { epoch: 2, from_account: from, reason: spentOwed ? 'spent' : h.weighted(MIGRATION_REASONS) }, kind: 'pin_migrated' });
      spentOwed = false;
    }
    // One release for the drawer and the smoke checks: a pin that expired on a cooled account whose window was
    // spent, with no sibling holding allowance to move to.
    if (events.length === 0 && releaseOwed && r.status === 200 && lane && lane.accountIds.length > 1) {
      events.push({ ...pin, accountId: r.accountId, at: r.at, detail: { reasons: ['cooldown', 'spent'] }, kind: 'pin_expired' });
      releaseOwed = false;
    }
    if (events.length) out.set(r.id, events);
  }
  return out;
}

/** The events the mock's request detail answers, by request id; a failover writes one, a few rows carry a spent or a hot window. */
export const REQUEST_EVENTS: Map<string, RequestEvent[]> = buildRequestEvents();

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
  { reason: 'idle_limit', weight: 11 },
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

function rollups(): HourlyRollup[] {
  const h = createRng(4_242);
  const groups = new Map<string, RequestLog[]>();
  for (const r of REQUESTS) {
    const hour = iso(startOfHour(Date.parse(r.at)));
    const key = [hour, r.keyId, r.poolId, r.accountId, r.model, r.kind, r.sessionId].join('|');
    const list = groups.get(key) ?? [];
    list.push(r);
    groups.set(key, list);
  }
  const out: HourlyRollup[] = [];
  for (const [key, list] of groups) {
    const [hour, keyId, poolId, accountId, model, kind, sessionId] = key.split('|') as [string, string, string, string, string, RequestKind, string];
    const okRows = list.filter((r) => r.status === 200);
    const isProbe = kind === 'probe';
    const cacheMissReasons = emptyCacheMissReasons();
    for (const r of list) for (const a of r.attempts) if (a.cacheMissReason) cacheMissReasons[a.cacheMissReason] = missCount(cacheMissReasons, a.cacheMissReason) + 1;
    // Roughly a third of warm reads on the desktop and laptop keys follow a probe
    // closely enough to credit the probe with the hit.
    const kept = isProbe || !warmsCaches(keyId, poolId) ? [] : okRows.filter((r) => r.usage.cacheRead > 0 && h.chance(0.35));
    const probeCostUsd = isProbe ? list.reduce((s, r) => s + r.costUsd, 0) : 0;
    out.push({
      hour,
      keyId,
      keyGeneration: Date.parse(keyById.get(keyId)!.createdAt),
      kind,
      poolId,
      accountId,
      model,
      sessionId,
      requests: {
        ok: okRows.length,
        rate_limited: list.filter((r) => r.status === 429).length,
        overloaded: list.filter((r) => r.status === 529).length,
        upstream_error: list.filter((r) => r.status !== 200 && r.status !== 429 && r.status !== 529).length,
      },
      // Every attempt but the last one moved the request to another member.
      failovers: list.reduce((s, r) => s + Math.max(0, r.attempts.length - 1), 0),
      migrations: list.filter((r) => r.attempts.length > 1 && r.status === 200).length > 0 && h.chance(0.5) ? 1 : 0,
      effort: 'none',
      tokens: {
        uncachedInput: list.reduce((s, r) => s + r.usage.input, 0),
        cacheRead: list.reduce((s, r) => s + r.usage.cacheRead, 0),
        cacheWrite5m: list.reduce((s, r) => s + r.usage.cacheWrite5m, 0),
        cacheWrite1h: list.reduce((s, r) => s + r.usage.cacheWrite1h, 0),
        output: list.reduce((s, r) => s + r.usage.output, 0),
        thinking: list.reduce((s, r) => s + r.usage.thinking, 0),
        webFetch: list.reduce((s, r) => s + r.usage.webFetch, 0),
        webSearch: list.reduce((s, r) => s + r.usage.webSearch, 0),
      },
      cacheHits: okRows.filter((r) => r.usage.cacheRead > 0).length,
      cacheMisses: okRows.filter((r) => r.usage.cacheRead === 0 && r.usage.cacheWrite5m + r.usage.cacheWrite1h > 0).length,
      cacheMissReasons,
      expiredBetweenTurns: okRows.filter((r) => r.usage.cacheRead === 0 && r.attempts[r.attempts.length - 1].cacheMissReason === 'none').length,
      uncached: okRows.filter((r) => r.usage.cacheRead === 0 && r.usage.cacheWrite5m + r.usage.cacheWrite1h === 0).length,
      costUsd: list.reduce((s, r) => s + r.costUsd, 0),
      cost: sumSplit(list.map((r) => costSplitOf(r.model, r.usage))),
      histograms: histogramsOf(list),
      keptWarm: { requests: kept.length, tokens: kept.reduce((s, r) => s + r.usage.cacheRead, 0) },
      probes: isProbe ? { ok: okRows.length, failed: list.length - okRows.length, skipped: h.int(0, 2) } : { ok: 0, failed: 0, skipped: 0 },
      probeMigrated: isProbe && h.chance(0.35) ? Math.min(okRows.length, h.int(1, 2)) : 0,
      savings: { grossUsd: kept.reduce((s, r) => s + keptWarmSaving(r.model, r.usage.cacheRead), 0), probeCostUsd },
      ...(isProbe ? wastedProbes(h, okRows.length, probeCostUsd) : ZERO_WASTE),
    });
  }
  return out.sort((a, b) => Date.parse(a.hour) - Date.parse(b.hour));
}

export const ROLLUPS: HourlyRollup[] = rollups();

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

const DELETED_HOLDER_DAY = new Date(NOW - 9 * DAY).toDateString();

function historicalRollups(): HourlyRollup[] {
  const h = createRng(99);
  // A separate stream so the reason split does not shift the rest of the hour's draws.
  const hm = createRng(521);
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
      const laneWeight = lane.keyId === 'desktop' ? 1 : lane.keyId === 'laptop' ? 0.45 : 0.14;
      const n = Math.round(volume * laneWeight * h.float(3, 9));
      if (n === 0) continue;
      const model = h.weighted(lane.models);
      const accountId = h.pick(lane.accountIds);
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
      const inference: HourlyRollup = {
        hour: iso(t),
        effort: lane.keyId === 'desktop' ? 'high' : takesMaxEffort ? 'max' : 'none',
        keyId: lane.keyId,
        keyGeneration: Date.parse(keyById.get(lane.keyId)!.createdAt),
        kind: 'inference',
        poolId: lane.poolId,
        accountId,
        model,
        sessionId,
        requests: { ok, rate_limited: err > 0 && h.chance(0.6) ? err : 0, overloaded: err > 0 && h.chance(0.3) ? 1 : 0, upstream_error: 0 },
        failovers: err > 0 ? h.int(1, 3) : h.chance(0.12) ? 1 : 0,
        migrations: h.chance(0.08) ? 1 : 0,
        tokens,
        cacheHits: Math.round(ok * h.float(0.6, 0.8)),
        cacheMisses: Math.round(ok * h.float(0.1, 0.25)),
        ...synthesizeMissReasons(hm, ok, lane.keyId !== 'ci'),
        uncached: Math.max(0, ok - Math.round(ok * 0.85)),
        costUsd: costOf(model, usage),
        cost: costSplitOf(model, usage),
        histograms: {
          durationMs: histogramOf(duration),
          outputTps: histogramOf(sampled(h, ok, big ? 55 : 80, 0.35)),
          ttfbMs: histogramOf(ttfb),
          ttftMs: histogramOf(ttft),
        },
        keptWarm: { requests: keptRequests, tokens: keptTokens },
        probes: { ok: 0, failed: 0, skipped: 0 },
        probeMigrated: 0,
        savings: { grossUsd: keptWarmSaving(model, keptTokens), probeCostUsd: 0 },
        ...ZERO_WASTE,
      };
      if (unknownReasonOwed && lane.keyId === 'desktop' && t >= startOfHour(NOW) - 12 * DAY && inference.cacheMissReasons.none > inference.expiredBetweenTurns) {
        unknownReasonOwed = false;
        inference.cacheMissReasons.future_reason = 1;
        inference.cacheMissReasons.none -= 1;
      }
      out.push(inference);
      // The hours an earlier holder of `desktop` spent, deleted before the live key was created: one day of
      // the second week back, so the month answers the pair of key groups and the shorter ranges do not.
      if (lane.keyId === 'desktop' && daylight === 1 && hourOfDay < 11 && new Date(t).toDateString() === DELETED_HOLDER_DAY) {
        out.push({ ...inference, keyGeneration: DELETED_HOLDER_GENERATION });
      }
      // Keepalive probes ride on the pinned key; one row per lane-hour keeps
      // the "include probes" toggle meaningful across the longer ranges.
      if (warmsCaches(lane.keyId, lane.poolId) && daylight === 1) {
        const probes = h.int(2, 6);
        const failed = h.chance(0.08) ? 1 : 0;
        const probeUsage: Usage = { input: 0, cacheRead: probes * h.int(30_000, 80_000), cacheWrite5m: 0, cacheWrite1h: 0, output: probes, ...NO_ANSWER_FACTS };
        const probeCost = costOf(model, probeUsage);
        out.push({
          hour: iso(t),
          effort: 'none',
          keyId: lane.keyId,
          keyGeneration: Date.parse(keyById.get(lane.keyId)!.createdAt),
          kind: 'probe',
          poolId: lane.poolId,
          accountId,
          model,
          sessionId,
          requests: { ok: probes, rate_limited: 0, overloaded: 0, upstream_error: failed },
          failovers: 0,
          migrations: 0,
          tokens: { uncachedInput: 0, cacheRead: probeUsage.cacheRead, cacheWrite5m: 0, cacheWrite1h: 0, output: probes, ...NO_ANSWER_FACTS },
          cacheHits: probes,
          cacheMisses: 0,
          ...synthesizeMissReasons(hm, probes, true),
          uncached: 0,
          costUsd: probeCost,
          cost: costSplitOf(model, probeUsage),
          histograms: { durationMs: histogramOf(sampled(h, probes, 600, 0.3)), outputTps: emptyHistogram(), ttfbMs: histogramOf(sampled(h, probes, 320, 0.3)), ttftMs: emptyHistogram() },
          keptWarm: { requests: 0, tokens: 0 },
          probes: { ok: probes, failed, skipped: h.int(0, 2) },
          probeMigrated: h.chance(0.3) ? 1 : 0,
          savings: { grossUsd: 0, probeCostUsd: probeCost },
          ...wastedProbes(h, probes, probeCost),
        });
      }
    }
  }
  return out;
}

export const ALL_ROLLUPS: HourlyRollup[] = [...historicalRollups(), ...ROLLUPS];

// Hourly account facts: peak utilization per window, time spent ineligible, and
// pin churn. The last seven days follow the sampled quota history; older hours
// are synthesized with the same shape.
function accountRollups(): AccountRollup[] {
  const h = createRng(7_777);
  const out: AccountRollup[] = [];
  const start = startOfHour(NOW) - 30 * DAY;
  const end = startOfHour(NOW);
  const sampleIndex = new Map<string, Map<number, number>>();
  for (const hist of QUOTA_HISTORY) {
    const byHour = new Map<number, number>();
    for (const sm of hist.samples) {
      const hour = startOfHour(Date.parse(sm.at));
      byHour.set(hour, Math.max(byHour.get(hour) ?? 0, sm.utilization));
    }
    sampleIndex.set(`${hist.accountId}|${hist.windowId}`, byHour);
  }
  for (const account of ACCOUNTS) {
    // An upstream never paired has never been sampled, so it has no hours to report.
    if (account.kind !== 'oauth' || account.status === 'unpaired') continue;
    let drift5 = h.float(0.2, 0.7);
    let drift7 = h.float(0.3, 0.8);
    let driftF = h.float(0.05, 0.4);
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
      out.push({
        ...ACCOUNT_ROLLUP_SAMPLE,
        accountId: account.id,
        hour: iso(t),
        peak: { '5h': Math.round(peak5 * 1000) / 1000, '7d': Math.round(peak7 * 1000) / 1000, '7d_fable': peakF === null ? null : Math.round(peakF * 1000) / 1000 },
        cooldownSeconds: cooldown,
        overageSeconds: overage,
        pins: { created: busy ? h.int(0, 3) : h.chance(0.2) ? 1 : 0, migratedIn: busy && h.chance(0.25) ? 1 : 0, migratedOut: cooldown > 0 ? h.int(1, 3) : 0 },
      });
    }
  }
  return out;
}

export const ACCOUNT_ROLLUPS: AccountRollup[] = accountRollups();

/** What the ledger counted as it read the body, which is the only figure the drawer's "bytes dropped" may use. */
const byteLength = (v: unknown): number => new TextEncoder().encode(JSON.stringify(v)).length;

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
    response: {
      id: `msg_${request.id.slice(4)}`,
      type: 'message',
      role: 'assistant',
      model: isClaude ? request.model : `${request.model}-2026-05-01`,
      content: [{ type: 'text', text: 'The hook awaits a promise that resolves before the writer flushes. Moving the flush into the same tick fixes the race.' }],
      stop_reason: 'end_turn',
      usage: {
        input_tokens: request.usage.input,
        cache_read_input_tokens: request.usage.cacheRead,
        cache_creation_input_tokens: request.usage.cacheWrite5m + request.usage.cacheWrite1h,
        cache_creation: { ephemeral_5m_input_tokens: request.usage.cacheWrite5m, ephemeral_1h_input_tokens: request.usage.cacheWrite1h },
        output_tokens: request.usage.output,
      },
    },
  };
  // Counted once here, as the router counts the vectors it stored; the drawer reads these and measures nothing.
  const requestBytes = byteLength(bodies.request);
  const responseBytes = byteLength(bodies.response);
  return {
    ...bodies,
    requestBytesSeen: requestBytes,
    requestBytesStored: requestBytes,
    requestTruncated: request.requestTruncated,
    responseBytesSeen: responseBytes + (request.responseTruncated ? DROPPED_BYTES : 0),
    responseBytesStored: responseBytes,
    responseTruncated: request.responseTruncated,
  };
}
