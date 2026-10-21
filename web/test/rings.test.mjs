import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import * as d from './dist/api.js';

const NOW = Date.UTC(2026, 8, 16, 14, 30);
const HOUR = 3_600_000;
// The test tables' capacities: Max 20x, the anchor, 100 on the 5-hour window and 1000 on the week, Max 5x 25 and 250,
// Pro 5 and 50, Team 10 and no 7-day capacity; no Enterprise row at all.
const TABLES = JSON.parse(readFileSync(new URL('./fixtures/test_tables.json', import.meta.url), 'utf8')).body;
const COSTS = d.decodeSubscriptionCosts(TABLES.subscription_costs.content);

const window = (id, utilization, resetsInMs = 2 * HOUR) => ({ id, label: id, observedAt: new Date(NOW - 60_000).toISOString(), periodHours: id === '5h' ? 5 : 168, resetsAt: new Date(NOW + resetsInMs).toISOString(), utilization });
const account = (id, rateLimitTier, windows, extra = {}) => ({
  id,
  kind: 'oauth',
  label: id,
  routingEnabled: true,
  subscription: rateLimitTier === null ? null : { rateLimitTier },
  windows,
  ...extra,
});
const close = (actual, expected, what) => assert.ok(Math.abs(actual - expected) < 1e-9, `${what}: ${actual} against ${expected}`);

test('a Max 20x account at 10% left and a Pro account at 90% left weigh to 1.05 capacity and 0.145 left', () => {
  // Weighed by capacity over the anchor's: 100 / 100 and 5 / 100.
  const figures = d.ringFigures([account('primary', 'default_claude_max_20x', [window('5h', 0.9)]), account('secondary', 'default_claude_ai', [window('5h', 0.1)])], '5h', COSTS, NOW);
  assert.equal(figures.anchor, 'Max 20x', 'the capacity reads in windows of the tier the file anchors its unit on');
  close(figures.capacity, 1.05, 'capacity');
  close(figures.left, 0.145, 'left');
  assert.deepEqual(
    figures.rows.map((r) => [r.id, r.weight]),
    [
      ['secondary', 0.05],
      ['primary', 1],
    ],
  );
  assert.deepEqual(figures.leftOut, []);
  assert.equal(d.fmtPct(figures.left / figures.capacity, 1), '13.8%');
  assert.equal(d.fmtWindows(figures.capacity), '1.05');
  assert.equal(d.fmtWindows(1.3), '1.30');
});

test("the ring's rows read the account with the most of its own window left first", () => {
  const tier = 'default_claude_max_5x';
  const figures = d.ringFigures([account('low', tier, [window('5h', 0.8)]), account('high', tier, [window('5h', 0.1)]), account('half', tier, [window('5h', 0.5)])], '5h', COSTS, NOW);
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
  const figures = d.ringFigures([account('b', tier, [window('5h', 0.6)]), account('a', tier, [window('5h', 0.6)])], '5h', COSTS, NOW);
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
  const figures = d.ringFigures(accounts, '7d', COSTS, NOW);
  assert.deepEqual(
    figures.rows.map((r) => r.id),
    ['primary'],
  );
  assert.deepEqual(figures.leftOut, [
    { id: 'nightly', label: 'nightly', reason: 'Team 7-day capacity not measured' },
    { id: 'team', label: 'team', reason: 'tier claude_enterprise is not in the cost model' },
    { id: 'fresh', label: 'fresh', reason: 'no subscription profile' },
  ]);
  close(figures.capacity, 1, 'capacity');
  close(figures.left, 0.59, 'left');
});

test('a ring with every account left out has no rows', () => {
  const accounts = [account('secondary', 'default_claude_team', [window('7d', 0.88)]), account('nightly', 'claude_enterprise', [window('7d', 0.35)])];
  const figures = d.ringFigures(accounts, '7d', COSTS, NOW);
  assert.deepEqual(figures.rows, []);
  assert.equal(figures.capacity, 0);
  assert.equal(figures.left, 0);
  assert.deepEqual(
    figures.leftOut.map((o) => o.reason),
    ['Team 7-day capacity not measured', 'tier claude_enterprise is not in the cost model'],
  );
});

test("an anchor with no capacity on the window weighs no account and names the anchor's tier as the one not measured", () => {
  const anchorless = { ...COSTS, capacity: { ...COSTS.capacity, default_claude_max_20x: { ...COSTS.capacity.default_claude_max_20x, sevenDay: null } } };
  const figures = d.ringFigures([account('primary', 'default_claude_max_20x', [window('7d', 0.41)]), account('nightly', 'default_claude_max_5x', [window('7d', 0.35)])], '7d', anchorless, NOW);
  assert.deepEqual(figures.rows, []);
  assert.deepEqual(
    figures.leftOut.map((o) => o.reason),
    ['Max 20x 7-day capacity not measured', 'Max 20x 7-day capacity not measured'],
  );
});

test('an expired window counts as a full allowance, keeps its weight and is not named', () => {
  const accounts = [account('primary', 'default_claude_max_20x', [window('5h', 0.8, -2 * HOUR)]), account('nightly', 'default_claude_max_5x', [window('5h', 0.6)])];
  const figures = d.ringFigures(accounts, '5h', COSTS, NOW);
  assert.deepEqual(
    figures.rows.map((r) => [r.id, r.weight, Number(r.remaining.toFixed(9))]),
    [
      ['primary', 1, 1],
      ['nightly', 0.25, 0.4],
    ],
  );
  assert.deepEqual(figures.leftOut, []);
  close(figures.capacity, 1.25, 'capacity');
  close(figures.left, 1.1, 'left');
});

test('an account with routing off or without the window is neither weighed nor named', () => {
  const accounts = [
    account('primary', 'default_claude_max_20x', [window('5h', 0.5)]),
    account('team', 'claude_team', [window('5h', 0.18)], { routingEnabled: false }),
    account('off', 'default_claude_ai', [window('5h', 0.2)], { routingEnabled: false }),
    account('spare', null, []),
    account('weekly', 'default_claude_max_5x', [window('7d', 0.3)]),
    account('overflow', null, [], { kind: 'api_key' }),
  ];
  const figures = d.ringFigures(accounts, '5h', COSTS, NOW);
  assert.deepEqual(
    figures.rows.map((r) => r.id),
    ['primary'],
  );
  assert.deepEqual(figures.leftOut, []);
});

test('a disconnected account with the windows of its last reading and no profile is on no ring and in no left-out list', () => {
  const accounts = [account('primary', 'default_claude_max_20x', [window('5h', 0.5), window('7d', 0.4)]), account('spare', null, [window('5h', 0.4), window('7d', 0.02)], { status: 'unpaired' })];
  for (const id of ['5h', '7d']) {
    const figures = d.ringFigures(accounts, id, COSTS, NOW);
    assert.deepEqual(
      figures.rows.map((r) => r.id),
      ['primary'],
      `the ${id} ring`,
    );
    assert.deepEqual(figures.leftOut, [], `the ${id} ring's left-out list`);
  }
});
