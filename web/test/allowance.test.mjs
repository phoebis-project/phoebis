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
 * The cost model's members the figures read, anchored on the Pro 7-day window: Ultra's week twenty Pro weeks and Max's
 * eight, and the Fable week a model week of the 7-day window at half its capacity.
 */
const COSTS = {
  capacity: {
    default_claude_ai: { fiveHour: 1_000, sevenDay: 5_000 },
    default_claude_max_20x: { fiveHour: 20_000, sevenDay: 100_000 },
    default_claude_max_5x: { fiveHour: 5_000, sevenDay: 40_000 },
  },
  modelWindows: { seven_day_fable: { capacityFactor: 0.5, of: 'seven_day' } },
  unit: { tier: 'default_claude_ai', window: 'seven_day' },
};
const PRO = { plan: 'pro', tier: 'default_claude_ai' };
const MAX_5X = { plan: 'max_5x', tier: 'default_claude_max_5x' };
const MAX_20X = { plan: 'max_20x', tier: 'default_claude_max_20x' };
const FREE = { plan: 'free', tier: 'default_claude_ai' };
const WEEKS = ['ultra', 'max', 'pro'];

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

test("a figure is the account's own share priced in the chosen week, a Fable-week reset at half its 7-day figure", () => {
  // Half of a Max week is four Pro weeks: a fifth of an Ultra week, half a Max week. Half of its Fable week, a week of
  // half the capacity, is two Pro weeks.
  const figures = (share, window, plan) => WEEKS.map((week) => d.allowanceFigure(share, window, plan, COSTS, week));
  assert.deepEqual(figures(0.5, 'seven_day', MAX_5X).map((f) => [f.kind, f.text]), [
    ['equivalent', '20% Ultra week'],
    ['equivalent', '50% Max week'],
    ['equivalent', '400% Pro week'],
  ]);
  assert.deepEqual(figures(0.5, 'seven_day_fable', MAX_5X).map((f) => [f.kind, f.text]), [
    ['equivalent', '10% Ultra week'],
    ['equivalent', '25% Max week'],
    ['equivalent', '200% Pro week'],
  ]);
  assert.ok(near(d.allowanceFigure(0.5, 'seven_day_fable', MAX_5X, COSTS, 'ultra').share, 0.1), 'the figure a row ranks and adds by is the chosen week\'s');
  // A Pro week is a twentieth of an Ultra week, and an Ultra account's own share reads as it stands under the Ultra
  // week.
  assert.deepEqual(figures(0.33, 'seven_day', PRO).map((f) => f.text), ['1.65% Ultra week', '4.13% Max week', '33% Pro week']);
  assert.equal(d.allowanceFigure(0.25, 'seven_day', MAX_20X, COSTS, 'ultra').text, '25% Ultra week');
});

test('a figure reads a dash where the plan is Free, the cost model sizes no capacity for the tier or no reading stands, and never an estimate', () => {
  for (const week of WEEKS) {
    const free = d.allowanceFigure(0.33, 'seven_day', FREE, COSTS, week);
    assert.deepEqual(free, { kind: 'unpriced', text: '-', title: d.ALLOWANCE_UNPRICED_TITLE }, `${week}: a Free plan reads its Pro tier's capacity no more`);
    assert.deepEqual(d.allowanceFigure(0.33, 'seven_day', { plan: 'max_20x', tier: 'default_claude_team' }, COSTS, week).kind, 'unpriced', `${week}: a tier the cost model lacks`);
    assert.deepEqual(d.allowanceFigure(0.33, 'seven_day_fable', { plan: 'pro', tier: null }, COSTS, week).kind, 'unpriced', `${week}: no stored tier`);
    assert.deepEqual(d.allowanceFigure(null, 'seven_day', PRO, COSTS, week), { kind: 'none', text: '-', title: d.NO_READING_TITLE }, week);
    // An arm the pages never draw, since they hold the skeleton while the cost model loads.
    assert.deepEqual(d.allowanceFigure(0.33, 'seven_day', PRO, null, week), { kind: 'unpriced', text: '-', title: d.ALLOWANCE_UNPRICED_TITLE }, `${week}: no cost model`);
  }
  const texts = [0, 0.004, 0.33, 0.999].flatMap((share) => WEEKS.flatMap((week) => [PRO, MAX_5X, FREE].flatMap((plan) => ['seven_day', 'seven_day_fable'].map((w) => d.allowanceFigure(share, w, plan, COSTS, week).text))));
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
      for (const week of WEEKS) {
        const figure = d.allowanceFigure(0.5, window, seat, COSTS, week);
        assert.equal(figure.kind, 'equivalent', `${seat.plan} on ${seat.tier}, ${window}, ${week}`);
        assert.deepEqual(figure, d.allowanceFigure(0.5, window, same, COSTS, week), `${seat.plan} on ${seat.tier}, ${window}, ${week}: the tier's weight`);
      }
    }
  }
  // The file's costs hold no Team row, so an Enterprise seat on that tier prices nothing, as the router's does.
  const unheld = { plan: 'enterprise', tier: 'default_claude_team' };
  for (const week of WEEKS) assert.deepEqual(d.allowanceFigure(0.5, 'seven_day', unheld, COSTS, week), { kind: 'unpriced', text: '-', title: d.ALLOWANCE_UNPRICED_TITLE }, `${week}: a tier the cost model lacks`);
  // The Team account leaves three of its Ultra weeks against the Ultra account's one, so it ranks first.
  const answer = {
    accounts: [accountOf('max', MAX_20X, [windowOf('seven_day', [0.25, 0.25, 0.25, 0.25])]), accountOf('team', { plan: 'team', tier: 'default_claude_max_20x' }, [windowOf('seven_day', [0.75, 0.75, 0.75, 0.75])])],
    from: iso(NOW - 29 * DAY),
    to: iso(NOW),
  };
  const rows = d.allowanceRows(answer, COSTS, 'ultra');
  assert.deepEqual(rows.map((r) => [r.accountId, r.unpriced]), [['team', false], ['max', false]]);
  assert.equal(d.fleetFigures(rows, 'ultra', d.weekScale(COSTS, 'ultra')).note, '2 accounts priced');
});

/** The cost model of the per-reset cases: Max's 7-day capacity a quarter of Ultra's, five Pro weeks. */
const QUARTER = { ...COSTS, capacity: { ...COSTS.capacity, default_claude_max_5x: { fiveHour: 5_000, sevenDay: 25_000 } } };
const NO_PAIR = { plan: null, tier: null };

test("a reset prices at the plan and the tier it ran under, and at the account's current pair where it carries none", () => {
  for (const week of WEEKS) {
    const bare = d.allowanceFigure(0.5, 'seven_day', MAX_20X, QUARTER, week);
    assert.equal(bare.kind, 'equivalent', week);
    assert.deepEqual(d.allowanceFigure(0.5, 'seven_day', MAX_20X, QUARTER, week, NO_PAIR), bare, `${week}: a reset the router recorded no pair for`);
    // A reset built without the two keys reads as one the router recorded no pair for, the wire writing both or
    // neither.
    assert.deepEqual(d.allowanceFigure(0.5, 'seven_day', MAX_20X, QUARTER, week, {}), bare, `${week}: a reset without the keys`);
    const earlier = d.allowanceFigure(0.5, 'seven_day', MAX_20X, QUARTER, week, MAX_5X);
    assert.equal(earlier.kind, 'equivalent', week);
    assert.ok(near(earlier.share, bare.share / 4), `${week}: a week under Max reads a quarter of the same share at Ultra, ${earlier.share}`);
    assert.deepEqual(d.allowanceFigure(0.5, 'seven_day', MAX_20X, QUARTER, week, FREE), { kind: 'unpriced', text: '-', title: d.ALLOWANCE_UNPRICED_TITLE }, `${week}: a week whose own pair prices nothing, though the account's would`);
  }
  assert.equal(d.allowanceFigure(0.5, 'seven_day', MAX_20X, QUARTER, 'ultra', MAX_5X).text, '12.5% Ultra week');
});

test("an account's windows and the card's column pairs read the 7-day window first, then each model week in the console's order", () => {
  const a = accountOf('a', MAX_20X, [windowOf('seven_day_opus', [0.5]), windowOf('seven_day_fable', [0.5]), windowOf('seven_day', [0.5])]);
  assert.deepEqual(d.allowanceWindows(a).map((w) => w.window), ['seven_day', 'seven_day_fable', 'seven_day_opus']);
  // The card draws the 7-day and Fable pairs whatever the answer holds, and a further model week's pair after them.
  assert.deepEqual(d.weeklyKeys([]), ['seven_day', 'seven_day_fable']);
  assert.deepEqual(d.weeklyKeys([accountOf('b', MAX_20X, [windowOf('seven_day', [0.5])]), a]), ['seven_day', 'seven_day_fable', 'seven_day_opus']);
});

test("a reset's hover names its instant and the reading's source in the block's own words, and its dates always carry the day", () => {
  const at = Date.UTC(2026, 8, 12, 20, 10);
  const reset = (source) => ({ observedAt: iso(at), plan: null, resetAt: iso(at), shareLeft: 0.5, source, tier: null });
  assert.deepEqual(
    ['sample', 'replayed'].map((source) => d.resetHover(reset(source))),
    ['Sep 12, 2026, 20:10:00, read from a usage sample', 'Sep 12, 2026, 20:10:00, read from replayed'],
  );
  // A week's reset lies days back, so even one within a day of now keeps its date.
  assert.deepEqual([d.resetInstant(iso(at)), d.resetInstant(iso(Date.UTC(2026, 9, 25, 7, 5)))], ['Sep 12 20:10', 'Oct 25 07:05']);
});

test("a reset's own share reads on its hover with the plan and the window named in words", () => {
  assert.deepEqual(
    [d.ownShareHover('Ultra', 'seven_day', 0.39), d.ownShareHover('Max', 'seven_day_fable', 0.12), d.ownShareHover('Max', 'seven_day_opus', 0.5)],
    ['Ultra: 39% of the 7-day window left', 'Max: 12% of the Fable week left', 'Max: 50% of seven_day_opus left'],
  );
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

test("the rows sort most left first on the four weeks' 7-day figure, then the Fable figure, then the id, an unpriced account last", () => {
  for (const week of WEEKS) {
    // Pro's weeks weigh a twentieth of Ultra's, so alpha's two weeks' worth reads a tenth of one Ultra week.
    assert.deepEqual(d.allowanceRows(RANKED, COSTS, week).map((r) => r.accountId), ['delta', 'echo', 'bravo', 'alpha', 'foxtrot', 'charlie'], week);
  }
  const rows = d.allowanceRows(RANKED, COSTS, 'ultra');
  const byId = Object.fromEntries(rows.map((r) => [r.accountId, r]));
  assert.deepEqual(Object.keys(byId.delta.figures), ['seven_day', 'seven_day_fable'], 'keyed by the wire key in the drawn order');
  assert.deepEqual(byId.delta.figures.seven_day, { counted: 4, last: d.allowanceFigure(0.25, 'seven_day', MAX_20X, COSTS, 'ultra'), weeks: d.allowanceFigure(1, 'seven_day', MAX_20X, COSTS, 'ultra') });
  assert.deepEqual(byId.bravo.figures.seven_day_fable.counted, 1);
  assert.deepEqual(Object.values(byId).map((r) => [r.accountId, r.resets, r.unpriced]), [
    ['delta', 4, false],
    ['echo', 4, false],
    ['bravo', 4, false],
    ['alpha', 4, false],
    ['foxtrot', 4, false],
    ['charlie', 4, true],
  ]);
  // The latest reset of the account whose term ended is the term's end, so its row carries the mark at that instant.
  assert.deepEqual(rows.map((r) => [r.accountId, r.termEndedAt]).filter(([, at]) => at !== null), [['foxtrot', iso(NOW - 3 * DAY)]]);
  assert.deepEqual([byId.charlie.figures.seven_day.last.kind, byId.charlie.figures.seven_day.weeks.kind], ['unpriced', 'unpriced']);
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

test("the fleet figures add the priced accounts' figures in the chosen week and name the unpriced accounts", () => {
  // 8 Ultra accounts leave 0.75 of a week each, 6 Ultra weeks, and 1.75 of their Fable weeks, 0.875 of an Ultra week.
  assert.deepEqual(
    WEEKS.map((week) => d.fleetFigures(d.allowanceRows(FLEET, COSTS, week), week, d.weekScale(COSTS, week))),
    [
      { fable: '87.5% Ultra week', note: '8 accounts priced, a9 left out unpriced', sevenDay: '600% Ultra week' },
      { fable: '219% Max week', note: '8 accounts priced, a9 left out unpriced', sevenDay: '1500% Max week' },
      { fable: '1750% Pro week', note: '8 accounts priced, a9 left out unpriced', sevenDay: '12000% Pro week' },
    ],
  );
  const priced = { ...FLEET, accounts: FLEET.accounts.slice(0, 1) };
  assert.deepEqual(d.fleetFigures(d.allowanceRows(priced, COSTS, 'pro'), 'pro', d.weekScale(COSTS, 'pro')).note, '1 account priced');
  assert.deepEqual(d.fleetFigures([], 'ultra', d.weekScale(COSTS, 'ultra')), { fable: '-', note: '0 accounts priced', sevenDay: '-' });
});

/**
 * An Ultra account whose newest 7-day week carries no pair and whose second ran under Max, each leaving a whole week,
 * beside an Ultra account whose four weeks leave half a week, and an account one of whose weeks ran on Free.
 */
const MIXED = {
  accounts: [
    accountOf('mixed', MAX_20X, [windowOf('seven_day', [1, 1], { pairs: [NO_PAIR, MAX_5X] })]),
    accountOf('steady', MAX_20X, [windowOf('seven_day', [0.125, 0.125, 0.125, 0.125])]),
  ],
  from: iso(NOW - 29 * DAY),
  to: iso(NOW),
};

test("the four weeks' figure, the rows and the fleet total add each week's figure at the tier it ran under", () => {
  const [mixed] = MIXED.accounts;
  const [window] = mixed.windows;
  // 1 week at Ultra and 1 at Max read 25 Pro weeks, 1.25 Ultra weeks, where the summed shares priced at Ultra read 2.
  const sums = WEEKS.map((week) => d.weeksFigure(window, mixed, QUARTER, week).share);
  assert.ok([1.25, 5, 25].every((sum, i) => near(sums[i], sum)), `the sums under the Ultra, Max and Pro weeks: ${sums}`);
  const weeks = d.weeksFigure(window, mixed, QUARTER, 'ultra');
  assert.deepEqual([weeks.kind, weeks.text], ['equivalent', '125% Ultra week']);
  const row = d.allowanceRows(MIXED, QUARTER, 'ultra').find((r) => r.accountId === 'mixed');
  assert.ok(near(row.figures.seven_day.weeks.share, 1.25) && near(row.figures.seven_day.last.share, 1), `weeks ${row.figures.seven_day.weeks.share}, last ${row.figures.seven_day.last.share}`);
  assert.equal(row.unpriced, false);
  // 1.25 beside steady's 0.5, where the summed shares priced once read 2.5.
  assert.equal(d.fleetFigures(d.allowanceRows(MIXED, QUARTER, 'ultra'), 'ultra', d.weekScale(QUARTER, 'ultra')).sevenDay, '175% Ultra week');

  // A week on Free prices nothing, so the four weeks it joins read the dash, and the row is marked under every week.
  const freed = accountOf('freed', MAX_20X, [windowOf('seven_day', [0.5, 0.5], { pairs: [NO_PAIR, FREE] })]);
  const answer = { ...MIXED, accounts: [...MIXED.accounts, freed] };
  for (const week of WEEKS) {
    assert.deepEqual(d.weeksFigure(freed.windows[0], freed, QUARTER, week), { kind: 'unpriced', text: '-', title: d.ALLOWANCE_UNPRICED_TITLE }, week);
    const rows = d.allowanceRows(answer, QUARTER, week);
    const freedRow = rows.find((r) => r.accountId === 'freed');
    assert.deepEqual([freedRow.figures.seven_day.last.kind, freedRow.figures.seven_day.weeks.kind, freedRow.unpriced], ['equivalent', 'unpriced', true], week);
    assert.deepEqual(rows.map((r) => r.accountId), ['mixed', 'steady', 'freed'], `${week}: the unpriced row last`);
    assert.equal(d.fleetFigures(rows, week, d.weekScale(QUARTER, week)).note, '2 accounts priced, freed left out unpriced', week);
  }
});

test("a cost model without the chosen week's tier titles every dash with that week, and the fleet note says so in place of a count", () => {
  // No Ultra row, so no figure reads in the Ultra week, whatever the account's own plan prices at.
  const ultraless = { ...COSTS, capacity: { default_claude_ai: COSTS.capacity.default_claude_ai, default_claude_max_5x: COSTS.capacity.default_claude_max_5x } };
  const title = 'Unpriced: the cost model holds no 7-day capacity for the Ultra week';
  assert.deepEqual(d.allowanceFigure(0.5, 'seven_day', MAX_5X, ultraless, 'ultra'), { kind: 'unpriced', text: '-', title });
  const rows = d.allowanceRows({ accounts: [accountOf('nightly', MAX_5X, [windowOf('seven_day', [0.5])])], from: iso(NOW - 29 * DAY), to: iso(NOW) }, ultraless, 'ultra');
  assert.deepEqual(d.fleetFigures(rows, 'ultra', d.weekScale(ultraless, 'ultra')), { fable: '-', note: title, sevenDay: '-' });
});
