import assert from 'node:assert/strict';
import { test } from 'node:test';
import { transformationOrder } from './dist/api.js';

test('the order lists an unknown key once where two inputs hold it', () => {
  // Two groups' key lists end to end, as the by-group card hands them in, each holding the key the console does not
  // know.
  const known = 'thinking_dropped.model_binding_mismatch';
  const unknown = 'thinking_dropped.future_reason';
  assert.deepEqual(transformationOrder([known, unknown, unknown]), [known, unknown]);
});
