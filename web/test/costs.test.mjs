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
