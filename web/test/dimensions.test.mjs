import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

test('an effort folds as the rollups file it: none for none, a level as itself, anything else as other', () => {
  assert.equal(d.foldEffort(null), d.NO_EFFORT);
  assert.deepEqual(d.EFFORT_LEVELS, ['low', 'medium', 'high', 'xhigh', 'max'], "the API's five levels in their order");
  for (const level of [...d.EFFORT_LEVELS, d.NO_EFFORT]) assert.equal(d.foldEffort(level), level, `${level} reads as itself`);
  for (const raw of ['HIGH', 'ultra']) assert.equal(d.foldEffort(raw), d.OTHER_DIMENSION, `${raw} reads as other`);
});

test('a request class folds as the rollups file it: - for none, an identifier up to 64 bytes as itself, anything else as other', () => {
  assert.equal(d.foldRequestClass(null), d.NO_CLASS);
  for (const raw of ['main', 'a/b:c.d_e-f', 'x'.repeat(64)]) assert.equal(d.foldRequestClass(raw), raw, `${raw.length} bytes of ${raw.slice(0, 12)} read as itself`);
  for (const raw of ['bad class', 'x'.repeat(65), 'café']) assert.equal(d.foldRequestClass(raw), d.OTHER_DIMENSION, `${raw.slice(0, 12)} reads as other`);
});

test("the Agent dimension leads the Subject group on both logs with the word agent, apart from the user agent's word", () => {
  assert.deepEqual(d.AGENT_ID, { field: 'agentId', group: 'Subject', kind: 'id', label: 'Agent', mono: true, word: 'agent' });
  for (const [view, dimensions] of [['requests', d.REQUESTS_DIMENSIONS], ['routingLog', d.ROUTING_LOG_DIMENSIONS]]) {
    assert.deepEqual(dimensions.slice(-3).map((x) => x.field), ['agentId', 'request', 'session'], `${view}: the list ends on the Subject group`);
    assert.deepEqual(dimensions.filter((x) => x.group === 'Subject').map((x) => x.field), ['agentId', 'request', 'session'], `${view}: the Subject group`);
    assert.equal(dimensions.at(-3), d.AGENT_ID, `${view}: the one shared dimension`);
    assert.equal(new Set(dimensions.map((x) => x.word)).size, dimensions.length, `${view}: no two dimensions share a word`);
  }
  assert.equal(d.REQUESTS_DIMENSIONS.find((x) => x.field === 'agent')?.word, 'user agent');
});

test("the Sessions view's filters are its three flags in the bar's order", () => {
  assert.deepEqual(
    d.SESSIONS_DIMENSIONS.map((x) => [x.field, x.kind, x.label]),
    [
      ['companion', 'flag', 'Companion only'],
      ['finished', 'flag', 'Finished sessions'],
      ['released', 'flag', 'Released agents'],
    ],
  );
});
