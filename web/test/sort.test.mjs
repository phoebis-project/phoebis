import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

const names = (rows, dir) => d.orderRowsBy(rows, (r) => r.v, dir, (r) => r.n).map((r) => r.n);

test('a row without a figure reads after every row with one under either direction, and the direction flips the rows with one', () => {
  const rows = [
    { n: 'b', v: 2 },
    { n: 'c', v: null },
    { n: 'a', v: 1 },
  ];
  assert.deepEqual(names(rows, 'asc'), ['a', 'b', 'c']);
  assert.deepEqual(names(rows, 'desc'), ['b', 'a', 'c']);
});

test('a tie reads by name ascending under both directions', () => {
  const rows = [
    { n: 'b', v: 1 },
    { n: 'a', v: 1 },
  ];
  assert.deepEqual(names(rows, 'asc'), ['a', 'b']);
  assert.deepEqual(names(rows, 'desc'), ['a', 'b']);
});
