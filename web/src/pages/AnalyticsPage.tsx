import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode, type RefObject } from 'react';
import { flushSync } from 'react-dom';
import { Area, AreaChart, Bar, BarChart, CartesianGrid, Dot, Line, LineChart, ReferenceLine, ResponsiveContainer, Tooltip, XAxis, YAxis } from 'recharts';
import { Link } from 'react-router-dom';
import { api } from '@/api';
import { iso } from '@/api/decode';
import { foldGroups, foldRequests, keptRows, latestGenerations, loadSessionRequests, sumGroups, type SessionRequests } from '@/api/fold';
import { useAppSettings } from '@/app/settings';
import type { Account, AccountRollup, AnalyticsGroup, AnalyticsGroupBy, AnalyticsPoint, AnalyticsQuery, BurnGroup, BurnPoint, CacheMissReason, CostSplit, GuardrailCounts, LatencyHistograms, MemberBudget, PlacementRow, RefusalCategoriesAnswer, RequestLog, RewritesAnswer, Settings, SubscriptionCosts, UnusedAllowanceAnswer } from '@/api/types';
import { TIME_BAR } from '@/components/charts/bars';
import { Donut, DonutLegend } from '@/components/charts/Donut';
import { BAR_GAP, CHART_HEIGHT, COUNT_AXIS, OUTCOME_SERIES, PAIRED_BAR_GAPS, PCT_AXIS, PERCENTILES, PROBE_SERIES, TOKEN_SERIES, accountColor, accountSlots, type ChartInk } from '@/components/charts/theme';
import { useChartInk } from '@/components/charts/useChartInk';
import { Legend, timeTip, type TooltipRow } from '@/components/charts/Tooltip';
import { CostReferenceLink } from '@/components/ui/CostReference';
import { CostLines } from '@/components/ui/CostTd';
import type { Option } from '@/components/ui/OptionList';
import { figuresValue, Tile } from '@/components/ui/Tile';
import { PopoverHost, useHoldHost } from '@/components/ui/Popover';
import { Badge, Card, ErrorState, Meter, Segmented, Skeleton, StateRow } from '@/components/ui/primitives';
import { FilterBar } from '@/components/FilterBar';
import { GroupBy as GroupByControl, GroupsButton } from '@/components/GroupBy';
import { SectionBar, sectionPanel } from '@/components/SectionBar';
import { SortControl } from '@/components/SortControl';
import { accountHref, addressWindow, ANALYTICS_CODEC, analyticsKeysOf, sortDirOf, type AnalyticsAddress, type SortDir } from '@/lib/address';
import { NO_RESET_IN_RANGE, allowanceRows, fleetFigures, rangeAllowanceSpan, resetInstant, weeklyKeys, weeklyResetsKey, type AllowanceFigure, type AllowanceRow, type FleetFigure } from '@/lib/allowance';
import { CONVERSATION_CLASSES, NATIVE_SUBSET, UNIT_SUBSET, analyticsPatch, burnColorOrder, costHeadOf, coveredWindow, dollarsOf, equivalentOrDash, errorRateOf, failureHead, firstBucketPartial, foldBurn, heldBars, heldHover, heldSpan, isolatedSlots, keptWarmNote, keptWarmTokens, lastBucketPartial, moveLabel, nativeCellOf, nativeFiguresOf, netSaving, niceAxisTop, oneAccountGroupOf, ownShareHint, peakLines, placementCells, placementRungLabel, probeParts, readBurn, sentences, sidesLine, sidesOf, sizedAccounts, slotIndex, slotUnpriced, UNPRICED_TITLE, unpricedNote, upstreamProbes, windowLines, type BurnLine, type BurnLineUnit, type BurnRow, type CostFigure, type DollarPosition, type WeekReading } from '@/lib/analytics';
import { ignoredNote, type AnalyticsControl } from '@/lib/analyticsControls';
import { bucketSizeFor } from '@/lib/buckets';
import { CACHE_MISS_REASONS, WASTED_PROBE_LABEL, WASTED_PROBE_REASONS, missLabel, missReasonOrder, missShort } from '@/lib/cache';
import { clock } from '@/lib/clock';
import { fableWeekCard, failed, fiveHourCard, pendingReading, weekCard, weekScale, type FiguredPoint } from '@/lib/costs';
import { countOf, groupInto } from '@/lib/counts';
import { REASON_HIT_PERCENT, relayReason } from '@/lib/dimensions';
import { ANALYTICS_DIMENSIONS, chipText, escapePattern } from '@/lib/filterBar';
import { effectiveInterval, ENDED_RANGE, readEnd, spanEnded } from '@/lib/follow';
import { SESSION_CHARS, fmtAbsolute, fmtBytes, fmtCompact, fmtDayTick, fmtDuration, fmtInt, fmtMs, fmtPct, fmtPeriodEnd, fmtShare, fmtShortTime, fmtUsd, fmtUsdTick, fmtWeek, keyLifetime, pct, plural, shortId, shortModel, upperFirst, usdTickDigits, weekShort } from '@/lib/format';
import { BUCKET_COUNT, BUCKET_EDGES, PERCENTILE_HINT, histogramCount } from '@/lib/histogram';
import { useAsync, useHashSection, useQueryFilters, useRangeTakenAt, useResizeObserver, useWeekReading, type Loaded } from '@/lib/hooks';
import { accountBound, transformationLabel, transformationOrder } from '@/lib/inputTransformations';
import { rateKind, REFUSED_TITLE } from '@/lib/requests';
import { CLIENT_PATTERNS_PLAIN, linkedKeyIds, refusalLink, requestsLink, type RequestsLinkInput, type RequestsLinkTarget } from '@/lib/requestsLink';
import { words } from '@/lib/routingLog';
import { orderRows, type SortFigure } from '@/lib/sort';
import { HOUR } from '@/lib/time';
import { plainOutputOf } from '@/lib/tokenBar';
import { UNIT_FUNDING, UNIT_POSITIONS, WEEK_SHORT, type UnitPosition, type WeekPosition } from '@/lib/unit';
import { WINDOW_KEYS, windowLabelOf, windowWord } from '@/lib/windows';

/** The dimensions the breakdown route groups on; `all` is the ungrouped total every section reads. */
type GroupBy = Exclude<AnalyticsGroupBy, 'all'>;
/** The four dimensions both the analytics query and the request log filter on. */
type QueryDim = 'accountId' | 'keyId' | 'model' | 'poolId';
/** The two pattern lists, which the analytics query reads whole and the request log as one client pattern at most. */
type PatternDim = 'clients' | 'userAgents';

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
const SESSION_SOURCE = 'From the request log, since rollups carry no session';
const SESSION_SOURCE_NOTE = `${SESSION_SOURCE}. Cache warming figures are unavailable per session`;
const HOLD_LEFT_OUT = "The slow-down's hold is left out";
const SESSION_COST_NOTE = 'Cost split is not available for a session view.';
const AGENT_NEEDS_SESSION = "An agent narrows one session's view, so pick a session first.";
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
// A token count and a request with no cache breakpoint could never hit, so they leave the base.
const hitRatioOf = (g: AnalyticsGroup) => {
  const base = g.cacheHits + g.cacheMisses + g.uncached - g.uncacheable;
  return base > 0 ? g.cacheHits / base : null;
};
const cachedShareOf = (g: AnalyticsGroup) => (inputOf(g) ? g.tokens.cacheRead / inputOf(g) : null);
const usdOrDash = (v: number | null) => (v === null ? '-' : fmtUsd(v));
/** A figure per counted unit, scaled, and null where the figure or the count is unknown or the count is not positive. */
const per = (v: number | null, count: number | null, scale: number) => (v === null || count === null || !(count > 0) ? null : (v / count) * scale);
/** The billed rows' count, the whole less the plan rows, unknown where the plan count is. */
const billedCount = (plan: number | null, whole: number) => (plan === null ? null : whole - plan);
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
    const billed = billedCount(count, whole);
    const equivalent = sides.equivalent === undefined ? null : per(sides.equivalent, count, scale);
    return {
      figures: [...(sides.equivalent === undefined ? [] : [{ text: equivalentOrDash(equivalent, week, weekFactor), word: 'subscription' }]), ...(sides.billed === undefined ? [] : [{ text: usdOrDash(per(sides.billed, billed, scale)), word: 'billed' }])],
      note:
        count === null || billed === null
          ? `${note(whole)} ${word}`
          : sides.billed === undefined
            ? `${note(count)} subscription ${word}`
            : sides.equivalent === undefined
              ? `${note(billed)} billed ${word}`
              : `${note(count)} subscription ${word}, ${note(billed)} billed`,
    };
  };
  return { perRequest: ratio(totals.planRequests, totals.requests, 1, 'requests', fmtInt), perThousand: ratio(totals.planOutputTokens, totals.tokens.output, 1000, 'output tokens', fmtCompact) };
}

/** The native table's columns: Cost, the figure in the chosen week with the billed dollars under it, then the ratios. */
const NATIVE_COLUMNS = analyticsKeysOf('cost', 'native').slice(1).map((k) => k.label);
/** The Traffic heads whose count leaves the refused requests or the client's cancels out, or counts either alone, say
 *  so on hover. */
const TRAFFIC_HEAD_TITLE: Partial<Record<string, string>> = {
  client_canceled: 'Requests the client canceled. They are left out of the error rate',
  error_rate: "The refused requests and the client's cancels are left out, see the Refused and Client canceled columns",
  refused: REFUSED_TITLE,
  requests: 'The refused requests are left out, see the Refused column',
};
/** A native cell's figure over its billed line, the full reading on its title. */
const nativeCell = (cell: { main: string; sub: string; title: string }): ReactNode => (
  <span title={cell.title || undefined}>
    <CostLines cell={cell} />
  </span>
);
/**
 * A group's native row: its requests, its cost (the plan rows' figure in the chosen week with the billed dollars under
 * it, the hint on its title where the row is one account's), and the figure per plan request and per 1k plan output
 * tokens with the billed dollars per billed request and per 1k billed output tokens under it, a side with no rows left
 * out and a figure with no count a dash.
 */
function nativeRowOf(g: AnalyticsGroup, week: WeekPosition, weekFactor: number, hint: string | null): { cells: ReactNode[]; figures: Partial<Record<string, SortFigure>>; key: string; label: string } {
  const sides = sidesOf(g);
  const figure = sides.equivalent === undefined ? null : g.costEquivalent;
  // Undefined leaves a side with no rows out of the cell, as the tiles leave it out.
  const both = (equivalent: number | null, billed: number | null, shown: string | null = null) => nativeCell(nativeCellOf(sides.equivalent === undefined ? undefined : equivalent, sides.billed === undefined ? undefined : billed, week, weekFactor, shown));
  const ratio = (planPer: number | null, planCount: number | null, whole: number, scale: number) => both(planPer, per(sides.billed ?? null, billedCount(planCount, whole), scale));
  const perRequest = per(figure, g.planRequests, 1);
  const perThousand = per(figure, g.planOutputTokens, 1000);
  // The route labels every group, the model grouping's empty key included, since the ledger labels that key itself.
  return {
    cells: [fmtInt(g.requests), both(figure, sides.billed ?? null, hint), ratio(perRequest, g.planRequests, g.requests, 1), ratio(perThousand, g.planOutputTokens, g.tokens.output, 1000)],
    // The Cost key reads the plan rows' figure, 0 for a group of billed rows alone and null where a plan row priced
    // nothing, so an unpriced group files last under either direction.
    figures: { cost: g.costEquivalent, per_1k_output: perThousand, per_request: perRequest, requests: g.requests },
    key: g.key,
    label: g.label,
  };
}

type Pct = Record<(typeof PERCENTILES)[number]['key'], number | null>;

/** The routes answer one output sum; the chart stacks its thinking share and the rest, so the page derives the rest. */
type SplitTokens = AnalyticsGroup['tokens'] & { plainOutput: number };
const withPlainOutput = (t: AnalyticsGroup['tokens']): SplitTokens => ({ ...t, plainOutput: plainOutputOf(t) });

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
const PROBE_COUNTERS = ['failed', 'held', 'migrated', 'ok', 'planOk', 'skipped'] as const;
/** Whether a group counts a request. A group of refused requests alone counts none, and only the Traffic table's
 *  Refused column has a figure for it. */
const countsRequest = (g: { requests: number }): boolean => g.requests > 0;
/** A read's answer with the range's start and end it was read for and the instant it left at. */
type Stamped<T> = { answer: T; endMs: number; fromMs: number; readAt: number };
/** Whether a bucket chart's first and last buckets each hold only part of their width. */
type PartialEnds = { first: boolean; last: boolean };
const WHOLE_ENDS: PartialEnds = { first: false, last: false };
/**
 * A read that runs while `reads` holds and answers null without a request otherwise (a hidden tab, the rollups in a
 * session view, a side the unit leaves out). It holds its refresh while it reads nothing, since each empty answer still
 * lands and re-renders the page, and `reads` joins the deps, so the opening of its tab reads it once.
 */
function useGatedRead<T>(reads: boolean, load: () => Promise<T>, deps: readonly unknown[], reload: { paused: boolean; refreshMs: number }): Loaded<T | null> {
  return useAsync(() => (reads ? load() : Promise.resolve(null)), [...deps, reads], { paused: reload.paused || !reads, refreshMs: reload.refreshMs });
}

/**
 * Every key the range holds on one dimension, read once per range so a filter never hides its own options. With no
 * filter set and the page grouped on this dimension, the page's breakdown is that very read and answers for it. While
 * the source is out (the breakdown reloading, perhaps still on the previous grouping, the own read starting once a
 * filter is set, or either one reading a new range), the last answer that was current stands in, so a list never
 * blanks and only ever offers this dimension's own keys.
 */
function useDimensionKeys(from: string, to: string, groupBy: GroupBy, generation: number, shared?: Loaded<AnalyticsGroup[] | null>): Loaded<AnalyticsGroup[] | null> {
  const own = useAsync(() => (shared ? Promise.resolve(null) : api.getAnalyticsBreakdown({ from, groupBy, to })), [from, groupBy, to, generation, shared !== undefined]);
  const source = shared ?? own;
  const current = source.current;
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
  const { address, filters: a, replaceLater, update: rawUpdate } = useQueryFilters(ANALYTICS_CODEC);
  // Every section but the Burn reads the default grouping under the Burn's own session grouping.
  const pageBy = a.by;
  const groupBy: GroupBy = pageBy === 'session' ? 'key' : pageBy;
  const clientGroups = a.groups;
  const includeProbes = a.probes;
  const filtersRef = useRef<HTMLDivElement>(null);
  const filtersHeight = useStickyHeight(filtersRef);
  // 5 s when the page opens, and page state, so a link or a reload opens on it.
  const [intervalMs, setIntervalMs] = useState(5_000);
  // The Burn's Cost reference dialog and the bar's menus and popovers hold every read through the page's host.
  const { cause: pauseCause, host } = useHoldHost();
  const paused = pauseCause !== null;
  const rangeTakenAt = useRangeTakenAt(a.range);
  const [tab, chooseTab] = useHashSection(SECTIONS, 'traffic');
  // A session id is one session per key lifetime, so a drill-down from a session row names the lifetime too. A session
  // without one reads unscoped.
  const sessionKey = a.session && a.key ? a.key : null;
  const sessionAgent = a.session && a.agentId ? a.agentId : null;
  const { setUnit, unit, week } = useAppSettings();
  const dollars = dollarsOf(unit);
  const funding = UNIT_FUNDING[unit];
  // A shared link lands on its position, then the choice persists like any other: the parameter is read into the
  // setting and dropped, so the address stays clean and one control owns the unit.
  useEffect(() => {
    if (a.unit === '') return;
    const known = UNIT_POSITIONS.find((p) => p === a.unit);
    if (known !== undefined) setUnit(known);
    replaceLater({ ...a, unit: '' });
  }, [a, replaceLater, setUnit]);
  // The hook keeps a list's identity while its value stands, so a unit change re-issues none of the reads they key.
  const filters = useMemo<Record<QueryDim, string[]>>(() => ({ accountId: a.accounts, keyId: a.keys, model: a.models, poolId: a.pools }), [a.accounts, a.keys, a.models, a.pools]);
  // No route groups on a session, so it filters the page from the side rather than joining `filters`. The address
  // reads an absent session as the empty string.
  const sessionId = a.session === '' ? null : a.session;
  // The request log reads one client pattern, so a session view's log read carries the one where exactly one stands.
  const logClient = a.clients.length === 1 ? a.clients[0] : undefined;
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
   * The bar's one writer, so the chips, Add filter and the address cannot disagree. Clear all's one patch commits
   * whole. A patch that changes nothing, such as a flag chosen again, moves no end and reads nothing.
   */
  const update = (patch: Partial<AnalyticsAddress>) => {
    const next = analyticsPatch(a, patch);
    if (Object.entries(next).every(([field, value]) => JSON.stringify(value) === JSON.stringify(a[field as keyof AnalyticsAddress]))) return;
    commit(next);
  };
  const commitGroups = (next: string) => commit({ groups: next });
  const changeGroupBy = (next: BurnGroup) => commit({ by: next });
  const groupByTitles = GROUP_BY_TITLES[tab];
  // The groups split what the grouping splits, so the button stands disabled where the select does.
  const groupByDisabled = 'disabledTitle' in groupByTitles ? groupByTitles.disabledTitle : undefined;

  // The query bounds come from the address's instants, which keep the milliseconds the inputs' minutes drop.
  const custom = a.range.preset === 'custom';
  const span = useMemo(() => {
    const { from, to } = addressWindow(a.range, '24h', now);
    return { fromMs: from, toMs: readEnd(custom, to, now) };
  }, [a.range, custom, now]);
  const fromIso = iso(span.fromMs);
  const toIso = iso(span.toMs);
  const ended = spanEnded(custom, span.toMs, rangeTakenAt);
  const polledMs = effectiveInterval(intervalMs, ended);
  // A session view draws no Cost table, so its Cost tab has no group table to order. A key the open tab's table lacks
  // draws Name while the address keeps it, so the tab that has the column orders by it again.
  const tabKeys = analyticsKeysOf(sessionId !== null && tab === 'cost' ? null : tab, unit);
  const sort = tabKeys.some((k) => k.id === a.sort) ? a.sort : 'name';
  const dir = sortDirOf(tabKeys, { dir: sort === a.sort ? a.dir : '', sort });
  const tableOrder = { dir, key: sort };

  const agentGroups = useMemo(() => (groupBy === 'user_agent' && clientGroups.trim() ? clientGroups.split('\n').map((s) => s.trim()).filter(Boolean) : undefined), [clientGroups, groupBy]);
  const query = useMemo<AnalyticsQuery>(
    () => ({ accountIds: filters.accountId, clients: a.clients, from: fromIso, groupBy, includeProbes, keyIds: filters.keyId, models: filters.model, poolIds: filters.poolId, to: toIso, userAgentGroups: agentGroups, userAgents: a.userAgents }),
    [agentGroups, a.clients, a.userAgents, filters, fromIso, groupBy, includeProbes, toIso],
  );
  // The route narrows the rows a session view folds by one client pattern at most, and the fold applies both pattern
  // lists over what it holds: several client patterns narrow here alone, a cursor page that crossed a filter change
  // may be wider than the patterns, and the user agent matches its folded token, as the unscoped page's rollups do,
  // where the route matches the raw header.
  const patternOptions = useMemo(() => ({ agentPatterns: a.userAgents, clientPatterns: a.clients }), [a.clients, a.userAgents]);
  // A custom span holds its end, so no dependency moves at the interval: its window's reads poll at it instead, and the
  // figures stay drawn until the new answer lands, where a request that finished late shows, in the session log when it
  // falls on the first page. A preset's end moves to now at a boundary, and that change reads again.
  const reload = { paused, refreshMs: custom ? polledMs : 0 };
  // A session view answers every section from the request log, so the rollup reads stay home while one is set.
  const series = useGatedRead(sessionId === null, () => api.getAnalyticsSeries(query), [query, sessionId, generation], reload);
  const breakdown = useGatedRead(sessionId === null, () => api.getAnalyticsBreakdown(query), [query, sessionId, generation], reload);
  const byModel = useGatedRead(tab === 'cache' && sessionId === null, () => api.getAnalyticsBreakdown({ ...query, groupBy: 'model' }), [query, sessionId, tab, generation], reload);
  // The Cache warming tab counts probe rows whatever the toggle says, its by-pool table among its figures.
  const byPool = useGatedRead(tab === 'cache-warming' && sessionId === null, () => api.getAnalyticsBreakdown({ ...query, groupBy: 'pool', includeProbes: true }), [query, sessionId, tab, generation], reload);
  // The log's last answer with the deps it answers, so an auto reload of the same deps reads the first page alone and
  // the later pages stand as the walk loaded them. Other deps walk the pages again. The ref names the current deps from
  // the moment a read starts, so a read under earlier deps that settles later keeps nothing.
  const heldLog = useRef<{ answer: SessionRequests | null; deps: string } | null>(null);
  const logKey = [sessionAgent, logClient, filters, fromIso, includeProbes, sessionId, sessionKey, toIso, generation];
  const logDeps = JSON.stringify(logKey);
  // The load runs unscoped as well, so its reset drops the held answer there and a session view entered again walks its
  // pages again.
  const log = useAsync(
    () => {
      if (heldLog.current?.deps !== logDeps) heldLog.current = { answer: null, deps: logDeps };
      if (sessionId === null) return Promise.resolve({ partial: false, requests: [] as RequestLog[] });
      const held = heldLog.current.answer ?? undefined;
      return loadSessionRequests(
        (cursor) =>
          api
            .listRequests({ accountIds: filters.accountId, agentId: sessionAgent ?? undefined, client: logClient, cursor, from: fromIso, includeProbes, keyIds: sessionKey && filters.keyId.length === 0 ? [sessionKey] : filters.keyId, limit: SESSION_PAGE_SIZE, models: filters.model, poolIds: filters.poolId, session: sessionId, to: toIso })
            // The route reads one key list, so a lifetime beside a key filter is applied here as well.
            .then((page) => ({ ...page, requests: page.requests.filter((r) => sessionKey === null || (r.keyId !== null && keyLifetime(r.keyId, r.keyGeneration) === sessionKey)) })),
        SESSION_PAGE_LIMIT,
        held,
      ).then((answer) => {
        if (heldLog.current?.deps === logDeps) heldLog.current = { answer, deps: logDeps };
        return answer;
      });
    },
    logKey,
    { paused: paused || sessionId === null, refreshMs: reload.refreshMs },
  );
  const accountRollups = useGatedRead(tab === 'accounts', () => api.listAccountRollups(fromIso, toIso), [fromIso, toIso, tab, generation], reload);
  // The fold rates the hour still running over the part of it this read saw, so the answer keeps its end and instant,
  // and the range pill names its window from the range's start it was read for. The instant is the read's own: the
  // range clock stands still while another tab is open, and a read made later in the same hour has seen more of it.
  const burn = useGatedRead(
    tab === 'burn',
    () => {
      const readAt = clock.now();
      return readBurn(api, { from: fromIso, to: toIso }, pageBy, { accountIds: filters.accountId, keyIds: filters.keyId, poolIds: filters.poolId, userAgentGroups: agentGroups }).then((read) => ({ ...read, endMs: span.toMs, fromMs: span.fromMs, readAt }));
    },
    [fromIso, toIso, pageBy, filters.accountId, filters.keyId, filters.poolId, agentGroups, tab, generation],
    reload,
  );
  /** A read with the range's start and end and the instant it left at, so its card's notes read its own answer. */
  const stamped = <T,>(read: Promise<T>): Promise<Stamped<T>> => {
    const readAt = clock.now();
    return read.then((answer) => ({ answer, endMs: span.toMs, fromMs: span.fromMs, readAt }));
  };
  // The registry's list, for the account slots alone: the range's option list holds the accounts with traffic and
  // differs from the Dashboard's, and one account keeps one color across the views. It reads with the window, since
  // each answer redraws every chart of the open tab for colors, hints and resets that seldom move.
  const registry = useAsync(() => api.listAccounts(), [fromIso, toIso, generation], reload);
  const slotOf = useMemo(() => accountSlots((registry.data ?? []).map((a) => a.id)), [registry.data]);
  // A weekly row changes once a week, so the unused allowance card reads once per range and Accounts filter, under a
  // preset again at each move of the page's `now`, again on Refresh and once the registry's read answers a weekly
  // window of an account under the filter past its reset, never at the list's cadence, under a custom range too. The
  // registry's first answer moves no reset, so the page's load reads once.
  const weeklyResetsAt = registry.data === null ? null : weeklyResetsKey(registry.data.filter((a) => filters.accountId.length === 0 || filters.accountId.includes(a.id)));
  const [resets, setResets] = useState({ at: weeklyResetsAt, moved: 0 });
  if (weeklyResetsAt !== resets.at) setResets({ at: weeklyResetsAt, moved: resets.at === null ? resets.moved : resets.moved + 1 });
  // The weekly resets of the range and of the week before its start, and the Accounts filter alone narrows them.
  const unusedAllowance = useAsync(() => (tab === 'accounts' ? api.getAnalyticsUnusedAllowance(rangeAllowanceSpan(span.fromMs, span.toMs), { accountIds: filters.accountId }) : Promise.resolve(null)), [tab, filters.accountId, fromIso, toIso, generation, resets.moved]);
  // The cost model, read once per page load whatever the tab: the chosen week's scale reads every week figure the page
  // draws, the plan chart's line category reads each account's stored tier against its capacity rows, and the unused
  // allowance card prices its figures through it.
  const { reading, settings: costSettings } = useWeekReading(week);
  const costs = costSettings.data?.subscriptionCosts ?? null;
  const { scale } = reading;
  const sized = useMemo(() => (burn.data && costs ? sizedAccounts(burn.data.accounts, costs) : null), [burn.data, costs]);
  // The range pill's burn sentence starts at the answer's earliest point, parsed once per answer, not on each render.
  const burnAnswer = burn.current;
  const burnFirstAt = useMemo(() => {
    const points = burnAnswer?.answer.points ?? [];
    return points.length === 0 ? null : points.reduce((at, p) => Math.min(at, Date.parse(p.at)), Number.POSITIVE_INFINITY);
  }, [burnAnswer]);
  const rewrites = useGatedRead(
    tab === 'traffic',
    () => stamped(api.getAnalyticsRewrites({ from: fromIso, to: toIso }, { accountIds: filters.accountId, poolIds: filters.poolId })),
    [fromIso, toIso, filters.accountId, filters.poolId, tab, generation],
    reload,
  );
  const refusalCategories = useGatedRead(tab === 'traffic', () => api.getAnalyticsRefusalCategories(query), [query, tab, generation], reload);
  // The counts route reads the range alone, which the card's subtitle says, since it carries no filter.
  const guardrailCounts = useGatedRead(tab === 'traffic', () => api.getAnalyticsGuardrails({ from: fromIso, to: toIso }), [fromIso, toIso, tab, generation], reload);
  // Side groupings on the page's own query, as byModel is. A session view folds them from the log.
  const bySpeed = useGatedRead((tab === 'traffic' || tab === 'cost') && sessionId === null, () => stamped(api.getAnalyticsSeries({ ...query, groupBy: 'speed' })), [query, sessionId, tab, generation], reload);
  const byEffort = useGatedRead(tab === 'traffic' && sessionId === null, () => api.getAnalyticsBreakdown({ ...query, groupBy: 'effort' }), [query, sessionId, tab, generation], reload);
  // A dollar side narrows the three cost-bearing sections to the rows that side paid for, a filter the page's reads
  // cannot carry (Traffic, Latency and the Cache token charts read every row), so those sections read a second query
  // while one of them shows. The whole reads the page's own.
  const fundedTab = tab === 'cache' || tab === 'cache-warming' || tab === 'cost';
  const funded = sessionId === null && (unit === 'usd' || unit === 'equivalent');
  // The funded series feeds the Cache card's cost bars and the Cost tab's Cost by group chart, and Cache warming draws
  // neither.
  const seriesTab = tab === 'cache' || tab === 'cost';
  const costSeries = useGatedRead(funded && seriesTab, () => stamped(api.getAnalyticsSeries({ ...query, funding })), [query, funding, funded, seriesTab, generation], reload);
  // The Cache warming card's Probe cost tile and the Probes by outcome legend read this breakdown on its tab, and they
  // read the probe rows counted under every position, the whole included: a breakdown that does not count probes drops
  // a group of plan probes alone and keeps a billed probe's dollars, even in a group with no request, whether a dollar
  // side dropped the group's real rows or the group is probe-only in the whole (the probe kind, an idle account over a
  // short range), and the three positions add up only when all three keep it. Under the whole the read carries no
  // funding.
  const warming = sessionId === null && tab === 'cache-warming';
  const costBreakdown = useGatedRead(
    (funded && fundedTab) || warming,
    () => api.getAnalyticsBreakdown({ ...query, funding: funded ? funding : undefined, includeProbes: query.includeProbes || warming }),
    [query, funding, funded, fundedTab, warming, generation],
    reload,
  );
  // The Saved by probes chart's series twin of that breakdown, counting probes under the same funding: the page's
  // series counts them only when the address asks, and a bucket left with plan probes alone would draw no probe cost
  // there.
  const warmingSeries = useGatedRead(warming, () => api.getAnalyticsSeries({ ...query, funding: funded ? funding : undefined, includeProbes: true }), [query, funding, funded, warming, generation], reload);
  // Probes by outcome counts the probes of both sides. Under native and All in dollars the two reads above carry no
  // funding and answer it, and under a dollar side, whose reads keep that side's rows, the chart reads the whole once
  // more. With the Probes toggle on, that read is the page's own series request, so the chart reads `series` instead.
  const probeSeries = useGatedRead(warming && funded && !includeProbes, () => api.getAnalyticsSeries({ ...query, includeProbes: true }), [query, funded, warming, generation], reload);
  // Native pairs the whole read's plan probes with the billed probes' dollars, which no member of the whole carries, so
  // the Cache warming tab reads the billed rows once more, counting probes as the whole read does.
  const nativeWarming = warming && unit === 'native';
  const billedBreakdown = useGatedRead(nativeWarming, () => api.getAnalyticsBreakdown({ ...query, funding: 'billed', includeProbes: true }), [query, nativeWarming, generation], reload);
  // And the chart's billed twin, whose dollars the native tooltip pairs with the plan side's equivalents.
  const billedWarmingSeries = useGatedRead(nativeWarming, () => api.getAnalyticsSeries({ ...query, funding: 'billed', includeProbes: true }), [query, nativeWarming, generation], reload);
  // A dollar side narrows Cache by model's two cost cells alone, its token columns reading every row, so the side's
  // rows by model come from a read of their own beside the page's.
  const byModelFunded = useGatedRead(tab === 'cache' && funded, () => api.getAnalyticsBreakdown({ ...query, funding, groupBy: 'model' }), [query, funding, funded, tab, generation], reload);
  // The by-pool table's Probe cost cell reads a dollar side's probes, and under native the billed probes beside the
  // whole read's equivalent. Probes count, so a pool left with probe rows alone on that side keeps its figure.
  const poolFunding = nativeWarming ? 'billed' : warming && funded ? funding : undefined;
  const fundedByPool = useGatedRead(poolFunding !== undefined, () => api.getAnalyticsBreakdown({ ...query, funding: poolFunding, groupBy: 'pool', includeProbes: true }), [query, poolFunding, generation], reload);
  const placements = useGatedRead(
    tab === 'cache' && sessionId === null,
    () => api.analyticsPlacements({ accountIds: filters.accountId, clients: a.clients, from: fromIso, keyIds: filters.keyId, models: filters.model, poolIds: filters.poolId, to: toIso, userAgents: a.userAgents }),
    [fromIso, toIso, filters.accountId, filters.keyId, filters.model, filters.poolId, a.clients, a.userAgents, sessionId, tab, generation],
    reload,
  );
  const budgets = useAsync(() => (tab === 'burn' ? api.analyticsBudgets() : Promise.resolve(null)), [tab]);
  // The agent stands in force in a session view alone, the one view whose reads send it.
  const controls: Record<AnalyticsControl, boolean> = { accounts: filters.accountId.length > 0, agent: sessionAgent !== null, clients: a.clients.length > 0, keys: filters.keyId.length > 0, models: filters.model.length > 0, pools: filters.poolId.length > 0, probes: includeProbes, session: sessionId !== null, userAgents: a.userAgents.length > 0 };
  const inForce = new Set((Object.keys(controls) as AnalyticsControl[]).filter((c) => controls[c]));
  // With no filter and no session the page's breakdown is the range's whole key list on its grouping.
  const plain = inForce.size === 0 && agentGroups === undefined;
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
  // The memos below key on a read's current answer through a local, since the hooks lint takes a `.current` in a
  // dependency list for a ref's.
  const rows = log.current?.requests ?? NO_REQUESTS;
  // The folds below share one pass of the pattern filter, and the key generations come from every row, since the kept
  // rows alone would label an earlier key holder as the live key.
  const kept = useMemo(() => keptRows(rows, patternOptions), [rows, patternOptions]);
  const latest = useMemo(() => latestGenerations(rows), [rows]);
  const points = useMemo(() => (stale ? [] : sessionId === null ? (series.data ?? []) : foldRequests(kept, rangeBucketMs, fromIso, toIso, groupBy, { agentGroups }, latest)), [stale, sessionId, series.data, kept, rangeBucketMs, fromIso, toIso, groupBy, agentGroups, latest]);
  const buckets = useMemo(() => bucketsOf(points), [points]);
  // The route chooses the bucket width; reading it back off two points keeps the ticks honest when that
  // choice differs from what the range rule would have picked.
  const bucketMs = buckets.length > 1 ? buckets[1].at - buckets[0].at : rangeBucketMs;
  /** A bucket chart's two flags, off its buckets, their width, the range it was read for and the instant it left at. A
   *  route sums whole hours from the hour that holds the range's start up to the hour ceiling of its end, and a session
   *  view's fold of the request log reads the range itself. */
  const partialEndsOf = (points: readonly { at: number }[], window: { fromMs: number; toMs: number }, at: number, routed: boolean): PartialEnds => {
    const covered = coveredWindow(routed ? 'rollup' : 'session', window, at, null);
    return {
      first: firstBucketPartial(points, covered.fromMs),
      last: lastBucketPartial(points, points.length > 1 ? points[1].at - points[0].at : rangeBucketMs, covered.toMs),
    };
  };
  const partial = partialEndsOf(buckets, span, now, sessionId === null);
  /** A stamped read's two flags, off its own buckets, its own range and the instant it left at. Each stamped read is a
   *  route's read, the rewrites read in a session view too, so the whole hours hold. False until the read answers. */
  const stampedPartial = <T,>(read: Stamped<T> | null, pointsOf: (answer: T) => readonly { at: string }[]): PartialEnds => {
    if (read === null) return WHOLE_ENDS;
    // The flags read the first point, the second for the width and the last, so a long answer parses those three alone.
    const all = pointsOf(read.answer);
    const ends = all.length <= 3 ? all : [all[0], all[1], all[all.length - 1]];
    return partialEndsOf(ends.map((p) => ({ at: Date.parse(p.at) })), { fromMs: read.fromMs, toMs: read.endMs }, read.readAt, true);
  };
  const groups = useMemo(() => (stale ? [] : sessionId === null ? (breakdown.data ?? []) : foldGroups(kept, groupBy, { agentGroups }, latest)), [stale, sessionId, breakdown.data, kept, groupBy, agentGroups, latest]);
  // Every group table but Traffic leaves out a group of refused requests alone.
  const requestGroups = useMemo(() => groups.filter(countsRequest), [groups]);
  const byPoolAnswer = byPool.current;
  const poolGroups = useMemo(() => (sessionId === null ? (byPoolAnswer ?? []) : tab === 'cache-warming' ? foldGroups(kept, 'pool', { agentGroups }, latest) : []), [sessionId, byPoolAnswer, tab, kept, agentGroups, latest]);
  const bySpeedAnswer = bySpeed.current?.answer;
  const speedPoints = useMemo(
    () => (sessionId === null ? (bySpeedAnswer ?? []) : tab === 'traffic' || tab === 'cost' ? foldRequests(kept, rangeBucketMs, fromIso, toIso, 'speed', { agentGroups }, latest) : []),
    [sessionId, bySpeedAnswer, tab, kept, rangeBucketMs, fromIso, toIso, agentGroups, latest],
  );
  const byEffortAnswer = byEffort.current;
  const effortGroups = useMemo(() => (sessionId === null ? (byEffortAnswer ?? []) : tab === 'traffic' ? foldGroups(kept, 'effort', { agentGroups }, latest) : []), [sessionId, byEffortAnswer, tab, kept, agentGroups, latest]);
  const totals = useMemo(() => totalsOf(groups), [groups]);
  // A session fold has no warming facts, so those figures read unavailable rather than zero.
  const { keptWarm } = totals;
  const probesUnmeasured = totals.fromRequests === true;
  // The cost-bearing sections' rows under the position: the funded query's under a dollar side, the page's own under
  // the whole, and in a session view the rows folded by who paid for them.
  const costSeriesAnswer = costSeries.current?.answer;
  const costPoints = useMemo(() => {
    if (sessionId !== null) return funding === undefined ? points : stale || !fundedTab ? [] : foldRequests(kept, rangeBucketMs, fromIso, toIso, groupBy, { agentGroups, funding }, latest);
    return funded ? (costSeriesAnswer ?? []) : points;
  }, [sessionId, funding, funded, points, stale, fundedTab, kept, rangeBucketMs, fromIso, toIso, groupBy, agentGroups, latest, costSeriesAnswer]);
  const costBreakdownAnswer = costBreakdown.current;
  const costGroups = useMemo(() => {
    if (sessionId !== null) return funding === undefined ? groups : stale || !fundedTab ? [] : foldGroups(kept, groupBy, { agentGroups, funding }, latest);
    return funded || warming ? (costBreakdownAnswer ?? []) : groups;
  }, [sessionId, funding, funded, warming, groups, stale, fundedTab, kept, groupBy, agentGroups, latest, costBreakdownAnswer]);
  // With Include probes off the route answers a group of billed probes alone for its dollars beside no request, so each
  // Cost table keeps a group whose dollars it draws are above zero, as the total and the Cost by chart keep them: the
  // native table's billed dollars and the dollar table's cost.
  const nativeCostGroups = useMemo(() => costGroups.filter((g) => countsRequest(g) || g.billedUsd > 0), [costGroups]);
  const dollarCostGroups = useMemo(() => costGroups.filter((g) => countsRequest(g) || g.costUsd > 0), [costGroups]);
  const costBuckets = useMemo(() => (costPoints === points ? buckets : bucketsOf(costPoints)), [costPoints, points, buckets]);
  const costTotals = useMemo(() => totalsOf(costGroups), [costGroups]);
  // A session view folds the request log, which the page reads with the toggle, so its figure holds no probe while the
  // toggle is off. Dollar equivalent reads the plan rows, which hold no billed probe.
  const costHead = costHeadOf(unit, costTotals, week, scale, !includeProbes && sessionId === null && unit !== 'equivalent');
  // A session view's figures come from the request log, so a head that draws them names that source after them.
  const sourced = (meta: string | undefined): string | undefined => (meta === undefined || sessionId === null ? meta : sentences([meta, SESSION_SOURCE]));
  const nativeRatios = scale === null ? null : nativeRatiosOf(costTotals, week, scale);
  // A session fold has no cost split either, so the donut and the cache cost card read unavailable in a session view.
  const costSplit = knownSplit(costTotals.cost);
  // The routes answer a named account's own-window shares on every group under any grouping, and under the account
  // grouping each account's on its own group. So a row under either reads one account's hint, and the tiles under one
  // named account sum its groups, whose shares add since they share its windows. A group summing several accounts and
  // the probe, saving, net and cache figures carry none. The registry names the plan and the tier, so a figure carries
  // no hint while it is out.
  const namedAccount = filters.accountId.length === 1 ? filters.accountId[0] : null;
  const hintOf = (id: string, shares: Record<string, number | null> | null): string | null => ownShareHint(registry.data?.find((x) => x.id === id), shares);
  const rowHint = (g: AnalyticsGroup): string | null => (groupBy === 'account' ? hintOf(g.key, g.costShares) : namedAccount === null ? null : hintOf(namedAccount, g.costShares));
  const oneGroup = useMemo(() => (namedAccount === null ? null : oneAccountGroupOf(costTotals, costGroups)), [costGroups, costTotals, namedAccount]);
  const tileHint = namedAccount === null || oneGroup === null ? null : hintOf(namedAccount, oneGroup.costShares);
  const burnAccount = filters.accountId.length === 1 ? filters.accountId[0] : null;
  // The plan chart sizes its lines by tier where each group holds one account's rows.
  const burnSizes = pageBy === 'account' || burnAccount !== null;
  // Where the cards size their lines by tier, which the cost model holds, the three plan cards wait on the model, so no
  // line reads tier unknown before the tiers land. Elsewhere each card's figures are the burn read's own.
  const burnWait: WeekReading | null = burnSizes && costs === null ? pendingReading(costSettings.error, week) : null;

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
  const speedLoading = sessionId === null ? bySpeed.current === null : loading;
  const effortLoading = sessionId === null ? byEffort.current === null : loading;
  const speedError = sessionId === null ? bySpeed.error : null;
  // The funded reads stand or fall on their own: while one is out or stale the cost surfaces read loading, and its
  // failure draws in place of their figures, so no figure of one position stands under another's subtitle.
  const costLoading = funded
    ? (seriesTab && costSeries.current === null) || costBreakdown.current === null
    : warming
      ? costBreakdown.current === null || (nativeWarming && billedBreakdown.current === null)
      : loading;
  const costError = funded ? ((seriesTab ? costSeries.error : null) ?? costBreakdown.error) : warming ? (costBreakdown.error ?? billedBreakdown.error) : null;
  const costFailure = failed(costError);
  // Under native every week figure waits on the scale: a surface draws its skeleton while the cost model is out and the
  // reading's error in its place, never a Pro-week figure under another week's label.
  const weekWait = unit === 'native' && scale === null;
  const figureLoading = costLoading || weekWait;
  const weekFailure = (error: string | null) => failed(error) ?? (weekWait ? reading.failure : null);
  const figureFailure = weekFailure(costError);
  const warmingError = sessionId === null ? (warmingSeries.error ?? billedWarmingSeries.error) : null;
  const modelError = byModel.error ?? byModelFunded.error;
  const poolError = sessionId === null ? (byPool.error ?? fundedByPool.error) : null;
  const billedBreakdownAnswer = billedBreakdown.current;
  const billedTotals = useMemo(() => (nativeWarming && billedBreakdownAnswer !== null ? sumGroups(billedBreakdownAnswer) : null), [nativeWarming, billedBreakdownAnswer]);
  const warmingTile = (lead: string, sides: ReturnType<typeof probeSidesOf>, usd: number) => warmingTileOf(unit, costTotals, figureLoading, sessionId !== null, reading, lead, sides, usd);
  const probeTile = warmingTile(`${fmtInt(upstreamProbes(costTotals))} probes sent`, probeSidesOf(costTotals, billedTotals, 'cost'), costTotals.probeCostUsd);
  const savingTile = warmingTile(keptWarmRequests(costTotals), savingSidesOf(costTotals, billedTotals, 'gross'), costTotals.savingUsd);
  const netTile = warmingTile('less the probe cost', savingSidesOf(costTotals, billedTotals, 'net'), netSaving(costTotals.savingUsd, costTotals.probeCostUsd));
  const warmingPoints = warmingSeries.current ?? NO_POINTS;
  const warmingBuckets = useMemo(() => bucketsOf(warmingPoints), [warmingPoints]);
  const billedWarmingPoints = nativeWarming ? billedWarmingSeries.current : null;
  const warmingLoading = warmingSeries.current === null || (nativeWarming && billedWarmingSeries.current === null);
  // Under the whole the chart's legend reads the tab's breakdown, and under a dollar side it sums the chart's own
  // buckets. A session view draws the page's fold of the log.
  const fundedProbes = includeProbes ? series : probeSeries;
  const probeAnswer = (funded ? fundedProbes : warmingSeries).current;
  const { probeBuckets, probeTotals } = useMemo(() => {
    const raw = sessionId !== null ? buckets : funded ? bucketsOf(probeAnswer ?? NO_POINTS) : warmingBuckets;
    // The counters are whole numbers, so the buckets sum to the answer's whole. `probeParts` floors the whole rather
    // than each bucket, since its floor at zero does not add.
    const summed = { failed: 0, held: 0, migrated: 0, ok: 0, planOk: 0, skipped: 0 };
    if (funded) for (const b of raw) for (const k of PROBE_COUNTERS) summed[k] += b.probes[k];
    return { probeBuckets: raw.map((b) => ({ ...b, probes: probeParts(b.probes) })), probeTotals: probeParts(sessionId !== null ? totals.probes : funded ? summed : costTotals.probes) };
  }, [sessionId, funded, probeAnswer, warmingBuckets, buckets, totals.probes, costTotals.probes]);
  const probeLoading = sessionId === null ? probeAnswer === null || (!funded && costBreakdown.current === null) : loading;
  const probeFailure = sessionId === null ? failed(funded ? fundedProbes.error : (warmingSeries.error ?? costBreakdown.error)) : null;
  const byModelFundedAnswer = byModelFunded.current;
  const fundedModels = useMemo(() => new Map((byModelFundedAnswer ?? []).map((g) => [g.key, g])), [byModelFundedAnswer]);
  const fundedByPoolAnswer = fundedByPool.current;
  const fundedPools = useMemo(() => new Map((fundedByPoolAnswer ?? []).map((g) => [g.key, g])), [fundedByPoolAnswer]);
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
  // session's key lifetime alone, and its agent where one stands, so its links carry both. The request log takes one
  // client pattern, so several draw every count plain. The user agent patterns ride joined, so any of them keeps a
  // count plain, since the request log matches the raw header.
  const linkInput: ReasonLink = {
    filters: { accountIds: filters.accountId, agentId: sessionAgent ?? '', clientPattern: logClient ?? '', includeProbes, keyIds: linkedKeyIds(filters.keyId, sessionKey), models: filters.model, poolIds: filters.poolId, sessionId, userAgentPattern: a.userAgents.join('\n') },
    window: { exact: sessionId !== null, fromMs: span.fromMs, nowMs: now, toMs: span.toMs },
  };
  const plainLinks = a.clients.length > 1;
  // The refusal route reads the page's own key selection, never a session view's key lifetime, and `refusalLink` opens
  // the whole hours the route counts and leaves the session and the agent out.
  const refusalInput: ReasonLink = { ...linkInput, filters: { ...linkInput.filters, keyIds: filters.keyId } };
  // With no session the page reads the rollups, which carry no agent, so an Agent chip would narrow nothing.
  const disabled: Record<string, string> = sessionId === null ? { agentId: AGENT_NEEDS_SESSION } : {};

  return (
    <PopoverHost.Provider value={host}>
      <div className="stack">
        <FilterBar
          address={address}
          dimensions={dimensions}
          disabled={disabled}
          filters={a}
          options={barOptions}
          range={{ now: clock.now, sentence: rangeSentenceOf(tab, sessionId !== null, coveredWindow('rollup', span, now, null), burnAnswer, burnFirstAt), window: { from: span.fromMs, to: span.toMs } }}
          ref={filtersRef}
          refresh={{
            disabled: ended ? ENDED_RANGE : undefined,
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
          <GroupByControl {...groupByTitles} onChange={changeGroupBy} options={GROUP_OPTIONS} value={pageBy} />
          {/* A pick reorders the rows the page holds, so it writes the address alone and moves no window's end. */}
          <SortControl disabledTitle={tabKeys.length === 1 ? "This tab's tables keep their own order" : undefined} keys={tabKeys} onChange={rawUpdate} value={{ dir: sort === a.sort ? a.dir : '', sort }} />
          {groupBy === 'user_agent' && <GroupsButton disabledTitle={groupByDisabled} onCommit={commitGroups} value={clientGroups} />}
        </FilterBar>
        {log.current?.partial && (
          <p className="muted" role="status">
            Partial: the first {fmtInt(SESSION_PAGE_LIMIT * SESSION_PAGE_SIZE)} requests of the range
          </p>
        )}

        <SectionBar sections={SECTIONS} shown={tab} onChoose={chooseTab} offset={filtersHeight} />

        {failure !== null && (
          <ErrorState title="Could not load the analytics for these filters" body={failure} retry={retry} />
        )}
        {empty && <div className="empty">No traffic in this range.</div>}

        {tab === 'traffic' && (
          <Section id="traffic" shown={shown} title="Traffic" meta={sourced(loading ? undefined : `${fmtInt(totals.requests)} requests, ${pctOrDash(errorRateOf(totals))} errors`)}>
            <div className="grid-2 traffic-grid">
              <ChartCard title="Requests by outcome" subtitle={noted('Per bucket, stacked', partial)} loading={loading} legend={OUTCOME_SERIES.map((s) => ({ color: ink.series[s.slot], label: s.label, value: fmtInt(totals[s.key]) }))}>
                <BarChart data={buckets} margin={CHART_MARGIN} barCategoryGap={BAR_GAP}>
                  <CartesianGrid vertical={false} stroke={ink.grid} />
                  <XAxis {...axis} />
                  <YAxis {...COUNT_AXIS} />
                  <Tooltip cursor={{ fill: ink.grid, opacity: 0.6 }} content={timeTip<Bucket>((p) => OUTCOME_SERIES.map((s) => ({ color: ink.series[s.slot], label: s.label, value: fmtInt(p[s.key]) })))} />
                  {stackedBars(OUTCOME_SERIES, (s) => s.key, ink)}
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
              <Card
                title={REFUSAL_CATEGORIES_TITLE}
                subtitle={sentences(['Answers the upstream refused in range, by model and category, each a request that succeeded', refusalCategories.current === null ? null : `${plural(refusalCategories.current.fallbacks, 'request')} presented a fallback token`, ignoredNote('refusals', inForce)])}
              >
                {refusalCategories.error !== null ? (
                  <ErrorState title="Could not load refusals by category" body={refusalCategories.error} retry={refusalCategories.reload} />
                ) : refusalCategories.current === null ? (
                  <Skeleton height={90} />
                ) : (
                  <CountBars color={ink.series[7]} empty="No answer was refused in this range." rows={refusalRows(refusalCategories.current, refusalInput, plainLinks)} />
                )}
              </Card>

              <ChartCard title="Failovers" subtitle={noted('Attempts that moved to another member', partial)} loading={loading} legend={[{ color: ink.series[3], label: 'Failovers', shape: 'line', value: fmtInt(totals.failovers) }]}>
                <LineChart data={buckets} margin={CHART_MARGIN}>
                  <CartesianGrid vertical={false} stroke={ink.grid} />
                  <XAxis {...axis} />
                  <YAxis {...COUNT_AXIS} />
                  <Tooltip cursor={{ stroke: ink.axis }} content={timeTip<Bucket>((p) => [{ color: ink.series[3], label: 'failovers', value: fmtInt(p.failovers) }])} />
                  <Line type="monotone" dataKey="failovers" stroke={ink.series[3]} strokeWidth={2} dot={false} isAnimationActive={false} />
                </LineChart>
              </ChartCard>

              <RewritesCard answer={rewrites.current?.answer ?? null} error={rewrites.error} axis={axis} partial={stampedPartial(rewrites.current, (answer) => answer.points)} inForce={inForce} />

              <SpeedCard points={speedPoints} error={speedError} loading={speedLoading} axis={axis} partial={sessionId === null ? stampedPartial(bySpeed.current, (points) => points) : partial} />

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
            <GuardrailsCard answer={guardrailCounts.current} error={guardrailCounts.error} inForce={inForce} />
            <BreakdownTable
              title={`Traffic by ${groupLabel.toLowerCase()}`}
              className="traffic-table"
              loading={loading}
              groupLabel={groupLabel}
              columns={analyticsKeysOf('traffic', unit)
                .slice(1)
                .map((k) =>
                  TRAFFIC_HEAD_TITLE[k.id] === undefined ? (
                    k.label
                  ) : (
                    <span key={k.id} title={TRAFFIC_HEAD_TITLE[k.id]}>
                      {k.label}
                    </span>
                  ),
                )}
              order={tableOrder}
              rows={groups.map((g) => ({
                key: g.key,
                label: g.label,
                // Search and Fetch count the server-tool calls the group's answers made, not tokens.
                cells: [fmtInt(g.requests), fmtInt(g.succeeded), fmtInt(g.rateLimited), fmtInt(g.failed), fmtInt(g.incomplete), fmtInt(g.clientCanceled), fmtInt(g.exhausted), fmtInt(g.rejected), fmtInt(g.refused), rateCell(errorRateOf(g)), fmtInt(g.failovers), fmtInt(g.tokens.webSearch), fmtInt(g.tokens.webFetch)],
                figures: { client_canceled: g.clientCanceled, error_rate: errorRateOf(g), exhausted: g.exhausted, failed: g.failed, failovers: g.failovers, fetch: g.tokens.webFetch, incomplete: g.incomplete, ok: g.succeeded, rate_limited: g.rateLimited, refused: g.refused, rejected: g.rejected, requests: g.requests, search: g.tokens.webSearch },
              }))}
            />
          </Section>
        )}

        {tab === 'burn' && (
          <Section id="burn" shown={shown} title="Burn" meta={sentences(['Tokens, spend and plan share over the range', ignoredNote('burn', inForce), includeProbes ? null : 'Probe rows are always counted here'])}>
            {/* Each plan card's figure is the burn read's own entry under the chosen tier. Where the lines are one
                account's, the plan cards wait on the tiers that pick their category and take the settings read's
                failure from `burnWait`. Spend per minute sizes nothing, and neither does Plan per hour under a dollar
                position, so each draws while the settings read is out. */}
            <BurnCharts
              account={burnAccount}
              answer={burn.current?.answer ?? null}
              accounts={burn.current?.accounts ?? null}
              costs={costs}
              error={burn.error}
              groupBy={pageBy}
              rangeEndMs={burn.data?.endMs}
              readAt={burn.data?.readAt}
              sized={sized}
              slotOf={slotOf}
              unit={unit}
              wait={burnWait}
              week={week}
            />
            <BudgetTable budgets={budgets.data} error={budgets.error} now={now} />
          </Section>
        )}

        {tab === 'latency' && (
          <Section id="latency" shown={shown} title="Latency" meta={sentences([sessionId === null ? 'Percentiles read from merged per-hour histograms' : SESSION_SOURCE, HOLD_LEFT_OUT])}>
            <div className="stack">
              {LATENCY_METRICS.map((m) => (
                <LatencyRow key={m.key} metric={m} series={buckets} totals={totals} loading={loading} axis={axis} />
              ))}
            </div>
            <BreakdownTable
              title={`Latency by ${groupLabel.toLowerCase()}`}
              loading={loading}
              groupLabel={groupLabel}
              columns={analyticsKeysOf('latency', unit).slice(1).map((k, i) => (i === 0 ? k.label : <PctLabel key={k.id} text={k.label} />))}
              order={tableOrder}
              rows={requestGroups.map((g) => ({
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
                figures: { duration_p99: g.percentiles.durationMs.p99, requests: g.requests, tps_p50: g.percentiles.outputTps.p50, ttfb_p50: g.percentiles.ttfbMs.p50, ttfb_p99: g.percentiles.ttfbMs.p99, ttft_p99: g.percentiles.ttftMs.p99 },
              }))}
            />
          </Section>
        )}

        {tab === 'cache' && (
          <Section id="cache" shown={shown} title="Cache" meta={sourced(loading ? undefined : `${fmtPct(hitRatioOf(totals) ?? 0)} hit ratio, ${fmtInt(totals.expiredBetweenTurns)} expired between turns, ${fmtPct(cachedShareOf(totals) ?? 0)} of input read from cache`)}>
            <div className="grid-2">
              <ChartCard title="Tokens" subtitle="Per bucket, stacked by kind" loading={loading} legend={TOKEN_SERIES.map((s) => ({ color: ink.series[s.slot], label: s.label, value: fmtCompact(totals.tokens[s.key]) }))}>
                <BarChart data={buckets} margin={CHART_MARGIN} barCategoryGap={BAR_GAP}>
                  <CartesianGrid vertical={false} stroke={ink.grid} />
                  <XAxis {...axis} />
                  <YAxis {...COUNT_AXIS} />
                  <Tooltip cursor={{ fill: ink.grid, opacity: 0.6 }} content={timeTip<Bucket>((p) => TOKEN_SERIES.map((s) => ({ color: ink.series[s.slot], label: s.label, value: fmtCompact(p.tokens[s.key]) })))} />
                  {stackedBars(TOKEN_SERIES, (s) => `tokens.${s.key}`, ink)}
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
                <LineChart data={buckets} margin={CHART_MARGIN}>
                  <CartesianGrid vertical={false} stroke={ink.grid} />
                  <XAxis {...axis} />
                  <YAxis {...PCT_AXIS} />
                  <Tooltip
                    cursor={{ stroke: ink.axis }}
                    content={timeTip<Bucket>((p) => [
                      { color: ink.series[0], label: 'cache hit ratio', value: pct(pct100(hitRatioOf(p))) },
                      { color: ink.series[2], label: 'cached share of input', value: pct(pct100(cachedShareOf(p))) },
                    ])}
                  />
                  <Line type="monotone" dataKey={(b: Bucket) => pct100(hitRatioOf(b))} stroke={ink.series[0]} strokeWidth={2} dot={false} connectNulls isAnimationActive={false} />
                  <Line type="monotone" dataKey={(b: Bucket) => pct100(cachedShareOf(b))} stroke={ink.series[2]} strokeWidth={2} dot={false} connectNulls isAnimationActive={false} />
                </LineChart>
              </ChartCard>

              <Card title="Misses by reason" subtitle={MISS_REASON_BLURB}>
                {loading ? <Skeleton height={190} /> : <ReasonBars link={linkInput} plainLinks={plainLinks} totals={totals} />}
              </Card>

              <Card title="Dropped thinking blocks" subtitle={DROPPED_THINKING_BLURB}>
                {loading ? <Skeleton height={190} /> : <DroppedThinkingBars link={linkInput} plainLinks={plainLinks} totals={totals} />}
              </Card>

              <ChartCard
                title="Write cost vs read cost"
                subtitle={
                  cacheEquivalent
                    ? `Share of the chosen week per bucket spent writing the cache and reading it back by the subscription requests${cacheSides.billed === undefined ? '' : `, the billed requests ${usdOrDash(costTotals.billed.cacheWrite)} writing and ${usdOrDash(costTotals.billed.cacheRead)} reading`}`
                    : `USD per bucket spent writing the cache and reading it back, ${UNIT_SPLIT[unit === 'native' ? 'usd' : dollars]}`
                }
                loading={cacheEquivalent ? figureLoading : costLoading}
                failure={cacheEquivalent ? figureFailure : costFailure}
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
                  <BarChart data={costBuckets} margin={CHART_MARGIN} {...PAIRED_BAR_GAPS}>
                    <CartesianGrid vertical={false} stroke={ink.grid} />
                    <XAxis {...axis} />
                    <YAxis tickFormatter={(v: number) => fmtShare(v * cacheScale)} tickLine={false} axisLine={false} width={60} />
                    <Tooltip
                      cursor={{ fill: ink.grid, opacity: 0.6 }}
                      content={timeTip<Bucket>((p) => [
                        { color: ink.series[3], label: 'write cost', value: equivalentOrDash(p.cacheEquivalent?.write, week, cacheScale) },
                        { color: ink.series[2], label: 'read cost', value: equivalentOrDash(p.cacheEquivalent?.read, week, cacheScale) },
                      ])}
                    />
                    <Bar dataKey="cacheEquivalent.write" fill={ink.series[3]} maxBarSize={12} {...TIME_BAR} />
                    <Bar dataKey="cacheEquivalent.read" fill={ink.series[2]} maxBarSize={12} {...TIME_BAR} />
                  </BarChart>
                ) : (
                  <BarChart data={costBuckets} margin={CHART_MARGIN} {...PAIRED_BAR_GAPS}>
                    <CartesianGrid vertical={false} stroke={ink.grid} />
                    <XAxis {...axis} />
                    <YAxis {...usdAxis(costBuckets.flatMap((b) => [b[cacheDollars].cacheWrite ?? 0, b[cacheDollars].cacheRead ?? 0]))} tickLine={false} axisLine={false} />
                    <Tooltip
                      cursor={{ fill: ink.grid, opacity: 0.6 }}
                      content={timeTip<Bucket>((p) => [
                        { color: ink.series[3], label: 'write cost', value: cacheSplit === null ? '-' : usdOrDash(p[cacheDollars].cacheWrite) },
                        { color: ink.series[2], label: 'read cost', value: cacheSplit === null ? '-' : usdOrDash(p[cacheDollars].cacheRead) },
                      ])}
                    />
                    {cacheSplit !== null && <Bar dataKey={`${cacheDollars}.cacheWrite`} fill={ink.series[3]} maxBarSize={12} {...TIME_BAR} />}
                    {cacheSplit !== null && <Bar dataKey={`${cacheDollars}.cacheRead`} fill={ink.series[2]} maxBarSize={12} {...TIME_BAR} />}
                  </BarChart>
                )}
              </ChartCard>
            </div>
            <MissReasonsByGroup groupBy={groupBy} groups={requestGroups} groupLabel={groupLabel} link={linkInput} loading={loading} order={tableOrder} plainLinks={plainLinks} />
            <DroppedThinkingByGroup groupBy={groupBy} groups={requestGroups} groupLabel={groupLabel} link={linkInput} loading={loading} order={tableOrder} plainLinks={plainLinks} />
            {sessionId === null && (
              <CacheByModel
                groups={byModel.current ?? []}
                funded={fundedModels}
                failure={weekFailure(modelError)}
                loading={byModel.current === null || (funded && byModelFunded.current === null) || weekWait}
                reading={reading}
                unit={unit}
              />
            )}
            {/* The route reads no session, so a session view has no placements of its own to show. */}
            {sessionId === null && (
              <PlacementTable error={placements.error} inForce={inForce} rows={placements.current} />
            )}
          </Section>
        )}

        {tab === 'cost' && (
          <Section id="cost" shown={shown} title="Cost" meta={sourced(figureLoading || figureFailure !== null || costHead === null ? undefined : costHead.meta)} metaTitle={costHead?.unpriced ? UNPRICED_TITLE : undefined}>
            <div className="tiles-row">
              <Tile label={costHead?.tile.label ?? 'Cost'} value={figureLoading || costHead === null ? null : figuresValue(costHead.tile.figures)} note={costHead?.tile.note ?? NATIVE_SUBSET} title={costHead?.unpriced ? UNPRICED_TITLE : unit === 'native' ? (tileHint ?? undefined) : undefined} failure={figureFailure} />
              {unit === 'native' ? (
                <>
                  <Tile label="Cost per request" value={figureLoading || nativeRatios === null ? null : figuresValue(nativeRatios.perRequest.figures)} note={nativeRatios === null ? null : nativeRatios.perRequest.note} failure={figureFailure} />
                  <Tile label="Cost per 1k output tokens" value={figureLoading || nativeRatios === null ? null : figuresValue(nativeRatios.perThousand.figures)} note={nativeRatios === null ? null : nativeRatios.perThousand.note} failure={figureFailure} />
                </>
              ) : (
                <>
                  <Tile label="Cost per request" value={costLoading ? null : fmtUsd(per(costTotals.costUsd, costTotals.requests, 1) ?? 0)} note={`${fmtInt(costTotals.requests)} requests`} failure={costFailure} />
                  <Tile label="Cost per 1k output tokens" value={costLoading ? null : fmtUsd(per(costTotals.costUsd, costTotals.tokens.output, 1000) ?? 0)} note={`${fmtCompact(costTotals.tokens.output)} output tokens`} failure={costFailure} />
                </>
              )}
              <Tile label="Fast mode spend" value={speedError !== null ? '-' : speedLoading ? null : fmtUsd(fastSpend)} note="usage credits, at the fast cells" />
            </div>
            <div className="grid-2">
              <CostByGroup points={costPoints} groupBy={groupBy} groupSlot={groupSlot} loading={figureLoading} failure={figureFailure} axis={axis} partial={funded ? stampedPartial(costSeries.current, (points) => points) : partial} reading={reading} unit={unit} />
              {/* Native draws a ring per funding, each in its own unit. */}
              <Card title="Cost split" subtitle={unit === 'native' ? NATIVE_SPLIT_NOTE : `By token kind, in dollars at the API price list, ${UNIT_SPLIT[dollars]}`}>
                {/* The request summary carries one total and no split, so a session view has nothing to draw here. */}
                {sessionId !== null ? (
                  <p className="muted">{SESSION_COST_NOTE}</p>
                ) : figureFailure !== null ? (
                  <ErrorState title={failureHead(figureFailure, 'cost split')} body={figureFailure.text} />
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
                failure={figureFailure}
                loading={figureLoading}
                groupLabel={groupLabel}
                columns={NATIVE_COLUMNS}
                order={tableOrder}
                rows={scale === null ? [] : nativeCostGroups.map((g) => nativeRowOf(g, week, scale, rowHint(g)))}
              />
            )}
            {sessionId === null && unit !== 'native' && (
              <BreakdownTable
                title={`Cost by ${groupLabel.toLowerCase()}`}
                subtitle={`Totals for the selected range and filters, ${costHead?.subset ?? ''}`}
                failure={costFailure}
                loading={costLoading}
                groupLabel={groupLabel}
                columns={analyticsKeysOf('cost', unit).slice(1).map((k) => k.label)}
                order={tableOrder}
                rows={dollarCostGroups.map((g) => {
                  // $0.00 would read as a cost of nothing, so a ratio with a zero count to divide by draws the dash.
                  const perRequest = per(g.costUsd, g.requests, 1);
                  const perThousand = per(g.costUsd, g.tokens.output, 1000);
                  return {
                    key: g.key,
                    label: g.label,
                    cells: [fmtInt(g.requests), fmtUsd(g.costUsd), fmtUsd(g.cost.input), usdOrDash(g.cost.cacheRead), usdOrDash(g.cost.cacheWrite), fmtUsd(g.cost.output), usdOrDash(perRequest), usdOrDash(perThousand)],
                    figures: { cache_read: g.cost.cacheRead, cache_write: g.cost.cacheWrite, cost: g.costUsd, input: g.cost.input, output: g.cost.output, per_1k_output: perThousand, per_request: perRequest, requests: g.requests },
                  };
                })}
              />
            )}
          </Section>
        )}

        {tab === 'cache-warming' && (
          <Section id="cache-warming" shown={shown} title="Cache warming" meta={sessionId === null ? 'Probe rows are always counted here, whatever the toggle above says' : SESSION_SOURCE_NOTE}>
            <div className="tiles-row">
              <Tile label="Probe cost" value={probeTile.value} note={probeTile.note} failure={figureFailure} />
              <Tile label="Saved by probes" value={savingTile.value} note={savingTile.note} failure={figureFailure} />
              <Tile label="Net saving" value={netTile.value} note={netTile.note} tone={netTile.critical ? 'critical' : undefined} failure={figureFailure} />
              <Tile label="Kept warm" value={loading ? null : keptWarm === null ? '-' : fmtInt(keptWarm.requests)} note={keptWarm === null ? UNAVAILABLE_NOTE : keptWarmNote(keptWarm)} />
            </div>
            <div className="grid-2 warming-grid">
              <ChartCard title="Probes by outcome" subtitle="Per bucket, stacked" failure={probeFailure} loading={probeLoading} legend={PROBE_SERIES.map((s) => ({ color: ink.series[s.slot], label: s.label, title: s.hint, value: probesUnmeasured ? '-' : fmtInt(probeTotals[s.key]) }))}>
                <BarChart data={probeBuckets} margin={CHART_MARGIN} barCategoryGap={BAR_GAP}>
                  <CartesianGrid vertical={false} stroke={ink.grid} />
                  <XAxis {...axis} />
                  <YAxis {...COUNT_AXIS} />
                  <Tooltip cursor={{ fill: ink.grid, opacity: 0.6 }} content={timeTip<Bucket>((p) => PROBE_SERIES.map((s) => ({ color: ink.series[s.slot], label: s.label, title: s.hint, value: probesUnmeasured ? '-' : fmtInt(p.probes[s.key]) })))} />
                  {!probesUnmeasured && stackedBars(PROBE_SERIES, (s) => `probes.${s.key}`, ink)}
                </BarChart>
              </ChartCard>

              <Card title="Wasted probes" subtitle={`${WASTED_PROBE_BLURB} ${WASTED_PROBE_UNIT[unit]}`}>
                {figureFailure !== null ? (
                  <ErrorState title={failureHead(figureFailure, 'wasted probes')} body={figureFailure.text} />
                ) : figureLoading ? (
                  <Skeleton height={190} />
                ) : probesUnmeasured ? (
                  <p className="muted">{WARMING_UNAVAILABLE}</p>
                ) : (
                  <WastedProbes group={costTotals} probes={upstreamProbes(costTotals)} unit={unit} billed={billedTotals} reading={reading} />
                )}
              </Card>

              <SavingChart
                axis={axis}
                billed={billedWarmingPoints}
                buckets={sessionId === null ? warmingBuckets : buckets}
                failure={weekFailure(warmingError)}
                loading={(sessionId === null ? warmingLoading : loading) || weekWait}
                points={sessionId === null ? warmingPoints : points}
                reading={reading}
                session={sessionId !== null}
                unit={unit}
              />

              <ChartCard title="Kept-warm requests" subtitle="Requests whose cache read followed a probe" loading={loading} legend={[{ color: ink.series[0], label: 'Kept-warm requests', value: keptWarm === null ? '-' : fmtInt(keptWarm.requests) }]}>
                <BarChart data={buckets} margin={CHART_MARGIN} barCategoryGap={BAR_GAP}>
                  <CartesianGrid vertical={false} stroke={ink.grid} />
                  <XAxis {...axis} />
                  <YAxis {...COUNT_AXIS} />
                  <Tooltip cursor={{ fill: ink.grid, opacity: 0.6 }} content={timeTip<Bucket>((p) => [{ color: ink.series[0], label: 'kept-warm requests', value: keptWarm === null || p.keptWarm === null ? '-' : fmtInt(p.keptWarm.requests) }])} />
                  {keptWarm !== null && <Bar dataKey="keptWarm.requests" fill={ink.series[0]} maxBarSize={22} {...TIME_BAR} />}
                </BarChart>
              </ChartCard>

              <ChartCard title="Kept-warm tokens" subtitle="Cache-read tokens on those requests" loading={loading} legend={[{ color: ink.series[2], label: 'Kept-warm tokens', value: keptWarm === null ? '-' : fmtCompact(keptWarmTokens(keptWarm)) }]}>
                <BarChart data={buckets} margin={CHART_MARGIN} barCategoryGap={BAR_GAP}>
                  <CartesianGrid vertical={false} stroke={ink.grid} />
                  <XAxis {...axis} />
                  <YAxis {...COUNT_AXIS} />
                  <Tooltip cursor={{ fill: ink.grid, opacity: 0.6 }} content={timeTip<Bucket>((p) => [{ color: ink.series[2], label: 'kept-warm tokens', value: keptWarm === null || p.keptWarm === null ? '-' : fmtCompact(keptWarmTokens(p.keptWarm)) }])} />
                  {keptWarm !== null && <Bar dataKey={(p: Bucket) => (p.keptWarm === null ? null : keptWarmTokens(p.keptWarm))} fill={ink.series[2]} maxBarSize={22} {...TIME_BAR} />}
                </BarChart>
              </ChartCard>
            </div>
            <KeepaliveByPool
              groups={poolGroups}
              funded={fundedPools}
              failure={weekFailure(poolError)}
              loading={(sessionId === null ? byPool.current === null || (poolFunding !== undefined && fundedByPool.current === null) : loading) || weekWait}
              reading={reading}
              unit={unit}
            />
          </Section>
        )}

        {tab === 'accounts' && (
          <Section id="accounts" shown={shown} title="Accounts" meta="Peak utilization, window usage, pace, ineligible time, and pin churn per account">
            <AccountsSection
              accountIds={filters.accountId}
              allowance={{ answer: unusedAllowance.current, costs: costSettings.data?.subscriptionCosts ?? null, error: unusedAllowance.error ?? costSettings.error }}
              bucketMs={bucketMs}
              error={accountRollups.error}
              inForce={inForce}
              registry={registry.data}
              rollups={accountRollups.current}
              slotOf={slotOf}
              span={span}
              axis={axis}
              week={week}
            />
          </Section>
        )}
      </div>
    </PopoverHost.Provider>
  );
}

const pct100 = (v: number | null) => (v === null ? null : v * 100);

/** Live border-box height of a sticky bar, so the bar below it can dock against it. */
function useStickyHeight(ref: RefObject<Element>): number {
  const [height, setHeight] = useState(0);
  useEffect(() => {
    if (ref.current) setHeight(ref.current.getBoundingClientRect().height);
  }, [ref]);
  const read = () => {
    if (ref.current) setHeight(ref.current.getBoundingClientRect().height);
  };
  useResizeObserver(ref, read);
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
const TABLES_SPLIT = "Splits this tab's tables. Its charts draw the total";
/**
 * The group-by's title per tab: what a grouping splits there, or, where no figure of the tab reads the grouping, the
 * reason the control stands disabled.
 */
const GROUP_BY_TITLES: Record<SectionId, { disabledTitle: string } | { title: string }> = {
  accounts: { disabledTitle: 'The Accounts tab draws one line and one row per account' },
  burn: { title: "Splits the Burn cards' lines" },
  cache: { title: TABLES_SPLIT },
  'cache-warming': { disabledTitle: 'The Cache warming tab draws the total and one row per pool' },
  cost: { title: "Splits this tab's tables and the Cost by chart" },
  latency: { title: TABLES_SPLIT },
  traffic: { title: TABLES_SPLIT },
};

/** `metaTitle` is the meta line's hover text, such as the estimate's sentence for a marked figure. */
function Section({ id, title, meta, metaTitle, shown = true, children }: { children: ReactNode; id: SectionId; meta?: string; metaTitle?: string; shown?: boolean; title: string }) {
  if (!shown) return null;
  return (
    <section className="analytics-section" {...sectionPanel(id)}>
      <header className="analytics-section__head">
        <h2 className="analytics-section__title">{title}</h2>
        {meta && (
          <span className="analytics-section__meta" title={metaTitle}>
            {meta}
          </span>
        )}
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

const keptWarmRequests = (g: AnalyticsGroup | null) => `${fmtInt(g?.keptWarm?.requests ?? 0)} requests kept warm`;

/**
 * A cache-warming tile per position, the Probe cost, Saved by probes and Net saving tiles apart in their side pair,
 * their dollars and their note's lead. Under native the plan side's figure in the chosen week stacks over the billed
 * dollars, the skeleton while the scale is out, and a dollar position reads its dollars. A session view folds request
 * rows, which measure no probe count and carry no credit, so it reads the figure unmeasured on every path. The tile
 * sums every account it reads, so it carries no hint. `critical` holds while the figure is under zero: under native
 * the plan side's, the widget's subject, since the billed dollars beside it never tone the tile.
 */
function warmingTileOf(unit: UnitPosition, totals: AnalyticsGroup, loading: boolean, session: boolean, reading: WeekReading, lead: string, sides: ReturnType<typeof probeSidesOf>, usd: number): { critical: boolean; note: string; value: ReactNode | null } {
  if (session || totals.fromRequests === true) return { critical: false, note: UNAVAILABLE_NOTE, value: loading ? null : '-' };
  if (unit === 'native') {
    const scale = loading ? null : reading.scale;
    return { critical: scale !== null && typeof sides.equivalent === 'number' && sides.equivalent < 0, note: `${lead}, ${NATIVE_SUBSET}`, value: scale === null ? null : figuresValue(nativeFiguresOf(sides, reading.week, scale).figures) };
  }
  return { critical: !loading && usd < 0, note: `${lead}, ${UNIT_SPLIT[dollarsOf(unit)]}`, value: loading ? null : fmtUsd(usd) };
}

function ChartCard({
  title,
  subtitle,
  loading,
  children,
  empty,
  failure = null,
  footer,
  legend,
  actions,
}: {
  /** Drawn in the card's header beside the title: a control the chart reads. */
  actions?: ReactNode;
  children: ReactNode;
  /** Drawn in place of the chart, legend and all, when the answer holds nothing the chart could draw. */
  empty?: ReactNode;
  /** A card reading a route of its own draws that route's failure in place of the chart, headed unpriced where it is
   *  the week reading's unpriced sentence, which no retry clears. */
  failure?: WeekReading['failure'];
  footer?: ReactNode;
  legend?: { color: string; dashed?: boolean; hollow?: boolean; label: string; shape?: 'line' | 'rect'; title?: string; value?: ReactNode }[];
  loading: boolean;
  subtitle?: ReactNode;
  title: string;
}) {
  return (
    <Card title={title} subtitle={subtitle} actions={actions}>
      {failure !== null ? (
        <ErrorState title={failureHead(failure, title.toLowerCase())} body={failure.text} />
      ) : loading ? (
        <Skeleton height={CHART_HEIGHT} />
      ) : empty ? (
        <p className="muted">{empty}</p>
      ) : (
        <>
          <ResponsiveContainer width="100%" height={CHART_HEIGHT}>
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

/** A group table's order: by the label under `name`, else by the figure the rows hold under that sort key. */
type TableOrder = { dir: SortDir; key: string };

function BreakdownTable({
  title,
  groupLabel,
  columns,
  rows,
  className = '',
  failure = null,
  loading = false,
  order,
  subtitle = 'Totals for the selected range and filters',
}: {
  className?: string;
  columns: ReactNode[];
  /** Headed unpriced where it is the week reading's unpriced sentence, which no retry clears. */
  failure?: WeekReading['failure'];
  groupLabel: string;
  loading?: boolean;
  /** The rows' order through `orderRows`, a row with no figure in the column last. A table over a fixed dimension
   *  passes none and keeps its rows' order. */
  order?: TableOrder;
  /** A row's `title` is its label cell's hover text, the label itself where none is given. `figures` holds a figure per
   *  sort key id for `order`, null where the cell has none to order by. */
  rows: { cells: ReactNode[]; figures?: Partial<Record<string, SortFigure>>; key: string; label: string; title?: string }[];
  subtitle?: string;
  title: string;
}) {
  const ordered = order === undefined ? rows : orderRows(rows, (r) => (order.key === 'name' ? r.label : (r.figures?.[order.key] ?? null)), order.dir, (r) => r.label);
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
            {failure === null &&
              ordered.map((r) => (
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
            {(failure !== null || rows.length === 0) && (
              <StateRow
                colSpan={columns.length + 1}
                empty="No rows."
                error={failure?.text ?? null}
                // The row draws the title with `error` alone, which is null while `failure` is, so the empty title
                // draws nowhere.
                errorTitle={failure === null ? '' : failureHead(failure, title.toLowerCase())}
                loading={loading}
              />
            )}
          </tbody>
        </table>
      </div>
    </Card>
  );
}

/**
 * Horizontal bars of counts in the order given, each with its share of the total and, where its row carries a target,
 * its count linking the requests it counts, and `empty` when every count is zero. A row whose label another row shares
 * carries a `key` of its own.
 */
function CountBars({ color, empty, rows }: { color: string; empty: string; rows: { count: number; key?: string; label: string; target?: RequestsLinkTarget; title?: string }[] }) {
  const max = Math.max(1, ...rows.map((r) => r.count));
  const total = rows.reduce((s, r) => s + r.count, 0);
  if (total === 0) return <p className="muted">{empty}</p>;
  return (
    <ul className="plain-list hbars hbars--errors">
      {rows.map((r) => (
        <HBarRow
          key={r.key ?? r.label}
          color={color}
          count={r.target ? <CountLink target={r.target}>{fmtInt(r.count)}</CountLink> : fmtInt(r.count)}
          fill={r.count / max}
          label={r.label}
          labelClass="mono"
          share={fmtPct(r.count / total)}
          title={r.title}
        />
      ))}
    </ul>
  );
}

/**
 * A bar list's row, `fill` the bar's share of its track. `count` is drawn as given, so a list can link it, and `title`
 * is the label's hover, for a label that shortens what it names. A `second` part takes its `portion` of the bar after
 * the first, in its own ink and with its own hover.
 */
function HBarRow({ color, count, fill, label, labelClass, rowClass, second, share, title }: { color: string; count: ReactNode; fill: number; label: string; labelClass?: string; rowClass?: string; second?: { color: string; portion: number; title: string }; share: string; title?: string }) {
  return (
    <li className={rowClass}>
      <span className={labelClass} title={title}>{label}</span>
      <div className="hbars__track">
        {second === undefined ? (
          <span className="hbars__fill" style={{ width: `${fill * 100}%`, background: color }} />
        ) : (
          // A part with nothing in it draws no segment, since a segment keeps a sliver's width.
          <span className="hbars__stack" style={{ width: `${fill * 100}%` }}>
            {second.portion < 1 && <span className="hbars__seg" style={{ background: color, flexGrow: 1 - second.portion }} />}
            {second.portion > 0 && <span className="hbars__seg" title={second.title} style={{ background: second.color, flexGrow: second.portion }} />}
          </span>
        )}
      </div>
      <span className="num">
        {count} <span className="muted">{share}</span>
      </span>
    </li>
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

/** One bar per model and category in the route's order, `-` read `none`, the model short on the label and whole on its
 *  hover, each count linking its model's and category's refused answers. */
const refusalRows = (answer: RefusalCategoriesAnswer, input: ReasonLink, plainLinks: boolean) =>
  answer.rows.map((r) => ({ count: r.requests, key: `${r.model}:${r.category}`, label: `${shortModel(r.model)} · ${r.category === '-' ? 'none' : r.category}`, target: plainLinks ? SEVERAL_CLIENTS : refusalLink(input, r.category, r.requests, r.model), title: r.model }));

/** Requests per effort level, each named as the rollups file it; `none`, the requests that asked for no level, last. */
function EffortBars({ groups, color }: { color: string; groups: AnalyticsGroup[] }) {
  const rows = groups
    .filter(countsRequest)
    .map((g) => ({ count: g.requests, label: g.key }))
    .sort((a, b) => Number(a.label === 'none') - Number(b.label === 'none') || b.count - a.count || a.label.localeCompare(b.label));
  return <CountBars color={color} empty="No requests in range." rows={rows} />;
}

const MISS_REASON_BLURB = `From the cache diagnostics beta: the reason each answered request drew. A gray part counts the requests under a reason that read ${REASON_HIT_PERCENT}% or more of their prompt from the cache. Not requested sent no diagnostics member, and No reason returned sent one and drew no reason`;

/** The hover of a fault reason's hit part, on its bar and beside its count in the table. */
const reasonHitTitle = (hits: number) => `${fmtInt(hits)} of these requests read ${REASON_HIT_PERCENT}% or more of their prompt from the cache`;

/**
 * Every reason but no divergence and the relay's own words reads as a fault, like the error bars above: a diagnosed
 * divergence, a missing previous message, an unavailable comparison and a name the console has no label for. The
 * relay's words (a request not asked, an answer that returned no reason and a comparison still running) name no
 * finished comparison. A fault's bar and cell draw apart the requests under it that still read their prompt from the
 * cache.
 */
const isFaultReason = (reason: CacheMissReason): boolean => !relayReason(reason);
/** A group counts its hits under every reason, and a bar or cell draws apart the hits under a fault alone. */
const faultHits = (group: Pick<AnalyticsGroup, 'cacheMissReasonHits'>, reason: CacheMissReason): number => (isFaultReason(reason) ? countOf(group.cacheMissReasonHits, reason) : 0);

function reasonColor(ink: ReturnType<typeof useChartInk>, reason: CacheMissReason): string {
  if (reason === 'none') return ink.series[2];
  return isFaultReason(reason) ? ink.series[7] : ink.muted;
}

/**
 * What every count of the reasons panel, the dropped thinking blocks card and the refusals card links under: the window
 * and the page's filters, without a count, a reason, a key of the dropped blocks or a category.
 */
type ReasonLink = Omit<RequestsLinkInput, 'count' | 'group' | 'narrow'>;

/**
 * A count that links into the Requests view: the link to the requests it counts, else the figure with why it stays
 * plain.
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

const requestShare = (count: number, totals: Pick<AnalyticsGroup, 'requests'>): string => fmtPct(totals.requests ? count / totals.requests : 0, 1);

/** A count of the reasons panel or the dropped thinking blocks card, linking its requests under `narrow`, and in its
 *  group where a table's row holds it. */
function LinkedCount({ count, group, link, narrow, plainLinks }: { count: number; group?: RequestsLinkInput['group']; link: ReasonLink; narrow: NonNullable<RequestsLinkInput['narrow']>; plainLinks: boolean }) {
  return <CountLink target={plainLinks ? SEVERAL_CLIENTS : requestsLink({ ...link, count, group, narrow })}>{fmtInt(count)}</CountLink>;
}

function ReasonBars({ link, plainLinks, totals }: { link: ReasonLink; plainLinks: boolean; totals: AnalyticsGroup }) {
  const ink = useChartInk();
  const rows = missReasonOrder([totals])
    .map((reason) => ({ count: countOf(totals.cacheMissReasons, reason), hits: faultHits(totals, reason), reason }))
    .sort((a, b) => b.count - a.count);
  const max = Math.max(1, ...rows.map((r) => r.count));
  if (rows.every((r) => r.count === 0)) return <p className="muted">No diagnostics in range.</p>;
  return (
    <ul className="plain-list hbars hbars--reasons">
      {rows.map((r) => (
        <HBarRow
          key={r.reason}
          color={reasonColor(ink, r.reason)}
          count={<LinkedCount count={r.count} link={link} narrow={{ cacheMissReasons: [r.reason] }} plainLinks={plainLinks} />}
          fill={r.count / max}
          label={missLabel(r.reason)}
          second={r.hits > 0 ? { color: ink.muted, portion: r.hits / r.count, title: reasonHitTitle(r.hits) } : undefined}
          share={requestShare(r.count, totals)}
        />
      ))}
    </ul>
  );
}

const DROPPED_THINKING_BLURB = 'From the thinking-binding beta: requests whose answer listed a thinking block it dropped or let pass, by type and reason. A bar counts requests, never blocks';

/**
 * One bar per key of the blocks the answers dropped, largest first, a key under an account reason in the fault ink,
 * then the moved requests and the requests with nothing dropped as plain lines, since the Requests filter lists keys
 * alone.
 */
function DroppedThinkingBars({ link, plainLinks, totals }: { link: ReasonLink; plainLinks: boolean; totals: AnalyticsGroup }) {
  const ink = useChartInk();
  // The sort is stable, so a tie keeps the order the console lists the keys in.
  const rows = transformationOrder(Object.keys(totals.inputTransformations))
    .map((key) => ({ count: countOf(totals.inputTransformations, key), key }))
    .sort((a, b) => b.count - a.count);
  const moved = totals.inputTransformationsMoved;
  const none = totals.inputTransformationsNone;
  if (rows.length === 0 && moved === 0 && none === 0) return <p className="muted">No answer reported its thinking blocks in range.</p>;
  const max = Math.max(1, ...rows.map((r) => r.count));
  return (
    <div className="stack">
      {rows.length > 0 && (
        <ul className="plain-list hbars hbars--reasons">
          {rows.map((r) => (
            <HBarRow
              key={r.key}
              color={accountBound(r.key) ? ink.series[7] : ink.muted}
              count={<LinkedCount count={r.count} link={link} narrow={{ inputTransformations: [r.key] }} plainLinks={plainLinks} />}
              fill={r.count / max}
              label={transformationLabel(r.key)}
              share={requestShare(r.count, totals)}
            />
          ))}
        </ul>
      )}
      <ul className="plain-list">
        <li>Moved to another account: {fmtInt(moved)}</li>
        <li>
          Nothing dropped: {fmtInt(none)} <span className="muted">{requestShare(none, totals)}</span>
        </li>
      </ul>
    </div>
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
    const of = nativeFiguresOf(cost, week, weekFactor).figures.map((f) => f.text);
    return {
      cost: { note: NATIVE_SUBSET, value: figuresValue(nativeFiguresOf(wasted, week, weekFactor).figures) },
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
        <Tile label="Wasted probes" value={fmtInt(group.wastedProbes)} note={`of ${fmtInt(probes)} probes sent`} />
        <Tile label="Wasted cost" value={tiles.cost.value} note={tiles.cost.note} />
        <Tile label="Share of probe cost" value={tiles.share.value} note={tiles.share.note} />
      </div>
      <ul className="plain-list hbars hbars--reasons hbars--under-tiles">
        {rows.map((r) => (
          // The longest reason wraps in this column, so the row anchors the bar and the count to its first line.
          <HBarRow
            key={r.reason}
            color={r.reason === 'idle_limit' ? ink.series[3] : ink.muted}
            count={fmtInt(r.count)}
            fill={r.count / max}
            label={WASTED_PROBE_LABEL[r.reason]}
            rowClass="hbars__row--top"
            share={fmtPct(group.wastedProbes ? r.count / group.wastedProbes : 0, 1)}
          />
        ))}
      </ul>
    </>
  );
}

/** A figure as the chart prints it: a side with no row reads its zero, and a side priced nothing a dash. */
const chartEquivalent = (s: ReturnType<typeof probeSidesOf>, week: WeekPosition, weekFactor: number): string => equivalentOrDash(s.equivalent === undefined ? 0 : s.equivalent, week, weekFactor);

/**
 * The Saved by probes chart: two bars per bucket, the gross saving and the probe cost, each bucket's net in the
 * tooltip, and the range's gross, cost and net in the legend, the net in the critical ink under zero. `points` is the
 * series that counts probes under the position's funding, `buckets` its fold, and `billed` native's billed twin. The
 * dollar positions draw dollars. Native draws the plan side's Pro-week figures, read in the chosen week, with the
 * billed dollars beside them in the tooltip, as the Probe cost tile pairs them, and the billed dollars alone where the
 * range holds no plan side, as the cache cost card does. A session view's fold carries no credit, so `session` draws
 * no bar and dashes the legend.
 */
function SavingChart({ axis, billed, buckets, failure, loading, points, reading, session, unit }: { axis: TimeAxis; billed: AnalyticsPoint[] | null; buckets: Bucket[]; failure: WeekReading['failure']; loading: boolean; points: AnalyticsPoint[]; reading: WeekReading; session: boolean; unit: UnitPosition }) {
  const ink = useChartInk();
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
  if (equivalents && weekFactor === null) return <ChartCard title={title} subtitle={subtitle} loading failure={failure}>{null}</ChartCard>;
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
    return rows([sidesLine(savingSidesOf(p, paid, 'gross'), week, factor), sidesLine(probeSidesOf(p, paid, 'cost'), week, factor), sidesLine(savingSidesOf(p, paid, 'net'), week, factor)]);
  };
  return (
    <ChartCard
      title={title}
      subtitle={subtitle}
      loading={loading}
      failure={failure}
      legend={[
        { color: ink.series[1], label: 'Saved by probes', value: figures.gross },
        { color: ink.series[3], label: 'Probe cost', value: figures.cost },
        { color: ink.muted, hollow: true, label: 'Net saving', value: negative ? <span className="is-negative">{figures.net}</span> : figures.net },
      ]}
    >
      <BarChart data={drawn} margin={CHART_MARGIN} {...PAIRED_BAR_GAPS}>
        <CartesianGrid vertical={false} stroke={ink.grid} />
        <XAxis {...axis} />
        {equivalents ? <YAxis tickFormatter={(v: number) => fmtShare(v * factor)} tickLine={false} axisLine={false} width={60} /> : <YAxis {...usdAxis(drawn.flatMap((b) => [b.savingUsd, b.probeCostUsd]))} tickLine={false} axisLine={false} />}
        <Tooltip cursor={{ fill: ink.grid, opacity: 0.6 }} content={timeTip<Bucket>(tip)} />
        {!session && <Bar dataKey={gross} fill={ink.series[1]} maxBarSize={22} {...TIME_BAR} />}
        {!session && <Bar dataKey={cost} fill={ink.series[3]} maxBarSize={22} {...TIME_BAR} />}
      </BarChart>
    </ChartCard>
  );
}

/** A raw reason name with a break opportunity after each underscore, so a header holding one wraps at its words. */
const breakable = (name: string): ReactNode => name.split(/(?<=_)/).flatMap((part, i) => (i === 0 ? [part] : [<wbr key={i} />, part]));

function MissReasonsByGroup({ groupBy, groups, groupLabel, link, loading, order, plainLinks }: { groupBy: GroupBy; groupLabel: string; groups: AnalyticsGroup[]; link: ReasonLink; loading: boolean; order: TableOrder; plainLinks: boolean }) {
  const reasons = missReasonOrder(groups);
  const cell = (g: AnalyticsGroup, reason: CacheMissReason) => {
    const count = countOf(g.cacheMissReasons, reason);
    const hits = faultHits(g, reason);
    const counted = <LinkedCount count={count} group={{ by: groupBy, key: g.key }} link={link} narrow={{ cacheMissReasons: [reason] }} plainLinks={plainLinks} />;
    if (hits === 0) return counted;
    return (
      <>
        {counted}{' '}
        <span className="muted" title={reasonHitTitle(hits)}>
          {fmtInt(hits)} hit
        </span>
      </>
    );
  };
  return (
    <BreakdownTable
      title={`Misses by reason by ${groupLabel.toLowerCase()}`}
      className="misses-table"
      loading={loading}
      groupLabel={groupLabel}
      columns={[
        ...reasons.map((r) => (
          <span key={r} title={missLabel(r)}>
            {breakable(missShort(r))}
          </span>
        )),
        'Expired between turns',
      ]}
      order={order}
      rows={groups.map((g) => ({
        key: g.key,
        label: g.label,
        // Expired between turns is no reason the Requests page filters by, so it stays plain.
        cells: [...reasons.map((reason) => cell(g, reason)), fmtInt(g.expiredBetweenTurns)],
        // The known reasons alone, since a reason the console does not know has no sort key.
        figures: { ...Object.fromEntries(CACHE_MISS_REASONS.map((reason) => [reason, countOf(g.cacheMissReasons, reason)])), expired_between_turns: g.expiredBetweenTurns },
      }))}
    />
  );
}

/**
 * A column per key the groups hold, each count linking its key's requests in its group, then Moved and Nothing dropped,
 * which no Requests filter lists. The keys are an open set and take no sort key, so a row carries no figure and the
 * group's label orders the rows.
 */
function DroppedThinkingByGroup({ groupBy, groups, groupLabel, link, loading, order, plainLinks }: { groupBy: GroupBy; groupLabel: string; groups: AnalyticsGroup[]; link: ReasonLink; loading: boolean; order: TableOrder; plainLinks: boolean }) {
  const keys = transformationOrder(groups.flatMap((g) => Object.keys(g.inputTransformations)));
  const cell = (g: AnalyticsGroup, key: string) => <LinkedCount count={countOf(g.inputTransformations, key)} group={{ by: groupBy, key: g.key }} link={link} narrow={{ inputTransformations: [key] }} plainLinks={plainLinks} />;
  return (
    <BreakdownTable
      title={`Dropped thinking blocks by ${groupLabel.toLowerCase()}`}
      loading={loading}
      groupLabel={groupLabel}
      columns={[
        ...keys.map((key) => (
          <span key={key} title={key}>
            {transformationLabel(key)}
          </span>
        )),
        'Moved',
        'Nothing dropped',
      ]}
      order={order}
      rows={groups.map((g) => ({
        key: g.key,
        label: g.label,
        cells: [...keys.map((key) => cell(g, key)), fmtInt(g.inputTransformationsMoved), fmtInt(g.inputTransformationsNone)],
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
        <LineChart data={data} margin={CHART_MARGIN}>
          <CartesianGrid vertical={false} stroke={ink.grid} />
          <XAxis {...axis} />
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
function CacheByModel({ failure, funded, groups, loading, reading, unit }: { failure: WeekReading['failure']; funded: Map<string, AnalyticsGroup>; groups: AnalyticsGroup[]; loading: boolean; reading: WeekReading; unit: UnitPosition }) {
  // A request refused before it resolved a model reads no cache, so the group without a model has no row here.
  const rows = useMemo(() => groups.filter((g) => g.key !== '' && countsRequest(g)).sort((a, b) => b.tokens.cacheRead - a.tokens.cacheRead), [groups]);
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
      failure={failure}
      loading={loading}
      groupLabel="Model"
      columns={['Reads', 'Writes 5m', 'Writes 1h', 'Hit ratio', 'Cached share', 'Read cost', 'Write cost']}
      rows={
        factor === null
          ? []
          : rows.map((g) => ({
              key: g.key,
              label: g.label,
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
function KeepaliveByPool({ failure, funded, groups, loading, reading, unit }: { failure: WeekReading['failure']; funded: Map<string, AnalyticsGroup>; groups: AnalyticsGroup[]; loading: boolean; reading: WeekReading; unit: UnitPosition }) {
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
      failure={failure}
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
 * unless `center` prints it shorter, and `label` names the ring at its center. `figureTitle` is a kind's figure's own
 * hover text.
 */
function CostDonut({ center, cost, figureTitle, format = fmtUsd, label = 'cost' }: { center?: (v: number) => string; cost: CostSplit; figureTitle?: (v: number) => string; format?: (v: number) => string; label?: string }) {
  const ink = useChartInk();
  const [hover, setHover] = useState<string | null>(null);
  const total = COST_PARTS.reduce((s, p) => s + cost[p.key], 0);
  return (
    <div className="split-row">
      <Donut slices={COST_PARTS.map((p) => ({ id: p.key, label: p.label, value: cost[p.key], color: ink.series[p.slot] }))} total={total} centerLabel={label} centerValue={(center ?? format)(total)} onHover={setHover} />
      <DonutLegend
        hover={hover}
        items={COST_PARTS.map((p) => ({
          color: ink.series[p.slot],
          id: p.key,
          label: <span>{p.label}</span>,
          value: (
            <>
              {format(cost[p.key])} <span className="muted">{fmtPct(total ? cost[p.key] / total : 0)}</span>
            </>
          ),
          valueTitle: figureTitle?.(cost[p.key]),
        }))}
      />
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
 * its gaps. A group that counts no request in any bucket and totals zero leaves whatever `drawn` says, as the group
 * tables leave it out. `unpriced` names the drawn groups that `flagged` reads unpriced in a bucket, a group folded into
 * Other among them, by their labels in the chart's order, and counts the buckets holding one.
 */
function stackOf(points: AnalyticsPoint[], value: (g: AnalyticsGroup) => number | null, drawn = false, flagged: (g: AnalyticsGroup) => boolean = () => false): Stack {
  const totals = new Map<string, number>();
  const unpriced = new Set<string>();
  const counted = new Set<string>();
  const labels = new Map<string, string>();
  for (const p of points) {
    for (const g of p.groups) {
      const v = value(g);
      totals.set(g.key, (totals.get(g.key) ?? 0) + (v ?? 0));
      if (v === null) unpriced.add(g.key);
      if (countsRequest(g)) counted.add(g.key);
      labels.set(g.key, g.label);
    }
  }
  const ranked = [...totals.entries()]
    .filter(([k, total]) => (counted.has(k) || total !== 0) && (!drawn || total > 0 || unpriced.has(k)))
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
function CostByGroup({ axis, failure, points, groupBy, groupSlot, loading, partial, reading, unit }: { axis: TimeAxis; failure: WeekReading['failure']; groupBy: GroupBy; groupSlot: (key: string) => string; loading: boolean; partial: PartialEnds; points: AnalyticsPoint[]; reading: WeekReading; unit: UnitPosition }) {
  const ink = useChartInk();
  const stack = useMemo(() => (unit === 'native' ? stackOf(points, (g) => g.costEquivalent, true, (g) => g.costEquivalent === null) : stackOf(points, (g) => g.costUsd)), [points, unit]);
  const billed = useMemo(() => (unit === 'native' ? stackOf(points, (g) => g.billedUsd, true) : null), [points, unit]);
  const { keys, labels, data } = stack;
  const color = (k: string) => (k === 'Other' ? ink.muted : groupSlot(k));
  const label = (k: string) => labels.get(k) ?? k;
  const title = `Cost by ${GROUP_LABEL[groupBy].toLowerCase()}`;
  const areas = (s: Stack) => s.keys.map((k) => <Area key={k} type="monotone" dataKey={k} stackId="a" stroke={color(k)} strokeWidth={1.5} fill={color(k)} fillOpacity={0.35} dot={false} isAnimationActive={false} />);
  const dollarCard = (s: Stack, cardTitle: string, subtitle: string) => (
    <ChartCard title={cardTitle} subtitle={subtitle} loading={loading} failure={failure} legend={s.keys.map((k) => ({ color: color(k), label: s.labels.get(k) ?? k }))}>
      <AreaChart data={s.data} margin={CHART_MARGIN}>
        <CartesianGrid vertical={false} stroke={ink.grid} />
        <XAxis {...axis} />
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
    const planDrawn = loading || failure !== null || keys.length > 0 || !billedDrawn;
    const subtitle = noted('Share of the chosen week per bucket, stacked, the subscription requests', partial);
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
            failure={failure}
            legend={keys.map((k) => ({ color: color(k), label: label(k) }))}
          >
            {weekFactor !== null && (
              <AreaChart data={data} margin={CHART_MARGIN}>
                <CartesianGrid vertical={false} stroke={ink.grid} />
                <XAxis {...axis} />
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
        {billedDrawn && dollarCard(billed, `Billed by ${GROUP_LABEL[groupBy].toLowerCase()}`, noted('USD per bucket, stacked, the billed requests', partial))}
      </>
    );
  }
  return dollarCard(stack, title, noted(`USD per bucket, stacked, ${UNIT_SUBSET[dollarsOf(unit)]}`, partial));
}

const FIRST_PARTIAL_NOTE = "The first bucket is partial: it holds the traffic from the window's start";
const SUMMED_PARTIAL_NOTE = "The last bucket is partial: it holds the traffic up to the range's end";
/** A bucket chart's subtitle, with a note for each end bucket that is partial, the first ahead of the last as the chart
 *  draws them. */
const noted = (subtitle: string, partial: PartialEnds, note = SUMMED_PARTIAL_NOTE): string => sentences([subtitle, partial.first ? FIRST_PARTIAL_NOTE : null, partial.last ? note : null]);
// Each read by its card and by the range pill's sentence, which names the three Traffic cards that read a route of
// their own in a session view, so a renamed card renames the sentence too.
const MIGRATION_REWRITES_TITLE = 'Migration rewrites';
const REFUSAL_CATEGORIES_TITLE = 'Refusals by category';
const GUARDRAILS_TITLE = 'Guardrails';
const spanWords = (w: { fromMs: number; toMs: number }): string => `${fmtAbsolute(w.fromMs)} to ${fmtAbsolute(w.toMs)}`;
/**
 * The range pill's sentence: the window the open tab's cards read, by the tab first and the view second. The burn read
 * and the account rollups send no session, so Burn and Accounts name their route's window in a session view too, where
 * three Traffic cards read their routes beside the folded ones. Burn names the window of the answer its cards draw,
 * whose first bucket the route chose, so it names none until that answer holds a point. `burnFirstAt` is that answer's
 * earliest point, null where it holds none.
 */
function rangeSentenceOf(tab: SectionId, session: boolean, wholeHours: { fromMs: number; toMs: number }, burn: Stamped<{ points: BurnPoint[] }> | null, burnFirstAt: number | null): string | undefined {
  if (tab === 'burn') {
    if (burn === null || burnFirstAt === null) return undefined;
    return `Reads from the first bucket's start: ${spanWords(coveredWindow('burn', { fromMs: burn.fromMs, toMs: burn.endMs }, burn.readAt, burnFirstAt))}`;
  }
  if (tab === 'accounts' || !session) return `Reads whole hours: ${spanWords(wholeHours)}`;
  if (tab === 'traffic') return `Reads the range itself. ${REFUSAL_CATEGORIES_TITLE}, ${MIGRATION_REWRITES_TITLE} and ${GUARDRAILS_TITLE} read whole hours: ${spanWords(wholeHours)}`;
  return 'Reads the range itself';
}
type TimeAxis = { axisLine: { stroke: string }; dataKey: 'at'; height?: number; label?: { fill: string; fontSize: number; position: 'insideBottomRight'; value: string }; minTickGap: number; tickFormatter: (v: number) => string; tickLine: false };
/**
 * A time axis over buckets `bucketMs` wide. The 6-hour and daily buckets are UTC multiples, so their ticks print the
 * viewer's date with the local time beside it where the boundary is not local midnight, and the axis names the
 * boundary once; hour ticks stay local, as the tooltips do.
 */
function timeAxis(bucketMs: number, ink: ChartInk): TimeAxis {
  const look = { axisLine: { stroke: ink.axis }, dataKey: 'at', minTickGap: 40, tickLine: false } as const;
  if (bucketMs < 6 * HOUR) return { ...look, tickFormatter: (v) => fmtShortTime(v) };
  // The label sits under the last tick, so the axis grows a line taller than its 30 px to keep the two apart.
  return { ...look, height: 40, label: { fill: ink.ink2, fontSize: 11, position: 'insideBottomRight', value: 'days start at 00:00 UTC' }, tickFormatter: (v) => fmtDayTick(v) };
}
/** The top tick label and a line's last point center on the top and right edges. The axes hold the other labels. */
const CHART_MARGIN = { top: 8, right: 8, bottom: 0, left: 0 };
/**
 * A stacked time chart's bars, one per series, the surface outline parting the segments and the top segment rounded.
 * A chart calls it as a function: recharts finds its series among its children by element type, which a component in
 * between would hide.
 */
const stackedBars = <S extends { key: string; slot: number }>(series: readonly S[], dataKeyOf: (s: S) => string, ink: ChartInk) =>
  series.map((s, i) => <Bar key={s.key} dataKey={dataKeyOf(s)} stackId="a" fill={ink.series[s.slot]} stroke={ink.surface} strokeWidth={1} maxBarSize={22} {...TIME_BAR} radius={i === series.length - 1 ? TIME_BAR.radius : 0} />);

type SpeedRow = { at: number; fast: number; standard: number };
const requestsIn = (p: AnalyticsPoint, key: string) => p.groups.find((g) => g.key === key)?.requests ?? 0;

/**
 * Requests per bucket by the speed their answers reported, else `fast` for a request that asked for it, stacked as
 * Requests by outcome stacks its outcomes.
 */
function SpeedCard({ axis, points, error, loading, partial }: { axis: TimeAxis; error: string | null; loading: boolean; partial: PartialEnds; points: AnalyticsPoint[] }) {
  const ink = useChartInk();
  const data = useMemo(() => points.map((p): SpeedRow => ({ at: Date.parse(p.at), fast: requestsIn(p, 'fast'), standard: requestsIn(p, 'standard') })), [points]);
  return (
    <ChartCard
      title="Requests by speed"
      subtitle={noted('Standard and fast per bucket, stacked', partial)}
      failure={failed(error)}
      loading={loading}
      legend={SPEED_SERIES.map((s) => ({ color: ink.series[s.slot], label: s.label, value: fmtInt(data.reduce((n, d) => n + d[s.key], 0)) }))}
    >
      <BarChart data={data} margin={CHART_MARGIN} barCategoryGap={BAR_GAP}>
        <CartesianGrid vertical={false} stroke={ink.grid} />
        <XAxis {...axis} />
        <YAxis {...COUNT_AXIS} />
        <Tooltip cursor={{ fill: ink.grid, opacity: 0.6 }} content={timeTip<SpeedRow>((p) => SPEED_SERIES.map((s) => ({ color: ink.series[s.slot], label: s.label, value: fmtInt(p[s.key]) })))} />
        {stackedBars(SPEED_SERIES, (s) => s.key, ink)}
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

function RewritesCard({ answer, axis, error, inForce, partial }: { answer: RewritesAnswer | null; axis: TimeAxis; error: string | null; inForce: ReadonlySet<AnalyticsControl>; partial: PartialEnds }) {
  const ink = useChartInk();
  const loading = answer === null;
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
      title={MIGRATION_REWRITES_TITLE}
      subtitle={noted(sentences(['Cache-write tokens the first request after a move wrote, per bucket', ignoredNote('rewrites', inForce)]), partial)}
      failure={failed(error)}
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
            <ul className="plain-list hbars hbars--moves" aria-label="Moves by reason">
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
          {answer && <HeldChart answer={answer} axis={axis} note={ignoredNote('rewritesHeld', inForce)} />}
        </>
      }
    >
      <BarChart data={data} margin={CHART_MARGIN} barCategoryGap={BAR_GAP}>
        <CartesianGrid vertical={false} stroke={ink.grid} />
        <XAxis {...axis} />
        <YAxis {...COUNT_AXIS} />
        <Tooltip cursor={{ fill: ink.grid, opacity: 0.6 }} content={timeTip<{ at: number; rewriteTokens: number }>((p) => [{ color: ink.series[1], label: 'rewrite tokens', value: fmtCompact(p.rewriteTokens) }])} />
        <Bar dataKey="rewriteTokens" fill={ink.series[1]} maxBarSize={22} {...TIME_BAR} />
      </BarChart>
    </ChartCard>
  );
}

type HeldRow = { at: number; heldMs: number; requests: number };

/**
 * The slow-down's held time per bucket, which the rewrites card draws under its moves: a bar per bucket in the
 * warning tone a held request's marker wears, its hover the requests that waited and their time, and its legend line
 * the range's sum. A range that held nothing says so on the legend line and draws no chart. `note` stands under the
 * legend line in a card subtitle's type, since the chart carries no subtitle of its own and a legend entry would read
 * as a series.
 */
function HeldChart({ answer, axis, note }: { answer: RewritesAnswer; axis: TimeAxis; note: string | null }) {
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
      {note !== null && <p className="card__subtitle">{note}</p>}
      {held.totalRequests > 0 && (
        <ResponsiveContainer width="100%" height={120}>
          <BarChart data={data} margin={CHART_MARGIN} barCategoryGap={BAR_GAP}>
            <CartesianGrid vertical={false} stroke={ink.grid} />
            <XAxis {...axis} />
            <YAxis tickFormatter={(v: number) => fmtMs(v)} tickLine={false} axisLine={false} width={44} />
            <Tooltip cursor={{ fill: ink.grid, opacity: 0.6 }} content={timeTip<HeldRow>((p) => [{ color: ink.warning, label: heldHover(p), value: null }])} />
            <Bar dataKey="heldMs" fill={ink.warning} maxBarSize={22} {...TIME_BAR} />
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
function GuardrailsCard({ answer, error, inForce }: { answer: GuardrailCounts | null; error: string | null; inForce: ReadonlySet<AnalyticsControl> }) {
  const rows = answer?.guardrails ?? [];
  return (
    <Card title={GUARDRAILS_TITLE} subtitle={sentences(['Matches by guardrail in range, every key', ignoredNote('guardrails', inForce)])} padded={false} className="card--table analytics-guardrails">
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
              <StateRow colSpan={4} empty="No guardrail matched in this range." error={error} errorTitle="Could not load the guardrail matches" loading={answer === null} />
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

const SPEND_TITLE = 'Spend per minute';
const SPEND_NOTE = "Dollars per minute: API-key traffic, a usage-based seat's whole spend, and a subscription account's fast-mode and overage answers";
/** The note's second sentence: what Plan per hour beside the card charts by its rate's kind, none where it draws a
 *  sentence and no chart. */
const SPEND_PLAN_NOTE: Record<ReturnType<typeof rateKind>['kind'], string | null> = {
  dash: null,
  dollars: 'Its plan quota is charted beside it in dollars at the API price list',
  equivalent: 'Its plan quota is charted beside it as a share of the chosen week',
};
/** A folded rest of subscription accounts as the dollars chart names it: its dollars are its members' fast-mode
 *  and overage answers, and its own label names the plan unit, which the dollars axis does not chart. */
const SPEND_REST = 'Other (fast mode or overage)';
const SPEND_REST_LABEL: Record<Exclude<BurnLineUnit, 'usd'>, string> = { unknown: 'Other (fast mode or overage, tier unknown)', window_share: SPEND_REST };
const TIER_UNKNOWN_HINT = 'A free plan, no tier on file or one the cost model does not hold, and no bucket of the range priced, so no share of the week is drawn';
const MODEL_UNLISTED_HINT = 'A bucket whose traffic ran on a model without a weight in the cost model prices no share and draws no point';
const TIER_UNSIZED_HINT = "A bucket priced under a cost revision that holds no capacity for the chosen tier on this card's window draws no point";
const NO_ACCOUNT: ReadonlySet<string> = new Set();
const EMPTY_FOLD = { lines: [] as BurnLine[], rows: [] as BurnRow[] };
const NO_POINTS: AnalyticsPoint[] = [];
const NO_REQUESTS: RequestLog[] = [];
const PARTIAL_NOTE = 'The last bucket is partial, rated over the part of its width it covers';
const PLAN_TITLE = 'Plan per hour';
const PLAN_SUBTITLE = 'The subscription traffic per hour as a share of the chosen week';
/** Plan per hour's subtitle under a dollar position. */
const PLAN_USD_SUBTITLE = 'The subscription traffic per hour in dollars at the API price list';
const FIVE_HOUR_TITLE = '5-hour per hour';
const FIVE_HOUR_SUBTITLE = "The subscription traffic per hour as a share of the chosen tier's 5-hour window, the peak band's factor applied by the hour";
const FIVE_HOUR_ULTRA_NOTE = "The Ultra tier's 5-hour capacity row is assumed, and an approved account carries no 5-hour window.";
const FABLE_WEEK_TITLE = 'Fable week per hour';
const FABLE_WEEK_SUBTITLE = "The subscription traffic per hour as a share of the chosen tier's Fable week";
const NO_PLAN_TRAFFIC = 'No traffic drew plan quota in this range.';
/** Plan per hour's empty sentence under a dollar position where the range holds subscription traffic, a line or a tier
 *  unknown entry under Native, and no group's plan dollars are above zero. */
const NO_LISTED_PLAN_TRAFFIC = 'No subscription traffic in this range carries an API list price.';
const NO_FABLE_TRAFFIC = 'No subscription traffic drew the Fable week in this range.';

/** A plan card's figure under the chosen week, in full for a hover and short for the legend. */
type FigureWords = (v: number, week: WeekPosition, form: 'full' | 'short') => string;
const weekWords: FigureWords = (v, week, form) => (form === 'full' ? fmtWeek(v, week) : weekShort(v, week));
/** A tier window's words, such as "0.11% of the Ultra 5-hour window" in full and "0.11% Ultra 5-hour" short, or "0.11%
 *  of the Ultra Fable week" and "0.11% Ultra Fable week", a zero "0%" in both as `fmtWeek` reads it. */
const tierWindowWords = (full: string, short: string): FigureWords => (v, week, form) => (v === 0 ? '0%' : form === 'full' ? `${fmtShare(v)} of the ${WEEK_SHORT[week]} ${full}` : `${fmtShare(v)} ${WEEK_SHORT[week]} ${short}`);
const fiveHourWords = tierWindowWords('5-hour window', '5-hour');
const fableWeekWords = tierWindowWords('Fable week', 'Fable week');

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
 * The burn route's views of one grouping: the dollars per minute, and the plan cards in the chosen week, its tier's
 * 5-hour window and its Fable week. A line's color follows its group within the answer rather than its rank, so a range
 * that reorders the groups does not repaint them, and the account grouping keeps each account's page-wide color.
 * `account` is the one account the Accounts filter names, and `accounts` the accounts read beside the answer, which
 * give a tooltip's hint its plan and tier. `unit` is the page's Unit position, which "Plan per hour" and "Spend per
 * minute" follow by the rate rule.
 */
function BurnCharts({
  account,
  accounts,
  answer,
  costs,
  error,
  groupBy,
  rangeEndMs,
  readAt,
  sized,
  slotOf,
  unit,
  wait,
  week,
}: {
  account: string | null;
  accounts: Account[] | null;
  answer: { bucketSeconds: number; points: BurnPoint[] } | null;
  costs: Settings['subscriptionCosts'] | null;
  error: string | null;
  groupBy: BurnGroup;
  rangeEndMs: number | undefined;
  readAt: number | undefined;
  sized: ReadonlySet<string> | null;
  slotOf: (id: string) => number | null;
  unit: UnitPosition;
  /** The reading the plan cards wait on while the settings read is out, null where they draw from the burn read. */
  wait: WeekReading | null;
  week: WeekPosition;
}) {
  const ink = useChartInk();
  const loading = answer === null;
  // Each plan card's points with its figure under the chosen tier and its sentence, and one fold per card over them, so
  // the legend's peak and per-hour mean come out in that card's window with the rows.
  const cards = useMemo(() => {
    const points = answer?.points ?? [];
    return { fable: fableWeekCard(points, week), fiveHour: fiveHourCard(points, week), week: weekCard(points, week) };
  }, [answer, week]);
  const fold = useCallback(
    (points: FiguredPoint[]) => (answer === null ? EMPTY_FOLD : foldBurn(points, groupBy, answer.bucketSeconds, sized ?? NO_ACCOUNT, rangeEndMs, readAt, { account })),
    [account, answer, groupBy, rangeEndMs, readAt, sized],
  );
  // Spend per minute, Plan per hour under a dollar position and the colors read this fold's lines too.
  const weekFold = useMemo(() => fold(cards.week.points), [cards.week, fold]);
  const fiveHourFold = useMemo(() => fold(cards.fiveHour.points), [cards.fiveHour, fold]);
  const fableWeekFold = useMemo(() => fold(cards.fable.points), [cards.fable, fold]);
  const { lines, rows } = weekFold;
  // The points by bucket and group, for the tooltip's hint, which reads a bucket's one point of one account.
  const pointsAt = useMemo(() => groupInto(answer?.points ?? [], (p) => `${Date.parse(p.at)}|${p.group}`), [answer]);
  const named = useMemo(() => burnColorOrder([weekFold, fiveHourFold, fableWeekFold]), [fableWeekFold, fiveHourFold, weekFold]);
  // The three folds can name more groups than the eight series colors, so a later group takes a color by its place,
  // wrapped round them. The series' last entry is the muted ink, which a named line never takes.
  const colorOf = (l: BurnLine) => (l.other ? ink.muted : groupBy === 'account' ? accountColor(ink, slotOf(l.key)) : ink.series[named.indexOf(l.key) % (ink.series.length - 1)]);
  // A session id runs past any legend, so it shows cut as the Sessions view cuts it. Both charts' legend entries
  // and the spend chart's tooltip show the whole id on hover.
  const nameOf = (l: BurnLine) => (groupBy === 'session' && !l.other ? shortId(l.label, SESSION_CHARS) : l.label);
  const titleOf = (l: BurnLine) => (groupBy === 'request_class' ? upperFirst(l.key === 'conversation' ? CONVERSATION_HINT : OVERHEAD_HINT) : groupBy === 'session' && !l.other ? l.label : undefined);
  const axis = timeAxis((answer?.bucketSeconds ?? 3_600) * 1_000, ink);
  // Read off the buckets, not the lines: the class grouping draws its two lines even over a range with no traffic.
  const empty = !loading && error === null && rows.length === 0 ? 'No traffic in this range.' : undefined;
  const byAccount = groupBy === 'account';
  // Where each group holds one account's rows, a line's unit is its account's.
  const shared = byAccount || account !== null;
  // Until the settings land no account reads as sized, so the rest names no tier it cannot know yet.
  const spendNameOf = (l: BurnLine) => (l.other && l.unit !== 'usd' ? (sized === null ? SPEND_REST : SPEND_REST_LABEL[l.unit]) : nameOf(l));
  const partial = rows[rows.length - 1]?.partial === true;
  // A bucket too short to set the scale would stretch the axis over a few seconds' rate, so the axis stops at the
  // buckets that set it and the short one runs past the top.
  const scaleTo = (total: number, setting: number, values: number[]) => {
    const top = Math.max(0, ...values);
    return setting > 0 && setting < total && top > 0 ? { allowDataOverflow: true, domain: [0, niceAxisTop(top)] as [number, number] } : {};
  };
  const settingRows = rows.filter((r) => r.setsScale);
  // Without a capped domain recharts spans every row, so the labels' precision reads every row too.
  const dollarAxisOf = (valuesOf: (from: BurnRow[]) => number[]) => {
    const setting = valuesOf(settingRows);
    const scale = scaleTo(rows.length, settingRows.length, setting);
    return { ...usdAxis('domain' in scale ? setting : valuesOf(rows)), ...scale };
  };
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
    return ownShareHint(owner, { five_hour: p.fiveHourShare, seven_day: p.sevenDayShare, ...(weekKey === undefined ? {} : { [weekKey]: p.modelWeekShare }) });
  };
  // A line draws while a plan paid for its traffic, a group of billed rows alone reading zero, and where each group
  // holds one account's rows only a line in the plan's unit draws: one the router sizes or whose points carry a figure.
  const plannedOf = (cardLines: BurnLine[]) => cardLines.map((l, i) => ({ i, l })).filter(({ l }) => !l.other && (!shared || l.unit === 'window_share') && (l.unpriced || l.unsized || (l.perHour !== null && l.perHour > 0)));
  const unknownOf = (cardLines: BurnLine[]) => (shared ? cardLines.filter((l) => !l.other && l.unit === 'unknown') : []);
  // The range holds subscription traffic where Plan per hour under Native draws a line or a tier unknown entry.
  const subscribed = plannedOf(lines).length > 0 || unknownOf(lines).length > 0;
  // The Fable week fold keeps the groups with Fable traffic alone, so where Plan per hour draws subscription traffic
  // the Fable week card with no line says that traffic drew no Fable week.
  const fableNoLine = subscribed ? NO_FABLE_TRAFFIC : NO_PLAN_TRAFFIC;
  const headOf = (subtitle: string, note: string | null, withPartial = partial) => (
    <>
      {subtitle}.{withPartial ? ` ${PARTIAL_NOTE}.` : ''}
      {note === null ? '' : ` ${note}`} <CostReferenceLink tab="tiers" />
    </>
  );
  const planCard = ({
    fold: { lines: cardLines, rows: cardRows },
    hint,
    noLine,
    note,
    subtitle,
    title,
    unpriced,
    words,
  }: {
    fold: { lines: BurnLine[]; rows: BurnRow[] };
    /** The tooltip's hint under a line, null on a card that draws no account's own shares. */
    hint: ((l: BurnLine, at: number) => string | null) | null;
    /** The empty sentence where the range holds traffic and the card draws no line and no tier unknown entry. */
    noLine: string;
    note: string | null;
    subtitle: string;
    title: string;
    /** The sentence the card's function reads in place of its chart, null where it draws. */
    unpriced: string | null;
    words: FigureWords;
  }) => {
    const head = headOf(subtitle, note);
    // While the cards wait on the cost model each draws its skeleton, the burn read's failure, or once that read
    // answered the settings read's failure.
    if (wait !== null) {
      return (
        <ChartCard key={title} title={title} subtitle={head} failure={failed(error) ?? (loading ? null : wait.failure)} loading>
          {null}
        </ChartCard>
      );
    }
    const planned = plannedOf(cardLines);
    const unknown = unknownOf(cardLines);
    const scaled = cardRows.filter((r) => r.setsScale);
    // The figure in the card's window, the full form on a hover and the short one in the legend.
    const figureOf = (v: number | null, form: 'full' | 'short') => (v === null ? '-' : words(v, week, form));
    const legend = [
      ...planned.map(({ l }) => ({
        color: colorOf(l),
        label: nameOf(l),
        shape: 'line' as const,
        title: sentences([titleOf(l) ?? null, l.peak === null ? null : `Highest hour ${figureOf(l.peak, 'full')}`]) || undefined,
        // The figure's own hover names it in full and masks the entry's title over the figure. A dash has none.
        value: l.perHour === null ? '-' : <span title={figureOf(l.perHour, 'full')}>{figureOf(l.perHour, 'short')}</span>,
      })),
      // One entry per cause of a gap for every bucket of that cause, whichever line it fell on. Muted, since neither
      // draws a line of its own.
      ...(planned.some(({ l }) => l.unpriced) ? [{ color: ink.muted, label: 'model unlisted', shape: 'line' as const, title: MODEL_UNLISTED_HINT }] : []),
      ...(planned.some(({ l }) => l.unsized) ? [{ color: ink.muted, label: 'tier unsized', shape: 'line' as const, title: TIER_UNSIZED_HINT }] : []),
      // Muted, since no line of its own is drawn.
      ...unknown.map((l) => ({ color: ink.muted, label: nameOf(l), shape: 'line' as const, title: sentences([titleOf(l) ?? null, TIER_UNKNOWN_HINT]), value: 'tier unknown' })),
    ];
    return (
      <ChartCard
        key={title}
        title={title}
        subtitle={head}
        failure={failed(error) ?? (unpriced === null ? null : { text: unpriced, unpriced: true })}
        loading={loading}
        empty={
          empty ??
          (planned.length > 0
            ? undefined
            : unknown.length > 0
              ? byAccount
                ? `No subscription account in range has a tier the router prices: ${unknown.map(nameOf).join(', ')}.`
                : 'The named account has a free plan, or has no tier on file or one the cost model does not hold, and no bucket of the range priced.'
              : noLine)
        }
        legend={legend}
      >
        <LineChart data={cardRows} margin={CHART_MARGIN}>
          <CartesianGrid vertical={false} stroke={ink.grid} />
          <XAxis type="number" scale="time" domain={['dataMin', 'dataMax']} ticks={cardRows.map((r) => r.at)} {...axis} />
          <YAxis tickFormatter={(v: number) => fmtShare(v)} tickLine={false} axisLine={false} width={52} {...scaleTo(cardRows.length, scaled.length, scaled.flatMap((r) => planned.map(({ i }) => r.equivalent[i] ?? 0)))} />
          {/* Recharts drops null entries by default, and a bucket whose every figure is null would lose its date, its
              dashes and the unpriced hint. */}
          <Tooltip
            cursor={{ stroke: ink.axis }}
            filterNull={false}
            content={timeTip<BurnRow>((r) =>
              planned.flatMap(({ i, l }) => {
                const v = r.equivalent[i];
                const said = hint === null ? null : hint(l, r.at);
                // Each gap's row names its own slot's cause, so a line holding both gaps names each where it fell.
                const gapHint = r.unsized[i] ? TIER_UNSIZED_HINT : l.unpriced && slotUnpriced(v, r.tokens[i]) ? MODEL_UNLISTED_HINT : undefined;
                return [{ color: colorOf(l), label: nameOf(l), title: gapHint, value: figureOf(v, 'full') }, ...(said === null ? [] : [{ label: said, value: null }])];
              }),
            )}
          />
          {planned.map(({ i, l }) => (
            // A null slot draws no point, and the gap stays since the lines do not connect across it.
            <Line key={i} type="monotone" dataKey={(r: BurnRow) => r.equivalent[i]} stroke={colorOf(l)} strokeWidth={2} dot={isolatedDot(cardRows.map((r) => r.equivalent[i]), colorOf(l))} isAnimationActive={false} />
          ))}
        </LineChart>
      </ChartCard>
    );
  };
  // Under All in dollars and Dollar equivalent the plan side charts its list dollars, which read no cost table, so the
  // card waits on no scale. A dollar figure reads neither a weight nor a tier, so a line draws wherever its group's
  // plan dollars are above zero, with no model unlisted entry, no tier unknown entry and no share hint.
  const planDollarCard = () => {
    const planned = lines.map((l, i) => ({ i, l })).filter(({ l }) => l.planUsdPerHour > 0);
    return (
      <ChartCard
        key={PLAN_TITLE}
        title={PLAN_TITLE}
        subtitle={headOf(PLAN_USD_SUBTITLE, null)}
        failure={failed(error)}
        loading={loading}
        empty={empty ?? (planned.length > 0 ? undefined : subscribed ? NO_LISTED_PLAN_TRAFFIC : NO_PLAN_TRAFFIC)}
        legend={planned.map(({ l }) => ({ color: colorOf(l), label: nameOf(l), shape: 'line' as const, title: sentences([titleOf(l) ?? null, l.planUsdPeak === null ? null : `Highest hour ${fmtUsd(l.planUsdPeak)}`]) || undefined, value: fmtUsd(l.planUsdPerHour) }))}
      >
        <LineChart data={rows} margin={CHART_MARGIN}>
          <CartesianGrid vertical={false} stroke={ink.grid} />
          <XAxis type="number" scale="time" domain={['dataMin', 'dataMax']} ticks={rows.map((r) => r.at)} {...axis} />
          <YAxis {...dollarAxisOf((from) => from.flatMap((r) => planned.map(({ i }) => r.planUsd[i])))} tickLine={false} axisLine={false} />
          <Tooltip cursor={{ stroke: ink.axis }} content={timeTip<BurnRow>((r) => planned.map(({ i, l }) => ({ color: colorOf(l), label: nameOf(l), value: fmtUsd(r.planUsd[i]) })))} />
          {planned.map(({ i, l }) => (
            <Line key={i} type="monotone" dataKey={(r: BurnRow) => r.planUsd[i]} stroke={colorOf(l)} strokeWidth={2} dot={false} isAnimationActive={false} />
          ))}
        </LineChart>
      </ChartCard>
    );
  };
  // A grouping other than the account one mixes both kinds of account, so its lines chart the dollars their API-key
  // traffic and fast-mode and overage answers spent. Under the account grouping a subscription account, or a folded
  // rest of them, draws a dollar line only for what its fast-mode and overage answers spent.
  const spendCard = () => {
    const spent = lines.map((l, i) => ({ i, l })).filter(({ l }) => !byAccount || l.unit === 'usd' || l.usdPerMinute > 0);
    return (
      <ChartCard
        title={SPEND_TITLE}
        subtitle={noted(spendNote, { first: false, last: partial }, PARTIAL_NOTE)}
        failure={failed(error)}
        loading={loading}
        empty={empty ?? (spent.length === 0 ? 'No account spent dollars in this range.' : undefined)}
        legend={spent.map(({ l }) => ({ color: colorOf(l), label: spendNameOf(l), shape: 'line' as const, title: titleOf(l), value: fmtUsd(l.usdPerMinute) }))}
      >
        <LineChart data={rows} margin={CHART_MARGIN}>
          <CartesianGrid vertical={false} stroke={ink.grid} />
          <XAxis {...axis} />
          <YAxis {...dollarAxisOf((from) => from.flatMap((r) => spent.map(({ i }) => r.usd[i])))} tickLine={false} axisLine={false} />
          <Tooltip cursor={{ stroke: ink.axis }} content={timeTip<BurnRow>((r) => spent.map(({ i, l }) => ({ color: colorOf(l), label: spendNameOf(l), title: titleOf(l), value: fmtUsd(r.usd[i]) })))} />
          {spent.map(({ i, l }) => (
            <Line key={i} type="monotone" dataKey={(r: BurnRow) => r.usd[i]} stroke={colorOf(l)} strokeWidth={2} dot={false} isAnimationActive={false} />
          ))}
        </LineChart>
      </ChartCard>
    );
  };
  /** A card whose side has no figure under the position keeps its place in the grid with the sentence of why, and its
   *  subtitle leaves out the partial bucket's note, since no bucket draws. */
  const sentenceCard = (title: string, subtitle: ReactNode, sentence: string) => (
    <ChartCard key={title} title={title} subtitle={subtitle} loading={false} empty={`${sentence}.`}>
      {null}
    </ChartCard>
  );
  const planRate = rateKind(unit, 'plan');
  const spendRate = rateKind(unit, 'billed');
  const planNote = SPEND_PLAN_NOTE[planRate.kind];
  const spendNote = `${SPEND_NOTE}${planNote === null ? '' : `. ${planNote}${byAccount ? '' : ' when grouped by account'}`}${groupBy === 'request_class' ? `. ${upperFirst(CLASS_NOTE)}` : ''}`;
  return (
    <div className="grid-2 burn-grid">
      {planRate.kind === 'equivalent'
        ? planCard({ fold: weekFold, hint: hintOf, noLine: NO_PLAN_TRAFFIC, note: null, subtitle: PLAN_SUBTITLE, title: PLAN_TITLE, unpriced: cards.week.unpriced, words: weekWords })
        : planRate.kind === 'dash'
          ? sentenceCard(PLAN_TITLE, headOf(PLAN_USD_SUBTITLE, null, false), planRate.title)
          : planDollarCard()}
      {planCard({ fold: fiveHourFold, hint: null, noLine: NO_PLAN_TRAFFIC, note: week === 'ultra' ? FIVE_HOUR_ULTRA_NOTE : null, subtitle: FIVE_HOUR_SUBTITLE, title: FIVE_HOUR_TITLE, unpriced: cards.fiveHour.unpriced, words: fiveHourWords })}
      {planCard({ fold: fableWeekFold, hint: null, noLine: fableNoLine, note: null, subtitle: FABLE_WEEK_SUBTITLE, title: FABLE_WEEK_TITLE, unpriced: cards.fable.unpriced, words: fableWeekWords })}
      {spendRate.kind === 'dash' ? sentenceCard(SPEND_TITLE, spendNote, spendRate.title) : spendCard()}
    </div>
  );
}

const PLACEMENT_HINT = "A placement by the conversation's own messages is a hit when the response read the prefix it was promised, and one by the static prefix when it read at all";

/** Placements by rung over the range, narrowed by the Accounts, Clients, Keys, Models, Pools and User agents
 *  filters. */
function PlacementTable({ error, inForce, rows }: { error: string | null; inForce: ReadonlySet<AnalyticsControl>; rows: PlacementRow[] | null }) {
  return (
    <Card title="Placements by rung" subtitle={sentences([PLACEMENT_HINT, ignoredNote('placements', inForce)])} padded={false} className="card--table">
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
              <StateRow colSpan={6} empty="No placements in range" error={error} errorTitle="Could not load the placements" loading={rows === null} />
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
                    <Link to={accountHref(b.accountId)}>{b.accountId}</Link>
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
              <StateRow colSpan={5} empty="No member carries a budget" error={error} errorTitle="Could not load the member budgets" loading={budgets === null} />
            )}
          </tbody>
        </table>
      </div>
    </Card>
  );
}

const NO_SAMPLE_HINT = 'no utilization sampled in range';
const UNSAMPLED = <span className="muted" title={NO_SAMPLE_HINT}>-</span>;
const NO_FABLE = <span className="muted" title="no Fable window">n/a</span>;
const peakCell = (v: number | null, none: ReactNode = UNSAMPLED): ReactNode => (v === null ? none : fmtPct(v));
const WINDOW_OPTIONS = WINDOW_KEYS.map((id) => ({ label: windowLabelOf(id), value: id }));

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
  inForce,
  registry,
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
  inForce: ReadonlySet<AnalyticsControl>;
  /** The registry read's accounts, null until its first answer and while that first read has failed. */
  registry: Account[] | null;
  rollups: AccountRollup[] | null;
  slotOf: (id: string) => number | null;
  span: { fromMs: number; toMs: number };
  week: WeekPosition;
}) {
  const ink = useChartInk();
  const loading = all === null;
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
  // A null Fable peak reads n/a only where the registry's row holds no Fable window, so an account the registry no
  // longer holds, and every account until the registry's first answer and while that first read has failed, reads the
  // unsampled dash.
  const fableless = useMemo(() => new Set((registry ?? []).filter((a) => a.fableWindow === null).map((a) => a.id)), [registry]);
  const data = useMemo(() => peakLines(rollups ?? [], accounts, span, bucketMs), [rollups, accounts, span, bucketMs]);
  const [window, setWindow] = useState('five_hour');
  const usage = useMemo(() => windowLines(rollups ?? [], window, 'usage', span, bucketMs), [rollups, window, span, bucketMs]);
  const pace = useMemo(() => windowLines(rollups ?? [], window, 'pace', span, bucketMs), [rollups, window, span, bucketMs]);
  // The axis takes the widest lead either side of zero, raised to a round tick, so ahead and behind read alike.
  const paceTop = useMemo(() => {
    const widest = Math.max(0, ...pace.data.flatMap((r) => pace.accounts.map((a) => Math.abs(r[a] ?? 0))));
    return widest > 0 ? niceAxisTop(widest) : 50;
  }, [pace]);
  const toggle = <Segmented value={window} options={WINDOW_OPTIONS} onChange={setWindow} ariaLabel="Window" />;
  const colorOf = (a: string) => accountColor(ink, slotOf(a));
  // The Peak chart draws a line for every account, so its tooltip keeps a row for each, and a window card's follows
  // the accounts its own answer draws.
  const legendOf = (listed: string[]) => listed.map((a) => ({ color: colorOf(a), label: a, shape: 'line' as const }));
  const shareTipOf = (listed: string[]) => timeTip<Record<string, number | null> & { at: number }>((p) => listed.map((a) => ({ color: colorOf(a), label: a, value: pct(p[a]) })));
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
        legend={legendOf(accounts.filter((a) => data.some((p) => p[a] !== null && p[a] !== undefined)))}
      >
        <LineChart data={data} margin={CHART_MARGIN}>
          <CartesianGrid vertical={false} stroke={ink.grid} />
          <XAxis {...axis} />
          <YAxis {...PCT_AXIS} />
          <Tooltip cursor={{ stroke: ink.axis }} content={shareTipOf(accounts)} />
          {accounts.map((a) => (
            <Line key={a} type="monotone" dataKey={a} stroke={colorOf(a)} strokeWidth={1.5} dot={false} connectNulls isAnimationActive={false} />
          ))}
        </LineChart>
      </ChartCard>
      <div className="grid-2">
        <ChartCard title="Window usage" subtitle="The window's utilization per bucket, from the quota samples" actions={toggle} loading={loading} legend={legendOf(usage.accounts)}>
          <LineChart data={usage.data} margin={CHART_MARGIN}>
            <CartesianGrid vertical={false} stroke={ink.grid} />
            <XAxis {...axis} />
            <YAxis {...PCT_AXIS} />
            <Tooltip cursor={{ stroke: ink.axis }} content={shareTipOf(usage.accounts)} />
            {/* The lines leave a null bucket unconnected, so a bucket with no sample reads as a gap rather than a
                slope, and a sampled bucket with a gap or the range's edge on both sides draws as a dot, as the share
                chart draws one. */}
            {usage.accounts.map((a) => (
              <Line key={a} type="monotone" dataKey={a} stroke={colorOf(a)} strokeWidth={1.5} dot={isolatedDot(usage.data.map((r) => r[a]), colorOf(a))} isAnimationActive={false} />
            ))}
          </LineChart>
        </ChartCard>
        <ChartCard title="Pace" subtitle="The lead over pace per bucket: above zero ahead, below behind, from the quota samples, before the router's burn projection" actions={toggle} loading={loading} legend={legendOf(pace.accounts)}>
          <LineChart data={pace.data} margin={CHART_MARGIN}>
            <CartesianGrid vertical={false} stroke={ink.grid} />
            <XAxis {...axis} />
            <YAxis domain={[-paceTop, paceTop]} ticks={[-paceTop, -paceTop / 2, 0, paceTop / 2, paceTop]} tickFormatter={(v: number) => pct(v, true)} tickLine={false} axisLine={false} width={44} />
            <ReferenceLine y={0} stroke={ink.axis} />
            <Tooltip cursor={{ stroke: ink.axis }} content={timeTip<Record<string, number | null> & { at: number }>((p) => pace.accounts.flatMap((a) => [{ color: colorOf(a), label: a, value: pct(p[a], true) }, { label: `Pace score, ${a}`, title: "the remaining share over the hours from the sample's instant to the window's deadline, the earlier of its reset and the account's end", value: scoreOf(p, a) }]))} />
            {pace.accounts.map((a) => (
              <Line key={a} type="monotone" dataKey={a} stroke={colorOf(a)} strokeWidth={1.5} dot={isolatedDot(pace.data.map((r) => r[a]), colorOf(a))} isAnimationActive={false} />
            ))}
          </LineChart>
        </ChartCard>
      </div>
      <BreakdownTable
        className="accounts-range"
        title="Accounts in range"
        subtitle={sentences(['Totals for the whole hours the range touches and the selected accounts', ignoredNote('accountsTab', inForce)])}
        failure={failed(error)}
        loading={loading}
        groupLabel="Account"
        columns={['Peak 5h', 'Peak 7d', 'Peak Fable', 'Time in overage', 'Time in cooldown', 'Pins created', 'Migrated in', 'Migrated out']}
        rows={table.map((r) => ({
          key: r.id,
          label: r.id,
          cells: [
            peakCell(r.peak5),
            peakCell(r.peak7),
            peakCell(r.peakF, fableless.has(r.id) ? NO_FABLE : UNSAMPLED),
            r.overage ? fmtDuration(r.overage * 1000) : <span className="muted">none</span>,
            r.cooldown ? fmtDuration(r.cooldown * 1000) : <span className="muted">none</span>,
            fmtInt(r.created),
            fmtInt(r.migratedIn),
            fmtInt(r.migratedOut),
          ],
        }))}
      />
      <AllowanceTable {...allowance} inForce={inForce} span={span} week={week} />
    </>
  );
}

/** A figure's cell, a dash in the muted ink that says why on hover. */
const allowanceCell = (key: string, figure: AllowanceFigure | undefined) => (
  <td key={key} className="right num col-fit">
    {figure === undefined ? (
      <span className="muted" title={NO_RESET_IN_RANGE}>
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

/** A fleet figure, its dash in the muted ink of a row's where it says why. */
const fleetValue = (figure: FleetFigure) =>
  figure.title === null ? (
    figure.text
  ) : (
    <span className="muted" title={figure.title}>
      {figure.text}
    </span>
  );

/** The Resets cell: the natural resets of the 7-day window, and the uses after them with a hover line per claim row. */
const resetsCell = (r: AllowanceRow) =>
  r.used === 0 ? (
    fmtInt(r.resets)
  ) : (
    <span title={r.claims.map((c) => `${windowWord(c.window)} cleared by a used reset, seen ${fmtAbsolute(c.reset.observedAt)}, ${fmtShare(c.reset.shareLeft)} left`).join('\n')}>{`${fmtInt(r.resets)}, ${fmtInt(r.used)} used`}</span>
  );

/**
 * "Unused allowance by account": each subscription account's weekly windows over the range under the Accounts filter in
 * the chosen week, each window's newest reset at or before the range's end beside the sum over its resets in the
 * range, ranked most left first, with the fleet's two figures over the table. A failed read, its own or the cost
 * model's, draws in place of the rows and the figures, so no figure stands without the read it prices.
 */
function AllowanceTable({ answer, costs, error, inForce, span, week }: { answer: UnusedAllowanceAnswer | null; costs: SubscriptionCosts | null; error: string | null; inForce: ReadonlySet<AnalyticsControl>; span: { fromMs: number; toMs: number }; week: WeekPosition }) {
  const rows = useMemo(() => (answer === null || costs === null ? null : allowanceRows(answer, costs, week, span)), [answer, costs, span, week]);
  const windows = useMemo(() => weeklyKeys(answer?.accounts ?? []), [answer]);
  const fleet = rows === null ? null : fleetFigures(rows, week, weekScale(costs, week));
  return (
    <Card
      title="Unused allowance by account"
      subtitle={sentences(["Each subscription account's weekly windows left unused at their resets in the range, under the Accounts filter. Last is the newest reset at or before the range's end", ignoredNote('unusedAllowance', inForce)])}
      padded={false}
      className="card--table accounts-range allowance-table"
    >
      {error === null && (
        <div className="req-totals">
          <div>
            <span className="stat__label">7-day, fleet total</span>
            <span className="stat__value">{fleet === null ? <Skeleton width={80} /> : fleetValue(fleet.sevenDay)}</span>
          </div>
          <div>
            <span className="stat__label">Fable, fleet total</span>
            <span className="stat__value">{fleet === null ? <Skeleton width={80} /> : fleetValue(fleet.fable)}</span>
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
                <th key={`${w}-last`} className="right col-fit">{`${windowWord(w)}, last`}</th>,
                <th key={`${w}-range`} className="right col-fit">{`${windowWord(w)}, in range`}</th>,
              ])}
            </tr>
          </thead>
          <tbody>
            {error === null &&
              rows?.map((r) => (
                <tr key={r.accountId}>
                  <td className="mono col-fill" title={r.accountId}>
                    <Link to={accountHref(r.accountId)}>{r.accountId}</Link>
                    {r.termEndedAt !== null && (
                      <span className="sub" title={fmtAbsolute(r.termEndedAt)}>
                        {`term ended ${resetInstant(r.termEndedAt)}`}
                      </span>
                    )}
                    {r.resets === 0 && <span className="sub">no reset in range</span>}
                  </td>
                  <td className="right num col-fit">{resetsCell(r)}</td>
                  {windows.flatMap((w) => [allowanceCell(`${w}-last`, r.figures[w]?.last), allowanceCell(`${w}-range`, r.figures[w]?.inRange)])}
                </tr>
              ))}
            {(error !== null || rows === null || rows.length === 0) && (
              <StateRow colSpan={2 + windows.length * 2} empty="No weekly reset in the range or the week before it" error={error} errorTitle="Could not load the unused allowance" loading={rows === null} />
            )}
          </tbody>
        </table>
      </div>
    </Card>
  );
}
