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

test('orderRows by cost puts a row with no spend last under either direction, ties by session id, and by requests reads the count', () => {
  // The creation times run in neither key's order, so a key that fell back to them reads otherwise.
  const row = (sessionId, createdAt, usd, requests) => ({ session: { createdAt: iso(createdAt), lastRequestAt: null, lastUsedAt: null, requests, sessionId, spend: usd === null ? null : { usd } } });
  const rows = [row('a', ms(13, 0), 0.5, 2), row('b', ms(14, 0), null, 9), row('c', ms(12, 0), 0.25, 4), row('d', ms(11, 0), 0.5, 1)];
  assert.deepEqual(sessionIds(d.orderRows(rows, 'cost', 'desc')), ['a', 'd', 'c', 'b']);
  assert.deepEqual(sessionIds(d.orderRows(rows, 'cost', 'asc')), ['c', 'a', 'd', 'b']);
  assert.deepEqual(sessionIds(d.orderRows(rows, 'requests', 'desc')), ['b', 'c', 'a', 'd']);
});

/** A line as `sessionLines` answers it: a recipe with its pin or none, or a pin with no recipe, with its lifecycle
 *  word. */
const line = (r, p, lifecycle) => ({ agentId: (r ?? p).agentId, agentType: null, lifecycle, model: (r ?? p).model, pin: p, recipe: r });
const linePin = (agentId, model, createdAt, lastUsedAt) => ({ accountId: 'primary', agentId, cacheTtlMs: 5 * MINUTE, createdAt: iso(createdAt), effort: 'high', lastInputTokens: 9_000, lastUsedAt: iso(lastUsedAt), model, warm: false });
const lineNames = (lines) => lines.map((l) => (l.model === 'claude-sonnet-5' ? l.agentId : `${l.agentId} on ${l.model}`));

test('orderLines puts main first, then the lines by lifecycle word, a pin-only line with its word, then the lines with no word, each group under the key and the direction', () => {
  const HAIKU = 'claude-haiku-4-5';
  const released = (agentId, at) => ({ ...recipe(agentId, 'released', at), cause: 'agent_released' });
  // Handed in no group's order: two busy lines, one on a pin and one with none, two idle ones, one of them a pin no
  // recipe stands on, two released ones with no pin, and e5, which no mod reports, pinned on two models at one instant.
  const lines = [
    line(null, linePin('e5', 'claude-sonnet-5', ms(13, 40), ms(14, 12)), null),
    line(released('e7c1', ms(14, 8)), null, 'released'),
    line(recipe('a4', 'busy', ms(14, 20)), null, 'busy'),
    line(null, linePin('d1', 'claude-sonnet-5', ms(13, 20), ms(14, 15)), 'idle'),
    line(recipe('c2', 'idle', ms(14, 5)), linePin('c2', 'claude-sonnet-5', ms(13, 10), ms(14, 5)), 'idle'),
    line(recipe('main', 'busy', ms(14, 0)), linePin('main', 'claude-sonnet-5', ms(13, 0), ms(14, 0)), 'busy'),
    line(null, linePin('e5', HAIKU, ms(13, 40), ms(14, 12)), null),
    line(released('91aa', ms(14, 2)), null, 'released'),
    line(recipe('b7', 'busy', ms(14, 10)), linePin('b7', 'claude-sonnet-5', ms(13, 30), ms(14, 10)), 'busy'),
  ];
  const before = lineNames(lines);
  const e5 = [`e5 on ${HAIKU}`, 'e5'];
  // Under created a line reads its own pin's creation, the busy line with no pin and the two released ones last in
  // their groups whichever the direction, the released pair tied and read by agent id.
  assert.deepEqual(lineNames(d.orderLines(lines, 'created', 'desc')), ['main', 'b7', 'a4', 'd1', 'c2', '91aa', 'e7c1', ...e5]);
  assert.deepEqual(lineNames(d.orderLines(lines, 'created', 'asc')), ['main', 'b7', 'a4', 'c2', 'd1', '91aa', 'e7c1', ...e5]);
  // Under activity a recipe line reads its last request and a pin-only line its pin's last use, newest first.
  assert.deepEqual(lineNames(d.orderLines(lines, 'activity', 'desc')), ['main', 'a4', 'b7', 'd1', 'c2', 'e7c1', '91aa', ...e5]);
  assert.deepEqual(lineNames(d.orderLines(lines, 'activity', 'asc')), ['main', 'b7', 'a4', 'c2', 'd1', '91aa', 'e7c1', ...e5]);
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

test('isFinished reads a session whose main ended with it, or whose every line the mod released', () => {
  const session = (cause) => ({ keepalive: { cause, nextProbeAt: null } });
  assert.equal(d.isFinished(session('session_ended'), [{ agentId: 'main', lifecycle: 'idle' }, { agentId: '3e', lifecycle: 'released' }]), true);
  assert.equal(d.isFinished(session('agent_released'), [{ agentId: 'main', lifecycle: 'released' }, { agentId: '3e', lifecycle: 'released' }]), true);
  assert.equal(d.isFinished(session('probing'), [{ agentId: 'main', lifecycle: 'busy' }, { agentId: '3e', lifecycle: 'released' }]), false);
  assert.equal(d.isFinished(session('scheduled'), []), false);
});

test('withinExpiry keeps a session whose last use lies inside the expiry of now and drops one at the bound, reads the last request where no use stands, and keeps every session while the expiry is unknown', () => {
  const now = Date.UTC(2026, 8, 16, 14, 30);
  const at = (msAgo) => new Date(now - msAgo).toISOString();
  assert.equal(d.withinExpiry({ lastRequestAt: null, lastUsedAt: at(HOUR - 1) }, 3_600, now), true);
  assert.equal(d.withinExpiry({ lastRequestAt: null, lastUsedAt: at(HOUR) }, 3_600, now), false);
  assert.equal(d.withinExpiry({ lastRequestAt: at(HOUR), lastUsedAt: null }, 3_600, now), false);
  assert.equal(d.withinExpiry({ lastRequestAt: at(HOUR - 1), lastUsedAt: null }, 3_600, now), true);
  assert.equal(d.withinExpiry({ lastRequestAt: null, lastUsedAt: at(HOUR) }, null, now), true);
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

/** A spend part with no request in it. */
const NO_SPEND = { billedUsd: {}, equivalent: 0, requests: 0, shares: {}, usd: 0 };
/** An agent's ledger entry as the decoder answers it, with no spend unless the members hand one. */
const ledgerEntry = (agentId, model, requests, lastRequestAt, members = {}) => ({ accountId: 'business', agentId, agentType: null, lastRequestAt: iso(lastRequestAt), model, requests, spend: { ...NO_SPEND, probes: NO_SPEND }, ...members });

test("sessionLines reads the recipes first with their pins, then every pin no recipe claims in the route's order, each line its pin's word where a pin stands", () => {
  const at = ms(14, 20);
  // Main's pin carries no word under its busy recipe and 7b2e's Sonnet pin another word than its recipe's, so a line
  // that read its recipe's word where a pin stands reads otherwise.
  const entries = [
    { ...pinEntry('main', 'claude-sonnet-5', 52_000), lifecycle: null },
    { ...pinEntry('7b2e', 'claude-haiku-4-5', 18_000), lifecycle: 'idle' },
    { ...pinEntry('7b2e', 'claude-sonnet-5', 31_000), lifecycle: 'busy' },
    { ...pinEntry('c21e', 'claude-sonnet-5', 9_000), lifecycle: null },
  ];
  // Every recipe runs on the helper's model, so 7b2e's Haiku pin is one no recipe claims, and 4f0c's release left none.
  const recipes = [recipe('main', 'busy', at), recipe('7b2e', 'idle', at), { ...recipe('4f0c', 'released', at), cause: 'agent_released' }];
  const lines = d.sessionLines({ agentContexts: entries, agentId: 'main', agentRequests: [], lifecycle: 'busy', model: 'claude-sonnet-5' }, recipes);
  assert.deepEqual(
    lines.map((l) => [l.agentId, l.model, l.recipe?.agentId ?? null, l.pin?.lastInputTokens ?? null, l.lifecycle]),
    [
      ['main', 'claude-sonnet-5', 'main', 52_000, null],
      ['7b2e', 'claude-sonnet-5', '7b2e', 31_000, 'busy'],
      ['4f0c', 'claude-sonnet-5', '4f0c', null, 'released'],
      ['7b2e', 'claude-haiku-4-5', null, 18_000, 'idle'],
      ['c21e', 'claude-sonnet-5', null, 9_000, null],
    ],
  );
  assert.ok(lines[0].recipe === recipes[0] && lines[0].pin === entries[0] && lines[3].pin === entries[1], 'each line carries the recipe and the entry it was handed');
  assert.deepEqual(
    d.sessionLines({ agentContexts: [], agentId: null, agentRequests: [], lifecycle: 'registered', model: null }, []),
    [{ agentId: 'main', agentType: null, lifecycle: 'registered', model: null, pin: null, recipe: null }],
    "a session with neither draws main's line from the row",
  );
});

test("sessionLines reads each line's type from the agent's newest ledger entry, and main's model off the ledger where the row's pin is another agent's", () => {
  // Main sent auxiliary calls alone, so the row reads d41c's pin and d41c's model, which are not main's.
  const row = {
    agentContexts: [{ ...pinEntry('d41c', 'claude-sonnet-4-5', 23_400), lifecycle: 'busy' }],
    agentId: 'd41c',
    agentRequests: [ledgerEntry('main', 'claude-haiku-4-5', 2, ms(14, 0)), ledgerEntry('d41c', 'claude-haiku-4-5', 1, ms(13, 0), { agentType: 'Plan' }), ledgerEntry('d41c', 'claude-sonnet-4-5', 1, ms(14, 10), { agentType: 'Explore' })],
    lifecycle: 'busy',
    model: 'claude-sonnet-4-5',
  };
  assert.deepEqual(
    d.sessionLines(row, []).map((l) => [l.agentId, l.model, l.agentType, l.lifecycle]),
    [
      ['main', 'claude-haiku-4-5', null, 'busy'],
      ['d41c', 'claude-sonnet-4-5', 'Explore', 'busy'],
    ],
  );
});

test("statusOf reads finished before the lifecycle word, unreported where no word stands, and the engine's unknown as unknown", () => {
  const session = (lifecycle) => ({ companion: 'reporting', lifecycle });
  assert.deepEqual(d.statusOf(session('busy'), true), { tone: 'neutral', word: 'finished' }, 'main busy when the session ended');
  assert.deepEqual(d.statusOf(session(null), false), { tone: 'muted', word: 'unreported' });
  assert.deepEqual(d.statusOf(session('unknown'), false), { tone: 'neutral', word: 'unknown' });
});

test("presenceOf reads a silent companion's span from when it fell silent and a reporting one's presence age", () => {
  const silent = { companion: 'silent', companionSince: iso(NOW - (2 * HOUR + 18 * MINUTE)), presenceAgeSeconds: 9 };
  assert.deepEqual(d.presenceOf(silent, NOW), { text: 'silent 2h 18m', tone: 'warning' });
  assert.deepEqual(d.presenceOf({ ...silent, companion: 'reporting', companionSince: null }, NOW), { text: 'seen 9s ago', tone: 'muted' });
});

test('earlierAgents lists the agents with ledgered requests that no line names, in id order, main never among them', () => {
  // No line names main, so a list that took main's entry for an earlier agent's reads it.
  const session = { agentRequests: [ledgerEntry('main', 'claude-sonnet-5', 4, ms(14, 0)), ledgerEntry('e9', 'claude-haiku-4-5', 1, ms(13, 0)), ledgerEntry('a1', 'claude-sonnet-5', 2, ms(14, 5)), ledgerEntry('c3', 'claude-haiku-4-5', 1, ms(13, 30))] };
  assert.deepEqual(
    d.earlierAgents(session, [{ agentId: 'a1' }]).map((a) => a.agentId),
    ['c3', 'e9'],
  );
});

test("agentFigures sums an agent's entries across its models and reads the newest entry's model, account and type", () => {
  const spend = (part, probes) => ({ ...part, probes });
  const session = {
    agentRequests: [
      ledgerEntry('a1', 'claude-haiku-4-5', 3, ms(13, 0), {
        agentType: 'Plan',
        spend: spend({ billedUsd: { fast: 0.25 }, equivalent: 0.5, requests: 3, shares: { business: { five_hour: 0.125 } }, usd: 0.75 }, { billedUsd: {}, equivalent: 0.25, requests: 1, shares: { business: { five_hour: 0.0625 } }, usd: 0.5 }),
      }),
      ledgerEntry('a1', 'claude-sonnet-5', 2, ms(14, 0), {
        accountId: 'studio',
        agentType: 'Explore',
        spend: spend({ billedUsd: { api_key: 0.5 }, equivalent: null, requests: 2, shares: { business: { five_hour: 0.25, seven_day: 0.125 } }, usd: 0.5 }, NO_SPEND),
      }),
      ledgerEntry('b2', 'claude-sonnet-5', 7, ms(14, 10)),
    ],
  };
  const a1 = d.agentFigures(session, 'a1');
  assert.deepEqual([a1.requests, a1.model, a1.accountId, a1.agentType, a1.lastRequestAt], [5, 'claude-sonnet-5', 'studio', 'Explore', iso(ms(14, 0))]);
  assert.deepEqual(a1.spend, {
    billedUsd: { api_key: 0.5, fast: 0.25 },
    equivalent: null,
    probes: { billedUsd: {}, equivalent: 0.25, requests: 1, shares: { business: { five_hour: 0.0625 } }, usd: 0.5 },
    requests: 5,
    shares: { business: { five_hour: 0.375, seven_day: 0.125 } },
    usd: 1.25,
  });
  assert.equal(d.agentFigures(session, 'f00d'), null, 'an agent with no entry');
});

/** Every filter off, as the address reads with none set. */
const NO_FILTERS = { account: [], active: '', agentType: [], cache: [], client: [], companion: [], effort: [], finished: false, forked: false, key: [], model: [], pool: [], released: false, search: '', slowed: false, status: [] };
/** A drawn line with the two members the Model and Status filters read. */
const matchLine = (agentId, model, lifecycle) => ({ agentId, agentType: null, lifecycle, model, pin: null, recipe: null });
const matchSession = { agentRequests: [], companion: 'reporting', forkedFrom: null, historical: false, keyGeneration: 1, keyId: 'desktop', lastRequestAt: iso(ms(14, 20)), lastUsedAt: iso(ms(14, 20)), sessionId: 'session-1', speed: null, userAgent: null };

test('matches lists a session under Agents filters where one of its drawn lines passes them all, not where two lines pass one each', () => {
  const filters = { ...NO_FILTERS, model: ['claude-opus-5'], status: ['busy'] };
  const row = (lines) => ({ finished: false, lines, poolId: 'all', session: matchSession });
  assert.equal(d.matches(row([matchLine('main', 'claude-opus-5', 'idle'), matchLine('a1', 'claude-sonnet-5', 'busy')]), filters, NOW), false, 'one line on the model, the other in the word');
  assert.equal(d.matches(row([matchLine('main', 'claude-sonnet-5', 'idle'), matchLine('a1', 'claude-opus-5', 'busy')]), filters, NOW), true, 'one line on the model in the word');
  const onModel = { ...NO_FILTERS, model: ['claude-opus-5'] };
  const finishedRow = { finished: true, lines: [matchLine('main', 'claude-sonnet-5', 'released'), matchLine('a1', 'claude-opus-5', 'released')], poolId: 'all', session: matchSession };
  assert.equal(d.matches(finishedRow, { ...onModel, finished: true }, NOW), true, "a finished row's released line, which the row draws without the flag");
  const liveLines = [matchLine('main', 'claude-sonnet-5', 'busy'), matchLine('a1', 'claude-opus-5', 'released')];
  assert.equal(d.matches(row(liveLines), onModel, NOW), false, "a live row's released line, which the row draws only under the flag");
  assert.equal(d.matches(row(liveLines), { ...onModel, released: true }, NOW), true, 'the same line under the flag');
});

test("matches reads a bare key id as the live key's rows and id@generation as that lifetime's alone", () => {
  const keyRow = (keyGeneration, historical) => ({ finished: false, lines: [], poolId: 'all', session: { ...matchSession, historical, keyGeneration } });
  const rows = [keyRow(2, false), keyRow(1, true)];
  assert.deepEqual(rows.map((r) => d.matches(r, { ...NO_FILTERS, key: ['desktop'] }, NOW)), [true, false], "a bare id keeps the live holder's row and turns the earlier holder's away");
  assert.deepEqual(rows.map((r) => d.matches(r, { ...NO_FILTERS, key: ['desktop@1'] }, NOW)), [false, true], 'a lifetime keeps its own row alone');
});

test('matches reads Active within against the later of the last use and the last request', () => {
  const row = (lastRequestAt, lastUsedAt) => ({ finished: false, lines: [], poolId: 'all', session: { ...matchSession, lastRequestAt, lastUsedAt } });
  const within = { ...NO_FILTERS, active: '1h' };
  assert.equal(d.matches(row(iso(NOW - 2 * HOUR), iso(NOW - 10 * MINUTE)), within, NOW), true, 'the last use inside the span');
  assert.equal(d.matches(row(iso(NOW - 10 * MINUTE), iso(NOW - 2 * HOUR)), within, NOW), true, 'the last request inside the span');
  assert.equal(d.matches(row(iso(NOW - 2 * HOUR), iso(NOW - 2 * HOUR)), within, NOW), false, 'both older than the span');
});

test("matches reads a line's Account as the Agents table shows it: none past a refused recipe beside a pin, the pin's with no recipe", () => {
  const row = (line) => ({ finished: false, lines: [line], poolId: 'all', session: matchSession });
  const onA = { ...NO_FILTERS, account: ['a'] };
  assert.equal(d.matches(row({ ...matchLine('a1', 'claude-opus-5', 'busy'), pin: { accountId: 'a' }, recipe: { accountId: null } }), onA, NOW), false, "a refused recipe's line names no account, whatever pin stands beside it");
  assert.equal(d.matches(row({ ...matchLine('a1', 'claude-opus-5', 'busy'), pin: { accountId: 'a' }, recipe: null }), onA, NOW), true, "a pin with no recipe names the pin's account");
});

test('hiddenLines counts the lines the Agents filters hide and the earlier agents', () => {
  const session = { ...matchSession, agentRequests: [ledgerEntry('main', 'claude-sonnet-5', 4, ms(14, 0)), ledgerEntry('a1', 'claude-opus-5', 2, ms(14, 5)), ledgerEntry('b2', 'claude-sonnet-5', 1, ms(14, 6)), ledgerEntry('e9', 'claude-haiku-4-5', 1, ms(13, 0))] };
  const row = { finished: false, lines: [matchLine('main', 'claude-sonnet-5', 'busy'), matchLine('a1', 'claude-opus-5', 'busy'), matchLine('b2', 'claude-sonnet-5', 'idle')], poolId: 'all', session };
  const { hidden, shown } = d.hiddenLines(row, { ...NO_FILTERS, model: ['claude-opus-5'] });
  assert.deepEqual([hidden, shown.map((l) => l.agentId)], [3, ['a1']]);
});

test("lineWordOf reads main's line on a session no mod reports as unreported, and a main the session's end released as released", () => {
  const session = { companion: 'none' };
  assert.equal(d.lineWordOf(session, matchLine('main', 'claude-sonnet-5', 'idle')), 'unreported', "the engine's own word for main");
  assert.equal(d.lineWordOf(session, matchLine('main', 'claude-sonnet-5', 'released')), 'released', 'the release the end left');
});

test("pinTtlMs reads the time left to the pin's last use plus its cache TTL, and none once that has passed or with no TTL", () => {
  const pin = { cacheTtlMs: 5 * MINUTE, lastUsedAt: iso(NOW - 5 * MINUTE + SECOND) };
  assert.equal(d.pinTtlMs(pin, NOW), SECOND, 'one second inside the TTL');
  assert.equal(d.pinTtlMs({ ...pin, lastUsedAt: iso(NOW - 5 * MINUTE - SECOND) }, NOW), null, 'one second past it');
  assert.equal(d.pinTtlMs({ ...pin, cacheTtlMs: null }, NOW), null, 'no TTL');
  assert.equal(d.pinTtlMs({ ...pin, lastUsedAt: null }, NOW), null, 'no last use');
});
