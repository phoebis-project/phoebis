import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import { ApiError, HttpApi, SessionNotKept, call, fakeClock, installClock, noteBuild, quietly, setBuildHook, setHooks, slug } from './dist/api.js';

const answer = (status, body, headers = {}) => ({ headers: { get: (k) => headers[k.toLowerCase()] ?? null }, ok: status >= 200 && status < 300, status, text: async () => (body === undefined ? '' : JSON.stringify(body)), json: async () => body });
const stub = (handler) => { const calls = []; globalThis.fetch = async (url, init = {}) => { calls.push({ init, url }); return handler(url, init); }; return calls; };
/** The build `test/vite.config.ts` gives the bundle, and a router build other than it. */
const BUNDLE_BUILD = 'feedfacefeedfacefeedfacefeedfacefeedface';
const ROUTER_BUILD = '0000000000000000000000000000000000000000';

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
  await assert.rejects(new HttpApi().login('x'));
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
    await assert.rejects(new HttpApi().login('x'), (e) => e instanceof ApiError && e.status === status);
  }
  stub(() => Promise.reject(new TypeError('fetch failed')));
  await assert.rejects(new HttpApi().login('x'), (e) => e instanceof ApiError && e.status === 0);
  assert.deepEqual([failures, forbidden, unauthorized], [0, 0, 0]);
  stub(() => answer(429, { error: 'too many failed logins, try again in a minute' }));
  await assert.rejects(call('GET', '/admin/keys'));
  assert.equal(failures, 1, 'the same refusal on any other route still reaches the toast');
});

test("a router build other than the bundle's calls the build hook once per registration, and no build, unknown or the bundle's own calls nothing", () => {
  const seen = [];
  setBuildHook((router) => seen.push(router));
  noteBuild(null);
  noteBuild('unknown');
  noteBuild(BUNDLE_BUILD);
  assert.deepEqual(seen, [], 'nothing differs');
  noteBuild(ROUTER_BUILD);
  noteBuild(ROUTER_BUILD);
  assert.deepEqual(seen, [ROUTER_BUILD], 'the hook fires once over two answers');
  setBuildHook((router) => seen.push(router));
  noteBuild(ROUTER_BUILD);
  assert.deepEqual(seen, [ROUTER_BUILD, ROUTER_BUILD], 'a registration arms the hook again');
  setBuildHook(null);
});

test('an answer that lands with no build hook registered notes nothing, so a registration after it waits for the next answer', () => {
  setBuildHook(null);
  noteBuild(ROUTER_BUILD);
  const seen = [];
  setBuildHook((router) => seen.push(router));
  assert.deepEqual(seen, []);
  noteBuild(ROUTER_BUILD);
  assert.deepEqual(seen, [ROUTER_BUILD]);
  setBuildHook(null);
});

test("call reads the router's build off every answer, a 204 and a refusal among them", async () => {
  setHooks({ onFailure: () => undefined, onForbidden: () => undefined, onRecovered: () => undefined, onUnauthorized: () => undefined });
  const seen = [];
  setBuildHook((router) => seen.push(router));
  stub(() => answer(200, { keys: [] }));
  await call('GET', '/admin/keys');
  stub(() => answer(200, { keys: [] }, { 'x-phoebis-build': BUNDLE_BUILD }));
  await call('GET', '/admin/keys');
  stub(() => answer(204, undefined, { 'x-phoebis-build': 'unknown' }));
  await call('DELETE', '/admin/keys/k');
  assert.deepEqual(seen, [], 'an older router, the same build and a router that cannot name its build draw nothing');
  stub(() => answer(204, undefined, { 'x-phoebis-build': ROUTER_BUILD }));
  await call('DELETE', '/admin/keys/k');
  assert.deepEqual(seen, [ROUTER_BUILD], 'a 204 names the build too');
  setBuildHook((router) => seen.push(router));
  stub(() => answer(503, { error: 'ledger locked' }, { 'x-phoebis-build': ROUTER_BUILD }));
  await assert.rejects(call('GET', '/admin/keys'));
  assert.deepEqual(seen, [ROUTER_BUILD, ROUTER_BUILD], 'and so does a refusal');
  setBuildHook(null);
});

test("the build hook's registration leaves the session's handlers in place, whichever of the two comes first", async () => {
  for (const buildFirst of [true, false]) {
    const ran = { build: 0, failure: 0, unauthorized: 0 };
    const handlers = { onFailure: () => { ran.failure += 1; }, onForbidden: () => undefined, onRecovered: () => undefined, onUnauthorized: () => { ran.unauthorized += 1; } };
    const hook = () => { ran.build += 1; };
    if (buildFirst) {
      setBuildHook(hook);
      setHooks(handlers);
    } else {
      setHooks(handlers);
      setBuildHook(hook);
    }
    stub(() => answer(401, { error: 'no admin session' }));
    await assert.rejects(call('GET', '/admin/keys'));
    stub(() => answer(503, { error: 'ledger locked' }));
    await assert.rejects(call('GET', '/admin/keys'));
    stub(() => answer(200, { keys: [] }, { 'x-phoebis-build': ROUTER_BUILD }));
    await call('GET', '/admin/keys');
    assert.deepEqual(ran, { build: 1, failure: 1, unauthorized: 1 }, buildFirst ? 'the build hook registered first' : 'the build hook registered last');
  }
  setBuildHook(null);
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
  await api.useLimitReset('acc', 'launch/1');
  assert.equal(calls[2].url, '/admin/accounts/acc/limit-resets/launch%2F1/use');
  assert.equal(slug('My Pool!'), 'my-pool');
  assert.equal(slug(`${'a'.repeat(63)} tail`), 'a'.repeat(63), 'a name cut at the limit keeps no trailing separator');
});

test('a use sends the revision of the member it was confirmed on as expected_revision, and no body without one', async () => {
  const calls = stub(() => answer(200, { outcome: 'used', result: 'reset' }));
  const api = new HttpApi();
  await api.useLimitReset('acc', 'rg-acc-1', 5);
  await api.useLimitReset('acc', 'rg-acc-1');
  assert.deepEqual(
    calls.map((c) => [c.init.method, c.init.body]),
    [
      ['POST', '{"expected_revision":5}'],
      ['POST', undefined],
    ],
  );
});

test('listRequests sends the key, the direction and the fallback switch, and none under the defaults, and listRoutingLog sends the direction', async () => {
  const calls = stub((url) => answer(200, url.startsWith('/admin/requests') ? { next_cursor: null, requests: [] } : { next_cursor: null, rows: [] }));
  const api = new HttpApi();
  await api.listRequests({ dir: 'asc', fallback: false, order: 'ttfb_ms' });
  const sent = new URL(calls[0].url, 'http://x').searchParams;
  assert.deepEqual([sent.get('dir'), sent.get('fallback'), sent.get('order')], ['asc', 'false', 'ttfb_ms'], 'a switch set to false travels too');
  await api.listRequests({});
  const bare = new URL(calls[1].url, 'http://x').searchParams;
  assert.deepEqual([bare.get('dir'), bare.get('fallback'), bare.get('order')], [null, null, null], "the route's defaults travel as no key");
  await api.listRoutingLog({ dir: 'asc' });
  assert.equal(new URL(calls[2].url, 'http://x').searchParams.get('dir'), 'asc');
});

test('listRoutingLog encodes comma lists, ISO instants and kinds, and getDecision names its path', async () => {
  const record = JSON.parse(readFileSync(new URL('./fixtures/decision.json', import.meta.url), 'utf8')).body;
  const calls = stub((url) => (url.startsWith('/admin/routing-log') ? answer(200, { next_cursor: null, rows: [] }) : url.includes('/gone/') ? answer(404, { error: 'no decision 1 for request gone' }) : answer(200, record)));
  const api = new HttpApi();
  const page = await api.listRoutingLog({ accountIds: ['primary'], agentId: 'main', cursor: '1000:1:r/1', from: '2026-09-16T13:30:00.000Z', keyIds: ['a', 'b'], kinds: ['placed', 'probe'], limit: 25, models: ['claude-sonnet-5'], poolIds: ['all'], request: 'req_1', session: 's1', to: '2026-09-16T14:30:00.000Z' });
  assert.deepEqual(page, { counts: null, nextCursor: null, rows: [] });
  const url = new URL(calls[0].url, 'http://x');
  assert.equal(url.pathname, '/admin/routing-log');
  assert.deepEqual(Object.fromEntries(url.searchParams), {
    accounts: 'primary', agent_id: 'main', cursor: '1000:1:r/1', from: '2026-09-16T13:30:00.000Z', keys: 'a,b', kinds: 'placed,probe', limit: '25', models: 'claude-sonnet-5', pools: 'all', request: 'req_1', session: 's1', to: '2026-09-16T14:30:00.000Z',
  }, 'the route reads RFC 3339 instants, so both times travel as the page wrote them');
  await api.listRoutingLog();
  assert.equal(calls[1].url, '/admin/routing-log', 'no filter sends no query');
  const decoded = await api.getDecision('req/1', 2);
  assert.equal(calls[2].url, '/admin/requests/req%2F1/decisions/2');
  assert.deepEqual([decoded.version, decoded.requestId], [3, 'r-static']);
  assert.equal(await api.getDecision('gone', 1), null, 'a decision the ledger no longer holds answers null');
});

test('getKeepaliveOverview sends the range it is given, and no query without one', async () => {
  const body = JSON.parse(readFileSync(new URL('./fixtures/keepalive_overview.json', import.meta.url), 'utf8')).body;
  const calls = stub(() => answer(200, body));
  const api = new HttpApi();
  await api.getKeepaliveOverview({ range: '30d' });
  await api.getKeepaliveOverview({ range: '7d' });
  await api.getKeepaliveOverview();
  assert.deepEqual(calls.map((c) => c.url), ['/admin/keepalive/overview?range=30d', '/admin/keepalive/overview?range=7d', '/admin/keepalive/overview']);
});

test('createAccount sends the id, kind, label, and secret in snake_case', async () => {
  const body = JSON.parse(readFileSync(new URL('./fixtures/account.json', import.meta.url), 'utf8')).body;
  const calls = stub(() => answer(200, body));
  await new HttpApi().createAccount({ id: 'console', kind: 'api_key', label: 'Console', secret: 'sk-ant-api03-FAKE' });
  assert.deepEqual([calls[0].init.method, calls[0].url, JSON.parse(calls[0].init.body)], ['POST', '/admin/accounts', { id: 'console', kind: 'api_key', label: 'Console', secret: 'sk-ant-api03-FAKE' }]);
});

test('the accounts list is one GET, and no usage route is read', async () => {
  const body = JSON.parse(readFileSync(new URL('./fixtures/accounts.json', import.meta.url), 'utf8')).body;
  const calls = stub(() => answer(200, body));
  await new HttpApi().listAccounts();
  assert.deepEqual(calls.map((c) => [c.init.method ?? 'GET', c.url]), [['GET', '/admin/accounts']], 'one read of the list, no usage read per account');
});

test('updateAccount is one PATCH that carries the canceled flag and the end in milliseconds', async () => {
  const body = JSON.parse(readFileSync(new URL('./fixtures/account.json', import.meta.url), 'utf8')).body;
  const calls = stub(() => answer(200, { ...body, canceled: true }));
  const end = '2026-10-19T09:15:00.000Z';
  const account = await new HttpApi().updateAccount('business', { canceled: true, subscriptionEndsAt: end });
  assert.deepEqual([calls[0].init.method, calls[0].url, JSON.parse(calls[0].init.body)], ['PATCH', '/admin/accounts/business', { canceled: true, subscription_ends_at: Date.parse(end) }]);
  assert.equal(account.canceled, true, 'the answer decodes as the account');
});

test('an account edit sends the revision it read as expected_revision, and none for a save that read no row', async () => {
  const body = JSON.parse(readFileSync(new URL('./fixtures/account.json', import.meta.url), 'utf8')).body;
  const calls = stub(() => answer(200, body));
  const api = new HttpApi();
  const end = '2026-10-19T09:15:00.000Z';
  const patch = { canceled: true, subscriptionEndsAt: end };
  await api.updateAccount('business', patch, 3);
  await api.renameAccount('business', 'x', 3);
  await api.updateAccount('business', patch);
  await api.renameAccount('business', 'x');
  assert.deepEqual(
    calls.map((c) => [c.init.method, c.url, JSON.parse(c.init.body)]),
    [
      ['PATCH', '/admin/accounts/business', { canceled: true, subscription_ends_at: Date.parse(end), expected_revision: 3 }],
      ['PATCH', '/admin/accounts/business', { label: 'x', expected_revision: 3 }],
      ['PATCH', '/admin/accounts/business', { canceled: true, subscription_ends_at: Date.parse(end) }],
      ['PATCH', '/admin/accounts/business', { label: 'x' }],
    ],
  );
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

test('listRequests passes the agent id as agent_id beside the session, and a query without either sends neither', async () => {
  const calls = stub(() => answer(200, { next_cursor: null, requests: [] }));
  await new HttpApi().listRequests({ agentId: 'main', session: 's1' });
  const params = new URL(calls[0].url, 'http://x').searchParams;
  assert.deepEqual([params.get('agent_id'), params.get('session')], ['main', 's1'], String(calls[0].url));
  await new HttpApi().listRequests({});
  const bare = new URL(calls[1].url, 'http://x').searchParams;
  assert.deepEqual([bare.has('agent_id'), bare.has('session')], [false, false], String(calls[1].url));
});

test('listRequests asks for the held requests alone when the query says so', async () => {
  const calls = stub(() => answer(200, { next_cursor: null, requests: [] }));
  await new HttpApi().listRequests({ held: true });
  assert.equal(new URL(calls[0].url, 'http://x').searchParams.get('held'), 'true');
  await new HttpApi().listRequests({});
  assert.ok(!String(calls[1].url).includes('held'), 'no held filter, no parameter');
});

test('listRequests marks a fixed-end range when the query says so', async () => {
  const calls = stub(() => answer(200, { landed_through: 0, next_cursor: null, requests: [] }));
  await new HttpApi().listRequests({ fixedEnd: true });
  assert.equal(new URL(calls[0].url, 'http://x').searchParams.get('fixed_end'), 'true');
  await new HttpApi().listRequests({});
  assert.ok(!String(calls[1].url).includes('fixed_end'), 'no fixed end, no parameter');
});

test('listRequests passes the stamp a poll read last as landed_after', async () => {
  const calls = stub(() => answer(200, { landed_through: 0, next_cursor: null, requests: [] }));
  await new HttpApi().listRequests({ landedAfter: 1_700_000_000_123 });
  assert.equal(new URL(calls[0].url, 'http://x').searchParams.get('landed_after'), '1700000000123');
  await new HttpApi().listRequests({});
  assert.ok(!String(calls[1].url).includes('landed_after'), 'no stamp, no parameter');
});

test('listRequests passes the speeds as one comma list', async () => {
  const calls = stub(() => answer(200, { next_cursor: null, requests: [] }));
  await new HttpApi().listRequests({ speeds: ['fast', 'standard'] });
  assert.equal(new URL(calls[0].url, 'http://x').searchParams.get('speeds'), 'fast,standard');
  await new HttpApi().listRequests({});
  assert.ok(!String(calls[1].url).includes('speeds'), 'no speed filter, no parameter');
});

test('listRequests passes the cache miss reasons, the efforts and the request classes as comma lists', async () => {
  const calls = stub(() => answer(200, { next_cursor: null, requests: [] }));
  await new HttpApi().listRequests({ cacheMissReasons: ['previous_message_not_found', 'tools_changed'], efforts: ['high', 'none'], requestClasses: ['main', 'other'] });
  const params = new URL(calls[0].url, 'http://x').searchParams;
  assert.deepEqual([params.get('cache_miss_reasons'), params.get('efforts'), params.get('request_classes')], ['previous_message_not_found,tools_changed', 'high,none', 'main,other']);
  await new HttpApi().listRequests({});
  for (const name of ['cache_miss_reasons', 'efforts', 'request_classes']) assert.ok(!String(calls[1].url).includes(name), `no ${name} filter, no parameter`);
});

test('the_classifiers_filter_serializes_beside_the_request_classes', async () => {
  const calls = stub(() => answer(200, { next_cursor: null, requests: [] }));
  await new HttpApi().listRequests({ classifiers: ['client', '-'], requestClasses: ['auxiliary'] });
  const params = new URL(calls[0].url, 'http://x').searchParams;
  assert.deepEqual([params.get('classifiers'), params.get('request_classes')], ['client,-', 'auxiliary']);
  await new HttpApi().listRequests({});
  assert.ok(!String(calls[1].url).includes('classifiers'), 'no classifier filter, no parameter');
});

test('the request list encodes the outcome list', async () => {
  const calls = stub(() => answer(200, { next_cursor: null, requests: [] }));
  await new HttpApi().listRequests({ outcomes: ['canceled', 'failed'] });
  assert.equal(new URL(calls[0].url, 'http://x').searchParams.get('outcomes'), 'canceled,failed');
  await new HttpApi().listRequests({});
  assert.ok(!String(calls[1].url).includes('outcomes'), 'no outcome filter, no parameter');
});

test('listRequests passes the refusal categories and the stop reasons as comma lists', async () => {
  const calls = stub(() => answer(200, { next_cursor: null, requests: [] }));
  await new HttpApi().listRequests({ refusalCategories: ['cyber', '-'], stopReasons: ['refusal'] });
  const params = new URL(calls[0].url, 'http://x').searchParams;
  assert.deepEqual([params.get('refusal_categories'), params.get('stop_reasons')], ['cyber,-', 'refusal']);
  await new HttpApi().listRequests({});
  for (const name of ['refusal_categories', 'stop_reasons']) assert.ok(!String(calls[1].url).includes(name), `no ${name} filter, no parameter`);
});

test('the refusal categories read sends the analytics query and answers the rows in the route order', async () => {
  const rows = [{ category: 'cyber', requests: 3 }, { category: '-', requests: 1 }];
  const calls = stub(() => answer(200, { from: 0, rows, to: 86_400_000 }));
  const q = { accountIds: ['primary'], from: '1970-01-01T00:00:00.000Z', groupBy: 'all', includeProbes: true, keyIds: ['desktop'], kinds: ['inference'], models: ['claude-sonnet-5'], poolIds: ['all'], to: '1970-01-02T00:00:00.000Z' };
  const read = await new HttpApi().getAnalyticsRefusalCategories(q);
  const url = new URL(calls[0].url, 'http://x');
  assert.equal(url.pathname, '/admin/analytics/refusal_categories');
  assert.deepEqual(Object.fromEntries(url.searchParams), { accounts: 'primary', group_by: 'all', include_probes: 'true', keys: 'desktop', kinds: 'inference', models: 'claude-sonnet-5', pools: 'all', range: '0,86400000' });
  assert.deepEqual(read, { fallbacks: 0, from: '1970-01-01T00:00:00.000Z', rows: [{ category: 'cyber', model: '', requests: 3 }, { category: '-', model: '', requests: 1 }], to: '1970-01-02T00:00:00.000Z' });
});

test('the unused allowance read sends the one range parameter and the Accounts filter alone, and no accounts without one', async () => {
  const calls = stub(() => answer(200, { accounts: [], from: 0, to: 86_400_000 }));
  const api = new HttpApi();
  const span = { from: '1970-01-01T00:00:00.000Z', to: '1970-01-02T00:00:00.000Z' };
  assert.deepEqual(await api.getAnalyticsUnusedAllowance(span, { accountIds: ['primary'] }), { accounts: [], from: span.from, to: span.to });
  await api.getAnalyticsUnusedAllowance(span);
  const [named, every] = calls.map((c) => new URL(c.url, 'http://x'));
  assert.equal(named.pathname, '/admin/analytics/unused_allowance');
  assert.deepEqual([...named.searchParams], [['accounts', 'primary'], ['range', '0,86400000']]);
  assert.deepEqual([every.pathname, [...every.searchParams]], ['/admin/analytics/unused_allowance', [['range', '0,86400000']]]);
});

test('listRequests passes the request and the session as they are', async () => {
  const calls = stub(() => answer(200, { next_cursor: null, requests: [] }));
  await new HttpApi().listRequests({ request: 'req_1', session: 's1' });
  assert.equal(calls[0].url, '/admin/requests?request=req_1&session=s1');
});

test('the analytics reads encode the range, the dimension, and every id list', async () => {
  const calls = stub((url) => answer(200, url.startsWith('/admin/analytics/series') ? { points: [] } : { rows: [] }));
  const api = new HttpApi();
  const q = { accountIds: ['business', 'direct'], clients: ['a', 'b'], from: '1970-01-01T00:00:00.000Z', funding: 'plan', groupBy: 'key', includeProbes: true, keyIds: ['desktop'], models: ['claude-sonnet-5'], poolIds: ['all'], to: '1970-01-02T00:00:00.000Z', userAgentGroups: ['a', 'b'] };
  assert.deepEqual(await api.getAnalyticsSeries(q), []);
  const series = new URL(calls[0].url, 'http://x');
  assert.equal(series.pathname, '/admin/analytics/series');
  assert.equal(series.searchParams.get('range'), '0,86400000');
  assert.equal(series.searchParams.get('group_by'), 'key');
  assert.equal(series.searchParams.get('include_probes'), 'true');
  assert.equal(series.searchParams.get('funding'), 'plan');
  assert.equal(series.searchParams.get('accounts'), 'business,direct');
  assert.equal(series.searchParams.get('keys'), 'desktop');
  assert.equal(series.searchParams.get('models'), 'claude-sonnet-5');
  assert.equal(series.searchParams.get('pools'), 'all');
  // One pattern per line, so a pattern holding a comma still names one group.
  assert.ok(String(calls[0].url).includes('user_agent_groups=a%0Ab'), String(calls[0].url));
  assert.ok(String(calls[0].url).includes('clients=a%0Ab'), String(calls[0].url));
  assert.deepEqual(await api.getAnalyticsBreakdown({ ...q, accountIds: [], clients: undefined, funding: undefined, groupBy: 'all', includeProbes: undefined, keyIds: [], models: [], poolIds: [], userAgentGroups: undefined }), []);
  const breakdown = new URL(calls[1].url, 'http://x');
  assert.equal(breakdown.pathname, '/admin/analytics/breakdown');
  assert.equal(breakdown.searchParams.get('group_by'), 'all');
  assert.deepEqual([...breakdown.searchParams.keys()], ['group_by', 'range'], 'an empty list, an unset flag and an unset funding are left off the query');
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
  // A subscription point that burned, whose window shares a misnamed field would read as null.
  const planned = bodies['/admin/analytics/burn'].points.findLastIndex((p) => p.five_hour_share > 0 && p.seven_day_share > 0);
  assert.ok(planned >= 0, 'the fixture reports a subscription point that burned');
  assert.deepEqual(
    [burn.bucketSeconds, burn.points.length, burn.points[planned].fiveHourShare, burn.points[planned].sevenDayShare],
    [3_600, bodies['/admin/analytics/burn'].points.length, bodies['/admin/analytics/burn'].points[planned].five_hour_share, bodies['/admin/analytics/burn'].points[planned].seven_day_share],
    'both window shares read as the route answers them',
  );
  assert.deepEqual([rewrites.moves, rewrites.points.length], [{ spent: 1 }, bodies['/admin/analytics/rewrites'].points.length]);
  await api.getAnalyticsBurn(range, 'user_agent', { accountIds: ['primary'], userAgentGroups: ['a,1', 'b'] });
  const filtered = new URL(calls[2].url, 'http://x');
  assert.deepEqual(
    [filtered.searchParams.get('accounts'), filtered.searchParams.get('group_by'), filtered.searchParams.get('user_agent_groups')],
    ['primary', 'user_agent', 'a,1\nb'],
    'the Accounts filter and the user agent groups, one pattern per line, travel with the grouping',
  );
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
  assert.deepEqual(budgets.map((b) => [b.poolId, b.budgetUsd, b.budgetPeriod]), [['main', 30, 'month'], ['main', 50, 'month']]);
  // The month end and the subscription's spend follow the capture's clock, so they are read from the capture.
  const wire = Object.fromEntries(bodies['/admin/analytics/budgets'].members.map((m) => [m.account_id, m]));
  const end = new Date(wire.direct.period_end).toISOString();
  assert.deepEqual(
    budgets.map((b) => [b.accountId, b.kind, b.spentUsd, b.periodEnd]),
    [
      ['business', 'oauth', wire.business.spent_usd, end],
      ['direct', 'api_key', 0.00715, end],
    ],
  );
});

test('the burn read sends the key and pool filters', async () => {
  const calls = stub(() => answer(200, {}));
  await new HttpApi().getAnalyticsBurn({ from: '1970-01-01T00:00:00.000Z', to: '1970-01-02T00:00:00.000Z' }, 'key', { keyIds: ['desktop', 'ci@1'], poolIds: ['all'] });
  const url = new URL(calls[0].url, 'http://x');
  assert.deepEqual([url.searchParams.get('keys'), url.searchParams.get('pools')], ['desktop,ci@1', 'all']);
});

test('the rewrites read sends the account and pool filters', async () => {
  const calls = stub(() => answer(200, {}));
  await new HttpApi().getAnalyticsRewrites({ from: '1970-01-01T00:00:00.000Z', to: '1970-01-02T00:00:00.000Z' }, { accountIds: ['primary'], poolIds: ['all'] });
  const url = new URL(calls[0].url, 'http://x');
  assert.deepEqual([url.searchParams.get('accounts'), url.searchParams.get('pools')], ['primary', 'all']);
});

test('the placements read sends the model and the client patterns', async () => {
  const calls = stub(() => answer(200, { rows: [] }));
  await new HttpApi().analyticsPlacements({ clients: ['claude-code/*', 'other'], from: '1970-01-01T00:00:00.000Z', models: ['claude-opus-5'], to: '1970-01-02T00:00:00.000Z' });
  const url = new URL(calls[0].url, 'http://x');
  // One pattern per line, since a pattern may hold a comma.
  assert.deepEqual([url.searchParams.get('models'), url.searchParams.get('clients')], ['claude-opus-5', 'claude-code/*\nother']);
});

test('getSession answers null on 401 without running the hook', async () => {
  let unauthorized = 0;
  setHooks({ onForbidden: () => undefined, onUnauthorized: () => { unauthorized += 1; } });
  stub(() => answer(401, { error: 'no admin session' }));
  assert.equal(await new HttpApi().getSession(), null);
  assert.equal(unauthorized, 0);
  stub(() => answer(200, { session_id: 's' }));
  assert.deepEqual(await new HttpApi().getSession(), { persistent: false, sessionId: 's' }, 'a body without the member reads as a session that is not kept');
});

test('getSession reads a kept session off the persistent member', async () => {
  stub(() => answer(200, { persistent: true, session_id: 's' }));
  assert.deepEqual(await new HttpApi().getSession(), { persistent: true, sessionId: 's' });
});

test('the session probe raises the status it got and 0 for no answer', async () => {
  stub(() => answer(503, { error: 'service unavailable' }));
  await assert.rejects(new HttpApi().getSession(), (e) => e instanceof ApiError && e.status === 503);
  stub(() => Promise.reject(new TypeError('fetch failed')));
  await assert.rejects(new HttpApi().getSession(), (e) => e instanceof ApiError && e.status === 0 && e.message === 'fetch failed');
});

/**
 * A fetch whose head answers at once and whose body stalls: the body fails as the read's signal aborts, and answers
 * `text` once the clock is past the deadline, so a read that took no signal fails its case rather than hanging it.
 */
const stalledBody = (time, text) => async (_url, init = {}) => {
  const body = new Promise((resolve, reject) => {
    time.setTimeout(() => resolve(text), 60_001);
    init.signal?.addEventListener('abort', () => reject(new DOMException('This operation was aborted', 'AbortError')));
  });
  return { headers: { get: () => null }, ok: true, status: 200, text: () => body };
};

test('a session read with no answer within the deadline fails as a read that never answered', async () => {
  // The suite has no window, so the clock is frozen before the read arms its timer, and only a tick fires one.
  const time = fakeClock();
  time.freeze(0);
  installClock(time);
  try {
    globalThis.fetch = stalledBody(time, JSON.stringify({ session_id: 's' }));
    const read = new HttpApi().getSession();
    time.tick(60_001);
    await assert.rejects(read, (e) => e instanceof ApiError && e.status === 0 && e.message === 'no answer within 60 s');
  } finally {
    installClock(null);
  }
});

test("a session body that is not JSON raises the decoder's error rather than reading as no answer", async () => {
  stub(() => ({ headers: { get: () => null }, ok: true, status: 200, text: async () => '<html>' }));
  await assert.rejects(new HttpApi().getSession(), (e) => e instanceof Error && !(e instanceof ApiError) && /session_id must be a string/.test(e.message));
});

test('a GET with no answer within the deadline fails as a read that never answered', async () => {
  setHooks({ onFailure: () => undefined, onForbidden: () => undefined, onRecovered: () => undefined, onUnauthorized: () => undefined });
  const time = fakeClock();
  time.freeze(0);
  installClock(time);
  try {
    globalThis.fetch = stalledBody(time, JSON.stringify({ accounts: [] }));
    const read = call('GET', '/admin/accounts');
    time.tick(60_001);
    await assert.rejects(read, (e) => e instanceof ApiError && e.status === 0 && e.message === 'no answer within 60 s');
  } finally {
    installClock(null);
  }
});

test('a login the router accepted with no cookie kept raises SessionNotKept', async () => {
  const calls = stub((_url, init) => (init.method === 'POST' ? answer(204) : answer(401, { error: 'no admin session' })));
  await assert.rejects(new HttpApi().login('secret'), (e) => e instanceof SessionNotKept && !(e instanceof ApiError));
  assert.deepEqual(calls.map((c) => c.init.method ?? 'GET'), ['POST', 'GET'], 'the session read follows the accepted login');
});

test('a login sends the Stay logged in choice as stay_logged_in, false where the caller passed none', async () => {
  const kept = stub((_url, init) => (init.method === 'POST' ? answer(204) : answer(200, { persistent: true, session_id: 's' })));
  await new HttpApi().login('s', true);
  assert.deepEqual(JSON.parse(kept[0].init.body), { secret: 's', stay_logged_in: true });
  const plain = stub((_url, init) => (init.method === 'POST' ? answer(204) : answer(200, { persistent: false, session_id: 's' })));
  await new HttpApi().login('s');
  assert.deepEqual(JSON.parse(plain[0].init.body), { secret: 's', stay_logged_in: false });
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

test('the request detail answers the stream cut its body carries, and none where the member is null or absent', async () => {
  const { stream_cut: _absent, ...body } = JSON.parse(readFileSync(new URL('./fixtures/request.json', import.meta.url), 'utf8')).body;
  const cut = { at: 1_000, attempt: 1, bytes_relayed: 846, code: 'stream_undecodable', text: 'error decoding response body', upstream_request_id: 'req_011CUcut' };
  const api = new HttpApi();
  stub(() => answer(200, { ...body, stream_cut: cut }));
  assert.deepEqual((await api.getRequest('r-failover')).streamCut, { at: '1970-01-01T00:00:01.000Z', attempt: 1, bytesRelayed: 846, code: 'stream_undecodable', text: 'error decoding response body', upstreamRequestId: 'req_011CUcut' });
  stub(() => answer(200, { ...body, stream_cut: null }));
  assert.equal((await api.getRequest('r-failover')).streamCut, null, 'a request with no cut reads none');
  stub(() => answer(200, body));
  assert.equal((await api.getRequest('r-failover')).streamCut, null, 'a body without the member reads none');
});

const keyEdit = { autoModeServer: null, bodyCaptureHours: null, enabled: true, longContextTwins: true, modelRules: [], modelRulesMode: 'extend', name: 'Desktop', notes: null, poolId: 'main' };

test('a key creation and a key edit send the auto mode override in snake_case', async () => {
  const row = JSON.parse(readFileSync(new URL('./fixtures/keys.json', import.meta.url), 'utf8')).body.keys[0];
  const calls = stub((url, init) => answer(200, { ...row, ...JSON.parse(init.body), ...(init.method === 'POST' ? { secret: 'phoebis-fixture' } : {}) }));
  const api = new HttpApi();
  const created = await api.createKey({ ...keyEdit, expiresAt: null });
  const edited = await api.updateKey('desktop', { ...keyEdit, autoModeServer: false });
  const sent = calls.map((c) => [c.init.method, c.url, JSON.parse(c.init.body).auto_mode_server]);
  assert.deepEqual(sent, [['POST', '/admin/keys', null], ['PATCH', '/admin/keys/desktop', false]]);
  assert.deepEqual([created.key.autoModeServer, edited.autoModeServer], [null, false]);
});

const poolSave = (name) => ({
  affinityTtlSeconds: 600,
  bodyCaptureHours: 0,
  cacheDiagnostics: false,
  cacheTtl: { enabled: false, injectWhenAbsent: false, metered: { main: '5m', models: [], subagent: '5m' }, plan: { main: '1h', models: [], subagent: '5m' } },
  keepalive: { enabled: false, maxIdleSeconds: 3_600, maxParkedSeconds: null, scheduledHorizonSeconds: null },
  affinityMode: 'when_cached',
  members: [{ accountId: 'business', budgetDailyUsd: null, budgetMonthlyUsd: null, priority: 0 }, { accountId: 'direct', budgetDailyUsd: 12.5, budgetMonthlyUsd: 50, priority: 1 }],
  minSharedPrefixTokens: 30_000,
  modelRules: [],
  name,
  loadShedding: false,
  overagePolicy: 'prefer_plan',
  preferSubscription: false,
  sessionPlacement: 'by_warm_prefix',
  status: 'active',
  strategy: 'pace',
  subagentPlacement: 'with_main',
  fiveHourCeiling: 0.8,
  weeklyCeiling: 0.9,
});

test('a pool save is one PATCH that carries the members with the settings', async () => {
  const row = JSON.parse(readFileSync(new URL('./fixtures/pools.json', import.meta.url), 'utf8')).body.pools[0];
  const calls = stub(() => answer(200, row));
  await new HttpApi().updatePool('main', poolSave(row.label));
  assert.deepEqual(calls.map((c) => [c.init.method, c.url]), [['PATCH', '/admin/pools/main']], 'a second write could fail after the first landed');
  const body = JSON.parse(calls[0].init.body);
  assert.deepEqual(body.members, [
    { account_id: 'business', budget_daily_usd: null, budget_monthly_usd: null, priority: 0 },
    { account_id: 'direct', budget_daily_usd: 12.5, budget_monthly_usd: 50, priority: 1 },
  ], 'a member without a budget sends null, which clears a stored one');
  assert.deepEqual([body.load_shedding, body.five_hour_ceiling, body.weekly_ceiling], [false, 0.8, 0.9], 'the switch is sent off as it is sent on');
  const refused = ['burn_window_seconds', 'pace_floor', 'select_below_utilization', 'shed_ahead_of_pace', 'shed_share', 'source_dwell_seconds', 'window_ceiling'];
  assert.deepEqual(refused.filter((f) => f in body), [], 'the body names no field the route refuses as unknown');
  assert.equal(body.cache_diagnostics, false, 'the switch is sent off as it is sent on');
  assert.deepEqual(
    [body.affinity_mode, body.subagent_placement, body.session_placement, body.min_shared_prefix_tokens],
    ['when_cached', 'with_main', 'by_warm_prefix', 30_000],
  );
});

test('a pool or key edit sends the revision it read as expected_revision, and none for a save that read no row', async () => {
  const pools = JSON.parse(readFileSync(new URL('./fixtures/pools.json', import.meta.url), 'utf8')).body.pools[0];
  const keys = JSON.parse(readFileSync(new URL('./fixtures/keys.json', import.meta.url), 'utf8')).body.keys[0];
  const calls = stub((url) => answer(200, url.startsWith('/admin/pools') ? pools : keys));
  const api = new HttpApi();
  await api.updatePool('main', poolSave(pools.label), pools.revision);
  await api.updateKey('desktop', keyEdit, keys.revision);
  await api.updatePool('main', poolSave(pools.label));
  const sent = calls.map((c) => JSON.parse(c.init.body).expected_revision);
  assert.deepEqual(sent, [pools.revision, keys.revision, undefined], "the row's token as the answer carried it");
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

test("the replay is one POST that decodes its counts, and a held writer's 503 reaches no toast, since the line draws it", async () => {
  let failures = 0;
  setHooks({ onFailure: () => { failures += 1; }, onForbidden: () => undefined, onUnauthorized: () => undefined });
  const body = (name) => JSON.parse(readFileSync(new URL(`./fixtures/${name}.json`, import.meta.url), 'utf8')).body;
  const calls = stub(() => answer(200, body('replay')));
  assert.deepEqual(await new HttpApi().replayParked(), { landed: 0, remaining: 0 });
  assert.deepEqual(calls.map((c) => [c.init.method, c.url, c.init.body]), [['POST', '/admin/ledger/parked/replay', undefined]]);
  stub(() => answer(503, body('replay_held')));
  await assert.rejects(new HttpApi().replayParked(), (e) => e instanceof ApiError && e.status === 503 && e.message === 'the ledger writer is held, try again');
  assert.equal(failures, 0, 'the dashboard line draws the refusal in its own slot');
});

test("a dialog's write reaches no toast when refused, since the dialog draws the refusal, while every other write still does", async () => {
  let failures = 0;
  let forbidden = 0;
  let unauthorized = 0;
  setHooks({ onFailure: () => { failures += 1; }, onForbidden: () => { forbidden += 1; }, onUnauthorized: () => { unauthorized += 1; } });
  const api = new HttpApi();
  const dialogs = [
    () => api.createAccount({ id: 'work', kind: 'oauth', label: 'work', secret: null }),
    () => api.createKey({ ...keyEdit, expiresAt: null }),
    () => api.createPool(poolSave('Spare')),
    () => api.readLimitResets('work'),
    () => api.renameAccount('work', 'Work', 1),
    () => api.updateAccount('work', { canceled: false, subscriptionEndsAt: null }, 1),
    () => api.updateKey('desktop', keyEdit, 1),
    () => api.updatePool('main', poolSave('Main'), 1),
    () => api.useLimitReset('work', 'rg-work-1'),
  ];
  for (const status of [400, 403, 409, 500]) {
    stub(() => answer(status, { error: 'refused' }));
    for (const write of dialogs) await assert.rejects(write(), (e) => e instanceof ApiError && e.status === status && e.message === 'refused');
  }
  stub(() => Promise.reject(new TypeError('fetch failed')));
  for (const write of dialogs) await assert.rejects(write(), (e) => e instanceof ApiError && e.status === 0);
  assert.deepEqual([failures, forbidden], [0, 0], 'the dialog shows the refusal once, in its own callout or footer');
  stub(() => answer(401, { error: 'no admin session' }));
  await assert.rejects(dialogs[0]());
  assert.equal(unauthorized, 1, 'a lapsed session still returns the console to the login');
  stub(() => answer(500, { error: 'the store is unavailable' }));
  const others = [
    () => api.clearCooldown('work', 'account'),
    () => api.deleteAccount('work'),
    () => api.pauseAccount('work'),
    () => api.resumeAccount('work'),
    () => api.revokeKey('desktop'),
    () => api.rotateKey('desktop'),
  ];
  for (const write of others) await assert.rejects(write());
  assert.equal(failures, others.length, 'a write no dialog draws keeps the toast');
});

test('a pairing start posts the account id or the new account quietly', async () => {
  let failures = 0;
  setHooks({ onFailure: () => { failures += 1; }, onForbidden: () => undefined, onUnauthorized: () => undefined });
  const body = JSON.parse(readFileSync(new URL('./fixtures/pairing_started.json', import.meta.url), 'utf8')).body;
  const calls = stub(() => answer(200, body));
  const api = new HttpApi();
  await api.startPairing({ accountId: 'primary' });
  await api.startPairing({ id: 'fresh', label: 'Fresh' });
  assert.deepEqual(
    calls.map((c) => [c.init.method, c.url, JSON.parse(c.init.body)]),
    [
      ['POST', '/admin/oauth/sessions', { account_id: 'primary', login: 'claude_ai' }],
      ['POST', '/admin/oauth/sessions', { login: 'claude_ai', new: { id: 'fresh', label: 'Fresh' } }],
    ],
    'an existing row travels as account_id, a row to create as new',
  );
  stub(() => answer(409, { error: 'an account with this id exists' }));
  await assert.rejects(api.startPairing({ id: 'primary', label: 'Primary' }), (e) => e instanceof ApiError && e.status === 409 && e.message === 'an account with this id exists');
  assert.equal(failures, 0, 'the pairing dialog draws the refusal in its own callout, so no toast');
});

test('the attention read is one GET, and its failures reach no toast, since the line draws its own', async () => {
  let failures = 0;
  let unauthorized = 0;
  setHooks({ onFailure: () => { failures += 1; }, onForbidden: () => undefined, onUnauthorized: () => { unauthorized += 1; } });
  const body = JSON.parse(readFileSync(new URL('./fixtures/attention.json', import.meta.url), 'utf8')).body;
  const calls = stub(() => answer(200, body));
  const items = await new HttpApi().getAttention();
  assert.deepEqual(calls.map((c) => [c.init.method, c.url]), [['GET', '/admin/attention']]);
  assert.ok(items.some((i) => i.kind === 'model_unpriced'), "the capture's unpriced model among them");
  assert.deepEqual(
    items.filter((i) => ['wall_reached', 'destination_spent', 'shed_no_runway'].includes(i.kind)).map((i) => [i.kind, i.subject.id, i.link]),
    [
      ['wall_reached', 'business', { id: 'business', page: 'accounts' }],
      ['destination_spent', 'business', { id: 'business', page: 'accounts' }],
      ['shed_no_runway', 'main', { id: 'main', page: 'pools' }],
    ],
    "the capture's three routing conditions among them, each with its link",
  );
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
