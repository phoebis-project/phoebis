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

test("the page's selections, its class pattern and a session id travel as the Requests page reads them", () => {
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
    search: SESSION,
    to: '2026-09-16T14:30:00.000Z',
  });
});

test("a session read under a key lifetime links with that lifetime alone, in place of the page's key selection", () => {
  assert.deepEqual(d.linkedKeyIds([], 'desktop@1000'), ['desktop@1000']);
  assert.deepEqual(d.linkedKeyIds(['laptop'], 'desktop@1000'), ['desktop@1000']);
  assert.deepEqual(d.linkedKeyIds(['laptop'], null), ['laptop']);
  const read = target(link({ filters: { keyIds: d.linkedKeyIds(['laptop'], 'desktop@1000'), sessionId: SESSION }, window: { exact: true } }).href);
  assert.equal(read.keys, 'desktop@1000');
  assert.equal(read.search, SESSION);
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
