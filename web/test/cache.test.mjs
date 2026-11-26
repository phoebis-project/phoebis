import assert from 'node:assert/strict';
import { test } from 'node:test';
import { missLabel, missShort } from './dist/api.js';

test("a misses-by-reason header reads a short word apart from the panel's full label", () => {
  assert.notEqual(missShort('previous_message_not_found'), missLabel('previous_message_not_found'), 'the header reads its short word');
});

test('a reason the console has no word for reads its own name, and an inherited member name is no word', () => {
  assert.equal(missShort('later_reason'), 'later_reason');
  assert.equal(missShort('__proto__'), '__proto__');
  assert.equal(missShort('constructor'), 'constructor');
});
