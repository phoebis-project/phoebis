import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import * as d from './dist/api.js';

const { allowsModel, checkPattern, matchesPattern, processRules } = d.modelPatterns;

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
 * A pattern of many stars against a value it cannot match is where a split-per-star matcher goes exponential. At the
 * bound, a matching pair answers true, and a non-matching pair answers false inside one second, which only an
 * exponential matcher exceeds. A pattern or a name past the bound answers no match at all.
 */
test('the matcher answers no match past its bounds', () => {
  const withinPattern = '*a'.repeat(128);
  const withinName = 'a'.repeat(256);
  assert.equal(matchesPattern(withinPattern, withinName), true, 'a pattern and a name at the bound fill the table and match');
  // 256 tokens, the last a literal b the name never holds.
  const missing = `${'*a'.repeat(127)}*b`;
  const start = Date.now();
  const missed = matchesPattern(missing, withinName);
  assert.ok(Date.now() - start < 1_000, 'a table-filled match must stay bounded');
  assert.equal(missed, false, 'a pattern at the bound whose last literal the name lacks answers no match');
  const overPattern = `${withinPattern}a`;
  assert.equal(matchesPattern(overPattern, withinName), false, 'a pattern past the bound answers no match');
  const overName = 'a'.repeat(257);
  assert.equal(matchesPattern(withinPattern, overName), false, 'a name past the bound answers no match');
});

// A flat list rather than named groups: each row is one refusal, or null for a pattern the route accepts.
test('the pattern check answers what the route answers, in its words', () => {
  for (const row of CASES.check) assert.equal(checkPattern(row.pattern), row.error, JSON.stringify(row.pattern));
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
