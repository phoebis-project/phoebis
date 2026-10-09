import type { AttemptWindow, LiveAttempt, LiveDelta, LiveMetric, LivePhases, LiveRequest, LiveTick, RequestKind, RequestLog, Transport, Usage } from '@/api/types';
import { histogramOf, percentileFromHistogram } from '@/lib/histogram';
import type { ByWindow } from '@/lib/windows';
import { ACCOUNTS } from './accounts';
import { createRng } from './rng';
import { LANES, REQUESTS, SESSION_IDS, USER_AGENTS, costOf, warmsCaches, type Lane } from './requests';
import { LIVE_REQUEST_SAMPLE } from './samples';
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
  'The retry path also swallowed the exit code. It now propagates it so CI fails loudly instead of passing on a partial write. ' +
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
  // A head reports the two shared windows and the overage week; a week only the poll reads never reaches an attempt.
  return account.windows.filter((w) => w.id === '5h' || w.id === '7d').map((w) => ({ id: w.id, resetsAt: w.resetsAt, utilization: w.utilization }));
};

// The live feed draws no thinking share and no server-tool calls: neither reaches the console before the
// request is over, so a stream in flight reports them as nothing spent yet.
const emptyUsage = (): Usage => ({ cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: 0, input: 0, output: 0, thinking: 0, webFetch: 0, webSearch: 0 });

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
    ...emptyUsage(),
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
  const ttftMs = ok && r.kind === 'inference' ? r.ttftMs : null;
  // Every key stays present, undefined where it does not apply, so a still-running request's phases (only
  // receivedAt set) and a finished one's share the same field set for a caller comparing shapes across the ring.
  const phases: LivePhases = {
    finishedAt: iso(at + durationMs),
    firstContentAt: ttftMs !== null ? iso(at + ttftMs) : undefined,
    headAt: r.ttfbMs !== null ? iso(at + r.ttfbMs) : undefined,
    receivedAt: r.at,
    selectedAt: iso(at + 60),
    streamEndAt: ttftMs !== null ? iso(at + Math.max(ttftMs + 10, durationMs - 40)) : undefined,
    writtenAt: iso(at + 140),
  };
  const streaming = r.ttftMs !== null ? Math.max(50, durationMs - r.ttftMs) : null;
  return {
    ...LIVE_REQUEST_SAMPLE,
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
    preview: null,
    sessionId: r.sessionId,
    state: 'finished',
    status: r.status,
    transport: r.transport,
    usage: r.usage,
    userAgent: r.userAgent,
  };
}

const metric = (value: number | null, series: number[], quality: LiveMetric['quality']): LiveMetric => ({ quality, series, value });

type Wire = Record<string, unknown>;
/** A frame as the router's socket sends it: snake_case names, Unix milliseconds, attempts counted from one. */
export type LiveFrame = { delta: Wire; kind: 'delta' } | { kind: 'snapshot'; snapshot: Wire };

const ms = (at: string) => Date.parse(at);
// The header sampler's name for the third window, which the decoder folds onto the UI's own; any other key travels
// as it is.
const WIRE_WINDOW: ByWindow<string> = { '5h': '5h', '7d': '7d', '7d_fable': '7d_oi' };
const wireAttempt = (a: LiveAttempt): Wire => ({
  account_id: a.accountId,
  cooldown_set: a.cooldownSet && { reason: a.cooldownSet.reason, until: ms(a.cooldownSet.until) },
  index: a.index + 1,
  outcome: a.outcome,
  started_at: ms(a.startedAt),
  upstream_status: a.upstreamStatus,
  windows: a.windows.map((w) => ({ id: WIRE_WINDOW[w.id] ?? w.id, resets_at: w.resetsAt === null ? null : ms(w.resetsAt), utilization: w.utilization })),
});
/** The router leaves a phase out until it is observed. */
function wirePhases(p: LivePhases): Wire {
  const at = { finished_at: p.finishedAt, first_content_at: p.firstContentAt, head_at: p.headAt, received_at: p.receivedAt, selected_at: p.selectedAt, stream_end_at: p.streamEndAt, written_at: p.writtenAt };
  return Object.fromEntries(Object.entries(at).flatMap(([k, v]) => (v === undefined ? [] : [[k, ms(v)]])));
}
// The router's live usage carries five token counts; the thinking share and the server tools reach the console
// with the ledger row alone.
const wireUsage = (u: Usage | null): Wire => ({ cache_read: u?.cacheRead ?? 0, cache_write_1h: u?.cacheWrite1h ?? 0, cache_write_5m: u?.cacheWrite5m ?? 0, input: u?.input ?? 0, output: u?.output ?? 0 });
const wireRequest = (r: LiveRequest): Wire => ({
  account_id: r.accountId,
  agent_id: r.agentId,
  attempts: r.attempts.map(wireAttempt),
  // Nothing in the console reads the byte count, so the simulator keeps none.
  bytes_relayed: 0,
  canonical_model: r.model,
  cost_usd: r.costUsd,
  error_code: r.errorCode,
  id: r.id,
  key_id: r.keyId,
  kind: r.kind,
  model: r.model,
  output_tps: r.outputTps,
  phases: wirePhases(r.phases),
  pool_id: r.poolId,
  preview: r.preview,
  session_id: r.sessionId,
  state: r.state,
  status: r.status,
  transport: r.transport,
  usage: wireUsage(r.usage),
  user_agent: r.userAgent,
});
const wireMetric = (m: LiveMetric): Wire => ({ quality: m.quality, series: m.series, value: m.value });
const wireTick = (t: LiveTick): Wire => {
  const m = t.metrics;
  return {
    at: ms(t.at),
    metrics: {
      active_streams: wireMetric(m.activeStreams),
      cost_per_minute: wireMetric(m.costPerMinute),
      error_rate: wireMetric(m.errorRate),
      output_tps: wireMetric(m.outputTps),
      requests_per_second: wireMetric(m.requestsPerSecond),
      ttfb_p50: wireMetric(m.ttfbP50),
      ttfb_p95: wireMetric(m.ttfbP95),
      ttfb_p99: wireMetric(m.ttfbP99),
    },
  };
};
function wireDelta(d: LiveDelta): Wire {
  switch (d.type) {
    case 'started':
      return { cursor: d.cursor, request: wireRequest(d.request), type: d.type };
    case 'selected':
      return { account_id: d.accountId, at: ms(d.at), attempt: wireAttempt(d.attempt), cursor: d.cursor, ...(d.failed ? { failed: wireAttempt(d.failed) } : {}), id: d.id, type: d.type };
    case 'head':
      return { at: ms(d.at), attempt: wireAttempt(d.attempt), cursor: d.cursor, id: d.id, status: d.status, type: d.type };
    case 'progress':
      return { at: ms(d.at), cursor: d.cursor, id: d.id, output_tps: d.outputTps, type: d.type, usage: wireUsage(d.usage) };
    case 'preview':
      return { chunk: d.chunk, cursor: d.cursor, id: d.id, type: d.type };
    case 'finished':
      return { at: ms(d.at), attempts: d.attempts.map(wireAttempt), cost_usd: d.costUsd, cursor: d.cursor, error_code: d.errorCode, id: d.id, phases: wirePhases(d.phases), status: d.status, type: d.type, usage: wireUsage(d.usage) };
    case 'tick':
      return { cursor: d.cursor, tick: wireTick(d.tick), type: d.type };
  }
}

/** A row the feed itself runs always has a usage; only a page's projection of a past instant has none. */
type FeedRequest = LiveRequest & { usage: Usage };

class LiveFeed {
  private cursor = 0;
  private handlers = new Set<(frame: LiveFrame) => void>();
  private inFlight = new Map<string, { plan: Plan; request: FeedRequest }>();
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
    this.seedStream(now);
    // Seeds may replay to completion at once (short count_tokens calls), so top up until the floor holds.
    for (let i = 0; i < 20 && this.inFlight.size < MIN_IN_FLIGHT + 1; i += 1) this.start(now - rng.int(400, 9_000));
  }

  /**
   * A laptop stream that opened before the feed and runs for another minute. A finished request carries no
   * preview, so this is the one an admin opens to read output while it arrives, whatever the drawn plans hold.
   */
  private seedStream(now: number) {
    const lane = LANES.find((l) => l.keyId === 'laptop') ?? LANES[0];
    const model = 'claude-sonnet-5';
    const plan: Plan = {
      attemptsPlanned: 1,
      durationMs: 80_000,
      failoverAt: 0,
      finalStatus: 200,
      finalUsage: finalUsageFor(model, 'inference', 200),
      firstContentAt: 1_600,
      headAt: 1_100,
      kind: 'inference',
      lastProgressAt: 0,
      model,
      nextPreviewAt: 1_600,
      previewIndex: 0,
      selectAt: 60,
      startedMs: now - 12_000,
      streamEndAt: 79_000,
      writtenAt: 140,
    };
    const request: FeedRequest = { ...this.requestFor(lane, plan), transport: 'sse' };
    this.inFlight.set(request.id, { plan, request });
    this.advanceOne(request, plan, now);
  }

  subscribe(handler: (frame: LiveFrame) => void): () => void {
    this.handlers.add(handler);
    handler({ kind: 'snapshot', snapshot: this.snapshot() });
    if (this.handlers.size === 1) this.run();
    return () => {
      this.handlers.delete(handler);
      if (this.handlers.size === 0) this.stop();
    };
  }

  private snapshot(): Wire {
    return {
      cursor: this.cursor,
      in_flight: [...this.inFlight.values()].map((e) => wireRequest(e.request)),
      recent: this.recent.map(wireRequest),
      tick: wireTick(this.tick(Date.now())),
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
    const wire = wireDelta(delta);
    for (const h of this.handlers) h({ delta: wire, kind: 'delta' });
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

  private requestFor(lane: Lane, plan: Plan): FeedRequest {
    const transport: Transport = plan.kind === 'inference' && rng.chance(0.86) ? 'sse' : 'json';
    return {
      ...LIVE_REQUEST_SAMPLE,
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
      // Every key stays present, undefined until advanceOne fills it in, so a snapshot taken mid-flight shows
      // the same field set a finished request does.
      phases: { finishedAt: undefined, firstContentAt: undefined, headAt: undefined, receivedAt: iso(plan.startedMs), selectedAt: undefined, streamEndAt: undefined, writtenAt: undefined },
      poolId: lane.poolId,
      preview: null,
      sessionId: rng.pick(LIVE_SESSION_IDS),
      state: 'in_flight',
      status: null,
      transport,
      usage: emptyUsage(),
      userAgent: USER_AGENTS[lane.keyId] ?? null,
    };
  }

  private attemptFor(req: LiveRequest, index: number, at: number, exclude: string[]): LiveAttempt {
    const lane = LANES.find((l) => l.keyId === req.keyId) ?? LANES[0];
    const candidates = lane.accountIds.filter((a) => !exclude.includes(a));
    const accountId = rng.pick(candidates.length ? candidates : lane.accountIds);
    return { accountId, cooldownSet: null, index, outcome: 'in_flight', startedAt: iso(at), upstreamStatus: null, windows: windowsFor(accountId) };
  }

  /** Runs every phase of a request straight through; used to seed the ring with finished rows. */
  private settle(req: FeedRequest, plan: Plan, finishAt: number): FeedRequest {
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
    const streamed = plan.finalStatus === 200 && req.kind === 'inference';
    // Every key stays present, undefined where it does not apply, matching a still-running request's phases.
    req.phases = {
      finishedAt: iso(finishAt),
      firstContentAt: streamed ? iso(s + plan.firstContentAt) : undefined,
      headAt: iso(s + plan.headAt),
      receivedAt: iso(s),
      selectedAt: iso(s + plan.selectAt),
      streamEndAt: streamed ? iso(s + plan.streamEndAt) : undefined,
      writtenAt: iso(s + plan.writtenAt),
    };
    if (streamed) req.outputTps = (plan.finalUsage.output / Math.max(50, plan.streamEndAt - plan.firstContentAt)) * 1000;
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

  private advanceOne(req: FeedRequest, plan: Plan, now: number) {
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
    // The router's feed drops a request's preview when it finishes, so a snapshot's ring never carries one.
    req.preview = null;
    req.state = 'finished';
    this.recent.unshift(req);
    const cutoff = finishedMs - RING_SPAN;
    this.recent = this.recent.filter((r) => Date.parse(r.phases.receivedAt) >= cutoff).slice(0, RING_CAP);
    this.emit({ at: req.phases.finishedAt, attempts: req.attempts.map((a) => ({ ...a })), costUsd: req.costUsd, cursor: this.next(), errorCode: req.errorCode, id: req.id, phases: { ...req.phases }, status: req.status ?? plan.finalStatus, type: 'finished', usage: { ...req.usage } });
  }
}

let feed: LiveFeed | null = null;

/** One simulator per page load, shared by every subscriber so the ring and cursor agree. */
export function liveFeed(): LiveFeed {
  if (!feed) feed = new LiveFeed();
  return feed;
}
