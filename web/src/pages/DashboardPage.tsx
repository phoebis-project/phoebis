import { useMemo, useState, type ReactNode } from 'react';
import { Link } from 'react-router-dom';
import { api } from '@/api';
import type { Account, AttentionKind, KeepaliveOverview, KeepaliveRecipe, Overview, RequestLog, Session, Settings, SubscriptionCosts } from '@/api/types';
import { useAppSettings } from '@/app/settings';
import { Donut, DonutLegend } from '@/components/charts/Donut';
import { Sparkline } from '@/components/charts/Sparkline';
import { PROBE_SERIES, accountColor, accountSlots } from '@/components/charts/theme';
import { useChartInk } from '@/components/charts/useChartInk';
import { StatusBadge } from '@/components/ui/AttemptTrail';
import { AttentionLine } from '@/components/ui/AttentionLine';
import { CostReferenceLink } from '@/components/ui/CostReference';
import { CostTd } from '@/components/ui/CostTd';
import { Badge, Card, EmptyState, ErrorRow, ErrorState, ReadNotice, Skeleton, SkeletonRows } from '@/components/ui/primitives';
import { ProbeBar } from '@/components/ui/ProbeBar';
import { WindowTriple } from '@/components/ui/QuotaBars';
import { SpeedChip } from '@/components/ui/SpeedChip';
import { sumTileOf } from '@/components/ui/Tile';
import { accountHref, requestHref } from '@/lib/address';
import { dollarsOf, dollarsUnder, equivalentOrDash, nativeSidesOf, UNIT_TILE_LABEL, type WeekReading } from '@/lib/analytics';
import { fmtAbsolute, fmtCompact, fmtInt, fmtMiB, fmtMs, fmtPct, fmtRelative, fmtTime, fmtUsd, shortModel, tokensTotal } from '@/lib/format';
import { failedFirst, refreshAttention, useAsync, useAttention, useNow, useWeekReading, type Loaded } from '@/lib/hooks';
import { isFinished, lineTotals, recipeFiler, sessionLines } from '@/lib/keepalive';
import { STATUS_LABEL, STATUS_TONE, accountCooldown, planExtraUsage, slowedOf } from '@/lib/quota';
import { REFUSED_TITLE, costHeaderTitle, glanceCellOf, requestCostUnit, revisionsTitle } from '@/lib/requests';
import { ringFigures, ringLine, ringRowTitle, type RingFigures, type RingLeftOut, type RingRow, type RingWindow } from '@/lib/rings';
import { slowDownChip } from '@/lib/selection';
import { HOUR } from '@/lib/time';
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

/**
 * The hours an overview covers, from its first hourly point to the instant the answer landed, null where either is
 * missing. The floor keeps the notes finite where the browser's clock runs far behind the router's.
 */
const hoursCovered = (o: Pick<Overview, 'hourly'>, fetchedAt: string | null): number | null => {
  const first = o.hourly[0]?.at;
  return first === undefined || fetchedAt === null ? null : Math.max(1, (Date.parse(fetchedAt) - Date.parse(first)) / HOUR);
};

/** The day's cost tile as `StatTile` draws it, a null value and note the skeleton. */
interface CostTile {
  failure: WeekReading['failure'];
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
 * cost model sizes no chosen week. The dollar positions read their dollars, each with its rate per hour as its note.
 * Each rate divides by `hours`, the span `hoursCovered` reads, so the note is the skeleton while that is null.
 */
function costTileOf(unit: UnitPosition, o: Overview, reading: WeekReading, hours: number | null): CostTile {
  const day = o.day24h;
  const tile = sumTileOf(unit, day, reading);
  if (unit !== 'native') {
    const dollars = dollarsUnder(unit, day);
    return {
      ...tile,
      note: hours === null ? null : `${fmtUsd(dollars / hours)} per hour`,
      spark: o.hourly.map((h) => dollarsUnder(unit, { billedUsd: h.billed, cost: h.cost, equivalent: h.equivalent })),
    };
  }
  const sides = nativeSidesOf(day);
  const plan = sides.equivalent !== undefined;
  const billed = sides.billed !== undefined;
  // In percent: the spark's scale floors at one, a dollar an hour on the dollar positions, and a floor of a whole
  // week an hour would flatten the day. An unpriced hour is no idle one, so it draws as a gap.
  const spark = (factor: number) => (plan || !billed ? o.hourly.map((h) => (h.equivalent === null ? null : h.equivalent * factor * 100)) : o.hourly.map((h) => h.billed));
  const { scale, week } = reading;
  if (scale === null) return { ...tile, note: null, spark: spark(1) };
  return {
    ...tile,
    note: hours === null ? null : billed || !plan ? `${fmtUsd(day.billedUsd / hours)} billed per hour` : `${equivalentOrDash(day.equivalent === null ? null : day.equivalent / hours, week, scale)} per hour`,
    spark: spark(scale),
  };
}

export function DashboardPage() {
  const { unit, week } = useAppSettings();
  const overview = useAsync(() => api.getOverview(), [], { refreshMs: 30_000 });
  const accounts = useAsync(() => api.listAccounts(), [], { refreshMs: 30_000 });
  const requests = useAsync(() => api.listRequests({ limit: 10 }), [], { refreshMs: 30_000 });
  const sessions = useAsync(() => api.listSessions(), [], { refreshMs: 30_000 });
  const recipes = useAsync(() => api.listKeepaliveRecipes(), [], { refreshMs: 30_000 });
  const { reading, settings } = useWeekReading(week, { refreshMs: 30_000 });
  const keepalive = useAsync(() => api.getKeepaliveOverview(), [], { refreshMs: 30_000 });
  const attention = useAttention();
  const held = (kind: AttentionKind) => (attention.items ?? []).some((item) => item.kind === kind);
  const now = useNow();
  const ink = useChartInk();

  const o = overview.data;
  const costs = settings.data?.subscriptionCosts ?? null;
  // Sparkline memoizes on its values, and the page re-renders on its five reads and the clock, so the sparks and the
  // cost tile follow the overview, the reading and the unit alone.
  const sparks = useMemo(() => {
    const hourly = o?.hourly ?? [];
    return { errors: hourly.map((h) => h.errors), refused: hourly.map((h) => h.refused), requests: hourly.map((h) => h.requests), tokens: hourly.map((h) => h.tokens) };
  }, [o]);
  const hours = o ? hoursCovered(o, overview.fetchedAt) : null;
  const costTile = useMemo(() => (o ? costTileOf(unit, o, reading, hours) : null), [hours, o, reading, unit]);
  const accountsFailure = failedFirst(accounts) ? <ErrorState title="Could not load the accounts" body={accounts.error} retry={accounts.reload} /> : null;
  const ringFailure = accountsFailure ?? (failedFirst(settings) ? <ErrorState title="Could not load the settings" body={settings.error} retry={settings.reload} /> : null);
  const ringUnpriced = reading.failure?.unpriced ? reading.failure.text : null;
  const overviewFailed = failedFirst(overview);
  const asOf = asOfLine(
    [
      ['24-hour figures', overview],
      ['accounts', accounts],
      ['keepalive overview', keepalive],
      ['recent requests', requests],
      ['recipes', recipes],
      ['sessions', sessions],
      ['settings', settings],
    ],
    now,
  );
  const retrySessions = () => {
    if (overviewFailed) overview.reload();
    if (failedFirst(sessions)) sessions.reload();
  };

  return (
    <div className="stack">
      <AttentionLine items={attention.items} error={attention.error} loadedAt={attention.loadedAt} now={now} refresh={refreshAttention} />
      {overviewFailed ? (
        <ErrorState title="Could not load the 24-hour figures" body={overview.error} retry={overview.reload} />
      ) : (
        <div className="stat-grid stat-grid--day">
          <StatTile label="Requests, 24h" value={o ? fmtInt(o.requests24h) : null} note={o && hours !== null ? `${(o.requests24h / hours).toFixed(1)} per hour` : null} spark={sparks.requests} title="Requests the router asked an account for. The refused requests are in the Refused tile." color={ink.series[0]} />
          <StatTile label="Tokens, 24h" value={o ? fmtCompact(o.tokens24h) : null} note={o ? `${fmtPct(o.tokens24h ? o.tokensCached24h / o.tokens24h : 0)} served from cache` : null} spark={sparks.tokens} color={ink.series[2]} />
          <StatTile label={`${UNIT_TILE_LABEL[dollarsOf(unit)]}, 24h`} value={costTile?.value ?? null} note={costTile?.note ?? null} spark={costTile?.spark ?? []} title={costTile?.title} failure={costTile?.failure} color={ink.series[1]} />
          <StatTile
            label="Error rate, 24h"
            value={o ? fmtPct(o.errorRate24h ?? 0, 1) : null}
            note={o ? (o.errorRate24h === null ? 'No requests' : o.topErrorCode ? `Most common: ${o.topErrorCode}` : 'No errors') : null}
            spark={sparks.errors}
            title="Errors among the requests the router asked an account for, the refused requests left out"
            color={ink.series[7]}
          />
          <StatTile label="Refused, 24h" value={o ? fmtInt(o.refused24h) : null} note={o ? (o.topRefusalCode ? `Most common: ${o.topRefusalCode}` : 'None') : null} spark={sparks.refused} title={REFUSED_TITLE} color={ink.series[3]} />
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
          unpriced={ringUnpriced}
        />
        <RemainingCard title="Fable week remaining" subtitle="Subscription accounts, Fable week, weighed by tier" windowId="7d_fable" accounts={accounts.data} costs={costs} now={now} week={week} failure={ringFailure} unpriced={ringUnpriced} />
      </div>

      <section>
        <h2 className="section-title">Accounts</h2>
        {/* One state across the section: inside the auto-fill grid it would take a single column. */}
        {accountsFailure ?? (
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
          <RecentTable rows={requests.data?.requests ?? null} error={requests.error} settings={settings.data} accounts={accounts.data} reading={reading} />
        </Card>
        <Card title="Sessions" subtitle="Sessions, their agents and the engine that keeps their caches warm" actions={<Link to="/sessions">Open</Link>}>
          {overviewFailed || failedFirst(sessions) ? (
            <ErrorState title="Could not load the sessions" body={overviewFailed ? overview.error : sessions.error} retry={retrySessions} />
          ) : (
            <SessionsSummary
              activePins={o?.activePins ?? null}
              busGaps={held('keepalive_bus_gaps')}
              expired={held('expired_between_turns')}
              keepalive={keepalive}
              now={now}
              recipes={recipes}
              sessions={sessions.data}
            />
          )}
        </Card>
      </div>
    </div>
  );
}

/** The route's last hourly point is the hour still running, so a spark's last point can read low beside whole hours. */
const SPARK_LAST_POINT = "The sparkline's last point is the current hour so far.";

/**
 * A failure draws as `Tile` draws one: a dash, its note, and its text alone in the title. Otherwise the title ends with
 * the sentence on the spark's last point, or is that sentence where the tile has none.
 */
function StatTile({ label, value, note, spark, color, failure = null, title }: { color: string; failure?: WeekReading['failure']; label: string; note: string | null; spark: (number | null)[]; title?: string; value: ReactNode }) {
  const hover = title === undefined ? SPARK_LAST_POINT : `${title}${title.endsWith('.') ? ' ' : '. '}${SPARK_LAST_POINT}`;
  return (
    <div className="stat" title={failure?.text ?? hover}>
      <span className="stat__label">{label}</span>
      <span className="stat__value">{failure ? '-' : (value ?? <Skeleton width={96} height={26} />)}</span>
      <span className="stat__note">{failure ? (failure.unpriced ? 'unpriced' : 'could not load') : (note ?? <Skeleton width={120} />)}</span>
      <div className="stat__spark">{spark.length ? <Sparkline values={spark} color={color} /> : <Skeleton height={36} />}</div>
    </div>
  );
}

/** The window's words in a ring's empty sentence. */
const RING_WINDOW: Record<RingWindow, string> = { '7d': '7-day window', '7d_fable': 'Fable week' };

/** `unpriced` is the week reading's unpriced sentence, null where the reading is priced or waits on the cost model. */
function RemainingCard({ title, subtitle, windowId, accounts, costs, now, week, failure, unpriced }: { accounts: Account[] | null; costs: SubscriptionCosts | null; failure: ReactNode; now: number; subtitle: ReactNode; title: string; unpriced: string | null; week: WeekPosition; windowId: RingWindow }) {
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
        // Where no listed account carries the window, a sentence about unsized tiers would say nothing true. Under a
        // week whose tier holds no 7-day capacity the week is what lacks a size, so the card names the week. Where a
        // sized account holds no share, the shares are what the ring lacks.
        <>
          <p className="muted remaining__none">
            {figures.emptyReason === 'no_window' ? `No account with a ${RING_WINDOW[windowId]}.` : unpriced !== null ? `${unpriced}.` : figures.emptyReason === 'unreported' ? `No listed account with a sized tier reports a share of the ${RING_WINDOW[windowId]}.` : `No listed account's tier is sized for the ${RING_WINDOW[windowId]}.`}
          </p>
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
      <DonutLegend hover={hover} items={rows.map((r) => ({ color: accountColor(ink, slotOf(r.id)), id: r.id, label: <Link to={accountHref(r.id)}>{r.label}</Link>, title: titleOf(r), value: fmtPct(r.remaining) }))} />
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
            <SpeedChip {...slowDownChip(slowed, now)} />
            {status}
          </span>
        ) : (
          status
        )}
      </div>
      {account.email && <div className="account-mini__sub ellipsis mono muted">{account.email}</div>}
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
function RecentCost({ accounts, reading, request, settings, unit }: { accounts: Account[] | null; reading: WeekReading; request: RequestLog; settings: Settings | null; unit: UnitPosition }) {
  return <CostTd cell={glanceCellOf(requestCostUnit(request, unit, accounts, settings?.subscriptionCosts ?? null), revisionsTitle(settings, request.at), reading.week, reading.scale)} failure={reading.failure} />;
}

function RecentTable({ accounts, rows, error, reading, settings }: { accounts: Account[] | null; error: string | null; reading: WeekReading; rows: RequestLog[] | null; settings: Settings | null }) {
  const { unit } = useAppSettings();
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
              ? <ErrorRow colSpan={8} error={error} errorTitle="Could not load the recent requests" />
              : <SkeletonRows count={6} colSpan={8} />
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
                      <span className="dash__model ellipsis" style={{ maxWidth: `${MODEL_CHARS}ch` }}>
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
                    <RecentCost accounts={accounts} reading={reading} request={r} settings={settings} unit={unit} />
                  </tr>
                );
              })}
        </tbody>
      </table>
    </div>
  );
}

const SUMMARY_FIGURES = [['Agents', 'agents'], ['Busy', 'busy'], ['Idle', 'idle'], ['Parked', 'parked']] as const;

/**
 * The hero is the router's own pin count: the session rows also hold sessions the ledger or the keepalive engine
 * alone knows and earlier key holders' ones, so only the rows still pinned name the accounts. The grid counts the live
 * sessions' lines as the Sessions view's summary line counts the rows its bare address keeps, the finished sessions
 * left out, so the two agree: a row's lines read its pins and the recipes filed under its key id and session id. A
 * recipe names no key generation and belongs to the live key alone, so an earlier holder's row reads none. A failed
 * first recipes read draws a dash in each figure under a notice naming it, as the engine block draws a failed overview.
 */
function SessionsSummary({ activePins, busGaps, expired, keepalive, now, recipes, sessions }: { activePins: number | null; busGaps: boolean; expired: boolean; keepalive: Loaded<KeepaliveOverview>; now: number; recipes: Loaded<KeepaliveRecipe[]>; sessions: Session[] | null }) {
  const recipeData = recipes.data;
  const totals = useMemo(() => {
    if (sessions === null || recipeData === null) return null;
    const recipesOf = recipeFiler(recipeData);
    const live = sessions.flatMap((session) => {
      const lines = sessionLines(session, recipesOf(session));
      return isFinished(lines) ? [] : [{ lines }];
    });
    return lineTotals(live);
  }, [sessions, recipeData]);
  const recipesFailed = failedFirst(recipes);
  if (!sessions || activePins === null || (totals === null && !recipesFailed)) return <Skeleton height={80} />;
  const figure = (n: number | undefined) => n ?? '-';
  return (
    <div className="sess-summary">
      <div className="sess-summary__hero">
        <span className="stat__value">{activePins}</span>
        <span className="stat__note">active pins across {new Set(sessions.filter((p) => p.pinned).map((p) => p.accountId)).size} accounts</span>
      </div>
      {recipesFailed && <ReadNotice className="sess-summary__notice" error={recipes.error} name="recipes" retry={recipes.reload} />}
      <dl className="sess-summary__grid">
        {SUMMARY_FIGURES.map(([label, key]) => (
          <div key={key}>
            <dt>{label}</dt>
            <dd className="num">{figure(totals?.[key])}</dd>
          </div>
        ))}
      </dl>
      <KeepaliveEngine busGaps={busGaps} expired={expired} now={now} read={keepalive} />
    </div>
  );
}

/**
 * The engine's caps, the day's probes and the misses beside them, each miss in the warning ink while its attention
 * item holds. A read whose first answer failed draws a dash in each figure under a notice naming it, so the hero and
 * the grid above keep the sessions' figures.
 */
function KeepaliveEngine({ busGaps, expired, now, read }: { busGaps: boolean; expired: boolean; now: number; read: Loaded<KeepaliveOverview> }) {
  const k = read.data;
  const failed = failedFirst(read);
  if (k === null && !failed) return <Skeleton height={140} />;
  const cap = (limit: { count: number; max: number }, fmt = fmtInt) => `${fmt(limit.count)} of ${fmt(limit.max)}`;
  const probes = k?.probes24h ?? null;
  const total = probes ? PROBE_SERIES.reduce((n, s) => n + probes[s.key], 0) : 0;
  const gap = k?.lastBusGapAt ?? null;
  return (
    <section className="sess-summary__engine">
      <h3 className="section-title">Engine caps</h3>
      {failed && <ReadNotice className="sess-summary__notice" error={read.error} name="keepalive overview" retry={read.reload} />}
      <dl className="sess-summary__caps">
        <div>
          <dt>Sessions</dt>
          <dd className="num">{k ? cap(k.limits.sessions) : '-'}</dd>
        </div>
        <div title="A released record that held a slot keeps it for one cache TTL after the release">
          <dt>Agent records</dt>
          <dd className="num">{k ? cap(k.limits.agents) : '-'}</dd>
        </div>
        <div>
          <dt>Retained</dt>
          <dd className="num">{k ? cap(k.limits.retainedBytes, fmtMiB) : '-'}</dd>
        </div>
      </dl>
      <h3 className="section-title">Cache probes, 24h</h3>
      <p className="sess-summary__probes num">{probes ? `${fmtInt(probes.ok)} ok (${fmtPct(total ? probes.ok / total : 0)}) of ${fmtInt(total)}` : '-'}</p>
      {probes && <ProbeBar probes={probes} />}
      <p className="sess-summary__line">
        {k ? (
          <>
            {/* The item counts the expiries a probe could have prevented, so the overview's count over every pool can
                stand above zero in the plain ink. */}
            <span className={expired ? 'is-warning' : undefined}>expired between turns {fmtInt(k.expiredBetweenTurns24h)}</span>
            {' · '}
            <span className={busGaps ? 'is-warning' : undefined} title={gap === null ? undefined : `The last gap was ${fmtRelative(gap, now)}, at ${fmtAbsolute(gap)}.`}>
              bus gaps {fmtInt(k.busGaps)}
            </span>
          </>
        ) : (
          '-'
        )}
      </p>
    </section>
  );
}
