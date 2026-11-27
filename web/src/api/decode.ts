import type {
  Account, AccountCatalog, AccountKind, AccountRollup, AgentRequests, AllowanceReset, AnalyticsGroup, AnalyticsPoint, ApiKey, AttemptOutcome, AttemptResponse, AttentionItem, BudgetPeriod, BurnPoint, BurnUnit, CacheMissReason, CacheTtl, CapturedBodies, CatalogModel,
  CohortHold, CostRevision, CostSplit, CreatedApiKey, DecisionKind, DecisionRecord, DecisionSummary, Exhaustion, FastLaneReading, FeatureAccountIds, FeatureCall, FeatureCallPage, FeatureWord, FundedSum, GuardVerdict, InputTransformations, KeepaliveOverview, KeepaliveRecipe, KeyFeatureAccount, KeyFigures, KeyModels, Lifecycle, LimitResets, LimitResetsRead, MemberBudget, ModelPrice, ModelRule, ModelRulesMode,
  MoveCause, Overview, PairingConfirmed, PairingSession, Placement, PlacementRow, Pool, PoolCacheTtl, PrefixEntry, PriceCells, PriceRevision, QuotaHistory, QuotaPoll, QuotaSnapshot, Range, RateLimitWindow, RecipeCause, Reconciliation, ReconciliationsAnswer, RecordCandidate, RecordHold, RecordOutcome, RecordSettings, Refusal, RefusalCategoriesAnswer, ReplayOutcome, RequestEvent, RequestLog, RequestPage, RequestPhases, RequestShape, ResetUse, RewritesAnswer,
  RoutingEvent, RoutingLogPage, RoutingLogRow, RuleCooldown, Score, SelectionTrace, Session, SessionDay, SessionInfo, SessionSpend, SessionSpendPart, Settings, Spend, Subscription, SubscriptionCosts, SummaryFact, TtlRules, TypeMultiplier, UnusedAllowanceAnswer, WastedProbeReason, WindowId, WindowSample,
} from './types';
import type { AgentContext, BurnFigures, MoveFigures, PoolPin, ShedStamp } from './types';
import type { StreamCut } from './types';
import type { Condition, Guardrail, GuardrailAttachment, GuardrailCounts, GuardrailMatch, GuardrailMode, GuardrailPreset, GuardrailPresetState, GuardrailRef, GuardrailTestResult } from './types';
import { ACCOUNT_KIND_VALUES, AFFINITY_MODE_VALUES, ATTEMPT_OUTCOME_VALUES, ATTENTION_KIND_VALUES, ATTENTION_PAGE_VALUES, ATTENTION_SUBJECT_VALUES, AUTO_MODE_VALUES, BUDGET_PERIOD_VALUES, BURN_UNIT_VALUES, BY_KIND_VALUES, CLASSIFIER_VALUES, COMPANION_PERMISSION_VALUES, CONDITION_OPERATOR_VALUES, CONDITION_SUBJECT_VALUES, DECISION_KIND_VALUES, DIAGNOSTICS_SOURCE_VALUES, ELIGIBILITY_VALUES, FACT_ENTITY_VALUES, FACT_UNIT_VALUES, FEATURE_OUTCOME_VALUES, FEATURE_WORD_VALUES, GUARDRAIL_MODE_VALUES, GUARD_VERDICT_VALUES, KEY_FEATURE_MODE_VALUES, LIFECYCLE_VALUES, MEMBER_FIT_VALUES, MESSAGE_SELECTOR_VALUES, MODEL_RULES_MODE_VALUES, OVERAGE_POLICY_VALUES, PARK_KIND_VALUES, PIN_REASON_VALUES, PLACEMENT_RUNG_VALUES, POLL_OUTCOME_VALUES, POOL_STATUS_VALUES, POOL_STRATEGY_VALUES, RECIPE_CAUSE_VALUES, RECIPE_STATE_VALUES, REQUEST_KIND_VALUES, REQUEST_OUTCOME_VALUES, RESET_GRANT_STATE_VALUES, RESET_OUTCOME_VALUES, RETURN_TO_PLAN_VALUES, SESSION_PLACEMENT_VALUES, SUBAGENT_PLACEMENT_VALUES, TEST_VERDICT_VALUES, WARM_SESSIONS_VALUES } from './types';
import { CACHE_TTL_DEFAULTS, METERED_TTL_DEFAULTS, PLAN_TTL_DEFAULTS, POOL_AFFINITY_DEFAULTS, POOL_ENGINE_DEFAULTS, type AccountPatch, type KeyInput, type PoolInput } from './client';
import type { GuardrailInput, GuardrailTestInput } from './client';
import { WASTED_PROBE_REASONS, emptyCacheMissReasons, emptyWastedProbeReasons } from '@/lib/cache';
import { emptyCounts } from '@/lib/counts';
import { earlierHolderLabel, fmtMinute, formatIn } from '@/lib/format';
import { BUCKET_COUNT, emptyHistogram } from '@/lib/histogram';
import { accountStatusOf, planWordLabel } from '@/lib/quota';
import { inWindowOrder, periodHoursOf, storedFableKey, windowKeyId, windowLabelOf } from '@/lib/windows';

type Rec = Record<string, unknown>;
export const rec = (v: unknown): Rec => (v && typeof v === 'object' ? (v as Rec) : {});
const orNull = <T,>(v: unknown, read: (r: Rec) => T): T | null => (v === null || typeof v !== 'object' ? null : read(rec(v)));
const isMap = (v: unknown): v is Rec => v !== null && typeof v === 'object' && !Array.isArray(v);
export const list = (v: unknown): unknown[] => (Array.isArray(v) ? v : []);
const str = (v: unknown, fallback = ''): string => (typeof v === 'string' ? v : fallback);
export const strOrNull = (v: unknown): string | null => (typeof v === 'string' ? v : null);
export const num = (v: unknown, fallback = 0): number => (typeof v === 'number' && Number.isFinite(v) ? v : fallback);
export const numOrNull = (v: unknown): number | null => (typeof v === 'number' && Number.isFinite(v) ? v : null);
const bool = (v: unknown, fallback = false): boolean => (typeof v === 'boolean' ? v : fallback);
const boolOrNull = (v: unknown): boolean | null => (typeof v === 'boolean' ? v : null);
export const strings = (v: unknown): string[] => list(v).filter((x): x is string => typeof x === 'string');
export const oneOf = <T extends string>(values: readonly T[], v: unknown): T | null => values.find((x) => x === v) ?? null;
export const perFeature = <T>(of: (feature: FeatureWord) => T): Record<FeatureWord, T> => Object.fromEntries(FEATURE_WORD_VALUES.map((f) => [f, of(f)])) as Record<FeatureWord, T>;
const accountKind = (v: unknown): AccountKind => oneOf(ACCOUNT_KIND_VALUES, v) ?? 'oauth';
const budgetPeriod = (v: unknown): BudgetPeriod => oneOf(BUDGET_PERIOD_VALUES, v) ?? 'month';
const rulesMode = (v: unknown): ModelRulesMode => oneOf(MODEL_RULES_MODE_VALUES, v) ?? 'extend';
const featureAccountIds = (v: unknown): FeatureAccountIds => {
  const r = rec(v);
  return perFeature((f) => strOrNull(r[f]));
};
/** A mode this build does not know, and an `account` without an id, read as the store's default, the pool's account. */
const keyFeatureAccount = (v: unknown): KeyFeatureAccount => {
  const r = rec(v);
  const mode = oneOf(KEY_FEATURE_MODE_VALUES, r.mode);
  const accountId = strOrNull(r.account_id);
  if (mode === 'account' && accountId !== null) return { accountId, mode };
  return { accountId: null, mode: mode === 'none' ? 'none' : 'inherit' };
};
/** A map the route keys by data (a move reason, a model id), with every entry that is not a number dropped. */
const numberMap = (v: unknown): Record<string, number> => Object.fromEntries(Object.entries(rec(v)).filter((e): e is [string, number] => numOrNull(e[1]) !== null));
const numberMaps = (v: unknown): Record<string, Record<string, number>> => Object.fromEntries(Object.entries(rec(v)).map(([k, x]) => [k, numberMap(x)]));
/** A map the route keys by data whose value may be null (a window the cost model could not price), each value read
 *  through `numOrNull`, so an unpriced window decodes null rather than zero. */
const nullableNumberMap = (v: unknown): Record<string, number | null> => Object.fromEntries(Object.entries(rec(v)).map(([k, x]) => [k, numOrNull(x)]));
/** Dollars or Pro-week figures by token kind, as the routes split a group's figure. */
const costSplit = (v: unknown): CostSplit => {
  const c = rec(v);
  return { cacheRead: num(c.cache_read), cacheWrite: num(c.cache_write), input: num(c.input), output: num(c.output) };
};

/** The wire carries Unix milliseconds; every page formats ISO strings. */
export const iso = (ms: number): string => new Date(ms).toISOString();
export const isoOrNull = (ms: unknown): string | null => (typeof ms === 'number' && Number.isFinite(ms) ? iso(ms) : null);
const isoOr = (ms: unknown, fallback: string): string => isoOrNull(ms) ?? fallback;
const epoch = '1970-01-01T00:00:00.000Z';

function decodeWindow(w: Rec): RateLimitWindow | null {
  const id = windowKeyId(str(w.id));
  if (id === null) return null;
  return {
    burn: numOrNull(w.burn), demand: numOrNull(w.demand), horizon: isoOrNull(w.horizon), id, label: windowLabelOf(id), lead: numOrNull(w.lead), observedAt: isoOr(w.observed_at, epoch), periodHours: periodHoursOf(id),
    projection: numOrNull(w.projection), resetsAt: isoOrNull(w.resets_at), utilization: numOrNull(w.utilization),
  };
}

/** A usage-based seat's spend window, null on every other account and on a row from a router without the member. */
function decodeSpend(v: unknown): Spend | null {
  return orNull(v, (s) => ({
    amounts: orNull(s.amounts, (a) => ({ currency: str(a.currency, 'USD'), exponent: num(a.exponent, 2), limitMinor: numOrNull(a.limit_minor), polledAt: isoOr(a.polled_at, epoch), usedMinor: num(a.used_minor) })),
    observedAt: isoOr(s.observed_at, epoch),
    // A source this build does not know names no header's instant, so the reset reads as unconfirmed.
    resetSource: str(s.reset_source) === 'head' ? 'head' : 'estimated',
    resetsAt: isoOrNull(s.resets_at),
    utilization: numOrNull(s.utilization),
  }));
}

/** The account's state and its flags folded onto the one badge the pages show. */
function accountStatus(a: Rec, cooldowns: Account['cooldowns']): Account['status'] {
  const state = str(a.state);
  const flag = state === 'reauth_required' || state === 'funding_required' ? state : state === 'unpaired' || state === 'disconnected' ? 'unpaired' : null;
  return accountStatusOf(flag, bool(a.enabled, true), cooldowns);
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
    modelRulesMode: rulesMode(k.model_rules_mode),
    poolId: str(k.pool_id),
    rules: list(k.rules).map(rec).map((r) => ({ cooldowns: list(r.cooldowns).map(decodeRuleCooldown), level: str(r.level) === 'pool' ? 'pool' : 'key', ...decodeModelRule(r) })),
  };
}

/** The typed subscription the route's `profile` and `subscription` records fold into; null when both are absent. */
export function decodeSubscription(profile: Rec | null, sub: Rec | null): Subscription | null {
  if (profile === null && sub === null) return null;
  return {
    billingType: str(profile?.billing_type),
    endsAt: isoOrNull(sub?.ends_at),
    organizationType: str(profile?.organization_type),
    plan: strOrNull(sub?.plan),
    planDisplayName: strOrNull(profile?.plan_display_name),
    rateLimitTier: str(sub?.tier ?? profile?.rate_limit_tier),
    since: isoOrNull(sub?.since),
    trialEndsAt: isoOrNull(sub?.trial_ends_at),
  };
}

const windowIds = (v: unknown): WindowId[] => strings(v).map(windowKeyId).filter((id): id is WindowId => id !== null);

export function decodeLimitResets(v: unknown): LimitResets | null {
  return orNull(v, (r) => ({
    cooldownUntil: isoOrNull(r.cooldown_until),
    eligible: bool(r.eligible),
    fableWeekShare: numOrNull(r.fable_week_share),
    grants: list(r.grants).map(rec).map((g) => ({
      blocking: windowIds(g.blocking), clears: windowIds(g.clears), endsAt: isoOr(g.ends_at, epoch), id: str(g.id), kind: str(g.kind), label: str(g.label),
      reason: strOrNull(g.reason), resetsLeft: num(g.resets_left), resetsTotal: num(g.resets_total), startsAt: isoOr(g.starts_at, epoch),
      state: oneOf(RESET_GRANT_STATE_VALUES, g.state) ?? 'unknown', usedAt: isoOrNull(g.used_at),
    })),
    ineligibleReason: strOrNull(r.ineligible_reason),
    left: num(r.left),
    readAt: isoOr(r.read_at, epoch),
    revision: num(r.revision),
  }));
}

export const decodeLimitResetsRead = (v: unknown): LimitResetsRead => ({ error: strOrNull(rec(v).error), limitResets: decodeLimitResets(rec(v).limit_resets) });

export function decodeResetUse(v: unknown): ResetUse {
  const u = rec(v);
  return {
    cleared: windowIds(u.cleared), cooldownUntil: isoOrNull(u.cooldown_until), limitResets: decodeLimitResets(u.limit_resets),
    // A class this console does not know is open, so no page says "not used" about it.
    outcome: oneOf(RESET_OUTCOME_VALUES, u.outcome) ?? 'unsettled',
    reason: strOrNull(u.reason), resetsLeft: numOrNull(u.resets_left), result: str(u.result), weeklyResetsAt: isoOrNull(u.weekly_resets_at),
  };
}

export function decodeAccount(v: unknown): Account {
  const a = rec(v);
  const cooldowns = list(a.cooldowns).map(rec).map((c) => ({ reason: str(c.reason), scope: str(c.scope), setAt: isoOr(c.set_at, epoch), until: isoOr(c.until, epoch), windowId: windowKeyId(str(c.window)) }));
  const windows = inWindowOrder(list(a.windows).map(rec).map(decodeWindow).filter((w): w is RateLimitWindow => w !== null), (w) => w.id);
  const profile = orNull(a.profile, (r) => r);
  const sub = orNull(a.subscription, (r) => r);
  const identity = rec(a.identity);
  const credential = rec(a.credential);
  const subscription = decodeSubscription(profile, sub);
  return {
    canceled: bool(a.canceled),
    companionId: str(a.companion_id),
    cooldowns,
    credential: { accessTokenExpiresAt: isoOrNull(credential.access_expires_at), lastError: strOrNull(credential.last_error), lastRefreshAt: isoOrNull(credential.last_refresh_at), quarantined: bool(credential.quarantined), refreshTokenExpiresAt: isoOrNull(credential.refresh_expires_at) },
    email: strOrNull(identity.email),
    // Kept when the provider switched extra usage off: the reason it gave exists for exactly that account.
    extraUsage: orNull(a.extra_usage, (x) => ({ disabledReason: strOrNull(x.disabled_reason), enabled: bool(x.is_enabled), inUse: bool(x.in_use), monthlyLimitCredits: numOrNull(x.monthly_limit), usedCredits: numOrNull(x.used_credits) })),
    fableWindow: windows.find((w) => w.id === '7d_fable') ?? null,
    fastLane: orNull(a.fast_lane, (f) => ({ input: fastLaneReading(f.input), output: fastLaneReading(f.output) })),
    featureGrants: Array.isArray(a.feature_grants) ? FEATURE_WORD_VALUES.filter((w) => list(a.feature_grants).includes(w)) : null,
    id: str(a.id),
    kind: accountKind(a.kind),
    label: str(a.label),
    limitResets: decodeLimitResets(a.limit_resets),
    overage: orNull(a.overage, (o) => ({ inUse: bool(o.in_use), observedAt: isoOr(o.observed_at, epoch), reset: isoOrNull(o.reset), since: isoOrNull(o.since) })),
    plan: planName(subscription),
    planEndedAt: isoOrNull(a.plan_ended_at),
    pool: orNull(a.pool, (p) => ({ poolId: str(p.pool_id), priority: num(p.priority) })),
    requests: { last24h: num(rec(a.requests).last_24h), last7d: num(rec(a.requests).last_7d), lifetime: num(rec(a.requests).lifetime) },
    // A row whose answer carried none reads 0, which the route refuses as stale, so the editor offers Reload rather
    // than landing over a row it could not vouch for.
    revision: num(a.revision),
    routingEnabled: bool(a.enabled, true),
    slowDown: orNull(a.slow_down, (s) => ({ factor: num(s.factor, 1), holdMs: num(s.hold_ms), ramp: numOrNull(s.ramp), since: isoOrNull(s.since), speed: num(s.speed, 1), stretchMs: num(s.stretch_ms), stretchProjection: numOrNull(s.stretch_projection), thresholdMs: num(s.threshold_ms) })),
    spend: decodeSpend(a.spend),
    status: accountStatus(a, cooldowns),
    subscription,
    usage: orNull(a.usage, (u) => decodeSnapshot(str(a.id), u)),
    usageBased: bool(a.usage_based),
    windows: windows.filter((w) => w.id !== '7d_fable'),
  };
}

function fastLaneReading(v: unknown): FastLaneReading | null {
  return orNull(v, (r) => ({ observedAt: isoOr(r.observed_at, epoch), resetsAt: isoOrNull(r.resets_at), utilization: num(r.utilization) }));
}

/** The plan's name: the profile's display name where it carries one, since claude.ai names the plan there, else the
 *  resolved word's label through one table, a word the table lacks shown as sent, and null with neither, since the tier
 *  names no plan on its own. */
export const planName = (sub: Subscription | null): string | null => (sub === null ? null : (sub.planDisplayName ?? (sub.plan === null ? null : planWordLabel(sub.plan))));

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

export function decodePairingConfirmed(v: unknown): PairingConfirmed {
  const c = rec(v);
  return { accountId: str(c.account_id), firstPollInFlight: bool(c.first_poll_in_flight), status: str(c.status) };
}

/** The history's samples keep the ledger's stored key, which spells a head's 7-day Fable window
 *  `seven_day_overage_included`, so the history reads that key as the Fable series. */
const historyWindowId = (id: string): WindowId | null => windowKeyId(storedFableKey(id));

export function decodeQuotaHistory(v: unknown): QuotaHistory[] {
  const q = rec(v);
  const byId = new Map<WindowId, QuotaHistory>();
  for (const w of list(q.windows).map(rec)) {
    const id = historyWindowId(str(w.id));
    if (id === null) continue;
    const samples = list(w.samples).map(rec).map((s) => ({ at: isoOr(s.at, epoch), utilization: numOrNull(s.utilization) }));
    const seen = byId.get(id);
    // A window the header and the poll both sampled is one series, whatever each called it.
    byId.set(id, seen ? { ...seen, samples: [...seen.samples, ...samples].sort((x, y) => x.at.localeCompare(y.at)) } : { accountId: str(q.account_id), samples, windowId: id });
  }
  return inWindowOrder([...byId.values()], (h) => h.windowId);
}

function decodePoll(v: unknown): QuotaPoll | null {
  return orNull(v, (p) => ({
    backoffSince: isoOrNull(p.backoff_since),
    lastAttemptAt: isoOrNull(p.last_attempt_at),
    lastCleanAt: isoOrNull(p.last_clean_at),
    lastError: strOrNull(p.last_error),
    // An outcome this build does not know reads as a failure rather than as a clean answer.
    lastOutcome: oneOf(POLL_OUTCOME_VALUES, p.last_outcome) ?? 'failed',
    nextAttemptAt: isoOrNull(p.next_attempt_at),
    rateLimitedInARow: num(p.rate_limited_in_a_row),
  }));
}

export function decodeSnapshot(accountId: string, v: unknown): QuotaSnapshot {
  const u = rec(v);
  return { accountId, error: strOrNull(u.error), fetchedAt: isoOrNull(u.fetched_at), heldUntil: isoOrNull(u.held_until), poll: decodePoll(u.poll) };
}

export function decodeAttention(v: unknown): AttentionItem {
  const a = rec(v);
  const subject = rec(a.subject);
  const link = a.link ? rec(a.link) : null;
  // A page this build does not know has no place to open the subject on, so the item draws no link.
  const page = link ? oneOf(ATTENTION_PAGE_VALUES, link.page) : null;
  const raw = str(a.kind);
  const kind = oneOf(ATTENTION_KIND_VALUES, raw);
  return {
    detail: rec(a.detail),
    kind: kind ?? 'unknown',
    // A kind this build does not know names no fix the console can vouch for, so it opens nothing whatever its page.
    link: kind !== null && link && page !== null ? { id: str(link.id), page } : null,
    pools: strings(a.pools),
    rawKind: kind === null ? raw : null,
    since: isoOr(a.since, epoch),
    subject: { id: str(subject.id), label: str(subject.label), type: oneOf(ATTENTION_SUBJECT_VALUES, subject.type) ?? 'pool' },
  };
}

export function decodeReplay(v: unknown): ReplayOutcome {
  const r = rec(v);
  return { landed: num(r.landed), remaining: num(r.remaining) };
}

const keyFigures = (v: unknown): KeyFigures => {
  const f = rec(v);
  return { billedUsd: num(f.billed_usd), cost: num(f.cost_usd), equivalent: numOrNull(f.equivalent), requests: num(f.requests) };
};

const daySum = (r: Rec): FundedSum => ({ billedUsd: num(r.billed_24h), cost: num(r.cost_24h), equivalent: numOrNull(r.equivalent_24h) });

/** A key's status from its stored flags: routing refuses a key that is revoked, expired or switched off, in that order
 *  of finality. */
export const keyStatusOf = (revoked: boolean, expiresAt: string | null, enabled: boolean, now: number): ApiKey['status'] =>
  revoked ? 'revoked' : expiresAt !== null && Date.parse(expiresAt) <= now ? 'expired' : enabled ? 'active' : 'disabled';

export function decodeKey(v: unknown): ApiKey {
  const k = rec(v);
  const expires = isoOrNull(k.expires_at);
  const revoked = isoOrNull(k.revoked_at);
  // A row from a route that reports no `enabled` is taken as on rather than off.
  const enabled = bool(k.enabled, true);
  const features = rec(k.feature_accounts);
  return {
    autoModeServer: boolOrNull(k.auto_mode_server),
    bodyCaptureHours: numOrNull(k.body_capture_hours),
    companionPermissions: COMPANION_PERMISSION_VALUES.filter((p) => list(k.companion_permissions).includes(p)),
    createdAt: isoOr(k.created_at, epoch),
    enabled,
    expiresAt: expires,
    featureAccounts: perFeature((f) => keyFeatureAccount(features[f])),
    guardrails: guardrailRefs(k.guardrails),
    id: str(k.id),
    lastUsedAt: isoOrNull(k.last_used_at),
    lifetime: keyFigures(k.lifetime),
    longContextTwins: bool(k.long_context_twins, true),
    modelRules: list(k.model_rules).map(decodeModelRule),
    modelRulesMode: rulesMode(k.model_rules_mode),
    name: str(k.label),
    notes: strOrNull(k.notes),
    poolGuardrails: guardrailRefs(k.pool_guardrails),
    poolId: str(k.pool_id),
    prefix: str(k.prefix),
    retiredSecretLastUsedAt: isoOrNull(k.retired_secret_last_used_at),
    // A row whose answer carried none reads 0, which the route refuses as stale, so the editor offers Reload rather
    // than landing over a row it could not vouch for.
    revision: num(k.revision),
    sessionsOnRetiredSecret: num(k.sessions_on_retired_secret),
    status: keyStatusOf(revoked !== null, expires, enabled, Date.now()),
    updatedAt: isoOr(k.updated_at, epoch),
    usage7d: { ...keyFigures(k.usage_7d), tokens: num(rec(k.usage_7d).tokens) },
  };
}

/** A created or rotated key: the row, and its secret, which the route answers this once. */
export const decodeCreatedKey = (v: unknown): CreatedApiKey => ({ key: decodeKey(v), secret: String(rec(v).secret) });

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

const decodeRange = (v: unknown): Range => {
  const r = rec(v);
  return { max: num(r.max), min: num(r.min) };
};

/** A pool with no member answers null, and so does a row from a router that resolves nothing. */
export function decodeKeepaliveResolved(v: unknown): Pool['keepaliveResolved'] {
  if (!isMap(v)) return null;
  return {
    breakEvenSeconds: decodeRange(v.break_even_seconds),
    horizonSeconds: decodeRange(v.horizon_seconds),
    idleSeconds: decodeRange(v.idle_seconds),
    idleShare: decodeRange(v.idle_share),
    parkedSeconds: decodeRange(v.parked_seconds),
  };
}

export function decodePool(v: unknown): Pool {
  const p = rec(v);
  const k = rec(p.keepalive);
  return {
    activePins: num(p.active_pins),
    affinityMode: oneOf(AFFINITY_MODE_VALUES, p.affinity_mode) ?? POOL_AFFINITY_DEFAULTS.affinityMode,
    affinityTtlSeconds: num(p.affinity_ttl_seconds),
    autoModeServer: bool(p.auto_mode_server, true),
    bodyCaptureHours: num(p.body_capture_hours),
    burnAverageSeconds: num(p.burn_average_seconds, POOL_ENGINE_DEFAULTS.burnAverageSeconds),
    cacheDiagnostics: bool(p.cache_diagnostics),
    cacheTtl: decodeCacheTtl(p.cache_ttl),
    day24h: daySum(p),
    fableReservationShedding: bool(p.fable_reservation_shedding, POOL_ENGINE_DEFAULTS.fableReservationShedding),
    featureAccounts: featureAccountIds(p.feature_accounts),
    fiveHourCeiling: num(p.five_hour_ceiling, POOL_ENGINE_DEFAULTS.fiveHourCeiling),
    guardrails: guardrailRefs(p.guardrails),
    id: str(p.id),
    inFlightCap: num(p.in_flight_cap, POOL_ENGINE_DEFAULTS.inFlightCap),
    keepalive: {
      enabled: bool(k.enabled), maxIdleSeconds: num(k.max_idle_seconds), maxParkedSeconds: numOrNull(k.max_parked_seconds), scheduledHorizonSeconds: numOrNull(k.scheduled_horizon_seconds),
    },
    keepaliveResolved: decodeKeepaliveResolved(p.keepalive_resolved),
    // A row without the key fields reads as the store's defaults.
    keys: list(p.keys).map(rec).map((k) => ({ enabled: bool(k.enabled, true), id: str(k.id), label: str(k.label), prefix: str(k.prefix) })),
    loadShedding: bool(p.load_shedding, POOL_ENGINE_DEFAULTS.loadShedding),
    members: list(p.members).map(rec).map((m) => ({
      accountId: str(m.account_id),
      activePins: num(m.active_pins),
      budgetDailyUsd: numOrNull(m.budget_daily_usd),
      budgetMonthlyUsd: numOrNull(m.budget_monthly_usd),
      budgetUntil: isoOrNull(m.budget_until),
      burn: numOrNull(m.burn),
      // A row from before the router reported eligibility reads as not servable rather than as servable.
      eligible: oneOf(ELIGIBILITY_VALUES, m.eligible) ?? 'unavailable',
      fableReservation: numOrNull(m.fable_reservation),
      factor: numOrNull(m.factor),
      // The route answers null for an account it cannot find, which is no API-key account either.
      kind: accountKind(m.kind),
      paceScore: numOrNull(m.pace_score),
      priority: num(m.priority),
      // A word outside the list has no rank in the order, so an entry reads it as no answer.
      regimeFits: m.regime_fits == null ? null : list(m.regime_fits).map(rec).map((f) => ({ fit: oneOf(MEMBER_FIT_VALUES, f.fit), regime: strOrNull(f.regime), runwayMs: numOrNull(f.runway_ms) })),
      speed: numOrNull(m.speed),
      spends: list(m.spends).map(rec).map((s) => ({ period: budgetPeriod(s.period), periodEnd: isoOrNull(s.period_end), spentUsd: num(s.spent_usd) })),
    })),
    migrateWhenSpent: bool(p.migrate_when_spent, POOL_ENGINE_DEFAULTS.migrateWhenSpent),
    minSharedPrefixTokens: num(p.min_shared_prefix_tokens, POOL_AFFINITY_DEFAULTS.minSharedPrefixTokens),
    modelRules: list(p.model_rules).map(decodeModelRule),
    movesPerHour: numberMap(p.moves_per_hour),
    name: str(p.label),
    overageMoveGraceMinutes: num(p.overage_move_grace_minutes, POOL_ENGINE_DEFAULTS.overageMoveGraceMinutes),
    overagePolicy: oneOf(OVERAGE_POLICY_VALUES, p.overage_policy) ?? 'prefer_plan',
    preferSubscription: bool(p.prefer_subscription),
    requests24h: num(p.requests_24h),
    retryInPlaceSeconds: num(p.retry_in_place_seconds, POOL_ENGINE_DEFAULTS.retryInPlaceSeconds),
    // A mode this build does not know reads as the default, which returns a session as the router did before the field.
    returnToPlan: oneOf(RETURN_TO_PLAN_VALUES, p.return_to_plan) ?? POOL_ENGINE_DEFAULTS.returnToPlan,
    returnToPlanDwellSeconds: num(p.return_to_plan_dwell_seconds, POOL_ENGINE_DEFAULTS.returnToPlanDwellSeconds),
    // A row whose answer carried none reads 0, which the route refuses as stale, so the editor offers Reload rather
    // than landing over a row it could not vouch for.
    revision: num(p.revision),
    sessionPlacement: oneOf(SESSION_PLACEMENT_VALUES, p.session_placement) ?? POOL_AFFINITY_DEFAULTS.sessionPlacement,
    shedCap: num(p.shed_cap, POOL_ENGINE_DEFAULTS.shedCap),
    shedDwellSeconds: num(p.shed_dwell_seconds, POOL_ENGINE_DEFAULTS.shedDwellSeconds),
    slowDown: bool(p.slow_down, POOL_ENGINE_DEFAULTS.slowDown),
    slowDownHoldSeconds: num(p.slow_down_hold_seconds, POOL_ENGINE_DEFAULTS.slowDownHoldSeconds),
    slowDownStretchSeconds: num(p.slow_down_stretch_seconds, POOL_ENGINE_DEFAULTS.slowDownStretchSeconds),
    status: oneOf(POOL_STATUS_VALUES, p.status) ?? 'active',
    strategy: oneOf(POOL_STRATEGY_VALUES, p.strategy) ?? 'pace',
    subagentPlacement: oneOf(SUBAGENT_PLACEMENT_VALUES, p.subagent_placement) ?? POOL_AFFINITY_DEFAULTS.subagentPlacement,
    thinkingDiagnostics: bool(p.thinking_diagnostics),
    traffic24h: list(p.traffic_24h).map(rec).map((b) => ({ at: isoOr(b.at, epoch), billedUsd: num(b.billed_usd), costUsd: num(b.cost_usd), equivalent: numOrNull(b.equivalent), failed: num(b.failed), requests: num(b.requests), tokens: num(b.tokens) })),
    updatedAt: isoOr(p.updated_at, epoch),
    warmSessions: oneOf(WARM_SESSIONS_VALUES, p.warm_sessions) ?? POOL_ENGINE_DEFAULTS.warmSessions,
    // An entry that is no object says nothing the dialog can draw, so it is left out rather than drawn blank.
    warnings: list(p.warnings).filter((w) => w !== null && typeof w === 'object').map(rec).map((w) => ({ code: str(w.code), fields: strings(w.fields), text: str(w.text) })),
    weeklyCeiling: num(p.weekly_ceiling, POOL_ENGINE_DEFAULTS.weeklyCeiling),
    windDownMinutes: num(p.wind_down_minutes, POOL_ENGINE_DEFAULTS.windDownMinutes),
  };
}

const outcomeOf = (v: unknown): AttemptOutcome => oneOf(ATTEMPT_OUTCOME_VALUES, v) ?? 'upstream_error';
const attemptWindows = (v: unknown) => list(v).map(rec).flatMap((w) => { const id = windowKeyId(str(w.id)); return id === null ? [] : [{ id, resetsAt: isoOrNull(w.resets_at), utilization: numOrNull(w.utilization) }]; });
const usageOf = (v: unknown) => { const u = rec(v); return { cacheRead: num(u.cache_read), cacheWrite1h: num(u.cache_write_1h), cacheWrite5m: num(u.cache_write_5m), input: num(u.input), output: num(u.output), thinking: num(u.thinking), webFetch: num(u.web_fetch), webSearch: num(u.web_search) }; };
const phasesOf = (v: unknown, at: string): RequestPhases => { const p = rec(v); return { finishedAt: isoOrNull(p.finished_at), firstContentAt: isoOrNull(p.first_content_at), headAt: isoOrNull(p.head_at), receivedAt: isoOr(p.received_at, at), selectedAt: isoOrNull(p.selected_at), streamEndAt: isoOrNull(p.stream_end_at), writtenAt: isoOrNull(p.written_at) }; };
const shapeOf = (v: unknown): RequestShape => { const s = rec(v); return { contextEdits: strings(s.context_edits), effort: strOrNull(s.effort), maxTokens: numOrNull(s.max_tokens), messageCount: num(s.message_count), requestedServiceTier: strOrNull(s.requested_service_tier), requestedSpeed: strOrNull(s.requested_speed), systemBlocks: num(s.system_blocks), temperature: numOrNull(s.temperature), thinkingBudget: numOrNull(s.thinking_budget), thinkingType: strOrNull(s.thinking_type), toolCount: num(s.tool_count) }; };
/**
 * The blocks an answer dropped, per key. An empty object stays an empty map apart from a null, since the none count
 * and the drawer's words read an answer that listed nothing apart from one without the member.
 */
const inputTransformationsOf = (v: unknown): InputTransformations | null =>
  orNull(v, (m) => Object.fromEntries(Object.entries(m).map(([key, x]) => [key, { blocks: num(rec(x).blocks), firstMessage: num(rec(x).first_message) }])));
const responseOf = (v: unknown): AttemptResponse => { const r = rec(v); return { clearedInputTokens: numOrNull(r.cleared_input_tokens), inferenceGeo: strOrNull(r.inference_geo), inputTransformations: inputTransformationsOf(r.input_transformations), refusalCategory: strOrNull(r.refusal_category), refusalRecommendedModel: strOrNull(r.refusal_recommended_model), servedModel: strOrNull(r.served_model), speed: strOrNull(r.speed), stopReason: strOrNull(r.stop_reason), stopSequence: strOrNull(r.stop_sequence) }; };

/**
 * The obstacles the router names for a candidate it passed over: the eligibility checks (`attempted`, `budget`,
 * `budget_unknown`, `cooldown`, `credits`, `operator`, `overage`, `subscription_ended`) and the partitions (`at_cap`,
 * `no_fit`, `ranked`, `spent`, `winding_down`). The trace keeps any other name as sent.
 */
export const SKIP_OBSTACLE_VALUES = ['at_cap', 'attempted', 'budget', 'budget_unknown', 'cooldown', 'credits', 'no_fit', 'operator', 'overage', 'ranked', 'spent', 'subscription_ended', 'winding_down'] as const;
const tierOf = (v: unknown): SelectionTrace['tier'] => {
  const t = list(v);
  return t.length === 3 && t.every((x) => numOrNull(x) !== null) ? [num(t[0]), num(t[1]), num(t[2])] : null;
};
/** A rung the drawer has no words for reads as no placement rather than as a verdict it cannot explain. */
const placementOf = (v: unknown): Placement | null =>
  orNull(v, (p) => {
    const rung = oneOf(PLACEMENT_RUNG_VALUES, p.rung);
    return rung === null ? null : { depth: num(p.depth), prefixTokens: num(p.prefix_tokens), rung };
  });
/** A row from before the waits were recorded reads none, so its hold says it waited and not for what. */
const cohortHoldOf = (v: unknown): CohortHold | null =>
  orNull(v, (h) => ({ cohort: strOrNull(h.cohort), heldMs: num(h.held_ms), waitedOn: strings(h.waited_on), waits: list(h.waits).map(rec).map((w) => ({ heldMs: num(w.held_ms), reason: str(w.reason) })) }));
/** A stamp written before the pin carried a reason reads as a round's move, as the route documents it. */
const shedOf = (v: unknown): ShedStamp | null => orNull(v, (x) => ({ at: isoOr(x.at, epoch), from: str(x.from), reason: str(x.reason, 'round') }));
function decodeSelection(v: unknown): SelectionTrace | null {
  return orNull(v, (s) => ({
    cacheRead: numOrNull(s.cache_read), cohortHold: cohortHoldOf(s.cohort_hold), moved: strOrNull(s.moved), placement: placementOf(s.placement), renewed: bool(s.renewed),
    skipped: list(s.skipped).map(rec).map((k) => ({ accountId: str(k.account_id), obstacle: str(k.obstacle) })), strategy: str(s.strategy), tier: tierOf(s.tier),
  }));
}

export function decodeRequest(v: unknown): RequestLog {
  const r = rec(v);
  return {
    accountId: strOrNull(r.account_id), agentId: strOrNull(r.agent_id), agentType: strOrNull(r.agent_type), at: isoOr(r.at, epoch),
    attempts: list(r.attempts).map(rec).map((a) => ({ accountId: str(a.account_id), accountKind: accountKind(a.account_kind), cacheMissReason: strOrNull(a.cache_miss_reason), cooldownSet: a.cooldown_set ? { reason: str(rec(a.cooldown_set).reason), until: isoOr(rec(a.cooldown_set).until, epoch) } : null, durationMs: numOrNull(a.duration_ms), entryExpired: boolOrNull(a.entry_expired), index: num(a.index) - 1, issuedDigest: strOrNull(a.issued_digest), outcome: outcomeOf(a.outcome), overageInUse: boolOrNull(a.overage_in_use), previousInputTokens: numOrNull(a.previous_input_tokens), response: responseOf(a.response), selection: decodeSelection(a.selection), serviceTier: strOrNull(a.service_tier), startedAt: isoOr(a.started_at, epoch), tier: strOrNull(a.tier), upstreamMessageId: strOrNull(a.upstream_message_id), upstreamRequestId: strOrNull(a.upstream_request_id), upstreamStatus: numOrNull(a.upstream_status), usageBased: bool(a.usage_based), windows: attemptWindows(a.windows) })),
    autoMode: oneOf(AUTO_MODE_VALUES, r.auto_mode),
    cacheMarker: boolOrNull(r.cache_marker),
    canonicalModel: str(r.canonical_model), capturedBodies: bool(r.captured_bodies), classifier: oneOf(CLASSIFIER_VALUES, r.classifier), client: strOrNull(r.client), clientContract: strOrNull(r.client_contract), clientDrift: strOrNull(r.client_drift), compaction: strOrNull(r.compaction), contextCompacted: strOrNull(r.context_compacted),
    costEquivalent: numOrNull(r.cost_equivalent),
    costShares: orNull(r.cost_shares, nullableNumberMap), costUsd: num(r.cost_usd),
    diagnosticsSource: oneOf(DIAGNOSTICS_SOURCE_VALUES, r.diagnostics_source),
    durationMs: numOrNull(r.duration_ms), errorCode: strOrNull(r.error_code), fallbackDigest: strOrNull(r.fallback_digest),
    forkDepth: numOrNull(r.fork_depth), forkedFrom: strOrNull(r.forked_from),
    guardrailId: strOrNull(r.guardrail_id), guardrails: list(r.guardrails).map(decodeGuardrailMatch),
    held: cohortHoldOf(r.held), id: str(r.id),
    keyGeneration: num(r.key_generation), keyId: strOrNull(r.key_id),
    kind: oneOf(REQUEST_KIND_VALUES, r.kind) ?? 'inference', model: str(r.model),
    outcome: oneOf(REQUEST_OUTCOME_VALUES, r.outcome) ?? 'failed',
    phases: phasesOf(r.phases, isoOr(r.at, epoch)),
    poolId: strOrNull(r.pool_id), prevToolMs: numOrNull(r.prev_tool_ms), refused: bool(r.refused), requestClass: strOrNull(r.request_class), responseTruncated: bool(r.response_truncated), sessionId: strOrNull(r.session_id), shape: shapeOf(r.shape), shareReason: r.share_reason === 'client_classifier' ? 'client_classifier' : null,
    status: numOrNull(r.status), storedTokens: { input: numOrNull(rec(r.usage).input), output: numOrNull(rec(r.usage).output) }, thinkingDiagnosticsSource: oneOf(DIAGNOSTICS_SOURCE_VALUES, r.thinking_diagnostics_source), tier: strOrNull(r.tier), transport: str(r.transport) === 'sse' ? 'sse' : 'json', ttfbMs: numOrNull(r.ttfb_ms), ttftMs: numOrNull(r.ttft_ms), turnOrigin: strOrNull(r.turn_origin), usage: usageOf(r.usage), userAgent: strOrNull(r.user_agent),
  };
}

export const decodeRequestPage = (v: unknown): RequestPage => ({ landedThrough: num(rec(v).landed_through), nextCursor: strOrNull(rec(v).next_cursor), requests: list(rec(v).requests).map(decodeRequest) });
const decodeFeatureCall = (v: unknown): FeatureCall => {
  const c = rec(v);
  return {
    accountId: str(c.account_id), durationMs: num(c.duration_ms), errorCode: strOrNull(c.error_code), feature: oneOf(FEATURE_WORD_VALUES, c.feature) ?? 'artifacts', id: str(c.id), keyId: str(c.key_id), method: str(c.method),
    outcome: oneOf(FEATURE_OUTCOME_VALUES, c.outcome) ?? 'failed', path: str(c.path), receivedAt: isoOr(c.received_at, epoch), status: numOrNull(c.status),
  };
};
export const decodeFeatureCallPage = (v: unknown): FeatureCallPage => ({ featureCalls: list(rec(v).feature_calls).map(decodeFeatureCall), nextCursor: strOrNull(rec(v).next_cursor) });
// The four counts are the route's own; nothing here measures the request or the response it passes through.
export const decodeBodies = (v: unknown): CapturedBodies => { const b = rec(v); return { request: b.request, requestBytesSeen: num(b.request_bytes_seen), requestId: str(b.request_id), response: b.response, responseBytesSeen: num(b.response_bytes_seen), responseBytesStored: num(b.response_bytes_stored), responseTruncated: bool(b.response_truncated) }; };
export const decodeRequestEvent = (v: unknown): RequestEvent => ({ accountId: strOrNull(rec(v).account_id), at: isoOr(rec(v).at, epoch), detail: orNull(rec(v).detail, (d) => d), keyId: strOrNull(rec(v).key_id), kind: str(rec(v).kind), model: strOrNull(rec(v).model), sessionId: strOrNull(rec(v).session_id) });
/** The detail's `stream_cut` member, null where the member is null or absent. */
export function decodeStreamCut(raw: unknown): StreamCut | null {
  return orNull(raw, (c) => ({ at: isoOr(c.at, epoch), attempt: num(c.attempt), bytesRelayed: num(c.bytes_relayed), code: str(c.code), text: str(c.text), upstreamRequestId: strOrNull(c.upstream_request_id) }));
}

const decisionKindOf = (v: unknown): DecisionKind | null => oneOf(DECISION_KIND_VALUES, v);
const scoreOf = (v: unknown): Score | null => orNull(v, (s) => ({ accountId: str(s.account_id), score: num(s.score), unit: str(s.unit) }));
/** A hold with no reason is a version 1 record's, which held for a slot alone. */
const holdOf = (v: unknown): RecordHold | null =>
  orNull(v, (h) => ({
    accountId: str(h.account_id), boundMs: num(h.bound_ms), cohort: strOrNull(h.cohort), cycleMs: numOrNull(h.cycle_ms), factor: numOrNull(h.factor), reason: str(h.reason, 'cap'), speed: numOrNull(h.speed),
    stretchMs: numOrNull(h.stretch_ms), thresholdMs: numOrNull(h.threshold_ms), until: isoOrNull(h.until), waitsFor: strOrNull(h.waits_for),
  }));
/** Both shapes the router writes read through one object, the other shape's members null. */
const causeOf = (v: unknown): MoveCause | null => orNull(v, (c) => ({ cause: strOrNull(c.cause), reason: strOrNull(c.reason), scope: strOrNull(c.scope), state: strOrNull(c.state), window: strOrNull(c.window) }));
const exhaustionOf = (v: unknown): Exhaustion | null => orNull(v, (x) => ({ deadline: isoOrNull(x.deadline), kind: str(x.kind), obstacle: strOrNull(x.obstacle), reason: strOrNull(x.reason), status: numOrNull(x.status), window: strOrNull(x.window) }));
const refusalOf = (v: unknown): Refusal | null => orNull(v, (x) => ({ code: str(x.code), kind: str(x.kind), reason: str(x.reason) }));
/** A value the console cannot draw as a number or a word drops its pair rather than drawing a guess. */
const factOf = (v: unknown): SummaryFact[] => {
  const f = rec(v);
  if (typeof f.value !== 'number' && typeof f.value !== 'string') return [];
  const entity = orNull(f.entity, (e) => {
    const kind = oneOf(FACT_ENTITY_VALUES, e.kind);
    return kind === null ? null : { id: str(e.id), kind };
  });
  return [{ entity, key: str(f.key), unit: oneOf(FACT_UNIT_VALUES, f.unit), value: f.value }];
};

export function decodeDecisionSummary(v: unknown): DecisionSummary {
  const r = rec(v);
  const by = rec(r.by);
  return {
    type: 'decision',
    accountId: strOrNull(r.account_id), agentId: strOrNull(r.agent_id), at: isoOr(r.at, epoch), attempt: num(r.attempt, 1), attemptCount: numOrNull(r.attempt_count),
    by: { kind: oneOf(BY_KIND_VALUES, by.kind) ?? 'none', tier: tierOf(by.tier), value: str(by.value) },
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
  if (r.type === 'decision') return typeof r.request_id === 'string' && numOrNull(r.seq) !== null && decisionKindOf(r.kind) !== null ? [decodeDecisionSummary(r)] : [];
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
    binding: strOrNull(c.binding),
    budget: list(c.budget).map(rec).map((b) => ({ budgetUsd: num(b.budget_usd), observedAt: isoOrNull(b.observed_at), period: str(b.period), periodEnd: isoOrNull(b.period_end), periodStart: isoOrNull(b.period_start), spentUsd: numOrNull(b.spent_usd) })),
    burn: numberMap(c.burn),
    cooldowns: list(c.cooldowns).map(rec).map((x) => ({ blocking: bool(x.blocking), reason: str(x.reason), scope: str(x.scope), until: isoOr(x.until, epoch), window: str(x.window) })),
    credits: orNull(c.credits, (x) => ({ blocked: strOrNull(x.blocked), disabledReason: strOrNull(x.disabled_reason), enabled: bool(x.enabled), observedAt: isoOr(x.observed_at, epoch), utilization: numOrNull(x.utilization) })),
    enabled: bool(c.enabled), factor: numOrNull(c.factor), fit: strOrNull(c.fit), inFlight: num(c.in_flight), inFlightCap: num(c.in_flight_cap), inOverage: bool(c.in_overage), incarnation: num(c.incarnation), kind: accountKind(c.kind),
    obstacle: orNull(c.obstacle, (o) => ({ name: str(o.name), reason: strOrNull(o.reason), scope: strOrNull(o.scope), until: isoOrNull(o.until), window: strOrNull(o.window) })),
    overage: orNull(c.overage, (o) => ({ observedAt: isoOr(o.observed_at, epoch), reset: isoOrNull(o.reset), since: isoOr(o.since, epoch), source: str(o.source) })),
    pace: orNull(c.pace, (p) => ({ burn: numOrNull(p.burn), deadline: isoOrNull(p.deadline), end: isoOrNull(p.end), fableReservation: numOrNull(p.fable_reservation), hours: numOrNull(p.hours), remaining: num(p.remaining), reset: isoOrNull(p.reset), score: num(p.score), window: strOrNull(p.window) })),
    partition: strOrNull(c.partition), planTier: strOrNull(c.plan_tier),
    prefix: { content: prefixEntryOf(prefix.content), contentMatch: prefixEntryOf(prefix.content_match), static: prefixEntryOf(prefix.static), staticMatch: prefixEntryOf(prefix.static_match), withMain: bool(prefix.with_main) },
    priority: num(c.priority), rank: numOrNull(c.rank), runwayMs: numOrNull(c.runway_ms), score: numOrNull(c.score), state: str(c.state),
    term: orNull(c.term, (t) => ({ canceled: bool(t.canceled), ended: bool(t.ended), endsAt: isoOrNull(t.ends_at), since: isoOrNull(t.since), trialEndsAt: isoOrNull(t.trial_ends_at), windingDown: bool(t.winding_down) })),
    tier: tierOrZero(c.tier), verdict: str(c.verdict),
    windows: Object.fromEntries(
      Object.entries(rec(c.windows)).map(([name, w]) => {
        const x = rec(w);
        return [name, { ageMs: num(x.age_ms), ahead: numOrNull(x.ahead), demand: numOrNull(x.demand), horizon: isoOrNull(x.horizon), observedAt: isoOr(x.observed_at, epoch), projection: numOrNull(x.projection), reserved: numOrNull(x.reserved), reset: isoOrNull(x.reset), source: str(x.source), utilization: num(x.utilization) }];
      }),
    ),
  };
}

function decodeRecordOutcome(v: unknown): RecordOutcome {
  const o = rec(v);
  return {
    accountId: strOrNull(o.account_id), cause: causeOf(o.cause), cohortHold: cohortHoldOf(o.cohort_hold),
    completion: orNull(o.completion, (c) => ({
      cacheMapWritten: Array.isArray(c.cache_map_written) ? c.cache_map_written.map(rec).map((m) => ({ accountId: str(m.account_id), depth: num(m.depth), prefixTokens: num(m.prefix_tokens), tier: str(m.tier), ttlMs: num(m.ttl_ms) })) : null,
      cacheReadTokens: numOrNull(c.cache_read_tokens), cacheWriteTokens: numOrNull(c.cache_write_tokens), completedAt: isoOrNull(c.completed_at), freed: bool(c.freed), heldMs: numOrNull(c.held_ms), hit: boolOrNull(c.hit), inputTokens: numOrNull(c.input_tokens), inputTransformations: inputTransformationsOf(c.input_transformations), promisedTokens: numOrNull(c.promised_tokens), status: numOrNull(c.status),
    })),
    exhaustion: exhaustionOf(o.exhaustion), fromAccountId: strOrNull(o.from_account_id), hold: holdOf(o.hold), inOverage: bool(o.in_overage), kind: decisionKindOf(o.kind) ?? 'placed',
    markCleared: orNull(o.mark_cleared, (m) => ({ markedAt: isoOr(m.marked_at, epoch), markedOn: str(m.marked_on), markedWindow: str(m.marked_window), projection: numOrNull(m.projection), reason: str(m.reason) })),
    pinEvents: list(o.pin_events).map(rec).map((e) => ({ kind: str(e.kind), reason: strOrNull(e.reason) })),
    pinWritten: orNull(o.pin_written, (p) => ({ action: str(p.action), epoch: num(p.epoch), hashesStamped: bool(p.hashes_stamped), lane: str(p.lane), refreshStamped: bool(p.refresh_stamped) })),
    refusal: refusalOf(o.refusal),
    reservation: orNull(o.reservation, (x) => ({ accountId: str(x.account_id), burn: numberMap(x.burn), expiresAt: isoOr(x.expires_at, epoch), rewrite: numberMap(x.rewrite) })),
    rule: strOrNull(o.rule), rung: placementOf(o.rung),
    shedMarks: Array.isArray(o.shed_marks) ? o.shed_marks.map(rec).map((m) => ({ agentId: str(m.agent_id), keyId: str(m.key_id), model: str(m.model), sessionId: str(m.session_id) })) : null,
    strategy: str(o.strategy), tier: tierOf(o.tier), upstreamModel: strOrNull(o.upstream_model),
  };
}

/**
 * The settings by the record's version: a record names the switch, the two ceilings, the cap, the average and the
 * dwell, and a version 1 record the three old names of the last three with its three removed settings, which `legacy`
 * keeps so an old record still shows what it read. The slow-down's three and the return-to-plan's two read null on a
 * record written before them, so the card draws them as not recorded.
 */
function decodeRecordSettings(x: Rec, version: number): RecordSettings {
  const shared = {
    affinityMode: str(x.affinity_mode), affinityTtlSeconds: num(x.affinity_ttl_seconds), fableReservationShedding: boolOrNull(x.fable_reservation_shedding), inFlightCap: num(x.in_flight_cap),
    migrateWhenSpent: bool(x.migrate_when_spent),
    minSharedPrefixTokens: num(x.min_shared_prefix_tokens), overageMoveGraceMinutes: num(x.overage_move_grace_minutes), overagePolicy: str(x.overage_policy), poolUpdatedAt: isoOr(x.pool_updated_at, epoch),
    preferSubscription: bool(x.prefer_subscription), retryInPlaceSeconds: num(x.retry_in_place_seconds), returnToPlan: strOrNull(x.return_to_plan), returnToPlanDwellSeconds: numOrNull(x.return_to_plan_dwell_seconds),
    sessionPlacement: str(x.session_placement), slowDown: boolOrNull(x.slow_down), slowDownHoldSeconds: numOrNull(x.slow_down_hold_seconds),
    slowDownStretchSeconds: numOrNull(x.slow_down_stretch_seconds), strategy: str(x.strategy), subagentPlacement: str(x.subagent_placement), warmSessions: str(x.warm_sessions), windDownMinutes: num(x.wind_down_minutes),
  };
  if (version === 1) {
    return {
      ...shared, burnAverageSeconds: num(x.burn_window_seconds), legacy: { paceFloor: num(x.pace_floor), selectBelowUtilization: numOrNull(x.select_below_utilization), shedAheadOfPace: numOrNull(x.shed_ahead_of_pace) },
      fiveHourCeiling: null, loadShedding: null, shedCap: num(x.shed_share), shedDwellSeconds: num(x.source_dwell_seconds), weeklyCeiling: null,
    };
  }
  // A record stored while the pool held one ceiling for every window carries `window_ceiling` alone, and its decision
  // read that fraction on both kinds.
  const single = num(x.window_ceiling);
  return {
    ...shared, burnAverageSeconds: num(x.burn_average_seconds), fiveHourCeiling: num(x.five_hour_ceiling, single), legacy: null, loadShedding: bool(x.load_shedding), shedCap: num(x.shed_cap),
    shedDwellSeconds: num(x.shed_dwell_seconds), weeklyCeiling: num(x.weekly_ceiling, single),
  };
}

export function decodeDecisionRecord(v: unknown): DecisionRecord {
  const r = rec(v);
  const version = num(r.version);
  const q = rec(r.request);
  const lane = rec(q.lane);
  const flags = rec(q.flags);
  const chain = rec(q.chain);
  const s = r.settings;
  return {
    candidates: list(r.candidates).map(decodeRecordCandidate),
    outcome: decodeRecordOutcome(r.outcome),
    pin: orNull(r.pin, (p) => ({
      accountId: strOrNull(p.account_id), createdAt: isoOrNull(p.created_at), cycleMs: numOrNull(p.cycle_ms), epoch: numOrNull(p.epoch), found: str(p.found, 'none'), idleMs: numOrNull(p.idle_ms), incarnation: numOrNull(p.incarnation), lane: strOrNull(p.lane), lastRefreshAt: isoOrNull(p.last_refresh_at), lastUsedAt: isoOrNull(p.last_used_at),
      marked: bool(p.marked), markedAt: isoOrNull(p.marked_at), markedOn: strOrNull(p.marked_on), markedWindow: strOrNull(p.marked_window), refreshAgeMs: numOrNull(p.refresh_age_ms), released: strings(p.released),
      shed: shedOf(p.shed), slowDownUntil: isoOrNull(p.slow_down_until), warmth: strOrNull(p.warmth), warmthCause: strOrNull(p.warmth_cause),
    })),
    // A word this build does not know reads as none, as a poll outcome does. The raw answer still carries it.
    pinReason: oneOf(PIN_REASON_VALUES, r.pin_reason),
    raw: v,
    request: {
      agentId: str(q.agent_id), at: isoOr(q.at, epoch), attempt: num(q.attempt, 1), attempted: strings(q.attempted), bodyTokens: numOrNull(q.body_tokens), canonicalModel: str(q.canonical_model),
      chain: { boundaries: num(chain.boundaries), staticBoundaries: num(chain.static_boundaries), staticHash: bool(chain.static_hash) }, clientClass: str(q.client_class),
      flags: { contextCompacted: bool(flags.context_compacted), count: bool(flags.count), fast: bool(flags.fast), marker: bool(flags.marker), portable: bool(flags.portable), probe: bool(flags.probe) },
      holds: list(q.holds).map(rec).map((h) => ({ accountId: str(h.account_id), attempt: num(h.attempt, 1), cohort: strOrNull(h.cohort), heldMs: num(h.held_ms), ranOut: bool(h.ran_out) })),
      keyGeneration: num(q.key_generation), keyId: str(q.key_id), lane: { effort: str(lane.effort), speed: str(lane.speed), thinking: str(lane.thinking) }, parentAgentId: strOrNull(q.parent_agent_id), poolId: str(q.pool_id),
      registryRevision: num(q.registry_revision), requestClass: strOrNull(q.request_class), requestId: str(q.request_id),
      seq: num(q.seq, 1),
      // A record before version 3 wrote the session under `routing_session_id`.
      sessionId: strOrNull(q.session_id ?? (version !== null && version <= 2 ? q.routing_session_id : null)),
    },
    requestId: str(r.request_id),
    seq: num(r.seq, 1),
    settings: orNull(s, (x) => decodeRecordSettings(x, version)),
    version,
    walk: list(r.walk).map(rec).map((w) => ({ facts: list(w.facts).map(rec).map((f) => ({ accountId: strOrNull(f.account_id), key: str(f.key), unit: strOrNull(f.unit), value: f.value ?? null })), step: str(w.step), verdict: str(w.verdict) })),
  };
}

const wastedReasons = (v: unknown): Record<WastedProbeReason, number> => {
  const out = emptyWastedProbeReasons();
  const m = rec(v);
  for (const k of WASTED_PROBE_REASONS) out[k] = num(m[k]);
  return out;
};
const countsOnto = (seed: Record<string, number>, v: unknown): Record<string, number> => {
  for (const [name, n] of Object.entries(rec(v))) seed[name] = num(n);
  return seed;
};
/** The route rolls reasons up by the name the beta sent, so a name the console does not know stays a key of its own. */
const missReasons = (v: unknown): Record<CacheMissReason, number> => countsOnto(emptyCacheMissReasons(), v);
/** The route answers an error code as the upstream or the router named it, so the codes are data, not a closed set. */
const counts = (v: unknown): Record<string, number> => countsOnto(emptyCounts(), v);
const histogram = (v: unknown): number[] => { const h = list(v).map((x) => num(x)); return h.length === BUCKET_COUNT ? h : emptyHistogram(); };
const quartet = (v: unknown) => { const q = rec(v); return { p50: numOrNull(q.p50), p90: numOrNull(q.p90), p95: numOrNull(q.p95), p99: numOrNull(q.p99) }; };

export function decodeAnalyticsGroup(v: unknown): AnalyticsGroup {
  const g = rec(v);
  const h = rec(g.histograms);
  const p = rec(g.percentiles);
  const t = rec(g.tokens);
  const keyId = strOrNull(g.key_id);
  const keyCreatedAt = numOrNull(g.key_created_at);
  return {
    attempts: num(g.attempts), billed: costSplit(g.billed), billedUsd: num(g.billed_usd),
    cacheEquivalent: orNull(g.cache_equivalent, (e) => ({ read: num(e.read), write: num(e.write) })),
    cacheHits: num(g.cache_hits), cacheMisses: num(g.cache_misses), cacheMissReasonHits: counts(g.cache_miss_reason_hits), cacheMissReasons: missReasons(g.cache_miss_reasons), clientCanceled: num(g.client_canceled),
    cost: costSplit(g.cost), costEquivalent: numOrNull(g.cost_equivalent), costUsd: num(g.cost_usd),
    costShares: orNull(g.cost_shares, nullableNumberMap),
    equivalentSplit: orNull(g.equivalent_split, costSplit),
    errors: counts(g.errors), exhausted: num(g.exhausted), expiredBetweenTurns: num(g.expired_between_turns), failed: num(g.failed), failovers: num(g.failovers),
    histograms: { durationMs: histogram(h.duration_ms), outputTps: histogram(h.output_tps), ttfbMs: histogram(h.ttfb_ms), ttftMs: histogram(h.ttft_ms) },
    incomplete: num(g.incomplete),
    inputTransformations: counts(g.input_transformations), inputTransformationsMoved: num(g.input_transformations_moved), inputTransformationsNone: num(g.input_transformations_none), inputTransformationsReported: num(g.input_transformations_reported),
    keptWarm: {
      planRequests: num(rec(g.kept_warm).plan_requests), planTokens1h: num(rec(g.kept_warm).plan_tokens_1h), planTokens5m: num(rec(g.kept_warm).plan_tokens_5m),
      requests: num(rec(g.kept_warm).requests), tokens1h: num(rec(g.kept_warm).tokens_1h), tokens5m: num(rec(g.kept_warm).tokens_5m),
    },
    key: str(g.key), keyCreatedAt, keyId,
    // The route's label of an earlier holder is fixed to one zone, so the console builds it in the viewer's zone from the
    // holder's members, and keeps the wire's label where the route alone knows it (a live key's id, the reserved id's
    // `unattributed`).
    label: keyId !== null && keyCreatedAt !== null ? earlierHolderLabel(keyId, keyCreatedAt) : str(g.label, str(g.key)),
    percentiles: { durationMs: quartet(p.duration_ms), outputTps: quartet(p.output_tps), ttfbMs: quartet(p.ttfb_ms), ttftMs: quartet(p.ttft_ms) },
    planOutputTokens: numOrNull(g.plan_output_tokens),
    planRequests: numOrNull(g.plan_requests),
    probeCostUsd: num(g.probe_cost_usd),
    probeEquivalent: orNull(g.probe_equivalent, (e) => ({ cost: num(e.cost), wasted: num(e.wasted) })),
    probes: { failed: num(rec(g.probes).failed), held: num(rec(g.probes).held), ok: num(rec(g.probes).ok), planOk: num(rec(g.probes).plan_ok), skipped: num(rec(g.probes).skipped) },
    rateLimited: num(g.rate_limited), refusals: counts(g.refusals), refused: num(g.refused), rejected: num(g.rejected), requests: num(g.requests),
    savingEquivalent: numOrNull(g.saving_equivalent),
    savingUsd: num(g.saving_usd),
    succeeded: num(g.succeeded), tokens: { cacheRead: num(t.cache_read), cacheWrite1h: num(t.cache_write_1h), cacheWrite5m: num(t.cache_write_5m), output: num(t.output), thinking: num(t.thinking), uncachedInput: num(t.uncached_input), webFetch: num(t.web_fetch), webSearch: num(t.web_search) }, uncacheable: num(g.uncacheable), uncached: num(g.uncached),
    wastedProbeReasons: wastedReasons(g.wasted_probe_reasons), wastedProbeUsd: num(g.wasted_probe_usd), wastedProbes: num(g.wasted_probes),
  };
}
export const decodeAnalyticsPoint = (v: unknown): AnalyticsPoint => ({ at: isoOr(rec(v).at, epoch), groups: list(rec(v).groups).map(decodeAnalyticsGroup) });

const decodeBurnPoint = (v: unknown): BurnPoint => {
  const p = rec(v);
  return {
    at: isoOr(p.at, epoch),
    cachedTokens: num(p.cached_tokens),
    equivalentByTier: nullableNumberMap(p.equivalent_by_tier),
    fableEquivalentByTier: nullableNumberMap(p.fable_equivalent_by_tier),
    fiveHourEquivalentByTier: nullableNumberMap(p.five_hour_equivalent_by_tier),
    fiveHourShare: numOrNull(p.five_hour_share),
    group: str(p.group),
    modelWeekShare: numOrNull(p.model_week_share),
    planUsd: num(p.plan_usd),
    sevenDayShare: numOrNull(p.seven_day_share),
    tokens: num(p.tokens),
    uncachedTokens: num(p.uncached_tokens),
    unit: burnUnit(p.unit),
    unpriced: bool(p.unpriced),
    usd: num(p.usd),
  };
};
export const decodeBurn = (v: unknown): { bucketSeconds: number; points: BurnPoint[] } => ({ bucketSeconds: num(rec(v).bucket_seconds, 3_600), points: list(rec(v).points).map(decodeBurnPoint) });
export const decodeRefusalCategories = (v: unknown): RefusalCategoriesAnswer => ({
  fallbacks: num(rec(v).fallbacks),
  from: isoOr(rec(v).from, epoch),
  rows: list(rec(v).rows).map(rec).map((r) => ({ category: str(r.category), model: str(r.model), requests: num(r.requests) })),
  to: isoOr(rec(v).to, epoch),
});
export const decodeRewrites = (v: unknown): RewritesAnswer => ({
  held: list(rec(v).held).map(rec).map((p) => ({ at: isoOr(p.at, epoch), heldMs: num(p.held_ms), requests: num(p.requests) })),
  moves: numberMap(rec(v).moves),
  movesByReasonAndFit: numberMaps(rec(v).moves_by_reason_and_fit),
  points: list(rec(v).points).map(rec).map((p) => ({ at: isoOr(p.at, epoch), rewriteTokens: num(p.rewrite_tokens) })),
});

export function decodePlacementRow(v: unknown): PlacementRow {
  const r = rec(v);
  return { cacheReadTokens: num(r.cache_read_tokens), hitShare: numOrNull(r.hit_share), hits: num(r.hits), placements: num(r.placements), promisedTokens: num(r.promised_tokens), rung: str(r.rung) };
}
export function decodeMemberBudget(v: unknown): MemberBudget {
  const m = rec(v);
  return {
    accountId: str(m.account_id), budgetPeriod: budgetPeriod(m.budget_period), budgetUsd: num(m.budget_usd),
    kind: accountKind(m.kind), periodEnd: isoOrNull(m.period_end), poolId: str(m.pool_id),
    spentUsd: numOrNull(m.spent_usd),
  };
}
export function decodeReconciliation(v: unknown): Reconciliation {
  const r = rec(v);
  return {
    accountId: str(r.account_id), computedAt: isoOr(r.computed_at, epoch), firstReadingAt: isoOr(r.first_reading_at, epoch), lastReadingAt: isoOr(r.last_reading_at, epoch),
    observedRise: num(r.observed_rise), predictedShare: num(r.predicted_share), ratio: num(r.ratio), requests: num(r.requests), window: str(r.window), windowEnd: isoOr(r.window_end, epoch),
  };
}
export const decodeReconciliations = (v: unknown): ReconciliationsAnswer => ({ reconciliations: list(rec(v).reconciliations).map(decodeReconciliation), requestRetentionDays: num(rec(v).request_retention_days) });

const decodeAllowanceReset = (v: unknown): AllowanceReset => {
  const r = rec(v);
  return { observedAt: isoOr(r.observed_at, epoch), plan: strOrNull(r.plan), resetAt: isoOr(r.reset_at, epoch), shareLeft: num(r.share_left), source: str(r.source), tier: strOrNull(r.tier) };
};
/** The sources of a row that a drop wrote, whose `resetAt` is the reset of the week that the drop cut and can lie
 *  ahead. */
const DROP_SOURCES: ReadonlySet<string> = new Set(['claim', 'plan_changed']);
/** The instant a row draws: the drop's for a row that a drop wrote, else the reset's. */
export const drawnInstant = (reset: AllowanceReset): string => (DROP_SOURCES.has(reset.source) ? reset.observedAt : reset.resetAt);
export const byDrawnNewest = (a: AllowanceReset, b: AllowanceReset) => Date.parse(drawnInstant(b)) - Date.parse(drawnInstant(a));

/** A window's rows parted by the source word, each list newest first by the instant a row draws at: the drop's for a
 *  claim row and for a plan change, which stands among the resets, the reset's for each other. The sort is stable, so
 *  rows of one instant keep the route's order. */
export function partAllowanceRows(rows: AllowanceReset[]): { claims: AllowanceReset[]; resets: AllowanceReset[] } {
  const claims: AllowanceReset[] = [];
  const resets: AllowanceReset[] = [];
  for (const row of rows) (row.source === 'claim' ? claims : resets).push(row);
  return { claims: claims.sort(byDrawnNewest), resets: resets.sort(byDrawnNewest) };
}
const decodeAllowanceRows = (v: unknown): { claims: AllowanceReset[]; resets: AllowanceReset[] } => partAllowanceRows(list(v).map(decodeAllowanceReset));
export const decodeUnusedAllowance = (v: unknown): UnusedAllowanceAnswer => ({
  accounts: list(rec(v).accounts)
    .map(rec)
    .map((a) => ({
      accountId: str(a.account_id),
      plan: strOrNull(a.plan),
      tier: strOrNull(a.tier),
      windows: list(a.windows)
        .map(rec)
        .map((w) => ({ ...decodeAllowanceRows(w.resets), totalShareLeft: num(w.total_share_left), window: str(w.window) })),
    })),
  from: isoOr(rec(v).from, epoch),
  to: isoOr(rec(v).to, epoch),
});

export function decodeAccountRollups(v: unknown): AccountRollup[] {
  return list(rec(v).accounts).map(rec).flatMap((a) => list(a.hours).map(rec).map((h) => {
    const hourIso = isoOr(h.hour, epoch);
    return {
      accountId: str(a.account_id), cooldownSeconds: num(h.cooldown_seconds), hour: hourIso, overageSeconds: num(h.overage_seconds),
      peak: { '5h': numOrNull(h.utilization_5h_max), '7d': numOrNull(h.utilization_7d_max), '7d_fable': numOrNull(h.utilization_7d_oi_max) },
      pins: { created: num(h.pins_created), migratedIn: num(h.pins_migrated_in), migratedOut: num(h.pins_migrated_out) },
      // The keys stay as the ledger spells them, with no `windowId` fold, since the cards never draw two windows as one.
      // The lead reads `numOrNull`, so a reset-only sample's null never reads as zero, exactly on pace.
      windows: Object.fromEntries(Object.entries(rec(h.windows)).flatMap(([id, w]) => (w === null ? [] : [[id, { deadline: isoOrNull(rec(w).deadline), lead: numOrNull(rec(w).lead), resetsAt: isoOrNull(rec(w).reset_at), sampledAt: isoOr(rec(w).sampled_at, hourIso), utilization: numOrNull(rec(w).utilization) }] as [string, WindowSample]]))),
    };
  }));
}

export function decodeDashboard(v: unknown): Overview {
  const d = rec(v);
  return {
    activePins: num(rec(d.sessions).active_pins),
    day24h: daySum(d),
    errorRate24h: numOrNull(d.error_rate_24h),
    hourly: list(d.hourly).map(rec).map((h) => ({ at: isoOr(h.at, epoch), billed: num(h.billed), cost: num(h.cost), equivalent: numOrNull(h.equivalent), errors: num(h.errors), refused: num(h.refused), requests: num(h.requests), tokens: num(h.tokens) })),
    refused24h: num(d.refused_24h),
    requests24h: num(d.requests_24h),
    tokens24h: num(d.tokens_24h),
    tokensCached24h: num(d.tokens_cached_24h),
    topErrorCode: strOrNull(d.top_error_code),
    topRefusalCode: strOrNull(d.top_refusal_code),
  };
}

const companion = (v: unknown): Session['companion'] => (str(v) === 'reporting' ? 'reporting' : str(v) === 'silent' ? 'silent' : 'none');
const lifecycle = (v: unknown): Lifecycle | null => oneOf(LIFECYCLE_VALUES, v);
const guardVerdict = (v: unknown): GuardVerdict | null => oneOf(GUARD_VERDICT_VALUES, v);

const burnUnit = (v: unknown): BurnUnit | null => oneOf(BURN_UNIT_VALUES, v);
const burnFigures = (r: Rec): BurnFigures => ({ burn: numOrNull(r.burn), burnAt: isoOrNull(r.burn_at), burnEquivalent: numOrNull(r.burn_equivalent), burnUnit: burnUnit(r.burn_unit), burnUsd: numOrNull(r.burn_usd), burnWindow: strOrNull(r.burn_window) });
const moveFigures = (r: Rec): MoveFigures => ({ moveCost: numOrNull(r.move_cost), moveDropCost: numOrNull(r.move_drop_cost), moveDropSize: numOrNull(r.move_drop_size), moveSize: numOrNull(r.move_size) });
const sessionMark = (v: unknown): Session['mark'] => orNull(v, (x) => ({ at: isoOr(x.at, epoch), on: str(x.on), window: str(x.window) }));

export function decodePoolPin(v: unknown): PoolPin {
  const p = rec(v);
  const { burn, burnAt, burnUnit, burnWindow } = burnFigures(p);
  return { accountBound: bool(p.account_bound), accountId: str(p.account_id), agentId: str(p.agent_id), burn, burnAt, burnUnit, burnWindow, ended: bool(p.ended), fast: bool(p.fast), keyId: str(p.key_id), mark: sessionMark(p.mark), model: str(p.model), ...moveFigures(p), sessionId: str(p.session_id), warm: bool(p.warm), warmUntil: isoOrNull(p.warm_until) };
}

function decodeAgentContext(v: unknown): AgentContext {
  const c = rec(v);
  return { accountBound: bool(c.account_bound), accountId: str(c.account_id), agentId: str(c.agent_id), ...burnFigures(c), cacheTtlMs: numOrNull(c.cache_ttl_ms), effort: strOrNull(c.effort), ended: bool(c.ended), fast: bool(c.fast), lastInputTokens: numOrNull(c.last_input_tokens), lastUsedAt: isoOrNull(c.last_used_at), lifecycle: lifecycle(c.lifecycle), model: str(c.model), ...moveFigures(c), pinnedAt: isoOrNull(c.pinned_at), warm: bool(c.warm), warmUntil: isoOrNull(c.warm_until) };
}

function decodeSessionSpendPart(v: unknown): SessionSpendPart {
  const p = rec(v);
  return { billedUsd: numberMap(p.billed_usd), billedUsdByAccount: numberMap(p.billed_usd_by_account), equivalent: numOrNull(p.equivalent), requests: num(p.requests), shares: numberMaps(p.shares), usd: num(p.usd) };
}
const decodeSessionSpend = (v: unknown): SessionSpend => ({ ...decodeSessionSpendPart(v), probes: decodeSessionSpendPart(rec(v).probes) });
function decodeSessionDay(v: unknown): SessionDay {
  const d = rec(v);
  return { billedUsd: numberMap(d.billed_usd), equivalent: numOrNull(d.equivalent), requests: numberMap(d.requests) };
}
function decodeAgentRequests(v: unknown): AgentRequests {
  const a = rec(v);
  return { accountId: strOrNull(a.account_id), agentId: str(a.agent_id), agentType: strOrNull(a.agent_type), firstRequestAt: isoOrNull(a.first_request_at), lastRequestAt: isoOrNull(a.last_request_at), model: str(a.model), spend: decodeSessionSpend(a.spend) };
}
function decodeSessionFacts(v: unknown): Session['facts'] {
  const f = rec(v);
  return { branch: strOrNull(f.branch), cwd: strOrNull(f.cwd), project: strOrNull(f.project), repository: strOrNull(f.repository), repositoryRoot: strOrNull(f.repository_root), title: strOrNull(f.title) };
}
function decodeSessionAgentFacts(v: unknown): Session['agentFacts'][number] {
  const a = rec(v);
  return { agentId: str(a.agent_id), agentType: strOrNull(a.agent_type), description: strOrNull(a.description), name: strOrNull(a.name) };
}

export function decodeSession(v: unknown): Session {
  const s = rec(v);
  const k = rec(s.keepalive);
  return { accountBound: boolOrNull(s.account_bound), accountId: strOrNull(s.account_id), agentContexts: list(s.agent_contexts).map(decodeAgentContext), agentFacts: list(s.agent_facts).map(decodeSessionAgentFacts), agentId: strOrNull(s.agent_id), agentRequests: list(s.agent_requests).map(decodeAgentRequests), ...burnFigures(s), cacheTtlMs: numOrNull(s.cache_ttl_ms), companion: companion(s.companion), companionSince: isoOrNull(s.companion_since), cycleMs: numOrNull(s.cycle_ms), day: decodeSessionDay(s.day), effort: strOrNull(s.effort), ended: boolOrNull(s.ended), factor: numOrNull(s.factor), facts: decodeSessionFacts(s.facts), fast: boolOrNull(s.fast), firstRequestAt: isoOrNull(s.first_request_at), forkDepth: numOrNull(s.fork_depth), forkedFrom: strOrNull(s.forked_from), historical: bool(s.historical), keepalive: { cause: recipeCause(k.cause), nextProbeAt: isoOrNull(k.next_probe_at) }, keyGeneration: num(s.key_generation), keyId: str(s.key_id), lastInferenceAt: isoOrNull(s.last_inference_at), lastInputTokens: numOrNull(s.last_input_tokens), lastRefreshAt: isoOrNull(s.last_refresh_at), lastRequestAt: isoOrNull(s.last_request_at), lastUsedAt: isoOrNull(s.last_used_at) ?? isoOrNull(s.last_request_at), lifecycle: lifecycle(s.lifecycle), mark: sessionMark(s.mark), model: strOrNull(s.model), ...moveFigures(s), pinned: bool(s.pinned), pinnedAt: isoOrNull(s.pinned_at), placements: num(s.placements), presenceAgeSeconds: numOrNull(s.presence_age_seconds), runwayBounded: boolOrNull(s.runway_bounded), runwayMs: numOrNull(s.runway_ms), sessionId: str(s.session_id), shed: shedOf(s.shed), speed: numOrNull(s.speed), spend: orNull(s.spend, decodeSessionSpend), userAgent: strOrNull(s.user_agent), warm: boolOrNull(s.warm) };
}

const recipeCause = (v: unknown): RecipeCause | null => oneOf(RECIPE_CAUSE_VALUES, v);
export function decodeRecipe(v: unknown): KeepaliveRecipe {
  const r = rec(v);
  return {
    accountId: strOrNull(r.account_id),
    agentId: str(r.agent_id),
    agentState: lifecycle(r.agent_state) ?? 'unknown',
    cause: recipeCause(r.cause) ?? 'scheduled',
    creditedToday: num(r.credited_today),
    deadline: isoOrNull(r.deadline),
    keyId: str(r.key_id),
    lastGuard: guardVerdict(r.last_guard),
    lastProbeAt: isoOrNull(r.last_probe_at),
    lastRequestAt: isoOrNull(r.last_request_at),
    model: str(r.model),
    nextProbeAt: isoOrNull(r.next_probe_at),
    parkKind: oneOf(PARK_KIND_VALUES, r.park_kind),
    poolId: str(r.pool_id),
    probesToday: num(r.probes_today),
    sessionId: str(r.session_id),
    state: oneOf(RECIPE_STATE_VALUES, r.state) ?? 'retained',
    ttlSeconds: num(r.ttl_seconds),
    wakeAt: isoOrNull(r.wake_at),
  };
}

export function decodeKeepaliveOverview(v: unknown): KeepaliveOverview {
  const o = rec(v);
  const p = rec(o.probes_24h);
  const l = rec(o.limits);
  const limit = (x: unknown) => ({ count: num(rec(x).count), max: num(rec(x).max) });
  const pools = list(o.pools).map(rec);
  return {
    busGaps: num(o.bus_gaps),
    expiredBetweenTurns24h: num(o.expired_between_turns_24h),
    lastBusGapAt: isoOrNull(o.last_bus_gap_at),
    limits: { agents: limit(l.agents), retainedBytes: limit(l.retained_bytes), sessions: limit(l.sessions) },
    pools: pools.map((q) => ({
      credited24h: num(q.credited_24h),
      idleReturns: list(q.idle_returns).map(rec).map((b) => ({ fromSeconds: num(b.from_seconds), reached: num(b.reached), returned: num(b.returned), toSeconds: numOrNull(b.to_seconds) })),
      poolId: str(q.pool_id),
      probes24h: num(q.probes_24h),
      wasted24h: num(q.wasted_24h),
      wastedReasons24h: wastedReasons(q.wasted_reasons_24h),
    })),
    probes24h: { failed: num(p.failed), held: num(p.held), ok: num(p.ok), skipped: num(p.skipped) },
  };
}

const priceCells = (m: Rec): PriceCells => ({ cacheReadPerM: num(m.cache_read), cacheWrite1hPerM: num(m.cache_write_1h), cacheWrite5mPerM: num(m.cache_write_5m), inputPerM: num(m.input), outputPerM: num(m.output) });
const price = (model: string, m: Rec): ModelPrice => ({
  ...priceCells(m),
  fast: orNull(m.fast, priceCells),
  longContext: orNull(m.long_context, (l) => ({ ...priceCells(l), over: num(l.over) })),
  model,
});
/**
 * The instant a table's `effective` parses to, or null. The route answers two forms: a date `YYYY-MM-DD`, in force
 * from 00:00 UTC of that day, and an instant `YYYY-MM-DDTHH:MM:SSZ`, in force from that second. A text that is
 * neither is kept as sent rather than throwing, which would fail the whole settings answer and every page that reads
 * it. Only text opening with an ISO date counts, since `Date.parse` reads some free text as a date ("version 3" as
 * March 2001).
 */
export const effectiveDateOf = (effective: string): string | null => {
  const parsed = /^\d{4}-\d{2}-\d{2}/.test(effective) ? Date.parse(effective) : Number.NaN;
  return Number.isNaN(parsed) ? null : iso(parsed);
};
/**
 * A table's `effective` as a label, free text as sent. A date-only entry reads its date in UTC, the zone it parses
 * in: it is midnight UTC, which the browser's own zone would show as the day before west of UTC. An entry that
 * carries an instant reads its date and its time to the minute in the browser's zone, like every other instant.
 */
export const effectiveLabel = (effective: string): string => {
  const at = effectiveDateOf(effective);
  if (at === null) return effective;
  return /^\d{4}-\d{2}-\d{2}$/.test(effective) ? formatIn({ day: '2-digit', month: 'short', timeZone: 'UTC', year: 'numeric' }, at) : fmtMinute(at);
};
const pricedModels = (content: Rec): ModelPrice[] => Object.entries(rec(content.models)).map(([model, row]) => price(model, rec(row)));
/** One dated entry of the price table, read as leniently as the entry in force. */
const priceRevision = (v: unknown): PriceRevision => {
  const effective = str(rec(v).effective, '');
  return { effective, effectiveDate: effectiveDateOf(effective), models: pricedModels(rec(rec(v).content)) };
};
/** The `api_prices` envelope: its rows keyed by model in the file's order, the date from the file it carries, and
 *  every dated entry of the file. */
export const decodePricing = (envelope: unknown): Settings['apiPrices'] => {
  const content = rec(rec(envelope).content);
  const effective = str(content.effective, '');
  return { effective, effectiveDate: effectiveDateOf(effective), models: pricedModels(content), revisions: list(rec(envelope).revisions).map(priceRevision), source: str(rec(envelope).source) };
};

const TYPE_KEYS: [keyof TypeMultiplier, string][] = [['cacheRead', 'cache_read'], ['cacheWrite1h', 'cache_write_1h'], ['cacheWrite5m', 'cache_write_5m'], ['input', 'input'], ['output', 'output']];
/** What the console says in place of a cost model it cannot read, which then prices nothing. */
const COSTS_UNREADABLE = "The cost model's answer could not be read.";
/** The empty model: it sizes no tier and weighs no model. */
const emptyCosts = (): SubscriptionCosts => ({
  capacity: {},
  effective: '',
  modelTier: {},
  modelWeight: {},
  modelWindows: {},
  overrides: {},
  peak: { capacityFactor: { fiveHour: 1, sevenDay: 1 }, fromHour: 0, toHour: 0, weekdays: false, zone: '' },
  regimes: {},
  tierOrder: [],
  typeMultiplier: { cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: 0, input: 0, output: 0 },
  unit: { tier: 'default_claude_ai', window: 'seven_day' },
});
// A fallback on a mistyped member would price a share the router never charges, so the cost model is read strictly.
// The throw never leaves `readCosts`: a throw out of the settings answer would fail every page that reads it, so the
// answer carries the empty model and a problem instead.
class Malformed extends Error {}
const must = <T>(ok: boolean, v: T): T => {
  if (!ok) throw new Malformed();
  return v;
};
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
const capacityRows = (v: unknown): SubscriptionCosts['capacity'] => byKey(v, (t) => ({ fiveHour: finiteOrNull(map(t).five_hour), sevenDay: finiteOrNull(map(t).seven_day) }));
/**
 * Every member the model requires is present and of its type: a capacity and a weight may be null, an override's
 * multiplier block and each multiplier in it absent or null, its `weight_by_window` block absent or null but never a
 * null weight inside it, and nothing else. The unit is the envelope's, one block every revision reads in.
 */
const strictCosts = (content: unknown, unit: unknown): SubscriptionCosts => {
  const c = map(content);
  const anchor = map(unit);
  const peak = map(c.peak);
  const factor = map(peak.capacity_factor);
  const multiplier = map(c.type_multiplier);
  return {
    capacity: capacityRows(c.capacity),
    effective: text(c.effective),
    modelTier: byKey(c.model_tier, text),
    modelWeight: byKey(c.model_weight, finiteOrNull),
    modelWindows: byKey(c.model_windows, (w) => ({ capacityFactor: finite(map(w).capacity_factor), of: text(map(w).of) })),
    overrides: byKey(c.overrides, override),
    peak: { capacityFactor: { fiveHour: finite(factor.five_hour), sevenDay: finite(factor.seven_day) }, fromHour: finite(peak.from_hour), toHour: finite(peak.to_hour), weekdays: flag(peak.weekdays), zone: text(peak.zone) },
    regimes: byKey(c.regimes, (r) => {
      const regime = map(r);
      const regimeFactor = map(map(regime.peak).capacity_factor);
      return {
        capacity: capacityRows(regime.capacity),
        entrypoints: [...must(Array.isArray(regime.entrypoints) && regime.entrypoints.every((t) => typeof t === 'string'), regime.entrypoints as string[])],
        peakCapacityFactor: { fiveHour: finite(regimeFactor.five_hour), sevenDay: finite(regimeFactor.seven_day) },
      };
    }),
    tierOrder: [...must(Array.isArray(c.tier_order) && c.tier_order.every((t) => typeof t === 'string'), c.tier_order as string[])],
    typeMultiplier: { cacheRead: finite(multiplier.cache_read), cacheWrite1h: finite(multiplier.cache_write_1h), cacheWrite5m: finite(multiplier.cache_write_5m), input: finite(multiplier.input), output: finite(multiplier.output) },
    unit: { tier: text(anchor.tier), window: text(anchor.window) },
  };
};
/** The model, or null when a member of it or of the unit is missing or of the wrong type. */
const readCosts = (content: unknown, unit: unknown): SubscriptionCosts | null => {
  try {
    return strictCosts(content, unit);
  } catch (e) {
    if (e instanceof Malformed) return null;
    throw e;
  }
};
/** The subscription cost model from the envelope's `content` under its `unit`. A content that is absent, or one that
 *  cannot be read or whose unit cannot be read, is the empty model. */
export const decodeSubscriptionCosts = (content: unknown, unit: unknown): SubscriptionCosts => (content === undefined ? emptyCosts() : (readCosts(content, unit) ?? emptyCosts()));
/** Every dated entry of the cost model, or null when the list or one entry cannot be read, as a member of the entry
 *  in force cannot. */
const readRevisions = (v: unknown, unit: unknown): CostRevision[] | null => {
  if (!Array.isArray(v)) return null;
  const revisions: CostRevision[] = [];
  for (const entry of v) {
    const effective = isMap(entry) && typeof entry.effective === 'string' ? entry.effective : null;
    const content = effective === null ? null : readCosts(entry.content, unit);
    if (effective === null || content === null) return null;
    revisions.push({ content, effective, effectiveDate: effectiveDateOf(effective) });
  }
  return revisions;
};
/**
 * The settings answer's `subscription_costs` envelope. An answer without it reads as the empty model with no problem;
 * one whose envelope cannot be read, its entry in force or any entry of its `revisions`, reads as the empty model with
 * the problem the cost views show.
 */
const decodeCostsEnvelope = (envelope: unknown): Settings['subscriptionCosts'] => {
  if (envelope === undefined) return { ...emptyCosts(), problem: null, revisions: [], source: '' };
  const costs = isMap(envelope) && typeof envelope.source === 'string' ? readCosts(envelope.content, envelope.unit) : null;
  const revisions = costs === null ? null : readRevisions(rec(envelope).revisions, rec(envelope).unit);
  return costs === null || revisions === null ? { ...emptyCosts(), problem: COSTS_UNREADABLE, revisions: [], source: str(rec(envelope).source) } : { ...costs, problem: null, revisions, source: str(rec(envelope).source) };
};

export const decodeSessionInfo = (v: unknown): SessionInfo => {
  const r = rec(v);
  // Named, since `text`'s refusal carries no message and the login's error line shows this one.
  if (typeof r.session_id !== 'string') throw new Error('session_id must be a string');
  return { persistent: r.persistent === true, sessionId: r.session_id };
};

export function decodeSettings(v: unknown): Settings {
  const s = rec(v);
  const l = rec(s.listeners);
  const kept = rec(s.retention);
  const store = rec(s.storage);
  return {
    about: { build: str(rec(s.about).build, 'unknown'), profile: str(rec(s.about).profile), version: str(rec(s.about).version) },
    access: { adminConfigured: bool(rec(s.access).admin_configured, true) },
    apiPrices: decodePricing(s.api_prices),
    features: orNull(s.features, (f) => ({ caFingerprint: str(f.ca_fingerprint), caPem: str(f.ca_pem) })),
    listeners: { metrics: str(l.metrics), proxy: str(l.proxy), proxyTls: bool(l.proxy_tls) },
    retention: { bodyCaptureMaxHours: num(kept.body_capture_max_hours), retentionDays: num(kept.retention_days), rollupRetentionDays: num(kept.rollup_retention_days) },
    sessions: { expireAfterSeconds: num(rec(s.sessions).expire_after_seconds), nonCompanionIdleSeconds: num(rec(s.sessions).non_companion_idle_seconds) },
    storage: { apiPricesPath: strOrNull(store.api_prices_path), ledgerPath: str(store.ledger_path), path: str(store.path), subscriptionCostsPath: strOrNull(store.subscription_costs_path) },
    subscriptionCosts: decodeCostsEnvelope(s.subscription_costs),
    usage: { limitResetWarningDays: num(rec(s.usage).limit_reset_warning_days) },
  };
}

export const encodeMembers = (members: PoolInput['members']) => members.map((m) => ({ account_id: m.accountId, budget_daily_usd: m.budgetDailyUsd, budget_monthly_usd: m.budgetMonthlyUsd, priority: m.priority }));
export const encodeModelRules = (rules: ModelRule[]) => rules.map((r) => ({ not: r.not, pattern: r.pattern }));
export const encodeTtlRules = (r: TtlRules) => ({ main: r.main, models: r.models.map((m) => ({ main: m.main, model: m.model, subagent: m.subagent })), subagent: r.subagent });

const encodeKeyFeature = (c: KeyFeatureAccount) => ({ account_id: c.accountId, mode: c.mode });

export const encodeAccountPatch = (p: AccountPatch) => ({ canceled: p.canceled, subscription_ends_at: p.subscriptionEndsAt === null ? null : Date.parse(p.subscriptionEndsAt) });
export const encodeKey = (i: KeyInput) => ({
  auto_mode_server: i.autoModeServer, body_capture_hours: i.bodyCaptureHours,
  companion_permissions: i.companionPermissions,
  enabled: i.enabled,
  ...(i.expiresAt === undefined ? {} : { expires_at: i.expiresAt === null ? null : Date.parse(i.expiresAt) }),
  feature_accounts: perFeature((f) => encodeKeyFeature(i.featureAccounts[f])),
  label: i.name, long_context_twins: i.longContextTwins,
  model_rules: encodeModelRules(i.modelRules), model_rules_mode: i.modelRulesMode, notes: i.notes, pool_id: i.poolId,
  ...(i.guardrails === undefined ? {} : { guardrails: i.guardrails }),
});
export const encodePool = (i: PoolInput) => ({
  affinity_mode: i.affinityMode, affinity_ttl_seconds: i.affinityTtlSeconds, auto_mode_server: i.autoModeServer, body_capture_hours: i.bodyCaptureHours,
  cache_diagnostics: i.cacheDiagnostics,
  cache_ttl: { enabled: i.cacheTtl.enabled, inject_when_absent: i.cacheTtl.injectWhenAbsent, metered: encodeTtlRules(i.cacheTtl.metered), plan: encodeTtlRules(i.cacheTtl.plan) },
  enabled: i.status === 'active', feature_accounts: perFeature((f) => i.featureAccounts[f]), model_rules: encodeModelRules(i.modelRules),
  keepalive: { enabled: i.keepalive.enabled, max_idle_seconds: i.keepalive.maxIdleSeconds, max_parked_seconds: i.keepalive.maxParkedSeconds, scheduled_horizon_seconds: i.keepalive.scheduledHorizonSeconds },
  label: i.name, members: encodeMembers(i.members),
  min_shared_prefix_tokens: i.minSharedPrefixTokens, overage_policy: i.overagePolicy, prefer_subscription: i.preferSubscription,
  session_placement: i.sessionPlacement, strategy: i.strategy, subagent_placement: i.subagentPlacement, thinking_diagnostics: i.thinkingDiagnostics,
  burn_average_seconds: i.burnAverageSeconds, fable_reservation_shedding: i.fableReservationShedding, five_hour_ceiling: i.fiveHourCeiling, in_flight_cap: i.inFlightCap, load_shedding: i.loadShedding, migrate_when_spent: i.migrateWhenSpent, overage_move_grace_minutes: i.overageMoveGraceMinutes,
  retry_in_place_seconds: i.retryInPlaceSeconds, return_to_plan: i.returnToPlan, return_to_plan_dwell_seconds: i.returnToPlanDwellSeconds, shed_cap: i.shedCap, shed_dwell_seconds: i.shedDwellSeconds,
  slow_down: i.slowDown, slow_down_hold_seconds: i.slowDownHoldSeconds, slow_down_stretch_seconds: i.slowDownStretchSeconds, warm_sessions: i.warmSessions, weekly_ceiling: i.weeklyCeiling, wind_down_minutes: i.windDownMinutes,
  ...(i.guardrails === undefined ? {} : { guardrails: i.guardrails }),
});

// The guardrails' shapes, read as leniently as the rows above: a word outside its set folds to a named default, and
// the lists read in their `position` order, which a detach can leave with a gap, so a position is never an index.

// `observe` is the route's own default for a guardrail created without a mode.
const guardrailMode = (v: unknown): GuardrailMode => oneOf(GUARDRAIL_MODE_VALUES, v) ?? 'observe';
const byPosition = <T extends { position: number }>(rows: T[]): T[] => [...rows].sort((a, b) => a.position - b.position);
/** A member the route leaves out where the condition takes none, never null. */
const optional = (v: unknown): string | undefined => (typeof v === 'string' ? v : undefined);

/** Every condition carries the four optional keys, undefined where the route left the member out. */
export function decodeCondition(v: unknown): Condition {
  const c = rec(v);
  const selector = optional(c.selector);
  return {
    name: optional(c.name),
    operator: oneOf(CONDITION_OPERATOR_VALUES, c.operator) ?? CONDITION_OPERATOR_VALUES[0],
    path: optional(c.path),
    selector: selector === undefined ? undefined : (oneOf(MESSAGE_SELECTOR_VALUES, selector) ?? MESSAGE_SELECTOR_VALUES[0]),
    subject: oneOf(CONDITION_SUBJECT_VALUES, c.subject) ?? CONDITION_SUBJECT_VALUES[0],
    value: optional(c.value),
  };
}

export function decodeGuardrailRef(v: unknown): GuardrailRef {
  const r = rec(v);
  return { enabled: bool(r.enabled, true), id: str(r.id), mode: guardrailMode(r.mode), name: str(r.name), position: num(r.position) };
}
const guardrailRefs = (v: unknown): GuardrailRef[] => byPosition(list(v).map(decodeGuardrailRef));

export function decodeGuardrailMatch(v: unknown): GuardrailMatch {
  const m = rec(v);
  return { blocked: bool(m.blocked), id: str(m.id), position: num(m.position) };
}

const attachmentsOf = (v: unknown): GuardrailAttachment[] => byPosition(list(v).map(rec).map((a) => ({ id: str(a.id), label: str(a.label), position: num(a.position) })));

/** The member is on the wire whether null or the stamp, while the OpenAPI document marks it optional: absent reads none. */
const presetStateOf = (v: unknown): GuardrailPresetState | null => {
  if (!isMap(v)) return null;
  const version = num(v.version, 1);
  return { currentVersion: num(v.current_version, version), customized: bool(v.customized), id: str(v.id), version };
};

export function decodeGuardrail(v: unknown): Guardrail {
  const g = rec(v);
  return {
    action: 'block',
    conditions: list(g.conditions).map(decodeCondition),
    createdAt: isoOr(g.created_at, epoch),
    description: str(g.description),
    enabled: bool(g.enabled, true),
    id: str(g.id),
    keys: attachmentsOf(g.keys),
    mode: guardrailMode(g.mode),
    name: str(g.name),
    pools: attachmentsOf(g.pools),
    preset: presetStateOf(g.preset),
    // A row whose answer carried none reads 0, which the route refuses as stale, so the dialog offers Reload.
    revision: num(g.revision),
    updatedAt: isoOr(g.updated_at, epoch),
  };
}

export function decodeGuardrailPreset(v: unknown): GuardrailPreset {
  const p = rec(v);
  return {
    conditions: list(p.conditions).map(decodeCondition),
    description: str(p.description),
    id: str(p.id),
    measuredOn: str(p.measured_on),
    name: str(p.name),
    suggestedMode: guardrailMode(p.suggested_mode),
    version: num(p.version, 1),
  };
}

export function decodeGuardrailTest(v: unknown): GuardrailTestResult {
  const t = rec(v);
  const sample = rec(t.sample);
  const totals = rec(t.totals);
  return {
    requests: list(t.requests)
      .map(rec)
      // A verdict this build does not know claims neither a match nor a miss.
      .map((r) => ({ condition: numOrNull(r.condition), id: str(r.id), receivedAt: isoOr(r.received_at, epoch), verdict: oneOf(TEST_VERDICT_VALUES, r.verdict) ?? 'indeterminate' })),
    // The route writes the side it sampled and leaves the other out.
    sample: { keyId: strOrNull(sample.key_id), limit: num(sample.limit), poolId: strOrNull(sample.pool_id), windowHours: num(sample.window_hours) },
    totals: { indeterminate: num(totals.indeterminate), matched: num(totals.matched), sampled: num(totals.sampled), unmatched: num(totals.unmatched) },
  };
}

export function decodeGuardrailCounts(v: unknown): GuardrailCounts {
  const c = rec(v);
  return {
    from: isoOr(c.from, epoch),
    guardrails: list(c.guardrails)
      .map(rec)
      .map((g) => ({ blocked: num(g.blocked), blockedBytes: num(g.blocked_bytes), id: str(g.id), name: strOrNull(g.name), observed: num(g.observed) })),
    to: isoOr(c.to, epoch),
  };
}

const encodeCondition = (c: Condition) => ({ name: c.name, operator: c.operator, path: c.path, selector: c.selector, subject: c.subject, value: c.value });

/** The members `input` defines, in the route's spelling. The transport's JSON leaves an undefined member out. */
export const encodeGuardrail = (input: Partial<GuardrailInput>): Record<string, unknown> => ({
  conditions: input.conditions?.map(encodeCondition),
  description: input.description,
  enabled: input.enabled,
  keys: input.keys,
  mode: input.mode,
  name: input.name,
  pools: input.pools,
  preset: input.preset,
  preset_version: input.presetVersion,
});

export const encodeGuardrailTest = (input: GuardrailTestInput): Record<string, unknown> => ({
  conditions: input.conditions?.map(encodeCondition),
  guardrail: input.guardrail,
  key_id: input.keyId,
  limit: input.limit,
  pool_id: input.poolId,
});
