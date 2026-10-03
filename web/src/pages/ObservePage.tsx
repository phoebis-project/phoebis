import { memo, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useSearchParams } from 'react-router-dom';
import { api } from '@/api';
import type { LiveDelta, LiveEvent, LiveMetric, LivePhases, LiveRequest, LiveSnapshot, LiveTick, MetricQuality } from '@/api/types';
import { useAppSettings } from '@/app/settings';
import { Sparkline } from '@/components/charts/Sparkline';
import { useChartInk } from '@/components/charts/useChartInk';
import { AttemptTrail } from '@/components/ui/AttemptTrail';
import { MultiSelect } from '@/components/ui/MultiSelect';
import { IconExpand, IconPause, IconPlay, IconWarning, IconX } from '@/components/ui/icons';
import { Badge, Button, Card, EmptyState, KeyValue, Meter, Segmented, Select, Skeleton, Toggle } from '@/components/ui/primitives';
import { clock } from '@/lib/clock';
import { coalesce } from '@/lib/frame';
import { fmtAbsolute, fmtCompact, fmtDuration, fmtInt, fmtMs, fmtPct, fmtShortTime, fmtTime, fmtUsd, shortAgent, shortId, tokensTotal, undatedModel } from '@/lib/format';
import { PERCENTILE_HINT } from '@/lib/histogram';
import { useNow } from '@/lib/hooks';
import { projectAt } from '@/lib/projection';

const RING_SPAN_S = 30 * 60;
const MINUTE_MS = 60_000;
type Dim = 'accountId' | 'keyId' | 'model' | 'poolId' | 'sessionId';
const DIMS: { dim: Dim; label: string }[] = [
  { dim: 'keyId', label: 'Key' },
  { dim: 'poolId', label: 'Pool' },
  { dim: 'accountId', label: 'Account' },
  { dim: 'model', label: 'Model' },
  { dim: 'sessionId', label: 'Session' },
];
type StatusFilter = '2xx' | '429' | '529' | '5xx' | 'all';

interface Store {
  cursor: number;
  inFlight: Map<string, LiveRequest>;
  recent: LiveRequest[];
  tick: LiveTick | null;
}

const emptyStore = (): Store => ({ cursor: 0, inFlight: new Map(), recent: [], tick: null });

function fromSnapshot(s: LiveSnapshot): Store {
  return { cursor: s.cursor, inFlight: new Map(s.inFlight.map((r) => [r.id, r])), recent: s.recent, tick: s.tick };
}

/** Each delta produces a new object for the touched request only, so memoized rows elsewhere keep their identity. */
function applyDelta(store: Store, d: LiveDelta): Store {
  const next: Store = { ...store, cursor: d.cursor };
  if (d.type === 'tick') return { ...next, tick: d.tick };
  if (d.type === 'started') {
    next.inFlight = new Map(store.inFlight).set(d.request.id, d.request);
    return next;
  }
  const cur = store.inFlight.get(d.id);
  if (!cur) return next;
  let updated: LiveRequest = cur;
  switch (d.type) {
    case 'selected': {
      const known = cur.attempts.some((a) => a.index === d.attempt.index);
      const attempts = cur.attempts.map((a) => (a.index === d.attempt.index ? d.attempt : d.failed && a.index === d.failed.index ? d.failed : a));
      updated = { ...cur, accountId: d.accountId, attempts: known ? attempts : [...attempts, d.attempt], phases: { ...cur.phases, selectedAt: cur.phases.selectedAt ?? d.at } };
      break;
    }
    case 'head':
      updated = { ...cur, status: d.status, attempts: cur.attempts.map((a) => (a.index === d.attempt.index ? d.attempt : a)), phases: { ...cur.phases, writtenAt: cur.phases.writtenAt ?? d.at, headAt: d.at } };
      break;
    case 'progress':
      updated = { ...cur, usage: d.usage, outputTps: d.outputTps, phases: { ...cur.phases, firstContentAt: cur.phases.firstContentAt ?? d.at } };
      break;
    case 'preview':
      updated = { ...cur, preview: (cur.preview ?? '') + d.chunk };
      break;
    case 'finished': {
      // The attempts come with the delta: a connection that failed before a head, and a stream that failed
      // after one, both settle an outcome here, and the row must not keep the one it was drawn with. The feed
      // drops a finished request's preview, so the row does too rather than show output a reconnect would lose.
      updated = { ...cur, state: 'finished', attempts: d.attempts, status: d.status, costUsd: d.costUsd, errorCode: d.errorCode, phases: d.phases, preview: null, usage: d.usage };
      const inFlight = new Map(store.inFlight);
      inFlight.delete(d.id);
      const cutoff = Date.parse(d.at) - RING_SPAN_S * 1000;
      return { ...next, inFlight, recent: [updated, ...store.recent].filter((r) => Date.parse(r.phases.receivedAt) >= cutoff).slice(0, 2_048) };
    }
  }
  next.inFlight = new Map(store.inFlight).set(d.id, updated);
  return next;
}

export function ObservePage() {
  const storeRef = useRef<Store>(emptyStore());
  const pausedRef = useRef(false);
  const lastTickAt = useRef(0);
  const [view, setView] = useState<Store>(emptyStore);
  const [paused, setPaused] = useState<number | null>(null);
  const [mode, setMode] = useState<'streams' | 'table'>('table');
  const [fullscreen, setFullscreen] = useState(false);
  const [span, setSpan] = useState<Span>({ endS: RING_SPAN_S, startS: 0 });
  // The window stops sliding once the end handle leaves "now", so a chosen span keeps its absolute times.
  const [anchor, setAnchor] = useState<number | null>(null);
  const [searchParams, setSearchParams] = useSearchParams();
  const sessionParam = searchParams.get('session');
  const [filters, setFilters] = useState<Record<Dim, string[]>>(() => ({ accountId: [], keyId: [], model: [], poolId: [], sessionId: sessionParam ? [sessionParam] : [] }));
  const [status, setStatus] = useState<StatusFilter>('all');
  const [inFlightOnly, setInFlightOnly] = useState(false);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const openerRef = useRef<HTMLElement | null>(null);
  const { canMutate } = useAppSettings();
  // The feed can send many frames inside one animation frame; the store takes each as it lands and the view the last.
  const pushView = useMemo(() => coalesce<Store>(setView), []);

  // Arriving from a session row carries the session in the query; dropping the
  // chip clears the parameter too so this does not put the filter straight back.
  useEffect(() => {
    if (sessionParam === null) return;
    setFilters((f) => (f.sessionId.includes(sessionParam) ? f : { ...f, sessionId: [sessionParam] }));
  }, [sessionParam]);
  const dropSession = (id: string) => {
    setFilters((f) => ({ ...f, sessionId: f.sessionId.filter((v) => v !== id) }));
    if (sessionParam !== id) return;
    const next = new URLSearchParams(searchParams);
    next.delete('session');
    setSearchParams(next, { replace: true });
  };

  useEffect(() => {
    const off = api.subscribeLive((e: LiveEvent) => {
      storeRef.current = e.kind === 'snapshot' ? fromSnapshot(e.snapshot) : applyDelta(storeRef.current, e.delta);
      if (e.kind === 'snapshot' || e.delta.type === 'tick') lastTickAt.current = clock.now();
      if (!pausedRef.current) pushView.push(storeRef.current);
    });
    return () => {
      off();
      pushView.cancel();
    };
  }, [pushView]);

  const togglePause = () => {
    const next = paused === null;
    pausedRef.current = next;
    setPaused(next ? clock.now() : null);
    // A frame an event queued just before the click would otherwise draw that event after it, so the view the pause
    // freezes is the last one drawn.
    if (next) pushView.cancel();
    else {
      pushView.push(storeRef.current);
      pushView.flush();
    }
  };

  const onSelect = useCallback((id: string, opener: HTMLElement | null) => {
    openerRef.current = opener;
    setSelectedId(id);
  }, []);
  const closeDetail = () => {
    setSelectedId(null);
    if (openerRef.current?.isConnected) openerRef.current.focus();
  };

  // The feed ticks once a second, so a second-resolution clock is what notices it stop.
  const wallClock = useNow(1_000);
  // Against the arrival stamp, never the tick's own `at`: that carries the router's clock, and the banner has
  // to mean the feed went quiet rather than that the two clocks disagree. A pause freezes the view on purpose.
  const connected = view.tick === null ? false : paused !== null || wallClock - lastTickAt.current < 5_000;
  const nowMs = view.tick ? Date.parse(view.tick.at) : clock.now();
  const historical = span.endS < RING_SPAN_S;
  const windowStart = (historical ? anchor ?? nowMs : nowMs) - RING_SPAN_S * 1000;
  const fromMs = windowStart + span.startS * 1000;
  const atMs = windowStart + span.endS * 1000;
  // Elapsed cells stop with the table: at the end of the span, or at the moment of pausing.
  const frozenAt = historical ? atMs : paused;

  const setRange = (next: Span) => {
    setAnchor(next.endS < RING_SPAN_S ? anchor ?? nowMs : null);
    setSpan(next);
  };
  const goLive = () => {
    setAnchor(null);
    setSpan((s) => ({ ...s, endS: RING_SPAN_S }));
  };

  // Parsed once per view: the density bins, the window's filter and the newest-first sort all read each row's arrival.
  const receivedMs = useMemo(() => new Map([...view.inFlight.values(), ...view.recent].map((r) => [r.id, Date.parse(r.phases.receivedAt)])), [view]);

  const sessionFilter = filters.sessionId;
  // Binned by wall-clock minute rather than by offset from the window start, so
  // a bar names the same minute as the axis tick under it and the two slide
  // left together while the feed is live. One bin over the ring's 30 covers the
  // partial minute the moving left edge sits in.
  const density = useMemo<Density>(() => {
    const firstMinute = Math.floor(windowStart / MINUTE_MS) * MINUTE_MS;
    const counts = new Array(RING_SPAN_S / 60 + 1).fill(0) as number[];
    for (const r of [...view.recent, ...view.inFlight.values()]) {
      if (sessionFilter.length && (r.sessionId === null || !sessionFilter.includes(r.sessionId))) continue;
      const bin = Math.floor((receivedMs.get(r.id)! - firstMinute) / MINUTE_MS);
      if (bin >= 0 && bin < counts.length) counts[bin] += 1;
    }
    return { counts, firstMinute };
  }, [view, receivedMs, windowStart, sessionFilter]);

  const all = useMemo(() => {
    const at = (r: LiveRequest) => receivedMs.get(r.id)!;
    if (historical) return view.recent.filter((r) => at(r) <= atMs && at(r) >= fromMs).map((r) => projectAt(r, atMs));
    return [...view.inFlight.values(), ...view.recent].filter((r) => at(r) >= fromMs).sort((a, b) => at(b) - at(a));
  }, [view, receivedMs, historical, atMs, fromMs]);

  const options = useMemo(() => {
    const distinct = (dim: Dim) => [...new Set(all.map((r) => r[dim]).filter((v): v is string => v !== null))].sort();
    return { accountId: distinct('accountId'), keyId: distinct('keyId'), model: distinct('model'), poolId: distinct('poolId'), sessionId: distinct('sessionId') };
  }, [all]);

  const rows = useMemo(
    () =>
      all.filter((r) => {
        if (inFlightOnly && r.state !== 'in_flight') return false;
        for (const { dim } of DIMS) if (filters[dim].length && !filters[dim].includes(r[dim] ?? '')) return false;
        if (status !== 'all') {
          const s = r.status;
          if (s === null) return false;
          if (status === '2xx' && !(s >= 200 && s < 300)) return false;
          if (status === '429' && s !== 429) return false;
          if (status === '529' && s !== 529) return false;
          if (status === '5xx' && !(s >= 500 && s !== 529)) return false;
        }
        return true;
      }),
    [all, filters, status, inFlightOnly],
  );

  const selected = useMemo(() => (selectedId ? all.find((r) => r.id === selectedId) ?? null : null), [all, selectedId]);
  const streams = useMemo(() => rows.filter((r) => r.state === 'in_flight'), [rows]);

  return (
    <div className={`observe ${selected ? 'has-detail' : ''}`}>
      <div className="observe__main">
        <MetricStrip tick={view.tick} />

        <div className="observe__toolbar">
          {/* A chosen session already shows as a removable chip further along this
              bar, so its dropdown would be a second control over one filter. */}
          {DIMS.filter(({ dim }) => dim !== 'sessionId' || filters.sessionId.length === 0).map(({ dim, label }) => (
            <MultiSelect
              key={dim}
              label={label}
              options={options[dim].map((v) => ({ label: dim === 'sessionId' ? shortId(v, 13) : v, value: v }))}
              value={filters[dim]}
              onChange={(v) => setFilters({ ...filters, [dim]: v })}
              mono
            />
          ))}
          <Select value={status} onChange={(e) => setStatus(e.target.value as StatusFilter)} aria-label="Status">
            <option value="all">Any status</option>
            <option value="2xx">2xx</option>
            <option value="429">429</option>
            <option value="529">529</option>
            <option value="5xx">Other 5xx</option>
          </Select>
          <label className="row">
            <Toggle checked={inFlightOnly} onChange={setInFlightOnly} label="In-flight only" />
            <span className="secondary">In-flight only</span>
          </label>
          <Button size="sm" icon={paused !== null ? <IconPlay size={13} /> : <IconPause size={13} />} onClick={togglePause} aria-pressed={paused !== null} disabled={historical}>
            {paused !== null ? 'Resume' : 'Pause'}
          </Button>
          {filters.sessionId.length > 0 && (
            <span className="chips">
              {filters.sessionId.map((id) => (
                <span key={id} className="chip chip--removable mono" title={id}>
                  Session {shortId(id, 13)}
                  <button type="button" aria-label={`Remove session filter ${id}`} onClick={() => dropSession(id)}>
                    <IconX size={12} />
                  </button>
                </span>
              ))}
            </span>
          )}
          <Segmented<'streams' | 'table'>
            ariaLabel="View"
            value={mode}
            onChange={setMode}
            options={[
              { value: 'table', label: 'Table' },
              { value: 'streams', label: 'Streams' },
            ]}
          />
        </div>

        <RangeStrip density={density} span={span} windowStart={windowStart} live={!historical} onChange={setRange} onLive={goLive} />
        {historical && (
          <div className="observe__banner" role="status">
            <IconWarning size={14} />
            <span>
              Historical: deltas not replayed. Showing the ring from {fmtAbsolute(new Date(fromMs).toISOString())} to {fmtAbsolute(new Date(atMs).toISOString())}.
            </span>
          </div>
        )}
        {paused !== null && !historical && (
          <div className="observe__banner" role="status">
            <IconPause size={14} />
            <span>Paused. Updates keep accumulating; Resume catches up.</span>
          </div>
        )}
        {!connected && view.tick !== null && (
          <div className="observe__banner" role="status">
            <IconWarning size={14} />
            <span>Reconnecting to the live feed…</span>
          </div>
        )}

        {mode === 'table' ? (
          <Card padded={false} className="card--table">
            {view.tick === null ? (
              <div style={{ padding: 20 }}>
                <Skeleton height={14} />
              </div>
            ) : rows.length === 0 ? (
              <div style={{ padding: 20 }}>
                <EmptyState title="Nothing matches" body="Clear a filter, or wait for the next request." />
              </div>
            ) : (
              <div className="table-wrap">
                <table className="table live-table">
                  <thead>
                    <tr>
                      <th>Time</th>
                      <th>Status</th>
                      <th>Key</th>
                      <th>Session</th>
                      <th>Agent</th>
                      <th>Model</th>
                      <th>Client</th>
                      <th>Account</th>
                      <th>Attempts</th>
                      <th className="right">Tokens so far</th>
                      <th className="right">tok/s</th>
                      <th className="right">Cost</th>
                      <th className="right">Elapsed</th>
                    </tr>
                  </thead>
                  <tbody>
                    {rows.map((r) => (
                      <LiveRow key={r.id} r={r} selected={r.id === selectedId} onSelect={onSelect} frozenAt={frozenAt} />
                    ))}
                  </tbody>
                </table>
              </div>
            )}
          </Card>
        ) : (
          <StreamsGrid streams={streams} fullscreen={fullscreen} onFullscreen={setFullscreen} canPreview={canMutate} frozenAt={frozenAt} />
        )}
      </div>

      {selected && <DetailPane r={selected} onClose={closeDetail} canPreview={canMutate} frozenAt={frozenAt} />}
    </div>
  );
}

/* Time range */

/** Seconds from the left edge of the 30-minute window; `endS` at the maximum means "now". */
interface Span {
  endS: number;
  startS: number;
}

/** Requests per wall-clock minute; `firstMinute` is the minute the leftmost bar covers. */
interface Density {
  counts: number[];
  firstMinute: number;
}

const MIN_SPAN_S = 60;
/** Leaves the top of the strip free for the now label, which sits over the bars. */
const BAR_MAX_PCT = 88;
const isoAt = (ms: number) => new Date(ms).toISOString();

function fmtSpan(ms: number): string {
  const total = Math.max(0, Math.round(ms / 1000));
  const m = Math.floor(total / 60);
  const s = total % 60;
  if (m === 0) return `${s}s`;
  return s === 0 ? `${m}m` : `${m}m ${s}s`;
}

/**
 * The 30-minute ring as a density strip with a two-handle range over it, under
 * a clock ruler. The ruler carries absolute times, so while the feed is live
 * the bars and the ticks slide left out of a strip that stays put and a fresh
 * minute arrives on the right; anchoring the span freezes `windowStart` and the
 * whole picture with it.
 */
function RangeStrip({ density, span, windowStart, live, onChange, onLive }: { density: Density; live: boolean; onChange: (s: Span) => void; onLive: () => void; span: Span; windowStart: number }) {
  // A range input takes focus on pointerdown, so one piece of state covers both
  // dragging a grip and nudging it with the arrow keys.
  const [grip, setGrip] = useState<'end' | 'start' | null>(null);
  const peak = Math.max(1, ...density.counts);
  const spanMs = RING_SPAN_S * 1000;
  const pct = (s: number) => (s / RING_SPAN_S) * 100;
  const atPct = (ms: number) => ((ms - windowStart) / spanMs) * 100;
  const fromMs = windowStart + span.startS * 1000;
  const toMs = windowStart + span.endS * 1000;
  const edgeMs = windowStart + spanMs;
  // Keeps a bound label whole when its grip sits against an edge, which clips.
  const labelPct = (p: number) => Math.min(96, Math.max(4, p));

  const ticks: { at: number; major: boolean }[] = [];
  for (let at = Math.ceil(windowStart / MINUTE_MS) * MINUTE_MS; at <= edgeMs; at += MINUTE_MS) {
    ticks.push({ at, major: new Date(at).getMinutes() % 5 === 0 });
  }

  return (
    <div className="range">
      <div className="range__strip">
        <div className="range__span" style={{ left: `${pct(span.startS)}%`, width: `${pct(span.endS - span.startS)}%` }} aria-hidden />
        <div className="range__bars" role="img" aria-label="Requests per minute over the last 30 minutes">
          {density.counts.map((n, i) => {
            const at = density.firstMinute + i * MINUTE_MS;
            const inSpan = at + MINUTE_MS > fromMs && at < toMs;
            return (
              <span
                key={at}
                className={`range__bar ${inSpan ? 'is-in' : ''}`}
                style={{ height: `${Math.max(3, (n / peak) * BAR_MAX_PCT)}%`, left: `${atPct(at)}%`, width: `${(MINUTE_MS / spanMs) * 100}%` }}
                title={`${fmtShortTime(isoAt(at))}: ${fmtInt(n)} requests`}
              />
            );
          })}
        </div>
        <div className={`range__now ${live ? '' : 'is-anchored'}`} aria-hidden>
          <span className="range__now-label num">{live ? `now ${fmtShortTime(isoAt(edgeMs))}` : `paused at ${fmtShortTime(isoAt(edgeMs))}`}</span>
        </div>
        <input
          type="range"
          className="range__handle"
          min={0}
          max={RING_SPAN_S}
          step={30}
          value={span.startS}
          onChange={(e) => onChange({ endS: span.endS, startS: Math.min(Number(e.target.value), span.endS - MIN_SPAN_S) })}
          onPointerDown={() => setGrip('start')}
          onFocus={() => setGrip('start')}
          onBlur={() => setGrip(null)}
          aria-label="Range start"
          aria-valuetext={fmtTime(isoAt(fromMs))}
        />
        <input
          type="range"
          className="range__handle"
          min={0}
          max={RING_SPAN_S}
          step={30}
          value={span.endS}
          onChange={(e) => onChange({ endS: Math.max(Number(e.target.value), span.startS + MIN_SPAN_S), startS: span.startS })}
          onPointerDown={() => setGrip('end')}
          onFocus={() => setGrip('end')}
          onBlur={() => setGrip(null)}
          aria-label="Range end"
          aria-valuetext={live ? `now, ${fmtTime(isoAt(toMs))}` : fmtTime(isoAt(toMs))}
        />
        {grip !== null && (
          <>
            <span className="range__bound num" style={{ left: `${labelPct(pct(span.startS))}%` }} aria-hidden>
              {fmtShortTime(isoAt(fromMs))}
            </span>
            <span className="range__bound num" style={{ left: `${labelPct(pct(span.endS))}%` }} aria-hidden>
              {fmtShortTime(isoAt(toMs))}
            </span>
          </>
        )}
      </div>
      <div className="range__axis" aria-hidden>
        {ticks.map((t) => (
          <span key={t.at} className={`range__tick ${t.major ? 'is-major' : ''}`} style={{ left: `${atPct(t.at)}%` }}>
            {t.major && <span className="range__tick-label num">{fmtShortTime(isoAt(t.at))}</span>}
          </span>
        ))}
      </div>
      <div className="range__foot">
        <span className="num">
          {fmtTime(isoAt(fromMs))} to {fmtTime(isoAt(toMs))}, {fmtSpan(toMs - fromMs)}
        </span>
        <Button size="sm" variant="ghost" onClick={onLive} disabled={live}>
          Live
        </Button>
      </div>
    </div>
  );
}

/* Metric strip */

const METRICS: { format: (v: number | null) => string; key: keyof LiveTick['metrics']; label: string; title?: string }[] = [
  { key: 'requestsPerSecond', label: 'Requests per second', format: (v) => (v === null ? '-' : v.toFixed(2)) },
  { key: 'activeStreams', label: 'Active streams', format: (v) => (v === null ? '-' : fmtInt(v)) },
  { key: 'errorRate', label: 'Error rate', format: (v) => (v === null ? '-' : fmtPct(v, 1)) },
  { key: 'ttfbP50', label: 'TTFB p50', format: fmtMs, title: PERCENTILE_HINT },
  { key: 'ttfbP95', label: 'TTFB p95', format: fmtMs, title: PERCENTILE_HINT },
  { key: 'ttfbP99', label: 'TTFB p99', format: fmtMs, title: PERCENTILE_HINT },
  { key: 'outputTps', label: 'Output tokens per second', format: (v) => (v === null ? '-' : v.toFixed(0)) },
  { key: 'costPerMinute', label: 'Estimated cost per minute', format: (v) => (v === null ? '-' : fmtUsd(v)) },
];
const QUALITY_HINT: Record<MetricQuality, string> = {
  derived: 'Derived: computed from other measured values over the last minute.',
  estimated: 'Estimated: priced at the effective list from token counts, not billed.',
  measured: 'Measured: read directly from the router over the last minute.',
};

function MetricStrip({ tick }: { tick: LiveTick | null }) {
  const ink = useChartInk();
  // One flex item with its footnote, so the page's gap between sections does not open between the two.
  return (
    <div>
      <div className="metric-strip">
        {METRICS.map((m) => {
          const metric: LiveMetric | undefined = tick?.metrics[m.key];
          return (
            <div key={m.key} className="mchip" title={metric ? `${m.label}, last minute. ${QUALITY_HINT[metric.quality]}${m.title ? ` ${m.title}.` : ''}` : undefined}>
              <span className="mchip__label">{m.label}</span>
              <span className="mchip__value">
                {metric ? m.format(metric.value) : <Skeleton width={56} height={20} />}
                {metric && (
                  <span className="mchip__quality" title={QUALITY_HINT[metric.quality]}>
                    {metric.quality}
                  </span>
                )}
              </span>
              <div className="mchip__spark">{metric ? <Sparkline values={metric.series.map((v) => v ?? 0)} color={m.key === 'errorRate' ? ink.series[7] : ink.series[0]} height={26} /> : <Skeleton height={26} />}</div>
            </div>
          );
        })}
      </div>
      <p className="metric-strip__note muted">
        {QUALITY_HINT.measured} {QUALITY_HINT.derived} {QUALITY_HINT.estimated}
      </p>
    </div>
  );
}

/* Table */

// A stream can commit its 200 head and fail after it, so a finished 200 reads as a success only without an error.
const statusDot = (r: LiveRequest) => (r.state === 'in_flight' ? 'ldot--inflight' : r.status === 200 && r.errorCode === null ? 'ldot--ok' : r.status === 429 ? 'ldot--warn' : 'ldot--bad');
const statusTone = (r: LiveRequest) => (r.status === 200 && r.errorCode === null ? 'good' : r.status === 429 ? 'warning' : r.status === 529 ? 'serious' : 'critical');
// The Status badge is the readable signal; the dot only repeats it in colour, so its name is for a pointer's hover.
const DOT_TITLE = { 'ldot--bad': 'failed', 'ldot--inflight': 'in flight', 'ldot--ok': 'succeeded', 'ldot--warn': 'rate limited' };

const LiveRow = memo(function LiveRow({ r, selected, onSelect, frozenAt }: { frozenAt: number | null; onSelect: (id: string, opener: HTMLElement | null) => void; r: LiveRequest; selected: boolean }) {
  const model = undatedModel(r.model);
  return (
    <tr className={selected ? 'is-selected' : ''} onClick={() => onSelect(r.id, null)} aria-selected={selected} data-kind={r.kind}>
      <td className="num" title={fmtAbsolute(r.phases.receivedAt)}>
        <span className={`ldot ${statusDot(r)}`} aria-hidden title={DOT_TITLE[statusDot(r)]} />
        {/* The row's click serves the mouse; this button is the same action reachable by Tab, and the pane hands the
            focus back to it on close. */}
        <button
          type="button"
          className="live-row__open"
          onClick={(e) => {
            e.stopPropagation();
            onSelect(r.id, e.currentTarget);
          }}
        >
          {fmtTime(r.phases.receivedAt)}
        </button>
      </td>
      <td>{r.status === null ? <span className="muted">pending</span> : <Badge tone={statusTone(r)} title={r.errorCode ?? undefined}>{r.status}</Badge>}</td>
      <td className="mono">{r.keyId}</td>
      <td className="mono" title={r.sessionId ?? undefined}>
        {r.sessionId === null ? <span className="muted">none</span> : shortId(r.sessionId, 8)}
      </td>
      <td className="mono">{r.agentId}</td>
      <td className="mono nowrap" title={model !== r.model ? r.model : undefined}>
        {model}
      </td>
      <td className="mono live-table__client" title={r.userAgent ?? undefined}>
        {r.userAgent === null ? <span className="muted">none</span> : shortAgent(r.userAgent)}
      </td>
      <td>{r.accountId ?? <span className="muted">selecting</span>}</td>
      <td>
        <AttemptTrail attempts={r.attempts} />
      </td>
      <td className="right num">
        {r.usage === null ? (
          <span className="muted">-</span>
        ) : (
          <>
            {fmtCompact(tokensTotal(r.usage))}
            <span className="sub">{fmtCompact(r.usage.cacheRead)} cached</span>
          </>
        )}
      </td>
      <td className="right num">{r.outputTps === null ? <span className="muted">-</span> : r.outputTps.toFixed(0)}</td>
      <td className="right num">{r.state === 'finished' ? fmtUsd(r.costUsd) : <span className="muted">-</span>}</td>
      <td className="right num elapsed">
        <Elapsed from={r.phases.receivedAt} to={r.phases.finishedAt ?? null} frozenAt={frozenAt} />
      </td>
    </tr>
  );
});

/** A clock that only runs while `active`, so finished rows hold no timer. */
function useClock(active: boolean, intervalMs: number): number {
  const [now, setNow] = useState(() => clock.now());
  useEffect(() => {
    if (!active) return;
    setNow(clock.now());
    const id = clock.setInterval(() => setNow(clock.now()), intervalMs);
    return () => clock.clearInterval(id);
  }, [active, intervalMs]);
  return now;
}

/** Ticks on its own so an in-flight row re-renders only this cell. */
function Elapsed({ from, to, frozenAt }: { from: string; frozenAt: number | null; to: string | null }) {
  const running = to === null && frozenAt === null;
  const now = useClock(running, 250);
  const end = to ? Date.parse(to) : frozenAt ?? now;
  const ms = Math.max(0, end - Date.parse(from));
  return <>{ms < 60_000 ? `${(ms / 1000).toFixed(1)} s` : fmtDuration(ms)}</>;
}

/* Streams */

function StreamsGrid({ streams, fullscreen, onFullscreen, canPreview, frozenAt }: { canPreview: boolean; frozenAt: number | null; fullscreen: boolean; onFullscreen: (v: boolean) => void; streams: LiveRequest[] }) {
  const count = streams.length;
  const cols = count <= 1 ? 1 : count === 2 ? 2 : 3;
  useEffect(() => {
    if (!fullscreen) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onFullscreen(false);
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [fullscreen, onFullscreen]);
  if (count === 0) return <EmptyState title="No streams in flight" body="Tiles appear as requests start." />;
  return (
    <div className={`streams streams--${cols} ${fullscreen ? 'is-fullscreen' : ''}`}>
      <div className="streams__bar">
        <Button size="sm" variant="ghost" icon={fullscreen ? <IconX size={13} /> : <IconExpand size={13} />} onClick={() => onFullscreen(!fullscreen)} aria-pressed={fullscreen}>
          {fullscreen ? 'Exit fullscreen' : 'Fullscreen'}
        </Button>
      </div>
      {streams.map((r) => (
        <StreamTile key={r.id} r={r} canPreview={canPreview} frozenAt={frozenAt} />
      ))}
    </div>
  );
}

const StreamTile = memo(function StreamTile({ r, canPreview, frozenAt }: { canPreview: boolean; frozenAt: number | null; r: LiveRequest }) {
  const tps = r.outputTps ?? 0;
  return (
    <article className="stile" aria-label={`Stream ${shortId(r.id, 12)}`}>
      <div className="stile__head">
        <span>
          <span className={`ldot ${statusDot(r)}`} aria-hidden />
          <b className="mono">{r.model}</b>
        </span>
        <span className="num">
          <Elapsed from={r.phases.receivedAt} to={r.phases.finishedAt ?? null} frozenAt={frozenAt} />
        </span>
      </div>
      <div className="row" style={{ fontSize: 12.5 }}>
        <span>{r.accountId ?? <span className="muted">selecting</span>}</span>
        <span className="muted mono">{r.keyId}</span>
        <span className="muted">{r.transport === 'sse' ? 'SSE' : 'JSON'}</span>
      </div>
      <div className="stile__meter">
        <Meter value={Math.min(1, tps / 150)} tone="accent" thin label="Output tokens per second" />
        <span className="num">{r.outputTps === null ? '-' : `${tps.toFixed(0)} tok/s`}</span>
      </div>
      {canPreview && r.preview !== null && (
        <div className="preview">
          {r.preview}
          {r.state === 'in_flight' && <span className="preview__cursor" aria-hidden />}
        </div>
      )}
    </article>
  );
});

/* Detail pane */

const PHASES: { key: keyof LivePhases; label: string }[] = [
  { key: 'receivedAt', label: 'Received' },
  { key: 'selectedAt', label: 'Selected' },
  { key: 'writtenAt', label: 'Written' },
  { key: 'headAt', label: 'Head' },
  { key: 'firstContentAt', label: 'First content' },
  { key: 'streamEndAt', label: 'Stream end' },
  { key: 'finishedAt', label: 'Finished' },
];

function DetailPane({ r, onClose, canPreview, frozenAt }: { canPreview: boolean; frozenAt: number | null; onClose: () => void; r: LiveRequest }) {
  const closeRef = useRef<HTMLButtonElement>(null);
  useEffect(() => closeRef.current?.focus(), []);
  const running = r.state === 'in_flight' && frozenAt === null;
  const now = useClock(running, 500);
  const endMs = r.phases.finishedAt ? Date.parse(r.phases.finishedAt) : frozenAt ?? now;
  return (
    <aside className="observe__detail" aria-label="Request detail">
      <div className="observe__detail-head">
        <div>
          <div className="drawer__title mono">{r.id}</div>
          <div className="muted" style={{ fontSize: 12.5 }}>
            {r.kind} on {r.keyId}, {r.transport === 'sse' ? 'SSE' : 'JSON'}
          </div>
        </div>
        <button ref={closeRef} type="button" className="dialog__close" aria-label="Close detail" onClick={onClose}>
          <IconX size={16} />
        </button>
      </div>

      <KeyValue rows={[{ label: 'Error', value: r.errorCode ?? <span className="muted">none</span>, mono: Boolean(r.errorCode) }]} />

      <section>
        <h3 className="section-title">Phases</h3>
        <Waterfall phases={r.phases} endMs={endMs} inFlight={r.state === 'in_flight'} />
      </section>

      <section>
        <h3 className="section-title">Attempts</h3>
        {r.attempts.length === 0 ? (
          <p className="muted">No account selected yet.</p>
        ) : (
          <ul className="attempts">
            {r.attempts.map((a) => (
              <li key={a.index} className={`attempt attempt--${a.outcome === 'in_flight' ? 'succeeded' : a.outcome}`}>
                <div className="attempt__head">
                  <span className="attempt__index">{a.index + 1}</span>
                  <b>{a.accountId}</b>
                  <span className="muted">{a.outcome === 'in_flight' ? 'in flight' : a.outcome.replace('_', ' ')}</span>
                  {a.upstreamStatus !== null && <Badge tone={a.upstreamStatus === 200 ? 'good' : a.upstreamStatus === 429 ? 'warning' : 'critical'}>{a.upstreamStatus}</Badge>}
                </div>
                {a.windows.length > 0 && (
                  <ul className="attempt__windows">
                    {a.windows.map((w) => (
                      <li key={w.id}>
                        <span className="mono muted">{w.id}</span>
                        {w.utilization === null ? <span className="muted">utilization not reported</span> : <span>{fmtPct(w.utilization)} used</span>}
                        <span className="muted">resets {fmtAbsolute(w.resetsAt)}</span>
                      </li>
                    ))}
                  </ul>
                )}
                {a.cooldownSet && (
                  <div className="attempt__cooldown">
                    <IconWarning size={13} />
                    <span>
                      cooldown set: {a.cooldownSet.reason}, until {fmtAbsolute(a.cooldownSet.until)}
                    </span>
                  </div>
                )}
              </li>
            ))}
          </ul>
        )}
      </section>

      <section>
        {r.usage === null ? (
          <>
            <h3 className="section-title">Usage (not reconstructed for this instant)</h3>
            <p className="muted">The ring keeps each request's final usage only, so the tokens counted by this instant are not known.</p>
          </>
        ) : (
          <>
            <h3 className="section-title">Usage{r.state === 'in_flight' ? ' so far' : ''}</h3>
            <KeyValue
              rows={[
                { label: 'Input', value: fmtInt(r.usage.input) },
                { label: 'Cache read', value: fmtInt(r.usage.cacheRead) },
                { label: 'Cache write, 5m', value: fmtInt(r.usage.cacheWrite5m) },
                { label: 'Cache write, 1h', value: fmtInt(r.usage.cacheWrite1h) },
                { label: 'Output', value: fmtInt(r.usage.output) },
                { label: 'Estimated cost', value: r.state === 'finished' ? fmtUsd(r.costUsd) : <span className="muted">not yet</span> },
              ]}
            />
          </>
        )}
      </section>

      <section>
        <h3 className="section-title">Preview</h3>
        {canPreview ? (
          r.preview === null ? (
            <p className="muted">{r.state === 'in_flight' ? 'Waiting for the first content token.' : 'No captured output for this request.'}</p>
          ) : (
            <div className="preview">
              {r.preview}
              {r.state === 'in_flight' && <span className="preview__cursor" aria-hidden />}
            </div>
          )
        ) : (
          <p className="muted">Preview is available to admin sessions.</p>
        )}
      </section>
    </aside>
  );
}

function Waterfall({ phases, endMs, inFlight }: { endMs: number; inFlight: boolean; phases: LivePhases }) {
  const ink = useChartInk();
  const startMs = Date.parse(phases.receivedAt);
  const total = Math.max(1, endMs - startMs);
  const points = PHASES.map((p) => ({ ...p, at: phases[p.key] ? Date.parse(phases[p.key]!) : null }));
  // A segment spans from the previous measured phase to this one; a phase with
  // no timestamp draws nothing rather than a zero-width sliver.
  const segments: { color: string; label: string; width: number }[] = [];
  let prev = startMs;
  points.slice(1).forEach((p, i) => {
    if (p.at === null) return;
    const width = Math.max(0.5, ((p.at - prev) / total) * 100);
    segments.push({ color: i < ink.ramp.length ? ink.ramp[i] : ink.muted, label: p.label, width });
    prev = p.at;
  });
  return (
    <div className="waterfall">
      <div className="waterfall__bar" role="img" aria-label={`Phase waterfall over ${fmtMs(total)}`}>
        {segments.map((s) => (
          <span key={s.label} className="waterfall__seg" style={{ width: `${s.width}%`, background: s.color }} title={s.label} />
        ))}
      </div>
      <ul className="waterfall__legend">
        {points.map((p, i) => (
          <li key={p.key}>
            <span className="waterfall__key" style={{ background: i === 0 ? ink.ink3 : p.at === null ? 'transparent' : i - 1 < ink.ramp.length ? ink.ramp[i - 1] : ink.muted, border: p.at === null ? `1px dashed ${ink.axis}` : undefined }} />
            <span>{p.label}</span>
            {p.at === null ? <span className="muted">{inFlight ? 'not yet' : 'not measured'}</span> : <span className="num">+{fmtMs(p.at - startMs)}</span>}
          </li>
        ))}
      </ul>
    </div>
  );
}
