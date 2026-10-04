import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import { ApiError, HttpApi, SessionNotKept, call, quietly, setHooks, slug } from './dist/api.js';

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
  stub(() => answer(403, { error: 'origin is not allowed' }));
  await assert.rejects(new HttpApi().pauseAccount('a'));
  assert.equal(forbidden, 1);
});

test("the login route's refusals reach the login screen alone, never the shell's toast", async () => {
  let failures = 0;
  let forbidden = 0;
  let unauthorized = 0;
  setHooks({ onFailure: () => { failures += 1; }, onForbidden: () => { forbidden += 1; }, onUnauthorized: () => { unauthorized += 1; } });
  for (const status of [400, 401, 403, 429, 500]) {
    stub(() => answer(status, { error: 'refused' }));
    await assert.rejects(call('POST', '/admin/session', { secret: 'x' }), (e) => e instanceof ApiError && e.status === status);
  }
  stub(() => Promise.reject(new TypeError('fetch failed')));
  await assert.rejects(call('POST', '/admin/session', { secret: 'x' }), (e) => e instanceof ApiError && e.status === 0);
  assert.deepEqual([failures, forbidden, unauthorized], [0, 0, 0]);
  stub(() => answer(429, { error: 'too many failed logins, try again in a minute' }));
  await assert.rejects(call('GET', '/admin/keys'));
  assert.equal(failures, 1, 'the same refusal on any other route still reaches the toast');
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

test('the accounts list carries the poll standing and no usage route is read', async () => {
  const body = JSON.parse(readFileSync(new URL('./fixtures/accounts.json', import.meta.url), 'utf8')).body;
  const calls = stub(() => answer(200, body));
  const accounts = await new HttpApi().listAccounts();
  assert.deepEqual(calls.map((c) => [c.init.method ?? 'GET', c.url]), [['GET', '/admin/accounts']], 'one read of the list, no usage read per account');
  assert.deepEqual(accounts.map((a) => [a.id, a.usage?.poll?.lastOutcome ?? a.usage]), [['business', 'clean'], ['direct', null]]);
});

test('updateAccount is one PATCH that carries the cancelled flag and the end in milliseconds', async () => {
  const body = JSON.parse(readFileSync(new URL('./fixtures/account.json', import.meta.url), 'utf8')).body;
  const calls = stub(() => answer(200, { ...body, cancelled: true }));
  const end = '2026-10-19T09:15:00.000Z';
  const account = await new HttpApi().updateAccount('business', { cancelled: true, subscriptionEndsAt: end });
  assert.deepEqual([calls[0].init.method, calls[0].url, JSON.parse(calls[0].init.body)], ['PATCH', '/admin/accounts/business', { cancelled: true, subscription_ends_at: Date.parse(end) }]);
  assert.equal(account.cancelled, true, 'the answer decodes as the account');
});

test('listRequests passes the user agent pattern as user_agent', async () => {
  const calls = stub(() => answer(200, { next_cursor: null, requests: [] }));
  await new HttpApi().listRequests({ userAgent: 'claude-cli/2.1.*' });
  assert.ok(String(calls[0].url).includes('user_agent=claude-cli%2F2.1.*'), String(calls[0].url));
});

test('listRequests passes the client pattern, the agent types, and the turn origins', async () => {
  const calls = stub(() => answer(200, { next_cursor: null, requests: [] }));
  await new HttpApi().listRequests({ agentTypes: ['Explore', 'Plan'], client: 'claude-code/*', turnOrigins: ['human'] });
  const url = String(calls[0].url);
  for (const part of ['client=claude-code%2F*', 'agent_types=Explore%2CPlan', 'turn_origins=human']) assert.ok(url.includes(part), `${part} in ${url}`);
});

test('listRequests asks for the held requests alone when the query says so', async () => {
  const calls = stub(() => answer(200, { next_cursor: null, requests: [] }));
  await new HttpApi().listRequests({ held: true });
  assert.equal(new URL(calls[0].url, 'http://x').searchParams.get('held'), 'true');
  await new HttpApi().listRequests({});
  assert.ok(!String(calls[1].url).includes('held'), 'no held filter, no parameter');
});

test('listRequests passes the speeds as one comma list', async () => {
  const calls = stub(() => answer(200, { next_cursor: null, requests: [] }));
  await new HttpApi().listRequests({ speeds: ['fast', 'standard'] });
  assert.equal(new URL(calls[0].url, 'http://x').searchParams.get('speeds'), 'fast,standard');
  await new HttpApi().listRequests({});
  assert.ok(!String(calls[1].url).includes('speeds'), 'no speed filter, no parameter');
});

test('the request list encodes the outcome list', async () => {
  const calls = stub(() => answer(200, { next_cursor: null, requests: [] }));
  await new HttpApi().listRequests({ outcomes: ['canceled', 'failed'] });
  assert.equal(new URL(calls[0].url, 'http://x').searchParams.get('outcomes'), 'canceled,failed');
  await new HttpApi().listRequests({});
  assert.ok(!String(calls[1].url).includes('outcomes'), 'no outcome filter, no parameter');
});

test('the analytics reads encode the range, the dimension, and every id list', async () => {
  const calls = stub((url) => answer(200, url.startsWith('/admin/analytics/series') ? { points: [] } : { rows: [] }));
  const api = new HttpApi();
  const q = { accountIds: ['business', 'direct'], clients: ['a', 'b'], from: '1970-01-01T00:00:00.000Z', groupBy: 'key', includeProbes: true, keyIds: ['desktop'], models: ['claude-sonnet-5'], poolIds: ['all'], to: '1970-01-02T00:00:00.000Z', userAgentGroups: ['a', 'b'] };
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
  assert.ok(String(calls[0].url).includes('clients=a%0Ab'), String(calls[0].url));
  assert.deepEqual(await api.getAnalyticsBreakdown({ ...q, accountIds: [], clients: undefined, groupBy: 'all', includeProbes: undefined, keyIds: [], models: [], poolIds: [], userAgentGroups: undefined }), []);
  const breakdown = new URL(calls[1].url, 'http://x');
  assert.equal(breakdown.pathname, '/admin/analytics/breakdown');
  assert.equal(breakdown.searchParams.get('group_by'), 'all');
  assert.deepEqual([...breakdown.searchParams.keys()], ['group_by', 'range'], 'an empty list and an unset flag are left off the query');
});

test('the burn and rewrite reads send the range and the grouping and answer what the routes decode to', async () => {
  const body = (name) => JSON.parse(readFileSync(new URL(`./fixtures/${name}.json`, import.meta.url), 'utf8')).body;
  const bodies = { '/admin/analytics/burn': body('analytics_burn'), '/admin/analytics/rewrites': body('analytics_rewrites') };
  const calls = stub((url) => answer(200, bodies[new URL(url, 'http://x').pathname]));
  const api = new HttpApi();
  const range = { from: '1970-01-01T00:00:00.000Z', to: '1970-01-02T00:00:00.000Z' };
  const burn = await api.getAnalyticsBurn(range, 'request_class');
  const rewrites = await api.getAnalyticsRewrites(range);
  const [burnUrl, rewritesUrl] = calls.map((c) => new URL(c.url, 'http://x'));
  assert.deepEqual([burnUrl.pathname, burnUrl.searchParams.get('range'), burnUrl.searchParams.get('group_by')], ['/admin/analytics/burn', '0,86400000', 'request_class']);
  assert.deepEqual([rewritesUrl.pathname, [...rewritesUrl.searchParams.keys()], rewritesUrl.searchParams.get('range')], ['/admin/analytics/rewrites', ['range'], '0,86400000']);
  // A subscription point that burned, whose 5-hour share a misnamed field would read as null.
  const planned = bodies['/admin/analytics/burn'].points.findLastIndex((p) => p.five_hour_share > 0);
  assert.ok(planned >= 0, 'the fixture reports a subscription point that burned');
  assert.deepEqual(
    [burn.bucketSeconds, burn.points.length, burn.points[planned].fiveHourShare, burn.points[planned].sevenDayShare],
    [3_600, bodies['/admin/analytics/burn'].points.length, bodies['/admin/analytics/burn'].points[planned].five_hour_share, null],
    'every 7-day cell is unmeasured',
  );
  assert.deepEqual([rewrites.moves, rewrites.points.length], [{ spent: 1 }, bodies['/admin/analytics/rewrites'].points.length]);
});

test('the placements read sends the range and the key and pool lists, the budgets read no query, and both decode', async () => {
  const body = (name) => JSON.parse(readFileSync(new URL(`./fixtures/${name}.json`, import.meta.url), 'utf8')).body;
  const bodies = { '/admin/analytics/budgets': body('analytics_budgets'), '/admin/analytics/placements': body('analytics_placements') };
  const calls = stub((url) => answer(200, bodies[new URL(url, 'http://x').pathname]));
  const api = new HttpApi();
  const range = { from: '1970-01-01T00:00:00.000Z', to: '1970-01-02T00:00:00.000Z' };
  const placements = await api.analyticsPlacements({ ...range, keyIds: ['desktop', 'ci'], poolIds: ['all'] });
  await api.analyticsPlacements({ ...range, keyIds: [], poolIds: [] });
  const budgets = await api.analyticsBudgets();
  const [filtered, bare, budgetsUrl] = calls.map((c) => new URL(c.url, 'http://x'));
  assert.deepEqual(
    [filtered.pathname, filtered.searchParams.get('range'), filtered.searchParams.get('keys'), filtered.searchParams.get('pools')],
    ['/admin/analytics/placements', '0,86400000', 'desktop,ci', 'all'],
  );
  assert.deepEqual([...bare.searchParams.keys()], ['range'], 'an empty key or pool list is left off the query');
  assert.deepEqual([budgetsUrl.pathname, budgetsUrl.search], ['/admin/analytics/budgets', ''], 'the budgets route reads no query');
  assert.deepEqual(placements, [{ cacheReadTokens: 1_000, hitShare: 1, hits: 2, placements: 2, promisedTokens: 1_000, rung: 'content' }]);
  // Each of the row's fields reads under its camel-case name with the route's value, the end as an ISO instant.
  const camel = (name) => name.replace(/_(\w)/g, (_, c) => c.toUpperCase());
  const decoded = (row) => Object.fromEntries(Object.entries(row).map(([k, v]) => [camel(k), k === 'period_end' ? new Date(v).toISOString() : v]));
  assert.deepEqual(budgets, bodies['/admin/analytics/budgets'].members.map(decoded));
  const end = new Date(2_678_400_000).toISOString();
  assert.deepEqual(
    budgets.map((b) => [b.accountId, b.kind, b.spentUsd, b.periodEnd]),
    [
      ['business', 'oauth', 0, end],
      ['direct', 'api_key', 0.0036, end],
    ],
  );
});

test('getSession answers null on 401 without running the hook', async () => {
  let unauthorized = 0;
  setHooks({ onForbidden: () => undefined, onUnauthorized: () => { unauthorized += 1; } });
  stub(() => answer(401, { error: 'no admin session' }));
  assert.equal(await new HttpApi().getSession(), null);
  assert.equal(unauthorized, 0);
  stub(() => answer(200, { session_id: 's' }));
  assert.deepEqual(await new HttpApi().getSession(), { sessionId: 's' });
});

test('the session probe raises the status it got and 0 for no answer', async () => {
  stub(() => answer(503, { error: 'service unavailable' }));
  await assert.rejects(new HttpApi().getSession(), (e) => e instanceof ApiError && e.status === 503);
  stub(() => Promise.reject(new TypeError('fetch failed')));
  await assert.rejects(new HttpApi().getSession(), (e) => e instanceof ApiError && e.status === 0 && e.message === 'fetch failed');
});

test('a login the router accepted with no cookie kept raises SessionNotKept', async () => {
  const calls = stub((_url, init) => (init.method === 'POST' ? answer(204) : answer(401, { error: 'no admin session' })));
  await assert.rejects(new HttpApi().login('secret'), (e) => e instanceof SessionNotKept && !(e instanceof ApiError));
  assert.deepEqual(calls.map((c) => c.init.method ?? 'GET'), ['POST', 'GET'], 'the session read follows the accepted login');
});

test('a failed call names its route on the error, whether the router answered or not', async () => {
  setHooks({ onFailure: () => undefined, onForbidden: () => undefined, onRecovered: () => undefined, onUnauthorized: () => undefined });
  stub(() => answer(503, { error: 'ledger locked' }));
  await assert.rejects(call('GET', '/admin/dashboard'), (e) => e instanceof ApiError && e.path === '/admin/dashboard');
  stub(() => Promise.reject(new TypeError('fetch failed')));
  await assert.rejects(call('GET', '/admin/sessions'), (e) => e instanceof ApiError && e.status === 0 && e.path === '/admin/sessions');
  assert.equal(new ApiError(409, 'taken', null).path, null, 'an error raised outside the transport names no route');
});

test('a call started inside quietly reports no failure, and one started after it does', async () => {
  const failures = [];
  setHooks({ onFailure: (f) => failures.push(f), onForbidden: () => undefined, onRecovered: () => undefined, onUnauthorized: () => undefined });
  stub(() => answer(503, { error: 'ledger locked' }));
  await assert.rejects(quietly(() => call('GET', '/admin/dashboard')), (e) => e instanceof ApiError && e.status === 503);
  stub(() => Promise.reject(new TypeError('fetch failed')));
  await assert.rejects(quietly(() => call('GET', '/admin/sessions')), (e) => e instanceof ApiError && e.status === 0);
  assert.deepEqual(failures, [], 'neither the refusal nor the missing answer reached the toast');
  stub(() => answer(503, { error: 'ledger locked' }));
  await assert.rejects(call('GET', '/admin/dashboard'));
  assert.deepEqual(failures.map((f) => [f.path, f.status]), [['/admin/dashboard', 503]], 'the window closed with the call it wrapped');
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

  stub(() => answer(200, { session_id: 's' }));
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

test('a live frame the decoder cannot read is skipped, warned about once, and the subscription stays open', () => {
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
  const warned = [];
  const warn = console.warn;
  console.warn = (...args) => warned.push(args);
  try {
    const events = [];
    const stop = new HttpApi().subscribeLive((e) => events.push(e));
    assert.doesNotThrow(() => sockets[0].onmessage({ data: 'not json' }));
    assert.doesNotThrow(() => sockets[0].onmessage({ data: 'null' }), 'a frame that parses to nothing the decoder can read');
    sockets[0].onmessage({ data: JSON.stringify({ kind: 'snapshot', snapshot: { cursor: 3, in_flight: [], recent: [] } }) });
    assert.deepEqual(events.map((e) => e.kind), ['snapshot'], 'the next frame still reaches the page');
    assert.equal(warned.length, 1);
    assert.deepEqual([sockets.length, sockets[0].closed], [1, undefined], 'the socket is neither closed nor replaced');
    stop();
  } finally {
    console.warn = warn;
  }
});

test('the request detail reads the row and its events from one GET, and a purged row reads as not found', async () => {
  const body = JSON.parse(readFileSync(new URL('./fixtures/request.json', import.meta.url), 'utf8')).body;
  const calls = stub(() => answer(200, body));
  const api = new HttpApi();
  const detail = await api.getRequest('r-failover');
  assert.deepEqual(calls.map((c) => [c.init.method, c.url]), [['GET', '/admin/requests/r-failover']]);
  assert.deepEqual([detail.request.id, detail.events.map((e) => e.kind)], [body.request.id, body.events.map((e) => e.kind)]);
  stub(() => answer(404, { error: 'no such request' }));
  assert.equal(await api.getRequest('gone'), null);
});

test('a key create and a key edit send the auto mode override in snake_case', async () => {
  const row = JSON.parse(readFileSync(new URL('./fixtures/keys.json', import.meta.url), 'utf8')).body.keys[0];
  const input = { autoModeServer: null, bodyCaptureHours: null, enabled: true, keepaliveOptOut: false, longContextTwins: true, modelRules: [], modelRulesMode: 'extend', name: 'Desktop', notes: null, poolId: 'main' };
  const calls = stub((url, init) => answer(200, { ...row, ...JSON.parse(init.body), ...(init.method === 'POST' ? { secret: 'phoebis-fixture' } : {}) }));
  const api = new HttpApi();
  const created = await api.createKey({ ...input, expiresInDays: null });
  const edited = await api.updateKey('desktop', { ...input, autoModeServer: false });
  const sent = calls.map((c) => [c.init.method, c.url, JSON.parse(c.init.body).auto_mode_server]);
  assert.deepEqual(sent, [['POST', '/admin/keys', null], ['PATCH', '/admin/keys/desktop', false]]);
  assert.deepEqual([created.key.autoModeServer, edited.autoModeServer], [null, false]);
});

test('a pool save is one PATCH that carries the members with the settings', async () => {
  const row = JSON.parse(readFileSync(new URL('./fixtures/pools.json', import.meta.url), 'utf8')).body.pools[0];
  const calls = stub(() => answer(200, row));
  await new HttpApi().updatePool('main', {
    affinityTtlSeconds: 600,
    bodyCaptureHours: 0,
    cacheDiagnostics: false,
    cacheTtl: { enabled: false, injectWhenAbsent: false, metered: { main: '5m', models: [], subagent: '5m' }, plan: { main: '1h', models: [], subagent: '5m' } },
    keepalive: { coldProbeLimit: 2, costCapUsdPerDay: null, enabled: false, jitterSeconds1h: 30, jitterSeconds5m: 5, leadSeconds1h: 360, leadSeconds5m: 60, maxIdleSeconds: 3_600, maxIdleTicks: 0, netNegativeGraceProbes: 2, retryMaxSeconds: 300, retrySeconds: 30, stopWhenNetNegative: true, warmSubagents: true },
    affinityMode: 'when_cached',
    members: [{ accountId: 'business', budgetDailyUsd: null, budgetMonthlyUsd: null, priority: 0 }, { accountId: 'direct', budgetDailyUsd: 12.5, budgetMonthlyUsd: 50, priority: 1 }],
    migrateAboveUtilization: null,
    minSharedPrefixTokens: 30_000,
    modelRules: [],
    name: row.label,
    overagePolicy: 'prefer_plan',
    preferSubscription: false,
    selectBelowUtilization: 0.4,
    sessionPlacement: 'by_warm_prefix',
    status: 'active',
    strategy: 'pace',
    subagentPlacement: 'with_main',
  });
  assert.deepEqual(calls.map((c) => [c.init.method, c.url]), [['PATCH', '/admin/pools/main']], 'a second write could fail after the first landed');
  const body = JSON.parse(calls[0].init.body);
  assert.deepEqual(body.members, [
    { account_id: 'business', budget_daily_usd: null, budget_monthly_usd: null, priority: 0 },
    { account_id: 'direct', budget_daily_usd: 12.5, budget_monthly_usd: 50, priority: 1 },
  ], 'a member without a budget sends null, which clears a stored one');
  assert.equal(body.select_below_utilization, 0.4);
  assert.equal(body.cache_diagnostics, false, 'the switch is sent off as it is sent on');
  assert.deepEqual(
    [body.affinity_mode, body.subagent_placement, body.session_placement, body.min_shared_prefix_tokens],
    ['when_cached', 'with_main', 'by_warm_prefix', 30_000],
  );
});

test('deleting an account calls the account route, and a disconnect keeps to the credentials route', async () => {
  const calls = stub(() => answer(204));
  const api = new HttpApi();
  assert.equal(await api.deleteAccount('work seat'), undefined);
  await api.disconnectAccount('work seat');
  assert.deepEqual(calls.map((c) => [c.init.method, c.url]), [['DELETE', '/admin/accounts/work%20seat'], ['DELETE', '/admin/accounts/work%20seat/credentials']]);
});

test('listCatalogOf reads each named catalog once and answers the union by id, a failed read adding nothing', async () => {
  const row = (id, displayName) => ({ created_at: '', display_name: displayName, id, max_input_tokens: null, max_tokens: null });
  const catalogs = {
    '/admin/accounts/alpha/catalog': answer(200, { error: null, fetched_at: 1_000_000, models: [row('claude-sonnet-5', 'Sonnet from alpha'), row('claude-haiku-4-5-20251001', 'Haiku')] }),
    '/admin/accounts/beta/catalog': answer(200, { error: null, fetched_at: 1_000_000, models: [row('claude-opus-5', 'Opus'), row('claude-sonnet-5', 'Sonnet from beta')] }),
    '/admin/accounts/gamma/catalog': answer(503, { error: 'the registry is unavailable' }),
  };
  let failures = 0;
  setHooks({ onFailure: () => { failures += 1; }, onForbidden: () => undefined, onUnauthorized: () => undefined });
  const calls = stub((url) => catalogs[url] ?? answer(404, { error: 'account not found' }));
  const models = await new HttpApi().listCatalogOf(['alpha', 'beta', 'gamma']);
  assert.deepEqual(calls.map((c) => [c.init.method, c.url]), [['GET', '/admin/accounts/alpha/catalog'], ['GET', '/admin/accounts/beta/catalog'], ['GET', '/admin/accounts/gamma/catalog']]);
  assert.deepEqual(models.map((m) => m.id), ['claude-haiku-4-5-20251001', 'claude-opus-5', 'claude-sonnet-5'], 'a model two accounts list appears once, in id order');
  assert.equal(models.find((m) => m.id === 'claude-sonnet-5').displayName, 'Sonnet from alpha', 'the first catalog naming an id answers for it');
  assert.equal(failures, 1, 'the failed read still reaches the toast while the union answers');
  const none = stub(() => answer(500));
  assert.deepEqual(await new HttpApi().listCatalogOf([]), []);
  assert.equal(none.length, 0, 'no account named, no read');
});

test('the attention read is one GET, and its failures reach no toast, since the line draws its own', async () => {
  let failures = 0;
  let unauthorized = 0;
  setHooks({ onFailure: () => { failures += 1; }, onForbidden: () => undefined, onUnauthorized: () => { unauthorized += 1; } });
  const body = JSON.parse(readFileSync(new URL('./fixtures/attention.json', import.meta.url), 'utf8')).body;
  const calls = stub(() => answer(200, body));
  const items = await new HttpApi().getAttention();
  assert.deepEqual(calls.map((c) => [c.init.method, c.url]), [['GET', '/admin/attention']]);
  assert.deepEqual(items.map((i) => [i.kind, i.subject.id, i.link]), [['pool_exhausted', 'main', { id: 'main', page: 'pools' }]]);
  stub(() => answer(500, { error: 'the store is unavailable' }));
  await assert.rejects(new HttpApi().getAttention(), (e) => e instanceof ApiError && e.status === 500 && e.message === 'the store is unavailable');
  stub(() => Promise.reject(new TypeError('fetch failed')));
  await assert.rejects(new HttpApi().getAttention(), (e) => e instanceof ApiError && e.status === 0);
  assert.equal(failures, 0, 'a failed attention read is drawn by the line, not toasted on every poll');
  stub(() => answer(401, { error: 'no admin session' }));
  await assert.rejects(new HttpApi().getAttention());
  assert.equal(unauthorized, 1, 'a lapsed session still returns the console to the login');
  stub(() => answer(500, { error: 'the store is unavailable' }));
  await assert.rejects(call('GET', '/admin/keys'));
  assert.equal(failures, 1, 'the same failure on any other read still reaches the toast');
});
