import type { Api, CreateKeyInput, KeyInput, PoolInput } from '@/api/client';
import { ApiError } from '@/api/http';
import type {
  Account, AccountInput, AlertDeliveryTest, AlertEvent, AlertRule, AlertRuleInput, AnalyticsQuery, ApiKey, CreatedApiKey, Delivery, LiveEvent, PairingInput, PairingPreview, PairingSession, Pool, RequestLog, RequestPage, RequestQuery,
  ServedModel, Session, SessionInfo, Settings, SyncResult,
} from '@/api/types';
import { ACCOUNTS, QUOTA_HISTORY, QUOTA_SNAPSHOTS } from './accounts';
import { breakdownOf, seriesOf } from './analytics';
import { CATALOG, IMPORTED_CATALOG, IMPORTED_PRICES, countPriceChanges } from './catalog';
import { liveFeed } from './live';
import { ALERT_EVENTS, ALERT_RULES, KEEPALIVE_OVERVIEW, RECIPES, SESSIONS, SETTINGS } from './operations';
import { KEYS, POOLS } from './pools';
import { ACCOUNT_ROLLUPS, ALL_ROLLUPS, REQUESTS, capturedBodiesFor, requestById } from './requests';
import { createRng } from './rng';
import { DAY, MINUTE, NOW, ago, ahead, iso } from './time';

// A short delay keeps loading states honest without making the mock feel slow.
const latency = <T,>(value: T, ms = 120): Promise<T> => new Promise((resolve) => setTimeout(() => resolve(value), ms));

// The registered redirect is a loopback port on the machine running Claude Code,
// so the callback the operator pastes carries both halves of the exchange.
const PAIRING_REDIRECT_URI = 'http://localhost:54545/callback';

/** The smoke runner writes this so every mount starts signed in instead of on the login screen. */
const SEEDED_SESSION_KEY = 'phoebis.mockSession';
const seededSession = (): boolean => {
  try {
    return localStorage.getItem(SEEDED_SESSION_KEY) !== null;
  } catch {
    return false;
  }
};

/** The role the mock build previews, which the alert events route reads to decide whether to blank a question. */
const PREVIEW_ROLE_KEY = 'phoebis.role';
const previewsViewer = (): boolean => {
  try {
    return localStorage.getItem(PREVIEW_ROLE_KEY) === 'viewer';
  } catch {
    return false;
  }
};

/** Set to stand in for a fresh installation, whose alert lists are empty until an operator writes a rule. */
const EMPTY_STORE_KEY = 'phoebis.mockEmpty';
const emptyStore = (): boolean => {
  try {
    return localStorage.getItem(EMPTY_STORE_KEY) === '1';
  } catch {
    return false;
  }
};

function fromCallbackUrl(url: string): { code: string; state: string } | null {
  let params: URLSearchParams;
  try {
    params = new URL(url).searchParams;
  } catch {
    return null;
  }
  const code = params.get('code');
  const state = params.get('state');
  return code && state ? { code, state } : null;
}

/**
 * What the events route hands a viewer: the question text came out of a captured body, which a viewer may not read
 * there either, so the route blanks it and says it did.
 */
const redactForViewer = (e: AlertEvent): AlertEvent =>
  e.detail !== null && 'question' in e.detail ? { ...e, detail: { ...e.detail, question: null, question_redacted: true } } : e;

function matchesQuery(r: RequestLog, q: RequestQuery): boolean {
  // A request the router never attributed matches no filter on that field.
  if (q.keyIds?.length && (r.keyId === null || !q.keyIds.includes(r.keyId))) return false;
  if (q.poolIds?.length && (r.poolId === null || !q.poolIds.includes(r.poolId))) return false;
  if (q.accountIds?.length && (r.accountId === null || !q.accountIds.includes(r.accountId))) return false;
  if (q.models?.length && !q.models.includes(r.model)) return false;
  if (q.statuses?.length && (r.status === null || !q.statuses.includes(r.status))) return false;
  if (q.kind && r.kind !== q.kind) return false;
  if (q.from && Date.parse(r.at) < Date.parse(q.from)) return false;
  if (q.to && Date.parse(r.at) > Date.parse(q.to)) return false;
  if (q.search) {
    const needle = q.search.toLowerCase();
    const hay = [r.id, r.sessionId ?? '', r.keyId ?? '', r.accountId ?? '', r.model, r.errorCode ?? '', r.agentId ?? ''].join(' ').toLowerCase();
    if (!hay.includes(needle)) return false;
  }
  return true;
}

export class MockApi implements Api {
  private accounts = ACCOUNTS.map((a) => ({ ...a }));
  private events: AlertEvent[] = ALERT_EVENTS.map((e) => ({ ...e }));
  private keys: ApiKey[] = [...KEYS];
  private pairings = new Map<string, string | null>();
  private pools: Pool[] = POOLS.map((p) => ({ ...p }));
  private rules: AlertRule[] = ALERT_RULES.map((r) => ({ ...r }));
  private secretRng = createRng(Date.now() % 100_000);
  private session: SessionInfo | null = null;
  private sessions: Session[] = [...SESSIONS];
  private settings: Settings = { ...SETTINGS };

  private token(n: number) {
    return Array.from({ length: n }, () => 'abcdefghijklmnopqrstuvwxyz0123456789'[this.secretRng.int(0, 35)]).join('');
  }

  private account(accountId: string): Account {
    const a = this.accounts.find((x) => x.id === accountId);
    if (!a) throw new ApiError(404, `Unknown account ${accountId}`, 'not_found');
    return a;
  }

  private putAccount(next: Account): Account {
    this.accounts = this.accounts.map((a) => (a.id === next.id ? next : a));
    return next;
  }

  cancelPairing(pairingId: string) {
    this.pairings.delete(pairingId);
    return latency(undefined, 150);
  }

  async clearCooldown(accountId: string, scope: string) {
    const a = this.account(accountId);
    const cooldowns = a.cooldowns.filter((c) => c.windowId !== scope && c.reason !== scope);
    this.putAccount({ ...a, cooldowns, status: cooldowns.length ? a.status : 'ready' });
    return latency(undefined, 200);
  }

  async clearFunding(accountId: string) {
    const a = this.account(accountId);
    this.putAccount({ ...a, extraUsage: a.extraUsage ? { ...a.extraUsage, disabledReason: null } : null, status: a.status === 'funding_required' ? 'ready' : a.status });
    return latency(undefined, 200);
  }

  async completePairing(input: PairingInput): Promise<PairingPreview> {
    const submitted = 'url' in input ? fromCallbackUrl(input.url) : input;
    if (!submitted) throw new Error('That URL carries no authorization code and state. Paste the whole address the browser landed on, or the code on its own.');
    if (!this.pairings.has(submitted.state)) throw new Error('Pairing expired. Start again.');
    const accountId = this.pairings.get(submitted.state);
    const existing = accountId ? this.accounts.find((a) => a.id === accountId) : null;
    return latency({ email: existing?.email ?? 'new.login@example.com', organization: existing ? `${existing.label} workspace` : 'Personal', state: submitted.state }, 500);
  }

  confirmPairing(pairingId: string) {
    this.pairings.delete(pairingId);
    return latency(undefined, 250);
  }

  confirmPriceImport(_token: string) {
    this.settings = { ...this.settings, pricing: { ...this.settings.pricing, effectiveDate: iso(NOW), models: IMPORTED_PRICES, source: 'imported' } };
    return latency(this.settings.pricing, 300);
  }

  createAccount(input: AccountInput): Promise<Account> {
    const account: Account = {
      catalogSyncedAt: null,
      cooldowns: [],
      credential: { accessTokenExpiresAt: null, lastError: null, lastRefreshAt: null, refreshTokenExpiresAt: null },
      email: null,
      extraUsage: null,
      fableWindow: null,
      id: input.id,
      kind: input.kind,
      label: input.label,
      plan: null,
      pools: [],
      requests: { last24h: 0, last7d: 0, lifetime: 0 },
      routingEnabled: true,
      servedModels: [],
      status: input.kind === 'oauth' ? 'unpaired' : 'ready',
      subscription: null,
      windows: [],
    };
    this.accounts = [...this.accounts, account];
    return latency(account, 300);
  }

  createAlertRule(input: AlertRuleInput): Promise<AlertRule> {
    const rule: AlertRule = { createdAt: iso(NOW), enabled: input.enabled, hasSecret: Boolean(input.secret), id: `rule_${this.token(6)}`, kind: input.kind, scope: input.scope, threshold: input.threshold, updatedAt: iso(NOW), webhookUrl: input.webhookUrl, windowId: input.windowId };
    this.rules = [...this.rules, rule];
    return latency(rule, 250);
  }

  async createKey(input: CreateKeyInput): Promise<CreatedApiKey> {
    const body = this.token(40);
    const secret = `phoebis-${body}`;
    const key: ApiKey = {
      id: `key_${body.slice(0, 6)}`,
      name: input.name,
      prefix: secret.slice(0, 8),
      poolId: input.poolId,
      allowedModels: input.allowedModels,
      status: 'active',
      createdAt: iso(NOW),
      expiresAt: input.expiresInDays ? iso(NOW + input.expiresInDays * DAY) : null,
      keepaliveOptOut: input.keepaliveOptOut,
      lastUsedAt: null,
      notes: input.notes,
      bodyCaptureHours: input.bodyCaptureHours,
      usage7d: { cost: 0, requests: 0, tokens: 0 },
      lifetime: { cost: 0, requests: 0 },
    };
    this.keys = [key, ...this.keys];
    return latency({ key, secret }, 300);
  }

  createPool(input: PoolInput): Promise<Pool> {
    const pool: Pool = {
      ...input,
      activePins: 0,
      cost24h: 0,
      id: input.name.toLowerCase().replace(/[^a-z0-9]+/g, '-'),
      members: input.members.map((m) => ({ ...m, eligible: 'eligible' as const })),
      requests24h: 0,
      traffic24h: [],
    };
    this.pools = [...this.pools, pool];
    return latency(pool, 300);
  }

  deleteAlertRule(id: string) {
    this.rules = this.rules.filter((r) => r.id !== id);
    return latency(undefined, 200);
  }

  deleteKey(id: string) {
    this.keys = this.keys.filter((k) => k.id !== id);
    return latency(undefined, 200);
  }

  deletePool(id: string) {
    this.pools = this.pools.filter((p) => p.id !== id);
    return latency(undefined, 200);
  }

  async disconnectAccount(accountId: string) {
    const a = this.account(accountId);
    this.putAccount({ ...a, credential: { accessTokenExpiresAt: null, lastError: null, lastRefreshAt: null, refreshTokenExpiresAt: null }, email: null, status: 'unpaired' });
    return latency(undefined, 250);
  }

  getAnalyticsBreakdown(query: AnalyticsQuery) {
    return latency(breakdownOf(ALL_ROLLUPS, query));
  }

  getAnalyticsSeries(query: AnalyticsQuery) {
    return latency(seriesOf(ALL_ROLLUPS, query));
  }

  getCapturedBodies(requestId: string) {
    const r = requestById.get(requestId);
    return latency(r ? capturedBodiesFor(r) : null);
  }

  getKeepaliveOverview() {
    return latency(KEEPALIVE_OVERVIEW);
  }

  getOverview() {
    const last24h = REQUESTS.filter((r) => Date.parse(r.at) >= NOW - DAY);
    const hourlyMap = new Map<string, { at: string; cost: number; errors: number; requests: number; tokens: number }>();
    for (let i = 23; i >= 0; i -= 1) {
      const at = iso(Math.floor((NOW - i * 3_600_000) / 3_600_000) * 3_600_000);
      hourlyMap.set(at, { at, cost: 0, errors: 0, requests: 0, tokens: 0 });
    }
    let tokens = 0;
    let cached = 0;
    let cost = 0;
    const errorCodes = new Map<string, number>();
    for (const r of last24h) {
      const at = iso(Math.floor(Date.parse(r.at) / 3_600_000) * 3_600_000);
      const bucket = hourlyMap.get(at);
      const total = r.usage.input + r.usage.cacheRead + r.usage.cacheWrite5m + r.usage.cacheWrite1h + r.usage.output;
      tokens += total;
      cached += r.usage.cacheRead;
      cost += r.costUsd;
      if (r.errorCode) errorCodes.set(r.errorCode, (errorCodes.get(r.errorCode) ?? 0) + 1);
      if (bucket) {
        bucket.requests += 1;
        bucket.tokens += total;
        bucket.cost += r.costUsd;
        if (r.status !== 200) bucket.errors += 1;
      }
    }
    const top = [...errorCodes.entries()].sort((a, b) => b[1] - a[1])[0];
    return latency({
      cost24h: cost,
      errorRate24h: last24h.length ? last24h.filter((r) => r.status !== 200).length / last24h.length : 0,
      hourly: [...hourlyMap.values()],
      requests24h: last24h.length,
      tokens24h: tokens,
      tokensCached24h: cached,
      topErrorCode: top ? top[0] : null,
    });
  }

  getQuotaHistory(accountId: string) {
    return latency(QUOTA_HISTORY.filter((h) => h.accountId === accountId));
  }

  getRequest(id: string) {
    return latency(requestById.get(id) ?? null);
  }

  getSession(): Promise<SessionInfo | null> {
    if (this.session) return latency(this.session, 60);
    return latency(seededSession() ? { role: 'admin' as const, sessionId: 'mock-admin' } : null, 60);
  }

  getSettings() {
    return latency({ ...this.settings, lastSyncAt: ago(40 * 1000) });
  }

  listAccountRollups(from: string, to: string) {
    const fromMs = Date.parse(from);
    const toMs = Date.parse(to);
    return latency(ACCOUNT_ROLLUPS.filter((r) => Date.parse(r.hour) >= fromMs && Date.parse(r.hour) <= toMs));
  }

  listAccounts() {
    return latency(this.accounts);
  }

  listAlertDeliveries(eventId: string): Promise<Delivery[]> {
    return latency(this.events.find((e) => e.id === eventId)?.deliveries ?? []);
  }

  listAlertEvents(state?: 'open' | 'resolved') {
    const stored = emptyStore() ? [] : this.events;
    const events = previewsViewer() ? stored.map(redactForViewer) : stored;
    return latency(state ? events.filter((e) => e.state === state) : events);
  }

  listAlertRules() {
    return latency(emptyStore() ? [] : this.rules);
  }

  listCatalog() {
    return latency(CATALOG);
  }

  listKeepaliveRecipes() {
    return latency(RECIPES);
  }

  listKeys() {
    return latency(this.keys);
  }

  listPools() {
    return latency(this.pools);
  }

  listQuotaSnapshots() {
    return latency(QUOTA_SNAPSHOTS);
  }

  listRequestPage(query: RequestQuery = {}): Promise<RequestPage> {
    const matched = REQUESTS.filter((r) => matchesQuery(r, query));
    const start = query.cursor ? matched.findIndex((r) => r.id === query.cursor) + 1 : 0;
    const limit = query.limit ?? matched.length;
    const page = matched.slice(start, start + limit);
    // A cursor only rides along while rows are left, which is how the route signals the end of the list.
    return latency({ nextCursor: start + limit < matched.length ? (page[page.length - 1]?.id ?? null) : null, requests: page });
  }

  listRequests(query: RequestQuery = {}) {
    return latency(REQUESTS.filter((r) => matchesQuery(r, query)));
  }

  listRollups(from: string, to: string) {
    const fromMs = Date.parse(from);
    const toMs = Date.parse(to);
    return latency(ALL_ROLLUPS.filter((r) => Date.parse(r.hour) >= fromMs && Date.parse(r.hour) <= toMs));
  }

  listSessions() {
    return latency(this.sessions);
  }

  login(secret: string): Promise<SessionInfo> {
    if (secret !== 'admin' && secret !== 'viewer') return Promise.reject(new ApiError(401, 'that secret does not match', null));
    this.session = { role: secret, sessionId: `mock-${secret}` };
    return latency(this.session, 200);
  }

  logout() {
    this.session = null;
    return latency(undefined, 150);
  }

  async pauseAccount(accountId: string) {
    const a = this.account(accountId);
    this.putAccount({ ...a, routingEnabled: false, status: 'paused' });
    return latency(undefined, 250);
  }

  previewPriceImport(_url: string) {
    return latency(
      { changes: countPriceChanges(this.settings.pricing.models, IMPORTED_PRICES), inFile: IMPORTED_CATALOG.length, models: IMPORTED_PRICES, served: IMPORTED_PRICES.length, token: `imp_${this.token(32)}` },
      400,
    );
  }

  refreshUsage(_accountId: string) {
    return latency(undefined, 400);
  }

  releaseSession(keyId: string, sessionId: string) {
    this.sessions = this.sessions.filter((s) => !(s.keyId === keyId && s.sessionId === sessionId));
    return latency(undefined, 250);
  }

  async renameAccount(accountId: string, label: string) {
    return latency(this.putAccount({ ...this.account(accountId), label }), 250);
  }

  resolveAlertEvent(id: string): Promise<AlertEvent> {
    const event = this.events.find((e) => e.id === id);
    if (!event) return Promise.reject(new ApiError(404, `Unknown event ${id}`, 'not_found'));
    const next: AlertEvent = { ...event, resolvedAt: iso(NOW), state: 'resolved' };
    this.events = this.events.map((e) => (e.id === id ? next : e));
    return latency(next, 250);
  }

  async resumeAccount(accountId: string) {
    const a = this.account(accountId);
    this.putAccount({ ...a, routingEnabled: true, status: a.cooldowns.length ? 'cooldown' : 'ready' });
    return latency(undefined, 250);
  }

  revokeKey(id: string) {
    this.keys = this.keys.map((k) => (k.id === id ? { ...k, status: 'revoked' as const } : k));
    return latency(undefined, 250);
  }

  rotateKey(id: string): Promise<CreatedApiKey> {
    const key = this.keys.find((k) => k.id === id);
    if (!key) return Promise.reject(new ApiError(404, `Unknown key ${id}`, 'not_found'));
    const secret = `phoebis-${this.token(40)}`;
    const next: ApiKey = { ...key, prefix: secret.slice(0, 8) };
    this.keys = this.keys.map((k) => (k.id === id ? next : k));
    return latency({ key: next, secret }, 300);
  }

  setSessionExpiry(seconds: number) {
    this.settings = { ...this.settings, sessions: { expireAfterSeconds: seconds } };
    return latency(this.settings, 250);
  }

  startPairing(accountId: string | null): Promise<PairingSession> {
    const state = this.token(24);
    this.pairings.set(state, accountId);
    const params = new URLSearchParams({
      client_id: '9d1c250a-e61b-44d9-88ed-5944d1962f5e',
      code_challenge: this.token(43),
      code_challenge_method: 'S256',
      redirect_uri: PAIRING_REDIRECT_URI,
      response_type: 'code',
      scope: 'org:create_api_key user:profile user:inference',
      state,
    });
    return latency({ authorizeUrl: `https://claude.ai/oauth/authorize?${params.toString()}`, expiresAt: ahead(10 * MINUTE), id: state, redirectUri: PAIRING_REDIRECT_URI, state }, 200);
  }

  subscribeLive(handler: (event: LiveEvent) => void) {
    return liveFeed().subscribe(handler);
  }

  async syncCatalog(accountId: string): Promise<SyncResult> {
    const a = this.account(accountId);
    this.putAccount({ ...a, catalogSyncedAt: iso(NOW) });
    return latency({ added: [], error: null, missing: a.servedModels.filter((m) => !m.inCatalog).map((m) => m.upstreamId), seeded: false }, 500);
  }

  testAlertRule(id: string): Promise<AlertDeliveryTest> {
    const rule = this.rules.find((r) => r.id === id);
    if (!rule) return Promise.reject(new ApiError(404, `Unknown rule ${id}`, 'not_found'));
    return latency({ error: null, status: 200 }, 400);
  }

  updateAlertRule(id: string, input: AlertRuleInput): Promise<AlertRule> {
    const rule = this.rules.find((r) => r.id === id);
    if (!rule) return Promise.reject(new ApiError(404, `Unknown rule ${id}`, 'not_found'));
    // An absent secret leaves the stored one alone, so the badge only turns off when an empty string clears it.
    const hasSecret = input.secret === undefined ? rule.hasSecret : input.secret !== '';
    const next: AlertRule = { ...rule, enabled: input.enabled, hasSecret, kind: input.kind, scope: input.scope, threshold: input.threshold, updatedAt: iso(NOW), webhookUrl: input.webhookUrl, windowId: input.windowId };
    this.rules = this.rules.map((r) => (r.id === id ? next : r));
    return latency(next, 250);
  }

  updateKey(id: string, input: KeyInput): Promise<ApiKey> {
    const idx = this.keys.findIndex((k) => k.id === id);
    if (idx < 0) return Promise.reject(new Error(`Unknown key ${id}`));
    const next = { ...this.keys[idx], ...input };
    this.keys = this.keys.map((k, i) => (i === idx ? next : k));
    return latency(next, 250);
  }

  updatePool(id: string, input: PoolInput): Promise<Pool> {
    const idx = this.pools.findIndex((p) => p.id === id);
    if (idx < 0) return Promise.reject(new Error(`Unknown pool ${id}`));
    const next: Pool = { ...this.pools[idx], ...input, members: input.members.map((m) => ({ ...m, eligible: 'eligible' as const })) };
    this.pools = this.pools.map((p, i) => (i === idx ? next : p));
    return latency(next, 250);
  }

  async updateServedModels(accountId: string, models: ServedModel[]) {
    return latency(this.putAccount({ ...this.account(accountId), servedModels: models }), 250);
  }

  async verifyAccount(accountId: string) {
    this.account(accountId);
    return latency({ ok: true, status: 200 }, 400);
  }
}
