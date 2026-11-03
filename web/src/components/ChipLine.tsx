import { type RefObject, useLayoutEffect, useRef, useState } from 'react';
import { FilterChip } from '@/components/FilterChip';
import { type Chip, foldChips } from '@/lib/filterBar';
import { useResizeObserver } from '@/lib/hooks';

/** The chips the line may show before it folds, the most `foldChips` keeps. */
const MOST = 5;

const childrenOf = (line: HTMLElement, className: string) => [...line.children].filter((el): el is HTMLElement => el instanceof HTMLElement && el.classList.contains(className));
/** The chips of a per-value dimension share their field, so the line keys and follows a chip by its field and value. */
const idOf = (chip: Chip) => `${chip.field}:${chip.value ?? ''}`;

export interface ChipLineProps {
  /** The "Add filter" button, which takes focus once no chip is left to take it. */
  addFilterRef: RefObject<HTMLButtonElement | null>;
  /** The view's address, whose change folds an expanded line again. */
  address: string;
  chips: Chip[];
  /**
   * The chip body the Add filter box opened from, or null. A per-value dimension's chips share their field, so the body
   * names the one chip the box stands for.
   */
  editing?: HTMLElement | null;
  onClearAll: () => void;
  onOpen: (field: string, anchor: HTMLElement) => void;
  /** A chip's x, with the chip's own value where its dimension draws a chip per value. */
  onRemove: (field: string, value?: string) => void;
}

/**
 * `div.chip-line` under the row, drawn while a chip stands: the shown chips, a dashed `button.chip.chip--more` reading
 * "+N more" with the fold's title, "Show fewer" while expanded, and "Clear all" (`.link-btn`) from two chips on. The
 * expanded state resets when the address changes. A remove moves focus to the next chip's body, else to
 * `addFilterRef`, and a chip folded away with the focus on it hands the focus to the fold chip. The bar keeps the line
 * mounted with no chips: it draws nothing then, and the focus hand-off after Clear all or the last chip's x runs in its
 * effects.
 */
export function ChipLine({ addFilterRef, address, chips, editing = null, onClearAll, onOpen, onRemove }: ChipLineProps): JSX.Element | null {
  const lineRef = useRef<HTMLDivElement>(null);
  const [fit, setFit] = useState(MOST);
  // The expansion is the bar's own and resets with the address. Clearing it as the address moves, rather than only
  // comparing, keeps a later return to that address folded.
  const [expandedFor, setExpandedFor] = useState<string | null>(null);
  if (expandedFor !== null && expandedFor !== address) setExpandedFor(null);
  const expanded = expandedFor === address;
  // The chip body that last took the focus, which a write from the expanded line can fold away with the focus on it.
  const focusedBody = useRef<HTMLElement | null>(null);
  const { folded, shown, title } = foldChips(chips, (count) => count <= fit);
  const drawn = expanded ? chips : chips.slice(0, MOST);

  // How many of the first five chips end inside the line with room left for the fold and Clear all. The chips past
  // the last fit are drawn hidden, so each measure shows them for the reading and hides them again before paint.
  const measure = () => {
    const line = lineRef.current;
    if (line === null || line.classList.contains('is-expanded')) return;
    const width = line.clientWidth;
    // A line no layout measured (jsdom, the smoke) reads as room for five, so the checks see the fold at five.
    if (width === 0) {
      setFit(MOST);
      return;
    }
    const items = childrenOf(line, 'chip--filter');
    const hidden = items.map((el) => el.hidden);
    for (const el of items) el.hidden = false;
    const widths = items.map((el) => el.getBoundingClientRect().width);
    items.forEach((el, i) => {
      el.hidden = hidden[i];
    });
    const more = childrenOf(line, 'chip--more')[0]?.getBoundingClientRect().width ?? 0;
    const clear = childrenOf(line, 'link-btn')[0]?.getBoundingClientRect().width ?? 0;
    const gap = parseFloat(getComputedStyle(line).columnGap) || 0;
    const fits = (count: number) => {
      let total = widths.slice(0, count).reduce((sum, w) => sum + w, 0) + gap * (count - 1);
      if (count < chips.length) total += gap + more;
      if (chips.length >= 2) total += gap + clear;
      return total <= width;
    };
    setFit(foldChips(chips, fits).shown.length);
  };
  // Every draw can change a chip's width (an option list loading turns ids into names), so each one measures again.
  useLayoutEffect(() => {
    measure();
  });
  useResizeObserver(lineRef, measure, chips.length > 0);

  // A chip that leaves with the focus on it (its x, the popover's Remove, Clear all) drops focus to the body, so the
  // chip that stood after it takes the focus, its fold when it is folded, else Add filter. A focused body the fold
  // hides hands the focus to the fold chip, whatever other chip the same commit removed.
  const before = useRef(chips);
  useLayoutEffect(() => {
    const was = before.current;
    before.current = chips;
    const at = document.activeElement;
    // A chip body the fold hides can still hold the focus while the commit's effects run, since a browser blurs it
    // later if at all, so a focus left on that body counts as dropped too.
    const dropped = at === null || at === document.body || !at.isConnected || (at === focusedBody.current && at.closest('[hidden]') !== null);
    if (!dropped) return;
    const gone = at !== null && at !== document.body && at.isConnected ? -1 : was.findIndex((c) => !chips.some((n) => idOf(n) === idOf(c)));
    const line = lineRef.current;
    if (gone < 0) {
      // A chip that stays but left the drawn set (a write from the expanded line resets the expansion) took the focus
      // with it, and the fold chip now stands for it.
      const body = focusedBody.current;
      if (body === null || (body.isConnected && body.closest('[hidden]') === null)) return;
      focusedBody.current = null;
      ((line === null ? undefined : childrenOf(line, 'chip--more')[0]) ?? addFilterRef.current)?.focus({ preventScroll: true });
      return;
    }
    const next = was.slice(gone + 1).find((c) => chips.some((n) => idOf(n) === idOf(c)));
    const index = next === undefined ? -1 : chips.findIndex((c) => idOf(c) === idOf(next));
    if (index < 0 || line === null) {
      addFilterRef.current?.focus({ preventScroll: true });
      return;
    }
    const chip = childrenOf(line, 'chip--filter')[index];
    const to = chip !== undefined && !chip.hidden ? chip.querySelector<HTMLElement>('.chip__body') : childrenOf(line, 'chip--more')[0];
    to?.focus({ preventScroll: true });
  });

  if (chips.length === 0) return null;
  return (
    <div
      ref={lineRef}
      className={expanded ? 'chip-line is-expanded' : 'chip-line'}
      onFocus={(e) => {
        focusedBody.current = e.target instanceof HTMLElement && e.target.classList.contains('chip__body') ? e.target : null;
      }}
      onBlur={(e) => {
        // A focus that moved on to another control, or left a body still shown (a click on the page's background), is
        // no longer the line's to hand off. The blur a browser fires as the fold hides the body keeps it.
        if (e.relatedTarget !== null || (e.target.isConnected && e.target.closest('[hidden]') === null)) focusedBody.current = null;
      }}
    >
      {drawn.map((chip, i) => (
        <FilterChip key={idOf(chip)} chip={chip} editing={editing} hidden={!expanded && i >= shown.length} onOpen={(anchor) => onOpen(chip.field, anchor)} onRemove={() => onRemove(chip.field, chip.value)} />
      ))}
      {(expanded || folded.length > 0) && (
        <button key="fold" type="button" className="chip chip--more" title={expanded ? undefined : title} aria-expanded={expanded} onClick={() => setExpandedFor(expanded ? null : address)}>
          {expanded ? 'Show fewer' : `+${folded.length} more`}
        </button>
      )}
      {chips.length >= 2 && (
        <button key="clear" type="button" className="link-btn" onClick={onClearAll}>
          Clear all
        </button>
      )}
    </div>
  );
}
