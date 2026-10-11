import assert from 'node:assert/strict';
import { test } from 'node:test';

// The mock skips its draws by the local hour, so the file runs in UTC whatever the host's zone; `node --test` gives each
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
