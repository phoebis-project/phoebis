import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

const event = (kind, agentId) => ({
  accountId: 'primary',
  at: '2026-01-01T00:00:00.000Z',
  detail: { agent_id: agentId, epoch: 1, from_account: 'direct', reason: 'cooldown' },
  keyId: 'desktop',
  kind,
  model: 'claude-sonnet-5',
  sessionId: 's1',
});

test("the trail keeps a session's pin event for its own agent and drops a sibling agent's", () => {
  const request = { agentId: 'main', canonicalModel: 'claude-sonnet-5', id: 'r1', keyId: 'desktop', sessionId: 's1' };
  const trail = d.requestTrail([event('pin_migrated', 'worker'), event('pin_expired', 'main')], request);
  assert.deepEqual(
    trail.map((e) => [e.kind, e.detail.agent_id]),
    [['pin_expired', 'main']],
  );
});

test('the trail reads a pin event recorded without an agent id as the main agent', () => {
  const request = { agentId: 'main', canonicalModel: 'claude-sonnet-5', id: 'r1', keyId: 'desktop', sessionId: 's1' };
  const legacy = { ...event('pin_expired', 'main'), detail: { reasons: ['cooldown'] } };
  assert.deepEqual(
    d.requestTrail([legacy], request).map((e) => e.kind),
    ['pin_expired'],
  );
  assert.deepEqual(d.requestTrail([legacy], { ...request, agentId: 'worker' }), [], "a subagent's request never takes it");
});
