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
      d.failoverPresetOf({ inFlightCap: 16, paceFloor: 0.05, retryInPlaceSeconds: 60 }),
      d.failoverPresetOf({ inFlightCap: 16, paceFloor: 0.05, retryInPlaceSeconds: 120 }),
      d.failoverPresetOf({ inFlightCap: 16, paceFloor: 0.05, retryInPlaceSeconds: 20 }),
    ],
    ['balanced', 'conservative', 'custom'],
    'balanced and conservative share the cap and the floor, so the retry-in-place bound alone tells them apart',
  );
  for (const off of [{ inFlightCap: 9 }, { paceFloor: 0.05 }, { retryInPlaceSeconds: 21 }]) assert.equal(d.failoverPresetOf({ ...d.FAILOVER_PRESETS.fast, ...off }), 'custom', JSON.stringify(off));
});

test('the proposed lead over pace is a lead the router can shed on, above the cool band', () => {
  assert.ok(d.SHED_AHEAD_OF_PACE_DEFAULT > 0 && d.SHED_AHEAD_OF_PACE_DEFAULT < 1);
  assert.ok(d.SHED_AHEAD_OF_PACE_DEFAULT - d.MIGRATE_BAND > 0, 'the cool line under the proposed lead stays above zero');
});

const NOW = Date.parse('2026-09-16T14:30:00Z');
const MINUTE = 60_000;
const at = (ms) => new Date(ms).toISOString();
// Each reset a whole period out, so nothing of the window has elapsed and a reading leads its pace by its utilization.
const reading = (id, utilization) => {
  const periodHours = id === '5h' ? 5 : 168;
  return { id, label: id, observedAt: at(NOW), periodHours, resetsAt: at(NOW + periodHours * 60 * MINUTE), utilization };
};
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
  shedAheadOfPace: 0.8,
  shedShare: 0.5,
  warmSessions: 'when_needed',
  ...over,
});
/** Every account the cost model sizes, so each subscription member's pins enter the round. */
const SIZED = new Set(['cool', 'hot', 'near', 'second']);
const preview = (sessions, over) => d.shedPreview(pool(over), [hot, cool], sessions, NOW, SIZED);
const four = [pin('warm-4', 4, true), pin('cold-3', 3, false), pin('warm-2', 2, true), pin('warm-1', 1, true)];
const member = (accountId, eligible = 'eligible') => ({ accountId, eligible, priority: 0 });
/** A round over `accounts`, every one of them a member, with the pool's balanced shedding settings. */
const replay = (accounts, sessions, over = {}) => d.shedPreview(pool({ members: accounts.map((a) => member(a.id)), ...over }), accounts, sessions, NOW, SIZED);

const WEEK = 168 * 60 * MINUTE;

test('a window leads its pace by its utilization less the share of it that has elapsed', () => {
  const lead = (resetsAt) => d.aheadOf({ resetsAt, utilization: 0.5 }, WEEK, NOW);
  assert.equal(lead(at(NOW + WEEK * 0.9)).toFixed(2), '0.40', 'a tenth of the week gone');
  assert.equal(lead(at(NOW + WEEK * 0.4)).toFixed(2), '-0.10', 'six tenths of the week gone');
  assert.equal(lead(null), 0.5, 'a window with no reset reads nothing elapsed');
  assert.equal(lead(at(NOW + WEEK * 2)), 0.5, 'a reset more than a window out reads nothing elapsed');
  assert.equal(lead(at(NOW - MINUTE)), -0.5, 'a window past its reset reads fully elapsed');
});

test('a member at half its week counts as hot at a margin of a fifth ten percent in and not sixty percent in', () => {
  const halfway = (elapsed) => ({ ...account('halfway', 0.1), windows: [reading('5h', 0.1), { ...reading('7d', 0.5), resetsAt: at(NOW + WEEK * (1 - elapsed)) }] });
  const members = (elapsed) => replay([halfway(elapsed)], [], { shedAheadOfPace: 0.2 }).members;
  assert.deepEqual([members(0.1), members(0.6)], [1, 0], '0.4 ahead of pace runs a round, 0.1 behind it does not');
});

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
  assert.deepEqual([preview(four, { shedAheadOfPace: 0.95 }).members, preview(four, { shedAheadOfPace: null }).members], [0, 0], 'no member at the threshold, or no threshold, replays no round');
  // Folded one burn window ago, 10 has decayed to 10/e, under the 5 folded now; a burn never folded reads as stored.
  const decayed = preview([pin('old', 10, false, { burnAt: at(NOW - 600_000) }), pin('new', 5, false), pin('unfolded', 1, false, { burnAt: null })]);
  assert.deepEqual([decayed.sessions, decayed.burnShare.toFixed(3)], [1, (5 / (10 / Math.E + 5 + 1)).toFixed(3)], 'the order and the share read each burn decayed to now');
});

test('a pin the cost model cannot price stays in the order unmarked and counts for the last-pin rule', () => {
  const unlisted = four.map((p) => (p.sessionId === 'warm-1' ? { ...p, burn: null } : p));
  const round = replay([hot, cool], unlisted);
  assert.deepEqual([round.sessions, round.warm, round.unpriced], [2, 1, 1], 'cold 3 then warm 4, as with every pin priced');
  assert.ok(d.shedPreviewLine(round, true).endsWith(' 1 pin the cost model cannot price stays.'), d.shedPreviewLine(round, true));
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

test("the pins of an account whose tier the cost model does not size stay out of the preview, unlike a pin it cannot price", () => {
  // The route reads no burn for any pin of such an account, while the round prices them on the anchor tier's rates
  // and sheds them; the preview cannot replay that order, so it leaves them out rather than call them unpriced.
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
  assert.equal(d.shedPreviewLine(same, true), `2 members run ahead of pace by the margin: hot (5-hour, 90 points ahead) and second (5-hour, 85 points ahead). A round there would move 3 sessions (1 warm), 83% of the hot members' recent burn together, in window share, to a cool sibling.`);
  const mixed = replay([hot, second, cool], [...four, ...heavy('usd')]);
  assert.deepEqual([mixed.unit, mixed.averaged, mixed.burnShare.toFixed(3)], [null, true, ((0.7 + 0.9) / 2).toFixed(3)], 'a window share and dollars cannot be summed, so each member counts once');
  assert.equal(same.averaged, false);
  assert.equal(d.shedPreviewLine(mixed, true), `2 members run ahead of pace by the margin: hot (5-hour, 90 points ahead) and second (5-hour, 85 points ahead). A round there would move 3 sessions (1 warm), 80% of each member's recent burn on average, to a cool sibling.`);
});

test('a pool previews its own pins on a shared account, so one pin per pool sheds nothing', () => {
  // The caller hands in the pool's slice, the sessions under its keys: `theirs` sits under another pool's key.
  const mine = pin('mine', 4, false);
  const theirs = pin('theirs', 3, false, { keyId: 'notebook' });
  assert.equal(preview([mine, theirs]).sessions, 1, 'over every pin on the account the heavier one is marked');
  const own = preview([mine]);
  assert.deepEqual([own.sessions, own.cool], [0, true], "the pool's one pin is its slice's last unmarked pin");
  assert.equal(d.shedPreviewLine(own, true), `1 member runs ahead of pace by the margin: hot (5-hour, 90 points ahead). A round there would move 0 sessions (0 warm), 0% of the hot members' recent burn together, in window share, to a cool sibling.`);
});

test('an api_key member reads cool while its worst live bucket holds room, whatever the margin', () => {
  const keyed = (utilization) => ({
    id: 'key',
    kind: 'api_key',
    label: 'key',
    windows: [{ id: 'itpm:claude-sonnet-5', label: 'itpm', observedAt: at(NOW), periodHours: 1 / 60, resetsAt: at(NOW + MINUTE), utilization }],
  });
  // `hot` keeps a round in the preview, so `cool` is read for a round with marks; the margin's band sits at 0.15.
  const read = (key) => d.shedPreview(pool({ members: [member('hot'), member('key')], shedAheadOfPace: 0.2 }), [hot, key], [pin('cold-3', 3, false)], NOW, SIZED).cool;
  assert.equal(read(keyed(0.95)), true, 'a bucket with room is a destination, far above the band');
  assert.equal(read(keyed(1)), false, 'an exhausted bucket is no destination');
});

test("an api_key member's cool reading counts its standing rpm, itpm and otpm buckets alone, as the router's", () => {
  const bucket = (id, utilization, over = {}) => ({ id, label: id, observedAt: at(NOW), periodHours: 1 / 60, resetsAt: at(NOW + MINUTE), utilization, ...over });
  const keyed = (...windows) => ({ id: 'key', kind: 'api_key', label: 'key', windows });
  const read = (key) => d.shedPreview(pool({ members: [member('hot'), member('key')], shedAheadOfPace: 0.2 }), [hot, key], [pin('cold-3', 3, false)], NOW, SIZED).cool;
  assert.equal(read(keyed(bucket('fast_input', 1), bucket('itpm:claude-sonnet-5', 0.4))), true, "an exhausted fast-lane bucket leaves the member cool");
  assert.equal(read(keyed(bucket('itpm:claude-sonnet-5', 1, { observedAt: at(NOW - 61_000), resetsAt: null }))), true, 'a reading without a reset, observed 61 s ago, says nothing about the account now');
  assert.equal(read(keyed(bucket('rpm:claude-sonnet-5', 1))), false, 'a standing rpm bucket at 1.0 is no destination');
  assert.equal(read(keyed(bucket('otpm:claude-sonnet-5', 1, { observedAt: at(NOW - 59_000), resetsAt: null }))), false, 'a reading without a reset, observed 59 s ago, still stands');
});

test('with no member under the band the preview marks sessions it cannot move', () => {
  // Under the threshold of 80 %, so it runs no round of its own, but above the 75 % a shed session has to land under.
  const near = account('near', 0.78);
  const stuck = replay([hot, near], four);
  assert.deepEqual([stuck.sessions, stuck.cool], [2, false]);
  assert.equal(d.shedPreviewLine(stuck, true), `1 member runs ahead of pace by the margin: hot (5-hour, 90 points ahead). A round there would mark 2 sessions (1 warm), 70% of the hot members' recent burn together, in window share, but no sibling reads under the margin less 5 points, so the sessions would be marked and stay.`);
  const cooling = d.shedPreview(pool({ members: [member('hot'), member('cool', 'cooldown')] }), [hot, cool], four, NOW, SIZED);
  assert.equal(cooling.cool, false, 'a member the router skips is no destination');
  const open = replay([hot, near, cool], four);
  assert.deepEqual([open.cool, d.shedPreviewLine(open, true)], [true, `1 member runs ahead of pace by the margin: hot (5-hour, 90 points ahead). A round there would move 2 sessions (1 warm), 70% of the hot members' recent burn together, in window share, to a cool sibling.`]);
  assert.equal(d.shedPreviewLine(open, false), 'No round runs while shed ahead of pace is off.');
  assert.equal(d.shedPreviewLine(replay([cool], []), true), 'No member is ahead of pace by the margin: the closest is cool, 20 points ahead.');
});

test('while the cost model is loading or did not load the preview names the model and counts nothing', () => {
  const round = replay([hot, cool], four);
  assert.equal(d.shedPreviewLine(round, true, 'loading'), 'The cost model is loading, and window-share pins are counted once it is read.');
  assert.equal(d.shedPreviewLine(round, true, 'missing'), 'The cost model did not load, so window-share pins cannot be counted.');
  assert.equal(d.shedPreviewLine(round, true, 'ready'), `1 member runs ahead of pace by the margin: hot (5-hour, 90 points ahead). A round there would move 2 sessions (1 warm), 70% of the hot members' recent burn together, in window share, to a cool sibling.`, 'a read model counts as before');
  assert.equal(d.shedPreviewLine(round, false, 'loading'), 'No round runs while shed ahead of pace is off.', 'with shed ahead of pace off no round runs, model or not');
});

/** A window of which `elapsed` has passed, so its lead over pace is its utilization less that share. */
const windowAt = (id, utilization, elapsed) => {
  const periodHours = id === '5h' ? 5 : 168;
  return { id, label: id, observedAt: at(NOW), periodHours, resetsAt: at(NOW + periodHours * 60 * MINUTE * (1 - elapsed)), utilization };
};
const oauth = (id, ...windows) => ({ id, kind: 'oauth', label: id, windows });
const leadOf = (entry) => [entry.id, entry.window, entry.lead.toFixed(2)];

test('the preview names the hot members with their leading window and lead', () => {
  // `fast` reaches the margin on its 5-hour window alone, `slow` on its 7-day window alone, and `both` on both, where
  // the round takes the 5-hour window first though its lead is the smaller.
  const fast = oauth('fast', windowAt('5h', 0.9, 0), windowAt('7d', 0.2, 0));
  const slow = oauth('slow', windowAt('5h', 0.1, 0), windowAt('7d', 0.9, 0));
  const both = oauth('both', windowAt('5h', 0.85, 0), windowAt('7d', 0.9, 0));
  const round = d.shedPreview(pool({ members: [member('slow'), member('fast'), member('both')] }), [fast, both, slow], [], NOW, SIZED);
  assert.deepEqual(
    round.hot.map(leadOf),
    [
      ['slow', '7d', '0.90'],
      ['fast', '5h', '0.90'],
      ['both', '5h', '0.85'],
    ],
    "the pool's member order, and the 5-hour window first where both reach the margin",
  );
  assert.deepEqual([round.members, round.closest, round.hot[0].label], [3, null, 'slow']);
});

test('the preview names the closest member when none is ahead', () => {
  // Every lead under the 0.8 margin: `behind` trails on both windows, `near` leads on its 7-day window.
  const behind = oauth('behind', windowAt('5h', 0.5, 0.53), windowAt('7d', 0.5, 0.6));
  const trailing = d.shedPreview(pool({ members: [member('behind')] }), [behind], [], NOW, SIZED);
  assert.deepEqual([trailing.hot, leadOf(trailing.closest)], [[], ['behind', '5h', '-0.03']], 'the larger of its two leads, 3 points behind on the 5-hour window');
  assert.equal(d.shedPreviewLine(trailing, true), 'No member is ahead of pace by the margin: the closest is behind, 3 points behind.');
  const near = oauth('near', windowAt('5h', 0.1, 0), windowAt('7d', 0.3, 0));
  const ahead = d.shedPreview(pool({ members: [member('behind'), member('near')] }), [behind, near], [], NOW, SIZED);
  assert.deepEqual(leadOf(ahead.closest), ['near', '7d', '0.30'], 'ahead of pace and still under the margin');
  assert.equal(d.shedPreviewLine(ahead, true), 'No member is ahead of pace by the margin: the closest is near, 30 points ahead.');
  // One point reads in the singular, and a lead that rounds to no point reads on pace.
  const line = (account) => d.shedPreviewLine(d.shedPreview(pool({ members: [member(account.id)] }), [account], [], NOW, SIZED), true);
  assert.equal(line(oauth('one', windowAt('5h', 0.51, 0.5))), 'No member is ahead of pace by the margin: the closest is one, 1 point ahead.');
  assert.equal(line(oauth('even', windowAt('5h', 0.504, 0.5))), 'No member is ahead of pace by the margin: the closest is even, on pace.');
  assert.equal(line(oauth('close', windowAt('5h', 0.507, 0.5))), 'No member is ahead of pace by the margin: the closest is close, on pace.', 'a lead under a hundredth reads on pace, as the Quota card prints it');
  // An API-key member has no shared window, so a pool of them alone has no closest member.
  const keyed = { id: 'key', kind: 'api_key', label: 'key', windows: [] };
  const none = d.shedPreview(pool({ members: [member('key')] }), [keyed], [], NOW, SIZED);
  assert.deepEqual([none.hot, none.closest], [[], null]);
  assert.equal(d.shedPreviewLine(none, true), 'No member is ahead of pace by the margin.');
});

test('the preview line reads the hot members, the round and the closest member in its three shapes', () => {
  // Half of each window elapsed: primary leads by 14 points on its 5-hour window, nightly by 9 on its 7-day one, and
  // spare trails on both, under the band of a margin of 8. Each hot member marks 7 of its 10 burn.
  const primary = oauth('primary', windowAt('5h', 0.64, 0.5), windowAt('7d', 0.3, 0.5));
  const nightly = oauth('nightly', windowAt('5h', 0.2, 0.5), windowAt('7d', 0.59, 0.5));
  const spare = oauth('spare', windowAt('5h', 0.1, 0.5), windowAt('7d', 0.1, 0.5));
  const pins = [...four.map((p) => ({ ...p, accountId: 'primary' })), ...four.map((p) => ({ ...p, accountId: 'nightly', sessionId: `n-${p.sessionId}` }))];
  const sized = new Set(['nightly', 'primary', 'spare']);
  const read = (accounts, over) => d.shedPreview(pool({ members: accounts.map((a) => member(a.id)), shedAheadOfPace: 0.08, ...over }), accounts, pins, NOW, sized);
  const cool = read([primary, nightly, spare]);
  assert.deepEqual(cool.hot.map(leadOf), [
    ['primary', '5h', '0.14'],
    ['nightly', '7d', '0.09'],
  ]);
  assert.equal(
    d.shedPreviewLine(cool, true),
    "2 members run ahead of pace by the margin: primary (5-hour, 14 points ahead) and nightly (7-day, 9 points ahead). A round there would move 4 sessions (2 warm), 70% of the hot members' recent burn together, in window share, to a cool sibling.",
  );
  const stuck = read([primary, nightly]);
  assert.equal(
    d.shedPreviewLine(stuck, true),
    "2 members run ahead of pace by the margin: primary (5-hour, 14 points ahead) and nightly (7-day, 9 points ahead). A round there would mark 4 sessions (2 warm), 70% of the hot members' recent burn together, in window share, but no sibling reads under the margin less 5 points, so the sessions would be marked and stay.",
  );
  const none = read([primary, nightly, spare], { shedAheadOfPace: 0.2 });
  assert.deepEqual([none.hot, leadOf(none.closest)], [[], ['primary', '5h', '0.14']]);
  assert.equal(d.shedPreviewLine(none, true), 'No member is ahead of pace by the margin: the closest is primary, 14 points ahead.');
});

test("the preview's share reads as the hot members' aggregate in one unit and as a per-member average across units", () => {
  const second = account('second', 0.85);
  const heavy = (unit) => [pin('big', 18, false, { accountId: 'second', burnUnit: unit }), pin('small', 2, false, { accountId: 'second', burnUnit: unit })];
  const inDollars = four.map((p) => ({ ...p, burnUnit: 'usd' }));
  // 7 of 10 and 18 of 20 in one unit sum to 25 of 30, while a window share and dollars average their own 70% and 90%.
  const line = (sessions) => d.shedPreviewLine(replay([hot, second, cool], sessions), true);
  const hot2 = '2 members run ahead of pace by the margin: hot (5-hour, 90 points ahead) and second (5-hour, 85 points ahead).';
  assert.equal(line([...four, ...heavy('window_share')]), `${hot2} A round there would move 3 sessions (1 warm), 83% of the hot members' recent burn together, in window share, to a cool sibling.`);
  assert.equal(line([...inDollars, ...heavy('usd')]), `${hot2} A round there would move 3 sessions (1 warm), 83% of the hot members' recent burn together, in dollars, to a cool sibling.`);
  assert.equal(line([...four, ...heavy('usd')]), `${hot2} A round there would move 3 sessions (1 warm), 80% of each member's recent burn on average, to a cool sibling.`);
});
