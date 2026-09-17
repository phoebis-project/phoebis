import { useCallback, useEffect, useRef, useState, type RefObject } from 'react';

export interface SpyOptions {
  /** Skip work while the owning surface is hidden, e.g. a closed dialog. */
  enabled?: boolean;
  /** Distance from the top of the scroller that counts as "in view", for a sticky header. */
  offset?: number;
  /** The scrolling element. Omitted means the page itself. */
  root?: RefObject<HTMLElement | null>;
}

const scrollerOf = (root?: RefObject<HTMLElement | null>): HTMLElement | null => (root ? root.current : (document.scrollingElement as HTMLElement | null));

const topWithin = (el: HTMLElement, scroller: HTMLElement, page: boolean) => (page ? el.getBoundingClientRect().top : el.getBoundingClientRect().top - scroller.getBoundingClientRect().top);

/**
 * The section covering the most of the visible band, measuring each one from
 * its own heading down to the next. Used at the very bottom of a scroller,
 * where a short last section never reaches the offset line and the rail would
 * otherwise name it while most of the view still belongs to its predecessor.
 */
function widestInView(ids: string[], scroller: HTMLElement, page: boolean, offset: number): string | null {
  const bounds = ids.map((id) => ({ id, el: document.getElementById(id) })).filter((b): b is { el: HTMLElement; id: string } => b.el !== null).map((b) => ({ id: b.id, top: topWithin(b.el, scroller, page) }));
  const viewBottom = scroller.clientHeight;
  const contentBottom = scroller.scrollHeight - scroller.scrollTop;
  let best: string | null = null;
  let bestArea = 0;
  for (const [i, b] of bounds.entries()) {
    const area = Math.min(i + 1 < bounds.length ? bounds[i + 1].top : contentBottom, viewBottom) - Math.max(b.top, offset);
    if (area > bestArea) {
      bestArea = area;
      best = b.id;
    }
  }
  return best;
}

/**
 * Reads the section whose heading sits at the top of the scroller, and returns
 * a setter for jumping straight to one. The setter wins for a moment so a
 * smooth scroll does not light up every section it passes on the way.
 */
export function useScrollSpy(ids: string[], { enabled = true, offset = 0, root }: SpyOptions = {}): [string, (id: string) => void] {
  const key = ids.join('|');
  const [active, setActive] = useState(() => ids[0] ?? '');
  const jumpedAt = useRef(0);

  const select = useCallback((id: string) => {
    jumpedAt.current = Date.now();
    setActive(id);
  }, []);

  useEffect(() => {
    if (!enabled) return;
    const list = key ? key.split('|') : [];
    if (list.length === 0) return;
    const scroller = scrollerOf(root);
    if (!scroller) return;
    const page = !root;
    const target: EventTarget = page ? window : scroller;

    const read = () => {
      if (Date.now() - jumpedAt.current < 600) return;
      // Past the last heading nothing can reach the offset line any more, so
      // fall back to area: whichever section fills most of the view owns the
      // rail. The last one wins only once it actually dominates the screen.
      if (scroller.scrollTop + scroller.clientHeight >= scroller.scrollHeight - 2) {
        setActive(widestInView(list, scroller, page, offset) ?? list[list.length - 1]);
        return;
      }
      let current = list[0];
      for (const id of list) {
        const el = document.getElementById(id);
        if (el && topWithin(el, scroller, page) <= offset + 1) current = id;
      }
      setActive(current);
    };

    read();
    target.addEventListener('scroll', read, { passive: true });
    window.addEventListener('resize', read);
    // A group expanding inside a section moves everything below it without any
    // scroll event, so the highlight would keep pointing at whatever was in view
    // before. Watch the sections and the scroller instead of trusting scroll.
    const observer = typeof ResizeObserver === 'function' ? new ResizeObserver(() => read()) : null;
    if (observer) {
      observer.observe(scroller);
      for (const id of list) {
        const el = document.getElementById(id);
        if (el) observer.observe(el);
      }
    }
    return () => {
      observer?.disconnect();
      target.removeEventListener('scroll', read);
      window.removeEventListener('resize', read);
    };
  }, [key, enabled, offset, root]);

  return [active, select];
}

/** Brings a section to the top of its scroller, honouring a reduced-motion preference. */
export function scrollToSection(id: string, { offset = 0, root }: { offset?: number; root?: RefObject<HTMLElement | null> } = {}): void {
  const scroller = scrollerOf(root);
  const el = document.getElementById(id);
  if (!scroller || !el) return;
  const top = Math.max(0, scroller.scrollTop + topWithin(el, scroller, !root) - offset);
  const smooth = !window.matchMedia?.('(prefers-reduced-motion: reduce)').matches;
  // jsdom leaves Element.scrollTo undefined, so the smoke harness takes the plain assignment.
  if (typeof scroller.scrollTo === 'function') scroller.scrollTo({ top, behavior: smooth ? 'smooth' : 'auto' });
  else scroller.scrollTop = top;
}
