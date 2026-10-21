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
      const applied = { ...{ ...d.SHEDDING_PRESETS.fast, loadShedding: on, windowCeiling: 0.9 }, ...d.SHEDDING_PRESETS[name] };
      assert.deepEqual([applied.loadShedding, applied.windowCeiling, d.sheddingPresetOf(applied)], [on, 0.9, name], `${name} with the switch ${on ? 'on' : 'off'}`);
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

/**
 * A window as the account routes serve it: its projection runs to the horizon `horizonIn` from now, and `burn` is the
 * share of it the account's sessions burn an hour. A null projection is a window the router holds no reading of.
 */
const win = (id, projection, horizonIn = 2 * HOUR, burn = 0.01) => ({
  burn: projection === null ? null : burn,
  horizon: projection === null ? null : at(NOW + horizonIn),
  id,
  label: id,
  observedAt: at(NOW),
  periodHours: id === '5h' ? 5 : 168,
  projection,
  resetsAt: at(NOW + horizonIn),
  utilization: projection === null ? 0.5 : Math.max(0, projection - 0.1),
});
/** A subscription account with its 5-hour and 7-day projections, and its Fable week's where one is named. */
const oauth = (id, five, seven, fable = null) => ({ fableWindow: fable === null ? null : win('7d_fable', fable, 5 * HOUR), id, kind: 'oauth', label: id, windows: [win('5h', five), win('7d', seven, 5 * HOUR)] });
/** A pin as the Sessions route serves it: its 5-hour burn decayed to `burnAt`, the instant of the read. */
const pin = (accountId, sessionId, burn, warm, over = {}) => ({
  accountId,
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
const member = (accountId, eligible = 'eligible', priority = 0) => ({ accountId, eligible, priority });
const pool = (accounts, over = {}) => ({
  burnAverageSeconds: 1_800,
  loadShedding: true,
  members: accounts.map((a) => member(a.id)),
  shedCap: 0.5,
  warmSessions: 'when_needed',
  windDownMinutes: 30,
  windowCeiling: 0.9,
  ...over,
});
const line = (accounts, sessions, over = {}) => d.shedPreviewLine(d.shedPreview(pool(accounts, over), accounts, sessions, NOW), NOW);
// hot projects 99 % at its 5-hour reset two hours out, nine points past the ceiling: an excess of 4.5 points an hour.
const hot = oauth('hot', 0.99, 0.4);
const cool = oauth('cool', 0.3, 0.2);
const three = [pin('hot', 'cold-a', 0.03, false), pin('hot', 'warm-b', 0.04, true), pin('hot', 'warm-c', 0.01, true)];

test('the round marks pins in order until the target, never the last unmarked pin, and passes over a pin it may not mark', () => {
  const ordered = [
    { burn: 3, id: 'cold-3' },
    { burn: 4, id: 'warm-4' },
    { burn: 2, id: 'warm-2' },
    { burn: 1, id: 'warm-1' },
  ];
  assert.deepEqual(d.shedMarks(ordered, 5), ['cold-3', 'warm-4'], 'marked in order until the target is reached');
  assert.deepEqual(d.shedMarks([{ burn: 9, id: 'only' }], 9), [], 'never the last unmarked pin');
  assert.deepEqual(d.shedMarks(ordered, 0), [], 'nothing for a zero target');
  assert.deepEqual(d.shedMarks([{ burn: 1, id: 'priced' }, { burn: null, id: 'unpriced' }, { burn: 2, id: 'last' }], 3), ['priced', 'last'], 'an unpriced pin is passed over and stays the one unmarked pin');
  assert.deepEqual(d.shedMarks([{ burn: 3, id: 'stuck', skip: true }, { burn: 2, id: 'free' }, { burn: 1, id: 'last' }], 2), ['free'], 'a pin with no sibling to move to is passed over and counts nothing toward the target');
});

test('the preview reads one member over the ceiling and the round that marks two of its sessions, word for word', () => {
  // The cap bounds the target at half of the 8 points an hour the pins burn: cold-a's 3 then warm-b's 4 reach it, and
  // the 7 they carry fall under the 9.5 points an hour that a projection of 109 % two hours out asks for.
  const over = oauth('hot', 1.09, 0.4);
  assert.equal(line([over, cool], three), '1 member projects over the ceiling: hot. A round there would mark 2 sessions (1 warm), 7.0 %/h of the excess, to move to a sibling with runway.');
  const preview = d.shedPreview(pool([over, cool]), [over, cool], three, NOW);
  assert.deepEqual([preview.hot.map((h) => [h.label, h.window]), preview.sessions, preview.warm, preview.covered.toFixed(3)], [[['hot', '5h']], 2, 1, '0.070']);
  // The marks overshoot an excess of 4.5 points an hour, so the round covers the excess whole.
  assert.equal(line([hot, cool], three), '1 member projects over the ceiling: hot. A round there would mark 2 sessions (1 warm), 4.5 %/h of the excess, to move to a sibling with runway.');
  assert.equal(line([hot, cool], three, { shedCap: 0.25 }), '1 member projects over the ceiling: hot. A round there would mark 1 session (0 warm), 3.0 %/h of the excess, to move to a sibling with runway.', 'a quarter of the burn is 2 points an hour, which cold-a reaches alone');
});

/**
 * An account as the routes serve it with its slow-down read at `factor` and a stretch of `stretchMinutes` against 30,
 * the holds having realized `speed`.
 */
const slowedAccount = (id, factor, stretchMinutes, speed = factor) => ({
  ...oauth(id, 0.5, 0.5),
  slowDown: { factor, holdMs: 120_000, since: factor < 1 ? at(NOW - 10 * MINUTE) : null, speed, stretchMs: stretchMinutes * MINUTE, thresholdMs: 30 * MINUTE },
});
/** A pool with the slow-down on, its members in the given order, each carrying the factor its account serves. */
const slowPool = (accounts, over = {}) => ({
  members: accounts.map((a, i) => ({ accountId: a.id, priority: i, speed: a.slowDown?.factor ?? null })),
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
  const ghost = { ...slowPool([alder]), members: [{ accountId: 'alder', priority: 0, speed: 0.5 }, { accountId: 'ghost', priority: 1, speed: 0.4 }] };
  assert.equal(d.slowDownPreviewLine(d.slowDownPreview(ghost, [alder])), '2 members are slowed: alder at 0.5x (42 min of 30 min), ghost at 0.4x. A session on ghost waits its cycle times 1.5, up to 120 s.', 'a member whose account did not load counts by its own factor, its stretch unknown');
});

test('the preview names the member closest to the ceiling when none projects over it on any window', () => {
  const near = oauth('near', 0.85, 0.3);
  assert.equal(line([near, cool], []), `No member projects over the ceiling on any window: the closest is near, 5h 85 % at ${d.instant(NOW + 2 * HOUR, NOW)}.`, 'a 5-hour horizon reads its clock time');
  const weekly = oauth('weekly', 0.4, 0.884);
  assert.equal(line([cool, weekly], []), 'No member projects over the ceiling on any window: the closest is weekly, 7d 88 % in 5h.', 'a weekly horizon reads the span to it');
  const fable = oauth('fable', 0.4, 0.5, 0.87);
  assert.equal(line([fable], [pin('fable', 'f', 0.01, false, { model: 'claude-fable-5-1' })]), 'No member projects over the ceiling on any window: the closest is fable, Fable week 87 % in 5h.', "the Fable week reads where a pin's model draws it");
  assert.equal(line([fable], [pin('fable', 's', 0.01, false)]), 'No member projects over the ceiling on any window: the closest is fable, 7d 50 % in 5h.', 'and only there');
  assert.equal(line([{ id: 'key', kind: 'api_key', label: 'key', windows: [], fableWindow: null }], []), 'No member projects over the ceiling on any window.', 'a pool of API-key members alone has no window to read');
});

test('the preview names every member over the ceiling, and says no sibling has runway when none can take their sessions', () => {
  const second = oauth('second', 0.4, 0.95);
  const pins = [...three, pin('second', 'a', 0.05, false), pin('second', 'b', 0.01, false)];
  assert.equal(line([hot, second], pins), '2 members project over the ceiling: hot, second. No sibling has runway for them, so nothing would be marked.');
  const cooled = { ...pool([hot, cool]), members: [member('hot'), member('cool', 'cooldown')] };
  assert.equal(d.shedPreviewLine(d.shedPreview(cooled, [hot, cool], three, NOW), NOW), '1 member projects over the ceiling: hot. No sibling has runway for them, so nothing would be marked.', 'a member the router skips is no destination');
  const unread = oauth('unread', null, 0.2);
  assert.equal(line([hot, unread], three), '1 member projects over the ceiling: hot. No sibling has runway for them, so nothing would be marked.', 'a sibling with no reading on a window the pin draws has no runway');
  const room = { fableWindow: null, id: 'key', kind: 'api_key', label: 'key', windows: [{ id: 'itpm:claude-sonnet-5', label: 'itpm', observedAt: at(NOW), periodHours: 1 / 60, resetsAt: at(NOW + MINUTE), utilization: 0.4 }] };
  assert.equal(line([hot, room], three), '1 member projects over the ceiling: hot. A round there would mark 2 sessions (1 warm), 4.5 %/h of the excess, to move to a sibling with runway.', 'an API-key sibling with room in its bucket takes them');
  const ordered = d.shedPreview({ ...pool([second, hot, cool]), members: [member('second', 'eligible', 1), member('hot'), member('cool', 'eligible', 1)] }, [second, hot, cool], pins, NOW);
  assert.deepEqual(ordered.hot.map((h) => h.label), ['hot', 'second'], 'the members in the order the Members group lists them, priority first');
});

test("a sibling whose plan ends inside the pool's wind-down has no runway, whatever its projection", () => {
  const ending = (hours) => ({ ...cool, cancelled: true, subscription: { endsAt: at(NOW + hours * HOUR), trialEndsAt: null } });
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
  // The Fable pin draws the Fable week, where busy sits over the ceiling, so the round passes over it and marks the two
  // Sonnet pins in its place, 3 then 1 points an hour, which reach the cap's 4. The Fable pin stays the last unmarked.
  const preview = d.shedPreview(pool([hot, fableFull]), [hot, fableFull], pins, NOW);
  assert.deepEqual([preview.hot.map((h) => h.label), preview.sessions, preview.covered.toFixed(3)], [['hot'], 2, '0.040'], 'busy draws no Fable pin of its own, so its Fable week fires no round there');
  assert.equal(d.shedPreviewLine(preview, NOW), '1 member projects over the ceiling: hot. A round there would mark 2 sessions (0 warm), 4.0 %/h of the excess, to move to a sibling with runway.');
});

test('a round fired by the Fable week counts the pins whose model draws it alone, so it plans no Sonnet move', () => {
  // fable-hot crosses on its Fable week alone: 99 % five hours out under the ceiling of 90, an excess of 1.8 points an
  // hour, while its shared windows sit under it. The Sonnet pin burns five points an hour and the two Fable pins one
  // and a half between them, but only the Fable pins draw the week that fired, so the Sonnet pin sits the round out and
  // the cap's 0.75 points an hour are reached by the heavier Fable pin alone.
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
  // hot crosses on its Fable week alone. The Sonnet pins draw no Fable week, so the round walks the Fable pin by
  // itself, and a member's last pin is never marked, however much the Sonnet pins burn on the shared windows.
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
 * `oauth` with its 5-hour window resetting `resetIn` from now. The window starts five hours before the reset, or at the
 * reset once it has passed.
 */
const fresh = (id, five, seven, resetIn) => ({ ...oauth(id, five, seven), windows: [win('5h', five, resetIn), win('7d', seven, 5 * HOUR)] });
/** The sentence that names a member whose 5-hour round the hold, twice the test pool's 30-minute average, took away. */
const heldSentence = (label, age) => `${label}'s 5-hour window is ${age} old, under the hold of 1 h, so its round reads the next window.`;

test('a member whose 5-hour window is under the hold is read on its next window and named', () => {
  // hot's 5-hour window reset ten minutes ago, so its round reads the 7-day window, over the ceiling too: 97 % five
  // hours out, an excess of 1.4 points an hour, which cold-a's 3 covers alone.
  const young = fresh('hot', 0.99, 0.97, 4 * HOUR + 50 * MINUTE);
  const preview = d.shedPreview(pool([young, cool]), [young, cool], three, NOW);
  assert.deepEqual(preview.hot.map((h) => h.window), ['7d']);
  assert.deepEqual(preview.held, [{ ageMs: 600_000, holdMs: 3_600_000, id: 'hot', label: 'hot' }]);
  assert.equal(d.shedPreviewLine(preview, NOW), `1 member projects over the ceiling: hot. A round there would mark 1 session (0 warm), 1.4 %/h of the excess, to move to a sibling with runway. ${heldSentence('hot', '10 min')}`);
  assert.equal(line([young, cool], three, { loadShedding: false }), `Load shedding is off: no round runs. 1 member projects over the ceiling: hot. ${heldSentence('hot', '10 min')}`, 'the switch off names the held member too');
});

test('a held member with no other window over the ceiling is named beside the closest', () => {
  const young = fresh('hot', 0.99, 0.4, 4 * HOUR + 50 * MINUTE);
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
    const member = fresh('hot', 0.99, 0.4, resetIn);
    const preview = d.shedPreview(pool([member, cool]), [member, cool], three, NOW);
    return [preview.hot.map((h) => h.window), preview.held];
  };
  assert.deepEqual(read(3 * HOUR + 59 * MINUTE), [['5h'], []], '61 minutes old');
  assert.deepEqual(read(4 * HOUR), [['5h'], []], 'a window exactly as old as the hold fires');
});

test('a 5-hour reset more than five hours ahead starts no fresh window, so nothing is held', () => {
  const ahead = fresh('hot', 0.99, 0.4, 6 * HOUR);
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

test("a round on a weekly window scales the pins' 5-hour burn by the account's burn on that window", () => {
  // weekly crosses on its 7-day window alone: 97 % at a horizon five hours out under a ceiling of 95, an excess of 0.4
  // points an hour. Its sessions burn the 7-day window a quarter as fast as the 5-hour one, so the pins' 5 and 3 points
  // read 1.25 and 0.75 there. The excess is under the cap's half of their 2, and a's 1.25 covers it alone.
  const weekly = { fableWindow: null, id: 'weekly', kind: 'oauth', label: 'weekly', windows: [win('5h', 0.6, 2 * HOUR, 0.08), win('7d', 0.97, 5 * HOUR, 0.02)] };
  const pins = [pin('weekly', 'a', 0.05, false), pin('weekly', 'b', 0.03, false)];
  const preview = d.shedPreview(pool([weekly, cool], { windowCeiling: 0.95 }), [weekly, cool], pins, NOW);
  assert.deepEqual(preview.hot.map((h) => [h.label, h.window, h.excess.toFixed(4)]), [['weekly', '7d', '0.0040']]);
  assert.equal(d.shedPreviewLine(preview, NOW), '1 member projects over the ceiling: weekly. A round there would mark 1 session (0 warm), 0.4 %/h of the excess, to move to a sibling with runway.');
});

test('with load shedding off the preview opens by saying no round runs, then names the projection alone', () => {
  assert.equal(line([hot, cool], three, { loadShedding: false }), 'Load shedding is off: no round runs. 1 member projects over the ceiling: hot.');
  const near = oauth('near', 0.85, 0.3);
  assert.equal(line([near], [], { loadShedding: false }), `Load shedding is off: no round runs. No member projects over the ceiling on any window: the closest is near, 5h 85 % at ${d.instant(NOW + 2 * HOUR, NOW)}.`);
});

test('the round marks cold pins first, then warm ones by burn over the prompt they would rewrite, all by that under always, and cold ones alone under never', () => {
  // The cap is half of the 7.7 points an hour the four pins burn. By burn over rewrite warm-b's 4 over 100 tokens
  // leads cold-a's 3 over 1000, cold-c's 0.5 over 1000 and warm-e's 0.2 over 1000.
  const pins = [pin('hot', 'cold-a', 0.03, false), pin('hot', 'warm-b', 0.04, true, { lastInputTokens: 100 }), pin('hot', 'cold-c', 0.005, false), pin('hot', 'warm-e', 0.002, true)];
  const read = (warmSessions) => {
    const p = d.shedPreview(pool([hot, cool], { warmSessions }), [hot, cool], pins, NOW);
    return [p.sessions, p.warm];
  };
  assert.deepEqual(read('when_needed'), [3, 1], 'cold-a and cold-c fall short of the cap, so warm-b follows');
  assert.deepEqual(read('always'), [1, 1], 'warm-b alone reaches the cap');
  assert.deepEqual(read('never'), [2, 0], 'the two cold pins, short of the cap, and no warm one');
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
