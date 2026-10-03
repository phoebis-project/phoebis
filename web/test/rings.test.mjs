import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

const NOW = Date.UTC(2026, 8, 16, 14, 30);
const HOUR = 3_600_000;
const TIERS = {
  default_claude_max_20x: { fiveHour: 1, sevenDay: 1 },
  default_claude_max_5x: { fiveHour: 0.25, sevenDay: null },
  default_claude_pro: { fiveHour: 0.05, sevenDay: null },
};

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
  const figures = d.ringFigures([account('business', 'default_claude_max_20x', [window('5h', 0.9)]), account('mail', 'default_claude_pro', [window('5h', 0.1)])], '5h', TIERS, NOW);
  close(figures.capacity, 1.05, 'capacity');
  close(figures.left, 0.145, 'left');
  assert.deepEqual(
    figures.rows.map((r) => [r.id, r.weight]),
    [
      ['business', 1],
      ['mail', 0.05],
    ],
  );
  assert.deepEqual(figures.leftOut, []);
  assert.equal(d.fmtPct(figures.left / figures.capacity, 1), '13.8%');
  assert.equal(d.fmtWindows(figures.capacity), '1.05');
  assert.equal(d.fmtWindows(1.3), '1.30');
});

test('an unsized tier and a null 7-day factor are left out with their reasons', () => {
  const accounts = [
    account('business', 'default_claude_max_20x', [window('7d', 0.41)]),
    account('personal', 'default_claude_max_5x', [window('7d', 0.35)]),
    account('team', 'claude_team', [window('7d', 0.52)]),
    account('fresh', null, [window('7d', 0.1)]),
  ];
  const figures = d.ringFigures(accounts, '7d', TIERS, NOW);
  assert.deepEqual(
    figures.rows.map((r) => r.id),
    ['business'],
  );
  assert.deepEqual(figures.leftOut, [
    { id: 'personal', label: 'personal', reason: 'Max 5x 7-day capacity not measured' },
    { id: 'team', label: 'team', reason: 'tier claude_team is not in the cost table' },
    { id: 'fresh', label: 'fresh', reason: 'no subscription profile' },
  ]);
  close(figures.capacity, 1, 'capacity');
  close(figures.left, 0.59, 'left');
});

test('a ring with every account left out has no rows', () => {
  const accounts = [account('mail', 'default_claude_pro', [window('7d', 0.88)]), account('personal', 'default_claude_max_5x', [window('7d', 0.35)])];
  const figures = d.ringFigures(accounts, '7d', TIERS, NOW);
  assert.deepEqual(figures.rows, []);
  assert.equal(figures.capacity, 0);
  assert.equal(figures.left, 0);
  assert.deepEqual(
    figures.leftOut.map((o) => o.reason),
    ['Pro 7-day capacity not measured', 'Max 5x 7-day capacity not measured'],
  );
});

test('an expired window counts as a full allowance, keeps its weight and is not named', () => {
  const accounts = [account('business', 'default_claude_max_20x', [window('5h', 0.8, -2 * HOUR)]), account('personal', 'default_claude_max_5x', [window('5h', 0.6)])];
  const figures = d.ringFigures(accounts, '5h', TIERS, NOW);
  assert.deepEqual(
    figures.rows.map((r) => [r.id, r.weight, Number(r.remaining.toFixed(9))]),
    [
      ['business', 1, 1],
      ['personal', 0.25, 0.4],
    ],
  );
  assert.deepEqual(figures.leftOut, []);
  close(figures.capacity, 1.25, 'capacity');
  close(figures.left, 1.1, 'left');
});

test('an account with routing off or without the window is neither weighed nor named', () => {
  const accounts = [
    account('business', 'default_claude_max_20x', [window('5h', 0.5)]),
    account('team', 'claude_team', [window('5h', 0.18)], { routingEnabled: false }),
    account('off', 'default_claude_pro', [window('5h', 0.2)], { routingEnabled: false }),
    account('spare', null, []),
    account('weekly', 'default_claude_max_5x', [window('7d', 0.3)]),
    account('console', null, [], { kind: 'api_key' }),
  ];
  const figures = d.ringFigures(accounts, '5h', TIERS, NOW);
  assert.deepEqual(
    figures.rows.map((r) => r.id),
    ['business'],
  );
  assert.deepEqual(figures.leftOut, []);
});
