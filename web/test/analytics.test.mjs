import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import * as d from './dist/api.js';

const fixture = (name) => JSON.parse(readFileSync(new URL(`./fixtures/${name}.json`, import.meta.url), 'utf8')).body;

const HOUR = 3_600_000;
const point = (at, group, over = {}) => {
  const p = { at: new Date(at).toISOString(), cachedTokens: 0, equivalent: null, fiveHourShare: null, group, planUsd: 0, sevenDayShare: null, uncachedTokens: 0, unit: null, unpriced: false, usd: 0, ...over };
  return { tokens: p.cachedTokens + p.uncachedTokens, ...p };
};
const NONE = new Set();
/** The cost model with nothing in it, which a test fills with the rows it needs. */
const EMPTY = d.decodeSubscriptionCosts(undefined, fixture('test_tables').subscription_costs.unit);

test('a conversation class and an overhead class split as the legend says', () => {
  for (const conversation of ['compaction', 'main', 'subagent']) assert.equal(d.classLegendOf(conversation), 'conversation', conversation);
  for (const overhead of ['auxiliary', 'workflow', 'a-class-added-later']) assert.equal(d.classLegendOf(overhead), 'overhead', overhead);
  assert.equal(d.classLegendOf('-'), 'overhead', 'a request that named no class is overhead');
  assert.equal(d.classLegendOf('Main'), 'overhead', 'the class is the header as sent, so the match is exact');

  // The burn fold draws the two legend entries whatever classes the range holds, and sums each side.
  const { lines, rows } = d.foldBurn(
    [point(0, 'main', { cachedTokens: 600 }), point(0, 'subagent', { uncachedTokens: 60 }), point(0, 'auxiliary', { cachedTokens: 120 }), point(0, '-', { uncachedTokens: 60 })],
    'request_class',
    60,
    NONE,
  );
  assert.deepEqual(lines.map((l) => l.key), ['conversation', 'overhead']);
  assert.deepEqual([rows[0].cached, rows[0].uncached], [[600, 120], [60, 60]], 'a one-minute bucket reads its tokens as they are');
});

test('the burn fold reads every token and dollar figure per minute of its bucket and the Pro-week figure per hour', () => {
  const { lines, rows } = d.foldBurn(
    [point(0, 'primary', { cachedTokens: 36_000, equivalent: 0.06, uncachedTokens: 3_600, unit: 'window_share' }), point(6 * HOUR, 'primary', { unit: 'window_share' })],
    'account',
    6 * 3_600,
    new Set(['primary']),
  );
  assert.deepEqual([rows[0].cached[0], rows[0].uncached[0], rows[0].usd[0]], [100, 10, 0], 'six hours of tokens over 360 minutes');
  assert.ok(Math.abs(rows[0].equivalent[0] - 0.01) < 1e-12, 'a figure of 6% of a Pro week over six hours is 1% an hour');
  assert.equal(rows[1].equivalent[0], 0, 'a bucket with no traffic reads zero on a line that priced');
  assert.equal(lines[0].peak, rows[0].equivalent[0]);
  assert.ok(Math.abs(lines[0].perHour - 0.005) < 1e-12, 'the range average spreads the figure over both buckets');
  assert.ok(Math.abs(lines[0].tokensPerMinute - 55) < 1e-9, 'the range average spreads the tokens over both buckets');
});

test('a range ending now, twenty minutes into the running hour, rates that bucket over 1,200 seconds', () => {
  const at13 = Date.UTC(2026, 8, 16, 13);
  const end = at13 + HOUR + 20 * 60_000;
  const { lines, rows } = d.foldBurn(
    [
      point(at13, 'primary', { equivalent: 0.3, planUsd: 0.3, uncachedTokens: 6_000, unit: 'window_share' }),
      point(at13, 'overflow', { unit: 'usd', usd: 6 }),
      point(at13 + HOUR, 'primary', { equivalent: 0.2, planUsd: 0.2, uncachedTokens: 4_000, unit: 'window_share' }),
      point(at13 + HOUR, 'overflow', { unit: 'usd', usd: 4 }),
    ],
    'account',
    3_600,
    new Set(['primary']),
    end,
    end,
  );
  assert.deepEqual(rows.map((r) => r.seconds), [3_600, 1_200], 'the full first hour, and the twenty minutes the second has run');
  const [primary, overflowLine] = [lines.findIndex((l) => l.key === 'primary'), lines.findIndex((l) => l.key === 'overflow')];
  assert.deepEqual([rows[0].uncached[primary], rows[1].uncached[primary]], [100, 200], 'tokens per minute of the time each bucket covers');
  assert.ok(Math.abs(rows[1].usd[overflowLine] - 0.2) < 1e-12, 'four dollars over twenty minutes');
  assert.ok(Math.abs(rows[1].equivalent[primary] - 0.6) < 1e-12, 'a figure of 20% over a third of an hour is 60% an hour');
  assert.ok(Math.abs(rows[1].planUsd[primary] - 0.6) < 1e-12, 'plan dollars of $0.20 over a third of an hour are $0.60 an hour');
  // Weighed 3:1 by the seconds each bucket covers: (100 x 3 + 200) / 4 and (0.3 x 3 + 0.6) / 4.
  assert.ok(Math.abs(lines[primary].tokensPerMinute - 125) < 1e-9, 'the legend weighs each bucket by the time it covers');
  assert.ok(Math.abs(lines[primary].perHour - 0.375) < 1e-12, 'and the figure');
  assert.ok(Math.abs(lines[primary].planUsdPerHour - 0.375) < 1e-12, 'and the plan dollars');
  assert.ok(Math.abs(lines[overflowLine].usdPerMinute - 0.125) < 1e-12, 'and the dollars');
});

test('a past range ending twenty minutes into an hour rates that hour in full', () => {
  // The route reads whole hours through the hour ceiling of the range's end, so it answered the whole 14:00 hour.
  const at13 = Date.UTC(2026, 8, 16, 13);
  const { rows } = d.foldBurn([point(at13, 'overflow', { uncachedTokens: 600 }), point(at13 + HOUR, 'overflow', { uncachedTokens: 600 })], 'key', 3_600, NONE, at13 + HOUR + 20 * 60_000, at13 + 3 * HOUR);
  assert.deepEqual(rows.map((r) => r.seconds), [3_600, 3_600]);
  assert.deepEqual(rows.map((r) => r.uncached[0]), [10, 10]);
});

test('a past range ending inside a 6-hour bucket rates it over the hours read', () => {
  // The bucket at 12:00 holds the hours 12:00 to 15:00, the hour ceiling of an end at 14:20.
  const at12 = Date.UTC(2026, 8, 16, 12);
  const end = at12 + 2 * HOUR + 20 * 60_000;
  const { rows } = d.foldBurn([point(at12 - 6 * HOUR, 'overflow', { uncachedTokens: 3_600 }), point(at12, 'overflow', { uncachedTokens: 1_800 })], 'key', 6 * 3_600, NONE, end, end + 24 * HOUR);
  assert.deepEqual(rows.map((r) => r.seconds), [21_600, 10_800]);
  assert.deepEqual(rows.map((r) => r.uncached[0]), [10, 10]);
});

test('a range ending on a bucket boundary rates every bucket over the full width', () => {
  const at12 = Date.UTC(2026, 8, 16, 12);
  const points = [point(at12, 'overflow', { cachedTokens: 1_200, equivalent: 0.1, unit: 'window_share' }), point(at12 + HOUR, 'overflow', { uncachedTokens: 600, equivalent: 0.05, unit: 'window_share' })];
  const sized = new Set(['overflow']);
  const ended = d.foldBurn(points, 'account', 3_600, sized, at12 + 2 * HOUR, at12 + 2 * HOUR);
  assert.deepEqual(ended.rows.map((r) => r.seconds), [3_600, 3_600]);
  assert.deepEqual(ended, d.foldBurn(points, 'account', 3_600, sized), 'the same figures as a fold with no end');
});

test('a range that ended inside the running hour, read ten minutes later, rates that hour up to the read', () => {
  const at14 = Date.UTC(2026, 8, 16, 14);
  const { rows } = d.foldBurn([point(at14, 'overflow', { uncachedTokens: 1_800 })], 'key', 3_600, NONE, at14 + 20 * 60_000, at14 + 30 * 60_000);
  assert.deepEqual([rows[0].seconds, rows[0].uncached[0]], [1_800, 60], 'the route answered the hour as it stood at the read, 14:00 to 14:30');
});

test('a running bucket sets the peak and the axis only once it covers a tenth of its width', () => {
  const at13 = Date.UTC(2026, 8, 16, 13);
  const points = [point(at13, 'primary', { equivalent: 0.06, planUsd: 0.06, uncachedTokens: 600, unit: 'window_share' }), point(at13 + HOUR, 'primary', { equivalent: 0.02, planUsd: 0.02, uncachedTokens: 100, unit: 'window_share' })];
  const sized = new Set(['primary']);
  const fold = (seconds) => d.foldBurn(points, 'account', 3_600, sized, at13 + HOUR + seconds * 1_000, at13 + HOUR + seconds * 1_000);

  const early = fold(20);
  assert.deepEqual(early.rows.map((r) => [r.seconds, r.partial, r.setsScale]), [[3_600, false, true], [20, true, false]]);
  assert.ok(Math.abs(early.rows[1].equivalent[0] - 3.6) < 1e-12, 'the running bucket keeps its rate: 2% over 20 seconds');
  assert.equal(early.lines[0].peak, early.rows[0].equivalent[0], 'twenty seconds in, the peak is the full hour');
  assert.equal(early.lines[0].planUsdPeak, 0.06, "twenty seconds in, the plan dollars' peak is the full hour's $0.06, never the running $3.60");

  const later = fold(7 * 60);
  assert.deepEqual(later.rows.map((r) => [r.seconds, r.partial, r.setsScale]), [[3_600, false, true], [420, true, true]]);
  assert.equal(later.lines[0].peak, later.rows[1].equivalent[0], 'seven minutes in, the running hour sets the peak');
});

test("a chart's last bucket is partial where the hours a route sums, a fold's range end or the read stop before the bucket's end", () => {
  const hourly = [{ at: 0 }, { at: 3_600_000 }];
  const daily = [{ at: 0 }, { at: 86_400_000 }];
  assert.deepEqual(
    [
      // A past range ending at 1.5 h: the route summed the 1 h bucket up to the hour ceiling, 2 h, its own end.
      d.lastBucketPartial(hourly, 3_600_000, 5_400_000, 86_400_000, true),
      // A past range ending at 25.5 h: the route stopped at the hour ceiling, 26 h, short of the daily bucket's 48 h.
      d.lastBucketPartial(daily, 86_400_000, 91_800_000, 864_000_000, true),
      // A live read at 1.5 h: no row past the read exists, so the running hour holds half its width.
      d.lastBucketPartial(hourly, 3_600_000, 5_400_000, 5_400_000, true),
      // The first range folded from the request log, which the route bounds at the range's end, 1.5 h.
      d.lastBucketPartial(hourly, 3_600_000, 5_400_000, 86_400_000, false),
    ],
    [false, true, true, true],
  );
});

test("a rollup read's window reaches the whole hours a past range touches", () => {
  // Read a day after the range ended, so no instant cuts the hour ceiling of its end.
  const range = { fromMs: Date.UTC(2026, 8, 16, 10, 30), toMs: Date.UTC(2026, 8, 16, 11, 30) };
  assert.deepEqual(d.coveredWindow('rollup', range, Date.UTC(2026, 8, 17, 11, 30), null), { fromMs: Date.UTC(2026, 8, 16, 10), toMs: Date.UTC(2026, 8, 16, 12) });
});

test("a live range's window ends at the read's instant", () => {
  // No row past the read exists yet, so the window stops short of the hour ceiling of 23:00.
  const end = Date.UTC(2026, 8, 16, 22, 31);
  assert.equal(d.coveredWindow('rollup', { fromMs: end - 24 * HOUR, toMs: end }, end, null).toMs, end);
});

test('the first bucket is partial only where the covered start lies inside it', () => {
  // The six-hour buckets of a 7-day range from 18:20 or from 19:20 both start at 18:00, and the whole hours cover
  // them from 18:00 and from 19:00.
  const buckets = [{ at: Date.UTC(2026, 8, 9, 18) }, { at: Date.UTC(2026, 8, 10, 0) }];
  assert.deepEqual([d.firstBucketPartial(buckets, Date.UTC(2026, 8, 9, 18)), d.firstBucketPartial(buckets, Date.UTC(2026, 8, 9, 19))], [false, true]);
});

test('a capped axis top rises to the last of the five ticks recharts would draw over it', () => {
  // The expected tops are recharts-scale's last value of getNiceTickValues([0, top], 5) for each top.
  const tops = [[1_023, 1_200], [1_000, 1_000], [61.5, 80], [9.2, 12], [0.004, 0.004], [0.0013, 0.0014]];
  assert.deepEqual(tops.map(([top]) => d.niceAxisTop(top)), tops.map(([, nice]) => nice));
  assert.equal(d.niceAxisTop(0), 0, 'an empty axis stays empty');
});

test('an account burns in the unit its figures come in, a subscription the cost model does not size whose points carry no figure in none, and every other grouping in dollars', () => {
  const answered = [
    point(0, 'overflow', { unit: 'usd', usd: 1.2 }),
    point(0, 'nightly', { equivalent: 0.001, unit: 'window_share' }),
    point(0, 'tierless', { uncachedTokens: 40, unit: 'window_share' }),
    point(0, 'idle', { equivalent: 0, unit: 'window_share' }),
    point(0, 'departed', { uncachedTokens: 20 }),
  ];
  const { lines } = d.foldBurn(answered, 'account', 3_600, new Set(['idle', 'nightly']));
  const byKey = Object.fromEntries(lines.map((l) => [l.key, l]));
  assert.deepEqual(
    ['overflow', 'nightly', 'tierless', 'idle', 'departed'].map((k) => byKey[k].unit),
    ['usd', 'window_share', 'unknown', 'window_share', 'usd'],
    'a subscription account whose tier the table sizes charts in the plan unit, and one whose tier it does not size and whose points carry no figure is tier unknown',
  );
  assert.deepEqual([byKey.tierless.peak, byKey.tierless.perHour, byKey.tierless.usdPerMinute], [null, null, 0], 'a tier-unknown account has no figure to chart');
  assert.deepEqual([byKey.idle.unit, byKey.idle.peak, byKey.idle.perHour], ['window_share', null, null], 'a sized account with no traffic in range priced none, so it reads a dash');
  assert.ok(d.foldBurn(answered.map((p) => ({ ...p, group: `k-${p.group}` })), 'key', 3_600, NONE).lines.every((l) => l.unit === 'usd'));
});

test('an account the cost model no longer sizes charts as a plan line where its points carry a figure, so its priced buckets stay drawn', () => {
  // A plan-ended account whose profile reads a free plan: the router prices no tier for it now, and the ledger priced
  // the range's buckets from the tier on their rows.
  const answered = [point(0, 'ended', { equivalent: 0.002, uncachedTokens: 40, unit: 'window_share' }), point(HOUR, 'ended', { uncachedTokens: 40, unit: 'window_share' })];
  const [line] = d.foldBurn(answered, 'account', 3_600, NONE).lines;
  assert.deepEqual([line.unit, line.perHour], ['window_share', 0.001], 'the priced bucket and the idle one');
});

test('the burn fold keeps the groups that burn the most tokens and sums the rest as other', () => {
  const answered = Array.from({ length: 9 }, (_, i) => point(0, `s${i}`, { cachedTokens: (i + 1) * 60, usd: 0.6 }));
  const { lines, rows } = d.foldBurn(answered, 'session', 60, NONE);
  assert.deepEqual(lines.map((l) => l.key), ['s8', 's7', 's6', 's5', 's4', 's3', 's2', 'other']);
  assert.equal(lines.at(-1).label, 'Other');
  assert.deepEqual([rows[0].cached.at(-1), rows[0].usd.at(-1)], [60 + 120, 1.2], 'the two smallest sessions, summed');
  assert.equal(rows[0].equivalent.at(-1), null, 'a folded rest draws no plan figure');
  assert.deepEqual(d.foldBurn([point(0, '-', { cachedTokens: 60 })], 'key', 60, NONE).lines.map((l) => l.label), ['unattributed']);
});

test('a burn line under the classifier grouping names the reserved id none, as the breakdown labels it, and unattributed under any other', () => {
  const reserved = [point(0, 'client', { cachedTokens: 120 }), point(0, '-', { cachedTokens: 60 })];
  assert.deepEqual(d.foldBurn(reserved, 'classifier', 60, NONE).lines.map((l) => [l.key, l.label]), [['client', 'client'], ['-', 'none']]);
  for (const groupBy of ['account', 'client', 'key', 'pool', 'user_agent']) assert.deepEqual(d.foldBurn(reserved, groupBy, 60, NONE).lines.map((l) => l.label), ['client', 'unattributed'], groupBy);
});

test('under the account grouping the rest folds into one other per unit, so no dollars hide behind a window share', () => {
  // Seven accounts burn the most; the three smallest are a priced subscription, a tier-unknown one and an
  // API key.
  const kept = Array.from({ length: 7 }, (_, i) => point(0, `u${i}`, { cachedTokens: 1_000 * (i + 2), equivalent: 0.001, unit: 'window_share' }));
  const answered = [
    ...kept,
    point(0, 'small-plan', { cachedTokens: 120, equivalent: 0.002, unit: 'window_share' }),
    point(0, 'small-tierless', { cachedTokens: 90, unit: 'window_share' }),
    point(0, 'small-key', { cachedTokens: 60, unit: 'usd', usd: 0.6 }),
  ];
  const sized = new Set([...kept.map((p) => p.group), 'small-plan']);
  const { lines, rows } = d.foldBurn(answered, 'account', 60, sized);
  const rest = lines.slice(7);
  assert.deepEqual(rest.map((l) => [l.label, l.unit, l.other]), [['Other (plan)', 'window_share', true], ['Other (tier unknown)', 'unknown', true], ['Other (dollars)', 'usd', true]]);
  assert.equal(new Set(lines.map((l) => l.key)).size, lines.length, 'each line keeps a key of its own');
  assert.deepEqual([rows[0].cached[7], rows[0].cached[8], rows[0].cached[9]], [120, 90, 60], 'each rest carries its own unit\'s accounts');
  assert.deepEqual([rows[0].usd[7], rows[0].usd[8], rows[0].usd[9]], [0, 0, 0.6], 'the dollar side keeps the API-key account\'s dollars');
  assert.equal(rows[0].equivalent[7], null, 'a folded rest draws no figure');
  const oneUnit = d.foldBurn([...kept, point(0, 'small-key', { cachedTokens: 60, unit: 'usd', usd: 0.6 }), point(0, 'tiny-key', { cachedTokens: 30, unit: 'usd', usd: 0.3 })], 'account', 60, sized);
  assert.deepEqual(oneUnit.lines.slice(7).map((l) => l.label), ['Other (dollars)'], 'a rest of one unit folds into that unit alone');
});

test('a line that priced no bucket keeps its idle buckets null, so its legend reads a dash', () => {
  const plan = (at, over) => point(at * HOUR, 'primary', { unit: 'window_share', ...over });
  const { lines, rows } = d.foldBurn([plan(0, { tokens: 100, unpriced: true }), plan(1, { tokens: 0 })], 'account', 3_600, new Set(['primary']));
  assert.deepEqual(rows.map((r) => r.equivalent[0]), [null, null], 'a flagged bucket and an idle one');
  assert.deepEqual([lines[0].perHour, lines[0].unpriced], [null, true]);
});

test('a bucket whose rows carried no token is idle whatever figure its rows answered', () => {
  // A failed request books a row with no token, which a listed model prices at zero.
  const plan = (at, over) => point(at * HOUR, 'primary', { unit: 'window_share', ...over });
  const { rows } = d.foldBurn([plan(0, { equivalent: 0.08, tokens: 100 }), plan(1, { equivalent: 0, tokens: 0 })], 'account', 3_600, new Set(['primary']));
  assert.deepEqual(rows.map((r) => r.equivalent[0]), [0.08, 0]);
  const failed = d.foldBurn([plan(0, { equivalent: 0, tokens: 0 })], 'account', 3_600, new Set(['primary']));
  assert.deepEqual([failed.rows[0].equivalent[0], failed.lines[0].perHour], [null, null], 'no bucket priced traffic, so its legend reads a dash rather than 0%');
});

test('a bucket with cache writes and output alone is not idle', () => {
  const plan = (at, over) => point(at * HOUR, 'primary', { unit: 'window_share', ...over });
  const { lines, rows } = d.foldBurn([plan(0, { cachedTokens: 0, equivalent: 0.02, tokens: 50, uncachedTokens: 0 })], 'account', 3_600, new Set(['primary']));
  assert.deepEqual([rows[0].equivalent[0], lines[0].perHour], [0.02, 0.02], 'its traffic priced rather than absent');
});

test('a sized account whose points carry no figure keeps its plan line', () => {
  // An Ultra account whose range ran throughout on a model the cost model has no weight for.
  const answered = [point(0, 'a', { tokens: 300, uncachedTokens: 300, unit: 'window_share', unpriced: true }), point(HOUR, 'a', { tokens: 200, uncachedTokens: 200, unit: 'window_share', unpriced: true })];
  assert.equal(d.foldBurn(answered, 'account', 3_600, new Set(['a'])).lines[0].unit, 'window_share');
  assert.equal(d.foldBurn(answered, 'account', 3_600, NONE).lines[0].unit, 'unknown', 'the same traffic on an account whose tier the cost model does not size');
});

test('a null slot is unpriced only with tokens behind it, so an idle slot of a line that priced nothing is not', () => {
  const plan = (at, over) => point(at * HOUR, 'primary', { unit: 'window_share', ...over });
  const { rows } = d.foldBurn([plan(0, { uncachedTokens: 10, unpriced: true }), plan(1, {})], 'account', 3_600, new Set(['primary']));
  const [busy, idle] = rows;
  assert.equal(d.slotUnpriced(busy.equivalent[0], busy.tokens[0]), true, 'traffic the cost model could not price');
  assert.equal(d.slotUnpriced(idle.equivalent[0], idle.tokens[0]), false, 'an idle slot has no traffic to list as model unlisted');
});

test("a bucket's share is null when any row is unpriced, in either order", () => {
  assert.equal(d.bucketShare([0.1, null]), null);
  assert.equal(d.bucketShare([null, 0.1]), null);
  assert.ok(Math.abs(d.bucketShare([0.1, 0.2]) - 0.3) < 1e-12);
  assert.equal(d.bucketShare([]), null, 'a bucket with no row has no share');
});

test("the mock's burn fold sums the rows' own shares, prices only rows with tokens and nulls a window on any unpriced row, in either order", () => {
  // The shipped model the mock prices through, so a row's shares are the ones the share rule gives for it.
  const costs = d.decodeSettings(fixture('settings')).subscriptionCosts;
  // The hour before the peak band opens in Los Angeles on Wednesday 2026-09-16. Primary is the story's Ultra account.
  const edge = d.bandNow(costs, Date.parse('2026-09-16T10:00:00Z')).at;
  const hour = edge - HOUR;
  const tier = 'default_claude_max_20x';
  const usageOf = (input) => ({ cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: 0, input, output: 0 });
  // A row carries its requests' shares, each priced at its completion, which can land past the hour it arrived in.
  const sharesAt = (model, input, at) => Object.fromEntries(['five_hour', 'seven_day'].map((w) => [w, d.shareIn(costs, w, tier, model, usageOf(input), d.inBand(costs, at))]));
  const row = (model, uncachedInput, completedAt = hour) => ({ accountId: 'primary', costUsd: 0, equivalent: d.equivalentIn(costs, model, usageOf(uncachedInput), d.inBand(costs, completedAt)), funding: 'plan', hour: new Date(hour).toISOString(), keyId: 'desktop', model, requestClass: 'main', sessionId: 's', shares: sharesAt(model, uncachedInput, completedAt), tokens: { cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: 0, output: 0, uncachedInput } });
  const range = { from: new Date(hour).toISOString(), to: new Date(hour + HOUR).toISOString() };
  const shareOf = (rows) => d.burnOf(rows, range, 'account').points.find((p) => p.group === 'primary' && Date.parse(p.at) === hour).fiveHourShare;
  // A request that arrived before the band opened and completed inside it, so the band's factor prices it.
  const priced = row('claude-opus-5-5', 1_000_000, edge + 60_000);
  const want = priced.shares.five_hour;
  const atTheHour = d.shareIn(costs, 'five_hour', tier, 'claude-opus-5-5', usageOf(1_000_000), d.inBand(costs, hour));
  assert.ok(want !== null && atTheHour !== null && want > atTheHour, 'the shipped model prices Opus 5.5 on Ultra, higher inside the band');
  // A failed request on Sonnet 4.5, which the cost model has no weight for, carries no token.
  const failed = row('claude-sonnet-4-5', 0);
  for (const rows of [[priced, failed], [failed, priced]]) assert.ok(Math.abs(shareOf(rows) - want) < 1e-12, 'a row with no token leaves the priced share, the row\'s own and not its tokens\' at the hour');
  assert.equal(shareOf([failed, row('claude-opus-5-5', 0)]), null, 'rows with no token alone are an idle bucket, not a share of zero');
  // An id whose segment names no model tier, which no fallback prices.
  const unpriced = row('claude-unknown-1', 1_000);
  for (const rows of [[priced, unpriced], [unpriced, priced]]) assert.equal(shareOf(rows), null, 'a row the cost model cannot price nulls the window');
});

test('an hour of two requests in different bands sums the per-request shares at their completions', async () => {
  const api = new d.MockApi();
  await api.login('admin');
  const costs = (await api.getSettings()).subscriptionCosts;
  const tierOf = new Map(d.ACCOUNTS.map((a) => [a.id, a.subscription?.rateLimitTier]));
  // A plan-funded inference the cost model prices, as the template the two rows are written from, on an account whose
  // set holds the 5-hour window and under a client whose tables shrink that window inside the band, so the two bands
  // price it apart.
  const fiveHour = new Set(d.ACCOUNTS.filter((a) => a.windows.some((w) => w.id === '5h')).map((a) => a.id));
  const template = [...d.requestById.values()].find((r) => r.kind === 'inference' && r.status === 200 && r.attempts.length === 1 && tierOf.get(r.accountId ?? '') !== undefined && fiveHour.has(r.accountId) && d.fundingOf(r.attempts[0]) === 'plan' && d.weightedWindows(costs, r.canonicalModel).length > 0 && d.tablesFor(r.client).peak.capacityFactor.fiveHour < 1);
  assert.ok(template, 'the story holds a priced plan-funded answer');
  const tier = tierOf.get(template.accountId);
  // The band's next edge after the early hours of a Wednesday, read off the table the mock prices with.
  const edge = d.bandNow(costs, Date.parse('2026-09-16T08:00:00Z')).at;
  assert.ok(edge !== null, 'the band flips within the week');
  const usage = (input, output) => ({ ...template.usage, cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: 0, input, output });
  const rowAt = (at, durationMs, u, id) => ({ ...template, at: new Date(at).toISOString(), durationMs, id, usage: u });
  // One arrives a minute before the edge and completes a minute past it, and the other lies wholly before the edge
  // inside the same hour, so the hour holds both and only the first completes on the far side.
  const crossing = rowAt(edge - 60_000, 120_000, usage(400_000, 20_000), 'req_crossing');
  const before = rowAt(edge - 20 * 60_000, 60_000, usage(250_000, 9_000), 'req_before');
  const hours = d.rollups([crossing, before]);
  assert.equal(hours.length, 1, 'the two requests share one hour of one lane');
  const share = (r, at) => d.shareIn(costs, 'five_hour', tier, r.canonicalModel, r.usage, d.inBand(costs, at));
  const completion = (r) => Date.parse(r.at) + r.durationMs;
  const got = hours[0].shares.five_hour;
  const want = share(crossing, completion(crossing)) + share(before, completion(before));
  assert.ok(Math.abs(got - want) < 1e-12, `the hour reads ${got} against the requests' own ${want}`);
  const atArrivals = share(crossing, Date.parse(crossing.at)) + share(before, Date.parse(before.at));
  assert.ok(Math.abs(got - atArrivals) > 1e-9, 'a request priced at its arrival reads the other band');
  const summed = usage(650_000, 29_000);
  for (const at of [completion(crossing), completion(before)]) {
    assert.ok(Math.abs(got - d.shareIn(costs, 'five_hour', tier, template.canonicalModel, summed, d.inBand(costs, at))) > 1e-9, 'the summed tokens priced at one instant read one band for both');
  }
});

test('a rollup counts an undecodable stream inside the upstream errors it is part of, so the error split never reads negative', () => {
  const template = [...d.requestById.values()].find((r) => r.kind === 'inference' && r.status === 200 && r.attempts.length === 1);
  assert.ok(template, 'the story holds a single-attempt answer');
  const rowOf = (status, id) => ({ ...template, errorCode: 'stream_undecodable', id, outcome: status === 200 ? 'succeeded' : 'failed', status });
  // The relay's 502 is the undecodable stream the ledger files. The same code under a 200 is not an upstream error.
  const hours = d.rollups([rowOf(502, 'req_cut'), rowOf(200, 'req_whole')]);
  const total = (read) => hours.reduce((n, h) => n + read(h.requests), 0);
  assert.deepEqual([total((r) => r.undecodable), total((r) => r.upstream_error)], [1, 1], 'the 502 alone counts, once under each');
  for (const h of hours) assert.ok(h.requests.undecodable <= h.requests.upstream_error, `${h.hour} reads ${h.requests.undecodable} undecodable of ${h.requests.upstream_error} upstream errors`);
});

// The plan word each tier resolves to on its own, so an account reads the plan the router would resolve for it and the
// gate prices it by its tier. A case that needs another word passes it.
const PLAN_OF_TIER = { claude_max_20x: 'max_20x', claude_team: 'team', default_claude_ai: 'pro', default_claude_max_20x: 'max_20x', default_claude_max_5x: 'max_5x', default_claude_zero: 'usage_based' };
const sizedAccount = (id, rateLimitTier, plan = PLAN_OF_TIER[rateLimitTier] ?? null) => ({ id, kind: rateLimitTier === null ? 'api_key' : 'oauth', subscription: rateLimitTier === null ? null : { plan, rateLimitTier } });
const SIZED_ACCOUNTS = [sizedAccount('stored', 'default_claude_max_20x'), sizedAccount('bare', 'claude_max_20x'), sizedAccount('team', 'claude_team'), sizedAccount('key', null)];
const SIZED_COSTS = { ...EMPTY, capacity: { default_claude_max_20x: { fiveHour: 1, sevenDay: 1 } } };
// Pro listed with a 7-day capacity alone, Max 5x with a 5-hour capacity alone.
const ONE_WINDOW = { ...SIZED_COSTS, capacity: { ...SIZED_COSTS.capacity, default_claude_ai: { fiveHour: null, sevenDay: 1 }, default_claude_max_5x: { fiveHour: 1, sevenDay: null } } };
const ONE_WINDOW_ACCOUNTS = [...SIZED_ACCOUNTS, sizedAccount('pro', 'default_claude_ai'), sizedAccount('max5', 'default_claude_max_5x')];

test('an account is sized for the plan chart when the router prices its stored tier and the cost model lists it, whatever windows it has a capacity on', () => {
  assert.deepEqual([...d.sizedAccounts(SIZED_ACCOUNTS, SIZED_COSTS)].sort(), ['bare', 'stored']);
  assert.deepEqual(
    [...d.sizedAccounts(ONE_WINDOW_ACCOUNTS, ONE_WINDOW)].sort(),
    ['bare', 'max5', 'pro', 'stored'],
    'a listed tier with no capacity on a window draws that window as model unlisted, never as tier unknown',
  );
  // Each tier is one the cost model lists, so the plan word alone leaves a free plan and a usage-based seat out and
  // keeps a Team seat.
  const gated = [sizedAccount('free', 'default_claude_ai', 'free'), sizedAccount('seat', 'default_claude_max_5x', 'usage_based'), sizedAccount('teamseat', 'default_claude_ai', 'team')];
  assert.deepEqual([...d.sizedAccounts([...ONE_WINDOW_ACCOUNTS, ...gated], ONE_WINDOW)].sort(), ['bare', 'max5', 'pro', 'stored', 'teamseat']);
});

test('an account is sized on a window only when its listed tier has a capacity there, as the shed preview reads the 5-hour one', () => {
  assert.deepEqual([...d.sizedAccounts(SIZED_ACCOUNTS, SIZED_COSTS, 'five_hour')].sort(), ['bare', 'stored']);
  assert.deepEqual(
    [...d.sizedAccounts(ONE_WINDOW_ACCOUNTS, ONE_WINDOW, 'five_hour')].sort(),
    ['bare', 'max5', 'stored'],
    'the sessions route reads no burn for a pin of a tier with no 5-hour capacity, so the preview leaves it out',
  );
  assert.deepEqual([...d.sizedAccounts(ONE_WINDOW_ACCOUNTS, ONE_WINDOW, 'seven_day')].sort(), ['bare', 'pro', 'stored'], 'the window named is the one read');
});

test('a burn read where the shares do not answer reads no accounts, so a failed account read leaves it whole, and the filters travel with it', async () => {
  const sent = [];
  const source = {
    getAnalyticsBurn: async (...args) => {
      sent.push(args);
      return { bucketSeconds: 3_600, points: [] };
    },
    listAccounts: async () => {
      throw new Error('the accounts read failed');
    },
  };
  const range = { from: new Date(0).toISOString(), to: new Date(HOUR).toISOString() };
  const two = { accountIds: ['primary', 'secondary'], userAgentGroups: undefined };
  for (const group of ['key', 'model', 'request_class', 'session']) {
    assert.deepEqual(await d.readBurn(source, range, group, two), { accounts: [], answer: { bucketSeconds: 3_600, points: [] } }, group);
  }
  assert.deepEqual(sent[0], [range, 'key', two], 'the Accounts filter and the user agent groups travel with the grouping');
  await assert.rejects(d.readBurn(source, range, 'account'), /the accounts read failed/, 'the account grouping sizes its lines by tier and needs the read');
  await assert.rejects(d.readBurn(source, range, 'model', { accountIds: ['primary'] }), /the accounts read failed/, 'so does a filter naming one account, whose shares answer under any grouping');
});

test('the plan card reads the Pro-week figure under any grouping', () => {
  const p = (at, group, over) => point(at * HOUR, group, { equivalent: 0.004, tokens: 100, uncachedTokens: 100, ...over });
  const { lines, rows } = d.foldBurn([p(0, 'desktop'), p(1, 'desktop'), p(0, 'laptop')], 'key', 3_600, NONE);
  const [desktop, laptop] = ['desktop', 'laptop'].map((k) => lines.findIndex((l) => l.key === k));
  assert.deepEqual(rows.map((r) => r.equivalent[desktop]), [0.004, 0.004], 'the figure per hour, which no account tier reads');
  assert.equal(lines[desktop].perHour, 0.004);
  assert.deepEqual([rows[1].equivalent[laptop], lines[laptop].unpriced], [0, false], 'an idle bucket reads zero once the line priced the figure');
  const unpriced = d.foldBurn([p(0, 'desktop', { equivalent: null, unpriced: true })], 'key', 3_600, NONE);
  assert.equal(unpriced.lines[0].unpriced, true, 'traffic on a model without a weight prices no figure, flagged, and lists as model unlisted');
});

test('a filter naming one account sizes every group by that account, so its figure charts as a plan line under any grouping', () => {
  // The points carry no figure, so the sized set alone tells the plan line from tier unknown.
  const p = (group) => point(0, group, { tokens: 100, uncachedTokens: 100, unit: 'window_share', unpriced: true });
  const answered = [p('claude-opus-5-5'), p('claude-sonnet-4-6')];
  const fold = (sized, account) => d.foldBurn(answered, 'model', 3_600, sized, undefined, undefined, { account }).lines.map((l) => l.unit);
  assert.deepEqual(fold(new Set(['primary']), 'primary'), ['window_share', 'window_share'], "the named account's tier sizes each model's line");
  assert.deepEqual(fold(NONE, 'primary'), ['unknown', 'unknown'], 'an account whose tier the cost model does not size reads tier unknown');
  assert.deepEqual(fold(new Set(['primary']), null), ['usd', 'usd'], 'with no account named the grouping mixes accounts');
});

test('under the request class grouping a filter naming one account sizes both class lines by that account', () => {
  const p = (group) => point(0, group, { tokens: 100, uncachedTokens: 100, unit: 'window_share', unpriced: true });
  const answered = [p('main'), p('subagent'), p('auxiliary')];
  const fold = (sized) => d.foldBurn(answered, 'request_class', 3_600, sized, undefined, undefined, { account: 'primary' }).lines.map((l) => [l.key, l.unit]);
  assert.deepEqual(fold(new Set(['primary'])), [['conversation', 'window_share'], ['overhead', 'window_share']], "the named account's tier sizes the two folded lines");
  assert.deepEqual(fold(NONE), [['conversation', 'unknown'], ['overhead', 'unknown']], 'an account whose tier the cost model does not size reads tier unknown on both');
});

test('the key grouping names an earlier holder of a key id as the breakdown does', () => {
  const generation = 1_791_720_940_000;
  const { lines } = d.foldBurn([point(0, 'desktop', { uncachedTokens: 60 }), point(0, `desktop@${generation}`, { uncachedTokens: 30 })], 'key', 60, NONE);
  assert.deepEqual(lines.map((l) => l.label), ['desktop', d.earlierHolderLabel('desktop', generation)]);
});

test("a deleted key's group reads its creation minute in the viewer's zone, and its wire label without the instant", () => {
  const created = 1_791_720_940_000;
  const group = { key: `desktop@${created}`, key_id: 'desktop', label: 'desktop (deleted)' };
  assert.equal(d.decodeAnalyticsGroup({ ...group, key_created_at: created }).label, `desktop (deleted, created ${d.fmtMinute(new Date(created).toISOString())})`, "the instant formats in the host's zone");
  assert.equal(d.decodeAnalyticsGroup(group).label, 'desktop (deleted)', 'a group with no instant reads its label alone');
});

test('a priced bucket between unpriced ones, or at an end beside one, is an isolated slot the plan chart marks', () => {
  const plan = (at, over) => point(at * HOUR, 'primary', { tokens: 100, uncachedTokens: 100, unit: 'window_share', unpriced: true, ...over });
  const { rows } = d.foldBurn([plan(0), plan(1, { equivalent: 0.05, unpriced: false }), plan(2)], 'account', 3_600, new Set(['primary']));
  const figures = rows.map((r) => r.equivalent[0]);
  assert.deepEqual(figures, [null, 0.05, null], 'a priced hour between two whose traffic went unpriced');
  assert.deepEqual(d.isolatedSlots(figures), [false, true, false], 'the line draws no segment through it, so it takes a dot');
  assert.deepEqual(d.isolatedSlots([0.02, null, 0.03, 0]), [true, false, false, false], "an end beside a null is isolated; a slot with a priced neighbor is part of a segment");
  assert.deepEqual(d.isolatedSlots([0]), [true], 'a lone bucket draws no segment either');
});

test('every burn read reads the accounts again, so a tier a profile refresh supplied sizes the account on the next range', async () => {
  const costs = { ...EMPTY, capacity: { default_claude_max_20x: { fiveHour: 1, sevenDay: 1 } } };
  const reads = [null, 'default_claude_max_20x'];
  const source = {
    getAnalyticsBurn: async () => ({ bucketSeconds: 3_600, points: [] }),
    listAccounts: async () => {
      const rateLimitTier = reads.shift();
      return [{ id: 'primary', kind: 'oauth', subscription: rateLimitTier === null ? null : { plan: 'max_20x', rateLimitTier } }];
    },
  };
  const range = { from: new Date(0).toISOString(), to: new Date(HOUR).toISOString() };
  const before = await d.readBurn(source, range, 'account');
  const after = await d.readBurn(source, range, 'account');
  assert.deepEqual([d.sizedAccounts(before.accounts, costs).has('primary'), d.sizedAccounts(after.accounts, costs).has('primary')], [false, true]);
  assert.deepEqual(after.answer, { bucketSeconds: 3_600, points: [] });
});

test("the placements table's hit share formats", () => {
  const row = { cacheReadTokens: 184_000, hitShare: 2 / 3, hits: 2, placements: 3, promisedTokens: 190_500, rung: 'content' };
  assert.deepEqual(d.placementCells(row), ['3', '2', '67%', '184K', '190.5K'], 'placements, hits, hit share, tokens read, tokens promised');
  assert.equal(d.placementCells({ ...row, hitShare: 1, hits: 3 })[2], '100%');
  assert.equal(d.placementCells({ ...row, hitShare: 0, hits: 0 })[2], '0%', 'a rung whose every placement missed reads zero');
  assert.equal(d.placementCells({ ...row, hitShare: null, hits: 0, placements: 0 })[2], '-', 'a rung with no placement has no share to read');
  assert.equal(d.placementCells({ ...row, promisedTokens: 0, rung: 'with_main' })[4], '-', 'a placement beside the main conversation promises no size');
  assert.deepEqual(['content', 'static', 'with_main', 'by_moonlight'].map(d.placementRungLabel), ['by its messages', 'by its static prefix', 'with the main conversation', 'by_moonlight']);
});

test("the mock files its hourly rows under the speed each hour's answers reported, one key's Opus hours fast", () => {
  const groups = d.breakdownOf(d.ALL_ROLLUPS, { from: d.ALL_ROLLUPS[0].hour, groupBy: 'speed', to: new Date(Date.parse(d.ALL_ROLLUPS.at(-1).hour) + HOUR).toISOString() });
  assert.deepEqual(groups.map((g) => g.key), ['fast', 'standard']);
  const fast = d.ALL_ROLLUPS.filter((r) => r.speed === 'fast');
  assert.ok(fast.length > 0 && fast.every((r) => r.keyId === 'desktop' && r.model.includes('opus') && r.kind === 'inference'));
  const served = (rows) => rows.filter((r) => r.kind !== 'probe').reduce((n, r) => n + r.requests.incomplete + r.requests.ok + r.requests.rate_limited + r.requests.overloaded + r.requests.upstream_error + r.requests.client_gone + r.requests.router_shutdown, 0);
  assert.deepEqual(groups.map((g) => g.requests), [served(fast), served(d.ALL_ROLLUPS.filter((r) => r.speed === 'standard'))]);
});

test('the probe totals sum each group\'s held probes, and the mock holds a few on its probe rows', () => {
  const row = fixture('analytics_breakdown').rows[0];
  const group = (held) => d.decodeAnalyticsGroup({ ...row, probes: { ...row.probes, held } });
  assert.equal(d.sumGroups([group(2), group(3)]).probes.held, 5, 'the chart and the tiles draw the sum');
  const probeRows = d.ALL_ROLLUPS.filter((r) => r.kind === 'probe');
  const held = probeRows.reduce((n, r) => n + r.probes.held, 0);
  assert.ok(held > 0 && held < probeRows.reduce((n, r) => n + r.probes.ok, 0) / 20, `a small held count: ${held}`);
  // The default range's first hour is the dashboard's, 23 hours before the current one, which is where the pick draws.
  const lastHour = Date.parse(d.ALL_ROLLUPS.at(-1).hour);
  const lastDay = probeRows.filter((r) => Date.parse(r.hour) >= lastHour - 23 * HOUR);
  assert.equal(lastDay.reduce((n, r) => n + r.probes.held, 0), 1, 'the last day holds one, so the default range draws the slice');
  const [all] = d.breakdownOf(d.ALL_ROLLUPS, { from: d.ALL_ROLLUPS[0].hour, groupBy: 'all', to: new Date(Date.parse(d.ALL_ROLLUPS.at(-1).hour) + HOUR).toISOString() });
  assert.equal(all.probes.held, held, 'the mock route sums the probe rows it groups');
});

test("the mock's client classifier group reads zero cache, saving and split figures over hours that read, wrote and kept warm", () => {
  // A plan-funded hour on the story's Ultra account lends the counters no share reads, and the case writes every
  // member the share rule does read.
  const template = d.ALL_ROLLUPS.find((r) => r.accountId === 'primary' && r.funding === 'plan' && r.kind === 'inference' && r.classifier === '-');
  assert.ok(template, 'the story holds a plan-funded hour on primary');
  const tokens = { ...template.tokens, cacheRead: 2_000_000, cacheWrite1h: 300_000, cacheWrite5m: 200_000, output: 40_000, uncachedInput: 50_000 };
  const row = (classifier, sessionId) => ({ ...template, classifier, equivalent: 0, keptWarm: { requests: 3, tokens1h: 400_000, tokens5m: 100_000 }, model: 'claude-sonnet-5-5', sessionId, shares: { five_hour: 0, seven_day: 0 }, tokens });
  const groups = d.breakdownOf([row('client', 's-free'), row('client', 's-free-again'), row('-', 's-plain')], { accountIds: ['primary'], from: template.hour, groupBy: 'classifier', includeProbes: false, to: new Date(Date.parse(template.hour) + HOUR).toISOString() });
  const figures = (g) => ({
    cache: [g.cacheEquivalent?.read, g.cacheEquivalent?.write],
    saving: [g.savingEquivalent],
    split: Object.values(g.equivalentSplit ?? {}),
  });
  const free = figures(groups.find((g) => g.key === 'client'));
  const plain = figures(groups.find((g) => g.key === '-'));
  // The same counts without the marker price above zero, so each zero below is the free rule's.
  for (const [family, values] of Object.entries(plain)) assert.ok(values.length > 0 && values.every((v) => v > 0), `the plain hour's ${family} figures: ${values}`);
  for (const [family, values] of Object.entries(free)) assert.ok(values.length > 0 && values.every((v) => v === 0), `the client group's ${family} figures: ${values}`);
});

test("the mock prices a fast hour at the model's fast cells", async () => {
  // The mock draws its story from the clock it loads at and its seeded stream, so the case loads a copy of the bundle
  // at one fixed instant, and the rollups read the same figures on every run in one time zone.
  const at = Date.UTC(2026, 8, 16, 14, 30);
  const wall = Date.now;
  Date.now = () => at;
  let pinned;
  try {
    pinned = await import(`./dist/api.js?fast-cells=${at}`);
  } finally {
    Date.now = wall;
  }
  const api = new pinned.MockApi();
  const cells = new Map((await api.getSettings()).apiPrices.models.map((p) => [p.model, p.fast]));
  const priced = (fast, u) => (u.input * fast.inputPerM + u.cacheRead * fast.cacheReadPerM + u.cacheWrite5m * fast.cacheWrite5mPerM + u.cacheWrite1h * fast.cacheWrite1hPerM + u.output * fast.outputPerM) / 1_000_000;
  // A synthesized hour prices its tokens whole; one folded from the log sums its requests, each rounded to 1e-4.
  const hours = pinned.ALL_ROLLUPS.filter((r) => r.speed === 'fast');
  assert.ok(hours.length > 0);
  for (const r of hours) {
    const want = priced(cells.get(r.model), { ...r.tokens, input: r.tokens.uncachedInput });
    assert.ok(Math.abs(r.costUsd - want) <= 0.001 + want * 0.001, `${r.hour} ${r.model}: ${r.costUsd} against ${want}`);
  }
});

test('the session fold files a request under the speed its serving attempt reported, else fast when it asked for fast, standard otherwise', () => {
  const rows = d.decodeRequestPage(fixture('requests')).requests;
  const served = (r) => r.attempts.at(-1)?.response.speed;
  // Of the fixture's 19 requests the failover's serving attempt answered fast and 16 others standard. The route flags
  // the blocked and the contract request as refused before any account, which the fold counts under no speed.
  assert.deepEqual(d.foldGroups(rows, 'speed').map((g) => [g.key, g.requests]).sort(), [['fast', 1], ['standard', 16]]);
  const named = rows.find((r) => served(r) === 'fast');
  const unnamed = { ...named, attempts: named.attempts.map((a) => ({ ...a, response: { ...a.response, speed: null } })) };
  assert.deepEqual(d.foldGroups([unnamed], 'speed').map((g) => g.key), ['standard'], 'an answer that named no speed files as standard, as the rollups file it');
  // The fixture's fast request failed over: its first attempt was standard and its serving one fast.
  assert.equal(named.attempts[0].response.speed, 'standard');
  const refused = { ...named, attempts: [], shape: { ...named.shape, requestedSpeed: 'fast' } };
  assert.deepEqual(d.foldGroups([refused], 'speed').map((g) => g.key), ['fast'], 'a fast request refused before any attempt files under the speed it asked for');
  const asked = { ...refused, shape: { ...refused.shape, requestedSpeed: 's'.repeat(300) } };
  assert.deepEqual(d.foldGroups([asked], 'speed').map((g) => g.key), ['standard'], 'any other text the caller sent as its speed files under standard');
});

test('the session fold groups a user agent under its first product token, as the rollups key it', () => {
  const [captured, failed] = d.decodeRequestPage(fixture('requests')).requests;
  assert.equal(captured.userAgent, 'claude-cli/2.1.310 (external, cli)');
  const rows = [captured, { ...failed, userAgent: 'claude-cli/2.1.310 (external, sdk-cli)' }, { ...failed, id: 'r-curl', userAgent: 'curl' }, { ...failed, id: 'r-none', userAgent: null }];
  assert.deepEqual(d.foldGroups(rows, 'user_agent').map((g) => [g.key, g.requests]).sort(), [['-', 1], ['claude-cli/2.1.310', 2], ['other', 1]], 'a header without a product token files under other, and one never sent under the reserved id');
  const named = d.foldGroups(rows, 'user_agent', { agentGroups: ['claude-cli/2.1.310'] });
  assert.deepEqual(named.map((g) => [g.key, g.requests]), [['claude-cli/2.1.310', 2], ['other', 2]], 'a pattern list names its groups by the token, as the breakdown route matches them');
});

test('the refusal count narrows by the client and user agent patterns, the agent folded to its product token and a request that named neither matched as -', () => {
  const [row] = d.decodeRequestPage(fixture('requests')).requests;
  const served = row.attempts.at(-1);
  // Each row refused under a category of its own, so the categories read back name the rows the count kept.
  const refused = (category, client, userAgent) => ({ ...row, attempts: [{ ...served, response: { ...served.response, refusalCategory: category, stopReason: 'refusal' } }], client, id: `r-${category}`, kind: 'inference', userAgent });
  const rows = [refused('cli-new', 'claude-code/cli', 'claude-cli/2.1.3 (external, cli)'), refused('cli-old', 'claude-code/cli', 'claude-cli/2.0.9 (external, cli)'), refused('sdk-new', 'claude-code/sdk-cli', 'claude-cli/2.1.3 (external, sdk-cli)'), refused('unnamed', null, null)];
  const at = Date.parse(row.at);
  const kept = (lists) =>
    d
      .refusalCategoriesOf(rows, { from: new Date(at - HOUR).toISOString(), groupBy: 'all', to: new Date(at + HOUR).toISOString(), ...lists })
      .rows.map((r) => r.category)
      .sort();
  assert.deepEqual(kept({}), ['cli-new', 'cli-old', 'sdk-new', 'unnamed'], 'no pattern keeps every refusal');
  assert.deepEqual(kept({ clients: ['claude-code/cli'] }), ['cli-new', 'cli-old']);
  assert.deepEqual(kept({ userAgents: ['claude-cli/2.1.*'] }), ['cli-new', 'sdk-new']);
  assert.deepEqual(kept({ userAgents: ['claude-cli/2.1.3'] }), ['cli-new', 'sdk-new'], 'the token names one build under both client labels');
  assert.deepEqual(kept({ userAgents: ['claude-cli/2.1.3 (external, cli)'] }), [], 'the whole header matches no folded token');
  assert.deepEqual(kept({ clients: ['claude-code/cli'], userAgents: ['claude-cli/2.1.*'] }), ['cli-new'], 'the two lists narrow together');
  assert.deepEqual(kept({ clients: ['no-such-client'] }), []);
  assert.deepEqual(kept({ userAgents: ['no-such-agent/*'] }), []);
  for (const pattern of ['*', '-']) {
    assert.ok(kept({ clients: [pattern] }).includes('unnamed'), `the request with no client counts under the client pattern ${pattern}`);
    assert.ok(kept({ userAgents: [pattern] }).includes('unnamed'), `the request with no user agent counts under the user agent pattern ${pattern}`);
  }
  assert.deepEqual(kept({ clients: ['-'], userAgents: ['-'] }), ['unnamed'], 'the dash names the request that named neither, and no other');
});

test('the session fold keys an effort and a request class as the rollups fold them', () => {
  const [row] = d.decodeRequestPage(fixture('requests')).requests;
  const odd = { ...row, id: 'r-odd', requestClass: 'c'.repeat(65), shape: { ...row.shape, effort: 'ultra' } };
  const unset = { ...row, id: 'r-unset', requestClass: null, shape: { ...row.shape, effort: null } };
  assert.deepEqual(d.foldGroups([odd], 'effort').map((g) => g.key), [d.OTHER_DIMENSION], 'an effort the API does not name');
  assert.deepEqual(d.foldGroups([unset], 'effort').map((g) => g.key), [d.NO_EFFORT], 'no effort');
  assert.deepEqual(d.foldGroups([odd], 'request_class').map((g) => g.key), [d.OTHER_DIMENSION], 'a class past 64 bytes');
  assert.deepEqual(d.foldGroups([unset], 'request_class').map((g) => g.key), [d.NO_CLASS], 'no class');
});

test('the session fold keys a model by its canonical id, as the rollups file it and the Models filter matches it', () => {
  const [row] = d.decodeRequestPage(fixture('requests')).requests;
  const wide = { ...row, id: 'r-wide', canonicalModel: 'claude-sonnet-5', model: 'claude-sonnet-5[1m]' };
  const plain = { ...row, id: 'r-plain', canonicalModel: 'claude-sonnet-5', model: 'claude-sonnet-5' };
  assert.deepEqual(d.foldGroups([wide, plain], 'model').map((g) => [g.key, g.label, g.requests]), [['claude-sonnet-5', 'claude-sonnet-5', 2]]);
});

test('the cooldown move reason reads failed over', () => {
  assert.equal(d.moveLabel('cooldown'), 'failed over', 'every reader of the map takes the word through moveLabel');
});

test('the rule and the failover move reasons together name each reason the map labels once', () => {
  assert.deepEqual([...d.RULE_MOVE_REASONS, ...d.FAILOVER_MOVE_REASONS].sort(), Object.keys(d.PIN_MIGRATION_LABEL).sort(), 'the two sets split the labeled reasons');
});

test("the held bars read one bar per bucket, the range's sum, and an empty range reads no bar", () => {
  const held = [
    { at: '2026-09-16T12:00:00.000Z', heldMs: 42_000, requests: 1 },
    { at: '2026-09-16T13:00:00.000Z', heldMs: 0, requests: 0 },
    { at: '2026-09-16T14:00:00.000Z', heldMs: 840_000, requests: 12 },
  ];
  const answer = { held, moves: { shed: 2 }, movesByReasonAndFit: { shed: { fits: 2 } }, points: [] };
  assert.deepEqual(d.heldBars(answer), { bars: held, totalMs: 882_000, totalRequests: 13 }, 'a bucket that held nothing keeps its bar at zero');
  assert.deepEqual(d.heldBars({ ...answer, held: [] }), { bars: [], totalMs: 0, totalRequests: 0 }, 'an answer from before the member holds no bucket');
});

test("a held bar's hover reads its requests and their time", () => {
  assert.equal(d.heldHover({ heldMs: 840_000, requests: 12 }), '12 requests held 14 min in all');
  assert.equal(d.heldHover({ heldMs: 3_900_000, requests: 1_200 }), '1,200 requests held 1 h 5 min in all');
  assert.equal(d.heldHover({ heldMs: 42_000, requests: 1 }), '1 request held 42 s in all', 'a wait under a minute reads its seconds');
  assert.equal(d.heldHover({ heldMs: 300, requests: 1 }), '1 request held 1 s in all', 'a wait that happened reads at least a second');
  assert.equal(d.heldHover({ heldMs: 0, requests: 0 }), 'no request held');
});

/** A route-shaped account group, the captured one with the members a case sets. */
/** The captured account's group with a priced Pro-week figure: the capture's day holds a request on a model without a
 *  weight, which nulls the figure the route answers. */
const accountGroup = (over = {}) => ({ ...d.decodeAnalyticsGroup(fixture('analytics_breakdown_accounts').rows[0]), costEquivalent: 0.004, ...over });

test('the address carries a unit as text and a bare address none', () => {
  // The page reads the parameter once into the console-wide setting and decides there what a value names.
  const unitIn = (search) => d.ANALYTICS_CODEC.fromParams(new URLSearchParams(search)).unit;
  assert.equal(unitIn('?unit=share'), 'share');
  assert.equal(unitIn('?unit=bogus'), 'bogus', 'a value the console does not know reads as sent');
  assert.equal(unitIn(''), '', 'no parameter reads empty');
  assert.equal(unitIn('?session=s-1'), '', 'another parameter names no unit');
  const bare = d.ANALYTICS_CODEC.fromParams(new URLSearchParams(''));
  assert.equal(d.ANALYTICS_CODEC.toParams({ ...bare, unit: '' }).has('unit'), false, 'a dropped unit leaves the address');
  assert.equal(d.ANALYTICS_CODEC.toParams({ ...bare, unit: 'share' }).toString(), 'unit=share');
});

test("one named account's shares sum over the groups the route answered them on, under any grouping", () => {
  const g = (costShares, planRequests = 2) => accountGroup({ costShares, planRequests });
  const plan = (x) => x.planRequests === null || x.planRequests > 0;
  const sum = (groups) => d.oneAccountShares(groups, plan);
  assert.deepEqual(sum([g({ five_hour: 0.25, seven_day: 0.125, seven_day_fable: 0.0625 }), g({ five_hour: 0.5, seven_day: 0.25 })]), { five_hour: 0.75, seven_day: 0.375, seven_day_fable: 0.0625 }, "each model's group adds to the account's own windows, the model week where a group ran on one");
  assert.deepEqual(sum([g({ five_hour: 0.25, seven_day: 0.125 }), g({ five_hour: 0.5 })]), { five_hour: 0.75 }, 'a group with plan requests that priced no figure on a window leaves it out, since a partial sum would read as the whole');
  assert.deepEqual(sum([g({ five_hour: 0.25, seven_day: 0.125 }), g({ five_hour: 0.5, seven_day: null })]), { five_hour: 0.75 }, 'and so does a window carried as null');
  assert.deepEqual(sum([g({ five_hour: 0.25, seven_day: 0.125 }), g({}, 0)]), { five_hour: 0.25, seven_day: 0.125 }, 'a group of billed rows alone carries no window to sum');
  assert.equal(sum([g(null), g(null)]), null, 'no group answered a share map');
});

test("one named account's whole is unavailable where a group with plan usage answered no share map, in either order", () => {
  const priced = accountGroup({ costShares: { five_hour: 0.25, seven_day: 0.125 } });
  const unmapped = accountGroup({ costShares: null });
  assert.ok(unmapped.planRequests > 0 && unmapped.planOutputTokens > 0, 'the captured group holds plan rows with tokens');
  for (const [what, groups] of [['priced first', [priced, unmapped]], ['unmapped first', [unmapped, priced]]]) {
    assert.equal(d.oneAccountShares(groups, d.planRowsRequired), null, `${what}: the priced group's shares are not the account's whole`);
  }
  assert.equal(d.oneAccountGroupOf(priced, [priced, unmapped]).costShares, null, 'and the whole the page reads is unavailable');
});

test('the idle slots of the plan card zero-fill against the flag, and an unpriced slot stays a gap', () => {
  const p = (at, over) => point(at * HOUR, 'desktop', { equivalent: 0.004, tokens: 100, uncachedTokens: 100, ...over });
  // The route answers a null figure with the flag unset for an idle bucket, and with the flag set for one holding a row
  // the cost model could not price.
  const { lines, rows } = d.foldBurn([p(0), p(1, { equivalent: null }), p(2, { equivalent: null, unpriced: true })], 'key', 3_600, NONE);
  assert.deepEqual(rows.map((r) => r.equivalent[0]), [0.004, 0, null], 'the idle slot reads zero and the unpriced one stays a gap');
  assert.ok(Math.abs(lines[0].perHour - 0.002) < 1e-12, 'the mean counts the idle slot and leaves the gap out');
  assert.equal(lines[0].unpriced, true, 'the legend reads the flag');
});

test("a plan card scales a bucket by the anchor's capacity at the read over the chosen tier's at the bucket, on the card's window", () => {
  // Under the Max week Pro, the anchor, reads 100 on the 7-day window and 10 on the 5-hour one in the first revision
  // and 200 and 40 in the second, which is in force at the read. Max 5x reads 200 and 20 in both, and the Fable week
  // is 0.5 of the 7-day window and then 0.25. The bucket falls in the first revision.
  const table = (pro, fable) => ({ ...EMPTY, capacity: { default_claude_ai: pro, default_claude_max_5x: { fiveHour: 20, sevenDay: 200 } }, modelWindows: { seven_day_fable: { capacityFactor: fable, of: 'seven_day' } } });
  const [first, second] = [table({ fiveHour: 10, sevenDay: 100 }, 0.5), table({ fiveHour: 40, sevenDay: 200 }, 0.25)];
  const costs = { ...second, revisions: [{ content: first, effective: '2026-09-01', effectiveDate: '2026-09-01T00:00:00.000Z' }, { content: second, effective: '2026-09-10', effectiveDate: '2026-09-10T00:00:00.000Z' }] };
  const readAt = Date.parse('2026-09-16T12:00:00Z');
  const points = [point(Date.parse('2026-09-05T12:00:00Z'), 'a', { equivalent: 0.1, fableEquivalent: 0.1, fiveHourEquivalent: 0.1, uncachedTokens: 100 })];
  assert.deepEqual(
    [
      d.scalePoints(points, d.weekScaleAt(costs, 'max', readAt))[0].equivalent,
      d.scalePoints(points, d.fiveHourScaleAt(costs, 'max', readAt), 'fiveHourEquivalent')[0].equivalent,
      d.scalePoints(points, d.fableWeekScaleAt(costs, 'max', readAt), 'fableEquivalent')[0].equivalent,
    ],
    [0.1, 0.2, 0.2],
    'the week 200 over 200, the 5-hour window 40 over 20, the Fable week 200 over half of 200',
  );
  const unsized = { ...EMPTY, revisions: [] };
  assert.deepEqual([d.weekScaleAt(unsized, 'max', readAt), d.fiveHourScaleAt(unsized, 'max', readAt), d.fableWeekScaleAt(unsized, 'max', readAt)], [null, null, null], 'a cost model with no anchor capacity scales no card');
});

test("a range whose later revision drops the chosen tier's capacity draws its earlier buckets and leaves its later ones as gaps", () => {
  // Under the Max week Max 5x reads 200 on the 7-day window beside Pro's 100 in the first revision and nothing in the
  // second, which takes effect between the first two hours and is in force at the read. The third hour's rows were
  // billed, a figure of 0, which drew nothing from the plan whatever the scale.
  const table = (max) => ({ ...EMPTY, capacity: { default_claude_ai: { fiveHour: null, sevenDay: 100 }, default_claude_max_5x: { fiveHour: null, sevenDay: max } } });
  const start = Date.parse('2026-09-10T00:00:00Z');
  const costs = { ...table(null), revisions: [{ content: table(200), effective: '2026-09-01', effectiveDate: '2026-09-01T00:00:00.000Z' }, { content: table(null), effective: '2026-09-10', effectiveDate: '2026-09-10T00:00:00.000Z' }] };
  const points = [point(start - HOUR, 'a', { equivalent: 0.2, uncachedTokens: 100 }), point(start, 'a', { equivalent: 0.2, uncachedTokens: 100 }), point(start + HOUR, 'a', { equivalent: 0, uncachedTokens: 100 })];
  const { lines, rows } = d.foldBurn(d.scalePoints(points, d.weekScaleAt(costs, 'max', start + HOUR)), 'model', 3_600, NONE);
  assert.deepEqual(rows.map((r) => r.equivalent[0]), [0.1, null, 0], "the first hour at Pro's 100 over Max 5x's 200, the second a gap, the third's billed zero");
  assert.equal(lines[0].unpriced, true, 'the gap reads unpriced rather than idle');
});

test("the plan cards' colors read one order, every fold's named lines, so a group one card draws and another folds into its rest keeps a color", () => {
  const week = { lines: [{ key: 'opus', other: false }, { key: 'sonnet', other: false }, { key: 'other', other: true }] };
  const fable = { lines: [{ key: 'fable', other: false }] };
  assert.deepEqual(d.burnColorOrder([week, fable]), ['fable', 'opus', 'sonnet']);
});

test('the Fable week card folds the Fable figure, a null beside tokens its gap, and draws no line for a group whose figure reads 0 wherever it has traffic', () => {
  const points = [
    point(0, 'a', { fableEquivalent: 0.02, uncachedTokens: 100 }),
    point(HOUR, 'a', { fableEquivalent: null, uncachedTokens: 100 }),
    point(0, 'b', { equivalent: 0.05, fableEquivalent: 0, uncachedTokens: 100 }),
    point(HOUR, 'b', { equivalent: 0.05, fableEquivalent: 0, uncachedTokens: 100 }),
  ];
  const { lines, rows } = d.foldBurn(d.scalePoints(d.figuredGroups(points, 'fableEquivalent'), () => 1, 'fableEquivalent'), 'model', 3_600, NONE);
  assert.deepEqual(lines.map((l) => l.key), ['a'], 'b ran on no Fable model');
  assert.deepEqual(rows.map((r) => r.equivalent[0]), [0.02, null]);
  assert.equal(lines[0].unpriced, true, 'the null beside tokens reads unpriced');
});

test("the Cost by card's note names the groups unpriced in range up to three, counts them past three, and counts their buckets, singular at one", () => {
  assert.equal(d.unpricedNote([], 0), null, 'nothing unpriced reads no note');
  assert.equal(d.unpricedNote(['research'], 1), 'research unpriced in 1 bucket, drawn as a gap');
  assert.equal(d.unpricedNote(['a', 'b', 'c'], 5), 'a · b · c unpriced in 5 buckets, drawn as gaps', 'up to three labels joined by middle dots');
  assert.equal(d.unpricedNote(['research', 'desktop (deleted, created Oct 11, 2026, 12:15)'], 3), 'research · desktop (deleted, created Oct 11, 2026, 12:15) unpriced in 3 buckets, drawn as gaps', "a deleted holder's label reads whole between the dots");
  assert.equal(d.unpricedNote(['a'], 3), 'a unpriced in 3 buckets, drawn as gaps', 'the gaps follow the buckets');
  assert.equal(d.unpricedNote(['a', 'b', 'c', 'd'], 4), '4 groups unpriced in 4 buckets, drawn as gaps', 'past three labels the note counts the groups');
});

test('the net saving is the gross less the probe cost, under zero where the probes cost more', () => {
  assert.equal(d.netSaving(1.5, 0.25), 1.25);
  assert.equal(d.netSaving(0.125, 0.5), -0.375, 'probes that cost more than they saved net below zero');
  assert.equal(d.netSaving(0, 0), 0);
});

test("the kept-warm note sums the two counts and names each TTL's", () => {
  assert.equal(d.keptWarmNote({ requests: 4, tokens1h: 4_345, tokens5m: 8_000 }), '12,345 tokens read warm, 8,000 at five minutes and 4,345 at an hour');
  assert.equal(d.keptWarmNote({ requests: 0, tokens1h: 0, tokens5m: 0 }), '0 tokens read warm, 0 at five minutes and 0 at an hour');
});

test("a group with nothing to charge a plan window forces no gap on the named account's whole: failures with no token", () => {
  const base = accountGroup();
  const none = Object.fromEntries(Object.keys(base.tokens).map((k) => [k, 0]));
  const priced = accountGroup({ costShares: { five_hour: 0.25, seven_day: 0.125 } });
  const failed = accountGroup({ costEquivalent: 0, costShares: {}, planOutputTokens: 0, tokens: none });
  assert.ok(failed.planRequests > 0, 'the failures still count as plan requests');
  assert.deepEqual(d.oneAccountShares([priced, failed], d.planRowsRequired), priced.costShares, 'plan requests that wrote no token carry no window to lack');
});

test('the probe cost counts the probes that reached the upstream, so a group whose probes were all held or skipped costs nothing', () => {
  const base = accountGroup();
  assert.equal(d.upstreamProbes(accountGroup({ probes: { ...base.probes, failed: 0, held: 3, ok: 0, skipped: 2 } })), 0, 'held and skipped probes never ran');
  assert.equal(d.upstreamProbes(accountGroup({ probes: { ...base.probes, failed: 2, held: 3, ok: 5, skipped: 4 } })), 7, 'the answered and the failed probes');
});

test('probeParts counts a migrated probe once', () => {
  assert.deepEqual(d.probeParts({ failed: 0, held: 0, migrated: 1, ok: 241, skipped: 0 }), { failed: 0, held: 0, migrated: 1, ok: 240, skipped: 0 }, "the route's ok holds the migrated probe, so the parts sum to 241");
});

test('probeParts floors the OK part at zero', () => {
  assert.equal(d.probeParts({ failed: 0, held: 0, migrated: 1, ok: 0, skipped: 0 }).ok, 0, 'a bucket whose mark landed apart from its request row');
});

test('a burn line by model names the group without a model "no model"', () => {
  const { lines } = d.foldBurn([point(0, 'claude-haiku-4-5', { uncachedTokens: 60 }), point(0, '')], 'model', 60, NONE);
  assert.deepEqual(lines.map((l) => l.label), ['claude-haiku-4-5', 'no model'], 'the burn line under the model grouping');
});

test('the session fold counts the plan requests and output tokens of a mixed-funding request set as the route does', () => {
  const plan = d.decodeRequestPage(fixture('requests')).requests.find((r) => r.id === 'r-captured');
  const attempt = plan.attempts[plan.attempts.length - 1];
  const shares = { five_hour: 0.25, seven_day: 0.125 };
  const rows = [
    { ...plan, costShares: shares, id: 'p-1', usage: { ...plan.usage, output: 100 } },
    { ...plan, costShares: shares, id: 'p-2', usage: { ...plan.usage, output: 300 } },
    { ...plan, attempts: [{ ...attempt, overageInUse: true }], costShares: null, id: 'o-1', usage: { ...plan.usage, output: 1000 } },
  ];
  const [folded] = d.foldGroups(rows, 'account');
  assert.equal(folded.requests, 3, 'the overage row counts as a request');
  assert.deepEqual([folded.planRequests, folded.planOutputTokens], [2, 400], 'and widens no plan count');
});

test("a plan-funded row with no token joins the session fold's plan count and not its share total, as the ledger sums it", () => {
  const plan = d.decodeRequestPage(fixture('requests')).requests.find((r) => r.id === 'r-captured');
  const shares = { five_hour: 0.25, seven_day: 0.125 };
  const rows = [
    { ...plan, costShares: shares, id: 'p-priced' },
    // A failed attempt on a model the cost model has no weight for: the route answers no share for a charge of nothing.
    { ...plan, costShares: null, id: 'p-failed', outcome: 'failed', usage: { ...plan.usage, cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: 0, input: 0, output: 0 } },
  ];
  const [folded] = d.foldGroups(rows, 'account');
  assert.deepEqual(folded.costShares, shares, "the total is the priced row's shares");
  assert.deepEqual([folded.planRequests, folded.planOutputTokens], [2, plan.usage.output], 'the failed row still counts as a plan request');
});

/** An account's hour as the accounts route answers it, the peaks and counts empty, the windows as given. */
const rollup = (accountId, hourMs, windows) => ({ accountId, cooldownSeconds: 0, hour: new Date(hourMs).toISOString(), overageSeconds: 0, peak: { '5h': null, '7d': null, '7d_fable': null }, pins: { created: 0, migratedIn: 0, migratedOut: 0 }, windows });
const windowSample = (over = {}) => ({ lead: 0.1, resetsAt: '2026-09-16T12:55:00.000Z', sampledAt: '2026-09-16T10:55:00.000Z', utilization: 0.5, ...over });

test("the window lines fold each account's utilization and lead per bucket and leave a gap where no sample is or the lead is null", () => {
  const t = Date.UTC(2026, 8, 16, 10);
  const span = { fromMs: t, toMs: t + 3 * HOUR };
  const rows = [
    rollup('a', t, { five_hour: windowSample({ lead: 0.2, utilization: 0.6 }), seven_day: windowSample({ lead: -0.1, utilization: 0.3 }) }),
    rollup('b', t, { five_hour: windowSample({ lead: null, resetsAt: '2026-09-16T11:00:00.000Z', utilization: null }) }),
    rollup('a', t + 2 * HOUR, { five_hour: windowSample({ lead: -0.05, utilization: 0.45 }) }),
  ];
  const usage = d.windowLines(rows, 'five_hour', 'usage', span, HOUR);
  assert.deepEqual(usage.accounts, ['a', 'b']);
  assert.deepEqual(
    usage.data,
    [
      { at: t, a: 60, b: null },
      { at: t + HOUR, a: null, b: null },
      { at: t + 2 * HOUR, a: 45, b: null },
      { at: t + 3 * HOUR, a: null, b: null },
    ],
    'the utilization in percent per bucket, a gap for a bucket without a sample and for a reset-only sample',
  );
  const pace = d.windowLines(rows, 'five_hour', 'pace', span, HOUR);
  assert.deepEqual(pace.data.map((r) => [r.a, r.b, r['b:score']]), [[20, null, null], [null, null, null], [-5, null, null], [null, null, null]], 'the lead in percent, the reset-only sample a gap on the pace side with no score');
  assert.ok(Math.abs(pace.data[0]['a:score'] - 0.2) < 1e-9, `the pace score of the sample the lead was read from rides beside it: ${pace.data[0]['a:score']}`);
  assert.equal(pace.data[1]['a:score'], null, 'and a gap carries none');
  const seven = d.windowLines(rows, 'seven_day', 'pace', span, HOUR);
  assert.deepEqual(seven.data.map((r) => r.a), [-10, null, null, null], 'another window reads its own samples alone');
  // A bucket wider than an hour draws the latest hour inside it that holds a sample, so one in its first hour alone
  // is no gap, and a later hour's sample wins over an earlier one.
  const wide = { fromMs: t, toMs: t + 2 * HOUR };
  const first = d.windowLines([rollup('a', t, { five_hour: windowSample({ utilization: 0.6 }) })], 'five_hour', 'usage', wide, 2 * HOUR);
  assert.deepEqual(first.data.map((r) => r.a), [60, null], 'a sample in the first hour of a two-hour bucket draws it');
  const later = d.windowLines([rollup('a', t + HOUR, { five_hour: windowSample({ utilization: 0.7 }) }), rollup('a', t, { five_hour: windowSample({ utilization: 0.6 }) })], 'five_hour', 'usage', wide, 2 * HOUR);
  assert.deepEqual(later.data.map((r) => r.a), [70, null], 'the later hour wins whatever order the rows arrive in');
});

test("the Peak grid keeps a bucket's largest peak, starts at the span's first edge and leaves a missing bucket null", () => {
  const t = Date.UTC(2026, 8, 16, 10);
  const peaked = (hourMs, peak) => ({ ...rollup('a', hourMs, {}), peak: { '5h': peak, '7d': null, '7d_fable': null } });
  const span = { fromMs: t + 30 * 60_000, toMs: t + 4 * HOUR };
  const rows = [peaked(t, 0.75), peaked(t + HOUR, 0.5), peaked(t + 4 * HOUR, 0.25)];
  assert.deepEqual(d.peakLines(rows, ['a'], span, 2 * HOUR), [
    { at: t, a: 75 },
    { at: t + 2 * HOUR, a: null },
    { at: t + 4 * HOUR, a: 25 },
  ]);
});

test("the pace score is the remaining share over the hours from the sample's instant to the reset", () => {
  const late = windowSample({ resetsAt: '2026-09-16T11:00:00.000Z', sampledAt: '2026-09-16T10:55:00.000Z', utilization: 0.5 });
  assert.ok(Math.abs(d.paceScoreOf(late) - 6) < 1e-9, `five minutes to the reset, not the hour from the bucket's start: ${d.paceScoreOf(late)}`);
  const start = windowSample({ resetsAt: '2026-09-16T11:00:00.000Z', sampledAt: '2026-09-16T10:00:00.000Z', utilization: 0.5 });
  assert.ok(Math.abs(d.paceScoreOf(start) - 0.5) < 1e-9, "scored from the hour's start the same sample would read 0.5");
  assert.equal(d.paceScoreOf(windowSample({ resetsAt: '2026-09-16T10:55:00.000Z', sampledAt: '2026-09-16T10:55:00.000Z', utilization: 0.5 })), null, "a reset at the sample's instant is a stale sample");
  assert.equal(d.paceScoreOf(windowSample({ resetsAt: '2026-09-16T10:00:00.000Z', sampledAt: '2026-09-16T10:55:00.000Z', utilization: 0.5 })), null, 'and so is a reset before it');
  assert.equal(d.paceScoreOf(windowSample({ resetsAt: '2026-09-16T12:55:00.000Z', sampledAt: '2026-09-16T10:55:00.000Z', utilization: 1.2 })), 0, 'a utilization past 1 has nothing remaining rather than a negative share');
});

test('a sample without a utilization has no pace score', () => {
  assert.equal(d.paceScoreOf(windowSample({ lead: null, resetsAt: '2026-09-16T11:00:00.000Z', utilization: null })), null, 'a reset-only sample');
  assert.equal(d.paceScoreOf(windowSample({ resetsAt: null, utilization: 0.5 })), null, 'and a sample without a reset has no horizon');
});

test('a figure reads in the chosen week, and a dash where none was priced', () => {
  assert.equal(d.equivalentOrDash(0.004, 'ultra', 0.1), '0.04% of an Ultra week');
  assert.equal(d.equivalentOrDash(0.004, 'pro', 1), '0.4% of a Pro week');
  assert.equal(d.equivalentOrDash(undefined, 'ultra', 0.1), '-', 'a side with no rows');
  assert.equal(d.equivalentOrDash(null, 'ultra', 0.1), '-', 'a side priced nothing');
});

test("the Cost section's head reads each position off one group of plan and billed rows", () => {
  const g = accountGroup();
  assert.ok(g.planRequests > 0 && g.requests > g.planRequests && g.billedUsd > 0 && g.costEquivalent === 0.004, 'the group holds rows of both fundings');
  const billed = d.fmtUsd(g.billedUsd);
  const dollars = d.fmtUsd(g.costUsd);
  const native = d.costHeadOf('native', g, 'ultra', 0.1);
  assert.deepEqual(native.tile, { figures: [{ text: '0.04% of an Ultra week', word: 'subscription' }, { text: billed, word: 'billed' }], label: 'Cost', note: d.NATIVE_SUBSET }, 'native stacks the figure in the chosen week over the billed dollars');
  assert.equal(native.subset, d.NATIVE_SUBSET);
  assert.equal(native.meta, `0.04% of an Ultra week and ${billed} in range`);
  assert.equal(d.costHeadOf('native', g, 'ultra', null), null, 'native waits on the scale, so no Pro-week figure reads under the chosen week');
  // A dollar position reads the totals its funded read answered, so the head prints their dollars under its words.
  const all = d.costHeadOf('all', g, 'ultra', null);
  assert.deepEqual([all.tile, all.subset, all.meta], [{ figures: [{ text: dollars }], label: 'Estimated cost', note: 'at the effective price list' }, 'all requests', `${dollars} in range, all requests`]);
  const usd = d.costHeadOf('usd', g, 'ultra', null);
  assert.deepEqual([usd.tile, usd.subset, usd.meta], [{ figures: [{ text: dollars }], label: 'Billed dollars', note: d.UNIT_SUBSET.usd }, d.UNIT_SUBSET.usd, `${dollars} in range, billed`]);
  const plan = d.costHeadOf('equivalent', g, 'ultra', null);
  assert.deepEqual([plan.tile, plan.subset, plan.meta], [{ figures: [{ text: dollars }], label: 'Dollar equivalent', note: d.UNIT_SUBSET.equivalent }, d.UNIT_SUBSET.equivalent, `${dollars} in range, equivalent`]);
});

test('native leaves out a side with no rows', () => {
  const g = accountGroup();
  const billed = d.fmtUsd(g.billedUsd);
  const plan = d.costHeadOf('native', accountGroup({ billedUsd: 0, requests: g.planRequests }), 'ultra', 0.1);
  assert.deepEqual([plan.tile.figures, plan.meta], [[{ text: '0.04% of an Ultra week', word: 'subscription' }], '0.04% of an Ultra week in range'], 'a sum of plan rows alone reads the figure alone');
  const billedOnly = d.costHeadOf('native', accountGroup({ costEquivalent: 0, planRequests: 0 }), 'ultra', 0.1);
  assert.deepEqual([billedOnly.tile.figures, billedOnly.meta], [[{ text: billed, word: 'billed' }], `${billed} in range`], 'a sum of billed rows alone reads the dollars alone');
  // A fold of request rows counts no plan request outside the account grouping, so its figures tell the sides apart.
  const folded = d.costHeadOf('native', accountGroup({ planRequests: null }), 'ultra', 0.1);
  assert.equal(folded.meta, `0.04% of an Ultra week and ${billed} in range`);
});

test('native reads a dash under its title on the subscription side where the route priced the plan rows nothing', () => {
  const g = accountGroup({ costEquivalent: null });
  assert.ok(g.planRequests > 0 && g.requests > g.planRequests, 'the captured group holds rows of both fundings');
  const billed = d.fmtUsd(g.billedUsd);
  assert.deepEqual(d.sidesOf(g), { billed: g.billedUsd, equivalent: null }, 'plan rows priced nothing read null');
  assert.deepEqual(d.sidesOf(accountGroup({ costEquivalent: 0, planRequests: 0 })), { billed: g.billedUsd, equivalent: undefined }, 'a sum with no plan row leaves the side out');
  const unpriced = d.costHeadOf('native', g, 'ultra', 0.1);
  assert.deepEqual([unpriced.tile.figures, unpriced.meta, unpriced.unpriced], [[{ text: '-', word: 'subscription' }, { text: billed, word: 'billed' }], `- and ${billed} in range`, true], 'the dash takes the unpriced title whatever the other rows priced');
});

test('a native table cell reads the short form in the chosen week with the billed dollars under it, the full reading and the hint on its title', () => {
  const g = accountGroup();
  const dollars = d.fmtUsd(g.billedUsd);
  assert.deepEqual(d.nativeCellOf(0.004, g.billedUsd, 'ultra', 0.1), { main: '0.04% Ultra week', sub: `${dollars} billed`, title: `0.04% of an Ultra week, ${dollars} billed` });
  assert.deepEqual(d.nativeCellOf(0.004, undefined, 'ultra', 0.1, 'Ultra: 0.11% of the 7-day window'), { main: '0.04% Ultra week', sub: '', title: 'Ultra: 0.11% of the 7-day window. 0.04% of an Ultra week' }, "one account's cell carries its hint ahead of the figure");
  assert.deepEqual(d.nativeCellOf(undefined, g.billedUsd, 'ultra', 0.1), { main: dollars, sub: '', title: `${dollars} billed` }, 'a cell with no plan row reads its billed dollars alone');
  assert.deepEqual(d.nativeCellOf(null, null, 'ultra', 0.1), { main: '-', sub: '-', title: '' }, 'a side priced nothing reads a dash');
  assert.deepEqual(d.nativeCellOf(undefined, undefined, 'ultra', 0.1), { main: '-', sub: '', title: '' }, 'a cell with no row on either side');
});

test('ratioOff marks a ratio outside the band after a rise of five percent', () => {
  assert.equal(d.ratioOff({ observedRise: 0.3, predictedShare: 0.3, ratio: 2 }), true, 'a window charged twice the table');
  assert.equal(d.ratioOff({ observedRise: 0.3, predictedShare: 0.3, ratio: 0.7 }), true, 'a window charged under the table');
  assert.equal(d.ratioOff({ observedRise: 0.04, predictedShare: 0.3, ratio: 3 }), false, 'a rise under five percent marks nothing, since whole-percent readings make it coarse');
  assert.equal(d.ratioOff({ observedRise: 0.05, predictedShare: 0.3, ratio: 3 }), true, 'a rise of five percent counts');
  assert.equal(d.ratioOff({ observedRise: 0.3, predictedShare: 0.3, ratio: 1.1 }), false);
  assert.deepEqual([0.8, 1.25].map((ratio) => d.ratioOff({ observedRise: 0.3, predictedShare: 0.3, ratio })), [false, false], 'the edges sit inside the band');
});

test('ratioOff marks no row whose priced share sits under the floor the rise reads, as the engine badges it', () => {
  assert.equal(d.ratioOff({ observedRise: 0.3, predictedShare: 0.04, ratio: 3 }), false, 'a priced share under five percent marks nothing, though the rise and the ratio would');
  assert.equal(d.ratioOff({ observedRise: 0.3, predictedShare: 0.04, ratio: 0.5 }), false, 'nor under the band');
  assert.equal(d.ratioOff({ observedRise: 0.3, predictedShare: d.MIN_RISE, ratio: 3 }), true, 'a priced share at the floor counts');
});

test('the error rate is the requests that did not succeed over the requests', () => {
  assert.equal(d.errorRateOf({ requests: 10, succeeded: 9 }), 0.1);
  assert.equal(d.errorRateOf({ requests: 0, succeeded: 0 }), null, 'a group with no request has no rate, which the cells draw as a dash');
});

test('sumGroups sums refused and the refusals map', () => {
  const row = fixture('analytics_breakdown').rows[0];
  const sum = d.sumGroups([d.decodeAnalyticsGroup({ ...row, refused: 2, refusals: { guardrail_blocked: 2 } }), d.decodeAnalyticsGroup({ ...row, refused: 1, refusals: { auth_unknown: 1 } })]);
  assert.equal(sum.refused, 3);
  assert.deepEqual({ ...sum.refusals }, { auth_unknown: 1, guardrail_blocked: 2 });
});

test('a color slot skips the empty key, so the first named key takes slot 0 and the empty key the fallback slot 7', () => {
  const keys = ['', 'a', 'b'];
  assert.deepEqual(['a', 'b', ''].map((k) => d.slotIndex(keys, k)), [0, 1, 7], 'the named keys count from 0 past the empty key, which takes slot 7');
});

test('a key not listed and a key past the eighth named one take the fallback slot 7', () => {
  const named = ['k1', 'k2', 'k3', 'k4', 'k5', 'k6', 'k7', 'k8', 'k9'];
  assert.equal(d.slotIndex(['', 'a', 'b'], 'c'), 7, 'a key not listed');
  assert.equal(d.slotIndex(['', ...named], 'k9'), 7, 'the ninth named key');
});

test('without an empty key the color slots follow the named keys in their order', () => {
  assert.equal(d.slotIndex(['a', 'b'], 'b'), 1);
});

test('an empty key between two named keys takes no slot of theirs, wherever the fallback path lists it', () => {
  assert.equal(d.slotIndex(['a', '', 'b'], 'b'), 1);
});

test("a failed surface heads the week reading's unpriced sentence unpriced and any other failure could not load", () => {
  const unpriced = { text: 'Unpriced: the cost model holds no 7-day capacity for the Max week', unpriced: true };
  assert.equal(d.failureHead(unpriced, 'cost split'), 'Unpriced: cost split', 'no retry clears an unpriced week');
  assert.equal(d.failureHead({ text: 'HTTP 503', unpriced: false }, 'cost split'), 'Could not load cost split');
});

test("a patch that moves the session drops the key lifetime and the agent, since each names the earlier session's", () => {
  assert.deepEqual(d.analyticsPatch({ session: 'a' }, { session: 'b' }), { agentId: '', key: '', session: 'b' }, 'a new session');
  assert.deepEqual(d.analyticsPatch({ session: 'a' }, { session: 'a' }), { session: 'a' }, 'the same session chosen again keeps both');
  const range = { from: '', preset: '7d', to: '' };
  assert.deepEqual(d.analyticsPatch({ session: 'a' }, { range }), { range }, 'a patch that names no session keeps both');
});
