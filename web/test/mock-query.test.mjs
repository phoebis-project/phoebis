import assert from 'node:assert/strict';
import { test } from 'node:test';

// The mock skips its draws by the local hour, so the file runs in UTC whatever the host's zone. `node --test` gives each
// file a process of its own.
process.env.TZ = 'UTC';

/** The instant the smoke run and the browser pass pin. The mock draws its story from the clock it loads at. */
const AT = '2026-09-16T14:30:00Z';
const wall = Date.now;
Date.now = () => Date.parse(AT);
let d;
try {
  d = await import('./dist/api.js');
} finally {
  Date.now = wall;
}

const api = new d.MockApi();
/** Every row the story holds, the probes among them. */
const all = (await api.listRequests({ includeProbes: true, limit: 100_000 })).requests;
const unprobed = all.filter((r) => r.kind !== 'probe');
const ids = (rows) => rows.map((r) => r.id).sort();
const listed = async (query) => ids((await api.listRequests({ limit: 100_000, ...query })).requests);
/** Each case reads a strict subset, so a filter the mock ignores cannot pass it. */
const narrowed = (rows, what) => assert.ok(rows.length > 0 && rows.length < unprobed.length, `the story holds ${rows.length} rows ${what} of ${unprobed.length}`);

test('the mock lists probes only when the query asks for them, as the route does', async () => {
  const probes = all.filter((r) => r.kind === 'probe');
  assert.ok(probes.length > 0, 'the story holds probes');
  assert.deepEqual((await api.listRequests({})).requests.filter((r) => r.kind === 'probe').map((r) => r.id), [], 'the first page of an unasked list holds no probe');
  assert.deepEqual(await listed({}), ids(unprobed), 'an unasked list holds every other row');
  assert.deepEqual(await listed({ includeProbes: true }), ids(all), 'a list that includes probes holds them');
  assert.deepEqual(await listed({ kind: 'probe' }), ids(probes), 'a list of the probe kind holds them alone');
});

test('the mock keeps a request under the cache miss reason of its serving attempt, as the route does', async () => {
  const expected = unprobed.filter((r) => r.attempts.at(-1)?.cacheMissReason === 'none');
  narrowed(expected, 'whose serving attempt reads none');
  assert.deepEqual(await listed({ cacheMissReasons: ['none'] }), ids(expected));
});

test("the mock keeps a request under its serving attempt's stop reason and refusal category, the dash for none, as the route does", async () => {
  const served = (r) => r.attempts.at(-1)?.response ?? null;
  const refused = unprobed.filter((r) => served(r)?.stopReason === 'refusal');
  narrowed(refused, 'whose serving attempt the upstream refused');
  assert.deepEqual(await listed({ stopReasons: ['refusal'] }), ids(refused));
  const cyber = unprobed.filter((r) => served(r)?.refusalCategory === 'cyber');
  narrowed(cyber, 'refused under cyber');
  assert.deepEqual(await listed({ refusalCategories: ['cyber'] }), ids(cyber));
  const uncategorized = unprobed.filter((r) => (served(r)?.refusalCategory ?? null) === null);
  narrowed(uncategorized, 'with no refusal category or no serving attempt');
  assert.deepEqual(await listed({ refusalCategories: ['-'] }), ids(uncategorized));
  const bare = refused.filter((r) => served(r).refusalCategory === null);
  assert.equal(bare.length, 1, 'the story holds one refusal the upstream gave no category');
  assert.deepEqual(await listed({ refusalCategories: ['-'], stopReasons: ['refusal'] }), ids(bare));
});

/** The refusal count over the story's last day under `lists`, as the page reads it. */
const refusalCount = (lists) => api.getAnalyticsRefusalCategories({ accountIds: [], from: new Date(Date.parse(AT) - 24 * 3_600_000).toISOString(), groupBy: 'all', includeProbes: false, keyIds: [], models: [], poolIds: [], to: AT, ...lists });
const refusalsIn = (answer) => answer.rows.reduce((n, r) => n + r.requests, 0);

test('the mock narrows the refusal count by the client and user agent patterns, the agent folded to its product token, and the user agent groups narrow nothing', async () => {
  // The story's six refused answers, the laptop's owed turns in `mock/requests.ts`, are all on `claude-code/cli` under
  // `claude-cli/2.0.44 (external, cli)`.
  const whole = refusalsIn(await refusalCount({}));
  assert.equal(whole, 6);
  assert.equal(refusalsIn(await refusalCount({ clients: ['claude-code/sdk-cli'] })), 0, 'no refused answer came from the sdk-cli client');
  assert.equal(refusalsIn(await refusalCount({ clients: ['claude-code/cli'] })), whole);
  assert.equal(refusalsIn(await refusalCount({ userAgents: ['claude-cli/2.0.44'] })), whole, "the laptop agent's token keeps its refusals");
  assert.equal(refusalsIn(await refusalCount({ userAgents: ['claude-cli/2.1.*'] })), 0, 'no refused answer came from a 2.1 build');
  assert.equal(refusalsIn(await refusalCount({ userAgentGroups: ['claude-cli/2.1.*'] })), whole, 'the groups partition the rows and keep every one');
});

test("a refusal bar's link carries the client pattern its count was read under and lists the rows the count counted", async () => {
  const now = Date.parse(AT);
  const from = now - 24 * 3_600_000;
  const pattern = 'claude-code/cli';
  const count = await refusalCount({ clients: [pattern] });
  // Three of the laptop's owed turns refused under cyber, every one on the pattern's client.
  const cyber = count.rows.find((r) => r.category === 'cyber');
  assert.equal(cyber?.requests, 3, JSON.stringify(count.rows));
  const counted = unprobed.filter((r) => Date.parse(r.at) >= from && Date.parse(r.at) <= now && d.matchesPattern(pattern, r.client ?? '-') && r.canonicalModel === cyber.model && r.attempts.at(-1)?.response?.stopReason === 'refusal' && r.attempts.at(-1).response.refusalCategory === 'cyber');
  const input = { filters: { accountIds: [], clientPattern: pattern, includeProbes: false, keyIds: [], models: [], poolIds: [], sessionId: null, userAgentPattern: '' }, window: { exact: false, fromMs: from, nowMs: now, toMs: now } };
  // The address read back as the Requests page reads it into its list query.
  const read = d.REQUESTS_CODEC.fromParams(new URL(d.refusalLink(input, 'cyber', cyber.requests, cyber.model).href, 'http://console.test').searchParams, now);
  assert.equal(read.client, pattern);
  const linked = await listed({ accountIds: read.accounts, client: read.client, from: read.range.from, includeProbes: read.kind === 'probe', keyIds: read.keys, models: read.models, poolIds: read.pools, refusalCategories: read.refusalCategories, session: read.session || undefined, stopReasons: read.stopReasons, to: read.range.to });
  assert.deepEqual(linked, ids(counted));
  assert.equal(linked.length, cyber.requests);
});

test('the mock leaves the stamp uncapped under a fixed end, so a row received inside the range and written after it is read once, as the route does', async () => {
  // The seed is the failed turn `seedHolds` (mock/requests.ts) writes, which waited out its 60 s pool's whole bound at
  // the cap after its attempts, so it finishes more than a minute after it arrived, and the ledger writes a row at its
  // finish: an end between the two holds the row received and not yet written at the end.
  const late = all.find((r) => r.errorCode === 'accounts_at_cap' && r.durationMs > 60_000);
  assert.ok(late, 'the story holds a failed turn that waited out its bound at the cap');
  const arrived = Date.parse(late.at);
  const finished = arrived + late.durationMs;
  const from = new Date(arrived - 60_000).toISOString();
  const to = new Date(arrived + 30_000).toISOString();
  const fixed = await api.listRequests({ fixedEnd: true, from, limit: 100_000, to });
  assert.ok(fixed.requests.some((r) => r.id === late.id), 'the row arrived inside the range and the mock clock is past its write');
  assert.ok(fixed.landedThrough >= finished, `the stamp ${fixed.landedThrough} reaches the row's finish ${finished}, past the end`);
  const capped = await api.listRequests({ from, limit: 100_000, to });
  assert.ok(capped.landedThrough <= Date.parse(to), `the stamp ${capped.landedThrough} stops at the end ${Date.parse(to)} without the flag`);
  const next = await api.listRequests({ fixedEnd: true, from, landedAfter: fixed.landedThrough, limit: 100_000, to });
  assert.ok(!next.requests.some((r) => r.id === late.id), 'a read from the stamp brings the row no more');
});

test('the mock keeps a request under its effort folded as the rollups file it', async () => {
  const expected = unprobed.filter((r) => r.shape.effort === null);
  narrowed(expected, 'without an effort');
  assert.deepEqual(await listed({ efforts: ['none'] }), ids(expected));
});

test('the mock keeps a request under its class folded as the rollups file it', async () => {
  const expected = unprobed.filter((r) => r.requestClass === null);
  narrowed(expected, 'without a class');
  assert.deepEqual(await listed({ requestClasses: ['-'] }), ids(expected));
});

test('the mock keeps a request under a guardrail that matched it, and under none where no guardrail matched, as the route does', async () => {
  const matched = unprobed.find((r) => r.guardrails.length > 0);
  assert.ok(matched, 'the story holds a row a guardrail matched');
  const id = matched.guardrails.at(-1).id;
  const carrying = unprobed.filter((r) => r.guardrails.some((m) => m.id === id));
  narrowed(carrying, `that ${id} matched`);
  assert.deepEqual(await listed({ guardrails: [id] }), ids(carrying));
  const none = unprobed.filter((r) => r.guardrails.length === 0);
  narrowed(none, 'that no guardrail matched');
  assert.deepEqual(await listed({ guardrails: ['-'] }), ids(none));
  assert.deepEqual(await listed({ guardrails: [id, '-'] }), ids([...carrying, ...none]), 'an id and none keep the rows of either');
});

test('the mock keeps a request under its canonical model, not its wire spelling, as the route compares it', async () => {
  const resolved = unprobed.find((r) => r.model !== r.canonicalModel);
  assert.ok(resolved, 'the story holds a request whose wire spelling differs from its canonical model');
  const expected = unprobed.filter((r) => r.canonicalModel === resolved.canonicalModel);
  narrowed(expected, `under ${resolved.canonicalModel}`);
  assert.deepEqual(await listed({ models: [resolved.canonicalModel] }), ids(expected));
  assert.deepEqual(await listed({ models: [resolved.canonicalModel.toUpperCase()] }), ids(expected), 'the route lowercases the selection');
});

test('the mock lists a request that names no account under the unattributed id', async () => {
  const expected = unprobed.filter((r) => r.accountId === null);
  narrowed(expected, 'without an account');
  assert.deepEqual(await listed({ accountIds: ['-'] }), ids(expected));
});

test('the mock lists the rows inside a from and to window', async () => {
  const [from, to] = ['2026-09-16T12:00:00.000Z', '2026-09-16T13:00:00.000Z'];
  const expected = unprobed.filter((r) => Date.parse(r.at) >= Date.parse(from) && Date.parse(r.at) <= Date.parse(to));
  narrowed(expected, 'inside 12:00 to 13:00');
  assert.deepEqual(await listed({ from, to }), ids(expected));
});

test("listRequests orders by a nullable key with the rows without a figure last under either direction and names the last row's figure and id in its cursor", async () => {
  // The story's rows from 14:15 to 14:18: the compacted laptop turn of 3,300 ms, bench's Fable turn of 9,800 ms, and
  // three requests the router answered before any attempt, which carry no duration.
  const page = async (dir, cursor) => {
    const p = await api.listRequests({ cursor, dir, from: '2026-09-16T14:15:00.000Z', limit: 2, order: 'duration_ms', to: '2026-09-16T14:18:00.000Z' });
    return [p.requests.map((r) => r.id), p.nextCursor];
  };
  assert.deepEqual(await page('asc'), [['req_31f0b7c2-6e1', 'req_1c7e4a9d-f52'], '9800:req_1c7e4a9d-f52']);
  assert.deepEqual(await page('asc', '9800:req_1c7e4a9d-f52'), [['req_2d8e5b13-f40', 'req_51d0e2b8-7a0'], ':req_51d0e2b8-7a0']);
  assert.deepEqual(await page('asc', ':req_51d0e2b8-7a0'), [['req_9b4f1c62-a07'], null]);
  assert.deepEqual(await page('desc'), [['req_1c7e4a9d-f52', 'req_31f0b7c2-6e1'], '3300:req_31f0b7c2-6e1']);
  assert.deepEqual(await page('desc', '3300:req_31f0b7c2-6e1'), [['req_9b4f1c62-a07', 'req_51d0e2b8-7a0'], ':req_51d0e2b8-7a0']);
  assert.deepEqual(await page('desc', ':req_51d0e2b8-7a0'), [['req_2d8e5b13-f40'], null]);
  // A token key reads the serving attempt's count, so the three rows no attempt served carry none and close the page.
  const tokens = await api.listRequests({ dir: 'asc', from: '2026-09-16T14:15:00.000Z', limit: 5, order: 'input_tokens', to: '2026-09-16T14:18:00.000Z' });
  assert.deepEqual(
    tokens.requests.map((r) => r.id),
    ['req_31f0b7c2-6e1', 'req_1c7e4a9d-f52', 'req_2d8e5b13-f40', 'req_51d0e2b8-7a0', 'req_9b4f1c62-a07'],
  );
});

test("the request list and the routing log answer a cursor their route's parser refuses with its 400, and a cursor the log wrote still turns its page", async () => {
  const refused = (message) => (e) => e instanceof d.ApiError && e.status === 400 && e.message === message;
  // A figure that is no number, one `Number` reads where the route's integer parse does not, an empty figure under the
  // default order, whose rows all carry one, and no separator.
  for (const cursor of ['abc:req_1', '1e3:req_1', ':req_1', 'req_1']) {
    await assert.rejects(api.listRequests({ cursor }), refused('cursor is <value>:<id>'), cursor);
  }
  // An instant that is no number, one separator, a rank other than 0 or 1, a decision id with no `/` and an event id
  // that is no number.
  for (const cursor of ['abc:1:req_1/1', '123:1', '100:2:r1/1', '100:1:req', '100:0:x']) {
    await assert.rejects(api.listRoutingLog({ cursor }), refused('cursor is <at>:<rank>:<id>'), cursor);
  }
  // The log's default range is the wall clock's last hour, which holds none of the story's rows.
  const from = new Date(Date.parse(AT) - 3_600_000).toISOString();
  const four = await api.listRoutingLog({ from, limit: 4, to: AT });
  const first = await api.listRoutingLog({ from, limit: 2, to: AT });
  assert.ok(first.nextCursor !== null, 'a page of two rows out of the hour offers a cursor');
  const next = await api.listRoutingLog({ cursor: first.nextCursor, from, limit: 2, to: AT });
  assert.deepEqual(next.rows, four.rows.slice(2), "the page under the cursor holds the hour's third and fourth rows");
});

test('the mock answers a request id with the one row, whole and never by prefix', async () => {
  const [row] = unprobed;
  assert.deepEqual(await listed({ request: row.id }), [row.id]);
  assert.deepEqual(await listed({ request: row.id.slice(0, 6) }), [], 'a prefix of an id is no id');
});

test("the mock answers a session id with the session's rows beside the search", async () => {
  const shared = (id) => unprobed.filter((r) => r.sessionId === id);
  const session = unprobed.map((r) => r.sessionId).find((id) => id !== null && shared(id).length > 1);
  assert.ok(session, 'the story holds a session two listed rows share');
  const expected = shared(session);
  narrowed(expected, `in session ${session}`);
  assert.deepEqual(await listed({ session }), ids(expected));
  const search = expected[0].model;
  const searched = await listed({ search });
  const inside = new Set(ids(expected));
  assert.ok(searched.some((id) => !inside.has(id)), `the search ${search} also hits rows outside the session`);
  assert.deepEqual(await listed({ search, session }), searched.filter((id) => inside.has(id)), 'the session and the search combine');
});

test("the mock answers an agent id with the agent's rows on both logs, an event naming no agent left out", async () => {
  const mains = unprobed.filter((r) => r.agentId === 'main');
  narrowed(mains, 'of the main agent');
  assert.deepEqual(await listed({ agentId: 'main' }), ids(mains));
  const subagent = unprobed.find((r) => r.agentId !== 'main' && r.agentId !== null)?.agentId;
  assert.ok(subagent, 'the story holds a subagent row');
  const own = unprobed.filter((r) => r.agentId === subagent);
  narrowed(own, `of subagent ${subagent}`);
  assert.deepEqual(await listed({ agentId: subagent }), ids(own));
  // Unasked, the mock reads the wall clock's last hour, which holds none of the story's rows.
  const from = new Date(Date.parse(AT) - 3_600_000).toISOString();
  const to = new Date(Date.parse(AT)).toISOString();
  const hour = await api.listRoutingLog({ from, limit: 200, to });
  assert.equal(hour.nextCursor, null, 'one page holds the hour');
  assert.ok(hour.rows.some((r) => r.type === 'event' && r.agentId === null), 'the hour holds an event that names no agent');
  const key = (r) => (r.type === 'decision' ? `${r.requestId}/${r.seq}` : `event/${r.id}`);
  for (const agentId of ['main', subagent]) {
    const kept = hour.rows.filter((r) => r.agentId === agentId);
    assert.ok(kept.length > 0 && kept.length < hour.rows.length, `the hour holds ${kept.length} rows of ${agentId} of ${hour.rows.length}`);
    const page = await api.listRoutingLog({ agentId, from, limit: 200, to });
    assert.ok(page.rows.every((r) => r.agentId === agentId), `every row of the ${agentId} page carries its agent id`);
    assert.deepEqual(page.rows.map(key), kept.map(key), `the ${agentId} page is the hour's rows of ${agentId}`);
  }
});

test('the mock leaves out a session no mod reports once its last use is older than the non-companion timeout, as the route does', async () => {
  const now = Date.parse(AT);
  const HOUR_S = 3_600;
  const TWO_HOURS_BACK = 'e5a17c3d-8f20-4b6e-9d41-2c7b0a9f6e58';
  const key = (s) => `${s.keyId}|${s.keyGeneration}|${s.sessionId}`;
  const named = (s) => `${s.keyId} on ${s.accountId ?? 'no account'} ${s.sessionId.slice(0, 8)}${s.historical ? ' earlier holder' : ''}`;
  const back = (s) => (d.lastActivity(s) === null ? 'no use on record' : `${Math.round((now - Date.parse(d.lastActivity(s))) / 60_000)} min back`);
  const sessions = await api.listSessions();
  const plain = sessions.filter((s) => s.companion === 'none');
  const overflow = plain.find((s) => s.keyId === 'desktop' && s.accountId === 'overflow');
  const pinless = plain.find((s) => s.keyId === 'laptop' && s.accountId === 'secondary' && !s.pinned);
  assert.ok(overflow && pinless, `the unpinned rows on overflow and secondary stay listed, their last use inside the hour: ${plain.map((s) => `${named(s)} (${back(s)})`).join(', ')}`);
  // The laptop row the ledger never saw holds no pin and no request: a null last use is never older than the timeout.
  const unused = plain.filter((s) => d.lastActivity(s) === null);
  assert.deepEqual(unused.map((s) => `${s.keyId} on ${s.accountId}`), ['laptop on nightly'], 'one row no mod reports has no use on record and is listed at any instant');
  assert.ok(d.sessionsListedAt(now, undefined, 10_800).some((s) => s.sessionId === TWO_HOURS_BACK), 'a three-hour timeout lists the row last used two hours back');
  assert.ok(!d.sessionsListedAt(now, undefined, HOUR_S).some((s) => s.sessionId === TWO_HOURS_BACK), 'the one-hour timeout leaves it out');
  const later = d.sessionsListedAt(now + 2 * HOUR_S * 1000);
  assert.deepEqual(later.filter((s) => s.companion === 'none').map(named), unused.map(named), 'two hours on, no row no mod reports with a use on record stays listed');
  assert.deepEqual(later.filter((s) => s.companion !== 'none').map(key), sessions.filter((s) => s.companion !== 'none').map(key), "every companion row of the story's list still stands two hours on");
});

test('the mock answers the keepalive overview over 7 days by default and over 30 under its range, and refuses any other word, as the route does', async () => {
  const bare = await api.getKeepaliveOverview();
  const week = await api.getKeepaliveOverview({ range: '7d' });
  const month = await api.getKeepaliveOverview({ range: '30d' });
  assert.deepEqual(week, bare, 'the default range is 7 days');
  const rest = (o) => ({ ...o, pools: o.pools.map(({ idleReturns: _idle, ...p }) => p) });
  assert.deepEqual(rest(month), rest(week), 'the range moves the idle returns alone');
  const total = (o, member) => o.pools.flatMap((p) => p.idleReturns).reduce((n, b) => n + b[member], 0);
  for (const [i, pool] of month.pools.entries()) {
    const seven = week.pools[i].idleReturns;
    const larger = pool.idleReturns.filter((b, j) => b.fromSeconds !== seven[j].fromSeconds || b.reached <= seven[j].reached || b.returned <= seven[j].returned);
    assert.deepEqual(larger, [], `${pool.poolId}: every bucket reaches and returns more windows over 30 days`);
  }
  assert.ok(total(month, 'reached') > total(week, 'reached') && total(month, 'returned') > total(week, 'returned'), 'the 30-day totals are larger');
  await assert.rejects(api.getKeepaliveOverview({ range: '14d' }), (e) => e instanceof d.ApiError && e.status === 400 && e.message === 'range is 7d or 30d');
});

test('under the empty-store switch a captured-bodies read answers null, as a fresh router holds no request', async () => {
  // A desktop request of the story whose bodies were captured, `mock/requests.ts`.
  const id = 'req_a6c19e73-5d2';
  // The switch lives in the browser's storage, so the case lends the mock a stand-in.
  const stored = new Map([['phoebis.mockEmpty', '1']]);
  globalThis.localStorage = { getItem: (key) => stored.get(key) ?? null };
  try {
    assert.equal(await api.getCapturedBodies(id), null);
  } finally {
    delete globalThis.localStorage;
  }
  assert.notEqual(await api.getCapturedBodies(id), null, 'without the switch the story answers the bodies');
});
