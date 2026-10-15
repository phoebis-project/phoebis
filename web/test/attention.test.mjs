import assert from 'node:assert/strict';
import { test } from 'node:test';
import { ATTENTION_KIND_VALUES, ATTENTION_LABEL, ATTENTION_REFRESH_MS, ATTENTION_TONE, attentionDetail, attentionHref, attentionLogLine, attentionTitle, attentionWords, createAttentionPoll, decodeAttention, fmtAbsolute, fmtShortTime } from './dist/api.js';

const NOW = Date.parse('2026-09-18T12:00:00.000Z');
const item = (kind, over = {}) => ({ detail: {}, kind, link: null, pools: [], since: '2026-09-18T10:00:00.000Z', subject: { id: kind, label: kind, type: 'account' }, ...over });
/** Lets every settled promise run its handlers, which the mocked interval leaves to the real event loop. */
const flush = () => new Promise((resolve) => setImmediate(resolve));

test('the attention kinds rank worst first, the order the route answers in', () => {
  assert.deepEqual([...ATTENTION_KIND_VALUES], ['pool_exhausted', 'client_drift', 'writer_degraded', 'certificate_reload_failed', 'reauth_required', 'funding_required', 'refresh_failed', 'certificate_expiring', 'ledger_record_parked', 'poll_backoff', 'model_unweighted', 'wall_reached', 'destination_spent', 'shed_no_runway']);
});

test("a listener's two certificate kinds decode as themselves, never as the worst kind", () => {
  const router = { link: null, pools: [], since: 1_000_000, subject: { id: 'proxy.tls', label: 'proxy.tls', type: 'router' } };
  const kinds = ['certificate_expiring', 'certificate_reload_failed'].map((kind) => decodeAttention({ ...router, detail: {}, kind }).kind);
  assert.deepEqual(kinds, ['certificate_expiring', 'certificate_reload_failed']);
});

test('each attention kind has its word and its tone, a client drift and a failed refresh serious', () => {
  assert.deepEqual(ATTENTION_LABEL, {
    certificate_expiring: 'Certificate expiring',
    certificate_reload_failed: 'Certificate reload failed',
    client_drift: 'Client drift',
    destination_spent: 'Destination spent within the hour',
    funding_required: 'Funding required',
    ledger_record_parked: 'Priced record parked',
    model_unweighted: 'Model weight estimated',
    poll_backoff: 'Usage poll backing off',
    pool_exhausted: 'Pool exhausted',
    reauth_required: 'Re-auth required',
    refresh_failed: 'Credential refresh failed',
    shed_no_runway: 'Rounds without a destination',
    wall_reached: 'Wall reached under the ceiling',
    writer_degraded: 'State writer degraded',
  });
  assert.deepEqual(ATTENTION_KIND_VALUES.map((kind) => ATTENTION_TONE[kind]), ['critical', 'serious', 'serious', 'serious', 'critical', 'serious', 'serious', 'warning', 'serious', 'warning', 'warning', 'warning', 'warning', 'warning']);
});

test('an item opens the page where its condition is fixed, and an item without a link opens nothing', () => {
  assert.equal(attentionHref(item('reauth_required', { link: { id: 'legacy', page: 'accounts' } }), NOW), '/accounts/legacy');
  assert.equal(attentionHref(item('client_drift', { link: { id: 'laptop', page: 'keys' } }), NOW), '/keys#laptop');
  assert.equal(attentionHref(item('client_drift', { link: { id: 'a b#c', page: 'keys' } }), NOW), '/keys#a%20b%23c', 'an id travels encoded');
  assert.equal(attentionHref(item('client_drift'), NOW), null, 'a drift refusal that carried no key has nothing to open');
  assert.equal(attentionHref(item('writer_degraded', { subject: { id: 'routing', label: 'routing', type: 'router' } }), NOW), null, "the router's own component has nothing to open");
  const model = decodeAttention({ detail: {}, kind: 'model_unweighted', link: { id: 'claude-sonnet-4-6', page: 'models' }, pools: [], since: NOW, subject: { id: 'claude-sonnet-4-6', label: 'claude-sonnet-4-6', type: 'model' } });
  assert.equal(attentionHref(model, NOW), '/models#claude-sonnet-4-6', "an unweighted model's item opens the Models page on the model");
});

test('a pool exhausted item opens the routing log since the condition began and the other kinds keep their targets', () => {
  const exhausted = item('pool_exhausted', { link: { id: 'jobs', page: 'pools' }, since: '2026-09-18T10:41:00.000Z' });
  const href = attentionHref(exhausted, NOW);
  const url = new URL(href, 'http://console.test');
  assert.equal(url.pathname, '/routing-log');
  assert.deepEqual(Object.fromEntries(url.searchParams), { from: '2026-09-18T10:41:00.000Z', kinds: 'exhausted', pools: 'jobs', to: '2026-09-18T12:00:00.000Z' }, 'the pool and the exhausted kind from the start of the condition to now');
  assert.equal(attentionHref(item('pool_exhausted', { link: { id: 'a b', page: 'pools' } }), NOW), '/routing-log?kinds=exhausted&pools=a+b&from=2026-09-18T10%3A00%3A00.000Z&to=2026-09-18T12%3A00%3A00.000Z', 'the pool id travels encoded');
  assert.equal(attentionHref(item('pool_exhausted'), NOW), null, 'a pool deleted since has nothing to open');
  assert.equal(attentionHref(item('ledger_record_parked', { link: { id: 'jobs', page: 'pools' } }), NOW), '/pools#jobs', "a parked record keeps its pool's card");
  assert.equal(attentionHref(item('reauth_required', { link: { id: 'legacy', page: 'accounts' } }), NOW), '/accounts/legacy');
  assert.equal(attentionHref(item('client_drift', { link: { id: 'laptop', page: 'keys' } }), NOW), '/keys#laptop');
});

test("a row reads each kind's detail as one sentence", () => {
  const sentence = (kind, detail) => attentionDetail(item(kind, { detail }), NOW);
  assert.equal(sentence('pool_exhausted', { key: 'ci', pool: 'jobs' }), 'no member could take a request from key ci');
  assert.equal(sentence('pool_exhausted', { key: null, pool: 'jobs' }), 'no member could take a request');
  assert.equal(sentence('client_drift', { client_version: '2.1.271', key: 'laptop', trigger: 'hash' }), 'its billing hash differs from the pinned build, newest refusal from key laptop');
  assert.equal(sentence('client_drift', { client_version: '2.1.271', key: null, trigger: 'version_suffix' }), 'its version suffix differs from the pinned build');
  assert.equal(sentence('reauth_required', { account: 'legacy', label: 'legacy' }), 'serves nothing until its credential is renewed');
  assert.equal(sentence('funding_required', { account: 'research', label: 'Research workspace' }), 'serves nothing until its organization is funded again');
  assert.equal(sentence('refresh_failed', { account: 'legacy', failures: 5, last_error: 'refresh_token rejected: invalid_grant', outcome: 'failed' }), '5 refreshes failed in a row: refresh_token rejected: invalid_grant');
  assert.equal(sentence('refresh_failed', { account: 'legacy', failures: 2, last_error: null, outcome: 'uncertain' }), '2 refreshes failed in a row');
  assert.equal(sentence('poll_backoff', { account: 'secondary', backoff_since: NOW - 42 * 60_000, next_attempt_at: NOW + 4 * 60_000, rate_limited_in_a_row: 6 }), '6 rate-limited answers in a row, next attempt in 4m');
  assert.equal(sentence('poll_backoff', { account: 'secondary', backoff_since: NOW - 42 * 60_000, next_attempt_at: null, rate_limited_in_a_row: 1 }), '1 rate-limited answer in a row');
  assert.equal(sentence('writer_degraded', { component: 'ledger' }), 'ledger batches are being retried, and budgeted members are held back until a batch lands');
  assert.equal(sentence('writer_degraded', { component: 'routing' }), 'routing state is kept in memory until the registry file takes writes again');
  assert.equal(sentence('ledger_record_parked', { accounts: ['research'], count: 1, pool: 'jobs' }), "1 priced record parked, landing at the next start or on the dashboard's Replay (research)");
  assert.equal(sentence('ledger_record_parked', { accounts: ['primary', 'overflow'], count: 3, pool: null }), "3 priced records parked, landing at the next start or on the dashboard's Replay (primary, overflow)");
  assert.equal(sentence('ledger_record_parked', { accounts: [], count: 2, pool: 'jobs' }), "2 priced records parked, landing at the next start or on the dashboard's Replay", 'a record that names no account adds none');
  const since = fmtAbsolute(item('model_unweighted').since);
  assert.equal(
    sentence('model_unweighted', { model: 'claude-sonnet-4-6', requests: 12, tier: 'sonnet', weight_of: 'claude-sonnet-5-5' }),
    `claude-sonnet-4-6 has no measured weight, so its 12 plan-funded requests since ${since} priced at the weight of claude-sonnet-5-5, the latest weighted model of the sonnet model tier`,
  );
  assert.equal(
    sentence('model_unweighted', { model: 'claude-opus-99', requests: 1, tier: null, weight_of: null }),
    `claude-opus-99 has no measured weight, so its 1 plan-funded request since ${since} priced at the weight of the latest weighted model of its model tier`,
    'a detail missing its tier and its donor names neither',
  );
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

test("a certificate item reads its last day and its first day past the end as today, and the floored days elsewhere", () => {
  const words = (left) => {
    const expires = NOW + left * 86_400_000;
    const line = attentionDetail(item('certificate_expiring', { detail: { cert_file: '/etc/phoebis/proxy.pem', days_left: left, expires }, subject: { id: 'proxy.tls', label: 'proxy.tls', type: 'router' } }), NOW);
    return line.replace(`The proxy listener's certificate /etc/phoebis/proxy.pem `, '').replace(/ \(.*$/, '');
  };
  assert.equal(words(0), 'expires today', 'a leaf that ends within a day floors to 0');
  assert.equal(words(-1), 'expired today', 'a leaf that ended within the last day floors to -1');
  assert.equal(words(1), 'expires in 1 day');
  assert.equal(words(-2), 'expired 2 days ago');
});

test("a badge's title lists the detail's members, an instant read as one, then the age", () => {
  const backoff = item('poll_backoff', { detail: { account: 'secondary', backoff_since: NOW - 42 * 60_000, next_attempt_at: null, rate_limited_in_a_row: 6 }, pools: ['all'], since: new Date(NOW - 42 * 60_000).toISOString() });
  assert.equal(attentionTitle(backoff, NOW), ['account: secondary', `backoff_since: ${fmtAbsolute(new Date(NOW - 42 * 60_000).toISOString())}`, 'rate_limited_in_a_row: 6', 'started 42m ago'].join('\n'));
  assert.equal(attentionTitle(item('ledger_record_parked', { detail: { pool: 'jobs', count: 1 }, pools: ['jobs'] }), NOW), ['count: 1', 'pool: jobs', 'started 2h ago'].join('\n'), 'the members read in name order');
});

test("a badge's title for an item that carries no pool leads with its sentence, which no pool card's row draws", () => {
  const expires = NOW + 12 * 86_400_000;
  const expiring = item('certificate_expiring', { detail: { cert_file: '/etc/phoebis/proxy.pem', days_left: 12, expires }, since: new Date(NOW - 18 * 86_400_000).toISOString(), subject: { id: 'proxy.tls', label: 'proxy.tls', type: 'router' } });
  assert.equal(
    attentionTitle(expiring, NOW),
    [`The proxy listener's certificate /etc/phoebis/proxy.pem expires in 12 days (${fmtAbsolute(new Date(expires).toISOString())}). Replace the pair and run phoebis tls reload.`, 'cert_file: /etc/phoebis/proxy.pem', 'days_left: 12', `expires: ${fmtAbsolute(new Date(expires).toISOString())}`, 'started 18d ago'].join('\n'),
    "a certificate's end reads as an instant",
  );
  const degraded = item('writer_degraded', { detail: { component: 'ledger' } });
  assert.equal(attentionTitle(degraded, NOW), [attentionDetail(degraded, NOW), 'component: ledger', 'started 2h ago'].join('\n'));
  const model = item('model_unweighted', { detail: { model: 'claude-sonnet-4-6', requests: 12, tier: 'sonnet', weight_of: 'claude-sonnet-5-5' }, subject: { id: 'claude-sonnet-4-6', label: 'claude-sonnet-4-6', type: 'model' } });
  assert.equal(attentionTitle(model, NOW).split('\n')[0], attentionDetail(model, NOW), "a model's item belongs to no pool card, so its title leads with its sentence");
  const pooled = item('writer_degraded', { detail: { component: 'ledger' }, pools: ['all'] });
  assert.equal(attentionTitle(pooled, NOW), ['component: ledger', 'started 2h ago'].join('\n'), "an item a pool carries reads its sentence on that pool's card");
});

test('a kind this build does not know opens its link, never a pool log named by an account', () => {
  // The route sends the start in milliseconds.
  const since = Date.parse('2026-09-18T10:00:00.000Z');
  const later = decodeAttention({ detail: { account: 'legacy' }, kind: 'quota_drift', link: { id: 'legacy', page: 'accounts' }, pools: [], since, subject: { id: 'legacy', label: 'legacy', type: 'account' } });
  assert.equal(later.kind, 'pool_exhausted', 'the decoder reads an unknown kind as the worst one');
  assert.equal(attentionHref(later, NOW), '/accounts/legacy');
  assert.equal(attentionTitle(later, NOW).split('\n').pop(), 'started 2h ago', 'and its title names no log');
  const pooled = decodeAttention({ detail: {}, kind: 'quota_drift', link: { id: 'jobs', page: 'pools' }, pools: ['jobs'], since, subject: { id: 'jobs', label: 'jobs', type: 'pool' } });
  assert.equal(new URL(attentionHref(pooled, NOW), 'http://console.test').pathname, '/routing-log', 'one that names a pool opens its log');
  const known = ['wall_reached', 'destination_spent', 'shed_no_runway'].map((kind) => decodeAttention({ detail: {}, kind, link: null, pools: [], since, subject: { id: 'x', label: 'x', type: 'account' } }).kind);
  assert.deepEqual(known, ['wall_reached', 'destination_spent', 'shed_no_runway'], 'the three routing conditions decode as themselves');
});

test("the three routing conditions open the routing log on their subject's moves, walls and rounds since the condition began", () => {
  const since = '2026-09-18T10:41:00.000Z';
  const window = { from: since, to: '2026-09-18T12:00:00.000Z' };
  const query = (it) => {
    const url = new URL(attentionHref(it, NOW), 'http://console.test');
    return [url.pathname, Object.fromEntries(url.searchParams)];
  };
  const account = { link: { id: 'primary', page: 'accounts' }, pools: ['all'], since, subject: { id: 'primary', label: 'primary', type: 'account' } };
  const pool = { link: { id: 'all', page: 'pools' }, pools: ['all'], since, subject: { id: 'all', label: 'all', type: 'pool' } };
  assert.deepEqual(query(item('shed_no_runway', pool)), ['/routing-log', { ...window, kinds: 'shedding_round', pools: 'all' }]);
  assert.deepEqual(query(item('destination_spent', account)), ['/routing-log', { ...window, accounts: 'primary', kinds: 'pin_migrated,window_spent' }]);
  assert.deepEqual(query(item('wall_reached', account)), ['/routing-log', { ...window, accounts: 'primary', kinds: 'window_spent' }]);
  assert.equal(attentionHref(item('wall_reached', { ...account, link: null }), NOW), null, 'an account the registry no longer holds opens nothing');
  const at = fmtShortTime(since);
  assert.deepEqual(
    [attentionLogLine(item('shed_no_runway', pool), NOW), attentionLogLine(item('destination_spent', account), NOW), attentionLogLine(item('wall_reached', account), NOW)],
    [`Opens the Routing log: pool all, shedding rounds, since ${at}`, `Opens the Routing log: account primary, moves and spent windows, since ${at}`, `Opens the Routing log: account primary, spent windows, since ${at}`],
  );
  assert.equal(attentionTitle(item('shed_no_runway', pool), NOW).split('\n').pop(), `Opens the Routing log: pool all, shedding rounds, since ${at}`, 'the title ends with what the log opens on');
});

test('the three routing conditions read the count, the setting and the start on the dashboard line and the pool card', () => {
  const since = '2026-09-18T10:41:00.000Z';
  const at = fmtShortTime(since);
  const account = { link: { id: 'primary', page: 'accounts' }, pools: ['all'], since, subject: { id: 'primary', label: 'primary', type: 'account' } };
  const main = { link: { id: 'main', page: 'pools' }, pools: ['main'], since, subject: { id: 'main', label: 'main', type: 'pool' } };
  const words = (kind, over, detail) => attentionWords(item(kind, { ...over, detail }), NOW);
  assert.equal(words('shed_no_runway', main, { accounts: ['primary'], rounds: 4, setting: ['window_ceiling'], setting_values: { window_ceiling: 0.95 } }), `main: 4 rounds marked nothing, no destination had runway, window_ceiling 95 %, since ${at}`);
  assert.equal(words('shed_no_runway', main, { accounts: ['primary'], rounds: 1, setting: ['window_ceiling'] }), `main: 1 round marked nothing, no destination had runway, window_ceiling, since ${at}`, 'a pool the registry lost carries no value');
  assert.equal(words('destination_spent', account, { fits: { fits: 1, rewrite_only: 1 }, moves: 2, setting: ['window_ceiling'], windows: ['five_hour'] }), `primary: 2 moves landed and the 5h window was spent within the hour, window_ceiling, since ${at}`);
  assert.equal(words('destination_spent', account, { fits: { least_bad: 1 }, moves: 1, setting: ['window_ceiling'], windows: ['five_hour', 'seven_day'] }), `primary: 1 move landed and the 5h and 7d windows were spent within the hour, window_ceiling, since ${at}`);
  assert.equal(words('wall_reached', account, { projection_error: 0.04, setting: ['shed_cap', 'window_ceiling'], walls: 1, windows: ['five_hour'] }), `primary: 1 wall reached on the 5h window, the last round projected 96 %, shed_cap and window_ceiling, since ${at}`);
  assert.equal(words('wall_reached', account, { projection_error: null, setting: ['shed_cap', 'window_ceiling'], walls: 2, windows: ['seven_day', 'seven_day_fable'] }), `primary: 2 walls reached on the 7d and Fable windows, shed_cap and window_ceiling, since ${at}`, 'a wall no round projected names no error');
  assert.equal(attentionWords(item('pool_exhausted', { detail: { pool: 'jobs' } }), NOW), null, 'another kind keeps its label and subject');
  for (const kind of ['wall_reached', 'destination_spent', 'shed_no_runway']) assert.equal(attentionDetail(item(kind, { ...account, detail: {} }), NOW), attentionWords(item(kind, { ...account, detail: {} }), NOW).replace(/^primary: /, ''), `${kind}'s row reads the words without the subject`);
});

test('the pool exhausted title names the day of a condition that began more than a day ago', () => {
  const since = '2026-09-16T10:41:00.000Z';
  const exhausted = item('pool_exhausted', { detail: { pool: 'jobs' }, link: { id: 'jobs', page: 'pools' }, since });
  const day = new Date(since).toLocaleDateString('en-US', { day: 'numeric', month: 'short' });
  assert.match(day, /^[A-Z][a-z]{2} \d{1,2}$/);
  assert.equal(attentionTitle(exhausted, NOW).split('\n').pop(), `Opens the Routing log: pool jobs, exhausted, since ${day} ${fmtShortTime(since)}`);
});

test('the pool exhausted title ends with the sentence that names the log', () => {
  const since = '2026-09-18T10:41:00.000Z';
  const exhausted = item('pool_exhausted', { detail: { pool: 'jobs', key: 'ci' }, link: { id: 'jobs', page: 'pools' }, pools: ['jobs'], since });
  assert.match(fmtShortTime(since), /^\d{2}:\d{2}$/, 'the condition starts at a clock time');
  assert.equal(attentionTitle(exhausted, NOW), ['key: ci', 'pool: jobs', 'started 1h 19m ago', `Opens the Routing log: pool jobs, exhausted, since ${fmtShortTime(since)}`].join('\n'));
  assert.equal(attentionTitle({ ...exhausted, link: null }, NOW).split('\n').pop(), 'started 1h 19m ago', 'an item that opens nothing names no log');
  assert.equal(attentionTitle(item('ledger_record_parked', { detail: { count: 1 }, link: { id: 'jobs', page: 'pools' }, since }), NOW).split('\n').pop(), 'started 1h 19m ago', 'another kind ends with its age');
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
