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
 * with its poll's end, and toast a failing streak once. The caller hands in the list's last answer and its paging
 * state, and reads back `following`, the rows to draw, the pager's next cursor and the handlers of the pager and the
 * switch, so nothing the caller passes depends on the hook's own state.
 */
export function useFollow<R>(input: {
  address: string;
  compare: (a: R, b: R) => number;
  identity: (row: R) => string;
  list: { nextCursor: string | null; rows: R[] } | null;
  on: boolean;
  paging: Paging;
  read: (cursor: string | undefined, to: number) => Promise<{ nextCursor: string | null; rows: R[] }>;
  setFollow: (on: boolean) => void;
  setPaging: Dispatch<SetStateAction<Paging>>;
  stale: boolean;
}) {
  const { address, compare, identity, list, on, paging, read, setFollow, setPaging, stale } = input;
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
  // The poll outlives renders, so it reads the current address, rows and reader through a ref.
  const latest = useRef({ address, compare, identity, overrun, read, shown });
  useLayoutEffect(() => {
    latest.current = { address, compare, identity, overrun, read, shown };
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
      // Before the list answers there is no row to stop at, so the poll reads the newest page alone.
      const pages: Promise<{ overrun: string | null; rows: R[] }> = start === null ? page(undefined).then((p) => ({ overrun: null, rows: p.rows })) : followPages(page, new Set(start.map(identify)), identify);
      pages.then(
        (polledPages) => {
          if (!armed.alive) return;
          for (const path of failures.paths) reportRecovery(path);
          failures.paths.clear();
          failures.failing = false;
          // An answer for filters the reader has left since, or for an address the list has not answered yet, lands
          // nowhere.
          const current = latest.current;
          if (current.address !== polled || current.shown === null) return;
          const known = new Set(current.shown.map(identify));
          // A gap an earlier poll left stays until a reload, and Next walks on from the newest one, which reaches the
          // older ones too.
          const next = polledPages.overrun === null ? current.overrun : { cursor: polledPages.overrun, to };
          setFollowed({ for: polled, fresh: new Set(polledPages.rows.map(identify).filter((k) => !known.has(k))), overrun: next, rows: merged(current.shown, polledPages.rows, identify, order) });
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
  // Past an overrun, Next walks on from the last row Follow read, so the rows it left are the next page.
  const nextCursor = overrun?.cursor ?? list?.nextCursor ?? null;
  // The tint marks what the last poll brought, and no poll follows to take it away.
  const stopFollowing = () => {
    setFollow(false);
    setFollowed((f) => f && { ...f, fresh: new Set() });
  };
  const goNext = () => {
    if (!nextCursor) return;
    // The rows under the reader would move, so an older page stops the poll.
    stopFollowing();
    setPaging({ for: address, cursor: nextCursor, stack: [...stack, cursor], to: overrun?.to ?? pagedTo });
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
  return { following, fresh, goNext, goPrevious, nextCursor, overrun, rows, setFollowed, stopFollowing, toggleFollow };
}
