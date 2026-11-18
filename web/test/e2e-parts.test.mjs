import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { PassThrough } from 'node:stream';
import { test } from 'node:test';
import { fanOut, selectParts } from '../e2e/parts.mjs';

const WALK_KEYS = ['light', 'dark', 'light-github', 'dark-github'];

/**
 * A child process as `spawn` answers one: its two output streams, and the one event it ends on. The event fires on the
 * turn after the start, once the fan-out listens, as a real child's does.
 */
const child = (event, ...args) => {
  const fake = new EventEmitter();
  fake.stdout = new PassThrough();
  fake.stderr = new PassThrough();
  setImmediate(() => fake.emit(event, ...args));
  return fake;
};
const closing = (code) => child('close', code, null);

test('the fan-out fails the pass where one child fails, never starts or ends on a signal', async () => {
  assert.equal((await fanOut(WALK_KEYS, (part) => closing(part === 'light-github' ? 1 : 0))).status, 1);
  assert.equal((await fanOut(WALK_KEYS, () => closing(0))).status, 0);
  assert.equal((await fanOut(WALK_KEYS, (part) => (part === 'dark' ? child('error', new Error('spawn ENOENT')) : closing(0)))).status, 1);
  // A walk the host killed under memory pressure closes with no code.
  const killed = await fanOut(WALK_KEYS, (part) => (part === 'dark-github' ? child('close', null, 'SIGKILL') : closing(0)));
  assert.notEqual(killed.status, 0);
  assert.match(killed.runs.find((run) => run.part === 'dark-github').output.map(({ chunk }) => chunk).join(''), /SIGKILL/);
  // A spawn the host refuses for memory throws in place of answering a child.
  const refused = await fanOut(WALK_KEYS, (part) => {
    if (part === 'light-github') throw new Error('spawn ENOMEM');
    return closing(0);
  });
  assert.deepEqual([refused.status, refused.runs.find((run) => run.part === 'light-github').output.map(({ chunk }) => chunk).join('')], [1, 'the browser pass of light-github did not start: spawn ENOMEM\n']);
  // Each child ends on the turn after its start and reads the latch of starts as it ends, failing where a part has yet
  // to start, so a fan-out that waited on one child before the next start fails the pass.
  let starts = 0;
  const latched = await fanOut(WALK_KEYS, () => {
    starts += 1;
    const fake = Object.assign(new EventEmitter(), { stderr: new PassThrough(), stdout: new PassThrough() });
    setImmediate(() => fake.emit('close', starts === WALK_KEYS.length ? 0 : 1, null));
    return fake;
  });
  assert.equal(latched.status, 0, 'every part started before a child ended');
});

test('the selection answers the parts its words name and throws on a word that names none', () => {
  assert.deepEqual(selectParts('light,rest', WALK_KEYS), ['light', 'rest']);
  // A part named twice would run twice into one shot directory.
  assert.deepEqual(selectParts('rest,light,light', WALK_KEYS), ['light', 'rest']);
  assert.deepEqual(selectParts(undefined, WALK_KEYS), ['light', 'dark', 'light-github', 'dark-github', 'rest']);
  assert.throws(() => selectParts('ligth', WALK_KEYS), /ligth/);
});
