import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

const FILLED = { cacheRead: 300, cacheWrite1h: 50, cacheWrite5m: 0, input: 100, output: 70, thinking: 20, webFetch: 0, webSearch: 0 };
const EMPTY = { cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: 0, input: 0, output: 0, thinking: 0, webFetch: 0, webSearch: 0 };

test("the segments follow the Tokens chart's order with the plain output less its thinking and no entry for an empty kind", () => {
  const segments = d.tokenSegments(FILLED);
  assert.deepEqual(
    segments.map((s) => [s.key, s.label, s.slot, s.count]),
    [
      ['uncachedInput', 'Uncached input', 0, 100],
      ['cacheRead', 'Cache read', 2, 300],
      ['cacheWrite1h', 'Cache write 1h', 1, 50],
      ['thinking', 'Thinking', 5, 20],
      ['plainOutput', 'Output', 6, 50],
    ],
  );
  assert.ok(!segments.some((s) => s.key === 'cacheWrite5m'));
  const sum = segments.reduce((n, s) => n + s.share, 0);
  assert.ok(Math.abs(sum - 1) < 1e-9, `the shares sum to ${sum}`);
  assert.equal(segments[1].share, 300 / 520);
});

test('a request with no tokens has no segment and a "no tokens" label', () => {
  assert.deepEqual(d.tokenSegments(EMPTY), []);
  assert.equal(d.tokenBarLabel(EMPTY), 'no tokens');
});

test("the label names the total and each kind's count", () => {
  assert.equal(d.tokenBarLabel(FILLED), '520 tokens: Uncached input 100, Cache read 300, Cache write 1h 50, Thinking 20, Output 50');
  assert.equal(d.tokenBarLabel({ ...EMPTY, cacheRead: 12_345 }), '12,345 tokens: Cache read 12,345');
});
