import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

const attempt = (accountKind, overageInUse, speed) => ({ accountKind, overageInUse, response: { speed } });
const PLAN = attempt('oauth', false, 'standard');
const request = (costShares, attempts = [PLAN], costUsd = 0.0048) => ({ attempts, costShares, costUsd });

test('a plan request with shares reads the model week over the 5-hour share in their short words, with every share in full and the equivalent on the title', () => {
  const unit = d.costUnitOf(request({ seven_day_fable: 0.0012, five_hour: 0.0042, seven_day: 0.00061 }));
  assert.deepEqual(
    unit,
    { kind: 'shares', shares: [{ key: 'five_hour', share: 0.0042 }, { key: 'seven_day', share: 0.00061 }, { key: 'seven_day_fable', share: 0.0012 }], usd: 0.0048 },
    'the shares read 5-hour, 7-day, then the model week, whatever order the route answered them in',
  );
  assert.deepEqual(d.costCellOf(unit), { main: '0.12% Fable week', sub: '0.42% 5h', title: '0.42% of 5-hour, 0.061% of 7-day, 0.12% of 7-day Fable, $0.0048 equivalent' });
});

test('a plan request of a model with no week reads the 7-day share', () => {
  const unit = d.costUnitOf(request({ five_hour: 0.0042, seven_day: 0.00061 }));
  assert.equal(unit.kind, 'shares');
  assert.deepEqual(d.costCellOf(unit), { main: '0.061% 7d', sub: '0.42% 5h', title: '0.42% of 5-hour, 0.061% of 7-day, $0.0048 equivalent' });
});

test('a plan request with a window the route could not price reads the priced ones alone', () => {
  const unit = d.costUnitOf(request({ five_hour: 0.0042, seven_day: 0.00061, seven_day_fable: null }));
  assert.deepEqual(unit.shares, [{ key: 'five_hour', share: 0.0042 }, { key: 'seven_day', share: 0.00061 }], 'a null window draws nothing');
  const cell = d.costCellOf(unit);
  assert.equal(cell.main, '0.061% 7d');
  assert.equal(cell.sub, '0.42% 5h');
  assert.equal(cell.title, '0.42% of 5-hour, 0.061% of 7-day, $0.0048 equivalent', 'the title names the two shares the route priced');
});

test('an overage request, a fast answer and an API key request read dollars over their funding word', () => {
  for (const [a, funding, word] of [
    [attempt('oauth', true, 'standard'), 'overage', 'overage'],
    [attempt('oauth', false, 'fast'), 'fast', 'fast mode'],
    [attempt('api_key', null, 'standard'), 'api_key', 'API key'],
  ]) {
    const unit = d.costUnitOf(request(null, [PLAN, a], 1.25));
    assert.deepEqual(unit, { funding, kind: 'dollars', usd: 1.25 }, `${word}: the serving attempt's funding, the last of the trail`);
    assert.deepEqual(d.costCellOf(unit), { main: '$1.25', sub: word, title: `Billed in dollars: ${word}` });
    assert.equal(d.FUNDING_WORD[funding], word);
  }
});

test('a plan request the route priced no share for reads dollars over equivalent with the no-share title', () => {
  assert.equal(d.NO_SHARE_TITLE, 'The cost model prices no share for this request');
  for (const shares of [{}, { five_hour: null, seven_day: null }, null]) {
    const unit = d.costUnitOf(request(shares));
    assert.deepEqual(unit, { kind: 'unpriced', usd: 0.0048 }, JSON.stringify(shares));
    assert.deepEqual(d.costCellOf(unit), { main: '$0.0048', sub: 'equivalent', title: d.NO_SHARE_TITLE }, JSON.stringify(shares));
  }
});

test('a plan request priced on the 5-hour window alone reads it once, and one priced on its week alone reads the week', () => {
  const fiveHour = d.costUnitOf(request({ five_hour: 0.0042 }));
  assert.deepEqual(d.costCellOf(fiveHour), { main: '0.42% 5h', sub: '', title: '0.42% of 5-hour, $0.0048 equivalent' }, 'the 5-hour share is the main line, so the second line stays empty');
  const week = d.costUnitOf(request({ seven_day_fable: 0.0012 }));
  assert.deepEqual(d.costCellOf(week), { main: '0.12% Fable week', sub: '', title: '0.12% of 7-day Fable, $0.0048 equivalent' });
});

test('a request with no attempt reads its zero dollars with no funding word and no title', () => {
  const unit = d.costUnitOf(request(null, [], 0));
  assert.deepEqual(unit, { kind: 'unserved', usd: 0 }, 'no account served it, so no funding is named');
  assert.deepEqual(d.costCellOf(unit), { main: '$0.00', sub: '', title: '' });
});
