import { createContext, type RefObject, useContext, useEffect, useId, useLayoutEffect, useMemo } from 'react';
import type { PauseCause } from '@/lib/follow';
import { useKeyedMap, useLatest } from '@/lib/hooks';

/** What an open overlay holds a page's auto reload for, in the refresh control's words for the pause. */
export type HoldCause = Extract<PauseCause, 'dialog' | 'menu'>;

/** A page's register of the overlays open under it, each under its own id with its cause. */
export interface HoldHost {
  add: (id: string, cause: HoldCause) => void;
  remove: (id: string) => void;
}

/**
 * The context a page provides: every popover, menu and cost reference dialog under it holds the page's auto reload
 * while it is open. A component under no provider holds nothing.
 */
export const PopoverHost = createContext<HoldHost | null>(null);

/** Holds the page's auto reload for `cause` while `active`, and lets go as it turns false or the caller unmounts. */
export function useHold(active: boolean, cause: HoldCause): void {
  const host = useContext(PopoverHost);
  const id = useId();
  useEffect(() => {
    if (!active || host === null) return;
    host.add(id, cause);
    return () => host.remove(id);
  }, [active, cause, host, id]);
}

/**
 * A page's host for its `PopoverHost` provider, and the cause its open overlays hold auto reload for: a dialog before
 * a menu, null while none is open.
 */
export function useHoldHost(): { cause: HoldCause | null; host: HoldHost } {
  const { map: holds, set: add, drop: remove } = useKeyedMap<HoldCause>();
  // An open overlay registers again whenever the host's value changes, so the value is made once.
  const host = useMemo<HoldHost>(() => ({ add, remove }), [add, remove]);
  const causes = [...holds.values()];
  return { cause: causes.includes('dialog') ? 'dialog' : causes.length > 0 ? 'menu' : null, host };
}

/**
 * Calls `onOutside` on a mouse press outside every node `inside` names, while `active` holds. Both are read when the
 * press lands, so a caller passes fresh functions each render and the listener stays put.
 */
export function useOutsidePress(active: boolean, inside: () => (Node | null | undefined)[], onOutside: () => void): void {
  const latest = useLatest({ inside, onOutside });
  useEffect(() => {
    if (!active) return;
    const down = (e: MouseEvent) => {
      const target = e.target as Node;
      if (!latest.current.inside().some((node) => node?.contains(target))) latest.current.onOutside();
    };
    document.addEventListener('mousedown', down);
    return () => document.removeEventListener('mousedown', down);
  }, [active, latest]);
}

/**
 * Leaves a fixed layer when its trigger moves: a fixed layer stays where it was drawn while the page moves under it, so
 * it leaves rather than float beside another row. A scroll is read by whether it moved `target` from `rest`, the box it
 * rested at once the layer opened: the scroll that brought the row into view can report after the click that opened
 * it, and a scroll inside the layer or in another pane moves nothing. A refresh that re-sorts the rows moves the
 * trigger with no scroll or resize to report it, so every render of the open layer reads the trigger by the same rule.
 * A resize leaves at once.
 */
export function useLeaveOnMove(active: boolean, target: RefObject<HTMLElement>, rest: RefObject<{ left: number; top: number }>, leave: () => void): void {
  const latest = useLatest(leave);
  useLayoutEffect(() => {
    if (active && movedFrom(target, rest)) leave();
  });
  useEffect(() => {
    if (!active) return;
    const onScroll = () => {
      if (movedFrom(target, rest)) latest.current();
    };
    const onResize = () => latest.current();
    window.addEventListener('scroll', onScroll, true);
    window.addEventListener('resize', onResize);
    return () => {
      window.removeEventListener('scroll', onScroll, true);
      window.removeEventListener('resize', onResize);
    };
  }, [active, latest, rest, target]);
}
const movedFrom = (target: RefObject<HTMLElement>, rest: RefObject<{ left: number; top: number }>): boolean => {
  const at = target.current?.getBoundingClientRect();
  return !at || !rest.current || at.top !== rest.current.top || at.left !== rest.current.left;
};
