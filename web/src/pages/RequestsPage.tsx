import { useEffect, useMemo, useRef, useState } from 'react';
import { Link, useLocation, useNavigate, useParams } from 'react-router-dom';
import { api } from '@/api';
import { iso } from '@/api/decode';
import type { Account, Attempt, AttemptResponse, CapturedBodies, CohortHold, DiagnosticsSource, Guardrail, RequestLog, RequestOutcome, RequestPage, RequestQuery, Settings, Usage } from '@/api/types';
import { CLASSIFIER_VALUES, REFUSAL_CATEGORIES, REQUEST_OUTCOME_VALUES, STOP_REASONS } from '@/api/types';
import { useAppSettings } from '@/app/settings';
import { useFollow, usePaging } from '@/app/useFollow';
import { BodyViewer } from '@/components/BodyViewer';
import { Chips } from '@/components/ChipView';
import { FilterBar } from '@/components/FilterBar';
import { PhaseWaterfall } from '@/components/PhaseWaterfall';
import { SortControl } from '@/components/SortControl';
import { TokenBar } from '@/components/TokenBar';
import { AttemptCount, StatusBadge } from '@/components/ui/AttemptTrail';
import { ColumnsControl } from '@/components/ui/ColumnsControl';
import { CostTd } from '@/components/ui/CostTd';
import type { Option } from '@/components/ui/OptionList';
import { IconChevronLeft, IconChevronRight, IconX } from '@/components/ui/icons';
import { PagerNav } from '@/components/ui/PagerNav';
import { PopoverHost, useHoldHost } from '@/components/ui/Popover';
import { Badge, Button, Card, CopyButton, Descriptions, Dialog, EmptyState, ErrorState, Input, KeyValue, Select, Skeleton, SkeletonRows, toggleRow } from '@/components/ui/primitives';
import { accountHref, addressWindow, REQUEST_ORDER_OF, REQUEST_PAGE_SIZES, REQUEST_SORT_KEYS, requestHref, REQUESTS_CODEC, sortDirOf } from '@/lib/address';
import { sentences, shareHint, shareHintLines, type WeekReading } from '@/lib/analytics';
import { autoModeLabel } from '@/lib/autoMode';
import { bodyLines, bodyText, findHits } from '@/lib/bodies';
import { CACHE_MISS_REASONS, isDivergence, missLabel } from '@/lib/cache';
import { AGENT_TYPES, TURN_ORIGINS } from '@/lib/client';
import { clock } from '@/lib/clock';
import { groupInto } from '@/lib/counts';
import { EFFORT_LEVELS, foldRequestClass, NO_CLASS, NO_EFFORT, OTHER_DIMENSION, REQUEST_KIND_LABEL, servedMissReason, servingAttempt, servingResponse } from '@/lib/dimensions';
import { keyOptions, notLoadedReason, REQUESTS_DIMENSIONS } from '@/lib/filterBar';
import { effectiveInterval, ENDED_RANGE, FOLLOW_OVERRUN, pollQuery, readEnd, rowsRefreshTitle, shownCount, shownWords, spanEnded } from '@/lib/follow';
import { bytesDropped, fmtAbsolute, fmtCompact, fmtDatedTime, fmtInt, fmtMs, fmtRelative, fmtTime, fmtUsd, fmtWeek, forkLabel, modelTitle, shortAgent, shortId, shortModel, tokensTotal } from '@/lib/format';
import { guardrailLabel, guardrailOptions } from '@/lib/guardrails';
import { useAsync, useNow, useQueryFilters, useRangeTakenAt, useWeekReading } from '@/lib/hooks';
import { costCellOf, costHeaderTitle, type CostTable, type CostUnit, costUnitOf, FREE_SHARE_TITLE, FUNDING_WORD, heldWords, NO_SHARE_TITLE, orderedTokens, requestCostUnit, requestHintArgs, requestOrder, revisionsTitle } from '@/lib/requests';
import { requestsWiden } from '@/lib/requestsLink';
import { type Chip, logHref, requestLink, words } from '@/lib/routingLog';
import { plainOutputOf } from '@/lib/tokenBar';
import type { UnitPosition } from '@/lib/unit';

// `fill` marks the columns the table draws `col-fill`, so the table takes its content width when none of them shows.
const COLUMNS = [
  { fill: true, id: 'time', label: 'Time' },
  { id: 'key', label: 'Key' },
  { id: 'pool', label: 'Pool' },
  { id: 'account', label: 'Account' },
  { id: 'model', label: 'Model' },
  { id: 'client', label: 'Client' },
  { id: 'kind', label: 'Kind' },
  { id: 'transport', label: 'Transport' },
  { id: 'status', label: 'Status' },
  { id: 'attempts', label: 'Attempts' },
  { id: 'ttfb', label: 'TTFB' },
  { id: 'duration', label: 'Duration' },
  { id: 'tokens', label: 'Tokens' },
  { id: 'input', label: 'Uncached input' },
  { id: 'output', label: 'Output tokens' },
  { id: 'cost', label: 'Cost' },
] as const;
type ColumnId = (typeof COLUMNS)[number]['id'];
// The drawer names all six, and leaving them out lets the default set fit the card at 1280 by 800.
const HIDDEN_BY_DEFAULT: readonly ColumnId[] = ['duration', 'input', 'kind', 'output', 'pool', 'transport'];
/** The columns the table shows while the drawer is open: derived at render, so the chosen set returns on close. */
const COMPACT: readonly ColumnId[] = ['time', 'model', 'status', 'attempts'];

/** The codes the router and Anthropic answer; any other code a page or the address holds is offered by its number. */
const STATUS_CODES: readonly (readonly [number, string])[] = [
  [200, 'OK'],
  [400, 'Bad request'],
  [401, 'Unauthorized'],
  [403, 'Forbidden'],
  [404, 'Not found'],
  [413, 'Payload too large'],
  [429, 'Rate limited'],
  [500, 'Server error'],
  [502, 'Bad gateway'],
  [503, 'Unavailable'],
  [504, 'Gateway timeout'],
  [524, 'Timeout'],
  [529, 'Overloaded'],
];
const statusOptions = (rows: RequestLog[], selected: string[]) => {
  const named = new Map(STATUS_CODES);
  const codes = new Set([...named.keys(), ...rows.flatMap((r) => (r.status === null ? [] : [r.status])), ...selected.map(Number)]);
  return [...codes].sort((a, b) => a - b).map((code) => ({ label: named.has(code) ? `${code} ${named.get(code)}` : String(code), value: String(code) }));
};
/**
 * The known reasons in their reading order, then every other name the loaded rows' serving attempts or the address
 * carry, alphabetically, so a link naming a reason the console has no label for still shows as chosen.
 */
const missReasonOptions = (rows: RequestLog[], selected: string[]) => {
  const served = rows.flatMap((r) => servedMissReason(r) ?? []);
  const others = new Set([...served, ...selected].filter((name) => !CACHE_MISS_REASONS.includes(name)));
  return [...CACHE_MISS_REASONS, ...[...others].sort()].map((name) => ({ label: missLabel(name), value: name }));
};
/** The known words, then the loaded rows' other values and the filter's own, each as itself, then `-` labeled `none`. */
function wordOptions(known: readonly string[], loaded: (string | null)[], chosen: string[]): Option[] {
  const others = new Set([...loaded, ...chosen].filter((v): v is string => v !== null && v !== '-' && !known.includes(v)));
  return [...[...known, ...[...others].sort()].map((v) => ({ label: v, value: v })), { label: 'none', value: '-' }];
}
/**
 * The classes the loaded rows and the address carry as the rollups fold them, alphabetically, then a request that sent
 * no class under the label the Analytics page gives it, then the classes the fold reads as `other`.
 */
const requestClassOptions = (rows: RequestLog[], selected: string[]) => {
  const classes = new Set([...rows.map((r) => foldRequestClass(r.requestClass)), ...selected].filter((c) => c !== NO_CLASS && c !== OTHER_DIMENSION));
  return [...[...classes].sort().map((c) => ({ label: c, value: c })), { label: 'unattributed', value: NO_CLASS }, { label: OTHER_DIMENSION, value: OTHER_DIMENSION }];
};


/** Keyed by every outcome the ledger records, so one it gains is a type error until it is named here. */
const REQUEST_OUTCOME_LABEL: Record<RequestOutcome, string> = {
  canceled: 'canceled',
  exhausted: 'exhausted',
  failed: 'failed',
  incomplete: 'incomplete',
  rejected: 'rejected',
  succeeded: 'succeeded',
};
const CLASSIFIER_LABEL: Record<(typeof CLASSIFIER_VALUES)[number], string> = { client: 'client classifier', client_off: 'client classifier off' };
const transportLabel = (r: Pick<RequestLog, 'transport'>) => (r.transport === 'sse' ? 'SSE' : 'JSON');

/**
 * The router's ids run to 44 characters, which would wrap the drawer head to a second line beside its two actions at
 * 400 px, so the head shows this many and its hover keeps the id whole.
 */
const HEAD_ID_CHARS = 24;
const GEOGRAPHY_TITLE = 'The inference geography the upstream reported for the answer: global or us. Not available means the model does not take the inference geography parameter.';
const UPSTREAM_REFUSAL_TITLE = 'The upstream refused this answer. Open the request for the stop reason and the recommended model.';
/** The issuer's account where a loaded row's attempt issued the presented token, the generic words otherwise. */
const fallbackTitle = (digest: string, issuedBy: ReadonlyMap<string, Attempt[]>): string => {
  const issuer = issuedBy.get(digest)?.[0]?.accountId ?? null;
  return issuer === null ? 'A fallback token presented with this request' : `A fallback token issued by ${issuer}`;
};

/** The log on this request, a quarter hour either side of it. A row with no duration is read as a moment. */
const routingLogHref = (r: RequestLog, now: number) => {
  const at = Date.parse(r.at);
  return logHref(requestLink(r.id, at, r.durationMs === null ? null : at + r.durationMs, now));
};

/** The figure's context a request's cost reads: the accounts read and the cost model the hint is composed from, and the
 *  chosen week with its scale and `failure`, as `weekReadingOf` reads them, which a figure waiting on the scale draws
 *  as a dash titled with the failure. */
interface CostContext extends WeekReading {
  accounts: Account[] | null;
  costs: CostTable | null;
}

/** A request's unit under a position, with the hint composed where the accounts read holds its account. */
function requestCost(request: RequestLog, unit: UnitPosition, context: CostContext): { hint: string[]; unit: CostUnit } {
  const args = requestHintArgs(request, context.accounts, context.costs);
  const lines = args === null ? [] : shareHintLines(...args);
  return { hint: lines, unit: costUnitOf(request, unit, args === null ? null : shareHint(...args)) };
}

/**
 * The Cost cell: the position's main line over its second, the hint and the dollar equivalent on the title, the
 * skeleton while the scale is out, and a dash titled with the context's failure where it carries one.
 */
function CostCell({ context, request, unit }: { context: CostContext; request: RequestLog; unit: UnitPosition }) {
  // The unit alone: the drawer's hint lines would be built for nothing per row.
  return <CostTd cell={costCellOf(requestCostUnit(request, unit, context.accounts, context.costs), context.week, context.scale)} className="requests__cost" failure={context.failure} />;
}

/** A component of its own, so the expression-bodied row map reads the hold's words once. */
function HeldBadge({ held }: { held: CohortHold }) {
  const words = heldWords(held);
  return (
    <Badge tone="warning" title={words.title}>
      {words.badge}
    </Badge>
  );
}

/** A token count's cell, a component of its own so the expression-bodied row map reads the count once. */
function OrderedTokensCell({ kind, request }: { kind: 'input' | 'output'; request: RequestLog }) {
  const count = orderedTokens(request, kind);
  return <td className="right num col-fit">{count === null ? <span className="muted" title="No count stored for this request">-</span> : fmtCompact(count)}</td>;
}

/**
 * The drawer's cost value: the figure in the chosen week as one chip with the dollar equivalent muted after it, the
 * dollars, the dollar equivalent with `equivalent` muted after it, or the dash of a position that reads nothing for the
 * request. The figure waits on the scale, so it draws its skeleton while the cost model is out and a dash where the
 * context carries a failure.
 */
function costRowValue(unit: CostUnit, context: CostContext) {
  const { failure, scale, week } = context;
  switch (unit.kind) {
    case 'equivalent':
      if (scale === null) return failure === null ? <Skeleton width={120} /> : <b>-</b>;
      return (
        <span className="usage-table__end">
          <Chips chips={[{ value: fmtWeek(unit.figure * scale, week) }]} />
          <span className="muted">{fmtUsd(unit.usd)} equivalent</span>
        </span>
      );
    case 'dash':
      return <b>-</b>;
    case 'dollars':
    case 'unserved':
      return <b>{fmtUsd(unit.usd)}</b>;
    case 'equivalent_dollars':
    case 'unpriced':
      return (
        <b>
          {fmtUsd(unit.usd)} <span className="muted">equivalent</span>
        </b>
      );
  }
}

/**
 * The drawer's Estimated cost row under one title: the context's failure where the figure waits on it, why a
 * dollar figure, a dash or a free call's figure stands where it does, the hint, the serving account's tier as the
 * attempt stored it, and the price list and the cost model in force at the request's instant while the settings are at
 * hand. A title on an element inside the cell would hide the row's own wherever the figure is hovered. The hint's
 * lines follow the figure as chips on a row of their own across the table, since the 5-hour line with its band word
 * runs wider than the value column of the 400 px drawer.
 */
function CostRows({ context, request: r, settings, unit }: { context: CostContext; request: RequestLog; settings: Settings | null; unit: UnitPosition }) {
  const { hint, unit: cost } = requestCost(r, unit, context);
  const failed = cost.kind === 'equivalent' && context.scale === null ? (context.failure?.text ?? null) : null;
  const why = (cost.kind === 'equivalent' || cost.kind === 'equivalent_dollars') && cost.reason !== null ? FREE_SHARE_TITLE : cost.kind === 'dollars' ? `Billed in dollars: ${FUNDING_WORD[cost.funding]}` : cost.kind === 'unpriced' ? NO_SHARE_TITLE : cost.kind === 'dash' ? cost.title : null;
  const shown = cost.kind === 'equivalent' || cost.kind === 'equivalent_dollars' ? cost.hint : null;
  const title = sentences([failed, why, shown, r.tier === null ? null : `Account tier ${r.tier}`, revisionsTitle(settings, r.at)]);
  return (
    <>
      <tr>
        <td>
          <b>Estimated cost</b>
        </td>
        <td className="right num" title={title || undefined}>
          {costRowValue(cost, context)}
        </td>
      </tr>
      {cost.kind === 'equivalent' && context.scale !== null && hint.length > 0 && (
        <tr className="usage-table__hint">
          <td colSpan={2} className="right" title={title || undefined}>
            <span className="usage-table__end">
              <Chips chips={hint.map((value) => ({ value }))} />
            </span>
          </td>
        </tr>
      )}
    </>
  );
}

/** The route answers its rows as `requests`, and the poll reads a list's as `rows`. */
const listed = (page: RequestPage) => ({ landedThrough: page.landedThrough, nextCursor: page.nextCursor, rows: page.requests });
/** A full largest page with more available, the widest count a freshly read page draws. */
const WIDEST_COUNT = shownWords(Math.max(...REQUEST_PAGE_SIZES), true);

export function RequestsPage() {
  const { requestId } = useParams();
  const navigate = useNavigate();
  const location = useLocation();
  const now = useNow();
  const keys = useAsync(() => api.listKeys(), []);
  const pools = useAsync(() => api.listPools(), []);
  const guardrails = useAsync(() => api.listGuardrails(), []);
  const accounts = useAsync(() => api.listAccounts(), []);
  const catalog = useAsync(() => api.listCatalog(), []);
  const { timeFormat, unit, week } = useAppSettings();
  // The revisions the drawer's cost row names, and the cost model whose capacities scale every figure to the chosen week
  // and whose band the hint reads at a request's completion.
  const { reading, settings } = useWeekReading(week);
  const costs = settings.data?.subscriptionCosts ?? null;
  const context: CostContext = { accounts: accounts.data, costs, ...reading };

  const { address, filters, replace, update } = useQueryFilters(REQUESTS_CODEC);
  const [visible, setVisible] = useState<Set<ColumnId>>(() => new Set(COLUMNS.map((c) => c.id).filter((id) => !HIDDEN_BY_DEFAULT.includes(id))));
  // Under a preset a cursor page here reads to the router's clock, as the poll under the watermark does, so only the
  // first page and a custom range read an end.
  const { cursor, pagedTo, reset: resetPaging, setPaging, stack } = usePaging(address);
  // The range ends where the list was loaded, so the first page and a custom range's cursor pages read against the
  // window the list was loaded under, a preset's cursor page to the router's clock, and Refresh moves the end to now.
  const [loadedAt, setLoadedAt] = useState(() => clock.now());
  // Off on the logs until the reader picks an interval, and page state, so a link or a reload opens the log off.
  const [intervalMs, setIntervalMs] = useState(0);
  const { cause: holdCause, host } = useHoldHost();
  const rangeTakenAt = useRangeTakenAt(filters.range);

  // A custom range names its own window, which Refresh reads again rather than moving it to now.
  const custom = filters.range.preset === 'custom';
  const span = useMemo(() => addressWindow(filters.range, '24h', loadedAt), [filters.range, loadedAt]);
  const ended = spanEnded(custom, span.to, rangeTakenAt);
  const polledMs = effectiveInterval(intervalMs, ended);
  // The read pages by the key, since a page of a paged list cannot be sorted on its own.
  const dir = sortDirOf(REQUEST_SORT_KEYS, filters);
  const order = REQUEST_ORDER_OF[filters.sort];
  const query = useMemo<RequestQuery>(
    // Probes are off the log unless they are what the filter asks for.
    () => ({ accountIds: filters.accounts, agentId: filters.agentId || undefined, agentTypes: filters.agentTypes, cacheMissReasons: filters.cacheMissReasons, classifiers: filters.classifiers, client: filters.client, cursor, dir, efforts: filters.efforts, fallback: filters.fallback === '' ? undefined : filters.fallback === 'true', from: iso(span.from), guardrails: filters.guardrails, held: filters.held, includeProbes: filters.kind === 'probe', keyIds: filters.keys, kind: filters.kind || null, limit: filters.rows, models: filters.models, order, outcomes: filters.outcomes, poolIds: filters.pools, refusalCategories: filters.refusalCategories, request: filters.request || undefined, requestClasses: filters.requestClasses, search: filters.search, session: filters.session || undefined, speeds: filters.speeds, statuses: filters.statuses.map(Number), stopReasons: filters.stopReasons, to: cursor !== undefined && !custom ? undefined : iso(readEnd(custom, span.to, pagedTo ?? span.to)), turnOrigins: filters.turnOrigins, userAgent: filters.agent }),
    [filters, cursor, custom, dir, order, span, pagedTo],
  );
  const requests = useAsync(() => api.listRequests(query), [query]);
  // The open drawer reads one of the rows, and an open menu or popover under the bar stands over the list, so the poll
  // waits for either to close.
  const { error, fresh, goNext, goPrevious, nextCursor, overrun, rows, setFollowed } = useFollow<RequestLog>({
    address,
    compare: requestOrder(order, dir),
    cursor,
    error: requests.error,
    fixedEnd: custom,
    identity: (r) => r.id,
    intervalMs: polledMs,
    list: requests.data && listed(requests.data),
    pagedTo,
    paused: Boolean(requestId) || holdCause !== null,
    read: (c, to, landedAfter) => api.listRequests(pollQuery(query, custom, span.to, to, c, landedAfter)).then(listed),
    setPaging,
    stack,
    stale: requests.stale,
  });
  const issuedBy = useMemo(() => groupInto(rows.flatMap((row) => row.attempts), (a) => a.issuedDigest), [rows]);

  const refresh = () => {
    resetPaging();
    setFollowed(null);
    setLoadedAt(clock.now());
    settings.reload();
  };
  // The page size and the order are how the operator reads the log rather than what it narrows to, so Reset filters
  // keeps them.
  const reset = () => replace({ ...REQUESTS_CODEC.defaults, dir: filters.dir, rows: filters.rows, sort: filters.sort });
  // Six of the lists scan the drawn rows, which grow while the log follows, and the page re-renders on every poll and
  // clock tick, so the lists follow their reads, the filters and the rows alone.
  const options = useMemo<Record<string, Option[]>>(
    () => ({
      accounts: (accounts.data ?? []).map((a) => ({ label: a.label, value: a.id })),
      agentTypes: AGENT_TYPES.map((v) => ({ label: v, value: v })),
      cacheMissReasons: missReasonOptions(rows, filters.cacheMissReasons),
      classifiers: [...CLASSIFIER_VALUES.map((v) => ({ label: CLASSIFIER_LABEL[v], value: v })), { label: 'none', value: NO_CLASS }],
      efforts: [...EFFORT_LEVELS, NO_EFFORT, OTHER_DIMENSION].map((v) => ({ label: v, value: v })),
      fallback: [
        { label: 'yes', value: 'true' },
        { label: 'no', value: 'false' },
      ],
      guardrails: guardrailOptions(guardrails.data, rows, filters.guardrails),
      keys: keyOptions(keys.data, filters.keys),
      kind: (['inference', 'count_tokens', 'probe'] as const).map((value) => ({ label: REQUEST_KIND_LABEL[value], value })),
      models: (catalog.data ?? []).map((m) => ({ label: m.id, value: m.id })),
      outcomes: REQUEST_OUTCOME_VALUES.map((o) => ({ label: REQUEST_OUTCOME_LABEL[o], value: o })),
      pools: (pools.data ?? []).map((p) => ({ label: p.name, value: p.id })),
      refusalCategories: wordOptions(REFUSAL_CATEGORIES, rows.map((r) => servingResponse(r)?.refusalCategory ?? null), filters.refusalCategories),
      requestClasses: requestClassOptions(rows, filters.requestClasses),
      speeds: [
        { label: 'fast', value: 'fast' },
        { label: 'standard', value: 'standard' },
      ],
      statuses: statusOptions(rows, filters.statuses),
      stopReasons: wordOptions(STOP_REASONS, rows.map((r) => servingResponse(r)?.stopReason ?? null), filters.stopReasons),
      turnOrigins: TURN_ORIGINS.map((v) => ({ label: v, value: v })),
    }),
    [accounts.data, catalog.data, filters, guardrails.data, keys.data, pools.data, rows],
  );
  const disabled = { ...notLoadedReason('keys', 'key', keys), ...notLoadedReason('pools', 'pool', pools), ...notLoadedReason('accounts', 'account', accounts), ...notLoadedReason('models', 'model', catalog), ...notLoadedReason('guardrails', 'guardrail', guardrails) };
  const selected = requestId ? rows.find((r) => r.id === requestId) ?? null : null;
  const shown = requestId ? new Set<ColumnId>(COMPACT) : visible;
  const fills = COLUMNS.some((c) => shown.has(c.id) && 'fill' in c && c.fill);
  const detailTo = (id: string) => ({ pathname: requestHref(id), search: location.search });
  const closeDrawer = () => navigate({ pathname: '/requests', search: location.search });

  return (
    <PopoverHost.Provider value={host}>
      <div className={`requests ${requestId ? 'has-drawer' : ''}`}>
        <FilterBar
          address={address}
          count={shownCount(rows, nextCursor, requests.data !== null)}
          countSizer={WIDEST_COUNT}
          dimensions={REQUESTS_DIMENSIONS}
          disabled={disabled}
          filters={filters}
          options={options}
          placeholder="Search id, session, key, account, model, error, message id"
          range={{ now: clock.now, window: span }}
          refresh={{
            disabled: ended ? ENDED_RANGE : undefined,
            intervalMs,
            onIntervalChange: setIntervalMs,
            onRefresh: refresh,
            paused: requestId ? 'request' : (holdCause ?? (cursor !== undefined ? 'olderPage' : null)),
            refreshTitle: rowsRefreshTitle(custom, filters.range.from, custom ? filters.range.to : loadedAt),
          }}
          update={update}
        >
          <ColumnsControl columns={COLUMNS} visible={visible} onChange={setVisible} />
          <SortControl keys={REQUEST_SORT_KEYS} onChange={update} value={filters} />
        </FilterBar>
        <div className="stack">
          {overrun !== null && (
            <div className="callout callout--warning requests__overrun" role="status">
              {FOLLOW_OVERRUN}
            </div>
          )}

          <Card padded={false} className="card--table">
            <div className="table-wrap">
              <table className={`table table--hover requests__table${timeFormat === '12h' ? ' requests__table--12h' : ''}${fills ? '' : ' table--no-fill'}`}>
                <thead>
                  <tr>
                    {shown.has('time') && <th className="col-fill">Time</th>}
                    {shown.has('key') && <th className="col-fit">Key</th>}
                    {shown.has('pool') && <th className="col-fit">Pool</th>}
                    {shown.has('account') && <th className="col-fit">Account</th>}
                    {shown.has('model') && <th className="col-fit">Model</th>}
                    {shown.has('client') && <th className="col-fit">Client</th>}
                    {shown.has('kind') && <th className="col-fit">Kind</th>}
                    {shown.has('transport') && <th className="col-fit">Transport</th>}
                    {shown.has('status') && <th className="col-fit">Status</th>}
                    {shown.has('attempts') && <th className="col-fit" title="How many attempts the request took, the trail on hover">Attempts</th>}
                    {shown.has('ttfb') && <th className="right col-fit">TTFB</th>}
                    {shown.has('duration') && <th className="right col-fit">Duration</th>}
                    {shown.has('tokens') && <th className="right col-fit">Tokens</th>}
                    {shown.has('input') && <th className="right col-fit">Uncached input</th>}
                    {shown.has('output') && <th className="right col-fit">Output tokens</th>}
                    {shown.has('cost') && (
                      <th className="right col-fit" title={costHeaderTitle(unit)}>
                        Cost
                      </th>
                    )}
                    <th className="col-fit" />
                  </tr>
                </thead>
                <tbody>
                  {/* A list read in flight over a stand-in page (the poll's newest page holding the place of a read that
                      failed) keeps the stand-in's rows drawn, so the skeleton rows are the empty table's alone. */}
                  {requests.loading && !requests.data && rows.length === 0
                    ? <SkeletonRows count={8} colSpan={shown.size + 1} />
                    : rows.map((r) => (
                        <tr
                          key={r.id}
                          className={[r.id === requestId ? 'is-selected' : '', fresh.has(r.id) ? 'is-new' : ''].filter(Boolean).join(' ')}
                          tabIndex={0}
                          {...toggleRow(() => (r.id === requestId ? closeDrawer() : navigate(detailTo(r.id))))}
                        >
                          {shown.has('time') && (
                            <td className="num requests__time col-fill" title={fmtAbsolute(r.at)}>
                              {fmtDatedTime(r.at)}
                              <span className="sub mono" title={r.id}>{r.id}</span>
                            </td>
                          )}
                          {shown.has('key') && <td className="mono col-fit">{r.keyId ?? '-'}</td>}
                          {shown.has('pool') && <td className="mono col-fit">{r.poolId ?? '-'}</td>}
                          {shown.has('account') && <td className="col-fit">{r.accountId ?? '-'}</td>}
                          {shown.has('model') && (
                            <td className="mono nowrap col-fit" title={modelTitle(r.model)}>
                              {r.model === '' ? <span className="muted">-</span> : shortModel(r.model)}
                            </td>
                          )}
                          {shown.has('client') && (
                            <td className="mono requests__client col-fit" title={r.client ?? undefined}>
                              {r.client === null ? <span className="muted">-</span> : <span className="requests__client-text ellipsis" title={r.client}>{r.client}</span>}
                              {r.agentType && (
                                <span className="sub ellipsis" title={r.agentType}>
                                  {r.agentType}
                                </span>
                              )}
                            </td>
                          )}
                          {shown.has('kind') && <td className="col-fit">{words(r.kind)}</td>}
                          {shown.has('transport') && (
                            <td className="col-fit">
                              <Badge tone={r.transport === 'sse' ? 'accent' : 'neutral'}>{transportLabel(r)}</Badge>
                            </td>
                          )}
                          {shown.has('status') && (
                            <td className="col-fit">
                              <span className="requests__status">
                                <StatusBadge status={r.status} />
                                {servingResponse(r)?.stopReason === 'refusal' && (
                                  <Badge tone="warning" title={UPSTREAM_REFUSAL_TITLE}>
                                    {servingResponse(r)?.refusalCategory ?? 'none'}
                                  </Badge>
                                )}
                                {r.fallbackDigest !== null && (
                                  <Badge tone="info" title={fallbackTitle(r.fallbackDigest, issuedBy)}>
                                    fallback
                                  </Badge>
                                )}
                                {r.outcome !== 'succeeded' && <span className="muted requests__outcome">{REQUEST_OUTCOME_LABEL[r.outcome]}</span>}
                              </span>
                            </td>
                          )}
                          {shown.has('attempts') && (
                            <td className="col-fit">
                              <span className="requests__attempts">
                                <AttemptCount attempts={r.attempts} />
                                {r.held && <HeldBadge held={r.held} />}
                              </span>
                            </td>
                          )}
                          {shown.has('ttfb') && <td className="right num col-fit">{fmtMs(r.ttfbMs)}</td>}
                          {shown.has('duration') && <td className="right num col-fit">{fmtMs(r.durationMs)}</td>}
                          {shown.has('tokens') && (
                            <td className="right num col-fit">
                              {fmtCompact(tokensTotal(r.usage))}
                              <span className="sub">{r.usage.cacheRead > 0 ? `${fmtCompact(r.usage.cacheRead)} cached` : 'uncached'}</span>
                            </td>
                          )}
                          {shown.has('input') && <OrderedTokensCell kind="input" request={r} />}
                          {shown.has('output') && <OrderedTokensCell kind="output" request={r} />}
                          {shown.has('cost') && <CostCell context={context} request={r} unit={unit} />}
                          <td className="right col-fit">
                            <span className="requests__links">
                              <Link to={routingLogHref(r, now)}>
                                Routing
                              </Link>
                              <Link to={detailTo(r.id)}>
                                Details
                              </Link>
                            </span>
                          </td>
                        </tr>
                      ))}
                  {!requests.loading && rows.length === 0 && (
                    <tr className="table__detail">
                      <td colSpan={shown.size + 1}>
                        {error ? (
                          <ErrorState title="Could not load the request log" body={error} retry={requests.reload} />
                        ) : (
                          <EmptyState title="No requests match" body="Widen the range or clear a filter." action={<Button onClick={reset}>Reset filters</Button>} />
                        )}
                      </td>
                    </tr>
                  )}
                </tbody>
              </table>
            </div>
            <footer className="pager">
              <label className="row">
                <span className="muted">Rows</span>
                <Select value={filters.rows} onChange={(e) => update({ rows: Number(e.target.value) })}>
                  {REQUEST_PAGE_SIZES.map((n) => (
                    <option key={n} value={n}>
                      {n}
                    </option>
                  ))}
                </Select>
              </label>
              <PagerNav atFirst={stack.length === 0} atLast={nextCursor === null} onFirst={resetPaging} onNext={goNext} onPrevious={goPrevious} />
            </footer>
          </Card>
        </div>

        {requestId && (
          <Descriptions>
            <RequestDrawer context={context} request={selected} requestId={requestId} guardrails={guardrails.data} now={now} rows={filters.rows} settings={settings.data} unit={unit} onClose={closeDrawer} />
          </Descriptions>
        )}
      </div>
    </PopoverHost.Provider>
  );
}

function RequestDrawer({ context, guardrails, request, requestId, now, onClose, rows, settings, unit }: { context: CostContext; guardrails: Guardrail[] | null; now: number; onClose: () => void; request: RequestLog | null; requestId: string; rows: number; settings: Settings | null; unit: UnitPosition }) {
  // The drawer reads the request on every open, and draws the list's own row whenever the page holds it.
  const detail = useAsync(() => api.getRequest(requestId), [requestId]);
  // A load that is pending or has failed leaves the answer to the request before it in hand, so only an answer
  // for this id is read.
  const fetched = detail.data?.request.id === requestId ? detail.data : null;
  const r = request ?? fetched?.request ?? null;
  const bodies = useAsync<CapturedBodies | null>(() => (r?.capturedBodies ? api.getCapturedBodies(r.id) : Promise.resolve(null)), [r?.id]);
  // A load that is pending or has failed leaves the answer to the request before it in hand; prompts and
  // responses belong to the row they were read for and to no other, so that answer is never drawn here.
  const otherRequestsBodies = bodies.data !== null && bodies.data?.requestId !== r?.id;
  useEffect(() => {
    // A dialog opened from the drawer takes Escape for itself. The drawer closes on the next one.
    const onKey = (e: KeyboardEvent) => e.key === 'Escape' && document.querySelector('dialog[open]') === null && onClose();
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);
  const serving = r ? servingAttempt(r) : null;
  const answer = serving ? answerChips(serving.response, serving.serviceTier) : [];

  return (
    <aside className="drawer" aria-label="Request details">
      <header className="drawer__head">
        <div>
          <h2 className="drawer__title mono" title={r?.id ?? requestId}>
            {shortId(r?.id ?? requestId, HEAD_ID_CHARS)}
          </h2>
          {r && (
            <p className="muted" title={fmtAbsolute(r.at)}>
              {fmtTime(r.at)}, {fmtRelative(r.at, now)}
            </p>
          )}
          {/* Each link writes a fresh Requests address, which leaves this request's route, so the drawer closes on the
              widened list. */}
          {r && (
            <p className="drawer__widen muted">
              Widen to
              {requestsWiden(r, now, rows).map((w) => (
                <Link key={w.label} to={w.href} title={w.title ?? undefined}>
                  {w.label}
                </Link>
              ))}
            </p>
          )}
        </div>
        <div className="drawer__actions">
          {r && (
            <Link className="btn btn--secondary btn--sm" to={routingLogHref(r, now)}>
              Routing log
            </Link>
          )}
          <button type="button" className="dialog__close" onClick={onClose} aria-label="Close details">
            <IconX size={16} />
          </button>
        </div>
      </header>
      {!r ? (
        detail.loading ? (
          <Skeleton height={200} />
        ) : detail.error !== null ? (
          <ErrorState title="Could not load the request" body={detail.error} retry={detail.reload} />
        ) : (
          <EmptyState title="Request not found" body="It may have aged out of the ledger." />
        )
      ) : (
        <div className="drawer__body">
          <section>
            <h3 className="section-title">Outcome</h3>
            <KeyValue
              rows={[
                { label: 'Status', value: <StatusBadge status={r.status} /> },
                { label: 'Outcome', value: REQUEST_OUTCOME_LABEL[r.outcome] },
                { label: 'Account', value: r.accountId === null ? '-' : <Link to={accountHref(r.accountId)}>{r.accountId}</Link> },
                { label: 'Model', value: r.model === '' ? <span className="muted">-</span> : r.model, mono: r.model !== '' },
                ...(r.canonicalModel !== r.model ? [{ label: 'Canonical model', value: r.canonicalModel, mono: true }] : []),
                { label: 'TTFB', value: fmtMs(r.ttfbMs) },
                { label: 'Duration', value: fmtMs(r.durationMs) },
                // `mono` sets the whole value's face, so the code takes its own mono span where a cut's words follow.
                {
                  label: 'Error',
                  value: !r.errorCode ? <span className="muted">none</span> : fetched?.streamCut ? <><span className="mono">{r.errorCode}</span>: {fetched.streamCut.text}</> : r.errorCode,
                  mono: Boolean(r.errorCode) && !fetched?.streamCut,
                },
                ...(r.clientDrift ? [{ label: 'Client drift', value: r.clientDrift, mono: true }] : []),
                ...(r.clientContract ? [{ label: 'Client contract', value: r.clientContract, mono: true }] : []),
                ...(serving
                  ? [
                      { label: 'Answer', value: answer.length ? <Chips chips={answer} /> : <span className="muted">none</span> },
                      { label: 'Cache', value: <CacheValue attempt={serving} usage={r.usage} /> },
                      { label: 'Diagnostics', title: 'Who put diagnostics.previous_message_id on the body the upstream compared', value: <DiagnosticsValue source={r.diagnosticsSource} /> },
                    ]
                  : []),
              ]}
            />
          </section>

          {r.guardrails.length > 0 && (
            <section>
              <h3 className="section-title">Guardrails</h3>
              <KeyValue
                rows={[...r.guardrails]
                  .sort((a, b) => a.position - b.position)
                  .map((m) => ({ label: guardrailLabel(m.id, guardrails), value: m.blocked ? <Badge tone="critical">blocked</Badge> : <Badge tone="neutral">observed</Badge> }))}
              />
            </section>
          )}

          <section>
            <h3 className="section-title">Phases</h3>
            <PhaseWaterfall phases={r.phases} />
          </section>

          <section>
            <h3 className="section-title">Usage</h3>
            <TokenBar usage={r.usage} />
            <table className="table usage-table">
              <tbody>
                <tr>
                  <td>Input (uncached)</td>
                  <td className="right num">{fmtInt(r.usage.input)}</td>
                </tr>
                <tr>
                  <td>Cache read</td>
                  <td className="right num">{fmtInt(r.usage.cacheRead)}</td>
                </tr>
                <tr>
                  <td>Cache write, 5m</td>
                  <td className="right num">{fmtInt(r.usage.cacheWrite5m)}</td>
                </tr>
                <tr>
                  <td>Cache write, 1h</td>
                  <td className="right num">{fmtInt(r.usage.cacheWrite1h)}</td>
                </tr>
                <tr>
                  <td>Output, thinking</td>
                  <td className="right num">{fmtInt(r.usage.thinking)}</td>
                </tr>
                <tr>
                  <td>Output, plain</td>
                  <td className="right num">{fmtInt(plainOutputOf(r.usage))}</td>
                </tr>
                <tr>
                  <td>Server tool calls</td>
                  <td className="right num">
                    {fmtInt(r.usage.webSearch)} search, {fmtInt(r.usage.webFetch)} fetch
                  </td>
                </tr>
                <CostRows context={context} request={r} settings={settings} unit={unit} />
              </tbody>
            </table>
          </section>

          <section>
            <h3 className="section-title">Request</h3>
            <KeyValue
              rows={[
                { label: 'Session', value: r.sessionId ?? '-', mono: true },
                { label: 'Agent', value: r.agentId ?? '-', mono: true },
                {
                  label: 'Client',
                  // The class badge sits in the row, so only the client text takes the mono face; the badge keeps
                  // the face every other badge in the drawer has.
                  value: (
                    <>
                      <span className="mono">{r.client ?? '-'}</span>
                      {r.requestClass !== null && (
                        <>
                          {' '}
                          <Badge title="The request class the client sent">{r.requestClass}</Badge>
                        </>
                      )}
                      {r.forkedFrom !== null && (
                        <div className="mono secondary" title={`fork of ${r.forkedFrom}`}>
                          {forkLabel(r.forkedFrom, r.forkDepth)}
                        </div>
                      )}
                    </>
                  ),
                },
                { label: 'User agent', value: r.userAgent ? shortAgent(r.userAgent) : '-', mono: true, title: r.userAgent ?? undefined },
                { label: 'Key', value: r.keyId ?? '-', mono: true },
                { label: 'Pool', value: r.poolId ?? '-', mono: true },
                { label: 'Kind', value: `${words(r.kind)}, ${transportLabel(r)}` },
                ...r.attempts.flatMap((a) => {
                  const ids = upstreamIdChips(a);
                  return ids.length ? [{ label: `Attempt ${a.index + 1}`, value: <Chips chips={ids} /> }] : [];
                }),
              ]}
            />
          </section>

          <section>
            <h3 className="section-title">Request shape</h3>
            <KeyValue
              rows={[
                { label: 'Effort', value: r.shape.effort ?? <span className="muted">not sent</span>, mono: Boolean(r.shape.effort) },
                { label: 'Thinking', value: r.shape.thinkingType ? `${r.shape.thinkingType}${r.shape.thinkingBudget !== null ? `, budget ${fmtInt(r.shape.thinkingBudget)}` : ''}` : <span className="muted">not sent</span> },
                { label: 'Max tokens', value: r.shape.maxTokens !== null ? fmtInt(r.shape.maxTokens) : <span className="muted">not sent</span> },
                { label: 'Tools, system blocks, messages', value: `${fmtInt(r.shape.toolCount)}, ${fmtInt(r.shape.systemBlocks)}, ${fmtInt(r.shape.messageCount)}` },
                { label: 'Context edits', value: r.shape.contextEdits.length ? r.shape.contextEdits.join(', ') : <span className="muted">none</span>, mono: r.shape.contextEdits.length > 0 },
                { label: 'Temperature', value: r.shape.temperature !== null ? String(r.shape.temperature) : <span className="muted">not sent</span> },
                { label: 'Speed, service tier', value: `${r.shape.requestedSpeed ?? 'default'}, ${r.shape.requestedServiceTier ?? 'default'}` },
                { label: 'Auto mode', value: autoModeLabel(r.autoMode), mono: false },
                { label: 'Classifier', title: r.classifier === 'client_off' ? "The client's own auto-mode classifier call, run off the account its judged turn ended on or with no turn to judge, so it is charged as any call" : "The client's own auto-mode classifier call, which takes no share of a plan window", value: r.classifier ?? <span className="muted">none</span>, mono: r.classifier !== null },
              ]}
            />
          </section>

          <section>
            <h3 className="section-title">Client hints</h3>
            <KeyValue
              rows={[
                { label: 'Agent type', value: r.agentType ?? <span className="muted">not sent</span>, mono: Boolean(r.agentType) },
                { label: 'Compaction', value: r.compaction ?? <span className="muted">not sent</span>, mono: Boolean(r.compaction) },
                { label: 'Context compacted', value: r.contextCompacted ?? <span className="muted">not sent</span>, mono: Boolean(r.contextCompacted) },
                { label: 'Turn origin', value: r.turnOrigin ?? <span className="muted">not sent</span>, mono: Boolean(r.turnOrigin) },
                { label: 'Tool time before this turn', value: r.prevToolMs !== null ? fmtMs(r.prevToolMs) : <span className="muted">not sent</span> },
              ]}
            />
          </section>

          <section>
            <h3 className="section-title">Captured bodies</h3>
            {!r.capturedBodies ? (
              <p className="muted">Not captured for this key. Enable body capture on the key to record the exchange.</p>
            ) : bodies.error ? (
              <ErrorState title="Could not load the captured bodies" body={bodies.error} retry={bodies.reload} />
            ) : bodies.loading || otherRequestsBodies ? (
              <Skeleton height={120} />
            ) : bodies.data ? (
              <div className="bodies">
                {/* Keyed on the request, so the next request's block starts with its dialog closed. */}
                <BodyBlock key={`request ${r.id}`} label="Request" requestId={r.id} value={bodies.data.request} />
                {bodies.data.response === null ? (
                  // A capture without a response byte has nothing to open or copy, so the block says why there is none.
                  // A request no account was tried for was answered by the router itself.
                  <div className="body-block">
                    <div className="body-block__head">
                      <span>Response</span>
                    </div>
                    <p className="muted">{r.attempts.length === 0 ? 'No response: the router refused the request before an account answered.' : 'No response was captured.'}</p>
                  </div>
                ) : (
                  <BodyBlock key={`response ${r.id}`} label="Response" requestId={r.id} value={bodies.data.response} dropped={bodies.data.responseTruncated ? bytesDropped(bodies.data.responseBytesSeen, bodies.data.responseBytesStored) : undefined} />
                )}
              </div>
            ) : (
              <p className="muted">Bodies for this request have aged out of retention.</p>
            )}
          </section>
        </div>
      )}
    </aside>
  );
}

/**
 * The answer's facts as the serving attempt's response named them, each its own chip, so no value has to be read out
 * of a joined line.
 */
function answerChips(response: AttemptResponse, serviceTier: string | null): Chip[] {
  const chips: Chip[] = [];
  if (response.stopReason) chips.push({ value: `stopped: ${response.stopReason}` });
  if (response.speed) chips.push({ title: "The answer's speed", value: response.speed });
  if (response.inferenceGeo) chips.push({ label: 'geography', title: GEOGRAPHY_TITLE, value: response.inferenceGeo === 'not_available' ? 'not available' : response.inferenceGeo });
  if (response.servedModel) chips.push({ mono: true, value: `served by ${response.servedModel}` });
  if (response.clearedInputTokens !== null && response.clearedInputTokens > 0) chips.push({ value: `${fmtInt(response.clearedInputTokens)} tokens cleared by context edits` });
  if (response.refusalCategory) chips.push({ value: `category: ${response.refusalCategory}` });
  if (response.refusalRecommendedModel) chips.push({ mono: true, value: `try ${response.refusalRecommendedModel}` });
  if (serviceTier) chips.push({ title: "The answer's service tier", value: `tier ${serviceTier}` });
  return chips;
}

/**
 * The upstream's own ids for one attempt: its request id, and the message id of an answer that carried one. The ids'
 * own `req_` and `msg_` prefixes name them, so a chip carries no label that would push it past the 400 px drawer.
 */
const upstreamIdChips = (a: Attempt): Chip[] => [
  ...(a.upstreamRequestId === null ? [] : [{ mono: true, title: "The upstream's request id", value: a.upstreamRequestId }]),
  ...(a.upstreamMessageId === null ? [] : [{ mono: true, title: "The upstream's message id", value: a.upstreamMessageId }]),
];

/**
 * The serving attempt's cache diagnostics. The usage is the request's: diagnostics and the token counts both come off
 * the response the final attempt drew.
 */
function CacheValue({ attempt, usage }: { attempt: Attempt; usage: Usage }) {
  const reason = attempt.cacheMissReason;
  const detail =
    reason === null
      ? 'no response to read'
      : reason === 'none'
        ? `${fmtCompact(usage.cacheRead)} read, ${fmtCompact(usage.cacheWrite5m + usage.cacheWrite1h)} written`
        : isDivergence(reason)
          ? `diverged at ${reason.slice(0, -'_changed'.length)}`
          : '';
  return (
    <>
      <span className={reason === null || reason === 'not_requested' ? 'muted' : undefined}>{reason === null ? 'n/a' : missLabel(reason)}</span>
      {detail && <span className="muted">, {detail}</span>}
    </>
  );
}

function DiagnosticsValue({ source }: { source: DiagnosticsSource | null }) {
  if (source === 'client') return <>sent by the client</>;
  if (source === 'router') return <>added by the router</>;
  return <span className="muted">none</span>;
}

/** A reader of `read`'s value that runs it once, the first time the value is asked for. */
function once<T>(read: () => T): () => T {
  let held: { value: T } | null = null;
  return () => (held ??= { value: read() }).value;
}

function BodyBlock({ dropped, label, requestId, value }: { dropped?: number; label: string; requestId: string; value: unknown }) {
  const [wide, setWide] = useState(false);
  // Indenting a large body is a long serialization and a far longer text, so the drawer draws no body and the indent
  // runs when the body is first opened wide or copied. The memo keeps it while the drawer shows this body.
  const textOf = useMemo(() => once(() => bodyText(value)), [value]);
  const linesOf = useMemo(() => once(() => bodyLines(textOf())), [textOf]);
  // One state for the drawer's group and the dialog's. An empty term asks for no lines, so only a typed term indents.
  const [find, setFind] = useState({ index: 0, step: 0, term: '' });
  const hits = useMemo(() => (find.term === '' ? [] : findHits(linesOf(), find.term)), [find.term, linesOf]);
  const index = Math.min(find.index, hits.length - 1);
  const active = hits.length === 0 ? null : hits[index];
  // The dialog leaves the tree while it is open, so the browser's close steps never hand the focus back to the control
  // that opened it. The link takes it as the dialog goes, or the drawer's field where a step opened the dialog.
  const link = useRef<HTMLButtonElement>(null);
  const drawerFind = useRef<HTMLInputElement>(null);
  const dialogFind = useRef<HTMLInputElement>(null);
  const fromField = useRef(false);
  const wasWide = useRef(false);
  // A child's effects run before its parent's, so the dialog's showModal has run and its own focus is taken over here.
  useEffect(() => {
    if (wide) {
      wasWide.current = true;
      if (fromField.current) dialogFind.current?.focus();
    } else if (wasWide.current) {
      wasWide.current = false;
      (fromField.current ? drawerFind.current : link.current)?.focus();
      fromField.current = false;
    }
  }, [wide]);
  // A step from the drawer opens the dialog on the active hit and leaves it there, so a fresh term opens on its first.
  const step = (by: number, inDialog: boolean) => {
    if (hits.length === 0) return;
    if (!inDialog && !wide) {
      fromField.current = true;
      setWide(true);
      return;
    }
    setFind((f) => ({ ...f, index: (Math.min(f.index, hits.length - 1) + by + hits.length) % hits.length, step: f.step + 1 }));
  };
  const findGroup = (at: typeof drawerFind, inDialog: boolean) => (
    <div className="body-block__find">
      <Input
        ref={at}
        placeholder="Find"
        aria-label={`Find in the ${label} body`}
        value={find.term}
        onChange={(e) => setFind({ index: 0, step: 0, term: e.target.value })}
        onKeyDown={(e) => {
          if (e.key === 'Enter') step(e.shiftKey ? -1 : 1, inDialog);
          else if (e.key === 'Escape' && find.term !== '') {
            // The dialog's cancel is the key's default action and the drawer's Escape listens on the window, so the key
            // that clears a term goes no further.
            e.preventDefault();
            e.stopPropagation();
            setFind({ index: 0, step: 0, term: '' });
          }
        }}
      />
      <span className="body-block__find-actions">
        {find.term !== '' && <span className={hits.length === 0 ? 'muted' : undefined}>{hits.length === 0 ? 'No match' : `${index + 1} of ${hits.length}`}</span>}
        <Button size="sm" variant="ghost" icon={<IconChevronLeft size={14} />} aria-label="Previous hit" disabled={hits.length === 0} onClick={() => step(-1, inDialog)} />
        <Button size="sm" variant="ghost" icon={<IconChevronRight size={14} />} aria-label="Next hit" disabled={hits.length === 0} onClick={() => step(1, inDialog)} />
        {find.term !== '' && (
          <Button
            size="sm"
            variant="ghost"
            icon={<IconX size={14} />}
            aria-label="Clear find"
            onClick={() => {
              setFind({ index: 0, step: 0, term: '' });
              at.current?.focus();
            }}
          />
        )}
      </span>
    </div>
  );
  return (
    <div className="body-block">
      <div className="body-block__head">
        <span>{label}</span>
        {dropped !== undefined && <span className="muted">cut at 8 MiB, {fmtInt(dropped)} bytes dropped</span>}
        <span className="body-block__actions">
          <button
            ref={link}
            type="button"
            className="link-btn"
            onClick={() => {
              fromField.current = false;
              setWide(true);
            }}
          >
            Open wide
          </button>
          <CopyButton text={textOf} label="Copy all" />
        </span>
      </div>
      {findGroup(drawerFind, false)}
      {/* Mounted only while open, so a closed dialog draws no rows. */}
      {wide && (
        <Dialog
          open={wide}
          onClose={() => setWide(false)}
          title={`${label} of ${shortId(requestId, HEAD_ID_CHARS)}`}
          width={1040}
          className="dialog--wide dialog--fixed"
          footer={<CopyButton text={textOf} label="Copy all" />}
        >
          {findGroup(dialogFind, true)}
          <BodyViewer find={{ active, step: find.step, term: find.term }} highlight lines={linesOf()} />
        </Dialog>
      )}
    </div>
  );
}
