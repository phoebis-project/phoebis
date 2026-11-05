import { useEffect, useRef, type RefObject } from 'react';
import { useLocation } from 'react-router-dom';
import { anchorOf } from '@/lib/attention';
import { scrollToElement } from '@/lib/scroll';

/**
 * The address's `#<id>` lands on the element of that id among those `selector` matches once `ready(id)` holds,
 * whenever the list arrives after the mount, and once per arrival: the page's own address rewrites keep the hash and
 * must not scroll the page back. The element is looked up inside `pageRef`, so an id such as `root` cannot name the
 * app's container. `deps` are the values `ready` reads.
 */
export function useHashLanding(pageRef: RefObject<HTMLElement>, selector: string, ready: (id: string) => boolean, deps: readonly unknown[]): void {
  const location = useLocation();
  const landed = useRef<string | null>(null);
  useEffect(() => {
    if (location.hash === landed.current) return;
    const id = anchorOf(location.hash);
    if (id === null) {
      landed.current = location.hash;
      return;
    }
    if (!ready(id)) return;
    const target = [...(pageRef.current?.querySelectorAll<HTMLElement>(selector) ?? [])].find((el) => el.id === id);
    if (!target) return;
    landed.current = location.hash;
    scrollToElement(target);
    target.focus({ preventScroll: true });
    // eslint-disable-next-line react-hooks/exhaustive-deps -- `deps` names what the fresh `ready` closure reads
  }, [location.hash, pageRef, selector, ...deps]);
}
