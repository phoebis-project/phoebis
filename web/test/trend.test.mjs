import assert from 'node:assert/strict';
import { test } from 'node:test';
import { joinTrend } from './dist/api.js';

// Minute offsets from a 5-minute boundary, so each sample's bucket reads off its offset.
const at = (minutes, seconds = 0) => new Date(Date.UTC(2026, 8, 16, 14, 30) + minutes * 60_000 + seconds * 1000).toISOString();
const series = (windowId, samples) => ({ accountId: 'business', samples: samples.map(([t, utilization]) => ({ at: t, utilization })), windowId });

test('samples on one bucket at different instants join', () => {
  const history = [
    series('5h', [
      [at(0, 10), 0.2],
      [at(5, 20), 0.3],
    ]),
    series('7d', [
      [at(4, 50), 0.5],
      [at(9, 59), 0.6],
    ]),
    series('7d_fable', [
      [at(0, 0), 0.1],
      [at(5, 0), 0.15],
    ]),
  ];
  assert.deepEqual(joinTrend(history, true, 1), [
    { at: Date.parse(at(0, 10)), u5: 20, u7: 50, uf: 10 },
    { at: Date.parse(at(5, 20)), u5: 30, u7: 60, uf: 15 },
  ]);
  // Without a Fable window the series stays out of the points even where samples exist.
  assert.deepEqual(
    joinTrend(history, false, 1).map((p) => p.uf),
    [null, null],
  );
});

test('a missing Fable bucket carries the previous value forward', () => {
  const history = [
    series('5h', [
      [at(0), 0.2],
      [at(5), 0.25],
      [at(10), 0.3],
    ]),
    series('7d', [
      [at(0), 0.4],
      [at(5), 0.41],
      [at(10), 0.42],
    ]),
    series('7d_fable', [
      [at(0), 0.12],
      [at(10), 0.125],
    ]),
  ];
  const points = joinTrend(history, true, 1);
  assert.deepEqual(
    points.map((p) => p.uf),
    [12, 12, 12.5],
  );
  assert.deepEqual(
    points.map((p) => p.u7),
    [40, 41, 42],
  );
});

test('a series that starts late is null before its first sample', () => {
  const history = [
    series('5h', [
      [at(0), 0.1],
      [at(5), 0.2],
      [at(10), 0.3],
      [at(15), 0.4],
    ]),
    series('7d', [
      [at(10), 0.5],
      [at(15), 0.6],
    ]),
    series('7d_fable', [[at(15), 0.75]]),
  ];
  const points = joinTrend(history, true, 1);
  assert.deepEqual(
    points.map((p) => [p.u7, p.uf]),
    [
      [null, null],
      [null, null],
      [50, null],
      [60, 75],
    ],
  );
  // The default keeps every third 5-hour sample, the first among them.
  assert.deepEqual(
    joinTrend(history, true).map((p) => p.at),
    [Date.parse(at(0)), Date.parse(at(15))],
  );
});
