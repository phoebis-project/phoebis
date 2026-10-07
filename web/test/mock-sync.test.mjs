import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import { test } from 'node:test';
import { render } from '../scripts/sync-mock.mjs';
import * as d from './dist/api.js';
import { same, shapeOf } from './shape.mjs';

const fixturesDir = new URL('./fixtures/', import.meta.url);
const fixture = (name) => JSON.parse(readFileSync(new URL(`./fixtures/${name}.json`, import.meta.url), 'utf8')).body;
const names = readdirSync(fixturesDir).filter((f) => f.endsWith('.json')).map((f) => f.slice(0, -5)).sort();

const camel = (s) => s.replace(/_([a-z0-9])/g, (_, c) => c.toUpperCase());
const day = new Date(Date.now() - 86_400_000).toISOString();
const now = new Date().toISOString();
const query = { from: day, groupBy: 'key', to: now };
// Signed in as the admin: several rows write, and the gate refuses those writes to anyone else.
const admin = async () => {
  const api = new d.MockApi();
  await api.login('admin');
  return api;
};
/**
 * A summary fact's value is a number or a word by design and a walk fact's any value the engine read, so both sides
 * read as one kind there; a member's windows are keyed by window name, data rather than schema, so the comparison
 * holds each entry to the fixture's first.
 */
const factValue = (f) => ({ ...f, value: typeof f.value === 'number' || typeof f.value === 'string' ? 'number or string' : f.value });
/** The merged page split by row type, so each of the two shapes meets the fixture's own rows of that type. */
const byType = (page) => ({ decisions: page.rows.filter((r) => r.type === 'decision').map((r) => ({ ...r, fact: r.fact.map(factValue) })), events: page.rows.filter((r) => r.type === 'event') });
const recordShape = (r) => r && { ...r, candidates: r.candidates.map((c) => ({ ...c, windows: Object.values(c.windows) })), walk: r.walk.map((w) => ({ ...w, facts: w.facts.map((f) => ({ ...f, value: 'any' })) })) };

/**
 * One row per fixture: how the decoder reads it, and how the mock answers the same route. A list route
 * compares every mock row against the fixture's first; a singleton fixture whose `Api` method lists (the one
 * account, the one quota snapshot) picks the story's `primary` row so one shape meets one; a route whose
 * answer depends on which story row it asks for walks the story to one that has rows, since an empty mock
 * list where the fixture has rows is a finding; `expect` stands in for a route with no decoder.
 */
const SYNC = {
  account: { decode: (b) => d.decodeAccount(b), mock: async (api) => (await api.listAccounts()).find((a) => a.id === 'primary') },
  account_catalog: { decode: (b) => d.decodeAccountCatalog(b), mock: (api) => api.getAccountCatalog('primary') },
  account_quota: { decode: (b) => d.decodeQuotaHistory(b), mock: (api) => api.getQuotaHistory('primary') },
  account_usage: { decode: (b) => d.decodeSnapshot('business', b), mock: async (api) => (await api.listAccounts()).find((a) => a.id === 'primary').usage },
  accounts: { decode: (b) => b.accounts.map(d.decodeAccount), mock: (api) => api.listAccounts() },
  analytics_accounts: { decode: (b) => d.decodeAccountRollups(b), mock: (api) => api.listAccountRollups(day, now) },
  analytics_breakdown: { decode: (b) => b.rows.map(d.decodeAnalyticsGroup), mock: (api) => api.getAnalyticsBreakdown(query) },
  analytics_breakdown_speed: { decode: (b) => b.rows.map(d.decodeAnalyticsGroup), mock: (api) => api.getAnalyticsBreakdown({ ...query, groupBy: 'speed' }) },
  analytics_budgets: { decode: (b) => b.members.map(d.decodeMemberBudget), mock: (api) => api.analyticsBudgets() },
  analytics_burn: { decode: (b) => d.decodeBurn(b), mock: (api) => api.getAnalyticsBurn(query, 'account') },
  analytics_placements: { decode: (b) => b.rows.map(d.decodePlacementRow), mock: (api) => api.analyticsPlacements(query) },
  analytics_rewrites: { decode: (b) => d.decodeRewrites(b), mock: (api) => api.getAnalyticsRewrites(query) },
  analytics_series: { decode: (b) => b.points.filter((p) => p.groups.length).map(d.decodeAnalyticsPoint), mock: async (api) => (await api.getAnalyticsSeries(query)).filter((p) => p.groups.length) },
  attention: { decode: (b) => b.items.map(d.decodeAttention), mock: (api) => api.getAttention() },
  dashboard: { decode: (b) => d.decodeDashboard(b), mock: (api) => api.getOverview() },
  // The story's static placement, whose `raw` is the record the mock answers where the fixture's is the route's body.
  decision: { decode: (b) => recordShape(d.decodeDecisionRecord(b)), mock: async (api) => recordShape(await api.getDecision('req_b2bed817-a58', 1)), opaque: (path) => path === '.settings' || path === '.raw' },
  keepalive_overview: { decode: (b) => d.decodeKeepaliveOverview(b), mock: (api) => api.getKeepaliveOverview() },
  keepalive_recipes: { decode: (b) => b.recipes.map(d.decodeRecipe), mock: (api) => api.listKeepaliveRecipes() },
  key_created: { decode: (b) => ({ key: d.decodeKey(b), secret: b.secret }), mock: (api) => api.createKey({ bodyCaptureHours: null, enabled: true, expiresInDays: null, keepaliveOptOut: false, longContextTwins: true, modelRules: [], modelRulesMode: 'extend', name: 'sync', notes: null, poolId: 'all' }) },
  key_models: { decode: (b) => d.decodeKeyModels(b), mock: (api) => api.getKeyModels('laptop') },
  keys: { decode: (b) => b.keys.map(d.decodeKey), mock: (api) => api.listKeys() },
  live_messages: { decode: (b) => ({ ...d.decodeLiveSnapshot(b.snapshot.snapshot), recent: [d.decodeLiveRequest(b.deltas[0].delta.request)] }), mock: (api) => { let snapshot; const stop = api.subscribeLive((e) => { if (e.kind === 'snapshot') snapshot = e.snapshot; }); stop(); return snapshot; } },
  models_state: { decode: (b) => d.decodeModelsState(b), mock: (api) => api.getModelsState() },
  pairing_started: { decode: (b) => d.decodePairingSession(b), mock: (api) => api.startPairing('legacy') },
  pools: { decode: (b) => b.pools.map(d.decodePool), mock: (api) => api.listPools() },
  request: { decode: (b) => ({ events: b.events.map(d.decodeRequestEvent), request: d.decodeRequest(b.request) }), mock: async (api) => { for (const r of (await api.listRequests({ limit: 500 })).requests) { const detail = await api.getRequest(r.id); if (detail.events.length) return detail; } throw new Error('no mock request carries an event'); } },
  // `.request`/`.response` are CapturedBodies' own declared-`unknown` fields (decode.ts's decodeBodies passes
  // them through raw), the same class of value OPAQUE covers for `.detail`.
  request_bodies: { decode: (b) => d.decodeBodies(b), mock: async (api) => { const page = await api.listRequests(); return api.getCapturedBodies(page.requests.find((r) => r.capturedBodies).id); }, opaque: (path) => path === '.request' || path === '.response' },
  requests: { decode: (b) => d.decodeRequestPage(b), mock: (api) => api.listRequests() },
  routing_log: { decode: (b) => byType(d.decodeRoutingLogPage(b)), mock: async (api) => byType(await api.listRoutingLog()) },
  session: { expect: (b) => Object.fromEntries(Object.keys(b).map((k) => [camel(k), 'any'])), mock: async (api) => { await api.login('admin'); return api.getSession(); } },
  sessions: { decode: (b) => b.sessions.map(d.decodeSession), mock: (api) => api.listSessions() },
  settings: { decode: (b) => d.decodeSettings(b), mock: (api) => api.getSettings() },
};
/** Fixtures no decoder and no `Api` method reads: captured for their route's shape, or read by a test of their own. */
const UNCONSUMED = ['bound_refusals', 'live_past', 'pool_traffic', 'pricing', 'sessions_costs', 'system', 'test_tables'];

test('the generated fixtures module is a fresh render', () => {
  const committed = readFileSync(new URL('../src/mock/generated/fixtures.ts', import.meta.url), 'utf8');
  assert.equal(committed, render(), 'web/src/mock/generated/fixtures.ts is stale; run npm run sync-mock');
});

test('every fixture has a row in the sync table or is listed as unconsumed', () => {
  for (const name of names) assert.ok(name in SYNC || UNCONSUMED.includes(name), `${name}.json: add a decoder, a mock method, and a SYNC row, or list it under UNCONSUMED`);
  for (const name of [...Object.keys(SYNC), ...UNCONSUMED]) assert.ok(names.includes(name), `${name} has no fixture`);
});

for (const [name, row] of Object.entries(SYNC)) {
  test(`the mock answers ${name} in the shape the decoder reads`, async () => {
    const api = await admin();
    const expected = row.expect ? row.expect(fixture(name)) : row.decode(fixture(name));
    const answered = await row.mock(api);
    const findings = row.expect ? Object.keys(expected).filter((k) => !(k in (answered ?? {}))).map((k) => `.${k}: only in the fixture`) : same(shapeOf(expected), answered, '', 0, row.opaque);
    assert.deepEqual(findings, [], `${name} drifts from the decoded fixture`);
  });
}

/**
 * The values of `x-claude-code-request-class`, as Claude Code's gateway compatibility guide lists them
 * (https://code.claude.com/docs/en/llm-gateway-protocol, Request headers). The console keeps any class as sent, so
 * the list holds only the mock to the vocabulary.
 */
const REQUEST_CLASS_VALUES = ['auxiliary', 'compaction', 'main', 'subagent', 'workflow'];

/**
 * Where a decoded member is a closed string set, the mock's values must be members: an unknown value falls
 * through every label table on a page, which the shape comparison above cannot see. Numbers and dates stay
 * unchecked. Every row of the mock's answer is walked, not only the first.
 */
const CLOSED = {
  accounts: { '[].kind': d.ACCOUNT_KIND_VALUES },
  analytics_budgets: { '[].budgetPeriod': d.BUDGET_PERIOD_VALUES, '[].kind': d.ACCOUNT_KIND_VALUES },
  analytics_placements: { '[].rung': d.PLACEMENT_RUNG_VALUES },
  attention: { '[].kind': d.ATTENTION_KIND_VALUES, '[].link.page': [...d.ATTENTION_PAGE_VALUES, undefined], '[].subject.type': d.ATTENTION_SUBJECT_VALUES },
  keepalive_recipes: { '[].reason': [...d.RECIPE_REASON_VALUES, null], '[].state': d.RECIPE_STATE_VALUES },
  live_messages: { '.recent[].attempts[].outcome': d.ATTEMPT_OUTCOME_VALUES, '.recent[].kind': d.REQUEST_KIND_VALUES },
  pools: {
    '[].members[].eligible': d.ELIGIBILITY_VALUES,
    '[].members[].spends[].period': d.BUDGET_PERIOD_VALUES,
    '[].overagePolicy': d.OVERAGE_POLICY_VALUES,
    '[].status': d.POOL_STATUS_VALUES,
    '[].strategy': d.POOL_STRATEGY_VALUES,
  },
  requests: {
    '.requests[].attempts[].outcome': d.ATTEMPT_OUTCOME_VALUES,
    '.requests[].attempts[].selection.moved': [...d.PIN_MIGRATION_REASON_VALUES, null],
    '.requests[].attempts[].selection.skipped[].obstacle': d.SKIP_OBSTACLE_VALUES,
    '.requests[].kind': d.REQUEST_KIND_VALUES,
    '.requests[].outcome': d.REQUEST_OUTCOME_VALUES,
    '.requests[].requestClass': REQUEST_CLASS_VALUES,
  },
  sessions: { '[].keepalive.state': [...d.AGENT_STATE_VALUES, null] },
};

/** Every value `path` names in `value`, beside the concrete path it sits at; `[]` fans out over a list's rows. */
function members(value, path, at = '') {
  if (path === '') return [[at, value]];
  const step = /^(\[\]|\.[A-Za-z0-9_]+)/.exec(path)[0];
  const rest = path.slice(step.length);
  if (step !== '[]') return members(value?.[step.slice(1)], rest, `${at}${step}`);
  // A member that is not a list where the path expects one is itself the finding, so it is reported as a value.
  return Array.isArray(value) ? value.flatMap((row, i) => members(row, rest, `${at}[${i}]`)) : [[at, value]];
}

for (const [name, paths] of Object.entries(CLOSED)) {
  test(`the mock answers ${name} with values inside the decoder's closed sets`, async () => {
    const answered = await SYNC[name].mock(await admin());
    for (const [path, set] of Object.entries(paths)) {
      const found = members(answered, path);
      // An answer with nothing at the path would pass without checking a single value.
      assert.ok(found.length > 0, `${name}${path}: the mock's answer holds no value there`);
      const outside = found.filter(([, v]) => !set.includes(v)).map(([at, v]) => `${at}: ${JSON.stringify(v)}`);
      assert.deepEqual(outside, [], `${name}${path} holds values outside ${JSON.stringify(set)}`);
    }
  });
}

/** The pool as the dialog would send it back unchanged, so a refusal below is the one field the case edits. */
const poolInputOf = (p) => ({ ...p, members: p.members.map((m) => ({ accountId: m.accountId, priority: m.priority })) });

test('the mock refuses the affinity and the keepalive values the route refuses', async () => {
  const api = await admin();
  const all = (await api.listPools()).find((p) => p.id === 'all');
  const refused = async (input, message) => assert.rejects(api.updatePool('all', input), (e) => e.status === 400 && e.message === message);
  await refused({ ...poolInputOf(all), affinityTtlSeconds: 30, keepalive: { ...all.keepalive, enabled: false } }, 'affinity_ttl_seconds is 0 or at least 60');
  await refused({ ...poolInputOf(all), keepalive: { ...all.keepalive, coldProbeLimit: 0 } }, 'keepalive.cold_probe_limit must be between 1 and 20, not 0');
  await refused({ ...poolInputOf(all), keepalive: { ...all.keepalive, leadSeconds5m: 300 } }, 'keepalive.lead_seconds_5m must be between 0 and 299, not 300');
  await refused({ ...poolInputOf(all), keepalive: { ...all.keepalive, retryMaxSeconds: 10, retrySeconds: 30 } }, 'keepalive.retry_max_seconds must be at least keepalive.retry_seconds');
  await refused({ ...poolInputOf(all), keepalive: { ...all.keepalive, costCapUsdPerDay: 0 } }, 'keepalive.probe_cost_cap_usd_per_day must be null or a positive number');
  await refused({ ...poolInputOf(all), affinityTtlSeconds: 120 }, 'keepalive.enabled requires affinity_ttl_seconds of at least 300: a probe follows the session pin, and a pin shorter than the shortest cache TTL cannot keep the session on the account it warmed');
  assert.equal((await api.listPools()).find((p) => p.id === 'all').affinityTtlSeconds, all.affinityTtlSeconds, 'a refused save stores nothing');
  const saved = await api.updatePool('all', { ...poolInputOf(all), selectBelowUtilization: 0.4, keepalive: { ...all.keepalive, enabled: false } });
  assert.deepEqual([saved.selectBelowUtilization, saved.keepalive.enabled], [0.4, false]);
});

test('a retry ceiling out of its range answers the range text, which the route checks before the ceiling-under-base rule', async () => {
  const api = await admin();
  const all = (await api.listPools()).find((p) => p.id === 'all');
  await assert.rejects(
    api.updatePool('all', { ...poolInputOf(all), keepalive: { ...all.keepalive, retryMaxSeconds: 0, retrySeconds: 30 } }),
    (e) => e.status === 400 && e.message === 'keepalive.retry_max_seconds must be between 1 and 604800, not 0',
  );
});

test('the mock stores a key edit\'s enabled flag and expiry and folds them into the status', async () => {
  const api = await admin();
  const batch = (await api.listKeys()).find((k) => k.id === 'batch');
  assert.deepEqual([batch.enabled, batch.status], [false, 'disabled']);
  const edit = { bodyCaptureHours: batch.bodyCaptureHours, enabled: false, keepaliveOptOut: batch.keepaliveOptOut, longContextTwins: batch.longContextTwins, modelRules: batch.modelRules, modelRulesMode: batch.modelRulesMode, name: 'nightly', notes: batch.notes, poolId: batch.poolId };
  const renamed = await api.updateKey('batch', { ...edit, expiresAt: undefined });
  assert.deepEqual([renamed.name, renamed.enabled, renamed.status, renamed.expiresAt], ['nightly', false, 'disabled', batch.expiresAt], 'an absent expiry leaves the stored one alone');
  const on = await api.updateKey('batch', { ...edit, enabled: true, expiresAt: '2099-01-01T00:00:00.000Z' });
  assert.deepEqual([on.enabled, on.status, on.expiresAt], [true, 'active', '2099-01-01T00:00:00.000Z']);
  const lapsed = await api.updateKey('batch', { ...edit, enabled: true, expiresAt: '2000-01-01T00:00:00.000Z' });
  assert.equal(lapsed.status, 'expired');
  assert.equal((await api.updateKey('batch', { ...edit, enabled: true, expiresAt: null })).expiresAt, null);
});

test('the mock deletes an account with its pool memberships, and a disconnect keeps the identity', async () => {
  const api = await admin();
  assert.ok((await api.listPools()).some((p) => p.members.some((m) => m.accountId === 'secondary')), 'the story puts secondary in a pool');
  await api.deleteAccount('secondary');
  assert.equal((await api.listAccounts()).some((a) => a.id === 'secondary'), false);
  assert.deepEqual((await api.listPools()).filter((p) => p.members.some((m) => m.accountId === 'secondary')).map((p) => p.id), [], 'the route removes every membership with the account');
  await assert.rejects(api.deleteAccount('secondary'), (e) => e.status === 404);
  await api.disconnectAccount('primary');
  const primary = (await api.listAccounts()).find((a) => a.id === 'primary');
  assert.deepEqual([primary.email, primary.status, primary.credential.refreshTokenExpiresAt, primary.pools.length > 0], ['operator@example.com', 'unpaired', null, true], 'a disconnect drops the credential and leaves the identity and the memberships');
});
