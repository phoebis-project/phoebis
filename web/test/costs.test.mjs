import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import * as d from './dist/api.js';

// The test tables, round throughout: weights Opus 5.5 2, Haiku 4.5 0.5 (1 on the week through its override, with a
// cache read at 0.2), Opus 5 null; multipliers cache read 0.1, writes 2, input 1, output 10; capacities Max 20x 100 on
// the 5-hour window and 1000 on the week, Pro 5 and 50, Team 10 and null; the peak band weekdays 5 to 11 in Los
// Angeles at a 5-hour capacity factor of 0.5; the Fable week half the 7-day window.
const tables = JSON.parse(readFileSync(new URL('./fixtures/test_tables.json', import.meta.url), 'utf8')).body;
const costs = d.decodeSubscriptionCosts(tables.subscription_costs.content);

const MAX_20X = 'default_claude_max_20x';
const OPUS = 'claude-opus-5-5';
// Wednesday 16 September 2026 at 08:00 in Los Angeles, inside the band, and at 13:00, outside it.
const IN = Date.UTC(2026, 8, 16, 15, 0);
const OUT = Date.UTC(2026, 8, 16, 20, 0);
// Saturday 19 September 2026 at 08:00 in Los Angeles: the band's hours on a day it does not cover.
const WEEKEND = Date.UTC(2026, 8, 19, 15, 0);
const usage = (over) => ({ cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: 0, input: 0, output: 0, ...over });
const close = (actual, expected, what) => assert.ok(actual !== null && Math.abs(actual - expected) <= Math.abs(expected) * 1e-12, `${what}: ${actual} against ${expected}`);

test("the rates are the weight times the type's multiplier over the tier's capacity, per million tokens", () => {
  const rates = d.ratesOf(costs, 'five_hour', MAX_20X, OPUS);
  // 2 x 0.1 / 100 x 1e6, 2 x 2 / 100 x 1e6, 2 x 1 / 100 x 1e6 and 2 x 10 / 100 x 1e6.
  const want = { cacheRead: 2_000, cacheWrite1h: 40_000, cacheWrite5m: 40_000, input: 20_000, output: 200_000 };
  assert.deepEqual(Object.keys(rates).sort(), Object.keys(want).sort());
  for (const [type, rate] of Object.entries(want)) close(rates[type], rate, type);
});

test("a share is the tokens' units over the tier's capacity, the 5-hour window's band factor applied inside the band", () => {
  close(d.shareOf(costs, 'five_hour', MAX_20X, OPUS, usage({ input: 1 }), OUT), 0.02, 'Max 20x out of band, 2 / 100');
  close(d.shareOf(costs, 'five_hour', MAX_20X, OPUS, usage({ input: 1 }), IN), 0.04, 'Max 20x in band, 2 / 100 / 0.5');
  close(d.shareOf(costs, 'five_hour', MAX_20X, OPUS, usage({ input: 1 }), WEEKEND), 0.02, 'the band covers weekdays alone');
  close(d.shareOf(costs, 'five_hour', 'default_claude_ai', OPUS, usage({ input: 1 }), OUT), 0.4, 'Pro, 2 / 5');
  close(d.shareOf(costs, 'seven_day', MAX_20X, OPUS, usage({ input: 1 }), IN), 0.002, 'the week, whose factor of 1 leaves the band alone, 2 / 1000');
  close(d.shareOf(costs, 'seven_day_fable', MAX_20X, OPUS, usage({ input: 1 }), OUT), 0.004, 'the Fable week, 2 / 1000 / 0.5');
  close(d.shareOf(costs, 'five_hour', MAX_20X, OPUS, usage({ cacheRead: 10, input: 1, output: 1 }), OUT), 0.24, 'mixed usage, (10 x 0.2 + 2 + 20) / 100');
});

test("an override replaces the multiplier and the window's weight it names and leaves the rest of the row alone", () => {
  close(d.shareOf(costs, 'seven_day', MAX_20X, 'claude-haiku-4-5', usage({ cacheRead: 1 }), OUT), 0.0002, "Haiku's cache read on the week, 1 x 0.2 / 1000");
  close(d.shareOf(costs, 'five_hour', MAX_20X, 'claude-haiku-4-5', usage({ input: 1 }), OUT), 0.005, "Haiku's input on the 5-hour window, 0.5 x 1 / 100");
});

test('a dated id reads its undated weight, and an unweighted model or an unsized tier prices no share', () => {
  close(d.shareOf(costs, 'five_hour', MAX_20X, 'claude-opus-5-5-20260101', usage({ input: 1 }), OUT), 0.02, 'the dated id');
  assert.equal(d.shareOf(costs, 'five_hour', MAX_20X, 'claude-opus-5', usage({ input: 1 }), OUT), null, 'a null weight');
  assert.equal(d.shareOf(costs, 'five_hour', MAX_20X, 'claude-sonnet-5', usage({ input: 1 }), OUT), null, 'a model the file does not list');
  assert.equal(d.shareOf(costs, 'seven_day', 'default_claude_team', OPUS, usage({ input: 1 }), OUT), null, "the Team week's null capacity");
  assert.equal(d.shareOf(costs, 'five_hour', 'claude_enterprise', OPUS, usage({ input: 1 }), OUT), null, 'a tier the file does not list');
  assert.equal(d.ratesOf(costs, 'five_hour_extra', MAX_20X, OPUS), null, 'a window the file does not price');
});

test("a tier's capacity is read as stored, then with the default_ prefix taken off or put on", () => {
  assert.equal(d.tierCapacity(costs, 'claude_ai', 'five_hour'), 5);
  assert.equal(d.tierCapacity(costs, 'default_claude_team', 'seven_day'), null);
  assert.equal(d.tierCapacity(costs, 'claude_enterprise', 'five_hour'), null);
  assert.equal(d.anchorTier(costs), MAX_20X);
});

test("the band factor is the reciprocal of the priced window's capacity factor inside the band, and 1 outside it", () => {
  assert.equal(d.bandFactor(costs, 'five_hour', IN), 2);
  assert.equal(d.bandFactor(costs, 'seven_day', IN), 1);
  assert.equal(d.bandFactor(costs, 'five_hour', OUT), 1);
  assert.equal(d.bandFactor(costs, 'five_hour', WEEKEND), 1);
});

test('a week the file does not list prices as the 7-day share once', () => {
  assert.deepEqual(d.pricedWindow(costs, 'seven_day_other'), { divisor: 1, priced: 'seven_day' });
  close(d.shareOf(costs, 'seven_day_other', MAX_20X, OPUS, usage({ input: 1 }), OUT), 0.002, 'the 7-day share, 2 / 1000');
});

test('a peak zone the browser cannot resolve prices out of band', () => {
  const unzoned = { ...costs, peak: { ...costs.peak, zone: 'Nowhere/Unknown' } };
  assert.equal(d.inBand(unzoned, IN), false);
  close(d.shareOf(unzoned, 'five_hour', MAX_20X, OPUS, usage({ input: 1 }), IN), 0.02, 'the out-of-band share inside the hours, 2 / 100');
});

// The band in a zone. Wednesday 16 September 2026 at 14:30 UTC, 07:30 in Los Angeles: inside the test table's band of
// weekdays 5 to 11 there, which reads 12:00 to 18:00 UTC while Los Angeles keeps summer time.
const NOW = Date.parse('2026-09-16T14:30:00Z');
const HOUR = 3_600_000;
const run = (from, to) => ({ from, to });
const clock24 = (minutes) => `${String(Math.floor(minutes / 60)).padStart(2, '0')}:${String(minutes % 60).padStart(2, '0')}`;
const WEEK_LABELS = ['Mon 14 Sep', 'Tue 15 Sep', 'Wed 16 Sep', 'Thu 17 Sep', 'Fri 18 Sep', 'Sat 19 Sep', 'Sun 20 Sep'];

test("the week holding an instant runs Monday to Sunday in the viewing zone, the band's runs per day in its minutes", () => {
  const week = d.bandWeek(costs, NOW, 'America/Los_Angeles');
  assert.deepEqual(week.map((day) => day.label), WEEK_LABELS);
  assert.deepEqual(week.map((day) => day.date), ['2026-09-14', '2026-09-15', '2026-09-16', '2026-09-17', '2026-09-18', '2026-09-19', '2026-09-20']);
  assert.deepEqual(week.map((day) => day.today), [false, false, true, false, false, false, false]);
  // 5 to 11 in the band's own zone, minutes 300 to 660, on the five weekdays alone.
  assert.deepEqual(week.map((day) => day.runs), [[run(300, 660)], [run(300, 660)], [run(300, 660)], [run(300, 660)], [run(300, 660)], [], []]);
});

test('the band draws where it falls in another zone: shifted, at a half-hour offset, and across midnight', () => {
  const runsIn = (zone) => d.bandWeek(costs, NOW, zone).map((day) => day.runs);
  // Berlin at UTC+2 reads 14:00 to 20:00, Kolkata at UTC+5:30 17:30 to 23:30.
  assert.deepEqual(runsIn('Europe/Berlin'), [[run(840, 1200)], [run(840, 1200)], [run(840, 1200)], [run(840, 1200)], [run(840, 1200)], [], []]);
  assert.deepEqual(runsIn('Asia/Kolkata'), [[run(1050, 1410)], [run(1050, 1410)], [run(1050, 1410)], [run(1050, 1410)], [run(1050, 1410)], [], []]);
  // Tokyo at UTC+9 reads 21:00 to 03:00: each weekday's run ends on the next day, so Saturday carries Friday's tail.
  const tokyo = d.bandWeek(costs, NOW, 'Asia/Tokyo');
  assert.deepEqual(tokyo.map((day) => day.label), WEEK_LABELS);
  const night = [run(0, 180), run(1260, 1440)];
  assert.deepEqual(tokyo.map((day) => day.runs), [[run(1260, 1440)], night, night, night, night, [run(0, 180)], []]);
});

test('the band says whether an instant is inside it and when that next flips, and a band that never flips names no instant', () => {
  assert.deepEqual(d.bandNow(costs, NOW), { at: Date.parse('2026-09-16T18:00:00Z'), inside: true });
  assert.deepEqual(d.bandNow(costs, Date.parse('2026-09-16T18:00:00Z')), { at: Date.parse('2026-09-17T12:00:00Z'), inside: false });
  const always = { ...costs, peak: { ...costs.peak, fromHour: 0, toHour: 24, weekdays: false } };
  assert.deepEqual(d.bandNow(always, NOW), { at: null, inside: true });
});

test("the band's runs between two instants are its own hours, clipped to the bounds, never a bucket's", () => {
  // A week of standard time from Monday 7 December 2026: 5 to 11 in Los Angeles at UTC-8 is 13:00 to 19:00 UTC.
  const at = (day, hour) => Date.UTC(2026, 11, day, hour);
  const week = d.bandRuns(costs, at(7, 0), at(14, 0));
  assert.deepEqual(week, [7, 8, 9, 10, 11].map((day) => run(at(day, 13), at(day, 19))));
  assert.ok(week.every((r) => r.from % (6 * HOUR) !== 0), 'no run starts on a six-hour bucket boundary');
  // Sixty days hold eight weeks and four weekdays: forty-four runs on the same hours.
  const sixty = d.bandRuns(costs, at(7, 0), at(7, 0) + 60 * 24 * HOUR);
  assert.equal(sixty.length, 44);
  assert.ok(sixty.every((r) => new Date(r.from).getUTCHours() === 13 && r.to - r.from === 6 * HOUR), 'every run 13:00 to 19:00 UTC');
  assert.ok(sixty.every((r) => r.from % (24 * HOUR) !== 0), 'no run starts on a midnight bucket boundary');
  assert.deepEqual(d.bandRuns(costs, at(7, 15), at(7, 17)), [run(at(7, 15), at(7, 17))]);
});

test("the next change of the band's local hours is the first later week whose runs differ, and none in its own zone", () => {
  // Berlin leaves summer time on 25 October and Los Angeles on 1 November: for that one week the band reads an hour
  // earlier in Berlin, and the week after reads as this one.
  assert.deepEqual(d.nextLocalChange(costs, NOW, 'Europe/Berlin'), { from: 'Mon 26 Oct', lasting: false, moved: false, text: '13:00 to 19:00', to: 'Fri 30 Oct' });
  assert.equal(d.nextLocalChange(costs, NOW, 'America/Los_Angeles'), null);
});

test("a change of the band's local hours that holds past its first week lasts, and names whose clocks move", () => {
  // UTC keeps its clock, so once Los Angeles leaves summer time the band reads an hour later for good.
  assert.deepEqual(d.nextLocalChange(costs, NOW, 'UTC'), { from: 'Mon 2 Nov', lasting: true, moved: true, text: '13:00 to 19:00', to: 'Fri 6 Nov' });
  // Auckland enters summer time on 27 September while Los Angeles keeps its clock: the band starts at 01:00 there,
  // on the Tuesday a Monday in Los Angeles falls on.
  assert.deepEqual(d.nextLocalChange(costs, NOW, 'Pacific/Auckland'), { from: 'Tue 29 Sep', lasting: true, moved: false, text: '01:00 to 07:00', to: 'Sat 3 Oct' });
});

test("the band's local hours read on the clock a caller passes, and a run past midnight says it ends the next day", () => {
  const clock12 = (minutes) => new Date(Date.UTC(2000, 0, 1, 0, minutes)).toLocaleTimeString('en-US', { hour: '2-digit', hourCycle: 'h12', minute: '2-digit', timeZone: 'UTC' });
  assert.equal(d.nextLocalChange(costs, NOW, 'Europe/Berlin', clock12)?.text, '01:00 PM to 07:00 PM');
  // Tokyo at UTC+9 reads the band's 5 to 11 in Los Angeles as 21:00 to 03:00.
  assert.equal(d.bandCaption(costs, NOW, 'Asia/Tokyo', clock24), 'Mon to Fri, 21:00 to 03:00 the next day here (Asia/Tokyo). In Los Angeles: Mon to Fri, 05:00 to 11:00.');
});

test('the models list family by family, Fable, Opus, Sonnet, Haiku, the newest version first, whatever order a file keeps', () => {
  // The test tables' models in reverse, a dated id, and a family the order does not name, which goes last.
  const shuffled = [...Object.keys(costs.modelWeight).reverse(), 'claude-mystery-2', 'claude-opus-4-8', 'claude-sonnet-5-20250101'];
  assert.deepEqual(d.familyOrder(shuffled), ['claude-fable-5-1', 'claude-opus-5-5', 'claude-opus-5', 'claude-opus-4-8', 'claude-sonnet-5-5', 'claude-sonnet-5-20250101', 'claude-haiku-4-5', 'claude-mystery-2']);
  assert.deepEqual(shuffled.slice(0, 2), ['claude-haiku-4-5', 'claude-sonnet-5-5'], 'the input is left as it was');
});

test("the caption names the band in the viewing zone beside the zone the file defines it in, or once where they are one", () => {
  assert.equal(d.bandCaption(costs, NOW, 'UTC', clock24), 'Mon to Fri, 12:00 to 18:00 here (UTC). In Los Angeles: Mon to Fri, 05:00 to 11:00.');
  assert.equal(d.bandCaption(costs, NOW, 'America/Los_Angeles', clock24), 'Mon to Fri, 05:00 to 11:00 in Los Angeles (America/Los_Angeles), the zone the cost model names.');
});

test("a tier's window reads as a multiple of the smallest sized tier's on that window", () => {
  assert.equal(d.smallestTier(costs, 'five_hour'), 'default_claude_ai');
  // Team's week has no capacity, so the week's smallest is Pro's 50.
  assert.equal(d.smallestTier(costs, 'seven_day'), 'default_claude_ai');
  const multiples = (window) => ['default_claude_ai', 'default_claude_max_5x', MAX_20X, 'default_claude_team'].map((tier) => d.windowMultiple(costs, tier, window));
  // 5, 25, 100 and 10 over 5; 50, 250 and 1000 over 50, Team's week unsized.
  assert.deepEqual(multiples('five_hour'), [1, 5, 20, 2]);
  assert.deepEqual(multiples('seven_day'), [1, 5, 20, null]);
});

test('the full 5-hour windows a week are the week over the window, each at its own band factor inside the band', () => {
  close(d.fullWindowsAWeek(costs, MAX_20X, false), 10, 'Max 20x outside, 1000 / 100');
  close(d.fullWindowsAWeek(costs, MAX_20X, true), 20, 'Max 20x inside, 1000 / (100 x 0.5)');
  close(d.fullWindowsAWeek(costs, 'default_claude_ai', false), 10, 'Pro outside, 50 / 5');
  close(d.fullWindowsAWeek(costs, 'default_claude_ai', true), 20, 'Pro inside, 50 / (5 x 0.5)');
  assert.equal(d.fullWindowsAWeek(costs, 'default_claude_team', false), null, "Team's unsized week");
  const bandedWeek = { ...costs, peak: { ...costs.peak, capacityFactor: { fiveHour: 0.5, sevenDay: 0.5 } } };
  close(d.fullWindowsAWeek(bandedWeek, MAX_20X, true), 10, 'a banded week, (1000 x 0.5) / (100 x 0.5)');
});
