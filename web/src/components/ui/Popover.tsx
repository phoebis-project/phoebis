import { createContext, type FocusEvent, type KeyboardEvent, type ReactNode, useCallback, useContext, useEffect, useLayoutEffect, useRef } from 'react';
import { createPortal } from 'react-dom';
import { useLatest } from '@/lib/hooks';
import { useOutsidePress } from './primitives';

/** The room a fixed box keeps from the window's edges and from its anchor. */
export const GAP = 8;
// Native fields count, so a box whose first control is a search field opens onto it.
export const TABBABLE = 'a[href], button:not(:disabled), input:not(:disabled):not([type="hidden"]), select:not(:disabled), textarea:not(:disabled), [tabindex]:not([tabindex="-1"])';

/**
 * The first stop after `anchor` in the page's Tab order, outside the box: where Tab would land if the box stood beside
 * its anchor rather than at the end of the body or the dialog.
 */
function stopAfter(anchor: HTMLElement, box: HTMLElement): HTMLElement | undefined {
  const scope = anchor.closest('dialog[open]') ?? document;
  return [...scope.querySelectorAll<HTMLElement>(TABBABLE)].find(
    (el) => (anchor.compareDocumentPosition(el) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0 && !anchor.contains(el) && !box.contains(el) && el.closest('[hidden]') === null && (el.checkVisibility?.() ?? true),
  );
}

/**
 * Places a fixed box under its anchor, above it where it would pass the window's foot, no taller than the window less
 * `GAP` above and below and kept `GAP` inside it, then reveals it. `left` answers the box's left edge from its measured
 * box and the anchor's.
 */
export function placeFixed(el: HTMLElement, anchor: HTMLElement, left: (box: DOMRect, at: DOMRect) => number): void {
  // A classic scrollbar stands inside the window's inner box but outside the fixed box's containing block.
  const tall = document.documentElement.clientHeight;
  el.style.maxHeight = `${Math.max(0, tall - 2 * GAP)}px`;
  // A fixed box shrinks to the room right of where it last stood, so it is measured from the window's corner.
  el.style.left = '0px';
  el.style.top = '0px';
  const box = el.getBoundingClientRect();
  const at = anchor.getBoundingClientRect();
  el.style.left = `${left(box, at)}px`;
  const below = at.bottom + GAP;
  const top = below + box.height > tall - GAP ? at.top - GAP - box.height : below;
  // A list as tall as the window over a low anchor starts at the window's top rather than above it.
  el.style.top = `${Math.max(GAP, Math.min(top, tall - GAP - box.height))}px`;
  el.style.visibility = 'visible';
}

/**
 * A box's blur handler, closing it when focus leaves for an element outside the box and its anchor. A blur naming no
 * element (a click on a button Safari gives no focus, the window losing focus) closes nothing, as a menu's does.
 */
export const blurLeaves = (anchor: HTMLElement, close: () => void) => (e: FocusEvent<HTMLElement>) => {
  const to = e.relatedTarget;
  if (to !== null && !e.currentTarget.contains(to) && !anchor.contains(to)) close();
};

/** The context a bar provides: every Popover under it reports its open state, so the page can pause auto reload while a menu or a popover is open. */
export const PopoverHost = createContext<{ add: (id: string) => void; remove: (id: string) => void } | null>(null);

export interface PopoverProps {
  /** The element the box hangs 8 px under, left edges aligned, and the one focus returns to on Escape. */
  anchor: HTMLElement | null;
  children: ReactNode;
  className?: string;
  id: string;
  /** The box's name while no element inside it names it. */
  label?: string;
  /** The id of the element inside the box that names it. */
  labelledBy?: string;
  /** Escape (consumed, so a drawer under the box stays open, and focus returned to the anchor), a mousedown outside the box and its anchor, and focus leaving for anything but the anchor. */
  onClose: () => void;
  open: boolean;
  /** A value whose change places the box again, such as the step an opener shows, since new content can change the box's size. */
  placeKey?: unknown;
  role?: 'dialog' | 'menu';
}

/**
 * StripPopover's rules on any anchor: createPortal at position fixed, visibility hidden until placed, kept 8 px inside
 * the window, flipped above when it would pass the window's foot, placed again on resize and any scroll, closed once
 * its anchor has left the document or the layout, aria-modal false, focus moved to the first tabbable (else the box)
 * on open. Tab past the last stop moves on to the stop after the anchor and Shift+Tab before the first returns to the
 * anchor, each closing the box.
 */
export function Popover({ anchor, children, className, id, label, labelledBy, onClose, open, placeKey, role = 'dialog' }: PopoverProps): JSX.Element | null {
  const ref = useRef<HTMLDivElement>(null);
  const host = useContext(PopoverHost);
  const shown = open && anchor !== null;
  // Read through a ref, so an opener's new close function each render places nothing again.
  const closeRef = useLatest(onClose);
  const place = useCallback(() => {
    const el = ref.current;
    if (el === null || anchor === null) return;
    // A box over an anchor that has left the document (as the decision strip's popover also checks) or the layout (the
    // top bar's legend button below 600 px) would hang from the window's corner, so it closes. A DOM with no layout
    // engine answers an empty getClientRects for every anchor and has no checkVisibility, so only the latter is read.
    if (!anchor.isConnected || anchor.checkVisibility?.() === false) {
      closeRef.current();
      return;
    }
    const wide = document.documentElement.clientWidth;
    placeFixed(el, anchor, (box, at) => Math.max(GAP, Math.min(at.left, wide - GAP - box.width)));
  }, [anchor, closeRef]);
  useLayoutEffect(place, [open, place, placeKey]);
  useEffect(() => {
    const el = ref.current;
    if (!shown || el === null) return;
    // The box's own scroll moves neither it nor its anchor.
    const scrolled = (e: Event) => {
      if (!(e.target instanceof Node && el.contains(e.target))) place();
    };
    window.addEventListener('resize', place);
    window.addEventListener('scroll', scrolled, true);
    // Content that loads or filters after the box opens changes its size, and a flipped box would keep its old top.
    const observer = typeof ResizeObserver === 'function' ? new ResizeObserver(place) : null;
    observer?.observe(el);
    return () => {
      window.removeEventListener('resize', place);
      window.removeEventListener('scroll', scrolled, true);
      observer?.disconnect();
    };
  }, [place, shown]);
  // After the placement, since a box still hidden takes no focus. A step that focused its own field as it mounted
  // keeps it.
  useEffect(() => {
    const el = ref.current;
    if (!shown || el === null || el.contains(document.activeElement)) return;
    (el.querySelector<HTMLElement>(TABBABLE) ?? el).focus({ preventScroll: true });
  }, [anchor, shown]);
  useEffect(() => {
    if (!shown || host === null) return;
    host.add(id);
    return () => host.remove(id);
  }, [host, id, shown]);
  useOutsidePress(shown && anchor !== null, () => [ref.current, anchor], onClose);
  if (!open || anchor === null) return null;
  const leave = blurLeaves(anchor, onClose);
  const keys = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
      anchor.focus({ preventScroll: true });
      onClose();
      return;
    }
    // The box stands at the end of the body, so Tab off either end would leave the page or land at its far end, with
    // the box left open behind. The keys take the walk back beside the anchor.
    if (e.key !== 'Tab') return;
    const root = e.currentTarget;
    const stops = [...root.querySelectorAll<HTMLElement>(TABBABLE)];
    const atEdge = e.shiftKey ? e.target === root || e.target === stops[0] : e.target === (stops.length > 0 ? stops[stops.length - 1] : root);
    if (!atEdge) return;
    e.preventDefault();
    const to = e.shiftKey ? anchor : (stopAfter(anchor, root) ?? anchor);
    to.focus({ preventScroll: to === anchor });
    // Focus leaving for anything but the anchor has closed the box already.
    if (to === anchor) onClose();
  };
  return createPortal(
    // Hidden until `place` has put it under its anchor, so a frame painted before then shows nothing. React writes a
    // style key only when its value changes between renders, so the reveal `place` writes on the element outlives a
    // re-render.
    <div
      ref={ref}
      className={className ? `popover ${className}` : 'popover'}
      id={id}
      role={role}
      aria-modal="false"
      aria-label={labelledBy === undefined ? label : undefined}
      aria-labelledby={labelledBy}
      tabIndex={-1}
      style={{ position: 'fixed', visibility: 'hidden' }}
      onBlur={leave}
      onKeyDown={keys}
    >
      {children}
    </div>,
    // A modal dialog leaves the rest of the document inert, so a box opened from inside one stands inside it.
    anchor.closest('dialog[open]') ?? document.body,
  );
}
