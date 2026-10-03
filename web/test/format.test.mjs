import assert from 'node:assert/strict';
import { test } from 'node:test';
import { earlierHolderLabel, fmtAbsolute, fmtEndsIn, fmtHourTick, fmtMiB, fmtShare, fmtShortTime, fmtTime, fmtUsd, placementHit, setTimeFormat } from './dist/api.js';

test('a placement is a hit when the cache read what it promised: a content one its prefix, any other one anything', () => {
  const content = { depth: 3, prefixTokens: 130_000, rung: 'content' };
  assert.equal(placementHit(content, 120_000), false, 'below the promise is a miss');
  assert.equal(placementHit(content, 130_000), true, 'at the promise is a hit');
  assert.equal(placementHit(content, 140_000), true);
  assert.equal(placementHit(content, null), null, 'no read, no verdict');
  const fixed = { depth: 2, prefixTokens: 30_000, rung: 'static' };
  assert.equal(placementHit(fixed, 1), true, 'a static placement with any read is a hit');
  assert.equal(placementHit(fixed, 0), false);
  assert.equal(placementHit(fixed, null), null);
  const main = { depth: 0, prefixTokens: 0, rung: 'with_main' };
  assert.equal(placementHit(main, 5), true);
  assert.equal(placementHit(main, 0), false);
});

test('an instant in the first hour of a day reads 00 on the 24-hour clock and 12 AM on the 12-hour one, never 24', () => {
  // Local midnight, since every formatter prints in the browser's zone.
  const midnight = new Date(2026, 8, 17, 0, 5, 9).toISOString();
  assert.match(fmtAbsolute(midnight), /, 00:05:09$/);
  assert.equal(fmtTime(midnight), '00:05:09');
  assert.equal(fmtShortTime(midnight), '00:05');
  assert.equal(fmtHourTick(Date.parse(midnight)), '00:05');
  setTimeFormat('12h');
  try {
    assert.match(fmtAbsolute(midnight), /, 12:05:09\sAM$/);
    assert.match(fmtTime(midnight), /^12:05:09\sAM$/);
    assert.match(fmtShortTime(new Date(2026, 8, 17, 13, 5).toISOString()), /^01:05\sPM$/);
  } finally {
    setTimeFormat('24h');
  }
});

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

test('an earlier holder reads its creation minute in UTC, and one with no creation time reads deleted alone', () => {
  assert.equal(earlierHolderLabel('desktop', 34_305_000), 'desktop (deleted, created 1970-01-01 09:31 UTC)', 'the seconds drop, the hour and the minute stay');
  assert.equal(earlierHolderLabel('desktop', 0), 'desktop (deleted)', 'generation 0 is a row filed before the generation column');
  assert.equal(earlierHolderLabel('desktop', Number('abc')), 'desktop (deleted)', 'a generation that is no number');
  assert.equal(earlierHolderLabel('desktop', Infinity), 'desktop (deleted)');
});

test('a window share reads as a percent to three significant digits with no trailing zero', () => {
  assert.equal(fmtShare(0.0841), '8.41%');
  assert.equal(fmtShare(0.00219), '0.219%');
  assert.equal(fmtShare(0.125), '12.5%');
  assert.equal(fmtShare(0.0061), '0.61%', 'a trailing zero drops');
  assert.equal(fmtShare(0.0000571), '0.00571%', 'a small share keeps its three digits');
});

test('a byte count reads in MiB, a whole number without decimals and any other to one decimal', () => {
  assert.equal(fmtMiB(268_435_456), '256 MiB');
  assert.equal(fmtMiB(1_572_864), '1.5 MiB');
  assert.equal(fmtMiB(0), '0 MiB');
});
