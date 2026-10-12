import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

const FROM = '2026-10-10T14:00:00.123Z';
const TO = '2026-10-10T15:59:59.999Z';
const preset = (name) => ({ from: '', preset: name, to: '' });
const CUSTOM = { from: FROM, preset: 'custom', to: TO };
const read = (codec, search) => codec.fromParams(new URLSearchParams(search));
const write = (codec, filters) => codec.toParams(filters).toString();

/** Each view's codec over a filled address: every list with two ids, every text set, every flag on, every choice off its default. */
const FILLED = {
  ANALYTICS_CODEC: { accounts: ['primary', 'secondary'], agent: 'claude-cli/*', burn: 'session', by: 'account', class: 'claude-code/*', groups: 'claude-cli/2.1.*\nclaude-cli/2.0.*', key: 'desktop@1000', keys: ['desktop', 'laptop'], models: ['claude-opus-5', 'claude-sonnet-5'], pools: ['all', 'jobs'], probes: true, range: preset('7d'), session: '407dd39b-dd60', unit: 'share' },
  KEYS_CODEC: { pools: ['all', 'jobs'], search: 'lap', status: 'active' },
  MODELS_CODEC: { key: 'laptop' },
  POOLS_CODEC: {},
  QUOTA_CODEC: {},
  REQUESTS_CODEC: { accounts: ['primary', 'secondary'], agent: 'claude-cli/*', agentTypes: ['main', 'subagent'], cacheMissReasons: ['messages_changed', 'ttl_expired'], client: 'claude-code/*', efforts: ['high', 'low'], held: true, keys: ['desktop', 'laptop'], kind: 'probe', models: ['claude-opus-5', 'claude-sonnet-5'], outcomes: ['failed', 'succeeded'], pools: ['all', 'jobs'], range: preset('6h'), requestClasses: ['conversation', 'title'], rows: '50', search: 'req_1d9c3e55', speeds: ['fast', 'standard'], statuses: ['200', '429'], turnOrigins: ['agent', 'user'] },
  ROUTING_LOG_CODEC: { accounts: ['primary', 'secondary'], keys: ['desktop', 'laptop'], kinds: ['exhausted', 'moved'], models: ['claude-opus-5', 'claude-sonnet-5'], pools: ['all', 'jobs'], range: preset('6h'), request: 'req_1d9c3e55', session: '407dd39b-dd60' },
  SESSIONS_CODEC: {},
};

test("each view's codec reads back the address it writes, the range at a preset and at a custom span", () => {
  assert.equal(Object.keys(FILLED).length, 8);
  for (const [name, filled] of Object.entries(FILLED)) {
    const codec = d[name];
    assert.deepEqual(codec.fromParams(codec.toParams(filled)), filled, name);
    if ('range' in filled) {
      const custom = { ...filled, range: CUSTOM };
      assert.deepEqual(codec.fromParams(codec.toParams(custom)), custom, `${name} under a custom span`);
    }
  }
});

test('every filter at its default leaves a bare address', () => {
  for (const name of Object.keys(FILLED)) assert.equal(write(d[name], read(d[name], '')), '', name);
});

test('a list splits on commas and joins back, empty entries dropped', () => {
  assert.deepEqual(read(d.ROUTING_LOG_CODEC, 'accounts=primary,,secondary').accounts, ['primary', 'secondary']);
  assert.equal(d.ROUTING_LOG_CODEC.toParams({ ...read(d.ROUTING_LOG_CODEC, ''), pools: ['all', 'jobs'] }).get('pools'), 'all,jobs');
});

test('a preset travels by its name, nothing for the default, and a custom span as its two instants after every filter', () => {
  assert.equal(write(d.ROUTING_LOG_CODEC, { ...read(d.ROUTING_LOG_CODEC, ''), range: preset('6h') }), 'range=6h');
  assert.equal(write(d.ROUTING_LOG_CODEC, { ...read(d.ROUTING_LOG_CODEC, ''), range: preset('1h') }), '');
  assert.equal(write(d.REQUESTS_CODEC, { ...read(d.REQUESTS_CODEC, ''), range: preset('24h') }), '');
  const params = d.ROUTING_LOG_CODEC.toParams({ ...FILLED.ROUTING_LOG_CODEC, range: CUSTOM });
  assert.deepEqual([...params.keys()], ['accounts', 'keys', 'kinds', 'models', 'pools', 'request', 'session', 'from', 'to']);
  assert.deepEqual([params.get('from'), params.get('to'), params.get('range')], [FROM, TO, null]);
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
  assert.equal(read(d.REQUESTS_CODEC, 'rows=7').rows, '25');
  assert.equal(read(d.ANALYTICS_CODEC, 'by=all').by, 'key');
  assert.equal(read(d.ANALYTICS_CODEC, 'unit=euro').unit, 'all');
  assert.equal(read(d.ANALYTICS_CODEC, 'burn=pool').burn, 'account');
});

test('an unknown key is absent after a round trip', () => {
  for (const name of Object.keys(FILLED)) assert.equal(write(d[name], read(d[name], 'nonsense=1')), '', name);
  assert.equal(write(d.KEYS_CODEC, read(d.KEYS_CODEC, 'pool=jobs&search=lap')), 'search=lap');
});

test("addressWindow reads a preset from now and a custom span as its own instants, apart from the quota helper's windowOf", () => {
  const now = Date.parse('2026-10-10T16:30:00Z');
  assert.equal(typeof d.windowOf, 'function');
  assert.deepEqual(d.addressWindow(read(d.ROUTING_LOG_CODEC, 'range=6h').range, d.RANGE_PRESETS, '1h', now), { from: now - 6 * 3_600_000, to: now });
  assert.deepEqual(d.addressWindow(read(d.ROUTING_LOG_CODEC, '').range, d.RANGE_PRESETS, '1h', now), { from: now - 3_600_000, to: now });
  assert.deepEqual(d.addressWindow(CUSTOM, d.RANGE_PRESETS, '1h', now), { from: Date.parse(FROM), to: Date.parse(TO) });
});

test('the one preset table carries the five presets by name, label and width, and each codec keeps its default', () => {
  assert.deepEqual(d.RANGE_PRESETS.map((r) => r.name), ['1h', '6h', '24h', '7d', '30d']);
  assert.deepEqual(d.RANGE_PRESETS[0], { label: 'Last hour', ms: 3_600_000, name: '1h' });
  assert.deepEqual(d.RANGE_PRESETS[4], { label: 'Last 30 days', ms: 2_592_000_000, name: '30d' });
  assert.equal(read(d.REQUESTS_CODEC, '').range.preset, '24h');
  assert.equal(read(d.ROUTING_LOG_CODEC, '').range.preset, '1h');
  assert.equal(read(d.ANALYTICS_CODEC, '').range.preset, '24h');
  // A name the table does not hold reads as the view's default.
  assert.equal(read(d.ROUTING_LOG_CODEC, 'range=3h').range.preset, '1h');
});
