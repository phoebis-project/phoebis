/**
 * The decoded fixture's shape: keys only, recursively. A list is its first row's shape, since fixture rows
 * are homogeneous; an empty list is 'empty' and null is 'null', each a place the fixture holds no information.
 */
export function shapeOf(v) {
  if (v === null || v === undefined) return 'null';
  if (Array.isArray(v)) return v.length ? [shapeOf(v[0])] : 'empty';
  if (typeof v === 'object') return Object.fromEntries(Object.keys(v).sort().map((k) => [k, shapeOf(v[k])]));
  return typeof v;
}

/** A `detail` (`AlertEventDetail`/`RequestEvent.detail`, both `Record<string, unknown> | null`) is the wire's
 * free-form object passed through undecoded, its keys per event kind: type only. */
export const OPAQUE = (path) => path.endsWith('.detail');

/** An open map (`AnalyticsGroup.errors`, `Record<string, number>`) holds one entry per error code observed in
 * the range; its keys are data, not schema. `cacheMissReasons` is one too past its eight known keys, since the
 * route answers any name the beta sent, unlike a closed enum-keyed record such as `wastedProbeReasons`, which
 * decodes with every key always present and stays checked by key set; so is `Pool.movesPerHour`, one entry per
 * move reason seen in the last hour, the rewrite route's `moves`, the same over the range, and a window fit's
 * `corrections`, one factor per model the fit corrected. Matched by path only where the fixture's own shape there
 * is an object, so a same-named scalar (`Overview.hourly[].errors`, a plain count) still takes the ordinary type
 * check below. */
const MAP = (path, want) => (path.endsWith('.errors') || path.endsWith('.cacheMissReasons') || path.endsWith('.movesPerHour') || path.endsWith('.moves') || path.endsWith('.corrections')) && typeof want === 'object' && want !== null && !Array.isArray(want);

const kindOf = (v) => (v === null || v === undefined ? 'null' : Array.isArray(v) ? 'list' : typeof v);

/**
 * Findings where the mock's `got` cannot stand in for the fixture's `want` shape. The fixture is the truth and
 * may lack information, so a 'null' or an 'empty' on its side asks nothing. The answer itself and its direct
 * members (depth 0 and 1: the value an `Api` method returns, a page's list, the members of a composite a sync
 * row builds) may not be null where the fixture has a value, nor an empty list where it has rows: that is the
 * mock answering nothing where the console would show something. Deeper, one fixture row cannot tell a
 * required field from a nullable one or a membership from an empty one (a failed request's `ttfbMs`, an
 * account in no pool, an Anthropic-key attempt's `windows`), so a null or an empty list there is the row's own
 * variation and asks nothing, while every row's key set is checked and every element of a mock list is held
 * to the fixture's first row. `passthrough`, an optional extension of `OPAQUE` a caller supplies per sync row,
 * covers a further path a specific decoder leaves undecoded (a declared `unknown`, such as
 * `CapturedBodies.request`/`.response`) the same way `OPAQUE` covers `.detail`.
 */
export function same(want, got, path = '', depth = 0, passthrough = () => false) {
  if (OPAQUE(path) || passthrough(path)) return ['null', 'object'].includes(kindOf(got)) ? [] : [`${path}: ${kindOf(got)} where an object or null is expected`];
  if (MAP(path, want)) {
    if (got === null || got === undefined) return depth <= 1 ? [`${path}: null in the mock where the fixture has a value`] : [];
    if (typeof got !== 'object' || Array.isArray(got)) return [`${path}: ${kindOf(got)} where a map is expected`];
    const entryType = Object.values(want)[0];
    return entryType === undefined ? [] : Object.entries(got).flatMap(([k, v]) => (typeof v === entryType ? [] : [`${path}.${k}: ${typeof v} against ${entryType}`]));
  }
  if (want === 'null' || want === 'empty') return [];
  if (got === null || got === undefined) return depth <= 1 ? [`${path}: null in the mock where the fixture has a value`] : [];
  if (Array.isArray(want)) {
    if (!Array.isArray(got)) return [`${path}: one side is a list`];
    if (got.length === 0) return depth <= 1 ? [`${path}: an empty list in the mock where the fixture has rows`] : [];
    return got.flatMap((row, i) => same(want[0], row, `${path}[${i}]`, depth + 1, passthrough));
  }
  if (Array.isArray(got)) return [`${path}: one side is a list`];
  if (typeof want === 'object') {
    if (typeof got !== 'object') return [`${path}: ${typeof got} against object`];
    const keys = [...new Set([...Object.keys(want), ...Object.keys(got)])].sort();
    return keys.flatMap((k) => (k in want && k in got ? same(want[k], got[k], `${path}.${k}`, depth + 1, passthrough) : [`${path}.${k}: ${k in want ? 'only in the decoded fixture' : 'only in the mock'}`]));
  }
  return want === typeof got ? [] : [`${path}: ${typeof got} against ${want}`];
}
