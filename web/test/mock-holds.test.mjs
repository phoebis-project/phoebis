import assert from 'node:assert/strict';
import { test } from 'node:test';

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
for (const at of ['2026-09-30T18:07:00Z', '2026-09-29T09:07:00Z']) {
  test(`a request held after its attempts carries every hold they named plus the bound it waited out, at ${at}`, async () => {
    const api = new (await storyAt(at)).MockApi();
    const { requests } = await api.listRequests({ held: true, limit: 1_000 });
    const row = requests.find((r) => r.attempts.length > 0 && r.errorCode === 'accounts_at_cap');
    assert.ok(row, 'the story holds a request answered at the cap after an attempt');
    const traced = [...row.attempts].reverse().find((a) => a.selection?.cohortHold)?.selection.cohortHold ?? null;
    const bound = (await api.listPools()).find((p) => p.id === row.poolId).retryInPlaceSeconds * 1_000;
    const last = row.held.waitedOn.at(-1);
    assert.deepEqual(row.held, { cohort: null, heldMs: (traced?.heldMs ?? 0) + bound, waitedOn: [...(traced?.waitedOn ?? []), last] });
    assert.ok(!row.attempts.some((a) => a.accountId === last), 'the last wait is on an account the request had not tried');
    assert.ok(row.attempts.every((a) => a.selection?.cohortHold?.heldMs !== row.held.heldMs), "no attempt's trace names the whole wait");
    assert.equal(row.status, 429);
  });
}

// In UTC this minute's at-cap row, refused before any attempt, is a desktop Opus turn, which asked for fast.
test('a request refused at the cap before any attempt lists under the fast speed it asked for and not under standard', async () => {
  const api = new (await storyAt('2026-09-30T20:07:00Z')).MockApi();
  const { requests } = await api.listRequests({ held: true, limit: 1_000 });
  const row = requests.find((r) => r.attempts.length === 0 && r.errorCode === 'accounts_at_cap');
  assert.equal(row?.shape.requestedSpeed, 'fast', 'the story holds an at-cap row that asked for fast');
  const listed = async (speed) => (await api.listRequests({ limit: 1_000, speeds: [speed] })).requests.some((r) => r.id === row.id);
  assert.deepEqual([await listed('fast'), await listed('standard')], [true, false]);
});
