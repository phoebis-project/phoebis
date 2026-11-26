import { Fragment, type ReactNode, useId, useMemo, useState } from 'react';
import { Link, useNavigate } from 'react-router-dom';
import { api } from '@/api';
import { iso } from '@/api/decode';
import type { Account, ApiKey, BurnFigures, CompanionState, KeepaliveRecipe, ModelTiers, Pool, RecipeState, Session, SessionAgentFacts, SessionSpend } from '@/api/types';
import { LIFECYCLE_VALUES } from '@/api/types';
import { useAppSettings } from '@/app/settings';
import { FilterBar } from '@/components/FilterBar';
import { GroupBy } from '@/components/GroupBy';
import { SortControl } from '@/components/SortControl';
import { ColumnsControl } from '@/components/ui/ColumnsControl';
import { CostReferenceLink } from '@/components/ui/CostReference';
import type { Option } from '@/components/ui/OptionList';
import { SpeedChip } from '@/components/ui/SpeedChip';
import { IconChevron } from '@/components/ui/icons';
import { PopoverHost, useHoldHost } from '@/components/ui/Popover';
import { Badge, Button, Card, EmptyState, ErrorState, KeyValue, Menu, ReadNotice, Skeleton, toggleRow, useHostedDescription } from '@/components/ui/primitives';
import { accountHref, ACTIVE_WITHIN, ANALYTICS_CODEC, keyHref, REQUESTS_CODEC, requestsHref as requestsAddressHref, SESSION_SORT_KEYS, SESSIONS_CODEC, type SessionGroupBy, type SessionsAddress, sortDirOf, type SortDir } from '@/lib/address';
import { UNPRICED_TITLE, sentences, type WeekReading } from '@/lib/analytics';
import { AGENT_TYPES, clientVersion } from '@/lib/client';
import { clock } from '@/lib/clock';
import { tierOrder } from '@/lib/costs';
import { lookup } from '@/lib/counts';
import { keyOptions, SESSIONS_DIMENSIONS } from '@/lib/filterBar';
import { SESSION_CHARS, fmtAbsolute, fmtDuration, fmtInt, fmtRelative, fmtShare, fmtUsd, joinPresent, keyLifetime, modelTitle, plural, shortId, shortModel, weekShort } from '@/lib/format';
import { agentCount, type AgentFigures, agentFigures, agentFiltersStand, agentLabel, CAUSE_HINT, CAUSE_LABEL, contextCell, type EarlierAgent, earlierAgents, expiryOf, figuresOf, groupSessions, hiddenLines, isFinished, lastActivity, lifecycleSplit, lineAccountOf, lineTotals, lineWordOf, type ListingLimits, MAIN_AGENT, matches, matchingAgents, openMark, orderLines, orderSessionRows, pinTtlMs, presenceOf, projectOf, recipeFiler, repositoryOf, requestCount, requestsSpan, sessionLabel, sessionLines, type SessionLine, shortPlace, STATUS_TONE, statusOf, type StatusWord, WARNING_CAUSES } from '@/lib/keepalive';
import { failedFirst, toggled, useAsync, useNow, useQueryFilters, useWeekReading } from '@/lib/hooks';
import { minuteSpan, type BadgeTone } from '@/lib/quota';
import { costCellOf, rateUnder, spendHint, spendHover, spendUnitOf } from '@/lib/requests';
import { ageWord, aroundSubject, logHref, rangeOfWindow, shedOrigin, triggerPhrase } from '@/lib/routingLog';
import { memberSpeed } from '@/lib/selection';
import type { UnitPosition } from '@/lib/unit';
import { windowHint } from '@/lib/windows';

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
  { id: 'project', label: 'Project' },
  { id: 'repository', label: 'Repository' },
  { id: 'runway', label: 'Runway' },
] as const;
type SessionColumnId = (typeof SESSION_COLUMNS)[number]['id'];
// The default set fits the card at 1280 by 800 without these: the Status cell carries the presence, the expanded row
// carries the pin's figures, the key, the probes, the project and the repository, the label reads the project's folder
// where no title stands, and the Columns control brings each back.
const HIDDEN_BY_DEFAULT: readonly SessionColumnId[] = ['companion', 'context', 'created', 'effort', 'key', 'lastRequest', 'nextProbe', 'project', 'repository', 'runway'];
// None leads as the default, and the two keys follow in name order.
const GROUP_OPTIONS: { label: string; value: SessionGroupBy }[] = [
  { label: 'None', value: 'none' },
  { label: 'Project', value: 'project' },
  { label: 'Repository', value: 'repository' },
];

/** The bar's count: the kept sessions and their agents, the matching ones while an Agents filter stands. */
const countText = (sessions: number, agents: number, matching: boolean) => `${plural(sessions, 'session')} · ${plural(agents, matching ? 'matching agent' : 'agent')}`;

/**
 * The pin's runway on its account as the route answers it: a span, `none` at zero, `unbounded` where no window binds.
 * Null for a row with no pin or no projection, which the cell draws as a dash.
 */
const runwayWord = (s: Session): string | null => (!s.pinned ? null : s.runwayMs !== null ? (s.runwayMs === 0 ? 'none' : minuteSpan(s.runwayMs)) : s.runwayBounded === false ? 'unbounded' : null);

/** A row's or a line's Status word with its sentence. */
const LIFECYCLE_HINT: Record<StatusWord, string> = {
  busy: 'Between model turns, running tools or streaming. Probed like any other agent, and the idle window of the main conversation keeps running.',
  finished: 'The mod released every agent of this session. Each record leaves the engine one cache TTL after its release, and a record that held a slot of the agent cap keeps it until then.',
  idle: "Stopped and waiting for the user. Probed until the pool's idle window runs out, the break-even where the pool sets none.",
  parked: 'Stopped while a background command, a subagent or a scheduled wakeup is open. Probed under the parked and horizon rules and the idle window.',
  registered: 'The mod registered this agent and no real request has reached the router from it yet',
  released: 'The mod released this agent, a subagent that finished or the main conversation. Its recipe is gone.',
  unknown: "The reporter could not tell this agent's state.",
  unreported: 'No companion mod reports this agent, so the router holds no lifecycle word for it.',
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
  none: 'No companion mod has registered this session. The router still pins it and still sees its requests.',
  reporting: "The companion mod reports this session's agents, their parks and the person's presence.",
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
const since = (at: string, now: number) => `${fmtAbsolute(at)}, ${fmtRelative(at, now)}`;
const PLACEMENTS_HINT = "How many times the router placed or moved this session's main conversation";
const NO_TRAFFIC_HINT = 'No ledgered request yet';
const NOT_REPORTED = <span className="muted">not reported</span>;
const REQUESTS_HINT = "Requests over the session's life";
interface Row {
  agentCount: number;
  /**
   * Every recipe filed under the session, as the route answered them: the no-recipe causes and the Cache cell read this
   * list.
   */
  agents: KeepaliveRecipe[];
  /** When the route stops listing the session, from `expiryOf`. */
  expiresAt: number | null;
  /** The engine is done with the session, which draws every line it holds under the released sentence. */
  finished: boolean;
  /**
   * Set where the idle limit ended every recipe, with the instant it ran out where the pool's window gives one. Its
   * phrase reads the clock, so `noRecipeText` writes it at draw time and a tick rebuilds no row.
   */
  idleLimit: { ranOutAt: string | null } | null;
  /** The session's lines in the view's order, the released ones among them, which `hiddenLines` draws or hides. */
  lines: SessionLine[];
  /** Why a session carries no recipe at all, the causes that need no clock, null while it has one. */
  noRecipe: string[] | null;
  /** The pool of the session's key, null where the key list does not hold the key or the pools did not load. */
  pool: Pool | null;
  /** The pool of the session's key, null where the key list does not hold the key. */
  poolId: string | null;
  /** The state the route's summary reads, null where no recipe keeps the session's prompt. */
  recipe: KeepaliveRecipe | null;
  session: Session;
  split: ReturnType<typeof lifecycleSplit>;
  status: ReturnType<typeof statusOf>;
}

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
/** The agent a link narrows the session to, with the first and the last request the request log's range reads off. */
type LinkAgent = { agentId: string; firstRequestAt: string | null; lastRequestAt: string | null };
/**
 * The request log on the session the client sent under the row's key lifetime, since a key id created again can hold
 * the same session id, over the range `requestsSpan` reads off the first and the last request of the session or of the
 * agent the link is for, read on the clock when the link is made (the menu's click, a line's draw): the page's `now` is
 * a sample up to five seconds old, and the request log measures its window from its own load. An agent without a first
 * or a last request of its own reads the session's. The session lands as the log's session chip, which matches the id
 * exactly, and an agent line's link adds its agent's.
 */
const requestsHref = (session: Pick<Session, 'firstRequestAt' | 'keyGeneration' | 'keyId' | 'lastRequestAt' | 'sessionId'>, agent?: LinkAgent) => {
  const defaults = REQUESTS_CODEC.defaults;
  const span = requestsSpan(agent?.firstRequestAt ?? session.firstRequestAt, agent?.lastRequestAt ?? session.lastRequestAt, clock.now());
  const range = span === null ? defaults.range : rangeOfWindow('preset' in span ? { range: span.preset } : span);
  return requestsAddressHref({ ...defaults, agentId: agent?.agentId ?? defaults.agentId, keys: [keyLifetime(session.keyId, session.keyGeneration)], range, session: session.sessionId });
};
/** The analytics on the session the client sent under the row's key lifetime, an agent line's narrowed to its agent. */
const analyticsHref = (session: Pick<Session, 'keyGeneration' | 'keyId' | 'sessionId'>, agentId?: string) =>
  `/analytics?${ANALYTICS_CODEC.toParams({ ...ANALYTICS_CODEC.defaults, agentId: agentId ?? ANALYTICS_CODEC.defaults.agentId, key: keyLifetime(session.keyId, session.keyGeneration), session: session.sessionId })}`;

/**
 * The menus' Analytics and request log items, the session's or an agent's, each address built when its item is chosen
 * over the first and the last request of the session or of the agent handed in. Both views read the ledger, so a
 * session that never reached it has nothing to open. The count leaves a refused request out while the log lists it, so
 * a ledgered session reads off its last request instead.
 */
const ledgerItems = (session: Session, navigate: (path: string) => void, agent?: LinkAgent) => {
  const hasTraffic = session.lastRequestAt !== null;
  const title = hasTraffic ? undefined : NO_TRAFFIC_HINT;
  return [
    { label: 'View in analytics', disabled: !hasTraffic, title, onClick: () => navigate(analyticsHref(session, agent?.agentId)) },
    { label: 'View in requests', disabled: !hasTraffic, title, onClick: () => navigate(requestsHref(session, agent)) },
  ];
};

/**
 * When the pool stopped probing: the longest idle window the router resolves for the pool's recipes, counted from the
 * pin's last use, a probe's among them, so the time never reads earlier than the window could have run.
 */
function idleLimitRanOutAt(session: Session, pool: Pool | null): string | null {
  const seconds = pool?.keepaliveResolved?.idleSeconds.max ?? 0;
  return seconds > 0 && session.lastUsedAt !== null ? iso(Date.parse(session.lastUsedAt) + seconds * 1000) : null;
}

/**
 * `poolsFailed`: the pool list did not load, so a session whose pool is not found has no cause the rows can name but
 * that. `sort` and `dir`: the sort control's key and direction, which order the rows and each row's lines.
 */
function buildRows(sessions: Session[], recipes: KeepaliveRecipe[], keys: ApiKey[], pools: Pool[], poolsFailed: boolean, sort: SessionsAddress['sort'], dir: SortDir, limits: ListingLimits | null): Row[] {
  const keyById = new Map(keys.map((k) => [k.id, k]));
  const poolById = new Map(pools.map((p) => [p.id, p]));
  const recipesOf = recipeFiler(recipes);
  const rows = sessions.map((session) => {
    const agents = recipesOf(session);
    const key = keyById.get(session.keyId) ?? null;
    const pool = key ? poolById.get(key.poolId) ?? null : null;
    // Every agent ended at the idle limit means the retained prompts are already
    // gone, so the session reads as having no recipe even though the rows remain.
    const idleLimitOnly = agents.length > 0 && agents.every((a) => a.cause === 'idle_limit');
    const causes: string[] = [];
    let idleLimit: Row['idleLimit'] = null;
    // The live key's pool and permissions say nothing about a session that no longer belongs to it, so an earlier
    // holder carries that reason alone.
    if (session.historical) causes.push(HISTORICAL_CAUSE);
    else if (poolsFailed && key !== null && pool === null) causes.push('the pool list did not load');
    else {
      if (pool && !pool.keepalive.enabled) causes.push(`pool ${pool.name} keeps no caches warm`);
      if (key && !key.companionPermissions.includes('keepalive')) causes.push(`key ${key.name} permits no cache warming`);
      if (idleLimitOnly) idleLimit = { ranOutAt: idleLimitRanOutAt(session, pool) };
      else if (causes.length === 0) causes.push('the last request left no cacheable prefix to replay');
    }
    // The full list stays beside the lines: the causes of a session without a line read every record, and a session
    // whose lines the mod all released still holds records, so it is no session without a recipe.
    const lines = sessionLines(session, agents);
    const finished = isFinished(lines);
    return {
      agentCount: agentCount(lines),
      agents,
      expiresAt: expiryOf(session, limits),
      finished,
      idleLimit,
      lines: orderLines(lines, sort, dir),
      noRecipe: agents.length === 0 || idleLimitOnly ? causes : null,
      pool,
      poolId: key?.poolId ?? null,
      recipe: agents.length === 0 || idleLimitOnly ? null : rowRecipe(agents),
      session,
      split: lifecycleSplit(lines),
      status: statusOf(session, finished),
    };
  });
  return orderSessionRows(rows, sort, dir);
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
    companion: words(Object.keys(COMPANION_HINT)),
    effort: words(held(lines.map((l) => l.pin?.effort ?? null)).sort()),
    key: keyOptions(keys, held(rows.map((r) => (r.session.historical ? keyLifetime(r.session.keyId, r.session.keyGeneration) : null)))),
    model: words(tierOrder(held(lines.map((l) => l.model)), modelTiers)),
    pool: pools.map((p) => ({ label: p.name, value: p.id })),
    status: words([...LIFECYCLE_VALUES, 'unreported'].sort()),
  };
}

/** The kept rows' figures the card's summary line reads, each agent counted as the Agents cell counts it. */
function daySummary(rows: readonly Row[]): { agents: number; billed: number; busy: number; equivalent: number | null; idle: number; parked: number; requests: number } {
  const sum = (values: number[]) => values.reduce((n, v) => n + v, 0);
  return {
    ...lineTotals(rows),
    billed: sum(rows.flatMap((r) => Object.values(r.session.day.billedUsd))),
    // A day the cost model priced nothing for leaves the sum unknown, as a null figure leaves a spend's, so the line
    // never passes a lower figure off as the whole.
    equivalent: rows.reduce<number | null>((total, r) => (total === null || r.session.day.equivalent === null ? null : total + r.session.day.equivalent), 0),
    requests: sum(rows.flatMap((r) => Object.values(r.session.day.requests))),
  };
}

/**
 * The recipe the row's Cache cell reads: main's where one stands, else the one with the newest last request, a recipe
 * without one last and ties by agent id.
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
  return [a.parkKind, ...times].join(' · ');
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
interface CostContext extends WeekReading {
  accountById: ReadonlyMap<string, Account>;
  accounts: Account[] | null;
  unit: UnitPosition;
}

export function SessionsPage() {
  const [intervalMs, setIntervalMs] = useState(5_000);
  // Every dialog and the bar's and the rows' menus hold the reads through the page's host, since an answer landing
  // under a row's open menu could add or drop rows beneath it.
  const { cause: pauseCause, host } = useHoldHost();
  const reread = { paused: pauseCause !== null, refreshMs: intervalMs };
  // The view lists the rows the route answers, so a row leaves when the route's next answer drops it, and the list is
  // read again on the interval. The accounts and the recipes read on the same interval, so the slow-down chips and the
  // lines move with the rows.
  const accounts = useAsync(() => api.listAccounts(), [], reread);
  const keys = useAsync(() => api.listKeys(), []);
  const pools = useAsync(() => api.listPools(), []);
  const recipes = useAsync(() => api.listKeepaliveRecipes(), [], reread);
  const sessions = useAsync(() => api.listSessions(), [], reread);
  const { modelTiers, unit, week } = useAppSettings();
  // The listing limits, the non-companion timeout and the session expiry, and the cost model whose capacities scale a
  // spend to the chosen week.
  const { reading: weekReading, settings } = useWeekReading(week);
  const now = useNow(5_000);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  // The open rows whose fold line was opened, by row, so each row unfolds the agents the filters hide on its own.
  const [unfolded, setUnfolded] = useState<ReadonlySet<string>>(() => new Set());
  const [visible, setVisible] = useState<Set<SessionColumnId>>(() => new Set(SESSION_COLUMNS.map((c) => c.id).filter((id) => !HIDDEN_BY_DEFAULT.includes(id))));
  // The base row the last fork link led to, marked until another fork link is followed.
  const [current, setCurrent] = useState<string | null>(null);
  const [howOpen, setHowOpen] = useState(false);
  const howId = useId();
  const { address, filters, update } = useQueryFilters(SESSIONS_CODEC);
  const dir = sortDirOf(SESSION_SORT_KEYS, filters);

  const sessionData = sessions.data;
  const recipeData = recipes.data;
  const limits = settings.data?.sessions ?? null;
  const poolsFailed = failedFirst(pools);
  // Built without the clock, so a tick only filters the rows again.
  const listed = useMemo(
    () => (sessionData && recipeData && keys.data ? buildRows(sessionData, recipeData, keys.data, pools.data ?? [], poolsFailed, filters.sort, dir, limits) : null),
    [sessionData, recipeData, keys.data, pools.data, poolsFailed, filters.sort, dir, limits],
  );
  // The route leaves out a session once it stops listing it, so the filters narrow the rows it answered.
  const rows = useMemo(() => listed?.filter((r) => matches(r, filters, now)) ?? null, [listed, filters, now]);
  const groups = useMemo(() => (rows === null || filters.by === 'none' ? null : groupSessions(rows, filters.by)), [rows, filters.by]);
  // The lines each kept row draws under the filters and how many the filters fold away, which its detail, its Next
  // probe cell and the bar's count read.
  const drawn = useMemo(() => new Map((rows ?? []).map((r) => [rowKey(r.session), hiddenLines(r, filters)])), [rows, filters]);
  const options = useMemo(() => (listed === null ? {} : filterOptions(listed, keys.data, pools.data ?? [], accounts.data ?? [], modelTiers)), [listed, keys.data, pools.data, accounts.data, modelTiers]);
  const hiddenRows = listed !== null && rows !== null ? listed.length - rows.length : 0;
  const hiddenTitle = hiddenRows > 0 ? `${plural(hiddenRows, 'session')} hidden by the filters` : undefined;
  const agentsKept = useMemo(() => matchingAgents(drawn.values()), [drawn]);
  const matching = agentFiltersStand(filters);
  const countWords = rows === null ? '' : countText(rows.length, agentsKept, matching);
  // Three digits in each figure hold the count's slot at one width, so a count up to 999 in each figure moves no line
  // of the bar, the empty one while the rows load among them.
  const countSizer = countText(999, 999, matching);
  const summary = useMemo(() => (rows === null ? null : daySummary(rows)), [rows]);
  // The rows wait on these three reads, so the first that failed before it ever answered names the notice, and the card
  // draws its error state where that read is the sessions. A refresh that fails after a first answer keeps the rows.
  const reads = { keys, recipes, sessions };
  const firstFailed = (['sessions', 'recipes', 'keys'] as const).find((n) => failedFirst(reads[n])) ?? null;
  // The listed rows by element id, so a fork's badge finds its base's row and reads the account the base sits on.
  const listedRows = useMemo(() => new Map((rows ?? []).map((r) => [rowDomId(r.session), r])), [rows]);
  // Main's pin, a fork's base and each line read their account here rather than off the rows, so an accounts answer
  // redraws the slow-down chips and the labels without rebuilding a row.
  const accountById = useMemo(() => new Map((accounts.data ?? []).map((a) => [a.id, a])), [accounts.data]);
  const cost: CostContext = { accountById, accounts: accounts.data, unit, ...weekReading };
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
  const sessionRows = (list: readonly Row[]) =>
    list.map((row) => {
      const id = rowKey(row.session);
      const lines = drawn.get(id);
      return (
        <SessionRows
          key={id}
          row={row}
          open={expanded.has(id)}
          onToggle={() => toggle(id)}
          now={now}
          columns={columns}
          cost={cost}
          current={current === rowDomId(row.session)}
          filtered={{ account: filters.account.length > 0, model: filters.model.length > 0 }}
          fold={matching ? (lines?.hidden ?? 0) : null}
          lines={lines ?? { drawn: [], hidden: 0, shown: [] }}
          listedRows={listedRows}
          onFollowFork={followFork}
          onUnfold={() => unfold(id)}
          unfolded={unfolded.has(id)}
          visible={visible}
        />
      );
    });

  return (
    <PopoverHost.Provider value={host}>
      <div className="stack">
        <FilterBar
          address={address}
          count={hiddenTitle === undefined ? countWords : <span title={hiddenTitle}>{countWords}</span>}
          countSizer={countSizer}
          dimensions={SESSIONS_DIMENSIONS}
          filters={filters}
          options={options}
          placeholder="Session id, title, project, repository or branch"
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
          <GroupBy onChange={(by) => update({ by })} options={GROUP_OPTIONS} value={filters.by} />
          <SortControl keys={SESSION_SORT_KEYS} onChange={update} value={filters} />
        </FilterBar>
        {firstFailed !== null && <ReadNotice className="sessions__notice" error={reads[firstFailed].error} name={firstFailed} retry={reads[firstFailed].reload} />}
        {failedFirst(accounts) && <ReadNotice className="sessions__notice" error={accounts.error} name="accounts" retry={accounts.reload} />}
        <div className="page-head">
          <div className="page-head__lead">
            <p className="page-head__meta">
              Every session the router lists: what it runs, where it is pinned, what it has spent and whether its prompt cache is warm.{' '}
              <button type="button" className="link-btn" aria-expanded={howOpen} aria-controls={howId} onClick={() => setHowOpen((v) => !v)}>
                How pins and recipes work
              </button>
            </p>
            <p id={howId} className="page-head__meta" hidden={!howOpen}>
              Every Claude Code session the router has seen within the session expiry, whether or not a companion mod registered it. A pin binds each model a session uses to one
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
                {`${plural(summary.agents, 'agent')}: ${fmtInt(summary.busy)} busy, ${fmtInt(summary.idle)} idle, ${fmtInt(summary.parked)} parked · ${plural(summary.requests, 'request')} and `}
                {/* The figure is a Pro-week figure read in the chosen week, so it waits on the cost model's scale. */}
                {summary.equivalent === null ? <span title={UNPRICED_TITLE}>-</span> : weekReading.scale !== null ? weekShort(summary.equivalent * weekReading.scale, week) : weekReading.failure === null ? <Skeleton width={64} /> : <span title={weekReading.failure.text}>-</span>}
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
                  {/* The headers and the rows share one keyed list, so a row that changes group keeps its instance
                      and its open disclosures. A row's key leads with its key id, a slug, so none starts with the
                      `[` of a header's key. */}
                  {groups === null
                    ? sessionRows(rows)
                    : groups.flatMap((group) => [
                        <tr key={JSON.stringify(['group', group.key])} className="table__group">
                          <td colSpan={columns} title={group.key ?? undefined}>
                            <span className="table__group-label ellipsis">{group.short}</span>
                            <span className="muted table__group-count">{plural(group.rows.length, 'session')}</span>
                          </td>
                        </tr>,
                        ...sessionRows(group.rows),
                      ])}
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
                <EmptyState title="No live sessions" body="A session appears here on its first request, or as soon as the companion mod registers it, and leaves when the router stops listing it." />
              ) : (
                <EmptyState title="No sessions kept by the filters" body={`${hiddenTitle}.`} />
              )}
            </div>
          )}
        </Card>
      </div>
    </PopoverHost.Provider>
  );
}

/**
 * A pin's burn per hour under the Unit position, by the rate rule: a plan pin's share of the chosen week or its list
 * dollars, a billed pin's dollars, a dash with why where the position holds no figure for the pin's side, and the
 * share of the pin's own window as a plan figure's title. A plan pin's dollars read `burnUsd` whatever `burn` reads,
 * since the route answers a dollar figure beside a null `burn` for a model the cost model does not weigh, and they
 * draw with no title there. Null with no pin, a pin with no unit, no figure for the position, or under Native while
 * the week's scale is out.
 */
function burnReading(pin: BurnFigures | null, cost: CostContext): { dash: boolean; text: string; title: string | undefined } | null {
  if (pin === null || pin.burnUnit === null) return null;
  const billed = pin.burnUnit === 'usd';
  const reading = rateUnder(cost.unit, { equivalent: pin.burn === null ? null : pin.burnEquivalent, side: billed ? 'billed' : 'plan', usd: billed ? pin.burn : pin.burnUsd });
  if (reading === null) return null;
  if (reading.kind === 'dash') return { dash: true, text: '-', title: reading.title };
  const title = billed || pin.burn === null || pin.burnWindow === null ? undefined : `${fmtShare(pin.burn)} of ${windowHint(pin.burnWindow)}`;
  if (reading.kind === 'dollars') return { dash: false, text: `${fmtUsd(reading.usd)}/h`, title };
  return cost.scale === null ? null : { dash: false, text: `${weekShort(reading.figure * cost.scale, cost.week)}/h`, title };
}

/** The pin's burn per hour under the Cost figure, a dash with why in the muted ink, and none where `burnReading` reads
 *  none. */
function BurnLine({ cost, session }: { cost: CostContext; session: Session }) {
  const burn = burnReading(session.pinned ? session : null, cost);
  return burn === null ? null : (
    <span className={burn.dash ? 'sub muted' : 'sub'} title={burn.title}>
      {burn.text}
    </span>
  );
}

/**
 * The Cost cell: a spend over client requests in the chosen unit, the session's or an agent's, as the request log's
 * cell reads a request, then the dollars billed beside a plan figure and the accounts other than `accountId` the
 * requests spent on, by a plan share or by billed dollars, a billed one with its dollars, the figure in full with the
 * probes' share on the title, and `burn` under it. The skeleton while the scale is out, a dash titled with the week
 * reading's failure where it carries one, and a muted dash before any ledgered request.
 */
function CostCell({ accountId, burn = null, cost, spend }: { accountId: string | null; burn?: ReactNode; cost: CostContext; spend: SessionSpend | null }) {
  let title: string | undefined;
  let body: ReactNode;
  // The spend is null where the ledger holds no request of the session or the agent.
  if (spend === null) {
    title = NO_TRAFFIC_HINT;
    body = <span className="muted">-</span>;
  } else {
    const hint = spendHint(spend, cost.accounts);
    const reading = spendUnitOf(spend, cost.unit, accountId, hint);
    const cell = costCellOf(reading.unit, cost.week, cost.scale);
    if (cell === null) {
      title = cost.failure?.text;
      body = cost.failure === null ? <Skeleton width={72} /> : '-';
    } else {
      title = spendHover(spend, cost.week, cost.scale, hint);
      const accountLines = reading.accounts.map((id) => {
        const label = cost.accountById.get(id)?.label ?? id;
        const billed = lookup(spend.billedUsdByAccount, id);
        return billed === undefined ? label : `${label}, ${fmtUsd(billed)} billed in dollars`;
      });
      body = (
        <>
          <span className="srow__pin">
            {cell.main}
            <BilledBadge usd={reading.billed} />
            {reading.accounts.length > 0 && (
              <Badge tone="neutral" title={`Other accounts the requests spent on:\n${accountLines.join('\n')}`}>
                +{reading.accounts.length}
              </Badge>
            )}
          </span>
          {cell.sub && <span className="sub">{cell.sub}</span>}
        </>
      );
    }
  }
  return (
    <td className="right num col-fit" title={title}>
      {body}
      {burn}
    </td>
  );
}

function BilledBadge({ usd }: { usd: number }) {
  if (usd <= 0) return null;
  return (
    <Badge tone="neutral" title={`${fmtUsd(usd)} billed in dollars`}>
      +$
    </Badge>
  );
}

const requestsFigure = (n: number): ReactNode => (n > 0 ? fmtInt(n) : <span className="muted">0</span>);

/** An account's link to its page with the kind on its title, described to a reader that never hovers. */
function AccountLink({ account, id }: { account: Account | null; id: string }) {
  const title = account ? KIND_HINT[account.kind] : undefined;
  const kind = useHostedDescription(title);
  return (
    <Link to={accountHref(id)} title={title} aria-describedby={kind}>
      {id}
    </Link>
  );
}

/**
 * The row's account: a link to its page with the kind on its title, described to a reader that never hovers, the
 * slow-down chip on a slowed pin, the shed mark, the account-bound badge, and the agents pinned on another account. A
 * dash with no account.
 */
function RowAccount({ accent, account, now, others, session }: { accent: boolean; account: Account | null; now: number; others: OtherPin[]; session: Session }) {
  if (session.accountId === null) return <span className="muted">-</span>;
  return (
    <LineAccount account={account} bound={session.accountBound} className="srow__pin" id={session.accountId} now={now} row={session}>
      <OtherPins accent={accent} agents={others} lead="Agents on another account:" />
    </LineAccount>
  );
}

/**
 * The warning badge `leaving` while a shedding round's mark stands on the session's pin, the mark's account, window and
 * instant on hover.
 */
function ShedBadge({ session }: { session: Pick<Session, 'accountId' | 'mark'> }) {
  const mark = openMark(session);
  if (mark === null) return null;
  return (
    <Badge tone="warning" title={`Marked to leave ${mark.on} ${triggerPhrase(mark.window)} at ${fmtAbsolute(mark.at)}. The next request moves the session`}>
      leaving
    </Badge>
  );
}

const ACCOUNT_BOUND_HINT =
  "This session's requests name state that its account holds, such as a file, a container or a skill. The router keeps its pin on that account, and no rule or round moves it.";

/** The badge of a pin that holds state on its account, and nothing for any other pin. */
function BoundBadge({ bound }: { bound: boolean | null }) {
  return bound === true ? (
    <Badge tone="neutral" title={ACCOUNT_BOUND_HINT}>
      account-bound
    </Badge>
  ) : null;
}

/** The pin's no-return stamp as the move that wrote it, in the Pin fold's words, or null with no stamp. */
const movedWord = (session: Pick<Session, 'shed'>): string | null => {
  if (session.shed === null) return null;
  return `${shedOrigin(session.shed.reason)} ${session.shed.from} at ${fmtAbsolute(session.shed.at)}`;
};

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
interface Fact {
  label: string;
  value: ReactNode;
}

/** A grid of an open row's facts under its title, with the title's link beside it. */
function Facts({ link, rows, title }: { link?: ReactNode; rows: Fact[]; title: string }) {
  return (
    <div className="facts__group">
      <div className="facts__head">
        <span className="facts__title">{title}</span>
        {link}
      </div>
      <KeyValue className="facts" rows={rows} />
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
  row,
  open,
  onToggle,
  now,
  columns,
  cost,
  current,
  filtered,
  fold,
  lines: { drawn: drawnLines, shown: shownLines },
  listedRows,
  onFollowFork,
  onUnfold,
  unfolded,
  visible,
}: {
  columns: number;
  cost: CostContext;
  /** Whether a fork link last led here. */
  current: boolean;
  /** Whether the Account and the Model filters stand, which accent the "+N" of their column. */
  filtered: { account: boolean; model: boolean };
  /** How many agents the Agents filters fold out of the open row, null while none stands. */
  fold: number | null;
  /** `hiddenLines`' answer under the filters: the lines the row draws and the ones it lists. */
  lines: ReturnType<typeof hiddenLines>;
  listedRows: ReadonlyMap<string, Row>;
  now: number;
  onFollowFork: (id: string) => void;
  onToggle: () => void;
  onUnfold: () => void;
  open: boolean;
  row: Row;
  /** Whether the fold line is open, which draws the agents it counts under the matching lines. */
  unfolded: boolean;
  visible: ReadonlySet<SessionColumnId>;
}) {
  const { expiresAt, recipe, session, status } = row;
  const account = cost.accountById.get(session.accountId ?? '') ?? null;
  const noRecipe = noRecipeText(row, now);
  const recipes = shownLines.flatMap((l) => (l.recipe === null ? [] : [l.recipe]));
  // A line takes its account from its pin, and from its recipe only where no pin stands, as the Agents table's cell
  // and the Account filter read it, and a line that names none is left out. A "+N" counts every other line the row
  // draws on another model or account, recipe or not, whatever the Agents filters match, and its hover reads them in
  // the order the expanded row draws them. It takes the accent where its column's filter stands and one of its lines
  // matches the filters, a line `shownLines` holds.
  const placed = drawnLines.flatMap((l) => {
    const accountId = lineAccountOf(l);
    return accountId !== null && l.model !== null ? [{ accountId, label: agentLabel(l.agentId, l.facts) ?? l.agentId, matched: shownLines.includes(l), model: l.model }] : [];
  });
  const otherModels = placed.filter((l) => l.model !== session.model);
  const otherAccounts = placed.filter((l) => l.accountId !== session.accountId);
  // Opened, the fold line draws the lines the filters hide and then the earlier agents, the agents it counts.
  const hiddenHere = open && fold !== null ? drawnLines.filter((l) => !shownLines.includes(l)) : [];
  // A retained agent with no time is waiting on its first stream event, which is
  // a different thing from a session nothing probes.
  const pendingProbe = recipes.some((a) => a.state === 'retained' && a.nextProbeAt === null);
  // The lines' request log links open under the check the menu's ledger items read.
  const hasTraffic = session.lastRequestAt !== null;
  const activity = lastActivity(session);
  const routingLogTo = () => logHref({ session: session.sessionId, ...aroundSubject(activity === null ? null : Date.parse(activity), now) });
  const forkBase = session.forkedFrom === null ? null : rowDomId({ ...session, sessionId: session.forkedFrom });
  // The base's row where the list holds it: the fork badge reads the account relation off the two listed rows, and
  // only while both pins are live, since a released or expired pin leaves the last ledgered request's account on the
  // row.
  const baseRow = forkBase === null ? null : (listedRows.get(forkBase) ?? null);
  const baseAccountId = baseRow !== null && baseRow.session.pinned ? baseRow.session.accountId : null;
  // The row's figures are main's conversation pin's, so a row with no pin reads a dash.
  const context = contextCell(session.pinned ? session.lastInputTokens : null);
  const label = sessionLabel(session);
  const shortSession = shortId(session.sessionId, SESSION_CHARS);
  const { facts } = session;
  const folder = projectOf(facts);
  const place = repositoryOf(facts);
  const cwdApart = facts.cwd === facts.project ? null : facts.cwd;
  const client = joinPresent([clientVersion(session.userAgent), session.placements > 1 ? `${session.placements} placements` : null], ' · ');
  const clientTitle = sentences([session.userAgent, session.placements > 1 ? PLACEMENTS_HINT : null]);
  const mainRecipe = row.agents.find((a) => a.agentId === MAIN_AGENT) ?? null;
  const park = status.word === 'parked' && mainRecipe !== null ? parkDetail(mainRecipe, now) : '';
  const statusTitle = joinPresent([LIFECYCLE_HINT[status.word], park], '\n');
  const presence = presenceOf(session, now);
  const split = Object.entries(row.split).flatMap(([word, n]) => (n > 0 ? [`${fmtInt(n)} ${word}`] : []));
  const relation = forkRelation(baseAccountId, session.pinned ? session.accountId : null);
  // The earlier agents are the ones no line names, a released line hidden by the filters among the named.
  const earlier = open ? earlierAgents(session, row.lines) : [];
  const [earlierOpen, setEarlierOpen] = useState(false);
  const [warmingOpen, setWarmingOpen] = useState(false);
  const tableId = useId();
  const warmingId = useId();
  const navigate = useNavigate();
  // The route answers the row's own instant as its display pin's, main's newest where main holds one, so it stays put
  // when main holds pins on two models and the lines change order with the sort.
  const pinnedSince = open ? session.pinnedAt : null;
  const mark = open ? openMark(session) : null;
  const moved = open ? movedWord(session) : null;
  const speed = memberSpeed(session, account, now);
  const agentRow = (agentId: string, line: SessionLine | null, figures: AgentFigures | null, facts: SessionAgentFacts | null) => (
    <AgentRow
      key={line === null ? `earlier|${agentId}` : `${agentId}|${line.model}`}
      agentId={agentId}
      cost={cost}
      facts={facts}
      figures={figures}
      line={line}
      now={now}
      session={session}
    />
  );
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
        <td className="col-fill" title={joinPresent([label, session.sessionId], '\n')}>
          {label !== null && <span className="srow__label ellipsis">{label}</span>}
          <span className="mono srow__id ellipsis">{session.sessionId}</span>
          {(forkBase !== null || client !== '') && (
            <span className="sub" title={clientTitle === '' ? undefined : clientTitle}>
              {forkBase !== null && (
                <ForkBadge
                  base={baseRow ? forkBase : null}
                  baseLabel={baseAccountId === null ? null : (cost.accountById.get(baseAccountId)?.label ?? baseAccountId)}
                  depth={session.forkDepth}
                  from={session.forkedFrom ?? ''}
                  onFollow={onFollowFork}
                  relation={relation}
                />
              )}
              {forkBase !== null && client !== '' && ' · '}
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
            {fmtInt(row.agentCount)}
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
          <td className="mono nowrap col-fit" title={session.model === null ? undefined : modelTitle(session.model)}>
            <span className="srow__pin">
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
            {requestsFigure(requestCount(session))}
            {session.lastRequestAt !== null && <span className="sub">{fmtRelative(session.lastRequestAt, now)}</span>}
          </td>
        )}
        {visible.has('cost') && <CostCell accountId={session.accountId} burn={<BurnLine cost={cost} session={session} />} cost={cost} spend={session.spend} />}
        {/* Expiry rides under the last-used time rather than in a column of its own, which is 90px the 1600px window did not have. */}
        {visible.has('lastUsed') && (
          <td className="col-fit" title={fmtAbsolute(activity)}>
            {fmtRelative(activity, now)}
            {expiresAt !== null && (
              <span className="sub" title={`The router stops listing this session at ${fmtAbsolute(expiresAt)}`}>
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
                <CacheBadge title={cacheTitle(session.warm, recipe, NO_RECIPE_HINT)} warm={session.warm} />
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
          <td className="col-fit" title={session.firstRequestAt === null ? undefined : fmtAbsolute(session.firstRequestAt)}>
            {session.firstRequestAt === null ? <span className="muted">-</span> : fmtRelative(session.firstRequestAt, now)}
          </td>
        )}
        {visible.has('effort') && <td className="col-fit">{session.effort ?? <span className="muted">-</span>}</td>}
        {/* No route lists one key and the Keys view's search matches no id, so the link lands on the key's row. */}
        {visible.has('key') && (
          <td className="mono col-fit">
            <Link to={keyHref(session.keyId)}>{session.keyId}</Link>
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
        {visible.has('project') && (
          <td className="col-fit" title={joinPresent([facts.project, cwdApart], '\n') || undefined}>
            {folder === null ? <span className="muted">-</span> : <span className="srow__place ellipsis">{shortPlace(folder)}</span>}
          </td>
        )}
        {visible.has('repository') && (
          <td className="col-fit" title={joinPresent([facts.repository, facts.branch, facts.repositoryRoot], '\n') || undefined}>
            {place === null ? <span className="muted">-</span> : <span className="srow__place ellipsis">{shortPlace(place)}</span>}
            {facts.branch !== null && <span className="sub srow__place ellipsis">{facts.branch}</span>}
          </td>
        )}
        {visible.has('runway') && (
          <td className="col-fit" title={session.pinned && session.cycleMs !== null ? `cycle ${ageWord(session.cycleMs)}` : undefined}>
            {runwayWord(session) ?? <span className="muted">-</span>}
          </td>
        )}
        {/* The actions sit in a menu, a fraction of spelled-out buttons' width, and close the row: the session's label
            leads it, so a reader scans the labels down the first column, and the actions stay reachable at the row's
            end at 1280 px. */}
        <td className="col-fit" onClick={(e) => e.stopPropagation()} onKeyDown={(e) => e.stopPropagation()}>
          <Menu
            label={`Actions for session ${label === null ? shortSession : `${label} (${shortSession})`}${session.historical ? ', deleted holder' : ''}`}
            items={[
              ...ledgerItems(session, navigate),
              // The routing log reads the decisions, which exist once a pin does, so it needs no ledgered request and
              // opens an hour either side of the session's last activity.
              { label: 'View in routing log', disabled: !session.sessionId, onClick: () => navigate(routingLogTo()) },
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
                    { label: 'Created', value: session.firstRequestAt === null ? <span className="muted">unknown</span> : since(session.firstRequestAt, now) },
                    {
                      label: 'Key',
                      value: session.historical ? (
                        <>
                          <span className="mono">{session.keyId}</span>,{' '}
                          <span className="muted" title={HISTORICAL_HINT}>
                            deleted holder
                          </span>
                        </>
                      ) : (
                        <>
                          {/* The Keys view lands on the key's row by its id and clears a filter that would hide it. */}
                          <Link className="mono" to={keyHref(session.keyId)}>
                            {session.keyId}
                          </Link>
                          {row.pool !== null && ` in pool ${row.pool.name}`}
                        </>
                      ),
                    },
                    { label: 'Client', value: session.userAgent === null ? <span className="muted">-</span> : <span title={session.userAgent}>{clientVersion(session.userAgent)}</span> },
                    {
                      label: 'Project',
                      value:
                        folder === null ? (
                          NOT_REPORTED
                        ) : (
                          <>
                            {facts.project !== null && <span className="mono">{facts.project}</span>}
                            {cwdApart !== null && <span className="sub">working directory {cwdApart}</span>}
                          </>
                        ),
                    },
                    { label: 'Repository', value: place === null ? NOT_REPORTED : <span className="mono">{place}</span> },
                    { label: 'Branch', value: facts.branch === null ? NOT_REPORTED : <span className="mono">{facts.branch}</span> },
                    {
                      label: 'Fork',
                      value: session.forkedFrom === null ? (
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
                              {shortId(session.forkedFrom, SESSION_CHARS)}
                            </a>
                          ) : (
                            <span className="mono" title={session.forkedFrom}>
                              {shortId(session.forkedFrom, SESSION_CHARS)}
                            </span>
                          )}
                          {session.forkDepth === null ? '' : ` at depth ${session.forkDepth}`}
                          {relation === null ? '' : `, ${relation}`}
                        </>
                      ),
                    },
                    { label: 'Placements', value: String(session.placements) },
                    { label: 'Companion', value: <CompanionChip now={now} session={session} /> },
                  ]}
                />
                <Facts
                  title="Routing"
                  link={<Link to={routingLogTo()}>View in routing log</Link>}
                  rows={[
                    {
                      label: 'Account',
                      value: session.accountId === null ? (
                        <span className="muted">none</span>
                      ) : (
                        <>
                          <Link to={accountHref(session.accountId)}>{session.accountId}</Link>
                          {account !== null && ` ${account.kind === 'oauth' ? 'subscription' : 'API key'}`}
                        </>
                      ),
                    },
                    { label: 'Pinned since', value: pinnedSince === null ? <span className="muted">-</span> : since(pinnedSince, now) },
                    {
                      label: 'Shed mark',
                      value: mark === null ? (
                        <span className="muted">none</span>
                      ) : (
                        <>
                          <ShedBadge session={session} />
                          {` on ${mark.on} ${triggerPhrase(mark.window)}`}
                        </>
                      ),
                    },
                    { label: 'Moved', value: moved ?? <span className="muted">none</span> },
                    { label: 'Runway', value: runwayWord(session) ?? <span className="muted">-</span> },
                    { label: 'Speed', value: session.speed !== null && speed.speed !== null && speed.speed < 1 ? <SpeedChip {...speed} /> : 'full speed' },
                    { label: 'Cycle', value: session.cycleMs === null ? <span className="muted">not measured yet</span> : ageWord(session.cycleMs) },
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
                    {[...shownLines, ...(unfolded ? hiddenHere : [])].map((line) => agentRow(line.agentId, line, agentFigures(session, line.agentId), line.facts))}
                    {(fold === null ? earlierOpen : unfolded) && earlier.map((agent) => agentRow(agent.agentId, null, agent, agent.facts))}
                  </tbody>
                </table>
              </div>
              {fold !== null && fold > 0 && <Disclosure className="srow__fold" controls={tableId} label={`${plural(fold, 'agent')} hidden by the filters`} onToggle={onUnfold} open={unfolded} />}
              {/* Under Agents filters the earlier agents are among the agents the fold line counts. */}
              {fold === null && earlier.length > 0 && <EarlierLine agents={earlier} controls={tableId} cost={cost} onToggle={() => setEarlierOpen((v) => !v)} open={earlierOpen} session={session} />}
              <div className="srow__warming">
                <Disclosure
                  controls={warmingId}
                  hint={noRecipe !== null ? 'no recipe' : stateSummary(recipes) || 'no recipe'}
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
                  {shownLines.length > 0 && (
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
                      {shownLines.map((line) => (
                        <AgentLine key={`${line.agentId}|${line.model}`} line={line} now={now} requestsTo={hasTraffic ? requestsHref(session, { agentId: line.agentId, firstRequestAt: line.firstRequestAt, lastRequestAt: lineLastRequestAt(line) }) : null} />
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
 * Cost cell reads it, `+$` where dollars were billed beside a plan figure, and on its title each agent's label with its
 * id, or the id alone where it has no label. Opened, each joins the Agents table the line controls.
 */
function EarlierLine({ agents, controls, cost, onToggle, open, session }: { agents: EarlierAgent[]; controls: string; cost: CostContext; onToggle: () => void; open: boolean; session: Session }) {
  const { spend } = figuresOf(agents);
  const reading = spendUnitOf(spend, cost.unit, session.accountId, null);
  const cell = costCellOf(reading.unit, cost.week, cost.scale);
  return (
    <Disclosure
      className="srow__earlier"
      controls={controls}
      hint={
        <>
          {`· ${plural(spend.requests, 'request')} · ${cell?.main ?? '-'}`}
          {reading.billed > 0 && (
            <>
              {' '}
              <BilledBadge usd={reading.billed} />
            </>
          )}
        </>
      }
      label={plural(agents.length, 'earlier agent')}
      onToggle={onToggle}
      open={open}
      title={agents.map((a) => agentNamed(a.agentId, a.facts)).join(', ')}
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
  baseLabel,
  depth,
  from,
  onFollow,
  relation,
}: {
  base: string | null;
  /**
   * The label of the listed base's account while its pin is live, as the Accounts page names it. Null off an unlisted
   * base or one whose pin is gone.
   */
  baseLabel: string | null;
  depth: number | null;
  from: string;
  onFollow: (id: string) => void;
  relation: string | null;
}) {
  const at = depth === null ? '' : ` at depth ${depth}`;
  const title =
    base === null
      ? `Branched from session ${from}${at}, which this list does not hold`
      : relation === null
        ? `Branched from session ${from}${at}. Go to its row`
        : `Branched from session ${from}${at}, ${relation} (${baseLabel}). Go to its row`;
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
  cold: "The pin's prompt cache is past its TTL or has no refresh on record, and the next request pays a cold read.",
  warm: "The pin's prompt cache is within its TTL of the last refresh.",
};

const cacheTitle = (warm: boolean, recipe: KeepaliveRecipe | null, noRecipe: string | null): string =>
  joinPresent([PIN_CACHE_HINT[warm ? 'warm' : 'cold'], recipe === null ? noRecipe : `${STATE_HINT[recipe.state]} ${CAUSE_HINT[recipe.cause]}`, recipe?.nextProbeAt ? `Next probe at ${fmtAbsolute(recipe.nextProbeAt)}.` : null], ' ');

function CacheBadge({ title, warm }: { title: string; warm: boolean }) {
  return (
    <Badge tone={warm ? 'good' : 'neutral'} dot title={title}>
      {warm ? 'warm' : 'cold'}
    </Badge>
  );
}

/**
 * A line's last request, its recipe's or, where no recipe stands, its pin's last use. The Last request cell draws it.
 * The Agent cell's link opens the preset that covers the agent's first request measured back from the click, which
 * holds this request too, or past the widest preset a span from the first request to a minute past this one, so the
 * log the link opens holds the request the cell names.
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
  const park = a === null ? '' : parkDetail(a, now);
  const label = agentLabel(line.agentId, line.facts);
  return (
    <div className="agents__row">
      <span className={label === null ? 'mono agents__id ellipsis' : 'agents__id ellipsis'} title={line.agentId}>
        {requestsTo === null ? (label ?? line.agentId) : <Link to={requestsTo}>{label ?? line.agentId}</Link>}
      </span>
      {ttlMs === null ? <span className="muted">-</span> : <span className="num">{fmtDuration(ttlMs)}</span>}
      <span {...timeCell(lastRequestAt, now, 'none')} />
      {a === null ? (
        <>
          <span className="muted">-</span>
          <span className="muted">-</span>
          <span className="muted">-</span>
          <span className="agents__state row" style={{ gap: 6 }}>
            <CacheBadge title={PIN_CACHE_HINT[cache]} warm={cache === 'warm'} />
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
              {park !== '' && ` · ${park}`}
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
 * holds. The account-bound badge reads the line's own pin on either account. An account the list lacks reads its id
 * alone.
 */
function LineAccount({ account, bound, children, className = 'agents__account', id, now, row }: { account: Account | null; bound: boolean | null; children?: ReactNode; className?: string; id: string; now: number; row: Session | null }) {
  return (
    <span className={className}>
      <AccountLink account={account} id={id} />
      {/* An unpinned row on a slowed account carries no speed, so the row's own speed gates the chip. */}
      {row === null ? <SpeedChip {...memberSpeed({ speed: null }, account, now)} /> : row.speed !== null && <SpeedChip {...memberSpeed(row, account, now)} />}
      {row !== null && <ShedBadge session={row} />}
      <BoundBadge bound={bound} />
      {children}
    </span>
  );
}

/** An agent as its label with its id behind it, or its id alone where no label stands. Two agents of one session
 *  can report one name or one description, so the id stays behind a label. */
const agentNamed = (agentId: string, facts: Pick<SessionAgentFacts, 'description' | 'name'> | null): string => {
  const label = agentLabel(agentId, facts);
  return label === null ? agentId : `${label} (${agentId})`;
};

/**
 * One row of the Agents table: a line's agent, read off its pin and its recipe, or an earlier agent, which holds no
 * line and reads its ledger entry with its reported facts and type, dashes for a pin's figures and `no pin`. Requests
 * and Cost read the agent's entry, its sums over every model it ran, and the menu opens the session's views narrowed to
 * the agent.
 */
function AgentRow({
  agentId,
  cost,
  facts,
  figures,
  line,
  now,
  session,
}: {
  agentId: string;
  cost: CostContext;
  /** What the companion reports of the agent, null where it reports none. */
  facts: SessionAgentFacts | null;
  /** The agent's entry, null for an agent the ledger holds no row of. */
  figures: AgentFigures | null;
  /** The line the agent draws, null for an earlier agent. */
  line: SessionLine | null;
  now: number;
  session: Session;
}) {
  const navigate = useNavigate();
  const pin = line?.pin ?? null;
  const recipe = line?.recipe ?? null;
  // The agent's request log link opens under the check the menu's ledger items read.
  const hasTraffic = session.lastRequestAt !== null;
  const lastRequestAt = figures?.lastRequestAt ?? null;
  const linked = { agentId, firstRequestAt: figures?.firstRequestAt ?? null, lastRequestAt };
  const model = line === null ? (figures?.model ?? null) : line.model;
  const short = model === null ? null : shortModel(model);
  // A line's account is its pin's, and its recipe's only where no pin stands, as the row's "+N" and the Account filter
  // read it.
  const accountId = line === null ? (figures?.accountId ?? null) : lineAccountOf(line);
  const type = agentId === MAIN_AGENT ? null : line === null ? (figures?.agentType ?? null) : line.agentType;
  const title = joinPresent([lastRequestAt === null ? null : `Last request ${since(lastRequestAt, now)}.`, pin?.pinnedAt ? `Pinned since ${since(pin.pinnedAt, now)}.` : null], ' ');
  const tokens = pin?.lastInputTokens ?? null;
  const context = contextCell(tokens);
  const burn = burnReading(pin, cost);
  // A line reads unreported where the route answers no word for its agent, as the row's Status does for main.
  const word = line === null ? null : lineWordOf(line);
  const park = word === 'parked' && recipe !== null ? parkDetail(recipe, now) : '';
  const hint = LIFECYCLE_HINT[word ?? 'unreported'];
  const label = agentLabel(agentId, facts);
  return (
    <tr>
      <td title={joinPresent([facts?.name, facts?.description, agentId, title], '\n')}>
        <span className={label === null ? 'mono agent-cell__label ellipsis' : 'agent-cell__label ellipsis'}>
          {hasTraffic ? <Link to={requestsHref(session, linked)}>{label ?? agentId}</Link> : (label ?? agentId)}
        </span>
        {label !== null && <span className="mono sub">{agentId}</span>}
      </td>
      <td>{type ?? <span className="muted">-</span>}</td>
      <td title={model === null ? undefined : modelTitle(model)}>
        {short === null ? <span className="muted">-</span> : <span className="mono nowrap">{short}</span>}
        <span className="sub">{pin?.effort ?? '-'}</span>
      </td>
      <td>{accountId === null ? <span className="muted">none</span> : <LineAccount account={cost.accountById.get(accountId) ?? null} bound={pin?.accountBound ?? null} id={accountId} now={now} row={accountId === session.accountId ? session : null} />}</td>
      <td title={tokens ? `${fmtInt(tokens)} tokens` : undefined}>
        <span className={context === '-' ? 'muted' : 'num'}>{context}</span>
      </td>
      <td title={burn?.title}>{burn === null || burn.dash ? <span className="muted">-</span> : <span className="num">{burn.text}</span>}</td>
      <td className="right num">{requestsFigure(figures === null ? 0 : requestCount(figures))}</td>
      <CostCell accountId={accountId} cost={cost} spend={figures?.spend ?? null} />
      <td>
        {line === null ? (
          <span className="muted">no pin</span>
        ) : word === null || word === 'unreported' ? (
          <span className="muted" title={hint}>
            unreported
          </span>
        ) : (
          <Badge tone={STATUS_TONE[word]} dot title={joinPresent([hint, park], '\n')}>
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
            <CacheBadge title={cacheTitle(pin.warm, recipe, null)} warm={pin.warm} />
            {recipe !== null && <span className="sub">{STATE_LABEL[recipe.state]}</span>}
          </>
        )}
      </td>
      <td>
        <Menu
          label={`Actions for agent ${agentNamed(agentId, facts)}`}
          items={[
            ...ledgerItems(session, navigate, linked),
            // An hour either side of the agent's last request, read when the item is chosen.
            { label: 'View in routing log', onClick: () => navigate(logHref({ agentId, session: session.sessionId, ...aroundSubject(lastRequestAt === null ? null : Date.parse(lastRequestAt), now) })) },
          ]}
        />
      </td>
    </tr>
  );
}

type OtherPin = { accountId: string; label: string; model: string };

/**
 * "+2" beside a row's figure, hovering into `lead` and one line per agent that sits on another model or account, in the
 * accent tone where one of those agents matches the filter standing on its column.
 */
function OtherPins({ accent, agents, lead }: { accent: boolean; agents: OtherPin[]; lead: string }) {
  if (agents.length === 0) return null;
  return (
    <Badge tone={accent ? 'accent' : 'neutral'} title={`${lead}\n${agents.map((a) => `${a.label}: ${a.model} on ${a.accountId}`).join('\n')}`}>
      +{agents.length}
    </Badge>
  );
}
