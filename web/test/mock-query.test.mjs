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
