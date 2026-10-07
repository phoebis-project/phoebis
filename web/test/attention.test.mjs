import assert from 'node:assert/strict';
import { test } from 'node:test';
import { ATTENTION_KIND_VALUES, ATTENTION_LABEL, ATTENTION_REFRESH_MS, ATTENTION_TONE, attentionDetail, attentionHref, attentionTitle, createAttentionPoll, decodeAttention, fmtAbsolute, fmtShortTime } from './dist/api.js';

const NOW = Date.parse('2026-09-18T12:00:00.000Z');
const item = (kind, over = {}) => ({ detail: {}, kind, link: null, pools: [], since: '2026-09-18T10:00:00.000Z', subject: { id: kind, label: kind, type: 'account' }, ...over });
/** Lets every settled promise run its handlers, which the mocked interval leaves to the real event loop. */
const flush = () => new Promise((resolve) => setImmediate(resolve));

test('the attention kinds rank worst first, the order the route answers in', () => {
  assert.deepEqual([...ATTENTION_KIND_VALUES], ['pool_exhausted', 'client_drift', 'writer_degraded', 'reauth_required', 'funding_required', 'refresh_failed', 'ledger_record_parked', 'poll_backoff']);
});

test('each attention kind has its word and its tone, a client drift serious', () => {
  assert.deepEqual(ATTENTION_LABEL, {
    client_drift: 'Client drift',
    funding_required: 'Funding required',
    ledger_record_parked: 'Priced record parked',
    poll_backoff: 'Usage poll backing off',
    pool_exhausted: 'Pool exhausted',
    reauth_required: 'Re-auth required',
    refresh_failed: 'Credential refresh failed',
    writer_degraded: 'State writer degraded',
  });
  assert.deepEqual(ATTENTION_KIND_VALUES.map((kind) => ATTENTION_TONE[kind]), ['critical', 'serious', 'serious', 'critical', 'serious', 'critical', 'serious', 'warning']);
});

test('an item opens the page where its condition is fixed, and an item without a link opens nothing', () => {
  assert.equal(attentionHref(item('reauth_required', { link: { id: 'legacy', page: 'accounts' } }), NOW), '/accounts/legacy');
  assert.equal(attentionHref(item('client_drift', { link: { id: 'laptop', page: 'keys' } }), NOW), '/keys#laptop');
  assert.equal(attentionHref(item('client_drift', { link: { id: 'a b#c', page: 'keys' } }), NOW), '/keys#a%20b%23c', 'an id travels encoded');
  assert.equal(attentionHref(item('client_drift'), NOW), null, 'a drift refusal that carried no key has nothing to open');
  assert.equal(attentionHref(item('writer_degraded', { subject: { id: 'routing', label: 'routing', type: 'router' } }), NOW), null, "the router's own component has nothing to open");
});

test('a pool exhausted item opens the routing log since the condition began and the other kinds keep their targets', () => {
  const exhausted = item('pool_exhausted', { link: { id: 'codex', page: 'pools' }, since: '2026-09-18T10:41:00.000Z' });
  const href = attentionHref(exhausted, NOW);
  const url = new URL(href, 'http://console.test');
  assert.equal(url.pathname, '/routing-log');
  assert.deepEqual(Object.fromEntries(url.searchParams), { from: '2026-09-18T10:41:00.000Z', kinds: 'exhausted', pools: 'codex', to: '2026-09-18T12:00:00.000Z' }, 'the pool and the exhausted kind from the start of the condition to now');
  assert.equal(attentionHref(item('pool_exhausted', { link: { id: 'a b', page: 'pools' } }), NOW), '/routing-log?kinds=exhausted&pools=a+b&from=2026-09-18T10%3A00%3A00.000Z&to=2026-09-18T12%3A00%3A00.000Z', 'the pool id travels encoded');
  assert.equal(attentionHref(item('pool_exhausted'), NOW), null, 'a pool deleted since has nothing to open');
  assert.equal(attentionHref(item('ledger_record_parked', { link: { id: 'codex', page: 'pools' } }), NOW), '/pools#codex', "a parked record keeps its pool's card");
  assert.equal(attentionHref(item('reauth_required', { link: { id: 'legacy', page: 'accounts' } }), NOW), '/accounts/legacy');
  assert.equal(attentionHref(item('client_drift', { link: { id: 'laptop', page: 'keys' } }), NOW), '/keys#laptop');
});

test("a row reads each kind's detail as one sentence", () => {
  const sentence = (kind, detail) => attentionDetail(item(kind, { detail }), NOW);
  assert.equal(sentence('pool_exhausted', { key: 'ci', pool: 'codex' }), 'no member could take a request from key ci');
  assert.equal(sentence('pool_exhausted', { key: null, pool: 'codex' }), 'no member could take a request');
  assert.equal(sentence('client_drift', { client_version: '2.1.271', key: 'laptop', trigger: 'hash' }), 'its billing hash differs from the pinned build, newest refusal from key laptop');
  assert.equal(sentence('client_drift', { client_version: '2.1.271', key: null, trigger: 'version_suffix' }), 'its version suffix differs from the pinned build');
  assert.equal(sentence('reauth_required', { account: 'legacy', label: 'legacy' }), 'serves nothing until its credential is renewed');
  assert.equal(sentence('funding_required', { account: 'research', label: 'Research workspace' }), 'serves nothing until its organization is funded again');
  assert.equal(sentence('refresh_failed', { account: 'legacy', failures: 5, last_error: 'refresh_token rejected: invalid_grant', outcome: 'failed' }), '5 refreshes failed in a row: refresh_token rejected: invalid_grant');
  assert.equal(sentence('refresh_failed', { account: 'legacy', failures: 2, last_error: null, outcome: 'uncertain' }), '2 refreshes failed in a row');
  assert.equal(sentence('poll_backoff', { account: 'secondary', backoff_since: NOW - 42 * 60_000, next_attempt_at: NOW + 4 * 60_000, rate_limited_in_a_row: 6 }), '6 rate-limited answers in a row, next attempt in 4m');
  assert.equal(sentence('poll_backoff', { account: 'secondary', backoff_since: NOW - 42 * 60_000, next_attempt_at: null, rate_limited_in_a_row: 1 }), '1 rate-limited answer in a row');
  assert.equal(sentence('writer_degraded', { component: 'ledger' }), 'ledger batches are being retried; budgeted members are held back until a batch lands');
  assert.equal(sentence('writer_degraded', { component: 'routing' }), 'routing state is kept in memory until the registry file takes writes again');
  assert.equal(sentence('ledger_record_parked', { accounts: ['research'], count: 1, pool: 'codex' }), '1 priced record parked, landing at the next start (research)');
  assert.equal(sentence('ledger_record_parked', { accounts: ['primary', 'overflow'], count: 3, pool: null }), '3 priced records parked, landing at the next start (primary, overflow)');
  assert.equal(sentence('ledger_record_parked', { accounts: [], count: 2, pool: 'codex' }), '2 priced records parked, landing at the next start', 'a record that names no account adds none');
});

test("a badge's title lists the detail's members, an instant read as one, then the age", () => {
  const backoff = item('poll_backoff', { detail: { account: 'secondary', backoff_since: NOW - 42 * 60_000, next_attempt_at: null, rate_limited_in_a_row: 6 }, since: new Date(NOW - 42 * 60_000).toISOString() });
  assert.equal(attentionTitle(backoff, NOW), ['account: secondary', `backoff_since: ${fmtAbsolute(new Date(NOW - 42 * 60_000).toISOString())}`, 'rate_limited_in_a_row: 6', 'started 42m ago'].join('\n'));
  assert.equal(attentionTitle(item('ledger_record_parked', { detail: { pool: 'codex', count: 1 } }), NOW), ['count: 1', 'pool: codex', 'started 2h 0m ago'].join('\n'), 'the members read in name order');
});

test('a kind this build does not know opens its link, never a pool log named by an account', () => {
  // The route sends the start in milliseconds.
  const since = Date.parse('2026-09-18T10:00:00.000Z');
  const later = decodeAttention({ detail: { account: 'legacy' }, kind: 'quota_drift', link: { id: 'legacy', page: 'accounts' }, pools: [], since, subject: { id: 'legacy', label: 'legacy', type: 'account' } });
  assert.equal(later.kind, 'pool_exhausted', 'the decoder reads an unknown kind as the worst one');
  assert.equal(attentionHref(later, NOW), '/accounts/legacy');
  assert.equal(attentionTitle(later, NOW).split('\n').pop(), 'started 2h 0m ago', 'and its title names no log');
  const pooled = decodeAttention({ detail: {}, kind: 'quota_drift', link: { id: 'codex', page: 'pools' }, pools: ['codex'], since, subject: { id: 'codex', label: 'codex', type: 'pool' } });
  assert.equal(new URL(attentionHref(pooled, NOW), 'http://console.test').pathname, '/routing-log', 'one that names a pool opens its log');
});

test('the pool exhausted title names the day of a condition that began more than a day ago', () => {
  const since = '2026-09-16T10:41:00.000Z';
  const exhausted = item('pool_exhausted', { detail: { pool: 'codex' }, link: { id: 'codex', page: 'pools' }, since });
  const day = new Date(since).toLocaleDateString('en-US', { day: 'numeric', month: 'short' });
  assert.match(day, /^[A-Z][a-z]{2} \d{1,2}$/);
  assert.equal(attentionTitle(exhausted, NOW).split('\n').pop(), `Opens the Routing log: pool codex, exhausted, since ${day} ${fmtShortTime(since)}`);
});

test('the pool exhausted title ends with the sentence that names the log', () => {
  const since = '2026-09-18T10:41:00.000Z';
  const exhausted = item('pool_exhausted', { detail: { pool: 'codex', key: 'ci' }, link: { id: 'codex', page: 'pools' }, since });
  assert.match(fmtShortTime(since), /^\d{2}:\d{2}$/, 'the condition starts at a clock time');
  assert.equal(attentionTitle(exhausted, NOW), ['key: ci', 'pool: codex', 'started 1h 19m ago', `Opens the Routing log: pool codex, exhausted, since ${fmtShortTime(since)}`].join('\n'));
  assert.equal(attentionTitle({ ...exhausted, link: null }, NOW).split('\n').pop(), 'started 1h 19m ago', 'an item that opens nothing names no log');
  assert.equal(attentionTitle(item('ledger_record_parked', { detail: { count: 1 }, link: { id: 'codex', page: 'pools' }, since }), NOW).split('\n').pop(), 'started 1h 19m ago', 'another kind ends with its age');
});

test('one poll serves every reader, reads again on its schedule, and keeps the last list through a failed read', async (t) => {
  t.mock.timers.enable({ apis: ['setInterval'] });
  let reads = 0;
  let answer = () => Promise.resolve([item('pool_exhausted')]);
  const poll = createAttentionPoll(() => {
    reads += 1;
    return answer();
  });
  assert.deepEqual(poll.snapshot(), { error: null, items: null, loadedAt: null }, 'nothing is read before a reader subscribes');
  let heard = 0;
  const first = poll.subscribe(() => {
    heard += 1;
  });
  const second = poll.subscribe(() => {});
  assert.equal(reads, 1, 'the first reader starts the read and the second shares it');
  await flush();
  const loaded = poll.snapshot();
  assert.deepEqual([loaded.error, loaded.items.map((i) => i.kind), typeof loaded.loadedAt], [null, ['pool_exhausted'], 'number']);
  assert.ok(heard > 0, 'a reader hears the answer');

  answer = () => Promise.reject(new Error('the store is unavailable'));
  t.mock.timers.tick(ATTENTION_REFRESH_MS - 1);
  assert.equal(reads, 1, 'the schedule waits its whole interval');
  t.mock.timers.tick(1);
  assert.equal(reads, 2);
  await flush();
  assert.deepEqual(poll.snapshot(), { error: 'the store is unavailable', items: loaded.items, loadedAt: loaded.loadedAt }, 'a failed read keeps the last list and when it was read');

  answer = () => Promise.resolve([]);
  t.mock.timers.tick(ATTENTION_REFRESH_MS);
  await flush();
  assert.deepEqual([reads, poll.snapshot().error, poll.snapshot().items], [3, null, []], 'the next answer clears the failure');

  first();
  t.mock.timers.tick(ATTENTION_REFRESH_MS);
  assert.equal(reads, 4, 'the schedule runs while any reader is left');
  second();
  t.mock.timers.tick(ATTENTION_REFRESH_MS * 3);
  assert.equal(reads, 4, 'the schedule stops with its last reader');
});

test('an older read that answers after a newer one leaves the newer answer standing', async (t) => {
  t.mock.timers.enable({ apis: ['setInterval'] });
  const pending = [];
  const poll = createAttentionPoll(() => new Promise((resolve) => pending.push(resolve)));
  const off = poll.subscribe(() => {});
  t.mock.timers.tick(ATTENTION_REFRESH_MS);
  assert.equal(pending.length, 2);
  pending[1]([]);
  await flush();
  pending[0]([item('pool_exhausted')]);
  await flush();
  assert.deepEqual(poll.snapshot().items, []);
  off();
});
