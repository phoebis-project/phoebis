import assert from 'node:assert/strict';
import { test } from 'node:test';

// The holder label and the minute formatter print in the host's zone, so the file reads them in UTC whatever the host's
// zone, and `node --test` gives each file a process of its own.
process.env.TZ = 'UTC';

const { earlierHolderLabel, equivalentShort, ESTIMATED_TITLE, fmtAbsolute, fmtBytes, fmtCredits, fmtDated, fmtDatedTime, fmtDayTick, fmtDuration, fmtEndsIn, fmtHours, fmtHourTick, fmtMiB, fmtMinor, fmtMinute, fmtShare, fmtShortTime, fmtTime, fmtUsd, fmtUsdTick, fmtWeek, forkLabel, instant, pct, placementHit, setTimeFormat, shareLabel, shareShort, shortModel, undatedModel, usdTickDigits, weekShort } = await import('./dist/api.js');

test("a fork reads its base cut at the session's width and the depth where one is known", () => {
  const base = '1a2b3c4d-5e6f-4a7b-9c8d-7e6f5a4b3c2d';
  assert.equal(forkLabel(base, 3), 'fork of 1a2b3c4d-5e6f… at 3', 'a base shortens as the Sessions page shortens a session id');
  assert.equal(forkLabel('s-1', 3), 'fork of s-1 at 3', 'a short base reads whole');
  assert.equal(forkLabel('s-1', null), 'fork of s-1', 'a fork without a depth names its base alone');
});

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

test("a log row's time within a day of now reads the clock alone, and one a day or more away its date before it", () => {
  const now = Date.parse('2026-09-16T14:30:00Z');
  const cases = [
    ['2026-09-16T09:12:05Z', '09:12:05', 'within the day the time alone'],
    ['2026-09-15T14:30:01Z', '14:30:01', 'a second short of a day'],
    ['2026-09-15T14:30:00Z', 'Sep 15 14:30:00', 'a day away carries its date'],
    ['2026-09-14T09:12:05Z', 'Sep 14 09:12:05', 'two days back, the seconds kept'],
    ['2026-08-16T08:05:09Z', 'Aug 16 08:05:09', 'a month away'],
    ['2026-09-17T14:30:00Z', 'Sep 17 14:30:00', 'a day ahead dates as a day back does'],
  ];
  for (const [at, expected, why] of cases) {
    assert.equal(fmtDatedTime(at, now), expected, why);
    // The instant formatter dates the same rows, so a log's time and a reset's agree on which carry a date.
    assert.equal(/^[A-Z]/.test(instant(Date.parse(at), now)), /^[A-Z]/.test(expected), `instant agrees on ${at}`);
  }
  setTimeFormat('12h');
  try {
    assert.match(fmtDatedTime('2026-09-16T13:05:09Z', now), /^01:05:09\sPM$/, 'the time format reaches the clock alone');
    assert.match(fmtDatedTime('2026-09-14T21:12:05Z', now), /^Sep 14 09:12:05\sPM$/, 'and the dated form');
  } finally {
    setTimeFormat('24h');
  }
});

test("a window's end carries its date whatever its distance from now, the time in the viewer's cycle", () => {
  // Read in UTC, the zone the file runs in: a viewer elsewhere reads the day and the hour of their own zone.
  const now = Date.parse('2026-09-16T14:30:00Z');
  assert.equal(fmtDated(Date.parse('2026-09-16T13:11:00Z')), 'Sep 16 13:11', 'a window that ended today keeps its date, where instant drops it');
  assert.equal(instant(Date.parse('2026-09-16T13:11:00Z'), now), '13:11');
  assert.equal(fmtDated(Date.parse('2026-08-30T03:05:00Z')), 'Aug 30 03:05');
  setTimeFormat('12h');
  try {
    assert.match(fmtDated(Date.parse('2026-09-16T13:11:00Z')), /^Sep 16 01:11\sPM$/, 'the time format reaches the time');
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
    ['1d 20h', '2d 23h', '3h 16m', '46m', '1h'],
    'the break-evens at the production leads, each unit floored',
  );
  assert.equal(fmtDuration(259_200_000), '3d', 'a whole number of days reads without its zero hours');
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

test("a share names its window through the console's labels and an unknown key as itself", () => {
  assert.equal(shareLabel('five_hour', 0.0042), '0.42% of 5-hour');
  assert.equal(shareLabel('seven_day', 0.00061), '0.061% of 7-day');
  assert.equal(shareLabel('seven_day_fable', 0.0012), '0.12% of 7-day Fable', 'the model week reads the label the pages draw');
  assert.equal(shareLabel('seven_day_opus', 0.5), '50% of seven_day_opus', 'a window the pages do not know keeps its key');
});

test("a share's short form names its window in a word, and every spelling of a window reads one word", () => {
  assert.equal(shareShort('five_hour', 0.0042), '0.42% 5h');
  assert.equal(shareShort('seven_day', 0.0012), '0.12% 7d');
  assert.equal(shareShort('seven_day_fable', 0.0012), '0.12% 7-day Fable');
  assert.equal(shareShort('seven_day_opus', 0.5), '50% seven_day_opus', 'a window the pages do not know keeps its key');
  assert.equal(shareLabel('seven_day_fable', 0.0012), '0.12% of 7-day Fable', 'the full form stays for the titles and the other pages');
});

test('an amount in minor units reads in dollars at its exponent, two digits where the reading names none', () => {
  assert.equal(fmtCredits(102_500, 2), '$1,025.00');
  assert.equal(fmtCredits(102_500), '$1,025.00', 'cents unless the reading says otherwise');
  assert.equal(fmtCredits(102_500, 0), '$102,500.00', 'an exponent of 0 counts whole dollars');
  assert.equal(fmtCredits(102_537, 2), '$1,025.37', 'cents print as they are');
});

test('a spend amount in minor units reads in its own currency at its exponent', () => {
  assert.equal(fmtMinor(12_500, 'EUR', 2), '€125.00', 'a euro amount reads in euros');
  assert.equal(fmtMinor(102_500, 'USD', 2), '$1,025.00');
  assert.equal(fmtMinor(102_537, 'USD', 2), '$1,025.37');
  assert.equal(fmtMinor(1_250, 'EUR', 1), '€125.00', 'the exponent places the point');
});

test('a spend amount under a code Intl rejects reads as a decimal with the code after it, or alone without a code', () => {
  assert.equal(fmtMinor(12_500, 'EURO', 2), '125.00 EURO', 'a code of four letters prints after the amount');
  assert.equal(fmtMinor(12_500, '', 2), '125.00', 'an empty code prints the amount alone');
});

test('a share of the chosen week reads as a percent of that week, zero as 0%, and its short form names the week', () => {
  assert.equal(fmtWeek(0.0011, 'ultra'), '0.11% of an Ultra week');
  assert.equal(fmtWeek(0, 'pro'), '0%');
  assert.equal(weekShort(0.0022, 'max'), '0.22% Max week');
});

test("an equivalent's short form names its 20x window in a word", () => {
  assert.equal(equivalentShort('five_hour', 0.014667), '1.47% 20x 5h');
  assert.equal(equivalentShort('seven_day', 0.002444), '0.244% 20x 7d');
  assert.equal(equivalentShort('seven_day_fable', 0.02222), '2.22% 20x 7-day Fable');
  assert.equal(equivalentShort('seven_day_opus', 0.5), '50% 20x seven_day_opus', 'a window the pages do not know keeps its key');
});

test('the estimated title is one sentence that names the model tier and no model', () => {
  assert.equal(typeof ESTIMATED_TITLE, 'string');
  assert.match(ESTIMATED_TITLE, /^Estimated [^.;:]+[^.;:\s]$/, 'one sentence, with no stop, semicolon or colon inside it and none at its end');
  assert.match(ESTIMATED_TITLE, /\bmodel tier\b/, 'the model tier named apart from the account tier');
  assert.doesNotMatch(ESTIMATED_TITLE, /claude-|fable|haiku|opus|sonnet/i, 'no model or family named, since the routes answer no donor');
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

test('a percent figure that rounds to zero reads 0%, never -0%, and a lead above it carries its sign where asked', () => {
  assert.equal(pct(-0.4, true), '0%', 'a lead just behind pace rounds to no lead at all');
  assert.equal(pct(-0.4), '0%');
  assert.equal(pct(0.4, true), '0%', 'nor does a lead just ahead take a sign');
  assert.equal(pct(-0.6, true), '-1%');
  assert.equal(pct(12.4, true), '+12%');
  assert.equal(pct(12.4), '12%', 'a figure read unsigned carries no plus');
  assert.equal(pct(null), '-');
  assert.equal(pct(undefined, true), '-');
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

test('a byte count reads in the largest binary unit it reaches, a whole figure without decimals and any other to one decimal', () => {
  assert.equal(fmtBytes(0), '0 B');
  assert.equal(fmtBytes(1023), '1023 B', 'below a KiB the bytes read whole');
  assert.equal(fmtBytes(1024), '1 KiB');
  assert.equal(fmtBytes(1_048_576), '1 MiB');
  assert.equal(fmtBytes(1_572_864), '1.5 MiB');
  assert.equal(fmtBytes(1_073_741_824), '1 GiB');
});
