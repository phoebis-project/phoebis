import { Fragment, useMemo, useState, type ReactNode } from 'react';
import { Link, useNavigate } from 'react-router-dom';
import { api } from '@/api';
import { iso } from '@/api/decode';
import type { DecisionKind, FactEntity, RoutingLogQuery, RoutingLogRow } from '@/api/types';
import { useAppSettings } from '@/app/settings';
import { useFollow, usePaging } from '@/app/useFollow';
import { ChipView } from '@/components/ChipView';
import { DecisionCard } from '@/components/DecisionCard';
import { EventDetail } from '@/components/EventDetail';
import { FilterBar } from '@/components/FilterBar';
import type { Option } from '@/components/ui/OptionList';
import { IconChevronRight } from '@/components/ui/icons';
import { PagerNav } from '@/components/ui/PagerNav';
import { PopoverHost, useHoldHost } from '@/components/ui/Popover';
import { Badge, Button, Card, EmptyState, ErrorState, Skeleton, toggleRow, useHostedDescription } from '@/components/ui/primitives';
import { addressWindow, type LogFilters, ROUTING_LOG_CODEC } from '@/lib/address';
import { clock } from '@/lib/clock';
import { tierOrder, weekReadingOf } from '@/lib/costs';
import { chipText, notLoadedReason, ROUTING_LOG_DIMENSIONS } from '@/lib/filterBar';
import { effectiveInterval, ENDED_RANGE, FOLLOW_OVERRUN, readEnd, shownCount, spanEnded } from '@/lib/follow';
import { fmtAbsolute, fmtDatedTime, fmtTime, plural, upperFirst } from '@/lib/format';
import { sameMembers } from '@/lib/forms';
import { toggled, useAsync, useQueryFilters, useRangeTakenAt } from '@/lib/hooks';
import { ALL_KINDS, byOf, catalogModel, compareRows, DECISION_KIND_LABEL, DECISION_KIND_TONE, entityHref, EVENT_KIND_LABEL, eventTone, factsOf, identityOf, KIND_PRESETS, type LogLink, logHref, movedFrom, registryTarget, words } from '@/lib/routingLog';

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

const Dash = () => <span className="muted">-</span>;

/** Every entity a row names is a link to the view of its kind. */
const EntityLink = ({ children, id, kind, mono = false }: { children?: ReactNode; id: string; kind: FactEntity['kind'] | 'request' | 'session'; mono?: boolean }) => (
  <Link to={entityHref(kind, id)} className={mono ? 'mono' : undefined} title={id}>
    {children ?? id}
  </Link>
);

export function RoutingLogPage() {
  const { address, filters, update: rawUpdate } = useQueryFilters(ROUTING_LOG_CODEC);
  // A set equal to every kind writes no key, since an absent `kinds` reads every kind.
  const update = (patch: Partial<LogFilters>) => rawUpdate(patch.kinds !== undefined && sameMembers(patch.kinds, ALL_KINDS) ? { ...patch, kinds: [] } : patch);
  const navigate = useNavigate();
  const keys = useAsync(() => api.listKeys(), []);
  const pools = useAsync(() => api.listPools(), []);
  const accounts = useAsync(() => api.listAccounts(), []);
  const catalog = useAsync(() => api.listCatalog(), []);
  const { modelTiers, week } = useAppSettings();
  // The cost model whose capacities scale a shedding round's Pro-week rewrite to the chosen week. The Pro week reads
  // without it.
  const settings = useAsync(() => api.getSettings(), []);
  const reading = weekReadingOf(settings.data?.subscriptionCosts ?? null, settings.error, week);
  const modelOptions = useMemo<Option[]>(() => tierOrder((catalog.data ?? []).map((m) => m.id), modelTiers).map((id) => ({ label: id, value: id })), [catalog.data, modelTiers]);

  // `pagedTo` holds the range's end a walk from the poll's overrun reads under, the end its cursor came from.
  const { cursor, pagedTo, reset: resetPaging, setPaging, stack } = usePaging(address);
  // The open rows belong to the page they were opened on, so another page or another filter starts with every row shut.
  const pageKey = `${cursor ?? ''}|${address}`;
  const [opened, setOpened] = useState<{ for: string; ids: Set<string> }>({ for: pageKey, ids: new Set() });
  if (opened.for !== pageKey) setOpened({ for: pageKey, ids: new Set() });
  const open = opened.for === pageKey ? opened.ids : new Set<string>();
  const toggle = (id: string) => setOpened((o) => ({ for: pageKey, ids: toggled(o.for === pageKey ? o.ids : new Set(), id) }));
  const widen = (link: LogLink) => navigate(logHref(link));
  // The last hour ends where the list was loaded, so a cursor page is read against the window its cursor came from.
  // Refresh moves the end to now.
  const [loadedAt, setLoadedAt] = useState(() => clock.now());
  // Off on the logs until the reader picks an interval, and page state, so a link or a reload opens the log off.
  const [intervalMs, setIntervalMs] = useState(0);
  const { cause: holdCause, host } = useHoldHost();
  const rangeTakenAt = useRangeTakenAt(filters.range);

  // A preset ends where the list was loaded, and a custom span reads its own instants, which Refresh reads again.
  const custom = filters.range.preset === 'custom';
  const range = useMemo(() => addressWindow(filters.range, '1h', loadedAt), [filters.range, loadedAt]);
  const ended = spanEnded(custom, range.to, rangeTakenAt);
  const polledMs = effectiveInterval(intervalMs, ended);
  const query = useMemo<RoutingLogQuery>(
    () => ({ accountIds: filters.accounts, agentId: filters.agentId || undefined, cursor, from: iso(range.from), keyIds: filters.keys, kinds: filters.kinds.length ? filters.kinds : undefined, limit: PAGE_SIZE, models: filters.models, poolIds: filters.pools, request: filters.request || undefined, session: filters.session || undefined, to: iso(readEnd(custom, range.to, pagedTo ?? range.to)) }),
    [filters, custom, range, cursor, pagedTo],
  );
  const log = useAsync(() => api.listRoutingLog(query), [query]);

  // An open menu or popover under the bar stands over the list, so the poll waits for it to close.
  const { error, fresh, goNext, goPrevious, nextCursor, overrun, rows, setFollowed } = useFollow<RoutingLogRow>({
    address,
    compare: compareRows,
    cursor,
    error: log.error,
    fixedEnd: false,
    identity: identityOf,
    intervalMs: polledMs,
    list: log.data,
    pagedTo,
    paused: holdCause !== null,
    read: (c, to) => api.listRoutingLog({ ...query, cursor: c, to: iso(readEnd(custom, range.to, to)) }),
    setPaging,
    stack,
    stale: log.stale,
  });

  // A preset is read again from now, and a custom span as it stands.
  const refresh = () => {
    resetPaging();
    setFollowed(null);
    setLoadedAt(clock.now());
    log.reload();
    settings.reload();
  };

  const now = clock.now();
  // An address that lists every kind is every kind, which draws no chip and every row of the Kinds list checked.
  const barFilters = useMemo(() => (sameMembers(filters.kinds, ALL_KINDS) ? { ...filters, kinds: [] } : filters), [filters]);
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
    models: modelOptions,
    pools: (pools.data ?? []).map((p) => ({ label: p.name, value: p.id })),
  };
  const disabled = { ...notLoadedReason('keys', 'key', keys), ...notLoadedReason('pools', 'pool', pools), ...notLoadedReason('accounts', 'account', accounts), ...notLoadedReason('models', 'model', catalog) };

  return (
    <PopoverHost.Provider value={host}>
      <div className="routing-log stack">
        <FilterBar
          address={address}
          count={shownCount(rows, nextCursor, log.data !== null)}
          dimensions={dimensions}
          disabled={disabled}
          filters={barFilters}
          options={options}
          presets={KIND_PRESET_ROWS}
          range={{ now: clock.now, window: range }}
          refresh={{
            disabled: ended ? ENDED_RANGE : undefined,
            intervalMs,
            onIntervalChange: setIntervalMs,
            onRefresh: refresh,
            paused: holdCause ?? (cursor !== undefined ? 'olderPage' : null),
            refreshTitle: custom ? `Rows from ${fmtTime(range.from)} to ${fmtTime(range.to)}. Refresh reads the same window again` : `Rows up to ${fmtTime(range.to)}. Refresh reads up to now`,
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
                      <tr key={i} className="table__detail">
                        <td colSpan={9}>
                          <Skeleton />
                        </td>
                      </tr>
                    ))
                  : rows.map((row) => {
                      const id = identityOf(row);
                      return (
                        <Fragment key={id}>
                          <LogRow row={row} rows={rows} isNew={fresh.has(id)} now={now} open={open.has(id)} onToggle={() => toggle(id)} />
                          {open.has(id) && (
                            <tr className="routing-log__panel table__detail">
                              <td colSpan={9}>{row.type === 'decision' ? <DecisionCard now={now} onWiden={widen} row={row} /> : <EventDetail now={now} row={row} rows={rows} week={reading} />}</td>
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
            <PagerNav atFirst={stack.length === 0} atLast={nextCursor === null} onFirst={resetPaging} onNext={goNext} onPrevious={goPrevious} />
          </footer>
        </Card>
      </div>
    </PopoverHost.Provider>
  );
}

/** The account a row is about: a move draws where it left and where it landed, a pool or key event its subject. */
function AccountCell({ row }: { row: RoutingLogRow }) {
  const from = row.type === 'decision' ? row.fromAccountId : movedFrom(row);
  const accountId = row.accountId ?? registryTarget(row, 'account');
  const keyId = row.type === 'event' ? (row.keyId ?? registryTarget(row, 'key')) : null;
  if (accountId !== null) {
    return (
      <span className="routing-log__cut ellipsis">
        {from !== null && (
          <>
            <EntityLink kind="account" id={from} /> <span className="muted">to</span>{' '}
          </>
        )}
        <EntityLink kind="account" id={accountId} />
      </span>
    );
  }
  if (row.type === 'event' && row.poolId !== null) {
    return (
      <span className="routing-log__cut ellipsis">
        <span className="muted">pool</span> <EntityLink kind="pool" id={row.poolId} />
      </span>
    );
  }
  if (keyId !== null) {
    return (
      <span className="routing-log__cut ellipsis">
        <span className="muted">key</span> <EntityLink kind="key" id={keyId} />
      </span>
    );
  }
  return <Dash />;
}

/** A catalog change that names one model draws it as the row's model. */
const modelOf = (row: RoutingLogRow) => {
  // The router records an empty model for a request it refused before it resolved one, which names no model to draw.
  const model = row.model === '' ? null : row.model;
  return model ?? catalogModel(row);
};

function LogRow({ isNew, now, onToggle, open, row, rows }: { isNew: boolean; now: number; onToggle: () => void; open: boolean; row: RoutingLogRow; rows: RoutingLogRow[] }) {
  const by = byOf(row, rows);
  const describedBy = useHostedDescription(by.title);
  const facts = factsOf(row, now, rows);
  const model = modelOf(row);
  const className = ['routing-log__row', isNew && 'is-new', open && 'is-open'].filter(Boolean).join(' ');
  return (
    <tr className={className} data-row={identityOf(row)} tabIndex={0} aria-expanded={open} {...toggleRow(onToggle)}>
      <td className="routing-log__chevron" aria-hidden>
        <IconChevronRight size={14} />
      </td>
      <td className="num routing-log__time" title={fmtAbsolute(row.at)}>
        {fmtDatedTime(row.at, now)}
        {row.type === 'decision' && (
          <span className="sub mono routing-log__cut ellipsis">
            <EntityLink kind="request" id={row.requestId} />
          </span>
        )}
      </td>
      <td className="routing-log__kind">
        {row.type === 'decision' ? <Badge tone={DECISION_KIND_TONE[row.kind]}>{DECISION_KIND_LABEL[row.kind]}</Badge> : <span className={`badge badge--${eventTone(row.kind)} routing-log__outline`}>{EVENT_KIND_LABEL[row.kind] ?? words(row.kind)}</span>}
        {/* An exhaustion after a failed attempt counts one past the request's attempts, so it draws no marker. */}
        {row.type === 'decision' && row.attempt > 1 && (row.attemptCount === null || row.attempt <= row.attemptCount) && <span className="sub">{row.attemptCount === null ? `attempt ${row.attempt}` : `attempt ${row.attempt}/${row.attemptCount}`}</span>}
      </td>
      <td className="routing-log__session">
        {row.sessionId === null ? (
          <Dash />
        ) : (
          <span className="routing-log__cut ellipsis">
            <EntityLink kind="session" id={row.sessionId} mono>
              {row.sessionId.slice(0, 8)}
            </EntityLink>
          </span>
        )}
        {row.agentId !== null && (
          <span className="sub mono routing-log__cut ellipsis" title={row.agentId}>
            {row.agentId}
          </span>
        )}
      </td>
      <td className="routing-log__account">
        <AccountCell row={row} />
        {model !== null && (
          <span className="sub mono routing-log__cut ellipsis routing-log__model-under">
            <EntityLink kind="model" id={model} />
          </span>
        )}
      </td>
      <td className="routing-log__model">
        {model === null ? (
          <Dash />
        ) : (
          <span className="mono routing-log__cut ellipsis">
            <EntityLink kind="model" id={model} />
          </span>
        )}
      </td>
      <td className="routing-log__by">
        <ChipView chip={{ ...by, title: by.title ?? by.value }} className="routing-log__chip ellipsis" describedBy={describedBy} />
      </td>
      <td className="routing-log__fact">
        {facts.length === 0 ? (
          <Dash />
        ) : (
          <div className="fact">
            {facts.map((p) => (
              <div className="fact__pair" key={p.label}>
                <span className="fact__label">{p.label}</span>
                <span className="fact__value ellipsis mono" title={p.value}>
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
