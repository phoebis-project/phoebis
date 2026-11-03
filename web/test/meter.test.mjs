import assert from 'node:assert/strict';
import { test } from 'node:test';

const { judged, latestPerWindow, meterGroups, SPARK, sparkOf, windowWord } = await import('./dist/api.js');

const HOUR = 3_600_000;
const DAY = 24 * HOUR;
const T = Date.parse('2026-09-16T14:00:00Z');

/** A reconciled window of the test's own, its ratio the rise over the share as the route sends it. */
const row = (accountId, window, endMs, observedRise, predictedShare) => ({
  accountId,
  computedAt: new Date(endMs + 37 * 60_000).toISOString(),
  estimated: 0,
  firstReadingAt: new Date(endMs - 4 * HOUR).toISOString(),
  lastReadingAt: new Date(endMs - 60_000).toISOString(),
  observedRise,
  predictedShare,
  ratio: observedRise / predictedShare,
  requests: 10,
  window,
  windowEnd: new Date(endMs).toISOString(),
});

// Two accounts over three window kinds, out of order, so the fold's own order is what the cases read.
const ROWS = [
  row('acct-b', 'seven_day', T - 4 * DAY, 0.7, 0.5),
  row('acct-a', 'five_hour', T - 14 * HOUR, 0.4, 0.6),
  row('acct-a', 'seven_day_fable', T - 3 * DAY, 0.2, 0.2),
  row('acct-b', 'five_hour', T - HOUR, 0.3, 0.15),
  row('acct-a', 'five_hour', T - 8 * HOUR, 0.03, 0.02),
  row('acct-a', 'seven_day', T - 10 * DAY, 0.5, 0.52),
  row('acct-a', 'five_hour', T - 2 * HOUR, 0.5, 0.5),
  row('acct-b', 'seven_day', T - 11 * DAY, 0.5, 0.5),
  row('acct-a', 'seven_day', T - 3 * DAY, 0.6, 0.6),
];
const ends = (rows) => rows.map((r) => r.windowEnd);
const newestFirst = (rows) => [...rows].sort((x, y) => Date.parse(y.windowEnd) - Date.parse(x.windowEnd));

test('a window reads its Usage card word, and a window the console does not know reads under its key', () => {
  assert.deepEqual(['five_hour', 'seven_day', 'seven_day_fable', 'seven_day_opus'].map(windowWord), ['5-hour', '7-day', 'Fable', 'seven_day_opus']);
});

test('the band judges a ratio once the rise and the priced share each reach 5%', () => {
  assert.equal(judged({ observedRise: 0.05, predictedShare: 0.05 }), true, 'the floor itself is judged');
  assert.equal(judged({ observedRise: 0.049, predictedShare: 0.4 }), false, 'a rise under the floor');
  assert.equal(judged({ observedRise: 0.4, predictedShare: 0.049 }), false, 'a priced share under the floor');
});

test('the groups fold by account and window kind, the weekly windows apart, each newest first with its counts', () => {
  const { fiveHour, weekly } = meterGroups(ROWS);
  assert.deepEqual(
    weekly.map((g) => [g.accountId, g.window]),
    [
      ['acct-a', 'seven_day'],
      ['acct-a', 'seven_day_fable'],
      ['acct-b', 'seven_day'],
    ],
    'the accounts by id, and inside an account the 7-day window before the model week',
  );
  assert.deepEqual(
    fiveHour.map((g) => [g.accountId, g.window]),
    [
      ['acct-a', 'five_hour'],
      ['acct-b', 'five_hour'],
    ],
  );
  // A row is judged once its rise and its priced share each reach 5%, and off where its ratio then leaves 0.8 to 1.25.
  const iso = (ms) => new Date(ms).toISOString();
  assert.deepEqual(
    [...weekly, ...fiveHour].map((g) => [g.accountId, g.window, ends(g.rows), g.latest.windowEnd, g.judged, g.off, g.leftOut]),
    [
      ['acct-a', 'seven_day', [iso(T - 3 * DAY), iso(T - 10 * DAY)], iso(T - 3 * DAY), 2, 0, 0],
      ['acct-a', 'seven_day_fable', [iso(T - 3 * DAY)], iso(T - 3 * DAY), 1, 0, 0],
      ['acct-b', 'seven_day', [iso(T - 4 * DAY), iso(T - 11 * DAY)], iso(T - 4 * DAY), 2, 1, 0],
      ['acct-a', 'five_hour', [iso(T - 2 * HOUR), iso(T - 8 * HOUR), iso(T - 14 * HOUR)], iso(T - 2 * HOUR), 2, 1, 1],
      ['acct-b', 'five_hour', [iso(T - HOUR)], iso(T - HOUR), 1, 1, 0],
    ],
    'each group lists its rows newest first with its latest the newest, and counts the rows judged, off and left out',
  );
  const a5 = fiveHour[0];
  assert.deepEqual([a5.rows.length, a5.judged, a5.off, a5.leftOut, a5.latest.windowEnd], [3, 2, 1, 1, new Date(T - 2 * HOUR).toISOString()], "acct-a's 5-hour group: one row off, one under the floor");
  assert.deepEqual([weekly[2].off, weekly[2].judged], [1, 2], "acct-b's 7-day group: its older 1.4 off");
  const known = meterGroups([row('acct-a', 'seven_day_opus', T - DAY, 0.2, 0.2), ...ROWS]).weekly.map((g) => g.window);
  assert.deepEqual(known, ['seven_day', 'seven_day_fable', 'seven_day_opus', 'seven_day'], 'a model week the console does not know follows the known ones');
});

test("one account's latest windows: the newest of each kind, in the windows' order", () => {
  const mine = ROWS.filter((r) => r.accountId === 'acct-a');
  const latest = latestPerWindow(mine);
  assert.deepEqual(
    latest.map((r) => [r.window, r.windowEnd]),
    [
      ['five_hour', new Date(T - 2 * HOUR).toISOString()],
      ['seven_day', new Date(T - 3 * DAY).toISOString()],
      ['seven_day_fable', new Date(T - 3 * DAY).toISOString()],
    ],
  );
  assert.deepEqual(latestPerWindow([]), [], 'no row, no line');
});

test("the spark spans the 30 days to the answer's newest window, the ratio's log clipped to 0.5 and 2, the judged windows alone", () => {
  assert.deepEqual([SPARK.width, SPARK.height, SPARK.floor, SPARK.ceiling, SPARK.band], [120, 24, 0.5, 2, [0.8, 1.25]]);
  const rows = newestFirst([
    row('acct-a', 'five_hour', T, 0.4, 0.1),
    row('acct-a', 'five_hour', T - 10 * DAY, 0.5, 0.5),
    row('acct-a', 'five_hour', T - 20 * DAY, 0.03, 0.4),
    row('acct-a', 'five_hour', T - 30 * DAY, 0.1, 0.5),
  ]);
  const [group] = meterGroups(rows).fiveHour;
  const spark = sparkOf(group, T);
  const at = (p) => [Math.round(p.x * 10) / 10, Math.round(p.y * 10) / 10, p.off];
  assert.deepEqual(
    spark.points.map(at),
    [
      [3, 22, true],
      [79, 12, false],
      [117, 2, true],
    ],
    'oldest first: 0.2 clipped to the floor at the left edge, 1.0 on the middle line, 4 clipped to the ceiling at the right edge, the window under the 5% floor left out',
  );
  assert.equal(spark.title, '3 judged, 2 off, 1 unjudged');
  assert.equal(spark.dots, true, 'three judged windows draw as dots');

  const later = sparkOf(group, T + 6 * DAY);
  assert.ok(later.points[2].x < 117 && later.points[0].x === 3, "a group whose newest window ends before the answer's newest stops short of the right edge, one axis for every line");
});

test('a group of eight judged windows draws them as dots, and one of nine as a line with its off windows alone dotted', () => {
  const judgedRows = (n) => Array.from({ length: n }, (_, i) => row('acct-a', 'five_hour', T - i * DAY, i === 3 ? 0.3 : 0.5, i === 3 ? 0.15 : 0.5));
  const eight = sparkOf(meterGroups(judgedRows(8)).fiveHour[0], T);
  const nine = sparkOf(meterGroups(judgedRows(9)).fiveHour[0], T);
  assert.deepEqual([eight.points.length, eight.dots, eight.title], [8, true, '8 judged, 1 off, 0 unjudged']);
  assert.deepEqual([nine.points.length, nine.dots, nine.points.filter((p) => p.off).length, nine.title], [9, false, 1, '9 judged, 1 off, 0 unjudged']);
});
