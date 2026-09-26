import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

test('a preset is derived from the values and custom when none matches', () => {
  for (const name of ['balanced', 'conservative', 'fast']) {
    assert.equal(d.sheddingPresetOf(d.SHEDDING_PRESETS[name]), name);
    assert.equal(d.failoverPresetOf(d.FAILOVER_PRESETS[name]), name);
  }
  const shedding = d.SHEDDING_PRESETS.balanced;
  for (const off of [{ shedShare: 0.4 }, { burnWindowSeconds: 601 }, { sourceDwellSeconds: 899 }]) assert.equal(d.sheddingPresetOf({ ...shedding, ...off }), 'custom', JSON.stringify(off));
  assert.equal(d.sheddingPresetOf({ ...d.SHEDDING_PRESETS.conservative, shedShare: 0.1 + 0.2 }), 'conservative', 'a share that reads back a rounding off still names its preset');
  assert.deepEqual(
    [
      d.failoverPresetOf({ inFlightCap: 16, paceFloor: 0.05, retryInPlaceSeconds: 10 }),
      d.failoverPresetOf({ inFlightCap: 16, paceFloor: 0.05, retryInPlaceSeconds: 20 }),
      d.failoverPresetOf({ inFlightCap: 16, paceFloor: 0.05, retryInPlaceSeconds: 15 }),
    ],
    ['balanced', 'conservative', 'custom'],
    'balanced and conservative share the cap and the floor, so the retry-in-place bound alone tells them apart',
  );
  for (const off of [{ inFlightCap: 9 }, { paceFloor: 0.05 }, { retryInPlaceSeconds: 6 }]) assert.equal(d.failoverPresetOf({ ...d.FAILOVER_PRESETS.fast, ...off }), 'custom', JSON.stringify(off));
});

const NOW = Date.parse('2026-09-16T14:30:00Z');
const MINUTE = 60_000;
const at = (ms) => new Date(ms).toISOString();
const reading = (id, utilization) => ({ id, label: id, observedAt: at(NOW), periodHours: id === '5h' ? 5 : 168, resetsAt: at(NOW + 60 * MINUTE), utilization });
const account = (id, fiveHour) => ({ id, kind: 'oauth', label: id, windows: [reading('5h', fiveHour), reading('7d', 0.2)] });
const hot = account('hot', 0.9);
const cool = account('cool', 0.1);
/** A pin folded at `NOW`, so its stored burn is its burn now; warm while its last refresh is younger than its TTL. */
const pin = (sessionId, burn, warm, over = {}) => ({
  accountId: 'hot',
  burn,
  burnAt: at(NOW),
  burnUnit: 'meter_tokens',
  cacheTtlMs: 5 * MINUTE,
  keyId: 'desktop',
  lastInputTokens: 1_000,
  lastRefreshAt: at(warm ? NOW - MINUTE : NOW - 10 * MINUTE),
  model: 'claude-sonnet-5',
  pinned: true,
  sessionId,
  ...over,
});
const pool = (over = {}) => ({
  burnWindowSeconds: 600,
  members: [
    { accountId: 'hot', eligible: 'eligible', priority: 0 },
    { accountId: 'cool', eligible: 'eligible', priority: 0 },
  ],
  migrateAboveUtilization: 0.8,
  shedShare: 0.5,
  warmSessions: 'when_needed',
  ...over,
});
const preview = (sessions, over) => d.shedPreview(pool(over), [hot, cool], sessions, NOW);
const four = [pin('warm-4', 4, true), pin('cold-3', 3, false), pin('warm-2', 2, true), pin('warm-1', 1, true)];

test('the preview orders cold pins first, stops at the share, and never marks the last pin', () => {
  assert.deepEqual(
    d.shedMarks(
      [
        { burn: 3, id: 'cold-3' },
        { burn: 4, id: 'warm-4' },
        { burn: 2, id: 'warm-2' },
        { burn: 1, id: 'warm-1' },
      ],
      5,
    ),
    ['cold-3', 'warm-4'],
    'marked in order until the target is reached',
  );
  assert.deepEqual(d.shedMarks([{ burn: 9, id: 'only' }], 9), [], 'never the last unmarked pin');
  assert.deepEqual(
    d.shedMarks(
      [
        { burn: 1, id: 'a' },
        { burn: 1, id: 'b' },
      ],
      0,
    ),
    [],
    'nothing for a zero target',
  );
  const round = preview(four);
  assert.deepEqual([round.sessions, round.warm, round.burnShare.toFixed(2), round.members, round.unit], [2, 1, '0.70', 1, 'meter_tokens'], 'cold 3 then warm 4 reach half of 10, and the cool member runs no round');
  const one = preview([pin('only', 100, false)]);
  assert.deepEqual([one.sessions, one.burnShare], [0, 0], 'a one-pin member sheds nothing');
  const pair = preview([pin('warm-9', 9, true), pin('cold-1', 1, false)]);
  assert.deepEqual([pair.sessions, pair.warm, pair.burnShare.toFixed(2)], [1, 0, '0.10'], 'the cold pin is marked first and the warm one is the last unmarked');
  assert.equal(preview(four.map((p) => ({ ...p, burn: 0 }))).sessions, 0, 'no burn is a zero target');
  assert.equal(preview(four, { shedShare: 0 }).sessions, 0, 'a share of 0 marks nothing');
  const never = preview(four, { warmSessions: 'never' });
  assert.deepEqual([never.sessions, never.warm, never.burnShare.toFixed(2)], [1, 0, '0.30'], 'never stops after the cold pins');
  assert.deepEqual([preview(four, { migrateAboveUtilization: 0.95 }).members, preview(four, { migrateAboveUtilization: null }).members], [0, 0], 'no member at the threshold, or no threshold, replays no round');
  // Folded one burn window ago, 10 has decayed to 10/e, under the 5 folded now; a burn never folded reads as stored.
  const decayed = preview([pin('old', 10, false, { burnAt: at(NOW - 600_000) }), pin('new', 5, false), pin('unfolded', 1, false, { burnAt: null })]);
  assert.deepEqual([decayed.sessions, decayed.burnShare.toFixed(3)], [1, (5 / (10 / Math.E + 5 + 1)).toFixed(3)], 'the order and the share read each burn decayed to now');
});
