import { useId, useMemo, useState } from 'react';
import { Link, useNavigate } from 'react-router-dom';
import { api } from '@/api';
import type { Account, ApiKey, CompanionState, KeepaliveOverview, KeepaliveRecipe, KeepaliveState, Pool, RecipeState, Session } from '@/api/types';
import { useSession } from '@/app/session';
import { HELD_PROBE_HINT, MIGRATED_PROBE_HINT } from '@/components/charts/theme';
import { FilterBar } from '@/components/FilterBar';
import { SortControl } from '@/components/SortControl';
import { ColumnsControl } from '@/components/ui/ColumnsControl';
import { CostReferenceLink } from '@/components/ui/CostReference';
import { SpeedChip } from '@/components/ui/SpeedChip';
import { Tile } from '@/components/ui/Tile';
import { IconChevron } from '@/components/ui/icons';
import { Badge, Button, Card, ConfirmDialog, EmptyState, ErrorState, Menu, Skeleton, useHostedDescription } from '@/components/ui/primitives';
import { ANALYTICS_CODEC, REQUESTS_CODEC, SESSION_SORT_KEYS, SESSIONS_CODEC, type SessionsAddress, sortAddressOf, sortDirOf, type SortDir } from '@/lib/address';
import { clock } from '@/lib/clock';
import { failed } from '@/lib/costs';
import { SESSIONS_DIMENSIONS } from '@/lib/filterBar';
import { SESSION_CHARS, fmtAbsolute, fmtDuration, fmtInt, fmtMiB, fmtPct, fmtRelative, keyGenerationOf, keyLifetime, messageOf, shortId, shortModel } from '@/lib/format';
import { CAUSE_HINT, CAUSE_LABEL, contextCell, firstFailedRead, isFinished, isReleasedLine, orderLines, orderRows, pinTtlMs, releasedCount, requestsRange, sessionLines, type SessionLine, type TileRead, WARNING_CAUSES } from '@/lib/keepalive';
import { useAsync, useNow, useQueryFilters } from '@/lib/hooks';
import { minuteSpan, type BadgeTone } from '@/lib/quota';
import { ageWord, aroundSubject, logHref } from '@/lib/routingLog';
import { memberSpeed } from '@/lib/selection';

// The optional columns: the Session column is the row's identity and the table's fill column, so it is always drawn.
const SESSION_COLUMNS = [
  { id: 'key', label: 'Key' },
  { id: 'model', label: 'Model' },
  { id: 'context', label: 'Context' },
  { id: 'effort', label: 'Effort' },
  { id: 'runway', label: 'Runway' },
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
// row carry the agents and their next probes, and the request log and analytics carry the request count, whose room the
// Context column takes.
const HIDDEN_BY_DEFAULT: readonly SessionColumnId[] = ['agents', 'lastRequest', 'nextProbe', 'requests'];

/**
 * The pin's runway on its account as the route answers it: a span, `none` at zero, `unbounded` where no window binds.
 * Null for a row with no pin or no projection, which the cell draws as a dash.
 */
const runwayWord = (s: Session): string | null => (!s.pinned ? null : s.runwayMs !== null ? (s.runwayMs === 0 ? 'none' : minuteSpan(s.runwayMs)) : s.runwayBounded === false ? 'unbounded' : null);

/** The id as sent on hover where the cell shows less of it. */
const modelTitle = (model: string, shown: string) => (shown === model ? undefined : model);

const PIN_TONE: Record<KeepaliveState, BadgeTone> = { busy: 'accent', idle: 'good', parked: 'neutral', released: 'neutral', unknown: 'neutral' };
const PIN_HINT: Record<KeepaliveState, string> = {
  busy: 'Between model turns, running tools or streaming. Probed like any other agent, and the idle window of the main conversation keeps running.',
  idle: "Stopped and waiting for the user. Probed until the pool's idle window runs out, the break-even where the pool sets none.",
  parked: 'Stopped while a background command, a subagent or a scheduled wakeup is open. Probed under the parked and horizon rules and the idle window.',
  released: 'The mod released this agent, a subagent that finished or the main conversation. Its recipe is gone.',
  unknown: "The reporter could not tell this agent's state.",
};
const STATE_TONE: Record<RecipeState, BadgeTone> = { ended: 'neutral', held: 'neutral', not_retained: 'neutral', paused: 'warning', retained: 'good' };
const STATE_LABEL: Record<RecipeState, string> = { ended: 'ended', held: 'held', not_retained: 'not retained', paused: 'paused', retained: 'retained' };
const STATE_HINT: Record<RecipeState, string> = {
  ended: 'The recipe ended. The next real request starts a new one.',
  held: 'The prompt is kept and no probe is sent while the cause holds, and a cause that schedules the next attempt sends it when the hold ends.',
  not_retained: 'The last request was not kept. The cause says why.',
  paused: 'The engine paused this recipe until the next real request. The cause says why.',
  retained: "The prompt is kept and its probe is scheduled or waiting for the request's first stream event.",
};
const NO_RECIPE_HINT = 'Nothing probes this session, so its prompt cache expires on its own schedule.';
const HISTORICAL_HINT = 'A holder of this key id that no longer exists. The ledger keeps its sessions until they expire';
const HISTORICAL_NO_RECIPE_HINT = 'Belongs to an earlier holder of this key id, so nothing is scheduled for it';
const HISTORICAL_CAUSE = 'this key id was created again and the session belongs to the earlier holder';
const KIND_HINT: Record<Account['kind'], string> = {
  api_key: 'API-key account: billed per token, no quota windows',
  oauth: 'Subscription account: quota windows apply',
};
const COMPANION_HINT: Record<CompanionState, string> = {
  none: 'No keepalive mod has registered this session. The router still pins it and still sees its requests.',
  reporting: "The keepalive mod reports this session's agents, their parks and the person's presence.",
  silent: 'The mod stopped reporting at the time shown. The retained prompts stay and are probed under the idle window from the last real request until it reports again or the window runs out.',
};
const COUNTS_HINT = 'Probes sent and requests they kept warm since midnight UTC';
const NO_TRAFFIC_HINT = 'This session has no ledgered requests, so there is nothing to show in analytics or the request log.';
interface Row {
  account: Account | null;
  /** Every recipe filed under the session, as the route answered them: the no-recipe causes read this list. */
  agents: KeepaliveRecipe[];
  /** The engine is done with the session, which draws every line it holds under the released sentence. */
  finished: boolean;
  /** The lines the row draws in the view's order: the recipes `shown` holds and the pins no recipe stands on. */
  lines: SessionLine[];
  /** Why a session carries no recipe at all; null while it has one. */
  noRecipe: string | null;
  /** The session's lines the view hides, which its Agents column takes off the engine's record count. */
  released: number;
  session: Session;
  /** The recipes the row draws, the release filter applied: the summary, the badges and the probe state read them. */
  shown: KeepaliveRecipe[];
}

/** One session id can be held by two generations of a key id, so all three name a row. */
const rowKey = (session: Pick<Session, 'keyGeneration' | 'keyId' | 'sessionId'>) => `${session.keyId}:${session.keyGeneration}:${session.sessionId}`;
/** The row's element id, so a fork's badge can link to its base; a session id may hold characters an id may not. */
const rowDomId = (session: Pick<Session, 'keyGeneration' | 'keyId' | 'sessionId'>) => `session-${encodeURIComponent(rowKey(session))}`;
/**
 * The request log on the session the client sent under the row's key lifetime, since a key id created again can hold
 * the same session id, inside the narrowest range that covers `lastRequestAt`, the last request of the session or of
 * the line the link is for, read on the clock when the link is made (the menu's click, a line's draw): the page's `now`
 * is a sample up to five seconds old, and the request log measures its window from its own load. The session lands as
 * the log's session chip, which matches the id exactly, and an agent line's link adds its agent's.
 */
const requestsHref = (trafficSession: string, session: Pick<Session, 'keyGeneration' | 'keyId'>, lastRequestAt: string | null, agentId?: string) => {
  const defaults = REQUESTS_CODEC.fromParams(new URLSearchParams());
  const range = requestsRange(lastRequestAt, clock.now());
  return `/requests?${REQUESTS_CODEC.toParams({ ...defaults, agentId: agentId ?? defaults.agentId, keys: [keyLifetime(session.keyId, session.keyGeneration)], range: range === null ? defaults.range : { from: '', preset: range, to: '' }, session: trafficSession })}`;
};
/** The analytics on the session the client sent under the row's key lifetime. */
const analyticsHref = (trafficSession: string, session: Pick<Session, 'keyGeneration' | 'keyId'>) =>
  `/analytics?${ANALYTICS_CODEC.toParams({ ...ANALYTICS_CODEC.fromParams(new URLSearchParams()), key: keyLifetime(session.keyId, session.keyGeneration), session: trafficSession })}`;

/**
 * A session id reads whole, since the cell's column cuts it with an ellipsis, and a sentence passes `chars` to cut it
 * at a label's width.
 */
const sessionLabel = (session: Session, chars?: number) => (chars === undefined ? session.sessionId : shortId(session.sessionId, chars));

/**
 * When the pool stopped probing: the longest idle window the router resolves for the pool's recipes, counted from the
 * session's last activity, so the time never reads earlier than the window could have run.
 */
function idleLimitRanOutAt(session: Session, pool: Pool | null): string | null {
  const seconds = pool?.keepaliveResolved?.idleSeconds.max ?? 0;
  return seconds > 0 && session.lastUsedAt !== null ? new Date(Date.parse(session.lastUsedAt) + seconds * 1000).toISOString() : null;
}

/**
 * `poolsFailed`: the pool list did not load, so a session whose pool is not found has no cause the rows can name but
 * that. `showReleased`: the Released agents filter, which draws a live session's released lines. `sort` and `dir`: the
 * sort control's key and direction, which order the rows and each row's lines.
 */
function buildRows(sessions: Session[], recipes: KeepaliveRecipe[], accounts: Account[], keys: ApiKey[], pools: Pool[], poolsFailed: boolean, showReleased: boolean, sort: string, dir: SortDir, now: number): Row[] {
  const accountById = new Map(accounts.map((a) => [a.id, a]));
  const keyById = new Map(keys.map((k) => [k.id, k]));
  const poolById = new Map(pools.map((p) => [p.id, p]));
  const bySession = new Map<string, KeepaliveRecipe[]>();
  // A recipe names no generation and belongs to the live key alone, so it is filed under that generation and
  // an earlier holder of the same key id reads none of them.
  const liveGeneration = (keyId: string) => {
    const k = keyById.get(keyId);
    return k ? keyGenerationOf(k) : null;
  };
  for (const r of recipes) {
    const g = liveGeneration(r.keyId);
    if (g === null) continue;
    const id = `${r.keyId}:${g}:${r.sessionId}`;
    bySession.set(id, [...(bySession.get(id) ?? []), r]);
  }
  const rows = sessions.map((session) => {
    const agents = bySession.get(rowKey(session)) ?? [];
    const key = keyById.get(session.keyId) ?? null;
    const pool = key ? poolById.get(key.poolId) ?? null : null;
    // Every agent ended at the idle limit means the retained prompts are already
    // gone, so the session reads as having no recipe even though the rows remain.
    const idleLimitOnly = agents.length > 0 && agents.every((a) => a.cause === 'idle_limit');
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
    // The full list stays beside the lines drawn: the causes of a session without a line read every record, and a
    // session whose lines the mod all released still holds records, so it is no session without a recipe.
    const finished = isFinished(session, agents);
    // A finished session keeps every line so its probe facts stay in view, and a live one draws its released lines
    // only while the filter asks for them.
    const drawsReleased = finished || showReleased;
    const shown = drawsReleased ? agents : agents.filter((a) => !isReleasedLine(a));
    return {
      account: accountById.get(session.accountId ?? '') ?? null,
      agents,
      finished,
      lines: orderLines(sessionLines(session, shown), sort, dir),
      noRecipe: agents.length === 0 || idleLimitOnly ? causes.join(' and ') : null,
      released: drawsReleased ? 0 : releasedCount(agents),
      session,
      shown,
    };
  });
  return orderRows(rows, sort, dir);
}

/**
 * Reads as "2 retained, 1 held" for the collapsed row, from the states still holding a prompt to the ones that ended. An
 * agent awaiting cache evidence is retained with no time yet, and counts as retained here.
 */
function stateSummary(agents: KeepaliveRecipe[]): string {
  const states: RecipeState[] = ['retained', 'held', 'paused', 'not_retained', 'ended'];
  return states
    .map((state) => ({ count: agents.filter((a) => a.state === state).length, state }))
    .filter((x) => x.count > 0)
    .map((x) => `${x.count} ${STATE_LABEL[x.state]}`)
    .join(', ');
}

/** The park's kind and the time it returns by, read after the lifecycle word: "background · deadline in 1h 12m". */
function parkDetail(a: KeepaliveRecipe, now: number): string {
  if (a.agentState !== 'parked' || a.parkKind === null) return '';
  const times = [a.deadline && `deadline ${fmtRelative(a.deadline, now)}`, a.wakeAt && `wakes ${fmtRelative(a.wakeAt, now)}`].filter(Boolean);
  return [a.parkKind, ...times].map((part) => ` · ${part}`).join('');
}

export function SessionsPage() {
  const [intervalMs, setIntervalMs] = useState(5_000);
  const [barOpen, setBarOpen] = useState(false);
  const [pending, setPending] = useState<Row | null>(null);
  const [costOpen, setCostOpen] = useState(false);
  // The rows whose menu is open, by row: an answer landing under an open menu could add or drop rows beneath it.
  const [openMenus, setOpenMenus] = useState<ReadonlySet<string>>(() => new Set());
  const pauseCause = pending !== null || costOpen ? 'dialog' : openMenus.size > 0 || barOpen ? 'menu' : null;
  const reread = { paused: pauseCause !== null, refreshMs: intervalMs };
  // The router keeps a session while it is used, so the list is read again rather than aged on this clock alone: a
  // session still sending requests would otherwise drop off once its first-read activity crossed the expiry. The
  // accounts, the overview and the recipes read on the same interval, so the lines' slow-down chips, the tiles and the
  // hidden lines move with the rows.
  const accounts = useAsync(() => api.listAccounts(), [], reread);
  const keys = useAsync(() => api.listKeys(), []);
  const overview = useAsync(() => api.getKeepaliveOverview(), [], reread);
  const pools = useAsync(() => api.listPools(), []);
  const recipes = useAsync(() => api.listKeepaliveRecipes(), [], reread);
  const sessions = useAsync(() => api.listSessions(), [], reread);
  const settings = useAsync(() => api.getSettings(), []);
  const now = useNow(5_000);
  const navigate = useNavigate();
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [visible, setVisible] = useState<Set<SessionColumnId>>(() => new Set(SESSION_COLUMNS.map((c) => c.id).filter((id) => !HIDDEN_BY_DEFAULT.includes(id))));
  const { notify } = useSession();
  // The base row the last fork link led to, marked until another fork link is followed.
  const [current, setCurrent] = useState<string | null>(null);
  const [howOpen, setHowOpen] = useState(false);
  const howId = useId();
  const { address, filters, update } = useQueryFilters(SESSIONS_CODEC);
  const dir = sortDirOf(SESSION_SORT_KEYS, filters);

  const sessionData = sessions.data;
  const recipeData = recipes.data;
  const expireAfterSeconds = settings.data?.sessions.expireAfterSeconds ?? null;
  const poolsFailed = pools.data === null && pools.error !== null;
  // The router forgets a session once it has been idle for the expiry, so this list has to as well.
  const listed = useMemo(
    () =>
      sessionData && recipeData && keys.data
        ? buildRows(sessionData, recipeData, accounts.data ?? [], keys.data, pools.data ?? [], poolsFailed, filters.released, filters.sort, dir, now).filter((r) => {
            const lastUsed = r.session.lastUsedAt ?? r.session.lastRequestAt;
            return expireAfterSeconds === null || lastUsed === null || Date.parse(lastUsed) + expireAfterSeconds * 1000 > now;
          })
        : null,
    [sessionData, recipeData, accounts.data, keys.data, pools.data, poolsFailed, filters.released, filters.sort, dir, expireAfterSeconds, now],
  );
  // The expiry stays the outer bound, and the filters narrow the list inside it.
  const rows = useMemo(
    () => listed?.filter((r) => (!filters.companion || r.session.companion === 'reporting' || r.session.companion === 'silent') && (filters.finished || !r.finished)) ?? null,
    [listed, filters.companion, filters.finished],
  );
  const hiddenRows = listed !== null && rows !== null ? listed.length - rows.length : 0;
  const hiddenTitle = hiddenRows > 0 ? `${fmtInt(hiddenRows)} ${hiddenRows === 1 ? 'session' : 'sessions'} hidden by the filters` : undefined;
  const o = overview.data;
  // A refresh that fails keeps the tiles it drew, so only a read that never answered draws them as failed.
  const overviewError = o === null ? overview.error : null;
  const readOf = (name: string, r: { data: unknown; error: string | null }): TileRead => ({ error: r.error, loaded: r.data !== null, name });
  // The Sessions and Agents figures count the rows, which the sessions, the recipes and the keys build, so each of
  // those reads can end their placeholder with its error. The other tiles read the overview alone.
  const countReads = [readOf('tiles', overview), readOf('sessions', sessions), readOf('recipes', recipes), readOf('keys', keys)];
  const firstFailed = firstFailedRead(countReads);
  // The card's own reads in the notice's order, so with the overview answered and two of them failed, the card and the
  // notice name the same one.
  const cardFailed = firstFailedRead([readOf('sessions', sessions), readOf('recipes', recipes), readOf('keys', keys)]);
  const reloadOf: Record<string, () => void> = { keys: keys.reload, recipes: recipes.reload, sessions: sessions.reload, tiles: overview.reload };
  // Counted over the rows shown, so the note agrees with the table under it and the count beside it.
  const withCompanion = rows ? rows.filter((r) => r.session.companion === 'reporting' || r.session.companion === 'silent').length : null;
  const withRecipe = rows ? rows.filter((r) => r.session.keepalive.state !== null).length : null;
  const sessionsNote = withCompanion !== null && withRecipe !== null ? `${fmtInt(withCompanion)} with the keepalive mod, ${fmtInt(withRecipe)} with a recipe` : null;
  // Counted over the rows the filters keep, so the Agents figure and its note agree with the table under them.
  const agentRecords = rows?.reduce((n, r) => n + r.session.agents, 0) ?? 0;
  const hidden = rows?.reduce((n, r) => n + r.released, 0) ?? 0;
  const lifecycle = rows?.reduce(
    (n, r) => {
      for (const a of r.agents) if (a.agentState === 'busy' || a.agentState === 'idle' || a.agentState === 'parked') n[a.agentState] += 1;
      return n;
    },
    { busy: 0, idle: 0, parked: 0 },
  );
  const onConfirmRelease = () => {
    if (!pending) return;
    void api.releaseSession(pending.session.keyId, pending.session.sessionId).then(
      () => {
        notify('Session released');
        sessions.reload();
        recipes.reload();
      },
      (e: unknown) => notify(messageOf(e)),
    );
  };
  // The listed rows by element id, so a fork's badge finds its base's row and reads the account the base sits on.
  const listedRows = useMemo(() => new Map((rows ?? []).map((r) => [rowDomId(r.session), r])), [rows]);
  // A line's account reads its kind and its slow-down off the accounts list, the row's own account being main's pin's.
  const accountById = useMemo(() => new Map((accounts.data ?? []).map((a) => [a.id, a])), [accounts.data]);
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
  // The expand gutter, the Session column and the menu are drawn whatever the control hides.
  const columns = visible.size + 3;

  const menuChange = (id: string, open: boolean) =>
    setOpenMenus((s) => {
      const next = new Set(s);
      if (open) next.add(id);
      else next.delete(id);
      return next;
    });

  return (
    <div className="stack">
      {/* TypeScript rejects an address that shares no key with the bar's optional range and search, so this one is read
          with an optional search it never sets. */}
      <FilterBar<SessionsAddress & { search?: string }>
        address={address}
        count={rows === null ? '' : `${rows.length} ${rows.length === 1 ? 'session' : 'sessions'}`}
        dimensions={SESSIONS_DIMENSIONS}
        filters={filters}
        onOpenChange={setBarOpen}
        refresh={{
          intervalMs,
          onIntervalChange: setIntervalMs,
          onRefresh: () => {
            accounts.reload();
            overview.reload();
            recipes.reload();
            sessions.reload();
          },
          paused: pauseCause,
          refreshTitle: 'Reads the list again now',
        }}
        update={update}
      >
        <ColumnsControl columns={SESSION_COLUMNS} visible={visible} onChange={setVisible} />
        <SortControl keys={SESSION_SORT_KEYS} onChange={(next) => update(sortAddressOf(SESSION_SORT_KEYS, next))} value={{ dir, sort: filters.sort }} />
      </FilterBar>
      <div className="page-head">
        <div className="page-head__lead">
          <p className="page-head__meta">
            Every session within the session expiry that the filters keep, with the account each model is pinned to and each agent&apos;s cache-warming recipe.{' '}
            <button type="button" className="link-btn" aria-expanded={howOpen} aria-controls={howId} onClick={() => setHowOpen((v) => !v)}>
              How pins and recipes work
            </button>
          </p>
          <p id={howId} className="page-head__meta" hidden={!howOpen}>
            Every Claude Code session the router has seen within the session expiry, and every conversation it identified without a session id, whether or not a keepalive mod registered it. A pin binds each model a session uses to one
            account, so subagents on another model may sit on another account, and with affinity off every request picks again. Where cache warming is on, each agent carries a recipe that replays its
            cacheable prefix just before the prompt cache expires, so the next real turn reads the cache instead of rewriting it.
          </p>
        </div>
        <CostReferenceLink onOpenChange={setCostOpen} tab="plan-rates" />
      </div>

      <div className="stat-grid stat-grid--caps">
        <Tile label="Sessions" value={o && rows ? <span className="nowrap">{`${fmtInt(rows.length)} of ${fmtInt(o.limits.sessions.max)}`}</span> : null} note={sessionsNote} title={hiddenTitle} failure={failed(firstFailed?.error ?? null)} />
        {/* The sessions and the recipes are two reads that land apart, so the figure can trail the hidden lines by a
            read, which is accepted. Only a line the recipes list carries comes off, so a record that never drew a line
            is never subtracted. */}
        <Tile
          label="Agents"
          value={
            o && rows ? (
              <span
                className="nowrap"
                title={`The rows shown hold ${fmtInt(agentRecords)} agent records against the cap.${hidden > 0 ? ` ${fmtInt(hidden)} of them are subagents the mod released while their session lives, which draw no line and keep their slot for one cache TTL after the release, then leave the engine.` : ''}${hiddenTitle === undefined ? '' : ` ${hiddenTitle}.`}`}
              >
                {`${fmtInt(Math.max(0, agentRecords - hidden))} of ${fmtInt(o.limits.agents.max)}`}
              </span>
            ) : null
          }
          note={lifecycle ? `${fmtInt(lifecycle.busy)} busy, ${fmtInt(lifecycle.idle)} idle, ${fmtInt(lifecycle.parked)} parked` : null}
          title={hiddenTitle}
          failure={failed(firstFailed?.error ?? null)}
        />
        <Tile
          label="Retained"
          value={o ? <span className="nowrap">{`${fmtMiB(o.limits.retainedBytes.count)} of ${fmtMiB(o.limits.retainedBytes.max)}`}</span> : null}
          note="prompts kept for probes"
          failure={failed(overviewError)}
        />
        <ProbeTile overview={o} error={overviewError} />
        {/* The count is router-wide, so a warm tone means at least one pool is paying for probes that did not land. */}
        <Tile
          label="Expired between turns, 24h"
          value={o ? fmtInt(o.expiredBetweenTurns24h) : null}
          note="misses cache warming exists to prevent"
          tone={o && o.expiredBetweenTurns24h > 0 && warmingSomewhere ? 'warning' : undefined}
          failure={failed(overviewError)}
        />
        <Tile
          label="Bus gaps"
          value={o ? fmtInt(o.busGaps) : null}
          note="times the keepalive engine missed bus events"
          tone={o && o.busGaps > 0 ? 'warning' : undefined}
          failure={failed(overviewError)}
        />
      </div>
      {firstFailed !== null && (
        <p className="sessions__notice" role="status">
          Could not load the {firstFailed.name}: {firstFailed.error}{' '}
          <Button size="sm" onClick={reloadOf[firstFailed.name]}>
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
        subtitle="One row per session. Open a row for the cache-warming recipe each agent carries"
        actions={
          rows && rows.length > 0 ? (
            <Button size="sm" onClick={() => setExpanded(allOpen ? new Set() : new Set(rows.map((r) => rowKey(r.session))))} aria-pressed={allOpen}>
              {allOpen ? 'Collapse all' : 'Expand all'}
            </Button>
          ) : undefined
        }
      >
        {rows === null && cardFailed !== null ? (
          <div style={{ padding: 20 }}>
            <ErrorState
              title="Could not load the sessions"
              body={cardFailed.error}
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
            {hiddenTitle === undefined ? (
              <EmptyState title="No live sessions" body="A session appears here on its first request, or as soon as the keepalive mod registers it, and drops off once it passes the session expiry." />
            ) : (
              <EmptyState title="No sessions kept by the filters" body={`${hiddenTitle}.`} />
            )}
          </div>
        ) : (
          <div className="table-wrap">
            <table className="table sessions-table">
              <thead>
                <tr>
                  {/* The gutter keeps its own width and takes neither column class, whose 1% would outrank it. */}
                  <th className="sessions-table__gutter" />
                  <th className="col-fill">Session</th>
                  {visible.has('key') && <th className="col-fit">Key</th>}
                  {visible.has('model') && <th className="col-fit">Model</th>}
                  {visible.has('context') && <th className="col-fit">Context</th>}
                  {visible.has('effort') && <th className="col-fit">Effort</th>}
                  {visible.has('runway') && <th className="col-fit">Runway</th>}
                  {visible.has('lastUsed') && <th className="col-fit">Last used</th>}
                  {visible.has('requests') && <th className="col-fit right">Requests</th>}
                  {visible.has('lastRequest') && <th className="col-fit">Last request</th>}
                  {visible.has('agents') && <th className="col-fit right">Agents</th>}
                  {visible.has('companion') && <th className="col-fit">Companion</th>}
                  {visible.has('cacheWarming') && <th className="col-fit">Cache warming</th>}
                  {visible.has('nextProbe') && <th className="col-fit">Next probe</th>}
                  <th className="col-fit" />
                </tr>
              </thead>
              <tbody>
                {rows.map((row) => (
                  <SessionRows
                    key={rowKey(row.session)}
                    accountById={accountById}
                    row={row}
                    open={expanded.has(rowKey(row.session))}
                    onToggle={() => toggle(rowKey(row.session))}
                    now={now}
                    columns={columns}
                    current={current === rowDomId(row.session)}
                    expireAfterSeconds={expireAfterSeconds}
                    forkBase={row.session.forkedFrom === null ? null : rowDomId({ ...row.session, sessionId: row.session.forkedFrom })}
                    listedRows={listedRows}
                    onFollowFork={followFork}
                    onMenuChange={(open) => menuChange(rowKey(row.session), open)}
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
            ? `Session ${sessionLabel(pending.session, SESSION_CHARS)} loses its pin to ${pending.session.accountId ?? 'its account'} and every recipe on it is dropped. The next request picks an account again and the prompt cache on ${pending.session.accountId ?? 'it'} goes cold.`
            : ''
        }
        onConfirm={onConfirmRelease}
      />
    </div>
  );
}

function SessionRows({
  accountById,
  row,
  open,
  onToggle,
  now,
  columns,
  current,
  expireAfterSeconds,
  forkBase,
  listedRows,
  onFollowFork,
  onMenuChange,
  onOpen,
  onRelease,
  visible,
}: {
  accountById: ReadonlyMap<string, Account>;
  columns: number;
  /** Whether a fork link last led here. */
  current: boolean;
  expireAfterSeconds: number | null;
  /** The element id the base of a forked session would carry, whether or not it is listed; null off a fork. */
  forkBase: string | null;
  listedRows: ReadonlyMap<string, Row>;
  now: number;
  onFollowFork: (id: string) => void;
  onMenuChange: (open: boolean) => void;
  onOpen: (path: string) => void;
  onRelease: () => void;
  onToggle: () => void;
  open: boolean;
  row: Row;
  visible: ReadonlySet<SessionColumnId>;
}) {
  const { account, lines, session, shown } = row;
  const summary = stateSummary(shown);
  // A line takes its account from its pin, or from its recipe where no pin stands, and a recipe whose request was
  // refused before any account served it names none, so that line is left out. The badge counts every other line on
  // another model, recipe or not, and the hover reads them in the order the expanded row draws them.
  const otherModels = lines.flatMap((l) => {
    const accountId = l.pin?.accountId ?? l.recipe?.accountId ?? null;
    return accountId !== null && l.model !== session.model ? [{ accountId, agentId: l.agentId, model: l.model }] : [];
  });
  // A retained agent with no time is waiting on its first stream event, which is
  // a different thing from a session nothing probes.
  const pendingProbe = shown.some((a) => a.state === 'retained' && a.nextProbeAt === null);
  // A session known from the ledger or from a registration alone carries no activity of its own, so the last
  // ledgered request stands in for it and the expiry counts from there.
  const lastUsed = session.lastUsedAt ?? session.lastRequestAt;
  const expiresAt = expireAfterSeconds === null || lastUsed === null ? null : new Date(Date.parse(lastUsed) + expireAfterSeconds * 1000).toISOString();
  // Analytics and the request log read the ledger, so a session that never reached it has nothing to open. The count
  // leaves a refused request out while the log lists it, so a ledgered row reads off the last request instead.
  const hasTraffic = session.lastRequestAt !== null;
  const trafficSession = session.sessionId;
  // The base's row where the list holds it: the fork badge reads the account relation off the two listed rows, and
  // only while both pins are live, since a released or expired pin leaves the last ledgered request's account on the
  // row.
  const baseRow = forkBase === null ? null : (listedRows.get(forkBase) ?? null);
  const baseAccountId = baseRow !== null && baseRow.session.pinned ? baseRow.session.accountId : null;
  // The row's figures are main's conversation pin's, so a row with no pin reads a dash.
  const context = contextCell(session.pinned ? session.lastInputTokens : null);
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
        <td className="mono col-fill" title={session.sessionId}>
          <span className="srow__id">{sessionLabel(session)}</span>
          {session.forkedFrom !== null && forkBase !== null && (
            <ForkBadge
              base={baseRow ? forkBase : null}
              baseAccount={baseAccountId === null ? null : { id: baseAccountId, label: baseRow?.account?.label ?? baseAccountId }}
              depth={session.forkDepth}
              from={session.forkedFrom}
              onFollow={onFollowFork}
              rowAccountId={session.pinned ? session.accountId : null}
            />
          )}
          {(session.epoch ?? 1) > 1 && (
            <span className="sub srow__epoch" title="How many times this session's pin was re-created">
              epoch {session.epoch}
            </span>
          )}
          {/* An unpinned row on a slowed account carries no speed, so the row's own speed gates main's chip. */}
          {session.speed !== null && <SpeedChip {...memberSpeed(session, account, now)} />}
        </td>
        {visible.has('key') && (
          <td className="mono col-fit">
            {session.keyId}
            {session.historical && (
              <span className="sub" title={HISTORICAL_HINT}>
                deleted holder
              </span>
            )}
          </td>
        )}
        {visible.has('model') && (
          <td className="mono nowrap col-fit" title={session.model === null ? undefined : modelTitle(session.model, shortModel(session.model))}>
            <span className="srow__pin">
              {session.model === null ? <span className="muted">-</span> : shortModel(session.model)}
              <OtherPins agents={otherModels} />
            </span>
          </td>
        )}
        {visible.has('context') && (
          <td className="col-fit">
            <span className={context === '-' ? 'muted' : 'num'}>{context}</span>
          </td>
        )}
        {visible.has('effort') && <td className="col-fit">{session.effort ?? <span className="muted">-</span>}</td>}
        {visible.has('runway') && (
          <td className="col-fit" title={session.pinned && session.cycleMs !== null ? `cycle ${ageWord(session.cycleMs)}` : undefined}>
            {runwayWord(session) ?? <span className="muted">-</span>}
          </td>
        )}
        {/* Expiry rides under the last-used time rather than in a column of its own, which is 90px the 1600px window did not have. */}
        {visible.has('lastUsed') && (
          <td className="col-fit" title={fmtAbsolute(lastUsed)}>
            {fmtRelative(lastUsed, now)}
            {expiresAt && (
              <span className="sub" title={`Drops off this list at ${fmtAbsolute(expiresAt)}`}>
                expires {fmtRelative(expiresAt, now)}
              </span>
            )}
          </td>
        )}
        {visible.has('requests') && (
          <td className="right num col-fit" title={hasTraffic ? undefined : NO_TRAFFIC_HINT}>
            {session.requests > 0 ? fmtInt(session.requests) : <span className="muted">0</span>}
          </td>
        )}
        {visible.has('lastRequest') && (
          <td className="col-fit" title={session.lastRequestAt ? fmtAbsolute(session.lastRequestAt) : undefined}>
            {session.lastRequestAt ? <span className="num">{fmtRelative(session.lastRequestAt, now)}</span> : <span className="muted">none</span>}
          </td>
        )}
        {/* The sessions and the recipes are read apart, so the count stops at zero while the two disagree. */}
        {visible.has('agents') && <td className="right num col-fit">{Math.max(0, session.agents - row.released)}</td>}
        {visible.has('companion') && (
          <td className="col-fit">
            {session.companion === 'reporting' ? (
              <>
                <Badge tone="good" dot title={COMPANION_HINT.reporting}>
                  reporting
                </Badge>
                {session.presenceAgeSeconds !== null && (
                  <span className="sub srow__presence" title="When the person was last seen at the terminal">
                    presence {fmtDuration(session.presenceAgeSeconds * 1000)} ago
                  </span>
                )}
              </>
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
          <td className="col-fit">
            {session.keepalive.state && !row.noRecipe ? (
              <>
                <Badge tone={PIN_TONE[session.keepalive.state]} dot title={PIN_HINT[session.keepalive.state]}>
                  {session.keepalive.state}
                </Badge>
                {summary && <span className="sub srow__summary">{summary}</span>}
              </>
            ) : (
              <span className="muted" title={session.historical ? HISTORICAL_NO_RECIPE_HINT : NO_RECIPE_HINT}>
                no recipe
              </span>
            )}
          </td>
        )}
        {visible.has('nextProbe') && (
          <td className="col-fit" title={session.keepalive.nextProbeAt ? fmtAbsolute(session.keepalive.nextProbeAt) : undefined}>
            {session.keepalive.nextProbeAt ? (
              <span className="num">{fmtRelative(session.keepalive.nextProbeAt, now)}</span>
            ) : (
              <span className="muted">{pendingProbe ? 'pending' : 'not scheduled'}</span>
            )}
          </td>
        )}
        {/* The actions sit in a menu, a fraction of spelled-out buttons' width, and close the row: the id leads it, so a
            reader scans ids down the first column, and the actions stay reachable at the row's end at 1280 px. */}
        <td className="col-fit" onClick={(e) => e.stopPropagation()} onKeyDown={(e) => e.stopPropagation()}>
          <Menu
            label={`Actions for session ${shortId(session.sessionId, SESSION_CHARS)}${session.historical ? ', deleted holder' : ''}`}
            onOpenChange={onMenuChange}
            items={[
              { label: 'View in analytics', disabled: !hasTraffic, title: hasTraffic ? undefined : NO_TRAFFIC_HINT, onClick: () => onOpen(analyticsHref(trafficSession, session)) },
              { label: 'View in requests', disabled: !hasTraffic, title: hasTraffic ? undefined : NO_TRAFFIC_HINT, onClick: () => onOpen(requestsHref(trafficSession, session, session.lastRequestAt)) },
              // The routing log reads the decisions, which exist once a pin does, so it needs no ledgered request.
              { label: 'View in routing log', disabled: !session.sessionId, onClick: () => onOpen(logHref({ session: session.sessionId, ...aroundSubject(session.lastRequestAt ? Date.parse(session.lastRequestAt) : null, now) })) },
              ...(!session.historical ? [{ label: 'Release', onClick: onRelease }] : []),
            ]}
          />
        </td>
      </tr>
      {open && (
        <tr className="srow__detail table__detail">
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
            {/* A finished session keeps its lines, so the sentence stands above them. */}
            {row.finished && shown.length > 0 && (
              <p className="agents__released">The mod released every agent of this session. Their records keep their slots of the cap for one cache TTL after the release and then leave the engine.</p>
            )}
            {/* The union's length, so a session no mod reports draws its pins' lines. */}
            {lines.length > 0 && (
              <div className="agents">
                <div className="agents__row agents__row--head">
                  <span>Agent</span>
                  <span>Model</span>
                  <span>Effort</span>
                  <span>Context</span>
                  <span>Account</span>
                  <span>TTL</span>
                  <span>Last request</span>
                  <span>Last probe</span>
                  <span>Next probe</span>
                  <span title={COUNTS_HINT}>Probes / credited</span>
                  <span>State</span>
                </div>
                {lines.map((line) => (
                  <AgentLine
                    key={`${line.agentId}|${line.model}`}
                    accountById={accountById}
                    line={line}
                    now={now}
                    requestsTo={hasTraffic ? requestsHref(trafficSession, session, lineLastRequestAt(line) ?? session.lastRequestAt, line.agentId) : null}
                  />
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
 * A forked session's marker: a link to its base's row where the base is listed, plain where it is not, with the base's
 * whole id and the depth on the title. The link stops at the row, whose click and Enter would otherwise open it. The
 * title describes the badge through the card's host, so a reader that never hovers hears the relation.
 */
function ForkBadge({
  base,
  baseAccount,
  depth,
  from,
  onFollow,
  rowAccountId,
}: {
  base: string | null;
  /**
   * The listed base's account while its pin is live, its label as the Accounts page names it. Null off an unlisted
   * base or one whose pin is gone.
   */
  baseAccount: { id: string; label: string } | null;
  depth: number | null;
  from: string;
  onFollow: (id: string) => void;
  /** The row's own account while its pin is live, and null off a row whose pin is gone. */
  rowAccountId: string | null;
}) {
  const at = depth === null ? '' : ` at depth ${depth}`;
  // The relation is read off the two listed rows alone: a base this list does not hold, or a row without a live pin,
  // is never claimed to be on the same account or another.
  const relation = base !== null && baseAccount !== null && rowAccountId !== null ? (baseAccount.id === rowAccountId ? 'same account' : 'on another account') : null;
  const title =
    base === null
      ? `Branched from session ${from}${at}, which this list does not hold`
      : relation === null
        ? `Branched from session ${from}${at}. Go to its row`
        : `Branched from session ${from}${at} on ${baseAccount?.label}. Go to its row`;
  const hosted = useHostedDescription(title);
  // The badge sits under the session id, so it names the relation or the depth alone, and the title the base.
  const words = relation !== null ? `fork, ${relation}` : depth === null ? 'fork' : `fork at ${depth}`;
  if (base === null)
    return (
      <span className="badge badge--accent srow__fork" title={title} aria-describedby={hosted}>
        {words}
      </span>
    );
  return (
    <a
      className="badge badge--accent srow__fork"
      href={`#${base}`}
      title={title}
      aria-describedby={hosted}
      onClick={(e) => {
        e.preventDefault();
        e.stopPropagation();
        onFollow(base);
      }}
      onKeyDown={(e) => e.stopPropagation()}
    >
      {words}
    </a>
  );
}

/** A pin no recipe stands on reads its prompt cache's state by its own TTL, each word with its sentence. */
const PIN_CACHE_HINT = {
  cold: "The pin's prompt cache has passed its TTL, and the next request pays a cold read.",
  warm: "The pin's prompt cache is within its TTL.",
};

/**
 * A line's last request, its recipe's or, where no recipe stands, its pin's last use. The Last request cell draws it
 * and the Agent cell's link opens the narrowest range that covers it, so the log the link opens holds the request the
 * cell names.
 */
const lineLastRequestAt = ({ pin, recipe }: SessionLine): string | null => (recipe !== null ? recipe.lastRequestAt : (pin?.lastUsedAt ?? null));

/**
 * One line of an expanded row. A recipe's line reads its own model, account, TTL, probes and state as the mod reports
 * them, and its pin's Effort and Context, dashes where it stands on none. A pin no recipe stands on reads its own
 * figures, its TTL as the time left on its cache, its last use as its last request, a dash in each probe cell and a
 * warm or cold state, with no lifecycle line under it.
 */
function AgentLine({ accountById, line, now, requestsTo }: { accountById: ReadonlyMap<string, Account>; line: SessionLine; now: number; requestsTo: string | null }) {
  const { pin, recipe: a } = line;
  const context = contextCell(pin?.lastInputTokens ?? null);
  const accountId = a !== null ? a.accountId : (pin?.accountId ?? null);
  const ttlMs = a !== null ? a.ttlSeconds * 1000 : pin === null ? null : pinTtlMs(pin, now);
  const lastRequestAt = lineLastRequestAt(line);
  const cache = pin?.warm ? 'warm' : 'cold';
  return (
    <div className="agents__row">
      <span className="mono agents__id" title={line.agentId}>
        {requestsTo === null ? line.agentId : <Link to={requestsTo}>{line.agentId}</Link>}
      </span>
      <span className="mono nowrap" title={modelTitle(line.model, shortModel(line.model))}>
        {shortModel(line.model)}
      </span>
      <span>{pin?.effort ?? <span className="muted">-</span>}</span>
      <span className={context === '-' ? 'muted' : 'num'}>{context}</span>
      {accountId === null ? <span className="muted">none</span> : <LineAccount account={accountById.get(accountId) ?? null} id={accountId} now={now} />}
      {ttlMs === null ? <span className="muted">-</span> : <span className="num">{fmtDuration(ttlMs)}</span>}
      <span title={lastRequestAt ? fmtAbsolute(lastRequestAt) : undefined}>{lastRequestAt ? <span className="num">{fmtRelative(lastRequestAt, now)}</span> : <span className="muted">none</span>}</span>
      {a === null ? (
        <>
          <span className="muted">-</span>
          <span className="muted">-</span>
          <span className="muted">-</span>
          <span className="agents__state row" style={{ gap: 6 }}>
            <Badge tone={cache === 'warm' ? 'good' : 'neutral'} dot title={PIN_CACHE_HINT[cache]}>
              {cache}
            </Badge>
          </span>
        </>
      ) : (
        <>
          <span className="agents__probe" title={a.lastProbeAt ? fmtAbsolute(a.lastProbeAt) : undefined}>
            {a.lastProbeAt ? <span className="num">{fmtRelative(a.lastProbeAt, now)}</span> : <span className="muted">none</span>}
          </span>
          <span title={a.nextProbeAt ? fmtAbsolute(a.nextProbeAt) : undefined}>
            {a.nextProbeAt ? <span className="num">{fmtRelative(a.nextProbeAt, now)}</span> : <span className="muted">{a.state === 'retained' ? 'on first stream event' : 'not scheduled'}</span>}
          </span>
          <span className="agents__counts num" title={COUNTS_HINT}>
            {a.probesToday} / {a.creditedToday}
          </span>
          {/* The state word carries its cause's sentence on hover, and the line below repeats it for a reader that
              never hovers. */}
          <span className="agents__state row" style={{ gap: 6 }}>
            <Badge tone={STATE_TONE[a.state]} dot title={CAUSE_HINT[a.cause]}>
              {STATE_LABEL[a.state]}
            </Badge>
            <Badge tone={WARNING_CAUSES.includes(a.cause) ? 'warning' : 'neutral'} title={CAUSE_HINT[a.cause]}>
              {CAUSE_LABEL[a.cause]}
            </Badge>
          </span>
          <div className="agents__more">
            <span className="agents__life">
              <Badge tone={PIN_TONE[a.agentState]} dot title={PIN_HINT[a.agentState]}>
                {a.agentState}
              </Badge>
              {parkDetail(a, now)}
            </span>
            <p className="agents__cause">
              {STATE_HINT[a.state]} {CAUSE_HINT[a.cause]}
            </p>
          </div>
        </>
      )}
    </div>
  );
}

/**
 * A line's account: a link to its page with the kind on its title, described to a reader that never hovers, and its
 * slow-down chip while the slow-down holds. An account the list lacks reads its id alone.
 */
function LineAccount({ account, id, now }: { account: Account | null; id: string; now: number }) {
  const title = account ? KIND_HINT[account.kind] : undefined;
  const kind = useHostedDescription(title);
  return (
    <span className="agents__account">
      <Link to={`/accounts/${id}`} title={title} aria-describedby={kind}>
        {id}
      </Link>
      <SpeedChip {...memberSpeed({ speed: null }, account, now)} />
    </span>
  );
}

/** "+2" beside a session's main pin, hovering into one line per agent that sits on another model. */
function OtherPins({ agents }: { agents: { accountId: string; agentId: string; model: string }[] }) {
  if (agents.length === 0) return null;
  return (
    <Badge tone="neutral" title={`Agents pinned elsewhere:\n${agents.map((a) => `${a.agentId}: ${a.model} on ${a.accountId}`).join('\n')}`}>
      +{agents.length}
    </Badge>
  );
}

function ProbeTile({ error, overview: o }: { error: string | null; overview: KeepaliveOverview | null }) {
  const total = o ? o.probes24h.ok + o.probes24h.migrated + o.probes24h.held + o.probes24h.skipped + o.probes24h.failed : 0;
  return (
    <Tile
      label="Cache probes, 24h"
      value={o ? fmtInt(total) : null}
      failure={failed(error)}
      note={
        o ? (
          <>
            <div className="probe-bar" role="img" aria-label={`Probes in the last 24h: ${o.probes24h.ok} ok, ${o.probes24h.migrated} migrated, ${o.probes24h.held} held, ${o.probes24h.skipped} skipped, ${o.probes24h.failed} failed`}>
              <span className="probe-bar__seg probe-bar__seg--ok" style={{ flex: o.probes24h.ok }} />
              <span className="probe-bar__seg probe-bar__seg--migrated" style={{ flex: o.probes24h.migrated }} title={MIGRATED_PROBE_HINT} />
              <span className="probe-bar__seg probe-bar__seg--held" style={{ flex: o.probes24h.held }} title={HELD_PROBE_HINT} />
              <span className="probe-bar__seg probe-bar__seg--skipped" style={{ flex: o.probes24h.skipped }} />
              <span className="probe-bar__seg probe-bar__seg--failed" style={{ flex: o.probes24h.failed }} />
            </div>
            <span className="stat__note">
              {fmtInt(o.probes24h.ok)} ok ({fmtPct(total ? o.probes24h.ok / total : 0)}), <span title={MIGRATED_PROBE_HINT}>{fmtInt(o.probes24h.migrated)} migrated</span>, <span title={HELD_PROBE_HINT}>{fmtInt(o.probes24h.held)} held</span>,{' '}
              {fmtInt(o.probes24h.skipped)} skipped,{' '}
              {fmtInt(o.probes24h.failed)} failed
            </span>
          </>
        ) : null
      }
    />
  );
}

