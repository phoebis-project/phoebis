import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

/** A fresh mock signed in the way the login screen signs a browser in. */
const signedIn = async () => {
  const api = new d.MockApi();
  await api.login('admin');
  return api;
};

/** The pool as the dialog would send it back unchanged, so a case edits only the fields it is about. */
const poolInputOf = (p) => ({ ...p, members: p.members.map((m) => ({ accountId: m.accountId, budgetDailyUsd: m.budgetDailyUsd, budgetMonthlyUsd: m.budgetMonthlyUsd, priority: m.priority })) });
const storyPool = async (api) => poolInputOf((await api.listPools()).find((p) => p.id === 'all'));
const keyInput = { autoModeServer: null, bodyCaptureHours: null, enabled: true, expiresInDays: null, keepaliveOptOut: false, longContextTwins: true, modelRules: [], modelRulesMode: 'extend', name: 'fresh', notes: null, poolId: 'all' };

const refusedAs = (status, message) => (e) => e instanceof d.ApiError && e.status === status && e.message === message && e.code === null;

test('a key or a pool whose id is taken is refused with the route\'s 409', async () => {
  const api = await signedIn();
  const counts = async () => [(await api.listKeys()).length, (await api.listPools()).length];
  const before = await counts();
  await assert.rejects(() => api.createKey({ ...keyInput, name: 'Laptop' }), refusedAs(409, 'a key with this id exists'));
  await assert.rejects(async () => api.createPool({ ...(await storyPool(api)), name: 'All' }), refusedAs(409, 'a pool with this id exists'));
  assert.deepEqual(await counts(), before);
});

test('the mock derives an id from a name the way the client does, which the slug rule always admits', async () => {
  const api = await signedIn();
  const pool = await api.createPool({ ...(await storyPool(api)), name: '  Nightly batch!  ' });
  const long = await api.createPool({ ...(await storyPool(api)), name: 'x'.repeat(80) });
  const { key } = await api.createKey({ ...keyInput, name: 'Build box' });
  assert.deepEqual([pool.id, long.id, key.id], [d.slug('  Nightly batch!  '), 'x'.repeat(64), 'build-box']);
  for (const id of [pool.id, long.id, key.id]) assert.match(id, /^[a-z0-9][a-z0-9-]{0,63}$/);
});

test('a pool create refuses the values a pool edit refuses', async () => {
  const api = await signedIn();
  const all = await storyPool(api);
  await assert.rejects(() => api.createPool({ ...all, affinityTtlSeconds: 30, keepalive: { ...all.keepalive, enabled: false }, name: 'short' }), refusedAs(400, 'affinity_ttl_seconds is 0 or at least 60'));
  await assert.rejects(() => api.createPool({ ...all, keepalive: { ...all.keepalive, coldProbeLimit: 0 }, name: 'cold' }), refusedAs(400, 'keepalive.cold_probe_limit must be between 1 and 20, not 0'));
  await assert.rejects(() => api.createPool({ ...all, bodyCaptureHours: 100_000, name: 'hoard' }), refusedAs(400, 'body_capture_hours exceeds stats.body_capture_max_hours'));
  assert.equal((await api.listPools()).some((p) => ['cold', 'hoard', 'short'].includes(p.id)), false);
});

test('a new pool reads each member the way the pool route does', async () => {
  const api = await signedIn();
  const members = (await api.listAccounts()).map((a) => ({ accountId: a.id, priority: 0 }));
  const verdicts = async (name, overagePolicy) => Object.fromEntries((await api.createPool({ ...(await storyPool(api)), members, name, overagePolicy })).members.map((m) => [m.accountId, m.eligible]));
  assert.deepEqual(await verdicts('first', 'never'), {
    business: 'eligible',
    console: 'unavailable',
    evals: 'eligible',
    mail: 'cooldown',
    personal: 'overage',
    spare: 'unavailable',
    team: 'unavailable',
  });
  // Funded again, the flagged account serves; the account the router reads in overage is an obstacle only under a
  // policy that admits none.
  await api.clearFunding('console');
  await api.pauseAccount('business');
  const second = await verdicts('second', 'never');
  assert.deepEqual([second.console, second.personal, second.business], ['eligible', 'overage', 'unavailable'], 'a paused account is unavailable like a flagged one');
  assert.equal((await verdicts('third', 'prefer_plan')).personal, 'eligible');
});

test('a pool write round-trips the pin and placement fields and the members\' budgets', async () => {
  const api = await signedIn();
  const all = await storyPool(api);
  const console_ = all.members.find((m) => m.accountId === 'console');
  assert.deepEqual([console_.budgetDailyUsd, console_.budgetMonthlyUsd], [5, 50], 'the story budgets its Anthropic-key member by the day and the month');
  const edit = {
    ...all,
    affinityMode: 'always',
    members: all.members.map((m) => (m.accountId === 'console' ? { ...m, budgetDailyUsd: 7.5, budgetMonthlyUsd: null } : m)),
    minSharedPrefixTokens: 30_000,
    sessionPlacement: 'by_strategy',
    subagentPlacement: 'with_main',
  };
  const saved = await api.updatePool('all', edit);
  const stored = (await api.listPools()).find((p) => p.id === 'all');
  for (const pool of [saved, stored]) {
    assert.deepEqual([pool.affinityMode, pool.minSharedPrefixTokens, pool.sessionPlacement, pool.subagentPlacement], ['always', 30_000, 'by_strategy', 'with_main']);
    const member = pool.members.find((m) => m.accountId === 'console');
    assert.deepEqual([member.budgetDailyUsd, member.budgetMonthlyUsd, member.kind, member.spends, member.budgetUntil], [7.5, null, 'api_key', [], null], 'the mock keeps no ledger, so a write answers no spend');
  }
  assert.deepEqual(poolInputOf(stored).members, edit.members, 'the members read back as they were written');
  const created = await api.createPool({ ...edit, name: 'budgeted' });
  assert.deepEqual(
    created.members.map((m) => [m.accountId, m.kind, m.budgetDailyUsd, m.budgetMonthlyUsd]),
    edit.members.map((m) => [m.accountId, m.accountId === 'console' ? 'api_key' : 'oauth', m.budgetDailyUsd, m.budgetMonthlyUsd]),
    'a create derives each member\'s kind from its account',
  );
  const bare = await api.createPool({ ...edit, members: [{ accountId: 'console', priority: 0 }], name: 'bare' });
  assert.deepEqual([bare.members[0].budgetDailyUsd, bare.members[0].budgetMonthlyUsd], [null, null], 'a member written without a budget field has no budget over that period');
});

test('the story reads a member at its budget as the route does, and a member half way as under it', async () => {
  const api = await signedIn();
  const pools = await api.listPools();
  const member = (poolId, accountId) => pools.find((p) => p.id === poolId).members.find((m) => m.accountId === accountId);
  const half = member('all', 'console');
  // The story flags this member's funding, which the route reads before any budget, so the verdict is the flag's and
  // the empty budgetUntil is the half-way reading.
  assert.deepEqual(
    [half.spends.map((s) => [s.period, s.spentUsd]), half.budgetDailyUsd, half.budgetMonthlyUsd, half.budgetUntil, half.eligible],
    [[['day', 1.1], ['month', 25]], 5, 50, null, 'unavailable'],
  );
  const spent = member('codex', 'evals');
  const [day] = spent.spends;
  assert.ok(day.spentUsd >= spent.budgetDailyUsd && Date.parse(day.periodEnd) > Date.now(), 'the story spends one member past its daily budget inside the day');
  assert.deepEqual([spent.budgetUntil, spent.eligible], [day.periodEnd, 'budget'], 'the route holds it to the day\'s end, the one budget it reached');
  assert.equal(pools.find((p) => p.id === 'codex').status, 'exhausted', 'its only member at its budget leaves the pool nothing to route to');
});

test('a pool answers its keys from the key registry', async () => {
  const api = await signedIn();
  const keysOf = async (poolId) => (await api.listPools()).find((p) => p.id === poolId).keys.map((k) => [k.id, k.enabled]);
  assert.deepEqual(await keysOf('studio'), [['notebook', true]]);
  await api.createKey({ ...keyInput, name: 'scratch', poolId: 'studio' });
  await api.revokeKey('notebook');
  assert.deepEqual(await keysOf('studio'), [['scratch', true], ['notebook', false]], 'a revocation switches the key off in the pool\'s list too');
  const laptop = (await api.listKeys()).find((k) => k.id === 'laptop');
  await api.updateKey('laptop', { ...keyInput, name: laptop.name, poolId: 'studio', expiresAt: undefined });
  assert.ok((await keysOf('studio')).some(([id]) => id === 'laptop') && !(await keysOf('all')).some(([id]) => id === 'laptop'), 'a moved key answers under its new pool alone');
});

test("a key write stores the server-side auto mode override and a pool write stores the switch", async () => {
  const api = await signedIn();
  const { key: inheriting } = await api.createKey({ ...keyInput, name: 'inheriting' });
  assert.equal(inheriting.autoModeServer, null, 'the mock answers the inherit it was sent');
  const { key: pinned } = await api.createKey({ ...keyInput, autoModeServer: false, name: 'pinned' });
  assert.equal(pinned.autoModeServer, false);
  const edited = await api.updateKey('pinned', { ...keyInput, autoModeServer: true, expiresAt: undefined, name: 'pinned' });
  assert.equal(edited.autoModeServer, true);
  const plain = { ...keyInput, name: 'plain' };
  delete plain.autoModeServer;
  assert.equal((await api.createKey(plain)).key.autoModeServer, null, "a body without the field takes the route's default");
  const pool = await storyPool(api);
  await api.updatePool('all', { ...pool, autoModeServer: false });
  assert.equal((await api.listPools()).find((p) => p.id === 'all').autoModeServer, false);
  const { autoModeServer: _dropped, ...unset } = pool;
  const created = await api.createPool({ ...unset, name: 'fresh' });
  assert.equal(created.autoModeServer, true, "a new pool without the field takes the route's default");
});


test('a pool write with an unknown spelling or a budget the route refuses is refused with the route\'s text', async () => {
  const api = await signedIn();
  const all = await storyPool(api);
  const before = await api.listPools();
  const refusals = [
    [{ affinityMode: 'sometimes' }, 'affinity_mode must be always or when_cached'],
    [{ subagentPlacement: 'alone' }, 'subagent_placement must be by_cohort, by_strategy, or with_main'],
    [{ sessionPlacement: 'by_luck' }, 'session_placement must be by_strategy or by_warm_prefix'],
    // The route reads the threshold as an unsigned whole number, so any other value fails the body's parse.
    [{ minSharedPrefixTokens: -1 }, 'body must be a JSON object'],
    [{ minSharedPrefixTokens: 1.5 }, 'body must be a JSON object'],
    [{ members: [{ accountId: 'console', budgetDailyUsd: 0, budgetMonthlyUsd: null, priority: 0 }] }, 'budget_daily_usd is above 0'],
    [{ members: [{ accountId: 'console', budgetDailyUsd: null, budgetMonthlyUsd: -5, priority: 0 }] }, 'budget_monthly_usd is above 0'],
  ];
  for (const [patch, message] of refusals) {
    await assert.rejects(() => api.updatePool('all', { ...all, ...patch }), refusedAs(400, message), message);
    await assert.rejects(() => api.createPool({ ...all, ...patch, name: 'refused' }), refusedAs(400, message), `${message} on create`);
  }
  assert.deepEqual(await api.listPools(), before, 'a refused write stores nothing');
});

test('a model rule list the route refuses is refused on every pool and key write, with the route\'s text', async () => {
  const api = await signedIn();
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
  const api = await signedIn();
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

test('an account patch is refused where the account route refuses it, and a cancelled end stands only with its flag', async () => {
  const api = await signedIn();
  const end = '2026-10-19T09:15:00.000Z';
  const before = await api.listAccounts();
  await assert.rejects(() => api.updateAccount('console', { cancelled: true, subscriptionEndsAt: end }), refusedAs(400, 'only an oauth account is cancelled'));
  await assert.rejects(() => api.updateAccount('business', { cancelled: false, subscriptionEndsAt: end }), refusedAs(400, 'subscription_ends_at needs cancelled'));
  await assert.rejects(() => api.updateAccount('business', { cancelled: true, subscriptionEndsAt: null }), refusedAs(400, 'cancelled needs subscription_ends_at'));
  await assert.rejects(() => api.updateAccount('nobody', { cancelled: false, subscriptionEndsAt: null }), refusedAs(404, 'account not found'));
  assert.deepEqual(await api.listAccounts(), before, 'a refused patch stores nothing');
  const cancelled = await api.updateAccount('business', { cancelled: true, subscriptionEndsAt: end });
  assert.deepEqual([cancelled.cancelled, cancelled.subscription.endsAt], [true, end]);
  const restored = await api.updateAccount('business', { cancelled: false, subscriptionEndsAt: null });
  assert.deepEqual([restored.cancelled, restored.subscription.endsAt], [false, null], 'the end leaves with the flag');
  const unpaired = await api.updateAccount('spare', { cancelled: true, subscriptionEndsAt: end });
  assert.equal(unpaired.subscription?.endsAt, end, 'an OAuth account that never paired still reports the end it was cancelled with');
});

test('a model-scoped cooldown reads its window from its reason, not a fixed model_rejection value', async () => {
  const api = await signedIn();
  const business = api.accounts.find((a) => a.id === 'business');
  const sample = business.cooldowns[0];
  business.cooldowns = [...business.cooldowns, { ...sample, reason: 'permission', scope: 'model:claude-test-permission' }];
  const state = await api.getModelsState();
  const permitted = state.cooldowns.find((g) => g.model === 'claude-test-permission');
  assert.equal(permitted.rows[0].window, 'permission', 'a non-model_rejected reason reports its own text as the window');
  const rejected = state.cooldowns.find((g) => g.model === 'claude-haiku-4-5-20251001');
  assert.equal(rejected.rows[0].window, 'model_rejection', 'model_rejected still reports model_rejection');
});

test('the catalog union names each model once and a failed fetch contributes nothing', async () => {
  const api = await signedIn();
  const ids = (models) => models.map((m) => m.id);
  const business = ids((await api.getAccountCatalog('business')).models);
  assert.ok(business.length > 0, 'the story has a catalog to union');
  assert.deepEqual(ids(await api.listCatalogOf(['business', 'console', 'personal', 'nobody'])), [...business].sort(), 'the failing account and the unknown one add nothing');
  assert.deepEqual(await api.listCatalogOf(['personal']), []);
  assert.deepEqual(await api.listCatalogOf([]), []);
});

test('a refusal never carries an error code, which the router sets for an unavailable registry alone', async () => {
  const api = await signedIn();
  const missing = [
    () => api.renameAccount('nobody', 'x'),
    () => api.rotateKey('nothing'),
  ];
  for (const call of missing) await assert.rejects(call, (e) => e instanceof d.ApiError && e.status === 404 && e.code === null);
});

test('an unknown id is refused with the route\'s own 404 text, which the console shows as the toast body', async () => {
  const api = await signedIn();
  await assert.rejects(() => api.renameAccount('nobody', 'x'), refusedAs(404, 'account not found'));
  await assert.rejects(() => api.rotateKey('nothing'), refusedAs(404, 'key not found'));
});

test('the quota snapshots never answer a status or a poll outcome the decoder cannot produce', async () => {
  const api = await signedIn();
  for (let read = 0; read < 2; read += 1) {
    const snapshots = await api.listQuotaSnapshots();
    assert.deepEqual(snapshots.map((s) => s.status).filter((s) => s !== 'error' && s !== 'success'), [], `read ${read + 1}`);
    const polls = snapshots.map((s) => s.poll);
    assert.deepEqual(polls.filter((p) => p !== null && !['clean', 'failed', 'rate_limited'].includes(p.lastOutcome)), [], `read ${read + 1}`);
    assert.ok(polls.includes(null) && polls.some((p) => p?.backoffSince), 'the story holds an account not yet polled and one backing off');
  }
});
