import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

const at = (iso) => Date.parse(iso);
const BASE = {
  count: 3,
  filters: { accountIds: [], clientPattern: '', includeProbes: false, keyIds: [], models: [], poolIds: [], sessionId: null, userAgentPattern: '' },
  reason: 'messages_changed',
  window: { exact: false, fromMs: at('2026-09-16T11:20:00Z'), nowMs: at('2026-09-16T14:30:00Z'), toMs: at('2026-09-16T14:10:00Z') },
};
const SESSION = '0b6f3c2e-8d41-4a7f-9e25-3c1d7a90b4f6';

const link = (over = {}) => d.requestsLink({ ...BASE, ...over, filters: { ...BASE.filters, ...over.filters }, window: { ...BASE.window, ...over.window } });
/** An address read back as its path and its parameters, since it percent-encodes the instants, the slashes and the `@`. */
const target = (href) => {
  const url = new URL(href, 'http://console.test');
  return { path: url.pathname, ...Object.fromEntries(url.searchParams) };
};

test("a refusal count links its category's refused answers over the counted window alone, with no session", () => {
  const now = at('2026-09-16T14:30:00Z');
  // A window off the hour, so the hour rounding of the reasons panel would widen it on both sides.
  const input = { filters: { ...BASE.filters, sessionId: SESSION }, window: { exact: false, fromMs: at('2026-09-16T12:15:00Z'), nowMs: now, toMs: at('2026-09-16T12:45:00Z') } };
  const read = d.REQUESTS_CODEC.fromParams(new URL(d.refusalLink(input, 'cyber', 2, 'claude-sonnet-5-5').href, 'http://console.test').searchParams, now);
  // The case's own instants just outside each bound and inside, read against the range the linked list answers.
  const instants = ['2026-09-16T12:14:59Z', '2026-09-16T12:30:00Z', '2026-09-16T12:45:01Z'];
  const kept = instants.filter((t) => at(t) >= Date.parse(read.range.from) && at(t) <= Date.parse(read.range.to));
  assert.deepEqual(
    { cacheMissReasons: read.cacheMissReasons, kept, range: read.range, refusalCategories: read.refusalCategories, session: read.session, stopReasons: read.stopReasons },
    { cacheMissReasons: [], kept: ['2026-09-16T12:30:00Z'], range: { from: '2026-09-16T12:15:00.000Z', preset: 'custom', to: '2026-09-16T12:45:00.000Z' }, refusalCategories: ['cyber'], session: '', stopReasons: ['refusal'] },
  );
  assert.equal(target(d.refusalLink(input, '-', 1, 'claude-sonnet-5-5').href).refusal_categories, '-', 'a refusal with no category links the dash');
});

test("a refusal count links its own model in place of the page's model selection, with the client pattern and no session, and reads plain under a user agent pattern as the reasons panel does, since the route reads both patterns", () => {
  const input = { filters: { ...BASE.filters, clientPattern: 'no-such-client', models: ['claude-sonnet-4-5', 'claude-sonnet-5-5'], sessionId: SESSION }, window: BASE.window };
  const answer = d.refusalLink(input, 'cyber', 3, 'claude-sonnet-5-5');
  assert.notEqual(answer.href, null, 'the bar links');
  const read = target(answer.href);
  assert.equal(read.models, 'claude-sonnet-5-5', "the bar's model alone, the page's two models left out");
  assert.equal(read.client, 'no-such-client', 'the client pattern travels');
  assert.equal(read.agent, undefined);
  assert.equal(read.session, undefined, 'the route reads no session');
  assert.equal(answer.title, 'List these requests on the Requests page');
  const agent = d.refusalLink({ ...input, filters: { ...input.filters, userAgentPattern: 'claude-cli*' } }, 'cyber', 3, 'claude-sonnet-5-5');
  assert.deepEqual(agent, { href: null, title: link({ filters: { userAgentPattern: 'claude-cli*' } }).title }, "the reasons panel's plain count under the same pattern");
});

test('a count links to the Requests page under its reason, from the hour holding the panel start to the end capped at now', () => {
  const answer = link();
  assert.deepEqual(target(answer.href), { cache_miss_reasons: 'messages_changed', from: '2026-09-16T11:00:00.000Z', path: '/requests', to: '2026-09-16T14:30:00.000Z' });
  assert.doesNotMatch(answer.title, /probes/);
});

test('the link ends one millisecond before the hour ceiling of the panel end, so an end on the hour stays in its own hour', () => {
  assert.equal(target(link({ window: { nowMs: at('2026-09-16T15:30:00Z') } }).href).to, '2026-09-16T14:59:59.999Z');
  assert.equal(target(link({ window: { nowMs: at('2026-09-16T14:30:00Z'), toMs: at('2026-09-16T12:00:00.000Z') } }).href).to, '2026-09-16T11:59:59.999Z');
});

test("a session view's link carries its exact window", () => {
  const { from, to } = target(link({ window: { exact: true } }).href);
  assert.deepEqual([from, to], ['2026-09-16T11:20:00.000Z', '2026-09-16T14:10:00.000Z']);
});

test("the page's selections, its class pattern and a session id travel as the Requests page reads them, the session as its own filter", () => {
  const read = target(link({ filters: { accountIds: ['primary', 'secondary'], clientPattern: 'claude-code/*', keyIds: ['desktop'], models: ['m'], poolIds: ['p'], sessionId: SESSION } }).href);
  assert.deepEqual(read, {
    accounts: 'primary,secondary',
    cache_miss_reasons: 'messages_changed',
    client: 'claude-code/*',
    from: '2026-09-16T11:00:00.000Z',
    keys: 'desktop',
    models: 'm',
    path: '/requests',
    pools: 'p',
    session: SESSION,
    to: '2026-09-16T14:30:00.000Z',
  });
});

test("a session read under a key lifetime links with that lifetime alone, in place of the page's key selection", () => {
  assert.deepEqual(d.linkedKeyIds([], 'desktop@1000'), ['desktop@1000']);
  assert.deepEqual(d.linkedKeyIds(['laptop'], 'desktop@1000'), ['desktop@1000']);
  assert.deepEqual(d.linkedKeyIds(['laptop'], null), ['laptop']);
  const read = target(link({ filters: { keyIds: d.linkedKeyIds(['laptop'], 'desktop@1000'), sessionId: SESSION }, window: { exact: true } }).href);
  assert.equal(read.keys, 'desktop@1000');
  assert.equal(read.session, SESSION);
});

test("a session view's link under an agent lists that agent's rows alone, and a refusal count's link names no agent, which the refusal route does not read", () => {
  const input = { filters: { ...BASE.filters, agentId: 'c21e', sessionId: SESSION }, window: { ...BASE.window, exact: true } };
  assert.equal(target(link(input).href).agent_id, 'c21e', 'the reasons panel count');
  assert.equal(target(d.refusalLink(input, 'cyber', 3, 'claude-sonnet-5-5').href).agent_id, undefined, 'the refusal count');
});

test("a link reads back through the Requests page's address as its custom span, its reason and its filters", () => {
  const href = link({ filters: { accountIds: ['primary'], keyIds: ['desktop@1000'], sessionId: SESSION }, group: { by: 'request_class', key: 'conversation' } }).href;
  const read = d.REQUESTS_CODEC.fromParams(new URL(href, 'http://console.test').searchParams);
  assert.deepEqual(read.range, { from: '2026-09-16T11:00:00.000Z', preset: 'custom', to: '2026-09-16T14:30:00.000Z' });
  assert.deepEqual([read.cacheMissReasons, read.accounts, read.keys, read.requestClasses, read.session], [['messages_changed'], ['primary'], ['desktop@1000'], ['conversation'], SESSION]);
  assert.equal(d.REQUESTS_CODEC.toParams(read).toString(), new URL(href, 'http://console.test').searchParams.toString());
});

test("a breakdown cell adds its row's group as the Requests filter of that dimension", () => {
  const cases = [
    ['account', 'a', 'accounts', 'a'],
    ['client', 'claude-code/cli', 'client', 'claude-code/cli'],
    ['effort', 'none', 'efforts', 'none'],
    ['key', 'desktop@1000', 'keys', 'desktop@1000'],
    ['kind', 'inference', 'kind', 'inference'],
    ['model', 'm', 'models', 'm'],
    ['pool', 'p', 'pools', 'p'],
    ['request_class', '-', 'request_classes', '-'],
    ['speed', 'fast', 'speeds', 'fast'],
  ];
  for (const [by, key, param, value] of cases) {
    const read = target(link({ group: { by, key } }).href);
    assert.equal(read[param], value, by);
    assert.equal(read.cache_miss_reasons, 'messages_changed', by);
    assert.equal(Object.keys(read).length, 5, `${by} adds one parameter`);
  }
});

test("a group replaces the page's selection of its own dimension and joins the others", () => {
  const filters = { accountIds: ['a', 'b'], clientPattern: 'claude-*', keyIds: ['laptop', 'desktop'], models: ['m', 'n'], poolIds: ['p', 'q'] };
  assert.equal(target(link({ filters, group: { by: 'account', key: 'b' } }).href).accounts, 'b');
  assert.equal(target(link({ filters, group: { by: 'client', key: 'claude-code/cli' } }).href).client, 'claude-code/cli');
  assert.equal(target(link({ filters, group: { by: 'key', key: 'desktop' } }).href).keys, 'desktop');
  assert.equal(target(link({ filters, group: { by: 'model', key: 'n' } }).href).models, 'n');
  assert.equal(target(link({ filters, group: { by: 'pool', key: 'q' } }).href).pools, 'q');
  const joined = target(link({ filters, group: { by: 'effort', key: 'high' } }).href);
  assert.deepEqual([joined.accounts, joined.client, joined.efforts, joined.keys, joined.models, joined.pools], ['a,b', 'claude-*', 'high', 'laptop,desktop', 'm,n', 'p,q']);
});

test('a client group the rollups fold to other stays plain, since the request log matches the raw class', () => {
  for (const key of [d.OTHER_DIMENSION, 'c'.repeat(65)]) {
    const answer = link({ group: { by: 'client', key } });
    assert.equal(answer.href, null, key);
    assert.match(answer.title, /raw client class/, key);
  }
  assert.notEqual(link({ group: { by: 'client', key: 'c'.repeat(64) } }).href, null, 'a class of 64 bytes is filed as sent');
});

test('a client pattern that matches the folded other stays plain, since the request log matches the raw class', () => {
  for (const clientPattern of [d.OTHER_DIMENSION, 'OTHER', 'oth*', '?ther', '*e*']) {
    for (const group of [undefined, { by: 'key', key: 'desktop' }, { by: 'model', key: 'm' }]) {
      const answer = link({ filters: { clientPattern }, group });
      assert.equal(answer.href, null, `${clientPattern} under ${group?.by ?? 'the totals'}`);
      assert.match(answer.title, /raw client class/, clientPattern);
      assert.match(answer.title, /pattern/, clientPattern);
    }
  }
});

test('a client pattern that misses other, or matches every class, passes through as the Requests filter', () => {
  for (const clientPattern of ['claude-code/*', '*']) {
    for (const group of [undefined, { by: 'key', key: 'desktop' }]) assert.equal(target(link({ filters: { clientPattern }, group }).href).client, clientPattern, clientPattern);
  }
  // A client row's own class takes the pattern's place, and the log files that class as sent.
  assert.equal(target(link({ filters: { clientPattern: '*e*' }, group: { by: 'client', key: 'claude-code/cli' } }).href).client, 'claude-code/cli');
});

test('the user agent grouping and the user agent pattern leave every count plain, since the request log matches the raw header', () => {
  const grouped = link({ group: { by: 'user_agent', key: 'claude-cli/2.1.269' } });
  assert.equal(grouped.href, null);
  assert.match(grouped.title, /raw header/);
  assert.match(grouped.title, /folded product token/);
  for (const group of [undefined, { by: 'key', key: 'desktop' }]) {
    const patterned = link({ filters: { userAgentPattern: 'claude-cli/*' }, group });
    assert.equal(patterned.href, null);
    assert.match(patterned.title, /raw header/);
    assert.match(patterned.title, /pattern/);
  }
});

test('a zero count stays plain, and the plain cases name the first reason that holds', () => {
  assert.deepEqual(link({ count: 0 }), { href: null, title: 'No requests to list' });
  assert.equal(link({ count: 0, filters: { userAgentPattern: 'claude-cli/*' }, group: { by: 'user_agent', key: 'x/1' } }).title, 'No requests to list');
  const pattern = link({ filters: { userAgentPattern: 'claude-cli/*' } }).title;
  assert.equal(link({ filters: { userAgentPattern: 'claude-cli/*' }, group: { by: 'user_agent', key: 'x/1' } }).title, pattern);
  assert.equal(link({ filters: { userAgentPattern: 'claude-cli/*' }, group: { by: 'client', key: d.OTHER_DIMENSION } }).title, pattern);
});

test("with probes included a link says the list leaves them out, and the kind grouping's probe row lists the probes", () => {
  const counted = link({ filters: { includeProbes: true } });
  assert.notEqual(counted.href, null);
  assert.match(counted.title, /leaves the probes out/);
  assert.equal(target(counted.href).kind, undefined);
  const probes = link({ filters: { includeProbes: true }, group: { by: 'kind', key: 'probe' } });
  assert.equal(target(probes.href).kind, 'probe');
  assert.doesNotMatch(probes.title, /probes out/);
});

const NOW = at('2026-09-16T14:30:00Z');
/** A request that arrived ten minutes before now and took four seconds. */
const ROW = { accountId: 'primary', at: '2026-09-16T14:20:20.500Z', durationMs: 4_000, id: 'req_1', sessionId: SESSION };

test("the drawer's three widenings write a fresh Requests address each, in the order this minute, this session, this account, with the page size kept", () => {
  const widened = d.requestsWiden(ROW, NOW, 50);
  assert.deepEqual(
    widened.map((w) => [w.label, w.title]),
    [
      ['this minute', null],
      ['this session', SESSION],
      ['this account', 'primary'],
    ],
  );
  assert.deepEqual(
    widened.map((w) => target(w.href)),
    [
      { from: '2026-09-16T14:20:00.000Z', path: '/requests', rows: '50', to: '2026-09-16T14:20:59.999Z' },
      { from: '2026-09-16T13:20:20.500Z', path: '/requests', rows: '50', session: SESSION, to: '2026-09-16T14:30:00.000Z' },
      { accounts: 'primary', from: '2026-09-16T14:05:20.500Z', path: '/requests', rows: '50', to: '2026-09-16T14:30:00.000Z' },
    ],
  );
  assert.deepEqual(
    d.requestsWiden(ROW, NOW, 25).map((w) => target(w.href).rows),
    [undefined, undefined, undefined],
    'the default page size leaves the address',
  );
});

test('a request without a session omits this session, and one without an account omits this account', () => {
  const labels = (over) => d.requestsWiden({ ...ROW, ...over }, NOW, 25).map((w) => w.label);
  assert.deepEqual(labels({ sessionId: null }), ['this minute', 'this account']);
  assert.deepEqual(labels({ accountId: null }), ['this minute', 'this session']);
  assert.deepEqual(labels({ accountId: null, sessionId: null }), ['this minute']);
});

test('a running request widens to its account from its start', () => {
  const early = { ...ROW, at: '2026-09-16T13:00:00.000Z' };
  const account = (row) => target(d.requestsWiden(row, NOW, 25)[2].href);
  assert.deepEqual([account({ ...early, durationMs: null }).from, account({ ...early, durationMs: null }).to], ['2026-09-16T12:45:00.000Z', '2026-09-16T13:15:00.000Z']);
  assert.equal(account({ ...early, durationMs: 120_000 }).to, '2026-09-16T13:17:00.000Z', 'a finished request widens to the quarter hour after its finish');
});

test("a request newer than the page's clock sample keeps its own instant inside each widening, this minute's from before its to", () => {
  // The page samples its clock every 15 s, so a row the list read after the sample arrived later than it, here in the
  // next clock minute.
  const fresh = { ...ROW, at: '2026-09-16T14:20:05.000Z' };
  const sample = at('2026-09-16T14:19:55Z');
  const widened = d.requestsWiden(fresh, sample, 25).map((w) => target(w.href));
  const arrived = Date.parse(fresh.at);
  assert.equal(widened.length, 3);
  for (const read of widened) {
    assert.ok(Date.parse(read.from) < Date.parse(read.to), `${read.from} before ${read.to}`);
    assert.ok(Date.parse(read.from) <= arrived && arrived <= Date.parse(read.to), `${read.from} to ${read.to} holds the request`);
  }
  assert.deepEqual([widened[0].from, widened[0].to], ['2026-09-16T14:20:00.000Z', '2026-09-16T14:20:09.000Z'], "this minute runs to the request's end");
});
