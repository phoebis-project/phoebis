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

test('no two dimensions on either log share a word, so the agent reads apart from the user agent', () => {
  for (const [view, dimensions] of [['requests', d.REQUESTS_DIMENSIONS], ['routingLog', d.ROUTING_LOG_DIMENSIONS]]) {
    assert.equal(new Set(dimensions.map((x) => x.word)).size, dimensions.length, `${view}: no two dimensions share a word`);
  }
});

test('the Analytics Client and User agent filters draw a chip per value, a long value cut after 32 characters', () => {
  const of = (field) => d.ANALYTICS_DIMENSIONS.find((x) => x.field === field);
  const long = 'claude-cli/2.1.302 (external, sdk-cli) x';
  const chips = d.chipsOf([of('userAgents')], { userAgents: ['claude-cli/2.0.*', long] }, {});
  assert.deepEqual(chips.map((c) => [c.text, c.title, c.value]), [
    ['user agent: claude-cli/2.0.*', 'claude-cli/2.0.*', 'claude-cli/2.0.*'],
    [`user agent: ${long.slice(0, 32)}…`, long, long],
  ]);
  assert.deepEqual(d.chipText(of('clients'), [long]), { text: `client: ${long.slice(0, 32)}…`, title: long, values: `${long.slice(0, 32)}…` });
  assert.deepEqual(d.chipText(of('clients'), ['x'.repeat(32)]), { text: `client: ${'x'.repeat(32)}`, title: 'x'.repeat(32), values: 'x'.repeat(32) }, 'a value of 32 characters reads whole');
});
