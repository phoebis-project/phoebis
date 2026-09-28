import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

const HOUR = 3_600_000;
const point = (at, group, over = {}) => ({ at: new Date(at).toISOString(), cachedTokens: 0, group, meterTokens: 0, planPercent: null, uncachedTokens: 0, usd: 0, ...over });

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
  );
  assert.deepEqual(lines.map((l) => l.key), ['conversation', 'overhead']);
  assert.deepEqual([rows[0].cached, rows[0].uncached], [[600, 120], [60, 60]], 'a one-minute bucket reads its tokens as they are');
});

test('the burn fold reads every figure per minute of its bucket and the plan share per hour', () => {
  const { lines, rows } = d.foldBurn([point(0, 'business', { cachedTokens: 36_000, meterTokens: 7_200, planPercent: 0.3, uncachedTokens: 3_600 }), point(6 * HOUR, 'business', { planPercent: null })], 'account', 6 * 3_600);
  assert.deepEqual([rows[0].cached[0], rows[0].uncached[0], rows[0].meter[0]], [100, 10, 20], 'six hours of tokens over 360 minutes');
  assert.ok(Math.abs(rows[0].plan[0] - 0.05) < 1e-12, 'a 30% share of the window over six hours is 5% an hour');
  assert.equal(rows[1].plan[0], null, 'a bucket the route could not size stays unsized');
  assert.equal(lines[0].planPeak, rows[0].plan[0]);
  assert.ok(Math.abs(lines[0].tokensPerMinute - 55) < 1e-9, 'the range average spreads the tokens over both buckets');
});

test('an upstream burns in the unit its figures come in, and every other grouping in dollars', () => {
  const answered = [point(0, 'console', { usd: 1.2 }), point(0, 'personal', { meterTokens: 900 }), point(0, 'unfitted', { planPercent: null, meterTokens: 40 }), point(0, 'idle', { planPercent: 0 })];
  const { lines } = d.foldBurn(answered, 'account', 3_600);
  const byKey = Object.fromEntries(lines.map((l) => [l.key, l]));
  assert.deepEqual([byKey.console.unit, byKey.personal.unit, byKey.unfitted.unit, byKey.idle.unit], ['usd', 'meter_tokens', 'meter_tokens', 'meter_tokens'], 'a plan share or meter tokens mark a subscription upstream');
  assert.equal(byKey.unfitted.planPeak, null, 'a subscription upstream with no plan share at all has no fit yet');
  assert.equal(byKey.idle.planPeak, 0);
  assert.ok(d.foldBurn(answered.map((p) => ({ ...p, group: `k-${p.group}` })), 'key', 3_600).lines.every((l) => l.unit === 'usd'));
});

test('the burn fold keeps the groups that burn the most tokens and sums the rest as other', () => {
  const answered = Array.from({ length: 9 }, (_, i) => point(0, `s${i}`, { cachedTokens: (i + 1) * 60, usd: 0.6 }));
  const { lines, rows } = d.foldBurn(answered, 'session', 60);
  assert.deepEqual(lines.map((l) => l.key), ['s8', 's7', 's6', 's5', 's4', 's3', 's2', 'other']);
  assert.equal(lines.at(-1).label, 'Other');
  assert.deepEqual([rows[0].cached.at(-1), rows[0].usd.at(-1)], [60 + 120, 1.2], 'the two smallest sessions, summed');
  assert.equal(rows[0].plan.at(-1), null, 'shares of different windows do not sum');
  assert.deepEqual(d.foldBurn([point(0, '-', { cachedTokens: 60 })], 'key', 60).lines.map((l) => l.label), ['unattributed']);
});

test('under the upstream grouping the rest folds into one other per unit, so no dollars hide behind meter tokens', () => {
  // Seven upstreams burn the most; the two smallest are a subscription one and an API-key one.
  const kept = Array.from({ length: 7 }, (_, i) => point(0, `u${i}`, { cachedTokens: 1_000 * (i + 2), meterTokens: 10 }));
  const answered = [...kept, point(0, 'small-plan', { cachedTokens: 120, meterTokens: 30, planPercent: 0.01 }), point(0, 'small-key', { cachedTokens: 60, usd: 0.6 })];
  const { lines, rows } = d.foldBurn(answered, 'account', 60);
  const rest = lines.slice(7);
  assert.deepEqual(rest.map((l) => [l.label, l.unit, l.other]), [['Other (plan)', 'meter_tokens', true], ['Other (dollars)', 'usd', true]]);
  assert.equal(new Set(lines.map((l) => l.key)).size, lines.length, 'each line keeps a key of its own');
  assert.deepEqual([rows[0].meter[7], rows[0].usd[7], rows[0].cached[7]], [30, 0, 120], 'the plan side carries the subscription upstream alone');
  assert.deepEqual([rows[0].usd[8], rows[0].meter[8], rows[0].cached[8]], [0.6, 0, 60], 'the dollar side keeps the API-key upstream\'s dollars');
  assert.deepEqual([rows[0].plan[7], rows[0].plan[8]], [null, null], 'a folded rest draws no plan share');
  const oneUnit = d.foldBurn([...kept, point(0, 'small-key', { cachedTokens: 60, usd: 0.6 }), point(0, 'tiny-key', { cachedTokens: 30, usd: 0.3 })], 'account', 60);
  assert.deepEqual(oneUnit.lines.slice(7).map((l) => l.label), ['Other (dollars)'], 'a rest of one unit folds into that unit alone');
});

test('a fitted scale reads its drift as a signed share, and past 20% either way it is wide', () => {
  assert.deepEqual([d.driftLabel(1.25), d.driftLabel(0.5), d.driftLabel(1)], ['+25.0%', '-50.0%', '0.0%']);
  assert.deepEqual([d.driftWide(1.2), d.driftWide(1.21), d.driftWide(0.79), d.driftWide(0.8), d.driftWide(null)], [false, true, true, false, false]);
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
