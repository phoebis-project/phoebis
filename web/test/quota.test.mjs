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

test("an account's slow-down draws while the speed its holds realized is under 1 and the 5-hour window it was read from has not reset", () => {
  const now = Date.UTC(2026, 8, 16, 14, 30);
  const entry = (factor, speed = factor) => ({ factor, holdMs: 120_000, ramp: null, since: new Date(now - 42 * 60_000).toISOString(), speed, stretchMs: 2_520_000, thresholdMs: 1_800_000 });
  const five = (resetsAt) => [{ id: '5h', label: '5-hour', observedAt: new Date(now - 3_600_000).toISOString(), periodHours: 5, resetsAt, utilization: 0.86 }];
  const live = five(new Date(now + 3_600_000).toISOString());
  assert.deepEqual(d.slowedOf({ slowDown: entry(0.5), windows: live }, now), entry(0.5));
  assert.equal(d.slowedOf({ slowDown: entry(0.5), windows: five(new Date(now - 5 * 60_000).toISOString()) }, now), null, 'the window reset after the read, so the entry is as stale as its figures');
  assert.equal(d.slowedOf({ slowDown: entry(1), windows: live }, now), null, 'an account at full speed');
  assert.equal(d.slowedOf({ slowDown: entry(0.6, 1), windows: live }, now), null, 'a factor under 1 whose holds realized full speed, as before a pin there has a cycle');
  assert.deepEqual(d.slowedOf({ slowDown: entry(0.6, 0.8), windows: live }, now), entry(0.6, 0.8), 'the holds realized a speed under 1');
  assert.equal(d.slowedOf({ slowDown: entry(0.5), windows: [] }, now), null, 'no 5-hour window to read it from');
  assert.equal(d.slowedOf({ slowDown: null, windows: live }, now), null, 'no entry');
});

test('a plan word the table lacks reads as sent, a key every plain object carries among them', () => {
  assert.equal(d.planWordLabel('some_new_word'), 'some_new_word');
  assert.equal(d.planWordLabel('constructor'), 'constructor', "the table's own keys alone label a word");
  assert.equal(d.planWordLabel('toString'), 'toString');
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

test('the end a pace reads is the saved end while canceled, else a trial end still ahead, as the router reads it', () => {
  const now = Date.UTC(2026, 8, 16, 14, 30);
  const iso = (ms) => new Date(ms).toISOString();
  const saved = now + 5 * 86_400_000;
  const trial = now + 8 * 86_400_000;
  const term = (over) => ({ endsAt: null, trialEndsAt: null, ...over });
  assert.equal(d.endOf({ canceled: true, subscription: term({ endsAt: iso(saved), trialEndsAt: iso(trial) }) }, now), saved, 'the saved end while canceled');
  assert.equal(d.endOf({ canceled: true, subscription: term({ trialEndsAt: iso(trial) }) }, now), null, 'a canceled account with no saved end has none, its trial aside');
  assert.equal(d.endOf({ canceled: false, subscription: term({ endsAt: iso(saved), trialEndsAt: iso(trial) }) }, now), trial, 'a trial still ahead, the saved end standing only while canceled');
  assert.equal(d.endOf({ canceled: false, subscription: term({ trialEndsAt: iso(now - 1) }) }, now), null, 'a trial that has passed');
  assert.equal(d.endOf({ canceled: false, subscription: term({}) }, now), null, 'an account neither canceled nor on a trial has no end');
  assert.equal(d.endOf({ canceled: false, subscription: null }, now), null, 'an API-key account');
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

test("a spent window reads the instant the router's forecast reaches the whole window, where the forecast runs past the last sample", () => {
  const now = Date.UTC(2026, 8, 16, 14, 30);
  const at = (ms) => new Date(now + ms).toISOString();
  // 20% sampled while the forecast reads 90% now and 110% at a horizon an hour out at 20% an hour: the forecast
  // reaches the whole window in 30 minutes, where the sample's 80% left would last 4 hours at that burn.
  const ahead = { burn: 0.2, horizon: at(HOUR), projection: 1.1, resetsAt: at(HOUR), utilization: 0.2 };
  assert.equal(d.projectionWords(ahead, now), 'spent in 30 min');
  assert.deepEqual(d.projectionParts(ahead, now, true), ['spent', 'in 30 min'], 'the compact form');
  // 50% sampled, a 95% forecast now at 10% an hour, so 105% an hour out: 30 minutes, where the sample reads 5 h.
  const weekly = { burn: 0.1, horizon: at(HOUR), projection: 1.05, resetsAt: at(3 * DAY), utilization: 0.5 };
  assert.equal(d.projectionWords(weekly, now), 'spent in 30 min', 'a weekly horizon before its reset');
  assert.equal(d.projectionWords(weekly, now + 20 * MINUTE), 'spent in 10 min', 'the span shortens on the clock between reads');
  assert.equal(d.projectionWords({ ...weekly, projection: 1.45 }, now), '0% left now', 'a forecast that reached the whole window before now');
  assert.equal(d.projectionWords({ ...weekly, horizon: at(-MINUTE) }, now, true), '0% left now', 'a horizon past with the forecast over the window');
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

test("a short timeline cell's hour drops its leading zero and carries its period under the 12-hour setting, and reads two digits under the 24-hour one", () => {
  // Local hours, since the cells open on the viewer's midnight and every formatter prints in the browser's zone.
  const at = (hour) => new Date(2026, 8, 16, hour).toISOString();
  const hours = [0, 6, 12, 18];
  d.setTimeFormat('12h');
  try {
    assert.deepEqual(hours.map((hour) => d.fmtShortHour(at(hour))), ['12 AM', '6 AM', '12 PM', '6 PM']);
  } finally {
    d.setTimeFormat('24h');
  }
  assert.deepEqual(hours.map((hour) => d.fmtShortHour(at(hour))), ['00', '06', '12', '18']);
});

test('the 5-hour timeline draws fewer cells at one width under the 12-hour setting, so each cell holds its short hour', () => {
  assert.equal(d.cellsFit(316, '24h'), 12, 'twelve cells of 26 px hold "06"');
  assert.equal(d.cellsFit(316, '12h'), 6, 'twelve cells of 26 px cut "12 AM", six of 52.67 px hold it');
  assert.equal(d.cellsFit(876, '24h'), 12);
  assert.equal(d.cellsFit(876, '12h'), 12, 'twelve cells of 73 px hold "12 AM"');
});

test('the Quota page lists a subscription account that is paired, not awaiting re-authentication and whose plan has not ended, and no API-key account', () => {
  for (const status of ['cooldown', 'funding_required', 'paused', 'ready']) assert.equal(d.isQuotaListed({ kind: 'oauth', planEndedAt: null, status }), true, `a subscription account reading ${status}`);
  for (const status of ['reauth_required', 'unpaired']) assert.equal(d.isQuotaListed({ kind: 'oauth', planEndedAt: null, status }), false, `a subscription account reading ${status}`);
  assert.equal(d.isQuotaListed({ kind: 'api_key', planEndedAt: null, status: 'ready' }), false, 'an API-key account');
  const ended = '2026-09-16T10:30:00.000Z';
  assert.equal(d.isQuotaListed({ kind: 'oauth', planEndedAt: ended, status: 'ready' }), false, 'the plan-ended flag alone withholds the Quota page');
  assert.equal(d.isQuotaListed({ canceled: true, kind: 'oauth', planEndedAt: null, status: 'ready', subscription: { endsAt: '2026-09-13T14:30:00.000Z' } }), true, 'a saved end that has passed counts for nothing on its own');
  assert.equal(d.isQuotaListed({ kind: 'oauth', planEndedAt: ended, status: 'cooldown' }), false, 'a flagged account in cooldown');
});

test('the plan-ended sentence reads its age in the long form and in the compact one the account cards draw', () => {
  const now = Date.UTC(2026, 8, 16, 14, 30);
  const ended = new Date(now - 4 * 3_600_000).toISOString();
  const ago = d.fmtRelative(ended, now);
  assert.equal(ago, '4h ago');
  assert.equal(d.planEndedSentence(ended, now), 'No quota window is read since the plan ended 4h ago.');
  assert.equal(d.planEndedSentence(ended, now, false), `No quota window is read since the plan ended ${ago}.`);
  assert.equal(d.planEndedSentence(ended, now, true), 'No window since the plan ended 4h ago');
});

test("the priced tier is the stored one under the six tiered plan words while the cost table holds its row, and none with the gate's reason otherwise", () => {
  // The table's own rows: Ultra and Team held, Enterprise and Pro's tier not.
  const costs = { capacity: { default_claude_max_20x: { fiveHour: 100, sevenDay: 1_000 }, default_claude_team: { fiveHour: 10, sevenDay: 100 } } };
  const priced = (plan, tier) => d.pricedTier(plan, tier, costs);
  for (const plan of ['max', 'max_20x', 'max_5x', 'pro']) assert.deepEqual(priced(plan, 'default_claude_max_20x'), { tier: 'default_claude_max_20x' }, `${plan} on a held tier`);
  assert.deepEqual(priced('team', 'default_claude_team'), { tier: 'default_claude_team' }, 'a Team seat on a held tier');
  assert.deepEqual(priced('team', 'claude_team'), { tier: 'claude_team' }, "the held tier matched with the prefix put on, as the cost model's lookup matches it");
  assert.deepEqual(priced('enterprise', 'default_claude_max_20x'), { tier: 'default_claude_max_20x' }, 'an Enterprise seat on a held tier');
  const unknown = { reason: 'tier unknown', tier: null };
  assert.deepEqual(priced('enterprise', 'default_claude_enterprise'), unknown, 'an Enterprise seat on a tier the table lacks');
  assert.deepEqual(priced('pro', 'default_claude_ai'), unknown, 'a Pro tier the table lacks');
  assert.deepEqual(priced('free', 'default_claude_max_20x'), { reason: 'no priced capacity for a free plan', tier: null }, 'a free plan on a held tier');
  assert.deepEqual(priced('usage_based', 'default_claude_max_20x'), { reason: 'a usage-based seat', tier: null }, 'a usage-based seat on a held tier');
  assert.deepEqual(priced(null, 'default_claude_max_20x'), unknown, 'no plan');
  assert.deepEqual(priced('max_20x', null), unknown, 'no tier');
  assert.deepEqual(priced('some_new_word', 'default_claude_max_20x'), unknown, 'a plan word outside the eight');
});

const SPEND_NOW = Date.UTC(2026, 8, 16, 14, 30);
/** A usage-based seat's spend reading, its estimated reset at 00:00 UTC on the first of the next month. */
const spendReading = (over = {}) => ({
  amounts: { currency: 'USD', exponent: 2, limitMinor: 250_000, polledAt: new Date(SPEND_NOW - 600_000).toISOString(), usedMinor: 102_500 },
  observedAt: new Date(SPEND_NOW - 300_000).toISOString(),
  resetSource: 'estimated',
  resetsAt: new Date(Date.UTC(2026, 9, 1)).toISOString(),
  utilization: 0.41,
  ...over,
});
const planWindow = (id, resetsInMs, lead = 0.1) => ({ id, label: id, lead, observedAt: new Date(SPEND_NOW - 40_000).toISOString(), periodHours: id === '5h' ? 5 : 168, resetsAt: new Date(SPEND_NOW + resetsInMs).toISOString(), utilization: 0.5 });
const seatAccount = (spend = spendReading()) => ({ fableWindow: null, id: 'seat', label: 'seat', spend, usageBased: true, windows: [] });
const planAccount = (id, windows) => ({ fableWindow: null, id, label: id, spend: null, usageBased: false, windows });

test("the soonest reset of a usage-based seat is its spend window's, while that reset is ahead", () => {
  assert.deepEqual(d.soonestResetAt(seatAccount(), SPEND_NOW), ['spend'], 'the one reset the seat carries');
  assert.deepEqual(d.soonestResetAt(seatAccount(spendReading({ resetsAt: new Date(SPEND_NOW - 60_000).toISOString() })), SPEND_NOW), [], 'a reset already passed recovers nothing');
  assert.deepEqual(d.soonestResetAt(seatAccount(null), SPEND_NOW), [], 'a seat with no spend reading yet');
  assert.deepEqual(d.soonestResetAt(planAccount('plan', [planWindow('5h', 3_600_000), planWindow('7d', 86_400_000)]), SPEND_NOW), ['5h'], 'a plan account reads its soonest window');
});

test('the soonest reset reads the Fable window beside the others and names every window that resets at that instant', () => {
  const fableFirst = { ...planAccount('fable', [planWindow('5h', 2 * HOUR), planWindow('7d', DAY)]), fableWindow: planWindow('7d_fable', HOUR) };
  assert.deepEqual(d.soonestResetAt(fableFirst, SPEND_NOW), ['7d_fable']);
  const tied = planAccount('tied', [planWindow('5h', HOUR), planWindow('7d', HOUR)]);
  assert.deepEqual(d.soonestResetAt(tied, SPEND_NOW), ['5h', '7d'], 'two windows at one instant');
});

test("the timeline's input and its lanes take an account with a spend reading beside the plan accounts, under both zooms", () => {
  const plan = planAccount('plan', [planWindow('5h', 3_600_000), planWindow('7d', 86_400_000)]);
  const seat = seatAccount();
  const none = planAccount('none', []);
  const input = [plan, seat, none].filter(d.onTimeline);
  assert.deepEqual(input.map((a) => a.id), ['plan', 'seat'], 'an account with neither a plan window nor a spend reading draws no lane');
  for (const id of ['5h', '7d']) {
    const lanes = d.timelineLanes(input, id);
    assert.deepEqual(lanes.map((l) => [l.account.id, l.window?.id ?? null, l.spend === null ? null : l.spend.utilization]), [['plan', id, null], ['seat', null, 0.41]], `the ${id} zoom`);
  }
});
