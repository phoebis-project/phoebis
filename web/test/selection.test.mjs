import assert from 'node:assert/strict';
import { test } from 'node:test';
import { selectionOrder } from './dist/api.js';

const HOUR = 3_600_000;
/** A reset relative to the real clock the pace score reads, so the test holds whenever it runs. */
const hoursFromNow = (h) => new Date(Date.now() + h * HOUR).toISOString();

const account = (id, over = {}) => ({
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
  status: 'ready',
  subscription: null,
  windows: [],
  ...over,
});
const window7d = (resetsAt, utilization = 0.5) => [{ id: '7d', label: '7-day', observedAt: '2026-09-18T00:00:00.000Z', periodHours: 168, resetsAt, utilization }];
const inOverage = { disabledReason: null, enabled: true, inUse: true, monthlyLimitCredits: 100, usedCredits: 10 };
// The eligibility verdict rides on the member, because the router computes it against its own live cooldowns and
// overages; a case states it there rather than re-deriving it from the account.
const member = (accountId, priority, eligible = 'eligible') => ({ accountId, eligible, priority });
const pool = (strategy, overagePolicy, members, over = {}) => {
  const merged = {
    activePins: 0,
    affinityTtlSeconds: 600,
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
  return merged;
};
const accounts = [account('over', { extraUsage: inOverage }), account('plan')];

test('an overage P0 beside a plan-funded P1 follows each overage policy', () => {
  const tiers = [member('over', 0), member('plan', 1)];
  const prefer = selectionOrder(pool('priority', 'prefer_plan', tiers), accounts, []);
  assert.deepEqual(prefer[0].next, ['over'], 'prefer_plan keeps the overage account eligible and the router lands on it, since no plan-funded sibling shares its tier');
  assert.equal(prefer[0].placements[0].deferred, true);
  const never = selectionOrder(pool('priority', 'never', [member('over', 0, 'overage'), member('plan', 1)]), accounts, []);
  assert.deepEqual(never[0].next, [], 'never makes overage an obstacle');
  assert.equal(never[0].placements[0].ineligible, 'overage');
  assert.deepEqual(never[1].next, ['plan'], 'so the next fresh session lands one tier down');
  const allow = selectionOrder(pool('priority', 'allow', tiers), accounts, []);
  assert.deepEqual(allow[0].next, ['over']);
  assert.equal(allow[0].placements[0].deferred, false, 'allow ignores overage');
  const sameTier = selectionOrder(pool('priority', 'prefer_plan', [member('over', 0), member('plan', 0)]), accounts, []);
  assert.deepEqual(sameTier[0].next, ['plan'], 'inside one tier the plan-funded member ranks ahead of the deferred one');
  assert.deepEqual(
    sameTier[0].placements.map((p) => p.rank),
    [1, 2],
  );
});

test('equal pace scores share a rank, an unsampled account outranks a used one, and every minimum is next', () => {
  const tied = [
    account('a', { windows: window7d(hoursFromNow(24), 0.5) }),
    account('b', { windows: window7d(hoursFromNow(24), 0.5) }),
    account('c', { windows: window7d(hoursFromNow(24), 0.9) }),
  ];
  const trio = tied.map((a) => member(a.id, 0));
  const pace = selectionOrder(pool('pace', 'allow', trio), tied, []);
  assert.deepEqual(pace[0].next.sort(), ['a', 'b'], 'two members with the same remaining allowance and reset are both next');
  assert.deepEqual(
    pace[0].placements.map((p) => [p.account.id, p.rank]),
    [
      ['a', 1],
      ['b', 1],
      ['c', 2],
    ],
  );
  // Nearly spent over a whole week left is a worse rate than an untouched account's, which the fallback
  // treats as a full week's remaining allowance.
  const untouched = [account('a', { windows: window7d(hoursFromNow(168), 0.95) }), account('b')];
  const ranked = selectionOrder(pool('pace', 'allow', untouched.map((a) => member(a.id, 0))), untouched, []);
  assert.deepEqual(ranked[0].next, ['b'], 'an unsampled account counts as untouched, ahead of one with a worse remaining-over-hours rate');
  const random = selectionOrder(pool('priority', 'allow', trio), tied, []);
  assert.deepEqual(random[0].next.sort(), ['a', 'b', 'c'], 'priority makes every eligible member of the first tier next');
});

test("the router's verdict decides eligibility and an obstacle trails its tier", () => {
  const trio = [account('busy'), account('cooled'), account('offline')];
  const members = [member('cooled', 0, 'cooldown'), member('offline', 0, 'unavailable'), member('busy', 0)];
  const tiers = selectionOrder(pool('priority', 'allow', members), trio, []);
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
  const tiers = selectionOrder(pool('priority', 'allow', [member('free', 0), member('held', 0)]), pair, sessions);
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
  const tiers = selectionOrder(pool('priority', 'prefer_plan', members, { preferSubscription: true }), trio, []);
  assert.deepEqual(tiers.map((t) => [t.group, t.priority, t.next]), [[0, 1, ['plan']], [1, 0, []], [2, 0, []]], 'the plan-funded P1 comes first, then the overage P0, then the key');
  const unflagged = selectionOrder(pool('priority', 'prefer_plan', members), trio, []);
  assert.equal(unflagged[0].group, 0, 'with the flag off every member shares one group whatever the strategy');
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
