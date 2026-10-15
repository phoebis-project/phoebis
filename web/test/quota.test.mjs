import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

test('a window is expired once its reset lies before now, never without a reset', () => {
  const now = Date.UTC(2026, 8, 16, 14, 30);
  const window = (resetsAt) => ({ id: '5h', label: '5-hour', observedAt: new Date(now - 60_000).toISOString(), periodHours: 5, resetsAt, utilization: 0.18 });
  assert.equal(d.isExpired(window(new Date(now - 2 * 86_400_000).toISOString()), now), true, 'a reset two days ago');
  assert.equal(d.isExpired(window(new Date(now + 3_600_000).toISOString()), now), false, 'a reset an hour ahead');
  assert.equal(d.isExpired(window(new Date(now).toISOString()), now), false, 'a reset at this instant has not passed yet');
  assert.equal(d.isExpired(window(null), now), false, 'a window reported without a reset');
});

test("the plan chip names the tier with its multiple and falls back to the profile's flag", () => {
  const chip = (plan, rateLimitTier) => d.planChip({ plan, subscription: { rateLimitTier } });
  assert.equal(chip('Max', 'default_claude_max_20x'), 'Max 20x');
  assert.equal(chip('Max', 'claude_max_5x'), 'Max 5x');
  assert.equal(chip('Max', 'anything_else'), 'Max', 'a tier the console does not parse reads the flag');
  assert.equal(chip(null, 'default_claude_ai'), 'Pro', 'the tier the profile reports for Pro, with no flag to fall back on');
  assert.equal(d.planChip({ plan: null, subscription: null }), null, 'no plan at all');
});

test('the extra usage line reads in use, enabled, disabled or unreported', () => {
  const extra = (enabled, inUse) => ({ disabledReason: null, enabled, inUse, monthlyLimitCredits: 2000, usedCredits: 612 });
  assert.equal(d.extraUsageState(extra(true, true)), 'in_use');
  assert.equal(d.extraUsageState(extra(true, false)), 'enabled');
  assert.equal(d.extraUsageState({ ...extra(false, false), disabledReason: 'Payment method declined' }), 'disabled');
  assert.equal(d.extraUsageState(null), 'unreported');
});

test('the poll reading keeps one or two lines and moves the rest into the title', () => {
  const now = Date.UTC(2026, 8, 16, 14, 30);
  const at = (ms) => new Date(now + ms).toISOString();
  const MINUTE = 60_000;
  const DAY = 86_400_000;
  const poll = (fields) => ({ backoffSince: null, lastAttemptAt: at(-40_000), lastCleanAt: at(-40_000), lastError: null, lastOutcome: 'clean', nextAttemptAt: null, rateLimitedInARow: 0, ...fields });
  const shaped = (name, reading) => {
    assert.ok(reading.lines.length >= 1 && reading.lines.length <= 2, `${name} draws one or two lines`);
    for (const line of reading.lines) assert.notEqual(line.text.trim(), '', `${name} draws no empty line`);
  };

  const clean = d.pollReading(poll({}), at(-40_000), now);
  shaped('a clean poll', clean);
  assert.ok(clean.title.split('\n').includes(`checked ${d.fmtAbsolute(at(-40_000))}`), 'the clean title names when the snapshot was read');

  const reason = 'usage request rejected with status 503';
  const failed = d.pollReading(poll({ lastAttemptAt: at(-3 * MINUTE), lastCleanAt: at(-2 * DAY), lastError: reason, lastOutcome: 'failed' }), at(-2 * DAY), now);
  shaped('a failed poll', failed);
  assert.equal(failed.lines.filter((l) => l.reason !== undefined).length, 1, 'one line carries the reason');
  assert.equal(failed.lines.find((l) => l.reason !== undefined)?.reason, reason);
  assert.ok(failed.title.split('\n').includes(reason), 'the title carries the reason whole');

  const nextAttemptAt = at(4 * MINUTE);
  const backoff = d.pollReading(poll({ backoffSince: at(-42 * MINUTE), lastAttemptAt: at(-11 * MINUTE), lastCleanAt: at(-47 * MINUTE), lastError: 'the usage endpoint rate-limited the account', lastOutcome: 'rate_limited', nextAttemptAt, rateLimitedInARow: 6 }), at(-47 * MINUTE), now);
  shaped('a poll backing off', backoff);
  assert.ok(backoff.lines.some((l) => l.tone === 'serious'), 'the backoff reads in the serious tone');
  const facts = backoff.title.split('\n');
  assert.ok(facts.some((f) => f.includes(d.fmtAbsolute(at(-42 * MINUTE)))), 'the title names since when the account backs off');
  assert.ok(facts.includes('6 rate-limited answers in a row'), 'the title names the run');
  assert.ok(facts.some((f) => f.includes(d.fmtAbsolute(nextAttemptAt))), 'the title names the next attempt');

  const none = d.pollReading(null, null, now);
  assert.deepEqual(none.lines.map((l) => l.text), ['not yet polled']);
});

test('a card the poll has not asked yet still names when its snapshot was read', () => {
  const now = Date.UTC(2026, 8, 16, 14, 30);
  const fetchedAt = new Date(now - 3 * 3_600_000).toISOString();
  const reading = d.pollReading(null, fetchedAt, now);
  assert.deepEqual(reading.lines, [{ text: 'not yet polled', tone: 'muted' }]);
  assert.equal(reading.title, `checked ${d.fmtAbsolute(fetchedAt)}`);
});

test('the pace words read ahead, behind or on pace from the lead the window carries', () => {
  const now = Date.UTC(2026, 8, 16, 14, 30);
  const MINUTE = 60_000;
  const HOUR = 60 * MINUTE;
  // The lead the router answers, worked here by hand: the used share less the share of the window elapsed.
  const window = (periodHours, utilization, resetsInMs, elapsed) => ({ id: periodHours === 5 ? '5h' : '7d', label: periodHours === 5 ? '5-hour' : '7-day', lead: elapsed === null ? null : utilization - elapsed, observedAt: new Date(now - 40_000).toISOString(), periodHours, resetsAt: resetsInMs === null ? null : new Date(now + resetsInMs).toISOString(), utilization });
  const session = window(5, 0.62, 2 * HOUR + 14 * MINUTE, 166 / 300);
  assert.equal(d.paceWords(session, now, null), '7% ahead of pace', '62% used with 55% of the window elapsed');
  assert.equal(d.paceWords(window(168, 0.41, 77.7 * HOUR, 90.3 / 168), now, null), '13% behind pace', '41% used with 54% of the week elapsed');
  assert.equal(d.paceWords(window(5, 0.506, 2.5 * HOUR, 0.5), now, null), 'on pace', 'a lead under a point reads on pace');
  assert.equal(d.paceWords(window(5, 0.512, 2.5 * HOUR, 0.5), now, null), '1% ahead of pace', 'the smallest lead past a point reads one');
  assert.equal(d.paceWords(window(5, 0.62, null, 0), now, null), null, 'a window without a reset time has no pace');
  assert.equal(d.paceWords(window(5, 0.62, 2 * HOUR + 14 * MINUTE, null), now, null), null, 'a window the answer carries no lead for has no pace');
  assert.equal(d.remainingTimeShare(session).toFixed(4), (134 / 300).toFixed(4), 'two hours and fourteen minutes of the five-hour window still to come');
  assert.equal(d.remainingTimeShare({ ...session, lead: 0.07, utilization: 0.62 }).toFixed(2), '0.45', 'the share still to come is one less the used share less the lead');
  assert.equal(d.remainingTimeShare(window(5, 0.62, null, 0)), null, 'a window without a reset time has no share still to come');
});

test("the pace words run to the account's end when it comes before the window's reset, and name it", () => {
  const now = Date.UTC(2026, 8, 16, 14, 30);
  const HOUR = 3_600_000;
  // Two days into a week that resets in five: to an end two days out half the span has gone, to the reset two sevenths.
  const week = { id: '7d', label: '7-day', lead: null, observedAt: new Date(now - 40_000).toISOString(), periodHours: 168, resetsAt: new Date(now + 120 * HOUR).toISOString(), utilization: 0.62 };
  const toReset = { ...week, lead: 0.62 - 48 / 168 };
  const end = now + 48 * HOUR;
  const toEnd = { ...week, lead: 0.62 - 48 / 96 };
  assert.equal(d.paceWords(toEnd, now, end), `12% ahead of pace to its end at ${d.formatFact(end, 'time', now)}`);
  assert.equal(d.paceWords(toReset, now, null), '33% ahead of pace', 'no end reads the reset');
  assert.equal(d.paceWords(toReset, now, Date.parse(week.resetsAt)), '33% ahead of pace', 'an end at the reset is the reset');
  assert.equal(d.paceWords(toReset, now, now + 200 * HOUR), '33% ahead of pace', 'an end past the reset is the reset');
  const soon = now + 3 * HOUR;
  assert.equal(d.paceWords({ ...week, lead: 0.9 - 48 / 51, utilization: 0.9 }, now, soon), `4% behind pace to its end at ${d.formatFact(soon, 'time', now)}`, '48 of 51 hours gone to an end three hours out');
  // The meter's mark reads the same lead, so the fill against it shows the lead the words name.
  assert.equal(d.remainingTimeShare(toEnd).toFixed(4), '0.5000', 'half the span to the end still to come');
  assert.equal(d.remainingTimeShare(toReset).toFixed(4), (120 / 168).toFixed(4), 'five days of the week still to come');
  assert.equal(d.remainingTimeShare({ ...week, lead: 0.62 - 1 }).toFixed(4), '0.0000', 'an end at or before the window opened leaves nothing to come');
  assert.equal(d.remainingTimeShare(week), null, 'a window the answer carries no lead for draws no mark');
});

test('the end a pace reads is the saved end while cancelled, else a trial end still ahead, as the router reads it', () => {
  const now = Date.UTC(2026, 8, 16, 14, 30);
  const iso = (ms) => new Date(ms).toISOString();
  const saved = now + 5 * 86_400_000;
  const trial = now + 8 * 86_400_000;
  const term = (over) => ({ endsAt: null, trialEndsAt: null, ...over });
  assert.equal(d.endOf({ cancelled: true, subscription: term({ endsAt: iso(saved), trialEndsAt: iso(trial) }) }, now), saved, 'the saved end while cancelled');
  assert.equal(d.endOf({ cancelled: true, subscription: term({ trialEndsAt: iso(trial) }) }, now), null, 'a cancelled account with no saved end has none, its trial aside');
  assert.equal(d.endOf({ cancelled: false, subscription: term({ endsAt: iso(saved), trialEndsAt: iso(trial) }) }, now), trial, 'a trial still ahead, the saved end standing only while cancelled');
  assert.equal(d.endOf({ cancelled: false, subscription: term({ trialEndsAt: iso(now - 1) }) }, now), null, 'a trial that has passed');
  assert.equal(d.endOf({ cancelled: false, subscription: term({}) }, now), null, 'an account neither cancelled nor on a trial has no end');
  assert.equal(d.endOf({ cancelled: false, subscription: null }, now), null, 'an API-key account');
});

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

test('the projection reads at the reset where its horizon lies within a minute of the reset, since its two instants come from two sources', () => {
  const now = Date.UTC(2026, 8, 16, 14, 30);
  const at = (ms) => new Date(now + ms).toISOString();
  const window = (horizon, reset) => ({ burn: 0.085, horizon: at(horizon), projection: 0.81, resetsAt: at(reset), utilization: 0.62 });
  const reset = 2 * HOUR + 13 * MINUTE;
  assert.equal(d.projectionWords(window(reset - 500, reset), now), '19% left at the reset', 'a horizon 500 ms before its reset');
  assert.equal(d.projectionWords(window(reset - 500, reset), now, true), '19% left at reset', 'the compact form');
  assert.equal(d.projectionWords(window(reset, reset), now), '19% left at the reset', 'a horizon on its reset');
  assert.equal(d.projectionWords(window(reset - 2 * MINUTE, reset), now), '19% left in 2 h 11 min', 'an end two minutes before the reset');
  assert.equal(d.projectionWords(window(5 * HOUR, 3 * DAY), now), '19% left in 5 h', 'a weekly horizon five hours out');
  assert.equal(d.projectionWords(window(5 * HOUR, 3 * DAY), now, true), '19% left in 5 h', 'the compact form of a weekly horizon');
  // A bar too narrow for the compact words breaks between the share and the horizon, each whole.
  assert.deepEqual(d.projectionParts(window(reset, reset), now, true), ['19% left', 'at reset']);
  assert.deepEqual(d.projectionParts(window(5 * HOUR, 3 * DAY), now, true), ['19% left', 'in 5 h']);
});

test('a span under a minute reads one minute, and a horizon at or before now reads now', () => {
  const now = Date.UTC(2026, 8, 16, 14, 30);
  const at = (ms) => new Date(now + ms).toISOString();
  assert.equal(d.minuteSpan(20_000), '1 min', 'twenty seconds');
  assert.equal(d.minuteSpan(29_999), '1 min', 'under half a minute');
  assert.equal(d.minuteSpan(5 * HOUR - 20_000), '5 h', 'a span seconds short of five hours');
  assert.equal(d.minuteSpan(4 * HOUR + 59 * MINUTE), '4 h 59 min');
  const window = (horizon) => ({ burn: 0, horizon: at(horizon), projection: 0.41, resetsAt: at(3 * DAY), utilization: 0.41 });
  assert.equal(d.projectionWords(window(0), now), '59% left now', 'a horizon at now, an end that has come');
  assert.equal(d.projectionWords(window(-MINUTE), now), '59% left now', 'a horizon a minute past');
  assert.equal(d.projectionWords(window(-MINUTE), now, true), '59% left now', 'the compact form');
  assert.deepEqual(d.projectionParts(window(-MINUTE), now, true), ['59% left', 'now']);
});

test('a window the projection spends before its horizon reads the instant it runs out, the same in both forms', () => {
  const now = Date.UTC(2026, 8, 16, 14, 30);
  const at = (ms) => new Date(now + ms).toISOString();
  // 85% used at 12% an hour reaches 109% at a reset two hours out, and the last 15% runs out in an hour and a quarter.
  const spent = { burn: 0.12, horizon: at(2 * HOUR), projection: 1.09, resetsAt: at(2 * HOUR), utilization: 0.85 };
  assert.equal(d.projectionWords(spent, now), 'spent in 1 h 15 min');
  assert.equal(d.projectionWords(spent, now, true), 'spent in 1 h 15 min', 'the compact form');
  assert.equal(d.projectionWords({ ...spent, resetsAt: at(3 * DAY) }, now), 'spent in 1 h 15 min', 'a weekly horizon before its reset');
  assert.deepEqual(d.projectionParts(spent, now, true), ['spent', 'in 1 h 15 min']);
});

test('a projection past the window without a burn reads no share left, and a window already used up reads none now', () => {
  const now = Date.UTC(2026, 8, 16, 14, 30);
  const at = (ms) => new Date(now + ms).toISOString();
  const over = { burn: null, horizon: at(2 * HOUR), projection: 1.09, resetsAt: at(2 * HOUR), utilization: 0.85 };
  assert.equal(d.projectionWords(over, now), '0% left at the reset', 'a projection past the window with no burn to run out along');
  assert.equal(d.projectionWords({ ...over, burn: 0 }, now, true), '0% left at reset', 'a burn of zero, in the compact form');
  assert.equal(d.projectionWords({ ...over, resetsAt: at(3 * DAY) }, now), '0% left in 2 h', 'a horizon before the reset');
  const full = { burn: 0.12, horizon: at(2 * HOUR), projection: 1.24, resetsAt: at(2 * HOUR), utilization: 1 };
  assert.equal(d.projectionWords(full, now), '0% left now', 'a window whose used share already reads 1');
  assert.equal(d.projectionWords({ ...full, utilization: 1.02 }, now, true), '0% left now', 'past 1, in the compact form');
  assert.equal(d.projectionWords({ ...full, projection: null }, now), null, 'a window without a projection');
});
