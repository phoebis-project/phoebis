import assert from 'node:assert/strict';
import { test } from 'node:test';
import { sampleDriven, selectionOrder } from './dist/api.js';

const account = (id, over = {}) => ({
  catalogSyncedAt: null,
  cooldowns: [],
  credential: { accessTokenExpiresAt: null, lastError: null, lastRefreshAt: null, refreshTokenExpiresAt: null },
  email: null,
  extraUsage: null,
  fableWindow: null,
  id,
  kind: 'oauth',
  label: id,
  plan: null,
  pools: [],
  requests: { last24h: 0, last7d: 0, lifetime: 0 },
  routingEnabled: true,
  servedModels: [],
  status: 'ready',
  subscription: null,
  windows: [],
  ...over,
});
const window5h = (utilization) => [{ id: '5h', label: '5-hour', observedAt: '2026-09-18T00:00:00.000Z', periodHours: 5, resetsAt: '2026-09-18T05:00:00.000Z', utilization }];
const window7d = (resetsAt) => [{ id: '7d', label: '7-day', observedAt: '2026-09-18T00:00:00.000Z', periodHours: 168, resetsAt, utilization: 0.5 }];
const inOverage = { disabledReason: null, enabled: true, inUse: true, monthlyLimitCredits: 100, usedCredits: 10 };
// The eligibility verdict rides on the member, because the router computes it against its own live cooldowns and
// overages; a case states it there rather than re-deriving it from the account.
const member = (accountId, priority, eligible = 'eligible') => ({ accountId, eligible, priority });
const pool = (strategy, overagePolicy, members, over = {}) => {
  const merged = {
    activePins: 0,
    affinityTtlSeconds: 600,
    apiKeySubStrategy: 'priority_random',
    bodyCaptureHours: 0,
    cacheTtl: null,
    cost24h: 0,
    id: 'p',
    keepalive: {
      coldProbeLimit: 2,
      costCapUsdPerDay: null,
      enabled: false,
      jitterSeconds1h: 30,
      jitterSeconds5m: 5,
      leadSeconds1h: 360,
      leadSeconds5m: 60,
      maxIdleSeconds: 3600,
      maxIdleTicks: 0,
      netNegativeGraceProbes: 2,
      retryMaxSeconds: 300,
      retrySeconds: 30,
      stopWhenNetNegative: true,
      warmSubagents: null,
    },
    members,
    migrateAboveUtilization: null,
    name: 'p',
    overagePolicy,
    preferSubscription: false,
    requests24h: 0,
    status: 'active',
    strategy,
    traffic24h: [],
    ...over,
  };
  // The route answers the effective flag, never the operator alone, so a case states the stored one and gets both.
  return { ...merged, preferSubscriptionEffective: merged.preferSubscription || sampleDriven(merged.strategy) };
};
const accounts = [account('over', { extraUsage: inOverage }), account('plan')];

test('an overage P0 beside a plan-funded P1 follows each overage policy', () => {
  const tiers = [member('over', 0), member('plan', 1)];
  const prefer = selectionOrder(pool('priority_random', 'prefer_plan', tiers), accounts, []);
  assert.deepEqual(prefer[0].next, ['over'], 'prefer_plan keeps the overage account eligible and the router lands on it, since no plan-funded sibling shares its tier');
  assert.equal(prefer[0].placements[0].deferred, true);
  const never = selectionOrder(pool('priority_random', 'never', [member('over', 0, 'overage'), member('plan', 1)]), accounts, []);
  assert.deepEqual(never[0].next, [], 'never makes overage an obstacle');
  assert.equal(never[0].placements[0].ineligible, 'overage');
  assert.deepEqual(never[1].next, ['plan'], 'so the next fresh session lands one tier down');
  const allow = selectionOrder(pool('priority_random', 'allow', tiers), accounts, []);
  assert.deepEqual(allow[0].next, ['over']);
  assert.equal(allow[0].placements[0].deferred, false, 'allow ignores overage');
  const sameTier = selectionOrder(pool('priority_random', 'prefer_plan', [member('over', 0), member('plan', 0)]), accounts, []);
  assert.deepEqual(sameTier[0].next, ['plan'], 'inside one tier the plan-funded member ranks ahead of the deferred one');
  assert.deepEqual(
    sameTier[0].placements.map((p) => p.rank),
    [1, 2],
  );
});

test('equal strategy scores share a rank and every minimum is next', () => {
  const tied = [account('a', { windows: window5h(0.2) }), account('b', { windows: window5h(0.2) }), account('c', { windows: window5h(0.7) })];
  const trio = tied.map((a) => member(a.id, 0));
  const least = selectionOrder(pool('least_utilized', 'allow', trio), tied, []);
  assert.deepEqual(least[0].next.sort(), ['a', 'b'], 'two members at the lowest utilization are both next');
  assert.deepEqual(
    least[0].placements.map((p) => [p.account.id, p.rank]),
    [
      ['a', 1],
      ['b', 1],
      ['c', 2],
    ],
  );
  const resets = [account('a', { windows: window7d('2026-09-19T00:00:00.000Z') }), account('b', { windows: window7d('2026-09-19T00:00:00.000Z') }), account('c')];
  const earliest = selectionOrder(pool('earliest_refresh', 'allow', trio), resets, []);
  assert.deepEqual(earliest[0].next.sort(), ['a', 'b'], 'two members with the same reset are both next');
  assert.equal(earliest[0].placements[2].rank, 2, 'a member without a sampled reset sorts last');
  const random = selectionOrder(pool('priority_random', 'allow', trio), tied, []);
  assert.deepEqual(random[0].next.sort(), ['a', 'b', 'c'], 'priority_random makes every eligible member of the first tier next');
});

test("the router's verdict decides eligibility and an obstacle trails its tier", () => {
  const trio = [account('busy'), account('cooled'), account('offline')];
  const members = [member('cooled', 0, 'cooldown'), member('offline', 0, 'unavailable'), member('busy', 0)];
  const tiers = selectionOrder(pool('priority_random', 'allow', members), trio, []);
  assert.deepEqual(tiers[0].next, ['busy'], 'only the member the router calls eligible is next');
  assert.deepEqual(
    tiers[0].placements.map((p) => [p.account.id, p.ineligible, p.rank]),
    [
      ['busy', null, 1],
      ['cooled', 'cooldown', null],
      ['offline', 'unavailable', null],
    ],
    'the obstacles trail the tier in label order, unranked, each carrying the reason the wire gave',
  );
});

test('a pin counts only while the session holds it', () => {
  const pair = [account('held'), account('free')];
  const sessions = [
    { accountId: 'held', pinned: true },
    { accountId: 'held', pinned: false },
    { accountId: 'free', pinned: false },
  ];
  const tiers = selectionOrder(pool('priority_random', 'allow', [member('free', 0), member('held', 0)]), pair, sessions);
  assert.deepEqual(
    tiers[0].placements.map((p) => [p.account.id, p.pins]),
    [
      ['free', 0],
      ['held', 1],
    ],
    'an expired session still sits in the list but no longer pins its upstream',
  );
});

test('the kind order walks plan oauth, overage oauth, then api keys ahead of priority', () => {
  const trio = [account('key', { kind: 'api_key' }), account('over', { extraUsage: inOverage }), account('plan')];
  const members = [member('key', 0), member('over', 0), member('plan', 1)];
  const tiers = selectionOrder(pool('priority_random', 'prefer_plan', members, { preferSubscription: true }), trio, []);
  assert.deepEqual(tiers.map((t) => [t.group, t.priority, t.next]), [[0, 1, ['plan']], [1, 0, []], [2, 0, []]], 'the plan-funded P1 comes first, then the overage P0, then the key');
  const forced = selectionOrder(pool('least_utilized', 'prefer_plan', members), trio, []);
  assert.equal(forced[0].group, 0, 'least_utilized forces the kind order whatever the flag says');
  assert.deepEqual(selectionOrder(pool('round_robin', 'allow', [member('a', 0), member('b', 0)]), [account('a'), account('b')], [])[0].next.sort(), ['a', 'b'], 'round robin marks every member of the tier as next');
});

test('round robin ranks the overage subtier behind the plan-funded one it rotates', () => {
  const members = [member('over', 0), member('plan', 0)];
  const tiers = selectionOrder(pool('round_robin', 'prefer_plan', members), accounts, []);
  assert.deepEqual(
    tiers[0].placements.map((p) => [p.account.id, p.deferred, p.rank]),
    [
      ['plan', false, 1],
      ['over', true, 2],
    ],
    'the router rotates the plan-funded members alone while they last, so the overage one is not next',
  );
  assert.deepEqual(tiers[0].next, ['plan']);
  const allow = selectionOrder(pool('round_robin', 'allow', members), accounts, []);
  assert.deepEqual(allow[0].next.sort(), ['over', 'plan'], 'allow rotates them together, so both are next');
  const deferredOnly = selectionOrder(pool('round_robin', 'prefer_plan', [member('over', 0)]), accounts, []);
  assert.deepEqual(deferredOnly[0].next, ['over'], 'a subtier with nothing ahead of it is still the one the router takes');
});

test('round robin with the subscription preference walks the kind tiers first and rotates inside each', () => {
  const trio = [account('key', { kind: 'api_key' }), account('a'), account('b')];
  const members = [member('key', 0), member('a', 1), member('b', 1)];
  const tiers = selectionOrder(pool('round_robin', 'allow', members, { preferSubscription: true }), trio, []);
  assert.deepEqual(
    tiers.map((t) => [t.group, t.priority, t.next.sort()]),
    [
      [0, 1, ['a', 'b']],
      [2, 0, []],
    ],
    'the subscription tier leads although its priority is lower, and every member of it is the rotation\'s next',
  );
  assert.deepEqual(tiers[0].placements.map((p) => p.rank), [1, 1], 'the cursor, not a score, picks among them');
  const flat = selectionOrder(pool('round_robin', 'allow', members), trio, []);
  assert.deepEqual(flat.map((t) => [t.group, t.priority, t.next]), [[0, 0, ['key']], [0, 1, []]], 'without the preference priority alone orders the tiers');
});
