import assert from 'node:assert/strict';
import { test } from 'node:test';
import { CACHE_MISS_LABEL, CACHE_MISS_REASONS, CACHE_MISS_SHORT, emptyCacheMissReasons, missLabel, missShort } from './dist/api.js';

test("the misses-by-reason headers read one short word per known reason, the full label staying the panel's", () => {
  assert.deepEqual(CACHE_MISS_SHORT, {
    messages_changed: 'History',
    model_changed: 'Model',
    none: 'None',
    not_requested: 'Not requested',
    pending: 'Pending',
    previous_message_not_found: 'No previous',
    system_changed: 'System',
    tools_changed: 'Tools',
    unavailable: 'Unavailable',
  });
  assert.deepEqual(Object.keys(CACHE_MISS_SHORT).sort(), [...CACHE_MISS_REASONS].sort(), 'every known reason has its word');
  for (const reason of CACHE_MISS_REASONS) assert.equal(missShort(reason), CACHE_MISS_SHORT[reason]);
  assert.equal(missLabel('previous_message_not_found'), CACHE_MISS_LABEL.previous_message_not_found, 'the full label stays');
});

test('a comparison still running reads Pending, listed after the two non-answers', () => {
  assert.deepEqual(CACHE_MISS_REASONS.slice(-3), ['unavailable', 'not_requested', 'pending']);
  assert.equal(missLabel('pending'), 'Pending');
  assert.equal(missShort('pending'), 'Pending');
  assert.equal(emptyCacheMissReasons().pending, 0, 'every column draws from zero');
});

test('a reason the console has no word for reads its own name, and an inherited member name is no word', () => {
  assert.equal(missShort('later_reason'), 'later_reason');
  assert.equal(missShort('__proto__'), '__proto__');
  assert.equal(missShort('constructor'), 'constructor');
});
