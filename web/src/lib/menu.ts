export type MenuPlacement = { kind: 'down' } | { kind: 'up' } | { kind: 'float'; left: number; top: number };

/** The parts of a clipping table wrap the placement reads, so a test can stand one in. */
export interface MenuWrap {
  clientHeight: number;
  clientTop: number;
  getBoundingClientRect(): { top: number };
}

/**
 * Where a menu's list goes, from its trigger's box, the wrap that clips it (null when nothing does) and the list's
 * own size, all in viewport pixels: below the trigger in flow when the room there holds it, above it in flow when
 * only the room above does, else in a fixed layer at the trigger, below it when the viewport has the room and above
 * it otherwise, right-aligned to it as the in-flow list is.
 */
export function menuPlacement(trigger: { bottom: number; right: number; top: number }, wrap: MenuWrap | null, list: { height: number; width: number }, viewportHeight: number): MenuPlacement {
  // The client box, not the border box: a wrap that scrolls sideways draws its scrollbar inside its border, over the
  // last rows of a list that would end there.
  const top = wrap ? wrap.getBoundingClientRect().top + wrap.clientTop : 0;
  const floor = wrap ? Math.min(top + wrap.clientHeight, viewportHeight) : viewportHeight;
  const ceiling = wrap ? Math.max(top, 0) : 0;
  const need = list.height + 4;
  if (floor - trigger.bottom >= need) return { kind: 'down' };
  if (trigger.top - ceiling >= need) return { kind: 'up' };
  const below = viewportHeight - trigger.bottom >= need;
  return { kind: 'float', left: trigger.right - list.width, top: below ? trigger.bottom + 4 : trigger.top - need };
}
