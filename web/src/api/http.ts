import type { AccountPatch, Api, CreateKeyInput, KeyInput, PoolInput } from './client';
import * as d from './decode';
import type { Account, AccountCatalog, AccountInput, AnalyticsGroup, AnalyticsPoint, AnalyticsQuery, ApiKey, BurnGroup, CatalogModel, CreatedApiKey, PairingInput, PairingPreview, PairingStart, RequestDetail, RequestQuery, RoutingLogQuery, SessionInfo } from './types';

export class ApiError extends Error {
  code: string | null;
  /** The route the failed call read, so a read of it that later succeeds can clear the failure notice it raised. */
  path: string | null;
  status: number;
  constructor(status: number, message: string, code: string | null, path: string | null = null) {
    super(message);
    this.code = code;
    this.path = path;
    this.status = status;
  }
}

/** The router took the secret, yet the session read that follows finds no session: the browser dropped the cookie. */
export class SessionNotKept extends Error {
  constructor() {
    super('the router accepted the secret but the browser kept no session cookie');
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
  onRecovered: (path: string) => void;
  onUnauthorized: () => void;
}
let hooks: Hooks = { onFailure: () => undefined, onForbidden: () => undefined, onRecovered: () => undefined, onUnauthorized: () => undefined };
/**
 * The session provider installs these once; a 401 anywhere returns the app to the login, a 403 names the
 * refusal, and everything else reaches the shell's toast from here rather than from each page.
 */
export const setHooks = (next: Hooks) => {
  hooks = next;
};

let quiet = false;

/**
 * Runs `fn` with the failure reports of the calls it starts dropped, so a refresh that continues a failing streak does
 * not toast the failure the streak's first read already reported. The window closes before `fn`'s promise settles,
 * so only the calls started inside it are quiet.
 */
export function quietly<T>(fn: () => T): T {
  const was = quiet;
  quiet = true;
  try {
    return fn();
  } finally {
    quiet = was;
  }
}

/** The one place a failed call is reported from, so the mock build can stand in for the transport too. */
export const reportFailure = (failure: Failure) => {
  if (!quiet) hooks.onFailure(failure);
};

/** A read of `path` answered after its failing streak, so the failure notice the streak raised is out of date. */
export const reportRecovery = (path: string) => {
  hooks.onRecovered(path);
};

/**
 * `quiet` keeps a failure from the shell's toast for a read that draws its own failed state and repeats on a schedule,
 * which would otherwise toast once per tick; a 401 still returns the console to the login.
 */
export async function call<T = unknown>(method: string, path: string, body?: unknown, raw = false, { quiet: asked = false }: { quiet?: boolean } = {}): Promise<T> {
  // Read before the first await: the quiet window is synchronous, and the report comes after the answer.
  const silent = asked || quiet;
  // The login screen draws every refusal of its own route, and the shell that would toast one mounts only after
  // the next login succeeds, where it would report a failure the operator has already read.
  const login = method === 'POST' && path === '/admin/session';
  let response: Response;
  try {
    response = await fetch(path, { body: body === undefined ? undefined : JSON.stringify(body), credentials: 'same-origin', headers: body === undefined ? {} : { 'content-type': 'application/json' }, method });
  } catch (e) {
    // A router that never answered: the page below draws its error state, and the hook says which route.
    const message = e instanceof Error ? e.message : String(e);
    if (!login && !silent) reportFailure({ message, path, status: 0 });
    throw new ApiError(0, message, null, path);
  }
  if (response.status === 204) return undefined as T;
  const text = await response.text();
  let parsed: unknown;
  try {
    parsed = text ? JSON.parse(text) : null;
  } catch {
    parsed = null;
  }
  if (!response.ok) {
    const answered = parsed && typeof parsed === 'object' && typeof (parsed as { error?: unknown }).error === 'string' ? (parsed as { error: string }).error : null;
    const message = answered ?? `${method} ${path} answered ${response.status}`;
    if (!login) {
      if (response.status === 401) hooks.onUnauthorized();
      else if (response.status === 403 && !silent) hooks.onForbidden(message);
      // A 404 is how the routes answer for a request or a body the router never kept, which the page reading
      // it draws as a state of its own; everything else is a failure the operator has to be told about.
      else if (response.status !== 404 && !silent) reportFailure({ message: answered, path, status: response.status });
    }
    throw new ApiError(response.status, message, response.headers.get('x-phoebis-error-code'), path);
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
// `qs` joins a list with commas, which a pattern may hold; every pattern list travels joined by newline instead.
const analytics = (q: AnalyticsQuery) => qs({ accounts: q.accountIds, clients: q.clients?.join('\n'), funding: q.funding, group_by: q.groupBy, include_probes: q.includeProbes, keys: q.keyIds, kinds: q.kinds, models: q.models, pools: q.poolIds, range: range(q.from, q.to), user_agent_groups: q.userAgentGroups?.join('\n'), user_agents: q.userAgents?.join('\n') });

/** Each model once, the first catalog naming an id answering for it, in id order; a fetch that failed adds nothing. */
export const catalogUnion = (catalogs: (AccountCatalog | null)[]): CatalogModel[] => {
  const out = new Map<string, CatalogModel>();
  for (const c of catalogs) for (const m of c?.models ?? []) if (!out.has(m.id)) out.set(m.id, m);
  return [...out.values()].sort((x, y) => x.id.localeCompare(y.id));
};

// The fence is a number the decoder read, so there is nothing to parse and nothing to refuse.
const withRevision = (body: Record<string, unknown>, revision?: number) =>
  revision === undefined ? body : { ...body, expected_revision: revision };

// The writes of the pool, key and account dialogs, the pairing dialog's start and the account's two inline editors:
// each shows a refusal in its own callout or footer, so the shell's toast would only repeat it.
const DIALOG_WRITE = { quiet: true };

export class HttpApi implements Api {
  analyticsBudgets = async () => (await call<{ members: unknown[] }>('GET', '/admin/analytics/budgets')).members.map(d.decodeMemberBudget);
  analyticsPlacements = async (q: Pick<AnalyticsQuery, 'from' | 'keyIds' | 'poolIds' | 'to'>) => (await call<{ rows: unknown[] }>('GET', `/admin/analytics/placements${qs({ keys: q.keyIds, pools: q.poolIds, range: range(q.from, q.to) })}`)).rows.map(d.decodePlacementRow);
  cancelPairing = (pairingId: string) => call<void>('DELETE', `/admin/oauth/sessions/${enc(pairingId)}`);
  clearCooldown = (accountId: string, scope: string) => call<void>('DELETE', `/admin/accounts/${enc(accountId)}/cooldowns/${enc(scope)}`);
  clearFunding = (accountId: string) => call<void>('POST', `/admin/accounts/${enc(accountId)}/clear-funding`);
  completePairing = async (pairingId: string, input: PairingInput): Promise<PairingPreview> => {
    const r = await call<{ pending: { email: string; organization_name: string | null } }>('POST', `/admin/oauth/sessions/${enc(pairingId)}/callback`, input);
    return { email: r.pending.email, organization: r.pending.organization_name ?? 'Personal', state: pairingId };
  };
  confirmPairing = (pairingId: string) => call<void>('POST', `/admin/oauth/sessions/${enc(pairingId)}/confirm`);
  createAccount = async (input: AccountInput): Promise<Account> => d.decodeAccount(await call('POST', '/admin/accounts', { id: input.id, kind: input.kind, label: input.label, secret: input.secret }, false, DIALOG_WRITE));
  createKey = async (input: CreateKeyInput): Promise<CreatedApiKey> => {
    const expires_at = input.expiresInDays === null ? null : Date.now() + input.expiresInDays * 86_400_000;
    const r = await call<Record<string, unknown>>('POST', '/admin/keys', { ...d.encodeKey(input), expires_at, id: slug(input.name) }, false, DIALOG_WRITE);
    return { key: d.decodeKey(r), secret: String(r.secret) };
  };
  createPool = async (input: PoolInput) => d.decodePool(await call('POST', '/admin/pools', { ...d.encodePool(input), id: slug(input.name) }, false, DIALOG_WRITE));
  deleteAccount = (accountId: string) => call<void>('DELETE', `/admin/accounts/${enc(accountId)}`);
  deleteKey = (id: string) => call<void>('DELETE', `/admin/keys/${enc(id)}`);
  deletePool = (id: string) => call<void>('DELETE', `/admin/pools/${enc(id)}`);
  disconnectAccount = (accountId: string) => call<void>('DELETE', `/admin/accounts/${enc(accountId)}/credentials`);
  getAccountCatalog = async (accountId: string, refresh?: boolean) => d.decodeAccountCatalog(await call('GET', `/admin/accounts/${enc(accountId)}/catalog${qs({ refresh: refresh ? 1 : undefined })}`));
  getAnalyticsBreakdown = async (q: AnalyticsQuery): Promise<AnalyticsGroup[]> => (await call<{ rows: unknown[] }>('GET', `/admin/analytics/breakdown${analytics(q)}`)).rows.map(d.decodeAnalyticsGroup);
  getAnalyticsBurn = async (r: Pick<AnalyticsQuery, 'from' | 'to'>, groupBy: BurnGroup, filters: Pick<AnalyticsQuery, 'accountIds' | 'userAgentGroups'> = {}) =>
    d.decodeBurn(await call('GET', `/admin/analytics/burn${qs({ accounts: filters.accountIds, group_by: groupBy, range: range(r.from, r.to), user_agent_groups: filters.userAgentGroups?.join('\n') })}`));
  getAnalyticsRewrites = async (r: Pick<AnalyticsQuery, 'from' | 'to'>) => d.decodeRewrites(await call('GET', `/admin/analytics/rewrites${qs({ range: range(r.from, r.to) })}`));
  getAnalyticsSeries = async (q: AnalyticsQuery): Promise<AnalyticsPoint[]> => (await call<{ points: unknown[] }>('GET', `/admin/analytics/series${analytics(q)}`)).points.map(d.decodeAnalyticsPoint);
  getAttention = async () => (await call<{ items: unknown[] }>('GET', '/admin/attention', undefined, false, { quiet: true })).items.map(d.decodeAttention);
  getCapturedBodies = async (requestId: string) => {
    try {
      return d.decodeBodies(await call('GET', `/admin/requests/${enc(requestId)}/bodies`));
    } catch (e) {
      if (e instanceof ApiError && e.status === 404) return null;
      throw e;
    }
  };
  getDecision = async (requestId: string, seq: number) => {
    try {
      return d.decodeDecisionRecord(await call('GET', `/admin/requests/${enc(requestId)}/decisions/${seq}`));
    } catch (e) {
      if (e instanceof ApiError && e.status === 404) return null;
      throw e;
    }
  };
  getKeepaliveOverview = async () => d.decodeKeepaliveOverview(await call('GET', '/admin/keepalive/overview'));
  getKeyModels = async (keyId: string) => d.decodeKeyModels(await call('GET', `/admin/keys/${enc(keyId)}/models`));
  getModelsState = async () => d.decodeModelsState(await call('GET', '/admin/models/state'));
  getOverview = async () => d.decodeDashboard(await call('GET', '/admin/dashboard'));
  getQuotaHistory = async (accountId: string) => d.decodeQuotaHistory(await call('GET', `/admin/accounts/${enc(accountId)}/quota?range=7d`));
  getRequest = async (id: string): Promise<RequestDetail | null> => {
    try {
      const body = await call<{ events: unknown[]; request: unknown }>('GET', `/admin/requests/${enc(id)}`);
      return { events: body.events.map(d.decodeRequestEvent), request: d.decodeRequest(body.request) };
    } catch (e) {
      if (e instanceof ApiError && e.status === 404) return null;
      throw e;
    }
  };
  getSession = async (): Promise<SessionInfo | null> => {
    // Read straight through: a missing session is the normal first load, not a lost one for the hook to report.
    let response: Response;
    try {
      response = await fetch('/admin/session', { credentials: 'same-origin' });
    } catch (e) {
      // Raised as `call` raises it, so the login screen can tell a router that never answered from "no session".
      throw new ApiError(0, e instanceof Error ? e.message : String(e), null);
    }
    if (response.status === 401) return null;
    if (!response.ok) throw new ApiError(response.status, `GET /admin/session answered ${response.status}`, null);
    const r = (await response.json()) as { session_id: string };
    return { sessionId: r.session_id };
  };
  getSettings = async () => d.decodeSettings(await call('GET', '/admin/settings'));
  listAccountRollups = async (from: string, to: string) => d.decodeAccountRollups(await call('GET', `/admin/analytics/accounts?range=${range(from, to)}`));
  listAccounts = async () => (await call<{ accounts: unknown[] }>('GET', '/admin/accounts')).accounts.map(d.decodeAccount);
  listCatalog = async (): Promise<CatalogModel[]> => {
    // The decoded status folds a disconnected account into 'unpaired' along with one never paired; both are
    // the accounts a catalog fetch has nothing to ask.
    const accounts = (await this.listAccounts()).filter((a) => a.status !== 'unpaired');
    return this.listCatalogOf(accounts.map((a) => a.id));
  };
  listCatalogOf = async (accountIds: string[]): Promise<CatalogModel[]> => catalogUnion(await Promise.all(accountIds.map((id) => this.getAccountCatalog(id).catch(() => null))));
  listKeepaliveRecipes = async () => (await call<{ recipes: unknown[] }>('GET', '/admin/keepalive/recipes')).recipes.map(d.decodeRecipe);
  listKeys = async () => (await call<{ keys: unknown[] }>('GET', '/admin/keys')).keys.map(d.decodeKey);
  listPools = async () => (await call<{ pools: unknown[] }>('GET', '/admin/pools')).pools.map(d.decodePool);
  listRequests = async (q: RequestQuery = {}) =>
    d.decodeRequestPage(
      await call('GET', `/admin/requests${qs({ accounts: q.accountIds, agent_types: q.agentTypes, cache_miss_reasons: q.cacheMissReasons, client: q.client, cursor: q.cursor, efforts: q.efforts, from: q.from ? Date.parse(q.from) : undefined, held: q.held, include_probes: q.includeProbes, keys: q.keyIds, kinds: q.kind ? [q.kind] : undefined, limit: q.limit, models: q.models, outcomes: q.outcomes, pools: q.poolIds, request: q.request, request_classes: q.requestClasses, session: q.session, speeds: q.speeds, statuses: q.statuses?.map(String), text: q.search, to: q.to ? Date.parse(q.to) : undefined, turn_origins: q.turnOrigins, user_agent: q.userAgent })}`),
    );
  // The route reads RFC 3339 instants, so the two times pass through as the page wrote them.
  listRoutingLog = async (q: RoutingLogQuery = {}) =>
    d.decodeRoutingLogPage(await call('GET', `/admin/routing-log${qs({ accounts: q.accountIds, cursor: q.cursor, from: q.from, keys: q.keyIds, kinds: q.kinds, limit: q.limit, models: q.models, pools: q.poolIds, request: q.request, session: q.session, to: q.to })}`));
  listSessions = async () => (await call<{ sessions: unknown[] }>('GET', '/admin/sessions')).sessions.map(d.decodeSession);
  login = async (secret: string): Promise<SessionInfo> => {
    await call<void>('POST', '/admin/session', { secret });
    const me = await this.getSession();
    if (!me) throw new SessionNotKept();
    return me;
  };
  logout = () => call<void>('DELETE', '/admin/session');
  pauseAccount = (accountId: string) => call<void>('POST', `/admin/accounts/${enc(accountId)}/pause`);
  refreshUsage = async (accountId: string) => {
    await call('POST', `/admin/accounts/${enc(accountId)}/usage/refresh`);
  };
  releaseSession = (keyId: string, sessionId: string) => call<void>('POST', `/admin/sessions/${enc(keyId)}/${enc(sessionId)}/release`);
  renameAccount = async (accountId: string, label: string, revision?: number) =>
    d.decodeAccount(await call('PATCH', `/admin/accounts/${enc(accountId)}`, withRevision({ label }, revision), false, DIALOG_WRITE));
  // Quiet: the dashboard's line draws a refusal, the held writer's `503` among them, in its own slot.
  replayParked = async () => d.decodeReplay(await call('POST', '/admin/ledger/parked/replay', undefined, false, { quiet: true }));
  resumeAccount = (accountId: string) => call<void>('POST', `/admin/accounts/${enc(accountId)}/resume`);
  revokeKey = (id: string) => call<void>('POST', `/admin/keys/${enc(id)}/revoke`);
  rotateKey = async (id: string): Promise<CreatedApiKey> => {
    const r = await call<Record<string, unknown>>('POST', `/admin/keys/${enc(id)}/rotate`);
    return { key: d.decodeKey(r), secret: String(r.secret) };
  };
  startPairing = async (start: PairingStart) =>
    d.decodePairingSession(await call('POST', '/admin/oauth/sessions', 'accountId' in start ? { account_id: start.accountId, login: 'claude_ai' } : { login: 'claude_ai', new: { id: start.id, label: start.label } }, false, DIALOG_WRITE));
  updateAccount = async (accountId: string, patch: AccountPatch, revision?: number) =>
    d.decodeAccount(
      await call('PATCH', `/admin/accounts/${enc(accountId)}`, withRevision(d.encodeAccountPatch(patch), revision), false, DIALOG_WRITE),
    );
  updateKey = async (id: string, input: KeyInput, revision?: number): Promise<ApiKey> => d.decodeKey(await call('PATCH', `/admin/keys/${enc(id)}`, withRevision(d.encodeKey(input), revision), false, DIALOG_WRITE));
  // The route writes the settings and the members in one transaction, so a refusal stores nothing; a 409
  // `stale_edit` means the row moved since the dialog read it, which the dialog answers with Reload.
  updatePool = async (id: string, input: PoolInput, revision?: number) => d.decodePool(await call('PATCH', `/admin/pools/${enc(id)}`, withRevision(d.encodePool(input), revision), false, DIALOG_WRITE));
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
