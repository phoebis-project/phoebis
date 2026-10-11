import { type Dispatch, type SetStateAction, useEffect, useRef, useState } from 'react';
import { ApiError, quietly, reportRecovery } from '@/api/http';
import { clock } from '@/lib/clock';
import { FOLLOW_MS, type Followed, followPages, merged } from '@/lib/follow';

/**
 * The poll both pages run: every FOLLOW_MS while `on`, read the newest pages up to now over the rows shown under
 * `address` (the followed rows it holds, else `base`), merge them under the fresh tint, keep an overrun's cursor with
 * its poll's end, and toast a failing streak once. The caller hands in the base query's rows and reads `following`
 * back, so nothing the caller passes depends on the hook's own state.
 */
export function useFollow<R>(input: { address: string; base: R[] | null; compare: (a: R, b: R) => number; identity: (row: R) => string; on: boolean; paged: boolean; read: (cursor: string | undefined, to: number) => Promise<{ nextCursor: string | null; rows: R[] }> }): { following: Followed<R> | null; setFollowed: Dispatch<SetStateAction<Followed<R> | null>> } {
  const { address, base, compare, identity, on, paged, read } = input;
  const [followed, setFollowed] = useState<Followed<R> | null>(null);
  // A cursor page draws the list's own rows, and the followed ones return with the first page.
  const following = followed !== null && followed.for === address && !paged ? followed : null;
  // The rows the poll merges into: those followed under this address, else the list's once it answers this query. While
  // the list still answers an earlier filter, its rows belong to that filter, so there is nothing to merge into yet.
  const shown = following?.rows ?? base;
  const overrun = following?.overrun ?? null;
  // The poll outlives renders, so it reads the current address, rows and reader through a ref.
  const latest = useRef({ address, compare, identity, overrun, read, shown });
  latest.current = { address, compare, identity, overrun, read, shown };
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
  return { following, setFollowed };
}
