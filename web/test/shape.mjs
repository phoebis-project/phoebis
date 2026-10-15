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

/** A `detail` (`RequestEvent.detail`, `Record<string, unknown> | null`) is the wire's free-form object passed
 * through undecoded, its keys per event kind: type only. */
export const OPAQUE = (path) => path.endsWith('.detail');

/** An open map (`AnalyticsGroup.errors`, `Record<string, number>`) holds one entry per error code observed in
 * the range; its keys are data, not schema. `cacheMissReasons` is one too past its eight known keys, since the
 * route answers any name the beta sent, unlike a closed enum-keyed record such as `wastedProbeReasons`, which
 * decodes with every key always present and stays checked by key set; so is `Pool.movesPerHour`, one entry per
 * move reason seen in the last hour, the rewrite route's `moves`, the same over the range, and a window fit's
 * `corrections`, one factor per model the fit corrected. The window-keyed maps are open the same way, one entry
 * per window the rows priced or sampled: a group's `costShares`, `costEquivalent`, `savingShares` and
 * `savingEquivalent` (a figure per window, an unpriced window dropped), its `cacheShares`, `cacheEquivalent`,
 * `probeShares` and `probeEquivalent` (an object per window) and an account hour's `windows` (a sample per window).
 * Matched by path only where the fixture's own shape there is an object, so a same-named scalar
 * (`Overview.hourly[].errors`, a plain count) still takes the ordinary type check below. */
const MAP_PATHS = ['.errors', '.cacheMissReasons', '.movesPerHour', '.moves', '.corrections', '.costShares', '.costEquivalent', '.cacheShares', '.cacheEquivalent', '.probeShares', '.probeEquivalent', '.savingShares', '.savingEquivalent', '.windows'];
const MAP = (path, want) => MAP_PATHS.some((suffix) => path.endsWith(suffix)) && typeof want === 'object' && want !== null && !Array.isArray(want);
/**
 * The entry kinds a map admits beside its first entry's own, by the path the map sits at: a request row's share and
 * equivalent maps (`.requests[n].costShares`, a request detail's `.request.costShares`, the dashboard's recent rows'
 * `.recent[n].costShares`, and `costEquivalent` at the same three) carry null for an applicable window the cost model
 * could not price, while a group's map drops such a window, so a null there is a finding.
 */
const MAP_ENTRY_KINDS = [
  [/(^\.request|\.requests\[\d+\])\.costEquivalent$/, ['null', 'number']],
  [/(^\.request|\.requests\[\d+\])\.costShares$/, ['null', 'number']],
  [/^\.recent\[\d+\]\.costEquivalent$/, ['null', 'number']],
  [/^\.recent\[\d+\]\.costShares$/, ['null', 'number']],
];

const kindOf = (v) => (v === null || v === undefined ? 'null' : Array.isArray(v) ? 'list' : typeof v);

/**
 * Findings where the mock's `got` cannot stand in for the fixture's `want` shape. The fixture is the truth and
 * may lack information, so a 'null' or an 'empty' on its side asks nothing. The answer itself and its direct
 * members (depth 0 and 1: the value an `Api` method returns, a page's list, the members of a composite a sync
 * row builds) may not be null where the fixture has a value, nor an empty list where it has rows: that is the
 * mock answering nothing where the console would show something. Deeper, one fixture row cannot tell a
 * required field from a nullable one or a membership from an empty one (a failed request's `ttfbMs`, an
 * account in no pool, an API-key attempt's `windows`), so a null or an empty list there is the row's own
 * variation and asks nothing, while every row's key set is checked and every element of a mock list is held
 * to the fixture's first row. `passthrough`, an optional extension of `OPAQUE` a caller supplies per sync row,
 * covers a further path a specific decoder leaves undecoded (a declared `unknown`, such as
 * `CapturedBodies.request`/`.response`) the same way `OPAQUE` covers `.detail`.
 */
export function same(want, got, path = '', depth = 0, passthrough = () => false) {
  // A passthrough hook answers true for the object-or-null case, or the kinds it admits (a stored body is JSON or
  // the text of a stream).
  const admitted = OPAQUE(path) ? true : passthrough(path);
  if (admitted) {
    const kinds = admitted === true ? ['object', 'null'] : admitted;
    const words = kinds.map((k) => (k === 'object' ? 'an object' : k === 'string' ? 'a string' : k));
    const expected = words.length <= 2 ? words.join(' or ') : `${words.slice(0, -1).join(', ')} or ${words[words.length - 1]}`;
    return kinds.includes(kindOf(got)) ? [] : [`${path}: ${kindOf(got)} where ${expected} is expected`];
  }
  if (MAP(path, want)) {
    if (got === null || got === undefined) return depth <= 1 ? [`${path}: null in the mock where the fixture has a value`] : [];
    if (typeof got !== 'object' || Array.isArray(got)) return [`${path}: ${kindOf(got)} where a map is expected`];
    const entryShape = Object.values(want)[0];
    if (entryShape === undefined) return [];
    const kinds = MAP_ENTRY_KINDS.find(([at]) => at.test(path))?.[1] ?? [];
    // An object entry is held to the fixture's first entry's shape, and a scalar one to its type, or a kind the map
    // admits.
    return Object.entries(got).flatMap(([k, v]) => (kinds.includes(kindOf(v)) ? [] : typeof entryShape === 'object' ? same(entryShape, v, `${path}.${k}`, depth + 1, passthrough) : typeof v === entryShape ? [] : [`${path}.${k}: ${typeof v} against ${entryShape}`]));
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
