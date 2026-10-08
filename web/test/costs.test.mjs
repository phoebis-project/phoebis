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

test('a model an override weighs on the week alone is weighted, a dash on the 5-hour window and a share on the week', () => {
  // The test table leaves Opus 5 at null; this copy weighs it 3 on the week through an override.
  const weekOnly = { ...costs, overrides: { ...costs.overrides, 'claude-opus-5': { weightByWindow: { seven_day: 3 } } } };
  assert.equal(d.isWeighted(weekOnly, 'claude-opus-5'), true, 'above the divider');
  assert.equal(d.isWeighted(costs, 'claude-opus-5'), false, 'under the divider without the override');
  assert.equal(d.shareOf(weekOnly, 'five_hour', MAX_20X, 'claude-opus-5', usage({ input: 1 }), OUT), null, 'the 5-hour window');
  close(d.shareOf(weekOnly, 'seven_day', MAX_20X, 'claude-opus-5', usage({ input: 1 }), OUT), 0.003, 'the week, 3 x 1 / 1000');
});

test('a Fable model an override weighs on the Fable week alone is weighted, a dash on the shared windows and a share on its week', () => {
  // The test table weighs Fable 5.1 10; this copy leaves its shared weight null and weighs it 10 on the Fable week.
  const FABLE = 'claude-fable-5-1';
  const weekOnly = { ...costs, modelWeight: { ...costs.modelWeight, [FABLE]: null }, overrides: { ...costs.overrides, [FABLE]: { weightByWindow: { seven_day_fable: 10 } } } };
  assert.equal(d.isWeighted(weekOnly, FABLE), true, 'above the divider');
  assert.equal(d.isWeighted({ ...weekOnly, overrides: costs.overrides }, FABLE), false, 'under the divider without the override');
  assert.equal(d.shareOf(weekOnly, 'five_hour', MAX_20X, FABLE, usage({ input: 1 }), OUT), null, 'the 5-hour window');
  assert.equal(d.shareOf(weekOnly, 'seven_day', MAX_20X, FABLE, usage({ input: 1 }), OUT), null, 'the 7-day window');
  close(d.shareOf(weekOnly, 'seven_day_fable', MAX_20X, FABLE, usage({ input: 1 }), OUT), 0.02, 'the Fable week, 10 x 1 / 1000 / 0.5');
  assert.deepEqual(d.modelWeeks(weekOnly, FABLE), ['seven_day_fable'], 'a Fable model carries the Fable week');
  assert.deepEqual(d.modelWeeks(weekOnly, OPUS), [], 'an Opus model carries none');
  // Opus carries no Fable week, so a weight there prices nothing it sends.
  const opusWeek = { ...costs, modelWeight: { ...costs.modelWeight, 'claude-opus-5': null }, overrides: { ...costs.overrides, 'claude-opus-5': { weightByWindow: { seven_day_fable: 10 } } } };
  assert.equal(d.isWeighted(opusWeek, 'claude-opus-5'), false, 'a week the model does not carry weighs nothing');
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

test('a peak zone the browser cannot read leaves the band state unknown, so a share prices on a given state alone', () => {
  const unzoned = { ...costs, peak: { ...costs.peak, zone: 'Not/AZone' } };
  assert.equal(d.inBand(unzoned, IN), null);
  assert.equal(d.inBand({ ...costs, peak: { ...costs.peak, zone: '' } }, IN), null, 'the empty zone reads the same');
  assert.deepEqual(d.bandNow(unzoned, IN), { unresolved: 'Not/AZone' });
  assert.equal(d.unreachableBand(d.bandNow(unzoned, IN)), null, 'both states stay in reach');
  assert.equal(d.shareOf(unzoned, 'five_hour', MAX_20X, OPUS, usage({ input: 1 }), IN), null, 'no share on an assumed state');
  close(d.shareIn(unzoned, 'five_hour', MAX_20X, OPUS, usage({ input: 1 }), true), 0.04, 'inside the band, 2 / 100 / 0.5');
  close(d.shareIn(unzoned, 'five_hour', MAX_20X, OPUS, usage({ input: 1 }), false), 0.02, 'outside it, 2 / 100');
  assert.deepEqual(d.bandRuns(unzoned, IN - 86_400_000, IN + 86_400_000), [], 'no hours to shade');
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
const HOUR = 3_600_000;
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

test('the models list family by family, Fable, Opus, Sonnet, Haiku, the newest version first, whatever order a file keeps', () => {
  // The test tables' models in reverse, a dated id, and a family the order does not name, which goes last.
  const shuffled = [...Object.keys(costs.modelWeight).reverse(), 'claude-mystery-2', 'claude-opus-4-8', 'claude-sonnet-5-20250101'];
  assert.deepEqual(d.familyOrder(shuffled), ['claude-fable-5-1', 'claude-opus-5-5', 'claude-opus-5', 'claude-opus-4-8', 'claude-sonnet-5-5', 'claude-sonnet-5-20250101', 'claude-haiku-4-5', 'claude-mystery-2']);
  assert.deepEqual(shuffled.slice(0, 2), ['claude-haiku-4-5', 'claude-sonnet-5-5'], 'the input is left as it was');
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

test('fundingOf reads the account kind, then a fast answer, then overage, and a missing attempt as dollars', () => {
  const attempt = (over) => ({ accountKind: 'oauth', overageInUse: false, response: { speed: 'standard' }, ...over });
  assert.equal(d.fundingOf(attempt({ accountKind: 'api_key', overageInUse: null, response: { speed: 'fast' } })), 'api_key', "an API-key account's answer bills in dollars whatever its speed");
  assert.equal(d.fundingOf(attempt({ overageInUse: true, response: { speed: 'fast' } })), 'fast', 'a fast answer draws usage credits whatever the overage flag');
  assert.equal(d.fundingOf(attempt({ overageInUse: true })), 'overage');
  assert.equal(d.fundingOf(attempt({})), 'plan');
  assert.equal(d.fundingOf(attempt({ overageInUse: null, response: { speed: null } })), 'plan', 'an unread flag and an unreported speed leave the window paying');
  assert.equal(d.fundingOf(null), 'api_key', 'a request with no attempt is billed in dollars');
});
