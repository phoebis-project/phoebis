import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

test('a window is expired once its reset lies before now, never without a reset', () => {
  const now = Date.UTC(2026, 8, 16, 14, 30);
  const window = (resetsAt) => ({ id: '5h', label: '5-hour', observedAt: new Date(now - 60_000).toISOString(), periodHours: 5, resetsAt, utilization: 0.18 });
  assert.equal(d.isExpired(window(new Date(now - 2 * 86_400_000).toISOString()), now), true, 'a reset two days ago');
  assert.equal(d.isExpired(window(new Date(now + 3_600_000).toISOString()), now), false, 'a reset an hour ahead');
  assert.equal(d.isExpired(window(new Date(now).toISOString()), now), false, 'a reset at this instant has not passed yet');
  assert.equal(d.isExpired(window(null), now), false, 'a window reported without a reset');
});
