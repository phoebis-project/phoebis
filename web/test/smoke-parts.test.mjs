import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { PassThrough } from 'node:stream';
import { test } from 'node:test';
import { fanOut, jobsOf, joinReports, selectViews, VIEWS } from '../smoke/parts.mjs';

/**
 * A runner as `spawn` answers one with a channel: its two output streams, and the events it ends on, fired in order on
 * the turn after the start, once the fan-out listens, as a real child's are.
 */
const child = (events) => {
  const fake = new EventEmitter();
  fake.stdout = new PassThrough();
  fake.stderr = new PassThrough();
  setImmediate(() => {
    for (const [event, ...args] of events) fake.emit(event, ...args);
  });
  return fake;
};
const REPORT = { blocks: 1, checks: [[1, 'a', true]], peak: 1, ran: [1], rows: 1 };
const passing = () => child([['message', REPORT], ['disconnect'], ['close', 0, null]]);
const textOf = (run) => run.output.map(({ chunk }) => chunk).join('');

test('the fan-out holds no more runners alive at one time than its limit', async () => {
  let alive = 0;
  let peak = 0;
  const runs = await fanOut(['/a', '/b', '/c', '/d'], 2, () => {
    alive += 1;
    peak = Math.max(peak, alive);
    const fake = passing();
    fake.on('close', () => {
      alive -= 1;
    });
    return fake;
  });
  assert.equal(peak, 2);
  assert.deepEqual(runs.map((run) => run.view), ['/a', '/b', '/c', '/d']);
});

test('the cap takes a whole number from 1 on and defaults to the cores, never past the views', () => {
  assert.throws(() => jobsOf('0', VIEWS, 4), /SMOKE_JOBS=0/);
  assert.throws(() => jobsOf('two', VIEWS, 4), /SMOKE_JOBS=two/);
  assert.equal(jobsOf('1', VIEWS, 4), 1);
  assert.equal(jobsOf(undefined, VIEWS, 4), 4);
  assert.equal(jobsOf(undefined, ['/keys', '/pools'], 4), 2);
});

test('the fan-out fails a runner that closed with 1, never started, ended on a signal or sent no report', async () => {
  const views = ['/a', '/b', '/c', '/d'];
  const runOf = async (start) => (await fanOut(views, 4, (view) => (view === '/c' ? start() : passing())))[2];
  assert.deepEqual((await fanOut(views, 4, passing)).map((run) => run.status), [0, 0, 0, 0]);
  assert.notEqual((await runOf(() => child([['message', REPORT], ['disconnect'], ['close', 1, null]]))).status, 0);
  assert.notEqual((await runOf(() => child([['error', new Error('spawn ENOENT')]]))).status, 0);
  assert.notEqual((await runOf(() => {
    throw new Error('spawn E2BIG');
  })).status, 0);
  const killed = await runOf(() => child([['disconnect'], ['close', null, 'SIGKILL']]));
  assert.notEqual(killed.status, 0);
  assert.match(textOf(killed), /SIGKILL/);
  // A runner that exited ahead of its report, so its checks never reached the join.
  const silent = await runOf(() => child([['disconnect'], ['close', 0, null]]));
  assert.notEqual(silent.status, 0);
  assert.match(textOf(silent), /no report/);
});

test('the join keys a check by its block, name and occurrence, and counts a check that two runners pushed once', () => {
  // Block 2 pushes one result more in the second runner, so a key by place would pair checks of different names.
  const first = { blocks: 5, checks: [[1, 'a', true], [1, 'a', true], [2, 'b', true], [2, 'c', false], [0, 'guard', false]], peak: 1, ran: [1, 2], rows: 273 };
  const second = { blocks: 5, checks: [[2, 'd', true], [2, 'b', true], [2, 'c', false], [3, 'e', true], [0, 'guard', false]], peak: 1, ran: [2, 3], rows: 273 };
  assert.deepEqual(joinReports([first, second]), { blocks: 5, passed: 5, rows: 273, skipped: 2, total: 7 });
});

test('the selection keeps a path outside the list and answers the list where no word names a view', () => {
  assert.deepEqual(selectViews(['/keys', '/nope']), ['/keys', '/nope']);
  assert.deepEqual(selectViews([]), VIEWS);
});
