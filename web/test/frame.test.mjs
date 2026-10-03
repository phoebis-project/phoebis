import assert from 'node:assert/strict';
import { test } from 'node:test';
import { coalesce } from './dist/api.js';

/** A frame clock the test runs by hand: `frame()` runs every callback queued since the last one. */
function manualFrames() {
  let next = 1;
  const queued = new Map();
  return {
    cancel: (id) => queued.delete(id),
    frame: () => {
      const due = [...queued.values()];
      queued.clear();
      for (const cb of due) cb();
    },
    pending: () => queued.size,
    schedule: (cb) => {
      const id = next;
      next += 1;
      queued.set(id, cb);
      return id;
    },
  };
}

test('many values inside one frame deliver once with the last', () => {
  const frames = manualFrames();
  const delivered = [];
  const view = coalesce((v) => delivered.push(v), frames.schedule, frames.cancel);
  for (const v of [1, 2, 3, 4, 5]) view.push(v);
  assert.deepEqual(delivered, []);
  assert.equal(frames.pending(), 1);
  frames.frame();
  assert.deepEqual(delivered, [5]);
  view.push(6);
  view.push(7);
  frames.frame();
  assert.deepEqual(delivered, [5, 7]);
  // A frame with nothing pushed since the last one delivers nothing.
  frames.frame();
  assert.deepEqual(delivered, [5, 7]);
});

test('flush delivers now and cancel drops a pending value', () => {
  const frames = manualFrames();
  const delivered = [];
  const view = coalesce((v) => delivered.push(v), frames.schedule, frames.cancel);
  view.push('a');
  view.flush();
  assert.deepEqual(delivered, ['a']);
  // The frame the push asked for still runs, and finds nothing left to deliver.
  frames.frame();
  assert.deepEqual(delivered, ['a']);
  view.push('b');
  view.cancel();
  assert.equal(frames.pending(), 0);
  frames.frame();
  assert.deepEqual(delivered, ['a']);
  view.push('c');
  frames.frame();
  assert.deepEqual(delivered, ['a', 'c']);
});
