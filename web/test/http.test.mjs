import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import { ApiError, HttpApi, call, setHooks, slug } from './dist/api.js';

const answer = (status, body, headers = {}) => ({ headers: { get: (k) => headers[k.toLowerCase()] ?? null }, ok: status >= 200 && status < 300, status, text: async () => (body === undefined ? '' : JSON.stringify(body)), json: async () => body });
const stub = (handler) => { const calls = []; globalThis.fetch = async (url, init = {}) => { calls.push({ init, url }); return handler(url, init); }; return calls; };

test('call sends same-origin credentials and JSON, and answers the parsed body', async () => {
  const calls = stub(() => answer(200, { keys: [] }));
  const body = await call('POST', '/admin/keys', { id: 'k' });
  assert.deepEqual(body, { keys: [] });
  assert.equal(calls[0].init.credentials, 'same-origin');
  assert.equal(calls[0].init.headers['content-type'], 'application/json');
  assert.equal(calls[0].init.body, '{"id":"k"}');
  stub(() => answer(204));
  assert.equal(await call('DELETE', '/admin/keys/k'), undefined, 'a 204 answers undefined');
});

test('a refusal becomes an ApiError with the message and the code header', async () => {
  stub(() => answer(409, { error: 'a key with this id exists' }, { 'x-phoebis-error-code': 'registry_unavailable' }));
  await assert.rejects(call('POST', '/admin/keys', {}), (e) => e instanceof ApiError && e.status === 409 && e.message === 'a key with this id exists' && e.code === 'registry_unavailable');
});

test('a 401 outside the login runs the unauthorized hook and the login 401 does not', async () => {
  let unauthorized = 0;
  let forbidden = 0;
  setHooks({ onForbidden: () => { forbidden += 1; }, onUnauthorized: () => { unauthorized += 1; } });
  stub(() => answer(401, { error: 'no admin session' }));
  await assert.rejects(call('GET', '/admin/keys'));
  await assert.rejects(call('POST', '/admin/session', { secret: 'x' }));
  assert.equal(unauthorized, 1);
  stub(() => answer(403, { error: 'the viewer role cannot change anything' }));
  await assert.rejects(new HttpApi().pauseAccount('a'));
  assert.equal(forbidden, 1);
});

test('the HttpApi encodes ids into paths and lists into comma-separated query values', async () => {
  const calls = stub((url) => (url.startsWith('/admin/requests') ? answer(200, { next_cursor: null, requests: [] }) : answer(204)));
  const api = new HttpApi();
  await api.listRequests({ keyIds: ['a', 'b'], search: 'x y', statuses: [200, 429], limit: 50 });
  const url = new URL(calls[0].url, 'http://x');
  assert.equal(url.searchParams.get('keys'), 'a,b');
  assert.equal(url.searchParams.get('statuses'), '200,429');
  assert.equal(url.searchParams.get('text'), 'x y');
  assert.equal(url.searchParams.get('limit'), '50');
  await api.clearCooldown('acc', 'model:claude-sonnet-5');
  assert.equal(calls[1].url, '/admin/accounts/acc/cooldowns/model%3Aclaude-sonnet-5');
  assert.equal(slug('My Pool!'), 'my-pool');
  assert.equal(slug(`${'a'.repeat(63)} tail`), 'a'.repeat(63), 'a name cut at the limit keeps no trailing separator');
});

test('createAccount sends the id, kind, label, and secret in snake_case', async () => {
  const body = JSON.parse(readFileSync(new URL('./fixtures/account.json', import.meta.url), 'utf8')).body;
  const calls = stub(() => answer(200, body));
  await new HttpApi().createAccount({ id: 'console', kind: 'api_key', label: 'Console', secret: 'sk-ant-api03-FAKE' });
  assert.deepEqual([calls[0].init.method, calls[0].url, JSON.parse(calls[0].init.body)], ['POST', '/admin/accounts', { id: 'console', kind: 'api_key', label: 'Console', secret: 'sk-ant-api03-FAKE' }]);
});

test('listRequests passes the client filter as user_agent', async () => {
  const calls = stub(() => answer(200, { next_cursor: null, requests: [] }));
  await new HttpApi().listRequests({ userAgent: 'claude-cli/2.1.*' });
  assert.ok(String(calls[0].url).includes('user_agent=claude-cli%2F2.1.*'), String(calls[0].url));
});

test('the analytics reads encode the range, the dimension, and every id list', async () => {
  const calls = stub((url) => answer(200, url.startsWith('/admin/analytics/series') ? { points: [] } : { rows: [] }));
  const api = new HttpApi();
  const q = { accountIds: ['business', 'direct'], from: '1970-01-01T00:00:00.000Z', groupBy: 'key', includeProbes: true, keyIds: ['desktop'], models: ['claude-sonnet-5'], poolIds: ['all'], to: '1970-01-02T00:00:00.000Z', userAgentGroups: ['a', 'b'] };
  assert.deepEqual(await api.getAnalyticsSeries(q), []);
  const series = new URL(calls[0].url, 'http://x');
  assert.equal(series.pathname, '/admin/analytics/series');
  assert.equal(series.searchParams.get('range'), '0,86400000');
  assert.equal(series.searchParams.get('group_by'), 'key');
  assert.equal(series.searchParams.get('include_probes'), 'true');
  assert.equal(series.searchParams.get('accounts'), 'business,direct');
  assert.equal(series.searchParams.get('keys'), 'desktop');
  assert.equal(series.searchParams.get('models'), 'claude-sonnet-5');
  assert.equal(series.searchParams.get('pools'), 'all');
  // One pattern per line, so a pattern holding a comma still names one group.
  assert.ok(String(calls[0].url).includes('user_agent_groups=a%0Ab'), String(calls[0].url));
  assert.deepEqual(await api.getAnalyticsBreakdown({ ...q, accountIds: [], groupBy: 'all', includeProbes: undefined, keyIds: [], models: [], poolIds: [], userAgentGroups: undefined }), []);
  const breakdown = new URL(calls[1].url, 'http://x');
  assert.equal(breakdown.pathname, '/admin/analytics/breakdown');
  assert.equal(breakdown.searchParams.get('group_by'), 'all');
  assert.deepEqual([...breakdown.searchParams.keys()], ['group_by', 'range'], 'an empty list and an unset flag are left off the query');
});

test('getSession answers null on 401 without running the hook', async () => {
  let unauthorized = 0;
  setHooks({ onForbidden: () => undefined, onUnauthorized: () => { unauthorized += 1; } });
  stub(() => answer(401, { error: 'no admin session' }));
  assert.equal(await new HttpApi().getSession(), null);
  assert.equal(unauthorized, 0);
  stub(() => answer(200, { role: 'viewer', session_id: 's' }));
  assert.deepEqual(await new HttpApi().getSession(), { role: 'viewer', sessionId: 's' });
});

test('the live feed reconnects behind a live session and stops once it is gone', async () => {
  const sockets = [];
  globalThis.WebSocket = class {
    constructor(url) {
      this.url = url;
      sockets.push(this);
    }
    close() {
      this.closed = true;
    }
  };
  globalThis.location = { host: 'router.test', protocol: 'http:' };
  let unauthorized = 0;
  setHooks({ onFailure: () => undefined, onForbidden: () => undefined, onUnauthorized: () => { unauthorized += 1; } });
  const settle = () => new Promise((r) => setTimeout(r, 1_200));

  stub(() => answer(200, { role: 'admin', session_id: 's' }));
  const live = new HttpApi().subscribeLive(() => undefined);
  assert.equal(sockets[0].url, 'ws://router.test/admin/live');
  sockets[0].onclose();
  await settle();
  assert.equal(sockets.length, 2, 'a dropped socket reopens while the session holds');
  assert.equal(unauthorized, 0);
  live();

  stub(() => answer(401, { error: 'no admin session' }));
  const expired = new HttpApi().subscribeLive(() => undefined);
  const opened = sockets.length;
  sockets[opened - 1].onclose();
  await settle();
  assert.equal(sockets.length, opened, 'an upgrade refused behind an expired session is not retried for ever');
  assert.equal(unauthorized, 1, 'the page reaches the login screen the way every other read does');
  expired();
});

test('a pool save is one PATCH that carries the members with the settings', async () => {
  const row = JSON.parse(readFileSync(new URL('./fixtures/pools.json', import.meta.url), 'utf8')).body.pools[0];
  const calls = stub(() => answer(200, row));
  await new HttpApi().updatePool('main', {
    affinityTtlSeconds: 600,
    apiKeySubStrategy: 'priority_random',
    bodyCaptureHours: 0,
    cacheDiagnostics: null,
    cacheTtl: { enabled: false, inOverage: null, main: '1h', models: [], subagent: '5m' },
    defaultAllowedModels: null,
    keepalive: { coldProbeLimit: 2, costCapUsdPerDay: null, enabled: false, jitterSeconds1h: 30, jitterSeconds5m: 5, leadSeconds1h: 360, leadSeconds5m: 60, maxIdleSeconds: 3_600, maxIdleTicks: 0, netNegativeGraceProbes: 2, retryMaxSeconds: 300, retrySeconds: 30, stopWhenNetNegative: true, warmSubagents: true },
    members: [{ accountId: 'business', priority: 0 }, { accountId: 'direct', priority: 1 }],
    migrateAboveUtilization: null,
    name: row.label,
    overagePolicy: 'prefer_plan',
    preferSubscription: false,
    selectBelowUtilization: 0.4,
    status: 'active',
    strategy: 'least_utilized',
  });
  assert.deepEqual(calls.map((c) => [c.init.method, c.url]), [['PATCH', '/admin/pools/main']], 'a second write could fail after the first landed');
  const body = JSON.parse(calls[0].init.body);
  assert.deepEqual(body.members, [{ account_id: 'business', priority: 0 }, { account_id: 'direct', priority: 1 }]);
  assert.equal(body.select_below_utilization, 0.4);
});

test('deleting an upstream calls the account route, and a disconnect keeps to the credentials route', async () => {
  const calls = stub(() => answer(204));
  const api = new HttpApi();
  assert.equal(await api.deleteAccount('work seat'), undefined);
  await api.disconnectAccount('work seat');
  assert.deepEqual(calls.map((c) => [c.init.method, c.url]), [['DELETE', '/admin/accounts/work%20seat'], ['DELETE', '/admin/accounts/work%20seat/credentials']]);
});
