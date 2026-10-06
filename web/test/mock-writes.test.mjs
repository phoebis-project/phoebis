import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import * as d from './dist/api.js';

const fixture = (name) => JSON.parse(readFileSync(new URL(`./fixtures/${name}.json`, import.meta.url), 'utf8')).body;

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
    legacy: 'unavailable',
    nightly: 'overage',
    overflow: 'unavailable',
    primary: 'eligible',
    research: 'eligible',
    secondary: 'cooldown',
    spare: 'unavailable',
  });
  // Funded again, the flagged account serves; the account the router reads in overage is an obstacle only under a
  // policy that admits none.
  await api.clearFunding('overflow');
  await api.pauseAccount('primary');
  const second = await verdicts('second', 'never');
  assert.deepEqual([second.overflow, second.nightly, second.primary], ['eligible', 'overage', 'unavailable'], 'a paused account is unavailable like a flagged one');
  assert.equal((await verdicts('third', 'prefer_plan')).nightly, 'eligible');
});

test('a pool write round-trips the pin and placement fields and the members\' budgets', async () => {
  const api = await signedIn();
  const all = await storyPool(api);
  const overflow = all.members.find((m) => m.accountId === 'overflow');
  assert.deepEqual([overflow.budgetDailyUsd, overflow.budgetMonthlyUsd], [5, 50], 'the story budgets its Anthropic-key member by the day and the month');
  const edit = {
    ...all,
    affinityMode: 'always',
    members: all.members.map((m) => (m.accountId === 'overflow' ? { ...m, budgetDailyUsd: 7.5, budgetMonthlyUsd: null } : m)),
    minSharedPrefixTokens: 30_000,
    sessionPlacement: 'by_strategy',
    subagentPlacement: 'with_main',
  };
  const saved = await api.updatePool('all', edit);
  const stored = (await api.listPools()).find((p) => p.id === 'all');
  for (const pool of [saved, stored]) {
    assert.deepEqual([pool.affinityMode, pool.minSharedPrefixTokens, pool.sessionPlacement, pool.subagentPlacement], ['always', 30_000, 'by_strategy', 'with_main']);
    const member = pool.members.find((m) => m.accountId === 'overflow');
    assert.deepEqual([member.budgetDailyUsd, member.budgetMonthlyUsd, member.kind, member.spends, member.budgetUntil], [7.5, null, 'api_key', [], null], 'the mock keeps no ledger, so a write answers no spend');
  }
  assert.deepEqual(poolInputOf(stored).members, edit.members, 'the members read back as they were written');
  const created = await api.createPool({ ...edit, name: 'budgeted' });
  assert.deepEqual(
    created.members.map((m) => [m.accountId, m.kind, m.budgetDailyUsd, m.budgetMonthlyUsd]),
    edit.members.map((m) => [m.accountId, m.accountId === 'overflow' ? 'api_key' : 'oauth', m.budgetDailyUsd, m.budgetMonthlyUsd]),
    'a create derives each member\'s kind from its account',
  );
  const bare = await api.createPool({ ...edit, members: [{ accountId: 'overflow', priority: 0 }], name: 'bare' });
  assert.deepEqual([bare.members[0].budgetDailyUsd, bare.members[0].budgetMonthlyUsd], [null, null], 'a member written without a budget field has no budget over that period');
});

test('the story reads a member at its budget as the route does, and a member half way as under it', async () => {
  const api = await signedIn();
  const pools = await api.listPools();
  const member = (poolId, accountId) => pools.find((p) => p.id === poolId).members.find((m) => m.accountId === accountId);
  const half = member('all', 'overflow');
  // The story flags this member's funding, which the route reads before any budget, so the verdict is the flag's and
  // the empty budgetUntil is the half-way reading.
  assert.deepEqual(
    [half.spends.map((s) => [s.period, s.spentUsd]), half.budgetDailyUsd, half.budgetMonthlyUsd, half.budgetUntil, half.eligible],
    [[['day', 1.1], ['month', 25]], 5, 50, null, 'unavailable'],
  );
  // The mock reads each member's verdict once, from the story's accounts as it is built, so the flag is cleared on the
  // story's account before a second mock reads it, and restored for the cases after this one.
  const seed = d.ACCOUNTS.find((a) => a.id === 'overflow');
  const flagged = seed.status;
  seed.status = 'ready';
  try {
    const cleared = (await (await signedIn()).listPools()).find((p) => p.id === 'all').members.find((m) => m.accountId === 'overflow');
    assert.deepEqual([cleared.budgetUntil, cleared.eligible], [null, 'eligible'], 'without the flag, a member under both its budgets is eligible');
  } finally {
    seed.status = flagged;
  }
  const spent = member('codex', 'research');
  const [day] = spent.spends;
  assert.ok(day.spentUsd >= spent.budgetDailyUsd && Date.parse(day.periodEnd) > Date.now(), 'the story spends one member past its daily budget inside the day');
  assert.deepEqual([spent.budgetUntil, spent.eligible], [day.periodEnd, 'budget'], 'the route holds it to the day\'s end, the one budget it reached');
  assert.equal(pools.find((p) => p.id === 'codex').status, 'exhausted', 'its only member at its budget leaves the pool nothing to route to');
});

test('a pool answers its keys from the key registry', async () => {
  const api = await signedIn();
  const keysOf = async (poolId) => (await api.listPools()).find((p) => p.id === poolId).keys.map((k) => [k.id, k.enabled]);
  assert.deepEqual(await keysOf('work'), [['notebook', true]]);
  await api.createKey({ ...keyInput, name: 'scratch', poolId: 'work' });
  await api.revokeKey('notebook');
  assert.deepEqual(await keysOf('work'), [['scratch', true], ['notebook', false]], 'a revocation switches the key off in the pool\'s list too');
  const laptop = (await api.listKeys()).find((k) => k.id === 'laptop');
  await api.updateKey('laptop', { ...keyInput, name: laptop.name, poolId: 'work', expiresAt: undefined });
  assert.ok((await keysOf('work')).some(([id]) => id === 'laptop') && !(await keysOf('all')).some(([id]) => id === 'laptop'), 'a moved key answers under its new pool alone');
});

const staleAs = (message) => (e) => e instanceof d.ApiError && e.status === 409 && e.message === message && e.code === 'stale_edit';
const POOL_STALE = 'the pool changed while it was being edited; reload and try again';
const KEY_STALE = 'the key changed while it was being edited; reload and try again';
const storyKey = async (api, id) => (await api.listKeys()).find((k) => k.id === id);

test('a pool or key save over a stamp the row no longer carries is refused as stale and stores nothing, and one over its own stamp moves it', async () => {
  const api = await signedIn();
  const pool = (await api.listPools()).find((p) => p.id === 'all');
  await assert.rejects(() => api.updatePool('all', { ...poolInputOf(pool), name: 'moved' }, '2020-01-01T00:00:00.000Z'), staleAs(POOL_STALE));
  assert.deepEqual((await api.listPools()).find((p) => p.id === 'all'), pool);
  const savedPool = await api.updatePool('all', { ...poolInputOf(pool), name: 'moved' }, pool.updatedAt);
  assert.equal(savedPool.name, 'moved');
  assert.ok(Date.parse(savedPool.updatedAt) > Date.parse(pool.updatedAt), 'the save moves the stamp past the one it was read at');
  const key = await storyKey(api, 'laptop');
  const edit = { ...keyInput, expiresAt: undefined, name: 'moved', poolId: key.poolId };
  await assert.rejects(() => api.updateKey('laptop', edit, '2020-01-01T00:00:00.000Z'), staleAs(KEY_STALE));
  assert.deepEqual(await storyKey(api, 'laptop'), key);
  const savedKey = await api.updateKey('laptop', edit, key.updatedAt);
  assert.equal(savedKey.name, 'moved');
  assert.ok(Date.parse(savedKey.updatedAt) > Date.parse(key.updatedAt), 'the save moves the stamp past the one it was read at');
});

test('a rotation or a revocation moves the key\'s stamp, so an edit read before it is refused as stale', async () => {
  const api = await signedIn();
  for (const [id, write] of [['laptop', (a) => a.rotateKey('laptop')], ['batch', (a) => a.revokeKey('batch')]]) {
    const before = await storyKey(api, id);
    await write(api);
    const after = await storyKey(api, id);
    assert.ok(Date.parse(after.updatedAt) > Date.parse(before.updatedAt), id);
    // The key's own switch, since the route refuses re-enabling a revoked key ahead of the fence.
    await assert.rejects(() => api.updateKey(id, { ...keyInput, enabled: before.enabled, expiresAt: undefined, name: 'moved', poolId: before.poolId }, before.updatedAt), staleAs(KEY_STALE), id);
    assert.equal((await storyKey(api, id)).name, before.name, id);
  }
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
    [{ members: [{ accountId: 'overflow', budgetDailyUsd: 0, budgetMonthlyUsd: null, priority: 0 }] }, 'budget_daily_usd is above 0'],
    [{ members: [{ accountId: 'overflow', budgetDailyUsd: null, budgetMonthlyUsd: -5, priority: 0 }] }, 'budget_monthly_usd is above 0'],
  ];
  for (const [patch, message] of refusals) {
    await assert.rejects(() => api.updatePool('all', { ...all, ...patch }), refusedAs(400, message), message);
    await assert.rejects(() => api.createPool({ ...all, ...patch, name: 'refused' }), refusedAs(400, message), `${message} on create`);
  }
  assert.deepEqual(await api.listPools(), before, 'a refused write stores nothing');
});

test('a pool save naming an unknown member answers the route\'s 404 and stores nothing', async () => {
  const api = await signedIn();
  const all = await storyPool(api);
  const before = await api.listPools();
  const ghost = { accountId: 'ghost', budgetDailyUsd: null, budgetMonthlyUsd: null, priority: 0 };
  await assert.rejects(() => api.updatePool('all', { ...all, members: [...all.members, ghost], name: 'everything' }), refusedAs(404, 'account ghost not found'));
  assert.deepEqual(await api.listPools(), before, 'neither the settings nor the members were stored');
});

test('a pool save naming an account twice answers the route\'s 409 ahead of every other member refusal and stores nothing', async () => {
  const api = await signedIn();
  const all = await storyPool(api);
  const before = await api.listPools();
  const member = (accountId, budgetDailyUsd = null) => ({ accountId, budgetDailyUsd, budgetMonthlyUsd: null, priority: 0 });
  const lists = [
    [[...all.members, { ...all.members[0], priority: 9 }], `members name account ${all.members[0].accountId} twice`],
    [[member('ghost'), member('ghost')], 'members name account ghost twice'],
    [[member('overflow', 0), member('overflow')], 'members name account overflow twice'],
  ];
  for (const [members, message] of lists) {
    await assert.rejects(() => api.updatePool('all', { ...all, members, name: 'everything' }), refusedAs(409, message), message);
    await assert.rejects(() => api.createPool({ ...all, members, name: 'twice' }), refusedAs(409, message), `${message} on create`);
  }
  assert.deepEqual(await api.listPools(), before, 'neither the settings nor the members were stored');
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
  await assert.rejects(() => api.updateAccount('overflow', { cancelled: true, subscriptionEndsAt: end }), refusedAs(400, 'only an oauth account is cancelled'));
  await assert.rejects(() => api.updateAccount('primary', { cancelled: false, subscriptionEndsAt: end }), refusedAs(400, 'subscription_ends_at needs cancelled'));
  await assert.rejects(() => api.updateAccount('primary', { cancelled: true, subscriptionEndsAt: null }), refusedAs(400, 'cancelled needs subscription_ends_at'));
  await assert.rejects(() => api.updateAccount('nobody', { cancelled: false, subscriptionEndsAt: null }), refusedAs(404, 'account not found'));
  assert.deepEqual(await api.listAccounts(), before, 'a refused patch stores nothing');
  const cancelled = await api.updateAccount('primary', { cancelled: true, subscriptionEndsAt: end });
  assert.deepEqual([cancelled.cancelled, cancelled.subscription.endsAt], [true, end]);
  const restored = await api.updateAccount('primary', { cancelled: false, subscriptionEndsAt: null });
  assert.deepEqual([restored.cancelled, restored.subscription.endsAt], [false, null], 'the end leaves with the flag');
  const unpaired = await api.updateAccount('spare', { cancelled: true, subscriptionEndsAt: end });
  assert.equal(unpaired.subscription?.endsAt, end, 'an OAuth account that never paired still reports the end it was cancelled with');
});

const ACCOUNT_STALE = 'the account changed while the edit was being saved';
const storyAccount = async (api, id) => (await api.listAccounts()).find((a) => a.id === id);

test('an account save over a revision the row no longer carries is refused as stale and stores nothing, and one over its own revision moves it', async () => {
  const api = await signedIn();
  const patch = { cancelled: true, subscriptionEndsAt: '2026-10-19T09:15:00.000Z' };
  const listed = await api.listAccounts();
  const before = listed.find((a) => a.id === 'primary');
  await assert.rejects(() => api.updateAccount('primary', patch, before.revision + 1), staleAs(ACCOUNT_STALE));
  assert.deepEqual(await api.listAccounts(), listed, 'a stale save stores nothing');
  const landed = await api.updateAccount('primary', patch, before.revision);
  assert.deepEqual([landed.cancelled, landed.revision], [true, before.revision + 1]);
  await assert.rejects(() => api.updateAccount('primary', patch, before.revision), staleAs(ACCOUNT_STALE), 'the counter moved past the sent revision');
  await assert.rejects(() => api.renameAccount('primary', 'x', landed.revision + 1), staleAs(ACCOUNT_STALE));
  assert.equal((await storyAccount(api, 'primary')).label, before.label);
  const renamed = await api.renameAccount('primary', 'x', landed.revision);
  assert.deepEqual([renamed.label, renamed.revision], ['x', landed.revision + 1]);
  const created = await api.createAccount({ id: 'fresh', kind: 'oauth', label: 'fresh', secret: null });
  assert.equal(created.revision, 1, 'a new row starts at the revision the route gives it');
});

test('a rename, an end edit, a pause, a resume, a funding clear or a disconnect moves the account\'s revision, so an edit read before it is refused as stale', async () => {
  const api = await signedIn();
  const end = '2026-10-19T09:15:00.000Z';
  let read = await storyAccount(api, 'primary');
  await api.renameAccount('primary', 'x');
  await assert.rejects(() => api.updateAccount('primary', { cancelled: true, subscriptionEndsAt: end }, read.revision), staleAs(ACCOUNT_STALE), 'after a rename');
  assert.equal((await storyAccount(api, 'primary')).cancelled, read.cancelled);
  read = await storyAccount(api, 'primary');
  await api.updateAccount('primary', { cancelled: false, subscriptionEndsAt: null });
  await assert.rejects(() => api.renameAccount('primary', 'y', read.revision), staleAs(ACCOUNT_STALE), 'after an end edit');
  assert.equal((await storyAccount(api, 'primary')).label, read.label);
  const secondary = await storyAccount(api, 'secondary');
  await api.pauseAccount('secondary');
  await assert.rejects(() => api.updateAccount('secondary', { cancelled: false, subscriptionEndsAt: null }, secondary.revision), staleAs(ACCOUNT_STALE), 'an end edit after a pause');
  await assert.rejects(() => api.renameAccount('secondary', 'y', secondary.revision), staleAs(ACCOUNT_STALE), 'a rename after a pause');
  const paused = await storyAccount(api, 'secondary');
  assert.deepEqual([paused.cancelled, paused.label], [secondary.cancelled, secondary.label]);
  await api.resumeAccount('secondary');
  await assert.rejects(() => api.renameAccount('secondary', 'y', paused.revision), staleAs(ACCOUNT_STALE), 'a rename after a resume');
  assert.equal((await storyAccount(api, 'secondary')).label, secondary.label);
  const funding = await storyAccount(api, 'overflow');
  await api.clearFunding('overflow');
  await assert.rejects(() => api.renameAccount('overflow', 'y', funding.revision), staleAs(ACCOUNT_STALE), 'a rename after a funding clear');
  assert.equal((await storyAccount(api, 'overflow')).label, funding.label);
  const connected = await storyAccount(api, 'legacy');
  await api.disconnectAccount('legacy');
  await assert.rejects(() => api.renameAccount('legacy', 'y', connected.revision), staleAs(ACCOUNT_STALE), 'a rename after a disconnect');
  assert.equal((await storyAccount(api, 'legacy')).label, connected.label);
});

test('a model-scoped cooldown reads its window from its reason, not a fixed model_rejection value', async () => {
  const api = await signedIn();
  const primary = api.accounts.find((a) => a.id === 'primary');
  const sample = primary.cooldowns[0];
  primary.cooldowns = [...primary.cooldowns, { ...sample, reason: 'permission', scope: 'model:claude-test-permission' }];
  const state = await api.getModelsState();
  const permitted = state.cooldowns.find((g) => g.model === 'claude-test-permission');
  assert.equal(permitted.rows[0].window, 'permission', 'a non-model_rejected reason reports its own text as the window');
  const rejected = state.cooldowns.find((g) => g.model === 'claude-haiku-4-5-20251001');
  assert.equal(rejected.rows[0].window, 'model_rejection', 'model_rejected still reports model_rejection');
});

test('the catalog union names each model once and a failed fetch contributes nothing', async () => {
  const api = await signedIn();
  const ids = (models) => models.map((m) => m.id);
  const primary = ids((await api.getAccountCatalog('primary')).models);
  assert.ok(primary.length > 0, 'the story has a catalog to union');
  assert.deepEqual(ids(await api.listCatalogOf(['primary', 'overflow', 'nightly', 'nobody'])), [...primary].sort(), 'the failing account and the unknown one add nothing');
  assert.deepEqual(await api.listCatalogOf(['nightly']), []);
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
    const accounts = await api.listAccounts();
    assert.deepEqual(accounts.filter((a) => (a.kind === 'api_key') !== (a.usage === null)).map((a) => a.id), [], `read ${read + 1}: a block on every subscription row and none on an Anthropic-key row`);
    const snapshots = accounts.flatMap((a) => (a.usage ? [a.usage] : []));
    assert.deepEqual(snapshots.map((s) => s.status).filter((s) => s !== 'error' && s !== 'success'), [], `read ${read + 1}`);
    const polls = snapshots.map((s) => s.poll);
    assert.deepEqual(polls.filter((p) => p !== null && !['clean', 'failed', 'rate_limited'].includes(p.lastOutcome)), [], `read ${read + 1}`);
    assert.ok(polls.includes(null) && polls.some((p) => p?.backoffSince), 'the story holds an account not yet polled and one backing off');
  }
});

test('the mock answers each captured bound probe with the route\'s status and sentence', async () => {
  const api = await signedIn();
  // The probes write the router's seeded pool `main`, which the story does not hold, so each pool probe replays on
  // the story pool `all` from its stored input. The rule count is left out: the mock judges a rule list with the
  // form's own check, which the form's test holds to these probes.
  const base = await storyPool(api);
  const probes = fixture('bound_refusals').probes.filter((p) => p.field !== 'model_rules');
  const camel = (s) => s.replace(/_([a-z0-9])/g, (_, c) => c.toUpperCase());
  const keepalive = (field) => camel(field.slice('keepalive.'.length));
  // The capture names under `with` what it sent beside the probed field, a retry ceiling at its base or a lead beside
  // its jitter, and the replay sends the same.
  const replay = ({ field, value, with: fields = {} }) => {
    const beside = Object.fromEntries(Object.entries(fields).map(([other, setting]) => [keepalive(other), setting]));
    if (field.startsWith('keepalive.')) return api.updatePool('all', { ...base, keepalive: { ...base.keepalive, ...beside, [keepalive(field)]: value } });
    return api.updatePool('all', { ...base, [camel(field)]: value });
  };
  // Every write waits out the mock's latency, so the probes run side by side; each sends a whole input, so none
  // reads another's.
  const answers = await Promise.allSettled(probes.map(replay));
  for (const [i, { error, field, route, status, value }] of probes.entries()) {
    const answer = answers[i];
    const label = `${route} ${field} ${value}`;
    if (status >= 200 && status < 300) {
      assert.equal(answer.status, 'fulfilled', `${label}: the route admitted it, the mock answered ${answer.reason?.message}`);
      continue;
    }
    assert.equal(answer.status, 'rejected', `${label}: the route answered ${status}`);
    assert.ok(answer.reason instanceof d.ApiError, label);
    assert.equal(answer.reason.status, status, label);
    // A value past the field's type fails the route's parse, whose sentence the capture leaves out.
    if (error !== undefined) assert.equal(answer.reason.message, error, label);
  }
});
