import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import * as d from './dist/api.js';

// The test tables, round throughout. The weights are Opus 5.5 2, Haiku 4.5 0.5 (1 on the week through its override,
// with a cache read at 0.2) and Opus 5 null. The multipliers are cache read 0.1, writes 2, input 1 and output 10. The
// capacities are Max 20x 100 on the 5-hour window and 1000 on the week, Pro 5 and 50, and Team 10 and null. The peak
// band runs weekdays 5 to 11 in Los Angeles at a 5-hour capacity factor of 0.5, and the 7-day Fable window is half the
// 7-day window. The regime desk, for the entrypoints desk and pane, sizes Max 20x 200 and 1250 and Pro 10 and 62.5 and
// holds both windows whole in the band.
const tables = JSON.parse(readFileSync(new URL('./fixtures/test_tables.json', import.meta.url), 'utf8')).body;
const costs = d.decodeSubscriptionCosts(tables.subscription_costs.content, tables.subscription_costs.unit);

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

test("a share is the tokens' units over the tier's capacity, each token type at its own rate", () => {
  close(d.shareIn(costs, 'five_hour', MAX_20X, OPUS, usage({ cacheRead: 10, input: 1, output: 1 }), false), 0.24, 'mixed usage, (10 x 0.2 + 2 + 20) / 100');
});

test("an override replaces the multiplier and the window's weight it names and leaves the rest of the row alone", () => {
  close(d.shareIn(costs, 'seven_day', MAX_20X, 'claude-haiku-4-5', usage({ cacheRead: 1 }), false), 0.0002, "Haiku's cache read on the week, 1 x 0.2 / 1000");
  close(d.shareIn(costs, 'five_hour', MAX_20X, 'claude-haiku-4-5', usage({ input: 1 }), false), 0.005, "Haiku's input on the 5-hour window, 0.5 x 1 / 100");
});

test('a Fable model an override weighs on the 7-day Fable window alone is weighted, a dash on the shared windows and a share on its week', () => {
  // The test table weighs Fable 5.1 10. This copy leaves its shared weight null and weighs it 10 on the 7-day Fable
  // window alone.
  const FABLE = 'claude-fable-5-1';
  const weekOnly = { ...costs, modelWeight: { ...costs.modelWeight, [FABLE]: null }, overrides: { ...costs.overrides, [FABLE]: { weightByWindow: { seven_day_fable: 10 } } } };
  assert.equal(d.shareIn(weekOnly, 'five_hour', MAX_20X, FABLE, usage({ input: 1 }), false), null, 'the 5-hour window');
  close(d.shareIn(weekOnly, 'seven_day_fable', MAX_20X, FABLE, usage({ input: 1 }), false), 0.02, 'the 7-day Fable window, 10 x 1 / 1000 / 0.5');
  assert.deepEqual(d.modelWeeks(weekOnly, FABLE), ['seven_day_fable'], 'a Fable model carries the 7-day Fable window');
  assert.deepEqual(d.modelWeeks(weekOnly, OPUS), [], 'an Opus model carries none');
  // Opus carries no 7-day Fable window, so a weight there prices nothing it sends.
  const opusWeek = { ...costs, modelWeight: { ...costs.modelWeight, 'claude-opus-5': null }, overrides: { ...costs.overrides, 'claude-opus-5': { weightByWindow: { seven_day_fable: 10 } } } };
  assert.deepEqual(d.weightedWindows(opusWeek, 'claude-opus-5'), [], 'a week the model does not carry weighs nothing');
});

test('a dated id reads its undated weight, and an unsized tier prices no share', () => {
  close(d.shareIn(costs, 'five_hour', MAX_20X, 'claude-opus-5-5-20260101', usage({ input: 1 }), false), 0.02, 'the dated id');
  assert.equal(d.shareIn(costs, 'seven_day', 'default_claude_team', OPUS, usage({ input: 1 }), false), null, "the Team week's null capacity");
  assert.equal(d.ratesOf(costs, 'five_hour_extra', MAX_20X, OPUS), null, 'a window the file does not price');
  assert.equal(d.weightOf(costs, 'claude-mystery-2', 'five_hour'), null, 'a model tier with no weighed model');
});

test("a model without a weight of its own prices nothing, though its model tier weighs another", () => {
  assert.equal(d.weightOf(costs, 'claude-opus-5', 'seven_day'), null, "Opus 5's null row, beside Opus 5.5's weight of 2");
});

test("the Pro-week figure is the share of the anchor tier's 7-day window", () => {
  close(d.equivalentIn(costs, OPUS, usage({ input: 1 }), false), 2 / 50, "Opus 5.5's input over Pro's week, 2 / 50");
});

test("the chosen week's scale is the anchor tier's 7-day capacity over the chosen tier's, and 1 under Pro without the cost model", () => {
  assert.equal(d.weekScale(costs, 'pro'), 1);
  close(d.weekScale(costs, 'max'), 50 / 250, 'Pro over Max 5x');
  close(d.weekScale(costs, 'ultra'), 50 / 1000, 'Pro over Max 20x');
  assert.equal(d.weekScale(null, 'pro'), 1, 'the Pro-week figure reads without the table');
  assert.equal(d.weekScale(null, 'max'), null, 'another week needs the table');
  // Max 20x's week at half its size while its 5-hour window stays, so a scale read off the 5-hour window reads apart.
  const halved = { ...costs, capacity: { ...costs.capacity, [MAX_20X]: { fiveHour: 100, sevenDay: 500 } } };
  close(d.weekScale(halved, 'ultra'), 50 / 500, 'the 7-day window, not the 5-hour one');
});

test("a loaded table that sizes no week for the chosen tier reads unpriced, and with no table the reading waits or carries the read's failure", () => {
  // Max 5x's week unsized while Pro's stays, so the Max week has no scale over a table that did load.
  const maxless = { ...costs, capacity: { ...costs.capacity, default_claude_max_5x: { fiveHour: 25, sevenDay: null } } };
  assert.deepEqual(d.weekReadingOf(maxless, null, 'max'), { failure: { text: 'Unpriced: the cost model holds no 7-day capacity for the Max week', unpriced: true }, scale: null, week: 'max' }, 'a loaded table never reads pending');
  assert.deepEqual(d.weekReadingOf(null, null, 'max'), { failure: null, scale: null, week: 'max' }, 'the read still out');
  assert.deepEqual(d.weekReadingOf(null, 'HTTP 503', 'max'), { failure: { text: 'HTTP 503', unpriced: false }, scale: null, week: 'max' }, "the read's failure");
});

test("a tier's capacity is read as stored, then with the default_ prefix taken off or put on", () => {
  assert.equal(d.tierCapacity(costs, 'claude_ai', 'five_hour'), 5);
  assert.equal(d.tierCapacity(costs, 'default_claude_team', 'seven_day'), null);
  assert.equal(d.tierCapacity(costs, 'claude_enterprise', 'five_hour'), null);
  assert.equal(d.anchorTier(costs), 'default_claude_ai', "the anchor is the envelope's unit tier");
});

test("the band factor is the reciprocal of the priced window's capacity factor inside the band, and 1 outside it", () => {
  assert.equal(d.bandFactor(costs, 'five_hour', IN), 2);
  assert.equal(d.bandFactor(costs, 'seven_day', IN), 1);
  assert.equal(d.bandFactor(costs, 'five_hour', OUT), 1);
  assert.equal(d.bandFactor(costs, 'five_hour', WEEKEND), 1);
});

test('a week the file does not list prices as the 7-day share once', () => {
  assert.deepEqual(d.pricedWindow(costs, 'seven_day_other'), { divisor: 1, priced: 'seven_day' });
});

test('a peak zone the browser cannot read leaves the band state unknown, so a share prices on a given state alone', () => {
  const unzoned = { ...costs, peak: { ...costs.peak, zone: 'Not/AZone' } };
  assert.equal(d.inBand(unzoned, IN), null);
  assert.equal(d.inBand({ ...costs, peak: { ...costs.peak, zone: '' } }, IN), null, 'the empty zone reads the same');
  assert.deepEqual(d.bandNow(unzoned, IN), { unresolved: 'Not/AZone' });
  assert.equal(d.unreachableBand(d.bandNow(unzoned, IN)), null, 'both states stay in reach');
  close(d.shareIn(unzoned, 'five_hour', MAX_20X, OPUS, usage({ input: 1 }), true), 0.04, 'inside the band, 2 / 100 / 0.5');
  close(d.shareIn(unzoned, 'five_hour', MAX_20X, OPUS, usage({ input: 1 }), false), 0.02, 'outside it, 2 / 100');
  assert.equal(d.nextLocalChange(unzoned, IN, 'Europe/Berlin'), null);
  const clock = (minutes) => `${String(Math.floor(minutes / 60)).padStart(2, '0')}:${String(minutes % 60).padStart(2, '0')}`;
  assert.equal(d.bandCaption(unzoned, IN, 'Europe/Berlin', clock), 'Mon to Fri, 05:00 to 11:00 in AZone (Not/AZone), the zone the cost model names.', 'the hours in the zone the file names alone');
});

test("a band day's interval is its hours in the band's own zone, read through that day's clock change", () => {
  const span = (peak, date) => d.bandSpan({ ...costs, peak: { ...costs.peak, ...peak } }, date);
  // Los Angeles leaves summer time on Sunday 1 November 2026 at 02:00: 5 to 11 there reads at UTC-8,
  // 13:00 to 19:00 UTC.
  assert.deepEqual(span({ weekdays: false }, '2026-11-01'), { end: Date.parse('2026-11-01T19:00:00Z'), start: Date.parse('2026-11-01T13:00:00Z') });
  // It enters summer time on Sunday 14 March 2027, 02:00 becoming 03:00 at 10:00 UTC: 5 to 11 reads at UTC-7.
  assert.deepEqual(span({ weekdays: false }, '2027-03-14'), { end: Date.parse('2027-03-14T18:00:00Z'), start: Date.parse('2027-03-14T12:00:00Z') });
  // A band from 2 starts at the change, 03:00 at UTC-7, since 02:00 never reads that day.
  assert.deepEqual(span({ fromHour: 2, toHour: 4, weekdays: false }, '2027-03-14'), { end: Date.parse('2027-03-14T11:00:00Z'), start: Date.parse('2027-03-14T10:00:00Z') });
  // On 7 November 2027 01:00 to 02:00 reads twice, at UTC-7 from 08:00 UTC and at UTC-8 from 09:00 UTC: the band from 1
  // to 2 holds both and ends at 02:00 at UTC-8.
  assert.deepEqual(span({ fromHour: 1, toHour: 2, weekdays: false }, '2027-11-07'), { end: Date.parse('2027-11-07T10:00:00Z'), start: Date.parse('2027-11-07T08:00:00Z') });
  assert.equal(span({}, '2026-09-19'), null, 'a Saturday under weekdays');
  assert.deepEqual(span({ fromHour: 0, toHour: 24, weekdays: false }, '2026-09-19'), { end: Date.parse('2026-09-20T07:00:00Z'), start: Date.parse('2026-09-19T07:00:00Z') }, 'to 24 is the next midnight');
  assert.deepEqual(d.bandSpan({ ...costs, peak: { ...costs.peak, zone: 'Not/AZone' } }, '2026-09-16'), { unresolved: 'Not/AZone' });
});

// The band in a zone. Wednesday 16 September 2026 at 14:30 UTC, 07:30 in Los Angeles: inside the test table's band of
// weekdays 5 to 11 there, which reads 12:00 to 18:00 UTC while Los Angeles keeps summer time.
const NOW = Date.parse('2026-09-16T14:30:00Z');
const run = (from, to) => ({ from, to });
const clock24 = (minutes) => `${String(Math.floor(minutes / 60)).padStart(2, '0')}:${String(minutes % 60).padStart(2, '0')}`;
const WEEK_LABELS = ['Mon 14 Sep', 'Tue 15 Sep', 'Wed 16 Sep', 'Thu 17 Sep', 'Fri 18 Sep', 'Sat 19 Sep', 'Sun 20 Sep'];

test("the week drawn runs Monday to Sunday in the viewing zone, from the later of the day holding the instant and the day today's band starts on, the band's runs per day in its minutes", () => {
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

test('a band on at every hour leaves Outside out of reach and says why, and a band that flips leaves both in reach', () => {
  const always = { ...costs, peak: { ...costs.peak, fromHour: 0, toHour: 24, weekdays: false } };
  assert.deepEqual(d.unreachableBand(d.bandNow(always, NOW)), { inside: false, title: 'The band is on at every hour.' });
  assert.equal(d.unreachableBand(d.bandNow(costs, NOW)), null);
});

test("the next change of the band's local hours runs from the first later band day that differs from today's until one reads as today's, and none in its own zone", () => {
  // Berlin leaves summer time on 25 October and Los Angeles on 1 November: for that one week the band reads an hour
  // earlier in Berlin, and the Monday after reads as today.
  assert.deepEqual(d.nextLocalChange(costs, NOW, 'Europe/Berlin'), { from: 'Mon 26 Oct', lasting: false, moved: false, text: '13:00 to 19:00', to: 'Fri 30 Oct' });
  // Los Angeles enters summer time on 14 March 2027 and Berlin on 28 March: the two weeks between read an hour
  // earlier in Berlin, the weekend inside them continuing the stretch, and 29 March reads as this week again.
  assert.deepEqual(d.nextLocalChange(costs, Date.parse('2027-02-24T12:00:00Z'), 'Europe/Berlin'), { from: 'Mon 15 Mar', lasting: false, moved: true, text: '13:00 to 19:00', to: 'Fri 26 Mar' });
  assert.equal(d.nextLocalChange(costs, NOW, 'America/Los_Angeles'), null);
});

test('a change opens on the first band day that differs from today, this week included, and reads to its own end however far apart the two zones change', () => {
  // Every day from Wednesday 21 October 2026, 12:00 to 18:00 UTC and 14:00 to 20:00 in Berlin: Berlin leaves summer
  // time on Sunday 25 October, so the band reads 13:00 to 19:00 there from that day, and on Sunday 1 November Los
  // Angeles leaves it too, 13:00 to 19:00 UTC and 14:00 to 20:00 in Berlin again. The change opens this week.
  assert.deepEqual(d.nextLocalChange({ ...costs, peak: { ...costs.peak, weekdays: false } }, Date.parse('2026-10-21T12:00:00Z'), 'Europe/Berlin'), { from: 'Sun 25 Oct', lasting: false, moved: false, text: '13:00 to 19:00', to: 'Sat 31 Oct' });
  // Monday 4 January 2027 in Cairo, at UTC+2: 13:00 to 19:00 UTC, 15:00 to 21:00 there. From Monday 15 March, exactly
  // ten weeks on, Los Angeles keeps summer time and the band reads 14:00 to 20:00; Cairo enters summer time, UTC+3, on
  // Friday 30 April at 00:00, so that day reads 15:00 to 21:00 again, seven weeks after the change opened.
  assert.deepEqual(d.nextLocalChange(costs, Date.parse('2027-01-04T12:00:00Z'), 'Africa/Cairo'), { from: 'Mon 15 Mar', lasting: false, moved: true, text: '14:00 to 20:00', to: 'Thu 29 Apr' });
});

test("on a day the band is off, today is the next band day: its week, its hours and the clocks the change is read against", () => {
  // Saturday 24 October 2026 in Los Angeles, the weekend Berlin leaves summer time: today is Monday 26 October, 12:00
  // to 18:00 UTC at UTC-7, 13:00 to 19:00 in Berlin at UTC+1, and the week drawn is that Monday's, with no day of it
  // holding now. Los Angeles leaves summer time on 1 November, so from Monday 2 November the band reads 14:00 to 20:00
  // there, and 13:00 to 19:00 again on Monday 15 March 2027, once Los Angeles keeps summer time and Berlin not yet.
  const at = Date.parse('2026-10-24T12:00:00Z');
  const week = d.bandWeek(costs, at, 'Europe/Berlin');
  assert.deepEqual(week.map((day) => day.label), ['Mon 26 Oct', 'Tue 27 Oct', 'Wed 28 Oct', 'Thu 29 Oct', 'Fri 30 Oct', 'Sat 31 Oct', 'Sun 1 Nov']);
  assert.deepEqual(week.map((day) => day.runs), [[run(780, 1140)], [run(780, 1140)], [run(780, 1140)], [run(780, 1140)], [run(780, 1140)], [], []]);
  assert.deepEqual(week.map((day) => day.today), [false, false, false, false, false, false, false], 'now lies before the week drawn');
  assert.equal(d.bandCaption(costs, at, 'Europe/Berlin', clock24), 'Mon to Fri, 13:00 to 19:00 here (Europe/Berlin). In Los Angeles: Mon to Fri, 05:00 to 11:00.');
  assert.deepEqual(d.nextLocalChange(costs, at, 'Europe/Berlin'), { from: 'Mon 2 Nov', lasting: false, moved: true, text: '14:00 to 20:00', to: 'Fri 12 Mar' });
  // Saturday 13 March 2027 at 12:00 in Los Angeles, still at UTC-8: today is Monday 15 March, at UTC-7 after the change
  // of 14 March, 12:00 to 18:00 UTC and 13:00 to 19:00 in Berlin. Berlin enters summer time on 28 March, so from Monday
  // 29 March the band reads 14:00 to 20:00 there, Berlin's own change; it reads 13:00 to 19:00 again on Monday 1
  // November, once Berlin leaves summer time on 31 October and before Los Angeles does on 7 November, 31 weeks on.
  assert.deepEqual(d.nextLocalChange(costs, Date.parse('2027-03-13T20:00:00Z'), 'Europe/Berlin'), { from: 'Mon 29 Mar', lasting: true, moved: false, text: '14:00 to 20:00', to: null });
});

test("on a band day whose band started on the viewer's day before, the week drawn is the one holding the instant", () => {
  // Every day, Monday 19 October 2026 at 08:00 in Berlin at UTC+2, Sunday 23:00 in Los Angeles at UTC-7: today's band
  // is Sunday's, 12:00 to 18:00 UTC, which started on Sunday in Berlin, but the instant lies in the week of Monday 19,
  // which is drawn with Monday marked. Its band days read 14:00 to 20:00 in Berlin, and Sunday 25 October, once Berlin
  // is at UTC+1, 13:00 to 19:00.
  const everyDay = d.bandWeek({ ...costs, peak: { ...costs.peak, weekdays: false } }, Date.parse('2026-10-19T06:00:00Z'), 'Europe/Berlin');
  assert.deepEqual(everyDay.map((day) => day.label), ['Mon 19 Oct', 'Tue 20 Oct', 'Wed 21 Oct', 'Thu 22 Oct', 'Fri 23 Oct', 'Sat 24 Oct', 'Sun 25 Oct']);
  assert.deepEqual(everyDay.map((day) => day.runs), [...Array(6).fill([run(840, 1200)]), [run(780, 1140)]]);
  assert.deepEqual(everyDay.map((day) => day.today), [true, false, false, false, false, false, false]);
});

test('a change that starts late in the ten weeks reads past them to the day that reads as today again', () => {
  // Seen from 6 January 2027, the gap between the two spring changes opens on 15 March, inside the ten weeks, and
  // Monday 29 March, the first day that reads as today again, falls past them: the stretch seen from 24 February.
  assert.deepEqual(d.nextLocalChange(costs, Date.parse('2027-01-06T12:00:00Z'), 'Europe/Berlin'), { from: 'Mon 15 Mar', lasting: false, moved: true, text: '13:00 to 19:00', to: 'Fri 26 Mar' });
});

test("a change of the band's local hours whose hours have not returned 26 weeks on lasts, and names whose clocks move", () => {
  // UTC keeps its clock: Los Angeles leaves summer time on 1 November, so from Monday 2 November the band reads 13:00
  // to 19:00, and returns to 12:00 to 18:00 on Monday 15 March, 19 weeks on. The stretch ends on Friday 12 March.
  assert.deepEqual(d.nextLocalChange(costs, NOW, 'UTC'), { from: 'Mon 2 Nov', lasting: false, moved: true, text: '13:00 to 19:00', to: 'Fri 12 Mar' });
  // From Wednesday 6 January 2027, 13:00 to 19:00 UTC: Los Angeles enters summer time on 14 March, so from Monday 15
  // March the band reads 12:00 to 18:00, and back on Monday 8 November, 34 weeks on.
  assert.deepEqual(d.nextLocalChange(costs, Date.parse('2027-01-06T12:00:00Z'), 'UTC'), { from: 'Mon 15 Mar', lasting: true, moved: true, text: '12:00 to 18:00', to: null });
  // Today's band reads 00:00 to 06:00 on Thursday 17 September in Auckland, at UTC+12. Auckland enters summer time on
  // 27 September, so from Tuesday 29 September, the Tuesday a Monday in Los Angeles falls on, it reads 01:00 to 07:00,
  // from 3 November 02:00 to 08:00, from 16 March 01:00 to 07:00, and 00:00 to 06:00 again on Tuesday 6 April, 27
  // weeks on.
  assert.deepEqual(d.nextLocalChange(costs, NOW, 'Pacific/Auckland'), { from: 'Tue 29 Sep', lasting: true, moved: false, text: '01:00 to 07:00', to: null });
});

test("the band's local hours read on the clock a caller passes, and a run past midnight says it ends the next day", () => {
  const clock12 = (minutes) => new Date(Date.UTC(2000, 0, 1, 0, minutes)).toLocaleTimeString('en-US', { hour: '2-digit', hourCycle: 'h12', minute: '2-digit', timeZone: 'UTC' });
  assert.equal(d.nextLocalChange(costs, NOW, 'Europe/Berlin', clock12)?.text, '01:00 PM to 07:00 PM');
  // Tokyo at UTC+9 reads the band's 5 to 11 in Los Angeles as 21:00 to 03:00.
  assert.equal(d.bandCaption(costs, NOW, 'Asia/Tokyo', clock24), 'Mon to Fri, 21:00 to 03:00 the next day here (Asia/Tokyo). In Los Angeles: Mon to Fri, 05:00 to 11:00.');
});

test("the models list tier by tier in the cost model's order, the newest version first, a model of no tier last, whatever order a file keeps", () => {
  // The test tables' models in reverse, a dated id, the mystery tier the order names last, and two ids of no tier.
  const shuffled = [...Object.keys(costs.modelWeight).reverse(), 'claude-unknown-1', 'claude-mystery-2', 'claude-opus-4-8', 'claude-unknown-2', 'claude-sonnet-5-20250101'];
  assert.deepEqual(costs.tierOrder, ['fable', 'opus', 'sonnet', 'haiku', 'mystery'], 'the test tables order five model tiers');
  assert.deepEqual(d.tierOrder(shuffled, costs), ['claude-fable-5-1', 'claude-opus-5-5', 'claude-opus-5', 'claude-opus-4-8', 'claude-sonnet-5-5', 'claude-sonnet-5-20250101', 'claude-haiku-4-5', 'claude-mystery-2', 'claude-unknown-2', 'claude-unknown-1']);
  assert.deepEqual(shuffled.slice(0, 2), ['claude-haiku-4-5', 'claude-sonnet-5-5'], 'the input is left as it was');
  // Each tier's newest model leads its group, and each id of no tier is a group of its own, so none folds under
  // another.
  assert.deepEqual(d.tierGroups(shuffled, costs), [
    { models: ['claude-fable-5-1'], tier: 'fable' },
    { models: ['claude-opus-5-5', 'claude-opus-5', 'claude-opus-4-8'], tier: 'opus' },
    { models: ['claude-sonnet-5-5', 'claude-sonnet-5-20250101'], tier: 'sonnet' },
    { models: ['claude-haiku-4-5'], tier: 'haiku' },
    { models: ['claude-mystery-2'], tier: 'mystery' },
    { models: ['claude-unknown-2'], tier: null },
    { models: ['claude-unknown-1'], tier: null },
  ]);
  // The empty model names no tier, so every id is of none, by version and then id, and nothing folds.
  const empty = d.decodeSubscriptionCosts(undefined, tables.subscription_costs.unit);
  const ids = ['claude-haiku-4-5', 'claude-opus-5', 'claude-opus-5-5', 'claude-fable-5-1', 'claude-sonnet-5-5'];
  assert.deepEqual(d.tierOrder(ids, empty), ['claude-opus-5-5', 'claude-sonnet-5-5', 'claude-fable-5-1', 'claude-opus-5', 'claude-haiku-4-5']);
  assert.deepEqual(d.tierGroups(ids, empty).map((g) => g.models), [['claude-opus-5-5'], ['claude-sonnet-5-5'], ['claude-fable-5-1'], ['claude-opus-5'], ['claude-haiku-4-5']]);
  assert.ok(d.tierGroups(ids, empty).every((g) => g.tier === null));
});

test("a model_tier entry moves a model into the tier it names, in both orders", () => {
  const remapped = { ...costs, modelTier: { ...costs.modelTier, 'claude-opus-4-8': 'haiku' } };
  const ids = ['claude-haiku-4-5', 'claude-opus-4-8', 'claude-opus-5-5', 'claude-sonnet-5-5'];
  // Inside the Haiku tier Opus 4.8 is the newer version.
  assert.deepEqual(d.tierOrder(ids, remapped), ['claude-opus-5-5', 'claude-sonnet-5-5', 'claude-opus-4-8', 'claude-haiku-4-5']);
  assert.deepEqual(d.tierGroups(ids, remapped).find((g) => g.tier === 'haiku')?.models, ['claude-opus-4-8', 'claude-haiku-4-5']);
  assert.deepEqual(d.tierGroups(ids, remapped).find((g) => g.tier === 'opus')?.models, ['claude-opus-5-5']);
});

test("the caption names the band in the viewing zone beside the zone the file defines it in, or once where they are one", () => {
  assert.equal(d.bandCaption(costs, NOW, 'UTC', clock24), 'Mon to Fri, 12:00 to 18:00 here (UTC). In Los Angeles: Mon to Fri, 05:00 to 11:00.');
  assert.equal(d.bandCaption(costs, NOW, 'America/Los_Angeles', clock24), 'Mon to Fri, 05:00 to 11:00 in Los Angeles (America/Los_Angeles), the zone the cost model names.');
  // Monday 26 October 2026, the week after Berlin leaves summer time and before Los Angeles does: 12:00 to 18:00 UTC.
  assert.equal(d.bandCaption(costs, Date.parse('2026-10-26T08:00:00Z'), 'Europe/Berlin', clock24), 'Mon to Fri, 13:00 to 19:00 here (Europe/Berlin). In Los Angeles: Mon to Fri, 05:00 to 11:00.');
});

// The test table's band on weekdays from 0 to 24 in Los Angeles: its days start at 07:00 UTC and 09:00 in Berlin while
// both keep summer time.
const wholeDays = { ...costs, peak: { ...costs.peak, fromHour: 0, toHour: 24 } };

test("a weekday band of whole days reads all day where its days start at midnight here, else its week's ends", () => {
  assert.equal(d.bandCaption(wholeDays, NOW, 'UTC', clock24), 'Mon 07:00 to Sat 07:00 here (UTC). In Los Angeles: Mon to Fri, all day.');
  assert.equal(d.bandCaption(wholeDays, NOW, 'Europe/Berlin', clock24), 'Mon 09:00 to Sat 09:00 here (Europe/Berlin). In Los Angeles: Mon to Fri, all day.');
  // Berlin's band at UTC+2 starts its week at 22:00 UTC on the Sunday before and ends it at 22:00 on Friday.
  const berlin = { ...wholeDays, peak: { ...wholeDays.peak, zone: 'Europe/Berlin' } };
  assert.equal(d.bandCaption(berlin, NOW, 'UTC', clock24), 'Sun 22:00 to Fri 22:00 here (UTC). In Berlin: Mon to Fri, all day.');
  // In Auckland at UTC+12 a band day starts at 12:00 UTC on the day before, 05:00 in Los Angeles, so its week falls a
  // day earlier there.
  const auckland = { ...wholeDays, peak: { ...wholeDays.peak, zone: 'Pacific/Auckland' } };
  assert.equal(d.bandCaption(auckland, NOW, 'America/Los_Angeles', clock24), 'Sun 05:00 to Fri 05:00 here (America/Los_Angeles). In Auckland: Mon to Fri, all day.');
  // Its own zone and Paris, on Berlin's clocks, start each band day at midnight.
  assert.equal(d.bandCaption(wholeDays, NOW, 'America/Los_Angeles', clock24), 'Mon to Fri, all day in Los Angeles (America/Los_Angeles), the zone the cost model names.');
  assert.equal(d.bandCaption(berlin, NOW, 'Europe/Paris', clock24), 'Mon to Fri, all day here (Europe/Paris). In Berlin: Mon to Fri, all day.');
});

test('a whole-day week reads from its first band day to its last across a clock change', () => {
  // Israel moves its clocks on Friday 27 March 2026, inside the band's week: the Monday starts at 22:00 UTC the Sunday
  // before, at UTC+2, and the Friday ends at 21:00 UTC, at UTC+3.
  const jerusalem = { ...wholeDays, peak: { ...wholeDays.peak, zone: 'Asia/Jerusalem' } };
  const wednesday = Date.parse('2026-03-25T12:00:00Z');
  assert.equal(d.bandCaption(jerusalem, wednesday, 'UTC', clock24), 'Sun 22:00 to Fri 21:00 here (UTC). In Jerusalem: Mon to Fri, all day.');
  // Athens keeps UTC+2 through that Friday, so the week starts at midnight there and still ends an hour early.
  assert.equal(d.bandCaption(jerusalem, wednesday, 'Europe/Athens', clock24), 'Mon 00:00 to Fri 23:00 here (Europe/Athens). In Jerusalem: Mon to Fri, all day.');
});

test('an every-day whole-day band reads Every day, all day in its own zone', () => {
  const everyDay = { ...wholeDays, peak: { ...wholeDays.peak, weekdays: false } };
  assert.equal(d.bandCaption(everyDay, NOW, 'America/Los_Angeles', clock24), 'Every day, all day in Los Angeles (America/Los_Angeles), the zone the cost model names.');
  assert.equal(d.bandCaption(everyDay, NOW, 'UTC', clock24), 'Every day, at every hour here (UTC). In Los Angeles: Every day, all day.');
});

test('a weekday band of whole days still names the next change of the hour its days start at here', () => {
  // Berlin leaves summer time on 25 October and Los Angeles on 1 November: for that one week the band's days start at
  // 08:00 in Berlin, and the Monday after reads as today.
  assert.deepEqual(d.nextLocalChange(wholeDays, NOW, 'Europe/Berlin'), { from: 'Mon 26 Oct', lasting: false, moved: false, text: '08:00 to 08:00 the next day', to: 'Fri 30 Oct' });
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

test('the overlay prices a listed entrypoint under its regime and anything else at the base', () => {
  assert.deepEqual(d.regimeNames(costs), ['desk']);
  assert.notDeepEqual(d.withRegime(costs, 'desk'), costs, 'the regime changes the model');
  assert.equal(d.withRegime(costs, 'nowhere'), costs, 'an unknown name is the base itself');
  assert.equal(d.withRegime(costs, null), costs);
});

test('every reader prices through the overlay', () => {
  const over = d.withRegime(costs, 'desk');
  const u = usage({ input: 1 });
  assert.equal(d.tierCapacity(over, MAX_20X, 'five_hour'), 200);
  assert.equal(d.tierCapacity(costs, MAX_20X, 'five_hour'), 100);
  assert.equal(d.tierCapacity(over, 'default_claude_team', 'five_hour'), 10, "a tier the regime does not name keeps the base's row");
  close(d.ratesOf(over, 'five_hour', MAX_20X, OPUS)?.input, 10_000, 'input, 2 x 1 / 200 x 1e6');
  close(d.ratesOf(costs, 'five_hour', MAX_20X, OPUS)?.input, 20_000, 'input at the base, 2 x 1 / 100 x 1e6');
  close(d.shareIn(over, 'five_hour', MAX_20X, OPUS, u, true), d.shareIn(over, 'five_hour', MAX_20X, OPUS, u, false), 'the regime holds the window whole inside the band');
  close(d.shareIn(costs, 'five_hour', MAX_20X, OPUS, u, true), 2 * d.shareIn(costs, 'five_hour', MAX_20X, OPUS, u, false), "the base's inside share doubles");
  assert.equal(d.bandFactor(over, 'five_hour', IN), 1);
  assert.equal(d.bandFactor(costs, 'five_hour', IN), 2);
  close(d.fullWindowsAWeek(over, MAX_20X, false), 6.25, 'Max 20x under the regime, 1250 / 200');
  close(d.fullWindowsAWeek(costs, MAX_20X, false), 10, 'Max 20x at the base, 1000 / 100');
  assert.equal(d.smallestTier(over, 'five_hour'), 'default_claude_ai');
  // The overlay's own smallest is the regime's Pro row of 10, so a card that compares a regime's tier with the base's
  // reads its multiple against the base's smallest tier instead.
  assert.equal(d.windowMultiple(over, MAX_20X, 'five_hour'), 20, '200 / 10');
});

// The shipped regime's key and tokens over the test tables' base, whose 5-hour band factor is 0.5.
const INTERACTIVE = { capacity: { [MAX_20X]: { fiveHour: 200, sevenDay: 1250 } }, entrypoints: ['claude-desktop', 'claude-desktop-3p', 'claude-vscode', 'cli'], peakCapacityFactor: { fiveHour: 1, sevenDay: 1 } };

test('a regime reads by its headline word and the clients it prices in alphabetical order, a key or token the map lacks reading raw', () => {
  assert.equal(d.regimeWord(null), 'Headless', 'the base');
  assert.equal(d.regimeWord('desk'), 'desk', 'a key the map lacks');
  assert.equal(d.regimeTitle(costs, 'desk'), 'desk (desk, pane)', 'tokens the map lacks');
  assert.equal(d.regimeTitle(costs, null), 'Headless (Claude Code CLI headless mode, the Agent SDK, Claude in Slack, etc.)');
  const shipped = { ...costs, regimes: { interactive: INTERACTIVE } };
  assert.equal(d.regimeWord('interactive'), 'Interactive');
  assert.equal(d.regimeClients(shipped, 'interactive'), 'Claude Code CLI, Claude Desktop, Claude VSCode', 'the two desktop tokens named once, in alphabetical order');
  const withPane = { ...costs, regimes: { interactive: { ...INTERACTIVE, entrypoints: ['pane', ...INTERACTIVE.entrypoints] } } };
  assert.equal(d.regimeClients(withPane, 'interactive'), 'Claude Code CLI, Claude Desktop, Claude VSCode, pane', 'a token the map lacks, raw in its alphabetical place');
  const withSdk = { ...costs, regimes: { interactive: { ...INTERACTIVE, entrypoints: [...INTERACTIVE.entrypoints, 'agent-sdk'] } } };
  assert.equal(d.regimeClients(withSdk, 'interactive'), 'agent-sdk, Claude Code CLI, Claude Desktop, Claude VSCode', 'a lowercase token the map lacks sorts by its letters, ahead of the Claude names');
  assert.equal(d.regimeTitle(shipped, 'interactive'), 'Interactive (Claude Code CLI, Claude Desktop, Claude VSCode)');
});

test("the Peak band card's sentence names the traffic the band applies to and the sessions it spares, and reads plain where it spares none", () => {
  const interactive = INTERACTIVE;
  const shipped = { ...costs, regimes: { interactive } };
  const rest = 'a request completing inside it spends more of its 5-hour window.';
  assert.equal(d.bandSentence(shipped), `The band applies to Headless traffic only: ${rest} Interactive sessions hold the full window at every hour.`);
  const desk = { ...costs.regimes.desk, peakCapacityFactor: { fiveHour: 0.5, sevenDay: 1 } };
  assert.equal(d.bandSentence({ ...costs, regimes: { interactive, desk } }), `The band applies to Headless and desk traffic only: ${rest} Interactive sessions hold the full window at every hour.`, 'a second regime that carries the band');
  const plain = 'A request completing inside the band spends more of its 5-hour window.';
  const carried = { ...interactive, peakCapacityFactor: { fiveHour: 0.7, sevenDay: 1 } };
  assert.equal(d.bandSentence({ ...costs, regimes: { interactive: carried } }), plain, 'no regime spared');
  assert.equal(d.bandSentence({ ...shipped, peak: { ...costs.peak, capacityFactor: { fiveHour: 1, sevenDay: 1 } } }), plain, 'a band that spares the base spares no regime');
});

test('fundingOf reads the account kind, then a fast answer, then overage, and a missing attempt as dollars', () => {
  const attempt = (over) => ({ accountKind: 'oauth', overageInUse: false, response: { speed: 'standard' }, ...over });
  assert.equal(d.fundingOf(attempt({ accountKind: 'api_key', overageInUse: null, response: { speed: 'fast' } })), 'api_key', "an API-key account's answer bills in dollars whatever its speed");
  assert.equal(d.fundingOf(attempt({ overageInUse: true, response: { speed: 'fast' } })), 'fast', 'a fast answer draws usage credits whatever the overage flag');
  assert.equal(d.fundingOf(attempt({ overageInUse: true })), 'overage');
  assert.equal(d.fundingOf(attempt({})), 'plan');
  assert.equal(d.fundingOf(attempt({ overageInUse: null, response: { speed: null } })), 'plan', 'an unread flag and an unreported speed leave the window paying');
  assert.equal(d.fundingOf(null), 'api_key', 'a request with no attempt is billed in dollars');
});

test("a model's tier is its model_tier entry, then its undated id's, then the tier its id names when the order lists it", () => {
  assert.equal(d.modelTierOf(costs, OPUS), 'opus', 'a listed id');
  assert.equal(d.modelTierOf(costs, 'claude-opus-5-5-20260101'), 'opus', "a dated id reads its undated id's entry");
  assert.equal(d.modelTierOf(costs, 'claude-opus-6'), 'opus', 'an unlisted id reads the tier it names');
  assert.equal(d.modelTierOf(costs, 'claude-mystery-2'), 'mystery', 'the fifth tier');
  assert.equal(d.modelTierOf(costs, 'gpt-5'), null, 'an id of no tier');
  assert.equal(d.modelTierOf(costs, 'claude-nova-1'), null, 'a tier the order does not list');
  assert.equal(d.modelTierOf({ ...costs, modelTier: { ...costs.modelTier, 'claude-nova-1': 'opus' } }, 'claude-nova-1'), 'opus', 'the entry wins over the id');
  assert.equal(d.modelTierOf({ modelTier: {}, tierOrder: [] }, OPUS), null, 'a cost model with no tiers names none');
});

test('every helper that reads a weight answers for a weighted model and returns nothing for a model without a weight of its own', () => {
  const u = usage({ input: 1 });
  const read = (model) => ({
    ratesOf: d.ratesOf(costs, 'five_hour', MAX_20X, model),
    shareIn: d.shareIn(costs, 'five_hour', MAX_20X, model, u, false),
    weightOf: d.weightOf(costs, model, 'five_hour'),
    weightedWindows: d.weightedWindows(costs, model),
  });
  const weighted = read(OPUS);
  assert.ok(Object.values(weighted).every((v) => v !== null && !(Array.isArray(v) && v.length === 0)), `a weighted model prices through every helper: ${JSON.stringify(weighted)}`);
  // Opus 4.8 sits in the Opus tier, which weighs Opus 5.5, and carries no weight of its own, so no reader borrows one.
  for (const model of ['claude-opus-4-8', 'claude-mystery-2', 'claude-unknown-1']) assert.deepEqual(Object.values(read(model)), [null, null, null, []], `${model} prices nowhere`);
});

test('the entry in force at an instant is the latest dated at or before it, else the first', () => {
  const wired = d.decodeSettings({ api_prices: tables.api_prices, subscription_costs: tables.subscription_costs });
  const revisions = wired.apiPrices.revisions ?? [];
  assert.deepEqual(revisions.map((r) => r.effective), ['2026-01-01', '2026-02-01'], 'the test tables carry two price entries');
  const between = Date.UTC(2026, 0, 15);
  assert.equal(d.revisionAt(revisions, between), revisions[0], 'an instant between the two reads the earlier');
  assert.equal(d.revisionAt(revisions, new Date(between).toISOString()), revisions[0], 'given as an ISO string too');
  assert.equal(d.revisionAt(revisions, Date.UTC(2025, 11, 1)), revisions[0], 'an instant before the first reads the first');
  assert.equal(d.revisionAt(revisions, '2025-12-01T00:00:00.000Z'), revisions[0]);
  assert.equal(d.revisionAt(revisions, Date.parse(revisions[1].effectiveDate)), revisions[1], 'the entry is in force from its own midnight');
  assert.equal(d.revisionAt(revisions, Date.UTC(2026, 8, 16)), revisions[1], 'an instant after the last reads the last');
  assert.equal(d.revisionAt(revisions, '2026-09-16T14:30:00Z'), revisions[1]);
  assert.equal(d.revisionAt([], between), null, 'an empty list has none');
  assert.equal(d.revisionAt(wired.subscriptionCosts.revisions ?? [], between)?.content.modelWeight[OPUS], costs.modelWeight[OPUS], "the cost model's entries read the same way");
});
