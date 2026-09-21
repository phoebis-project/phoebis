import type { Api, CreateKeyInput, KeyInput, PoolInput } from './client';
import * as d from './decode';
import type { Account, AccountInput, AlertDeliveryTest, AlertEvent, AlertRule, AlertRuleInput, AnalyticsGroup, AnalyticsPoint, AnalyticsQuery, ApiKey, CreatedApiKey, LiveEvent, PairingInput, PairingPreview, QuotaSnapshot, RequestQuery, ServedModel, SessionInfo } from './types';

export class ApiError extends Error {
  code: string | null;
  status: number;
  constructor(status: number, message: string, code: string | null) {
    super(message);
    this.code = code;
    this.status = status;
  }
}

/** What a failed call reports: the route it was, and the server's own message where there was one. */
export interface Failure {
  message: string | null;
  path: string;
  /** 0 when the router never answered, so a caller can word a transport error differently. */
  status: number;
}

interface Hooks {
  onFailure: (failure: Failure) => void;
  onForbidden: (message: string) => void;
  onUnauthorized: () => void;
}
let hooks: Hooks = { onFailure: () => undefined, onForbidden: () => undefined, onUnauthorized: () => undefined };
/**
 * The session provider installs these once; a 401 anywhere returns the app to the login, a 403 names the
 * refusal, and everything else reaches the shell's toast from here rather than from each page.
 */
export const setHooks = (next: Hooks) => {
  hooks = next;
};

/** The one place a failed call is reported from, so the mock build can stand in for the transport too. */
export const reportFailure = (failure: Failure) => hooks.onFailure(failure);

export async function call<T = unknown>(method: string, path: string, body?: unknown, raw = false): Promise<T> {
  let response: Response;
  try {
    response = await fetch(path, { body: body === undefined ? undefined : JSON.stringify(body), credentials: 'same-origin', headers: body === undefined ? {} : { 'content-type': 'application/json' }, method });
  } catch (e) {
    // A router that never answered: the page below draws its error state, and the hook says which route.
    const message = e instanceof Error ? e.message : String(e);
    reportFailure({ message, path, status: 0 });
    throw new ApiError(0, message, null);
  }
  if (response.status === 204) return undefined as T;
  const text = await response.text();
  let parsed: unknown = null;
  try {
    parsed = text ? JSON.parse(text) : null;
  } catch {
    parsed = null;
  }
  if (!response.ok) {
    const answered = parsed && typeof parsed === 'object' && typeof (parsed as { error?: unknown }).error === 'string' ? (parsed as { error: string }).error : null;
    const message = answered ?? `${method} ${path} answered ${response.status}`;
    // The login route's own 401 is the wrong secret, not a lost session.
    if (response.status === 401 && !(method === 'POST' && path === '/admin/session')) hooks.onUnauthorized();
    else if (response.status === 403) hooks.onForbidden(message);
    // A 404 is how the routes answer for a request or a body the router never kept, which the page reading
    // it draws as a state of its own; everything else is a failure the operator has to be told about.
    else if (response.status !== 404) reportFailure({ message: answered, path, status: response.status });
    throw new ApiError(response.status, message, response.headers.get('x-phoebis-error-code'));
  }
  return (raw ? text : parsed) as T;
}

const qs = (params: Record<string, string | number | boolean | string[] | undefined>) => {
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v === undefined || (Array.isArray(v) && v.length === 0)) continue;
    q.set(k, Array.isArray(v) ? v.join(',') : String(v));
  }
  const s = q.toString();
  return s ? `?${s}` : '';
};
const range = (from: string, to: string) => `${Date.parse(from)},${Date.parse(to)}`;
// `qs` joins a list with commas, which a pattern may hold; both pattern lists travel joined by newline instead.
const analytics = (q: AnalyticsQuery) => qs({ accounts: q.accountIds, group_by: q.groupBy, include_probes: q.includeProbes, keys: q.keyIds, kinds: q.kinds, models: q.models, pools: q.poolIds, range: range(q.from, q.to), user_agent_groups: q.userAgentGroups?.join('\n'), user_agents: q.userAgents?.join('\n') });

export class HttpApi implements Api {
  cancelPairing = (pairingId: string) => call<void>('DELETE', `/admin/oauth/sessions/${enc(pairingId)}`);
  clearCooldown = (accountId: string, scope: string) => call<void>('DELETE', `/admin/accounts/${enc(accountId)}/cooldowns/${enc(scope)}`);
  clearFunding = (accountId: string) => call<void>('POST', `/admin/accounts/${enc(accountId)}/clear-funding`);
  completePairing = async (pairingId: string, input: PairingInput): Promise<PairingPreview> => {
    const r = await call<{ pending: { email: string; organization_name: string | null } }>('POST', `/admin/oauth/sessions/${enc(pairingId)}/callback`, input);
    return { email: r.pending.email, organization: r.pending.organization_name ?? 'Personal', state: pairingId };
  };
  confirmPairing = (pairingId: string) => call<void>('POST', `/admin/oauth/sessions/${enc(pairingId)}/confirm`);
  confirmPriceImport = async (token: string) => d.decodePricing(await call('POST', '/admin/pricing/import/confirm', { token }));
  createAccount = async (input: AccountInput): Promise<Account> => d.decodeAccount(await call('POST', '/admin/accounts', { id: input.id, kind: input.kind, label: input.label, secret: input.secret }));
  createAlertRule = async (input: AlertRuleInput): Promise<AlertRule> => d.decodeAlertRule(await call('POST', '/admin/alerts/rules', d.encodeRule(input)));
  createKey = async (input: CreateKeyInput): Promise<CreatedApiKey> => {
    const expires_at = input.expiresInDays === null ? null : Date.now() + input.expiresInDays * 86_400_000;
    const r = await call<Record<string, unknown>>('POST', '/admin/keys', { ...d.encodeKey(input), expires_at, id: slug(input.name) });
    return { key: d.decodeKey(r), secret: String(r.secret) };
  };
  createPool = async (input: PoolInput) => d.decodePool(await call('POST', '/admin/pools', { ...d.encodePool(input), id: slug(input.name) }));
  deleteAlertRule = (id: string) => call<void>('DELETE', `/admin/alerts/rules/${enc(id)}`);
  deleteKey = (id: string) => call<void>('DELETE', `/admin/keys/${enc(id)}`);
  deletePool = (id: string) => call<void>('DELETE', `/admin/pools/${enc(id)}`);
  disconnectAccount = (accountId: string) => call<void>('DELETE', `/admin/accounts/${enc(accountId)}/credentials`);
  getAnalyticsBreakdown = async (q: AnalyticsQuery): Promise<AnalyticsGroup[]> => (await call<{ rows: unknown[] }>('GET', `/admin/analytics/breakdown${analytics(q)}`)).rows.map(d.decodeAnalyticsGroup);
  getAnalyticsSeries = async (q: AnalyticsQuery): Promise<AnalyticsPoint[]> => (await call<{ points: unknown[] }>('GET', `/admin/analytics/series${analytics(q)}`)).points.map(d.decodeAnalyticsPoint);
  getCapturedBodies = async (requestId: string) => {
    try {
      return d.decodeBodies(await call('GET', `/admin/requests/${enc(requestId)}/bodies`));
    } catch (e) {
      if (e instanceof ApiError && e.status === 404) return null;
      throw e;
    }
  };
  getKeepaliveOverview = async () => d.decodeKeepaliveOverview(await call('GET', '/admin/keepalive/overview'));
  getOverview = async () => d.decodeDashboard(await call('GET', '/admin/dashboard'));
  getQuotaHistory = async (accountId: string) => d.decodeQuotaHistory(await call('GET', `/admin/accounts/${enc(accountId)}/quota?range=7d`));
  getRequest = async (id: string) => {
    try {
      return d.decodeRequest((await call<{ request: unknown }>('GET', `/admin/requests/${enc(id)}`)).request);
    } catch (e) {
      if (e instanceof ApiError && e.status === 404) return null;
      throw e;
    }
  };
  getSession = async (): Promise<SessionInfo | null> => {
    // Read straight through: a missing session is the normal first load, not a lost one for the hook to report.
    const response = await fetch('/admin/session', { credentials: 'same-origin' });
    if (response.status === 401) return null;
    if (!response.ok) throw new ApiError(response.status, `GET /admin/session answered ${response.status}`, null);
    const r = (await response.json()) as { role: string; session_id: string };
    return { role: r.role === 'viewer' ? 'viewer' : 'admin', sessionId: r.session_id };
  };
  getSettings = async () => d.decodeSettings(await call('GET', '/admin/settings'));
  listAccountRollups = async (from: string, to: string) => d.decodeAccountRollups(await call('GET', `/admin/analytics/accounts?range=${range(from, to)}`));
  listAccounts = async () => (await call<{ accounts: unknown[] }>('GET', '/admin/accounts')).accounts.map(d.decodeAccount);
  listAlertDeliveries = async (eventId: string) => (await call<{ deliveries: unknown[] }>('GET', `/admin/alerts/events/${enc(eventId)}/deliveries`)).deliveries.map(d.decodeDelivery);
  listAlertEvents = async (state?: 'open' | 'resolved'): Promise<AlertEvent[]> => (await call<{ events: unknown[] }>('GET', `/admin/alerts/events${qs({ state })}`)).events.map(d.decodeAlertEvent);
  listAlertRules = async () => (await call<{ rules: unknown[] }>('GET', '/admin/alerts/rules')).rules.map(d.decodeAlertRule);
  listCatalog = async () => d.decodeCatalog(await this.listAccounts());
  listKeepaliveRecipes = async () => (await call<{ recipes: unknown[] }>('GET', '/admin/keepalive/recipes')).recipes.map(d.decodeRecipe);
  listKeys = async () => (await call<{ keys: unknown[] }>('GET', '/admin/keys')).keys.map(d.decodeKey);
  listPools = async () => (await call<{ pools: unknown[] }>('GET', '/admin/pools')).pools.map(d.decodePool);
  listQuotaSnapshots = async (): Promise<QuotaSnapshot[]> => {
    const accounts = (await this.listAccounts()).filter((a) => a.kind === 'oauth');
    return Promise.all(
      accounts.map(async (a) => {
        try {
          return d.decodeSnapshot(a.id, await call('GET', `/admin/accounts/${enc(a.id)}/usage`));
        } catch (e) {
          return { accountId: a.id, error: e instanceof Error ? e.message : String(e), fetchedAt: null, status: 'error' as const };
        }
      }),
    );
  };
  listRequestEvents = async (id: string) => (await call<{ events: unknown[] }>('GET', `/admin/requests/${enc(id)}`)).events.map(d.decodeRequestEvent);
  listRequests = async (q: RequestQuery = {}) =>
    d.decodeRequestPage(
      await call('GET', `/admin/requests${qs({ accounts: q.accountIds, cursor: q.cursor, from: q.from ? Date.parse(q.from) : undefined, include_probes: q.includeProbes, keys: q.keyIds, kinds: q.kind ? [q.kind] : undefined, limit: q.limit, models: q.models, pools: q.poolIds, statuses: q.statuses?.map(String), text: q.search, to: q.to ? Date.parse(q.to) : undefined, user_agent: q.userAgent })}`),
    );
  listSessions = async () => (await call<{ sessions: unknown[] }>('GET', '/admin/sessions')).sessions.map(d.decodeSession);
  login = async (secret: string): Promise<SessionInfo> => {
    await call<void>('POST', '/admin/session', { secret });
    const me = await this.getSession();
    if (!me) throw new ApiError(401, 'the session did not take', null);
    return me;
  };
  logout = () => call<void>('DELETE', '/admin/session');
  pauseAccount = (accountId: string) => call<void>('POST', `/admin/accounts/${enc(accountId)}/pause`);
  previewPriceImport = async (url: string) => d.decodePriceImport(await call('POST', '/admin/pricing/import', { url }));
  refreshUsage = async (accountId: string) => {
    await call('POST', `/admin/accounts/${enc(accountId)}/usage/refresh`);
  };
  releaseSession = (keyId: string, sessionId: string) => call<void>('POST', `/admin/sessions/${enc(keyId)}/${enc(sessionId)}/release`);
  renameAccount = async (accountId: string, label: string) => d.decodeAccount(await call('PATCH', `/admin/accounts/${enc(accountId)}`, { label }));
  resolveAlertEvent = async (id: string) => d.decodeAlertEvent(await call('POST', `/admin/alerts/events/${enc(id)}/resolve`));
  resumeAccount = (accountId: string) => call<void>('POST', `/admin/accounts/${enc(accountId)}/resume`);
  revokeKey = (id: string) => call<void>('POST', `/admin/keys/${enc(id)}/revoke`);
  rotateKey = async (id: string): Promise<CreatedApiKey> => {
    const r = await call<Record<string, unknown>>('POST', `/admin/keys/${enc(id)}/rotate`);
    return { key: d.decodeKey(r), secret: String(r.secret) };
  };
  startPairing = async (accountId: string) => d.decodePairingSession(await call('POST', '/admin/oauth/sessions', { account_id: accountId, login: 'claude_ai' }));
  subscribeLive = (handler: (event: LiveEvent) => void): (() => void) => {
    const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
    let socket: WebSocket | null = null;
    let closed = false;
    let retry = 1_000;
    const open = () => {
      if (closed) return;
      socket = new WebSocket(`${scheme}://${location.host}/admin/live`);
      socket.onmessage = (m) => {
        const v = JSON.parse(String(m.data)) as { kind: string; snapshot?: unknown; delta?: unknown };
        if (v.kind === 'snapshot') {
          retry = 1_000;
          handler({ kind: 'snapshot', snapshot: d.decodeLiveSnapshot(v.snapshot) });
        } else {
          const delta = d.decodeLiveDelta(v.delta);
          if (delta) handler({ delta, kind: 'delta' });
        }
      };
      // A dropped socket (the feed disconnects a slow client) reconnects to a fresh snapshot with a growing wait.
      socket.onclose = () => {
        if (!closed) {
          setTimeout(() => void reopen(), retry);
          retry = Math.min(retry * 2, 30_000);
        }
      };
    };
    /**
     * A refused upgrade never passes through `call`, so an expired session would reconnect for ever behind a
     * page that never learns it has to log in again: the session is read before each retry, and only a
     * router that answers "no session" stops the feed. A probe that fails to answer at all is treated as the
     * outage it probably is, and the retry goes ahead.
     */
    const reopen = async () => {
      if (closed) return;
      const session = await this.getSession().then(
        (s) => s,
        () => 'unknown' as const,
      );
      if (closed) return;
      if (session === null) {
        hooks.onUnauthorized();
        return;
      }
      open();
    };
    open();
    return () => {
      closed = true;
      socket?.close();
    };
  };
  syncCatalog = async (accountId: string) => {
    const r = await call<Record<string, unknown>>('POST', `/admin/accounts/${enc(accountId)}/models/sync`);
    return { added: (r.added as string[]) ?? [], error: (r.error as string | null) ?? null, missing: (r.missing as string[]) ?? [], seeded: Boolean(r.seeded) };
  };
  testAlertRule = async (id: string): Promise<AlertDeliveryTest> => {
    const r = await call<Record<string, unknown>>('POST', `/admin/alerts/rules/${enc(id)}/test`);
    return { error: (r.error as string | null) ?? null, status: typeof r.status === 'number' ? r.status : null };
  };
  updateAlertRule = async (id: string, input: AlertRuleInput) => d.decodeAlertRule(await call('PATCH', `/admin/alerts/rules/${enc(id)}`, d.encodeRule(input)));
  updateKey = async (id: string, input: KeyInput): Promise<ApiKey> => d.decodeKey(await call('PATCH', `/admin/keys/${enc(id)}`, d.encodeKey(input)));
  updatePool = async (id: string, input: PoolInput) => {
    const { members, ...rest } = d.encodePool(input);
    await call('PATCH', `/admin/pools/${enc(id)}`, rest);
    return d.decodePool(await call('PUT', `/admin/pools/${enc(id)}/members`, { members }));
  };
  updateServedModels = async (accountId: string, models: ServedModel[]) => {
    await call('PUT', `/admin/accounts/${enc(accountId)}/models`, { models: models.map((m) => ({ long_context: m.longContext, selected: m.selected, upstream_id: m.upstreamId })) });
    return d.decodeAccount(await call('GET', `/admin/accounts/${enc(accountId)}`));
  };
  verifyAccount = (accountId: string) => call<{ ok: boolean; status: number }>('POST', `/admin/accounts/${enc(accountId)}/verify`);
}

const enc = encodeURIComponent;
/** The API wants a slug id; the dialogs ask for a name, so the id is derived the way the mock derived pool ids. */
export const slug = (name: string) =>
  name
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .slice(0, 64)
    .replace(/^-+|-+$/g, '') || 'item';
