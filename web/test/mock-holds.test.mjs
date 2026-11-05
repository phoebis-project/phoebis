import assert from 'node:assert/strict';
import { test } from 'node:test';

// The mock skips its draws by the local hour and every minute below is chosen in UTC, so the file runs there whatever
// the host's zone; `node --test` gives each file a process of its own.
process.env.TZ = 'UTC';

// The mock draws its story from the clock it loads at, so each minute loads a copy of the bundle of its own.
const storyAt = async (at) => {
  const wall = Date.now;
  Date.now = () => Date.parse(at);
  try {
    return await import(`./dist/api.js?at=${at}`);
  } finally {
    Date.now = wall;
  }
};

// In UTC, the first minute draws a turn whose attempt's trace already held; the second draws no failed turn with one
// attempt, so a turn that failed over takes the story.
const CLOCKS = [
  ['2026-09-28T21:35:00Z', 'a traced hold'],
  ['2026-09-29T04:39:00Z', 'a failover'],
];

for (const [at, branch] of CLOCKS) {
  test(`a request held after its attempts carries every hold they named plus the bound it waited out, at ${at}`, async () => {
    const api = new (await storyAt(at)).MockApi();
    const { requests } = await api.listRequests({ held: true, limit: 1_000 });
    const row = requests.find((r) => r.attempts.length > 0 && r.errorCode === 'accounts_at_cap');
    assert.ok(row, 'the story holds a request answered at the cap after an attempt');
    const traced = [...row.attempts].reverse().find((a) => a.selection?.cohortHold)?.selection.cohortHold ?? null;
    assert.ok(branch === 'a traced hold' ? traced !== null : row.attempts.length > 1, `the minute draws ${branch}`);
    const bound = (await api.listPools()).find((p) => p.id === row.poolId).retryInPlaceSeconds * 1_000;
    const last = row.held.waitedOn.at(-1);
    assert.deepEqual(row.held, { cohort: null, heldMs: (traced?.heldMs ?? 0) + bound, waitedOn: [...(traced?.waitedOn ?? []), last], waits: [...(traced?.waits ?? []), { heldMs: bound, reason: 'cap' }] });
    assert.ok(!row.attempts.some((a) => a.accountId === last), 'the last wait is on an account the request had not tried');
    assert.ok(row.attempts.every((a) => a.selection?.cohortHold?.heldMs !== row.held.heldMs), "no attempt's trace names the whole wait");
    assert.equal(row.status, 429);
  });
}

// In UTC this minute's at-cap row, refused before any attempt, is a desktop Opus turn, which asked for fast.
test('a request refused at the cap before any attempt lists under the fast speed it asked for and not under standard', async () => {
  const api = new (await storyAt('2026-09-30T10:02:00Z')).MockApi();
  const { requests } = await api.listRequests({ held: true, limit: 1_000 });
  const row = requests.find((r) => r.attempts.length === 0 && r.errorCode === 'accounts_at_cap');
  assert.equal(row?.shape.requestedSpeed, 'fast', 'the story holds an at-cap row that asked for fast');
  const listed = async (speed) => (await api.listRequests({ limit: 1_000, speeds: [speed] })).requests.some((r) => r.id === row.id);
  assert.deepEqual([await listed('fast'), await listed('standard')], [true, false]);
});

// The routing log's story holds one subagent request on its cohort's account and places it there once a slot frees,
// at whichever clock the mock loads.
for (const [at] of CLOCKS) {
  test(`the mock's held records agree with the trail, at ${at}`, async () => {
    const api = new (await storyAt(at)).MockApi();
    const { requests } = await api.listRequests({ held: true, limit: 1_000 });
    const row = requests.find((r) => r.id === 'req_69e30548-32e');
    assert.ok(row, "the story's held request lists among the held");
    const [hold, placement] = await Promise.all([api.getDecision(row.id, 1), api.getDecision(row.id, 2)]);
    assert.deepEqual([hold.outcome.kind, placement.outcome.kind], ['held', 'placed']);
    assert.deepEqual([hold.request.attempt, placement.request.attempt], [1, 1], 'the hold and the placement after it are two decisions of one attempt');
    const bound = (await api.listPools()).find((p) => p.id === row.poolId).retryInPlaceSeconds * 1_000;
    assert.equal(hold.outcome.hold.boundMs, bound, "the hold's bound is the pool's");
    assert.equal(hold.outcome.hold.accountId, row.held.waitedOn.at(-1), 'the hold waits where the trail says it waited');
    assert.equal(placement.outcome.accountId, row.attempts[0].accountId, 'the placement dispatched the attempt the trail lists');
    assert.deepEqual(placement.outcome.cohortHold, row.held, "the placement carries the request's hold");
    assert.deepEqual(row.attempts[0].selection.cohortHold, row.held, "the attempt's trace names the hold the record placed after");
    const around = { from: new Date(Date.parse(row.at) - 15 * 60_000).toISOString(), to: new Date(Date.parse(row.at) + 15 * 60_000).toISOString() };
    const page = await api.listRoutingLog({ ...around, request: row.id });
    const decisions = page.rows.filter((r) => r.type === 'decision').sort((x, y) => x.seq - y.seq);
    assert.deepEqual(decisions.map((d) => [d.seq, d.kind]), [[1, 'held'], [2, 'placed']]);
    assert.deepEqual(decisions[1].fact, [{ entity: null, key: 'held', unit: 'ms', value: row.held.heldMs }], 'the placement after the hold reads the wait the trail names');
    assert.equal(page.counts.decisions, 2);
  });
}
