import assert from 'node:assert/strict';
import { test } from 'node:test';

// Every clock time below is read in UTC whatever the host's zone. `node --test` gives each file a process of its own,
// so the setting reaches no other file.
process.env.TZ = 'UTC';

const d = await import('./dist/api.js');

const SECOND = 1_000;
const MINUTE = 60 * SECOND;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;
const NOW = Date.UTC(2026, 8, 16, 14, 30);
const ms = (h, m) => Date.UTC(2026, 8, 16, h, m);
const iso = (t) => new Date(t).toISOString();

/** A recipe as the decoder answers it, with the three fields the order reads set per row. */
const recipe = (agentId, agentState, lastRequestAt) => ({
  accountId: 'primary',
  agentId,
  agentState,
  cause: 'scheduled',
  creditedToday: 0,
  deadline: null,
  keyId: 'desktop',
  lastGuard: 'rearm',
  lastProbeAt: null,
  lastRequestAt: lastRequestAt === null ? null : iso(lastRequestAt),
  model: 'claude-sonnet-5',
  nextProbeAt: null,
  parkKind: null,
  poolId: 'all',
  probesToday: 0,
  sessionId: 'session-1',
  state: 'retained',
  ttlSeconds: 300,
  wakeAt: null,
});
const words = (list) => list.map((r) => (r.agentId === 'main' ? 'main' : r.agentState));
const ids = (list) => list.map((r) => r.agentId);

test('orderRecipes puts main first wherever it sat, then the words busy, idle, parked with unknown, released', () => {
  const at = ms(14, 20);
  const list = [recipe('3e', 'released', at), recipe('u7', 'unknown', at), recipe('b4', 'busy', at), recipe('main', 'idle', ms(13, 0)), recipe('p2', 'parked', at), recipe('i9', 'idle', at)];
  const sorted = d.orderRecipes(list);
  assert.deepEqual(words(sorted), ['main', 'busy', 'idle', 'parked', 'unknown', 'released']);
  assert.deepEqual(ids(sorted), ['main', 'b4', 'i9', 'p2', 'u7', '3e']);
});

test('orderRecipes reads the newest last request first inside a word, an agent without one last, and ties by agent id', () => {
  const list = [recipe('0a', 'busy', null), recipe('e9', 'idle', ms(14, 10)), recipe('c3', 'busy', ms(14, 20)), recipe('4d', 'idle', ms(14, 10)), recipe('7f', 'busy', ms(14, 29))];
  const sorted = d.orderRecipes(list);
  assert.deepEqual(ids(sorted), ['7f', 'c3', '0a', '4d', 'e9']);
  assert.deepEqual(ids(d.orderRecipes(sorted)), ids(sorted));
});

test('orderRecipes leaves the list it was handed as it was', () => {
  const list = [recipe('3e', 'released', ms(14, 20)), recipe('b4', 'busy', ms(14, 0)), recipe('main', 'idle', ms(13, 0))];
  const before = ids(list);
  const sorted = d.orderRecipes(list);
  assert.deepEqual(ids(list), before);
  assert.notDeepEqual(ids(sorted), before);
});

test('requestsRange picks the narrowest range that covers the last request, with a minute of headroom under each width', () => {
  const cases = [
    [59 * MINUTE, '1h'],
    [59 * MINUTE + SECOND, '6h'],
    [5 * HOUR + 59 * MINUTE, '6h'],
    [5 * HOUR + 59 * MINUTE + SECOND, '24h'],
    [23 * HOUR + 59 * MINUTE, '24h'],
    [23 * HOUR + 59 * MINUTE + SECOND, '7d'],
    [6 * DAY + 23 * HOUR + 59 * MINUTE, '7d'],
    [6 * DAY + 23 * HOUR + 59 * MINUTE + SECOND, '30d'],
    [29 * DAY + 23 * HOUR + 59 * MINUTE, '30d'],
    [30 * DAY, null],
  ];
  assert.deepEqual(
    cases.map(([age]) => d.requestsRange(iso(NOW - age), NOW)),
    cases.map(([, range]) => range),
  );
  assert.equal(d.requestsRange(null, NOW), null);
  // The widths are the Requests page's own presets, read from the one table the address grammar holds.
  assert.deepEqual(d.RANGE_PRESETS.map((r) => r.name), ['1h', '6h', '24h', '7d', '30d']);
});

test('releasedCount counts the lines the view hides and nothing else', () => {
  const at = ms(14, 20);
  const released = (agentId, cause) => ({ ...recipe(agentId, 'released', at), cause });
  const live = [recipe('main', 'busy', at), recipe('b4', 'busy', at), recipe('i9', 'idle', at), recipe('p2', 'parked', at), recipe('u7', 'unknown', at)];
  assert.equal(d.releasedCount(live), 0);
  assert.equal(d.releasedCount([live[0], released('3e', 'agent_released'), live[2], released('91', 'agent_released'), live[3]]), 2);
  assert.equal(d.releasedCount([released('3e', 'agent_released'), released('91', 'agent_released'), released('a0', 'agent_released')]), 3);
  assert.equal(d.releasedCount([released('3e', 'session_ended'), released('91', 'released')]), 0);
  assert.deepEqual(
    [released('3e', 'agent_released'), released('3e', 'session_ended'), released('3e', 'released')].map((r) => d.isReleasedLine(r)),
    [true, false, false],
  );
});
