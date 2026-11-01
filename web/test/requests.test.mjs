import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

const attempt = (accountKind, overageInUse, speed) => ({ accountKind, overageInUse, response: { speed } });
const PLAN = attempt('oauth', false, 'standard');
const request = (costShares, attempts = [PLAN], costUsd = 0.0048, more = {}) => ({ attempts, costEquivalent: null, costShares, costUsd, shareReason: null, ...more });
/** The position under which a plan request reads its own window shares. */
const SHARE_POSITIONS = ['share'];
const POSITIONS = ['all', 'equivalent', 'native', 'share', 'usd'];
/** The two positions under which a plan request reads its dollar equivalent. */
const EQUIVALENT_DOLLAR_POSITIONS = ['all', 'equivalent'];

test('a plan request with shares reads the model week over the 5-hour share in their short words, with every share in full and the equivalent on the title', () => {
  for (const position of SHARE_POSITIONS) {
    const unit = d.costUnitOf(request({ seven_day_fable: 0.0012, five_hour: 0.0042, seven_day: 0.00061 }), position);
    assert.deepEqual(
      unit,
      { estimated: false, kind: 'shares', reason: null, shares: [{ key: 'five_hour', share: 0.0042 }, { key: 'seven_day', share: 0.00061 }, { key: 'seven_day_fable', share: 0.0012 }], usd: 0.0048 },
      `${position}: the shares read 5-hour, 7-day, then the model week, whatever order the route answered them in`,
    );
    assert.deepEqual(d.costCellOf(unit), { main: '0.12% 7-day Fable', sub: '0.42% 5h', title: '0.42% of 5-hour, 0.061% of 7-day, 0.12% of 7-day Fable, $0.0048 equivalent' }, position);
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

test('an overage request, a fast answer, an API key request and a usage-based request read dollars over their funding word under every position but Dollar equivalent', () => {
  for (const [a, funding, word] of [
    [attempt('oauth', true, 'standard'), 'overage', 'overage'],
    [attempt('oauth', false, 'fast'), 'fast', 'fast mode'],
    [attempt('api_key', null, 'standard'), 'api_key', 'API key'],
    // The class reads ahead of the speed and the overage flag, so a seat's fast answer is usage-based too.
    [{ ...attempt('oauth', null, 'fast'), usageBased: true }, 'usage_based', 'usage-based'],
  ]) {
    for (const position of POSITIONS.filter((p) => p !== 'equivalent')) {
      const unit = d.costUnitOf(request(null, [PLAN, a], 1.25), position);
      assert.deepEqual(unit, { estimated: false, funding, kind: 'dollars', usd: 1.25 }, `${word} under ${position}: the serving attempt's funding, the last of the trail`);
      assert.deepEqual(d.costCellOf(unit), { main: '$1.25', sub: word, title: `Billed in dollars: ${word}` }, `${word} under ${position}`);
    }
    assert.equal(d.FUNDING_WORD[funding], word);
  }
});

test('under Dollar equivalent an overage request, a fast answer and an API key request read a dash titled no subscription equivalent', () => {
  assert.equal(d.NO_EQUIVALENT_TITLE, 'Billed in dollars, no subscription equivalent');
  for (const a of [attempt('oauth', true, 'standard'), attempt('oauth', false, 'fast'), attempt('api_key', null, 'standard')]) {
    const unit = d.costUnitOf(request(null, [PLAN, a], 1.25), 'equivalent');
    const what = JSON.stringify(a);
    assert.deepEqual(unit, { estimated: false, kind: 'dash', title: d.NO_EQUIVALENT_TITLE, usd: 1.25 }, what);
    assert.deepEqual(d.costCellOf(unit), { main: '-', sub: '', title: d.NO_EQUIVALENT_TITLE }, `${what}: a dash shows no figure`);
  }
});

test('under All in dollars and Dollar equivalent a plan request reads its dollar equivalent over equivalent, with its Pro-week figure in full and the dollars on the title', () => {
  const r = request({ five_hour: 0.0042, seven_day: 0.00061 }, [PLAN], 0.0048, { costEquivalent: 0.002444 });
  for (const position of EQUIVALENT_DOLLAR_POSITIONS) {
    const unit = d.costUnitOf(r, position);
    assert.deepEqual(unit, { equivalents: [{ key: 'seven_day', share: 0.002444 }], estimated: false, kind: 'equivalent_dollars', reason: null, usd: 0.0048 }, `${position}: the figure reads as the 7-day equivalent, and the own-window shares are not read`);
    assert.deepEqual(d.costCellOf(unit), { main: '$0.0048', sub: 'equivalent', title: '0.244% of 20x 7-day, $0.0048 equivalent' }, position);
  }
});

test('under All in dollars and Dollar equivalent a plan request the route priced no equivalent for reads its dollar equivalent alone on the title', () => {
  for (const position of EQUIVALENT_DOLLAR_POSITIONS) {
    const unit = d.costUnitOf(request({ five_hour: 0.0042 }, [PLAN], 0.0048, { costEquivalent: null }), position);
    assert.deepEqual(unit, { equivalents: [], estimated: false, kind: 'equivalent_dollars', reason: null, usd: 0.0048 }, position);
    assert.deepEqual(d.costCellOf(unit), { main: '$0.0048', sub: 'equivalent', title: '$0.0048 equivalent' }, position);
  }
});

test('under Billed dollars a plan request reads a dash titled nothing billed', () => {
  assert.equal(d.NOTHING_BILLED_TITLE, 'Nothing billed: a subscription request');
  const unit = d.costUnitOf(request({ five_hour: 0.0042, seven_day: 0.00061 }, [PLAN], 0.0048, { costEquivalent: 0.014667 }), 'usd');
  assert.deepEqual(unit, { estimated: false, kind: 'dash', title: d.NOTHING_BILLED_TITLE, usd: 0.0048 });
  assert.deepEqual(d.costCellOf(unit), { main: '-', sub: '', title: d.NOTHING_BILLED_TITLE }, 'a dash shows no figure');
});

test('the Cost header names what a subscription and a billed request read under each position', () => {
  assert.deepEqual(Object.fromEntries(POSITIONS.map((position) => [position, d.costHeaderTitle(position)])), {
    all: 'Dollar equivalent for a request a subscription window paid, dollars for an API key, fast mode, overage or usage-based request',
    equivalent: 'Dollar equivalent for a request a subscription window paid, a dash for an API key, fast mode, overage or usage-based request',
    native: '20x window equivalents for a request a subscription window paid, dollars for an API key, fast mode, overage or usage-based request',
    share: 'Window shares for a request a subscription window paid, dollars for an API key, fast mode, overage or usage-based request',
    usd: 'A dash for a request a subscription window paid, dollars for an API key, fast mode, overage or usage-based request',
  });
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

test('a_free_calls_cost_cell_reads_zero_with_the_free_title', () => {
  assert.equal(d.FREE_SHARE_TITLE, 'Free on the plan: a client classifier call takes no window share');
  const free = request({ five_hour: 0, seven_day: 0 }, [PLAN], 0.0012, { costEquivalent: 0, shareReason: 'client_classifier' });
  const zero = [{ key: 'five_hour', share: 0 }, { key: 'seven_day', share: 0 }];
  for (const position of SHARE_POSITIONS) {
    const unit = d.costUnitOf(free, position);
    assert.deepEqual(unit, { estimated: false, kind: 'shares', reason: 'client_classifier', shares: zero, usd: 0.0012 }, position);
    assert.deepEqual(d.costCellOf(unit), { main: '0% 7d', sub: '0% 5h', title: `${d.FREE_SHARE_TITLE}. 0% of 5-hour, 0% of 7-day, $0.0012 equivalent` }, `${position}: the zero shares read as figures, the title says why ahead of the estimate`);
  }
  const figure = [{ key: 'seven_day', share: 0 }];
  const native = d.costUnitOf(free, 'native');
  assert.deepEqual(native, { equivalents: figure, estimated: false, kind: 'equivalent', reason: 'client_classifier', usd: 0.0012 });
  assert.deepEqual(d.costCellOf(native), { main: '0% 20x 7d', sub: '', title: `${d.FREE_SHARE_TITLE}. 0%, $0.0012 equivalent` }, 'native: the zero figure reads as a figure, the title says why ahead of the estimate');
  for (const position of EQUIVALENT_DOLLAR_POSITIONS) {
    const unit = d.costUnitOf(free, position);
    assert.deepEqual(unit, { equivalents: figure, estimated: false, kind: 'equivalent_dollars', reason: 'client_classifier', usd: 0.0012 }, position);
    assert.deepEqual(d.costCellOf(unit), { main: '$0.0012', sub: 'equivalent', title: `${d.FREE_SHARE_TITLE}. 0%, $0.0012 equivalent` }, `${position}: the dollar equivalent reads over equivalent, the title says why its figure reads zero ahead of the estimate`);
  }
  const billed = d.costUnitOf(free, 'usd');
  assert.deepEqual(billed, { estimated: false, kind: 'dash', title: d.NOTHING_BILLED_TITLE, usd: 0.0012 });
  assert.deepEqual(d.costCellOf(billed), { main: '-', sub: '', title: d.NOTHING_BILLED_TITLE }, 'usd: a plan-funded call bills nothing, so it reads the dash every subscription request reads');
});

test('a plan request priced on the 5-hour window alone reads it once, and one priced on its week alone reads the week', () => {
  for (const position of SHARE_POSITIONS) {
    const fiveHour = d.costUnitOf(request({ five_hour: 0.0042 }), position);
    assert.deepEqual(d.costCellOf(fiveHour), { main: '0.42% 5h', sub: '', title: '0.42% of 5-hour, $0.0048 equivalent' }, `${position}: the 5-hour share is the main line, so the second line stays empty`);
    const week = d.costUnitOf(request({ seven_day_fable: 0.0012 }), position);
    assert.deepEqual(d.costCellOf(week), { main: '0.12% 7-day Fable', sub: '', title: '0.12% of 7-day Fable, $0.0048 equivalent' }, position);
  }
});

test('a request with no attempt reads its zero dollars with no funding word and no title under every position', () => {
  for (const position of POSITIONS) {
    const unit = d.costUnitOf(request(null, [], 0), position);
    assert.deepEqual(unit, { estimated: false, kind: 'unserved', usd: 0 }, `${position}: no account served it, so no funding is named`);
    assert.deepEqual(d.costCellOf(unit), { main: '$0.00', sub: '', title: '' }, position);
  }
});

test('under native a plan request reads its Pro-week figure as the 7-day equivalent in its short word, the figure in full and the dollars on the title', () => {
  const r = request({ five_hour: 0.0042, seven_day: 0.00061, seven_day_fable: 0.0012 }, [PLAN], 0.0048, { costEquivalent: 0.002444 });
  const unit = d.costUnitOf(r, 'native');
  assert.deepEqual(unit, { equivalents: [{ key: 'seven_day', share: 0.002444 }], estimated: false, kind: 'equivalent', reason: null, usd: 0.0048 }, 'the own-window shares are not read');
  assert.deepEqual(d.costCellOf(unit), { main: '0.244% 20x 7d', sub: '', title: '0.244% of 20x 7-day, $0.0048 equivalent' });
});

test('under native a plan request the route priced no equivalent for reads dollars over equivalent with no estimated mark', () => {
  const unit = d.costUnitOf(request({ five_hour: 0.0042, seven_day: 0.00061 }, [PLAN], 0.0048, { costEquivalent: null }), 'native');
  assert.deepEqual(unit, { estimated: false, kind: 'unpriced', usd: 0.0048 });
  assert.deepEqual(d.costCellOf(unit), { main: '$0.0048', sub: 'equivalent', title: d.NO_SHARE_TITLE });
});

/** A settings answer's two revision lists, enough for the title, written here so no shipped date is pinned. */
const dated = (effective) => ({ effective, effectiveDate: `${effective}T00:00:00.000Z` });
const SETTINGS = { apiPrices: { revisions: [dated('2026-10-01'), dated('2026-10-08')] }, subscriptionCosts: { revisions: [dated('2026-10-05')] } };

test('the revisions in force at a request name the latest price list and cost model at or before its instant, the first before any, and no revision for an empty list', () => {
  assert.equal(d.revisionsTitle(SETTINGS, '2026-10-09T12:00:00Z'), 'Priced at the API prices of Oct 08, 2026 and the cost model of Oct 05, 2026');
  assert.equal(d.revisionsTitle(SETTINGS, '2026-10-06T12:00:00Z'), 'Priced at the API prices of Oct 01, 2026 and the cost model of Oct 05, 2026');
  assert.equal(d.revisionsTitle(SETTINGS, '2026-09-01T12:00:00Z'), 'Priced at the API prices of Oct 01, 2026 and the cost model of Oct 05, 2026', 'an instant before every entry reads the first');
  assert.equal(d.revisionsTitle({ ...SETTINGS, subscriptionCosts: { revisions: [] } }, '2026-10-09T12:00:00Z'), 'Priced at the API prices of Oct 08, 2026 and the cost model of no revision');
  assert.equal(d.revisionsTitle(null, '2026-10-09T12:00:00Z'), null, 'no settings at hand, no sentence');
});

test("the Dashboard's glance cell reads an equivalent unit's lead window alone in its 20x short form, with the full reading and the revisions on the title", () => {
  const r = request(null, [PLAN], 0.0048, { costEquivalent: 0.002444 });
  const revisions = 'Priced at the API prices of Oct 08, 2026 and the cost model of Oct 05, 2026';
  const full = '0.244% of 20x 7-day, $0.0048 equivalent';
  assert.deepEqual(d.glanceCellOf(d.costUnitOf(r, 'native'), revisions), { main: '0.244% 20x 7d', sub: '', title: `${full}. ${revisions}` });
  assert.deepEqual(d.glanceCellOf(d.costUnitOf(r, 'native'), null), { main: '0.244% 20x 7d', sub: '', title: full }, 'no settings at hand, the reading alone');
});

test("the Dashboard's glance cell of a client classifier call leads its title with the free sentence, ahead of the reading and the revisions", () => {
  const revisions = 'Priced at the API prices of Oct 08, 2026 and the cost model of Oct 05, 2026';
  const free = request({ five_hour: 0, seven_day: 0 }, [PLAN], 0.0012, { costEquivalent: 0, shareReason: 'client_classifier' });
  const unit = d.costUnitOf(free, 'native');
  assert.equal(unit.reason, 'client_classifier');
  assert.deepEqual(d.glanceCellOf(unit, revisions), { main: '0% 20x 7d', sub: '', title: `${d.FREE_SHARE_TITLE}. 0%, $0.0012 equivalent. ${revisions}` });
  assert.deepEqual(d.glanceCellOf(unit, null), { main: '0% 20x 7d', sub: '', title: `${d.FREE_SHARE_TITLE}. 0%, $0.0012 equivalent` }, 'no settings at hand, the sentence and the reading');
  const plain = d.costUnitOf({ ...free, shareReason: null }, 'native');
  assert.deepEqual(d.glanceCellOf(plain, revisions), { main: '0% 20x 7d', sub: '', title: `0%, $0.0012 equivalent. ${revisions}` }, 'without the reason the title reads the zero alone');
});

test("the Dashboard's glance cell reads every other unit as the Requests cell does", () => {
  const revisions = 'Priced at the API prices of Oct 08, 2026 and the cost model of Oct 05, 2026';
  for (const unit of [
    d.costUnitOf(request(null, [attempt('api_key', null, 'standard')], 1.25), 'native'),
    d.costUnitOf(request({ five_hour: 0.0042, seven_day: 0.00061 }), 'share'),
    d.costUnitOf(request(null, [PLAN], 0.0048, { costEquivalent: null }), 'native'),
    d.costUnitOf(request(null, [], 0), 'native'),
  ]) {
    assert.deepEqual(d.glanceCellOf(unit, revisions), d.costCellOf(unit), unit.kind);
  }
});

test("the Dashboard's glance cell reads a plan and a billed request under each dollar position as the Requests cell does, a dash with no mark", () => {
  const revisions = 'Priced at the API prices of Oct 08, 2026 and the cost model of Oct 05, 2026';
  const plan = request({ five_hour: 0.0042, seven_day: 0.00061 }, [PLAN], 0.0048, { costEquivalent: 0.002444 });
  const billed = request(null, [attempt('oauth', true, 'standard')], 1.25);
  const equivalent = { main: '$0.0048', sub: 'equivalent', title: '0.244% of 20x 7-day, $0.0048 equivalent' };
  for (const [r, position, cell] of [
    [plan, 'all', equivalent],
    [plan, 'equivalent', equivalent],
    [plan, 'usd', { main: '-', sub: '', title: d.NOTHING_BILLED_TITLE }],
    [billed, 'all', { main: '$1.25', sub: 'overage', title: 'Billed in dollars: overage' }],
    [billed, 'equivalent', { main: '-', sub: '', title: d.NO_EQUIVALENT_TITLE }],
    [billed, 'usd', { main: '$1.25', sub: 'overage', title: 'Billed in dollars: overage' }],
  ]) {
    const unit = d.costUnitOf(r, position);
    const what = `${r === plan ? 'plan' : 'billed'} under ${position}`;
    assert.deepEqual(d.glanceCellOf(unit, revisions), cell, what);
    assert.deepEqual(d.glanceCellOf(unit, revisions), d.costCellOf(unit), `${what}: the Requests cell's reading`);
  }
});

test("a held row reads waited with each wait's reason, and a row without waits reads waited alone", () => {
  const hold = (waits, more = {}) => ({ cohort: null, heldMs: waits.reduce((n, w) => n + w.heldMs, 0), waitedOn: ['acct-a'], waits, ...more });
  assert.deepEqual(d.heldWords(hold([{ heldMs: 42_000, reason: 'slow_down' }, { heldMs: 3_000, reason: 'cap' }])), { badge: 'waited 45 s', title: 'waited for the slow-down 42 s, for a slot 3 s on acct-a' }, 'the waits read in their order');
  assert.deepEqual(d.heldWords(hold([{ heldMs: 39_000, reason: 'reset' }], { cohort: 'acct-b', waitedOn: ['acct-c'] })), { badge: 'waited 39 s', title: 'waited for the reset 39 s on acct-b' }, "a cohort's account is where the hold was placed");
  assert.deepEqual(d.heldWords(hold([{ heldMs: 120_000, reason: 'slow_down' }])), { badge: 'waited 2 min', title: 'waited for the slow-down 2 min on acct-a' }, 'a minute or more reads the span');
  assert.deepEqual(d.heldWords(hold([{ heldMs: 2_000, reason: 'rewrite' }])), { badge: 'waited 2 s', title: 'waited for the rewrite in flight 2 s on acct-a' }, "a moved pin's wait for the head of the request that carried the move");
  assert.deepEqual(d.heldWords(hold([{ heldMs: 1_250, reason: 'quota' }])), { badge: 'waited 1 s', title: 'waited for quota 1 s on acct-a' }, 'a reason the console has no words for reads as the router sent it');
  assert.deepEqual(d.heldWords({ ...hold([]), heldMs: 1_250 }), { badge: 'waited 1 s', title: 'waited 1 s on acct-a' }, 'a row from before the waits names no reason');
  assert.deepEqual(d.heldWords(hold([{ heldMs: 300, reason: 'cap' }, { heldMs: 100, reason: 'cap' }])), { badge: 'waited 1 s', title: 'waited for a slot 1 s, for a slot 1 s on acct-a' }, 'a wait that happened reads at least a second');
  assert.deepEqual(d.heldWords({ ...hold([]), heldMs: 400 }), { badge: 'waited 1 s', title: 'waited 1 s on acct-a' }, 'a row from before the waits reads at least a second too');
});

test('a held row whose waits span two accounts names each wait with its own, and waits on one account keep the one-account form', () => {
  const waits = [{ heldMs: 42_000, reason: 'slow_down' }, { heldMs: 3_000, reason: 'cap' }];
  const hold = (more) => ({ cohort: null, heldMs: 45_000, waitedOn: ['studio', 'primary'], waits, ...more });
  assert.deepEqual(d.heldWords(hold({})), { badge: 'waited 45 s', title: 'waited for the slow-down 42 s on studio, for a slot 3 s on primary' }, 'each account read from the waits at its position');
  assert.deepEqual(d.heldWords(hold({ cohort: 'primary' })), { badge: 'waited 45 s', title: 'waited for the slow-down 42 s on studio, for a slot 3 s on primary' }, 'under a cohort each wait still names its own account');
  assert.deepEqual(d.heldWords(hold({ waitedOn: ['studio', 'studio'] })), { badge: 'waited 45 s', title: 'waited for the slow-down 42 s, for a slot 3 s on studio' }, 'two waits on one account');
  assert.deepEqual(d.heldWords(hold({ waitedOn: [] })), { badge: 'waited 45 s', title: 'waited for the slow-down 42 s, for a slot 3 s on an account' }, 'a row that names no account');
});

test('the_codec_reads_and_writes_the_guardrails_list', () => {
  const read = d.REQUESTS_CODEC.fromParams(new URLSearchParams('guardrails=large-auxiliary,-'));
  assert.deepEqual(read.guardrails, ['large-auxiliary', '-'], 'an id and none, as the route reads the comma list');
  const written = d.REQUESTS_CODEC.toParams(read);
  assert.deepEqual([...written.keys()], ['guardrails'], 'every other filter stands at its default');
  assert.equal(written.get('guardrails'), 'large-auxiliary,-');
  assert.deepEqual(d.REQUESTS_CODEC.fromParams(new URLSearchParams('')).guardrails, [], 'a bare address filters on no guardrail');
});

test("the Refused hover names the refusals the request figures leave out, and the router's own refusals that count as requests", () => {
  assert.equal(
    d.REFUSED_TITLE,
    "Requests refused for the client's or a policy's cause before any account was asked. The router's own refusals, account_canceled, accounts_unavailable, body_budget_exhausted, key_body_reads, ledger_behind and router_unavailable, count as requests instead.",
  );
});
