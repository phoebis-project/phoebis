import { useEffect, useMemo, useRef, useState, type ReactNode, type RefObject } from 'react';
import { Area, AreaChart, Bar, BarChart, CartesianGrid, ComposedChart, Line, LineChart, ResponsiveContainer, Tooltip, XAxis, YAxis } from 'recharts';
import { useSearchParams } from 'react-router-dom';
import { api } from '@/api';
import type { AccountRollup, CacheMissReason, CostSplit, HourlyRollup, LatencyHistograms, WastedProbeReason } from '@/api/types';
import { Donut } from '@/components/charts/Donut';
import { OUTCOME_SERIES, PERCENTILES, PROBE_SERIES, TOKEN_SERIES, accountColor } from '@/components/charts/theme';
import { useChartInk } from '@/components/charts/useChartInk';
import { ChartTooltip, Legend, type TooltipRow } from '@/components/charts/Tooltip';
import { MultiSelect } from '@/components/ui/MultiSelect';
import { IconX } from '@/components/ui/icons';
import { Card, Input, Segmented, Select, Skeleton, Toggle } from '@/components/ui/primitives';
import { CACHE_MISS_LABEL, CACHE_MISS_REASONS, WASTED_PROBE_LABEL, WASTED_PROBE_REASONS, emptyCacheMissReasons, emptyWastedProbeReasons } from '@/lib/cache';
import { fmtAbsolute, fmtCompact, fmtDayShort, fmtDuration, fmtHourTick, fmtInt, fmtMs, fmtPct, fmtUsd, shortId } from '@/lib/format';
import { BUCKET_COUNT, BUCKET_EDGES, PERCENTILE_HINT, emptyHistogram, histogramCount, mergeHistograms, percentileFromHistogram } from '@/lib/histogram';
import { useAsync } from '@/lib/hooks';
import { scrollToSection, useScrollSpy } from '@/lib/scroll';

type Range = '1h' | '24h' | '30d' | '7d' | 'custom';
type GroupBy = 'accountId' | 'keyId' | 'model' | 'poolId';
/** Session filters the rollups but does not group them, so the breakdown tables keep their shape. */
type Dim = GroupBy | 'sessionId';

const HOUR = 3_600_000;
const RANGE_MS: Record<Exclude<Range, 'custom'>, number> = { '1h': HOUR, '24h': 24 * HOUR, '7d': 7 * 24 * HOUR, '30d': 30 * 24 * HOUR };
const GROUP_LABEL: Record<GroupBy, string> = { accountId: 'Upstream', keyId: 'Key', model: 'Model', poolId: 'Pool' };
const DIM_LABEL: Record<Dim, string> = { ...GROUP_LABEL, sessionId: 'Session' };
const DIMS: Dim[] = ['keyId', 'poolId', 'accountId', 'model', 'sessionId'];
const ERROR_CODES = [
  { key: 'rate_limited', code: 'rate_limit_error' },
  { key: 'overloaded', code: 'overloaded_error' },
  { key: 'upstream_error', code: 'api_error' },
] as const;
const COST_PARTS: { key: keyof CostSplit; label: string; slot: number }[] = [
  { key: 'input', label: 'Input', slot: 0 },
  { key: 'cacheRead', label: 'Cache read', slot: 2 },
  { key: 'cacheWrite', label: 'Cache write', slot: 3 },
  { key: 'output', label: 'Output', slot: 6 },
];
const LATENCY_METRICS: { format: (v: number | null) => string; key: keyof LatencyHistograms; label: string; unit: string }[] = [
  { key: 'ttfbMs', label: 'Time to first byte', unit: 'ms', format: fmtMs },
  { key: 'ttftMs', label: 'Time to first content token', unit: 'ms', format: fmtMs },
  { key: 'durationMs', label: 'Total duration', unit: 'ms', format: fmtMs },
  { key: 'outputTps', label: 'Output tokens per second', unit: 'tok/s', format: (v) => (v === null ? '-' : `${v.toFixed(v >= 100 ? 0 : 1)} tok/s`) },
];

interface Agg {
  cacheHits: number;
  cacheMisses: number;
  cacheMissReasons: Record<CacheMissReason, number>;
  cacheRead: number;
  cacheWrite1h: number;
  cacheWrite5m: number;
  cost: CostSplit;
  costUsd: number;
  expiredBetweenTurns: number;
  failovers: number;
  grossSavedUsd: number;
  hist: LatencyHistograms;
  keptWarmRequests: number;
  keptWarmTokens: number;
  migrations: number;
  ok: number;
  output: number;
  overloaded: number;
  probeCostUsd: number;
  probesFailed: number;
  probesMigrated: number;
  probesOk: number;
  probesSkipped: number;
  rate_limited: number;
  uncached: number;
  uncachedInput: number;
  upstream_error: number;
  wastedProbes: number;
  wastedProbeReasons: Record<WastedProbeReason, number>;
  wastedProbeUsd: number;
}

function sumRollups(rows: HourlyRollup[]): Agg {
  const a: Agg = {
    cacheHits: 0,
    cacheMisses: 0,
    cacheMissReasons: emptyCacheMissReasons(),
    cacheRead: 0,
    cacheWrite1h: 0,
    cacheWrite5m: 0,
    cost: { cacheRead: 0, cacheWrite: 0, input: 0, output: 0 },
    costUsd: 0,
    expiredBetweenTurns: 0,
    failovers: 0,
    grossSavedUsd: 0,
    hist: { durationMs: emptyHistogram(), outputTps: emptyHistogram(), ttfbMs: emptyHistogram(), ttftMs: emptyHistogram() },
    keptWarmRequests: 0,
    keptWarmTokens: 0,
    migrations: 0,
    ok: 0,
    output: 0,
    overloaded: 0,
    probeCostUsd: 0,
    probesFailed: 0,
    probesMigrated: 0,
    probesOk: 0,
    probesSkipped: 0,
    rate_limited: 0,
    uncached: 0,
    uncachedInput: 0,
    upstream_error: 0,
    wastedProbes: 0,
    wastedProbeReasons: emptyWastedProbeReasons(),
    wastedProbeUsd: 0,
  };
  for (const r of rows) {
    a.ok += r.requests.ok;
    a.rate_limited += r.requests.rate_limited;
    a.overloaded += r.requests.overloaded;
    a.upstream_error += r.requests.upstream_error;
    a.uncachedInput += r.tokens.uncachedInput;
    a.cacheRead += r.tokens.cacheRead;
    a.cacheWrite5m += r.tokens.cacheWrite5m;
    a.cacheWrite1h += r.tokens.cacheWrite1h;
    a.output += r.tokens.output;
    a.cacheHits += r.cacheHits;
    a.cacheMisses += r.cacheMisses;
    for (const reason of CACHE_MISS_REASONS) a.cacheMissReasons[reason] += r.cacheMissReasons[reason];
    a.expiredBetweenTurns += r.expiredBetweenTurns;
    a.uncached += r.uncached;
    a.costUsd += r.costUsd;
    a.cost.input += r.cost.input;
    a.cost.cacheRead += r.cost.cacheRead;
    a.cost.cacheWrite += r.cost.cacheWrite;
    a.cost.output += r.cost.output;
    a.failovers += r.failovers;
    a.migrations += r.migrations;
    a.keptWarmRequests += r.keptWarm.requests;
    a.keptWarmTokens += r.keptWarm.tokens;
    a.probesOk += r.probes.ok;
    a.probesMigrated += r.probeMigrated;
    a.probesSkipped += r.probes.skipped;
    a.probesFailed += r.probes.failed;
    a.wastedProbes += r.wastedProbes;
    a.wastedProbeUsd += r.wastedProbeUsd;
    for (const reason of WASTED_PROBE_REASONS) a.wastedProbeReasons[reason] += r.wastedProbeReasons[reason];
    a.grossSavedUsd += r.savings.grossUsd;
    a.probeCostUsd += r.savings.probeCostUsd;
  }
  for (const m of LATENCY_METRICS) a.hist[m.key] = mergeHistograms(rows.map((r) => r.histograms[m.key]));
  return a;
}

const requestsOf = (a: Agg) => a.ok + a.rate_limited + a.overloaded + a.upstream_error;
const inputOf = (a: Agg) => a.uncachedInput + a.cacheRead + a.cacheWrite5m + a.cacheWrite1h;
const hitRatioOf = (a: Agg) => (a.cacheHits + a.cacheMisses + a.uncached ? a.cacheHits / (a.cacheHits + a.cacheMisses + a.uncached) : null);
const cachedShareOf = (a: Agg) => (inputOf(a) ? a.cacheRead / inputOf(a) : null);

type Pct = Record<(typeof PERCENTILES)[number]['key'], number | null>;
const pctOf = (h: readonly number[]): Pct => Object.fromEntries(PERCENTILES.map((p) => [p.key, percentileFromHistogram(h, p.p)])) as Pct;

interface Bucket extends Agg {
  at: number;
}

function bucketize(rows: HourlyRollup[], fromMs: number, toMs: number, bucketMs: number): Bucket[] {
  const groups = new Map<number, HourlyRollup[]>();
  const start = Math.floor(fromMs / bucketMs) * bucketMs;
  for (let t = start; t <= toMs; t += bucketMs) groups.set(t, []);
  for (const r of rows) {
    const at = Math.floor(Date.parse(r.hour) / bucketMs) * bucketMs;
    groups.get(at)?.push(r);
  }
  return [...groups.entries()].map(([at, list]) => ({ at, ...sumRollups(list) }));
}

function timeTip<T extends { at: number }>(rows: (p: T) => TooltipRow[]) {
  return function TipContent({ payload }: { payload?: readonly { payload?: unknown }[] }) {
    const p = payload?.[0]?.payload as T | undefined;
    if (!p) return null;
    return <ChartTooltip title={fmtAbsolute(new Date(p.at).toISOString())} rows={rows(p)} />;
  };
}

const bucketSizeFor = (spanMs: number) => (spanMs <= 48 * HOUR ? HOUR : spanMs <= 14 * 24 * HOUR ? 6 * HOUR : 24 * HOUR);
const toLocalInput = (ms: number) => {
  const d = new Date(ms);
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
};

export function AnalyticsPage() {
  const now = useMemo(() => Date.now(), []);
  const [range, setRange] = useState<Range>('24h');
  const [custom, setCustom] = useState({ from: toLocalInput(now - 3 * 24 * HOUR), to: toLocalInput(now) });
  const [groupBy, setGroupBy] = useState<GroupBy>('keyId');
  const [includeProbes, setIncludeProbes] = useState(false);
  const filtersRef = useRef<HTMLDivElement>(null);
  const filtersHeight = useStickyHeight(filtersRef);
  // Three bars stack at the top now, and the filter bar wraps at narrow widths,
  // so the jump offset is measured rather than assumed.
  const headOffset = SECTION_OFFSET + filtersHeight;
  const [searchParams, setSearchParams] = useSearchParams();
  const sessionParam = searchParams.get('session');
  const [filters, setFilters] = useState<Record<Dim, string[]>>(() => ({ accountId: [], keyId: [], model: [], poolId: [], sessionId: sessionParam ? [sessionParam] : [] }));
  const ink = useChartInk();

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

  const span = useMemo(() => {
    if (range === 'custom') {
      const from = Date.parse(custom.from);
      const to = Date.parse(custom.to);
      if (Number.isNaN(from) || Number.isNaN(to) || to <= from) return { fromMs: now - 24 * HOUR, toMs: now, invalid: true };
      return { fromMs: from, toMs: Math.min(to, now), invalid: false };
    }
    return { fromMs: now - RANGE_MS[range], toMs: now, invalid: false };
  }, [range, custom, now]);
  const fromIso = new Date(span.fromMs).toISOString();
  const toIso = new Date(span.toMs).toISOString();
  const rollups = useAsync(() => api.listRollups(fromIso, toIso), [fromIso, toIso]);
  const accountRollups = useAsync(() => api.listAccountRollups(fromIso, toIso), [fromIso, toIso]);

  const options = useMemo(() => {
    const all = rollups.data ?? [];
    const distinct = (dim: Dim) => [...new Set(all.map((r) => r[dim]))].sort();
    return { accountId: distinct('accountId'), keyId: distinct('keyId'), model: distinct('model'), poolId: distinct('poolId'), sessionId: distinct('sessionId') };
  }, [rollups.data]);

  const filtered = useMemo(() => {
    const rows = rollups.data ?? [];
    return rows.filter((r) => {
      if (!includeProbes && r.kind === 'probe') return false;
      for (const dim of DIMS) if (filters[dim].length && !filters[dim].includes(r[dim])) return false;
      return true;
    });
  }, [rollups.data, includeProbes, filters]);
  // Cache-warming figures always include the probe rows; the toggle only scopes the traffic sections.
  const withProbes = useMemo(() => (rollups.data ?? []).filter((r) => DIMS.every((dim) => !filters[dim].length || filters[dim].includes(r[dim]))), [rollups.data, filters]);

  const bucketMs = bucketSizeFor(span.toMs - span.fromMs);
  const series = useMemo(() => bucketize(filtered, span.fromMs, span.toMs, bucketMs), [filtered, span, bucketMs]);
  const probeSeries = useMemo(() => bucketize(withProbes, span.fromMs, span.toMs, bucketMs), [withProbes, span, bucketMs]);
  const totals = useMemo(() => sumRollups(filtered), [filtered]);
  const probeTotals = useMemo(() => sumRollups(withProbes), [withProbes]);
  const groups = useMemo(() => {
    const map = new Map<string, HourlyRollup[]>();
    for (const r of filtered) map.set(r[groupBy], [...(map.get(r[groupBy]) ?? []), r]);
    return [...map.entries()].map(([key, rows]) => ({ key, ...sumRollups(rows) })).sort((a, b) => requestsOf(b) - requestsOf(a));
  }, [filtered, groupBy]);
  // Wasted probes live on probe rows, which the traffic toggle can hide, so the
  // Cache panel and its column read the probe-inclusive rows the way the cache
  // warming section does.
  const probeGroups = useMemo(() => {
    const map = new Map<string, HourlyRollup[]>();
    for (const r of withProbes) map.set(r[groupBy], [...(map.get(r[groupBy]) ?? []), r]);
    return new Map([...map.entries()].map(([key, rows]) => [key, sumRollups(rows)]));
  }, [withProbes, groupBy]);
  const groupSlot = useMemo(() => {
    // Slots follow the full option list so a filter never repaints the survivors.
    const all = options[groupBy];
    return (key: string) => (groupBy === 'accountId' ? accountColor(ink, key) : ink.series[Math.min(7, all.indexOf(key) < 0 ? 7 : all.indexOf(key))]);
  }, [options, groupBy, ink]);

  const tick = (v: number) => (bucketMs >= 6 * HOUR ? fmtDayShort(v) : fmtHourTick(v));
  const loading = !rollups.data;
  const empty = !loading && filtered.length === 0;
  const groupLabel = GROUP_LABEL[groupBy];

  return (
    <div className="stack">
      <div className="filters filters--sticky" ref={filtersRef}>
        <Segmented<Range>
          ariaLabel="Range"
          value={range}
          onChange={setRange}
          options={[
            { value: '1h', label: '1h' },
            { value: '24h', label: '24h' },
            { value: '7d', label: '7d' },
            { value: '30d', label: '30d' },
            { value: 'custom', label: 'Custom' },
          ]}
        />
        {range === 'custom' && (
          <span className="filters__range">
            <Input type="datetime-local" value={custom.from} max={custom.to} onChange={(e) => setCustom({ ...custom, from: e.target.value })} aria-label="From" />
            <span className="muted">to</span>
            <Input type="datetime-local" value={custom.to} min={custom.from} max={toLocalInput(now)} onChange={(e) => setCustom({ ...custom, to: e.target.value })} aria-label="To" />
            {span.invalid && <span className="pairing__expired">End must be after start; showing the last 24h.</span>}
          </span>
        )}
        <span className="filters__sep" aria-hidden />
        <label className="row">
          <span className="muted">Group by</span>
          <Select value={groupBy} onChange={(e) => setGroupBy(e.target.value as GroupBy)}>
            {(Object.keys(GROUP_LABEL) as GroupBy[]).map((g) => (
              <option key={g} value={g}>
                {GROUP_LABEL[g]}
              </option>
            ))}
          </Select>
        </label>
        {/* A chosen session already shows as a removable chip further along this
            bar, so its dropdown would be a second control over one filter. */}
        {DIMS.filter((dim) => dim !== 'sessionId' || filters.sessionId.length === 0).map((dim) => (
          <MultiSelect
            key={dim}
            label={DIM_LABEL[dim]}
            options={options[dim].map((v) => ({ label: dim === 'sessionId' ? shortId(v, 13) : v, value: v }))}
            value={filters[dim]}
            onChange={(v) => setFilters({ ...filters, [dim]: v })}
            mono
          />
        ))}
        <label className="row">
          <Toggle checked={includeProbes} onChange={setIncludeProbes} label="Include probes" />
          <span className="secondary">Include cache probes</span>
        </label>
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
      </div>

      <SectionBar offset={headOffset} top={TOPBAR_HEIGHT + filtersHeight} />

      {empty && <div className="empty">No traffic in this range.</div>}

      {/* Traffic */}
      <Section id="traffic" offset={headOffset} title="Traffic" meta={`${fmtInt(requestsOf(totals))} requests, ${fmtPct(requestsOf(totals) ? (requestsOf(totals) - totals.ok) / requestsOf(totals) : 0, 1)} errors`}>
        <div className="grid-2">
          <ChartCard title="Requests by outcome" subtitle="Per bucket, stacked" loading={loading} legend={OUTCOME_SERIES.map((s) => ({ color: ink.series[s.slot], label: s.label, value: fmtInt(totals[s.key]) }))}>
            <BarChart data={series} margin={{ top: 8, right: 8, bottom: 0, left: 0 }} barCategoryGap={2}>
              <CartesianGrid vertical={false} stroke={ink.grid} />
              <XAxis dataKey="at" tickFormatter={tick} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
              <YAxis tickLine={false} axisLine={false} width={36} allowDecimals={false} />
              <Tooltip cursor={{ fill: ink.grid, opacity: 0.6 }} content={timeTip<Bucket>((p) => OUTCOME_SERIES.map((s) => ({ color: ink.series[s.slot], label: s.label, value: fmtInt(p[s.key]) })))} />
              {OUTCOME_SERIES.map((s, i) => (
                <Bar key={s.key} dataKey={s.key} stackId="a" fill={ink.series[s.slot]} stroke={ink.surface} strokeWidth={1} maxBarSize={22} radius={i === OUTCOME_SERIES.length - 1 ? [3, 3, 0, 0] : 0} isAnimationActive={false} />
              ))}
            </BarChart>
          </ChartCard>

          <ChartCard
            title="Failovers and migrations"
            subtitle="Attempts that moved to another member, and pins that moved between members"
            loading={loading}
            legend={[
              { color: ink.series[3], label: 'Failovers', shape: 'line', value: fmtInt(totals.failovers) },
              { color: ink.series[6], label: 'Migrations', shape: 'line', value: fmtInt(totals.migrations) },
            ]}
          >
            <LineChart data={series} margin={{ top: 8, right: 8, bottom: 0, left: 0 }}>
              <CartesianGrid vertical={false} stroke={ink.grid} />
              <XAxis dataKey="at" tickFormatter={tick} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
              <YAxis tickLine={false} axisLine={false} width={36} allowDecimals={false} />
              <Tooltip
                cursor={{ stroke: ink.axis }}
                content={timeTip<Bucket>((p) => [
                  { color: ink.series[3], label: 'failovers', value: fmtInt(p.failovers) },
                  { color: ink.series[6], label: 'migrations', value: fmtInt(p.migrations) },
                ])}
              />
              <Line type="monotone" dataKey="failovers" stroke={ink.series[3]} strokeWidth={2} dot={false} isAnimationActive={false} />
              <Line type="monotone" dataKey="migrations" stroke={ink.series[6]} strokeWidth={2} dot={false} isAnimationActive={false} />
            </LineChart>
          </ChartCard>

          <Card title="Errors by code" subtitle="Non-200 responses in range">
            {loading ? (
              <Skeleton height={90} />
            ) : (
              <ErrorBars totals={totals} color={ink.series[7]} />
            )}
          </Card>
        </div>
        <BreakdownTable
          title={`Traffic by ${groupLabel.toLowerCase()}`}
          groupLabel={groupLabel}
          columns={['Requests', 'OK', 'Rate limited', 'Overloaded', 'Upstream error', 'Error rate', 'Failovers', 'Migrations']}
          rows={groups.map((g) => ({
            key: g.key,
            cells: [fmtInt(requestsOf(g)), fmtInt(g.ok), fmtInt(g.rate_limited), fmtInt(g.overloaded), fmtInt(g.upstream_error), fmtPct(requestsOf(g) ? (requestsOf(g) - g.ok) / requestsOf(g) : 0, 1), fmtInt(g.failovers), fmtInt(g.migrations)],
          }))}
        />
      </Section>

      {/* Latency */}
      <Section id="latency" offset={headOffset} title="Latency" meta="Percentiles read from merged per-hour histograms">
        <div className="stack">
          {LATENCY_METRICS.map((m) => (
            <LatencyRow key={m.key} metric={m} series={series} hist={totals.hist[m.key]} loading={loading} tick={tick} />
          ))}
        </div>
        <BreakdownTable
          title={`Latency by ${groupLabel.toLowerCase()}`}
          groupLabel={groupLabel}
          columns={[
            'Requests',
            <PctLabel key="a" text="TTFB p50" />,
            <PctLabel key="b" text="TTFB p99" />,
            <PctLabel key="c" text="TTFT p99" />,
            <PctLabel key="d" text="Duration p99" />,
            <PctLabel key="e" text="tok/s p50" />,
          ]}
          rows={groups.map((g) => {
            const ttfb = pctOf(g.hist.ttfbMs);
            const ttft = pctOf(g.hist.ttftMs);
            const dur = pctOf(g.hist.durationMs);
            const tps = pctOf(g.hist.outputTps);
            return { key: g.key, cells: [fmtInt(requestsOf(g)), fmtMs(ttfb.p50), fmtMs(ttfb.p99), fmtMs(ttft.p99), fmtMs(dur.p99), tps.p50 === null ? '-' : tps.p50.toFixed(0)] };
          })}
        />
      </Section>

      {/* Cache */}
      <Section id="cache" offset={headOffset} title="Cache" meta={`${fmtPct(hitRatioOf(totals) ?? 0)} hit ratio, ${fmtInt(totals.expiredBetweenTurns)} expired between turns, ${fmtPct(cachedShareOf(totals) ?? 0)} of input read from cache`}>
        <div className="grid-2">
          <ChartCard title="Tokens" subtitle="Per bucket, stacked by kind" loading={loading} legend={TOKEN_SERIES.map((s) => ({ color: ink.series[s.slot], label: s.label, value: fmtCompact(totals[s.key]) }))}>
            <BarChart data={series} margin={{ top: 8, right: 8, bottom: 0, left: 0 }} barCategoryGap={2}>
              <CartesianGrid vertical={false} stroke={ink.grid} />
              <XAxis dataKey="at" tickFormatter={tick} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
              <YAxis tickFormatter={(v: number) => fmtCompact(v)} tickLine={false} axisLine={false} width={44} />
              <Tooltip cursor={{ fill: ink.grid, opacity: 0.6 }} content={timeTip<Bucket>((p) => TOKEN_SERIES.map((s) => ({ color: ink.series[s.slot], label: s.label, value: fmtCompact(p[s.key]) })))} />
              {TOKEN_SERIES.map((s, i) => (
                <Bar key={s.key} dataKey={s.key} stackId="a" fill={ink.series[s.slot]} stroke={ink.surface} strokeWidth={1} maxBarSize={22} radius={i === TOKEN_SERIES.length - 1 ? [3, 3, 0, 0] : 0} isAnimationActive={false} />
              ))}
            </BarChart>
          </ChartCard>

          <ChartCard
            title="Hit ratio and cached share"
            subtitle="Share of requests that read the cache, and share of input tokens served from it"
            loading={loading}
            legend={[
              { color: ink.series[0], label: 'Cache hit ratio', shape: 'line', value: fmtPct(hitRatioOf(totals) ?? 0) },
              { color: ink.series[2], label: 'Cached share of input', shape: 'line', value: fmtPct(cachedShareOf(totals) ?? 0) },
            ]}
          >
            <LineChart data={series.map((b) => ({ ...b, hitRatio: pct100(hitRatioOf(b)), cachedShare: pct100(cachedShareOf(b)) }))} margin={{ top: 8, right: 8, bottom: 0, left: 0 }}>
              <CartesianGrid vertical={false} stroke={ink.grid} />
              <XAxis dataKey="at" tickFormatter={tick} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
              <YAxis domain={[0, 100]} ticks={[0, 25, 50, 75, 100]} tickFormatter={(v: number) => `${v}%`} tickLine={false} axisLine={false} width={40} />
              <Tooltip
                cursor={{ stroke: ink.axis }}
                content={timeTip<Bucket & { cachedShare: number | null; hitRatio: number | null }>((p) => [
                  { color: ink.series[0], label: 'cache hit ratio', value: p.hitRatio === null ? '-' : `${p.hitRatio.toFixed(0)}%` },
                  { color: ink.series[2], label: 'cached share of input', value: p.cachedShare === null ? '-' : `${p.cachedShare.toFixed(0)}%` },
                ])}
              />
              <Line type="monotone" dataKey="hitRatio" stroke={ink.series[0]} strokeWidth={2} dot={false} connectNulls isAnimationActive={false} />
              <Line type="monotone" dataKey="cachedShare" stroke={ink.series[2]} strokeWidth={2} dot={false} connectNulls isAnimationActive={false} />
            </LineChart>
          </ChartCard>

          <ChartCard
            title="Write cost vs read cost"
            subtitle="USD per bucket spent writing the cache and reading it back"
            loading={loading}
            legend={[
              { color: ink.series[3], label: 'Write cost', value: fmtUsd(totals.cost.cacheWrite) },
              { color: ink.series[2], label: 'Read cost', value: fmtUsd(totals.cost.cacheRead) },
            ]}
          >
            <BarChart data={series} margin={{ top: 8, right: 8, bottom: 0, left: 0 }} barCategoryGap={4} barGap={2}>
              <CartesianGrid vertical={false} stroke={ink.grid} />
              <XAxis dataKey="at" tickFormatter={tick} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
              <YAxis tickFormatter={(v: number) => `$${v.toFixed(v >= 10 ? 0 : 1)}`} tickLine={false} axisLine={false} width={44} />
              <Tooltip
                cursor={{ fill: ink.grid, opacity: 0.6 }}
                content={timeTip<Bucket>((p) => [
                  { color: ink.series[3], label: 'write cost', value: fmtUsd(p.cost.cacheWrite) },
                  { color: ink.series[2], label: 'read cost', value: fmtUsd(p.cost.cacheRead) },
                ])}
              />
              <Bar dataKey="cost.cacheWrite" fill={ink.series[3]} maxBarSize={12} radius={[3, 3, 0, 0]} isAnimationActive={false} />
              <Bar dataKey="cost.cacheRead" fill={ink.series[2]} maxBarSize={12} radius={[3, 3, 0, 0]} isAnimationActive={false} />
            </BarChart>
          </ChartCard>

          <Card title="Misses by reason" subtitle={MISS_REASON_BLURB}>
            {loading ? <Skeleton height={190} /> : <ReasonBars totals={totals} requests={requestsOf(totals)} />}
          </Card>

          <Card title="Wasted probes" subtitle={WASTED_PROBE_BLURB}>
            {loading ? <Skeleton height={190} /> : <WastedProbes totals={probeTotals} />}
          </Card>
        </div>
        <MissReasonsByGroup groups={groups} groupLabel={groupLabel} probeGroups={probeGroups} />
        <CacheByModel rows={filtered} />
      </Section>

      {/* Cost */}
      <Section id="cost" offset={headOffset} title="Cost" meta={`${fmtUsd(totals.costUsd)} in range`}>
        <div className="tiles-row">
          <Tile label="Estimated cost" value={loading ? null : fmtUsd(totals.costUsd)} note="at the effective price list" />
          <Tile label="Cost per request" value={loading ? null : fmtUsd(requestsOf(totals) ? totals.costUsd / requestsOf(totals) : 0)} note={`${fmtInt(requestsOf(totals))} requests`} />
          <Tile label="Cost per 1k output tokens" value={loading ? null : fmtUsd(totals.output ? (totals.costUsd / totals.output) * 1000 : 0)} note={`${fmtCompact(totals.output)} output tokens`} />
        </div>
        <div className="grid-2">
          <CostByGroup series={series} filtered={filtered} groupBy={groupBy} groupSlot={groupSlot} loading={loading} tick={tick} bucketMs={bucketMs} span={span} />
          <Card title="Cost split" subtitle="Share of spend by token kind for the range">
            {loading ? (
              <Skeleton height={148} />
            ) : (
              <CostDonut cost={totals.cost} />
            )}
          </Card>
        </div>
        <BreakdownTable
          title={`Cost by ${groupLabel.toLowerCase()}`}
          groupLabel={groupLabel}
          columns={['Requests', 'Cost', 'Input', 'Cache read', 'Cache write', 'Output', 'Per request', 'Per 1k output']}
          rows={groups.map((g) => ({
            key: g.key,
            cells: [fmtInt(requestsOf(g)), fmtUsd(g.costUsd), fmtUsd(g.cost.input), fmtUsd(g.cost.cacheRead), fmtUsd(g.cost.cacheWrite), fmtUsd(g.cost.output), fmtUsd(requestsOf(g) ? g.costUsd / requestsOf(g) : 0), fmtUsd(g.output ? (g.costUsd / g.output) * 1000 : 0)],
          }))}
        />
      </Section>

      {/* Cache warming */}
      <Section id="cache-warming" offset={headOffset} title="Cache warming" meta="Probe rows are always counted here, whatever the toggle above says">
        <div className="tiles-row">
          <Tile label="Gross saved" value={loading ? null : fmtUsd(probeTotals.grossSavedUsd)} note="cache writes avoided by kept-warm reads" />
          <Tile label="Probe cost" value={loading ? null : fmtUsd(probeTotals.probeCostUsd)} note={`${fmtInt(probeTotals.probesOk + probeTotals.probesSkipped + probeTotals.probesFailed)} probes`} />
          <Tile label="Net saved" value={loading ? null : fmtUsd(probeTotals.grossSavedUsd - probeTotals.probeCostUsd)} note="gross saved minus probe cost" tone={probeTotals.grossSavedUsd - probeTotals.probeCostUsd < 0 ? 'critical' : undefined} />
          <Tile label="Kept warm" value={loading ? null : fmtInt(probeTotals.keptWarmRequests)} note={`${fmtCompact(probeTotals.keptWarmTokens)} tokens read warm`} />
        </div>
        <div className="grid-2">
          <ChartCard title="Probes by outcome" subtitle="Per bucket, stacked" loading={loading} legend={PROBE_SERIES.map((s) => ({ color: ink.series[s.slot], label: s.label, title: s.hint, value: fmtInt(probeTotals[s.key]) }))}>
            <BarChart data={probeSeries} margin={{ top: 8, right: 8, bottom: 0, left: 0 }} barCategoryGap={2}>
              <CartesianGrid vertical={false} stroke={ink.grid} />
              <XAxis dataKey="at" tickFormatter={tick} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
              <YAxis tickLine={false} axisLine={false} width={36} allowDecimals={false} />
              <Tooltip cursor={{ fill: ink.grid, opacity: 0.6 }} content={timeTip<Bucket>((p) => PROBE_SERIES.map((s) => ({ color: ink.series[s.slot], label: s.label, title: s.hint, value: fmtInt(p[s.key]) })))} />
              {PROBE_SERIES.map((s, i) => (
                <Bar key={s.key} dataKey={s.key} stackId="a" fill={ink.series[s.slot]} stroke={ink.surface} strokeWidth={1} maxBarSize={22} radius={i === PROBE_SERIES.length - 1 ? [3, 3, 0, 0] : 0} isAnimationActive={false} />
              ))}
            </BarChart>
          </ChartCard>

          <ChartCard
            title="Savings"
            subtitle="Gross saved and probe cost per bucket; the line is the net"
            loading={loading}
            legend={[
              { color: ink.series[2], label: 'Gross saved', value: fmtUsd(probeTotals.grossSavedUsd) },
              { color: ink.series[3], label: 'Probe cost', value: fmtUsd(probeTotals.probeCostUsd) },
              { color: ink.ink2, label: 'Net saved', shape: 'line', value: <NetValue value={probeTotals.grossSavedUsd - probeTotals.probeCostUsd} /> },
            ]}
          >
            <ComposedChart data={probeSeries.map((b) => ({ ...b, net: b.grossSavedUsd - b.probeCostUsd }))} margin={{ top: 8, right: 8, bottom: 0, left: 0 }} barCategoryGap={4} barGap={2}>
              <CartesianGrid vertical={false} stroke={ink.grid} />
              <XAxis dataKey="at" tickFormatter={tick} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
              <YAxis tickFormatter={(v: number) => `$${v.toFixed(v >= 10 ? 0 : 2)}`} tickLine={false} axisLine={false} width={48} />
              <Tooltip
                cursor={{ fill: ink.grid, opacity: 0.6 }}
                content={timeTip<Bucket & { net: number }>((p) => [
                  { color: ink.series[2], label: 'gross saved', value: fmtUsd(p.grossSavedUsd) },
                  { color: ink.series[3], label: 'probe cost', value: fmtUsd(p.probeCostUsd) },
                  { color: ink.ink2, label: 'net', value: <NetValue value={p.net} /> },
                ])}
              />
              <Bar dataKey="grossSavedUsd" fill={ink.series[2]} maxBarSize={12} radius={[3, 3, 0, 0]} isAnimationActive={false} />
              <Bar dataKey="probeCostUsd" fill={ink.series[3]} maxBarSize={12} radius={[3, 3, 0, 0]} isAnimationActive={false} />
              <Line type="monotone" dataKey="net" stroke={ink.ink2} strokeWidth={2} dot={false} isAnimationActive={false} />
            </ComposedChart>
          </ChartCard>

          <ChartCard title="Kept-warm requests" subtitle="Requests whose cache read followed a probe" loading={loading} legend={[{ color: ink.series[0], label: 'Kept-warm requests', value: fmtInt(probeTotals.keptWarmRequests) }]}>
            <BarChart data={probeSeries} margin={{ top: 8, right: 8, bottom: 0, left: 0 }} barCategoryGap={2}>
              <CartesianGrid vertical={false} stroke={ink.grid} />
              <XAxis dataKey="at" tickFormatter={tick} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
              <YAxis tickLine={false} axisLine={false} width={36} allowDecimals={false} />
              <Tooltip cursor={{ fill: ink.grid, opacity: 0.6 }} content={timeTip<Bucket>((p) => [{ color: ink.series[0], label: 'kept-warm requests', value: fmtInt(p.keptWarmRequests) }])} />
              <Bar dataKey="keptWarmRequests" fill={ink.series[0]} maxBarSize={22} radius={[3, 3, 0, 0]} isAnimationActive={false} />
            </BarChart>
          </ChartCard>

          <ChartCard title="Kept-warm tokens" subtitle="Cache-read tokens on those requests" loading={loading} legend={[{ color: ink.series[2], label: 'Kept-warm tokens', value: fmtCompact(probeTotals.keptWarmTokens) }]}>
            <BarChart data={probeSeries} margin={{ top: 8, right: 8, bottom: 0, left: 0 }} barCategoryGap={2}>
              <CartesianGrid vertical={false} stroke={ink.grid} />
              <XAxis dataKey="at" tickFormatter={tick} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
              <YAxis tickFormatter={(v: number) => fmtCompact(v)} tickLine={false} axisLine={false} width={44} />
              <Tooltip cursor={{ fill: ink.grid, opacity: 0.6 }} content={timeTip<Bucket>((p) => [{ color: ink.series[2], label: 'kept-warm tokens', value: fmtCompact(p.keptWarmTokens) }])} />
              <Bar dataKey="keptWarmTokens" fill={ink.series[2]} maxBarSize={22} radius={[3, 3, 0, 0]} isAnimationActive={false} />
            </BarChart>
          </ChartCard>
        </div>
        <KeepaliveByPool rows={withProbes} />
      </Section>

      {/* Accounts */}
      <Section id="accounts" offset={headOffset} title="Upstreams" meta="Peak utilization, ineligible time, and pin churn per upstream">
        <AccountsSection rollups={accountRollups.data} loading={!accountRollups.data} span={span} bucketMs={bucketMs} tick={tick} />
      </Section>
    </div>
  );
}

const pct100 = (v: number | null) => (v === null ? null : v * 100);

/** Height of the shell's sticky topbar; every bar the page sticks rides under it. */
const TOPBAR_HEIGHT = 52;
// The topbar and the section bar both stick, so a section scrolled to the top
// has to clear them; the filter bar's measured height is added on top.
const SECTION_OFFSET = 100;

/** Live border-box height of a sticky bar, so anchors below it can clear it. */
function useStickyHeight(ref: RefObject<HTMLElement | null>): number {
  const [height, setHeight] = useState(0);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const read = () => setHeight(el.getBoundingClientRect().height);
    read();
    if (typeof ResizeObserver !== 'function') return;
    const observer = new ResizeObserver(read);
    observer.observe(el);
    return () => observer.disconnect();
  }, [ref]);
  return height;
}
const SECTIONS = [
  { id: 'traffic', label: 'Traffic' },
  { id: 'latency', label: 'Latency' },
  { id: 'cache', label: 'Cache' },
  { id: 'cost', label: 'Cost' },
  { id: 'cache-warming', label: 'Cache warming' },
  { id: 'accounts', label: 'Upstreams' },
];

function SectionBar({ offset, top }: { offset: number; top: number }) {
  const [active, select] = useScrollSpy(
    SECTIONS.map((s) => s.id),
    { offset },
  );
  return (
    <nav className="section-bar" aria-label="Sections" style={{ top }}>
      {SECTIONS.map((s) => (
        <button
          key={s.id}
          type="button"
          className={`section-bar__link ${s.id === active ? 'is-active' : ''}`}
          aria-current={s.id === active ? 'true' : undefined}
          onClick={() => {
            select(s.id);
            scrollToSection(s.id, { offset });
          }}
        >
          {s.label}
        </button>
      ))}
    </nav>
  );
}

function Section({ id, title, meta, offset, children }: { children: ReactNode; id: string; meta?: string; offset: number; title: string }) {
  return (
    <section className="analytics-section" id={id} style={{ scrollMarginTop: offset }}>
      <header className="analytics-section__head">
        <h2 className="analytics-section__title">{title}</h2>
        {meta && <span className="analytics-section__meta">{meta}</span>}
      </header>
      {children}
    </section>
  );
}

function ChartCard({ title, subtitle, loading, children, legend, height = 220 }: { children: ReactNode; height?: number; legend?: { color: string; dashed?: boolean; label: string; shape?: 'line' | 'rect'; title?: string; value?: ReactNode }[]; loading: boolean; subtitle?: string; title: string }) {
  return (
    <Card title={title} subtitle={subtitle}>
      {loading ? (
        <Skeleton height={height} />
      ) : (
        <>
          <ResponsiveContainer width="100%" height={height}>
            {children as never}
          </ResponsiveContainer>
          {legend && <Legend items={legend} />}
        </>
      )}
    </Card>
  );
}

function Tile({ label, value, note, tone }: { label: string; note?: string; tone?: 'critical'; value: string | null }) {
  return (
    <div className="stat stat--compact">
      <span className="stat__label">{label}</span>
      <span className={`stat__value ${tone === 'critical' ? 'is-critical' : ''}`}>{value ?? <Skeleton width={70} height={22} />}</span>
      {note && <span className="stat__note">{note}</span>}
    </div>
  );
}

function NetValue({ value }: { value: number }) {
  return <span className={`num ${value < 0 ? 'is-negative' : ''}`}>{fmtUsd(value)}</span>;
}

function PctLabel({ text }: { text: string }) {
  return (
    <span className="pct-label" title={PERCENTILE_HINT}>
      {text}
    </span>
  );
}

function BreakdownTable({ title, groupLabel, columns, rows }: { columns: ReactNode[]; groupLabel: string; rows: { cells: ReactNode[]; key: string }[]; title: string }) {
  return (
    <Card title={title} subtitle="Totals for the selected range and filters" padded={false} className="card--table">
      <div className="table-wrap">
        <table className="table">
          <thead>
            <tr>
              <th>{groupLabel}</th>
              {columns.map((c, i) => (
                <th key={i} className="right">
                  {c}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {rows.map((r) => (
              <tr key={r.key}>
                <td className="mono">{r.key}</td>
                {r.cells.map((c, i) => (
                  <td key={i} className="right num">
                    {c}
                  </td>
                ))}
              </tr>
            ))}
            {rows.length === 0 && (
              <tr>
                <td colSpan={columns.length + 1} className="muted">
                  No rows.
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
    </Card>
  );
}

function ErrorBars({ totals, color }: { color: string; totals: Agg }) {
  const rows = ERROR_CODES.map((e) => ({ code: e.code, count: totals[e.key] })).sort((a, b) => b.count - a.count);
  const max = Math.max(1, ...rows.map((r) => r.count));
  const total = rows.reduce((s, r) => s + r.count, 0);
  if (total === 0) return <p className="muted">No errors in range.</p>;
  return (
    <ul className="hbars hbars--errors">
      {rows.map((r) => (
        <li key={r.code}>
          <span className="mono">{r.code}</span>
          <div className="hbars__track">
            <span className="hbars__fill" style={{ width: `${(r.count / max) * 100}%`, background: color }} />
          </div>
          <span className="num">
            {fmtInt(r.count)} <span className="muted">{fmtPct(r.count / total)}</span>
          </span>
        </li>
      ))}
    </ul>
  );
}

const MISS_REASON_BLURB =
  'From the cache diagnostics beta: why the prefix did not hit, for requests that asked. No divergence with zero cache reads means the entry expired between turns.';

/** The two non-answers read as neutral; a diagnosed divergence reads as a fault, like the error bars above. */
function reasonColor(ink: ReturnType<typeof useChartInk>, reason: CacheMissReason): string {
  if (reason === 'none') return ink.series[2];
  if (reason === 'not_requested') return ink.muted;
  return ink.series[7];
}

function ReasonBars({ totals, requests }: { requests: number; totals: Agg }) {
  const ink = useChartInk();
  const rows = CACHE_MISS_REASONS.map((reason) => ({ count: totals.cacheMissReasons[reason], reason })).sort((a, b) => b.count - a.count);
  const max = Math.max(1, ...rows.map((r) => r.count));
  if (rows.every((r) => r.count === 0)) return <p className="muted">No diagnostics in range.</p>;
  return (
    <ul className="hbars hbars--reasons">
      {rows.map((r) => (
        <li key={r.reason}>
          <span>{CACHE_MISS_LABEL[r.reason]}</span>
          <div className="hbars__track">
            <span className="hbars__fill" style={{ width: `${(r.count / max) * 100}%`, background: reasonColor(ink, r.reason) }} />
          </div>
          <span className="num">
            {fmtInt(r.count)} <span className="muted">{fmtPct(requests ? r.count / requests : 0, 1)}</span>
          </span>
        </li>
      ))}
    </ul>
  );
}

const WASTED_PROBE_BLURB =
  "Probes on recipes that no real request ever followed, settled when the recipe stopped. A high idle-limit share means the pool's max idle time is set too high for how these sessions are used.";

function WastedProbes({ totals }: { totals: Agg }) {
  const ink = useChartInk();
  const rows = WASTED_PROBE_REASONS.map((reason) => ({ count: totals.wastedProbeReasons[reason], reason })).sort((a, b) => b.count - a.count);
  const max = Math.max(1, ...rows.map((r) => r.count));
  if (totals.wastedProbes === 0) return <p className="muted">No wasted probes in range.</p>;
  return (
    <>
      <div className="tiles-row tiles-row--panel">
        <Tile label="Wasted probes" value={fmtInt(totals.wastedProbes)} note={`of ${fmtInt(totals.probesOk + totals.probesSkipped + totals.probesFailed)} probes`} />
        <Tile label="Wasted cost" value={fmtUsd(totals.wastedProbeUsd)} note="at list prices" />
        <Tile label="Share of probe cost" value={fmtPct(totals.probeCostUsd ? totals.wastedProbeUsd / totals.probeCostUsd : 0, 1)} note={`of ${fmtUsd(totals.probeCostUsd)} on probes`} />
      </div>
      <ul className="hbars hbars--reasons">
        {rows.map((r) => (
          <li key={r.reason}>
            <span>{WASTED_PROBE_LABEL[r.reason]}</span>
            <div className="hbars__track">
              <span className="hbars__fill" style={{ width: `${(r.count / max) * 100}%`, background: r.reason === 'idle_limit' ? ink.series[3] : ink.muted }} />
            </div>
            <span className="num">
              {fmtInt(r.count)} <span className="muted">{fmtPct(totals.wastedProbes ? r.count / totals.wastedProbes : 0, 1)}</span>
            </span>
          </li>
        ))}
      </ul>
    </>
  );
}

function MissReasonsByGroup({ groups, groupLabel, probeGroups }: { groupLabel: string; groups: ({ key: string } & Agg)[]; probeGroups: Map<string, Agg> }) {
  return (
    <BreakdownTable
      title={`Misses by reason by ${groupLabel.toLowerCase()}`}
      groupLabel={groupLabel}
      columns={[...CACHE_MISS_REASONS.map((reason) => CACHE_MISS_LABEL[reason]), 'Expired between turns', 'Wasted probes']}
      rows={groups.map((g) => ({
        key: g.key,
        cells: [...CACHE_MISS_REASONS.map((reason) => fmtInt(g.cacheMissReasons[reason])), fmtInt(g.expiredBetweenTurns), fmtInt(probeGroups.get(g.key)?.wastedProbes ?? 0)],
      }))}
    />
  );
}

function LatencyRow({ metric, series, hist, loading, tick }: { hist: number[]; loading: boolean; metric: (typeof LATENCY_METRICS)[number]; series: Bucket[]; tick: (v: number) => string }) {
  const ink = useChartInk();
  const data = useMemo(() => series.map((b) => ({ at: b.at, ...pctOf(b.hist[metric.key]) })), [series, metric.key]);
  const overall = pctOf(hist);
  const fmt = (v: number | null) => metric.format(v);
  return (
    <div className="metric-pair">
      <ChartCard
        title={metric.label}
        subtitle="p50, p90, p95, and p99 per bucket; p99 emphasized"
        loading={loading}
        legend={PERCENTILES.map((p) => ({ color: ink.ramp[p.rampIndex], label: p.label, shape: 'line', title: PERCENTILE_HINT, value: fmt(overall[p.key]) }))}
      >
        <LineChart data={data} margin={{ top: 8, right: 8, bottom: 0, left: 0 }}>
          <CartesianGrid vertical={false} stroke={ink.grid} />
          <XAxis dataKey="at" tickFormatter={tick} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
          <YAxis tickFormatter={(v: number) => (metric.unit === 'ms' ? fmtMs(v) : v.toFixed(0))} tickLine={false} axisLine={false} width={52} />
          <Tooltip cursor={{ stroke: ink.axis }} content={timeTip<{ at: number } & Pct>((p) => PERCENTILES.map((q) => ({ color: ink.ramp[q.rampIndex], label: q.label, title: PERCENTILE_HINT, value: fmt(p[q.key]) })))} />
          {PERCENTILES.map((p) => (
            <Line key={p.key} type="monotone" dataKey={p.key} stroke={ink.ramp[p.rampIndex]} strokeWidth={p.key === 'p99' ? 2.5 : 1.5} dot={false} connectNulls isAnimationActive={false} />
          ))}
        </LineChart>
      </ChartCard>
      <Card title="Distribution" subtitle={`Whole range, ${fmtInt(histogramCount(hist))} samples`}>
        {loading ? <Skeleton height={100} /> : <Histogram h={hist} format={(v) => metric.format(v)} p99={overall.p99} />}
      </Card>
    </div>
  );
}

function Histogram({ h, format, p99 }: { format: (v: number) => string; h: number[]; p99: number | null }) {
  const count = histogramCount(h);
  if (count === 0) return <p className="muted">No samples in range.</p>;
  let first = 0;
  let last = BUCKET_COUNT - 1;
  while (first < last && h[first] === 0) first += 1;
  while (last > first && h[last] === 0) last -= 1;
  const max = Math.max(1, ...h.slice(first, last + 1));
  return (
    <div className="hist">
      <div className="hist__bars" role="img" aria-label={`${count} samples between ${format(BUCKET_EDGES[first])} and ${format(BUCKET_EDGES[last + 1])}`}>
        {h.slice(first, last + 1).map((n, i) => {
          const idx = first + i;
          const isP99 = p99 !== null && p99 >= BUCKET_EDGES[idx] && p99 < BUCKET_EDGES[idx + 1];
          return <span key={idx} className={`hist__bar ${isP99 ? 'is-p99' : ''}`} style={{ height: `${Math.max(2, (n / max) * 100)}%`, opacity: n === 0 ? 0.25 : 1 }} title={`${format(BUCKET_EDGES[idx])} to ${format(BUCKET_EDGES[idx + 1])}: ${fmtInt(n)}`} />;
        })}
      </div>
      <div className="hist__axis num">
        <span>{format(BUCKET_EDGES[first])}</span>
        <span className="pct-label" title={PERCENTILE_HINT}>
          p99 {p99 === null ? '-' : format(p99)}
        </span>
        <span>{format(BUCKET_EDGES[last + 1])}</span>
      </div>
    </div>
  );
}

function CacheByModel({ rows }: { rows: HourlyRollup[] }) {
  const groups = useMemo(() => {
    const map = new Map<string, HourlyRollup[]>();
    for (const r of rows) map.set(r.model, [...(map.get(r.model) ?? []), r]);
    return [...map.entries()].map(([key, list]) => ({ key, ...sumRollups(list) })).sort((a, b) => b.cacheRead - a.cacheRead);
  }, [rows]);
  return (
    <BreakdownTable
      title="Cache by model"
      groupLabel="Model"
      columns={['Reads', 'Writes 5m', 'Writes 1h', 'Hit ratio', 'Cached share', 'Read cost', 'Write cost']}
      rows={groups.map((g) => ({ key: g.key, cells: [fmtCompact(g.cacheRead), fmtCompact(g.cacheWrite5m), fmtCompact(g.cacheWrite1h), fmtPct(hitRatioOf(g) ?? 0), fmtPct(cachedShareOf(g) ?? 0), fmtUsd(g.cost.cacheRead), fmtUsd(g.cost.cacheWrite)] }))}
    />
  );
}

function KeepaliveByPool({ rows }: { rows: HourlyRollup[] }) {
  const groups = useMemo(() => {
    const map = new Map<string, HourlyRollup[]>();
    for (const r of rows) map.set(r.poolId, [...(map.get(r.poolId) ?? []), r]);
    return [...map.entries()].map(([key, list]) => ({ key, ...sumRollups(list) })).sort((a, b) => b.grossSavedUsd - a.grossSavedUsd);
  }, [rows]);
  return (
    <BreakdownTable
      title="Cache warming by pool"
      groupLabel="Pool"
      columns={['Probes OK', 'Skipped', 'Failed', 'Kept-warm requests', 'Kept-warm tokens', 'Gross saved', 'Probe cost', 'Net saved']}
      rows={groups.map((g) => ({
        key: g.key,
        cells: [fmtInt(g.probesOk), fmtInt(g.probesSkipped), fmtInt(g.probesFailed), fmtInt(g.keptWarmRequests), fmtCompact(g.keptWarmTokens), fmtUsd(g.grossSavedUsd), fmtUsd(g.probeCostUsd), <NetValue key="net" value={g.grossSavedUsd - g.probeCostUsd} />],
      }))}
    />
  );
}

function CostDonut({ cost }: { cost: CostSplit }) {
  const ink = useChartInk();
  const [hover, setHover] = useState<string | null>(null);
  const total = COST_PARTS.reduce((s, p) => s + cost[p.key], 0);
  return (
    <div className="split-row">
      <Donut slices={COST_PARTS.map((p) => ({ id: p.key, label: p.label, value: cost[p.key], color: ink.series[p.slot] }))} total={total} centerLabel="cost" centerValue={fmtUsd(total)} onHover={setHover} />
      <ul className="remaining__list">
        {COST_PARTS.map((p) => (
          <li key={p.key} className={hover && hover !== p.key ? 'is-dim' : ''}>
            <span className="legend__swatch" style={{ background: ink.series[p.slot] }} />
            <span>{p.label}</span>
            <span className="num">
              {fmtUsd(cost[p.key])} <span className="muted">{fmtPct(total ? cost[p.key] / total : 0)}</span>
            </span>
          </li>
        ))}
      </ul>
    </div>
  );
}

const MAX_STACKED_GROUPS = 7;

function CostByGroup({ series, filtered, groupBy, groupSlot, loading, tick, bucketMs, span }: { bucketMs: number; filtered: HourlyRollup[]; groupBy: GroupBy; groupSlot: (key: string) => string; loading: boolean; series: Bucket[]; span: { fromMs: number; toMs: number }; tick: (v: number) => string }) {
  const ink = useChartInk();
  const { keys, data } = useMemo(() => {
    const totals = new Map<string, number>();
    for (const r of filtered) totals.set(r[groupBy], (totals.get(r[groupBy]) ?? 0) + r.costUsd);
    const ranked = [...totals.entries()].sort((a, b) => b[1] - a[1]).map(([k]) => k);
    const top = ranked.slice(0, MAX_STACKED_GROUPS);
    const folded = ranked.length > MAX_STACKED_GROUPS;
    const keys = folded ? [...top, 'Other'] : top;
    const rows = series.map((b) => {
      const row: Record<string, number> = { at: b.at };
      for (const k of keys) row[k] = 0;
      return row;
    });
    const byAt = new Map(rows.map((r) => [r.at, r]));
    for (const r of filtered) {
      const at = Math.floor(Date.parse(r.hour) / bucketMs) * bucketMs;
      const row = byAt.get(at);
      if (!row) continue;
      const k = top.includes(r[groupBy]) ? r[groupBy] : 'Other';
      if (k in row) row[k] += r.costUsd;
    }
    return { keys, data: rows };
  }, [series, filtered, groupBy, bucketMs, span]);
  const color = (k: string) => (k === 'Other' ? ink.muted : groupSlot(k));
  return (
    <ChartCard title={`Cost by ${GROUP_LABEL[groupBy].toLowerCase()}`} subtitle="USD per bucket, stacked" loading={loading} legend={keys.map((k) => ({ color: color(k), label: k }))}>
      <AreaChart data={data} margin={{ top: 8, right: 8, bottom: 0, left: 0 }}>
        <CartesianGrid vertical={false} stroke={ink.grid} />
        <XAxis dataKey="at" tickFormatter={tick} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
        <YAxis tickFormatter={(v: number) => `$${v.toFixed(v >= 10 ? 0 : 1)}`} tickLine={false} axisLine={false} width={44} />
        <Tooltip cursor={{ stroke: ink.axis }} content={timeTip<Record<string, number> & { at: number }>((p) => keys.map((k) => ({ color: color(k), label: k, value: fmtUsd(p[k] ?? 0) })))} />
        {keys.map((k) => (
          <Area key={k} type="monotone" dataKey={k} stackId="a" stroke={color(k)} strokeWidth={1.5} fill={color(k)} fillOpacity={0.35} dot={false} isAnimationActive={false} />
        ))}
      </AreaChart>
    </ChartCard>
  );
}

function AccountsSection({ rollups, loading, span, bucketMs, tick }: { bucketMs: number; loading: boolean; rollups: AccountRollup[] | null; span: { fromMs: number; toMs: number }; tick: (v: number) => string }) {
  const ink = useChartInk();
  const accounts = useMemo(() => [...new Set((rollups ?? []).map((r) => r.accountId))].sort(), [rollups]);
  const table = useMemo(
    () =>
      accounts.map((id) => {
        const rows = (rollups ?? []).filter((r) => r.accountId === id);
        const hasFable = rows.some((r) => r.peak['7d_fable'] !== null);
        return {
          id,
          peak5: Math.max(0, ...rows.map((r) => r.peak['5h'])),
          peak7: Math.max(0, ...rows.map((r) => r.peak['7d'])),
          peakF: hasFable ? Math.max(0, ...rows.map((r) => r.peak['7d_fable'] ?? 0)) : null,
          overage: rows.reduce((s, r) => s + r.overageSeconds, 0),
          cooldown: rows.reduce((s, r) => s + r.cooldownSeconds, 0),
          created: rows.reduce((s, r) => s + r.pins.created, 0),
          migratedIn: rows.reduce((s, r) => s + r.pins.migratedIn, 0),
          migratedOut: rows.reduce((s, r) => s + r.pins.migratedOut, 0),
        };
      }),
    [accounts, rollups],
  );
  const data = useMemo(() => {
    const start = Math.floor(span.fromMs / bucketMs) * bucketMs;
    const rows: Record<string, number | null>[] = [];
    const byAt = new Map<number, Record<string, number | null>>();
    for (let t = start; t <= span.toMs; t += bucketMs) {
      const row: Record<string, number | null> = { at: t };
      for (const a of accounts) row[a] = null;
      rows.push(row);
      byAt.set(t, row);
    }
    for (const r of rollups ?? []) {
      const row = byAt.get(Math.floor(Date.parse(r.hour) / bucketMs) * bucketMs);
      if (!row) continue;
      row[r.accountId] = Math.max(row[r.accountId] ?? 0, r.peak['5h'] * 100);
    }
    return rows;
  }, [rollups, accounts, span, bucketMs]);
  return (
    <>
      <ChartCard title="Peak 5-hour utilization" subtitle="Highest utilization seen in each bucket, per upstream" loading={loading} legend={accounts.map((a) => ({ color: accountColor(ink, a), label: a, shape: 'line' }))}>
        <LineChart data={data} margin={{ top: 8, right: 8, bottom: 0, left: 0 }}>
          <CartesianGrid vertical={false} stroke={ink.grid} />
          <XAxis dataKey="at" tickFormatter={tick} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
          <YAxis domain={[0, 100]} ticks={[0, 25, 50, 75, 100]} tickFormatter={(v: number) => `${v}%`} tickLine={false} axisLine={false} width={40} />
          <Tooltip cursor={{ stroke: ink.axis }} content={timeTip<Record<string, number | null> & { at: number }>((p) => accounts.map((a) => ({ color: accountColor(ink, a), label: a, value: p[a] === null || p[a] === undefined ? '-' : `${(p[a] as number).toFixed(0)}%` })))} />
          {accounts.map((a) => (
            <Line key={a} type="monotone" dataKey={a} stroke={accountColor(ink, a)} strokeWidth={1.5} dot={false} connectNulls isAnimationActive={false} />
          ))}
        </LineChart>
      </ChartCard>
      <BreakdownTable
        title="Upstreams in range"
        groupLabel="Upstream"
        columns={['Peak 5h', 'Peak 7d', 'Peak Fable 7d', 'Time in overage', 'Time in cooldown', 'Pins created', 'Migrated in', 'Migrated out']}
        rows={table.map((r) => ({
          key: r.id,
          cells: [
            fmtPct(r.peak5),
            fmtPct(r.peak7),
            r.peakF === null ? <span className="muted" title="no Fable window">n/a</span> : fmtPct(r.peakF),
            r.overage ? fmtDuration(r.overage * 1000) : <span className="muted">none</span>,
            r.cooldown ? fmtDuration(r.cooldown * 1000) : <span className="muted">none</span>,
            fmtInt(r.created),
            fmtInt(r.migratedIn),
            fmtInt(r.migratedOut),
          ],
        }))}
      />
    </>
  );
}
