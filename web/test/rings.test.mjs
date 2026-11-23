import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import * as d from './dist/api.js';

const NOW = Date.UTC(2026, 8, 16, 14, 30);
const HOUR = 3_600_000;
// The test tables' 7-day capacities: Ultra 1000, Max 250, Pro, the anchor, 50, Team none, and no Enterprise row at
// all. The Fable week is a model week of the 7-day window at a factor of 0.5.
const TABLES = JSON.parse(readFileSync(new URL('./fixtures/test_tables.json', import.meta.url), 'utf8')).body;
const COSTS = d.decodeSubscriptionCosts(TABLES.subscription_costs.content, TABLES.subscription_costs.unit);

const window = (id, utilization, resetsInMs = 2 * HOUR) => ({ id, label: id, observedAt: new Date(NOW - 60_000).toISOString(), periodHours: id === '5h' ? 5 : 168, resetsAt: new Date(NOW + resetsInMs).toISOString(), utilization });
// The plan word each tier resolves to on its own, so an account reads the plan the router would resolve for it and the
// gate weighs it by its tier. A case that needs another word passes it.
const PLAN_OF_TIER = {
  claude_enterprise: 'enterprise',
  claude_team: 'team',
  default_claude_ai: 'pro',
  default_claude_max_20x: 'max_20x',
  default_claude_max_5x: 'max_5x',
  default_claude_team: 'team',
  default_claude_zero: 'usage_based',
};
// The decoder holds the Fable week apart from `windows`, so an account carries it as `fableWindow` alone.
const account = (id, rateLimitTier, windows, extra = {}, plan = PLAN_OF_TIER[rateLimitTier] ?? null) => ({
  fableWindow: null,
  id,
  kind: 'oauth',
  label: id,
  planEndedAt: null,
  routingEnabled: true,
  subscription: rateLimitTier === null ? null : { plan, rateLimitTier },
  windows,
  ...extra,
});
const fable = (utilization, resetsInMs) => ({ fableWindow: window('7d_fable', utilization, resetsInMs) });
const close = (actual, expected, what) => assert.ok(Math.abs(actual - expected) < 1e-9, `${what}: ${actual} against ${expected}`);
const weights = (figures) => figures.rows.map((r) => [r.id, Number(r.weight.toFixed(9))]);

test("the 7-day ring weighs each account by its tier's 7-day capacity over the chosen week's tier's", () => {
  // An Ultra account at 10% left and a Pro account at 80% left: 1000 and 50 over Ultra's 1000, over Pro's 50.
  const accounts = [account('primary', 'default_claude_max_20x', [window('7d', 0.9)]), account('secondary', 'default_claude_ai', [window('7d', 0.2)])];
  const ultra = d.ringFigures(accounts, '7d', COSTS, 'ultra', NOW);
  assert.deepEqual(weights(ultra), [
    ['secondary', 0.05],
    ['primary', 1],
  ]);
  close(ultra.capacity, 1.05, 'capacity under the Ultra week');
  close(ultra.left, 0.14, 'left under the Ultra week');
  const pro = d.ringFigures(accounts, '7d', COSTS, 'pro', NOW);
  assert.deepEqual(weights(pro), [
    ['secondary', 1],
    ['primary', 20],
  ]);
  close(pro.capacity, 21, 'capacity under the Pro week');
  close(pro.left, 2.8, 'left under the Pro week');
  assert.deepEqual([ultra.leftOut, pro.leftOut], [[], []]);
});

test("the Fable ring weighs the accounts whose Fable week the poll carries and names a Pro account without one", () => {
  // The Fable week is a factor of each tier's 7-day window, one factor for every tier, so the factor cancels and the
  // ring weighs by the 7-day capacities: an Ultra account one Ultra Fable week, a Max account a quarter.
  const accounts = [
    account('primary', 'default_claude_max_20x', [window('5h', 0.2), window('7d', 0.3)], fable(0.5)),
    account('nightly', 'default_claude_max_5x', [window('5h', 0.1), window('7d', 0.2)], fable(0.6)),
    account('secondary', 'default_claude_ai', [window('5h', 0.1), window('7d', 0.2)]),
  ];
  const figures = d.ringFigures(accounts, '7d_fable', COSTS, 'ultra', NOW);
  assert.deepEqual(weights(figures), [
    ['primary', 1],
    ['nightly', 0.25],
  ]);
  close(figures.capacity, 1.25, 'capacity');
  close(figures.left, 0.6, 'left');
  assert.deepEqual(figures.leftOut, [{ id: 'secondary', label: 'secondary', reason: 'no Fable week' }]);
});

test("each ring's line and row titles count the chosen week's weeks, the Fable ring's none under the Pro week", () => {
  const sevenDay = [account('primary', 'default_claude_max_20x', [window('7d', 0.9)]), account('secondary', 'default_claude_ai', [window('7d', 0.2)])];
  const lines = (accounts, id) => ['ultra', 'max', 'pro'].map((week) => d.ringLine(d.ringFigures(accounts, id, COSTS, week, NOW)));
  assert.deepEqual(lines(sevenDay, '7d'), ['0.14 of 1.05 Ultra weeks left', '0.56 of 4.20 Max weeks left', '2.80 of 21.00 Pro weeks left']);
  const ultra = d.ringFigures(sevenDay, '7d', COSTS, 'ultra', NOW);
  assert.deepEqual(
    ultra.rows.map((r) => d.ringRowTitle(ultra, r)),
    ['secondary: 0.04 of 1.05 Ultra weeks, 4% of the pooled capacity', 'primary: 0.10 of 1.05 Ultra weeks, 10% of the pooled capacity'],
  );
  const fableWeek = [account('primary', 'default_claude_max_20x', [window('7d', 0.3)], fable(0.5)), account('nightly', 'default_claude_max_5x', [window('7d', 0.2)], fable(0.6))];
  assert.deepEqual(lines(fableWeek, '7d_fable'), ['0.60 of 1.25 Ultra Fable weeks left', '2.40 of 5.00 Max Fable weeks left', null], 'Pro has no Fable week, so no line counts Pro Fable weeks');
  const pro = d.ringFigures(fableWeek, '7d_fable', COSTS, 'pro', NOW);
  assert.deepEqual(
    pro.rows.map((r) => d.ringRowTitle(pro, r)),
    ['primary: 40% of the pooled capacity', 'nightly: 8% of the pooled capacity'],
  );
});

test('an account without the ring\'s window is named with the window\'s reason, one with no window at all "not read yet", and one with routing off neither weighed nor named', () => {
  const accounts = [
    account('primary', 'default_claude_max_20x', [window('7d', 0.5)]),
    account('approved', 'default_claude_max_20x', [window('5h', 0.4)], fable(0.3)),
    account('fresh', 'default_claude_max_5x', []),
    // The window test runs before the profile's, so an account with neither reads as one the poll has not read.
    account('spare', null, []),
    account('bare', null, [window('7d', 0.2)]),
    account('off', 'default_claude_ai', [window('7d', 0.2)], { routingEnabled: false }),
    account('overflow', null, [], { kind: 'api_key' }),
  ];
  const sevenDay = d.ringFigures(accounts, '7d', COSTS, 'ultra', NOW);
  assert.deepEqual(
    sevenDay.rows.map((r) => r.id),
    ['primary'],
  );
  assert.deepEqual(sevenDay.leftOut, [
    { id: 'approved', label: 'approved', reason: 'no 7-day window' },
    { id: 'fresh', label: 'fresh', reason: 'not read yet' },
    { id: 'spare', label: 'spare', reason: 'not read yet' },
    { id: 'bare', label: 'bare', reason: 'no subscription profile' },
  ]);
  assert.deepEqual(
    d.ringFigures(accounts, '7d_fable', COSTS, 'ultra', NOW).leftOut.map((o) => [o.id, o.reason]),
    [
      ['primary', 'no Fable week'],
      ['fresh', 'not read yet'],
      ['spare', 'not read yet'],
      ['bare', 'no Fable week'],
    ],
  );
});

test("a ring whose listed accounts carry the window tells them from a fleet with no account on the window, a gated account's window counted", () => {
  // A free plan on the 7-day window weighs nothing, and its window still stands, so the ring may not say no account
  // has the window.
  const free = d.ringFigures([account('free', 'default_claude_ai', [window('7d', 0.5)], {}, 'free')], '7d', COSTS, 'ultra', NOW);
  assert.deepEqual([free.rows, free.carried], [[], true]);
  const pro = d.ringFigures([account('secondary', 'default_claude_ai', [window('7d', 0.5)])], '7d_fable', COSTS, 'ultra', NOW);
  assert.deepEqual([pro.rows, pro.carried], [[], false]);
});

test("the ring's rows read the account with the most of its own window left first", () => {
  const tier = 'default_claude_max_5x';
  const figures = d.ringFigures([account('low', tier, [window('7d', 0.8)]), account('high', tier, [window('7d', 0.1)]), account('half', tier, [window('7d', 0.5)])], '7d', COSTS, 'ultra', NOW);
  assert.deepEqual(
    figures.rows.map((r) => [r.id, Number(r.remaining.toFixed(9))]),
    [
      ['high', 0.9],
      ['half', 0.5],
      ['low', 0.2],
    ],
    'shares of 0.2, 0.9 and 0.5 left read 0.9, 0.5, 0.2',
  );
});

test("two of the ring's rows with equal shares left read by label", () => {
  const tier = 'default_claude_max_5x';
  const figures = d.ringFigures([account('b', tier, [window('7d', 0.6)]), account('a', tier, [window('7d', 0.6)])], '7d', COSTS, 'ultra', NOW);
  assert.deepEqual(
    figures.rows.map((r) => r.id),
    ['a', 'b'],
  );
});

test('an unsized tier and a null 7-day capacity are left out with their reasons', () => {
  const accounts = [
    account('primary', 'default_claude_max_20x', [window('7d', 0.41)]),
    account('nightly', 'default_claude_team', [window('7d', 0.35)]),
    account('team', 'claude_enterprise', [window('7d', 0.52)]),
    account('fresh', null, [window('7d', 0.1)]),
  ];
  const figures = d.ringFigures(accounts, '7d', COSTS, 'pro', NOW);
  assert.deepEqual(
    figures.rows.map((r) => r.id),
    ['primary'],
  );
  assert.deepEqual(figures.leftOut, [
    { id: 'nightly', label: 'nightly', reason: 'Team 7-day capacity not measured' },
    { id: 'team', label: 'team', reason: 'tier unknown' },
    { id: 'fresh', label: 'fresh', reason: 'no subscription profile' },
  ]);
  close(figures.capacity, 20, 'capacity');
  close(figures.left, 11.8, 'left');
});

test('a ring with every account left out has no rows', () => {
  const accounts = [account('secondary', 'default_claude_team', [window('7d', 0.88)]), account('nightly', 'claude_enterprise', [window('7d', 0.35)])];
  const figures = d.ringFigures(accounts, '7d', COSTS, 'pro', NOW);
  assert.deepEqual(figures.rows, []);
  assert.equal(figures.capacity, 0);
  assert.equal(figures.left, 0);
  assert.deepEqual(
    figures.leftOut.map((o) => o.reason),
    ['Team 7-day capacity not measured', 'tier unknown'],
  );
});

test("a chosen week whose tier has no 7-day capacity weighs no account and names that tier as the one not measured", () => {
  const unsized = { ...COSTS, capacity: { ...COSTS.capacity, default_claude_max_20x: { ...COSTS.capacity.default_claude_max_20x, sevenDay: null } } };
  const figures = d.ringFigures([account('secondary', 'default_claude_ai', [window('7d', 0.41)]), account('nightly', 'default_claude_max_5x', [window('7d', 0.35)])], '7d', unsized, 'ultra', NOW);
  assert.deepEqual(figures.rows, []);
  assert.deepEqual(
    figures.leftOut.map((o) => o.reason),
    ['Ultra 7-day capacity not measured', 'Ultra 7-day capacity not measured'],
  );
});

test('a cost model that sizes no Fable week weighs no account on the Fable ring and names the tier', () => {
  const weekless = { ...COSTS, modelWindows: {} };
  const accounts = [account('primary', 'default_claude_max_20x', [window('7d', 0.3)], fable(0.5)), account('nightly', 'default_claude_max_5x', [window('7d', 0.2)], fable(0.6))];
  const figures = d.ringFigures(accounts, '7d_fable', weekless, 'ultra', NOW);
  assert.deepEqual(figures.rows, []);
  assert.deepEqual(
    figures.leftOut.map((o) => o.reason),
    ['Ultra Fable week capacity not measured', 'Max Fable week capacity not measured'],
  );
  assert.deepEqual(weights(d.ringFigures(accounts, '7d', weekless, 'ultra', NOW)), [
    ['nightly', 0.25],
    ['primary', 1],
  ], 'the 7-day ring reads no model week');
});

test('an expired window counts as a full allowance, keeps its weight and is not named', () => {
  const accounts = [account('primary', 'default_claude_max_20x', [window('7d', 0.8, -2 * HOUR)]), account('nightly', 'default_claude_max_5x', [window('7d', 0.6)])];
  const figures = d.ringFigures(accounts, '7d', COSTS, 'pro', NOW);
  assert.deepEqual(
    figures.rows.map((r) => [r.id, r.weight, Number(r.remaining.toFixed(9))]),
    [
      ['primary', 20, 1],
      ['nightly', 5, 0.4],
    ],
  );
  assert.deepEqual(figures.leftOut, []);
  close(figures.capacity, 25, 'capacity');
  close(figures.left, 22, 'left');
});

test('an account whose window has no share is left out of the ring and of its capacity', () => {
  const tier = 'default_claude_max_20x';
  const figures = d.ringFigures([account('read', tier, [window('7d', 0.5)]), account('gap', tier, [window('7d', null)]), account('stale', tier, [window('7d', null, -2 * HOUR)])], '7d', COSTS, 'ultra', NOW);
  assert.deepEqual([figures.rows.map((r) => r.id), figures.capacity, figures.left, figures.leftOut.map((o) => [o.id, o.reason])], [['read'], 1, 0.5, [['gap', 'share not reported'], ['stale', 'share not reported']]]);
});

test('an empty ring tells missing shares from unsized tiers', () => {
  const shareless = d.ringFigures([account('primary', 'default_claude_max_20x', [window('7d', null)])], '7d', COSTS, 'ultra', NOW);
  const unsized = d.ringFigures([account('seat', 'default_claude_team', [window('7d', 0.2)])], '7d', COSTS, 'ultra', NOW);
  assert.deepEqual([shareless.rows, shareless.carried, shareless.unreported], [[], true, true], 'a sized account with no share');
  assert.deepEqual([unsized.rows, unsized.carried, unsized.unreported], [[], true, false], 'a tier with no 7-day capacity');
});

test('a disconnected account with the windows of its last reading and no profile is on no ring and in no left-out list', () => {
  const accounts = [account('primary', 'default_claude_max_20x', [window('5h', 0.5), window('7d', 0.4)], fable(0.3)), account('spare', null, [window('5h', 0.4), window('7d', 0.02)], { ...fable(0.1), status: 'unpaired' })];
  for (const id of ['7d', '7d_fable']) {
    const figures = d.ringFigures(accounts, id, COSTS, 'ultra', NOW);
    assert.deepEqual(
      figures.rows.map((r) => r.id),
      ['primary'],
      `the ${id} ring`,
    );
    assert.deepEqual(figures.leftOut, [], `the ${id} ring's left-out list`);
  }
});

test('a usage-based seat is named under both rings with its reason and weighs nothing, with or without the window', () => {
  // One seat carries windows into the class and the other reports none, as a real seat does, so the case reads the
  // plan word before the window test. An API-key account is on no ring and in no list.
  const accounts = [
    account('primary', 'default_claude_max_20x', [window('5h', 0.5), window('7d', 0.4)], fable(0.3)),
    account('seat', 'default_claude_zero', [window('5h', 0.3), window('7d', 0.2)], { ...fable(0.1), usageBased: true }),
    account('bare', 'default_claude_zero', [], { usageBased: true }),
    account('key', null, [], { kind: 'api_key' }),
  ];
  for (const id of ['7d', '7d_fable']) {
    const figures = d.ringFigures(accounts, id, COSTS, 'pro', NOW);
    assert.deepEqual(figures.rows.map((r) => [r.id, r.weight]), [['primary', 20]], `the ${id} ring`);
    assert.deepEqual(
      figures.leftOut,
      [
        { id: 'seat', label: 'seat', reason: 'a usage-based seat' },
        { id: 'bare', label: 'bare', reason: 'a usage-based seat' },
      ],
      `the ${id} ring's left-out list`,
    );
  }
});

test('a free plan is named under the ring with its reason and weighs nothing, while a Pro plan on the same tier weighs its capacity', () => {
  // A free organization reports the tier the profile reports for Pro, so the plan word and not the tier decides.
  const free = d.ringFigures([account('free', 'default_claude_ai', [window('7d', 0.5)], {}, 'free')], '7d', COSTS, 'pro', NOW);
  assert.deepEqual(free.rows, []);
  assert.deepEqual(free.leftOut, [{ id: 'free', label: 'free', reason: 'no priced capacity for a free plan' }]);
  assert.deepEqual([free.capacity, free.left], [0, 0]);
  const pro = d.ringFigures([account('pro', 'default_claude_ai', [window('7d', 0.5)], {}, 'pro')], '7d', COSTS, 'pro', NOW);
  assert.deepEqual(pro.rows.map((r) => [r.id, r.weight]), [['pro', 1]], "Pro's 50 over the Pro week's 50");
  assert.deepEqual(pro.leftOut, []);
});
