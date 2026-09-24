import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import * as d from './dist/api.js';

const { allowsModel, matchesPattern, processRules } = d.modelPatterns;

const CASES = JSON.parse(readFileSync(new URL('../../crates/routing/tests/fixtures/model_rules_cases.json', import.meta.url), 'utf8'));

function assertMatcherCase(c) {
  assert.equal(matchesPattern(c.pattern, c.name), c.matches, `${JSON.stringify(c.pattern)} against ${JSON.stringify(c.name)}`);
}

for (const [name, cases] of Object.entries(CASES.matcher)) {
  test(name, () => {
    for (const c of cases) assertMatcherCase(c);
  });
}

/**
 * A pattern of many stars against a value that cannot match is where a split-per-star matcher goes exponential;
 * the table walk answers in bounded steps, and a pattern or a name past the bound answers no match at all.
 */
test('the matcher answers no match past its bounds', () => {
  const withinPattern = '*a'.repeat(128);
  const withinName = 'a'.repeat(256);
  assert.equal([...withinPattern].length, 256);
  assert.equal([...withinName].length, 256);
  const start = Date.now();
  matchesPattern(withinPattern, withinName);
  assert.ok(Date.now() - start < 1_000, 'a table-filled match must stay bounded');
  const overPattern = `${withinPattern}a`;
  assert.equal(matchesPattern(overPattern, withinName), false, 'a pattern past the bound answers no match');
  const overName = 'a'.repeat(257);
  assert.equal(matchesPattern(withinPattern, overName), false, 'a name past the bound answers no match');
});

function assertEvaluatorCase(c) {
  const processed = processRules(c.key, c.mode, c.pool);
  assert.equal(allowsModel(processed, c.name), c.allowed, `name ${JSON.stringify(c.name)}`);
  if (c.levels) assert.deepEqual(processed.map((p) => p.level), c.levels, `processed levels for ${JSON.stringify(c.name)}`);
}

for (const [name, cases] of Object.entries(CASES.evaluator)) {
  test(name, () => {
    for (const c of cases) assertEvaluatorCase(c);
  });
}
