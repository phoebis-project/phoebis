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
  burnUnit: 'window_share',
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
/** Every account the cost table sizes, so each subscription member's pins enter the round. */
const SIZED = new Set(['cool', 'hot', 'near', 'second']);
const preview = (sessions, over) => d.shedPreview(pool(over), [hot, cool], sessions, NOW, SIZED);
const four = [pin('warm-4', 4, true), pin('cold-3', 3, false), pin('warm-2', 2, true), pin('warm-1', 1, true)];
const member = (accountId, eligible = 'eligible') => ({ accountId, eligible, priority: 0 });
/** A round over `accounts`, every one of them a member, with the pool's balanced shedding settings. */
const replay = (accounts, sessions, over = {}) => d.shedPreview(pool({ members: accounts.map((a) => member(a.id)), ...over }), accounts, sessions, NOW, SIZED);

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
  assert.deepEqual([round.sessions, round.warm, round.burnShare.toFixed(2), round.members, round.unit], [2, 1, '0.70', 1, 'window_share'], 'cold 3 then warm 4 reach half of 10, and the cool member runs no round');
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

test('a pin the table cannot price stays in the order unmarked and counts for the last-pin rule', () => {
  const unlisted = four.map((p) => (p.sessionId === 'warm-1' ? { ...p, burn: null } : p));
  const round = replay([hot, cool], unlisted);
  assert.deepEqual([round.sessions, round.warm, round.unpriced], [2, 1, 1], 'cold 3 then warm 4, as with every pin priced');
  assert.ok(d.shedPreviewLine(round, true).endsWith(' 1 pin the table cannot price stays.'), d.shedPreviewLine(round, true));
  const pair = replay([hot, cool], [pin('priced', 1, false), pin('unlisted', null, false)]);
  assert.deepEqual([pair.sessions, pair.unpriced], [1, 1], 'without the unpriced pin the priced one would be the last unmarked');
  assert.deepEqual(
    d.shedMarks(
      [
        { burn: 1, id: 'priced' },
        { burn: null, id: 'unlisted' },
        { burn: 2, id: 'last' },
      ],
      3,
    ),
    ['priced', 'last'],
    'the unpriced pin is passed over and is the one unmarked pin the rule keeps',
  );
});

test("the pins of an account whose tier the table does not size stay out of the preview, unlike a pin it cannot price", () => {
  // The route reads no burn for any pin of such an account, while the round prices them on the reference tier and
  // sheds them; the preview cannot replay that order, so it leaves them out rather than call them unpriced.
  const team = account('team', 0.95);
  const pins = [pin('team-a', null, false, { accountId: 'team' }), pin('team-b', null, false, { accountId: 'team' })];
  const round = d.shedPreview(pool({ members: [member('team'), member('cool')] }), [team, cool], pins, NOW, new Set(['cool']));
  assert.deepEqual([round.members, round.sessions, round.unpriced], [1, 0, 0]);
  assert.equal(d.shedPreviewLine(round, true).includes('cannot price'), false, d.shedPreviewLine(round, true));
  const overage = d.shedPreview(pool({ members: [member('team'), member('cool')] }), [team, cool], pins.map((p, i) => ({ ...p, burn: i + 1, burnUnit: 'usd' })), NOW, new Set(['cool']));
  assert.equal(overage.sessions, 1, 'in dollars the round reads no tier, so the pins enter it');
});

test('under never an unpriced cold pin keeps no warm mark', () => {
  const pins = [pin('cold-priced', 1, false), pin('cold-unlisted', null, false), pin('warm', 5, true)];
  const round = replay([hot, cool], pins, { shedShare: 0.9, warmSessions: 'never' });
  assert.deepEqual([round.sessions, round.warm], [1, 0], 'the marks are cut at the one priced cold pin, not at both cold pins');
});

test('a round under always ranks a warm pin ahead of a cold one by burn over the prompt it would rewrite', () => {
  // Burn over rewrite: warm-b's 3/100 leads cold-a's 4/1000 and cold-c's 3/1000, though both cold pins burn as much.
  const pins = [pin('cold-a', 4, false), pin('warm-b', 3, true, { lastInputTokens: 100 }), pin('cold-c', 3, false)];
  const whenNeeded = replay([hot, cool], pins);
  const always = replay([hot, cool], pins, { warmSessions: 'always' });
  assert.deepEqual([whenNeeded.sessions, whenNeeded.warm, whenNeeded.burnShare.toFixed(2)], [2, 0, '0.70'], 'cold-a then cold-c, the warm pin left');
  assert.deepEqual([always.sessions, always.warm, always.burnShare.toFixed(2)], [2, 1, '0.70'], 'warm-b then cold-a');
});

test('two hot members in one unit share the burn by weight, and members in two units average their shares', () => {
  const second = account('second', 0.85);
  const heavy = (unit) => [pin('big', 18, false, { accountId: 'second', burnUnit: unit }), pin('small', 2, false, { accountId: 'second', burnUnit: unit })];
  // The four pins mark 7 of 10; the second member marks 18 of 20.
  const same = replay([hot, second, cool], [...four, ...heavy('window_share')]);
  assert.deepEqual([same.sessions, same.warm, same.members, same.unit, same.burnShare.toFixed(3)], [3, 1, 2, 'window_share', (25 / 30).toFixed(3)]);
  assert.equal(d.shedPreviewLine(same, true), 'Would move 3 sessions, 1 warm, 83% of the burn in window share, across 2 accounts.');
  const mixed = replay([hot, second, cool], [...four, ...heavy('usd')]);
  assert.deepEqual([mixed.unit, mixed.averaged, mixed.burnShare.toFixed(3)], [null, true, ((0.7 + 0.9) / 2).toFixed(3)], 'a window share and dollars cannot be summed, so each member counts once');
  assert.equal(same.averaged, false);
  assert.equal(d.shedPreviewLine(mixed, true), "Would move 3 sessions, 1 warm, 80% of each account's burn on average, across 2 accounts.");
});

test('with no member under the band the preview marks sessions it cannot move', () => {
  // Under the threshold of 80 %, so it runs no round of its own, but above the 75 % a shed session has to land under.
  const near = account('near', 0.78);
  const stuck = replay([hot, near], four);
  assert.deepEqual([stuck.sessions, stuck.cool], [2, false]);
  assert.equal(d.shedPreviewLine(stuck, true), 'Would mark 2 sessions, 1 warm, 70% of the burn in window share, but no member reads under the band to take them.');
  const cooling = d.shedPreview(pool({ members: [member('hot'), member('cool', 'cooldown')] }), [hot, cool], four, NOW, SIZED);
  assert.equal(cooling.cool, false, 'a member the router skips is no destination');
  const open = replay([hot, near, cool], four);
  assert.deepEqual([open.cool, d.shedPreviewLine(open, true)], [true, 'Would move 2 sessions, 1 warm, 70% of the burn in window share.']);
  assert.equal(d.shedPreviewLine(open, false), 'No round runs while migrate above utilization is off.');
  assert.equal(d.shedPreviewLine(replay([cool], []), true), 'No member is above the threshold.');
});

test('while the cost table is loading or did not load the preview names the table and counts nothing', () => {
  const round = replay([hot, cool], four);
  assert.equal(d.shedPreviewLine(round, true, 'loading'), 'The cost table is loading; window-share pins are counted once it is read.');
  assert.equal(d.shedPreviewLine(round, true, 'missing'), 'The cost table did not load, so window-share pins cannot be counted.');
  assert.equal(d.shedPreviewLine(round, true, 'ready'), 'Would move 2 sessions, 1 warm, 70% of the burn in window share.', 'a read table counts as before');
  assert.equal(d.shedPreviewLine(round, false, 'loading'), 'No round runs while migrate above utilization is off.', 'with migrate off no round runs, table or not');
});
