/**
 * The folds the router's rollups and its request filters apply to a request's effort and class, so a console control
 * and the mock file a request under the value the route matches it by.
 */

/** The API's five effort levels in their order, the Effort control's options before `none` and `other`. */
export const EFFORT_LEVELS = ['low', 'medium', 'high', 'xhigh', 'max'] as const;
export const NO_EFFORT = 'none';
export const NO_CLASS = '-';
export const OTHER_DIMENSION = 'other';

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
