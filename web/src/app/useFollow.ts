import { type Dispatch, type SetStateAction, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { ApiError, quietly, reportRecovery } from '@/api/http';
import { clock } from '@/lib/clock';
import { FOLLOW_MS, type Followed, followPages, merged } from '@/lib/follow';

/** A list's cursor walk and the pages before it, kept for the address (`for`) it was read under. */
export interface Paging {
  for: string;
  cursor?: string;
  stack: (string | undefined)[];
  to?: number;
}

/**
 * The poll both pages run: every FOLLOW_MS while `on`, read the newest pages up to now over the rows shown under
 * `address` (the followed rows it holds, else the list's), merge them under the fresh tint, keep an overrun's cursor
 * with its poll's end, and toast a failing streak once. The caller hands in the list's last answer, its error and its
 * paging state, and reads back `following`, the rows to draw, the error to show, the pager's next cursor and the
 * handlers of the pager and the switch, so nothing the caller passes depends on the hook's own state.
 */
export function useFollow<R>(input: {
  address: string;
  compare: (a: R, b: R) => number;
  error: string | null;
  identity: (row: R) => string;
  list: { nextCursor: string | null; rows: R[] } | null;
  on: boolean;
  paging: Paging;
  read: (cursor: string | undefined, to: number) => Promise<{ nextCursor: string | null; rows: R[] }>;
  setFollow: (on: boolean) => void;
  setPaging: Dispatch<SetStateAction<Paging>>;
  stale: boolean;
}) {
  const { address, compare, error, identity, list, on, paging, read, setFollow, setPaging, stale } = input;
  const cursor = paging.for === address ? paging.cursor : undefined;
  const stack = paging.for === address ? paging.stack : [];
  const pagedTo = paging.for === address ? paging.to : undefined;
  const [followed, setFollowed] = useState<Followed<R> | null>(null);
  // A cursor page draws the list's own rows, and the followed ones return with the first page.
  const following = followed !== null && followed.for === address && cursor === undefined ? followed : null;
  // The rows the poll merges into: those followed under this address, else the list's once it answers this query. While
  // the list still answers an earlier filter, its rows belong to that filter, so there is nothing to merge into yet.
  const shown = following?.rows ?? (stale ? null : (list?.rows ?? null));
  const overrun = following?.overrun ?? null;
  const standIn = following?.standIn ?? null;
  // The poll outlives renders, so it reads the current address, rows and reader through a ref.
  const latest = useRef({ address, compare, failed: error !== null, identity, overrun, read, shown, standIn });
  useLayoutEffect(() => {
    latest.current = { address, compare, failed: error !== null, identity, overrun, read, shown, standIn };
  });
  // A failing streak toasts once: the polls that continue it run quiet, and the one that ends it takes the streak's
  // notice down, as `useAsync`'s refresh does.
  const streak = useRef({ failing: false, paths: new Set<string>() });
  useEffect(() => {
    if (!on) return;
    // An answer that lands once the poll has stopped would tint rows no later poll clears.
    const armed = { alive: true };
    const failures = streak.current;
    // Each Follow run toasts its own streak once, so a run armed while the route still fails says so again.
    failures.failing = false;
    const id = clock.setInterval(() => {
      const { address: polled, compare: order, identity: identify, read: reader, shown: start } = latest.current;
      // Every page of one poll reads up to the same instant, so its cursors walk one fixed range.
      const to = clock.now();
      const page = (cursor: string | undefined) => (failures.failing ? quietly(() => reader(cursor, to)) : reader(cursor, to));
      const last = start?.[start.length - 1];
      const below = (r: R) => last !== undefined && order(r, last) > 0;
      // Before the list answers there is no row to stop at, so the poll reads the newest page alone and keeps the
      // cursor past it for the answer that stands in for a list read that failed.
      const pages: Promise<{ cursor?: string | null; overrun: string | null; rows: R[] }> = start === null ? page(undefined).then((p) => ({ cursor: p.nextCursor, overrun: null, rows: p.rows })) : followPages(page, new Set(start.map(identify)), identify, below);
      pages.then(
        (polledPages) => {
          if (!armed.alive) return;
          for (const path of failures.paths) reportRecovery(path);
          failures.paths.clear();
          failures.failing = false;
          // An answer for filters the reader has left since lands nowhere.
          const current = latest.current;
          if (current.address !== polled) return;
          if (current.shown === null) {
            // A list read still on its way answers for itself. One that failed left nothing to merge into, so the
            // newest page stands in for it untinted and clears its error.
            if (current.failed && polledPages.cursor !== undefined) setFollowed({ for: polled, fresh: new Set(), overrun: null, rows: polledPages.rows, standIn: { cursor: polledPages.cursor, to } });
            return;
          }
          const known = new Set(current.shown.map(identify));
          // A gap an earlier poll left stays until a reload, and Next walks on from the newest one, which reaches the
          // older ones too.
          const next = polledPages.overrun === null ? current.overrun : { cursor: polledPages.overrun, to };
          setFollowed({ for: polled, fresh: new Set(polledPages.rows.map(identify).filter((k) => !known.has(k))), overrun: next, rows: merged(current.shown, polledPages.rows, identify, order), standIn: current.standIn });
        },
        (err: unknown) => {
          if (!armed.alive) return;
          failures.failing = true;
          if (err instanceof ApiError && err.path !== null) failures.paths.add(err.path);
        },
      );
    }, FOLLOW_MS);
    return () => {
      armed.alive = false;
      clock.clearInterval(id);
    };
  }, [on]);

  const rows = following?.rows ?? list?.rows ?? [];
  const fresh = following?.fresh ?? new Set<string>();
  // Past an overrun, Next walks on from the last row Follow read, so the rows it left are the next page. A poll that
  // stood in for the list walks on from its own page, under the end it read to.
  const walk = overrun ?? standIn;
  const nextCursor = walk === null ? (list?.nextCursor ?? null) : walk.cursor;
  // The tint marks what the last poll brought, and no poll follows to take it away.
  const stopFollowing = () => {
    setFollow(false);
    setFollowed((f) => f && { ...f, fresh: new Set() });
  };
  const goNext = () => {
    if (!nextCursor) return;
    // The rows under the reader would move, so an older page stops the poll.
    stopFollowing();
    setPaging({ for: address, cursor: nextCursor, stack: [...stack, cursor], to: walk?.to ?? pagedTo });
  };
  const goPrevious = () => {
    const copy = [...stack];
    const previous = copy.pop();
    setPaging({ for: address, cursor: previous, stack: copy, to: pagedTo });
  };
  const toggleFollow = (checked: boolean) => {
    if (checked) setFollow(true);
    else stopFollowing();
  };
  return { error: standIn === null ? error : null, following, fresh, goNext, goPrevious, nextCursor, overrun, rows, setFollowed, stopFollowing, toggleFollow };
}
