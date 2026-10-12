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

test("the pace words read ahead, behind or on pace from the window's used and elapsed shares", () => {
  const now = Date.UTC(2026, 8, 16, 14, 30);
  const MINUTE = 60_000;
  const HOUR = 60 * MINUTE;
  const window = (periodHours, utilization, resetsInMs) => ({ id: periodHours === 5 ? '5h' : '7d', label: periodHours === 5 ? '5-hour' : '7-day', observedAt: new Date(now - 40_000).toISOString(), periodHours, resetsAt: resetsInMs === null ? null : new Date(now + resetsInMs).toISOString(), utilization });
  const session = window(5, 0.62, 2 * HOUR + 14 * MINUTE);
  assert.equal(d.paceWords(session, now, null), '7% ahead of pace', '62% used with 55% of the window elapsed');
  assert.equal(d.paceWords(window(168, 0.41, 77.7 * HOUR), now, null), '13% behind pace', '41% used with 54% of the week elapsed');
  assert.equal(d.paceWords(window(5, 0.506, 2.5 * HOUR), now, null), 'on pace', 'a lead under a point reads on pace');
  assert.equal(d.paceWords(window(5, 0.512, 2.5 * HOUR), now, null), '1% ahead of pace', 'the smallest lead past a point reads one');
  assert.equal(d.paceWords(window(5, 0.62, null), now, null), null, 'a window without a reset time has no pace');
  assert.equal(d.remainingTimeShare(session, now).toFixed(4), (134 / 300).toFixed(4), 'two hours and fourteen minutes of the five-hour window still to come');
  assert.equal(d.remainingTimeShare(window(5, 0.62, null), now), null, 'a window without a reset time has no share still to come');
});

test("the pace words run to the account's end when it comes before the window's reset, and name it", () => {
  const now = Date.UTC(2026, 8, 16, 14, 30);
  const HOUR = 3_600_000;
  // Two days into a week that resets in five: to an end two days out half the span has gone, to the reset two sevenths.
  const week = { id: '7d', label: '7-day', observedAt: new Date(now - 40_000).toISOString(), periodHours: 168, resetsAt: new Date(now + 120 * HOUR).toISOString(), utilization: 0.62 };
  const end = now + 48 * HOUR;
  assert.equal(d.paceWords(week, now, end), `12% ahead of pace to its end at ${d.formatFact(end, 'time', now)}`);
  assert.equal(d.paceWords(week, now, null), '33% ahead of pace', 'no end reads the reset');
  assert.equal(d.paceWords(week, now, Date.parse(week.resetsAt)), '33% ahead of pace', 'an end at the reset is the reset');
  assert.equal(d.paceWords(week, now, now + 200 * HOUR), '33% ahead of pace', 'an end past the reset is the reset');
  const soon = now + 3 * HOUR;
  assert.equal(d.paceWords({ ...week, utilization: 0.9 }, now, soon), `4% behind pace to its end at ${d.formatFact(soon, 'time', now)}`, '48 of 51 hours gone to an end three hours out');
});

test('the end a pace reads is the saved end while cancelled, else a trial end still ahead, as the router reads it', () => {
  const now = Date.UTC(2026, 8, 16, 14, 30);
  const iso = (ms) => new Date(ms).toISOString();
  const saved = now + 5 * 86_400_000;
  const trial = now + 8 * 86_400_000;
  const term = (over) => ({ derivedEndsAt: iso(now + 30 * 86_400_000), endsAt: null, trialEndsAt: null, ...over });
  assert.equal(d.endOf({ cancelled: true, subscription: term({ endsAt: iso(saved), trialEndsAt: iso(trial) }) }, now), saved, 'the saved end while cancelled');
  assert.equal(d.endOf({ cancelled: true, subscription: term({ trialEndsAt: iso(trial) }) }, now), null, 'a cancelled account with no saved end has none, its trial aside');
  assert.equal(d.endOf({ cancelled: false, subscription: term({ endsAt: iso(saved), trialEndsAt: iso(trial) }) }, now), trial, 'a trial still ahead, the saved end standing only while cancelled');
  assert.equal(d.endOf({ cancelled: false, subscription: term({ trialEndsAt: iso(now - 1) }) }, now), null, 'a trial that has passed');
  assert.equal(d.endOf({ cancelled: false, subscription: term({}) }, now), null, 'the derived end is a prefill, not an end');
  assert.equal(d.endOf({ cancelled: false, subscription: null }, now), null, 'an API-key account');
});
