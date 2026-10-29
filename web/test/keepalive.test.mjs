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

test('contextCell reads the tokens compact, and a dash with none or before the first turn', () => {
  assert.deepEqual([d.contextCell(184_000), d.contextCell(18_400)], ['184K', '18.4K']);
  assert.deepEqual([d.contextCell(null), d.contextCell(0)], ['-', '-'], 'a session with no pin or no figure, and a pin before its first turn');
});

/** A pin's entry as the decoder answers it, last used ten minutes before the clock on a five-minute TTL. */
const pinEntry = (agentId, model, lastInputTokens) => ({
  accountId: 'primary',
  agentId,
  cacheTtlMs: 5 * MINUTE,
  createdAt: iso(ms(13, 0)),
  effort: 'high',
  lastInputTokens,
  lastUsedAt: iso(ms(14, 20)),
  model,
  warm: false,
});
/** A row as the decoder answers it: main's pin on `model`, with figures of its own, and the route's entries. */
const sessionRow = (model, agentContexts) => ({ agentContexts, agentId: 'main', companion: 'reporting', lastInputTokens: 52_000, model, pinned: true });

test("an agent line reads the route's entry for its agent on its recipe's model, main's among them, and none where the route lists no such pin", () => {
  const main = pinEntry('main', 'claude-sonnet-5-5', 52_000);
  // The route sorts the entries by agent and then model, main first, so the first entry of 7b2e is the other model's.
  const row = sessionRow('claude-sonnet-5-5', [main, pinEntry('7b2e', 'claude-haiku-4-5', 18_000), pinEntry('7b2e', 'claude-sonnet-5-5', 31_000)]);
  assert.deepEqual(d.lineFigures(row, 'main', 'claude-sonnet-5-5'), main, "main's line reads its entry from the list, not the row");
  assert.equal(d.lineFigures(row, '7b2e', 'claude-sonnet-5-5')?.lastInputTokens, 31_000, 'the line on the second model reads the second pin');
  assert.equal(d.lineFigures(row, '7b2e', 'claude-haiku-4-5')?.lastInputTokens, 18_000, 'the line on the first model reads the first pin');
  assert.equal(d.lineFigures(row, '7b2e', 'claude-sonnet-5-5[1M]')?.lastInputTokens, 31_000, 'the long-context suffix folds out case-blind');
  assert.equal(d.lineFigures(row, '7b2e', 'claude-opus-5'), null, 'an agent with no pin on the model reads none');
  assert.equal(d.lineFigures(row, 'main', 'claude-sonnet-5'), null, 'main on a model it holds no pin on reads none');
  assert.equal(d.lineFigures(row, '4f0c', 'claude-sonnet-5-5'), null, 'an agent the list lacks, a released one among them, reads none');
});

test("the fixture's row reads each agent line through its own entry, and main's through the list alone", () => {
  const wire = JSON.parse(readFileSync(new URL('./fixtures/sessions.json', import.meta.url), 'utf8')).body.sessions.find((s) => s.session_id === 's1');
  const s1 = d.decodeSession(wire);
  assert.ok(wire.agent_contexts.length > 0 && s1.agentId === 'main' && s1.lastInputTokens !== null, 'the row carries agent lines and its main pin has a turn');
  for (const c of wire.agent_contexts) assert.equal(d.lineFigures(s1, c.agent_id, c.model)?.lastInputTokens, c.last_input_tokens, `${c.agent_id} on ${c.model}`);
  // Main's line reads main's entry where the list holds one and none where it holds none, never the row's figures.
  const listed = s1.agentContexts.find((c) => c.agentId === 'main' && c.model === s1.model) ?? null;
  assert.deepEqual(d.lineFigures(s1, 'main', s1.model), listed);
});

test("sessionLines reads the recipes first with their pins, then every pin no recipe claims in the route's order", () => {
  const at = ms(14, 20);
  const entries = [pinEntry('main', 'claude-sonnet-5', 52_000), pinEntry('7b2e', 'claude-haiku-4-5', 18_000), pinEntry('7b2e', 'claude-sonnet-5', 31_000), pinEntry('c21e', 'claude-sonnet-5', 9_000)];
  // Every recipe runs on the helper's model, so 7b2e's Haiku pin is one no recipe claims, and 4f0c's release left none.
  const recipes = [recipe('main', 'busy', at), recipe('7b2e', 'idle', at), { ...recipe('4f0c', 'released', at), cause: 'agent_released' }];
  const lines = d.sessionLines({ agentContexts: entries }, recipes);
  assert.deepEqual(
    lines.map((l) => [l.agentId, l.model, l.recipe?.agentId ?? null, l.pin?.lastInputTokens ?? null]),
    [
      ['main', 'claude-sonnet-5', 'main', 52_000],
      ['7b2e', 'claude-sonnet-5', '7b2e', 31_000],
      ['4f0c', 'claude-sonnet-5', '4f0c', null],
      ['7b2e', 'claude-haiku-4-5', null, 18_000],
      ['c21e', 'claude-sonnet-5', null, 9_000],
    ],
  );
  assert.ok(lines[0].recipe === recipes[0] && lines[0].pin === entries[0] && lines[3].pin === entries[1], 'each line carries the recipe and the entry it was handed');
  assert.deepEqual(d.sessionLines({ agentContexts: [] }, []), [], 'a session with neither draws no line');
});

test("pinTtlMs reads the time left to the pin's last use plus its cache TTL, and none once that has passed or with no TTL", () => {
  const pin = { cacheTtlMs: 5 * MINUTE, lastUsedAt: iso(NOW - 5 * MINUTE + SECOND) };
  assert.equal(d.pinTtlMs(pin, NOW), SECOND, 'one second inside the TTL');
  assert.equal(d.pinTtlMs({ ...pin, lastUsedAt: iso(NOW - 5 * MINUTE - SECOND) }, NOW), null, 'one second past it');
  assert.equal(d.pinTtlMs({ ...pin, cacheTtlMs: null }, NOW), null, 'no TTL');
  assert.equal(d.pinTtlMs({ ...pin, lastUsedAt: null }, NOW), null, 'no last use');
});

test('the compacted cause reads its label and the sentence that says no probe goes out until the next main turn', () => {
  assert.equal(d.CAUSE_LABEL.compacted, 'compacted');
  assert.equal(
    d.CAUSE_HINT.compacted,
    'A completed compaction replaced the context this recipe kept warm, so the recipe stopped and no probe is sent. The next main turn starts a recipe for the new context.',
  );
});

test('the idle limit cause reads its label and the sentence that names the break-even where the pool sets no window', () => {
  assert.equal(d.CAUSE_LABEL.idle_limit, 'idle limit');
  assert.equal(
    d.CAUSE_HINT.idle_limit,
    "The pool's idle window, the break-even where the pool sets none, ran out with no real request, so the retained prompt was discarded. The next real request starts a new recipe.",
  );
});

test('the causes the retry ladder and the kept recipe move read their sentences, and their labels stand', () => {
  const want = {
    no_probe_cache_evidence: "The last probe's answer showed no cache read or write. The next attempt is scheduled on the retry ladder before the entry expires.",
    probe_pool_exhausted: 'Every account of the pool was exhausted or cooled when the probe was selected. The next attempt is scheduled on the retry ladder before the entry expires.',
    probe_quota_exhausted: "The account's usage window was spent when the last probe was sent. The next attempt is scheduled on the retry ladder before the entry expires.",
    probe_retry_limit: "The last probes failed with retryable errors along the retry ladder until the entry's TTL, so the engine stopped retrying.",
    probe_transport_error: 'The last probe failed on the transport before an answer came back. The next attempt is scheduled on the retry ladder before the entry expires.',
    request_canceled: "The agent's request was canceled before it finished, so the last answered request's recipe stays armed where one was, and the canceled body is never probed.",
    request_failed: "The agent's request failed, so the last answered request's recipe stays armed where one was, and the failed body is never probed.",
    retryable_probe_error: 'The last probe failed with a retryable error. The next attempt is scheduled on the retry ladder before the entry expires.',
    slow_probe: 'The last probe took its whole refresh interval, so the next one is scheduled on the retry ladder before the entry expires rather than starting at once.',
  };
  assert.deepEqual(Object.fromEntries(Object.keys(want).map((cause) => [cause, d.CAUSE_HINT[cause]])), want);
  assert.equal(d.CAUSE_LABEL.no_probe_cache_evidence, 'no probe cache evidence');
  assert.equal(d.CAUSE_LABEL.probe_retry_limit, 'retry limit');
  assert.equal(d.CAUSE_LABEL.retryable_probe_error, 'retrying');
  assert.equal(d.CAUSE_LABEL.slow_probe, 'slow probe');
});

/** A read as a tile names it: the word the notice uses, its error, and whether it has ever answered. */
const tileRead = (name, error, loaded) => ({ error, loaded, name });

test('firstFailedRead answers no read while every read has answered or none has failed', () => {
  assert.equal(d.firstFailedRead([tileRead('tiles', null, true), tileRead('sessions', null, true), tileRead('recipes', null, true), tileRead('keys', null, true)]), null, 'every read answered');
  assert.equal(d.firstFailedRead([tileRead('tiles', null, false), tileRead('sessions', null, false), tileRead('recipes', null, true), tileRead('keys', null, false)]), null, 'the reads still in flight');
  assert.equal(d.firstFailedRead([]), null);
});

test('firstFailedRead answers the overview where it failed before it answered, and the sessions where the overview answered', () => {
  const overview = tileRead('tiles', 'ledger locked', false);
  assert.equal(d.firstFailedRead([overview, tileRead('sessions', null, true), tileRead('recipes', null, true), tileRead('keys', null, true)]), overview);
  const sessions = tileRead('sessions', 'store busy', false);
  assert.equal(d.firstFailedRead([tileRead('tiles', null, true), sessions, tileRead('recipes', null, true), tileRead('keys', null, true)]), sessions);
});

test('firstFailedRead answers the first failed read in the order handed where two failed', () => {
  const recipes = tileRead('recipes', 'recipes locked', false);
  const keys = tileRead('keys', 'registry locked', false);
  assert.equal(d.firstFailedRead([tileRead('tiles', null, true), tileRead('sessions', null, true), recipes, keys]), recipes);
  assert.equal(d.firstFailedRead([tileRead('tiles', null, true), tileRead('sessions', null, true), keys, recipes]), keys);
});

test('firstFailedRead answers no read for a refresh that failed after a first answer, whose figures the tile keeps', () => {
  assert.equal(d.firstFailedRead([tileRead('tiles', null, true), tileRead('sessions', 'store busy', true), tileRead('recipes', null, true), tileRead('keys', null, true)]), null);
  const keys = tileRead('keys', 'registry locked', false);
  assert.equal(d.firstFailedRead([tileRead('tiles', 'ledger locked', true), tileRead('sessions', null, true), tileRead('recipes', null, true), keys]), keys, 'a later read that never answered still names its error');
});
