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

/**
 * One row per fixture: how the decoder reads it, and how the mock answers the same route. A list route
 * compares every mock row against the fixture's first; a singleton fixture whose `Api` method lists (the one
 * account, the one quota snapshot) picks the story's `business` row so one shape meets one; a route whose
 * answer depends on which story row it asks for walks the story to one that has rows, since an empty mock
 * list where the fixture has rows is a finding; `expect` stands in for a route with no decoder.
 */
const SYNC = {
  account: { decode: (b) => d.decodeAccount(b), mock: async (api) => (await api.listAccounts()).find((a) => a.id === 'business') },
  account_quota: { decode: (b) => d.decodeQuotaHistory(b), mock: (api) => api.getQuotaHistory('business') },
  account_sync: { expect: (b) => Object.fromEntries(Object.keys(b).map((k) => [camel(k), 'any'])), mock: (api) => api.syncCatalog('business') },
  account_usage: { decode: (b) => d.decodeSnapshot('business', b), mock: async (api) => (await api.listQuotaSnapshots()).find((s) => s.accountId === 'business') },
  accounts: { decode: (b) => b.accounts.map(d.decodeAccount), mock: (api) => api.listAccounts() },
  alert_deliveries: { decode: (b) => b.deliveries.map(d.decodeDelivery), mock: async (api) => { for (const e of await api.listAlertEvents()) { const rows = await api.listAlertDeliveries(e.id); if (rows.length) return rows; } throw new Error('no mock alert event carries a delivery'); } },
  alert_events: { decode: (b) => b.events.map(d.decodeAlertEvent), mock: (api) => api.listAlertEvents() },
  alert_rules: { decode: (b) => b.rules.map(d.decodeAlertRule), mock: (api) => api.listAlertRules() },
  analytics_accounts: { decode: (b) => d.decodeAccountRollups(b), mock: (api) => api.listAccountRollups(day, now) },
  analytics_breakdown: { decode: (b) => b.rows.map(d.decodeAnalyticsGroup), mock: (api) => api.getAnalyticsBreakdown(query) },
  analytics_series: { decode: (b) => b.points.filter((p) => p.groups.length).map(d.decodeAnalyticsPoint), mock: async (api) => (await api.getAnalyticsSeries(query)).filter((p) => p.groups.length) },
  dashboard: { decode: (b) => d.decodeDashboard(b), mock: (api) => api.getOverview() },
  keepalive_overview: { decode: (b) => d.decodeKeepaliveOverview(b), mock: (api) => api.getKeepaliveOverview() },
  keepalive_recipes: { decode: (b) => b.recipes.map(d.decodeRecipe), mock: (api) => api.listKeepaliveRecipes() },
  key_created: { decode: (b) => ({ key: d.decodeKey(b), secret: b.secret }), mock: (api) => api.createKey({ allowedModels: null, bodyCaptureHours: null, expiresInDays: null, keepaliveOptOut: false, name: 'sync', notes: null, poolId: 'all' }) },
  keys: { decode: (b) => b.keys.map(d.decodeKey), mock: (api) => api.listKeys() },
  live_messages: { decode: (b) => ({ ...d.decodeLiveSnapshot(b.snapshot.snapshot), recent: [d.decodeLiveRequest(b.deltas[0].delta.request)] }), mock: (api) => { let snapshot; const stop = api.subscribeLive((e) => { if (e.kind === 'snapshot') snapshot = e.snapshot; }); stop(); return snapshot; } },
  pairing_started: { decode: (b) => d.decodePairingSession(b), mock: (api) => api.startPairing('team') },
  pools: { decode: (b) => b.pools.map(d.decodePool), mock: (api) => api.listPools() },
  pricing_import: { decode: (b) => d.decodePriceImport(b), mock: (api) => api.previewPriceImport('https://example.invalid/prices.json') },
  request: { decode: (b) => ({ events: b.events.map(d.decodeRequestEvent), request: d.decodeRequest(b.request) }), mock: async (api) => { for (const r of (await api.listRequests({ limit: 500 })).requests) { const events = await api.listRequestEvents(r.id); if (events.length) return { events, request: await api.getRequest(r.id) }; } throw new Error('no mock request carries an event'); } },
  // `.request`/`.response` are CapturedBodies' own declared-`unknown` fields (decode.ts's decodeBodies passes
  // them through raw), the same class of value OPAQUE covers for `.detail`.
  request_bodies: { decode: (b) => d.decodeBodies(b), mock: async (api) => { const page = await api.listRequests(); return api.getCapturedBodies(page.requests.find((r) => r.capturedBodies).id); }, opaque: (path) => path === '.request' || path === '.response' },
  requests: { decode: (b) => d.decodeRequestPage(b), mock: (api) => api.listRequests() },
  session: { expect: (b) => Object.fromEntries(Object.keys(b).map((k) => [camel(k), 'any'])), mock: async (api) => { await api.login('admin'); return api.getSession(); } },
  sessions: { decode: (b) => b.sessions.map(d.decodeSession), mock: (api) => api.listSessions() },
  settings: { decode: (b) => d.decodeSettings(b), mock: (api) => api.getSettings() },
};
/** Fixtures captured for their route's shape alone: no decoder and no `Api` method reads them today. */
const UNCONSUMED = ['account_models', 'live_past', 'pool_traffic', 'pricing', 'sessions_costs', 'system'];

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
    const api = new d.MockApi();
    const expected = row.expect ? row.expect(fixture(name)) : row.decode(fixture(name));
    const answered = await row.mock(api);
    const findings = row.expect ? Object.keys(expected).filter((k) => !(k in (answered ?? {}))).map((k) => `.${k}: only in the fixture`) : same(shapeOf(expected), answered, '', 0, row.opaque);
    assert.deepEqual(findings, [], `${name} drifts from the decoded fixture`);
  });
}
