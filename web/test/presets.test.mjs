import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

test('a preset is derived from the values and custom when none matches', () => {
  assert.deepEqual(d.SHEDDING_PRESETS, {
    balanced: { burnAverageSeconds: 1_800, shedCap: 0.5, shedDwellSeconds: 900 },
    conservative: { burnAverageSeconds: 3_600, shedCap: 0.3, shedDwellSeconds: 1_800 },
    fast: { burnAverageSeconds: 900, shedCap: 0.5, shedDwellSeconds: 600 },
  });
  assert.deepEqual(d.FAILOVER_PRESETS, {
    balanced: { inFlightCap: 16, retryInPlaceSeconds: 60 },
    conservative: { inFlightCap: 16, retryInPlaceSeconds: 120 },
    fast: { inFlightCap: 8, retryInPlaceSeconds: 20 },
  });
  for (const name of ['balanced', 'conservative', 'fast']) {
    assert.equal(d.sheddingPresetOf(d.SHEDDING_PRESETS[name]), name);
    assert.equal(d.failoverPresetOf(d.FAILOVER_PRESETS[name]), name);
  }
  const shedding = d.SHEDDING_PRESETS.balanced;
  for (const off of [{ shedCap: 0.4 }, { burnAverageSeconds: 1_801 }, { shedDwellSeconds: 899 }]) assert.equal(d.sheddingPresetOf({ ...shedding, ...off }), 'custom', JSON.stringify(off));
  assert.equal(d.sheddingPresetOf({ ...d.SHEDDING_PRESETS.conservative, shedCap: 0.1 + 0.2 }), 'conservative', 'a cap that reads back a rounding off still names its preset');
  assert.deepEqual(
    [
      d.failoverPresetOf({ inFlightCap: 16, retryInPlaceSeconds: 60 }),
      d.failoverPresetOf({ inFlightCap: 16, retryInPlaceSeconds: 120 }),
      d.failoverPresetOf({ inFlightCap: 16, retryInPlaceSeconds: 20 }),
    ],
    ['balanced', 'conservative', 'custom'],
    'balanced and conservative share the cap, so the retry-in-place bound alone tells them apart',
  );
  for (const off of [{ inFlightCap: 9 }, { retryInPlaceSeconds: 21 }]) assert.equal(d.failoverPresetOf({ ...d.FAILOVER_PRESETS.fast, ...off }), 'custom', JSON.stringify(off));
});

test('a preset sets the cap, the average and the dwell and never the switch, so applying one leaves the switch as it was', () => {
  for (const [name, values] of Object.entries(d.SHEDDING_PRESETS)) assert.deepEqual(Object.keys(values).sort(), ['burnAverageSeconds', 'shedCap', 'shedDwellSeconds'], name);
  for (const [name, values] of Object.entries(d.FAILOVER_PRESETS)) assert.deepEqual(Object.keys(values).sort(), ['inFlightCap', 'retryInPlaceSeconds'], name);
  for (const on of [true, false]) {
    for (const name of ['balanced', 'conservative', 'fast']) {
      const applied = { ...{ ...d.SHEDDING_PRESETS.fast, fiveHourCeiling: 0.9, loadShedding: on, weeklyCeiling: 0.9 }, ...d.SHEDDING_PRESETS[name] };
      assert.deepEqual([applied.loadShedding, applied.fiveHourCeiling, applied.weeklyCeiling, d.sheddingPresetOf(applied)], [on, 0.9, 0.9, name], `${name} with the switch ${on ? 'on' : 'off'}`);
    }
  }
  const untouched = ['returnToPlan', 'returnToPlanDwellSeconds', 'slowDown', 'slowDownHoldSeconds', 'slowDownStretchSeconds'];
  for (const [name, values] of [...Object.entries(d.SHEDDING_PRESETS), ...Object.entries(d.FAILOVER_PRESETS)]) {
    assert.deepEqual(Object.keys(values).filter((key) => untouched.includes(key)), [], `${name} reaches none of the slow-down and return-to-plan fields`);
  }
});

const NOW = Date.parse('2026-09-16T14:30:00Z');
const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const at = (ms) => new Date(ms).toISOString();

/** A weekly reset three days out, past the five-hour bound, so a weekly window there reads its own ceiling. */
const WEEKS_RESET = at(NOW + 72 * HOUR);

/**
 * A window as the account routes serve it: its projection runs to the horizon `horizonIn` from now, and `burn` is the
 * share of it the account's sessions burn an hour. A null projection is a window the router holds no reading of. It
 * reads its own ceiling: a 5-hour window holds no reset ahead and a weekly one a reset past the five-hour bound, so the
 * round's line there is the pool's ceiling and not the wall.
 */
const win = (id, projection, horizonIn = 2 * HOUR, burn = 0.01) => ({
  burn: projection === null ? null : burn,
  horizon: projection === null ? null : at(NOW + horizonIn),
  id,
  label: id,
  observedAt: at(NOW),
  periodHours: id === '5h' ? 5 : 168,
  projection,
  resetsAt: id === '5h' ? null : WEEKS_RESET,
  utilization: projection === null ? 0.5 : Math.max(0, projection - 0.1),
});
/** A subscription account with its 5-hour and 7-day projections, and its 7-day Fable window's where one is named. */
const oauth = (id, five, seven, fable = null) => ({ fableWindow: fable === null ? null : win('7d_fable', fable, 5 * HOUR), id, kind: 'oauth', label: id, windows: [win('5h', five), win('7d', seven, 5 * HOUR)] });
/**
 * A pin as the Sessions route serves it: its burn on `burnWindow` decayed to `burnAt`, the instant of the read, main's,
 * with no line and no mark.
 */
const pin = (accountId, sessionId, burn, warm, over = {}) => ({
  accountId,
  agentContexts: [],
  agentId: 'main',
  burn,
  burnAt: at(NOW),
  burnUnit: 'window_share',
  burnWindow: 'five_hour',
  cacheTtlMs: 5 * MINUTE,
  keyId: 'desktop',
  lastInputTokens: 1_000,
  lastRefreshAt: at(warm ? NOW - MINUTE : NOW - 10 * MINUTE),
  mark: null,
  model: 'claude-sonnet-5',
  pinned: true,
  sessionId,
  ...over,
});
const member = (accountId, eligible = 'eligible', priority = 0) => ({ accountId, eligible, priority });
const pool = (accounts, over = {}) => ({
  burnAverageSeconds: 1_800,
  fiveHourCeiling: 0.9,
  loadShedding: true,
  members: accounts.map((a) => member(a.id)),
  shedCap: 0.5,
  warmSessions: 'when_needed',
  weeklyCeiling: 0.9,
  windDownMinutes: 30,
  ...over,
});
const line = (accounts, sessions, over = {}) => d.shedPreviewLine(d.shedPreview(pool(accounts, over), accounts, sessions, NOW), NOW);
// hot projects 99 % at its 5-hour horizon two hours out, with no reset ahead, nine points past the ceiling: an excess
// of 4.5 points an hour.
const hot = oauth('hot', 0.99, 0.4);
const cool = oauth('cool', 0.3, 0.2);
const three = [pin('hot', 'cold-a', 0.03, false), pin('hot', 'warm-b', 0.025, true), pin('hot', 'warm-c', 0.01, true)];

test('the round marks pins in order until the target and passes over a pin it may not mark', () => {
  const ordered = [
    { burn: 3, id: 'cold-3' },
    { burn: 4, id: 'warm-4' },
    { burn: 2, id: 'warm-2' },
    { burn: 1, id: 'warm-1' },
  ];
  assert.deepEqual(d.shedMarks(ordered, 5), ['cold-3', 'warm-4'], 'marked in order until the target is reached');
  assert.deepEqual(d.shedMarks(ordered, 0), [], 'nothing for a zero target');
  assert.deepEqual(d.shedMarks([{ burn: 1, id: 'priced' }, { burn: null, id: 'unpriced' }, { burn: 2, id: 'last' }], 3), ['priced', 'last'], 'an unpriced pin is passed over and stays the one unmarked pin');
  assert.deepEqual(d.shedMarks([{ burn: 3, id: 'stuck', skip: true }, { burn: 2, id: 'free' }, { burn: 1, id: 'last' }], 2), ['free'], 'a pin with no sibling to move to is passed over and counts nothing toward the target');
});

test('the round refuses a mark that leaves the unmarked priced pins under a tenth of its burn sum, and marks a later pin that leaves more', () => {
  // Of a sum of 1.0, a's mark leaves 0.5. b's would leave c's 0.05, under the tenth of 0.1, and c's leaves b's 0.45.
  assert.deepEqual(d.shedMarks([{ burn: 0.5, id: 'a' }, { burn: 0.45, id: 'b' }, { burn: 0.05, id: 'c' }], 0.95), ['a', 'c']);
});

test('the round marks a pin whose mark leaves exactly a tenth of its burn sum on the account', () => {
  // Of a sum of 1.0, a's mark leaves b's 0.1, the tenth itself. The sum less a's burn reads 0.09999999999999998.
  assert.deepEqual(d.shedMarks([{ burn: 0.9, id: 'a' }, { burn: 0.1, id: 'b' }], 0.9), ['a']);
});

test('the round refuses no mark while an unpriced pin stands unmarked', () => {
  // The unpriced pin's burn is unknown rather than none, so it stays on the account and b's mark leaves it there.
  assert.deepEqual(d.shedMarks([{ burn: 0.5, id: 'a' }, { burn: 0.45, id: 'b' }, { burn: 0.05, id: 'c' }, { burn: null, id: 'unpriced' }], 0.95), ['a', 'b']);
});

test('a pin the walk passes over counts among the unmarked priced pins the guard reads', () => {
  // stuck has no sibling to move to, so its 0.6 of the sum of 0.95 stays on the account beside free's mark.
  assert.deepEqual(d.shedMarks([{ burn: 0.6, id: 'stuck', skip: true }, { burn: 0.35, id: 'free' }], 0.35), ['free']);
});

test('a standing mark counts as removed and lifts no guard, a covering pin is the cheapest move, and a sliver stays', () => {
  assert.deepEqual(
    {
      // The rest is 0.5 past s's 0.1. b and c each cover it, and c's move costs 2 where b's costs 9.
      cheapest: d.shedMarks([{ burn: 0.1, id: 's', standing: true }, { burn: 0.3, cost: 5, id: 'a' }, { burn: 0.6, cost: 9, id: 'b' }, { burn: 0.6, cost: 2, id: 'c' }], 0.6),
      // The rest is 0.8 past s's 0.1, and a's 0.03 is under its tenth. c's mark after b's would leave a's 0.03 of the
      // sum of 0.93, under its tenth.
      sliver: d.shedMarks([{ burn: 0.1, id: 's', standing: true }, { burn: 0.03, id: 'a' }, { burn: 0.4, id: 'b' }, { burn: 0.4, id: 'c' }], 0.9),
      // s's 0.4 is a move pending, so the rest of the target of 0.5 is 0.1, which b covers.
      standing: d.shedMarks([{ burn: 0.4, id: 's', standing: true }, { burn: 0.35, id: 'b' }, { burn: 0.25, id: 'c' }], 0.5),
      // An unpriced pin under a standing mark is leaving, so it is not the pin that stays, and b's mark would leave
      // 0.05.
      standingUnpriced: d.shedMarks([{ burn: 0.5, id: 'a' }, { burn: 0.45, id: 'b' }, { burn: 0.05, id: 'c' }, { burn: null, id: 's', standing: true }], 0.95),
    },
    { cheapest: ['c'], sliver: ['b'], standing: ['b'], standingUnpriced: ['a', 'c'] },
  );
});

test('the preview reads one member over the ceiling and the round that marks two of its sessions, word for word', () => {
  // The cap bounds the target at half of the 6.5 points an hour the pins burn, which no pin covers alone: cold-a's 3
  // goes first, then warm-b's 2.5, the first of the two warm pins that cover the rest at one cost, reaches it. The 5.5
  // they carry fall under the 9.5 points an hour that a projection of 109 % two hours out asks for.
  const over = oauth('hot', 1.09, 0.4);
  assert.equal(line([over, cool], three), '1 member projects over the ceiling: hot. A round there would mark 2 sessions (1 warm), 5.5 %/h of the excess, to move to a sibling with runway.');
  const preview = d.shedPreview(pool([over, cool]), [over, cool], three, NOW);
  assert.deepEqual([preview.hot.map((h) => [h.label, h.window]), preview.sessions, preview.warm, preview.covered.toFixed(3)], [[['hot', '5h']], 2, 1, '0.055']);
  // The marks overshoot an excess of 4.5 points an hour, so the round covers the excess whole.
  assert.equal(line([hot, cool], three), '1 member projects over the ceiling: hot. A round there would mark 2 sessions (1 warm), 4.5 %/h of the excess, to move to a sibling with runway.');
  assert.equal(line([hot, cool], three, { shedCap: 0.25 }), '1 member projects over the ceiling: hot. A round there would mark 1 session (0 warm), 3.0 %/h of the excess, to move to a sibling with runway.', "a quarter of the burn is about 1.6 points an hour, which cold-a's cold move covers at no cost");
});

/**
 * An account as the routes serve it with its slow-down read at `factor` and a stretch of `stretchMinutes` against 30,
 * the holds having realized `speed`.
 */
const slowedAccount = (id, factor, stretchMinutes, speed = factor) => ({
  ...oauth(id, 0.5, 0.5),
  slowDown: { factor, holdMs: 120_000, since: factor < 1 ? at(NOW - 10 * MINUTE) : null, speed, stretchMs: stretchMinutes * MINUTE, thresholdMs: 30 * MINUTE },
});
/**
 * A pool with the slow-down on, its members in the given order, each carrying the factor and the speed its account
 * serves.
 */
const slowPool = (accounts, over = {}) => ({
  members: accounts.map((a, i) => ({ accountId: a.id, factor: a.slowDown?.factor ?? null, priority: i, speed: a.slowDown?.speed ?? null })),
  slowDown: true,
  slowDownHoldSeconds: 120,
  slowDownStretchSeconds: 1_800,
  ...over,
});
const slowLine = (accounts, over = {}) => d.slowDownPreviewLine(d.slowDownPreview(slowPool(accounts, over), accounts));

test('the slow-down preview reads off, no slowed member, or the slowed members and the wait on the slowest, word for word', () => {
  const studio = slowedAccount('studio', 0.5, 42);
  const unslowed = (id) => slowedAccount(id, 1, 0);
  assert.equal(slowLine([studio], { slowDown: false }), 'Slow-down is off: no request is held.', 'the switch off holds nothing, whatever a member reads');
  assert.equal(slowLine([oauth('quiet', 0.5, 0.5)]), 'No member is slowed.', 'no member served a slow-down');
  assert.equal(slowLine([unslowed('calm'), unslowed('still')]), 'No member is slowed.', 'members read at full speed');
  assert.equal(slowLine([studio, unslowed('annex')]), '1 member is slowed: studio at 0.5x (42 min of 30 min). A session there waits its cycle times 1.0, up to 120 s.');
  assert.equal(
    slowLine([slowedAccount('studio', 0.5, 42, 1)]),
    '1 member is slowed: studio at 0.5x (42 min of 30 min). A session there waits its cycle times 1.0, up to 120 s.',
    'the factor is read, never the speed the holds realized, which reads 1 until a pin there has a cycle',
  );
  assert.equal(
    slowLine([slowedAccount('alder', 0.5, 42), slowedAccount('birch', 0.35, 55), unslowed('cedar')]),
    '2 members are slowed: alder at 0.5x (42 min of 30 min), birch at 0.35x (55 min of 30 min). A session on birch waits its cycle times 1.9, up to 120 s.',
    'the closing sentence reads the slowest member',
  );
  assert.equal(slowLine([studio], { slowDownHoldSeconds: 60, slowDownStretchSeconds: 9_000 }), '1 member is slowed: studio at 0.5x (42 min of 2 h 30 min). A session there waits its cycle times 1.0, up to 60 s.', "the threshold and the hold are the pool's");
  assert.equal(slowLine([slowedAccount('full', 0, 70)]), '1 member is slowed: full at 0x (1 h 10 min of 30 min). A session there waits the full 120 s.', 'at the ceiling a session waits the whole hold');
  const alder = slowedAccount('alder', 0.5, 42);
  const ghost = { ...slowPool([alder]), members: [{ accountId: 'alder', factor: 0.5, priority: 0, speed: 0.5 }, { accountId: 'ghost', factor: 0.4, priority: 1, speed: 1 }] };
  assert.equal(d.slowDownPreviewLine(d.slowDownPreview(ghost, [alder])), '2 members are slowed: alder at 0.5x (42 min of 30 min), ghost at 0.4x. A session on ghost waits its cycle times 1.5, up to 120 s.', 'a member whose account did not load counts by its own factor, its stretch unknown');
});

test('the preview names the member closest to the ceiling when none projects over it on any window', () => {
  const near = oauth('near', 0.85, 0.3);
  assert.equal(line([near, cool], []), `No member projects over the ceiling on any window: the closest is near, 5h 85 % at ${d.instant(NOW + 2 * HOUR, NOW)}.`, 'a 5-hour horizon reads its clock time');
  const weekly = oauth('weekly', 0.4, 0.884);
  assert.equal(line([cool, weekly], []), 'No member projects over the ceiling on any window: the closest is weekly, 7d 88 % in 5h.', 'a weekly horizon reads the span to it');
  const fable = oauth('fable', 0.4, 0.5, 0.87);
  assert.equal(line([fable], [pin('fable', 'f', 0.01, false, { model: 'claude-fable-5-1' })]), 'No member projects over the ceiling on any window: the closest is fable, 7-day Fable 87 % in 5h.', "the 7-day Fable window reads where a pin's model draws it");
  assert.equal(line([fable], [pin('fable', 's', 0.01, false)]), 'No member projects over the ceiling on any window: the closest is fable, 7d 50 % in 5h.', 'and only there');
  assert.equal(line([{ id: 'key', kind: 'api_key', label: 'key', windows: [], fableWindow: null }], []), 'No member projects over the ceiling on any window.', 'a pool of API-key members alone has no window to read');
});

test('the preview reads each window against its own ceiling', () => {
  // weekly's 7-day window projects 93 % at the five-hour bound, three points past the weekly ceiling of 90: an excess
  // of 0.6 points an hour over the five hours. five's 5-hour window projects 93 % with no reset ahead, under the 5-hour
  // ceiling of 95.
  const weekly = { fableWindow: null, id: 'weekly', kind: 'oauth', label: 'weekly', windows: [{ ...win('5h', 0.3), resetsAt: null }, { ...win('7d', 0.93, 5 * HOUR), resetsAt: WEEKS_RESET }] };
  const five = { fableWindow: null, id: 'five', kind: 'oauth', label: 'five', windows: [{ ...win('5h', 0.93), resetsAt: null }, { ...win('7d', 0.2, 5 * HOUR), resetsAt: WEEKS_RESET }] };
  const preview = d.shedPreview(pool([weekly, five], { fiveHourCeiling: 0.95, weeklyCeiling: 0.9 }), [weekly, five], [], NOW);
  assert.deepEqual(preview.hot.map((h) => [h.label, h.window, h.excess.toFixed(4)]), [['weekly', '7d', '0.0060']]);
});

test("a sibling's runway reads the weekly ceiling on its 7-day window", () => {
  // hot projects 97 % with no 5-hour reset ahead, over the 5-hour ceiling of 95: an excess of one point an hour, which
  // cold-a's cold move covers. Its one sibling's 7-day window reads 97 %, under the weekly ceiling of 100.
  const over = { fableWindow: null, id: 'hot', kind: 'oauth', label: 'hot', windows: [{ ...win('5h', 0.97), resetsAt: null }, { ...win('7d', 0.4, 5 * HOUR), resetsAt: WEEKS_RESET }] };
  const sibling = { fableWindow: null, id: 'sibling', kind: 'oauth', label: 'sibling', windows: [{ ...win('5h', 0.3), resetsAt: null }, { ...win('7d', 0.97, 5 * HOUR), resetsAt: WEEKS_RESET }] };
  const preview = d.shedPreview(pool([over, sibling], { fiveHourCeiling: 0.95, weeklyCeiling: 1 }), [over, sibling], three, NOW);
  assert.deepEqual([preview.sessions, preview.noRunway], [1, false]);
});

test("closest ranks by the room to a window's own ceiling", () => {
  // near's 5-hour window projects 90 %, five points under the 5-hour ceiling of 95. high's 7-day window projects 94 %,
  // higher, but six points under the weekly ceiling of 100.
  const near = { fableWindow: null, id: 'near', kind: 'oauth', label: 'near', windows: [{ ...win('5h', 0.9), resetsAt: null }, { ...win('7d', 0.2, 5 * HOUR), resetsAt: WEEKS_RESET }] };
  const high = { fableWindow: null, id: 'high', kind: 'oauth', label: 'high', windows: [{ ...win('5h', 0.3), resetsAt: null }, { ...win('7d', 0.94, 5 * HOUR), resetsAt: WEEKS_RESET }] };
  const preview = d.shedPreview(pool([near, high], { fiveHourCeiling: 0.95, weeklyCeiling: 1 }), [near, high], [], NOW);
  assert.deepEqual(preview.closest, { horizon: NOW + 2 * HOUR, label: 'near', projection: 0.9, window: '5h' });
});

test("the round's line is the wall at a reset and at the account's end", () => {
  const hotOf = (account, ceilings) => d.shedPreview(pool([account], ceilings), [account], [], NOW).hot.map((h) => [h.window, h.excess.toFixed(4)]);
  const defaults = { fiveHourCeiling: 0.95, weeklyCeiling: 1 };
  const weekly = { fiveHourCeiling: 0.95, weeklyCeiling: 0.9 };
  // Each tested window reads its horizon two hours out. A 5-hour reset there and the account's end there bound it at
  // the wall, and so does a weekly reset there, since it lies inside the five hours.
  const twoHours = at(NOW + 2 * HOUR);
  const quiet7d = { ...win('7d', 0.2, 5 * HOUR), resetsAt: WEEKS_RESET };
  const quiet5h = { ...win('5h', 0.3), resetsAt: null };
  const fiveAtWall = { fableWindow: null, id: 'm', kind: 'oauth', label: 'm', windows: [{ ...win('5h', 0.97), resetsAt: twoHours }, quiet7d] };
  const fiveOverWall = { fableWindow: null, id: 'm', kind: 'oauth', label: 'm', windows: [{ ...win('5h', 1.03), resetsAt: twoHours }, quiet7d] };
  const fiveWithoutReset = { fableWindow: null, id: 'm', kind: 'oauth', label: 'm', windows: [{ ...win('5h', 0.97), resetsAt: null }, quiet7d] };
  const weekAtReset = { fableWindow: null, id: 'm', kind: 'oauth', label: 'm', windows: [quiet5h, { ...win('7d', 0.93, 2 * HOUR), resetsAt: twoHours }] };
  const weekAtEnd = { ...weekAtReset, canceled: true, subscription: { endsAt: twoHours, trialEndsAt: null }, windows: [quiet5h, { ...win('7d', 0.93, 2 * HOUR), resetsAt: WEEKS_RESET }] };
  assert.deepEqual(
    {
      fiveAtWall: hotOf(fiveAtWall, defaults),
      fiveOverWall: hotOf(fiveOverWall, defaults),
      fiveWithoutReset: hotOf(fiveWithoutReset, defaults),
      weekAtEnd: hotOf(weekAtEnd, weekly),
      weekAtReset: hotOf(weekAtReset, weekly),
    },
    // 103 % three points over the wall two hours out is 1.5 points an hour, and 97 % two points over the 5-hour ceiling
    // with no reset 1 point an hour.
    { fiveAtWall: [], fiveOverWall: [['5h', '0.0150']], fiveWithoutReset: [['5h', '0.0100']], weekAtEnd: [], weekAtReset: [] },
  );
});

test('the preview names every member over the ceiling, and says no sibling has runway when none can take their sessions', () => {
  const second = oauth('second', 0.4, 0.95);
  const pins = [...three, pin('second', 'a', 0.05, false), pin('second', 'b', 0.01, false)];
  assert.equal(line([hot, second], pins), '2 members project over the ceiling: hot, second. No sibling has runway for them, so nothing would be marked.');
  const cooled = { ...pool([hot, cool]), members: [member('hot'), member('cool', 'cooldown')] };
  assert.equal(d.shedPreviewLine(d.shedPreview(cooled, [hot, cool], three, NOW), NOW), '1 member projects over the ceiling: hot. No sibling has runway for them, so nothing would be marked.', 'a member the router skips is no destination');
  const unread = oauth('unread', null, 0.2);
  assert.equal(line([hot, unread], three), '1 member projects over the ceiling: hot. No sibling has runway for them, so nothing would be marked.', 'a sibling with no reading on a window the pin draws has no runway');
  const weekOnly = { fableWindow: null, id: 'week-only', kind: 'oauth', label: 'week-only', windows: [win('7d', 0.2, 5 * HOUR)] };
  assert.equal(line([hot, weekOnly], three), '1 member projects over the ceiling: hot. A round there would mark 2 sessions (1 warm), 4.5 %/h of the excess, to move to a sibling with runway.', 'a sibling whose window set lacks the 5-hour window has runway on its 7-day window');
  const room = { fableWindow: null, id: 'key', kind: 'api_key', label: 'key', windows: [{ id: 'itpm:claude-sonnet-5', label: 'itpm', observedAt: at(NOW), periodHours: 1 / 60, resetsAt: at(NOW + MINUTE), utilization: 0.4 }] };
  assert.equal(line([hot, room], three), '1 member projects over the ceiling: hot. A round there would mark 2 sessions (1 warm), 4.5 %/h of the excess, to move to a sibling with runway.', 'an API-key sibling with room in its bucket takes them');
  const ordered = d.shedPreview({ ...pool([second, hot, cool]), members: [member('second', 'eligible', 1), member('hot'), member('cool', 'eligible', 1)] }, [second, hot, cool], pins, NOW);
  assert.deepEqual(ordered.hot.map((h) => h.label), ['hot', 'second'], 'the members in the order the Members group lists them, priority first');
});

test("a sibling whose plan ends inside the pool's wind-down has no runway, whatever its projection", () => {
  const ending = (hours) => ({ ...cool, canceled: true, subscription: { endsAt: at(NOW + hours * HOUR), trialEndsAt: null } });
  const marked = '1 member projects over the ceiling: hot. A round there would mark 2 sessions (1 warm), 4.5 %/h of the excess, to move to a sibling with runway.';
  const stranded = '1 member projects over the ceiling: hot. No sibling has runway for them, so nothing would be marked.';
  assert.equal(line([hot, ending(4)], three, { windDownMinutes: 180 }), marked, 'an end four hours out lies past a three-hour wind-down');
  assert.equal(line([hot, ending(4)], three, { windDownMinutes: 300 }), stranded, 'and inside a five-hour one');
  assert.equal(line([hot, ending(4)], three, { windDownMinutes: 240 }), stranded, 'an end exactly the wind-down away is inside it');
});

test('a member over the ceiling with no priced pin reads that a round would mark no session', () => {
  const said = '1 member projects over the ceiling: hot. A round there would mark no session: none is priced.';
  assert.equal(line([hot, cool], [pin('hot', 'a', null, false), pin('hot', 'b', null, true)]), said, 'every pin unpriced');
  assert.equal(line([hot, cool], []), said, 'no pin at all');
});

test('the preview reads no priced pin only where a round walks: the switch on and a member over the ceiling', () => {
  assert.equal(d.shedPreview(pool([hot, cool], { loadShedding: false }), [hot, cool], [], NOW).unpriced, false, 'the switch off walks no member');
  const near = oauth('near', 0.85, 0.3);
  assert.equal(d.shedPreview(pool([near, cool]), [near, cool], [], NOW).unpriced, false, 'no member over the ceiling walks none');
  assert.equal(d.shedPreview(pool([hot, cool]), [hot, cool], [], NOW).unpriced, true, 'a member over the ceiling with no pin to price');
});

/** The round's sentence over `hot` when it would mark no priced pin for `reason`. */
const quiet = (reason) => `1 member projects over the ceiling: hot. A round there would mark no session: ${reason}`;

test("a round that would mark no priced pin because each is its member's last says so", () => {
  assert.equal(line([hot, cool], [pin('hot', 'only', 0.03, false)]), quiet("each is its member's last."));
});

test('a round under never that would mark no priced pin because each is warm says so', () => {
  assert.equal(line([hot, cool], [pin('hot', 'a', 0.03, true), pin('hot', 'b', 0.02, true)], { warmSessions: 'never' }), quiet('each is warm.'));
});

test('a round that would mark no priced pin because none burns, a zero target, says so', () => {
  assert.equal(line([hot, cool], [pin('hot', 'a', 0, false), pin('hot', 'b', 0, false)]), quiet('none burns.'));
});

test("the preview reads a sibling's runway on every window the pin's model draws", () => {
  const fableFull = oauth('busy', 0.3, 0.2, 0.92);
  const fable = (id, burn) => pin('hot', id, burn, false, { model: 'claude-fable-5-1' });
  const pins = [fable('fable-a', 0.04), pin('hot', 'sonnet-b', 0.03, false), pin('hot', 'sonnet-c', 0.01, false)];
  // The Fable pin draws the 7-day Fable window, where busy sits over the ceiling, so the round passes over it and marks
  // the two Sonnet pins in its place, 3 then 1 points an hour, which reach the cap's 4. The Fable pin stays the last
  // unmarked.
  const preview = d.shedPreview(pool([hot, fableFull]), [hot, fableFull], pins, NOW);
  assert.deepEqual([preview.hot.map((h) => h.label), preview.sessions, preview.covered.toFixed(3)], [['hot'], 2, '0.040'], 'busy draws no Fable pin of its own, so its 7-day Fable window fires no round there');
  assert.equal(d.shedPreviewLine(preview, NOW), '1 member projects over the ceiling: hot. A round there would mark 2 sessions (0 warm), 4.0 %/h of the excess, to move to a sibling with runway.');
});

test('a round fired by the 7-day Fable window counts the pins whose model draws it alone, so it plans no Sonnet move', () => {
  // fable-hot crosses on its 7-day Fable window alone: 99 % five hours out under the ceiling of 90, an excess of 1.8
  // points an hour, while its shared windows sit under it. The Sonnet pin burns five points an hour and the two Fable
  // pins one and a half between them, but only the Fable pins draw the week that fired, so the Sonnet pin sits the
  // round out and the cap's 0.75 points an hour are reached by the heavier Fable pin alone.
  const fableHot = oauth('fable-hot', 0.5, 0.4, 0.99);
  const sibling = oauth('cool', 0.3, 0.2, 0.3);
  const fable = (id, burn) => pin('fable-hot', id, burn, false, { model: 'claude-fable-5-1' });
  const pins = [pin('fable-hot', 'sonnet-a', 0.05, false), fable('fable-b', 0.01), fable('fable-c', 0.005)];
  const preview = d.shedPreview(pool([fableHot, sibling]), [fableHot, sibling], pins, NOW);
  assert.deepEqual(preview.hot.map((h) => [h.label, h.window, h.excess.toFixed(3)]), [['fable-hot', '7d_fable', '0.018']]);
  assert.deepEqual([preview.sessions, preview.covered.toFixed(3), preview.unpriced], [1, '0.010', false], "fable-b's point alone, never the Sonnet pin's five");
  assert.equal(d.shedPreviewLine(preview, NOW), '1 member projects over the ceiling: fable-hot. A round there would mark 1 session (0 warm), 1.0 %/h of the excess, to move to a sibling with runway.');
});

test("a pin whose model does not draw the fired window is out of the preview's round", () => {
  // hot crosses on its 7-day Fable window alone. The Sonnet pins draw no 7-day Fable window, so the round walks the
  // Fable pin by itself, and a member's last pin is never marked, however much the Sonnet pins burn on the shared
  // windows.
  const fableHot = oauth('hot', 0.3, 0.2, 0.97);
  const sibling = oauth('cool', 0.3, 0.2, 0.3);
  const pins = [pin('hot', 'fable-a', 0.01, false, { model: 'claude-fable-5-1' }), pin('hot', 'sonnet-b', 0.05, false), pin('hot', 'sonnet-c', 0.03, false)];
  const preview = d.shedPreview(pool([fableHot, sibling]), [fableHot, sibling], pins, NOW);
  assert.deepEqual([preview.hot.map((h) => h.window), preview.quiet, preview.sessions], [['7d_fable'], 'last', 0]);
  assert.equal(d.shedPreviewLine(preview, NOW), quiet("each is its member's last."));
});

test('the preview counts an unpriced pin on the window for the last-pin rule still', () => {
  // On a 5-hour round both models draw the window, so the Sonnet pin served in dollars stays in the order unpriced and
  // the Fable pin is not its member's last.
  const sibling = oauth('cool', 0.3, 0.2, 0.3);
  const pins = [pin('hot', 'fable-a', 0.03, false, { model: 'claude-fable-5-1' }), pin('hot', 'sonnet-b', 0.02, false, { burnUnit: 'usd' })];
  const preview = d.shedPreview(pool([hot, sibling]), [hot, sibling], pins, NOW);
  assert.deepEqual([preview.hot.map((h) => h.window), preview.sessions, preview.quiet], [['5h'], 1, null]);
  assert.equal(d.shedPreviewLine(preview, NOW), '1 member projects over the ceiling: hot. A round there would mark 1 session (0 warm), 3.0 %/h of the excess, to move to a sibling with runway.');
});

/**
 * `oauth` with its 5-hour window resetting `resetIn` from now, its horizon there. The window starts five hours before
 * the reset, or at the reset once it has passed. A reset ahead sets the round's line at the wall.
 */
const fresh = (id, five, seven, resetIn) => ({ ...oauth(id, five, seven), windows: [{ ...win('5h', five, resetIn), resetsAt: at(NOW + resetIn) }, win('7d', seven, 5 * HOUR)] });
/** The sentence that names a member whose 5-hour round the hold, twice the test pool's 30-minute average, took away. */
const heldSentence = (label, age) => `${label}'s 5-hour window is ${age} old, under the hold of 1 h, so its round reads the next window.`;

test('a member whose 5-hour window is under the hold is read on its next window and named', () => {
  // hot's 5-hour window reset ten minutes ago and projects past the wall at its next reset, so its round reads the
  // 7-day window, over the ceiling too: 97 % five hours out, an excess of 1.4 points an hour, which cold-a's 3 covers
  // alone.
  const young = fresh('hot', 1.09, 0.97, 4 * HOUR + 50 * MINUTE);
  const preview = d.shedPreview(pool([young, cool]), [young, cool], three, NOW);
  assert.deepEqual(preview.hot.map((h) => h.window), ['7d']);
  assert.deepEqual(preview.held, [{ ageMs: 600_000, holdMs: 3_600_000, id: 'hot', label: 'hot' }]);
  assert.equal(d.shedPreviewLine(preview, NOW), `1 member projects over the ceiling: hot. A round there would mark 1 session (0 warm), 1.4 %/h of the excess, to move to a sibling with runway. ${heldSentence('hot', '10 min')}`);
  assert.equal(line([young, cool], three, { loadShedding: false }), `Load shedding is off: no round runs. 1 member projects over the ceiling: hot. ${heldSentence('hot', '10 min')}`, 'the switch off names the held member too');
});

test('a held member with no other window over the ceiling is named beside the closest', () => {
  const young = fresh('hot', 1.09, 0.4, 4 * HOUR + 50 * MINUTE);
  const preview = d.shedPreview(pool([young, cool]), [young, cool], three, NOW);
  assert.deepEqual([preview.hot.length, preview.closest], [0, { horizon: NOW + 5 * HOUR, label: 'hot', projection: 0.4, window: '7d' }]);
  // The closest weekly window reads the span to its horizon, whose spelling the duration format decides.
  const said = d.shedPreviewLine(preview, NOW);
  assert.ok(said.startsWith('No member projects over the ceiling on any window: the closest is hot, 7d 40 % in '), said);
  assert.ok(said.endsWith(heldSentence('hot', '10 min')), said);
});

test('a passed reset with no new one starts the window at the reset', () => {
  const passed = fresh('hot', 0.99, 0.4, -5 * MINUTE);
  const preview = d.shedPreview(pool([passed, cool]), [passed, cool], three, NOW);
  assert.deepEqual([preview.hot.length, preview.held], [0, [{ ageMs: 300_000, holdMs: 3_600_000, id: 'hot', label: 'hot' }]]);
});

test('a window older than the hold fires as today', () => {
  const read = (resetIn) => {
    const member = fresh('hot', 1.09, 0.4, resetIn);
    const preview = d.shedPreview(pool([member, cool]), [member, cool], three, NOW);
    return [preview.hot.map((h) => h.window), preview.held];
  };
  assert.deepEqual(read(3 * HOUR + 59 * MINUTE), [['5h'], []], '61 minutes old');
  assert.deepEqual(read(4 * HOUR), [['5h'], []], 'a window exactly as old as the hold fires');
});

test('a 5-hour reset more than five hours ahead starts no fresh window, so nothing is held', () => {
  const ahead = fresh('hot', 1.09, 0.4, 6 * HOUR);
  const preview = d.shedPreview(pool([ahead, cool]), [ahead, cool], three, NOW);
  assert.deepEqual([preview.hot.map((h) => h.window), preview.held], [['5h'], []]);
});

test('a held member whose 5-hour projection reads under the ceiling is not named', () => {
  // calm's 5-hour window is ten minutes old and reads 50 %, so the hold took no round away from it.
  const calm = fresh('calm', 0.5, 0.2, 4 * HOUR + 50 * MINUTE);
  const preview = d.shedPreview(pool([hot, calm]), [hot, calm], three, NOW);
  assert.deepEqual(preview.held, []);
  assert.equal(d.shedPreviewLine(preview, NOW), '1 member projects over the ceiling: hot. A round there would mark 2 sessions (1 warm), 4.5 %/h of the excess, to move to a sibling with runway.');
});

test('a held 5-hour window is not the closest reading, however near the ceiling it projects', () => {
  // young's 5-hour window reads 85 %, the highest in the pool, but it opened ten minutes ago, so its projection rests
  // on the burn the last window left behind, the reading the hold distrusts. No round reads it, so the closest is the
  // nearest window a round would read: low's 5-hour window at 60 %.
  const young = fresh('young', 0.85, 0.4, 4 * HOUR + 50 * MINUTE);
  const low = oauth('low', 0.6, 0.3);
  const preview = d.shedPreview(pool([young, low]), [young, low], [], NOW);
  assert.deepEqual(preview.closest, { horizon: NOW + 2 * HOUR, label: 'low', projection: 0.6, window: '5h' });
  assert.deepEqual(preview.held, []);
  assert.equal(d.shedPreviewLine(preview, NOW), `No member projects over the ceiling on any window: the closest is low, 5h 60 % at ${d.instant(NOW + 2 * HOUR, NOW)}.`);
});

test('a pin on an account without a 5-hour window scales its 7-day burn by 1 on a 7-day round and by the Fable burn over the 7-day burn on a Fable round', () => {
  // approved has no 5-hour window, so its pins' burn is a share of the 7-day window. On the 7-day round, 97 % at a
  // horizon five hours out under a ceiling of 95 asks 0.4 points an hour, which a's 5 points cover.
  const approved = { fableWindow: null, id: 'approved', kind: 'oauth', label: 'approved', windows: [win('7d', 0.97, 5 * HOUR, 0.02)] };
  const weekly = [pin('approved', 'a', 0.05, false, { burnWindow: 'seven_day' }), pin('approved', 'b', 0.03, false, { burnWindow: 'seven_day' })];
  assert.equal(line([approved, cool], weekly, { fiveHourCeiling: 0.95, weeklyCeiling: 0.95 }),'1 member projects over the ceiling: approved. A round there would mark 1 session (0 warm), 0.4 %/h of the excess, to move to a sibling with runway.');
  // On the Fable round the account burns its Fable week half as fast as its 7-day window, so the pins' 3 and 2 points
  // read 1.5 and 1 there. 99 % two hours out asks 2 points an hour, the cap's half of their 2.5 bounds it at 1.25, and
  // a's 1.5 covers that alone.
  const fable = { fableWindow: win('7d_fable', 0.99, 2 * HOUR, 0.01), id: 'fable', kind: 'oauth', label: 'fable', windows: [win('7d', 0.5, 5 * HOUR, 0.02)] };
  const sibling = oauth('sibling', 0.3, 0.2, 0.2);
  const pins = [pin('fable', 'a', 0.03, false, { burnWindow: 'seven_day', model: 'claude-fable-5' }), pin('fable', 'b', 0.02, false, { burnWindow: 'seven_day', model: 'claude-fable-5' })];
  const preview = d.shedPreview(pool([fable, sibling], { fiveHourCeiling: 0.95, weeklyCeiling: 0.95 }), [fable, sibling], pins, NOW);
  assert.deepEqual(preview.hot.map((h) => [h.label, h.window]), [['fable', '7d_fable']]);
  assert.equal(d.shedPreviewLine(preview, NOW), '1 member projects over the ceiling: fable. A round there would mark 1 session (0 warm), 1.5 %/h of the excess, to move to a sibling with runway.');
});

test('with load shedding off the preview opens by saying no round runs, then names the projection alone', () => {
  assert.equal(line([hot, cool], three, { loadShedding: false }), 'Load shedding is off: no round runs. 1 member projects over the ceiling: hot.');
  const near = oauth('near', 0.85, 0.3);
  assert.equal(line([near], [], { loadShedding: false }), `Load shedding is off: no round runs. No member projects over the ceiling on any window: the closest is near, 5h 85 % at ${d.instant(NOW + 2 * HOUR, NOW)}.`);
});

test('the round marks cold pins first, then warm ones by burn over the prompt they would rewrite, all by that under always, and cold ones alone under never', () => {
  // The cap is half of the 4.9 points an hour the three pins burn, which no pin covers alone. By burn over rewrite
  // warm-b's 2 over 100 tokens leads warm-e's 0.9 over 1000 and cold-a's 2 over 5000. Once one pin is marked, each pin
  // left covers the rest of 0.45 points, and the round marks the cheapest move among those it may mark.
  const pins = [pin('hot', 'cold-a', 0.02, false, { lastInputTokens: 5_000 }), pin('hot', 'warm-b', 0.02, true, { lastInputTokens: 100 }), pin('hot', 'warm-e', 0.009, true)];
  const read = (warmSessions) => {
    const p = d.shedPreview(pool([hot, cool], { warmSessions }), [hot, cool], pins, NOW);
    return [p.sessions, p.warm];
  };
  assert.deepEqual(read('when_needed'), [2, 1], "cold-a first, then warm-b, whose rewrite is cheaper than warm-e's");
  assert.deepEqual(read('always'), [2, 2], "warm-b first, then warm-e, whose rewrite is cheaper than cold-a's");
  assert.deepEqual(read('never'), [1, 0], 'cold-a alone, short of the cap, and no warm pin');
});

test("the round marks the covering pin whose rewrite costs the least, though another stands before it in the order", () => {
  // A cap of a quarter puts the target at 1.75 of the 7 points an hour the two warm pins burn, and each covers it.
  // warm-x leads the order by burn over prompt, 4 over 1,000 tokens against warm-y's 3 over 800, and warm-y's rewrite
  // of 800 tokens is the cheaper move, so the round marks warm-y and removes its 3 points.
  const pins = [pin('hot', 'warm-x', 0.04, true), pin('hot', 'warm-y', 0.03, true, { lastInputTokens: 800 })];
  const preview = d.shedPreview(pool([hot, cool], { shedCap: 0.25 }), [hot, cool], pins, NOW);
  assert.deepEqual([preview.sessions, preview.warm, preview.covered], [1, 1, 0.03]);
});

test("the round decays each pin's served burn from the instant it was served to the preview's own, and never marks an unpriced pin", () => {
  // Served half an hour, one burn average, before the preview's instant, cold-a's 3 points an hour read 3/e there,
  // under cold-b's 1.2, so cold-b leads the order and alone reaches the cap's half of their 2.3. The unpriced pin
  // stays.
  const aged = [pin('hot', 'cold-a', 0.03, false, { burnAt: at(NOW - 30 * MINUTE) }), pin('hot', 'cold-b', 0.012, false), pin('hot', 'cold-c', null, false)];
  const preview = d.shedPreview(pool([hot, cool]), [hot, cool], aged, NOW);
  assert.deepEqual([preview.sessions, preview.covered.toFixed(4)], [1, '0.0120']);
  const unpriced = d.shedPreview(pool([hot, cool], { shedCap: 1 }), [hot, cool], [pin('hot', 'priced', 0.03, false), pin('hot', 'unpriced', null, false)], NOW);
  assert.deepEqual([unpriced.sessions, unpriced.covered], [1, 0.03], 'the unpriced pin stays unmarked and counts among the pins, so the priced one is not the last');
});

test("the preview's round leaves out a pin whose line reads its agent ended, from its order, its sum and its count", () => {
  // With both pins walked the cap's half of their 5 points an hour is 2.5, which a's 3 covers, leaving b's 2 on hot.
  // With b's agent ended the round walks a alone, and its mark would leave none of the burn on hot.
  const b = (ended) => pin('hot', 'b', 0.02, false, { agentContexts: [{ agentId: 'main', ended, model: 'claude-sonnet-5' }] });
  const read = (sessions) => {
    const preview = d.shedPreview(pool([hot, cool]), [hot, cool], sessions, NOW);
    return [preview.sessions, preview.covered, preview.quiet];
  };
  assert.deepEqual(read([pin('hot', 'a', 0.03, false), b(false)]), [1, 0.03, null]);
  assert.deepEqual(read([pin('hot', 'a', 0.03, false), b(true)]), [0, 0, 'last']);
});
