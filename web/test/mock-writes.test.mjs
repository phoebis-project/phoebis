import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

/** A fresh mock signed in the way the login screen signs a browser in. */
const as = async (role) => {
  const api = new d.MockApi();
  await api.login(role);
  return api;
};

/** The pool as the dialog would send it back unchanged, so a case edits only the fields it is about. */
const poolInputOf = (p) => ({ ...p, members: p.members.map((m) => ({ accountId: m.accountId, priority: m.priority })) });
const storyPool = async (api) => poolInputOf((await api.listPools()).find((p) => p.id === 'all'));
const keyInput = { bodyCaptureHours: null, enabled: true, expiresInDays: null, keepaliveOptOut: false, longContextTwins: true, modelRules: [], modelRulesMode: 'extend', name: 'fresh', notes: null, poolId: 'all' };
const ruleInputOf = (r) => ({ enabled: r.enabled, kind: r.kind, scope: r.scope, threshold: r.threshold, webhookUrl: r.webhookUrl, windowId: r.windowId });
const newRule = { enabled: true, kind: 'utilization_above', scope: { id: 'all', type: 'pool' }, threshold: 0.8, webhookUrl: 'https://hooks.example.com/phoebis/new', windowId: '5h' };

/** Every `Api` method the gate treats as a write, called with inputs the admin would be answered for. */
const WRITES = [
  ['cancelPairing', (api) => api.cancelPairing('pair_unknown')],
  ['clearCooldown', (api) => api.clearCooldown('mail', 'account')],
  ['clearFunding', (api) => api.clearFunding('personal')],
  ['completePairing', (api) => api.completePairing('pair_unknown', { code: 'code', state: 'state' })],
  ['confirmPairing', (api) => api.confirmPairing('pair_unknown')],
  ['confirmPriceImport', (api) => api.confirmPriceImport('imp_unknown')],
  ['createAccount', (api) => api.createAccount({ id: 'fresh', kind: 'api_key', label: 'Fresh', secret: 'sk-ant-api03-FAKE' })],
  ['createAlertRule', (api) => api.createAlertRule(newRule)],
  ['createKey', (api) => api.createKey(keyInput)],
  ['createPool', async (api) => api.createPool({ ...(await storyPool(api)), name: 'fresh' })],
  ['deleteAccount', (api) => api.deleteAccount('spare')],
  ['deleteAlertRule', (api) => api.deleteAlertRule('rule_7d')],
  ['deleteKey', (api) => api.deleteKey('batch')],
  ['deletePool', (api) => api.deletePool('codex')],
  ['disconnectAccount', (api) => api.disconnectAccount('business')],
  ['pauseAccount', (api) => api.pauseAccount('business')],
  ['previewPriceImport', (api) => api.previewPriceImport('https://example.invalid/prices.json')],
  ['releaseSession', async (api) => { const s = (await api.listSessions()).find((x) => !x.historical); return api.releaseSession(s.keyId, s.sessionId); }],
  ['renameAccount', (api) => api.renameAccount('business', 'Renamed')],
  ['resolveAlertEvent', async (api) => api.resolveAlertEvent((await api.listAlertEvents('open'))[0].id)],
  ['resumeAccount', (api) => api.resumeAccount('team')],
  ['revokeKey', (api) => api.revokeKey('laptop')],
  ['rotateKey', (api) => api.rotateKey('laptop')],
  ['startPairing', (api) => api.startPairing('team')],
  ['testAlertRule', (api) => api.testAlertRule('rule_7d')],
  ['updateAlertRule', async (api) => api.updateAlertRule('rule_7d', ruleInputOf((await api.listAlertRules()).find((r) => r.id === 'rule_7d')))],
  ['updateKey', (api) => api.updateKey('laptop', { ...keyInput, name: 'laptop', expiresAt: undefined })],
  ['updatePool', async (api) => api.updatePool('all', await storyPool(api))],
  ['verifyAccount', (api) => api.verifyAccount('business')],
];
/** The gate lets a viewer end its own session and refresh a usage snapshot; there is no session before a login. */
const EXEMPT = ['login', 'logout', 'refreshUsage'];
const READS = [
  'getAccountCatalog', 'getAnalyticsBreakdown', 'getAnalyticsSeries', 'getCapturedBodies', 'getKeepaliveOverview', 'getKeyModels', 'getModelsState', 'getOverview', 'getQuotaHistory', 'getRequest', 'getSession', 'getSettings',
  'listAccountRollups', 'listAccounts', 'listAlertDeliveries', 'listAlertEvents', 'listAlertRules', 'listCatalog', 'listCatalogOf', 'listKeepaliveRecipes', 'listKeys', 'listLifecycleDeliveries',
  'listPools', 'listQuotaSnapshots', 'listRequests', 'listSessions', 'subscribeLive',
];

const refusedAs = (status, message) => (e) => e instanceof d.ApiError && e.status === status && e.message === message && e.code === null;

test('the write table names every method of the client, so a new write cannot miss the role check', () => {
  assert.deepEqual([...WRITES.map(([name]) => name), ...EXEMPT, ...READS].sort(), Object.keys(new d.HttpApi()).sort());
});

test('every write refuses a viewer with the gate\'s 403 and changes nothing', async () => {
  const api = await as('viewer');
  const before = await Promise.all([api.listAccounts(), api.listAlertRules(), api.listKeys(), api.listPools(), api.listSessions()]);
  for (const [name, invoke] of WRITES) {
    await assert.rejects(() => invoke(api), refusedAs(403, 'the viewer role cannot change anything'), name);
  }
  assert.deepEqual(await Promise.all([api.listAccounts(), api.listAlertRules(), api.listKeys(), api.listPools(), api.listSessions()]), before);
});

test('a viewer may still refresh usage and log out, which the gate lets through', async () => {
  const api = await as('viewer');
  await api.refreshUsage('business');
  await api.logout();
  assert.equal(await api.getSession(), null);
});

test('captured bodies are the admin\'s, as the bodies route answers a viewer', async () => {
  const viewer = await as('viewer');
  const captured = (await viewer.listRequests({ limit: 500 })).requests.find((r) => r.capturedBodies);
  await assert.rejects(() => viewer.getCapturedBodies(captured.id), refusedAs(403, 'captured bodies are for administrators'));
  const admin = await as('admin');
  assert.notEqual(await admin.getCapturedBodies(captured.id), null);
});

test('a viewer is answered the webhook host alone, where the admin reads the whole URL', async () => {
  const viewer = await as('viewer');
  const admin = await as('admin');
  const full = (await admin.listAlertRules()).find((r) => r.id === 'rule_7d').webhookUrl;
  assert.equal(full, 'https://hooks.example.com/phoebis/quota');
  assert.deepEqual([...new Set((await viewer.listAlertRules()).map((r) => r.webhookUrl))], ['hooks.example.com']);
});

test('a utilization rule without its threshold or its window is refused with the route\'s text', async () => {
  const api = await as('admin');
  const count = (await api.listAlertRules()).length;
  await assert.rejects(() => api.createAlertRule({ ...newRule, threshold: null }), refusedAs(400, 'utilization_above needs a threshold between 0 and 1'));
  await assert.rejects(() => api.createAlertRule({ ...newRule, windowId: null }), refusedAs(400, 'utilization_above needs a window of 5h, 7d, or 7d_oi'));
  const rule = (await api.listAlertRules()).find((r) => r.id === 'rule_7d');
  await assert.rejects(() => api.updateAlertRule('rule_7d', { ...ruleInputOf(rule), windowId: null }), refusedAs(400, 'utilization_above needs a window of 5h, 7d, or 7d_oi'), 'create and patch refuse the same rows');
  assert.equal((await api.listAlertRules()).length, count, 'a refused rule is not stored');
  assert.equal((await api.createAlertRule(newRule)).windowId, '5h');
});

test('a rule with an open event keeps its condition until the event resolves', async () => {
  const api = await as('admin');
  const rule = (await api.listAlertRules()).find((r) => r.id === 'rule_7d');
  const open = (await api.listAlertEvents('open')).filter((e) => e.ruleId === 'rule_7d');
  assert.equal(open.length, 1, 'the story holds one open event on this rule');
  const moved = { ...ruleInputOf(rule), kind: 'pool_exhausted', threshold: null, windowId: null };
  await assert.rejects(() => api.updateAlertRule('rule_7d', moved), refusedAs(409, 'the condition cannot change while 1 event is open for this rule'));
  assert.equal((await api.updateAlertRule('rule_7d', { ...ruleInputOf(rule), threshold: 0.85 })).threshold, 0.85, 'an edit that keeps the kind saves');
  await api.resolveAlertEvent(open[0].id);
  assert.equal((await api.updateAlertRule('rule_7d', moved)).kind, 'pool_exhausted');
});

test('a key or a pool whose id is taken is refused with the route\'s 409', async () => {
  const api = await as('admin');
  const counts = async () => [(await api.listKeys()).length, (await api.listPools()).length];
  const before = await counts();
  await assert.rejects(() => api.createKey({ ...keyInput, name: 'Laptop' }), refusedAs(409, 'a key with this id exists'));
  await assert.rejects(async () => api.createPool({ ...(await storyPool(api)), name: 'All' }), refusedAs(409, 'a pool with this id exists'));
  assert.deepEqual(await counts(), before);
});

test('the mock derives an id from a name the way the client does, which the slug rule always admits', async () => {
  const api = await as('admin');
  const pool = await api.createPool({ ...(await storyPool(api)), name: '  Nightly batch!  ' });
  const long = await api.createPool({ ...(await storyPool(api)), name: 'x'.repeat(80) });
  const { key } = await api.createKey({ ...keyInput, name: 'Build box' });
  assert.deepEqual([pool.id, long.id, key.id], [d.slug('  Nightly batch!  '), 'x'.repeat(64), 'build-box']);
  for (const id of [pool.id, long.id, key.id]) assert.match(id, /^[a-z0-9][a-z0-9-]{0,63}$/);
});

test('a pool create refuses the values a pool edit refuses', async () => {
  const api = await as('admin');
  const all = await storyPool(api);
  await assert.rejects(() => api.createPool({ ...all, affinityTtlSeconds: 30, keepalive: { ...all.keepalive, enabled: false }, name: 'short' }), refusedAs(400, 'affinity_ttl_seconds is 0 or at least 60'));
  await assert.rejects(() => api.createPool({ ...all, keepalive: { ...all.keepalive, coldProbeLimit: 0 }, name: 'cold' }), refusedAs(400, 'keepalive.cold_probe_limit must be between 1 and 20, not 0'));
  await assert.rejects(() => api.createPool({ ...all, bodyCaptureHours: 100_000, name: 'hoard' }), refusedAs(400, 'body_capture_hours exceeds stats.body_capture_max_hours'));
  assert.equal((await api.listPools()).some((p) => ['cold', 'hoard', 'short'].includes(p.id)), false);
});

test('a new pool reads each member the way the pool route does', async () => {
  const api = await as('admin');
  const members = (await api.listAccounts()).map((a) => ({ accountId: a.id, priority: 0 }));
  const verdicts = async (name, overagePolicy) => Object.fromEntries((await api.createPool({ ...(await storyPool(api)), members, name, overagePolicy })).members.map((m) => [m.accountId, m.eligible]));
  assert.deepEqual(await verdicts('first', 'never'), {
    business: 'eligible',
    console: 'eligible',
    evals: 'eligible',
    mail: 'cooldown',
    personal: 'unavailable',
    spare: 'unavailable',
    team: 'unavailable',
  });
  // Funded again, the upstream the router reads in overage is an obstacle only under a policy that admits none.
  await api.clearFunding('personal');
  await api.pauseAccount('business');
  const second = await verdicts('second', 'never');
  assert.deepEqual([second.personal, second.business], ['overage', 'unavailable'], 'a paused upstream is unavailable like a flagged one');
  assert.equal((await verdicts('third', 'prefer_plan')).personal, 'eligible');
});

test('a model rule list the route refuses is refused on every pool and key write, with the route\'s text', async () => {
  const api = await as('admin');
  const all = await storyPool(api);
  const laptop = { ...keyInput, name: 'laptop' };
  const stored = async () => [(await api.listPools()).map((p) => p.modelRules), (await api.listKeys()).map((k) => [k.modelRules, k.modelRulesMode])];
  const before = await stored();
  const writes = (modelRules) => [
    () => api.createPool({ ...all, modelRules, name: 'ruled' }),
    () => api.updatePool('all', { ...all, modelRules }),
    () => api.createKey({ ...keyInput, modelRules }),
    () => api.updateKey('laptop', { ...laptop, modelRules }),
  ];
  const stars = [{ not: false, pattern: 'claude-*' }, { not: true, pattern: 'a***' }];
  for (const write of writes(stars)) await assert.rejects(write, refusedAs(400, 'model_rules[1]: pattern holds three stars in a row'));
  const long = Array.from({ length: 257 }, () => ({ not: false, pattern: '' }));
  for (const write of writes(long)) await assert.rejects(write, refusedAs(400, 'model_rules holds at most 256 entries'), 'the bound is checked before any entry');
  const mode = refusedAs(400, 'model_rules_mode is extend or replace');
  await assert.rejects(() => api.createKey({ ...keyInput, modelRulesMode: 'merge' }), mode);
  await assert.rejects(() => api.updateKey('laptop', { ...laptop, modelRulesMode: 'merge' }), mode);
  await assert.rejects(() => api.updateKey('laptop', { ...laptop, modelRules: stars, modelRulesMode: 'merge' }), refusedAs(400, 'model_rules[1]: pattern holds three stars in a row'), 'the list is read before the mode');
  assert.deepEqual(await stored(), before, 'a refused write stores nothing');
  const bound = Array.from({ length: 256 }, () => ({ not: false, pattern: '*' }));
  assert.equal((await api.updatePool('all', { ...all, modelRules: bound })).modelRules.length, 256, 'the bound admits 256 entries');
});

test('a key write without the model fields takes the route\'s defaults on create and keeps the stored ones on edit', async () => {
  const api = await as('admin');
  const { longContextTwins: _t, modelRules: _r, modelRulesMode: _m, ...bare } = keyInput;
  const { key } = await api.createKey({ ...bare, name: 'bare' });
  assert.deepEqual([key.modelRules, key.modelRulesMode, key.longContextTwins], [[], 'extend', true]);
  const laptop = (await api.listKeys()).find((k) => k.id === 'laptop');
  const edited = await api.updateKey('laptop', { ...bare, name: 'laptop' });
  assert.deepEqual([edited.modelRules, edited.modelRulesMode, edited.longContextTwins], [laptop.modelRules, laptop.modelRulesMode, laptop.longContextTwins]);
  const { modelRules: _p, ...unruled } = await storyPool(api);
  const pool = await api.createPool({ ...unruled, name: 'unruled' });
  assert.deepEqual(pool.modelRules, [], 'a pool created without a list starts with none');
});

test('the catalog union names each model once and a failed fetch contributes nothing', async () => {
  const api = await as('viewer');
  const ids = (models) => models.map((m) => m.id);
  const business = ids((await api.getAccountCatalog('business')).models);
  assert.ok(business.length > 0, 'the story has a catalog to union');
  assert.deepEqual(ids(await api.listCatalogOf(['business', 'console', 'personal', 'nobody'])), [...business].sort(), 'the failing upstream and the unknown one add nothing');
  assert.deepEqual(await api.listCatalogOf(['personal']), []);
  assert.deepEqual(await api.listCatalogOf([]), []);
});

test('a refusal never carries an error code, which the router sets for an unavailable registry alone', async () => {
  const api = await as('admin');
  const missing = [
    () => api.confirmPriceImport('imp_unknown'),
    () => api.renameAccount('nobody', 'x'),
    () => api.resolveAlertEvent('nothing'),
    () => api.rotateKey('nothing'),
    () => api.testAlertRule('nothing'),
    () => api.updateAlertRule('nothing', newRule),
  ];
  for (const call of missing) await assert.rejects(call, (e) => e instanceof d.ApiError && e.status === 404 && e.code === null);
});

test('an unknown id is refused with the route\'s own 404 text, which the console shows as the toast body', async () => {
  const api = await as('admin');
  await assert.rejects(() => api.renameAccount('nobody', 'x'), refusedAs(404, 'account not found'));
  await assert.rejects(() => api.resolveAlertEvent('nothing'), refusedAs(404, 'alert event not found'));
  await assert.rejects(() => api.rotateKey('nothing'), refusedAs(404, 'key not found'));
  await assert.rejects(() => api.testAlertRule('nothing'), refusedAs(404, 'alert rule not found'));
  await assert.rejects(() => api.updateAlertRule('nothing', newRule), refusedAs(404, 'alert rule not found'));
});

test('the quota snapshots never answer a status the decoder cannot produce', async () => {
  const api = await as('admin');
  for (let read = 0; read < 2; read += 1) {
    const statuses = (await api.listQuotaSnapshots()).map((s) => s.status);
    assert.deepEqual(statuses.filter((s) => s !== 'error' && s !== 'success'), [], `read ${read + 1}`);
  }
});
