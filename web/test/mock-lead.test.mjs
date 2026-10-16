import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

const NOW = Date.parse('2026-09-16T14:30:00Z');
const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const WEEK = 168 * HOUR;
const at = (ms) => new Date(ms).toISOString();

test('a window leads its pace by its utilization less the share of it that has elapsed', () => {
  const lead = (resetsAt, end = null) => d.leadOf({ resetsAt, utilization: 0.5 }, WEEK, NOW, end);
  assert.equal(lead(at(NOW + WEEK * 0.9)).toFixed(2), '0.40', 'a tenth of the week gone');
  assert.equal(lead(at(NOW + WEEK * 0.4)).toFixed(2), '-0.10', 'six tenths of the week gone');
  assert.equal(lead(null), 0.5, 'a window with no reset reads nothing elapsed');
  assert.equal(lead(at(NOW + WEEK * 2)), 0.5, 'a reset more than a window out reads nothing elapsed');
  assert.equal(lead(at(NOW - MINUTE)), -0.5, 'a window past its reset reads fully elapsed');
  // The share runs to the earlier of the reset and the account's end, as the router's `elapsed_share_at` reads it.
  const reset = at(NOW + WEEK * 0.9);
  assert.equal(lead(reset, NOW + WEEK / 10).toFixed(2), '0.00', 'an end a tenth of the week out: half the span to it gone');
  assert.equal(lead(reset, NOW + WEEK * 0.9).toFixed(2), '0.40', 'an end at the reset reads the reset');
  assert.equal(lead(reset, NOW + WEEK * 3).toFixed(2), '0.40', 'an end past the reset reads the reset');
  assert.equal(lead(reset, NOW - WEEK / 10), -0.5, "an end at the window's start reads fully elapsed");
  assert.equal(lead(reset, NOW - WEEK), -0.5, "an end before the window's start reads fully elapsed");
  assert.equal(lead(null, NOW + MINUTE), 0.5, 'without a reset nothing has elapsed, whatever the end');
});
