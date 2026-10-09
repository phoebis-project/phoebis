import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

test('an account takes the slot of its id in the registry order, whatever order the ids were given', () => {
  const slotOf = d.accountSlots(['c', 'a', 'b']);
  assert.equal(slotOf('a'), 0);
  assert.equal(slotOf('b'), 1);
  assert.equal(slotOf('c'), 2);
});

test('the ninth account cycles to the first slot and an id outside the list has none', () => {
  const slotOf = d.accountSlots(['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i']);
  assert.equal(slotOf('i'), 0);
  assert.equal(slotOf('h'), 7);
  assert.equal(slotOf('z'), null);
});

test('a slot draws its series ink and no slot draws the muted ink', () => {
  const ink = { muted: 'm', series: ['s0', 's1', 's2', 's3', 's4', 's5', 's6', 's7', 'm'] };
  assert.equal(d.accountColor(ink, 0), 's0');
  assert.equal(d.accountColor(ink, 3), 's3');
  assert.equal(d.accountColor(ink, null), 'm');
});
