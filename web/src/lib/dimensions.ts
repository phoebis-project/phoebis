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

/**
 * A request's speed as the rollups file it: its serving answer's, else fast where it asked for fast, since a fast
 * request refused before any attempt served still asked for fast, else standard.
 */
export const foldSpeed = (r: Pick<RequestLog, 'attempts' | 'shape'>): string => servingAttempt(r)?.response.speed ?? (r.shape.requestedSpeed === 'fast' ? 'fast' : 'standard');

/** The serving attempt's cache-miss reason, the one the rollups count a request's reason on. */
export const servedMissReason = (r: Pick<RequestLog, 'attempts'>): CacheMissReason | null => servingAttempt(r)?.cacheMissReason ?? null;

/** An answer that read no cache though the diagnostics named no change, so its prefix expired between turns. */
export const expiredBetweenTurns = (r: Pick<RequestLog, 'attempts' | 'usage'>): boolean => r.usage.cacheRead === 0 && servedMissReason(r) === 'none';

/** The serving attempt's answer, where an attempt served the request. */
export const servingResponse = (r: Pick<RequestLog, 'attempts'>): AttemptResponse | null => servingAttempt(r)?.response ?? null;
