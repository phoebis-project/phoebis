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

test('a sibling whose plan ends inside the next five hours has no runway, whatever its projection', () => {
  const ending = (hours) => ({ ...cool, cancelled: true, subscription: { endsAt: at(NOW + hours * HOUR), trialEndsAt: null } });
  assert.equal(line([hot, ending(4)], three), '1 member projects over the ceiling: hot. No sibling has runway for them, so nothing would be marked.', 'an end four hours out falls inside every horizon');
  assert.equal(line([hot, ending(6)], three), '1 member projects over the ceiling: hot. A round there would mark 2 sessions (1 warm), 4.5 %/h of the excess, to move to a sibling with runway.', 'an end six hours out does not');
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
  // hour, while its shared windows sit under it. The Sonnet pin burns five points an hour and the Fable pin one, but
  // only the Fable pin draws the week that fired, so the round marks it and the Sonnet pin reads unpriced there.
  const fableHot = oauth('fable-hot', 0.5, 0.4, 0.99);
  const sibling = oauth('cool', 0.3, 0.2, 0.3);
  const pins = [pin('fable-hot', 'sonnet-a', 0.05, false), pin('fable-hot', 'fable-b', 0.01, false, { model: 'claude-fable-5-1' })];
  const preview = d.shedPreview(pool([fableHot, sibling]), [fableHot, sibling], pins, NOW);
  assert.deepEqual(preview.hot.map((h) => [h.label, h.window, h.excess.toFixed(3)]), [['fable-hot', '7d_fable', '0.018']]);
  assert.deepEqual([preview.sessions, preview.covered.toFixed(3), preview.unpriced], [1, '0.010', false], "the Fable pin's point alone, never the Sonnet pin's five");
  assert.equal(d.shedPreviewLine(preview, NOW), '1 member projects over the ceiling: fable-hot. A round there would mark 1 session (0 warm), 1.0 %/h of the excess, to move to a sibling with runway.');
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
  // Served half an hour, one burn average, before the preview's instant, cold-a's 3 points an hour read 3/e there, under
  // cold-b's 1.2, so cold-b leads the order and alone reaches the cap's half of their 2.3. The unpriced pin stays.
  const aged = [pin('hot', 'cold-a', 0.03, false, { burnAt: at(NOW - 30 * MINUTE) }), pin('hot', 'cold-b', 0.012, false), pin('hot', 'cold-c', null, false)];
  const preview = d.shedPreview(pool([hot, cool]), [hot, cool], aged, NOW);
  assert.deepEqual([preview.sessions, preview.covered.toFixed(4)], [1, '0.0120']);
  const unpriced = d.shedPreview(pool([hot, cool], { shedCap: 1 }), [hot, cool], [pin('hot', 'priced', 0.03, false), pin('hot', 'unpriced', null, false)], NOW);
  assert.deepEqual([unpriced.sessions, unpriced.covered], [1, 0.03], 'the unpriced pin stays unmarked and counts among the pins, so the priced one is not the last');
});
