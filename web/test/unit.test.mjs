import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

const LEGEND = {
  native: 'Each request in the unit its funding pays in: a subscription request as 20x window equivalents, an API key, overage or fast-mode request in dollars.',
  all: 'Every request in dollars at the API price list, a subscription request at its dollar equivalent.',
  usd: 'The overage, fast-mode and API key requests alone, the dollars the account holder is charged.',
  share: "A subscription account's requests as shares of its own 5-hour, 7-day and model-week windows. Reads where one account is named, since shares of different accounts' windows do not add.",
  equivalent: 'The subscription requests alone at the API price list.',
};
const NOTE =
  "A 20x window equivalent is the share of a Max 20x account's window the same tokens would spend, the cost model's unit over the Max 20x capacity, so figures of different account tiers compare and add. The 5-hour column carries the peak band's weight, the 7-day column the week's factor, and a model week its own capacity factor. A figure marked ~ is estimated: its model has no measured weight and reads the latest weighed model of its model tier.";

test("the positions run native first, then the Cost section's four", () => {
  assert.deepEqual([...d.UNIT_POSITIONS], ['native', 'all', 'usd', 'share', 'equivalent']);
});

test('the long and the short labels name every position and nothing else', () => {
  const positions = [...d.UNIT_POSITIONS].sort();
  assert.equal(positions.length, 5);
  assert.deepEqual(Object.keys(d.UNIT_LABEL).sort(), positions);
  assert.deepEqual(Object.keys(d.UNIT_SHORT).sort(), positions);
  assert.deepEqual(d.UNIT_LABEL, { all: 'All in dollars', equivalent: 'Dollar equivalent', native: 'Native', share: 'Window shares', usd: 'Billed dollars' });
  assert.deepEqual(d.UNIT_SHORT, { all: 'All $', equivalent: 'Equivalent $', native: 'Native', share: 'Shares', usd: 'Billed $' });
});

test("the legend explains every position and the 20x window equivalent in the spec's wording", () => {
  assert.deepEqual(Object.keys(d.UNIT_LEGEND).sort(), [...d.UNIT_POSITIONS].sort());
  assert.deepEqual(d.UNIT_LEGEND, LEGEND);
  assert.equal(d.UNIT_LEGEND_NOTE, NOTE);
});

test("the legend's closing paragraph names the account tiers and the model tier apart", () => {
  assert.ok(d.UNIT_LEGEND_NOTE.includes('figures of different account tiers compare and add'), 'the equivalent compares across account tiers');
  assert.ok(d.UNIT_LEGEND_NOTE.includes('the latest weighed model of its model tier'), "an estimate reads its model tier's latest weighed model");
  assert.deepEqual(d.UNIT_LEGEND_NOTE.match(/\b(?<!account |model )tiers?\b/g) ?? [], [], 'every tier the paragraph mentions is named');
});

test('no legend sentence joins two clauses with a semicolon', () => {
  const texts = [...Object.values(d.UNIT_LEGEND), d.UNIT_LEGEND_NOTE];
  assert.equal(texts.length, 6);
  assert.deepEqual(texts.filter((t) => t === '' || t.includes(';')), []);
});

test('a stored value the console does not know reads native', () => {
  for (const position of d.UNIT_POSITIONS) assert.equal(d.positionOf(position), position, position);
  assert.equal(d.positionOf('share'), 'share');
  assert.equal(d.positionOf('bogus'), 'native');
  assert.equal(d.positionOf(''), 'native');
  assert.equal(d.positionOf(null), 'native');
  assert.equal(d.positionOf(undefined), 'native');
});

test('native reads both sides of one answer and every other position its funding side', () => {
  assert.ok(Object.hasOwn(d.UNIT_FUNDING, 'native'), 'native is a position of the map');
  assert.equal(d.UNIT_FUNDING.native, undefined);
  assert.deepEqual(d.UNIT_FUNDING, { all: undefined, equivalent: 'plan', native: undefined, share: 'plan', usd: 'billed' });
});
