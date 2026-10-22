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
 * read as one kind there. A member's windows and its burns are keyed by window name, data rather than schema, so the
 * comparison holds each entry to the fixture's first.
 */
const factValue = (f) => ({ ...f, value: typeof f.value === 'number' || typeof f.value === 'string' ? 'number or string' : f.value });
/** The merged page split by row type, so each of the two shapes meets the fixture's own rows of that type. */
const byType = (page) => ({ decisions: page.rows.filter((r) => r.type === 'decision').map((r) => ({ ...r, fact: r.fact.map(factValue) })), events: page.rows.filter((r) => r.type === 'event') });
const recordShape = (r) => r && { ...r, candidates: r.candidates.map((c) => ({ ...c, burn: Object.values(c.burn), windows: Object.values(c.windows) })), walk: r.walk.map((w) => ({ ...w, facts: w.facts.map((f) => ({ ...f, value: 'any' })) })) };

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
  analytics_breakdown_accounts: { decode: (b) => b.rows.map(d.decodeAnalyticsGroup), mock: (api) => api.getAnalyticsBreakdown({ ...query, groupBy: 'account' }) },
  analytics_breakdown_speed: { decode: (b) => b.rows.map(d.decodeAnalyticsGroup), mock: (api) => api.getAnalyticsBreakdown({ ...query, groupBy: 'speed' }) },
  analytics_budgets: { decode: (b) => b.members.map(d.decodeMemberBudget), mock: (api) => api.analyticsBudgets() },
  analytics_burn: { decode: (b) => d.decodeBurn(b), mock: (api) => api.getAnalyticsBurn(query, 'account') },
  analytics_guardrails: { decode: (b) => d.decodeGuardrailCounts(b), mock: (api) => api.getAnalyticsGuardrails(query) },
  analytics_placements: { decode: (b) => b.rows.map(d.decodePlacementRow), mock: (api) => api.analyticsPlacements(query) },
  analytics_reconciliations: { decode: (b) => d.decodeReconciliations(b), mock: (api) => api.getAnalyticsReconciliations() },
  // Its moves by reason and fit are a map of maps keyed by data on both levels, which the mock's consistency test holds
  // to the moves.
  analytics_rewrites: { decode: (b) => d.decodeRewrites(b), mock: (api) => api.getAnalyticsRewrites(query), opaque: (path) => (path === '.movesByReasonAndFit' ? ['object'] : false) },
  analytics_series: { decode: (b) => b.points.filter((p) => p.groups.length).map(d.decodeAnalyticsPoint), mock: async (api) => (await api.getAnalyticsSeries(query)).filter((p) => p.groups.length) },
  analytics_series_accounts: { decode: (b) => b.points.filter((p) => p.groups.length).map(d.decodeAnalyticsPoint), mock: async (api) => (await api.getAnalyticsSeries({ ...query, groupBy: 'account' })).filter((p) => p.groups.length) },
  attention: { decode: (b) => b.items.map(d.decodeAttention), mock: (api) => api.getAttention() },
  // The route answers a null refusal code on a day with no refusal, so a null there is the wire's own.
  dashboard: { decode: (b) => d.decodeDashboard(b), mock: (api) => api.getOverview(), opaque: (path) => (path === '.topRefusalCode' ? ['string', 'null'] : false) },
  // The story's static placement, whose `raw` is the record the mock answers where the fixture's is the route's body.
  decision: { decode: (b) => recordShape(d.decodeDecisionRecord(b)), mock: async (api) => recordShape(await api.getDecision('req_b2bed817-a58', 1)), opaque: (path) => path === '.settings' || path === '.raw' },
  // From the preset with a pool, so the answer carries the stamp and an attachment as the captured create does.
  guardrail_created: { decode: (b) => d.decodeGuardrail(b), mock: (api) => api.createGuardrail({ description: '', enabled: true, mode: 'observe', name: 'sync', pools: ['all'], preset: 'subagent-status-summary' }) },
  guardrail_presets: { decode: (b) => b.presets.map(d.decodeGuardrailPreset), mock: (api) => api.listGuardrailPresets() },
  guardrail_show: { decode: (b) => d.decodeGuardrail(b), mock: (api) => api.getGuardrail('subagent-status-summary') },
  guardrail_test: { decode: (b) => d.decodeGuardrailTest(b), mock: (api) => api.testGuardrail({ guardrail: 'subagent-status-summary', limit: 50, poolId: 'all' }) },
  guardrails: { decode: (b) => b.guardrails.map(d.decodeGuardrail), mock: (api) => api.listGuardrails() },
  keepalive_overview: { decode: (b) => d.decodeKeepaliveOverview(b), mock: (api) => api.getKeepaliveOverview() },
  keepalive_recipes: { decode: (b) => b.recipes.map(d.decodeRecipe), mock: (api) => api.listKeepaliveRecipes() },
  key_created: { decode: (b) => ({ key: d.decodeKey(b), secret: b.secret }), mock: (api) => api.createKey({ bodyCaptureHours: null, enabled: true, expiresInDays: null, keepaliveOptOut: false, longContextTwins: true, modelRules: [], modelRulesMode: 'extend', name: 'sync', notes: null, poolId: 'all' }) },
  key_models: { decode: (b) => d.decodeKeyModels(b), mock: (api) => api.getKeyModels('laptop') },
  keys: { decode: (b) => b.keys.map(d.decodeKey), mock: (api) => api.listKeys() },
  pairing_started: { decode: (b) => d.decodePairingSession(b), mock: (api) => api.startPairing({ accountId: 'legacy' }) },
  pools: { decode: (b) => b.pools.map(d.decodePool), mock: (api) => api.listPools() },
  replay: { decode: (b) => d.decodeReplay(b), mock: (api) => api.replayParked() },
  request: { decode: (b) => ({ events: b.events.map(d.decodeRequestEvent), request: d.decodeRequest(b.request), streamCut: d.decodeStreamCut(b.stream_cut) }), mock: async (api) => { for (const r of (await api.listRequests({ limit: 500 })).requests) { const detail = await api.getRequest(r.id); if (detail.events.length) return detail; } throw new Error('no mock request carries an event'); } },
  // `.request`/`.response` are CapturedBodies' own declared-`unknown` fields (decode.ts's decodeBodies passes
  // them through raw), the same class of value OPAQUE covers for `.detail`. A stream's capture is the text of its
  // events, which only a response is, so the string is admitted on `.response` alone.
  request_bodies: { decode: (b) => d.decodeBodies(b), mock: async (api) => { const page = await api.listRequests(); return api.getCapturedBodies(page.requests.find((r) => r.capturedBodies).id); }, opaque: (path) => (path === '.request' ? ['object', 'null'] : path === '.response' ? ['object', 'null', 'string'] : false) },
  requests: { decode: (b) => d.decodeRequestPage(b), mock: (api) => api.listRequests() },
  routing_log: { decode: (b) => byType(d.decodeRoutingLogPage(b)), mock: async (api) => byType(await api.listRoutingLog()) },
  session: { expect: (b) => Object.fromEntries(Object.keys(b).map((k) => [camel(k), 'any'])), mock: async (api) => { await api.login('admin'); return api.getSession(); } },
  sessions: { decode: (b) => b.sessions.map(d.decodeSession), mock: (api) => api.listSessions() },
  settings: { decode: (b) => d.decodeSettings(b), mock: (api) => api.getSettings() },
};
/** Fixtures no decoder and no `Api` method reads: captured for their route's shape, or read by a test of their own. */
const UNCONSUMED = ['bound_refusals', 'oauth_session', 'pool_traffic', 'pricing', 'replay_held', 'sessions_costs', 'system', 'test_tables'];

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
  // A condition of another subject than `message` carries no selector.
  guardrail_presets: { '[].conditions[].selector': [...d.MESSAGE_SELECTOR_VALUES, undefined] },
  guardrail_test: { '.requests[].verdict': d.TEST_VERDICT_VALUES },
  guardrails: { '[].conditions[].operator': d.CONDITION_OPERATOR_VALUES, '[].conditions[].subject': d.CONDITION_SUBJECT_VALUES, '[].mode': d.GUARDRAIL_MODE_VALUES },
  keepalive_recipes: { '[].agentState': d.AGENT_STATE_VALUES, '[].cause': d.RECIPE_CAUSE_VALUES, '[].parkKind': [...d.PARK_KIND_VALUES, null], '[].state': d.RECIPE_STATE_VALUES },
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
    // A client that sends no class header, an SDK runner among them, reads null.
    '.requests[].requestClass': [...REQUEST_CLASS_VALUES, null],
  },
  sessions: { '[].keepalive.cause': [...d.RECIPE_CAUSE_VALUES, null], '[].keepalive.state': [...d.AGENT_STATE_VALUES, null] },
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

/**
 * The pool as the dialog would send it back unchanged, so a refusal below is the one field the case edits. The listed
 * pool's guardrails are refs, and the dialog sends that list only when its group changed.
 */
const poolInputOf = ({ guardrails: _, ...p }) => ({ ...p, members: p.members.map((m) => ({ accountId: m.accountId, priority: m.priority })) });

test('the mock refuses the affinity and the keepalive values the route refuses', async () => {
  const api = await admin();
  const all = (await api.listPools()).find((p) => p.id === 'all');
  const refused = async (input, message) => assert.rejects(api.updatePool('all', input), (e) => e.status === 400 && e.message === message);
  await refused({ ...poolInputOf(all), affinityTtlSeconds: 30, keepalive: { ...all.keepalive, enabled: false } }, 'affinity_ttl_seconds is 0 or at least 60');
  await refused({ ...poolInputOf(all), keepalive: { ...all.keepalive, maxIdleSeconds: 31_536_001 } }, 'keepalive.max_idle_seconds must be between 0 and 31536000, not 31536001');
  await refused({ ...poolInputOf(all), keepalive: { ...all.keepalive, maxParkedSeconds: 59 } }, 'keepalive.max_parked_seconds must be between 60 and 604800, not 59');
  await refused({ ...poolInputOf(all), keepalive: { ...all.keepalive, scheduledHorizonSeconds: 604_801 } }, 'keepalive.scheduled_horizon_seconds must be between 60 and 604800, not 604801');
  await refused({ ...poolInputOf(all), affinityTtlSeconds: 120 }, 'keepalive.enabled requires affinity_ttl_seconds of at least 300: a probe follows the session pin, and a pin shorter than the shortest cache TTL cannot keep the session on the account it warmed');
  assert.equal((await api.listPools()).find((p) => p.id === 'all').affinityTtlSeconds, all.affinityTtlSeconds, 'a refused save stores nothing');
  const saved = await api.updatePool('all', { ...poolInputOf(all), windowCeiling: 0.8, keepalive: { ...all.keepalive, enabled: false }, returnToPlan: 'when_cold', slowDown: true, slowDownStretchSeconds: 2_400 });
  assert.deepEqual([saved.windowCeiling, saved.keepalive.enabled], [0.8, false]);
  // The save goes back to the route as the body the dialog sends, and the route's answer through the decoder.
  const read = d.decodePool({ ...fixture('pools').pools[0], ...d.encodePool({ ...poolInputOf(saved), status: 'active' }) });
  assert.deepEqual([read.slowDown, read.slowDownStretchSeconds, read.returnToPlan], [true, 2_400, 'when_cold'], 'the slow-down, its stretch and the return mode read back as saved');
});

test('a null ceiling is the break-even, which the mock admits as the route does', async () => {
  const api = await admin();
  const all = (await api.listPools()).find((p) => p.id === 'all');
  const saved = await api.updatePool('all', { ...poolInputOf(all), keepalive: { ...all.keepalive, maxParkedSeconds: 7_200, scheduledHorizonSeconds: null } });
  assert.deepEqual([saved.keepalive.maxParkedSeconds, saved.keepalive.scheduledHorizonSeconds], [7_200, null]);
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
  assert.deepEqual([primary.email, primary.status, primary.credential.refreshTokenExpiresAt, primary.pool !== null], ['operator@example.com', 'unpaired', null, true], 'a disconnect drops the credential and leaves the identity and the membership');
});
