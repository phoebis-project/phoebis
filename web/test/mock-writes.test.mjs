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

/**
 * The pool as the dialog would send it back unchanged, so a case edits only the fields it is about. The listed pool's
 * guardrails are refs, and the dialog sends that list only when its group changed.
 */
const poolInputOf = ({ guardrails: _, ...p }) => ({ ...p, members: p.members.map((m) => ({ accountId: m.accountId, budgetDailyUsd: m.budgetDailyUsd, budgetMonthlyUsd: m.budgetMonthlyUsd, priority: m.priority })) });
const storyPool = async (api) => poolInputOf((await api.listPools()).find((p) => p.id === 'all'));
const keyInput = { autoModeServer: null, bodyCaptureHours: null, enabled: true, longContextTwins: true, modelRules: [], modelRulesMode: 'extend', name: 'fresh', notes: null, poolId: 'all' };

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
  const pool = await api.createPool({ ...(await storyPool(api)), members: [], name: '  Nightly batch!  ' });
  const long = await api.createPool({ ...(await storyPool(api)), members: [], name: 'x'.repeat(80) });
  const { key } = await api.createKey({ ...keyInput, name: 'Build box' });
  assert.deepEqual([pool.id, long.id, key.id], [d.slug('  Nightly batch!  '), 'x'.repeat(64), 'build-box']);
  for (const id of [pool.id, long.id, key.id]) assert.match(id, /^[a-z0-9][a-z0-9-]{0,63}$/);
});

test("the first replay lands the story's parked record, whose item leaves the attention answer, and a second finds nothing", async () => {
  const api = await signedIn();
  const parked = async () => (await api.getAttention()).filter((i) => i.kind === 'ledger_record_parked').length;
  assert.equal(await parked(), 1);
  assert.deepEqual(await api.replayParked(), { landed: 1, remaining: 0 });
  assert.equal(await parked(), 0);
  assert.deepEqual(await api.replayParked(), { landed: 0, remaining: 0 });
});

test("a held writer's replay answers the route's 503 and lands nothing", async () => {
  const api = await signedIn();
  // The story's switches live in the browser's storage, which Node lacks, so the case lends the mock a stand-in.
  const stored = new Map([['phoebis.mockHeldWriter', '1']]);
  globalThis.localStorage = { getItem: (key) => stored.get(key) ?? null };
  try {
    await assert.rejects(() => api.replayParked(), refusedAs(503, 'the ledger writer is held, try again'));
  } finally {
    delete globalThis.localStorage;
  }
  assert.equal((await api.getAttention()).filter((i) => i.kind === 'ledger_record_parked').length, 1, 'the parked record is still the story\'s');
});

test('a pool creation refuses the values a pool edit refuses', async () => {
  const api = await signedIn();
  const all = await storyPool(api);
  await assert.rejects(() => api.createPool({ ...all, affinityTtlSeconds: 30, keepalive: { ...all.keepalive, enabled: false }, name: 'short' }), refusedAs(400, 'affinity_ttl_seconds is 0 or at least 60'));
  await assert.rejects(() => api.createPool({ ...all, keepalive: { ...all.keepalive, maxParkedSeconds: 59 }, name: 'cold' }), refusedAs(400, 'keepalive.max_parked_seconds must be between 60 and 604800, not 59'));
  await assert.rejects(() => api.createPool({ ...all, bodyCaptureHours: 100_000, name: 'hoard' }), refusedAs(400, 'body_capture_hours exceeds stats.body_capture_max_hours'));
  await assert.rejects(() => api.createPool({ ...all, name: 'stretch', slowDownStretchSeconds: 59 }), refusedAs(400, 'slow_down_stretch_seconds is between 60 and 18000'));
  await assert.rejects(() => api.createPool({ ...all, name: 'hold', slowDownHoldSeconds: 0 }), refusedAs(400, 'slow_down_hold_seconds is between 1 and 300'));
  await assert.rejects(() => api.createPool({ ...all, name: 'mode', returnToPlan: 'sometimes' }), refusedAs(400, 'return_to_plan must be always, when_cold, or never'));
  await assert.rejects(() => api.createPool({ ...all, name: 'dwell', returnToPlanDwellSeconds: 86_401 }), refusedAs(400, 'return_to_plan_dwell_seconds is between 0 and 86400'));
  assert.equal((await api.listPools()).some((p) => ['cold', 'dwell', 'hoard', 'hold', 'mode', 'short', 'stretch'].includes(p.id)), false);
});

test('a new pool reads each member the way the pool route does', async () => {
  const api = await signedIn();
  // An account belongs to one pool, so the story's pools let every account go and each probing pool leaves once read.
  for (const p of await api.listPools()) await api.updatePool(p.id, { ...poolInputOf(p), members: [] });
  const members = (await api.listAccounts()).map((a) => ({ accountId: a.id, priority: 0 }));
  const verdicts = async (name, overagePolicy) => {
    const pool = await api.createPool({ ...(await storyPool(api)), members, name, overagePolicy });
    await api.deletePool(pool.id);
    return Object.fromEntries(pool.members.map((m) => [m.accountId, m.eligible]));
  };
  assert.deepEqual(await verdicts('first', 'never'), {
    annex: 'eligible',
    contract: 'eligible',
    legacy: 'unavailable',
    nightly: 'overage',
    overflow: 'unavailable',
    primary: 'eligible',
    research: 'eligible',
    reserve: 'eligible',
    secondary: 'cooldown',
    spare: 'unavailable',
    studio: 'eligible',
    // The sunset seed's plan ended, which the route reads as a term past its end.
    sunset: 'subscription_ended',
    workshop: 'eligible',
    // The router has not loaded the key, so it answers no projection of it.
    youngest: 'unknown',
    zenith: 'spent',
    zephyr: 'eligible',
  });
  // Two members, one half of the ended term each, since the sunset seed holds both halves at once.
  await api.updateAccount('sunset', { canceled: false, subscriptionEndsAt: null });
  await api.updateAccount('reserve', { canceled: true, subscriptionEndsAt: new Date(Date.now() - 60_000).toISOString() });
  const ended = await verdicts('ended', 'never');
  assert.equal(ended.sunset, 'subscription_ended', 'the flagged half alone: a plan-ended account with its cancel cleared reads its subscription ended');
  assert.equal(ended.reserve, 'subscription_ended', 'the canceled half alone: an unflagged account canceled past its saved end reads its subscription ended');
  // Funded again and resumed from the pause its lapse led to, overflow serves. The account the router reads in overage
  // is an obstacle only under a policy that admits none.
  await api.clearFunding('overflow');
  await api.resumeAccount('overflow');
  await api.pauseAccount('primary');
  const second = await verdicts('second', 'never');
  assert.deepEqual([second.overflow, second.nightly, second.primary], ['eligible', 'overage', 'unavailable'], 'a paused account is unavailable like a flagged one');
  assert.equal((await verdicts('third', 'prefer_plan')).nightly, 'eligible');
});

test('a pool write round-trips the pin and placement fields and the members\' budgets', async () => {
  const api = await signedIn();
  const all = await storyPool(api);
  const overflow = all.members.find((m) => m.accountId === 'overflow');
  assert.deepEqual([overflow.budgetDailyUsd, overflow.budgetMonthlyUsd], [5, 50], 'the story budgets its API-key member by the day and the month');
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
  // An account belongs to one pool, so all lets its members go before a new pool takes them, and that pool leaves
  // before the next.
  await api.updatePool('all', { ...edit, members: [] });
  const created = await api.createPool({ ...edit, name: 'budgeted' });
  assert.deepEqual(
    created.members.map((m) => [m.accountId, m.kind, m.budgetDailyUsd, m.budgetMonthlyUsd]),
    edit.members.map((m) => [m.accountId, m.accountId === 'overflow' ? 'api_key' : 'oauth', m.budgetDailyUsd, m.budgetMonthlyUsd]),
    'a create derives each member\'s kind from its account',
  );
  await api.deletePool(created.id);
  const bare = await api.createPool({ ...edit, members: [{ accountId: 'overflow', priority: 0 }], name: 'bare' });
  assert.deepEqual([bare.members[0].budgetDailyUsd, bare.members[0].budgetMonthlyUsd], [null, null], 'a member written without a budget field has no budget over that period');
});

test('a pool save reads the resolved figures off the saved members, so a member of another kind moves them', async () => {
  const api = await signedIn();
  // The work pool's subscription member alone reads the plan's figures, and its API-key member brings the dollars'.
  const work = poolInputOf((await api.listPools()).find((p) => p.id === 'work'));
  const studio = work.members.filter((m) => m.accountId === 'studio');
  assert.deepEqual(work.members.map((m) => m.accountId), ['studio', 'annex'], 'the story seeds work with a subscription member and an API-key one');
  const before = (await api.updatePool('work', { ...work, members: studio })).keepaliveResolved;
  assert.ok(before !== null, 'a pool with a member resolves its figures');
  const saved = await api.updatePool('work', { ...work, members: [...studio, { accountId: 'annex', priority: 1 }] });
  assert.notDeepEqual(saved.keepaliveResolved, before, 'an API-key member brings the dollar figures');
  assert.ok(saved.keepaliveResolved.breakEvenSeconds.min < before.breakEvenSeconds.min, `the low end reads ${saved.keepaliveResolved.breakEvenSeconds.min} against ${before.breakEvenSeconds.min}`);
});

test('a pool with no member answers exhausted while it is enabled, as the route does', async () => {
  const api = await signedIn();
  // The story's pool all is degraded, so each create states the status the dialog sends.
  const empty = await api.createPool({ ...(await storyPool(api)), members: [], name: 'empty', status: 'active' });
  assert.equal(empty.status, 'exhausted', 'a pool saved as active with no member');
  const disabled = await api.updatePool(empty.id, { ...poolInputOf(empty), status: 'disabled' }, empty.revision);
  assert.equal(disabled.status, 'disabled', 'a disabled pool with no member');
  // An API-key account is ready from its create, so the pool has a member that takes requests.
  await api.createAccount({ id: 'fresh', kind: 'api_key', label: 'fresh', secret: 'sk-ant-api03-FAKE' });
  const single = await api.createPool({ ...(await storyPool(api)), members: [{ accountId: 'fresh', priority: 0 }], name: 'single', status: 'active' });
  assert.deepEqual([single.members[0].eligible, single.status], ['eligible', 'active'], 'a pool saved as active with a member that takes requests');
  await api.deleteAccount('fresh');
  assert.equal((await api.listPools()).find((p) => p.id === single.id)?.status, 'exhausted', 'an account delete that took the last member');
});

test('a created and a rotated key answer a sixteen-character prefix', async () => {
  const api = await signedIn();
  const created = await api.createKey({ ...keyInput, name: 'prefixed' });
  const rotated = await api.rotateKey('laptop');
  for (const [name, { key, secret }] of [['created', created], ['rotated', rotated]]) {
    assert.deepEqual([key.prefix.length, key.prefix.startsWith('phoebis-'), secret.startsWith(key.prefix)], [16, true, true], `the ${name} key's prefix ${key.prefix}`);
  }
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
  // The mock reads each member's verdict once, from the story's accounts as it is built, so the flag is cleared and
  // the operator's pause lifted on the story's account before a second mock reads it, and both are restored for the
  // cases after this one.
  const seed = d.ACCOUNTS.find((a) => a.id === 'overflow');
  const [flagged, paused] = [seed.status, seed.routingEnabled];
  seed.status = 'ready';
  seed.routingEnabled = true;
  try {
    const cleared = (await (await signedIn()).listPools()).find((p) => p.id === 'all').members.find((m) => m.accountId === 'overflow');
    assert.deepEqual([cleared.budgetUntil, cleared.eligible], [null, 'eligible'], 'without the flag, a member under both its budgets is eligible');
  } finally {
    seed.status = flagged;
    seed.routingEnabled = paused;
  }
  const spent = member('jobs', 'research');
  const [day] = spent.spends;
  assert.ok(day.spentUsd >= spent.budgetDailyUsd && Date.parse(day.periodEnd) > Date.now(), 'the story spends one member past its daily budget inside the day');
  assert.deepEqual([spent.budgetUntil, spent.eligible], [day.periodEnd, 'budget'], 'the route holds it to the day\'s end, the one budget it reached');
  assert.equal(pools.find((p) => p.id === 'jobs').status, 'exhausted', 'its only member at its budget leaves the pool nothing to route to');
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

test("a key edit naming a pool that does not exist answers the route's 404 and stores nothing, and a move to a known pool lands", async () => {
  const api = await signedIn();
  const before = await api.listKeys();
  const laptop = before.find((k) => k.id === 'laptop');
  const edit = { ...keyInput, expiresAt: undefined, name: laptop.name };
  await assert.rejects(() => api.updateKey('laptop', { ...edit, poolId: 'ghost' }), refusedAs(404, 'pool not found'));
  assert.deepEqual(await api.listKeys(), before, 'a refused edit stores nothing');
  const moved = await api.updateKey('laptop', { ...edit, poolId: 'work' });
  assert.deepEqual([laptop.poolId, moved.poolId], ['all', 'work'], 'a known pool takes the key');
});

test("a key creation naming a pool that does not exist answers the route's 404 and writes no key, and one under a known pool lands", async () => {
  const api = await signedIn();
  const before = await api.listKeys();
  await assert.rejects(() => api.createKey({ ...keyInput, name: 'stray', poolId: 'ghost' }), refusedAs(404, 'pool not found'));
  assert.deepEqual(await api.listKeys(), before, 'no key was written');
  const { key } = await api.createKey({ ...keyInput, name: 'stray', poolId: 'work' });
  assert.deepEqual([key.id, key.poolId], ['stray', 'work']);
  assert.equal((await api.listKeys()).length, before.length + 1, 'the key under a known pool is written');
});

const staleAs = (message) => (e) => e instanceof d.ApiError && e.status === 409 && e.message === message && e.code === 'stale_edit';
const POOL_STALE = 'the pool changed while it was being edited, so reload and try again';
const KEY_STALE = 'the key changed while it was being edited, so reload and try again';
const storyKey = async (api, id) => (await api.listKeys()).find((k) => k.id === id);

/** The route's token: the row's incarnation above the 42 bits of its stamp. */
const tokenOf = (incarnation, updatedAt) => incarnation * 2 ** 42 + Date.parse(updatedAt);

test('a pool or key save over a revision the row no longer carries is refused as stale and stores nothing, and one over its own revision moves it', async () => {
  const api = await signedIn();
  const pool = (await api.listPools()).find((p) => p.id === 'all');
  assert.equal(pool.revision, tokenOf(1, pool.updatedAt), "the story's pool carries the token of its stamp");
  await assert.rejects(() => api.updatePool('all', { ...poolInputOf(pool), name: 'moved' }, pool.revision - 1), staleAs(POOL_STALE));
  assert.deepEqual((await api.listPools()).find((p) => p.id === 'all'), pool);
  const savedPool = await api.updatePool('all', { ...poolInputOf(pool), name: 'moved' }, pool.revision);
  assert.equal(savedPool.name, 'moved');
  assert.ok(Date.parse(savedPool.updatedAt) > Date.parse(pool.updatedAt), 'the save moves the stamp past the one it was read at');
  assert.equal(savedPool.revision, tokenOf(1, savedPool.updatedAt), 'the token moves with the stamp');
  await assert.rejects(() => api.updatePool('all', { ...poolInputOf(pool), name: 'again' }, pool.revision), staleAs(POOL_STALE), 'the stamp moved past the sent token');
  const key = await storyKey(api, 'laptop');
  assert.equal(key.revision, tokenOf(1, key.updatedAt), "the story's key carries the token of its stamp");
  const edit = { ...keyInput, expiresAt: undefined, name: 'moved', poolId: key.poolId };
  await assert.rejects(() => api.updateKey('laptop', edit, key.revision + 1), staleAs(KEY_STALE));
  assert.deepEqual(await storyKey(api, 'laptop'), key);
  const savedKey = await api.updateKey('laptop', edit, key.revision);
  assert.equal(savedKey.name, 'moved');
  assert.ok(Date.parse(savedKey.updatedAt) > Date.parse(key.updatedAt), 'the save moves the stamp past the one it was read at');
  assert.equal(savedKey.revision, tokenOf(1, savedKey.updatedAt), 'the token moves with the stamp');
  await assert.rejects(() => api.updateKey('laptop', edit, key.revision), staleAs(KEY_STALE), 'the stamp moved past the sent token');
});

test("a stale key edit onto a pool that does not exist answers the route's 404, since the route reads the pool before it compares the stamp", async () => {
  const api = await signedIn();
  const key = await storyKey(api, 'laptop');
  const edit = { ...keyInput, expiresAt: undefined, name: 'moved', poolId: key.poolId };
  await assert.rejects(() => api.updateKey('laptop', { ...edit, poolId: 'ghost' }, key.revision + 1), refusedAs(404, 'pool not found'));
  assert.deepEqual(await storyKey(api, 'laptop'), key, 'a refused edit stores nothing');
});

test("a key edit onto a pool that does not exist answers the route's 404 ahead of the rules and the doubled-guardrail refusals", async () => {
  const api = await signedIn();
  const before = await api.listKeys();
  const laptop = before.find((k) => k.id === 'laptop');
  const edit = { ...keyInput, expiresAt: undefined, name: laptop.name };
  const refusals = [
    [{ guardrails: ['fresh', 'fresh'] }, 'guardrails lists fresh twice'],
    [{ modelRulesMode: 'merge' }, 'model_rules_mode is extend or replace'],
  ];
  for (const [patch, message] of refusals) {
    await assert.rejects(() => api.updateKey('laptop', { ...edit, ...patch, poolId: 'ghost' }, laptop.revision), refusedAs(404, 'pool not found'), `${message} onto a missing pool`);
    assert.deepEqual(await api.listKeys(), before, 'a refused edit stores nothing');
    await assert.rejects(() => api.updateKey('laptop', { ...edit, ...patch, poolId: laptop.poolId }, laptop.revision), refusedAs(400, message), `${message} onto its own pool`);
    assert.deepEqual(await api.listKeys(), before, 'a refused edit stores nothing');
  }
});

test("a key edit or creation carrying a member outside the input's set answers the route's 400 and stores nothing", async () => {
  const api = await signedIn();
  const before = await api.listKeys();
  const laptop = before.find((k) => k.id === 'laptop');
  const edit = { ...keyInput, expiresAt: undefined, name: laptop.name };
  const shape = refusedAs(400, 'body must be a JSON object');
  await assert.rejects(() => api.updateKey('laptop', { ...edit, color: 'red' }), shape, 'a member of no input');
  assert.deepEqual(await api.listKeys(), before, 'a refused edit stores nothing');
  await assert.rejects(() => api.createKey({ ...keyInput, color: 'red' }), shape, 'a member of no input on a create');
  assert.deepEqual(await api.listKeys(), before, 'a refused create writes no key');
  const { key } = await api.createKey({ ...keyInput, expiresAt: null, name: 'plain' });
  assert.equal(key.id, 'plain', 'a create carrying its own expiry lands');
});

test('a pool or key save opened before a guardrail-side attachment is refused as stale, and a reread lists the guardrail', async () => {
  const api = await signedIn();
  const pool = (await api.listPools()).find((p) => p.id === 'all');
  const key = await storyKey(api, 'laptop');
  const conditions = [{ operator: 'equals', subject: 'request_class', value: 'auxiliary' }];
  const attached = await api.createGuardrail({ conditions, description: '', enabled: true, keys: ['laptop'], mode: 'observe', name: 'Fresh', pools: ['all'] });
  await assert.rejects(() => api.updatePool('all', { ...poolInputOf(pool), name: 'moved' }, pool.revision), staleAs(POOL_STALE));
  const edit = { ...keyInput, expiresAt: undefined, name: 'moved', poolId: key.poolId };
  await assert.rejects(() => api.updateKey('laptop', edit, key.revision), staleAs(KEY_STALE));
  assert.ok((await api.listPools()).find((p) => p.id === 'all').guardrails.some((r) => r.id === attached.id), 'the reread pool lists the guardrail');
  assert.ok((await storyKey(api, 'laptop')).guardrails.some((r) => r.id === attached.id), 'the reread key lists the guardrail');
});

test('a guardrail save opened before a pool-side detach is refused as stale', async () => {
  const api = await signedIn();
  const GUARDRAIL_STALE = 'the guardrail changed while it was being edited, so reload and try again';
  const read = await api.getGuardrail('subagent-status-summary');
  const pool = (await api.listPools()).find((p) => p.id === 'all');
  await api.updatePool('all', { ...poolInputOf(pool), guardrails: [] }, pool.revision);
  await assert.rejects(() => api.updateGuardrail('subagent-status-summary', { mode: 'enforce' }, read.revision), staleAs(GUARDRAIL_STALE));
  const detached = await api.getGuardrail('subagent-status-summary');
  assert.ok(detached.revision > read.revision, "the detach moves the guardrail's revision");
  const emptied = (await api.listPools()).find((p) => p.id === 'all');
  await api.updatePool('all', { ...poolInputOf(emptied), guardrails: ['subagent-status-summary'] }, emptied.revision);
  assert.ok((await api.getGuardrail('subagent-status-summary')).revision > detached.revision, "the attach moves the guardrail's revision");
});

test('an editor opened before its pool or key was deleted and created again is refused as stale and stores nothing', async () => {
  const api = await signedIn();
  const fresh = { ...(await storyPool(api)), members: [], name: 'fresh' };
  const spare = { ...keyInput, name: 'spare', poolId: 'fresh' };
  const first = await api.createPool(fresh);
  const { key: firstKey } = await api.createKey({ ...spare, expiresAt: null });
  await api.deleteKey('spare');
  await api.deletePool('fresh');
  const pool = await api.createPool(fresh);
  const { key } = await api.createKey({ ...spare, expiresAt: null });
  assert.deepEqual([pool.updatedAt, key.updatedAt], [first.updatedAt, firstKey.updatedAt], 'each replacement lands in its predecessor\'s millisecond');
  assert.deepEqual([pool.revision, key.revision], [tokenOf(2, pool.updatedAt), tokenOf(2, key.updatedAt)], 'each replacement takes the next incarnation');
  await assert.rejects(() => api.updatePool('fresh', { ...poolInputOf(pool), name: 'stale' }, first.revision), staleAs(POOL_STALE));
  await assert.rejects(() => api.updateKey('spare', { ...spare, expiresAt: undefined, name: 'stale' }, firstKey.revision), staleAs(KEY_STALE));
  assert.deepEqual([(await api.listPools()).find((p) => p.id === 'fresh').name, (await storyKey(api, 'spare')).name], ['fresh', 'spare'], 'the stale saves stored nothing');
  const landedPool = await api.updatePool('fresh', { ...poolInputOf(pool), name: 'landed' }, pool.revision);
  const landedKey = await api.updateKey('spare', { ...spare, expiresAt: undefined, name: 'landed' }, key.revision);
  assert.deepEqual([landedPool.name, landedKey.name], ['landed', 'landed'], "the replacement's own token lands");
});

test("a deleted key's session rows read historical with no pin and keep their placements, as the route lists a row no live key holds", async () => {
  const api = await signedIn();
  const laptop = async () => (await api.listSessions()).filter((s) => s.keyId === 'laptop');
  const before = await laptop();
  await api.deleteKey('laptop');
  const after = await laptop();
  assert.deepEqual([...new Set(before.map((s) => s.historical))], [false], "the story's laptop rows read live");
  assert.deepEqual([...new Set(after.map((s) => s.historical))], [true], 'each of them reads historical once the key is gone');
  // With the key's pins gone, the list reads each row's last request alone against the 3,600 s non-companion timeout.
  // The story's requests 15 m 40 s, 7 m 07 s and 3 m 30 s back keep 5eeed6ce, 7d3e1c20 and c64358b5, the last by its
  // Haiku 4.5 subagent's turn (`req_e07b3c82-d95` in mock/requests.ts). The other five laptop rows leave: 1c0f9727,
  // 76ddb35b and cd9a5904 hold drawn requests at most, which all fall over an hour back, and 0f1d7c64 and 3e9b7c21
  // hold no laptop request.
  const pins = Object.fromEntries(after.map((s) => [s.sessionId, { lines: s.agentContexts.length, pinned: s.pinned }]));
  const kept = { '5eeed6ce-bf6f-4c62-91f4-775b7fa78f23': { lines: 0, pinned: false }, '7d3e1c20-5b4a-4e8f-9c61-0a2b3c4d5e6f': { lines: 0, pinned: false }, 'c64358b5-b04f-4047-baf2-5e86716ea490': { lines: 0, pinned: false } };
  assert.deepEqual(pins, kept, 'the rows a ledger request inside the timeout keeps, each with no pin and no line');
  const placements = (rows) => Object.fromEntries(rows.map((s) => [s.sessionId, s.placements]));
  assert.deepEqual(placements(after), placements(before.filter((s) => s.sessionId in kept)), 'the delete moves no placement');
});

test("a pool deleted and created again under its id counts the pins of the keys it holds now, so it reads no former member's pin", async () => {
  const api = await signedIn();
  const poolOf = async (id) => (await api.listPools()).find((p) => p.id === id);
  const formerPins = (p) => Math.max(0, p.activePins - p.members.reduce((n, m) => n + m.activePins, 0));
  const [work, all] = [await poolOf('work'), await poolOf('all')];
  assert.ok(work.activePins > 0 && formerPins(work) > 0, "the story's work pool holds pins, one of them on a former member");
  // A pool leaves once no key points at it, so its keys move first and take their pins along.
  for (const k of (await api.listKeys()).filter((x) => x.poolId === 'work')) await api.updateKey(k.id, { ...keyInput, expiresAt: undefined, name: k.name, poolId: 'all' });
  await api.deletePool('work');
  const again = await api.createPool({ ...poolInputOf(work), members: [] });
  assert.deepEqual([again.id, again.activePins, formerPins(again)], ['work', 0, 0], 'the pool created again holds no key, so no pin');
  const moved = await poolOf('all');
  assert.equal(moved.activePins, all.activePins + work.activePins, "the pool the keys moved to counts their pins");
  // The moved keys' former-member pin sits on nightly, a member of all, and the pins on work's own members sit on
  // accounts all does not hold.
  const cells = (p) => p.members.reduce((n, m) => n + m.activePins, 0);
  assert.deepEqual(
    [cells(moved), formerPins(moved)],
    [cells(all) + formerPins(work), formerPins(all) + cells(work)],
    "each member's cell counts the pins under the pool's current keys, as the total does",
  );
});

test('a rotation or a revocation moves the key\'s stamp, so an edit read before it is refused as stale', async () => {
  const api = await signedIn();
  for (const [id, write] of [['laptop', (a) => a.rotateKey('laptop')], ['batch', (a) => a.revokeKey('batch')]]) {
    const before = await storyKey(api, id);
    await write(api);
    const after = await storyKey(api, id);
    assert.ok(Date.parse(after.updatedAt) > Date.parse(before.updatedAt), id);
    assert.equal(after.revision, tokenOf(1, after.updatedAt), id);
    // The key's own switch, since the route refuses re-enabling a revoked key ahead of the fence.
    await assert.rejects(() => api.updateKey(id, { ...keyInput, enabled: before.enabled, expiresAt: undefined, name: 'moved', poolId: before.poolId }, before.revision), staleAs(KEY_STALE), id);
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
  const created = await api.createPool({ ...unset, members: [], name: 'fresh' });
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

test("a stale pool save answers the route's member-list refusals first, then 409 stale_edit ahead of the member lookups, and stores nothing", async () => {
  const api = await signedIn();
  const before = await api.listPools();
  const all = before.find((p) => p.id === 'all');
  const input = poolInputOf(all);
  const member = (accountId, budgetDailyUsd = null) => ({ accountId, budgetDailyUsd, budgetMonthlyUsd: null, priority: 0 });
  const stale = all.revision - 1;
  // The route reads the list's own shape, a repeat and a budget, before it compares the stamp.
  await assert.rejects(() => api.updatePool('all', { ...input, members: [member('overflow'), member('overflow')] }, stale), refusedAs(409, 'members name account overflow twice'), 'a repeat');
  await assert.rejects(() => api.updatePool('all', { ...input, members: [member('overflow', 0)] }, stale), refusedAs(400, 'budget_daily_usd is above 0'), 'a budget');
  // The store looks a member up only once the stamp has passed: an unknown member and an account the work pool holds.
  const lookups = [[...input.members, member('ghost')], [...input.members, member('studio')]];
  for (const members of lookups) {
    await assert.rejects(() => api.updatePool('all', { ...input, members }, stale), staleAs(POOL_STALE), members.map((m) => m.accountId).join());
  }
  assert.deepEqual(await api.listPools(), before, 'neither the settings nor the members were stored');
  await assert.rejects(() => api.updatePool('all', { ...input, members: lookups[0] }, all.revision), refusedAs(404, 'account ghost not found'), 'over its own revision the member refusal answers');
  await assert.rejects(() => api.updatePool('all', { ...input, members: lookups[1] }, all.revision), refusedAs(409, 'account studio is a member of pool work'), 'over its own revision the held member answers');
});

test("a pool save naming an account another pool holds answers the route's 409 and stores nothing", async () => {
  const api = await signedIn();
  const before = await api.listPools();
  const work = poolInputOf(before.find((p) => p.id === 'work'));
  const primary = { accountId: 'primary', budgetDailyUsd: null, budgetMonthlyUsd: null, priority: 0 };
  const held = refusedAs(409, 'account primary is a member of pool all');
  await assert.rejects(() => api.createPool({ ...work, members: [primary], name: 'third' }), held, 'on create');
  await assert.rejects(() => api.updatePool('work', { ...work, members: [...work.members, primary] }), held, 'on edit');
  assert.deepEqual(await api.listPools(), before, 'neither the settings nor the members were stored');
});

test('an account answers the pool a member write left it in', async () => {
  const api = await signedIn();
  const pools = await api.listPools();
  const all = poolInputOf(pools.find((p) => p.id === 'all'));
  const work = poolInputOf(pools.find((p) => p.id === 'work'));
  const poolOf = async (id) => (await api.listAccounts()).find((a) => a.id === id).pool;
  assert.deepEqual(await poolOf('primary'), { poolId: 'all', priority: 0 });
  await api.updatePool('all', { ...all, members: all.members.filter((m) => m.accountId !== 'primary') });
  assert.equal(await poolOf('primary'), null, 'out of all, primary is in no pool');
  await api.updatePool('work', { ...work, members: [...work.members, { accountId: 'primary', budgetDailyUsd: null, budgetMonthlyUsd: null, priority: 3 }] });
  assert.deepEqual(await poolOf('primary'), { poolId: 'work', priority: 3 });
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
  const { key } = await api.createKey({ ...bare, expiresAt: null, name: 'bare' });
  assert.deepEqual([key.modelRules, key.modelRulesMode, key.longContextTwins], [[], 'extend', true]);
  const laptop = (await api.listKeys()).find((k) => k.id === 'laptop');
  const edited = await api.updateKey('laptop', { ...bare, name: 'laptop' });
  assert.deepEqual([edited.modelRules, edited.modelRulesMode, edited.longContextTwins], [laptop.modelRules, laptop.modelRulesMode, laptop.longContextTwins]);
  const { modelRules: _p, ...unruled } = await storyPool(api);
  const pool = await api.createPool({ ...unruled, members: [], name: 'unruled' });
  assert.deepEqual(pool.modelRules, [], 'a pool created without a list starts with none');
});

test('an account patch is refused where the account route refuses it, and a canceled end stands only with its flag', async () => {
  const api = await signedIn();
  const end = '2026-10-19T09:15:00.000Z';
  const before = await api.listAccounts();
  await assert.rejects(() => api.updateAccount('overflow', { canceled: true, subscriptionEndsAt: end }), refusedAs(400, 'only an oauth account is canceled'));
  await assert.rejects(() => api.updateAccount('nobody', { canceled: false, subscriptionEndsAt: null }), refusedAs(404, 'account not found'));
  assert.deepEqual(await api.listAccounts(), before, 'a refused patch stores nothing');
  const canceled = await api.updateAccount('primary', { canceled: true, subscriptionEndsAt: end });
  assert.deepEqual([canceled.canceled, canceled.subscription.endsAt], [true, end]);
  const restored = await api.updateAccount('primary', { canceled: false, subscriptionEndsAt: null });
  assert.deepEqual([restored.canceled, restored.subscription.endsAt], [false, null], 'the end leaves with the flag');
  const unpaired = await api.updateAccount('spare', { canceled: true, subscriptionEndsAt: end });
  assert.equal(unpaired.subscription?.endsAt, end, 'a disconnected OAuth account still reports the end it was canceled with');
});

const ACCOUNT_STALE = 'the account changed while it was being edited, so reload and try again';
const storyAccount = async (api, id) => (await api.listAccounts()).find((a) => a.id === id);

test('an account save over a revision the row no longer carries is refused as stale and stores nothing, and one over its own revision moves it', async () => {
  const api = await signedIn();
  const patch = { canceled: true, subscriptionEndsAt: '2026-10-19T09:15:00.000Z' };
  const listed = await api.listAccounts();
  const before = listed.find((a) => a.id === 'primary');
  await assert.rejects(() => api.updateAccount('primary', patch, before.revision + 1), staleAs(ACCOUNT_STALE));
  assert.deepEqual(await api.listAccounts(), listed, 'a stale save stores nothing');
  const landed = await api.updateAccount('primary', patch, before.revision);
  assert.deepEqual([landed.canceled, landed.revision], [true, before.revision + 1]);
  await assert.rejects(() => api.updateAccount('primary', patch, before.revision), staleAs(ACCOUNT_STALE), 'the counter moved past the sent revision');
  await assert.rejects(() => api.renameAccount('primary', 'x', landed.revision + 1), staleAs(ACCOUNT_STALE));
  assert.equal((await storyAccount(api, 'primary')).label, before.label);
  const renamed = await api.renameAccount('primary', 'x', landed.revision);
  assert.deepEqual([renamed.label, renamed.revision], ['x', landed.revision + 1]);
  const created = await api.createAccount({ id: 'fresh', kind: 'oauth', label: 'fresh', secret: null });
  assert.equal(created.revision, 2 ** 32 + 1, 'a new row starts at the token the route gives it');
});

test('an editor opened before its account was deleted and created again is refused as stale and stores nothing', async () => {
  const api = await signedIn();
  const first = await api.createAccount({ id: 'fresh', kind: 'oauth', label: 'fresh', secret: null });
  await api.deleteAccount('fresh');
  const replacement = await api.createAccount({ id: 'fresh', kind: 'oauth', label: 'again', secret: null });
  assert.notEqual(replacement.revision, first.revision, 'the replacement never answers a token its predecessor did');
  await assert.rejects(() => api.renameAccount('fresh', 'stale', first.revision), staleAs(ACCOUNT_STALE));
  await assert.rejects(() => api.updateAccount('fresh', { canceled: true, subscriptionEndsAt: '2026-10-19T09:15:00.000Z' }, first.revision), staleAs(ACCOUNT_STALE));
  const stored = await storyAccount(api, 'fresh');
  assert.deepEqual([stored.label, stored.canceled, stored.revision], ['again', replacement.canceled, replacement.revision], 'the stale saves stored nothing');
});

test("an account's deletion sets the key's word that names it to none and keeps the key's other word", async () => {
  const api = await signedIn();
  await api.createAccount({ id: 'fresh', kind: 'oauth', label: 'fresh', secret: null });
  await api.createKey({ ...keyInput, featureAccounts: { artifacts: { accountId: 'fresh', mode: 'account' }, connectors: { accountId: 'primary', mode: 'account' } }, name: 'named' });
  await api.deleteAccount('fresh');
  const key = (await api.listKeys()).find((k) => k.id === 'named');
  assert.deepEqual(key?.featureAccounts, { artifacts: { accountId: null, mode: 'none' }, connectors: { accountId: 'primary', mode: 'account' } });
});

test('a pairing started for a new account writes its row at the confirm and none at a cancel', async () => {
  const api = await signedIn();
  const before = (await api.listAccounts()).length;
  const started = await api.startPairing({ id: 'fresh', label: 'Fresh' });
  await api.completePairing(started.id, { code: 'code', state: started.state });
  await api.confirmPairing(started.id);
  const created = await storyAccount(api, 'fresh');
  assert.deepEqual([created?.kind, created?.label, created?.status, created?.email], ['oauth', 'Fresh', 'ready', 'operator+new@example.com'], 'the confirm writes the row ready with the identity it paired');
  assert.deepEqual([typeof created?.credential.accessTokenExpiresAt, typeof created?.credential.refreshTokenExpiresAt], ['string', 'string'], 'the confirm stores both expiries the tokens it exchanged carry');
  const canceled = await api.startPairing({ id: 'fresh2', label: 'Fresh 2' });
  await api.cancelPairing(canceled.id);
  assert.equal(await storyAccount(api, 'fresh2'), undefined, 'a cancel leaves no row');
  await assert.rejects(() => api.startPairing({ id: 'primary', label: 'x' }), refusedAs(409, 'an account with this id exists'), 'a start under a listed id is refused at once');
  const raced = await api.startPairing({ id: 'fresh3', label: 'Fresh 3' });
  const taken = await api.createAccount({ id: 'fresh3', kind: 'oauth', label: 'Taken first', secret: null });
  await api.completePairing(raced.id, { code: 'code', state: raced.state });
  await assert.rejects(() => api.confirmPairing(raced.id), refusedAs(409, 'an account with this id exists'), 'a confirm whose id a create took meanwhile is refused');
  assert.deepEqual(await storyAccount(api, 'fresh3'), taken, 'the created row is unchanged, unpaired and without an email');
  assert.equal((await api.listAccounts()).length, before + 2, 'the confirmed row and the created row are the two that landed');
});

test('a confirmed re-pair keeps a plan-ended flag and starts no first poll where the stored profile names a free organization, clears it and records the plan found again where the profile reports a plan, and records nothing for an unflagged account', async () => {
  // The case holds the clock the mock reads and steps it past the first poll's few seconds instead of waiting.
  const wall = Date.now;
  let now = wall();
  Date.now = () => now;
  try {
    const api = await signedIn();
    const repair = async (accountId) => {
      const started = await api.startPairing({ accountId });
      await api.completePairing(started.id, { code: 'code', state: started.state });
      return api.confirmPairing(started.id);
    };
    // Sunset's stored document names the free organization its ended plan left, so the confirm keeps the flag, and the
    // poll's schedule reads no flagged account, so no first reading lands on the row.
    await repair('sunset');
    now += 2_200;
    assert.deepEqual((await storyAccount(api, 'sunset')).windows, [], 'no first reading lands on the flagged row past the poll');
    // A paid organization reports a plan. The row and its document are new objects, since the seed's document is the
    // one every mock of the process shares.
    const flagged = api.accounts.find((a) => a.id === 'sunset');
    api.putAccount({ ...flagged, subscription: { ...flagged.subscription, organizationType: 'claude_pro' } });
    const cleared = await repair('sunset');
    assert.equal((await storyAccount(api, 'sunset')).planEndedAt, null, 'the confirm clears the flag');
    // The router reads the first poll after the clear, so a confirm that clears the flag starts one.
    assert.equal(cleared.firstPollInFlight, true, 'a confirm that clears the flag starts the first poll');
    await repair('reserve');
    const { rows } = await api.listRoutingLog({ kinds: ['plan_restored'] });
    // The router writes the row from the confirmed profile: the paid organization on sunset's seed tier.
    assert.deepEqual(
      rows.map((e) => [e.kind, e.accountId, e.detail]),
      [['plan_restored', 'sunset', { organization_type: 'claude_pro', rate_limit_tier: 'default_claude_ai', source: 'pairing', subscription_status: 'active' }]],
    );
  } finally {
    Date.now = wall;
  }
});

test("a re-pair of a held account clears the row's limit resets and moves the revision of its resets, and a read during the poll's backoff keeps them cleared", async () => {
  const api = await signedIn();
  const repair = async (accountId) => {
    const started = await api.startPairing({ accountId });
    await api.completePairing(started.id, { code: 'code', state: started.state });
    return api.confirmPairing(started.id);
  };
  // Nightly holds a reset block, which the router's pairing clears from the store while it moves the block's epoch.
  const before = (await api.readLimitResets('nightly')).limitResets.revision;
  await repair('nightly');
  assert.equal((await storyAccount(api, 'nightly')).limitResets, null, 'the confirm clears the limit resets the row read');
  // The use names no token, so only the route's read of its stored block can refuse it.
  await assert.rejects(() => api.useLimitReset('nightly', 'rg-nightly-1'), (e) => e instanceof d.ApiError && e.status === 409 && e.code === 'limit_reset_not_usable', 'a use before the next read finds no stored block');
  const after = (await api.readLimitResets('nightly')).limitResets.revision;
  assert.notEqual(after, before, 'the next read answers a revision no token from before the pairing matches');
  // Secondary's poll waits out a backoff that ends minutes past the story's clock, so the route answers the stored
  // block, which the pairing cleared. The case holds the clock the mock reads a minute inside that backoff, so a slow
  // run cannot carry the read past its end.
  const next = Date.parse((await storyAccount(api, 'secondary')).usage.poll.nextAttemptAt);
  const wall = Date.now;
  Date.now = () => next - 60_000;
  try {
    await repair('secondary');
    const held = await api.readLimitResets('secondary');
    assert.deepEqual([held.error, held.limitResets, (await storyAccount(api, 'secondary')).limitResets], ['the account is waiting out a rate limit', null, null], "a read inside the backoff answers the backoff's error and no member, and stores none");
  } finally {
    Date.now = wall;
  }
});

test("a re-pair's confirm clears the held row's flag, so spare, routed with no cooldown, reads ready and legacy, whose routing is off, reads paused", async () => {
  const api = await signedIn();
  const repair = async (accountId) => {
    const started = await api.startPairing({ accountId });
    await api.completePairing(started.id, { code: 'code', state: started.state });
    return api.confirmPairing(started.id);
  };
  // The story seals legacy's credential under another storage key, so its re-pair confirms after a disconnect.
  await api.disconnectAccount('legacy');
  await repair('spare');
  await repair('legacy');
  assert.deepEqual([(await storyAccount(api, 'spare')).status, (await storyAccount(api, 'legacy')).status], ['ready', 'paused']);
});

test("a second re-pair of legacy after its disconnect and a confirm confirms, since the confirm stored a credential under the key in use", async () => {
  const api = await signedIn();
  const repair = async () => {
    const started = await api.startPairing({ accountId: 'legacy' });
    await api.completePairing(started.id, { code: 'code', state: started.state });
    return api.confirmPairing(started.id);
  };
  // The disconnect deletes the credential the story sealed under an earlier storage key, so the first re-pair
  // confirms and stores the tokens it exchanged.
  await api.disconnectAccount('legacy');
  await repair();
  assert.equal((await repair()).status, 'confirmed', 'the next re-pair confirms over the credential the last one stored');
});

test('a disconnected account paired again reads both features in its grants, which the scopes of its new sign-in cover', async () => {
  const api = await signedIn();
  // The story seals legacy's credential under another storage key, so its re-pair confirms after a disconnect, which
  // leaves the row no grant.
  await api.disconnectAccount('legacy');
  const started = await api.startPairing({ accountId: 'legacy' });
  await api.completePairing(started.id, { code: 'code', state: started.state });
  await api.confirmPairing(started.id);
  assert.deepEqual((await storyAccount(api, 'legacy')).featureGrants, ['artifacts', 'connectors']);
});

test('a rename, an end edit, a pause, a resume, a funding clear or a disconnect moves the account\'s revision, so an edit read before it is refused as stale', async () => {
  const api = await signedIn();
  const end = '2026-10-19T09:15:00.000Z';
  let read = await storyAccount(api, 'primary');
  await api.renameAccount('primary', 'x');
  await assert.rejects(() => api.updateAccount('primary', { canceled: true, subscriptionEndsAt: end }, read.revision), staleAs(ACCOUNT_STALE), 'after a rename');
  assert.equal((await storyAccount(api, 'primary')).canceled, read.canceled);
  read = await storyAccount(api, 'primary');
  await api.updateAccount('primary', { canceled: false, subscriptionEndsAt: null });
  await assert.rejects(() => api.renameAccount('primary', 'y', read.revision), staleAs(ACCOUNT_STALE), 'after an end edit');
  assert.equal((await storyAccount(api, 'primary')).label, read.label);
  const secondary = await storyAccount(api, 'secondary');
  await api.pauseAccount('secondary');
  await assert.rejects(() => api.updateAccount('secondary', { canceled: false, subscriptionEndsAt: null }, secondary.revision), staleAs(ACCOUNT_STALE), 'an end edit after a pause');
  await assert.rejects(() => api.renameAccount('secondary', 'y', secondary.revision), staleAs(ACCOUNT_STALE), 'a rename after a pause');
  const paused = await storyAccount(api, 'secondary');
  assert.deepEqual([paused.canceled, paused.label], [secondary.canceled, secondary.label]);
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

test("a disconnect keeps the row's last reading in its usage block, with no error and no poll", async () => {
  const api = await signedIn();
  // The 5-hour window, listed first, read twenty minutes before the 7-day one, so only the newest instant reads 11:30.
  const reserve = api.accounts.find((a) => a.id === 'reserve');
  reserve.windows = reserve.windows.map((w) => ({ ...w, observedAt: w.id === '5h' ? '2026-09-14T11:10:00.000Z' : '2026-09-14T11:30:00.000Z' }));
  await api.disconnectAccount('reserve');
  const { usage } = await storyAccount(api, 'reserve');
  assert.deepEqual([usage.error, usage.fetchedAt, usage.poll], [null, '2026-09-14T11:30:00.000Z', null]);
});

test("a disconnect of an account that needs re-auth keeps the instant of the last reading its answer withholds", async () => {
  const api = await signedIn();
  const read = (await storyAccount(api, 'legacy')).usage.fetchedAt;
  await api.disconnectAccount('legacy');
  const { usage } = await storyAccount(api, 'legacy');
  assert.equal(typeof read, 'string', 'legacy names the instant of its last clean read before the disconnect');
  assert.deepEqual([usage.error, usage.fetchedAt, usage.poll], [null, read, null]);
});

test('a first reading lands on no row that a disconnect left without a credential after its confirm', async () => {
  // The case holds the clock the mock reads and steps it past the first poll's few seconds instead of waiting.
  const wall = Date.now;
  let now = wall();
  Date.now = () => now;
  try {
    const api = await signedIn();
    const started = await api.startPairing({ accountId: 'spare' });
    await api.completePairing(started.id, { code: 'code', state: started.state });
    await api.confirmPairing(started.id);
    await api.disconnectAccount('spare');
    now += 2_200;
    const spare = await storyAccount(api, 'spare');
    assert.deepEqual([spare.windows, spare.fableWindow], [[], null]);
  } finally {
    Date.now = wall;
  }
});

test('a funding clear on a paused account leaves it paused', async () => {
  const api = await signedIn();
  await api.pauseAccount('overflow');
  await api.clearFunding('overflow');
  assert.equal((await storyAccount(api, 'overflow')).status, 'paused');
});

test('a paused account whose last cooldown clears stays paused', async () => {
  const api = await signedIn();
  // Primary's other row holds the fast lane alone, which cools none of its standard traffic.
  await api.pauseAccount('primary');
  await api.clearCooldown('primary', 'model:claude-haiku-4-5-20251001');
  assert.equal((await storyAccount(api, 'primary')).status, 'paused');
});

test('a model-scoped cooldown reads its window from its reason, not a fixed model_rejection value', async () => {
  const api = await signedIn();
  const primary = api.accounts.find((a) => a.id === 'primary');
  const sample = primary.cooldowns[0];
  // laptop's rules allow both models among the four ahead of their closing deny, and primary is a member of laptop's
  // pool.
  primary.cooldowns = [...primary.cooldowns, { ...sample, reason: 'permission', scope: 'model:claude-sonnet-4-5' }];
  const { rules } = await api.getKeyModels('laptop');
  const windowOn = (pattern) => rules.find((r) => r.pattern === pattern && !r.not)?.cooldowns.find((c) => c.accountId === 'primary')?.window;
  assert.equal(windowOn('claude-sonnet-4-5'), 'permission', 'a non-model_rejected reason reports its own text as the window');
  assert.equal(windowOn('claude-haiku-4-5-20251001'), 'model_rejection', 'model_rejected still reports model_rejection');
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

test('an unknown id is refused with the route\'s own 404 text, which the console shows as the toast body', async () => {
  const api = await signedIn();
  await assert.rejects(() => api.renameAccount('nobody', 'x'), refusedAs(404, 'account not found'));
  await assert.rejects(() => api.rotateKey('nothing'), refusedAs(404, 'key not found'));
});

test('every subscription row carries a usage block and no API-key row does', async () => {
  const api = await signedIn();
  for (let read = 0; read < 2; read += 1) {
    const accounts = await api.listAccounts();
    assert.deepEqual(accounts.filter((a) => (a.kind === 'api_key') !== (a.usage === null)).map((a) => a.id), [], `read ${read + 1}: a block on every subscription row and none on an API-key row`);
  }
});

test('the mock answers each captured bound probe with the route\'s status and sentence', async () => {
  const api = await signedIn();
  // The probes write the router's seeded pool `main`, which the story does not hold, so each pool probe replays on
  // the story pool `all` from its stored input. The rule count is left out: the mock judges a rule list with the
  // form's own check, which the form's test holds to these probes.
  const base = await storyPool(api);
  const probes = fixture('bound_refusals').probes.filter((p) => p.field !== 'model_rules');
  assert.deepEqual(['five_hour_ceiling', 'shed_cap', 'weekly_ceiling'].filter((field) => !probes.some((p) => p.field === field)), [], 'the three fractions are among the probes');
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

test("the mock answers a two-field mistake with the route's first sentence, the 5-hour ceiling before the weekly one and both before the cap, the average and the dwell, the wind-down before the stretch, the hold, the mode and the return dwell", async () => {
  const api = await signedIn();
  const base = await storyPool(api);
  const slowed = { returnToPlan: 'sometimes', returnToPlanDwellSeconds: 86_401, slowDownHoldSeconds: 0, slowDownStretchSeconds: 59 };
  const cases = [
    [{ fiveHourCeiling: 0.4, weeklyCeiling: 0.4 }, 'five_hour_ceiling is between 0.5 and 1.0'],
    [{ burnAverageSeconds: 599, fiveHourCeiling: 0.4, shedCap: 0.05, shedDwellSeconds: 299 }, 'five_hour_ceiling is between 0.5 and 1.0'],
    [{ shedCap: 0.05, weeklyCeiling: 0.4 }, 'weekly_ceiling is between 0.5 and 1.0'],
    [{ burnAverageSeconds: 599, shedCap: 0.05, shedDwellSeconds: 299 }, 'shed_cap is between 0.1 and 1.0'],
    [{ burnAverageSeconds: 599, shedDwellSeconds: 299 }, 'burn_average_seconds is between 600 and 7200'],
    [{ ...slowed, windDownMinutes: 10_081 }, 'wind_down_minutes is at most 10080 (one week)'],
    [slowed, 'slow_down_stretch_seconds is between 60 and 18000'],
    [{ ...slowed, slowDownStretchSeconds: 60 }, 'slow_down_hold_seconds is between 1 and 300'],
    [{ ...slowed, slowDownHoldSeconds: 1, slowDownStretchSeconds: 60 }, 'return_to_plan must be always, when_cold, or never'],
    [{ ...slowed, returnToPlan: 'never', slowDownHoldSeconds: 1, slowDownStretchSeconds: 60 }, 'return_to_plan_dwell_seconds is between 0 and 86400'],
  ];
  for (const [patch, error] of cases) await assert.rejects(() => api.updatePool('all', { ...base, ...patch }), refusedAs(400, error), JSON.stringify(patch));
});

test('a_guardrail_is_created_edited_attached_and_deleted_on_the_mock_with_the_routes_refusals', async () => {
  const api = await signedIn();
  const GUARDRAIL_STALE = 'the guardrail changed while it was being edited, so reload and try again';
  const conditions = [{ operator: 'equals', subject: 'request_class', value: 'auxiliary' }];
  const input = { conditions, description: '', enabled: true, mode: 'observe', name: 'Fresh' };
  const ids = async () => (await api.listGuardrails()).map((g) => g.id);
  const listOf = async (pool) => (await api.listPools()).find((p) => p.id === pool).guardrails.map((r) => [r.id, r.position]);
  const before = await ids();
  await assert.rejects(() => api.createGuardrail({ ...input, name: 'Large auxiliary' }), refusedAs(409, 'a guardrail with this id exists'));
  await assert.rejects(() => api.createGuardrail({ ...input, name: 'Test' }), refusedAs(400, 'id is reserved'));
  await assert.rejects(() => api.createGuardrail({ ...input, conditions: [] }), refusedAs(400, 'conditions holds 1 to 16 entries'));
  await assert.rejects(() => api.createGuardrail({ ...input, conditions: [...conditions, { operator: 'present', subject: 'header' }] }), refusedAs(400, 'conditions[1]: name is required'));
  await assert.rejects(() => api.createGuardrail({ ...input, keys: ['ghost'] }), refusedAs(404, 'key ghost not found'));
  await assert.rejects(() => api.createGuardrail({ ...input, name: 'Model: Opus Only' }), refusedAs(400, 'name must not contain "model:"'));
  await assert.rejects(() => api.createGuardrail({ ...input, name: 'oauth TOKEN has been REVOKED' }), refusedAs(400, 'name must not contain "OAuth token has been revoked"'));
  await assert.rejects(() => api.createGuardrail({ ...input, name: 'Prompt Is Too Long!' }), refusedAs(400, 'name must not contain "prompt is too long"'));
  assert.deepEqual(await ids(), before, 'a refused create stores nothing');

  const fresh = await api.createGuardrail({ ...input, keys: ['laptop'], pools: ['all'] });
  assert.deepEqual([fresh.id, fresh.preset, fresh.pools, fresh.keys], ['fresh', null, [{ id: 'all', label: 'all', position: 1 }], [{ id: 'laptop', label: 'laptop', position: 0 }]], 'a new guardrail joins the end of each list it names');
  const laptop = (await api.listKeys()).find((k) => k.id === 'laptop');
  assert.deepEqual([laptop.poolGuardrails.map((r) => r.id), laptop.guardrails.map((r) => r.id)], [['subagent-status-summary', 'fresh'], ['fresh']], "a key reads its pool's list apart from its own");
  const other = { ...(await storyPool(api)), guardrails: ['fresh'], members: [], name: 'other' };
  await assert.rejects(() => api.createPool(other), refusedAs(400, 'guardrails applies on edit'));

  await assert.rejects(() => api.updateGuardrail('fresh', { mode: 'enforce' }, fresh.revision - 1), staleAs(GUARDRAIL_STALE));
  await assert.rejects(() => api.updateGuardrail('fresh', { presetVersion: 1 }), refusedAs(400, 'guardrail has no preset'));
  await assert.rejects(() => api.updateGuardrail('fresh', { preset: 'subagent-status-summary' }), refusedAs(400, 'preset applies on create'));
  await assert.rejects(() => api.updateGuardrail('ghost', { mode: 'enforce' }), refusedAs(404, 'guardrail not found'));
  const enforced = await api.updateGuardrail('fresh', { mode: 'enforce' }, fresh.revision);
  assert.equal(enforced.mode, 'enforce');
  assert.ok(enforced.revision > fresh.revision, 'the save moves the revision');

  const all = (await api.listPools()).find((p) => p.id === 'all');
  await assert.rejects(() => api.updatePool('all', { ...poolInputOf(all), guardrails: ['fresh', 'fresh'] }), refusedAs(400, 'guardrails lists fresh twice'));
  await assert.rejects(() => api.updatePool('all', { ...poolInputOf(all), guardrails: ['ghost'] }), refusedAs(404, 'guardrail ghost not found'));
  const saved = await api.updatePool('all', { ...poolInputOf(all), guardrails: ['fresh', 'subagent-status-summary'] }, all.revision);
  assert.deepEqual(saved.guardrails.map((r) => [r.id, r.position]), [['fresh', 0], ['subagent-status-summary', 1]], "the pool's side replaces its list whole");
  assert.deepEqual((await api.getGuardrail('subagent-status-summary')).pools, [{ id: 'all', label: 'all', position: 1 }]);
  await api.updateGuardrail('fresh', { pools: [] }, enforced.revision);
  assert.deepEqual(await listOf('all'), [['subagent-status-summary', 1]], 'a detach from the guardrail side keeps the other positions');

  const many = [];
  for (let i = 0; i < 64; i += 1) many.push((await api.createGuardrail({ ...input, name: `bound ${i}` })).id);
  const pool = (await api.listPools()).find((p) => p.id === 'all');
  await assert.rejects(() => api.updatePool('all', { ...poolInputOf(pool), guardrails: [...many, 'fresh'] }), refusedAs(400, 'pool all would hold more than 64 guardrails'));
  await assert.rejects(() => api.updateKey('laptop', { ...keyInput, expiresAt: undefined, guardrails: many, name: 'laptop' }), refusedAs(400, 'key laptop would hold more than 64 guardrails'));
  assert.deepEqual(await listOf('all'), [['subagent-status-summary', 1]], 'a refused attachment stores nothing');
  assert.deepEqual((await api.listKeys()).find((k) => k.id === 'laptop').guardrails.map((r) => r.id), ['fresh']);

  // Attached again ahead of the preset's row, so the delete has a pool side and an entry after it whose position moves
  // if the delete renumbers the list.
  const again = (await api.listPools()).find((p) => p.id === 'all');
  await api.updatePool('all', { ...poolInputOf(again), guardrails: ['fresh', 'subagent-status-summary'] }, again.revision);
  assert.deepEqual(await listOf('all'), [['fresh', 0], ['subagent-status-summary', 1]]);
  await api.deleteGuardrail('fresh');
  assert.deepEqual(await listOf('all'), [['subagent-status-summary', 1]], "the delete takes the guardrail off the pool's list and keeps the other entry's position");
  await assert.rejects(() => api.getGuardrail('fresh'), refusedAs(404, 'guardrail not found'));
  const keys = await api.listKeys();
  assert.ok(keys.every((k) => k.guardrails.every((r) => r.id !== 'fresh')), 'the delete detaches the guardrail from every key');
});

test("a login with Stay logged in checked keeps the flag in the signed-in mark for a reload, and the smoke's bare mark reads a login left unchecked", async () => {
  // The mark lives in the browser's storage, which the case lends the mock as a stand-in that a reload reads.
  const stored = new Map();
  globalThis.localStorage = { getItem: (key) => stored.get(key) ?? null, removeItem: (key) => stored.delete(key), setItem: (key, value) => stored.set(key, String(value)) };
  try {
    await new d.MockApi().login('admin', true);
    // A reload builds the mock anew, whose session reads the mark alone.
    assert.deepEqual(await new d.MockApi().getSession(), { persistent: true, sessionId: 'mock-admin' }, 'a reload after a checked login');
    stored.set('phoebis.mockSession', 'admin');
    assert.deepEqual(await new d.MockApi().getSession(), { persistent: false, sessionId: 'mock-admin' }, 'the bare mark the smoke writes');
  } finally {
    delete globalThis.localStorage;
  }
});
