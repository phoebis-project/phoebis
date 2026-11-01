import assert from 'node:assert/strict';
import { test } from 'node:test';

// The dates below are read in UTC, so the file runs there whatever the host's zone, and `node --test` gives each file
// a process of its own.
process.env.TZ = 'UTC';

const d = await import('./dist/api.js');

const DAY = 86_400_000;
const WEEK = 7 * DAY;
const iso = (ms) => new Date(ms).toISOString();
const near = (a, b) => Math.abs(a - b) < 1e-9;

/**
 * The cost model's members the figures read, with capacities of the file's own: Pro's week a twentieth of the anchor's
 * and Max 5x's two fifths, the Fable week a model week of the 7-day window.
 */
const COSTS = {
  capacity: {
    default_claude_ai: { fiveHour: 1_000, sevenDay: 5_000 },
    default_claude_max_20x: { fiveHour: 20_000, sevenDay: 100_000 },
    default_claude_max_5x: { fiveHour: 5_000, sevenDay: 40_000 },
  },
  modelWindows: { seven_day_fable: { capacityFactor: 0.25, of: 'seven_day' } },
  unit: { definedBy: { band: 'off_peak', tier: 'default_claude_max_20x', window: 'five_hour' }, unitsPerWindow: 20_000 },
};
const PRO = { plan: 'pro', tier: 'default_claude_ai' };
const MAX_5X = { plan: 'max_5x', tier: 'default_claude_max_5x' };
const MAX_20X = { plan: 'max_20x', tier: 'default_claude_max_20x' };
const FREE = { plan: 'free', tier: 'default_claude_ai' };
const EQUIVALENT_UNITS = ['native', 'all', 'usd', 'equivalent'];

const NOW = Date.UTC(2026, 9, 25, 18, 0);
/**
 * A window's resets, newest first, a week apart back from `newest`, with the route's total and count over them, each
 * carrying the pair `pairs` names for it and the decoded nulls otherwise.
 */
const windowOf = (window, shares, { newest = NOW - DAY, pairs = [], sources = [], total } = {}) => {
  const resets = shares.map((shareLeft, i) => ({ observedAt: iso(newest - i * WEEK), plan: pairs[i]?.plan ?? null, resetAt: iso(newest - i * WEEK), shareLeft, source: sources[i] ?? 'sample', tier: pairs[i]?.tier ?? null }));
  return { resets, resetsCounted: resets.length, totalShareLeft: total ?? shares.reduce((s, x) => s + x, 0), window };
};
const accountOf = (accountId, plan, windows) => ({ accountId, ...plan, windows });

test('the span runs 29 days behind now to now, four weeks and a day of slack', () => {
  assert.equal(d.ALLOWANCE_WEEKS, 4);
  assert.deepEqual(d.allowanceSpan(NOW), { from: iso(NOW - 29 * DAY), to: iso(NOW) });
});

test("the newest four resets total the answer's figures where it lists four or fewer, and the console's own sum and count over more", () => {
  // Three shares whose sum in floating point is 0.6000000000000001, beside the route's 0.6.
  const three = windowOf('seven_day', [0.1, 0.2, 0.3], { total: 0.6 });
  assert.deepEqual(d.newestResets(three), { counted: 3, resets: three.resets, total: 0.6 });
  const six = windowOf('seven_day', [0.5, 0.25, 0.125, 0.25, 0.75, 1], { total: 2.875 });
  const newest = d.newestResets(six);
  assert.deepEqual([newest.counted, newest.resets, newest.total], [4, six.resets.slice(0, 4), 1.125]);
  assert.equal(d.newestResets(six, 2).total, 0.75, 'a narrower read sums its own newest');
  // A week the term ended inside is a reset of that week, so its share counts in the total as any other.
  const ended = windowOf('seven_day', [0.5, 0.25, 0.25, 0.125, 0.75], { sources: ['term_ended'] });
  assert.deepEqual([d.newestResets(ended).counted, d.newestResets(ended).total, d.newestResets(ended).resets[0].source], [4, 1.125, 'term_ended']);
});

test("a figure is the share of the account's own window under Window shares and its 20x window equivalent under every other position", () => {
  assert.deepEqual(d.allowanceFigure(0.33, 'seven_day', PRO, COSTS, 'share'), { kind: 'share', share: 0.33, text: d.fmtShare(0.33) });
  assert.equal(d.allowanceFigure(0.33, 'seven_day', PRO, COSTS, 'share').text, '33%');
  for (const unit of EQUIVALENT_UNITS) {
    // A Pro week is a twentieth of the anchor's, so 33% of it is 1.65% of a Max 20x week.
    const pro = d.allowanceFigure(0.33, 'seven_day', PRO, COSTS, unit);
    assert.equal(pro.kind, 'equivalent', unit);
    assert.ok(near(pro.share, 0.0165), `${unit}: ${pro.share}`);
    assert.equal(pro.text, d.equivalentShort('seven_day', pro.share), unit);
    assert.equal(pro.text, '1.65% 20x 7d', unit);
    // A model week prices through the shared window it divides, whose capacity factor every tier shares.
    const fable = d.allowanceFigure(0.5, 'seven_day_fable', MAX_5X, COSTS, unit);
    assert.deepEqual([fable.kind, fable.text], ['equivalent', '20% 20x 7-day Fable'], unit);
    assert.equal(d.allowanceFigure(0.25, 'seven_day', MAX_20X, COSTS, unit).text, '25% 20x 7d', `${unit}: the anchor's own share`);
  }
});

test('a figure reads a dash where the plan is Free, the cost model sizes no capacity for the tier or no reading stands, and never an estimate', () => {
  for (const unit of EQUIVALENT_UNITS) {
    const free = d.allowanceFigure(0.33, 'seven_day', FREE, COSTS, unit);
    assert.deepEqual(free, { kind: 'unpriced', text: '-', title: d.ALLOWANCE_UNPRICED_TITLE }, `${unit}: a Free plan reads its Pro tier's capacity no more`);
    assert.deepEqual(d.allowanceFigure(0.33, 'seven_day', { plan: 'max_20x', tier: 'default_claude_team' }, COSTS, unit).kind, 'unpriced', `${unit}: a tier the cost model lacks`);
    assert.deepEqual(d.allowanceFigure(0.33, 'seven_day_fable', { plan: 'pro', tier: null }, COSTS, unit).kind, 'unpriced', `${unit}: no stored tier`);
  }
  assert.equal(d.allowanceFigure(0.33, 'seven_day', FREE, COSTS, 'share').text, '33%', 'Window shares reads a Free account its shares');
  for (const unit of ['share', ...EQUIVALENT_UNITS]) assert.deepEqual(d.allowanceFigure(null, 'seven_day', PRO, COSTS, unit), { kind: 'none', text: '-', title: d.NO_READING_TITLE }, unit);
  const texts = [0, 0.004, 0.33, 0.999].flatMap((share) => ['share', ...EQUIVALENT_UNITS].flatMap((unit) => [PRO, MAX_5X, FREE].flatMap((plan) => ['seven_day', 'seven_day_fable'].map((w) => d.allowanceFigure(share, w, plan, COSTS, unit).text))));
  assert.deepEqual(texts.filter((t) => t.includes('~')), [], 'no model is priced here, so no figure is estimated');
});

test('a Team or Enterprise seat prices through its tier where the cost model holds the row and reads the dash where it lacks it', () => {
  // Each held tier prices as the plan it resolves to on its own would, at that tier's weight.
  const held = [
    [{ plan: 'team', tier: 'default_claude_ai' }, PRO],
    [{ plan: 'team', tier: 'default_claude_max_20x' }, MAX_20X],
    [{ plan: 'enterprise', tier: 'default_claude_max_5x' }, MAX_5X],
  ];
  for (const [seat, same] of held) {
    for (const window of ['seven_day', 'seven_day_fable']) {
      for (const unit of EQUIVALENT_UNITS) {
        const figure = d.allowanceFigure(0.5, window, seat, COSTS, unit);
        assert.equal(figure.kind, 'equivalent', `${seat.plan} on ${seat.tier}, ${window}, ${unit}`);
        assert.deepEqual(figure, d.allowanceFigure(0.5, window, same, COSTS, unit), `${seat.plan} on ${seat.tier}, ${window}, ${unit}: the tier's weight`);
      }
      assert.deepEqual(d.allowanceFigure(0.5, window, seat, COSTS, 'share'), { kind: 'share', share: 0.5, text: '50%' }, `${seat.plan} on ${seat.tier}, ${window}: Window shares reads the share`);
    }
  }
  // The file's costs hold no Team row, so an Enterprise seat on that tier prices nothing, as the router's does.
  const unheld = { plan: 'enterprise', tier: 'default_claude_team' };
  for (const unit of EQUIVALENT_UNITS) assert.deepEqual(d.allowanceFigure(0.5, 'seven_day', unheld, COSTS, unit), { kind: 'unpriced', text: '-', title: d.ALLOWANCE_UNPRICED_TITLE }, `${unit}: a tier the cost model lacks`);
  // The Team account leaves three of its Max 20x weeks against the Max 20x account's one, so it ranks first.
  const answer = {
    accounts: [accountOf('max', MAX_20X, [windowOf('seven_day', [0.25, 0.25, 0.25, 0.25])]), accountOf('team', { plan: 'team', tier: 'default_claude_max_20x' }, [windowOf('seven_day', [0.75, 0.75, 0.75, 0.75])])],
    from: iso(NOW - 29 * DAY),
    to: iso(NOW),
  };
  const rows = d.allowanceRows(answer, COSTS, 'native');
  assert.deepEqual(rows.map((r) => [r.accountId, r.unpriced]), [['team', false], ['max', false]]);
  assert.equal(d.fleetFigures(rows, 'native').note, '2 accounts priced');
});

/** The cost model of the per-reset cases: Max 5x's 7-day capacity a quarter of the anchor's, Pro's a twentieth. */
const QUARTER = { ...COSTS, capacity: { ...COSTS.capacity, default_claude_max_5x: { fiveHour: 5_000, sevenDay: 25_000 } } };
const NO_PAIR = { plan: null, tier: null };

test("a reset prices at the plan and the tier it ran under, and at the account's current pair where it carries none", () => {
  for (const unit of EQUIVALENT_UNITS) {
    const bare = d.allowanceFigure(0.5, 'seven_day', MAX_20X, QUARTER, unit);
    assert.deepEqual([bare.kind, bare.share], ['equivalent', 0.5], `${unit}: the anchor's own week`);
    assert.deepEqual(d.allowanceFigure(0.5, 'seven_day', MAX_20X, QUARTER, unit, NO_PAIR), bare, `${unit}: a reset the router recorded no pair for`);
    // A reset built without the two keys reads as one the router recorded no pair for, the wire writing both or
    // neither.
    assert.deepEqual(d.allowanceFigure(0.5, 'seven_day', MAX_20X, QUARTER, unit, {}), bare, `${unit}: a reset without the keys`);
    const earlier = d.allowanceFigure(0.5, 'seven_day', MAX_20X, QUARTER, unit, MAX_5X);
    assert.equal(earlier.kind, 'equivalent', unit);
    assert.ok(near(earlier.share, bare.share / 4), `${unit}: a week under Max 5x reads a quarter of the same share at Max 20x, ${earlier.share}`);
    assert.equal(earlier.text, d.equivalentShort('seven_day', 0.125), unit);
    assert.deepEqual(d.allowanceFigure(0.5, 'seven_day', MAX_20X, QUARTER, unit, FREE), { kind: 'unpriced', text: '-', title: d.ALLOWANCE_UNPRICED_TITLE }, `${unit}: a week whose own pair prices nothing, though the account's would`);
  }
  // The shares price nothing, so the pair changes no share, with no cost model as with one.
  for (const costs of [QUARTER, null]) {
    for (const pair of [MAX_5X, FREE, NO_PAIR]) assert.deepEqual(d.allowanceFigure(0.5, 'seven_day', MAX_20X, costs, 'share', pair), { kind: 'share', share: 0.5, text: '50%' }, `${pair.plan} ${costs === null ? 'without' : 'with'} a cost model`);
  }
});

test('the subtitle names the unit the figures read in', () => {
  assert.equal(d.allowanceSubtitle('share'), 'The share of each weekly window left unused when it reset, the last four weeks');
  for (const unit of EQUIVALENT_UNITS) assert.equal(d.allowanceSubtitle(unit), 'Each weekly window left unused when it reset, in 20x window equivalents, the last four weeks', unit);
});

test("an account's windows and the card's column pairs read the 7-day window first, then each model week in the console's order", () => {
  const a = accountOf('a', MAX_20X, [windowOf('seven_day_opus', [0.5]), windowOf('seven_day_fable', [0.5]), windowOf('seven_day', [0.5])]);
  assert.deepEqual(d.allowanceWindows(a).map((w) => w.window), ['seven_day', 'seven_day_fable', 'seven_day_opus']);
  // The card draws the 7-day and Fable pairs whatever the answer holds, and a further model week's pair after them.
  assert.deepEqual(d.weeklyKeys([]), ['seven_day', 'seven_day_fable']);
  assert.deepEqual(d.weeklyKeys([accountOf('b', MAX_20X, [windowOf('seven_day', [0.5])]), a]), ['seven_day', 'seven_day_fable', 'seven_day_opus']);
  assert.deepEqual(['seven_day', 'seven_day_fable', 'seven_day_opus'].map(d.allowanceColumnWord), ['7-day', 'Fable', 'seven_day_opus']);
});

test("a reset's hover names its instant and the reading's source in the block's own words, and its dates always carry the day", () => {
  const at = Date.UTC(2026, 8, 12, 20, 10);
  const reset = (source) => ({ observedAt: iso(at), plan: null, resetAt: iso(at), shareLeft: 0.5, source, tier: null });
  assert.deepEqual(
    ['sample', 'replayed'].map((source) => d.resetHover(reset(source))),
    ['Sep 12, 2026, 20:10:00, read from a usage sample', 'Sep 12, 2026, 20:10:00, read from replayed'],
  );
  // A week's reset lies days back, so even one within a day of now keeps its date.
  assert.deepEqual([d.resetDate(iso(at)), d.resetInstant(iso(at)), d.resetInstant(iso(Date.UTC(2026, 9, 25, 7, 5)))], ['Sep 12', 'Sep 12 20:10', 'Oct 25 07:05']);
});

/**
 * Six accounts whose four weeks' 7-day shares tie three ways at one week's worth, told apart by the Fable figure and
 * then the id, beside a Free account with the most left and an account whose latest reset is its term's end.
 */
const RANKED = {
  accounts: [
    accountOf('alpha', PRO, [windowOf('seven_day', [0.5, 0.5, 0.5, 0.5])]),
    accountOf('bravo', MAX_20X, [windowOf('seven_day', [0.25, 0.25, 0.25, 0.25]), windowOf('seven_day_fable', [0.125])]),
    accountOf('charlie', FREE, [windowOf('seven_day', [0.75, 0.75, 0.75, 0.75])]),
    accountOf('delta', MAX_20X, [windowOf('seven_day_fable', [0.5]), windowOf('seven_day', [0.25, 0.25, 0.25, 0.25])]),
    accountOf('echo', MAX_20X, [windowOf('seven_day', [0.25, 0.25, 0.25, 0.25]), windowOf('seven_day_fable', [0.5])]),
    accountOf('foxtrot', PRO, [windowOf('seven_day', [0.25, 0.25, 0.25, 0.25], { newest: NOW - 3 * DAY, sources: ['term_ended'] })]),
  ],
  from: iso(NOW - 29 * DAY),
  to: iso(NOW),
};

test('the rows sort most left first on the four weeks\' 7-day figure, then the Fable figure, then the id, an unpriced account last under the equivalents', () => {
  const shares = d.allowanceRows(RANKED, COSTS, 'share');
  // Window shares reads the Free account's shares, so it ranks in its share's place.
  assert.deepEqual(shares.map((r) => r.accountId), ['charlie', 'alpha', 'delta', 'echo', 'bravo', 'foxtrot']);
  for (const unit of EQUIVALENT_UNITS) {
    // Pro's weeks weigh a twentieth of Max 20x's, so alpha's two weeks' worth reads a tenth of one anchor week.
    assert.deepEqual(d.allowanceRows(RANKED, COSTS, unit).map((r) => r.accountId), ['delta', 'echo', 'bravo', 'alpha', 'foxtrot', 'charlie'], unit);
  }
  const byId = Object.fromEntries(shares.map((r) => [r.accountId, r]));
  assert.deepEqual(Object.keys(byId.delta.figures), ['seven_day', 'seven_day_fable'], 'keyed by the wire key in the drawn order');
  assert.deepEqual(byId.delta.figures.seven_day, { counted: 4, last: d.allowanceFigure(0.25, 'seven_day', MAX_20X, COSTS, 'share'), weeks: d.allowanceFigure(1, 'seven_day', MAX_20X, COSTS, 'share') });
  assert.deepEqual(byId.bravo.figures.seven_day_fable.counted, 1);
  assert.deepEqual(Object.values(byId).map((r) => [r.accountId, r.resets, r.unpriced]), [
    ['charlie', 4, true],
    ['alpha', 4, false],
    ['delta', 4, false],
    ['echo', 4, false],
    ['bravo', 4, false],
    ['foxtrot', 4, false],
  ]);
  // The latest reset of the account whose term ended is the term's end, so its row carries the mark at that instant.
  assert.deepEqual(shares.map((r) => [r.accountId, r.termEndedAt]).filter(([, at]) => at !== null), [['foxtrot', iso(NOW - 3 * DAY)]]);
  const charlie = d.allowanceRows(RANKED, COSTS, 'native').find((r) => r.accountId === 'charlie');
  assert.deepEqual([charlie.figures.seven_day.last.kind, charlie.figures.seven_day.weeks.kind], ['unpriced', 'unpriced']);
});

test('under Window shares the rows read the shares with no cost model, the unpriced mark read off the plan word', () => {
  // The shares price nothing, so the card draws them before the cost model lands or after its read failed.
  const bare = d.allowanceRows(RANKED, null, 'share');
  assert.deepEqual(bare, d.allowanceRows(RANKED, COSTS, 'share'));
  // An equivalent with no cost model reads the dash, an arm the pages never draw, since they wait on the model there.
  for (const unit of EQUIVALENT_UNITS) assert.deepEqual(d.allowanceFigure(0.33, 'seven_day', PRO, null, unit), { kind: 'unpriced', text: '-', title: d.ALLOWANCE_UNPRICED_TITLE }, unit);
});

/** Nine accounts with 17 resets of the 7-day window and three of the Fable week, the ninth on a Free plan. */
const FLEET = {
  accounts: [
    ...Array.from({ length: 8 }, (_, i) =>
      accountOf(`a${i + 1}`, MAX_20X, [windowOf('seven_day', [0.5, 0.25]), ...(i === 0 ? [windowOf('seven_day_fable', [0.5, 0.25])] : i === 1 ? [windowOf('seven_day_fable', [1])] : [])]),
    ),
    accountOf('a9', FREE, [windowOf('seven_day', [0.25])]),
  ],
  from: iso(NOW - 29 * DAY),
  to: iso(NOW),
};

test("the fleet figures read the mean share per reset under Window shares and the priced accounts' equivalents added under every other position", () => {
  // 8 accounts leave 0.75 of a week over two resets and the Free one 0.25 over one: 6.25 over 17 resets. The Fable
  // weeks leave 1.75 over three resets.
  assert.deepEqual(d.fleetFigures(d.allowanceRows(FLEET, COSTS, 'share'), 'share'), {
    fable: d.fmtShare(1.75 / 3),
    note: "17 resets on 9 accounts, each a share of its account's own week",
    sevenDay: d.fmtShare(6.25 / 17),
  });
  for (const unit of EQUIVALENT_UNITS) {
    assert.deepEqual(d.fleetFigures(d.allowanceRows(FLEET, COSTS, unit), unit), { fable: d.equivalentShort('seven_day_fable', 1.75), note: '8 accounts priced, a9 left out unpriced', sevenDay: d.equivalentShort('seven_day', 6) }, unit);
  }
  const priced = { ...FLEET, accounts: FLEET.accounts.slice(0, 1) };
  assert.deepEqual(d.fleetFigures(d.allowanceRows(priced, COSTS, 'usd'), 'usd').note, '1 account priced');
  assert.deepEqual(d.fleetFigures(d.allowanceRows({ ...FLEET, accounts: FLEET.accounts.slice(8) }, COSTS, 'share'), 'share'), { fable: '-', note: "1 reset on 1 account, each a share of its account's own week", sevenDay: d.fmtShare(0.25) });
  assert.deepEqual(d.fleetFigures([], 'native'), { fable: '-', note: '0 accounts priced', sevenDay: '-' });
});

/**
 * A Max 20x account whose newest 7-day week carries no pair and whose second ran under Max 5x, each leaving a whole
 * week, beside a Max 20x account whose four weeks leave half a week, and an account one of whose weeks ran on Free.
 */
const MIXED = {
  accounts: [
    accountOf('mixed', MAX_20X, [windowOf('seven_day', [1, 1], { pairs: [NO_PAIR, MAX_5X] })]),
    accountOf('steady', MAX_20X, [windowOf('seven_day', [0.125, 0.125, 0.125, 0.125])]),
  ],
  from: iso(NOW - 29 * DAY),
  to: iso(NOW),
};

test("the four weeks' figure, the rows and the fleet total add each week's equivalent at the tier it ran under", () => {
  const [mixed, steady] = MIXED.accounts;
  const [window] = mixed.windows;
  for (const unit of EQUIVALENT_UNITS) {
    // 1 week at Max 20x and 1 at Max 5x read 1.25 anchor weeks, where the summed shares priced at Max 20x read 2.
    const weeks = d.weeksFigure(window, mixed, QUARTER, unit);
    assert.equal(weeks.kind, 'equivalent', unit);
    assert.ok(near(weeks.share, 1.25), `${unit}: ${weeks.share}`);
    assert.equal(weeks.text, d.equivalentShort('seven_day', 1.25), unit);
    const row = d.allowanceRows(MIXED, QUARTER, unit).find((r) => r.accountId === 'mixed');
    assert.ok(near(row.figures.seven_day.weeks.share, 1.25) && near(row.figures.seven_day.last.share, 1), `${unit}: weeks ${row.figures.seven_day.weeks.share}, last ${row.figures.seven_day.last.share}`);
    assert.equal(row.unpriced, false, unit);
    // 1.25 beside steady's 0.5, where the summed shares priced once read 2.5.
    assert.equal(d.fleetFigures(d.allowanceRows(MIXED, QUARTER, unit), unit).sevenDay, d.equivalentShort('seven_day', 1.75), unit);
  }
  assert.deepEqual(d.weeksFigure(window, mixed, QUARTER, 'share'), { kind: 'share', share: 2, text: d.fmtShare(2) }, "Window shares adds the shares as the answer's total");
  assert.deepEqual(d.allowanceRows(MIXED, QUARTER, 'share').find((r) => r.accountId === 'mixed').figures.seven_day.weeks, { kind: 'share', share: 2, text: d.fmtShare(2) });
  assert.deepEqual(d.weeksFigure(steady.windows[0], steady, null, 'share'), { kind: 'share', share: 0.5, text: d.fmtShare(0.5) }, 'the shares with no cost model');

  // A week on Free prices nothing, so the four weeks it joins read the dash, and the row is marked under every unit.
  const freed = accountOf('freed', MAX_20X, [windowOf('seven_day', [0.5, 0.5], { pairs: [NO_PAIR, FREE] })]);
  const answer = { ...MIXED, accounts: [...MIXED.accounts, freed] };
  for (const unit of EQUIVALENT_UNITS) {
    assert.deepEqual(d.weeksFigure(freed.windows[0], freed, QUARTER, unit), { kind: 'unpriced', text: '-', title: d.ALLOWANCE_UNPRICED_TITLE }, unit);
    const rows = d.allowanceRows(answer, QUARTER, unit);
    const row = rows.find((r) => r.accountId === 'freed');
    assert.deepEqual([row.figures.seven_day.last.kind, row.figures.seven_day.weeks.kind, row.unpriced], ['equivalent', 'unpriced', true], unit);
    assert.deepEqual(rows.map((r) => r.accountId), ['mixed', 'steady', 'freed'], `${unit}: the unpriced row last`);
    assert.deepEqual(d.fleetFigures(rows, unit), { fable: '-', note: '2 accounts priced, freed left out unpriced', sevenDay: d.equivalentShort('seven_day', 1.75) }, unit);
  }
  for (const costs of [QUARTER, null]) {
    const marks = d.allowanceRows(answer, costs, 'share').map((r) => [r.accountId, r.unpriced]);
    assert.deepEqual(marks, [['mixed', false], ['freed', true], ['steady', false]], `Window shares ${costs === null ? 'without' : 'with'} a cost model`);
  }
});
