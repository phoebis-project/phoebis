import assert from 'node:assert/strict';
import { test } from 'node:test';
import { ATTENTION_LABEL, ATTENTION_REFRESH_MS, ATTENTION_TONE, attentionDetail, attentionHref, attentionLogLine, attentionTitle, attentionWords, createAttentionPoll, decodeAttention, fmtAbsolute, fmtShortTime } from './dist/api.js';

const NOW = Date.parse('2026-09-18T12:00:00.000Z');
const item = (kind, over = {}) => ({ detail: {}, kind, link: null, pools: [], since: '2026-09-18T10:00:00.000Z', subject: { id: kind, label: kind, type: 'account' }, ...over });
/** Lets every settled promise run its handlers, which the mocked interval leaves to the real event loop. */
const flush = () => new Promise((resolve) => setImmediate(resolve));

test("a listener's two certificate kinds decode as themselves, never as the worst kind", () => {
  const router = { link: null, pools: [], since: 1_000_000, subject: { id: 'proxy.tls', label: 'proxy.tls', type: 'router' } };
  const kinds = ['certificate_expiring', 'certificate_reload_failed'].map((kind) => decodeAttention({ ...router, detail: {}, kind }).kind);
  assert.deepEqual(kinds, ['certificate_expiring', 'certificate_reload_failed']);
});

test('an item opens the page where its condition is fixed, and an item without a link opens nothing', () => {
  assert.equal(attentionHref(item('reauth_required', { link: { id: 'legacy', page: 'accounts' } }), NOW), '/accounts/legacy');
  assert.equal(attentionHref(item('client_drift', { link: { id: 'laptop', page: 'keys' } }), NOW), '/keys#laptop');
  assert.equal(attentionHref(item('client_drift', { link: { id: 'a b#c', page: 'keys' } }), NOW), '/keys#a%20b%23c', 'an id travels encoded');
  assert.equal(attentionHref(item('client_drift'), NOW), null, 'a drift refusal that carried no key has nothing to open');
  assert.equal(attentionHref(item('meter_drift', { link: { id: 'nightly', page: 'accounts' } }), NOW), '/meter?accounts=nightly', 'a meter drift opens the Meter view narrowed to its account');
  assert.equal(attentionHref(item('writer_degraded', { subject: { id: 'routing', label: 'routing', type: 'router' } }), NOW), null, "the router's own component has nothing to open");
  // A model's weight shows in the cost reference dialog, which no address opens, so the item has nothing to open.
  const unpriced = { detail: { model: 'claude-sonnet-4-5', requests: 12 }, kind: 'model_unpriced', pools: [], since: NOW, subject: { id: 'claude-sonnet-4-5', label: 'claude-sonnet-4-5', type: 'model' } };
  for (const link of [{ id: 'claude-sonnet-4-5', page: 'models' }, null]) {
    const model = decodeAttention({ ...unpriced, link });
    const drawn = [model.link, attentionHref(model, NOW), attentionWords(model, NOW) ?? `${ATTENTION_LABEL[model.kind]}: ${model.subject.label}`, attentionTitle(model, NOW).split('\n')[0]];
    assert.deepEqual(drawn, [null, null, 'Model unpriced: claude-sonnet-4-5', attentionDetail(model, NOW)], `an unpriced model's item ${link === null ? 'with no link' : 'naming the models page'} opens nothing and draws its label, its detail on the title`);
  }
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
});

test("the keepalive engine's three conditions read their detail as the dashboard line's sentence", () => {
  const keepalive = (kind, id, detail) => item(kind, { detail, subject: { id, label: 'Keepalive', type: 'router' } });
  assert.equal(attentionWords(keepalive('keepalive_bus_gaps', 'keepalive', { gaps: 3 }), NOW), 'Keepalive bus gaps: 3 since the start');
  assert.equal(attentionWords(keepalive('keepalive_cap', 'keepalive.retained', { cap: 'retained', pct: 0.92 }), NOW), 'Keepalive cap at 92%: retained prompts');
  assert.equal(attentionWords(keepalive('expired_between_turns', 'keepalive', { expired: 4 }), NOW), 'Expired between turns: 4 in 24h');
});

test('a keepalive condition opens the Sessions view, though its link names no page', () => {
  assert.equal(attentionHref(item('keepalive_cap', { subject: { id: 'keepalive.retained', label: 'Keepalive', type: 'router' } }), NOW), '/sessions');
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
  const since = fmtAbsolute(item('model_unpriced').since);
  assert.equal(sentence('model_unpriced', { model: 'claude-sonnet-4-5', requests: 12 }), `claude-sonnet-4-5 has no weight in the cost model, so its 12 plan-funded requests since ${since} priced no window share.`);
  assert.equal(sentence('model_unpriced', { model: 'claude-opus-99', requests: 1 }), `claude-opus-99 has no weight in the cost model, so its 1 plan-funded request since ${since} priced no window share.`, 'one request reads singular');
  const slowed = { bar_ms: 3_600_000, factor_min: 0.35, open: true, setting: ['slow_down_stretch_seconds'], setting_values: { slow_down_stretch_seconds: 1800 }, slowed_ms: 7_700_000, spans: 2 };
  assert.equal(sentence('slowed_long', slowed), `slowed 2 h 8 min of the last 24 h, over 1 h, slowest 0.35x, slow_down_stretch_seconds 1800, since ${fmtShortTime(item('slowed_long').since)}`);
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
  const model = item('model_unpriced', { detail: { model: 'claude-sonnet-4-5', requests: 12 }, subject: { id: 'claude-sonnet-4-5', label: 'claude-sonnet-4-5', type: 'model' } });
  assert.equal(attentionTitle(model, NOW).split('\n')[0], attentionDetail(model, NOW), "a model's item belongs to no pool card, so its title leads with its sentence");
  const pooled = item('writer_degraded', { detail: { component: 'ledger' }, pools: ['all'] });
  assert.equal(attentionTitle(pooled, NOW), ['component: ledger', 'started 2h ago'].join('\n'), "an item a pool carries reads its sentence on that pool's card");
});

test('a kind this console does not know draws no link and never a pool log', () => {
  // The route sends the start in milliseconds.
  const since = Date.parse('2026-09-18T10:00:00.000Z');
  const later = decodeAttention({ detail: { account: 'legacy' }, kind: 'quota_drift', link: { id: 'legacy', page: 'accounts' }, pools: [], since, subject: { id: 'legacy', label: 'legacy', type: 'account' } });
  assert.deepEqual([later.kind, later.rawKind], ['unknown', 'quota_drift'], 'the decoder reads an unknown kind as unknown, never as a kind it knows');
  assert.equal(attentionHref(later, NOW), null);
  assert.equal(attentionTitle(later, NOW).split('\n').pop(), 'started 2h ago', 'and its title names no log');
  const pooled = decodeAttention({ detail: {}, kind: 'quota_drift', link: { id: 'jobs', page: 'pools' }, pools: ['jobs'], since, subject: { id: 'jobs', label: 'jobs', type: 'pool' } });
  assert.deepEqual([attentionHref(pooled, NOW), attentionLogLine(pooled, NOW)], [null, null], 'one that names a pool opens no log of it either');
  const known = ['wall_reached', 'destination_spent', 'shed_no_runway', 'slowed_long'].map((kind) => decodeAttention({ detail: {}, kind, link: null, pools: [], since, subject: { id: 'x', label: 'x', type: 'account' } }).kind);
  assert.deepEqual(known, ['wall_reached', 'destination_spent', 'shed_no_runway', 'slowed_long'], 'the four routing conditions decode as themselves');
});

test('an unknown condition reads its label in the warning tone, names its raw kind on hover and opens nothing, whatever its link', () => {
  assert.deepEqual([ATTENTION_LABEL.unknown, ATTENTION_TONE.unknown], ['Unknown condition', 'warning']);
  const lone = item('unknown', { link: { id: 'legacy', page: 'accounts' }, rawKind: 'later_kind' });
  assert.equal(attentionWords(lone, NOW), null, 'the badge reads its label and subject');
  assert.deepEqual(attentionTitle(lone, NOW).split('\n'), ['kind: later_kind', 'a condition this console does not know (later_kind)', 'started 2h ago']);
  const pooled = item('unknown', { detail: { pool: 'jobs' }, link: { id: 'jobs', page: 'pools' }, pools: ['jobs'], rawKind: 'later_kind' });
  assert.deepEqual(attentionTitle(pooled, NOW).split('\n'), ['kind: later_kind', 'pool: jobs', 'started 2h ago'], "a pooled item's title names the raw kind too, the sentence left to the pool card's row");
  assert.equal(attentionDetail(pooled, NOW), 'a condition this console does not know (later_kind)');
  assert.deepEqual([lone, pooled].map((it) => [attentionHref(it, NOW), attentionLogLine(it, NOW)]), [[null, null], [null, null]]);
});

test("the four routing conditions open the routing log on their subject's moves, walls, rounds and slow-downs since the condition began", () => {
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
  assert.deepEqual(query(item('slowed_long', account)), ['/routing-log', { ...window, accounts: 'primary', kinds: 'slow_down_ended,slow_down_started' }]);
  assert.equal(attentionHref(item('wall_reached', { ...account, link: null }), NOW), null, 'an account the registry no longer holds opens nothing');
  const at = fmtShortTime(since);
  assert.deepEqual(
    [attentionLogLine(item('shed_no_runway', pool), NOW), attentionLogLine(item('destination_spent', account), NOW), attentionLogLine(item('wall_reached', account), NOW), attentionLogLine(item('slowed_long', account), NOW)],
    [`Opens the Routing log: pool all, shedding rounds, since ${at}`, `Opens the Routing log: account primary, moves and spent windows, since ${at}`, `Opens the Routing log: account primary, spent windows, since ${at}`, `Opens the Routing log: account primary, slow-downs, since ${at}`],
  );
  assert.equal(attentionTitle(item('shed_no_runway', pool), NOW).split('\n').pop(), `Opens the Routing log: pool all, shedding rounds, since ${at}`, 'the title ends with what the log opens on');
});

test('the four routing conditions read the count, the setting and the start on the dashboard line and the pool card', () => {
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
  // After a slow-down round the wall reads how long before its reset it came, since that round's projection measures
  // nothing there.
  const early = { early_ms: 1_500_000, projection_error: null, setting: ['shed_cap', 'window_ceiling'], walls: 1, windows: ['five_hour'] };
  assert.equal(words('wall_reached', account, early), `primary: 1 wall reached on the 5h window, the wall came 25 min before the reset, shed_cap and window_ceiling, since ${at}`);
  assert.equal(words('wall_reached', account, { ...early, early_ms: 40_000 }), `primary: 1 wall reached on the 5h window, the wall came 40 s before the reset, shed_cap and window_ceiling, since ${at}`, 'a span under a minute');
  assert.equal(words('wall_reached', account, { ...early, early_ms: 0 }), `primary: 1 wall reached on the 5h window, the wall came at the reset, shed_cap and window_ceiling, since ${at}`, 'a wall at or after the reset');
  assert.equal(words('wall_reached', account, { ...early, early_ms: null }), `primary: 1 wall reached on the 5h window, shed_cap and window_ceiling, since ${at}`, 'neither figure where both are null');
  const studio = { ...account, link: { id: 'studio', page: 'accounts' }, pools: ['work'], subject: { id: 'studio', label: 'studio', type: 'account' } };
  const slowed = { bar_ms: 3_600_000, factor_min: 0.35, open: true, setting: ['slow_down_stretch_seconds'], setting_values: { slow_down_stretch_seconds: 1800 }, slowed_ms: 7_700_000, spans: 2 };
  assert.equal(words('slowed_long', studio, slowed), `studio: slowed 2 h 8 min of the last 24 h, over 1 h, slowest 0.35x, slow_down_stretch_seconds 1800, since ${at}`);
  assert.equal(words('slowed_long', studio, { ...slowed, factor_min: 0.5, open: false, slowed_ms: 3_720_000, spans: 1 }), `studio: slowed 1 h 2 min of the last 24 h, over 1 h, slowest 0.5x, slow_down_stretch_seconds 1800, since ${at}`, 'the factor drops a trailing zero');
  assert.equal(
    words('slowed_long', studio, { ...slowed, bar_ms: 18_000_000, setting_values: { slow_down_stretch_seconds: 9000 }, slowed_ms: 19_500_000 }),
    `studio: slowed 5 h 25 min of the last 24 h, over 5 h, slowest 0.35x, slow_down_stretch_seconds 9000, since ${at}`,
    'the bar reads in the same span words as the day',
  );
  assert.equal(words('slowed_long', studio, { ...slowed, factor_min: null, setting_values: undefined }), `studio: slowed 2 h 8 min of the last 24 h, over 1 h, slow_down_stretch_seconds, since ${at}`, 'a span with no factor read and a pool the registry lost name neither');
  assert.equal(words('slowed_long', studio, { ...slowed, bar_ms: null }), `studio: slowed 2 h 8 min of the last 24 h, slowest 0.35x, slow_down_stretch_seconds 1800, since ${at}`, 'a detail without a bar names none');
  assert.equal(attentionWords(item('pool_exhausted', { detail: { pool: 'jobs' } }), NOW), null, 'another kind keeps its label and subject');
  for (const kind of ['wall_reached', 'destination_spent', 'shed_no_runway', 'slowed_long']) assert.equal(attentionDetail(item(kind, { ...account, detail: {} }), NOW), attentionWords(item(kind, { ...account, detail: {} }), NOW).replace(/^primary: /, ''), `${kind}'s row reads the words without the subject`);
});

test('a meter drift item words its off windows and opens the Meter view on the account', () => {
  // A local clock time, so the words read 14:20 in any zone, and it lies within a day of NOW there.
  const since = new Date(2026, 8, 18, 14, 20).toISOString();
  const account = { link: { id: 'acct-a', page: 'accounts' }, pools: ['main'], since, subject: { id: 'acct-a', label: 'acct-a', type: 'account' } };
  const windows = [
    { observed_rise: 0.3, predicted_share: 0.15, ratio: 2.0, requests: 40, window: 'five_hour', window_end: Date.parse(since) },
    { observed_rise: 0.26, predicted_share: 0.2, ratio: 1.3, requests: 120, window: 'seven_day', window_end: Date.parse(since) + 3_600_000 },
  ];
  const drift = item('meter_drift', { ...account, detail: { effective: '2026-09-01', source: 'shipped', windows } });
  assert.equal(attentionWords(drift, NOW), 'acct-a: 5h charged 2.0x the table, 7d 1.3x, since 14:20', 'the first window carries the verb and the table, each later one its word and ratio');
  assert.equal(attentionDetail(drift, NOW), '5h charged 2.0x the table, 7d 1.3x, since 14:20', "the pool card's row reads the words without the subject");
  const fable = item('meter_drift', { ...account, detail: { effective: '2026-09-01', source: 'file', windows: [{ observed_rise: 0.12, predicted_share: 0.18, ratio: 0.7, requests: 9, window: 'seven_day_fable', window_end: Date.parse(since) }] } });
  assert.equal(attentionWords(fable, NOW), 'acct-a: Fable charged 0.7x the table, since 14:20', 'a ratio drops its one trailing zero');
  assert.equal(ATTENTION_TONE.meter_drift, 'warning');
  const url = new URL(attentionHref(drift, NOW), 'http://console.test');
  assert.deepEqual([url.pathname, Object.fromEntries(url.searchParams), url.hash], ['/meter', { accounts: 'acct-a' }, ''], 'the Meter view on the account');
  assert.equal(attentionLogLine(drift, NOW), null, 'the item opens no routing log');
});

test("a meter drift badge's title dates each window's end and reads the windows' other values as the route sends them", () => {
  const since = '2026-09-18T10:41:00.000Z';
  const end = Date.parse(since);
  const windows = [
    { observed_rise: 0.3, predicted_share: 0.15, ratio: 2.0, requests: 40, window: 'five_hour', window_end: end },
    { observed_rise: 0.26, predicted_share: 0.2, ratio: 1.3, requests: 120, window: 'seven_day', window_end: end + 3_600_000 },
  ];
  const drift = item('meter_drift', { detail: { effective: '2026-09-01', source: 'shipped', windows }, link: { id: 'acct-a', page: 'accounts' }, pools: ['main'], since, subject: { id: 'acct-a', label: 'acct-a', type: 'account' } });
  // The ends read in the viewer's zone, so the case reads them through the console's own format.
  const date = (ms) => JSON.stringify(fmtAbsolute(new Date(ms).toISOString()));
  assert.deepEqual(attentionTitle(drift, NOW).split('\n'), [
    'effective: 2026-09-01',
    'source: shipped',
    `windows: [{"observed_rise":0.3,"predicted_share":0.15,"ratio":2,"requests":40,"window":"five_hour","window_end":${date(end)}},{"observed_rise":0.26,"predicted_share":0.2,"ratio":1.3,"requests":120,"window":"seven_day","window_end":${date(end + 3_600_000)}}]`,
    'started 1h 19m ago',
  ]);
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

test('a read held across a tick lands, and the tick after it reads again', async (t) => {
  t.mock.timers.enable({ apis: ['setInterval'] });
  const pending = [];
  const poll = createAttentionPoll(() => new Promise((resolve) => pending.push(resolve)));
  const off = poll.subscribe(() => {});
  t.mock.timers.tick(ATTENTION_REFRESH_MS);
  assert.equal(pending.length, 1, 'the tick finds the first read still out and reads nothing');
  pending[0]([item('pool_exhausted')]);
  await flush();
  assert.deepEqual(poll.snapshot().items.map((i) => i.kind), ['pool_exhausted'], 'the held read lands');
  t.mock.timers.tick(ATTENTION_REFRESH_MS);
  assert.equal(pending.length, 2, 'the tick after it reads again');
  off();
});

test("a write's refresh reads beside a read still out, and the older answer that lands after it leaves the newer one standing", async (t) => {
  t.mock.timers.enable({ apis: ['setInterval'] });
  const pending = [];
  const poll = createAttentionPoll(() => new Promise((resolve) => pending.push(resolve)));
  const off = poll.subscribe(() => {});
  void poll.refresh();
  assert.equal(pending.length, 2, 'the refresh reads while the first read is still out');
  pending[1]([]);
  await flush();
  pending[0]([item('pool_exhausted')]);
  await flush();
  assert.deepEqual(poll.snapshot().items, []);
  off();
});

// Last in the file, since the DOM globals it sets for React stay set for the rest of the process.
test('two conditions this console does not know on one subject draw a pool card row each, under keys of their own', async (t) => {
  // React reads whether a DOM exists when its module loads, so the window stands before the bundle that draws is
  // imported.
  const { JSDOM } = await import('jsdom');
  const { window } = new JSDOM('<!doctype html><html><body></body></html>');
  for (const key of ['document', 'navigator', 'HTMLElement', 'Element', 'Node', 'Event']) {
    Object.defineProperty(globalThis, key, { value: window[key], configurable: true, writable: true });
  }
  globalThis.window = window;
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
  t.after(() => window.close());
  const { act, AttentionRows, createElement: h, createRoot, MemoryRouter } = await import('./dist/ui.js');
  const since = Date.parse('2026-09-18T10:00:00.000Z');
  const onPool = { detail: {}, link: { id: 'jobs', page: 'pools' }, pools: ['jobs'], since, subject: { id: 'jobs', label: 'jobs', type: 'pool' } };
  const items = ['quota_drift', 'later_kind'].map((kind) => decodeAttention({ ...onPool, kind }));
  assert.deepEqual(items.map((it) => it.kind), ['unknown', 'unknown'], 'both decode to the one unknown kind');
  const warnings = [];
  const error = console.error;
  console.error = (...args) => warnings.push(args.map(String).join(' '));
  const container = document.body.appendChild(document.createElement('div'));
  const root = createRoot(container);
  try {
    act(() => root.render(h(MemoryRouter, null, h(AttentionRows, { items, now: NOW }))));
  } finally {
    console.error = error;
  }
  const rows = container.querySelectorAll('.attention__row').length;
  act(() => root.unmount());
  assert.equal(rows, 2);
  assert.deepEqual(
    warnings.filter((w) => w.includes('same key')),
    [],
    'React warns of no two rows under one key',
  );
});
