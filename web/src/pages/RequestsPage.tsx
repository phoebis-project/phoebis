import { useEffect, useMemo, useRef, useState } from 'react';
import { Link, useLocation, useNavigate, useParams } from 'react-router-dom';
import { api } from '@/api';
import { iso, REQUEST_OUTCOME_VALUES } from '@/api/decode';
import type { ApiKey, Attempt, AttemptResponse, CapturedBodies, DiagnosticsSource, Guardrail, RequestLog, RequestOutcome, RequestPage, RequestQuery, Settings, Usage } from '@/api/types';
import { useAppSettings } from '@/app/settings';
import { useFollow } from '@/app/useFollow';
import { BodyViewer } from '@/components/BodyViewer';
import { Chips } from '@/components/ChipView';
import { FilterBar } from '@/components/FilterBar';
import { PhaseWaterfall } from '@/components/PhaseWaterfall';
import { TokenBar } from '@/components/TokenBar';
import { AttemptCount } from '@/components/ui/AttemptTrail';
import { ColumnsControl } from '@/components/ui/ColumnsControl';
import type { Option } from '@/components/ui/OptionList';
import { IconChevronLeft, IconChevronRight, IconChevronsLeft, IconX } from '@/components/ui/icons';
import { Badge, Button, Card, CopyButton, Descriptions, Dialog, EmptyState, ErrorState, KeyValue, Select, Skeleton } from '@/components/ui/primitives';
import { addressWindow, RANGE_PRESETS, REQUESTS_CODEC, type RequestsAddress } from '@/lib/address';
import { autoModeLabel } from '@/lib/autoMode';
import { bodyLines, bodyText } from '@/lib/bodies';
import { CACHE_MISS_REASONS, isDivergence, missLabel } from '@/lib/cache';
import { AGENT_TYPES, TURN_ORIGINS } from '@/lib/client';
import { clock } from '@/lib/clock';
import { EFFORT_LEVELS, foldRequestClass, NO_CLASS, NO_EFFORT, OTHER_DIMENSION } from '@/lib/dimensions';
import { notLoadedReason, REQUESTS_DIMENSIONS } from '@/lib/filterBar';
import { effectiveInterval, FOLLOW_OVERRUN, type Paging, pagingFor, pollQuery, readEnd, spanEnded } from '@/lib/follow';
import { bytesDropped, earlierHolderLabel, fmtAbsolute, fmtCompact, fmtDatedTime, fmtEquivalent, fmtInt, fmtMs, fmtRelative, fmtTime, fmtUsd, shareLabel, shortAgent, shortId, shortModel, tokensTotal, withEstimate } from '@/lib/format';
import { guardrailLabel, guardrailOptions } from '@/lib/guardrails';
import { useAsync, useNow, useQueryFilters, useRangeTakenAt } from '@/lib/hooks';
import { planLabel } from '@/lib/quota';
import { costCellOf, costHeaderTitle, type CostUnit, costUnitOf, FREE_SHARE_TITLE, FUNDING_WORD, heldWords, NO_SHARE_TITLE, revisionsTitle } from '@/lib/requests';
import { requestsWiden } from '@/lib/requestsLink';
import { type Chip, logHref, requestLink } from '@/lib/routingLog';
import type { UnitPosition } from '@/lib/unit';
import { StatusBadge } from './DashboardPage';

// `fill` marks the columns the table draws `col-fill`, so the table takes its content width when none of them shows.
const COLUMNS = [
  { id: 'time', label: 'Time' },
  { id: 'key', label: 'Key' },
  { id: 'pool', label: 'Pool' },
  { id: 'account', label: 'Account' },
  { id: 'model', label: 'Model' },
  { fill: true, id: 'client', label: 'Client' },
  { id: 'kind', label: 'Kind' },
  { id: 'transport', label: 'Transport' },
  { id: 'status', label: 'Status' },
  { id: 'attempts', label: 'Attempts' },
  { id: 'ttfb', label: 'TTFB' },
  { id: 'tokens', label: 'Tokens' },
  { id: 'cost', label: 'Cost' },
] as const;
type ColumnId = (typeof COLUMNS)[number]['id'];
// The drawer names all three, and leaving them out lets the default set fit the card at 1280 by 800.
const HIDDEN_BY_DEFAULT: readonly ColumnId[] = ['kind', 'pool', 'transport'];
/** The columns the table shows while the drawer is open: derived at render, so the chosen set returns on close. */
const COMPACT: readonly ColumnId[] = ['time', 'model', 'status', 'attempts'];

const PAGE_SIZES = [10, 25, 50, 100];
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
/** The words the ledger records an outcome under; one the address carries that names no outcome is dropped. */
const outcomesOf = (values: string[]): RequestOutcome[] => values.flatMap((v) => REQUEST_OUTCOME_VALUES.filter((o) => o === v));
/**
 * The known reasons in their reading order, then every other name the loaded rows' serving attempts or the address
 * carry, alphabetically, so a link naming a reason the console has no label for still shows as chosen.
 */
const missReasonOptions = (rows: RequestLog[], selected: string[]) => {
  const served = rows.flatMap((r) => r.attempts[r.attempts.length - 1]?.cacheMissReason ?? []);
  const others = new Set([...served, ...selected].filter((name) => !CACHE_MISS_REASONS.includes(name)));
  return [...CACHE_MISS_REASONS, ...[...others].sort()].map((name) => ({ label: missLabel(name), value: name }));
};
/**
 * The classes the loaded rows and the address carry as the rollups fold them, alphabetically, then a request that sent
 * no class under the label the Analytics page gives it, then the classes the fold reads as `other`.
 */
const requestClassOptions = (rows: RequestLog[], selected: string[]) => {
  const classes = new Set([...rows.map((r) => foldRequestClass(r.requestClass)), ...selected].filter((c) => c !== NO_CLASS && c !== OTHER_DIMENSION));
  return [...[...classes].sort().map((c) => ({ label: c, value: c })), { label: 'unattributed', value: NO_CLASS }, { label: OTHER_DIMENSION, value: OTHER_DIMENSION }];
};

/**
 * Every filter, the range and the page size, which the page's address holds so a reload or a shared link keeps them,
 * with the page size as a number and the outcomes and status codes the page could have written.
 */
type Filters = Omit<RequestsAddress, 'outcomes' | 'rows'> & { outcomes: RequestOutcome[]; rows: number };

const DEFAULTS = REQUESTS_CODEC.fromParams(new URLSearchParams());

/** Keyed by every outcome the ledger records, so one it gains is a type error until it is named here. */
const REQUEST_OUTCOME_LABEL: Record<RequestOutcome, string> = {
  canceled: 'canceled',
  exhausted: 'exhausted',
  failed: 'failed',
  incomplete: 'incomplete',
  rejected: 'rejected',
  succeeded: 'succeeded',
};


/**
 * The router's ids run to 44 characters, which would wrap the drawer head to a second line beside its two actions at
 * 400 px, so the head shows this many and its hover keeps the id whole.
 */
const HEAD_ID_CHARS = 24;
const GEOGRAPHY_TITLE = 'The inference geography the upstream reported for the answer: global or us. Not available means the model does not take the inference geography parameter.';

/** The log on this request, a quarter hour either side of it. A row with no duration is read as a moment. */
const routingLogHref = (r: RequestLog, now: number) => {
  const at = Date.parse(r.at);
  return logHref(requestLink(r.id, at, r.durationMs === null ? null : at + r.durationMs, now));
};

/** The Cost cell: the position's main line over its second, every figure and the dollar equivalent on the title. */
function CostCell({ request, unit }: { request: RequestLog; unit: UnitPosition }) {
  const cell = costCellOf(costUnitOf(request, unit));
  return (
    <td className="right num col-fit" title={cell.title || undefined}>
      {cell.main}
      {cell.sub && <span className="sub">{cell.sub}</span>}
    </td>
  );
}

/**
 * The drawer's cost row: the equivalent or share chips with the dollar equivalent muted after them, the dollars, the
 * dollar equivalent with `equivalent` muted after it, or the dash of a position that reads nothing for the request.
 * An estimated figure reads a tilde before its first value, and the row's title says why.
 */
function costRowValue(unit: CostUnit) {
  const mark = unit.estimated ? '~' : '';
  const chipsOf = (figures: { key: string; share: number }[], label: (key: string, share: number) => string) => figures.map(({ key, share }, i) => ({ value: `${i === 0 ? mark : ''}${label(key, share)}` }));
  switch (unit.kind) {
    case 'equivalent':
      return (
        <span className="usage-table__end">
          <Chips chips={chipsOf(unit.equivalents, fmtEquivalent)} />
          <span className="muted">{fmtUsd(unit.usd)} equivalent</span>
        </span>
      );
    case 'shares':
      return (
        <span className="usage-table__end">
          <Chips chips={chipsOf(unit.shares, shareLabel)} />
          <span className="muted">{fmtUsd(unit.usd)} equivalent</span>
        </span>
      );
    case 'dash':
      return <b>-</b>;
    case 'dollars':
      return (
        <b>
          {mark}
          {fmtUsd(unit.usd)}
        </b>
      );
    case 'equivalent_dollars':
    case 'unpriced':
      return (
        <b>
          {mark}
          {fmtUsd(unit.usd)} <span className="muted">equivalent</span>
        </b>
      );
    case 'unserved':
      return (
        <b>
          {mark}
          {fmtUsd(unit.usd)}
        </b>
      );
  }
}

/**
 * The drawer's cost value under one title: why a dollar figure, a dash or a free call's figures stand where they do,
 * the serving account's tier by its plan, the price list and the cost model in force at the request's instant while
 * the settings are at hand, and the estimate of a figure. A title on an element inside the cell would hide the row's
 * own wherever the figure is hovered.
 */
function CostRowCell({ request: r, settings, unit }: { request: RequestLog; settings: Settings | null; unit: UnitPosition }) {
  const cost = costUnitOf(r, unit);
  const why = (cost.kind === 'shares' || cost.kind === 'equivalent' || cost.kind === 'equivalent_dollars') && cost.reason !== null ? FREE_SHARE_TITLE : cost.kind === 'dollars' ? `Billed in dollars: ${FUNDING_WORD[cost.funding]}` : cost.kind === 'unpriced' ? NO_SHARE_TITLE : cost.kind === 'dash' ? cost.title : null;
  const title = withEstimate([why, r.tier === null ? null : `Account tier ${planLabel(r.tier)}`, revisionsTitle(settings, r.at)].filter((part) => part !== null).join('. '), cost.estimated && cost.kind !== 'dash');
  return (
    <td className="right num" title={title || undefined}>
      {costRowValue(cost)}
    </td>
  );
}

/**
 * The Keys control's options: the live keys, then each lifetime the address names that no live key holds, under its
 * earlier holder's label, so a link from an earlier holder's session reads whole. Until the key list lands no lifetime
 * can be told from a live key's own, so none is listed.
 */
function keyOptions(keys: ApiKey[] | null, selected: string[]): Option[] {
  if (keys === null) return [];
  const live = new Map(keys.map((k) => [k.id, Date.parse(k.createdAt)]));
  const earlier = selected.flatMap((value) => {
    const at = value.lastIndexOf('@');
    if (at < 0) return [];
    const id = value.slice(0, at);
    const generation = Number(value.slice(at + 1));
    return live.get(id) === generation ? [] : [{ label: earlierHolderLabel(id, generation), value }];
  });
  return [...keys.map((k) => ({ label: k.name, value: k.id })), ...earlier];
}

/** The route's order, which a poll's merge keeps: newest first, and the higher id first at one instant. */
const newestFirst = (a: RequestLog, b: RequestLog) => Date.parse(b.at) - Date.parse(a.at) || (a.id < b.id ? 1 : a.id > b.id ? -1 : 0);
/** The route answers its rows as `requests`, and the poll reads a list's as `rows`. */
const listed = (page: RequestPage) => ({ landedThrough: page.landedThrough, nextCursor: page.nextCursor, rows: page.requests });

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
  // The revisions the drawer's cost row names. The cells read no cost model, since the route prices every figure.
  const settings = useAsync(() => api.getSettings(), []);
  const { unit } = useAppSettings();

  const { address, filters: read, replace } = useQueryFilters(REQUESTS_CODEC);
  const filters = useMemo<Filters>(() => ({ ...read, outcomes: outcomesOf(read.outcomes), rows: Number(read.rows), statuses: read.statuses.filter((code) => /^\d+$/.test(code)) }), [read]);
  const [visible, setVisible] = useState<Set<ColumnId>>(() => new Set(COLUMNS.map((c) => c.id).filter((id) => !HIDDEN_BY_DEFAULT.includes(id))));
  // The cursor and the pages before it belong to the address they were read under, so any change of the address (a
  // filter, Reset, a link) drops them in the render that brings it, with no reset of their own to render apart and read
  // the old filters once more without the cursor. The stale entry is replaced in that same render, so changing a
  // filter back does not return to the page left behind under it.
  // `to` holds the range's end a walk from the poll's overrun reads under, the end its cursor came from.
  const [paging, setPaging] = useState<Paging>({ for: address, stack: [] });
  if (paging.for !== address) setPaging({ for: address, stack: [] });
  const { cursor, stack, to: pagedTo } = pagingFor(paging, address);
  // The range ends where the list was loaded, so a cursor page is read against the window its cursor came from;
  // Refresh moves the end to now.
  const [loadedAt, setLoadedAt] = useState(() => clock.now());
  // Off on the logs until the reader picks an interval, and page state, so a link or a reload opens the log off.
  const [intervalMs, setIntervalMs] = useState(0);
  const [barOpen, setBarOpen] = useState(false);
  const rangeTakenAt = useRangeTakenAt(filters.range);
  const update = (patch: Partial<Filters>) => {
    const next = { ...filters, ...patch };
    replace({ ...next, rows: String(next.rows) });
  };

  // A custom range names its own window, which Refresh reads again rather than moving it to now.
  const custom = filters.range.preset === 'custom';
  const span = useMemo(() => addressWindow(filters.range, RANGE_PRESETS, '24h', loadedAt), [filters.range, loadedAt]);
  const ended = spanEnded(custom, span.to, rangeTakenAt);
  const polledMs = effectiveInterval(intervalMs, ended);
  const query = useMemo<RequestQuery>(
    // Probes are off the log unless they are what the filter asks for.
    () => ({ accountIds: filters.accounts, agentTypes: filters.agentTypes, cacheMissReasons: filters.cacheMissReasons, classifiers: filters.classifiers, client: filters.client, cursor, efforts: filters.efforts, from: iso(span.from), guardrails: filters.guardrails, held: filters.held, includeProbes: filters.kind === 'probe', keyIds: filters.keys, kind: filters.kind || null, limit: filters.rows, models: filters.models, outcomes: filters.outcomes, poolIds: filters.pools, request: filters.request || undefined, requestClasses: filters.requestClasses, search: filters.search, session: filters.session || undefined, speeds: filters.speeds, statuses: filters.statuses.map(Number), to: iso(readEnd(custom, span.to, pagedTo ?? span.to)), turnOrigins: filters.turnOrigins, userAgent: filters.agent }),
    [filters, cursor, custom, span, pagedTo],
  );
  const requests = useAsync(() => api.listRequests(query), [query]);
  // The open drawer reads one of the rows, and an open menu or popover under the bar stands over the list, so the poll
  // waits for either to close.
  const { error, fresh, goNext, goPrevious, nextCursor, overrun, rows, setFollowed } = useFollow<RequestLog>({
    address,
    compare: newestFirst,
    error: requests.error,
    identity: (r) => r.id,
    intervalMs: polledMs,
    list: requests.data && listed(requests.data),
    paging,
    paused: Boolean(requestId) || barOpen,
    read: (c, to, landedAfter) => api.listRequests(pollQuery(query, custom, span.to, to, c, landedAfter)).then(listed),
    setPaging,
    stale: requests.stale,
  });

  const resetPaging = () => setPaging({ for: address, stack: [] });
  const refresh = () => {
    resetPaging();
    setFollowed(null);
    setLoadedAt(clock.now());
  };
  // The page size is how the operator reads the log rather than what it narrows to, so Reset filters keeps it.
  const reset = () => replace({ ...DEFAULTS, rows: read.rows });
  const options: Record<string, Option[]> = {
    accounts: (accounts.data ?? []).map((a) => ({ label: a.label, value: a.id })),
    agentTypes: AGENT_TYPES.map((v) => ({ label: v, value: v })),
    cacheMissReasons: missReasonOptions(rows, filters.cacheMissReasons),
    classifiers: [
      { label: 'client classifier', value: 'client' },
      { label: 'none', value: NO_CLASS },
    ],
    efforts: [...EFFORT_LEVELS, NO_EFFORT, OTHER_DIMENSION].map((v) => ({ label: v, value: v })),
    guardrails: guardrailOptions(guardrails.data, rows, filters.guardrails),
    keys: keyOptions(keys.data, filters.keys),
    kind: [
      { label: 'Inference', value: 'inference' },
      { label: 'Count tokens', value: 'count_tokens' },
      { label: 'Probe', value: 'probe' },
    ],
    models: (catalog.data ?? []).map((m) => ({ label: m.id, value: m.id })),
    outcomes: REQUEST_OUTCOME_VALUES.map((o) => ({ label: REQUEST_OUTCOME_LABEL[o], value: o })),
    pools: (pools.data ?? []).map((p) => ({ label: p.name, value: p.id })),
    requestClasses: requestClassOptions(rows, filters.requestClasses),
    speeds: [
      { label: 'fast', value: 'fast' },
      { label: 'standard', value: 'standard' },
    ],
    statuses: statusOptions(rows, filters.statuses),
    turnOrigins: TURN_ORIGINS.map((v) => ({ label: v, value: v })),
  };
  const disabled = { ...notLoadedReason('keys', 'key', keys), ...notLoadedReason('pools', 'pool', pools), ...notLoadedReason('accounts', 'account', accounts), ...notLoadedReason('models', 'model', catalog), ...notLoadedReason('guardrails', 'guardrail', guardrails) };
  const selected = requestId ? rows.find((r) => r.id === requestId) ?? null : null;
  const shown = requestId ? new Set<ColumnId>(COMPACT) : visible;
  const fills = COLUMNS.some((c) => shown.has(c.id) && 'fill' in c && c.fill);

  return (
    <div className={`requests ${requestId ? 'has-drawer' : ''}`}>
      <FilterBar
        address={address}
        count={rows.length > 0 ? `${fmtInt(rows.length)} shown${nextCursor ? ', more available' : ''}` : requests.data === null ? '' : 'No rows'}
        dimensions={REQUESTS_DIMENSIONS}
        disabled={disabled}
        filters={filters}
        onOpenChange={setBarOpen}
        options={options}
        placeholder="Search id, session, key, account, model, error, message id"
        range={{ now: clock.now, window: span }}
        refresh={{
          disabled: ended ? 'A custom range that has ended has nothing new to read' : undefined,
          intervalMs,
          onIntervalChange: setIntervalMs,
          onRefresh: refresh,
          paused: requestId ? 'request' : barOpen ? 'menu' : cursor !== undefined ? 'olderPage' : null,
          refreshTitle: custom ? `Rows from ${fmtTime(filters.range.from)} to ${fmtTime(filters.range.to)}. Refresh reads the same window again` : `Rows up to ${fmtTime(iso(loadedAt))}. Refresh reads up to now`,
        }}
        update={update}
      >
        <ColumnsControl columns={COLUMNS} visible={visible} onChange={setVisible} />
      </FilterBar>
      <div className="stack">
        {overrun !== null && (
          <div className="callout callout--warning requests__overrun" role="status">
            {FOLLOW_OVERRUN}
          </div>
        )}

        <Card padded={false} className="card--table">
          <div className="table-wrap">
            <table className={`table table--hover requests__table${fills ? '' : ' table--no-fill'}`}>
              <thead>
                <tr>
                  {shown.has('time') && <th className="col-fit">Time</th>}
                  {shown.has('key') && <th className="col-fit">Key</th>}
                  {shown.has('pool') && <th className="col-fit">Pool</th>}
                  {shown.has('account') && <th className="col-fit">Account</th>}
                  {shown.has('model') && <th className="col-fit">Model</th>}
                  {shown.has('client') && <th className="col-fill">Client</th>}
                  {shown.has('kind') && <th className="col-fit">Kind</th>}
                  {shown.has('transport') && <th className="col-fit">Transport</th>}
                  {shown.has('status') && <th className="col-fit">Status</th>}
                  {shown.has('attempts') && <th className="col-fit" title="How many attempts the request took, the trail on hover">Attempts</th>}
                  {shown.has('ttfb') && <th className="right col-fit">TTFB</th>}
                  {shown.has('tokens') && <th className="right col-fit">Tokens</th>}
                  {shown.has('cost') && (
                    <th className="right col-fit" title={costHeaderTitle(unit)}>
                      Cost
                    </th>
                  )}
                  <th className="col-fit" />
                </tr>
              </thead>
              <tbody>
                {requests.loading && !requests.data
                  ? Array.from({ length: 8 }, (_, i) => (
                      <tr key={i} className="table__detail">
                        <td colSpan={shown.size + 1}>
                          <Skeleton />
                        </td>
                      </tr>
                    ))
                  : rows.map((r) => (
                      <tr
                        key={r.id}
                        className={[r.id === requestId ? 'is-selected' : '', fresh.has(r.id) ? 'is-new' : ''].filter(Boolean).join(' ')}
                        onClick={() => navigate({ pathname: `/requests/${r.id}`, search: location.search })}
                        tabIndex={0}
                        // A key pressed on a link in the row is the link's own, so Enter on Routing opens the log
                        // alone.
                        onKeyDown={(e) => e.key === 'Enter' && e.target === e.currentTarget && navigate({ pathname: `/requests/${r.id}`, search: location.search })}
                      >
                        {shown.has('time') && (
                          <td className="num col-fit" title={fmtAbsolute(r.at)}>
                            {fmtDatedTime(r.at, now)}
                            <span className="sub mono" title={r.id}>{shortId(r.id, 12)}</span>
                          </td>
                        )}
                        {shown.has('key') && <td className="mono col-fit">{r.keyId ?? '-'}</td>}
                        {shown.has('pool') && <td className="mono col-fit">{r.poolId ?? '-'}</td>}
                        {shown.has('account') && <td className="col-fit">{r.accountId ?? '-'}</td>}
                        {shown.has('model') && (
                          <td className="mono nowrap col-fit" title={shortModel(r.model) === r.model ? undefined : r.model}>
                            {shortModel(r.model)}
                          </td>
                        )}
                        {shown.has('client') && (
                          <td className="mono requests__client col-fill" title={r.client ?? undefined}>
                            {r.client === null ? <span className="muted">-</span> : <span className="requests__client-text" title={r.client}>{r.client}</span>}
                            {r.agentType && (
                              <span className="sub" title={r.agentType}>
                                {r.agentType}
                              </span>
                            )}
                          </td>
                        )}
                        {shown.has('kind') && <td className="col-fit">{r.kind.replace('_', ' ')}</td>}
                        {shown.has('transport') && (
                          <td className="col-fit">
                            <Badge tone={r.transport === 'sse' ? 'accent' : 'neutral'}>{r.transport === 'sse' ? 'SSE' : 'JSON'}</Badge>
                          </td>
                        )}
                        {shown.has('status') && (
                          <td className="col-fit">
                            <span className="requests__status">
                              <StatusBadge status={r.status} />
                              {r.outcome !== 'succeeded' && <span className="muted requests__outcome">{REQUEST_OUTCOME_LABEL[r.outcome]}</span>}
                            </span>
                          </td>
                        )}
                        {shown.has('attempts') && (
                          <td className="col-fit">
                            <span className="requests__attempts">
                              <AttemptCount attempts={r.attempts} />
                              {r.held && (
                                <Badge tone="warning" title={heldWords(r.held).title}>
                                  {heldWords(r.held).badge}
                                </Badge>
                              )}
                            </span>
                          </td>
                        )}
                        {shown.has('ttfb') && <td className="right num col-fit">{fmtMs(r.ttfbMs)}</td>}
                        {shown.has('tokens') && (
                          <td className="right num col-fit">
                            {fmtCompact(tokensTotal(r.usage))}
                            <span className="sub">{r.usage.cacheRead > 0 ? `${fmtCompact(r.usage.cacheRead)} cached` : 'uncached'}</span>
                          </td>
                        )}
                        {shown.has('cost') && <CostCell request={r} unit={unit} />}
                        <td className="right col-fit">
                          <span className="requests__links">
                            <Link to={routingLogHref(r, now)} onClick={(e) => e.stopPropagation()}>
                              Routing
                            </Link>
                            <Link to={{ pathname: `/requests/${r.id}`, search: location.search }} onClick={(e) => e.stopPropagation()}>
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
                        <ErrorState title="Could not load the request log" body={error} action={<Button onClick={requests.reload}>Try again</Button>} />
                      ) : (
                        <EmptyState title="No requests match" body="Widen the date range or clear a filter." action={<Button onClick={reset}>Reset filters</Button>} />
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
                {PAGE_SIZES.map((n) => (
                  <option key={n} value={n}>
                    {n}
                  </option>
                ))}
              </Select>
            </label>
            <div className="pager__nav">
              <button type="button" onClick={resetPaging} disabled={stack.length === 0} aria-label="First page">
                <IconChevronsLeft size={15} />
              </button>
              <button type="button" onClick={goPrevious} disabled={stack.length === 0} aria-label="Previous page">
                <IconChevronLeft size={15} />
              </button>
              <button type="button" onClick={goNext} disabled={nextCursor === null} aria-label="Next page">
                <IconChevronRight size={15} />
              </button>
            </div>
          </footer>
        </Card>
      </div>

      {requestId && (
        <Descriptions>
          <RequestDrawer request={selected} requestId={requestId} guardrails={guardrails.data} now={now} rows={read.rows} settings={settings.data} unit={unit} onClose={() => navigate({ pathname: '/requests', search: location.search })} />
        </Descriptions>
      )}
    </div>
  );
}

function RequestDrawer({ guardrails, request, requestId, now, onClose, rows, settings, unit }: { guardrails: Guardrail[] | null; now: number; onClose: () => void; request: RequestLog | null; requestId: string; rows: string; settings: Settings | null; unit: UnitPosition }) {
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
  // The last attempt is the one whose answer the request ended on.
  const serving = r && r.attempts.length > 0 ? r.attempts[r.attempts.length - 1] : null;
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
                { label: 'Account', value: r.accountId === null ? '-' : <Link to={`/accounts/${r.accountId}`}>{r.accountId}</Link> },
                { label: 'Model', value: r.model, mono: true },
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
                  <td className="right num">{fmtInt(Math.max(0, r.usage.output - r.usage.thinking))}</td>
                </tr>
                <tr>
                  <td>Server tool calls</td>
                  <td className="right num">
                    {fmtInt(r.usage.webSearch)} search, {fmtInt(r.usage.webFetch)} fetch
                  </td>
                </tr>
                <tr>
                  <td>
                    <b>Estimated cost</b>
                  </td>
                  <CostRowCell request={r} settings={settings} unit={unit} />
                </tr>
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
                    </>
                  ),
                },
                { label: 'User agent', value: r.userAgent ? shortAgent(r.userAgent) : '-', mono: true, title: r.userAgent ?? undefined },
                { label: 'Key', value: r.keyId ?? '-', mono: true },
                { label: 'Pool', value: r.poolId ?? '-', mono: true },
                { label: 'Kind', value: `${r.kind.replace('_', ' ')}, ${r.transport === 'sse' ? 'SSE' : 'JSON'}` },
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
                { label: 'Classifier', title: "The client's own auto-mode classifier call, which takes no share of a plan window", value: r.classifier ?? <span className="muted">none</span>, mono: r.classifier !== null },
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
              <ErrorState title="Could not load the captured bodies" body={bodies.error} action={<Button onClick={bodies.reload}>Try again</Button>} />
            ) : bodies.loading || otherRequestsBodies ? (
              <Skeleton height={120} />
            ) : bodies.data ? (
              <div className="bodies">
                {/* Keyed on the request, so the next request's block starts with its dialog closed. */}
                <BodyBlock key={`request ${r.id}`} label="Request" requestId={r.id} value={bodies.data.request} dropped={bodies.data.requestTruncated ? bytesDropped(bodies.data.requestBytesSeen, bodies.data.requestBytesStored) : undefined} />
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
  // The dialog leaves the tree while it is open, so the browser's close steps never hand the focus back to the control
  // that opened it. The link takes it as the dialog goes.
  const link = useRef<HTMLButtonElement>(null);
  const wasWide = useRef(false);
  useEffect(() => {
    if (wide) wasWide.current = true;
    else if (wasWide.current) {
      wasWide.current = false;
      link.current?.focus();
    }
  }, [wide]);
  return (
    <div className="body-block">
      <div className="body-block__head">
        <span>{label}</span>
        {dropped !== undefined && <span className="muted">cut at 8 MiB, {fmtInt(dropped)} bytes dropped</span>}
        <span className="body-block__actions">
          <button ref={link} type="button" className="link-btn" onClick={() => setWide(true)}>
            Open wide
          </button>
          <CopyButton text={textOf} label="Copy all" />
        </span>
      </div>
      {/* Mounted only while open, so a closed dialog draws no rows. */}
      {wide && (
        <Dialog
          open={wide}
          onClose={() => setWide(false)}
          title={`${label} of ${shortId(requestId, HEAD_ID_CHARS)}`}
          width={1040}
          className="dialog--wide"
          footer={<CopyButton text={textOf} label="Copy all" />}
        >
          <BodyViewer highlight lines={linesOf()} />
        </Dialog>
      )}
    </div>
  );
}
