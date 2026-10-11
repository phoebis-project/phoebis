export const FOLLOW_MS = 5_000;
/** The pages one Follow poll reads at most, so a burst of arrivals cannot keep a poll reading without end. */
export const FOLLOW_PAGES = 8;
export const FOLLOW_OVERRUN = 'More rows arrived than Follow could fetch. Reload to read them.';

/**
 * Where Follow stopped short of the rows the list held: the cursor past its last row and the end of the range it
 * read.
 */
export interface Overrun {
  cursor: string;
  to: number;
}

/** Follow's rows belong to the address they were read under (`for`), so a filter change sets them aside. */
export interface Followed<R> {
  for: string;
  fresh: Set<string>;
  overrun: Overrun | null;
  rows: R[];
}

/**
 * A poll's answer over the rows shown, one row per identity so a row the poll reads again never lands twice, in
 * `compare`'s order.
 */
export function merged<R>(shown: R[], polled: R[], identity: (row: R) => string, compare: (a: R, b: R) => number): R[] {
  const byIdentity = new Map(shown.map((r) => [identity(r), r]));
  for (const r of polled) byIdentity.set(identity(r), r);
  return [...byIdentity.values()].sort(compare);
}

/**
 * One Follow poll over any row type: the newest page and each one after it until a page holds a known row, or the
 * route has no more. `overrun` is the cursor past the last row read when `FOLLOW_PAGES` pages ran out first, so the
 * rows between it and the list stay reachable.
 */
export async function followPages<R>(read: (cursor: string | undefined) => Promise<{ nextCursor: string | null; rows: R[] }>, known: ReadonlySet<string>, identity: (row: R) => string): Promise<{ overrun: string | null; rows: R[] }> {
  const rows: R[] = [];
  let cursor: string | undefined;
  for (let n = 0; n < FOLLOW_PAGES; n += 1) {
    const page = await read(cursor);
    rows.push(...page.rows);
    if (page.nextCursor === null || page.rows.some((r) => known.has(identity(r)))) return { overrun: null, rows };
    cursor = page.nextCursor;
  }
  return { overrun: cursor ?? null, rows };
}
