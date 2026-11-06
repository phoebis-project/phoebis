import { Fragment, type KeyboardEvent, type MouseEvent, type ReactNode, useId, useMemo, useState } from 'react';
import { Link, useNavigate } from 'react-router-dom';
import { api } from '@/api';
import { iso } from '@/api/decode';
import type { Account, AgentContext, ApiKey, CompanionState, KeepaliveRecipe, ModelTiers, Pool, RecipeState, Session, SessionSpend } from '@/api/types';
import { useAppSettings } from '@/app/settings';
import { useSession } from '@/app/session';
import { FilterBar } from '@/components/FilterBar';
import { SortControl } from '@/components/SortControl';
import { ColumnsControl } from '@/components/ui/ColumnsControl';
import { CostReferenceLink } from '@/components/ui/CostReference';
import type { Option } from '@/components/ui/OptionList';
import { SpeedChip } from '@/components/ui/SpeedChip';
import { IconChevron } from '@/components/ui/icons';
import { PopoverHost, useHoldHost } from '@/components/ui/Popover';
import { Badge, Button, Card, ConfirmDialog, EmptyState, ErrorState, Menu, Skeleton, toggleRow, useHostedDescription } from '@/components/ui/primitives';
import { accountHref, ANALYTICS_CODEC, REQUESTS_CODEC, requestsHref as requestsAddressHref, SESSION_SORT_KEYS, SESSIONS_CODEC, type SessionsAddress, sortAddressOf, sortDirOf, type SortDir } from '@/lib/address';
import { HINT_WINDOW, sentences, shareHint, type WeekReading } from '@/lib/analytics';
import { AGENT_TYPES, clientVersion } from '@/lib/client';
import { clock } from '@/lib/clock';
import { tierOrder, weekReadingOf } from '@/lib/costs';
import { groupInto, lookup } from '@/lib/counts';
import { SESSIONS_DIMENSIONS } from '@/lib/filterBar';
import { SESSION_CHARS, fmtAbsolute, fmtCount, fmtDuration, fmtInt, fmtRelative, fmtShare, fmtUsd, joinPresent, keyGenerationOf, keyLifetime, messageOf, shortId, shortModel, weekShort } from '@/lib/format';
import { ACTIVE_WITHIN, addSpend, agentCount, type AgentFigures, agentFigures, agentFiltersStand, CAUSE_HINT, CAUSE_LABEL, contextCell, drawnLines, earlierAgents, hiddenLines, isFinished, lastActivity, lifecycleSplit, lineWordOf, MAIN_AGENT, matches, matchingAgents, orderLines, orderRows, pinTtlMs, presenceOf, requestsRange, sessionLines, type SessionLine, STATUS_TONE, statusOf, type StatusWord, WARNING_CAUSES } from '@/lib/keepalive';
import { failedFirst, toggled, useAsync, useNow, useQueryFilters } from '@/lib/hooks';
import { minuteSpan, type BadgeTone } from '@/lib/quota';
import { costCellOf, spendHintArgs, spendHover, spendUnitOf } from '@/lib/requests';
import { ageWord, aroundSubject, logHref } from '@/lib/routingLog';
import { memberSpeed } from '@/lib/selection';
import type { UnitPosition, WeekPosition } from '@/lib/unit';
import { keyOptions } from './RequestsPage';

// The Session column is the row's identity and the table's fill column, so it is always drawn and the control toggles
// the others.
const SESSION_COLUMNS = [
  { id: 'status', label: 'Status' },
  { id: 'agents', label: 'Agents', right: true },
  { id: 'model', label: 'Model' },
  { id: 'account', label: 'Account' },
  { id: 'requests', label: 'Requests', right: true },
  { id: 'cost', label: 'Cost', right: true },
  { id: 'lastUsed', label: 'Last used' },
  { id: 'cache', label: 'Cache' },
  { id: 'companion', label: 'Companion' },
  { id: 'context', label: 'Context' },
  { id: 'created', label: 'Created' },
  { id: 'effort', label: 'Effort' },
  { id: 'key', label: 'Key' },
  { id: 'lastRequest', label: 'Last request' },
  { id: 'nextProbe', label: 'Next probe' },
  { id: 'runway', label: 'Runway' },
] as const;
type SessionColumnId = (typeof SESSION_COLUMNS)[number]['id'];
// The default set fits the card at 1280 by 800 without these: the Status cell carries the presence, the expanded row
// carries the pin's figures, the key and the probes, and the Columns control brings each back.
const HIDDEN_BY_DEFAULT: readonly SessionColumnId[] = ['companion', 'context', 'created', 'effort', 'key', 'lastRequest', 'nextProbe', 'runway'];

/**
 * The pin's runway on its account as the route answers it: a span, `none` at zero, `unbounded` where no window binds.
 * Null for a row with no pin or no projection, which the cell draws as a dash.
 */
const runwayWord = (s: Session): string | null => (!s.pinned ? null : s.runwayMs !== null ? (s.runwayMs === 0 ? 'none' : minuteSpan(s.runwayMs)) : s.runwayBounded === false ? 'unbounded' : null);

/** The id as sent on hover where the cell shows less of it. */
const modelTitle = (model: string, shown: string) => (shown === model ? undefined : model);

/** A row's or a line's Status word with its sentence. */
const LIFECYCLE_HINT: Record<StatusWord, string> = {
  busy: 'Between model turns, running tools or streaming. Probed like any other agent, and the idle window of the main conversation keeps running.',
  finished: 'The mod released every agent of this session. Their records keep their slots of the cap for one cache TTL after the release and then leave the engine.',
  idle: "Stopped and waiting for the user. Probed until the pool's idle window runs out, the break-even where the pool sets none.",
  parked: 'Stopped while a background command, a subagent or a scheduled wakeup is open. Probed under the parked and horizon rules and the idle window.',
  registered: 'The mod registered this session and no real request has reached the router from it yet',
  released: 'The mod released this agent, a subagent that finished or the main conversation. Its recipe is gone.',
  unknown: "The reporter could not tell this agent's state.",
  unreported: 'No keepalive mod reports this agent, so the router holds no lifecycle word for it.',
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
/**
 * An optional instant's cell, spread into its wrapper: the time relative to now, else `empty` muted, with the absolute
 * instant on the wrapper's title, so the hover target is the whole cell.
 */
const timeCell = (at: string | null, now: number, empty: string): { children: ReactNode; title: string | undefined } => ({
  children: at ? <span className="num">{fmtRelative(at, now)}</span> : <span className="muted">{empty}</span>,
  title: at ? fmtAbsolute(at) : undefined,
});
const EPOCH_HINT = "How many times this session's pin was re-created";
const NO_TRAFFIC_HINT = 'No ledgered request yet';
const REQUESTS_HINT = "Requests over the session's life";
interface Row {
  /**
   * Every recipe filed under the session, as the route answered them: the no-recipe causes and the Cache cell read this
   * list.
   */
  agents: KeepaliveRecipe[];
  /** When the router forgets the session, from `expiryOf`. */
  expiresAt: number | null;
  /** The engine is done with the session, which draws every line it holds under the released sentence. */
  finished: boolean;
  /**
   * Set where the idle limit ended every recipe, with the instant it ran out where the pool's window gives one. Its
   * phrase reads the clock, so `noRecipeText` writes it at draw time and a tick rebuilds no row.
   */
  idleLimit: { ranOutAt: string | null } | null;
  /**
   * The instant the Last used cell and the expiry count from. A session known from the ledger or from a registration
   * alone carries no activity of its own, so the last ledgered request stands in for it.
   */
  lastUsed: string | null;
  /** The session's lines in the view's order, the released ones among them, which `hiddenLines` draws or hides. */
  lines: SessionLine[];
  /** Why a session carries no recipe at all, the causes that need no clock, null while it has one. */
  noRecipe: string[] | null;
  /** The pool of the session's key, null where the key list does not hold the key or the pools did not load. */
  pool: Pool | null;
  /** The pool of the session's key, null where the key list does not hold the key. */
  poolId: string | null;
  session: Session;
  status: ReturnType<typeof statusOf>;
}

/** The instant the router forgets an idle session, null without an expiry or a use. */
const expiryOf = (lastUsed: string | null, expireAfterSeconds: number | null): number | null =>
  expireAfterSeconds === null || lastUsed === null ? null : Date.parse(lastUsed) + expireAfterSeconds * 1000;

/** Why a session carries no recipe at all, its causes joined into one clause, null while it has one. */
const noRecipeText = ({ idleLimit, noRecipe }: Pick<Row, 'idleLimit' | 'noRecipe'>, now: number): string | null => {
  if (noRecipe === null) return null;
  const idle = idleLimit === null ? [] : [idleLimit.ranOutAt ? `the idle limit ran out ${fmtRelative(idleLimit.ranOutAt, now)}` : 'the idle limit ran out'];
  return [...noRecipe, ...idle].join(' and ');
};

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
  const defaults = REQUESTS_CODEC.defaults;
  const range = requestsRange(lastRequestAt, clock.now());
  return requestsAddressHref({ ...defaults, agentId: agentId ?? defaults.agentId, keys: [keyLifetime(session.keyId, session.keyGeneration)], range: range === null ? defaults.range : { from: '', preset: range, to: '' }, session: trafficSession });
};
/** The analytics on the session the client sent under the row's key lifetime, an agent line's narrowed to its agent. */
const analyticsHref = (trafficSession: string, session: Pick<Session, 'keyGeneration' | 'keyId'>, agentId?: string) =>
  `/analytics?${ANALYTICS_CODEC.toParams({ ...ANALYTICS_CODEC.defaults, agentId: agentId ?? ANALYTICS_CODEC.defaults.agentId, key: keyLifetime(session.keyId, session.keyGeneration), session: trafficSession })}`;

/**
 * A session id reads whole, since the cell's column cuts it with an ellipsis, and a sentence passes `chars` to cut it
 * at a label's width.
 */
const sessionLabel = (session: Session, chars?: number) => (chars === undefined ? session.sessionId : shortId(session.sessionId, chars));

/** A link inside a row opens its page, so its click and Enter stop at the link rather than opening the row. */
const stopAtLink = (e: KeyboardEvent | MouseEvent) => e.stopPropagation();

/**
 * When the pool stopped probing: the longest idle window the router resolves for the pool's recipes, counted from the
 * session's last activity, so the time never reads earlier than the window could have run.
 */
function idleLimitRanOutAt(session: Session, pool: Pool | null): string | null {
  const seconds = pool?.keepaliveResolved?.idleSeconds.max ?? 0;
  return seconds > 0 && session.lastUsedAt !== null ? iso(Date.parse(session.lastUsedAt) + seconds * 1000) : null;
}

/**
 * `poolsFailed`: the pool list did not load, so a session whose pool is not found has no cause the rows can name but
 * that. `sort` and `dir`: the sort control's key and direction, which order the rows and each row's lines.
 */
function buildRows(sessions: Session[], recipes: KeepaliveRecipe[], keys: ApiKey[], pools: Pool[], poolsFailed: boolean, sort: SessionsAddress['sort'], dir: SortDir, expireAfterSeconds: number | null): Row[] {
  const keyById = new Map(keys.map((k) => [k.id, k]));
  // A recipe names no generation and belongs to the live key alone, so it is filed under that generation and
  // an earlier holder of the same key id reads none of them.
  const generationById = new Map(keys.map((k) => [k.id, keyGenerationOf(k)]));
  const poolById = new Map(pools.map((p) => [p.id, p]));
  const bySession = groupInto(recipes, (r) => {
    const g = generationById.get(r.keyId);
    return g === undefined ? null : rowKey({ keyGeneration: g, keyId: r.keyId, sessionId: r.sessionId });
  });
  const rows = sessions.map((session) => {
    const agents = bySession.get(rowKey(session)) ?? [];
    const key = keyById.get(session.keyId) ?? null;
    const pool = key ? poolById.get(key.poolId) ?? null : null;
    // Every agent ended at the idle limit means the retained prompts are already
    // gone, so the session reads as having no recipe even though the rows remain.
    const idleLimitOnly = agents.length > 0 && agents.every((a) => a.cause === 'idle_limit');
    const causes: string[] = [];
    let idleLimit: Row['idleLimit'] = null;
    // The live key's pool and opt-out say nothing about a session that no longer belongs to it, so an earlier
    // holder carries that reason alone.
    if (session.historical) causes.push(HISTORICAL_CAUSE);
    else if (poolsFailed && key !== null && pool === null) causes.push('the pool list did not load');
    else {
      if (pool && !pool.keepalive.enabled) causes.push(`pool ${pool.name} keeps no caches warm`);
      if (key?.keepaliveOptOut) causes.push(`key ${key.name} opts out`);
      if (idleLimitOnly) idleLimit = { ranOutAt: idleLimitRanOutAt(session, pool) };
      else if (causes.length === 0) causes.push('the last request left no cacheable prefix to replay');
    }
    const lastUsed = session.lastUsedAt ?? session.lastRequestAt;
    // The full list stays beside the lines: the causes of a session without a line read every record, and a session
    // whose lines the mod all released still holds records, so it is no session without a recipe.
    const lines = sessionLines(session, agents);
    return {
      agents,
      expiresAt: expiryOf(lastUsed, expireAfterSeconds),
      finished: isFinished(session, lines),
      idleLimit,
      lastUsed,
      lines: orderLines(lines, sort, dir),
      noRecipe: agents.length === 0 || idleLimitOnly ? causes : null,
      pool,
      poolId: key?.poolId ?? null,
      session,
      status: statusOf(session, lines),
    };
  });
  return orderRows(rows, sort, dir);
}

/**
 * The Add filter lists' rows: the fixed words, the listed rows' client versions and their lines' types, efforts and
 * models, and the keys', pools' and accounts' entries, an earlier holder's lifetime among the keys under its own label
 * as the Requests view lists one, and the models in the cost model's tier order.
 */
function filterOptions(rows: readonly Row[], keys: ApiKey[] | null, pools: Pool[], accounts: Account[], modelTiers: ModelTiers): Record<string, Option[]> {
  const words = (values: readonly string[]): Option[] => values.map((value) => ({ label: value, value }));
  const held = (values: (string | null)[]) => [...new Set(values.filter((v): v is string => v !== null))];
  const lines = rows.flatMap((r) => r.lines);
  return {
    account: accounts.map((a) => ({ label: a.label, value: a.id })),
    active: ACTIVE_WITHIN.map((a) => ({ label: a.label, value: a.id })),
    agentType: words(held([...AGENT_TYPES, ...lines.map((l) => l.agentType)]).sort()),
    cache: words(['cold', 'no recipe', 'warm']),
    // Numeric, so 2.1.40 lists before 2.1.310.
    client: words(held(rows.map((r) => clientVersion(r.session.userAgent))).sort((a, b) => a.localeCompare(b, undefined, { numeric: true }))),
    companion: words(['none', 'reporting', 'silent']),
    effort: words(held(lines.map((l) => l.pin?.effort ?? null)).sort()),
    key: keyOptions(keys, held(rows.map((r) => (r.session.historical ? keyLifetime(r.session.keyId, r.session.keyGeneration) : null)))),
    model: words(tierOrder(held(lines.map((l) => l.model)), modelTiers)),
    pool: pools.map((p) => ({ label: p.name, value: p.id })),
    status: words(['busy', 'idle', 'parked', 'registered', 'released', 'unknown', 'unreported']),
  };
}

/** The kept rows' figures the card's summary line reads, each agent counted as the Agents cell counts it. */
function daySummary(rows: readonly Row[]): { agents: number; billed: number; busy: number; equivalent: number; idle: number; parked: number; requests: number } {
  const sum = (values: number[]) => values.reduce((n, v) => n + v, 0);
  const splits = rows.map((r) => lifecycleSplit(r.session, r.lines));
  return {
    agents: sum(rows.map((r) => agentCount(r.lines))),
    billed: sum(rows.flatMap((r) => Object.values(r.session.day.billedUsd))),
    busy: sum(splits.map((s) => s.busy)),
    // A day the cost model priced nothing for adds nothing to the figure.
    equivalent: sum(rows.flatMap((r) => (r.session.day.equivalent === null ? [] : [r.session.day.equivalent]))),
    idle: sum(splits.map((s) => s.idle)),
    parked: sum(splits.map((s) => s.parked)),
    requests: sum(rows.flatMap((r) => Object.values(r.session.day.requests))),
  };
}

/**
 * The recipe the row's Cache cell reads, as the route picks the row's `keepalive` summary: main's where one stands,
 * else the one with the newest last request, a recipe without one last and ties by agent id.
 */
function rowRecipe(agents: readonly KeepaliveRecipe[]): KeepaliveRecipe | null {
  const main = agents.find((a) => a.agentId === MAIN_AGENT);
  if (main !== undefined) return main;
  const at = (a: KeepaliveRecipe) => (a.lastRequestAt === null ? Number.NEGATIVE_INFINITY : Date.parse(a.lastRequestAt));
  return [...agents].sort((a, b) => at(b) - at(a) || a.agentId.localeCompare(b.agentId))[0] ?? null;
}

/**
 * Reads as "2 retained, 1 held" for the Cache warming details' line, from the states still holding a prompt to the ones
 * that ended. An agent awaiting cache evidence is retained with no time yet, and counts as retained here.
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

/** The presence line's hover: the companion word's sentence and the instant the line counts from. */
function presenceTitle(session: Session, now: number): string | undefined {
  if (session.companion === 'reporting' && session.presenceAgeSeconds !== null) return `${COMPANION_HINT.reporting} Last seen ${fmtAbsolute(iso(now - session.presenceAgeSeconds * 1000))}.`;
  if (session.companion === 'silent' && session.companionSince !== null) return `${COMPANION_HINT.silent} Silent since ${fmtAbsolute(session.companionSince)}.`;
  return undefined;
}

/**
 * What the Cost cell reads beside the row: the accounts its hint names, by id for its "+N" hover, the chosen unit and
 * week, and the week's scale with its `failure`, as `weekReadingOf` answers them.
 */
interface CostContext {
  accountById: ReadonlyMap<string, Account>;
  accounts: Account[] | null;
  failure: WeekReading['failure'];
  scale: number | null;
  unit: UnitPosition;
  week: WeekPosition;
}

export function SessionsPage() {
  const [intervalMs, setIntervalMs] = useState(5_000);
  const [pending, setPending] = useState<Row | null>(null);
  // The Cost reference dialog and the bar's and the rows' menus hold the reads through the page's host, since an answer
  // landing under a row's open menu could add or drop rows beneath it.
  const { cause: holdCause, host } = useHoldHost();
  const pauseCause = pending !== null ? 'dialog' : holdCause;
  const reread = { paused: pauseCause !== null, refreshMs: intervalMs };
  // The router keeps a session while it is used, so the list is read again rather than aged on this clock alone: a
  // session still sending requests would otherwise drop off once its first-read activity crossed the expiry. The
  // accounts and the recipes read on the same interval, so the slow-down chips and the lines move with the rows.
  const accounts = useAsync(() => api.listAccounts(), [], reread);
  const keys = useAsync(() => api.listKeys(), []);
  const pools = useAsync(() => api.listPools(), []);
  const recipes = useAsync(() => api.listKeepaliveRecipes(), [], reread);
  const sessions = useAsync(() => api.listSessions(), [], reread);
  // The session expiry, and the cost model whose capacities scale a spend to the chosen week.
  const settings = useAsync(() => api.getSettings(), []);
  const { modelTiers, unit, week } = useAppSettings();
  const weekReading = weekReadingOf(settings.data?.subscriptionCosts ?? null, settings.error, week);
  const now = useNow(5_000);
  const navigate = useNavigate();
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  // The open rows whose fold line was opened, by row, so each row unfolds the agents the filters hide on its own.
  const [unfolded, setUnfolded] = useState<ReadonlySet<string>>(() => new Set());
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
  const poolsFailed = failedFirst(pools);
  // Built without the clock, so a tick only filters the rows again.
  const built = useMemo(
    () => (sessionData && recipeData && keys.data ? buildRows(sessionData, recipeData, keys.data, pools.data ?? [], poolsFailed, filters.sort, dir, expireAfterSeconds) : null),
    [sessionData, recipeData, keys.data, pools.data, poolsFailed, filters.sort, dir, expireAfterSeconds],
  );
  // The router forgets a session once it has been idle for the expiry, so this list has to as well.
  const listed = useMemo(() => built?.filter((r) => r.expiresAt === null || r.expiresAt > now) ?? null, [built, now]);
  // The expiry stays the outer bound, and the filters narrow the list inside it.
  const rows = useMemo(() => listed?.filter((r) => matches(r, filters, now)) ?? null, [listed, filters, now]);
  // The lines each kept row draws under the filters and how many the filters fold away, which its detail and its Next
  // probe cell read.
  const drawn = useMemo(() => new Map((rows ?? []).map((r) => [rowKey(r.session), hiddenLines(r, filters)])), [rows, filters]);
  const options = useMemo(() => (listed === null ? {} : filterOptions(listed, keys.data, pools.data ?? [], accounts.data ?? [], modelTiers)), [listed, keys.data, pools.data, accounts.data, modelTiers]);
  const hiddenRows = listed !== null && rows !== null ? listed.length - rows.length : 0;
  const hiddenTitle = hiddenRows > 0 ? `${fmtCount(hiddenRows, 'session')} hidden by the filters` : undefined;
  const agentsKept = rows === null ? 0 : matchingAgents(rows, filters);
  const matching = agentFiltersStand(filters);
  const countWords = rows === null ? '' : `${fmtCount(rows.length, 'session')} · ${fmtCount(agentsKept, matching ? 'matching agent' : 'agent')}`;
  const summary = rows === null ? null : daySummary(rows);
  // The rows wait on these three reads, so the first that failed before it ever answered names the notice, and the card
  // draws its error state where that read is the sessions. A refresh that fails after a first answer keeps the rows.
  const reads = { keys, recipes, sessions };
  const firstFailed = (['sessions', 'recipes', 'keys'] as const).find((n) => failedFirst(reads[n])) ?? null;
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
  // Main's pin, a fork's base and each line read their account here rather than off the rows, so an accounts answer
  // redraws the slow-down chips and the labels without rebuilding a row.
  const accountById = useMemo(() => new Map((accounts.data ?? []).map((a) => [a.id, a])), [accounts.data]);
  const cost: CostContext = { accountById, accounts: accounts.data, failure: weekReading.failure, scale: weekReading.scale, unit, week };
  const followFork = (id: string) => {
    // jsdom leaves scrollIntoView undefined.
    document.getElementById(id)?.scrollIntoView?.({ block: 'center' });
    setCurrent(id);
  };
  const allOpen = rows !== null && rows.length > 0 && rows.every((r) => expanded.has(rowKey(r.session)));
  const toggle = (id: string) => setExpanded((prev) => toggled(prev, id));
  const unfold = (id: string) => setUnfolded((prev) => toggled(prev, id));
  // The expand gutter, the Session column and the menu are drawn whatever the control hides.
  const columns = visible.size + 3;

  return (
    <PopoverHost.Provider value={host}>
      <div className="stack">
        <FilterBar
          address={address}
          count={hiddenTitle === undefined ? countWords : <span title={hiddenTitle}>{countWords}</span>}
          dimensions={SESSIONS_DIMENSIONS}
          filters={filters}
          options={options}
          placeholder="Session id"
          refresh={{
            intervalMs,
            onIntervalChange: setIntervalMs,
            onRefresh: () => {
              accounts.reload();
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
        {firstFailed !== null && <ReadNotice error={reads[firstFailed].error} name={firstFailed} retry={reads[firstFailed].reload} />}
        {failedFirst(accounts) && <ReadNotice error={accounts.error} name="accounts" retry={accounts.reload} />}
        <div className="page-head">
          <div className="page-head__lead">
            <p className="page-head__meta">
              Every session the router lists: what it runs, where it is pinned, what it has spent and whether its prompt cache is warm.{' '}
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
          <CostReferenceLink tab="plan-rates" />
        </div>

        <Card
          padded={false}
          className="card--table"
          title={<span title="One row per session. Open a row for its agents, its routing and its cache warming.">Sessions</span>}
          subtitle={
            summary === null ? undefined : (
              <>
                {`${fmtCount(summary.agents, 'agent')}: ${fmtInt(summary.busy)} busy, ${fmtInt(summary.idle)} idle, ${fmtInt(summary.parked)} parked · ${fmtCount(summary.requests, 'request')} and `}
                {/* The figure is a Pro-week figure read in the chosen week, so it waits on the cost model's scale. */}
                {weekReading.scale !== null ? weekShort(summary.equivalent * weekReading.scale, week) : weekReading.failure === null ? <Skeleton width={64} /> : <span title={weekReading.failure.text}>-</span>}
                {summary.billed > 0 && ` + ${fmtUsd(summary.billed)}`}
                {' in 24 h'}
              </>
            )
          }
          actions={
            rows && rows.length > 0 ? (
              <Button size="sm" onClick={() => setExpanded(allOpen ? new Set() : new Set(rows.map((r) => rowKey(r.session))))} aria-pressed={allOpen}>
                {allOpen ? 'Collapse all' : 'Expand all'}
              </Button>
            ) : undefined
          }
        >
          {rows !== null && rows.length > 0 ? (
            <div className="table-wrap">
              <table className="table sessions-table">
                <thead>
                  <tr>
                    {/* The gutter keeps its own width and takes neither column class, whose 1% would outrank it. */}
                    <th className="sessions-table__gutter" />
                    <th className="col-fill">Session</th>
                    {SESSION_COLUMNS.filter((c) => visible.has(c.id)).map((c) => (
                      <th key={c.id} className={'right' in c ? 'col-fit right' : 'col-fit'}>
                        {c.label}
                      </th>
                    ))}
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
                      cost={cost}
                      current={current === rowDomId(row.session)}
                      drawn={drawn.get(rowKey(row.session))?.shown ?? []}
                      filtered={{ account: filters.account.length > 0, model: filters.model.length > 0 }}
                      fold={matching ? (drawn.get(rowKey(row.session))?.hidden ?? 0) : null}
                      forkBase={row.session.forkedFrom === null ? null : rowDomId({ ...row.session, sessionId: row.session.forkedFrom })}
                      listedRows={listedRows}
                      onFollowFork={followFork}
                      onOpen={(path) => navigate(path)}
                      onRelease={() => setPending(row)}
                      onUnfold={() => unfold(rowKey(row.session))}
                      released={filters.released}
                      unfolded={unfolded.has(rowKey(row.session))}
                      visible={visible}
                    />
                  ))}
                </tbody>
              </table>
            </div>
          ) : (
            <div style={{ padding: 20 }}>
              {rows === null && firstFailed === 'sessions' ? (
                <ErrorState
                  title="Could not load the sessions"
                  body={reads.sessions.error}
                  retry={() => {
                    sessions.reload();
                    keys.reload();
                    recipes.reload();
                  }}
                />
              ) : rows === null ? (
                <Skeleton height={14} />
              ) : hiddenTitle === undefined ? (
                <EmptyState title="No live sessions" body="A session appears here on its first request, or as soon as the keepalive mod registers it, and drops off once it passes the session expiry." />
              ) : (
                <EmptyState title="No sessions kept by the filters" body={`${hiddenTitle}.`} />
              )}
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
    </PopoverHost.Provider>
  );
}

function ReadNotice({ error, name, retry }: { error: string | null; name: string; retry: () => void }) {
  return (
    <p className="sessions__notice" role="status">
      Could not load the {name}: {error}{' '}
      <Button size="sm" onClick={retry}>
        Try again
      </Button>
    </p>
  );
}

/** A pin's burn members: the row's for its display pin, a line's for its own. */
type BurnFigures = Pick<AgentContext, 'burn' | 'burnEquivalent' | 'burnUnit' | 'burnWindow'>;

/**
 * A pin's burn per hour: the share of the chosen week on a plan pin, the dollars on a pin billed in dollars, and the
 * share of the pin's own window as its title. Null with no pin, no figure, or while the week's scale is out.
 */
function burnReading(pin: BurnFigures | null, cost: CostContext): { text: string; title: string | undefined } | null {
  if (pin === null || pin.burn === null) return null;
  if (pin.burnUnit === 'usd') return { text: `${fmtUsd(pin.burn)}/h`, title: undefined };
  if (pin.burnUnit !== 'window_share' || pin.burnEquivalent === null || cost.scale === null) return null;
  const window = pin.burnWindow === null ? null : (lookup(HINT_WINDOW, pin.burnWindow) ?? pin.burnWindow);
  return { text: `${weekShort(pin.burnEquivalent * cost.scale, cost.week)}/h`, title: window === null ? undefined : `${fmtShare(pin.burn)} of ${window}` };
}

/** The pin's burn per hour under the Cost figure, none on an unpinned row or a pin with no figure. */
function BurnLine({ cost, session }: { cost: CostContext; session: Session }) {
  const burn = burnReading(session.pinned ? session : null, cost);
  return burn === null ? null : (
    <span className="sub" title={burn.title}>
      {burn.text}
    </span>
  );
}

/**
 * The Cost cell: a spend over client requests in the chosen unit, the session's or an agent's, as the request log's
 * cell reads a request, then the dollars billed beside a plan figure and the accounts other than `accountId` whose
 * windows the requests spent on, the figure in full with the probes' share on the title, and `burn` under it. The
 * skeleton while the scale is out, a dash titled with the week reading's failure where it carries one, and a muted
 * dash before any ledgered request.
 */
function CostCell({ accountId, burn = null, cost, spend }: { accountId: string | null; burn?: ReactNode; cost: CostContext; spend: SessionSpend | null }) {
  const args = spend === null ? null : spendHintArgs(spend, cost.accounts);
  const hint = args === null ? null : shareHint(...args);
  const reading = spendUnitOf(spend, cost.unit, accountId, hint);
  if (spend === null || reading === null)
    return (
      <td className="right num col-fit" title={NO_TRAFFIC_HINT}>
        <span className="muted">-</span>
        {burn}
      </td>
    );
  const cell = costCellOf(reading.unit, cost.week, cost.scale);
  if (cell === null)
    return cost.failure === null ? (
      <td className="right num col-fit">
        <Skeleton width={72} />
        {burn}
      </td>
    ) : (
      <td className="right num col-fit" title={cost.failure.text}>
        -{burn}
      </td>
    );
  return (
    <td className="right num col-fit" title={spendHover(spend, cost.week, cost.scale, hint)}>
      <span className="srow__pin srow__pin--wraps">
        {cell.main}
        {reading.billed > 0 && <Badge tone="neutral" title={`${fmtUsd(reading.billed)} billed in dollars`}>+$</Badge>}
        {reading.accounts.length > 0 && (
          <Badge tone="neutral" title={`Other accounts the requests spent on:\n${reading.accounts.map((id) => cost.accountById.get(id)?.label ?? id).join('\n')}`}>
            +{reading.accounts.length}
          </Badge>
        )}
      </span>
      {cell.sub && <span className="sub">{cell.sub}</span>}
      {burn}
    </td>
  );
}

/**
 * The row's account: a link to its page with the kind on its title, described to a reader that never hovers, the
 * slow-down chip on a slowed pin, the shed mark, and the agents pinned on another account. A dash with no account.
 */
function RowAccount({ accent, account, now, others, session }: { accent: boolean; account: Account | null; now: number; others: { accountId: string; agentId: string; model: string }[]; session: Session }) {
  const title = account ? KIND_HINT[account.kind] : undefined;
  const kind = useHostedDescription(title);
  if (session.accountId === null) return <span className="muted">-</span>;
  return (
    <span className="srow__pin">
      <Link to={accountHref(session.accountId)} title={title} aria-describedby={kind} onClick={stopAtLink} onKeyDown={stopAtLink}>
        {session.accountId}
      </Link>
      {/* An unpinned row on a slowed account carries no speed, so the row's own speed gates the chip. */}
      {session.speed !== null && <SpeedChip {...memberSpeed(session, account, now)} />}
      <ShedBadge session={session} />
      <OtherPins accent={accent} agents={others} lead="Agents on another account:" />
    </span>
  );
}

/** The warning badge `leaving` on a session a shedding round or a failover marked to leave its account. */
function ShedBadge({ session }: { session: Pick<Session, 'shedFrom' | 'shedReason'> }) {
  if (session.shedFrom === null) return null;
  // A pin stamped before the mark carried its reason reads as shed.
  return (
    <Badge tone="warning" title={`Marked to leave ${session.shedFrom}: ${session.shedReason ?? 'shed'}. The next request moves the session`}>
      leaving
    </Badge>
  );
}

/** The mod's state for a session: `reporting` with `seen {ago}`, `silent` with how long, or `none` muted. */
function CompanionChip({ now, session }: { now: number; session: Session }) {
  const presence = presenceOf(session, now);
  if (session.companion === 'reporting')
    return (
      <>
        <Badge tone="good" dot title={COMPANION_HINT.reporting}>
          reporting
        </Badge>
        {presence && (
          <span className="sub srow__presence" title={presenceTitle(session, now)}>
            {presence.text}
          </span>
        )}
      </>
    );
  if (session.companion === 'silent')
    return (
      <>
        <Badge tone="warning" dot title={COMPANION_HINT.silent}>
          silent
        </Badge>
        {session.companionSince !== null && (
          <span className="sub" title={presenceTitle(session, now)}>
            {fmtDuration(now - Date.parse(session.companionSince))}
          </span>
        )}
      </>
    );
  return (
    <span className="muted" title={COMPANION_HINT.none}>
      none
    </span>
  );
}

/**
 * A fork's relation to its base's account, read off the two listed rows alone: a base this list does not hold, or a
 * row without a live pin, is never claimed to be on the same account or another.
 */
const forkRelation = (baseAccountId: string | null, rowAccountId: string | null): string | null =>
  baseAccountId === null || rowAccountId === null ? null : baseAccountId === rowAccountId ? 'same account' : 'on another account';

/** One fact of an open row: its label and its value. */
type Fact = [string, ReactNode];

/** A grid of an open row's facts under its title, with the title's link beside it. */
function Facts({ link, rows, title }: { link?: ReactNode; rows: Fact[]; title: string }) {
  return (
    <div className="facts__group">
      <div className="facts__head">
        <span className="facts__title">{title}</span>
        {link}
      </div>
      <dl className="kv facts">
        {rows.map(([label, value]) => (
          <div key={label} className="kv__row">
            <dt>{label}</dt>
            <dd>{value}</dd>
          </div>
        ))}
      </dl>
    </div>
  );
}

/**
 * A line that opens and shuts what `controls` names, as the head's disclosure does: the chevron, turned as the row's
 * gutter is while open, and the label on a link button, then the hint in the secondary ink.
 */
function Disclosure({ className, controls, hint, label, onToggle, open, title }: { className?: string; controls: string; hint?: ReactNode; label: string; onToggle: () => void; open: boolean; title?: string }) {
  return (
    <div className={className === undefined ? 'disclosure' : `disclosure ${className}`}>
      <button type="button" className="link-btn disclosure__toggle" aria-expanded={open} aria-controls={controls} title={title} onClick={onToggle}>
        <span className="disclosure__chevron" aria-hidden>
          <IconChevron size={14} />
        </span>
        {label}
      </button>
      {hint !== undefined && <span className="disclosure__hint">{hint}</span>}
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
  cost,
  current,
  drawn,
  filtered,
  fold,
  forkBase,
  listedRows,
  onFollowFork,
  onOpen,
  onRelease,
  onUnfold,
  released,
  unfolded,
  visible,
}: {
  accountById: ReadonlyMap<string, Account>;
  columns: number;
  cost: CostContext;
  /** Whether a fork link last led here. */
  current: boolean;
  /** The lines the row draws, `hiddenLines`' answer under the filters. */
  drawn: SessionLine[];
  /** Whether the Account and the Model filters stand, which accent the "+N" of their column. */
  filtered: { account: boolean; model: boolean };
  /** How many agents the Agents filters fold out of the open row, null while none stands. */
  fold: number | null;
  /** The element id the base of a forked session would carry, whether or not it is listed; null off a fork. */
  forkBase: string | null;
  listedRows: ReadonlyMap<string, Row>;
  now: number;
  onFollowFork: (id: string) => void;
  onOpen: (path: string) => void;
  onRelease: () => void;
  onToggle: () => void;
  onUnfold: () => void;
  open: boolean;
  /** Whether the Released agents filter stands, which draws a live row's released lines. */
  released: boolean;
  row: Row;
  /** Whether the fold line is open, which draws the agents it counts under the matching lines. */
  unfolded: boolean;
  visible: ReadonlySet<SessionColumnId>;
}) {
  const { expiresAt, lastUsed, session, status } = row;
  const account = accountById.get(session.accountId ?? '') ?? null;
  const noRecipe = noRecipeText(row, now);
  const lines = drawn;
  const shown = drawn.flatMap((l) => (l.recipe === null ? [] : [l.recipe]));
  // A line takes its account from its pin, or from its recipe where no pin stands, and a recipe whose request was
  // refused before any account served it names none, so that line is left out. A "+N" counts every other line the row
  // draws on another model or account, recipe or not, whatever the Agents filters match, and its hover reads them in
  // the order the expanded row draws them. It takes the accent where its column's filter stands and one of its lines
  // matches the filters, a line `drawn` holds.
  const rowLines = drawnLines(row, { released });
  const placed = rowLines.flatMap((l) => {
    const accountId = l.pin?.accountId ?? l.recipe?.accountId ?? null;
    return accountId !== null && l.model !== null ? [{ accountId, agentId: l.agentId, matched: drawn.includes(l), model: l.model }] : [];
  });
  const otherModels = placed.filter((l) => l.model !== session.model);
  const otherAccounts = placed.filter((l) => l.accountId !== session.accountId);
  // Opened, the fold line draws the lines the filters hide and then the earlier agents, the agents it counts.
  const hiddenHere = fold === null ? [] : rowLines.filter((l) => !drawn.includes(l));
  // A retained agent with no time is waiting on its first stream event, which is
  // a different thing from a session nothing probes.
  const pendingProbe = shown.some((a) => a.state === 'retained' && a.nextProbeAt === null);
  // Analytics and the request log read the ledger, so a session that never reached it has nothing to open. The count
  // leaves a refused request out while the log lists it, so a ledgered row reads off the last request instead.
  const hasTraffic = session.lastRequestAt !== null;
  const trafficSession = session.sessionId;
  const activity = lastActivity(session);
  // The base's row where the list holds it: the fork badge reads the account relation off the two listed rows, and
  // only while both pins are live, since a released or expired pin leaves the last ledgered request's account on the
  // row.
  const baseRow = forkBase === null ? null : (listedRows.get(forkBase) ?? null);
  const baseAccountId = baseRow !== null && baseRow.session.pinned ? baseRow.session.accountId : null;
  // The row's figures are main's conversation pin's, so a row with no pin reads a dash.
  const context = contextCell(session.pinned ? session.lastInputTokens : null);
  const forked = session.forkedFrom !== null && forkBase !== null;
  const client = joinPresent([clientVersion(session.userAgent), (session.epoch ?? 1) > 1 ? `epoch ${session.epoch}` : null], ' · ');
  const clientTitle = sentences([session.userAgent, (session.epoch ?? 1) > 1 ? EPOCH_HINT : null]);
  const mainRecipe = row.agents.find((a) => a.agentId === MAIN_AGENT) ?? null;
  const park = status.word === 'parked' && mainRecipe !== null ? parkDetail(mainRecipe, now).replace(/^ · /, '') : '';
  const statusTitle = park === '' ? LIFECYCLE_HINT[status.word] : `${LIFECYCLE_HINT[status.word]}\n${park}`;
  const presence = presenceOf(session, now);
  const split = Object.entries(lifecycleSplit(session, row.lines)).flatMap(([word, n]) => (n > 0 ? [`${fmtInt(n)} ${word}`] : []));
  // The state the route's summary reads, unless no recipe keeps the session's prompt.
  const recipe = row.noRecipe === null ? rowRecipe(row.agents) : null;
  const relation = forkRelation(baseAccountId, session.pinned ? session.accountId : null);
  // The earlier agents are the ones no line names, a released line hidden by the filters among the named.
  const earlier = earlierAgents(session, row.lines);
  const [earlierOpen, setEarlierOpen] = useState(false);
  const [warmingOpen, setWarmingOpen] = useState(false);
  const tableId = useId();
  const warmingId = useId();
  const since = (iso: string) => `${fmtAbsolute(iso)}, ${fmtRelative(iso, now)}`;
  const pinnedSince = row.lines.find((l) => l.agentId === MAIN_AGENT)?.pin?.createdAt ?? session.createdAt;
  const speed = memberSpeed(session, account, now);
  const agentRow = (agentId: string, line: SessionLine | null, figures: AgentFigures | null) => (
    <AgentRow
      key={line === null ? `earlier|${agentId}` : `${agentId}|${line.model}`}
      accountById={accountById}
      agentId={agentId}
      cost={cost}
      figures={figures}
      line={line}
      now={now}
      onOpen={onOpen}
      session={session}
    />
  );
  const cacheTitle =
    session.warm === null
      ? undefined
      : joinPresent([PIN_CACHE_HINT[session.warm ? 'warm' : 'cold'], recipe === null ? NO_RECIPE_HINT : `${STATE_HINT[recipe.state]} ${CAUSE_HINT[recipe.cause]}`, recipe?.nextProbeAt ? `Next probe at ${fmtAbsolute(recipe.nextProbeAt)}.` : null], ' ');
  return (
    <>
      <tr
        id={rowDomId(session)}
        className={`srow ${open ? 'is-open' : ''}`}
        aria-current={current ? 'true' : undefined}
        aria-expanded={open}
        tabIndex={0}
        {...toggleRow(onToggle)}
      >
        <td className="sessions-table__gutter">
          <span className="srow__chevron" aria-hidden>
            <IconChevron size={14} />
          </span>
        </td>
        <td className="mono col-fill" title={session.sessionId}>
          <span className="srow__id ellipsis">{sessionLabel(session)}</span>
          {(forked || client !== '') && (
            <span className="sub srow__client" title={clientTitle === '' ? undefined : clientTitle}>
              {forked && (
                <ForkBadge
                  base={baseRow ? forkBase : null}
                  baseAccount={baseAccountId === null ? null : { id: baseAccountId, label: accountById.get(baseAccountId)?.label ?? baseAccountId }}
                  depth={session.forkDepth}
                  from={session.forkedFrom ?? ''}
                  onFollow={onFollowFork}
                  rowAccountId={session.pinned ? session.accountId : null}
                />
              )}
              {forked && client !== '' && ' · '}
              {client}
            </span>
          )}
        </td>
        {visible.has('status') && (
          <td className="col-fit">
            {status.tone === 'muted' ? (
              <span className="muted" title={statusTitle}>
                {status.word}
              </span>
            ) : (
              <Badge tone={status.tone} dot title={statusTitle}>
                {status.word}
              </Badge>
            )}
            {presence && (
              <span className={presence.tone === 'warning' ? 'sub srow__presence srow__presence--warning' : 'sub srow__presence'} title={presenceTitle(session, now)}>
                {presence.text}
              </span>
            )}
          </td>
        )}
        {visible.has('agents') && (
          <td className="right num col-fit">
            {fmtInt(agentCount(row.lines))}
            {split.length > 0 && (
              <span className="sub srow__split">
                {split.map((part, i) => (
                  <Fragment key={part}>
                    {i > 0 && ', '}
                    <span className="nowrap">{part}</span>
                  </Fragment>
                ))}
              </span>
            )}
          </td>
        )}
        {visible.has('model') && (
          <td className="mono nowrap col-fit" title={session.model === null ? undefined : modelTitle(session.model, shortModel(session.model))}>
            <span className="srow__pin srow__pin--wraps">
              {session.model === null ? <span className="muted">-</span> : shortModel(session.model)}
              <OtherPins accent={filtered.model && otherModels.some((l) => l.matched)} agents={otherModels} lead="Agents on another model:" />
            </span>
          </td>
        )}
        {visible.has('account') && (
          <td className="col-fit">
            <RowAccount accent={filtered.account && otherAccounts.some((l) => l.matched)} account={account} now={now} others={otherAccounts} session={session} />
          </td>
        )}
        {visible.has('requests') && (
          <td className="right num col-fit" title={sentences([REQUESTS_HINT, session.lastRequestAt === null ? null : `Last request ${fmtAbsolute(session.lastRequestAt)}`])}>
            {session.requests > 0 ? fmtInt(session.requests) : <span className="muted">0</span>}
            {session.lastRequestAt !== null && <span className="sub">{fmtRelative(session.lastRequestAt, now)}</span>}
          </td>
        )}
        {visible.has('cost') && <CostCell accountId={session.accountId} burn={<BurnLine cost={cost} session={session} />} cost={cost} spend={session.spend} />}
        {/* Expiry rides under the last-used time rather than in a column of its own, which is 90px the 1600px window did not have. */}
        {visible.has('lastUsed') && (
          <td className="col-fit" title={fmtAbsolute(lastUsed)}>
            {fmtRelative(lastUsed, now)}
            {expiresAt !== null && (
              <span className="sub" title={`Drops off this list at ${fmtAbsolute(expiresAt)}`}>
                expires {fmtRelative(expiresAt, now)}
              </span>
            )}
          </td>
        )}
        {visible.has('cache') && (
          <td className="col-fit">
            {session.warm === null ? (
              <span className="muted">no pin</span>
            ) : (
              <>
                <Badge tone={session.warm ? 'good' : 'neutral'} dot title={cacheTitle}>
                  {session.warm ? 'warm' : 'cold'}
                </Badge>
                <span className="sub">{recipe === null ? 'no recipe' : STATE_LABEL[recipe.state]}</span>
              </>
            )}
          </td>
        )}
        {visible.has('companion') && (
          <td className="col-fit">
            <CompanionChip now={now} session={session} />
          </td>
        )}
        {visible.has('context') && (
          <td className="col-fit">
            <span className={context === '-' ? 'muted' : 'num'}>{context}</span>
          </td>
        )}
        {visible.has('created') && (
          <td className="col-fit" title={session.createdAt === null ? undefined : fmtAbsolute(session.createdAt)}>
            {session.createdAt === null ? <span className="muted">-</span> : fmtRelative(session.createdAt, now)}
          </td>
        )}
        {visible.has('effort') && <td className="col-fit">{session.effort ?? <span className="muted">-</span>}</td>}
        {/* No route lists one key and the Keys view's search matches no id, so the link lands on the key's row. */}
        {visible.has('key') && (
          <td className="mono col-fit">
            <Link to={`/keys#${encodeURIComponent(session.keyId)}`} onClick={stopAtLink} onKeyDown={stopAtLink}>
              {session.keyId}
            </Link>
            {session.historical && (
              <span className="sub" title={HISTORICAL_HINT}>
                deleted holder
              </span>
            )}
          </td>
        )}
        {visible.has('lastRequest') && <td className="col-fit">{session.lastRequestAt ? <span className="num">{fmtAbsolute(session.lastRequestAt)}</span> : <span className="muted">none</span>}</td>}
        {visible.has('nextProbe') && (
          <td className="col-fit" {...timeCell(session.keepalive.nextProbeAt, now, pendingProbe ? 'pending' : 'not scheduled')} />
        )}
        {visible.has('runway') && (
          <td className="col-fit" title={session.pinned && session.cycleMs !== null ? `cycle ${ageWord(session.cycleMs)}` : undefined}>
            {runwayWord(session) ?? <span className="muted">-</span>}
          </td>
        )}
        {/* The actions sit in a menu, a fraction of spelled-out buttons' width, and close the row: the id leads it, so a
            reader scans ids down the first column, and the actions stay reachable at the row's end at 1280 px. */}
        <td className="col-fit" onClick={(e) => e.stopPropagation()} onKeyDown={(e) => e.stopPropagation()}>
          <Menu
            label={`Actions for session ${shortId(session.sessionId, SESSION_CHARS)}${session.historical ? ', deleted holder' : ''}`}
            items={[
              { label: 'View in analytics', disabled: !hasTraffic, title: hasTraffic ? undefined : NO_TRAFFIC_HINT, onClick: () => onOpen(analyticsHref(trafficSession, session)) },
              { label: 'View in requests', disabled: !hasTraffic, title: hasTraffic ? undefined : NO_TRAFFIC_HINT, onClick: () => onOpen(requestsHref(trafficSession, session, session.lastRequestAt)) },
              // The routing log reads the decisions, which exist once a pin does, so it needs no ledgered request and
              // opens an hour either side of the session's last activity.
              { label: 'View in routing log', disabled: !session.sessionId, onClick: () => onOpen(logHref({ session: session.sessionId, ...aroundSubject(activity === null ? null : Date.parse(activity), now) })) },
              ...(!session.historical ? [{ label: 'Release', onClick: onRelease }] : []),
            ]}
          />
        </td>
      </tr>
      {open && (
        <tr className="srow__detail table__detail">
          <td colSpan={columns}>
            <div className="srow__panel">
              {row.finished && <p className="srow__finished">{LIFECYCLE_HINT.finished}</p>}
              <div className="srow__facts">
                <Facts
                  title="Session"
                  rows={[
                    ['Created', session.createdAt === null ? <span className="muted">unknown</span> : since(session.createdAt)],
                    [
                      'Key',
                      session.historical ? (
                        <>
                          <span className="mono">{session.keyId}</span>,{' '}
                          <span className="muted" title={HISTORICAL_HINT}>
                            deleted holder
                          </span>
                        </>
                      ) : (
                        <>
                          {/* The Keys view lands on the key's row by its id and clears a filter that would hide it. */}
                          <Link className="mono" to={`/keys#${encodeURIComponent(session.keyId)}`}>
                            {session.keyId}
                          </Link>
                          {row.pool !== null && ` in pool ${row.pool.name}`}
                        </>
                      ),
                    ],
                    ['Client', session.userAgent === null ? <span className="muted">-</span> : <span title={session.userAgent}>{clientVersion(session.userAgent)}</span>],
                    [
                      'Fork',
                      session.forkedFrom === null ? (
                        <span className="muted">none</span>
                      ) : (
                        <>
                          {baseRow !== null && forkBase !== null ? (
                            <a
                              className="mono"
                              href={`#${forkBase}`}
                              title={session.forkedFrom}
                              onClick={(e) => {
                                e.preventDefault();
                                onFollowFork(forkBase);
                              }}
                            >
                              {shortId(session.forkedFrom, 13)}
                            </a>
                          ) : (
                            <span className="mono" title={session.forkedFrom}>
                              {shortId(session.forkedFrom, 13)}
                            </span>
                          )}
                          {session.forkDepth === null ? '' : ` at depth ${session.forkDepth}`}
                          {relation === null ? '' : `, ${relation}`}
                        </>
                      ),
                    ],
                    ['Epoch', String(session.epoch ?? 1)],
                    ['Companion', <CompanionChip now={now} session={session} />],
                  ]}
                />
                <Facts
                  title="Routing"
                  link={<Link to={logHref({ session: session.sessionId, ...aroundSubject(activity === null ? null : Date.parse(activity), now) })}>View in routing log</Link>}
                  rows={[
                    [
                      'Account',
                      session.accountId === null ? (
                        <span className="muted">none</span>
                      ) : (
                        <>
                          <Link to={accountHref(session.accountId)}>{session.accountId}</Link>
                          {account !== null && ` ${account.kind === 'oauth' ? 'subscription' : 'API key'}`}
                        </>
                      ),
                    ],
                    ['Pinned since', pinnedSince === null ? <span className="muted">-</span> : since(pinnedSince)],
                    [
                      'Shed mark',
                      session.shedFrom === null ? (
                        <span className="muted">none</span>
                      ) : (
                        <>
                          <ShedBadge session={session} />
                          {` ${session.shedFrom} (${session.shedReason ?? 'shed'})`}
                        </>
                      ),
                    ],
                    ['Runway', runwayWord(session) ?? <span className="muted">-</span>],
                    ['Speed', session.speed !== null && speed.factor !== null && speed.factor < 1 ? <SpeedChip {...speed} /> : 'full speed'],
                    ['Cycle', session.cycleMs === null ? <span className="muted">not measured yet</span> : ageWord(session.cycleMs)],
                  ]}
                />
              </div>
              {/* At a phone's width the open row is narrower than the table, which then scrolls in its own wrap. That
                  wrap clips the agent menus' lists, so it is a table wrap, whose edges a menu bounds its list by. */}
              <div className="table-wrap agents-table__wrap">
                <table id={tableId} className="table agents-table">
                  <thead>
                    <tr>
                      <th>Agent</th>
                      <th>Type</th>
                      <th>Model</th>
                      <th>Account</th>
                      <th>Context</th>
                      <th>Burn</th>
                      <th className="right">Requests</th>
                      <th className="right">Cost</th>
                      <th>Status</th>
                      <th>Cache</th>
                      <th />
                    </tr>
                  </thead>
                  <tbody>
                    {lines.map((line) => agentRow(line.agentId, line, agentFigures(session, line.agentId)))}
                    {unfolded && hiddenHere.map((line) => agentRow(line.agentId, line, agentFigures(session, line.agentId)))}
                    {(fold === null ? earlierOpen : unfolded) && earlier.map((agent) => agentRow(agent.agentId, null, agent))}
                  </tbody>
                </table>
              </div>
              {fold !== null && fold > 0 && <Disclosure className="srow__fold" controls={tableId} label={`${fmtCount(fold, 'agent')} hidden by the filters`} onToggle={onUnfold} open={unfolded} />}
              {/* Under Agents filters the earlier agents are among the agents the fold line counts. */}
              {fold === null && earlier.length > 0 && <EarlierLine agents={earlier} controls={tableId} cost={cost} onToggle={() => setEarlierOpen((v) => !v)} open={earlierOpen} session={session} />}
              <div className="srow__warming">
                <Disclosure
                  controls={warmingId}
                  hint={noRecipe !== null ? 'no recipe' : stateSummary(shown) || 'no recipe'}
                  label="Cache warming"
                  onToggle={() => setWarmingOpen((v) => !v)}
                  open={warmingOpen}
                />
                <div id={warmingId} className="srow__warming-body" hidden={!warmingOpen}>
                  {noRecipe && (
                    <p className="agents__none">
                      <Badge tone="neutral" title={NO_RECIPE_HINT}>
                        no recipe
                      </Badge>
                      <span>Nothing is scheduled because {noRecipe}.</span>
                    </p>
                  )}
                  {/* The union's length, so a session no mod reports draws its pins' lines. */}
                  {lines.length > 0 && (
                    <div className="agents">
                      <div className="agents__row agents__row--head">
                        <span>Agent</span>
                        <span>TTL</span>
                        <span>Last request</span>
                        <span>Last probe</span>
                        <span>Next probe</span>
                        <span title={COUNTS_HINT}>Probes / credited</span>
                        <span>State</span>
                      </div>
                      {lines.map((line) => (
                        <AgentLine key={`${line.agentId}|${line.model}`} line={line} now={now} requestsTo={hasTraffic ? requestsHref(trafficSession, session, lineLastRequestAt(line) ?? session.lastRequestAt, line.agentId) : null} />
                      ))}
                    </div>
                  )}
                </div>
              </div>
            </div>
          </td>
        </tr>
      )}
    </>
  );
}

/**
 * The earlier agents line: how many agents the ledger holds rows of with no line, their requests and their spend as the
 * Cost cell reads it, `+$` where dollars were billed beside a plan figure, and their ids on its title. Opened, each
 * joins the Agents table the line controls.
 */
function EarlierLine({ agents, controls, cost, onToggle, open, session }: { agents: (AgentFigures & { agentId: string })[]; controls: string; cost: CostContext; onToggle: () => void; open: boolean; session: Session }) {
  const [first, ...rest] = agents;
  const spend = rest.reduce((sum, agent) => addSpend(sum, agent.spend), first.spend);
  const requests = rest.reduce((n, agent) => n + agent.requests, first.requests);
  const reading = spendUnitOf(spend, cost.unit, session.accountId, null);
  const cell = reading === null ? null : costCellOf(reading.unit, cost.week, cost.scale);
  return (
    <Disclosure
      className="srow__earlier"
      controls={controls}
      hint={
        <>
          {`· ${fmtCount(requests, 'request')} · ${cell?.main ?? '-'}`}
          {reading !== null && reading.billed > 0 && (
            <>
              {' '}
              <Badge tone="neutral" title={`${fmtUsd(reading.billed)} billed in dollars`}>
                +$
              </Badge>
            </>
          )}
        </>
      }
      label={fmtCount(agents.length, 'earlier agent')}
      onToggle={onToggle}
      open={open}
      title={agents.map((a) => a.agentId).join(', ')}
    />
  );
}

/**
 * A forked session's marker: a link to its base's row where the base is listed, plain where it is not, with the base's
 * whole id, the depth and the relation to the base's account on the title. The link stops at the row, whose click and
 * Enter would otherwise open it. The title describes the badge through the card's host, so a reader that never hovers
 * hears the relation.
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
  const relation = forkRelation(baseAccount?.id ?? null, rowAccountId);
  const title =
    base === null
      ? `Branched from session ${from}${at}, which this list does not hold`
      : relation === null
        ? `Branched from session ${from}${at}. Go to its row`
        : `Branched from session ${from}${at}, ${relation} (${baseAccount?.label}). Go to its row`;
  const hosted = useHostedDescription(title);
  if (base === null)
    return (
      <span className="badge badge--accent srow__fork" title={title} aria-describedby={hosted}>
        fork
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
      fork
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
 * One line of the Cache warming details. A recipe's line reads its TTL, probes and state as the mod reports them. A pin
 * no recipe stands on reads its TTL as the time left on its cache, its last use as its last request, a dash in each
 * probe cell and a warm or cold state, with no lifecycle line under it.
 */
function AgentLine({ line, now, requestsTo }: { line: SessionLine; now: number; requestsTo: string | null }) {
  const { pin, recipe: a } = line;
  const ttlMs = a !== null ? a.ttlSeconds * 1000 : pin === null ? null : pinTtlMs(pin, now);
  const lastRequestAt = lineLastRequestAt(line);
  const cache = pin?.warm ? 'warm' : 'cold';
  return (
    <div className="agents__row">
      <span className="mono agents__id" title={line.agentId}>
        {requestsTo === null ? line.agentId : <Link to={requestsTo}>{line.agentId}</Link>}
      </span>
      {ttlMs === null ? <span className="muted">-</span> : <span className="num">{fmtDuration(ttlMs)}</span>}
      <span {...timeCell(lastRequestAt, now, 'none')} />
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
          <span className="agents__probe" {...timeCell(a.lastProbeAt, now, 'none')} />
          <span {...timeCell(a.nextProbeAt, now, a.state === 'retained' ? 'on first stream event' : 'not scheduled')} />
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
              <Badge tone={STATUS_TONE[a.agentState]} dot title={LIFECYCLE_HINT[a.agentState]}>
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
 * A line's account: a link to its page with the kind on its title, described to a reader that never hovers, then on the
 * row's own account the row's slow-down chip and shed mark, and on another the account's slow-down chip while it
 * holds. An account the list lacks reads its id alone.
 */
function LineAccount({ account, id, now, row }: { account: Account | null; id: string; now: number; row: Session | null }) {
  const title = account ? KIND_HINT[account.kind] : undefined;
  const kind = useHostedDescription(title);
  return (
    <span className="agents__account">
      <Link to={accountHref(id)} title={title} aria-describedby={kind}>
        {id}
      </Link>
      {row === null ? <SpeedChip {...memberSpeed({ speed: null }, account, now)} /> : row.speed !== null && <SpeedChip {...memberSpeed(row, account, now)} />}
      {row !== null && <ShedBadge session={row} />}
    </span>
  );
}

/**
 * One row of the Agents table: a line's agent, read off its pin and its recipe, or an earlier agent, which holds no
 * line and reads its newest ledger entry, dashes for a pin's figures and `no pin`. Requests and Cost read every entry
 * of the agent summed, and the menu opens the session's views narrowed to the agent.
 */
function AgentRow({
  accountById,
  agentId,
  cost,
  figures,
  line,
  now,
  onOpen,
  session,
}: {
  accountById: ReadonlyMap<string, Account>;
  agentId: string;
  cost: CostContext;
  /** The agent's entries summed, null for an agent the ledger holds no row of. */
  figures: AgentFigures | null;
  /** The line the agent draws, null for an earlier agent. */
  line: SessionLine | null;
  now: number;
  onOpen: (path: string) => void;
  session: Session;
}) {
  const pin = line?.pin ?? null;
  const recipe = line?.recipe ?? null;
  // Analytics and the request log read the ledger, so a session that never reached it has nothing to open.
  const hasTraffic = session.lastRequestAt !== null;
  const lastRequestAt = figures?.lastRequestAt ?? null;
  const model = line === null ? (figures?.model ?? null) : line.model;
  const short = model === null ? null : shortModel(model);
  // A recipe names the account its request reached, and none where the request was refused before any account served
  // it, whatever pin stands beside it.
  const accountId = line === null ? (figures?.accountId ?? null) : recipe !== null ? recipe.accountId : (pin?.accountId ?? null);
  const type = agentId === MAIN_AGENT ? null : line === null ? (figures?.agentType ?? null) : line.agentType;
  const since = (iso: string) => `${fmtAbsolute(iso)}, ${fmtRelative(iso, now)}`;
  const title = joinPresent([lastRequestAt === null ? null : `Last request ${since(lastRequestAt)}.`, pin?.createdAt ? `Pinned since ${since(pin.createdAt)}.` : null], ' ');
  const tokens = pin?.lastInputTokens ?? null;
  const context = contextCell(tokens);
  const burn = burnReading(pin, cost);
  // Main reads unreported on a session no mod reports, as its row does, whatever word the engine holds for it but the
  // release a session's end leaves.
  const word = line === null ? null : lineWordOf(session, line);
  const park = word === 'parked' && recipe !== null ? parkDetail(recipe, now).replace(/^ · /, '') : '';
  const hint = LIFECYCLE_HINT[word ?? 'unreported'];
  const cacheTitle =
    pin === null
      ? undefined
      : joinPresent([PIN_CACHE_HINT[pin.warm ? 'warm' : 'cold'], recipe === null ? null : `${STATE_HINT[recipe.state]} ${CAUSE_HINT[recipe.cause]}`, recipe?.nextProbeAt ? `Next probe at ${fmtAbsolute(recipe.nextProbeAt)}.` : null], ' ');
  return (
    <tr>
      <td className="mono" title={title === '' ? undefined : title}>
        {hasTraffic ? <Link to={requestsHref(session.sessionId, session, lastRequestAt, agentId)}>{agentId}</Link> : agentId}
      </td>
      <td>{type ?? <span className="muted">-</span>}</td>
      <td title={model === null || short === null ? undefined : modelTitle(model, short)}>
        {short === null ? <span className="muted">-</span> : <span className="mono nowrap">{short}</span>}
        <span className="sub">{pin?.effort ?? '-'}</span>
      </td>
      <td>{accountId === null ? <span className="muted">none</span> : <LineAccount account={accountById.get(accountId) ?? null} id={accountId} now={now} row={accountId === session.accountId ? session : null} />}</td>
      <td title={tokens ? `${fmtInt(tokens)} tokens` : undefined}>
        <span className={context === '-' ? 'muted' : 'num'}>{context}</span>
      </td>
      <td title={burn?.title}>{burn === null ? <span className="muted">-</span> : <span className="num">{burn.text}</span>}</td>
      <td className="right num">{figures !== null && figures.requests > 0 ? fmtInt(figures.requests) : <span className="muted">0</span>}</td>
      <CostCell accountId={accountId} cost={cost} spend={figures?.spend ?? null} />
      <td>
        {line === null ? (
          <span className="muted">no pin</span>
        ) : word === null || word === 'unreported' ? (
          <span className="muted" title={hint}>
            unreported
          </span>
        ) : (
          <Badge tone={STATUS_TONE[word]} dot title={park === '' ? hint : `${hint}\n${park}`}>
            {word}
          </Badge>
        )}
      </td>
      <td>
        {line === null ? (
          <span className="muted">-</span>
        ) : pin === null ? (
          <span className="muted">no pin</span>
        ) : (
          <>
            <Badge tone={pin.warm ? 'good' : 'neutral'} dot title={cacheTitle}>
              {pin.warm ? 'warm' : 'cold'}
            </Badge>
            {recipe !== null && <span className="sub">{STATE_LABEL[recipe.state]}</span>}
          </>
        )}
      </td>
      <td onClick={(e) => e.stopPropagation()} onKeyDown={(e) => e.stopPropagation()}>
        <Menu
          label={`Actions for agent ${agentId}`}
          items={[
            { label: 'View in analytics', disabled: !hasTraffic, title: hasTraffic ? undefined : NO_TRAFFIC_HINT, onClick: () => onOpen(analyticsHref(session.sessionId, session, agentId)) },
            { label: 'View in requests', disabled: !hasTraffic, title: hasTraffic ? undefined : NO_TRAFFIC_HINT, onClick: () => onOpen(requestsHref(session.sessionId, session, lastRequestAt, agentId)) },
            // An hour either side of the agent's last request, read when the item is chosen.
            { label: 'View in routing log', onClick: () => onOpen(logHref({ agentId, session: session.sessionId, ...aroundSubject(lastRequestAt === null ? null : Date.parse(lastRequestAt), now) })) },
          ]}
        />
      </td>
    </tr>
  );
}

/**
 * "+2" beside a row's figure, hovering into `lead` and one line per agent that sits on another model or account, in the
 * accent tone where one of those agents matches the filter standing on its column.
 */
function OtherPins({ accent, agents, lead }: { accent: boolean; agents: { accountId: string; agentId: string; model: string }[]; lead: string }) {
  if (agents.length === 0) return null;
  return (
    <Badge tone={accent ? 'accent' : 'neutral'} title={`${lead}\n${agents.map((a) => `${a.agentId}: ${a.model} on ${a.accountId}`).join('\n')}`}>
      +{agents.length}
    </Badge>
  );
}
