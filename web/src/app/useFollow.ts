import { type Dispatch, type SetStateAction, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { ApiError, quietly, reportRecovery } from '@/api/http';
import { clock } from '@/lib/clock';
import { type Followed, followPages, merged, type Overrun, type Paging, pagingFor } from '@/lib/follow';
import { armPoll, counted } from '@/lib/hooks';

/**
 * The auto reload poll both logs run: every `intervalMs` while the tab is visible, the page's `paused` is false and the
 * first page shows, read the newest pages up to now over the rows shown under `address` (the followed rows it holds,
 * else the list's), merge them under the fresh tint, keep an overrun's cursor with its poll's end, and toast a failing
 * streak once. When a wait ends a full interval after the last answer, the poll reads at once. A tick or such a read
 * that finds a poll of the hook still out is skipped, and so is one while no rows show under `address` and the list's
 * read has not failed, since the list's own read is then still out. The caller hands in the list's last answer, its
 * error and its paging state, and reads back the rows to draw, the error to show, the pager's next cursor and the
 * pager's handlers, so nothing the caller passes depends on the hook's own state.
 *
 * A list that answers `landedThrough` runs the poll under a watermark: every page of a poll reads the rows written
 * after it (`read`'s `landedAfter`), the list's own stamp first and then each poll's first page's, so a request that
 * finished below the rows shown lands among them. A list that answers none keeps the page walk from the newest page.
 */
export function useFollow<R>(input: {
  address: string;
  compare: (a: R, b: R) => number;
  error: string | null;
  /**
   * Whether the range holds its own end (a custom range), so with no next cursor a late row below the list's last row
   * is the list's.
   */
  fixedEnd: boolean;
  identity: (row: R) => string;
  /** The auto reload interval in milliseconds, 0 for off. */
  intervalMs: number;
  list: { landedThrough?: number; nextCursor: string | null; rows: R[] } | null;
  paging: Paging;
  /** The page's own pause (an open drawer). A cursor page and a hidden tab are the hook's. */
  paused: boolean;
  read: (cursor: string | undefined, to: number, landedAfter?: number) => Promise<{ landedThrough?: number; nextCursor: string | null; rows: R[] }>;
  setPaging: Dispatch<SetStateAction<Paging>>;
  stale: boolean;
}): { error: string | null; fresh: Set<string>; goNext: () => void; goPrevious: () => void; nextCursor: string | null; overrun: Overrun | null; rows: R[]; setFollowed: Dispatch<SetStateAction<Followed<R> | null>> } {
  const { address, compare, error, fixedEnd, identity, intervalMs, list, paging, paused, read, setPaging, stale } = input;
  const { cursor, stack, to: pagedTo } = pagingFor(paging, address);
  const [followed, setFollowed] = useState<Followed<R> | null>(null);
  // The tint marks what the last poll brought, or after the tab's return what its read and the tick after it brought,
  // and with auto reload off no poll follows to take it away.
  const [intervalSeen, setIntervalSeen] = useState(intervalMs);
  if (intervalSeen !== intervalMs) {
    setIntervalSeen(intervalMs);
    if (intervalMs === 0) setFollowed((f) => f && { ...f, fresh: new Set() });
  }
  // A cursor page draws the list's own rows, and the followed ones return with the first page.
  const following = followed !== null && followed.for === address && cursor === undefined ? followed : null;
  // A stand-in only holds the place of a list read that failed, so the list's answer under this query takes over with
  // its rows, its stamp and its cursor.
  if (following !== null && following.standIn !== null && error === null && list !== null && !stale) setFollowed(null);
  // The rows the poll merges into: those followed under this address, else the list's once it answers this query. While
  // the list still answers an earlier filter, its rows belong to that filter, so there is nothing to merge into yet.
  const shown = following?.rows ?? (stale ? null : (list?.rows ?? null));
  // The stamp the rows shown were read through, so it moves with them.
  const mark = following ? following.landedThrough : stale ? undefined : list?.landedThrough;
  const overrun = following?.overrun ?? null;
  const standIn = following?.standIn ?? null;
  // The poll outlives renders, so it reads the current address, rows and reader through a ref.
  const latest = useRef({ address, compare, failed: error !== null, fixedEnd, identity, list: stale ? null : list, mark, overrun, read, shown, standIn });
  useLayoutEffect(() => {
    latest.current = { address, compare, failed: error !== null, fixedEnd, identity, list: stale ? null : list, mark, overrun, read, shown, standIn };
  });
  // A failing streak toasts once: the polls that continue it run quiet, and the one that ends it takes the streak's
  // notice down, as `useAsync`'s refresh does.
  const streak = useRef({ failing: false, paths: new Set<string>() });
  // When the last poll answered, null until one has, so a wait that ends reads at once only after a full interval.
  const lastAnswered = useRef<number | null>(null);
  // The hook's polls still out. It outlives the poll's effect, so the read at once and the first tick after a drawer
  // closes, the interval changes or the first page returns skip while a poll from before it is out.
  const out = useRef(0);
  // The rows under the reader would move on an older page, so its cursor holds the poll until the first page returns.
  const polling = intervalMs > 0 && !paused && cursor === undefined;
  // A streak toasts once until the address or the interval changes, so the poll a pause's end arms runs on quiet.
  // Declared above the poll's effect, since an interval change re-runs that effect with a read at once, which has to
  // find the streak reset.
  useEffect(() => {
    streak.current.failing = false;
  }, [address, intervalMs]);
  useEffect(() => {
    if (!polling) return;
    // An answer that lands once the poll has stopped would tint rows no later poll clears.
    const armed = { alive: true };
    const failures = streak.current;
    // The rows the tab's return read brought. The interval keeps its phase, so its next tick can follow that read
    // within milliseconds and bring nothing, and that tick's answer keeps their tint beside its own rows.
    let kept: { for: string; rows: Set<string> } | null = null;
    const poll = (returned = false) => {
      const { address: polled, compare: order, fixedEnd, identity: identify, list, mark: landedAfter, read: reader, shown: start, standIn: standing } = latest.current;
      // Every page of one poll reads from the same stamp and is handed the same instant, so a reader that bounds its
      // range by it walks one fixed range. A reader under a watermark may read to the router's clock instead, and the
      // instant still dates the poll's overrun and stand-in.
      const to = clock.now();
      const last = start?.[start.length - 1];
      const below = (r: R) => last !== undefined && order(r, last) > 0;
      // A stand-in is the list while it stands, so its page's cursor says whether the range holds rows past those
      // shown. A stale list hands none, since its cursor is another query's.
      const tail = standing === null ? list?.nextCursor : standing.cursor;
      const lands = fixedEnd && tail === null;
      // On a custom range each page the poll reads notes a row it withholds below the rows shown while the ref holds no
      // list (a stale or a failed read) and no stand-in, since such a poll lands nothing below and the stamp must not
      // pass the row.
      let withheld = false;
      const page = (cursor: string | undefined) => {
        const answer = failures.failing ? quietly(() => reader(cursor, to, landedAfter)) : reader(cursor, to, landedAfter);
        return answer.then((p) => {
          if (fixedEnd && list === null && standing === null && p.rows.some(below)) withheld = true;
          return p;
        });
      };
      // Before the list answers there is no row to stop at, so the poll reads the newest page alone and keeps the
      // cursor past it for the answer that stands in for a list read that failed.
      const pages: Promise<{ cursor?: string | null; landedThrough?: number; overrun: string | null; rows: R[] }> =
        start === null ? page(undefined).then((p) => ({ cursor: p.nextCursor, landedThrough: p.landedThrough, overrun: null, rows: p.rows })) : followPages(page, new Set(start.map(identify)), identify, below, landedAfter === undefined, () => armed.alive, lands);
      counted(out, pages).then(
        (polledPages) => {
          if (!armed.alive) return;
          lastAnswered.current = clock.now();
          for (const path of failures.paths) reportRecovery(path);
          failures.paths.clear();
          failures.failing = false;
          // An answer for filters the reader has left since lands nowhere.
          const current = latest.current;
          if (current.address !== polled) return;
          if (current.shown === null) {
            // A list read in flight after a failure keeps `failed` until it answers, so a poll that lands meanwhile
            // stands in for it untinted and clears its error, and the stand-in's rows stay drawn until the list answers
            // under the same address, a Refresh or another address.
            if (current.failed && polledPages.cursor !== undefined) setFollowed({ for: polled, fresh: new Set(), landedThrough: polledPages.landedThrough, overrun: null, rows: polledPages.rows, standIn: { cursor: polledPages.cursor, to } });
            return;
          }
          const known = new Set(current.shown.map(identify));
          // A gap an earlier poll left stays until a reload, and Next walks on from the newest one, which reaches the
          // older ones too.
          const next = polledPages.overrun === null ? current.overrun : { cursor: polledPages.overrun, to };
          const rows = merged(current.shown, polledPages.rows, identify, order);
          const brought = new Set(polledPages.rows.map(identify).filter((k) => !known.has(k)));
          const fresh = kept !== null && kept.for === polled ? new Set([...kept.rows, ...brought]) : brought;
          kept = returned ? { for: polled, rows: brought } : null;
          setFollowed({ for: polled, fresh, landedThrough: withheld ? landedAfter : (polledPages.landedThrough ?? landedAfter), overrun: next, rows, standIn: current.standIn });
        },
        (err: unknown) => {
          if (!armed.alive) return;
          // A failure under an address the reader has left since leaves the new address's streak to its own polls.
          if (latest.current.address === polled) failures.failing = true;
          if (err instanceof ApiError && err.path !== null) failures.paths.add(err.path);
        },
      );
    };
    // Only the tab's return read keeps its tint through the tick after it.
    const disarm = armPoll(intervalMs, poll, () => out.current === 0 && (latest.current.shown !== null || latest.current.failed), () => lastAnswered.current, true);
    return () => {
      armed.alive = false;
      disarm();
    };
    // The poll reads the address, the rows and the reader through `latest`, so the effect re-arms only when the
    // interval or the pause changes.
  }, [intervalMs, polling]);

  const rows = following?.rows ?? list?.rows ?? [];
  const fresh = following?.fresh ?? new Set<string>();
  // Past an overrun, Next walks on from the last row the poll read, so the rows it left are the next page. A poll that
  // stood in for the list walks on from its own page, under the end it read to.
  const walk = overrun ?? standIn;
  const nextCursor = walk === null ? (list?.nextCursor ?? null) : walk.cursor;
  const goNext = () => {
    if (!nextCursor) return;
    // The cursor pauses the poll, and the tint marks what the last poll brought with no poll to take it away.
    setFollowed((f) => f && { ...f, fresh: new Set() });
    setPaging({ for: address, cursor: nextCursor, stack: [...stack, cursor], to: walk?.to ?? pagedTo });
  };
  const goPrevious = () => {
    const copy = [...stack];
    const previous = copy.pop();
    setPaging({ for: address, cursor: previous, stack: copy, to: pagedTo });
  };
  return { error: standIn === null ? error : null, fresh, goNext, goPrevious, nextCursor, overrun, rows, setFollowed };
}
