import { useEffect, useId, useRef, useState } from 'react';
import { positionOf, UNIT_LABEL, UNIT_LEGEND, UNIT_LEGEND_NOTE, UNIT_POSITIONS, UNIT_SHORT, type UnitPosition } from '@/lib/unit';
import { IconInfo } from './icons';
import { Select } from './primitives';

/**
 * The unit every view prices in. At 900 px and below the bar spans the viewport, so each position reads its short
 * label.
 */
export function UnitSwitch({ onChange, short, unit }: { onChange: (u: UnitPosition) => void; short: boolean; unit: UnitPosition }) {
  return (
    // Keyed on the label set: Chromium keeps a select's width when its options' text changes, so a resize across the
    // breakpoint would draw the short labels in the long labels' box, or a long label cut short in the other direction.
    <Select key={short ? 'short' : 'long'} aria-label="Unit" className="unit-switch" value={unit} onChange={(e) => onChange(positionOf(e.target.value))}>
      {UNIT_POSITIONS.map((p) => (
        <option key={p} value={p}>
          {short ? UNIT_SHORT[p] : UNIT_LABEL[p]}
        </option>
      ))}
    </Select>
  );
}

const LEGEND_LABEL = 'What the units mean';

/**
 * The unit positions explained, a popover from the info button beside the switch. It takes the focus as it opens,
 * Escape closes it and hands the focus back to the button, and a press outside closes it with the focus left where the
 * press lands, as the console's popovers do.
 */
export function UnitLegend() {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLSpanElement>(null);
  const buttonRef = useRef<HTMLButtonElement>(null);
  const boxRef = useRef<HTMLDivElement>(null);
  const id = useId();
  useEffect(() => {
    if (!open) return;
    // At a phone's width the open box runs past the window, so a plain focus would scroll the page to it.
    boxRef.current?.focus({ preventScroll: true });
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return;
      buttonRef.current?.focus({ preventScroll: true });
      setOpen(false);
    };
    // The button sits inside the anchor too, so its own press toggles rather than closing and opening again.
    const onPointer = (e: Event) => {
      if (!ref.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener('keydown', onKey);
    document.addEventListener('pointerdown', onPointer);
    return () => {
      document.removeEventListener('keydown', onKey);
      document.removeEventListener('pointerdown', onPointer);
    };
  }, [open]);
  return (
    <span className="unit-legend__anchor" ref={ref}>
      <button ref={buttonRef} type="button" className="unit-legend__button" aria-label={LEGEND_LABEL} title={LEGEND_LABEL} aria-expanded={open} aria-controls={open ? id : undefined} onClick={() => setOpen((v) => !v)}>
        <IconInfo size={15} />
      </button>
      {open && (
        <div ref={boxRef} id={id} role="dialog" aria-label={LEGEND_LABEL} className="unit-legend" tabIndex={-1}>
          {UNIT_POSITIONS.map((p) => (
            <p key={p} className="unit-legend__line">
              <b>{UNIT_LABEL[p]}</b> {UNIT_LEGEND[p]}
            </p>
          ))}
          <p className="unit-legend__line">{UNIT_LEGEND_NOTE}</p>
        </div>
      )}
    </span>
  );
}
