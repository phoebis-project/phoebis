import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

test("the Request shape row names the request's auto-mode standing", () => {
  assert.deepEqual(
    [d.autoModeLabel('server'), d.autoModeLabel('stripped'), d.autoModeLabel('client_stage1'), d.autoModeLabel('client_stage2'), d.autoModeLabel(null)],
    ['server-side check requested', "server-side check dropped by this key's setting", 'client classifier, stage 1', 'client classifier, stage 2', 'not requested'],
  );
});

test("a server-side check's verdict reads in its own words, an unavailable one with its reason", () => {
  const words = ['not_flagged', 'flagged', 'skipped', 'unsupported', 'none', 'unrecognized'].map((v) => d.verdictLabel('server', v, null));
  assert.deepEqual(words, ['not flagged', 'flagged', 'skipped', 'unsupported', 'no verdict returned', 'unrecognized result']);
  assert.equal(d.verdictLabel('server', 'unavailable', 'timeout'), 'unavailable (timeout)');
  assert.equal(d.verdictLabel('server', 'unavailable', null), 'unavailable');
});

test("a classifier request's verdict reads in its own words, a refusal with its category", () => {
  for (const stage of ['client_stage1', 'client_stage2']) {
    const words = ['not_flagged', 'flagged', 'unreadable'].map((v) => d.verdictLabel(stage, v, null));
    assert.deepEqual(words, ['not flagged', 'flagged', 'unreadable answer'], stage);
  }
  assert.equal(d.verdictLabel('client_stage2', 'refused', 'reasoning_extraction'), 'refused (reasoning_extraction)');
  assert.equal(d.verdictLabel('client_stage2', 'flagged', 'destructive'), 'flagged (destructive)', "a stage-2 category rides the verdict's words");
});

test('a verdict with no words for its check reads as sent, with its detail', () => {
  assert.equal(d.verdictLabel('server', 'maybe', null), 'maybe');
  assert.equal(d.verdictLabel('server', 'maybe', 'why'), 'maybe (why)');
  assert.equal(d.verdictLabel('client_stage1', 'none', null), 'none', "the server's words are not a classifier request's");
  assert.equal(d.verdictLabel('server', 'unreadable', null), 'unreadable', "a classifier request's words are not the server's");
  assert.equal(d.verdictLabel(null, 'flagged', null), 'flagged');
  assert.equal(d.verdictLabel('server', 'constructor', null), 'constructor', 'a name every object inherits is a name like any other');
});
