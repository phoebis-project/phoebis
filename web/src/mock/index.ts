import type { Api, CreateKeyInput, KeyInput, PoolInput } from '@/api/client';
import type { ApiKey, CreatedApiKey, LiveEvent, PairingInput, PairingPreview, PairingSession, Pool, RequestLog, RequestQuery, ServedModel, Settings } from '@/api/types';
import { ACCOUNTS, QUOTA_HISTORY, QUOTA_SNAPSHOTS } from './accounts';
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
  private keys: ApiKey[] = [...KEYS];
  private pairings = new Map<string, string | null>();
  private pools: Pool[] = POOLS.map((p) => ({ ...p }));
  private secretRng = createRng(Date.now() % 100_000);
  private settings: Settings = { ...SETTINGS };

  private token(n: number) {
    return Array.from({ length: n }, () => 'abcdefghijklmnopqrstuvwxyz0123456789'[this.secretRng.int(0, 35)]).join('');
  }

  async completePairing(input: PairingInput): Promise<PairingPreview> {
    const submitted = 'url' in input ? fromCallbackUrl(input.url) : input;
    if (!submitted) throw new Error('That URL carries no authorization code and state. Paste the whole address the browser landed on, or the code on its own.');
    if (!this.pairings.has(submitted.state)) throw new Error('Pairing expired. Start again.');
    const accountId = this.pairings.get(submitted.state);
    const existing = accountId ? this.accounts.find((a) => a.id === accountId) : null;
    return latency({ email: existing?.email ?? 'new.login@example.com', organization: existing ? `${existing.label} workspace` : 'Personal', state: submitted.state }, 500);
  }

  confirmPairing(state: string) {
    this.pairings.delete(state);
    return latency(undefined, 250);
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

  listAlertEvents() {
    return latency(ALERT_EVENTS);
  }

  listAlertRules() {
    return latency(ALERT_RULES);
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

  listRequests(query: RequestQuery = {}) {
    return latency(REQUESTS.filter((r) => matchesQuery(r, query)));
  }

  listRollups(from: string, to: string) {
    const fromMs = Date.parse(from);
    const toMs = Date.parse(to);
    return latency(ALL_ROLLUPS.filter((r) => Date.parse(r.hour) >= fromMs && Date.parse(r.hour) <= toMs));
  }

  listSessions() {
    return latency(SESSIONS);
  }

  previewPriceImport(_url: string) {
    return latency(
      { changes: countPriceChanges(this.settings.pricing.models, IMPORTED_PRICES), inFile: IMPORTED_CATALOG.length, models: IMPORTED_PRICES, served: IMPORTED_PRICES.length, token: `imp_${this.token(32)}` },
      400,
    );
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

  updateServedModels(accountId: string, models: ServedModel[]) {
    const idx = this.accounts.findIndex((a) => a.id === accountId);
    if (idx < 0) return Promise.reject(new Error(`Unknown account ${accountId}`));
    const next = { ...this.accounts[idx], servedModels: models };
    this.accounts = this.accounts.map((a, i) => (i === idx ? next : a));
    return latency(next, 250);
  }
}
