import assert from 'node:assert/strict';
import { test } from 'node:test';
import { CACHE_MISS_REASONS, emptyCacheMissReasons, missLabel, missShort } from './dist/api.js';

test("a misses-by-reason header reads a short word apart from the panel's full label", () => {
  assert.notEqual(missShort('previous_message_not_found'), missLabel('previous_message_not_found'), 'the header reads its short word');
});

test('a comparison still running reads Pending, listed after the three non-answers', () => {
  assert.deepEqual(CACHE_MISS_REASONS.slice(-4), ['unavailable', 'not_requested', 'not_returned', 'pending']);
  assert.equal(missLabel('pending'), 'Pending');
  assert.equal(missShort('pending'), 'Pending');
  assert.equal(emptyCacheMissReasons().pending, 0, 'every column draws from zero');
});

test('a reason the console has no word for reads its own name, and an inherited member name is no word', () => {
  assert.equal(missShort('later_reason'), 'later_reason');
  assert.equal(missShort('__proto__'), '__proto__');
  assert.equal(missShort('constructor'), 'constructor');
});
