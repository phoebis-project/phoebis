import assert from 'node:assert/strict';
import { test } from 'node:test';
import { fmtUsd } from './dist/api.js';

test('a zero cost never carries a sign, whatever the wire sent', () => {
  assert.equal(fmtUsd(-0), '$0.00', 'an empty sum of f64 costs reaches the decoder as -0');
  assert.equal(fmtUsd(0), '$0.00');
  assert.equal(fmtUsd(-0.004), '-$0.0040', 'a real negative still reads as one');
  assert.equal(fmtUsd(1.5), '$1.50');
});
