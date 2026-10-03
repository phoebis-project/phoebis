/**
 * Brings an element to the top of the view, smoothly unless the reader asked for reduced motion. jsdom implements no
 * `scrollIntoView`, so the smoke run passes through without a scroll unless it stubs one.
 */
export function scrollToElement(el: Element | null | undefined): void {
  if (!el || typeof el.scrollIntoView !== 'function') return;
  const reduced = typeof window.matchMedia === 'function' && window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  el.scrollIntoView({ behavior: reduced ? 'auto' : 'smooth', block: 'start' });
}
