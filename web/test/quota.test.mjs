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
  assert.equal(chip('Pro', 'default_claude_ai'), 'Pro', 'the tier the profile reports for Pro');
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
