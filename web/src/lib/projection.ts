import type { LiveRequest } from '@/api/types';

/**
 * A finished row as it looked at `atMs`. The ring holds finished rows only, so a row that had not finished by
 * that instant is shown as it looked then: in flight, head known if seen. What the ring keeps no history of
 * (the usage, the throughput) is marked unknown rather than shown at its final value, and what the completion
 * assigns (the error code, the status the client was answered with) is held back: a stream that failed after
 * its 200 head reads a clean 200 until the instant reaches the failure.
 */
export function projectAt(r: LiveRequest, atMs: number): LiveRequest {
  const finishedAt = r.phases.finishedAt ? Date.parse(r.phases.finishedAt) : 0;
  if (finishedAt <= atMs) return r;
  const seen = (t?: string) => (t && Date.parse(t) <= atMs ? t : undefined);
  const started = r.attempts.filter((a) => Date.parse(a.startedAt) <= atMs);
  // The row records the final attempt's head time alone, so the last attempt started by this instant
  // keeps its response facts only when it is the final one and its head came first; otherwise its
  // status, cooldown and windows may postdate the instant.
  const headKnown = seen(r.phases.headAt) !== undefined && started.length === r.attempts.length;
  const attempts = started.map((a, i) => (i < started.length - 1 ? a : headKnown ? { ...a, outcome: 'in_flight' as const } : { ...a, cooldownSet: null, outcome: 'in_flight' as const, upstreamStatus: null, windows: [] }));
  return {
    ...r,
    accountId: seen(r.phases.selectedAt) ? r.accountId : null,
    attempts,
    costUsd: 0,
    errorCode: null,
    outputTps: null,
    phases: { receivedAt: r.phases.receivedAt, selectedAt: seen(r.phases.selectedAt), writtenAt: seen(r.phases.writtenAt), headAt: seen(r.phases.headAt), firstContentAt: seen(r.phases.firstContentAt), streamEndAt: seen(r.phases.streamEndAt) },
    preview: null,
    state: 'in_flight',
    status: headKnown ? (attempts[attempts.length - 1]?.upstreamStatus ?? null) : null,
    usage: null,
  };
}
