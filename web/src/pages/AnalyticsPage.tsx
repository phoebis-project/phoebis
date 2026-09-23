import { useEffect, useMemo, useRef, useState, type ReactNode, type RefObject } from 'react';
import { Area, AreaChart, Bar, BarChart, CartesianGrid, ComposedChart, Line, LineChart, ResponsiveContainer, Tooltip, XAxis, YAxis } from 'recharts';
import { useSearchParams } from 'react-router-dom';
import { api } from '@/api';
import { foldGroups, foldRequests, loadSessionRequests, sumGroups } from '@/api/fold';
import type { AccountRollup, AnalyticsGroup, AnalyticsGroupBy, AnalyticsPoint, AnalyticsQuery, CacheMissReason, CostSplit, LatencyHistograms, RequestLog } from '@/api/types';
import { Donut } from '@/components/charts/Donut';
import { OUTCOME_SERIES, PERCENTILES, PROBE_SERIES, TOKEN_SERIES, accountColor } from '@/components/charts/theme';
import { useChartInk } from '@/components/charts/useChartInk';
import { ChartTooltip, Legend, type TooltipRow } from '@/components/charts/Tooltip';
import { MultiSelect, type Option } from '@/components/ui/MultiSelect';
import { IconX } from '@/components/ui/icons';
import { Button, Card, ErrorState, Input, Segmented, Select, Skeleton, Toggle } from '@/components/ui/primitives';
import { bucketSizeFor } from '@/lib/buckets';
import { WASTED_PROBE_LABEL, WASTED_PROBE_REASONS, missLabel, missReasonOrder } from '@/lib/cache';
import { fmtAbsolute, fmtCompact, fmtDayShort, fmtDuration, fmtHourTick, fmtInt, fmtMs, fmtPct, fmtUsd, shortId } from '@/lib/format';
import { BUCKET_COUNT, BUCKET_EDGES, PERCENTILE_HINT, histogramCount } from '@/lib/histogram';
import { useAsync } from '@/lib/hooks';
import { scrollToSection, useScrollSpy } from '@/lib/scroll';

type Range = '1h' | '24h' | '30d' | '7d' | 'custom';
/** The dimensions the breakdown route groups on; `all` is the ungrouped total every section reads. */
type GroupBy = Exclude<AnalyticsGroupBy, 'all'>;
/** The four dimensions both the analytics query and the request log filter on. */
type QueryDim = 'accountId' | 'keyId' | 'model' | 'poolId';
/** No route groups on a session, so it filters the page from the side rather than joining the group-by. */
type Dim = QueryDim | 'sessionId';

const HOUR = 3_600_000;
const RANGE_MS: Record<Exclude<Range, 'custom'>, number> = { '1h': HOUR, '24h': 24 * HOUR, '7d': 7 * 24 * HOUR, '30d': 30 * 24 * HOUR };
const GROUP_LABEL: Record<GroupBy, string> = { account: 'Upstream', key: 'Key', effort: 'Effort', kind: 'Kind', model: 'Model', pool: 'Pool', user_agent: 'Client' };
const DIM_LABEL: Record<QueryDim, string> = { accountId: 'Upstream', keyId: 'Key', model: 'Model', poolId: 'Pool' };
// Ordered the way a request travels them rather than alphabetically: the key names the pool, which picks the upstream.
const DIMS: QueryDim[] = ['keyId', 'poolId', 'accountId', 'model'];
/** Filter choices come from a breakdown on that dimension, so every value the range holds is offered. */
const DIM_GROUP: Record<QueryDim, GroupBy> = { accountId: 'account', keyId: 'key', model: 'model', poolId: 'pool' };
/** The log has no session filter, so a session view pages through it; both bounds together are the note's figure. */
const SESSION_PAGE_SIZE = 500;
const SESSION_PAGE_LIMIT = 20;
const SESSION_SOURCE_NOTE = 'From the request log, since rollups carry no session; cache warming figures are unavailable per session';
const SESSION_COST_NOTE = 'Cost split is not available for a session view.';
const UNAVAILABLE_NOTE = 'not in the request log';
const WARMING_UNAVAILABLE = 'Cache warming figures are not available for a session view.';
/** The breakdown route's bounds on a pattern list; the field holds no more than the route accepts. */
const MAX_PATTERNS = 32;
const MAX_PATTERN_CHARS = 256;
const PATTERN_BOUND_HINT = '32 patterns of 256 characters at most, as the route accepts';
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

const inputOf = (g: AnalyticsGroup) => g.tokens.uncachedInput + g.tokens.cacheRead + g.tokens.cacheWrite5m + g.tokens.cacheWrite1h;
const hitRatioOf = (g: AnalyticsGroup) => (g.cacheHits + g.cacheMisses + g.uncached ? g.cacheHits / (g.cacheHits + g.cacheMisses + g.uncached) : null);
const cachedShareOf = (g: AnalyticsGroup) => (inputOf(g) ? g.tokens.cacheRead / inputOf(g) : null);
const errorRateOf = (g: AnalyticsGroup) => (g.requests ? (g.requests - g.succeeded) / g.requests : 0);
const probesOf = (g: AnalyticsGroup) => g.probes.ok + g.probes.skipped + g.probes.failed;
const usdOrDash = (v: number | null) => (v === null ? '-' : fmtUsd(v));
/** The split as the donut draws it, or null for a group folded from request rows, which carry one total. */
const knownSplit = (c: AnalyticsGroup['cost']): CostSplit | null => (c.cacheRead === null || c.cacheWrite === null ? null : { ...c, cacheRead: c.cacheRead, cacheWrite: c.cacheWrite });

/** Lines past the count are dropped and each line cut at the length, counted in characters as the route counts them. */
function boundPatterns(text: string): { cut: boolean; text: string } {
  const bounded = text
    .split('\n')
    .slice(0, MAX_PATTERNS)
    .map((line) => ([...line].length > MAX_PATTERN_CHARS ? [...line].slice(0, MAX_PATTERN_CHARS).join('') : line))
    .join('\n');
  return { cut: bounded !== text, text: bounded };
}

type Pct = Record<(typeof PERCENTILES)[number]['key'], number | null>;

/** The routes answer one output sum; the chart stacks its thinking share and the rest, so the page derives the rest. */
type SplitTokens = AnalyticsGroup['tokens'] & { plainOutput: number };
const withPlainOutput = (t: AnalyticsGroup['tokens']): SplitTokens => ({ ...t, plainOutput: Math.max(0, t.output - t.thinking) });

interface Bucket extends AnalyticsGroup {
  at: number;
  tokens: SplitTokens;
}

function timeTip<T extends { at: number }>(rows: (p: T) => TooltipRow[]) {
  return function TipContent({ payload }: { payload?: readonly { payload?: unknown }[] }) {
    const p = payload?.[0]?.payload as T | undefined;
    if (!p) return null;
    return <ChartTooltip title={fmtAbsolute(new Date(p.at).toISOString())} rows={rows(p)} />;
  };
}

const toLocalInput = (ms: number) => {
  const d = new Date(ms);
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
};

/** Every key the range holds on one dimension, read once per range so a filter never hides its own options. */
function useDimensionKeys(from: string, to: string, groupBy: GroupBy) {
  return useAsync(() => api.getAnalyticsBreakdown({ from, groupBy, to }), [from, to, groupBy]);
}

const optionsOf = (rows: AnalyticsGroup[] | null): Option[] => (rows ?? []).map((g) => ({ label: g.label, value: g.key })).sort((a, b) => a.label.localeCompare(b.label));

export function AnalyticsPage() {
  const now = useMemo(() => Date.now(), []);
  const [range, setRange] = useState<Range>('24h');
  const [custom, setCustom] = useState({ from: toLocalInput(now - 3 * 24 * HOUR), to: toLocalInput(now) });
  const [groupBy, setGroupBy] = useState<GroupBy>('key');
  const [clientPattern, setClientPattern] = useState('');
  const [clientGroups, setClientGroups] = useState('');
  const [groupsCut, setGroupsCut] = useState(false);
  const [includeProbes, setIncludeProbes] = useState(false);
  const filtersRef = useRef<HTMLDivElement>(null);
  const filtersHeight = useStickyHeight(filtersRef);
  // Three bars stack at the top now, and the filter bar wraps at narrow widths,
  // so the jump offset is measured rather than assumed.
  const headOffset = SECTION_OFFSET + filtersHeight;
  const [searchParams, setSearchParams] = useSearchParams();
  const sessionParam = searchParams.get('session');
  // A session id is one session per key lifetime, so a drill-down from a session row names the lifetime too.
  const sessionKey = sessionParam === null ? null : searchParams.get('key');
  const [filters, setFilters] = useState<Record<Dim, string[]>>(() => ({ accountId: [], keyId: [], model: [], poolId: [], sessionId: sessionParam ? [sessionParam] : [] }));
  const [sessionText, setSessionText] = useState(sessionParam ?? '');
  const ink = useChartInk();

  // Arriving from a session row carries the session in the query; dropping the
  // filter clears the parameter too so this does not put it straight back.
  useEffect(() => {
    if (sessionParam === null) return;
    setFilters((f) => (f.sessionId.includes(sessionParam) ? f : { ...f, sessionId: [sessionParam] }));
    setSessionText(sessionParam);
  }, [sessionParam]);
  /** The one writer for the session filter, so the field, the chip, and the query parameter cannot disagree. */
  const selectSession = (id: string) => {
    setFilters((f) => ((f.sessionId[0] ?? '') === id ? f : { ...f, sessionId: id ? [id] : [] }));
    setSessionText(id);
    if (sessionParam === null || sessionParam === id) return;
    const next = new URLSearchParams(searchParams);
    next.delete('key');
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
  const sessionId = filters.sessionId[0] ?? null;

  const agentGroups = useMemo(() => (groupBy === 'user_agent' && clientGroups.trim() ? clientGroups.split('\n').map((s) => s.trim()).filter(Boolean) : undefined), [clientGroups, groupBy]);
  const query = useMemo<AnalyticsQuery>(
    () => ({ accountIds: filters.accountId, from: fromIso, groupBy, includeProbes, keyIds: filters.keyId, models: filters.model, poolIds: filters.poolId, to: toIso, userAgentGroups: agentGroups, userAgents: clientPattern.trim() ? [clientPattern.trim()] : undefined }),
    [agentGroups, clientPattern, filters, fromIso, groupBy, includeProbes, toIso],
  );
  // The route already narrowed the rows a session view folds, but a cursor page that crossed a filter change
  // may be wider than the pattern, so the fold applies both controls again over what it holds.
  const foldOptions = useMemo(() => ({ agentGroups, agentPatterns: clientPattern.trim() ? [clientPattern.trim()] : undefined }), [agentGroups, clientPattern]);
  // A session view answers every section from the request log, so the rollup reads stay home while one is set.
  const series = useAsync(() => (sessionId === null ? api.getAnalyticsSeries(query) : Promise.resolve<AnalyticsPoint[]>([])), [query, sessionId]);
  const breakdown = useAsync(() => (sessionId === null ? api.getAnalyticsBreakdown(query) : Promise.resolve<AnalyticsGroup[]>([])), [query, sessionId]);
  const byModel = useAsync(() => (sessionId === null ? api.getAnalyticsBreakdown({ ...query, groupBy: 'model' }) : Promise.resolve<AnalyticsGroup[]>([])), [query, sessionId]);
  const byPool = useAsync(() => (sessionId === null ? api.getAnalyticsBreakdown({ ...query, groupBy: 'pool' }) : Promise.resolve<AnalyticsGroup[]>([])), [query, sessionId]);
  const log = useAsync(
    () =>
      sessionId === null
        ? Promise.resolve({ partial: false, requests: [] as RequestLog[] })
        : loadSessionRequests(
            (cursor) =>
              api
                .listRequests({ accountIds: filters.accountId, cursor, from: fromIso, includeProbes, keyIds: sessionKey && filters.keyId.length === 0 ? [sessionKey] : filters.keyId, limit: SESSION_PAGE_SIZE, models: filters.model, poolIds: filters.poolId, search: sessionId, to: toIso, userAgent: clientPattern.trim() || undefined })
                // The search matches an id across several fields, so the page keeps only the exact session; the
                // route reads one key list, so a lifetime beside a key filter is applied here as well.
                .then((page) => ({ ...page, requests: page.requests.filter((r) => r.sessionId === sessionId && (sessionKey === null || `${r.keyId}@${r.keyGeneration}` === sessionKey)) })),
            SESSION_PAGE_LIMIT,
          ),
    [clientPattern, filters, fromIso, includeProbes, sessionId, sessionKey, toIso],
  );
  const accountRollups = useAsync(() => api.listAccountRollups(fromIso, toIso), [fromIso, toIso]);
  const accountKeys = useDimensionKeys(fromIso, toIso, DIM_GROUP.accountId);
  const keyKeys = useDimensionKeys(fromIso, toIso, DIM_GROUP.keyId);
  const modelKeys = useDimensionKeys(fromIso, toIso, DIM_GROUP.model);
  const poolKeys = useDimensionKeys(fromIso, toIso, DIM_GROUP.poolId);
  const options = useMemo<Record<QueryDim, Option[]>>(
    () => ({ accountId: optionsOf(accountKeys.data), keyId: optionsOf(keyKeys.data), model: optionsOf(modelKeys.data), poolId: optionsOf(poolKeys.data) }),
    [accountKeys.data, keyKeys.data, modelKeys.data, poolKeys.data],
  );

  const rangeBucketMs = bucketSizeFor(span.toMs - span.fromMs);
  // A read that failed or that still answers the filters before the change draws nothing, so no figure stands
  // under a filter it was not read for.
  const failure = series.error ?? breakdown.error ?? log.error;
  const stale = series.stale || breakdown.stale || log.stale;
  const retry = () => {
    series.reload();
    breakdown.reload();
    log.reload();
  };
  const rows = useMemo(() => (log.stale ? [] : (log.data?.requests ?? [])), [log.stale, log.data]);
  const points = useMemo(() => (stale ? [] : sessionId === null ? (series.data ?? []) : foldRequests(rows, rangeBucketMs, fromIso, toIso, groupBy, foldOptions)), [stale, sessionId, series.data, rows, rangeBucketMs, fromIso, toIso, groupBy, foldOptions]);
  const buckets = useMemo<Bucket[]>(
    () =>
      points.map((p) => {
        const summed = sumGroups(p.groups);
        return { at: Date.parse(p.at), ...summed, tokens: withPlainOutput(summed.tokens) };
      }),
    [points],
  );
  // The route chooses the bucket width; reading it back off two points keeps the ticks honest when that
  // choice differs from what the range rule would have picked.
  const bucketMs = buckets.length > 1 ? buckets[1].at - buckets[0].at : rangeBucketMs;
  const groups = useMemo(() => (stale ? [] : sessionId === null ? (breakdown.data ?? []) : foldGroups(rows, groupBy, foldOptions)), [stale, sessionId, breakdown.data, rows, groupBy, foldOptions]);
  const poolGroups = useMemo(() => (sessionId === null ? (byPool.stale ? [] : (byPool.data ?? [])) : foldGroups(rows, 'pool', foldOptions)), [sessionId, byPool.stale, byPool.data, rows, foldOptions]);
  const totals = useMemo(() => {
    const summed = sumGroups(groups);
    return { ...summed, tokens: withPlainOutput(summed.tokens) };
  }, [groups]);
  // A session fold has no cost split and no warming facts; those figures read unavailable rather than zero.
  const split = knownSplit(totals.cost);
  const { keptWarm, savings } = totals;
  const otherFilters = filters.keyId.length > 0 || filters.poolId.length > 0 || filters.model.length > 0 || clientPattern.trim() !== '';

  const groupSlot = useMemo(() => {
    // Slots follow the range's whole key list so a filter never repaints the survivors; a dimension with no
    // option list of its own falls back to the keys the breakdown answered.
    const listed = groupBy === 'account' ? options.accountId : groupBy === 'key' ? options.keyId : groupBy === 'model' ? options.model : groupBy === 'pool' ? options.poolId : [];
    const keys = listed.length ? listed.map((o) => o.value) : groups.map((g) => g.key);
    return (key: string) => (groupBy === 'account' ? accountColor(ink, key) : ink.series[Math.min(7, keys.indexOf(key) < 0 ? 7 : keys.indexOf(key))]);
  }, [options, groups, groupBy, ink]);

  const tick = (v: number) => (bucketMs >= 6 * HOUR ? fmtDayShort(v) : fmtHourTick(v));
  const loading = stale || (sessionId === null ? series.data === null || breakdown.data === null : log.data === null);
  const empty = !loading && groups.length === 0;
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
        {!log.stale && log.data?.partial && <span className="secondary">Partial: the first {fmtInt(SESSION_PAGE_LIMIT * SESSION_PAGE_SIZE)} requests of the range</span>}
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
        {DIMS.map((dim) => (
          <MultiSelect key={dim} label={DIM_LABEL[dim]} options={options[dim]} value={filters[dim]} onChange={(v) => setFilters({ ...filters, [dim]: v })} mono />
        ))}
        {/* `.mono` shrinks the text to 0.92em, which would leave this control shorter than the rest of the bar. */}
        <Input value={clientPattern} onChange={(e) => setClientPattern(e.target.value)} placeholder="Client pattern" aria-label="Client pattern" title="* matches any run of characters, ? one; a backslash quotes either" className="mono" style={{ fontSize: 'inherit', width: 180 }} />
        {groupBy === 'user_agent' && (
          <>
            <textarea
              value={clientGroups}
              maxLength={MAX_PATTERNS * (MAX_PATTERN_CHARS + 1)}
              onChange={(e) => {
                const next = boundPatterns(e.target.value);
                setClientGroups(next.text);
                setGroupsCut(next.cut);
              }}
              placeholder={'Client groups, one pattern per line\nclaude-cli/2.1.*\nclaude-cli/2.0.*'}
              aria-label="Client groups"
              title="Each pattern names a group; every other client groups under other"
              className="mono filters__groups"
              rows={2}
            />
            {groupsCut && <span className="secondary">{PATTERN_BOUND_HINT}</span>}
          </>
        )}
        {/* No route enumerates sessions, so this one is typed or pasted rather than picked; the chip beside it
            is how a session arriving from the sessions list shows itself. */}
        <label className="row">
          <span className="muted">Session</span>
          <Input
            type="search"
            placeholder="Session id"
            aria-label="Session id"
            className="filters__session"
            value={sessionText}
            onChange={(e) => setSessionText(e.target.value)}
            onBlur={() => selectSession(sessionText.trim())}
            onKeyDown={(e) => {
              if (e.key === 'Enter') selectSession(sessionText.trim());
            }}
          />
        </label>
        <label className="row">
          <Toggle checked={includeProbes} onChange={setIncludeProbes} label="Include probes" />
          <span className="secondary">Include cache probes</span>
        </label>
        {filters.sessionId.length > 0 && (
          <span className="chips">
            {filters.sessionId.map((id) => (
              <span key={id} className="chip chip--removable mono" title={id}>
                Session {shortId(id, 13)}
                <button type="button" aria-label={`Remove session filter ${id}`} onClick={() => selectSession('')}>
                  <IconX size={12} />
                </button>
              </span>
            ))}
          </span>
        )}
      </div>

      <SectionBar offset={headOffset} top={TOPBAR_HEIGHT + filtersHeight} />

      {failure !== null && (
        <ErrorState
          title="Could not load the analytics for these filters"
          body={failure}
          action={
            <Button size="sm" onClick={retry}>
              Retry
            </Button>
          }
        />
      )}
      {empty && <div className="empty">No traffic in this range.</div>}

      {/* Traffic */}
      <Section id="traffic" offset={headOffset} title="Traffic" meta={loading ? undefined : `${fmtInt(totals.requests)} requests, ${fmtPct(errorRateOf(totals), 1)} errors`}>
        <div className="grid-2">
          <ChartCard title="Requests by outcome" subtitle="Per bucket, stacked" loading={loading} legend={OUTCOME_SERIES.map((s) => ({ color: ink.series[s.slot], label: s.label, value: fmtInt(totals[s.key]) }))}>
            <BarChart data={buckets} margin={{ top: 8, right: 8, bottom: 0, left: 0 }} barCategoryGap={2}>
              <CartesianGrid vertical={false} stroke={ink.grid} />
              <XAxis dataKey="at" tickFormatter={tick} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
              <YAxis tickLine={false} axisLine={false} width={36} allowDecimals={false} />
              <Tooltip cursor={{ fill: ink.grid, opacity: 0.6 }} content={timeTip<Bucket>((p) => OUTCOME_SERIES.map((s) => ({ color: ink.series[s.slot], label: s.label, value: fmtInt(p[s.key]) })))} />
              {OUTCOME_SERIES.map((s, i) => (
                <Bar key={s.key} dataKey={s.key} stackId="a" fill={ink.series[s.slot]} stroke={ink.surface} strokeWidth={1} maxBarSize={22} radius={i === OUTCOME_SERIES.length - 1 ? [3, 3, 0, 0] : 0} isAnimationActive={false} />
              ))}
            </BarChart>
          </ChartCard>

          <ChartCard title="Failovers" subtitle="Attempts that moved to another member" loading={loading} legend={[{ color: ink.series[3], label: 'Failovers', shape: 'line', value: fmtInt(totals.failovers) }]}>
            <LineChart data={buckets} margin={{ top: 8, right: 8, bottom: 0, left: 0 }}>
              <CartesianGrid vertical={false} stroke={ink.grid} />
              <XAxis dataKey="at" tickFormatter={tick} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
              <YAxis tickLine={false} axisLine={false} width={36} allowDecimals={false} />
              <Tooltip cursor={{ stroke: ink.axis }} content={timeTip<Bucket>((p) => [{ color: ink.series[3], label: 'failovers', value: fmtInt(p.failovers) }])} />
              <Line type="monotone" dataKey="failovers" stroke={ink.series[3]} strokeWidth={2} dot={false} isAnimationActive={false} />
            </LineChart>
          </ChartCard>

          <Card title="Errors by code" subtitle="Non-200 responses in range">
            {loading ? <Skeleton height={90} /> : <ErrorBars errors={totals.errors} color={ink.series[7]} />}
          </Card>
        </div>
        <BreakdownTable
          title={`Traffic by ${groupLabel.toLowerCase()}`}
          loading={loading}
          groupLabel={groupLabel}
          columns={['Requests', 'OK', 'Rate limited', 'Failed', 'Exhausted', 'Rejected', 'Error rate', 'Failovers', 'Search', 'Fetch']}
          rows={groups.map((g) => ({
            key: g.key,
            label: g.label,
            // Search and Fetch count the server-tool calls the group's answers made, not tokens.
            cells: [fmtInt(g.requests), fmtInt(g.succeeded), fmtInt(g.rateLimited), fmtInt(g.failed), fmtInt(g.exhausted), fmtInt(g.rejected), fmtPct(errorRateOf(g), 1), fmtInt(g.failovers), fmtInt(g.tokens.webSearch), fmtInt(g.tokens.webFetch)],
          }))}
        />
      </Section>

      {/* Latency */}
      <Section id="latency" offset={headOffset} title="Latency" meta="Percentiles read from merged per-hour histograms">
        <div className="stack">
          {LATENCY_METRICS.map((m) => (
            <LatencyRow key={m.key} metric={m} series={buckets} totals={totals} loading={loading} tick={tick} />
          ))}
        </div>
        <BreakdownTable
          title={`Latency by ${groupLabel.toLowerCase()}`}
          loading={loading}
          groupLabel={groupLabel}
          columns={[
            'Requests',
            <PctLabel key="a" text="TTFB p50" />,
            <PctLabel key="b" text="TTFB p99" />,
            <PctLabel key="c" text="TTFT p99" />,
            <PctLabel key="d" text="Duration p99" />,
            <PctLabel key="e" text="tok/s p50" />,
          ]}
          rows={groups.map((g) => ({
            key: g.key,
            label: g.label,
            cells: [
              fmtInt(g.requests),
              fmtMs(g.percentiles.ttfbMs.p50),
              fmtMs(g.percentiles.ttfbMs.p99),
              fmtMs(g.percentiles.ttftMs.p99),
              fmtMs(g.percentiles.durationMs.p99),
              g.percentiles.outputTps.p50 === null ? '-' : g.percentiles.outputTps.p50.toFixed(0),
            ],
          }))}
        />
      </Section>

      {/* Cache */}
      <Section id="cache" offset={headOffset} title="Cache" meta={loading ? undefined : `${fmtPct(hitRatioOf(totals) ?? 0)} hit ratio, ${fmtInt(totals.expiredBetweenTurns)} expired between turns, ${fmtPct(cachedShareOf(totals) ?? 0)} of input read from cache`}>
        <div className="grid-2">
          <ChartCard title="Tokens" subtitle="Per bucket, stacked by kind" loading={loading} legend={TOKEN_SERIES.map((s) => ({ color: ink.series[s.slot], label: s.label, value: fmtCompact(totals.tokens[s.key]) }))}>
            <BarChart data={buckets} margin={{ top: 8, right: 8, bottom: 0, left: 0 }} barCategoryGap={2}>
              <CartesianGrid vertical={false} stroke={ink.grid} />
              <XAxis dataKey="at" tickFormatter={tick} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
              <YAxis tickFormatter={(v: number) => fmtCompact(v)} tickLine={false} axisLine={false} width={44} />
              <Tooltip cursor={{ fill: ink.grid, opacity: 0.6 }} content={timeTip<Bucket>((p) => TOKEN_SERIES.map((s) => ({ color: ink.series[s.slot], label: s.label, value: fmtCompact(p.tokens[s.key]) })))} />
              {TOKEN_SERIES.map((s, i) => (
                <Bar key={s.key} dataKey={`tokens.${s.key}`} stackId="a" fill={ink.series[s.slot]} stroke={ink.surface} strokeWidth={1} maxBarSize={22} radius={i === TOKEN_SERIES.length - 1 ? [3, 3, 0, 0] : 0} isAnimationActive={false} />
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
            <LineChart data={buckets.map((b) => ({ ...b, hitRatio: pct100(hitRatioOf(b)), cachedShare: pct100(cachedShareOf(b)) }))} margin={{ top: 8, right: 8, bottom: 0, left: 0 }}>
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
              { color: ink.series[3], label: 'Write cost', value: usdOrDash(totals.cost.cacheWrite) },
              { color: ink.series[2], label: 'Read cost', value: usdOrDash(totals.cost.cacheRead) },
            ]}
          >
            <BarChart data={buckets} margin={{ top: 8, right: 8, bottom: 0, left: 0 }} barCategoryGap={4} barGap={2}>
              <CartesianGrid vertical={false} stroke={ink.grid} />
              <XAxis dataKey="at" tickFormatter={tick} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
              <YAxis tickFormatter={(v: number) => `$${v.toFixed(v >= 10 ? 0 : 1)}`} tickLine={false} axisLine={false} width={44} />
              <Tooltip
                cursor={{ fill: ink.grid, opacity: 0.6 }}
                content={timeTip<Bucket>((p) => [
                  { color: ink.series[3], label: 'write cost', value: split === null ? '-' : usdOrDash(p.cost.cacheWrite) },
                  { color: ink.series[2], label: 'read cost', value: split === null ? '-' : usdOrDash(p.cost.cacheRead) },
                ])}
              />
              {split !== null && <Bar dataKey="cost.cacheWrite" fill={ink.series[3]} maxBarSize={12} radius={[3, 3, 0, 0]} isAnimationActive={false} />}
              {split !== null && <Bar dataKey="cost.cacheRead" fill={ink.series[2]} maxBarSize={12} radius={[3, 3, 0, 0]} isAnimationActive={false} />}
            </BarChart>
          </ChartCard>

          <Card title="Misses by reason" subtitle={MISS_REASON_BLURB}>
            {loading ? <Skeleton height={190} /> : <ReasonBars totals={totals} />}
          </Card>
        </div>
        <MissReasonsByGroup groups={groups} groupLabel={groupLabel} loading={loading} />
        {sessionId === null && <CacheByModel groups={byModel.stale ? [] : (byModel.data ?? [])} error={byModel.error} loading={byModel.data === null || byModel.stale} />}
      </Section>

      {/* Cost */}
      <Section id="cost" offset={headOffset} title="Cost" meta={loading ? undefined : `${fmtUsd(totals.costUsd)} in range`}>
        <div className="tiles-row">
          <Tile label="Estimated cost" value={loading ? null : fmtUsd(totals.costUsd)} note="at the effective price list" />
          <Tile label="Cost per request" value={loading ? null : fmtUsd(totals.requests ? totals.costUsd / totals.requests : 0)} note={`${fmtInt(totals.requests)} requests`} />
          <Tile label="Cost per 1k output tokens" value={loading ? null : fmtUsd(totals.tokens.output ? (totals.costUsd / totals.tokens.output) * 1000 : 0)} note={`${fmtCompact(totals.tokens.output)} output tokens`} />
        </div>
        <div className="grid-2">
          <CostByGroup points={points} groupBy={groupBy} groupSlot={groupSlot} loading={loading} tick={tick} />
          <Card title="Cost split" subtitle="Share of spend by token kind for the range">
            {/* The request summary carries one total and no split, so a session view has nothing to draw here. */}
            {sessionId !== null ? <p className="muted">{SESSION_COST_NOTE}</p> : loading || split === null ? <Skeleton height={148} /> : <CostDonut cost={split} />}
          </Card>
        </div>
        {sessionId === null && (
          <BreakdownTable
            title={`Cost by ${groupLabel.toLowerCase()}`}
            loading={loading}
            groupLabel={groupLabel}
            columns={['Requests', 'Cost', 'Input', 'Cache read', 'Cache write', 'Output', 'Per request', 'Per 1k output']}
            rows={groups.map((g) => ({
              key: g.key,
              label: g.label,
              cells: [
                fmtInt(g.requests),
                fmtUsd(g.costUsd),
                fmtUsd(g.cost.input),
                usdOrDash(g.cost.cacheRead),
                usdOrDash(g.cost.cacheWrite),
                fmtUsd(g.cost.output),
                fmtUsd(g.requests ? g.costUsd / g.requests : 0),
                fmtUsd(g.tokens.output ? (g.costUsd / g.tokens.output) * 1000 : 0),
              ],
            }))}
          />
        )}
      </Section>

      {/* Cache warming */}
      <Section id="cache-warming" offset={headOffset} title="Cache warming" meta={sessionId === null ? 'Probe rows are always counted here, whatever the toggle above says' : SESSION_SOURCE_NOTE}>
        <div className="tiles-row">
          <Tile label="Gross saved" value={loading ? null : savings === null ? '-' : fmtUsd(savings.grossUsd)} note="cache writes avoided by kept-warm reads" />
          <Tile label="Probe cost" value={loading ? null : savings === null ? '-' : fmtUsd(savings.probeCostUsd)} note={savings === null ? UNAVAILABLE_NOTE : `${fmtInt(probesOf(totals))} probes`} />
          <Tile
            label="Net saved"
            value={loading ? null : savings === null ? '-' : fmtUsd(savings.grossUsd - savings.probeCostUsd)}
            note="gross saved minus probe cost"
            tone={savings !== null && savings.grossUsd - savings.probeCostUsd < 0 ? 'critical' : undefined}
          />
          <Tile label="Kept warm" value={loading ? null : keptWarm === null ? '-' : fmtInt(keptWarm.requests)} note={keptWarm === null ? UNAVAILABLE_NOTE : `${fmtCompact(keptWarm.tokens)} tokens read warm`} />
        </div>
        <div className="grid-2">
          <ChartCard title="Probes by outcome" subtitle="Per bucket, stacked" loading={loading} legend={PROBE_SERIES.map((s) => ({ color: ink.series[s.slot], label: s.label, title: s.hint, value: savings === null ? '-' : fmtInt(totals.probes[s.key]) }))}>
            <BarChart data={buckets} margin={{ top: 8, right: 8, bottom: 0, left: 0 }} barCategoryGap={2}>
              <CartesianGrid vertical={false} stroke={ink.grid} />
              <XAxis dataKey="at" tickFormatter={tick} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
              <YAxis tickLine={false} axisLine={false} width={36} allowDecimals={false} />
              <Tooltip cursor={{ fill: ink.grid, opacity: 0.6 }} content={timeTip<Bucket>((p) => PROBE_SERIES.map((s) => ({ color: ink.series[s.slot], label: s.label, title: s.hint, value: savings === null ? '-' : fmtInt(p.probes[s.key]) })))} />
              {savings !== null &&
                PROBE_SERIES.map((s, i) => (
                  <Bar key={s.key} dataKey={`probes.${s.key}`} stackId="a" fill={ink.series[s.slot]} stroke={ink.surface} strokeWidth={1} maxBarSize={22} radius={i === PROBE_SERIES.length - 1 ? [3, 3, 0, 0] : 0} isAnimationActive={false} />
                ))}
            </BarChart>
          </ChartCard>

          <ChartCard
            title="Savings"
            subtitle="Gross saved and probe cost per bucket; the line is the net"
            loading={loading}
            legend={[
              { color: ink.series[2], label: 'Gross saved', value: savings === null ? '-' : fmtUsd(savings.grossUsd) },
              { color: ink.series[3], label: 'Probe cost', value: savings === null ? '-' : fmtUsd(savings.probeCostUsd) },
              { color: ink.ink2, label: 'Net saved', shape: 'line', value: savings === null ? '-' : <NetValue value={savings.grossUsd - savings.probeCostUsd} /> },
            ]}
          >
            <ComposedChart data={buckets.map((b) => ({ ...b, net: b.savings === null ? null : b.savings.grossUsd - b.savings.probeCostUsd }))} margin={{ top: 8, right: 8, bottom: 0, left: 0 }} barCategoryGap={4} barGap={2}>
              <CartesianGrid vertical={false} stroke={ink.grid} />
              <XAxis dataKey="at" tickFormatter={tick} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
              <YAxis tickFormatter={(v: number) => `$${v.toFixed(v >= 10 ? 0 : 2)}`} tickLine={false} axisLine={false} width={48} />
              <Tooltip
                cursor={{ fill: ink.grid, opacity: 0.6 }}
                content={timeTip<Bucket & { net: number | null }>((p) => [
                  { color: ink.series[2], label: 'gross saved', value: savings === null || p.savings === null ? '-' : fmtUsd(p.savings.grossUsd) },
                  { color: ink.series[3], label: 'probe cost', value: savings === null || p.savings === null ? '-' : fmtUsd(p.savings.probeCostUsd) },
                  { color: ink.ink2, label: 'net', value: savings === null || p.net === null ? '-' : <NetValue value={p.net} /> },
                ])}
              />
              {savings !== null && <Bar dataKey="savings.grossUsd" fill={ink.series[2]} maxBarSize={12} radius={[3, 3, 0, 0]} isAnimationActive={false} />}
              {savings !== null && <Bar dataKey="savings.probeCostUsd" fill={ink.series[3]} maxBarSize={12} radius={[3, 3, 0, 0]} isAnimationActive={false} />}
              {savings !== null && <Line type="monotone" dataKey="net" stroke={ink.ink2} strokeWidth={2} dot={false} isAnimationActive={false} />}
            </ComposedChart>
          </ChartCard>

          <Card title="Wasted probes" subtitle={WASTED_PROBE_BLURB}>
            {loading ? <Skeleton height={190} /> : savings === null ? <p className="muted">{WARMING_UNAVAILABLE}</p> : <WastedProbes savings={savings} probes={probesOf(totals)} />}
          </Card>

          <ChartCard title="Kept-warm requests" subtitle="Requests whose cache read followed a probe" loading={loading} legend={[{ color: ink.series[0], label: 'Kept-warm requests', value: keptWarm === null ? '-' : fmtInt(keptWarm.requests) }]}>
            <BarChart data={buckets} margin={{ top: 8, right: 8, bottom: 0, left: 0 }} barCategoryGap={2}>
              <CartesianGrid vertical={false} stroke={ink.grid} />
              <XAxis dataKey="at" tickFormatter={tick} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
              <YAxis tickLine={false} axisLine={false} width={36} allowDecimals={false} />
              <Tooltip cursor={{ fill: ink.grid, opacity: 0.6 }} content={timeTip<Bucket>((p) => [{ color: ink.series[0], label: 'kept-warm requests', value: keptWarm === null || p.keptWarm === null ? '-' : fmtInt(p.keptWarm.requests) }])} />
              {keptWarm !== null && <Bar dataKey="keptWarm.requests" fill={ink.series[0]} maxBarSize={22} radius={[3, 3, 0, 0]} isAnimationActive={false} />}
            </BarChart>
          </ChartCard>

          <ChartCard title="Kept-warm tokens" subtitle="Cache-read tokens on those requests" loading={loading} legend={[{ color: ink.series[2], label: 'Kept-warm tokens', value: keptWarm === null ? '-' : fmtCompact(keptWarm.tokens) }]}>
            <BarChart data={buckets} margin={{ top: 8, right: 8, bottom: 0, left: 0 }} barCategoryGap={2}>
              <CartesianGrid vertical={false} stroke={ink.grid} />
              <XAxis dataKey="at" tickFormatter={tick} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
              <YAxis tickFormatter={(v: number) => fmtCompact(v)} tickLine={false} axisLine={false} width={44} />
              <Tooltip cursor={{ fill: ink.grid, opacity: 0.6 }} content={timeTip<Bucket>((p) => [{ color: ink.series[2], label: 'kept-warm tokens', value: keptWarm === null || p.keptWarm === null ? '-' : fmtCompact(p.keptWarm.tokens) }])} />
              {keptWarm !== null && <Bar dataKey="keptWarm.tokens" fill={ink.series[2]} maxBarSize={22} radius={[3, 3, 0, 0]} isAnimationActive={false} />}
            </BarChart>
          </ChartCard>
        </div>
        <KeepaliveByPool groups={poolGroups} error={sessionId === null ? byPool.error : null} loading={sessionId === null ? byPool.data === null || byPool.stale : loading} />
      </Section>

      {/* Accounts */}
      <Section id="accounts" offset={headOffset} title="Upstreams" meta="Peak utilization, ineligible time, and pin churn per upstream">
        <AccountsSection
          accountIds={filters.accountId}
          bucketMs={bucketMs}
          error={accountRollups.error}
          loading={accountRollups.data === null || accountRollups.stale}
          otherFilters={otherFilters}
          rollups={accountRollups.stale ? null : accountRollups.data}
          span={span}
          tick={tick}
        />
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

function BreakdownTable({
  title,
  groupLabel,
  columns,
  rows,
  error = null,
  loading = false,
  subtitle = 'Totals for the selected range and filters',
}: {
  columns: ReactNode[];
  error?: string | null;
  groupLabel: string;
  loading?: boolean;
  rows: { cells: ReactNode[]; key: string; label: string }[];
  subtitle?: string;
  title: string;
}) {
  return (
    <Card title={title} subtitle={subtitle} padded={false} className="card--table">
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
                <td className="mono">{r.label}</td>
                {r.cells.map((c, i) => (
                  <td key={i} className="right num">
                    {c}
                  </td>
                ))}
              </tr>
            ))}
            {rows.length === 0 && (
              <tr>
                <td colSpan={columns.length + 1} className={error ? '' : 'muted'}>
                  {error ? <ErrorState title="Could not load this breakdown" body={error} /> : loading ? <Skeleton height={22} /> : 'No rows.'}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
    </Card>
  );
}

function ErrorBars({ errors, color }: { color: string; errors: Record<string, number> }) {
  const rows = Object.entries(errors)
    .map(([code, count]) => ({ code, count }))
    .sort((a, b) => b.count - a.count || a.code.localeCompare(b.code));
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

function ReasonBars({ totals }: { totals: AnalyticsGroup }) {
  const ink = useChartInk();
  const rows = missReasonOrder([totals])
    .map((reason) => ({ count: totals.cacheMissReasons[reason] ?? 0, reason }))
    .sort((a, b) => b.count - a.count);
  const max = Math.max(1, ...rows.map((r) => r.count));
  if (rows.every((r) => r.count === 0)) return <p className="muted">No diagnostics in range.</p>;
  return (
    <ul className="hbars hbars--reasons">
      {rows.map((r) => (
        <li key={r.reason}>
          <span>{missLabel(r.reason)}</span>
          <div className="hbars__track">
            <span className="hbars__fill" style={{ width: `${(r.count / max) * 100}%`, background: reasonColor(ink, r.reason) }} />
          </div>
          <span className="num">
            {fmtInt(r.count)} <span className="muted">{fmtPct(totals.requests ? r.count / totals.requests : 0, 1)}</span>
          </span>
        </li>
      ))}
    </ul>
  );
}

const WASTED_PROBE_BLURB =
  "Probes on recipes that no real request ever followed, settled when the recipe stopped. A high idle-limit share means the pool's max idle time is set too high for how these sessions are used.";

function WastedProbes({ savings, probes }: { probes: number; savings: NonNullable<AnalyticsGroup['savings']> }) {
  const ink = useChartInk();
  const rows = WASTED_PROBE_REASONS.map((reason) => ({ count: savings.wastedProbeReasons[reason], reason })).sort((a, b) => b.count - a.count);
  const max = Math.max(1, ...rows.map((r) => r.count));
  if (savings.wastedProbes === 0) return <p className="muted">No wasted probes in range.</p>;
  return (
    <>
      <div className="tiles-row tiles-row--panel">
        <Tile label="Wasted probes" value={fmtInt(savings.wastedProbes)} note={`of ${fmtInt(probes)} probes`} />
        <Tile label="Wasted cost" value={fmtUsd(savings.wastedProbeUsd)} note="at list prices" />
        <Tile label="Share of probe cost" value={fmtPct(savings.probeCostUsd ? savings.wastedProbeUsd / savings.probeCostUsd : 0, 1)} note={`of ${fmtUsd(savings.probeCostUsd)} on probes`} />
      </div>
      <ul className="hbars hbars--reasons">
        {rows.map((r) => (
          // The longest reason wraps in this column, so the row anchors the bar and the count to its first line.
          <li key={r.reason} className="hbars__row--top">
            <span>{WASTED_PROBE_LABEL[r.reason]}</span>
            <div className="hbars__track">
              <span className="hbars__fill" style={{ width: `${(r.count / max) * 100}%`, background: r.reason === 'idle_limit' ? ink.series[3] : ink.muted }} />
            </div>
            <span className="num">
              {fmtInt(r.count)} <span className="muted">{fmtPct(savings.wastedProbes ? r.count / savings.wastedProbes : 0, 1)}</span>
            </span>
          </li>
        ))}
      </ul>
    </>
  );
}

function MissReasonsByGroup({ groups, groupLabel, loading }: { groupLabel: string; groups: AnalyticsGroup[]; loading: boolean }) {
  const order = missReasonOrder(groups);
  return (
    <BreakdownTable
      title={`Misses by reason by ${groupLabel.toLowerCase()}`}
      loading={loading}
      groupLabel={groupLabel}
      columns={[...order.map(missLabel), 'Expired between turns']}
      rows={groups.map((g) => ({
        key: g.key,
        label: g.label,
        cells: [...order.map((reason) => fmtInt(g.cacheMissReasons[reason] ?? 0)), fmtInt(g.expiredBetweenTurns)],
      }))}
    />
  );
}

function LatencyRow({ metric, series, totals, loading, tick }: { loading: boolean; metric: (typeof LATENCY_METRICS)[number]; series: Bucket[]; tick: (v: number) => string; totals: AnalyticsGroup }) {
  const ink = useChartInk();
  const data = useMemo(() => series.map((b) => ({ at: b.at, ...b.percentiles[metric.key] })), [series, metric.key]);
  const hist = totals.histograms[metric.key];
  const overall = totals.percentiles[metric.key];
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

function CacheByModel({ groups, error, loading }: { error: string | null; groups: AnalyticsGroup[]; loading: boolean }) {
  const rows = useMemo(() => [...groups].sort((a, b) => b.tokens.cacheRead - a.tokens.cacheRead), [groups]);
  return (
    <BreakdownTable
      title="Cache by model"
      error={error}
      loading={loading}
      groupLabel="Model"
      columns={['Reads', 'Writes 5m', 'Writes 1h', 'Hit ratio', 'Cached share', 'Read cost', 'Write cost']}
      rows={rows.map((g) => ({
        key: g.key,
        label: g.label,
        cells: [fmtCompact(g.tokens.cacheRead), fmtCompact(g.tokens.cacheWrite5m), fmtCompact(g.tokens.cacheWrite1h), fmtPct(hitRatioOf(g) ?? 0), fmtPct(cachedShareOf(g) ?? 0), usdOrDash(g.cost.cacheRead), usdOrDash(g.cost.cacheWrite)],
      }))}
    />
  );
}

function KeepaliveByPool({ groups, error, loading }: { error: string | null; groups: AnalyticsGroup[]; loading: boolean }) {
  const rows = useMemo(() => [...groups].sort((a, b) => (b.savings?.grossUsd ?? 0) - (a.savings?.grossUsd ?? 0)), [groups]);
  return (
    <BreakdownTable
      title="Cache warming by pool"
      error={error}
      loading={loading}
      groupLabel="Pool"
      columns={['Probes OK', 'Skipped', 'Failed', 'Kept-warm requests', 'Kept-warm tokens', 'Gross saved', 'Probe cost', 'Net saved']}
      rows={rows.map((g) => ({
        key: g.key,
        label: g.label,
        // A group folded from request rows measured none of these.
        cells: [
          g.fromRequests ? '-' : fmtInt(g.probes.ok),
          g.fromRequests ? '-' : fmtInt(g.probes.skipped),
          g.fromRequests ? '-' : fmtInt(g.probes.failed),
          g.keptWarm === null ? '-' : fmtInt(g.keptWarm.requests),
          g.keptWarm === null ? '-' : fmtCompact(g.keptWarm.tokens),
          g.savings === null ? '-' : fmtUsd(g.savings.grossUsd),
          g.savings === null ? '-' : fmtUsd(g.savings.probeCostUsd),
          g.savings === null ? '-' : <NetValue key="net" value={g.savings.grossUsd - g.savings.probeCostUsd} />,
        ],
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

function CostByGroup({ points, groupBy, groupSlot, loading, tick }: { groupBy: GroupBy; groupSlot: (key: string) => string; loading: boolean; points: AnalyticsPoint[]; tick: (v: number) => string }) {
  const ink = useChartInk();
  const { keys, labels, data } = useMemo(() => {
    const totals = new Map<string, number>();
    const labels = new Map<string, string>();
    for (const p of points) {
      for (const g of p.groups) {
        totals.set(g.key, (totals.get(g.key) ?? 0) + g.costUsd);
        labels.set(g.key, g.label);
      }
    }
    const ranked = [...totals.entries()].sort((a, b) => b[1] - a[1]).map(([k]) => k);
    const top = ranked.slice(0, MAX_STACKED_GROUPS);
    const keys = ranked.length > MAX_STACKED_GROUPS ? [...top, 'Other'] : top;
    const data = points.map((p) => {
      const row: Record<string, number> = { at: Date.parse(p.at) };
      for (const k of keys) row[k] = 0;
      for (const g of p.groups) {
        const k = top.includes(g.key) ? g.key : 'Other';
        if (k in row) row[k] += g.costUsd;
      }
      return row;
    });
    return { keys, labels, data };
  }, [points]);
  const color = (k: string) => (k === 'Other' ? ink.muted : groupSlot(k));
  const label = (k: string) => labels.get(k) ?? k;
  return (
    <ChartCard title={`Cost by ${GROUP_LABEL[groupBy].toLowerCase()}`} subtitle="USD per bucket, stacked" loading={loading} legend={keys.map((k) => ({ color: color(k), label: label(k) }))}>
      <AreaChart data={data} margin={{ top: 8, right: 8, bottom: 0, left: 0 }}>
        <CartesianGrid vertical={false} stroke={ink.grid} />
        <XAxis dataKey="at" tickFormatter={tick} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
        <YAxis tickFormatter={(v: number) => `$${v.toFixed(v >= 10 ? 0 : 1)}`} tickLine={false} axisLine={false} width={44} />
        <Tooltip cursor={{ stroke: ink.axis }} content={timeTip<Record<string, number> & { at: number }>((p) => keys.map((k) => ({ color: color(k), label: label(k), value: fmtUsd(p[k] ?? 0) })))} />
        {keys.map((k) => (
          <Area key={k} type="monotone" dataKey={k} stackId="a" stroke={color(k)} strokeWidth={1.5} fill={color(k)} fillOpacity={0.35} dot={false} isAnimationActive={false} />
        ))}
      </AreaChart>
    </ChartCard>
  );
}

const NO_SAMPLE_HINT = 'no utilization sampled in range';

/** An hour with no sample carries no peak, so the maximum is over the sampled hours alone and null without one. */
function peakOf(values: (number | null)[]): number | null {
  const sampled = values.filter((v): v is number => v !== null);
  return sampled.length ? Math.max(...sampled) : null;
}

/**
 * The account rollups carry the range and the upstream alone, so the page's upstream selection narrows them here
 * and every other filter says it does not apply.
 */
function AccountsSection({
  accountIds,
  bucketMs,
  error,
  loading,
  otherFilters,
  rollups: all,
  span,
  tick,
}: {
  accountIds: string[];
  bucketMs: number;
  error: string | null;
  loading: boolean;
  otherFilters: boolean;
  rollups: AccountRollup[] | null;
  span: { fromMs: number; toMs: number };
  tick: (v: number) => string;
}) {
  const ink = useChartInk();
  const rollups = useMemo(() => (all === null ? null : accountIds.length === 0 ? all : all.filter((r) => accountIds.includes(r.accountId))), [all, accountIds]);
  const accounts = useMemo(() => [...new Set((rollups ?? []).map((r) => r.accountId))].sort(), [rollups]);
  const table = useMemo(
    () =>
      accounts.map((id) => {
        const rows = (rollups ?? []).filter((r) => r.accountId === id);
        return {
          id,
          peak5: peakOf(rows.map((r) => r.peak['5h'])),
          peak7: peakOf(rows.map((r) => r.peak['7d'])),
          peakF: peakOf(rows.map((r) => r.peak['7d_fable'])),
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
      const peak = r.peak['5h'];
      if (!row || peak === null) continue;
      row[r.accountId] = Math.max(row[r.accountId] ?? 0, peak * 100);
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
        subtitle={`Totals for the selected range and upstreams${otherFilters ? '; the key, pool, model and client filters do not apply here' : ''}`}
        error={error}
        loading={loading}
        groupLabel="Upstream"
        columns={['Peak 5h', 'Peak 7d', 'Peak Fable 7d', 'Time in overage', 'Time in cooldown', 'Pins created', 'Migrated in', 'Migrated out']}
        rows={table.map((r) => ({
          key: r.id,
          label: r.id,
          cells: [
            r.peak5 === null ? <span className="muted" title={NO_SAMPLE_HINT}>-</span> : fmtPct(r.peak5),
            r.peak7 === null ? <span className="muted" title={NO_SAMPLE_HINT}>-</span> : fmtPct(r.peak7),
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
