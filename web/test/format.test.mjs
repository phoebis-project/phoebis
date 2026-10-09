import assert from 'node:assert/strict';
import { test } from 'node:test';

// The holder label and the minute formatter print in the host's zone, so the file reads them in UTC whatever the host's
// zone, and `node --test` gives each file a process of its own.
process.env.TZ = 'UTC';

const { earlierHolderLabel, fmtAbsolute, fmtDayTick, fmtDuration, fmtEndsIn, fmtHours, fmtHourTick, fmtMiB, fmtMinute, fmtShare, fmtShortTime, fmtTime, fmtUsd, fmtUsdTick, placementHit, setTimeFormat, shortModel, undatedModel, usdTickDigits } = await import('./dist/api.js');

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

test('a day tick prints the date in the zone named, and the local time where the boundary is not midnight there', () => {
  // The day buckets start at 00:00 UTC, so a tick on that boundary is the date alone where the day turns there and
  // carries the local clock where it does not.
  const at = Date.parse('2026-09-16T00:00Z');
  assert.equal(fmtDayTick(at, 'UTC'), 'Sep 16');
  assert.equal(fmtDayTick(at, 'America/Los_Angeles'), 'Sep 15 17:00', 'the zone passed is the zone printed, with the hour the boundary falls at there');
  assert.equal(fmtDayTick(at, 'Asia/Kolkata'), 'Sep 16 05:30');
  assert.equal(fmtDayTick(at), 'Sep 16', "the browser's zone when none is named, UTC here");
  setTimeFormat('12h');
  try {
    assert.match(fmtDayTick(at, 'America/Los_Angeles'), /^Sep 15 05:00\sPM$/, 'the time format reaches the tick where the boundary carries a clock');
    assert.equal(fmtDayTick(at, 'UTC'), 'Sep 16', 'a boundary at midnight stays the date alone on either clock');
  } finally {
    setTimeFormat('24h');
  }
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

test("an earlier holder reads its creation minute in the viewer's zone, and one with no creation time reads deleted alone", () => {
  assert.equal(earlierHolderLabel('desktop', 34_305_000), 'desktop (deleted, created Jan 01, 1970, 09:31)', 'the seconds drop, the hour and the minute stay, under the time format');
  assert.equal(earlierHolderLabel('desktop', 0), 'desktop (deleted)', 'generation 0 is a row filed before the generation column');
  assert.equal(earlierHolderLabel('desktop', Number('abc')), 'desktop (deleted)', 'a generation that is no number');
  assert.equal(earlierHolderLabel('desktop', Infinity), 'desktop (deleted)');
  assert.equal(fmtMinute('2026-09-14T09:31:45Z'), 'Sep 14, 2026, 09:31', 'the shape of fmtAbsolute with the seconds dropped');
  assert.equal(fmtMinute(null), '-');
  setTimeFormat('12h');
  try {
    assert.match(fmtMinute('2026-09-14T09:31:45Z'), /^Sep 14, 2026, 09:31\sAM$/);
    assert.match(earlierHolderLabel('desktop', 34_305_000), /^desktop \(deleted, created Jan 01, 1970, 09:31\sAM\)$/, 'the label follows the time format as every other timestamp does');
  } finally {
    setTimeFormat('24h');
  }
});

test('a ceiling reads floored: the dialog in days, hours and minutes, the card in whole hours or minutes', () => {
  assert.deepEqual(
    [158_760_000, 255_960_000, 11_760_000, 2_760_000, 3_600_000].map(fmtDuration),
    ['1d 20h', '2d 23h', '3h 16m', '46m', '1h 0m'],
    'the break-evens at the production leads, each unit floored',
  );
  assert.equal(fmtHours(158_760_000), '44h', '44.1 hours never rounds up to 45');
  assert.equal(fmtHours(255_960_000), '71h');
  assert.equal(fmtHours(3_600_000), '1h', 'one hour reads in hours');
  assert.equal(fmtHours(3_599_999), '59m', 'a millisecond short of an hour reads in minutes');
  assert.equal(fmtHours(2_760_000), '46m');
});

test('a window share reads as a percent to three significant digits with no trailing zero', () => {
  assert.equal(fmtShare(0.0841), '8.41%');
  assert.equal(fmtShare(0.00219), '0.219%');
  assert.equal(fmtShare(0.125), '12.5%');
  assert.equal(fmtShare(0.0061), '0.61%', 'a trailing zero drops');
  assert.equal(fmtShare(0.0000571), '0.00571%', 'a small share keeps its three digits');
});

test('a dated model id reads undated, any other as sent', () => {
  assert.equal(undatedModel('claude-haiku-4-5-20251001'), 'claude-haiku-4-5', 'the date suffix the catalog lists drops');
  assert.equal(undatedModel('claude-opus-4-1'), 'claude-opus-4-1');
  assert.equal(undatedModel('claude-sonnet-4-5'), 'claude-sonnet-4-5');
  assert.equal(undatedModel('claude-sonnet-4-5-2025'), 'claude-sonnet-4-5-2025', 'a shorter number is no date');
  assert.equal(undatedModel('claude-sonnet-4-5-202510011'), 'claude-sonnet-4-5-202510011', 'nor is a longer one');
});

test('a short model id drops the claude- prefix and the date, and an id without the prefix keeps the rest', () => {
  assert.equal(shortModel('claude-haiku-4-5-20251001'), 'haiku-4-5', 'both the prefix and the date drop');
  assert.equal(shortModel('claude-sonnet-5-5'), 'sonnet-5-5');
  assert.equal(shortModel('opus-4-1'), 'opus-4-1', 'an id without the prefix reads as sent');
  assert.equal(shortModel('my-claude-model'), 'my-claude-model', 'the prefix drops only at the start');
});

test('dollar ticks step as fmtUsd does, with one precision for every tick of an axis', () => {
  const axis = (ticks) => ticks.map((v) => fmtUsdTick(v, usdTickDigits(ticks)));
  assert.deepEqual(axis([12.4]), ['$12'], 'whole dollars from 10');
  assert.deepEqual(axis([3.5]), ['$3.50'], 'cents under 10');
  assert.deepEqual(axis([0.005]), ['$0.0050'], 'four decimals under a cent');
  assert.deepEqual(axis([0]), ['$0.00'], 'an axis at zero reads cents');
  assert.deepEqual(axis([0, 10, 20, 30]), ['$0', '$10', '$20', '$30']);
  assert.deepEqual(axis([0, 2.5, 5, 7.5]), ['$0.00', '$2.50', '$5.00', '$7.50']);
  // Labeled one by one, these would read $0.00, $0.0040, $0.0080, $0.01 and $0.02, with two ticks at $0.01.
  assert.deepEqual(axis([0, 0.004, 0.008, 0.012, 0.016]), ['$0.000', '$0.004', '$0.008', '$0.012', '$0.016'], 'a sub-dime axis keeps every tick apart');
  assert.deepEqual(axis([0, 0.0015, 0.003, 0.0045, 0.006]), ['$0.0000', '$0.0015', '$0.0030', '$0.0045', '$0.0060']);
  assert.equal(usdTickDigits([]), 2, 'an axis with nothing drawn reads cents');
});

test('a dollar axis under a tenth of a cent adds a decimal per decade, so its ticks still read apart', () => {
  const axis = (ticks) => ticks.map((v) => fmtUsdTick(v, usdTickDigits(ticks)));
  // The ticks Recharts picks for a $0.0003 and a $0.00033 maximum; four decimals read them $0.0001, $0.0001 and
  // $0.0003, $0.0003.
  assert.deepEqual(axis([0, 0.000075, 0.00015, 0.000225, 0.0003]), ['$0.00000', '$0.00007', '$0.00015', '$0.00022', '$0.00030']);
  const finer = axis([0, 0.000085, 0.00017, 0.000255, 0.00034]);
  assert.equal(new Set(finer).size, finer.length, `every tick reads apart: ${finer.join(' ')}`);
  assert.equal(usdTickDigits([0.001]), 4, 'a tenth of a cent keeps the four decimals');
  assert.equal(usdTickDigits([0.0009]), 5);
  assert.equal(usdTickDigits([0.00009]), 6);
  assert.equal(usdTickDigits([1e-12]), 8, 'the decimals stop at eight');
});

test('a negative dollar tick puts its sign before the currency, and a tick at zero carries none', () => {
  assert.deepEqual(['-$5.00', '$0.00', '$5.00'], [-5, 0, 5].map((v) => fmtUsdTick(v, usdTickDigits([-5, 0, 5]))));
  assert.equal(usdTickDigits([-12, 3]), 0, 'the precision reads the largest figure either side of zero');
  assert.equal(fmtUsdTick(-12, 0), '-$12');
  assert.equal(fmtUsdTick(-0, 2), '$0.00');
  assert.equal(fmtUsdTick(-0.001, 2), '$0.00', 'a figure that rounds to zero reads as zero');
});

test('a byte count reads in MiB, a whole number without decimals and any other to one decimal', () => {
  assert.equal(fmtMiB(268_435_456), '256 MiB');
  assert.equal(fmtMiB(1_572_864), '1.5 MiB');
  assert.equal(fmtMiB(0), '0 MiB');
});
