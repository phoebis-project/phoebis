import { useMemo, useState } from 'react';
import { Link, useNavigate } from 'react-router-dom';
import { api } from '@/api';
import type { Account, ApiKey, CompanionState, KeepaliveOverview, KeepaliveRecipe, KeepaliveState, Pool, RecipeState, Session } from '@/api/types';
import { useAppSettings } from '@/app/settings';
import { CostReferenceLink } from '@/components/ui/CostReference';
import { IconChevron } from '@/components/ui/icons';
import { Badge, Button, Card, ConfirmDialog, EmptyState, Menu, Meter, Skeleton, Toast } from '@/components/ui/primitives';
import { fmtAbsolute, fmtDuration, fmtInt, fmtPct, fmtRelative, fmtUsd, shortId } from '@/lib/format';
import { REASON_HINT, REASON_LABEL, WARNING_REASONS } from '@/lib/keepalive';
import { useAsync, useNow } from '@/lib/hooks';
import type { BadgeTone } from '@/lib/quota';

const PIN_TONE: Record<KeepaliveState, BadgeTone> = { busy: 'accent', idle: 'good', parked: 'neutral' };
const PIN_HINT: Record<KeepaliveState, string> = {
  busy: 'The agent is between model turns, running tools or streaming. Its recipe is probed like any other and the idle limits do not apply.',
  idle: "The agent stopped and waits for the user. Probes continue until the pool's max idle time or idle probe budget runs out.",
  parked: 'The agent stopped while background tasks it owns still run; they wake it when they finish, so probes continue and the idle limits do not apply.',
};
const STATE_TONE: Record<RecipeState, BadgeTone> = { armed: 'good', held: 'neutral', probing: 'accent' };
const STATE_HINT: Record<RecipeState, string> = {
  armed: 'The next probe is scheduled, or arms from the first stream event of the request in flight.',
  held: 'No probe is scheduled; the reason chip says why.',
  probing: 'A probe is in flight right now.',
};
const NO_RECIPE_HINT = 'Nothing probes this session, so its prompt cache expires on its own schedule.';
const KIND_LABEL: Record<Account['kind'], string> = { api_key: 'api key', oauth: 'subscription' };
const KIND_HINT: Record<Account['kind'], string> = {
  api_key: 'API key upstream: billed per token, no quota windows',
  oauth: 'Subscription upstream: quota windows apply',
};
const COMPANION_HINT: Record<CompanionState, string> = {
  none: 'No cache-keepalive companion ever registered for this session. The router still pins it and still sees its requests.',
  reporting: 'A cache-keepalive companion is registered for this session and its agents are reporting their lifecycle.',
  silent:
    "The companion stopped reporting at the time shown. The retained prompts stay and are probed under the pool's idle limits, counted from the last real request, until the companion registers again or the limits run out.",
};
const MIGRATED_PROBE_HINT =
  "The probe was served by another account than the recipe's; it paid the write the next request would have paid there, so it counts for neither the cold streak nor the net.";
const NO_TRAFFIC_HINT = 'This session has no ledgered requests, so there is nothing to show in analytics or observe.';

interface Row {
  account: Account | null;
  agents: KeepaliveRecipe[];
  /** Why a session carries no recipe at all; null while it has one. */
  noRecipe: string | null;
  session: Session;
}

/** When the pool stopped probing: the max idle time counted from the session's last activity. */
function idleLimitRanOutAt(session: Session, pool: Pool | null): string | null {
  const seconds = pool?.keepalive.maxIdleSeconds ?? 0;
  return seconds > 0 && session.lastUsedAt !== null ? new Date(Date.parse(session.lastUsedAt) + seconds * 1000).toISOString() : null;
}

function buildRows(sessions: Session[], recipes: KeepaliveRecipe[], accounts: Account[], keys: ApiKey[], pools: Pool[], now: number): Row[] {
  const accountById = new Map(accounts.map((a) => [a.id, a]));
  const keyById = new Map(keys.map((k) => [k.id, k]));
  const poolById = new Map(pools.map((p) => [p.id, p]));
  const bySession = new Map<string, KeepaliveRecipe[]>();
  for (const r of recipes) bySession.set(r.sessionId, [...(bySession.get(r.sessionId) ?? []), r]);
  return sessions.map((session) => {
    const agents = bySession.get(session.sessionId) ?? [];
    const key = keyById.get(session.keyId) ?? null;
    const pool = key ? poolById.get(key.poolId) ?? null : null;
    // Every agent held for the idle limit means the retained prompts are already
    // gone, so the session reads as having no recipe even though the rows remain.
    const idleLimitOnly = agents.length > 0 && agents.every((a) => a.reason === 'idle_limit');
    const causes: string[] = [];
    if (pool && !pool.keepalive.enabled) causes.push(`pool ${pool.name} keeps no caches warm`);
    if (key?.keepaliveOptOut) causes.push(`key ${key.name} opts out`);
    if (idleLimitOnly) {
      const ranOut = idleLimitRanOutAt(session, pool);
      causes.push(ranOut ? `the idle limit ran out ${fmtRelative(ranOut, now)}` : 'the idle limit ran out');
    }
    if (causes.length === 0) causes.push('the last request left no cacheable prefix to replay');
    return { account: accountById.get(session.accountId ?? '') ?? null, agents, noRecipe: agents.length === 0 || idleLimitOnly ? causes.join(' and ') : null, session };
  });
}

/**
 * Reads as "2 armed, 1 held" for the collapsed row. An agent awaiting cache
 * evidence is armed with no time yet, and counts as armed here.
 */
function stateSummary(agents: KeepaliveRecipe[]): string {
  const states: RecipeState[] = ['armed', 'held', 'probing'];
  return states
    .map((state) => ({ count: agents.filter((a) => a.state === state).length, state }))
    .filter((x) => x.count > 0)
    .map((x) => `${x.count} ${x.state}`)
    .join(', ');
}

export function SessionsPage() {
  const accounts = useAsync(() => api.listAccounts(), []);
  const keys = useAsync(() => api.listKeys(), []);
  const overview = useAsync(() => api.getKeepaliveOverview(), []);
  const pools = useAsync(() => api.listPools(), []);
  const recipes = useAsync(() => api.listKeepaliveRecipes(), []);
  const sessions = useAsync(() => api.listSessions(), []);
  const settings = useAsync(() => api.getSettings(), []);
  const now = useNow(5_000);
  const navigate = useNavigate();
  const { canMutate } = useAppSettings();
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [pending, setPending] = useState<Row | null>(null);
  const [toast, setToast] = useState<string | null>(null);

  const sessionData = sessions.data;
  const recipeData = recipes.data;
  const expireAfterSeconds = settings.data?.sessions.expireAfterSeconds ?? null;
  // The router forgets a session once it has been idle for the expiry, so this list has to as well.
  const rows = useMemo(
    () =>
      sessionData && recipeData
        ? buildRows(sessionData, recipeData, accounts.data ?? [], keys.data ?? [], pools.data ?? [], now).filter((r) => {
            const lastUsed = r.session.lastUsedAt ?? r.session.lastRequestAt;
            return expireAfterSeconds === null || lastUsed === null || Date.parse(lastUsed) + expireAfterSeconds * 1000 > now;
          })
        : null,
    [sessionData, recipeData, accounts.data, keys.data, pools.data, expireAfterSeconds, now],
  );
  const o = overview.data;
  const sessionsNote = o ? `${fmtInt(o.sessionsWithCompanion)} with a companion, ${fmtInt(o.sessions)} with a recipe` : null;
  const onConfirmRelease = () => {
    if (!pending) return;
    void api.releaseSession(pending.session.keyId, pending.session.sessionId).then(
      () => {
        setToast('Session released');
        sessions.reload();
        recipes.reload();
      },
      (e: unknown) => setToast(e instanceof Error ? e.message : String(e)),
    );
  };
  const warmingSomewhere = (pools.data ?? []).some((p) => p.keepalive.enabled);
  const allOpen = rows !== null && rows.length > 0 && rows.every((r) => expanded.has(r.session.sessionId));
  const toggle = (id: string) =>
    setExpanded((prev) => {
      const next = new Set(prev);
      if (!next.delete(id)) next.add(id);
      return next;
    });
  const columns = 13;

  return (
    <div className="stack">
      <div className="page-head">
        <p className="page-head__meta">
          Every Claude Code session the router has seen within the session expiry, whether or not a cache-keepalive companion registered for it. A pin binds each model a session uses to one
          upstream, so subagents on another model may sit on another upstream, and with affinity off every request picks again; where cache warming is on, each agent carries a recipe that replays its
          cacheable prefix just before the prompt cache expires, so the next real turn reads the cache instead of rewriting it.
        </p>
        <CostReferenceLink />
      </div>

      <div className="stat-grid stat-grid--6">
        <Tile label="Sessions" value={rows ? fmtInt(rows.length) : null} note={sessionsNote} />
        <Tile label="Cache-warming agents" value={o ? `${o.agents.busy} busy` : null} note={o ? `${o.agents.idle} idle, ${o.agents.parked} parked` : null} />
        <ProbeTile overview={o} />
        {/* The count is router-wide, so a warm tone means at least one pool is paying for probes that did not land. */}
        <Tile
          label="Expired between turns, 24h"
          value={o ? fmtInt(o.expiredBetweenTurns24h) : null}
          note="misses cache warming exists to prevent"
          tone={o && o.expiredBetweenTurns24h > 0 && warmingSomewhere ? 'warning' : undefined}
        />
        <Tile label="Net saved, 7d" value={o ? fmtUsd(o.netSaved7dUsd) : null} note="gross saved minus probe cost" tone={o && o.netSaved7dUsd < 0 ? 'critical' : undefined} />
        <CostTile overview={o} />
      </div>

      <Card
        padded={false}
        className="card--table"
        title="Sessions"
        subtitle="One row per session; open a row for the cache-warming recipe each agent carries"
        actions={
          rows && rows.length > 0 ? (
            <Button size="sm" onClick={() => setExpanded(allOpen ? new Set() : new Set(rows.map((r) => r.session.sessionId)))} aria-pressed={allOpen}>
              {allOpen ? 'Collapse all' : 'Expand all'}
            </Button>
          ) : undefined
        }
      >
        {rows === null ? (
          <div style={{ padding: 20 }}>
            <Skeleton height={14} />
          </div>
        ) : rows.length === 0 ? (
          <div style={{ padding: 20 }}>
            <EmptyState title="No live sessions" body="A session appears here on its first request and drops off once it passes the session expiry." />
          </div>
        ) : (
          <div className="table-wrap">
            <table className="table sessions-table">
              <thead>
                <tr>
                  <th className="sessions-table__gutter" />
                  <th>Key</th>
                  <th>Session</th>
                  <th>Model</th>
                  <th>Upstream</th>
                  <th>Last used</th>
                  <th className="right">Requests</th>
                  <th>Last request</th>
                  <th className="right">Agents</th>
                  <th>Companion</th>
                  <th>Cache warming</th>
                  <th>Next probe</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {rows.map((row) => (
                  <SessionRows
                    key={row.session.sessionId}
                    row={row}
                    open={expanded.has(row.session.sessionId)}
                    onToggle={() => toggle(row.session.sessionId)}
                    now={now}
                    canMutate={canMutate}
                    columns={columns}
                    expireAfterSeconds={expireAfterSeconds}
                    onOpen={(path) => navigate(path)}
                    onRelease={() => setPending(row)}
                  />
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Card>

      <ConfirmDialog
        open={pending !== null}
        onClose={() => setPending(null)}
        title="Release session"
        confirmLabel="Release"
        body={
          pending
            ? `Session ${shortId(pending.session.sessionId, 13)} loses its pin to ${pending.session.accountId ?? 'its upstream'} and every recipe on it is dropped. The next request picks an upstream again and the prompt cache on ${pending.session.accountId ?? 'it'} goes cold.`
            : ''
        }
        onConfirm={onConfirmRelease}
      />
      <Toast message={toast} onDone={() => setToast(null)} />
    </div>
  );
}

function SessionRows({
  row,
  open,
  onToggle,
  now,
  canMutate,
  columns,
  expireAfterSeconds,
  onOpen,
  onRelease,
}: {
  canMutate: boolean;
  columns: number;
  expireAfterSeconds: number | null;
  now: number;
  onOpen: (path: string) => void;
  onRelease: () => void;
  onToggle: () => void;
  open: boolean;
  row: Row;
}) {
  const { account, agents, session } = row;
  const summary = stateSummary(agents);
  const otherModels = agents.filter((a) => a.model !== session.model);
  const otherAccounts = agents.filter((a) => a.accountId !== session.accountId);
  // An armed agent with no time is waiting on its first stream event, which is
  // a different thing from a session nothing probes.
  const pendingProbe = agents.some((a) => a.state === 'armed' && a.nextProbeAt === null);
  // A session known from the ledger or from a registration alone carries no activity of its own, so the last
  // ledgered request stands in for it and the expiry counts from there.
  const lastUsed = session.lastUsedAt ?? session.lastRequestAt;
  const expiresAt = expireAfterSeconds === null || lastUsed === null ? null : new Date(Date.parse(lastUsed) + expireAfterSeconds * 1000).toISOString();
  // Analytics and observe both read the ledger, so a session that never reached it has nothing to open.
  const hasTraffic = session.requests > 0;
  return (
    <>
      <tr
        className={`srow ${open ? 'is-open' : ''}`}
        aria-expanded={open}
        tabIndex={0}
        onClick={onToggle}
        onKeyDown={(e) => {
          if (e.key !== 'Enter' && e.key !== ' ') return;
          e.preventDefault();
          onToggle();
        }}
      >
        <td className="sessions-table__gutter">
          <span className="srow__chevron" aria-hidden>
            <IconChevron size={14} />
          </span>
        </td>
        <td className="mono">{session.keyId}</td>
        <td className="mono" title={session.sessionId}>
          {shortId(session.sessionId, 13)}
          {(session.epoch ?? 1) > 1 && (
            <span className="sub srow__epoch" title="How many times this session's pin was re-created">
              epoch {session.epoch}
            </span>
          )}
        </td>
        <td className="mono">
          <span className="srow__pin">
            {session.model ?? <span className="muted">-</span>}
            <OtherPins agents={otherModels} />
          </span>
        </td>
        <td>
          <span className="srow__account">
            {session.accountId === null ? (
              <span className="muted">-</span>
            ) : (
              <Link to={`/upstreams/${session.accountId}`} onClick={(e) => e.stopPropagation()}>
                {session.accountId}
              </Link>
            )}
            {account && (
              <Badge tone="neutral" title={KIND_HINT[account.kind]}>
                {KIND_LABEL[account.kind]}
              </Badge>
            )}
            <OtherPins agents={otherAccounts} />
          </span>
        </td>
        {/* Expiry rides under the last-used time rather than in a column of its own, which is 90px the 1600px window did not have. */}
        <td title={fmtAbsolute(lastUsed)}>
          {fmtRelative(lastUsed, now)}
          {expiresAt && (
            <span className="sub" title={`Drops off this list at ${fmtAbsolute(expiresAt)}`}>
              expires {fmtRelative(expiresAt, now)}
            </span>
          )}
        </td>
        <td className="right num" title={hasTraffic ? undefined : NO_TRAFFIC_HINT}>
          {hasTraffic ? fmtInt(session.requests) : <span className="muted">0</span>}
        </td>
        <td title={session.lastRequestAt ? fmtAbsolute(session.lastRequestAt) : undefined}>
          {session.lastRequestAt ? <span className="num">{fmtRelative(session.lastRequestAt, now)}</span> : <span className="muted">none</span>}
        </td>
        <td className="right num">{session.agents}</td>
        <td>
          {session.companion === 'reporting' ? (
            <Badge tone="good" dot title={COMPANION_HINT.reporting}>
              reporting
            </Badge>
          ) : session.companion === 'silent' ? (
            <>
              <Badge tone="warning" dot title={COMPANION_HINT.silent}>
                silent
              </Badge>
              <span className="sub">since {fmtRelative(session.companionSince, now)}</span>
            </>
          ) : (
            <span className="muted" title={COMPANION_HINT.none}>
              none
            </span>
          )}
        </td>
        <td>
          {session.keepalive.state && !row.noRecipe ? (
            <>
              <Badge tone={PIN_TONE[session.keepalive.state]} dot title={PIN_HINT[session.keepalive.state]}>
                {session.keepalive.state}
              </Badge>
              {summary && <span className="sub">{summary}</span>}
            </>
          ) : (
            <span className="muted" title={NO_RECIPE_HINT}>
              no recipe
            </span>
          )}
        </td>
        <td title={session.keepalive.nextProbeAt ? fmtAbsolute(session.keepalive.nextProbeAt) : undefined}>
          {session.keepalive.nextProbeAt ? (
            <span className="num">{fmtRelative(session.keepalive.nextProbeAt, now)}</span>
          ) : (
            <span className="muted">{pendingProbe ? 'pending' : 'not scheduled'}</span>
          )}
        </td>
        {/* Spelled-out actions cost 300px of row width, which is what pushed the table past the window. */}
        <td className="right" onClick={(e) => e.stopPropagation()} onKeyDown={(e) => e.stopPropagation()}>
          <Menu
            label={`Actions for session ${shortId(session.sessionId, 13)}`}
            items={[
              { label: 'View in analytics', disabled: !hasTraffic, title: hasTraffic ? undefined : NO_TRAFFIC_HINT, onClick: () => onOpen(`/analytics?session=${encodeURIComponent(session.sessionId)}`) },
              { label: 'View in observe', disabled: !hasTraffic, title: hasTraffic ? undefined : NO_TRAFFIC_HINT, onClick: () => onOpen(`/observe?session=${encodeURIComponent(session.sessionId)}`) },
              ...(canMutate ? [{ label: 'Release', onClick: onRelease }] : []),
            ]}
          />
        </td>
      </tr>
      {open && (
        <tr className="srow__detail">
          <td colSpan={columns}>
            <p className="agents__created">
              {session.createdAt === null ? 'Created: unknown' : `Created ${fmtAbsolute(session.createdAt)}, ${fmtRelative(session.createdAt, now)}`}
            </p>
            {row.noRecipe && (
              <p className="agents__none">
                <Badge tone="neutral" title={NO_RECIPE_HINT}>
                  no recipe
                </Badge>
                <span>Nothing is scheduled because {row.noRecipe}.</span>
              </p>
            )}
            {agents.length > 0 && (
              <div className="agents">
                <div className="agents__row agents__row--head">
                  <span>Agent</span>
                  <span>Model</span>
                  <span>Upstream</span>
                  <span>TTL</span>
                  <span>Next probe</span>
                  <span>Last request</span>
                  <span>State</span>
                </div>
                {agents.map((a) => (
                  <div className="agents__row" key={a.agentId}>
                    <span className="mono">{a.agentId}</span>
                    <span className="mono">{a.model}</span>
                    <span className="mono">{a.accountId}</span>
                    <span className="num">{fmtDuration(a.ttlSeconds * 1000)}</span>
                    <span title={a.nextProbeAt ? fmtAbsolute(a.nextProbeAt) : undefined}>
                      {a.nextProbeAt ? <span className="num">{fmtRelative(a.nextProbeAt, now)}</span> : <span className="muted">{a.state === 'armed' ? 'on first stream event' : 'not scheduled'}</span>}
                    </span>
                    <span title={a.lastRequestAt ? fmtAbsolute(a.lastRequestAt) : undefined}>{a.lastRequestAt ? <span className="num">{fmtRelative(a.lastRequestAt, now)}</span> : <span className="muted">none</span>}</span>
                    <span className="row" style={{ gap: 6 }}>
                      <Badge tone={STATE_TONE[a.state]} dot title={STATE_HINT[a.state]}>
                        {a.state}
                      </Badge>
                      {a.reason && (
                        <Badge tone={WARNING_REASONS.includes(a.reason) ? 'warning' : 'neutral'} title={REASON_HINT[a.reason]}>
                          {REASON_LABEL[a.reason]}
                        </Badge>
                      )}
                    </span>
                  </div>
                ))}
              </div>
            )}
          </td>
        </tr>
      )}
    </>
  );
}

/** "+2" beside a session's main pin, hovering into one line per agent that sits elsewhere. */
function OtherPins({ agents }: { agents: KeepaliveRecipe[] }) {
  if (agents.length === 0) return null;
  return (
    <Badge tone="neutral" title={`Agents pinned elsewhere:\n${agents.map((a) => `${a.agentId}: ${a.model} on ${a.accountId}`).join('\n')}`}>
      +{agents.length}
    </Badge>
  );
}

function Tile({ label, value, note, tone }: { label: string; note: string | null; tone?: 'critical' | 'warning'; value: string | null }) {
  return (
    <div className="stat stat--compact">
      <span className="stat__label">{label}</span>
      <span className={`stat__value ${tone ? `is-${tone}` : ''}`}>{value ?? <Skeleton width={60} height={22} />}</span>
      <span className="stat__note">{note ?? <Skeleton width={90} />}</span>
    </div>
  );
}

function ProbeTile({ overview: o }: { overview: KeepaliveOverview | null }) {
  const total = o ? o.probes24h.ok + o.probes24h.migrated + o.probes24h.skipped + o.probes24h.failed : 0;
  return (
    <div className="stat stat--compact">
      <span className="stat__label">Cache probes, 24h</span>
      <span className="stat__value">{o ? fmtInt(total) : <Skeleton width={60} height={22} />}</span>
      {o ? (
        <>
          <div className="probe-bar" role="img" aria-label={`Probes in the last 24h: ${o.probes24h.ok} ok, ${o.probes24h.migrated} migrated, ${o.probes24h.skipped} skipped, ${o.probes24h.failed} failed`}>
            <span className="probe-bar__seg probe-bar__seg--ok" style={{ flex: o.probes24h.ok }} />
            <span className="probe-bar__seg probe-bar__seg--migrated" style={{ flex: o.probes24h.migrated }} title={MIGRATED_PROBE_HINT} />
            <span className="probe-bar__seg probe-bar__seg--skipped" style={{ flex: o.probes24h.skipped }} />
            <span className="probe-bar__seg probe-bar__seg--failed" style={{ flex: o.probes24h.failed }} />
          </div>
          <span className="stat__note">
            {fmtInt(o.probes24h.ok)} ok ({fmtPct(total ? o.probes24h.ok / total : 0)}), <span title={MIGRATED_PROBE_HINT}>{fmtInt(o.probes24h.migrated)} migrated</span>, {fmtInt(o.probes24h.skipped)} skipped,{' '}
            {fmtInt(o.probes24h.failed)} failed
          </span>
        </>
      ) : (
        <Skeleton width={120} />
      )}
    </div>
  );
}

function CostTile({ overview: o }: { overview: KeepaliveOverview | null }) {
  return (
    <div className="stat stat--compact">
      <span className="stat__label">Probe cost today</span>
      {o ? (
        <ul className="cost-caps">
          {o.costToday.map((c) => {
            const ratio = c.capUsd ? c.usd / c.capUsd : 0;
            return (
              <li key={c.poolId}>
                <div className="cost-caps__head">
                  <span className="mono">{c.poolId}</span>
                  <span className="num">
                    {fmtUsd(c.usd)}
                    {c.capUsd !== null ? <span className="muted"> of {fmtUsd(c.capUsd)}</span> : <span className="muted"> uncapped</span>}
                  </span>
                </div>
                {c.capUsd !== null && <Meter value={ratio} tone={ratio >= 0.9 ? 'low' : ratio >= 0.6 ? 'warn' : 'accent'} thin label={`Probe cost today for ${c.poolId} against its cap`} />}
              </li>
            );
          })}
        </ul>
      ) : (
        <Skeleton width={120} height={22} />
      )}
    </div>
  );
}
