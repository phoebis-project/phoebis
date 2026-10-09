import { useEffect, useMemo, useRef, useState } from 'react';
import { Link, useLocation, useNavigate, useParams, useSearchParams } from 'react-router-dom';
import { api } from '@/api';
import { REQUEST_OUTCOME_VALUES } from '@/api/decode';
import type { Attempt, AttemptResponse, CapturedBodies, CohortHold, RequestKind, RequestLog, RequestOutcome, RequestQuery, Usage } from '@/api/types';
import { Chips } from '@/components/DecisionCard';
import { PhaseWaterfall } from '@/components/PhaseWaterfall';
import { TokenBar } from '@/components/TokenBar';
import { AttemptCount } from '@/components/ui/AttemptTrail';
import { ColumnsControl } from '@/components/ui/ColumnsControl';
import { MultiSelect } from '@/components/ui/MultiSelect';
import { PatternInput } from '@/components/ui/PatternInput';
import { IconChevronLeft, IconChevronRight, IconChevronsLeft, IconRefresh, IconX } from '@/components/ui/icons';
import { Badge, Button, Card, CopyButton, Descriptions, Dialog, EmptyState, ErrorState, KeyValue, SearchInput, Select, Skeleton, Toggle } from '@/components/ui/primitives';
import { autoModeLabel } from '@/lib/autoMode';
import { bodyText, highlightJson } from '@/lib/bodies';
import { isDivergence, missLabel } from '@/lib/cache';
import { AGENT_TYPES, TURN_ORIGINS } from '@/lib/client';
import { clock } from '@/lib/clock';
import { bytesDropped, fmtAbsolute, fmtCompact, fmtInt, fmtMs, fmtRelative, fmtTime, fmtUsd, shareLabel, shortAgent, shortId, shortModel, tokensTotal } from '@/lib/format';
import { useAsync, useCommittedText, useNow } from '@/lib/hooks';
import { costCellOf, type CostUnit, costUnitOf, FUNDING_WORD, NO_SHARE_TITLE } from '@/lib/requests';
import { type Chip, logHref, requestLink } from '@/lib/routingLog';
import { StatusBadge } from './DashboardPage';

const COLUMNS = [
  { id: 'time', label: 'Time' },
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
  { id: 'tokens', label: 'Tokens' },
  { id: 'cost', label: 'Cost' },
] as const;
type ColumnId = (typeof COLUMNS)[number]['id'];
// The drawer names all three, and leaving them out lets the default set fit the card at 1280 by 800.
const HIDDEN_BY_DEFAULT: readonly ColumnId[] = ['kind', 'pool', 'transport'];
/** The columns the table shows while the drawer is open: derived at render, so the chosen set returns on close. */
const COMPACT: readonly ColumnId[] = ['time', 'model', 'status', 'attempts'];

const RANGES = [
  { label: 'Last hour', value: '1h', ms: 3_600_000 },
  { label: 'Last 6 hours', value: '6h', ms: 6 * 3_600_000 },
  { label: 'Last 24 hours', value: '24h', ms: 24 * 3_600_000 },
];
const PAGE_SIZES = [10, 25, 50, 100];
const KINDS: readonly RequestKind[] = ['count_tokens', 'inference', 'probe'];
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

/** Every filter, the range and the page size, which the page's address holds so a reload or a shared link keeps them. */
interface Filters {
  accounts: string[];
  /** The user agent pattern. */
  agent: string;
  agentTypes: string[];
  client: string;
  held: boolean;
  keys: string[];
  kind: '' | RequestKind;
  models: string[];
  outcomes: RequestOutcome[];
  pools: string[];
  range: string;
  rows: number;
  search: string;
  speeds: string[];
  statuses: string[];
  turnOrigins: string[];
}

const DEFAULTS: Filters = { accounts: [], agent: '', agentTypes: [], client: '', held: false, keys: [], kind: '', models: [], outcomes: [], pools: [], range: '24h', rows: 25, search: '', speeds: [], statuses: [], turnOrigins: [] };

/** Absent keys read as the defaults, and so does a kind, range, page size or status the page could not have written. */
function filtersFromParams(params: URLSearchParams): Filters {
  const text = (name: string) => params.get(name) ?? '';
  const list = (name: string) => text(name).split(',').filter(Boolean);
  const kind = KINDS.find((k) => k === params.get('kind'));
  const range = RANGES.find((r) => r.value === params.get('range'));
  const rows = Number(params.get('rows'));
  return {
    accounts: list('accounts'),
    agent: text('agent'),
    agentTypes: list('agent_types'),
    client: text('client'),
    held: params.get('held') === '1',
    keys: list('keys'),
    kind: kind ?? DEFAULTS.kind,
    models: list('models'),
    outcomes: outcomesOf(list('outcomes')),
    pools: list('pools'),
    range: range?.value ?? DEFAULTS.range,
    rows: PAGE_SIZES.includes(rows) ? rows : DEFAULTS.rows,
    search: text('search'),
    speeds: list('speeds'),
    statuses: list('statuses').filter((code) => /^\d+$/.test(code)),
    turnOrigins: list('turn_origins'),
  };
}

/** A filter at its default is left out, so an unfiltered page has a bare address. */
function paramsFromFilters(f: Filters): URLSearchParams {
  const params = new URLSearchParams();
  const put = (name: string, value: string) => {
    if (value) params.set(name, value);
  };
  put('search', f.search);
  put('client', f.client);
  put('agent', f.agent);
  put('agent_types', f.agentTypes.join(','));
  put('turn_origins', f.turnOrigins.join(','));
  put('speeds', f.speeds.join(','));
  put('held', f.held ? '1' : '');
  put('keys', f.keys.join(','));
  put('pools', f.pools.join(','));
  put('accounts', f.accounts.join(','));
  put('models', f.models.join(','));
  put('statuses', f.statuses.join(','));
  put('outcomes', f.outcomes.join(','));
  put('kind', f.kind);
  put('range', f.range === DEFAULTS.range ? '' : f.range);
  put('rows', f.rows === DEFAULTS.rows ? '' : String(f.rows));
  return params;
}

/** Keyed by every outcome the ledger records, so one it gains is a type error until it is named here. */
const REQUEST_OUTCOME_LABEL: Record<RequestOutcome, string> = {
  canceled: 'canceled',
  exhausted: 'exhausted',
  failed: 'failed',
  incomplete: 'incomplete',
  rejected: 'rejected',
  succeeded: 'succeeded',
};

/** Where a hold waited: its cohort's account, else the last one it waited on. */
const heldOn = (h: CohortHold) => h.cohort ?? h.waitedOn[h.waitedOn.length - 1] ?? 'an account';
/** The badge rounds the wait, so its hover carries it whole with where it was placed. */
const heldTitle = (h: CohortHold) => `waited ${fmtInt(h.heldMs)} ms on ${heldOn(h)}`;

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

/** The Cost cell: the native unit's main line over its second, every share and the equivalent on the title. */
function CostCell({ request }: { request: RequestLog }) {
  const cell = costCellOf(costUnitOf(request));
  return (
    <td className="right num" title={cell.title || undefined}>
      {cell.main}
      {cell.sub && <span className="sub">{cell.sub}</span>}
    </td>
  );
}

/** The drawer's cost row: the share chips with the equivalent muted after them, or the dollars with why. */
function costRowValue(unit: CostUnit) {
  switch (unit.kind) {
    case 'shares':
      return (
        <>
          <Chips chips={unit.shares.map(({ key, share }) => ({ value: shareLabel(key, share) }))} />
          <span className="muted">{fmtUsd(unit.usd)} equivalent</span>
        </>
      );
    case 'dollars':
      return <b title={`Billed in dollars: ${FUNDING_WORD[unit.funding]}`}>{fmtUsd(unit.usd)}</b>;
    case 'unpriced':
      return (
        <b title={NO_SHARE_TITLE}>
          {fmtUsd(unit.usd)} <span className="muted">equivalent</span>
        </b>
      );
    case 'unserved':
      return <b>{fmtUsd(unit.usd)}</b>;
  }
}

export function RequestsPage() {
  const { requestId } = useParams();
  const navigate = useNavigate();
  const location = useLocation();
  const now = useNow();
  const keys = useAsync(() => api.listKeys(), []);
  const pools = useAsync(() => api.listPools(), []);
  const accounts = useAsync(() => api.listAccounts(), []);
  const catalog = useAsync(() => api.listCatalog(), []);

  const [searchParams, setSearchParams] = useSearchParams();
  const filters = useMemo(() => filtersFromParams(searchParams), [searchParams]);
  const address = searchParams.toString();
  const [visible, setVisible] = useState<Set<ColumnId>>(() => new Set(COLUMNS.map((c) => c.id).filter((id) => !HIDDEN_BY_DEFAULT.includes(id))));
  // The cursor and the pages before it belong to the address they were read under, so any change of the address (a
  // filter, Reset, a link) drops them in the render that brings it, with no reset of their own to render apart and read
  // the old filters once more without the cursor. The stale entry is replaced in that same render, so changing a
  // filter back does not return to the page left behind under it.
  const [paging, setPaging] = useState<{ for: string; cursor?: string; stack: (string | undefined)[] }>({ for: address, stack: [] });
  if (paging.for !== address) setPaging({ for: address, stack: [] });
  const cursor = paging.for === address ? paging.cursor : undefined;
  const stack = paging.for === address ? paging.stack : [];
  // The range ends where the list was loaded, so a cursor page is read against the window its cursor came from;
  // Refresh moves the end to now.
  const [loadedAt, setLoadedAt] = useState(() => clock.now());
  // Written at once rather than in the router's transition: a click that follows typing blurs the search first, and its
  // own write or navigation would otherwise build on the address from before the blur's commit and drop it.
  const update = (patch: Partial<Filters>) => setSearchParams(paramsFromFilters({ ...filters, ...patch }), { flushSync: true, replace: true });
  const search = useCommittedText(filters.search, (next) => update({ search: next }));

  const query = useMemo<RequestQuery>(() => {
    const r = RANGES.find((x) => x.value === filters.range) ?? RANGES[2];
    // Probes are off the log unless they are what the filter asks for.
    return { accountIds: filters.accounts, agentTypes: filters.agentTypes, client: filters.client, cursor, from: new Date(loadedAt - r.ms).toISOString(), held: filters.held, includeProbes: filters.kind === 'probe', keyIds: filters.keys, kind: filters.kind || null, limit: filters.rows, models: filters.models, outcomes: filters.outcomes, poolIds: filters.pools, search: filters.search, speeds: filters.speeds, statuses: filters.statuses.map(Number), to: new Date(loadedAt).toISOString(), turnOrigins: filters.turnOrigins, userAgent: filters.agent };
  }, [filters, cursor, loadedAt]);
  const requests = useAsync(() => api.listRequests(query), [query]);

  const resetPaging = () => setPaging({ for: address, stack: [] });
  const refresh = () => {
    resetPaging();
    setLoadedAt(clock.now());
  };

  const rows = requests.data?.requests ?? [];
  const nextCursor = requests.data?.nextCursor ?? null;
  const goNext = () => {
    if (!nextCursor) return;
    setPaging({ for: address, cursor: nextCursor, stack: [...stack, cursor] });
  };
  const goPrevious = () => {
    const copy = [...stack];
    const previous = copy.pop();
    setPaging({ for: address, cursor: previous, stack: copy });
  };
  // The page size is how the operator reads the log rather than what it narrows to, so Reset keeps it.
  const active = paramsFromFilters({ ...filters, rows: DEFAULTS.rows }).toString() !== '';
  const reset = () => setSearchParams(paramsFromFilters({ ...DEFAULTS, rows: filters.rows }), { replace: true });
  const selected = requestId ? rows.find((r) => r.id === requestId) ?? null : null;
  const shown = requestId ? new Set<ColumnId>(COMPACT) : visible;

  return (
    <div className={`requests ${requestId ? 'has-drawer' : ''}`}>
      <div className="stack">
        <div className="filters">
          <SearchInput placeholder="Search id, session, key, account, model, error, message id" value={search.draft} onChange={(e) => search.onChange(e.target.value)} onBlur={search.onBlur} onKeyDown={search.onKeyDown} className="filters__search" />
          <Select value={filters.range} onChange={(e) => update({ range: e.target.value })} aria-label="Date range">
            {RANGES.map((r) => (
              <option key={r.value} value={r.value}>
                {r.label}
              </option>
            ))}
          </Select>
          <MultiSelect
            label="Keys"
            options={(keys.data ?? []).map((k) => ({ label: k.name, value: k.id }))}
            value={filters.keys}
            onChange={(v) => update({ keys: v })}
            disabled={keys.data === null && keys.error !== null}
            description={keys.error ? `The key list did not load: ${keys.error}` : undefined}
            mono
          />
          <MultiSelect
            label="Pools"
            options={(pools.data ?? []).map((p) => ({ label: p.name, value: p.id }))}
            value={filters.pools}
            onChange={(v) => update({ pools: v })}
            disabled={pools.data === null && pools.error !== null}
            description={pools.error ? `The pool list did not load: ${pools.error}` : undefined}
            mono
          />
          <MultiSelect
            label="Accounts"
            options={(accounts.data ?? []).map((a) => ({ label: a.label, value: a.id }))}
            value={filters.accounts}
            onChange={(v) => update({ accounts: v })}
            disabled={accounts.data === null && accounts.error !== null}
            description={accounts.error ? `The account list did not load: ${accounts.error}` : undefined}
          />
          <MultiSelect
            label="Models"
            options={(catalog.data ?? []).map((m) => ({ label: m.id, value: m.id }))}
            value={filters.models}
            onChange={(v) => update({ models: v })}
            disabled={catalog.data === null && catalog.error !== null}
            description={catalog.error ? `The model list did not load: ${catalog.error}` : undefined}
            searchable
            mono
          />
          <PatternInput label="Client pattern" value={filters.client} onCommit={(v) => update({ client: v })} />
          <PatternInput label="User agent pattern" value={filters.agent} onCommit={(v) => update({ agent: v })} />
          <MultiSelect label="Status" options={statusOptions(rows, filters.statuses)} value={filters.statuses} onChange={(v) => update({ statuses: v })} />
          <MultiSelect
            label="Outcome"
            options={REQUEST_OUTCOME_VALUES.map((o) => ({ label: REQUEST_OUTCOME_LABEL[o], value: o }))}
            value={filters.outcomes}
            onChange={(v) => update({ outcomes: outcomesOf(v) })}
          />
          <MultiSelect label="Agent type" options={AGENT_TYPES.map((v) => ({ label: v, value: v }))} value={filters.agentTypes} onChange={(v) => update({ agentTypes: v })} mono />
          <MultiSelect label="Turn origin" options={TURN_ORIGINS.map((v) => ({ label: v, value: v }))} value={filters.turnOrigins} onChange={(v) => update({ turnOrigins: v })} mono />
          <Select value={filters.kind} onChange={(e) => update({ kind: e.target.value as '' | RequestKind })} aria-label="Kind">
            <option value="">Any kind</option>
            <option value="inference">Inference</option>
            <option value="count_tokens">Count tokens</option>
            <option value="probe">Probe</option>
          </Select>
          <MultiSelect
            label="Speed"
            options={[
              { label: 'fast', value: 'fast' },
              { label: 'standard', value: 'standard' },
            ]}
            value={filters.speeds}
            onChange={(v) => update({ speeds: v })}
            mono
          />
          <label className="row">
            <Toggle checked={filters.held} onChange={(v) => update({ held: v })} label="Held only" />
            <span className="secondary">Held only</span>
          </label>
          {active ? (
            <button type="button" className="link-btn" onClick={reset}>
              Reset
            </button>
          ) : null}
          <div className="filters__cols">
            <Button size="sm" icon={<IconRefresh size={14} />} onClick={refresh} title={`Rows up to ${fmtTime(new Date(loadedAt).toISOString())}; Refresh reads up to now`}>
              Refresh
            </Button>
            <ColumnsControl columns={COLUMNS} visible={visible} onChange={setVisible} />
          </div>
        </div>

        <Card padded={false} className="card--table">
          <div className="table-wrap">
            <table className="table table--hover requests__table">
              <thead>
                <tr>
                  {shown.has('time') && <th>Time</th>}
                  {shown.has('key') && <th>Key</th>}
                  {shown.has('pool') && <th>Pool</th>}
                  {shown.has('account') && <th>Account</th>}
                  {shown.has('model') && <th>Model</th>}
                  {shown.has('client') && <th>Client</th>}
                  {shown.has('kind') && <th>Kind</th>}
                  {shown.has('transport') && <th>Transport</th>}
                  {shown.has('status') && <th>Status</th>}
                  {shown.has('attempts') && <th title="How many attempts the request took, the trail on hover">Attempts</th>}
                  {shown.has('ttfb') && <th className="right">TTFB</th>}
                  {shown.has('tokens') && <th className="right">Tokens</th>}
                  {shown.has('cost') && (
                    <th className="right" title="Window shares for a request a subscription window paid, dollars for an overage, fast mode or API key request">
                      Cost
                    </th>
                  )}
                  <th />
                </tr>
              </thead>
              <tbody>
                {requests.loading && !requests.data
                  ? Array.from({ length: 8 }, (_, i) => (
                      <tr key={i}>
                        <td colSpan={shown.size + 1}>
                          <Skeleton />
                        </td>
                      </tr>
                    ))
                  : rows.map((r) => (
                      <tr
                        key={r.id}
                        className={r.id === requestId ? 'is-selected' : ''}
                        onClick={() => navigate({ pathname: `/requests/${r.id}`, search: location.search })}
                        tabIndex={0}
                        // A key pressed on a link in the row is the link's own, so Enter on Routing opens the log alone.
                        onKeyDown={(e) => e.key === 'Enter' && e.target === e.currentTarget && navigate({ pathname: `/requests/${r.id}`, search: location.search })}
                      >
                        {shown.has('time') && (
                          <td className="num" title={fmtAbsolute(r.at)}>
                            {fmtTime(r.at)}
                            <span className="sub mono">{shortId(r.id, 12)}</span>
                          </td>
                        )}
                        {shown.has('key') && <td className="mono">{r.keyId ?? '-'}</td>}
                        {shown.has('pool') && <td className="mono">{r.poolId ?? '-'}</td>}
                        {shown.has('account') && <td>{r.accountId ?? '-'}</td>}
                        {shown.has('model') && (
                          <td className="mono nowrap" title={shortModel(r.model) === r.model ? undefined : r.model}>
                            {shortModel(r.model)}
                          </td>
                        )}
                        {shown.has('client') && (
                          <td className="mono requests__client" title={r.client ?? undefined}>
                            {r.client === null ? <span className="muted">-</span> : <span className="requests__client-text">{r.client}</span>}
                            {r.agentType && (
                              <span className="sub" title={r.agentType}>
                                {r.agentType}
                              </span>
                            )}
                          </td>
                        )}
                        {shown.has('kind') && <td>{r.kind.replace('_', ' ')}</td>}
                        {shown.has('transport') && (
                          <td>
                            <Badge tone={r.transport === 'sse' ? 'accent' : 'neutral'}>{r.transport === 'sse' ? 'SSE' : 'JSON'}</Badge>
                          </td>
                        )}
                        {shown.has('status') && (
                          <td>
                            <span className="requests__status">
                              <StatusBadge status={r.status} />
                              {r.outcome !== 'succeeded' && <span className="muted requests__outcome">{REQUEST_OUTCOME_LABEL[r.outcome]}</span>}
                            </span>
                          </td>
                        )}
                        {shown.has('attempts') && (
                          <td>
                            <span className="requests__attempts">
                              <AttemptCount attempts={r.attempts} />
                              {r.held && (
                                <Badge tone="neutral" title={heldTitle(r.held)}>
                                  held {fmtMs(r.held.heldMs)}
                                </Badge>
                              )}
                            </span>
                          </td>
                        )}
                        {shown.has('ttfb') && <td className="right num">{fmtMs(r.ttfbMs)}</td>}
                        {shown.has('tokens') && (
                          <td className="right num">
                            {fmtCompact(tokensTotal(r.usage))}
                            <span className="sub">{r.usage.cacheRead > 0 ? `${fmtCompact(r.usage.cacheRead)} cached` : 'uncached'}</span>
                          </td>
                        )}
                        {shown.has('cost') && <CostCell request={r} />}
                        <td className="right">
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
                  <tr>
                    <td colSpan={shown.size + 1}>
                      {requests.error ? (
                        <ErrorState title="Could not load the request log" body={requests.error} action={<Button onClick={requests.reload}>Try again</Button>} />
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
            <span className="muted num">
              {rows.length === 0 ? 'No rows' : `${fmtInt(rows.length)} shown`}
              {nextCursor ? ', more available' : ''}
            </span>
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
          <RequestDrawer request={selected} requestId={requestId} now={now} onClose={() => navigate({ pathname: '/requests', search: location.search })} />
        </Descriptions>
      )}
    </div>
  );
}

function RequestDrawer({ request, requestId, now, onClose }: { now: number; onClose: () => void; request: RequestLog | null; requestId: string }) {
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
  const answer = serving ? answerChips(serving.response) : [];

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
                { label: 'Error', value: r.errorCode ?? <span className="muted">none</span>, mono: Boolean(r.errorCode) },
                ...(r.clientDrift ? [{ label: 'Client drift', value: r.clientDrift, mono: true }] : []),
                ...(serving
                  ? [
                      { label: 'Answer', value: answer.length ? <Chips chips={answer} /> : <span className="muted">none</span> },
                      { label: 'Cache', value: <CacheValue attempt={serving} usage={r.usage} /> },
                    ]
                  : []),
              ]}
            />
          </section>

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
                  <td className="right num">{costRowValue(costUnitOf(r))}</td>
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
                {/* Keyed on the request, so a large body of the next request starts compact again. */}
                <BodyBlock key={`request ${r.id}`} label="Request" requestId={r.id} value={bodies.data.request} stored={bodies.data.requestBytesStored} dropped={bodies.data.requestTruncated ? bytesDropped(bodies.data.requestBytesSeen, bodies.data.requestBytesStored) : undefined} />
                <BodyBlock key={`response ${r.id}`} label="Response" requestId={r.id} value={bodies.data.response} stored={bodies.data.responseBytesStored} dropped={bodies.data.responseTruncated ? bytesDropped(bodies.data.responseBytesSeen, bodies.data.responseBytesStored) : undefined} />
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
function answerChips(response: AttemptResponse): Chip[] {
  const chips: Chip[] = [];
  if (response.stopReason) chips.push({ value: `stopped: ${response.stopReason}` });
  if (response.speed) chips.push({ title: "The answer's speed", value: response.speed });
  if (response.inferenceGeo) chips.push({ label: 'geography', title: GEOGRAPHY_TITLE, value: response.inferenceGeo === 'not_available' ? 'not available' : response.inferenceGeo });
  if (response.servedModel) chips.push({ mono: true, value: `served by ${response.servedModel}` });
  if (response.clearedInputTokens !== null && response.clearedInputTokens > 0) chips.push({ value: `${fmtInt(response.clearedInputTokens)} tokens cleared by context edits` });
  if (response.refusalCategory) chips.push({ value: `category: ${response.refusalCategory}` });
  if (response.refusalRecommendedModel) chips.push({ mono: true, value: `try ${response.refusalRecommendedModel}` });
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

/** Indenting a body past this many stored bytes is a long render and a far longer text, so it waits to be asked. */
const PRETTY_LIMIT = 256 * 1024;

function BodyBlock({ dropped, label, requestId, stored, value }: { dropped?: number; label: string; requestId: string; stored: number; value: unknown }) {
  // The stored byte count sizes the body, so the first draw serializes it once; the memo keeps the drawer's redraws on
  // every clock tick and list reload from serializing it again.
  const large = stored > PRETTY_LIMIT;
  const [pretty, setPretty] = useState(false);
  const [wide, setWide] = useState(false);
  const text = useMemo(() => bodyText(value, large && !pretty), [large, pretty, value]);
  // A compact large body stays plain, and a large stream shows its stored text until Pretty-print splits it.
  const tokens = useMemo(() => (large && !pretty ? null : highlightJson(text)), [large, pretty, text]);
  const drawn = useMemo(() => (tokens === null ? text : tokens.map((t, i) => (t.cls === null ? t.text : <span key={i} className={t.cls}>{t.text}</span>))), [text, tokens]);
  // Pretty-print leaves as it is pressed, so the focus moves to the text it indented rather than falling to the page.
  // Under the open dialog the block's own text is inert, so the dialog's takes it.
  const pre = useRef<HTMLPreElement>(null);
  const widePre = useRef<HTMLPreElement>(null);
  useEffect(() => {
    if (pretty) (wide ? widePre : pre).current?.focus();
  }, [pretty]);
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
          {large && !pretty && (
            <Button size="sm" variant="ghost" onClick={() => setPretty(true)}>
              Pretty-print
            </Button>
          )}
          <button ref={link} type="button" className="body-block__link" onClick={() => setWide(true)}>
            Open wide
          </button>
          <CopyButton text={text} />
        </span>
      </div>
      <pre ref={pre} className="body-block__pre" tabIndex={0}>
        {drawn}
      </pre>
      {/* Mounted only while open, so the drawer holds one text per block. */}
      {wide && (
        <Dialog
          open={wide}
          onClose={() => setWide(false)}
          title={`${label} of ${shortId(requestId, HEAD_ID_CHARS)}`}
          width={1040}
          className="dialog--wide"
          footer={
            <>
              {large && !pretty && (
                <Button size="sm" variant="ghost" onClick={() => setPretty(true)}>
                  Pretty-print
                </Button>
              )}
              <CopyButton text={text} />
            </>
          }
        >
          <pre ref={widePre} className="body-block__pre" tabIndex={0}>
            {drawn}
          </pre>
        </Dialog>
      )}
    </div>
  );
}
