import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

const at = (s) => new Date(Date.UTC(2026, 8, 16, 14, 30) + s * 1000).toISOString();
const ms = (s) => Date.parse(at(s));

// A stream whose 200 head committed at 1.5 s and which the upstream overloaded at 10 s, as the ring keeps it.
const failedStream = {
  accountId: 'primary',
  agentId: 'main',
  attempts: [{ accountId: 'primary', cooldownSet: null, index: 0, outcome: 'overloaded', startedAt: at(0.2), upstreamStatus: 200, windows: [] }],
  costUsd: 0.42,
  errorCode: 'overloaded_error',
  id: 'r-stream',
  keyId: 'desktop',
  kind: 'inference',
  model: 'claude-sonnet-5',
  outputTps: 55,
  phases: { finishedAt: at(10), firstContentAt: at(2.5), headAt: at(1.5), receivedAt: at(0), selectedAt: at(0.1), streamEndAt: at(10), writtenAt: at(0.3) },
  poolId: 'main',
  preview: null,
  sessionId: 's-1',
  state: 'finished',
  status: 200,
  transport: 'sse',
  usage: { cacheRead: 0, cacheWrite1h: 0, cacheWrite5m: 0, input: 10, output: 400, thinking: 0, webFetch: 0, webSearch: 0 },
  userAgent: 'claude-cli/2.1.0',
};

test('a stream that fails after its 200 head reads in flight and clean before the failure, and failed after it', () => {
  const before = d.projectAt(failedStream, ms(2));
  assert.equal(before.state, 'in_flight');
  assert.equal(before.errorCode, null, 'the completion assigns the error, so an instant before it has none');
  assert.equal(before.status, 200, "the head's status is known from 1.5 s");
  assert.deepEqual(before.attempts.map((a) => [a.outcome, a.upstreamStatus]), [['in_flight', 200]]);
  assert.deepEqual([before.usage, before.outputTps, before.costUsd], [null, null, 0]);
  assert.equal(before.phases.finishedAt, undefined);
  assert.equal(before.phases.streamEndAt, undefined);
  const after = d.projectAt(failedStream, ms(11));
  assert.equal(after, failedStream, 'a row finished by the instant reads as the ring keeps it');
  assert.equal(after.errorCode, 'overloaded_error');
});

test('a request before its head reads no status and no response facts on the attempt it is waiting on', () => {
  const failover = {
    ...failedStream,
    attempts: [
      { accountId: 'nightly', cooldownSet: { reason: 'rate_limited', until: at(60) }, index: 0, outcome: 'rate_limited', startedAt: at(0.2), upstreamStatus: 429, windows: [{ id: '5h', resetsAt: at(3600), utilization: 1 }] },
      { accountId: 'primary', cooldownSet: null, index: 1, outcome: 'overloaded', startedAt: at(1), upstreamStatus: 529, windows: [] },
    ],
    errorCode: 'overloaded_error',
    phases: { ...failedStream.phases, headAt: at(3) },
    status: 529,
  };
  const waiting = d.projectAt(failover, ms(2));
  assert.equal(waiting.status, null);
  assert.equal(waiting.errorCode, null);
  assert.deepEqual(waiting.attempts.map((a) => [a.outcome, a.upstreamStatus]), [['rate_limited', 429], ['in_flight', null]], 'the earlier attempt settled before the instant; the last one has not answered');
  const first = d.projectAt(failover, ms(0.5));
  assert.equal(first.attempts.length, 1, 'an attempt that had not started by the instant is not shown');
  assert.equal(first.attempts[0].upstreamStatus, null, 'the row keeps no head time for an earlier attempt, so its status may postdate the instant');
});
