import type { Attempt, AttemptResponse, CacheMissReason, RequestKind, RequestLog } from '@/api/types';

/**
 * The folds the router's rollups and its request filters apply to a request, so a console control and the mock file a
 * request under the value the route matches it by.
 */

/** The API's five effort levels in their order, the Effort control's options before `none` and `other`. */
export const EFFORT_LEVELS = ['low', 'medium', 'high', 'xhigh', 'max'] as const;
export const NO_EFFORT = 'none';
export const NO_CLASS = '-';
export const OTHER_DIMENSION = 'other';
/** A request kind's label, one for the Requests kind filter and the guardrail editor's kind condition alike. */
export const REQUEST_KIND_LABEL: Record<RequestKind, string> = { count_tokens: 'Count tokens', inference: 'Inference', probe: 'Probe' };

const FILED_EFFORTS: readonly string[] = [...EFFORT_LEVELS, NO_EFFORT];
// The relay's identifier charset is ASCII, so a match's length in characters is its length in bytes.
const IDENTIFIER = /^[A-Za-z0-9./:_-]{1,64}$/;

/** A request's effort as the rollups file it: `none` for none, the level itself, else `other`. */
export function foldEffort(raw: string | null): string {
  if (raw === null) return NO_EFFORT;
  return FILED_EFFORTS.includes(raw) ? raw : OTHER_DIMENSION;
}

/** A request's class as the rollups file it: `-` for none, the class while it holds to the identifier charset and
 * 64 bytes, else `other`. */
export function foldRequestClass(raw: string | null): string {
  if (raw === null) return NO_CLASS;
  return IDENTIFIER.test(raw) ? raw : OTHER_DIMENSION;
}

/** A request's classifier standing as the rollups file it: `-` for a request the marker does not hold. */
export const foldClassifier = (value: string | null): string => value ?? NO_CLASS;

/** The attempt whose answer the request ended on, the last, null for a request no attempt served. */
export const servingAttempt = (r: Pick<RequestLog, 'attempts'>): Attempt | null => r.attempts[r.attempts.length - 1] ?? null;

/** The least span from the first content to the end that makes a tok/s sample. */
const TPS_MIN_SPAN_MS = 500;

/** A row's four latency samples as the rollup keeps them: each timing less the row's slow-down waits, never under
 *  zero, and a tok/s sample only over a span of TPS_MIN_SPAN_MS or more. A cap, reset or rewrite wait stays in. */
export function latencySamples(r: Pick<RequestLog, 'durationMs' | 'held' | 'ttfbMs' | 'ttftMs' | 'usage'>): { durationMs: number | null; outputTps: number | null; ttfbMs: number | null; ttftMs: number | null } {
  const hold = (r.held?.waits ?? []).reduce((sum, w) => (w.reason === 'slow_down' ? sum + w.heldMs : sum), 0);
  const lessHold = (ms: number | null) => (ms === null ? null : Math.max(0, ms - hold));
  // The hold stands in both timings, so the span between them needs no subtraction.
  const span = r.durationMs === null || r.ttftMs === null ? null : r.durationMs - r.ttftMs;
  const outputTps = span !== null && span >= TPS_MIN_SPAN_MS && r.usage.output > 0 ? r.usage.output / (span / 1000) : null;
  return { durationMs: lessHold(r.durationMs), outputTps, ttfbMs: lessHold(r.ttfbMs), ttftMs: lessHold(r.ttftMs) };
}

/**
 * A request's speed as the rollups file it: its serving answer's, else fast where it asked for fast, since a fast
 * request refused before any attempt served still asked for fast, else standard.
 */
export const foldSpeed = (r: Pick<RequestLog, 'attempts' | 'shape'>): string => servingAttempt(r)?.response.speed ?? (r.shape.requestedSpeed === 'fast' ? 'fast' : 'standard');

/** The serving attempt's cache-miss reason, the one the rollups count a request's reason on. */
export const servedMissReason = (r: Pick<RequestLog, 'attempts'>): CacheMissReason | null => servingAttempt(r)?.cacheMissReason ?? null;

/** The percent of its prompt a request reads from the cache to count as a hit under its miss reason. */
export const REASON_HIT_PERCENT = 90;
/** Whether a request hit under its reason: its cache read against the read, both writes and the uncached input. */
export const hitUnderReason = (r: Pick<RequestLog, 'usage'>): boolean => {
  const u = r.usage;
  return u.cacheRead > 0 && 100 * u.cacheRead >= REASON_HIT_PERCENT * (u.cacheRead + u.cacheWrite5m + u.cacheWrite1h + u.input);
};

/**
 * The reasons that leave a short read to the cache's expiry: no divergence, and the relay's own words. A reason the
 * upstream names says why its comparison missed.
 */
const EXPIRY_REASONS: readonly CacheMissReason[] = ['none', 'not_requested', 'not_returned', 'pending'];
/** Whether a reason is one of `EXPIRY_REASONS`, which name no diagnosed fault. */
export const relayReason = (reason: CacheMissReason): boolean => EXPIRY_REASONS.includes(reason);

/**
 * Whether a turn found its cache expired: its body carried a cache marker, and its serving attempt stored a usage
 * report, found the pin's entry past its lifetime and read under half the size the pin's last report left on that
 * account, under no reason or one of `EXPIRY_REASONS`. So a short read inside a long entry and an unmarked call never
 * count. A null previous size counts nothing, and so does an attempt stored before the router wrote whether the entry
 * had outlived its lifetime. A probe and a token count are no turns.
 */
export const expiredBetweenTurns = (r: Pick<RequestLog, 'attempts' | 'cacheMarker' | 'kind' | 'storedTokens' | 'usage'>): boolean => {
  const serving = servingAttempt(r);
  if (!r.cacheMarker || serving?.entryExpired !== true || serving.previousInputTokens === null || r.kind === 'probe' || r.kind === 'count_tokens' || r.storedTokens.input === null) return false;
  const reason = serving.cacheMissReason;
  return 2 * r.usage.cacheRead < serving.previousInputTokens && (reason === null || relayReason(reason));
};

/** The serving attempt's answer, where an attempt served the request. */
export const servingResponse = (r: Pick<RequestLog, 'attempts'>): AttemptResponse | null => servingAttempt(r)?.response ?? null;
