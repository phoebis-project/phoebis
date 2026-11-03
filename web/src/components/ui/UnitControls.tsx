import { useId, useState } from 'react';
import { positionOf, UNIT_LABEL, UNIT_LEGEND, UNIT_LEGEND_NOTE, UNIT_POSITIONS, UNIT_SHORT, WEEK_LABEL, WEEK_POSITIONS, WEEK_SHORT, weekOf, type UnitPosition, type WeekPosition } from '@/lib/unit';
import { IconInfo } from './icons';
import { Popover } from './Popover';
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

/**
 * The week a subscription figure reads in. It applies under Native alone, so under a dollar position it stays drawn,
 * disabled, and its title says why.
 */
export function WeekSwitch({ disabled, onChange, short, week }: { disabled: boolean; onChange: (w: WeekPosition) => void; short: boolean; week: WeekPosition }) {
  return (
    // Keyed on the label set for the reason the unit switch is.
    <Select key={short ? 'short' : 'long'} aria-label="Week" className="week-switch" value={week} disabled={disabled} title={disabled ? 'The week applies under Native' : undefined} onChange={(e) => onChange(weekOf(e.target.value))}>
      {WEEK_POSITIONS.map((w) => (
        <option key={w} value={w}>
          {short ? WEEK_SHORT[w] : WEEK_LABEL[w]}
        </option>
      ))}
    </Select>
  );
}

const LEGEND_LABEL = 'What the units mean';

/** The unit positions explained, in a popover hung from the info button beside the switch. */
export function UnitLegend() {
  const [open, setOpen] = useState(false);
  const [anchor, setAnchor] = useState<HTMLButtonElement | null>(null);
  const id = useId();
  return (
    <>
      <button ref={setAnchor} type="button" className="unit-legend__button" aria-label={LEGEND_LABEL} title={LEGEND_LABEL} aria-expanded={open} aria-controls={open ? id : undefined} onClick={() => setOpen((v) => !v)}>
        <IconInfo size={15} />
      </button>
      <Popover anchor={anchor} className="unit-legend" id={id} label={LEGEND_LABEL} onClose={() => setOpen(false)} open={open}>
        {UNIT_POSITIONS.map((p) => (
          <p key={p} className="unit-legend__line">
            <b>{UNIT_LABEL[p]}</b> {UNIT_LEGEND[p]}
          </p>
        ))}
        <p className="unit-legend__line">{UNIT_LEGEND_NOTE}</p>
      </Popover>
    </>
  );
}
