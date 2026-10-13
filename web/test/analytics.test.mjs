import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import * as d from './dist/api.js';

const fixture = (name) => JSON.parse(readFileSync(new URL(`./fixtures/${name}.json`, import.meta.url), 'utf8')).body;

const HOUR = 3_600_000;
const point = (at, group, over = {}) => {
  const p = { at: new Date(at).toISOString(), cachedTokens: 0, fiveHourShare: null, group, sevenDayShare: null, uncachedTokens: 0, unit: null, usd: 0, ...over };
  return { tokens: p.cachedTokens + p.uncachedTokens, ...p };
};
const NONE = new Set();
/** The cost model with nothing in it, which a test fills with the rows it needs. */
const EMPTY = d.decodeSubscriptionCosts(undefined);

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

test('the burn fold reads every token and dollar figure per minute of its bucket and both window shares per hour', () => {
  const { lines, rows } = d.foldBurn(
    [point(0, 'primary', { cachedTokens: 36_000, fiveHourShare: 0.3, sevenDayShare: 0.06, uncachedTokens: 3_600, unit: 'window_share' }), point(6 * HOUR, 'primary', { unit: 'window_share' })],
    'account',
    6 * 3_600,
    new Set(['primary']),
  );
  assert.deepEqual([rows[0].cached[0], rows[0].uncached[0], rows[0].usd[0]], [100, 10, 0], 'six hours of tokens over 360 minutes');
  assert.ok(Math.abs(rows[0].fiveHour[0] - 0.05) < 1e-12, 'a 30% share of the 5-hour window over six hours is 5% an hour');
  assert.ok(Math.abs(rows[0].sevenDay[0] - 0.01) < 1e-12, 'a 6% share of the 7-day window over six hours is 1% an hour');
  assert.deepEqual([rows[1].fiveHour[0], rows[1].sevenDay[0]], [0, 0], 'a bucket with no traffic reads zero on a window the line priced');
  assert.equal(lines[0].fiveHourPeak, rows[0].fiveHour[0]);
  assert.ok(Math.abs(lines[0].fiveHourPerHour - 0.025) < 1e-12, 'the range average spreads the 5-hour share over both buckets');
  assert.ok(Math.abs(lines[0].sevenDayPerHour - 0.005) < 1e-12, 'and the 7-day share');
  assert.ok(Math.abs(lines[0].tokensPerMinute - 55) < 1e-9, 'the range average spreads the tokens over both buckets');
});

test('a range ending now, twenty minutes into the running hour, rates that bucket over 1,200 seconds', () => {
  const at13 = Date.UTC(2026, 8, 16, 13);
  const end = at13 + HOUR + 20 * 60_000;
  const { lines, rows } = d.foldBurn(
    [
      point(at13, 'primary', { fiveHourShare: 0.3, uncachedTokens: 6_000, unit: 'window_share' }),
      point(at13, 'overflow', { unit: 'usd', usd: 6 }),
      point(at13 + HOUR, 'primary', { fiveHourShare: 0.2, uncachedTokens: 4_000, unit: 'window_share' }),
      point(at13 + HOUR, 'overflow', { unit: 'usd', usd: 4 }),
    ],
    'account',
    3_600,
    new Set(['primary']),
    d.MAX_BURN_LINES,
    end,
    end,
  );
  assert.deepEqual(rows.map((r) => r.seconds), [3_600, 1_200], 'the full first hour, and the twenty minutes the second has run');
  const [primary, overflowLine] = [lines.findIndex((l) => l.key === 'primary'), lines.findIndex((l) => l.key === 'overflow')];
  assert.deepEqual([rows[0].uncached[primary], rows[1].uncached[primary]], [100, 200], 'tokens per minute of the time each bucket covers');
  assert.ok(Math.abs(rows[1].usd[overflowLine] - 0.2) < 1e-12, 'four dollars over twenty minutes');
  assert.ok(Math.abs(rows[1].fiveHour[primary] - 0.6) < 1e-12, 'a 20% share over a third of an hour is 60% an hour');
  // Weighed 3:1 by the seconds each bucket covers: (100 x 3 + 200) / 4 and (0.3 x 3 + 0.6) / 4.
  assert.ok(Math.abs(lines[primary].tokensPerMinute - 125) < 1e-9, 'the legend weighs each bucket by the time it covers');
  assert.ok(Math.abs(lines[primary].fiveHourPerHour - 0.375) < 1e-12, 'and the window shares');
  assert.ok(Math.abs(lines[overflowLine].usdPerMinute - 0.125) < 1e-12, 'and the dollars');
});

test('a past range ending twenty minutes into an hour rates that hour in full', () => {
  // The route reads whole hours through the hour ceiling of the range's end, so it answered the whole 14:00 hour.
  const at13 = Date.UTC(2026, 8, 16, 13);
  const { rows } = d.foldBurn([point(at13, 'overflow', { uncachedTokens: 600 }), point(at13 + HOUR, 'overflow', { uncachedTokens: 600 })], 'key', 3_600, NONE, d.MAX_BURN_LINES, at13 + HOUR + 20 * 60_000, at13 + 3 * HOUR);
  assert.deepEqual(rows.map((r) => r.seconds), [3_600, 3_600]);
  assert.deepEqual(rows.map((r) => r.uncached[0]), [10, 10]);
});

test('a past range ending inside a 6-hour bucket rates it over the hours read', () => {
  // The bucket at 12:00 holds the hours 12:00 to 15:00, the hour ceiling of an end at 14:20.
  const at12 = Date.UTC(2026, 8, 16, 12);
  const end = at12 + 2 * HOUR + 20 * 60_000;
  const { rows } = d.foldBurn([point(at12 - 6 * HOUR, 'overflow', { uncachedTokens: 3_600 }), point(at12, 'overflow', { uncachedTokens: 1_800 })], 'key', 6 * 3_600, NONE, d.MAX_BURN_LINES, end, end + 24 * HOUR);
  assert.deepEqual(rows.map((r) => r.seconds), [21_600, 10_800]);
  assert.deepEqual(rows.map((r) => r.uncached[0]), [10, 10]);
});

test('a range ending on a bucket boundary rates every bucket over the full width', () => {
  const at12 = Date.UTC(2026, 8, 16, 12);
  const points = [point(at12, 'overflow', { cachedTokens: 1_200, fiveHourShare: 0.1, unit: 'window_share' }), point(at12 + HOUR, 'overflow', { uncachedTokens: 600, fiveHourShare: 0.05, unit: 'window_share' })];
  const sized = new Set(['overflow']);
  const ended = d.foldBurn(points, 'account', 3_600, sized, d.MAX_BURN_LINES, at12 + 2 * HOUR, at12 + 2 * HOUR);
  assert.deepEqual(ended.rows.map((r) => r.seconds), [3_600, 3_600]);
  assert.deepEqual(ended, d.foldBurn(points, 'account', 3_600, sized), 'the same figures as a fold with no end');
});

test('a range that ended inside the running hour, read ten minutes later, rates that hour up to the read', () => {
  const at14 = Date.UTC(2026, 8, 16, 14);
  const { rows } = d.foldBurn([point(at14, 'overflow', { uncachedTokens: 1_800 })], 'key', 3_600, NONE, d.MAX_BURN_LINES, at14 + 20 * 60_000, at14 + 30 * 60_000);
  assert.deepEqual([rows[0].seconds, rows[0].uncached[0]], [1_800, 60], 'the route answered the hour as it stood at the read, 14:00 to 14:30');
});

test('a running bucket sets the peak and the axis only once it covers a tenth of its width', () => {
  const at13 = Date.UTC(2026, 8, 16, 13);
  const points = [point(at13, 'primary', { fiveHourShare: 0.06, uncachedTokens: 600, unit: 'window_share' }), point(at13 + HOUR, 'primary', { fiveHourShare: 0.02, uncachedTokens: 100, unit: 'window_share' })];
  const sized = new Set(['primary']);
  const fold = (seconds) => d.foldBurn(points, 'account', 3_600, sized, d.MAX_BURN_LINES, at13 + HOUR + seconds * 1_000, at13 + HOUR + seconds * 1_000);

  const early = fold(20);
  assert.deepEqual(early.rows.map((r) => [r.seconds, r.partial, r.setsScale]), [[3_600, false, true], [20, true, false]]);
  assert.ok(Math.abs(early.rows[1].fiveHour[0] - 3.6) < 1e-12, 'the running bucket keeps its rate: 2% over 20 seconds');
  assert.equal(early.lines[0].fiveHourPeak, early.rows[0].fiveHour[0], 'twenty seconds in, the peak is the full hour');

  const later = fold(7 * 60);
  assert.deepEqual(later.rows.map((r) => [r.seconds, r.partial, r.setsScale]), [[3_600, false, true], [420, true, true]]);
  assert.equal(later.lines[0].fiveHourPeak, later.rows[1].fiveHour[0], 'seven minutes in, the running hour sets the peak');
});

test('a capped axis top rises to the last of the five ticks recharts would draw over it', () => {
  // The expected tops are recharts-scale's last value of getNiceTickValues([0, top], 5) for each top.
  const tops = [[1_023, 1_200], [1_000, 1_000], [61.5, 80], [9.2, 12], [0.004, 0.004], [0.0013, 0.0014]];
  assert.deepEqual(tops.map(([top]) => d.niceAxisTop(top)), tops.map(([, nice]) => nice));
  assert.equal(d.niceAxisTop(0), 0, 'an empty axis stays empty');
});

test('an account burns in the unit its figures come in, a subscription whose tier the cost model does not size in none, and every other grouping in dollars', () => {
  const answered = [
    point(0, 'overflow', { unit: 'usd', usd: 1.2 }),
    point(0, 'nightly', { fiveHourShare: 0.01, sevenDayShare: 0.001, unit: 'window_share' }),
    point(0, 'tierless', { uncachedTokens: 40, unit: 'window_share' }),
    point(0, 'idle', { fiveHourShare: 0, sevenDayShare: 0, unit: 'window_share' }),
    point(0, 'departed', { uncachedTokens: 20 }),
  ];
  const { lines } = d.foldBurn(answered, 'account', 3_600, new Set(['idle', 'nightly']));
  const byKey = Object.fromEntries(lines.map((l) => [l.key, l]));
  assert.deepEqual(
    ['overflow', 'nightly', 'tierless', 'idle', 'departed'].map((k) => byKey[k].unit),
    ['usd', 'window_share', 'unknown', 'window_share', 'usd'],
    'a subscription account whose tier the table sizes charts in window share; one whose tier it does not size is tier unknown',
  );
  assert.deepEqual([byKey.tierless.fiveHourPeak, byKey.tierless.fiveHourPerHour, byKey.tierless.sevenDayPerHour, byKey.tierless.usdPerMinute], [null, null, null, 0], 'a tier-unknown account has no figure to chart');
  assert.deepEqual([byKey.idle.unit, byKey.idle.fiveHourPeak, byKey.idle.fiveHourPerHour], ['window_share', null, null], 'a sized account with no traffic in range priced none, so it reads a dash');
  assert.ok(d.foldBurn(answered.map((p) => ({ ...p, group: `k-${p.group}` })), 'key', 3_600, NONE).lines.every((l) => l.unit === 'usd'));
});

test('the burn fold keeps the groups that burn the most tokens and sums the rest as other', () => {
  const answered = Array.from({ length: 9 }, (_, i) => point(0, `s${i}`, { cachedTokens: (i + 1) * 60, usd: 0.6 }));
  const { lines, rows } = d.foldBurn(answered, 'session', 60, NONE);
  assert.deepEqual(lines.map((l) => l.key), ['s8', 's7', 's6', 's5', 's4', 's3', 's2', 'other']);
  assert.equal(lines.at(-1).label, 'Other');
  assert.deepEqual([rows[0].cached.at(-1), rows[0].usd.at(-1)], [60 + 120, 1.2], 'the two smallest sessions, summed');
  assert.deepEqual([rows[0].fiveHour.at(-1), rows[0].sevenDay.at(-1)], [null, null], 'shares of different windows do not sum');
  assert.deepEqual(d.foldBurn([point(0, '-', { cachedTokens: 60 })], 'key', 60, NONE).lines.map((l) => l.label), ['unattributed']);
});

test('under the account grouping the rest folds into one other per unit, so no dollars hide behind a window share', () => {
  // Seven accounts burn the most; the three smallest are a priced subscription, a tier-unknown one and an
  // API key.
  const kept = Array.from({ length: 7 }, (_, i) => point(0, `u${i}`, { cachedTokens: 1_000 * (i + 2), fiveHourShare: 0.001, unit: 'window_share' }));
  const answered = [
    ...kept,
    point(0, 'small-plan', { cachedTokens: 120, fiveHourShare: 0.01, sevenDayShare: 0.002, unit: 'window_share' }),
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
  assert.deepEqual([rows[0].fiveHour[7], rows[0].sevenDay[7]], [null, null], 'a folded rest draws no share');
  const oneUnit = d.foldBurn([...kept, point(0, 'small-key', { cachedTokens: 60, unit: 'usd', usd: 0.6 }), point(0, 'tiny-key', { cachedTokens: 30, unit: 'usd', usd: 0.3 })], 'account', 60, sized);
  assert.deepEqual(oneUnit.lines.slice(7).map((l) => l.label), ['Other (dollars)'], 'a rest of one unit folds into that unit alone');
});

test('an idle bucket reads zero only on a window that priced traffic, and a window with none stays null', () => {
  const plan = (at, over) => point(at * HOUR, 'primary', { unit: 'window_share', ...over });
  const { lines, rows } = d.foldBurn(
    [plan(0, { fiveHourShare: 0.08, tokens: 100 }), plan(1, { tokens: 100 }), plan(2, { tokens: 0 })],
    'account',
    3_600,
    new Set(['primary']),
  );
  assert.deepEqual(rows.map((r) => r.fiveHour[0]), [0.08, null, 0], 'a priced bucket, one whose traffic went unpriced, and an idle one');
  assert.deepEqual(rows.map((r) => r.sevenDay[0]), [null, null, null], 'the idle bucket stays null on a window no bucket priced');
  assert.ok(Math.abs(lines[0].fiveHourPerHour - 0.04) < 1e-12, 'the mean over the priced and the idle bucket');
  assert.equal(lines[0].sevenDayPerHour, null);
  assert.deepEqual(lines[0].unpriced, { fiveHour: true, sevenDay: true });
});

test('a bucket whose rows carried no token is idle whatever share its rows answered', () => {
  // A failed request books a row with no token, which a listed model prices at zero on either window.
  const plan = (at, over) => point(at * HOUR, 'primary', { unit: 'window_share', ...over });
  const { lines, rows } = d.foldBurn([plan(0, { fiveHourShare: 0.08, tokens: 100 }), plan(1, { fiveHourShare: 0, sevenDayShare: 0, tokens: 0 })], 'account', 3_600, new Set(['primary']));
  assert.deepEqual(rows.map((r) => r.fiveHour[0]), [0.08, 0]);
  assert.deepEqual(rows.map((r) => r.sevenDay[0]), [null, null], 'no bucket priced traffic on the 7-day window, so its legend reads a dash rather than 0%');
  assert.equal(lines[0].sevenDayPerHour, null);
});

test('a bucket with cache writes and output alone is not idle', () => {
  const plan = (at, over) => point(at * HOUR, 'primary', { unit: 'window_share', ...over });
  const { lines, rows } = d.foldBurn([plan(0, { fiveHourShare: 0.02, uncachedTokens: 10 }), plan(1, { cachedTokens: 0, tokens: 50, uncachedTokens: 0 })], 'account', 3_600, new Set(['primary']));
  assert.deepEqual(rows.map((r) => r.fiveHour[0]), [0.02, null], 'its traffic went unpriced rather than absent');
  assert.equal(lines[0].unpriced.fiveHour, true);
});

test('a known tier whose every share is null is a planned line listed as model unlisted', () => {
  // A Max 20x account whose range traffic ran on a model the cost model has no weight for throughout.
  const answered = [point(0, 'a', { tokens: 300, uncachedTokens: 300, unit: 'window_share' }), point(HOUR, 'a', { tokens: 200, uncachedTokens: 200, unit: 'window_share' })];
  const [line] = d.foldBurn(answered, 'account', 3_600, new Set(['a'])).lines;
  assert.deepEqual([line.unit, line.fiveHourPerHour, line.sevenDayPerHour, line.unpriced], ['window_share', null, null, { fiveHour: true, sevenDay: true }]);
  assert.equal(d.foldBurn(answered, 'account', 3_600, NONE).lines[0].unit, 'unknown', 'the same traffic on an account whose tier the cost model does not size');
});

test('a known tier with only a priced 7-day share keeps its 7-day series', () => {
  const answered = [0, 1].map((h) => point(h * HOUR, 'a', { sevenDayShare: 0.01, tokens: 100, uncachedTokens: 100, unit: 'window_share' }));
  const [line] = d.foldBurn(answered, 'account', 3_600, new Set(['a'])).lines;
  assert.equal(line.unit, 'window_share');
  assert.notEqual(line.sevenDayPerHour, null);
  assert.equal(line.fiveHourPerHour, null);
  assert.deepEqual(line.unpriced, { fiveHour: true, sevenDay: false });
});

test('a null slot is unpriced only with tokens behind it, so an idle slot on a never-priced window is not', () => {
  // The account prices its 5-hour window and never its 7-day one, so both of its 7-day slots read null.
  const plan = (at, over) => point(at * HOUR, 'primary', { unit: 'window_share', ...over });
  const { lines, rows } = d.foldBurn([plan(0, { fiveHourShare: 0.02, uncachedTokens: 10 }), plan(1, {})], 'account', 3_600, new Set(['primary']));
  const [busy, idle] = rows;
  assert.deepEqual([busy.sevenDay[0], idle.sevenDay[0], idle.fiveHour[0]], [null, null, 0]);
  assert.equal(d.slotUnpriced(busy.sevenDay[0], busy.tokens[0]), true, 'traffic the table could not price');
  assert.equal(d.slotUnpriced(idle.sevenDay[0], idle.tokens[0]), false, 'an idle slot has no traffic to list as model unlisted');
  assert.equal(d.slotUnpriced(idle.fiveHour[0], idle.tokens[0]), false);
  assert.deepEqual(lines[0].unpriced, { fiveHour: false, sevenDay: true }, 'the legend reads the slots the same way');
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
  // The hour before the peak band opens in Los Angeles on Wednesday 2026-09-16. Primary is the story's Max 20x.
  const edge = d.bandNow(costs, Date.parse('2026-09-16T10:00:00Z')).at;
  const hour = edge - HOUR;
  const tier = 'default_claude_max_20x';
  const usageOf = (input) => ({ cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: 0, input, output: 0 });
  // A row carries its requests' shares, each priced at its completion, which can land past the hour it arrived in.
  const sharesAt = (model, input, at) => Object.fromEntries(['five_hour', 'seven_day'].map((w) => [w, d.shareOf(costs, w, tier, model, usageOf(input), at)]));
  const equivalentsAt = (model, input, at) => Object.fromEntries(['five_hour', 'seven_day'].map((w) => [w, d.equivalentOf(costs, w, model, usageOf(input), at)?.equivalent ?? null]));
  const row = (model, uncachedInput, completedAt = hour) => ({ accountId: 'primary', costUsd: 0, equivalents: equivalentsAt(model, uncachedInput, completedAt), estimated: false, funding: 'plan', hour: new Date(hour).toISOString(), keyId: 'desktop', model, requestClass: 'main', sessionId: 's', shares: sharesAt(model, uncachedInput, completedAt), tokens: { cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: 0, output: 0, uncachedInput } });
  const range = { from: new Date(hour).toISOString(), to: new Date(hour + HOUR).toISOString() };
  const shareOf = (rows) => d.burnOf(rows, range, 'account').points.find((p) => p.group === 'primary' && Date.parse(p.at) === hour).fiveHourShare;
  // A request that arrived before the band opened and completed inside it, so the band's factor prices it.
  const priced = row('claude-opus-5-5', 1_000_000, edge + 60_000);
  const want = priced.shares.five_hour;
  const atTheHour = d.shareOf(costs, 'five_hour', tier, 'claude-opus-5-5', usageOf(1_000_000), hour);
  assert.ok(want !== null && atTheHour !== null && want > atTheHour, 'the shipped model prices Opus 5.5 on Max 20x, higher inside the band');
  // A failed request on Sonnet 4.5, which the cost model has no weight for, carries no token.
  const failed = row('claude-sonnet-4-5', 0);
  for (const rows of [[priced, failed], [failed, priced]]) assert.ok(Math.abs(shareOf(rows) - want) < 1e-12, 'a row with no token leaves the priced share, the row\'s own and not its tokens\' at the hour');
  assert.equal(shareOf([failed, row('claude-opus-5-5', 0)]), null, 'rows with no token alone are an idle bucket, not a share of zero');
  const unpriced = row('claude-opus-5', 1_000);
  for (const rows of [[priced, unpriced], [unpriced, priced]]) assert.equal(shareOf(rows), null, 'a row the cost model cannot price nulls the window');
});

test('an hour of two requests in different bands sums the per-request shares at their completions', async () => {
  const api = new d.MockApi();
  await api.login('admin');
  const costs = (await api.getSettings()).subscriptionCosts;
  const tierOf = new Map(d.ACCOUNTS.map((a) => [a.id, a.subscription?.rateLimitTier]));
  // A plan-funded inference the cost model prices, as the template the two rows are written from.
  const template = [...d.requestById.values()].find((r) => r.kind === 'inference' && r.status === 200 && r.attempts.length === 1 && tierOf.get(r.accountId ?? '') !== undefined && d.fundingOf(r.attempts[0]) === 'plan' && d.isWeighted(costs, r.canonicalModel));
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
  const share = (r, at) => d.shareOf(costs, 'five_hour', tier, r.canonicalModel, r.usage, at);
  const completion = (r) => Date.parse(r.at) + r.durationMs;
  const got = hours[0].shares.five_hour;
  const want = share(crossing, completion(crossing)) + share(before, completion(before));
  assert.ok(Math.abs(got - want) < 1e-12, `the hour reads ${got} against the requests' own ${want}`);
  const atArrivals = share(crossing, Date.parse(crossing.at)) + share(before, Date.parse(before.at));
  assert.ok(Math.abs(got - atArrivals) > 1e-9, 'a request priced at its arrival reads the other band');
  const summed = usage(650_000, 29_000);
  for (const at of [completion(crossing), completion(before)]) {
    assert.ok(Math.abs(got - d.shareOf(costs, 'five_hour', tier, template.canonicalModel, summed, at)) > 1e-9, 'the summed tokens priced at one instant read one band for both');
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

const sizedAccount = (id, rateLimitTier) => ({ id, kind: rateLimitTier === null ? 'api_key' : 'oauth', subscription: rateLimitTier === null ? null : { rateLimitTier } });
const SIZED_ACCOUNTS = [sizedAccount('stored', 'default_claude_max_20x'), sizedAccount('bare', 'claude_max_20x'), sizedAccount('team', 'claude_team'), sizedAccount('key', null)];
const SIZED_COSTS = { ...EMPTY, capacity: { default_claude_max_20x: { fiveHour: 1, sevenDay: 1 } } };
// Pro listed with a 7-day capacity alone, Max 5x with a 5-hour capacity alone.
const ONE_WINDOW = { ...SIZED_COSTS, capacity: { ...SIZED_COSTS.capacity, default_claude_ai: { fiveHour: null, sevenDay: 1 }, default_claude_max_5x: { fiveHour: 1, sevenDay: null } } };
const ONE_WINDOW_ACCOUNTS = [...SIZED_ACCOUNTS, sizedAccount('pro', 'default_claude_ai'), sizedAccount('max5', 'default_claude_max_5x')];

test('an account is sized for the plan chart when the cost model lists its stored tier, whatever windows it has a capacity on', () => {
  assert.deepEqual([...d.sizedAccounts(SIZED_ACCOUNTS, SIZED_COSTS)].sort(), ['bare', 'stored']);
  assert.deepEqual(
    [...d.sizedAccounts(ONE_WINDOW_ACCOUNTS, ONE_WINDOW)].sort(),
    ['bare', 'max5', 'pro', 'stored'],
    'a listed tier with no capacity on a window draws that window as model unlisted, never as tier unknown',
  );
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

test('the burn fold marks the slots whose bucket priced through the model tier fallback, and each line holding one', () => {
  const plan = (at, group, over) => point(at * HOUR, group, { fiveHourShare: 0.01, sevenDayShare: 0.001, tokens: 100, uncachedTokens: 100, unit: 'window_share', ...over });
  const { lines, rows } = d.foldBurn([plan(0, 'primary', { estimated: true }), plan(1, 'primary'), plan(0, 'secondary'), plan(1, 'secondary')], 'account', 3_600, new Set(['primary', 'secondary']));
  const [primary, secondary] = ['primary', 'secondary'].map((k) => lines.findIndex((l) => l.key === k));
  assert.deepEqual(rows.map((r) => r.estimated[primary]), [true, false], 'the bucket the route marked, and not the next one');
  assert.deepEqual(rows.map((r) => r.estimated[secondary]), [false, false]);
  assert.deepEqual([lines[primary].estimated, lines[secondary].estimated], [true, false], 'the legend lists the estimate for the line that holds one');
  const many = Array.from({ length: 9 }, (_, i) => plan(0, `s${i}`, { cachedTokens: (i + 1) * 60, estimated: true }));
  const folded = d.foldBurn(many, 'session', 60, NONE);
  assert.deepEqual([folded.lines.at(-1).other, folded.lines.at(-1).estimated, folded.rows[0].estimated.at(-1)], [true, false, false], 'a folded rest draws no plan line, so it carries no mark');
  assert.ok(folded.lines.slice(0, -1).every((l) => l.estimated), 'every kept line keeps its own');
});

test('the plan cards read the 20x equivalents under any grouping, and the model week beside the two shared windows', () => {
  const p = (at, group, over) => point(at * HOUR, group, { fiveHourEquivalent: 0.02, modelWeekEquivalent: null, sevenDayEquivalent: 0.004, tokens: 100, uncachedTokens: 100, ...over });
  const { lines, rows } = d.foldBurn([p(0, 'desktop', { modelWeekEquivalent: 0.008 }), p(1, 'desktop'), p(0, 'laptop')], 'key', 3_600, NONE, d.MAX_BURN_LINES, undefined, undefined, { figure: 'equivalent' });
  const [desktop, laptop] = ['desktop', 'laptop'].map((k) => lines.findIndex((l) => l.key === k));
  assert.deepEqual(rows.map((r) => [r.fiveHour[desktop], r.sevenDay[desktop]]), [[0.02, 0.004], [0.02, 0.004]], 'the equivalents per hour, which no account tier reads');
  assert.deepEqual(rows.map((r) => r.week[desktop]), [0.008, 0], 'a bucket with traffic and none on a model with a week of its own reads zero there once the line priced the week');
  assert.ok(Math.abs(lines[desktop].weekPerHour - 0.004) < 1e-12);
  assert.deepEqual([rows.map((r) => r.week[laptop]), lines[laptop].weekPerHour], [[null, null], null], 'a line that never ran on such a model has no week to chart');
  assert.deepEqual([rows[1].fiveHour[laptop], lines[laptop].unpriced], [0, { fiveHour: false, sevenDay: false }], 'an idle bucket reads zero on a window the line priced');
  const unpriced = d.foldBurn([p(0, 'desktop', { fiveHourEquivalent: null, sevenDayEquivalent: null })], 'key', 3_600, NONE, d.MAX_BURN_LINES, undefined, undefined, { figure: 'equivalent' });
  assert.deepEqual(unpriced.lines[0].unpriced, { fiveHour: true, sevenDay: true }, 'traffic on a model of no known model tier prices no equivalent and lists as model unlisted');
  const shares = d.foldBurn([p(0, 'desktop')], 'key', 3_600, NONE);
  assert.deepEqual([shares.rows[0].fiveHour[0], shares.rows[0].sevenDay[0]], [null, null], 'the share figure reads the shares, which a grouping of several accounts never answers');
});

test('a filter naming one account sizes every group by that account, so its shares chart under any grouping', () => {
  const p = (group) => point(0, group, { fiveHourShare: 0.01, sevenDayShare: 0.002, tokens: 100, uncachedTokens: 100, unit: 'window_share' });
  const answered = [p('claude-opus-5-5'), p('claude-sonnet-4-6')];
  const fold = (sized, account) => d.foldBurn(answered, 'model', 3_600, sized, d.MAX_BURN_LINES, undefined, undefined, { account }).lines.map((l) => l.unit);
  assert.deepEqual(fold(new Set(['primary']), 'primary'), ['window_share', 'window_share'], "the named account's tier sizes each model's line");
  assert.deepEqual(fold(NONE, 'primary'), ['unknown', 'unknown'], 'an account whose tier the cost model does not size reads tier unknown');
  assert.deepEqual(fold(new Set(['primary']), null), ['usd', 'usd'], 'with no account named the grouping mixes accounts');
});

test('the key grouping names an earlier holder of a key id as the breakdown does', () => {
  const generation = 1_791_720_940_000;
  const { lines } = d.foldBurn([point(0, 'desktop', { uncachedTokens: 60 }), point(0, `desktop@${generation}`, { uncachedTokens: 30 })], 'key', 60, NONE);
  assert.deepEqual(lines.map((l) => l.label), ['desktop', d.earlierHolderLabel('desktop', generation)]);
});

test('a priced bucket between unpriced ones, or at an end beside one, is an isolated slot the plan chart marks', () => {
  const plan = (at, over) => point(at * HOUR, 'primary', { tokens: 100, uncachedTokens: 100, unit: 'window_share', ...over });
  const { rows } = d.foldBurn([plan(0), plan(1, { fiveHourShare: 0.05 }), plan(2)], 'account', 3_600, new Set(['primary']));
  const fiveHour = rows.map((r) => r.fiveHour[0]);
  assert.deepEqual(fiveHour, [null, 0.05, null], 'a priced hour between two whose traffic went unpriced');
  assert.deepEqual(d.isolatedSlots(fiveHour), [false, true, false], 'the line draws no segment through it, so it takes a dot');
  assert.deepEqual(d.isolatedSlots(rows.map((r) => r.sevenDay[0])), [false, false, false], 'a window that priced nothing has no slot to mark');
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
      return [{ id: 'primary', kind: 'oauth', subscription: rateLimitTier === null ? null : { rateLimitTier } }];
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
  const groups = d.groupsOf(d.ALL_ROLLUPS, { from: d.ALL_ROLLUPS[0].hour, groupBy: 'speed', to: d.ALL_ROLLUPS.at(-1).hour });
  assert.deepEqual(groups.map((g) => g.key), ['fast', 'standard']);
  const fast = d.ALL_ROLLUPS.filter((r) => r.speed === 'fast');
  assert.ok(fast.length > 0 && fast.every((r) => r.keyId === 'desktop' && r.model.includes('opus') && r.kind === 'inference'));
  const served = (rows) => rows.filter((r) => r.kind !== 'probe').reduce((n, r) => n + r.requests.incomplete + r.requests.ok + r.requests.rate_limited + r.requests.overloaded + r.requests.upstream_error + r.requests.client_gone, 0);
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
  const [all] = d.groupsOf(d.ALL_ROLLUPS, { from: d.ALL_ROLLUPS[0].hour, groupBy: 'all', to: d.ALL_ROLLUPS.at(-1).hour });
  assert.equal(all.probes.held, held, 'the mock route sums the probe rows it groups');
});

test("the mock prices a fast hour and a fast answer at the model's fast cells", async () => {
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
  const { requests } = await api.listRequests({ speeds: ['fast'] });
  assert.ok(requests.length > 0);
  // A price rounded to 1e-4 in two orders of operation can land one unit in the last place apart. The compare counts
  // whole units, since a float difference of one unit can read a hair over 1e-4.
  for (const r of requests) {
    const want = Math.round(priced(cells.get(r.model), r.usage) * 10_000);
    assert.ok(Math.abs(Math.round(r.costUsd * 10_000) - want) <= 1, `${r.id}: ${r.costUsd} against ${want / 10_000}`);
  }
});

test('the session fold files a request under the speed its serving attempt reported, else fast when it asked for fast, standard otherwise', () => {
  const rows = d.decodeRequestPage(fixture('requests')).requests;
  const served = (r) => r.attempts.at(-1)?.response.speed;
  const speedOf = (r) => served(r) ?? (r.shape.requestedSpeed === 'fast' ? 'fast' : 'standard');
  assert.deepEqual(d.foldGroups(rows, 'speed').map((g) => [g.key, g.requests]).sort(), [['fast', rows.filter((r) => speedOf(r) === 'fast').length], ['standard', rows.filter((r) => speedOf(r) !== 'fast').length]]);
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
  assert.equal(captured.userAgent, 'claude-cli/2.1.269 (external, cli)');
  const rows = [captured, { ...failed, userAgent: 'claude-cli/2.1.269 (external, sdk-cli)' }, { ...failed, id: 'r-curl', userAgent: 'curl' }, { ...failed, id: 'r-none', userAgent: null }];
  assert.deepEqual(d.foldGroups(rows, 'user_agent').map((g) => [g.key, g.requests]).sort(), [['-', 1], ['claude-cli/2.1.269', 2], ['other', 1]], 'a header without a product token files under other, and one never sent under the reserved id');
  const named = d.foldGroups(rows, 'user_agent', { agentGroups: ['claude-cli/2.1.269'] });
  assert.deepEqual(named.map((g) => [g.key, g.requests]), [['claude-cli/2.1.269', 2], ['other', 2]], 'a pattern list names its groups by the token, as the breakdown route matches them');
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

test('the cooldown move reason reads failed over and the other seven keep their words', () => {
  assert.deepEqual(d.PIN_MIGRATION_LABEL, {
    cooldown: 'failed over',
    drain: 'winding down',
    fast_lane: 'fast lane cooling',
    mark: 'account marked',
    overage: 'overage',
    return_to_plan: 'returned to plan',
    shed: 'shed',
    spent: 'window spent',
  });
  assert.equal(d.moveLabel('cooldown'), 'failed over', 'every reader of the map takes the word through moveLabel');
});

test('RULE_MOVE_REASONS names the five rule reasons and leaves the three failover reasons out', () => {
  assert.deepEqual([...d.RULE_MOVE_REASONS], ['drain', 'overage', 'return_to_plan', 'shed', 'spent']);
  const failovers = Object.keys(d.PIN_MIGRATION_LABEL).filter((reason) => !d.RULE_MOVE_REASONS.includes(reason));
  assert.deepEqual(failovers, ['cooldown', 'fast_lane', 'mark'], 'every reason the map names is a rule reason or a failover');
});

test('FAILOVER_MOVE_REASONS names the three failover reasons and no reason the map does not label', () => {
  assert.deepEqual([...d.FAILOVER_MOVE_REASONS], ['cooldown', 'fast_lane', 'mark']);
  assert.deepEqual([...d.RULE_MOVE_REASONS, ...d.FAILOVER_MOVE_REASONS].sort(), Object.keys(d.PIN_MIGRATION_LABEL).sort(), 'the two sets split the labeled reasons');
  assert.equal(d.FAILOVER_MOVE_REASONS.includes('unknown'), false, "the route's unknown reason is no failover");
});

/** A route-shaped account group, the captured one with the members a case sets. */
const accountGroup = (over = {}) => ({ ...d.decodeAnalyticsGroup(fixture('analytics_breakdown_accounts').rows[0]), ...over });

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
  const sum = (groups) => d.oneAccountShares(groups, (x) => x.costShares, (a, b) => a + b, plan);
  assert.deepEqual(sum([g({ five_hour: 0.25, seven_day: 0.125, seven_day_fable: 0.0625 }), g({ five_hour: 0.5, seven_day: 0.25 })]), { five_hour: 0.75, seven_day: 0.375, seven_day_fable: 0.0625 }, "each model's group adds to the account's own windows, the model week where a group ran on one");
  assert.deepEqual(sum([g({ five_hour: 0.25, seven_day: 0.125 }), g({ five_hour: 0.5 })]), { five_hour: 0.75 }, 'a group with plan requests that priced no figure on a window leaves it out, since a partial sum would read as the whole');
  assert.deepEqual(sum([g({ five_hour: 0.25, seven_day: 0.125 }), g({ five_hour: 0.5, seven_day: null })]), { five_hour: 0.75 }, 'and so does a window carried as null');
  assert.deepEqual(sum([g({ five_hour: 0.25, seven_day: 0.125 }), g({}, 0)]), { five_hour: 0.25, seven_day: 0.125 }, 'a group of billed rows alone carries no window to sum');
  assert.equal(sum([g(null), g(null)]), null, 'no group answered a share map');
  const probes = d.oneAccountShares([accountGroup({ probeShares: { seven_day: { cost: 0.25, wasted: 0.125 } } }), accountGroup({ probeShares: {} })], (x) => x.probeShares, (a, b) => ({ cost: a.cost + b.cost, wasted: a.wasted === null || b.wasted === null ? null : a.wasted + b.wasted }));
  assert.deepEqual(probes, { seven_day: { cost: 0.25, wasted: 0.125 } }, 'a group without probes adds none, whatever its plan requests');
});

test('the model week is labeled by the key the share map carries, so a second model week reads under its own name', () => {
  assert.equal(d.weekLabelOf('seven_day_fable'), '7-day Fable');
  assert.equal(d.weekLabelOf('seven_day_opus'), 'seven_day_opus', 'a week the console has no label for reads under its key');
  assert.equal(d.weekLabelOf(undefined), '7-day Fable', 'no map names a week');
  assert.equal(d.weekColumnOf([accountGroup({ costShares: { five_hour: 0.2, seven_day: 0.08, seven_day_opus: 0.04 } }), accountGroup({ costShares: null })]), 'seven_day_opus');
  assert.equal(d.weekColumnOf([accountGroup({ costShares: { five_hour: 0.2, seven_day: 0.08, seven_day_fable: 0.04 } }), accountGroup({ costShares: { five_hour: 0.1, seven_day: 0.05, seven_day_opus: 0.02 } })]), '7-day Fable, seven_day_opus', 'each distinct week once');
  assert.equal(d.weekColumnOf([accountGroup({ costShares: { five_hour: 0.2, seven_day: 0.08 } })]), '7-day Fable', 'a table of Sonnet groups keeps the Fable week column');
  assert.deepEqual(d.shareLines([{ at: '2026-09-16T11:00:00.000Z', groups: [accountGroup({ costShares: { five_hour: 0.1, seven_day: 0.05, seven_day_opus: 0.02 }, key: 'primary' })] }]).weeks, { primary: 'seven_day_opus' }, 'the lines name the week each account charts');
});

test('a share row labels each window and divides the 7-day share per plan request and per 1k plan output', () => {
  const base = accountGroup();
  const g = accountGroup({ costShares: { five_hour: 0.2, seven_day: 0.08, seven_day_fable: 0.04 }, planOutputTokens: 2000, planRequests: 4, requests: 10, tokens: { ...base.tokens, output: 5000 } });
  assert.deepEqual(d.shareRow(g), { fiveHour: '20%', perRequest: '2%', perThousand: '4%', sevenDay: '8%', week: '4%' }, 'the averages divide by the plan-funded rows, never by every request');
  assert.deepEqual(d.shareRow(accountGroup({ costShares: null, planOutputTokens: null, planRequests: null })), { fiveHour: null, perRequest: null, perThousand: null, sevenDay: null, week: null }, 'a group the route priced no share for reads no cell');
  assert.deepEqual(d.shareRow(accountGroup({ costShares: { five_hour: 0.2, seven_day: 0.08 }, planOutputTokens: null, planRequests: null })), { fiveHour: '20%', perRequest: null, perThousand: null, sevenDay: '8%', week: null }, 'null counts read null averages, and a Sonnet group names no week');
  assert.deepEqual(d.shareRow(accountGroup({ costShares: { five_hour: 0.2, seven_day: null, seven_day_fable: null }, planOutputTokens: 2000, planRequests: 4 })), { fiveHour: '20%', perRequest: null, perThousand: null, sevenDay: null, week: null }, 'an unpriced window reads null, and so do the averages over it');
  assert.deepEqual(d.shareRow(accountGroup({ costShares: { five_hour: 0.2, seven_day: 0.08 }, planOutputTokens: 0, planRequests: 0 })), { fiveHour: '20%', perRequest: null, perThousand: null, sevenDay: '8%', week: null }, 'a zero count divides nothing');
});

test("the share lines carry each account's 7-day and week shares per bucket with a gap for a missing one", () => {
  const at = (h) => new Date(Date.UTC(2026, 8, 16, h)).toISOString();
  const fable = accountGroup({ costShares: { five_hour: 0.2, seven_day: 0.08, seven_day_fable: 0.04 }, key: 'primary', label: 'primary' });
  const haiku = accountGroup({ costShares: { five_hour: 0.1, seven_day: 0.05 }, key: 'nightly', label: 'nightly' });
  const apiKey = accountGroup({ costShares: null, key: 'overflow', label: 'overflow' });
  const { accounts, data } = d.shareLines([
    { at: at(11), groups: [fable, haiku, apiKey] },
    { at: at(12), groups: [{ ...fable, costShares: { five_hour: 0.3, seven_day: 0.1, seven_day_fable: null } }, apiKey] },
  ]);
  assert.deepEqual(accounts, ['nightly', 'primary'], 'the accounts with a share map, an API key account left out');
  assert.deepEqual(
    data,
    [
      { at: Date.UTC(2026, 8, 16, 11), 'nightly:5h': 0.1, 'nightly:7d': 0.05, 'nightly:week': null, 'primary:5h': 0.2, 'primary:7d': 0.08, 'primary:week': 0.04 },
      { at: Date.UTC(2026, 8, 16, 12), 'nightly:5h': null, 'nightly:7d': null, 'nightly:week': null, 'primary:5h': 0.3, 'primary:7d': 0.1, 'primary:week': null },
    ],
    'a bucket an account has no group in, and a window it has no share on, read null, which the chart leaves as a gap',
  );
  assert.deepEqual(d.shareLines([{ at: at(11), groups: [apiKey] }]), { accounts: [], data: [{ at: Date.UTC(2026, 8, 16, 11) }], labels: {}, weeks: {} }, 'no share map, no line');
  const holder = accountGroup({ costShares: { five_hour: 0.1, seven_day: 0.05 }, key: 'desktop@1000', label: 'desktop (deleted)' });
  assert.deepEqual(d.shareLines([{ at: at(11), groups: [holder] }]).labels, { 'desktop@1000': 'desktop (deleted)' }, 'a line is named by its group label, the grouping the page reads, rather than its key');
});

test('a share line with one priced bucket between two gaps marks it as an isolated slot, which the chart draws as a dot', () => {
  const at = (h) => new Date(Date.UTC(2026, 8, 16, h)).toISOString();
  const primary = (costShares) => accountGroup({ costShares, key: 'primary', label: 'primary' });
  const { data } = d.shareLines([
    { at: at(11), groups: [primary(null)] },
    { at: at(12), groups: [primary({ five_hour: 0.1, seven_day: 0.05 })] },
    { at: at(13), groups: [primary(null)] },
  ]);
  assert.deepEqual(data.map((r) => r['primary:7d']), [null, 0.05, null], 'the priced bucket sits between two gaps');
  assert.deepEqual(d.isolatedSlots(data.map((r) => r['primary:7d'])), [false, true, false], 'so it is the one isolated slot');
  assert.deepEqual(d.isolatedSlots(data.map((r) => r['primary:week'])), [false, false, false], 'a window never priced marks none');
  const edge = d.shareLines([{ at: at(11), groups: [primary({ five_hour: 0.1, seven_day: 0.05 })] }, { at: at(12), groups: [primary(null)] }]).data;
  assert.deepEqual(d.isolatedSlots(edge.map((r) => r['primary:7d'])), [true, false], "the range's edge counts as a gap");
});

test('the session and non-session views agree on the share averages over one mixed-funding request set', () => {
  const plan = d.decodeRequestPage(fixture('requests')).requests.find((r) => r.id === 'r-captured');
  const attempt = plan.attempts[plan.attempts.length - 1];
  const shares = { five_hour: 0.25, seven_day: 0.125 };
  const rows = [
    { ...plan, costShares: shares, id: 'p-1', usage: { ...plan.usage, output: 100 } },
    { ...plan, costShares: shares, id: 'p-2', usage: { ...plan.usage, output: 300 } },
    { ...plan, attempts: [{ ...attempt, overageInUse: true }], costShares: null, id: 'o-1', usage: { ...plan.usage, output: 1000 } },
  ];
  const [folded] = d.foldGroups(rows, 'account');
  const base = accountGroup();
  const route = accountGroup({ costShares: { five_hour: 0.5, seven_day: 0.25 }, key: plan.accountId, label: plan.accountId, planOutputTokens: 400, planRequests: 2, requests: 3, tokens: { ...base.tokens, output: 1400 } });
  assert.equal(folded.requests, 3, 'the overage row counts as a request');
  assert.deepEqual([folded.planRequests, folded.planOutputTokens], [2, 400], 'and widens no share denominator');
  assert.deepEqual(d.shareRow(folded), d.shareRow(route), 'the fold and the route divide by the same plan-funded rows');
  assert.deepEqual(d.shareRow(folded), { fiveHour: '50%', perRequest: '12.5%', perThousand: '62.5%', sevenDay: '25%', week: null });
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

test('the sampled windows list the three windows and the extra-usage week only while a row holds it', () => {
  const t = Date.UTC(2026, 8, 16, 10);
  assert.deepEqual(d.sampledWindows([]), ['five_hour', 'seven_day', 'seven_day_fable'], 'the three the router reads are offered without a row');
  assert.deepEqual(d.sampledWindows([rollup('a', t, { seven_day_opus: windowSample() })]), ['five_hour', 'seven_day', 'seven_day_fable'], 'another model week the samples carry is not offered');
  assert.deepEqual(d.sampledWindows([rollup('a', t, {}), rollup('b', t + HOUR, { seven_day_overage_included: windowSample() })]), ['five_hour', 'seven_day', 'seven_day_fable', 'seven_day_overage_included'], 'the extra-usage week joins fourth while any row holds it');
});

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

test('an equivalent cell names its window, carries the mark of an estimate and reads a dash where none was priced', () => {
  const g = accountGroup();
  const { five_hour: fiveHour, seven_day: sevenDay } = g.costEquivalent;
  assert.equal(d.equivalentOrDash('five_hour', fiveHour), d.fmtEquivalent('five_hour', fiveHour));
  assert.equal(d.equivalentOrDash('seven_day', sevenDay, true), `~${d.fmtEquivalent('seven_day', sevenDay)}`, 'an estimated figure leads with the tilde');
  assert.equal(d.equivalentOrDash('seven_day_fable', g.costEquivalent.seven_day_fable, true), '-', 'a window the group priced nothing on reads a dash, which no estimate marks');
  assert.equal(d.equivalentOrDash('seven_day', null), '-');
});

test("the Cost section's head reads each position off one group of plan and billed rows", () => {
  const g = accountGroup();
  assert.ok(g.planRequests > 0 && g.requests > g.planRequests && g.billedUsd > 0 && g.costEquivalent.seven_day > 0, 'the captured group holds rows of both fundings');
  const equivalent = d.fmtEquivalent('seven_day', g.costEquivalent.seven_day);
  const billed = d.fmtUsd(g.billedUsd);
  const dollars = d.fmtUsd(g.costUsd);
  const native = d.costHeadOf('native', g);
  assert.deepEqual(native.tile, { figures: [{ text: equivalent, word: 'subscription' }, { text: billed, word: 'billed' }], label: 'Cost', note: d.NATIVE_SUBSET }, 'native stacks the 20x 7-day equivalent over the billed dollars');
  assert.equal(native.subset, d.NATIVE_SUBSET);
  assert.equal(native.meta, `${equivalent} and ${billed} in range`);
  assert.equal(native.estimated, false);
  // A dollar position reads the totals its funded read answered, so the head prints their dollars under its words.
  const all = d.costHeadOf('all', g);
  assert.deepEqual([all.tile, all.subset, all.meta], [{ figures: [{ text: dollars }], label: 'Estimated cost', note: 'at the effective price list' }, 'all requests', `${dollars} in range, all requests`]);
  const usd = d.costHeadOf('usd', g);
  assert.deepEqual([usd.tile, usd.subset, usd.meta], [{ figures: [{ text: dollars }], label: 'Billed dollars', note: d.UNIT_SUBSET.usd }, d.UNIT_SUBSET.usd, `${dollars} in range, billed`]);
  const plan = d.costHeadOf('equivalent', g);
  assert.deepEqual([plan.tile, plan.subset, plan.meta], [{ figures: [{ text: dollars }], label: 'Dollar equivalent', note: d.UNIT_SUBSET.equivalent }, d.UNIT_SUBSET.equivalent, `${dollars} in range, equivalent`]);
  // The shares read per account, so the head carries the meta alone and the table its own subtitle.
  assert.deepEqual(d.costHeadOf('share', g), { estimated: false, meta: 'shares per account', subset: null, tile: null });
});

test('native marks an estimated equivalent and leaves out a side with no rows, and no dollar figure is marked', () => {
  const g = accountGroup({ estimated: true });
  const equivalent = d.fmtEquivalent('seven_day', g.costEquivalent.seven_day);
  const billed = d.fmtUsd(g.billedUsd);
  const marked = d.costHeadOf('native', g);
  assert.equal(marked.meta, `~${equivalent} and ${billed} in range`);
  assert.deepEqual(marked.tile.figures, [{ text: `~${equivalent}`, word: 'subscription' }, { text: billed, word: 'billed' }]);
  assert.equal(marked.estimated, true);
  for (const unit of ['all', 'usd', 'equivalent']) assert.equal(d.costHeadOf(unit, g).estimated, false, `${unit} reads dollars, which no estimate prices`);
  const plan = d.costHeadOf('native', accountGroup({ billedUsd: 0, requests: g.planRequests }));
  assert.deepEqual([plan.tile.figures, plan.meta], [[{ text: equivalent, word: 'subscription' }], `${equivalent} in range`], 'a sum of plan rows alone reads the equivalent alone');
  const billedOnly = d.costHeadOf('native', accountGroup({ costEquivalent: {}, estimated: true, planRequests: 0 }));
  assert.deepEqual([billedOnly.tile.figures, billedOnly.meta, billedOnly.estimated], [[{ text: billed, word: 'billed' }], `${billed} in range`, false], 'a sum of billed rows alone reads the dollars alone, with no mark');
  // A fold of request rows counts no plan request outside the account grouping, so its figures tell the sides apart.
  const folded = d.costHeadOf('native', accountGroup({ planRequests: null }));
  assert.equal(folded.meta, `${equivalent} and ${billed} in range`);
});

test('a native table cell reads the short 20x form with the billed dollars under it, the full reading and the estimate on its title', () => {
  const g = accountGroup();
  const sevenDay = g.costEquivalent.seven_day;
  const full = d.fmtEquivalent('seven_day', sevenDay);
  assert.deepEqual(d.nativeCellOf('seven_day', sevenDay, false, g.billedUsd), { main: d.equivalentShort('seven_day', sevenDay), sub: `${d.fmtUsd(g.billedUsd)} billed`, title: `${full}, ${d.fmtUsd(g.billedUsd)} billed` });
  assert.deepEqual(d.nativeCellOf('seven_day', sevenDay, true), { main: `~${d.equivalentShort('seven_day', sevenDay)}`, sub: '', title: `${full}. ${d.ESTIMATED_TITLE}.` }, 'a plan-only cell marks its estimate');
  assert.deepEqual(d.nativeCellOf('seven_day', undefined, true, g.billedUsd), { main: d.fmtUsd(g.billedUsd), sub: '', title: `${d.fmtUsd(g.billedUsd)} billed` }, 'a cell with no plan row reads its billed dollars alone, unmarked');
  assert.deepEqual(d.nativeCellOf('seven_day', null, true, null), { main: '-', sub: '-', title: '' }, 'a side priced nothing reads a dash');
  assert.deepEqual(d.nativeCellOf('five_hour', undefined, false), { main: '-', sub: '', title: '' }, 'a cell with no row on either side');
});

test("a share note names the half that applies: the filter for one account's whole, the grouping or the filter for a share per group", () => {
  // The routes answer a named account's shares under any grouping, so a surface that reads one account's whole needs
  // the filter alone, and one that reads a share per group needs either half.
  assert.notEqual(d.ONE_ACCOUNT_SHARE_NOTE, d.SHARE_NOTE, "the Analytics notes are their own sentences");
  assert.ok(d.ONE_ACCOUNT_SHARE_NOTE.includes('Accounts filter') && !d.ONE_ACCOUNT_SHARE_NOTE.includes('grouping') && !d.ONE_ACCOUNT_SHARE_NOTE.includes('group by'), d.ONE_ACCOUNT_SHARE_NOTE);
  assert.ok(d.GROUPED_SHARE_NOTE.includes('Accounts filter') && d.GROUPED_SHARE_NOTE.includes('group by account'), d.GROUPED_SHARE_NOTE);
  assert.ok(d.ONE_ACCOUNT_SHARE_NOTE.startsWith(d.GROUPED_SHARE_NOTE.slice(0, d.GROUPED_SHARE_NOTE.indexOf(':') + 1)), 'the two notes open on one sentence');
});
