import { useMemo, useState, type ReactNode } from 'react';
import { Link } from 'react-router-dom';
import { api } from '@/api';
import type { Account, CostTable, RequestLog, Session } from '@/api/types';
import { Donut } from '@/components/charts/Donut';
import { Sparkline } from '@/components/charts/Sparkline';
import { accountColor } from '@/components/charts/theme';
import { useChartInk } from '@/components/charts/useChartInk';
import { Badge, Button, Card, EmptyState, ErrorState, Skeleton } from '@/components/ui/primitives';
import { WindowTriple } from '@/components/ui/QuotaBars';
import { fmtAbsolute, fmtCompact, fmtInt, fmtMs, fmtPct, fmtRelative, fmtTime, fmtUsd, shortId, shortModel, tokensTotal } from '@/lib/format';
import { useAsync, useNow, type Loaded } from '@/lib/hooks';
import { STATUS_LABEL, STATUS_TONE, isFastScope } from '@/lib/quota';
import { fmtWindows, ringFigures, type RingFigures, type RingLeftOut, type RingRow } from '@/lib/rings';

/**
 * A read whose first answer failed holds no data, so its card would wait on a placeholder forever; a refresh that fails
 * with data held keeps the card drawn.
 */
const failedFirst = (read: Loaded<unknown>) => read.data === null && read.error !== null;

function ReadFailed({ title, error, retry }: { error: string | null; retry: () => void; title: string }) {
  return <ErrorState title={title} body={error} action={<Button size="sm" onClick={retry}>Try again</Button>} />;
}

export function DashboardPage() {
  const overview = useAsync(() => api.getOverview(), [], { refreshMs: 30_000 });
  const accounts = useAsync(() => api.listAccounts(), [], { refreshMs: 30_000 });
  const requests = useAsync(() => api.listRequests({ limit: 10 }), [], { refreshMs: 30_000 });
  const sessions = useAsync(() => api.listSessions(), [], { refreshMs: 30_000 });
  const settings = useAsync(() => api.getSettings(), [], { refreshMs: 30_000 });
  const now = useNow();
  const ink = useChartInk();

  const o = overview.data;
  const hourly = o?.hourly ?? [];
  const tiers = settings.data?.planTables.cost.tiers ?? null;
  const ringFailure = failedFirst(accounts) ? (
    <ReadFailed title="Could not load the accounts" error={accounts.error} retry={accounts.reload} />
  ) : failedFirst(settings) ? (
    <ReadFailed title="Could not load the settings" error={settings.error} retry={settings.reload} />
  ) : null;
  const retrySessions = () => {
    if (failedFirst(overview)) overview.reload();
    if (failedFirst(sessions)) sessions.reload();
  };

  return (
    <div className="stack">
      {failedFirst(overview) ? (
        <ReadFailed title="Could not load the 24-hour figures" error={overview.error} retry={overview.reload} />
      ) : (
        <div className="stat-grid">
          <StatTile label="Requests, 24h" value={o ? fmtInt(o.requests24h) : null} note={o ? `${(o.requests24h / 24).toFixed(1)} per hour` : null} spark={hourly.map((h) => h.requests)} color={ink.series[0]} />
          <StatTile label="Tokens, 24h" value={o ? fmtCompact(o.tokens24h) : null} note={o ? `${fmtPct(o.tokens24h ? o.tokensCached24h / o.tokens24h : 0)} served from cache` : null} spark={hourly.map((h) => h.tokens)} color={ink.series[2]} />
          <StatTile label="Estimated cost, 24h" value={o ? fmtUsd(o.cost24h) : null} note={o ? `${fmtUsd(o.cost24h / 24)} per hour` : null} spark={hourly.map((h) => h.cost)} color={ink.series[1]} />
          <StatTile
            label="Error rate, 24h"
            value={o ? fmtPct(o.errorRate24h ?? 0, 1) : null}
            note={o ? (o.errorRate24h === null ? 'No requests' : o.topErrorCode ? `Most common: ${o.topErrorCode}` : 'No errors') : null}
            spark={hourly.map((h) => h.errors)}
            color={ink.series[7]}
          />
        </div>
      )}
      {overview.data !== null && (
        <p className="muted dash__asof" title={fmtAbsolute(overview.fetchedAt)}>
          {overview.error === null ? `As of ${fmtRelative(overview.fetchedAt, now)}` : `Last refresh failed: ${overview.error}; showing data as of ${fmtRelative(overview.fetchedAt, now)}`}
        </p>
      )}

      <div className="grid-2">
        <RemainingCard title="5h remaining" subtitle="Subscription accounts, 5-hour window, weighed by tier" windowId="5h" accounts={accounts.data} tiers={tiers} now={now} failure={ringFailure} />
        <RemainingCard title="7d remaining" subtitle="Subscription accounts, 7-day window, weighed by tier" windowId="7d" accounts={accounts.data} tiers={tiers} now={now} failure={ringFailure} />
      </div>

      <section>
        <h2 className="section-title">Accounts</h2>
        {failedFirst(accounts) ? (
          // One state across the section: inside the auto-fill grid it would take a single column.
          <ReadFailed title="Could not load the accounts" error={accounts.error} retry={accounts.reload} />
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
          <RecentTable rows={requests.data?.requests ?? null} error={requests.error} />
        </Card>
        <Card title="Sessions" subtitle="Sticky pins and cache-warming agents" actions={<Link to="/sessions">Open</Link>}>
          {failedFirst(overview) || failedFirst(sessions) ? (
            <ReadFailed title="Could not load the sessions" error={failedFirst(overview) ? overview.error : sessions.error} retry={retrySessions} />
          ) : (
            <SessionsSummary activePins={o?.activePins ?? null} sessions={sessions.data} />
          )}
        </Card>
      </div>
    </div>
  );
}

function StatTile({ label, value, note, spark, color }: { color: string; label: string; note: string | null; spark: number[]; value: string | null }) {
  return (
    <div className="stat">
      <span className="stat__label">{label}</span>
      <span className="stat__value">{value ?? <Skeleton width={96} height={26} />}</span>
      <span className="stat__note">{note ?? <Skeleton width={120} />}</span>
      <div className="stat__spark">{spark.length ? <Sparkline values={spark} color={color} /> : <Skeleton height={36} />}</div>
    </div>
  );
}

function RemainingCard({ title, subtitle, windowId, accounts, tiers, now, failure }: { accounts: Account[] | null; failure: ReactNode; now: number; subtitle: string; tiers: CostTable['tiers'] | null; title: string; windowId: '5h' | '7d' }) {
  const figures = useMemo(() => (accounts && tiers ? ringFigures(accounts, windowId, tiers, now) : null), [accounts, tiers, windowId, now]);
  return (
    <Card title={title} subtitle={subtitle}>
      {failure ? (
        failure
      ) : figures === null ? (
        <Skeleton height={148} />
      ) : figures.rows.length === 0 && figures.leftOut.length === 0 ? (
        // A registry of Anthropic-key accounts alone has no window to draw, and an empty ring reads as exhausted.
        <EmptyState title="No subscription accounts" body="A subscription account reports the window this card draws." />
      ) : figures.rows.length === 0 ? (
        <>
          <p className="muted remaining__none">No listed account's tier is sized for the {windowId === '5h' ? '5-hour' : '7-day'} window.</p>
          <LeftOut accounts={figures.leftOut} />
        </>
      ) : (
        <WeighedRing figures={figures} />
      )}
    </Card>
  );
}

function WeighedRing({ figures }: { figures: RingFigures }) {
  const ink = useChartInk();
  const [hover, setHover] = useState<string | null>(null);
  const { capacity, left, leftOut, rows } = figures;
  const share = (r: RingRow) => r.weight * r.remaining;
  const titleOf = (r: RingRow) => `${r.label}: ${fmtWindows(share(r))} of ${fmtWindows(capacity)} Max 20x windows, ${fmtPct(share(r) / capacity)} of the pooled capacity`;
  return (
    <div className="remaining">
      <Donut
        slices={rows.map((r) => ({ id: r.id, label: r.label, value: share(r), color: accountColor(ink, r.id), title: titleOf(r) }))}
        total={capacity}
        centerLabel="of capacity left"
        centerValue={fmtPct(left / capacity)}
        onHover={setHover}
      />
      <ul className="remaining__list">
        {rows.map((r) => (
          <li key={r.id} className={hover && hover !== r.id ? 'is-dim' : ''} title={titleOf(r)}>
            <span className="legend__swatch" style={{ background: accountColor(ink, r.id) }} />
            <Link to={`/accounts/${r.id}`}>{r.label}</Link>
            <span className="num">{fmtPct(r.remaining)}</span>
          </li>
        ))}
      </ul>
      <p className="remaining__line">
        {fmtWindows(left)} of {fmtWindows(capacity)} Max 20x windows left
      </p>
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
  // A fast-lane row holds fast-mode requests alone, so the line reads a row that cools the account.
  const cooldown = account.cooldowns.find((c) => !isFastScope(c.scope));
  return (
    <Link to={`/accounts/${account.id}`} className="card card--padded account-mini">
      <div className="account-mini__head">
        <span className="account-mini__name">{account.label}</span>
        <Badge tone={STATUS_TONE[account.status]} dot>
          {STATUS_LABEL[account.status]}
        </Badge>
      </div>
      {account.email && <div className="account-mini__sub mono muted">{account.email}</div>}
      <WindowTriple account={account} now={now} compact />
      <div className="account-mini__foot">
        {account.extraUsage?.inUse && <Badge tone="serious">Overage active</Badge>}
        {account.status === 'cooldown' && cooldown && (
          <span className="muted" title={fmtAbsolute(cooldown.until)}>
            cooldown ends {fmtRelative(cooldown.until, now)}
          </span>
        )}
        {account.kind === 'api_key' && <span className="muted">Anthropic key</span>}
      </div>
    </Link>
  );
}

/**
 * The Model column's cap in characters of the monospace face: a short id such as `sonnet-5-5` shows whole, and a longer
 * one is cut behind an ellipsis. The cell's title carries the id as sent whenever the cell shows less of it.
 */
const MODEL_CHARS = 12;

function RecentTable({ rows, error }: { error: string | null; rows: RequestLog[] | null }) {
  return (
    <div className="table-wrap">
      <table className="table dash__recent">
        <thead>
          <tr>
            <th>Time</th>
            <th>Key</th>
            <th>Account</th>
            <th>Model</th>
            <th>Status</th>
            <th className="right">TTFB</th>
            <th className="right">Tokens</th>
            <th className="right">Cost</th>
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
                  <tr key={i}>
                    <td colSpan={8}>
                      <Skeleton />
                    </td>
                  </tr>
                ))
            : rows.map((r) => {
                const shown = shortModel(r.model);
                return (
                  <tr key={r.id}>
                    <td className="num" title={fmtAbsolute(r.at)}>
                      <Link to={`/requests/${r.id}`}>{fmtTime(r.at)}</Link>
                      <span className="sub mono">{shortId(r.id, 12)}</span>
                    </td>
                    <td className="mono">{r.keyId}</td>
                    <td>{r.accountId}</td>
                    <td className="mono nowrap" title={shown !== r.model || shown.length > MODEL_CHARS ? r.model : undefined}>
                      <span className="dash__model" style={{ maxWidth: `${MODEL_CHARS}ch` }}>
                        {shown}
                      </span>
                    </td>
                    <td>
                      <StatusBadge status={r.status} attempts={r.attempts.length} />
                    </td>
                    <td className="right num">{fmtMs(r.ttfbMs)}</td>
                    <td className="right num">
                      {fmtCompact(tokensTotal(r.usage))}
                      <span className="sub">{fmtCompact(r.usage.cacheRead)} cached</span>
                    </td>
                    <td className="right num">{fmtUsd(r.costUsd)}</td>
                  </tr>
                );
              })}
        </tbody>
      </table>
    </div>
  );
}

export function StatusBadge({ status, attempts = 1 }: { attempts?: number; status: number | null }) {
  const tone = status === null ? 'neutral' : status === 200 ? 'good' : status === 429 ? 'warning' : status === 529 ? 'serious' : 'critical';
  return (
    <Badge tone={tone} title={attempts > 1 ? `${attempts} attempts` : undefined}>
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
  const byState = { busy: 0, idle: 0, parked: 0 };
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
