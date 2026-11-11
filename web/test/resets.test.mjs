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
