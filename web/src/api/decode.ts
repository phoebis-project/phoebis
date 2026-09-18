import type {
  Account, AccountRollup, AlertEvent, AlertRule, AlertRuleInput, AnalyticsGroup, AnalyticsPoint, ApiKey, ApiKeySubStrategy, AttemptOutcome, CacheMissReason, CapturedBodies, CatalogModel,
  Delivery, KeepaliveOverview, KeepaliveRecipe, LiveAttempt, LiveDelta, LiveMetric, LiveRequest, LiveSnapshot, LiveTick, MemberEligibility, ModelPrice, Overview, PairingSession, Pool,
  PriceImportPreview, QuotaHistory, QuotaSnapshot, RateLimitWindow, RequestLog, RequestPage, ServedModel, Session, Settings, WindowId,
} from './types';
import type { KeyInput, PoolInput } from './client';
import { CACHE_MISS_REASONS, WASTED_PROBE_REASONS, emptyCacheMissReasons, emptyWastedProbeReasons } from '@/lib/cache';
import { BUCKET_COUNT, emptyHistogram } from '@/lib/histogram';
import { WINDOW_HOURS, WINDOW_LABEL, WINDOW_ORDER } from '@/lib/windows';

type Rec = Record<string, unknown>;
const rec = (v: unknown): Rec => (v && typeof v === 'object' ? (v as Rec) : {});
const list = (v: unknown): unknown[] => (Array.isArray(v) ? v : []);
const str = (v: unknown, fallback = ''): string => (typeof v === 'string' ? v : fallback);
const strOrNull = (v: unknown): string | null => (typeof v === 'string' ? v : null);
const num = (v: unknown, fallback = 0): number => (typeof v === 'number' && Number.isFinite(v) ? v : fallback);
const numOrNull = (v: unknown): number | null => (typeof v === 'number' && Number.isFinite(v) ? v : null);
const bool = (v: unknown, fallback = false): boolean => (typeof v === 'boolean' ? v : fallback);
const strings = (v: unknown): string[] => list(v).filter((x): x is string => typeof x === 'string');

/** The wire carries Unix milliseconds; every page formats ISO strings. */
export const iso = (ms: number): string => new Date(ms).toISOString();
export const isoOrNull = (ms: unknown): string | null => (typeof ms === 'number' && Number.isFinite(ms) ? iso(ms) : null);
const isoOr = (ms: unknown, fallback: string): string => isoOrNull(ms) ?? fallback;
const epoch = '1970-01-01T00:00:00.000Z';

/**
 * The header sampler writes the short forms and the usage poll writes the endpoint's own keys; the ledger keeps
 * both, and each pair is one window to the UI. `fable` and `7d_oi` both mean the model-scoped weekly window the
 * UI draws third.
 */
const WINDOW_IDS = new Map<string, WindowId>([['5h', '5h'], ['7d', '7d'], ['7d_oi', '7d_fable'], ['fable', '7d_fable'], ['five_hour', '5h'], ['seven_day', '7d'], ['seven_day_opus', '7d']]);
const windowId = (id: string): WindowId | null => WINDOW_IDS.get(id) ?? null;

function decodeWindow(w: Rec): RateLimitWindow | null {
  const id = windowId(str(w.id));
  if (id === null) return null;
  return { id, label: WINDOW_LABEL[id], observedAt: isoOr(w.observed_at, epoch), periodHours: WINDOW_HOURS[id], resetsAt: isoOrNull(w.resets_at), utilization: num(w.utilization) };
}

/** One window per id, the newest reading when both samplers wrote one, in the order the pages draw them. */
function foldWindows(windows: RateLimitWindow[]): RateLimitWindow[] {
  const newest = new Map<WindowId, RateLimitWindow>();
  for (const w of windows) {
    const seen = newest.get(w.id);
    if (!seen || Date.parse(w.observedAt) > Date.parse(seen.observedAt)) newest.set(w.id, w);
  }
  return WINDOW_ORDER.flatMap((id) => newest.get(id) ?? []);
}

/** The account's state and its flags folded onto the one badge the pages show. */
function accountStatus(a: Rec, cooldowns: unknown[]): Account['status'] {
  const state = str(a.state);
  if (state === 'reauth_required' || state === 'funding_required') return state;
  if (state === 'unpaired' || state === 'disconnected' || state === 'pairing') return 'unpaired';
  if (!bool(a.enabled, true)) return 'paused';
  if (cooldowns.length > 0) return 'cooldown';
  return 'ready';
}

function decodeServedModel(m: Rec): ServedModel {
  const upstreamId = str(m.upstream_id);
  return { canonicalId: str(m.canonical_id, upstreamId), id: str(m.canonical_id, upstreamId), inCatalog: !bool(m.missing), isNew: bool(m.new), longContext: bool(m.long_context), manual: str(m.source) === 'manual', selected: bool(m.selected), upstreamId };
}

export function decodeAccount(v: unknown): Account {
  const a = rec(v);
  const cooldowns = list(a.cooldowns).map(rec);
  const windows = foldWindows(list(a.windows).map(rec).map(decodeWindow).filter((w): w is RateLimitWindow => w !== null));
  const extra = a.extra_usage === null || a.extra_usage === undefined ? null : rec(a.extra_usage);
  const profile = a.profile === null || a.profile === undefined ? null : rec(a.profile);
  const identity = rec(a.identity);
  const credential = rec(a.credential);
  return {
    catalogSyncedAt: isoOrNull(a.catalog_synced_at),
    cooldowns: cooldowns.map((c) => ({ reason: str(c.reason), scope: str(c.scope), setAt: isoOr(c.set_at, epoch), until: isoOr(c.until, epoch), windowId: windowId(str(c.window)) })),
    credential: { accessTokenExpiresAt: isoOrNull(credential.access_expires_at), lastError: strOrNull(credential.last_error), lastRefreshAt: isoOrNull(credential.last_refresh_at), refreshTokenExpiresAt: isoOrNull(credential.refresh_expires_at) },
    email: strOrNull(identity.email),
    extraUsage: extra && bool(extra.is_enabled) ? { disabledReason: strOrNull(extra.disabled_reason), enabled: true, inUse: bool(extra.in_use), monthlyLimitCredits: num(extra.monthly_limit), usedCredits: num(extra.used_credits) } : null,
    fableWindow: windows.find((w) => w.id === '7d_fable') ?? null,
    id: str(a.id),
    kind: str(a.kind) === 'api_key' ? 'api_key' : 'oauth',
    label: str(a.label),
    plan: profile ? planLabel(str(profile.rate_limit_tier)) : null,
    pools: list(a.pools).map(rec).map((p) => ({ poolId: str(p.pool_id), priority: num(p.priority) })),
    requests: { last24h: num(rec(a.requests).last_24h), last7d: num(rec(a.requests).last_7d), lifetime: num(rec(a.requests).lifetime) },
    routingEnabled: bool(a.enabled, true),
    servedModels: list(a.served_models).map(rec).map(decodeServedModel),
    status: accountStatus(a, cooldowns),
    // The route answers the profile's three labels and nothing else, so the rest stays null rather than being invented.
    subscription: profile ? { billingType: str(profile.billing_type), organizationType: str(profile.organization_type), rateLimitTier: str(profile.rate_limit_tier), since: null, status: null, trialEndsAt: null } : null,
    windows: windows.filter((w) => w.id !== '7d_fable'),
  };
}

/** `default_claude_max_5x` reads as "Max 5x"; anything else is shown as sent. */
export function planLabel(tier: string): string {
  const m = /^(?:default_)?claude_(max|pro|team|enterprise)(?:_(\d+)x)?$/.exec(tier);
  if (!m) return tier;
  const name = m[1][0].toUpperCase() + m[1].slice(1);
  return m[2] ? `${name} ${m[2]}x` : name;
}

export function decodeCatalog(accounts: Account[]): CatalogModel[] {
  // The catalog is the union of what the accounts serve.
  const out = new Map<string, CatalogModel>();
  for (const a of accounts) for (const m of a.servedModels) if (!out.has(m.id)) out.set(m.id, { id: m.id, longContext: m.longContext });
  return [...out.values()].sort((x, y) => x.id.localeCompare(y.id));
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
  return WINDOW_ORDER.flatMap((id) => byId.get(id) ?? []);
}

export function decodeSnapshot(accountId: string, v: unknown): QuotaSnapshot {
  const u = rec(v);
  const error = strOrNull(u.error);
  return { accountId, error, fetchedAt: isoOrNull(u.fetched_at), status: error ? 'error' : 'success' };
}

export function decodeKey(v: unknown): ApiKey {
  const k = rec(v);
  const expires = isoOrNull(k.expires_at);
  const revoked = isoOrNull(k.revoked_at);
  return {
    allowedModels: k.allowed_models === null || k.allowed_models === undefined ? null : strings(k.allowed_models),
    bodyCaptureHours: numOrNull(k.body_capture_hours),
    createdAt: isoOr(k.created_at, epoch),
    expiresAt: expires,
    id: str(k.id),
    keepaliveOptOut: bool(k.keepalive_opt_out),
    lastUsedAt: isoOrNull(k.last_used_at),
    lifetime: { cost: num(rec(k.lifetime).cost_usd), requests: num(rec(k.lifetime).requests) },
    name: str(k.label),
    notes: strOrNull(k.notes),
    poolId: str(k.pool_id),
    prefix: str(k.prefix),
    // Routing refuses a key that is revoked, expired or switched off, in that order of finality; a row from
    // a route that reports no `enabled` is taken as on rather than off.
    status: revoked !== null ? 'revoked' : expires !== null && Date.parse(expires) <= Date.now() ? 'expired' : bool(k.enabled, true) ? 'active' : 'disabled',
    usage7d: { cost: num(rec(k.usage_7d).cost_usd), requests: num(rec(k.usage_7d).requests), tokens: num(rec(k.usage_7d).tokens) },
  };
}

const ELIGIBILITY: MemberEligibility[] = ['cooldown', 'eligible', 'overage', 'unavailable'];
const SUB_STRATEGIES: ApiKeySubStrategy[] = ['priority_random', 'round_robin'];

export function decodePool(v: unknown): Pool {
  const p = rec(v);
  const k = rec(p.keepalive);
  return {
    activePins: num(p.active_pins),
    affinityTtlSeconds: num(p.affinity_ttl_seconds),
    apiKeySubStrategy: SUB_STRATEGIES.find((x) => x === str(p.api_key_sub_strategy)) ?? 'priority_random',
    bodyCaptureHours: num(p.body_capture_hours),
    cacheTtl: null,
    cost24h: num(p.cost_24h),
    defaultAllowedModels: p.default_allowed_models === null || p.default_allowed_models === undefined ? null : strings(p.default_allowed_models),
    id: str(p.id),
    keepalive: {
      coldProbeLimit: num(k.cold_probe_limit), costCapUsdPerDay: numOrNull(k.probe_cost_cap_usd_per_day), enabled: bool(k.enabled), jitterSeconds1h: num(k.jitter_seconds_1h), jitterSeconds5m: num(k.jitter_seconds_5m),
      leadSeconds1h: num(k.lead_seconds_1h), leadSeconds5m: num(k.lead_seconds_5m), maxIdleSeconds: num(k.max_idle_seconds), maxIdleTicks: num(k.max_idle_ticks), netNegativeGraceProbes: num(k.net_negative_grace_probes),
      retryMaxSeconds: num(k.retry_max_seconds), retrySeconds: num(k.retry_seconds), stopWhenNetNegative: bool(k.stop_when_net_negative), warmSubagents: null,
    },
    // A row from before the router reported eligibility reads as not servable rather than as servable.
    members: list(p.members).map(rec).map((m) => ({ accountId: str(m.account_id), eligible: ELIGIBILITY.find((e) => e === str(m.eligible)) ?? 'unavailable', priority: num(m.priority) })),
    migrateAboveUtilization: numOrNull(p.migrate_above_utilization),
    name: str(p.label),
    overagePolicy: (['allow', 'never', 'prefer_plan'] as const).find((x) => x === str(p.overage_policy)) ?? 'prefer_plan',
    preferSubscription: bool(p.prefer_subscription),
    preferSubscriptionEffective: bool(p.prefer_subscription_effective),
    requests24h: num(p.requests_24h),
    status: (['active', 'degraded', 'disabled', 'exhausted'] as const).find((x) => x === str(p.status)) ?? 'active',
    strategy: (['earliest_refresh', 'least_utilized', 'priority_random', 'round_robin'] as const).find((x) => x === str(p.strategy)) ?? 'least_utilized',
    traffic24h: list(p.traffic_24h).map(rec).map((b) => ({ at: isoOr(b.at, epoch), costUsd: num(b.cost_usd), failed: num(b.failed), requests: num(b.requests), tokens: num(b.tokens) })),
  };
}

const OUTCOMES: AttemptOutcome[] = ['auth_failed', 'canceled', 'rate_limited', 'succeeded', 'transport_error', 'upstream_error'];
const outcomeOf = (v: unknown): AttemptOutcome => OUTCOMES.find((o) => o === str(v)) ?? 'upstream_error';
const missReason = (v: unknown): CacheMissReason | null => ((CACHE_MISS_REASONS as string[]).includes(str(v)) ? (str(v) as CacheMissReason) : v === null || v === undefined ? null : 'unavailable');
const attemptWindows = (v: unknown) => list(v).map(rec).flatMap((w) => { const id = windowId(str(w.id)); return id === null ? [] : [{ id, resetsAt: isoOrNull(w.resets_at), utilization: num(w.utilization) }]; });
const usageOf = (v: unknown) => { const u = rec(v); return { cacheRead: num(u.cache_read), cacheWrite1h: num(u.cache_write_1h), cacheWrite5m: num(u.cache_write_5m), input: num(u.input), output: num(u.output) }; };

export function decodeRequest(v: unknown): RequestLog {
  const r = rec(v);
  return {
    accountId: strOrNull(r.account_id), agentId: strOrNull(r.agent_id), at: isoOr(r.at, epoch),
    attempts: list(r.attempts).map(rec).map((a) => ({ accountId: str(a.account_id), cacheMissedInputTokens: null, cacheMissReason: missReason(a.cache_miss_reason), cooldownSet: a.cooldown_set ? { reason: str(rec(a.cooldown_set).reason), until: isoOr(rec(a.cooldown_set).until, epoch) } : null, durationMs: numOrNull(a.duration_ms), index: num(a.index) - 1, outcome: outcomeOf(a.outcome), startedAt: isoOr(a.started_at, epoch), upstreamStatus: numOrNull(a.upstream_status), windows: attemptWindows(a.windows) })),
    capturedBodies: bool(r.captured_bodies), costUsd: num(r.cost_usd), durationMs: numOrNull(r.duration_ms), errorCode: strOrNull(r.error_code), id: str(r.id), keyId: strOrNull(r.key_id),
    kind: (['count_tokens', 'inference', 'probe'] as const).find((k) => k === str(r.kind)) ?? 'inference', model: str(r.model),
    outcome: (['canceled', 'exhausted', 'failed', 'rejected', 'succeeded'] as const).find((o) => o === str(r.outcome)) ?? 'failed',
    poolId: strOrNull(r.pool_id), sessionId: strOrNull(r.session_id), status: numOrNull(r.status), transport: str(r.transport) === 'sse' ? 'sse' : 'json', ttfbMs: numOrNull(r.ttfb_ms), ttftMs: numOrNull(r.ttft_ms), usage: usageOf(r.usage), userAgent: strOrNull(r.user_agent),
  };
}

export const decodeRequestPage = (v: unknown): RequestPage => ({ nextCursor: strOrNull(rec(v).next_cursor), requests: list(rec(v).requests).map(decodeRequest) });
export const decodeBodies = (v: unknown): CapturedBodies => ({ request: rec(v).request, requestId: str(rec(v).request_id), response: rec(v).response });

const record = <K extends string>(keys: readonly K[], v: unknown, empty: () => Record<K, number>): Record<K, number> => {
  const out = empty();
  const m = rec(v);
  for (const k of keys) out[k] = num(m[k]);
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
    attempts: num(g.attempts), cacheHits: num(g.cache_hits), cacheMisses: num(g.cache_misses), cacheMissReasons: record(CACHE_MISS_REASONS, g.cache_miss_reasons, emptyCacheMissReasons),
    cost: { cacheRead: num(c.cache_read), cacheWrite: num(c.cache_write), input: num(c.input), output: num(c.output) }, costUsd: num(g.cost_usd),
    errors: Object.fromEntries(Object.entries(rec(g.errors)).map(([k, n]) => [k, num(n)])), exhausted: num(g.exhausted), expiredBetweenTurns: num(g.expired_between_turns), failed: num(g.failed), failovers: num(g.failovers),
    histograms: { durationMs: histogram(h.duration_ms), outputTps: histogram(h.output_tps), ttfbMs: histogram(h.ttfb_ms), ttftMs: histogram(h.ttft_ms) },
    keptWarm: { requests: num(rec(g.kept_warm).requests), tokens: num(rec(g.kept_warm).tokens) }, key: str(g.key), label: str(g.label, str(g.key)),
    percentiles: { durationMs: quartet(p.duration_ms), outputTps: quartet(p.output_tps), ttfbMs: quartet(p.ttfb_ms), ttftMs: quartet(p.ttft_ms) },
    probes: { failed: num(rec(g.probes).failed), migrated: num(rec(g.probes).migrated), ok: num(rec(g.probes).ok), skipped: num(rec(g.probes).skipped) },
    rateLimited: num(g.rate_limited), rejected: num(g.rejected), requests: num(g.requests),
    savings: { grossUsd: num(s.gross_usd), probeCostUsd: num(s.probe_cost_usd), wastedProbeReasons: record(WASTED_PROBE_REASONS, s.wasted_probe_reasons, emptyWastedProbeReasons), wastedProbeUsd: num(s.wasted_probe_usd), wastedProbes: num(s.wasted_probes) },
    succeeded: num(g.succeeded), tokens: { cacheRead: num(t.cache_read), cacheWrite1h: num(t.cache_write_1h), cacheWrite5m: num(t.cache_write_5m), output: num(t.output), uncachedInput: num(t.uncached_input) }, uncached: num(g.uncached),
  };
}
export const decodeAnalyticsPoint = (v: unknown): AnalyticsPoint => ({ at: isoOr(rec(v).at, epoch), groups: list(rec(v).groups).map(decodeAnalyticsGroup) });

export function decodeAccountRollups(v: unknown): AccountRollup[] {
  return list(rec(v).accounts).map(rec).flatMap((a) => list(a.hours).map(rec).map((h) => ({
    accountId: str(a.account_id), cooldownSeconds: num(h.cooldown_seconds), hour: isoOr(h.hour, epoch), overageSeconds: num(h.overage_seconds),
    peak: { '5h': num(h.utilization_5h_max), '7d': num(h.utilization_7d_max), '7d_fable': numOrNull(h.utilization_7d_oi_max) },
    pins: { created: num(h.pins_created), migratedIn: num(h.pins_migrated_in), migratedOut: num(h.pins_migrated_out) },
  })));
}

export function decodeDashboard(v: unknown): Overview {
  const d = rec(v);
  return { cost24h: num(d.cost_24h), errorRate24h: numOrNull(d.error_rate_24h), hourly: list(d.hourly).map(rec).map((h) => ({ at: isoOr(h.at, epoch), cost: num(h.cost), errors: num(h.errors), requests: num(h.requests), tokens: num(h.tokens) })), requests24h: num(d.requests_24h), tokens24h: num(d.tokens_24h), tokensCached24h: num(d.tokens_cached_24h), topErrorCode: strOrNull(d.top_error_code) };
}

const companion = (v: unknown): Session['companion'] => (str(v) === 'reporting' ? 'reporting' : str(v) === 'silent' ? 'silent' : 'none');
const agentState = (v: unknown): Session['keepalive']['state'] => (['busy', 'idle', 'parked'] as const).find((s) => s === str(v)) ?? null;

export function decodeSession(v: unknown): Session {
  const s = rec(v);
  const k = rec(s.keepalive);
  return { accountId: strOrNull(s.account_id), agents: num(s.agents), companion: companion(s.companion), companionSince: isoOrNull(s.companion_since), createdAt: isoOrNull(s.created_at), epoch: numOrNull(s.epoch), keepalive: { nextProbeAt: isoOrNull(k.next_probe_at), state: agentState(k.state) }, keyId: str(s.key_id), lastRequestAt: isoOrNull(s.last_request_at), lastUsedAt: isoOrNull(s.last_used_at) ?? isoOrNull(s.last_request_at), model: strOrNull(s.model), pinned: bool(s.pinned), requests: num(s.requests), sessionId: str(s.session_id) };
}

const RECIPE_REASONS = ['affinity_too_short', 'awaiting_cache_evidence', 'cap_reached', 'cold_probes', 'cooled', 'idle_limit', 'net_negative', 'oauth_unverified', 'overage', 'paused', 'probe_error', 'quota_exhausted', 'reauth', 'retrying', 'storage_limit', 'unsupported_shape'] as const;
export function decodeRecipe(v: unknown): KeepaliveRecipe {
  const r = rec(v);
  return { accountId: strOrNull(r.account_id), agentId: str(r.agent_id), keyId: str(r.key_id), lastRequestAt: isoOrNull(r.last_request_at), model: str(r.model), nextProbeAt: isoOrNull(r.next_probe_at), poolId: str(r.pool_id), reason: RECIPE_REASONS.find((x) => x === str(r.reason)) ?? null, sessionId: str(r.session_id), state: (['armed', 'held', 'probing'] as const).find((x) => x === str(r.state)) ?? 'armed', ttlSeconds: num(r.ttl_seconds) };
}

export function decodeKeepaliveOverview(v: unknown): KeepaliveOverview {
  const o = rec(v);
  const p = rec(o.probes_24h);
  return { agents: { busy: num(rec(o.agents).busy), idle: num(rec(o.agents).idle), parked: num(rec(o.agents).parked) }, costToday: list(o.cost_today).map(rec).map((c) => ({ capUsd: numOrNull(c.cap_usd), poolId: str(c.pool_id), usd: num(c.usd) })), expiredBetweenTurns24h: num(o.expired_between_turns_24h), netSaved7dUsd: num(o.net_saved_7d_usd), probes24h: { failed: num(p.failed), migrated: num(p.migrated), ok: num(p.ok), skipped: num(p.skipped) }, sessions: num(o.sessions), sessionsWithCompanion: num(o.sessions_with_companion) };
}

const ALERT_KINDS = ['ask_user_question', 'client_drift', 'funding_required', 'keepalive_cost_cap', 'pool_exhausted', 'reauth_required', 'refresh_failed', 'utilization_above'] as const;
const scopeOf = (v: unknown): AlertRule['scope'] => { const s = rec(v); return { id: strOrNull(s.id), type: (['account', 'all', 'any_account', 'key', 'pool'] as const).find((t) => t === str(s.type)) ?? 'all' }; };
export function decodeAlertRule(v: unknown): AlertRule {
  const r = rec(v);
  return { createdAt: isoOr(r.created_at, epoch), enabled: bool(r.enabled), hasSecret: bool(r.has_secret), id: str(r.id), kind: ALERT_KINDS.find((k) => k === str(r.kind)) ?? 'utilization_above', scope: scopeOf(r.scope), threshold: numOrNull(r.threshold), updatedAt: isoOr(r.updated_at, epoch), webhookUrl: str(r.webhook_url), windowId: (['5h', '7d', '7d_oi'] as const).find((w) => w === str(r.window)) ?? null };
}
export const decodeDelivery = (v: unknown): Delivery => ({ at: isoOr(rec(v).at, epoch), attempt: num(rec(v).attempt), error: strOrNull(rec(v).error), state: str(rec(v).state) === 'resolved' ? 'resolved' : 'opened', status: numOrNull(rec(v).status) });
export function decodeAlertEvent(v: unknown): AlertEvent {
  const e = rec(v);
  return { deliveries: list(e.deliveries).map(decodeDelivery), detail: e.detail && typeof e.detail === 'object' ? (e.detail as Record<string, unknown>) : null, id: str(e.id), kind: ALERT_KINDS.find((k) => k === str(e.kind)) ?? 'utilization_above', openedAt: isoOr(e.opened_at, epoch), resolvedAt: isoOrNull(e.resolved_at), ruleId: str(e.rule_id), state: str(e.state) === 'resolved' ? 'resolved' : 'open', subject: str(e.subject) };
}

const price = (m: Rec): ModelPrice => ({ cacheReadPerM: num(m.cache_read_per_m), cacheWrite1hPerM: num(m.cache_write_1h_per_m), cacheWrite5mPerM: num(m.cache_write_5m_per_m), inputPerM: num(m.input_per_m), model: str(m.model), outputPerM: num(m.output_per_m) });
export const decodePricing = (v: unknown): Settings['pricing'] => ({ effectiveDate: new Date(str(rec(v).effective, '1970-01-01')).toISOString(), models: list(rec(v).models).map(rec).map(price), source: str(rec(v).source) });
export const decodePriceImport = (v: unknown): PriceImportPreview => ({ changes: list(rec(v).changes).length, inFile: num(rec(v).in_file), models: list(rec(v).models).map(rec).map(price), served: num(rec(v).served), token: str(rec(v).token) });

export function decodeSettings(v: unknown): Settings {
  const s = rec(v);
  const k = rec(s.keepalive);
  const r = rec(s.retention);
  const l = rec(s.listeners);
  return {
    about: { profile: str(rec(s.about).profile), publicUrl: strOrNull(rec(s.about).public_url), version: str(rec(s.about).version) },
    access: { adminConfigured: bool(rec(s.access).admin_configured, true), viewerConfigured: bool(rec(s.access).viewer_configured) },
    keepalive: { leaseSeconds: num(k.lease_seconds), maxAgents: num(k.max_agents), maxRequestBytes: num(k.max_request_bytes), maxRetainedBytes: num(k.max_retained_bytes), maxSessions: num(k.max_sessions), maxWorkers: num(k.max_workers), oauthPrewarmVerified: bool(k.oauth_prewarm_verified) },
    lastSyncAt: isoOrNull(s.last_sync_at),
    listeners: { admin: str(l.admin), metrics: str(l.metrics), proxy: str(l.proxy) },
    pricing: decodePricing(s.pricing),
    retention: { bodyCaptureMaxHours: num(r.body_capture_max_hours), ledgerDays: num(r.ledger_days), quotaSamplesDays: num(r.quota_samples_days) },
    sessions: { expireAfterSeconds: num(rec(s.sessions).expire_after_seconds) },
  };
}

const liveAttempt = (v: unknown): LiveAttempt => { const a = rec(v); return { accountId: str(a.account_id), cooldownSet: a.cooldown_set ? { reason: str(rec(a.cooldown_set).reason), until: isoOr(rec(a.cooldown_set).until, epoch) } : null, index: num(a.index) - 1, outcome: str(a.outcome) === 'in_flight' ? 'in_flight' : outcomeOf(a.outcome), startedAt: isoOr(a.started_at, epoch), upstreamStatus: numOrNull(a.upstream_status), windows: attemptWindows(a.windows) }; };
const livePhases = (v: unknown) => { const p = rec(v); const at = (k: string) => isoOrNull(p[k]) ?? undefined; return { finishedAt: at('finished_at'), firstContentAt: at('first_content_at'), headAt: at('head_at'), receivedAt: isoOr(p.received_at, epoch), selectedAt: at('selected_at'), streamEndAt: at('stream_end_at'), writtenAt: at('written_at') }; };
export function decodeLiveRequest(v: unknown): LiveRequest {
  const r = rec(v);
  return { accountId: strOrNull(r.account_id), agentId: str(r.agent_id, 'main'), attempts: list(r.attempts).map(liveAttempt), costUsd: num(r.cost_usd), errorCode: strOrNull(r.error_code), id: str(r.id), keyId: strOrNull(r.key_id), kind: (['count_tokens', 'inference', 'probe'] as const).find((k) => k === str(r.kind)) ?? 'inference', model: str(r.model), outputTps: numOrNull(r.output_tps), phases: livePhases(r.phases), poolId: strOrNull(r.pool_id), preview: strOrNull(r.preview), sessionId: strOrNull(r.session_id), state: str(r.state) === 'in_flight' ? 'in_flight' : 'finished', status: numOrNull(r.status), transport: str(r.transport) === 'sse' ? 'sse' : 'json', usage: usageOf(r.usage) };
}
const liveMetric = (v: unknown): LiveMetric => ({ quality: (['derived', 'estimated', 'measured'] as const).find((q) => q === str(rec(v).quality)) ?? 'measured', series: list(rec(v).series).map(numOrNull), value: numOrNull(rec(v).value) });
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

export const encodeKey = (i: KeyInput) => ({ allowed_models: i.allowedModels, body_capture_hours: i.bodyCaptureHours, keepalive_opt_out: i.keepaliveOptOut, label: i.name, notes: i.notes, pool_id: i.poolId });
export const encodePool = (i: PoolInput) => ({
  affinity_ttl_seconds: i.affinityTtlSeconds, api_key_sub_strategy: i.apiKeySubStrategy, body_capture_hours: i.bodyCaptureHours, default_allowed_models: i.defaultAllowedModels, enabled: i.status === 'active',
  keepalive: { cold_probe_limit: i.keepalive.coldProbeLimit, enabled: i.keepalive.enabled, jitter_seconds_1h: i.keepalive.jitterSeconds1h, jitter_seconds_5m: i.keepalive.jitterSeconds5m, lead_seconds_1h: i.keepalive.leadSeconds1h, lead_seconds_5m: i.keepalive.leadSeconds5m, max_idle_seconds: i.keepalive.maxIdleSeconds, max_idle_ticks: i.keepalive.maxIdleTicks, net_negative_grace_probes: i.keepalive.netNegativeGraceProbes, probe_cost_cap_usd_per_day: i.keepalive.costCapUsdPerDay, retry_max_seconds: i.keepalive.retryMaxSeconds, retry_seconds: i.keepalive.retrySeconds, stop_when_net_negative: i.keepalive.stopWhenNetNegative },
  label: i.name, members: i.members.map((m) => ({ account_id: m.accountId, priority: m.priority })), migrate_above_utilization: i.migrateAboveUtilization, overage_policy: i.overagePolicy,
  // No `prefer_subscription_effective`: the route computes it from the stored flag and the strategy, and an input
  // that carried one could contradict the strategy it was saved with.
  prefer_subscription: i.preferSubscription, strategy: i.strategy,
});
export const encodeRule = (i: AlertRuleInput) => ({ enabled: i.enabled, kind: i.kind, scope: i.scope, ...(i.secret === undefined ? {} : { secret: i.secret }), threshold: i.threshold, webhook_url: i.webhookUrl, window: i.windowId });
