import { useEffect, useMemo, useState } from 'react';
import { Link, useNavigate, useParams } from 'react-router-dom';
import { api } from '@/api';
import type { Attempt, CapturedBodies, CohortHold, PinReleaseReason, RequestKind, RequestLog, RequestQuery, Usage, WindowId } from '@/api/types';
import { PIN_RELEASE_REASON_VALUES } from '@/api/decode';
import { useAppSettings } from '@/app/settings';
import { SelectionTrace } from '@/components/SelectionTrace';
import { AttemptTrail } from '@/components/ui/AttemptTrail';
import { MultiSelect } from '@/components/ui/MultiSelect';
import { IconChevronLeft, IconChevronRight, IconChevronsLeft, IconColumns, IconRefresh, IconX } from '@/components/ui/icons';
import { Badge, Button, Card, CopyButton, EmptyState, ErrorState, Input, KeyValue, SearchInput, Select, Skeleton, Toggle } from '@/components/ui/primitives';
import { moveLabel } from '@/lib/analytics';
import { autoModeLabel } from '@/lib/autoMode';
import { isDivergence, missLabel } from '@/lib/cache';
import { AGENT_TYPES, CLIENT_CLASS_CHARS, TURN_ORIGINS } from '@/lib/client';
import { bytesDropped, fmtAbsolute, fmtCompact, fmtInt, fmtMs, fmtPct, fmtRelative, fmtTime, fmtUsd, shortAgent, shortId, tokensTotal } from '@/lib/format';
import { useAsync, useNow } from '@/lib/hooks';
import { requestTrail } from '@/lib/trail';
import { StatusBadge } from './DashboardPage';

const COLUMNS = [
  { id: 'time', label: 'Time' },
  { id: 'key', label: 'Key' },
  { id: 'pool', label: 'Pool' },
  { id: 'account', label: 'Upstream' },
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

const RANGES = [
  { label: 'Last hour', value: '1h', ms: 3_600_000 },
  { label: 'Last 6 hours', value: '6h', ms: 6 * 3_600_000 },
  { label: 'Last 24 hours', value: '24h', ms: 24 * 3_600_000 },
];

const OUTCOME_LABEL: Record<Attempt['outcome'], string> = {
  auth_failed: 'Auth failed',
  canceled: 'Canceled',
  rate_limited: 'Rate limited',
  succeeded: 'Succeeded',
  transport_error: 'Transport error',
  upstream_error: 'Upstream error',
};

const PIN_RELEASE_LABEL: Record<PinReleaseReason, string> = {
  attempted: 'already tried by this request',
  budget: 'upstream at its budget',
  client_error: 'client error',
  cooldown: 'account cooled',
  deleted: 'upstream deleted',
  disconnected: 'upstream disconnected',
  expired: 'pin idle past the affinity window',
  fast_credits: 'usage credits unavailable for fast mode',
  fast_rate_limit: 'fast lane rate limited',
  funding_required: 'funding required',
  overage: 'in overage',
  paused: 'upstream paused',
  reauth_required: 'needs re-auth',
  released: 'nothing else stood against the upstream',
  spent: 'window spent',
  subscription_ended: 'subscription ended',
  unpaired: 'upstream unpaired',
  unserved: 'model no longer served here',
};
const releaseLabel = (reasons: unknown): string =>
  (Array.isArray(reasons) ? reasons : [])
    .map((r) => {
      const reason = PIN_RELEASE_REASON_VALUES.find((v) => v === r);
      return reason === undefined ? String(r) : PIN_RELEASE_LABEL[reason];
    })
    .join(', ');

/** Where a hold waited: its cohort's upstream, else the last one it waited on. */
const heldOn = (h: CohortHold) => h.cohort ?? h.waitedOn[h.waitedOn.length - 1] ?? 'an upstream';
/** The badge rounds the wait, so its hover carries it whole with where it was placed, as the drawer names it. */
const heldTitle = (h: CohortHold) => `waited ${fmtInt(h.heldMs)} ms on ${heldOn(h)}`;

// A fixed three-slot readout: a plan that serves no Fable reports two windows in
// its headers, and the empty third slot is the point rather than a gap to hide.
const ATTEMPT_WINDOWS: { id: WindowId; label: string }[] = [
  { id: '5h', label: '5h' },
  { id: '7d', label: '7d' },
  { id: '7d_fable', label: 'Fable' },
];

export function RequestsPage() {
  const { requestId } = useParams();
  const navigate = useNavigate();
  const now = useNow();
  const keys = useAsync(() => api.listKeys(), []);
  const pools = useAsync(() => api.listPools(), []);
  const accounts = useAsync(() => api.listAccounts(), []);
  const catalog = useAsync(() => api.listCatalog(), []);

  const [search, setSearch] = useState('');
  const [userAgent, setUserAgent] = useState('');
  const [clientPattern, setClientPattern] = useState('');
  const [agentTypes, setAgentTypes] = useState<string[]>([]);
  const [turnOrigins, setTurnOrigins] = useState<string[]>([]);
  const [speeds, setSpeeds] = useState<string[]>([]);
  const [held, setHeld] = useState(false);
  const [keyIds, setKeyIds] = useState<string[]>([]);
  const [poolIds, setPoolIds] = useState<string[]>([]);
  const [accountIds, setAccountIds] = useState<string[]>([]);
  const [models, setModels] = useState<string[]>([]);
  const [statuses, setStatuses] = useState<string[]>([]);
  const [kind, setKind] = useState<'' | RequestKind>('');
  const [range, setRange] = useState('24h');
  const [visible, setVisible] = useState<Set<ColumnId>>(() => new Set(COLUMNS.map((c) => c.id)));
  const [colsOpen, setColsOpen] = useState(false);
  const [pageSize, setPageSize] = useState(25);
  const [cursor, setCursor] = useState<string | undefined>(undefined);
  const [stack, setStack] = useState<(string | undefined)[]>([]);
  // The range ends where the list was loaded, so a cursor page is read against the window its cursor came from;
  // Refresh moves the end to now.
  const [loadedAt, setLoadedAt] = useState(() => Date.now());

  const query = useMemo<RequestQuery>(() => {
    const r = RANGES.find((x) => x.value === range) ?? RANGES[2];
    // Probes are off the log unless they are what the filter asks for.
    return { accountIds, agentTypes, client: clientPattern, cursor, from: new Date(loadedAt - r.ms).toISOString(), held, includeProbes: kind === 'probe', keyIds, kind: kind || null, limit: pageSize, models, poolIds, search, speeds, statuses: statuses.map(Number), to: new Date(loadedAt).toISOString(), turnOrigins, userAgent };
  }, [search, userAgent, clientPattern, agentTypes, turnOrigins, speeds, held, keyIds, poolIds, accountIds, models, statuses, kind, range, pageSize, cursor, loadedAt]);
  const requests = useAsync(() => api.listRequests(query), [query]);

  const resetPaging = () => {
    setCursor(undefined);
    setStack([]);
  };
  const refresh = () => {
    resetPaging();
    setLoadedAt(Date.now());
  };
  useEffect(resetPaging, [search, userAgent, clientPattern, agentTypes, turnOrigins, speeds, held, keyIds, poolIds, accountIds, models, statuses, kind, range, pageSize]);

  const rows = requests.data?.requests ?? [];
  // Sized to the longest class the loaded rows actually carry, floored at the header label so the column never
  // collapses under it and capped at the longest known class; a wider forged class truncates behind the cell's
  // ellipsis instead of widening the column.
  const clientColumnChars = Math.min(CLIENT_CLASS_CHARS, Math.max('Client'.length, ...rows.map((r) => (r.client ?? '').length)));
  const nextCursor = requests.data?.nextCursor ?? null;
  const goNext = () => {
    if (!nextCursor) return;
    setStack((s) => [...s, cursor]);
    setCursor(nextCursor);
  };
  const goPrevious = () => {
    const copy = [...stack];
    const previous = copy.pop();
    setStack(copy);
    setCursor(previous);
  };
  const active = search || userAgent || clientPattern || agentTypes.length || turnOrigins.length || speeds.length || held || keyIds.length || poolIds.length || accountIds.length || models.length || statuses.length || kind || range !== '24h';
  const reset = () => {
    setSearch('');
    setUserAgent('');
    setClientPattern('');
    setAgentTypes([]);
    setTurnOrigins([]);
    setSpeeds([]);
    setHeld(false);
    setKeyIds([]);
    setPoolIds([]);
    setAccountIds([]);
    setModels([]);
    setStatuses([]);
    setKind('');
    setRange('24h');
  };
  const selected = requestId ? rows.find((r) => r.id === requestId) ?? null : null;

  return (
    <div className={`requests ${requestId ? 'has-drawer' : ''}`}>
      <div className="stack">
        <div className="filters">
          <SearchInput placeholder="Search id, session, key, upstream, model, error" value={search} onChange={(e) => setSearch(e.target.value)} className="filters__search" />
          <Select value={range} onChange={(e) => setRange(e.target.value)} aria-label="Date range">
            {RANGES.map((r) => (
              <option key={r.value} value={r.value}>
                {r.label}
              </option>
            ))}
          </Select>
          <MultiSelect label="Keys" options={(keys.data ?? []).map((k) => ({ label: k.name, value: k.id }))} value={keyIds} onChange={setKeyIds} mono />
          <MultiSelect label="Pools" options={(pools.data ?? []).map((p) => ({ label: p.name, value: p.id }))} value={poolIds} onChange={setPoolIds} mono />
          <MultiSelect label="Upstreams" options={(accounts.data ?? []).map((a) => ({ label: a.label, value: a.id }))} value={accountIds} onChange={setAccountIds} />
          <MultiSelect label="Models" options={(catalog.data ?? []).map((m) => ({ label: m.id, value: m.id }))} value={models} onChange={setModels} searchable mono />
          {/* `.mono` shrinks the text to 0.92em, which would leave these controls shorter than the rest of the bar. */}
          <Input value={clientPattern} onChange={(e) => setClientPattern(e.target.value)} placeholder="Client pattern" aria-label="Client pattern" title="* matches any run of characters, ? one; a backslash quotes either" className="mono" style={{ fontSize: 'inherit', width: 180 }} />
          <Input value={userAgent} onChange={(e) => setUserAgent(e.target.value)} placeholder="User agent pattern" aria-label="User agent pattern" title="* matches any run of characters, ? one; a backslash quotes either" className="mono" style={{ fontSize: 'inherit', width: 180 }} />
          <MultiSelect
            label="Status"
            options={[
              { label: '200 OK', value: '200' },
              { label: '429 Rate limited', value: '429' },
              { label: '502 Bad gateway', value: '502' },
              { label: '529 Overloaded', value: '529' },
            ]}
            value={statuses}
            onChange={setStatuses}
          />
          <MultiSelect label="Agent type" options={AGENT_TYPES.map((v) => ({ label: v, value: v }))} value={agentTypes} onChange={setAgentTypes} mono />
          <MultiSelect label="Turn origin" options={TURN_ORIGINS.map((v) => ({ label: v, value: v }))} value={turnOrigins} onChange={setTurnOrigins} mono />
          <Select value={kind} onChange={(e) => setKind(e.target.value as '' | RequestKind)} aria-label="Kind">
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
            value={speeds}
            onChange={setSpeeds}
            mono
          />
          <label className="row">
            <Toggle checked={held} onChange={setHeld} label="Held only" />
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
            <Button size="sm" icon={<IconColumns size={14} />} onClick={() => setColsOpen((v) => !v)} aria-expanded={colsOpen}>
              Columns ({visible.size})
            </Button>
            {colsOpen && (
              <div className="cols-popover">
                {COLUMNS.map((c) => (
                  <label key={c.id}>
                    <input
                      type="checkbox"
                      checked={visible.has(c.id)}
                      onChange={(e) => {
                        const next = new Set(visible);
                        if (e.target.checked) next.add(c.id);
                        else next.delete(c.id);
                        setVisible(next);
                      }}
                    />
                    {c.label}
                  </label>
                ))}
              </div>
            )}
          </div>
        </div>

        <Card padded={false} className="card--table">
          <div className="table-wrap">
            <table className="table table--hover requests__table">
              <thead>
                <tr>
                  {visible.has('time') && <th>Time</th>}
                  {visible.has('key') && <th>Key</th>}
                  {visible.has('pool') && <th>Pool</th>}
                  {visible.has('account') && <th>Upstream</th>}
                  {visible.has('model') && <th>Model</th>}
                  {visible.has('client') && <th>Client</th>}
                  {visible.has('kind') && <th>Kind</th>}
                  {visible.has('transport') && <th>Transport</th>}
                  {visible.has('status') && <th>Status</th>}
                  {visible.has('attempts') && <th>Attempts</th>}
                  {visible.has('ttfb') && <th className="right">TTFB</th>}
                  {visible.has('tokens') && <th className="right">Tokens</th>}
                  {visible.has('cost') && <th className="right">Cost</th>}
                  <th />
                </tr>
              </thead>
              <tbody>
                {requests.loading && !requests.data
                  ? Array.from({ length: 8 }, (_, i) => (
                      <tr key={i}>
                        <td colSpan={visible.size + 1}>
                          <Skeleton />
                        </td>
                      </tr>
                    ))
                  : rows.map((r) => (
                      <tr key={r.id} className={r.id === requestId ? 'is-selected' : ''} onClick={() => navigate(`/requests/${r.id}`)} tabIndex={0} onKeyDown={(e) => e.key === 'Enter' && navigate(`/requests/${r.id}`)}>
                        {visible.has('time') && (
                          <td className="num" title={fmtAbsolute(r.at)}>
                            {fmtTime(r.at)}
                            <span className="sub mono">{shortId(r.id, 12)}</span>
                          </td>
                        )}
                        {visible.has('key') && <td className="mono">{r.keyId ?? '-'}</td>}
                        {visible.has('pool') && <td className="mono">{r.poolId ?? '-'}</td>}
                        {visible.has('account') && <td>{r.accountId ?? '-'}</td>}
                        {visible.has('model') && <td className="mono">{r.model}</td>}
                        {visible.has('client') && (
                          <td className="mono requests__client" title={r.client ?? undefined} style={{ minWidth: `${clientColumnChars}ch` }}>
                            {r.client ?? <span className="muted">-</span>}
                            {r.agentType && <span className="sub">{r.agentType}</span>}
                          </td>
                        )}
                        {visible.has('kind') && <td>{r.kind.replace('_', ' ')}</td>}
                        {visible.has('transport') && (
                          <td>
                            <Badge tone={r.transport === 'sse' ? 'accent' : 'neutral'}>{r.transport === 'sse' ? 'SSE' : 'JSON'}</Badge>
                          </td>
                        )}
                        {visible.has('status') && (
                          <td>
                            <StatusBadge status={r.status} />
                          </td>
                        )}
                        {visible.has('attempts') && (
                          <td>
                            <span className="requests__attempts">
                              <AttemptTrail attempts={r.attempts} />
                              {r.held && (
                                <Badge tone="neutral" title={heldTitle(r.held)}>
                                  held {fmtMs(r.held.heldMs)}
                                </Badge>
                              )}
                            </span>
                          </td>
                        )}
                        {visible.has('ttfb') && <td className="right num">{fmtMs(r.ttfbMs)}</td>}
                        {visible.has('tokens') && (
                          <td className="right num">
                            {fmtCompact(tokensTotal(r.usage))}
                            <span className="sub">{r.usage.cacheRead > 0 ? `${fmtCompact(r.usage.cacheRead)} cached` : 'uncached'}</span>
                          </td>
                        )}
                        {visible.has('cost') && <td className="right num">{fmtUsd(r.costUsd)}</td>}
                        <td className="right">
                          <Link to={`/requests/${r.id}`} onClick={(e) => e.stopPropagation()}>
                            Details
                          </Link>
                        </td>
                      </tr>
                    ))}
                {!requests.loading && rows.length === 0 && (
                  <tr>
                    <td colSpan={visible.size + 1}>
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
              <Select value={pageSize} onChange={(e) => setPageSize(Number(e.target.value))}>
                {[10, 25, 50, 100].map((n) => (
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

      {requestId && <RequestDrawer request={selected} requestId={requestId} now={now} onClose={() => navigate('/requests')} />}
    </div>
  );
}

function RequestDrawer({ request, requestId, now, onClose }: { now: number; onClose: () => void; request: RequestLog | null; requestId: string }) {
  const { canMutate } = useAppSettings();
  // One read answers the row and its events; the list's own row draws the summary while it runs.
  const detail = useAsync(() => api.getRequest(requestId), [requestId]);
  // A load that is pending or has failed leaves the answer to the request before it in hand, so only an answer
  // for this id is read.
  const fetched = detail.data?.request.id === requestId ? detail.data : null;
  const r = request ?? fetched?.request ?? null;
  const bodies = useAsync<CapturedBodies | null>(() => (r?.capturedBodies && canMutate ? api.getCapturedBodies(r.id) : Promise.resolve(null)), [r?.id, canMutate]);
  // A load that is pending or has failed leaves the answer to the request before it in hand; prompts and
  // responses belong to the row they were read for and to no other, so that answer is never drawn here.
  const otherRequestsBodies = bodies.data !== null && bodies.data?.requestId !== r?.id;
  const trail = r === null || fetched === null ? [] : requestTrail(fetched.events, r);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === 'Escape' && onClose();
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);

  return (
    <aside className="drawer" aria-label="Request details">
      <header className="drawer__head">
        <div>
          <h2 className="drawer__title mono">{r?.id ?? requestId}</h2>
          {r && (
            <p className="muted" title={fmtAbsolute(r.at)}>
              {fmtAbsolute(r.at)} ({fmtRelative(r.at, now)})
            </p>
          )}
        </div>
        <button type="button" className="dialog__close" onClick={onClose} aria-label="Close details">
          <IconX size={16} />
        </button>
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
            <h3 className="section-title">Summary</h3>
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
                { label: 'Upstream', value: r.accountId === null ? '-' : <Link to={`/upstreams/${r.accountId}`}>{r.accountId}</Link> },
                { label: 'Model', value: r.model, mono: true },
                ...(r.canonicalModel !== r.model ? [{ label: 'Canonical model', value: r.canonicalModel, mono: true }] : []),
                { label: 'Kind', value: `${r.kind.replace('_', ' ')}, ${r.transport === 'sse' ? 'SSE' : 'JSON'}` },
                { label: 'Status', value: <StatusBadge status={r.status} /> },
                { label: 'TTFB', value: fmtMs(r.ttfbMs) },
                { label: 'Duration', value: fmtMs(r.durationMs) },
                { label: 'Error', value: r.errorCode ?? <span className="muted">none</span>, mono: Boolean(r.errorCode) },
                ...(r.clientDrift ? [{ label: 'Client drift', value: r.clientDrift, mono: true }] : []),
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
            <h3 className="section-title">Attempts</h3>
            <ol className="attempts">
              {r.attempts.map((a) => (
                <li key={a.index} className={`attempt attempt--${a.outcome}`}>
                  <div className="attempt__head">
                    <span className="attempt__index">{a.index + 1}</span>
                    <Link to={`/upstreams/${a.accountId}`}>{a.accountId}</Link>
                    <StatusBadge status={a.upstreamStatus} />
                    <span className="secondary">{OUTCOME_LABEL[a.outcome]}</span>
                    <span className="muted num" style={{ marginLeft: 'auto' }}>
                      {fmtMs(a.durationMs)}
                    </span>
                  </div>
                  <ul className="attempt__windows">
                    {a.windows.length > 0 &&
                      ATTEMPT_WINDOWS.map(({ id, label }) => {
                        const w = a.windows.find((x) => x.id === id) ?? null;
                        return (
                          <li key={id}>
                            <span className="muted">{label}</span>
                            <span className={`num ${w && w.utilization !== null ? '' : 'muted'}`}>{w ? (w.utilization === null ? 'utilization not reported' : `${fmtPct(w.utilization)} used`) : 'n/a'}</span>
                            <span className="muted" title={w ? fmtAbsolute(w.resetsAt) : undefined}>
                              {w ? `resets ${fmtRelative(w.resetsAt, now)}` : 'not reported'}
                            </span>
                          </li>
                        );
                      })}
                    <CacheRow attempt={a} usage={r.usage} />
                  </ul>
                  {a.selection && <SelectionTrace trace={a.selection} usage={r.usage} />}
                  {(a.response.stopReason || a.response.servedModel || (a.response.clearedInputTokens !== null && a.response.clearedInputTokens > 0)) && (
                    <div className="attempt__response secondary">
                      {a.response.stopReason && <span>stopped: {a.response.stopReason}{a.response.refusalCategory ? ` (${a.response.refusalCategory}${a.response.refusalRecommendedModel ? `, try ${a.response.refusalRecommendedModel}` : ''})` : ''}</span>}
                      {a.response.servedModel && <span className="mono">served by {a.response.servedModel}</span>}
                      {a.response.speed && <span>{a.response.speed}</span>}
                      {a.response.inferenceGeo && <span>{a.response.inferenceGeo}</span>}
                      {a.response.clearedInputTokens !== null && a.response.clearedInputTokens > 0 && <span>{fmtInt(a.response.clearedInputTokens)} tokens cleared by context edits</span>}
                    </div>
                  )}
                  {a.cooldownSet && (
                    <div className="attempt__cooldown">
                      <Badge tone="warning">cooldown set</Badge>
                      <span>
                        {a.cooldownSet.reason}, until <span title={fmtAbsolute(a.cooldownSet.until)}>{fmtRelative(a.cooldownSet.until, now)}</span>
                      </span>
                    </div>
                  )}
                </li>
              ))}
            </ol>
            {/* One builder sums every hold into the row and into the trace of the attempt placed after them, so the two
                agree only when an attempt followed every wait; a hold that ended with nothing dispatched, or one after
                an attempt that failed over, is named by no trace. */}
            {r.held && !r.attempts.some((a) => a.selection?.cohortHold?.heldMs === r.held?.heldMs) && (
              <p className="attempt__hold">
                held {fmtMs(r.held.heldMs)} on {heldOn(r.held)}
              </p>
            )}
            {trail.map((e) => {
              if (e.kind === 'pin_expired') {
                return (
                  <p className="attempt__migration" key={`${e.at}-${e.accountId ?? '-'}`} title={fmtAbsolute(e.at)}>
                    Session released from {e.accountId ? <Link to={`/upstreams/${e.accountId}`}>{e.accountId}</Link> : '-'}: {releaseLabel(e.detail?.reasons)}
                  </p>
                );
              }
              const account = e.accountId ? <Link to={`/upstreams/${e.accountId}`}>{e.accountId}</Link> : '-';
              if (e.kind === 'model_rejected') {
                return (
                  <p className="attempt__migration" key={`${e.kind}-${e.at}-${e.accountId ?? '-'}`} title={fmtAbsolute(e.at)}>
                    Model rejected by {account}: {String(e.detail?.wire_model ?? r.model)}
                  </p>
                );
              }
              if (e.kind === 'model_permitted') {
                const rejectedBy = Array.isArray(e.detail?.rejected_by) ? e.detail.rejected_by.map(String) : [];
                return (
                  <p className="attempt__migration" key={`${e.kind}-${e.at}-${e.accountId ?? '-'}`} title={fmtAbsolute(e.at)}>
                    Model permitted by {account} after{' '}
                    {rejectedBy.map((id, i) => (
                      <span key={id}>
                        {i > 0 && ', '}
                        <Link to={`/upstreams/${id}`}>{id}</Link>
                      </span>
                    ))}
                  </p>
                );
              }
              const reason = String(e.detail?.reason ?? '');
              const from = String(e.detail?.from_account ?? '-');
              return (
                <p className="attempt__migration" key={`${e.at}-${from}`} title={fmtAbsolute(e.at)}>
                  Session moved from <Link to={`/upstreams/${from}`}>{from}</Link> to{' '}
                  {e.accountId ? <Link to={`/upstreams/${e.accountId}`}>{e.accountId}</Link> : '-'}: {moveLabel(reason)}
                </p>
              );
            })}
          </section>

          <section>
            <h3 className="section-title">Usage</h3>
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
                  <td className="right num">
                    <b>{fmtUsd(r.costUsd)}</b>
                  </td>
                </tr>
              </tbody>
            </table>
          </section>

          <section>
            <h3 className="section-title">Captured bodies</h3>
            {!r.capturedBodies ? (
              <p className="muted">Not captured for this key. Enable body capture on the key to record the exchange.</p>
            ) : !canMutate ? (
              <p className="muted">Captured, but only the admin secret can read bodies.</p>
            ) : bodies.error ? (
              <ErrorState title="Could not load the captured bodies" body={bodies.error} action={<Button onClick={bodies.reload}>Try again</Button>} />
            ) : bodies.loading || otherRequestsBodies ? (
              <Skeleton height={120} />
            ) : bodies.data ? (
              <div className="bodies">
                <BodyBlock label="Request" value={bodies.data.request} dropped={bodies.data.requestTruncated ? bytesDropped(bodies.data.requestBytesSeen, bodies.data.requestBytesStored) : undefined} />
                <BodyBlock label="Response" value={bodies.data.response} dropped={bodies.data.responseTruncated ? bytesDropped(bodies.data.responseBytesSeen, bodies.data.responseBytesStored) : undefined} />
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
 * The cache diagnostics reading for one attempt, on the same three-slot grid as
 * the window rows above it. The usage is the request's: diagnostics and the
 * token counts both come off the response the final attempt drew.
 */
function CacheRow({ attempt, usage }: { attempt: Attempt; usage: Usage }) {
  const reason = attempt.cacheMissReason;
  const detail = () => {
    if (reason === null) return 'no response to read';
    if (reason === 'none') return `${fmtCompact(usage.cacheRead)} read, ${fmtCompact(usage.cacheWrite5m + usage.cacheWrite1h)} written`;
    if (isDivergence(reason)) return `diverged at ${reason.slice(0, -'_changed'.length)}`;
    return '';
  };
  return (
    <li>
      <span className="muted">Cache</span>
      <span className={reason === null || reason === 'not_requested' ? 'muted' : ''}>{reason === null ? 'n/a' : missLabel(reason)}</span>
      <span className="muted">{detail()}</span>
    </li>
  );
}

function BodyBlock({ label, value, dropped }: { dropped?: number; label: string; value: unknown }) {
  const text = JSON.stringify(value, null, 2);
  return (
    <div className="body-block">
      <div className="body-block__head">
        <span>{label}</span>
        {dropped !== undefined && <span className="muted">cut at 8 MiB, {fmtInt(dropped)} bytes dropped</span>}
        <CopyButton text={text} />
      </div>
      <pre className="body-block__pre">{text}</pre>
    </div>
  );
}
