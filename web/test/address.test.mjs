import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

const FROM = '2026-10-10T14:00:00.123Z';
const TO = '2026-10-10T15:59:59.999Z';
const preset = (name) => ({ from: '', preset: name, to: '' });
const CUSTOM = { from: FROM, preset: 'custom', to: TO };
const read = (codec, search) => codec.fromParams(new URLSearchParams(search));
const write = (codec, filters) => codec.toParams(filters).toString();

/**
 * The Accounts, Analytics, Keys, Meter, Requests, Routing log and Sessions codecs over a filled address: every list
 * with two ids, every text set, every flag on, every choice off its default.
 */
const FILLED = {
  ACCOUNTS_CODEC: { dir: 'asc', search: 'git', sort: 'usage', status: 'paused' },
  ANALYTICS_CODEC: { accounts: ['primary', 'secondary'], agentId: 'c21e', by: 'session', clients: ['claude-code/cli', 'claude-code/sdk*'], dir: 'asc', groups: 'claude-cli/2.1.*\nclaude-cli/2.0.*', key: 'desktop@1000', keys: ['desktop', 'laptop'], models: ['claude-opus-5', 'claude-sonnet-5'], pools: ['all', 'jobs'], probes: true, range: preset('7d'), session: '407dd39b-dd60', sort: 'error_rate', unit: 'share', userAgents: ['claude-cli/2.1.3 (external, cli)', 'claude-cli/2.0.*'] },
  KEYS_CODEC: { dir: 'desc', pools: ['all', 'jobs'], search: 'lap', sort: 'expiry', status: 'active' },
  METER_CODEC: { accounts: ['primary', 'secondary'] },
  REQUESTS_CODEC: { accounts: ['primary', 'secondary'], agent: 'claude-cli/*', agentId: 'main', agentTypes: ['main', 'subagent'], cacheMissReasons: ['messages_changed', 'ttl_expired'], classifiers: ['client', '-'], client: 'claude-code/*', dir: 'asc', efforts: ['high', 'low'], fallback: 'true', guardrails: ['large-auxiliary', '-'], held: true, inputTransformations: ['thinking_dropped.end_user_binding_mismatch', 'thinking_dropped.prefix_binding_mismatch'], keys: ['desktop', 'laptop'], kind: 'probe', models: ['claude-opus-5', 'claude-sonnet-5'], outcomes: ['failed', 'succeeded'], pools: ['all', 'jobs'], range: preset('6h'), refusalCategories: ['cyber', '-'], request: 'req_f8085653', requestClasses: ['conversation', 'title'], rows: 50, search: 'req_1d9c3e55', session: '407dd39b-dd60', sort: 'ttfb', speeds: ['fast', 'standard'], statuses: ['200', '429'], stopReasons: ['refusal', '-'], turnOrigins: ['agent', 'user'] },
  ROUTING_LOG_CODEC: { accounts: ['primary', 'secondary'], agentId: 'main', dir: 'asc', keys: ['desktop', 'laptop'], kinds: ['exhausted', 'moved'], models: ['claude-opus-5', 'claude-sonnet-5'], pools: ['all', 'jobs'], range: preset('6h'), request: 'req_1d9c3e55', session: '407dd39b-dd60', sort: 'time' },
  SESSIONS_CODEC: { account: ['primary', 'studio'], active: '1h', agentType: ['review,security', 'main'], by: 'project', cache: ['cold', 'no recipe'], client: ['Claude Code 2.1.310', 'claude-desktop/1.4'], companion: ['reporting', 'silent'], dir: 'asc', effort: ['high', 'low'], finished: true, forked: true, key: ['desktop', 'laptop@3'], model: ['claude-opus-5', 'claude-sonnet-5'], pool: ['all', 'jobs'], released: true, search: '407dd39b', session: '407dd39b-dd60', slowed: true, sort: 'cost', status: ['busy', 'unreported'] },
};

test('the Accounts, Analytics, Keys, Meter, Requests, Routing log and Sessions codecs read back the address they write, the range at a preset and at a custom span', () => {
  for (const [name, filled] of Object.entries(FILLED)) {
    const codec = d[name];
    assert.deepEqual(codec.fromParams(codec.toParams(filled)), filled, name);
    if ('range' in filled) {
      const custom = { ...filled, range: CUSTOM };
      assert.deepEqual(codec.fromParams(codec.toParams(custom)), custom, `${name} under a custom span`);
    }
  }
});

test('the Sessions codec writes the filters off their defaults alone, in the bar order, reads no age, and Reset leaves a bare address', () => {
  const defaults = read(d.SESSIONS_CODEC, '');
  assert.deepEqual(defaults, { account: [], active: '', agentType: [], by: 'none', cache: [], client: [], companion: [], dir: '', effort: [], finished: false, forked: false, key: [], model: [], pool: [], released: false, search: '', session: '', slowed: false, sort: 'created', status: [] });
  assert.equal(write(d.SESSIONS_CODEC, defaults), '');
  assert.deepEqual(read(d.SESSIONS_CODEC, 'age=6h'), defaults, 'an age is a key the codec does not know');
  const set = { ...defaults, companion: ['reporting', 'silent'], released: true };
  assert.equal(write(d.SESSIONS_CODEC, set), 'companion=reporting%2Csilent&released=1');
  assert.deepEqual(read(d.SESSIONS_CODEC, 'companion=reporting,silent&age=6h&released=1'), set);
  assert.equal(write(d.SESSIONS_CODEC, { ...set, dir: 'asc', sort: 'activity' }), 'companion=reporting%2Csilent&released=1&sort=activity&dir=asc', 'the control follows the filters in the bar');
  const reset = Object.fromEntries(d.SESSIONS_DIMENSIONS.map((x) => [x.field, d.emptyValue(x)]));
  assert.equal(write(d.SESSIONS_CODEC, { ...set, finished: true, ...reset }), '');
});

test('the Sessions codec carries the Agents lists under their own keys and a flag as 1', () => {
  const set = { ...read(d.SESSIONS_CODEC, ''), account: ['primary', 'studio'], forked: true, status: ['busy', 'unreported'] };
  const written = write(d.SESSIONS_CODEC, set);
  assert.equal(written, 'forked=1&account=primary%2Cstudio&status=busy%2Cunreported');
  assert.deepEqual(read(d.SESSIONS_CODEC, written), set);
});

test("both logs' codecs and the Analytics codec carry the agent id as agent_id, and agent stays the Requests view's user agent pattern", () => {
  for (const [name, codec] of [['requests', d.REQUESTS_CODEC], ['routing log', d.ROUTING_LOG_CODEC], ['analytics', d.ANALYTICS_CODEC]]) {
    const bare = read(codec, '');
    assert.equal(bare.agentId, '', `${name}: a bare address reads no agent id`);
    assert.equal(write(codec, { ...bare, agentId: 'main' }), 'agent_id=main', name);
    assert.equal(read(codec, 'agent_id=main').agentId, 'main', name);
  }
  const both = read(d.REQUESTS_CODEC, 'agent=claude-cli%2F*&agent_id=main');
  assert.deepEqual([both.agent, both.agentId], ['claude-cli/*', 'main']);
});

test("the Accounts codec carries the sort's direction after its key, written only where it differs from the key's own", () => {
  const bare = read(d.ACCOUNTS_CODEC, '');
  assert.equal(write(d.ACCOUNTS_CODEC, { ...bare, dir: 'asc', sort: 'usage' }), 'sort=usage&dir=asc');
  assert.equal(write(d.ACCOUNTS_CODEC, { ...bare, dir: '', sort: 'usage' }), 'sort=usage', "the key's own direction leaves the address");
  assert.equal(write(d.ACCOUNTS_CODEC, { ...bare, dir: '', sort: 'name' }), '');
  assert.equal(read(d.ACCOUNTS_CODEC, 'sort=usage&dir=bogus').dir, '');
  // A saved `?sort=usage` keeps meaning the most used first.
  assert.equal(d.sortDirOf(d.ACCOUNT_SORT_KEYS, { dir: '', sort: 'usage' }), 'desc');
  assert.equal(d.sortDirOf(d.ACCOUNT_SORT_KEYS, { dir: 'asc', sort: 'usage' }), 'asc');
  assert.deepEqual(d.sortAddressOf(d.ACCOUNT_SORT_KEYS, { dir: 'desc', sort: 'usage' }), { dir: '', sort: 'usage' });
  assert.deepEqual(d.sortAddressOf(d.ACCOUNT_SORT_KEYS, { dir: 'asc', sort: 'usage' }), { dir: 'asc', sort: 'usage' });
});

test('a pick of Default order writes no direction to the Quota address, whatever direction the pick sends', () => {
  assert.equal(write(d.QUOTA_CODEC, d.sortAddressOf(d.QUOTA_SORT_KEYS, { dir: 'desc', sort: 'default' })), '');
});

test('every filter at its default leaves a bare address', () => {
  for (const name of Object.keys(FILLED)) assert.equal(write(d[name], read(d[name], '')), '', name);
});

test('the Routing log codec reads a Kinds list naming every kind as no Kinds filter and writes none for it', () => {
  // An absent `kinds` reads every kind, so the full set and the empty list are one filter with one address.
  assert.deepEqual(read(d.ROUTING_LOG_CODEC, `kinds=${d.ALL_KINDS.join(',')}`).kinds, []);
  assert.equal(write(d.ROUTING_LOG_CODEC, { ...read(d.ROUTING_LOG_CODEC, ''), kinds: [...d.ALL_KINDS] }), '');
});

test('a list splits on commas and joins back, empty entries dropped', () => {
  assert.deepEqual(read(d.ROUTING_LOG_CODEC, 'accounts=primary,,secondary').accounts, ['primary', 'secondary']);
  assert.equal(d.ROUTING_LOG_CODEC.toParams({ ...read(d.ROUTING_LOG_CODEC, ''), pools: ['all', 'jobs'] }).get('pools'), 'all,jobs');
});

test('the Analytics client and user agent patterns ride the address joined by a newline, a comma and a space kept whole', () => {
  const bare = read(d.ANALYTICS_CODEC, '');
  assert.deepEqual([bare.clients, bare.userAgents], [[], []]);
  const agents = ['claude-cli/2.1.3 (external, cli)', 'claude-cli/2.0.*'];
  const written = write(d.ANALYTICS_CODEC, { ...bare, userAgents: agents });
  assert.equal(written, 'user_agents=claude-cli%2F2.1.3+%28external%2C+cli%29%0Aclaude-cli%2F2.0.*');
  assert.deepEqual(read(d.ANALYTICS_CODEC, written).userAgents, agents, 'both patterns read back whole');
  assert.equal(write(d.ANALYTICS_CODEC, { ...bare, clients: ['claude-code/cli'] }), 'clients=claude-code%2Fcli');
  assert.deepEqual(read(d.ANALYTICS_CODEC, 'clients=claude-code%2Fcli%0A%0Aclaude-code%2Fsdk*').clients, ['claude-code/cli', 'claude-code/sdk*'], 'an empty line is dropped');
  assert.equal(write(d.ANALYTICS_CODEC, { ...bare, clients: [], userAgents: [] }), '', 'an empty list writes nothing');
  assert.equal(write(d.ANALYTICS_CODEC, read(d.ANALYTICS_CODEC, 'agent=claude-cli%2F*&class=claude-code%2F*')), '', 'the single pattern keys read nothing');
});

test('a picked value travels as a pattern that matches itself in full, the grammar characters quoted', () => {
  assert.equal(d.escapePattern('claude-cli/2.1.3 (external, cli)'), 'claude-cli/2.1.3 (external, cli)');
  assert.equal(d.escapePattern('a*b?c\\d'), 'a\\*b\\?c\\\\d');
  assert.ok(d.matchesPattern(d.escapePattern('a*b?c\\d'), 'a*b?c\\d'), 'the escaped value matches the value itself');
  assert.ok(!d.matchesPattern(d.escapePattern('a*b'), 'axxb'), 'and a star in it matches a star alone');
});

test('a preset travels by its name, nothing for the default, and a custom span as its two instants after every filter', () => {
  assert.equal(write(d.ROUTING_LOG_CODEC, { ...read(d.ROUTING_LOG_CODEC, ''), range: preset('6h') }), 'range=6h');
  assert.equal(write(d.ROUTING_LOG_CODEC, { ...read(d.ROUTING_LOG_CODEC, ''), range: preset('1h') }), '');
  assert.equal(write(d.REQUESTS_CODEC, { ...read(d.REQUESTS_CODEC, ''), range: preset('24h') }), '');
  const params = d.ROUTING_LOG_CODEC.toParams({ ...FILLED.ROUTING_LOG_CODEC, range: CUSTOM });
  assert.deepEqual([...params.keys()], ['accounts', 'agent_id', 'keys', 'kinds', 'models', 'pools', 'request', 'session', 'dir', 'from', 'to']);
  assert.deepEqual([params.get('from'), params.get('to'), params.get('range')], [FROM, TO, null]);
});

test('the Requests codec carries a request and a session after the search, each exact and absent at its default', () => {
  const filters = read(d.REQUESTS_CODEC, 'request=req_1&session=s1');
  assert.deepEqual([filters.request, filters.session], ['req_1', 's1']);
  assert.deepEqual(read(d.REQUESTS_CODEC, write(d.REQUESTS_CODEC, filters)), filters);
  assert.deepEqual([...d.REQUESTS_CODEC.toParams({ ...filters, client: 'claude-code/*', search: 'claude' }).keys()], ['search', 'request', 'session', 'client']);
  const bare = read(d.REQUESTS_CODEC, '');
  assert.deepEqual([bare.request, bare.session], ['', '']);
  assert.equal(write(d.REQUESTS_CODEC, bare), '');
});

test('the Accounts codec round-trips its search, sort and status and writes nothing at the defaults', () => {
  const filters = read(d.ACCOUNTS_CODEC, 'search=git&sort=usage&status=paused');
  assert.deepEqual(filters, { dir: '', search: 'git', sort: 'usage', status: 'paused' });
  assert.equal(write(d.ACCOUNTS_CODEC, filters), 'search=git&sort=usage&status=paused');
  assert.deepEqual(read(d.ACCOUNTS_CODEC, ''), { dir: '', search: '', sort: 'name', status: 'all' });
  assert.equal(write(d.ACCOUNTS_CODEC, read(d.ACCOUNTS_CODEC, '')), '');
  assert.deepEqual(read(d.ACCOUNTS_CODEC, 'sort=newest&status=disabled'), { dir: '', search: '', sort: 'name', status: 'all' });
});

test('from and to win over range while both parse, and the next write drops range', () => {
  const filters = read(d.ROUTING_LOG_CODEC, `range=3h&from=${FROM}&to=${TO}`);
  assert.deepEqual(filters.range, CUSTOM);
  assert.equal(write(d.ROUTING_LOG_CODEC, filters), new URLSearchParams({ from: FROM, to: TO }).toString());
});

test('a lone from, an unknown preset, an epoch from without to and a pair whose from is not before to read as the default', () => {
  for (const search of [`from=${FROM}`, `to=${TO}`, 'range=2h', `from=${Date.parse(FROM)}`, `from=${TO}&to=${FROM}`, `from=${FROM}&to=${FROM}`, `from=nonsense&to=${TO}`]) {
    assert.deepEqual(read(d.ROUTING_LOG_CODEC, search).range, preset('1h'), search);
    assert.deepEqual(read(d.REQUESTS_CODEC, search).range, preset('24h'), search);
  }
  // A backward pair beside a preset reads as that preset.
  assert.deepEqual(read(d.ANALYTICS_CODEC, `range=7d&from=${TO}&to=${FROM}`).range, preset('7d'));
});

test('a custom pair whose to lies past now reads with to at now, and a pair wholly after now reads as the preset', () => {
  const now = Date.parse('2026-10-10T15:00:00Z');
  const at = (ms) => new Date(ms).toISOString();
  const rangeOf = (search) => d.ROUTING_LOG_CODEC.fromParams(new URLSearchParams(search), now).range;
  assert.deepEqual(rangeOf(`from=${FROM}&to=${at(now + 3_600_000)}`), { from: FROM, preset: 'custom', to: at(now) });
  assert.deepEqual(rangeOf(`from=${FROM}&to=${at(now)}`), { from: FROM, preset: 'custom', to: at(now) }, 'an end at now stands');
  assert.deepEqual(rangeOf(`range=6h&from=${at(now + 3_600_000)}&to=${at(now + 7_200_000)}`), preset('6h'));
});

test('an epoch-millisecond pair reads as the custom span and writes back as the ISO pair', () => {
  const filters = read(d.ROUTING_LOG_CODEC, `from=${Date.parse(FROM)}&to=${Date.parse(TO)}`);
  assert.deepEqual(filters.range, CUSTOM);
  assert.equal(write(d.ROUTING_LOG_CODEC, filters), new URLSearchParams({ from: FROM, to: TO }).toString());
});

test('an integer past the dates a Date holds reads as empty without a throw', () => {
  assert.equal(d.instantOf('99999999999999999999'), '');
  assert.equal(d.instantOf(String(Date.parse(FROM))), FROM);
  assert.equal(d.instantOf('2026-10-10T14:00:00Z'), '2026-10-10T14:00:00.000Z');
  assert.equal(d.instantOf(null), '');
  assert.deepEqual(read(d.REQUESTS_CODEC, `from=99999999999999999999&to=${TO}`).range, preset('24h'));
});

test('a value outside a choice reads as the default', () => {
  assert.equal(read(d.KEYS_CODEC, 'status=lost').status, 'all');
  assert.equal(read(d.REQUESTS_CODEC, 'kind=bogus').kind, '');
  assert.equal(read(d.REQUESTS_CODEC, 'rows=7').rows, 25);
  assert.equal(read(d.ANALYTICS_CODEC, 'by=all').by, 'key');
  assert.equal(read(d.ANALYTICS_CODEC, 'by=session').by, 'session', "the group-by takes the Burn section's own session grouping");
  assert.equal(read(d.ANALYTICS_CODEC, 'burn=session').by, 'key', 'and no grouping of its own travels for the Burn section');
});

test('an unknown key is absent after a round trip', () => {
  for (const name of Object.keys(FILLED)) assert.equal(write(d[name], read(d[name], 'nonsense=1')), '', name);
  assert.equal(write(d.KEYS_CODEC, read(d.KEYS_CODEC, 'pool=jobs&search=lap')), 'search=lap');
});

test('addressWindow reads a preset from now and a custom span as its own instants', () => {
  const now = Date.parse('2026-10-10T16:30:00Z');
  assert.deepEqual(d.addressWindow(read(d.ROUTING_LOG_CODEC, 'range=6h').range, '1h', now), { from: now - 6 * 3_600_000, to: now });
  assert.deepEqual(d.addressWindow(read(d.ROUTING_LOG_CODEC, '').range, '1h', now), { from: now - 3_600_000, to: now });
  assert.deepEqual(d.addressWindow(CUSTOM, '1h', now), { from: Date.parse(FROM), to: Date.parse(TO) });
});

test('each codec reads its default preset, and a name the table does not hold reads as that default', () => {
  assert.equal(read(d.REQUESTS_CODEC, '').range.preset, '24h');
  assert.equal(read(d.ROUTING_LOG_CODEC, '').range.preset, '1h');
  assert.equal(read(d.ANALYTICS_CODEC, '').range.preset, '24h');
  // A name the table does not hold reads as the view's default.
  assert.equal(read(d.ROUTING_LOG_CODEC, 'range=3h').range.preset, '1h');
});

test("an account's link encodes its id, so a reserved character stays inside the one path segment", () => {
  assert.equal(d.accountHref('a b/c'), '/accounts/a%20b%2Fc');
});

test("a request's link encodes its id, so a reserved character stays inside the one path segment", () => {
  assert.equal(d.requestHref('req a/b?c'), '/requests/req%20a%2Fb%3Fc');
});
