import { Fragment, useMemo, useState, type MouseEvent, type ReactNode } from 'react';
import { Link, useNavigate } from 'react-router-dom';
import { api } from '@/api';
import { iso } from '@/api/decode';
import type { DecisionKind, DecisionSummary, FactEntity, RoutingLogQuery, RoutingLogRow } from '@/api/types';
import { type Paging, useFollow } from '@/app/useFollow';
import { DecisionCard } from '@/components/DecisionCard';
import { EventDetail } from '@/components/EventDetail';
import { MultiSelect, type Option } from '@/components/ui/MultiSelect';
import { IconChevronLeft, IconChevronRight, IconChevronsLeft, IconRefresh } from '@/components/ui/icons';
import { Badge, Button, Card, EmptyState, ErrorState, Input, Select, Skeleton, Toggle, useDescription } from '@/components/ui/primitives';
import { addressWindow, type LogFilters, ROUTING_LOG_CODEC, ROUTING_LOG_RANGES } from '@/lib/address';
import { clock } from '@/lib/clock';
import { FOLLOW_OVERRUN } from '@/lib/follow';
import { fmtAbsolute, fmtTime, toLocalInput, upperFirst } from '@/lib/format';
import { useAsync, useCommittedText, useQueryFilters } from '@/lib/hooks';
import { DECISION_KIND_LABEL, DECISION_KIND_TONE, EVENT_KIND_LABEL, type LogLink, aroundRequest, aroundSubject, byOf, clockMinute, compareModels, compareRows, entityHref, eventTone, factsOf, formatFact, identityOf, logHref } from '@/lib/routingLog';

const PAGE_SIZE = 25;

const DECISION_KINDS = Object.keys(DECISION_KIND_LABEL).sort() as DecisionKind[];
const HIDDEN_EVENT = 'warm_prefix_expired';
const EVENT_KINDS = Object.keys(EVENT_KIND_LABEL).filter((k) => k !== HIDDEN_EVENT);
/** The route's default set: every decision kind but a probe's and every event kind but a warm prefix's expiry. */
const DEFAULT_KINDS = [...DECISION_KINDS, ...EVENT_KINDS];
const KIND_OPTIONS: Option[] = [
  ...DECISION_KINDS.map((k) => ({ group: 'Decisions', label: upperFirst(DECISION_KIND_LABEL[k]), value: k })),
  { group: 'Decisions', label: 'Probe decisions (hidden by default)', value: 'probe' },
  ...EVENT_KINDS.map((k) => ({ group: 'Events', label: upperFirst(EVENT_KIND_LABEL[k]), value: k })).sort((a, b) => a.label.localeCompare(b.label)),
  { group: 'Events', label: 'Warm prefix expired (hidden by default)', value: HIDDEN_EVENT },
];
const kindWords = (kind: string) => (kind === 'probe' ? 'probe decisions' : (DECISION_KIND_LABEL[kind as DecisionKind] ?? EVENT_KIND_LABEL[kind] ?? kind));

const DEFAULTS: LogFilters = { accounts: [], keys: [], kinds: [], models: [], pools: [], range: { from: '', preset: '1h', to: '' }, request: '', session: '' };

const sameSet = (a: string[], b: string[]) => a.length === b.length && a.every((x) => b.includes(x));

const Dash = () => <span className="muted">-</span>;

/** Every entity a row names is a link to the view of its kind. */
const EntityLink = ({ children, id, kind, mono = false }: { children?: ReactNode; id: string; kind: FactEntity['kind'] | 'request' | 'session'; mono?: boolean }) => (
  <Link to={entityHref(kind, id)} className={mono ? 'mono' : undefined} title={id}>
    {children ?? id}
  </Link>
);

export function RoutingLogPage() {
  const { address, filters, replace, update } = useQueryFilters(ROUTING_LOG_CODEC);
  const navigate = useNavigate();
  const keys = useAsync(() => api.listKeys(), []);
  const pools = useAsync(() => api.listPools(), []);
  const accounts = useAsync(() => api.listAccounts(), []);
  const catalog = useAsync(() => api.listCatalog(), []);

  // The cursor and the pages before it belong to the address they were read under, so a filter change drops them in
  // the render that brings it, and one list request follows.
  // `to` holds the range's end a walk from Follow's overrun reads under, the end its cursor came from.
  const [paging, setPaging] = useState<Paging>({ for: address, stack: [] });
  if (paging.for !== address) setPaging({ for: address, stack: [] });
  const cursor = paging.for === address ? paging.cursor : undefined;
  const stack = paging.for === address ? paging.stack : [];
  const pagedTo = paging.for === address ? paging.to : undefined;
  // The open rows belong to the page they were opened on, so another page or another filter starts with every row shut.
  const pageKey = `${cursor ?? ''}|${address}`;
  const [opened, setOpened] = useState<{ for: string; ids: Set<string> }>({ for: pageKey, ids: new Set() });
  if (opened.for !== pageKey) setOpened({ for: pageKey, ids: new Set() });
  const open = opened.for === pageKey ? opened.ids : new Set<string>();
  const toggle = (id: string) =>
    setOpened((o) => {
      const ids = new Set(o.for === pageKey ? o.ids : []);
      if (ids.has(id)) ids.delete(id);
      else ids.add(id);
      return { for: pageKey, ids };
    });
  const widen = (link: LogLink) => navigate(logHref(link));
  // The last hour ends where the list was loaded, so a cursor page is read against the window its cursor came from.
  // Refresh moves the end to now.
  const [loadedAt, setLoadedAt] = useState(() => clock.now());
  const [customOpen, setCustomOpen] = useState(false);
  const [follow, setFollow] = useState(false);
  const session = useCommittedText(filters.session, (next) => update({ session: next.trim() }));
  const request = useCommittedText(filters.request, (next) => update({ request: next.trim() }));

  // A preset ends where the list was loaded, and a custom span reads its own instants.
  const range = useMemo(() => addressWindow(filters.range, ROUTING_LOG_RANGES, '1h', loadedAt), [filters.range, loadedAt]);
  const query = useMemo<RoutingLogQuery>(
    () => ({ accountIds: filters.accounts, cursor, from: iso(range.from), keyIds: filters.keys, kinds: filters.kinds.length ? filters.kinds : undefined, limit: PAGE_SIZE, models: filters.models, poolIds: filters.pools, request: filters.request || undefined, session: filters.session || undefined, to: iso(pagedTo ?? range.to) }),
    [filters, range, cursor, pagedTo],
  );
  const log = useAsync(() => api.listRoutingLog(query), [query]);

  const { fresh, goNext, goPrevious, nextCursor, overrun, rows, setFollowed, toggleFollow } = useFollow<RoutingLogRow>({
    address,
    compare: compareRows,
    identity: identityOf,
    list: log.data,
    on: follow && cursor === undefined,
    paging,
    read: (c, to) => api.listRoutingLog({ ...query, cursor: c, to: iso(to) }),
    setFollow,
    setPaging,
    stale: log.stale,
  });

  const rangeValue = customOpen || filters.range.preset === 'custom' ? 'custom' : filters.range.preset;
  // Custom starts from the window the list shows, so the choice holds.
  const chooseRange = (value: string) => {
    setCustomOpen(value === 'custom');
    update({ range: value === 'custom' ? { from: iso(range.from), preset: 'custom', to: iso(range.to) } : { from: '', preset: value, to: '' } });
  };
  // A bound that would end the span before it starts writes nothing, since the address would read that pair as the
  // preset.
  const setBound = (bound: 'from' | 'to', local: string) => {
    const ms = Date.parse(local);
    if (Number.isNaN(ms)) return;
    const next = bound === 'from' ? { from: ms, to: range.to } : { from: range.from, to: Math.min(ms, clock.now()) };
    if (next.from >= next.to) return;
    update({ range: { from: iso(next.from), preset: 'custom', to: iso(next.to) } });
  };
  // A preset is read again from now, and a custom span as it stands.
  const refresh = () => {
    setPaging({ for: address, stack: [] });
    setFollowed(null);
    setLoadedAt(clock.now());
    log.reload();
  };
  const clear = () => {
    setCustomOpen(false);
    replace(DEFAULTS);
  };

  const now = clock.now();
  const narrowed = filters.session !== '' || filters.accounts.length > 0 || filters.pools.length > 0 || filters.keys.length > 0 || filters.models.length > 0 || filters.kinds.length > 0 || rangeValue === 'custom';

  return (
    <div className="routing-log stack">
      <div className="filters">
        <Select value={rangeValue} onChange={(e) => chooseRange(e.target.value)} aria-label="Range">
          {ROUTING_LOG_RANGES.map((r) => (
            <option key={r.name} value={r.name}>
              {r.label}
            </option>
          ))}
          <option value="custom">Custom range</option>
        </Select>
        {rangeValue === 'custom' && (
          <>
            <Input type="datetime-local" aria-label="From" value={toLocalInput(range.from)} onChange={(e) => setBound('from', e.target.value)} />
            <Input type="datetime-local" aria-label="To" value={toLocalInput(range.to)} onChange={(e) => setBound('to', e.target.value)} />
          </>
        )}
        <MultiSelect
          label="Kinds"
          options={KIND_OPTIONS}
          value={filters.kinds.length ? filters.kinds : DEFAULT_KINDS}
          active={filters.kinds.length > 0}
          summary={filters.kinds.length ? undefined : 'Kinds (2 hidden)'}
          onChange={(v) => update({ kinds: v.length === 0 || sameSet(v, DEFAULT_KINDS) ? [] : v })}
        />
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
          options={(catalog.data ?? [])
            .map((m) => m.id)
            .sort(compareModels)
            .map((id) => ({ label: id, value: id }))}
          value={filters.models}
          onChange={(v) => update({ models: v })}
          disabled={catalog.data === null && catalog.error !== null}
          description={catalog.error ? `The model list did not load: ${catalog.error}` : undefined}
          searchable
          mono
        />
        <Input className="mono routing-log__id" placeholder="Session id" aria-label="Session id" value={session.draft} onChange={(e) => session.onChange(e.target.value)} onBlur={session.onBlur} onKeyDown={session.onKeyDown} />
        <Input className="mono routing-log__id" placeholder="Request id" aria-label="Request id" value={request.draft} onChange={(e) => request.onChange(e.target.value)} onBlur={request.onBlur} onKeyDown={request.onKeyDown} />
        <label className="row routing-log__follow">
          <Toggle checked={follow} onChange={toggleFollow} label="Follow" />
          <span className="secondary">{follow ? 'polling every 5 s, the range ends now' : 'Follow'}</span>
        </label>
        <div className="filters__cols">
          <Button size="sm" icon={<IconRefresh size={14} />} onClick={refresh} title={`Rows up to ${fmtTime(iso(range.to))}. Refresh reads up to now.`}>
            Refresh
          </Button>
        </div>
      </div>

      {filters.request !== '' ? (
        <RequestScope requestId={filters.request} rows={rows} counts={log.data?.counts ?? null} now={now} onClear={clear} />
      ) : narrowed ? (
        <Narrowing filters={filters} range={range} rows={rows.length} more={nextCursor !== null} now={now} onClear={clear} />
      ) : null}

      {overrun !== null && (
        <div className="callout callout--warning routing-log__overrun" role="status">
          {FOLLOW_OVERRUN}
        </div>
      )}

      <Card padded={false} className="card--table">
        <div className="table-wrap">
          <table className="table routing-log__table">
            <thead>
              <tr>
                <th className="routing-log__chevron" />
                <th className="routing-log__time">Time</th>
                <th className="routing-log__kind">Kind</th>
                <th className="routing-log__session">Session</th>
                <th className="routing-log__account">Account</th>
                <th className="routing-log__model">Model</th>
                <th className="routing-log__by">By</th>
                <th className="routing-log__fact">Fact</th>
                <th className="routing-log__link" />
              </tr>
            </thead>
            <tbody>
              {log.loading && !log.data
                ? Array.from({ length: 8 }, (_, i) => (
                    <tr key={i}>
                      <td colSpan={9}>
                        <Skeleton />
                      </td>
                    </tr>
                  ))
                : rows.map((row, i) => {
                    const id = identityOf(row);
                    return (
                      <Fragment key={id}>
                        <LogRow row={row} rows={rows} isNew={fresh.has(id)} now={now} open={open.has(id)} striped={i % 2 === 1} onToggle={() => toggle(id)} />
                        {open.has(id) && (
                          <tr className="routing-log__panel">
                            <td colSpan={9}>{row.type === 'decision' ? <DecisionCard now={now} onWiden={widen} row={row} /> : <EventDetail now={now} row={row} rows={rows} />}</td>
                          </tr>
                        )}
                      </Fragment>
                    );
                  })}
              {!log.loading && rows.length === 0 && (
                <tr>
                  <td colSpan={9}>
                    {log.error ? (
                      <ErrorState title="Could not load the routing log" body={log.error} action={<Button onClick={log.reload}>Try again</Button>} />
                    ) : (
                      <EmptyState title="No decisions or events in this range." body="Widen the range or clear a filter." />
                    )}
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
        <footer className="pager">
          <span className="muted num">
            {rows.length === 0 ? 'No rows' : `${rows.length} shown`}
            {nextCursor ? ', more available' : ''}
          </span>
          <div className="pager__nav">
            <button type="button" onClick={() => setPaging({ for: address, stack: [] })} disabled={stack.length === 0} aria-label="First page">
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
  );
}

const plural = (n: number, one: string) => `${n} ${one}${n === 1 ? '' : 's'}`;

/** The line a request filter opens: the request's counts, the three widenings around its time, and Clear. */
function RequestScope({ counts, now, onClear, requestId, rows }: { counts: { decisions: number; events: number } | null; now: number; onClear: () => void; requestId: string; rows: RoutingLogRow[] }) {
  const decisions = rows.filter((r): r is DecisionSummary => r.type === 'decision' && r.requestId === requestId);
  // The rows run newest first, so the request's first decision is the last and its final placement the first.
  const at = decisions.length ? Date.parse(decisions[decisions.length - 1].at) : null;
  const sessionId = decisions.find((d) => d.sessionId !== null)?.sessionId ?? null;
  const accountId = decisions.find((d) => d.accountId !== null)?.accountId ?? null;
  const widenings = [
    { href: at !== null && sessionId !== null ? logHref({ session: sessionId, ...aroundSubject(at, now) }) : null, label: 'this session', title: sessionId },
    { href: at !== null && accountId !== null ? logHref({ accounts: [accountId], ...aroundRequest(at, null, now) }) : null, label: 'this account', title: accountId },
    { href: at === null ? null : logHref(clockMinute(at, now)), label: 'this minute', title: null },
  ].filter((w): w is { href: string; label: string; title: string | null } => w.href !== null);
  return (
    <div className="routing-log__scope">
      <span>
        Request{' '}
        <b>
          <EntityLink kind="request" id={requestId} mono />
        </b>
        {counts && `: ${plural(counts.decisions, 'decision')}, ${plural(counts.events, 'event')}`}
      </span>
      {widenings.length > 0 && (
        <span className="routing-log__widen">
          Widen to
          {widenings.map((w) => (
            <Link key={w.label} to={w.href} title={w.title ?? undefined}>
              {w.label}
            </Link>
          ))}
        </span>
      )}
      <button type="button" className="link-btn routing-log__clear" onClick={onClear}>
        Clear
      </button>
    </div>
  );
}

/** The line any other narrowing opens: what the rows are narrowed to, the range, and the page's count. */
function Narrowing({ filters, more, now, onClear, range, rows }: { filters: LogFilters; more: boolean; now: number; onClear: () => void; range: { from: number; to: number }; rows: number }) {
  const parts: ReactNode[] = [];
  const entities = (kind: FactEntity['kind'], ids: string[]) => {
    if (ids.length === 0) return;
    parts.push(
      <Fragment key={kind}>
        {ids.length === 1 ? kind : `${kind}s`}{' '}
        {ids.map((id, i) => (
          <Fragment key={id}>
            {i > 0 && ', '}
            <b>
              <EntityLink kind={kind} id={id} />
            </b>
          </Fragment>
        ))}
      </Fragment>,
    );
  };
  if (filters.session) {
    parts.push(
      <Fragment key="session">
        session{' '}
        <b>
          <EntityLink kind="session" id={filters.session} mono>
            {filters.session.slice(0, 8)}
          </EntityLink>
        </b>
      </Fragment>,
    );
  }
  entities('account', filters.accounts);
  entities('pool', filters.pools);
  entities('key', filters.keys);
  entities('model', filters.models);
  if (filters.kinds.length) {
    parts.push(
      <Fragment key="kinds">
        {filters.kinds.length === 1 ? 'kind' : 'kinds'} <b>{filters.kinds.map(kindWords).join(', ')}</b>
      </Fragment>,
    );
  }
  return (
    <div className="routing-log__scope">
      <span>
        Narrowed to{' '}
        {parts.map((p, i) => (
          <Fragment key={i}>
            {i > 0 && ', '}
            {p}
          </Fragment>
        ))}
        {parts.length > 0 && ', '}
        {formatFact(range.from, 'time', now)} to {formatFact(range.to, 'time', now)}: {plural(rows, 'row')}
        {more && ', more'}
      </span>
      <button type="button" className="link-btn routing-log__clear" onClick={onClear}>
        Clear
      </button>
    </div>
  );
}

/** The account a row is about: a move draws where it left and where it landed, a pool or key event its subject. */
function AccountCell({ row }: { row: RoutingLogRow }) {
  const from = row.type === 'decision' ? row.fromAccountId : row.kind === 'pin_migrated' && typeof row.detail.from_account === 'string' ? row.detail.from_account : null;
  if (row.accountId !== null) {
    return (
      <span className="routing-log__cut">
        {from !== null && (
          <>
            <EntityLink kind="account" id={from} /> <span className="muted">to</span>{' '}
          </>
        )}
        <EntityLink kind="account" id={row.accountId} />
      </span>
    );
  }
  if (row.type === 'event' && row.poolId !== null) {
    return (
      <span className="routing-log__cut">
        <span className="muted">pool</span> <EntityLink kind="pool" id={row.poolId} />
      </span>
    );
  }
  if (row.type === 'event' && row.keyId !== null) {
    return (
      <span className="routing-log__cut">
        <span className="muted">key</span> <EntityLink kind="key" id={row.keyId} />
      </span>
    );
  }
  return <Dash />;
}

/** A catalog change that names one model draws it as the row's model. */
const modelOf = (row: RoutingLogRow) => {
  if (row.model !== null || row.type !== 'event' || row.kind !== 'catalog_changed') return row.model;
  const named = [row.detail.added, row.detail.removed].flatMap((v) => (Array.isArray(v) ? v : []));
  return named.length === 1 && typeof named[0] === 'string' ? named[0] : null;
};

function LogRow({ isNew, now, onToggle, open, row, rows, striped }: { isNew: boolean; now: number; onToggle: () => void; open: boolean; row: RoutingLogRow; rows: RoutingLogRow[]; striped: boolean }) {
  const by = byOf(row, rows);
  const described = useDescription(by.title);
  const facts = factsOf(row, now, rows);
  const model = modelOf(row);
  const className = ['routing-log__row', isNew && 'is-new', open && 'is-open', striped && 'is-striped'].filter(Boolean).join(' ');
  // A click on a link or a button inside the row follows it, and anywhere else opens the row.
  const click = (e: MouseEvent<HTMLTableRowElement>) => {
    if (!(e.target as Element).closest('a, button')) onToggle();
  };
  return (
    <tr
      className={className}
      data-row={identityOf(row)}
      tabIndex={0}
      aria-expanded={open}
      onClick={click}
      onKeyDown={(e) => {
        if (e.target !== e.currentTarget || (e.key !== 'Enter' && e.key !== ' ')) return;
        e.preventDefault();
        onToggle();
      }}
    >
      <td className="routing-log__chevron" aria-hidden>
        <IconChevronRight size={14} />
      </td>
      <td className="num routing-log__time" title={fmtAbsolute(row.at)}>
        {fmtTime(row.at)}
        {row.type === 'decision' && (
          <span className="sub mono routing-log__cut">
            <EntityLink kind="request" id={row.requestId} />
          </span>
        )}
      </td>
      <td className="routing-log__kind">
        {row.type === 'decision' ? <Badge tone={DECISION_KIND_TONE[row.kind]}>{DECISION_KIND_LABEL[row.kind]}</Badge> : <span className={`badge badge--${eventTone(row.kind)} routing-log__outline`}>{EVENT_KIND_LABEL[row.kind] ?? row.kind.replace(/_/g, ' ')}</span>}
        {/* An exhaustion after a failed attempt counts one past the request's attempts, so it draws no marker. */}
        {row.type === 'decision' && row.attempt > 1 && (row.attemptCount === null || row.attempt <= row.attemptCount) && <span className="sub">{row.attemptCount === null ? `attempt ${row.attempt}` : `attempt ${row.attempt}/${row.attemptCount}`}</span>}
      </td>
      <td className="routing-log__session">
        {row.sessionId === null ? (
          <Dash />
        ) : (
          <span className="routing-log__cut">
            <EntityLink kind="session" id={row.sessionId} mono>
              {row.sessionId.slice(0, 8)}
            </EntityLink>
          </span>
        )}
        {row.agentId !== null && <span className="sub mono routing-log__cut">{row.agentId}</span>}
      </td>
      <td className="routing-log__account">
        <AccountCell row={row} />
        {model !== null && (
          <span className="sub mono routing-log__cut routing-log__model-under">
            <EntityLink kind="model" id={model} />
          </span>
        )}
      </td>
      <td className="routing-log__model">
        {model === null ? (
          <Dash />
        ) : (
          <span className="mono routing-log__cut">
            <EntityLink kind="model" id={model} />
          </span>
        )}
      </td>
      <td className="routing-log__by">
        <span className="chip routing-log__chip" title={by.title ?? by.value} aria-describedby={described && !described.sibling ? described.id : undefined}>
          {by.label && <span className="chip__label">{by.label}</span>}
          {by.value}
        </span>
      </td>
      <td className="routing-log__fact">
        {facts.length === 0 ? (
          <Dash />
        ) : (
          <div className="fact">
            {facts.map((p) => (
              <div className="fact__pair" key={p.label}>
                <span className="fact__label">{p.label}</span>
                <span className="fact__value mono" title={p.value}>
                  {p.entity ? <EntityLink kind={p.entity.kind} id={p.entity.id}>{p.value}</EntityLink> : p.value}
                </span>
              </div>
            ))}
          </div>
        )}
      </td>
      <td className="routing-log__link">{row.requestId === null ? <Dash /> : <Link to={entityHref('request', row.requestId)}>Details</Link>}</td>
    </tr>
  );
}
