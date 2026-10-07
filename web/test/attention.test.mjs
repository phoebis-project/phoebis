import assert from 'node:assert/strict';
import { test } from 'node:test';
import { ATTENTION_KIND_VALUES, ATTENTION_LABEL, ATTENTION_REFRESH_MS, ATTENTION_TONE, attentionDetail, attentionHref, attentionTitle, createAttentionPoll, decodeAttention, fmtAbsolute } from './dist/api.js';

const NOW = Date.parse('2026-09-18T12:00:00.000Z');
const item = (kind, over = {}) => ({ detail: {}, kind, link: null, pools: [], since: '2026-09-18T10:00:00.000Z', subject: { id: kind, label: kind, type: 'account' }, ...over });
/** Lets every settled promise run its handlers, which the mocked interval leaves to the real event loop. */
const flush = () => new Promise((resolve) => setImmediate(resolve));

test('the attention kinds rank worst first, the order the route answers in', () => {
  assert.deepEqual([...ATTENTION_KIND_VALUES], ['pool_exhausted', 'client_drift', 'writer_degraded', 'certificate_reload_failed', 'reauth_required', 'funding_required', 'refresh_failed', 'certificate_expiring', 'ledger_record_parked', 'poll_backoff']);
});

test("a listener's two certificate kinds decode as themselves, never as the worst kind", () => {
  const router = { link: null, pools: [], since: 1_000_000, subject: { id: 'proxy.tls', label: 'proxy.tls', type: 'router' } };
  const kinds = ['certificate_expiring', 'certificate_reload_failed'].map((kind) => decodeAttention({ ...router, detail: {}, kind }).kind);
  assert.deepEqual(kinds, ['certificate_expiring', 'certificate_reload_failed']);
});

test('each attention kind has its word and its tone, a client drift serious', () => {
  assert.deepEqual(ATTENTION_LABEL, {
    certificate_expiring: 'Certificate expiring',
    certificate_reload_failed: 'Certificate reload failed',
    client_drift: 'Client drift',
    funding_required: 'Funding required',
    ledger_record_parked: 'Priced record parked',
    poll_backoff: 'Usage poll backing off',
    pool_exhausted: 'Pool exhausted',
    reauth_required: 'Re-auth required',
    refresh_failed: 'Credential refresh failed',
    writer_degraded: 'State writer degraded',
  });
  assert.deepEqual(ATTENTION_KIND_VALUES.map((kind) => ATTENTION_TONE[kind]), ['critical', 'serious', 'serious', 'serious', 'critical', 'serious', 'critical', 'warning', 'serious', 'warning']);
});

test('an item opens the page where its condition is fixed, and an item without a link opens nothing', () => {
  assert.equal(attentionHref(item('reauth_required', { link: { id: 'legacy', page: 'accounts' } })), '/accounts/legacy');
  assert.equal(attentionHref(item('pool_exhausted', { link: { id: 'codex', page: 'pools' } })), '/pools#codex');
  assert.equal(attentionHref(item('client_drift', { link: { id: 'laptop', page: 'keys' } })), '/keys#laptop');
  assert.equal(attentionHref(item('client_drift', { link: { id: 'a b#c', page: 'keys' } })), '/keys#a%20b%23c', 'an id travels encoded');
  assert.equal(attentionHref(item('client_drift')), null, 'a drift refusal that carried no key has nothing to open');
  assert.equal(attentionHref(item('writer_degraded', { subject: { id: 'routing', label: 'routing', type: 'router' } })), null, "the router's own component has nothing to open");
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
  assert.equal(sentence('ledger_record_parked', { accounts: ['research'], count: 1, pool: 'codex' }), "1 priced record parked, landing at the next start or on the dashboard's Replay (research)");
  assert.equal(sentence('ledger_record_parked', { accounts: ['primary', 'overflow'], count: 3, pool: null }), "3 priced records parked, landing at the next start or on the dashboard's Replay (primary, overflow)");
  assert.equal(sentence('ledger_record_parked', { accounts: [], count: 2, pool: 'codex' }), "2 priced records parked, landing at the next start or on the dashboard's Replay", 'a record that names no account adds none');
});

test("a listener's certificate items read the listener, the file, the date and what to do", () => {
  const listener = (kind, id, detail) => attentionDetail(item(kind, { detail, subject: { id, label: id, type: 'router' } }), NOW);
  const ahead = NOW + 12 * 86_400_000;
  const behind = NOW - 3 * 86_400_000;
  const date = (ms) => fmtAbsolute(new Date(ms).toISOString());
  assert.equal(
    listener('certificate_expiring', 'proxy.tls', { cert_file: '/etc/phoebis/proxy.pem', days_left: 12, expires: ahead }),
    `The proxy listener's certificate /etc/phoebis/proxy.pem expires in 12 days (${date(ahead)}). Replace the pair and run phoebis tls reload.`,
  );
  assert.equal(
    listener('certificate_expiring', 'admin.tls', { cert_file: '/etc/phoebis/admin.pem', days_left: -3, expires: behind }),
    `The admin listener's certificate /etc/phoebis/admin.pem expired 3 days ago (${date(behind)}). Replace the pair and run phoebis tls reload.`,
    'a leaf past its end counts the days since',
  );
  assert.equal(
    listener('certificate_expiring', 'proxy.tls', { cert_file: '/etc/phoebis/proxy.pem', days_left: 1, expires: NOW + 86_400_000 }),
    `The proxy listener's certificate /etc/phoebis/proxy.pem expires in 1 day (${date(NOW + 86_400_000)}). Replace the pair and run phoebis tls reload.`,
  );
  const loaded = NOW - 40 * 86_400_000;
  assert.equal(
    listener('certificate_reload_failed', 'proxy.tls', {
      cert_file: '/etc/phoebis/proxy.pem',
      error: 'proxy.tls.key_file /etc/phoebis/proxy.key: does not match the certificate in /etc/phoebis/proxy.pem',
      key_file: '/etc/phoebis/proxy.key',
      serving_since: loaded,
    }),
    `The proxy listener's pair did not load: proxy.tls.key_file /etc/phoebis/proxy.key: does not match the certificate in /etc/phoebis/proxy.pem. The router keeps serving the pair it loaded at ${date(loaded)}.`,
  );
});

test("a badge's title lists the detail's members, an instant read as one, then the age", () => {
  const backoff = item('poll_backoff', { detail: { account: 'secondary', backoff_since: NOW - 42 * 60_000, next_attempt_at: null, rate_limited_in_a_row: 6 }, since: new Date(NOW - 42 * 60_000).toISOString() });
  assert.equal(attentionTitle(backoff, NOW), ['account: secondary', `backoff_since: ${fmtAbsolute(new Date(NOW - 42 * 60_000).toISOString())}`, 'rate_limited_in_a_row: 6', 'started 42m ago'].join('\n'));
  assert.equal(attentionTitle(item('pool_exhausted', { detail: { pool: 'codex', key: 'ci' } }), NOW), ['key: ci', 'pool: codex', 'started 2h 0m ago'].join('\n'), 'the members read in name order');
  const expires = NOW + 12 * 86_400_000;
  const expiring = item('certificate_expiring', { detail: { cert_file: '/etc/phoebis/proxy.pem', days_left: 12, expires }, since: new Date(NOW - 18 * 86_400_000).toISOString() });
  assert.equal(attentionTitle(expiring, NOW), ['cert_file: /etc/phoebis/proxy.pem', 'days_left: 12', `expires: ${fmtAbsolute(new Date(expires).toISOString())}`, 'started 18d 0h ago'].join('\n'), "a certificate's end reads as an instant");
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
