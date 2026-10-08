import { useEffect, useMemo, useRef, useState, type ReactNode, type RefObject } from 'react';
import { Area, AreaChart, Bar, BarChart, CartesianGrid, Cell, ComposedChart, Dot, Line, LineChart, ReferenceArea, ResponsiveContainer, Tooltip, XAxis, YAxis } from 'recharts';
import { Link, useLocation, useNavigate, useSearchParams } from 'react-router-dom';
import { api } from '@/api';
import { foldGroups, foldRequests, loadSessionRequests, sumGroups } from '@/api/fold';
import type { AccountRollup, AnalyticsGroup, AnalyticsGroupBy, AnalyticsPoint, AnalyticsQuery, BurnGroup, BurnPoint, CacheMissReason, CostSplit, LatencyHistograms, MemberBudget, PlacementRow, RequestLog, RewritePoint, SubscriptionCosts } from '@/api/types';
import { Donut } from '@/components/charts/Donut';
import { OUTCOME_SERIES, PERCENTILES, PROBE_SERIES, TOKEN_SERIES, accountColor, type ChartInk } from '@/components/charts/theme';
import { useChartInk } from '@/components/charts/useChartInk';
import { ChartTooltip, Legend, type TooltipRow } from '@/components/charts/Tooltip';
import { CostReferenceLink } from '@/components/ui/CostReference';
import { MultiSelect, type Option } from '@/components/ui/MultiSelect';
import { PatternInput } from '@/components/ui/PatternInput';
import { Tile } from '@/components/ui/Tile';
import { IconX } from '@/components/ui/icons';
import { Badge, Button, Card, ErrorState, Input, Meter, Segmented, Select, Skeleton, Toggle } from '@/components/ui/primitives';
import { SectionBar } from '@/components/SectionBar';
import { CONVERSATION_CLASSES, MAX_BURN_LINES, foldBurn, isolatedSlots, moveLabel, niceAxisTop, placementCells, placementRungLabel, readBurn, sizedAccounts, slotUnpriced, type BurnLine, type BurnLineUnit, type BurnRow } from '@/lib/analytics';
import { bucketSizeFor } from '@/lib/buckets';
import { WASTED_PROBE_LABEL, WASTED_PROBE_REASONS, missCount, missLabel, missReasonOrder } from '@/lib/cache';
import { clock } from '@/lib/clock';
import { bandRuns } from '@/lib/costs';
import { fmtAbsolute, fmtCompact, fmtDayShort, fmtDuration, fmtHourTick, fmtInt, fmtMs, fmtPct, fmtPeriodEnd, fmtShare, fmtUsd, fmtUsdTick, shortId, toLocalInput, upperFirst, usdTickDigits } from '@/lib/format';
import { BUCKET_COUNT, BUCKET_EDGES, PERCENTILE_HINT, histogramCount } from '@/lib/histogram';
import { useAsync, useCommittedText, type Loaded } from '@/lib/hooks';

type Range = '1h' | '24h' | '30d' | '7d' | 'custom';
/** The dimensions the breakdown route groups on; `all` is the ungrouped total every section reads. */
type GroupBy = Exclude<AnalyticsGroupBy, 'all'>;
/** The four dimensions both the analytics query and the request log filter on. */
type QueryDim = 'accountId' | 'keyId' | 'model' | 'poolId';
/** No route groups on a session, so it filters the page from the side rather than joining the group-by. */
type Dim = QueryDim | 'sessionId';

const HOUR = 3_600_000;
const RANGE_MS: Record<Exclude<Range, 'custom'>, number> = { '1h': HOUR, '24h': 24 * HOUR, '7d': 7 * 24 * HOUR, '30d': 30 * 24 * HOUR };
const GROUP_LABEL: Record<GroupBy, string> = { account: 'Account', client: 'Client', key: 'Key', effort: 'Effort', kind: 'Kind', model: 'Model', pool: 'Pool', request_class: 'Request class', speed: 'Speed', user_agent: 'User agent' };
/** The two speeds the rollups file an answer under, standard at the foot of each stacked bar. */
const SPEED_SERIES = [
  { key: 'standard', label: 'standard', slot: 0 },
  { key: 'fast', label: 'fast', slot: 4 },
] as const;
const BURN_GROUP_LABEL: Record<BurnGroup, string> = { account: 'Account', key: 'Key', request_class: 'Request class', session: 'Session' };
// Account first: the route's default, and the one grouping that draws the plan share.
const BURN_GROUPS: BurnGroup[] = ['account', 'key', 'session', 'request_class'];
const DIM_LABEL: Record<QueryDim, string> = { accountId: 'Account', keyId: 'Key', model: 'Model', poolId: 'Pool' };
// Ordered the way a request travels them rather than alphabetically: the key names the pool, which picks the account.
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
const probesOf = (g: AnalyticsGroup) => g.probes.ok + g.probes.held + g.probes.skipped + g.probes.failed;
const usdOrDash = (v: number | null) => (v === null ? '-' : fmtUsd(v));
/**
 * A dollar axis's tick labels, all at the one precision the largest figure the axis draws sets, and a width that
 * holds the longest of them: the chart's box clips a label wider than its axis. At the axis's 11 px a digit or the
 * currency sign takes 7 px, the point 3.5 and a minus 4, with 8 px between a label and the plot. 56 px is the floor,
 * so the dollar axes line up at the precisions a busy router draws.
 */
const usdAxis = (values: Iterable<number>) => {
  let max = 0;
  let negative = false;
  for (const v of values) {
    max = Math.max(max, Math.abs(v));
    negative ||= v < 0;
  }
  const digits = usdTickDigits([max]);
  // The top tick can round up past the largest figure into the next power of ten, $9.20 to $10.00.
  const whole = max < 1 ? 1 : Math.floor(Math.log10(max)) + 2;
  const label = 7 + 7 * whole + (digits > 0 ? 3.5 + 7 * digits : 0) + (negative ? 4 : 0);
  return { tickFormatter: (v: number) => fmtUsdTick(v, digits), width: Math.max(56, Math.ceil(label) + 8) };
};
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

/**
 * Every key the range holds on one dimension, read once per range so a filter never hides its own options. With no
 * filter set and the page grouped on this dimension, the page's breakdown is that very read and answers for it. While
 * the source is out (the breakdown reloading, perhaps still on the previous grouping, the own read starting once a
 * filter is set, or either one reading a new range), the last answer that was current stands in, so a list never
 * blanks and only ever offers this dimension's own keys.
 */
function useDimensionKeys(from: string, to: string, groupBy: GroupBy, generation: number, shared?: Loaded<AnalyticsGroup[]>): Loaded<AnalyticsGroup[] | null> {
  const own = useAsync(() => (shared ? Promise.resolve(null) : api.getAnalyticsBreakdown({ from, groupBy, to })), [from, groupBy, to, generation, shared !== undefined]);
  const source = shared ?? own;
  const current = source.stale ? null : source.data;
  const [last, setLast] = useState<AnalyticsGroup[] | null>(null);
  useEffect(() => {
    if (current !== null) setLast(current);
  }, [current]);
  return current !== null ? source : { ...source, data: last };
}

const optionsOf = (rows: AnalyticsGroup[] | null): Option[] => (rows ?? []).map((g) => ({ label: g.label, value: g.key })).sort((a, b) => a.label.localeCompare(b.label));

export function AnalyticsPage() {
  const [now, setNow] = useState(() => clock.now());
  // Try again bumps it with the end, so every read keyed on the window runs again even while the clock stands still.
  const [generation, setGeneration] = useState(0);
  const moveEnd = () => setNow(clock.now());
  const [range, setRange] = useState<Range>('24h');
  const [custom, setCustom] = useState({ from: toLocalInput(now - 3 * 24 * HOUR), to: toLocalInput(now) });
  const [groupBy, setGroupBy] = useState<GroupBy>('key');
  const [userAgentPattern, setUserAgentPattern] = useState('');
  const [classPattern, setClassPattern] = useState('');
  const [clientGroups, setClientGroups] = useState('');
  const [groupsCut, setGroupsCut] = useState(false);
  const [includeProbes, setIncludeProbes] = useState(false);
  const [burnGroup, setBurnGroup] = useState<BurnGroup>('account');
  const filtersRef = useRef<HTMLDivElement>(null);
  const filtersHeight = useStickyHeight(filtersRef);
  const location = useLocation();
  const navigate = useNavigate();
  // The hash is the tab: a link or a reload lands on the section it names, and a change replaces the entry with the
  // search kept, so a session filter survives and Back leaves the page rather than walking the tabs.
  const tab = useMemo<SectionId>(() => {
    const id = location.hash.slice(1);
    return SECTIONS.some((s) => s.id === id) ? (id as SectionId) : 'traffic';
  }, [location.hash]);
  const chooseTab = (id: SectionId) => {
    navigate({ hash: `#${id}`, pathname: location.pathname, search: location.search }, { replace: true });
    scroller()?.scrollTo?.({ top: 0 });
  };
  const [searchParams] = useSearchParams();
  const sessionParam = searchParams.get('session');
  // A session id is one session per key lifetime, so a drill-down from a session row names the lifetime too.
  const sessionKey = sessionParam === null ? null : searchParams.get('key');
  const [filters, setFilters] = useState<Record<Dim, string[]>>(() => ({ accountId: [], keyId: [], model: [], poolId: [], sessionId: sessionParam ? [sessionParam] : [] }));
  const [sessionText, setSessionText] = useState(sessionParam ?? '');
  const ink = useChartInk();
  // A committed filter reads the window up to the instant it changed, as a range change does.
  const commitClass = (next: string) => {
    setClassPattern(next);
    moveEnd();
  };
  const commitAgent = (next: string) => {
    setUserAgentPattern(next);
    moveEnd();
  };
  const commitGroups = (next: string) => {
    setClientGroups(next);
    moveEnd();
  };
  const changeRange = (next: Range) => {
    if (next === range) return;
    setRange(next);
    moveEnd();
  };
  const changeCustom = (next: { from: string; to: string }) => {
    setCustom(next);
    moveEnd();
  };
  const changeGroupBy = (next: GroupBy) => {
    setGroupBy(next);
    moveEnd();
  };
  const changeFilter = (dim: QueryDim, next: string[]) => {
    setFilters({ ...filters, [dim]: next });
    moveEnd();
  };
  const changeProbes = (on: boolean) => {
    setIncludeProbes(on);
    moveEnd();
  };
  const changeBurnGroup = (next: BurnGroup) => {
    if (next === burnGroup) return;
    setBurnGroup(next);
    moveEnd();
  };
  // Enter inserts a line in the groups field, so it commits on the pause and on blur alone.
  const groupsText = useCommittedText(clientGroups, commitGroups);

  // Arriving from a session row carries the session in the query; dropping the
  // filter clears the parameter too so this does not put it straight back.
  useEffect(() => {
    if (sessionParam === null) return;
    setFilters((f) => (f.sessionId.includes(sessionParam) ? f : { ...f, sessionId: [sessionParam] }));
    setSessionText(sessionParam);
  }, [sessionParam]);
  /** The one writer for the session filter, so the field, the chip, and the query parameter cannot disagree. */
  const selectSession = (id: string) => {
    if ((filters.sessionId[0] ?? '') !== id) moveEnd();
    setFilters((f) => ((f.sessionId[0] ?? '') === id ? f : { ...f, sessionId: id ? [id] : [] }));
    setSessionText(id);
    if (sessionParam === null || sessionParam === id) return;
    const next = new URLSearchParams(searchParams);
    next.delete('key');
    next.delete('session');
    const search = next.toString();
    navigate({ hash: location.hash, pathname: location.pathname, search: search ? `?${search}` : '' }, { replace: true });
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
    () => ({ accountIds: filters.accountId, clients: classPattern.trim() ? [classPattern.trim()] : undefined, from: fromIso, groupBy, includeProbes, keyIds: filters.keyId, models: filters.model, poolIds: filters.poolId, to: toIso, userAgentGroups: agentGroups, userAgents: userAgentPattern.trim() ? [userAgentPattern.trim()] : undefined }),
    [agentGroups, classPattern, userAgentPattern, filters, fromIso, groupBy, includeProbes, toIso],
  );
  // The route narrows the rows a session view folds by the class pattern alone, and the fold applies both pattern
  // controls over what it holds: a cursor page that crossed a filter change may be wider than the pattern, and the
  // user agent matches its folded token, as the unscoped page's rollups do, where the route matches the raw header.
  const foldOptions = useMemo(() => ({ agentGroups, agentPatterns: userAgentPattern.trim() ? [userAgentPattern.trim()] : undefined, clientPatterns: classPattern.trim() ? [classPattern.trim()] : undefined }), [agentGroups, classPattern, userAgentPattern]);
  // A session view answers every section from the request log, so the rollup reads stay home while one is set.
  const series = useAsync(() => (sessionId === null ? api.getAnalyticsSeries(query) : Promise.resolve<AnalyticsPoint[]>([])), [query, sessionId, generation]);
  const breakdown = useAsync(() => (sessionId === null ? api.getAnalyticsBreakdown(query) : Promise.resolve<AnalyticsGroup[]>([])), [query, sessionId, generation]);
  // A read only a hidden tab draws answers null without a request, and runs once its tab is chosen.
  const byModel = useAsync(() => (tab === 'cache' && sessionId === null ? api.getAnalyticsBreakdown({ ...query, groupBy: 'model' }) : Promise.resolve<AnalyticsGroup[] | null>(sessionId === null ? null : [])), [query, sessionId, tab, generation]);
  const byPool = useAsync(() => (tab === 'cache-warming' && sessionId === null ? api.getAnalyticsBreakdown({ ...query, groupBy: 'pool' }) : Promise.resolve<AnalyticsGroup[] | null>(sessionId === null ? null : [])), [query, sessionId, tab, generation]);
  const log = useAsync(
    () =>
      sessionId === null
        ? Promise.resolve({ partial: false, requests: [] as RequestLog[] })
        : loadSessionRequests(
            (cursor) =>
              api
                .listRequests({ accountIds: filters.accountId, client: classPattern.trim() || undefined, cursor, from: fromIso, includeProbes, keyIds: sessionKey && filters.keyId.length === 0 ? [sessionKey] : filters.keyId, limit: SESSION_PAGE_SIZE, models: filters.model, poolIds: filters.poolId, search: sessionId, to: toIso })
                // The search matches an id across several fields, so the page keeps only the exact session; the
                // route reads one key list, so a lifetime beside a key filter is applied here as well.
                .then((page) => ({ ...page, requests: page.requests.filter((r) => r.sessionId === sessionId && (sessionKey === null || `${r.keyId}@${r.keyGeneration}` === sessionKey)) })),
            SESSION_PAGE_LIMIT,
          ),
    [classPattern, filters, fromIso, includeProbes, sessionId, sessionKey, toIso, generation],
  );
  const accountRollups = useAsync(() => (tab === 'accounts' ? api.listAccountRollups(fromIso, toIso) : Promise.resolve(null)), [fromIso, toIso, tab, generation]);
  // The fold rates the hour still running over the part of it this read saw, so the answer keeps its end and instant.
  // The instant is the read's own: the range clock stands still while another tab is open, and a read made later in
  // the same hour has seen more of it.
  const burn = useAsync(() => {
    if (tab !== 'burn') return Promise.resolve(null);
    const readAt = clock.now();
    return readBurn(api, { from: fromIso, to: toIso }, burnGroup).then((read) => ({ ...read, endMs: span.toMs, readAt }));
  }, [fromIso, toIso, burnGroup, tab, generation]);
  // The plan chart's line category reads each account's stored tier against the cost model's capacity rows.
  const burnSettings = useAsync(() => (tab === 'burn' ? api.getSettings() : Promise.resolve(null)), [tab]);
  const sized = useMemo(() => (burn.data && burnSettings.data ? sizedAccounts(burn.data.accounts, burnSettings.data.subscriptionCosts) : null), [burn.data, burnSettings.data]);
  const rewrites = useAsync(() => (tab === 'traffic' ? api.getAnalyticsRewrites({ from: fromIso, to: toIso }) : Promise.resolve(null)), [fromIso, toIso, tab, generation]);
  // Side groupings on the page's own query, as byModel and byPool are; a session view folds them from the log.
  const bySpeed = useAsync(() => (sessionId === null ? api.getAnalyticsSeries({ ...query, groupBy: 'speed' }) : Promise.resolve<AnalyticsPoint[]>([])), [query, sessionId, generation]);
  const byEffort = useAsync(() => (tab === 'traffic' && sessionId === null ? api.getAnalyticsBreakdown({ ...query, groupBy: 'effort' }) : Promise.resolve<AnalyticsGroup[] | null>(sessionId === null ? null : [])), [query, sessionId, tab, generation]);
  const placements = useAsync(
    () => (tab === 'cache' && sessionId === null ? api.analyticsPlacements({ from: fromIso, keyIds: filters.keyId, poolIds: filters.poolId, to: toIso }) : Promise.resolve<PlacementRow[] | null>(sessionId === null ? null : [])),
    [fromIso, toIso, filters.keyId, filters.poolId, sessionId, tab, generation],
  );
  const budgets = useAsync(() => (tab === 'burn' ? api.analyticsBudgets() : Promise.resolve(null)), [tab]);
  // With no filter and no session the page's breakdown is the range's whole key list on its grouping.
  const plain = sessionId === null && filters.accountId.length === 0 && filters.keyId.length === 0 && filters.model.length === 0 && filters.poolId.length === 0 && classPattern.trim() === '' && userAgentPattern.trim() === '' && !includeProbes && agentGroups === undefined;
  const accountKeys = useDimensionKeys(fromIso, toIso, DIM_GROUP.accountId, generation, plain && groupBy === DIM_GROUP.accountId ? breakdown : undefined);
  const keyKeys = useDimensionKeys(fromIso, toIso, DIM_GROUP.keyId, generation, plain && groupBy === DIM_GROUP.keyId ? breakdown : undefined);
  const modelKeys = useDimensionKeys(fromIso, toIso, DIM_GROUP.model, generation, plain && groupBy === DIM_GROUP.model ? breakdown : undefined);
  const poolKeys = useDimensionKeys(fromIso, toIso, DIM_GROUP.poolId, generation, plain && groupBy === DIM_GROUP.poolId ? breakdown : undefined);
  const options = useMemo<Record<QueryDim, Option[]>>(
    () => ({ accountId: optionsOf(accountKeys.data), keyId: optionsOf(keyKeys.data), model: optionsOf(modelKeys.data), poolId: optionsOf(poolKeys.data) }),
    [accountKeys.data, keyKeys.data, modelKeys.data, poolKeys.data],
  );

  const rangeBucketMs = bucketSizeFor(span.toMs - span.fromMs);
  // Once a minute: a passed bucket boundary moves the end, so a page left open shows the hour it is in.
  useEffect(() => {
    const id = clock.setInterval(() => {
      if (Math.floor(clock.now() / rangeBucketMs) !== Math.floor(now / rangeBucketMs)) setNow(clock.now());
    }, 60_000);
    return () => clock.clearInterval(id);
  }, [now, rangeBucketMs]);
  // A read that failed or that still answers the filters before the change draws nothing, so no figure stands
  // under a filter it was not read for.
  const failure = series.error ?? breakdown.error ?? log.error;
  const stale = series.stale || breakdown.stale || log.stale;
  // An outage of the ledger fails the section and filter-choice reads with these three, and this is the one
  // control that reads again, so it reads everything the view draws from: the reads keyed on the window through the
  // moved end and the generation in one update, so none runs twice, and the two with no window by reloading.
  const retry = () => {
    setNow(clock.now());
    setGeneration((g) => g + 1);
    budgets.reload();
    burnSettings.reload();
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
  const speedPoints = useMemo(
    () => (sessionId === null ? (bySpeed.stale ? [] : (bySpeed.data ?? [])) : foldRequests(rows, rangeBucketMs, fromIso, toIso, 'speed', foldOptions)),
    [sessionId, bySpeed.stale, bySpeed.data, rows, rangeBucketMs, fromIso, toIso, foldOptions],
  );
  const effortGroups = useMemo(() => (sessionId === null ? (byEffort.stale ? [] : (byEffort.data ?? [])) : foldGroups(rows, 'effort', foldOptions)), [sessionId, byEffort.stale, byEffort.data, rows, foldOptions]);
  const totals = useMemo(() => {
    const summed = sumGroups(groups);
    return { ...summed, tokens: withPlainOutput(summed.tokens) };
  }, [groups]);
  // A session fold has no cost split and no warming facts; those figures read unavailable rather than zero.
  const split = knownSplit(totals.cost);
  const { keptWarm, savings } = totals;
  const otherFilters = filters.keyId.length > 0 || filters.poolId.length > 0 || filters.model.length > 0 || userAgentPattern.trim() !== '' || classPattern.trim() !== '' || sessionId !== null || includeProbes;
  // The burn, rewrite and fit routes read the range alone, so every filter of the bar passes them by.
  const unfiltered = otherFilters || filters.accountId.length > 0;

  const groupSlot = useMemo(() => {
    // Slots follow the range's whole key list so a filter never repaints the survivors; a dimension with no
    // option list of its own falls back to the keys the breakdown answered.
    const listed = groupBy === 'account' ? options.accountId : groupBy === 'key' ? options.keyId : groupBy === 'model' ? options.model : groupBy === 'pool' ? options.poolId : [];
    const keys = listed.length ? listed.map((o) => o.value) : groups.map((g) => g.key);
    return (key: string) => (groupBy === 'account' ? accountColor(ink, key) : ink.series[Math.min(7, keys.indexOf(key) < 0 ? 7 : keys.indexOf(key))]);
  }, [options, groups, groupBy, ink]);

  const axis = timeAxis(bucketMs, ink);
  // A failed read shows its error alone: a skeleton beside it would say a load is still running, and an empty
  // fold would read as zero traffic, so the sections it feeds stay out until a read succeeds.
  const shown = failure === null;
  const loading = shown && (stale || (sessionId === null ? series.data === null || breakdown.data === null : log.data === null));
  const empty = shown && !loading && groups.length === 0;
  const speedLoading = sessionId === null ? bySpeed.data === null || bySpeed.stale : loading;
  const effortLoading = sessionId === null ? byEffort.data === null || byEffort.stale : loading;
  const speedError = sessionId === null ? bySpeed.error : null;
  // Usage credits alone fund a fast answer, on a subscription account as on an API key.
  const fastSpend = speedPoints.reduce((n, p) => n + (p.groups.find((g) => g.key === 'fast')?.costUsd ?? 0), 0);
  const groupLabel = GROUP_LABEL[groupBy];

  return (
    <div className="stack">
      <div className="filters filters--sticky" ref={filtersRef}>
        <Segmented<Range>
          ariaLabel="Range"
          title={`Window ends ${fmtAbsolute(toIso)}`}
          value={range}
          onChange={changeRange}
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
            <Input type="datetime-local" value={custom.from} max={custom.to} onChange={(e) => changeCustom({ ...custom, from: e.target.value })} aria-label="From" />
            <span className="muted">to</span>
            <Input type="datetime-local" value={custom.to} min={custom.from} max={toLocalInput(now)} onChange={(e) => changeCustom({ ...custom, to: e.target.value })} aria-label="To" />
            {span.invalid && <span className="pairing__expired">End must be after start; showing the last 24h.</span>}
          </span>
        )}
        {!log.stale && log.data?.partial && <span className="secondary">Partial: the first {fmtInt(SESSION_PAGE_LIMIT * SESSION_PAGE_SIZE)} requests of the range</span>}
        <span className="filters__sep" aria-hidden />
        <label className="row">
          <span className="muted">Group by</span>
          <Select value={groupBy} onChange={(e) => changeGroupBy(e.target.value as GroupBy)} className="groupby-select">
            {(Object.keys(GROUP_LABEL) as GroupBy[]).map((g) => (
              <option key={g} value={g}>
                {GROUP_LABEL[g]}
              </option>
            ))}
          </Select>
        </label>
        {DIMS.map((dim) => (
          <MultiSelect key={dim} label={DIM_LABEL[dim]} options={options[dim]} value={filters[dim]} onChange={(v) => changeFilter(dim, v)} mono />
        ))}
        <PatternInput label="Client pattern" value={classPattern} onCommit={commitClass} />
        <PatternInput label="User agent pattern" value={userAgentPattern} onCommit={commitAgent} />
        {groupBy === 'user_agent' && (
          <>
            <textarea
              value={groupsText.draft}
              maxLength={MAX_PATTERNS * (MAX_PATTERN_CHARS + 1)}
              onBlur={groupsText.onBlur}
              onChange={(e) => {
                const next = boundPatterns(e.target.value);
                groupsText.onChange(next.text);
                setGroupsCut(next.cut);
              }}
              placeholder={'User agent groups, one pattern per line\nclaude-cli/2.1.*\nclaude-cli/2.0.*'}
              aria-label="User agent groups"
              title="Each pattern names a group; every other user agent groups under other"
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
          <Toggle checked={includeProbes} onChange={changeProbes} label="Include probes" />
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

      <SectionBar sections={SECTIONS} shown={tab} onChoose={chooseTab} top={TOPBAR_HEIGHT + filtersHeight} />

      {failure !== null && (
        <ErrorState
          title="Could not load the analytics for these filters"
          body={failure}
          action={
            <Button size="sm" onClick={retry}>
              Try again
            </Button>
          }
        />
      )}
      {empty && <div className="empty">No traffic in this range.</div>}

      {tab === 'traffic' && (
        <Section id="traffic" shown={shown} title="Traffic" meta={loading ? undefined : `${fmtInt(totals.requests)} requests, ${fmtPct(errorRateOf(totals), 1)} errors`}>
          <div className="grid-2">
            <ChartCard title="Requests by outcome" subtitle="Per bucket, stacked" loading={loading} legend={OUTCOME_SERIES.map((s) => ({ color: ink.series[s.slot], label: s.label, value: fmtInt(totals[s.key]) }))}>
              <BarChart data={buckets} margin={{ top: 8, right: 8, bottom: 0, left: 0 }} barCategoryGap={2}>
                <CartesianGrid vertical={false} stroke={ink.grid} />
                <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
                <YAxis tickLine={false} axisLine={false} width={36} allowDecimals={false} />
                <Tooltip cursor={{ fill: ink.grid, opacity: 0.6 }} content={timeTip<Bucket>((p) => OUTCOME_SERIES.map((s) => ({ color: ink.series[s.slot], label: s.label, value: fmtInt(p[s.key]) })))} />
                {OUTCOME_SERIES.map((s, i) => (
                  <Bar key={s.key} dataKey={s.key} stackId="a" fill={ink.series[s.slot]} stroke={ink.surface} strokeWidth={1} maxBarSize={22} radius={i === OUTCOME_SERIES.length - 1 ? [3, 3, 0, 0] : 0} isAnimationActive={false} />
                ))}
              </BarChart>
            </ChartCard>

            <Card title="Errors by code" subtitle="Non-200 responses in range">
              {loading ? <Skeleton height={90} /> : <ErrorBars errors={totals.errors} color={ink.series[7]} />}
            </Card>

            {/* Failovers and the rewrites share a row, so what a move cost the cache sits beside the failovers. */}
            <ChartCard title="Failovers" subtitle="Attempts that moved to another member" loading={loading} legend={[{ color: ink.series[3], label: 'Failovers', shape: 'line', value: fmtInt(totals.failovers) }]}>
              <LineChart data={buckets} margin={{ top: 8, right: 8, bottom: 0, left: 0 }}>
                <CartesianGrid vertical={false} stroke={ink.grid} />
                <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
                <YAxis tickLine={false} axisLine={false} width={36} allowDecimals={false} />
                <Tooltip cursor={{ stroke: ink.axis }} content={timeTip<Bucket>((p) => [{ color: ink.series[3], label: 'failovers', value: fmtInt(p.failovers) }])} />
                <Line type="monotone" dataKey="failovers" stroke={ink.series[3]} strokeWidth={2} dot={false} isAnimationActive={false} />
              </LineChart>
            </ChartCard>

            <RewritesCard answer={rewrites.stale ? null : rewrites.data} error={rewrites.error} loading={rewrites.data === null || rewrites.stale} axis={axis} unfiltered={unfiltered} />

            <SpeedCard points={speedPoints} error={speedError} loading={speedLoading} axis={axis} />

            <Card title="Effort levels" subtitle="Requests per effort level in range">
              {sessionId === null && byEffort.error !== null ? (
                <ErrorState title="Could not load effort levels" body={byEffort.error} />
              ) : effortLoading ? (
                <Skeleton height={90} />
              ) : (
                <EffortBars groups={effortGroups} color={ink.series[2]} />
              )}
            </Card>
          </div>
          <BreakdownTable
            title={`Traffic by ${groupLabel.toLowerCase()}`}
            loading={loading}
            groupLabel={groupLabel}
            columns={['Requests', 'OK', 'Rate limited', 'Failed', 'Incomplete', 'Exhausted', 'Rejected', 'Error rate', 'Failovers', 'Search', 'Fetch']}
            rows={groups.map((g) => ({
              key: g.key,
              label: g.label,
              // Search and Fetch count the server-tool calls the group's answers made, not tokens.
              cells: [fmtInt(g.requests), fmtInt(g.succeeded), fmtInt(g.rateLimited), fmtInt(g.failed), fmtInt(g.incomplete), fmtInt(g.exhausted), fmtInt(g.rejected), fmtPct(errorRateOf(g), 1), fmtInt(g.failovers), fmtInt(g.tokens.webSearch), fmtInt(g.tokens.webFetch)],
            }))}
          />
        </Section>
      )}

      {tab === 'burn' && (
        <Section id="burn" shown={shown} title="Burn" meta={`Tokens, spend and plan share over the range${unfiltered ? UNFILTERED_NOTE : ''}`}>
          <div className="row analytics-groupby">
            <span className="muted">Group by</span>
            <Select value={burnGroup} onChange={(e) => changeBurnGroup(e.target.value as BurnGroup)} aria-label="Burn group by" className="groupby-select">
              {BURN_GROUPS.map((g) => (
                <option key={g} value={g}>
                  {BURN_GROUP_LABEL[g]}
                </option>
              ))}
            </Select>
            <span className="muted analytics-groupby__reason">Burn groups by account, key, session or request class alone; the plan shares read under Account, since shares of different accounts&apos; windows do not add.</span>
          </div>
          {/* Only the account grouping sizes its lines by tier, so another one neither waits on nor fails with the settings read. */}
          <BurnCharts
            answer={burn.stale ? null : (burn.data?.answer ?? null)}
            costs={burnSettings.data?.subscriptionCosts ?? null}
            error={burn.error ?? (burnGroup === 'account' ? burnSettings.error : null)}
            groupBy={burnGroup}
            loading={burn.data === null || burn.stale || (burnGroup === 'account' && sized === null)}
            rangeEndMs={burn.data?.endMs}
            readAt={burn.data?.readAt}
            sized={sized}
          />
          <BudgetTable budgets={budgets.data} error={budgets.error} now={now} />
        </Section>
      )}

      {tab === 'latency' && (
        <Section id="latency" shown={shown} title="Latency" meta="Percentiles read from merged per-hour histograms">
          <div className="stack">
            {LATENCY_METRICS.map((m) => (
              <LatencyRow key={m.key} metric={m} series={buckets} totals={totals} loading={loading} axis={axis} />
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
      )}

      {tab === 'cache' && (
        <Section id="cache" shown={shown} title="Cache" meta={loading ? undefined : `${fmtPct(hitRatioOf(totals) ?? 0)} hit ratio, ${fmtInt(totals.expiredBetweenTurns)} expired between turns, ${fmtPct(cachedShareOf(totals) ?? 0)} of input read from cache`}>
          <div className="grid-2">
            <ChartCard title="Tokens" subtitle="Per bucket, stacked by kind" loading={loading} legend={TOKEN_SERIES.map((s) => ({ color: ink.series[s.slot], label: s.label, value: fmtCompact(totals.tokens[s.key]) }))}>
              <BarChart data={buckets} margin={{ top: 8, right: 8, bottom: 0, left: 0 }} barCategoryGap={2}>
                <CartesianGrid vertical={false} stroke={ink.grid} />
                <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
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
                <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
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
                <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
                <YAxis {...usdAxis(buckets.flatMap((b) => [b.cost.cacheWrite ?? 0, b.cost.cacheRead ?? 0]))} tickLine={false} axisLine={false} />
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
          {/* The route reads no session, so a session view has no placements of its own to show. */}
          {sessionId === null && (
            <PlacementTable
              error={placements.error}
              rows={placements.stale ? null : placements.data}
              unfiltered={filters.accountId.length > 0 || filters.model.length > 0 || userAgentPattern.trim() !== '' || classPattern.trim() !== ''}
            />
          )}
        </Section>
      )}

      {tab === 'cost' && (
        <Section id="cost" shown={shown} title="Cost" meta={loading ? undefined : `${fmtUsd(totals.costUsd)} in range`}>
          <div className="tiles-row">
            <Tile label="Estimated cost" value={loading ? null : fmtUsd(totals.costUsd)} note="at the effective price list" />
            <Tile label="Cost per request" value={loading ? null : fmtUsd(totals.requests ? totals.costUsd / totals.requests : 0)} note={`${fmtInt(totals.requests)} requests`} />
            <Tile label="Cost per 1k output tokens" value={loading ? null : fmtUsd(totals.tokens.output ? (totals.costUsd / totals.tokens.output) * 1000 : 0)} note={`${fmtCompact(totals.tokens.output)} output tokens`} />
            <Tile label="Fast mode spend" value={speedError !== null ? '-' : speedLoading ? null : fmtUsd(fastSpend)} note="usage credits, at the fast cells" />
          </div>
          <div className="grid-2">
            <CostByGroup points={points} groupBy={groupBy} groupSlot={groupSlot} loading={loading} axis={axis} />
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
      )}

      {tab === 'cache-warming' && (
        <Section id="cache-warming" shown={shown} title="Cache warming" meta={sessionId === null ? 'Probe rows are always counted here, whatever the toggle above says' : SESSION_SOURCE_NOTE}>
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
                <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
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
                <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
                <YAxis {...usdAxis(buckets.flatMap((b) => (b.savings === null ? [] : [b.savings.grossUsd, b.savings.probeCostUsd, b.savings.grossUsd - b.savings.probeCostUsd])))} tickLine={false} axisLine={false} />
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
                <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
                <YAxis tickLine={false} axisLine={false} width={36} allowDecimals={false} />
                <Tooltip cursor={{ fill: ink.grid, opacity: 0.6 }} content={timeTip<Bucket>((p) => [{ color: ink.series[0], label: 'kept-warm requests', value: keptWarm === null || p.keptWarm === null ? '-' : fmtInt(p.keptWarm.requests) }])} />
                {keptWarm !== null && <Bar dataKey="keptWarm.requests" fill={ink.series[0]} maxBarSize={22} radius={[3, 3, 0, 0]} isAnimationActive={false} />}
              </BarChart>
            </ChartCard>

            <ChartCard title="Kept-warm tokens" subtitle="Cache-read tokens on those requests" loading={loading} legend={[{ color: ink.series[2], label: 'Kept-warm tokens', value: keptWarm === null ? '-' : fmtCompact(keptWarm.tokens) }]}>
              <BarChart data={buckets} margin={{ top: 8, right: 8, bottom: 0, left: 0 }} barCategoryGap={2}>
                <CartesianGrid vertical={false} stroke={ink.grid} />
                <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
                <YAxis tickFormatter={(v: number) => fmtCompact(v)} tickLine={false} axisLine={false} width={44} />
                <Tooltip cursor={{ fill: ink.grid, opacity: 0.6 }} content={timeTip<Bucket>((p) => [{ color: ink.series[2], label: 'kept-warm tokens', value: keptWarm === null || p.keptWarm === null ? '-' : fmtCompact(p.keptWarm.tokens) }])} />
                {keptWarm !== null && <Bar dataKey="keptWarm.tokens" fill={ink.series[2]} maxBarSize={22} radius={[3, 3, 0, 0]} isAnimationActive={false} />}
              </BarChart>
            </ChartCard>
          </div>
          <KeepaliveByPool groups={poolGroups} error={sessionId === null ? byPool.error : null} loading={sessionId === null ? byPool.data === null || byPool.stale : loading} />
        </Section>
      )}

      {tab === 'accounts' && (
        <Section id="accounts" shown={shown} title="Accounts" meta="Peak utilization, ineligible time, and pin churn per account">
          <AccountsSection
            accountIds={filters.accountId}
            bucketMs={bucketMs}
            error={accountRollups.error}
            loading={accountRollups.data === null || accountRollups.stale}
            otherFilters={otherFilters}
            rollups={accountRollups.stale ? null : accountRollups.data}
            span={span}
            axis={axis}
          />
        </Section>
      )}
    </div>
  );
}

const pct100 = (v: number | null) => (v === null ? null : v * 100);

/** Height of the shell's sticky topbar; every bar the page sticks rides under it. */
const TOPBAR_HEIGHT = 52;

/** Live border-box height of a sticky bar, so the bar below it can dock against it. */
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
  { id: 'burn', label: 'Burn' },
  { id: 'latency', label: 'Latency' },
  { id: 'cache', label: 'Cache' },
  { id: 'cost', label: 'Cost' },
  { id: 'cache-warming', label: 'Cache warming' },
  { id: 'accounts', label: 'Accounts' },
] as const;
type SectionId = (typeof SECTIONS)[number]['id'];

/** The element the page scrolls; jsdom implements no `scrollTo` on it, so a caller guards the call. */
const scroller = (): Element | null => document.scrollingElement;

function Section({ id, title, meta, shown = true, children }: { children: ReactNode; id: SectionId; meta?: string; shown?: boolean; title: string }) {
  if (!shown) return null;
  return (
    <section className="analytics-section" id={id} role="tabpanel" aria-labelledby={`tab-${id}`}>
      <header className="analytics-section__head">
        <h2 className="analytics-section__title">{title}</h2>
        {meta && <span className="analytics-section__meta">{meta}</span>}
      </header>
      {children}
    </section>
  );
}

function ChartCard({
  title,
  subtitle,
  loading,
  children,
  empty,
  error = null,
  footer,
  legend,
  height = 220,
}: {
  children: ReactNode;
  /** Drawn in place of the chart, legend and all, when the answer holds nothing the chart could draw. */
  empty?: ReactNode;
  /** A card reading a route of its own draws that route's failure in place of the chart. */
  error?: string | null;
  footer?: ReactNode;
  height?: number;
  legend?: { color: string; dashed?: boolean; label: string; shape?: 'line' | 'rect'; title?: string; value?: ReactNode }[];
  loading: boolean;
  subtitle?: ReactNode;
  title: string;
}) {
  return (
    <Card title={title} subtitle={subtitle}>
      {error !== null ? (
        <ErrorState title={`Could not load ${title.toLowerCase()}`} body={error} />
      ) : loading ? (
        <Skeleton height={height} />
      ) : empty ? (
        <p className="muted">{empty}</p>
      ) : (
        <>
          <ResponsiveContainer width="100%" height={height}>
            {children as never}
          </ResponsiveContainer>
          {legend && <Legend items={legend} />}
          {footer}
        </>
      )}
    </Card>
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

/** Horizontal bars of counts in the order given, each with its share of the total; `empty` when every count is zero. */
function CountBars({ color, empty, rows }: { color: string; empty: string; rows: { count: number; label: string }[] }) {
  const max = Math.max(1, ...rows.map((r) => r.count));
  const total = rows.reduce((s, r) => s + r.count, 0);
  if (total === 0) return <p className="muted">{empty}</p>;
  return (
    <ul className="hbars hbars--errors">
      {rows.map((r) => (
        <li key={r.label}>
          <span className="mono">{r.label}</span>
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

function ErrorBars({ errors, color }: { color: string; errors: Record<string, number> }) {
  const rows = Object.entries(errors)
    .map(([label, count]) => ({ count, label }))
    .sort((a, b) => b.count - a.count || a.label.localeCompare(b.label));
  return <CountBars color={color} empty="No errors in range." rows={rows} />;
}

/** Requests per effort level, each named as the rollups file it; `none`, the requests that asked for no level, last. */
function EffortBars({ groups, color }: { color: string; groups: AnalyticsGroup[] }) {
  const rows = groups
    .filter((g) => g.requests > 0)
    .map((g) => ({ count: g.requests, label: g.key }))
    .sort((a, b) => Number(a.label === 'none') - Number(b.label === 'none') || b.count - a.count || a.label.localeCompare(b.label));
  return <CountBars color={color} empty="No requests in range." rows={rows} />;
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
    .map((reason) => ({ count: missCount(totals.cacheMissReasons, reason), reason }))
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
  "Probes no real request came back for, settled when the recipe stopped, lapsed or was evicted, or when the request after them read no cache. A high idle-limit share means the pool's max idle time is set too high for how these sessions are used.";

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
        cells: [...order.map((reason) => fmtInt(missCount(g.cacheMissReasons, reason))), fmtInt(g.expiredBetweenTurns)],
      }))}
    />
  );
}

function LatencyRow({ axis, metric, series, totals, loading }: { axis: TimeAxis; loading: boolean; metric: (typeof LATENCY_METRICS)[number]; series: Bucket[]; totals: AnalyticsGroup }) {
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
          <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
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
      columns={['Probes OK', 'Held', 'Skipped', 'Failed', 'Kept-warm requests', 'Kept-warm tokens', 'Gross saved', 'Probe cost', 'Net saved']}
      rows={rows.map((g) => ({
        key: g.key,
        label: g.label,
        // A group folded from request rows measured none of these.
        cells: [
          g.fromRequests ? '-' : fmtInt(g.probes.ok),
          g.fromRequests ? '-' : fmtInt(g.probes.held),
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

function CostByGroup({ axis, points, groupBy, groupSlot, loading }: { axis: TimeAxis; groupBy: GroupBy; groupSlot: (key: string) => string; loading: boolean; points: AnalyticsPoint[] }) {
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
        <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
        <YAxis {...usdAxis(data.map((row) => keys.reduce((n, k) => n + row[k], 0)))} tickLine={false} axisLine={false} />
        <Tooltip cursor={{ stroke: ink.axis }} content={timeTip<Record<string, number> & { at: number }>((p) => keys.map((k) => ({ color: color(k), label: label(k), value: fmtUsd(p[k] ?? 0) })))} />
        {keys.map((k) => (
          <Area key={k} type="monotone" dataKey={k} stackId="a" stroke={color(k)} strokeWidth={1.5} fill={color(k)} fillOpacity={0.35} dot={false} isAnimationActive={false} />
        ))}
      </AreaChart>
    </ChartCard>
  );
}

const UNFILTERED_NOTE = '; the filters above do not apply here';
type TimeAxis = { height?: number; label?: { fill: string; fontSize: number; position: 'insideBottomRight'; value: string }; tickFormatter: (v: number) => string };
/**
 * A time axis over buckets `bucketMs` wide. The 6-hour and daily buckets are UTC multiples, so their day ticks print
 * the UTC date and the axis says so once; hour ticks stay local, as the tooltips do.
 */
function timeAxis(bucketMs: number, ink: ChartInk): TimeAxis {
  if (bucketMs < 6 * HOUR) return { tickFormatter: (v) => fmtHourTick(v) };
  // The label sits under the last tick, so the axis grows a line taller than its 30 px to keep the two apart.
  return { height: 40, label: { fill: ink.ink2, fontSize: 11, position: 'insideBottomRight', value: 'UTC days' }, tickFormatter: (v) => fmtDayShort(v, 'UTC') };
}

type SpeedRow = { at: number; fast: number; standard: number };
const requestsIn = (p: AnalyticsPoint, key: string) => p.groups.find((g) => g.key === key)?.requests ?? 0;

/**
 * Requests per bucket by the speed their answers reported, else `fast` for a request that asked for it, stacked as
 * Requests by outcome stacks its outcomes.
 */
function SpeedCard({ axis, points, error, loading }: { axis: TimeAxis; error: string | null; loading: boolean; points: AnalyticsPoint[] }) {
  const ink = useChartInk();
  const data = useMemo(() => points.map((p): SpeedRow => ({ at: Date.parse(p.at), fast: requestsIn(p, 'fast'), standard: requestsIn(p, 'standard') })), [points]);
  return (
    <ChartCard
      title="Requests by speed"
      subtitle="Standard and fast per bucket, stacked"
      error={error}
      loading={loading}
      legend={SPEED_SERIES.map((s) => ({ color: ink.series[s.slot], label: s.label, value: fmtInt(data.reduce((n, d) => n + d[s.key], 0)) }))}
    >
      <BarChart data={data} margin={{ top: 8, right: 8, bottom: 0, left: 0 }} barCategoryGap={2}>
        <CartesianGrid vertical={false} stroke={ink.grid} />
        <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
        <YAxis tickLine={false} axisLine={false} width={36} allowDecimals={false} />
        <Tooltip cursor={{ fill: ink.grid, opacity: 0.6 }} content={timeTip<SpeedRow>((p) => SPEED_SERIES.map((s) => ({ color: ink.series[s.slot], label: s.label, value: fmtInt(p[s.key]) })))} />
        {SPEED_SERIES.map((s, i) => (
          <Bar key={s.key} dataKey={s.key} stackId="a" fill={ink.series[s.slot]} stroke={ink.surface} strokeWidth={1} maxBarSize={22} radius={i === SPEED_SERIES.length - 1 ? [3, 3, 0, 0] : 0} isAnimationActive={false} />
        ))}
      </BarChart>
    </ChartCard>
  );
}

function RewritesCard({ answer, axis, error, loading, unfiltered }: { answer: { moves: Record<string, number>; points: RewritePoint[] } | null; axis: TimeAxis; error: string | null; loading: boolean; unfiltered: boolean }) {
  const ink = useChartInk();
  const data = useMemo(() => (answer?.points ?? []).map((p) => ({ at: Date.parse(p.at), rewriteTokens: p.rewriteTokens })), [answer]);
  const total = data.reduce((n, p) => n + p.rewriteTokens, 0);
  const moves = Object.entries(answer?.moves ?? {}).sort(([a], [b]) => a.localeCompare(b));
  return (
    <ChartCard
      title="Migration rewrites"
      subtitle={`Cache-write tokens the first request after a move wrote, per bucket${unfiltered ? UNFILTERED_NOTE : ''}`}
      error={error}
      loading={loading}
      legend={[{ color: ink.series[1], label: 'Rewrite tokens', value: fmtCompact(total) }]}
      footer={
        <ul className="legend" aria-label="Moves by reason">
          <li className="muted">Moves</li>
          {moves.length === 0 ? (
            <li className="muted">none in range</li>
          ) : (
            moves.map(([reason, n]) => (
              <li key={reason}>
                {upperFirst(moveLabel(reason))}
                <span className="legend__value num">{fmtInt(n)}</span>
              </li>
            ))
          )}
        </ul>
      }
    >
      <BarChart data={data} margin={{ top: 8, right: 8, bottom: 0, left: 0 }} barCategoryGap={2}>
        <CartesianGrid vertical={false} stroke={ink.grid} />
        <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
        <YAxis tickFormatter={(v: number) => fmtCompact(v)} tickLine={false} axisLine={false} width={44} />
        <Tooltip cursor={{ fill: ink.grid, opacity: 0.6 }} content={timeTip<{ at: number; rewriteTokens: number }>((p) => [{ color: ink.series[1], label: 'rewrite tokens', value: fmtCompact(p.rewriteTokens) }])} />
        <Bar dataKey="rewriteTokens" fill={ink.series[1]} maxBarSize={22} radius={[3, 3, 0, 0]} isAnimationActive={false} />
      </BarChart>
    </ChartCard>
  );
}

const CONVERSATION_HINT = CONVERSATION_CLASSES.join(', ');
const OVERHEAD_HINT = 'every other class, and requests that named none';
const CLASS_NOTE = `conversation is ${CONVERSATION_HINT}; overhead is ${OVERHEAD_HINT}`;

const SPEND_NOTE = "Dollars per minute: API-key traffic, and a subscription account's fast-mode answers; its plan quota spends window share, charted under Plan per hour";
/** A folded rest of subscription accounts as the dollars chart names it: its dollars are its members' fast-mode
 *  answers, and its own label names the plan unit, which the dollars axis does not chart. */
const SPEND_REST_LABEL: Record<Exclude<BurnLineUnit, 'usd'>, string> = { unknown: 'Other (fast mode, tier unknown)', window_share: 'Other (fast mode)' };
const PLAN_NOTE =
  "The share of each subscription account's own 5-hour window (solid) and 7-day window (dashed) its traffic used per hour, the 5-hour share weighed by the peak band, whose hours are shaded";
const TIER_UNKNOWN_HINT = 'No tier on file, or one the cost model does not size, so no share of its windows is priced';
const MODEL_UNLISTED_HINT = 'A bucket whose traffic ran on a model the cost model has no weight for, or on a tier it has no capacity for on that window, prices no share there and draws no point';
const NO_ACCOUNT: ReadonlySet<string> = new Set();
const PARTIAL_NOTE = 'The last bucket is partial, rated over the part of its width it covers';
/** A partial bucket's bars draw at this share of their series' opacity. */
const PARTIAL_TONE = 0.5;

/**
 * A plan line's dot, drawn on the slots `isolatedSlots` names alone: the line leaves its null slots unconnected, so a
 * share with a null or the series' end on both sides draws no segment of its own.
 */
const isolatedDot = (values: (number | null)[], color: string) => {
  const isolated = isolatedSlots(values);
  return ({ cx, cy, index, key }: { cx?: number; cy?: number; index: number; key?: string }) =>
    isolated[index] ? <Dot key={key} className="recharts-line-dot" cx={cx} cy={cy} r={3} fill={color} stroke={color} /> : <g key={key} />;
};

/**
 * The burn route's three views of one grouping. A line's color follows its group within the answer rather than
 * its rank, so a range that reorders the groups does not repaint them; the account grouping keeps each account's
 * page-wide color.
 */
function BurnCharts({
  answer,
  costs,
  error,
  groupBy,
  loading,
  rangeEndMs,
  readAt,
  sized,
}: {
  answer: { bucketSeconds: number; points: BurnPoint[] } | null;
  costs: SubscriptionCosts | null;
  error: string | null;
  groupBy: BurnGroup;
  loading: boolean;
  rangeEndMs: number | undefined;
  readAt: number | undefined;
  sized: ReadonlySet<string> | null;
}) {
  const ink = useChartInk();
  const { lines, rows } = useMemo(
    () => (answer === null ? { lines: [] as BurnLine[], rows: [] as BurnRow[] } : foldBurn(answer.points, groupBy, answer.bucketSeconds, sized ?? NO_ACCOUNT, MAX_BURN_LINES, rangeEndMs, readAt)),
    [answer, groupBy, rangeEndMs, readAt, sized],
  );
  const named = useMemo(() => lines.filter((l) => !l.other).map((l) => l.key).sort(), [lines]);
  const colorOf = (l: BurnLine) => (l.other ? ink.muted : groupBy === 'account' ? accountColor(ink, l.key) : ink.series[Math.min(7, named.indexOf(l.key))]);
  // A session id runs past any legend, so it shows cut the way the session filter's chip cuts it, whole on hover.
  const nameOf = (l: BurnLine) => (groupBy === 'session' && !l.other ? shortId(l.label, 13) : l.label);
  const titleOf = (l: BurnLine) => (groupBy === 'request_class' ? (l.key === 'conversation' ? CONVERSATION_HINT : OVERHEAD_HINT) : groupBy === 'session' && !l.other ? l.label : undefined);
  const axis = timeAxis((answer?.bucketSeconds ?? 3_600) * 1_000, ink);
  const label = BURN_GROUP_LABEL[groupBy].toLowerCase();
  // Read off the buckets, not the lines: the class grouping draws its two lines even over a range with no traffic.
  const empty = !loading && error === null && rows.length === 0 ? 'No traffic in this range.' : undefined;
  const byAccount = groupBy === 'account';
  // Every other grouping mixes both kinds of account, so its lines chart the dollars their API-key traffic and
  // fast-mode answers spent; under the account grouping a subscription account, or a folded rest of them, draws a
  // dollar line only for the usage credits its fast-mode answers spent.
  const spent = lines.map((l, i) => ({ i, l })).filter(({ l }) => !byAccount || l.unit === 'usd' || l.usdPerMinute > 0);
  const spendNameOf = (l: BurnLine) => (l.other && l.unit !== 'usd' ? SPEND_REST_LABEL[l.unit] : nameOf(l));
  const planned = lines.map((l, i) => ({ i, l })).filter(({ l }) => byAccount && !l.other && l.unit === 'window_share');
  const unpriced = lines.filter((l) => byAccount && !l.other && l.unit === 'unknown');
  const shareOf = (share: number | null) => (share === null ? '-' : fmtShare(share));
  const partial = rows[rows.length - 1]?.partial === true;
  // The band's own hours over the instants the plan chart spans: the buckets are UTC multiples whose starts fall off
  // the band's edges, so shading whole buckets would mark the wrong hours.
  const band = useMemo(() => (costs === null || !byAccount || rows.length < 2 ? [] : bandRuns(costs, rows[0].at, rows[rows.length - 1].at)), [byAccount, costs, rows]);
  const partialCells = (opacity: number) => rows.map((r) => <Cell key={r.at} {...(r.partial ? { className: 'is-partial', fillOpacity: opacity * PARTIAL_TONE } : {})} />);
  // A bucket too short to set the scale would stretch the axis over a few seconds' rate, so the axis stops at the
  // buckets that set it and the short one runs past the top.
  const scaled = rows.filter((r) => r.setsScale);
  const scaleTo = (values: number[]) => {
    const top = Math.max(0, ...values);
    return scaled.length > 0 && scaled.length < rows.length && top > 0 ? { allowDataOverflow: true, domain: [0, niceAxisTop(top)] as [number, number] } : {};
  };
  const sum = (values: number[]) => values.reduce((n, v) => n + v, 0);
  const spentValues = scaled.flatMap((r) => spent.map(({ i }) => r.usd[i]));
  const spentScale = scaleTo(spentValues);
  // Without a capped domain recharts spans every row, so the labels' precision reads every row too.
  const spentAxis = usdAxis('domain' in spentScale ? spentValues : rows.flatMap((r) => spent.map(({ i }) => r.usd[i])));
  return (
    <div className="grid-2">
      <ChartCard
        title="Tokens per minute"
        subtitle={`Uncached input and cache reads per minute, stacked by ${label}; the fainter part of each is the cache read${groupBy === 'request_class' ? `. ${upperFirst(CLASS_NOTE)}` : ''}${partial ? `. ${PARTIAL_NOTE}, and drawn lighter` : ''}`}
        error={error}
        loading={loading}
        empty={empty}
        legend={lines.map((l) => ({ color: colorOf(l), label: nameOf(l), title: titleOf(l), value: fmtCompact(l.tokensPerMinute) }))}
      >
        <BarChart data={rows} margin={{ top: 8, right: 8, bottom: 0, left: 0 }} barCategoryGap={2}>
          <CartesianGrid vertical={false} stroke={ink.grid} />
          <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
          <YAxis tickFormatter={(v: number) => fmtCompact(v)} tickLine={false} axisLine={false} width={44} {...scaleTo(scaled.map((r) => sum(r.uncached) + sum(r.cached)))} />
          <Tooltip
            cursor={{ fill: ink.grid, opacity: 0.6 }}
            content={timeTip<BurnRow>((r) => [
              ...lines.map((l, i) => ({ color: colorOf(l), label: nameOf(l), title: titleOf(l), value: `${fmtCompact(r.uncached[i])} uncached, ${fmtCompact(r.cached[i])} cached` })),
              { label: 'total', value: fmtCompact(r.uncached.reduce((n, v) => n + v, 0) + r.cached.reduce((n, v) => n + v, 0)) },
            ])}
          />
          {lines.flatMap((l, i) => [
            <Bar key={`${i}-uncached`} dataKey={(r: BurnRow) => r.uncached[i]} stackId="a" fill={colorOf(l)} stroke={ink.surface} strokeWidth={1} maxBarSize={22} isAnimationActive={false}>
              {partialCells(1)}
            </Bar>,
            <Bar key={`${i}-cached`} dataKey={(r: BurnRow) => r.cached[i]} stackId="a" fill={colorOf(l)} fillOpacity={0.45} stroke={ink.surface} strokeWidth={1} maxBarSize={22} radius={i === lines.length - 1 ? [3, 3, 0, 0] : 0} isAnimationActive={false}>
              {partialCells(0.45)}
            </Bar>,
          ])}
        </BarChart>
      </ChartCard>

      <ChartCard
        title="Spend per minute"
        subtitle={`${byAccount ? SPEND_NOTE : `${SPEND_NOTE} when grouped by account`}${partial ? `. ${PARTIAL_NOTE}` : ''}`}
        error={error}
        loading={loading}
        empty={empty ?? (spent.length === 0 ? 'No account spent dollars in this range.' : undefined)}
        legend={spent.map(({ l }) => ({ color: colorOf(l), label: spendNameOf(l), shape: 'line' as const, title: titleOf(l), value: fmtUsd(l.usdPerMinute) }))}
      >
        <LineChart data={rows} margin={{ top: 8, right: 8, bottom: 0, left: 0 }}>
          <CartesianGrid vertical={false} stroke={ink.grid} />
          <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
          <YAxis {...spentAxis} tickLine={false} axisLine={false} {...spentScale} />
          <Tooltip cursor={{ stroke: ink.axis }} content={timeTip<BurnRow>((r) => spent.map(({ i, l }) => ({ color: colorOf(l), label: spendNameOf(l), title: titleOf(l), value: fmtUsd(r.usd[i]) })))} />
          {spent.map(({ i, l }) => (
            <Line key={i} type="monotone" dataKey={(r: BurnRow) => r.usd[i]} stroke={colorOf(l)} strokeWidth={2} dot={false} isAnimationActive={false} />
          ))}
        </LineChart>
      </ChartCard>

      {byAccount && (
        <ChartCard
          title="Plan per hour"
          subtitle={
            <>
              {PLAN_NOTE}.{partial ? ` ${PARTIAL_NOTE}.` : ''} <CostReferenceLink tab="tiers" />
            </>
          }
          error={error}
          loading={loading}
          empty={
            empty ??
            (planned.length > 0
              ? undefined
              : unpriced.length > 0
                ? `No subscription account in range has a tier the cost model sizes: ${unpriced.map(nameOf).join(', ')}.`
                : 'No subscription account burned in this range.')
          }
          legend={[
            ...planned.flatMap(({ l }) => [
              {
                color: colorOf(l),
                label: `${nameOf(l)} (5-hour)`,
                shape: 'line' as const,
                title: l.fiveHourPeak === null ? undefined : `Highest hour ${fmtShare(l.fiveHourPeak)}`,
                value: shareOf(l.fiveHourPerHour),
              },
              {
                color: colorOf(l),
                dashed: true,
                label: `${nameOf(l)} (7-day)`,
                shape: 'line' as const,
                value: shareOf(l.sevenDayPerHour),
              },
            ]),
            // One entry for every share the cost model cannot price, whichever account and window it fell on; muted,
            // since it draws no line of its own.
            ...(planned.some(({ l }) => l.unpriced.fiveHour || l.unpriced.sevenDay)
              ? [
                  {
                    color: ink.muted,
                    label: 'model unlisted',
                    shape: 'line' as const,
                    title: MODEL_UNLISTED_HINT,
                    value: [planned.some(({ l }) => l.unpriced.fiveHour) ? '5-hour' : null, planned.some(({ l }) => l.unpriced.sevenDay) ? '7-day' : null].filter((w) => w !== null).join(', '),
                  },
                ]
              : []),
            // Muted, since no line of its own is drawn.
            ...unpriced.map((l) => ({ color: ink.muted, label: nameOf(l), shape: 'line' as const, title: TIER_UNKNOWN_HINT, value: 'tier unknown' })),
          ]}
        >
          <LineChart data={rows} margin={{ top: 8, right: 8, bottom: 0, left: 0 }}>
            <CartesianGrid vertical={false} stroke={ink.grid} />
            {/* A category axis places a reference area on its categories alone, so this one counts instants and ticks on the buckets. */}
            <XAxis dataKey="at" type="number" scale="time" domain={['dataMin', 'dataMax']} ticks={rows.map((r) => r.at)} {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
            <YAxis tickFormatter={(v: number) => fmtShare(v)} tickLine={false} axisLine={false} width={52} {...scaleTo(scaled.flatMap((r) => planned.flatMap(({ i }) => [r.fiveHour[i] ?? 0, r.sevenDay[i] ?? 0])))} />
            {/* Recharts drops null entries by default, and a bucket whose every share is null would lose its date, its dashes and the unpriced hint. */}
            <Tooltip
              cursor={{ stroke: ink.axis }}
              filterNull={false}
              content={timeTip<BurnRow>((r) => [
                ...planned.flatMap(({ i, l }) => [
                  { color: colorOf(l), label: `${nameOf(l)} (5-hour)`, title: slotUnpriced(r.fiveHour[i], r.tokens[i]) ? MODEL_UNLISTED_HINT : undefined, value: shareOf(r.fiveHour[i]) },
                  { color: colorOf(l), label: `${nameOf(l)} (7-day)`, title: slotUnpriced(r.sevenDay[i], r.tokens[i]) ? MODEL_UNLISTED_HINT : undefined, value: shareOf(r.sevenDay[i]) },
                ]),
              ])}
            />
            {band.map((r) => (
              <ReferenceArea key={r.from} x1={r.from} x2={r.to} fill={ink.series[6]} fillOpacity={0.34} ifOverflow="hidden" />
            ))}
            {planned.flatMap(({ i, l }) => [
              // A null slot draws no point, and the gap stays since the lines do not connect across it.
              <Line key={`${i}-5h`} type="monotone" dataKey={(r: BurnRow) => r.fiveHour[i]} stroke={colorOf(l)} strokeWidth={2} dot={isolatedDot(rows.map((r) => r.fiveHour[i]), colorOf(l))} isAnimationActive={false} />,
              <Line key={`${i}-7d`} type="monotone" dataKey={(r: BurnRow) => r.sevenDay[i]} stroke={colorOf(l)} strokeWidth={2} strokeDasharray="5 3" dot={isolatedDot(rows.map((r) => r.sevenDay[i]), colorOf(l))} isAnimationActive={false} />,
            ])}
          </LineChart>
        </ChartCard>
      )}
    </div>
  );
}

const PLACEMENT_HINT = "A placement by the conversation's own messages is a hit when the response read the prefix it was promised; one by the static prefix when it read at all";
const PLACEMENT_UNFILTERED_NOTE = '. The account, model and pattern filters above do not apply here';

/** Placements by rung over the range, narrowed by the key and pool filters, the two the route reads here. */
function PlacementTable({ error, rows, unfiltered }: { error: string | null; rows: PlacementRow[] | null; unfiltered: boolean }) {
  return (
    <Card title="Placements by rung" subtitle={`${PLACEMENT_HINT}${unfiltered ? PLACEMENT_UNFILTERED_NOTE : ''}`} padded={false} className="card--table">
      <div className="table-wrap">
        <table className="table">
          <thead>
            <tr>
              <th>Rung</th>
              <th className="right">Placements</th>
              <th className="right">Hits</th>
              <th className="right">Hit share</th>
              <th className="right">Tokens read</th>
              <th className="right">Tokens promised</th>
            </tr>
          </thead>
          <tbody>
            {(rows ?? []).map((r) => (
              <tr key={r.rung}>
                <td>{placementRungLabel(r.rung)}</td>
                {placementCells(r).map((cell, i) => (
                  <td key={i} className="right num">
                    {cell}
                  </td>
                ))}
              </tr>
            ))}
            {(rows === null || rows.length === 0) && (
              <tr>
                <td colSpan={6} className={error ? '' : 'muted'}>
                  {error ? <ErrorState title="Could not load the placements" body={error} /> : rows === null ? <Skeleton height={22} /> : 'No placements in range'}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
    </Card>
  );
}

const BUDGET_HINT = "Each budgeted member's spend in its current period, an API-key member's whole spend and a subscription member's metered spend; the range and the filters above do not apply here";

/** Spend against each member's budget; a member at its budget is skipped until its period ends. */
function BudgetTable({ budgets, error, now }: { budgets: MemberBudget[] | null; error: string | null; now: number }) {
  return (
    <Card title="Member budgets" subtitle={BUDGET_HINT} padded={false} className="card--table">
      <div className="table-wrap">
        <table className="table budgets-table">
          <thead>
            <tr>
              <th>Pool</th>
              <th>Account</th>
              <th>Spent</th>
              <th className="right">Budget</th>
              <th>Period ends</th>
            </tr>
          </thead>
          <tbody>
            {(budgets ?? []).map((b) => {
              const ratio = b.spentUsd === null ? 0 : b.spentUsd / b.budgetUsd;
              // A spend reported for a period that has since ended says nothing about the one running now.
              const reached = b.spentUsd !== null && b.spentUsd >= b.budgetUsd && b.periodEnd !== null && Date.parse(b.periodEnd) > now;
              return (
                <tr key={`${b.poolId}-${b.accountId}-${b.budgetPeriod}`}>
                  <td className="mono">{b.poolId}</td>
                  <td className="mono">
                    <Link to={`/accounts/${b.accountId}`}>{b.accountId}</Link>
                  </td>
                  <td>
                    <span className="budgets-table__spent">
                      {b.spentUsd === null ? (
                        <span className="muted" title="The ledger has not reported this member's spend yet">
                          not reported
                        </span>
                      ) : (
                        <span className="num">
                          {b.kind === 'oauth' && <span className="muted">metered </span>}
                          {fmtUsd(b.spentUsd)}
                        </span>
                      )}
                      {reached && (
                        <Badge tone="warning" title="Routing skips this member until its period ends">
                          at budget
                        </Badge>
                      )}
                    </span>
                    <Meter value={ratio} tone={reached ? 'low' : ratio >= 0.8 ? 'warn' : 'accent'} thin label={`Spend of ${b.accountId} in pool ${b.poolId} against its budget`} />
                  </td>
                  <td className="right num">
                    {fmtUsd(b.budgetUsd)} per {b.budgetPeriod}
                  </td>
                  <td title={b.periodEnd ? fmtAbsolute(b.periodEnd) : undefined}>{b.periodEnd ? <span className="num">{fmtPeriodEnd(b.periodEnd)}</span> : <span className="muted">-</span>}</td>
                </tr>
              );
            })}
            {(budgets === null || budgets.length === 0) && (
              <tr>
                <td colSpan={5} className={error ? '' : 'muted'}>
                  {error ? <ErrorState title="Could not load the member budgets" body={error} /> : budgets === null ? <Skeleton height={22} /> : 'No member carries a budget'}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
    </Card>
  );
}

const NO_SAMPLE_HINT = 'no utilization sampled in range';

/** An hour with no sample carries no peak, so the maximum is over the sampled hours alone and null without one. */
function peakOf(values: (number | null)[]): number | null {
  const sampled = values.filter((v): v is number => v !== null);
  return sampled.length ? Math.max(...sampled) : null;
}

/**
 * The account rollups carry the range and the account alone, so the page's account selection narrows them here
 * and every other filter says it does not apply.
 */
function AccountsSection({
  accountIds,
  axis,
  bucketMs,
  error,
  loading,
  otherFilters,
  rollups: all,
  span,
}: {
  accountIds: string[];
  axis: TimeAxis;
  bucketMs: number;
  error: string | null;
  loading: boolean;
  otherFilters: boolean;
  rollups: AccountRollup[] | null;
  span: { fromMs: number; toMs: number };
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
      <ChartCard title="Peak 5-hour utilization" subtitle="Highest utilization seen in each bucket, per account" loading={loading} legend={accounts.map((a) => ({ color: accountColor(ink, a), label: a, shape: 'line' }))}>
        <LineChart data={data} margin={{ top: 8, right: 8, bottom: 0, left: 0 }}>
          <CartesianGrid vertical={false} stroke={ink.grid} />
          <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
          <YAxis domain={[0, 100]} ticks={[0, 25, 50, 75, 100]} tickFormatter={(v: number) => `${v}%`} tickLine={false} axisLine={false} width={40} />
          <Tooltip cursor={{ stroke: ink.axis }} content={timeTip<Record<string, number | null> & { at: number }>((p) => accounts.map((a) => ({ color: accountColor(ink, a), label: a, value: p[a] === null || p[a] === undefined ? '-' : `${(p[a] as number).toFixed(0)}%` })))} />
          {accounts.map((a) => (
            <Line key={a} type="monotone" dataKey={a} stroke={accountColor(ink, a)} strokeWidth={1.5} dot={false} connectNulls isAnimationActive={false} />
          ))}
        </LineChart>
      </ChartCard>
      <BreakdownTable
        title="Accounts in range"
        subtitle={`Totals for the selected range and accounts${otherFilters ? '; the key, pool, model, client and session filters and the probes toggle do not apply here' : ''}`}
        error={error}
        loading={loading}
        groupLabel="Account"
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
