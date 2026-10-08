import { useId, useMemo, useState } from 'react';
import { Link, useNavigate } from 'react-router-dom';
import { api } from '@/api';
import type { Account, ApiKey, CompanionState, KeepaliveOverview, KeepaliveRecipe, KeepaliveState, Pool, RecipeState, Session } from '@/api/types';
import { useSession } from '@/app/session';
import { ColumnsControl } from '@/components/ui/ColumnsControl';
import { CostReferenceLink } from '@/components/ui/CostReference';
import { Tile } from '@/components/ui/Tile';
import { IconChevron } from '@/components/ui/icons';
import { Badge, Button, Card, ConfirmDialog, EmptyState, ErrorState, Menu, Skeleton, useDescription } from '@/components/ui/primitives';
import { fmtAbsolute, fmtDuration, fmtInt, fmtPct, fmtRelative, shortId, shortModel, undatedModel } from '@/lib/format';
import { REASON_HINT, REASON_LABEL, WARNING_REASONS } from '@/lib/keepalive';
import { useAsync, useNow } from '@/lib/hooks';
import type { BadgeTone } from '@/lib/quota';
import { aroundSubject, logHref } from '@/lib/routingLog';

const SESSION_COLUMNS = [
  { id: 'key', label: 'Key' },
  { id: 'session', label: 'Session' },
  { id: 'model', label: 'Model' },
  { id: 'account', label: 'Account' },
  { id: 'lastUsed', label: 'Last used' },
  { id: 'requests', label: 'Requests' },
  { id: 'lastRequest', label: 'Last request' },
  { id: 'agents', label: 'Agents' },
  { id: 'companion', label: 'Companion' },
  { id: 'cacheWarming', label: 'Cache warming' },
  { id: 'nextProbe', label: 'Next probe' },
] as const;
type SessionColumnId = (typeof SESSION_COLUMNS)[number]['id'];
// The default set fits the card at 1280 by 800 without these: Last used carries recency, Cache warming and the expanded
// row carry the agents and their next probes.
const HIDDEN_BY_DEFAULT: readonly SessionColumnId[] = ['agents', 'lastRequest', 'nextProbe'];

/** The id as sent on hover where the cell shows less of it. */
const modelTitle = (model: string, shown: string) => (shown === model ? undefined : model);

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
const HISTORICAL_HINT = 'A holder of this key id that no longer exists; the ledger keeps its sessions until they expire';
const HISTORICAL_NO_RECIPE_HINT = 'Belongs to an earlier holder of this key id; nothing is scheduled for it';
const HISTORICAL_CAUSE = 'this key id was created again and the session belongs to the earlier holder';
const KIND_HINT: Record<Account['kind'], string> = {
  api_key: 'Anthropic key account: billed per token, no quota windows',
  oauth: 'Subscription account: quota windows apply',
};
const COMPANION_HINT: Record<CompanionState, string> = {
  none: 'No cache-keepalive companion ever registered for this session. The router still pins it and still sees its requests.',
  reporting: 'A cache-keepalive companion is registered for this session and its agents are reporting their lifecycle.',
  silent:
    "The companion stopped reporting at the time shown. The retained prompts stay and are probed under the pool's idle limits, counted from the last real request, until the companion registers again or the limits run out.",
};
const MIGRATED_PROBE_HINT =
  "The probe was served by another account than the recipe's; it paid the write the next request would have paid there, so it counts for neither the cold streak nor the wasted count.";
const NO_TRAFFIC_HINT = 'This session has no ledgered requests, so there is nothing to show in analytics or observe.';
interface Row {
  account: Account | null;
  agents: KeepaliveRecipe[];
  /** Why a session carries no recipe at all; null while it has one. */
  noRecipe: string | null;
  session: Session;
}

/** One session id can be held by two generations of a key id, so all three name a row. */
const rowKey = (session: Pick<Session, 'keyGeneration' | 'keyId' | 'sessionId'>) => `${session.keyId}:${session.keyGeneration}:${session.sessionId}`;
/** The row's element id, so a fork's badge can link to its base; a session id may hold characters an id may not. */
const rowDomId = (session: Pick<Session, 'keyGeneration' | 'keyId' | 'sessionId'>) => `session-${encodeURIComponent(rowKey(session))}`;

const sessionLabel = (session: Session) =>
  shortId(session.sessionId, 13);

/** When the pool stopped probing: the max idle time counted from the session's last activity. */
function idleLimitRanOutAt(session: Session, pool: Pool | null): string | null {
  const seconds = pool?.keepalive.maxIdleSeconds ?? 0;
  return seconds > 0 && session.lastUsedAt !== null ? new Date(Date.parse(session.lastUsedAt) + seconds * 1000).toISOString() : null;
}

/** `poolsFailed`: the pool list did not load, so a session whose pool is not found has no cause the rows can name but that. */
function buildRows(sessions: Session[], recipes: KeepaliveRecipe[], accounts: Account[], keys: ApiKey[], pools: Pool[], poolsFailed: boolean, now: number): Row[] {
  const accountById = new Map(accounts.map((a) => [a.id, a]));
  const keyById = new Map(keys.map((k) => [k.id, k]));
  const poolById = new Map(pools.map((p) => [p.id, p]));
  const bySession = new Map<string, KeepaliveRecipe[]>();
  // A recipe names no generation and belongs to the live key alone, so it is filed under that generation and
  // an earlier holder of the same key id reads none of them.
  const liveGeneration = (keyId: string) => {
    const k = keyById.get(keyId);
    return k ? Date.parse(k.createdAt) : null;
  };
  for (const r of recipes) {
    const g = liveGeneration(r.keyId);
    if (g === null) continue;
    const id = `${r.keyId}:${g}:${r.sessionId}`;
    bySession.set(id, [...(bySession.get(id) ?? []), r]);
  }
  return sessions.map((session) => {
    const agents = bySession.get(rowKey(session)) ?? [];
    const key = keyById.get(session.keyId) ?? null;
    const pool = key ? poolById.get(key.poolId) ?? null : null;
    // Every agent held for the idle limit means the retained prompts are already
    // gone, so the session reads as having no recipe even though the rows remain.
    const idleLimitOnly = agents.length > 0 && agents.every((a) => a.reason === 'idle_limit');
    const causes: string[] = [];
    // The live key's pool and opt-out say nothing about a session that no longer belongs to it, so an earlier
    // holder carries that reason alone.
    if (session.historical) causes.push(HISTORICAL_CAUSE);
    else if (poolsFailed && key !== null && pool === null) causes.push('the pool list did not load');
    else {
      if (pool && !pool.keepalive.enabled) causes.push(`pool ${pool.name} keeps no caches warm`);
      if (key?.keepaliveOptOut) causes.push(`key ${key.name} opts out`);
      if (idleLimitOnly) {
        const ranOut = idleLimitRanOutAt(session, pool);
        causes.push(ranOut ? `the idle limit ran out ${fmtRelative(ranOut, now)}` : 'the idle limit ran out');
      }
      if (causes.length === 0) causes.push('the last request left no cacheable prefix to replay');
    }
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
  // The router keeps a session while it is used, so the list is read again rather than aged on this clock alone:
  // a session still sending requests would otherwise drop off once its first-read activity crossed the expiry. The
  // overview reads a week of rollups for tiles that move slowly, so it is read once a minute.
  const tick = useNow(15_000);
  const overviewTick = useNow(60_000);
  const accounts = useAsync(() => api.listAccounts(), []);
  const keys = useAsync(() => api.listKeys(), []);
  const overview = useAsync(() => api.getKeepaliveOverview(), [overviewTick]);
  const pools = useAsync(() => api.listPools(), []);
  const recipes = useAsync(() => api.listKeepaliveRecipes(), [tick]);
  const sessions = useAsync(() => api.listSessions(), [tick]);
  const settings = useAsync(() => api.getSettings(), []);
  const now = useNow(5_000);
  const navigate = useNavigate();
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [visible, setVisible] = useState<Set<SessionColumnId>>(() => new Set(SESSION_COLUMNS.map((c) => c.id).filter((id) => !HIDDEN_BY_DEFAULT.includes(id))));
  const [pending, setPending] = useState<Row | null>(null);
  const { notify } = useSession();
  // The base row the last fork link led to, marked until another fork link is followed.
  const [current, setCurrent] = useState<string | null>(null);
  const [howOpen, setHowOpen] = useState(false);
  const howId = useId();

  const sessionData = sessions.data;
  const recipeData = recipes.data;
  const expireAfterSeconds = settings.data?.sessions.expireAfterSeconds ?? null;
  const poolsFailed = pools.data === null && pools.error !== null;
  // The router forgets a session once it has been idle for the expiry, so this list has to as well.
  const rows = useMemo(
    () =>
      sessionData && recipeData && keys.data
        ? buildRows(sessionData, recipeData, accounts.data ?? [], keys.data, pools.data ?? [], poolsFailed, now).filter((r) => {
            const lastUsed = r.session.lastUsedAt ?? r.session.lastRequestAt;
            return expireAfterSeconds === null || lastUsed === null || Date.parse(lastUsed) + expireAfterSeconds * 1000 > now;
          })
        : null,
    [sessionData, recipeData, accounts.data, keys.data, pools.data, poolsFailed, expireAfterSeconds, now],
  );
  const o = overview.data;
  // A refresh that fails keeps the tiles it drew, so only a read that never answered draws them as failed.
  const overviewError = o === null ? overview.error : null;
  // Counted over the rows shown, so the note agrees with the table under it and the count beside it.
  const withCompanion = rows ? rows.filter((r) => r.session.companion === 'reporting' || r.session.companion === 'silent').length : null;
  const withRecipe = rows ? rows.filter((r) => r.session.keepalive.state !== null).length : null;
  const sessionsNote = withCompanion !== null && withRecipe !== null ? `${fmtInt(withCompanion)} with a companion, ${fmtInt(withRecipe)} with a recipe` : null;
  const onConfirmRelease = () => {
    if (!pending) return;
    void api.releaseSession(pending.session.keyId, pending.session.sessionId).then(
      () => {
        notify('Session released');
        sessions.reload();
        recipes.reload();
      },
      (e: unknown) => notify(e instanceof Error ? e.message : String(e)),
    );
  };
  const listedIds = useMemo(() => new Set((rows ?? []).map((r) => rowDomId(r.session))), [rows]);
  const followFork = (id: string) => {
    // jsdom leaves scrollIntoView undefined.
    document.getElementById(id)?.scrollIntoView?.({ block: 'center' });
    setCurrent(id);
  };
  const warmingSomewhere = (pools.data ?? []).some((p) => p.keepalive.enabled);
  const allOpen = rows !== null && rows.length > 0 && rows.every((r) => expanded.has(rowKey(r.session)));
  const toggle = (id: string) =>
    setExpanded((prev) => {
      const next = new Set(prev);
      if (!next.delete(id)) next.add(id);
      return next;
    });
  // The expand gutter and the menu are drawn whatever the control hides.
  const columns = visible.size + 2;

  return (
    <div className="stack">
      <div className="page-head">
        <div className="page-head__lead">
          <p className="page-head__meta">
            Every session within the session expiry, with the account each model is pinned to and each agent&apos;s cache-warming recipe.{' '}
            <button type="button" className="link-btn" aria-expanded={howOpen} aria-controls={howId} onClick={() => setHowOpen((v) => !v)}>
              How pins and recipes work
            </button>
          </p>
          <p id={howId} className="page-head__meta" hidden={!howOpen}>
            Every Claude Code session the router has seen within the session expiry, and every conversation it identified without a session id, whether or not a cache-keepalive companion registered for it. A pin binds each model a session uses to one
            account, so subagents on another model may sit on another account, and with affinity off every request picks again; where cache warming is on, each agent carries a recipe that replays its
            cacheable prefix just before the prompt cache expires, so the next real turn reads the cache instead of rewriting it.
          </p>
        </div>
        <CostReferenceLink tab="plan-rates" />
      </div>

      <div className="stat-grid stat-grid--fit">
        <Tile label="Sessions" value={rows ? fmtInt(rows.length) : null} note={sessionsNote} />
        <Tile label="Cache-warming agents" value={o ? `${o.agents.busy} busy` : null} note={o ? `${o.agents.idle} idle, ${o.agents.parked} parked` : null} error={overviewError} />
        <ProbeTile overview={o} error={overviewError} />
        {/* The count is router-wide, so a warm tone means at least one pool is paying for probes that did not land. */}
        <Tile
          label="Expired between turns, 24h"
          value={o ? fmtInt(o.expiredBetweenTurns24h) : null}
          note="misses cache warming exists to prevent"
          tone={o && o.expiredBetweenTurns24h > 0 && warmingSomewhere ? 'warning' : undefined}
          error={overviewError}
        />
        <Tile
          label="Bus gaps"
          value={o ? fmtInt(o.busGaps) : null}
          note="times the keepalive engine missed bus events"
          tone={o && o.busGaps > 0 ? 'warning' : undefined}
          error={overviewError}
        />
      </div>
      {overviewError !== null && (
        <p className="sessions__notice" role="status">
          Could not load the tiles: {overviewError}{' '}
          <Button size="sm" onClick={overview.reload}>
            Try again
          </Button>
        </p>
      )}
      {accounts.data === null && accounts.error !== null && (
        <p className="sessions__notice" role="status">
          Could not load the accounts: {accounts.error}{' '}
          <Button size="sm" onClick={accounts.reload}>
            Try again
          </Button>
        </p>
      )}

      <Card
        padded={false}
        className="card--table"
        title="Sessions"
        subtitle="One row per session; open a row for the cache-warming recipe each agent carries"
        actions={
          rows && rows.length > 0 ? (
            <>
              <Button size="sm" onClick={() => setExpanded(allOpen ? new Set() : new Set(rows.map((r) => rowKey(r.session))))} aria-pressed={allOpen}>
                {allOpen ? 'Collapse all' : 'Expand all'}
              </Button>
              <div className="filters__cols">
                <ColumnsControl columns={SESSION_COLUMNS} visible={visible} onChange={setVisible} />
              </div>
            </>
          ) : undefined
        }
      >
        {rows === null && (sessions.error ?? keys.error ?? recipes.error) ? (
          <div style={{ padding: 20 }}>
            <ErrorState
              title="Could not load the sessions"
              body={sessions.error ?? keys.error ?? recipes.error ?? ''}
              action={
                <Button
                  size="sm"
                  onClick={() => {
                    sessions.reload();
                    keys.reload();
                    recipes.reload();
                  }}
                >
                  Try again
                </Button>
              }
            />
          </div>
        ) : rows === null ? (
          <div style={{ padding: 20 }}>
            <Skeleton height={14} />
          </div>
        ) : rows.length === 0 ? (
          <div style={{ padding: 20 }}>
            <EmptyState title="No live sessions" body="A session appears here on its first request, or as soon as a companion registers for it, and drops off once it passes the session expiry." />
          </div>
        ) : (
          <div className="table-wrap">
            <table className="table sessions-table">
              <thead>
                <tr>
                  <th className="sessions-table__gutter" />
                  <th />
                  {visible.has('key') && <th>Key</th>}
                  {visible.has('session') && <th>Session</th>}
                  {visible.has('model') && <th>Model</th>}
                  {visible.has('account') && <th>Account</th>}
                  {visible.has('lastUsed') && <th>Last used</th>}
                  {visible.has('requests') && <th className="right">Requests</th>}
                  {visible.has('lastRequest') && <th>Last request</th>}
                  {visible.has('agents') && <th className="right">Agents</th>}
                  {visible.has('companion') && <th>Companion</th>}
                  {visible.has('cacheWarming') && <th>Cache warming</th>}
                  {visible.has('nextProbe') && <th>Next probe</th>}
                </tr>
              </thead>
              <tbody>
                {rows.map((row) => (
                  <SessionRows
                    key={rowKey(row.session)}
                    row={row}
                    open={expanded.has(rowKey(row.session))}
                    onToggle={() => toggle(rowKey(row.session))}
                    now={now}
                    columns={columns}
                    current={current === rowDomId(row.session)}
                    expireAfterSeconds={expireAfterSeconds}
                    forkBase={row.session.forkedFrom === null ? null : rowDomId({ ...row.session, sessionId: row.session.forkedFrom })}
                    listedIds={listedIds}
                    onFollowFork={followFork}
                    onOpen={(path) => navigate(path)}
                    onRelease={() => setPending(row)}
                    visible={visible}
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
            ? `Session ${sessionLabel(pending.session)} loses its pin to ${pending.session.accountId ?? 'its account'} and every recipe on it is dropped. The next request picks an account again and the prompt cache on ${pending.session.accountId ?? 'it'} goes cold.`
            : ''
        }
        onConfirm={onConfirmRelease}
      />
    </div>
  );
}

function SessionRows({
  row,
  open,
  onToggle,
  now,
  columns,
  current,
  expireAfterSeconds,
  forkBase,
  listedIds,
  onFollowFork,
  onOpen,
  onRelease,
  visible,
}: {
  columns: number;
  /** Whether a fork link last led here. */
  current: boolean;
  expireAfterSeconds: number | null;
  /** The element id the base of a forked session would carry, whether or not it is listed; null off a fork. */
  forkBase: string | null;
  listedIds: Set<string>;
  now: number;
  onFollowFork: (id: string) => void;
  onOpen: (path: string) => void;
  onRelease: () => void;
  onToggle: () => void;
  open: boolean;
  row: Row;
  visible: ReadonlySet<SessionColumnId>;
}) {
  const { account, agents, session } = row;
  // The kind gave its badge's width to the fit; the description keeps it within reach of a reader that never hovers.
  const kind = useDescription(visible.has('account') && account ? KIND_HINT[account.kind] : undefined);
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
  const trafficSession = session.sessionId;
  return (
    <>
      <tr
        id={rowDomId(session)}
        className={`srow ${open ? 'is-open' : ''}`}
        aria-current={current ? 'true' : undefined}
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
        {/* The actions sit in a menu, a fraction of spelled-out buttons' width, and lead the row so they stay reachable at
            1280 px whatever the columns after them need. */}
        <td onClick={(e) => e.stopPropagation()} onKeyDown={(e) => e.stopPropagation()}>
          <Menu
            align="left"
            label={`Actions for session ${shortId(session.sessionId, 13)}${session.historical ? ', deleted holder' : ''}`}
            items={[
              { label: 'View in analytics', disabled: !hasTraffic, title: hasTraffic ? undefined : NO_TRAFFIC_HINT, onClick: () => onOpen(`/analytics?session=${encodeURIComponent(trafficSession)}&key=${encodeURIComponent(`${session.keyId}@${session.keyGeneration}`)}`) },
              { label: 'View in observe', disabled: !hasTraffic, title: hasTraffic ? undefined : NO_TRAFFIC_HINT, onClick: () => onOpen(`/observe?session=${encodeURIComponent(trafficSession)}`) },
              // The routing log reads the decisions, which exist once a pin does, so it needs no ledgered request.
              { label: 'View in routing log', disabled: !session.sessionId, onClick: () => onOpen(logHref({ session: session.sessionId, ...aroundSubject(session.lastRequestAt ? Date.parse(session.lastRequestAt) : null, now) })) },
              ...(!session.historical ? [{ label: 'Release', onClick: onRelease }] : []),
            ]}
          />
        </td>
        {visible.has('key') && (
          <td className="mono">
            {session.keyId}
            {session.historical && (
              <span className="sub" title={HISTORICAL_HINT}>
                deleted holder
              </span>
            )}
          </td>
        )}
        {visible.has('session') && (
          <td className="mono" title={session.sessionId}>
            {sessionLabel(session)}
            {session.forkedFrom !== null && forkBase !== null && <ForkBadge base={listedIds.has(forkBase) ? forkBase : null} depth={session.forkDepth} from={session.forkedFrom} onFollow={onFollowFork} />}
            {(session.epoch ?? 1) > 1 && (
              <span className="sub srow__epoch" title="How many times this session's pin was re-created">
                epoch {session.epoch}
              </span>
            )}
          </td>
        )}
        {visible.has('model') && (
          <td className="mono nowrap" title={session.model === null ? undefined : modelTitle(session.model, shortModel(session.model))}>
            <span className="srow__pin">
              {session.model === null ? <span className="muted">-</span> : shortModel(session.model)}
              <OtherPins agents={otherModels} />
            </span>
          </td>
        )}
        {visible.has('account') && (
          <td title={account ? KIND_HINT[account.kind] : undefined} aria-describedby={kind && !kind.sibling ? kind.id : undefined}>
            <span className="srow__account">
              {session.accountId === null ? (
                <span className="muted">-</span>
              ) : (
                <Link to={`/accounts/${session.accountId}`} onClick={(e) => e.stopPropagation()}>
                  {session.accountId}
                </Link>
              )}
              <OtherPins agents={otherAccounts} />
            </span>
          </td>
        )}
        {/* Expiry rides under the last-used time rather than in a column of its own, which is 90px the 1600px window did not have. */}
        {visible.has('lastUsed') && (
          <td title={fmtAbsolute(lastUsed)}>
            {fmtRelative(lastUsed, now)}
            {expiresAt && (
              <span className="sub" title={`Drops off this list at ${fmtAbsolute(expiresAt)}`}>
                expires {fmtRelative(expiresAt, now)}
              </span>
            )}
          </td>
        )}
        {visible.has('requests') && (
          <td className="right num" title={hasTraffic ? undefined : NO_TRAFFIC_HINT}>
            {hasTraffic ? fmtInt(session.requests) : <span className="muted">0</span>}
          </td>
        )}
        {visible.has('lastRequest') && (
          <td title={session.lastRequestAt ? fmtAbsolute(session.lastRequestAt) : undefined}>
            {session.lastRequestAt ? <span className="num">{fmtRelative(session.lastRequestAt, now)}</span> : <span className="muted">none</span>}
          </td>
        )}
        {visible.has('agents') && <td className="right num">{session.agents}</td>}
        {visible.has('companion') && (
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
        )}
        {visible.has('cacheWarming') && (
          <td>
            {session.keepalive.state && !row.noRecipe ? (
              <>
                <Badge tone={PIN_TONE[session.keepalive.state]} dot title={PIN_HINT[session.keepalive.state]}>
                  {session.keepalive.state}
                </Badge>
                {summary && <span className="sub">{summary}</span>}
              </>
            ) : (
              <span className="muted" title={session.historical ? HISTORICAL_NO_RECIPE_HINT : NO_RECIPE_HINT}>
                no recipe
              </span>
            )}
          </td>
        )}
        {visible.has('nextProbe') && (
          <td title={session.keepalive.nextProbeAt ? fmtAbsolute(session.keepalive.nextProbeAt) : undefined}>
            {session.keepalive.nextProbeAt ? (
              <span className="num">{fmtRelative(session.keepalive.nextProbeAt, now)}</span>
            ) : (
              <span className="muted">{pendingProbe ? 'pending' : 'not scheduled'}</span>
            )}
          </td>
        )}
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
                  <span>Account</span>
                  <span>TTL</span>
                  <span>Next probe</span>
                  <span>Last request</span>
                  <span>State</span>
                </div>
                {agents.map((a) => (
                  <div className="agents__row" key={a.agentId}>
                    <span className="mono">{a.agentId}</span>
                    <span className="mono nowrap" title={modelTitle(a.model, undatedModel(a.model))}>
                      {undatedModel(a.model)}
                    </span>
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

/**
 * A forked session's marker: a link to its base's row where the base is listed, the base's id where it is not. The
 * link stops at the row, whose click and Enter would otherwise open it.
 */
function ForkBadge({ base, depth, from, onFollow }: { base: string | null; depth: number | null; from: string; onFollow: (id: string) => void }) {
  const at = depth === null ? '' : ` at depth ${depth}`;
  if (base === null)
    return (
      <span className="badge badge--accent srow__fork" title={`Branched from session ${from}${at}, which this list does not hold`}>
        fork of {shortId(from, 13)}
      </span>
    );
  return (
    <a
      className="badge badge--accent srow__fork"
      href={`#${base}`}
      title={`Branched from session ${from}${at}; follow to its row`}
      onClick={(e) => {
        e.preventDefault();
        e.stopPropagation();
        onFollow(base);
      }}
      onKeyDown={(e) => e.stopPropagation()}
    >
      {depth === null ? 'fork' : `fork at ${depth}`}
    </a>
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

function ProbeTile({ error, overview: o }: { error: string | null; overview: KeepaliveOverview | null }) {
  const total = o ? o.probes24h.ok + o.probes24h.migrated + o.probes24h.skipped + o.probes24h.failed : 0;
  return (
    <Tile
      label="Cache probes, 24h"
      value={o ? fmtInt(total) : null}
      error={error}
      note={
        o ? (
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
        ) : null
      }
    />
  );
}

