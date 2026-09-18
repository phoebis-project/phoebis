import type { AttemptWindow, LiveAttempt, LiveDelta, LiveEvent, LiveMetric, LivePhases, LiveRequest, LiveSnapshot, LiveTick, RequestKind, RequestLog, Transport, Usage } from '@/api/types';
import { histogramOf, percentileFromHistogram } from '@/lib/histogram';
import { ACCOUNTS } from './accounts';
import { createRng } from './rng';
import { LANES, REQUESTS, SESSION_IDS, costOf, warmsCaches, type Lane } from './requests';
import { MINUTE, iso } from './time';

const TRAFFIC_KINDS: { value: RequestKind; weight: number }[] = [
  { value: 'inference', weight: 12 },
  { value: 'count_tokens', weight: 2 },
];
const WARMED_KINDS: { value: RequestKind; weight: number }[] = [...TRAFFIC_KINDS, { value: 'probe', weight: 2 }];

const RING_CAP = 2_048;
const RING_SPAN = 30 * MINUTE;
const MIN_IN_FLIGHT = 3;
const MAX_IN_FLIGHT = 8;

const PREVIEW_SOURCE =
  'The hook awaits a promise that resolves before the writer flushes, so the last line is lost whenever the process exits early. ' +
  'Moving the flush into the same tick removes the race. I will add a regression test that spawns the hook with a short buffer and asserts the trailing newline. ' +
  'The retry path also swallowed the exit code; it now propagates it so CI fails loudly instead of passing on a partial write. ' +
  'Running the suite now to confirm nothing else depended on the old ordering.';
const PREVIEW_WORDS = PREVIEW_SOURCE.split(' ');

const rng = createRng(31_337);
// The live feed replays the same sessions the sessions view lists, so arriving
// from a row with ?session= lands on traffic rather than an empty ring.
const LIVE_SESSION_IDS = SESSION_IDS.slice(0, 9);
const AGENT_IDS = ['main', 'main', 'main', 'a7f3', 'c21e', 'e90b'];

interface Plan {
  attemptsPlanned: number;
  durationMs: number;
  failoverAt: number;
  finalStatus: number;
  finalUsage: Usage;
  firstContentAt: number;
  headAt: number;
  kind: RequestKind;
  lastProgressAt: number;
  model: string;
  nextPreviewAt: number;
  previewIndex: number;
  selectAt: number;
  startedMs: number;
  streamEndAt: number;
  writtenAt: number;
}

const windowsFor = (accountId: string): AttemptWindow[] => {
  const account = ACCOUNTS.find((a) => a.id === accountId);
  if (!account || account.kind !== 'oauth') return [];
  return account.windows.map((w) => ({ id: w.id, resetsAt: w.resetsAt, utilization: w.utilization }));
};

const emptyUsage = (): Usage => ({ cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: 0, input: 0, output: 0 });

function finalUsageFor(model: string, kind: RequestKind, status: number): Usage {
  if (status !== 200) return emptyUsage();
  if (kind === 'probe') return { ...emptyUsage(), cacheRead: rng.int(30_000, 90_000), output: 1 };
  if (kind === 'count_tokens') return { ...emptyUsage(), input: rng.int(2_000, 60_000) };
  const big = model.includes('fable') || model.includes('opus');
  const context = rng.int(big ? 40_000 : 12_000, big ? 180_000 : 90_000);
  const cacheRead = rng.chance(0.75) ? Math.round(context * rng.float(0.8, 0.98)) : 0;
  const fresh = context - cacheRead;
  const writes1h = rng.chance(0.35);
  return {
    cacheRead,
    cacheWrite1h: writes1h ? Math.round(fresh * rng.float(0.6, 0.95)) : 0,
    cacheWrite5m: writes1h ? 0 : Math.round(fresh * rng.float(0.6, 0.95)),
    input: Math.round(fresh * rng.float(0.05, 0.35)),
    output: rng.int(big ? 400 : 120, big ? 6_000 : 2_400),
  };
}

/** Finished requests from the fixture ledger become ring entries with phase times reconstructed from their durations. */
function fromLog(r: RequestLog): LiveRequest {
  const at = Date.parse(r.at);
  const ok = r.status === 200;
  const durationMs = r.durationMs ?? 0;
  const phases: LivePhases = { receivedAt: r.at, selectedAt: iso(at + 60), writtenAt: iso(at + 140) };
  if (r.ttfbMs !== null) phases.headAt = iso(at + r.ttfbMs);
  if (ok && r.kind === 'inference' && r.ttftMs !== null) {
    phases.firstContentAt = iso(at + r.ttftMs);
    phases.streamEndAt = iso(at + Math.max(r.ttftMs + 10, durationMs - 40));
  }
  phases.finishedAt = iso(at + durationMs);
  const streaming = r.ttftMs !== null ? Math.max(50, durationMs - r.ttftMs) : null;
  return {
    accountId: r.accountId,
    agentId: r.agentId ?? 'main',
    attempts: r.attempts.map((a) => ({ accountId: a.accountId, cooldownSet: a.cooldownSet, index: a.index, outcome: a.outcome, startedAt: a.startedAt, upstreamStatus: a.upstreamStatus, windows: a.windows })),
    costUsd: r.costUsd,
    errorCode: r.errorCode,
    id: r.id,
    keyId: r.keyId,
    kind: r.kind,
    model: r.model,
    outputTps: ok && r.kind === 'inference' && streaming ? (r.usage.output / streaming) * 1000 : null,
    phases,
    poolId: r.poolId,
    preview: r.keyId === 'laptop' && ok && r.kind === 'inference' ? PREVIEW_WORDS.slice(0, rng.int(12, PREVIEW_WORDS.length)).join(' ') : null,
    sessionId: r.sessionId,
    state: 'finished',
    status: r.status,
    transport: r.transport,
    usage: r.usage,
  };
}

const metric = (value: number | null, series: number[], quality: LiveMetric['quality']): LiveMetric => ({ quality, series, value });

class LiveFeed {
  private cursor = 0;
  private handlers = new Set<(e: LiveEvent) => void>();
  private inFlight = new Map<string, { plan: Plan; request: LiveRequest }>();
  private loopTimer: ReturnType<typeof setTimeout> | null = null;
  private recent: LiveRequest[] = [];
  private series = new Map<keyof LiveTick['metrics'], number[]>();
  private tickTimer: ReturnType<typeof setInterval> | null = null;

  constructor() {
    const now = Date.now();
    const seeded = REQUESTS.filter((r) => Date.parse(r.at) >= now - RING_SPAN).map(fromLog);
    // The ledger is sparse over the last half hour; extra rows give the scroll-back something to show.
    const extra: LiveRequest[] = [];
    for (let i = 0; i < 48; i += 1) {
      const lane = rng.weighted([{ value: LANES[0], weight: 11 }, { value: LANES[1], weight: 5 }, { value: LANES[2], weight: 1.6 }]);
      const startedMs = now - rng.int(20, RING_SPAN / 1000 - 60) * 1000;
      const plan = this.planFor(lane, startedMs);
      const req = this.requestFor(lane, plan);
      extra.push(this.settle(req, plan, plan.startedMs + plan.durationMs));
    }
    this.recent = [...seeded, ...extra].sort((a, b) => Date.parse(b.phases.receivedAt) - Date.parse(a.phases.receivedAt));
    // Backfill a minute of metric history from the ring so the first paint has sparklines.
    const keys: (keyof LiveTick['metrics'])[] = ['activeStreams', 'costPerMinute', 'errorRate', 'outputTps', 'requestsPerSecond', 'ttfbP50', 'ttfbP95', 'ttfbP99'];
    for (const k of keys) this.series.set(k, []);
    for (let t = now - 59_000; t <= now; t += 1000) {
      const m = this.measure(t);
      for (const k of keys) this.push(k, m[k]);
    }
    // Seeds may replay to completion at once (short count_tokens calls), so top up until the floor holds.
    for (let i = 0; i < 20 && this.inFlight.size < MIN_IN_FLIGHT + 1; i += 1) this.start(now - rng.int(400, 9_000));
  }

  subscribe(handler: (e: LiveEvent) => void): () => void {
    this.handlers.add(handler);
    handler({ kind: 'snapshot', snapshot: this.snapshot() });
    if (this.handlers.size === 1) this.run();
    return () => {
      this.handlers.delete(handler);
      if (this.handlers.size === 0) this.stop();
    };
  }

  private snapshot(): LiveSnapshot {
    return {
      cursor: this.cursor,
      inFlight: [...this.inFlight.values()].map((e) => ({ ...e.request, attempts: [...e.request.attempts], phases: { ...e.request.phases }, usage: { ...e.request.usage } })),
      recent: this.recent.map((r) => ({ ...r })),
      tick: this.tick(Date.now()),
    };
  }

  private run() {
    if (this.loopTimer) return;
    const step = () => {
      this.advance(Date.now());
      this.loopTimer = setTimeout(step, rng.int(300, 900));
    };
    this.loopTimer = setTimeout(step, rng.int(300, 900));
    this.tickTimer = setInterval(() => this.emit({ cursor: this.next(), tick: this.tick(Date.now()), type: 'tick' }), 1000);
  }

  private stop() {
    if (this.loopTimer) clearTimeout(this.loopTimer);
    if (this.tickTimer) clearInterval(this.tickTimer);
    this.loopTimer = null;
    this.tickTimer = null;
  }

  private next() {
    this.cursor += 1;
    return this.cursor;
  }

  private emit(delta: LiveDelta) {
    for (const h of this.handlers) h({ kind: 'delta', delta });
  }

  private push(key: keyof LiveTick['metrics'], value: number | null) {
    const s = this.series.get(key)!;
    s.push(value ?? 0);
    if (s.length > 60) s.shift();
  }

  /** Metrics over the minute ending at `t`: finished rows by finish time, streams by their span. */
  private measure(t: number): Record<keyof LiveTick['metrics'], number | null> {
    const from = t - 60_000;
    const finished = this.recent.filter((r) => {
      const f = Date.parse(r.phases.finishedAt ?? r.phases.receivedAt);
      return f > from && f <= t;
    });
    const streaming = [...this.recent, ...[...this.inFlight.values()].map((e) => e.request)].filter((r) => {
      const start = Date.parse(r.phases.firstContentAt ?? '') || null;
      const end = r.phases.streamEndAt ? Date.parse(r.phases.streamEndAt) : Number.POSITIVE_INFINITY;
      return r.transport === 'sse' && start !== null && start <= t && end >= t;
    });
    const ttfbs = finished.map((r) => (r.phases.headAt ? Date.parse(r.phases.headAt) - Date.parse(r.phases.receivedAt) : null)).filter((v): v is number => v !== null && v > 0);
    const hist = histogramOf(ttfbs);
    return {
      activeStreams: streaming.length,
      costPerMinute: finished.reduce((s, r) => s + r.costUsd, 0),
      errorRate: finished.length ? finished.filter((r) => r.status !== 200).length / finished.length : null,
      outputTps: streaming.reduce((s, r) => s + (r.outputTps ?? 0), 0),
      requestsPerSecond: finished.length / 60,
      ttfbP50: percentileFromHistogram(hist, 0.5),
      ttfbP95: percentileFromHistogram(hist, 0.95),
      ttfbP99: percentileFromHistogram(hist, 0.99),
    };
  }

  private tick(t: number): LiveTick {
    const m = this.measure(t);
    for (const k of Object.keys(m) as (keyof LiveTick['metrics'])[]) this.push(k, m[k]);
    const s = (k: keyof LiveTick['metrics']) => [...this.series.get(k)!];
    return {
      at: iso(t),
      metrics: {
        activeStreams: metric(m.activeStreams, s('activeStreams'), 'measured'),
        costPerMinute: metric(m.costPerMinute, s('costPerMinute'), 'estimated'),
        errorRate: metric(m.errorRate, s('errorRate'), 'derived'),
        outputTps: metric(m.outputTps, s('outputTps'), 'measured'),
        requestsPerSecond: metric(m.requestsPerSecond, s('requestsPerSecond'), 'measured'),
        ttfbP50: metric(m.ttfbP50, s('ttfbP50'), 'derived'),
        ttfbP95: metric(m.ttfbP95, s('ttfbP95'), 'derived'),
        ttfbP99: metric(m.ttfbP99, s('ttfbP99'), 'derived'),
      },
    };
  }

  private planFor(lane: Lane, startedMs: number): Plan {
    const kind = rng.weighted<RequestKind>(warmsCaches(lane.keyId, lane.poolId) ? WARMED_KINDS : TRAFFIC_KINDS);
    const model = rng.weighted(lane.models);
    const finalStatus = kind === 'probe' ? (rng.chance(0.97) ? 200 : 502) : rng.weighted([{ value: 200, weight: 90 }, { value: 429, weight: 4 }, { value: 529, weight: 3 }, { value: 502, weight: 3 }]);
    const inference = kind === 'inference';
    const durationMs = !inference ? rng.int(300, 1_500) : finalStatus === 200 ? rng.int(2_000, 40_000) : rng.int(600, 2_500);
    const headAt = inference ? Math.min(durationMs - 100, rng.int(420, 4_200)) : Math.min(durationMs - 50, rng.int(180, 900));
    const firstContentAt = headAt + rng.int(40, 600);
    const attemptsPlanned = lane.poolId === 'all' && rng.chance(0.15) ? 2 : 1;
    const selectAt = rng.int(30, 200);
    return {
      attemptsPlanned,
      durationMs,
      failoverAt: selectAt + rng.int(300, 900),
      finalStatus,
      finalUsage: finalUsageFor(model, kind, finalStatus),
      firstContentAt,
      headAt,
      kind,
      lastProgressAt: 0,
      model,
      nextPreviewAt: firstContentAt,
      previewIndex: 0,
      selectAt,
      startedMs,
      streamEndAt: Math.max(firstContentAt + 20, durationMs - rng.int(20, 120)),
      writtenAt: selectAt + rng.int(20, 120),
    };
  }

  private requestFor(lane: Lane, plan: Plan): LiveRequest {
    const transport: Transport = plan.kind === 'inference' && rng.chance(0.86) ? 'sse' : 'json';
    return {
      accountId: null,
      agentId: rng.pick(AGENT_IDS),
      attempts: [],
      costUsd: 0,
      errorCode: null,
      id: `req_${rng.uuid().slice(0, 12)}`,
      keyId: lane.keyId,
      kind: plan.kind,
      model: plan.model,
      outputTps: null,
      phases: { receivedAt: iso(plan.startedMs) },
      poolId: lane.poolId,
      preview: null,
      sessionId: rng.pick(LIVE_SESSION_IDS),
      state: 'in_flight',
      status: null,
      transport,
      usage: emptyUsage(),
    };
  }

  private attemptFor(req: LiveRequest, index: number, at: number, exclude: string[]): LiveAttempt {
    const lane = LANES.find((l) => l.keyId === req.keyId) ?? LANES[0];
    const candidates = lane.accountIds.filter((a) => !exclude.includes(a));
    const accountId = rng.pick(candidates.length ? candidates : lane.accountIds);
    return { accountId, cooldownSet: null, index, outcome: 'in_flight', startedAt: iso(at), upstreamStatus: null, windows: windowsFor(accountId) };
  }

  /** Runs every phase of a request straight through; used to seed the ring with finished rows. */
  private settle(req: LiveRequest, plan: Plan, finishAt: number): LiveRequest {
    const s = plan.startedMs;
    const first = this.attemptFor(req, 0, s + plan.selectAt, []);
    req.attempts = [first];
    if (plan.attemptsPlanned > 1) {
      first.outcome = 'rate_limited';
      first.upstreamStatus = 429;
      first.cooldownSet = { reason: '429 on 5h window', until: iso(s + rng.int(20, 75) * MINUTE) };
      req.attempts.push(this.attemptFor(req, 1, s + plan.failoverAt, [first.accountId]));
    }
    const head = req.attempts[req.attempts.length - 1];
    head.upstreamStatus = plan.finalStatus;
    head.outcome = plan.finalStatus === 200 ? 'succeeded' : plan.finalStatus === 429 ? 'rate_limited' : 'upstream_error';
    req.accountId = head.accountId;
    req.status = plan.finalStatus;
    req.errorCode = plan.finalStatus === 200 ? null : plan.finalStatus === 429 ? 'rate_limit_error' : plan.finalStatus === 529 ? 'overloaded_error' : 'api_error';
    req.usage = plan.finalUsage;
    req.costUsd = Math.round(costOf(req.model, req.usage) * 10_000) / 10_000;
    req.phases = { receivedAt: iso(s), selectedAt: iso(s + plan.selectAt), writtenAt: iso(s + plan.writtenAt), headAt: iso(s + plan.headAt), finishedAt: iso(finishAt) };
    if (plan.finalStatus === 200 && req.kind === 'inference') {
      req.phases.firstContentAt = iso(s + plan.firstContentAt);
      req.phases.streamEndAt = iso(s + plan.streamEndAt);
      req.outputTps = (plan.finalUsage.output / Math.max(50, plan.streamEndAt - plan.firstContentAt)) * 1000;
      if (req.keyId === 'laptop') req.preview = PREVIEW_WORDS.slice(0, rng.int(12, PREVIEW_WORDS.length)).join(' ');
    }
    req.state = 'finished';
    return req;
  }

  private start(startedMs: number) {
    const lane = rng.weighted([{ value: LANES[0], weight: 11 }, { value: LANES[1], weight: 5 }, { value: LANES[2], weight: 1.6 }]);
    const plan = this.planFor(lane, startedMs);
    const request = this.requestFor(lane, plan);
    this.inFlight.set(request.id, { plan, request });
    this.emit({ cursor: this.next(), request: { ...request, attempts: [...request.attempts], phases: { ...request.phases }, usage: { ...request.usage } }, type: 'started' });
    // Seeded rows started in the past; replay what is already due so the snapshot is consistent.
    this.advanceOne(request, plan, Date.now());
  }

  private advance(now: number) {
    for (const { plan, request } of [...this.inFlight.values()]) this.advanceOne(request, plan, now);
    const count = this.inFlight.size;
    if (count < MIN_IN_FLIGHT || (count < MAX_IN_FLIGHT && rng.chance(0.3))) this.start(now);
  }

  private advanceOne(req: LiveRequest, plan: Plan, now: number) {
    const elapsed = now - plan.startedMs;
    const at = (offset: number) => iso(plan.startedMs + offset);
    if (req.attempts.length === 0 && elapsed >= plan.selectAt) {
      const attempt = this.attemptFor(req, 0, plan.startedMs + plan.selectAt, []);
      req.attempts.push(attempt);
      req.accountId = attempt.accountId;
      req.phases.selectedAt = at(plan.selectAt);
      this.emit({ accountId: attempt.accountId, at: at(plan.selectAt), attempt: { ...attempt }, cursor: this.next(), id: req.id, type: 'selected' });
    }
    if (req.attempts.length > 0 && !req.phases.writtenAt && elapsed >= plan.writtenAt) req.phases.writtenAt = at(plan.writtenAt);
    if (plan.attemptsPlanned > 1 && req.attempts.length === 1 && elapsed >= plan.failoverAt) {
      const first = req.attempts[0];
      first.outcome = 'rate_limited';
      first.upstreamStatus = 429;
      first.cooldownSet = { reason: '429 on 5h window', until: iso(plan.startedMs + rng.int(20, 75) * MINUTE) };
      first.windows = first.windows.map((w) => (w.id === '5h' ? { ...w, utilization: 1 } : w));
      const second = this.attemptFor(req, 1, plan.startedMs + plan.failoverAt, [first.accountId]);
      req.attempts.push(second);
      req.accountId = second.accountId;
      this.emit({ accountId: second.accountId, at: at(plan.failoverAt), attempt: { ...second }, cursor: this.next(), failed: { ...first }, id: req.id, type: 'selected' });
    }
    if (req.attempts.length === plan.attemptsPlanned && req.status === null && elapsed >= plan.headAt) {
      const head = req.attempts[req.attempts.length - 1];
      head.upstreamStatus = plan.finalStatus;
      head.outcome = plan.finalStatus === 200 ? 'succeeded' : plan.finalStatus === 429 ? 'rate_limited' : 'upstream_error';
      req.status = plan.finalStatus;
      req.phases.headAt = at(plan.headAt);
      // The head frame carries the input side of usage; output accrues from here.
      req.usage = { ...plan.finalUsage, output: 0 };
      this.emit({ at: at(plan.headAt), attempt: { ...head }, cursor: this.next(), id: req.id, status: plan.finalStatus, type: 'head' });
    }
    const streaming = req.status === 200 && req.kind === 'inference';
    if (streaming && !req.phases.firstContentAt && elapsed >= plan.firstContentAt) req.phases.firstContentAt = at(plan.firstContentAt);
    if (streaming && req.phases.firstContentAt && elapsed < plan.streamEndAt && now - plan.lastProgressAt >= 500) {
      const frac = Math.min(1, (elapsed - plan.firstContentAt) / Math.max(1, plan.streamEndAt - plan.firstContentAt));
      const output = Math.round(plan.finalUsage.output * frac);
      const tps = (plan.finalUsage.output / Math.max(50, plan.streamEndAt - plan.firstContentAt)) * 1000 * rng.float(0.85, 1.15);
      req.usage = { ...req.usage, output };
      req.outputTps = tps;
      plan.lastProgressAt = now;
      this.emit({ at: iso(now), cursor: this.next(), id: req.id, outputTps: tps, type: 'progress', usage: { ...req.usage } });
    }
    if (streaming && req.keyId === 'laptop' && req.phases.firstContentAt && elapsed >= plan.nextPreviewAt && plan.previewIndex < PREVIEW_WORDS.length && elapsed < plan.streamEndAt) {
      const n = rng.int(3, 8);
      const chunk = `${plan.previewIndex === 0 ? '' : ' '}${PREVIEW_WORDS.slice(plan.previewIndex, plan.previewIndex + n).join(' ')}`;
      plan.previewIndex += n;
      plan.nextPreviewAt = elapsed + rng.int(350, 900);
      req.preview = (req.preview ?? '') + chunk;
      this.emit({ chunk, cursor: this.next(), id: req.id, type: 'preview' });
    }
    if (streaming && !req.phases.streamEndAt && elapsed >= plan.streamEndAt) req.phases.streamEndAt = at(plan.streamEndAt);
    if (req.status !== null && elapsed >= plan.durationMs) this.finish(req, plan, plan.startedMs + plan.durationMs);
  }

  private finish(req: LiveRequest, plan: Plan, finishedMs: number) {
    this.inFlight.delete(req.id);
    req.usage = plan.finalUsage;
    req.costUsd = Math.round(costOf(req.model, req.usage) * 10_000) / 10_000;
    req.errorCode = plan.finalStatus === 200 ? null : plan.finalStatus === 429 ? 'rate_limit_error' : plan.finalStatus === 529 ? 'overloaded_error' : 'api_error';
    req.phases.finishedAt = iso(finishedMs);
    if (req.status === 200 && req.kind === 'inference') {
      req.outputTps = (plan.finalUsage.output / Math.max(50, plan.streamEndAt - plan.firstContentAt)) * 1000;
      if (!req.phases.streamEndAt) req.phases.streamEndAt = iso(plan.startedMs + plan.streamEndAt);
    }
    req.state = 'finished';
    this.recent.unshift(req);
    const cutoff = finishedMs - RING_SPAN;
    this.recent = this.recent.filter((r) => Date.parse(r.phases.receivedAt) >= cutoff).slice(0, RING_CAP);
    this.emit({ at: req.phases.finishedAt, costUsd: req.costUsd, cursor: this.next(), errorCode: req.errorCode, id: req.id, phases: { ...req.phases }, status: req.status ?? plan.finalStatus, type: 'finished', usage: { ...req.usage } });
  }
}

let feed: LiveFeed | null = null;

/** One simulator per page load, shared by every subscriber so the ring and cursor agree. */
export function liveFeed(): LiveFeed {
  if (!feed) feed = new LiveFeed();
  return feed;
}
