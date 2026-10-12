import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

const attempt = (accountKind, overageInUse, speed) => ({ accountKind, overageInUse, response: { speed } });
const PLAN = attempt('oauth', false, 'standard');
const request = (costShares, attempts = [PLAN], costUsd = 0.0048, more = {}) => ({ attempts, costEquivalent: null, costShares, costUsd, estimated: false, ...more });
/** The positions under which a plan request reads its own window shares, as the Requests page drew it before the switch. */
const SHARE_POSITIONS = ['all', 'equivalent', 'share', 'usd'];
const POSITIONS = ['native', ...SHARE_POSITIONS];
/** A cell title as an estimated figure carries it: its own title, then the estimate as a sentence of its own. */
const markedTitle = (title) => `${title}. ${d.ESTIMATED_TITLE}.`;

test('a plan request with shares reads the model week over the 5-hour share in their short words, with every share in full and the equivalent on the title', () => {
  for (const position of SHARE_POSITIONS) {
    const unit = d.costUnitOf(request({ seven_day_fable: 0.0012, five_hour: 0.0042, seven_day: 0.00061 }), position);
    assert.deepEqual(
      unit,
      { estimated: false, kind: 'shares', shares: [{ key: 'five_hour', share: 0.0042 }, { key: 'seven_day', share: 0.00061 }, { key: 'seven_day_fable', share: 0.0012 }], usd: 0.0048 },
      `${position}: the shares read 5-hour, 7-day, then the model week, whatever order the route answered them in`,
    );
    assert.deepEqual(d.costCellOf(unit), { main: '0.12% Fable week', sub: '0.42% 5h', title: '0.42% of 5-hour, 0.061% of 7-day, 0.12% of 7-day Fable, $0.0048 equivalent' }, position);
  }
});

test('a plan request of a model with no week reads the 7-day share', () => {
  for (const position of SHARE_POSITIONS) {
    const unit = d.costUnitOf(request({ five_hour: 0.0042, seven_day: 0.00061 }), position);
    assert.equal(unit.kind, 'shares', position);
    assert.deepEqual(d.costCellOf(unit), { main: '0.061% 7d', sub: '0.42% 5h', title: '0.42% of 5-hour, 0.061% of 7-day, $0.0048 equivalent' }, position);
  }
});

test('a plan request with a window the route could not price reads the priced ones alone', () => {
  for (const position of SHARE_POSITIONS) {
    const unit = d.costUnitOf(request({ five_hour: 0.0042, seven_day: 0.00061, seven_day_fable: null }), position);
    assert.deepEqual(unit.shares, [{ key: 'five_hour', share: 0.0042 }, { key: 'seven_day', share: 0.00061 }], `${position}: a null window draws nothing`);
    const cell = d.costCellOf(unit);
    assert.equal(cell.main, '0.061% 7d', position);
    assert.equal(cell.sub, '0.42% 5h', position);
    assert.equal(cell.title, '0.42% of 5-hour, 0.061% of 7-day, $0.0048 equivalent', `${position}: the title names the two shares the route priced`);
  }
});

test('an overage request, a fast answer and an API key request read dollars over their funding word under every position', () => {
  for (const [a, funding, word] of [
    [attempt('oauth', true, 'standard'), 'overage', 'overage'],
    [attempt('oauth', false, 'fast'), 'fast', 'fast mode'],
    [attempt('api_key', null, 'standard'), 'api_key', 'API key'],
  ]) {
    for (const position of POSITIONS) {
      const unit = d.costUnitOf(request(null, [PLAN, a], 1.25), position);
      assert.deepEqual(unit, { estimated: false, funding, kind: 'dollars', usd: 1.25 }, `${word} under ${position}: the serving attempt's funding, the last of the trail`);
      assert.deepEqual(d.costCellOf(unit), { main: '$1.25', sub: word, title: `Billed in dollars: ${word}` }, `${word} under ${position}`);
    }
    assert.equal(d.FUNDING_WORD[funding], word);
  }
});

test('a plan request the route priced no share for reads dollars over equivalent with the no-share title', () => {
  assert.equal(d.NO_SHARE_TITLE, 'The cost model prices no share for this request');
  for (const position of SHARE_POSITIONS) {
    for (const shares of [{}, { five_hour: null, seven_day: null }, null]) {
      const unit = d.costUnitOf(request(shares), position);
      assert.deepEqual(unit, { estimated: false, kind: 'unpriced', usd: 0.0048 }, `${position} ${JSON.stringify(shares)}`);
      assert.deepEqual(d.costCellOf(unit), { main: '$0.0048', sub: 'equivalent', title: d.NO_SHARE_TITLE }, `${position} ${JSON.stringify(shares)}`);
    }
  }
});

test('a plan request priced on the 5-hour window alone reads it once, and one priced on its week alone reads the week', () => {
  for (const position of SHARE_POSITIONS) {
    const fiveHour = d.costUnitOf(request({ five_hour: 0.0042 }), position);
    assert.deepEqual(d.costCellOf(fiveHour), { main: '0.42% 5h', sub: '', title: '0.42% of 5-hour, $0.0048 equivalent' }, `${position}: the 5-hour share is the main line, so the second line stays empty`);
    const week = d.costUnitOf(request({ seven_day_fable: 0.0012 }), position);
    assert.deepEqual(d.costCellOf(week), { main: '0.12% Fable week', sub: '', title: '0.12% of 7-day Fable, $0.0048 equivalent' }, position);
  }
});

test('a request with no attempt reads its zero dollars with no funding word and no title under every position', () => {
  for (const position of POSITIONS) {
    const unit = d.costUnitOf(request(null, [], 0), position);
    assert.deepEqual(unit, { estimated: false, kind: 'unserved', usd: 0 }, `${position}: no account served it, so no funding is named`);
    assert.deepEqual(d.costCellOf(unit), { main: '$0.00', sub: '', title: '' }, position);
  }
});

test('under native a plan request reads its 20x equivalents: the 5-hour one over the 7-day one in their short words, every equivalent in full and the dollars on the title', () => {
  const r = request({ five_hour: 0.0042, seven_day: 0.00061, seven_day_fable: 0.0012 }, [PLAN], 0.0048, { costEquivalent: { seven_day_fable: 0.02222, five_hour: 0.014667, seven_day: 0.002444 } });
  const unit = d.costUnitOf(r, 'native');
  assert.deepEqual(
    unit,
    { equivalents: [{ key: 'five_hour', share: 0.014667 }, { key: 'seven_day', share: 0.002444 }, { key: 'seven_day_fable', share: 0.02222 }], estimated: false, kind: 'equivalent', usd: 0.0048 },
    'the equivalents read 5-hour, 7-day, then the model week, whatever order the route answered them in, and the own-window shares are not read',
  );
  assert.deepEqual(d.costCellOf(unit), { main: '1.47% 20x 5h', sub: '0.244% 20x 7d', title: '1.47% of 20x 5-hour, 0.244% of 20x 7-day, 2.22% of 20x Fable week, $0.0048 equivalent' });
});

test('under native a plan request priced on the 5-hour window alone reads it once, with the null 7-day window left out', () => {
  const unit = d.costUnitOf(request(null, [PLAN], 0.0048, { costEquivalent: { five_hour: 0.01, seven_day: null } }), 'native');
  assert.deepEqual(unit, { equivalents: [{ key: 'five_hour', share: 0.01 }], estimated: false, kind: 'equivalent', usd: 0.0048 });
  assert.deepEqual(d.costCellOf(unit), { main: '1% 20x 5h', sub: '', title: '1% of 20x 5-hour, $0.0048 equivalent' });
});

test('under native a plan request with no 5-hour equivalent leads with its 7-day one, the model week under it', () => {
  const unit = d.costUnitOf(request(null, [PLAN], 0.0048, { costEquivalent: { five_hour: null, seven_day: 0.002444, seven_day_fable: 0.02222 } }), 'native');
  assert.deepEqual(unit.equivalents, [{ key: 'seven_day', share: 0.002444 }, { key: 'seven_day_fable', share: 0.02222 }], 'the null 5-hour window draws nothing');
  assert.deepEqual(d.costCellOf(unit), { main: '0.244% 20x 7d', sub: '2.22% 20x Fable week', title: '0.244% of 20x 7-day, 2.22% of 20x Fable week, $0.0048 equivalent' });
});

test('under native a plan request of a model weighed on its week alone reads the week once', () => {
  const unit = d.costUnitOf(request(null, [PLAN], 0.0048, { costEquivalent: { seven_day_fable: 0.02222 } }), 'native');
  assert.deepEqual(unit, { equivalents: [{ key: 'seven_day_fable', share: 0.02222 }], estimated: false, kind: 'equivalent', usd: 0.0048 });
  assert.deepEqual(d.costCellOf(unit), { main: '2.22% 20x Fable week', sub: '', title: '2.22% of 20x Fable week, $0.0048 equivalent' });
});

test('under native a plan request the route priced no equivalent for reads dollars over equivalent with no estimated mark', () => {
  for (const costEquivalent of [null, {}, { five_hour: null, seven_day: null }]) {
    const unit = d.costUnitOf(request({ five_hour: 0.0042, seven_day: 0.00061 }, [PLAN], 0.0048, { costEquivalent }), 'native');
    assert.deepEqual(unit, { estimated: false, kind: 'unpriced', usd: 0.0048 }, JSON.stringify(costEquivalent));
    assert.deepEqual(d.costCellOf(unit), { main: '$0.0048', sub: 'equivalent', title: d.NO_SHARE_TITLE }, JSON.stringify(costEquivalent));
  }
});

test('a figure the route marked estimated reads a tilde before its main line and the estimate after its own title, under native and under share', () => {
  const r = request({ five_hour: 0.0042, seven_day: 0.00061 }, [PLAN], 0.0048, { costEquivalent: { five_hour: 0.014667, seven_day: 0.002444 }, estimated: true });
  const native = d.costUnitOf(r, 'native');
  assert.equal(native.estimated, true, 'the equivalent kind carries the flag');
  assert.deepEqual(d.costCellOf(native), { main: '~1.47% 20x 5h', sub: '0.244% 20x 7d', title: markedTitle('1.47% of 20x 5-hour, 0.244% of 20x 7-day, $0.0048 equivalent') });
  const share = d.costUnitOf(r, 'share');
  assert.equal(share.estimated, true, 'the shares kind carries the flag');
  assert.deepEqual(d.costCellOf(share), { main: '~0.061% 7d', sub: '0.42% 5h', title: markedTitle('0.42% of 5-hour, 0.061% of 7-day, $0.0048 equivalent') });
});

test('a billed request the route marked estimated reads the tilde and the estimate under usd, and every kind carries the mark', () => {
  const billed = d.costUnitOf(request(null, [attempt('api_key', null, 'standard')], 1.25, { estimated: true }), 'usd');
  assert.deepEqual(billed, { estimated: true, funding: 'api_key', kind: 'dollars', usd: 1.25 });
  assert.deepEqual(d.costCellOf(billed), { main: '~$1.25', sub: 'API key', title: markedTitle('Billed in dollars: API key') });
  assert.deepEqual(d.costCellOf({ estimated: true, kind: 'unpriced', usd: 0.0048 }), { main: '~$0.0048', sub: 'equivalent', title: markedTitle(d.NO_SHARE_TITLE) });
  assert.deepEqual(d.costCellOf({ estimated: true, kind: 'unserved', usd: 0 }), { main: '~$0.00', sub: '', title: `${d.ESTIMATED_TITLE}.` }, 'an empty title reads the estimate alone');
});
