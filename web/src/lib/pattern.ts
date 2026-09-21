/**
 * The wildcard grammar the client filters take, in the console's own copy: `*` for any run of characters, `?`
 * for one, and a backslash to quote either. The routes match the same grammar over the ledger, so the local
 * fold a session view runs and the mock both read it from here rather than each inventing a matcher of its own.
 */

/** ASCII case folds, as the routes fold it through SQLite's `LIKE`; a code point outside ASCII compares as itself. */
const same = (a: string, b: string) => a === b || (a.length === 1 && b.length === 1 && a.charCodeAt(0) < 128 && b.charCodeAt(0) < 128 && a.toLowerCase() === b.toLowerCase());

/**
 * The match and the number of steps it took, the count being what the unit test bounds. Two cursors walk the
 * pattern and the value; a `*` records where it stood and where the value stood, and a later mismatch resumes
 * after that star with the value one character further on, so no split is tried twice and the work is
 * `pattern * value` at worst rather than one path per way of splitting the value among the stars.
 */
export function matchesPatternCounted(pattern: string, value: string): { matched: boolean; steps: number } {
  const p = [...pattern];
  const v = [...value];
  let pi = 0;
  let vi = 0;
  let steps = 0;
  // The pattern position after the last star seen and the value position that star currently covers up to.
  let star: { after: number; covered: number } | null = null;
  while (vi < v.length) {
    steps += 1;
    const c = p[pi];
    if (c === '*') {
      star = { after: pi + 1, covered: vi };
      pi += 1;
      continue;
    }
    // A trailing backslash quotes nothing, so it carries no literal and can only fall back to a star.
    const literal = c === undefined ? null : c === '?' ? { text: '?', width: 1 } : c === '\\' ? (pi + 1 < p.length ? { text: p[pi + 1], width: 2 } : null) : { text: c, width: 1 };
    if (literal !== null && ((literal.text === '?' && literal.width === 1) || same(literal.text, v[vi]))) {
      pi += literal.width;
      vi += 1;
    } else if (star !== null) {
      pi = star.after;
      vi = star.covered + 1;
      star = { after: star.after, covered: vi };
    } else {
      return { matched: false, steps };
    }
  }
  // The value is spent; what is left of the pattern must match nothing: stars, or a trailing backslash.
  while (pi < p.length) {
    if (p[pi] === '*') pi += 1;
    else if (p[pi] === '\\' && pi + 1 === p.length) pi += 1;
    else return { matched: false, steps };
  }
  return { matched: true, steps };
}

/** Whether `value` matches `pattern`: `*` any run, `?` one character, a backslash quotes either, ASCII case folded. */
export const matchesPattern = (pattern: string, value: string): boolean => matchesPatternCounted(pattern, value).matched;
