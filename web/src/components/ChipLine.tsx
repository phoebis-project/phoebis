import { type RefObject, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { FilterChip } from '@/components/FilterChip';
import { type Chip, foldChips } from '@/lib/filterBar';

/** The chips the line may show before it folds, the most `foldChips` keeps. */
const MOST = 5;

const childrenOf = (line: HTMLElement, className: string) => [...line.children].filter((el): el is HTMLElement => el instanceof HTMLElement && el.classList.contains(className));

export interface ChipLineProps {
  /** The "Add filter" button, which takes focus once no chip is left to take it. */
  addFilterRef: RefObject<HTMLButtonElement | null>;
  /** The view's address, whose change folds an expanded line again. */
  address: string;
  chips: Chip[];
  onClearAll: () => void;
  onOpen: (field: string, anchor: HTMLElement) => void;
  onRemove: (field: string) => void;
}

/**
 * `div.chip-line` under the row, drawn while a chip stands: the shown chips, a dashed `button.chip.chip--more` reading
 * "+N more" with the fold's title, "Show fewer" while expanded, and "Clear all" (`.link-btn`) from two chips on. The
 * expanded state resets when the address changes. A remove moves focus to the next chip's body, else to
 * `addFilterRef`.
 */
export function ChipLine({ addFilterRef, address, chips, onClearAll, onOpen, onRemove }: ChipLineProps): JSX.Element | null {
  const lineRef = useRef<HTMLDivElement>(null);
  const [fit, setFit] = useState(MOST);
  // The expansion belongs to the address it was opened on, so a new address reads as folded with no reset to render.
  const [expandedFor, setExpandedFor] = useState<string | null>(null);
  const expanded = expandedFor === address;
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
  const measureRef = useRef(measure);
  // Every draw can change a chip's width (an option list loading turns ids into names), so each one measures again.
  useLayoutEffect(() => {
    measureRef.current = measure;
    measure();
  });
  const drawnLine = chips.length > 0;
  useEffect(() => {
    const line = lineRef.current;
    if (!drawnLine || line === null || typeof ResizeObserver !== 'function') return;
    const observer = new ResizeObserver(() => measureRef.current());
    observer.observe(line);
    return () => observer.disconnect();
  }, [drawnLine]);

  // A chip that leaves with the focus on it (its x, the popover's Remove, Clear all) drops focus to the body, so the
  // chip that stood after it takes the focus, its fold when it is folded, else Add filter.
  const before = useRef(chips);
  useLayoutEffect(() => {
    const was = before.current;
    before.current = chips;
    const at = document.activeElement;
    if (at !== null && at !== document.body && at.isConnected) return;
    const gone = was.findIndex((c) => !chips.some((n) => n.field === c.field));
    if (gone < 0) return;
    const next = was.slice(gone + 1).find((c) => chips.some((n) => n.field === c.field));
    const index = next === undefined ? -1 : chips.findIndex((c) => c.field === next.field);
    const line = lineRef.current;
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
    <div ref={lineRef} className={expanded ? 'chip-line is-expanded' : 'chip-line'}>
      {drawn.map((chip, i) => (
        <FilterChip key={chip.field} chip={chip} hidden={!expanded && i >= shown.length} onOpen={(anchor) => onOpen(chip.field, anchor)} onRemove={() => onRemove(chip.field)} />
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
