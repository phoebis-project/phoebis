import { useEffect, useMemo, useRef, useState, type ReactNode, type RefObject } from 'react';
import { flushSync } from 'react-dom';
import { Area, AreaChart, Bar, BarChart, CartesianGrid, Dot, Line, LineChart, ReferenceLine, ResponsiveContainer, Tooltip, XAxis, YAxis } from 'recharts';
import { Link, useLocation, useNavigate } from 'react-router-dom';
import { api } from '@/api';
import { windowId } from '@/api/decode';
import { foldGroups, foldRequests, loadSessionRequests, sumGroups, type SessionRequests } from '@/api/fold';
import { useAppSettings } from '@/app/settings';
import type { Account, AccountRollup, AnalyticsGroup, AnalyticsGroupBy, AnalyticsPoint, AnalyticsQuery, BurnGroup, BurnPoint, CacheMissReason, CostSplit, GuardrailCounts, LatencyHistograms, MemberBudget, PlacementRow, RefusalCategoriesAnswer, RequestLog, RewritesAnswer, SubscriptionCosts, UnusedAllowanceAnswer } from '@/api/types';
import { Donut } from '@/components/charts/Donut';
import { OUTCOME_SERIES, PERCENTILES, PROBE_SERIES, TOKEN_SERIES, accountColor, accountSlots, type ChartInk } from '@/components/charts/theme';
import { useChartInk } from '@/components/charts/useChartInk';
import { ChartTooltip, Legend, type TooltipRow } from '@/components/charts/Tooltip';
import { CostReferenceLink } from '@/components/ui/CostReference';
import type { Option } from '@/components/ui/OptionList';
import { figuresValue, Tile } from '@/components/ui/Tile';
import { Badge, Button, Card, ErrorState, Meter, Segmented, Skeleton } from '@/components/ui/primitives';
import { FilterBar } from '@/components/FilterBar';
import { GroupBy as GroupByControl, GroupsButton } from '@/components/GroupBy';
import { SectionBar } from '@/components/SectionBar';
import { addressWindow, ANALYTICS_CODEC, type AnalyticsAddress, RANGE_PRESETS } from '@/lib/address';
import { ALLOWANCE_WEEKS, allowanceColumnWord, allowanceRows, allowanceSpan, fleetFigures, resetInstant, weeklyKeys, type AllowanceFigure } from '@/lib/allowance';
import { CONVERSATION_CLASSES, MAX_BURN_LINES, NATIVE_SUBSET, UNIT_SUBSET, costHeadOf, dollarsOf, equivalentOrDash, errorRateOf, foldBurn, heldBars, heldHover, heldSpan, hintPlan, isolatedSlots, keptWarmNote, keptWarmTokens, modelGroupLabel, moveLabel, nativeCellOf, netSaving, niceAxisTop, oneAccountGroupOf, placementCells, placementRungLabel, readBurn, sampledWindows, shareHint, sidesOf, sizedAccounts, slotIndex, slotUnpriced, UNPRICED_TITLE, unpricedNote, upstreamProbes, windowLines, type BurnLine, type BurnLineUnit, type BurnRow, type CostFigure, type DollarPosition, type WeekReading } from '@/lib/analytics';
import { bucketSizeFor } from '@/lib/buckets';
import { WASTED_PROBE_LABEL, WASTED_PROBE_REASONS, missCount, missLabel, missReasonOrder, missShort } from '@/lib/cache';
import { clock } from '@/lib/clock';
import { weekReadingOf, weekScale } from '@/lib/costs';
import { ANALYTICS_DIMENSIONS, chipText, escapePattern } from '@/lib/filterBar';
import { effectiveInterval, readEnd, spanEnded } from '@/lib/follow';
import { fmtAbsolute, fmtBytes, fmtCompact, fmtDayTick, fmtDuration, fmtHourTick, fmtInt, fmtMs, fmtPct, fmtPeriodEnd, fmtShare, fmtShortTime, fmtUsd, fmtUsdTick, fmtWeek, pct, shortId, upperFirst, usdTickDigits, weekShort } from '@/lib/format';
import { BUCKET_COUNT, BUCKET_EDGES, PERCENTILE_HINT, histogramCount } from '@/lib/histogram';
import { useAsync, useQueryFilters, useRangeTakenAt, type Loaded } from '@/lib/hooks';
import { REFUSED_TITLE } from '@/lib/requests';
import { CLIENT_PATTERNS_PLAIN, linkedKeyIds, refusalLink, requestsLink, type RequestsLinkInput, type RequestsLinkTarget } from '@/lib/requestsLink';
import { words } from '@/lib/routingLog';
import { UNIT_FUNDING, UNIT_POSITIONS, type UnitPosition, type WeekPosition } from '@/lib/unit';
import { WINDOW_HOURS, WINDOW_LABEL } from '@/lib/windows';

/** The dimensions the breakdown route groups on; `all` is the ungrouped total every section reads. */
type GroupBy = Exclude<AnalyticsGroupBy, 'all'>;
/** The four dimensions both the analytics query and the request log filter on. */
type QueryDim = 'accountId' | 'keyId' | 'model' | 'poolId';
/** The two pattern lists, which the analytics query reads whole and the request log as one client pattern at most. */
type PatternDim = 'clients' | 'userAgents';
/** No route groups on a session, so it filters the page from the side rather than joining the group-by. */
type Dim = QueryDim | 'sessionId';

const HOUR = 3_600_000;
const GROUP_LABEL: Record<GroupBy, string> = { account: 'Account', classifier: 'Classifier', client: 'Client', key: 'Key', effort: 'Effort', kind: 'Kind', model: 'Model', pool: 'Pool', request_class: 'Request class', speed: 'Speed', user_agent: 'User agent' };
/** The two speeds the rollups file an answer under, standard at the foot of each stacked bar. */
const SPEED_SERIES = [
  { key: 'standard', label: 'standard', slot: 0 },
  { key: 'fast', label: 'fast', slot: 4 },
] as const;
/** The group-by's twelfth option, which the Burn section alone honors: no rollup carries a session. */
const SESSION_OPTION = 'Session (Burn only)';
const SESSION_OPTION_HINT = 'The Burn section groups by session, and every other section groups by key, since no rollup carries a session';
/** The page's one group-by control: the breakdown's eleven groupings, then the Burn section's own session grouping. */
const GROUP_OPTIONS: readonly { label: string; title?: string; value: BurnGroup }[] = [...(Object.keys(GROUP_LABEL) as GroupBy[]).map((value) => ({ label: GROUP_LABEL[value], value })), { label: SESSION_OPTION, title: SESSION_OPTION_HINT, value: 'session' }];
/** Filter choices come from a breakdown on that dimension, so every value the range holds is offered. */
const DIM_GROUP: Record<QueryDim | PatternDim, GroupBy> = { accountId: 'account', clients: 'client', keyId: 'key', model: 'model', poolId: 'pool', userAgents: 'user_agent' };
/** A session view pages through the session's rows, and both bounds together are the note's figure. */
const SESSION_PAGE_SIZE = 500;
const SESSION_PAGE_LIMIT = 20;
const SESSION_SOURCE_NOTE = 'From the request log, since rollups carry no session. Cache warming figures are unavailable per session';
const SESSION_COST_NOTE = 'Cost split is not available for a session view.';
// The rings' centers read bare, so the subtitle names the week the subscription ring's percents are of.
const NATIVE_SPLIT_NOTE = `By token kind, ${NATIVE_SUBSET}`;
const UNAVAILABLE_NOTE = 'not in the request log';
const WARMING_UNAVAILABLE = 'Cache warming figures are not available for a session view.';
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

/** A dollar position's requests in the Cost split's, the cache cost card's and the Probe cost tile's shorter form. */
const UNIT_SPLIT: Record<DollarPosition, string> = { all: 'all requests', equivalent: "the subscription requests' equivalent", usd: 'billed' };
/** An error rate in the Traffic head's line, a dash where the range rated no request. */
const pctOrDash = (rate: number | null) => (rate === null ? '-' : fmtPct(rate, 1));
/** An error rate in a Traffic cell, a muted dash for a group with no request to rate. */
const rateCell = (rate: number | null): ReactNode => (rate === null ? <span className="muted">-</span> : fmtPct(rate, 1));

const inputOf = (g: AnalyticsGroup) => g.tokens.uncachedInput + g.tokens.cacheRead + g.tokens.cacheWrite5m + g.tokens.cacheWrite1h;
const hitRatioOf = (g: AnalyticsGroup) => (g.cacheHits + g.cacheMisses + g.uncached ? g.cacheHits / (g.cacheHits + g.cacheMisses + g.uncached) : null);
const cachedShareOf = (g: AnalyticsGroup) => (inputOf(g) ? g.tokens.cacheRead / inputOf(g) : null);
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

type NativeRatio = { figures: CostFigure[]; note: string };
/**
 * The native Cost per request and per 1k output tiles: the plan rows' figure in the chosen week over the plan requests
 * and output tokens the route counts beside the billed dollars over the rest, a side with no rows left out as the
 * first tile leaves it, and a dash where a count is unknown or zero or the route priced the plan rows nothing.
 */
function nativeRatiosOf(totals: AnalyticsGroup, week: WeekPosition, weekFactor: number): { perRequest: NativeRatio; perThousand: NativeRatio } {
  const sides = sidesOf(totals);
  const ratio = (count: number | null, whole: number, scale: number, word: string, note: (n: number) => string): NativeRatio => {
    const billedCount = count === null ? null : whole - count;
    const per = (v: number | null, n: number | null) => (v === null || n === null || !(n > 0) ? null : (v / n) * scale);
    const equivalent = sides.equivalent === undefined ? null : per(sides.equivalent, count);
    return {
      figures: [...(sides.equivalent === undefined ? [] : [{ text: equivalentOrDash(equivalent, week, weekFactor), word: 'subscription' }]), ...(sides.billed === undefined ? [] : [{ text: usdOrDash(per(sides.billed, billedCount)), word: 'billed' }])],
      note:
        count === null || billedCount === null
          ? `${note(whole)} ${word}`
          : sides.billed === undefined
            ? `${note(count)} subscription ${word}`
            : sides.equivalent === undefined
              ? `${note(billedCount)} billed ${word}`
              : `${note(count)} subscription ${word}, ${note(billedCount)} billed`,
    };
  };
  return { perRequest: ratio(totals.planRequests, totals.requests, 1, 'requests', fmtInt), perThousand: ratio(totals.planOutputTokens, totals.tokens.output, 1000, 'output tokens', fmtCompact) };
}

/** The native table's columns: Cost, the figure in the chosen week with the billed dollars under it, then the ratios. */
const NATIVE_COLUMNS = ['Requests', 'Cost', 'Per request', 'Per 1k output'];
/** A native cell's figure over its billed line, the full reading on its title. */
const nativeCell = (cell: { main: string; sub: string; title: string }): ReactNode => (
  <span title={cell.title || undefined}>
    {cell.main}
    {cell.sub && <span className="sub">{cell.sub}</span>}
  </span>
);
/**
 * A group's native row: its requests, its cost (the plan rows' figure in the chosen week with the billed dollars under
 * it, the hint on its title where the row is one account's), and the figure per plan request and per 1k plan output
 * tokens with the billed dollars per billed request and per 1k billed output tokens under it, a side with no rows left
 * out and a figure with no count a dash.
 */
function nativeRowOf(g: AnalyticsGroup, week: WeekPosition, weekFactor: number, hint: string | null): { cells: ReactNode[]; key: string; label: string; title: string } {
  const sides = sidesOf(g);
  const figure = sides.equivalent === undefined ? null : g.costEquivalent;
  const per = (v: number | null, count: number | null, scale: number) => (v === null || count === null || !(count > 0) ? null : (v / count) * scale);
  // Undefined leaves a side with no rows out of the cell, as the tiles leave it out.
  const both = (equivalent: number | null, billed: number | null, shown: string | null = null) => nativeCell(nativeCellOf(sides.equivalent === undefined ? undefined : equivalent, sides.billed === undefined ? undefined : billed, week, weekFactor, shown));
  const ratio = (planCount: number | null, whole: number, scale: number) => both(per(figure, planCount, scale), per(sides.billed ?? null, planCount === null ? null : whole - planCount, scale));
  const label = modelGroupLabel(g);
  return {
    cells: [fmtInt(g.requests), both(figure, sides.billed ?? null, hint), ratio(g.planRequests, g.requests, 1), ratio(g.planOutputTokens, g.tokens.output, 1000)],
    key: g.key,
    label,
    title: label,
  };
}

type Pct = Record<(typeof PERCENTILES)[number]['key'], number | null>;

/** The routes answer one output sum; the chart stacks its thinking share and the rest, so the page derives the rest. */
type SplitTokens = AnalyticsGroup['tokens'] & { plainOutput: number };
const withPlainOutput = (t: AnalyticsGroup['tokens']): SplitTokens => ({ ...t, plainOutput: Math.max(0, t.output - t.thinking) });

interface Bucket extends AnalyticsGroup {
  at: number;
  tokens: SplitTokens;
}

/** The groups summed with the plain output split from the thinking, as the tiles and the charts read a total. */
const totalsOf = (groups: AnalyticsGroup[]) => {
  const summed = sumGroups(groups);
  return { ...summed, tokens: withPlainOutput(summed.tokens) };
};
const bucketsOf = (points: AnalyticsPoint[]): Bucket[] => points.map((p) => ({ at: Date.parse(p.at), ...totalsOf(p.groups) }));

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
  if (current !== null && current !== last) setLast(current);
  return current !== null ? source : { ...source, data: last };
}

// Two lifetimes of one key id can share a label, so the older lists first.
const optionsOf = (rows: AnalyticsGroup[] | null): Option[] =>
  [...(rows ?? [])].sort((a, b) => a.label.localeCompare(b.label) || (a.keyCreatedAt ?? 0) - (b.keyCreatedAt ?? 0)).map((g) => ({ label: g.label, value: g.key }));
// A pick travels as a pattern, so a key holding the grammar's characters matches itself alone. The empty key is left
// out, as the model list leaves it out.
const patternOptionsOf = (rows: AnalyticsGroup[] | null): Option[] => optionsOf(rows?.filter((g) => g.key !== '') ?? null).map((o) => ({ ...o, value: escapePattern(o.value) }));

export function AnalyticsPage() {
  const [now, setNow] = useState(() => clock.now());
  // Try again and Refresh bump it with the end, so every read keyed on the window runs again even while the clock
  // stands still.
  const [generation, setGeneration] = useState(0);
  // Every filter lives in the address, so a link or a reload lands on the same view.
  const { address, filters: a, update: rawUpdate } = useQueryFilters(ANALYTICS_CODEC);
  // Every section but the Burn reads the default grouping under the Burn's own session grouping.
  const pageBy = a.by as BurnGroup;
  const groupBy: GroupBy = pageBy === 'session' ? 'key' : pageBy;
  const clientGroups = a.groups;
  const includeProbes = a.probes;
  const filtersRef = useRef<HTMLDivElement>(null);
  const filtersHeight = useStickyHeight(filtersRef);
  // 5 s when the page opens, and page state, so a link or a reload opens on it.
  const [intervalMs, setIntervalMs] = useState(5_000);
  const [barOpen, setBarOpen] = useState(false);
  const [costOpen, setCostOpen] = useState(false);
  const pauseCause = costOpen ? 'dialog' : barOpen ? 'menu' : null;
  const paused = pauseCause !== null;
  const rangeTakenAt = useRangeTakenAt(a.range);
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
  // A session id is one session per key lifetime, so a drill-down from a session row names the lifetime too. A session
  // without one reads unscoped.
  const sessionKey = a.session && a.key ? a.key : null;
  const { setUnit, unit, week } = useAppSettings();
  const dollars = dollarsOf(unit);
  const funding = UNIT_FUNDING[unit];
  // A shared link lands on its position, then the choice persists like any other: the parameter is read into the
  // setting and dropped, so the address stays clean and one control owns the unit. The drop replaces the entry the
  // router's usual way, since `update` flushes at once, which React cannot do inside an effect.
  useEffect(() => {
    if (a.unit === '') return;
    const known = UNIT_POSITIONS.find((p) => p === a.unit);
    if (known !== undefined) setUnit(known);
    const search = ANALYTICS_CODEC.toParams({ ...a, unit: '' }).toString();
    navigate({ hash: location.hash, pathname: location.pathname, search: search ? `?${search}` : '' }, { replace: true });
  }, [a, location.hash, location.pathname, navigate, setUnit]);
  // Keyed on the values: every address change reads new lists, and a unit change must not re-issue the reads the
  // filters key. A list value holds no comma, since the address splits on them.
  const accountList = a.accounts.join(',');
  const keyList = a.keys.join(',');
  const modelList = a.models.join(',');
  const poolList = a.pools.join(',');
  const filters = useMemo<Record<Dim, string[]>>(() => {
    const split = (joined: string) => (joined ? joined.split(',') : []);
    return { accountId: split(accountList), keyId: split(keyList), model: split(modelList), poolId: split(poolList), sessionId: a.session ? [a.session] : [] };
  }, [accountList, keyList, modelList, poolList, a.session]);
  // A pattern may hold a comma, so the two pattern lists key on a newline, as the address joins them.
  const clientList = a.clients.join('\n');
  const userAgentList = a.userAgents.join('\n');
  const patterns = useMemo<Record<PatternDim, string[]>>(() => {
    const split = (joined: string) => (joined ? joined.split('\n') : []);
    return { clients: split(clientList), userAgents: split(userAgentList) };
  }, [clientList, userAgentList]);
  // The request log reads one client pattern, so a session view's log read carries the one where exactly one stands.
  const logClient = patterns.clients.length === 1 ? patterns.clients[0] : undefined;
  const ink = useChartInk();
  // A committed filter reads the window up to the instant it changed, as a range change does. The end and the address
  // change in one render, so each read goes out once: the hook writes the address at once, and a pattern commits from
  // a timer, where the end alone would otherwise render apart.
  const commit = (patch: Partial<AnalyticsAddress>, at = clock.now()) =>
    flushSync(() => {
      setNow(at);
      rawUpdate(patch);
    });
  /**
   * The bar's one writer, so the chips, Add filter and the address cannot disagree. A key lifetime belongs to the
   * session it arrived with, so a patch that moves the session drops it, and Clear all's one patch commits whole. A
   * patch that changes nothing, such as a flag chosen again, moves no end and reads nothing.
   */
  const update = (patch: Partial<AnalyticsAddress>) => {
    const next = 'session' in patch && patch.session !== a.session ? { ...patch, key: '' } : patch;
    if (Object.entries(next).every(([field, value]) => JSON.stringify(value) === JSON.stringify(a[field as keyof AnalyticsAddress]))) return;
    commit(next);
  };
  const commitGroups = (next: string) => commit({ groups: next });
  const changeGroupBy = (next: BurnGroup) => commit({ by: next });

  // The query bounds come from the address's instants, which keep the milliseconds the inputs' minutes drop.
  const custom = a.range.preset === 'custom';
  const span = useMemo(() => {
    const { from, to } = addressWindow(a.range, RANGE_PRESETS, '24h', now);
    return { fromMs: from, toMs: readEnd(custom, to, now) };
  }, [a.range, custom, now]);
  const fromIso = new Date(span.fromMs).toISOString();
  const toIso = new Date(span.toMs).toISOString();
  const ended = spanEnded(custom, span.toMs, rangeTakenAt);
  const polledMs = effectiveInterval(intervalMs, ended);
  const sessionId = filters.sessionId[0] ?? null;

  const agentGroups = useMemo(() => (groupBy === 'user_agent' && clientGroups.trim() ? clientGroups.split('\n').map((s) => s.trim()).filter(Boolean) : undefined), [clientGroups, groupBy]);
  const query = useMemo<AnalyticsQuery>(
    () => ({ accountIds: filters.accountId, clients: patterns.clients.length ? patterns.clients : undefined, from: fromIso, groupBy, includeProbes, keyIds: filters.keyId, models: filters.model, poolIds: filters.poolId, to: toIso, userAgentGroups: agentGroups, userAgents: patterns.userAgents.length ? patterns.userAgents : undefined }),
    [agentGroups, patterns, filters, fromIso, groupBy, includeProbes, toIso],
  );
  // The route narrows the rows a session view folds by one client pattern at most, and the fold applies both pattern
  // lists over what it holds: several client patterns narrow here alone, a cursor page that crossed a filter change
  // may be wider than the patterns, and the user agent matches its folded token, as the unscoped page's rollups do,
  // where the route matches the raw header.
  const foldOptions = useMemo(() => ({ agentGroups, agentPatterns: patterns.userAgents.length ? patterns.userAgents : undefined, clientPatterns: patterns.clients.length ? patterns.clients : undefined }), [agentGroups, patterns]);
  // A custom span holds its end, so no dependency moves at the interval: its window's reads poll at it instead, and the
  // figures stay drawn until the new answer lands, where a request that finished late shows, in the session log when it
  // falls on the first page. A preset's end moves to now at a boundary, and that change reads again.
  const reload = { paused, refreshMs: custom ? polledMs : 0 };
  // A session view answers every section from the request log, so the rollup reads stay home while one is set.
  const series = useAsync(() => (sessionId === null ? api.getAnalyticsSeries(query) : Promise.resolve<AnalyticsPoint[]>([])), [query, sessionId, generation], reload);
  const breakdown = useAsync(() => (sessionId === null ? api.getAnalyticsBreakdown(query) : Promise.resolve<AnalyticsGroup[]>([])), [query, sessionId, generation], reload);
  // A read only a hidden tab draws answers null without a request, and runs once its tab is chosen.
  const byModel = useAsync(() => (tab === 'cache' && sessionId === null ? api.getAnalyticsBreakdown({ ...query, groupBy: 'model' }) : Promise.resolve<AnalyticsGroup[] | null>(sessionId === null ? null : [])), [query, sessionId, tab, generation], reload);
  const byPool = useAsync(() => (tab === 'cache-warming' && sessionId === null ? api.getAnalyticsBreakdown({ ...query, groupBy: 'pool' }) : Promise.resolve<AnalyticsGroup[] | null>(sessionId === null ? null : [])), [query, sessionId, tab, generation], reload);
  // The log's last answer with the deps it answers, so an auto reload of the same deps reads the first page alone and
  // the later pages stand as the walk loaded them. Other deps walk the pages again. The ref names the current deps from
  // the moment a read starts, so a read under earlier deps that settles later keeps nothing.
  const heldLog = useRef<{ answer: SessionRequests | null; deps: string } | null>(null);
  const logDeps = JSON.stringify([logClient, filters, fromIso, includeProbes, sessionId, sessionKey, toIso, generation]);
  const log = useAsync(
    () => {
      if (heldLog.current?.deps !== logDeps) heldLog.current = { answer: null, deps: logDeps };
      if (sessionId === null) return Promise.resolve({ partial: false, requests: [] as RequestLog[] });
      const held = heldLog.current.answer ?? undefined;
      return loadSessionRequests(
        (cursor) =>
          api
            .listRequests({ accountIds: filters.accountId, client: logClient, cursor, from: fromIso, includeProbes, keyIds: sessionKey && filters.keyId.length === 0 ? [sessionKey] : filters.keyId, limit: SESSION_PAGE_SIZE, models: filters.model, poolIds: filters.poolId, session: sessionId, to: toIso })
            // The route reads one key list, so a lifetime beside a key filter is applied here as well.
            .then((page) => ({ ...page, requests: page.requests.filter((r) => sessionKey === null || `${r.keyId}@${r.keyGeneration}` === sessionKey) })),
        SESSION_PAGE_LIMIT,
        held,
      ).then((answer) => {
        if (heldLog.current?.deps === logDeps) heldLog.current = { answer, deps: logDeps };
        return answer;
      });
    },
    [logClient, filters, fromIso, includeProbes, sessionId, sessionKey, toIso, generation],
    reload,
  );
  const accountRollups = useAsync(() => (tab === 'accounts' ? api.listAccountRollups(fromIso, toIso) : Promise.resolve(null)), [fromIso, toIso, tab, generation], reload);
  // The fold rates the hour still running over the part of it this read saw, so the answer keeps its end and instant.
  // The instant is the read's own: the range clock stands still while another tab is open, and a read made later in
  // the same hour has seen more of it.
  const burn = useAsync(() => {
    if (tab !== 'burn') return Promise.resolve(null);
    const readAt = clock.now();
    return readBurn(api, { from: fromIso, to: toIso }, pageBy, { accountIds: filters.accountId, userAgentGroups: agentGroups }).then((read) => ({ ...read, endMs: span.toMs, readAt }));
  }, [fromIso, toIso, pageBy, filters.accountId, agentGroups, tab, generation], reload);
  // The registry's list, for the account slots alone: the range's option list holds the accounts with traffic and
  // differs from the Dashboard's, and one account keeps one color across the views.
  const registry = useAsync(() => api.listAccounts(), [], { paused, refreshMs: polledMs });
  const slotOf = useMemo(() => accountSlots((registry.data ?? []).map((a) => a.id)), [registry.data]);
  // A weekly row changes once a week, so the unused allowance card reads once per Accounts filter, again on Refresh and
  // once the registry's poll answers a weekly window of an account under the filter past its reset, never at the list's
  // cadence, under a custom range too. The registry's first answer moves no reset, so the page's load reads once.
  const weeklyResetsAt =
    registry.data === null
      ? null
      : registry.data
          .filter((a) => filters.accountId.length === 0 || filters.accountId.includes(a.id))
          .flatMap((a) => [...a.windows, ...(a.fableWindow ? [a.fableWindow] : [])])
          .filter((w) => w.periodHours === WINDOW_HOURS['7d'])
          .map((w) => w.resetsAt ?? '')
          .join(',');
  const [resets, setResets] = useState({ at: weeklyResetsAt, moved: 0 });
  if (weeklyResetsAt !== resets.at) setResets({ at: weeklyResetsAt, moved: resets.at === null ? resets.moved : resets.moved + 1 });
  // The weekly resets of the last four weeks whatever the range, so a range change reads nothing, and the Accounts
  // filter alone narrows them.
  const unusedAllowance = useAsync(() => (tab === 'accounts' ? api.getAnalyticsUnusedAllowance(allowanceSpan(now), { accountIds: filters.accountId }) : Promise.resolve(null)), [tab, filters.accountId, generation, resets.moved]);
  // The cost model, read once per page load whatever the tab: the chosen week's scale reads every week figure the page
  // draws, the plan chart's line category reads each account's stored tier against its capacity rows, and the unused
  // allowance card prices its figures through it.
  const costSettings = useAsync(() => api.getSettings(), []);
  const costs = costSettings.data?.subscriptionCosts ?? null;
  const reading = weekReadingOf(costs, costSettings.error, week);
  const { scale } = reading;
  const sized = useMemo(() => (burn.data && costs ? sizedAccounts(burn.data.accounts, costs) : null), [burn.data, costs]);
  const rewrites = useAsync(() => (tab === 'traffic' ? api.getAnalyticsRewrites({ from: fromIso, to: toIso }) : Promise.resolve(null)), [fromIso, toIso, tab, generation], reload);
  const refusalCategories = useAsync(() => (tab === 'traffic' ? api.getAnalyticsRefusalCategories(query) : Promise.resolve(null)), [query, tab, generation], reload);
  // The counts route reads the range alone, which the card's subtitle says, since it carries no filter.
  const guardrailCounts = useAsync(() => (tab === 'traffic' ? api.getAnalyticsGuardrails({ from: fromIso, to: toIso }) : Promise.resolve(null)), [fromIso, toIso, tab, generation], reload);
  // Side groupings on the page's own query, as byModel and byPool are; a session view folds them from the log.
  const bySpeed = useAsync(() => ((tab === 'traffic' || tab === 'cost') && sessionId === null ? api.getAnalyticsSeries({ ...query, groupBy: 'speed' }) : Promise.resolve<AnalyticsPoint[] | null>(sessionId === null ? null : [])), [query, sessionId, tab, generation], reload);
  const byEffort = useAsync(() => (tab === 'traffic' && sessionId === null ? api.getAnalyticsBreakdown({ ...query, groupBy: 'effort' }) : Promise.resolve<AnalyticsGroup[] | null>(sessionId === null ? null : [])), [query, sessionId, tab, generation], reload);
  // A dollar side narrows the three cost-bearing sections to the rows that side paid for, a filter the page's reads
  // cannot carry (Traffic, Latency and the Cache token charts read every row), so those sections read a second query
  // while one of them shows. The whole reads the page's own.
  const fundedTab = tab === 'cache' || tab === 'cache-warming' || tab === 'cost';
  const funded = sessionId === null && (unit === 'usd' || unit === 'equivalent');
  const costSeries = useAsync(() => (funded && fundedTab ? api.getAnalyticsSeries({ ...query, funding }) : Promise.resolve<AnalyticsPoint[] | null>(null)), [query, funding, funded, fundedTab, generation], reload);
  // The Cache warming card's Probe cost tile is the one reader of this breakdown on its tab, and it reads the probe
  // rows counted under every position, the whole included: a breakdown that does not count probes drops a group left
  // with probe rows alone, whether a dollar side dropped the group's real rows or the group is probe-only in the whole
  // (the probe kind, an idle account over a short range), and the three positions add up only when all three keep it.
  // Under the whole the read carries no funding.
  const warming = sessionId === null && tab === 'cache-warming';
  const costBreakdown = useAsync(
    () => ((funded && fundedTab) || warming ? api.getAnalyticsBreakdown({ ...query, funding: funded ? funding : undefined, includeProbes: query.includeProbes || warming }) : Promise.resolve<AnalyticsGroup[] | null>(null)),
    [query, funding, funded, fundedTab, warming, generation],
    reload,
  );
  // The Saved by probes chart's series twin of that breakdown, counting probes under the same funding: the page's
  // series counts them only when the address asks, and a bucket left with probe rows alone would draw no probe cost
  // there.
  const warmingSeries = useAsync(
    () => (warming ? api.getAnalyticsSeries({ ...query, funding: funded ? funding : undefined, includeProbes: true }) : Promise.resolve<AnalyticsPoint[] | null>(null)),
    [query, funding, funded, warming, generation],
    reload,
  );
  // Native pairs the whole read's plan probes with the billed probes' dollars, which no member of the whole carries, so
  // the Cache warming tab reads the billed rows once more, counting probes as the whole read does.
  const nativeWarming = warming && unit === 'native';
  const billedBreakdown = useAsync(() => (nativeWarming ? api.getAnalyticsBreakdown({ ...query, funding: 'billed', includeProbes: true }) : Promise.resolve<AnalyticsGroup[] | null>(null)), [query, nativeWarming, generation], reload);
  // And the chart's billed twin, whose dollars the native tooltip pairs with the plan side's equivalents.
  const billedWarmingSeries = useAsync(() => (nativeWarming ? api.getAnalyticsSeries({ ...query, funding: 'billed', includeProbes: true }) : Promise.resolve<AnalyticsPoint[] | null>(null)), [query, nativeWarming, generation], reload);
  // A dollar side narrows Cache by model's two cost cells alone, its token columns reading every row, so the side's
  // rows by model come from a read of their own beside the page's.
  const byModelFunded = useAsync(() => (tab === 'cache' && funded ? api.getAnalyticsBreakdown({ ...query, funding, groupBy: 'model' }) : Promise.resolve<AnalyticsGroup[] | null>(null)), [query, funding, funded, tab, generation], reload);
  // The by-pool table's Probe cost cell reads a dollar side's probes, and under native the billed probes beside the
  // whole read's equivalent. Probes count, so a pool left with probe rows alone on that side keeps its figure.
  const poolFunding = nativeWarming ? 'billed' : warming && funded ? funding : undefined;
  const fundedByPool = useAsync(
    () => (poolFunding !== undefined ? api.getAnalyticsBreakdown({ ...query, funding: poolFunding, groupBy: 'pool', includeProbes: true }) : Promise.resolve<AnalyticsGroup[] | null>(null)),
    [query, poolFunding, generation],
    reload,
  );
  const placements = useAsync(
    () => (tab === 'cache' && sessionId === null ? api.analyticsPlacements({ from: fromIso, keyIds: filters.keyId, poolIds: filters.poolId, to: toIso }) : Promise.resolve<PlacementRow[] | null>(sessionId === null ? null : [])),
    [fromIso, toIso, filters.keyId, filters.poolId, sessionId, tab, generation],
    reload,
  );
  const budgets = useAsync(() => (tab === 'burn' ? api.analyticsBudgets() : Promise.resolve(null)), [tab]);
  // With no filter and no session the page's breakdown is the range's whole key list on its grouping.
  const plain = sessionId === null && filters.accountId.length === 0 && filters.keyId.length === 0 && filters.model.length === 0 && filters.poolId.length === 0 && patterns.clients.length === 0 && patterns.userAgents.length === 0 && !includeProbes && agentGroups === undefined;
  const accountKeys = useDimensionKeys(fromIso, toIso, DIM_GROUP.accountId, generation, plain && groupBy === DIM_GROUP.accountId ? breakdown : undefined);
  const keyKeys = useDimensionKeys(fromIso, toIso, DIM_GROUP.keyId, generation, plain && groupBy === DIM_GROUP.keyId ? breakdown : undefined);
  const modelKeys = useDimensionKeys(fromIso, toIso, DIM_GROUP.model, generation, plain && groupBy === DIM_GROUP.model ? breakdown : undefined);
  const poolKeys = useDimensionKeys(fromIso, toIso, DIM_GROUP.poolId, generation, plain && groupBy === DIM_GROUP.poolId ? breakdown : undefined);
  const clientKeys = useDimensionKeys(fromIso, toIso, DIM_GROUP.clients, generation, plain && groupBy === DIM_GROUP.clients ? breakdown : undefined);
  const userAgentKeys = useDimensionKeys(fromIso, toIso, DIM_GROUP.userAgents, generation, plain && groupBy === DIM_GROUP.userAgents ? breakdown : undefined);
  const options = useMemo<Record<QueryDim | PatternDim, Option[]>>(
    // The group without a model has no model id to filter on, and a blank option would read as an error.
    () => ({
      accountId: optionsOf(accountKeys.data),
      clients: patternOptionsOf(clientKeys.data),
      keyId: optionsOf(keyKeys.data),
      model: optionsOf(modelKeys.data?.filter((g) => g.key !== '') ?? null),
      poolId: optionsOf(poolKeys.data),
      userAgents: patternOptionsOf(userAgentKeys.data),
    }),
    [accountKeys.data, clientKeys.data, keyKeys.data, modelKeys.data, poolKeys.data, userAgentKeys.data],
  );
  const barOptions = useMemo(() => ({ accounts: options.accountId, clients: options.clients, keys: options.keyId, models: options.model, pools: options.poolId, userAgents: options.userAgents }), [options]);
  // The session chip keeps its id form, and its title names the key lifetime the session arrived with, which draws no
  // chip of its own.
  const dimensions = useMemo(
    () => ANALYTICS_DIMENSIONS.map((d) => (d.field !== 'session' ? d : { ...d, text: (value: unknown, known: Option[]) => ({ ...chipText(d, value, known), title: a.key ? `${String(value)} under the key ${a.key}` : String(value) }) })),
    [a.key],
  );

  const rangeBucketMs = bucketSizeFor(span.toMs - span.fromMs);
  // At the auto reload interval: a passed bucket boundary moves the end, so a page left open shows the hour it is in.
  // Off, the end holds where it was read. The check waits, as the reads do, while a menu under the bar or a dialog is
  // open or the tab is hidden.
  useEffect(() => {
    if (polledMs === 0 || paused) return;
    const id = clock.setInterval(() => {
      if (!document.hidden && Math.floor(clock.now() / rangeBucketMs) !== Math.floor(now / rangeBucketMs)) setNow(clock.now());
    }, polledMs);
    return () => clock.clearInterval(id);
  }, [now, paused, polledMs, rangeBucketMs]);
  // A read that failed or that still answers the filters before the change draws nothing, so no figure stands
  // under a filter it was not read for.
  const failure = series.error ?? breakdown.error ?? log.error;
  const stale = series.stale || breakdown.stale || log.stale;
  // An outage of the ledger fails the section and filter-choice reads with these three, and Try again and the bar's
  // Refresh are the controls that read again, so they read everything the view draws from: the reads keyed on the
  // window through the moved end and the generation in one update, so none runs twice, and the two with no window by
  // reloading.
  const retry = () => {
    setNow(clock.now());
    setGeneration((g) => g + 1);
    budgets.reload();
    costSettings.reload();
  };
  const rows = useMemo(() => (log.stale ? [] : (log.data?.requests ?? [])), [log.stale, log.data]);
  const points = useMemo(() => (stale ? [] : sessionId === null ? (series.data ?? []) : foldRequests(rows, rangeBucketMs, fromIso, toIso, groupBy, foldOptions)), [stale, sessionId, series.data, rows, rangeBucketMs, fromIso, toIso, groupBy, foldOptions]);
  const buckets = useMemo(() => bucketsOf(points), [points]);
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
  const totals = useMemo(() => totalsOf(groups), [groups]);
  // A session fold has no warming facts, so those figures read unavailable rather than zero.
  const { keptWarm } = totals;
  const probesUnmeasured = totals.fromRequests === true;
  // The cost-bearing sections' rows under the position: the funded query's under a dollar side, the page's own under
  // the whole, and in a session view the rows folded by who paid for them.
  const costPoints = useMemo(() => {
    if (sessionId !== null) return funding === undefined ? points : stale ? [] : foldRequests(rows, rangeBucketMs, fromIso, toIso, groupBy, { ...foldOptions, funding });
    return funded ? (costSeries.stale ? [] : (costSeries.data ?? [])) : points;
  }, [sessionId, funding, funded, points, stale, rows, rangeBucketMs, fromIso, toIso, groupBy, foldOptions, costSeries.stale, costSeries.data]);
  const costGroups = useMemo(() => {
    if (sessionId !== null) return funding === undefined ? groups : stale ? [] : foldGroups(rows, groupBy, { ...foldOptions, funding });
    return funded || warming ? (costBreakdown.stale ? [] : (costBreakdown.data ?? [])) : groups;
  }, [sessionId, funding, funded, warming, groups, stale, rows, groupBy, foldOptions, costBreakdown.stale, costBreakdown.data]);
  const costBuckets = useMemo(() => (costPoints === points ? buckets : bucketsOf(costPoints)), [costPoints, points, buckets]);
  const costTotals = useMemo(() => totalsOf(costGroups), [costGroups]);
  const costHead = costHeadOf(unit, costTotals, week, scale);
  const nativeRatios = scale === null ? null : nativeRatiosOf(costTotals, week, scale);
  // A session fold has no cost split either, so the donut and the cache cost card read unavailable in a session view.
  const costSplit = knownSplit(costTotals.cost);
  // The routes answer a named account's own-window shares on every group under any grouping, and under the account
  // grouping each account's on its own group. So a row under either reads one account's hint, and the tiles under one
  // named account sum its groups, whose shares add since they share its windows. A group summing several accounts and
  // the probe, saving, net and cache figures carry none. The registry names the plan and the tier, so a figure carries
  // no hint while it is out.
  const namedAccount = filters.accountId.length === 1 ? filters.accountId[0] : null;
  const hintOf = (id: string, shares: Record<string, number | null> | null): string | null => {
    const account = registry.data?.find((x) => x.id === id);
    const tier = account?.subscription?.rateLimitTier ?? null;
    const plan = account === undefined ? null : hintPlan(account, tier);
    return plan === null ? null : shareHint(plan, shares, tier, null);
  };
  const rowHint = (g: AnalyticsGroup): string | null => (groupBy === 'account' ? hintOf(g.key, g.costShares) : namedAccount === null ? null : hintOf(namedAccount, g.costShares));
  const oneGroup = useMemo(() => (namedAccount === null ? null : oneAccountGroupOf(costTotals, costGroups)), [costGroups, costTotals, namedAccount]);
  const tileHint = namedAccount === null || oneGroup === null ? null : hintOf(namedAccount, oneGroup.costShares);
  const otherFilters = filters.keyId.length > 0 || filters.poolId.length > 0 || filters.model.length > 0 || patterns.userAgents.length > 0 || patterns.clients.length > 0 || sessionId !== null || includeProbes;
  // The rewrite and fit routes read the range alone, so every filter of the bar passes them by. The burn route reads
  // the Accounts filter and the user agent groups beside it.
  const unfiltered = otherFilters || filters.accountId.length > 0;
  const burnAccount = filters.accountId.length === 1 ? filters.accountId[0] : null;
  // The plan chart sizes its lines by tier where each group holds one account's rows.
  const burnSizes = pageBy === 'account' || burnAccount !== null;
  const burnReading: WeekReading = burnSizes && sized === null ? { error: costSettings.error, scale: null, unpriced: false, week } : reading;

  const groupSlot = useMemo(() => {
    // Slots follow the range's whole key list so a filter never repaints the survivors, and an account's slot is the
    // registry's. A dimension with no option list of its own falls back to the keys the breakdown answered.
    const listed = groupBy === 'account' ? options.accountId : groupBy === 'key' ? options.keyId : groupBy === 'model' ? options.model : groupBy === 'pool' ? options.poolId : [];
    const keys = listed.length ? listed.map((o) => o.value) : groups.map((g) => g.key);
    return (key: string) => (groupBy === 'account' ? accountColor(ink, slotOf(key)) : ink.series[slotIndex(keys, key)]);
  }, [options, groups, groupBy, ink, slotOf]);

  const axis = timeAxis(bucketMs, ink);
  // A failed read shows its error alone: a skeleton beside it would say a load is still running, and an empty
  // fold would read as zero traffic, so the sections it feeds stay out until a read succeeds.
  const shown = failure === null;
  const loading = shown && (stale || (sessionId === null ? series.data === null || breakdown.data === null : log.data === null));
  const empty = shown && !loading && groups.length === 0;
  const speedLoading = sessionId === null ? bySpeed.data === null || bySpeed.stale : loading;
  const effortLoading = sessionId === null ? byEffort.data === null || byEffort.stale : loading;
  const speedError = sessionId === null ? bySpeed.error : null;
  // The funded reads stand or fall on their own: while one is out or stale the cost surfaces read loading, and its
  // failure draws in place of their figures, so no figure of one position stands under another's subtitle.
  const costLoading = funded
    ? costSeries.data === null || costSeries.stale || costBreakdown.data === null || costBreakdown.stale
    : warming
      ? costBreakdown.data === null || costBreakdown.stale || (nativeWarming && (billedBreakdown.data === null || billedBreakdown.stale))
      : loading;
  const costError = funded ? (costSeries.error ?? costBreakdown.error) : warming ? (costBreakdown.error ?? billedBreakdown.error) : null;
  // Under native every week figure waits on the scale: a surface draws its skeleton while the cost model is out and the
  // reading's error in its place, never a Pro-week figure under another week's label.
  const weekWait = unit === 'native' && scale === null;
  const figureLoading = costLoading || weekWait;
  const figureError = costError ?? (weekWait ? reading.error : null);
  const figureUnpriced = costError === null && weekWait && reading.unpriced;
  const warmingError = sessionId === null ? (warmingSeries.error ?? billedWarmingSeries.error) : null;
  const modelError = byModel.error ?? byModelFunded.error;
  const poolError = sessionId === null ? (byPool.error ?? fundedByPool.error) : null;
  const billedTotals = useMemo(() => (nativeWarming && !billedBreakdown.stale && billedBreakdown.data !== null ? sumGroups(billedBreakdown.data) : null), [nativeWarming, billedBreakdown.stale, billedBreakdown.data]);
  const probeTile = probeTileOf(unit, costTotals, billedTotals, figureLoading, sessionId !== null, reading);
  const savingTile = savingTileOf(unit, costTotals, billedTotals, figureLoading, sessionId !== null, reading);
  const netTile = netTileOf(unit, costTotals, billedTotals, figureLoading, sessionId !== null, reading);
  const warmingPoints = useMemo(() => (warmingSeries.stale ? [] : (warmingSeries.data ?? [])), [warmingSeries.stale, warmingSeries.data]);
  const billedWarmingPoints = nativeWarming && !billedWarmingSeries.stale ? billedWarmingSeries.data : null;
  const warmingLoading = warmingSeries.data === null || warmingSeries.stale || (nativeWarming && (billedWarmingSeries.data === null || billedWarmingSeries.stale));
  const fundedModels = useMemo(() => new Map((byModelFunded.stale ? [] : (byModelFunded.data ?? [])).map((g) => [g.key, g])), [byModelFunded.stale, byModelFunded.data]);
  const fundedPools = useMemo(() => new Map((fundedByPool.stale ? [] : (fundedByPool.data ?? [])).map((g) => [g.key, g])), [fundedByPool.stale, fundedByPool.data]);
  // Native draws the cache card's bars in the plan rows' figure with the billed dollars in its subtitle, and a range
  // with no plan row draws the billed rows' dollars, its one side.
  const cacheSides = sidesOf(costTotals);
  const cacheEquivalent = unit === 'native' && cacheSides.equivalent !== undefined;
  const cacheScale = cacheEquivalent ? scale : null;
  const cacheDollars = unit === 'native' ? 'billed' : 'cost';
  const cacheSplit = unit === 'native' ? knownSplit(costTotals.billed) : costSplit;
  // Absent on a fold and wherever no plan row priced, so the legend reads a dash there.
  const cacheFigure = costTotals.cacheEquivalent ?? undefined;
  // Usage credits alone fund a fast answer, on a subscription account as on an API key.
  const fastSpend = speedPoints.reduce((n, p) => n + (p.groups.find((g) => g.key === 'fast')?.costUsd ?? 0), 0);
  const groupLabel = GROUP_LABEL[groupBy];
  // The reasons panel's counts link under one window and the page's filters. A session view's log read keeps the
  // session's key lifetime alone, so its links carry that lifetime. The request log takes one client pattern, so
  // several draw every count plain. The user agent patterns ride joined, so any of them keeps a count plain, since the
  // request log matches the raw header.
  const linkInput: ReasonLink = {
    filters: { accountIds: filters.accountId, clientPattern: patterns.clients.length === 1 ? patterns.clients[0] : '', includeProbes, keyIds: linkedKeyIds(filters.keyId, sessionId === null ? null : sessionKey), models: filters.model, poolIds: filters.poolId, sessionId, userAgentPattern: patterns.userAgents.join('\n') },
    window: { exact: sessionId !== null, fromMs: span.fromMs, nowMs: now, toMs: span.toMs },
  };
  const plainLinks = patterns.clients.length > 1;
  // The refusal route reads the page's own key selection, never a session view's key lifetime, and `refusalLink` sets
  // the exact window and leaves the session out.
  const refusalInput: ReasonLink = { ...linkInput, filters: { ...linkInput.filters, keyIds: filters.keyId } };

  return (
    <div className="stack">
      <FilterBar
        address={address}
        dimensions={dimensions}
        filters={a}
        onOpenChange={setBarOpen}
        options={barOptions}
        range={{ now: clock.now, window: { from: span.fromMs, to: span.toMs } }}
        ref={filtersRef}
        refresh={{
          disabled: ended ? 'A custom range that has ended has nothing new to read' : undefined,
          intervalMs,
          note: intervalMs === 0 && !custom ? `the window ends ${fmtShortTime(toIso)}` : undefined,
          onIntervalChange: setIntervalMs,
          onRefresh: retry,
          paused: pauseCause,
          refreshTitle: custom ? `Window from ${fmtShortTime(fromIso)} to ${fmtShortTime(toIso)}. Refresh reads the same window again` : `Window ends ${fmtShortTime(toIso)}. Refresh reads up to now`,
        }}
        sticky
        update={update}
      >
        <GroupByControl onChange={changeGroupBy} options={GROUP_OPTIONS} value={pageBy} />
        {groupBy === 'user_agent' && <GroupsButton onCommit={commitGroups} value={clientGroups} />}
      </FilterBar>
      {!log.stale && log.data?.partial && (
        <p className="muted" role="status">
          Partial: the first {fmtInt(SESSION_PAGE_LIMIT * SESSION_PAGE_SIZE)} requests of the range
        </p>
      )}

      <SectionBar sections={SECTIONS} shown={tab} onChoose={chooseTab} offset={filtersHeight} />

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
        <Section id="traffic" shown={shown} title="Traffic" meta={loading ? undefined : `${fmtInt(totals.requests)} requests, ${pctOrDash(errorRateOf(totals))} errors`}>
          <div className="grid-2 traffic-grid">
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

            <Card title="Errors by code" subtitle="Failed and incomplete responses in range, the refused requests left out">
              {loading ? <Skeleton height={90} /> : <ErrorBars errors={totals.errors} color={ink.series[7]} />}
            </Card>

            {/* The two refusal lists follow the errors in turn, so the grid pairs Requests by outcome with Errors by
                code, the two refusal cards, Failovers with Migration rewrites and Requests by speed with Effort levels,
                eight cards and no lone one. */}
            <Card title="Refusals by code" subtitle="Requests the router refused before any account was asked, not in the counts">
              {loading ? <Skeleton height={90} /> : <ErrorBars color={ink.series[7]} empty="No request was refused in this range." errors={totals.refusals} />}
            </Card>

            {/* Its own error with Try again, as a card reading a route of its own draws one, and no count while the
                answer is stale, so no figure stands under links for filters it was not read for. */}
            <Card title="Refusals by category" subtitle={`Answers the upstream refused in range, by category, each a request that succeeded${refusalsNote(sessionId !== null)}`}>
              {refusalCategories.error !== null ? (
                <ErrorState
                  title="Could not load refusals by category"
                  body={refusalCategories.error}
                  action={
                    <Button size="sm" onClick={refusalCategories.reload}>
                      Try again
                    </Button>
                  }
                />
              ) : refusalCategories.data === null || refusalCategories.stale ? (
                <Skeleton height={90} />
              ) : (
                <CountBars color={ink.series[7]} empty="No answer was refused in this range." rows={refusalRows(refusalCategories.data, refusalInput, plainLinks)} />
              )}
            </Card>

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
          {/* Below the grid at full width, so the four columns read as the table under it does. */}
          <GuardrailsCard answer={guardrailCounts.stale ? null : guardrailCounts.data} error={guardrailCounts.error} />
          <BreakdownTable
            title={`Traffic by ${groupLabel.toLowerCase()}`}
            loading={loading}
            groupLabel={groupLabel}
            columns={[<span key="requests" title="The refused requests are left out, see the Refused column">Requests</span>, 'OK', 'Rate limited', 'Failed', 'Incomplete', 'Exhausted', 'Rejected', <span key="refused" title={REFUSED_TITLE}>Refused</span>, <span key="rate" title="The refused requests are left out, see the Refused column">Error rate</span>, 'Failovers', 'Search', 'Fetch']}
            rows={groups.map((g) => ({
              key: g.key,
              label: modelGroupLabel(g),
              // Search and Fetch count the server-tool calls the group's answers made, not tokens.
              cells: [fmtInt(g.requests), fmtInt(g.succeeded), fmtInt(g.rateLimited), fmtInt(g.failed), fmtInt(g.incomplete), fmtInt(g.exhausted), fmtInt(g.rejected), fmtInt(g.refused), rateCell(errorRateOf(g)), fmtInt(g.failovers), fmtInt(g.tokens.webSearch), fmtInt(g.tokens.webFetch)],
            }))}
          />
        </Section>
      )}

      {tab === 'burn' && (
        <Section id="burn" shown={shown} title="Burn" meta={`Tokens, spend and plan share over the range${otherFilters ? BURN_UNFILTERED_NOTE : ''}`}>
          {/* The plan card's figure waits on the chosen week's scale and, where its lines are one account's, on the
              tiers that pick their category, both carried by `burnReading` with their failure. Spend per minute
              sizes nothing, so it draws while the settings read is out. */}
          <BurnCharts
            account={burnAccount}
            answer={burn.stale ? null : (burn.data?.answer ?? null)}
            accounts={burn.stale ? null : (burn.data?.accounts ?? null)}
            costs={costs}
            error={burn.error}
            groupBy={pageBy}
            loading={burn.data === null || burn.stale}
            reading={burnReading}
            onCostReferenceOpenChange={setCostOpen}
            rangeEndMs={burn.data?.endMs}
            readAt={burn.data?.readAt}
            sized={sized}
            slotOf={slotOf}
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
              label: modelGroupLabel(g),
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
              subtitle={
                cacheEquivalent
                  ? `Share of the chosen week per bucket spent writing the cache and reading it back by the subscription requests${cacheSides.billed === undefined ? '' : `, the billed requests ${usdOrDash(costTotals.billed.cacheWrite)} writing and ${usdOrDash(costTotals.billed.cacheRead)} reading`}`
                  : `USD per bucket spent writing the cache and reading it back, ${UNIT_SPLIT[unit === 'native' ? 'usd' : dollars]}`
              }
              loading={cacheEquivalent ? figureLoading : costLoading}
              error={cacheEquivalent ? figureError : costError}
              unpriced={cacheEquivalent && figureUnpriced}
              legend={
                cacheScale !== null
                  ? [
                      { color: ink.series[3], label: 'Write cost', value: equivalentOrDash(cacheFigure?.write, week, cacheScale) },
                      { color: ink.series[2], label: 'Read cost', value: equivalentOrDash(cacheFigure?.read, week, cacheScale) },
                    ]
                  : [
                      { color: ink.series[3], label: 'Write cost', value: usdOrDash(costTotals[cacheDollars].cacheWrite) },
                      { color: ink.series[2], label: 'Read cost', value: usdOrDash(costTotals[cacheDollars].cacheRead) },
                    ]
              }
            >
              {/* The bars draw the Pro-week figure, so the axis and the tooltip read it in the chosen week. */}
              {cacheScale !== null ? (
                <BarChart data={costBuckets} margin={{ top: 8, right: 8, bottom: 0, left: 0 }} barCategoryGap={4} barGap={2}>
                  <CartesianGrid vertical={false} stroke={ink.grid} />
                  <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
                  <YAxis tickFormatter={(v: number) => fmtShare(v * cacheScale)} tickLine={false} axisLine={false} width={60} />
                  <Tooltip
                    cursor={{ fill: ink.grid, opacity: 0.6 }}
                    content={timeTip<Bucket>((p) => [
                      { color: ink.series[3], label: 'write cost', value: equivalentOrDash(p.cacheEquivalent?.write, week, cacheScale) },
                      { color: ink.series[2], label: 'read cost', value: equivalentOrDash(p.cacheEquivalent?.read, week, cacheScale) },
                    ])}
                  />
                  <Bar dataKey="cacheEquivalent.write" fill={ink.series[3]} maxBarSize={12} radius={[3, 3, 0, 0]} isAnimationActive={false} />
                  <Bar dataKey="cacheEquivalent.read" fill={ink.series[2]} maxBarSize={12} radius={[3, 3, 0, 0]} isAnimationActive={false} />
                </BarChart>
              ) : (
                <BarChart data={costBuckets} margin={{ top: 8, right: 8, bottom: 0, left: 0 }} barCategoryGap={4} barGap={2}>
                  <CartesianGrid vertical={false} stroke={ink.grid} />
                  <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
                  <YAxis {...usdAxis(costBuckets.flatMap((b) => [b[cacheDollars].cacheWrite ?? 0, b[cacheDollars].cacheRead ?? 0]))} tickLine={false} axisLine={false} />
                  <Tooltip
                    cursor={{ fill: ink.grid, opacity: 0.6 }}
                    content={timeTip<Bucket>((p) => [
                      { color: ink.series[3], label: 'write cost', value: cacheSplit === null ? '-' : usdOrDash(p[cacheDollars].cacheWrite) },
                      { color: ink.series[2], label: 'read cost', value: cacheSplit === null ? '-' : usdOrDash(p[cacheDollars].cacheRead) },
                    ])}
                  />
                  {cacheSplit !== null && <Bar dataKey={`${cacheDollars}.cacheWrite`} fill={ink.series[3]} maxBarSize={12} radius={[3, 3, 0, 0]} isAnimationActive={false} />}
                  {cacheSplit !== null && <Bar dataKey={`${cacheDollars}.cacheRead`} fill={ink.series[2]} maxBarSize={12} radius={[3, 3, 0, 0]} isAnimationActive={false} />}
                </BarChart>
              )}
            </ChartCard>

            <Card title="Misses by reason" subtitle={MISS_REASON_BLURB}>
              {loading ? <Skeleton height={190} /> : <ReasonBars link={linkInput} plainLinks={plainLinks} totals={totals} />}
            </Card>
          </div>
          <MissReasonsByGroup groupBy={groupBy} groups={groups} groupLabel={groupLabel} link={linkInput} loading={loading} plainLinks={plainLinks} />
          {sessionId === null && (
            <CacheByModel
              groups={byModel.stale ? [] : (byModel.data ?? [])}
              funded={fundedModels}
              error={modelError ?? (weekWait ? reading.error : null)}
              unpriced={modelError === null && weekWait && reading.unpriced}
              loading={byModel.data === null || byModel.stale || (funded && (byModelFunded.data === null || byModelFunded.stale)) || weekWait}
              reading={reading}
              unit={unit}
            />
          )}
          {/* The route reads no session, so a session view has no placements of its own to show. */}
          {sessionId === null && (
            <PlacementTable
              error={placements.error}
              rows={placements.stale ? null : placements.data}
              unfiltered={filters.accountId.length > 0 || filters.model.length > 0 || patterns.userAgents.length > 0 || patterns.clients.length > 0}
            />
          )}
        </Section>
      )}

      {tab === 'cost' && (
        <Section id="cost" shown={shown} title="Cost" meta={figureLoading || figureError !== null || costHead === null ? undefined : costHead.meta} metaTitle={costHead?.unpriced ? UNPRICED_TITLE : undefined}>
          <div className="tiles-row">
            <Tile label={costHead?.tile.label ?? 'Cost'} value={figureLoading || costHead === null ? null : figuresValue(costHead.tile.figures)} note={costHead?.tile.note ?? NATIVE_SUBSET} title={costHead?.unpriced ? UNPRICED_TITLE : unit === 'native' ? (tileHint ?? undefined) : undefined} error={figureError} unpriced={figureUnpriced} />
            {unit === 'native' ? (
              <>
                <Tile label="Cost per request" value={figureLoading || nativeRatios === null ? null : figuresValue(nativeRatios.perRequest.figures)} note={nativeRatios === null ? null : nativeRatios.perRequest.note} error={figureError} unpriced={figureUnpriced} />
                <Tile label="Cost per 1k output tokens" value={figureLoading || nativeRatios === null ? null : figuresValue(nativeRatios.perThousand.figures)} note={nativeRatios === null ? null : nativeRatios.perThousand.note} error={figureError} unpriced={figureUnpriced} />
              </>
            ) : (
              <>
                <Tile label="Cost per request" value={costLoading ? null : fmtUsd(costTotals.requests ? costTotals.costUsd / costTotals.requests : 0)} note={`${fmtInt(costTotals.requests)} requests`} error={costError} />
                <Tile label="Cost per 1k output tokens" value={costLoading ? null : fmtUsd(costTotals.tokens.output ? (costTotals.costUsd / costTotals.tokens.output) * 1000 : 0)} note={`${fmtCompact(costTotals.tokens.output)} output tokens`} error={costError} />
              </>
            )}
            <Tile label="Fast mode spend" value={speedError !== null ? '-' : speedLoading ? null : fmtUsd(fastSpend)} note="usage credits, at the fast cells" />
          </div>
          <div className="grid-2">
            <CostByGroup points={costPoints} groupBy={groupBy} groupSlot={groupSlot} loading={figureLoading} error={figureError} unpriced={figureUnpriced} axis={axis} reading={reading} unit={unit} />
            {/* Native draws a ring per funding, each in its own unit. */}
            <Card title="Cost split" subtitle={unit === 'native' ? NATIVE_SPLIT_NOTE : `By token kind, in dollars at the API price list, ${UNIT_SPLIT[dollars]}`}>
              {/* The request summary carries one total and no split, so a session view has nothing to draw here. */}
              {sessionId !== null ? (
                <p className="muted">{SESSION_COST_NOTE}</p>
              ) : figureError !== null ? (
                <ErrorState title={figureUnpriced ? 'Unpriced: cost split' : 'Could not load cost split'} body={figureError} />
              ) : figureLoading ? (
                <Skeleton height={148} />
              ) : unit === 'native' ? (
                scale === null ? <Skeleton height={148} /> : <NativeSplit totals={costTotals} week={week} weekFactor={scale} />
              ) : costSplit === null ? (
                <Skeleton height={148} />
              ) : (
                <CostDonut cost={costSplit} />
              )}
            </Card>
          </div>
          {sessionId === null && unit === 'native' && (
            <BreakdownTable
              className="native-cost-table"
              title={`Cost by ${groupLabel.toLowerCase()}`}
              subtitle={`Totals for the selected range and filters, ${NATIVE_SUBSET}`}
              error={figureError}
              unpriced={figureUnpriced}
              loading={figureLoading}
              groupLabel={groupLabel}
              columns={NATIVE_COLUMNS}
              rows={scale === null ? [] : costGroups.map((g) => nativeRowOf(g, week, scale, rowHint(g)))}
            />
          )}
          {sessionId === null && unit !== 'native' && (
            <BreakdownTable
              title={`Cost by ${groupLabel.toLowerCase()}`}
              subtitle={`Totals for the selected range and filters, ${costHead?.subset ?? ''}`}
              error={costError}
              loading={costLoading}
              groupLabel={groupLabel}
              columns={['Requests', 'Cost', 'Input', 'Cache read', 'Cache write', 'Output', 'Per request', 'Per 1k output']}
              rows={costGroups.map((g) => ({
                key: g.key,
                label: modelGroupLabel(g),
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
            <Tile label="Probe cost" value={probeTile.value} note={probeTile.note} error={figureError} unpriced={figureUnpriced} />
            <Tile label="Saved by probes" value={savingTile.value} note={savingTile.note} error={figureError} unpriced={figureUnpriced} />
            <Tile label="Net saving" value={netTile.value} note={netTile.note} tone={netTile.critical ? 'critical' : undefined} error={figureError} unpriced={figureUnpriced} />
            <Tile label="Kept warm" value={loading ? null : keptWarm === null ? '-' : fmtInt(keptWarm.requests)} note={keptWarm === null ? UNAVAILABLE_NOTE : keptWarmNote(keptWarm)} />
          </div>
          <div className="grid-2 warming-grid">
            <ChartCard title="Probes by outcome" subtitle="Per bucket, stacked" loading={loading} legend={PROBE_SERIES.map((s) => ({ color: ink.series[s.slot], label: s.label, title: s.hint, value: probesUnmeasured ? '-' : fmtInt(totals.probes[s.key]) }))}>
              <BarChart data={buckets} margin={{ top: 8, right: 8, bottom: 0, left: 0 }} barCategoryGap={2}>
                <CartesianGrid vertical={false} stroke={ink.grid} />
                <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
                <YAxis tickLine={false} axisLine={false} width={36} allowDecimals={false} />
                <Tooltip cursor={{ fill: ink.grid, opacity: 0.6 }} content={timeTip<Bucket>((p) => PROBE_SERIES.map((s) => ({ color: ink.series[s.slot], label: s.label, title: s.hint, value: probesUnmeasured ? '-' : fmtInt(p.probes[s.key]) })))} />
                {!probesUnmeasured &&
                  PROBE_SERIES.map((s, i) => (
                    <Bar key={s.key} dataKey={`probes.${s.key}`} stackId="a" fill={ink.series[s.slot]} stroke={ink.surface} strokeWidth={1} maxBarSize={22} radius={i === PROBE_SERIES.length - 1 ? [3, 3, 0, 0] : 0} isAnimationActive={false} />
                  ))}
              </BarChart>
            </ChartCard>

            <Card title="Wasted probes" subtitle={`${WASTED_PROBE_BLURB} ${WASTED_PROBE_UNIT[unit]}`}>
              {figureError !== null ? (
                <ErrorState title={figureUnpriced ? 'Unpriced: wasted probes' : 'Could not load wasted probes'} body={figureError} />
              ) : figureLoading ? (
                <Skeleton height={190} />
              ) : probesUnmeasured ? (
                <p className="muted">{WARMING_UNAVAILABLE}</p>
              ) : (
                <WastedProbes group={costTotals} probes={probesOf(costTotals)} unit={unit} billed={billedTotals} reading={reading} />
              )}
            </Card>

            <SavingChart
              axis={axis}
              billed={billedWarmingPoints}
              error={warmingError ?? (weekWait ? reading.error : null)}
              loading={(sessionId === null ? warmingLoading : loading) || weekWait}
              points={sessionId === null ? warmingPoints : points}
              reading={reading}
              session={sessionId !== null}
              unit={unit}
              unpriced={warmingError === null && weekWait && reading.unpriced}
            />

            <ChartCard title="Kept-warm requests" subtitle="Requests whose cache read followed a probe" loading={loading} legend={[{ color: ink.series[0], label: 'Kept-warm requests', value: keptWarm === null ? '-' : fmtInt(keptWarm.requests) }]}>
              <BarChart data={buckets} margin={{ top: 8, right: 8, bottom: 0, left: 0 }} barCategoryGap={2}>
                <CartesianGrid vertical={false} stroke={ink.grid} />
                <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
                <YAxis tickLine={false} axisLine={false} width={36} allowDecimals={false} />
                <Tooltip cursor={{ fill: ink.grid, opacity: 0.6 }} content={timeTip<Bucket>((p) => [{ color: ink.series[0], label: 'kept-warm requests', value: keptWarm === null || p.keptWarm === null ? '-' : fmtInt(p.keptWarm.requests) }])} />
                {keptWarm !== null && <Bar dataKey="keptWarm.requests" fill={ink.series[0]} maxBarSize={22} radius={[3, 3, 0, 0]} isAnimationActive={false} />}
              </BarChart>
            </ChartCard>

            <ChartCard title="Kept-warm tokens" subtitle="Cache-read tokens on those requests" loading={loading} legend={[{ color: ink.series[2], label: 'Kept-warm tokens', value: keptWarm === null ? '-' : fmtCompact(keptWarmTokens(keptWarm)) }]}>
              <BarChart data={buckets} margin={{ top: 8, right: 8, bottom: 0, left: 0 }} barCategoryGap={2}>
                <CartesianGrid vertical={false} stroke={ink.grid} />
                <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
                <YAxis tickFormatter={(v: number) => fmtCompact(v)} tickLine={false} axisLine={false} width={44} />
                <Tooltip cursor={{ fill: ink.grid, opacity: 0.6 }} content={timeTip<Bucket>((p) => [{ color: ink.series[2], label: 'kept-warm tokens', value: keptWarm === null || p.keptWarm === null ? '-' : fmtCompact(keptWarmTokens(p.keptWarm)) }])} />
                {keptWarm !== null && <Bar dataKey={(p: Bucket) => (p.keptWarm === null ? null : keptWarmTokens(p.keptWarm))} fill={ink.series[2]} maxBarSize={22} radius={[3, 3, 0, 0]} isAnimationActive={false} />}
              </BarChart>
            </ChartCard>
          </div>
          <KeepaliveByPool
            groups={poolGroups}
            funded={fundedPools}
            error={poolError ?? (weekWait ? reading.error : null)}
            unpriced={poolError === null && weekWait && reading.unpriced}
            loading={(sessionId === null ? byPool.data === null || byPool.stale || (poolFunding !== undefined && (fundedByPool.data === null || fundedByPool.stale)) : loading) || weekWait}
            reading={reading}
            unit={unit}
          />
        </Section>
      )}

      {tab === 'accounts' && (
        <Section id="accounts" shown={shown} title="Accounts" meta="Peak utilization, window usage, pace, ineligible time, and pin churn per account">
          <AccountsSection
            accountIds={filters.accountId}
            allowance={{ answer: unusedAllowance.stale ? null : unusedAllowance.data, costs: costSettings.data?.subscriptionCosts ?? null, error: unusedAllowance.error ?? costSettings.error }}
            bucketMs={bucketMs}
            error={accountRollups.error}
            loading={accountRollups.data === null || accountRollups.stale}
            otherFilters={otherFilters}
            rollups={accountRollups.stale ? null : accountRollups.data}
            slotOf={slotOf}
            span={span}
            axis={axis}
            week={week}
          />
        </Section>
      )}
    </div>
  );
}

const pct100 = (v: number | null) => (v === null ? null : v * 100);

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

/** `metaTitle` is the meta line's hover text, such as the estimate's sentence for a marked figure. */
function Section({ id, title, meta, metaTitle, controls, shown = true, children }: { children: ReactNode; controls?: ReactNode; id: SectionId; meta?: string; metaTitle?: string; shown?: boolean; title: string }) {
  if (!shown) return null;
  return (
    <section className="analytics-section" id={id} role="tabpanel" aria-labelledby={`tab-${id}`}>
      <header className="analytics-section__head">
        <h2 className="analytics-section__title">{title}</h2>
        {meta && (
          <span className="analytics-section__meta" title={metaTitle}>
            {meta}
          </span>
        )}
        {controls}
      </header>
      {children}
    </section>
  );
}

/**
 * A probe figure's two sides under native: the plan probes' Pro-week figure off the whole read and the billed probes'
 * dollars off the billed read, told apart by the counts of their answered and failed probes (`upstreamProbes`), since a
 * held or skipped probe costs nothing. `undefined` leaves out a side with no such probe, null reads a dash where the
 * plan probes priced nothing, and a group with no such probe on either side reads its zero in each unit, as an empty
 * range does.
 */
function probeSidesOf(whole: AnalyticsGroup, billed: AnalyticsGroup | null | undefined, read: 'cost' | 'wasted'): { billed: number | undefined; equivalent: number | null | undefined } {
  const billedProbes = billed ? upstreamProbes(billed) : 0;
  const planProbes = upstreamProbes(whole) - billedProbes;
  const priced = whole.probeEquivalent === null ? null : whole.probeEquivalent[read];
  const dollars = billed ? (read === 'cost' ? billed.probeCostUsd : billed.wastedProbeUsd) : 0;
  if (planProbes <= 0 && billedProbes <= 0) return { billed: dollars, equivalent: priced ?? 0 };
  return { billed: billedProbes > 0 ? dollars : undefined, equivalent: planProbes > 0 ? priced : undefined };
}
const probeFiguresOf = (sides: ReturnType<typeof probeSidesOf>, week: WeekPosition, weekFactor: number): CostFigure[] => [
  ...(sides.equivalent === undefined ? [] : [{ text: equivalentOrDash(sides.equivalent, week, weekFactor), word: 'subscription' }]),
  ...(sides.billed === undefined ? [] : [{ text: fmtUsd(sides.billed), word: 'billed' }]),
];

/**
 * The saving's two sides under native, told apart by their kept-warm credits, the plan side's the whole read's less the
 * billed read's: the plan credits' Pro-week saving off the whole read and the billed credits' dollars off `billed`.
 * `net` reads each side's saving less its probes' cost, so a side stands there on a credit or on an answered or failed
 * probe, and a side whose probes kept nothing warm nets their cost under zero. A held or skipped probe costs
 * nothing, so it leaves its side's saving whole. `undefined` leaves a side out, null reads a dash where the plan rows
 * priced nothing, and a group with nothing on either side reads its zero in each unit.
 */
function savingSidesOf(whole: AnalyticsGroup, billed: AnalyticsGroup | null | undefined, read: 'gross' | 'net'): ReturnType<typeof probeSidesOf> {
  const credits = (g: AnalyticsGroup | null | undefined) => (g?.keptWarm ? keptWarmTokens(g.keptWarm) : 0);
  const billedCredits = credits(billed);
  const planCredits = credits(whole) - billedCredits;
  const billedProbes = billed ? upstreamProbes(billed) : 0;
  const planProbes = upstreamProbes(whole) - billedProbes;
  const net = read === 'net';
  const planSaving = planCredits <= 0 ? 0 : whole.savingEquivalent;
  const planCost = !net || planProbes <= 0 ? 0 : (whole.probeEquivalent?.cost ?? null);
  const plan = planCredits > 0 || (net && planProbes > 0);
  const paid = billedCredits > 0 || (net && billedProbes > 0);
  const equivalent = planSaving === null || planCost === null ? null : netSaving(planSaving, planCost);
  const dollars = netSaving(billed?.savingUsd ?? 0, net ? (billed?.probeCostUsd ?? 0) : 0);
  if (!plan && !paid) return { billed: dollars, equivalent: equivalent ?? 0 };
  return { billed: paid ? dollars : undefined, equivalent: plan ? equivalent : undefined };
}

/**
 * The Probe cost tile per position: under native the plan probes' figure in the chosen week of the whole read stacked
 * over the billed probes' dollars of `billed`, the skeleton while the scale is out, and under a dollar position the
 * position's probe dollars. A session view folds request rows, which measure no probe count, so `session` reads the
 * count unmeasured on every path. The tile sums the probes of every account it reads, so it carries no hint.
 */
function probeTileOf(unit: UnitPosition, totals: AnalyticsGroup, billed: AnalyticsGroup | null, loading: boolean, session: boolean, reading: WeekReading): { note: string; value: ReactNode | null } {
  const unmeasured = session || totals.fromRequests === true;
  if (unmeasured) return { note: UNAVAILABLE_NOTE, value: loading ? null : '-' };
  if (unit === 'native') {
    const sides = probeSidesOf(totals, billed, 'cost');
    return { note: `${fmtInt(probesOf(totals))} probes, ${NATIVE_SUBSET}`, value: loading || reading.scale === null ? null : figuresValue(probeFiguresOf(sides, reading.week, reading.scale)) };
  }
  return { note: `${fmtInt(probesOf(totals))} probes, ${UNIT_SPLIT[dollarsOf(unit)]}`, value: loading ? null : fmtUsd(totals.probeCostUsd) };
}

const keptWarmRequests = (g: AnalyticsGroup | null) => `${fmtInt(g?.keptWarm?.requests ?? 0)} requests kept warm`;

/**
 * The saving tile's figures per position, shaped as probeTileOf's: under native the plan rows' figure in the chosen
 * week over the billed dollars, under all the dollars, under usd the billed subset, under equivalent the equivalent.
 * The note leads with the credited requests, and a session view, whose fold carries no credit, reads the figure
 * unmeasured.
 */
function savingTileOf(unit: UnitPosition, totals: AnalyticsGroup, billed: AnalyticsGroup | null, loading: boolean, session: boolean, reading: WeekReading): { note: string; value: ReactNode | null } {
  if (session || totals.fromRequests === true) return { note: UNAVAILABLE_NOTE, value: loading ? null : '-' };
  if (unit === 'native') {
    const sides = savingSidesOf(totals, billed, 'gross');
    return { note: `${keptWarmRequests(totals)}, ${NATIVE_SUBSET}`, value: loading || reading.scale === null ? null : figuresValue(probeFiguresOf(sides, reading.week, reading.scale)) };
  }
  return { note: `${keptWarmRequests(totals)}, ${UNIT_SPLIT[dollarsOf(unit)]}`, value: loading ? null : fmtUsd(totals.savingUsd) };
}

/**
 * The Net saving tile, savingTileOf's shape over the saving less the probe cost per side, `critical` while the figure
 * is under zero. Under native that is the plan side's figure, the widget's subject, and the billed dollars beside it
 * never tone the tile.
 */
function netTileOf(unit: UnitPosition, totals: AnalyticsGroup, billed: AnalyticsGroup | null, loading: boolean, session: boolean, reading: WeekReading): { critical: boolean; note: string; value: ReactNode | null } {
  const less = (subset: string) => `less the probe cost, ${subset}`;
  if (session || totals.fromRequests === true) return { critical: false, note: UNAVAILABLE_NOTE, value: loading ? null : '-' };
  if (unit === 'native') {
    const sides = savingSidesOf(totals, billed, 'net');
    const shown = !loading && reading.scale !== null;
    return { critical: shown && typeof sides.equivalent === 'number' && sides.equivalent < 0, note: less(NATIVE_SUBSET), value: shown && reading.scale !== null ? figuresValue(probeFiguresOf(sides, reading.week, reading.scale)) : null };
  }
  const net = netSaving(totals.savingUsd, totals.probeCostUsd);
  return { critical: !loading && net < 0, note: less(UNIT_SPLIT[dollarsOf(unit)]), value: loading ? null : fmtUsd(net) };
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
  actions,
  unpriced = false,
}: {
  /** Drawn in the card's header beside the title: a control the chart reads. */
  actions?: ReactNode;
  children: ReactNode;
  /** Drawn in place of the chart, legend and all, when the answer holds nothing the chart could draw. */
  empty?: ReactNode;
  /** A card reading a route of its own draws that route's failure in place of the chart. */
  error?: string | null;
  footer?: ReactNode;
  height?: number;
  legend?: { color: string; dashed?: boolean; hollow?: boolean; label: string; shape?: 'line' | 'rect'; title?: string; value?: ReactNode }[];
  loading: boolean;
  subtitle?: ReactNode;
  title: string;
  /** `error` is the week reading's unpriced sentence, which no retry clears, so the state heads it unpriced. */
  unpriced?: boolean;
}) {
  return (
    <Card title={title} subtitle={subtitle} actions={actions}>
      {error !== null ? (
        <ErrorState title={unpriced ? `Unpriced: ${title.toLowerCase()}` : `Could not load ${title.toLowerCase()}`} body={error} />
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
  className = '',
  error = null,
  loading = false,
  subtitle = 'Totals for the selected range and filters',
  unpriced = false,
}: {
  className?: string;
  columns: ReactNode[];
  error?: string | null;
  groupLabel: string;
  loading?: boolean;
  /** A row's `title` is its label cell's hover text, the label itself where none is given. */
  rows: { cells: ReactNode[]; key: string; label: string; title?: string }[];
  subtitle?: string;
  title: string;
  /** `error` is the week reading's unpriced sentence, which no retry clears, so the state heads it unpriced. */
  unpriced?: boolean;
}) {
  return (
    <Card title={title} subtitle={subtitle} padded={false} className={className ? `card--table ${className}` : 'card--table'}>
      <div className="table-wrap">
        <table className="table">
          <thead>
            <tr>
              <th className="col-fill">{groupLabel}</th>
              {columns.map((c, i) => (
                <th key={i} className="right col-fit">
                  {c}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {/* A table that reads a second answer beside its rows' own, a dollar side's cells say, draws that answer's
                failure in place of the rows, so no cell stands without the figure it was read for. */}
            {error === null &&
              rows.map((r) => (
                <tr key={r.key}>
                  <td className="mono col-fill" title={r.title ?? r.label}>
                    {r.label}
                  </td>
                  {r.cells.map((c, i) => (
                    <td key={i} className="right num col-fit">
                      {c}
                    </td>
                  ))}
                </tr>
              ))}
            {(error !== null || rows.length === 0) && (
              <tr>
                <td colSpan={columns.length + 1} className={error ? '' : 'muted'}>
                  {error ? <ErrorState title={unpriced ? `Unpriced: ${title.toLowerCase()}` : 'Could not load this breakdown'} body={error} /> : loading ? <Skeleton height={22} /> : 'No rows.'}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
    </Card>
  );
}

/**
 * Horizontal bars of counts in the order given, each with its share of the total and, where its row carries a target,
 * its count linking the requests it counts, and `empty` when every count is zero.
 */
function CountBars({ color, empty, rows }: { color: string; empty: string; rows: { count: number; label: string; target?: RequestsLinkTarget }[] }) {
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
            {r.target ? <CountLink target={r.target}>{fmtInt(r.count)}</CountLink> : fmtInt(r.count)} <span className="muted">{fmtPct(r.count / total)}</span>
          </span>
        </li>
      ))}
    </ul>
  );
}

function ErrorBars({ errors, color, empty = 'No errors in range.' }: { color: string; empty?: string; errors: Record<string, number> }) {
  const rows = Object.entries(errors)
    .map(([label, count]) => ({ count, label }))
    .sort((a, b) => b.count - a.count || a.label.localeCompare(b.label));
  return <CountBars color={color} empty={empty} rows={rows} />;
}

/** A count under several client patterns stays plain, since a link could carry one of them alone. */
const SEVERAL_CLIENTS: RequestsLinkTarget = { href: null, title: CLIENT_PATTERNS_PLAIN };

/** One bar per category in the route's order, `-` read `none`, each count linking its category's refused answers. */
const refusalRows = (answer: RefusalCategoriesAnswer, input: ReasonLink, plainLinks: boolean) =>
  answer.rows.map((r) => ({ count: r.requests, label: r.category === '-' ? 'none' : r.category, target: plainLinks ? SEVERAL_CLIENTS : refusalLink(input, r.category, r.requests) }));

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

/**
 * A request not asked and a comparison still running read as neutral, since neither has a finished comparison behind
 * it. Every other reason but no divergence reads as a fault, like the error bars above: a diagnosed divergence, a
 * missing previous message, an unavailable comparison and a name the console has no label for.
 */
function reasonColor(ink: ReturnType<typeof useChartInk>, reason: CacheMissReason): string {
  if (reason === 'none') return ink.series[2];
  if (reason === 'not_requested' || reason === 'pending') return ink.muted;
  return ink.series[7];
}

/**
 * What every count of the reasons panel and the refusals card links under: the window and the page's filters, without
 * a count, a reason or a category.
 */
type ReasonLink = Omit<RequestsLinkInput, 'count' | 'group' | 'reason' | 'refusalCategory'>;

/**
 * A count of the reasons panel or the refusals card: a link to the requests it counts, else the figure with why it
 * stays plain.
 */
function CountLink({ children, target }: { children: ReactNode; target: RequestsLinkTarget }) {
  return target.href === null ? (
    <span title={target.title}>{children}</span>
  ) : (
    <Link to={target.href} title={target.title}>
      {children}
    </Link>
  );
}

function ReasonBars({ link, plainLinks, totals }: { link: ReasonLink; plainLinks: boolean; totals: AnalyticsGroup }) {
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
            <CountLink target={plainLinks ? SEVERAL_CLIENTS : requestsLink({ ...link, count: r.count, reason: r.reason })}>{fmtInt(r.count)}</CountLink>{' '}
            <span className="muted">{fmtPct(totals.requests ? r.count / totals.requests : 0, 1)}</span>
          </span>
        </li>
      ))}
    </ul>
  );
}

const WASTED_PROBE_BLURB =
  "Probes no real request came back for, settled when the recipe stopped, lapsed or was evicted, or when the request after them read no cache. A high idle-limit share says the pool's idle window outlasts how these sessions are used. A high parked-limit or horizon share says the parked ceilings do.";
/** The Wasted probes blurb's closing sentence, the unit its cost tiles read in under each position. */
const WASTED_PROBE_UNIT: Record<UnitPosition, string> = {
  all: 'In dollars at list prices.',
  equivalent: 'In dollars at list prices.',
  native: "In the unit each probe's funding pays in.",
  usd: 'In dollars at list prices.',
};

type ProbeTile = { note?: string; value: ReactNode };
/**
 * The Wasted cost and Share of probe cost tiles per position. Native stacks the wasted plan probes' figure in the chosen
 * week over the wasted billed probes' dollars and reads the share off the plan side, the billed side's where no plan
 * probe ran, and their skeletons while the scale is out. A dollar position reads its own rows' dollars.
 */
function wastedTilesOf(unit: UnitPosition, group: AnalyticsGroup, billed: AnalyticsGroup | null, reading: WeekReading): { cost: ProbeTile; share: ProbeTile } {
  if (unit === 'native') {
    const { scale: weekFactor, week } = reading;
    if (weekFactor === null) return { cost: { value: null }, share: { value: null } };
    const wasted = probeSidesOf(group, billed, 'wasted');
    const cost = probeSidesOf(group, billed, 'cost');
    const ratio = (w: number | null | undefined, c: number | null | undefined) => (w === null || w === undefined || c === null || c === undefined ? null : c ? w / c : 0);
    const share = cost.equivalent === undefined ? ratio(wasted.billed, cost.billed) : ratio(wasted.equivalent, cost.equivalent);
    const of = probeFiguresOf(cost, week, weekFactor).map((f) => f.text);
    return {
      cost: { note: NATIVE_SUBSET, value: figuresValue(probeFiguresOf(wasted, week, weekFactor)) },
      share: { note: `of ${of.join(' and ')} on probes`, value: share === null ? '-' : fmtPct(share, 1) },
    };
  }
  const dollars = dollarsOf(unit);
  return {
    cost: { note: dollars === 'all' ? 'at list prices' : `at list prices, ${UNIT_SPLIT[dollars]}`, value: fmtUsd(group.wastedProbeUsd) },
    share: { note: `of ${fmtUsd(group.probeCostUsd)} on probes`, value: fmtPct(group.probeCostUsd ? group.wastedProbeUsd / group.probeCostUsd : 0, 1) },
  };
}

/** `group` is the position's rows (the whole under native), `billed` native's billed read. */
function WastedProbes({ billed, group, probes, reading, unit }: { billed: AnalyticsGroup | null; group: AnalyticsGroup; probes: number; reading: WeekReading; unit: UnitPosition }) {
  const ink = useChartInk();
  const rows = WASTED_PROBE_REASONS.map((reason) => ({ count: group.wastedProbeReasons[reason], reason })).sort((a, b) => b.count - a.count);
  const max = Math.max(1, ...rows.map((r) => r.count));
  if (group.wastedProbes === 0) return <p className="muted">No wasted probes in range.</p>;
  const tiles = wastedTilesOf(unit, group, billed, reading);
  return (
    <>
      <div className="tiles-row tiles-row--panel">
        <Tile label="Wasted probes" value={fmtInt(group.wastedProbes)} note={`of ${fmtInt(probes)} probes`} />
        <Tile label="Wasted cost" value={tiles.cost.value} note={tiles.cost.note} />
        <Tile label="Share of probe cost" value={tiles.share.value} note={tiles.share.note} />
      </div>
      <ul className="hbars hbars--reasons hbars--under-tiles">
        {rows.map((r) => (
          // The longest reason wraps in this column, so the row anchors the bar and the count to its first line.
          <li key={r.reason} className="hbars__row--top">
            <span>{WASTED_PROBE_LABEL[r.reason]}</span>
            <div className="hbars__track">
              <span className="hbars__fill" style={{ width: `${(r.count / max) * 100}%`, background: r.reason === 'idle_limit' ? ink.series[3] : ink.muted }} />
            </div>
            <span className="num">
              {fmtInt(r.count)} <span className="muted">{fmtPct(group.wastedProbes ? r.count / group.wastedProbes : 0, 1)}</span>
            </span>
          </li>
        ))}
      </ul>
    </>
  );
}

/**
 * A side pair in one line, the plan side's figure in the chosen week and the billed side's dollars, as the native
 * tooltip reads it.
 */
const sidesText = (s: ReturnType<typeof probeSidesOf>, week: WeekPosition, weekFactor: number): string => [...(s.equivalent === undefined ? [] : [equivalentOrDash(s.equivalent, week, weekFactor)]), ...(s.billed === undefined ? [] : [`${fmtUsd(s.billed)} billed`])].join(', ');
/** A figure as the chart prints it: a side with no row reads its zero, and a side priced nothing a dash. */
const chartEquivalent = (s: ReturnType<typeof probeSidesOf>, week: WeekPosition, weekFactor: number): string => equivalentOrDash(s.equivalent === undefined ? 0 : s.equivalent, week, weekFactor);

/**
 * The Saved by probes chart: two bars per bucket, the gross saving and the probe cost, each bucket's net in the
 * tooltip, and the range's gross, cost and net in the legend, the net in the critical ink under zero. `points` is the
 * series that counts probes under the position's funding and `billed` native's billed twin. The dollar positions draw
 * dollars. Native draws the plan side's Pro-week figures, read in the chosen week, with the billed dollars beside them
 * in the tooltip, as the Probe cost tile pairs them, and the billed dollars alone where the range holds no plan side,
 * as the cache cost card does. A session view's fold carries no credit, so `session` draws no bar and dashes the
 * legend.
 */
function SavingChart({ axis, billed, error, loading, points, reading, session, unit, unpriced }: { axis: TimeAxis; billed: AnalyticsPoint[] | null; error: string | null; loading: boolean; points: AnalyticsPoint[]; reading: WeekReading; session: boolean; unit: UnitPosition; unpriced: boolean }) {
  const ink = useChartInk();
  const buckets = useMemo(() => bucketsOf(points), [points]);
  const billedBuckets = useMemo(() => (billed === null ? null : bucketsOf(billed)), [billed]);
  const billedAt = useMemo(() => new Map((billedBuckets ?? []).map((b) => [b.at, b])), [billedBuckets]);
  const totals = useMemo(() => totalsOf(points.flatMap((p) => p.groups)), [points]);
  const billedTotals = useMemo(() => (billed === null ? null : totalsOf(billed.flatMap((p) => p.groups))), [billed]);
  const native = unit === 'native';
  const range = { cost: probeSidesOf(totals, billedTotals, 'cost'), gross: savingSidesOf(totals, billedTotals, 'gross'), net: savingSidesOf(totals, billedTotals, 'net') };
  const equivalents = native && range.net.equivalent !== undefined;
  const title = 'Saved by probes';
  const subtitle = "Per bucket, the gross saving against the probes' cost";
  // The figures wait on the scale, so the card draws its skeleton while it is out.
  const { scale: weekFactor, week } = reading;
  if (equivalents && weekFactor === null) return <ChartCard title={title} subtitle={subtitle} loading error={error} unpriced={unpriced}>{null}</ChartCard>;
  // The dollar figures read no scale, and under the equivalents the return above leaves one.
  const factor = weekFactor ?? 1;
  const drawn = native && !equivalents ? (billedBuckets ?? []) : buckets;
  const dollars = native && !equivalents ? (billedTotals ?? totals) : totals;
  const net = equivalents ? (range.net.equivalent === undefined ? 0 : range.net.equivalent) : netSaving(dollars.savingUsd, dollars.probeCostUsd);
  const negative = !session && net !== null && net < 0;
  const figures = session
    ? { cost: '-', gross: '-', net: '-' }
    : equivalents
      ? { cost: chartEquivalent(range.cost, week, factor), gross: chartEquivalent(range.gross, week, factor), net: chartEquivalent(range.net, week, factor) }
      : { cost: fmtUsd(dollars.probeCostUsd), gross: fmtUsd(dollars.savingUsd), net: fmtUsd(net ?? 0) };
  // A bucket that priced nothing draws no height.
  const gross = (p: Bucket) => (equivalents ? (p.savingEquivalent ?? 0) : p.savingUsd);
  const cost = (p: Bucket) => (equivalents ? (p.probeEquivalent?.cost ?? 0) : p.probeCostUsd);
  const tip = (p: Bucket): TooltipRow[] => {
    const rows = (values: [string, string, string]) => [
      { color: ink.series[1], label: 'saved', value: values[0] },
      { color: ink.series[3], label: 'probe cost', value: values[1] },
      { label: 'net', value: values[2] },
    ];
    if (session) return rows(['-', '-', '-']);
    if (!equivalents) return rows([fmtUsd(p.savingUsd), fmtUsd(p.probeCostUsd), fmtUsd(netSaving(p.savingUsd, p.probeCostUsd))]);
    const paid = billedAt.get(p.at) ?? null;
    return rows([sidesText(savingSidesOf(p, paid, 'gross'), week, factor), sidesText(probeSidesOf(p, paid, 'cost'), week, factor), sidesText(savingSidesOf(p, paid, 'net'), week, factor)]);
  };
  return (
    <ChartCard
      title={title}
      subtitle={subtitle}
      loading={loading}
      error={error}
      unpriced={unpriced}
      legend={[
        { color: ink.series[1], label: 'Saved by probes', value: figures.gross },
        { color: ink.series[3], label: 'Probe cost', value: figures.cost },
        { color: ink.muted, hollow: true, label: 'Net saving', value: negative ? <span className="is-negative">{figures.net}</span> : figures.net },
      ]}
    >
      <BarChart data={drawn} margin={{ top: 8, right: 8, bottom: 0, left: 0 }} barCategoryGap={4} barGap={2}>
        <CartesianGrid vertical={false} stroke={ink.grid} />
        <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
        {equivalents ? <YAxis tickFormatter={(v: number) => fmtShare(v * factor)} tickLine={false} axisLine={false} width={60} /> : <YAxis {...usdAxis(drawn.flatMap((b) => [b.savingUsd, b.probeCostUsd]))} tickLine={false} axisLine={false} />}
        <Tooltip cursor={{ fill: ink.grid, opacity: 0.6 }} content={timeTip<Bucket>(tip)} />
        {!session && <Bar dataKey={gross} fill={ink.series[1]} maxBarSize={22} radius={[3, 3, 0, 0]} isAnimationActive={false} />}
        {!session && <Bar dataKey={cost} fill={ink.series[3]} maxBarSize={22} radius={[3, 3, 0, 0]} isAnimationActive={false} />}
      </BarChart>
    </ChartCard>
  );
}

/** A raw reason name with a break opportunity after each underscore, so a header holding one wraps at its words. */
const breakable = (name: string): ReactNode => name.split(/(?<=_)/).flatMap((part, i) => (i === 0 ? [part] : [<wbr key={i} />, part]));

function MissReasonsByGroup({ groupBy, groups, groupLabel, link, loading, plainLinks }: { groupBy: GroupBy; groupLabel: string; groups: AnalyticsGroup[]; link: ReasonLink; loading: boolean; plainLinks: boolean }) {
  const order = missReasonOrder(groups);
  const cell = (g: AnalyticsGroup, reason: CacheMissReason) => {
    const count = missCount(g.cacheMissReasons, reason);
    return <CountLink target={plainLinks ? SEVERAL_CLIENTS : requestsLink({ ...link, count, group: { by: groupBy, key: g.key }, reason })}>{fmtInt(count)}</CountLink>;
  };
  return (
    <BreakdownTable
      title={`Misses by reason by ${groupLabel.toLowerCase()}`}
      className="misses-table"
      loading={loading}
      groupLabel={groupLabel}
      columns={[
        ...order.map((r) => (
          <span key={r} title={missLabel(r)}>
            {breakable(missShort(r))}
          </span>
        )),
        'Expired between turns',
      ]}
      rows={groups.map((g) => ({
        key: g.key,
        label: modelGroupLabel(g),
        // Expired between turns is no reason the Requests page filters by, so it stays plain.
        cells: [...order.map((reason) => cell(g, reason)), fmtInt(g.expiredBetweenTurns)],
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
        subtitle="p50, p90, p95, and p99 per bucket, with p99 emphasized"
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

/**
 * Cache by model under a position: the token columns read every row, and the Read cost and Write cost cells read the
 * position's side. Native pairs each model's plan rows' figure in the chosen week with its billed rows' dollars in each
 * cell, and a dollar side reads its own read by model (`funded`, by key). The cells sum every account a model ran on,
 * so they carry no hint, and the table reads no rows while the scale is out, which its loading draws.
 */
function CacheByModel({ error, funded, groups, loading, reading, unit, unpriced }: { error: string | null; funded: Map<string, AnalyticsGroup>; groups: AnalyticsGroup[]; loading: boolean; reading: WeekReading; unit: UnitPosition; unpriced: boolean }) {
  // A request refused before it resolved a model reads no cache, so the group without a model has no row here.
  const rows = useMemo(() => groups.filter((g) => g.key !== '').sort((a, b) => b.tokens.cacheRead - a.tokens.cacheRead), [groups]);
  const subtitle = unit === 'native' ? `Totals for the selected range and filters, ${NATIVE_SUBSET}` : `Totals for the selected range and filters, ${UNIT_SUBSET[dollarsOf(unit)]}`;
  const { scale: weekFactor, week } = reading;
  const costsOf = (g: AnalyticsGroup, factor: number): ReactNode[] => {
    if (unit === 'native') {
      const sides = sidesOf(g);
      // Null where no plan row priced, which reads a dash on the plan side.
      const priced = g.cacheEquivalent;
      const cell = (side: 'read' | 'write', billed: number | null) => nativeCell(nativeCellOf(sides.equivalent === undefined ? undefined : (priced?.[side] ?? null), sides.billed === undefined ? undefined : billed, week, factor));
      return [cell('read', g.billed.cacheRead), cell('write', g.billed.cacheWrite)];
    }
    // A model the side has no row of spent nothing on that side.
    const side = unit === 'all' ? g : (funded.get(g.key) ?? null);
    return side === null ? [fmtUsd(0), fmtUsd(0)] : [usdOrDash(side.cost.cacheRead), usdOrDash(side.cost.cacheWrite)];
  };
  // A dollar position reads no scale.
  const factor = unit === 'native' ? weekFactor : 1;
  return (
    <BreakdownTable
      title="Cache by model"
      subtitle={subtitle}
      error={error}
      unpriced={unpriced}
      loading={loading}
      groupLabel="Model"
      columns={['Reads', 'Writes 5m', 'Writes 1h', 'Hit ratio', 'Cached share', 'Read cost', 'Write cost']}
      rows={
        factor === null
          ? []
          : rows.map((g) => ({
              key: g.key,
              label: g.label,
              title: g.label,
              cells: [fmtCompact(g.tokens.cacheRead), fmtCompact(g.tokens.cacheWrite5m), fmtCompact(g.tokens.cacheWrite1h), fmtPct(hitRatioOf(g) ?? 0), fmtPct(cachedShareOf(g) ?? 0), ...costsOf(g, factor)],
            }))
      }
    />
  );
}

/**
 * The Probe cost and Saved cells follow the position as their tiles do: native pairs the pool's plan side's figure in
 * the chosen week with its billed side's dollars (`funded`, the billed read by pool), and a dollar side reads its own
 * read by pool. The other columns read every row, and the table reads no rows while the scale is out.
 */
function KeepaliveByPool({ error, funded, groups, loading, reading, unit, unpriced }: { error: string | null; funded: Map<string, AnalyticsGroup>; groups: AnalyticsGroup[]; loading: boolean; reading: WeekReading; unit: UnitPosition; unpriced: boolean }) {
  const rows = useMemo(() => [...groups].sort((a, b) => (b.keptWarm === null ? 0 : keptWarmTokens(b.keptWarm)) - (a.keptWarm === null ? 0 : keptWarmTokens(a.keptWarm))), [groups]);
  const { scale: weekFactor, week } = reading;
  const probeCost = (g: AnalyticsGroup, factor: number): ReactNode => {
    if (unit === 'native') {
      const sides = probeSidesOf(g, funded.get(g.key), 'cost');
      return nativeCell(nativeCellOf(sides.equivalent, sides.billed, week, factor));
    }
    return fmtUsd(unit === 'all' ? g.probeCostUsd : (funded.get(g.key)?.probeCostUsd ?? 0));
  };
  const saved = (g: AnalyticsGroup, factor: number): ReactNode => {
    if (unit === 'native') {
      const sides = savingSidesOf(g, funded.get(g.key), 'gross');
      return nativeCell(nativeCellOf(sides.equivalent, sides.billed, week, factor));
    }
    return fmtUsd(unit === 'all' ? g.savingUsd : (funded.get(g.key)?.savingUsd ?? 0));
  };
  // A dollar position reads no scale.
  const factor = unit === 'native' ? weekFactor : 1;
  return (
    <BreakdownTable
      title="Cache warming by pool"
      error={error}
      unpriced={unpriced}
      loading={loading}
      groupLabel="Pool"
      columns={['Probes OK', 'Held', 'Skipped', 'Failed', 'Kept-warm requests', 'Kept-warm tokens', 'Probe cost', 'Saved']}
      rows={
        factor === null
          ? []
          : rows.map((g) => ({
              key: g.key,
              label: g.label,
              // A group folded from request rows measured none of these.
              cells: [
                g.fromRequests ? '-' : fmtInt(g.probes.ok),
                g.fromRequests ? '-' : fmtInt(g.probes.held),
                g.fromRequests ? '-' : fmtInt(g.probes.skipped),
                g.fromRequests ? '-' : fmtInt(g.probes.failed),
                g.keptWarm === null ? '-' : fmtInt(g.keptWarm.requests),
                g.keptWarm === null ? '-' : fmtCompact(keptWarmTokens(g.keptWarm)),
                g.fromRequests ? '-' : probeCost(g, factor),
                g.fromRequests ? '-' : saved(g, factor),
              ],
            }))
      }
    />
  );
}

/**
 * One ring of the cost by token kind. `format` prints each kind's figure in the ring's unit and the center's total
 * unless `center` prints it shorter, `label` names the ring at its center, and `title` is the ring's hover text.
 * `figureTitle` is a kind's figure's own hover text, which masks `title` over the figure.
 */
function CostDonut({ center, cost, figureTitle, format = fmtUsd, label = 'cost', title }: { center?: (v: number) => string; cost: CostSplit; figureTitle?: (v: number) => string; format?: (v: number) => string; label?: string; title?: string }) {
  const ink = useChartInk();
  const [hover, setHover] = useState<string | null>(null);
  const total = COST_PARTS.reduce((s, p) => s + cost[p.key], 0);
  return (
    <div className="split-row" title={title}>
      <Donut slices={COST_PARTS.map((p) => ({ id: p.key, label: p.label, value: cost[p.key], color: ink.series[p.slot] }))} total={total} centerLabel={label} centerValue={(center ?? format)(total)} onHover={setHover} />
      <ul className="remaining__list">
        {COST_PARTS.map((p) => (
          <li key={p.key} className={hover && hover !== p.key ? 'is-dim' : ''}>
            <span className="legend__swatch" style={{ background: ink.series[p.slot] }} />
            <span>{p.label}</span>
            <span className="num" title={figureTitle?.(cost[p.key])}>
              {format(cost[p.key])} <span className="muted">{fmtPct(total ? cost[p.key] / total : 0)}</span>
            </span>
          </li>
        ))}
      </ul>
    </div>
  );
}

/**
 * Native's cost split: the plan rows' figure by token kind in the chosen week beside the billed dollars by token kind,
 * a side with no rows left out. The subscription ring's center reads a bare percent, since the card's subtitle names
 * the week, so the figure fits inside the ring, and its list names the week in each kind's short form, with the full
 * form on the hover, since each row also reads the kind's proportion of the ring as a bare percent. Plan rows the route
 * priced nothing on draw no ring, and with no billed ring beside it the card reads a dash.
 */
function NativeSplit({ totals, week, weekFactor }: { totals: AnalyticsGroup; week: WeekPosition; weekFactor: number }) {
  const sides = sidesOf(totals);
  const split = sides.equivalent === undefined || sides.equivalent === null ? null : totals.equivalentSplit;
  const subscription = split === null ? null : { cacheRead: split.cacheRead * weekFactor, cacheWrite: split.cacheWrite * weekFactor, input: split.input * weekFactor, output: split.output * weekFactor };
  const billed = sides.billed === undefined ? null : knownSplit(totals.billed);
  const rings = [
    ...(subscription === null ? [] : [<CostDonut key="subscription" cost={subscription} center={(v) => fmtShare(v)} figureTitle={(v) => fmtWeek(v, week)} format={(v) => weekShort(v, week)} label="subscription" />]),
    ...(billed === null ? [] : [<CostDonut key="billed" cost={billed} label="billed" />]),
  ];
  if (rings.length === 0) return sides.equivalent === null ? <p className="muted">-</p> : <Skeleton height={148} />;
  return rings.length === 1 ? rings[0] : <div className="split-pair">{rings}</div>;
}

const MAX_STACKED_GROUPS = 7;

type Stack = { data: Record<string, number | null>[]; keys: string[]; labels: Map<string, string>; unpriced: { buckets: number; labels: string[] } };
/**
 * The groups stacked by one figure per bucket: the seven with the largest total in range and the rest as Other. A null
 * figure is one the route priced nothing on: it leaves its stack's bucket a gap, a dash in the tooltip, since a partial
 * sum would read as the whole. `drawn` keeps only the groups whose figure is above zero somewhere in range or unpriced
 * somewhere, for a side that holds some of the page's groups, so a group priced nothing throughout stays listed with
 * its gaps. `unpriced` names the drawn groups that `flagged` reads unpriced in a bucket, a group folded into Other among
 * them, by their labels in the chart's order, and counts the buckets holding one.
 */
function stackOf(points: AnalyticsPoint[], value: (g: AnalyticsGroup) => number | null, drawn = false, flagged: (g: AnalyticsGroup) => boolean = () => false): Stack {
  const totals = new Map<string, number>();
  const unpriced = new Set<string>();
  const labels = new Map<string, string>();
  for (const p of points) {
    for (const g of p.groups) {
      const v = value(g);
      totals.set(g.key, (totals.get(g.key) ?? 0) + (v ?? 0));
      if (v === null) unpriced.add(g.key);
      labels.set(g.key, modelGroupLabel(g));
    }
  }
  const ranked = [...totals.entries()]
    .filter(([k, total]) => !drawn || total > 0 || unpriced.has(k))
    .sort((a, b) => b[1] - a[1])
    .map(([k]) => k);
  const top = ranked.slice(0, MAX_STACKED_GROUPS);
  const keys = ranked.length > MAX_STACKED_GROUPS ? [...top, 'Other'] : top;
  const listed = new Set(ranked);
  const unpricedGroups = new Set<string>();
  let unpricedBuckets = 0;
  for (const p of points) {
    const hit = p.groups.filter((g) => listed.has(g.key) && flagged(g));
    for (const g of hit) unpricedGroups.add(g.key);
    if (hit.length > 0) unpricedBuckets += 1;
  }
  const data = points.map((p) => {
    const row: Record<string, number | null> = { at: Date.parse(p.at) };
    for (const k of keys) row[k] = 0;
    for (const g of p.groups) {
      const k = top.includes(g.key) ? g.key : 'Other';
      if (!(k in row)) continue;
      const v = value(g);
      const sum = row[k];
      row[k] = v === null || sum === null ? null : sum + v;
    }
    return row;
  });
  return { data, keys, labels, unpriced: { buckets: unpricedBuckets, labels: ranked.filter((k) => unpricedGroups.has(k)).map((k) => labels.get(k) ?? k) } };
}

/**
 * Native stacks the plan rows' Pro-week figure per bucket, its axis and tooltip in the chosen week, and draws the billed
 * rows' dollars on a card of their own. A group sums every account it holds, so the chart carries no hint.
 */
function CostByGroup({ axis, error, points, groupBy, groupSlot, loading, reading, unit, unpriced }: { axis: TimeAxis; error: string | null; groupBy: GroupBy; groupSlot: (key: string) => string; loading: boolean; points: AnalyticsPoint[]; reading: WeekReading; unit: UnitPosition; unpriced: boolean }) {
  const ink = useChartInk();
  const stack = useMemo(() => (unit === 'native' ? stackOf(points, (g) => g.costEquivalent, true, (g) => g.costEquivalent === null) : stackOf(points, (g) => g.costUsd)), [points, unit]);
  const billed = useMemo(() => (unit === 'native' ? stackOf(points, (g) => g.billedUsd, true) : null), [points, unit]);
  const { keys, labels, data } = stack;
  const color = (k: string) => (k === 'Other' ? ink.muted : groupSlot(k));
  const label = (k: string) => labels.get(k) ?? k;
  const title = `Cost by ${GROUP_LABEL[groupBy].toLowerCase()}`;
  const areas = (s: Stack) => s.keys.map((k) => <Area key={k} type="monotone" dataKey={k} stackId="a" stroke={color(k)} strokeWidth={1.5} fill={color(k)} fillOpacity={0.35} dot={false} isAnimationActive={false} />);
  const dollarCard = (s: Stack, cardTitle: string, subtitle: string) => (
    <ChartCard title={cardTitle} subtitle={subtitle} loading={loading} error={error} unpriced={unpriced} legend={s.keys.map((k) => ({ color: color(k), label: s.labels.get(k) ?? k }))}>
      <AreaChart data={s.data} margin={{ top: 8, right: 8, bottom: 0, left: 0 }}>
        <CartesianGrid vertical={false} stroke={ink.grid} />
        <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
        <YAxis {...usdAxis(s.data.map((row) => s.keys.reduce((n, k) => n + (row[k] ?? 0), 0)))} tickLine={false} axisLine={false} />
        <Tooltip cursor={{ stroke: ink.axis }} content={timeTip<Record<string, number | null> & { at: number }>((p) => s.keys.map((k) => ({ color: color(k), label: s.labels.get(k) ?? k, value: fmtUsd(p[k] ?? 0) })))} />
        {areas(s)}
      </AreaChart>
    </ChartCard>
  );
  if (unit === 'native') {
    const billedDrawn = billed !== null && billed.keys.length > 0;
    // A range with no plan row leaves the plan card out while the billed card draws, as the tiles leave out a side
    // with no rows. With neither side the card stays, so an empty range still reads as one.
    const planDrawn = loading || error !== null || keys.length > 0 || !billedDrawn;
    const subtitle = 'Share of the chosen week per bucket, stacked, the subscription requests';
    const note = unpricedNote(stack.unpriced.labels, stack.unpriced.buckets);
    const { scale: weekFactor, week } = reading;
    return (
      <>
        {planDrawn && (
          <ChartCard
            title={title}
            subtitle={
              note === null ? (
                subtitle
              ) : (
                <>
                  {subtitle}
                  <br />
                  <span className="unpriced-note">{note}</span>
                </>
              )
            }
            loading={loading || weekFactor === null}
            error={error}
            unpriced={unpriced}
            legend={keys.map((k) => ({ color: color(k), label: label(k) }))}
          >
            {weekFactor !== null && (
              <AreaChart data={data} margin={{ top: 8, right: 8, bottom: 0, left: 0 }}>
                <CartesianGrid vertical={false} stroke={ink.grid} />
                <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
                <YAxis tickFormatter={(v: number) => fmtShare(v * weekFactor)} tickLine={false} axisLine={false} width={60} />
                {/* Recharts drops null entries by default, and a bucket whose every group the route priced nothing on
                    would lose its tooltip and its dashes. */}
                <Tooltip
                  cursor={{ stroke: ink.axis }}
                  filterNull={false}
                  content={timeTip<Record<string, number | null> & { at: number }>((p) => keys.map((k) => ({ color: color(k), label: label(k), value: equivalentOrDash(p[k], week, weekFactor) })))}
                />
                {areas(stack)}
              </AreaChart>
            )}
          </ChartCard>
        )}
        {billedDrawn && dollarCard(billed, `Billed by ${GROUP_LABEL[groupBy].toLowerCase()}`, 'USD per bucket, stacked, the billed requests')}
      </>
    );
  }
  return dollarCard(stack, title, `USD per bucket, stacked, ${UNIT_SUBSET[dollarsOf(unit)]}`);
}

const UNFILTERED_NOTE = '. The filters above do not apply here';
/** The refusals card's note: the session filter, the one filter of the page the refusal route does not read, if set. */
function refusalsNote(session: boolean): string {
  return session ? '. The session filter does not apply here' : '';
}
const BURN_UNFILTERED_NOTE = '. Of the filters above, the Accounts filter and the user agent groups alone apply here';
type TimeAxis = { height?: number; label?: { fill: string; fontSize: number; position: 'insideBottomRight'; value: string }; tickFormatter: (v: number) => string };
/**
 * A time axis over buckets `bucketMs` wide. The 6-hour and daily buckets are UTC multiples, so their ticks print the
 * viewer's date with the local time beside it where the boundary is not local midnight, and the axis names the
 * boundary once; hour ticks stay local, as the tooltips do.
 */
function timeAxis(bucketMs: number, ink: ChartInk): TimeAxis {
  if (bucketMs < 6 * HOUR) return { tickFormatter: (v) => fmtHourTick(v) };
  // The label sits under the last tick, so the axis grows a line taller than its 30 px to keep the two apart.
  return { height: 40, label: { fill: ink.ink2, fontSize: 11, position: 'insideBottomRight', value: 'days start at 00:00 UTC' }, tickFormatter: (v) => fmtDayTick(v) };
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

/** The destination fits in the gate's order, so a reason's bar reads from its best landings to its worst. */
const MOVE_FITS = ['fits', 'rewrite_only', 'least_bad', 'unknown'];
const fitRank = (fit: string) => (MOVE_FITS.includes(fit) ? MOVE_FITS.indexOf(fit) : MOVE_FITS.length);
/**
 * A fit's segment in a status ink apart from the card's Rewrite tokens series: `fits` good, `rewrite only` warning,
 * `least bad` critical, and `unknown` (a move whose record carries no fit) the muted series ink.
 */
const fitColor = (ink: ChartInk, fit: string): string => (fit === 'fits' ? ink.good : fit === 'rewrite_only' ? ink.warning : fit === 'least_bad' ? ink.critical : ink.muted);

function RewritesCard({ answer, axis, error, loading, unfiltered }: { answer: RewritesAnswer | null; axis: TimeAxis; error: string | null; loading: boolean; unfiltered: boolean }) {
  const ink = useChartInk();
  const data = useMemo(() => (answer?.points ?? []).map((p) => ({ at: Date.parse(p.at), rewriteTokens: p.rewriteTokens })), [answer]);
  const total = data.reduce((n, p) => n + p.rewriteTokens, 0);
  const moves = Object.entries(answer?.moves ?? {}).sort(([a], [b]) => a.localeCompare(b));
  const fitsOf = (reason: string) =>
    Object.entries(answer?.movesByReasonAndFit[reason] ?? {})
      .filter(([, n]) => n > 0)
      .sort(([a], [b]) => fitRank(a) - fitRank(b) || a.localeCompare(b));
  const drawn = [...new Set(moves.flatMap(([reason]) => fitsOf(reason).map(([fit]) => fit)))].sort((a, b) => fitRank(a) - fitRank(b) || a.localeCompare(b));
  const longest = Math.max(1, ...moves.map(([, n]) => n));
  return (
    <ChartCard
      title="Migration rewrites"
      subtitle={`Cache-write tokens the first request after a move wrote, per bucket${unfiltered ? UNFILTERED_NOTE : ''}`}
      error={error}
      loading={loading}
      legend={[{ color: ink.series[1], label: 'Rewrite tokens', value: fmtCompact(total) }]}
      footer={
        <>
          <ul className="legend" aria-label="Moves by destination fit">
            <li className="muted">Moves</li>
            {moves.length === 0 ? (
              <li className="muted">none in range</li>
            ) : (
              drawn.map((fit) => (
                <li key={fit}>
                  <span className="legend__swatch legend__swatch--rect" style={{ background: fitColor(ink, fit) }} />
                  {words(fit)}
                </li>
              ))
            )}
          </ul>
          {moves.length > 0 && (
            <ul className="hbars hbars--moves" aria-label="Moves by reason">
              {moves.map(([reason, n]) => {
                const label = upperFirst(moveLabel(reason));
                const fits = fitsOf(reason);
                return (
                  <li key={reason}>
                    <span className="hbars__label">{label}</span>
                    <div className="hbars__track">
                      <span className="hbars__stack" role="img" aria-label={`${label}: ${fits.map(([fit, k]) => `${words(fit)} ${fmtInt(k)}`).join(', ')}`} style={{ width: `${(n / longest) * 100}%` }}>
                        {fits.map(([fit, k]) => (
                          <span key={fit} className="hbars__seg" title={`${words(fit)} ${fmtInt(k)}`} style={{ background: fitColor(ink, fit), flexGrow: k }} />
                        ))}
                      </span>
                    </div>
                    <span className="hbars__value num">{fmtInt(n)}</span>
                  </li>
                );
              })}
            </ul>
          )}
          {answer && <HeldChart answer={answer} axis={axis} />}
        </>
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

type HeldRow = { at: number; heldMs: number; requests: number };

/**
 * The slow-down's held time per bucket, which the rewrites card draws under its moves: a bar per bucket in the
 * warning tone a held request's marker wears, its hover the requests that waited and their time, and its legend line
 * the range's sum. A range that held nothing says so on the legend line and draws no chart.
 */
function HeldChart({ answer, axis }: { answer: RewritesAnswer; axis: TimeAxis }) {
  const ink = useChartInk();
  const held = useMemo(() => heldBars(answer), [answer]);
  const data = useMemo(() => held.bars.map((b): HeldRow => ({ at: Date.parse(b.at), heldMs: b.heldMs, requests: b.requests })), [held]);
  return (
    <div className="held-chart">
      <ul className="legend" aria-label="Held by the slow-down">
        <li className="muted">Held by the slow-down</li>
        {held.totalRequests === 0 ? (
          <li className="muted">none in range</li>
        ) : (
          <li title={heldHover({ heldMs: held.totalMs, requests: held.totalRequests })}>
            <span className="legend__swatch legend__swatch--rect" style={{ background: ink.warning }} />
            Held time
            <span className="legend__value num">{heldSpan(held.totalMs)}</span>
          </li>
        )}
      </ul>
      {held.totalRequests > 0 && (
        <ResponsiveContainer width="100%" height={120}>
          <BarChart data={data} margin={{ top: 8, right: 8, bottom: 0, left: 0 }} barCategoryGap={2}>
            <CartesianGrid vertical={false} stroke={ink.grid} />
            <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
            <YAxis tickFormatter={(v: number) => fmtMs(v)} tickLine={false} axisLine={false} width={44} />
            <Tooltip cursor={{ fill: ink.grid, opacity: 0.6 }} content={timeTip<HeldRow>((p) => [{ color: ink.warning, label: heldHover(p), value: null }])} />
            <Bar dataKey="heldMs" fill={ink.warning} maxBarSize={22} radius={[3, 3, 0, 0]} isAnimationActive={false} />
          </BarChart>
        </ResponsiveContainer>
      )}
    </div>
  );
}

/**
 * The guardrails that matched a request in the range: the observed matches, the blocks and the blocked requests'
 * bytes, each guardrail by its current name, or its id marked deleted where the registry no longer holds it.
 */
function GuardrailsCard({ answer, error }: { answer: GuardrailCounts | null; error: string | null }) {
  const rows = answer?.guardrails ?? [];
  return (
    <Card title="Guardrails" subtitle="Matches by guardrail in range, every key" padded={false} className="card--table analytics-guardrails">
      <div className="table-wrap">
        <table className="table">
          <thead>
            <tr>
              <th className="col-fill">Guardrail</th>
              <th className="right col-fit">Observed</th>
              <th className="right col-fit">Blocked</th>
              <th className="right col-fit">Blocked bytes</th>
            </tr>
          </thead>
          <tbody>
            {error === null &&
              rows.map((c) => {
                const label = c.name ?? `${c.id} (deleted)`;
                return (
                  <tr key={c.id}>
                    <td className="col-fill" title={label}>
                      {label}
                    </td>
                    <td className="right num col-fit">{fmtInt(c.observed)}</td>
                    <td className="right num col-fit">{fmtInt(c.blocked)}</td>
                    <td className="right num col-fit">{fmtBytes(c.blockedBytes)}</td>
                  </tr>
                );
              })}
            {(error !== null || rows.length === 0) && (
              <tr>
                <td colSpan={4} className={error ? '' : 'muted'}>
                  {error ? <ErrorState title="Could not load the guardrail matches" body={error} /> : answer === null ? <Skeleton height={22} /> : 'No guardrail matched in this range.'}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
    </Card>
  );
}

const CONVERSATION_HINT = CONVERSATION_CLASSES.join(', ');
const OVERHEAD_HINT = 'every other class, and requests that named none';
const CLASS_NOTE = `conversation is ${CONVERSATION_HINT}. Overhead is ${OVERHEAD_HINT}`;

const SPEND_NOTE = "Dollars per minute: API-key traffic, a usage-based seat's whole spend, and a subscription account's fast-mode and overage answers. Its plan quota is charted beside it as a share of the chosen week";
/** A folded rest of subscription accounts as the dollars chart names it: its dollars are its members' fast-mode
 *  and overage answers, and its own label names the plan unit, which the dollars axis does not chart. */
const SPEND_REST = 'Other (fast mode or overage)';
const SPEND_REST_LABEL: Record<Exclude<BurnLineUnit, 'usd'>, string> = { unknown: 'Other (fast mode or overage, tier unknown)', window_share: SPEND_REST };
const TIER_UNKNOWN_HINT = 'A free plan, no tier on file or one the cost model does not hold, and no bucket of the range priced, so no share of the week is drawn';
const MODEL_UNLISTED_HINT = 'A bucket whose traffic ran on a model without a weight in the cost model prices no share and draws no point';
const NO_ACCOUNT: ReadonlySet<string> = new Set();
const PARTIAL_NOTE = 'The last bucket is partial, rated over the part of its width it covers';
const PLAN_TITLE = 'Plan per hour';
const PLAN_SUBTITLE = 'The subscription traffic per hour as a share of the chosen week';

/**
 * A line's dot, drawn on the slots `isolatedSlots` names alone: the line leaves its null slots unconnected, so a
 * value with a null or the series' end on both sides draws no segment of its own.
 */
const isolatedDot = (values: (number | null)[], color: string) => {
  const isolated = isolatedSlots(values);
  return ({ cx, cy, index, key }: { cx?: number; cy?: number; index: number; key?: string }) =>
    isolated[index] ? <Dot key={key} className="recharts-line-dot" cx={cx} cy={cy} r={3} fill={color} stroke={color} /> : <g key={key} />;
};

/**
 * The burn route's views of one grouping: the dollars per minute, and the plan card in the chosen week. A line's color
 * follows its group within the answer rather than its rank, so a range that reorders the groups does not repaint
 * them, and the account grouping keeps each account's page-wide color. `account` is the one account the Accounts
 * filter names, and `accounts` the accounts read beside the answer, which give a tooltip's hint its plan and tier.
 */
function BurnCharts({
  account,
  accounts,
  answer,
  costs,
  error,
  groupBy,
  loading,
  onCostReferenceOpenChange,
  rangeEndMs,
  readAt,
  reading,
  sized,
  slotOf,
}: {
  account: string | null;
  accounts: Account[] | null;
  answer: { bucketSeconds: number; points: BurnPoint[] } | null;
  costs: SubscriptionCosts | null;
  error: string | null;
  groupBy: BurnGroup;
  loading: boolean;
  /** The Cost reference dialog opening and closing, which the page's auto reload waits for. */
  onCostReferenceOpenChange: (open: boolean) => void;
  rangeEndMs: number | undefined;
  readAt: number | undefined;
  reading: WeekReading;
  sized: ReadonlySet<string> | null;
  slotOf: (id: string) => number | null;
}) {
  const ink = useChartInk();
  const { lines, rows } = useMemo(
    () => (answer === null ? { lines: [] as BurnLine[], rows: [] as BurnRow[] } : foldBurn(answer.points, groupBy, answer.bucketSeconds, sized ?? NO_ACCOUNT, MAX_BURN_LINES, rangeEndMs, readAt, { account })),
    [account, answer, groupBy, rangeEndMs, readAt, sized],
  );
  // The points by bucket and group, for the tooltip's hint, which reads a bucket's one point of one account.
  const pointsAt = useMemo(() => {
    const at = new Map<string, BurnPoint[]>();
    for (const p of answer?.points ?? []) {
      const slot = `${Date.parse(p.at)}|${p.group}`;
      at.set(slot, [...(at.get(slot) ?? []), p]);
    }
    return at;
  }, [answer]);
  const named = useMemo(() => lines.filter((l) => !l.other).map((l) => l.key).sort(), [lines]);
  const colorOf = (l: BurnLine) => (l.other ? ink.muted : groupBy === 'account' ? accountColor(ink, slotOf(l.key)) : ink.series[Math.min(7, named.indexOf(l.key))]);
  // A session id runs past any legend, so it shows cut the way the session filter's chip cuts it, whole on hover.
  const nameOf = (l: BurnLine) => (groupBy === 'session' && !l.other ? shortId(l.label, 13) : l.label);
  const titleOf = (l: BurnLine) => (groupBy === 'request_class' ? (l.key === 'conversation' ? CONVERSATION_HINT : OVERHEAD_HINT) : groupBy === 'session' && !l.other ? l.label : undefined);
  const axis = timeAxis((answer?.bucketSeconds ?? 3_600) * 1_000, ink);
  // Read off the buckets, not the lines: the class grouping draws its two lines even over a range with no traffic.
  const empty = !loading && error === null && rows.length === 0 ? 'No traffic in this range.' : undefined;
  const byAccount = groupBy === 'account';
  // Where each group holds one account's rows, a line's unit is its account's.
  const shared = byAccount || account !== null;
  // Every other grouping mixes both kinds of account, so its lines chart the dollars their API-key traffic and
  // fast-mode and overage answers spent. Under the account grouping a subscription account, or a folded rest of
  // them, draws a dollar line only for what its fast-mode and overage answers spent.
  const spent = lines.map((l, i) => ({ i, l })).filter(({ l }) => !byAccount || l.unit === 'usd' || l.usdPerMinute > 0);
  // Until the settings land no account reads as sized, so the rest names no tier it cannot know yet.
  const spendNameOf = (l: BurnLine) => (l.other && l.unit !== 'usd' ? (sized === null ? SPEND_REST : SPEND_REST_LABEL[l.unit]) : nameOf(l));
  // A line draws while a plan paid for its traffic, a group of billed rows alone reading zero, and where each group
  // holds one account's rows only a line in the plan's unit draws: one the router sizes or whose points carry a figure.
  const planned = lines.map((l, i) => ({ i, l })).filter(({ l }) => !l.other && (!shared || l.unit === 'window_share') && (l.unpriced || (l.perHour !== null && l.perHour > 0)));
  const unknown = shared ? lines.filter((l) => !l.other && l.unit === 'unknown') : [];
  const partial = rows[rows.length - 1]?.partial === true;
  // A bucket too short to set the scale would stretch the axis over a few seconds' rate, so the axis stops at the
  // buckets that set it and the short one runs past the top.
  const scaled = rows.filter((r) => r.setsScale);
  const scaleTo = (values: number[]) => {
    const top = Math.max(0, ...values);
    return scaled.length > 0 && scaled.length < rows.length && top > 0 ? { allowDataOverflow: true, domain: [0, niceAxisTop(top)] as [number, number] } : {};
  };
  const spentValues = scaled.flatMap((r) => spent.map(({ i }) => r.usd[i]));
  const spentScale = scaleTo(spentValues);
  // Without a capped domain recharts spans every row, so the labels' precision reads every row too.
  const spentAxis = usdAxis('domain' in spentScale ? spentValues : rows.flatMap((r) => spent.map(({ i }) => r.usd[i])));
  // The route answers a bucket's shares without the model week's key, so the hint names the model week the cost model
  // carries on the 7-day window.
  const weekKey = costs === null ? undefined : Object.keys(costs.modelWindows).find((key) => costs.modelWindows[key].of === 'seven_day');
  // The hint reads one account's line alone, a folded line (the request classes, the rest) carrying none, and a bucket
  // has no one completion, so no band word.
  const hintOf = (l: BurnLine, at: number): string | null => {
    const owner = l.other || groupBy === 'request_class' || !shared ? undefined : accounts?.find((a) => a.id === (byAccount ? l.key : account));
    const here = pointsAt.get(`${at}|${l.key}`) ?? [];
    if (owner === undefined || here.length !== 1) return null;
    const [p] = here;
    const tier = owner.subscription?.rateLimitTier ?? null;
    const plan = hintPlan(owner, tier);
    const shares: Record<string, number | null> = { five_hour: p.fiveHourShare, seven_day: p.sevenDayShare, ...(weekKey === undefined ? {} : { [weekKey]: p.modelWeekShare }) };
    return plan === null ? null : shareHint(plan, shares, tier, null);
  };
  const subtitle = (
    <>
      {PLAN_SUBTITLE}.{partial ? ` ${PARTIAL_NOTE}.` : ''} <CostReferenceLink onOpenChange={onCostReferenceOpenChange} tab="tiers" />
    </>
  );
  const { scale: weekFactor, week } = reading;
  const planCard = (factor: number) => {
    // The figure in the chosen week, the full form on a hover and the short one in the legend.
    const figureOf = (v: number | null, form: 'full' | 'short') => (v === null ? '-' : form === 'full' ? fmtWeek(v * factor, week) : weekShort(v * factor, week));
    const legend = [
      ...planned.map(({ l }) => ({
        color: colorOf(l),
        label: nameOf(l),
        shape: 'line' as const,
        title: l.peak === null ? undefined : `Highest hour ${figureOf(l.peak, 'full')}`,
        // The figure's own hover names it in full and masks the entry's title over the figure. A dash has none.
        value: l.perHour === null ? '-' : <span title={figureOf(l.perHour, 'full')}>{figureOf(l.perHour, 'short')}</span>,
      })),
      // One entry for every bucket the cost model could not price, whichever line it fell on. Muted, since it draws no
      // line of its own.
      ...(planned.some(({ l }) => l.unpriced) ? [{ color: ink.muted, label: 'model unlisted', shape: 'line' as const, title: MODEL_UNLISTED_HINT }] : []),
      // Muted, since no line of its own is drawn.
      ...unknown.map((l) => ({ color: ink.muted, label: nameOf(l), shape: 'line' as const, title: TIER_UNKNOWN_HINT, value: 'tier unknown' })),
    ];
    return (
      <ChartCard
        key={PLAN_TITLE}
        title={PLAN_TITLE}
        subtitle={subtitle}
        error={error}
        loading={loading}
        empty={
          empty ??
          (planned.length > 0
            ? undefined
            : unknown.length > 0
              ? byAccount
                ? `No subscription account in range has a tier the router prices: ${unknown.map(nameOf).join(', ')}.`
                : 'The named account has a free plan, or has no tier on file or one the cost model does not hold, and no bucket of the range priced.'
              : 'No subscription traffic in this range.')
        }
        legend={legend}
      >
        <LineChart data={rows} margin={{ top: 8, right: 8, bottom: 0, left: 0 }}>
          <CartesianGrid vertical={false} stroke={ink.grid} />
          <XAxis dataKey="at" type="number" scale="time" domain={['dataMin', 'dataMax']} ticks={rows.map((r) => r.at)} {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
          <YAxis tickFormatter={(v: number) => fmtShare(v * factor)} tickLine={false} axisLine={false} width={52} {...scaleTo(scaled.flatMap((r) => planned.map(({ i }) => r.equivalent[i] ?? 0)))} />
          {/* Recharts drops null entries by default, and a bucket whose every figure is null would lose its date, its
              dashes and the unpriced hint. */}
          <Tooltip
            cursor={{ stroke: ink.axis }}
            filterNull={false}
            content={timeTip<BurnRow>((r) =>
              planned.flatMap(({ i, l }) => {
                const v = r.equivalent[i];
                const hint = hintOf(l, r.at);
                return [{ color: colorOf(l), label: nameOf(l), title: l.unpriced && slotUnpriced(v, r.tokens[i]) ? MODEL_UNLISTED_HINT : undefined, value: figureOf(v, 'full') }, ...(hint === null ? [] : [{ label: hint, value: null }])];
              }),
            )}
          />
          {planned.map(({ i, l }) => (
            // A null slot draws no point, and the gap stays since the lines do not connect across it.
            <Line key={i} type="monotone" dataKey={(r: BurnRow) => r.equivalent[i]} stroke={colorOf(l)} strokeWidth={2} dot={isolatedDot(rows.map((r) => r.equivalent[i]), colorOf(l))} isAnimationActive={false} />
          ))}
        </LineChart>
      </ChartCard>
    );
  };
  return (
    <div className="grid-2 burn-grid">
      {/* The figure waits on the chosen week's scale, so the card draws its skeleton while the cost model is out and its
          failure in its place. */}
      {weekFactor === null ? (
        <ChartCard title={PLAN_TITLE} subtitle={subtitle} error={error ?? reading.error} unpriced={error === null && reading.unpriced} loading>
          {null}
        </ChartCard>
      ) : (
        planCard(weekFactor)
      )}
      <ChartCard
        title="Spend per minute"
        subtitle={`${byAccount ? SPEND_NOTE : `${SPEND_NOTE} when grouped by account`}${groupBy === 'request_class' ? `. ${upperFirst(CLASS_NOTE)}` : ''}${partial ? `. ${PARTIAL_NOTE}` : ''}`}
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
    </div>
  );
}

const PLACEMENT_HINT = "A placement by the conversation's own messages is a hit when the response read the prefix it was promised, and one by the static prefix when it read at all";
const PLACEMENT_UNFILTERED_NOTE = '. The account, model and pattern filters above do not apply here';

/** Placements by rung over the range, narrowed by the key and pool filters, the two the route reads here. */
function PlacementTable({ error, rows, unfiltered }: { error: string | null; rows: PlacementRow[] | null; unfiltered: boolean }) {
  return (
    <Card title="Placements by rung" subtitle={`${PLACEMENT_HINT}${unfiltered ? PLACEMENT_UNFILTERED_NOTE : ''}`} padded={false} className="card--table">
      <div className="table-wrap">
        <table className="table placements-table">
          <thead>
            <tr>
              <th className="col-fill">Rung</th>
              <th className="right col-fit">Placements</th>
              <th className="right col-fit">Hits</th>
              <th className="right col-fit">Hit share</th>
              <th className="right col-fit">Tokens read</th>
              <th className="right col-fit">Tokens promised</th>
            </tr>
          </thead>
          <tbody>
            {(rows ?? []).map((r) => {
              const rung = placementRungLabel(r.rung);
              return (
                <tr key={r.rung}>
                  <td className="col-fill" title={rung}>
                    {rung}
                  </td>
                  {placementCells(r).map((cell, i) => (
                    <td key={i} className="right num col-fit">
                      {cell}
                    </td>
                  ))}
                </tr>
              );
            })}
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

const BUDGET_HINT = "Each budgeted member's spend in its current period, an API-key member's whole spend and a subscription member's metered spend. The range and the filters above do not apply here";

/** Spend against each member's budget; a member at its budget is skipped until its period ends. */
function BudgetTable({ budgets, error, now }: { budgets: MemberBudget[] | null; error: string | null; now: number }) {
  return (
    <Card title="Member budgets" subtitle={BUDGET_HINT} padded={false} className="card--table">
      <div className="table-wrap">
        <table className="table budgets-table">
          <thead>
            <tr>
              <th className="col-fill">Pool</th>
              <th className="col-fill">Account</th>
              <th className="col-fit">Spent</th>
              <th className="right col-fit">Budget</th>
              <th className="col-fit" title="a period ends at midnight UTC">Period ends</th>
            </tr>
          </thead>
          <tbody>
            {(budgets ?? []).map((b) => {
              const ratio = b.spentUsd === null ? 0 : b.spentUsd / b.budgetUsd;
              // A spend reported for a period that has since ended says nothing about the one running now.
              const reached = b.spentUsd !== null && b.spentUsd >= b.budgetUsd && b.periodEnd !== null && Date.parse(b.periodEnd) > now;
              return (
                <tr key={`${b.poolId}-${b.accountId}-${b.budgetPeriod}`}>
                  <td className="mono col-fill" title={b.poolId}>
                    {b.poolId}
                  </td>
                  <td className="mono col-fill" title={b.accountId}>
                    <Link to={`/accounts/${b.accountId}`}>{b.accountId}</Link>
                  </td>
                  <td className="col-fit">
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
                  <td className="right num col-fit">
                    {fmtUsd(b.budgetUsd)} per {b.budgetPeriod}
                  </td>
                  <td className="col-fit" title={b.periodEnd ? fmtAbsolute(b.periodEnd) : undefined}>
                    {b.periodEnd ? <span className="num">{fmtPeriodEnd(b.periodEnd)}</span> : <span className="muted">-</span>}
                  </td>
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

/**
 * The toggle's word for a sampled window: the console's label for the three the router reads, and the extra-usage
 * week by its own name, since the decoder folds that key onto the model week elsewhere and the cards never draw two
 * windows as one.
 */
const toggleLabel = (id: string): string => (id === 'seven_day_overage_included' ? '7-day, extra usage' : WINDOW_LABEL[windowId(id) ?? ''] ?? id);

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
  allowance,
  axis,
  bucketMs,
  error,
  loading,
  otherFilters,
  rollups: all,
  slotOf,
  span,
  week,
}: {
  accountIds: string[];
  /** The unused allowance card's read, its answer null while it runs, and the cost model its figures price through. */
  allowance: { answer: UnusedAllowanceAnswer | null; costs: SubscriptionCosts | null; error: string | null };
  axis: TimeAxis;
  bucketMs: number;
  error: string | null;
  loading: boolean;
  otherFilters: boolean;
  rollups: AccountRollup[] | null;
  slotOf: (id: string) => number | null;
  span: { fromMs: number; toMs: number };
  week: WeekPosition;
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
  const windows = useMemo(() => sampledWindows(rollups ?? []), [rollups]);
  const [window, setWindow] = useState('five_hour');
  // A chosen window no longer offered (the extra-usage week once no row of the range holds it) falls to the 5-hour one.
  const chosen = windows.includes(window) ? window : 'five_hour';
  const usage = useMemo(() => windowLines(rollups ?? [], chosen, 'usage', span, bucketMs), [rollups, chosen, span, bucketMs]);
  const pace = useMemo(() => windowLines(rollups ?? [], chosen, 'pace', span, bucketMs), [rollups, chosen, span, bucketMs]);
  // The axis takes the widest lead either side of zero, raised to a round tick, so ahead and behind read alike.
  const paceTop = useMemo(() => {
    const widest = Math.max(0, ...pace.data.flatMap((r) => accounts.map((a) => Math.abs(r[a] ?? 0))));
    return widest > 0 ? niceAxisTop(widest) : 50;
  }, [pace, accounts]);
  const toggle = <Segmented value={chosen} options={windows.map((id) => ({ label: toggleLabel(id), value: id }))} onChange={setWindow} ariaLabel="Window" />;
  const legend = accounts.map((a) => ({ color: accountColor(ink, slotOf(a)), label: a, shape: 'line' as const }));
  const scoreOf = (p: Record<string, number | null>, a: string) => {
    const score = p[`${a}:score`];
    return score === null || score === undefined ? '-' : score.toFixed(2);
  };
  return (
    <>
      <ChartCard
        title="Peak 5-hour utilization"
        subtitle="Highest utilization seen in each bucket, per account"
        loading={loading}
        legend={accounts.filter((a) => data.some((p) => p[a] !== null && p[a] !== undefined)).map((a) => ({ color: accountColor(ink, slotOf(a)), label: a, shape: 'line' }))}
      >
        <LineChart data={data} margin={{ top: 8, right: 8, bottom: 0, left: 0 }}>
          <CartesianGrid vertical={false} stroke={ink.grid} />
          <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
          <YAxis domain={[0, 100]} ticks={[0, 25, 50, 75, 100]} tickFormatter={(v: number) => `${v}%`} tickLine={false} axisLine={false} width={40} />
          <Tooltip cursor={{ stroke: ink.axis }} content={timeTip<Record<string, number | null> & { at: number }>((p) => accounts.map((a) => ({ color: accountColor(ink, slotOf(a)), label: a, value: p[a] === null || p[a] === undefined ? '-' : `${(p[a] as number).toFixed(0)}%` })))} />
          {accounts.map((a) => (
            <Line key={a} type="monotone" dataKey={a} stroke={accountColor(ink, slotOf(a))} strokeWidth={1.5} dot={false} connectNulls isAnimationActive={false} />
          ))}
        </LineChart>
      </ChartCard>
      <div className="grid-2">
        <ChartCard title="Window usage" subtitle="The window's utilization per bucket, from the quota samples" actions={toggle} loading={loading} legend={legend}>
          <LineChart data={usage.data} margin={{ top: 8, right: 8, bottom: 0, left: 0 }}>
            <CartesianGrid vertical={false} stroke={ink.grid} />
            <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
            <YAxis domain={[0, 100]} ticks={[0, 25, 50, 75, 100]} tickFormatter={(v: number) => `${v}%`} tickLine={false} axisLine={false} width={40} />
            <Tooltip cursor={{ stroke: ink.axis }} content={timeTip<Record<string, number | null> & { at: number }>((p) => accounts.map((a) => ({ color: accountColor(ink, slotOf(a)), label: a, value: pct(p[a]) })))} />
            {/* The lines leave a null bucket unconnected, so a bucket with no sample reads as a gap rather than a
                slope, and a sampled bucket with a gap or the range's edge on both sides draws as a dot, as the share
                chart draws one. */}
            {accounts.map((a) => (
              <Line key={a} type="monotone" dataKey={a} stroke={accountColor(ink, slotOf(a))} strokeWidth={1.5} dot={isolatedDot(usage.data.map((r) => r[a]), accountColor(ink, slotOf(a)))} isAnimationActive={false} />
            ))}
          </LineChart>
        </ChartCard>
        <ChartCard title="Pace" subtitle="The lead over pace per bucket: above zero ahead, below behind, from the quota samples, before the router's burn projection" actions={toggle} loading={loading} legend={legend}>
          <LineChart data={pace.data} margin={{ top: 8, right: 8, bottom: 0, left: 0 }}>
            <CartesianGrid vertical={false} stroke={ink.grid} />
            <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
            <YAxis domain={[-paceTop, paceTop]} ticks={[-paceTop, -paceTop / 2, 0, paceTop / 2, paceTop]} tickFormatter={(v: number) => pct(v, true)} tickLine={false} axisLine={false} width={44} />
            <ReferenceLine y={0} stroke={ink.axis} />
            <Tooltip cursor={{ stroke: ink.axis }} content={timeTip<Record<string, number | null> & { at: number }>((p) => accounts.flatMap((a) => [{ color: accountColor(ink, slotOf(a)), label: a, value: pct(p[a], true) }, { label: `Pace score, ${a}`, title: "the remaining share over the hours from the sample's instant to the window's reset", value: scoreOf(p, a) }]))} />
            {accounts.map((a) => (
              <Line key={a} type="monotone" dataKey={a} stroke={accountColor(ink, slotOf(a))} strokeWidth={1.5} dot={isolatedDot(pace.data.map((r) => r[a]), accountColor(ink, slotOf(a)))} isAnimationActive={false} />
            ))}
          </LineChart>
        </ChartCard>
      </div>
      <BreakdownTable
        className="accounts-range"
        title="Accounts in range"
        subtitle={`Totals for the selected range and accounts${otherFilters ? '. The key, pool, model, client and session filters and the probes toggle do not apply here' : ''}`}
        error={error}
        loading={loading}
        groupLabel="Account"
        columns={['Peak 5h', 'Peak 7d', 'Peak 7d, extra usage', 'Time in overage', 'Time in cooldown', 'Pins created', 'Migrated in', 'Migrated out']}
        rows={table.map((r) => ({
          key: r.id,
          label: r.id,
          cells: [
            r.peak5 === null ? <span className="muted" title={NO_SAMPLE_HINT}>-</span> : fmtPct(r.peak5),
            r.peak7 === null ? <span className="muted" title={NO_SAMPLE_HINT}>-</span> : fmtPct(r.peak7),
            r.peakF === null ? <span className="muted" title="no extra-usage window">n/a</span> : fmtPct(r.peakF),
            r.overage ? fmtDuration(r.overage * 1000) : <span className="muted">none</span>,
            r.cooldown ? fmtDuration(r.cooldown * 1000) : <span className="muted">none</span>,
            fmtInt(r.created),
            fmtInt(r.migratedIn),
            fmtInt(r.migratedOut),
          ],
        }))}
      />
      <AllowanceTable {...allowance} week={week} />
    </>
  );
}

/** A figure's cell, a dash in the muted ink that says why on hover. */
const allowanceCell = (key: string, figure: AllowanceFigure | undefined) => (
  <td key={key} className="right num col-fit">
    {figure === undefined ? (
      <span className="muted" title="No reset of this window in the last four weeks">
        -
      </span>
    ) : figure.kind === 'unpriced' || figure.kind === 'none' ? (
      <span className="muted" title={figure.title}>
        -
      </span>
    ) : (
      figure.text
    )}
  </td>
);

/**
 * "Unused allowance by account": each subscription account's weekly windows over the last four weeks under the
 * Accounts filter in the chosen week, ranked most left first, with the fleet's two figures over the table. A failed
 * read, its own or the cost model's, draws in place of the rows and the figures, so no figure stands without the read
 * it prices.
 */
function AllowanceTable({ answer, costs, error, week }: { answer: UnusedAllowanceAnswer | null; costs: SubscriptionCosts | null; error: string | null; week: WeekPosition }) {
  const rows = useMemo(() => (answer === null || costs === null ? null : allowanceRows(answer, costs, week)), [answer, costs, week]);
  const windows = useMemo(() => weeklyKeys(answer?.accounts ?? []), [answer]);
  const fleet = rows === null ? null : fleetFigures(rows, week, weekScale(costs, week));
  return (
    <Card title="Unused allowance by account" subtitle="Each subscription account's weekly windows left unused at their resets, the last four weeks, under the Accounts filter. The range does not apply here" padded={false} className="card--table accounts-range allowance-table">
      {error === null && (
        <div className="req-totals">
          <div>
            <span className="stat__label">7-day, fleet total</span>
            <span className="stat__value">{fleet === null ? <Skeleton width={80} /> : fleet.sevenDay}</span>
          </div>
          <div>
            <span className="stat__label">Fable, fleet total</span>
            <span className="stat__value">{fleet === null ? <Skeleton width={80} /> : fleet.fable}</span>
          </div>
          <div>
            <span className="stat__note">{fleet === null ? <Skeleton width={160} /> : fleet.note}</span>
          </div>
        </div>
      )}
      <div className="table-wrap">
        <table className="table">
          <thead>
            <tr>
              <th className="col-fill">Account</th>
              <th className="right col-fit">Resets</th>
              {windows.flatMap((w) => [
                <th key={`${w}-last`} className="right col-fit">{`${allowanceColumnWord(w)}, last`}</th>,
                <th key={`${w}-weeks`} className="right col-fit">{`${allowanceColumnWord(w)}, ${ALLOWANCE_WEEKS} weeks`}</th>,
              ])}
            </tr>
          </thead>
          <tbody>
            {error === null &&
              rows?.map((r) => (
                <tr key={r.accountId}>
                  <td className="mono col-fill" title={r.accountId}>
                    <Link to={`/accounts/${r.accountId}`}>{r.accountId}</Link>
                    {r.termEndedAt !== null && (
                      <span className="sub" title={fmtAbsolute(r.termEndedAt)}>
                        {`term ended ${resetInstant(r.termEndedAt)}`}
                      </span>
                    )}
                  </td>
                  <td className="right num col-fit">{fmtInt(r.resets)}</td>
                  {windows.flatMap((w) => [allowanceCell(`${w}-last`, r.figures[w]?.last), allowanceCell(`${w}-weeks`, r.figures[w]?.weeks)])}
                </tr>
              ))}
            {(error !== null || rows === null || rows.length === 0) && (
              <tr>
                <td colSpan={2 + windows.length * 2} className={error === null && rows !== null ? 'muted' : ''}>
                  {error !== null ? <ErrorState title="Could not load the unused allowance" body={error} /> : rows === null ? <Skeleton height={22} /> : 'No weekly reset in the last four weeks'}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
    </Card>
  );
}
