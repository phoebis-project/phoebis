import assert from 'node:assert/strict';
import { test } from 'node:test';
// No case here renders, so the bundle loads with no window, as keys.test.mjs loads it.
import { sumTileOf } from './dist/ui.js';

test("a sum tile under native hands a failed week reading's failure on with no value, and a pending reading neither", () => {
  const sum = { billedUsd: 0, cost: 0, equivalent: null };
  assert.deepEqual(sumTileOf('native', sum, { failure: { text: 'HTTP 503', unpriced: false }, scale: null, week: 'max' }), { failure: { text: 'HTTP 503', unpriced: false }, value: null });
  assert.deepEqual(sumTileOf('native', sum, { failure: null, scale: null, week: 'max' }), { failure: null, value: null }, 'a reading still out');
});
