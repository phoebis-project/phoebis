import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

const CLI = 'claude-cli/2.1.269 (external, cli)';
const SDK = 'claude-cli/2.1.269 (external, sdk-cli)';

test('a star matches any run and a question mark one character', () => {
  assert.ok(d.matchesPattern('claude-cli/*', CLI));
  assert.ok(d.matchesPattern('claude-cli/*', SDK));
  assert.ok(d.matchesPattern('*sdk-cli*', SDK));
  assert.ok(!d.matchesPattern('*sdk-cli*', CLI));
  assert.ok(d.matchesPattern('claude-cli/2.1.2?? (external, cli)', CLI));
  assert.ok(!d.matchesPattern('claude-cli/2.1.2? (external, cli)', CLI), 'one mark is one character');
  assert.ok(d.matchesPattern('*', ''), 'a star alone matches the empty value');
  assert.ok(!d.matchesPattern('?', ''), 'a mark needs a character');
  assert.ok(d.matchesPattern('CLAUDE-CLI/*', CLI), 'ASCII case folds as LIKE folds it');
  assert.ok(d.matchesPattern('*cli)', CLI), 'a star before a literal tail backtracks to the last fit');
  assert.ok(d.matchesPattern('a*b*c', 'aXbYbZc'), "a later star resumes after the earlier one's last fit");
  assert.ok(!d.matchesPattern('a*b*c', 'aXbYbZ'), 'and fails when the tail never lands');
});

test('a failing pattern of many stars answers in bounded steps', () => {
  const stars = d.matchesPatternCounted('*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*b', 'a'.repeat(30));
  assert.equal(stars.matched, false);
  assert.ok(stars.steps < 100_000, `${stars.steps} steps for a fifteen-star pattern over thirty characters`);
  const alternating = d.matchesPatternCounted('*?*?*?*?*?*?*?*?*?*?*?*?*?*?*?*?b', 'a'.repeat(32));
  assert.equal(alternating.matched, false);
  assert.ok(alternating.steps < 100_000, `${alternating.steps} steps for an alternating pattern over thirty-two characters`);
});

test('a pattern with no wildcard is an exact match', () => {
  assert.ok(d.matchesPattern(CLI, CLI));
  assert.ok(!d.matchesPattern(CLI, SDK));
  assert.ok(!d.matchesPattern('claude-cli/2.1.269', CLI), 'no implicit trailing star');
});

test('a backslash quotes the next character', () => {
  assert.ok(d.matchesPattern('a\\*b', 'a*b'));
  assert.ok(!d.matchesPattern('a\\*b', 'aXb'));
  assert.ok(d.matchesPattern('a\\?b', 'a?b'));
  assert.ok(!d.matchesPattern('a\\?b', 'aXb'));
  assert.ok(d.matchesPattern('a\\\\b', 'a\\b'));
  assert.ok(d.matchesPattern('a\\', 'a'), 'a trailing backslash quotes nothing');
});
