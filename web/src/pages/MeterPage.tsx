import { Fragment, useMemo, useState } from 'react';
import { Link } from 'react-router-dom';
import { api } from '@/api';
import { FilterBar } from '@/components/FilterBar';
import { MeterRatio } from '@/components/MeterRatio';
import type { Option } from '@/components/ui/OptionList';
import { IconChevron } from '@/components/ui/icons';
import { PopoverHost, useHoldHost } from '@/components/ui/Popover';
import { Button, Card, ErrorState, SkeletonRows, toggleRow } from '@/components/ui/primitives';
import { accountHref, METER_CODEC } from '@/lib/address';
import { clock } from '@/lib/clock';
import { METER_DIMENSIONS, notLoadedReason } from '@/lib/filterBar';
import { fmtAbsolute, fmtDated, fmtInt, fmtShare, instant, plural } from '@/lib/format';
import { toggled, useAsync, useQueryFilters } from '@/lib/hooks';
import { GROUP_PAGE, GROUP_STEP, METER_HINT, meterGroups, SPARK, sparkOf, sparkY, type MeterGroup } from '@/lib/meter';
import { windowWord } from '@/lib/windows';

const COLUMNS = 8;
const groupKey = (g: Pick<MeterGroup, 'accountId' | 'window'>) => `${g.accountId}:${g.window}`;

/** The group's judged windows over the trailing 30 days to the answer's newest window, the band shaded behind them. */
function Spark({ group, newest }: { group: MeterGroup; newest: number }) {
  const { dots, points, title } = sparkOf(group, newest);
  const [low, high] = SPARK.band;
  const top = sparkY(high);
  const mid = sparkY(1);
  const xy = (p: { x: number; y: number }) => `${p.x.toFixed(1)},${p.y.toFixed(1)}`;
  return (
    <svg className="meter-spark" width={SPARK.width} height={SPARK.height} viewBox={`0 0 ${SPARK.width} ${SPARK.height}`} role="img" aria-label={title}>
      <title>{title}</title>
      <rect className="meter-spark__band" x={0} y={top} width={SPARK.width} height={sparkY(low) - top} />
      {/* Chromium reads a stroked shape's box half a stroke wider, so edge to edge the column needs a pixel more. */}
      <line className="meter-spark__mid" x1={0.5} x2={SPARK.width - 0.5} y1={mid} y2={mid} />
      {points.length > 1 && <polyline className="meter-spark__line" points={points.map(xy).join(' ')} />}
      {points
        .filter((p) => p.off || dots)
        .map((p) => (
          <circle key={xy(p)} className={p.off ? 'meter-spark__dot meter-spark__dot--off' : 'meter-spark__dot'} cx={p.x} cy={p.y} r={p.off ? 2.25 : 1.75} />
        ))}
    </svg>
  );
}

/** An open group's windows, newest first, in the columns the reconciliation table kept, a page at a time. */
function GroupWindows({ group, now, onMore, shown }: { group: MeterGroup; now: number; onMore: () => void; shown: number }) {
  const rows = group.rows.slice(0, shown);
  return (
    <div className="meter-detail">
      <table className="table meter-detail__table">
        <thead>
          <tr>
            <th className="col-fit">Ended</th>
            <th className="right col-fit">Rise</th>
            <th className="right col-fit">Predicted</th>
            <th className="right col-fit">Ratio</th>
            <th className="right col-fit">Requests</th>
            <th className="col-fill" />
          </tr>
        </thead>
        <tbody>
          {rows.map((r) => (
            <tr key={r.windowEnd}>
              <td className="col-fit" title={`${fmtAbsolute(r.windowEnd)}, reconciled ${fmtAbsolute(r.computedAt)}`}>
                <span className="num">{instant(Date.parse(r.windowEnd), now)}</span>
              </td>
              <td className="right num col-fit">{fmtShare(r.observedRise)}</td>
              <td className="right num col-fit">{fmtShare(r.predictedShare)}</td>
              <td className="right num col-fit">
                <MeterRatio row={r} />
              </td>
              <td className="right num col-fit">{fmtInt(r.requests)}</td>
              <td className="col-fill" />
            </tr>
          ))}
        </tbody>
      </table>
      {group.rows.length > shown && (
        <div className="meter-detail__more">
          <span>{`${fmtInt(shown)} of ${fmtInt(group.rows.length)} windows, newest first`}</span>
          <button type="button" className="link-btn" onClick={onMore}>
            {`Show ${fmtInt(GROUP_STEP)} more`}
          </button>
        </div>
      )}
    </div>
  );
}

function GroupLine({ group, newest, onToggle, open }: { group: MeterGroup; newest: number; onToggle: () => void; open: boolean }) {
  const { latest } = group;
  // A click on the account link follows it, and anywhere else on the line opens the group.
  return (
    <tr className={`srow meter-row${open ? ' is-open' : ''}`} aria-expanded={open} tabIndex={0} {...toggleRow(onToggle)}>
      <td className="meter-table__gutter">
        <span className="srow__chevron" aria-hidden>
          <IconChevron size={14} />
        </span>
      </td>
      <td className="mono col-fill" title={group.accountId}>
        <Link to={accountHref(group.accountId)}>{group.accountId}</Link>
      </td>
      <td className="col-fit">{windowWord(group.window)}</td>
      <td className="right num col-fit">
        <MeterRatio row={latest} />
      </td>
      <td className="right num col-fit">{group.off > 0 ? fmtInt(group.off) : <span className="muted">none</span>}</td>
      <td className="col-fit meter-row__spark">
        <Spark group={group} newest={newest} />
      </td>
      <td className="col-fit" title={fmtAbsolute(latest.windowEnd)}>
        <span className="num">{fmtDated(Date.parse(latest.windowEnd))}</span>
      </td>
      <td className="right num col-fit">{fmtInt(group.rows.length)}</td>
    </tr>
  );
}

interface GroupsCardProps {
  accounts: string[];
  error: string | null;
  first: number;
  groups: MeterGroup[] | null;
  newest: number;
  now: number;
  onExpand: (keys: string[]) => void;
  onMore: (key: string) => void;
  onRetry: () => void;
  onToggle: (key: string) => void;
  open: ReadonlySet<string>;
  shown: ReadonlyMap<string, number>;
  title: string;
  variant: 'five-hour' | 'weekly';
}

function GroupsCard({ accounts, error, first, groups, newest, now, onExpand, onMore, onRetry, onToggle, open, shown, title, variant }: GroupsCardProps) {
  return (
    <Card
      padded={false}
      className="card--table"
      title={title}
      actions={
        <Button size="sm" disabled={!groups || groups.length === 0} onClick={() => onExpand((groups ?? []).map(groupKey))}>
          Expand all
        </Button>
      }
    >
      <div className="table-wrap">
        <table className={`table meter-table meter-table--${variant}`}>
          <thead>
            <tr>
              {/* The gutter keeps its own width and takes neither column class, whose 1% would outrank it. */}
              <th className="meter-table__gutter" />
              <th className="col-fill">Account</th>
              <th className="col-fit">Window</th>
              <th className="right col-fit">Latest ratio</th>
              <th className="right col-fit">Off</th>
              <th className="col-fit">Ratio, 30 days</th>
              <th className="col-fit">Latest end</th>
              <th className="right col-fit">Windows</th>
            </tr>
          </thead>
          <tbody>
            {groups === null && error === null
              ? <SkeletonRows count={3} colSpan={COLUMNS} />
              : (groups ?? []).map((g) => {
                  const key = groupKey(g);
                  return (
                    <Fragment key={key}>
                      <GroupLine group={g} newest={newest} open={open.has(key)} onToggle={() => onToggle(key)} />
                      {open.has(key) && (
                        <tr className="srow__detail table__detail">
                          <td colSpan={COLUMNS}>
                            <GroupWindows group={g} now={now} shown={shown.get(key) ?? first} onMore={() => onMore(key)} />
                          </td>
                        </tr>
                      )}
                    </Fragment>
                  );
                })}
            {error !== null ? (
              <tr>
                <td colSpan={COLUMNS}>
                  <ErrorState title="Could not load the meter reconciliation" body={error} retry={onRetry} />
                </td>
              </tr>
            ) : (
              groups?.length === 0 && (
                <tr>
                  <td colSpan={COLUMNS} className="muted">
                    {accounts.length === 0 ? 'No completed window yet' : `No completed window yet for ${accounts.join(', ')}`}
                  </td>
                </tr>
              )
            )}
          </tbody>
        </table>
      </div>
    </Card>
  );
}

export function MeterPage(): JSX.Element {
  const { address, filters, update } = useQueryFilters(METER_CODEC);
  const accounts = useAsync(() => api.listAccounts(), []);
  // Off on this view until the reader picks an interval, and page state, so a link or a reload opens it off.
  const [intervalMs, setIntervalMs] = useState(0);
  const { cause: pauseCause, host } = useHoldHost();
  const read = useAsync(() => api.getAnalyticsReconciliations({ accountIds: filters.accounts }), [filters.accounts], { paused: pauseCause !== null, refreshMs: intervalMs });
  // An answer for the filters before a change draws nothing, so no group stands under a filter it was not read for.
  const rows = read.current?.reconciliations ?? null;
  const groups = useMemo(() => (rows === null ? null : meterGroups(rows)), [rows]);
  // One x axis for every spark: the 30 days to the answer's newest window end.
  const newest = useMemo(() => Math.max(0, ...(rows ?? []).map((r) => Date.parse(r.windowEnd))), [rows]);
  // A group's open state and its shown windows are page state, so a reload or a link opens every group shut.
  const [open, setOpen] = useState<ReadonlySet<string>>(() => new Set());
  const [shown, setShown] = useState<ReadonlyMap<string, number>>(() => new Map());
  const toggle = (key: string) => setOpen((was) => toggled(was, key));
  const expand = (keys: string[]) => setOpen((was) => new Set([...was, ...keys]));
  const more = (first: number) => (key: string) => setShown((was) => new Map(was).set(key, (was.get(key) ?? first) + GROUP_STEP));

  const error = rows === null ? read.error : null;
  const count = rows === null ? '' : `${plural((groups?.weekly.length ?? 0) + (groups?.fiveHour.length ?? 0), 'group')}, ${plural(rows.length, 'window')}`;
  const options: Record<string, Option[]> = { accounts: (accounts.data ?? []).map((a) => ({ label: a.label, value: a.id })) };
  const now = clock.now();
  const card = { accounts: filters.accounts, error, newest, now, onExpand: expand, onRetry: read.reload, onToggle: toggle, open, shown };

  return (
    <PopoverHost.Provider value={host}>
      <div className="meter-view stack">
        <FilterBar
          address={address}
          count={count}
          dimensions={METER_DIMENSIONS}
          disabled={notLoadedReason('accounts', 'account', accounts)}
          filters={filters}
          options={options}
          refresh={{
            intervalMs,
            onIntervalChange: setIntervalMs,
            onRefresh: () => {
              accounts.reload();
              read.reload();
            },
            paused: pauseCause,
            refreshTitle: 'Reads the windows again now',
          }}
          update={update}
        />
        <div className="page-head">
          <div className="page-head__lead">
            <p className="page-head__meta">{METER_HINT}</p>
          </div>
        </div>
        <GroupsCard {...card} first={GROUP_PAGE.weekly} groups={groups?.weekly ?? null} onMore={more(GROUP_PAGE.weekly)} title="Weekly windows" variant="weekly" />
        <GroupsCard {...card} first={GROUP_PAGE.fiveHour} groups={groups?.fiveHour ?? null} onMore={more(GROUP_PAGE.fiveHour)} title="5-hour windows" variant="five-hour" />
      </div>
    </PopoverHost.Provider>
  );
}
