import { useId, useState } from 'react';
import { RadioRow } from '@/components/ui/OptionList';
import { Popover } from '@/components/ui/Popover';
import { Button } from '@/components/ui/primitives';
import type { SortDir, SortKey } from '@/lib/address';

export interface SortControlProps<K extends string = string> {
  keys: readonly SortKey<K>[];
  onChange: (next: { dir: SortDir; sort: K }) => void;
  /** The resolved direction, never the address's empty `dir`: the page maps it through sortDirOf and sortAddressOf. */
  value: { dir: SortDir; sort: K };
}

const DIRECTIONS: readonly { dir: SortDir; label: string }[] = [
  { dir: 'asc', label: 'Ascending' },
  { dir: 'desc', label: 'Descending' },
];

/** IconArrowRight's stroke, turned up for ascending and down for descending. */
function DirectionArrow({ dir }: { dir: SortDir }): JSX.Element {
  return (
    <svg aria-hidden fill="none" height={14} stroke="currentColor" strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.75} viewBox="0 0 24 24" width={14}>
      <path d="M5 12h14M13 6l6 6-6 6" transform={dir === 'asc' ? 'rotate(-90 12 12)' : 'rotate(90 12 12)'} />
    </svg>
  );
}

/**
 * A 32 px secondary button reading "Sort: {label}" with the direction's arrow, the aria-label "Sort" and the title
 * "Sorted by {label}, {ascending|descending}", its Popover holding the view's keys as radios and, under a rule,
 * Ascending and Descending. A key's pick sends the key in its own default direction, a direction's pick keeps the key.
 */
export function SortControl<K extends string>({ keys, onChange, value }: SortControlProps<K>): JSX.Element {
  const boxId = useId();
  const headId = useId();
  // Held in state through the ref callback, since the hooks lint refuses a ref read while rendering.
  const [anchor, setAnchor] = useState<HTMLButtonElement | null>(null);
  const [open, setOpen] = useState(false);
  const label = keys.find((k) => k.id === value.sort)?.label ?? value.sort;
  // The pick unmounts the radio that holds the focus, so the focus goes back to the button first, as Add filter's does.
  const pick = (next: { dir: SortDir; sort: K }) => {
    anchor?.focus({ preventScroll: true });
    setOpen(false);
    onChange(next);
  };
  const radio = (id: string, text: string, on: boolean, choose: () => void) => (
    <RadioRow key={id} checked={on} onClick={choose}>
      {text}
    </RadioRow>
  );
  return (
    <>
      <Button ref={setAnchor} size="sm" aria-label="Sort" aria-haspopup="dialog" aria-expanded={open} title={`Sorted by ${label}, ${value.dir === 'asc' ? 'ascending' : 'descending'}`} onClick={() => setOpen((v) => !v)}>
        {`Sort: ${label}`}
        <DirectionArrow dir={value.dir} />
      </Button>
      <Popover anchor={anchor} id={boxId} labelledBy={headId} onClose={() => setOpen(false)} open={open}>
        <span className="sr-only" id={headId}>
          Sort
        </span>
        <div className="sort-popover">
          <div role="radiogroup" aria-labelledby={headId}>
            {keys.map((k) => radio(k.id, k.label, k.id === value.sort, () => pick({ dir: k.defaultDir, sort: k.id })))}
          </div>
          <hr className="sort-popover__rule" />
          <div role="radiogroup" aria-label="Direction">
            {DIRECTIONS.map((d) => radio(d.dir, d.label, d.dir === value.dir, () => pick({ dir: d.dir, sort: value.sort })))}
          </div>
        </div>
      </Popover>
    </>
  );
}
