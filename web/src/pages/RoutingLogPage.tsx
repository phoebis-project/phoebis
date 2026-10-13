import { Fragment, useMemo, useState, type MouseEvent, type ReactNode } from 'react';
import { Link, useNavigate } from 'react-router-dom';
import { api } from '@/api';
import { iso } from '@/api/decode';
import type { DecisionKind, FactEntity, RoutingLogQuery, RoutingLogRow } from '@/api/types';
import { type Paging, useFollow } from '@/app/useFollow';
import { DecisionCard } from '@/components/DecisionCard';
import { EventDetail } from '@/components/EventDetail';
import { FilterBar } from '@/components/FilterBar';
import type { Option } from '@/components/ui/OptionList';
import { IconChevronLeft, IconChevronRight, IconChevronsLeft } from '@/components/ui/icons';
import { Badge, Button, Card, EmptyState, ErrorState, Skeleton, useDescription } from '@/components/ui/primitives';
import { addressWindow, type LogFilters, RANGE_PRESETS, ROUTING_LOG_CODEC } from '@/lib/address';
import { clock } from '@/lib/clock';
import { chipText, notLoadedReason, ROUTING_LOG_DIMENSIONS } from '@/lib/filterBar';
import { effectiveInterval, FOLLOW_OVERRUN, readEnd, spanEnded } from '@/lib/follow';
import { fmtAbsolute, fmtInt, fmtTime, upperFirst } from '@/lib/format';
import { useAsync, useQueryFilters, useRangeTakenAt } from '@/lib/hooks';
import { ALL_KINDS, byOf, compareModels, compareRows, DECISION_KIND_LABEL, DECISION_KIND_TONE, entityHref, EVENT_KIND_LABEL, eventTone, factsOf, identityOf, KIND_PRESETS, type LogLink, logHref } from '@/lib/routingLog';

const PAGE_SIZE = 25;

const DECISION_KINDS = Object.keys(DECISION_KIND_LABEL).sort() as DecisionKind[];
const KIND_OPTIONS: Option[] = [
  ...DECISION_KINDS.map((k) => ({ group: 'Decisions', label: upperFirst(DECISION_KIND_LABEL[k]), value: k })),
  // The pseudo-kind ends its group, since it names the probes among the six kinds above it rather than a kind of its own.
  { group: 'Decisions', label: 'Probe decisions', value: 'probe' },
  ...Object.keys(EVENT_KIND_LABEL)
    .map((k) => ({ group: 'Events', label: upperFirst(EVENT_KIND_LABEL[k]), value: k }))
    .sort((a, b) => a.label.localeCompare(b.label)),
];
const KIND_PRESET_ROWS = { kinds: KIND_PRESETS };

// Compared by the set's size, since a list that repeats one kind and leaves another out is as long as every kind.
const sameSet = (a: readonly string[], b: readonly string[]) => new Set(a).size === b.length && a.every((x) => b.includes(x));

const Dash = () => <span className="muted">-</span>;

/** Every entity a row names is a link to the view of its kind. */
const EntityLink = ({ children, id, kind, mono = false }: { children?: ReactNode; id: string; kind: FactEntity['kind'] | 'request' | 'session'; mono?: boolean }) => (
  <Link to={entityHref(kind, id)} className={mono ? 'mono' : undefined} title={id}>
    {children ?? id}
  </Link>
);

const plural = (n: number, one: string) => `${n} ${one}${n === 1 ? '' : 's'}`;

export function RoutingLogPage() {
  const { address, filters, update: rawUpdate } = useQueryFilters(ROUTING_LOG_CODEC);
  // A set equal to every kind writes no key, since an absent `kinds` reads every kind.
  const update = (patch: Partial<LogFilters>) => rawUpdate(patch.kinds !== undefined && sameSet(patch.kinds, ALL_KINDS) ? { ...patch, kinds: [] } : patch);
  const navigate = useNavigate();
  const keys = useAsync(() => api.listKeys(), []);
  const pools = useAsync(() => api.listPools(), []);
  const accounts = useAsync(() => api.listAccounts(), []);
  const catalog = useAsync(() => api.listCatalog(), []);

  // The cursor and the pages before it belong to the address they were read under, so a filter change drops them in
  // the render that brings it, and one list request follows.
  // `to` holds the range's end a walk from the poll's overrun reads under, the end its cursor came from.
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
  // Off on the logs until the reader picks an interval, and page state, so a link or a reload opens the log off.
  const [intervalMs, setIntervalMs] = useState(0);
  const [barOpen, setBarOpen] = useState(false);
  const rangeTakenAt = useRangeTakenAt(filters.range);

  // A preset ends where the list was loaded, and a custom span reads its own instants, which Refresh reads again.
  const custom = filters.range.preset === 'custom';
  const range = useMemo(() => addressWindow(filters.range, RANGE_PRESETS, '1h', loadedAt), [filters.range, loadedAt]);
  const ended = spanEnded(custom, range.to, rangeTakenAt);
  const polledMs = effectiveInterval(intervalMs, ended);
  const query = useMemo<RoutingLogQuery>(
    () => ({ accountIds: filters.accounts, cursor, from: iso(range.from), keyIds: filters.keys, kinds: filters.kinds.length ? filters.kinds : undefined, limit: PAGE_SIZE, models: filters.models, poolIds: filters.pools, request: filters.request || undefined, session: filters.session || undefined, to: iso(readEnd(custom, range.to, pagedTo ?? range.to)) }),
    [filters, custom, range, cursor, pagedTo],
  );
  const log = useAsync(() => api.listRoutingLog(query), [query]);

  // An open menu or popover under the bar stands over the list, so the poll waits for it to close.
  const { error, fresh, goNext, goPrevious, nextCursor, overrun, rows, setFollowed } = useFollow<RoutingLogRow>({
    address,
    compare: compareRows,
    error: log.error,
    identity: identityOf,
    intervalMs: polledMs,
    list: log.data,
    paging,
    paused: barOpen,
    read: (c, to) => api.listRoutingLog({ ...query, cursor: c, to: iso(readEnd(custom, range.to, to)) }),
    setPaging,
    stale: log.stale,
  });

  // A preset is read again from now, and a custom span as it stands.
  const refresh = () => {
    setPaging({ for: address, stack: [] });
    setFollowed(null);
    setLoadedAt(clock.now());
    log.reload();
  };

  const now = clock.now();
  // An address that lists every kind is every kind, which draws no chip and every row of the Kinds list checked.
  const barFilters = useMemo(() => (sameSet(filters.kinds, ALL_KINDS) ? { ...filters, kinds: [] } : filters), [filters]);
  const counts = log.data?.counts ?? null;
  // The request chip keeps its id form and carries the route's counts for the request in its title.
  const dimensions = useMemo(
    () =>
      ROUTING_LOG_DIMENSIONS.map((d) =>
        d.field !== 'request'
          ? d
          : {
              ...d,
              text: (value: unknown) => {
                const chip = chipText(d, value);
                return counts ? { ...chip, title: `${String(value)}: ${plural(counts.decisions, 'decision')}, ${plural(counts.events, 'event')}` } : chip;
              },
            },
      ),
    [counts],
  );
  const options: Record<string, Option[]> = {
    accounts: (accounts.data ?? []).map((a) => ({ label: a.label, value: a.id })),
    keys: (keys.data ?? []).map((k) => ({ label: k.name, value: k.id })),
    kinds: KIND_OPTIONS,
    models: (catalog.data ?? [])
      .map((m) => m.id)
      .sort(compareModels)
      .map((id) => ({ label: id, value: id })),
    pools: (pools.data ?? []).map((p) => ({ label: p.name, value: p.id })),
  };
  const disabled = { ...notLoadedReason('keys', 'key', keys), ...notLoadedReason('pools', 'pool', pools), ...notLoadedReason('accounts', 'account', accounts), ...notLoadedReason('models', 'model', catalog) };

  return (
    <div className="routing-log stack">
      <FilterBar
        address={address}
        count={rows.length > 0 ? `${fmtInt(rows.length)} shown${nextCursor ? ', more available' : ''}` : log.data === null ? '' : 'No rows'}
        dimensions={dimensions}
        disabled={disabled}
        filters={barFilters}
        onOpenChange={setBarOpen}
        options={options}
        presets={KIND_PRESET_ROWS}
        range={{ now: clock.now, window: range }}
        refresh={{
          disabled: ended ? 'A custom range that has ended has nothing new to read' : undefined,
          intervalMs,
          onIntervalChange: setIntervalMs,
          onRefresh: refresh,
          paused: barOpen ? 'menu' : cursor !== undefined ? 'olderPage' : null,
          refreshTitle: custom ? `Rows from ${fmtTime(iso(range.from))} to ${fmtTime(iso(range.to))}. Refresh reads the same window again` : `Rows up to ${fmtTime(iso(range.to))}. Refresh reads up to now`,
        }}
        update={update}
      />

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
                    {error ? (
                      <ErrorState title="Could not load the routing log" body={error} action={<Button onClick={log.reload}>Try again</Button>} />
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
