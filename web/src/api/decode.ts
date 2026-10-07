import type {
  Account, AccountCatalog, AccountRollup, AffinityMode, AnalyticsGroup, AnalyticsPoint, ApiKey, AttemptOutcome, AttemptResponse, AttentionItem, BudgetPeriod, BurnPoint, BurnUnit, CacheMissReason, CacheTtl, CapturedBodies, CatalogModel,
  CohortHold, DecisionBy, DecisionKind, DecisionRecord, DecisionSummary, FactEntity, FactUnit, FastLaneReading, FastPrice, GuardVerdict, KeepaliveOverview, KeepaliveRecipe, KeyModels, KnownWindowId, LiveAttempt, LiveDelta, LiveMetric, LiveRequest, LiveSnapshot, LiveTick, MemberBudget, MemberEligibility, ModelPrice, ModelRule, ModelRulesMode,
  ModelsState, Overview, PairingSession, PinMigrationReason, PinReason, PinReleaseReason, Placement, PlacementRow, Pool, PoolCacheTtl, PrefixEntry, QuotaHistory, QuotaPoll, QuotaSnapshot, RateLimitWindow, RecordCandidate, RecordOutcome, RequestEvent, RequestLog, RequestOutcome, RequestPage, RequestShape, RewritePoint,
  RoutingEvent, RoutingLogPage, RoutingLogRow, RuleCooldown, Score, SelectionTrace, Session, SessionPlacement, Settings, SubagentPlacement, Subscription, SubscriptionCosts, SummaryFact, TtlRules, TypeMultiplier, WarmSessions, WindowId,
} from './types';
import { ACCOUNT_KIND_VALUES, ATTENTION_KIND_VALUES, AUTO_MODE_VALUES } from './types';
import { CACHE_TTL_DEFAULTS, METERED_TTL_DEFAULTS, PLAN_TTL_DEFAULTS, POOL_AFFINITY_DEFAULTS, POOL_ENGINE_DEFAULTS, type AccountPatch, type KeyInput, type PoolInput } from './client';
import { WASTED_PROBE_REASONS, emptyCacheMissReasons, emptyWastedProbeReasons } from '@/lib/cache';
import { emptyCounts } from '@/lib/counts';
import { fmtDate } from '@/lib/format';
import { BUCKET_COUNT, emptyHistogram } from '@/lib/histogram';
import { isFastScope } from '@/lib/quota';
import { WINDOW_HOURS, WINDOW_LABEL, WINDOW_ORDER, hoursOfKey } from '@/lib/windows';

type Rec = Record<string, unknown>;
const rec = (v: unknown): Rec => (v && typeof v === 'object' ? (v as Rec) : {});
const list = (v: unknown): unknown[] => (Array.isArray(v) ? v : []);
const str = (v: unknown, fallback = ''): string => (typeof v === 'string' ? v : fallback);
const strOrNull = (v: unknown): string | null => (typeof v === 'string' ? v : null);
const num = (v: unknown, fallback = 0): number => (typeof v === 'number' && Number.isFinite(v) ? v : fallback);
const numOrNull = (v: unknown): number | null => (typeof v === 'number' && Number.isFinite(v) ? v : null);
const bool = (v: unknown, fallback = false): boolean => (typeof v === 'boolean' ? v : fallback);
const strings = (v: unknown): string[] => list(v).filter((x): x is string => typeof x === 'string');
/** A map the route keys by data (a move reason, a model id), with every entry that is not a number dropped. */
const numberMap = (v: unknown): Record<string, number> => Object.fromEntries(Object.entries(rec(v)).filter((e): e is [string, number] => numOrNull(e[1]) !== null));

/** The wire carries Unix milliseconds; every page formats ISO strings. */
export const iso = (ms: number): string => new Date(ms).toISOString();
export const isoOrNull = (ms: unknown): string | null => (typeof ms === 'number' && Number.isFinite(ms) ? iso(ms) : null);
const isoOr = (ms: unknown, fallback: string): string => isoOrNull(ms) ?? fallback;
const epoch = '1970-01-01T00:00:00.000Z';

/**
 * The ledger, the request log and the usage endpoint spell a window by the endpoint's key; the live feed carries
 * the head's short ids and the router's own windows name the Fable week `fable`. Each spelling of a window is one
 * window to the UI: `seven_day_fable`, `seven_day_overage_included`, `7d_oi` and `fable` all mean the model-scoped
 * week the UI draws third. Any other key is a window of its own, kept under its name.
 */
const WINDOW_IDS = new Map<string, KnownWindowId>([
  ['5h', '5h'],
  ['7d', '7d'],
  ['7d_oi', '7d_fable'],
  ['fable', '7d_fable'],
  ['five_hour', '5h'],
  ['seven_day', '7d'],
  ['seven_day_fable', '7d_fable'],
  ['seven_day_overage_included', '7d_fable'],
]);
/** The UI's id for a window key: a known window's own id, any other key as it is, `null` for no key at all. */
export const windowId = (id: string): WindowId | null => WINDOW_IDS.get(id) ?? (id === '' ? null : id);

/** The known windows in the order the pages draw them, then every other id in the order it was first seen. */
function inDrawnOrder<T>(byId: Map<WindowId, T>): T[] {
  const known: readonly WindowId[] = WINDOW_ORDER;
  return [...WINDOW_ORDER.flatMap((id) => byId.get(id) ?? []), ...[...byId].filter(([id]) => !known.includes(id)).map(([, value]) => value)];
}

function decodeWindow(w: Rec): RateLimitWindow | null {
  const id = windowId(str(w.id));
  if (id === null) return null;
  return { id, label: WINDOW_LABEL[id] ?? id, observedAt: isoOr(w.observed_at, epoch), periodHours: WINDOW_HOURS[id] ?? hoursOfKey(id), resetsAt: isoOrNull(w.resets_at), utilization: num(w.utilization) };
}

/** One window per id, the newest reading when both samplers wrote one, in the order the pages draw them. */
function foldWindows(windows: RateLimitWindow[]): RateLimitWindow[] {
  const newest = new Map<WindowId, RateLimitWindow>();
  for (const w of windows) {
    const seen = newest.get(w.id);
    if (!seen || Date.parse(w.observedAt) > Date.parse(seen.observedAt)) newest.set(w.id, w);
  }
  return inDrawnOrder(newest);
}

/** The account's state and its flags folded onto the one badge the pages show. */
function accountStatus(a: Rec, cooldowns: unknown[]): Account['status'] {
  const state = str(a.state);
  if (state === 'reauth_required' || state === 'funding_required') return state;
  if (state === 'unpaired' || state === 'disconnected') return 'unpaired';
  if (!bool(a.enabled, true)) return 'paused';
  // A fast-lane row holds fast-mode requests alone: the account still serves its standard traffic.
  if (cooldowns.some((c) => !isFastScope(str(rec(c).scope)))) return 'cooldown';
  return 'ready';
}

export function decodeModelRule(v: unknown): ModelRule {
  const r = rec(v);
  return { not: bool(r.not), pattern: str(r.pattern) };
}

export function decodeRuleCooldown(v: unknown): RuleCooldown {
  const c = rec(v);
  return { accountId: str(c.account_id), reason: str(c.reason), scope: str(c.scope), setAt: isoOr(c.set_at, epoch), until: isoOr(c.until, epoch), window: str(c.window) };
}

export function decodeKeyModels(v: unknown): KeyModels {
  const k = rec(v);
  return {
    keyId: str(k.key_id),
    longContextTwins: bool(k.long_context_twins, true),
    modelRulesMode: MODEL_RULES_MODE_VALUES.find((x) => x === str(k.model_rules_mode)) ?? 'extend',
    poolId: str(k.pool_id),
    rules: list(k.rules).map(rec).map((r) => ({ cooldowns: list(r.cooldowns).map(decodeRuleCooldown), level: str(r.level) === 'pool' ? 'pool' : 'key', not: bool(r.not), pattern: str(r.pattern) })),
  };
}

export function decodeModelsState(v: unknown): ModelsState {
  const s = rec(v);
  return {
    cooldowns: list(s.cooldowns).map(rec).map((g) => ({ model: str(g.model), rows: list(g.rows).map(decodeRuleCooldown) })),
  };
}

// The account and attention kinds are defined beside the types they spell; exported here with the decoder's other
// closed sets.
export { ACCOUNT_KIND_VALUES, ATTENTION_KIND_VALUES };

/** The typed subscription the route's `profile` and `subscription` records fold into; null when both are absent. */
export function decodeSubscription(profile: Rec | null, sub: Rec | null): Subscription | null {
  if (profile === null && sub === null) return null;
  return {
    billingType: str(profile?.billing_type),
    derivedEndsAt: isoOrNull(sub?.derived_ends_at),
    endsAt: isoOrNull(sub?.ends_at),
    organizationType: str(profile?.organization_type),
    plan: sub?.plan === 'max' || sub?.plan === 'pro' ? sub.plan : null,
    rateLimitTier: str(sub?.tier ?? profile?.rate_limit_tier),
    since: isoOrNull(sub?.since),
    trialEndsAt: isoOrNull(sub?.trial_ends_at),
  };
}

export function decodeAccount(v: unknown): Account {
  const a = rec(v);
  const cooldowns = list(a.cooldowns).map(rec);
  const windows = foldWindows(list(a.windows).map(rec).map(decodeWindow).filter((w): w is RateLimitWindow => w !== null));
  const fastLane = a.fast_lane === null || a.fast_lane === undefined ? null : rec(a.fast_lane);
  const extra = a.extra_usage === null || a.extra_usage === undefined ? null : rec(a.extra_usage);
  const overage = a.overage === null || a.overage === undefined ? null : rec(a.overage);
  const profile = a.profile === null || a.profile === undefined ? null : rec(a.profile);
  const sub = a.subscription === null || a.subscription === undefined ? null : rec(a.subscription);
  const identity = rec(a.identity);
  const credential = rec(a.credential);
  const subscription = decodeSubscription(profile, sub);
  return {
    cancelled: bool(a.cancelled),
    cooldowns: cooldowns.map((c) => ({ reason: str(c.reason), scope: str(c.scope), setAt: isoOr(c.set_at, epoch), until: isoOr(c.until, epoch), windowId: windowId(str(c.window)) })),
    credential: { accessTokenExpiresAt: isoOrNull(credential.access_expires_at), lastError: strOrNull(credential.last_error), lastRefreshAt: isoOrNull(credential.last_refresh_at), quarantined: bool(credential.quarantined), refreshTokenExpiresAt: isoOrNull(credential.refresh_expires_at) },
    email: strOrNull(identity.email),
    // Kept when the provider switched extra usage off: the reason it gave exists for exactly that account.
    extraUsage: extra ? { disabledReason: strOrNull(extra.disabled_reason), enabled: bool(extra.is_enabled), inUse: bool(extra.in_use), monthlyLimitCredits: num(extra.monthly_limit), usedCredits: num(extra.used_credits) } : null,
    fableWindow: windows.find((w) => w.id === '7d_fable') ?? null,
    fastLane: fastLane ? { input: fastLaneReading(fastLane.input), output: fastLaneReading(fastLane.output) } : null,
    id: str(a.id),
    kind: ACCOUNT_KIND_VALUES.find((k) => k === str(a.kind)) ?? 'oauth',
    label: str(a.label),
    overage: overage ? { inUse: bool(overage.in_use), observedAt: isoOr(overage.observed_at, epoch), reset: isoOrNull(overage.reset), since: isoOrNull(overage.since) } : null,
    plan: planName(subscription),
    pools: list(a.pools).map(rec).map((p) => ({ poolId: str(p.pool_id), priority: num(p.priority) })),
    requests: { last24h: num(rec(a.requests).last_24h), last7d: num(rec(a.requests).last_7d), lifetime: num(rec(a.requests).lifetime) },
    // A row whose answer carried none reads 0, which the route refuses as stale, so the editor offers Reload rather
    // than landing over a row it could not vouch for.
    revision: num(a.revision),
    routingEnabled: bool(a.enabled, true),
    status: accountStatus(a, cooldowns),
    subscription,
    usage: a.usage === null || a.usage === undefined ? null : decodeSnapshot(str(a.id), a.usage),
    windows: windows.filter((w) => w.id !== '7d_fable'),
  };
}

function fastLaneReading(v: unknown): FastLaneReading | null {
  if (v === null || v === undefined) return null;
  const r = rec(v);
  return { observedAt: isoOr(r.observed_at, epoch), resetsAt: isoOrNull(r.resets_at), utilization: num(r.utilization) };
}

/** "Max" or "Pro" from the typed subscription's plan; the tier's own label when the document has no flags. */
export function planName(sub: Subscription | null): string | null {
  if (sub === null) return null;
  if (sub.plan === 'max') return 'Max';
  if (sub.plan === 'pro') return 'Pro';
  return sub.rateLimitTier ? planLabel(sub.rateLimitTier) : null;
}

/** `default_claude_max_5x` reads as "Max 5x" and `default_claude_ai`, the tier the profile reports for Pro, as "Pro";
 *  anything else is shown as sent. */
export function planLabel(tier: string): string {
  const m = /^(?:default_)?claude_(ai|max|pro|team|enterprise)(?:_(\d+)x)?$/.exec(tier);
  if (!m) return tier;
  const name = m[1] === 'ai' ? 'Pro' : m[1][0].toUpperCase() + m[1].slice(1);
  return m[2] ? `${name} ${m[2]}x` : name;
}

export function decodeCatalogModel(v: unknown): CatalogModel {
  const m = rec(v);
  return { createdAt: str(m.created_at), displayName: str(m.display_name), effortLevels: strings(m.effort_levels), id: str(m.id), maxInputTokens: numOrNull(m.max_input_tokens), maxTokens: numOrNull(m.max_tokens) };
}

export function decodeAccountCatalog(v: unknown): AccountCatalog {
  const c = rec(v);
  return { error: strOrNull(c.error), fetchedAt: isoOr(c.fetched_at, epoch), models: list(c.models).map(decodeCatalogModel) };
}

export function decodePairingSession(v: unknown): PairingSession {
  const p = rec(v);
  return { authorizeUrl: str(p.authorization_url), expiresAt: isoOr(p.expires_at, epoch), id: str(p.id), redirectUri: str(p.redirect_uri), state: new URL(str(p.authorization_url), 'http://x').searchParams.get('state') ?? '' };
}

export function decodeQuotaHistory(v: unknown): QuotaHistory[] {
  const q = rec(v);
  const byId = new Map<WindowId, QuotaHistory>();
  for (const w of list(q.windows).map(rec)) {
    const id = windowId(str(w.id));
    if (id === null) continue;
    const samples = list(w.samples).map(rec).map((s) => ({ at: isoOr(s.at, epoch), utilization: num(s.utilization) }));
    const seen = byId.get(id);
    // A window the header and the poll both sampled is one series, whatever each called it.
    byId.set(id, seen ? { ...seen, samples: [...seen.samples, ...samples].sort((x, y) => x.at.localeCompare(y.at)) } : { accountId: str(q.account_id), samples, windowId: id });
  }
  return inDrawnOrder(byId);
}

export const POLL_OUTCOME_VALUES = ['clean', 'failed', 'rate_limited'] as const;

function decodePoll(v: unknown): QuotaPoll | null {
  if (!v || typeof v !== 'object') return null;
  const p = rec(v);
  return {
    backoffSince: isoOrNull(p.backoff_since),
    lastAttemptAt: isoOrNull(p.last_attempt_at),
    lastCleanAt: isoOrNull(p.last_clean_at),
    lastError: strOrNull(p.last_error),
    // An outcome this build does not know reads as a failure rather than as a clean answer.
    lastOutcome: POLL_OUTCOME_VALUES.find((o) => o === str(p.last_outcome)) ?? 'failed',
    nextAttemptAt: isoOrNull(p.next_attempt_at),
    rateLimitedInARow: num(p.rate_limited_in_a_row),
  };
}

export function decodeSnapshot(accountId: string, v: unknown): QuotaSnapshot {
  const u = rec(v);
  const error = strOrNull(u.error);
  return { accountId, error, fetchedAt: isoOrNull(u.fetched_at), poll: decodePoll(u.poll), status: error ? 'error' : 'success' };
}

export const ATTENTION_PAGE_VALUES = ['accounts', 'keys', 'pools'] as const satisfies readonly NonNullable<AttentionItem['link']>['page'][];
export const ATTENTION_SUBJECT_VALUES = ['account', 'client', 'pool', 'router'] as const satisfies readonly AttentionItem['subject']['type'][];

export function decodeAttention(v: unknown): AttentionItem {
  const a = rec(v);
  const subject = rec(a.subject);
  const link = a.link ? rec(a.link) : null;
  return {
    detail: rec(a.detail),
    // A kind this build does not know reads as the worst one, so it is never drawn quieter than it may be.
    kind: ATTENTION_KIND_VALUES.find((k) => k === str(a.kind)) ?? 'pool_exhausted',
    link: link && { id: str(link.id), page: ATTENTION_PAGE_VALUES.find((p) => p === str(link.page)) ?? 'pools' },
    pools: strings(a.pools),
    since: isoOr(a.since, epoch),
    subject: { id: str(subject.id), label: str(subject.label), type: ATTENTION_SUBJECT_VALUES.find((t) => t === str(subject.type)) ?? 'pool' },
  };
}

export function decodeKey(v: unknown): ApiKey {
  const k = rec(v);
  const expires = isoOrNull(k.expires_at);
  const revoked = isoOrNull(k.revoked_at);
  // A row from a route that reports no `enabled` is taken as on rather than off.
  const enabled = bool(k.enabled, true);
  return {
    autoModeServer: k.auto_mode_server === true ? true : k.auto_mode_server === false ? false : null,
    bodyCaptureHours: numOrNull(k.body_capture_hours),
    createdAt: isoOr(k.created_at, epoch),
    enabled,
    expiresAt: expires,
    id: str(k.id),
    keepaliveOptOut: bool(k.keepalive_opt_out),
    lastUsedAt: isoOrNull(k.last_used_at),
    lifetime: { cost: num(rec(k.lifetime).cost_usd), requests: num(rec(k.lifetime).requests) },
    longContextTwins: bool(k.long_context_twins, true),
    modelRules: list(k.model_rules).map(decodeModelRule),
    modelRulesMode: MODEL_RULES_MODE_VALUES.find((x) => x === str(k.model_rules_mode)) ?? 'extend',
    name: str(k.label),
    notes: strOrNull(k.notes),
    poolId: str(k.pool_id),
    prefix: str(k.prefix),
    retiredSecretLastUsedAt: isoOrNull(k.retired_secret_last_used_at),
    sessionsOnRetiredSecret: num(k.sessions_on_retired_secret),
    // Routing refuses a key that is revoked, expired or switched off, in that order of finality.
    status: revoked !== null ? 'revoked' : expires !== null && Date.parse(expires) <= Date.now() ? 'expired' : enabled ? 'active' : 'disabled',
    updatedAt: isoOr(k.updated_at, epoch),
    usage7d: { cost: num(rec(k.usage_7d).cost_usd), requests: num(rec(k.usage_7d).requests), tokens: num(rec(k.usage_7d).tokens) },
  };
}

export const AFFINITY_MODE_VALUES = ['always', 'when_cached'] as const satisfies readonly AffinityMode[];
export const BUDGET_PERIOD_VALUES = ['day', 'month'] as const satisfies readonly BudgetPeriod[];
export const ELIGIBILITY_VALUES = ['budget', 'cooldown', 'eligible', 'overage', 'unavailable'] as const satisfies readonly MemberEligibility[];
export const SESSION_PLACEMENT_VALUES = ['by_strategy', 'by_warm_prefix'] as const satisfies readonly SessionPlacement[];
export const SUBAGENT_PLACEMENT_VALUES = ['by_cohort', 'by_strategy', 'with_main'] as const satisfies readonly SubagentPlacement[];
export const MODEL_RULES_MODE_VALUES = ['extend', 'replace'] as const satisfies readonly ModelRulesMode[];
export const OVERAGE_POLICY_VALUES = ['allow', 'never', 'prefer_plan'] as const;
export const POOL_STATUS_VALUES = ['active', 'degraded', 'disabled', 'exhausted'] as const;
export const WARM_SESSIONS_VALUES = ['always', 'never', 'when_needed'] as const satisfies readonly WarmSessions[];
export const POOL_STRATEGY_VALUES = ['pace', 'priority', 'round_robin'] as const;
const ttl = (v: unknown, fallback: CacheTtl): CacheTtl => (v === '1h' || v === '5m' ? v : fallback);

/** Read field by field against `fallback`, so a route that answers a partial rule set never yields an empty TTL. */
function decodeTtlRules(v: unknown, fallback: TtlRules): TtlRules {
  const c = rec(v);
  return {
    main: ttl(c.main, fallback.main),
    models: list(c.models).map(rec).map((m) => ({ main: ttl(m.main, fallback.main), model: str(m.model), subagent: ttl(m.subagent, fallback.subagent) })),
    subagent: ttl(c.subagent, fallback.subagent),
  };
}

export function decodeCacheTtl(v: unknown): PoolCacheTtl {
  const c = rec(v);
  return {
    enabled: bool(c.enabled, CACHE_TTL_DEFAULTS.enabled),
    injectWhenAbsent: bool(c.inject_when_absent, CACHE_TTL_DEFAULTS.injectWhenAbsent),
    metered: decodeTtlRules(c.metered, METERED_TTL_DEFAULTS),
    plan: decodeTtlRules(c.plan, PLAN_TTL_DEFAULTS),
  };
}

export function decodePool(v: unknown): Pool {
  const p = rec(v);
  const k = rec(p.keepalive);
  return {
    activePins: num(p.active_pins),
    affinityMode: AFFINITY_MODE_VALUES.find((x) => x === str(p.affinity_mode)) ?? POOL_AFFINITY_DEFAULTS.affinityMode,
    affinityTtlSeconds: num(p.affinity_ttl_seconds),
    autoModeServer: bool(p.auto_mode_server, true),
    bodyCaptureHours: num(p.body_capture_hours),
    burnWindowSeconds: num(p.burn_window_seconds, POOL_ENGINE_DEFAULTS.burnWindowSeconds),
    cacheDiagnostics: bool(p.cache_diagnostics),
    cacheTtl: decodeCacheTtl(p.cache_ttl),
    cost24h: num(p.cost_24h),
    id: str(p.id),
    inFlightCap: num(p.in_flight_cap, POOL_ENGINE_DEFAULTS.inFlightCap),
    keepalive: {
      enabled: bool(k.enabled), maxIdleSeconds: num(k.max_idle_seconds), maxParkedSeconds: numOrNull(k.max_parked_seconds), scheduledHorizonSeconds: numOrNull(k.scheduled_horizon_seconds),
    },
    // A row without the key fields reads as the store's defaults.
    keys: list(p.keys).map(rec).map((k) => ({ enabled: bool(k.enabled, true), id: str(k.id), label: str(k.label), prefix: str(k.prefix) })),
    members: list(p.members).map(rec).map((m) => ({
      accountId: str(m.account_id),
      budgetDailyUsd: numOrNull(m.budget_daily_usd),
      budgetMonthlyUsd: numOrNull(m.budget_monthly_usd),
      budgetUntil: isoOrNull(m.budget_until),
      // A row from before the router reported eligibility reads as not servable rather than as servable.
      eligible: ELIGIBILITY_VALUES.find((e) => e === str(m.eligible)) ?? 'unavailable',
      // The route answers null for an account it cannot find, which is no Anthropic-key account either.
      kind: ACCOUNT_KIND_VALUES.find((x) => x === str(m.kind)) ?? 'oauth',
      priority: num(m.priority),
      spends: list(m.spends).map(rec).map((s) => ({ period: BUDGET_PERIOD_VALUES.find((x) => x === str(s.period)) ?? 'month', periodEnd: isoOrNull(s.period_end), spentUsd: num(s.spent_usd) })),
    })),
    migrateWhenSpent: bool(p.migrate_when_spent, POOL_ENGINE_DEFAULTS.migrateWhenSpent),
    minSharedPrefixTokens: num(p.min_shared_prefix_tokens, POOL_AFFINITY_DEFAULTS.minSharedPrefixTokens),
    modelRules: list(p.model_rules).map(decodeModelRule),
    movesPerHour: Object.fromEntries(Object.entries(rec(p.moves_per_hour)).filter((e): e is [string, number] => numOrNull(e[1]) !== null)),
    name: str(p.label),
    overageMoveGraceMinutes: num(p.overage_move_grace_minutes, POOL_ENGINE_DEFAULTS.overageMoveGraceMinutes),
    overagePolicy: OVERAGE_POLICY_VALUES.find((x) => x === str(p.overage_policy)) ?? 'prefer_plan',
    paceFloor: num(p.pace_floor, POOL_ENGINE_DEFAULTS.paceFloor),
    preferSubscription: bool(p.prefer_subscription),
    requests24h: num(p.requests_24h),
    retryInPlaceSeconds: num(p.retry_in_place_seconds, POOL_ENGINE_DEFAULTS.retryInPlaceSeconds),
    selectBelowUtilization: numOrNull(p.select_below_utilization),
    sessionPlacement: SESSION_PLACEMENT_VALUES.find((x) => x === str(p.session_placement)) ?? POOL_AFFINITY_DEFAULTS.sessionPlacement,
    shedAheadOfPace: numOrNull(p.shed_ahead_of_pace),
    shedShare: num(p.shed_share, POOL_ENGINE_DEFAULTS.shedShare),
    sourceDwellSeconds: num(p.source_dwell_seconds, POOL_ENGINE_DEFAULTS.sourceDwellSeconds),
    status: POOL_STATUS_VALUES.find((x) => x === str(p.status)) ?? 'active',
    strategy: POOL_STRATEGY_VALUES.find((x) => x === str(p.strategy)) ?? 'pace',
    subagentPlacement: SUBAGENT_PLACEMENT_VALUES.find((x) => x === str(p.subagent_placement)) ?? POOL_AFFINITY_DEFAULTS.subagentPlacement,
    traffic24h: list(p.traffic_24h).map(rec).map((b) => ({ at: isoOr(b.at, epoch), costUsd: num(b.cost_usd), failed: num(b.failed), requests: num(b.requests), tokens: num(b.tokens) })),
    updatedAt: isoOr(p.updated_at, epoch),
    warmSessions: WARM_SESSIONS_VALUES.find((x) => x === str(p.warm_sessions)) ?? POOL_ENGINE_DEFAULTS.warmSessions,
    windDownMinutes: num(p.wind_down_minutes, POOL_ENGINE_DEFAULTS.windDownMinutes),
  };
}

export const ATTEMPT_OUTCOME_VALUES = ['auth_failed', 'canceled', 'rate_limited', 'succeeded', 'transport_error', 'upstream_error'] as const satisfies readonly AttemptOutcome[];
export const REQUEST_KIND_VALUES = ['count_tokens', 'inference', 'probe'] as const;
export const REQUEST_OUTCOME_VALUES = ['canceled', 'exhausted', 'failed', 'incomplete', 'rejected', 'succeeded'] as const satisfies readonly RequestOutcome[];
const outcomeOf = (v: unknown): AttemptOutcome => ATTEMPT_OUTCOME_VALUES.find((o) => o === str(v)) ?? 'upstream_error';
const missReason = (v: unknown): CacheMissReason | null => strOrNull(v);
const attemptWindows = (v: unknown) => list(v).map(rec).flatMap((w) => { const id = windowId(str(w.id)); return id === null ? [] : [{ id, resetsAt: isoOrNull(w.resets_at), utilization: numOrNull(w.utilization) }]; });
const usageOf = (v: unknown) => { const u = rec(v); return { cacheRead: num(u.cache_read), cacheWrite1h: num(u.cache_write_1h), cacheWrite5m: num(u.cache_write_5m), input: num(u.input), output: num(u.output), thinking: num(u.thinking), webFetch: num(u.web_fetch), webSearch: num(u.web_search) }; };
const shapeOf = (v: unknown): RequestShape => { const s = rec(v); return { contextEdits: strings(s.context_edits), effort: strOrNull(s.effort), maxTokens: numOrNull(s.max_tokens), messageCount: num(s.message_count), requestedServiceTier: strOrNull(s.requested_service_tier), requestedSpeed: strOrNull(s.requested_speed), systemBlocks: num(s.system_blocks), temperature: numOrNull(s.temperature), thinkingBudget: numOrNull(s.thinking_budget), thinkingType: strOrNull(s.thinking_type), toolCount: num(s.tool_count) }; };
const responseOf = (v: unknown): AttemptResponse => { const r = rec(v); return { clearedInputTokens: numOrNull(r.cleared_input_tokens), inferenceGeo: strOrNull(r.inference_geo), refusalCategory: strOrNull(r.refusal_category), refusalRecommendedModel: strOrNull(r.refusal_recommended_model), servedModel: strOrNull(r.served_model), speed: strOrNull(r.speed), stopReason: strOrNull(r.stop_reason), stopSequence: strOrNull(r.stop_sequence) }; };

/** The move reasons a `pin_migrated` event and a trace's `moved` carry. */
export const PIN_MIGRATION_REASON_VALUES = ['cooldown', 'drain', 'fast_lane', 'mark', 'overage', 'return_to_plan', 'shed', 'spent'] as const satisfies readonly PinMigrationReason[];
/** The reasons a `pin_expired` event carries; the drawer words any other name as sent. */
export const PIN_RELEASE_REASON_VALUES = [
  'attempted', 'budget', 'client_error', 'cooldown', 'deleted', 'disconnected', 'expired', 'fast_credits', 'fast_rate_limit', 'funding_required', 'overage', 'paused', 'reauth_required', 'released', 'spent', 'subscription_ended', 'unpaired', 'unserved',
] as const satisfies readonly PinReleaseReason[];
/**
 * The obstacles the router names for a candidate it passed over: the eligibility checks (`attempted`, `budget`,
 * `budget_unknown`, `cooldown`, `credits`, `operator`, `overage`, `subscription_ended`) and the partitions
 * (`above_ceiling`, `at_cap`, `floor`, `ranked`, `spent`, `winding_down`). The trace keeps any other name as sent.
 */
export const SKIP_OBSTACLE_VALUES = ['above_ceiling', 'at_cap', 'attempted', 'budget', 'budget_unknown', 'cooldown', 'credits', 'floor', 'operator', 'overage', 'ranked', 'spent', 'subscription_ended', 'winding_down'] as const;
export const PLACEMENT_RUNG_VALUES = ['content', 'static', 'with_main'] as const satisfies readonly Placement['rung'][];
const tierOf = (v: unknown): SelectionTrace['tier'] => {
  const t = list(v);
  return t.length === 3 && t.every((x) => numOrNull(x) !== null) ? [num(t[0]), num(t[1]), num(t[2])] : null;
};
/** A rung the drawer has no words for reads as no placement rather than as a verdict it cannot explain. */
const placementOf = (v: unknown): Placement | null => {
  if (v === null || typeof v !== 'object') return null;
  const p = rec(v);
  const rung = PLACEMENT_RUNG_VALUES.find((r) => r === str(p.rung));
  return rung === undefined ? null : { depth: num(p.depth), prefixTokens: num(p.prefix_tokens), rung };
};
const cohortHoldOf = (v: unknown): CohortHold | null => (v === null || typeof v !== 'object' ? null : { cohort: strOrNull(rec(v).cohort), heldMs: num(rec(v).held_ms), waitedOn: strings(rec(v).waited_on) });
export function decodeSelection(v: unknown): SelectionTrace | null {
  if (v === null || typeof v !== 'object') return null;
  const s = rec(v);
  return {
    cacheRead: numOrNull(s.cache_read), cohortHold: cohortHoldOf(s.cohort_hold), moved: strOrNull(s.moved), placement: placementOf(s.placement), renewed: bool(s.renewed),
    skipped: list(s.skipped).map(rec).map((k) => ({ accountId: str(k.account_id), obstacle: str(k.obstacle) })), strategy: str(s.strategy), tier: tierOf(s.tier),
  };
}

export function decodeRequest(v: unknown): RequestLog {
  const r = rec(v);
  return {
    accountId: strOrNull(r.account_id), agentId: strOrNull(r.agent_id), agentType: strOrNull(r.agent_type), at: isoOr(r.at, epoch),
    attempts: list(r.attempts).map(rec).map((a) => ({ accountId: str(a.account_id), cacheMissReason: missReason(a.cache_miss_reason), cooldownSet: a.cooldown_set ? { reason: str(rec(a.cooldown_set).reason), until: isoOr(rec(a.cooldown_set).until, epoch) } : null, durationMs: numOrNull(a.duration_ms), index: num(a.index) - 1, outcome: outcomeOf(a.outcome), response: responseOf(a.response), selection: decodeSelection(a.selection), startedAt: isoOr(a.started_at, epoch), upstreamMessageId: strOrNull(a.upstream_message_id), upstreamRequestId: strOrNull(a.upstream_request_id), upstreamStatus: numOrNull(a.upstream_status), windows: attemptWindows(a.windows) })),
    autoMode: AUTO_MODE_VALUES.find((m) => m === str(r.auto_mode)) ?? null,
    canonicalModel: str(r.canonical_model), capturedBodies: bool(r.captured_bodies), client: strOrNull(r.client), clientDrift: strOrNull(r.client_drift), compaction: strOrNull(r.compaction), contextCompacted: strOrNull(r.context_compacted), costUsd: num(r.cost_usd), durationMs: numOrNull(r.duration_ms), errorCode: strOrNull(r.error_code),
    forkDepth: numOrNull(r.fork_depth), forkedFrom: strOrNull(r.forked_from), held: cohortHoldOf(r.held), id: str(r.id),
    keyGeneration: num(r.key_generation), keyId: strOrNull(r.key_id),
    kind: REQUEST_KIND_VALUES.find((k) => k === str(r.kind)) ?? 'inference', model: str(r.model),
    outcome: REQUEST_OUTCOME_VALUES.find((o) => o === str(r.outcome)) ?? 'failed',
    poolId: strOrNull(r.pool_id), prevToolMs: numOrNull(r.prev_tool_ms), requestClass: strOrNull(r.request_class), requestTruncated: bool(r.request_truncated), responseTruncated: bool(r.response_truncated), sessionId: strOrNull(r.session_id), shape: shapeOf(r.shape),
    status: numOrNull(r.status), transport: str(r.transport) === 'sse' ? 'sse' : 'json', ttfbMs: numOrNull(r.ttfb_ms), ttftMs: numOrNull(r.ttft_ms), turnOrigin: strOrNull(r.turn_origin), usage: usageOf(r.usage), userAgent: strOrNull(r.user_agent),
  };
}

export const decodeRequestPage = (v: unknown): RequestPage => ({ nextCursor: strOrNull(rec(v).next_cursor), requests: list(rec(v).requests).map(decodeRequest) });
// The four counts are the route's own; nothing here measures the request or the response it passes through.
export const decodeBodies = (v: unknown): CapturedBodies => { const b = rec(v); return { request: b.request, requestBytesSeen: num(b.request_bytes_seen), requestBytesStored: num(b.request_bytes_stored), requestId: str(b.request_id), requestTruncated: bool(b.request_truncated), response: b.response, responseBytesSeen: num(b.response_bytes_seen), responseBytesStored: num(b.response_bytes_stored), responseTruncated: bool(b.response_truncated) }; };
export const decodeRequestEvent = (v: unknown): RequestEvent => ({ accountId: strOrNull(rec(v).account_id), at: isoOr(rec(v).at, epoch), detail: rec(v).detail === null || rec(v).detail === undefined ? null : rec(rec(v).detail), keyId: strOrNull(rec(v).key_id), kind: str(rec(v).kind), model: strOrNull(rec(v).model), sessionId: strOrNull(rec(v).session_id) });

export const DECISION_KIND_VALUES = ['exhausted', 'held', 'moved', 'placed', 'refused', 'renewed'] as const satisfies readonly DecisionKind[];
export const PIN_REASON_VALUES = ['not_looked_up', 'pinning_off'] as const satisfies readonly PinReason[];
const BY_KIND_VALUES = ['exhaustion', 'hold', 'none', 'refusal', 'rule', 'rung', 'strategy'] as const satisfies readonly DecisionBy['kind'][];
const FACT_UNIT_VALUES = ['count', 'ms', 'percent', 'percent_per_hour', 'text', 'time', 'tokens', 'usd'] as const satisfies readonly FactUnit[];
const FACT_ENTITY_VALUES = ['account', 'key', 'model', 'pool'] as const satisfies readonly FactEntity['kind'][];
const decisionKindOf = (v: unknown): DecisionKind | undefined => DECISION_KIND_VALUES.find((k) => k === str(v));
const orNull = <T,>(v: unknown, read: (r: Rec) => T): T | null => (v === null || typeof v !== 'object' ? null : read(rec(v)));
const scoreOf = (v: unknown): Score | null => orNull(v, (s) => ({ accountId: str(s.account_id), score: num(s.score), unit: str(s.unit) }));
const holdOf = (v: unknown) => orNull(v, (h) => ({ accountId: str(h.account_id), boundMs: num(h.bound_ms), cohort: strOrNull(h.cohort) }));
const exhaustionOf = (v: unknown) => orNull(v, (x) => ({ deadline: isoOrNull(x.deadline), kind: str(x.kind), obstacle: strOrNull(x.obstacle), reason: strOrNull(x.reason), window: strOrNull(x.window) }));
const refusalOf = (v: unknown) => orNull(v, (x) => ({ kind: str(x.kind), reason: str(x.reason) }));
/** A value the console cannot draw as a number or a word drops its pair rather than drawing a guess. */
const factOf = (v: unknown): SummaryFact[] => {
  const f = rec(v);
  if (typeof f.value !== 'number' && typeof f.value !== 'string') return [];
  const entity = orNull(f.entity, (e) => {
    const kind = FACT_ENTITY_VALUES.find((k) => k === str(e.kind));
    return kind === undefined ? null : { id: str(e.id), kind };
  });
  return [{ entity, key: str(f.key), unit: FACT_UNIT_VALUES.find((u) => u === str(f.unit)) ?? null, value: f.value }];
};

export function decodeDecisionSummary(v: unknown): DecisionSummary {
  const r = rec(v);
  const by = rec(r.by);
  return {
    type: 'decision',
    accountId: strOrNull(r.account_id), agentId: strOrNull(r.agent_id), at: isoOr(r.at, epoch), attempt: num(r.attempt, 1), attemptCount: numOrNull(r.attempt_count),
    by: { kind: BY_KIND_VALUES.find((k) => k === str(by.kind)) ?? 'none', tier: tierOf(by.tier), value: str(by.value) },
    cache: orNull(r.cache, (c) => ({ hit: bool(c.hit), inputTokens: num(c.input_tokens), promisedTokens: numOrNull(c.promised_tokens), readTokens: num(c.read_tokens) })),
    exhaustion: exhaustionOf(r.exhaustion), fact: list(r.fact).flatMap(factOf), fast: bool(r.fast), fromAccountId: strOrNull(r.from_account_id), hold: holdOf(r.hold), keyId: strOrNull(r.key_id),
    kind: decisionKindOf(r.kind) ?? 'placed', model: strOrNull(r.model), poolId: strOrNull(r.pool_id), probe: bool(r.probe), refusal: refusalOf(r.refusal), requestId: str(r.request_id), rule: strOrNull(r.rule), rung: placementOf(r.rung),
    runnerUp: scoreOf(r.runner_up), score: scoreOf(r.score), seq: num(r.seq, 1), sessionId: strOrNull(r.session_id), skipped: num(r.skipped), status: numOrNull(r.status), strategy: str(r.strategy), tier: tierOf(r.tier),
    warmth: orNull(r.warmth, (w) => ({ cause: str(w.cause), verdict: str(w.verdict) })),
  };
}

export function decodeRoutingEvent(v: unknown): RoutingEvent {
  const e = rec(v);
  return {
    type: 'event',
    accountId: strOrNull(e.account_id), actor: strOrNull(e.actor), agentId: strOrNull(e.agent_id), at: isoOr(e.at, epoch), detail: rec(e.detail), id: num(e.id), keyId: strOrNull(e.key_id), kind: str(e.kind),
    model: strOrNull(e.model), poolId: strOrNull(e.pool_id), requestId: strOrNull(e.request_id), sessionId: strOrNull(e.session_id),
  };
}

/**
 * A row's `type` picks its decoder. A row of another type, or one missing the identity its link and its cursor are
 * built from (a decision's request id, seq and kind, an event's id and kind), is dropped rather than drawn as a row
 * that leads nowhere.
 */
const routingLogRowOf = (v: unknown): RoutingLogRow[] => {
  const r = rec(v);
  if (r.type === 'decision') return typeof r.request_id === 'string' && numOrNull(r.seq) !== null && decisionKindOf(r.kind) !== undefined ? [decodeDecisionSummary(r)] : [];
  if (r.type === 'event') return numOrNull(r.id) !== null && typeof r.kind === 'string' ? [decodeRoutingEvent(r)] : [];
  return [];
};

export function decodeRoutingLogPage(v: unknown): RoutingLogPage {
  const p = rec(v);
  return { counts: orNull(p.counts, (c) => ({ decisions: num(c.decisions), events: num(c.events) })), nextCursor: strOrNull(p.next_cursor), rows: list(p.rows).flatMap(routingLogRowOf) };
}

const prefixEntryOf = (v: unknown): PrefixEntry | null =>
  orNull(v, (e) => ({ depth: num(e.depth), hash: str(e.hash), live: bool(e.live), prefixTokens: num(e.prefix_tokens), refusedBy: strOrNull(e.refused_by), ttlMs: num(e.ttl_ms), writtenAt: isoOr(e.written_at, epoch) }));
const tierOrZero = (v: unknown): [number, number, number] => tierOf(v) ?? [0, 0, 0];

function decodeRecordCandidate(v: unknown): RecordCandidate {
  const c = rec(v);
  const prefix = rec(c.prefix);
  return {
    accountId: str(c.account_id), allowanceKnown: bool(c.allowance_known),
    budget: list(c.budget).map(rec).map((b) => ({ budgetUsd: num(b.budget_usd), observedAt: isoOrNull(b.observed_at), period: str(b.period), periodEnd: isoOrNull(b.period_end), periodStart: isoOrNull(b.period_start), spentUsd: numOrNull(b.spent_usd) })),
    cooldowns: list(c.cooldowns).map(rec).map((x) => ({ blocking: bool(x.blocking), reason: str(x.reason), scope: str(x.scope), until: isoOr(x.until, epoch), window: str(x.window) })),
    credits: orNull(c.credits, (x) => ({ blocked: strOrNull(x.blocked), disabledReason: strOrNull(x.disabled_reason), enabled: bool(x.enabled), observedAt: isoOr(x.observed_at, epoch), utilization: numOrNull(x.utilization) })),
    enabled: bool(c.enabled), inFlight: num(c.in_flight), inFlightCap: num(c.in_flight_cap), inOverage: bool(c.in_overage), incarnation: num(c.incarnation), kind: str(c.kind) === 'api_key' ? 'api_key' : 'oauth',
    obstacle: orNull(c.obstacle, (o) => ({ name: str(o.name), reason: strOrNull(o.reason), until: isoOrNull(o.until), window: strOrNull(o.window) })),
    overage: orNull(c.overage, (o) => ({ observedAt: isoOr(o.observed_at, epoch), reset: isoOrNull(o.reset), since: isoOr(o.since, epoch), source: str(o.source) })),
    pace: orNull(c.pace, (p) => ({ deadline: isoOrNull(p.deadline), end: isoOrNull(p.end), hours: numOrNull(p.hours), remaining: num(p.remaining), reset: isoOrNull(p.reset), score: num(p.score) })),
    partition: strOrNull(c.partition), planTier: strOrNull(c.plan_tier),
    prefix: { content: prefixEntryOf(prefix.content), contentMatch: prefixEntryOf(prefix.content_match), static: prefixEntryOf(prefix.static), staticMatch: prefixEntryOf(prefix.static_match), withMain: bool(prefix.with_main) },
    priority: num(c.priority), rank: numOrNull(c.rank), score: numOrNull(c.score), state: str(c.state),
    term: orNull(c.term, (t) => ({ cancelled: bool(t.cancelled), ended: bool(t.ended), endsAt: isoOrNull(t.ends_at), since: isoOrNull(t.since), trialEndsAt: isoOrNull(t.trial_ends_at), windingDown: bool(t.winding_down) })),
    tier: tierOrZero(c.tier), verdict: str(c.verdict),
    windows: Object.fromEntries(
      Object.entries(rec(c.windows)).map(([name, w]) => {
        const x = rec(w);
        return [name, { ageMs: num(x.age_ms), ahead: numOrNull(x.ahead), observedAt: isoOr(x.observed_at, epoch), reset: isoOrNull(x.reset), source: str(x.source), utilization: num(x.utilization) }];
      }),
    ),
  };
}

function decodeRecordOutcome(v: unknown): RecordOutcome {
  const o = rec(v);
  return {
    accountId: strOrNull(o.account_id), cohortHold: cohortHoldOf(o.cohort_hold),
    completion: orNull(o.completion, (c) => ({
      cacheMapWritten: Array.isArray(c.cache_map_written) ? c.cache_map_written.map(rec).map((m) => ({ accountId: str(m.account_id), depth: num(m.depth), prefixTokens: num(m.prefix_tokens), tier: str(m.tier), ttlMs: num(m.ttl_ms) })) : null,
      cacheReadTokens: numOrNull(c.cache_read_tokens), completedAt: isoOrNull(c.completed_at), freed: bool(c.freed), heldMs: numOrNull(c.held_ms), hit: typeof c.hit === 'boolean' ? c.hit : null, inputTokens: numOrNull(c.input_tokens), promisedTokens: numOrNull(c.promised_tokens), status: numOrNull(c.status),
    })),
    exhaustion: exhaustionOf(o.exhaustion), fromAccountId: strOrNull(o.from_account_id), hold: holdOf(o.hold), inOverage: bool(o.in_overage), kind: decisionKindOf(o.kind) ?? 'placed',
    pinEvents: list(o.pin_events).map(rec).map((e) => ({ kind: str(e.kind), reason: strOrNull(e.reason) })),
    pinWritten: orNull(o.pin_written, (p) => ({ action: str(p.action), epoch: num(p.epoch), hashesStamped: bool(p.hashes_stamped), lane: str(p.lane), refreshStamped: bool(p.refresh_stamped) })),
    refusal: refusalOf(o.refusal), rule: strOrNull(o.rule), rung: placementOf(o.rung),
    shedMarks: Array.isArray(o.shed_marks) ? o.shed_marks.map(rec).map((m) => ({ agentId: str(m.agent_id), keyId: str(m.key_id), model: str(m.model), sessionId: str(m.session_id) })) : null,
    strategy: str(o.strategy), tier: tierOf(o.tier), upstreamModel: strOrNull(o.upstream_model),
  };
}

export function decodeDecisionRecord(v: unknown): DecisionRecord {
  const r = rec(v);
  const q = rec(r.request);
  const lane = rec(q.lane);
  const flags = rec(q.flags);
  const chain = rec(q.chain);
  const s = r.settings;
  return {
    candidates: list(r.candidates).map(decodeRecordCandidate),
    outcome: decodeRecordOutcome(r.outcome),
    pin: orNull(r.pin, (p) => ({
      accountId: strOrNull(p.account_id), createdAt: isoOrNull(p.created_at), epoch: numOrNull(p.epoch), found: str(p.found, 'none'), idleMs: numOrNull(p.idle_ms), incarnation: numOrNull(p.incarnation), lane: strOrNull(p.lane), lastRefreshAt: isoOrNull(p.last_refresh_at), lastUsedAt: isoOrNull(p.last_used_at),
      marked: bool(p.marked), refreshAgeMs: numOrNull(p.refresh_age_ms), released: strings(p.released), shed: orNull(p.shed, (x) => ({ at: isoOr(x.at, epoch), from: str(x.from) })), warmth: strOrNull(p.warmth), warmthCause: strOrNull(p.warmth_cause),
    })),
    // A word this build does not know reads as none, as a poll outcome does. The raw answer still carries it.
    pinReason: PIN_REASON_VALUES.find((x) => x === r.pin_reason) ?? null,
    raw: v,
    request: {
      agentId: str(q.agent_id), at: isoOr(q.at, epoch), attempt: num(q.attempt, 1), attempted: strings(q.attempted), canonicalModel: str(q.canonical_model),
      chain: { boundaries: num(chain.boundaries), staticBoundaries: num(chain.static_boundaries), staticHash: bool(chain.static_hash) }, clientClass: str(q.client_class),
      flags: { contextCompacted: bool(flags.context_compacted), count: bool(flags.count), fast: bool(flags.fast), marker: bool(flags.marker), portable: bool(flags.portable), probe: bool(flags.probe) },
      holds: list(q.holds).map(rec).map((h) => ({ accountId: str(h.account_id), attempt: num(h.attempt, 1), cohort: strOrNull(h.cohort), heldMs: num(h.held_ms) })),
      keyGeneration: num(q.key_generation), keyId: str(q.key_id), lane: { effort: str(lane.effort), speed: str(lane.speed), thinking: str(lane.thinking) }, parentAgentId: strOrNull(q.parent_agent_id), poolId: str(q.pool_id),
      registryRevision: num(q.registry_revision), requestClass: strOrNull(q.request_class), requestId: str(q.request_id), seq: num(q.seq, 1),
    },
    requestId: str(r.request_id),
    seq: num(r.seq, 1),
    settings: orNull(s, (x) => ({
      affinityMode: str(x.affinity_mode), affinityTtlSeconds: num(x.affinity_ttl_seconds), burnWindowSeconds: num(x.burn_window_seconds), inFlightCap: num(x.in_flight_cap), migrateBand: num(x.migrate_band),
      migrateWhenSpent: bool(x.migrate_when_spent), minSharedPrefixTokens: num(x.min_shared_prefix_tokens), overageMoveGraceMinutes: num(x.overage_move_grace_minutes), overagePolicy: str(x.overage_policy), paceFloor: num(x.pace_floor),
      poolUpdatedAt: isoOr(x.pool_updated_at, epoch), preferSubscription: bool(x.prefer_subscription), retryInPlaceSeconds: num(x.retry_in_place_seconds), selectBelowUtilization: numOrNull(x.select_below_utilization), sessionPlacement: str(x.session_placement),
      shedAheadOfPace: numOrNull(x.shed_ahead_of_pace), shedShare: num(x.shed_share), sourceDwellSeconds: num(x.source_dwell_seconds), strategy: str(x.strategy), subagentPlacement: str(x.subagent_placement), warmSessions: str(x.warm_sessions), windDownMinutes: num(x.wind_down_minutes),
    })),
    version: num(r.version),
    walk: list(r.walk).map(rec).map((w) => ({ facts: list(w.facts).map(rec).map((f) => ({ accountId: strOrNull(f.account_id), key: str(f.key), unit: strOrNull(f.unit), value: f.value ?? null })), step: str(w.step), verdict: str(w.verdict) })),
  };
}

const record = <K extends string>(keys: readonly K[], v: unknown, empty: () => Record<K, number>): Record<K, number> => {
  const out = empty();
  const m = rec(v);
  for (const k of keys) out[k] = num(m[k]);
  return out;
};
/** The route rolls reasons up by the name the beta sent, so a name the console does not know stays a key of its own. */
const missReasons = (v: unknown): Record<CacheMissReason, number> => {
  const out = emptyCacheMissReasons();
  for (const [reason, n] of Object.entries(rec(v))) out[reason] = num(n);
  return out;
};
/** The route answers an error code as the upstream or the router named it, so the codes are data, not a closed set. */
const counts = (v: unknown): Record<string, number> => {
  const out = emptyCounts();
  for (const [name, n] of Object.entries(rec(v))) out[name] = num(n);
  return out;
};
const histogram = (v: unknown): number[] => { const h = list(v).map((x) => num(x)); return h.length === BUCKET_COUNT ? h : emptyHistogram(); };
const quartet = (v: unknown) => { const q = rec(v); return { p50: numOrNull(q.p50), p90: numOrNull(q.p90), p95: numOrNull(q.p95), p99: numOrNull(q.p99) }; };

export function decodeAnalyticsGroup(v: unknown): AnalyticsGroup {
  const g = rec(v);
  const h = rec(g.histograms);
  const p = rec(g.percentiles);
  const s = rec(g.savings);
  const t = rec(g.tokens);
  const c = rec(g.cost);
  return {
    attempts: num(g.attempts), cacheHits: num(g.cache_hits), cacheMisses: num(g.cache_misses), cacheMissReasons: missReasons(g.cache_miss_reasons),
    cost: { cacheRead: num(c.cache_read), cacheWrite: num(c.cache_write), input: num(c.input), output: num(c.output) }, costUsd: num(g.cost_usd),
    errors: counts(g.errors), exhausted: num(g.exhausted), expiredBetweenTurns: num(g.expired_between_turns), failed: num(g.failed), failovers: num(g.failovers),
    histograms: { durationMs: histogram(h.duration_ms), outputTps: histogram(h.output_tps), ttfbMs: histogram(h.ttfb_ms), ttftMs: histogram(h.ttft_ms) },
    incomplete: num(g.incomplete), keptWarm: { requests: num(rec(g.kept_warm).requests), tokens: num(rec(g.kept_warm).tokens) }, key: str(g.key), label: str(g.label, str(g.key)),
    percentiles: { durationMs: quartet(p.duration_ms), outputTps: quartet(p.output_tps), ttfbMs: quartet(p.ttfb_ms), ttftMs: quartet(p.ttft_ms) },
    probes: { failed: num(rec(g.probes).failed), migrated: num(rec(g.probes).migrated), ok: num(rec(g.probes).ok), skipped: num(rec(g.probes).skipped) },
    rateLimited: num(g.rate_limited), rejected: num(g.rejected), requests: num(g.requests),
    savings: { grossUsd: num(s.gross_usd), probeCostUsd: num(s.probe_cost_usd), wastedProbeReasons: record(WASTED_PROBE_REASONS, s.wasted_probe_reasons, emptyWastedProbeReasons), wastedProbeUsd: num(s.wasted_probe_usd), wastedProbes: num(s.wasted_probes) },
    succeeded: num(g.succeeded), tokens: { cacheRead: num(t.cache_read), cacheWrite1h: num(t.cache_write_1h), cacheWrite5m: num(t.cache_write_5m), output: num(t.output), thinking: num(t.thinking), uncachedInput: num(t.uncached_input), webFetch: num(t.web_fetch), webSearch: num(t.web_search) }, uncached: num(g.uncached),
  };
}
export const decodeAnalyticsPoint = (v: unknown): AnalyticsPoint => ({ at: isoOr(rec(v).at, epoch), groups: list(rec(v).groups).map(decodeAnalyticsGroup) });

export const decodeBurnPoint = (v: unknown): BurnPoint => {
  const p = rec(v);
  return {
    at: isoOr(p.at, epoch),
    cachedTokens: num(p.cached_tokens),
    fiveHourShare: numOrNull(p.five_hour_share),
    group: str(p.group),
    sevenDayShare: numOrNull(p.seven_day_share),
    tokens: num(p.tokens),
    uncachedTokens: num(p.uncached_tokens),
    unit: burnUnit(p.unit),
    usd: num(p.usd),
  };
};
export const decodeBurn = (v: unknown): { bucketSeconds: number; points: BurnPoint[] } => ({ bucketSeconds: num(rec(v).bucket_seconds, 3_600), points: list(rec(v).points).map(decodeBurnPoint) });
export const decodeRewrites = (v: unknown): { moves: Record<string, number>; points: RewritePoint[] } => ({
  moves: numberMap(rec(v).moves),
  points: list(rec(v).points).map(rec).map((p) => ({ at: isoOr(p.at, epoch), rewriteTokens: num(p.rewrite_tokens) })),
});

export function decodePlacementRow(v: unknown): PlacementRow {
  const r = rec(v);
  return { cacheReadTokens: num(r.cache_read_tokens), hitShare: numOrNull(r.hit_share), hits: num(r.hits), placements: num(r.placements), promisedTokens: num(r.promised_tokens), rung: str(r.rung) };
}
export function decodeMemberBudget(v: unknown): MemberBudget {
  const m = rec(v);
  return {
    accountId: str(m.account_id), budgetPeriod: BUDGET_PERIOD_VALUES.find((x) => x === str(m.budget_period)) ?? 'month', budgetUsd: num(m.budget_usd),
    kind: ACCOUNT_KIND_VALUES.find((x) => x === str(m.kind)) ?? 'oauth', periodEnd: isoOrNull(m.period_end), poolId: str(m.pool_id),
    spentUsd: numOrNull(m.spent_usd),
  };
}

export function decodeAccountRollups(v: unknown): AccountRollup[] {
  return list(rec(v).accounts).map(rec).flatMap((a) => list(a.hours).map(rec).map((h) => ({
    accountId: str(a.account_id), cooldownSeconds: num(h.cooldown_seconds), hour: isoOr(h.hour, epoch), overageSeconds: num(h.overage_seconds),
    peak: { '5h': numOrNull(h.utilization_5h_max), '7d': numOrNull(h.utilization_7d_max), '7d_fable': numOrNull(h.utilization_7d_oi_max) },
    pins: { created: num(h.pins_created), migratedIn: num(h.pins_migrated_in), migratedOut: num(h.pins_migrated_out) },
  })));
}

export function decodeDashboard(v: unknown): Overview {
  const d = rec(v);
  return { activePins: num(rec(d.sessions).active_pins), cost24h: num(d.cost_24h), errorRate24h: numOrNull(d.error_rate_24h), hourly: list(d.hourly).map(rec).map((h) => ({ at: isoOr(h.at, epoch), cost: num(h.cost), errors: num(h.errors), requests: num(h.requests), tokens: num(h.tokens) })), requests24h: num(d.requests_24h), tokens24h: num(d.tokens_24h), tokensCached24h: num(d.tokens_cached_24h), topErrorCode: strOrNull(d.top_error_code) };
}

const companion = (v: unknown): Session['companion'] => (str(v) === 'reporting' ? 'reporting' : str(v) === 'silent' ? 'silent' : 'none');
export const AGENT_STATE_VALUES = ['busy', 'idle', 'parked'] as const;
const agentState = (v: unknown): Session['keepalive']['state'] => AGENT_STATE_VALUES.find((s) => s === str(v)) ?? null;
export const GUARD_VERDICT_VALUES = ['auxiliary', 'foreign', 'rearm', 'restart'] as const;
const guardVerdict = (v: unknown): GuardVerdict | null => GUARD_VERDICT_VALUES.find((x) => x === str(v)) ?? null;

export const BURN_UNIT_VALUES = ['usd', 'window_share'] as const satisfies readonly BurnUnit[];
const burnUnit = (v: unknown): BurnUnit | null => BURN_UNIT_VALUES.find((u) => u === str(v)) ?? null;

export function decodeSession(v: unknown): Session {
  const s = rec(v);
  const k = rec(s.keepalive);
  return { accountId: strOrNull(s.account_id), agentId: strOrNull(s.agent_id), agents: num(s.agents), burn: numOrNull(s.burn), burnAt: isoOrNull(s.burn_at), burnUnit: burnUnit(s.burn_unit), cacheTtlMs: numOrNull(s.cache_ttl_ms), companion: companion(s.companion), companionSince: isoOrNull(s.companion_since), createdAt: isoOrNull(s.created_at), epoch: numOrNull(s.epoch), forkDepth: numOrNull(s.fork_depth), forkedFrom: strOrNull(s.forked_from), historical: bool(s.historical), keepalive: { lastGuard: guardVerdict(k.last_guard), netFiveHourShare: numOrNull(k.net_five_hour_share), netUsd: numOrNull(k.net_usd), nextProbeAt: isoOrNull(k.next_probe_at), state: agentState(k.state) }, keyGeneration: num(s.key_generation), keyId: str(s.key_id), lastInputTokens: numOrNull(s.last_input_tokens), lastRefreshAt: isoOrNull(s.last_refresh_at), lastRequestAt: isoOrNull(s.last_request_at), lastUsedAt: isoOrNull(s.last_used_at) ?? isoOrNull(s.last_request_at), model: strOrNull(s.model), pinned: bool(s.pinned), requests: num(s.requests), sessionId: str(s.session_id), shedFrom: strOrNull(s.shed_from) };
}

export const RECIPE_REASON_VALUES = ['affinity_too_short', 'awaiting_cache_evidence', 'budget', 'cap_reached', 'cold_probes', 'cooled', 'evicted', 'expired', 'idle_limit', 'ledger_behind', 'net_negative', 'overage', 'paused', 'pool_exhausted', 'probe_error', 'quota_exhausted', 'reauth', 'retrying', 'storage_limit', 'unsupported_shape'] as const;
export const RECIPE_STATE_VALUES = ['armed', 'held', 'probing'] as const;
export function decodeRecipe(v: unknown): KeepaliveRecipe {
  const r = rec(v);
  return { accountId: strOrNull(r.account_id), agentId: str(r.agent_id), keyId: str(r.key_id), lastGuard: guardVerdict(r.last_guard), lastRequestAt: isoOrNull(r.last_request_at), model: str(r.model), netFiveHourShare: num(r.net_five_hour_share), netUsd: num(r.net_usd), nextProbeAt: isoOrNull(r.next_probe_at), poolId: str(r.pool_id), reason: RECIPE_REASON_VALUES.find((x) => x === str(r.reason)) ?? null, sessionId: str(r.session_id), state: RECIPE_STATE_VALUES.find((x) => x === str(r.state)) ?? 'armed', ttlSeconds: num(r.ttl_seconds) };
}

export function decodeKeepaliveOverview(v: unknown): KeepaliveOverview {
  const o = rec(v);
  const p = rec(o.probes_24h);
  return { agents: { busy: num(rec(o.agents).busy), idle: num(rec(o.agents).idle), parked: num(rec(o.agents).parked) }, busGaps: num(o.bus_gaps), expiredBetweenTurns24h: num(o.expired_between_turns_24h), netSaved7dUsd: num(o.net_saved_7d_usd), probes24h: { failed: num(p.failed), migrated: num(p.migrated), ok: num(p.ok), skipped: num(p.skipped) } };
}

export const WINDOW_ID_VALUES = ['5h', '7d', '7d_oi'] as const;

const priceCells = (m: Rec): FastPrice => ({ cacheReadPerM: num(m.cache_read), cacheWrite1hPerM: num(m.cache_write_1h), cacheWrite5mPerM: num(m.cache_write_5m), inputPerM: num(m.input), outputPerM: num(m.output) });
const price = (model: string, m: Rec): ModelPrice => ({ ...priceCells(m), fast: m.fast && typeof m.fast === 'object' ? priceCells(rec(m.fast)) : null, model });
/**
 * The instant a table's free-text `effective` date parses to, or null. A text that is not a date is kept as sent rather
 * than throwing, which would fail the whole settings answer and every page that reads it. Only text opening with an
 * ISO date counts, since `Date.parse` reads some free text as a date ("version 3" as March 2001).
 */
export const effectiveDateOf = (effective: string): string | null => {
  const parsed = /^\d{4}-\d{2}-\d{2}/.test(effective) ? Date.parse(effective) : Number.NaN;
  return Number.isNaN(parsed) ? null : new Date(parsed).toISOString();
};
/**
 * A table's date as the router sent it, free text as sent. A date-only `effective` reads in UTC, the zone it parses
 * in: it is midnight UTC, which the browser's own zone would show as the day before west of UTC. A date with a time
 * names an instant, which reads in the browser's zone through `fmtDate` like every other instant.
 */
export const effectiveLabel = (effective: string): string => {
  const at = effectiveDateOf(effective);
  if (at === null) return effective;
  return /^\d{4}-\d{2}-\d{2}$/.test(effective) ? new Date(at).toLocaleDateString('en-US', { day: '2-digit', month: 'short', timeZone: 'UTC', year: 'numeric' }) : fmtDate(at);
};
/** The `api_prices` envelope: its rows keyed by model in the file's order, the date from the file it carries. */
export const decodePricing = (envelope: unknown): Settings['apiPrices'] => {
  const content = rec(rec(envelope).content);
  const effective = str(content.effective, '');
  return { effective, effectiveDate: effectiveDateOf(effective), models: Object.entries(rec(content.models)).map(([model, row]) => price(model, rec(row))), source: str(rec(envelope).source) };
};

const TYPE_KEYS: [keyof TypeMultiplier, string][] = [['cacheRead', 'cache_read'], ['cacheWrite1h', 'cache_write_1h'], ['cacheWrite5m', 'cache_write_5m'], ['input', 'input'], ['output', 'output']];
/** What the console says in place of a cost model it cannot read, which then prices nothing. */
const COSTS_UNREADABLE = "The cost model's answer could not be read.";
/** The empty model: it sizes no tier and weighs no model. */
const emptyCosts = (): SubscriptionCosts => ({
  capacity: {},
  effective: '',
  modelWeight: {},
  modelWindows: {},
  overrides: {},
  peak: { capacityFactor: { fiveHour: 1, sevenDay: 1 }, fromHour: 0, toHour: 0, weekdays: false, zone: '' },
  typeMultiplier: { cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: 0, input: 0, output: 0 },
  unit: { definedBy: { band: '', tier: '', window: '' }, unitsPerWindow: 0 },
});
// A fallback on a mistyped member would price a share the router never charges, so the cost model is read strictly.
// The throw never leaves `readCosts`: a throw out of the settings answer would fail every page that reads it, so the
// answer carries the empty model and a problem instead.
class Malformed extends Error {}
const must = <T>(ok: boolean, v: T): T => {
  if (!ok) throw new Malformed();
  return v;
};
const isMap = (v: unknown): v is Rec => v !== null && typeof v === 'object' && !Array.isArray(v);
const map = (v: unknown): Rec => must(isMap(v), v as Rec);
const finite = (v: unknown): number => must(typeof v === 'number' && Number.isFinite(v), v as number);
const finiteOrNull = (v: unknown): number | null => (v === null ? null : finite(v));
const text = (v: unknown): string => must(typeof v === 'string', v as string);
const flag = (v: unknown): boolean => must(typeof v === 'boolean', v as boolean);
const byKey = <T>(v: unknown, of: (row: unknown) => T): Record<string, T> => Object.fromEntries(Object.entries(map(v)).map(([key, row]) => [key, of(row)]));
// Absent and null both leave an override's member out, as the file's optional members read.
const absent = (v: unknown): boolean => v === undefined || v === null;
// An override names only what it changes, so a member it leaves out stays out rather than reading as zero.
const override = (v: unknown): SubscriptionCosts['overrides'][string] => {
  const o = map(v);
  const named = absent(o.type_multiplier) ? null : map(o.type_multiplier);
  const typeMultiplier: Partial<TypeMultiplier> | null = named === null ? null : Object.fromEntries(TYPE_KEYS.filter(([, wire]) => !absent(named[wire])).map(([key, wire]) => [key, finite(named[wire])]));
  return { ...(typeMultiplier === null ? {} : { typeMultiplier }), ...(absent(o.weight_by_window) ? {} : { weightByWindow: byKey(o.weight_by_window, finite) }) };
};
/**
 * Every member the model requires is present and of its type: a capacity and a weight may be null, an override's
 * multiplier block and each multiplier in it absent or null, its `weight_by_window` block absent or null but never a
 * null weight inside it, and nothing else.
 */
const strictCosts = (content: unknown): SubscriptionCosts => {
  const c = map(content);
  const unit = map(c.unit);
  const definedBy = map(unit.defined_by);
  const peak = map(c.peak);
  const factor = map(peak.capacity_factor);
  const multiplier = map(c.type_multiplier);
  return {
    capacity: byKey(c.capacity, (t) => ({ fiveHour: finiteOrNull(map(t).five_hour), sevenDay: finiteOrNull(map(t).seven_day) })),
    effective: text(c.effective),
    modelWeight: byKey(c.model_weight, finiteOrNull),
    modelWindows: byKey(c.model_windows, (w) => ({ capacityFactor: finite(map(w).capacity_factor), of: text(map(w).of) })),
    overrides: byKey(c.overrides, override),
    peak: { capacityFactor: { fiveHour: finite(factor.five_hour), sevenDay: finite(factor.seven_day) }, fromHour: finite(peak.from_hour), toHour: finite(peak.to_hour), weekdays: flag(peak.weekdays), zone: text(peak.zone) },
    typeMultiplier: { cacheRead: finite(multiplier.cache_read), cacheWrite1h: finite(multiplier.cache_write_1h), cacheWrite5m: finite(multiplier.cache_write_5m), input: finite(multiplier.input), output: finite(multiplier.output) },
    unit: { definedBy: { band: text(definedBy.band), tier: text(definedBy.tier), window: text(definedBy.window) }, unitsPerWindow: finite(unit.units_per_window) },
  };
};
/** The model, or null when a member is missing or of the wrong type. */
const readCosts = (content: unknown): SubscriptionCosts | null => {
  try {
    return strictCosts(content);
  } catch (e) {
    if (e instanceof Malformed) return null;
    throw e;
  }
};
/** The subscription cost model from the envelope's `content`; a content that is absent or cannot be read is the empty model. */
export const decodeSubscriptionCosts = (content: unknown): SubscriptionCosts => (content === undefined ? emptyCosts() : (readCosts(content) ?? emptyCosts()));
/**
 * The settings answer's `subscription_costs` envelope. An answer without it reads as the empty model with no problem;
 * one whose envelope cannot be read reads as the empty model with the problem the cost views show.
 */
const decodeCostsEnvelope = (envelope: unknown): Settings['subscriptionCosts'] => {
  if (envelope === undefined) return { ...emptyCosts(), problem: null, source: '' };
  const costs = isMap(envelope) && typeof envelope.source === 'string' ? readCosts(envelope.content) : null;
  return costs === null ? { ...emptyCosts(), problem: COSTS_UNREADABLE, source: str(rec(envelope).source) } : { ...costs, problem: null, source: str(rec(envelope).source) };
};

export function decodeSettings(v: unknown): Settings {
  const s = rec(v);
  const k = rec(s.keepalive);
  const l = rec(s.listeners);
  const store = rec(s.storage);
  return {
    about: { profile: str(rec(s.about).profile), version: str(rec(s.about).version) },
    access: { adminConfigured: bool(rec(s.access).admin_configured, true) },
    apiPrices: decodePricing(s.api_prices),
    keepalive: { maxAgents: num(k.max_agents), maxRetainedBytes: num(k.max_retained_bytes), maxSessions: num(k.max_sessions) },
    listeners: { metrics: str(l.metrics), proxy: str(l.proxy) },
    retention: { bodyCaptureMaxHours: num(rec(s.retention).body_capture_max_hours) },
    sessions: { expireAfterSeconds: num(rec(s.sessions).expire_after_seconds) },
    storage: { apiPricesPath: strOrNull(store.api_prices_path), ledgerPath: str(store.ledger_path), path: str(store.path), subscriptionCostsPath: strOrNull(store.subscription_costs_path) },
    subscriptionCosts: decodeCostsEnvelope(s.subscription_costs),
    usage: { cacheTtlSeconds: num(rec(s.usage).cache_ttl_seconds) },
  };
}

const liveAttempt = (v: unknown): LiveAttempt => { const a = rec(v); return { accountId: str(a.account_id), cooldownSet: a.cooldown_set ? { reason: str(rec(a.cooldown_set).reason), until: isoOr(rec(a.cooldown_set).until, epoch) } : null, index: num(a.index) - 1, outcome: str(a.outcome) === 'in_flight' ? 'in_flight' : outcomeOf(a.outcome), startedAt: isoOr(a.started_at, epoch), upstreamStatus: numOrNull(a.upstream_status), windows: attemptWindows(a.windows) }; };
const livePhases = (v: unknown) => { const p = rec(v); const at = (k: string) => isoOrNull(p[k]) ?? undefined; return { finishedAt: at('finished_at'), firstContentAt: at('first_content_at'), headAt: at('head_at'), receivedAt: isoOr(p.received_at, epoch), selectedAt: at('selected_at'), streamEndAt: at('stream_end_at'), writtenAt: at('written_at') }; };
export function decodeLiveRequest(v: unknown): LiveRequest {
  const r = rec(v);
  return { accountId: strOrNull(r.account_id), agentId: str(r.agent_id, 'main'), attempts: list(r.attempts).map(liveAttempt), costUsd: num(r.cost_usd), errorCode: strOrNull(r.error_code), id: str(r.id), keyId: strOrNull(r.key_id), kind: REQUEST_KIND_VALUES.find((k) => k === str(r.kind)) ?? 'inference', model: str(r.model), outputTps: numOrNull(r.output_tps), phases: livePhases(r.phases), poolId: strOrNull(r.pool_id), preview: strOrNull(r.preview), sessionId: strOrNull(r.session_id), state: str(r.state) === 'in_flight' ? 'in_flight' : 'finished', status: numOrNull(r.status), transport: str(r.transport) === 'sse' ? 'sse' : 'json', usage: usageOf(r.usage), userAgent: strOrNull(r.user_agent) };
}
export const LIVE_METRIC_QUALITY_VALUES = ['derived', 'estimated', 'measured'] as const;
const liveMetric = (v: unknown): LiveMetric => ({ quality: LIVE_METRIC_QUALITY_VALUES.find((q) => q === str(rec(v).quality)) ?? 'measured', series: list(rec(v).series).map(numOrNull), value: numOrNull(rec(v).value) });
export function decodeLiveTick(v: unknown): LiveTick {
  const m = rec(rec(v).metrics);
  return { at: isoOr(rec(v).at, epoch), metrics: { activeStreams: liveMetric(m.active_streams), costPerMinute: liveMetric(m.cost_per_minute), errorRate: liveMetric(m.error_rate), outputTps: liveMetric(m.output_tps), requestsPerSecond: liveMetric(m.requests_per_second), ttfbP50: liveMetric(m.ttfb_p50), ttfbP95: liveMetric(m.ttfb_p95), ttfbP99: liveMetric(m.ttfb_p99) } };
}
export const decodeLiveSnapshot = (v: unknown): LiveSnapshot => ({ cursor: num(rec(v).cursor), inFlight: list(rec(v).in_flight).map(decodeLiveRequest), recent: list(rec(v).recent).map(decodeLiveRequest), tick: decodeLiveTick(rec(v).tick) });
export function decodeLiveDelta(v: unknown): LiveDelta | null {
  const d = rec(v);
  const cursor = num(d.cursor);
  const id = str(d.id);
  switch (str(d.type)) {
    case 'started': return { cursor, request: decodeLiveRequest(d.request), type: 'started' };
    case 'selected': return { accountId: str(d.account_id), at: isoOr(d.at, epoch), attempt: liveAttempt(d.attempt), cursor, failed: d.failed ? liveAttempt(d.failed) : undefined, id, type: 'selected' };
    case 'head': return { at: isoOr(d.at, epoch), attempt: liveAttempt(d.attempt), cursor, id, status: num(d.status), type: 'head' };
    case 'progress': return { at: isoOr(d.at, epoch), cursor, id, outputTps: numOrNull(d.output_tps), type: 'progress', usage: usageOf(d.usage) };
    case 'preview': return { chunk: str(d.chunk), cursor, id, type: 'preview' };
    case 'finished': return { at: isoOr(d.at, epoch), attempts: list(d.attempts).map(liveAttempt), costUsd: num(d.cost_usd), cursor, errorCode: strOrNull(d.error_code), id, phases: livePhases(d.phases), status: numOrNull(d.status), type: 'finished', usage: usageOf(d.usage) };
    case 'tick': return { cursor, tick: decodeLiveTick(d.tick), type: 'tick' };
    default: return null;
  }
}

const encodeModelRules = (rules: ModelRule[]) => rules.map((r) => ({ not: r.not, pattern: r.pattern }));
const encodeTtlRules = (r: TtlRules) => ({ main: r.main, models: r.models.map((m) => ({ main: m.main, model: m.model, subagent: m.subagent })), subagent: r.subagent });

export const encodeAccountPatch = (p: AccountPatch) => ({ cancelled: p.cancelled, subscription_ends_at: p.subscriptionEndsAt === null ? null : Date.parse(p.subscriptionEndsAt) });
export const encodeKey = (i: KeyInput) => ({
  auto_mode_server: i.autoModeServer, body_capture_hours: i.bodyCaptureHours, enabled: i.enabled,
  ...(i.expiresAt === undefined ? {} : { expires_at: i.expiresAt === null ? null : Date.parse(i.expiresAt) }),
  keepalive_opt_out: i.keepaliveOptOut, label: i.name, long_context_twins: i.longContextTwins,
  model_rules: encodeModelRules(i.modelRules), model_rules_mode: i.modelRulesMode, notes: i.notes, pool_id: i.poolId,
});
export const encodePool = (i: PoolInput) => ({
  affinity_mode: i.affinityMode, affinity_ttl_seconds: i.affinityTtlSeconds, auto_mode_server: i.autoModeServer, body_capture_hours: i.bodyCaptureHours,
  cache_diagnostics: i.cacheDiagnostics,
  cache_ttl: { enabled: i.cacheTtl.enabled, inject_when_absent: i.cacheTtl.injectWhenAbsent, metered: encodeTtlRules(i.cacheTtl.metered), plan: encodeTtlRules(i.cacheTtl.plan) },
  enabled: i.status === 'active', model_rules: encodeModelRules(i.modelRules),
  keepalive: { enabled: i.keepalive.enabled, max_idle_seconds: i.keepalive.maxIdleSeconds, max_parked_seconds: i.keepalive.maxParkedSeconds, scheduled_horizon_seconds: i.keepalive.scheduledHorizonSeconds },
  label: i.name, members: i.members.map((m) => ({ account_id: m.accountId, budget_daily_usd: m.budgetDailyUsd, budget_monthly_usd: m.budgetMonthlyUsd, priority: m.priority })), shed_ahead_of_pace: i.shedAheadOfPace,
  min_shared_prefix_tokens: i.minSharedPrefixTokens, overage_policy: i.overagePolicy, prefer_subscription: i.preferSubscription, select_below_utilization: i.selectBelowUtilization,
  session_placement: i.sessionPlacement, strategy: i.strategy, subagent_placement: i.subagentPlacement,
  burn_window_seconds: i.burnWindowSeconds, in_flight_cap: i.inFlightCap, migrate_when_spent: i.migrateWhenSpent, overage_move_grace_minutes: i.overageMoveGraceMinutes, pace_floor: i.paceFloor,
  retry_in_place_seconds: i.retryInPlaceSeconds, shed_share: i.shedShare, source_dwell_seconds: i.sourceDwellSeconds, warm_sessions: i.warmSessions, wind_down_minutes: i.windDownMinutes,
});
