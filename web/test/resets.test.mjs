import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

const NOW = Date.parse('2026-09-16T14:30:00Z');
const oneReset = (state, endsAt) => ({
  cooldownUntil: null,
  grants: [{ blocking: [], clears: ['7d_fable'], endsAt, id: 'rg-a-1', kind: 'fable', label: 'Launch reset', reason: null, resetsLeft: 1, resetsTotal: 1, startsAt: '2026-09-01T00:00:00.000Z', state, usedAt: null }],
  left: 1,
});

test("the chip warns from the edge of the warning's days", () => {
  const edge = d.resetChip(oneReset('usable', '2026-09-21T14:30:00.000Z'), NOW, 5);
  const past = d.resetChip(oneReset('usable', '2026-09-21T14:30:00.001Z'), NOW, 5);
  assert.deepEqual({ ends: edge?.ends, tone: edge?.tone }, { ends: 'ends in 5 days', tone: 'warning' }, 'a reset that ends exactly five days out');
  assert.deepEqual({ ends: past?.ends, tone: past?.tone }, { ends: null, tone: 'plain' }, 'a reset that ends a millisecond past the five days');
});

test('a reason keeps the warning', () => {
  const chip = d.resetChip(oneReset('cooldown', '2026-09-18T14:30:00.000Z'), NOW, 5);
  assert.deepEqual({ state: chip?.state, tone: chip?.tone, usable: chip?.usable }, { state: 'on a cooldown', tone: 'warning', usable: null });
});

const windowAt = (utilization) => ({ resetsAt: '2026-09-20T00:00:00.000Z', utilization });

test('the estimate leaves where it prints the reading', () => {
  const grant = { clears: ['7d_fable'], kind: 'fable' };
  // The 7-day window reads 78% left: 1 - (0.22 - 0.004) prints 78% too, and 1 - (0.22 - 0.02) prints 80%.
  assert.equal(d.resetEstimate(grant, 0.004, windowAt(0.22), windowAt(0.5), NOW), null, 'a share that rounds away');
  assert.deepEqual(d.resetEstimate(grant, 0.02, windowAt(0.22), windowAt(0.5), NOW), { figure: '80%' }, 'a share of two points');
});

test('a lost answer is not a refusal', () => {
  const path = '/admin/accounts/a/limit-resets/rg-a-1/use';
  const grant = { id: 'rg-a-1', resetsTotal: 1 };
  const lost = d.resetAnswer({ thrown: new d.ApiError(0, 'Failed to fetch', null, path) }, 'a', grant, NOW);
  assert.deepEqual({ kind: lost.kind, notUsed: lost.sentence.includes('not used') }, { kind: 'unconfirmed', notUsed: false }, 'a call that got no answer');
  const failed = d.resetAnswer({ thrown: new d.ApiError(500, 'internal error', null, path) }, 'a', grant, NOW);
  assert.deepEqual({ kind: failed.kind, notUsed: failed.sentence.includes('not used') }, { kind: 'unconfirmed', notUsed: false }, 'a 500 with no code');
  const busy = d.resetAnswer({ thrown: new d.ApiError(409, 'a reset of this account is being used now', 'limit_reset_in_flight', path) }, 'a', grant, NOW);
  assert.equal(busy.kind, 'refused', 'a 409 the route answered before the claim left');
});

test('the warning needs every window full', () => {
  assert.equal(d.resetClearsNothing([windowAt(0), windowAt(0)], NOW), true, 'the 5-hour window and the Fable week both at 100% left');
  assert.equal(d.resetClearsNothing([windowAt(0), windowAt(0.6)], NOW), false, 'the Fable week at 40% left');
});

test('a live window with no share never reads full, and a live window with nothing used does', () => {
  assert.deepEqual([d.readsFull(windowAt(null), NOW), d.readsFull(windowAt(0), NOW)], [false, true]);
});

test('a grant that names no window has nothing to warn about', () => {
  assert.equal(d.resetClearsNothing([], NOW), false);
});

test('a grant that names no window has no clears words', () => {
  assert.equal(d.clearsWords([]), null, 'a grant with an empty clears');
  assert.equal(d.clearsWords(['5h', '7d']), '5h and 7d', 'a grant that clears the 5-hour and the 7-day window');
});
