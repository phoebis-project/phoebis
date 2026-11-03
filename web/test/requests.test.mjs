import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

const attempt = (accountKind, overageInUse, speed) => ({ accountKind, overageInUse, response: { speed } });
const PLAN = attempt('oauth', false, 'standard');
const request = (costShares, attempts = [PLAN], costUsd = 0.0048, more = {}) => ({ attempts, costEquivalent: null, costShares, costUsd, shareReason: null, tier: 'default_claude_max_20x', ...more });
/** The four positions the select offers. */
const POSITIONS = ['all', 'equivalent', 'native', 'usd'];
/** The two positions under which a plan request reads its dollar equivalent. */
const EQUIVALENT_DOLLAR_POSITIONS = ['all', 'equivalent'];
/** The chosen week and its scale over the Pro-week figure, written here so no shipped capacity is pinned. */
const WEEK = 'ultra';
const SCALE = 0.1;
const cellOf = (unit) => d.costCellOf(unit, WEEK, SCALE);

test('the hint of an Ultra row whose shares carry no 5-hour window names the 7-day window and the Fable week alone', () => {
  assert.equal(d.shareHint('Ultra', { seven_day: 0.0011, seven_day_fable: 0.0022 }, 'default_claude_max_20x', false), 'Ultra: 0.11% of the 7-day window, 0.22% of the Fable week');
});

test('the hint of a Pro row reads no Fable line, though its shares carry the week', () => {
  assert.equal(d.shareHint('Pro', { five_hour: 0.133, seven_day: 0.0111, seven_day_fable: 0.0022 }, 'default_claude_ai', false), 'Pro: 13.3% of the 5-hour window, 1.11% of the 7-day window');
});

test('a Max row that completed inside the band reads the band after its 5-hour share, one line per window, and none outside the band or with the band unread', () => {
  // The bare spelling of the Max tier, as a profile can report it, which the Fable week applies to as well.
  const shares = { five_hour: 0.0267, seven_day: 0.0022, seven_day_fable: 0.0044 };
  assert.deepEqual(d.shareHintLines('Max', shares, 'claude_max_5x', true), ['Max', '2.67% of the 5-hour window (in the band)', '0.22% of the 7-day window', '0.44% of the Fable week']);
  assert.equal(d.shareHint('Max', shares, 'claude_max_5x', true), 'Max: 2.67% of the 5-hour window (in the band), 0.22% of the 7-day window, 0.44% of the Fable week');
  assert.equal(d.shareHint('Max', shares, 'claude_max_5x', false), 'Max: 2.67% of the 5-hour window, 0.22% of the 7-day window, 0.44% of the Fable week', 'outside the band');
  assert.equal(d.shareHint('Max', shares, 'claude_max_5x', null), 'Max: 2.67% of the 5-hour window, 0.22% of the 7-day window, 0.44% of the Fable week', 'a surface that cannot read the band names none');
});

test('a request that starts before the band and completes inside it reads the band at its completion', () => {
  // A band of 13:00 to 19:00 UTC on weekdays, and a Wednesday request from 12:59 to 13:01.
  const costs = { peak: { fromHour: 13, toHour: 19, weekdays: true, zone: 'UTC' } };
  const shares = { five_hour: 0.0267, seven_day: 0.0022 };
  const row = { accountId: 'nightly', at: '2026-09-16T12:59:00Z', costShares: shares, durationMs: 120_000, tier: 'default_claude_max_5x' };
  assert.deepEqual(d.requestHintArgs(row, [{ id: 'nightly', subscription: null }], costs), ['Max', shares, 'default_claude_max_5x', true]);
});

test('a row whose shares are null, or name no window the route priced, reads no hint', () => {
  assert.equal(d.shareHint('Ultra', null, 'default_claude_max_20x', false), null);
  assert.equal(d.shareHint('Ultra', { five_hour: null, seven_day: null }, 'default_claude_max_20x', false), null, 'a window the route could not price names nothing');
  assert.deepEqual(d.shareHintLines('Ultra', null, 'default_claude_max_20x', false), []);
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
      const unit = d.costUnitOf(request(null, [PLAN, a], 1.25), position, null);
      assert.deepEqual(unit, { funding, kind: 'dollars', usd: 1.25 }, `${word} under ${position}: the serving attempt's funding, the last of the trail`);
      assert.deepEqual(cellOf(unit), { main: '$1.25', sub: word, title: `Billed in dollars: ${word}` }, `${word} under ${position}`);
    }
    assert.equal(d.FUNDING_WORD[funding], word);
  }
});

test('under Dollar equivalent an overage request, a fast answer and an API key request read a dash titled no subscription equivalent', () => {
  assert.equal(d.NO_EQUIVALENT_TITLE, 'Billed in dollars, no subscription equivalent');
  for (const a of [attempt('oauth', true, 'standard'), attempt('oauth', false, 'fast'), attempt('api_key', null, 'standard')]) {
    const unit = d.costUnitOf(request(null, [PLAN, a], 1.25), 'equivalent', null);
    const what = JSON.stringify(a);
    assert.deepEqual(unit, { kind: 'dash', title: d.NO_EQUIVALENT_TITLE, usd: 1.25 }, what);
    assert.deepEqual(cellOf(unit), { main: '-', sub: '', title: d.NO_EQUIVALENT_TITLE }, `${what}: a dash shows no figure`);
  }
});

test('under All in dollars and Dollar equivalent a plan request reads its dollar equivalent over equivalent, with its figure in the chosen week and the dollars on the title', () => {
  const r = request({ five_hour: 0.0042, seven_day: 0.00061 }, [PLAN], 0.0048, { costEquivalent: 0.002444 });
  for (const position of EQUIVALENT_DOLLAR_POSITIONS) {
    const unit = d.costUnitOf(r, position, null);
    assert.deepEqual(unit, { figure: 0.002444, hint: null, kind: 'equivalent_dollars', reason: null, usd: 0.0048 }, `${position}: the one figure, and the own-window shares are not read`);
    assert.deepEqual(cellOf(unit), { main: '$0.0048', sub: 'equivalent', title: '0.0244% of an Ultra week, $0.0048 equivalent' }, position);
  }
});

test('under All in dollars and Dollar equivalent a plan request the route priced nothing for reads its dollar equivalent with the no-share title', () => {
  for (const position of EQUIVALENT_DOLLAR_POSITIONS) {
    const unit = d.costUnitOf(request({ five_hour: 0.0042 }, [PLAN], 0.0048, { costEquivalent: null }), position, null);
    assert.deepEqual(unit, { kind: 'unpriced', usd: 0.0048 }, position);
    assert.deepEqual(cellOf(unit), { main: '$0.0048', sub: 'equivalent', title: d.NO_SHARE_TITLE }, position);
  }
});

test('under Billed dollars a plan request reads a dash titled nothing billed', () => {
  assert.equal(d.NOTHING_BILLED_TITLE, 'Nothing billed: a subscription request');
  const unit = d.costUnitOf(request({ five_hour: 0.0042, seven_day: 0.00061 }, [PLAN], 0.0048, { costEquivalent: 0.014667 }), 'usd', null);
  assert.deepEqual(unit, { kind: 'dash', title: d.NOTHING_BILLED_TITLE, usd: 0.0048 });
  assert.deepEqual(cellOf(unit), { main: '-', sub: '', title: d.NOTHING_BILLED_TITLE }, 'a dash shows no figure');
});

test('a_free_calls_cost_cell_reads_zero_with_the_free_title', () => {
  assert.equal(d.FREE_SHARE_TITLE, 'Free on the plan: a client classifier call takes no window share');
  const free = request({ five_hour: 0, seven_day: 0 }, [PLAN], 0.0012, { costEquivalent: 0, shareReason: 'client_classifier' });
  const native = d.costUnitOf(free, 'native', null);
  assert.deepEqual(native, { figure: 0, hint: null, kind: 'equivalent', reason: 'client_classifier', usd: 0.0012 });
  assert.deepEqual(cellOf(native), { main: '0% Ultra week', sub: '', title: `${d.FREE_SHARE_TITLE}. $0.0012 equivalent` }, 'native: the zero figure reads as a figure, the title says why ahead of the dollars');
  for (const position of EQUIVALENT_DOLLAR_POSITIONS) {
    const unit = d.costUnitOf(free, position, null);
    assert.deepEqual(unit, { figure: 0, hint: null, kind: 'equivalent_dollars', reason: 'client_classifier', usd: 0.0012 }, position);
    assert.deepEqual(cellOf(unit), { main: '$0.0012', sub: 'equivalent', title: `${d.FREE_SHARE_TITLE}. 0%, $0.0012 equivalent` }, `${position}: the dollar equivalent reads over equivalent, the title says why its figure reads zero`);
  }
  const billed = d.costUnitOf(free, 'usd', null);
  assert.deepEqual(billed, { kind: 'dash', title: d.NOTHING_BILLED_TITLE, usd: 0.0012 });
  assert.deepEqual(cellOf(billed), { main: '-', sub: '', title: d.NOTHING_BILLED_TITLE }, 'usd: a plan-funded call bills nothing, so it reads the dash every subscription request reads');
});

test('a request with no attempt reads its zero dollars with no funding word and no title under every position', () => {
  for (const position of POSITIONS) {
    const unit = d.costUnitOf(request(null, [], 0), position, null);
    assert.deepEqual(unit, { kind: 'unserved', usd: 0 }, `${position}: no account served it, so no funding is named`);
    assert.deepEqual(cellOf(unit), { main: '$0.00', sub: '', title: '' }, position);
  }
});

test('under native a plan request reads its figure in the chosen week on one line, its title the hint, then the dollar equivalent', () => {
  const r = request({ five_hour: 0.0042, seven_day: 0.00061, seven_day_fable: 0.0012 }, [PLAN], 0.0048, { costEquivalent: 0.002444 });
  const hint = 'Ultra: 0.42% of the 5-hour window, 0.061% of the 7-day window, 0.12% of the Fable week';
  const unit = d.costUnitOf(r, 'native', hint);
  assert.deepEqual(unit, { figure: 0.002444, hint, kind: 'equivalent', reason: null, usd: 0.0048 }, 'the own-window shares reach the cell through the hint alone');
  assert.deepEqual(cellOf(unit), { main: '0.0244% Ultra week', sub: '', title: `${hint}. $0.0048 equivalent` });
  assert.deepEqual(cellOf(d.costUnitOf(r, 'native', null)), { main: '0.0244% Ultra week', sub: '', title: '$0.0048 equivalent' }, 'a row with no hint reads the dollars alone on its title');
});

test("a week figure waits on the cost model's scale: a native cell reads none, and a dollar cell's title leaves the figure out", () => {
  const r = request(null, [PLAN], 0.0048, { costEquivalent: 0.002444 });
  assert.equal(d.costCellOf(d.costUnitOf(r, 'native', null), WEEK, null), null, 'the surface draws its skeleton, never the Pro-week figure under the chosen week');
  assert.deepEqual(d.costCellOf(d.costUnitOf(r, 'all', null), WEEK, null), { main: '$0.0048', sub: 'equivalent', title: '$0.0048 equivalent' });
  assert.deepEqual(d.costCellOf(d.costUnitOf(r, 'native', null), 'pro', d.weekScale(null, 'pro')), { main: '0.244% Pro week', sub: '', title: '$0.0048 equivalent' }, 'the Pro week reads the figure as the route answers it');
});

test('under native a plan request the route priced no equivalent for reads dollars over equivalent with the no-share title', () => {
  const unit = d.costUnitOf(request({ five_hour: 0.0042, seven_day: 0.00061 }, [PLAN], 0.0048, { costEquivalent: null }), 'native', 'Ultra: 0.42% of the 5-hour window');
  assert.deepEqual(unit, { kind: 'unpriced', usd: 0.0048 });
  assert.deepEqual(cellOf(unit), { main: '$0.0048', sub: 'equivalent', title: d.NO_SHARE_TITLE });
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

test("the Dashboard's glance cell reads an equivalent unit's figure in the chosen week, with the hint, the dollars and the revisions on the title", () => {
  const r = request(null, [PLAN], 0.0048, { costEquivalent: 0.002444 });
  const revisions = 'Priced at the API prices of Oct 08, 2026 and the cost model of Oct 05, 2026';
  const hint = 'Ultra: 0.061% of the 7-day window';
  assert.deepEqual(d.glanceCellOf(d.costUnitOf(r, 'native', hint), revisions, WEEK, SCALE), { main: '0.0244% Ultra week', sub: '', title: `${hint}. $0.0048 equivalent. ${revisions}` });
  assert.deepEqual(d.glanceCellOf(d.costUnitOf(r, 'native', null), null, WEEK, SCALE), { main: '0.0244% Ultra week', sub: '', title: '$0.0048 equivalent' }, 'no settings at hand, the dollars alone');
  assert.equal(d.glanceCellOf(d.costUnitOf(r, 'native', hint), revisions, WEEK, null), null, 'no scale, no figure');
});

test("the Dashboard's glance cell of a client classifier call leads its title with the free sentence, ahead of the dollars and the revisions", () => {
  const revisions = 'Priced at the API prices of Oct 08, 2026 and the cost model of Oct 05, 2026';
  const free = request({ five_hour: 0, seven_day: 0 }, [PLAN], 0.0012, { costEquivalent: 0, shareReason: 'client_classifier' });
  const unit = d.costUnitOf(free, 'native', null);
  assert.equal(unit.reason, 'client_classifier');
  assert.deepEqual(d.glanceCellOf(unit, revisions, WEEK, SCALE), { main: '0% Ultra week', sub: '', title: `${d.FREE_SHARE_TITLE}. $0.0012 equivalent. ${revisions}` });
  assert.deepEqual(d.glanceCellOf(unit, null, WEEK, SCALE), { main: '0% Ultra week', sub: '', title: `${d.FREE_SHARE_TITLE}. $0.0012 equivalent` }, 'no settings at hand, the sentence and the dollars');
  const plain = d.costUnitOf({ ...free, shareReason: null }, 'native', null);
  assert.deepEqual(d.glanceCellOf(plain, revisions, WEEK, SCALE), { main: '0% Ultra week', sub: '', title: `$0.0012 equivalent. ${revisions}` }, 'without the reason the title reads the dollars alone');
});

test("the Dashboard's glance cell reads every other unit as the Requests cell does", () => {
  const revisions = 'Priced at the API prices of Oct 08, 2026 and the cost model of Oct 05, 2026';
  for (const unit of [d.costUnitOf(request(null, [attempt('api_key', null, 'standard')], 1.25), 'native', null), d.costUnitOf(request(null, [PLAN], 0.0048, { costEquivalent: null }), 'native', null), d.costUnitOf(request(null, [], 0), 'native', null)]) {
    assert.deepEqual(d.glanceCellOf(unit, revisions, WEEK, SCALE), cellOf(unit), unit.kind);
  }
});

test("the Dashboard's glance cell reads a plan and a billed request under each dollar position as the Requests cell does", () => {
  const revisions = 'Priced at the API prices of Oct 08, 2026 and the cost model of Oct 05, 2026';
  const plan = request({ five_hour: 0.0042, seven_day: 0.00061 }, [PLAN], 0.0048, { costEquivalent: 0.002444 });
  const billed = request(null, [attempt('oauth', true, 'standard')], 1.25);
  const equivalent = { main: '$0.0048', sub: 'equivalent', title: '0.0244% of an Ultra week, $0.0048 equivalent' };
  for (const [r, position, cell] of [
    [plan, 'all', equivalent],
    [plan, 'equivalent', equivalent],
    [plan, 'usd', { main: '-', sub: '', title: d.NOTHING_BILLED_TITLE }],
    [billed, 'all', { main: '$1.25', sub: 'overage', title: 'Billed in dollars: overage' }],
    [billed, 'equivalent', { main: '-', sub: '', title: d.NO_EQUIVALENT_TITLE }],
    [billed, 'usd', { main: '$1.25', sub: 'overage', title: 'Billed in dollars: overage' }],
  ]) {
    const unit = d.costUnitOf(r, position, null);
    const what = `${r === plan ? 'plan' : 'billed'} under ${position}`;
    assert.deepEqual(d.glanceCellOf(unit, revisions, WEEK, SCALE), cell, what);
    assert.deepEqual(d.glanceCellOf(unit, revisions, WEEK, SCALE), cellOf(unit), `${what}: the Requests cell's reading`);
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

test('the request order puts the newer request first and the higher id first at one instant', () => {
  const rows = [
    { at: '2026-09-16T10:00:00.000Z', id: 'req_a' },
    { at: '2026-09-16T10:00:00.000Z', id: 'req_b' },
    { at: '2026-09-16T11:00:00.000Z', id: 'req_c' },
  ];
  assert.deepEqual([...rows].sort(d.compareRequests).map((r) => r.id), ['req_c', 'req_b', 'req_a']);
});
