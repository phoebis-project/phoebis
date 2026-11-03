import { useMemo, useState, type ReactNode } from 'react';
import { Link } from 'react-router-dom';
import { api } from '@/api';
import type { Account, KeepaliveState, Overview, RequestLog, Session, Settings, SubscriptionCosts } from '@/api/types';
import { useAppSettings } from '@/app/settings';
import { Donut } from '@/components/charts/Donut';
import { Sparkline } from '@/components/charts/Sparkline';
import { accountColor, accountSlots } from '@/components/charts/theme';
import { useChartInk } from '@/components/charts/useChartInk';
import { statusTone } from '@/components/ui/AttemptTrail';
import { AttentionLine } from '@/components/ui/AttentionLine';
import { CostReferenceLink } from '@/components/ui/CostReference';
import { CostTd } from '@/components/ui/CostTd';
import { Badge, Card, EmptyState, ErrorState, Skeleton } from '@/components/ui/primitives';
import { WindowTriple } from '@/components/ui/QuotaBars';
import { SpeedChip } from '@/components/ui/SpeedChip';
import { figuresValue } from '@/components/ui/Tile';
import { accountHref, requestHref } from '@/lib/address';
import { dollarsUnder, equivalentOrDash, nativeFiguresOf, nativeSidesOf, shareHint, UNPRICED_TITLE, type FundedSum, type WeekReading } from '@/lib/analytics';
import { weekReadingOf } from '@/lib/costs';
import { fmtAbsolute, fmtCompact, fmtInt, fmtMs, fmtPct, fmtRelative, fmtTime, fmtUsd, shortModel, tokensTotal } from '@/lib/format';
import { failedFirst, refreshAttention, useAsync, useAttention, useNow, type Loaded } from '@/lib/hooks';
import { STATUS_LABEL, STATUS_TONE, accountCooldown, planExtraUsage, slowedOf } from '@/lib/quota';
import { REFUSED_TITLE, costHeaderTitle, costUnitOf, glanceCellOf, requestHintArgs, revisionsTitle } from '@/lib/requests';
import { ringFigures, ringLine, ringRowTitle, type RingFigures, type RingLeftOut, type RingRow, type RingWindow } from '@/lib/rings';
import { speedHover } from '@/lib/selection';
import type { UnitPosition, WeekPosition } from '@/lib/unit';

/**
 * The as-of line over the page's reads, each named as its card's error names it: the figures are as old as the oldest
 * answer any card draws, and a read whose refresh failed after an answer is named with its error. A read that never
 * answered is left to its card, which names the error already. Where both the accounts and the settings reads fail
 * first, the ring names the accounts error, and the settings error shows once the accounts answer.
 */
function asOfLine(reads: [string, Loaded<unknown>][], now: number): { text: string; title: string } | null {
  const answered = reads.flatMap(([, r]) => (r.fetchedAt === null ? [] : [r.fetchedAt]));
  if (answered.length === 0) return null;
  const oldest = answered.reduce((a, b) => (Date.parse(b) < Date.parse(a) ? b : a));
  const failed = reads.find(([, r]) => r.fetchedAt !== null && r.error !== null);
  const asOf = fmtRelative(oldest, now);
  return { text: failed ? `Last refresh of the ${failed[0]} failed: ${failed[1].error}. Showing data as of ${asOf}` : `As of ${asOf}`, title: fmtAbsolute(oldest) };
}

/** The day's cost tile as `StatTile` draws it, a null value and note the skeleton. */
interface CostTile {
  note: string | null;
  spark: (number | null)[];
  title?: string;
  value: ReactNode;
}
/**
 * The day's cost tile under a position. Native stacks the plan rows' figure in the chosen week, the Pools page's
 * reading of a day, over the billed rows' dollars, a side with no rows left out, and an empty day reads its zero in
 * each unit. A day a plan row priced nothing on reads the dash under `UNPRICED_TITLE`, its note a dash too while no
 * request was billed, and its spark leaves an unpriced hour a gap. The figure waits on the scale, so native draws the
 * skeleton while the cost model is out, and the reading's failure as a dash titled with it, noted unpriced where the
 * cost model sizes no chosen week. The dollar positions read their dollars.
 */
function costTileOf(unit: UnitPosition, o: Overview, reading: WeekReading): CostTile {
  const { failure, scale, week } = reading;
  const day: FundedSum = { billedUsd: o.billed24h, cost: o.cost24h, equivalent: o.equivalent24h };
  if (unit !== 'native') {
    const dollars = dollarsUnder(unit, day);
    return {
      note: unit === 'equivalent' ? 'at the API price list, the subscription requests' : `${fmtUsd(dollars / 24)} per hour`,
      spark: o.hourly.map((h) => dollarsUnder(unit, { billedUsd: h.billed, cost: h.cost, equivalent: h.equivalent })),
      value: fmtUsd(dollars),
    };
  }
  const sides = nativeSidesOf(day);
  const plan = sides.equivalent !== undefined;
  const billed = sides.billed !== undefined;
  // In percent: the spark's scale floors at one, a dollar an hour on the dollar positions, and a floor of a whole
  // week an hour would flatten the day. An unpriced hour is no idle one, so it draws as a gap.
  const spark = (factor: number) => (plan || !billed ? o.hourly.map((h) => (h.equivalent === null ? null : h.equivalent * factor * 100)) : o.hourly.map((h) => h.billed));
  if (scale === null) return failure === null ? { note: null, spark: spark(1), value: null } : { note: failure.unpriced ? 'unpriced' : 'could not load', spark: spark(1), title: failure.text, value: '-' };
  const { figures, unpriced } = nativeFiguresOf(sides, week, scale);
  return {
    note: billed || !plan ? `${fmtUsd(o.billed24h / 24)} billed per hour` : `${equivalentOrDash(o.equivalent24h === null ? null : o.equivalent24h / 24, week, scale)} per hour`,
    spark: spark(scale),
    title: unpriced ? UNPRICED_TITLE : undefined,
    value: figuresValue(figures),
  };
}

export function DashboardPage() {
  const { unit, week } = useAppSettings();
  const overview = useAsync(() => api.getOverview(), [], { refreshMs: 30_000 });
  const accounts = useAsync(() => api.listAccounts(), [], { refreshMs: 30_000 });
  const requests = useAsync(() => api.listRequests({ limit: 10 }), [], { refreshMs: 30_000 });
  const sessions = useAsync(() => api.listSessions(), [], { refreshMs: 30_000 });
  const settings = useAsync(() => api.getSettings(), [], { refreshMs: 30_000 });
  const attention = useAttention();
  const now = useNow();
  const ink = useChartInk();

  const o = overview.data;
  const hourly = o?.hourly ?? [];
  const costs = settings.data?.subscriptionCosts ?? null;
  const reading = weekReadingOf(costs, settings.error, week);
  const costTile = o ? costTileOf(unit, o, reading) : null;
  const ringFailure = failedFirst(accounts) ? (
    <ErrorState title="Could not load the accounts" body={accounts.error} retry={accounts.reload} />
  ) : failedFirst(settings) ? (
    <ErrorState title="Could not load the settings" body={settings.error} retry={settings.reload} />
  ) : null;
  const asOf = asOfLine(
    [
      ['24-hour figures', overview],
      ['accounts', accounts],
      ['recent requests', requests],
      ['sessions', sessions],
      ['settings', settings],
    ],
    now,
  );
  const retrySessions = () => {
    if (failedFirst(overview)) overview.reload();
    if (failedFirst(sessions)) sessions.reload();
  };

  return (
    <div className="stack">
      <AttentionLine items={attention.items} error={attention.error} loadedAt={attention.loadedAt} now={now} refresh={refreshAttention} />
      {failedFirst(overview) ? (
        <ErrorState title="Could not load the 24-hour figures" body={overview.error} retry={overview.reload} />
      ) : (
        <div className="stat-grid stat-grid--day">
          <StatTile label="Requests, 24h" value={o ? fmtInt(o.requests24h) : null} note={o ? `${(o.requests24h / 24).toFixed(1)} per hour` : null} spark={hourly.map((h) => h.requests)} title="Requests the router asked an account for. The refused requests are in the Refused tile." color={ink.series[0]} />
          <StatTile label="Tokens, 24h" value={o ? fmtCompact(o.tokens24h) : null} note={o ? `${fmtPct(o.tokens24h ? o.tokensCached24h / o.tokens24h : 0)} served from cache` : null} spark={hourly.map((h) => h.tokens)} color={ink.series[2]} />
          <StatTile label="Estimated cost, 24h" value={costTile?.value ?? null} note={costTile?.note ?? null} spark={costTile?.spark ?? []} title={costTile?.title} color={ink.series[1]} />
          <StatTile
            label="Error rate, 24h"
            value={o ? fmtPct(o.errorRate24h ?? 0, 1) : null}
            note={o ? (o.errorRate24h === null ? 'No requests' : o.topErrorCode ? `Most common: ${o.topErrorCode}` : 'No errors') : null}
            spark={hourly.map((h) => h.errors)}
            title="Errors among the requests the router asked an account for, the refused requests left out"
            color={ink.series[7]}
          />
          <StatTile label="Refused, 24h" value={o ? fmtInt(o.refused24h) : null} note={o ? (o.topRefusalCode ? `Most common: ${o.topRefusalCode}` : 'None') : null} spark={hourly.map((h) => h.refused)} title={REFUSED_TITLE} color={ink.series[3]} />
        </div>
      )}
      {asOf && (
        <p className="muted dash__asof" title={asOf.title}>
          {asOf.text}
        </p>
      )}

      <div className="grid-2">
        <RemainingCard
          title="7d remaining"
          subtitle={
            <>
              Subscription accounts, 7-day window, weighed by tier <CostReferenceLink tab="tiers" />
            </>
          }
          windowId="7d"
          accounts={accounts.data}
          costs={costs}
          now={now}
          week={week}
          failure={ringFailure}
        />
        <RemainingCard title="Fable week remaining" subtitle="Subscription accounts, Fable week, weighed by tier" windowId="7d_fable" accounts={accounts.data} costs={costs} now={now} week={week} failure={ringFailure} />
      </div>

      <section>
        <h2 className="section-title">Accounts</h2>
        {failedFirst(accounts) ? (
          // One state across the section: inside the auto-fill grid it would take a single column.
          <ErrorState title="Could not load the accounts" body={accounts.error} retry={accounts.reload} />
        ) : (
          <div className="account-strip">
            {accounts.data
              ? accounts.data.map((a) => <AccountMini key={a.id} account={a} now={now} />)
              : Array.from({ length: 5 }, (_, i) => (
                  <div key={i} className="card card--padded">
                    <Skeleton width="60%" />
                    <Skeleton height={6} className="mt" />
                    <Skeleton height={6} className="mt" />
                  </div>
                ))}
          </div>
        )}
      </section>

      <div className="dash-bottom">
        <Card title="Recent requests" subtitle="Last 10 across every pool" actions={<Link to="/requests">All requests</Link>} padded={false} className="card--table">
          <RecentTable rows={requests.data?.requests ?? null} error={requests.error} settings={settings.data} settingsError={reading.failure?.text ?? null} accounts={accounts.data} scale={reading.scale} />
        </Card>
        <Card title="Sessions" subtitle="Sticky pins and cache-warming agents" actions={<Link to="/sessions">Open</Link>}>
          {failedFirst(overview) || failedFirst(sessions) ? (
            <ErrorState title="Could not load the sessions" body={failedFirst(overview) ? overview.error : sessions.error} retry={retrySessions} />
          ) : (
            <SessionsSummary activePins={o?.activePins ?? null} sessions={sessions.data} />
          )}
        </Card>
      </div>
    </div>
  );
}

function StatTile({ label, value, note, spark, color, title }: { color: string; label: string; note: string | null; spark: (number | null)[]; title?: string; value: ReactNode }) {
  return (
    <div className="stat" title={title}>
      <span className="stat__label">{label}</span>
      <span className="stat__value">{value ?? <Skeleton width={96} height={26} />}</span>
      <span className="stat__note">{note ?? <Skeleton width={120} />}</span>
      <div className="stat__spark">{spark.length ? <Sparkline values={spark} color={color} /> : <Skeleton height={36} />}</div>
    </div>
  );
}

/** The window's words in a ring's empty sentence. */
const RING_WINDOW: Record<RingWindow, string> = { '7d': '7-day window', '7d_fable': 'Fable week' };

function RemainingCard({ title, subtitle, windowId, accounts, costs, now, week, failure }: { accounts: Account[] | null; costs: SubscriptionCosts | null; failure: ReactNode; now: number; subtitle: ReactNode; title: string; week: WeekPosition; windowId: RingWindow }) {
  const figures = useMemo(() => (accounts && costs ? ringFigures(accounts, windowId, costs, week, now) : null), [accounts, costs, windowId, week, now]);
  const slotOf = useMemo(() => accountSlots((accounts ?? []).map((a) => a.id)), [accounts]);
  return (
    <Card title={title} subtitle={subtitle}>
      {failure ? (
        failure
      ) : figures === null ? (
        <Skeleton height={148} />
      ) : figures.rows.length === 0 && figures.leftOut.length === 0 ? (
        // A registry of API-key accounts alone has no window to draw, and an empty ring reads as exhausted.
        <EmptyState title="No subscription accounts" body="A subscription account reports the window this card draws." />
      ) : figures.rows.length === 0 ? (
        // Where no listed account carries the window, a sentence about unsized tiers would say nothing true.
        <>
          <p className="muted remaining__none">{figures.carried ? `No listed account's tier is sized for the ${RING_WINDOW[windowId]}.` : `No account with a ${RING_WINDOW[windowId]}.`}</p>
          <LeftOut accounts={figures.leftOut} />
        </>
      ) : (
        <WeighedRing figures={figures} slotOf={slotOf} />
      )}
    </Card>
  );
}

function WeighedRing({ figures, slotOf }: { figures: RingFigures; slotOf: (id: string) => number | null }) {
  const ink = useChartInk();
  const [hover, setHover] = useState<string | null>(null);
  const { capacity, left, leftOut, rows } = figures;
  const share = (r: RingRow) => r.weight * r.remaining;
  const titleOf = (r: RingRow) => ringRowTitle(figures, r);
  // The Fable ring draws no line under the Pro week, since Pro has no Fable week, and keeps its ring and rows.
  const line = ringLine(figures);
  return (
    <div className="remaining">
      <Donut
        slices={rows.map((r) => ({ id: r.id, label: r.label, value: share(r), color: accountColor(ink, slotOf(r.id)), title: titleOf(r) }))}
        total={capacity}
        centerLabel="of capacity left"
        centerValue={fmtPct(left / capacity)}
        onHover={setHover}
      />
      <ul className="remaining__list">
        {rows.map((r) => (
          <li key={r.id} className={hover && hover !== r.id ? 'is-dim' : ''} title={titleOf(r)}>
            <span className="legend__swatch" style={{ background: accountColor(ink, slotOf(r.id)) }} />
            <Link to={accountHref(r.id)}>{r.label}</Link>
            <span className="num">{fmtPct(r.remaining)}</span>
          </li>
        ))}
      </ul>
      {line !== null && <p className="remaining__line">{line}</p>}
      {leftOut.length > 0 && <LeftOut accounts={leftOut} />}
    </div>
  );
}

function LeftOut({ accounts }: { accounts: RingLeftOut[] }) {
  return (
    <ul className="remaining__left-out muted">
      {accounts.map((a) => (
        <li key={a.id}>
          {a.label}: {a.reason}
        </li>
      ))}
    </ul>
  );
}

function AccountMini({ account, now }: { account: Account; now: number }) {
  const cooldown = accountCooldown(account.cooldowns);
  const slowed = slowedOf(account, now);
  const status = (
    <Badge tone={STATUS_TONE[account.status]} dot>
      {STATUS_LABEL[account.status]}
    </Badge>
  );
  return (
    <Link to={accountHref(account.id)} className="card card--padded account-mini">
      <div className="account-mini__head">
        <span className="account-mini__name">{account.label}</span>
        {/* The compact 5-hour bar has no room for the speed chip beside its label, so the chip stands by the status, the
            two kept together at the head's far end. */}
        {slowed ? (
          <span className="account-mini__status">
            <SpeedChip factor={slowed.factor} hover={speedHover(slowed, now)} />
            {status}
          </span>
        ) : (
          status
        )}
      </div>
      {account.email && <div className="account-mini__sub mono muted">{account.email}</div>}
      <WindowTriple account={account} now={now} compact />
      <div className="account-mini__foot">
        {planExtraUsage(account)?.inUse && <Badge tone="serious">Overage active</Badge>}
        {account.status === 'cooldown' && cooldown && (
          <span className="muted" title={fmtAbsolute(cooldown.until)}>
            cooldown ends {fmtRelative(cooldown.until, now)}
          </span>
        )}
        {account.kind === 'api_key' && <span className="muted">API key</span>}
      </div>
    </Link>
  );
}

/**
 * The Model column's cap in characters of the monospace face: a short id such as `sonnet-5-5` shows whole, and a longer
 * one is cut behind an ellipsis. The cell's title carries the id as sent whenever the cell shows less of it.
 */
const MODEL_CHARS = 12;

/**
 * A recent request's Cost cell, read under each position as the Requests cell reads it, with the hint composed from the
 * accounts read and the cost model and the revisions in force on a native figure's title. The skeleton stands while
 * the scale is out, and a dash titled with the reading's error where it carries one.
 */
function RecentCost({ accounts, error, request, scale, settings, unit, week }: { accounts: Account[] | null; error: string | null; request: RequestLog; scale: number | null; settings: Settings | null; unit: UnitPosition; week: WeekPosition }) {
  const args = requestHintArgs(request, accounts, settings?.subscriptionCosts ?? null);
  return <CostTd cell={glanceCellOf(costUnitOf(request, unit, args === null ? null : shareHint(...args)), revisionsTitle(settings, request.at), week, scale)} error={error} />;
}

function RecentTable({ accounts, rows, error, scale, settings, settingsError }: { accounts: Account[] | null; error: string | null; rows: RequestLog[] | null; scale: number | null; settings: Settings | null; settingsError: string | null }) {
  const { unit, week } = useAppSettings();
  return (
    <div className="table-wrap">
      <table className="table dash__recent">
        <thead>
          <tr>
            <th className="col-fill">Time</th>
            <th className="col-fit">Key</th>
            <th className="col-fit">Account</th>
            <th className="col-fit">Model</th>
            <th className="col-fit">Status</th>
            <th className="right col-fit">TTFB</th>
            <th className="right col-fit">Tokens</th>
            <th className="right col-fit" title={costHeaderTitle(unit)}>Cost</th>
          </tr>
        </thead>
        <tbody>
          {rows === null
            ? error
              ? (
                  <tr>
                    <td colSpan={8}>
                      <ErrorState title="Could not load the recent requests" body={error} />
                    </td>
                  </tr>
                )
              : Array.from({ length: 6 }, (_, i) => (
                  <tr key={i} className="table__detail">
                    <td colSpan={8}>
                      <Skeleton />
                    </td>
                  </tr>
                ))
            : rows.map((r) => {
                const shown = shortModel(r.model);
                return (
                  <tr key={r.id}>
                    <td className="num col-fill" title={fmtAbsolute(r.at)}>
                      <Link to={requestHref(r.id)}>{fmtTime(r.at)}</Link>
                      <span className="sub mono" title={r.id}>
                        {r.id}
                      </span>
                    </td>
                    <td className="mono col-fit" title={r.keyId ?? undefined}>
                      {r.keyId}
                    </td>
                    <td className="col-fit">{r.accountId}</td>
                    <td className="mono nowrap col-fit" title={shown !== r.model || shown.length > MODEL_CHARS ? r.model : undefined}>
                      <span className="dash__model" style={{ maxWidth: `${MODEL_CHARS}ch` }}>
                        {shown}
                      </span>
                    </td>
                    <td className="col-fit">
                      <StatusBadge status={r.status} attempts={r.attempts.length} />
                    </td>
                    <td className="right num col-fit">{fmtMs(r.ttfbMs)}</td>
                    <td className="right num col-fit">
                      {fmtCompact(tokensTotal(r.usage))}
                      <span className="sub">{fmtCompact(r.usage.cacheRead)} cached</span>
                    </td>
                    <RecentCost accounts={accounts} error={settingsError} request={r} scale={scale} settings={settings} unit={unit} week={week} />
                  </tr>
                );
              })}
        </tbody>
      </table>
    </div>
  );
}

export function StatusBadge({ status, attempts = 1 }: { attempts?: number; status: number | null }) {
  return (
    <Badge tone={statusTone(status)} title={attempts > 1 ? `${attempts} attempts` : undefined}>
      {status ?? '-'}
      {attempts > 1 && <span className="badge__extra">×{attempts}</span>}
    </Badge>
  );
}

/**
 * The hero is the router's own pin count: the session rows also hold sessions the ledger or the keepalive engine
 * alone knows and earlier key holders' ones, so only the rows still pinned name the accounts.
 */
function SessionsSummary({ activePins, sessions }: { activePins: number | null; sessions: Session[] | null }) {
  if (!sessions || activePins === null) return <Skeleton height={80} />;
  const byState: Record<KeepaliveState, number> = { busy: 0, idle: 0, parked: 0, released: 0, unknown: 0 };
  let agents = 0;
  for (const p of sessions) {
    if (p.keepalive.state) byState[p.keepalive.state] += 1;
    agents += p.agents;
  }
  return (
    <div className="sess-summary">
      <div className="sess-summary__hero">
        <span className="stat__value">{activePins}</span>
        <span className="stat__note">active pins across {new Set(sessions.filter((p) => p.pinned).map((p) => p.accountId)).size} accounts</span>
      </div>
      <dl className="sess-summary__grid">
        <div>
          <dt>Agents</dt>
          <dd className="num">{agents}</dd>
        </div>
        <div>
          <dt>Busy</dt>
          <dd className="num">{byState.busy}</dd>
        </div>
        <div>
          <dt>Idle</dt>
          <dd className="num">{byState.idle}</dd>
        </div>
        <div>
          <dt>Parked</dt>
          <dd className="num">{byState.parked}</dd>
        </div>
      </dl>
    </div>
  );
}
