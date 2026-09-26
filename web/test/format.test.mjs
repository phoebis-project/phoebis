import assert from 'node:assert/strict';
import { test } from 'node:test';
import { fmtEndsIn, fmtUsd } from './dist/api.js';

test('a zero cost never carries a sign, whatever the wire sent', () => {
  assert.equal(fmtUsd(-0), '$0.00', 'an empty sum of f64 costs reaches the decoder as -0');
  assert.equal(fmtUsd(0), '$0.00');
  assert.equal(fmtUsd(-0.004), '-$0.0040', 'a real negative still reads as one');
  assert.equal(fmtUsd(1.5), '$1.50');
});

test('an end reads in whole days, then hours, then minutes, and one already past as ended', () => {
  const now = Date.parse('2026-09-16T14:30:00Z');
  const at = (ms) => new Date(now + ms).toISOString();
  const MINUTE = 60_000;
  const HOUR = 60 * MINUTE;
  const DAY = 24 * HOUR;
  assert.equal(fmtEndsIn(at(12 * DAY + 5 * HOUR), now), 'ends in 12 days');
  assert.equal(fmtEndsIn(at(DAY + 23 * HOUR), now), 'ends in 1 day', 'a part day never rounds up');
  assert.equal(fmtEndsIn(at(3 * HOUR + 20 * MINUTE), now), 'ends in 3 hours');
  assert.equal(fmtEndsIn(at(HOUR), now), 'ends in 1 hour');
  assert.equal(fmtEndsIn(at(45 * MINUTE), now), 'ends in 45 minutes');
  assert.equal(fmtEndsIn(at(20_000), now), 'ends in 1 minute', 'under a minute still names one');
  assert.equal(fmtEndsIn(at(-(2 * DAY + HOUR)), now), 'ended 2 days ago');
  assert.equal(fmtEndsIn(at(-5 * MINUTE), now), 'ended 5 minutes ago');
  assert.equal(fmtEndsIn(at(0), now), 'ended 1 minute ago', 'the end itself has passed');
});
