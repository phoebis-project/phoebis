import { useMemo, useState } from 'react';
import { Link } from 'react-router-dom';
import { api } from '@/api';
import type { Account, RequestLog, Session } from '@/api/types';
import { Donut } from '@/components/charts/Donut';
import { Sparkline } from '@/components/charts/Sparkline';
import { accountColor } from '@/components/charts/theme';
import { useChartInk } from '@/components/charts/useChartInk';
import { Badge, Card, Skeleton } from '@/components/ui/primitives';
import { WindowTriple } from '@/components/ui/QuotaBars';
import { fmtAbsolute, fmtCompact, fmtInt, fmtMs, fmtPct, fmtRelative, fmtTime, fmtUsd, shortId, tokensTotal } from '@/lib/format';
import { useAsync, useNow } from '@/lib/hooks';
import { STATUS_LABEL, STATUS_TONE, isSubscription, remainingOf, windowOf } from '@/lib/quota';

export function DashboardPage() {
  const overview = useAsync(() => api.getOverview(), []);
  const accounts = useAsync(() => api.listAccounts(), []);
  const requests = useAsync(() => api.listRequestPage({ limit: 10 }), []);
  const sessions = useAsync(() => api.listSessions(), []);
  const now = useNow();
  const ink = useChartInk();

  const o = overview.data;
  const hourly = o?.hourly ?? [];

  return (
    <div className="stack">
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

      <div className="grid-2">
        <RemainingCard title="5h remaining" subtitle="Subscription upstreams, 5-hour window" windowId="5h" accounts={accounts.data} />
        <RemainingCard title="7d remaining" subtitle="Subscription upstreams, 7-day window" windowId="7d" accounts={accounts.data} />
      </div>

      <section>
        <h2 className="section-title">Upstreams</h2>
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
      </section>

      <div className="dash-bottom">
        <Card title="Recent requests" subtitle="Last 10 across every pool" actions={<Link to="/requests">All requests</Link>} padded={false} className="card--table">
          <RecentTable rows={requests.data?.requests ?? null} />
        </Card>
        <Card title="Sessions" subtitle="Sticky pins and cache-warming agents" actions={<Link to="/sessions">Open</Link>}>
          <SessionsSummary pins={sessions.data} />
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

function RemainingCard({ title, subtitle, windowId, accounts }: { accounts: Account[] | null; subtitle: string; title: string; windowId: '5h' | '7d' }) {
  const ink = useChartInk();
  const [hover, setHover] = useState<string | null>(null);
  const rows = useMemo(() => {
    if (!accounts) return [];
    return accounts
      .filter((a) => isSubscription(a) && a.routingEnabled)
      .map((a) => ({ account: a, window: windowOf(a, windowId) }))
      .filter((r) => r.window !== null)
      .map((r) => ({ id: r.account.id, label: r.account.label, remaining: remainingOf(r.window!), color: accountColor(ink, r.account.id) }));
  }, [accounts, windowId, ink]);
  // Every listed account weighs one unit of capacity, so the mean remaining
  // share is also the ring's colored fraction.
  const totalRemaining = rows.reduce((s, r) => s + r.remaining, 0);
  const mean = rows.length ? totalRemaining / rows.length : 0;
  const shareOfPool = (remaining: number) => (rows.length ? remaining / rows.length : 0);
  return (
    <Card title={title} subtitle={subtitle}>
      {accounts ? (
        <div className="remaining">
          <Donut
            slices={rows.map((r) => ({ id: r.id, label: r.label, value: r.remaining, color: r.color, title: `${r.label}: ${fmtPct(r.remaining)} of its window remaining (${fmtPct(shareOfPool(r.remaining))} of pooled capacity)` }))}
            total={rows.length}
            centerLabel="remaining"
            centerValue={fmtPct(mean)}
            onHover={setHover}
          />
          <ul className="remaining__list">
            {rows.map((r) => (
              <li key={r.id} className={hover && hover !== r.id ? 'is-dim' : ''} title={`${fmtPct(shareOfPool(r.remaining))} of pooled capacity`}>
                <span className="legend__swatch" style={{ background: r.color }} />
                <Link to={`/upstreams/${r.id}`}>{r.label}</Link>
                <span className="num">{fmtPct(r.remaining)}</span>
              </li>
            ))}
          </ul>
        </div>
      ) : (
        <Skeleton height={148} />
      )}
    </Card>
  );
}

function AccountMini({ account, now }: { account: Account; now: number }) {
  return (
    <Link to={`/upstreams/${account.id}`} className="card card--padded account-mini">
      <div className="account-mini__head">
        <span className="account-mini__name">{account.label}</span>
        <Badge tone={STATUS_TONE[account.status]} dot>
          {STATUS_LABEL[account.status]}
        </Badge>
      </div>
      <WindowTriple account={account} now={now} compact />
      <div className="account-mini__foot">
        {account.extraUsage?.inUse && <Badge tone="serious">Overage active</Badge>}
        {account.status === 'cooldown' && account.cooldowns[0] && (
          <span className="muted" title={fmtAbsolute(account.cooldowns[0].until)}>
            cooldown ends {fmtRelative(account.cooldowns[0].until, now)}
          </span>
        )}
        {account.kind === 'api_key' && <span className="muted">API key upstream</span>}
      </div>
    </Link>
  );
}

function RecentTable({ rows }: { rows: RequestLog[] | null }) {
  return (
    <div className="table-wrap">
      <table className="table">
        <thead>
          <tr>
            <th>Time</th>
            <th>Key</th>
            <th>Upstream</th>
            <th>Model</th>
            <th>Status</th>
            <th className="right">TTFB</th>
            <th className="right">Tokens</th>
            <th className="right">Cost</th>
          </tr>
        </thead>
        <tbody>
          {rows === null
            ? Array.from({ length: 6 }, (_, i) => (
                <tr key={i}>
                  <td colSpan={8}>
                    <Skeleton />
                  </td>
                </tr>
              ))
            : rows.map((r) => (
                <tr key={r.id}>
                  <td className="num" title={fmtAbsolute(r.at)}>
                    <Link to={`/requests/${r.id}`}>{fmtTime(r.at)}</Link>
                    <span className="sub mono">{shortId(r.id, 12)}</span>
                  </td>
                  <td className="mono">{r.keyId}</td>
                  <td>{r.accountId}</td>
                  <td className="mono">{r.model}</td>
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
              ))}
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

function SessionsSummary({ pins }: { pins: Session[] | null }) {
  if (!pins) return <Skeleton height={80} />;
  const byState = { busy: 0, idle: 0, parked: 0 };
  let agents = 0;
  for (const p of pins) {
    if (p.keepalive.state) byState[p.keepalive.state] += 1;
    agents += p.agents;
  }
  return (
    <div className="sess-summary">
      <div className="sess-summary__hero">
        <span className="stat__value">{pins.length}</span>
        <span className="stat__note">active pins across {new Set(pins.map((p) => p.accountId)).size} upstreams</span>
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
