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
const orderRow = (sessionId, firstRequestAt, lastUsedAt, lastInferenceAt) => ({ session: { firstRequestAt: firstRequestAt && iso(firstRequestAt), lastInferenceAt: lastInferenceAt && iso(lastInferenceAt), lastUsedAt: lastUsedAt && iso(lastUsedAt), sessionId } });
const sessionIds = (rows) => rows.map((r) => r.session.sessionId);

test('orderSessionRows reads the newest first request first, ties by session id and a row with no first request last under either direction', () => {
  // a1 and a2 share a first request an hour after b, and n has none. Each row's pin was used at another time than its
  // last turn, in another order, so a key that read the pin's use, alone or as the later of the two, orders the
  // activity reads otherwise.
  const rows = [orderRow('n', null, null, null), orderRow('b', ms(13, 0), ms(14, 25), ms(14, 1)), orderRow('a2', ms(14, 0), ms(14, 2), ms(14, 20)), orderRow('a1', ms(14, 0), ms(14, 10), ms(14, 5))];
  const before = sessionIds(rows);
  assert.deepEqual(sessionIds(d.orderSessionRows(rows, 'created', 'desc')), ['a1', 'a2', 'b', 'n']);
  assert.deepEqual(sessionIds(d.orderSessionRows(rows, 'created', 'asc')), ['b', 'a1', 'a2', 'n']);
  assert.deepEqual(sessionIds(d.orderSessionRows(rows, 'activity', 'desc')), ['a2', 'a1', 'b', 'n'], "the last inference request decides, whatever the pin's last use");
  assert.deepEqual(sessionIds(d.orderSessionRows(rows, 'activity', 'asc')), ['b', 'a1', 'a2', 'n'], 'a row with neither time stays last');
  assert.deepEqual(sessionIds(rows), before, 'the list handed in is left as it is');
  assert.deepEqual(
    rows.map((r) => d.lastActivity(r.session)),
    [null, iso(ms(14, 1)), iso(ms(14, 20)), iso(ms(14, 5))],
  );
});

test('orderSessionRows by cost puts a row with no spend last under either direction, and by requests reads the count', () => {
  // The first requests run in neither key's order, so a key that fell back to them reads otherwise.
  // A row with no spend carries a null one, as the route answers it, so it counts no request.
  const row = (sessionId, firstRequestAt, usd, requests) => ({ session: { firstRequestAt: iso(firstRequestAt), lastInferenceAt: null, lastUsedAt: null, sessionId, spend: usd === null ? null : { billedUsd: {}, equivalent: 0, probes: { billedUsd: {}, equivalent: 0, requests: 0, shares: {}, usd: 0 }, requests, shares: {}, usd } } });
  const rows = [row('a', ms(13, 0), 0.5, 2), row('b', ms(14, 0), null, 0), row('c', ms(12, 0), 0.25, 4), row('d', ms(11, 0), 0.5, 1)];
  assert.deepEqual(sessionIds(d.orderSessionRows(rows, 'cost', 'desc')), ['a', 'd', 'c', 'b']);
  assert.deepEqual(sessionIds(d.orderSessionRows(rows, 'cost', 'asc')), ['c', 'a', 'd', 'b']);
  assert.deepEqual(sessionIds(d.orderSessionRows(rows, 'requests', 'desc')), ['c', 'a', 'd', 'b']);
});

/** A line as `sessionLines` answers it: a recipe with its pin or none, or a pin with no recipe, with its lifecycle
 *  word and its agent's first request, null for an agent with no ledger entry. */
const line = (r, p, lifecycle, firstRequestAt = null) => ({ agentId: (r ?? p).agentId, agentType: null, firstRequestAt: firstRequestAt && iso(firstRequestAt), lifecycle, model: (r ?? p).model, pin: p, recipe: r });
const linePin = (agentId, model, lastUsedAt) => ({ accountId: 'primary', agentId, cacheTtlMs: 5 * MINUTE, effort: 'high', lastInputTokens: 9_000, lastUsedAt: iso(lastUsedAt), model, warm: false });
const lineNames = (lines) => lines.map((l) => (l.model === 'claude-sonnet-5' ? l.agentId : `${l.agentId} on ${l.model}`));

test('orderLines puts main first, then the lines by lifecycle word, a pin-only line with its word, then the lines with no word, each group under the key and the direction', () => {
  const HAIKU = 'claude-haiku-4-5';
  const released = (agentId, at) => ({ ...recipe(agentId, 'released', at), cause: 'agent_released' });
  // Handed in no group's order: two busy lines, one on a pin and one with none, two idle ones, one of them a pin no
  // recipe stands on, two released ones with no pin, and e5, which no mod reports, pinned on two models with one first
  // request.
  const lines = [
    line(null, linePin('e5', 'claude-sonnet-5', ms(14, 12)), null, ms(13, 40)),
    line(released('e7c1', ms(14, 8)), null, 'released'),
    line(recipe('a4', 'busy', ms(14, 20)), null, 'busy'),
    line(null, linePin('d1', 'claude-sonnet-5', ms(14, 15)), 'idle', ms(13, 20)),
    line(recipe('c2', 'idle', ms(14, 5)), linePin('c2', 'claude-sonnet-5', ms(14, 5)), 'idle', ms(13, 10)),
    line(recipe('main', 'busy', ms(14, 0)), linePin('main', 'claude-sonnet-5', ms(14, 0)), 'busy', ms(13, 0)),
    line(null, linePin('e5', HAIKU, ms(14, 12)), null, ms(13, 40)),
    line(released('91aa', ms(14, 2)), null, 'released'),
    line(recipe('b7', 'busy', ms(14, 10)), linePin('b7', 'claude-sonnet-5', ms(14, 10)), 'busy', ms(13, 30)),
  ];
  const before = lineNames(lines);
  const e5 = [`e5 on ${HAIKU}`, 'e5'];
  // Under created a line reads its agent's first request, the busy line and the two released ones with no ledger entry
  // last in their groups whichever the direction, the released pair tied and read by agent id.
  assert.deepEqual(lineNames(d.orderLines(lines, 'created', 'desc')), ['main', 'b7', 'a4', 'd1', 'c2', '91aa', 'e7c1', ...e5]);
  assert.deepEqual(lineNames(d.orderLines(lines, 'created', 'asc')), ['main', 'b7', 'a4', 'c2', 'd1', '91aa', 'e7c1', ...e5]);
  // Under activity a recipe line reads its last request and a pin-only line its pin's last use, newest first.
  assert.deepEqual(lineNames(d.orderLines(lines, 'activity', 'desc')), ['main', 'a4', 'b7', 'd1', 'c2', 'e7c1', '91aa', ...e5]);
  assert.deepEqual(lineNames(d.orderLines(lines, 'activity', 'asc')), ['main', 'b7', 'a4', 'c2', 'd1', '91aa', 'e7c1', ...e5]);
  assert.deepEqual(lineNames(lines), before, 'the list handed in is left as it is');
});

test('requestsSpan picks the narrowest preset that covers the first request, with a minute of headroom under each width', () => {
  // The last request a minute back, so the span past the widest preset ends at the clock.
  const last = iso(NOW - MINUTE);
  const cases = [
    [59 * MINUTE, { preset: '1h' }],
    [59 * MINUTE + SECOND, { preset: '6h' }],
    [5 * HOUR + 59 * MINUTE, { preset: '6h' }],
    [5 * HOUR + 59 * MINUTE + SECOND, { preset: '24h' }],
    [23 * HOUR + 59 * MINUTE, { preset: '24h' }],
    [23 * HOUR + 59 * MINUTE + SECOND, { preset: '7d' }],
    [6 * DAY + 23 * HOUR + 59 * MINUTE, { preset: '7d' }],
    [6 * DAY + 23 * HOUR + 59 * MINUTE + SECOND, { preset: '30d' }],
    [29 * DAY + 23 * HOUR + 59 * MINUTE, { preset: '30d' }],
    [30 * DAY, { from: NOW - 30 * DAY, to: NOW }],
  ];
  assert.deepEqual(
    cases.map(([age]) => d.requestsSpan(iso(NOW - age), last, NOW)),
    cases.map(([, span]) => span),
  );
  assert.equal(d.requestsSpan(null, last, NOW), null);
});

test('a first request past the widest preset opens a custom span', () => {
  // The last request two days back, so a span that ended at the click reads otherwise.
  assert.deepEqual(d.requestsSpan(iso(NOW - 31 * DAY), iso(NOW - 2 * DAY), NOW), { from: NOW - 31 * DAY, to: NOW - 2 * DAY + MINUTE });
});

test('a session that draws a line is finished once every line it draws is released', () => {
  assert.equal(d.isFinished([{ agentId: 'main', lifecycle: 'released' }, { agentId: '3e', lifecycle: 'released' }]), true);
  assert.equal(d.isFinished([{ agentId: 'main', lifecycle: 'busy' }, { agentId: '3e', lifecycle: 'released' }]), false);
  assert.equal(d.isFinished([]), false);
});

test("expiryOf reads a row no companion reports as listed until the lesser limit after its latest instant, and a companion row as never dropped", () => {
  const limits = { expireAfterSeconds: 86_400, nonCompanionIdleSeconds: 1_800 };
  // The companion's end is the latest of the three instants, and the timeout the lesser of the two limits.
  const row = { companion: 'none', companionSince: iso(ms(10, 20)), lastRequestAt: iso(ms(9, 30)), lastUsedAt: iso(ms(10, 0)) };
  assert.equal(d.expiryOf(row, limits), ms(10, 50));
  assert.equal(d.expiryOf({ ...row, companion: 'reporting' }, limits), null);
});

test("openMark reads a mark that stands on the row's own account, and none off a no-return stamp alone", () => {
  const moved = { accountId: 'overflow', mark: null, shed: { at: iso(ms(14, 2)), from: 'primary', reason: 'cooldown' } };
  assert.equal(d.openMark(moved), null, 'a pin a failover moved off primary onto overflow');
  const marked = { accountId: 'studio', mark: { at: iso(ms(14, 5)), on: 'studio', window: 'five_hour' }, shed: null };
  assert.deepEqual(d.openMark(marked), { at: iso(ms(14, 5)), on: 'studio', window: 'five_hour' });
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
  effort: 'high',
  lastInputTokens,
  lastUsedAt: iso(ms(14, 20)),
  model,
  warm: false,
});
/** A row as the decoder answers it: main's pin on `model`, with figures of its own, and the route's entries. */
const sessionRow = (model, agentContexts) => ({ agentContexts, agentId: 'main', companion: 'reporting', lastInputTokens: 52_000, model, pinned: true });

test("an agent line reads the route's entry for its agent on its recipe's model, and none where the route lists no such pin", () => {
  const main = pinEntry('main', 'claude-sonnet-5-5', 52_000);
  // The route sorts the entries by agent and then model, main first, so the first entry of 7b2e is the other model's.
  const row = sessionRow('claude-sonnet-5-5', [main, pinEntry('7b2e', 'claude-haiku-4-5', 18_000), pinEntry('7b2e', 'claude-sonnet-5-5', 31_000)]);
  // Main's entry stands in the list, so no line of main is drawn from the row and the recipe's line leads.
  const pinOf = (agentId, model) => d.sessionLines({ ...row, agentFacts: [], agentRequests: [] }, [{ ...recipe(agentId, 'idle', null), model }])[0].pin;
  assert.equal(pinOf('7b2e', 'claude-sonnet-5-5[1M]')?.lastInputTokens, 31_000, 'the long-context suffix folds out case-blind');
  assert.equal(pinOf('main', 'claude-sonnet-5'), null, 'main on a model it holds no pin on reads none');
});

/** A spend part with no request in it. */
const NO_SPEND = { billedUsd: {}, equivalent: 0, requests: 0, shares: {}, usd: 0 };
/** A session that reports no fact, which a case spreads under the one member it sets. */
const NO_FACTS = { branch: null, cwd: null, project: null, repository: null, repositoryRoot: null, title: null };
/** An agent's ledger entry as the decoder answers it, with no spend unless the members hand one. */
const ledgerEntry = (agentId, model, lastRequestAt, members = {}) => ({ accountId: 'business', agentId, agentType: null, firstRequestAt: null, lastRequestAt: iso(lastRequestAt), model, spend: { ...NO_SPEND, probes: NO_SPEND }, ...members });

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
  const lines = d.sessionLines({ agentContexts: entries, agentFacts: [], agentId: 'main', agentRequests: [], lifecycle: 'busy', model: 'claude-sonnet-5' }, recipes);
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
    d.sessionLines({ agentContexts: [], agentFacts: [], agentId: null, agentRequests: [], lifecycle: 'registered', model: null }, []),
    [{ agentId: 'main', agentType: null, facts: null, firstRequestAt: null, lifecycle: 'registered', model: null, pin: null, recipe: null }],
    "a session with neither draws main's line from the row",
  );
});

test("sessionLines reads each line's type from its companion's report, else from its own agent's ledger entry, its first request from that entry, and main's model off the ledger where the row's pin is another agent's", () => {
  // Main sent auxiliary calls alone, so the row reads d41c's pin and d41c's model, which are not main's. The companion
  // reports a type for e52f and one for main, and none for d41c.
  const row = {
    agentContexts: [
      { ...pinEntry('d41c', 'claude-sonnet-4-5', 23_400), lifecycle: 'busy' },
      { ...pinEntry('e52f', 'claude-sonnet-4-5', 11_200), lifecycle: 'idle' },
    ],
    agentFacts: [
      { agentId: 'e52f', agentType: 'code-reviewer', description: null, name: null },
      { agentId: 'main', agentType: 'orchestrator', description: null, name: 'lead' },
    ],
    agentId: 'd41c',
    agentRequests: [
      ledgerEntry('main', 'claude-haiku-4-5', ms(14, 0), { firstRequestAt: iso(ms(13, 0)) }),
      ledgerEntry('d41c', 'claude-sonnet-4-5', ms(14, 10), { agentType: 'Explore', firstRequestAt: iso(ms(13, 30)) }),
      ledgerEntry('e52f', 'claude-sonnet-4-5', ms(14, 5), { agentType: 'custom', firstRequestAt: iso(ms(13, 45)) }),
    ],
    lifecycle: 'busy',
    model: 'claude-sonnet-4-5',
  };
  const lines = d.sessionLines(row, []);
  assert.deepEqual(
    lines.map((l) => [l.agentId, l.model, l.agentType, l.firstRequestAt, l.lifecycle]),
    [
      ['main', 'claude-haiku-4-5', null, iso(ms(13, 0)), 'busy'],
      ['d41c', 'claude-sonnet-4-5', 'Explore', iso(ms(13, 30)), 'busy'],
      ['e52f', 'claude-sonnet-4-5', 'code-reviewer', iso(ms(13, 45)), 'idle'],
    ],
  );
  assert.equal(lines[0].facts, null, "main's line reads no fact its companion reported");
});

test('sessionLabel reads the title, else the base name of the project, for which the working directory stands in, else null', () => {
  assert.equal(d.sessionLabel({ facts: { ...NO_FACTS, project: '/home/u/app/' }, sessionId: 'b2c4' }), 'app', 'a trailing separator');
  assert.equal(d.sessionLabel({ facts: { ...NO_FACTS, project: 'C:\\work\\app' }, sessionId: 'b2c4' }), 'app', 'a Windows path');
  assert.equal(d.sessionLabel({ facts: { ...NO_FACTS, project: 'C:\\work\\app\\' }, sessionId: 'b2c4' }), 'app', 'a Windows path with a trailing separator');
  assert.equal(d.sessionLabel({ facts: { ...NO_FACTS, project: '/' }, sessionId: 'b2c4' }), null, 'a root has no segment, so the id stands');
  assert.equal(d.sessionLabel({ facts: { ...NO_FACTS, project: 'C:\\' }, sessionId: 'b2c4' }), null, 'a drive root has no segment either');
  assert.equal(d.sessionLabel({ facts: { ...NO_FACTS, title: 'b2c4' }, sessionId: 'b2c4' }), null, 'a title that is the id');
  assert.equal(d.sessionLabel({ facts: { ...NO_FACTS, cwd: '/home/dev/work/scratch' }, sessionId: 'b2c4' }), 'scratch', 'a working directory stands in for a project that is not reported');
  assert.equal(d.sessionLabel({ facts: { ...NO_FACTS, cwd: '/home/dev/work/scratch', project: '/' }, sessionId: 'b2c4' }), null, 'a reported project at a root keeps the id, whatever the working directory reads');
});

test('agentLabel reads the name, else the description, else null', () => {
  assert.equal(d.agentLabel('7b2e', { description: 'Review the retry loop', name: null }), 'Review the retry loop', 'no name');
  assert.equal(d.agentLabel('7b2e', null), null, 'no report');
  assert.equal(d.agentLabel('7b2e', { description: null, name: null }), null, 'a report with neither');
  assert.equal(d.agentLabel('7b2e', { description: null, name: '7b2e' }), null, 'a name that is the id');
});

test("groupSessions keeps a checkout and its worktrees in one repository group, the groups in name order and each group's rows as handed", () => {
  const row = (id, facts) => ({ id, session: { facts: { ...NO_FACTS, ...facts } } });
  const rows = [
    row('b', { project: '/home/u/work/beta-shop', repository: 'git.example/beta/shop', repositoryRoot: '/home/u/work/beta-shop' }),
    row('w', { project: '/home/u/work/shop-worktrees/tax', repository: 'git.example/acme/shop', repositoryRoot: '/home/u/work/shop' }),
    row('n', {}),
    row('z', { project: '/home/u/work/Zeta', repositoryRoot: '/home/u/work/Zeta' }),
    row('c', { project: '/home/u/work/shop', repository: 'git.example/acme/shop', repositoryRoot: '/home/u/work/shop' }),
    row('y', { project: '/home/u/work/Zeta-worktrees/fix', repositoryRoot: '/home/u/work/Zeta' }),
    row('a', { project: '/home/u/work/alpha', repository: 'git.example/acme/alpha', repositoryRoot: '/home/u/work/alpha' }),
    row('s', { cwd: '/home/dev/work/scratch' }),
  ];
  const read = (by) => d.groupSessions(rows, by).map((g) => [g.key, g.short, g.rows.map((r) => r.id)]);
  assert.deepEqual(read('repository'), [
    ['git.example/acme/alpha', 'alpha', ['a']],
    ['git.example/acme/shop', 'shop', ['w', 'c']],
    ['git.example/beta/shop', 'shop', ['b']],
    ['/home/u/work/Zeta', 'Zeta', ['z', 'y']],
    [null, 'No repository', ['n', 's']],
  ]);
  assert.deepEqual(read('project'), [
    ['/home/u/work/alpha', 'alpha', ['a']],
    ['/home/u/work/beta-shop', 'beta-shop', ['b']],
    ['/home/u/work/Zeta-worktrees/fix', 'fix', ['y']],
    ['/home/dev/work/scratch', 'scratch', ['s']],
    ['/home/u/work/shop', 'shop', ['c']],
    ['/home/u/work/shop-worktrees/tax', 'tax', ['w']],
    ['/home/u/work/Zeta', 'Zeta', ['z']],
    [null, 'No project', ['n']],
  ]);
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
  const session = { agentFacts: [], agentRequests: [ledgerEntry('main', 'claude-sonnet-5', ms(14, 0)), ledgerEntry('e9', 'claude-haiku-4-5', ms(13, 0)), ledgerEntry('a1', 'claude-sonnet-5', ms(14, 5)), ledgerEntry('c3', 'claude-haiku-4-5', ms(13, 30))] };
  assert.deepEqual(
    d.earlierAgents(session, [{ agentId: 'a1' }]).map((a) => a.agentId),
    ['c3', 'e9'],
  );
});

test("earlierAgents reads an earlier agent's reported name, and its reported type ahead of its ledger entry's", () => {
  const session = {
    agentFacts: [{ agentId: 'a1', agentType: 'code-reviewer', description: null, name: 'reviewer' }],
    agentRequests: [ledgerEntry('main', 'claude-sonnet-5', ms(14, 0)), ledgerEntry('a1', 'claude-sonnet-5', ms(14, 5), { agentType: 'general-purpose' })],
  };
  assert.deepEqual(d.earlierAgents(session, []).map((a) => [a.agentId, a.facts?.name ?? null, a.agentType]), [['a1', 'reviewer', 'code-reviewer']]);
});

/** Every filter off, as the address reads with none set. */
const NO_FILTERS = { account: [], active: '', agentType: [], cache: [], client: [], companion: [], effort: [], finished: false, forked: false, key: [], model: [], pool: [], released: false, search: '', session: '', slowed: false, status: [] };
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

/** A row under Active within: its last turn, which a first turn's row holds none of, and its pin's last use. */
const activeRow = (lastInferenceAt, lastUsedAt) => ({ finished: false, lines: [], poolId: 'all', session: { ...matchSession, lastInferenceAt, lastRequestAt: lastInferenceAt, lastUsedAt } });

test("a probe's pin use is no activity", () => {
  // A probe stamped the pin ten minutes back, and the last turn is eleven hours old.
  assert.equal(d.matches(activeRow(iso(NOW - 11 * HOUR), iso(NOW - 10 * MINUTE)), { ...NO_FILTERS, active: '1h' }, NOW), false);
});

test("a session on its first turn reads its pin's use", () => {
  assert.equal(d.matches(activeRow(null, iso(NOW - 10 * MINUTE)), { ...NO_FILTERS, active: '1h' }, NOW), true);
});

test("matches reads a line's Account as the Agents table shows it: the pin's first, beside a recipe that names none too", () => {
  const row = (line) => ({ finished: false, lines: [line], poolId: 'all', session: matchSession });
  const onA = { ...NO_FILTERS, account: ['a'] };
  assert.equal(d.matches(row({ ...matchLine('a1', 'claude-opus-5', 'busy'), pin: { accountId: 'a' }, recipe: { accountId: null } }), onA, NOW), true, 'a pin names its account ahead of a recipe that names none');
  assert.equal(d.matches(row({ ...matchLine('a1', 'claude-opus-5', 'busy'), pin: { accountId: 'a' }, recipe: null }), onA, NOW), true, "a pin with no recipe names the pin's account");
});

test('matches searches the id by its start and the reported facts by a word, whatever the case', () => {
  const found = (session, search) => d.matches({ finished: false, lines: [], poolId: 'all', session: { ...matchSession, facts: NO_FACTS, ...session } }, { ...NO_FILTERS, search }, NOW);
  assert.equal(found({ facts: { ...NO_FACTS, title: 'Gateway fixes' } }, 'GATE'), true, 'a word of the title');
  assert.equal(found({ facts: { ...NO_FACTS, project: '/home/u/work/storefront-gateway' } }, 'GATE'), true, 'the end of the project path');
  assert.equal(found({ facts: { ...NO_FACTS, branch: 'fix/gateway-retry' } }, 'GATE'), true, 'a word of the branch');
  assert.equal(found({ facts: { ...NO_FACTS, cwd: '/home/u/work/storefront/gateway' } }, 'GATE'), true, 'the end of the working directory');
  assert.equal(found({ facts: { ...NO_FACTS, repository: 'git.example/acme/gateway' } }, 'GATE'), true, 'a word of the repository');
  assert.equal(found({ facts: { ...NO_FACTS, repositoryRoot: '/home/u/work/gateway' } }, 'GATE'), true, 'the end of the repository root');
  assert.equal(found({ sessionId: 'b2c4d6' }, 'b2'), true, "the id's start");
  assert.equal(found({ sessionId: 'a7b2c4' }, 'b2'), false, 'the id past its start');
});

test('matches keeps the one row whose id is the session key, whole, where the search would keep three', () => {
  const kept = (session) => d.matches({ finished: false, lines: [], poolId: 'all', session: { ...matchSession, facts: NO_FACTS, ...session } }, { ...NO_FILTERS, session: 'build' }, NOW);
  assert.deepEqual([kept({ sessionId: 'build' }), kept({ sessionId: 'build-2' }), kept({ facts: { ...NO_FACTS, title: 'build' }, sessionId: 'c7' })], [true, false, false]);
});

test('hiddenLines counts the lines the Agents filters hide and the earlier agents', () => {
  const session = { ...matchSession, agentRequests: [ledgerEntry('main', 'claude-sonnet-5', ms(14, 0)), ledgerEntry('a1', 'claude-opus-5', ms(14, 5)), ledgerEntry('b2', 'claude-sonnet-5', ms(14, 6)), ledgerEntry('e9', 'claude-haiku-4-5', ms(13, 0))] };
  const row = { finished: false, lines: [matchLine('main', 'claude-sonnet-5', 'busy'), matchLine('a1', 'claude-opus-5', 'busy'), matchLine('b2', 'claude-sonnet-5', 'idle')], poolId: 'all', session };
  const { hidden, shown } = d.hiddenLines(row, { ...NO_FILTERS, model: ['claude-opus-5'] });
  assert.deepEqual([hidden, shown.map((l) => l.agentId)], [3, ['a1']]);
});
