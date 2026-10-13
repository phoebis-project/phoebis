import { useEffect, useMemo, useRef, useState, type ReactNode, type RefObject } from 'react';
import { flushSync } from 'react-dom';
import { Area, AreaChart, Bar, BarChart, CartesianGrid, Dot, Line, LineChart, ReferenceArea, ReferenceLine, ResponsiveContainer, Tooltip, XAxis, YAxis } from 'recharts';
import { Link, useLocation, useNavigate } from 'react-router-dom';
import { api } from '@/api';
import { iso, windowId } from '@/api/decode';
import { foldGroups, foldRequests, loadSessionRequests, sumGroups } from '@/api/fold';
import { useAppSettings } from '@/app/settings';
import type { AccountRollup, AnalyticsGroup, AnalyticsGroupBy, AnalyticsPoint, AnalyticsQuery, BurnGroup, BurnPoint, CacheMissReason, CostSplit, LatencyHistograms, MemberBudget, PlacementRow, RequestLog, RewritePoint, SubscriptionCosts } from '@/api/types';
import { Donut } from '@/components/charts/Donut';
import { OUTCOME_SERIES, PERCENTILES, PROBE_SERIES, TOKEN_SERIES, accountColor, accountSlots, type ChartInk } from '@/components/charts/theme';
import { useChartInk } from '@/components/charts/useChartInk';
import { ChartTooltip, Legend, type TooltipRow } from '@/components/charts/Tooltip';
import { CostReferenceLink } from '@/components/ui/CostReference';
import { MultiSelect, type Option } from '@/components/ui/MultiSelect';
import { PatternInput } from '@/components/ui/PatternInput';
import { figuresValue, Tile } from '@/components/ui/Tile';
import { IconX } from '@/components/ui/icons';
import { Badge, Button, Card, ErrorState, Input, Meter, Segmented, Select, Skeleton, Toggle } from '@/components/ui/primitives';
import { SectionBar } from '@/components/SectionBar';
import { addressWindow, ANALYTICS_CODEC, ANALYTICS_RANGES, type AnalyticsAddress } from '@/lib/address';
import { CONVERSATION_CLASSES, GROUPED_SHARE_NOTE, MAX_BURN_LINES, NATIVE_7D_SUBSET, NATIVE_SUBSET, ONE_ACCOUNT_SHARE_NOTE, UNIT_SUBSET, addReadWrite, costHeadOf, dollarsOf, equivalentOrDash, foldBurn, isolatedSlots, moveLabel, nativeCellOf, niceAxisTop, oneAccountGroupOf, oneAccountShares, own, placementCells, placementRungLabel, planRowsRequired, readBurn, sampledWindows, shareLines, shareRow, sidesOf, sizedAccounts, slotUnpriced, weekColumnOf, weekKeyOf, weekLabelOf, windowLines, type BurnFigure, type BurnLine, type BurnLineUnit, type BurnRow, type CostFigure, type DollarPosition } from '@/lib/analytics';
import { bucketSizeFor } from '@/lib/buckets';
import { WASTED_PROBE_LABEL, WASTED_PROBE_REASONS, missCount, missLabel, missReasonOrder, missShort } from '@/lib/cache';
import { clock } from '@/lib/clock';
import { bandRuns } from '@/lib/costs';
import { ESTIMATED_TITLE, equivalentShort, fmtAbsolute, fmtCompact, fmtDayTick, fmtDuration, fmtHourTick, fmtInt, fmtMs, fmtPct, fmtPeriodEnd, fmtShare, fmtUsd, fmtUsdTick, pct, shareLabel, shortId, toLocalInput, upperFirst, usdTickDigits, withEstimate } from '@/lib/format';
import { BUCKET_COUNT, BUCKET_EDGES, PERCENTILE_HINT, histogramCount } from '@/lib/histogram';
import { useAsync, useCommittedText, useQueryFilters, type Loaded } from '@/lib/hooks';
import { linkedKeyIds, requestsLink, type RequestsLinkInput, type RequestsLinkTarget } from '@/lib/requestsLink';
import { UNIT_FUNDING, UNIT_POSITIONS, type UnitPosition } from '@/lib/unit';
import { WINDOW_LABEL } from '@/lib/windows';

type Range = '1h' | '24h' | '30d' | '7d' | 'custom';
/** The dimensions the breakdown route groups on; `all` is the ungrouped total every section reads. */
type GroupBy = Exclude<AnalyticsGroupBy, 'all'>;
/** The four dimensions both the analytics query and the request log filter on. */
type QueryDim = 'accountId' | 'keyId' | 'model' | 'poolId';
/** No route groups on a session, so it filters the page from the side rather than joining the group-by. */
type Dim = QueryDim | 'sessionId';

const HOUR = 3_600_000;
const GROUP_LABEL: Record<GroupBy, string> = { account: 'Account', client: 'Client', key: 'Key', effort: 'Effort', kind: 'Kind', model: 'Model', pool: 'Pool', request_class: 'Request class', speed: 'Speed', user_agent: 'User agent' };
/** The two speeds the rollups file an answer under, standard at the foot of each stacked bar. */
const SPEED_SERIES = [
  { key: 'standard', label: 'standard', slot: 0 },
  { key: 'fast', label: 'fast', slot: 4 },
] as const;
/** The group-by's eleventh option, which the Burn section alone honors: no rollup carries a session. */
const SESSION_OPTION = 'Session (Burn only)';
const SESSION_OPTION_HINT = 'The Burn section groups by session, and every other section groups by key, since no rollup carries a session';
const DIM_LABEL: Record<QueryDim, string> = { accountId: 'Account', keyId: 'Key', model: 'Model', poolId: 'Pool' };
/** The address key each filter dimension travels under. */
const DIM_KEY: Record<QueryDim, 'accounts' | 'keys' | 'models' | 'pools'> = { accountId: 'accounts', keyId: 'keys', model: 'models', poolId: 'pools' };
// Ordered the way a request travels them rather than alphabetically: the key names the pool, which picks the account.
const DIMS: QueryDim[] = ['keyId', 'poolId', 'accountId', 'model'];
/** Filter choices come from a breakdown on that dimension, so every value the range holds is offered. */
const DIM_GROUP: Record<QueryDim, GroupBy> = { accountId: 'account', keyId: 'key', model: 'model', poolId: 'pool' };
/** The log has no session filter, so a session view pages through it; both bounds together are the note's figure. */
const SESSION_PAGE_SIZE = 500;
const SESSION_PAGE_LIMIT = 20;
const SESSION_SOURCE_NOTE = 'From the request log, since rollups carry no session. Cache warming figures are unavailable per session';
const SESSION_COST_NOTE = 'Cost split is not available for a session view.';
// The rings' centers read bare, so the subtitle names the window the subscription ring's percents are of.
const NATIVE_SPLIT_NOTE = `By token kind, ${NATIVE_7D_SUBSET}`;
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

/** A dollar position's requests in the Cost split's, the cache cost card's and the Probe cost tile's shorter form. */
const UNIT_SPLIT: Record<DollarPosition, string> = { all: 'all requests', equivalent: "the subscription requests' equivalent", usd: 'billed' };
const PER_REQUEST_SHARE_NOTE = 'of the 7-day window per plan-funded request';
const PER_THOUSAND_SHARE_NOTE = 'of the 7-day window per 1k plan-funded output tokens';
/** A share drawn bare, where the header or the legend already names the window. */
const shareOrDash = (share: number | null | undefined) => (share === null || share === undefined ? '-' : fmtShare(share));

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

type NativeRatio = { estimated: boolean; figures: CostFigure[]; note: string };
/**
 * The native Cost per request and per 1k output tiles: the plan rows' 20x 7-day equivalent over the plan requests and
 * output tokens the route counts beside the billed dollars over the rest, a side with no rows left out as the first
 * tile leaves it, and a dash where a count is unknown or zero or the route priced the plan rows nothing on the window.
 */
function nativeRatiosOf(totals: AnalyticsGroup): { perRequest: NativeRatio; perThousand: NativeRatio } {
  const sides = sidesOf(totals, 'seven_day');
  const ratio = (count: number | null, whole: number, scale: number, word: string, note: (n: number) => string): NativeRatio => {
    const billedCount = count === null ? null : whole - count;
    const per = (v: number | null, n: number | null) => (v === null || n === null || !(n > 0) ? null : (v / n) * scale);
    const equivalent = sides.equivalent === undefined ? null : per(sides.equivalent, count);
    const estimated = totals.estimated && equivalent !== null;
    return {
      estimated,
      figures: [...(sides.equivalent === undefined ? [] : [{ text: equivalentOrDash('seven_day', equivalent, estimated), word: 'subscription' }]), ...(sides.billed === undefined ? [] : [{ text: usdOrDash(per(sides.billed, billedCount)), word: 'billed' }])],
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

/**
 * The native table's columns: Cost, the 20x 7-day equivalent with the billed dollars under it, then the other windows,
 * the week column named by the groups' equivalent keys as `weekColumnOf` names it for the shares, then the ratios.
 */
const nativeColumns = (groups: AnalyticsGroup[]): string[] => ['Requests', 'Cost', '20x 5-hour', `20x ${weekColumnOf(groups.map((g) => ({ costShares: g.costEquivalent })))}`, 'Per request', 'Per 1k output'];
/** A native cell's figure over its billed line, the full reading on its title. */
const nativeCell = (cell: { main: string; sub: string; title: string }): ReactNode => (
  <span title={cell.title || undefined}>
    {cell.main}
    {cell.sub && <span className="sub">{cell.sub}</span>}
  </span>
);
/**
 * A group's native row: its requests, its cost (the plan rows' 20x 7-day equivalent with the billed dollars under it),
 * its 5-hour and model week equivalents, and the 7-day equivalent per plan request and per 1k plan output tokens with
 * the billed dollars per billed request and per 1k billed output tokens under it, a side with no rows left out and a
 * figure with no count a dash. An estimated group marks every equivalent it shows and names the estimate on its label.
 */
function nativeRowOf(g: AnalyticsGroup): { cells: ReactNode[]; key: string; label: string; title: string } {
  const sides = sidesOf(g, 'seven_day');
  const priced = (key: string | undefined) => (sides.equivalent === undefined || key === undefined || !own(g.costEquivalent, key) ? null : g.costEquivalent[key]);
  const sevenDay = priced('seven_day');
  const per = (v: number | null, count: number | null, scale: number) => (v === null || count === null || !(count > 0) ? null : (v / count) * scale);
  // Undefined leaves a side with no rows out of the cell, as the tiles leave it out.
  const both = (equivalent: number | null, billed: number | null) => nativeCell(nativeCellOf('seven_day', sides.equivalent === undefined ? undefined : equivalent, g.estimated, sides.billed === undefined ? undefined : billed));
  const ratio = (planCount: number | null, whole: number, scale: number) => both(per(sevenDay, planCount, scale), per(sides.billed ?? null, planCount === null ? null : whole - planCount, scale));
  const windowCell = (key: string | undefined) => nativeCell(nativeCellOf(key ?? '', priced(key), g.estimated));
  const week = weekKeyOf(g.costEquivalent);
  return {
    cells: [
      fmtInt(g.requests),
      both(sevenDay, sides.billed ?? null),
      windowCell('five_hour'),
      windowCell(week),
      ratio(g.planRequests, g.requests, 1),
      ratio(g.planOutputTokens, g.tokens.output, 1000),
    ],
    key: g.key,
    label: g.label,
    title: withEstimate(g.label, g.estimated),
  };
}

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

export function AnalyticsPage() {
  const [now, setNow] = useState(() => clock.now());
  // Try again bumps it with the end, so every read keyed on the window runs again even while the clock stands still.
  const [generation, setGeneration] = useState(0);
  // Every filter lives in the address, so a link or a reload lands on the same view.
  const { filters: a, update } = useQueryFilters(ANALYTICS_CODEC);
  const range = a.range.preset as Range;
  // The custom inputs edit a draft of the address's pair, kept while it does not end after it starts, which the
  // address would read as the preset.
  const pair = `${a.range.from} ${a.range.to}`;
  const [draft, setDraft] = useState<{ for: string; from: string; to: string } | null>(null);
  const custom = draft !== null && draft.for === pair ? draft : { from: toLocalInput(Date.parse(a.range.from)), to: toLocalInput(Date.parse(a.range.to)) };
  // Every section but the Burn reads the default grouping under the Burn's own session grouping.
  const pageBy = a.by as BurnGroup;
  const groupBy: GroupBy = pageBy === 'session' ? 'key' : pageBy;
  const userAgentPattern = a.agent;
  const classPattern = a.class;
  const clientGroups = a.groups;
  const [groupsCut, setGroupsCut] = useState(false);
  const includeProbes = a.probes;
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
  // A session id is one session per key lifetime, so a drill-down from a session row names the lifetime too. A session
  // without one reads unscoped.
  const sessionKey = a.session && a.key ? a.key : null;
  const { setUnit, unit } = useAppSettings();
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
  const [sessionText, setSessionText] = useState(a.session);
  // The field follows the address, so a link or the bare Analytics entry replaces what it shows.
  const [sessionFor, setSessionFor] = useState(a.session);
  if (sessionFor !== a.session) {
    setSessionFor(a.session);
    setSessionText(a.session);
  }
  const ink = useChartInk();
  // A committed filter reads the window up to the instant it changed, as a range change does. The end and the address
  // change in one render, so each read goes out once: the hook writes the address at once, and a pattern commits from
  // a timer, where the end alone would otherwise render apart.
  const commit = (patch: Partial<AnalyticsAddress>, at = clock.now()) =>
    flushSync(() => {
      setNow(at);
      update(patch);
    });
  const commitClass = (next: string) => commit({ class: next });
  const commitAgent = (next: string) => commit({ agent: next });
  const commitGroups = (next: string) => commit({ groups: next });
  // Custom opens on the three days up to now, so the choice holds.
  const changeRange = (next: Range) => {
    if (next === range) return;
    const at = clock.now();
    commit({ range: next === 'custom' ? { from: iso(at - 3 * 24 * HOUR), preset: 'custom', to: iso(at) } : { from: '', preset: next, to: '' } }, at);
  };
  // An edit is the minute the operator typed. The other bound is the one an earlier edit left in the draft, else the
  // address's instant, kept to the millisecond.
  const changeBound = (bound: 'from' | 'to', local: string) => {
    const at = Date.parse(local);
    const kept = (side: 'from' | 'to') => (custom[side] === toLocalInput(Date.parse(a.range[side])) ? Date.parse(a.range[side]) : Date.parse(custom[side]));
    const next = bound === 'from' ? { from: at, to: kept('to') } : { from: kept('from'), to: at };
    if (Number.isNaN(next.from) || Number.isNaN(next.to) || next.to <= next.from) {
      setDraft(bound === 'from' ? { ...custom, for: pair, from: local } : { ...custom, for: pair, to: local });
      return;
    }
    setDraft(null);
    commit({ range: { from: iso(next.from), preset: 'custom', to: iso(next.to) } });
  };
  const changeGroupBy = (next: BurnGroup) => commit({ by: next });
  const changeFilter = (dim: QueryDim, next: string[]) => {
    const patch: Partial<AnalyticsAddress> = {};
    patch[DIM_KEY[dim]] = next;
    commit(patch);
  };
  const changeProbes = (on: boolean) => commit({ probes: on });
  // Enter inserts a line in the groups field, so it commits on the pause and on blur alone.
  const groupsText = useCommittedText(clientGroups, commitGroups);

  /**
   * The one writer for the session filter, so the field, the chip and the address cannot disagree. A key lifetime
   * belongs to the session it arrived with, so a new session drops it.
   */
  const selectSession = (id: string) => {
    setSessionText(id);
    if (id === a.session) return;
    commit({ key: '', session: id });
  };

  // The query bounds come from the address's instants, which keep the milliseconds the inputs' minutes drop.
  const span = useMemo(() => {
    const { from, to } = addressWindow(a.range, ANALYTICS_RANGES, '24h', now);
    return { fromMs: from, toMs: Math.min(to, now) };
  }, [a.range, now]);
  const draftInvalid = draft !== null && draft.for === pair;
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
    return readBurn(api, { from: fromIso, to: toIso }, pageBy, { accountIds: filters.accountId, userAgentGroups: agentGroups }).then((read) => ({ ...read, endMs: span.toMs, readAt }));
  }, [fromIso, toIso, pageBy, filters.accountId, agentGroups, tab, generation]);
  // The registry's list, for the account slots alone: the range's option list holds the accounts with traffic and
  // differs from the Dashboard's, and one account keeps one color across the views.
  const registry = useAsync(() => api.listAccounts(), [], { refreshMs: 30_000 });
  const slotOf = useMemo(() => accountSlots((registry.data ?? []).map((a) => a.id)), [registry.data]);
  // The plan chart's line category reads each account's stored tier against the cost model's capacity rows.
  const burnSettings = useAsync(() => (tab === 'burn' ? api.getSettings() : Promise.resolve(null)), [tab]);
  const sized = useMemo(() => (burn.data && burnSettings.data ? sizedAccounts(burn.data.accounts, burnSettings.data.subscriptionCosts) : null), [burn.data, burnSettings.data]);
  const rewrites = useAsync(() => (tab === 'traffic' ? api.getAnalyticsRewrites({ from: fromIso, to: toIso }) : Promise.resolve(null)), [fromIso, toIso, tab, generation]);
  // Side groupings on the page's own query, as byModel and byPool are; a session view folds them from the log.
  const bySpeed = useAsync(() => (sessionId === null ? api.getAnalyticsSeries({ ...query, groupBy: 'speed' }) : Promise.resolve<AnalyticsPoint[]>([])), [query, sessionId, generation]);
  const byEffort = useAsync(() => (tab === 'traffic' && sessionId === null ? api.getAnalyticsBreakdown({ ...query, groupBy: 'effort' }) : Promise.resolve<AnalyticsGroup[] | null>(sessionId === null ? null : [])), [query, sessionId, tab, generation]);
  // A dollar side narrows the three cost-bearing sections to the rows that side paid for, a filter the page's reads
  // cannot carry (Traffic, Latency and the Cache token charts read every row), so those sections read a second query
  // while one of them shows. A share sums plan-funded rows alone whatever the filter, so that position and the whole
  // read the page's own.
  const fundedTab = tab === 'cache' || tab === 'cache-warming' || tab === 'cost';
  const funded = sessionId === null && (unit === 'usd' || unit === 'equivalent');
  const costSeries = useAsync(() => (funded && fundedTab ? api.getAnalyticsSeries({ ...query, funding }) : Promise.resolve<AnalyticsPoint[] | null>(null)), [query, funding, funded, fundedTab, generation]);
  // The Cache warming card's Probe cost tile is the one reader of this breakdown on its tab, and it reads the probe
  // rows counted under every position, the whole and the shares included: a breakdown that does not count probes
  // drops a group left with probe rows alone, whether a dollar side dropped the group's real rows or the group is
  // probe-only in the whole (the probe kind, an idle account over a short range), and the three positions add up
  // only when all three keep it. Under the whole and the shares the read carries no funding.
  const warming = sessionId === null && tab === 'cache-warming';
  const costBreakdown = useAsync(
    () => ((funded && fundedTab) || warming ? api.getAnalyticsBreakdown({ ...query, funding: funded ? funding : undefined, includeProbes: query.includeProbes || warming }) : Promise.resolve<AnalyticsGroup[] | null>(null)),
    [query, funding, funded, fundedTab, warming, generation],
  );
  // Native pairs the whole read's plan probes with the billed probes' dollars, which no member of the whole carries, so
  // the Cache warming tab reads the billed rows once more, counting probes as the whole read does.
  const nativeWarming = warming && unit === 'native';
  const billedBreakdown = useAsync(() => (nativeWarming ? api.getAnalyticsBreakdown({ ...query, funding: 'billed', includeProbes: true }) : Promise.resolve<AnalyticsGroup[] | null>(null)), [query, nativeWarming, generation]);
  // A dollar side narrows Cache by model's two cost cells alone, its token columns reading every row, so the side's
  // rows by model come from a read of their own beside the page's.
  const byModelFunded = useAsync(() => (tab === 'cache' && funded ? api.getAnalyticsBreakdown({ ...query, funding, groupBy: 'model' }) : Promise.resolve<AnalyticsGroup[] | null>(null)), [query, funding, funded, tab, generation]);
  // The by-pool table's Probe cost cell reads a dollar side's probes, and under native the billed probes beside the
  // whole read's equivalent. Probes count, so a pool left with probe rows alone on that side keeps its figure.
  const poolFunding = nativeWarming ? 'billed' : warming && funded ? funding : undefined;
  const fundedByPool = useAsync(
    () => (poolFunding !== undefined ? api.getAnalyticsBreakdown({ ...query, funding: poolFunding, groupBy: 'pool', includeProbes: true }) : Promise.resolve<AnalyticsGroup[] | null>(null)),
    [query, poolFunding, generation],
  );
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
  // the whole and under shares, and in a session view the rows folded by who paid for them.
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
  const costHead = costHeadOf(unit, costTotals);
  const nativeRatios = nativeRatiosOf(costTotals);
  // A session fold has no cost split either, so the donut and the cache cost card read unavailable in a session view.
  const costSplit = knownSplit(costTotals.cost);
  // The routes answer a named account's own-window shares on every group under any grouping, and under the account
  // grouping each account's on its own group. So the per-group surfaces read shares under either, and the surfaces that
  // read one account's whole sum the named account's groups, whose shares add since they share its windows.
  const namedAccount = filters.accountId.length === 1 ? filters.accountId[0] : null;
  const sharedGroups = groupBy === 'account' || namedAccount !== null;
  const oneGroup = useMemo(() => (namedAccount === null ? null : oneAccountGroupOf(costTotals, costGroups)), [costGroups, costTotals, namedAccount]);
  const cacheShareRows = useMemo(() => (unit === 'share' && namedAccount !== null ? cacheShareRowsOf(costPoints) : []), [unit, namedAccount, costPoints]);
  // The cache chart's week rows are keyed off the one account's map, so its legend names that map's week.
  const cacheWeek = weekLabelOf(oneGroup === null || oneGroup.cacheShares === null ? undefined : weekKeyOf(oneGroup.cacheShares));
  const otherFilters = filters.keyId.length > 0 || filters.poolId.length > 0 || filters.model.length > 0 || userAgentPattern.trim() !== '' || classPattern.trim() !== '' || sessionId !== null || includeProbes;
  // The rewrite and fit routes read the range alone, so every filter of the bar passes them by. The burn route reads
  // the Accounts filter and the user agent groups beside it.
  const unfiltered = otherFilters || filters.accountId.length > 0;
  // The plan cards follow the unit: the shares under Window shares, where the route answers them for the account
  // grouping or one named account, and the 20x equivalents, a subscription request's native unit, under every other.
  const burnFigure: BurnFigure = unit === 'share' ? 'share' : 'equivalent';
  const burnAccount = filters.accountId.length === 1 ? filters.accountId[0] : null;
  const burnSizes = burnFigure === 'share' && (pageBy === 'account' || burnAccount !== null);

  const groupSlot = useMemo(() => {
    // Slots follow the range's whole key list so a filter never repaints the survivors, and an account's slot is the
    // registry's. A dimension with no option list of its own falls back to the keys the breakdown answered.
    const listed = groupBy === 'account' ? options.accountId : groupBy === 'key' ? options.keyId : groupBy === 'model' ? options.model : groupBy === 'pool' ? options.poolId : [];
    const keys = listed.length ? listed.map((o) => o.value) : groups.map((g) => g.key);
    return (key: string) => (groupBy === 'account' ? accountColor(ink, slotOf(key)) : ink.series[Math.min(7, keys.indexOf(key) < 0 ? 7 : keys.indexOf(key))]);
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
  const billedTotals = useMemo(() => (nativeWarming && !billedBreakdown.stale && billedBreakdown.data !== null ? sumGroups(billedBreakdown.data) : null), [nativeWarming, billedBreakdown.stale, billedBreakdown.data]);
  const shareTiles = shareTilesOf(groupBy, namedAccount, oneGroup, costLoading);
  const probeTile = probeTileOf(unit, namedAccount, oneGroup, costTotals, billedTotals, costLoading, sessionId !== null);
  const fundedModels = useMemo(() => new Map((byModelFunded.stale ? [] : (byModelFunded.data ?? [])).map((g) => [g.key, g])), [byModelFunded.stale, byModelFunded.data]);
  const fundedPools = useMemo(() => new Map((fundedByPool.stale ? [] : (fundedByPool.data ?? [])).map((g) => [g.key, g])), [fundedByPool.stale, fundedByPool.data]);
  // Native draws the cache card's bars in the plan rows' equivalent with the billed dollars in its subtitle, and a
  // range with no plan row draws the billed rows' dollars, its one side.
  const cacheSides = sidesOf(costTotals, 'seven_day');
  const cacheEquivalent = unit === 'native' && cacheSides.equivalent !== undefined;
  const cacheDollars = unit === 'native' ? 'billed' : 'cost';
  const cacheSplit = unit === 'native' ? knownSplit(costTotals.billed) : costSplit;
  // Absent on a fold and wherever no plan row priced, so the legend reads a dash there.
  const cacheSevenDay = costTotals.cacheEquivalent.seven_day as { read: number; write: number } | undefined;
  const cacheMarked = costTotals.estimated && cacheSevenDay !== undefined;
  // Usage credits alone fund a fast answer, on a subscription account as on an API key.
  const fastSpend = speedPoints.reduce((n, p) => n + (p.groups.find((g) => g.key === 'fast')?.costUsd ?? 0), 0);
  const groupLabel = GROUP_LABEL[groupBy];
  // The reasons panel's counts link under one window and the page's filters. A session view's log read keeps the
  // session's key lifetime alone, so its links carry that lifetime.
  const linkInput: ReasonLink = {
    filters: { accountIds: filters.accountId, clientPattern: classPattern.trim(), includeProbes, keyIds: linkedKeyIds(filters.keyId, sessionId === null ? null : sessionKey), models: filters.model, poolIds: filters.poolId, sessionId, userAgentPattern: userAgentPattern.trim() },
    window: { exact: sessionId !== null, fromMs: span.fromMs, nowMs: now, toMs: span.toMs },
  };

  return (
    <div className="stack">
      <div className="filters filters--sticky" ref={filtersRef}>
        <Segmented<Range>
          ariaLabel="Range"
          title={`Window ends ${fmtAbsolute(toIso)}`}
          value={range}
          onChange={changeRange}
          options={[...ANALYTICS_RANGES.map((r) => ({ value: r.name, label: r.label })), { value: 'custom', label: 'Custom' }]}
        />
        {range === 'custom' && (
          <span className="filters__range">
            <Input type="datetime-local" value={custom.from} max={custom.to} onChange={(e) => changeBound('from', e.target.value)} aria-label="From" />
            <span className="muted">to</span>
            <Input type="datetime-local" value={custom.to} min={custom.from} max={toLocalInput(now)} onChange={(e) => changeBound('to', e.target.value)} aria-label="To" />
            {draftInvalid && <span className="pairing__expired">End must be after start.</span>}
          </span>
        )}
        {!log.stale && log.data?.partial && <span className="secondary">Partial: the first {fmtInt(SESSION_PAGE_LIMIT * SESSION_PAGE_SIZE)} requests of the range</span>}
        <span className="filters__sep" aria-hidden />
        <label className="row">
          <span className="muted">Group by</span>
          <Select value={pageBy} onChange={(e) => changeGroupBy(e.target.value as BurnGroup)} className="groupby-select">
            {(Object.keys(GROUP_LABEL) as GroupBy[]).map((g) => (
              <option key={g} value={g}>
                {GROUP_LABEL[g]}
              </option>
            ))}
            <option value="session" title={SESSION_OPTION_HINT}>
              {SESSION_OPTION}
            </option>
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
              title="Each pattern names a group, and every other user agent groups under other"
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
        <Section id="burn" shown={shown} title="Burn" meta={`Tokens, spend and plan share over the range${otherFilters ? BURN_UNFILTERED_NOTE : ''}`}>
          {/* Only the shares size their lines by tier, so the equivalents neither wait on nor fail with the settings
              read. */}
          <BurnCharts
            account={burnAccount}
            answer={burn.stale ? null : (burn.data?.answer ?? null)}
            costs={burnSettings.data?.subscriptionCosts ?? null}
            error={burn.error ?? (burnSizes ? burnSettings.error : null)}
            figure={burnFigure}
            groupBy={pageBy}
            loading={burn.data === null || burn.stale || (burnSizes && sized === null)}
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
              subtitle={
                unit === 'share'
                  ? "Shares of the account's 7-day window per bucket, writing the cache and reading it back"
                  : cacheEquivalent
                    ? `20x 7-day equivalent per bucket spent writing the cache and reading it back by the subscription requests${cacheSides.billed === undefined ? '' : `, the billed requests ${usdOrDash(costTotals.billed.cacheWrite)} writing and ${usdOrDash(costTotals.billed.cacheRead)} reading`}`
                    : `USD per bucket spent writing the cache and reading it back, ${UNIT_SPLIT[unit === 'native' ? 'usd' : dollars]}`
              }
              loading={costLoading}
              error={costError}
              empty={unit === 'share' && namedAccount === null ? ONE_ACCOUNT_SHARE_NOTE : undefined}
              legend={
                unit === 'share'
                  ? [
                      { color: ink.series[3], label: 'Write share', value: cacheShareOf(oneGroup, 'write') },
                      { color: ink.series[2], label: 'Read share', value: cacheShareOf(oneGroup, 'read') },
                    ]
                  : cacheEquivalent
                    ? [
                        { color: ink.series[3], label: 'Write cost', title: cacheMarked ? ESTIMATED_TITLE : undefined, value: equivalentOrDash('seven_day', cacheSevenDay?.write, costTotals.estimated) },
                        { color: ink.series[2], label: 'Read cost', title: cacheMarked ? ESTIMATED_TITLE : undefined, value: equivalentOrDash('seven_day', cacheSevenDay?.read, costTotals.estimated) },
                      ]
                    : [
                        { color: ink.series[3], label: 'Write cost', value: usdOrDash(costTotals[cacheDollars].cacheWrite) },
                        { color: ink.series[2], label: 'Read cost', value: usdOrDash(costTotals[cacheDollars].cacheRead) },
                      ]
              }
            >
              {unit === 'share' ? (
                <BarChart data={cacheShareRows} margin={{ top: 8, right: 8, bottom: 0, left: 0 }} barCategoryGap={4} barGap={2}>
                  <CartesianGrid vertical={false} stroke={ink.grid} />
                  <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
                  <YAxis tickFormatter={(v: number) => fmtShare(v)} tickLine={false} axisLine={false} width={60} />
                  <Tooltip
                    cursor={{ fill: ink.grid, opacity: 0.6 }}
                    content={timeTip<CacheShareRow>((p) => [
                      { color: ink.series[3], label: 'write (5-hour)', value: shareOrDash(p.write.fiveHour) },
                      { color: ink.series[3], label: 'write (7-day)', value: shareOrDash(p.write.sevenDay) },
                      { color: ink.series[3], label: `write (${cacheWeek})`, value: shareOrDash(p.write.week) },
                      { color: ink.series[2], label: 'read (5-hour)', value: shareOrDash(p.read.fiveHour) },
                      { color: ink.series[2], label: 'read (7-day)', value: shareOrDash(p.read.sevenDay) },
                      { color: ink.series[2], label: `read (${cacheWeek})`, value: shareOrDash(p.read.week) },
                    ])}
                  />
                  <Bar dataKey="write.sevenDay" fill={ink.series[3]} maxBarSize={12} radius={[3, 3, 0, 0]} isAnimationActive={false} />
                  <Bar dataKey="read.sevenDay" fill={ink.series[2]} maxBarSize={12} radius={[3, 3, 0, 0]} isAnimationActive={false} />
                </BarChart>
              ) : cacheEquivalent ? (
                <BarChart data={costBuckets} margin={{ top: 8, right: 8, bottom: 0, left: 0 }} barCategoryGap={4} barGap={2}>
                  <CartesianGrid vertical={false} stroke={ink.grid} />
                  <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
                  <YAxis tickFormatter={(v: number) => fmtShare(v)} tickLine={false} axisLine={false} width={60} />
                  <Tooltip
                    cursor={{ fill: ink.grid, opacity: 0.6 }}
                    content={timeTip<Bucket>((p) => [
                      { color: ink.series[3], label: 'write cost', value: equivalentOrDash('seven_day', p.cacheEquivalent.seven_day?.write, p.estimated) },
                      { color: ink.series[2], label: 'read cost', value: equivalentOrDash('seven_day', p.cacheEquivalent.seven_day?.read, p.estimated) },
                    ])}
                  />
                  <Bar dataKey="cacheEquivalent.seven_day.write" fill={ink.series[3]} maxBarSize={12} radius={[3, 3, 0, 0]} isAnimationActive={false} />
                  <Bar dataKey="cacheEquivalent.seven_day.read" fill={ink.series[2]} maxBarSize={12} radius={[3, 3, 0, 0]} isAnimationActive={false} />
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
              {loading ? <Skeleton height={190} /> : <ReasonBars link={linkInput} totals={totals} />}
            </Card>
          </div>
          <MissReasonsByGroup groupBy={groupBy} groups={groups} groupLabel={groupLabel} link={linkInput} loading={loading} />
          {sessionId === null && (
            <CacheByModel
              groups={byModel.stale ? [] : (byModel.data ?? [])}
              funded={fundedModels}
              error={byModel.error ?? byModelFunded.error}
              loading={byModel.data === null || byModel.stale || (funded && (byModelFunded.data === null || byModelFunded.stale))}
              namedAccount={namedAccount}
              unit={unit}
            />
          )}
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
        <Section id="cost" shown={shown} title="Cost" meta={costLoading || costError !== null ? undefined : costHead.meta} metaTitle={costHead.estimated ? ESTIMATED_TITLE : undefined}>
          <div className="tiles-row">
            {unit === 'share' ? (
              <>
                <Tile label="Window share" value={shareTiles.sevenDay.value} note={shareTiles.sevenDay.note} />
                <Tile label="Per request" value={shareTiles.perRequest.value} note={shareTiles.perRequest.note} />
                <Tile label="Per 1k output" value={shareTiles.perThousand.value} note={shareTiles.perThousand.note} />
              </>
            ) : (
              <>
                <Tile label={costHead.tile?.label ?? ''} value={costLoading ? null : figuresValue(costHead.tile?.figures ?? [])} note={costHead.tile?.note} title={costHead.estimated ? ESTIMATED_TITLE : undefined} error={costError} />
                {unit === 'native' ? (
                  <>
                    <Tile label="Cost per request" value={costLoading ? null : figuresValue(nativeRatios.perRequest.figures)} note={nativeRatios.perRequest.note} title={nativeRatios.perRequest.estimated ? ESTIMATED_TITLE : undefined} error={costError} />
                    <Tile label="Cost per 1k output tokens" value={costLoading ? null : figuresValue(nativeRatios.perThousand.figures)} note={nativeRatios.perThousand.note} title={nativeRatios.perThousand.estimated ? ESTIMATED_TITLE : undefined} error={costError} />
                  </>
                ) : (
                  <>
                    <Tile label="Cost per request" value={costLoading ? null : fmtUsd(costTotals.requests ? costTotals.costUsd / costTotals.requests : 0)} note={`${fmtInt(costTotals.requests)} requests`} error={costError} />
                    <Tile label="Cost per 1k output tokens" value={costLoading ? null : fmtUsd(costTotals.tokens.output ? (costTotals.costUsd / costTotals.tokens.output) * 1000 : 0)} note={`${fmtCompact(costTotals.tokens.output)} output tokens`} error={costError} />
                  </>
                )}
              </>
            )}
            <Tile label="Fast mode spend" value={speedError !== null ? '-' : speedLoading ? null : fmtUsd(fastSpend)} note="usage credits, at the fast cells" />
          </div>
          <div className="grid-2">
            <CostByGroup points={costPoints} groupBy={groupBy} groupSlot={groupSlot} loading={costLoading} error={costError} axis={axis} shared={sharedGroups} unit={unit} />
            {/* Native draws a ring per funding, each in its own unit. Under the shares the split reads the whole in
                dollars, as under All in dollars. */}
            <Card title="Cost split" subtitle={unit === 'native' ? NATIVE_SPLIT_NOTE : `By token kind, in dollars at the API price list, ${UNIT_SPLIT[dollars]}`}>
              {/* The request summary carries one total and no split, so a session view has nothing to draw here. */}
              {sessionId !== null ? (
                <p className="muted">{SESSION_COST_NOTE}</p>
              ) : costError !== null ? (
                <ErrorState title="Could not load cost split" body={costError} />
              ) : costLoading ? (
                <Skeleton height={148} />
              ) : unit === 'native' ? (
                <NativeSplit totals={costTotals} />
              ) : costSplit === null ? (
                <Skeleton height={148} />
              ) : (
                <CostDonut cost={costSplit} />
              )}
            </Card>
          </div>
          {sessionId === null && unit === 'share' && !sharedGroups && (
            <Card title={`Cost by ${groupLabel.toLowerCase()}`} subtitle="Shares of each account's own windows">
              <p className="muted">{GROUPED_SHARE_NOTE}</p>
            </Card>
          )}
          {sessionId === null && unit === 'share' && sharedGroups && (
            <BreakdownTable
              title={`Cost by ${groupLabel.toLowerCase()}`}
              subtitle={groupBy === 'account' ? "Shares of each account's own windows for the range, over its plan-funded requests" : "Shares of the named account's own windows for the range, over its plan-funded requests"}
              loading={costLoading}
              groupLabel={groupLabel}
              columns={['Plan requests', '5-hour', '7-day', weekColumnOf(costGroups), 'Per request', 'Per 1k output']}
              rows={costGroups.map((g) => {
                const r = shareRow(g);
                return { key: g.key, label: g.label, cells: [g.planRequests === null ? '-' : fmtInt(g.planRequests), r.fiveHour ?? '-', r.sevenDay ?? '-', r.week ?? '-', r.perRequest ?? '-', r.perThousand ?? '-'] };
              })}
            />
          )}
          {sessionId === null && unit === 'native' && (
            <BreakdownTable
              className="native-cost-table"
              title={`Cost by ${groupLabel.toLowerCase()}`}
              subtitle={`Totals for the selected range and filters, ${NATIVE_SUBSET}`}
              error={costError}
              loading={costLoading}
              groupLabel={groupLabel}
              columns={nativeColumns(costGroups)}
              rows={costGroups.map(nativeRowOf)}
            />
          )}
          {sessionId === null && unit !== 'share' && unit !== 'native' && (
            <BreakdownTable
              title={`Cost by ${groupLabel.toLowerCase()}`}
              subtitle={`Totals for the selected range and filters, ${costHead.subset}`}
              error={costError}
              loading={costLoading}
              groupLabel={groupLabel}
              columns={['Requests', 'Cost', 'Input', 'Cache read', 'Cache write', 'Output', 'Per request', 'Per 1k output']}
              rows={costGroups.map((g) => ({
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
            <Tile label="Probe cost" value={probeTile.value} note={probeTile.note} title={probeTile.title} error={costError} />
            <Tile label="Kept warm" value={loading ? null : keptWarm === null ? '-' : fmtInt(keptWarm.requests)} note={keptWarm === null ? UNAVAILABLE_NOTE : `${fmtCompact(keptWarm.tokens)} tokens read warm`} />
          </div>
          <div className="grid-2">
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
              {costError !== null ? (
                <ErrorState title="Could not load wasted probes" body={costError} />
              ) : costLoading ? (
                <Skeleton height={190} />
              ) : probesUnmeasured ? (
                <p className="muted">{WARMING_UNAVAILABLE}</p>
              ) : (
                <WastedProbes group={costTotals} probes={probesOf(costTotals)} unit={unit} billed={billedTotals} account={namedAccount === null ? null : { group: oneGroup }} />
              )}
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
          <KeepaliveByPool
            groups={poolGroups}
            funded={fundedPools}
            error={sessionId === null ? (byPool.error ?? fundedByPool.error) : null}
            loading={sessionId === null ? byPool.data === null || byPool.stale || (poolFunding !== undefined && (fundedByPool.data === null || fundedByPool.stale)) : loading}
            unit={unit}
          />
        </Section>
      )}

      {tab === 'accounts' && (
        <Section id="accounts" shown={shown} title="Accounts" meta="Peak utilization, window usage, pace, ineligible time, and pin churn per account">
          <AccountsSection
            accountIds={filters.accountId}
            bucketMs={bucketMs}
            error={accountRollups.error}
            loading={accountRollups.data === null || accountRollups.stale}
            otherFilters={otherFilters}
            rollups={accountRollups.stale ? null : accountRollups.data}
            slotOf={slotOf}
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

type ShareTile = { note: string | undefined; value: string | null };
/**
 * The three Cost tiles under "Window shares": the named account's 7-day share with its other windows in the note, and
 * the 7-day share per plan-funded request and per 1k plan-funded output tokens, read under any grouping. With none or
 * several accounts named the tiles read "per account below" under the account grouping, since shares of different
 * accounts' windows do not add, and under another grouping a dash under the note that names the filter.
 */
function shareTilesOf(groupBy: GroupBy, namedAccount: string | null, group: AnalyticsGroup | null, loading: boolean): { perRequest: ShareTile; perThousand: ShareTile; sevenDay: ShareTile } {
  if (namedAccount === null) {
    const tile: ShareTile = groupBy === 'account' ? { note: undefined, value: loading ? null : 'per account below' } : { note: ONE_ACCOUNT_SHARE_NOTE, value: loading ? null : '-' };
    return { perRequest: tile, perThousand: tile, sevenDay: tile };
  }
  const shares = group?.costShares ?? null;
  const row = group === null ? null : shareRow(group);
  const named = (key: string | undefined) => (shares === null || key === undefined || !own(shares, key) || shares[key] === null ? null : shareLabel(key, shares[key] as number));
  const sevenDay = named('seven_day');
  const others = [named('five_hour'), named(shares === null ? undefined : weekKeyOf(shares))].filter((s): s is string => s !== null);
  return {
    perRequest: { note: PER_REQUEST_SHARE_NOTE, value: loading ? null : (row?.perRequest ?? '-') },
    perThousand: { note: PER_THOUSAND_SHARE_NOTE, value: loading ? null : (row?.perThousand ?? '-') },
    sevenDay: { note: others.length ? others.join(', ') : undefined, value: loading ? null : (sevenDay ?? '-') },
  };
}

/**
 * A probe figure's two sides under native: the plan probes' 20x 7-day equivalent off the whole read and the billed
 * probes' dollars off the billed read, told apart by their probe counts. `undefined` leaves out a side with no probe,
 * null reads a dash where the plan probes priced nothing on the window, and a group with no probe on either side reads
 * its zero in each unit, as an empty range does, with no mark, since no figure of it is an estimate.
 */
function probeSidesOf(whole: AnalyticsGroup, billed: AnalyticsGroup | null | undefined, read: 'cost' | 'wasted'): { billed: number | undefined; equivalent: number | null | undefined; estimated: boolean } {
  const billedProbes = billed ? probesOf(billed) : 0;
  const planProbes = probesOf(whole) - billedProbes;
  const priced = own(whole.probeEquivalent, 'seven_day') ? whole.probeEquivalent.seven_day[read] : null;
  const dollars = billed ? (read === 'cost' ? billed.probeCostUsd : billed.wastedProbeUsd) : 0;
  if (planProbes <= 0 && billedProbes <= 0) return { billed: dollars, equivalent: priced ?? 0, estimated: false };
  return { billed: billedProbes > 0 ? dollars : undefined, equivalent: planProbes > 0 ? priced : undefined, estimated: whole.estimated && planProbes > 0 && priced !== null };
}
const probeFiguresOf = (sides: ReturnType<typeof probeSidesOf>): CostFigure[] => [
  ...(sides.equivalent === undefined ? [] : [{ text: equivalentOrDash('seven_day', sides.equivalent, sides.estimated), word: 'subscription' }]),
  ...(sides.billed === undefined ? [] : [{ text: fmtUsd(sides.billed), word: 'billed' }]),
];

/**
 * The Probe cost tile per position: under "Window shares" the named account's probe share of its 7-day window with
 * the other windows and the probe count in the note, a dash with the count where none is priced, and the note alone
 * with none or several accounts named. Under native the plan probes' 20x 7-day equivalent of the whole read stacked
 * over the billed probes' dollars of `billed`, and under a dollar position the position's probe dollars. A session view
 * folds request rows, which measure no probe count, so `session` reads the count unmeasured on every path even where
 * the fold holds no group at all (the named account's rows all billed, or a position that keeps no row).
 */
function probeTileOf(unit: UnitPosition, namedAccount: string | null, group: AnalyticsGroup | null, totals: AnalyticsGroup, billed: AnalyticsGroup | null, loading: boolean, session: boolean): { note: string; title?: string; value: ReactNode | null } {
  if (unit === 'share') {
    if (namedAccount === null) return { note: ONE_ACCOUNT_SHARE_NOTE, value: loading ? null : '-' };
    const probes = session || group?.fromRequests ? UNAVAILABLE_NOTE : `${fmtInt(group === null ? 0 : probesOf(group))} probes`;
    const shares = group?.probeShares ?? null;
    const named = (key: string | undefined) => (shares === null || key === undefined || !own(shares, key) ? null : shareLabel(key, shares[key].cost));
    const sevenDay = named('seven_day');
    const others = [named('five_hour'), named(shares === null ? undefined : weekKeyOf(shares))].filter((s): s is string => s !== null);
    return { note: [...others, probes].join(', '), value: loading ? null : (sevenDay ?? '-') };
  }
  const unmeasured = session || totals.fromRequests === true;
  if (unmeasured) return { note: UNAVAILABLE_NOTE, value: loading ? null : '-' };
  if (unit === 'native') {
    const sides = probeSidesOf(totals, billed, 'cost');
    return { note: `${fmtInt(probesOf(totals))} probes, ${NATIVE_7D_SUBSET}`, title: sides.estimated ? ESTIMATED_TITLE : undefined, value: loading ? null : figuresValue(probeFiguresOf(sides)) };
  }
  return { note: `${fmtInt(probesOf(totals))} probes, ${UNIT_SPLIT[dollarsOf(unit)]}`, value: loading ? null : fmtUsd(totals.probeCostUsd) };
}

/** The named account's cache share of its 7-day window on one side, read off its group, a dash where none is priced. */
const cacheShareOf = (group: AnalyticsGroup | null, side: 'read' | 'write'): string => {
  const shares = group?.cacheShares ?? null;
  return shares === null || !own(shares, 'seven_day') ? '-' : shareLabel('seven_day', shares.seven_day[side]);
};

type WindowShares = Record<'fiveHour' | 'sevenDay' | 'week', number | null>;
interface CacheShareRow {
  at: number;
  read: WindowShares;
  write: WindowShares;
}

/** The named account's cache read and write shares per bucket, summed over its groups, each window null where the
 *  bucket holds none. */
function cacheShareRowsOf(points: AnalyticsPoint[]): CacheShareRow[] {
  return points.map((p) => {
    const shares = oneAccountShares(p.groups, (g) => g.cacheShares, addReadWrite, planRowsRequired);
    const week = shares === null ? undefined : weekKeyOf(shares);
    const side = (of: 'read' | 'write'): WindowShares => {
      const at = (key: string | undefined) => (shares === null || key === undefined || !own(shares, key) ? null : shares[key][of]);
      return { fiveHour: at('five_hour'), sevenDay: at('seven_day'), week: at(week) };
    };
    return { at: Date.parse(p.at), read: side('read'), write: side('write') };
  });
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
}) {
  return (
    <Card title={title} subtitle={subtitle} actions={actions}>
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

/** What every count of the reasons panel links under: the window and the page's filters, without a count or a reason. */
type ReasonLink = Omit<RequestsLinkInput, 'count' | 'group' | 'reason'>;

/** A count of the reasons panel: a link to the requests it counts, else the figure with why it stays plain. */
function CountLink({ children, target }: { children: ReactNode; target: RequestsLinkTarget }) {
  return target.href === null ? (
    <span title={target.title}>{children}</span>
  ) : (
    <Link to={target.href} title={target.title}>
      {children}
    </Link>
  );
}

function ReasonBars({ link, totals }: { link: ReasonLink; totals: AnalyticsGroup }) {
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
            <CountLink target={requestsLink({ ...link, count: r.count, reason: r.reason })}>{fmtInt(r.count)}</CountLink>{' '}
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
  share: "In the one account's window shares.",
  usd: 'In dollars at list prices.',
};

type ProbeTile = { note?: string; title?: string; value: ReactNode };
/**
 * The Wasted cost and Share of probe cost tiles per position. Native stacks the wasted plan probes' 20x 7-day
 * equivalent over the wasted billed probes' dollars and reads the share off the plan side, the billed side's where no
 * plan probe ran. Under "Window shares" the named account's probe shares read, as the Probe cost tile reads them, and
 * with none named a dash under the note that names the filter. A dollar position reads its own rows' dollars.
 */
function wastedTilesOf(unit: UnitPosition, group: AnalyticsGroup, billed: AnalyticsGroup | null, account: { group: AnalyticsGroup | null } | null): { cost: ProbeTile; share: ProbeTile } {
  if (unit === 'native') {
    const wasted = probeSidesOf(group, billed, 'wasted');
    const cost = probeSidesOf(group, billed, 'cost');
    const ratio = (w: number | null | undefined, c: number | null | undefined) => (w === null || w === undefined || c === null || c === undefined ? null : c ? w / c : 0);
    const share = cost.equivalent === undefined ? ratio(wasted.billed, cost.billed) : ratio(wasted.equivalent, cost.equivalent);
    const of = probeFiguresOf(cost).map((f) => f.text);
    return {
      cost: { note: NATIVE_7D_SUBSET, title: wasted.estimated ? ESTIMATED_TITLE : undefined, value: figuresValue(probeFiguresOf(wasted)) },
      share: { note: `of ${of.join(' and ')} on probes`, value: share === null ? '-' : fmtPct(share, 1) },
    };
  }
  if (unit === 'share') {
    if (account === null) return { cost: { note: ONE_ACCOUNT_SHARE_NOTE, value: '-' }, share: { value: '-' } };
    const shares = account.group?.probeShares ?? null;
    const named = (key: string | undefined) => (shares === null || key === undefined || !own(shares, key) || shares[key].wasted === null ? null : shareLabel(key, shares[key].wasted));
    const sevenDay = shares !== null && own(shares, 'seven_day') ? shares.seven_day : null;
    const others = [named('five_hour'), named(shares === null ? undefined : weekKeyOf(shares))].filter((s): s is string => s !== null);
    return {
      cost: { note: others.length ? others.join(', ') : undefined, value: named('seven_day') ?? '-' },
      share: sevenDay === null || sevenDay.wasted === null ? { value: '-' } : { note: `of ${shareLabel('seven_day', sevenDay.cost)} on probes`, value: fmtPct(sevenDay.cost ? sevenDay.wasted / sevenDay.cost : 0, 1) },
    };
  }
  return {
    cost: { note: unit === 'all' ? 'at list prices' : `at list prices, ${UNIT_SPLIT[unit]}`, value: fmtUsd(group.wastedProbeUsd) },
    share: { note: `of ${fmtUsd(group.probeCostUsd)} on probes`, value: fmtPct(group.probeCostUsd ? group.wastedProbeUsd / group.probeCostUsd : 0, 1) },
  };
}

/**
 * `group` is the position's rows (the whole under native and the shares), `billed` native's billed read, and `account`
 * the one account the shares read, null with none named.
 */
function WastedProbes({ account, billed, group, probes, unit }: { account: { group: AnalyticsGroup | null } | null; billed: AnalyticsGroup | null; group: AnalyticsGroup; probes: number; unit: UnitPosition }) {
  const ink = useChartInk();
  const rows = WASTED_PROBE_REASONS.map((reason) => ({ count: group.wastedProbeReasons[reason], reason })).sort((a, b) => b.count - a.count);
  const max = Math.max(1, ...rows.map((r) => r.count));
  if (group.wastedProbes === 0) return <p className="muted">No wasted probes in range.</p>;
  const tiles = wastedTilesOf(unit, group, billed, account);
  return (
    <>
      <div className="tiles-row tiles-row--panel">
        <Tile label="Wasted probes" value={fmtInt(group.wastedProbes)} note={`of ${fmtInt(probes)} probes`} />
        <Tile label="Wasted cost" value={tiles.cost.value} note={tiles.cost.note} title={tiles.cost.title} />
        <Tile label="Share of probe cost" value={tiles.share.value} note={tiles.share.note} />
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
              {fmtInt(r.count)} <span className="muted">{fmtPct(group.wastedProbes ? r.count / group.wastedProbes : 0, 1)}</span>
            </span>
          </li>
        ))}
      </ul>
    </>
  );
}

/** A raw reason name with a break opportunity after each underscore, so a header holding one wraps at its words. */
const breakable = (name: string): ReactNode => name.split(/(?<=_)/).flatMap((part, i) => (i === 0 ? [part] : [<wbr key={i} />, part]));

function MissReasonsByGroup({ groupBy, groups, groupLabel, link, loading }: { groupBy: GroupBy; groupLabel: string; groups: AnalyticsGroup[]; link: ReasonLink; loading: boolean }) {
  const order = missReasonOrder(groups);
  const cell = (g: AnalyticsGroup, reason: CacheMissReason) => {
    const count = missCount(g.cacheMissReasons, reason);
    return <CountLink target={requestsLink({ ...link, count, group: { by: groupBy, key: g.key }, reason })}>{fmtInt(count)}</CountLink>;
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
        label: g.label,
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
 * position's side. Native pairs each model's plan rows' 20x 7-day equivalent with its billed rows' dollars in each
 * cell, a dollar side reads its own read by model (`funded`, by key), and the shares read each model's 7-day share
 * where the route answers it, which it does while `namedAccount`, the Accounts filter's one entry, is set.
 */
function CacheByModel({ error, funded, groups, loading, namedAccount, unit }: { error: string | null; funded: Map<string, AnalyticsGroup>; groups: AnalyticsGroup[]; loading: boolean; namedAccount: string | null; unit: UnitPosition }) {
  const rows = useMemo(() => [...groups].sort((a, b) => b.tokens.cacheRead - a.tokens.cacheRead), [groups]);
  const subtitle =
    unit === 'native'
      ? `Totals for the selected range and filters, ${NATIVE_7D_SUBSET}`
      : unit === 'share'
        ? namedAccount === null
          ? ONE_ACCOUNT_SHARE_NOTE
          : "Totals for the selected range and filters, one account's 7-day window shares"
        : `Totals for the selected range and filters, ${UNIT_SUBSET[unit]}`;
  const costsOf = (g: AnalyticsGroup): { cells: ReactNode[]; estimated: boolean } => {
    if (unit === 'native') {
      const sides = sidesOf(g, 'seven_day');
      // Absent where no plan row priced, which reads a dash on the plan side.
      const priced = g.cacheEquivalent.seven_day as { read: number; write: number } | undefined;
      const cell = (side: 'read' | 'write', billed: number | null) => nativeCell(nativeCellOf('seven_day', sides.equivalent === undefined ? undefined : (priced?.[side] ?? null), g.estimated, sides.billed === undefined ? undefined : billed));
      return { cells: [cell('read', g.billed.cacheRead), cell('write', g.billed.cacheWrite)], estimated: g.estimated && sides.equivalent !== undefined && priced !== undefined };
    }
    if (unit === 'share') return { cells: [cacheShareOf(g, 'read'), cacheShareOf(g, 'write')], estimated: false };
    // A model the side has no row of spent nothing on that side.
    const side = unit === 'all' ? g : (funded.get(g.key) ?? null);
    return { cells: side === null ? [fmtUsd(0), fmtUsd(0)] : [usdOrDash(side.cost.cacheRead), usdOrDash(side.cost.cacheWrite)], estimated: false };
  };
  return (
    <BreakdownTable
      title="Cache by model"
      subtitle={subtitle}
      error={error}
      loading={loading}
      groupLabel="Model"
      columns={['Reads', 'Writes 5m', 'Writes 1h', 'Hit ratio', 'Cached share', 'Read cost', 'Write cost']}
      rows={rows.map((g) => {
        const costs = costsOf(g);
        return {
          key: g.key,
          label: g.label,
          title: withEstimate(g.label, costs.estimated),
          cells: [fmtCompact(g.tokens.cacheRead), fmtCompact(g.tokens.cacheWrite5m), fmtCompact(g.tokens.cacheWrite1h), fmtPct(hitRatioOf(g) ?? 0), fmtPct(cachedShareOf(g) ?? 0), ...costs.cells],
        };
      })}
    />
  );
}

/**
 * The Probe cost cell follows the position as the Probe cost tile does: native pairs the pool's plan probes' 20x 7-day
 * equivalent with its billed probes' dollars (`funded`, the billed read by pool), a dollar side reads its own read by
 * pool, and the shares read the pool's probe share where the route answers one. The other columns read every row.
 */
function KeepaliveByPool({ error, funded, groups, loading, unit }: { error: string | null; funded: Map<string, AnalyticsGroup>; groups: AnalyticsGroup[]; loading: boolean; unit: UnitPosition }) {
  const rows = useMemo(() => [...groups].sort((a, b) => (b.keptWarm?.tokens ?? 0) - (a.keptWarm?.tokens ?? 0)), [groups]);
  const probeCost = (g: AnalyticsGroup): ReactNode => {
    if (unit === 'native') {
      const sides = probeSidesOf(g, funded.get(g.key), 'cost');
      return nativeCell(nativeCellOf('seven_day', sides.equivalent, sides.estimated, sides.billed));
    }
    if (unit === 'share') {
      const shares = g.probeShares;
      return shares === null || !own(shares, 'seven_day') ? '-' : shareLabel('seven_day', shares.seven_day.cost);
    }
    return fmtUsd(unit === 'all' ? g.probeCostUsd : (funded.get(g.key)?.probeCostUsd ?? 0));
  };
  return (
    <BreakdownTable
      title="Cache warming by pool"
      error={error}
      loading={loading}
      groupLabel="Pool"
      columns={['Probes OK', 'Held', 'Skipped', 'Failed', 'Kept-warm requests', 'Kept-warm tokens', 'Probe cost']}
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
          g.fromRequests ? '-' : probeCost(g),
        ],
      }))}
    />
  );
}

/**
 * One ring of the cost by token kind. `format` prints each kind's figure in the ring's unit and the center's total
 * unless `center` prints it shorter, `label` names the ring at its center, and `title` is the ring's hover text, such
 * as the estimate's sentence.
 */
function CostDonut({ center, cost, format = fmtUsd, label = 'cost', title }: { center?: (v: number) => string; cost: CostSplit; format?: (v: number) => string; label?: string; title?: string }) {
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
            <span className="num">
              {format(cost[p.key])} <span className="muted">{fmtPct(total ? cost[p.key] / total : 0)}</span>
            </span>
          </li>
        ))}
      </ul>
    </div>
  );
}

/**
 * Native's cost split: the plan rows' 20x 7-day equivalent by token kind beside the billed dollars by token kind, a
 * side with no rows left out. The subscription ring's center reads a bare percent, since the card's subtitle names the
 * window, so a marked figure fits inside the ring, and its list names the window, since each row also reads the kind's
 * proportion of the ring as a bare percent. Plan rows the route priced nothing on the window draw no ring, since the
 * decoder reads the route's empty split as four zeros, and with no billed ring beside it the card reads a dash.
 */
function NativeSplit({ totals }: { totals: AnalyticsGroup }) {
  const sides = sidesOf(totals, 'seven_day');
  const subscription = sides.equivalent === undefined || sides.equivalent === null ? null : totals.equivalentSplit;
  const billed = sides.billed === undefined ? null : knownSplit(totals.billed);
  const mark = totals.estimated ? '~' : '';
  const rings = [
    ...(subscription === null ? [] : [<CostDonut key="subscription" cost={subscription} center={(v) => `${mark}${fmtShare(v)}`} format={(v) => `${mark}${equivalentShort('seven_day', v)}`} label="subscription" title={totals.estimated ? ESTIMATED_TITLE : undefined} />]),
    ...(billed === null ? [] : [<CostDonut key="billed" cost={billed} label="billed" />]),
  ];
  if (rings.length === 0) return sides.equivalent === null ? <p className="muted">-</p> : <Skeleton height={148} />;
  return rings.length === 1 ? rings[0] : <div className="split-pair">{rings}</div>;
}

const MAX_STACKED_GROUPS = 7;

type Stack = { data: Record<string, number>[]; estimated: Map<number, Set<string>>; keys: string[]; labels: Map<string, string> };
/**
 * The groups stacked by one figure per bucket: the seven with the largest total in range and the rest as Other, and
 * per bucket the stacks an estimated group's figure fed. `drawn` keeps only the groups whose figure is above zero
 * somewhere in range, for a side that holds some of the page's groups.
 */
function stackOf(points: AnalyticsPoint[], value: (g: AnalyticsGroup) => number, drawn = false): Stack {
  const totals = new Map<string, number>();
  const labels = new Map<string, string>();
  for (const p of points) {
    for (const g of p.groups) {
      totals.set(g.key, (totals.get(g.key) ?? 0) + value(g));
      labels.set(g.key, g.label);
    }
  }
  const ranked = [...totals.entries()]
    .filter(([, total]) => !drawn || total > 0)
    .sort((a, b) => b[1] - a[1])
    .map(([k]) => k);
  const top = ranked.slice(0, MAX_STACKED_GROUPS);
  const keys = ranked.length > MAX_STACKED_GROUPS ? [...top, 'Other'] : top;
  const estimated = new Map<number, Set<string>>();
  const data = points.map((p) => {
    const at = Date.parse(p.at);
    const row: Record<string, number> = { at };
    const marked = new Set<string>();
    for (const k of keys) row[k] = 0;
    for (const g of p.groups) {
      const k = top.includes(g.key) ? g.key : 'Other';
      if (!(k in row)) continue;
      row[k] += value(g);
      if (g.estimated && value(g) > 0) marked.add(k);
    }
    estimated.set(at, marked);
    return row;
  });
  return { data, estimated, keys, labels };
}
const sevenDayEquivalentOf = (g: AnalyticsGroup) => (own(g.costEquivalent, 'seven_day') ? g.costEquivalent.seven_day : 0);

/**
 * `shared` is whether the route answers the shares on the page's groups: under the account grouping, or with one
 * account named.
 */
function CostByGroup({ axis, error, points, groupBy, groupSlot, loading, shared, unit }: { axis: TimeAxis; error: string | null; groupBy: GroupBy; groupSlot: (key: string) => string; loading: boolean; points: AnalyticsPoint[]; shared: boolean; unit: UnitPosition }) {
  const ink = useChartInk();
  const lines = useMemo(() => shareLines(points), [points]);
  // Native stacks the plan rows' equivalent and draws the billed rows' dollars on a card of their own.
  const stack = useMemo(() => (unit === 'native' ? stackOf(points, sevenDayEquivalentOf, true) : stackOf(points, (g) => g.costUsd)), [points, unit]);
  const billed = useMemo(() => (unit === 'native' ? stackOf(points, (g) => g.billedUsd, true) : null), [points, unit]);
  const { keys, labels, data } = stack;
  const color = (k: string) => (k === 'Other' ? ink.muted : groupSlot(k));
  const label = (k: string) => labels.get(k) ?? k;
  const title = `Cost by ${GROUP_LABEL[groupBy].toLowerCase()}`;
  const areas = (s: Stack) => s.keys.map((k) => <Area key={k} type="monotone" dataKey={k} stackId="a" stroke={color(k)} strokeWidth={1.5} fill={color(k)} fillOpacity={0.35} dot={false} isAnimationActive={false} />);
  const dollarCard = (s: Stack, cardTitle: string, subtitle: string) => (
    <ChartCard title={cardTitle} subtitle={subtitle} loading={loading} error={error} legend={s.keys.map((k) => ({ color: color(k), label: s.labels.get(k) ?? k }))}>
      <AreaChart data={s.data} margin={{ top: 8, right: 8, bottom: 0, left: 0 }}>
        <CartesianGrid vertical={false} stroke={ink.grid} />
        <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
        <YAxis {...usdAxis(s.data.map((row) => s.keys.reduce((n, k) => n + row[k], 0)))} tickLine={false} axisLine={false} />
        <Tooltip cursor={{ stroke: ink.axis }} content={timeTip<Record<string, number> & { at: number }>((p) => s.keys.map((k) => ({ color: color(k), label: s.labels.get(k) ?? k, value: fmtUsd(p[k] ?? 0) })))} />
        {areas(s)}
      </AreaChart>
    </ChartCard>
  );
  if (unit === 'native') {
    const marked = (k: string) => data.some((row) => stack.estimated.get(row.at)?.has(k) ?? false);
    const billedDrawn = billed !== null && billed.keys.length > 0;
    // A range with no plan row leaves the equivalent card out while the billed card draws, as the tiles leave out a
    // side with no rows. With neither side the card stays, so an empty range still reads as one.
    const planDrawn = loading || error !== null || keys.length > 0 || !billedDrawn;
    return (
      <>
        {planDrawn && (
          <ChartCard title={title} subtitle="20x 7-day equivalent per bucket, stacked, the subscription requests" loading={loading} error={error} legend={keys.map((k) => ({ color: color(k), label: label(k), title: marked(k) ? ESTIMATED_TITLE : undefined }))}>
            <AreaChart data={data} margin={{ top: 8, right: 8, bottom: 0, left: 0 }}>
              <CartesianGrid vertical={false} stroke={ink.grid} />
              <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
              <YAxis tickFormatter={(v: number) => fmtShare(v)} tickLine={false} axisLine={false} width={60} />
              <Tooltip
                cursor={{ stroke: ink.axis }}
                content={timeTip<Record<string, number> & { at: number }>((p) => keys.map((k) => ({ color: color(k), label: label(k), value: equivalentOrDash('seven_day', p[k] ?? 0, stack.estimated.get(p.at)?.has(k) ?? false) })))}
              />
              {areas(stack)}
            </AreaChart>
          </ChartCard>
        )}
        {billedDrawn && dollarCard(billed, `Billed by ${GROUP_LABEL[groupBy].toLowerCase()}`, 'USD per bucket, stacked, the billed requests')}
      </>
    );
  }
  if (unit === 'share') {
    // A model week a group never priced in the range lists no dashed entry, as its line draws nothing.
    const weekOf = (a: string) => lines.data.some((row) => row[`${a}:week`] !== null);
    // A line is named by its group's label, which names the page's grouping: an account under the account grouping, a
    // model, a key or another dimension of the one named account under any other.
    const nameOf = (a: string) => lines.labels[a] ?? a;
    return (
      <ChartCard
        title={title}
        subtitle={groupBy === 'account' ? "Each account's share of its own 7-day window per bucket, solid, and of its model week, dashed" : `Each ${GROUP_LABEL[groupBy].toLowerCase()}'s share of the named account's 7-day window per bucket, solid, and of its model week, dashed`}
        loading={loading}
        error={error}
        empty={shared ? undefined : GROUPED_SHARE_NOTE}
        legend={lines.accounts.flatMap((a) => [{ color: groupSlot(a), label: `${nameOf(a)} (7-day)`, shape: 'line' as const }, ...(weekOf(a) ? [{ color: groupSlot(a), dashed: true, label: `${nameOf(a)} (${weekLabelOf(lines.weeks[a])})`, shape: 'line' as const }] : [])])}
      >
        <LineChart data={lines.data} margin={{ top: 8, right: 8, bottom: 0, left: 0 }}>
          <CartesianGrid vertical={false} stroke={ink.grid} />
          <XAxis dataKey="at" {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
          <YAxis tickFormatter={(v: number) => fmtShare(v)} tickLine={false} axisLine={false} width={60} />
          <Tooltip
            cursor={{ stroke: ink.axis }}
            content={timeTip<Record<string, number | null> & { at: number }>((p) =>
              lines.accounts.flatMap((a) => [
                { color: groupSlot(a), label: `${nameOf(a)} (5-hour)`, value: shareOrDash(p[`${a}:5h`]) },
                { color: groupSlot(a), label: `${nameOf(a)} (7-day)`, value: shareOrDash(p[`${a}:7d`]) },
                ...(weekOf(a) ? [{ color: groupSlot(a), label: `${nameOf(a)} (${weekLabelOf(lines.weeks[a])})`, value: shareOrDash(p[`${a}:week`]) }] : []),
              ]),
            )}
          />
          {/* The lines leave a null bucket unconnected, so a bucket with no share reads as a gap rather than a slope, and
              a priced bucket with a gap or the range's edge on both sides draws as a dot, as the Burn plan lines do. */}
          {lines.accounts.map((a) => (
            <Line key={`${a}:7d`} type="monotone" dataKey={`${a}:7d`} stroke={groupSlot(a)} strokeWidth={2} dot={isolatedDot(lines.data.map((r) => r[`${a}:7d`]), groupSlot(a))} isAnimationActive={false} />
          ))}
          {lines.accounts.map((a) => (
            <Line key={`${a}:week`} type="monotone" dataKey={`${a}:week`} stroke={groupSlot(a)} strokeWidth={2} strokeDasharray="4 3" dot={isolatedDot(lines.data.map((r) => r[`${a}:week`]), groupSlot(a))} isAnimationActive={false} />
          ))}
        </LineChart>
      </ChartCard>
    );
  }
  return dollarCard(stack, title, `USD per bucket, stacked, ${UNIT_SUBSET[dollarsOf(unit)]}`);
}

const UNFILTERED_NOTE = '. The filters above do not apply here';
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
const CLASS_NOTE = `conversation is ${CONVERSATION_HINT}. Overhead is ${OVERHEAD_HINT}`;

const SPEND_NOTE = "Dollars per minute: API-key traffic, and a subscription account's fast-mode and overage answers. Its plan quota spends window share, charted per window beside it";
/** A folded rest of subscription accounts as the dollars chart names it: its dollars are its members' fast-mode
 *  and overage answers, and its own label names the plan unit, which the dollars axis does not chart. */
const SPEND_REST = 'Other (fast mode or overage)';
const SPEND_REST_LABEL: Record<Exclude<BurnLineUnit, 'usd'>, string> = { unknown: 'Other (fast mode or overage, tier unknown)', window_share: SPEND_REST };
const TIER_UNKNOWN_HINT = 'No tier on file, or one the cost model does not size, so no share of its windows is priced';
const MODEL_UNLISTED_HINT = 'A bucket whose traffic ran on a model of no known tier, or on a tier the cost model has no capacity for on that window, prices no share there and draws no point';
// "Model tier", since the tier unknown entries beside it name the account tier.
const ESTIMATED_HINT = 'A bucket whose model has no measured weight and reads the latest weighed model of its model tier';
const NO_ACCOUNT: ReadonlySet<string> = new Set();
const PARTIAL_NOTE = 'The last bucket is partial, rated over the part of its width it covers';

/**
 * One plan card's window: the rows' figure it reads, the line's per-hour mean, whether a line left traffic unpriced on
 * it, and its subtitle under each figure. Only the 5-hour card shades the peak band, the one window it weighs. The
 * route names no model week, so its card reads the one the cost model ships, the Fable week.
 */
interface PlanWindow {
  band: boolean;
  equivalent: string;
  /** The window's key, which names it on an equivalent ("1.47% of 20x 5-hour"). */
  key: string;
  /** Whether the slot's figure is one the route marked estimated, a zero the fold filled in never being one. */
  marked: (r: BurnRow, slot: number) => boolean;
  perHour: (l: BurnLine) => number | null;
  read: (r: BurnRow) => (number | null)[];
  share: (whose: string) => string;
  title: string;
  unpriced: (l: BurnLine) => boolean;
  word: string;
}
const PLAN_WINDOWS: PlanWindow[] = [
  {
    band: true,
    equivalent: 'The subscription traffic per hour in 20x 5-hour equivalents, weighed by the peak band, whose hours are shaded',
    key: 'five_hour',
    marked: (r, slot) => r.estimated[slot],
    perHour: (l) => l.fiveHourPerHour,
    read: (r) => r.fiveHour,
    share: (whose) => `The share of ${whose} own 5-hour window its traffic used per hour, weighed by the peak band, whose hours are shaded`,
    title: '5-hour per hour',
    unpriced: (l) => l.unpriced.fiveHour,
    word: '5-hour',
  },
  {
    band: false,
    equivalent: 'The subscription traffic per hour in 20x 7-day equivalents',
    key: 'seven_day',
    marked: (r, slot) => r.estimated[slot],
    perHour: (l) => l.sevenDayPerHour,
    read: (r) => r.sevenDay,
    share: (whose) => `The share of ${whose} own 7-day window its traffic used per hour`,
    title: '7-day per hour',
    unpriced: (l) => l.unpriced.sevenDay,
    word: '7-day',
  },
  {
    band: false,
    equivalent: `The subscription traffic per hour in 20x equivalents of its model's own week, the ${weekLabelOf(undefined)} window`,
    key: 'seven_day_fable',
    marked: (r, slot) => r.estimated[slot] && r.weekAnswered[slot],
    perHour: (l) => l.weekPerHour,
    read: (r) => r.week,
    share: (whose) => `The share of ${whose} own model week its traffic used per hour, the ${weekLabelOf(undefined)} window`,
    title: 'Model week per hour',
    unpriced: () => false,
    word: weekLabelOf(undefined),
  },
];

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
 * A plan line's dot: hollow on a slot whose bucket priced through the model tier's fallback, a channel of its own
 * beside the dash and the isolated slot's filled dot, which it takes elsewhere.
 */
const estimatedDot = (values: (number | null)[], estimated: boolean[], color: string, ground: string) => {
  const isolated = isolatedDot(values, color);
  return (props: { cx?: number; cy?: number; index: number; key?: string }) =>
    estimated[props.index] && values[props.index] !== null ? (
      <Dot key={props.key} className="recharts-line-dot recharts-line-dot--estimated" cx={props.cx} cy={props.cy} r={3} fill={ground} stroke={color} strokeWidth={2} />
    ) : (
      isolated(props)
    );
};

/**
 * The burn route's views of one grouping: the dollars per minute, and one plan card per window. A line's color
 * follows its group within the answer rather than its rank, so a range that reorders the groups does not repaint
 * them, and the account grouping keeps each account's page-wide color. `figure` is what the plan cards read, and
 * `account` the one account the Accounts filter names, whose shares answer under any grouping.
 */
function BurnCharts({
  account,
  answer,
  costs,
  error,
  figure,
  groupBy,
  loading,
  rangeEndMs,
  readAt,
  sized,
  slotOf,
}: {
  account: string | null;
  answer: { bucketSeconds: number; points: BurnPoint[] } | null;
  costs: SubscriptionCosts | null;
  error: string | null;
  figure: BurnFigure;
  groupBy: BurnGroup;
  loading: boolean;
  rangeEndMs: number | undefined;
  readAt: number | undefined;
  sized: ReadonlySet<string> | null;
  slotOf: (id: string) => number | null;
}) {
  const ink = useChartInk();
  const { lines, rows } = useMemo(
    () => (answer === null ? { lines: [] as BurnLine[], rows: [] as BurnRow[] } : foldBurn(answer.points, groupBy, answer.bucketSeconds, sized ?? NO_ACCOUNT, MAX_BURN_LINES, rangeEndMs, readAt, { account, figure })),
    [account, answer, figure, groupBy, rangeEndMs, readAt, sized],
  );
  const named = useMemo(() => lines.filter((l) => !l.other).map((l) => l.key).sort(), [lines]);
  const colorOf = (l: BurnLine) => (l.other ? ink.muted : groupBy === 'account' ? accountColor(ink, slotOf(l.key)) : ink.series[Math.min(7, named.indexOf(l.key))]);
  // A session id runs past any legend, so it shows cut the way the session filter's chip cuts it, whole on hover.
  const nameOf = (l: BurnLine) => (groupBy === 'session' && !l.other ? shortId(l.label, 13) : l.label);
  const titleOf = (l: BurnLine) => (groupBy === 'request_class' ? (l.key === 'conversation' ? CONVERSATION_HINT : OVERHEAD_HINT) : groupBy === 'session' && !l.other ? l.label : undefined);
  const axis = timeAxis((answer?.bucketSeconds ?? 3_600) * 1_000, ink);
  // Read off the buckets, not the lines: the class grouping draws its two lines even over a range with no traffic.
  const empty = !loading && error === null && rows.length === 0 ? 'No traffic in this range.' : undefined;
  const byAccount = groupBy === 'account';
  // The route answers the shares where each group holds one account's rows.
  const shared = byAccount || account !== null;
  // Every other grouping mixes both kinds of account, so its lines chart the dollars their API-key traffic and
  // fast-mode and overage answers spent. Under the account grouping a subscription account, or a folded rest of
  // them, draws a dollar line only for what its fast-mode and overage answers spent.
  const spent = lines.map((l, i) => ({ i, l })).filter(({ l }) => !byAccount || l.unit === 'usd' || l.usdPerMinute > 0);
  // Until the settings land no account reads as sized, so the rest names no tier it cannot know yet.
  const spendNameOf = (l: BurnLine) => (l.other && l.unit !== 'usd' ? (sized === null ? SPEND_REST : SPEND_REST_LABEL[l.unit]) : nameOf(l));
  // Under the shares a sized subscription account draws, an idle one reading a dash. The equivalents read no tier, so
  // every group whose traffic a plan paid for draws, and a group of billed rows alone, which reads zero, does not.
  const planned = lines
    .map((l, i) => ({ i, l }))
    .filter(({ l }) => !l.other && (figure === 'share' ? l.unit === 'window_share' : l.unpriced.fiveHour || l.unpriced.sevenDay || [l.fiveHourPerHour, l.sevenDayPerHour, l.weekPerHour].some((v) => v !== null && v > 0)));
  const unpriced = figure === 'share' && shared ? lines.filter((l) => !l.other && l.unit === 'unknown') : [];
  // A share reads bare, its card naming the window, while an equivalent names its window ("1.47% of 20x 5-hour"), in
  // the short form in the legend.
  const figureOf = (w: PlanWindow, v: number | null, estimated: boolean, form: 'full' | 'short') => {
    if (v === null) return '-';
    if (figure === 'share') return `${estimated ? '~' : ''}${fmtShare(v)}`;
    return form === 'full' ? equivalentOrDash(w.key, v, estimated) : `${estimated ? '~' : ''}${equivalentShort(w.key, v)}`;
  };
  const partial = rows[rows.length - 1]?.partial === true;
  // The band's own hours over the instants the plan chart spans: the buckets are UTC multiples whose starts fall off
  // the band's edges, so shading whole buckets would mark the wrong hours.
  const band = useMemo(() => (costs === null || rows.length < 2 ? [] : bandRuns(costs, rows[0].at, rows[rows.length - 1].at)), [costs, rows]);
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
  const whose = byAccount ? "each subscription account's" : "the named account's";
  // The model week's card draws while a line ran on a model with a week of its own.
  const windows = PLAN_WINDOWS.filter((w) => w !== PLAN_WINDOWS[2] || planned.some(({ l }) => l.weekPerHour !== null));
  const planCard = (w: PlanWindow) => {
    const drawn = w === PLAN_WINDOWS[2] ? planned.filter(({ l }) => l.weekPerHour !== null) : planned;
    const legend = [
      ...drawn.map(({ i, l }) => {
        // Per window, as the line's dots read it, so a model week the fold filled at zero takes no mark from estimated
        // 5-hour buckets.
        const marked = rows.some((r) => w.marked(r, i));
        return {
          color: colorOf(l),
          label: nameOf(l),
          shape: 'line' as const,
          title: withEstimate(w.band && l.fiveHourPeak !== null ? `Highest hour ${figureOf(w, l.fiveHourPeak, false, 'full')}` : '', marked) || undefined,
          value: figureOf(w, w.perHour(l), marked, 'short'),
        };
      }),
      // One entry for every figure the cost model cannot price on the window, whichever line it fell on. Muted, since
      // it draws no line of its own.
      ...(drawn.some(({ l }) => w.unpriced(l)) ? [{ color: ink.muted, label: 'model unlisted', shape: 'line' as const, title: MODEL_UNLISTED_HINT, value: w.word }] : []),
      ...(drawn.some(({ l }) => l.estimated) ? [{ color: ink.muted, hollow: true, label: 'estimated', shape: 'line' as const, title: ESTIMATED_HINT }] : []),
      // Muted, since no line of its own is drawn.
      ...unpriced.map((l) => ({ color: ink.muted, label: nameOf(l), shape: 'line' as const, title: TIER_UNKNOWN_HINT, value: 'tier unknown' })),
    ];
    return (
      <ChartCard
        key={w.title}
        title={w.title}
        subtitle={
          <>
            {figure === 'share' ? w.share(whose) : w.equivalent}.{partial ? ` ${PARTIAL_NOTE}.` : ''} <CostReferenceLink tab="tiers" />
          </>
        }
        error={error}
        loading={loading}
        empty={
          empty ??
          (figure === 'share' && !shared
            ? GROUPED_SHARE_NOTE
            : drawn.length > 0
              ? undefined
              : unpriced.length > 0
                ? byAccount
                  ? `No subscription account in range has a tier the cost model sizes: ${unpriced.map(nameOf).join(', ')}.`
                  : 'The named account has no tier on file, or one the cost model does not size, so no share of its windows is priced.'
                : figure === 'share'
                  ? 'No subscription account burned in this range.'
                  : 'No subscription traffic in this range.')
        }
        legend={legend}
      >
        <LineChart data={rows} margin={{ top: 8, right: 8, bottom: 0, left: 0 }}>
          <CartesianGrid vertical={false} stroke={ink.grid} />
          {/* A category axis places a reference area on its categories alone, so this one counts instants and ticks on
              the buckets. */}
          <XAxis dataKey="at" type="number" scale="time" domain={['dataMin', 'dataMax']} ticks={rows.map((r) => r.at)} {...axis} tickLine={false} axisLine={{ stroke: ink.axis }} minTickGap={40} />
          <YAxis tickFormatter={(v: number) => fmtShare(v)} tickLine={false} axisLine={false} width={52} {...scaleTo(scaled.flatMap((r) => drawn.map(({ i }) => w.read(r)[i] ?? 0)))} />
          {/* Recharts drops null entries by default, and a bucket whose every figure is null would lose its date, its
              dashes and the unpriced hint. */}
          <Tooltip
            cursor={{ stroke: ink.axis }}
            filterNull={false}
            content={timeTip<BurnRow>((r) =>
              drawn.map(({ i, l }) => {
                const v = w.read(r)[i];
                const marked = w.marked(r, i) && v !== null;
                return { color: colorOf(l), label: nameOf(l), title: w.unpriced(l) && slotUnpriced(v, r.tokens[i]) ? MODEL_UNLISTED_HINT : marked ? ESTIMATED_HINT : undefined, value: figureOf(w, v, marked, 'full') };
              }),
            )}
          />
          {w.band && band.map((run) => <ReferenceArea key={run.from} x1={run.from} x2={run.to} fill={ink.series[6]} fillOpacity={0.34} ifOverflow="hidden" />)}
          {drawn.map(({ i, l }) => (
            // A null slot draws no point, and the gap stays since the lines do not connect across it.
            <Line key={i} type="monotone" dataKey={(r: BurnRow) => w.read(r)[i]} stroke={colorOf(l)} strokeWidth={2} dot={estimatedDot(rows.map((r) => w.read(r)[i]), rows.map((r) => w.marked(r, i)), colorOf(l), ink.surface)} isAnimationActive={false} />
          ))}
        </LineChart>
      </ChartCard>
    );
  };
  return (
    <div className="grid-2 burn-grid">
      {windows.map(planCard)}
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
  axis,
  bucketMs,
  error,
  loading,
  otherFilters,
  rollups: all,
  slotOf,
  span,
}: {
  accountIds: string[];
  axis: TimeAxis;
  bucketMs: number;
  error: string | null;
  loading: boolean;
  otherFilters: boolean;
  rollups: AccountRollup[] | null;
  slotOf: (id: string) => number | null;
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
    </>
  );
}
