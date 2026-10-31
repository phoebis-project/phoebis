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
/** A row as the page builds it, with the three times the order reads, each null or a clock time on the test's day. */
const orderRow = (sessionId, createdAt, lastUsedAt, lastRequestAt) => ({ session: { createdAt: createdAt && iso(createdAt), lastRequestAt: lastRequestAt && iso(lastRequestAt), lastUsedAt: lastUsedAt && iso(lastUsedAt), sessionId } });
const sessionIds = (rows) => rows.map((r) => r.session.sessionId);

test('orderRows reads the newest created first, ties by session id and a row with no creation time last under either direction', () => {
  // a1 and a2 share an instant an hour after b, and n has no creation time. Each row's later time is a different one of
  // its two, so a key that read either alone would order the activity reads otherwise.
  const rows = [orderRow('n', null, null, null), orderRow('b', ms(13, 0), ms(14, 25), null), orderRow('a2', ms(14, 0), ms(14, 2), ms(14, 20)), orderRow('a1', ms(14, 0), ms(14, 10), ms(14, 5))];
  const before = sessionIds(rows);
  assert.deepEqual(sessionIds(d.orderRows(rows, 'created', 'desc')), ['a1', 'a2', 'b', 'n']);
  assert.deepEqual(sessionIds(d.orderRows(rows, 'created', 'asc')), ['b', 'a1', 'a2', 'n']);
  assert.deepEqual(sessionIds(d.orderRows(rows, 'activity', 'desc')), ['b', 'a2', 'a1', 'n'], 'the later of the last use and the last request decides');
  assert.deepEqual(sessionIds(d.orderRows(rows, 'activity', 'asc')), ['a1', 'a2', 'b', 'n'], 'a row with neither time stays last');
  assert.deepEqual(sessionIds(rows), before, 'the list handed in is left as it is');
  assert.deepEqual(
    rows.map((r) => d.lastActivity(r.session)),
    [null, iso(ms(14, 25)), iso(ms(14, 20)), iso(ms(14, 10))],
  );
});

/** A line as `sessionLines` answers it: a recipe with its pin or none, or a pin with no recipe. */
const line = (r, p) => ({ agentId: (r ?? p).agentId, model: (r ?? p).model, pin: p, recipe: r });
const linePin = (agentId, model, createdAt, lastUsedAt) => ({ accountId: 'primary', agentId, cacheTtlMs: 5 * MINUTE, createdAt: iso(createdAt), effort: 'high', lastInputTokens: 9_000, lastUsedAt: iso(lastUsedAt), model, warm: false });
const lineNames = (lines) => lines.map((l) => (l.model === 'claude-sonnet-5' ? l.agentId : `${l.agentId} on ${l.model}`));

test('orderLines puts main first, then the recipe lines by lifecycle word, then the pins no recipe stands on, each group under the key and the direction', () => {
  const HAIKU = 'claude-haiku-4-5';
  const released = (agentId, at) => ({ ...recipe(agentId, 'released', at), cause: 'agent_released' });
  // Handed in no group's order: two busy lines, one on a pin and one with none, an idle one, two released ones with no
  // pin, and two pin-only agents whose creation and last use run opposite ways, e5 pinned on two models at one instant.
  const lines = [
    line(null, linePin('e5', 'claude-sonnet-5', ms(13, 40), ms(14, 12))),
    line(released('e7c1', ms(14, 8)), null),
    line(recipe('a4', 'busy', ms(14, 20)), null),
    line(null, linePin('d1', 'claude-sonnet-5', ms(13, 20), ms(14, 15))),
    line(recipe('c2', 'idle', ms(14, 5)), linePin('c2', 'claude-sonnet-5', ms(13, 10), ms(14, 5))),
    line(recipe('main', 'busy', ms(14, 0)), linePin('main', 'claude-sonnet-5', ms(13, 0), ms(14, 0))),
    line(null, linePin('e5', HAIKU, ms(13, 40), ms(14, 12))),
    line(released('91aa', ms(14, 2)), null),
    line(recipe('b7', 'busy', ms(14, 10)), linePin('b7', 'claude-sonnet-5', ms(13, 30), ms(14, 10))),
  ];
  const before = lineNames(lines);
  const e5 = [`e5 on ${HAIKU}`, 'e5'];
  // Under created a line reads its own pin's creation, the busy line with no pin and the two released ones last in
  // their groups whichever the direction, the released pair tied and read by agent id.
  assert.deepEqual(lineNames(d.orderLines(lines, 'created', 'desc')), ['main', 'b7', 'a4', 'c2', '91aa', 'e7c1', ...e5, 'd1']);
  assert.deepEqual(lineNames(d.orderLines(lines, 'created', 'asc')), ['main', 'b7', 'a4', 'c2', '91aa', 'e7c1', 'd1', ...e5]);
  // Under activity a recipe line reads its last request and a pin-only line its pin's last use, newest first.
  assert.deepEqual(lineNames(d.orderLines(lines, 'activity', 'desc')), ['main', 'a4', 'b7', 'c2', 'e7c1', '91aa', 'd1', ...e5]);
  assert.deepEqual(lineNames(d.orderLines(lines, 'activity', 'asc')), ['main', 'b7', 'a4', 'c2', '91aa', 'e7c1', ...e5, 'd1']);
  assert.deepEqual(lineNames(lines), before, 'the list handed in is left as it is');
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
