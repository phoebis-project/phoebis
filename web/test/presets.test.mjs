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
 * `oauth` with its 5-hour window resetting `resetIn` from now, its horizon there. The window starts five hours before
 * the reset, or at the reset once it has passed. A reset ahead sets the round's line at the wall.
 */
const fresh = (id, five, seven, resetIn) => ({ ...oauth(id, five, seven), windows: [{ ...win('5h', five, resetIn), resetsAt: at(NOW + resetIn) }, win('7d', seven, 5 * HOUR)] });
/**
 * A pin as the pool's pins route serves it: main's, its burn on `burnWindow` decayed to `burnAt`, the instant of the
 * read, warm until four minutes after it or cold since five minutes before it, and no mark.
 */
const pin = (accountId, sessionId, burn, warm, over = {}) => ({
  accountBound: false,
  accountId,
  agentId: 'main',
  burn,
  burnAt: at(NOW),
  burnUnit: 'window_share',
  burnWindow: 'five_hour',
  ended: false,
  fast: false,
  keyId: 'desktop',
  mark: null,
  model: 'claude-sonnet-5',
  moveCost: null,
  moveDropCost: null,
  moveDropSize: null,
  moveSize: null,
  sessionId,
  warm,
  warmUntil: at(warm ? NOW + 4 * MINUTE : NOW - 5 * MINUTE),
  ...over,
});
const member = (accountId, eligible = 'eligible', priority = 0) => ({ accountId, eligible, fableReservation: null, priority });
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
const line = (accounts, pins, over = {}) => d.shedPreviewLine(d.shedPreview(pool(accounts, over), accounts, pins, NOW), NOW);
// hot projects 109 % at its 5-hour reset two hours out, nine points past the wall: an excess of 4.5 points an hour. Its
// window opened three hours ago, past the hold.
const hot = fresh('hot', 1.09, 0.4, 2 * HOUR);
const cool = oauth('cool', 0.3, 0.2);
const three = [pin('hot', 'cold-a', 0.03, false), pin('hot', 'warm-b', 0.025, true), pin('hot', 'warm-c', 0.01, true)];

/**
 * hot projecting 109 % at its 5-hour reset a minute out, nine points past the wall: an excess of 540 points an hour, so
 * the cap's share of the pins' burn sets a round's target.
 */
const steep = fresh('hot', 1.09, 0.4, MINUTE);
/** What `steep`'s round beside `cool` marks over `pins`: its sessions, the warm ones and the burn they carry. */
const marksOf = (pins, over = {}) => {
  const preview = d.shedPreview(pool([steep, cool], over), [steep, cool], pins, NOW);
  return [preview.sessions, preview.warm, preview.covered.toFixed(3)];
};

test('the round marks a pin whose mark leaves exactly a tenth of its burn sum on the account', () => {
  // A cap of 0.9 puts the target at 0.9 of the two pins' 1.0, which a's 0.9 covers, and its mark leaves b's 0.1, the
  // tenth itself: 1 session, none warm, 0.9. The sum less a's burn reads 0.09999999999999998, so a guard that read it
  // would refuse a and mark b: 1 session, none warm, 0.1.
  assert.deepEqual(marksOf([pin('hot', 'a', 0.9, false), pin('hot', 'b', 0.1, false)], { shedCap: 0.9 }), [1, 0, '0.900']);
});

test('a standing mark lifts no guard, and an unpriced pin under one is not the pin that stays', () => {
  const mark = { at: at(NOW - 5 * MINUTE), on: 'hot', window: 'five_hour' };
  assert.deepEqual(
    {
      // s's 1 point an hour under its mark is a move pending, and the cold a's 0.3 under a tenth of the rest stays as
      // a sliver. The warm b's mark leaves a's 0.3 and c's 4, and c's after it would leave a's 0.3 alone, under a tenth
      // of the 9.3 points the pins burn: 1 session, 1 warm, 4 points. A guard that counted s's point among the pins
      // that stay would let c's mark through: 2 sessions, 2 warm, 8 points. A cap of 1 sets the target at the whole
      // 9.3, and any target past 5 points marks the same pins.
      guard: marksOf([pin('hot', 's', 0.01, false, { mark }), pin('hot', 'a', 0.003, false), pin('hot', 'b', 0.04, true), pin('hot', 'c', 0.04, true)], { shedCap: 1 }),
      // The unpriced s under its mark is leaving, so the guard reads on. Of the target of 9.5, a's 5 go first, b's mark
      // would leave c's 0.5 of the 10, under its tenth, and c's leaves b's 4.5: 2 sessions, none warm, 5.5 points. A
      // walk that kept s as the pin that stays would refuse no mark and take b after a: 2 sessions, none warm, 9.5.
      unpriced: marksOf([pin('hot', 'a', 0.05, false), pin('hot', 'b', 0.045, false), pin('hot', 'c', 0.005, false), pin('hot', 's', null, false, { mark })], { shedCap: 0.95 }),
    },
    { guard: [1, 1, '0.040'], unpriced: [2, 0, '0.055'] },
  );
});

test('the preview reads one member over the ceiling and the round that marks two of its sessions, word for word', () => {
  // The cap bounds the target at half of the 6.5 points an hour the pins burn, which no pin covers alone: cold-a's 3
  // goes first, then warm-b's 2.5, the first of the two warm pins that cover the rest at one cost, reaches it. The 5.5
  // they carry fall under the 9.5 points an hour that a projection of 119 % at the reset two hours out asks for.
  const over = fresh('hot', 1.19, 0.4, 2 * HOUR);
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
  // hot projects 102 % at its 5-hour reset two hours out, past the wall: an excess of one point an hour, which cold-a's
  // cold move covers. Its one sibling's 7-day window reads 97 %, under the weekly ceiling of 100.
  const over = fresh('hot', 1.02, 0.4, 2 * HOUR);
  const sibling = { fableWindow: null, id: 'sibling', kind: 'oauth', label: 'sibling', windows: [{ ...win('5h', 0.3), resetsAt: null }, { ...win('7d', 0.97, 5 * HOUR), resetsAt: WEEKS_RESET }] };
  const preview = d.shedPreview(pool([over, sibling], { fiveHourCeiling: 0.95, weeklyCeiling: 1 }), [over, sibling], three, NOW);
  assert.deepEqual([preview.sessions, preview.quiet], [1, null]);
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
  assert.equal(d.shedPreview(pool([hot, cool], { loadShedding: false }), [hot, cool], [], NOW).quiet, null, 'the switch off walks no member');
  const near = oauth('near', 0.85, 0.3);
  assert.equal(d.shedPreview(pool([near, cool]), [near, cool], [], NOW).quiet, null, 'no member over the ceiling walks none');
  assert.equal(d.shedPreview(pool([hot, cool]), [hot, cool], [], NOW).quiet, 'unpriced', 'a member over the ceiling with no pin to price');
});

/** The round's sentence over `hot` when it would mark no priced pin for `reason`. */
const quiet = (reason) => `1 member projects over the ceiling: hot. A round there would mark no session: ${reason}`;

test('a round that would mark no priced pin because each would leave its member too little burn says so', () => {
  assert.equal(line([hot, cool], [pin('hot', 'only', 0.03, false)]), quiet('each would leave its member too little burn.'));
});

test('a round under never that would mark no priced pin because each is warm says so', () => {
  assert.equal(line([hot, cool], [pin('hot', 'a', 0.03, true), pin('hot', 'b', 0.02, true)], { warmSessions: 'never' }), quiet('each is warm.'));
});

test('a round that would mark no priced pin because none burns, a zero target, says so', () => {
  assert.equal(line([hot, cool], [pin('hot', 'a', 0, false), pin('hot', 'b', 0, false)]), quiet('none burns.'));
});

test('a round whose standing marks cover the target says so', () => {
  // Under never, s's 3 points an hour stand warm under a mark on hot, a move pending, beside c's cold 1. The 3 cover
  // the target of 2, the cap's half of the 4, so the walk stops at c's position and marks nothing. Never also keeps the
  // warm s from the walk, and its mark still names the cause.
  const c = pin('hot', 'c', 0.01, false);
  const s = pin('hot', 's', 0.03, true, { mark: { at: at(NOW - 5 * MINUTE), on: 'hot', window: 'five_hour' } });
  assert.equal(line([hot, cool], [c, s], { warmSessions: 'never' }), quiet('the sessions already marked cover it.'));
});

test('a round whose pins are each too small to move says so, ahead of the guard', () => {
  // heavy's 5 points an hour cover the target of 2.6, the cap's half of the 5.2 the two pins burn, but its mark would
  // leave light's 0.2 on hot, under the guard's tenth of 0.52. light's 0.2 is under a tenth of the rest of 2.6.
  const pins = [pin('hot', 'heavy', 0.05, false), pin('hot', 'light', 0.002, false)];
  // alder's and ivy's one pin of 3 points an hour each covers its target of 1.5, and its mark would leave its member
  // no burn, so both walks read the guard. The preview walks alder, hot and ivy by their labels at one priority, and
  // the line names hot's sliver, the first cause in the router's order, from the middle walk.
  const alder = fresh('alder', 1.09, 0.4, 2 * HOUR);
  const ivy = fresh('ivy', 1.09, 0.4, 2 * HOUR);
  assert.equal(
    line([alder, hot, ivy, cool], [pin('alder', 'only', 0.03, false), ...pins, pin('ivy', 'only', 0.03, false)]),
    '3 members project over the ceiling: alder, hot, ivy. A round there would mark no session: each is too small to move.',
  );
});

test('a round with a pin no sibling has runway for, which burns nothing, reads the cause of the pins it walks', () => {
  // z's Fable model draws the 7-day Fable window, of which cool holds no reading, so no sibling has runway for it, but
  // z burns nothing and leaves the round before its order. a alone covers the target of 1.5, the cap's half of its 3,
  // and its mark would leave hot no burn.
  const pins = [pin('hot', 'a', 0.03, false), pin('hot', 'z', 0, false, { model: 'claude-fable-5-1' })];
  assert.equal(line([hot, cool], pins), quiet('each would leave its member too little burn.'));
});

test('a pin that burns nothing leaves the round before its order', () => {
  // hot's two pins burn nothing, so its target is zero, and under never other's two pins are warm, so its walk stops
  // after its cold pins, of which it holds none. No pin is left that a round could mark.
  const other = fresh('other', 1.09, 0.4, 2 * HOUR);
  const beside = [pin('hot', 'z1', 0, false), pin('hot', 'z2', 0, false), pin('other', 'a', 0.03, true), pin('other', 'b', 0.02, true)];
  // b's 3 points an hour cover the target of 1.5, the cap's half of b's 3, and its mark would leave hot no burn, since
  // z burns nothing and stands in neither the order nor the guard's sum.
  const oneZero = [pin('hot', 'b', 0.03, false), pin('hot', 'z', 0, false)];
  assert.deepEqual(
    { beside: line([hot, other, cool], beside, { warmSessions: 'never' }), oneZero: line([hot, cool], oneZero) },
    {
      beside: '2 members project over the ceiling: hot, other. A round there would mark no session: each is warm.',
      oneZero: quiet('each would leave its member too little burn.'),
    },
  );
});

test('a round whose walks name no cause though a pin burns, as under a shed cap of zero, says none is priced', () => {
  // A cap of zero puts the target at nothing, so the walk reads no pin, though a's 3 and b's 2 points an hour burn and
  // a round could mark either.
  assert.equal(line([hot, cool], [pin('hot', 'a', 0.03, false), pin('hot', 'b', 0.02, false)], { shedCap: 0 }), quiet('none is priced.'));
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
  assert.deepEqual([preview.sessions, preview.covered.toFixed(3), preview.quiet], [1, '0.010', null], "fable-b's point alone, never the Sonnet pin's five");
  assert.equal(d.shedPreviewLine(preview, NOW), '1 member projects over the ceiling: fable-hot. A round there would mark 1 session (0 warm), 1.0 %/h of the excess, to move to a sibling with runway.');
});

test("a pin whose model does not draw the fired window is out of the preview's round", () => {
  // hot crosses on its 7-day Fable window alone. The Sonnet pins draw no 7-day Fable window, so the round walks the
  // Fable pin by itself, and the guard keeps a member's one walked pin, however much the Sonnet pins burn on the
  // shared windows.
  const fableHot = oauth('hot', 0.3, 0.2, 0.97);
  const sibling = oauth('cool', 0.3, 0.2, 0.3);
  const pins = [pin('hot', 'fable-a', 0.01, false, { model: 'claude-fable-5-1' }), pin('hot', 'sonnet-b', 0.05, false), pin('hot', 'sonnet-c', 0.03, false)];
  const preview = d.shedPreview(pool([fableHot, sibling]), [fableHot, sibling], pins, NOW);
  assert.deepEqual([preview.hot.map((h) => h.window), preview.quiet, preview.sessions], [['7d_fable'], 'last_pin', 0]);
  assert.equal(d.shedPreviewLine(preview, NOW), quiet('each would leave its member too little burn.'));
});

test('the preview counts an unpriced pin on the window for the last-pin rule still', () => {
  // On a 5-hour round both models draw the window, so the Sonnet pin served in dollars stays in the order unpriced and
  // the guard allows the Fable pin's mark.
  const sibling = oauth('cool', 0.3, 0.2, 0.3);
  const pins = [pin('hot', 'fable-a', 0.03, false, { model: 'claude-fable-5-1' }), pin('hot', 'sonnet-b', 0.02, false, { burnUnit: 'usd' })];
  const preview = d.shedPreview(pool([hot, sibling]), [hot, sibling], pins, NOW);
  assert.deepEqual([preview.hot.map((h) => h.window), preview.sessions, preview.quiet], [['5h'], 1, null]);
  assert.equal(d.shedPreviewLine(preview, NOW), '1 member projects over the ceiling: hot. A round there would mark 1 session (0 warm), 3.0 %/h of the excess, to move to a sibling with runway.');
});

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
  const passed = fresh('hot', 1.09, 0.4, -5 * MINUTE);
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
  assert.equal(line([approved, cool], weekly, { fiveHourCeiling: 0.95, weeklyCeiling: 0.95 }), '1 member projects over the ceiling: approved. A round there would mark 1 session (0 warm), 0.4 %/h of the excess, to move to a sibling with runway.');
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

test('the round marks cold pins first, then warm ones by burn over the size of their move, all by that under always, and cold ones alone under never', () => {
  // The cap is half of the 4.9 points an hour the three pins burn, which no pin covers alone. By burn over the size the
  // round divides by warm-b's 2 over 100 tokens leads warm-e's 0.9 over 1000 and cold-a's 2 over 5000. Their prompts
  // stand in the other order, so an order by burn over the prompt puts cold-a first under always. Once one pin is
  // marked, each pin left covers the rest of 0.45 points, and the round marks the cheapest move among those it may
  // mark, each move costing its size's tokens at 1e-7 apiece.
  const pins = [
    pin('hot', 'cold-a', 0.02, false, { lastInputTokens: 100, moveCost: 0.0005, moveSize: 5_000 }),
    pin('hot', 'warm-b', 0.02, true, { lastInputTokens: 5_000, moveCost: 0.00001, moveSize: 100 }),
    pin('hot', 'warm-e', 0.009, true, { moveCost: 0.0001, moveSize: 1_000 }),
  ];
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
  // warm-x leads the order by burn over the size of its move, 4 over 1,000 tokens against warm-y's 3 over 800, and
  // warm-y's move, 0.00008 against warm-x's 0.0001, is the cheaper, so the round marks warm-y and removes its 3 points.
  const pins = [pin('hot', 'warm-x', 0.04, true, { moveCost: 0.0001, moveSize: 1_000 }), pin('hot', 'warm-y', 0.03, true, { moveCost: 0.00008, moveSize: 800 })];
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

test("a pin's burn decays to nothing once its instant lies more than thirty burn averages back", () => {
  // Thirty of the pool's averages of 1,800 s are 54,000,000 ms. At exactly that span idle's 0.03 decays by e to the
  // minus 30, to about 2.8e-15, so the sum reads 0.03 and about 2.8e-15, since only a burn of exactly 0 leaves the
  // order, and the target is the cap's half of it, about 0.015, under the excess of 0.045. fresh leads the order and
  // covers the target, but the guard refuses it, since idle's 2.8e-15 left on hot is under a tenth of the sum, 0.003.
  // idle covers nothing and is under a tenth of the rest, 0.0015, so it stays as a sliver, the cause the line names
  // first. One millisecond further back idle reads 0 and leaves before the order, so fresh walks alone on a target of
  // 0.015: fresh, over a tenth of the rest, is no sliver, and its mark would leave hot no burn.
  const read = (idleBack) => line([hot, cool], [pin('hot', 'fresh', 0.03, false), pin('hot', 'idle', 0.03, false, { burnAt: at(NOW - idleBack) })]);
  assert.deepEqual(
    { atCutoff: read(54_000_000), past: read(54_000_001) },
    { atCutoff: quiet('each is too small to move.'), past: quiet('each would leave its member too little burn.') },
  );
});

test("the preview's round leaves out a pin that reads its agent ended, from its order, its sum and its count", () => {
  // With both pins walked the cap's half of their 5 points an hour is 2.5, which a's 3 covers, leaving b's 2 on hot.
  // With b's agent ended the round walks a alone, and its mark would leave none of the burn on hot.
  const b = (ended) => pin('hot', 'b', 0.02, false, { ended });
  const read = (pins) => {
    const preview = d.shedPreview(pool([hot, cool]), [hot, cool], pins, NOW);
    return [preview.sessions, preview.covered, preview.quiet];
  };
  assert.deepEqual(read([pin('hot', 'a', 0.03, false), b(false)]), [1, 0.03, null]);
  assert.deepEqual(read([pin('hot', 'a', 0.03, false), b(true)]), [0, 0, 'last_pin']);
});

/**
 * The round's sentence over `hot` where it marks the one cold pin of 3 points an hour beside t's 2: the cap's half of
 * their 5 is 2.5, which the 3 alone covers, and its mark leaves t's 2 on hot, over the guard's tenth of 0.5.
 */
const markedOne = '1 member projects over the ceiling: hot. A round there would mark 1 session (0 warm), 3.0 %/h of the excess, to move to a sibling with runway.';
/** The second priced pin on hot, whose 2 points an hour left on hot let the guard pass the 3-point pin's mark. */
const second = pin('hot', 't', 0.02, false);

test("the preview walks a subagent's pin on its own member", () => {
  // The session's main pin stands on cool, and its subagent's pin burns 3 points an hour on hot.
  assert.equal(line([hot, cool], [pin('cool', 's', 0.01, false), pin('hot', 's', 0.03, false, { agentId: 'sub' }), second]), markedOne);
});

test('two pins of one session and one model are walked and marked apart, and a pin reads its warmth off its cold instant', () => {
  // r's main pin burns 3 points an hour on hot and its subagent's pin 1 on the same model. The cap's half of their 4
  // is 2, which main's 3 covers alone, and its mark leaves sub's 1 on hot, over the guard's tenth of 0.4.
  assert.equal(line([hot, cool], [pin('hot', 'r', 0.03, false), pin('hot', 'r', 0.01, false, { agentId: 'sub' })]), markedOne);
  // The subagent's pin is served warm and went cold a minute before the dialog's clock, so it walks cold beside t,
  // the sums of case 1: its 3 covers the target of 2.5 and leaves t's 2 on hot.
  const cooled = pin('hot', 's', 0.03, true, { agentId: 'sub', warmUntil: at(NOW - MINUTE) });
  assert.equal(line([hot, cool], [pin('cool', 's', 0.01, false), cooled, second]), markedOne, 'a pin past its cold instant reads cold whatever it was served');
});

test('a fast-lane pin is out of the round', () => {
  const read = (fast) => line([hot, cool], [pin('hot', 'f', 0.03, false, { fast }), second]);
  assert.equal(read(true), quiet('each would leave its member too little burn.'), "on the fast lane f is out, and t's mark would leave hot no burn");
  assert.equal(read(false), markedOne);
});

test('a member whose one Fable pin rides the fast lane still reads its Fable week', () => {
  // fable-hot projects 99 % five hours out on its 7-day Fable window, over the weekly ceiling of 90, and its shared
  // windows sit under it. Its one Fable pin rides the fast lane and leaves the round, but its model's week still counts
  // among the windows the trigger reads, so the week fires.
  const fableHot = oauth('fable-hot', 0.5, 0.4, 0.99);
  const preview = d.shedPreview(pool([fableHot, cool]), [fableHot, cool], [pin('fable-hot', 'f', 0.01, false, { fast: true, model: 'claude-fable-5-1' })], NOW);
  assert.deepEqual(preview.hot.map((h) => [h.label, h.window]), [['fable-hot', '7d_fable']]);
});

test('the pick compares the served cost of a move', () => {
  // Under always both pins rank by burn over the size of their move: cold's 3 points an hour over 1,000 tokens ahead
  // of warm's 3 over 2,000. The cap's half of their 6 is 3, which each covers, and each mark leaves the other's 3 on
  // hot. warm's move costs a third of cold's though its size is twice cold's, so the round marks warm.
  const pins = [pin('hot', 'cold', 0.03, false, { moveCost: 0.0003, moveSize: 1_000 }), pin('hot', 'warm', 0.03, true, { moveCost: 0.0001, moveSize: 2_000 })];
  const preview = d.shedPreview(pool([hot, cool], { warmSessions: 'always' }), [hot, cool], pins, NOW);
  assert.deepEqual([preview.sessions, preview.warm], [1, 1]);
});

test('a cold pin pays its dropped thinking, and nothing where it drops none', () => {
  // The cap's half of the 6 points an hour the two pins burn is 3, which each covers, and each mark leaves the other's
  // 3 on hot. cold walks first. Its move pays the share of the thinking it would drop, 0.0003 against warm's 0.0001, so
  // the round marks warm. With nothing dropped cold's move pays nothing, under warm's and under its own rewrite of
  // 0.0005, so the round marks cold.
  const warm = pin('hot', 'warm', 0.03, true, { moveCost: 0.0001 });
  const read = (drop) => {
    const preview = d.shedPreview(pool([hot, cool]), [hot, cool], [pin('hot', 'cold', 0.03, false, { moveCost: 0.0005, ...drop }), warm], NOW);
    return [preview.sessions, preview.warm];
  };
  assert.deepEqual(read({ moveDropCost: 0.0003, moveDropSize: 2_400 }), [1, 1]);
  assert.deepEqual(read({ moveDropCost: null, moveDropSize: null }), [1, 0]);
});

test('the cold group walks the pins that drop nothing first', () => {
  // At a cap of 1 the target is the two pins' whole 3 points an hour, which neither covers alone, and the guard refuses
  // the second mark, which would leave hot no burn. So the round marks the pin it walks first, and the covered figure
  // reads that pin's burn.
  const read = (pins) => d.shedPreview(pool([hot, cool], { shedCap: 1 }), [hot, cool], pins, NOW).covered;
  // A move that drops nothing walks ahead of one that drops thinking, though the latter burns twice as much.
  assert.equal(read([pin('hot', 'none', 0.01, false), pin('hot', 'drops', 0.02, false, { moveDropCost: 0.0001, moveDropSize: 1_000 })]), 0.01);
  // Of two that drop thinking, light's 1 point over 1,000 weighted tokens walks ahead of heavy's 2 over 10,000, though
  // its burn is the smaller.
  assert.equal(read([pin('hot', 'light', 0.01, false, { moveDropCost: 0.0001, moveDropSize: 1_000 }), pin('hot', 'heavy', 0.02, false, { moveDropCost: 0.001, moveDropSize: 10_000 })]), 0.01);
  // The move that drops nothing still walks first where the other's 2 points over one weighted token top its 1.
  assert.equal(read([pin('hot', 'none', 0.01, false), pin('hot', 'dense', 0.02, false, { moveDropCost: 0.000001, moveDropSize: 1 })]), 0.01);
});

/**
 * `res`, an account whose 7-day window reads `utilization` under a projection of `projection` five hours out, and
 * whose 5-hour window projects `five` at its reset two hours out.
 */
const reserving = (utilization, five = 0.3, projection = 0.85) => {
  const res = fresh('res', five, projection, 2 * HOUR);
  return { ...res, fableWindow: win('7d_fable', 0.3, 5 * HOUR), windows: [res.windows[0], { ...res.windows[1], utilization }] };
};
/** A sibling with runway on every window a pin of `res` draws, its Fable week too. */
const runway = oauth('cool', 0.3, 0.2, 0.1);
/** res's three cold pins: a Fable one, which draws the Fable week, and two Sonnet ones, which do not. */
const reservingPins = [pin('res', 'f', 0.03, false, { model: 'claude-fable-5-1' }), pin('res', 's1', 0.03, false), pin('res', 's2', 0.02, false)];
/** The preview of a Pace pool with the option on, its member res holding `reservation`. */
const reservationPreview = (account, reservation, over = {}) =>
  d.shedPreview(pool([account, runway], { fableReservationShedding: true, members: [{ accountId: 'res', fableReservation: reservation, priority: 0 }, member('cool')], strategy: 'pace', ...over }), [account, runway], reservingPins, NOW);

test('the preview mirrors a Fable reservation round on the non-Fable pins of a member whose 7-day room stands under its reservation', () => {
  // res's 7-day room is 1 less its reading of 0.8, 0.2, under its reservation of 0.25, while no window crosses the
  // ceiling of 0.9, the closest its 7-day projection of 85 %. The round walks s1 and s2 alone, whose 5 points an hour
  // the cap halves to 2.5, which s1's 3 covers, leaving s2's 2 over a tenth of the 5. A walk of f's 3 beside them would
  // halve 8 to 4, which no pin covers, and mark f and then s1.
  const none = { idle: [], labels: [], sessions: 0, warm: 0 };
  // hot's 5-hour round beside res's reservation round, each marking its own pins: two of hot's three, one of them warm,
  // and s1 on res.
  const res = reserving(0.8);
  const beside = d.shedPreview(
    pool([hot, res, runway], { fableReservationShedding: true, members: [member('cool'), member('hot'), { accountId: 'res', fableReservation: 0.25, priority: 0 }], strategy: 'pace' }),
    [hot, res, runway],
    [...three, ...reservingPins],
    NOW,
  );
  assert.deepEqual(
    {
      beside: d.shedPreviewLine(beside, NOW),
      equalRoom: reservationPreview(reserving(0.75), 0.25).reservation,
      floor: reservationPreview(reserving(1.05), 0).reservation,
      noProjection: reservationPreview(reserving(0.8, 0.3, null), 0.25).reservation,
      on: d.shedPreviewLine(reservationPreview(reserving(0.8), 0.25), NOW),
      optionOff: reservationPreview(reserving(0.8), 0.25, { fableReservationShedding: false }).reservation,
      priority: reservationPreview(reserving(0.8), 0.25, { strategy: 'priority' }).reservation,
      sheddingOff: reservationPreview(reserving(0.8), 0.25, { loadShedding: false }).reservation,
      windowFires: reservationPreview(reserving(0.8, 1.05), 0.25).reservation,
    },
    {
      beside:
        '1 member projects over the ceiling: hot. A round there would mark 2 sessions (1 warm), 4.5 %/h of the excess, to move to a sibling with runway. 1 member holds less 7-day room than its Fable reservation: res. A round there would mark 1 non-Fable session (0 warm), to move to a sibling with runway.',
      // 0.75 used leaves a room of 0.25, the whole reservation.
      equalRoom: none,
      // 1.05 used leaves no room, which is no room under a reservation of 0.
      floor: none,
      // A 7-day window with no projection fires no reservation round, whatever its room.
      noProjection: none,
      on: 'No member projects over the ceiling on any window: the closest is res, 7d 85 % in 5h. 1 member holds less 7-day room than its Fable reservation: res. A round there would mark 1 non-Fable session (0 warm), to move to a sibling with runway.',
      optionOff: none,
      priority: none,
      sheddingOff: none,
      // res's 5-hour projection of 105 % at its reset fires its own round, which the reservation's test never follows.
      windowFires: none,
    },
  );
});

test('a heavy pin with no runway beside slivers answers no_runway', () => {
  // heavy's Fable model draws the 7-day Fable window, of which cool holds no reading, so no sibling has runway for it.
  // The target is the cap's half of the 5.6 points an hour the pins burn, 2.8, which heavy covers, and its mark would
  // leave the slivers' 0.6 on hot, over the guard's tenth of 0.56, so the gate reads it. Each sliver is under a tenth of
  // the rest.
  const pins = [pin('hot', 'heavy', 0.05, false, { model: 'claude-fable-5-1' }), pin('hot', 's1', 0.002, false), pin('hot', 's2', 0.002, false), pin('hot', 's3', 0.002, false)];
  assert.equal(d.shedPreview(pool([hot, cool]), [hot, cool], pins, NOW).quiet, 'no_runway');
});

test('slivers with no runway answer sliver', () => {
  // hot projects 120 % at its reset an hour out, so the excess of 20 points an hour exceeds the 11 points the pins burn,
  // which a cap of 1 sets as the target. Each pin's 1 point is under a tenth of it, and hot has no sibling.
  const soon = fresh('hot', 1.2, 0.4, HOUR);
  const slivers = Array.from({ length: 11 }, (_, i) => pin('hot', `s${i}`, 0.01, false));
  assert.equal(d.shedPreview(pool([soon], { shedCap: 1 }), [soon], slivers, NOW).quiet, 'sliver');
});

test('a lone pin with no runway answers last_pin', () => {
  // a covers the target of 1.5, the cap's half of its 3, and its mark would leave hot no burn, so the guard keeps it
  // before the gate reads that hot has no sibling.
  assert.equal(d.shedPreview(pool([hot]), [hot], [pin('hot', 'a', 0.03, false)], NOW).quiet, 'last_pin');
});

test('a warm pin under a standing mark with never answers standing', () => {
  const s = pin('hot', 's', 0.03, true, { mark: { at: at(NOW - 5 * MINUTE), on: 'hot', window: 'five_hour' } });
  assert.equal(d.shedPreview(pool([hot, cool], { warmSessions: 'never' }), [hot, cool], [s], NOW).quiet, 'standing');
});

/**
 * An account whose 5-hour window resets `resetIn` from now, projecting `five` there, with the slow-down entry's stretch
 * figure `stretch` against a threshold of 30 minutes.
 */
const stretched = (id, five, stretch, resetIn = 3 * HOUR) => ({ ...fresh(id, five, 0.4, resetIn), slowDown: { stretchProjection: stretch, thresholdMs: 30 * MINUTE } });

test("a slowed pool's 5-hour window reads the stretch figure against the 5-hour ceiling, under the hold too", () => {
  // stretch's figure of 93 % is over the 5-hour ceiling of 90, though its projection of 97 % at its reset is under the
  // wall. unread carries no figure, so its projection of 102 % fires nothing. young and young-unread hold the same
  // figures on a window that opened ten minutes ago, under the hold of an hour.
  const accounts = [stretched('stretch', 0.97, 0.93), stretched('unread', 1.02, null), stretched('young', 0.97, 0.93, 4 * HOUR + 50 * MINUTE), stretched('young-unread', 1.02, null, 4 * HOUR + 50 * MINUTE)];
  const preview = d.shedPreview(pool(accounts, { slowDown: true }), accounts, [], NOW);
  assert.deepEqual({ held: preview.held.map((h) => h.label), hot: preview.hot.map((h) => h.label) }, { held: ['young'], hot: ['stretch'] });
});

test("a slowed member's stretch round reads its horizon off the served horizon less the threshold", () => {
  // The account ends two hours out, ahead of its 5-hour reset three hours out, so the route serves the end as the
  // window's horizon. The round's horizon is 30 minutes before it, an hour and a half out, and the figure of 96 %, six
  // points over the 5-hour ceiling of 90, asks 4 points an hour.
  const ends = at(NOW + 2 * HOUR);
  const base = stretched('ending', 0.97, 0.96);
  const ending = { ...base, canceled: true, subscription: { endsAt: ends, trialEndsAt: null }, windows: [{ ...base.windows[0], horizon: ends }, base.windows[1]] };
  const preview = d.shedPreview(pool([ending], { slowDown: true }), [ending], [], NOW);
  assert.deepEqual(preview.hot.map((h) => [h.label, h.horizon, h.excess.toFixed(4)]), [['ending', NOW + 90 * MINUTE, '0.0400']]);
});

test('a walk whose pins each hold state on their account marks none and answers account_bound', () => {
  // Without the flag, a's 3 points an hour would cover the target of 2.5, the cap's half of the 5, and leave b's 2.
  const pins = [pin('hot', 'a', 0.03, false, { accountBound: true }), pin('hot', 'b', 0.02, false, { accountBound: true })];
  const preview = d.shedPreview(pool([hot, cool]), [hot, cool], pins, NOW);
  assert.deepEqual([preview.sessions, preview.quiet], [0, 'account_bound']);
});

/**
 * res's reservation round over `pins`, res's 7-day room of 0.2 under its reservation of 0.25, beside `sibling` holding
 * `siblingReservation`.
 */
const reservationRound = (pins, sibling = runway, siblingReservation = null) => {
  const res = reserving(0.8);
  const members = [
    { accountId: 'res', fableReservation: 0.25, priority: 0 },
    { accountId: sibling.id, eligible: 'eligible', fableReservation: siblingReservation, priority: 0 },
  ];
  return d.shedPreview(pool([res, sibling], { fableReservationShedding: true, members, strategy: 'pace' }), [res, sibling], pins, NOW).reservation;
};
const fable = (sessionId, burn, over = {}) => pin('res', sessionId, burn, false, { model: 'claude-fable-5-1', ...over });

test("a reservation round's guard counts the Fable pins that stay on the account", () => {
  // s alone covers the target of 1.5, the cap's half of its 3. Its mark leaves the Fable pins' 4 points an hour on
  // res, over the guard's tenth of 0.3.
  assert.equal(reservationRound([pin('res', 's', 0.03, false), fable('f1', 0.02), fable('f2', 0.02)]).sessions, 1);
});

test("a reservation round's guard reads a Fable pin with no priced burn as a burn it cannot weigh", () => {
  assert.equal(reservationRound([pin('res', 's', 0.03, false), fable('f', null)]).sessions, 1);
});

test("a reservation round's guard counts nothing for a Fable pin whose agent ended", () => {
  // With f's agent ended nothing stays beside s, so the guard keeps s, the round's last_pin.
  assert.equal(reservationRound([pin('res', 's', 0.03, false), fable('f', null, { ended: true })]).sessions, 0);
});

test('a reservation round moves no pin onto a sibling under its own Fable reservation', () => {
  // Each pin's 3 points an hour cover the target of 3, the cap's half of their 6, and each mark leaves the other's 3.
  // The sibling's 7-day room of 0.2 is under its reservation of 0.25, so no pin has a destination, the round's
  // no_runway, and with no reservation the sibling takes one.
  const sibling = { ...oauth('cool', 0.3, 0.85, 0.1), windows: [win('5h', 0.3), { ...win('7d', 0.85, 5 * HOUR), utilization: 0.8 }] };
  const pins = [pin('res', 's1', 0.03, false), pin('res', 's2', 0.03, false)];
  assert.deepEqual([reservationRound(pins, sibling, 0.25).sessions, reservationRound(pins, sibling, null).sessions], [0, 1]);
});

test('a member under its reservation with no pin of another model to move runs no round', () => {
  const fablePins = [fable('f1', 0.03), fable('f2', 0.02)];
  assert.deepEqual(reservationRound(fablePins), { idle: ['res'], labels: [], sessions: 0, warm: 0 });
  // A pin of another model that burns nothing is a pin the round's test can move, so the round runs and marks none.
  assert.deepEqual(reservationRound([...fablePins, pin('res', 'z', 0, false)]), { idle: [], labels: ['res'], sessions: 0, warm: 0 }, 'a pin of another model that burns nothing');
});
