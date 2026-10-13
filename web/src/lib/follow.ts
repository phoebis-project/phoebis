/** The pages one auto reload poll reads at most, so a burst of arrivals cannot keep a poll reading without end. */
export const FOLLOW_PAGES = 8;
/** The intervals the split Refresh button offers, 0 for off. */
export const RELOAD_INTERVALS = [
  { label: 'Off', ms: 0 },
  { label: '5 s', ms: 5_000 },
  { label: '10 s', ms: 10_000 },
  { label: '30 s', ms: 30_000 },
  { label: '1 min', ms: 60_000 },
] as const;
export const FOLLOW_OVERRUN = 'More rows arrived than auto reload could fetch. Reload to read them.';
/**
 * The one interval the control and the poll read: 0 while a custom span has ended (nothing new to read, the part Off
 * and disabled), else the chosen interval.
 */
export const effectiveInterval = (intervalMs: number, ended: boolean): number => (ended ? 0 : intervalMs);
/** The grace a custom span's end keeps against the instant the page took its range. */
const ENDED_GRACE_MS = 60_000;
/**
 * Whether a custom span had ended when the page took its range, its end more than a minute before that instant. A link
 * capped at the click lands inside the minute and reads on, and a span widened or typed with an end in the past has
 * ended however long the page has been open.
 */
export const spanEnded = (custom: boolean, end: number, takenAt: number): boolean => custom && end < takenAt - ENDED_GRACE_MS;
/**
 * The end a read takes: a custom span holds its own end, so a poll or an older page reads only the rows inside the span
 * the Custom pill prints, and a preset reads up to the instant given (the poll's clock, or the end a cursor came from).
 */
export const readEnd = (custom: boolean, end: number, at: number): number => (custom ? end : at);
/** What the split button's title names while auto reload waits. The hidden tab is the control's own reading. */
export const PAUSE_CAUSE = {
  dialog: 'paused while a dialog is open',
  hidden: 'paused while the tab is hidden',
  menu: 'paused while a menu is open',
  olderPage: 'paused on an older page',
  request: 'paused while a request is open',
} as const;
export type PauseCause = keyof typeof PAUSE_CAUSE;

/**
 * Where the poll stopped short of the rows the list held: the cursor past its last row and the end of the range it
 * read.
 */
export interface Overrun {
  cursor: string;
  to: number;
}

/** The poll's rows belong to the address they were read under (`for`), so a filter change sets them aside. */
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
 * One auto reload poll over any row type: the newest page and each one after it until a page brings no row the list
 * lacks, or the route has no more. The request ledger lists a request by when it arrived and writes it when it
 * finishes, so a row can land below rows the list holds, and a page that holds a known row ends the poll only when it
 * brings no new one. A row `below` the list's last row is on the pages Next reads, so it neither keeps the poll reading
 * nor lands. `overrun` is the cursor past the last row read when `FOLLOW_PAGES` pages ran out first, so the rows
 * between it and the list stay reachable.
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
