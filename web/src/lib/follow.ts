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
  /**
   * Where Next walks on when a poll's newest page took the place of a list read that failed: the cursor past that page,
   * null where the route had no more, and the end of the range it read.
   */
  standIn: { cursor: string | null; to: number } | null;
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
 * One Follow poll over any row type: the newest page and each one after it until a page brings no row the list lacks,
 * or the route has no more. The request ledger lists a request by when it arrived and writes it when it finishes, so a
 * row can land below rows the list holds, and a page that holds a known row ends the poll only when it brings no new
 * one. A row `below` the list's last row is on the pages Next reads, so it neither keeps the poll reading nor lands.
 * `overrun` is the cursor past the last row read when `FOLLOW_PAGES` pages ran out first, so the rows between it and
 * the list stay reachable.
 */
export async function followPages<R>(read: (cursor: string | undefined) => Promise<{ nextCursor: string | null; rows: R[] }>, known: ReadonlySet<string>, identity: (row: R) => string, below: (row: R) => boolean): Promise<{ overrun: string | null; rows: R[] }> {
  const rows: R[] = [];
  let cursor: string | undefined;
  for (let n = 0; n < FOLLOW_PAGES; n += 1) {
    const page = await read(cursor);
    const listed = page.rows.filter((r) => !below(r));
    rows.push(...listed);
    if (page.nextCursor === null || listed.every((r) => known.has(identity(r)))) return { overrun: null, rows };
    cursor = page.nextCursor;
  }
  return { overrun: cursor ?? null, rows };
}
