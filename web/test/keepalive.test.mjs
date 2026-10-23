import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
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

test('isFinished reads a session whose main ended with it or was released, or whose every line the mod released', () => {
  const at = ms(14, 20);
  const line = (agentId, agentState, cause) => ({ ...recipe(agentId, agentState, at), cause });
  const session = (state, cause) => ({ keepalive: { cause, lastGuard: null, nextProbeAt: null, state } });
  assert.equal(d.isFinished(session('idle', 'session_ended'), [line('main', 'idle', 'session_ended'), line('3e', 'released', 'agent_released')]), true);
  assert.equal(d.isFinished(session('released', 'released'), [line('main', 'released', 'released')]), true);
  assert.equal(d.isFinished(session('released', 'agent_released'), [line('main', 'released', 'agent_released'), line('3e', 'released', 'agent_released')]), true);
  assert.equal(d.isFinished(session('busy', 'probing'), [line('main', 'busy', 'probing'), line('3e', 'released', 'agent_released')]), false);
  assert.equal(d.isFinished(session('idle', 'scheduled'), []), false);
});

test('hiddenByAge hides a row no companion reports once its last use is past the preset, and never a companion row', () => {
  const row = (companion, lastUsedAt, lastRequestAt = null) => ({ companion, lastRequestAt: lastRequestAt === null ? null : iso(lastRequestAt), lastUsedAt: lastUsedAt === null ? null : iso(lastUsedAt) });
  for (const companion of ['reporting', 'silent']) assert.equal(d.hiddenByAge(row(companion, NOW - 30 * DAY), '15m', NOW), false, companion);
  const twoHours = row('none', NOW - 2 * HOUR);
  assert.deepEqual(['1h', '6h', 'none'].map((age) => d.hiddenByAge(twoHours, age, NOW)), [true, false, false]);
  // A row the router has no use of its own for dates from its last ledgered request.
  const ledgered = row('none', null, NOW - 2 * HOUR);
  assert.deepEqual(['1h', '6h'].map((age) => d.hiddenByAge(ledgered, age, NOW)), [true, false]);
});

test("contextCell reads the tokens, the share of the model's window and the cached prefix, each compact", () => {
  assert.deepEqual(d.contextCell(184_000, 200_000, 0.92, 171_000), { main: '184K', share: '92% of 200K', prefix: 'prefix 171K' });
  assert.deepEqual(d.contextCell(184_000, null, null, 171_000), { main: '184K', share: null, prefix: 'prefix 171K' }, 'a null window drops the share and reads the tokens alone');
  assert.deepEqual(d.contextCell(18_400, 200_000, 0.092, null), { main: '18.4K', share: '9.2% of 200K', prefix: null }, 'a pin with no entry standing reads no prefix');
  assert.deepEqual(d.contextCell(null, null, null, null), { main: '-', share: null, prefix: null }, 'a session with no pin or no figure reads a dash');
});

/** A companion row as the decoder answers it: the main agent's pin on `model` and the route's entries for the others. */
const companionRow = (model, agentContexts) => ({ agentContexts, agentId: 'main', cachedPrefixTokens: 41_000, companion: 'reporting', contextShare: 0.26, contextWindowTokens: 200_000, lastInputTokens: 52_000, model, pinned: true });
const pinEntry = (agentId, model, lastInputTokens) => ({ agentId, cachedPrefixTokens: null, contextShare: null, contextWindowTokens: 200_000, lastInputTokens, model });

test("an agent line reads the pin of its agent on its recipe's model, and none where the route answers no such pin", () => {
  // The route sorts the entries by agent and then model, so the first entry of the agent is the other model's pin.
  const row = companionRow('claude-sonnet-5-5', [pinEntry('7b2e', 'claude-haiku-4-5', 18_000), pinEntry('7b2e', 'claude-sonnet-5-5', 31_000)]);
  assert.equal(d.lineFigures(row, '7b2e', 'claude-sonnet-5-5')?.lastInputTokens, 31_000, 'the line on the second model reads the second pin');
  assert.equal(d.lineFigures(row, '7b2e', 'claude-haiku-4-5')?.lastInputTokens, 18_000, 'the line on the first model reads the first pin');
  assert.equal(d.lineFigures(row, '7b2e', 'claude-sonnet-5-5[1M]')?.lastInputTokens, 31_000, 'the long-context suffix folds out case-blind');
  assert.equal(d.lineFigures(row, '7b2e', 'claude-opus-5'), null, 'an agent with no pin on the model reads none');
  assert.equal(d.lineFigures(row, 'main', 'claude-sonnet-5-5')?.lastInputTokens, 52_000, "the row's agent on the row's model reads the row");
  assert.equal(d.lineFigures(row, 'main', 'claude-sonnet-5'), null, "the main agent on another model than the row's pin reads none");
});

test("the fixture's companion row reads no figures for a main recipe on another model than its pin", () => {
  const wire = JSON.parse(readFileSync(new URL('./fixtures/sessions.json', import.meta.url), 'utf8')).body.sessions.find((s) => s.session_id === 's1');
  const s1 = d.decodeSession(wire);
  assert.ok(s1.agentId === 'main' && s1.model !== 'claude-sonnet-5' && s1.lastInputTokens !== null, "the row's main pin is on another model and has a turn");
  assert.equal(d.lineFigures(s1, 'main', 'claude-sonnet-5'), null);
  assert.equal(d.lineFigures(s1, 'main', s1.model)?.lastInputTokens, s1.lastInputTokens, "the main recipe on the pin's model reads the row");
});

test('the compacted cause reads its label and the sentence that says no probe goes out until the next main turn', () => {
  assert.equal(d.CAUSE_LABEL.compacted, 'compacted');
  assert.equal(
    d.CAUSE_HINT.compacted,
    'A completed compaction replaced the context this recipe kept warm, so the recipe stopped and no probe is sent. The next main turn starts a recipe for the new context.',
  );
});
