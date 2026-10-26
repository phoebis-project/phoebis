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
/** A window's resets, newest first, a week apart back from `newest`, with the route's total and count over them. */
const windowOf = (window, shares, { newest = NOW - DAY, sources = [], total } = {}) => {
  const resets = shares.map((shareLeft, i) => ({ observedAt: iso(newest - i * WEEK), resetAt: iso(newest - i * WEEK), shareLeft, source: sources[i] ?? 'sample' }));
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
    assert.deepEqual(d.allowanceFigure(0.33, 'seven_day', { plan: 'team', tier: 'default_claude_team' }, COSTS, unit).kind, 'unpriced', `${unit}: a tier the cost model lacks`);
    assert.deepEqual(d.allowanceFigure(0.33, 'seven_day_fable', { plan: null, tier: null }, COSTS, unit).kind, 'unpriced', `${unit}: no stored tier`);
  }
  assert.equal(d.ALLOWANCE_UNPRICED_TITLE, "Unpriced: the cost model holds no capacity for this account's plan");
  assert.notEqual(d.ALLOWANCE_UNPRICED_TITLE, d.UNPRICED_TITLE, "the analytics library's own title keeps its words beside this one");
  assert.equal(d.allowanceFigure(0.33, 'seven_day', FREE, COSTS, 'share').text, '33%', 'Window shares reads a Free account its shares');
  for (const unit of ['share', ...EQUIVALENT_UNITS]) assert.deepEqual(d.allowanceFigure(null, 'seven_day', PRO, COSTS, unit), { kind: 'none', text: '-', title: d.NO_READING_TITLE }, unit);
  assert.equal(d.NO_READING_TITLE, 'no reading at this reset');
  assert.equal(d.FREE_NOTE, 'Free has no priced capacity in the cost model, so the equivalents read a dash. Window shares reads the shares.');
  const texts = [0, 0.004, 0.33, 0.999].flatMap((share) => ['share', ...EQUIVALENT_UNITS].flatMap((unit) => [PRO, MAX_5X, FREE].flatMap((plan) => ['seven_day', 'seven_day_fable'].map((w) => d.allowanceFigure(share, w, plan, COSTS, unit).text))));
  assert.deepEqual(texts.filter((t) => t.includes('~')), [], 'no model is priced here, so no figure is estimated');
});

test('a Team or Enterprise plan reads the dash on a tier the cost model sizes', () => {
  // Each tier is one the file's costs size, so only the plan word keeps these accounts unpriced, as the router's does.
  const plans = [
    { plan: 'team', tier: 'default_claude_ai' },
    { plan: 'team', tier: 'default_claude_max_20x' },
    { plan: 'enterprise', tier: 'default_claude_max_5x' },
  ];
  for (const plan of plans) {
    for (const window of ['seven_day', 'seven_day_fable']) {
      for (const unit of EQUIVALENT_UNITS) assert.deepEqual(d.allowanceFigure(0.5, window, plan, COSTS, unit), { kind: 'unpriced', text: '-', title: d.ALLOWANCE_UNPRICED_TITLE }, `${plan.plan} on ${plan.tier}, ${window}, ${unit}`);
      assert.deepEqual(d.allowanceFigure(0.5, window, plan, COSTS, 'share'), { kind: 'share', share: 0.5, text: '50%' }, `${plan.plan} on ${plan.tier}, ${window}: Window shares reads the share`);
    }
  }
  // The Team account leaves more than the Max 20x one, so only its plan ranks it last.
  const answer = {
    accounts: [accountOf('max', MAX_20X, [windowOf('seven_day', [0.25, 0.25, 0.25, 0.25])]), accountOf('team', { plan: 'team', tier: 'default_claude_max_20x' }, [windowOf('seven_day', [0.75, 0.75, 0.75, 0.75])])],
    from: iso(NOW - 29 * DAY),
    to: iso(NOW),
  };
  const rows = d.allowanceRows(answer, COSTS, 'native');
  assert.deepEqual(rows.map((r) => [r.accountId, r.unpriced]), [['max', false], ['team', true]]);
  assert.equal(d.fleetFigures(rows, 'native').note, '1 account priced, team left out unpriced');
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
  const reset = (source) => ({ observedAt: iso(at), resetAt: iso(at), shareLeft: 0.5, source });
  assert.deepEqual(
    ['sample', 'clock', 'term_ended', 'replayed'].map((source) => d.resetHover(reset(source))),
    [`${d.fmtAbsolute(iso(at))}, read from a usage sample`, `${d.fmtAbsolute(iso(at))}, read from the clock walk`, `${d.fmtAbsolute(iso(at))}, read from the term's end`, `${d.fmtAbsolute(iso(at))}, read from replayed`],
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
