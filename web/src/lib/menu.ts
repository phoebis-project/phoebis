export type MenuPlacement = { kind: 'down' } | { kind: 'up' } | { kind: 'float'; left: number; top: number };

/** Which edge of its trigger a list lines up with: the right for a menu at a row's end, the left for one at its start. */
export type MenuAlign = 'left' | 'right';

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
 * it otherwise, aligned to the same edge of it as the in-flow list is.
 */
export function menuPlacement(trigger: { bottom: number; left: number; right: number; top: number }, wrap: MenuWrap | null, list: { height: number; width: number }, viewportHeight: number, align: MenuAlign = 'right'): MenuPlacement {
  // The client box, not the border box: a wrap that scrolls sideways draws its scrollbar inside its border, over the
  // last rows of a list that would end there.
  const top = wrap ? wrap.getBoundingClientRect().top + wrap.clientTop : 0;
  const floor = wrap ? Math.min(top + wrap.clientHeight, viewportHeight) : viewportHeight;
  const ceiling = wrap ? Math.max(top, 0) : 0;
  const need = list.height + 4;
  if (floor - trigger.bottom >= need) return { kind: 'down' };
  if (trigger.top - ceiling >= need) return { kind: 'up' };
  return floatAt(trigger, list, viewportHeight, align);
}

/** The fixed layer at a trigger: below it when the viewport has the room and above it otherwise, on the `align` edge. */
export function floatAt(trigger: { bottom: number; left: number; right: number; top: number }, list: { height: number; width: number }, viewportHeight: number, align: MenuAlign = 'right'): Extract<MenuPlacement, { kind: 'float' }> {
  const need = list.height + 4;
  const below = viewportHeight - trigger.bottom >= need;
  return { kind: 'float', left: align === 'left' ? trigger.left : trigger.right - list.width, top: below ? trigger.bottom + 4 : trigger.top - need };
}
